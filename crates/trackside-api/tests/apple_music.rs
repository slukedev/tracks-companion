//! Apple Music developer tokens over real HTTP: the admin-only, write-only `MusicKit` key, the
//! token any library credential may fetch, the shared cache and its rotation, `/me`, HTTPS
//! and the per-user rate limit.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use reqwest::header::{HeaderMap, HeaderValue};
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair,
    UnparsedPublicKey,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use trackside_api::{ApiState, LocalState, NoOps, ServerInfo, wire};
use trackside_auth::{Auth, SecretKey};
use trackside_core::testing::{FakeAcquireControl, FakeScanControl, TestDb};
use trackside_core::{BoxFuture, Config};
use trackside_media::{Media, MediaError, TranscodeProcess, TranscodeSpec, Transcoder};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PASSWORD: &str = "correct horse battery";

#[derive(Debug)]
struct NoTranscoder;

impl Transcoder for NoTranscoder {
    fn version(&self) -> Option<String> {
        None
    }

    fn spawn(&self, _spec: TranscodeSpec) -> Result<Box<dyn TranscodeProcess>, MediaError> {
        Err(MediaError::NoTranscoder)
    }
}

struct Health;

impl trackside_api::HealthSource for Health {
    fn overview(&self) -> BoxFuture<'_, wire::admin::Overview> {
        Box::pin(async { wire::admin::Overview::default() })
    }

    fn ready(&self) -> wire::Ready {
        wire::Ready {
            status: wire::ReadyStatus::Ready,
            checks: std::collections::BTreeMap::new(),
            notes: Vec::new(),
        }
    }
}

struct Server {
    base: String,
    host: String,
    http: reqwest::Client,
    auth: Arc<Auth>,
    test: TestDb,
    data: tempfile::TempDir,
    stop: CancellationToken,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// `x-test-peer` poses as another client address; `x-test-https` marks the request as HTTPS.
async fn test_peer(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(peer) = request
        .headers()
        .get("x-test-peer")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<SocketAddr>().ok())
    {
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
    }
    if request.headers().contains_key("x-test-https") {
        request.extensions_mut().insert(trackside_auth::Https);
    }
    next.run(request).await
}

