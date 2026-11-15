//! The extractors over real HTTP on an ephemeral port.

mod common;

use common::{PASSWORD, TestResult, device, harness, harness_with, ip, send, serve, wp1};
use trackside_auth::{CSRF_HEADER, KeyKind, Scopes};

#[tokio::test]
async fn key_in_query_string_is_rejected_outside_media_routes() -> TestResult {
    let h = harness()?;
    let addr = serve(h.auth.clone()).await?;
    for path in [
        "/api?apiKey=tsk_abc",
        "/api?api_key=x",
        "/api?token=x",
        "/api?mt=abc",
        "/api?id=tsk_leaked",
        "/admin?key=x",
    ] {
        let reply = send(addr, "GET", path, &[]).await?;
        assert_eq!(reply.status, 400, "{path}");
        assert!(reply.body.contains("credential_in_query"), "{path}");
    }
    let media = send(addr, "GET", "/media?mt=not-a-token", &[]).await?;
    assert_eq!(media.status, 401);
    Ok(())
}

#[tokio::test]
async fn session_write_without_csrf_header_is_rejected() -> TestResult {
    let h = harness()?;
    let addr = serve(h.auth.clone()).await?;
    let host = addr.to_string();
    let origin = format!("http://{addr}");
    let reply = send(
        addr,
        "POST",
        "/api",
        &[
            ("Cookie", "__Host-trackside_session=whatever"),
            ("Origin", &origin),
            ("Host", &host),
        ],
    )
    .await?;
    assert_eq!(reply.status, 403);
    assert!(reply.body.contains("\"csrf\""));
    Ok(())
}

#[tokio::test]
async fn foreign_origin_is_rejected() -> TestResult {
    let h = harness()?;
    let addr = serve(h.auth.clone()).await?;
    for origin in ["https://evil.example", "null", "http://127.0.0.1:1"] {
        let reply = send(
            addr,
            "POST",
            "/admin",
            &[
                ("Cookie", "trackside_session=whatever"),
                ("Origin", origin),
                (CSRF_HEADER, "1"),
            ],
        )
        .await?;
        assert_eq!(reply.status, 403, "{origin}");
        assert!(reply.body.contains("\"csrf\""), "{origin}");
    }
    Ok(())
}

#[tokio::test]
async fn requests_before_setup_say_setup_is_required() -> TestResult {
    let h = harness()?;
    let addr = serve(h.auth.clone()).await?;
    let reply = send(addr, "GET", "/api", &[]).await?;
    assert_eq!(reply.status, 403);
    assert!(reply.body.contains("setup_required"));
    Ok(())
}

#[tokio::test]
async fn forwarded_headers_are_ignored_from_untrusted_peers() -> TestResult {
    let untrusted = harness()?;
    let addr = serve(untrusted.auth.clone()).await?;
    let spoofed = send(addr, "GET", "/ip", &[("X-Forwarded-For", "198.51.100.4")]).await?;
    assert_eq!(spoofed.body, "127.0.0.1");

    let trusted = harness_with(common::config(), vec!["127.0.0.0/8".parse()?])?;
    let addr = serve(trusted.auth.clone()).await?;
    let forwarded = send(
        addr,
        "GET",
        "/ip",
        &[("X-Forwarded-For", "198.51.100.4, 127.0.0.9")],
    )
    .await?;
    assert_eq!(forwarded.body, "198.51.100.4");
    let v6 = send(
        addr,
        "GET",
        "/ip",
        &[("Forwarded", "for=\"[2001:db8::4]:99\"")],
    )
    .await?;
    assert_eq!(v6.body, "2001:db8::4");
    Ok(())
}

