//! Commands: correlation, chunked answers, error mapping, timeouts, the in-flight bound,
//! cancellation and what each typed method sends.

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use support::{FakeServer, wait_state};
use trackside_mass::models::{MediaType, QueueOption, RepeatMode};
use trackside_mass::{
    Client, ConnectionState, ErrorCode, OpenSubsonicSource, PlayMedia, PlayerCommand, RequestError,
    SearchQuery, SetupError, Token,
};

async fn connected(server: &mut FakeServer) -> (Client, support::FakeConn) {
    let client = Client::start(server.options());
    let conn = server.accept_ready().await;
    wait_state(&client, ConnectionState::is_connected).await;
    (client, conn)
}

#[tokio::test]
async fn answers_out_of_order_reach_the_right_caller() {
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;

    let first = tokio::spawn({
        let client = client.clone();
        async move { client.call::<String>("one", None).await }
    });
    let a = conn.recv().await;
    let second = tokio::spawn({
        let client = client.clone();
        async move { client.call::<String>("two", None).await }
    });
    let b = conn.recv().await;
    assert_ne!(a.id, b.id);
    conn.reply(&b.id, json!(b.command)).await;
    conn.reply(&a.id, json!(a.command)).await;

    assert_eq!(first.await.expect("joined").expect("answer"), "one");
    assert_eq!(second.await.expect("joined").expect("answer"), "two");
    client.shutdown().await;
}

#[tokio::test]
async fn partial_chunks_are_assembled_in_order() {
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;

    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call::<Vec<u32>>("music/tracks/library_items", None)
                .await
        }
    });
    let sent = conn.recv().await;
    conn.reply_partial(&sent.id, json!([1, 2, 3])).await;
    conn.reply_partial(&sent.id, json!([4, 5])).await;
    conn.reply(&sent.id, json!([6])).await;

    assert_eq!(
        call.await.expect("joined").expect("answer"),
        vec![1, 2, 3, 4, 5, 6]
    );
    client.shutdown().await;
}

#[tokio::test]
async fn a_chunked_answer_with_an_empty_last_chunk_is_complete() {
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.call::<Vec<u32>>("x", None).await }
    });
    let sent = conn.recv().await;
    conn.reply_partial(&sent.id, json!([1, 2])).await;
    conn.reply(&sent.id, json!([])).await;
    assert_eq!(call.await.expect("joined").expect("answer"), vec![1, 2]);
    client.shutdown().await;
}

#[tokio::test]
async fn an_answer_larger_than_the_limit_is_refused() {
    let mut server = FakeServer::start().await;
    let mut options = server.options();
    options.max_result_items = 4;
    let client = Client::start(options);
    let mut conn = server.accept_ready().await;
    wait_state(&client, ConnectionState::is_connected).await;

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.call::<Vec<u32>>("big", None).await }
    });
    let sent = conn.recv().await;
    conn.reply_partial(&sent.id, json!([1, 2, 3])).await;
    conn.reply_partial(&sent.id, json!([4, 5])).await;
    let error = call.await.expect("joined").expect_err("too large");
    assert!(
        matches!(error, RequestError::TooLarge { limit: 4, .. }),
        "{error:?}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn a_chunked_answer_over_its_byte_limit_is_refused() {
    let mut server = FakeServer::start().await;
    let mut options = server.options();
    options.max_result_bytes = 4096;
    let client = Client::start(options);
    let mut conn = server.accept_ready().await;
    wait_state(&client, ConnectionState::is_connected).await;

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.call::<Vec<String>>("big", None).await }
    });
    let sent = conn.recv().await;
    let chunk = json!(["x".repeat(1500)]);
    for _ in 0..3 {
        conn.reply_partial(&sent.id, chunk.clone()).await;
    }
    let error = call.await.expect("joined").expect_err("too many bytes");
    assert!(
        matches!(error, RequestError::TooManyBytes { limit: 4096, .. }),
        "{error:?}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn chunked_answers_together_stay_under_the_assembling_limit() {
    let mut server = FakeServer::start().await;
    let mut options = server.options();
    options.max_result_bytes = 1 << 20;
    options.max_assembling_bytes = 4096;
    let client = Client::start(options);
    let mut conn = server.accept_ready().await;
    wait_state(&client, ConnectionState::is_connected).await;

    let first = tokio::spawn({
        let client = client.clone();
        async move { client.call::<Vec<String>>("one", None).await }
    });
    let one = conn.recv().await;
    let second = tokio::spawn({
        let client = client.clone();
        async move { client.call::<Vec<String>>("two", None).await }
    });
    let two = conn.recv().await;
    let chunk = json!(["x".repeat(1500)]);
    conn.reply_partial(&one.id, chunk.clone()).await;
    conn.reply_partial(&one.id, chunk.clone()).await;
    conn.reply_partial(&two.id, chunk.clone()).await;
    let error = second
        .await
        .expect("joined")
        .expect_err("over the shared limit");
    assert!(
        matches!(error, RequestError::TooManyBytes { limit: 4096, .. }),
        "{error:?}"
    );
    conn.reply(&one.id, json!([])).await;
    assert_eq!(first.await.expect("joined").expect("answer").len(), 2);
    client.shutdown().await;
}

