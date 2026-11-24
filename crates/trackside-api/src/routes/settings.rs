//! Dashboard-editable settings: the keys marked D in architecture section 11. A value set by
//! the configuration file or environment wins and is read-only; otherwise the dashboard's
//! value in the `settings` table wins over the built-in default.

use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use trackside_auth::Auth;
use trackside_core::config::{BackupSchedule, ConfigSource, Secret, parse_duration, parse_size};
use trackside_core::{Config, Db, LibraryId, PublicId};

use crate::ApiState;

/// The type of a setting, for validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Text,
    TextList,
    Bool,
    /// A duration such as `"30m"`, or `"off"` when `off` is true.
    Duration {
        off: bool,
    },
    Size,
    Count,
    Profile,
    Schedule,
    Library,
    Url,
    /// Write-only text, sealed at rest. Its value on the wire is whether it is set.
    Secret,
}

pub(crate) struct Key {
    pub name: &'static str,
    pub kind: Kind,
    pub value: fn(&Config) -> Value,
}

pub(crate) const KEYS: &[Key] = &[
    Key {
        name: "server.name",
        kind: Kind::Text,
        value: |c| json!(c.server.name),
    },
    Key {
        name: "server.public_urls",
        kind: Kind::TextList,
        value: |c| json!(c.server.public_urls),
    },
    Key {
        name: "scanner.watch",
        kind: Kind::Bool,
        value: |c| json!(c.scanner.watch),
    },
    Key {
        name: "scanner.quick_interval",
        kind: Kind::Duration { off: true },
        value: |c| json!(duration_or_off(c.scanner.quick_interval)),
    },
    Key {
        name: "scanner.full_interval",
        kind: Kind::Duration { off: true },
        value: |c| json!(duration_or_off(c.scanner.full_interval)),
    },
    Key {
        name: "scanner.missing_grace",
        kind: Kind::Duration { off: false },
        value: |c| json!(duration_text(c.scanner.missing_grace)),
    },
    Key {
        name: "media.transcode_cache_max",
        kind: Kind::Size,
        value: |c| json!(size_text(c.media.transcode_cache_max)),
    },
    Key {
        name: "media.art_cache_max",
        kind: Kind::Size,
        value: |c| json!(size_text(c.media.art_cache_max)),
    },
    Key {
        name: "media.cellular_profile",
        kind: Kind::Profile,
        value: |c| json!(c.media.cellular_profile),
    },
    Key {
        name: LEGACY_SUBSONIC,
        kind: Kind::Bool,
        value: |c| json!(c.auth.legacy_subsonic),
    },
    Key {
        name: "discovery.mdns",
        kind: Kind::Bool,
        value: |c| json!(c.discovery.mdns),
    },
    Key {
        name: "backup.schedule",
        kind: Kind::Schedule,
        value: |c| json!(schedule_text(c.backup.schedule)),
    },
    Key {
        name: "backup.keep",
        kind: Kind::Count,
        value: |c| json!(c.backup.keep),
    },
    Key {
        name: "uploads.enabled",
        kind: Kind::Bool,
        value: |c| json!(c.uploads.enabled),
    },
    Key {
        name: "acquire.enabled",
        kind: Kind::Bool,
        value: |c| json!(c.acquire.enabled),
    },
    Key {
        name: "acquire.library",
        kind: Kind::Library,
        value: |c| json!(c.acquire.library),
    },
    Key {
        name: "acquire.prefer",
        kind: Kind::TextList,
        value: |c| json!(c.acquire.prefer),
    },
    Key {
        name: "acquire.member_daily",
        kind: Kind::Count,
        value: |c| json!(c.acquire.member_daily),
    },
    Key {
        name: "acquire.member_active",
        kind: Kind::Count,
        value: |c| json!(c.acquire.member_active),
    },
    Key {
        name: "acquire.global_daily",
        kind: Kind::Count,
        value: |c| json!(c.acquire.global_daily),
    },
    Key {
        name: "acquire.instant_play",
        kind: Kind::Bool,
        value: |c| json!(c.acquire.instant_play),
    },
    Key {
        name: FETCHED_KEEP,
        kind: Kind::Count,
        value: |c| json!(c.acquire.fetched_keep),
    },
    Key {
        name: FETCHED_MAX_BYTES,
        kind: Kind::Size,
        value: |c| json!(size_text(c.acquire.fetched_max_bytes)),
    },
    Key {
        name: "acquire.ytdlp.enabled",
        kind: Kind::Bool,
        value: |c| json!(c.acquire.ytdlp.enabled),
    },
    Key {
        name: "acquire.soulseek.enabled",
        kind: Kind::Bool,
        value: |c| json!(c.acquire.soulseek.enabled),
    },
    Key {
        name: "acquire.soulseek.url",
        kind: Kind::Url,
        value: |c| json!(c.acquire.soulseek.url),
    },
    Key {
        name: "enrichment.musicbrainz",
        kind: Kind::Bool,
        value: |c| json!(c.enrichment.musicbrainz),
    },
    Key {
        name: LASTFM_API_KEY,
        kind: Kind::Secret,
        value: |c| json!(c.enrichment.lastfm_api_key.is_some()),
    },
    Key {
        name: super::cloud::ENABLED,
        kind: Kind::Bool,
        value: |_| json!(true),
    },
    Key {
        name: super::cloud::QUOTA_ITEMS,
        kind: Kind::Count,
        value: |_| json!(trackside_cloud::Limits::DEFAULT_ITEMS),
    },
    Key {
        name: super::cloud::QUOTA_BYTES,
        kind: Kind::Size,
        value: |_| json!(size_text(trackside_cloud::Limits::DEFAULT_BYTES)),
    },
];

