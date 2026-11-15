//! Subsonic credentials: `apiKey`, and `u`+`t`+`s` or `u`+`p` against app passwords only.

mod common;

use common::{BoxError, Harness, PASSWORD, TestResult, harness, ip, subsonic_denied, wp1};
use std::fmt::Write as _;

use md5::{Digest, Md5};
use trackside_auth::{KeyKind, Scopes, SubsonicAuthError, SubsonicCredentials, Via};
use trackside_core::{Permissions, UserKey};

fn creds(
    username: Option<&str>,
    token: Option<&str>,
    salt: Option<&str>,
    password: Option<&str>,
    api_key: Option<&str>,
) -> SubsonicCredentials {
    SubsonicCredentials {
        username: username.map(str::to_owned),
        token: token.map(str::to_owned),
        salt: salt.map(str::to_owned),
        password: password.map(str::to_owned),
        api_key: api_key.map(str::to_owned),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn token(password: &str, salt: &str) -> String {
    hex(&Md5::digest(format!("{password}{salt}").as_bytes()))
}

async fn legacy_user(h: &Harness) -> Result<(UserKey, String), BoxError> {
    let admin = h.admin().await?;
    let zoe = h
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
        .create_key(zoe, KeyKind::AppPassword, "DSub", Scopes::KEY_DEFAULT)
        .await?;
    Ok((zoe, app.secret))
}

#[tokio::test]
async fn api_key_with_username_is_error_43() -> TestResult {
    let h = harness()?;
    let mixed = [
        creds(Some("luke"), None, None, None, Some("tsk_x")),
        creds(None, Some("t"), Some("s"), None, Some("tsk_x")),
        creds(None, None, None, Some("p"), Some("tsk_x")),
        creds(Some("luke"), Some("t"), Some("s"), Some("p"), None),
    ];
    for c in mixed {
        let error = subsonic_denied(h.auth.authenticate_subsonic(&c, ip(1)).await)?;
        assert_eq!(error.code(), 43);
    }
    Ok(())
}

#[tokio::test]
async fn missing_parameters_are_error_10() -> TestResult {
    let h = harness()?;
    for c in [
        creds(None, None, None, None, None),
        creds(Some("luke"), None, None, None, None),
        creds(Some("luke"), Some("t"), None, None, None),
        creds(None, Some("t"), Some("s"), None, None),
        creds(Some(""), None, None, Some("x"), None),
    ] {
        let error = subsonic_denied(h.auth.authenticate_subsonic(&c, ip(1)).await)?;
        assert_eq!(error, SubsonicAuthError::Missing);
    }
    Ok(())
}

#[tokio::test]
async fn subsonic_token_auth_works_only_with_app_passwords() -> TestResult {
    wp1(async {
        let h = harness()?;
        let (_, app) = legacy_user(&h).await?;
        let salt = "c19b2d";
        let ok = h
            .auth
            .authenticate_subsonic(
                &creds(
                    Some("zoe"),
                    Some(&token(&app, salt)),
                    Some(salt),
                    None,
                    None,
                ),
                ip(1),
            )
            .await
            .map_err(|e| e.to_string())?;
        assert_eq!((ok.username.as_str(), ok.via), ("zoe", Via::AppPassword));
        assert_eq!(ok.scopes, Scopes::KEY_DEFAULT);
        let upper = token(&app, salt).to_uppercase();
        h.auth
            .authenticate_subsonic(
                &creds(Some("ZOE"), Some(&upper), Some(salt), None, None),
                ip(1),
            )
            .await
            .map_err(|e| e.to_string())?;
        let plain = h
            .auth
            .authenticate_subsonic(&creds(Some("zoe"), None, None, Some(&app), None), ip(1))
            .await;
        assert!(plain.is_ok());
        let hex = hex(app.as_bytes());
        let encoded = h
            .auth
            .authenticate_subsonic(
                &creds(Some("zoe"), None, None, Some(&format!("enc:{hex}")), None),
                ip(1),
            )
            .await;
        assert!(encoded.is_ok());

        let wrong = subsonic_denied(
            h.auth
                .authenticate_subsonic(
                    &creds(
                        Some("zoe"),
                        Some(&token("nope", salt)),
                        Some(salt),
                        None,
                        None,
                    ),
                    ip(2),
                )
                .await,
        )?;
        assert_eq!(wrong, SubsonicAuthError::WrongCredentials);

        // The admin has no app password: token auth is not available for that account.
        let none = subsonic_denied(
            h.auth
                .authenticate_subsonic(
                    &creds(
                        Some("admin"),
                        Some(&token(PASSWORD, salt)),
                        Some(salt),
                        None,
                        None,
                    ),
                    ip(3),
                )
                .await,
        )?;
        assert_eq!(none, SubsonicAuthError::TokenNotSupported);
        // Unknown users answer the same, so names cannot be probed.
        let unknown = subsonic_denied(
            h.auth
                .authenticate_subsonic(
                    &creds(Some("ghost"), Some("x"), Some(salt), None, None),
                    ip(4),
                )
                .await,
        )?;
        assert_eq!(unknown, SubsonicAuthError::TokenNotSupported);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn login_password_is_never_accepted_by_subsonic() -> TestResult {
    wp1(async {
        let h = harness()?;
        legacy_user(&h).await?;
        let salt = "abcdef";
        for c in [
            creds(
                Some("zoe"),
                Some(&token(PASSWORD, salt)),
                Some(salt),
                None,
                None,
            ),
            creds(Some("zoe"), None, None, Some(PASSWORD), None),
            creds(Some("admin"), None, None, Some(PASSWORD), None),
        ] {
            let error = subsonic_denied(h.auth.authenticate_subsonic(&c, ip(5)).await)?;
            assert!(matches!(
                error,
                SubsonicAuthError::WrongCredentials | SubsonicAuthError::TokenNotSupported
            ));
            h.advance(std::time::Duration::from_secs(60));
        }
        Ok(())
    })
    .await
}

#[tokio::test]
async fn revoked_app_password_stops_working() -> TestResult {
    wp1(async {
        let h = harness()?;
        let (zoe, app) = legacy_user(&h).await?;
        let c = creds(Some("zoe"), None, None, Some(&app), None);
        h.auth
            .authenticate_subsonic(&c, ip(1))
            .await
            .map_err(|e| e.to_string())?;
        let keys =
            h.db.db
                .read(move |conn| trackside_core::repo::keys::list(conn, Some(zoe)))
                .await?;
        let id = keys.first().ok_or("no key")?.id.clone();
        h.auth.revoke_key(Some(zoe), &id).await?;
        assert!(h.auth.authenticate_subsonic(&c, ip(1)).await.is_err());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn legacy_auth_can_be_turned_off_server_wide() -> TestResult {
    wp1(async {
        let mut config = common::config();
        let on = harness()?;
        let (_, app) = legacy_user(&on).await?;
        config.legacy_subsonic = false;
        let off = std::sync::Arc::new(trackside_auth::Auth::new(
            on.db.db.clone(),
            &config,
            trackside_auth::SecretKey::from_bytes([42; 32]),
            Vec::new(),
            on.db.clock.clone(),
        ));
        let error = subsonic_denied(
            off.authenticate_subsonic(&creds(Some("zoe"), None, None, Some(&app), None), ip(1))
                .await,
        )?;
        assert_eq!(error, SubsonicAuthError::TokenNotSupported);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn api_key_is_accepted_and_bad_keys_are_error_44() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let key = h
            .auth
            .create_key(admin, KeyKind::ApiKey, "Symfonium", Scopes::KEY_DEFAULT)
            .await?;
        let ok = h
            .auth
            .authenticate_subsonic(&creds(None, None, None, None, Some(&key.secret)), ip(1))
            .await
            .map_err(|e| e.to_string())?;
        assert_eq!(ok.via, Via::ApiKey);
        let bad = subsonic_denied(
            h.auth
                .authenticate_subsonic(&creds(None, None, None, None, Some("tsk_not-a-key")), ip(2))
                .await,
        )?;
        assert_eq!(bad.code(), 44);
        let (_, app) = (
            0,
            h.auth
                .create_key(admin, KeyKind::AppPassword, "DSub", Scopes::KEY_DEFAULT)
                .await?,
        );
        let as_api_key = subsonic_denied(
            h.auth
                .authenticate_subsonic(&creds(None, None, None, None, Some(&app.secret)), ip(2))
                .await,
        )?;
        assert_eq!(as_api_key.code(), 44);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn subsonic_failures_are_rate_limited_per_address() -> TestResult {
    wp1(async {
        let h = harness()?;
        legacy_user(&h).await?;
        let mut limited = None;
        for _ in 0..8 {
            if let Err(e @ SubsonicAuthError::RateLimited { .. }) = h
                .auth
                .authenticate_subsonic(&creds(Some("zoe"), None, None, Some("guess"), None), ip(9))
                .await
            {
                limited = Some(e);
            }
        }
        assert_eq!(limited.map(SubsonicAuthError::code), Some(40));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn app_password_mints_a_media_token_that_dies_with_it() -> TestResult {
    wp1(async {
        let h = harness()?;
        let (zoe, app) = legacy_user(&h).await?;
        let principal = h
            .auth
            .authenticate_subsonic(&creds(Some("zoe"), None, None, Some(&app), None), ip(1))
            .await
            .map_err(|e| e.to_string())?;
        let token = h.auth.mint_media_token(&principal)?;
        let uri = format!("/api/v1/songs/s1/stream?mt={}", token.token);
        let (parts, ()) = http::Request::builder()
            .uri(&uri)
            .body(())
            .unwrap_or_default()
            .into_parts();
        let played = h
            .auth
            .authenticate(&parts, ip(1), trackside_auth::RouteClass::Media)
            .await?;
        assert_eq!(played.username, "zoe");
        let key = principal.key.clone().ok_or("no key")?;
        h.auth.revoke_key(Some(zoe), &key).await?;
        assert!(
            h.auth
                .authenticate(&parts, ip(1), trackside_auth::RouteClass::Media)
                .await
                .is_err()
        );
        Ok(())
    })
    .await
}
