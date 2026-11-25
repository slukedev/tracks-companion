//! Running the server: backups and restore, shutdown and crashes, a library that goes away,
//! a full data disk, upgrades and downgrades, and life behind a reverse proxy.

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{
    ADMIN, PASSWORD, app_login, boot, library_id, setup_admin, snapshot_songs, wait_scan,
};
use serde_json::json;
use trackside_e2e::library::{Format, Track, write_track};
use trackside_e2e::{Auth, Server, ServerSpec};

/// Runs a `trackside` subcommand against a data directory.
fn cli(spec: &ServerSpec, args: &[&str]) -> anyhow::Result<std::process::Output> {
    let mut command = Command::new(&spec.bin);
    command.arg("--data-dir").arg(&spec.data_dir).args(args);
    for (key, _) in std::env::vars() {
        if key.starts_with("TRACKSIDE_") {
            command.env_remove(key);
        }
    }
    Ok(command.output()?)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backup_restores_with_the_cli_once_the_server_has_stopped() -> anyhow::Result<()> {
    let Some(mut env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = env.client.clone();
    let halo = env.song("Halo").await?;
    let halo_id = halo["id"].as_str().unwrap_or_default().to_owned();
    assert_eq!(
        c.put(&format!("/me/stars/song/{halo_id}"), &env.key, &json!({}))
            .await?
            .status,
        204
    );
    let started = c.post("/admin/backups", &env.admin, &json!({})).await?;
    assert_eq!(started.status, 202, "{}", started.text());
    let listed = c
        .poll("/admin/backups", &env.admin, Duration::from_secs(30), |r| {
            r.json()["backups"]
                .as_array()
                .is_some_and(|b| !b.is_empty())
        })
        .await?
        .json();
    let name = listed["backups"][0]["name"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert_eq!(listed["backups"][0]["kind"], "manual", "{listed}");
    assert_eq!(listed["backups"][0]["has_key"], true);
    let download = c.get(&format!("/admin/backups/{name}"), &env.admin).await?;
    assert_eq!(download.status, 200);
    assert!(download.body.starts_with(b"SQLite format 3"));

    let unstar = c
        .delete(&format!("/me/stars/song/{halo_id}"), &env.key)
        .await?;
    assert_eq!(unstar.status, 204);
    let file = env.server.data_dir().join("backups").join(&name);
    let file_arg = file.display().to_string();
    let refused = cli(&env.server.spec, &["backup", "restore", &file_arg])?;
    assert!(
        !refused.status.success(),
        "restore must refuse while the server runs"
    );

    assert!(
        env.server.stop()?.success(),
        "a clean stop: {}",
        env.server.log_text()
    );
    let list = cli(&env.server.spec, &["backup", "list"])?;
    assert!(String::from_utf8_lossy(&list.stdout).contains(&name));
    let restored = cli(&env.server.spec, &["backup", "restore", &file_arg])?;
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    let aside = std::fs::read_dir(env.server.data_dir())?
        .flatten()
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("trackside.db.before-restore-")
        });
    assert!(aside, "the replaced database is kept");

    env.server.restart()?;
    let c = env.server.client();
    let (key, _) = app_login(&c, ADMIN, PASSWORD, "AFTER-RESTORE").await?;
    let stars = c.get("/me/stars?kind=song", &key).await?.json();
    assert_eq!(
        stars["items"][0]["id"],
        halo_id.as_str(),
        "the star made before the backup is back: {stars}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_lets_an_open_stream_finish_and_then_refuses_connections() -> anyhow::Result<()> {
    let Some(mut env) = boot(|s| s.env("TRACKSIDE_SERVER__SHUTDOWN_GRACE", "10s")).await? else {
        return Ok(());
    };
    let mut long = Track::new("Long/01 Long.wav", "Long Song", "Long", "Long");
    long.format = Format::Wav;
    long.seconds = 60.0;
    write_track(&env.ffmpeg, env.music(), &long)?;
    env.scan("quick").await?;
    let song = env.song("Long Song").await?;
    let url = env.client.api(&format!(
        "/songs/{}/stream",
        song["id"].as_str().unwrap_or_default()
    ));
    let expected = std::fs::metadata(env.music().join(&long.rel))?.len() as usize;
    let mut response = env
        .client
        .request(reqwest::Method::GET, &url, &env.key)
        .send()
        .await?;
    let mut got = response.chunk().await?.map_or(0, |b| b.len());
    let pid = env.server.pid().unwrap_or_default().to_string();
    Command::new("kill").args(["-TERM", &pid]).status()?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let refused = env.client.http.get(env.client.api("/ping")).send().await;
    assert!(
        refused.is_err() || refused.is_ok_and(|r| r.status() != 200),
        "no new requests while stopping"
    );
    while let Some(chunk) = response.chunk().await? {
        got += chunk.len();
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(got, expected, "the open stream finished");
    let deadline = Instant::now() + Duration::from_secs(20);
    while env.server.running() {
        assert!(Instant::now() < deadline, "the server did not exit");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        env.server.log_text().contains("stopped"),
        "{}",
        env.server.log_text()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kill_during_a_scan_is_recovered_at_the_next_start() -> anyhow::Result<()> {
    let Some(ffmpeg) = common::ffmpeg() else {
        return Ok(());
    };
    let dir = common::tempdir()?;
    let music = dir.path().join("music");
    let synth = trackside_e2e::perf::synth_library(&ffmpeg, &music, 3000, 11)?;
    let mut server = Server::start(common::spec(dir.path())?)?;
    let c = server.client();
    let admin = setup_admin(&server, &c).await?;
    let created = c
        .post(
            "/admin/libraries",
            &admin,
            &json!({"name": "Music", "root": music.display().to_string()}),
        )
        .await?;
    assert_eq!(created.status, 201);
    let library = created.json()["id"].as_str().unwrap_or_default().to_owned();
    c.poll(
        &format!("/admin/libraries/{library}/scans"),
        &admin,
        Duration::from_secs(30),
        |r| r.json()["scans"][0]["files_seen"].as_u64().unwrap_or(0) > 200,
    )
    .await?;
    server.kill();
    server.restart()?;
    let c = server.client();
    let admin = Auth::Session(
        c.post(
            "/auth/session",
            &Auth::None,
            &json!({"username": ADMIN, "password": PASSWORD}),
        )
        .await?
        .cookie()
        .unwrap_or_default(),
    );
    let scans = c
        .get(
            &format!("/admin/libraries/{library}/scans?limit=10"),
            &admin,
        )
        .await?
        .json();
    let states: Vec<&str> = scans["scans"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["state"].as_str())
        .collect();
    assert!(
        states.contains(&"aborted"),
        "the killed run is marked aborted: {scans}"
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        wait_scan(&c, &admin, &library).await?;
        let (key, _) = app_login(&c, ADMIN, PASSWORD, "KILL").await?;
        let (songs, _) = snapshot_songs(&c, &key).await?;
        if songs.len() == synth.songs {
            let mut ids: Vec<&str> = songs.iter().filter_map(|s| s["id"].as_str()).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), synth.songs, "no song twice");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{} of {} songs after the restart",
            songs.len(),
            synth.songs
        );
        let _ = c
            .post(
                &format!("/admin/libraries/{library}/scan"),
                &admin,
                &json!({"kind": "quick"}),
            )
            .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_library_that_disappears_keeps_its_songs_and_comes_back() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let (before, _) = snapshot_songs(c, &env.key).await?;
    let halo = env.song("Halo").await?;
    let away = env.dir.path().join("unmounted");
    std::fs::rename(env.music(), &away)?;
    std::fs::create_dir(env.music())?;
    let library = env.library_id().await?;
    let _ = c
        .post(
            &format!("/admin/libraries/{library}/scan"),
            &env.admin,
            &json!({"kind": "full"}),
        )
        .await?;
    wait_scan(c, &env.admin, &library).await?;
    let libraries = c.get("/libraries", &env.key).await?.json();
    assert_eq!(
        libraries["libraries"][0]["state"], "unavailable",
        "{libraries}"
    );
    let (during, _) = snapshot_songs(c, &env.key).await?;
    assert_eq!(
        during.len(),
        before.len(),
        "nothing is deleted while the library is away"
    );
    assert!(
        during.iter().all(|s| s["missing"] != true),
        "nothing is marked missing"
    );
    let stream = c
        .get(
            &format!("/songs/{}/stream", halo["id"].as_str().unwrap_or_default()),
            &env.key,
        )
        .await?;
    assert_eq!(stream.status, 503);
    assert_eq!(stream.code(), "library_unavailable");
    assert_eq!(stream.header("retry-after"), Some("30"));
    let overview = c.get("/admin/overview", &env.admin).await?.json();
    let problem = overview["problems"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["code"] == "library_unavailable")
        .cloned();
    let problem = problem.ok_or_else(|| anyhow::anyhow!("no problem shown: {overview}"))?;
    assert!(
        problem["title"]
            .as_str()
            .is_some_and(|t| t.split_whitespace().count() <= 6)
    );

    std::fs::remove_dir(env.music())?;
    std::fs::rename(&away, env.music())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let _ = c
            .post(
                &format!("/admin/libraries/{library}/scan"),
                &env.admin,
                &json!({"kind": "quick"}),
            )
            .await?;
        wait_scan(c, &env.admin, &library).await?;
        let state = c.get("/libraries", &env.key).await?.json()["libraries"][0]["state"].clone();
        if state == "ok" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the library never came back: {state}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let again = c
        .get(
            &format!("/songs/{}/stream", halo["id"].as_str().unwrap_or_default()),
            &env.key,
        )
        .await?;
    assert_eq!(again.status, 200);
    let (after, _) = snapshot_songs(c, &env.key).await?;
    assert_eq!(after.len(), before.len());
    Ok(())
}

/// Mounts a small tmpfs, when this host allows it.
struct Tmpfs(std::path::PathBuf);

impl Tmpfs {
    fn mount(path: &Path, size: &str) -> Option<Self> {
        std::fs::create_dir_all(path).ok()?;
        let ok = Command::new("mount")
            .args(["-t", "tmpfs", "-o", &format!("size={size}"), "tmpfs"])
            .arg(path)
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            Some(Self(path.to_path_buf()))
        } else {
            eprintln!("skipping: cannot mount a tmpfs here");
            None
        }
    }
}

impl Drop for Tmpfs {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg("-l").arg(&self.0).status();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_data_disk_refuses_writes_and_keeps_serving_reads() -> anyhow::Result<()> {
    let Some(ffmpeg) = common::ffmpeg() else {
        return Ok(());
    };
    let dir = common::tempdir()?;
    let Some(volume) = Tmpfs::mount(&dir.path().join("data"), "48m") else {
        return Ok(());
    };
    let music = dir.path().join("music");
    let lib = trackside_e2e::library::small_library(&ffmpeg, &music)?;
    let spec = ServerSpec::new(trackside_e2e::binary()?, volume.0.join("trackside"))
        .env("TRACKSIDE_MUSIC_DIR", music.display().to_string());
    let mut server = Server::start(spec)?;
    let c = server.client();
    let admin = setup_admin(&server, &c).await?;
    wait_scan(&c, &admin, &library_id(&c, &admin).await?).await?;
    let (key, _) = app_login(&c, ADMIN, PASSWORD, "FULL").await?;
    let (songs, _) = snapshot_songs(&c, &key).await?;
    assert_eq!(songs.len(), lib.tracks.len() + 1);

    let filler = volume.0.join("filler");
    let _ = Command::new("dd")
        .args(["if=/dev/zero", "bs=1M", "count=64"])
        .arg(format!("of={}", filler.display()))
        .output()?;
    let id = songs[0]["id"].clone();
    let mut refused = None;
    for round in 0..20 {
        let listens: Vec<serde_json::Value> = (0..1000)
            .map(|i| json!({"client_listen_id": format!("FULL-{round}-{i}"), "song_id": id,
                "started_at": trackside_e2e::rfc3339(trackside_e2e::now_ms() - 1000 * i64::from(i)),
                "played_ms": 1000, "title": format!("{} {i}", "t".repeat(150)), "artist": "a".repeat(100), "duration_ms": 1000}))
            .collect();
        let reply = c
            .post("/listens", &key, &json!({"client_time": trackside_e2e::rfc3339(trackside_e2e::now_ms()), "listens": listens}))
            .await?;
        assert_ne!(
            reply.status,
            500,
            "a full disk is never a 500: {}",
            reply.text()
        );
        if reply.status == 507 {
            refused = Some(reply);
            break;
        }
        let accepted = reply.json()["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["status"] == "accepted")
            .count();
        assert!(
            reply.status == 200 && accepted > 0,
            "round {round}: {} {}",
            reply.status,
            &reply.text()[..reply.text().len().min(300)]
        );
    }
    let refused = refused.ok_or_else(|| anyhow::anyhow!("writes never failed on a full disk"))?;
    assert_eq!(refused.code(), "insufficient_storage");
    let read = c
        .get(&format!("/songs/{}", id.as_str().unwrap_or_default()), &key)
        .await?;
    assert_eq!(read.status, 200, "reads continue");
    let stream = c
        .get(
            &format!("/songs/{}/stream", id.as_str().unwrap_or_default()),
            &key,
        )
        .await?;
    assert_eq!(stream.status, 200, "streams continue");
    std::fs::remove_file(&filler)?;
    let retry = c
        .post("/listens", &key, &json!({"client_time": trackside_e2e::rfc3339(trackside_e2e::now_ms()), "listens": [
            {"client_listen_id": "AFTER", "song_id": id, "started_at": trackside_e2e::rfc3339(trackside_e2e::now_ms()),
             "played_ms": 1000, "title": "t", "artist": "a", "duration_ms": 1000}]}))
        .await?;
    assert_eq!(
        retry.status,
        200,
        "writes work again once space returns: {}",
        retry.text()
    );
    assert!(server.stop()?.success());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_database_is_backed_up_and_migrated_and_a_newer_one_refused() -> anyhow::Result<()>
{
    let Some(mut env) = boot(|s| s).await? else {
        return Ok(());
    };
    let (before, _) = snapshot_songs(&env.client, &env.key).await?;
    assert!(env.server.stop()?.success());
    let db_path = env.server.data_dir().join("trackside.db");
    {
        let db = rusqlite::Connection::open(&db_path)?;
        db.execute_batch(
            "DROP TABLE external_art; DROP INDEX listens_user_id;
             DROP INDEX similar_songs_target;
             DROP TABLE cloud_devices; DROP TABLE cloud_entities; DROP TABLE cloud_items;
             DROP TABLE cloud_meta; DROP TABLE cloud_state;
             DROP TABLE enrichment_fetches; DROP TABLE enrichment_artists;
             DROP TABLE enrichment_top_songs; DROP TABLE enrichment_song_names;
             DROP TABLE stats_hourly; DROP TABLE stats_daily;
             ALTER TABLE sessions DROP COLUMN recent_auth_at;
             ALTER TABLE invites DROP COLUMN can_download;
             ALTER TABLE invites DROP COLUMN legacy_subsonic;
             ALTER TABLE invites DROP COLUMN max_bitrate_kbps;
             PRAGMA user_version = 1;",
        )?;
    }
    env.server.restart()?;
    let backup = std::fs::read_dir(env.server.data_dir().join("backups"))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("pre-migrate-v1-v5-"));
    assert!(
        backup.is_some(),
        "no pre-migration backup: {}",
        env.server.log_text()
    );
    let c = env.server.client();
    let (key, _) = app_login(&c, ADMIN, PASSWORD, "UPGRADED").await?;
    let (after, _) = snapshot_songs(&c, &key).await?;
    assert_eq!(
        after.len(),
        before.len(),
        "the library survives the upgrade"
    );
    assert_eq!(
        c.get("/admin/stats/streams", &Auth::None).await?.status,
        401
    );

    assert!(env.server.stop()?.success());
    rusqlite::Connection::open(&db_path)?.execute_batch("PRAGMA user_version = 99;")?;
    let refused = Server::start(env.server.spec.clone());
    let message = match refused {
        Ok(mut s) => {
            let text = s.log_text();
            s.kill();
            anyhow::bail!("a newer database was opened: {text}");
        }
        Err(e) => format!("{e:#}"),
    };
    assert!(message.contains("newer"), "{message}");
    assert!(
        message.contains("backup restore"),
        "the refusal says how to go back: {message}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn behind_a_proxy_the_base_path_and_forwarded_headers_from_trusted_peers_apply()
-> anyhow::Result<()> {
    let Some(env) = boot(|s| {
        s.env("TRACKSIDE_SERVER__BASE_PATH", "/music")
            .env("TRACKSIDE_SERVER__TRUSTED_PROXIES", "127.0.0.1/32")
            .env("TRACKSIDE_AUTH__LOGIN_RATE", "3/1m")
    })
    .await?
    else {
        return Ok(());
    };
    let c = &env.client;
    assert_eq!(c.base_path, "/music");
    let ping = c
        .http
        .get(format!("{}/music/api/v1/ping", c.origin))
        .send()
        .await?;
    assert_eq!(ping.status(), 200);
    let bare = c
        .http
        .get(format!("{}/api/v1/ping", c.origin))
        .send()
        .await?;
    assert_ne!(bare.status(), 200, "the API lives under the base path only");
    let health = c.http.get(format!("{}/healthz", c.origin)).send().await?;
    assert_eq!(
        health.status(),
        200,
        "health checks stay at the root for the proxy"
    );

    let halo = env.song("Halo").await?;
    let decision = c
        .post("/playback/decisions", &env.key, &json!({"song_id": halo["id"], "network": "cellular",
            "client": {"direct": [{"container": "mp3", "codecs": ["mp3"]}], "hls": true, "max_bitrate_kbps": 256}}))
        .await?
        .json();
    let url = decision["url"].as_str().unwrap_or_default();
    let playlist = c.get(url, &env.key).await?;
    assert_eq!(
        playlist.status,
        200,
        "{url} resolves under the base path: {}",
        playlist.text()
    );

    let login = |ip: &'static str, user: &'static str| {
        let c = c.clone();
        async move {
            let reply = c
                .http
                .post(c.api("/auth/session"))
                .header("x-forwarded-for", ip)
                .header("x-forwarded-proto", "https")
                .json(&json!({"username": user, "password": "wrong password!"}))
                .send()
                .await?;
            Ok::<u16, anyhow::Error>(reply.status().as_u16())
        }
    };
    let mut limited = false;
    for n in 0..12 {
        let user = [
            "ann", "ben", "cat", "dan", "eve", "fay", "gus", "hal", "ivy", "jo", "kim", "lee",
        ][n];
        if login("203.0.113.7", user).await? == 429 {
            limited = true;
            break;
        }
    }
    assert!(limited, "one forwarded address is rate limited");
    assert_eq!(
        login("203.0.113.8", "mo").await?,
        401,
        "another forwarded address is not"
    );

    let session = c
        .http
        .post(c.api("/auth/session"))
        .header("x-forwarded-for", "198.51.100.9")
        .header("x-forwarded-proto", "https")
        .json(&json!({"username": ADMIN, "password": PASSWORD}))
        .send()
        .await?;
    assert_eq!(session.status(), 200);
    let cookie = session
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        cookie.contains("Secure"),
        "behind an HTTPS proxy the cookie is Secure: {cookie}"
    );
    Ok(())
}
