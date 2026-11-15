//! Fixes from the independent review, each driven over HTTP against the real binary: uploads
//! that survive a full scan, per-user stream caps, HLS jobs tied to their user, artwork and
//! public playlists that respect library grants, upload permission taken away mid-upload,
//! metrics labelled by route, and invite links that ignore a forged `Host`.

#![expect(clippy::too_many_lines, reason = "a scenario reads best as one story")]

mod common;

use std::time::{Duration, Instant};

use anyhow::Context;
use common::{Env, app_login, boot, snapshot_songs, wait_scan};
use reqwest::Method;
use serde_json::{Value, json};
use trackside_e2e::Auth;
use trackside_e2e::client::send;
use trackside_e2e::library::{Format, Track, write_track};

/// A member who sees only `libraries`, signed in from an app.
async fn member(
    env: &Env,
    name: &str,
    libraries: &[&str],
    permissions: Value,
) -> anyhow::Result<(Auth, String)> {
    let c = &env.client;
    let invite = c
        .post(
            "/admin/invites",
            &env.admin,
            &json!({"role": "member", "library_ids": libraries, "permissions": permissions}),
        )
        .await?;
    anyhow::ensure!(invite.status == 201, "{}", invite.text());
    let url = invite.json()["url"].as_str().unwrap_or_default().to_owned();
    let token = url.rsplit('/').next().unwrap_or_default().to_owned();
    let password = "another good password";
    let accepted = c
        .post(
            &format!("/invites/{token}/accept"),
            &Auth::None,
            &json!({"username": name, "password": password}),
        )
        .await?;
    anyhow::ensure!(accepted.status == 201, "{}", accepted.text());
    let (key, _) = app_login(c, name, password, &format!("{name}-PHONE")).await?;
    let users = c.get("/admin/users", &env.admin).await?.json();
    let id = users["users"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|u| u["username"] == name)
        .and_then(|u| u["id"].as_str())
        .context("no such user")?
        .to_owned();
    Ok((key, id))
}

/// A second library only some users see, with one song in it.
async fn kids_library(env: &Env) -> anyhow::Result<String> {
    let kids = env.dir.path().join("kids");
    let mut lullaby = Track::new("Lullaby.mp3", "Lullaby", "Nursery", "Bedtime");
    lullaby.freq = 1234;
    write_track(&env.ffmpeg, &kids, &lullaby)?;
    let created = env
        .client
        .post(
            "/admin/libraries",
            &env.admin,
            &json!({"name": "Kids", "root": kids.display().to_string(), "grant_all_users": false}),
        )
        .await?;
    anyhow::ensure!(created.status == 201, "{}", created.text());
    let id = created.json()["id"].as_str().unwrap_or_default().to_owned();
    wait_scan(&env.client, &env.admin, &id).await?;
    Ok(id)
}

