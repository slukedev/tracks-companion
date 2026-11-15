//! What every scenario starts from: a generated library, a running server, its first admin
//! (set up with the token from the log) and a device key for that admin.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use trackside_e2e::client::device;
use trackside_e2e::library::{self, Library};
use trackside_e2e::{Auth, Client, Server, ServerSpec};

pub const ADMIN: &str = "luke";
pub const PASSWORD: &str = "correct horse battery";

pub struct Env {
    pub dir: tempfile::TempDir,
    pub ffmpeg: PathBuf,
    pub lib: Library,
    pub server: Server,
    pub client: Client,
    pub admin: Auth,
    pub key: Auth,
}

pub fn ffmpeg() -> Option<PathBuf> {
    library::ffmpeg()
}

/// A temporary directory inside `TRACKSIDE_E2E_TMP` (or the system's), for data and music.
pub fn tempdir() -> anyhow::Result<tempfile::TempDir> {
    match std::env::var_os("TRACKSIDE_E2E_TMP") {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            Ok(tempfile::tempdir_in(dir)?)
        }
        None => Ok(tempfile::tempdir()?),
    }
}

pub fn spec(dir: &Path) -> anyhow::Result<ServerSpec> {
    Ok(ServerSpec::new(trackside_e2e::binary()?, dir.join("data")))
}

/// Creates the first admin through `/setup` with the token from the log; returns the
/// session.
pub async fn setup_admin(server: &Server, client: &Client) -> anyhow::Result<Auth> {
    let token = server.setup_token().context("no setup token in the log")?;
    let reply = client
        .post(
            "/setup",
            &Auth::None,
            &json!({"setup_token": token, "username": ADMIN, "display_name": "Luke", "password": PASSWORD}),
        )
        .await?;
    if reply.status != 201 {
        bail!("setup answered {}: {}", reply.status, reply.text());
    }
    Ok(Auth::Session(
        reply.cookie().context("setup set no cookie")?,
    ))
}

/// Signs in from an app and returns the device key.
pub async fn app_login(
    client: &Client,
    username: &str,
    password: &str,
    device_id: &str,
) -> anyhow::Result<(Auth, Value)> {
    let reply = client
        .post(
            "/auth/login",
            &Auth::None,
            &json!({"username": username, "password": password, "device": device(device_id, "Test iPhone")}),
        )
        .await?;
    if reply.status != 201 {
        bail!("login answered {}: {}", reply.status, reply.text());
    }
    let body = reply.json();
    let key = body["api_key"].as_str().context("no api_key")?.to_owned();
    Ok((Auth::Key(key), body))
}

/// The id of the library made from `music_dir` ("Music").
pub async fn library_id(client: &Client, admin: &Auth) -> anyhow::Result<String> {
    let reply = client.get("/admin/libraries", admin).await?;
    reply.json()["libraries"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|l| l["name"] == "Music")
        .and_then(|l| l["id"].as_str())
        .map(str::to_owned)
        .with_context(|| format!("no Music library: {}", reply.text()))
}

/// Waits until the library's newest scan finished and nothing is queued, and returns it.
pub async fn wait_scan(client: &Client, admin: &Auth, library: &str) -> anyhow::Result<Value> {
    let path = format!("/admin/libraries/{library}/scans?limit=5");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let scans = client.get(&path, admin).await?.json();
        let newest = scans["scans"][0].clone();
        let state = newest["state"].as_str().unwrap_or("");
        let overview = client.get("/admin/overview", admin).await?.json();
        let scanning = overview["libraries"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|l| l["scanning"].as_bool() == Some(true));
        if !state.is_empty() && state != "running" && !scanning {
            return Ok(newest);
        }
        if std::time::Instant::now() > deadline {
            bail!("scan did not finish: {scans}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Every song a key's sync snapshot holds, by title.
pub async fn snapshot_songs(client: &Client, key: &Auth) -> anyhow::Result<(Vec<Value>, String)> {
    let mut songs = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..1000 {
        let path = match &cursor {
            Some(c) => format!("/sync?limit=500&cursor={c}"),
            None => String::from("/sync?limit=500"),
        };
        let reply = client.get(&path, key).await?;
        if reply.status != 200 {
            bail!("sync answered {}: {}", reply.status, reply.text());
        }
        let page = reply.json();
        if let Some(items) = page["songs"].as_array() {
            songs.extend(items.iter().cloned());
        }
        let next = page["next_cursor"].as_str().map(str::to_owned);
        if page["complete"].as_bool() == Some(true) {
            return Ok((songs, next.context("no delta cursor")?));
        }
        cursor = Some(next.context("snapshot page without next_cursor")?);
    }
    bail!("snapshot never completed")
}

pub fn titles(songs: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = songs
        .iter()
        .filter_map(|s| s["title"].as_str().map(str::to_owned))
        .collect();
    out.sort();
    out
}

/// A server on a fresh small library, its admin set up, the first scan done.
pub async fn boot(configure: impl FnOnce(ServerSpec) -> ServerSpec) -> anyhow::Result<Option<Env>> {
    let Some(ffmpeg) = ffmpeg() else {
        return Ok(None);
    };
    let dir = tempdir()?;
    let music = dir.path().join("music");
    let lib = library::small_library(&ffmpeg, &music)?;
    let spec = configure(spec(dir.path())?.env("TRACKSIDE_MUSIC_DIR", music.display().to_string()));
    let server = Server::start(spec)?;
    let client = server.client();
    let admin = setup_admin(&server, &client).await?;
    let library = library_id(&client, &admin).await?;
    wait_scan(&client, &admin, &library).await?;
    let (key, _) = app_login(&client, ADMIN, PASSWORD, "E2E-IPHONE").await?;
    Ok(Some(Env {
        dir,
        ffmpeg,
        lib,
        server,
        client,
        admin,
        key,
    }))
}

impl Env {
    pub fn music(&self) -> &Path {
        &self.lib.root
    }

    pub async fn library_id(&self) -> anyhow::Result<String> {
        library_id(&self.client, &self.admin).await
    }

    /// Runs a scan of `kind` and waits for it.
    pub async fn scan(&self, kind: &str) -> anyhow::Result<Value> {
        let id = self.library_id().await?;
        let reply = self
            .client
            .post(
                &format!("/admin/libraries/{id}/scan"),
                &self.admin,
                &json!({"kind": kind}),
            )
            .await?;
        if reply.status != 202 {
            bail!("scan answered {}: {}", reply.status, reply.text());
        }
        wait_scan(&self.client, &self.admin, &id).await
    }

    /// A song's id by title, from search.
    pub async fn song(&self, title: &str) -> anyhow::Result<Value> {
        let (songs, _) = snapshot_songs(&self.client, &self.key).await?;
        songs
            .into_iter()
            .find(|s| s["title"].as_str() == Some(title))
            .with_context(|| format!("no song titled {title}"))
    }
}