async fn start() -> Result<Server, Box<dyn std::error::Error>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let test = TestDb::new()?;
    let data = tempfile::tempdir()?;
    let mut config = Config::defaults(data.path());
    config.auth.argon2_memory = 1 << 16;
    config.auth.argon2_iterations = 1;
    let auth = Arc::new(Auth::new(
        test.db.clone(),
        &config.auth,
        SecretKey::from_bytes([7; 32]),
        Vec::new(),
        test.clock.clone(),
    ));
    let media = Arc::new(Media::new(
        &config.media,
        &data.path().join("cache"),
        test.db.clone(),
        Arc::new(NoTranscoder),
        test.clock.clone(),
    )?);
    let stop = CancellationToken::new();
    let state = ApiState {
        db: test.db.clone(),
        auth: Arc::clone(&auth),
        media,
        scan: Arc::new(FakeScanControl::new()),
        acquire: Arc::new(FakeAcquireControl::new()),
        enrich: Arc::new(trackside_core::NoEnrich),
        events: test.events.clone(),
        config: Arc::new(config),
        server: ServerInfo::new(
            uuid::Uuid::new_v4(),
            "Home",
            trackside_core::Clock::now(&*test.clock),
        ),
        health: Arc::new(Health),
        ops: Arc::new(NoOps),
        local: Arc::new(LocalState::new(stop.clone())),
    };
    let app = trackside_api::router(state).layer(axum::middleware::from_fn(test_peer));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address: SocketAddr = listener.local_addr()?;
    let shutdown = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await;
    });
    Ok(Server {
        base: format!("http://{address}/api/v1"),
        host: format!("http://{address}"),
        http: reqwest::Client::new(),
        auth,
        test,
        data,
        stop,
    })
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn setup_admin(&self) -> Result<String, Box<dyn std::error::Error>> {
        let token = self
            .auth
            .ensure_setup_token(self.data.path())
            .await?
            .ok_or("no setup token")?;
        let response = self
            .http
            .post(self.url("/setup"))
            .json(&json!({"setup_token": token, "username": "luke", "password": PASSWORD}))
            .send()
            .await?;
        assert_eq!(response.status(), 201);
        cookie_of(&response)
    }

    fn session(&self, cookie: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(value) = HeaderValue::from_str(cookie) {
            headers.insert("cookie", value);
        }
        headers.insert("x-trackside-csrf", HeaderValue::from_static("1"));
        if let Ok(origin) = HeaderValue::from_str(&self.host) {
            headers.insert("origin", origin);
        }
        headers
    }

    async fn add_member(&self, cookie: &str, name: &str) -> TestResult {
        let response = self
            .http
            .post(self.url("/admin/users"))
            .headers(self.session(cookie))
            .json(&json!({"username": name, "password": PASSWORD, "library_ids": []}))
            .send()
            .await?;
        assert_eq!(response.status(), 201, "{:?}", response.text().await);
        Ok(())
    }

    async fn sign_in(&self, user: &str) -> Result<String, Box<dyn std::error::Error>> {
        let response = self
            .http
            .post(self.url("/auth/session"))
            .header("origin", &self.host)
            .json(&json!({"username": user, "password": PASSWORD}))
            .send()
            .await?;
        assert_eq!(response.status(), 200, "{:?}", response.text().await);
        cookie_of(&response)
    }

    async fn reauth(&self, cookie: &str) -> TestResult {
        let response = self
            .http
            .post(self.url("/auth/reauth"))
            .headers(self.session(cookie))
            .json(&json!({"password": PASSWORD}))
            .send()
            .await?;
        assert!(
            response.status().is_success(),
            "{:?}",
            response.text().await
        );
        Ok(())
    }

    async fn device_key(
        &self,
        user: &str,
        device: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let response = self
            .http
            .post(self.url("/auth/login"))
            .json(&json!({"username": user, "password": PASSWORD,
                "device": {"client_device_id": device, "name": device, "platform": "android"}}))
            .send()
            .await?;
        assert_eq!(response.status(), 201);
        let body: wire::auth::PairResult = response.json().await?;
        Ok(body.api_key)
    }

    async fn put_key(
        &self,
        cookie: &str,
        body: &Value,
    ) -> Result<(u16, String), Box<dyn std::error::Error>> {
        let response = self
            .http
            .put(self.url("/admin/apple-music"))
            .headers(self.session(cookie))
            .json(body)
            .send()
            .await?;
        Ok((response.status().as_u16(), response.text().await?))
    }

    async fn token(&self, key: &str) -> Result<reqwest::Response, Box<dyn std::error::Error>> {
        Ok(self
            .http
            .get(self.url("/me/apple-music/developer-token"))
            .bearer_auth(key)
            .send()
            .await?)
    }

    /// The stored settings row, as text.
    async fn stored_row(&self) -> Result<String, Box<dyn std::error::Error>> {
        Ok(self
            .test
            .db
            .read(|c| {
                Ok(
                    trackside_core::repo::settings::get::<Value>(c, "apple_music.developer_key")?
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                )
            })
            .await?)
    }

    async fn me_features(&self, key: &str) -> Result<Value, Box<dyn std::error::Error>> {
        let me: Value = self
            .http
            .get(self.url("/me"))
            .bearer_auth(key)
            .send()
            .await?
            .json()
            .await?;
        Ok(me["features"].clone())
    }
}

fn cookie_of(response: &reqwest::Response) -> Result<String, Box<dyn std::error::Error>> {
    Ok(response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.split(';').next().map(str::to_owned))
        .ok_or("no cookie")?)
}

/// A new P-256 key: its `.p8` text and its public key.
fn new_key() -> Result<(String, Vec<u8>), Box<dyn std::error::Error>> {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
        .map_err(|_| "generate")?;
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
        .map_err(|_| "parse")?;
    let body = STANDARD.encode(pkcs8.as_ref());
    let lines: Vec<String> = body
        .as_bytes()
        .chunks(64)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        lines.join("\n")
    );
    Ok((pem, pair.public_key().as_ref().to_vec()))
}

fn upload(pem: &str) -> Value {
    json!({"private_key": pem, "key_id": "ABC123DEFG", "team_id": "TEAM456XYZ"})
}

fn decode(part: &str) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part)?)?)
}

