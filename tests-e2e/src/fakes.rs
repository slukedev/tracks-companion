//! Stand-ins for the acquisition providers' outside world: a yt-dlp script that serves a
//! local file, and an slskd API on a loopback port. Nothing here touches the network.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use axum::Json;
use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use serde_json::{Value, json};

/// A fake `yt-dlp`: `--version` answers a recent version, a search (`-J`) answers one video
/// titled `<artist> - <title>`, and a download copies `source` into the `-P` directory as
/// `audio.<ext>` while printing progress lines. With a file named `hang` next to the script,
/// downloads wait until it is removed; each run appends its arguments to `calls`.
#[derive(Clone, Debug)]
pub struct FakeYtdlp {
    pub dir: PathBuf,
    pub script: PathBuf,
}

impl FakeYtdlp {
    pub fn write(
        dir: &Path,
        source: &Path,
        artist: &str,
        title: &str,
        seconds: f32,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let ext = source.extension().and_then(|e| e.to_str()).unwrap_or("m4a");
        let search = json!({
            "entries": [{
                "id": "e2eVideo001",
                "title": format!("{artist} - {title} (Official Audio)"),
                "uploader": artist,
                "duration": seconds,
            }]
        })
        .to_string();
        let script = dir.join("yt-dlp");
        let body = format!(
            r#"#!/bin/sh
dir='{dir}'
echo "$*" >> "$dir/calls"
for a in "$@"; do
  if [ "$a" = "--version" ]; then echo 2026.11.03; exit 0; fi
done
case " $* " in
  *" -J "*) cat <<'JSON'
{search}
JSON
  exit 0 ;;
  *" --skip-download "*) exit 0 ;;
  *" -o - "*) cat '{source}'; exit 0 ;;
esac
out=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-P" ]; then out="$a"; fi
  prev="$a"
done
[ -n "$out" ] || {{ echo "ERROR: no -P" >&2; exit 2; }}
size=$(wc -c < '{source}' | tr -d ' ')
echo "{{\"downloaded_bytes\": 0, \"total_bytes\": $size}}"
while [ -e "$dir/hang" ]; do sleep 0.1; done
mkdir -p "$out"
cp '{source}' "$out/audio.{ext}"
echo "{{\"downloaded_bytes\": $size, \"total_bytes\": $size}}"
"#,
            dir = dir.display(),
            source = source.display(),
        );
        std::fs::write(&script, body)?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            script,
        })
    }

    /// Makes downloads wait until [`FakeYtdlp::release`].
    pub fn hold(&self) -> std::io::Result<()> {
        std::fs::write(self.dir.join("hang"), b"")
    }

    pub fn release(&self) {
        let _ = std::fs::remove_file(self.dir.join("hang"));
    }

    /// The argument lists it was run with.
    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[derive(Debug)]
struct SlskdState {
    key: String,
    downloads: PathBuf,
    source: PathBuf,
    remote_name: String,
    polls: u32,
    log: Vec<String>,
}

type Shared = Arc<Mutex<SlskdState>>;

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, SlskdState> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A fake slskd (API v0) that finds one FLAC from one peer and "downloads" it by copying
/// `source` into its downloads directory after a couple of polls.
#[derive(Clone, Debug)]
pub struct FakeSlskd {
    pub url: String,
    pub key: String,
    pub downloads: PathBuf,
    state: Shared,
}

