//! Layering of the configuration: built-in defaults, the TOML file, environment variables and
//! CLI overrides, with the source of every key and `config show`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use super::{Config, ConfigError, ConfigReport, ConfigSource, REDACTED, TlsMode, default_data_dir};

/// How a key's text from the environment or the command line becomes a TOML value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Text,
    Bool,
    Integer,
    Float,
    /// Comma-separated, or whitespace-separated when there is no comma.
    List,
    /// Comma-separated only: items may hold spaces.
    CommaList,
}

/// One configuration key of architecture section 11.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    /// Dotted, as in the TOML file.
    pub key: &'static str,
    /// The environment variable that sets it.
    pub env: &'static str,
    pub kind: ValueKind,
    /// Holds a secret: never shown, and also read from `<env>_FILE`.
    pub secret: bool,
    /// Also editable in the dashboard, unless set by the file or the environment.
    pub dashboard: bool,
}

const fn key(key: &'static str, env: &'static str, kind: ValueKind) -> KeyInfo {
    KeyInfo {
        key,
        env,
        kind,
        secret: false,
        dashboard: false,
    }
}

const fn dash(key: &'static str, env: &'static str, kind: ValueKind) -> KeyInfo {
    KeyInfo {
        key,
        env,
        kind,
        secret: false,
        dashboard: true,
    }
}

const fn secret(key: &'static str, env: &'static str) -> KeyInfo {
    KeyInfo {
        key,
        env,
        kind: ValueKind::Text,
        secret: true,
        dashboard: false,
    }
}

use ValueKind::{Bool, CommaList, Float, Integer, List, Text};