#[tokio::test]
async fn error_answers_keep_their_code_and_details() {
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.player("gone").await }
    });
    let sent = conn.recv().await;
    conn.reply_error(&sent.id, 10, "Player gone is not available")
        .await;

    let error = call.await.expect("joined").expect_err("error");
    let RequestError::Server(error) = error else {
        panic!("{error:?}")
    };
    assert_eq!(error.code, ErrorCode::PlayerUnavailable);
    assert_eq!(
        error.details.as_deref(),
        Some("Player gone is not available")
    );
    client.shutdown().await;
}

#[tokio::test]
async fn an_answer_of_the_wrong_shape_is_a_decode_error() {
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.server_info().await }
    });
    let sent = conn.recv().await;
    conn.reply(&sent.id, json!("not an object")).await;
    let error = call.await.expect("joined").expect_err("decode");
    assert!(matches!(error, RequestError::Decode { ref command, .. } if command == "info"));
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_request_times_out_and_frees_its_slot() {
    let mut server = FakeServer::start().await;
    let mut options = server.options();
    options.max_in_flight = 1;
    let client = Client::start(options);
    let mut conn = server.accept().await;
    conn.handshake().await;
    // The two snapshot requests share the single slot; answer them one at a time.
    conn.serve_snapshot(json!([]), json!([])).await;
    wait_state(&client, ConnectionState::is_connected).await;

    let error = client
        .call_with_timeout::<Value>("slow", None, Duration::from_secs(3))
        .await
        .expect_err("times out");
    assert!(matches!(error, RequestError::Timeout(t) if t == Duration::from_secs(3)));
    let _slow = conn.recv().await;

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.call::<u32>("next", None).await }
    });
    let sent = conn.recv().await;
    assert_eq!(sent.command, "next");
    conn.reply(&sent.id, json!(7)).await;
    assert_eq!(call.await.expect("joined").expect("answer"), 7);
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn requests_beyond_the_in_flight_bound_wait_and_then_give_up() {
    let mut server = FakeServer::start().await;
    let mut options = server.options();
    options.max_in_flight = 1;
    let client = Client::start(options);
    let mut conn = server.accept().await;
    conn.handshake().await;
    conn.serve_snapshot(json!([]), json!([])).await;
    wait_state(&client, ConnectionState::is_connected).await;

    let hung = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_with_timeout::<Value>("hung", None, Duration::from_secs(60))
                .await
        }
    });
    let _ = conn.recv().await;
    let error = client
        .call_with_timeout::<Value>("second", None, Duration::from_secs(1))
        .await
        .expect_err("busy");
    assert!(matches!(error, RequestError::Busy), "{error:?}");
    hung.abort();
    client.shutdown().await;
}