#[tokio::test]
async fn media_token_rejected_on_api_routes() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let issued = h
            .auth
            .issue_device_key(admin, device("phone", "Phone"))
            .await?;
        let mt = h.auth.mint_media_token(&issued.principal)?;
        let addr = serve(h.auth.clone()).await?;

        let media = send(addr, "GET", &format!("/media?mt={}", mt.token), &[]).await?;
        assert_eq!(media.status, 200);
        assert_eq!(media.body, "admin MediaToken");

        let in_query = send(addr, "GET", &format!("/api?mt={}", mt.token), &[]).await?;
        assert_eq!(in_query.status, 400);
        let bearer = format!("Bearer {}", mt.token);
        let as_header = send(addr, "GET", "/api", &[("Authorization", &bearer)]).await?;
        assert_eq!(as_header.status, 401);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn device_key_works_on_api_and_media_routes_but_never_admin() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let issued = h
            .auth
            .issue_device_key(admin, device("phone", "Phone"))
            .await?;
        let addr = serve(h.auth.clone()).await?;
        let bearer = format!("Bearer {}", issued.secret);
        let api = send(addr, "GET", "/api", &[("Authorization", &bearer)]).await?;
        assert_eq!((api.status, api.body.as_str()), (200, "admin DeviceKey"));
        let media = send(addr, "GET", "/media", &[("Authorization", &bearer)]).await?;
        assert_eq!(media.status, 200);
        let admin_route = send(addr, "GET", "/admin", &[("Authorization", &bearer)]).await?;
        assert_eq!(admin_route.status, 403);
        let session_route = send(addr, "GET", "/session", &[("Authorization", &bearer)]).await?;
        assert_eq!(session_route.status, 403);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn admin_session_with_csrf_reaches_admin_routes_and_members_do_not() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let member = h.member(admin, "mia").await?;
        let addr = serve(h.auth.clone()).await?;
        let origin = format!("http://{addr}");

        let admin_user = h.auth.login("admin", PASSWORD, ip(1)).await?;
        let cookie = h
            .auth
            .create_session(admin_user, ip(1), Some("test"))
            .await?;
        let header = format!("__Host-trackside_session={}", cookie.value);
        let read = send(addr, "GET", "/admin", &[("Cookie", &header)]).await?;
        assert_eq!((read.status, read.body.as_str()), (200, "admin"));
        let change = send(
            addr,
            "POST",
            "/admin",
            &[("Cookie", &header), ("Origin", &origin), (CSRF_HEADER, "1")],
        )
        .await?;
        assert_eq!(change.status, 200);
        let forged = send(
            addr,
            "POST",
            "/admin",
            &[("Cookie", &header), ("Origin", &origin)],
        )
        .await?;
        assert_eq!(forged.status, 403);

        let member_cookie = h.auth.create_session(member, ip(2), None).await?;
        let member_header = format!("__Host-trackside_session={}", member_cookie.value);
        let refused = send(addr, "GET", "/admin", &[("Cookie", &member_header)]).await?;
        assert_eq!(refused.status, 403);
        let own = send(addr, "GET", "/session", &[("Cookie", &member_header)]).await?;
        assert_eq!((own.status, own.body.as_str()), (200, "mia"));

        h.auth.end_session(&cookie.value).await?;
        let ended = send(addr, "GET", "/admin", &[("Cookie", &header)]).await?;
        assert_eq!(ended.status, 401);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn key_without_the_scope_is_forbidden() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let key = h
            .auth
            .create_key(admin, KeyKind::ApiKey, "Reader", Scopes::LIBRARY)
            .await?;
        let addr = serve(h.auth.clone()).await?;
        let bearer = format!("Bearer {}", key.secret);
        let read = send(addr, "GET", "/api", &[("Authorization", &bearer)]).await?;
        assert_eq!((read.status, read.body.as_str()), (200, "admin ApiKey"));
        let write = send(addr, "POST", "/write", &[("Authorization", &bearer)]).await?;
        assert_eq!(write.status, 403);
        let media = send(addr, "GET", "/media", &[("Authorization", &bearer)]).await?;
        assert_eq!(media.status, 403);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn rate_limited_reply_carries_retry_after() -> TestResult {
    wp1(async {
        let h = harness()?;
        h.admin().await?;
        for _ in 0..5 {
            let _ = h.auth.login("admin", "wrong password", ip(9)).await;
        }
        let error = common::denied(h.auth.login("admin", "wrong password", ip(9)).await)?;
        let response = axum::response::IntoResponse::into_response(error);
        assert_eq!(response.status(), 429);
        assert!(response.headers().contains_key("retry-after"));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn a_request_forwarded_by_an_untrusted_proxy_never_counts_as_local() -> TestResult {
    let h = harness()?;
    let lan: std::net::IpAddr = "192.168.1.20".parse()?;
    let direct = http::HeaderMap::new();
    let mut forwarded = http::HeaderMap::new();
    forwarded.insert(
        "x-forwarded-for",
        http::HeaderValue::from_static("203.0.113.9"),
    );
    assert!(h.auth.is_local_client(&direct, lan));
    assert!(!h.auth.is_local_client(&forwarded, lan));

    assert!(!h.auth.untrusted_proxy_seen());
    let (parts, ()) = http::Request::builder()
        .uri("/api/v1/me")
        .header("x-forwarded-for", "203.0.113.9")
        .body(())?
        .into_parts();
    let peer = std::net::SocketAddr::new(lan, 50_000);
    assert_eq!(h.auth.client_ip(&parts, peer), lan);
    assert!(h.auth.untrusted_proxy_seen());

    let trusted = harness_with(common::config(), vec!["192.168.0.0/16".parse()?])?;
    assert!(trusted.auth.is_local_client(&forwarded, lan));
    assert_eq!(
        trusted.auth.client_ip(&parts, peer),
        "203.0.113.9".parse::<std::net::IpAddr>()?
    );
    assert!(!trusted.auth.untrusted_proxy_seen());
    Ok(())
}
