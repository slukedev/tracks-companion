//! The handshake: greeting, schema check and authentication, and how each failure shows.

mod support;

use std::time::Duration;

use serde_json::json;
use support::{FakeServer, TOKEN, wait_state};
use trackside_mass::{
    Client, ConnectError, ConnectionState, ErrorCode, Mismatch, Options, Stage, Token, probe,
};

#[tokio::test]
async fn a_good_handshake_reports_the_server_version_and_schema() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let _conn = server.accept_ready().await;

    let state = wait_state(&client, ConnectionState::is_connected).await;
    let ConnectionState::Connected {
        server_version,
        schema,
        session,
    } = state
    else {
        panic!("not connected")
    };
    assert_eq!(server_version, "2.10.13");
    assert_eq!(schema, 65);
    assert_eq!(
        session.user.as_ref().map(|u| u.username.as_str()),
        Some("trackside")
    );
    assert_eq!(session.generation, 1);
    client.shutdown().await;
}

#[tokio::test]
async fn the_configured_token_is_sent_in_the_auth_command() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut conn = server.accept().await;
    conn.greet(128, 28).await;
    let auth = conn.expect_auth().await;
    assert_eq!(auth.args, json!({"token": TOKEN}));
    client.shutdown().await;
}

#[tokio::test]
async fn an_invalid_token_shows_as_auth_failed() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut conn = server.accept().await;
    conn.greet(65, 28).await;
    let auth = conn.expect_auth().await;
    conn.reply_error(&auth.id, 23, "Invalid or expired token")
        .await;

    let state = wait_state(&client, |s| matches!(s, ConnectionState::AuthFailed { .. })).await;
    let ConnectionState::AuthFailed { error, .. } = state else {
        unreachable!()
    };
    assert!(error.is_invalid_token());
    assert_eq!(error.code, ErrorCode::InvalidToken);
    client.shutdown().await;
}

#[tokio::test]
async fn a_new_token_reconnects_after_an_auth_failure() {
    let mut server = FakeServer::start().await;
    let mut options = server.options();
    options.auth_retry = Duration::from_secs(3600);
    let client = Client::start(options);
    let mut conn = server.accept().await;
    conn.greet(65, 28).await;
    let auth = conn.expect_auth().await;
    conn.reply_error(&auth.id, 23, "Invalid or expired token")
        .await;
    wait_state(&client, |s| matches!(s, ConnectionState::AuthFailed { .. })).await;

    client.set_token(Token::new("fresh-token"));
    let mut conn = server.accept().await;
    conn.greet(65, 28).await;
    let auth = conn.expect_auth().await;
    assert_eq!(auth.args["token"], "fresh-token");
    client.shutdown().await;
}

#[tokio::test]
async fn a_server_older_than_2_10_is_incompatible() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut conn = server.accept().await;
    conn.greet(29, 28).await;

    let state = wait_state(&client, |s| {
        matches!(s, ConnectionState::Incompatible { .. })
    })
    .await;
    let ConnectionState::Incompatible { error, .. } = state else {
        unreachable!()
    };
    assert_eq!(error.mismatch, Mismatch::ServerTooOld);
    client.shutdown().await;
}

#[tokio::test]
async fn a_server_that_dropped_our_schema_is_incompatible_and_never_sees_the_token() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut conn = server.accept().await;
    conn.greet(300, 200).await;

    let state = wait_state(&client, |s| {
        matches!(s, ConnectionState::Incompatible { .. })
    })
    .await;
    let ConnectionState::Incompatible { error, .. } = state else {
        unreachable!()
    };
    assert_eq!(error.mismatch, Mismatch::ClientTooOld);
    conn.closed().await;
    client.shutdown().await;
}

#[tokio::test]
async fn the_2_11_beta_schema_is_accepted() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut conn = server.accept().await;
    conn.greet(128, 28).await;
    let auth = conn.expect_auth().await;
    conn.accept_auth(&auth.id).await;
    let state = wait_state(&client, ConnectionState::is_connected).await;
    assert!(matches!(
        state,
        ConnectionState::Connected { schema: 128, .. }
    ));
    client.shutdown().await;
}

