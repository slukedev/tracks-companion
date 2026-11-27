//! Device-code linking: start, lookup, approve or deny, poll, and the limits around them.

mod common;

use std::time::Duration;

use common::{BoxError, Harness, TestResult, device, harness, ip};
use trackside_auth::{
    AuthError, DeviceLinkStart, LINK_ATTEMPTS, LINK_INTERVAL, LINK_LIFETIME, LinkDecision,
    LinkError, MAX_INTERVAL, MAX_PENDING_LINKS, NewDevice, Principal, SLOW_DOWN_STEP, Scopes, Via,
};
use trackside_core::repo::devices;
use trackside_core::{ServerEvent, UserKey};

fn tv(install: &str) -> NewDevice {
    NewDevice {
        client_device_id: install.to_owned(),
        name: String::from("Living Room TV"),
        platform: String::from("android_tv"),
        model: Some(String::from("Chromecast with Google TV")),
        app_name: Some(String::from("Tracks")),
        app_version: Some(String::from("3.2")),
    }
}

/// A person's phone: a device key with every scope.
async fn phone(h: &Harness, user: UserKey, install: &str) -> Result<Principal, BoxError> {
    Ok(h.auth
        .issue_device_key(user, device(install, "Luke's iPhone"))
        .await?
        .principal)
}

fn link_error<T: std::fmt::Debug>(result: Result<T, LinkError>) -> Result<LinkError, BoxError> {
    match result {
        Ok(value) => Err(format!("expected a link error, got {value:?}").into()),
        Err(e) => Ok(e),
    }
}

/// A code with the same first half and a different last character: it reaches the link.
fn near_miss(code: &str) -> String {
    let mut chars: Vec<char> = code.chars().collect();
    if let Some(last) = chars.last_mut() {
        *last = if *last == 'Z' { 'Y' } else { 'Z' };
    }
    chars.into_iter().collect()
}

async fn started(h: &Harness, install: &str, last: u8) -> Result<DeviceLinkStart, BoxError> {
    Ok(h.auth.start_device_link(tv(install), ip(last)).await?)
}

#[tokio::test]
async fn an_approved_link_hands_out_its_key_once() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let approver = phone(&h, admin, "phone").await?;
    let link = started(&h, "tv-1", 10).await?;
    assert_eq!(link.user_code.len(), 9);
    assert_eq!(link.interval, LINK_INTERVAL);
    assert!(link.device_code.len() >= 43);

    assert!(matches!(
        link_error(h.auth.poll_device_link(&link.device_code, ip(10)).await)?,
        LinkError::Pending
    ));

    let typed = link.user_code.to_lowercase().replace('-', "");
    let seen = h.auth.lookup_device_link(&approver, &typed).await?;
    assert_eq!(seen.device_name, "Living Room TV");
    assert_eq!(seen.platform, "android_tv");
    assert!(!seen.nearby);

    h.auth
        .decide_device_link(&approver, &link.user_code, LinkDecision::Approve, ip(2))
        .await?;
    let before = h.db.db.read(move |c| devices::list(c, Some(admin))).await?;
    assert_eq!(before.len(), 1, "the TV is added when it collects its key");

    let mut events = h.db.db.events().subscribe();
    h.advance(LINK_INTERVAL);
    let issued = h.auth.poll_device_link(&link.device_code, ip(10)).await?;
    assert!(issued.secret.starts_with("tsk_"));
    assert_eq!(issued.device_name, "Living Room TV");
    assert_eq!(issued.principal.username, "admin");
    assert_eq!(issued.principal.via, Via::DeviceKey);
    assert_eq!(issued.principal.scopes, Scopes::ALL);
    h.auth.mint_media_token(&issued.principal)?;
    let event = events.recv().await?;
    assert!(matches!(
        &*event,
        ServerEvent::DeviceLinked { user, name, .. } if *user == admin && name == "Living Room TV"
    ));

    h.advance(LINK_INTERVAL);
    assert!(matches!(
        link_error(h.auth.poll_device_link(&link.device_code, ip(10)).await)?,
        LinkError::Expired
    ));
    assert!(matches!(
        link_error(h.auth.lookup_device_link(&approver, &link.user_code).await)?,
        LinkError::Used
    ));
    Ok(())
}