#[tokio::test(flavor = "multi_thread")]
async fn uploaded_songs_survive_a_full_scan_and_stop_when_permission_is_taken_away()
-> anyhow::Result<()> {
    let Some(env) = boot(|s| s.env("TRACKSIDE_UPLOADS__ENABLED", "true")).await? else {
        return Ok(());
    };
    let c = &env.client;
    let root = env.dir.path().join("uploads");
    std::fs::create_dir_all(&root)?;
    let created = c
        .post(
            "/admin/libraries",
            &env.admin,
            &json!({"name": "Uploads", "root": root.display().to_string(), "writable": true}),
        )
        .await?;
    assert_eq!(created.status, 201, "{}", created.text());
    let uploads = created.json()["id"].as_str().unwrap_or_default().to_owned();
    let targets = c
        .put(
            "/admin/settings/targets",
            &env.admin,
            &json!({"uploads_library": uploads}),
        )
        .await?;
    assert!(targets.status < 300, "{}", targets.text());

    let source = env.dir.path().join("source");
    let mut song = Track::new("Upload.flac", "Sent From Phone", "Luke", "Uploads");
    song.freq = 777;
    song.format = Format::Flac;
    let file = write_track(&env.ffmpeg, &source, &song)?;
    let bytes = std::fs::read(&file)?;
    let size = bytes.len();
    let upload = |name: &str| json!({"file_name": name, "size": size});
    let started = c.post("/uploads", &env.key, &upload("Upload.flac")).await?;
    assert_eq!(started.status, 201, "{}", started.text());
    let id = started.json()["id"].as_str().unwrap_or_default().to_owned();
    let put = c
        .request(Method::PUT, &c.api(&format!("/uploads/{id}")), &env.key)
        .header("content-range", format!("bytes 0-{}/{size}", size - 1))
        .body(bytes.clone());
    let put = send(put).await?;
    assert_eq!(put.status, 200, "{}", put.text());
    let done = c
        .post(&format!("/uploads/{id}/complete"), &env.key, &json!({}))
        .await?;
    assert_eq!(done.status, 202, "{}", done.text());
    let deadline = Instant::now() + Duration::from_secs(60);
    let song_id = loop {
        let status = c.get(&format!("/uploads/{id}"), &env.key).await?.json();
        if status["state"] == "imported" {
            break status["song_id"].as_str().unwrap_or_default().to_owned();
        }
        anyhow::ensure!(
            Instant::now() < deadline && status["state"] != "failed",
            "upload stuck: {status}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let scanned = c
        .post(
            &format!("/admin/libraries/{uploads}/scan"),
            &env.admin,
            &json!({"kind": "full"}),
        )
        .await?;
    assert_eq!(scanned.status, 202, "{}", scanned.text());
    let run = wait_scan(c, &env.admin, &uploads).await?;
    assert_eq!(run["missing"], 0, "{run}");
    let after = c.get(&format!("/songs/{song_id}"), &env.key).await?;
    assert_eq!(
        after.status,
        200,
        "the upload left the library: {}",
        after.text()
    );

    let (mia, mia_id) = member(
        &env,
        "mia",
        &[uploads.as_str()],
        json!({"can_upload": true}),
    )
    .await?;
    let mut receiving = Vec::new();
    for n in 0..8 {
        let reply = c
            .post("/uploads", &mia, &upload(&format!("{n}.flac")))
            .await?;
        assert_eq!(reply.status, 201, "{}", reply.text());
        receiving.push(reply.json()["id"].as_str().unwrap_or_default().to_owned());
    }
    let ninth = c.post("/uploads", &mia, &upload("9.flac")).await?;
    assert_eq!(ninth.status, 429, "{}", ninth.text());
    assert_eq!(ninth.code(), "quota_exceeded");

    let revoked = c
        .patch(
            &format!("/admin/users/{mia_id}"),
            &env.admin,
            &json!({"permissions": {"can_upload": false}}),
        )
        .await?;
    assert!(revoked.status < 300, "{}", revoked.text());
    let chunk = c
        .request(
            Method::PUT,
            &c.api(&format!("/uploads/{}", receiving[0])),
            &mia,
        )
        .header("content-range", format!("bytes 0-{}/{size}", size - 1))
        .body(bytes);
    let chunk = send(chunk).await?;
    assert_eq!(chunk.status, 403, "{}", chunk.text());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn each_user_gets_eight_streams_at_once() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let mut long = Track::new("Long/Long.wav", "Long", "Luke", "Long");
    long.format = Format::Wav;
    long.seconds = 240.0;
    long.freq = 911;
    write_track(&env.ffmpeg, env.music(), &long)?;
    env.scan("quick").await?;
    let song = env.song("Long").await?;
    let path = format!("/songs/{}/stream", song["id"].as_str().unwrap_or_default());
    let c = &env.client;
    let mut open = Vec::new();
    for _ in 0..8 {
        let response = c
            .request(Method::GET, &c.api(&path), &env.key)
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        open.push(response);
    }
    let ninth = c.get(&path, &env.key).await?;
    assert_eq!(ninth.status, 429, "{}", ninth.text());
    drop(open.pop());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let again = c
            .request(Method::GET, &c.api(&path), &env.key)
            .header("range", "bytes=0-1023")
            .send()
            .await?;
        if again.status() == 206 {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the slot never came back");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn hls_jobs_artwork_and_public_playlists_follow_library_grants() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let music = env.library_id().await?;
    let kids = kids_library(&env).await?;
    let (eve, _) = member(&env, "eve", &[music.as_str()], json!({})).await?;
    let (mia, _) = member(&env, "mia", &[kids.as_str()], json!({})).await?;

    let halo = env.song("Halo").await?;
    let halo_id = halo["id"].as_str().unwrap_or_default();
    let decision = c
        .post(
            "/playback/decisions",
            &env.key,
            &json!({"song_id": halo_id, "network": "cellular", "client": {
                "direct": [{"container": "mp3", "codecs": ["mp3"]}],
                "hls": true, "preferred_profile": "aac-256", "max_bitrate_kbps": 256,
                "allow_original_fallback": false}}),
        )
        .await?;
    assert_eq!(decision.status, 200, "{}", decision.text());
    let playlist = decision.json()["url"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(playlist.contains("/transcodes/"), "{playlist}");
    assert_eq!(c.get(&playlist, &env.key).await?.status, 200);
    let stolen = c.get(&playlist, &eve).await?;
    assert_eq!(
        stolen.status,
        404,
        "another user's HLS job: {}",
        stolen.text()
    );

    let art = halo["artwork_id"].as_str().context("Halo has artwork")?;
    assert_eq!(c.get(&format!("/art/{art}"), &eve).await?.status, 200);
    let hidden = c.get(&format!("/art/{art}"), &mia).await?;
    assert_eq!(hidden.status, 404, "artwork from a library mia can't see");

    let (lullabies, _) = snapshot_songs(c, &env.key).await?;
    let lullaby = lullabies
        .iter()
        .find(|s| s["title"] == "Lullaby")
        .and_then(|s| s["id"].as_str())
        .context("no lullaby")?
        .to_owned();
    let created = c
        .post(
            "/playlists",
            &env.key,
            &json!({"name": "Mixed", "is_public": true, "entries": [halo_id, lullaby]}),
        )
        .await?;
    assert_eq!(created.status, 201, "{}", created.text());
    let id = created.json()["id"].as_str().unwrap_or_default().to_owned();
    let seen = c.get(&format!("/playlists/{id}"), &mia).await?;
    assert_eq!(seen.status, 200, "{}", seen.text());
    let ids: Vec<String> = seen.json()["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["song_id"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        ids,
        [lullaby],
        "a public playlist named a song mia can't see"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_are_labelled_by_route_and_invite_links_ignore_a_forged_host() -> anyhow::Result<()>
{
    let Some(env) = boot(|s| s.env("TRACKSIDE_METRICS__ENABLED", "true")).await? else {
        return Ok(());
    };
    let c = &env.client;
    for id in ["s7m2k4q9x1v8b3n6c0de", "s0000000000000000001", "short"] {
        let _ = c.get(&format!("/songs/{id}"), &env.key).await?;
    }
    let _ = c.get(&c.url("/no/such/page/x7k2m9"), &Auth::None).await?;
    let metrics = c.get(&c.url("/metrics"), &Auth::None).await?.text();
    assert!(
        metrics.contains("route=\"/api/v1/songs/{id}\""),
        "{metrics}"
    );
    for raw in ["s7m2k4q9x1v8b3n6c0de", "short", "x7k2m9"] {
        assert!(!metrics.contains(raw), "{raw} became a label: {metrics}");
    }

    let Auth::Session(cookie) = &env.admin else {
        anyhow::bail!("the admin has no session");
    };
    let invite = reqwest::Client::new()
        .post(c.api("/admin/invites"))
        .header("cookie", cookie)
        .header("x-trackside-csrf", "1")
        .header("host", "evil.example")
        .header("origin", "http://evil.example")
        .json(&json!({"role": "member"}));
    let invite = send(invite).await?;
    assert_eq!(invite.status, 201, "{}", invite.text());
    let url = invite.json()["url"].as_str().unwrap_or_default().to_owned();
    assert!(!url.contains("evil.example"), "{url}");
    Ok(())
}
