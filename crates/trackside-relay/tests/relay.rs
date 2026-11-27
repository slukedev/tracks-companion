//! The relay against the fake Sendspin server: epochs, the publication rule, clears, caps, a
//! slow reader, idle teardown, limits and reconnects.

mod support;

use std::time::Duration;

use serde_json::json;
use support::{FRAME_US, connect, eventually, request, rig, send_frames, tone};
use trackside_relay::{EndReason, FetchError, RelayError, SessionState, Tier};

#[tokio::test]
async fn segments_appear_only_once_their_end_minus_the_output_delay_has_passed() {
    let mut rig = rig(|c| c.first_segment_wait = Duration::from_millis(300)).await;
    let opened = rig
        .manager
        .open(request("phone", Tier::Lossless))
        .await
        .expect("open");
    assert!(opened.created);
    let session = opened.session;
    assert_eq!(
        session.info().hls.as_deref(),
        Some("/api/v1/relay/h-phone/1/master.m3u8"),
        "the first epoch's URL works before any audio"
    );
    let mut conn = connect(&mut rig.server).await;
    let early_master = {
        let session = std::sync::Arc::clone(&session);
        tokio::spawn(async move { session.master_playlist(1, None).await })
    };
    let (header, frames) = tone();
    conn.start_stream(&header).await;
    assert!(early_master.await.expect("task").is_ok());
    // The first segment (ten frames, 1.045 s) is audible 5 s from now: published at about 2 s.
    let start = conn.server_now_us() + 1_000_000 + 4_000_000 - 1_044_897 + 1_000_000;
    send_frames(&mut conn, start, &frames[..30]).await;
    eventually(|| session.info().generation == 1).await;
    let info = session.info();
    assert_eq!(info.state, SessionState::Playing);
    assert_eq!(
        info.hls.as_deref(),
        Some("/api/v1/relay/h-phone/1/master.m3u8")
    );

    let early = session
        .media_playlist(1, Some("mt=x"))
        .await
        .expect("playlist");
    assert!(!early.contains("#EXTINF"), "{early}");
    let opened_at = tokio::time::Instant::now();
    let mut playlist = String::new();
    while !playlist.contains("#EXTINF") {
        tokio::time::sleep(Duration::from_millis(25)).await;
        playlist = session
            .media_playlist(1, Some("mt=x"))
            .await
            .expect("playlist");
    }
    let waited = opened_at.elapsed();
    assert!(waited >= Duration::from_millis(1500), "{waited:?}");
    assert!(playlist.contains("0.m4s?mt=x"), "{playlist}");
    assert!(playlist.contains("#EXT-X-PROGRAM-DATE-TIME:"), "{playlist}");

    let master = session
        .master_playlist(1, Some("mt=x"))
        .await
        .expect("master");
    assert!(master.contains("CODECS=\"fLaC\""), "{master}");
    let init = session.init_segment(1).await.expect("init");
    assert_eq!(&init[4..8], b"ftyp");
    let segment = session.segment(1, 0).await.expect("segment");
    assert_eq!(&segment[4..8], b"moof");
    rig.cancel.cancel();
}

async fn playing(
    rig: &mut support::Rig,
) -> (
    std::sync::Arc<trackside_relay::RelaySession>,
    trackside_sendspin::testing::FakeConn,
) {
    let session = rig
        .manager
        .open(request("phone", Tier::Lossless))
        .await
        .expect("open")
        .session;
    let mut conn = connect(&mut rig.server).await;
    let (header, _) = tone();
    conn.start_stream(&header).await;
    eventually(|| session.info().generation == 1).await;
    (session, conn)
}

#[tokio::test]
async fn a_clear_starts_a_new_epoch_and_ends_the_old_playlist() {
    let mut rig = rig(|_| {}).await;
    let (session, mut conn) = playing(&mut rig).await;
    let (_, frames) = tone();
    let now = conn.server_now_us();
    send_frames(&mut conn, now - 10_000_000, &frames[..12]).await;
    eventually(|| {
        session
            .playlist_snapshot(1, None)
            .unwrap_or_default()
            .contains("#EXTINF")
    })
    .await;

    conn.send("stream/clear", json!({"server_transmitted": now}))
        .await;
    eventually(|| session.info().generation == 2).await;
    assert_eq!(
        session.info().hls.as_deref(),
        Some("/api/v1/relay/h-phone/2/master.m3u8")
    );
    let old = session.media_playlist(1, None).await.expect("old epoch");
    assert!(old.ends_with("#EXT-X-ENDLIST\n"), "{old}");
    send_frames(&mut conn, now - 5_000_000, &frames[12..24]).await;
    let new = session.media_playlist(2, None).await.expect("new epoch");
    assert!(new.contains("#EXT-X-MEDIA-SEQUENCE:0\n"), "{new}");
    assert!(new.contains("0.m4s"), "{new}");
    assert!(!new.contains("ENDLIST"));
    rig.cancel.cancel();
}

