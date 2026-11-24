//! Device keys, API keys, app passwords and media tokens.

mod common;

use std::time::Duration;

use common::{BoxError, Harness, TestResult, denied, device, harness, ip, wp1};
use trackside_auth::{AuthError, KeyKind, Principal, RouteClass, Scopes, SubsonicCredentials, Via};
use trackside_core::{Permissions, ServerEvent};

fn bearer(secret: &str) -> http::request::Parts {
    parts("/api/v1/me", Some(secret))
}

fn parts(uri: &str, bearer: Option<&str>) -> http::request::Parts {
    let mut builder = http::Request::builder().uri(uri);
    if let Some(secret) = bearer {
        builder = builder.header("authorization", format!("Bearer {secret}"));
    }
    let (parts, ()) = builder.body(()).unwrap_or_default().into_parts();
    parts
}

async fn api(h: &Harness, secret: &str) -> Result<Principal, AuthError> {
    h.auth
        .authenticate(&bearer(secret), ip(1), RouteClass::Api)
        .await
}

async fn media(h: &Harness, token: &str) -> Result<Principal, AuthError> {
    h.auth
        .authenticate(
            &parts(&format!("/api/v1/songs/s1/stream?mt={token}"), None),
            ip(1),
            RouteClass::Media,
        )
        .await
}

#[tokio::test]
async fn revoked_key_is_rejected_within_one_second() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;

        // Revoked through this process: rejected on the very next request.
        let phone = h
            .auth
            .issue_device_key(admin, device("phone", "Phone"))
            .await?;
        api(&h, &phone.secret).await?;
        h.auth.revoke_key(Some(admin), &phone.key).await?;
        assert!(matches!(
            denied(api(&h, &phone.secret).await)?,
            AuthError::Unauthenticated
        ));

        // Revoked by another process sharing the database (the CLI): the cached entry lives
        // at most one second.
        let tablet = h
            .auth
            .issue_device_key(admin, device("tablet", "Tablet"))
            .await?;
        api(&h, &tablet.secret).await?;
        let cli = h.second_auth([42; 32]);
        cli.revoke_device(None, &tablet.device).await?;
        h.advance(Duration::from_secs(1));
        assert!(matches!(
            denied(api(&h, &tablet.secret).await)?,
            AuthError::Unauthenticated
        ));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn revocation_event_drops_the_cached_key_at_once() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let phone = h
            .auth
            .issue_device_key(admin, device("phone", "Phone"))
            .await?;
        api(&h, &phone.secret).await?;
        let device_key = phone.principal.device.as_ref().ok_or("no device")?.0;
        // Revoke behind the cache's back, as another component would, then announce it.
        let id = phone.device.clone();
        h.db.db
            .write(move |tx| trackside_core::repo::devices::revoke(tx, None, &id))
            .await?;
        h.db.events.publish(ServerEvent::CredentialRevoked {
            user: phone.principal.access.user,
            device: Some(device_key),
        });
        for _ in 0..1000 {
            if api(&h, &phone.secret).await.is_err() {
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
        Err::<(), BoxError>("the cached key outlived the revocation event".into())
    })
    .await
}