#[tokio::test]
async fn a_server_without_users_asks_for_setup() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options_without_retry());
    let mut conn = server.accept().await;
    conn.greet(65, 28).await;
    conn.send(json!({"message_id": "connection", "error_code": 503, "details": "Setup required"}))
        .await;

    let state = wait_state(&client, |s| {
        matches!(
            s,
            ConnectionState::Disconnected {
                error: Some(ConnectError::SetupRequired),
                ..
            }
        )
    })
    .await;
    assert!(matches!(
        state,
        ConnectionState::Disconnected {
            retry_in: Some(_),
            ..
        }
    ));
    client.shutdown().await;
}

#[tokio::test]
async fn a_first_message_that_is_not_the_greeting_is_a_protocol_error() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options_without_retry());
    let mut conn = server.accept().await;
    conn.send(json!({"event": "player_updated", "object_id": "p", "data": {}}))
        .await;
    wait_state(&client, |s| {
        matches!(
            s,
            ConnectionState::Disconnected {
                error: Some(ConnectError::Protocol(_)),
                ..
            }
        )
    })
    .await;
    client.shutdown().await;
}

#[tokio::test]
async fn an_unreachable_server_is_a_network_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    let endpoint = trackside_mass::Endpoint::parse(&format!("http://{addr}")).expect("endpoint");
    let mut options = Options::new(endpoint, Token::new(TOKEN));
    options.backoff.initial = Duration::from_secs(3600);
    let client = Client::start(options);
    wait_state(&client, |s| {
        matches!(
            s,
            ConnectionState::Disconnected {
                error: Some(ConnectError::Network(_)),
                ..
            }
        )
    })
    .await;
    client.shutdown().await;
}

#[tokio::test]
async fn an_empty_token_is_never_sent() {
    let server = FakeServer::start().await;
    let mut options = server.options_without_retry();
    options.token = Token::new("   ");
    let client = Client::start(options);
    wait_state(&client, |s| {
        matches!(
            s,
            ConnectionState::Disconnected {
                error: Some(ConnectError::NoToken),
                ..
            }
        )
    })
    .await;
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_server_that_never_greets_times_out() {
    let mut server = FakeServer::start().await;
    let mut options = server.options_without_retry();
    options.handshake_timeout = Duration::from_secs(2);
    let client = Client::start(options);
    let _conn = server.accept().await;
    wait_state(&client, |s| {
        matches!(
            s,
            ConnectionState::Disconnected {
                error: Some(ConnectError::Timeout(Stage::Greeting)),
                ..
            }
        )
    })
    .await;
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_server_that_never_answers_auth_times_out() {
    let mut server = FakeServer::start().await;
    let mut options = server.options_without_retry();
    options.handshake_timeout = Duration::from_secs(2);
    let client = Client::start(options);
    let mut conn = server.accept().await;
    conn.greet(65, 28).await;
    let _ = conn.expect_auth().await;
    wait_state(&client, |s| {
        matches!(
            s,
            ConnectionState::Disconnected {
                error: Some(ConnectError::Timeout(Stage::Auth)),
                ..
            }
        )
    })
    .await;
    client.shutdown().await;
}

#[tokio::test]
async fn probe_reports_the_session_and_closes() {
    let mut server = FakeServer::start().await;
    let options = server.options();
    let probing = tokio::spawn(async move { probe(&options).await });
    let mut conn = server.accept().await;
    conn.handshake().await;
    let session = probing.await.expect("joined").expect("probe succeeds");
    assert_eq!(session.server.server_version, "2.10.13");
    conn.closed().await;
}

#[tokio::test]
async fn probe_reports_a_refused_token() {
    let mut server = FakeServer::start().await;
    let options = server.options();
    let probing = tokio::spawn(async move { probe(&options).await });
    let mut conn = server.accept().await;
    conn.greet(65, 28).await;
    let auth = conn.expect_auth().await;
    conn.reply_error(&auth.id, 21, "Authentication failed")
        .await;
    let error = probing.await.expect("joined").expect_err("refused");
    assert!(matches!(error, ConnectError::Auth(a) if a.code == ErrorCode::AuthenticationFailed));
}

#[tokio::test]
async fn shutdown_ends_in_stopped() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut conn = server.accept_ready().await;
    client.shutdown().await;
    assert_eq!(client.current_state(), ConnectionState::Stopped);
    conn.closed().await;
}

#[tokio::test]
async fn dropping_the_last_client_closes_the_connection() {
    let mut server = FakeServer::start().await;
    let client = Client::start(server.options());
    let mut conn = server.accept_ready().await;
    drop(client);
    conn.closed().await;
}