/// Every key, in the order of architecture section 11.
pub const KEYS: &[KeyInfo] = &[
    key("data_dir", "TRACKSIDE_DATA_DIR", Text),
    key("music_dir", "TRACKSIDE_MUSIC_DIR", Text),
    dash("server.name", "TRACKSIDE_SERVER__NAME", Text),
    key("server.listen", "TRACKSIDE_SERVER__LISTEN", List),
    key("server.base_path", "TRACKSIDE_SERVER__BASE_PATH", Text),
    dash("server.public_urls", "TRACKSIDE_SERVER__PUBLIC_URLS", List),
    key(
        "server.trusted_proxies",
        "TRACKSIDE_SERVER__TRUSTED_PROXIES",
        List,
    ),
    key(
        "server.local_networks",
        "TRACKSIDE_SERVER__LOCAL_NETWORKS",
        List,
    ),
    key(
        "server.tailscale_is_local",
        "TRACKSIDE_SERVER__TAILSCALE_IS_LOCAL",
        Bool,
    ),
    key(
        "server.max_connections",
        "TRACKSIDE_SERVER__MAX_CONNECTIONS",
        Integer,
    ),
    key(
        "server.header_timeout",
        "TRACKSIDE_SERVER__HEADER_TIMEOUT",
        Text,
    ),
    key(
        "server.request_timeout",
        "TRACKSIDE_SERVER__REQUEST_TIMEOUT",
        Text,
    ),
    key(
        "server.shutdown_grace",
        "TRACKSIDE_SERVER__SHUTDOWN_GRACE",
        Text,
    ),
    key("tls.mode", "TRACKSIDE_TLS__MODE", Text),
    key("tls.cert_file", "TRACKSIDE_TLS__CERT_FILE", Text),
    key("tls.key_file", "TRACKSIDE_TLS__KEY_FILE", Text),
    key("tls.acme_domains", "TRACKSIDE_TLS__ACME_DOMAINS", List),
    key("tls.acme_email", "TRACKSIDE_TLS__ACME_EMAIL", Text),
    key("tls.acme_directory", "TRACKSIDE_TLS__ACME_DIRECTORY", Text),
    key("tls.acme_cache", "TRACKSIDE_TLS__ACME_CACHE", Text),
    key("database.path", "TRACKSIDE_DATABASE__PATH", Text),
    key("database.readers", "TRACKSIDE_DATABASE__READERS", Integer),
    key("database.cache", "TRACKSIDE_DATABASE__CACHE", Text),
    key("database.mmap", "TRACKSIDE_DATABASE__MMAP", Text),
    key(
        "database.allow_network_fs",
        "TRACKSIDE_DATABASE__ALLOW_NETWORK_FS",
        Bool,
    ),
    key("cache_dir", "TRACKSIDE_CACHE_DIR", Text),
    key("scanner.threads", "TRACKSIDE_SCANNER__THREADS", Integer),
    dash("scanner.watch", "TRACKSIDE_SCANNER__WATCH", Bool),
    key(
        "scanner.watch_debounce",
        "TRACKSIDE_SCANNER__WATCH_DEBOUNCE",
        Text,
    ),
    dash(
        "scanner.quick_interval",
        "TRACKSIDE_SCANNER__QUICK_INTERVAL",
        Text,
    ),
    dash(
        "scanner.full_interval",
        "TRACKSIDE_SCANNER__FULL_INTERVAL",
        Text,
    ),
    key("scanner.settle", "TRACKSIDE_SCANNER__SETTLE", Text),
    key(
        "scanner.follow_symlinks",
        "TRACKSIDE_SCANNER__FOLLOW_SYMLINKS",
        Bool,
    ),
    dash(
        "scanner.missing_grace",
        "TRACKSIDE_SCANNER__MISSING_GRACE",
        Text,
    ),
    key(
        "scanner.empty_guard",
        "TRACKSIDE_SCANNER__EMPTY_GUARD",
        Float,
    ),
    key(
        "scanner.file_timeout",
        "TRACKSIDE_SCANNER__FILE_TIMEOUT",
        Text,
    ),
    key("scanner.ignore", "TRACKSIDE_SCANNER__IGNORE", List),
    key("scanner.extensions", "TRACKSIDE_SCANNER__EXTENSIONS", List),
    key("media.ffmpeg_path", "TRACKSIDE_MEDIA__FFMPEG_PATH", Text),
    key(
        "media.transcode_max_concurrent",
        "TRACKSIDE_MEDIA__TRANSCODE_MAX_CONCURRENT",
        Integer,
    ),
    dash(
        "media.transcode_cache_max",
        "TRACKSIDE_MEDIA__TRANSCODE_CACHE_MAX",
        Text,
    ),
    dash(
        "media.art_cache_max",
        "TRACKSIDE_MEDIA__ART_CACHE_MAX",
        Text,
    ),
    key(
        "media.cache_min_free",
        "TRACKSIDE_MEDIA__CACHE_MIN_FREE",
        Text,
    ),
    key("media.hls_segment", "TRACKSIDE_MEDIA__HLS_SEGMENT", Text),
    dash(
        "media.cellular_profile",
        "TRACKSIDE_MEDIA__CELLULAR_PROFILE",
        Text,
    ),
    key(
        "auth.secret_key_file",
        "TRACKSIDE_AUTH__SECRET_KEY_FILE",
        Text,
    ),
    dash(
        "auth.legacy_subsonic",
        "TRACKSIDE_AUTH__LEGACY_SUBSONIC",
        Bool,
    ),
    key("auth.session_idle", "TRACKSIDE_AUTH__SESSION_IDLE", Text),
    key("auth.session_max", "TRACKSIDE_AUTH__SESSION_MAX", Text),
    key(
        "auth.media_token_ttl",
        "TRACKSIDE_AUTH__MEDIA_TOKEN_TTL",
        Text,
    ),
    key("auth.argon2_memory", "TRACKSIDE_AUTH__ARGON2_MEMORY", Text),
    key(
        "auth.argon2_iterations",
        "TRACKSIDE_AUTH__ARGON2_ITERATIONS",
        Integer,
    ),
    key("auth.login_rate", "TRACKSIDE_AUTH__LOGIN_RATE", Text),
    key("auth.admin_username", "TRACKSIDE_ADMIN_USERNAME", Text),
    secret("auth.admin_password", "TRACKSIDE_ADMIN_PASSWORD"),
    key(
        "sync.tombstone_retention",
        "TRACKSIDE_SYNC__TOMBSTONE_RETENTION",
        Text,
    ),
    key("sync.max_page", "TRACKSIDE_SYNC__MAX_PAGE", Integer),
    dash("discovery.mdns", "TRACKSIDE_DISCOVERY__MDNS", Bool),
    key("backup.dir", "TRACKSIDE_BACKUP__DIR", Text),
    dash("backup.schedule", "TRACKSIDE_BACKUP__SCHEDULE", Text),
    dash("backup.keep", "TRACKSIDE_BACKUP__KEEP", Integer),
    key(
        "backup.include_secret_key",
        "TRACKSIDE_BACKUP__INCLUDE_SECRET_KEY",
        Bool,
    ),
    key("log.level", "TRACKSIDE_LOG__LEVEL", Text),
    key("log.format", "TRACKSIDE_LOG__FORMAT", Text),
    key("metrics.enabled", "TRACKSIDE_METRICS__ENABLED", Bool),
    key("metrics.listen", "TRACKSIDE_METRICS__LISTEN", Text),
    dash("uploads.enabled", "TRACKSIDE_UPLOADS__ENABLED", Bool),
    key("uploads.max_file", "TRACKSIDE_UPLOADS__MAX_FILE", Text),
    key("uploads.inbox_dir", "TRACKSIDE_UPLOADS__INBOX_DIR", Text),
    key(
        "uploads.path_template",
        "TRACKSIDE_UPLOADS__PATH_TEMPLATE",
        Text,
    ),
    dash("acquire.enabled", "TRACKSIDE_ACQUIRE__ENABLED", Bool),
    dash("acquire.library", "TRACKSIDE_ACQUIRE__LIBRARY", Text),
    key(
        "acquire.staging_dir",
        "TRACKSIDE_ACQUIRE__STAGING_DIR",
        Text,
    ),
    dash("acquire.prefer", "TRACKSIDE_ACQUIRE__PREFER", List),
    key(
        "acquire.search_timeout",
        "TRACKSIDE_ACQUIRE__SEARCH_TIMEOUT",
        Text,
    ),
    key(
        "acquire.max_file_bytes",
        "TRACKSIDE_ACQUIRE__MAX_FILE_BYTES",
        Text,
    ),
    dash(
        "acquire.member_daily",
        "TRACKSIDE_ACQUIRE__MEMBER_DAILY",
        Integer,
    ),
    dash(
        "acquire.member_active",
        "TRACKSIDE_ACQUIRE__MEMBER_ACTIVE",
        Integer,
    ),
    dash(
        "acquire.global_daily",
        "TRACKSIDE_ACQUIRE__GLOBAL_DAILY",
        Integer,
    ),
    dash(
        "acquire.instant_play",
        "TRACKSIDE_ACQUIRE__INSTANT_PLAY",
        Bool,
    ),
    dash(
        "acquire.fetched_keep",
        "TRACKSIDE_ACQUIRE__FETCHED_KEEP",
        Integer,
    ),
    dash(
        "acquire.fetched_max_bytes",
        "TRACKSIDE_ACQUIRE__FETCHED_MAX_BYTES",
        Text,
    ),
    key(
        "acquire.reject_words",
        "TRACKSIDE_ACQUIRE__REJECT_WORDS",
        CommaList,
    ),
    key(
        "acquire.path_template",
        "TRACKSIDE_ACQUIRE__PATH_TEMPLATE",
        Text,
    ),
    dash(
        "acquire.ytdlp.enabled",
        "TRACKSIDE_ACQUIRE__YTDLP__ENABLED",
        Bool,
    ),
    key("acquire.ytdlp.path", "TRACKSIDE_ACQUIRE__YTDLP__PATH", Text),
    key(
        "acquire.ytdlp.js_runtime",
        "TRACKSIDE_ACQUIRE__YTDLP__JS_RUNTIME",
        Text,
    ),
    key(
        "acquire.ytdlp.cookies_file",
        "TRACKSIDE_ACQUIRE__YTDLP__COOKIES_FILE",
        Text,
    ),
    key(
        "acquire.ytdlp.concurrency",
        "TRACKSIDE_ACQUIRE__YTDLP__CONCURRENCY",
        Integer,
    ),
    key(
        "acquire.ytdlp.search_rate",
        "TRACKSIDE_ACQUIRE__YTDLP__SEARCH_RATE",
        Text,
    ),
    key(
        "acquire.ytdlp.max_age_days",
        "TRACKSIDE_ACQUIRE__YTDLP__MAX_AGE_DAYS",
        Integer,
    ),
    dash(
        "acquire.soulseek.enabled",
        "TRACKSIDE_ACQUIRE__SOULSEEK__ENABLED",
        Bool,
    ),
    dash(
        "acquire.soulseek.url",
        "TRACKSIDE_ACQUIRE__SOULSEEK__URL",
        Text,
    ),
    secret(
        "acquire.soulseek.api_key",
        "TRACKSIDE_ACQUIRE__SOULSEEK__API_KEY",
    ),
    key(
        "acquire.soulseek.downloads_dir",
        "TRACKSIDE_ACQUIRE__SOULSEEK__DOWNLOADS_DIR",
        Text,
    ),
    key(
        "acquire.soulseek.concurrency",
        "TRACKSIDE_ACQUIRE__SOULSEEK__CONCURRENCY",
        Integer,
    ),
    key(
        "acquire.soulseek.search_timeout",
        "TRACKSIDE_ACQUIRE__SOULSEEK__SEARCH_TIMEOUT",
        Text,
    ),
    key(
        "acquire.soulseek.stall_timeout",
        "TRACKSIDE_ACQUIRE__SOULSEEK__STALL_TIMEOUT",
        Text,
    ),
    key(
        "acquire.soulseek.min_bitrate",
        "TRACKSIDE_ACQUIRE__SOULSEEK__MIN_BITRATE",
        Integer,
    ),
    dash(
        "enrichment.musicbrainz",
        "TRACKSIDE_ENRICHMENT__MUSICBRAINZ",
        Bool,
    ),
    KeyInfo {
        dashboard: true,
        ..secret(
            "enrichment.lastfm_api_key",
            "TRACKSIDE_ENRICHMENT__LASTFM_API_KEY",
        )
    },
];

