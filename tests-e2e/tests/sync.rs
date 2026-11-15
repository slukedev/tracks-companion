//! Delta sync while files change on disk, search, listens and events, as Tracks uses them.

#![expect(clippy::too_many_lines, reason = "a scenario reads best as one story")]

mod common;

use std::time::{Duration, Instant};

use common::{ADMIN, PASSWORD, app_login, boot, snapshot_songs};
use futures_util::StreamExt;
use serde_json::{Value, json};
use trackside_e2e::library::{Track, nfd, retag, write_track};
use trackside_e2e::{Auth, Client, now_ms, rfc3339};

/// Follows the delta cursor until `has_more` is false; returns the pages and the new cursor.
async fn delta(client: &Client, key: &Auth, cursor: &str) -> anyhow::Result<(Vec<Value>, String)> {
    let mut cursor = cursor.to_owned();
    let mut pages = Vec::new();
    loop {
        let reply = client.get(&format!("/sync?cursor={cursor}"), key).await?;
        anyhow::ensure!(
            reply.status == 200,
            "delta: {} {}",
            reply.status,
            reply.text()
        );
        let page = reply.json();
        anyhow::ensure!(page["mode"] == "delta", "{page}");
        page["next_cursor"]
            .as_str()
            .unwrap_or_default()
            .clone_into(&mut cursor);
        let more = page["has_more"].as_bool() == Some(true);
        pages.push(page);
        if !more {
            return Ok((pages, cursor));
        }
    }
}

fn upserted<'a>(pages: &'a [Value], kind: &str) -> Vec<&'a Value> {
    pages
        .iter()
        .flat_map(|p| p["upserts"][kind].as_array().into_iter().flatten())
        .collect()
}

