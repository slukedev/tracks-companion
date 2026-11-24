//! The merge rules: given the stored copy of a record and a pushed item, what to store. Pure,
//! so every rule is tested without a database. ADR 0012 explains each rule.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use trackside_core::Millis;

use crate::devices::Registry;
use crate::entity::{Entity, Rule};
use crate::model::{Body, Fields, PushItem, Record, Reject};
use crate::validate::not_ahead;

/// Play fields that never change once set: a different value is a conflict.
pub const PLAY_CORE: &[&str] = &[
    "title",
    "artistName",
    "albumTitle",
    "playedAtMs",
    "kind",
    "source",
    "recordedByDeviceID",
    "platform",
];
/// Play fields where the newest write wins. Every other play field is an enrichment: the
/// first non-null value wins.
pub const PLAY_NEWEST: &[&str] = &["artworkURL"];
/// Station fields that take the minimum and the maximum.
pub const STATION_FIRST_SEEN: &str = "firstSeenMs";
pub const STATION_LAST_SEEN: &str = "lastSeenMs";
/// The forgotten-song field that takes the maximum.
pub const FORGOTTEN_AT: &str = "forgottenAtMs";
/// A playlist entry's playlist, which never changes.
pub const ENTRY_PLAYLIST: &str = "playlistID";

/// What the server keeps beside the fields and never sends: per-field times for the
/// newest-wins fields, and a session's owner.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub t: BTreeMap<String, i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl Meta {
    pub fn is_empty(&self) -> bool {
        self.t.is_empty() && self.owner.is_none()
    }
}

/// A record as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Stored {
    pub id: String,
    pub version: i64,
    /// `None` for a tombstone.
    pub fields: Option<Fields>,
    pub meta: Meta,
    pub parent: Option<String>,
    pub device_id: String,
    pub modified_at: Millis,
    pub deleted_at: Option<Millis>,
    pub merged_into: Option<String>,
    pub bytes: i64,
}

impl Stored {
    /// The record as a client sees it.
    pub fn record(&self) -> Record {
        Record {
            id: self.id.clone(),
            version: self.version,
            device_id: self.device_id.clone(),
            client_modified_at: self.modified_at,
            body: match &self.fields {
                Some(fields) => Body::Live(fields.clone()),
                None => Body::Deleted {
                    deleted_at: self.deleted_at.unwrap_or(self.modified_at),
                    merged_into: self.merged_into.clone(),
                },
            },
        }
    }
}

/// What to store next.
#[derive(Clone, Debug, PartialEq)]
pub struct Next {
    pub fields: Option<Fields>,
    pub meta: Meta,
    pub parent: Option<String>,
    pub modified_at: Millis,
    pub deleted_at: Option<Millis>,
    pub merged_into: Option<String>,
}

/// The verdict on one item.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// Store this as a new version.
    Write(Next),
    /// Nothing a client sees changes. `touch` carries newer per-field times or a newer
    /// `client_modified_at` to keep without a new version.
    Duplicate { touch: Option<(Meta, Millis)> },
    /// The stored copy stands.
    Conflict,
    Reject(Reject),
}

fn without_nulls(fields: &Fields) -> Fields {
    fields
        .iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn text<'a>(fields: &'a Fields, name: &str) -> Option<&'a str> {
    fields.get(name).and_then(Value::as_str)
}

fn cas_fails(item: &PushItem, stored: &Stored) -> bool {
    item.base_version.is_some_and(|v| v != stored.version)
}

fn owner(stored: &Stored) -> &str {
    stored.meta.owner.as_deref().unwrap_or(&stored.device_id)
}