/// Checks the token's signature against `public` and returns its claims.
fn verify(token: &str, public: &[u8]) -> Result<Value, Box<dyn std::error::Error>> {
    let (message, signature) = token.rsplit_once('.').ok_or("not a JWT")?;
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, public)
        .verify(message.as_bytes(), &URL_SAFE_NO_PAD.decode(signature)?)
        .map_err(|_| "signature does not verify")?;
    let mut parts = message.split('.');
    let header = decode(parts.next().ok_or("no header")?)?;
    assert_eq!(header, json!({"alg": "ES256", "kid": "ABC123DEFG"}));
    let claims = decode(parts.next().ok_or("no claims")?)?;
    assert_eq!(claims["iss"], "TEAM456XYZ");
    assert!(claims.get("origin").is_none(), "no origin claim");
    let span = claims["exp"].as_i64().ok_or("exp")? - claims["iat"].as_i64().ok_or("iat")?;
    assert!(span > 0 && span <= 7 * 86_400, "{span}");
    Ok(claims)
}

#[tokio::test]
async fn without_a_key_the_token_is_404_and_me_does_not_offer_it() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let key = server.device_key("luke", "pixel").await?;
    let anonymous = server
        .http
        .get(server.url("/me/apple-music/developer-token"))
        .send()
        .await?;
    assert_eq!(anonymous.status(), 401);
    let missing = server.token(&key).await?;
    assert_eq!(missing.status(), 404);
    let body: Value = missing.json().await?;
    assert_eq!(body["error"]["code"], "not_found");
    assert!(server.me_features(&key).await?.get("apple_music").is_none());
    let admin: Value = server
        .http
        .get(server.url("/admin/apple-music"))
        .headers(server.session(&cookie))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(admin, json!({"configured": false}));
    Ok(())
}

#[tokio::test]
async fn only_admins_set_the_key_and_it_is_never_sent_back() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    server.add_member(&cookie, "nana").await?;
    let nana = server.sign_in("nana").await?;
    let nana_key = server.device_key("nana", "nana-phone").await?;
    let admin_key = server.device_key("luke", "pixel").await?;
    let (pem, _) = new_key()?;
    let der_b64: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();

    let (status, _) = server.put_key(&nana, &upload(&pem)).await?;
    assert_eq!(status, 403, "a member's session");
    for key in [&nana_key, &admin_key] {
        let refused = server
            .http
            .put(server.url("/admin/apple-music"))
            .bearer_auth(key)
            .json(&upload(&pem))
            .send()
            .await?;
        assert_eq!(refused.status(), 403, "a device key");
    }

    for (body, field) in [
        (upload("not a key"), "private_key"),
        (
            upload("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----"),
            "private_key",
        ),
        (
            json!({"private_key": pem, "key_id": "short", "team_id": "TEAM456XYZ"}),
            "key_id",
        ),
        (
            json!({"private_key": pem, "key_id": "ABC123DEFG", "team_id": "TEAM 456XY"}),
            "team_id",
        ),
    ] {
        let (status, text) = server.put_key(&cookie, &body).await?;
        assert_eq!(status, 400, "{text}");
        let error: Value = serde_json::from_str(&text)?;
        assert_eq!(error["error"]["details"]["field"], field, "{text}");
    }

    let (status, text) = server.put_key(&cookie, &upload(&pem)).await?;
    assert_eq!(status, 200, "{text}");
    let saved: Value = serde_json::from_str(&text)?;
    assert_eq!(saved["configured"], true);
    assert_eq!(saved["key_id"], "ABC123DEFG");
    assert_eq!(saved["team_id"], "TEAM456XYZ");
    assert!(saved["set_at"].is_string());

    for path in [
        "/admin/apple-music",
        "/admin/settings",
        "/admin/audit",
        "/me",
    ] {
        let text = server
            .http
            .get(server.url(path))
            .headers(server.session(&cookie))
            .send()
            .await?
            .text()
            .await?;
        assert!(
            !text.contains("PRIVATE KEY") && !text.contains(&der_b64[..40]),
            "{path} shows the key"
        );
    }
    let member_get = server
        .http
        .get(server.url("/admin/apple-music"))
        .headers(server.session(&nana))
        .send()
        .await?;
    assert_eq!(member_get.status(), 403);

    let row = server.stored_row().await?;
    assert!(
        row.contains("sealed") && !row.contains(&der_b64[..40]),
        "sealed at rest"
    );

    let remove = server
        .http
        .delete(server.url("/admin/apple-music"))
        .headers(server.session(&nana))
        .send()
        .await?;
    assert_eq!(remove.status(), 403, "members cannot remove it");
    let remove = server
        .http
        .delete(server.url("/admin/apple-music"))
        .headers(server.session(&cookie))
        .send()
        .await?;
    assert_eq!(remove.status(), 204);
    assert_eq!(server.token(&nana_key).await?.status(), 404);
    Ok(())
}

