//! Configuration: every key of architecture section 11, typed and resolved.
//!
//! [`Config::defaults`] gives the built-in defaults for a data directory. [`load_config`]
//! layers the TOML file, environment variables and CLI overrides on top and reports where each
//! value came from.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A secret value from the configuration. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// Serializes as a placeholder, so no dump of the configuration can carry the secret.
impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(REDACTED)
    }
}

/// What `config show` and any serialized configuration print instead of a secret.
pub const REDACTED: &str = "<redacted>";

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// A token-bucket rate such as `"10/1m"`: `count` events per `per`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    pub count: u32,
    pub per: Duration,
}

impl Rate {
    pub fn parse(text: &str) -> Option<Self> {
        let (count, per) = text.trim().split_once('/')?;
        let count = count.trim().parse().ok()?;
        let per = parse_duration(per)?;
        (count > 0 && !per.is_zero()).then_some(Self { count, per })
    }
}

impl Serialize for Rate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{}/{}", self.count, format_duration(self.per)))
    }
}

impl<'de> Deserialize<'de> for Rate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!("invalid rate {text:?}, expected like \"10/1m\""))
        })
    }
}

/// Parses `"500ms"`, `"30s"`, `"15m"`, `"6h"` or `"30d"`.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().ok()?;
    let seconds = match unit.trim() {
        "ms" => return Some(Duration::from_millis(number)),
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return None,
    };
    Some(Duration::from_secs(number.checked_mul(seconds)?))
}

/// Parses a size such as `"512MiB"`, `"10GiB"`, `"1GB"` or a plain byte count.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().ok()?;
    let multiplier: u64 = match unit.trim() {
        "" | "B" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        "KB" | "kB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        _ => return None,
    };
    number.checked_mul(multiplier)
}

/// A duration in the largest unit that divides it: `"30d"`, `"6h"`, `"15m"`, `"30s"`, `"500ms"`.
pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if !ms.is_multiple_of(1000) {
        return format!("{ms}ms");
    }
    let s = d.as_secs();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if s > 0 && s.is_multiple_of(size) {
            return format!("{}{unit}", s / size);
        }
    }
    format!("{s}s")
}

/// A size in the largest binary unit that divides it: `"10GiB"`, `"512MiB"`, or bytes.
pub fn format_size(bytes: u64) -> String {
    for (unit, shift) in [("TiB", 40), ("GiB", 30), ("MiB", 20), ("KiB", 10)] {
        let size = 1u64 << shift;
        if bytes > 0 && bytes.is_multiple_of(size) {
            return format!("{}{unit}", bytes / size);
        }
    }
    bytes.to_string()
}

/// Serializers matching [`serde_units`], so the defaults can be layered as TOML.
mod ser {
    use std::time::Duration;

    use serde::Serializer;

    pub fn duration<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::format_duration(*d))
    }

    #[expect(clippy::ref_option, reason = "serde passes fields by reference")]
    pub fn optional_duration<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_str(&super::format_duration(*d)),
            None => s.serialize_str("off"),
        }
    }

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde passes fields by reference"
    )]
    pub fn size<S: Serializer>(bytes: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::format_size(*bytes))
    }
}

/// Serde helpers for the duration and size strings of the configuration file.
pub mod serde_units {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum TextOrNumber {
        Text(String),
        Number(u64),
    }

    /// A duration string, or a number of seconds.
    pub fn duration<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        match TextOrNumber::deserialize(deserializer)? {
            TextOrNumber::Number(seconds) => Ok(Duration::from_secs(seconds)),
            TextOrNumber::Text(text) => super::parse_duration(&text).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "invalid duration {text:?}, expected like \"30s\""
                ))
            }),
        }
    }

    /// Like [`duration`], with `"off"` meaning `None`.
    pub fn optional_duration<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        match TextOrNumber::deserialize(deserializer)? {
            TextOrNumber::Text(text) if text.trim() == "off" => Ok(None),
            TextOrNumber::Number(seconds) => Ok(Some(Duration::from_secs(seconds))),
            TextOrNumber::Text(text) => super::parse_duration(&text).map(Some).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "invalid duration {text:?}, expected like \"30m\" or \"off\""
                ))
            }),
        }
    }

    /// A size string, or a number of bytes.
    pub fn size<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        match TextOrNumber::deserialize(deserializer)? {
            TextOrNumber::Number(bytes) => Ok(bytes),
            TextOrNumber::Text(text) => super::parse_size(&text).ok_or_else(|| {
                serde::de::Error::custom(format!("invalid size {text:?}, expected like \"512MiB\""))
            }),
        }
    }
}

