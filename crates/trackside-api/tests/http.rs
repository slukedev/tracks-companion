//! The native API over real HTTP on an ephemeral port, with a real in-memory database, real
//! auth, and the core fakes for the scanner and the acquirer.
//!
//! These need WP1's repositories and WP4's auth; the ones that also need media (WP3) or seeded
//! songs (WP1 ingest plus WP2) say so in their `ignore` reason.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use trackside_api::{ApiState, LocalState, NoOps, ServerInfo, wire};
use trackside_auth::{Auth, SecretKey};
use trackside_core::control::{
    AcquireControl, AcquireError, AcquireFeature, AcquireRequest, AcquisitionFilter,
    AcquisitionState, AcquisitionView, AdminAcquisitionView, ExternalPlay, ExternalPlayRequest,
    ExternalQuery, ExternalSearch, InstantStream, ProviderChange, ProviderStatus, ProviderTest,
};
use trackside_core::testing::{FakeAcquireControl, FakeScanControl, TestDb};
use trackside_core::{Access, AcquisitionId, BoxFuture, Config, Page, Permissions, random_id};
use trackside_media::{Media, MediaError, TranscodeProcess, TranscodeSpec, Transcoder};

type TestResult = Result<(), Box<dyn std::error::Error>>;

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

/// Acquisition on for everyone who may request: answers with a queued request.
#[derive(Debug, Default)]
struct OpenAcquisition;

fn queued() -> AcquisitionView {
    AcquisitionView {
        id: random_id(),
        state: AcquisitionState::Queued,
        title: String::from("Halo"),
        artist: String::from("Beyoncé"),
        album: None,
        progress: None,
        quality: None,
        song: None,
        existing: false,
        failure: None,
        stream: None,
        created_at: trackside_core::testing::TEST_START,
        updated_at: trackside_core::testing::TEST_START,
    }
}

impl AcquireControl for OpenAcquisition {
    fn feature_for(
        &self,
        caller: &Access,
        permissions: &Permissions,
        request: bool,
    ) -> Option<AcquireFeature> {
        (request && (caller.is_admin() || permissions.can_request)).then_some(AcquireFeature {
            instant_play: false,
            external_play: false,
        })
    }
    fn search<'a>(
        &'a self,
        _: &'a Access,
        _: ExternalQuery,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        Box::pin(async {
            Ok(ExternalSearch {
                search_id: String::from("v1"),
                complete: true,
                in_library: Vec::new(),
                candidates: Vec::new(),
            })
        })
    }
    fn search_more<'a>(
        &'a self,
        a: &'a Access,
        _: &'a str,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        self.search(a, ExternalQuery::Text(String::new()))
    }
    fn request<'a>(
        &'a self,
        _: &'a Access,
        _: AcquireRequest,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        Box::pin(async { Ok(queued()) })
    }
    fn get<'a>(
        &'a self,
        _: &'a Access,
        _: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        Box::pin(async { Ok(queued()) })
    }
    fn list<'a>(
        &'a self,
        _: &'a Access,
        _: AcquisitionFilter,
    ) -> BoxFuture<'a, Result<Page<AcquisitionView>, AcquireError>> {
        Box::pin(async { Ok(Page::last(vec![queued()])) })
    }
    fn cancel<'a>(
        &'a self,
        _: &'a Access,
        _: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<(), AcquireError>> {
        Box::pin(async { Err(AcquireError::InvalidState) })
    }
    fn instant_stream(&self, _: &Access, _: &AcquisitionId) -> Result<InstantStream, AcquireError> {
        Err(AcquireError::NotFound)
    }
    fn play_external<'a>(
        &'a self,
        _: &'a Access,
        request: ExternalPlayRequest,
    ) -> BoxFuture<'a, Result<ExternalPlay, AcquireError>> {
        Box::pin(async move {
            match request {
                ExternalPlayRequest::Candidate(token) => Ok(ExternalPlay {
                    token: Some(token),
                    title: String::from("Halo"),
                    artist: String::from("Beyoncé"),
                    duration_ms: Some(261_000),
                    song: None,
                }),
                ExternalPlayRequest::Song { title, artist, .. } => Ok(ExternalPlay {
                    token: None,
                    title,
                    artist,
                    duration_ms: Some(261_000),
                    song: Some(random_id()),
                }),
            }
        })
    }
    fn providers(&self) -> BoxFuture<'_, Vec<ProviderStatus>> {
        Box::pin(async { Vec::new() })
    }
    fn configure(&self, _: ProviderChange) -> BoxFuture<'_, Result<(), AcquireError>> {
        Box::pin(async { Ok(()) })
    }
    fn test_provider<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<ProviderTest, AcquireError>> {
        Box::pin(async { Err(AcquireError::NotFound) })
    }
    fn update_ytdlp(&self) -> BoxFuture<'_, Result<(), AcquireError>> {
        Box::pin(async { Ok(()) })
    }
    fn retry<'a>(&'a self, _: &'a AcquisitionId) -> BoxFuture<'a, Result<(), AcquireError>> {
        Box::pin(async { Ok(()) })
    }
    fn admin_list(
        &self,
        _: AcquisitionFilter,
    ) -> BoxFuture<'_, Result<Page<AdminAcquisitionView>, AcquireError>> {
        Box::pin(async { Ok(Page::default()) })
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
            checks: std::collections::BTreeMap::default(),
            notes: Vec::new(),
        }
    }
}