#[tokio::test]
async fn a_denied_link_tells_the_device_and_cannot_be_approved() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let approver = phone(&h, admin, "phone").await?;
    let link = started(&h, "tv-1", 10).await?;
    let denied = h
        .auth
        .decide_device_link(&approver, &link.user_code, LinkDecision::Deny, ip(2))
        .await?;
    assert_eq!(denied.device_name, "Living Room TV");
    assert!(matches!(
        link_error(h.auth.poll_device_link(&link.device_code, ip(10)).await)?,
        LinkError::Denied
    ));
    assert!(matches!(
        link_error(
            h.auth
                .decide_device_link(&approver, &link.user_code, LinkDecision::Approve, ip(2))
                .await
        )?,
        LinkError::Used
    ));
    Ok(())
}

#[tokio::test]
async fn a_code_is_approved_once() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let mia = h.member(admin, "mia").await?;
    let luke = phone(&h, admin, "phone").await?;
    let mia = phone(&h, mia, "mia-phone").await?;
    let link = started(&h, "tv-1", 10).await?;
    h.auth
        .decide_device_link(&luke, &link.user_code, LinkDecision::Approve, ip(2))
        .await?;
    assert!(matches!(
        link_error(
            h.auth
                .decide_device_link(&mia, &link.user_code, LinkDecision::Approve, ip(3))
                .await
        )?,
        LinkError::Used
    ));
    let issued = h.auth.poll_device_link(&link.device_code, ip(10)).await?;
    assert_eq!(issued.principal.username, "admin");
    Ok(())
}

#[tokio::test]
async fn links_expire_after_ten_minutes() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let approver = phone(&h, admin, "phone").await?;
    let link = started(&h, "tv-1", 10).await?;
    h.advance(LINK_LIFETIME);
    assert!(matches!(
        link_error(h.auth.poll_device_link(&link.device_code, ip(10)).await)?,
        LinkError::Expired
    ));
    assert!(matches!(
        link_error(h.auth.lookup_device_link(&approver, &link.user_code).await)?,
        LinkError::Expired
    ));
    assert!(matches!(
        link_error(
            h.auth
                .decide_device_link(&approver, &link.user_code, LinkDecision::Approve, ip(2))
                .await
        )?,
        LinkError::Expired
    ));
    Ok(())
}

#[tokio::test]
async fn an_approval_not_collected_in_time_expires() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let approver = phone(&h, admin, "phone").await?;
    let link = started(&h, "tv-1", 10).await?;
    h.auth
        .decide_device_link(&approver, &link.user_code, LinkDecision::Approve, ip(2))
        .await?;
    h.advance(LINK_LIFETIME);
    assert!(matches!(
        link_error(h.auth.poll_device_link(&link.device_code, ip(10)).await)?,
        LinkError::Expired
    ));
    let listed = h.db.db.read(move |c| devices::list(c, Some(admin))).await?;
    assert_eq!(listed.len(), 1, "no device is left behind");
    Ok(())
}

#[tokio::test]
async fn polling_too_often_slows_the_device_down() -> TestResult {
    let h = harness()?;
    let link = started(&h, "tv-1", 10).await?;
    let poll = || h.auth.poll_device_link(&link.device_code, ip(10));
    assert!(matches!(link_error(poll().await)?, LinkError::Pending));
    h.advance(Duration::from_secs(1));
    let LinkError::SlowDown { interval } = link_error(poll().await)? else {
        return Err("expected slow_down".into());
    };
    assert_eq!(interval, LINK_INTERVAL + SLOW_DOWN_STEP);
    // One second short of the new interval still counts as on time.
    h.advance(interval.saturating_sub(Duration::from_secs(1)));
    assert!(matches!(link_error(poll().await)?, LinkError::Pending));
    for _ in 0..20 {
        let _ = poll().await;
    }
    let LinkError::SlowDown { interval } = link_error(poll().await)? else {
        return Err("expected slow_down".into());
    };
    assert_eq!(interval, MAX_INTERVAL);
    Ok(())
}

