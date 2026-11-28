//! The phone relay over real HTTP, against the fake Music Assistant (`trackside-mass`) and the
//! fake Sendspin server (`trackside-sendspin`): a session and its HLS under the owning
//! device's media token, generations, tiers, limits, owner-only speakers and events, transfer
//! to this device, Music Assistant going away, device revocation and shutdown.

#[path = "../../trackside-relay/tests/support/mod.rs"]
#[allow(unused_imports)]
mod tone;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use trackside_api::{ApiState, LocalState, NoOps, ServerInfo, wire};
use trackside_assistant::{Assistant, Deps, RelayTuning, Tuning};
use trackside_auth::{Auth, SecretKey};
use trackside_core::testing::{FakeAcquireControl, FakeScanControl, TestDb};
use trackside_core::{BoxFuture, Config, ConfigReport, SpeakerId};
use trackside_mass::testing::{FakeAssistant, Model, player, provider, queue};
use trackside_media::{Media, MediaError, TranscodeProcess, TranscodeSpec, Transcoder};
use trackside_sendspin::testing::{FakeConn, FakeServer};

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

/// The server as run.rs wires it, with the relay pointed at `spin` and short waits.
async fn start(
    spin: &FakeServer,
    edit: impl FnOnce(&mut Config, &mut RelayTuning),
) -> Result<Server, Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let test = TestDb::new()?;
    let data = tempfile::tempdir()?;
    let mut config = Config::defaults(data.path());
    config.auth.argon2_memory = 1 << 16;
    config.auth.argon2_iterations = 1;
    config.media.ffmpeg_path = Some(data.path().join("no-ffmpeg"));
    let mut relay = RelayTuning {
        sendspin_url: Some(spin.url()),
        fetch_timeout: Duration::from_secs(20),
        paused_timeout: Duration::from_secs(20),
        activation_timeout: Duration::from_secs(10),
        first_segment_wait: Duration::from_millis(500),
        segment_wait: Duration::from_millis(300),
        player_wait: Duration::from_secs(5),
    };
    edit(&mut config, &mut relay);
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
            relay,
        },
    })
    .await;
    let state = ApiState {
        db: test.db.clone(),
        auth: Arc::clone(&auth),
        media,
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
    let app = trackside_api::router(state).layer(axum::middleware::from_fn(test_peer));
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

/// A response as the tests look at it.
struct Got {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: bytes::Bytes,
}

impl Got {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
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

    async fn add_user(&self, cookie: &str, name: &str, permissions: Value) -> Result<(), Error> {
        let response = self
            .http
            .post(self.url("/admin/users"))
            .headers(self.session(cookie))
            .json(
                &json!({"username": name, "password": PASSWORD, "library_ids": [],
                          "permissions": permissions}),
            )
            .send()
            .await?;
        assert_eq!(response.status(), 201, "{}", response.text().await?);
        Ok(())
    }

    async fn device_key(&self, user: &str, device: &str) -> Result<String, Error> {
        let response = self
            .http
            .post(self.url("/auth/login"))
            .json(&json!({"username": user, "password": PASSWORD,
                "device": {"client_device_id": device, "name": device, "platform": "ios"}}))
            .send()
            .await?;
        assert_eq!(response.status(), 201);
        let body: wire::auth::PairResult = response.json().await?;
        Ok(body.api_key)
    }

    async fn media_token(&self, key: &str) -> Result<String, Error> {
        let (status, body) = self
            .call(key, reqwest::Method::POST, "/media-token", None)
            .await?;
        assert_eq!(status, 200, "{body}");
        Ok(body["token"].as_str().ok_or("token")?.to_owned())
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

    /// A playlist or segment by its absolute path (`/api/v1/relay/...`) and query.
    async fn media(&self, method: reqwest::Method, path_and_query: &str) -> Result<Got, Error> {
        let response = self
            .http
            .request(method, format!("{}{path_and_query}", self.host))
            .send()
            .await?;
        Ok(Got {
            status: response.status().as_u16(),
            headers: response.headers().clone(),
            body: response.bytes().await?,
        })
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

    async fn until_media(
        &self,
        path_and_query: &str,
        done: impl Fn(&Got) -> bool,
    ) -> Result<Got, Error> {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let got = self.media(reqwest::Method::GET, path_and_query).await?;
            if done(&got) {
                return Ok(got);
            }
            if tokio::time::Instant::now() > deadline {
                return Err(format!("{path_and_query}: {} {}", got.status, got.text()).into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// An event stream for `key`, collecting text until `wanted` shows up.
    async fn events(&self, key: &str) -> Result<Events, Error> {
        let response = self
            .http
            .get(self.url("/events"))
            .bearer_auth(key)
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        Ok(Events {
            stream: Box::pin(response.bytes_stream()),
            seen: String::new(),
        })
    }
}

type ByteStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

struct Events {
    stream: ByteStream,
    seen: String,
}

impl Events {
    async fn until(&mut self, wanted: &str) -> Result<String, Error> {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        while !self.seen.contains(wanted) {
            let chunk = tokio::time::timeout_at(deadline, self.stream.next())
                .await
                .map_err(|_| format!("no {wanted} in {}", self.seen))?
                .ok_or("the stream ended")??;
            self.seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        Ok(self.seen.clone())
    }

    /// What arrives within `wait`.
    async fn within(&mut self, wait: Duration) -> String {
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Some(Ok(chunk))) = tokio::time::timeout_at(deadline, self.stream.next()).await
        {
            self.seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        self.seen.clone()
    }
}

const SPOTIFY: &str = "spotify--Xb4LkP2q";

fn model() -> Model {
    let mut model = Model::default();
    model.players = vec![player("sonos-kitchen", "Kitchen", "sonos--Kq2m8VbR")];
    let mut kitchen = queue("sonos-kitchen");
    kitchen["items"] = json!(3);
    kitchen["state"] = json!("playing");
    model.queues = vec![kitchen];
    model.providers = vec![
        provider("sonos--Kq2m8VbR", "sonos", "player", "Sonos", false),
        provider("sendspin--Sp1n", "sendspin", "player", "Sendspin", false),
        provider(SPOTIFY, "spotify", "music", "Spotify", true),
    ];
    model
}

struct Rig {
    server: Server,
    fake: FakeAssistant,
    spin: FakeServer,
    cookie: String,
}

async fn rig(edit: impl FnOnce(&mut Config, &mut RelayTuning)) -> Result<Rig, Error> {
    rig_with(model(), edit).await
}

async fn rig_with(
    model: Model,
    edit: impl FnOnce(&mut Config, &mut RelayTuning),
) -> Result<Rig, Error> {
    rig_on(FakeServer::start().await, model, json!({}), edit).await
}

/// A rig on `spin`, with `settings` added to the Music Assistant settings it saves.
async fn rig_on(
    spin: FakeServer,
    model: Model,
    settings: Value,
    edit: impl FnOnce(&mut Config, &mut RelayTuning),
) -> Result<Rig, Error> {
    let server = start(&spin, edit).await?;
    let fake = FakeAssistant::start(model).await;
    let cookie = server.setup_admin().await?;
    let mut patch = json!({"url": fake.url(), "token": fake.token(), "auto_register": false});
    if let (Some(patch), Some(extra)) = (patch.as_object_mut(), settings.as_object()) {
        patch.extend(extra.clone());
    }
    let (status, body) = server
        .admin(
            &cookie,
            reqwest::Method::PATCH,
            "/music-assistant",
            Some(patch),
        )
        .await?;
    assert_eq!(status, 200, "{body}");
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let (_, status) = server
            .admin(&cookie, reqwest::Method::GET, "/music-assistant", None)
            .await?;
        if status["server"]["relay_supported"] == true {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{status}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(Rig {
        server,
        fake,
        spin,
        cookie,
    })
}

/// Accepts the relay's Sendspin connection, checks it carries the Music Assistant token,
/// brings it to an active player and lists that player in Music Assistant.
async fn join(rig: &mut Rig) -> FakeConn {
    let mut conn = rig.spin.accept().await;
    let auth = conn.expect_auth().await;
    assert_eq!(auth["token"], rig.fake.token().as_str());
    conn.auth_ok().await;
    conn.ready().await;
    let id = conn.client_id();
    let mut phone = player(&id, "Luke's iPhone", "sendspin--Sp1n");
    phone["type"] = json!("player");
    phone["hide_in_ui"] = json!(true);
    phone["supported_features"] = json!(["pause", "seek", "next_previous", "play_media"]);
    let phone_queue = queue(&id);
    rig.fake.with(|m| {
        m.players.push(phone.clone());
        m.queues.push(phone_queue.clone());
    });
    rig.fake.emit("player_added", Some(&id), phone);
    rig.fake.emit("queue_added", Some(&id), phone_queue);
    conn
}

/// Opens a lossless session for `key` and returns its body.
async fn open(server: &Server, key: &str) -> Result<Value, Error> {
    let (status, session) = server
        .call(
            key,
            reqwest::Method::POST,
            "/relay/sessions",
            Some(json!({"tier": "lossless"})),
        )
        .await?;
    assert_eq!(status, 201, "{session}");
    Ok(session)
}

fn hls(session: &Value) -> String {
    session["stream"]["hls"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// Streams the tone with timestamps far enough in the past that every segment is due.
async fn play_tone(conn: &mut FakeConn) {
    let (header, frames) = tone::tone();
    conn.start_stream(&header).await;
    let start = conn.server_now_us() - 20_000_000;
    tone::send_frames(conn, start, &frames).await;
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one session, start to end")]
async fn a_phone_plays_through_its_relay_under_its_own_media_token() -> TestResult {
    let mut rig = rig(|_, _| {}).await?;
    let server = &rig.server;
    let key = server.device_key("luke", "iphone").await?;
    let mt = server.media_token(&key).await?;
    let (_, me) = server.call(&key, reqwest::Method::GET, "/me", None).await?;
    assert_eq!(
        me["features"]["catalog"],
        json!({"relay": true}),
        "lossless only without ffmpeg"
    );
    let mut events = server.events(&key).await?;

    let session = open(server, &key).await?;
    assert_eq!(session["state"], "starting");
    assert_eq!(session["generation"], 0);
    assert_eq!(session["tier"], "lossless");
    assert_eq!(session["latency_ms"], 4000);
    let id = session["id"].as_str().ok_or("id")?.to_owned();
    assert!(id.starts_with('h'), "{id}");
    assert_eq!(hls(&session), format!("/api/v1/relay/{id}/1/master.m3u8"));
    let (status, again) = server
        .call(&key, reqwest::Method::POST, "/relay/sessions", None)
        .await?;
    assert_eq!((status, &again["id"]), (200, &json!(id)), "one per device");

    let mut conn = join(&mut rig).await;
    let server = &rig.server;
    let speaker_id = SpeakerId::derive(&Model::default().server_id, &conn.client_id());
    assert_eq!(session["speaker_id"], speaker_id.to_string());
    let speakers = server
        .until(&key, "/speakers", |_, b| {
            b["speakers"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s["kind"] == "phone"))
        })
        .await?;
    let phone = speakers["speakers"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["kind"] == "phone"))
        .ok_or("phone")?;
    assert_eq!(phone["id"], speaker_id.to_string());
    assert_eq!(phone["protocol"], "relay");
    assert_eq!(phone["relay"]["session_id"], json!(id));
    assert_eq!(phone["relay"]["this_device"], true);
    assert!(phone.get("volume").is_none() && phone.get("can_group_with").is_none());
    let kitchen = speakers["speakers"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["name"] == "Kitchen"))
        .ok_or("kitchen")?;
    assert!(
        !kitchen.to_string().contains(&speaker_id.to_string()),
        "never a grouping target"
    );

    play_tone(&mut conn).await;
    let live = server
        .until(&key, &format!("/relay/sessions/{id}"), |_, b| {
            b["state"] == "playing" && b["generation"] == 1
        })
        .await?;
    assert_eq!(hls(&live), hls(&session), "the first URL plays");

    let master = server
        .media(reqwest::Method::GET, &format!("{}?mt={mt}", hls(&live)))
        .await?;
    assert_eq!(master.status, 200, "{}", master.text());
    assert_eq!(
        master.header("content-type"),
        "application/vnd.apple.mpegurl"
    );
    assert_eq!(master.header("cache-control"), "no-store");
    assert!(
        master.text().contains("CODECS=\"fLaC\""),
        "{}",
        master.text()
    );
    assert!(master.text().contains(&format!("index.m3u8?mt={mt}")));
    let base = format!("/api/v1/relay/{id}/1");
    let index = server
        .until_media(&format!("{base}/index.m3u8?mt={mt}"), |g| {
            g.text().contains("#EXTINF")
        })
        .await?;
    assert_eq!(index.header("cache-control"), "no-store");
    assert!(index.text().contains(&format!("0.m4s?mt={mt}")));
    assert!(index.text().contains(&format!("init.mp4?mt={mt}")));
    let init = server
        .media(reqwest::Method::GET, &format!("{base}/init.mp4?mt={mt}"))
        .await?;
    assert_eq!(
        (init.status, init.header("content-type")),
        (200, "audio/mp4")
    );
    assert_eq!(&init.body[4..8], b"ftyp");
    let segment = server
        .media(reqwest::Method::GET, &format!("{base}/0.m4s?mt={mt}"))
        .await?;
    assert_eq!(segment.status, 200);
    assert_eq!(segment.header("cache-control"), "no-store");
    assert_eq!(&segment.body[4..8], b"moof");
    let head = server
        .media(reqwest::Method::HEAD, &format!("{base}/0.m4s?mt={mt}"))
        .await?;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());

    // Without, with a forged, or with another device's token: nothing.
    assert_eq!(
        server
            .media(reqwest::Method::GET, &format!("{base}/0.m4s"))
            .await?
            .status,
        401
    );
    assert_eq!(
        server
            .media(reqwest::Method::GET, &format!("{base}/0.m4s?mt=forged"))
            .await?
            .status,
        401
    );
    let other_device = server.device_key("luke", "ipad").await?;
    let other_mt = server.media_token(&other_device).await?;
    assert_eq!(
        server
            .media(reqwest::Method::GET, &format!("{base}/0.m4s?mt={other_mt}"))
            .await?
            .status,
        404,
        "a media token works only for the device that owns the session"
    );
    for wrong in [
        "/7/master.m3u8",
        "/1/seg-0.m4s",
        "/x/index.m3u8",
        "/1/99999.m4s",
    ] {
        let got = server
            .media(
                reqwest::Method::GET,
                &format!("/api/v1/relay/{id}{wrong}?mt={mt}"),
            )
            .await?;
        assert_eq!(got.status, 404, "{wrong}");
    }

    // A seek or skip starts a new generation; the old playlist ends.
    conn.send(
        "stream/clear",
        json!({"server_transmitted": conn.server_now_us()}),
    )
    .await;
    let moved = server
        .until(&key, &format!("/relay/sessions/{id}"), |_, b| {
            b["generation"] == 2
        })
        .await?;
    assert_eq!(hls(&moved), format!("/api/v1/relay/{id}/2/master.m3u8"));
    let old = server
        .media(reqwest::Method::GET, &format!("{base}/index.m3u8?mt={mt}"))
        .await?;
    assert!(old.text().ends_with("#EXT-X-ENDLIST\n"), "{}", old.text());

    let seen = events.until("\"generation\":2").await?;
    assert!(seen.contains("event: relay"), "{seen}");
    assert!(seen.contains("event: speaker_added"), "{seen}");
    assert!(!seen.contains(&mt), "tokens never travel in events");

    let (status, _) = server
        .call(
            &key,
            reqwest::Method::DELETE,
            &format!("/relay/sessions/{id}"),
            None,
        )
        .await?;
    assert_eq!(status, 204);
    let goodbye = conn.expect("client/goodbye").await;
    assert_eq!(goodbye["reason"], "user_request");
    let ended = server
        .until(&key, &format!("/relay/sessions/{id}"), |_, b| {
            b["state"] == "ended"
        })
        .await?;
    assert_eq!(ended["ended_reason"], "closed");
    assert!(ended.get("stream").is_none());
    events.until("event: speaker_removed").await?;
    let (status, body) = server
        .call(
            &key,
            reqwest::Method::POST,
            &format!("/speakers/{speaker_id}/playback"),
            Some(json!({"action": "pause"})),
        )
        .await?;
    assert_eq!(
        (status, &body["error"]["details"]["reason"]),
        (409, &json!("relay_ended"))
    );
    rig.server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn only_device_keys_of_people_who_may_play_services_get_a_relay() -> TestResult {
    let rig = rig(|_, _| {}).await?;
    let server = &rig.server;
    let response = server
        .http
        .post(server.url("/relay/sessions"))
        .headers(server.session(&rig.cookie))
        .json(&json!({}))
        .send()
        .await?;
    assert_eq!(response.status(), 403, "a dashboard session");
    server
        .add_user(&rig.cookie, "mia", json!({"can_play_services": false}))
        .await?;
    let mia = server.device_key("mia", "pixel").await?;
    let (status, _) = server
        .call(&mia, reqwest::Method::POST, "/relay/sessions", None)
        .await?;
    assert_eq!(status, 403, "can_play_services is off");
    let key = server.device_key("luke", "iphone").await?;
    let (status, body) = server
        .call(
            &key,
            reqwest::Method::POST,
            "/relay/sessions",
            Some(json!({"tier": "cellular"})),
        )
        .await?;
    assert_eq!(
        (status, &body["error"]["details"]["capability"]),
        (422, &json!("relay_cellular")),
        "no ffmpeg"
    );
    let (status, _) = server
        .call(
            &key,
            reqwest::Method::POST,
            "/relay/sessions",
            Some(json!({"tier": "dolby"})),
        )
        .await?;
    assert_eq!(status, 400);
    rig.server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_server_without_sendspin_cannot_relay() -> TestResult {
    let mut plain = model();
    plain.providers.retain(|p| p["domain"] != "sendspin");
    let spin = FakeServer::start().await;
    let server = start(&spin, |_, _| {}).await?;
    let fake = FakeAssistant::start(plain).await;
    let cookie = server.setup_admin().await?;
    server
        .admin(
            &cookie,
            reqwest::Method::PATCH,
            "/music-assistant",
            Some(json!({"url": fake.url(), "token": fake.token(), "auto_register": false})),
        )
        .await?;
    let key = server.device_key("luke", "iphone").await?;
    server
        .until(&key, "/speakers", |_, b| b["state"] == "ok")
        .await?;
    let (_, me) = server.call(&key, reqwest::Method::GET, "/me", None).await?;
    assert_eq!(me["features"]["catalog"], json!({}), "{me}");
    let (status, body) = server
        .call(&key, reqwest::Method::POST, "/relay/sessions", None)
        .await?;
    assert_eq!(
        (status, &body["error"]["details"]["capability"]),
        (422, &json!("relay"))
    );
    server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn sessions_stay_within_the_device_user_and_server_limits() -> TestResult {
    let rig = rig(|config, _| config.music_assistant.relay_max_per_user = 2).await?;
    let server = &rig.server;
    let phone = server.device_key("luke", "iphone").await?;
    let tablet = server.device_key("luke", "ipad").await?;
    let mac = server.device_key("luke", "mac").await?;
    open(server, &phone).await?;
    open(server, &tablet).await?;
    let (status, body) = server
        .call(&mac, reqwest::Method::POST, "/relay/sessions", None)
        .await?;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        body["error"]["details"],
        json!({"limit": 2, "reason": "relay_limit"})
    );
    let (_, list) = server
        .call(&mac, reqwest::Method::GET, "/relay/sessions", None)
        .await?;
    assert_eq!(
        list["sessions"].as_array().map(Vec::len),
        Some(2),
        "all devices"
    );

    let (status, _) = server
        .admin(
            &rig.cookie,
            reqwest::Method::PATCH,
            "/music-assistant",
            Some(json!({"relay_max_sessions": 2})),
        )
        .await?;
    assert_eq!(status, 200);
    server.add_user(&rig.cookie, "mia", json!({})).await?;
    let mia = server.device_key("mia", "pixel").await?;
    let (status, body) = server
        .call(&mia, reqwest::Method::POST, "/relay/sessions", None)
        .await?;
    assert_eq!(
        (status, &body["error"]["details"]["limit"]),
        (409, &json!(2)),
        "the server limit changes without a restart"
    );
    let (_, status) = server
        .admin(&rig.cookie, reqwest::Method::GET, "/music-assistant", None)
        .await?;
    assert_eq!(status["relays"], json!({"active": 2, "max": 2}));
    rig.server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_relay_speaker_and_its_events_are_its_owners_alone() -> TestResult {
    let mut rig = rig(|_, _| {}).await?;
    rig.server.add_user(&rig.cookie, "mia", json!({})).await?;
    let mia = rig.server.device_key("mia", "pixel").await?;
    let mut theirs = rig.server.events(&mia).await?;
    let key = rig.server.device_key("luke", "iphone").await?;
    let session = open(&rig.server, &key).await?;
    let _conn = join(&mut rig).await;
    let server = &rig.server;
    let id = session["id"].as_str().ok_or("id")?;
    let speaker = session["speaker_id"].as_str().ok_or("speaker")?;
    server
        .until(&key, &format!("/speakers/{speaker}"), |status, _| {
            status == 200
        })
        .await?;
    let (_, list) = server
        .call(&mia, reqwest::Method::GET, "/speakers", None)
        .await?;
    assert_eq!(list["speakers"].as_array().map(Vec::len), Some(1), "{list}");
    for (method, path, body) in [
        (reqwest::Method::GET, format!("/speakers/{speaker}"), None),
        (
            reqwest::Method::GET,
            format!("/speakers/{speaker}/queue"),
            None,
        ),
        (
            reqwest::Method::POST,
            format!("/speakers/{speaker}/playback"),
            Some(json!({"action": "pause"})),
        ),
        (reqwest::Method::GET, format!("/relay/sessions/{id}"), None),
        (
            reqwest::Method::DELETE,
            format!("/relay/sessions/{id}"),
            None,
        ),
        (
            reqwest::Method::PATCH,
            format!("/relay/sessions/{id}"),
            Some(json!({"tier": "lossless"})),
        ),
    ] {
        let (status, _) = server.call(&mia, method, &path, body).await?;
        assert_eq!(status, 404, "{path}");
    }
    let seen = theirs.within(Duration::from_millis(600)).await;
    assert!(!seen.contains("event: relay"), "{seen}");
    assert!(!seen.contains(speaker), "{seen}");
    rig.server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_speakers_queue_moves_to_this_device() -> TestResult {
    let mut rig = rig(|_, _| {}).await?;
    let key = rig.server.device_key("luke", "iphone").await?;
    let kitchen = SpeakerId::derive(&Model::default().server_id, "sonos-kitchen");
    rig.server
        .until(&key, &format!("/speakers/{kitchen}"), |status, b| {
            status == 200
                && b["capabilities"]
                    .as_array()
                    .is_some_and(|c| c.contains(&json!("transfer")))
        })
        .await?;
    let (status, reply) = rig
        .server
        .call(
            &key,
            reqwest::Method::POST,
            &format!("/speakers/{kitchen}/transfer"),
            Some(json!({"to": "this_device"})),
        )
        .await?;
    assert_eq!(status, 202, "{reply}");
    assert_eq!(reply["action"], "transfer");
    assert_eq!(reply["speaker"]["kind"], "phone");
    assert_eq!(reply["speaker"]["relay"]["this_device"], true);
    assert_eq!(reply["relay"]["state"], "starting");
    assert_eq!(reply["relay"]["speaker_id"], reply["speaker"]["id"]);
    let conn = join(&mut rig).await;
    let sent = rig.fake.wait_for("player_queues/transfer", 1).await;
    assert_eq!(sent[0].args["source_queue_id"], "sonos-kitchen");
    assert_eq!(sent[0].args["target_queue_id"], conn.client_id().as_str());

    let session_key = rig.server.device_key("luke", "ipad").await?;
    let (status, again) = rig
        .server
        .call(&session_key, reqwest::Method::GET, "/relay/sessions", None)
        .await?;
    assert_eq!(
        (status, again["sessions"].as_array().map(Vec::len)),
        (200, Some(1))
    );
    rig.server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn nobody_listening_pauses_music_assistant() -> TestResult {
    let mut rig = rig(|_, relay| relay.fetch_timeout = Duration::from_millis(500)).await?;
    let key = rig.server.device_key("luke", "iphone").await?;
    open(&rig.server, &key).await?;
    let mut conn = join(&mut rig).await;
    play_tone(&mut conn).await;
    let paused = rig.fake.wait_for("player_queues/pause", 1).await;
    assert_eq!(paused[0].args["queue_id"], conn.client_id().as_str());
    rig.server.assistant.shutdown().await;
    Ok(())
}

/// The stream slots of `username` still free, taking none for long.
fn free_stream_slots(server: &Server, username: &str) -> Result<usize, Error> {
    let name = username.to_owned();
    let user = server
        .test
        .db
        .read_blocking(move |c| trackside_core::repo::users::by_username(c, &name))?
        .ok_or("no such user")?;
    let taken: Vec<_> = std::iter::from_fn(|| server.auth.user_stream_slot(user.key)).collect();
    Ok(taken.len())
}

async fn until_free_slots(server: &Server, username: &str, wanted: usize) -> TestResult {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let free = free_stream_slots(server, username)?;
        if free == wanted {
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            return Err(format!("{free} stream slots free, wanted {wanted}").into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_playing_relay_holds_a_stream_slot_until_it_pauses_or_ends() -> TestResult {
    let mut rig = rig(|_, _| {}).await?;
    let key = rig.server.device_key("luke", "iphone").await?;
    let session = open(&rig.server, &key).await?;
    let path = format!("/relay/sessions/{}", session["id"].as_str().ok_or("id")?);
    let mut conn = join(&mut rig).await;
    rig.server
        .until(&key, &path, |_, s| s["state"] == "ready")
        .await?;
    assert_eq!(
        free_stream_slots(&rig.server, "luke")?,
        8,
        "ready holds none"
    );

    play_tone(&mut conn).await;
    rig.server
        .until(&key, &path, |_, s| s["state"] == "playing")
        .await?;
    until_free_slots(&rig.server, "luke", 7).await?;

    conn.send("stream/end", json!({})).await;
    rig.server
        .until(&key, &path, |_, s| s["state"] == "paused")
        .await?;
    until_free_slots(&rig.server, "luke", 8).await?;

    play_tone(&mut conn).await;
    until_free_slots(&rig.server, "luke", 7).await?;
    let (status, _) = rig
        .server
        .call(&key, reqwest::Method::DELETE, &path, None)
        .await?;
    assert_eq!(status, 204);
    until_free_slots(&rig.server, "luke", 8).await?;
    assert!(
        rig.fake.received_named("player_queues/pause").is_empty(),
        "within the limit nothing is paused"
    );
    rig.server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_relay_over_the_users_stream_limit_pauses_music_assistant() -> TestResult {
    let mut rig = rig(|_, _| {}).await?;
    let key = rig.server.device_key("luke", "iphone").await?;
    let session = open(&rig.server, &key).await?;
    let path = format!("/relay/sessions/{}", session["id"].as_str().ok_or("id")?);
    let mut conn = join(&mut rig).await;
    let user = rig
        .server
        .test
        .db
        .read_blocking(|c| trackside_core::repo::users::by_username(c, "luke"))?
        .ok_or("luke")?;
    let streams: Vec<_> =
        std::iter::from_fn(|| rig.server.auth.user_stream_slot(user.key)).collect();
    assert_eq!(streams.len(), 8);

    play_tone(&mut conn).await;
    let paused = rig.fake.wait_for("player_queues/pause", 1).await;
    assert_eq!(paused[0].args["queue_id"], conn.client_id().as_str());
    rig.server
        .until(&key, &path, |_, s| s["state"] == "playing")
        .await?;

    drop(streams);
    conn.send("stream/end", json!({})).await;
    rig.server
        .until(&key, &path, |_, s| s["state"] == "paused")
        .await?;
    play_tone(&mut conn).await;
    until_free_slots(&rig.server, "luke", 7).await?;
    rig.server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn the_relay_trusts_music_assistants_custom_ca() -> TestResult {
    let spin = FakeServer::start().await;
    let front = spin.tls_front().await;
    let url = front.url.clone();
    let mut rig = rig_on(spin, model(), json!({"ca_pem": front.pem}), |_, relay| {
        relay.sendspin_url = Some(url);
    })
    .await?;
    let key = rig.server.device_key("luke", "iphone").await?;
    let session = open(&rig.server, &key).await?;
    let path = format!("/relay/sessions/{}", session["id"].as_str().ok_or("id")?);
    let _conn = join(&mut rig).await;
    rig.server
        .until(&key, &path, |_, s| s["state"] == "ready")
        .await?;
    rig.server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn without_the_custom_ca_the_relay_refuses_a_private_certificate() -> TestResult {
    let spin = FakeServer::start().await;
    let front = spin.tls_front().await;
    let url = front.url.clone();
    let mut rig = rig_on(spin, model(), json!({}), |_, relay| {
        relay.sendspin_url = Some(url);
    })
    .await?;
    let key = rig.server.device_key("luke", "iphone").await?;
    open(&rig.server, &key).await?;
    assert!(
        rig.spin.try_accept(Duration::from_secs(1)).await.is_none(),
        "the platform store does not know the certificate"
    );
    rig.server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn sessions_end_when_music_assistant_goes_away() -> TestResult {
    let mut rig = rig(|_, _| {}).await?;
    let key = rig.server.device_key("luke", "iphone").await?;
    let session = open(&rig.server, &key).await?;
    let _conn = join(&mut rig).await;
    let id = session["id"].as_str().ok_or("id")?;
    let mut events = rig.server.events(&key).await?;
    rig.fake.with(|m| m.reachable = false);
    rig.fake.disconnect_all();
    let ended = rig
        .server
        .until(&key, &format!("/relay/sessions/{id}"), |_, b| {
            b["state"] == "ended"
        })
        .await?;
    assert_eq!(ended["ended_reason"], "assistant_unavailable");
    let seen = events.until("assistant_unavailable").await?;
    assert!(seen.contains("event: relay"), "{seen}");
    rig.server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn revoking_the_device_ends_its_relay_and_deletes_its_player_key() -> TestResult {
    let mut rig = rig(|_, _| {}).await?;
    let key = rig.server.device_key("luke", "iphone").await?;
    let session = open(&rig.server, &key).await?;
    let _conn = join(&mut rig).await;
    let id = session["id"].as_str().ok_or("id")?.to_owned();
    let device = session["device_id"].as_str().ok_or("device")?.to_owned();
    let rows = |server: &Server| -> Result<Vec<(String, String)>, Error> {
        Ok(server.test.db.read_blocking(|c| {
            let mut statement = c.prepare(
                "SELECT key, value FROM settings WHERE key LIKE 'music_assistant.relay_identity.%'",
            )?;
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<(String, String)>, _>>()?;
            Ok(rows)
        })?)
    };
    let stored = rows(&rig.server)?;
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].0,
        format!("music_assistant.relay_identity.{device}")
    );
    let other = rig.server.device_key("luke", "ipad").await?;
    let (status, _) = rig
        .server
        .call(
            &other,
            reqwest::Method::DELETE,
            &format!("/me/devices/{device}"),
            None,
        )
        .await?;
    assert_eq!(status, 204);
    let ended = rig
        .server
        .until(&other, &format!("/relay/sessions/{id}"), |_, b| {
            b["state"] == "ended"
        })
        .await?;
    assert_eq!(ended["ended_reason"], "revoked");
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !rows(&rig.server)?.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "the key stays");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    rig.server.assistant.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn shutting_down_says_goodbye_to_music_assistant() -> TestResult {
    let mut rig = rig(|_, _| {}).await?;
    let key = rig.server.device_key("luke", "iphone").await?;
    let session = open(&rig.server, &key).await?;
    let id = session["id"].as_str().ok_or("id")?.to_owned();
    let mut conn = join(&mut rig).await;
    let assistant = rig.server.assistant.clone();
    let shutdown = tokio::spawn(async move { assistant.shutdown().await });
    let goodbye = conn.expect("client/goodbye").await;
    assert_eq!(goodbye["reason"], "shutdown");
    shutdown.await?;
    let (status, ended) = rig
        .server
        .call(
            &key,
            reqwest::Method::GET,
            &format!("/relay/sessions/{id}"),
            None,
        )
        .await?;
    assert_eq!(status, 200);
    assert_eq!(
        (&ended["state"], &ended["ended_reason"]),
        (&json!("ended"), &json!("server_restart"))
    );
    Ok(())
}

/// ffmpeg for the cellular tier; `None` skips, unless `TRACKSIDE_TEST_FFMPEG=1` says it must
/// be there.
fn ffmpeg() -> Option<std::path::PathBuf> {
    let found = trackside_relay::find_ffmpeg(None);
    if found.is_none() {
        assert!(
            std::env::var("TRACKSIDE_TEST_FFMPEG").as_deref() != Ok("1"),
            "TRACKSIDE_TEST_FFMPEG=1 but ffmpeg was not found"
        );
        eprintln!("skipping: ffmpeg not found");
    }
    found
}

#[tokio::test]
async fn a_tier_change_while_playing_starts_a_new_generation() -> TestResult {
    let Some(ffmpeg) = ffmpeg() else {
        return Ok(());
    };
    let mut rig = rig(|config, _| config.media.ffmpeg_path = Some(ffmpeg)).await?;
    let key = rig.server.device_key("luke", "iphone").await?;
    let (_, me) = rig
        .server
        .call(&key, reqwest::Method::GET, "/me", None)
        .await?;
    assert_eq!(
        me["features"]["catalog"],
        json!({"relay": true, "relay_cellular": true})
    );
    let session = open(&rig.server, &key).await?;
    let id = session["id"].as_str().ok_or("id")?.to_owned();
    let mut conn = join(&mut rig).await;
    play_tone(&mut conn).await;
    let server = &rig.server;
    server
        .until(&key, &format!("/relay/sessions/{id}"), |_, b| {
            b["generation"] == 1
        })
        .await?;
    let (status, patched) = server
        .call(
            &key,
            reqwest::Method::PATCH,
            &format!("/relay/sessions/{id}"),
            Some(json!({"tier": "cellular"})),
        )
        .await?;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["tier"], "cellular");
    assert_eq!(patched["generation"], 2);
    assert_eq!(
        patched["stream"]["format"],
        json!({"bitrate_kbps": 256, "channels": 2, "codec": "aac", "container": "mp4",
               "sample_rate": 44100})
    );
    assert_eq!(hls(&patched), format!("/api/v1/relay/{id}/2/master.m3u8"));
    let other = server.device_key("luke", "ipad").await?;
    let (status, _) = server
        .call(
            &other,
            reqwest::Method::PATCH,
            &format!("/relay/sessions/{id}"),
            Some(json!({"tier": "lossless"})),
        )
        .await?;
    assert_eq!(status, 404, "only the owning device changes the tier");
    rig.server.assistant.shutdown().await;
    Ok(())
}