#[tokio::test]
async fn a_clear_from_a_timestamp_keeps_the_epoch() {
    let mut rig = rig(|_| {}).await;
    let (session, mut conn) = playing(&mut rig).await;
    let (_, frames) = tone();
    let start = conn.server_now_us() + 3_000_000;
    send_frames(&mut conn, start, &frames[..30]).await;
    let from = start + 20 * FRAME_US;
    conn.send(
        "stream/clear",
        json!({"server_transmitted": 1, "from_timestamp": from}),
    )
    .await;
    send_frames(&mut conn, from, &frames[30..40]).await;
    eventually(|| {
        session
            .playlist_snapshot(1, None)
            .unwrap_or_default()
            .matches("#EXTINF")
            .count()
            >= 2
    })
    .await;
    assert_eq!(session.info().generation, 1);
    assert_eq!(session.violations(), 0);
    rig.cancel.cancel();
}

#[tokio::test]
async fn a_stream_end_pauses_and_ends_the_playlist() {
    let mut rig = rig(|_| {}).await;
    let (session, mut conn) = playing(&mut rig).await;
    conn.send("stream/end", json!({})).await;
    eventually(|| session.info().state == SessionState::Paused).await;
    assert_eq!(
        session.info().hls.as_deref(),
        Some("/api/v1/relay/h-phone/2/master.m3u8"),
        "paused, the URL is where the resume will play"
    );
    let playlist = session.media_playlist(1, None).await.expect("playlist");
    assert!(playlist.ends_with("#EXT-X-ENDLIST\n"), "{playlist}");
    assert_eq!(session.segment(1, 0).await, Err(FetchError::NotFound));
    rig.cancel.cancel();
}

#[tokio::test]
async fn a_server_over_its_buffer_is_capped() {
    let mut rig = rig(|c| {
        c.pending_cap = 200_000;
        c.published_cap = 100_000;
    })
    .await;
    let (session, mut conn) = playing(&mut rig).await;
    let (_, frames) = tone();
    // A minute of audio, all far in the future: none of it may be published yet.
    let start = conn.server_now_us() + 600_000_000;
    for round in 0..10 {
        send_frames(&mut conn, start + round * 57 * FRAME_US, &frames).await;
    }
    eventually(|| session.violations() > 0).await;
    assert!(session.held_bytes() <= 300_000, "{}", session.held_bytes());
    // And far in the past: published at once, but the ring stays under its cap.
    conn.send("stream/clear", json!({"server_transmitted": 1}))
        .await;
    let past = conn.server_now_us() - 600_000_000;
    for round in 0..10 {
        send_frames(&mut conn, past + round * 57 * FRAME_US, &frames).await;
    }
    eventually(|| {
        session
            .playlist_snapshot(2, None)
            .unwrap_or_default()
            .contains("#EXTINF")
    })
    .await;
    assert!(session.held_bytes() <= 300_000, "{}", session.held_bytes());
    rig.cancel.cancel();
}

