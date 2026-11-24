//! Settings: one value per key with the time it was set and the device that set it. The
//! server decides, the same way Tracks' `SyncedSetting.decide` does, and is authoritative:
//!
//! - Newest `modified_at` wins per key; a tie keeps the stored value.
//! - Union keys ([`UNION_KEYS`]) merge both lists and keep the later time.
//! - A cleared value is an explicit `null` with its time, and wins like any other value.
//! - A seed (`modified_at` at the 1970 epoch) never replaces a stored value.
//! - Keys the server does not know (platform-only settings) round-trip with newest-wins.
//! - [`crate::envelope`] values are stored opaque; `nearbyKey` must be one.

use std::collections::BTreeSet;

use serde_json::Value;
use trackside_core::Millis;

use crate::envelope;
use crate::limits::{MAX_DEVICE_ID_BYTES, MAX_SETTING_BYTES};
use crate::model::Reject;
use crate::validate::{is_label, not_ahead};

/// Keys whose value is a list of text that only grows: both sides' entries are kept.
pub const UNION_KEYS: &[&str] = &["forgottenSongs", "forgottenSongDates"];
/// Settings keys are short names.
pub const MAX_KEY_BYTES: usize = 128;

/// A stored setting.
#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    pub key: String,
    pub value: Value,
    pub modified_at: Millis,
    pub device_id: String,
    pub version: i64,
}

/// A change a client sends.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingWrite {
    pub key: String,
    pub value: Value,
    pub modified_at: Millis,
    pub device_id: String,
}

/// What happened to one key.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingOutcome {
    /// Stored. `current` is what is stored now, which for a union key may hold more than was
    /// sent.
    Accepted,
    /// The stored value is the same.
    Duplicate,
    /// The stored value is newer, or a seed met a real value. `current` is the one that stands.
    Conflict,
    Rejected(Reject),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SettingResult {
    pub key: String,
    pub outcome: SettingOutcome,
    /// The authoritative value after the decision; absent when rejected or never stored.
    pub current: Option<Setting>,
}

/// Every setting changed after a cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsPage {
    pub settings: Vec<Setting>,
    pub cursor: crate::epoch::Cursor,
}

/// What to store, before a version is given to it.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Store { value: Value, modified_at: Millis },
    Duplicate,
    Conflict,
    Reject(Reject),
}

pub fn is_union(key: &str) -> bool {
    UNION_KEYS.contains(&key)
}

fn text_list(value: &Value) -> Option<BTreeSet<String>> {
    match value {
        Value::Null => Some(BTreeSet::new()),
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => None,
    }
}

/// Checks a write on its own: key, device, time, size, encryption.
pub fn check(write: &SettingWrite, now: Millis) -> Result<(), Reject> {
    if !is_label(&write.key, MAX_KEY_BYTES) || !is_label(&write.device_id, MAX_DEVICE_ID_BYTES) {
        return Err(Reject::Invalid);
    }
    if !not_ahead(write.modified_at, now) || write.modified_at.0 < 0 {
        return Err(Reject::Future);
    }
    let bytes = serde_json::to_string(&write.value).map_or(usize::MAX, |s| s.len());
    if bytes > MAX_SETTING_BYTES {
        return Err(Reject::TooLarge);
    }
    let sealed = envelope::is_envelope(&write.value);
    if sealed && envelope::check(&write.value).is_err() {
        return Err(Reject::Invalid);
    }
    if envelope::encrypted_only(&write.key) && !sealed && !write.value.is_null() {
        return Err(Reject::EncryptionRequired);
    }
    if is_union(&write.key) && text_list(&write.value).is_none() {
        return Err(Reject::Invalid);
    }
    Ok(())
}