impl FakeSlskd {
    pub async fn start(
        downloads: &Path,
        source: &Path,
        artist: &str,
        album: &str,
        title: &str,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(downloads)?;
        let key = String::from("e2e-slskd-key-0123456789");
        let remote_name = format!("@@music\\{artist}\\{album}\\01 - {title}.flac");
        let state: Shared = Arc::new(Mutex::new(SlskdState {
            key: key.clone(),
            downloads: downloads.to_path_buf(),
            source: source.to_path_buf(),
            remote_name,
            polls: 0,
            log: Vec::new(),
        }));
        let app = Router::new()
            .route("/api/v0/application", get(application))
            .route("/api/v0/searches", post(search_create))
            .route(
                "/api/v0/searches/{id}",
                get(search_state).delete(search_delete),
            )
            .route("/api/v0/searches/{id}/responses", get(search_responses))
            .route(
                "/api/v0/transfers/downloads/{username}",
                post(enqueue).get(transfers),
            )
            .route(
                "/api/v0/transfers/downloads/{username}/{id}",
                axum::routing::delete(cancel),
            )
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            url: format!("http://{addr}"),
            key,
            downloads: downloads.to_path_buf(),
            state,
        })
    }

    pub fn log(&self) -> Vec<String> {
        lock(&self.state).log.clone()
    }
}

fn authorised(headers: &HeaderMap, s: &Shared) -> bool {
    headers.get("x-api-key").and_then(|v| v.to_str().ok()) == Some(lock(s).key.as_str())
}

async fn application(
    State(s): State<Shared>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorised(&headers, &s) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(json!({
        "version": {"full": "0.24.1.0", "current": "0.24.1"},
        "server": {"state": "Connected, LoggedIn", "isConnected": true, "isLoggedIn": true}
    })))
}

async fn search_create(
    State(s): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if !authorised(&headers, &s) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    lock(&s).log.push(format!(
        "search {}",
        body["searchText"].as_str().unwrap_or_default()
    ));
    Ok(Json(json!({"id": body["id"], "isComplete": false})))
}

async fn search_state() -> Json<Value> {
    Json(json!({"isComplete": true, "responseCount": 1}))
}

async fn search_delete() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn search_responses(State(s): State<Shared>) -> Json<Value> {
    let state = lock(&s);
    let size = std::fs::metadata(&state.source).map_or(0, |m| m.len());
    Json(json!([{
        "username": "e2e peer",
        "hasFreeUploadSlot": true,
        "queueLength": 0,
        "uploadSpeed": 5_000_000,
        "fileCount": 1,
        "files": [{
            "filename": state.remote_name,
            "size": size,
            "bitRate": 900,
            "sampleRate": 44100,
            "bitDepth": 16,
            "length": 2,
            "isLocked": false
        }]
    }]))
}

async fn enqueue(
    State(s): State<Shared>,
    UrlPath(user): UrlPath<String>,
    Json(body): Json<Value>,
) -> StatusCode {
    lock(&s).log.push(format!(
        "enqueue {user} {}",
        body[0]["filename"].as_str().unwrap_or_default()
    ));
    StatusCode::CREATED
}

async fn transfers(State(s): State<Shared>, UrlPath(user): UrlPath<String>) -> Json<Value> {
    let mut state = lock(&s);
    state.polls += 1;
    let size = std::fs::metadata(&state.source).map_or(0, |m| m.len());
    let (label, bytes) = if state.polls < 2 {
        ("InProgress", size / 2)
    } else {
        let name = state
            .remote_name
            .rsplit('\\')
            .next()
            .unwrap_or("song.flac")
            .to_owned();
        let album = state
            .remote_name
            .rsplit('\\')
            .nth(1)
            .unwrap_or("album")
            .to_owned();
        let dir = state.downloads.join(album);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::copy(&state.source, dir.join(name));
        ("Completed, Succeeded", size)
    };
    Json(json!({
        "username": user,
        "directories": [{"directory": "x", "fileCount": 1, "files": [{
            "id": "1b2c3d4e-0000-0000-0000-000000000000",
            "username": user,
            "filename": state.remote_name,
            "size": size,
            "state": label,
            "bytesTransferred": bytes,
            "requestedAt": "2026-11-14T09:30:00"
        }]}]
    }))
}

async fn cancel(
    State(s): State<Shared>,
    UrlPath((user, id)): UrlPath<(String, String)>,
) -> StatusCode {
    lock(&s).log.push(format!("cancel {user} {id}"));
    StatusCode::NO_CONTENT
}
