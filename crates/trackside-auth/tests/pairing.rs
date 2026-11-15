//! Pairing offers (code and QR) and device-initiated pairing requests.

mod common;

use std::time::Duration;

use common::{TestResult, denied, device, harness, ip, wp1};
use trackside_auth::{AuthError, PairPoll, RouteClass};

fn wrong_code(right: &str) -> String {
    // Same first half, so the guess reaches the offer; a different second half.
    let mut chars: Vec<char> = right.chars().collect();
    if let Some(last) = chars.last_mut() {
        *last = if *last == 'Z' { 'Y' } else { 'Z' };
    }
    chars.into_iter().collect()
}

#[tokio::test]
async fn offer_is_claimed_by_typed_code_once() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let offer = h
            .auth
            .create_pairing_offer(admin, Some("Mac Studio"))
            .await?;
        assert_eq!(offer.code.len(), 9);
        let typed = offer.code.to_lowercase().replace('-', "");
        let issued = h
            .auth
            .claim_pairing(&typed, device("mac", "Mac"), ip(3))
            .await?;
        assert_eq!(issued.device_name, "Mac Studio");
        assert_eq!(issued.principal.username, "admin");
        h.auth.mint_media_token(&issued.principal)?;
        let again = denied(
            h.auth
                .claim_pairing(&offer.code, device("x", "X"), ip(3))
                .await,
        )?;
        assert!(matches!(again, AuthError::Unauthenticated));
        let by_qr = denied(
            h.auth
                .claim_pairing(&offer.qr_secret, device("x", "X"), ip(3))
                .await,
        )?;
        assert!(matches!(by_qr, AuthError::Unauthenticated));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn offer_is_claimed_by_qr_secret() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let offer = h.auth.create_pairing_offer(admin, None).await?;
        let issued = h
            .auth
            .claim_pairing(&offer.qr_secret, device("phone", "Luke's iPhone"), ip(3))
            .await?;
        assert_eq!(issued.device_name, "Luke's iPhone");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn pairing_offer_dies_after_five_wrong_codes() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let offer = h.auth.create_pairing_offer(admin, None).await?;
        let wrong = wrong_code(&offer.code);
        for attempt in 0..5u8 {
            // Different addresses, so only the offer's own count is in play.
            let error = denied(
                h.auth
                    .claim_pairing(&wrong, device("x", "X"), ip(20 + attempt))
                    .await,
            )?;
            assert!(matches!(error, AuthError::Unauthenticated));
        }
        let error = denied(
            h.auth
                .claim_pairing(&offer.code, device("x", "X"), ip(40))
                .await,
        )?;
        assert!(matches!(error, AuthError::Unauthenticated));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn offer_expires_after_ten_minutes_and_can_be_cancelled() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mia = h.member(admin, "mia").await?;
        let offer = h.auth.create_pairing_offer(admin, None).await?;
        h.advance(Duration::from_secs(600));
        let error = denied(
            h.auth
                .claim_pairing(&offer.code, device("x", "X"), ip(3))
                .await,
        )?;
        assert!(matches!(error, AuthError::Unauthenticated));

        let offer = h.auth.create_pairing_offer(admin, None).await?;
        assert!(matches!(
            denied(h.auth.cancel_pairing_offer(mia, &offer.id).await)?,
            AuthError::NotFound
        ));
        h.auth.cancel_pairing_offer(admin, &offer.id).await?;
        let error = denied(
            h.auth
                .claim_pairing(&offer.code, device("x", "X"), ip(3))
                .await,
        )?;
        assert!(matches!(error, AuthError::Unauthenticated));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn guessing_pairing_codes_from_one_address_is_rate_limited() -> TestResult {
    let h = harness()?;
    let mut limited = false;
    for _ in 0..8 {
        if let Err(AuthError::RateLimited { .. }) = h
            .auth
            .claim_pairing("ABCD-EFGH", device("x", "X"), ip(50))
            .await
        {
            limited = true;
        }
    }
    assert!(limited);
    Ok(())
}

#[tokio::test]
async fn pair_request_returns_the_key_exactly_once() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let request = h
            .auth
            .start_pair_request(device("living-room", "Living Room Mac"), ip(7))
            .await?;
        assert_eq!(request.code.len(), 7);
        assert!(matches!(
            h.auth
                .poll_pair_request(&request.id, &request.poll_secret)
                .await?,
            PairPoll::Pending
        ));
        assert!(matches!(
            denied(h.auth.poll_pair_request(&request.id, "wrong secret").await)?,
            AuthError::NotFound
        ));
        let approved = h.auth.approve_pair_request(admin, &request.code).await?;
        assert_eq!(approved.device_name.as_deref(), Some("Living Room Mac"));
        assert_eq!(approved.platform.as_deref(), Some("ios"));

        let PairPoll::Approved(issued) = h
            .auth
            .poll_pair_request(&request.id, &request.poll_secret)
            .await?
        else {
            return Err("expected the key".into());
        };
        assert_eq!(issued.principal.username, "admin");
        let (parts, ()) = http::Request::builder()
            .uri("/api/v1/me")
            .header("authorization", format!("Bearer {}", issued.secret))
            .body(())
            .unwrap_or_default()
            .into_parts();
        h.auth.authenticate(&parts, ip(7), RouteClass::Api).await?;
        h.auth.mint_media_token(&issued.principal)?;

        let again = denied(
            h.auth
                .poll_pair_request(&request.id, &request.poll_secret)
                .await,
        )?;
        assert!(matches!(again, AuthError::NotFound));
        let twice = denied(h.auth.approve_pair_request(admin, &request.code).await)?;
        assert!(matches!(twice, AuthError::NotFound));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn pair_request_expires_unapproved() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let request = h.auth.start_pair_request(device("x", "X"), ip(7)).await?;
        h.advance(Duration::from_secs(600));
        assert!(matches!(
            h.auth
                .poll_pair_request(&request.id, &request.poll_secret)
                .await?,
            PairPoll::Expired
        ));
        let error = denied(h.auth.approve_pair_request(admin, &request.code).await)?;
        assert!(matches!(error, AuthError::NotFound));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn five_wrong_approval_codes_then_rate_limited() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let mia = h.member(admin, "mia").await?;
        let request = h.auth.start_pair_request(device("x", "X"), ip(7)).await?;
        let digits: String = request.code.chars().filter(char::is_ascii_digit).collect();
        let wrong = if digits == "000000" {
            "000001"
        } else {
            "000000"
        };
        for _ in 0..5 {
            assert!(matches!(
                denied(h.auth.approve_pair_request(admin, wrong).await)?,
                AuthError::NotFound
            ));
        }
        assert!(matches!(
            denied(h.auth.approve_pair_request(admin, &request.code).await)?,
            AuthError::RateLimited { .. }
        ));
        // Another signed-in user is not affected.
        h.auth.approve_pair_request(mia, &request.code).await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn approved_request_cannot_be_collected_after_the_secret_key_changes() -> TestResult {
    wp1(async {
        let h = harness()?;
        let admin = h.admin().await?;
        let request = h.auth.start_pair_request(device("x", "X"), ip(7)).await?;
        h.auth.approve_pair_request(admin, &request.code).await?;
        let restarted = h.second_auth([5; 32]);
        assert!(matches!(
            restarted
                .poll_pair_request(&request.id, &request.poll_secret)
                .await?,
            PairPoll::Expired
        ));
        Ok(())
    })
    .await
}
