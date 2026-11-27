//! The client against a scripted fake Sendspin server: proxy auth, the handshake and how each
//! failure shows, activation order, time sync, framing, streams, reconnects and goodbye.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;
use trackside_sendspin::testing::{
    FakeConn, FakeServer, HandshakeScript, TOKEN, collect, next_event,
};
use trackside_sendspin::{
    Backoff, Client, Config, Error, Event, FrameError, GoodbyeReason, Identity, NoiseError, Token,
};

fn config(server: &FakeServer, identity: Identity) -> Config {
    let mut config = Config::new(
        server.url(),
        Some(Token::new(TOKEN)),
        identity,
        "Luke's iPhone",
    );
    config.backoff = Backoff {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(50),
    };
    config.handshake_timeout = Duration::from_secs(5);
    config
}

struct Setup {
    server: FakeServer,
    client: Client,
    events: UnboundedReceiver<Event>,
    cancel: CancellationToken,
}

async fn start_with(edit: impl FnOnce(&mut Config)) -> Setup {
    let server = FakeServer::start().await;
    let mut config = config(&server, Identity::generate().expect("key"));
    edit(&mut config);
    let (handler, events) = collect();
    let cancel = CancellationToken::new();
    let client = Client::start(config, handler, cancel.clone());
    Setup {
        server,
        client,
        events,
        cancel,
    }
}

async fn start() -> Setup {
    start_with(|_| {}).await
}

async fn accept_authed(server: &mut FakeServer) -> FakeConn {
    let mut conn = server.accept().await;
    conn.expect_auth().await;
    conn.auth_ok().await;
    conn
}

async fn disconnected(events: &mut UnboundedReceiver<Event>) -> Error {
    match next_event(events, |e| matches!(e, Event::Disconnected { .. })).await {
        Event::Disconnected { error, .. } => error,
        _ => unreachable!(),
    }
}

async fn stopped(events: &mut UnboundedReceiver<Event>) -> Option<Error> {
    match next_event(events, |e| matches!(e, Event::Stopped { .. })).await {
        Event::Stopped { error } => error,
        _ => unreachable!(),
    }
}

/// Reads client messages for `wait`, answering pings, and returns them.
async fn drain(conn: &mut FakeConn, wait: Duration) -> Vec<Value> {
    let mut out = Vec::new();
    let _ = tokio::time::timeout(wait, async {
        while let Some(message) = conn.try_recv().await {
            out.push(message);
        }
    })
    .await;
    out
}

#[tokio::test]
async fn the_proxy_auth_carries_the_token_and_the_client_id() {
    let mut s = start().await;
    let mut conn = s.server.accept().await;
    let auth = conn.expect_auth().await;
    assert_eq!(auth["token"], TOKEN);
    assert_eq!(auth["client_id"], s.client.client_id());
    s.cancel.cancel();
}