#[tokio::test]
async fn a_link_locks_after_five_wrong_codes_reach_it() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let link = started(&h, "tv-1", 10).await?;
    let wrong = near_miss(&link.user_code);
    for n in 0..LINK_ATTEMPTS {
        // A different approver each time, so only the link's own count is in play.
        let user = h.member(admin, &format!("guess{n}")).await?;
        let guesser = phone(&h, user, &format!("guess-phone-{n}")).await?;
        assert!(matches!(
            link_error(h.auth.lookup_device_link(&guesser, &wrong).await)?,
            LinkError::NotFound
        ));
    }
    let owner = phone(&h, admin, "phone").await?;
    assert!(matches!(
        link_error(h.auth.lookup_device_link(&owner, &link.user_code).await)?,
        LinkError::Locked
    ));
    assert!(matches!(
        link_error(h.auth.poll_device_link(&link.device_code, ip(10)).await)?,
        LinkError::Expired
    ));
    Ok(())
}

#[tokio::test]
async fn an_approver_who_keeps_guessing_is_rate_limited() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let approver = phone(&h, admin, "phone").await?;
    let link = started(&h, "tv-1", 10).await?;
    for guess in ["AAAA-AAAA", "BBBB-BBBB", "CCCC-CCCC", "DDDD-DDDD", "short"] {
        assert!(matches!(
            link_error(h.auth.lookup_device_link(&approver, guess).await)?,
            LinkError::NotFound
        ));
    }
    // The right code waits too, so guessing cannot be told apart from finding.
    assert!(matches!(
        link_error(h.auth.lookup_device_link(&approver, &link.user_code).await)?,
        LinkError::Auth(AuthError::RateLimited { .. })
    ));
    h.advance(Duration::from_mins(3));
    h.auth
        .lookup_device_link(&approver, &link.user_code)
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_link_never_grants_more_than_its_approver_holds() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let mut narrow = phone(&h, admin, "phone").await?;
    narrow.scopes = Scopes::KEY_DEFAULT;
    let link = started(&h, "tv-1", 10).await?;
    h.auth
        .decide_device_link(&narrow, &link.user_code, LinkDecision::Approve, ip(2))
        .await?;
    let issued = h.auth.poll_device_link(&link.device_code, ip(10)).await?;
    assert_eq!(issued.principal.scopes, Scopes::KEY_DEFAULT);
    assert!(!issued.principal.scopes.contains(Scopes::DOWNLOAD));
    Ok(())
}

#[tokio::test]
async fn only_sessions_and_device_keys_may_decide() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let link = started(&h, "tv-1", 10).await?;
    let mut session = phone(&h, admin, "phone").await?;
    session.via = Via::Session;
    h.auth.lookup_device_link(&session, &link.user_code).await?;
    for via in [Via::ApiKey, Via::AppPassword, Via::MediaToken] {
        let mut key = session.clone();
        key.via = via;
        assert!(matches!(
            link_error(
                h.auth
                    .decide_device_link(&key, &link.user_code, LinkDecision::Approve, ip(2))
                    .await
            )?,
            LinkError::Auth(AuthError::Forbidden)
        ));
    }
    Ok(())
}

#[tokio::test]
async fn starting_links_is_limited_per_address() -> TestResult {
    let h = harness()?;
    for n in 0..5 {
        started(&h, &format!("tv-{n}"), 10).await?;
    }
    assert!(matches!(
        h.auth.start_device_link(tv("tv-9"), ip(10)).await,
        Err(AuthError::RateLimited { .. })
    ));
    started(&h, "tv-other", 11).await?;
    h.advance(Duration::from_mins(1));
    started(&h, "tv-later", 10).await?;
    Ok(())
}