/// Environment variables the server reads that are not configuration keys.
const OTHER_ENV: &[&str] = &["TRACKSIDE_CONFIG"];

fn info(key: &str) -> Option<&'static KeyInfo> {
    KEYS.iter().find(|k| k.key == key)
}

fn invalid(key: &str, message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: key.to_owned(),
        message: message.into(),
    }
}

/// Converts text from the environment or the command line to a TOML value of the key's kind.
fn parse_value(info: &KeyInfo, text: &str) -> Result<Value, ConfigError> {
    let text = text.trim();
    Ok(match info.kind {
        ValueKind::Text => Value::String(text.to_owned()),
        ValueKind::Bool => Value::Boolean(match text.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => {
                return Err(invalid(
                    info.key,
                    format!("expected true or false, got {text:?}"),
                ));
            }
        }),
        ValueKind::Integer => Value::Integer(
            text.parse()
                .map_err(|_| invalid(info.key, format!("expected a whole number, got {text:?}")))?,
        ),
        ValueKind::Float => Value::Float(
            text.parse()
                .map_err(|_| invalid(info.key, format!("expected a number, got {text:?}")))?,
        ),
        ValueKind::List | ValueKind::CommaList => {
            let items: Vec<&str> = if text.contains(',') || info.kind == ValueKind::CommaList {
                text.split(',').collect()
            } else {
                text.split_whitespace().collect()
            };
            Value::Array(
                items
                    .into_iter()
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(|item| Value::String(item.to_owned()))
                    .collect(),
            )
        }
    })
}