struct Server {
    base: String,
    host: String,
    http: reqwest::Client,
    auth: Arc<Auth>,
    _test: TestDb,
    data: tempfile::TempDir,
    stop: CancellationToken,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn start_with(
    acquire: Arc<dyn AcquireControl>,
) -> Result<Server, Box<dyn std::error::Error>> {
    start_with_transcoder(acquire, Arc::new(NoTranscoder)).await
}

async fn start_with_transcoder(
    acquire: Arc<dyn AcquireControl>,
    transcoder: Arc<dyn Transcoder>,
) -> Result<Server, Box<dyn std::error::Error>> {
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
        transcoder,
        test.clock.clone(),
    )?);
    let stop = CancellationToken::new();
    let state = ApiState {
        db: test.db.clone(),
        auth: Arc::clone(&auth),
        media,
        scan: Arc::new(FakeScanControl::new()),
        acquire,
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
        _test: test,
        data,
        stop,
    })
}

/// Lets a test pose as another client: `x-test-peer` replaces the peer address, and
/// `x-test-https` marks the request as arrived over HTTPS, as the server's middleware would.
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
    start_with(Arc::new(FakeAcquireControl::new())).await
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Creates the first admin through `/setup` and returns its session cookie header.
    async fn setup_admin(&self) -> Result<String, Box<dyn std::error::Error>> {
        let token = self
            .auth
            .ensure_setup_token(self.data.path())
            .await?
            .ok_or("no setup token")?;
        let response = self
            .http
            .post(self.url("/setup"))
            .json(&json!({"setup_token": token, "username": "luke", "display_name": "Luke", "password": "correct horse battery"}))
            .send()
            .await?;
        assert_eq!(response.status(), 201, "{:?}", response.text().await);
        let cookie = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| v.split(';').next().map(str::to_owned))
            .ok_or("no cookie")?;
        Ok(cookie)
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

    /// Signs the admin in from an app and returns the device key.
    async fn device_key(&self) -> Result<String, Box<dyn std::error::Error>> {
        let response = self
            .http
            .post(self.url("/auth/login"))
            .json(&json!({"username": "luke", "password": "correct horse battery",
                "device": {"client_device_id": "8C2E", "name": "Luke's iPhone", "platform": "ios", "app_name": "Tracks", "app_version": "3.2"}}))
            .send()
            .await?;
        assert_eq!(response.status(), 201);
        let body: wire::auth::PairResult = response.json().await?;
        Ok(body.api_key)
    }
}

#[tokio::test]
async fn ping_and_capabilities_answer_without_auth_and_never_mention_acquisition() -> TestResult {
    let server = start_with(Arc::new(OpenAcquisition)).await?;
    let ping: wire::Ping = server
        .http
        .get(server.url("/ping"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(ping.time.0, trackside_core::testing::TEST_START);
    let response = server.http.get(server.url("/capabilities")).send().await?;
    assert!(response.headers().contains_key("x-request-id"));
    let text = response.text().await?;
    assert!(!text.contains("acquisition"), "{text}");
    let capabilities: wire::Capabilities = serde_json::from_str(&text)?;
    assert_eq!(capabilities.product, "trackside");
    assert!(capabilities.setup_required);
    assert!(!capabilities.media.transcoding);
    Ok(())
}

#[tokio::test]
async fn unknown_routes_are_json_404_with_the_request_id() -> TestResult {
    let server = start().await?;
    let response = server.http.get(server.url("/nope")).send().await?;
    assert_eq!(response.status(), 404);
    let id = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body: wire::ErrorEnvelope = response.json().await?;
    assert_eq!(body.error.code, "not_found");
    assert_eq!(body.error.request_id, id);
    let wrong = server.http.delete(server.url("/ping")).send().await?;
    assert_eq!(wrong.status(), 405);
    let body: wire::ErrorEnvelope = wrong.json().await?;
    assert_eq!(body.error.code, "method_not_allowed");
    Ok(())
}

#[tokio::test]
async fn authenticated_routes_refuse_anonymous_callers_and_keys_in_queries() -> TestResult {
    let server = start().await?;
    let before = server.http.get(server.url("/me")).send().await?;
    assert_eq!(before.status(), 403);
    let body: wire::ErrorEnvelope = before.json().await?;
    assert_eq!(body.error.code, "setup_required");
    server.setup_admin().await?;
    let response = server.http.get(server.url("/me")).send().await?;
    assert_eq!(response.status(), 401);
    let body: wire::ErrorEnvelope = response.json().await?;
    assert_eq!(body.error.code, "unauthenticated");
    assert!(body.error.request_id.is_some());
    let query = server
        .http
        .get(server.url("/me?api_key=tsk_abc"))
        .send()
        .await?;
    assert_eq!(query.status(), 400);
    let body: wire::ErrorEnvelope = query.json().await?;
    assert_eq!(body.error.code, "credential_in_query");
    Ok(())
}

#[tokio::test]
async fn setup_then_session_then_app_sign_in_and_me() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let status: wire::auth::SetupStatus = server
        .http
        .get(server.url("/setup/status"))
        .send()
        .await?
        .json()
        .await?;
    assert!(!status.setup_required);
    let again = server
        .http
        .post(server.url("/setup"))
        .json(&json!({"setup_token": "x", "username": "eve", "password": "correct horse battery"}))
        .send()
        .await?;
    assert_eq!(again.status(), 409);
    let user: wire::auth::User = server
        .http
        .get(server.url("/auth/session"))
        .headers(server.session(&cookie))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(user.role, "admin");
    let key = server.device_key().await?;
    let me: Value = server
        .http
        .get(server.url("/me"))
        .bearer_auth(&key)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(me["user"]["username"], "luke");
    assert_eq!(me["device"]["name"], "Luke's iPhone");
    assert!(me["features"].get("acquisition").is_none(), "{me}");
    let token: wire::auth::MediaToken = server
        .http
        .post(server.url("/media-token"))
        .bearer_auth(&key)
        .send()
        .await?
        .json()
        .await?;
    assert_ne!(token.token, "");
    Ok(())
}

#[tokio::test]
async fn wrong_password_is_the_same_401_for_known_and_unknown_users() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let mut bodies = Vec::new();
    for username in ["luke", "nobody"] {
        let response = server
            .http
            .post(server.url("/auth/session"))
            .json(&json!({"username": username, "password": "wrong password!"}))
            .send()
            .await?;
        assert_eq!(response.status(), 401);
        let body: wire::ErrorEnvelope = response.json().await?;
        bodies.push((body.error.code, body.error.message));
    }
    assert_eq!(bodies[0], bodies[1]);
    Ok(())
}

