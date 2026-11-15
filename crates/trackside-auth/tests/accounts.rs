//! Setup, login, sessions, passwords, invites, resets and the last-admin rule.

mod common;

use std::time::{Duration, Instant};

use common::{PASSWORD, TestResult, denied, device, harness, harness_with, ip, new_user, wp1};
use trackside_auth::{AuthError, NewInvite, SetupState, UserPatch};
use trackside_core::repo::libraries::{self, NewLibrary};
use trackside_core::{LibrarySet, Permissions, Role};

#[tokio::test]
async fn setup_token_file_is_private_and_reused_until_setup() -> TestResult {
    let h = harness()?;
    let first = h
        .auth
        .ensure_setup_token(h.data.path())
        .await?
        .ok_or("no token")?;
    let path = h.data.path().join("setup-token");
    assert_eq!(std::fs::read_to_string(&path)?.trim(), first);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );
    }
    let again = h.auth.ensure_setup_token(h.data.path()).await?;
    assert_eq!(again.as_deref(), Some(first.as_str()));
    let banner = trackside_auth::setup_banner(&first, "http://192.168.1.20:4747/setup", &path);
    assert!(banner.contains(&first) && banner.contains("setup-token"));
    assert!(!banner.contains('\u{2014}'));
    Ok(())
}

#[tokio::test]
async fn wrong_setup_token_is_refused() -> TestResult {
    let h = harness()?;
    h.auth.ensure_setup_token(h.data.path()).await?;
    let wrong = "0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000";
    let error = denied(
        h.auth
            .complete_setup(wrong, new_user("eve", Role::Admin), ip(66))
            .await,
    )?;
    assert!(matches!(error, AuthError::Unauthenticated));
    let error = denied(
        h.auth
            .complete_setup("", new_user("eve", Role::Admin), ip(66))
            .await,
    )?;
    assert!(matches!(error, AuthError::Unauthenticated));
    assert_eq!(h.auth.setup_state().await?, SetupState::NeedsAdmin);
    Ok(())
}

#[tokio::test]
async fn guessing_the_setup_token_is_rate_limited() -> TestResult {
    let h = harness()?;
    h.auth.ensure_setup_token(h.data.path()).await?;
    let mut limited = false;
    for _ in 0..8 {
        if let Err(AuthError::RateLimited { .. }) = h
            .auth
            .complete_setup("ZZZZ", new_user("eve", Role::Admin), ip(66))
            .await
        {
            limited = true;
        }
    }
    assert!(limited);
    Ok(())
}

