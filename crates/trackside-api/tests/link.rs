//! Device-code linking over real HTTP: a TV starts a link and polls, a phone or the dashboard
//! looks it up and approves or denies it, and the TV gets the same `PairResult` as pairing.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
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
        assistant: std::sync::Arc::new(trackside_core::NoAssistant),
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

    async fn post(
        &self,
        path: &str,
        auth: Cred<'_>,
        body: &Value,
    ) -> Result<(u16, HeaderMap, Value), Box<dyn std::error::Error>> {
        let request = self.http.post(self.url(path)).json(body);
        let request = match auth {
            Cred::None => request,
            Cred::Key(key) => request.bearer_auth(key),
            Cred::Session(cookie) => request.headers(self.session(cookie)),
            Cred::Peer(peer) => request.header("x-test-peer", peer),
        };
        let response = request.send().await?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response.text().await?;
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text)?
        };
        Ok((status, headers, value))
    }

    async fn start_link(&self) -> Result<wire::auth::LinkStart, Box<dyn std::error::Error>> {
        let (status, _, body) = self.post("/link/start", Cred::None, &tv()).await?;
        assert_eq!(status, 201, "{body}");
        Ok(serde_json::from_value(body)?)
    }

    async fn poll(
        &self,
        link: &wire::auth::LinkStart,
    ) -> Result<(u16, HeaderMap, Value), Box<dyn std::error::Error>> {
        self.post(
            "/link/poll",
            Cred::None,
            &json!({"device_code": link.device_code}),
        )
        .await
    }
}