#[tokio::test]
async fn starting_links_is_limited_across_the_server() -> TestResult {
    let h = harness()?;
    let mut limited = false;
    for n in 0..=200u8 {
        if let Err(AuthError::RateLimited { .. }) = h
            .auth
            .start_device_link(tv(&format!("tv-{n}")), ip(n))
            .await
        {
            limited = true;
            break;
        }
    }
    assert!(limited);
    Ok(())
}

#[tokio::test]
async fn pending_links_are_bounded() -> TestResult {
    let h = harness()?;
    let addresses = u8::try_from(MAX_PENDING_LINKS)?;
    for n in 0..addresses {
        started(&h, &format!("tv-{n}"), n).await?;
        // Keeps the server-wide start rate out of the way.
        h.advance(Duration::from_secs(2));
    }
    assert!(matches!(
        h.auth.start_device_link(tv("one-more"), ip(250)).await,
        Err(AuthError::RateLimited { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn links_survive_a_restart() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let link = started(&h, "tv-1", 10).await?;
    let restarted = h.second_auth([42; 32]);
    let approver = restarted
        .issue_device_key(admin, device("phone", "Luke's iPhone"))
        .await?
        .principal;
    restarted
        .decide_device_link(&approver, &link.user_code, LinkDecision::Approve, ip(2))
        .await?;
    let issued = restarted
        .poll_device_link(&link.device_code, ip(10))
        .await?;
    assert_eq!(issued.device_name, "Living Room TV");
    Ok(())
}

#[tokio::test]
async fn unknown_device_codes_are_expired() -> TestResult {
    let h = harness()?;
    let link = started(&h, "tv-1", 10).await?;
    let mut forged = link.device_code.clone();
    forged.pop();
    forged.push('A');
    assert!(matches!(
        link_error(h.auth.poll_device_link(&forged, ip(10)).await)?,
        LinkError::Expired
    ));
    assert!(matches!(
        link_error(h.auth.poll_device_link("", ip(10)).await)?,
        LinkError::Expired
    ));
    Ok(())
}

#[tokio::test]
async fn a_disabled_approver_links_nothing() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let mia = h.member(admin, "mia").await?;
    let approver = phone(&h, mia, "mia-phone").await?;
    let link = started(&h, "tv-1", 10).await?;
    h.auth
        .decide_device_link(&approver, &link.user_code, LinkDecision::Approve, ip(2))
        .await?;
    h.auth
        .update_user(
            Some(admin),
            mia,
            trackside_auth::UserPatch {
                disabled: Some(true),
                ..trackside_auth::UserPatch::default()
            },
            None,
        )
        .await?;
    assert!(matches!(
        link_error(h.auth.poll_device_link(&link.device_code, ip(10)).await)?,
        LinkError::Expired
    ));
    Ok(())
}

#[tokio::test]
async fn guessing_across_many_accounts_hits_the_server_wide_limit() -> TestResult {
    let h = harness()?;
    let admin = h.admin().await?;
    let link = started(&h, "tv-1", 10).await?;
    let mut limited = false;
    for n in 0..20 {
        let user = h.member(admin, &format!("guess{n}")).await?;
        let guesser = phone(&h, user, &format!("guess-phone-{n}")).await?;
        // Four each stays under every approver's own limit.
        for guess in ["AAAA-AAAA", "BBBB-BBBB", "CCCC-CCCC", "DDDD-DDDD"] {
            if let LinkError::Auth(AuthError::RateLimited { .. }) =
                link_error(h.auth.lookup_device_link(&guesser, guess).await)?
            {
                limited = true;
            }
        }
    }
    assert!(limited);
    let owner = phone(&h, admin, "phone").await?;
    assert!(matches!(
        link_error(h.auth.lookup_device_link(&owner, &link.user_code).await)?,
        LinkError::Auth(AuthError::RateLimited { .. })
    ));
    Ok(())
}