#[tokio::test]
async fn a_dropped_caller_does_not_disturb_later_requests() {
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;

    let abandoned = tokio::spawn({
        let client = client.clone();
        async move { client.call::<Value>("abandoned", None).await }
    });
    let first = conn.recv().await;
    abandoned.abort();
    let _ = abandoned.await;
    conn.reply(&first.id, json!("late")).await;

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.call::<String>("after", None).await }
    });
    let sent = conn.recv().await;
    conn.reply(&sent.id, json!("fine")).await;
    assert_eq!(call.await.expect("joined").expect("answer"), "fine");
    client.shutdown().await;
}

#[tokio::test]
async fn a_disconnect_mid_request_fails_it_as_lost() {
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.call::<Value>("players/all", None).await }
    });
    let _ = conn.recv().await;
    conn.close().await;
    let error = call.await.expect("joined").expect_err("lost");
    assert!(matches!(error, RequestError::ConnectionLost), "{error:?}");
    client.shutdown().await;
}

#[tokio::test]
async fn requests_while_disconnected_fail_fast() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options_without_retry());
    let mut conn = server.accept().await;
    conn.greet(65, 28).await;
    let auth = conn.expect_auth().await;
    conn.reply_error(&auth.id, 23, "Invalid or expired token")
        .await;
    wait_state(&client, |s| matches!(s, ConnectionState::AuthFailed { .. })).await;

    let started = std::time::Instant::now();
    let error = client.players().await.expect_err("not connected");
    assert!(matches!(error, RequestError::NotConnected), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
    client.shutdown().await;
}

#[tokio::test]
async fn shutdown_fails_pending_requests_and_later_ones() {
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.call::<Value>("pending", None).await }
    });
    let _ = conn.recv().await;
    client.shutdown().await;
    let error = call.await.expect("joined").expect_err("shut down");
    assert!(matches!(error, RequestError::ShutDown), "{error:?}");
    let error = client.players().await.expect_err("shut down");
    assert!(matches!(error, RequestError::ShutDown), "{error:?}");
}

/// Runs `call` against a fresh connection and returns what it sent, answering `answer`.
async fn sent_by<F, Fut, T>(call: F, answer: Value) -> (support::Sent, T)
where
    F: FnOnce(Client) -> Fut,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;
    let task = tokio::spawn(call(client.clone()));
    let sent = conn.recv().await;
    conn.reply(&sent.id, answer).await;
    let result = task.await.expect("joined");
    client.shutdown().await;
    (sent, result)
}

#[tokio::test]
async fn play_media_sends_uris_option_and_start_item() {
    let (sent, result) = sent_by(
        |c| async move {
            c.play_media(
                "kitchen",
                &PlayMedia::uri("spotify://album/abc")
                    .option(QueueOption::ReplaceNext)
                    .start_item("spotify://track/def"),
            )
            .await
        },
        Value::Null,
    )
    .await;
    result.expect("ok");
    assert_eq!(sent.command, "player_queues/play_media");
    assert_eq!(
        sent.args,
        json!({
            "queue_id": "kitchen",
            "media": ["spotify://album/abc"],
            "option": "replace_next",
            "radio_mode": false,
            "start_item": "spotify://track/def"
        })
    );
}

#[tokio::test]
async fn player_commands_use_the_server_argument_names() {
    let cases = [
        (
            PlayerCommand::Seek(42),
            "players/cmd/seek",
            json!({"player_id": "p", "position": 42}),
        ),
        (
            PlayerCommand::VolumeSet(30),
            "players/cmd/volume_set",
            json!({"player_id": "p", "volume_level": 30}),
        ),
        (
            PlayerCommand::VolumeMute(true),
            "players/cmd/volume_mute",
            json!({"player_id": "p", "muted": true}),
        ),
        (
            PlayerCommand::Power(false),
            "players/cmd/power",
            json!({"player_id": "p", "powered": false}),
        ),
        (
            PlayerCommand::Group {
                target: "leader".into(),
            },
            "players/cmd/group",
            json!({"player_id": "p", "target_player": "leader"}),
        ),
        (
            PlayerCommand::PlayPause,
            "players/cmd/play_pause",
            json!({"player_id": "p"}),
        ),
    ];
    for (command, name, args) in cases {
        let (sent, result) = sent_by(
            move |c| async move { c.player_command("p", command).await },
            Value::Null,
        )
        .await;
        result.expect("ok");
        assert_eq!(sent.command, name);
        assert_eq!(sent.args, args);
    }
}

