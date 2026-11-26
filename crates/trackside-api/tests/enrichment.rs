//! Suggestions over real HTTP: the Last.fm key as a sealed, write-only dashboard setting, the
//! status and test endpoints, not-yet-owned suggestions only for callers who may request, and
//! every shelf (similar, top songs, On Repeat, picks) answering from local signals without it.

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
    fn instant_stream<'a>(
        &'a self,
        _: &'a Access,
        _: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<InstantStream, AcquireError>> {
        Box::pin(async { Err(AcquireError::NotFound) })
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
    /// Lookups something waited for. A request must never.
    waited: Mutex<Vec<Target>>,
    tested: Mutex<Vec<Option<String>>>,
}

impl EnrichControl for FakeEnrich {
    fn ensure(&self, targets: Vec<Target>, wait: Duration) -> BoxFuture<'_, ()> {
        self.waited
            .lock()
            .expect("lock")
            .extend(targets.iter().cloned());
        self.lookups.lock().expect("lock").extend(targets);
        // As a slow Last.fm would: a request that waited would show it.
        Box::pin(tokio::time::sleep(wait))
    }
    fn queue(&self, targets: Vec<Target>) {
        self.lookups.lock().expect("lock").extend(targets);
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
    media: Arc<Media>,
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
    let subsonic = trackside_subsonic::SubsonicState {
        db: test.db.clone(),
        auth: Arc::clone(&auth),
        media: Arc::clone(&media),
        scan: Arc::new(FakeScanControl::new()),
        enrich: enrich.clone(),
        config: Arc::new(config.clone()),
        server_id: uuid::Uuid::nil(),
    };
    let state = ApiState {
        db: test.db.clone(),
        auth: Arc::clone(&auth),
        media: Arc::clone(&media),
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
    let app = trackside_api::router(state).merge(trackside_subsonic::router(subsonic));
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
        media,
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
    server.test.clock.advance(Duration::from_mins(11));
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

/// Two albums of one genre and a third of another, no Last.fm data.
fn plain_library(test: &TestDb) -> Result<Vec<SongId>, Box<dyn std::error::Error>> {
    let library = test.add_library("Music", Path::new("/music"))?;
    let mut files = Vec::new();
    for (artist, album, year, genre, titles) in [
        (
            "Radiohead",
            "OK Computer",
            1997,
            "Rock",
            ["Airbag", "Paranoid Android", "Lucky"],
        ),
        (
            "Blur",
            "Blur",
            1997,
            "Rock",
            ["Beetlebum", "Song 2", "M.O.R."],
        ),
        (
            "Portishead",
            "Dummy",
            1994,
            "Trip Hop",
            ["Roads", "Glory Box", "Numb"],
        ),
    ] {
        for title in titles {
            let mut file = scanned(&format!("{title}.flac"), title, artist, Some(album));
            file.tags.year = Some(year);
            file.tags.genres = vec![genre.to_owned()];
            files.push(file);
        }
    }
    Ok(test
        .ingest(library.key, &files)?
        .iter()
        .map(|r| r.song().clone())
        .collect())
}

fn song_titles(body: &Value) -> Vec<String> {
    body["songs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["title"].as_str().map(str::to_owned))
        .collect()
}

async fn listen(
    server: &Server,
    key: &str,
    plays: &[(&SongId, &str)],
) -> Result<(), Box<dyn std::error::Error>> {
    let listens: Vec<Value> = plays
        .iter()
        .map(|(song, at)| json!({"client_listen_id": format!("{song}-{at}"), "song_id": song.to_string(), "started_at": at}))
        .collect();
    let response = server
        .http
        .post(server.url("/listens"))
        .bearer_auth(key)
        .json(&json!({ "listens": listens }))
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    Ok(())
}

#[tokio::test]
async fn every_shelf_answers_without_last_fm() -> TestResult {
    let server = start(Arc::new(FakeAcquireControl::new()), false).await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let ids = plain_library(&server.test)?;
    let paranoid = &ids[1];

    let similar = get_json(&server, &key, &format!("/songs/{paranoid}/similar")).await?;
    let found = song_titles(&similar);
    assert_eq!(&found[..2], ["Airbag", "Lucky"], "{found:?}");
    assert!(found.contains(&String::from("Song 2")), "{found:?}");
    let song = get_json(&server, &key, &format!("/songs/{paranoid}")).await?;
    let radiohead = song["artist_id"].as_str().ok_or("no artist id")?.to_owned();
    let artists = get_json(&server, &key, &format!("/artists/{radiohead}/similar")).await?;
    assert_eq!(artists["artists"][0]["name"], "Blur", "{artists}");
    let top = get_json(&server, &key, &format!("/artists/{radiohead}/top-songs")).await?;
    assert_eq!(song_titles(&top).len(), 3);

    for path in [
        "/songs/top",
        "/me/mixes/onRepeat",
        "/me/mixes/rediscover",
        "/me/get-more",
    ] {
        assert_eq!(
            get_json(&server, &key, path).await?,
            json!({"songs": []}),
            "{path} before any listens"
        );
    }
    let picks = get_json(&server, &key, "/me/picks").await?;
    assert!(!song_titles(&picks).is_empty(), "{picks}");
    assert!(picks.get("external").is_none());
    let bad = server
        .http
        .get(server.url("/me/picks?tz=Not/AZone"))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(bad.status(), 400);

    // Six songs three times each in the last few days: enough for the engine's On Repeat.
    let played = [0, 3, 4, 5, 6, 8];
    let times = [
        "2026-11-14T08:00:00.000Z",
        "2026-11-13T08:00:00.000Z",
        "2026-11-12T08:00:00.000Z",
    ];
    let mut plays: Vec<(&SongId, &str)> = Vec::new();
    for i in played {
        plays.extend(times.iter().map(|at| (&ids[i], *at)));
    }
    listen(&server, &key, &plays).await?;
    let on_repeat = get_json(&server, &key, "/me/mixes/onRepeat?tz=UTC").await?;
    let mut found = song_titles(&on_repeat);
    found.sort();
    assert_eq!(
        found,
        ["Airbag", "Beetlebum", "M.O.R.", "Numb", "Roads", "Song 2"]
    );
    assert_eq!(on_repeat["songs"][0]["me"]["play_count"], 3);
    let top = get_json(&server, &key, "/songs/top?count=3").await?;
    assert_eq!(song_titles(&top).len(), 3);
    let picks = get_json(&server, &key, "/me/picks").await?;
    let mut found = song_titles(&picks);
    found.sort();
    assert_eq!(
        found,
        ["Glory Box", "Lucky", "Paranoid Android"],
        "unplayed only"
    );
    let more = get_json(&server, &key, "/me/get-more").await?;
    let mut found = song_titles(&more);
    found.sort();
    assert_eq!(found, ["Glory Box", "Lucky", "Paranoid Android"]);
    assert!(more.get("external").is_none());
    let lookups = server.enrich.lookups.lock().unwrap().clone();
    assert!(
        lookups.contains(&Target::artist("Portishead")),
        "{lookups:?}"
    );
    Ok(())
}

#[tokio::test]
async fn picks_merge_last_fm_and_in_library_artists_come_first_when_asked() -> TestResult {
    let server = start(Arc::new(OpenAcquisition), false).await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let creep = seed(&server.test)?;
    listen(&server, &key, &[(&creep, "2026-11-14T08:00:00.000Z")]).await?;
    let picks = get_json(&server, &key, "/me/picks").await?;
    assert_eq!(song_titles(&picks), ["Roads"]);
    assert_eq!(
        picks["external"],
        json!([{"title": "Glory Box", "artist": "Portishead"}])
    );
    let more = get_json(&server, &key, "/me/get-more").await?;
    assert_eq!(
        more["external"],
        json!([{"title": "No Surprises", "artist": "Radiohead"}]),
        "{more}"
    );

    let elsewhere: Vec<Related> = (0..5)
        .map(|i| Related {
            title: String::new(),
            artist: format!("Nobody Here {i}"),
            score: 0.95,
        })
        .collect();
    server.test.db.write_blocking(move |tx| {
        store::store_artist(
            tx,
            "radiohead",
            "Radiohead",
            &ArtistFacts {
                similar: [
                    elsewhere,
                    vec![Related {
                        title: String::new(),
                        artist: String::from("Portishead"),
                        score: 0.5,
                    }],
                ]
                .concat(),
                top: Vec::new(),
                bio: None,
            },
            86_400_000,
        )
    })?;
    let song: Value = get_json(&server, &key, &format!("/songs/{creep}")).await?;
    let artist = song["artist_id"].as_str().ok_or("no artist id")?.to_owned();
    let all = get_json(&server, &key, &format!("/artists/{artist}/similar?count=3")).await?;
    assert!(
        all["artists"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|a| a["in_library"] == false),
        "{all}"
    );
    let ours = get_json(
        &server,
        &key,
        &format!("/artists/{artist}/similar?count=3&in_library=true"),
    )
    .await?;
    assert_eq!(ours["artists"].as_array().map(Vec::len), Some(1), "{ours}");
    assert_eq!(ours["artists"][0]["name"], "Portishead");
    Ok(())
}

/// A 16 by 16 grey PNG, small enough to write by hand: stored (uncompressed) deflate.
fn tiny_png() -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    let mut raw = Vec::new();
    for _ in 0..16 {
        raw.push(0);
        raw.extend_from_slice(&[0x80; 16]);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    let len = raw.len() as u16;
    let mut zlib = vec![0x78, 0x01, 0x01];
    zlib.extend_from_slice(&len.to_le_bytes());
    zlib.extend_from_slice(&(!len).to_le_bytes());
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut header = Vec::new();
    header.extend_from_slice(&16u32.to_be_bytes());
    header.extend_from_slice(&16u32.to_be_bytes());
    header.extend_from_slice(&[8, 0, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Glory Box's cover, as the worker would have kept it.
fn keep_cover(
    server: &Server,
) -> Result<trackside_core::ExternalArtId, Box<dyn std::error::Error>> {
    let id = trackside_core::ExternalArtId::derive("4128a6eb29f94943c9d206c08e625904");
    server
        .media
        .artwork_store()
        .put_external(&id, &tiny_png())?;
    let (stored, key) = (
        id.clone(),
        tracks_model::song_key("Glory Box", "Portishead"),
    );
    server
        .test
        .db
        .write_blocking(move |tx| store::store_art(tx, &key, Some(&stored), 86_400_000))?;
    Ok(id)
}

#[tokio::test]
async fn suggestions_carry_their_cover_once_it_was_looked_up() -> TestResult {
    let server = start(Arc::new(OpenAcquisition), false).await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let creep = seed(&server.test)?;
    let path = format!("/songs/{creep}/similar");
    let before = get_json(&server, &key, &path).await?;
    assert_eq!(
        before["external"],
        json!([{"title": "Glory Box", "artist": "Portishead"}]),
        "no cover until it is looked up"
    );
    assert!(
        server
            .enrich
            .lookups
            .lock()
            .unwrap()
            .contains(&Target::art("Glory Box", "Portishead")),
        "the cover is looked up in the background"
    );
    let id = keep_cover(&server)?;
    let after = get_json(&server, &key, &path).await?;
    assert_eq!(
        after["external"],
        json!([{"title": "Glory Box", "artist": "Portishead", "artwork_id": id.to_string()}])
    );
    assert!(
        !after.to_string().contains("http"),
        "no third-party URL: {after}"
    );

    // Served like library art: with a media token, resized on request, cacheable.
    let token: Value = server
        .http
        .post(server.url("/media-token"))
        .bearer_auth(&key)
        .send()
        .await?
        .json()
        .await?;
    let mt = token["token"].as_str().ok_or("no media token")?;
    let art = server
        .http
        .get(server.url(&format!("/art/{id}?size=600&mt={mt}")))
        .send()
        .await?;
    assert_eq!(art.status(), 200);
    assert_eq!(art.headers()["content-type"], "image/png");
    assert_eq!(
        art.headers()["cache-control"],
        "private, max-age=31536000, immutable"
    );
    assert_eq!(art.bytes().await?.as_ref(), tiny_png().as_slice());
    let anonymous = server
        .http
        .get(server.url(&format!("/art/{id}")))
        .send()
        .await?;
    assert_eq!(anonymous.status(), 401);
    let unknown = trackside_core::ExternalArtId::derive("not shown anywhere");
    let missing = server
        .http
        .get(server.url(&format!("/art/{unknown}?mt={mt}")))
        .send()
        .await?;
    assert_eq!(missing.status(), 404, "only covers some suggestion shows");
    Ok(())
}

#[tokio::test]
async fn covers_are_only_for_callers_who_see_suggestions_to_request() -> TestResult {
    let server = start(Arc::new(FakeAcquireControl::new()), false).await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    seed(&server.test)?;
    let id = keep_cover(&server)?;
    let art = server
        .http
        .get(server.url(&format!("/art/{id}")))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(art.status(), 404);
    Ok(())
}

#[tokio::test]
async fn a_suggestion_request_never_waits_for_last_fm() -> TestResult {
    let server = start(Arc::new(OpenAcquisition), false).await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let creep = seed(&server.test)?;
    listen(&server, &key, &[(&creep, "2026-11-14T08:00:00.000Z")]).await?;
    let song: Value = get_json(&server, &key, &format!("/songs/{creep}")).await?;
    let artist = song["artist_id"].as_str().ok_or("no artist id")?.to_owned();
    let started = std::time::Instant::now();
    for path in [
        String::from("/me/picks"),
        String::from("/me/get-more"),
        String::from("/me/mixes/onRepeat"),
        String::from("/songs/top"),
        format!("/songs/{creep}/similar"),
        format!("/artists/{artist}/similar"),
        format!("/artists/{artist}/top-songs"),
        format!("/artists/{artist}"),
    ] {
        get_json(&server, &key, &path).await?;
    }
    for endpoint in [
        format!("getSimilarSongs2?id={creep}"),
        format!("getSimilarSongs2?id={artist}"),
        format!("getTopSongs?id={artist}"),
        format!("getArtistInfo2?id={artist}"),
    ] {
        let response = server
            .http
            .get(format!(
                "{}/rest/{endpoint}&apiKey={key}&f=json&v=1.16.1&c=test",
                server.host
            ))
            .send()
            .await?;
        assert_eq!(response.status(), 200, "{endpoint}");
        let body: Value = response.json().await?;
        assert_eq!(
            body["subsonic-response"]["status"], "ok",
            "{endpoint}: {body}"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "took {:?}",
        started.elapsed()
    );
    assert!(server.enrich.waited.lock().unwrap().is_empty());
    let lookups = server.enrich.lookups.lock().unwrap().clone();
    assert!(
        lookups.contains(&Target::song("Creep", "Radiohead")),
        "{lookups:?}"
    );
    assert!(lookups.contains(&Target::artist("Radiohead")));
    Ok(())
}

#[tokio::test]
async fn unchanged_suggestions_answer_304() -> TestResult {
    let server = start(Arc::new(OpenAcquisition), false).await?;
    server.setup_admin().await?;
    let key = server.device_key().await?;
    let creep = seed(&server.test)?;
    let path = server.url(&format!("/songs/{creep}/similar"));
    let first = server.http.get(&path).bearer_auth(&key).send().await?;
    assert_eq!(first.status(), 200);
    let etag = first.headers()["etag"].to_str()?.to_owned();
    let again = server
        .http
        .get(&path)
        .bearer_auth(&key)
        .header("if-none-match", &etag)
        .send()
        .await?;
    assert_eq!(again.status(), 304);
    keep_cover(&server)?;
    let changed = server
        .http
        .get(&path)
        .bearer_auth(&key)
        .header("if-none-match", &etag)
        .send()
        .await?;
    assert_eq!(changed.status(), 200, "a cover looked up is a change");
    Ok(())
}