/// Sets a dotted key in a table, creating intermediate tables.
fn set(table: &mut Table, key: &str, value: Value) {
    let mut parts: Vec<&str> = key.split('.').collect();
    let Some(last) = parts.pop() else { return };
    let mut current = table;
    for part in parts {
        let entry = current
            .entry(part.to_owned())
            .or_insert_with(|| Value::Table(Table::new()));
        if !entry.is_table() {
            *entry = Value::Table(Table::new());
        }
        let Value::Table(next) = entry else { return };
        current = next;
    }
    current.insert(last.to_owned(), value);
}

fn get<'a>(table: &'a Table, key: &str) -> Option<&'a Value> {
    let mut parts = key.split('.');
    let first = parts.next()?;
    let mut value = table.get(first)?;
    for part in parts {
        value = value.as_table()?.get(part)?;
    }
    Some(value)
}

/// Copies the file's values over the defaults, recording each known key and warning about
/// unknown ones.
fn layer_file(base: &mut Table, file: &Table, prefix: &str, report: &mut ConfigReport) {
    for (name, value) in file {
        let dotted = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if let Some(info) = info(&dotted) {
            set(base, info.key, value.clone());
            report.sources.insert(info.key, ConfigSource::File);
        } else if let Value::Table(inner) = value
            && KEYS
                .iter()
                .any(|k| k.key.starts_with(&format!("{dotted}.")))
        {
            layer_file(base, inner, &dotted, report);
        } else {
            report.warnings.push(format!(
                "unknown configuration key {dotted:?} in the file is ignored"
            ));
        }
    }
}

