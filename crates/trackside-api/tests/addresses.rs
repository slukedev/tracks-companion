//! Address updates for paired devices (`GET /server/urls`, SSE `server_urls`) and the limits on
//! fetched songs in the settings and overview, over real HTTP.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use trackside_api::{ApiState, LocalState, NoOps, ServerInfo, wire};
use trackside_auth::{Auth, SecretKey};
use trackside_core::control::{
    AcquireControl, AcquireError, AcquireFeature, AcquireRequest, AcquisitionFilter,
    AcquisitionView, AdminAcquisitionView, ExternalQuery, ExternalSearch, FetchedUsage,
    InstantStream, ProviderChange, ProviderHealthState, ProviderStats, ProviderStatus,
    ProviderTest,
};
use trackside_core::testing::{FakeAcquireControl, FakeScanControl, TestDb};
use trackside_core::{Access, AcquisitionId, BoxFuture, Config, Page, Permissions};
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

/// The core fake, with one provider, fetched-song usage, and the limit changes it was sent.
#[derive(Debug, Default)]
struct Fetching {
    inner: FakeAcquireControl,
    limits: Mutex<Vec<(u32, u64)>>,
}

impl AcquireControl for Fetching {
    fn feature_for(
        &self,
        caller: &Access,
        permissions: &Permissions,
        request: bool,
    ) -> Option<AcquireFeature> {
        self.inner.feature_for(caller, permissions, request)
    }
    fn search<'a>(
        &'a self,
        a: &'a Access,
        q: ExternalQuery,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        self.inner.search(a, q)
    }
    fn search_more<'a>(
        &'a self,
        a: &'a Access,
        s: &'a str,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        self.inner.search_more(a, s)
    }
    fn request<'a>(
        &'a self,
        a: &'a Access,
        r: AcquireRequest,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        self.inner.request(a, r)
    }
    fn get<'a>(
        &'a self,
        a: &'a Access,
        id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        self.inner.get(a, id)
    }
    fn list<'a>(
        &'a self,
        a: &'a Access,
        f: AcquisitionFilter,
    ) -> BoxFuture<'a, Result<Page<AcquisitionView>, AcquireError>> {
        self.inner.list(a, f)
    }
    fn cancel<'a>(
        &'a self,
        a: &'a Access,
        id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<(), AcquireError>> {
        self.inner.cancel(a, id)
    }
    fn instant_stream<'a>(
        &'a self,
        a: &'a Access,
        id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<InstantStream, AcquireError>> {
        self.inner.instant_stream(a, id)
    }
    fn fetched_usage(&self) -> Option<FetchedUsage> {
        let (keep, max_bytes) = self
            .limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .copied()
            .unwrap_or((500, 20 << 30));
        Some(FetchedUsage {
            songs: 3,
            bytes: 30_000_000,
            fetching: 1,
            keep,
            max_bytes,
        })
    }
    fn providers(&self) -> BoxFuture<'_, Vec<ProviderStatus>> {
        Box::pin(async {
            vec![ProviderStatus {
                name: String::from("ytdlp"),
                enabled: true,
                configured: true,
                health: ProviderHealthState::Ok,
                detail: None,
                cooldown_until: None,
                version: None,
                latest_known: None,
                settings: json!({}),
                stats: ProviderStats::default(),
            }]
        })
    }
    fn configure(&self, change: ProviderChange) -> BoxFuture<'_, Result<(), AcquireError>> {
        if let ProviderChange::Fetched { keep, max_bytes } = change {
            self.limits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((keep, max_bytes));
        }
        Box::pin(async { Ok(()) })
    }
    fn test_provider<'a>(
        &'a self,
        n: &'a str,
        q: &'a str,
    ) -> BoxFuture<'a, Result<ProviderTest, AcquireError>> {
        self.inner.test_provider(n, q)
    }
    fn update_ytdlp(&self) -> BoxFuture<'_, Result<(), AcquireError>> {
        self.inner.update_ytdlp()
    }
    fn retry<'a>(&'a self, id: &'a AcquisitionId) -> BoxFuture<'a, Result<(), AcquireError>> {
        self.inner.retry(id)
    }
    fn admin_list(
        &self,
        f: AcquisitionFilter,
    ) -> BoxFuture<'_, Result<Page<AdminAcquisitionView>, AcquireError>> {
        self.inner.admin_list(f)
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
    acquire: Arc<Fetching>,
    _test: TestDb,
    data: tempfile::TempDir,
    stop: CancellationToken,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn lan(url: &str) -> wire::auth::ServerUrl {
    wire::auth::ServerUrl {
        url: url.to_owned(),
        kind: wire::auth::UrlKind::Lan,
        tls: false,
        fingerprint: None,
    }
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
    let server = ServerInfo::new(
        uuid::Uuid::new_v4(),
        "Home",
        trackside_core::Clock::now(&*test.clock),
    );
    server.urls.set(vec![lan("http://192.168.1.20:4747")]);
    let acquire = Arc::new(Fetching::default());
    let state = ApiState {
        db: test.db.clone(),
        auth: Arc::clone(&auth),
        media,
        scan: Arc::new(FakeScanControl::new()),
        acquire: Arc::clone(&acquire) as Arc<dyn AcquireControl>,
        enrich: Arc::new(trackside_core::NoEnrich),
        events: test.events.clone(),
        config: Arc::new(config),
        server,
        health: Arc::new(Health),
        ops: Arc::new(NoOps),
        local: Arc::new(LocalState::new(stop.clone())),
    };
    let app = trackside_api::router(state);
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
        acquire,
        _test: test,
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
            .json(&json!({"setup_token": token, "username": "luke", "password": "correct horse battery"}))
            .send()
            .await?;
        assert_eq!(response.status(), 201);
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

    async fn device_key(&self) -> Result<String, Box<dyn std::error::Error>> {
        let response = self
            .http
            .post(self.url("/auth/login"))
            .json(&json!({"username": "luke", "password": "correct horse battery",
                "device": {"client_device_id": "8C2E", "name": "Luke's iPhone", "platform": "ios"}}))
            .send()
            .await?;
        assert_eq!(response.status(), 201);
        let body: wire::auth::PairResult = response.json().await?;
        Ok(body.api_key)
    }

    async fn urls(&self, key: &str) -> Result<wire::auth::ServerUrls, Box<dyn std::error::Error>> {
        let response = self
            .http
            .get(self.url("/server/urls"))
            .bearer_auth(key)
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        Ok(response.json().await?)
    }

    async fn patch_settings(
        &self,
        cookie: &str,
        body: Value,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        let response = self
            .http
            .patch(self.url("/admin/settings"))
            .headers(self.session(cookie))
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await?;
        assert_eq!(status, 200, "{body}");
        Ok(body)
    }
}

