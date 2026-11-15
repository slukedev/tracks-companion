//! Last.fm suggestions over real HTTP: the key as a sealed, write-only dashboard setting, the
//! status and test endpoints, and not-yet-owned suggestions only for callers who may request.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use trackside_api::{ApiState, LocalState, NoOps, ServerInfo, wire};
use trackside_auth::{Auth, SecretKey};
use trackside_core::config::{ConfigReport, ConfigSource, Secret};
use trackside_core::control::{
    AcquireControl, AcquireError, AcquireFeature, AcquireRequest, AcquisitionFilter,
    AcquisitionView, AdminAcquisitionView, ExternalQuery, ExternalSearch, InstantStream,
    ProviderChange, ProviderStatus, ProviderTest,
};
use trackside_core::enrich::{EnrichControl, EnrichStatus, EnrichTest, Target};
use trackside_core::repo::enrichment::{self as store, ArtistFacts, Related};
use trackside_core::testing::{FakeAcquireControl, FakeScanControl, TestDb, scanned};
use trackside_core::{
    Access, AcquisitionId, BoxFuture, Clock, Config, Millis, Page, Permissions, SongId,
};
use trackside_media::{Media, MediaError, TranscodeProcess, TranscodeSpec, Transcoder};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const KEY: &str = "0123456789abcdef0123456789abcdef";

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

/// Acquisition on for everyone who may request; nothing else is used here.
#[derive(Debug)]
struct OpenAcquisition;

fn absent<'a, T: Send + 'a>() -> BoxFuture<'a, Result<T, AcquireError>> {
    Box::pin(async { Err(AcquireError::NotFound) })
}

impl AcquireControl for OpenAcquisition {
    fn feature_for(&self, a: &Access, p: &Permissions, request: bool) -> Option<AcquireFeature> {
        (request && (a.is_admin() || p.can_request)).then_some(AcquireFeature {
            instant_play: false,
            external_play: false,
        })
    }
    fn search<'a>(
        &'a self,
        _: &'a Access,
        _: ExternalQuery,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        absent()
    }
    fn search_more<'a>(
        &'a self,
        _: &'a Access,
        _: &'a str,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        absent()
    }
    fn request<'a>(
        &'a self,
        _: &'a Access,
        _: AcquireRequest,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        absent()
    }
    fn get<'a>(
        &'a self,
        _: &'a Access,
        _: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        absent()
    }
    fn list<'a>(
        &'a self,
        _: &'a Access,
        _: AcquisitionFilter,
    ) -> BoxFuture<'a, Result<Page<AcquisitionView>, AcquireError>> {
        absent()
    }
    fn cancel<'a>(
        &'a self,
        _: &'a Access,
        _: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<(), AcquireError>> {
        absent()
    }
    fn instant_stream(&self, _: &Access, _: &AcquisitionId) -> Result<InstantStream, AcquireError> {
        Err(AcquireError::NotFound)
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
        absent()
    }
    fn update_ytdlp(&self) -> BoxFuture<'_, Result<(), AcquireError>> {
        Box::pin(async { Ok(()) })
    }
    fn retry<'a>(&'a self, _: &'a AcquisitionId) -> BoxFuture<'a, Result<(), AcquireError>> {
        absent()
    }
    fn admin_list(
        &self,
        _: AcquisitionFilter,
    ) -> BoxFuture<'_, Result<Page<AdminAcquisitionView>, AcquireError>> {
        Box::pin(async { Ok(Page::default()) })
    }
}

/// Remembers the keys it was given and the lookups asked for.
#[derive(Debug, Default)]
struct FakeEnrich {
    keys: Mutex<Vec<Option<String>>>,
    lookups: Mutex<Vec<Target>>,
    tested: Mutex<Vec<Option<String>>>,
}