#[tokio::test]
async fn media_token_is_rejected_after_device_revoked() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let phone = h
            .auth
            .issue_device_key(admin, device("phone", "Phone"))
            .await?;
        let token = h.auth.mint_media_token(&phone.principal)?;
        assert_eq!(media(&h, &token.token).await?.via, Via::MediaToken);
        h.auth.revoke_device(Some(admin), &phone.device).await?;
        assert!(matches!(
            denied(media(&h, &token.token).await)?,
            AuthError::MediaTokenExpired
        ));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn media_token_dies_when_the_device_signs_in_again() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let first = h
            .auth
            .issue_device_key(admin, device("phone", "Phone"))
            .await?;
        let token = h.auth.mint_media_token(&first.principal)?;
        let second = h
            .auth
            .issue_device_key(admin, device("phone", "Phone"))
            .await?;
        assert_eq!(first.device, second.device);
        assert!(matches!(
            denied(api(&h, &first.secret).await)?,
            AuthError::Unauthenticated
        ));
        assert!(matches!(
            denied(media(&h, &token.token).await)?,
            AuthError::MediaTokenExpired
        ));
        api(&h, &second.secret).await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn media_token_expires_by_the_server_clock() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let phone = h
            .auth
            .issue_device_key(admin, device("phone", "Phone"))
            .await?;
        let token = h.auth.mint_media_token(&phone.principal)?;
        assert_eq!(
            token.expires_at,
            trackside_core::Clock::now(h.db.clock.as_ref())
                .saturating_add(h.config.media_token_ttl)
        );
        h.advance(h.config.media_token_ttl);
        media(&h, &token.token).await?;
        h.advance(Duration::from_millis(1));
        assert!(matches!(
            denied(media(&h, &token.token).await)?,
            AuthError::MediaTokenExpired
        ));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn media_token_survives_the_server_clock_stepping_back() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let phone = h
            .auth
            .issue_device_key(admin, device("phone", "Phone"))
            .await?;
        let token = h.auth.mint_media_token(&phone.principal)?;
        let now = trackside_core::Clock::now(h.db.clock.as_ref());
        h.db.clock.set(now.saturating_sub(Duration::from_secs(120)));
        media(&h, &token.token).await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn media_token_minted_by_a_clock_far_ahead_is_refused() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let phone = h
            .auth
            .issue_device_key(admin, device("phone", "Phone"))
            .await?;
        let now = trackside_core::Clock::now(h.db.clock.as_ref());
        h.advance(Duration::from_hours(24));
        let token = h.auth.mint_media_token(&phone.principal)?;
        h.db.clock.set(now);
        assert!(matches!(
            denied(media(&h, &token.token).await)?,
            AuthError::Unauthenticated
        ));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn forged_or_foreign_media_tokens_are_refused() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let phone = h
            .auth
            .issue_device_key(admin, device("phone", "Phone"))
            .await?;
        let other_server = h.second_auth([7; 32]);
        let foreign = other_server
            .issue_device_key(admin, device("x", "X"))
            .await?;
        let token = other_server.mint_media_token(&foreign.principal)?;
        assert!(matches!(
            denied(media(&h, &token.token).await)?,
            AuthError::Unauthenticated
        ));
        let mine = h.auth.mint_media_token(&phone.principal)?;
        let tampered = format!("A{}", mine.token);
        assert!(matches!(
            denied(media(&h, &tampered).await)?,
            AuthError::Unauthenticated
        ));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn api_key_media_token_is_bound_to_the_user_and_key() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let key = h
            .auth
            .create_key(admin, KeyKind::ApiKey, "Symfonium", Scopes::KEY_DEFAULT)
            .await?;
        let other = h
            .auth
            .create_key(admin, KeyKind::ApiKey, "Other", Scopes::KEY_DEFAULT)
            .await?;
        let principal = api(&h, &key.secret).await?;
        let token = h.auth.mint_media_token(&principal)?;
        let played = media(&h, &token.token).await?;
        assert_eq!(played.via, Via::MediaToken);
        assert_eq!(played.key.as_ref(), Some(&key.key.id));
        assert_eq!(played.device, None);
        // The key had no download scope, so neither does its token.
        assert!(played.scopes.contains(Scopes::STREAM));
        assert!(!played.scopes.contains(Scopes::DOWNLOAD));
        assert!(!played.scopes.contains(Scopes::WRITE));

        h.auth.revoke_key(Some(admin), &key.key.id).await?;
        assert!(matches!(
            denied(media(&h, &token.token).await)?,
            AuthError::MediaTokenExpired
        ));
        // Revoking one key leaves tokens minted by another alone.
        let other_token = h.auth.mint_media_token(&api(&h, &other.secret).await?)?;
        media(&h, &other_token.token).await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn keys_without_stream_and_sessions_cannot_mint_media_tokens() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let reader = h
            .auth
            .create_key(admin, KeyKind::ApiKey, "Reader", Scopes::LIBRARY)
            .await?;
        let principal = api(&h, &reader.secret).await?;
        assert!(matches!(
            h.auth.mint_media_token(&principal),
            Err(AuthError::Forbidden)
        ));
        let session = h.auth.create_session(admin, ip(1), None).await?;
        let (parts, ()) = http::Request::builder()
            .uri("/api/v1/me")
            .header(
                "cookie",
                format!("__Host-trackside_session={}", session.value),
            )
            .body(())
            .unwrap_or_default()
            .into_parts();
        let browser = h.auth.authenticate(&parts, ip(1), RouteClass::Api).await?;
        assert!(matches!(
            h.auth.mint_media_token(&browser),
            Err(AuthError::Forbidden)
        ));
        let phone = h.auth.issue_device_key(admin, device("p", "Phone")).await?;
        let token = h.auth.mint_media_token(&phone.principal)?;
        let from_token = media(&h, &token.token).await?;
        assert!(matches!(
            h.auth.mint_media_token(&from_token),
            Err(AuthError::Forbidden)
        ));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn two_users_sharing_one_device_get_separate_keys() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mia = h.member(admin, "mia").await?;
        let luke = h
            .auth
            .issue_device_key(admin, device("family-ipad", "iPad"))
            .await?;
        let her = h
            .auth
            .issue_device_key(mia, device("family-ipad", "iPad"))
            .await?;
        assert_ne!(luke.device, her.device);
        assert_eq!(api(&h, &luke.secret).await?.username, "admin");
        assert_eq!(api(&h, &her.secret).await?.username, "mia");
        h.auth.revoke_device(Some(mia), &her.device).await?;
        assert!(api(&h, &her.secret).await.is_err());
        assert_eq!(api(&h, &luke.secret).await?.username, "admin");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn one_user_with_many_devices_keeps_each_key() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mut issued = Vec::new();
        for install in ["iphone", "ipad", "mac", "watch"] {
            issued.push(
                h.auth
                    .issue_device_key(admin, device(install, install))
                    .await?,
            );
        }
        for key in &issued {
            api(&h, &key.secret).await?;
        }
        h.auth.revoke_device(Some(admin), &issued[1].device).await?;
        for (i, key) in issued.iter().enumerate() {
            assert_eq!(api(&h, &key.secret).await.is_ok(), i != 1);
        }
        Ok(())
    })
    .await
}