fn read_secret_file(info: &KeyInfo, path: &str) -> Result<String, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: PathBuf::from(path),
        source,
    })?;
    let value = text.trim_end_matches(['\r', '\n']).to_owned();
    if value.is_empty() {
        return Err(invalid(info.key, format!("{path} is empty")));
    }
    Ok(value)
}

fn env_value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn layer_env(
    base: &mut Table,
    env: &[(String, String)],
    report: &mut ConfigReport,
) -> Result<(), ConfigError> {
    for info in KEYS {
        let direct = env_value(env, info.env).filter(|v| !v.is_empty());
        let from_file = if info.secret {
            env_value(env, &format!("{}_FILE", info.env)).filter(|v| !v.is_empty())
        } else {
            None
        };
        let text = match (direct, from_file) {
            (Some(value), Some(_)) => {
                report.warnings.push(format!(
                    "{} and {}_FILE are both set; using {}",
                    info.env, info.env, info.env
                ));
                value.to_owned()
            }
            (Some(value), None) => value.to_owned(),
            (None, Some(path)) => read_secret_file(info, path)?,
            (None, None) => {
                if info.key == "log.level"
                    && let Some(rust_log) = env_value(env, "RUST_LOG").filter(|v| !v.is_empty())
                {
                    rust_log.to_owned()
                } else {
                    continue;
                }
            }
        };
        set(base, info.key, parse_value(info, &text)?);
        report.sources.insert(info.key, ConfigSource::Env);
    }
    for (name, _) in env {
        if !name.starts_with("TRACKSIDE_") {
            continue;
        }
        let known = OTHER_ENV.contains(&name.as_str())
            || KEYS
                .iter()
                .any(|k| k.env == name || (k.secret && format!("{}_FILE", k.env) == *name));
        if !known {
            report
                .warnings
                .push(format!("unknown environment variable {name} is ignored"));
        }
    }
    Ok(())
}

fn layer_overrides(
    base: &mut Table,
    overrides: &[(String, String)],
    report: &mut ConfigReport,
) -> Result<(), ConfigError> {
    for (key, text) in overrides {
        let info = info(key.trim()).ok_or_else(|| invalid(key, "no such configuration key"))?;
        set(base, info.key, parse_value(info, text)?);
        report.sources.insert(info.key, ConfigSource::Override);
    }
    Ok(())
}

fn read_file(path: &Path) -> Result<Table, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    text.parse::<Table>().map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

/// Where the data directory is, which the other defaults hang off.
fn data_dir(sources: &super::ConfigSources, file: Option<&Table>) -> Result<PathBuf, ConfigError> {
    if let Some((_, value)) = sources
        .overrides
        .iter()
        .rev()
        .find(|(k, _)| k == "data_dir")
    {
        return Ok(PathBuf::from(value.trim()));
    }
    if let Some(value) = env_value(&sources.env, "TRACKSIDE_DATA_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    if let Some(value) = file.and_then(|f| f.get("data_dir")) {
        return value
            .as_str()
            .map(PathBuf::from)
            .ok_or_else(|| invalid("data_dir", "expected a path"));
    }
    Ok(default_data_dir())
}