/// Decides what happens to `item` given the stored copy. `now` is the server's time.
pub fn decide(
    entity: Entity,
    stored: Option<&Stored>,
    item: &PushItem,
    registry: &Registry,
    now: Millis,
) -> Decision {
    let rule = entity.rule();
    if item.is_delete() {
        return delete(rule, stored, item, registry);
    }
    let Some(incoming) = &item.fields else {
        return Decision::Reject(Reject::Invalid);
    };
    match stored {
        None => {
            if item.base_version.is_some_and(|v| v != 0) {
                return Decision::Conflict;
            }
            fresh(entity, incoming, item, now)
        }
        Some(s) if s.fields.is_none() => {
            let deleted_at = s.deleted_at.unwrap_or(s.modified_at);
            if rule == Rule::LastWriter {
                if item.client_modified_at <= deleted_at {
                    return Decision::Conflict;
                }
            } else if !item.undelete {
                return Decision::Reject(Reject::Deleted);
            }
            if rule == Rule::Owned && !registry.succeeds(&item.device_id, owner(s)) {
                return Decision::Reject(Reject::NotOwner);
            }
            if cas_fails(item, s) {
                return Decision::Conflict;
            }
            fresh(entity, incoming, item, now)
        }
        Some(s) => live(entity, s, incoming, item, registry),
    }
}

fn delete(rule: Rule, stored: Option<&Stored>, item: &PushItem, registry: &Registry) -> Decision {
    if rule == Rule::GrowOnly {
        return Decision::Reject(Reject::GrowOnly);
    }
    let deleted_at = item.deleted_at.unwrap_or(item.client_modified_at);
    let tombstone = |meta: Meta, parent: Option<String>| {
        Decision::Write(Next {
            fields: None,
            meta,
            parent,
            modified_at: deleted_at,
            deleted_at: Some(deleted_at),
            merged_into: item.merged_into.clone(),
        })
    };
    match stored {
        None if item.base_version.is_some_and(|v| v != 0) => Decision::Conflict,
        None => {
            let meta = Meta {
                owner: (rule == Rule::Owned).then(|| item.device_id.clone()),
                ..Meta::default()
            };
            tombstone(meta, None)
        }
        Some(s) if s.fields.is_none() => Decision::Duplicate { touch: None },
        Some(s) => {
            if rule == Rule::Owned && !registry.succeeds(&item.device_id, owner(s)) {
                return Decision::Reject(Reject::NotOwner);
            }
            if rule == Rule::LastWriter && deleted_at <= s.modified_at {
                return Decision::Conflict;
            }
            if cas_fails(item, s) {
                return Decision::Conflict;
            }
            let meta = Meta {
                owner: s.meta.owner.clone(),
                ..Meta::default()
            };
            tombstone(meta, s.parent.clone())
        }
    }
}

/// A record stored for the first time (or again after an undelete).
fn fresh(entity: Entity, incoming: &Fields, item: &PushItem, now: Millis) -> Decision {
    let mut fields = without_nulls(incoming);
    let mut meta = Meta::default();
    let at = item.client_modified_at.0;
    let mut parent = None;
    let needs = |names: &[&str], fields: &Fields| names.iter().all(|n| text(fields, n).is_some());
    match entity.rule() {
        Rule::Play => {
            let Some(played_at) = fields.get("playedAtMs").and_then(Value::as_i64) else {
                return Decision::Reject(Reject::Invalid);
            };
            if !needs(&["title", "artistName"], &fields) {
                return Decision::Reject(Reject::Invalid);
            }
            if !not_ahead(Millis(played_at), now) {
                return Decision::Reject(Reject::Future);
            }
            if !fields.contains_key("songKey") {
                let key = tracks_model::song_key(
                    text(&fields, "title").unwrap_or_default(),
                    text(&fields, "artistName").unwrap_or_default(),
                );
                fields.insert(String::from("songKey"), Value::String(key));
            }
            for name in PLAY_NEWEST {
                if fields.contains_key(*name) {
                    meta.t.insert((*name).to_owned(), at);
                }
            }
        }
        Rule::Owned => {
            let owner = text(&fields, "recordedByDeviceID")
                .or_else(|| text(&fields, "deviceID"))
                .unwrap_or(&item.device_id);
            meta.owner = Some(owner.to_owned());
        }
        Rule::Station => {
            if !needs(&["name"], &fields) {
                return Decision::Reject(Reject::Invalid);
            }
        }
        Rule::LastWriter => {
            if entity != Entity::Likes && !needs(&["name"], &fields) {
                return Decision::Reject(Reject::Invalid);
            }
        }
        Rule::PerField => {
            if entity == Entity::PlaylistEntries {
                if !needs(&[ENTRY_PLAYLIST, "order"], &fields) {
                    return Decision::Reject(Reject::Invalid);
                }
                parent = text(&fields, ENTRY_PLAYLIST).map(str::to_owned);
            }
            meta.t = fields.keys().map(|k| (k.clone(), at)).collect();
        }
        Rule::GrowOnly => {}
        Rule::Setting => return Decision::Reject(Reject::Invalid),
    }
    Decision::Write(Next {
        fields: Some(fields),
        meta,
        parent,
        modified_at: item.client_modified_at,
        deleted_at: None,
        merged_into: None,
    })
}

