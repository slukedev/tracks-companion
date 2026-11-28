//! The streaming catalog over real HTTP, against the fake Music Assistant of
//! `trackside-mass` (feature `testing`): which services are listed, search across them
//! with the services' answers interleaved, a slow service reported while the rest answer,
//! the short-lived cache, owned songs named by `song_id` and never mixed in, item detail
//! and paging, saved libraries, permissions and rate limits, and catalog artwork fetched
//! only from addresses Music Assistant gave, refused when private, and kept re-encoded.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use trackside_api::{ApiState, LocalState, NoOps, ServerInfo, wire};
use trackside_assistant::{Assistant, Deps, Tuning};
use trackside_auth::{Auth, SecretKey};
use trackside_core::testing::{FakeAcquireControl, FakeScanControl, TestDb, scanned};
use trackside_core::{BoxFuture, Config, ConfigReport, SongId};
use trackside_mass::testing::{FakeAssistant, Model, player, provider, queue};
use trackside_media::{Media, MediaError, TranscodeProcess, TranscodeSpec, Transcoder};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

const PASSWORD: &str = "correct horse battery";
const PATIENCE: Duration = Duration::from_secs(10);

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
    _test: TestDb,
    data: tempfile::TempDir,
    assistant: Assistant,
    stop: CancellationToken,
    /// The song every test library has.
    song: SongId,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn test_peer(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            9,
        ))));
    next.run(request).await
}

#[expect(
    clippy::too_many_lines,
    reason = "the whole server, wired as run.rs does"
)]
async fn start() -> Result<Server, Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let test = TestDb::new()?;
    let data = tempfile::tempdir()?;
    let library = test.add_library("Music", data.path())?;
    test.ingest(
        library.key,
        &[scanned("a/halo.flac", "Halo", "Beyoncé", Some("I Am"))],
    )?;
    let song = test.db.read_blocking(|c| {
        trackside_core::repo::songs::get_many(c, &trackside_core::Access::system(), &[])
            .map(|_| ())?;
        c.query_row("SELECT pid FROM songs LIMIT 1", [], |row| {
            row.get::<_, SongId>(0)
        })
        .map_err(Into::into)
    })?;
    let mut config = Config::defaults(data.path());
    config.auth.argon2_memory = 1 << 16;
    config.auth.argon2_iterations = 1;
    let config = Arc::new(config);
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address: SocketAddr = listener.local_addr()?;
    let lan = format!("http://{address}");
    let assistant = Assistant::start(Deps {
        db: test.db.clone(),
        auth: Arc::clone(&auth),
        events: test.events.clone(),
        config: Arc::clone(&config),
        report: Arc::new(ConfigReport::default()),
        server_name: String::from("Home"),
        lan_url: Arc::new(move || Some(lan.clone())),
        shutdown: stop.clone(),
        tuning: Tuning {
            unavailable_after: Duration::from_millis(400),
            command_timeout: Duration::from_secs(3),
            retry: Duration::from_millis(50),
            relay: trackside_assistant::RelayTuning::default(),
        },
    })
    .await;
    let state = ApiState {
        db: test.db.clone(),
        auth: Arc::clone(&auth),
        media: Arc::clone(&media),
        scan: Arc::new(FakeScanControl::new()),
        acquire: Arc::new(FakeAcquireControl::new()),
        enrich: Arc::new(trackside_core::NoEnrich),
        assistant: Arc::new(assistant.clone()),
        events: test.events.clone(),
        config: Arc::clone(&config),
        server: ServerInfo::new(
            uuid::Uuid::new_v4(),
            "Home",
            trackside_core::Clock::now(&*test.clock),
        ),
        health: Arc::new(Health),
        ops: Arc::new(NoOps),
        local: Arc::new(LocalState::new(stop.clone())),
    };
    let subsonic = trackside_subsonic::SubsonicState {
        db: test.db.clone(),
        auth: Arc::clone(&auth),
        media,
        scan: Arc::new(FakeScanControl::new()),
        enrich: Arc::new(trackside_core::NoEnrich),
        config: Arc::clone(&config),
        server_id: uuid::Uuid::nil(),
    };
    let app = trackside_api::router(state)
        .merge(trackside_subsonic::router_with(
            subsonic,
            trackside_core::now_playing::NowPlayingRegistry::new(),
        ))
        .layer(axum::middleware::from_fn(test_peer));
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
        assistant,
        stop,
        song,
    })
}

