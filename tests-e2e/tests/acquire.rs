//! Acquisition end to end with a fake yt-dlp: search, request, import, events, joining an
//! existing song, a job that survives a crash, and settings left over from an older version.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{Env, boot};
use serde_json::{Value, json};
use trackside_e2e::fakes::FakeYtdlp;
use trackside_e2e::library::{Format, Track, write_track};
use trackside_e2e::{Auth, Client};

const ARTIST: &str = "E2E Artist";
const TITLE: &str = "Requested Song";

/// A 3 s file for the fakes to hand over.
fn source(
    env_dir: &std::path::Path,
    ffmpeg: &std::path::Path,
    format: Format,
) -> anyhow::Result<PathBuf> {
    let mut t = Track::new(format!("source.{}", format.ext()), "x", "y", "z");
    t.format = format;
    t.seconds = 3.0;
    t.freq = 610;
    write_track(ffmpeg, &env_dir.join("sources"), &t)
}

/// Creates the writable library acquisitions land in and makes it the target.
async fn acquire_target(env: &Env) -> anyhow::Result<String> {
    let root = env.dir.path().join("acquired");
    std::fs::create_dir_all(&root)?;
    let created = env
        .client
        .post(
            "/admin/libraries",
            &env.admin,
            &json!({"name": "Acquired", "root": root.display().to_string(), "writable": true}),
        )
        .await?;
    anyhow::ensure!(created.status == 201, "{}", created.text());
    let id = created.json()["id"].as_str().unwrap_or_default().to_owned();
    let set = env
        .client
        .put(
            "/admin/settings/targets",
            &env.admin,
            &json!({"acquire_library": id}),
        )
        .await?;
    anyhow::ensure!(set.status < 300, "{}", set.text());
    Ok(id)
}

