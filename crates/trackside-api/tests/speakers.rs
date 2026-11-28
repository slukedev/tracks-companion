//! Speakers and the Music Assistant admin over real HTTP, against the fake Music Assistant of
//! `trackside-mass` (feature `testing`): settings that never show the token, reconnecting
//! when they change, Trackside's registration as a source (first time, repeated, repaired,
//! refused for a non-admin token), what the registration key may do, the speakers list,
//! commands with their expected state and outcome events, Music Assistant going away,
//! permissions and rate limits.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use trackside_api::{ApiState, LocalState, NoOps, ServerInfo, wire};
use trackside_assistant::{Assistant, Deps, Tuning};
use trackside_auth::{Auth, SecretKey};
use trackside_core::testing::{FakeAcquireControl, FakeScanControl, TestDb, scanned};
use trackside_core::{BoxFuture, Config, ConfigReport, PublicId, SongId};
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
    test: TestDb,
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
        test,
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

fn model() -> Model {
    let mut model = Model::default();
    model.players = vec![
        player("sonos-kitchen", "Kitchen", "sonos--Kq2m8VbR"),
        player("sonos-living", "Living Room", "sonos--Kq2m8VbR"),
    ];
    model.queues = vec![queue("sonos-kitchen"), queue("sonos-living")];
    model.providers = vec![
        provider("sonos--Kq2m8VbR", "sonos", "player", "Sonos", false),
        provider("spotify--Xb4LkP2q", "spotify", "music", "Spotify", true),
    ];
    model
}

