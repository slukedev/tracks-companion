//! Trackside Cloud over real HTTP: device keys only, each user only ever their own data
//! (admins included), the batch path, cursors, settings, quotas from the dashboard settings,
//! the `cloud` event, and capabilities.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use trackside_api::{ApiState, LocalState, NoOps, ServerInfo, wire};
use trackside_auth::{Auth, SecretKey};
use trackside_core::testing::{FakeAcquireControl, FakeScanControl, TestDb};
use trackside_core::{BoxFuture, Config};
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

const PASSWORD: &str = "correct horse battery";

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

    async fn add_member(
        &self,
        cookie: &str,
        name: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let response = self
            .http
            .post(self.url("/admin/users"))
            .headers(self.session(cookie))
            .json(&json!({"username": name, "password": PASSWORD, "library_ids": []}))
            .send()
            .await?;
        assert_eq!(response.status(), 201, "{:?}", response.text().await);
        let user: Value = response.json().await?;
        Ok(user["id"].as_str().ok_or("no id")?.to_owned())
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
                "device": {"client_device_id": device, "name": device, "platform": "ios"}}))
            .send()
            .await?;
        assert_eq!(response.status(), 201);
        let body: wire::auth::PairResult = response.json().await?;
        Ok(body.api_key)
    }

    async fn get(&self, key: &str, path: &str) -> Result<(u16, Value), Box<dyn std::error::Error>> {
        let response = self
            .http
            .get(self.url(path))
            .bearer_auth(key)
            .send()
            .await?;
        let status = response.status().as_u16();
        Ok((status, response.json().await.unwrap_or(Value::Null)))
    }

    async fn send(
        &self,
        method: reqwest::Method,
        key: &str,
        path: &str,
        body: &Value,
    ) -> Result<(u16, Value), Box<dyn std::error::Error>> {
        let response = self
            .http
            .request(method, self.url(path))
            .bearer_auth(key)
            .json(body)
            .send()
            .await?;
        let status = response.status().as_u16();
        Ok((status, response.json().await.unwrap_or(Value::Null)))
    }

    async fn post(
        &self,
        key: &str,
        path: &str,
        body: &Value,
    ) -> Result<(u16, Value), Box<dyn std::error::Error>> {
        self.send(reqwest::Method::POST, key, path, body).await
    }
}

const NOW: &str = "2026-11-14T09:00:00.000Z";

fn play(id: &str, device: &str) -> Value {
    json!({"id": id, "client_modified_at": NOW, "device_id": device,
           "fields": {"title": "Halo", "artistName": "Beyoncé", "playedAtMs": 1_794_640_000_000_i64,
                      "kind": "onDemand", "source": "appleMusic", "recordedByDeviceID": device}})
}

