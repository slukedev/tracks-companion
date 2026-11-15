//! The `trackside` command line.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use trackside_auth::{Auth, SecretKey};
use trackside_core::config::{
    ConfigError, ConfigSources, LogFormat, TlsMode, default_config_file, default_data_dir,
    load_config, render_show,
};
use trackside_core::repo::users::{self, UserPatch};
use trackside_core::{Clock, Config, ConfigReport, Db, EventBus, Role, SystemClock};

use crate::backup::{BackupKind, Backups};

#[derive(Debug, Parser)]
#[command(
    name = "trackside",
    version,
    about = "Trackside, a self-hosted music server for the Tracks app"
)]
pub struct Cli {
    /// The configuration file (default: <data dir>/config.toml when it exists).
    #[arg(long, global = true, env = "TRACKSIDE_CONFIG")]
    pub config: Option<PathBuf>,
    /// The data directory.
    #[arg(long, global = true, env = "TRACKSIDE_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the server (the default).
    Serve,
    /// Manage accounts on this host, also while the server runs.
    #[command(subcommand)]
    Admin(AdminCommand),
    /// Make, list and restore database backups.
    #[command(subcommand)]
    Backup(BackupCommand),
    /// Check the configuration, data directory and database without starting.
    Check,
    /// Exit 0 when the local server answers /healthz, 1 otherwise (for container health checks).
    Healthcheck {
        /// The base URL to check instead of the configured listener.
        #[arg(long)]
        url: Option<String>,
    },
    /// Show the effective configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Print the version.
    Version,
}

#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// Give a user a new temporary password and end their sessions.
    ResetPassword { username: String },
    /// Make a user an admin or a member.
    SetRole { username: String, role: String },
    /// List every user.
    ListUsers,
    /// Make a new setup token while no admin exists.
    CreateSetupToken,
}

#[derive(Debug, Subcommand)]
pub enum BackupCommand {
    /// Write a backup now.
    Create,
    /// List backups, newest first.
    List,
    /// Replace the database with a backup. The server must be stopped.
    Restore { file: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print every value with where it came from. Secrets are redacted.
    Show,
}

/// Loads the configuration for the command line's data directory and file.
pub fn configuration(cli: &Cli) -> anyhow::Result<(Config, ConfigReport)> {
    configuration_with(cli, std::env::vars().collect())
}

fn configuration_with(
    cli: &Cli,
    env: Vec<(String, String)>,
) -> anyhow::Result<(Config, ConfigReport)> {
    let from_env = env
        .iter()
        .find(|(k, v)| k == "TRACKSIDE_DATA_DIR" && !v.is_empty())
        .map(|(_, v)| PathBuf::from(v));
    // clap fills `--data-dir` from the environment variable too; only a value that differs
    // from it was typed on the command line.
    let overrides = match &cli.data_dir {
        Some(dir) if from_env.as_ref() != Some(dir) => {
            vec![(String::from("data_dir"), dir.display().to_string())]
        }
        _ => Vec::new(),
    };
    let data_dir = cli
        .data_dir
        .clone()
        .or(from_env)
        .unwrap_or_else(default_data_dir);
    let file = cli
        .config
        .clone()
        .or_else(|| Some(default_config_file(&data_dir)).filter(|p| p.exists()));
    let sources = ConfigSources {
        file,
        env,
        overrides,
    };
    match load_config(&sources) {
        Ok(loaded) => Ok(loaded),
        Err(ConfigError::Unimplemented(_)) => {
            Ok((Config::defaults(data_dir), ConfigReport::default()))
        }
        Err(e) => Err(e.into()),
    }
}

/// Logs as JSON when not on a terminal, unless `log.format` says otherwise.
pub fn init_logging(config: &Config) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log.level));
    let json = match config.log.format {
        Some(LogFormat::Json) => true,
        Some(LogFormat::Pretty) => false,
        None => !std::io::stderr().is_terminal(),
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    let _ = if json {
        builder.json().try_init()
    } else {
        builder.try_init()
    };
}

async fn wait_for_signal(shutdown: CancellationToken) {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for SIGTERM");
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
    shutdown.cancel();
}

fn open(config: &Config) -> anyhow::Result<Db> {
    crate::run::open_database(config, EventBus::default(), Arc::new(SystemClock))
}

fn auth(config: &Config, db: &Db) -> anyhow::Result<Auth> {
    let (secret, _) =
        SecretKey::load_or_create(&config.auth.secret_key_file).with_context(|| {
            format!(
                "cannot read the secret key {}",
                config.auth.secret_key_file.display()
            )
        })?;
    Ok(Auth::new(
        db.clone(),
        &config.auth,
        secret,
        Vec::new(),
        Arc::new(SystemClock),
    ))
}