impl EnrichControl for FakeEnrich {
    fn ensure(&self, targets: Vec<Target>, _wait: Duration) -> BoxFuture<'_, ()> {
        self.lookups.lock().expect("lock").extend(targets);
        Box::pin(async {})
    }
    fn set_key(&self, key: Option<Secret>) {
        self.keys
            .lock()
            .expect("lock")
            .push(key.map(|k| k.expose().to_owned()));
    }
    fn status(&self) -> BoxFuture<'_, EnrichStatus> {
        Box::pin(async {
            EnrichStatus {
                configured: true,
                artists: 12,
                songs: 30,
                last_fetch: Some(Millis(1_794_648_600_000)),
                last_error: Some((
                    Millis(1_794_648_000_000),
                    String::from("Last.fm error 29: Rate Limit Exceeded"),
                )),
            }
        })
    }
    fn test(&self, key: Option<Secret>) -> BoxFuture<'_, EnrichTest> {
        let ok = key.as_ref().is_some_and(|k| k.expose() == KEY);
        self.tested
            .lock()
            .expect("lock")
            .push(key.map(|k| k.expose().to_owned()));
        Box::pin(async move {
            EnrichTest {
                ok,
                elapsed: Duration::from_millis(120),
                detail: (!ok).then(|| String::from("Last.fm error 10: Invalid API key")),
            }
        })
    }
}

struct Server {
    base: String,
    host: String,
    http: reqwest::Client,
    auth: Arc<Auth>,
    test: TestDb,
    enrich: Arc<FakeEnrich>,
    data: tempfile::TempDir,
    stop: CancellationToken,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
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

async fn start(
    acquire: Arc<dyn AcquireControl>,
    key_from_config: bool,
) -> Result<Server, Box<dyn std::error::Error>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let test = TestDb::new()?;
    let data = tempfile::tempdir()?;
    let mut config = Config::defaults(data.path());
    config.auth.argon2_memory = 1 << 16;
    config.auth.argon2_iterations = 1;
    let mut report = ConfigReport::default();
    if key_from_config {
        config.enrichment.lastfm_api_key = Some(Secret::new(KEY));
        report
            .sources
            .insert("enrichment.lastfm_api_key", ConfigSource::Env);
    }
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
    let enrich = Arc::new(FakeEnrich::default());
    let stop = CancellationToken::new();
    let mut server = ServerInfo::new(uuid::Uuid::new_v4(), "Home", Clock::now(&*test.clock));
    server.report = Arc::new(report);
    let state = ApiState {
        db: test.db.clone(),
        auth: Arc::clone(&auth),
        media,
        scan: Arc::new(FakeScanControl::new()),
        acquire,
        enrich: enrich.clone(),
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
        test,
        enrich,
        data,
        stop,
    })
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn setup_admin(&self) -> Result<HeaderMap, Box<dyn std::error::Error>> {
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
        assert_eq!(response.status(), 201);
        let cookie = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| v.split(';').next().map(str::to_owned))
            .ok_or("no cookie")?;
        let mut headers = HeaderMap::new();
        headers.insert("cookie", HeaderValue::from_str(&cookie)?);
        headers.insert("x-trackside-csrf", HeaderValue::from_static("1"));
        headers.insert("origin", HeaderValue::from_str(&self.host)?);
        Ok(headers)
    }

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

    async fn patch_key(
        &self,
        session: &HeaderMap,
        value: Value,
    ) -> Result<(u16, String), Box<dyn std::error::Error>> {
        let response = self
            .http
            .patch(self.url("/admin/settings"))
            .headers(session.clone())
            .json(&json!({ "enrichment.lastfm_api_key": value }))
            .send()
            .await?;
        Ok((response.status().as_u16(), response.text().await?))
    }
}