#[tokio::test]
async fn admin_endpoints_need_a_session_and_csrf() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let key = server.device_key().await?;
    let with_key = server
        .http
        .get(server.url("/admin/users"))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(with_key.status(), 403);
    let mut no_csrf = server.session(&cookie);
    no_csrf.remove("x-trackside-csrf");
    let refused = server
        .http
        .post(server.url("/admin/invites"))
        .headers(no_csrf)
        .json(&json!({}))
        .send()
        .await?;
    assert_eq!(refused.status(), 403);
    let body: wire::ErrorEnvelope = refused.json().await?;
    assert_eq!(body.error.code, "csrf");
    let users: wire::admin::AdminUsers = server
        .http
        .get(server.url("/admin/users"))
        .headers(server.session(&cookie))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(users.users.len(), 1);
    let invite = server
        .http
        .post(server.url("/admin/invites"))
        .headers(server.session(&cookie))
        .json(&json!({"role": "member", "note": "For Nana"}))
        .send()
        .await?;
    assert_eq!(invite.status(), 201);
    let invite: wire::admin::AdminInvite = invite.json().await?;
    let qr = server
        .http
        .get(format!(
            "{}{}",
            server.host,
            invite.qr_svg_url.ok_or("no qr")?
        ))
        .headers(server.session(&cookie))
        .send()
        .await?;
    assert_eq!(
        qr.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/svg+xml")
    );
    let token = invite
        .url
        .ok_or("no url")?
        .rsplit('/')
        .next()
        .ok_or("no token")?
        .to_owned();
    let info: wire::auth::InviteInfo = server
        .http
        .get(server.url(&format!("/invites/{token}")))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(info.role, "member");
    assert_eq!(info.note.as_deref(), Some("For Nana"));
    assert!(info.invited_by.is_some(), "the admin's display name");
    Ok(())
}

#[tokio::test]
async fn turning_legacy_subsonic_off_in_the_dashboard_applies_at_once() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    assert!(server.auth.legacy_subsonic());
    for (value, expected) in [(json!(false), false), (Value::Null, true)] {
        let response = server
            .http
            .patch(server.url("/admin/settings"))
            .headers(server.session(&cookie))
            .json(&json!({ "auth.legacy_subsonic": value }))
            .send()
            .await?;
        assert_eq!(response.status(), 200, "{:?}", response.text().await);
        assert_eq!(server.auth.legacy_subsonic(), expected);
    }
    Ok(())
}

#[tokio::test]
async fn features_acquisition_follows_the_acquirer_and_endpoints_404_without_it() -> TestResult {
    let closed = start().await?;
    closed.setup_admin().await?;
    let key = closed.device_key().await?;
    let search = closed
        .http
        .post(closed.url("/acquisitions/search"))
        .bearer_auth(&key)
        .json(&json!({"query": "halo"}))
        .send()
        .await?;
    assert_eq!(search.status(), 404);
    let open = start_with(Arc::new(OpenAcquisition)).await?;
    open.setup_admin().await?;
    let key = open.device_key().await?;
    let me: Value = open
        .http
        .get(open.url("/me"))
        .bearer_auth(&key)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(me["features"]["acquisition"]["instant_play"], false);
    let request = open
        .http
        .post(open.url("/acquisitions"))
        .bearer_auth(&key)
        .json(&json!({"title": "Halo", "artist": "Beyoncé"}))
        .send()
        .await?;
    assert_eq!(request.status(), 202);
    let cancel = open
        .http
        .delete(open.url(&format!("/acquisitions/{}", random_id::<AcquisitionId>())))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(cancel.status(), 422);
    let external: Value = open
        .http
        .get(open.url("/search?q=halo&types=song,external"))
        .bearer_auth(&key)
        .send()
        .await?
        .json()
        .await?;
    assert!(external.get("external").is_some(), "{external}");
    let library: Value = open
        .http
        .get(open.url("/search?q=halo"))
        .bearer_auth(&key)
        .send()
        .await?
        .json()
        .await?;
    assert!(library.get("external").is_none(), "{library}");
    Ok(())
}