#[tokio::test]
async fn a_volume_above_100_is_refused_before_sending() {
    let mut server = FakeServer::start().await;
    let (client, _conn) = connected(&mut server).await;
    let error = client
        .player_command("p", PlayerCommand::VolumeSet(101))
        .await
        .expect_err("refused");
    assert!(matches!(error, RequestError::InvalidArgument(_)));
    client.shutdown().await;
}

#[tokio::test]
async fn set_members_sends_only_non_empty_lists() {
    let (sent, result) = sent_by(
        |c| async move { c.player_set_members("leader", &["a".into()], &[]).await },
        Value::Null,
    )
    .await;
    result.expect("ok");
    assert_eq!(
        sent.args,
        json!({"target_player": "leader", "player_ids_to_add": ["a"]})
    );
}

#[tokio::test]
async fn search_sends_types_and_limit() {
    let (sent, result) = sent_by(
        |c| async move {
            c.search(&SearchQuery::new("daft punk").media_types([MediaType::Track, MediaType::Album]).limit(5))
                .await
        },
        json!({"tracks": [{"item_id": "1", "provider": "library", "name": "One More Time", "media_type": "track"}]}),
    )
    .await;
    assert_eq!(result.expect("ok").tracks[0].name, "One More Time");
    assert_eq!(
        sent.args,
        json!({"search_query": "daft punk", "limit": 5, "library_only": false, "media_types": ["track", "album"]})
    );
}

#[tokio::test]
async fn library_items_use_the_plural_command() {
    let (sent, result) = sent_by(
        |c| async move {
            c.library_items(
                &MediaType::Album,
                &trackside_mass::LibraryQuery::page(50, 100),
            )
            .await
        },
        json!([]),
    )
    .await;
    result.expect("ok");
    assert_eq!(sent.command, "music/albums/library_items");
    assert_eq!(sent.args, json!({"limit": 50, "offset": 100}));
}

#[tokio::test]
async fn queue_repeat_and_transfer_send_their_arguments() {
    let (sent, result) = sent_by(
        |c| async move { c.queue_repeat("q", &RepeatMode::All).await },
        Value::Null,
    )
    .await;
    result.expect("ok");
    assert_eq!(sent.args, json!({"queue_id": "q", "repeat_mode": "all"}));

    let (sent, result) = sent_by(
        |c| async move { c.queue_transfer("a", "b", Some(true)).await },
        Value::Null,
    )
    .await;
    result.expect("ok");
    assert_eq!(
        sent.args,
        json!({"source_queue_id": "a", "target_queue_id": "b", "auto_play": true})
    );
}