/// The decide rule. `write` has passed [`check`].
pub fn decide(stored: Option<&Setting>, write: &SettingWrite) -> Verdict {
    if is_union(&write.key) {
        let mut merged = stored.and_then(|s| text_list(&s.value)).unwrap_or_default();
        merged.extend(text_list(&write.value).unwrap_or_default());
        let value = Value::Array(merged.into_iter().map(Value::String).collect());
        if serde_json::to_string(&value).map_or(usize::MAX, |s| s.len()) > MAX_SETTING_BYTES {
            return Verdict::Reject(Reject::TooLarge);
        }
        let modified_at =
            stored.map_or(write.modified_at, |s| s.modified_at.max(write.modified_at));
        return match stored {
            Some(s) if s.value == value && s.modified_at == modified_at => Verdict::Duplicate,
            _ => Verdict::Store { value, modified_at },
        };
    }
    match stored {
        None => Verdict::Store {
            value: write.value.clone(),
            modified_at: write.modified_at,
        },
        Some(s) if s.value == write.value && s.modified_at >= write.modified_at => {
            Verdict::Duplicate
        }
        Some(s) if write.modified_at > s.modified_at && write.modified_at.0 > 0 => Verdict::Store {
            value: write.value.clone(),
            modified_at: write.modified_at,
        },
        Some(_) => Verdict::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn write(key: &str, value: Value, at: i64) -> SettingWrite {
        SettingWrite {
            key: key.to_owned(),
            value,
            modified_at: Millis(at),
            device_id: String::from("D1"),
        }
    }

    fn stored(key: &str, value: Value, at: i64) -> Setting {
        Setting {
            key: key.to_owned(),
            value,
            modified_at: Millis(at),
            device_id: String::from("D0"),
            version: 1,
        }
    }

    #[test]
    fn newest_wins_and_ties_keep_the_stored_value() {
        let s = stored("playlistName", json!("Tracks"), 100);
        assert_eq!(
            decide(Some(&s), &write("playlistName", json!("Mine"), 200)),
            Verdict::Store {
                value: json!("Mine"),
                modified_at: Millis(200)
            }
        );
        assert_eq!(
            decide(Some(&s), &write("playlistName", json!("Mine"), 100)),
            Verdict::Conflict
        );
        assert_eq!(
            decide(Some(&s), &write("playlistName", json!("Mine"), 50)),
            Verdict::Conflict
        );
        assert_eq!(
            decide(Some(&s), &write("playlistName", json!("Tracks"), 50)),
            Verdict::Duplicate
        );
    }

    #[test]
    fn seeds_never_replace_a_value_and_lose_ties_to_the_first_seed() {
        let real = stored("autoAddToPlaylist", json!(true), 1_000);
        assert_eq!(
            decide(Some(&real), &write("autoAddToPlaylist", json!(false), 0)),
            Verdict::Conflict
        );
        let seed = stored("autoAddToPlaylist", json!(true), 0);
        assert_eq!(
            decide(Some(&seed), &write("autoAddToPlaylist", json!(false), 0)),
            Verdict::Conflict
        );
        assert_eq!(
            decide(Some(&seed), &write("autoAddToPlaylist", json!(false), 5)),
            Verdict::Store {
                value: json!(false),
                modified_at: Millis(5)
            }
        );
        assert_eq!(
            decide(None, &write("autoAddToPlaylist", json!(false), 0)),
            Verdict::Store {
                value: json!(false),
                modified_at: Millis(0)
            }
        );
    }

    #[test]
    fn a_cleared_value_is_an_explicit_null_that_wins_by_time() {
        let s = stored("playlistID", json!("pl.1"), 100);
        assert_eq!(
            decide(Some(&s), &write("playlistID", Value::Null, 200)),
            Verdict::Store {
                value: Value::Null,
                modified_at: Millis(200)
            }
        );
        let cleared = stored("playlistID", Value::Null, 200);
        assert_eq!(
            decide(Some(&cleared), &write("playlistID", json!("pl.1"), 150)),
            Verdict::Conflict
        );
        assert_eq!(
            decide(Some(&cleared), &write("playlistID", json!("old"), 0)),
            Verdict::Conflict
        );
    }

    #[test]
    fn union_keys_merge_and_ignore_time_order() {
        let s = stored("forgottenSongs", json!(["b", "a"]), 200);
        assert_eq!(
            decide(Some(&s), &write("forgottenSongs", json!(["c", "a"]), 100)),
            Verdict::Store {
                value: json!(["a", "b", "c"]),
                modified_at: Millis(200)
            }
        );
        let sorted = stored("forgottenSongs", json!(["a", "b"]), 200);
        assert_eq!(
            decide(Some(&sorted), &write("forgottenSongs", json!(["a"]), 100)),
            Verdict::Duplicate
        );
        assert_eq!(
            decide(Some(&sorted), &write("forgottenSongs", Value::Null, 300)),
            Verdict::Store {
                value: json!(["a", "b"]),
                modified_at: Millis(300)
            },
            "nothing un-forgets a song"
        );
        assert_eq!(
            check(&write("forgottenSongs", json!("a"), 1), Millis(10)),
            Err(Reject::Invalid)
        );
    }

    #[test]
    fn nearby_key_must_be_client_encrypted() {
        let now = Millis(1_000);
        let plain = write("nearbyKey", json!("q83vEjRWeJA="), 1);
        assert_eq!(check(&plain, now), Err(Reject::EncryptionRequired));
        let bytes = write("nearbyKey", json!([1, 2, 3]), 1);
        assert_eq!(check(&bytes, now), Err(Reject::EncryptionRequired));
        let disguised = write(
            "nearbyKey",
            json!({"$encrypted": {"alg": "none", "ct": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}}),
            1,
        );
        assert_eq!(
            check(&disguised, now),
            Err(Reject::Invalid),
            "32 bytes is a raw key"
        );
        let sealed = write(
            "nearbyKey",
            json!({"$encrypted": {"v": 1, "alg": "chachapoly", "ct":
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}),
            1,
        );
        assert_eq!(check(&sealed, now), Ok(()));
        assert_eq!(check(&write("nearbyKey", Value::Null, 1), now), Ok(()));
        assert_eq!(
            check(&write("macOnlySetting", json!({"x": 1}), 1), now),
            Ok(())
        );
    }
}