#[tokio::test]
async fn playing_without_keeping_is_there_only_with_the_feature() -> TestResult {
    let token = trackside_core::CandidateToken::derive("ytdlp", "{\"id\":\"abc\"}").to_string();
    let closed = start().await?;
    closed.setup_admin().await?;
    let key = closed.device_key().await?;
    let play = closed
        .http
        .post(closed.url("/external/play"))
        .bearer_auth(&key)
        .json(&json!({"candidate": token}))
        .send()
        .await?;
    assert_eq!(play.status(), 404);
    let stream = closed
        .http
        .get(closed.url(&format!("/external/plays/{token}/stream")))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(stream.status(), 404);

    let open = start_with(Arc::new(OpenAcquisition)).await?;
    open.setup_admin().await?;
    let key = open.device_key().await?;
    let played: Value = open
        .http
        .post(open.url("/external/play"))
        .bearer_auth(&key)
        .json(&json!({"candidate": token}))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(
        played["stream"]["hls"],
        format!("/api/v1/external/plays/{token}/stream.m3u8")
    );
    assert!(played.get("song_id").is_none());
    let owned: Value = open
        .http
        .post(open.url("/external/play"))
        .bearer_auth(&key)
        .json(&json!({"title": "Halo", "artist": "Beyoncé"}))
        .send()
        .await?
        .json()
        .await?;
    assert!(owned["song_id"].is_string(), "{owned}");
    assert!(owned.get("stream").is_none());
    let empty = open
        .http
        .post(open.url("/external/play"))
        .bearer_auth(&key)
        .json(&json!({"title": "Halo"}))
        .send()
        .await?;
    assert_eq!(empty.status(), 400);
    let gone = open
        .http
        .get(open.url(&format!("/external/plays/{token}/stream")))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(gone.status(), 404);
    Ok(())
}

#[tokio::test]
async fn pairing_by_code_and_by_device_request() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let offer: wire::auth::PairingCode = server
        .http
        .post(server.url("/me/pairing-codes"))
        .headers(server.session(&cookie))
        .json(&json!({"device_name": "Mac Studio"}))
        .send()
        .await?
        .json()
        .await?;
    assert!(offer.qr_payload.starts_with("trackside://pair?d="));
    let svg = server
        .http
        .get(format!("{}{}", server.host, offer.qr_svg_url))
        .headers(server.session(&cookie))
        .send()
        .await?;
    assert_eq!(svg.status(), 200);
    let device = json!({"client_device_id": "MAC1", "name": "Mac Studio", "platform": "macos"});
    let claimed = server
        .http
        .post(server.url("/pair/claim"))
        .json(&json!({"code": offer.code, "device": device}))
        .send()
        .await?;
    assert_eq!(claimed.status(), 201);
    let reuse = server
        .http
        .post(server.url("/pair/claim"))
        .json(&json!({"code": offer.code, "device": device}))
        .send()
        .await?;
    assert_eq!(reuse.status(), 401);

    let request: wire::auth::PairRequest = server.http.post(server.url("/pair/requests"))
        .json(&json!({"device": {"client_device_id": "TV1", "name": "Living Room Mac", "platform": "macos"}}))
        .send().await?.json().await?;
    let pending: wire::auth::PairPoll = server
        .http
        .get(server.url(&format!("/pair/requests/{}", request.id)))
        .header("authorization", format!("Pair {}", request.poll_secret))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(pending.state, wire::auth::PairState::Pending);
    let approved = server
        .http
        .post(server.url("/pair/requests/approve"))
        .headers(server.session(&cookie))
        .json(&json!({"code": request.code}))
        .send()
        .await?;
    assert_eq!(approved.status(), 200);
    let body: wire::auth::Approved = approved.json().await?;
    assert_eq!(body.device.name.as_deref(), Some("Living Room Mac"));
    let done: wire::auth::PairPoll = server
        .http
        .get(server.url(&format!("/pair/requests/{}", request.id)))
        .header("authorization", format!("Pair {}", request.poll_secret))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(done.state, wire::auth::PairState::Approved);
    assert!(done.result.is_some());
    let gone = server
        .http
        .get(server.url(&format!("/pair/requests/{}", request.id)))
        .header("authorization", format!("Pair {}", request.poll_secret))
        .send()
        .await?;
    assert_eq!(gone.status(), 404);
    Ok(())
}