/// How a request is authenticated.
#[derive(Clone, Copy)]
enum Cred<'a> {
    None,
    Key(&'a str),
    Session(&'a str),
    /// No credential, from this peer address.
    Peer(&'a str),
}

fn tv() -> Value {
    json!({"device": {"client_device_id": "tv-install-1", "name": "Living Room TV",
        "platform": "android_tv", "model": "Chromecast with Google TV", "app_name": "Tracks",
        "app_version": "3.2"}})
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
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

#[tokio::test]
async fn a_tv_is_linked_by_a_phone_and_gets_a_pair_result() -> TestResult {
    let server = start().await?;
    let _admin = server.setup_admin().await?;
    let phone = server.device_key("luke", "phone").await?;
    let link = server.start_link().await?;
    assert_eq!(link.user_code.len(), 9);
    assert_eq!(link.interval_s, 5);
    assert!(
        link.verification_url
            .ends_with(&format!("/link?code={}", link.user_code))
    );
    assert!(link.link_url.starts_with("tracks://link?server="));
    assert!(
        link.link_url
            .ends_with(&format!("&code={}", link.user_code))
    );

    let (status, _, body) = server.poll(&link).await?;
    assert_eq!((status, code(&body)), (428, "authorization_pending"));

    let typed = link.user_code.to_lowercase().replace('-', "");
    let (status, _, body) = server
        .post(
            "/link/lookup",
            Cred::Key(&phone),
            &json!({"user_code": typed}),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let info: wire::auth::LinkInfo = serde_json::from_value(body)?;
    assert_eq!(info.state, wire::auth::LinkState::Pending);
    assert_eq!(info.device.name, "Living Room TV");
    assert_eq!(info.device.platform, "android_tv");
    assert!(info.nearby);

    let (status, _, body) = server
        .post(
            "/link/approve",
            Cred::Key(&phone),
            &json!({"user_code": link.user_code}),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "approved");

    server.test.clock.advance(Duration::from_secs(5));
    let (status, _, body) = server.poll(&link).await?;
    assert_eq!(status, 200, "{body}");
    let result: wire::auth::PairResult = serde_json::from_value(body)?;
    assert_eq!(result.device.name, "Living Room TV");
    assert_eq!(result.user.username, "luke");

    let me = server
        .http
        .get(server.url("/me"))
        .bearer_auth(&result.api_key)
        .send()
        .await?;
    assert_eq!(me.status(), 200);
    let admin = server
        .http
        .get(server.url("/admin/overview"))
        .bearer_auth(&result.api_key)
        .send()
        .await?;
    assert_eq!(
        admin.status(),
        403,
        "a linked key never reaches admin routes"
    );

    server.test.clock.advance(Duration::from_secs(5));
    let (status, _, body) = server.poll(&link).await?;
    assert_eq!((status, code(&body)), (410, "expired_token"));
    let (status, _, body) = server
        .post(
            "/link/lookup",
            Cred::Key(&phone),
            &json!({"user_code": link.user_code}),
        )
        .await?;
    assert_eq!((status, code(&body)), (409, "conflict"));
    assert_eq!(body["error"]["details"]["reason"], "link_used");
    Ok(())
}

#[tokio::test]
async fn the_dashboard_denies_with_its_session_and_csrf() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let link = server.start_link().await?;
    let bare = server
        .http
        .post(server.url("/link/deny"))
        .header("cookie", &cookie)
        .json(&json!({"user_code": link.user_code}))
        .send()
        .await?;
    assert_eq!(bare.status(), 403);
    let (status, _, body) = server
        .post(
            "/link/deny",
            Cred::Session(&cookie),
            &json!({"user_code": link.user_code}),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "denied");
    let (status, _, body) = server.poll(&link).await?;
    assert_eq!((status, code(&body)), (403, "access_denied"));
    Ok(())
}

#[tokio::test]
async fn polling_too_often_answers_slow_down() -> TestResult {
    let server = start().await?;
    let link = server.start_link().await?;
    let _ = server.poll(&link).await?;
    let (status, headers, body) = server.poll(&link).await?;
    assert_eq!((status, code(&body)), (429, "slow_down"));
    assert_eq!(body["error"]["details"]["interval_s"], 10);
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("10")
    );
    Ok(())
}

#[tokio::test]
async fn approvers_hear_what_is_wrong_with_a_code() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let phone = server.device_key("luke", "phone").await?;
    let (status, _, body) = server
        .post(
            "/link/lookup",
            Cred::Key(&phone),
            &json!({"user_code": "AAAA-AAAA"}),
        )
        .await?;
    assert_eq!((status, code(&body)), (404, "not_found"));

    let (status, _, key) = server
        .post(
            "/me/api-keys",
            Cred::Session(&cookie),
            &json!({"kind": "api_key", "name": "Script", "scopes": ["library", "stream", "write"]}),
        )
        .await?;
    assert_eq!(status, 201, "{key}");
    let secret = key["secret"].as_str().ok_or("no secret")?;
    let link = server.start_link().await?;
    server.test.clock.advance(Duration::from_mins(10));
    let (status, _, body) = server
        .post(
            "/link/lookup",
            Cred::Key(&phone),
            &json!({"user_code": link.user_code}),
        )
        .await?;
    assert_eq!((status, code(&body)), (410, "link_expired"));

    let (status, _, body) = server
        .post(
            "/link/lookup",
            Cred::None,
            &json!({"user_code": link.user_code}),
        )
        .await?;
    assert_eq!((status, code(&body)), (401, "unauthenticated"));

    let fresh = server.start_link().await?;
    let (status, _, body) = server
        .post(
            "/link/approve",
            Cred::Key(secret),
            &json!({"user_code": fresh.user_code}),
        )
        .await?;
    assert_eq!((status, code(&body)), (403, "forbidden"));
    Ok(())
}

#[tokio::test]
async fn a_tv_on_the_internet_must_use_https() -> TestResult {
    let server = start().await?;
    let (status, _, body) = server
        .post("/link/start", Cred::Peer("203.0.113.9:5000"), &tv())
        .await?;
    assert_eq!((status, code(&body)), (403, "https_required"));
    Ok(())
}

#[tokio::test]
async fn starting_links_is_rate_limited() -> TestResult {
    let server = start().await?;
    for _ in 0..5 {
        server.start_link().await?;
    }
    let (status, headers, body) = server.post("/link/start", Cred::None, &tv()).await?;
    assert_eq!((status, code(&body)), (429, "rate_limited"));
    assert!(headers.contains_key("retry-after"));
    Ok(())
}

#[tokio::test]
async fn capabilities_offer_device_links() -> TestResult {
    let server = start().await?;
    let caps: Value = server
        .http
        .get(server.url("/capabilities"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(caps["auth"]["device_link"], true);
    Ok(())
}