async fn wait_done(c: &Client, key: &Auth, id: &str, timeout: Duration) -> anyhow::Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let a = c.get(&format!("/acquisitions/{id}"), key).await?.json();
        match a["state"].as_str() {
            Some("done") => return Ok(a),
            Some("failed" | "cancelled") => anyhow::bail!("acquisition ended badly: {a}"),
            _ => {}
        }
        anyhow::ensure!(Instant::now() < deadline, "not done in time: {a}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn request(
    c: &Client,
    key: &Auth,
    candidate: Option<&str>,
) -> anyhow::Result<trackside_e2e::Reply> {
    let mut body = json!({"title": TITLE, "artist": ARTIST, "duration_ms": 3000});
    if let Some(token) = candidate {
        body["candidate"] = json!(token);
    }
    c.post("/acquisitions", key, &body).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_through_ytdlp_is_imported_streamed_and_joined_later() -> anyhow::Result<()> {
    let Some(ffmpeg) = common::ffmpeg() else {
        return Ok(());
    };
    let tmp = common::tempdir()?;
    let src = source(tmp.path(), &ffmpeg, Format::M4a)?;
    let fake = FakeYtdlp::write(&tmp.path().join("bin"), &src, ARTIST, TITLE, 3.0)?;
    let script = fake.script.display().to_string();
    let Some(env) = boot(|s| {
        s.env("TRACKSIDE_ACQUIRE__ENABLED", "true")
            .env("TRACKSIDE_ACQUIRE__YTDLP__ENABLED", "true")
            .env("TRACKSIDE_ACQUIRE__YTDLP__PATH", script.clone())
    })
    .await?
    else {
        return Ok(());
    };
    let c = &env.client;
    acquire_target(&env).await?;
    let me = c.get("/me", &env.key).await?.json();
    assert!(me["features"]["acquisition"].is_object(), "{me}");
    let caps = c.get("/capabilities", &Auth::None).await?.text();
    assert!(
        !caps.contains("acqui"),
        "capabilities never mention acquisition"
    );

    let found = c
        .post(
            "/acquisitions/search",
            &env.key,
            &json!({"title": TITLE, "artist": ARTIST, "duration_ms": 3000}),
        )
        .await?;
    assert_eq!(found.status, 200, "{}", found.text());
    let found = found.json();
    assert!(
        !found.to_string().contains("ytdlp"),
        "no provider names outside admin: {found}"
    );
    let token = found["candidates"][0]["token"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(!token.is_empty(), "{found}");

    let mut events = c
        .request(reqwest::Method::GET, &c.api("/events"), &env.key)
        .send()
        .await?;
    let started = request(c, &env.key, Some(&token)).await?;
    assert_eq!(started.status, 202, "{}", started.text());
    let id = started.json()["id"].as_str().unwrap_or_default().to_owned();
    let done = wait_done(c, &env.key, &id, Duration::from_secs(60)).await?;
    let song_id = done["song_id"].as_str().unwrap_or_default().to_owned();
    assert_eq!(done["existing"], false);
    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen.contains("event: acquisition") && Instant::now() < deadline {
        if let Ok(Ok(Some(chunk))) =
            tokio::time::timeout(Duration::from_millis(500), events.chunk()).await
        {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
    assert!(
        seen.contains("event: acquisition"),
        "SSE carries acquisition events: {seen}"
    );
    drop(events);

    let song = c.get(&format!("/songs/{song_id}"), &env.key).await?;
    assert_eq!(song.status, 200);
    assert_eq!(song.json()["title"], TITLE);
    let stream = c.get(&format!("/songs/{song_id}/stream"), &env.key).await?;
    assert_eq!(stream.status, 200);
    assert!(
        fake.calls().iter().any(|c| c.contains("-J")),
        "it searched with the fake"
    );

    let again = request(c, &env.key, None).await?;
    assert_eq!(again.status, 200, "{}", again.text());
    assert_eq!(again.json()["existing"], true);
    assert_eq!(again.json()["song_id"], song_id.as_str());

    let admin_view = c.get("/admin/acquisitions", &env.admin).await?.json();
    let entry = admin_view["acquisitions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["existing"] == false)
        .cloned()
        .unwrap_or_default();
    assert_eq!(entry["provider"], "ytdlp", "{admin_view}");
    assert!(entry["candidate"]["quality"].is_string(), "{entry}");
    let requests = c
        .get("/admin/stats/requests?days=7&tz=UTC", &env.admin)
        .await?
        .json();
    assert_eq!(requests["added"][6].as_u64(), Some(2), "{requests}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn soulseek_settings_from_an_older_version_are_ignored_and_ytdlp_still_acquires()
-> anyhow::Result<()> {
    let Some(ffmpeg) = common::ffmpeg() else {
        return Ok(());
    };
    let tmp = common::tempdir()?;
    let src = source(tmp.path(), &ffmpeg, Format::M4a)?;
    let fake = FakeYtdlp::write(&tmp.path().join("bin"), &src, ARTIST, TITLE, 3.0)?;
    let script = fake.script.display().to_string();
    let key_file = tmp.path().join("slskd-key");
    std::fs::write(&key_file, "old-key-0123456789")?;
    let key_file = key_file.display().to_string();
    let Some(env) = boot(move |s| {
        s.env("TRACKSIDE_ACQUIRE__ENABLED", "true")
            .env("TRACKSIDE_ACQUIRE__YTDLP__ENABLED", "true")
            .env("TRACKSIDE_ACQUIRE__YTDLP__PATH", script.clone())
            .env("TRACKSIDE_ACQUIRE__SOULSEEK__ENABLED", "true")
            .env("TRACKSIDE_ACQUIRE__SOULSEEK__URL", "http://127.0.0.1:9")
            .env(
                "TRACKSIDE_ACQUIRE__SOULSEEK__API_KEY_FILE",
                key_file.clone(),
            )
            .env("TRACKSIDE_ACQUIRE__SOULSEEK__MIN_BITRATE", "128")
    })
    .await?
    else {
        return Ok(());
    };
    let logs = env.server.log_text();
    for key in [
        "acquire.soulseek.enabled",
        "acquire.soulseek.url",
        "acquire.soulseek.api_key",
    ] {
        let warned = format!("{key} is deprecated and ignored");
        assert_eq!(logs.matches(&warned).count(), 1, "{key}: {logs}");
    }
    assert!(
        logs.contains("acquire.soulseek.min_bitrate is deprecated and sets acquire.min_bitrate"),
        "{logs}"
    );
    assert!(!logs.contains("old-key-0123456789"), "{logs}");

    let c = &env.client;
    let providers = c.get("/admin/acquire/providers", &env.admin).await?.json();
    let names: Vec<&str> = providers["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(names, ["ytdlp"], "{providers}");
    acquire_target(&env).await?;
    let started = request(c, &env.key, None).await?;
    assert_eq!(started.status, 202, "{}", started.text());
    let id = started.json()["id"].as_str().unwrap_or_default().to_owned();
    let done = wait_done(c, &env.key, &id, Duration::from_secs(60)).await?;
    assert!(done["song_id"].is_string(), "{done}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_download_resumes_after_the_server_is_killed() -> anyhow::Result<()> {
    let Some(ffmpeg) = common::ffmpeg() else {
        return Ok(());
    };
    let tmp = common::tempdir()?;
    let src = source(tmp.path(), &ffmpeg, Format::M4a)?;
    let fake = FakeYtdlp::write(&tmp.path().join("bin"), &src, ARTIST, TITLE, 3.0)?;
    fake.hold()?;
    let script = fake.script.display().to_string();
    let Some(mut env) = boot(|s| {
        s.env("TRACKSIDE_ACQUIRE__ENABLED", "true")
            .env("TRACKSIDE_ACQUIRE__YTDLP__ENABLED", "true")
            .env("TRACKSIDE_ACQUIRE__YTDLP__PATH", script.clone())
    })
    .await?
    else {
        return Ok(());
    };
    acquire_target(&env).await?;
    let started = request(&env.client, &env.key, None).await?;
    assert_eq!(started.status, 202, "{}", started.text());
    let id = started.json()["id"].as_str().unwrap_or_default().to_owned();
    env.client
        .poll(
            &format!("/acquisitions/{id}"),
            &env.key,
            Duration::from_secs(30),
            |r| r.json()["state"] == "downloading",
        )
        .await?;
    env.server.kill();
    fake.release();
    env.server.restart()?;
    let c = env.server.client();
    let done = wait_done(&c, &env.key, &id, Duration::from_secs(60)).await?;
    assert!(done["song_id"].is_string(), "{done}");
    let fetches = fake.calls().iter().filter(|c| c.contains(" -x ")).count();
    assert!(
        fetches >= 2,
        "the download ran again after the restart: {:?}",
        fake.calls()
    );
    Ok(())
}