#[tokio::test]
async fn listens_are_idempotent_per_item() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let batch = json!({"client_time": "2026-11-14T09:30:00.000Z", "listens": [
        {"client_listen_id": "A1", "started_at": "2026-11-13T21:10:02.000Z", "played_ms": 254_000, "title": "Halo", "artist": "Beyoncé"},
        {"client_listen_id": "A2", "started_at": "2026-11-20T21:10:02.000Z", "played_ms": 1000, "title": "Halo", "artist": "Beyoncé"},
        {"client_listen_id": "A3", "started_at": "nope"}
    ]});
    let first: wire::social::ListenResults = server
        .http
        .post(server.url("/listens"))
        .bearer_auth(&key)
        .json(&batch)
        .send()
        .await?
        .json()
        .await?;
    let statuses: Vec<_> = first
        .results
        .iter()
        .map(|r| (r.status.as_str(), r.reason.as_deref()))
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("accepted", None),
            ("rejected", Some("future")),
            ("rejected", Some("invalid"))
        ]
    );
    let second: wire::social::ListenResults = server
        .http
        .post(server.url("/listens"))
        .bearer_auth(&key)
        .json(&batch)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(second.results[0].status, "duplicate");
    Ok(())
}

#[tokio::test]
async fn idempotency_key_replays_and_detects_mismatch() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let create = |name: &str| {
        server
            .http
            .post(server.url("/playlists"))
            .bearer_auth(&key)
            .header("idempotency-key", "K1")
            .json(&json!({"name": name}))
    };
    let first = create("Late Drive").send().await?;
    assert_eq!(first.status(), 201);
    let first: wire::Playlist = first.json().await?;
    let replay = create("Late Drive").send().await?;
    assert_eq!(replay.status(), 201);
    assert_eq!(
        replay
            .headers()
            .get("idempotent-replayed")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
    let replay: wire::Playlist = replay.json().await?;
    assert_eq!(replay.id, first.id);
    let other = create("Morning").send().await?;
    assert_eq!(other.status(), 409);
    let body: wire::ErrorEnvelope = other.json().await?;
    assert_eq!(body.error.code, "idempotency_mismatch");
    let list: wire::social::Playlists = server
        .http
        .get(server.url("/playlists"))
        .bearer_auth(&key)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(list.playlists.len(), 1);
    Ok(())
}

#[tokio::test]
async fn playlist_entries_need_the_current_revision() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let created: wire::Playlist = server
        .http
        .post(server.url("/playlists"))
        .bearer_auth(&key)
        .json(&json!({"name": "Late Drive"}))
        .send()
        .await?
        .json()
        .await?;
    let page = server
        .http
        .get(server.url(&format!("/playlists/{}", created.id)))
        .bearer_auth(&key)
        .send()
        .await?;
    let etag = page
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .ok_or("no etag")?
        .to_owned();
    assert_eq!(etag, format!("\"{}\"", created.revision));
    let ok = server
        .http
        .put(server.url(&format!("/playlists/{}/entries", created.id)))
        .bearer_auth(&key)
        .header("if-match", &etag)
        .json(&json!({"entries": []}))
        .send()
        .await?;
    assert_eq!(ok.status(), 200);
    let stale = server
        .http
        .put(server.url(&format!("/playlists/{}/entries", created.id)))
        .bearer_auth(&key)
        .header("if-match", &etag)
        .json(&json!({"entries": []}))
        .send()
        .await?;
    assert_eq!(stale.status(), 412);
    Ok(())
}

#[tokio::test]
async fn stars_on_unknown_items_are_404_and_art_without_artwork_is_404() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let song = trackside_core::SongId::derive(&trackside_core::AudioDigest([4; 32]));
    let star = server
        .http
        .put(server.url(&format!("/me/stars/song/{song}")))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(star.status(), 404);
    let bad_kind = server
        .http
        .put(server.url(&format!("/me/stars/playlist/{song}")))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(bad_kind.status(), 404);
    let art = server
        .http
        .get(server.url(&format!("/art/{song}")))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(art.status(), 404);
    Ok(())
}

#[tokio::test]
async fn play_queue_is_last_writer_wins() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let empty = server
        .http
        .get(server.url("/me/play-queue"))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(empty.status(), 204);
    let a = trackside_core::SongId::derive(&trackside_core::AudioDigest([5; 32]));
    let b = trackside_core::SongId::derive(&trackside_core::AudioDigest([6; 32]));
    for (entries, by) in [
        (vec![a.to_string()], "iPhone"),
        (vec![b.to_string(), a.to_string()], "Mac"),
    ] {
        let put = server
            .http
            .put(server.url("/me/play-queue"))
            .bearer_auth(&key)
            .json(&json!({"entries": entries, "current": 0, "position_ms": 1000, "changed_by": by}))
            .send()
            .await?;
        assert_eq!(put.status(), 200);
    }
    let queue: wire::social::PlayQueue = server
        .http
        .get(server.url("/me/play-queue"))
        .bearer_auth(&key)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(queue.entries, vec![b.to_string(), a.to_string()]);
    assert_eq!(queue.changed_by, "Mac");
    Ok(())
}