#[tokio::test]
async fn a_good_handshake_connects_and_sends_the_hello() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    let init = conn.read_client_init_value().await;
    assert_eq!(
        init["payload"],
        json!({"client_id": s.client.client_id(), "version": 1, "suite": "25519_ChaChaPoly_SHA256"})
    );
    drop(conn);

    let mut conn = accept_authed(&mut s.server).await;
    let hello = conn.ready().await;
    let Event::Connected {
        server_id,
        server_name,
        generation,
        ..
    } = next_event(&mut s.events, |e| matches!(e, Event::Connected { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(server_id, conn.server.client_id());
    assert_eq!(server_name, "Music Assistant");
    assert_eq!(generation, 1);
    assert_eq!(hello["name"], "Luke's iPhone");
    assert_eq!(hello["device_info"]["product_name"], "Mobile Application");
    assert_eq!(hello["unpaired_access"]["enabled"], true);
    assert_eq!(
        hello["player@v1_support"]["supported_formats"],
        json!([{"codec": "flac", "channels": 2, "sample_rate": 44100, "bit_depth": 16}])
    );
    assert_eq!(hello["player@v1_support"]["buffer_capacity"], 1_048_576);
    s.cancel.cancel();
}

#[tokio::test]
async fn a_refused_token_stops_without_retrying() {
    let mut s = start().await;
    let mut conn = s.server.accept().await;
    conn.expect_auth().await;
    conn.refuse_auth("Invalid or expired token").await;
    assert_eq!(
        stopped(&mut s.events).await,
        Some(Error::Auth("Invalid or expired token".into()))
    );
    assert!(
        s.server
            .try_accept(Duration::from_millis(300))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn a_tampered_init_exchange_fails_silently() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    let script = HandshakeScript {
        tamper_prologue: true,
        ..HandshakeScript::default()
    };
    assert!(conn.handshake_with(&script).await.is_err());
    assert_eq!(
        disconnected(&mut s.events).await,
        Error::Noise(NoiseError::Handshake)
    );
    s.cancel.cancel();
}

#[tokio::test]
async fn a_server_with_another_key_fails_the_handshake() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    let script = HandshakeScript {
        wrong_static: true,
        ..HandshakeScript::default()
    };
    assert!(conn.handshake_with(&script).await.is_err());
    assert_eq!(
        disconnected(&mut s.events).await,
        Error::Noise(NoiseError::Handshake)
    );
    s.cancel.cancel();
}

#[tokio::test]
async fn a_pairing_psk_the_client_lacks_falls_back_to_the_sentinel() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    let script = HandshakeScript {
        message_1_payload:
            json!({"psk_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "psk_category": "lt"})
                .to_string()
                .into_bytes(),
        ..HandshakeScript::default()
    };
    conn.handshake_with(&script)
        .await
        .expect("sentinel fallback");
    s.cancel.cancel();
}

#[tokio::test]
async fn a_malformed_message_1_payload_fails_silently() {
    for payload in [
        b"not json".to_vec(),
        json!({"psk_id": "GFsV9tLaSQm9HcFWpKsgYQOr7wFTvNUtkmFwuVz3zoo", "psk_category": "xx"})
            .to_string()
            .into_bytes(),
        json!({"psk_id": "short", "psk_category": "sn"})
            .to_string()
            .into_bytes(),
    ] {
        let mut s = start().await;
        let mut conn = accept_authed(&mut s.server).await;
        let script = HandshakeScript {
            message_1_payload: payload,
            ..HandshakeScript::default()
        };
        assert!(conn.handshake_with(&script).await.is_err());
        assert_eq!(
            disconnected(&mut s.events).await,
            Error::Noise(NoiseError::Payload)
        );
        s.cancel.cancel();
    }
}

#[tokio::test]
async fn a_server_error_stops_the_client() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.read_client_init().await;
    conn.send_text(r#"{"type":"server/error","payload":{"reason":"unsupported_suite"}}"#)
        .await;
    assert_eq!(
        stopped(&mut s.events).await,
        Some(Error::Rejected("unsupported_suite".into()))
    );
}

#[tokio::test]
async fn nothing_but_the_hello_goes_out_before_activation() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.handshake().await;
    assert_eq!(
        drain(&mut conn, Duration::from_millis(200)).await,
        Vec::<Value>::new()
    );
    conn.send("server/hello", json!({"name": "MA"})).await;
    let early = drain(&mut conn, Duration::from_millis(300)).await;
    assert_eq!(early.len(), 1, "{early:?}");
    assert_eq!(early[0]["type"], "client/hello");
    s.cancel.cancel();
}

#[tokio::test]
async fn client_state_waits_for_the_player_role_and_a_measurement() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.handshake().await;
    conn.send("server/hello", json!({"name": "MA"})).await;
    conn.expect("client/hello").await;
    conn.send("server/activate", json!({"activities": []}))
        .await;

    // A guest is first held with no roles: time sync runs, client/state does not.
    let first = conn.recv().await;
    assert_eq!(first["type"], "client/time");
    conn.answer_time(&first["payload"]).await;
    let mut kinds = Vec::new();
    for _ in 0..7 {
        let message = conn.recv().await;
        conn.answer_time(&message["payload"]).await;
        kinds.push(message["type"].clone());
    }
    assert!(kinds.iter().all(|k| k == "client/time"), "{kinds:?}");
    let after_burst = drain(&mut conn, Duration::from_millis(200)).await;
    assert!(after_burst.iter().all(|m| m["type"] != "client/state"));

    conn.activate_player().await;
    let state = conn.expect("client/state").await;
    assert_eq!(
        state,
        json!({"available": true, "player": {"output_delay_ms": 4000, "required_lead_time_ms": 1000,
               "min_buffer_ms": 300, "supported_commands": []}})
    );
    let activated = next_event(&mut s.events, |e| {
        matches!(e, Event::Activated { player: true, .. })
    })
    .await;
    assert_eq!(
        activated,
        Event::Activated {
            player: true,
            playback: true
        }
    );
    s.cancel.cancel();
}

#[tokio::test]
async fn a_burst_sends_each_request_after_the_previous_reply() {
    let mut s = start_with(|c| c.time_reply_timeout = Duration::from_secs(3)).await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.handshake().await;
    conn.send("server/hello", json!({"name": "MA"})).await;
    conn.expect("client/hello").await;
    conn.activate_player().await;
    for _ in 0..8 {
        let request = conn.recv().await;
        assert_eq!(request["type"], "client/time");
        assert!(
            drain(&mut conn, Duration::from_millis(150))
                .await
                .is_empty(),
            "a second request before the reply"
        );
        conn.answer_time(&request["payload"]).await;
    }
    assert_eq!(conn.expect("client/state").await["available"], true);
    next_event(&mut s.events, |e| matches!(e, Event::Synced)).await;
    let mapped = s.client.clock().to_local_us(conn.server_now_us());
    let local = s.client.clock().now_us();
    assert!(
        mapped.is_some_and(|m| (m - local).abs() < 200_000),
        "{mapped:?} vs {local}"
    );
    s.cancel.cancel();
}

#[tokio::test]
async fn bursts_repeat_within_the_interval() {
    let mut s = start_with(|c| c.burst_interval = Duration::from_millis(400)).await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    let started = tokio::time::Instant::now();
    let mut requests = 0;
    while requests < 16 {
        let message = conn.recv().await;
        if message["type"] == "client/time" {
            requests += 1;
            conn.answer_time(&message["payload"]).await;
        }
    }
    assert!(started.elapsed() < Duration::from_secs(3));
    s.cancel.cancel();
}

#[tokio::test]
async fn an_unanswered_time_request_does_not_stall_the_burst() {
    let mut s = start_with(|c| c.time_reply_timeout = Duration::from_millis(100)).await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.handshake().await;
    conn.send("server/hello", json!({"name": "MA"})).await;
    conn.expect("client/hello").await;
    conn.activate_player().await;
    let ignored = conn.recv().await;
    assert_eq!(ignored["type"], "client/time");
    conn.expect("client/state").await;
    s.cancel.cancel();
}

async fn streaming(s: &mut Setup) -> FakeConn {
    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    conn.start_stream(b"fLaC-header").await;
    let Event::StreamStart { format, in_place } =
        next_event(&mut s.events, |e| matches!(e, Event::StreamStart { .. })).await
    else {
        unreachable!()
    };
    assert!(!in_place);
    assert_eq!(format.header_bytes().as_deref(), Some(&b"fLaC-header"[..]));
    conn
}

#[tokio::test]
async fn audio_clear_and_end_arrive_in_order() {
    let mut s = start().await;
    let mut conn = streaming(&mut s).await;
    conn.send_audio(10, b"one").await;
    conn.send(
        "stream/clear",
        json!({"server_transmitted": 1, "from_timestamp": 50}),
    )
    .await;
    conn.send_audio(50, b"two").await;
    conn.send(
        "stream/clear",
        json!({"server_transmitted": 2, "roles": ["visualizer"]}),
    )
    .await;
    conn.send("stream/clear", json!({"server_transmitted": 3}))
        .await;
    conn.start_stream(b"fLaC-header").await;
    conn.send("stream/end", json!({})).await;
    conn.send_audio(90, b"late").await;
    conn.send("server/hello", json!({"name": "ignored"})).await;
    conn.send(
        "stream/start",
        json!({"server_transmitted": 4, "player": {"codec": "flac",
        "sample_rate": 44100, "channels": 2, "bit_depth": 16}}),
    )
    .await;

    let mut seen = Vec::new();
    while seen.len() < 7 {
        let event = s.events.recv().await.expect("event");
        if !matches!(
            event,
            Event::Synced | Event::Activated { .. } | Event::Connected { .. }
        ) {
            seen.push(event);
        }
    }
    assert!(matches!(&seen[0], Event::Audio(c) if c.timestamp == 10 && c.data == "one"));
    assert_eq!(
        seen[1],
        Event::Clear {
            from_timestamp: Some(50)
        }
    );
    assert!(matches!(&seen[2], Event::Audio(c) if c.timestamp == 50));
    assert_eq!(
        seen[3],
        Event::Clear {
            from_timestamp: None
        }
    );
    assert!(matches!(
        &seen[4],
        Event::StreamStart { in_place: true, .. }
    ));
    assert_eq!(seen[5], Event::End);
    assert!(matches!(
        &seen[6],
        Event::StreamStart {
            in_place: false,
            ..
        }
    ));
    s.cancel.cancel();
}

#[tokio::test]
async fn fragmented_messages_are_reassembled() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    let header = vec![b'x'; 3000];
    conn.send_fragmented(
        "stream/start",
        json!({"server_transmitted": 1, "player": {"codec": "flac", "sample_rate": 44100,
               "channels": 2, "bit_depth": 16,
               "codec_header": base64_standard(&header)}}),
        700,
    )
    .await;
    let Event::StreamStart { format, .. } =
        next_event(&mut s.events, |e| matches!(e, Event::StreamStart { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(format.header_bytes(), Some(header));
    s.cancel.cancel();
}

fn base64_standard(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[tokio::test]
async fn a_malformed_fragment_sequence_closes() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    conn.send_plain(&[1, 0x00, b'x']).await;
    assert!(matches!(
        disconnected(&mut s.events).await,
        Error::Frame(FrameError::Fragment(_))
    ));
    s.cancel.cancel();
}

#[tokio::test]
async fn a_short_audio_chunk_closes() {
    let mut s = start().await;
    let mut conn = streaming(&mut s).await;
    conn.send_plain(&[4, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0]).await;
    assert_eq!(
        disconnected(&mut s.events).await,
        Error::Frame(FrameError::ShortAudio)
    );
    s.cancel.cancel();
}

#[tokio::test]
async fn unknown_messages_and_binary_ids_are_ignored() {
    let mut s = start().await;
    let mut conn = streaming(&mut s).await;
    conn.send("server/future", json!({"x": 1})).await;
    conn.send_plain(&[8, 1, 2, 3]).await;
    conn.send_plain(&[200]).await;
    conn.send_audio(5, b"still here").await;
    let event = next_event(&mut s.events, |e| matches!(e, Event::Audio(_))).await;
    assert!(matches!(event, Event::Audio(c) if c.timestamp == 5));
    s.cancel.cancel();
}

#[tokio::test]
async fn cleartext_after_the_handshake_closes() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    conn.send_text(r#"{"type":"server/hello","payload":{}}"#)
        .await;
    assert!(matches!(
        disconnected(&mut s.events).await,
        Error::Protocol(_)
    ));
    s.cancel.cancel();
}

#[tokio::test]
async fn a_garbled_transport_message_closes() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    conn.send_binary_raw(vec![9; 40]).await;
    assert_eq!(
        disconnected(&mut s.events).await,
        Error::Noise(NoiseError::Transport)
    );
    s.cancel.cancel();
}

#[tokio::test]
async fn a_malformed_group_id_closes() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    conn.send(
        "group/update",
        json!({"playback_state": "playing", "group_id": "nope", "group_name": "x"}),
    )
    .await;
    assert!(matches!(
        disconnected(&mut s.events).await,
        Error::Protocol(_)
    ));
    s.cancel.cancel();
}

#[tokio::test]
async fn pairing_is_declined_with_pair_abort() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    conn.send(
        "server/activate",
        json!({"activities": ["pairing"], "pairing": {"method": "pairing_psk"}}),
    )
    .await;
    assert_eq!(
        conn.expect("pair/abort").await,
        json!({"reason": "method_not_supported"})
    );
    s.cancel.cancel();
}