fn cookie_of(response: &reqwest::Response) -> Result<String, Error> {
    Ok(response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.split(';').next().map(str::to_owned))
        .ok_or("no cookie")?)
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn setup_admin(&self) -> Result<String, Error> {
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

    async fn admin(
        &self,
        cookie: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value), Error> {
        let mut request = self
            .http
            .request(method, self.url(&format!("/admin{path}")))
            .headers(self.session(cookie));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    async fn device_key(&self, user: &str, device: &str) -> Result<String, Error> {
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

    async fn call(
        &self,
        key: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value), Error> {
        let mut request = self.http.request(method, self.url(path)).bearer_auth(key);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    /// Points Trackside at the fake and waits until it is connected.
    async fn connect(&self, cookie: &str, fake: &FakeAssistant) -> Result<Value, Error> {
        let (status, body) = self
            .admin(
                cookie,
                reqwest::Method::PATCH,
                "/music-assistant",
                Some(json!({"url": fake.url(), "token": fake.token()})),
            )
            .await?;
        assert_eq!(status, 200, "{body}");
        self.until_admin(cookie, "/music-assistant", |b| {
            b["connection"]["state"] == "connected"
        })
        .await
    }

    async fn until_admin(
        &self,
        cookie: &str,
        path: &str,
        done: impl Fn(&Value) -> bool,
    ) -> Result<Value, Error> {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let (_, body) = self.admin(cookie, reqwest::Method::GET, path, None).await?;
            if done(&body) {
                return Ok(body);
            }
            if tokio::time::Instant::now() > deadline {
                return Err(format!("{path} never got there: {body}").into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn until(
        &self,
        key: &str,
        path: &str,
        done: impl Fn(u16, &Value) -> bool,
    ) -> Result<Value, Error> {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let (status, body) = self.call(key, reqwest::Method::GET, path, None).await?;
            if done(status, &body) {
                return Ok(body);
            }
            if tokio::time::Instant::now() > deadline {
                return Err(format!("{path} never got there: {status} {body}").into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

const SPOTIFY: &str = "spotify--Xb4LkP2q";
const TIDAL: &str = "tidal--Rt7mQ2";
const PROXY_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn service_id(instance: &str) -> String {
    trackside_core::ServiceId::derive(&Model::default().server_id, instance).to_string()
}

fn streaming(instance: &str, domain: &str, name: &str, features: &[&str]) -> Value {
    let mut value = provider(instance, domain, "music", name, true);
    value["supported_features"] = json!(features);
    value
}

fn artist_of(instance: &str, id: &str, name: &str) -> Value {
    json!({"item_id": id, "provider": instance, "name": name, "media_type": "artist"})
}

/// A track as Music Assistant serializes one from a provider.
fn ma_track(instance: &str, id: &str, title: &str, artist: &str) -> Value {
    let domain = instance.split("--").next().unwrap_or(instance);
    json!({
        "item_id": id, "provider": instance, "name": title, "media_type": "track",
        "duration": 200.5, "track_number": 1, "disc_number": 1,
        "artists": [artist_of(instance, &format!("{id}-artist"), artist)],
        "album": {"item_id": format!("{id}-album"), "provider": instance, "name": "An Album",
                  "media_type": "album"},
        "provider_mappings": [{"item_id": id, "provider_domain": domain,
                               "provider_instance": instance, "available": true}],
    })
}

fn ma_item(instance: &str, kind: &str, id: &str, name: &str) -> Value {
    let domain = instance.split("--").next().unwrap_or(instance);
    json!({
        "item_id": id, "provider": instance, "name": name, "media_type": kind,
        "owner": "Spotify", "is_editable": false,
        "artists": [artist_of(instance, "a1", "Beyoncé")],
        "provider_mappings": [{"item_id": id, "provider_domain": domain,
                               "provider_instance": instance, "available": true}],
    })
}

fn model() -> Model {
    let mut model = Model::default();
    model.players = vec![player("sonos-kitchen", "Kitchen", "sonos--Kq2m8VbR")];
    model.queues = vec![queue("sonos-kitchen")];
    model.providers = vec![
        provider("sonos--Kq2m8VbR", "sonos", "player", "Sonos", false),
        streaming(
            SPOTIFY,
            "spotify",
            "Spotify",
            &["search", "library_playlists", "artist_toptracks"],
        ),
        streaming(TIDAL, "tidal", "Tidal", &["search"]),
        provider(
            "filesystem_local--m",
            "filesystem_local",
            "music",
            "Music",
            false,
        ),
        // A server source that calls itself streaming is still not a service.
        streaming(
            "opensubsonic--Q",
            "opensubsonic",
            "Open Subsonic",
            &["search"],
        ),
    ];
    let mut halo = ma_track(SPOTIFY, "0Wqd0zA5wNpQnQBGEUH0f4", "Halo", "Beyoncé");
    halo["album"]["image"] = json!({"type": "thumb", "path": "/x.jpg", "provider": "spotify",
                                    "proxy_id": PROXY_ID});
    model.search.insert(
        SPOTIFY.to_owned(),
        json!({
            "tracks": [halo, ma_track(SPOTIFY, "s2", "Run the World", "Beyoncé")],
            "albums": [ma_item(SPOTIFY, "album", "39P7", "I Am... Sasha Fierce")],
            "artists": [artist_of(SPOTIFY, "6vWD", "Beyoncé")],
            "playlists": [ma_item(SPOTIFY, "playlist", "37i9", "This Is Beyoncé")],
        }),
    );
    model.search.insert(
        TIDAL.to_owned(),
        json!({"tracks": [ma_track(TIDAL, "1781887", "Halo", "Beyoncé")]}),
    );
    model
}

async fn connected() -> Result<(Server, FakeAssistant, String), Error> {
    connected_with(model()).await
}

async fn connected_with(model: Model) -> Result<(Server, FakeAssistant, String), Error> {
    let server = start().await?;
    let fake = FakeAssistant::start(model).await;
    let cookie = server.setup_admin().await?;
    server.connect(&cookie, &fake).await?;
    Ok((server, fake, cookie))
}

impl Server {
    /// Waits until the catalog lists `count` services.
    async fn services(&self, key: &str, count: usize) -> Result<Value, Error> {
        self.until(key, "/catalog/services", |status, b| {
            status == 200 && b["services"].as_array().is_some_and(|s| s.len() == count)
        })
        .await
    }

    async fn media_token(&self, key: &str) -> Result<String, Error> {
        let token: wire::auth::MediaToken = self
            .http
            .post(self.url("/media-token"))
            .bearer_auth(key)
            .send()
            .await?
            .json()
            .await?;
        Ok(token.token)
    }

    async fn art(&self, token: &str, id: &str) -> Result<(u16, String, Vec<u8>), Error> {
        self.art_at(token, id, 0).await
    }

    async fn art_at(
        &self,
        token: &str,
        id: &str,
        size: u32,
    ) -> Result<(u16, String, Vec<u8>), Error> {
        let response = self
            .http
            .get(self.url(&if size == 0 {
                format!("/art/{id}?mt={token}")
            } else {
                format!("/art/{id}?mt={token}&size={size}")
            }))
            .send()
            .await?;
        let status = response.status().as_u16();
        let kind = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        Ok((status, kind, response.bytes().await?.to_vec()))
    }
}

fn titles(section: &Value) -> Vec<(String, String)> {
    section["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|t| {
                    (
                        t["title"].as_str().unwrap_or_default().to_owned(),
                        t["service_id"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(width, height, image::Rgb([10, 120, 200]));
    let mut out = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap_or_default();
    out.into_inner()
}

#[tokio::test]
async fn only_streaming_services_are_listed_and_hidden_ones_are_left_out() -> TestResult {
    let (server, _fake, cookie) = connected().await?;
    let key = server.device_key("luke", "phone").await?;
    let list = server.services(&key, 2).await?;
    assert_eq!(list["state"], "ok");
    let spotify = &list["services"][0];
    assert_eq!(spotify["id"], service_id(SPOTIFY));
    assert_eq!(spotify["name"], "Spotify");
    assert_eq!(spotify["logo_key"], "spotify");
    assert_eq!(spotify["quality"], "high");
    assert_eq!(spotify["account"], json!({"state": "ok"}));
    assert_eq!(
        spotify["capabilities"],
        json!(["library", "playlists", "search", "top_tracks"])
    );
    assert_eq!(list["services"][1]["logo_key"], "tidal");
    let shown = list.to_string();
    assert!(
        !shown.contains("opensubsonic") && !shown.contains("filesystem"),
        "{shown}"
    );
    assert!(!shown.contains(SPOTIFY), "raw instance ids never appear");

    let (status, _) = server
        .admin(
            &cookie,
            reqwest::Method::PATCH,
            "/music-assistant",
            Some(json!({"hidden_services": [TIDAL]})),
        )
        .await?;
    assert_eq!(status, 200);
    let list = server.services(&key, 1).await?;
    assert_eq!(list["services"][0]["logo_key"], "spotify");
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn search_interleaves_services_and_names_owned_songs() -> TestResult {
    let (server, fake, _cookie) = connected().await?;
    let key = server.device_key("luke", "phone").await?;
    server.services(&key, 2).await?;
    let (status, body) = server
        .call(&key, reqwest::Method::GET, "/catalog/search?q=halo", None)
        .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["complete"], true);
    assert_eq!(
        body["services"],
        json!([{"id": service_id(SPOTIFY), "state": "ok"},
               {"id": service_id(TIDAL), "state": "ok"}])
    );
    let (spotify, tidal) = (service_id(SPOTIFY), service_id(TIDAL));
    assert_eq!(
        titles(&body["tracks"]),
        [
            (String::from("Halo"), spotify.clone()),
            (String::from("Halo"), tidal.clone()),
            (String::from("Run the World"), spotify.clone())
        ],
        "best of each service first"
    );
    let halo = &body["tracks"]["items"][0];
    assert_eq!(halo["song_id"], server.song.to_string(), "the owned copy");
    assert_eq!(halo["song_key"], "halo\u{1f}beyonce");
    assert_eq!(halo["duration_ms"], 200_500);
    assert!(halo["ref"].as_str().is_some_and(|r| r.starts_with("mc1.")));
    assert!(body["tracks"]["items"][2].get("song_id").is_none());
    assert_eq!(
        body["albums"]["items"][0]["album_key"],
        "i am... sasha fierce\u{1f}beyonce"
    );
    assert_eq!(body["artists"]["items"][0]["identity"], "beyonce");
    assert_eq!(body["playlists"]["items"][0]["owner"], "Spotify");
    assert!(body.get("radio").is_none(), "not asked for");

    // Each service is asked on its own, with the library for what the owner saved.
    let searches = fake.received_named("music/search");
    assert_eq!(searches.len(), 2);
    let mut asked: Vec<Value> = searches
        .iter()
        .map(|r| r.args["providers"].clone())
        .collect();
    asked.sort_by_key(ToString::to_string);
    assert_eq!(
        asked,
        [json!([SPOTIFY, "library"]), json!([TIDAL, "library"])]
    );

    // The same search again is answered from memory.
    let (_, again) = server
        .call(&key, reqwest::Method::GET, "/catalog/search?q=HALO", None)
        .await?;
    assert_eq!(again, body);
    assert_eq!(fake.received_named("music/search").len(), 2);

    // Narrowed to one service and one kind.
    let (_, narrow) = server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("/catalog/search?q=halo&types=tracks&services={tidal}&limit=5"),
            None,
        )
        .await?;
    assert_eq!(titles(&narrow["tracks"]), [(String::from("Halo"), tidal)]);
    assert!(narrow.get("albums").is_none());
    let last = fake.received_named("music/search").pop().ok_or("asked")?;
    assert_eq!(last.args["media_types"], json!(["track"]));
    assert_eq!(last.args["limit"], 5);

    for (query, field) in [
        ("/catalog/search?q=halo&types=song", "types"),
        ("/catalog/search?q=halo&services=nope", "services"),
    ] {
        let (status, error) = server.call(&key, reqwest::Method::GET, query, None).await?;
        assert_eq!(status, 400, "{query}");
        assert_eq!(error["error"]["details"]["field"], field);
    }
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_slow_service_is_reported_while_the_others_answer() -> TestResult {
    let mut slow = model();
    slow.delays
        .insert(format!("music/search:{TIDAL}"), Duration::from_secs(8));
    let (server, _fake, _cookie) = connected_with(slow).await?;
    let key = server.device_key("luke", "phone").await?;
    server.services(&key, 2).await?;
    let started = tokio::time::Instant::now();
    let (status, body) = server
        .call(&key, reqwest::Method::GET, "/catalog/search?q=halo", None)
        .await?;
    let waited = started.elapsed();
    assert_eq!(status, 200, "{body}");
    assert!(waited < Duration::from_secs(7), "{waited:?}");
    assert_eq!(body["complete"], false);
    assert_eq!(
        body["services"],
        json!([{"id": service_id(SPOTIFY), "state": "ok"},
               {"id": service_id(TIDAL), "state": "timed_out"}])
    );
    assert!(
        titles(&body["tracks"])
            .iter()
            .all(|(_, service)| *service == service_id(SPOTIFY))
    );
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn the_library_search_keeps_catalog_results_apart() -> TestResult {
    let (server, _fake, _cookie) = connected().await?;
    let key = server.device_key("luke", "phone").await?;
    server.services(&key, 2).await?;
    let (status, body) = server
        .call(
            &key,
            reqwest::Method::GET,
            "/search?q=halo&types=song,catalog&catalog_types=track",
            None,
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let songs = body["songs"]["items"].as_array().ok_or("songs")?;
    assert_eq!(songs.len(), 1);
    assert_eq!(songs[0]["id"], server.song.to_string());
    assert!(
        songs.iter().all(|s| s.get("ref").is_none()),
        "never interleaved"
    );
    let catalog = &body["catalog"];
    assert_eq!(
        catalog["tracks"]["items"][0]["song_id"],
        server.song.to_string()
    );
    assert!(catalog.get("albums").is_none());
    assert!(body.get("albums").is_none());

    let (status, only) = server
        .call(
            &key,
            reqwest::Method::GET,
            "/search?q=halo&types=catalog",
            None,
        )
        .await?;
    assert_eq!(status, 200);
    assert!(only.get("songs").is_none(), "{only}");
    assert!(only["catalog"]["albums"]["items"].is_array());

    let (status, error) = server
        .call(
            &key,
            reqwest::Method::GET,
            "/search?q=halo&types=song,catalog&cursor=eyJvIjoyMH0",
            None,
        )
        .await?;
    assert_eq!(status, 400);
    assert_eq!(error["error"]["details"]["field"], "types");
    server.assistant.shutdown().await;

    // With Music Assistant gone the section says so, and the library still answers.
    let (status, away) = server
        .until(&key, "/search?q=halo&types=song,catalog", |_, b| {
            b["catalog"]["state"] == "unavailable"
        })
        .await
        .map(|b| (200, b))?;
    assert_eq!(status, 200);
    assert_eq!(
        away["catalog"],
        json!({"complete": true, "services": [], "state": "unavailable"})
    );
    assert_eq!(away["songs"]["items"][0]["id"], server.song.to_string());
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "every kind of item, one after another"
)]
async fn items_resolve_through_their_refs_and_playlists_page() -> TestResult {
    let mut items = model();
    let album_uri = format!("{SPOTIFY}://album/39P7");
    items.items.insert(
        album_uri.clone(),
        ma_item(SPOTIFY, "album", "39P7", "I Am... Sasha Fierce"),
    );
    items.listings.insert(
        format!("music/albums/album_tracks:{SPOTIFY}/39P7"),
        vec![
            {
                let mut t = ma_track(SPOTIFY, "t9", "Single Ladies", "Beyoncé");
                t["track_number"] = json!(9);
                t
            },
            {
                let mut t = ma_track(SPOTIFY, "t3", "Halo", "Beyoncé");
                t["track_number"] = json!(3);
                t
            },
        ],
    );
    items.items.insert(
        format!("{SPOTIFY}://playlist/37i9"),
        ma_item(SPOTIFY, "playlist", "37i9", "This Is Beyoncé"),
    );
    items.listings.insert(
        format!("music/playlists/playlist_tracks:{SPOTIFY}/37i9"),
        (0..5)
            .map(|n| ma_track(SPOTIFY, &format!("p{n}"), &format!("Song {n}"), "Beyoncé"))
            .collect(),
    );
    items.items.insert(
        format!("{SPOTIFY}://artist/6vWD"),
        artist_of(SPOTIFY, "6vWD", "Beyoncé"),
    );
    items.listings.insert(
        format!("music/artists/artist_tracks:{SPOTIFY}/6vWD"),
        (0..30)
            .map(|n| ma_track(SPOTIFY, &format!("top{n}"), &format!("Top {n}"), "Beyoncé"))
            .collect(),
    );
    items.listings.insert(
        format!("music/artists/artist_albums:{SPOTIFY}/6vWD"),
        vec![ma_item(SPOTIFY, "album", "39P7", "I Am... Sasha Fierce")],
    );
    let (server, fake, _cookie) = connected_with(items).await?;
    let key = server.device_key("luke", "phone").await?;
    server.services(&key, 2).await?;
    let (_, found) = server
        .call(&key, reqwest::Method::GET, "/catalog/search?q=halo", None)
        .await?;
    let album_ref = found["albums"]["items"][0]["ref"].as_str().ok_or("album")?;
    let (status, album) = server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("/catalog/items/{album_ref}"),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{album}");
    assert_eq!(album["album"]["track_count"], 2);
    let order: Vec<&str> = album["tracks"]
        .as_array()
        .ok_or("tracks")?
        .iter()
        .filter_map(|t| t["title"].as_str())
        .collect();
    assert_eq!(order, ["Halo", "Single Ladies"], "in disc and track order");
    assert_eq!(album["tracks"][0]["song_id"], server.song.to_string());

    let playlist_ref = found["playlists"]["items"][0]["ref"]
        .as_str()
        .ok_or("playlist")?;
    let path = format!("/catalog/items/{playlist_ref}?limit=2");
    let (_, first) = server.call(&key, reqwest::Method::GET, &path, None).await?;
    assert_eq!(first["playlist"]["track_count"], 5);
    assert_eq!(first["playlist"]["editable"], false);
    assert_eq!(titles(&json!({"items": first["tracks"]})).len(), 2);
    let cursor = first["next_cursor"].as_str().ok_or("cursor")?;
    let (_, second) = server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("{path}&cursor={cursor}"),
            None,
        )
        .await?;
    assert_eq!(second["tracks"][0]["title"], "Song 2");
    let cursor = second["next_cursor"].as_str().ok_or("cursor")?;
    let (_, third) = server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("{path}&cursor={cursor}"),
            None,
        )
        .await?;
    assert_eq!(third["tracks"][0]["title"], "Song 4");
    assert!(third.get("next_cursor").is_none());
    assert_eq!(
        fake.received_named("music/playlists/playlist_tracks").len(),
        1,
        "paging reads Music Assistant once"
    );

    let artist_ref = found["artists"]["items"][0]["ref"]
        .as_str()
        .ok_or("artist")?;
    let (_, artist) = server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("/catalog/items/{artist_ref}"),
            None,
        )
        .await?;
    assert_eq!(artist["top_tracks"].as_array().map(Vec::len), Some(20));
    assert_eq!(artist["albums"][0]["name"], "I Am... Sasha Fierce");

    let track_ref = found["tracks"]["items"][0]["ref"].as_str().ok_or("track")?;
    let (status, _) = server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("/catalog/items/{track_ref}"),
            None,
        )
        .await?;
    assert_eq!(status, 404, "the service no longer has it");

    let qobuz = trackside_core::catalog_ref::CatalogRef::new(
        trackside_core::catalog_ref::RefKind::Album,
        "qobuz",
        "1",
        Some("qobuz--gone"),
    )
    .encode()
    .ok_or("ref")?;
    let (status, error) = server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("/catalog/items/{qobuz}"),
            None,
        )
        .await?;
    assert_eq!(status, 409);
    assert_eq!(error["error"]["details"]["reason"], "service_signed_out");
    let (status, error) = server
        .call(&key, reqwest::Method::GET, "/catalog/items/mc1.!!!", None)
        .await?;
    assert_eq!(status, 400);
    assert_eq!(error["error"]["details"]["field"], "ref");
    let (status, _) = server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("{path}&cursor=nope"),
            None,
        )
        .await?;
    assert_eq!(status, 400);

    // A ref from the catalog plays on a speaker through the same resolution.
    let list = server
        .until(&key, "/speakers", |_, b| {
            b["speakers"].as_array().is_some_and(|s| s.len() == 1)
        })
        .await?;
    let kitchen = list["speakers"][0]["id"].as_str().ok_or("kitchen")?;
    let (status, body) = server
        .call(
            &key,
            reqwest::Method::POST,
            &format!("/speakers/{kitchen}/play"),
            Some(json!({"items": [{"ref": album_ref}]})),
        )
        .await?;
    assert_eq!(status, 202, "{body}");
    let played = fake.wait_for("player_queues/play_media", 1).await;
    assert_eq!(played[0].args["media"], json!([album_uri]));
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn saved_libraries_page_newest_first() -> TestResult {
    let mut saved = model();
    saved.listings.insert(
        format!("music/playlists/library_items:{SPOTIFY}"),
        (0..3)
            .map(|n| ma_item(SPOTIFY, "playlist", &format!("pl{n}"), &format!("Mine {n}")))
            .collect(),
    );
    let (server, fake, _cookie) = connected_with(saved).await?;
    let key = server.device_key("luke", "phone").await?;
    server.services(&key, 2).await?;
    let spotify = service_id(SPOTIFY);
    let (status, page) = server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("/catalog/services/{spotify}/library?limit=2"),
            None,
        )
        .await?;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["items"][0]["kind"], "playlist");
    assert_eq!(page["items"][1]["name"], "Mine 1");
    assert!(page.get("synced_at").is_none());
    let asked = fake
        .received_named("music/playlists/library_items")
        .pop()
        .ok_or("asked")?;
    assert_eq!(asked.args["order_by"], "timestamp_added_desc");
    assert_eq!(asked.args["provider"], SPOTIFY);
    let cursor = page["next_cursor"].as_str().ok_or("cursor")?;
    let (_, rest) = server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("/catalog/services/{spotify}/library?limit=2&cursor={cursor}"),
            None,
        )
        .await?;
    assert_eq!(rest["items"].as_array().map(Vec::len), Some(1));
    assert!(rest.get("next_cursor").is_none());

    let (status, error) = server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("/catalog/services/{}/library", service_id(TIDAL)),
            None,
        )
        .await?;
    assert_eq!(status, 422);
    assert_eq!(error["error"]["details"]["capability"], "library");
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn members_without_can_play_services_get_no_catalog() -> TestResult {
    let (server, _fake, cookie) = connected().await?;
    let response = server
        .http
        .post(server.url("/admin/users"))
        .headers(server.session(&cookie))
        .json(
            &json!({"username": "mia", "password": PASSWORD, "library_ids": [],
                      "permissions": {"can_play_services": false}}),
        )
        .send()
        .await?;
    assert_eq!(response.status(), 201, "{:?}", response.text().await);
    let key = server.device_key("mia", "mia-phone").await?;
    let (_, me) = server.call(&key, reqwest::Method::GET, "/me", None).await?;
    assert!(me["features"].get("catalog").is_none(), "{me}");
    for path in [
        String::from("/catalog/services"),
        String::from("/catalog/search?q=halo"),
        format!("/catalog/services/{}/library", service_id(SPOTIFY)),
    ] {
        let (status, _) = server.call(&key, reqwest::Method::GET, &path, None).await?;
        assert_eq!(status, 404, "{path}");
    }
    let (status, body) = server
        .call(
            &key,
            reqwest::Method::GET,
            "/search?q=halo&types=song,catalog",
            None,
        )
        .await?;
    assert_eq!(status, 200);
    assert!(body.get("catalog").is_none(), "left out, never an error");
    let token = server.media_token(&key).await?;
    let id = trackside_core::CatalogArtId::derive("x").to_string();
    assert_eq!(server.art(&token, &id).await?.0, 404);
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn members_see_only_their_own_songs_named() -> TestResult {
    let (server, _fake, cookie) = connected().await?;
    let response = server
        .http
        .post(server.url("/admin/users"))
        .headers(server.session(&cookie))
        .json(&json!({"username": "mia", "password": PASSWORD, "library_ids": []}))
        .send()
        .await?;
    assert_eq!(response.status(), 201);
    let key = server.device_key("mia", "mia-phone").await?;
    server.services(&key, 2).await?;
    let (_, body) = server
        .call(&key, reqwest::Method::GET, "/catalog/search?q=halo", None)
        .await?;
    assert_eq!(body["tracks"]["items"][0]["title"], "Halo");
    assert!(
        body["tracks"]["items"][0].get("song_id").is_none(),
        "mia cannot see the library holding it"
    );
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn searches_are_rate_limited_per_user() -> TestResult {
    let (server, _fake, _cookie) = connected().await?;
    let key = server.device_key("luke", "phone").await?;
    server.services(&key, 2).await?;
    let mut statuses = Vec::new();
    for _ in 0..32 {
        let (status, _) = server
            .call(&key, reqwest::Method::GET, "/catalog/search?q=halo", None)
            .await?;
        statuses.push(status);
    }
    assert!(statuses[..30].iter().all(|s| *s == 200), "{statuses:?}");
    assert_eq!(statuses[31], 429);
    let response = server
        .http
        .get(server.url("/catalog/search?q=halo"))
        .bearer_auth(&key)
        .send()
        .await?;
    assert!(response.headers().contains_key("retry-after"));
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn catalog_art_is_fetched_from_music_assistant_once_and_kept_as_jpeg() -> TestResult {
    let mut art = model();
    let path = format!("/imageproxy/{PROXY_ID}?size=0");
    art.images
        .insert(path.clone(), (String::from("image/png"), png(400, 400)));
    let mut private = ma_track(SPOTIFY, "s3", "Private", "Beyoncé");
    private["album"]["image"] = json!({"type": "thumb", "provider": "spotify",
        "path": "https://169.254.169.254/latest/meta-data", "remotely_accessible": true});
    let mut svg = ma_track(SPOTIFY, "s4", "Vector", "Beyoncé");
    let svg_id = "f".repeat(64);
    svg["album"]["image"] = json!({"type": "thumb", "path": "/v.svg", "provider": "spotify",
                                   "proxy_id": svg_id});
    art.images.insert(
        format!("/imageproxy/{svg_id}?size=0"),
        (
            String::from("image/svg+xml"),
            b"<svg xmlns='http://www.w3.org/2000/svg' width='300' height='300'/>".to_vec(),
        ),
    );
    if let Some(tracks) = art
        .search
        .get_mut(SPOTIFY)
        .and_then(|s| s["tracks"].as_array_mut())
    {
        tracks.push(private);
        tracks.push(svg);
    }
    let (server, fake, _cookie) = connected_with(art).await?;
    let key = server.device_key("luke", "phone").await?;
    server.services(&key, 2).await?;
    let (_, body) = server
        .call(
            &key,
            reqwest::Method::GET,
            "/catalog/search?q=halo&types=track",
            None,
        )
        .await?;
    let art_of = |title: &str| -> Option<String> {
        body["tracks"]["items"]
            .as_array()?
            .iter()
            .find(|t| t["title"] == title && t["service_id"] == service_id(SPOTIFY))
            .and_then(|t| t["artwork_id"].as_str())
            .map(str::to_owned)
    };
    let halo = art_of("Halo").ok_or("halo art")?;
    assert!(halo.starts_with('m'));
    let token = server.media_token(&key).await?;

    let (status, kind, bytes) = server.art(&token, &halo).await?;
    assert_eq!((status, kind.as_str()), (200, "image/jpeg"), "re-encoded");
    assert_eq!(image::guess_format(&bytes)?, image::ImageFormat::Jpeg);
    let (status, kind, small) = server.art_at(&token, &halo, 64).await?;
    assert_eq!((status, kind.as_str()), (200, "image/jpeg"));
    assert!(small.len() < bytes.len());
    assert_eq!(fake.image_requests(), [path], "fetched once, then cached");

    let private = art_of("Private").ok_or("private art")?;
    assert_eq!(server.art(&token, &private).await?.0, 404, "never fetched");
    let vector = art_of("Vector").ok_or("svg art")?;
    assert_eq!(
        server.art(&token, &vector).await?.0,
        404,
        "svg is not an image here"
    );
    assert_eq!(
        fake.image_requests().len(),
        2,
        "the private address was never asked"
    );

    let unknown = trackside_core::CatalogArtId::derive("https://example.com/nope.jpg");
    assert_eq!(
        server
            .art(&token, trackside_core::PublicId::as_str(&unknown))
            .await?
            .0,
        404
    );
    assert_eq!(
        fake.image_requests().len(),
        2,
        "only addresses Music Assistant gave"
    );
    server.assistant.shutdown().await;
    Ok(())
}

const APPLE: &str = "apple_music--Hq7cT2";
const PROXY_ID_2: &str = "5e1f0a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7";

/// An Apple Music station as Music Assistant lists it: a dynamic playlist on 2.10, a radio on
/// 2.11.
fn station(id: &str, name: &str, kind: &str) -> Value {
    json!({
        "item_id": id, "provider": APPLE, "name": name, "media_type": kind, "is_dynamic": true,
        "uri": format!("{APPLE}://{kind}/{id}"), "is_editable": false,
        "provider_mappings": [{"item_id": id, "provider_domain": "apple_music",
                               "provider_instance": APPLE, "available": true}],
        "metadata": {"images": [{"type": "thumb", "path": format!("{kind}/{id}"), "provider": APPLE,
                                 "remotely_accessible": false, "proxy_id": PROXY_ID_2}]},
    })
}

fn folder(provider: &str, id: &str, name: &str) -> Value {
    json!({"item_id": id, "provider": provider, "name": name, "media_type": "folder",
           "path": format!("{provider}://{id}"), "items": [], "type": "default",
           "enabled_by_default": true})
}

/// The model with Apple Music and Spotify recommending, stations shaped as `station_kind`.
fn recommending(station_kind: &str) -> Model {
    let mut model = model();
    if station_kind == "playlist" {
        model.version = String::from("2.10.14");
    } else {
        model.version = String::from("2.11.0b11");
        model.schema = 128;
    }
    model.providers.push(streaming(
        APPLE,
        "apple_music",
        "Apple Music",
        &["search", "library_tracks", "recommendations"],
    ));
    for p in &mut model.providers {
        if p["instance_id"] == SPOTIFY {
            p["supported_features"] = json!(["search", "library_playlists", "recommendations"]);
        }
    }
    model.recommendations = vec![
        folder("recommendations", "recently_played", "Recently played"),
        folder(APPLE, "stations-for-you", "Stations for You"),
        folder(SPOTIFY, "made-for-you", "Made For Luke"),
        folder("lastfm_recommendations", "similar", "Similar artists"),
        folder(TIDAL, "tidal-row", "Tidal does not recommend here"),
    ];
    model.listings.insert(
        String::from("music/recommendations/items:recommendations/recently_played"),
        vec![ma_track(SPOTIFY, "s2", "Run the World", "Beyoncé")],
    );
    model.listings.insert(
        format!("music/recommendations/items:{APPLE}/stations-for-you"),
        vec![
            station("ra.q-GAI6IDE2MjgwNjYwNTE", "Beyoncé Station", station_kind),
            json!({"item_id": "genres", "provider": APPLE, "name": "Genres", "media_type": "folder"}),
            json!({"item_id": "p1", "provider": APPLE, "name": "A Podcast", "media_type": "podcast"}),
        ],
    );
    model.listings.insert(
        format!("music/recommendations/items:{SPOTIFY}/made-for-you"),
        vec![
            ma_track(SPOTIFY, "0Wqd0zA5wNpQnQBGEUH0f4", "Halo", "Beyoncé"),
            ma_item(SPOTIFY, "playlist", "37i9", "Daily Mix 1"),
        ],
    );
    model
}

impl Server {
    async fn recommendations(&self, key: &str, query: &str) -> Result<(u16, Value), Error> {
        self.call(
            key,
            reqwest::Method::GET,
            &format!("/catalog/recommendations{query}"),
            None,
        )
        .await
    }
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one walk through the route")]
async fn recommendations_shelve_the_services_rows_and_name_owned_songs() -> TestResult {
    let (server, fake, _cookie) = connected_with(recommending("radio")).await?;
    let key = server.device_key("luke", "phone").await?;
    let list = server.services(&key, 3).await?;
    let apple = list["services"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["logo_key"] == "apple_music"))
        .ok_or("apple music listed")?;
    assert_eq!(
        apple["capabilities"],
        json!(["library", "recommendations", "search"])
    );
    let (_, me) = server.call(&key, reqwest::Method::GET, "/me", None).await?;
    assert_eq!(me["features"]["catalog"]["recommendations"], true);

    let (status, body) = server.recommendations(&key, "").await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["complete"], true);
    let (spotify, apple_id) = (service_id(SPOTIFY), service_id(APPLE));
    assert_eq!(
        body["services"],
        json!([{"id": spotify, "state": "ok"}, {"id": apple_id, "state": "ok"}])
    );
    let shelves = body["shelves"].as_array().ok_or("shelves")?;
    let titles: Vec<&str> = shelves.iter().filter_map(|s| s["title"].as_str()).collect();
    assert_eq!(
        titles,
        ["Stations for You", "Made For Luke"],
        "in Music Assistant's order, without the library's or other providers' rows"
    );
    let stations = &shelves[0];
    assert_eq!(stations["service_id"], apple_id);
    assert!(
        stations["id"]
            .as_str()
            .is_some_and(|id| id.starts_with('w'))
    );
    assert!(stations.get("type").is_none() && stations.get("subtitle").is_none());
    let items = stations["items"].as_array().ok_or("items")?;
    assert_eq!(items.len(), 1, "folders and podcasts are left out");
    assert_eq!(items[0]["kind"], "radio");
    assert_eq!(items[0]["name"], "Beyoncé Station");
    assert!(
        items[0]["artwork_id"]
            .as_str()
            .is_some_and(|a| a.starts_with('m'))
    );
    let station_ref = items[0]["ref"].as_str().ok_or("ref")?.to_owned();
    let mixes = shelves[1]["items"].as_array().ok_or("items")?;
    assert_eq!(mixes[0]["kind"], "track");
    assert_eq!(
        mixes[0]["song_id"],
        server.song.to_string(),
        "the owned copy"
    );
    assert_eq!(mixes[1]["kind"], "playlist");
    let shown = body.to_string();
    assert!(
        !shown.contains(SPOTIFY) && !shown.contains(APPLE),
        "{shown}"
    );

    let asked = fake.received_named("music/recommendations/items");
    let mut rows: Vec<String> = asked
        .iter()
        .map(|r| {
            format!(
                "{}/{}",
                r.args["provider"].as_str().unwrap_or_default(),
                r.args["item_id"].as_str().unwrap_or_default()
            )
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        [
            format!("{APPLE}/stations-for-you"),
            format!("{SPOTIFY}/made-for-you")
        ],
        "only the services' rows are read"
    );
    assert!(asked.iter().all(|r| r.args.get("hide_played").is_none()));

    // Kept: asking again reads nothing from Music Assistant.
    let (_, again) = server.recommendations(&key, "").await?;
    assert_eq!(again, body);
    assert_eq!(fake.received_named("music/recommendations").len(), 1);

    // Narrowed to one service, with fewer items per shelf.
    let (_, narrow) = server
        .recommendations(&key, &format!("?services={spotify}&limit=1"))
        .await?;
    assert_eq!(narrow["services"], json!([{"id": spotify, "state": "ok"}]));
    assert_eq!(narrow["shelves"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        narrow["shelves"][0]["items"].as_array().map(Vec::len),
        Some(1)
    );

    // A change to the services forgets what was kept.
    fake.emit("providers_updated", None, json!([]));
    server
        .until(&key, "/catalog/recommendations", |_, _| {
            fake.received_named("music/recommendations").len() >= 3
        })
        .await?;

    for (query, field) in [
        ("?limit=0", "limit"),
        ("?limit=51", "limit"),
        ("?services=nope", "services"),
    ] {
        let (status, error) = server.recommendations(&key, query).await?;
        assert_eq!(status, 400, "{query}");
        assert_eq!(error["error"]["details"]["field"], field);
    }

    // A 2.11 station plays on a speaker as a radio.
    let list = server
        .until(&key, "/speakers", |_, b| {
            b["speakers"].as_array().is_some_and(|s| s.len() == 1)
        })
        .await?;
    let kitchen = list["speakers"][0]["id"].as_str().ok_or("kitchen")?;
    let (status, played) = server
        .call(
            &key,
            reqwest::Method::POST,
            &format!("/speakers/{kitchen}/play"),
            Some(json!({"items": [{"ref": station_ref}]})),
        )
        .await?;
    assert_eq!(status, 202, "{played}");
    let played = fake.wait_for("player_queues/play_media", 1).await;
    assert_eq!(
        played[0].args["media"],
        json!([format!("{APPLE}://radio/ra.q-GAI6IDE2MjgwNjYwNTE")])
    );
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn stations_of_music_assistant_2_10_are_playlists_that_play() -> TestResult {
    let (server, fake, _cookie) = connected_with(recommending("playlist")).await?;
    let key = server.device_key("luke", "phone").await?;
    server.services(&key, 3).await?;
    let (status, body) = server
        .recommendations(&key, &format!("?services={}", service_id(APPLE)))
        .await?;
    assert_eq!(status, 200, "{body}");
    let station = &body["shelves"][0]["items"][0];
    assert_eq!(station["kind"], "playlist", "{body}");
    assert_eq!(station["editable"], false);
    let reference = station["ref"].as_str().ok_or("ref")?;
    let list = server
        .until(&key, "/speakers", |_, b| {
            b["speakers"].as_array().is_some_and(|s| s.len() == 1)
        })
        .await?;
    let kitchen = list["speakers"][0]["id"].as_str().ok_or("kitchen")?;
    let (status, played) = server
        .call(
            &key,
            reqwest::Method::POST,
            &format!("/speakers/{kitchen}/play"),
            Some(json!({"items": [{"ref": reference}]})),
        )
        .await?;
    assert_eq!(status, 202, "{played}");
    let played = fake.wait_for("player_queues/play_media", 1).await;
    assert_eq!(
        played[0].args["media"],
        json!([format!("{APPLE}://playlist/ra.q-GAI6IDE2MjgwNjYwNTE")]),
        "2.10 plays a station only as a playlist"
    );
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_slow_row_is_left_out_while_the_others_answer() -> TestResult {
    let mut slow = recommending("radio");
    slow.delays.insert(
        format!("music/recommendations/items:{APPLE}/stations-for-you"),
        Duration::from_secs(8),
    );
    let (server, fake, _cookie) = connected_with(slow).await?;
    let key = server.device_key("luke", "phone").await?;
    server.services(&key, 3).await?;
    let started = tokio::time::Instant::now();
    let (status, body) = server.recommendations(&key, "").await?;
    let waited = started.elapsed();
    assert_eq!(status, 200, "{body}");
    assert!(waited < Duration::from_secs(7), "{waited:?}");
    assert_eq!(body["complete"], false);
    assert_eq!(
        body["services"],
        json!([{"id": service_id(SPOTIFY), "state": "ok"},
               {"id": service_id(APPLE), "state": "timed_out"}])
    );
    assert_eq!(body["shelves"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["shelves"][0]["title"], "Made For Luke");

    // A partial answer is kept briefly: at once it is served from memory.
    let (_, again) = server.recommendations(&key, "").await?;
    assert_eq!(again, body);
    assert_eq!(fake.received_named("music/recommendations").len(), 1);
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn recommendations_need_the_catalog_count_as_reads_and_need_music_assistant() -> TestResult {
    let (server, fake, cookie) = connected_with(recommending("radio")).await?;
    let response = server
        .http
        .post(server.url("/admin/users"))
        .headers(server.session(&cookie))
        .json(
            &json!({"username": "mia", "password": PASSWORD, "library_ids": [],
                      "permissions": {"can_play_services": false}}),
        )
        .send()
        .await?;
    assert_eq!(response.status(), 201);
    let mia = server.device_key("mia", "mia-phone").await?;
    assert_eq!(server.recommendations(&mia, "").await?.0, 404);

    let key = server.device_key("luke", "phone").await?;
    server.services(&key, 3).await?;
    let mut statuses = Vec::new();
    for _ in 0..122 {
        statuses.push(server.recommendations(&key, "").await?.0);
    }
    assert!(statuses[..120].iter().all(|s| *s == 200), "{statuses:?}");
    assert_eq!(
        statuses[121], 429,
        "item reads and recommendations share a limit"
    );

    let response = server
        .http
        .post(server.url("/admin/users"))
        .headers(server.session(&cookie))
        .json(&json!({"username": "ana", "password": PASSWORD, "library_ids": []}))
        .send()
        .await?;
    assert_eq!(response.status(), 201);
    let other = server.device_key("ana", "ana-phone").await?;
    assert_eq!(server.recommendations(&other, "").await?.0, 200);
    fake.with(|m| m.reachable = false);
    fake.disconnect_all();
    let error = server
        .until(&other, "/catalog/recommendations", |status, _| {
            status == 503
        })
        .await?;
    assert_eq!(error["error"]["code"], "catalog_unavailable");
    server.assistant.shutdown().await;
    Ok(())
}