/// The outcome of merging into a live record.
struct Merged {
    fields: Fields,
    meta: Meta,
    /// Part of the push lost to the stored copy.
    lost: bool,
    /// The newest-wins time moved even though no value did.
    touched: bool,
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64()
}

/// First non-null wins: returns whether the incoming value lost.
fn first_non_null(fields: &mut Fields, name: &str, value: &Value) -> bool {
    match fields.get(name) {
        None | Some(Value::Null) => {
            fields.insert(name.to_owned(), value.clone());
            false
        }
        Some(old) => old != value,
    }
}

/// Keeps the smaller (`min`) or larger number. Values that are not numbers fall back to first
/// non-null.
fn extreme(fields: &mut Fields, name: &str, value: &Value, min: bool) -> bool {
    match (fields.get(name).and_then(number), number(value)) {
        (Some(old), Some(new)) => {
            if (min && new < old) || (!min && new > old) {
                fields.insert(name.to_owned(), value.clone());
            }
            false
        }
        _ => first_non_null(fields, name, value),
    }
}

/// Newest wins for one field, by its per-field time.
fn newest(
    merged: &mut Merged,
    name: &str,
    value: &Value,
    at: i64,
) {
    let current = merged.meta.t.get(name).copied().unwrap_or(i64::MIN);
    let differs = match merged.fields.get(name) {
        None => !value.is_null(),
        Some(old) => old != value,
    };
    if at > current {
        merged.meta.t.insert(name.to_owned(), at);
        merged.touched = true;
        if value.is_null() {
            merged.fields.remove(name);
        } else {
            merged.fields.insert(name.to_owned(), value.clone());
        }
    } else if differs {
        merged.lost = true;
    }
}