#[tokio::test]
async fn sync_snapshot_completes_and_a_star_arrives_in_the_next_delta() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let mut cursor: Option<String> = None;
    let mut delta_cursor = None;
    for _ in 0..20 {
        let url = match &cursor {
            Some(c) => server.url(&format!("/sync?cursor={c}")),
            None => server.url("/sync"),
        };
        let page: wire::sync::SyncPage = server
            .http
            .get(url)
            .bearer_auth(&key)
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(page.mode, wire::sync::SyncMode::Snapshot);
        if page.complete == Some(true) {
            delta_cursor = Some(page.next_cursor);
            break;
        }
        cursor = Some(page.next_cursor);
    }
    let delta_cursor = delta_cursor.ok_or("snapshot never completed")?;
    let created: wire::Playlist = server
        .http
        .post(server.url("/playlists"))
        .bearer_auth(&key)
        .json(&json!({"name": "Late Drive"}))
        .send()
        .await?
        .json()
        .await?;
    let delta: wire::sync::SyncPage = server
        .http
        .get(server.url(&format!("/sync?cursor={delta_cursor}")))
        .bearer_auth(&key)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(delta.mode, wire::sync::SyncMode::Delta);
    let upserts = delta.upserts.ok_or("no upserts")?;
    assert_eq!(
        upserts.playlists.first().map(|p| p.playlist.id.clone()),
        Some(created.id)
    );
    let bad = server
        .http
        .get(server.url("/sync?cursor=garbage"))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(bad.status(), 400);
    Ok(())
}

#[tokio::test]
async fn events_say_hello_then_changed_after_a_write() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let mut stream = server
        .http
        .get(server.url("/events"))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(
        stream
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        stream
            .headers()
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok()),
        Some("no")
    );
    let mut seen = String::new();
    while !seen.contains("event: hello") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.chunk())
            .await??
            .ok_or("stream ended")?;
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    server
        .http
        .post(server.url("/playlists"))
        .bearer_auth(&key)
        .json(&json!({"name": "Late Drive"}))
        .send()
        .await?;
    while !seen.contains("event: changed") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.chunk())
            .await??
            .ok_or("stream ended")?;
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    Ok(())
}

#[tokio::test]
async fn a_user_may_hold_sixteen_event_streams() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let mut open = Vec::new();
    for _ in 0..trackside_auth::EVENT_STREAMS_PER_USER {
        let stream = server
            .http
            .get(server.url("/events"))
            .bearer_auth(&key)
            .send()
            .await?;
        assert_eq!(stream.status(), 200);
        open.push(stream);
    }
    let refused = server
        .http
        .get(server.url("/events"))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(refused.status(), 429);
    assert!(refused.headers().contains_key("retry-after"));
    drop(open.pop());
    let mut again = None;
    for _ in 0..50 {
        let response = server
            .http
            .get(server.url("/events"))
            .bearer_auth(&key)
            .send()
            .await?;
        if response.status() == 200 {
            again = Some(response);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        again.is_some(),
        "a closed stream did not give its slot back"
    );
    Ok(())
}

#[tokio::test]
async fn passwords_and_keys_over_plain_http_from_the_internet_are_refused() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let key = server.device_key().await?;
    // Every route that takes a password, a setup, invite, reset or pairing secret, or hands
    // out a key; and an ordinary route reached with a key.
    let posts = [
        "/setup",
        "/auth/session",
        "/auth/login",
        "/auth/reauth",
        "/me/password",
        "/me/api-keys",
        "/invites/abc123/accept",
        "/password-resets/abc123",
        "/pair/claim",
        "/pair/requests",
    ];
    let call = |method: reqwest::Method, path: &str, peer: &str, https: bool| {
        let mut request = server
            .http
            .request(method, server.url(path))
            .header("x-test-peer", peer)
            .header("cookie", &cookie)
            .header("x-trackside-csrf", "1")
            .header("origin", &server.host)
            .json(&json!({}));
        if https {
            request = request.header("x-test-https", "1");
        }
        request.send()
    };
    let code = |body: Value| body["error"]["code"].as_str().unwrap_or("").to_owned();
    for path in posts {
        let refused = call(reqwest::Method::POST, path, "203.0.113.7:4000", false).await?;
        assert_eq!(refused.status(), 403, "{path}");
        assert_eq!(code(refused.json().await?), "https_required", "{path}");
        for (peer, https) in [
            ("203.0.113.7:4000", true),
            ("192.168.1.20:4000", false),
            ("[fd12::7]:4000", false),
            ("100.101.102.103:4000", false),
        ] {
            let allowed = call(reqwest::Method::POST, path, peer, https).await?;
            let status = allowed.status();
            let body: Value = allowed.json().await.unwrap_or(Value::Null);
            assert_ne!(
                code(body),
                "https_required",
                "{path} from {peer}, https {https}"
            );
            assert_ne!(status, 500, "{path}");
        }
    }
    let poll = server
        .http
        .get(server.url("/pair/requests/p123"))
        .header("x-test-peer", "203.0.113.7:4000")
        .header("authorization", "Pair secret")
        .send()
        .await?;
    assert_eq!(code(poll.json().await?), "https_required");

    let with_key = |peer: &'static str, https: bool| {
        let mut request = server
            .http
            .get(server.url("/me"))
            .header("x-test-peer", peer)
            .bearer_auth(&key);
        if https {
            request = request.header("x-test-https", "1");
        }
        request.send()
    };
    let refused = with_key("[2001:db8::9]:4000", false).await?;
    assert_eq!(refused.status(), 403);
    assert_eq!(code(refused.json().await?), "https_required");
    assert_eq!(with_key("[2001:db8::9]:4000", true).await?.status(), 200);
    assert_eq!(with_key("10.1.2.3:4000", false).await?.status(), 200);
    // A forwarding header from a peer that isn't a trusted proxy changes nothing.
    let spoofed = server
        .http
        .get(server.url("/me"))
        .header("x-test-peer", "203.0.113.7:4000")
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "192.168.1.20")
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(code(spoofed.json().await?), "https_required");
    Ok(())
}

fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

impl Server {
    async fn ping_proof(
        &self,
        query: &[(&str, &str)],
        forwarded: bool,
    ) -> Result<wire::Ping, Box<dyn std::error::Error>> {
        let query: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut request = self
            .http
            .get(format!("{}?{}", self.url("/ping"), query.join("&")));
        if forwarded {
            request = request.header("x-forwarded-for", "203.0.113.9");
        }
        Ok(request.send().await?.json().await?)
    }
}

#[tokio::test]
async fn ping_proves_the_server_knows_the_device_key_for_the_address_asked() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let response = server
        .http
        .post(server.url("/auth/login"))
        .json(
            &json!({"username": "luke", "password": "correct horse battery",
            "device": {"client_device_id": "PROOF1", "name": "Luke's iPhone", "platform": "ios"}}),
        )
        .send()
        .await?;
    let paired: wire::auth::PairResult = response.json().await?;
    let key = sha256_of(paired.api_key.as_bytes());
    let nonce = "q7rm4x2k9tbv6wnz3hcj8d";
    let device = paired.device.id.as_str();

    for host in ["192.168.1.10:4533", "music.example.com"] {
        let ping = server
            .ping_proof(
                &[("challenge", nonce), ("host", host), ("device", device)],
                false,
            )
            .await?;
        let expected = trackside_auth::proof::endpoint_proof(&key, &ping.server_id, host, nonce);
        assert_eq!(ping.proof.as_deref(), Some(expected.as_str()), "{host}");
    }

    let relayed = server
        .ping_proof(
            &[
                ("challenge", nonce),
                ("host", "192.168.1.10:4533"),
                ("device", device),
            ],
            true,
        )
        .await?;
    assert_eq!(
        relayed.proof, None,
        "a nearby address is proven only to a nearby client"
    );
    let public = server
        .ping_proof(
            &[
                ("challenge", nonce),
                ("host", "music.example.com"),
                ("device", device),
            ],
            true,
        )
        .await?;
    assert!(public.proof.is_some());

    let stranger = trackside_core::random_id::<trackside_core::DeviceId>().to_string();
    let unknown = server
        .ping_proof(
            &[
                ("challenge", nonce),
                ("host", "music.example.com"),
                ("device", stranger.as_str()),
            ],
            false,
        )
        .await?;
    assert_eq!(unknown.proof, None);
    let plain = server.ping_proof(&[], false).await?;
    assert_eq!(plain.proof, None);
    Ok(())
}

#[tokio::test]
async fn ping_proves_the_server_made_a_qr_offer_and_stops_once_it_is_claimed() -> TestResult {
    use base64::Engine;
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let offer: wire::auth::PairingCode = server
        .http
        .post(server.url("/me/pairing-codes"))
        .headers(server.session(&cookie))
        .json(&json!({}))
        .send()
        .await?
        .json()
        .await?;
    let encoded = offer
        .qr_payload
        .strip_prefix("trackside://pair?d=")
        .ok_or("not a pairing link")?;
    let payload: wire::auth::QrPayload =
        serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded)?)?;
    assert_eq!(payload.offer, offer.id);
    let mut tagged = b"offer qr\0".to_vec();
    tagged.extend_from_slice(payload.code.as_bytes());
    let key = sha256_of(&tagged);
    let nonce = "q7rm4x2k9tbv6wnz3hcj8d";
    let query = [
        ("challenge", nonce),
        ("host", "home.local:4533"),
        ("offer", offer.id.as_str()),
    ];
    let ping = server.ping_proof(&query, false).await?;
    let expected =
        trackside_auth::proof::endpoint_proof(&key, &ping.server_id, "home.local:4533", nonce);
    assert_eq!(ping.proof.as_deref(), Some(expected.as_str()));

    let claimed = server
        .http
        .post(server.url("/pair/claim"))
        .json(&json!({"code": payload.code, "device": {"client_device_id": "QR1", "name": "iPhone", "platform": "ios"}}))
        .send()
        .await?;
    assert_eq!(claimed.status(), 201);
    assert_eq!(server.ping_proof(&query, false).await?.proof, None);
    Ok(())
}

/// Open acquisition whose every acquisition is downloading with instant play, and has no
/// HLS job of its own, so the API starts one.
struct InstantWithoutHls {
    open: OpenAcquisition,
    file: std::path::PathBuf,
    growing: tokio::sync::watch::Receiver<trackside_core::control::GrowState>,
}