#[tokio::test]
async fn only_an_authenticated_device_reads_the_addresses() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let anonymous = server.http.get(server.url("/server/urls")).send().await?;
    assert_eq!(anonymous.status(), 401);
    let wrong = server
        .http
        .get(server.url("/server/urls"))
        .bearer_auth("tsk_not-a-key")
        .send()
        .await?;
    assert_eq!(wrong.status(), 401);
    let in_query = server
        .http
        .get(server.url("/server/urls?api_key=tsk_x"))
        .send()
        .await?;
    assert_eq!(in_query.status(), 400, "a key in the query is refused");

    let key = server.device_key().await?;
    let body = server.urls(&key).await?;
    let me: Value = server
        .http
        .get(server.url("/me"))
        .bearer_auth(&key)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(serde_json::to_value(&body.urls)?, me["server"]["urls"]);
    assert_eq!(
        body.server_id,
        me["server"]["server_id"].as_str().unwrap_or_default()
    );
    assert_eq!(body.version, trackside_api::build::urls_version(&body.urls));
    Ok(())
}

#[tokio::test]
async fn the_version_is_the_etag_and_unchanged_lists_are_304() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let response = server
        .http
        .get(server.url("/server/urls"))
        .bearer_auth(&key)
        .send()
        .await?;
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .ok_or("no etag")?
        .to_owned();
    let body: wire::auth::ServerUrls = response.json().await?;
    assert_eq!(etag, format!("\"{}\"", body.version));
    let again = server
        .http
        .get(server.url("/server/urls"))
        .bearer_auth(&key)
        .header("if-none-match", &etag)
        .send()
        .await?;
    assert_eq!(again.status(), 304);
    let stale = server
        .http
        .get(server.url("/server/urls"))
        .bearer_auth(&key)
        .header("if-none-match", "\"0000\"")
        .send()
        .await?;
    assert_eq!(stale.status(), 200);
    Ok(())
}