/// The server-wide Last.fm key for suggestions.
pub const LASTFM_API_KEY: &str = "enrichment.lastfm_api_key";
/// How many songs played without being kept stay fetched, and the most they may take.
pub(crate) const FETCHED_KEEP: &str = "acquire.fetched_keep";
pub(crate) const FETCHED_MAX_BYTES: &str = "acquire.fetched_max_bytes";
/// Whether legacy Subsonic auth (app passwords) is on server-wide.
pub(crate) const LEGACY_SUBSONIC: &str = "auth.legacy_subsonic";

/// Hands the `auth.*` settings the dashboard stored to `auth`, unless the configuration sets
/// them. The server calls it once at start; `PATCH /admin/settings` keeps it current after.
pub async fn apply_stored_auth(db: &Db, auth: &Auth, report: &trackside_core::ConfigReport) {
    if report.source(LEGACY_SUBSONIC) != ConfigSource::Default {
        return;
    }
    let stored = db
        .read(|c| trackside_core::repo::settings::get::<Value>(c, LEGACY_SUBSONIC))
        .await;
    match stored {
        Ok(Some(Value::Bool(on))) => auth.set_legacy_subsonic(on),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "cannot read auth.legacy_subsonic"),
    }
}

/// What a secret setting is stored as: `{"sealed": "<base64>"}`.
pub(crate) fn sealed_value(auth: &Auth, name: &str, plain: &str) -> Result<Value, String> {
    let sealed = auth.seal_setting(name, plain).map_err(|e| e.to_string())?;
    Ok(json!({ "sealed": base64::engine::general_purpose::STANDARD.encode(sealed) }))
}

/// A secret setting the dashboard stored, opened. `None` when unset or when it no longer
/// opens (the secret key was replaced).
pub async fn stored_secret(db: &Db, auth: &Auth, name: &'static str) -> Option<Secret> {
    let value = db
        .read(move |c| trackside_core::repo::settings::get::<Value>(c, name))
        .await
        .ok()
        .flatten()?;
    let sealed = value.get("sealed")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(sealed)
        .ok()?;
    match auth.open_setting(name, &bytes) {
        Ok(plain) => Some(Secret::new(plain)),
        Err(e) => {
            tracing::warn!(setting = name, error = %e, "a stored secret setting cannot be opened; set it again");
            None
        }
    }
}

