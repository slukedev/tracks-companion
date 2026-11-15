//! Accounts and access, driven over HTTP: setup, invites, per-library grants, pairing, bad
//! credentials, rate limits, re-authentication, security headers and log redaction.

#![expect(clippy::too_many_lines, reason = "a scenario reads best as one story")]

mod common;

use std::time::Duration;

use common::{ADMIN, PASSWORD, app_login, boot, snapshot_songs, titles, wait_scan};
use serde_json::json;
use trackside_e2e::Auth;
use trackside_e2e::client::device;
use trackside_e2e::library::{Track, write_track};

/// An invite's token from its URL.
fn invite_token(url: &str) -> String {
    url.rsplit('/').next().unwrap_or_default().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn members_see_only_the_libraries_they_were_given() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let kids = env.dir.path().join("kids");
    let mut lullaby = Track::new("Lullaby.mp3", "Lullaby", "Nursery", "Bedtime");
    lullaby.freq = 1234;
    write_track(&env.ffmpeg, &kids, &lullaby)?;
    let created = c
        .post(
            "/admin/libraries",
            &env.admin,
            &json!({"name": "Kids", "root": kids.display().to_string(), "grant_all_users": false}),
        )
        .await?;
    assert_eq!(created.status, 201, "{}", created.text());
    let kids_id = created.json()["id"].as_str().unwrap_or_default().to_owned();
    wait_scan(c, &env.admin, &kids_id).await?;

    let invite = c
        .post(
            "/admin/invites",
            &env.admin,
            &json!({"role": "member", "library_ids": [kids_id], "note": "for Mia",
                    "permissions": {"can_download": false}}),
        )
        .await?;
    assert_eq!(invite.status, 201, "{}", invite.text());
    let token = invite_token(invite.json()["url"].as_str().unwrap_or_default());
    let info = c.get(&format!("/invites/{token}"), &Auth::None).await?;
    assert_eq!(info.json()["valid"], true);
    let accepted = c
        .post(
            &format!("/invites/{token}/accept"),
            &Auth::None,
            &json!({"username": "mia", "display_name": "Mia", "password": "another good password"}),
        )
        .await?;
    assert_eq!(accepted.status, 201, "{}", accepted.text());
    let again = c
        .post(
            &format!("/invites/{token}/accept"),
            &Auth::None,
            &json!({"username": "eve", "password": "another good password"}),
        )
        .await?;
    assert!(
        again.status >= 400,
        "an invite works once: {}",
        again.status
    );

    let (mia, _) = app_login(c, "mia", "another good password", "MIA-PHONE").await?;
    let (songs, _) = snapshot_songs(c, &mia).await?;
    assert_eq!(titles(&songs), ["Lullaby"]);
    let (admin_songs, _) = snapshot_songs(c, &env.key).await?;
    assert_eq!(admin_songs.len(), env.lib.tracks.len() + 2);

    let halo = env.song("Halo").await?;
    let halo_id = halo["id"].as_str().unwrap_or_default();
    for path in [
        format!("/songs/{halo_id}"),
        format!("/songs/{halo_id}/stream"),
        format!("/art/{halo_id}"),
    ] {
        let reply = c.get(&path, &mia).await?;
        assert_eq!(
            reply.status,
            404,
            "{path} leaks to a member: {}",
            reply.text()
        );
    }
    let search = c.get("/search?q=halo", &mia).await?.json();
    assert!(
        search["songs"]["items"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{search}"
    );
    let download = c
        .get(
            &format!(
                "/songs/{}/download",
                songs[0]["id"].as_str().unwrap_or_default()
            ),
            &mia,
        )
        .await?;
    assert_eq!(download.status, 403, "can_download is off");
    let admin_page = c.get("/admin/users", &mia).await?;
    assert_eq!(admin_page.status, 403);

    let users = c.get("/admin/users", &env.admin).await?.json();
    let mia_id = users["users"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|u| u["username"] == "mia")
        .and_then(|u| u["id"].as_str())
        .unwrap_or_default()
        .to_owned();
    let libraries = c.get("/admin/libraries", &env.admin).await?.json();
    let lib_id = libraries["libraries"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|l| l["name"] == "Music")
        .and_then(|l| l["id"].as_str())
        .unwrap_or_default()
        .to_owned();
    let granted = c
        .put(
            &format!("/admin/users/{mia_id}/libraries"),
            &env.admin,
            &json!({"library_ids": [lib_id, kids_id]}),
        )
        .await?;
    assert!(granted.status < 300, "{}", granted.text());
    let (now_songs, _) = snapshot_songs(c, &mia).await?;
    assert_eq!(now_songs.len(), env.lib.tracks.len() + 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_pairs_by_code_and_a_spent_code_is_refused() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let offer = c
        .post(
            "/me/pairing-codes",
            &env.admin,
            &json!({"device_name": "Mac Studio"}),
        )
        .await?;
    assert_eq!(offer.status, 201, "{}", offer.text());
    let code = offer.json()["code"].as_str().unwrap_or_default().to_owned();
    let claimed = c
        .post(
            "/pair/claim",
            &Auth::None,
            &json!({"code": code, "device": device("MAC-1", "Mac Studio")}),
        )
        .await?;
    assert_eq!(claimed.status, 201, "{}", claimed.text());
    let result = claimed.json();
    let key = Auth::Key(result["api_key"].as_str().unwrap_or_default().to_owned());
    let me = c.get("/me", &key).await?.json();
    assert_eq!(me["user"]["username"], ADMIN);
    assert_eq!(me["device"]["name"], "Mac Studio");
    assert!(
        result["server"]["urls"]
            .as_array()
            .is_some_and(|u| !u.is_empty())
    );
    let reuse = c
        .post(
            "/pair/claim",
            &Auth::None,
            &json!({"code": code, "device": device("MAC-2", "Other")}),
        )
        .await?;
    assert_eq!(reuse.status, 401);

    let offer = c
        .post(
            "/me/pairing-codes",
            &env.admin,
            &json!({"device_name": "Guess Me"}),
        )
        .await?
        .json();
    for _ in 0..5 {
        let wrong = c
            .post(
                "/pair/claim",
                &Auth::None,
                &json!({"code": "AAAA-AAAA", "device": device("X", "X")}),
            )
            .await?;
        assert!(
            wrong.status == 401 || wrong.status == 429,
            "{}",
            wrong.status
        );
    }
    let late = c
        .post(
            "/pair/claim",
            &Auth::None,
            &json!({"code": offer["code"], "device": device("MAC-3", "Late")}),
        )
        .await?;
    assert!(
        late.status == 401 || late.status == 429,
        "guessing must cost the offer or the address: {}",
        late.status
    );
    if late.status == 429 {
        assert!(late.header("retry-after").is_some());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_credentials_are_refused_and_repeated_failures_are_limited() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s.env("TRACKSIDE_AUTH__LOGIN_RATE", "5/1m")).await? else {
        return Ok(());
    };
    let c = &env.client;
    let anonymous = c.get("/me", &Auth::None).await?;
    assert_eq!(anonymous.status, 401);
    assert_eq!(anonymous.code(), "unauthenticated");
    let bogus = c
        .get("/me", &Auth::Key(String::from("tsk_not_a_real_key")))
        .await?;
    assert_eq!(bogus.status, 401);
    let Auth::Key(key) = &env.key else {
        anyhow::bail!("no key");
    };
    let in_query = c
        .http
        .get(c.api(&format!("/me?api_key={key}")))
        .send()
        .await?;
    assert_eq!(in_query.status(), 400);
    let csrf = c
        .http
        .post(c.api("/admin/invites"))
        .header(
            "cookie",
            match &env.admin {
                Auth::Session(cookie) => cookie.clone(),
                _ => String::new(),
            },
        )
        .json(&json!({}))
        .send()
        .await?;
    assert_eq!(csrf.status(), 403);

    let mut limited = None;
    for _ in 0..20 {
        let reply = c
            .post(
                "/auth/login",
                &Auth::None,
                &json!({"username": ADMIN, "password": "wrong password!", "device": device("D", "D")}),
            )
            .await?;
        if reply.status == 429 {
            limited = Some(reply);
            break;
        }
        assert_eq!(reply.status, 401);
    }
    let limited = limited.ok_or_else(|| anyhow::anyhow!("never rate limited"))?;
    let retry: u64 = limited
        .header("retry-after")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("429 without Retry-After"))?;
    assert!(retry >= 1);
    assert!(
        env.server
            .wait_log(Duration::from_secs(5), |l| l
                .contains("authentication failure"))
            .is_some()
            || env.server.log_text().contains("password"),
        "failed logins are logged for fail2ban"
    );
    let subsonic = c
        .http
        .get(c.url("/rest/ping?u=luke&p=wrong&v=1.16.1&c=Tracks&f=json"))
        .send()
        .await?;
    let body: serde_json::Value = subsonic.json().await?;
    assert_eq!(body["subsonic-response"]["status"], "failed", "{body}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sensitive_actions_need_a_recent_password_check() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let session = c.get("/auth/session", &env.admin).await?.json();
    let until = session["recent_auth_until"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(!until.is_empty(), "{session}");

    let created = c
        .post(
            "/me/api-keys",
            &env.admin,
            &json!({"kind": "api_key", "name": "fresh", "scopes": ["library"]}),
        )
        .await?;
    assert_eq!(
        created.status,
        201,
        "a fresh sign-in counts: {}",
        created.text()
    );

    age_sessions(env.server.data_dir())?;
    let refused = c
        .post(
            "/me/api-keys",
            &env.admin,
            &json!({"kind": "api_key", "name": "stale", "scopes": ["library"]}),
        )
        .await?;
    assert_eq!(refused.status, 403);
    assert_eq!(refused.code(), "reauth_required");
    let invite_admin = c
        .post("/admin/invites", &env.admin, &json!({"role": "admin"}))
        .await?;
    assert_eq!(
        invite_admin.code(),
        "reauth_required",
        "{} {}",
        invite_admin.status,
        invite_admin.text()
    );
    let invite_member = c
        .post("/admin/invites", &env.admin, &json!({"role": "member"}))
        .await?;
    assert_eq!(invite_member.status, 201, "members need no reauth");

    let wrong = c
        .post(
            "/auth/reauth",
            &env.admin,
            &json!({"password": "not it at all"}),
        )
        .await?;
    assert_eq!(wrong.status, 401);
    let right = c
        .post("/auth/reauth", &env.admin, &json!({"password": PASSWORD}))
        .await?;
    assert_eq!(right.status, 204, "{}", right.text());
    let retried = c
        .post(
            "/me/api-keys",
            &env.admin,
            &json!({"kind": "api_key", "name": "stale", "scopes": ["library"]}),
        )
        .await?;
    assert_eq!(retried.status, 201, "{}", retried.text());
    Ok(())
}

/// Moves every session's last password check 20 minutes into the past, as if the admin
/// signed in a while ago.
fn age_sessions(data_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = rusqlite::Connection::open(data_dir.join("trackside.db"))?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute(
        "UPDATE sessions SET recent_auth_at = coalesce(recent_auth_at, created_at) - 1200000",
        [],
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn responses_carry_the_security_headers_and_token_paths_stay_out_of_logs()
-> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let page = c.http.get(c.url("/")).send().await?;
    let h = page.headers();
    let csp = h
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    for part in [
        "default-src 'self'",
        "script-src 'self'",
        "style-src 'self'",
        "object-src 'none'",
        "base-uri 'none'",
        "frame-ancestors 'none'",
    ] {
        assert!(csp.contains(part), "CSP lacks {part}: {csp}");
    }
    for (name, value) in [
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
        ("cache-control", "no-store"),
    ] {
        assert_eq!(
            h.get(name).and_then(|v| v.to_str().ok()),
            Some(value),
            "{name} on index.html"
        );
    }
    assert!(h.contains_key("permissions-policy"));
    let asset = c.http.get(c.url("/app.js")).send().await?;
    assert_ne!(
        asset
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "static assets may be cached"
    );
    let me = c.get("/me", &env.key).await?;
    assert_eq!(me.header("cache-control"), Some("no-store"));
    assert_eq!(me.header("x-frame-options"), Some("DENY"));

    let secret = "SECRETTOKENabc123xyz";
    for path in [
        format!("/invite/{secret}"),
        format!("/reset/{secret}"),
        format!("/api/v1/invites/{secret}"),
        format!("/api/v1/password-resets/{secret}"),
    ] {
        let reply = c.http.get(c.url(&path)).send().await?;
        if path.starts_with("/invite/") || path.starts_with("/reset/") {
            assert_eq!(reply.status(), 200, "{path} serves its page");
            assert_eq!(
                reply
                    .headers()
                    .get("cache-control")
                    .and_then(|v| v.to_str().ok()),
                Some("no-store")
            );
        }
    }
    env.server.wait_log(Duration::from_secs(5), |l| {
        l.contains("password-resets/REDACTED")
    });
    let logs = env.server.log_text();
    assert!(!logs.contains(secret), "a token reached the log");
    assert!(logs.contains("/invite/REDACTED"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn stats_have_aligned_buckets_and_validate_their_parameters() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let halo = env.song("Halo").await?;
    let now = jiff_now();
    let listens = json!({"client_time": now, "listens": [{
        "client_listen_id": "STATS-1", "song_id": halo["id"], "started_at": now,
        "played_ms": 2000, "title": "Halo", "artist": "Beyoncé", "duration_ms": 2000}]});
    assert_eq!(c.post("/listens", &env.key, &listens).await?.status, 200);
    let stream = c
        .get(
            &format!("/songs/{}/stream", halo["id"].as_str().unwrap_or_default()),
            &env.key,
        )
        .await?;
    assert_eq!(stream.status, 200);

    let listening = c
        .get("/admin/stats/listening?days=7&tz=Europe/London", &env.admin)
        .await?;
    assert_eq!(listening.status, 200, "{}", listening.text());
    assert_eq!(listening.header("cache-control"), Some("no-store"));
    let body = listening.json();
    assert_eq!(body["tz"], "Europe/London");
    assert_eq!(body["days"].as_array().map(Vec::len), Some(7));
    let luke = body["users"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|u| u["username"] == ADMIN)
        .cloned()
        .unwrap_or_default();
    assert_eq!(luke["plays"].as_array().map(Vec::len), Some(7));
    assert_eq!(luke["plays"][6], 1, "{body}");

    let streams = c
        .get("/admin/stats/streams?hours=6", &env.admin)
        .await?
        .json();
    assert_eq!(streams["hours"].as_array().map(Vec::len), Some(6));
    for series in ["streams", "transcodes", "failed"] {
        assert_eq!(
            streams[series].as_array().map(Vec::len),
            Some(6),
            "{streams}"
        );
    }
    assert!(
        streams["streams"][5].as_u64().is_some(),
        "the current hour is recorded"
    );

    let storage = c
        .get("/admin/stats/storage?days=7", &env.admin)
        .await?
        .json();
    assert_eq!(storage["days"].as_array().map(Vec::len), Some(7));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let storage = c
            .get("/admin/stats/storage?days=7", &env.admin)
            .await?
            .json();
        if storage["data_free_bytes"][6].as_u64().is_some() {
            assert!(storage["database_bytes"][6].as_u64().unwrap_or(0) > 0);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no storage snapshot: {storage}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    for bad in [
        "/admin/stats/listening?days=3",
        "/admin/stats/streams?hours=100",
        "/admin/stats/storage?days=400",
    ] {
        let reply = c.get(bad, &env.admin).await?;
        assert_eq!(reply.status, 400, "{bad}");
    }
    let requests = c.get("/admin/stats/requests?tz=UTC", &env.admin).await?;
    if requests.status == 200 {
        let body = requests.json();
        for series in ["added", "failed", "cancelled"] {
            assert_eq!(body[series].as_array().map(Vec::len), Some(30), "{body}");
        }
    } else {
        assert_eq!(requests.status, 404, "built without acquisition");
    }
    assert_eq!(c.get("/admin/stats/listening", &env.key).await?.status, 403);
    Ok(())
}

fn jiff_now() -> String {
    trackside_e2e::rfc3339(trackside_e2e::now_ms())
}