use serde_units::{duration, optional_duration, size};

/// The whole configuration, resolved.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub data_dir: PathBuf,
    pub music_dir: Option<PathBuf>,
    pub cache_dir: PathBuf,
    pub server: ServerConfig,
    pub tls: TlsConfig,
    pub database: DatabaseConfig,
    pub scanner: ScannerConfig,
    pub media: MediaConfig,
    pub auth: AuthConfig,
    pub sync: SyncConfig,
    pub discovery: DiscoveryConfig,
    pub backup: BackupConfig,
    pub log: LogConfig,
    pub metrics: MetricsConfig,
    pub uploads: UploadsConfig,
    pub acquire: AcquireConfig,
    pub enrichment: EnrichmentConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServerConfig {
    pub name: String,
    pub listen: Vec<SocketAddr>,
    pub base_path: String,
    pub public_urls: Vec<String>,
    pub trusted_proxies: Vec<ipnet::IpNet>,
    /// Networks that count as local besides the private, loopback, link-local and Tailscale
    /// ranges, such as a LAN's global IPv6 prefix: their clients may send passwords and keys
    /// over plain HTTP.
    pub local_networks: Vec<ipnet::IpNet>,
    pub max_connections: u32,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub header_timeout: Duration,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub request_timeout: Duration,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub shutdown_grace: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    Off,
    Files,
    Acme,
    SelfSigned,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TlsConfig {
    pub mode: TlsMode,
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    pub acme_domains: Vec<String>,
    pub acme_email: Option<String>,
    pub acme_directory: String,
    pub acme_cache: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DatabaseConfig {
    pub path: PathBuf,
    pub readers: usize,
    #[serde(deserialize_with = "size", serialize_with = "ser::size")]
    pub cache: u64,
    #[serde(deserialize_with = "size", serialize_with = "ser::size")]
    pub mmap: u64,
    pub allow_network_fs: bool,
}

impl DatabaseConfig {
    /// Options for [`crate::Db::open`].
    pub fn options(&self) -> crate::DbOptions {
        crate::DbOptions {
            path: self.path.clone(),
            readers: self.readers,
            cache_bytes: self.cache,
            mmap_bytes: self.mmap,
            allow_network_fs: self.allow_network_fs,
            filesystem: crate::db::FilesystemCheck::Detect,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScannerConfig {
    pub threads: usize,
    pub watch: bool,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub watch_debounce: Duration,
    #[serde(
        deserialize_with = "optional_duration",
        serialize_with = "ser::optional_duration"
    )]
    pub quick_interval: Option<Duration>,
    #[serde(
        deserialize_with = "optional_duration",
        serialize_with = "ser::optional_duration"
    )]
    pub full_interval: Option<Duration>,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub settle: Duration,
    pub follow_symlinks: bool,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub missing_grace: Duration,
    pub empty_guard: f64,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub file_timeout: Duration,
    pub ignore: Vec<String>,
    pub extensions: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MediaConfig {
    /// `None` looks for `ffmpeg` on `PATH`.
    pub ffmpeg_path: Option<PathBuf>,
    pub transcode_max_concurrent: usize,
    #[serde(deserialize_with = "size", serialize_with = "ser::size")]
    pub transcode_cache_max: u64,
    #[serde(deserialize_with = "size", serialize_with = "ser::size")]
    pub art_cache_max: u64,
    #[serde(deserialize_with = "size", serialize_with = "ser::size")]
    pub cache_min_free: u64,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub hls_segment: Duration,
    /// A profile name such as `aac-256`, parsed by the media crate.
    pub cellular_profile: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AuthConfig {
    pub secret_key_file: PathBuf,
    pub legacy_subsonic: bool,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub session_idle: Duration,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub session_max: Duration,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub media_token_ttl: Duration,
    #[serde(deserialize_with = "size", serialize_with = "ser::size")]
    pub argon2_memory: u64,
    pub argon2_iterations: u32,
    pub login_rate: Rate,
    pub admin_username: Option<String>,
    pub admin_password: Option<Secret>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncConfig {
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub tombstone_retention: Duration,
    pub max_page: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiscoveryConfig {
    pub mdns: bool,
}

/// When scheduled backups run, in server local time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupSchedule {
    Off,
    Daily {
        hour: u8,
        minute: u8,
    },
    Weekly {
        weekday: jiff::civil::Weekday,
        hour: u8,
        minute: u8,
    },
}

impl BackupSchedule {
    /// Parses `off`, `daily HH:MM` or `weekly <day> HH:MM` (day as `mon` or `monday`).
    pub fn parse(text: &str) -> Option<Self> {
        let mut words = text.split_whitespace();
        let schedule = match words.next()? {
            "off" => Self::Off,
            "daily" => {
                let (hour, minute) = parse_clock(words.next()?)?;
                Self::Daily { hour, minute }
            }
            "weekly" => {
                let weekday = parse_weekday(words.next()?)?;
                let (hour, minute) = parse_clock(words.next()?)?;
                Self::Weekly {
                    weekday,
                    hour,
                    minute,
                }
            }
            _ => return None,
        };
        words.next().is_none().then_some(schedule)
    }
}

fn parse_clock(text: &str) -> Option<(u8, u8)> {
    let (hour, minute) = text.split_once(':')?;
    let hour: u8 = hour.parse().ok()?;
    let minute: u8 = minute.parse().ok()?;
    (hour < 24 && minute < 60).then_some((hour, minute))
}

fn parse_weekday(text: &str) -> Option<jiff::civil::Weekday> {
    use jiff::civil::Weekday;
    let lower = text.to_ascii_lowercase();
    let days = [
        ("mon", Weekday::Monday),
        ("tue", Weekday::Tuesday),
        ("wed", Weekday::Wednesday),
        ("thu", Weekday::Thursday),
        ("fri", Weekday::Friday),
        ("sat", Weekday::Saturday),
        ("sun", Weekday::Sunday),
    ];
    days.iter()
        .find(|(short, day)| lower == *short || lower == format!("{day:?}").to_ascii_lowercase())
        .map(|(_, day)| *day)
}

impl std::fmt::Display for BackupSchedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Daily { hour, minute } => write!(f, "daily {hour:02}:{minute:02}"),
            Self::Weekly {
                weekday,
                hour,
                minute,
            } => {
                let day = format!("{weekday:?}").to_ascii_lowercase();
                write!(f, "weekly {} {hour:02}:{minute:02}", &day[..3])
            }
        }
    }
}

impl Serialize for BackupSchedule {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for BackupSchedule {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!("invalid schedule {text:?}, expected \"off\", \"daily HH:MM\" or \"weekly <day> HH:MM\""))
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BackupConfig {
    pub dir: PathBuf,
    pub schedule: BackupSchedule,
    pub keep: u32,
    pub include_secret_key: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LogConfig {
    pub level: String,
    /// `None`: pretty on a terminal, JSON otherwise.
    pub format: Option<LogFormat>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MetricsConfig {
    pub enabled: bool,
    /// `None` serves `/metrics` on the main listener.
    pub listen: Option<SocketAddr>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UploadsConfig {
    pub enabled: bool,
    #[serde(deserialize_with = "size", serialize_with = "ser::size")]
    pub max_file: u64,
    pub inbox_dir: Option<PathBuf>,
    pub path_template: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AcquireConfig {
    pub enabled: bool,
    /// Pid or root path of the writable acquired library.
    pub library: Option<String>,
    pub staging_dir: PathBuf,
    /// Provider names in order of preference.
    pub prefer: Vec<String>,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub search_timeout: Duration,
    #[serde(deserialize_with = "size", serialize_with = "ser::size")]
    pub max_file_bytes: u64,
    pub member_daily: u32,
    pub member_active: u32,
    pub global_daily: u32,
    pub instant_play: bool,
    /// Songs played without being kept: how many stay fetched, and the most their files may
    /// take together.
    pub fetched_keep: u32,
    #[serde(deserialize_with = "size", serialize_with = "ser::size")]
    pub fetched_max_bytes: u64,
    pub reject_words: Vec<String>,
    pub path_template: String,
    pub ytdlp: YtdlpConfig,
    pub soulseek: SoulseekConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct YtdlpConfig {
    pub enabled: bool,
    /// `None`: `<data>/bin/yt-dlp` if present, else `yt-dlp` on `PATH`.
    pub path: Option<PathBuf>,
    /// `None`: `deno` on `PATH` if present.
    pub js_runtime: Option<PathBuf>,
    pub cookies_file: Option<PathBuf>,
    pub concurrency: u32,
    pub search_rate: Rate,
    pub max_age_days: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SoulseekConfig {
    pub enabled: bool,
    pub url: Option<String>,
    pub api_key: Option<Secret>,
    pub downloads_dir: Option<PathBuf>,
    pub concurrency: u32,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub search_timeout: Duration,
    #[serde(deserialize_with = "duration", serialize_with = "ser::duration")]
    pub stall_timeout: Duration,
    pub min_bitrate: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EnrichmentConfig {
    pub musicbrainz: bool,
    pub lastfm_api_key: Option<Secret>,
}

const DEFAULT_PORT: u16 = 4747;

const REJECT_WORDS: &[&str] = &[
    "live",
    "remix",
    "karaoke",
    "instrumental",
    "cover",
    "nightcore",
    "sped up",
    "slowed",
    "8d",
    "reverb",
    "acapella",
    "tribute",
];

fn cpus() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| String::from("Trackside"))
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|&s| s.to_owned()).collect()
}

impl Config {
    /// The built-in defaults of architecture section 11, with paths under `data_dir`.
    pub fn defaults(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir: PathBuf = data_dir.into();
        let cpus = cpus();
        Self {
            music_dir: None,
            cache_dir: data_dir.join("cache"),
            server: ServerConfig {
                name: host_name(),
                listen: vec![SocketAddr::from((
                    std::net::Ipv6Addr::UNSPECIFIED,
                    DEFAULT_PORT,
                ))],
                base_path: String::new(),
                public_urls: Vec::new(),
                trusted_proxies: Vec::new(),
                local_networks: Vec::new(),
                max_connections: 1024,
                header_timeout: Duration::from_secs(30),
                request_timeout: Duration::from_secs(60),
                shutdown_grace: Duration::from_secs(10),
            },
            tls: TlsConfig {
                mode: TlsMode::Off,
                cert_file: None,
                key_file: None,
                acme_domains: Vec::new(),
                acme_email: None,
                acme_directory: String::from("https://acme-v02.api.letsencrypt.org/directory"),
                acme_cache: data_dir.join("acme"),
            },
            database: DatabaseConfig {
                path: data_dir.join("trackside.db"),
                readers: cpus.min(8),
                cache: 32 << 20,
                mmap: 256 << 20,
                allow_network_fs: false,
            },
            scanner: ScannerConfig::defaults(cpus),
            media: MediaConfig {
                ffmpeg_path: None,
                transcode_max_concurrent: (cpus / 2).max(1),
                transcode_cache_max: 10 << 30,
                art_cache_max: 2 << 30,
                cache_min_free: 1 << 30,
                hls_segment: Duration::from_secs(6),
                cellular_profile: String::from("aac-256"),
            },
            auth: AuthConfig {
                secret_key_file: data_dir.join("secret.key"),
                legacy_subsonic: true,
                session_idle: Duration::from_secs(14 * 86_400),
                session_max: Duration::from_secs(90 * 86_400),
                media_token_ttl: Duration::from_secs(7 * 86_400),
                argon2_memory: 19 << 20,
                argon2_iterations: 2,
                login_rate: Rate {
                    count: 10,
                    per: Duration::from_secs(60),
                },
                admin_username: None,
                admin_password: None,
            },
            sync: SyncConfig {
                tombstone_retention: Duration::from_secs(90 * 86_400),
                max_page: 5000,
            },
            discovery: DiscoveryConfig { mdns: true },
            backup: BackupConfig {
                dir: data_dir.join("backups"),
                schedule: BackupSchedule::Daily {
                    hour: 3,
                    minute: 30,
                },
                keep: 7,
                include_secret_key: true,
            },
            log: LogConfig {
                level: String::from("info"),
                format: None,
            },
            metrics: MetricsConfig {
                enabled: false,
                listen: None,
            },
            uploads: UploadsConfig {
                enabled: false,
                max_file: 2 << 30,
                inbox_dir: None,
                path_template: String::from(
                    "{album_artist}/{album}/{disc}-{track:02} {title}.{ext}",
                ),
            },
            acquire: AcquireConfig::defaults(&data_dir),
            enrichment: EnrichmentConfig {
                musicbrainz: false,
                lastfm_api_key: None,
            },
            data_dir,
        }
    }
}

impl ScannerConfig {
    fn defaults(cpus: usize) -> Self {
        Self {
            threads: cpus.min(4),
            watch: true,
            watch_debounce: Duration::from_secs(5),
            quick_interval: Some(Duration::from_secs(30 * 60)),
            full_interval: None,
            settle: Duration::from_secs(10),
            follow_symlinks: true,
            missing_grace: Duration::from_secs(30 * 86_400),
            empty_guard: 0.5,
            file_timeout: Duration::from_secs(30),
            ignore: strings(&[".trackside-ignore", ".nomedia", ".ndignore"]),
            extensions: strings(&[
                "flac", "m4a", "mp4", "alac", "mp3", "aac", "ogg", "oga", "opus", "wav", "aif",
                "aiff", "wv", "ape",
            ]),
        }
    }
}

impl AcquireConfig {
    fn defaults(data_dir: &Path) -> Self {
        Self {
            enabled: false,
            library: None,
            staging_dir: data_dir.join("acquire").join("staging"),
            prefer: strings(&["soulseek", "ytdlp"]),
            search_timeout: Duration::from_secs(8),
            max_file_bytes: 1 << 30,
            member_daily: 50,
            member_active: 5,
            global_daily: 500,
            instant_play: false,
            fetched_keep: 500,
            fetched_max_bytes: 20 << 30,
            reject_words: strings(REJECT_WORDS),
            path_template: String::from("{album_artist}/{album}/{disc}-{track:02} {title}.{ext}"),
            ytdlp: YtdlpConfig {
                enabled: false,
                path: None,
                js_runtime: None,
                cookies_file: None,
                concurrency: 2,
                search_rate: Rate {
                    count: 10,
                    per: Duration::from_secs(60),
                },
                max_age_days: 60,
            },
            soulseek: SoulseekConfig {
                enabled: false,
                url: None,
                api_key: None,
                downloads_dir: None,
                concurrency: 3,
                search_timeout: Duration::from_secs(15),
                stall_timeout: Duration::from_secs(120),
                min_bitrate: 256,
            },
        }
    }
}

/// The platform default data directory: `$STATE_DIRECTORY` (systemd), else
/// `$XDG_DATA_HOME/trackside`, else `~/.local/share/trackside`; on macOS
/// `~/Library/Application Support/Trackside`. Docker images set `TRACKSIDE_DATA_DIR=/data`.
pub fn default_data_dir() -> PathBuf {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(state) = var("STATE_DIRECTORY") {
        // systemd may pass several directories separated by ':'; the first is ours.
        let first = state.to_string_lossy().split(':').next().map(PathBuf::from);
        if let Some(first) = first {
            return first;
        }
    }
    let home = var("HOME").unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        return home
            .join("Library")
            .join("Application Support")
            .join("Trackside");
    }
    var("XDG_DATA_HOME")
        .unwrap_or_else(|| home.join(".local").join("share"))
        .join("trackside")
}

/// Where configuration comes from, in increasing precedence after the built-in defaults.
#[derive(Clone, Debug, Default)]
pub struct ConfigSources {
    /// The TOML file, if one was named or found.
    pub file: Option<PathBuf>,
    /// Environment variables (all of them; only `TRACKSIDE_*` and `RUST_LOG` are read).
    pub env: Vec<(String, String)>,
    /// CLI overrides as dotted key and value.
    pub overrides: Vec<(String, String)>,
}

/// Where one key's effective value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSource {
    Default,
    File,
    Env,
    Override,
}

/// The source of every key that is not a default, and warnings found while loading.
#[derive(Clone, Debug, Default)]
pub struct ConfigReport {
    pub sources: BTreeMap<&'static str, ConfigSource>,
    pub warnings: Vec<String>,
}

impl ConfigReport {
    /// Where `key` (dotted, as in architecture section 11) came from.
    pub fn source(&self, key: &str) -> ConfigSource {
        self.sources
            .get(key)
            .copied()
            .unwrap_or(ConfigSource::Default)
    }
}

/// Why the configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{key}: {message}")]
    Invalid { key: String, message: String },
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

mod loader;

pub use loader::{KEYS, KeyInfo, ShownValue, ValueKind, load_config, render_show, show};

/// Keys set by the file or the environment, which the dashboard shows as read-only.
pub fn locked_keys(report: &ConfigReport) -> BTreeSet<&'static str> {
    report
        .sources
        .iter()
        .filter(|(_, source)| matches!(source, ConfigSource::File | ConfigSource::Env))
        .map(|(key, _)| *key)
        .collect()
}

/// The default path of the configuration file for a data directory.
pub fn default_config_file(data_dir: &Path) -> PathBuf {
    data_dir.join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_every_unit() {
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("15m"), Some(Duration::from_secs(900)));
        assert_eq!(parse_duration("6h"), Some(Duration::from_secs(21_600)));
        assert_eq!(parse_duration("30d"), Some(Duration::from_secs(2_592_000)));
        assert_eq!(parse_duration("30"), None);
        assert_eq!(parse_duration("5 weeks"), None);
    }

    #[test]
    fn sizes_parse_binary_and_decimal_units() {
        assert_eq!(parse_size("512MiB"), Some(512 << 20));
        assert_eq!(parse_size("10GiB"), Some(10 << 30));
        assert_eq!(parse_size("1GB"), Some(1_000_000_000));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size("1 parsec"), None);
    }

    #[test]
    fn rates_parse() {
        assert_eq!(
            Rate::parse("10/1m"),
            Some(Rate {
                count: 10,
                per: Duration::from_secs(60)
            })
        );
        assert_eq!(Rate::parse("0/1m"), None);
        assert_eq!(Rate::parse("ten"), None);
    }

    #[test]
    fn backup_schedules_parse() {
        assert_eq!(BackupSchedule::parse("off"), Some(BackupSchedule::Off));
        assert_eq!(
            BackupSchedule::parse("daily 03:30"),
            Some(BackupSchedule::Daily {
                hour: 3,
                minute: 30
            })
        );
        assert_eq!(
            BackupSchedule::parse("weekly sun 04:00"),
            Some(BackupSchedule::Weekly {
                weekday: jiff::civil::Weekday::Sunday,
                hour: 4,
                minute: 0
            })
        );
        assert_eq!(BackupSchedule::parse("daily 25:00"), None);
    }

    #[test]
    fn defaults_put_paths_under_the_data_dir() {
        let config = Config::defaults("/data");
        assert_eq!(config.database.path, Path::new("/data/trackside.db"));
        assert_eq!(config.cache_dir, Path::new("/data/cache"));
        assert_eq!(config.server.listen[0].port(), 4747);
        assert!(config.server.listen[0].is_ipv6());
    }

    #[test]
    fn secrets_are_redacted_in_debug() {
        let secret = Secret::new("hunter2");
        assert!(!format!("{secret:?}").contains("hunter2"));
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn locked_keys_are_those_from_file_and_env() {
        let mut report = ConfigReport::default();
        report.sources.insert("scanner.watch", ConfigSource::File);
        report.sources.insert("server.name", ConfigSource::Env);
        report.sources.insert("log.level", ConfigSource::Override);
        let locked = locked_keys(&report);
        assert_eq!(
            locked.into_iter().collect::<Vec<_>>(),
            ["scanner.watch", "server.name"]
        );
    }
}