impl AcquireControl for InstantWithoutHls {
    fn feature_for(
        &self,
        caller: &Access,
        permissions: &Permissions,
        request: bool,
    ) -> Option<AcquireFeature> {
        self.open.feature_for(caller, permissions, request)
    }
    fn search<'a>(
        &'a self,
        a: &'a Access,
        q: ExternalQuery,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        self.open.search(a, q)
    }
    fn search_more<'a>(
        &'a self,
        a: &'a Access,
        id: &'a str,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        self.open.search_more(a, id)
    }
    fn request<'a>(
        &'a self,
        a: &'a Access,
        r: AcquireRequest,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        self.open.request(a, r)
    }
    fn get<'a>(
        &'a self,
        a: &'a Access,
        id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        self.open.get(a, id)
    }
    fn list<'a>(
        &'a self,
        a: &'a Access,
        f: AcquisitionFilter,
    ) -> BoxFuture<'a, Result<Page<AcquisitionView>, AcquireError>> {
        self.open.list(a, f)
    }
    fn cancel<'a>(
        &'a self,
        a: &'a Access,
        id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<(), AcquireError>> {
        self.open.cancel(a, id)
    }
    fn instant_stream(&self, _: &Access, _: &AcquisitionId) -> Result<InstantStream, AcquireError> {
        Ok(InstantStream {
            progressive: trackside_core::control::GrowingFile {
                path: self.file.clone(),
                state: self.growing.clone(),
            },
            hls_job: None,
            content_type: "audio/mp4",
            duration_ms: Some(12_000),
        })
    }
    fn providers(&self) -> BoxFuture<'_, Vec<ProviderStatus>> {
        self.open.providers()
    }
    fn configure(&self, c: ProviderChange) -> BoxFuture<'_, Result<(), AcquireError>> {
        self.open.configure(c)
    }
    fn test_provider<'a>(
        &'a self,
        n: &'a str,
        t: &'a str,
    ) -> BoxFuture<'a, Result<ProviderTest, AcquireError>> {
        self.open.test_provider(n, t)
    }
    fn update_ytdlp(&self) -> BoxFuture<'_, Result<(), AcquireError>> {
        self.open.update_ytdlp()
    }
    fn retry<'a>(&'a self, id: &'a AcquisitionId) -> BoxFuture<'a, Result<(), AcquireError>> {
        self.open.retry(id)
    }
    fn admin_list(
        &self,
        f: AcquisitionFilter,
    ) -> BoxFuture<'_, Result<Page<AdminAcquisitionView>, AcquireError>> {
        self.open.admin_list(f)
    }
}

#[tokio::test]
async fn instant_play_without_an_acquirer_job_starts_one_hls_job_per_acquisition() -> TestResult {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("growing.m4a");
    std::fs::write(&file, b"partial")?;
    let (_grow, growing) =
        tokio::sync::watch::channel(trackside_core::control::GrowState::Growing { bytes: 7 });
    let fake = Arc::new(trackside_media::testing::FakeTranscoder::new());
    fake.set_hls(trackside_media::testing::FakeHls::Manual);
    let acquire = Arc::new(InstantWithoutHls {
        open: OpenAcquisition,
        file,
        growing,
    });
    let server = start_with_transcoder(acquire, fake.clone()).await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let id = random_id::<AcquisitionId>();
    let other = random_id::<AcquisitionId>();
    let ask = |id: &AcquisitionId| {
        server
            .http
            .get(server.url(&format!("/acquisitions/{id}/stream.m3u8")))
            .bearer_auth(&key)
            .send()
    };
    let (first, second) = tokio::join!(ask(&id), ask(&id));
    assert_eq!(first?.status(), 200);
    assert_eq!(second?.status(), 200);
    assert_eq!(ask(&id).await?.status(), 200);
    assert_eq!(fake.spawned().len(), 1, "one job for one acquisition");
    assert_eq!(ask(&other).await?.status(), 200);
    assert_eq!(fake.spawned().len(), 2);
    Ok(())
}

#[tokio::test]
async fn an_https_refusal_names_the_https_addresses() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let key = server.device_key().await?;
    let refuse = async || -> Result<Value, Box<dyn std::error::Error>> {
        let response = server
            .http
            .get(server.url("/me"))
            .header("x-test-peer", "203.0.113.7:4000")
            // Nothing a client sends may become an address.
            .header("host", "evil.example")
            .header("x-forwarded-host", "evil.example")
            .bearer_auth(&key)
            .send()
            .await?;
        assert_eq!(response.status(), 403);
        Ok(response.json().await?)
    };
    let before = refuse().await?;
    assert_eq!(before["error"]["code"], "https_required");
    assert!(
        before["error"]["details"].is_null(),
        "no https address yet: {before}"
    );
    let patched = server
        .http
        .patch(server.url("/admin/settings"))
        .headers(server.session(&cookie))
        .json(&json!({"server.public_urls": ["http://plain.example.com", "https://music.example.com/"]}))
        .send()
        .await?;
    assert_eq!(patched.status(), 200);
    let after = refuse().await?;
    assert_eq!(
        after["error"]["details"]["https_urls"],
        json!([{"url": "https://music.example.com", "kind": "public", "tls": true}]),
        "{after}"
    );
    assert!(after["error"]["request_id"].is_string());
    Ok(())
}
