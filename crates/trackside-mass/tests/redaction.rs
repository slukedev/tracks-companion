//! The token never shows in `Debug` output, states, errors or logs.

mod support;

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::json;
use support::{FakeServer, TOKEN, wait_state};
use trackside_mass::{Client, ConnectionState, OpenSubsonicSource, Token};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
    }
}

#[test]
fn options_and_tokens_redact_in_debug() {
    let server_options = trackside_mass::Options::new(
        trackside_mass::Endpoint::parse("ma.local").expect("endpoint"),
        Token::new(TOKEN),
    );
    let shown = format!("{server_options:?} {server_options:#?}");
    assert!(!shown.contains(TOKEN), "{shown}");
    let source = OpenSubsonicSource::new("http://ts", Token::new("tsk_secret_key"));
    assert!(!format!("{source:?}").contains("tsk_secret_key"));
}

#[tokio::test]
async fn nothing_logged_or_reported_carries_the_token() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut server = FakeServer::start().await;
    let mut options = server.options();
    options.auth_retry = std::time::Duration::from_secs(3600);
    let client = Client::start(options);
    let mut conn = server.accept_ready().await;
    wait_state(&client, ConnectionState::is_connected).await;

    let register = tokio::spawn({
        let client = client.clone();
        async move {
            let source = OpenSubsonicSource::new("http://ts", Token::new("tsk_secret_key"));
            client.register_opensubsonic(&source).await
        }
    });
    let setup = conn.recv().await;
    conn.reply(
        &setup.id,
        json!({"flow_id": "f", "step_id": "user", "type": "form"}),
    )
    .await;
    let submit = conn.recv().await;
    conn.reply_error(&submit.id, 0, "boom").await;
    let abort = conn.recv().await;
    assert_eq!(abort.command, "config/flows/abort");
    conn.reply(&abort.id, json!(null)).await;
    let error = register.await.expect("joined").expect_err("fails");

    conn.close().await;
    let mut conn = server.accept().await;
    conn.greet(65, 28).await;
    let auth = conn.expect_auth().await;
    conn.reply_error(&auth.id, 23, "Invalid or expired token")
        .await;
    let state = wait_state(&client, |s| matches!(s, ConnectionState::AuthFailed { .. })).await;
    client.shutdown().await;

    let reported = format!("{error} {error:?} {state:?} {client:?}");
    let logs = captured.text();
    assert!(logs.contains("connected to Music Assistant"), "{logs}");
    for secret in [TOKEN, "tsk_secret_key"] {
        assert!(!reported.contains(secret), "{reported}");
        assert!(!logs.contains(secret), "{logs}");
    }
}