#[tokio::test]
async fn request_headers_never_add_an_address() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let plain = server.urls(&key).await?;
    let spoofed: wire::auth::ServerUrls = server
        .http
        .get(server.url("/server/urls"))
        .bearer_auth(&key)
        .header("host", "evil.example")
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(spoofed, plain);
    assert!(
        spoofed.urls.iter().all(|u| !u.url.contains("evil")),
        "{spoofed:?}"
    );
    // Forwarding headers from a peer that is not a trusted proxy: the key is refused over
    // plain HTTP, and the refusal names no address from them either.
    let forwarded = server
        .http
        .get(server.url("/server/urls"))
        .bearer_auth(&key)
        .header("x-forwarded-host", "evil.example")
        .header("x-forwarded-proto", "https")
        .header("forwarded", "host=evil.example;proto=https")
        .send()
        .await?;
    assert_eq!(forwarded.status(), 403);
    let body = forwarded.text().await?;
    assert!(
        body.contains("https_required") && !body.contains("evil"),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn a_new_public_address_reaches_open_streams_and_the_list() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let key = server.device_key().await?;
    let before = server.urls(&key).await?;
    let mut stream = server
        .http
        .get(server.url("/events"))
        .bearer_auth(&key)
        .send()
        .await?;
    let mut seen = String::new();
    let mut read_until = async |want: &str| -> TestResult {
        while !seen.contains(want) {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.chunk())
                .await??
                .ok_or("stream ended")?;
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        Ok(())
    };
    read_until("event: hello").await?;
    server
        .patch_settings(
            &cookie,
            json!({"server.public_urls": ["https://music.example.com/"]}),
        )
        .await?;
    let after = server.urls(&key).await?;
    assert_ne!(after.version, before.version);
    let added = after.urls.last().ok_or("no urls")?;
    assert_eq!(added.url, "https://music.example.com");
    assert_eq!(added.kind, wire::auth::UrlKind::Public);
    assert!(added.tls);
    read_until(&format!("data: {{\"version\":\"{}\"}}", after.version)).await?;
    assert!(seen.contains("event: server_urls"), "{seen}");
    Ok(())
}

#[tokio::test]
async fn fetched_song_limits_are_settings_that_apply_at_once() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let settings: Value = server
        .http
        .get(server.url("/admin/settings"))
        .headers(server.session(&cookie))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(
        settings["acquire.fetched_keep"],
        json!({"value": 500, "source": "default", "editable": true})
    );
    assert_eq!(
        settings["acquire.fetched_max_bytes"],
        json!({"value": "20GiB", "source": "default", "editable": true})
    );

    let changed = server
        .patch_settings(
            &cookie,
            json!({"acquire.fetched_keep": 100, "acquire.fetched_max_bytes": "5GiB"}),
        )
        .await?;
    assert_eq!(changed["acquire.fetched_keep"]["value"], 100);
    assert_eq!(changed["acquire.fetched_max_bytes"]["source"], "dashboard");
    server
        .patch_settings(&cookie, json!({"acquire.fetched_keep": 0}))
        .await?;
    server
        .patch_settings(&cookie, json!({"acquire.fetched_max_bytes": null}))
        .await?;
    assert_eq!(
        *server
            .acquire
            .limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![(100, 5 << 30), (0, 5 << 30), (0, 20 << 30)],
        "each change sends both limits as they now stand"
    );

    for bad in [
        json!({"acquire.fetched_keep": -1}),
        json!({"acquire.fetched_max_bytes": "lots"}),
    ] {
        let response = server
            .http
            .patch(server.url("/admin/settings"))
            .headers(server.session(&cookie))
            .json(&bad)
            .send()
            .await?;
        assert_eq!(response.status(), 400, "{bad}");
    }

    let overview: Value = server
        .http
        .get(server.url("/admin/overview"))
        .headers(server.session(&cookie))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(
        overview["acquisition"]["fetched"],
        json!({"songs": 3, "bytes": 30_000_000, "fetching": 1, "keep": 0, "max_bytes": 20_u64 << 30})
    );
    Ok(())
}