/// A model with nothing registered and auto-registration disabled.
async fn connected() -> Result<(Server, FakeAssistant, String), Error> {
    let server = start().await?;
    let fake = FakeAssistant::start(model()).await;
    let cookie = server.setup_admin().await?;
    server.connect(&cookie, &fake).await?;
    Ok((server, fake, cookie))
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one round trip, step by step")]
async fn settings_round_trip_without_the_token_ever_coming_back() -> TestResult {
    let server = start().await?;
    let fake = FakeAssistant::start(model()).await;
    let cookie = server.setup_admin().await?;
    let (status, unset) = server
        .admin(&cookie, reqwest::Method::GET, "/music-assistant", None)
        .await?;
    assert_eq!(status, 200);
    assert_eq!(
        unset,
        json!({"configured": false, "connection": {"state": "not_set_up"}, "enabled": false, "token_set": false})
    );

    let status = server.connect(&cookie, &fake).await?;
    let shown = status.to_string();
    assert!(!shown.contains(&fake.token()), "{shown}");
    assert_eq!(status["token_set"], true);
    assert_eq!(
        status["source"],
        json!({"token": "dashboard", "url": "dashboard"})
    );
    assert_eq!(status["server"]["version"], "2.10.13");
    assert_eq!(status["user"]["role"], "admin");
    let stored: String = server.test.db.read_blocking(|c| {
        c.query_row("SELECT group_concat(value) FROM settings", [], |row| {
            row.get(0)
        })
        .map_err(Into::into)
    })?;
    assert!(
        !stored.contains(&fake.token()),
        "the token is sealed at rest"
    );

    let (status, body) = server
        .admin(
            &cookie,
            reqwest::Method::PATCH,
            "/music-assistant",
            Some(json!({"url": "ftp://nope"})),
        )
        .await?;
    assert_eq!(
        (status, &body["error"]["details"]["field"]),
        (400, &json!("url"))
    );
    let (status, _) = server
        .admin(
            &cookie,
            reqwest::Method::PATCH,
            "/music-assistant",
            Some(json!({"surprise": 1})),
        )
        .await?;
    assert_eq!(status, 400, "unknown fields are refused");

    let (status, test) = server
        .admin(
            &cookie,
            reqwest::Method::POST,
            "/music-assistant/test",
            Some(json!({"token": "wrong"})),
        )
        .await?;
    assert_eq!(status, 200);
    assert_eq!(test["ok"], false);
    assert!(
        test["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("refused the token"),
        "{test}"
    );
    assert!(
        !test.to_string().contains("wrong\""),
        "never echoes the token"
    );
    let (_, good) = server
        .admin(
            &cookie,
            reqwest::Method::POST,
            "/music-assistant/test",
            None,
        )
        .await?;
    assert_eq!(good["ok"], true, "{good}");
    assert_eq!(good["players"], 2);
    for _ in 0..4 {
        server
            .admin(
                &cookie,
                reqwest::Method::POST,
                "/music-assistant/test",
                None,
            )
            .await?;
    }
    let (status, _) = server
        .admin(
            &cookie,
            reqwest::Method::POST,
            "/music-assistant/test",
            None,
        )
        .await?;
    assert_eq!(status, 429, "six tests a minute");

    let (status, _) = server
        .admin(&cookie, reqwest::Method::DELETE, "/music-assistant", None)
        .await?;
    assert_eq!(status, 204);
    let (_, gone) = server
        .admin(&cookie, reqwest::Method::GET, "/music-assistant", None)
        .await?;
    assert_eq!(gone["configured"], false);
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_new_address_reconnects_without_a_restart() -> TestResult {
    let (server, first, cookie) = connected().await?;
    let key = server.device_key("luke", "phone").await?;
    let list = server
        .until(&key, "/speakers", |_, b| {
            b["speakers"].as_array().is_some_and(|s| s.len() == 2)
        })
        .await?;
    assert_eq!(list["state"], "ok");

    let mut other = model();
    other.players = vec![player("cast-bedroom", "Bedroom", "chromecast--x")];
    other.queues = vec![queue("cast-bedroom")];
    other.server_id = String::from("another-server");
    let second = FakeAssistant::start(other).await;
    server.connect(&cookie, &second).await?;
    let list = server
        .until(&key, "/speakers", |_, b| {
            b["speakers"]
                .as_array()
                .is_some_and(|s| s.len() == 1 && s[0]["name"] == "Bedroom")
        })
        .await?;
    assert_eq!(list["speakers"][0]["protocol"], "cast");
    drop(first);
    server.assistant.shutdown().await;
    Ok(())
}

async fn registered(server: &Server, cookie: &str) -> Result<Value, Error> {
    server
        .until_admin(cookie, "/music-assistant/registration", |b| {
            b["state"] == "registered"
        })
        .await
}

#[tokio::test]
async fn trackside_registers_itself_with_a_key_that_only_reads_and_streams() -> TestResult {
    let (server, fake, cookie) = connected().await?;
    let view = registered(&server, &cookie).await?;
    assert_eq!(view["key"]["scopes"], json!(["library", "stream"]));
    assert_eq!(view["service_user"]["username"], "music-assistant");
    let submitted = fake.wait_for("config/flows/submit", 1).await;
    let values = &submitted[0].args["values"];
    let secret = values["api_key"].as_str().ok_or("no key")?.to_owned();
    assert_eq!(values["baseURL"], "http://127.0.0.1");
    assert!(values["port"].as_u64().is_some());
    assert_eq!(values["enable_legacy_auth"], false);
    assert!(
        !view.to_string().contains(&secret),
        "the key's secret is never shown"
    );

    // The key reads the library over Subsonic...
    let ping = server
        .http
        .get(format!(
            "{}/rest/getArtists.view?apiKey={secret}&f=json&c=ma&v=1.16.1",
            server.host
        ))
        .send()
        .await?
        .text()
        .await?;
    assert!(ping.contains("\"status\":\"ok\""), "{ping}");
    // ...but writes nothing there, and has no native API.
    let star = server
        .http
        .get(format!(
            "{}/rest/star.view?apiKey={secret}&f=json&c=ma&v=1.16.1&id={}",
            server.host, server.song
        ))
        .send()
        .await?
        .text()
        .await?;
    assert!(star.contains("\"code\":50"), "{star}");
    let (status, _) = server
        .call(&secret, reqwest::Method::GET, "/me", None)
        .await?;
    assert_eq!(status, 401);
    // The service user cannot sign in, and is listed as one.
    let response = server
        .http
        .post(server.url("/auth/session"))
        .header("origin", &server.host)
        .json(&json!({"username": "music-assistant", "password": ""}))
        .send()
        .await?;
    assert_eq!(response.status(), 401);
    let (_, users) = server
        .admin(&cookie, reqwest::Method::GET, "/users", None)
        .await?;
    let service = users["users"]
        .as_array()
        .and_then(|u| u.iter().find(|u| u["username"] == "music-assistant"))
        .cloned()
        .ok_or("listed")?;
    assert_eq!(service["service"], "music_assistant");
    assert_eq!(service["role"], "member");
    assert!(
        service["permissions"]
            .as_object()
            .is_some_and(|p| p.values().all(|v| v == false)),
        "a service account has no permissions: {service}"
    );
    let (_, keys) = server
        .admin(&cookie, reqwest::Method::GET, "/api-keys", None)
        .await?;
    let managed = keys["keys"]
        .as_array()
        .and_then(|k| k.iter().find(|k| k["managed_by"] == "music_assistant"))
        .cloned()
        .ok_or("managed key listed")?;
    assert_eq!(managed["kind"], "api_key");
    assert_eq!(managed["name"], "Music Assistant");
    assert_eq!(managed["scopes"], json!(["library", "stream"]));
    assert_eq!(managed["user"]["id"], service["id"]);
    let (status, _) = server
        .admin(
            &cookie,
            reqwest::Method::POST,
            &format!(
                "/users/{}/password-reset",
                service["id"].as_str().unwrap_or_default()
            ),
            None,
        )
        .await?;
    assert_eq!(status, 409, "a service account has no password");
    let user_path = format!("/users/{}", service["id"].as_str().unwrap_or_default());
    for change in [
        json!({"role": "admin"}),
        json!({"permissions": {"can_download": true}}),
    ] {
        let (status, body) = server
            .admin(
                &cookie,
                reqwest::Method::PATCH,
                &user_path,
                Some(change.clone()),
            )
            .await?;
        assert_eq!(status, 409, "{change}: {body}");
        assert_eq!(
            body["error"]["details"]["reason"], "service_account",
            "{change}"
        );
    }
    let (_, unchanged) = server
        .admin(&cookie, reqwest::Method::GET, &user_path, None)
        .await?;
    assert_eq!(unchanged["role"], "member");
    assert_eq!(unchanged["permissions"]["can_download"], false);

    // Unregistering removes the source and revokes the key.
    let (status, _) = server
        .admin(
            &cookie,
            reqwest::Method::DELETE,
            "/music-assistant/registration",
            None,
        )
        .await?;
    assert_eq!(status, 204);
    fake.wait_for("config/providers/remove", 1).await;
    let after = server
        .http
        .get(format!(
            "{}/rest/ping.view?apiKey={secret}&f=json&c=ma&v=1.16.1",
            server.host
        ))
        .send()
        .await?
        .text()
        .await?;
    assert!(after.contains("\"status\":\"failed\""), "{after}");
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn registration_is_kept_across_reconnects_and_repaired_when_music_assistant_loses_it()
-> TestResult {
    let (server, fake, cookie) = connected().await?;
    let first = registered(&server, &cookie).await?;
    fake.disconnect_all();
    server
        .until_admin(&cookie, "/music-assistant", |b| {
            b["connection"]["state"] == "connected"
        })
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fake.received_named("config/flows/submit").len(),
        1,
        "a reconnect does not register again"
    );

    let old_secret = fake.received_named("config/flows/submit")[0].args["values"]["api_key"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    fake.with(|m| {
        m.configs.clear();
        m.providers.retain(|p| p["domain"] != "opensubsonic");
    });
    fake.disconnect_all();
    fake.wait_for("config/flows/submit", 2).await;
    let repaired = server
        .until_admin(&cookie, "/music-assistant/registration", |b| {
            b["state"] == "registered" && b["provider_instance_id"] != first["provider_instance_id"]
        })
        .await?;
    assert_ne!(repaired["key"]["id"], first["key"]["id"], "a new key");
    let old = server
        .http
        .get(format!(
            "{}/rest/ping.view?apiKey={old_secret}&f=json&c=ma&v=1.16.1",
            server.host
        ))
        .send()
        .await?
        .text()
        .await?;
    assert!(
        old.contains("\"status\":\"failed\""),
        "the old key is revoked: {old}"
    );
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_token_that_is_not_an_admins_cannot_register() -> TestResult {
    let server = start().await?;
    let mut m = model();
    m.role = String::from("user");
    let fake = FakeAssistant::start(m).await;
    let cookie = server.setup_admin().await?;
    server.connect(&cookie, &fake).await?;
    let view = server
        .until_admin(&cookie, "/music-assistant/registration", |b| {
            b["state"] == "needs_admin"
        })
        .await?;
    assert!(
        view["last_error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not an admin")
    );
    assert_eq!(fake.received_named("config/flows/submit"), Vec::new());
    server.assistant.shutdown().await;
    Ok(())
}

fn speaker_id(list: &Value, name: &str) -> String {
    list["speakers"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["name"] == name))
        .and_then(|s| s["id"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// An SSE stream, read event by event.
struct Events {
    response: reqwest::Response,
    buffer: String,
}

impl Events {
    async fn open(server: &Server, key: &str) -> Result<Self, Error> {
        let response = server
            .http
            .get(server.url("/events"))
            .bearer_auth(key)
            .send()
            .await?;
        let mut events = Self {
            response,
            buffer: String::new(),
        };
        events.next("hello").await?;
        Ok(events)
    }

    /// The data of the next event named `name`; earlier events of other names are skipped.
    async fn next(&mut self, name: &str) -> Result<Value, Error> {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            while let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                if block.lines().any(|l| l == format!("event: {name}")) {
                    let data = block
                        .lines()
                        .find_map(|l| l.strip_prefix("data: "))
                        .unwrap_or("{}");
                    return Ok(serde_json::from_str(data)?);
                }
            }
            let chunk = tokio::time::timeout_at(deadline, self.response.chunk())
                .await
                .map_err(|_| format!("no {name} event in time"))??
                .ok_or("stream ended")?;
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// The outcome of one command, skipping others'.
    async fn outcome_of(&mut self, command: &str) -> Result<Value, Error> {
        loop {
            let event = self.next("speaker_command").await?;
            if event["command_id"] == command {
                return Ok(event);
            }
        }
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one session of commands, step by step"
)]
async fn commands_answer_with_the_expected_state_and_their_outcome_follows() -> TestResult {
    let (server, fake, cookie) = connected().await?;
    let key = server.device_key("luke", "phone").await?;
    let list = server
        .until(&key, "/speakers", |_, b| {
            b["speakers"].as_array().is_some_and(|s| s.len() == 2)
        })
        .await?;
    let kitchen = speaker_id(&list, "Kitchen");
    assert!(kitchen.starts_with('o'));
    let speaker = &list["speakers"][0];
    assert!(
        speaker["capabilities"]
            .as_array()
            .is_some_and(|c| c.contains(&json!("volume"))),
        "{speaker}"
    );
    let (status, me) = server.call(&key, reqwest::Method::GET, "/me", None).await?;
    assert_eq!(status, 200);
    assert_eq!(me["features"]["speakers"], json!({"control": true}));

    let mut events = Events::open(&server, &key).await?;

    let (status, accepted) = server
        .call(
            &key,
            reqwest::Method::PATCH,
            &format!("/speakers/{kitchen}"),
            Some(json!({"volume": 55})),
        )
        .await?;
    assert_eq!(status, 202, "{accepted}");
    assert_eq!(accepted["action"], "update");
    assert_eq!(accepted["speaker"]["volume"], 55, "the expected state");
    let command = accepted["command_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let outcome = events.next("speaker_command").await?;
    assert_eq!(
        outcome,
        json!({"command_id": command, "outcome": "done", "speaker_id": kitchen})
    );
    let sent = fake.wait_for("players/cmd/volume_set", 1).await;
    assert_eq!(
        sent[0].args,
        json!({"player_id": "sonos-kitchen", "volume_level": 55})
    );
    let updated = events.next("speaker_updated").await?;
    assert_eq!(
        updated["volume"], 55,
        "Music Assistant's own update is announced"
    );

    // Refused before anything is sent.
    let (status, body) = server
        .call(
            &key,
            reqwest::Method::POST,
            &format!("/speakers/{kitchen}/playback"),
            Some(json!({"action": "seek", "position_ms": 1000})),
        )
        .await?;
    assert_eq!(
        (status, &body["error"]["details"]),
        (422, &json!({"capability": "seek"})),
        "nothing is loaded, so no seek? {body}"
    );
    let (status, _) = server
        .call(
            &key,
            reqwest::Method::PATCH,
            &format!("/speakers/{kitchen}"),
            Some(json!({"volume": 101})),
        )
        .await?;
    assert_eq!(status, 400);
    let (status, _) = server
        .call(
            &key,
            reqwest::Method::GET,
            "/speakers/onotaspeaker000000000",
            None,
        )
        .await?;
    assert_eq!(status, 404);

    // A Trackside song plays from Trackside's own source.
    registered(&server, &cookie).await?;
    let (status, played) = server
        .call(&key, reqwest::Method::POST, &format!("/speakers/{kitchen}/play"),
              Some(json!({"items": [{"song_id": server.song.as_str()}], "context": {"kind": "album", "name": "I Am"}})))
        .await?;
    assert_eq!(status, 202, "{played}");
    assert_eq!(
        played["speaker"]["now_playing"]["song_id"],
        server.song.as_str()
    );
    assert_eq!(played["speaker"]["playback"]["state"], "playing");
    assert_eq!(played["speaker"]["context"]["name"], "I Am");
    let media = fake.wait_for("player_queues/play_media", 1).await;
    let uri = media[0].args["media"][0].as_str().unwrap_or_default();
    assert!(
        uri.starts_with("opensubsonic--Fake")
            && uri.ends_with(&format!("://track/{}", server.song)),
        "{uri}"
    );
    let (status, body) = server
        .call(
            &key,
            reqwest::Method::POST,
            &format!("/speakers/{kitchen}/play"),
            Some(json!({"items": []})),
        )
        .await?;
    assert_eq!(
        (status, &body["error"]["details"]["field"]),
        (400, &json!("items"))
    );

    // A refused command fails afterwards, with the reason.
    fake.with(|m| {
        m.failures.insert(
            String::from("players/cmd/power"),
            (11, String::from("nope")),
        );
    });
    let (status, accepted) = server
        .call(
            &key,
            reqwest::Method::PATCH,
            &format!("/speakers/{kitchen}"),
            Some(json!({"powered": false})),
        )
        .await?;
    assert_eq!(status, 202);
    assert_eq!(accepted["speaker"]["availability"], "powered_off");
    let outcome = events
        .outcome_of(accepted["command_id"].as_str().unwrap_or_default())
        .await?;
    assert_eq!(outcome["outcome"], "failed");
    assert_eq!(outcome["error"]["code"], "command_failed");
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn idempotent_play_requests_are_replayed() -> TestResult {
    let (server, fake, cookie) = connected().await?;
    registered(&server, &cookie).await?;
    let key = server.device_key("luke", "phone").await?;
    let list = server
        .until(&key, "/speakers", |_, b| {
            b["speakers"].as_array().is_some_and(|s| s.len() == 2)
        })
        .await?;
    let kitchen = speaker_id(&list, "Kitchen");
    let body = json!({"items": [{"song_id": server.song.as_str()}]});
    let mut ids = Vec::new();
    for _ in 0..2 {
        let response = server
            .http
            .post(server.url(&format!("/speakers/{kitchen}/play")))
            .bearer_auth(&key)
            .header("idempotency-key", "retry-1")
            .json(&body)
            .send()
            .await?;
        assert_eq!(response.status(), 202);
        let value: Value = response.json().await?;
        ids.push(value["command_id"].clone());
    }
    assert_eq!(ids[0], ids[1]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.received_named("player_queues/play_media").len(), 1);
    let stored: i64 = server.test.db.read_blocking(|c| {
        c.query_row("SELECT count(*) FROM idempotency_keys", [], |row| {
            row.get(0)
        })
        .map_err(Into::into)
    })?;
    assert_eq!(
        stored, 0,
        "speaker answers name what plays: replays stay in memory"
    );
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn speakers_become_connecting_then_unavailable_when_music_assistant_goes() -> TestResult {
    let (server, fake, _cookie) = connected().await?;
    let key = server.device_key("luke", "phone").await?;
    let list = server
        .until(&key, "/speakers", |_, b| {
            b["speakers"].as_array().is_some_and(|s| s.len() == 2)
        })
        .await?;
    let kitchen = speaker_id(&list, "Kitchen");
    let mut events = Events::open(&server, &key).await?;

    fake.with(|m| m.reachable = false);
    fake.disconnect_all();
    let connecting = events.next("speakers").await?;
    assert_eq!(connecting, json!({"state": "connecting"}));
    let unavailable = events.next("speakers").await?;
    assert_eq!(unavailable["state"], "unavailable");
    assert!(unavailable["since"].is_string());
    let list = server
        .until(&key, "/speakers", |_, b| b["state"] == "unavailable")
        .await?;
    assert_eq!(list["speakers"], json!([]), "an empty list, never an error");
    let response = server
        .http
        .get(server.url(&format!("/speakers/{kitchen}")))
        .bearer_auth(&key)
        .send()
        .await?;
    assert_eq!(response.status(), 503);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("10")
    );
    let body: Value = response.json().await?;
    assert_eq!(body["error"]["code"], "speakers_unavailable");
    let (_, me) = server.call(&key, reqwest::Method::GET, "/me", None).await?;
    assert!(
        me["features"]["speakers"].is_object(),
        "the feature stays while Music Assistant is away"
    );

    fake.with(|m| m.reachable = true);
    let back = events.next("speakers").await?;
    assert_eq!(back, json!({"state": "ok"}));
    events.next("speaker_added").await?;
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn members_without_can_use_speakers_see_no_speakers() -> TestResult {
    let (server, _fake, cookie) = connected().await?;
    let response = server
        .http
        .post(server.url("/admin/users"))
        .headers(server.session(&cookie))
        .json(
            &json!({"username": "mia", "password": PASSWORD, "library_ids": [],
                      "permissions": {"can_use_speakers": false}}),
        )
        .send()
        .await?;
    assert_eq!(response.status(), 201, "{:?}", response.text().await);
    let key = server.device_key("mia", "mia-phone").await?;
    let (status, _) = server
        .call(&key, reqwest::Method::GET, "/speakers", None)
        .await?;
    assert_eq!(status, 404);
    let (_, me) = server.call(&key, reqwest::Method::GET, "/me", None).await?;
    assert!(me["features"].get("speakers").is_none(), "{me}");
    assert_eq!(me["user"]["permissions"]["can_use_speakers"], false);
    assert!(
        me["features"]["catalog"].is_object(),
        "can_play_services is separate"
    );

    // A read-only key sees speakers but cannot control them.
    let admin_id = server
        .test
        .db
        .read_blocking(|c| {
            trackside_core::repo::users::by_username(c, "luke").map(|u| u.map(|u| u.key))
        })?
        .ok_or("luke")?;
    let issued = server
        .auth
        .create_key(
            admin_id,
            trackside_core::repo::keys::KeyKind::ApiKey,
            "Reader",
            trackside_auth::Scopes::LIBRARY,
        )
        .await?;
    let (_, me) = server
        .call(&issued.secret, reqwest::Method::GET, "/me", None)
        .await?;
    assert_eq!(me["features"]["speakers"], json!({"control": false}));
    let list = server
        .until(&issued.secret, "/speakers", |_, b| {
            b["speakers"].as_array().is_some_and(|s| s.len() == 2)
        })
        .await?;
    let kitchen = speaker_id(&list, "Kitchen");
    let (status, _) = server
        .call(
            &issued.secret,
            reqwest::Method::POST,
            &format!("/speakers/{kitchen}/playback"),
            Some(json!({"action": "stop"})),
        )
        .await?;
    assert_eq!(status, 403);
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn speaker_commands_are_rate_limited_per_user() -> TestResult {
    let (server, _fake, _cookie) = connected().await?;
    let key = server.device_key("luke", "phone").await?;
    let list = server
        .until(&key, "/speakers", |_, b| {
            b["speakers"].as_array().is_some_and(|s| s.len() == 2)
        })
        .await?;
    let kitchen = speaker_id(&list, "Kitchen");
    let mut statuses = Vec::new();
    for volume in 0..40 {
        let (status, _) = server
            .call(
                &key,
                reqwest::Method::PATCH,
                &format!("/speakers/{kitchen}"),
                Some(json!({"volume": volume})),
            )
            .await?;
        statuses.push(status);
    }
    assert!(statuses[..30].iter().all(|s| *s == 202), "{statuses:?}");
    assert!(statuses.contains(&429), "{statuses:?}");
    let _ = futures_util::stream::empty::<()>().next().await;
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn the_token_goes_nowhere_new_without_a_recent_password() -> TestResult {
    let (server, fake, cookie) = connected().await?;
    let elsewhere = FakeAssistant::start(model()).await;
    server.test.clock.advance(Duration::from_mins(11));
    let post = |body: Value| {
        server.admin(
            &cookie,
            reqwest::Method::POST,
            "/music-assistant/test",
            Some(body),
        )
    };
    let (status, body) = post(json!({"url": elsewhere.url()})).await?;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "reauth_required");
    let (status, body) = post(json!({"url": fake.url()})).await?;
    assert_eq!((status, &body["ok"]), (200, &json!(true)), "{body}");
    let (status, body) = post(json!({"url": elsewhere.url(), "token": elsewhere.token()})).await?;
    assert_eq!((status, &body["ok"]), (200, &json!(true)), "{body}");

    let patch = |body: Value| {
        server.admin(
            &cookie,
            reqwest::Method::PATCH,
            "/music-assistant",
            Some(body),
        )
    };
    for change in [
        json!({"ca_pem": "-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n"}),
        json!({"ca_pem": null}),
        json!({"lan_url": "http://192.168.1.99:4747"}),
        json!({"url": elsewhere.url()}),
    ] {
        let (status, body) = patch(change.clone()).await?;
        assert_eq!(status, 403, "{change}: {body}");
        assert_eq!(body["error"]["code"], "reauth_required", "{change}");
    }
    let (status, body) = patch(json!({"hidden_services": []})).await?;
    assert_eq!(status, 200, "{body}");
    server.assistant.shutdown().await;
    Ok(())
}

async fn ping_with(server: &Server, secret: &str) -> Result<String, Error> {
    Ok(server
        .http
        .get(format!(
            "{}/rest/ping.view?apiKey={secret}&f=json&c=ma&v=1.16.1",
            server.host
        ))
        .send()
        .await?
        .text()
        .await?)
}

async fn live_managed_keys(server: &Server, cookie: &str) -> Result<usize, Error> {
    let (_, keys) = server
        .admin(cookie, reqwest::Method::GET, "/api-keys", None)
        .await?;
    Ok(keys["keys"].as_array().map_or(0, |k| {
        k.iter()
            .filter(|k| k["managed_by"] == "music_assistant")
            .count()
    }))
}

#[tokio::test]
async fn moving_to_another_music_assistant_retires_the_old_source_and_key() -> TestResult {
    let (server, first, cookie) = connected().await?;
    registered(&server, &cookie).await?;
    let submitted = first.wait_for("config/flows/submit", 1).await;
    let old = submitted[0].args["values"]["api_key"]
        .as_str()
        .ok_or("no key")?
        .to_owned();
    assert!(
        ping_with(&server, &old)
            .await?
            .contains("\"status\":\"ok\"")
    );

    let mut other = model();
    other.server_id = String::from("another-server");
    let second = FakeAssistant::start(other).await;
    server.connect(&cookie, &second).await?;
    first.wait_for("config/providers/remove", 1).await;
    let after = ping_with(&server, &old).await?;
    assert!(after.contains("\"status\":\"failed\""), "{after}");
    registered(&server, &cookie).await?;
    let submitted = second.wait_for("config/flows/submit", 1).await;
    let new = submitted[0].args["values"]["api_key"]
        .as_str()
        .ok_or("no key")?
        .to_owned();
    assert!(
        ping_with(&server, &new)
            .await?
            .contains("\"status\":\"ok\"")
    );
    assert_eq!(live_managed_keys(&server, &cookie).await?, 1);
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_reinstalled_music_assistant_at_the_same_address_gets_a_new_key() -> TestResult {
    let (server, fake, cookie) = connected().await?;
    registered(&server, &cookie).await?;
    let submitted = fake.wait_for("config/flows/submit", 1).await;
    let old = submitted[0].args["values"]["api_key"]
        .as_str()
        .ok_or("no key")?
        .to_owned();
    fake.with(|m| m.server_id = String::from("reinstalled"));
    fake.disconnect_all();
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !ping_with(&server, &old)
        .await?
        .contains("\"status\":\"failed\"")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the old key still works"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let submitted = fake.wait_for("config/flows/submit", 2).await;
    let new = submitted[1].args["values"]["api_key"]
        .as_str()
        .ok_or("no key")?
        .to_owned();
    assert!(
        ping_with(&server, &new)
            .await?
            .contains("\"status\":\"ok\"")
    );
    assert_eq!(live_managed_keys(&server, &cookie).await?, 1);
    server.assistant.shutdown().await;
    Ok(())
}