/// The URL `healthcheck` asks: the first listener, on the loopback address.
pub fn healthcheck_url(config: &Config) -> String {
    let port = config
        .server
        .listen
        .first()
        .map_or(4747, std::net::SocketAddr::port);
    let scheme = if config.tls.mode == TlsMode::Off {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://127.0.0.1:{port}")
}

/// Whether `/healthz` at `base` answers `200 ok` within five seconds.
pub async fn healthcheck(base: &str) -> bool {
    crate::tls::install_provider();
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .danger_accept_invalid_certs(true)
        .build()
    else {
        return false;
    };
    match client
        .get(format!("{}/healthz", base.trim_end_matches('/')))
        .send()
        .await
    {
        Ok(response) => response.status() == reqwest::StatusCode::OK,
        Err(_) => false,
    }
}

/// Runs the command line. Returns the process exit code.
pub async fn main(cli: Cli) -> anyhow::Result<i32> {
    let (config, report) = configuration(&cli)?;
    let command = cli.command.unwrap_or(Command::Serve);
    match command {
        Command::Serve => {
            init_logging(&config);
            let shutdown = CancellationToken::new();
            tokio::spawn(wait_for_signal(shutdown.clone()));
            crate::run::run(config, report, shutdown).await?;
        }
        Command::Version => println!("trackside {}", env!("CARGO_PKG_VERSION")),
        Command::Healthcheck { url } => {
            let base = url.unwrap_or_else(|| healthcheck_url(&config));
            return Ok(i32::from(!healthcheck(&base).await));
        }
        Command::Config(ConfigCommand::Show) => {
            print!("{}", render_show(&config, &report));
        }
        Command::Check => return Ok(check(&config)),
        Command::Admin(admin) => admin_command(&config, admin).await?,
        Command::Backup(backup) => backup_command(&config, backup).await?,
    }
    Ok(0)
}

async fn admin_command(config: &Config, command: AdminCommand) -> anyhow::Result<()> {
    let db = open(config)?;
    let auth = auth(config, &db)?;
    match command {
        AdminCommand::ResetPassword { username } => {
            let password = auth
                .admin_reset_password(&username)
                .await
                .with_context(|| format!("cannot reset the password of {username}"))?;
            println!(
                "New temporary password for {username}: {}",
                password.as_str()
            );
            println!("They are asked to change it after signing in. Their sessions have ended.");
        }
        AdminCommand::SetRole { username, role } => {
            let role = Role::parse(&role).context("the role is admin or member")?;
            let name = username.clone();
            let user = db
                .read(move |c| users::by_username(c, &name))
                .await?
                .with_context(|| format!("no user named {username}"))?;
            let patch = UserPatch {
                role: Some(role),
                ..UserPatch::default()
            };
            auth.update_user(None, user.key, patch, None).await?;
            println!("{username} is now {}.", role.as_str());
        }
        AdminCommand::ListUsers => {
            for user in db.read(users::list).await? {
                let disabled = if user.disabled_at.is_some() {
                    " (disabled)"
                } else {
                    ""
                };
                println!(
                    "{}\t{}\t{}{disabled}",
                    user.username,
                    user.role.as_str(),
                    user.display_name
                );
            }
        }
        AdminCommand::CreateSetupToken => {
            let token = auth.rotate_setup_token(&config.data_dir).await?;
            let file = config.data_dir.join("setup-token");
            println!("{}", trackside_auth::setup_banner(&token, "/setup", &file));
        }
    }
    Ok(())
}

async fn backup_command(config: &Config, command: BackupCommand) -> anyhow::Result<()> {
    let backups = Backups::new(
        config.backup.dir.clone(),
        config
            .backup
            .include_secret_key
            .then(|| config.auth.secret_key_file.clone()),
        config.backup.keep,
    );
    match command {
        BackupCommand::Create => {
            let db = open(config)?;
            let now = db.clock().now();
            let path = tokio::task::spawn_blocking(move || {
                backups.create_blocking(&db, BackupKind::Manual, now)
            })
            .await??;
            println!("{}", path.display());
        }
        BackupCommand::List => {
            for backup in backups.list()? {
                println!(
                    "{}\t{}\t{}\t{} bytes",
                    backup.name,
                    backup.kind,
                    backup.at.0.to_rfc3339(),
                    backup.bytes
                );
            }
        }
        BackupCommand::Restore { file } => {
            let aside = crate::backup::restore(
                &file,
                &config.database.path,
                &config.data_dir,
                SystemClock.now(),
            )?;
            println!(
                "Restored {}. The previous database is at {}.",
                file.display(),
                aside.display()
            );
        }
    }
    Ok(())
}

fn check(config: &Config) -> i32 {
    let mut failed = false;
    let mut line = |ok: bool, what: String| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        failed |= !ok;
    };
    line(
        true,
        format!(
            "configuration loaded; data directory {}",
            config.data_dir.display()
        ),
    );
    let network = trackside_core::db::is_network_filesystem(
        config.database.path.parent().unwrap_or(&config.data_dir),
    )
    .unwrap_or(false);
    line(
        !network || config.database.allow_network_fs,
        format!(
            "database on a {} filesystem",
            if network { "network" } else { "local" }
        ),
    );
    match Db::inspect_schema_version(&config.database.path) {
        Ok(None) => line(
            true,
            String::from("no database yet; it is made at first start"),
        ),
        Ok(Some(v)) if v > trackside_core::db::SCHEMA_VERSION => {
            line(
                false,
                format!("database schema {v} is newer than this server knows"),
            );
        }
        Ok(Some(v)) => {
            line(
                true,
                format!(
                    "database schema {v} (this server knows {})",
                    trackside_core::db::SCHEMA_VERSION
                ),
            );
            match Db::check_file(&config.database.path, false) {
                Ok(problems) if problems.is_empty() => {
                    line(true, String::from("database integrity check passed"));
                }
                Ok(problems) => line(
                    false,
                    format!("database integrity check: {}", problems.join("; ")),
                ),
                Err(e) => line(
                    false,
                    format!("database integrity check could not run: {e}"),
                ),
            }
        }
        Err(e) => line(false, format!("database cannot be read: {e}")),
    }
    line(
        true,
        format!(
            "server {} ",
            if crate::lock::DataDirLock::is_held(&config.data_dir) {
                "is running"
            } else {
                "is not running"
            }
        ),
    );
    let ffmpeg =
        trackside_media::FfmpegTranscoder::detect(config.media.ffmpeg_path.as_deref()).is_some();
    line(
        true,
        format!(
            "ffmpeg {}",
            if ffmpeg {
                "found"
            } else {
                "not found (direct play only)"
            }
        ),
    );
    i32::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_subcommand_means_serve_and_admin_commands_parse() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["trackside"])?;
        assert!(cli.command.is_none());
        let cli = Cli::try_parse_from(["trackside", "admin", "set-role", "luke", "admin"])?;
        assert!(matches!(
            cli.command,
            Some(Command::Admin(AdminCommand::SetRole { .. }))
        ));
        let cli = Cli::try_parse_from([
            "trackside",
            "--data-dir",
            "/tmp/x",
            "backup",
            "restore",
            "a.db",
        ])?;
        assert_eq!(cli.data_dir, Some(PathBuf::from("/tmp/x")));
        assert!(Cli::try_parse_from(["trackside", "fly"]).is_err());
        Ok(())
    }

    #[test]
    fn a_data_dir_typed_on_the_command_line_is_reported_as_such()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().display().to_string();
        let typed = Cli::try_parse_from(["trackside", "--data-dir", &path, "config", "show"])?;
        let (config, report) = configuration_with(&typed, Vec::new())?;
        assert_eq!(config.data_dir, dir.path());
        let shown = render_show(&config, &report);
        assert!(
            shown.contains(&format!("data_dir = \"{path}\"  # command line"))
                || shown.contains(&format!("data_dir = {path}  # command line")),
            "{shown}"
        );
        assert!(!shown.contains("Config {"), "{shown}");

        let env = vec![(String::from("TRACKSIDE_DATA_DIR"), path.clone())];
        let from_env = Cli {
            data_dir: Some(dir.path().to_path_buf()),
            ..Cli::try_parse_from(["trackside", "config", "show"])?
        };
        let (_, report) = configuration_with(&from_env, env)?;
        assert_eq!(
            report.sources.get("data_dir"),
            Some(&trackside_core::config::ConfigSource::Env)
        );
        Ok(())
    }

    #[test]
    fn healthcheck_uses_the_first_listener_and_tls_mode() {
        let mut config = Config::defaults("/data");
        assert_eq!(healthcheck_url(&config), "http://127.0.0.1:4747");
        config.tls.mode = TlsMode::SelfSigned;
        assert_eq!(healthcheck_url(&config), "https://127.0.0.1:4747");
    }
}