/// Loads the configuration from defaults, the file, the environment and overrides, in that
/// order of precedence, and reports where each key came from.
pub fn load_config(sources: &super::ConfigSources) -> Result<(Config, ConfigReport), ConfigError> {
    let file = sources.file.as_deref().map(read_file).transpose()?;
    let data_dir = data_dir(sources, file.as_ref())?;
    let defaults = Config::defaults(&data_dir);
    let Value::Table(mut base) =
        Value::try_from(&defaults).map_err(|e| invalid("defaults", e.to_string()))?
    else {
        return Err(invalid("defaults", "not a table"));
    };
    let mut report = ConfigReport::default();
    if let Some(file) = &file {
        layer_file(&mut base, file, "", &mut report);
    }
    layer_env(&mut base, &sources.env, &mut report)?;
    layer_overrides(&mut base, &sources.overrides, &mut report)?;
    let mut config: Config = Value::Table(base)
        .try_into()
        .map_err(|e: toml::de::Error| {
            let message = e.message().to_owned();
            let key = KEYS
                .iter()
                .find(|k| {
                    message.contains(&format!("`{}`", k.key.rsplit('.').next().unwrap_or(k.key)))
                })
                .map_or("configuration", |k| k.key);
            invalid(key, message)
        })?;
    validate(&mut config, &mut report)?;
    Ok((config, report))
}

fn validate(config: &mut Config, report: &mut ConfigReport) -> Result<(), ConfigError> {
    if config.database.readers == 0 {
        return Err(invalid("database.readers", "must be at least 1"));
    }
    if config.scanner.threads == 0 {
        return Err(invalid("scanner.threads", "must be at least 1"));
    }
    if config.media.transcode_max_concurrent == 0 {
        return Err(invalid(
            "media.transcode_max_concurrent",
            "must be at least 1",
        ));
    }
    if !(0.0..=1.0).contains(&config.scanner.empty_guard) {
        return Err(invalid("scanner.empty_guard", "must be between 0 and 1"));
    }
    if config.sync.max_page == 0 || config.sync.max_page > crate::sync::MAX_LIMIT {
        return Err(invalid(
            "sync.max_page",
            format!("must be between 1 and {}", crate::sync::MAX_LIMIT),
        ));
    }
    if config.server.listen.is_empty() {
        return Err(invalid("server.listen", "needs at least one address"));
    }
    let base = config.server.base_path.trim().trim_end_matches('/');
    config.server.base_path = if base.is_empty() {
        String::new()
    } else if base.starts_with('/') {
        base.to_owned()
    } else {
        format!("/{base}")
    };
    match config.tls.mode {
        TlsMode::Files if config.tls.cert_file.is_none() || config.tls.key_file.is_none() => {
            return Err(invalid(
                "tls.mode",
                "\"files\" needs tls.cert_file and tls.key_file",
            ));
        }
        TlsMode::Acme if config.tls.acme_domains.is_empty() => {
            return Err(invalid("tls.mode", "\"acme\" needs tls.acme_domains"));
        }
        _ => {}
    }
    if config.auth.admin_password.is_some() && config.auth.admin_username.is_none() {
        report.warnings.push(String::from(
            "auth.admin_password is set without auth.admin_username and is ignored",
        ));
    }
    Ok(())
}

/// One line of `trackside config show`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownValue {
    pub key: &'static str,
    pub env: &'static str,
    /// The effective value as TOML, `<redacted>` for a set secret, `unset` for no value.
    pub value: String,
    pub source: ConfigSource,
    pub dashboard: bool,
}

/// Every key with its effective value and source. Secrets are redacted.
pub fn show(config: &Config, report: &ConfigReport) -> Vec<ShownValue> {
    let table = match Value::try_from(config) {
        Ok(Value::Table(table)) => table,
        _ => Table::new(),
    };
    KEYS.iter()
        .map(|info| {
            let value = match get(&table, info.key) {
                None => String::from("unset"),
                Some(_) if info.secret => String::from(REDACTED),
                Some(value) => value.to_string(),
            };
            ShownValue {
                key: info.key,
                env: info.env,
                value,
                source: report.source(info.key),
                dashboard: info.dashboard,
            }
        })
        .collect()
}

/// `trackside config show` as text: one `key = value  # source` line per key, then warnings.
pub fn render_show(config: &Config, report: &ConfigReport) -> String {
    let mut out = String::new();
    for line in show(config, report) {
        let source = match line.source {
            ConfigSource::Default => "default",
            ConfigSource::File => "file",
            ConfigSource::Env => "environment",
            ConfigSource::Override => "command line",
        };
        let _ = writeln!(out, "{} = {}  # {source}", line.key, line.value);
    }
    for warning in &report.warnings {
        let _ = writeln!(out, "# warning: {warning}");
    }
    out
}