#[tokio::test]
async fn members_get_one_shared_token_that_rotates_when_due_or_when_the_key_changes() -> TestResult
{
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    server.add_member(&cookie, "nana").await?;
    let phone = server.device_key("nana", "nana-phone").await?;
    let tablet = server.device_key("nana", "nana-tablet").await?;
    let (pem, public) = new_key()?;
    assert_eq!(server.put_key(&cookie, &upload(&pem)).await?.0, 200);
    assert_eq!(
        server.me_features(&phone).await?["apple_music"],
        json!({"developer_token": true})
    );

    let response = server.token(&phone).await?;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let first: wire::auth::DeveloperToken = response.json().await?;
    let claims = verify(&first.token, &public)?;
    assert_eq!(claims["exp"].as_i64(), Some(first.expires_at.0.0 / 1000));
    assert_eq!(
        first.expires_at.0.since(first.refresh_after.0),
        Duration::from_hours(1)
    );

    let again: wire::auth::DeveloperToken = server.token(&tablet).await?.json().await?;
    assert_eq!(again.token, first.token, "the cached token is shared");

    server.test.clock.advance(Duration::from_hours(22));
    let still: wire::auth::DeveloperToken = server.token(&phone).await?.json().await?;
    assert_eq!(still.token, first.token, "reused until refresh_after");

    server
        .test
        .clock
        .advance(Duration::from_hours(1) + Duration::from_secs(1));
    let renewed: wire::auth::DeveloperToken = server.token(&phone).await?.json().await?;
    assert_ne!(
        renewed.token, first.token,
        "a new one once refresh_after passed"
    );
    let renewed_claims = verify(&renewed.token, &public)?;
    assert!(renewed_claims["iat"].as_i64() > claims["iat"].as_i64());

    server.reauth(&cookie).await?;
    let (pem2, public2) = new_key()?;
    assert_eq!(server.put_key(&cookie, &upload(&pem2)).await?.0, 200);
    let rotated: wire::auth::DeveloperToken = server.token(&phone).await?.json().await?;
    assert_ne!(rotated.token, renewed.token, "a new key signs at once");
    verify(&rotated.token, &public2)?;
    assert!(verify(&rotated.token, &public).is_err());

    let remove = server
        .http
        .delete(server.url("/admin/apple-music"))
        .headers(server.session(&cookie))
        .send()
        .await?;
    assert_eq!(remove.status(), 204);
    assert_eq!(server.token(&phone).await?.status(), 404);
    assert!(
        server
            .me_features(&phone)
            .await?
            .get("apple_music")
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn plain_http_from_the_internet_is_refused_and_requests_are_limited_per_user() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let key = server.device_key("luke", "pixel").await?;
    let (pem, _) = new_key()?;
    assert_eq!(server.put_key(&cookie, &upload(&pem)).await?.0, 200);
    let from = |peer: &'static str, https: bool| {
        let mut request = server
            .http
            .get(server.url("/me/apple-music/developer-token"))
            .header("x-test-peer", peer)
            .bearer_auth(&key);
        if https {
            request = request.header("x-test-https", "1");
        }
        request.send()
    };
    let refused = from("203.0.113.7:4000", false).await?;
    assert_eq!(refused.status(), 403);
    let body: Value = refused.json().await?;
    assert_eq!(body["error"]["code"], "https_required");
    assert_eq!(from("203.0.113.7:4000", true).await?.status(), 200);
    assert_eq!(from("192.168.1.20:4000", false).await?.status(), 200);

    let limit = trackside_auth::DEVELOPER_TOKENS_PER_MINUTE as usize;
    for _ in 2..limit {
        assert_eq!(server.token(&key).await?.status(), 200);
    }
    let limited = server.token(&key).await?;
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key("retry-after"));

    // Another user has their own budget.
    server.add_member(&cookie, "nana").await?;
    let nana = server.device_key("nana", "nana-phone").await?;
    assert_eq!(server.token(&nana).await?.status(), 200);
    Ok(())
}
