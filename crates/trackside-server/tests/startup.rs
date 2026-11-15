//! The real server on ephemeral ports: startup, the data directory lock, health, the
//! dashboard, base paths and forwarded headers, the CLI, and graceful shutdown.
//!
//! Tests that sign in need WP1's repositories and WP4's auth; they are written against them
//! and run once both have landed.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use trackside_core::config::Secret;
use trackside_core::{Config, ConfigReport, Db};
use trackside_server::cli::{self, AdminCommand, Cli, Command};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn config(data: &Path) -> Config {
    let mut config = Config::defaults(data);
    config.server.listen = vec![SocketAddr::from(([127, 0, 0, 1], 0))];
    config.database.allow_network_fs = true;
    config.discovery.mdns = false;
    config.auth.argon2_memory = 1 << 16;
    config.auth.argon2_iterations = 1;
    config.server.shutdown_grace = Duration::from_secs(1);
    config
}

struct Running {
    address: SocketAddr,
    stop: CancellationToken,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Running {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    async fn stop(self) -> anyhow::Result<()> {
        self.stop.cancel();
        self.task.await?
    }
}

async fn start(config: Config) -> Result<Running, Box<dyn std::error::Error>> {
    let stop = CancellationToken::new();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(trackside_server::run_with(
        config,
        ConfigReport::default(),
        stop.clone(),
        move |addresses| {
            let _ = bound_tx.send(addresses.to_vec());
        },
    ));
    let addresses = bound_rx
        .await
        .map_err(|_| "the server stopped before listening")?;
    Ok(Running {
        address: addresses[0],
        stop,
        task,
    })
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

#[tokio::test]
async fn server_migrates_the_database_serves_healthz_and_stops() -> TestResult {
    let dir = tempfile::tempdir()?;
    let config = config(&dir.path().join("data"));
    let db_path = config.database.path.clone();
    let server = start(config).await?;
    let client = http();
    let health = client.get(server.url("/healthz")).send().await?;
    assert_eq!(health.status(), 200);
    assert_eq!(health.text().await?, "ok");
    let ready: serde_json::Value = client
        .get(server.url("/readyz"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(ready["status"], "ready");
    assert!(ready["checks"]["ffmpeg"].is_string(), "{ready}");
    server.stop().await?;
    assert_eq!(
        Db::inspect_schema_version(&db_path)?,
        Some(trackside_core::db::SCHEMA_VERSION)
    );
    Ok(())
}

#[tokio::test]
async fn zero_config_start_listens_and_prints_setup_token() -> TestResult {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("data");
    let server = start(config(&data)).await?;
    let client = http();
    let capabilities: serde_json::Value = client
        .get(server.url("/api/v1/capabilities"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(capabilities["product"], "trackside");
    assert_eq!(capabilities["setup_required"], true);
    let token = std::fs::read_to_string(data.join("setup-token"))?;
    assert_ne!(token.trim(), "");
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn dashboard_is_served_with_a_strict_csp_and_api_errors_are_json() -> TestResult {
    let dir = tempfile::tempdir()?;
    let server = start(config(&dir.path().join("data"))).await?;
    let client = http();
    let index = client.get(server.url("/")).send().await?;
    assert_eq!(index.status(), 200);
    let csp = index
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(csp.starts_with("default-src 'self'"), "{csp}");
    assert_eq!(
        index
            .headers()
            .get("x-frame-options")
            .and_then(|v| v.to_str().ok()),
        Some("DENY")
    );
    assert_eq!(
        index
            .headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    let setup = client.get(server.url("/setup")).send().await?;
    assert!(setup.text().await?.contains("<html"));
    let fixtures = client
        .get(server.url("/dev/fixtures/me.json"))
        .send()
        .await?;
    assert_eq!(fixtures.status(), 404);
    let missing = client.get(server.url("/api/v1/nothing")).send().await?;
    assert_eq!(missing.status(), 404);
    assert!(missing.headers().contains_key("x-request-id"));
    let body: serde_json::Value = missing.json().await?;
    assert_eq!(body["error"]["code"], "not_found");
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn second_server_on_same_data_dir_exits() -> TestResult {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("data");
    let first = start(config(&data)).await?;
    let second = trackside_server::run(
        config(&data),
        ConfigReport::default(),
        CancellationToken::new(),
    )
    .await;
    let error = second.err().ok_or("the second server started")?;
    assert!(
        error.to_string().contains("another Trackside server"),
        "{error}"
    );
    first.stop().await?;
    Ok(())
}

#[tokio::test]
async fn healthcheck_exits_nonzero_when_down() -> TestResult {
    let dir = tempfile::tempdir()?;
    let server = start(config(&dir.path().join("data"))).await?;
    let base = format!("http://{}", server.address);
    assert!(cli::healthcheck(&base).await);
    server.stop().await?;
    assert!(!cli::healthcheck(&base).await);
    Ok(())
}

#[tokio::test]
async fn restore_refuses_while_locked() -> TestResult {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("data");
    let config = config(&data);
    let db_path = config.database.path.clone();
    start(config.clone()).await?.stop().await?;
    let backup = dir.path().join("trackside-20261114T093000Z-manual.db");
    std::fs::copy(&db_path, &backup)?;
    let server = start(config).await?;
    let refused =
        trackside_server::backup::restore(&backup, &db_path, &data, trackside_core::Millis(0));
    assert!(refused.is_err());
    server.stop().await?;
    let aside =
        trackside_server::backup::restore(&backup, &db_path, &data, trackside_core::Millis(0))?;
    assert!(aside.exists());
    assert_eq!(
        Db::inspect_schema_version(&db_path)?,
        Some(trackside_core::db::SCHEMA_VERSION)
    );
    Ok(())
}

#[tokio::test]
async fn music_dir_creates_first_library_once() -> TestResult {
    let dir = tempfile::tempdir()?;
    let music = dir.path().join("music");
    std::fs::create_dir_all(&music)?;
    std::fs::write(music.join("01 Halo.flac"), b"not really audio")?;
    let data = dir.path().join("data");
    let mut first = config(&data);
    first.music_dir = Some(music.clone());
    start(first.clone()).await?.stop().await?;
    start(first).await?.stop().await?;
    let test = trackside_core::testing::TestDb::new()?;
    let db = Db::open(
        &config(&data).database.options(),
        test.events.clone(),
        test.clock.clone(),
    )?;
    let libraries = db
        .read(|c| trackside_core::repo::libraries::list(c, &trackside_core::Access::system()))
        .await?;
    assert_eq!(libraries.len(), 1);
    assert_eq!(libraries[0].name, "Music");
    Ok(())
}

#[tokio::test]
async fn shutdown_drains_and_checkpoints() -> TestResult {
    let dir = tempfile::tempdir()?;
    let config = config(&dir.path().join("data"));
    let wal = std::path::PathBuf::from(format!("{}-wal", config.database.path.display()));
    let grace = config.server.shutdown_grace;
    let server = start(config).await?;
    let client = http();
    let ready = client.get(server.url("/readyz")).send().await?;
    assert_eq!(ready.status(), 200);
    let started = std::time::Instant::now();
    server.stop().await?;
    assert!(
        started.elapsed() < grace + Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    assert!(
        std::fs::metadata(&wal).map_or(0, |m| m.len()) == 0,
        "the WAL was not checkpointed"
    );
    Ok(())
}

fn with_admin(data: &Path) -> Config {
    let mut config = config(data);
    config.auth.admin_username = Some(String::from("luke"));
    config.auth.admin_password = Some(Secret::new("correct horse battery"));
    config
}

#[tokio::test]
async fn forwarded_proto_and_base_path_produce_correct_absolute_urls() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut config = with_admin(&dir.path().join("data"));
    config.server.base_path = String::from("/music/");
    config.server.trusted_proxies = vec!["127.0.0.1/32".parse()?];
    let server = start(config).await?;
    let client = http();
    let capabilities: serde_json::Value = client
        .get(server.url("/music/api/v1/capabilities"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(capabilities["subsonic"]["path"], "/music/rest");
    assert_eq!(
        client.get(server.url("/healthz")).send().await?.status(),
        200
    );
    assert_eq!(
        client.get(server.url("/music/")).send().await?.status(),
        200
    );
    let login = serde_json::json!({"username": "luke", "password": "correct horse battery"});
    let secure = client
        .post(server.url("/music/api/v1/auth/session"))
        .header("x-forwarded-proto", "https")
        .json(&login)
        .send()
        .await?;
    assert_eq!(secure.status(), 200, "{:?}", secure.text().await);
    let cookie = secure
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        cookie.starts_with("__Host-trackside_session=") && cookie.contains("Secure"),
        "{cookie}"
    );
    let plain = client
        .post(server.url("/music/api/v1/auth/session"))
        .json(&login)
        .send()
        .await?;
    let cookie = plain
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(cookie.starts_with("trackside_session="), "{cookie}");
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn reset_password_works_while_server_runs() -> TestResult {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("data");
    let server = start(with_admin(&data)).await?;
    let configured = with_admin(&data);
    let code = cli_admin(
        &configured,
        AdminCommand::ResetPassword {
            username: String::from("luke"),
        },
    )
    .await?;
    assert_eq!(code, 0);
    let code = cli_admin(&configured, AdminCommand::ListUsers).await?;
    assert_eq!(code, 0);
    server.stop().await?;
    Ok(())
}

async fn cli_admin(config: &Config, command: AdminCommand) -> anyhow::Result<i32> {
    cli::main(Cli {
        config: None,
        data_dir: Some(config.data_dir.clone()),
        command: Some(Command::Admin(command)),
    })
    .await
}

#[tokio::test]
async fn database_on_network_fs_is_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data)?;
    let mut refused = config(&data);
    refused.database.allow_network_fs = false;
    trackside_core::testing::assume_network_filesystem(&data);
    let result =
        trackside_server::run(refused, ConfigReport::default(), CancellationToken::new()).await;
    trackside_core::testing::clear_hooks(&data);
    let error = result
        .err()
        .ok_or("the server started on a network filesystem")?;
    assert!(error.to_string().contains("network filesystem"), "{error}");
    Ok(())
}

#[tokio::test]
async fn crash_during_migration_leaves_previous_schema_and_the_next_start_retries() -> TestResult {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data)?;
    let config = config(&data);
    let db_path = config.database.path.clone();
    trackside_core::testing::crash_migration_of(&db_path);
    let crashed = trackside_server::run(
        config.clone(),
        ConfigReport::default(),
        CancellationToken::new(),
    )
    .await;
    trackside_core::testing::clear_hooks(&data);
    assert!(crashed.is_err(), "the migration hook did not fire");
    let version = Db::inspect_schema_version(&db_path)?;
    assert!(matches!(version, None | Some(0)), "{version:?}");
    start(config).await?.stop().await?;
    assert_eq!(
        Db::inspect_schema_version(&db_path)?,
        Some(trackside_core::db::SCHEMA_VERSION)
    );
    Ok(())
}