#[tokio::test]
async fn a_list_answer_drops_entries_that_do_not_decode() {
    let (_, result) = sent_by(
        |c| async move { c.players().await },
        json!([support::player("a", "A", 10), {"no": "player id"}, support::player("b", "B", 20)]),
    )
    .await;
    let players = result.expect("ok");
    assert_eq!(
        players
            .iter()
            .map(|p| p.player_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
}

fn source() -> OpenSubsonicSource {
    let mut source = OpenSubsonicSource::new("http://192.168.1.10", Token::new("tsk_scoped_key"));
    source.port = Some(4747);
    source.name = Some("Trackside".into());
    source
}

#[tokio::test]
async fn registering_opensubsonic_runs_the_setup_flow() {
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;
    let task = tokio::spawn({
        let client = client.clone();
        async move { client.register_opensubsonic(&source()).await }
    });

    let setup = conn.recv().await;
    assert_eq!(setup.command, "config/providers/setup");
    assert_eq!(setup.args, json!({"provider_domain": "opensubsonic"}));
    conn.reply(&setup.id, json!({"flow_id": "f1", "step_id": "user", "type": "form", "entries": [], "errors": {}, "last_step": true}))
        .await;

    let submit = conn.recv().await;
    assert_eq!(submit.command, "config/flows/submit");
    assert_eq!(
        submit.args,
        json!({"flow_id": "f1", "values": {
            "baseURL": "http://192.168.1.10", "api_key": "tsk_scoped_key", "enable_legacy_auth": false, "port": 4747
        }})
    );
    conn.reply(&submit.id, json!({"flow_id": "f1", "step_id": "finish", "type": "finish", "result": {"instance_id": "opensubsonic--x1"}}))
        .await;

    let save = conn.recv().await;
    assert_eq!(save.command, "config/providers/save");
    assert_eq!(
        save.args,
        json!({"provider_domain": "opensubsonic", "instance_id": "opensubsonic--x1", "values": {"name": "Trackside"}})
    );
    conn.reply(&save.id, json!({"type": "music", "domain": "opensubsonic", "instance_id": "opensubsonic--x1", "values": {}}))
        .await;

    assert_eq!(
        task.await.expect("joined").expect("registered"),
        "opensubsonic--x1"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn a_rejected_opensubsonic_setup_is_aborted() {
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;
    let task = tokio::spawn({
        let client = client.clone();
        async move { client.register_opensubsonic(&source()).await }
    });
    let setup = conn.recv().await;
    conn.reply(
        &setup.id,
        json!({"flow_id": "f2", "step_id": "user", "type": "form"}),
    )
    .await;
    let submit = conn.recv().await;
    conn.reply(&submit.id, json!({"flow_id": "f2", "step_id": "user", "type": "form", "errors": {"base": "Could not connect"}}))
        .await;
    let abort = conn.recv().await;
    assert_eq!(abort.command, "config/flows/abort");
    assert_eq!(abort.args, json!({"flow_id": "f2"}));
    conn.reply(&abort.id, Value::Null).await;

    let error = task.await.expect("joined").expect_err("rejected");
    assert!(
        matches!(error, SetupError::Rejected(ref e) if e.contains("Could not connect")),
        "{error:?}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn an_aborted_setup_reports_the_reason() {
    let mut server = FakeServer::start().await;
    let (client, mut conn) = connected(&mut server).await;
    let task = tokio::spawn({
        let client = client.clone();
        async move { client.register_opensubsonic(&source()).await }
    });
    let setup = conn.recv().await;
    conn.reply(&setup.id, json!({"flow_id": "f3", "step_id": "abort", "type": "abort", "reason": "missing_dependency"}))
        .await;
    let error = task.await.expect("joined").expect_err("aborted");
    assert!(
        matches!(error, SetupError::Aborted(ref r) if r == "missing_dependency"),
        "{error:?}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn recommendation_rows_drop_entries_that_do_not_decode() {
    let (sent, result) = sent_by(
        |c| async move { c.recommendations().await },
        json!([
            {"item_id": "made-for-you", "provider": "apple_music--x", "name": "Made for You", "media_type": "folder", "items": []},
            "nope",
            {"item_id": ["x"], "provider": "y", "name": "z"}
        ]),
    )
    .await;
    let rows = result.expect("ok");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Made for You");
    assert_eq!(sent.command, "music/recommendations");
    assert_eq!(sent.args, Value::Null);
}

#[tokio::test]
async fn recommendation_items_send_only_the_arguments_asked_for() {
    let (sent, result) = sent_by(
        |c| async move {
            c.recommendation_items("apple_music--x", "made-for-you", &[], false)
                .await
        },
        json!([{"item_id": "ra.1", "provider": "apple_music--x", "name": "Station", "media_type": "radio"}, 7]),
    )
    .await;
    assert_eq!(result.expect("ok").len(), 1);
    assert_eq!(sent.command, "music/recommendations/items");
    assert_eq!(
        sent.args,
        json!({"provider": "apple_music--x", "item_id": "made-for-you"})
    );

    let (sent, _) = sent_by(
        |c| async move {
            c.recommendation_items("recommendations", "in_progress", &["tidal--y".into()], true)
                .await
        },
        json!([]),
    )
    .await;
    assert_eq!(
        sent.args,
        json!({"provider": "recommendations", "item_id": "in_progress",
               "providers": ["tidal--y"], "hide_played": true})
    );
}