#[tokio::test]
async fn losing_the_player_role_ends_the_stream() {
    let mut s = start().await;
    let mut conn = streaming(&mut s).await;
    conn.send(
        "server/activate",
        json!({"activities": [], "active_roles": []}),
    )
    .await;
    next_event(&mut s.events, |e| matches!(e, Event::End)).await;
    s.cancel.cancel();
}

#[tokio::test]
async fn a_rehandshake_keeps_the_stream() {
    let mut s = start().await;
    let mut conn = streaming(&mut s).await;
    conn.rehandshake().await;
    conn.send("server/activate", json!({"activities": ["playback"]}))
        .await;
    conn.send_audio(77, b"after").await;
    let event = next_event(&mut s.events, |e| matches!(e, Event::Audio(_) | Event::End)).await;
    assert!(
        matches!(&event, Event::Audio(c) if c.timestamp == 77),
        "{event:?}"
    );
    s.cancel.cancel();
}

#[tokio::test]
async fn a_dropped_connection_comes_back_warm_with_the_same_client_id() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    let first_id = conn.client_id();
    conn.drop_connection();
    assert_eq!(disconnected(&mut s.events).await, Error::Closed);

    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    assert_eq!(conn.client_id(), first_id);
    let Event::Connected {
        generation, warm, ..
    } = next_event(&mut s.events, |e| {
        matches!(e, Event::Connected { generation: 2, .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!((generation, warm), (2, true));
    s.cancel.cancel();
}

#[tokio::test]
async fn a_drop_during_a_stream_ends_it() {
    let mut s = start().await;
    let conn = streaming(&mut s).await;
    conn.drop_connection();
    next_event(&mut s.events, |e| matches!(e, Event::End)).await;
    s.cancel.cancel();
}

#[tokio::test]
async fn closing_says_goodbye_with_the_reason() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    let (closed, ()) = tokio::join!(
        conn.expect("client/goodbye"),
        s.client.close(GoodbyeReason::UserRequest)
    );
    assert_eq!(closed, json!({"reason": "user_request"}));
    assert_eq!(stopped(&mut s.events).await, None);
}

#[tokio::test]
async fn shutdown_says_goodbye_shutdown() {
    let mut s = start().await;
    let mut conn = accept_authed(&mut s.server).await;
    conn.ready().await;
    s.cancel.cancel();
    assert_eq!(
        conn.expect("client/goodbye").await,
        json!({"reason": "shutdown"})
    );
    conn.closed().await;
    assert_eq!(stopped(&mut s.events).await, None);
}

#[tokio::test]
async fn the_client_gives_up_after_its_window() {
    let mut s = start_with(|c| {
        c.give_up_after = Duration::from_millis(200);
        c.auth_timeout = Duration::from_millis(100);
    })
    .await;
    for _ in 0..3 {
        if let Some(conn) = s.server.try_accept(Duration::from_millis(300)).await {
            conn.drop_connection();
        }
    }
    assert!(stopped(&mut s.events).await.is_some());
}