#[tokio::test]
async fn the_key_is_sealed_at_rest_and_never_sent_back() -> TestResult {
    let server = start(Arc::new(FakeAcquireControl::new()), false).await?;
    let session = server.setup_admin().await?;
    let (status, text) = server.patch_key(&session, json!(KEY)).await?;
    assert_eq!(status, 200, "{text}");
    assert!(!text.contains(KEY), "{text}");
    let settings: Value = serde_json::from_str(&text)?;
    assert_eq!(
        settings["enrichment.lastfm_api_key"],
        json!({"value": true, "source": "dashboard", "editable": true})
    );
    let read = server
        .http
        .get(server.url("/admin/settings"))
        .headers(session.clone())
        .send()
        .await?
        .text()
        .await?;
    assert!(!read.contains(KEY));
    let raw: String = server.test.db.read_blocking(|c| {
        Ok(
            c.query_row("SELECT group_concat(value, '') FROM settings", [], |r| {
                r.get(0)
            })?,
        )
    })?;
    assert!(!raw.contains(KEY), "stored in plain text");
    let audit = server
        .http
        .get(server.url("/admin/audit"))
        .headers(session.clone())
        .send()
        .await?
        .text()
        .await?;
    assert!(!audit.contains(KEY));
    let opened = trackside_api::build::stored_secret(
        &server.test.db,
        &server.auth,
        trackside_api::build::LASTFM_API_KEY,
    )
    .await;
    assert_eq!(opened.as_ref().map(Secret::expose), Some(KEY));
    assert_eq!(
        server.enrich.keys.lock().unwrap().clone(),
        [Some(String::from(KEY))]
    );

    let (status, text) = server.patch_key(&session, Value::Null).await?;
    assert_eq!(status, 200);
    let settings: Value = serde_json::from_str(&text)?;
    assert_eq!(settings["enrichment.lastfm_api_key"]["value"], false);
    assert_eq!(settings["enrichment.lastfm_api_key"]["source"], "default");
    assert_eq!(server.enrich.keys.lock().unwrap().last(), Some(&None));
    Ok(())
}

#[tokio::test]
async fn changing_the_key_needs_a_recent_password() -> TestResult {
    let server = start(Arc::new(FakeAcquireControl::new()), false).await?;
    let session = server.setup_admin().await?;
    server.test.clock.advance(Duration::from_secs(11 * 60));
    let (status, text) = server.patch_key(&session, json!(KEY)).await?;
    assert_eq!(status, 403);
    assert!(text.contains("reauth_required"), "{text}");
    assert!(server.enrich.keys.lock().unwrap().is_empty());
    let (status, text) = server.patch_key(&session, json!("not a key")).await?;
    assert_eq!(status, 403, "{text}");
    Ok(())
}

#[tokio::test]
async fn a_key_from_the_environment_is_locked() -> TestResult {
    let server = start(Arc::new(FakeAcquireControl::new()), true).await?;
    let session = server.setup_admin().await?;
    let settings: Value = server
        .http
        .get(server.url("/admin/settings"))
        .headers(session.clone())
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(
        settings["enrichment.lastfm_api_key"],
        json!({"value": true, "source": "config", "editable": false})
    );
    let (status, text) = server.patch_key(&session, json!("abc")).await?;
    assert_eq!(status, 409, "{text}");
    assert!(text.contains("set_by_config"));
    Ok(())
}

#[tokio::test]
async fn status_and_test_report_without_echoing_the_key() -> TestResult {
    let server = start(Arc::new(FakeAcquireControl::new()), false).await?;
    let session = server.setup_admin().await?;
    let status: wire::enrichment::EnrichmentStatus = server
        .http
        .get(server.url("/admin/enrichment"))
        .headers(session.clone())
        .send()
        .await?
        .json()
        .await?;
    assert_eq!((status.artists, status.songs), (12, 30));
    assert!(status.last_error.is_some_and(|e| e.message.contains("29")));

    let ok = server
        .http
        .post(server.url("/admin/enrichment/test"))
        .headers(session.clone())
        .json(&json!({ "api_key": KEY }))
        .send()
        .await?
        .text()
        .await?;
    assert!(!ok.contains(KEY));
    let ok: wire::enrichment::EnrichmentTestResult = serde_json::from_str(&ok)?;
    assert!(ok.ok);
    let current: wire::enrichment::EnrichmentTestResult = server
        .http
        .post(server.url("/admin/enrichment/test"))
        .headers(session.clone())
        .send()
        .await?
        .json()
        .await?;
    assert!(!current.ok);
    assert_eq!(
        server.enrich.tested.lock().unwrap().clone(),
        [Some(String::from(KEY)), None]
    );
    let key = server.device_key().await?;
    let as_app = server
        .http
        .get(server.url("/admin/enrichment"))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(as_app.status(), 403);
    Ok(())
}

