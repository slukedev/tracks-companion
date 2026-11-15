//! Transport rules over real HTTP: routing, `.view`, formPost, formats, errors, and the auth
//! checks the `/rest` layer makes before asking the auth package. None of these need seeded
//! data, so they run on the skeleton.

mod support;

use support::{Server, TestResult, query};

#[tokio::test]
async fn open_subsonic_extensions_need_no_auth_with_and_without_view() -> TestResult {
    let server = Server::start().await?;
    for path in [
        "/rest/getOpenSubsonicExtensions?f=json",
        "/rest/getOpenSubsonicExtensions.view?f=json",
    ] {
        let reply = server.get(path).await?;
        assert_eq!(reply.status, 200);
        assert_eq!(
            reply.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        let env = reply.envelope()?;
        assert_eq!(env["status"], "ok");
        assert_eq!(env["openSubsonic"], true);
        assert_eq!(env["type"], "trackside");
        assert_eq!(env["version"], "1.16.1");
        let names: Vec<&str> = env["openSubsonicExtensions"]
            .as_array()
            .ok_or("no list")?
            .iter()
            .filter_map(|e| e["name"].as_str())
            .collect();
        assert_eq!(
            names,
            [
                "apiKeyAuthentication",
                "formPost",
                "songLyrics",
                "transcodeOffset",
                "indexBasedQueue",
                "topSongsByArtistId",
                "playbackReport"
            ]
        );
    }
    Ok(())
}

#[tokio::test]
async fn xml_is_the_default_format() -> TestResult {
    let server = Server::start().await?;
    let reply = server.get("/rest/getOpenSubsonicExtensions").await?;
    assert_eq!(
        reply.header("content-type"),
        Some("text/xml; charset=utf-8")
    );
    let text = reply.text();
    assert!(text.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?><subsonic-response xmlns=\"http://subsonic.org/restapi\" status=\"ok\""));
    assert!(text.contains(
        "<openSubsonicExtensions name=\"formPost\"><versions>1</versions></openSubsonicExtensions>"
    ));
    Ok(())
}

#[tokio::test]
async fn form_post_body_equals_query_behaviour() -> TestResult {
    let server = Server::start().await?;
    let by_query = server
        .get("/rest/getOpenSubsonicExtensions.view?f=json&c=Amperfy")
        .await?;
    let by_form = server
        .post_form("/rest/getOpenSubsonicExtensions.view", "f=json&c=Amperfy")
        .await?;
    assert_eq!(by_query.status, by_form.status);
    assert_eq!(by_query.body, by_form.body);
    let conflict_query = server
        .get(&format!(
            "/rest/ping?{}",
            query(&[("apiKey", "tsk_x"), ("u", "luke"), ("f", "json")])
        ))
        .await?;
    let conflict_form = server
        .post_form("/rest/ping", "apiKey=tsk_x&u=luke&f=json")
        .await?;
    assert_eq!(conflict_query.error_code(), Some(43));
    assert_eq!(conflict_form.error_code(), Some(43));
    assert_eq!(conflict_query.body, conflict_form.body);
    Ok(())
}

#[tokio::test]
async fn api_key_and_u_together_is_43() -> TestResult {
    let server = Server::start().await?;
    for extra in [("u", "luke"), ("p", "secret"), ("t", "abc"), ("s", "salt")] {
        let target = format!(
            "/rest/ping.view?{}",
            query(&[
                ("apiKey", "tsk_whatever"),
                extra,
                ("f", "json"),
                ("v", "1.16.1"),
                ("c", "Symfonium")
            ])
        );
        let reply = server.get(&target).await?;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.error_code(), Some(43), "with {}", extra.0);
        assert_eq!(reply.envelope()?["status"], "failed");
    }
    Ok(())
}

#[tokio::test]
async fn missing_credentials_are_error_10() -> TestResult {
    let server = Server::start().await?;
    for target in [
        "/rest/ping?f=json",
        "/rest/ping?f=json&u=luke",
        "/rest/ping?f=json&u=luke&t=abc",
    ] {
        let reply = server.get(target).await?;
        assert_eq!(reply.error_code(), Some(10), "{target}");
    }
    Ok(())
}

#[tokio::test]
async fn media_errors_never_come_with_200() -> TestResult {
    let server = Server::start().await?;
    for endpoint in ["stream", "download", "getCoverArt", "hls", "getAvatar"] {
        let reply = server.get(&format!("/rest/{endpoint}?id=s1")).await?;
        assert_eq!(reply.status, 400, "{endpoint}");
        assert_eq!(reply.error_code(), Some(10), "{endpoint}");
        assert!(reply.text().contains("status=\"failed\""));
    }
    let conflict = server.get("/rest/stream?id=s1&apiKey=k&u=luke").await?;
    assert_eq!(conflict.status, 401);
    assert_eq!(conflict.error_code(), Some(43));
    Ok(())
}

#[tokio::test]
async fn jsonp_is_error_0() -> TestResult {
    let server = Server::start().await?;
    let reply = server.get("/rest/ping?f=jsonp&callback=cb").await?;
    assert_eq!(reply.error_code(), Some(0));
    assert!(
        reply.envelope()?["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("jsonp")
    );
    Ok(())
}

#[tokio::test]
async fn unknown_endpoints_are_404_with_an_envelope() -> TestResult {
    let server = Server::start().await?;
    for name in [
        "getSonicSimilarTracks",
        "getTranscodeDecision",
        "nonsense.view",
    ] {
        let reply = server.get(&format!("/rest/{name}?f=json")).await?;
        assert_eq!(reply.status, 404, "{name}");
        assert_eq!(reply.error_code(), Some(0), "{name}");
    }
    Ok(())
}

#[tokio::test]
async fn errors_decode_the_way_tracks_decodes_them() -> TestResult {
    let server = Server::start().await?;
    let reply = server
        .get("/rest/getArtists?u=luke&f=json&v=1.16.1&c=Tracks")
        .await?;
    let env = reply.envelope()?;
    assert_eq!(env["status"], "failed");
    assert!(env["error"]["code"].is_i64());
    assert!(env["error"]["message"].is_string());
    Ok(())
}

#[tokio::test]
async fn other_methods_are_refused() -> TestResult {
    let server = Server::start().await?;
    let reply = support::send(server.addr, "DELETE", "/rest/ping", &[], &[]).await?;
    assert_eq!(reply.status, 405);
    Ok(())
}