#[tokio::test]
async fn users_cannot_revoke_each_others_keys() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mia = h.member(admin, "mia").await?;
        let hers = h.auth.issue_device_key(mia, device("p", "Phone")).await?;
        let mine = h.auth.issue_device_key(admin, device("p", "Phone")).await?;
        assert!(matches!(
            denied(h.auth.revoke_device(Some(admin), &hers.device).await)?,
            AuthError::NotFound
        ));
        assert!(matches!(
            denied(h.auth.revoke_key(Some(mia), &mine.key).await)?,
            AuthError::NotFound
        ));
        api(&h, &hers.secret).await?;
        h.auth.revoke_device(None, &hers.device).await?;
        assert!(api(&h, &hers.secret).await.is_err());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn keys_are_stored_only_as_hashes() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let phone = h.auth.issue_device_key(admin, device("p", "Phone")).await?;
        let app = h
            .auth
            .create_key(admin, KeyKind::AppPassword, "DSub", Scopes::KEY_DEFAULT)
            .await?;
        let secrets = [phone.secret.clone(), app.secret.clone()];
        let found = h
            .db
            .db
            .read(move |c| {
                let mut statement = c.prepare(
                    "SELECT count(*) FROM api_keys WHERE CAST(secret_hash AS TEXT) = ?1 OR instr(secret_sealed, CAST(?1 AS BLOB)) > 0",
                )?;
                let mut total = 0i64;
                for secret in &secrets {
                    total += statement.query_row([secret], |row| row.get::<_, i64>(0))?;
                }
                Ok(total)
            })
            .await?;
        assert_eq!(found, 0);
        assert_eq!(app.key.prefix, app.secret.chars().take(10).collect::<String>());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn app_passwords_need_the_legacy_permission_and_never_work_as_bearer() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mia = h.member(admin, "mia").await?;
        let refused = denied(
            h.auth
                .create_key(mia, KeyKind::AppPassword, "DSub", Scopes::KEY_DEFAULT)
                .await,
        )?;
        assert!(matches!(refused, AuthError::Forbidden));
        let legacy = h
            .member_with(
                admin,
                "zoe",
                Permissions {
                    legacy_subsonic: true,
                    ..Permissions::default()
                },
            )
            .await?;
        let app = h
            .auth
            .create_key(legacy, KeyKind::AppPassword, "DSub", Scopes::KEY_DEFAULT)
            .await?;
        assert_eq!(app.secret.len(), 24);
        assert_eq!(app.username, "zoe");
        assert!(api(&h, &app.secret).await.is_err());
        assert!(matches!(
            denied(
                h.auth
                    .create_key(legacy, KeyKind::Device, "x", Scopes::ALL)
                    .await
            )?,
            AuthError::Invalid(_)
        ));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn lost_secret_key_invalidates_app_passwords_only() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let phone = h.auth.issue_device_key(admin, device("p", "Phone")).await?;
        let token = h.auth.mint_media_token(&phone.principal)?;
        let app = h
            .auth
            .create_key(admin, KeyKind::AppPassword, "DSub", Scopes::KEY_DEFAULT)
            .await?;
        let api_key = h
            .auth
            .create_key(admin, KeyKind::ApiKey, "Symfonium", Scopes::KEY_DEFAULT)
            .await?;
        let session = h.auth.create_session(admin, ip(1), None).await?;
        let legacy = SubsonicCredentials {
            username: Some(String::from("admin")),
            password: Some(app.secret.clone()),
            ..SubsonicCredentials::default()
        };
        h.auth
            .authenticate_subsonic(&legacy, ip(1))
            .await
            .map_err(|e| e.to_string())?;
        assert!(h.auth.check_secret_key().await?);

        // The server restarts with a new secret key.
        let restarted = h.second_auth([99; 32]);
        assert!(!restarted.check_secret_key().await?);
        assert!(
            restarted
                .authenticate_subsonic(&legacy, ip(1))
                .await
                .is_err()
        );
        assert!(
            restarted
                .authenticate(
                    &parts(&format!("/media?mt={}", token.token), None),
                    ip(1),
                    RouteClass::Media
                )
                .await
                .is_err()
        );
        restarted
            .authenticate(&bearer(&phone.secret), ip(1), RouteClass::Api)
            .await?;
        restarted
            .authenticate(&bearer(&api_key.secret), ip(1), RouteClass::Api)
            .await?;
        let (cookie, ()) = http::Request::builder()
            .uri("/api/v1/me")
            .header(
                "cookie",
                format!("__Host-trackside_session={}", session.value),
            )
            .body(())
            .unwrap_or_default()
            .into_parts();
        restarted
            .authenticate(&cookie, ip(1), RouteClass::Api)
            .await?;
        assert_eq!(
            restarted.login("admin", common::PASSWORD, ip(1)).await?,
            admin
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn busy_key_is_limited_to_its_request_rate() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let phone = h.auth.issue_device_key(admin, device("p", "Phone")).await?;
        for _ in 0..200 {
            api(&h, &phone.secret).await?;
        }
        assert!(matches!(
            denied(api(&h, &phone.secret).await)?,
            AuthError::RateLimited { .. }
        ));
        // Media bytes are not counted.
        h.auth
            .authenticate(&bearer(&phone.secret), ip(1), RouteClass::Media)
            .await?;
        h.advance(Duration::from_secs(1));
        api(&h, &phone.secret).await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn stream_slots_cap_each_user() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let phone = h.auth.issue_device_key(admin, device("p", "Phone")).await?;
        let held: Vec<_> = (0..trackside_auth::STREAMS_PER_USER)
            .map(|_| h.auth.stream_slot(&phone.principal))
            .collect::<Result<_, _>>()?;
        assert!(h.auth.stream_slot(&phone.principal).is_err());
        drop(held);
        drop(h.auth.stream_slot(&phone.principal)?);
        Ok(())
    })
    .await
}