/// Creep in the library; Last.fm relates it to Roads (here) and Glory Box (not here), and
/// ranks Radiohead's No Surprises, which is not here either.
fn seed(test: &TestDb) -> Result<SongId, Box<dyn std::error::Error>> {
    let library = test.add_library("Music", Path::new("/music"))?;
    let found = test.ingest(
        library.key,
        &[
            scanned("1.flac", "Creep", "Radiohead", Some("Pablo Honey")),
            scanned("2.flac", "Roads", "Portishead", Some("Dummy")),
        ],
    )?;
    let key = tracks_model::song_key("Creep", "Radiohead");
    let stored = key.clone();
    test.db.write_blocking(move |tx| {
        store::store_song(
            tx,
            &stored,
            &[
                Related {
                    title: String::from("Roads"),
                    artist: String::from("Portishead"),
                    score: 0.9,
                },
                Related {
                    title: String::from("Glory Box"),
                    artist: String::from("Portishead"),
                    score: 0.8,
                },
            ],
            86_400_000,
        )?;
        store::store_artist(
            tx,
            "radiohead",
            "Radiohead",
            &ArtistFacts {
                similar: Vec::new(),
                top: vec![String::from("Creep"), String::from("No Surprises")],
                bio: Some(String::from("An English rock band.")),
            },
            86_400_000,
        )
    })?;
    Ok(found[0].song().clone())
}

async fn get_json(
    server: &Server,
    key: &str,
    path: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = server
        .http
        .get(server.url(path))
        .bearer_auth(key)
        .send()
        .await?;
    assert_eq!(response.status(), 200, "{path}");
    Ok(response.json().await?)
}

#[tokio::test]
async fn suggestions_not_on_the_server_need_acquisition() -> TestResult {
    for (acquire, expected) in [
        (
            Arc::new(FakeAcquireControl::new()) as Arc<dyn AcquireControl>,
            false,
        ),
        (Arc::new(OpenAcquisition) as Arc<dyn AcquireControl>, true),
    ] {
        let server = start(acquire, false).await?;
        server.setup_admin().await?;
        let key = server.device_key().await?;
        let creep = seed(&server.test)?;
        let similar = get_json(&server, &key, &format!("/songs/{creep}/similar")).await?;
        let titles: Vec<&str> = similar["songs"]
            .as_array()
            .ok_or("no songs")?
            .iter()
            .filter_map(|s| s["title"].as_str())
            .collect();
        assert_eq!(titles.first(), Some(&"Roads"));
        if expected {
            assert_eq!(
                similar["external"],
                json!([{"title": "Glory Box", "artist": "Portishead"}])
            );
        } else {
            assert!(similar.get("external").is_none(), "{similar}");
        }
        assert!(!similar.to_string().to_lowercase().contains("last.fm"));

        let song: Value = get_json(&server, &key, &format!("/songs/{creep}")).await?;
        let artist = song["artist_id"].as_str().ok_or("no artist id")?.to_owned();
        let top = get_json(&server, &key, &format!("/artists/{artist}/top-songs")).await?;
        assert_eq!(top["songs"][0]["title"], "Creep");
        if expected {
            assert_eq!(
                top["external"],
                json!([{"title": "No Surprises", "artist": "Radiohead"}])
            );
        } else {
            assert!(top.get("external").is_none());
        }
        let page = get_json(&server, &key, &format!("/artists/{artist}")).await?;
        assert_eq!(page["bio"], "An English rock band.");
        let lookups = server.enrich.lookups.lock().unwrap().clone();
        assert!(lookups.contains(&Target::song("Creep", "Radiohead")));
        assert!(lookups.contains(&Target::artist("Radiohead")));
    }
    Ok(())
}