#[tokio::test]
async fn a_waiting_reader_never_holds_up_the_stream() {
    let mut rig = rig(|c| c.segment_wait = Duration::from_secs(5)).await;
    let (session, mut conn) = playing(&mut rig).await;
    let reader = {
        let session = std::sync::Arc::clone(&session);
        tokio::spawn(async move { session.segment(1, 1).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    conn.send("stream/clear", json!({"server_transmitted": 1}))
        .await;
    let (_, frames) = tone();
    let past = conn.server_now_us() - 20_000_000;
    send_frames(&mut conn, past, &frames[..25]).await;
    eventually(|| {
        session
            .playlist_snapshot(2, None)
            .unwrap_or_default()
            .contains("1.m4s")
    })
    .await;
    assert_eq!(reader.await.expect("reader"), Err(FetchError::NotFound));
    rig.cancel.cancel();
}

#[tokio::test]
async fn nobody_fetching_pauses_then_the_session_ends_idle() {
    let mut rig = rig(|c| {
        c.fetch_timeout = Duration::from_millis(300);
        c.paused_timeout = Duration::from_millis(400);
    })
    .await;
    let (session, mut conn) = playing(&mut rig).await;
    let player = format!("pause {}", session.player_id());
    eventually(|| rig.control.calls().contains(&player)).await;
    conn.send("stream/end", json!({})).await;
    assert_eq!(
        conn.expect("client/goodbye").await,
        json!({"reason": "user_request"})
    );
    let info = session.info();
    assert_eq!(info.state, SessionState::Ended);
    assert_eq!(info.ended_reason, Some(EndReason::Idle));
    assert_eq!(info.hls, None);
    assert_eq!(rig.manager.active(), 0);
}

#[tokio::test]
async fn fetching_keeps_a_playing_session_going() {
    let mut rig = rig(|c| c.fetch_timeout = Duration::from_millis(300)).await;
    let (session, _conn) = playing(&mut rig).await;
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = session.master_playlist(1, None).await;
    }
    assert!(rig.control.calls().is_empty(), "{:?}", rig.control.calls());
    rig.cancel.cancel();
}

#[tokio::test]
async fn one_session_per_device_and_limits_per_server_and_user() {
    let mut rig = rig(|c| {
        c.max_sessions = 2;
        c.max_per_user = 1;
    })
    .await;
    let first = rig
        .manager
        .open(request("a", Tier::Lossless))
        .await
        .expect("a");
    let again = rig
        .manager
        .open(request("a", Tier::Lossless))
        .await
        .expect("a again");
    assert!(!again.created);
    assert_eq!(first.session.id(), again.session.id());
    assert_eq!(
        rig.manager.open(request("b", Tier::Lossless)).await.err(),
        Some(RelayError::Limit { limit: 1 })
    );
    let mut other = request("c", Tier::Lossless);
    other.user = "sam".into();
    rig.manager.open(other).await.expect("another user");
    let mut third = request("d", Tier::Lossless);
    third.user = "kim".into();
    assert_eq!(
        rig.manager.open(third).await.err(),
        Some(RelayError::Limit { limit: 2 })
    );
    assert_eq!(
        rig.manager.open(request("a", Tier::Cellular)).await.err(),
        Some(RelayError::Unsupported("cellular"))
    );
    let _ = rig.server.accept().await;
    rig.cancel.cancel();
}

#[tokio::test]
async fn a_device_keeps_its_player_id_across_sessions() {
    let mut rig = rig(|_| {}).await;
    let first = rig
        .manager
        .open(request("a", Tier::Lossless))
        .await
        .expect("a");
    let player = first.session.player_id().to_owned();
    let mut conn = connect(&mut rig.server).await;
    assert_eq!(conn.client_id(), player);
    let (goodbye, ()) = tokio::join!(
        conn.expect("client/goodbye"),
        first.session.close(EndReason::Closed)
    );
    assert_eq!(goodbye, json!({"reason": "user_request"}));
    let second = rig
        .manager
        .open(request("a", Tier::Lossless))
        .await
        .expect("again");
    assert!(second.created);
    assert_eq!(second.session.player_id(), player);
    rig.cancel.cancel();
}

#[tokio::test]
async fn a_drop_while_playing_reconnects_and_resumes() {
    let mut rig = rig(|_| {}).await;
    let (session, conn) = playing(&mut rig).await;
    conn.drop_connection();
    eventually(|| session.info().state == SessionState::Paused).await;
    let _conn = connect(&mut rig.server).await;
    let player = format!("resume {}", session.player_id());
    eventually(|| rig.control.calls().contains(&player)).await;
    rig.cancel.cancel();
}

#[tokio::test]
async fn shutdown_says_goodbye_shutdown() {
    let mut rig = rig(|_| {}).await;
    let (session, mut conn) = playing(&mut rig).await;
    rig.cancel.cancel();
    assert_eq!(
        conn.expect("client/goodbye").await,
        json!({"reason": "shutdown"})
    );
    eventually(|| session.info().ended_reason == Some(EndReason::ServerRestart)).await;
}