pub(crate) fn key(name: &str) -> Option<&'static Key> {
    KEYS.iter().find(|k| k.name == name)
}

/// Whether the configuration file, environment or command line set `name`.
pub(crate) fn locked(state: &ApiState, name: &str) -> bool {
    state.server.report.source(name) != ConfigSource::Default
}

/// The dashboard's stored value for `name`, unless the configuration locks it. `None` means
/// "use the configuration's value".
pub(crate) async fn effective(state: &ApiState, name: &'static str) -> Option<Value> {
    if locked(state, name) {
        return None;
    }
    state
        .db
        .read(move |c| trackside_core::repo::settings::get::<Value>(c, name))
        .await
        .ok()
        .flatten()
        .filter(|v| !v.is_null())
}

/// The effective boolean value of a D key.
pub(crate) async fn effective_bool(state: &ApiState, name: &'static str) -> bool {
    match effective(state, name).await {
        Some(Value::Bool(value)) => value,
        _ => key(name)
            .map(|k| (k.value)(&state.config))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// Checks a new value for a key and returns it as stored.
pub(crate) fn validate(kind: Kind, value: &Value) -> Result<Value, String> {
    let text = || value.as_str().map(str::trim);
    match kind {
        Kind::Text => match text() {
            Some(t) if !t.is_empty() && t.len() <= 200 => Ok(json!(t)),
            _ => Err(String::from(
                "must be non-empty text of at most 200 characters",
            )),
        },
        Kind::TextList => {
            let items = value.as_array().ok_or("must be a list of text")?;
            let mut out = Vec::new();
            for item in items {
                let text = item.as_str().ok_or("must be a list of text")?.trim();
                if !text.is_empty() {
                    out.push(json!(text));
                }
            }
            Ok(Value::Array(out))
        }
        Kind::Bool => value
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| String::from("must be true or false")),
        Kind::Duration { off } => match text() {
            Some("off") if off => Ok(json!("off")),
            Some(t) if parse_duration(t).is_some() => Ok(json!(t)),
            _ => Err(String::from("must be a duration such as \"30m\"")),
        },
        Kind::Size => match text() {
            Some(t) if parse_size(t).is_some() => Ok(json!(t)),
            _ => Err(String::from("must be a size such as \"10GiB\"")),
        },
        Kind::Count => value
            .as_u64()
            .filter(|n| u32::try_from(*n).is_ok())
            .map(|n| json!(n))
            .ok_or_else(|| String::from("must be a whole number")),
        Kind::Profile => match text() {
            Some(t) if trackside_media::Profile::parse(t).is_some() => {
                Ok(json!(t.to_ascii_lowercase()))
            }
            _ => Err(String::from(
                "must be a transcode profile such as \"aac-256\"",
            )),
        },
        Kind::Schedule => match text() {
            Some(t) if BackupSchedule::parse(t).is_some() => Ok(json!(t)),
            _ => Err(String::from(
                "must be \"off\", \"daily HH:MM\" or \"weekly <day> HH:MM\"",
            )),
        },
        Kind::Library => match value {
            Value::Null => Ok(Value::Null),
            Value::String(t) if LibraryId::parse(t).is_ok() || t.starts_with('/') => Ok(json!(t)),
            _ => Err(String::from("must be a library id or a root path")),
        },
        Kind::Url => match text() {
            Some(t) if t.starts_with("http://") || t.starts_with("https://") => Ok(json!(t)),
            None if value.is_null() => Ok(Value::Null),
            _ => Err(String::from("must be an http or https URL")),
        },
        Kind::Secret => match text() {
            Some(t)
                if !t.is_empty() && t.len() <= 128 && t.chars().all(|c| c.is_ascii_graphic()) =>
            {
                Ok(json!(t))
            }
            _ => Err(String::from("must be a key of letters and digits")),
        },
    }
}

/// A duration in its largest whole unit: `"30m"`, `"6h"`, `"30d"`.
pub(crate) fn duration_text(d: Duration) -> String {
    let ms = d.as_millis();
    for (unit, size) in [
        ("d", 86_400_000),
        ("h", 3_600_000),
        ("m", 60_000),
        ("s", 1_000),
    ] {
        if ms >= size && ms.is_multiple_of(size) {
            return format!("{}{unit}", ms / size);
        }
    }
    format!("{ms}ms")
}

fn duration_or_off(d: Option<Duration>) -> String {
    d.map_or_else(|| String::from("off"), duration_text)
}

/// A size in its largest whole binary unit: `"10GiB"`, `"512MiB"`.
pub(crate) fn size_text(bytes: u64) -> String {
    for (unit, shift) in [("TiB", 40), ("GiB", 30), ("MiB", 20), ("KiB", 10)] {
        let size = 1_u64 << shift;
        if bytes >= size && bytes.is_multiple_of(size) {
            return format!("{}{unit}", bytes / size);
        }
    }
    bytes.to_string()
}

fn schedule_text(schedule: BackupSchedule) -> String {
    match schedule {
        BackupSchedule::Off => String::from("off"),
        BackupSchedule::Daily { hour, minute } => format!("daily {hour:02}:{minute:02}"),
        BackupSchedule::Weekly {
            weekday,
            hour,
            minute,
        } => {
            let day = match weekday {
                jiff::civil::Weekday::Monday => "mon",
                jiff::civil::Weekday::Tuesday => "tue",
                jiff::civil::Weekday::Wednesday => "wed",
                jiff::civil::Weekday::Thursday => "thu",
                jiff::civil::Weekday::Friday => "fri",
                jiff::civil::Weekday::Saturday => "sat",
                jiff::civil::Weekday::Sunday => "sun",
            };
            format!("weekly {day} {hour:02}:{minute:02}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_sizes_print_in_their_largest_unit() {
        assert_eq!(duration_text(Duration::from_mins(30)), "30m");
        assert_eq!(duration_text(Duration::from_hours(30 * 24)), "30d");
        assert_eq!(duration_text(Duration::from_millis(1500)), "1500ms");
        assert_eq!(size_text(10 << 30), "10GiB");
        assert_eq!(size_text(1000), "1000");
    }

    #[test]
    fn values_are_validated_by_kind() {
        assert!(validate(Kind::Bool, &json!(true)).is_ok());
        assert!(validate(Kind::Bool, &json!("yes")).is_err());
        assert_eq!(
            validate(Kind::Duration { off: true }, &json!("off")),
            Ok(json!("off"))
        );
        assert!(validate(Kind::Duration { off: false }, &json!("off")).is_err());
        assert!(validate(Kind::Size, &json!("2GiB")).is_ok());
        assert!(validate(Kind::Profile, &json!("wma")).is_err());
        assert!(validate(Kind::Schedule, &json!("weekly mon 03:30")).is_ok());
        assert!(validate(Kind::Text, &json!("  ")).is_err());
        assert!(validate(Kind::Secret, &json!("0123abcd")).is_ok());
        assert!(validate(Kind::Secret, &json!("a key")).is_err());
        assert_eq!(
            validate(Kind::TextList, &json!([" https://a ", ""])),
            Ok(json!(["https://a"]))
        );
    }

    #[test]
    fn every_key_has_a_default_that_validates() {
        let config = Config::defaults("/data");
        for key in KEYS {
            let value = (key.value)(&config);
            if value.is_null() || key.kind == Kind::Secret {
                continue;
            }
            assert!(validate(key.kind, &value).is_ok(), "{}: {value}", key.name);
        }
    }
}