#[tokio::test]
async fn setup_token_works_once() -> TestResult {
    wp1(async {
        let h = harness()?;
        let token = h
            .auth
            .ensure_setup_token(h.data.path())
            .await?
            .ok_or("no token")?;
        let typed = token.to_lowercase().replace('-', " ");
        h.auth
            .complete_setup(&typed, new_user("luke", Role::Admin), ip(1))
            .await?;
        assert!(!h.data.path().join("setup-token").exists());
        assert_eq!(h.auth.setup_state().await?, SetupState::Done);
        let again = denied(
            h.auth
                .complete_setup(&token, new_user("eve", Role::Admin), ip(2))
                .await,
        )?;
        assert!(matches!(again, AuthError::Conflict(_)));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn setup_is_refused_after_an_admin_exists() -> TestResult {
    wp1(async {
        let h = harness()?;
        h.admin().await?;
        assert_eq!(h.auth.ensure_setup_token(h.data.path()).await?, None);
        let error = denied(h.auth.rotate_setup_token(h.data.path()).await)?;
        assert!(matches!(error, AuthError::Conflict(_)));
        // A second server process sees the same state from the database.
        let other = h.second_auth([42; 32]);
        assert_eq!(other.setup_state().await?, SetupState::Done);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn setup_makes_an_admin_whatever_role_is_sent() -> TestResult {
    wp1(async {
        let h = harness()?;
        let token = h
            .auth
            .ensure_setup_token(h.data.path())
            .await?
            .ok_or("no token")?;
        let user = h
            .auth
            .complete_setup(&token, new_user("luke", Role::Member), ip(1))
            .await?;
        let issued = h.auth.issue_device_key(user, device("mac", "Mac")).await?;
        assert_eq!(issued.principal.role, Role::Admin);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn login_failure_message_is_identical_for_unknown_and_wrong_password() -> TestResult {
    wp1(async {
        let mut config = common::config();
        config.argon2_memory = 4 << 20;
        config.argon2_iterations = 2;
        let h = harness_with(config, Vec::new())?;
        h.admin().await?;
        let wrong = denied(h.auth.login("admin", "not the password", ip(1)).await)?;
        let unknown = denied(h.auth.login("nobody", "not the password", ip(2)).await)?;
        assert_eq!(wrong.to_string(), unknown.to_string());
        assert!(matches!(wrong, AuthError::Unauthenticated));
        assert!(matches!(unknown, AuthError::Unauthenticated));

        let mut known = Vec::new();
        let mut missing = Vec::new();
        for round in 0..9u8 {
            // Fresh addresses and a moving clock keep the limits out of the measurement.
            h.advance(Duration::from_secs(3600));
            let start = Instant::now();
            let _ = h
                .auth
                .login("admin", "not the password", ip(10 + round))
                .await;
            known.push(start.elapsed());
            let start = Instant::now();
            let _ = h
                .auth
                .login("nobody", "not the password", ip(100 + round))
                .await;
            missing.push(start.elapsed());
        }
        known.sort();
        missing.sort();
        let known = known[4].as_secs_f64();
        let missing = missing[4].as_secs_f64();
        let ratio = known / missing;
        assert!(
            (0.9..=1.1).contains(&ratio),
            "wrong password took {known:.4}s, unknown user {missing:.4}s"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn fifth_failure_triggers_backoff_for_that_user_and_ip_only() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        h.member(admin, "mia").await?;
        let (a, b) = (ip(10), ip(11));
        for _ in 0..5 {
            let error = denied(h.auth.login("admin", "wrong", a).await)?;
            assert!(matches!(error, AuthError::Unauthenticated));
        }
        let blocked = denied(h.auth.login("admin", PASSWORD, a).await)?;
        assert!(
            matches!(blocked, AuthError::RateLimited { retry_after } if retry_after == Duration::from_secs(2))
        );
        assert_eq!(h.auth.login("admin", PASSWORD, b).await?, admin);
        assert!(h.auth.login("mia", PASSWORD, a).await.is_ok());
        h.advance(Duration::from_secs(2));
        assert_eq!(h.auth.login("admin", PASSWORD, a).await?, admin);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn usernames_match_without_regard_to_case() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        assert_eq!(h.auth.login("ADMIN", PASSWORD, ip(1)).await?, admin);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn disabled_user_cannot_sign_in_or_use_keys() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mia = h.member(admin, "mia").await?;
        let phone = h.auth.issue_device_key(mia, device("p", "Phone")).await?;
        h.auth
            .update_user(
                Some(admin),
                mia,
                UserPatch {
                    disabled: Some(true),
                    ..UserPatch::default()
                },
                None,
            )
            .await?;
        assert!(matches!(
            denied(h.auth.login("mia", PASSWORD, ip(3)).await)?,
            AuthError::Unauthenticated
        ));
        let addr = common::serve(h.auth.clone()).await?;
        let bearer = format!("Bearer {}", phone.secret);
        let reply = common::send(addr, "GET", "/api", &[("Authorization", &bearer)]).await?;
        assert_eq!(reply.status, 401);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn last_admin_cannot_be_demoted() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let demote = UserPatch {
            role: Some(Role::Member),
            ..UserPatch::default()
        };
        let error = denied(
            h.auth
                .update_user(Some(admin), admin, demote.clone(), None)
                .await,
        )?;
        assert!(matches!(error, AuthError::Conflict(_)));
        let disable = UserPatch {
            disabled: Some(true),
            ..UserPatch::default()
        };
        let error = denied(h.auth.update_user(Some(admin), admin, disable, None).await)?;
        assert!(matches!(error, AuthError::Conflict(_)));
        let error = denied(h.auth.delete_user(admin, admin, None).await)?;
        assert!(matches!(error, AuthError::Conflict(_)));

        let second = h.member(admin, "mia").await?;
        h.auth
            .update_user(
                Some(admin),
                second,
                UserPatch {
                    role: Some(Role::Admin),
                    ..UserPatch::default()
                },
                None,
            )
            .await?;
        h.auth
            .update_user(Some(second), admin, demote, None)
            .await?;
        let error = denied(h.auth.delete_user(admin, second, None).await)?;
        assert!(matches!(error, AuthError::NotFound | AuthError::Forbidden));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn members_cannot_manage_users_or_invites() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mia = h.member(admin, "mia").await?;
        let error = denied(
            h.auth
                .create_user(mia, new_user("eve", Role::Admin), None)
                .await,
        )?;
        assert!(matches!(error, AuthError::Forbidden));
        let error = denied(h.auth.create_invite(mia, invite(Role::Admin)).await)?;
        assert!(matches!(error, AuthError::Forbidden));
        let error = denied(h.auth.create_password_reset(mia, admin).await)?;
        assert!(matches!(error, AuthError::Forbidden));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn password_change_ends_other_sessions_and_keeps_devices() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mia = h.member(admin, "mia").await?;
        let here = h.auth.create_session(mia, ip(1), None).await?;
        let elsewhere = h.auth.create_session(mia, ip(2), None).await?;
        let phone = h.auth.issue_device_key(mia, device("p", "Phone")).await?;

        let wrong = denied(
            h.auth
                .change_password(
                    mia,
                    "not it",
                    "a new password",
                    Some(&here.value),
                    false,
                    ip(1),
                )
                .await,
        )?;
        assert!(matches!(wrong, AuthError::Forbidden));
        h.auth
            .change_password(
                mia,
                PASSWORD,
                "a new password",
                Some(&here.value),
                false,
                ip(1),
            )
            .await?;

        let addr = common::serve(h.auth.clone()).await?;
        let get = |cookie: String| async move {
            let header = format!("__Host-trackside_session={cookie}");
            common::send(addr, "GET", "/session", &[("Cookie", &header)]).await
        };
        assert_eq!(get(here.value.clone()).await?.status, 200);
        assert_eq!(get(elsewhere.value.clone()).await?.status, 401);
        let bearer = format!("Bearer {}", phone.secret);
        let device = common::send(addr, "GET", "/api", &[("Authorization", &bearer)]).await?;
        assert_eq!(device.status, 200);
        assert!(h.auth.login("mia", PASSWORD, ip(1)).await.is_err());
        assert_eq!(h.auth.login("mia", "a new password", ip(1)).await?, mia);

        h.auth
            .change_password(mia, "a new password", "another password", None, true, ip(1))
            .await?;
        let revoked = common::send(addr, "GET", "/api", &[("Authorization", &bearer)]).await?;
        assert_eq!(revoked.status, 401);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn session_expires_when_idle_and_at_the_absolute_limit() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let idle = h.auth.create_session(admin, ip(1), None).await?;
        h.advance(h.config.session_idle + Duration::from_secs(1));
        assert!(matches!(
            denied(
                h.auth
                    .authenticate(
                        &cookie_parts(&idle.value),
                        ip(1),
                        trackside_auth::RouteClass::Api
                    )
                    .await
            )?,
            AuthError::Unauthenticated
        ));

        let busy = h.auth.create_session(admin, ip(1), None).await?;
        let mut elapsed = Duration::ZERO;
        while elapsed < h.config.session_max {
            h.auth
                .authenticate(
                    &cookie_parts(&busy.value),
                    ip(1),
                    trackside_auth::RouteClass::Api,
                )
                .await?;
            h.advance(Duration::from_secs(86_400));
            elapsed += Duration::from_secs(86_400);
        }
        assert!(matches!(
            denied(
                h.auth
                    .authenticate(
                        &cookie_parts(&busy.value),
                        ip(1),
                        trackside_auth::RouteClass::Api
                    )
                    .await
            )?,
            AuthError::Unauthenticated
        ));
        Ok(())
    })
    .await
}

fn cookie_parts(value: &str) -> http::request::Parts {
    let (parts, ()) = http::Request::builder()
        .uri("/api/v1/me")
        .header("cookie", format!("__Host-trackside_session={value}"))
        .body(())
        .unwrap_or_default()
        .into_parts();
    parts
}

fn invite(role: Role) -> NewInvite {
    NewInvite {
        role,
        permissions: trackside_core::Permissions {
            can_request: false,
            can_upload: true,
            ..trackside_core::Permissions::default()
        },
        libraries: Vec::new(),
        note: Some(String::from("for Mia")),
        expires_in: Duration::from_secs(7 * 86_400),
    }
}

#[tokio::test]
async fn invite_is_single_use_and_sets_role_and_permissions() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let issued = h.auth.create_invite(admin, invite(Role::Member)).await?;
        let info = h.auth.invite_info(&issued.token).await?;
        assert_eq!(info.role, Role::Member);

        let mut eager = new_user("mia", Role::Admin);
        eager.permissions = Permissions {
            can_request: true,
            legacy_subsonic: true,
            ..Permissions::default()
        };
        let mia = h.auth.accept_invite(&issued.token, eager, ip(5)).await?;
        let phone = h.auth.issue_device_key(mia, device("p", "Phone")).await?;
        assert_eq!(phone.principal.role, Role::Member);
        assert!(!phone.principal.permissions.can_request);
        assert!(phone.principal.permissions.can_upload);
        assert!(!phone.principal.permissions.legacy_subsonic);

        let again = denied(
            h.auth
                .accept_invite(&issued.token, new_user("eve", Role::Member), ip(5))
                .await,
        )?;
        assert!(matches!(again, AuthError::NotFound));
        assert!(matches!(
            denied(h.auth.invite_info(&issued.token).await)?,
            AuthError::NotFound
        ));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn expired_invite_is_refused() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let issued = h.auth.create_invite(admin, invite(Role::Member)).await?;
        h.advance(Duration::from_secs(7 * 86_400));
        let error = denied(
            h.auth
                .accept_invite(&issued.token, new_user("mia", Role::Member), ip(5))
                .await,
        )?;
        assert!(matches!(error, AuthError::NotFound));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn invite_with_a_taken_username_stays_open() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let issued = h.auth.create_invite(admin, invite(Role::Member)).await?;
        let taken = denied(
            h.auth
                .accept_invite(&issued.token, new_user("Admin", Role::Member), ip(5))
                .await,
        )?;
        assert!(matches!(taken, AuthError::Conflict(_)));
        h.auth
            .accept_invite(&issued.token, new_user("mia", Role::Member), ip(5))
            .await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn invite_grants_only_its_libraries() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let dirs = [tempfile::tempdir()?, tempfile::tempdir()?];
        let mut made = Vec::new();
        for (i, dir) in dirs.iter().enumerate() {
            let new = NewLibrary {
                name: format!("Library {i}"),
                root: dir.path().to_path_buf(),
                writable: false,
                watch: false,
                grant_all_users: false,
                granted_by: None,
            };
            made.push(h.db.db.write(move |tx| libraries::create(tx, &new)).await?);
        }
        let mut grant = invite(Role::Member);
        grant.libraries = vec![made[0].id.clone()];
        let issued = h.auth.create_invite(admin, grant).await?;
        let mia = h
            .auth
            .accept_invite(&issued.token, new_user("mia", Role::Member), ip(5))
            .await?;
        let phone = h.auth.issue_device_key(mia, device("p", "Phone")).await?;
        let access = &phone.principal.access;
        assert!(access.can_see(made[0].key));
        assert!(!access.can_see(made[1].key));
        assert!(matches!(access.libraries, LibrarySet::Only(_)));
        let admin_phone = h.auth.issue_device_key(admin, device("a", "Admin")).await?;
        assert!(admin_phone.principal.access.can_see(made[1].key));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn password_reset_link_works_once_and_ends_sessions() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mia = h.member(admin, "mia").await?;
        let session = h.auth.create_session(mia, ip(1), None).await?;
        let reset = h.auth.create_password_reset(admin, mia).await?;
        h.auth.password_reset_info(&reset.token).await?;
        assert!(h.auth.use_password_reset(&reset.token, "").await.is_err());
        h.auth
            .use_password_reset(&reset.token, "fresh password")
            .await?;
        assert_eq!(h.auth.login("mia", "fresh password", ip(1)).await?, mia);
        assert!(matches!(
            denied(
                h.auth
                    .authenticate(
                        &cookie_parts(&session.value),
                        ip(1),
                        trackside_auth::RouteClass::Api
                    )
                    .await
            )?,
            AuthError::Unauthenticated
        ));
        let again = denied(
            h.auth
                .use_password_reset(&reset.token, "other password")
                .await,
        )?;
        assert!(matches!(again, AuthError::NotFound));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn password_reset_link_expires_after_a_day() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mia = h.member(admin, "mia").await?;
        let reset = h.auth.create_password_reset(admin, mia).await?;
        h.advance(Duration::from_secs(86_400));
        let error = denied(
            h.auth
                .use_password_reset(&reset.token, "fresh password")
                .await,
        )?;
        assert!(matches!(error, AuthError::NotFound));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn cli_reset_gives_a_one_time_password_and_requires_a_change() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let session = h.auth.create_session(admin, ip(1), None).await?;
        let cli = h.second_auth([42; 32]);
        let password = cli.admin_reset_password("admin").await?;
        let user = h.auth.login("admin", &password, ip(1)).await?;
        assert_eq!(user, admin);
        let phone = h.auth.issue_device_key(admin, device("p", "Phone")).await?;
        assert!(
            h.auth
                .password_change_required(&phone.principal.user)
                .await?
        );
        assert!(matches!(
            denied(
                h.auth
                    .authenticate(
                        &cookie_parts(&session.value),
                        ip(1),
                        trackside_auth::RouteClass::Api
                    )
                    .await
            )?,
            AuthError::Unauthenticated
        ));
        h.auth
            .change_password(admin, &password, "my own password", None, false, ip(1))
            .await?;
        assert!(
            !h.auth
                .password_change_required(&phone.principal.user)
                .await?
        );
        assert!(matches!(
            denied(cli.admin_reset_password("nobody").await)?,
            AuthError::NotFound
        ));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn members_change_only_their_own_display_name() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mia = h.member(admin, "mia").await?;
        let renamed = h
            .auth
            .update_user(
                Some(mia),
                mia,
                UserPatch {
                    display_name: Some(String::from("Mia R")),
                    ..UserPatch::default()
                },
                None,
            )
            .await?;
        assert_eq!(renamed.display_name, "Mia R");
        let promote = UserPatch {
            role: Some(Role::Admin),
            ..UserPatch::default()
        };
        let error = denied(
            h.auth
                .update_user(Some(mia), mia, promote.clone(), None)
                .await,
        )?;
        assert!(matches!(error, AuthError::Forbidden));
        let rename_admin = UserPatch {
            display_name: Some(String::from("pwned")),
            ..UserPatch::default()
        };
        let error = denied(
            h.auth
                .update_user(Some(mia), admin, rename_admin, None)
                .await,
        )?;
        assert!(matches!(error, AuthError::Forbidden));
        // The CLI (`trackside admin set-role`) acts without a session.
        h.auth.update_user(None, mia, promote, None).await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn racing_acceptances_of_one_invite_create_one_user() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let issued = h.auth.create_invite(admin, invite(Role::Member)).await?;
        let (first, second) = tokio::join!(
            h.auth
                .accept_invite(&issued.token, new_user("mia", Role::Member), ip(5)),
            h.auth
                .accept_invite(&issued.token, new_user("eve", Role::Member), ip(6)),
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let loser = if first.is_ok() { second } else { first };
        assert!(matches!(denied(loser)?, AuthError::NotFound));
        Ok(())
    })
    .await
}
