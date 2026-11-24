//! Size and shape checks on pushed items, before any merge rule runs.

use serde_json::Value;
use trackside_core::Millis;

use crate::entity::{Entity, Key};
use crate::limits::{
    MAX_ARRAY, MAX_CLOCK_AHEAD, MAX_DEPTH, MAX_DEVICE_ID_BYTES, MAX_FIELD_NAME_BYTES,
    MAX_FIELDS_BYTES, MAX_ID_BYTES, MAX_KEYS, MAX_STRING_BYTES,
};
use crate::model::{Fields, PushItem, Reject};

/// Text without control characters (apart from the separator inside song identities), at
/// most `max` bytes, and not empty.
pub fn is_label(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && !text
            .chars()
            .any(|c| c.is_control() && c != tracks_model::KEY_SEPARATOR)
}

/// Whether a client time is no further ahead of `now` than a day.
pub fn not_ahead(at: Millis, now: Millis) -> bool {
    at.0 <= now.0.saturating_add(MAX_CLOCK_AHEAD.as_millis() as i64)
}

fn check_value(value: &Value, depth: usize) -> Result<(), Reject> {
    match value {
        Value::String(s) if s.len() > MAX_STRING_BYTES => Err(Reject::TooLarge),
        Value::Array(items) => {
            if items.len() > MAX_ARRAY || depth >= MAX_DEPTH {
                return Err(Reject::TooLarge);
            }
            items.iter().try_for_each(|v| check_value(v, depth + 1))
        }
        Value::Object(map) => {
            if map.len() > MAX_KEYS || depth >= MAX_DEPTH {
                return Err(Reject::TooLarge);
            }
            for (k, v) in map {
                if k.len() > MAX_FIELD_NAME_BYTES {
                    return Err(Reject::TooLarge);
                }
                check_value(v, depth + 1)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Checks the fields' size and nesting.
pub fn check_fields(fields: &Fields) -> Result<(), Reject> {
    if fields.len() > MAX_KEYS {
        return Err(Reject::TooLarge);
    }
    for (k, v) in fields {
        if k.is_empty() || k.len() > MAX_FIELD_NAME_BYTES {
            return Err(Reject::Invalid);
        }
        check_value(v, 1)?;
    }
    let bytes = serde_json::to_string(fields).map_or(usize::MAX, |s| s.len());
    if bytes > MAX_FIELDS_BYTES {
        return Err(Reject::TooLarge);
    }
    Ok(())
}

fn text<'a>(fields: Option<&'a Fields>, name: &str) -> Option<&'a str> {
    fields?.get(name)?.as_str()
}

/// The id a keyed entity's fields give, when they give one.
fn derived_key(entity: Entity, fields: Option<&Fields>) -> Option<String> {
    match entity.key() {
        Key::FoldedName => text(fields, "name").map(tracks_model::folded),
        Key::SongIdentity => Some(tracks_model::song_key(
            text(fields, "title")?,
            text(fields, "artistName")?,
        )),
        Key::ClientId | Key::SettingName => None,
    }
}

/// Checks one item and settles its id: a keyed entity may leave `id` empty and get the key its
/// fields give. Returns the id to store under.
pub fn check_item(entity: Entity, item: &PushItem, now: Millis) -> Result<String, Reject> {
    if !is_label(&item.device_id, MAX_DEVICE_ID_BYTES) {
        return Err(Reject::Invalid);
    }
    if !not_ahead(item.client_modified_at, now) || item.deleted_at.is_some_and(|at| !not_ahead(at, now))
    {
        return Err(Reject::Future);
    }
    if item.merged_into.as_ref().is_some_and(|m| !is_label(m, MAX_ID_BYTES))
        || item.base_version.is_some_and(|v| v < 0)
    {
        return Err(Reject::Invalid);
    }
    let fields = if item.is_delete() {
        None
    } else {
        let fields = item.fields.as_ref().ok_or(Reject::Invalid)?;
        check_fields(fields)?;
        Some(fields)
    };
    let derived = derived_key(entity, fields);
    let id = match (item.id.is_empty(), derived) {
        (true, Some(key)) => key,
        (false, Some(key)) if key != item.id => return Err(Reject::IdentityMismatch),
        _ => item.id.clone(),
    };
    if !is_label(&id, MAX_ID_BYTES) {
        return Err(Reject::Invalid);
    }
    if entity.key() == Key::SongIdentity && !id.contains(tracks_model::KEY_SEPARATOR) {
        return Err(Reject::IdentityMismatch);
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn item(id: &str, fields: Value) -> PushItem {
        PushItem {
            id: id.to_owned(),
            base_version: None,
            fields: match fields {
                Value::Object(map) => Some(map),
                _ => None,
            },
            client_modified_at: Millis(1000),
            device_id: String::from("D1"),
            deleted_at: None,
            merged_into: None,
            undelete: false,
        }
    }

    #[test]
    fn keyed_entities_derive_and_check_their_ids() {
        let now = Millis(2000);
        assert_eq!(
            check_item(Entity::Stations, &item("", json!({"name": " Radio Café "})), now),
            Ok(String::from("radio cafe"))
        );
        assert_eq!(
            check_item(Entity::Stations, &item("other", json!({"name": "Radio"})), now),
            Err(Reject::IdentityMismatch)
        );
        assert_eq!(
            check_item(
                Entity::Likes,
                &item("", json!({"title": "Halo", "artistName": "Beyoncé", "loved": true})),
                now
            ),
            Ok(String::from("halo\u{1f}beyonce"))
        );
        assert_eq!(
            check_item(Entity::Likes, &item("halo", json!({"loved": true})), now),
            Err(Reject::IdentityMismatch)
        );
    }

    #[test]
    fn sizes_and_times_are_limited() {
        let now = Millis(1000);
        let long = "x".repeat(MAX_STRING_BYTES + 1);
        assert_eq!(
            check_item(Entity::Plays, &item("p1", json!({"title": long})), now),
            Err(Reject::TooLarge)
        );
        let deep = json!({"a": {"b": {"c": {"d": {"e": 1}}}}});
        assert_eq!(
            check_item(Entity::Plays, &item("p1", deep), now),
            Err(Reject::TooLarge)
        );
        let mut ahead = item("p1", json!({}));
        ahead.client_modified_at = Millis(1000 + 2 * 86_400_000);
        assert_eq!(check_item(Entity::Plays, &ahead, now), Err(Reject::Future));
        let mut no_device = item("p1", json!({}));
        no_device.device_id = String::new();
        assert_eq!(check_item(Entity::Plays, &no_device, now), Err(Reject::Invalid));
    }
}