fn statuses(body: &Value) -> Vec<String> {
    body["results"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|r| {
                    r["reason"]
                        .as_str()
                        .or(r["status"].as_str())
                        .unwrap_or("?")
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every cloud read, with what an empty answer looks like.
const READS: &[&str] = &[
    "/cloud/plays",
    "/cloud/sessions",
    "/cloud/stations",
    "/cloud/forgotten-songs",
    "/cloud/blocked-artists",
    "/cloud/excluded-stations",
    "/cloud/playlists",
    "/cloud/playlist-entries",
    "/cloud/likes",
    "/cloud/settings",
    "/cloud/devices",
    "/cloud/usage",
];

fn is_empty(path: &str, body: &Value) -> bool {
    match path {
        "/cloud/settings" => body["settings"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty),
        "/cloud/devices" => body["devices"].as_array().is_some_and(Vec::is_empty),
        "/cloud/usage" => body["items"] == 0,
        _ => {
            body["upserts"].as_array().is_some_and(Vec::is_empty)
                && body["deletes"].as_array().is_some_and(Vec::is_empty)
        }
    }
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one walk over every cloud route")]
async fn each_user_sees_and_changes_only_their_own_cloud_data() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    server.add_member(&cookie, "nana").await?;
    let luke = server.device_key("luke", "LUKE-IPHONE").await?;
    let nana = server.device_key("nana", "NANA-IPHONE").await?;

    // Luke (an admin) fills every kind of record.
    let writes = [
        ("/cloud/plays:batch", play("p1", "LUKE-IPHONE")),
        (
            "/cloud/sessions:batch",
            json!({"id": "s1", "client_modified_at": NOW, "device_id": "LUKE-IPHONE", "fields": {"startedAtMs": 1}}),
        ),
        (
            "/cloud/stations:batch",
            json!({"id": "", "client_modified_at": NOW, "device_id": "LUKE-IPHONE", "fields": {"name": "Radio X"}}),
        ),
        (
            "/cloud/forgotten-songs:batch",
            json!({"id": "halo\u{1f}beyonce", "client_modified_at": NOW, "device_id": "LUKE-IPHONE", "fields": {"forgottenAtMs": 1}}),
        ),
        (
            "/cloud/blocked-artists:batch",
            json!({"id": "", "client_modified_at": NOW, "device_id": "LUKE-IPHONE", "fields": {"name": "ABBA"}}),
        ),
        (
            "/cloud/excluded-stations:batch",
            json!({"id": "", "client_modified_at": NOW, "device_id": "LUKE-IPHONE", "fields": {"name": "Talk FM"}}),
        ),
        (
            "/cloud/playlists:batch",
            json!({"id": "pl", "client_modified_at": NOW, "device_id": "LUKE-IPHONE", "fields": {"name": "Drive"}}),
        ),
        (
            "/cloud/playlist-entries:batch",
            json!({"id": "e1", "client_modified_at": NOW, "device_id": "LUKE-IPHONE", "fields": {"playlistID": "pl", "order": "a0"}}),
        ),
        (
            "/cloud/likes:batch",
            json!({"id": "halo\u{1f}beyonce", "client_modified_at": NOW, "device_id": "LUKE-IPHONE", "fields": {"loved": true}}),
        ),
    ];
    for (path, item) in &writes {
        let (status, body) = server.post(&luke, path, &json!({"items": [item]})).await?;
        assert_eq!(status, 200, "{path}: {body}");
        assert_eq!(statuses(&body), vec!["accepted"], "{path}");
    }
    let (status, _) = server
        .send(reqwest::Method::PATCH, &luke, "/cloud/settings",
              &json!({"settings": {"playlistName": {"value": "Luke's", "modified_at": NOW, "device_id": "LUKE-IPHONE"}}}))
        .await?;
    assert_eq!(status, 200);
    let (status, _) = server
        .send(
            reqwest::Method::PUT,
            &luke,
            "/cloud/devices/LUKE-IPHONE",
            &json!({"predecessors": ["OLD"]}),
        )
        .await?;
    assert_eq!(status, 200);

    // Nana sees none of it, on every read.
    for path in READS {
        let (status, body) = server.get(&nana, path).await?;
        assert_eq!(status, 200, "{path}");
        assert!(is_empty(path, &body), "{path}: {body}");
        let (_, mine) = server.get(&luke, path).await?;
        assert!(!is_empty(path, &mine), "{path} for luke: {mine}");
    }
    // Nana's writes with Luke's ids land in Nana's space.
    for (path, item) in &writes {
        let mut item = item.clone();
        item["device_id"] = json!("NANA-IPHONE");
        let (_, body) = server.post(&nana, path, &json!({"items": [item]})).await?;
        assert_eq!(statuses(&body), vec!["accepted"], "{path}");
    }
    let delete = json!({"items": [{"id": "p1", "client_modified_at": NOW, "device_id": "NANA-IPHONE", "deleted_at": NOW}]});
    server.post(&nana, "/cloud/plays:batch", &delete).await?;
    let (_, lukes) = server.get(&luke, "/cloud/plays").await?;
    assert_eq!(lukes["upserts"].as_array().map(Vec::len), Some(1));
    assert_eq!(lukes["deletes"].as_array().map(Vec::len), Some(0));
    let (_, lukes) = server.get(&luke, "/cloud/settings").await?;
    assert_eq!(lukes["settings"]["playlistName"]["value"], "Luke's");

    // The admin's dashboard session reaches no cloud route, and the admin user page shows
    // counts only.
    for path in READS {
        let response = server
            .http
            .get(server.url(path))
            .headers(server.session(&cookie))
            .send()
            .await?;
        assert_eq!(response.status(), 403, "{path}");
    }
    let (_, users) = {
        let response = server
            .http
            .get(server.url("/admin/users"))
            .headers(server.session(&cookie))
            .send()
            .await?;
        (response.status(), response.json::<Value>().await?)
    };
    let nana_id = users["users"]
        .as_array()
        .and_then(|u| u.iter().find(|u| u["username"] == "nana"))
        .and_then(|u| u["id"].as_str())
        .ok_or("no nana")?
        .to_owned();
    let detail: Value = server
        .http
        .get(server.url(&format!("/admin/users/{nana_id}")))
        .headers(server.session(&cookie))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(detail["cloud"]["items"], 9, "{detail}");
    assert_eq!(
        detail["cloud"].as_object().map(serde_json::Map::len),
        Some(2),
        "numbers only"
    );
    let text = detail.to_string();
    assert!(
        !text.contains("Halo") && !text.contains("Radio X"),
        "{text}"
    );

    // Deleting a user deletes their cloud data.
    let response = server
        .http
        .delete(server.url(&format!("/admin/users/{nana_id}")))
        .headers(server.session(&cookie))
        .send()
        .await?;
    assert_eq!(response.status(), 204);
    let left: i64 = server
        .test
        .db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM cloud_items", [], |row| row.get(0))?))
        .await?;
    assert_eq!(left, 10, "only luke's rows: nine records and a setting");
    Ok(())
}

#[tokio::test]
async fn only_device_keys_reach_cloud_routes() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let response = server
        .http
        .post(server.url("/me/api-keys"))
        .headers(server.session(&cookie))
        .json(&json!({"kind": "api_key", "name": "Symfonium", "scopes": ["library", "stream", "write"]}))
        .send()
        .await?;
    assert_eq!(response.status(), 201);
    let key: Value = response.json().await?;
    let api_key = key["secret"].as_str().ok_or("no secret")?;
    let (status, _) = server.get(api_key, "/cloud/plays").await?;
    assert_eq!(status, 403);
    let (status, _) = server
        .post(api_key, "/cloud/plays:batch", &json!({"items": []}))
        .await?;
    assert_eq!(status, 403);
    let anonymous = server.http.get(server.url("/cloud/plays")).send().await?;
    assert_eq!(anonymous.status(), 401);
    Ok(())
}

#[tokio::test]
async fn batches_cursors_and_limits() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key("luke", "IPHONE").await?;
    let items: Vec<Value> = (0..3).map(|n| play(&format!("p{n}"), "IPHONE")).collect();
    let (status, body) = server
        .post(&key, "/cloud/plays:batch", &json!({"items": items}))
        .await?;
    assert_eq!(status, 200);
    assert_eq!(statuses(&body), vec!["accepted"; 3]);
    let (_, replay) = server
        .post(&key, "/cloud/plays:batch", &json!({"items": items}))
        .await?;
    assert_eq!(statuses(&replay), vec!["duplicate"; 3]);

    let (_, first) = server.get(&key, "/cloud/plays?limit=2").await?;
    assert_eq!(first["has_more"], true);
    let cursor = first["cursor"].as_str().ok_or("cursor")?.to_owned();
    let (_, second) = server
        .get(&key, &format!("/cloud/plays?cursor={cursor}&limit=2"))
        .await?;
    assert_eq!(second["has_more"], false);
    assert_eq!(second["upserts"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        second["upserts"][0]["fields"]["songKey"],
        "halo\u{1f}beyonce"
    );

    let (status, body) = server.get(&key, "/cloud/plays?cursor=99999").await?;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (410, Some("sync_reset"))
    );
    let (status, _) = server.get(&key, "/cloud/plays?cursor=abc").await?;
    assert_eq!(status, 400);
    let (status, _) = server.get(&key, "/cloud/stats").await?;
    assert_eq!(status, 404);
    let (status, _) = server
        .post(&key, "/cloud/plays", &json!({"items": []}))
        .await?;
    assert_eq!(status, 404, "a push needs :batch");
    let (status, _) = server
        .post(&key, "/cloud/settings:batch", &json!({"items": []}))
        .await?;
    assert_eq!(status, 404, "settings use PATCH");
    // A full batch may be well over the 1 MiB that other routes allow.
    let big: Vec<Value> = (0..1000)
        .map(|n| {
            let mut item = play(&format!("b{n}"), "IPHONE");
            item["fields"]["note"] = json!("x".repeat(2000));
            item
        })
        .collect();
    let (status, body) = server
        .post(&key, "/cloud/plays:batch", &json!({"items": big}))
        .await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(statuses(&body).len(), 1000);
    let many: Vec<Value> = (0..1001)
        .map(|n| play(&format!("m{n}"), "IPHONE"))
        .collect();
    let (status, _) = server
        .post(&key, "/cloud/plays:batch", &json!({"items": many}))
        .await?;
    assert_eq!(status, 413);
    Ok(())
}

#[tokio::test]
async fn settings_over_http_refuse_a_plaintext_nearby_key() -> TestResult {
    let server = start().await?;
    server.setup_admin().await?;
    let key = server.device_key("luke", "IPHONE").await?;
    let patch = json!({"settings": {
        "nearbyKey": {"value": "q83vEjRWeJAq83vEjRWeJAq83vEjRWeJAq83vEjRWeJA=", "modified_at": NOW, "device_id": "IPHONE"},
        "playlistID": {"value": null, "modified_at": NOW, "device_id": "IPHONE"},
        "autoAddToPlaylist": {"value": true, "modified_at": "1970-01-01T00:00:00.000Z", "device_id": "IPHONE"}
    }});
    let (status, body) = server
        .send(reqwest::Method::PATCH, &key, "/cloud/settings", &patch)
        .await?;
    assert_eq!(status, 200, "{body}");
    let by_key: std::collections::BTreeMap<String, Value> = body["results"]
        .as_array()
        .ok_or("results")?
        .iter()
        .map(|r| (r["key"].as_str().unwrap_or_default().to_owned(), r.clone()))
        .collect();
    assert_eq!(by_key["nearbyKey"]["reason"], "encryption_required");
    assert_eq!(by_key["playlistID"]["status"], "accepted");
    assert_eq!(by_key["playlistID"]["current"]["value"], Value::Null);
    let missing = json!({"settings": {"x": {"modified_at": NOW, "device_id": "IPHONE"}}});
    let (status, _) = server
        .send(reqwest::Method::PATCH, &key, "/cloud/settings", &missing)
        .await?;
    assert_eq!(status, 400, "a value must be sent, null to clear");
    let (_, all) = server.get(&key, "/cloud/settings").await?;
    assert!(all["settings"].get("nearbyKey").is_none());
    assert_eq!(
        all["settings"]["autoAddToPlaylist"]["modified_at"],
        "1970-01-01T00:00:00.000Z"
    );
    Ok(())
}

#[tokio::test]
async fn the_admin_sets_quotas_and_can_turn_cloud_off() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    let key = server.device_key("luke", "IPHONE").await?;
    let caps: Value = server
        .http
        .get(server.url("/capabilities"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(caps["features"]["cloud"], true);
    assert_eq!(caps["cloud"]["max_batch"], 1000);
    let response = server
        .http
        .patch(server.url("/admin/settings"))
        .headers(server.session(&cookie))
        .json(&json!({"cloud.quota_items": 2}))
        .send()
        .await?;
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
    let items: Vec<Value> = (0..3).map(|n| play(&format!("p{n}"), "IPHONE")).collect();
    let (_, body) = server
        .post(&key, "/cloud/plays:batch", &json!({"items": items}))
        .await?;
    assert_eq!(
        statuses(&body),
        vec!["accepted", "accepted", "quota_exceeded"]
    );
    let (_, usage) = server.get(&key, "/cloud/usage").await?;
    assert_eq!(usage["quota"]["items"], 2);
    assert_eq!(usage["entities"]["plays"]["items"], 2);
    let response = server
        .http
        .patch(server.url("/admin/settings"))
        .headers(server.session(&cookie))
        .json(&json!({"cloud.enabled": false}))
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    let (status, _) = server.get(&key, "/cloud/plays").await?;
    assert_eq!(status, 404);
    let caps: Value = server
        .http
        .get(server.url("/capabilities"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(caps["features"]["cloud"], false);
    assert!(caps.get("cloud").is_none());
    Ok(())
}

async fn read_until(
    stream: &mut reqwest::Response,
    seen: &mut String,
    needle: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    while !seen.contains(needle) {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.chunk())
            .await??
            .ok_or("stream ended")?;
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    Ok(())
}

#[tokio::test]
async fn the_cloud_event_says_what_to_pull_and_only_to_its_owner() -> TestResult {
    let server = start().await?;
    let cookie = server.setup_admin().await?;
    server.add_member(&cookie, "nana").await?;
    let luke = server.device_key("luke", "LUKE").await?;
    let nana = server.device_key("nana", "NANA").await?;
    let mut lukes = server
        .http
        .get(server.url("/events"))
        .bearer_auth(&luke)
        .send()
        .await?;
    let mut nanas = server
        .http
        .get(server.url("/events"))
        .bearer_auth(&nana)
        .send()
        .await?;
    let (mut seen_luke, mut seen_nana) = (String::new(), String::new());
    read_until(&mut lukes, &mut seen_luke, "event: hello").await?;
    read_until(&mut nanas, &mut seen_nana, "event: hello").await?;
    server
        .post(
            &luke,
            "/cloud/plays:batch",
            &json!({"items": [play("p1", "LUKE")]}),
        )
        .await?;
    server.post(&nana, "/cloud/likes:batch", &json!({"items": [
        {"id": "a\u{1f}b", "client_modified_at": NOW, "device_id": "NANA", "fields": {"loved": true}}]})).await?;
    read_until(&mut lukes, &mut seen_luke, "event: cloud").await?;
    read_until(&mut nanas, &mut seen_nana, "event: cloud").await?;
    assert!(
        seen_luke.contains(r#"data: {"entity":"plays"}"#),
        "{seen_luke}"
    );
    assert!(
        !seen_luke.contains("likes") && !seen_luke.contains("Halo"),
        "{seen_luke}"
    );
    assert!(
        seen_nana.contains(r#"data: {"entity":"likes"}"#),
        "{seen_nana}"
    );
    assert!(!seen_nana.contains("plays"), "{seen_nana}");
    Ok(())
}