fn deleted(pages: &[Value], kind: &str) -> Vec<String> {
    pages
        .iter()
        .flat_map(|p| p["deletes"][kind].as_array().into_iter().flatten())
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

/// The ids of the song at `rel` and of its exact duplicate elsewhere in the library. Which of the
/// two gets the id derived from the digest alone depends on which the first scan read first, so
/// a title cannot tell them apart.
async fn duplicate_ids(
    client: &Client,
    admin: &Auth,
    rel: &std::path::Path,
) -> anyhow::Result<(String, String)> {
    let reply = client.get("/admin/duplicates?limit=200", admin).await?;
    anyhow::ensure!(reply.status == 200, "duplicates: {}", reply.text());
    let body = reply.json();
    let wanted = rel.display().to_string();
    let group = body["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| g["songs"].as_array())
        .find(|songs| songs.iter().any(|s| s["path"] == wanted.as_str()))
        .ok_or_else(|| anyhow::anyhow!("{wanted} has no duplicate: {body}"))?;
    let id = |original: bool| {
        group
            .iter()
            .find(|s| (s["path"] == wanted.as_str()) == original)
            .and_then(|s| s["id"].as_str())
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("{body}"))
    };
    Ok((id(true)?, id(false)?))
}

#[tokio::test(flavor = "multi_thread")]
async fn delta_sync_follows_adds_retags_moves_and_deletes_on_disk() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let (before, cursor) = snapshot_songs(c, &env.key).await?;
    let id_of = |songs: &[Value], title: &str| -> String {
        songs
            .iter()
            .find(|s| s["title"] == title)
            .and_then(|s| s["id"].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let (unchanged, nothing) = delta(c, &env.key, &cursor).await?;
    assert!(upserted(&unchanged, "songs").is_empty(), "{unchanged:?}");

    let root = env.music();
    let mut fresh = Track::new(
        "New Album/01 Brand New.flac",
        "Brand New",
        "Newcomer",
        "New Album",
    );
    fresh.format = trackside_e2e::library::Format::Flac;
    fresh.freq = 1999;
    write_track(&env.ffmpeg, root, &fresh)?;
    let boy_rel = env.lib.track("If I Were a Boy").map(|t| t.rel.clone());
    let boy_rel = boy_rel.ok_or_else(|| anyhow::anyhow!("no track"))?;
    let (boy_id, copy_id) = duplicate_ids(c, &env.admin, &boy_rel).await?;
    let boy = root.join(&boy_rel);
    retag(&boy, "If I Were a Girl")?;
    let glosoli = env.lib.track("Glósóli").map(|t| root.join(&t.rel));
    let glosoli = glosoli.ok_or_else(|| anyhow::anyhow!("no track"))?;
    let moved = root.join("Moved Here").join(nfd("Glósóli moved.flac"));
    std::fs::create_dir_all(moved.parent().unwrap_or(root))?;
    std::fs::rename(&glosoli, &moved)?;
    let filler = env.lib.track("Filler 0").map(|t| root.join(&t.rel));
    let filler = filler.ok_or_else(|| anyhow::anyhow!("no track"))?;
    std::fs::remove_file(&filler)?;

    let deadline = Instant::now() + Duration::from_secs(40);
    let mut cursor = nothing;
    let mut seen_songs: Vec<Value> = Vec::new();
    let mut gone: Vec<String> = Vec::new();
    loop {
        let (pages, next) = delta(c, &env.key, &cursor).await?;
        cursor = next;
        seen_songs.extend(upserted(&pages, "songs").into_iter().cloned());
        gone.extend(deleted(&pages, "songs"));
        let titles: Vec<&str> = seen_songs
            .iter()
            .filter_map(|s| s["title"].as_str())
            .collect();
        let filler_id = id_of(&before, "Filler 0");
        let filler_done = gone.contains(&filler_id)
            || seen_songs
                .iter()
                .any(|s| s["id"] == filler_id.as_str() && s["missing"] == true);
        if titles.contains(&"Brand New") && titles.contains(&"If I Were a Girl") && filler_done {
            break;
        }
        if Instant::now() > deadline {
            anyhow::bail!(
                "changes never arrived; saw {titles:?}, deleted {gone:?}\n{}",
                env.server.log_text()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let retagged = seen_songs
        .iter()
        .rev()
        .find(|s| s["title"] == "If I Were a Girl")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        retagged["id"].as_str(),
        Some(boy_id.as_str()),
        "a retag keeps the song's id"
    );

    env.scan("quick").await?;
    let (after, _) = snapshot_songs(c, &env.key).await?;
    let title_of = |id: &str| {
        after
            .iter()
            .find(|s| s["id"] == id)
            .map(|s| (s["title"].clone(), s["missing"].clone()))
    };
    assert_eq!(
        title_of(&boy_id),
        Some((json!("If I Were a Girl"), json!(false))),
        "the retagged song is present under its old id"
    );
    assert_eq!(
        title_of(&copy_id),
        Some((json!("If I Were a Boy"), json!(false))),
        "the untouched duplicate keeps its id and title"
    );
    let added: Vec<&Value> = after
        .iter()
        .filter(|s| !before.iter().any(|b| b["id"] == s["id"]))
        .collect();
    assert_eq!(
        added.iter().map(|s| &s["title"]).collect::<Vec<_>>(),
        [&json!("Brand New")],
        "only the new file became a new song: {added:?}"
    );
    assert_eq!(
        id_of(&after, "Glósóli"),
        id_of(&before, "Glósóli"),
        "a move keeps the song's id"
    );
    let stream = c
        .get(
            &format!("/songs/{}/stream", id_of(&after, "Glósóli")),
            &env.key,
        )
        .await?;
    assert_eq!(
        stream.status, 200,
        "the moved file streams from its new place"
    );
    let searched = c
        .get("/search?q=brand%20new&types=song", &env.key)
        .await?
        .json();
    assert_eq!(
        searched["songs"]["items"][0]["title"], "Brand New",
        "{searched}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn search_folds_case_accents_and_matches_every_word_as_a_prefix() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let cases: &[(&str, &str, &str)] = &[
        ("glosoli", "songs", "Glósóli"),
        ("GLÓSÓLI", "songs", "Glósóli"),
        ("hoppi", "songs", "Hoppípolla"),
        ("love song t", "songs", "Love Song Two"),
        ("sigur", "artists", "Sigur Rós"),
        ("beyonce", "artists", "Beyoncé"),
        ("sasha fierce", "albums", "I Am... Sasha Fierce"),
        ("東京", "songs", "東京"),
    ];
    for (q, kind, expected) in cases {
        let path = format!("/search?q={}", percent(q));
        let reply = c.get(&path, &env.key).await?;
        assert_eq!(reply.status, 200);
        let body = reply.json();
        let names: Vec<&str> = body[kind]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i["title"].as_str().or_else(|| i["name"].as_str()))
            .collect();
        assert!(names.contains(expected), "{q}: {names:?}");
    }
    let page = c
        .get("/search?q=filler&types=song&limit=2", &env.key)
        .await?
        .json();
    let next = page["songs"]["next_cursor"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(!next.is_empty(), "{page}");
    let more = c
        .get(
            &format!("/search?q=filler&types=song&limit=2&cursor={next}"),
            &env.key,
        )
        .await?
        .json();
    let first: Vec<&Value> = page["songs"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let second: Vec<&Value> = more["songs"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    assert!(
        first
            .iter()
            .all(|a| second.iter().all(|b| a["id"] != b["id"]))
    );
    Ok(())
}

fn percent(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn listens_from_two_devices_are_deduplicated_and_retries_are_safe() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let (_, cursor) = snapshot_songs(c, &env.key).await?;
    let (second, _) = app_login(c, ADMIN, PASSWORD, "E2E-MAC").await?;
    let halo = env.song("Halo").await?;
    let start = now_ms() - 3_600_000;
    let items: Vec<Value> = (0..500)
        .map(|i| {
            json!({
                "client_listen_id": format!("PLAY-{i:04}"),
                "song_id": if i % 2 == 0 { halo["id"].clone() } else { Value::Null },
                "started_at": rfc3339(start + i * 1000),
                "played_ms": 2000,
                "title": if i % 2 == 0 { "Halo" } else { "Not On This Server" },
                "artist": "Beyoncé",
                "duration_ms": 2000
            })
        })
        .map(|mut v| {
            if v["song_id"].is_null() {
                v.as_object_mut().map(|o| o.remove("song_id"));
            }
            v
        })
        .collect();
    let batch = json!({"client_time": rfc3339(now_ms()), "listens": items});
    let started = Instant::now();
    let first = c.post("/listens", &env.key, &batch).await?;
    let elapsed = started.elapsed();
    assert_eq!(first.status, 200, "{}", first.text());
    let results = first.json();
    let accepted = results["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["status"] == "accepted")
        .count();
    assert_eq!(accepted, 500, "{results}");
    eprintln!("500 listens took {elapsed:?}");

    let retry = c.post("/listens", &env.key, &batch).await?.json();
    assert!(
        retry["results"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|r| r["status"] == "duplicate"),
        "a retry is all duplicates"
    );
    let overlap = json!({"client_time": rfc3339(now_ms()), "listens": [
        batch["listens"][0].clone(),
        batch["listens"][0].clone(),
        {"client_listen_id": "MAC-ONLY", "song_id": halo["id"], "started_at": rfc3339(start),
         "played_ms": 2000, "title": "Halo", "artist": "Beyoncé", "duration_ms": 2000},
        {"client_listen_id": "FUTURE", "song_id": halo["id"],
         "started_at": rfc3339(now_ms() + 3 * 86_400_000), "played_ms": 2000,
         "title": "Halo", "artist": "Beyoncé", "duration_ms": 2000}
    ]});
    let mixed = c.post("/listens", &second, &overlap).await?.json();
    let statuses: Vec<&str> = mixed["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["status"].as_str())
        .collect();
    assert_eq!(
        statuses,
        ["duplicate", "duplicate", "accepted", "rejected"],
        "{mixed}"
    );
    assert_eq!(mixed["results"][3]["reason"], "future");

    let (pages, _) = delta(c, &env.key, &cursor).await?;
    let stats = upserted(&pages, "play_stats");
    let halo_stats = stats
        .iter()
        .find(|s| s["id"] == halo["id"])
        .ok_or_else(|| anyhow::anyhow!("no play_stats for Halo: {pages:?}"))?;
    assert_eq!(halo_stats["play_count"], 251, "{halo_stats}");

    let export = c.get("/me/listens/export", &env.admin).await?;
    assert_eq!(export.status, 200);
    assert_eq!(export.text().lines().count(), 501);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn events_say_hello_report_changes_and_resume_from_last_event_id() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let halo = env.song("Halo").await?;
    let halo_id = halo["id"].as_str().unwrap_or_default().to_owned();
    let mut events = open_events(c, &env.key, None).await?;
    let hello = events.next_event(Duration::from_secs(5)).await?;
    assert_eq!(hello.0, "hello", "{hello:?}");
    let hello_seq = hello.1["seq"].as_i64().unwrap_or_default();
    let star = c
        .put(&format!("/me/stars/song/{halo_id}"), &env.key, &json!({}))
        .await?;
    assert_eq!(star.status, 204);
    let changed = events
        .next_named("changed", Duration::from_secs(10))
        .await?;
    let changed_seq = changed.1["seq"].as_i64().unwrap_or_default();
    assert!(changed_seq > hello_seq);
    drop(events);

    let mut resumed = open_events(c, &env.key, Some(hello_seq)).await?;
    let first = resumed.next_event(Duration::from_secs(5)).await?;
    let second = resumed.next_event(Duration::from_secs(5)).await?;
    assert!(
        [first.0.as_str(), second.0.as_str()].contains(&"changed"),
        "resuming from an old id reports the change at once: {first:?} {second:?}"
    );
    drop(resumed);
    let mut current = open_events(c, &env.key, Some(changed_seq)).await?;
    let _hello = current.next_event(Duration::from_secs(5)).await?;
    let quiet = current
        .next_named("changed", Duration::from_millis(1500))
        .await;
    assert!(quiet.is_err(), "nothing changed since the id: {quiet:?}");
    Ok(())
}

/// A server-sent event stream read incrementally.
struct Events {
    stream: futures_util::stream::BoxStream<'static, reqwest::Result<axum::body::Bytes>>,
    buffer: String,
}

async fn open_events(client: &Client, key: &Auth, last: Option<i64>) -> anyhow::Result<Events> {
    let mut request = client.request(reqwest::Method::GET, &client.api("/events"), key);
    if let Some(last) = last {
        request = request.header("last-event-id", last.to_string());
    }
    let response = request.send().await?;
    anyhow::ensure!(response.status() == 200, "events: {}", response.status());
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    anyhow::ensure!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    anyhow::ensure!(
        response
            .headers()
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok())
            == Some("no")
    );
    Ok(Events {
        stream: response.bytes_stream().boxed(),
        buffer: String::new(),
    })
}

impl Events {
    async fn next_event(&mut self, timeout: Duration) -> anyhow::Result<(String, Value)> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                let mut name = String::from("message");
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        v.trim().clone_into(&mut name);
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim());
                    }
                }
                if block.lines().all(|l| l.starts_with(':') || l.is_empty()) {
                    continue;
                }
                return Ok((name, serde_json::from_str(&data).unwrap_or(Value::Null)));
            }
            let chunk = tokio::time::timeout_at(deadline, self.stream.next())
                .await
                .map_err(|_| anyhow::anyhow!("no event in {timeout:?}"))?;
            match chunk {
                Some(Ok(bytes)) => self.buffer.push_str(&String::from_utf8_lossy(&bytes)),
                Some(Err(e)) => return Err(e.into()),
                None => anyhow::bail!("the stream ended"),
            }
        }
    }

    async fn next_named(
        &mut self,
        name: &str,
        timeout: Duration,
    ) -> anyhow::Result<(String, Value)> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = self.next_event(left).await?;
            if event.0 == name {
                return Ok(event);
            }
        }
    }
}