fn live(
    entity: Entity,
    stored: &Stored,
    incoming: &Fields,
    item: &PushItem,
    registry: &Registry,
) -> Decision {
    let rule = entity.rule();
    let at = item.client_modified_at.0;
    let current = stored.fields.clone().unwrap_or_default();
    let mut merged = Merged {
        fields: current.clone(),
        meta: stored.meta.clone(),
        lost: false,
        touched: false,
    };
    match rule {
        Rule::Play => {
            for (name, value) in incoming.iter().filter(|(_, v)| !v.is_null()) {
                if PLAY_NEWEST.contains(&name.as_str()) {
                    newest(&mut merged, name, value, at);
                } else if first_non_null(&mut merged.fields, name, value) {
                    if PLAY_CORE.contains(&name.as_str()) {
                        return Decision::Conflict;
                    }
                    merged.lost = true;
                }
            }
        }
        Rule::Owned => {
            if !registry.succeeds(&item.device_id, owner(stored)) {
                return Decision::Reject(Reject::NotOwner);
            }
            for (name, value) in incoming {
                if value.is_null() {
                    merged.fields.remove(name);
                } else {
                    merged.fields.insert(name.clone(), value.clone());
                }
            }
        }
        Rule::Station | Rule::GrowOnly => {
            for (name, value) in incoming.iter().filter(|(_, v)| !v.is_null()) {
                let lost = match name.as_str() {
                    STATION_FIRST_SEEN if rule == Rule::Station => {
                        extreme(&mut merged.fields, name, value, true)
                    }
                    STATION_LAST_SEEN if rule == Rule::Station => {
                        extreme(&mut merged.fields, name, value, false)
                    }
                    FORGOTTEN_AT if rule == Rule::GrowOnly => {
                        extreme(&mut merged.fields, name, value, false)
                    }
                    _ => first_non_null(&mut merged.fields, name, value),
                };
                merged.lost |= lost;
            }
        }
        Rule::LastWriter => {
            let replacement = without_nulls(incoming);
            if item.client_modified_at > stored.modified_at {
                merged.fields = replacement;
                merged.touched = true;
            } else if replacement != current {
                merged.lost = true;
            }
        }
        Rule::PerField => {
            for (name, value) in incoming {
                if entity == Entity::PlaylistEntries
                    && name == ENTRY_PLAYLIST
                    && current.get(name).is_some_and(|old| old != value)
                {
                    return Decision::Conflict;
                }
                newest(&mut merged, name, value, at);
            }
        }
        Rule::Setting => return Decision::Reject(Reject::Invalid),
    }
    if merged.fields == current {
        let touch = (merged.touched && (rule == Rule::LastWriter || merged.meta != stored.meta))
            .then(|| (merged.meta.clone(), item.client_modified_at.max(stored.modified_at)));
        return if merged.lost {
            Decision::Conflict
        } else {
            Decision::Duplicate { touch }
        };
    }
    if cas_fails(item, stored) {
        return Decision::Conflict;
    }
    Decision::Write(Next {
        fields: Some(merged.fields),
        meta: merged.meta,
        parent: stored.parent.clone(),
        modified_at: item.client_modified_at.max(stored.modified_at),
        deleted_at: None,
        merged_into: None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NOW: Millis = Millis(1_800_000_000_000);

    fn push(id: &str, fields: Value, at: i64, device: &str) -> PushItem {
        PushItem {
            id: id.to_owned(),
            base_version: None,
            fields: match fields {
                Value::Object(map) => Some(map),
                _ => None,
            },
            client_modified_at: Millis(at),
            device_id: device.to_owned(),
            deleted_at: None,
            merged_into: None,
            undelete: false,
        }
    }

    fn tombstone(id: &str, at: i64, device: &str) -> PushItem {
        PushItem {
            fields: None,
            deleted_at: Some(Millis(at)),
            ..push(id, json!({}), at, device)
        }
    }

    /// Applies a decision the way the store would, for chaining.
    fn apply(stored: Option<Stored>, decision: &Decision, item: &PushItem) -> Option<Stored> {
        match decision {
            Decision::Write(next) => Some(Stored {
                id: item.id.clone(),
                version: stored.as_ref().map_or(1, |s| s.version + 1),
                fields: next.fields.clone(),
                meta: next.meta.clone(),
                parent: next.parent.clone(),
                device_id: item.device_id.clone(),
                modified_at: next.modified_at,
                deleted_at: next.deleted_at,
                merged_into: next.merged_into.clone(),
                bytes: 0,
            }),
            Decision::Duplicate { touch: Some((meta, at)) } => stored.map(|mut s| {
                s.meta = meta.clone();
                s.modified_at = *at;
                s
            }),
            _ => stored,
        }
    }

    fn run(entity: Entity, stored: Option<Stored>, item: &PushItem) -> (Decision, Option<Stored>) {
        let decision = decide(entity, stored.as_ref(), item, &Registry::default(), NOW);
        let next = apply(stored, &decision, item);
        (decision, next)
    }

    fn fields(stored: Option<&Stored>) -> Value {
        Value::Object(stored.and_then(|s| s.fields.clone()).unwrap_or_default())
    }

    fn play() -> Value {
        json!({"title": "Halo", "artistName": "Beyoncé", "playedAtMs": 1_790_000_000_000_i64,
               "kind": "onDemand", "source": "appleMusic", "recordedByDeviceID": "D1"})
    }

    #[test]
    fn plays_fill_enrichments_once_and_refuse_core_changes() {
        let first = push("p1", play(), 10, "D1");
        let (d, s) = run(Entity::Plays, None, &first);
        assert!(matches!(d, Decision::Write(_)));
        assert_eq!(fields(s.as_ref())["songKey"], "halo\u{1f}beyonce");
        let (d, s) = run(Entity::Plays, s, &first);
        assert_eq!(d, Decision::Duplicate { touch: None }, "replay");
        let enrich = push("p1", json!({"catalogID": "123", "scrobbledAt": 5}), 20, "D2");
        let (d, s) = run(Entity::Plays, s, &enrich);
        assert!(matches!(d, Decision::Write(_)));
        let other = push("p1", json!({"catalogID": "999"}), 30, "D3");
        let (d, s) = run(Entity::Plays, s, &other);
        assert_eq!(d, Decision::Conflict, "first non-null wins");
        assert_eq!(fields(s.as_ref())["catalogID"], "123");
        let retitle = push("p1", json!({"title": "Halo (Live)"}), 40, "D1");
        assert_eq!(run(Entity::Plays, s.clone(), &retitle).0, Decision::Conflict);
        let art = push("p1", json!({"artworkURL": "https://a/2"}), 50, "D2");
        let (d, s) = run(Entity::Plays, s, &art);
        assert!(matches!(d, Decision::Write(_)));
        let stale_art = push("p1", json!({"artworkURL": "https://a/1"}), 45, "D1");
        let (d, s) = run(Entity::Plays, s, &stale_art);
        assert_eq!(d, Decision::Conflict, "artworkURL is newest-wins");
        assert_eq!(fields(s.as_ref())["artworkURL"], "https://a/2");
    }

    #[test]
    fn plays_need_their_core_and_not_the_future() {
        let bare = push("p1", json!({"title": "Halo"}), 10, "D1");
        assert_eq!(run(Entity::Plays, None, &bare).0, Decision::Reject(Reject::Invalid));
        let mut ahead = play();
        ahead["playedAtMs"] = json!(NOW.0 + 3 * 86_400_000);
        let item = push("p1", ahead, 10, "D1");
        assert_eq!(run(Entity::Plays, None, &item).0, Decision::Reject(Reject::Future));
    }

    #[test]
    fn tombstones_refuse_upserts_until_an_explicit_undelete() {
        let (_, s) = run(Entity::Plays, None, &push("p1", play(), 10, "D1"));
        let mut merged = tombstone("p1", 20, "D1");
        merged.merged_into = Some(String::from("p0"));
        let (d, s) = run(Entity::Plays, s, &merged);
        assert!(matches!(d, Decision::Write(_)));
        assert_eq!(
            s.as_ref().map(Stored::record).map(|r| r.body),
            Some(Body::Deleted {
                deleted_at: Millis(20),
                merged_into: Some(String::from("p0"))
            })
        );
        assert_eq!(run(Entity::Plays, s.clone(), &merged).0, Decision::Duplicate { touch: None });
        let again = push("p1", play(), 30, "D2");
        assert_eq!(run(Entity::Plays, s.clone(), &again).0, Decision::Reject(Reject::Deleted));
        let undelete = PushItem {
            undelete: true,
            ..again
        };
        let (d, s) = run(Entity::Plays, s, &undelete);
        assert!(matches!(d, Decision::Write(_)));
        assert_eq!(fields(s.as_ref())["title"], "Halo");
    }

    #[test]
    fn base_version_is_compare_and_set_but_replays_stay_duplicates() {
        let item = push("s1", json!({"name": "Morning"}), 10, "D1");
        let (_, s) = run(Entity::Playlists, None, &item);
        let version = s.as_ref().map_or(0, |s| s.version);
        let stale = PushItem {
            base_version: Some(version + 5),
            ..push("s1", json!({"name": "Evening"}), 20, "D1")
        };
        assert_eq!(run(Entity::Playlists, s.clone(), &stale).0, Decision::Conflict);
        let fresh_cas = PushItem {
            base_version: Some(version),
            ..push("s1", json!({"name": "Evening"}), 20, "D1")
        };
        let (d, s) = run(Entity::Playlists, s, &fresh_cas);
        assert!(matches!(d, Decision::Write(_)));
        let (d, _) = run(Entity::Playlists, s, &fresh_cas);
        assert!(matches!(d, Decision::Duplicate { .. }), "a replay is not a conflict");
        let must_be_new = PushItem {
            base_version: Some(3),
            ..push("s2", json!({"name": "x"}), 10, "D1")
        };
        assert_eq!(run(Entity::Playlists, None, &must_be_new).0, Decision::Conflict);
    }

    #[test]
    fn stations_merge_by_extremes_and_first_values() {
        let first = push("radio x", json!({"name": "Radio X", "firstSeenMs": 100, "lastSeenMs": 200}), 1, "D1");
        let (_, s) = run(Entity::Stations, None, &first);
        let second = push("radio x", json!({"name": "RADIO X", "firstSeenMs": 50, "lastSeenMs": 150,
                                       "catalogID": "ra.1"}), 2, "D2");
        let (d, s) = run(Entity::Stations, s, &second);
        assert!(matches!(d, Decision::Write(_)));
        assert_eq!(
            fields(s.as_ref()),
            json!({"name": "Radio X", "firstSeenMs": 50, "lastSeenMs": 200, "catalogID": "ra.1"})
        );
        let third = push("radio x", json!({"lastSeenMs": 300, "catalogID": "ra.2"}), 3, "D3");
        let (_, s) = run(Entity::Stations, s, &third);
        assert_eq!(fields(s.as_ref())["lastSeenMs"], 300);
        assert_eq!(fields(s.as_ref())["catalogID"], "ra.1");
    }

    #[test]
    fn forgotten_songs_only_grow_and_keep_the_latest_date() {
        let id = "halo\u{1f}beyonce";
        let (_, s) = run(Entity::ForgottenSongs, None, &push(id, json!({"forgottenAtMs": 10}), 1, "D1"));
        let (_, s) = run(Entity::ForgottenSongs, s, &push(id, json!({"forgottenAtMs": 30}), 2, "D2"));
        let (_, s) = run(Entity::ForgottenSongs, s, &push(id, json!({"forgottenAtMs": 20}), 3, "D3"));
        assert_eq!(fields(s.as_ref())["forgottenAtMs"], 30);
        assert_eq!(
            run(Entity::ForgottenSongs, s, &tombstone(id, 4, "D1")).0,
            Decision::Reject(Reject::GrowOnly)
        );
    }

    #[test]
    fn per_item_lists_are_last_writer_wins_with_tombstones() {
        let add = push("abba", json!({"name": "ABBA"}), 10, "D1");
        let (_, s) = run(Entity::BlockedArtists, None, &add);
        let late_remove = tombstone("abba", 5, "D2");
        assert_eq!(run(Entity::BlockedArtists, s.clone(), &late_remove).0, Decision::Conflict);
        let (d, s) = run(Entity::BlockedArtists, s, &tombstone("abba", 20, "D2"));
        assert!(matches!(d, Decision::Write(_)));
        assert_eq!(
            run(Entity::BlockedArtists, s.clone(), &push("abba", json!({"name": "ABBA"}), 15, "D1")).0,
            Decision::Conflict,
            "an add older than the remove loses"
        );
        let (d, s) = run(Entity::BlockedArtists, s, &push("abba", json!({"name": "ABBA"}), 30, "D1"));
        assert!(matches!(d, Decision::Write(_)), "a newer add brings it back without undelete");
        assert!(s.is_some_and(|s| s.fields.is_some()));
    }

    #[test]
    fn same_value_at_a_newer_time_still_beats_an_older_remove() {
        let (_, s) = run(Entity::Likes, None, &push("a\u{1f}b", json!({"loved": true}), 10, "D1"));
        let (d, s) = run(Entity::Likes, s, &push("a\u{1f}b", json!({"loved": true}), 30, "D2"));
        assert!(matches!(d, Decision::Duplicate { touch: Some(_) }));
        assert_eq!(
            run(Entity::Likes, s, &tombstone("a\u{1f}b", 20, "D3")).0,
            Decision::Conflict
        );
    }

    #[test]
    fn playlists_are_newest_wins_per_field() {
        let (_, s) = run(Entity::Playlists, None, &push("pl", json!({"name": "A", "comment": "x"}), 10, "D1"));
        let rename = push("pl", json!({"name": "B"}), 30, "D1");
        let recomment = push("pl", json!({"comment": "y", "name": "Old"}), 20, "D2");
        let (_, s) = run(Entity::Playlists, s, &rename);
        let (d, s) = run(Entity::Playlists, s, &recomment);
        assert!(matches!(d, Decision::Write(_)));
        assert_eq!(fields(s.as_ref()), json!({"name": "B", "comment": "y"}));
        let clear = push("pl", json!({"comment": null}), 40, "D2");
        let (_, s) = run(Entity::Playlists, s, &clear);
        assert_eq!(fields(s.as_ref()), json!({"name": "B"}));
    }

    #[test]
    fn playlist_entries_need_a_playlist_and_keep_it() {
        let bare = push("e1", json!({"order": "a0"}), 1, "D1");
        assert_eq!(run(Entity::PlaylistEntries, None, &bare).0, Decision::Reject(Reject::Invalid));
        let entry = push("e1", json!({"playlistID": "pl", "order": "a0",
                                      "track": {"title": "Halo", "artist": "Beyoncé", "ids": {"appleMusic": "1"}}}), 1, "D1");
        let (_, s) = run(Entity::PlaylistEntries, None, &entry);
        assert_eq!(s.as_ref().and_then(|s| s.parent.clone()).as_deref(), Some("pl"));
        let moved = push("e1", json!({"order": "a1"}), 2, "D2");
        let (d, s) = run(Entity::PlaylistEntries, s, &moved);
        assert!(matches!(d, Decision::Write(_)));
        let elsewhere = push("e1", json!({"playlistID": "other"}), 3, "D2");
        assert_eq!(run(Entity::PlaylistEntries, s, &elsewhere).0, Decision::Conflict);
    }

    #[test]
    fn sessions_are_written_by_their_owner_and_its_successors() {
        let registry = Registry::new([(String::from("D2"), vec![String::from("D1")])]);
        let create = push("s1", json!({"startedAtMs": 1}), 1, "D1");
        let first = decide(Entity::Sessions, None, &create, &registry, NOW);
        let s = apply(None, &first, &create);
        assert_eq!(s.as_ref().and_then(|s| s.meta.owner.clone()).as_deref(), Some("D1"));
        let stranger = push("s1", json!({"endedAtMs": 2}), 2, "D3");
        assert_eq!(
            decide(Entity::Sessions, s.as_ref(), &stranger, &registry, NOW),
            Decision::Reject(Reject::NotOwner)
        );
        let restored = push("s1", json!({"endedAtMs": 2}), 2, "D2");
        assert!(matches!(
            decide(Entity::Sessions, s.as_ref(), &restored, &registry, NOW),
            Decision::Write(_)
        ));
        assert_eq!(
            decide(Entity::Sessions, s.as_ref(), &tombstone("s1", 3, "D3"), &registry, NOW),
            Decision::Reject(Reject::NotOwner)
        );
    }

    #[test]
    fn unknown_fields_round_trip() {
        let mut fields_in = play();
        fields_in["futureField"] = json!({"nested": [1, 2]});
        fields_in["kind"] = json!("someNewKind");
        let (_, s) = run(Entity::Plays, None, &push("p1", fields_in.clone(), 1, "D1"));
        let stored = fields(s.as_ref());
        assert_eq!(stored["futureField"], fields_in["futureField"]);
        assert_eq!(stored["kind"], "someNewKind");
    }
}
