//! Small helpers shared by the repositories: access filters, id lists, cursors, paths and
//! integer conversions.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::access::{Access, LibrarySet};
use crate::db::DbError;
use crate::ids::LibraryKey;

/// A SQL condition on `column` (an integer library id) that holds for libraries the caller may
/// see, further narrowed to `only` when given. Library keys are integers, so writing them into
/// the statement is safe. The column is written as `+column` so SQLite never drives a query
/// from the `(library_id, ...)` indexes: with few libraries that would read most of the table
/// and sort it, where walking the index of the requested order and filtering is cheap.
pub(crate) fn visible(a: &Access, column: &str, only: Option<LibraryKey>) -> String {
    let allowed: Option<Vec<i64>> = match (&a.libraries, only) {
        (LibrarySet::All, None) => None,
        (LibrarySet::All, Some(key)) => Some(vec![key.0]),
        (LibrarySet::Only(keys), None) => Some(keys.iter().map(|k| k.0).collect()),
        (LibrarySet::Only(keys), Some(key)) => {
            Some(keys.iter().filter(|k| **k == key).map(|k| k.0).collect())
        }
    };
    match allowed {
        None => String::from("1"),
        Some(keys) if keys.is_empty() => String::from("0"),
        Some(keys) => {
            let mut out = format!("+{column} IN (");
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{key}");
            }
            out.push(')');
            out
        }
    }
}

/// A JSON array of strings, bound as one parameter for `IN (SELECT value FROM json_each(?))`.
pub(crate) fn json_strings<I, S>(items: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = String::from("[");
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::Value::String(item.as_ref().to_owned()).to_string());
    }
    out.push(']');
    out
}

/// A JSON array of integers, for `IN (SELECT value FROM json_each(?))`.
pub(crate) fn json_ints<I: IntoIterator<Item = i64>>(items: I) -> String {
    let mut out = String::from("[");
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{item}");
    }
    out.push(']');
    out
}

/// An opaque list cursor: base64url of the JSON of the last item's sort key.
pub(crate) fn encode_cursor<T: Serialize>(key: &T) -> String {
    // The keys are plain integers and strings, which always serialize.
    let json = serde_json::to_vec(key).unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

/// Reads a cursor made by [`encode_cursor`]. A malformed one is `Invalid("cursor")`.
pub(crate) fn decode_cursor<T: DeserializeOwned>(cursor: &str) -> Result<T, DbError> {
    let bad = || DbError::Invalid(String::from("cursor"));
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor.trim())
        .map_err(|_| bad())?;
    serde_json::from_slice(&bytes).map_err(|_| bad())
}

/// The exact OS bytes of a path.
pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

/// A path from stored OS bytes.
pub(crate) fn bytes_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// `root` joined with a stored relative path. An empty relative path is the root itself.
pub(crate) fn join_rel(root: &Path, rel_path: &[u8]) -> PathBuf {
    if rel_path.is_empty() {
        root.to_path_buf()
    } else {
        root.join(bytes_path(rel_path))
    }
}

/// The parent of a relative path (`a/b` for `a/b/c.flac`, empty for `c.flac`).
pub(crate) fn rel_parent(rel_path: &[u8]) -> &[u8] {
    match rel_path.iter().rposition(|&b| b == b'/') {
        Some(i) => &rel_path[..i],
        None => &[],
    }
}

/// The last component of a relative path.
pub(crate) fn rel_name(rel_path: &[u8]) -> &[u8] {
    match rel_path.iter().rposition(|&b| b == b'/') {
        Some(i) => &rel_path[i + 1..],
        None => rel_path,
    }
}

/// A stored non-negative integer as `u32`, saturating.
pub(crate) fn to_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

/// A stored non-negative integer as `u64`.
pub(crate) fn to_u64(value: i64) -> u64 {
    u64::try_from(value.max(0)).unwrap_or(0)
}

/// An optional stored integer as `u32`.
pub(crate) fn opt_u32(value: Option<i64>) -> Option<u32> {
    value.map(to_u32)
}

/// An optional stored integer as `u8`.
pub(crate) fn opt_u8(value: Option<i64>) -> Option<u8> {
    value.map(|v| u8::try_from(v.max(0)).unwrap_or(u8::MAX))
}

/// A `u64` for storage.
pub(crate) fn from_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// A stored 32-byte hash.
pub(crate) fn hash32(bytes: Option<Vec<u8>>) -> Option<[u8; 32]> {
    bytes.and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
}

/// Maps a uniqueness violation to `Conflict(what)`, leaving other errors as they are.
pub(crate) fn conflict_on_unique(e: rusqlite::Error, what: &str) -> DbError {
    match &e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            DbError::Conflict(what.to_owned())
        }
        _ => DbError::Sqlite(e),
    }
}

/// A limit clamped to `1..=max`, with `0` meaning `default`.
pub(crate) fn clamp_limit(limit: u32, default: u32, max: u32) -> u32 {
    if limit == 0 { default } else { limit.min(max) }
}

/// One column of a keyset ordering. `expr` must never be NULL.
#[derive(Clone, Debug)]
pub(crate) struct OrderKey {
    pub expr: String,
    pub desc: bool,
}

impl OrderKey {
    pub(crate) fn asc(expr: impl Into<String>) -> Self {
        Self {
            expr: expr.into(),
            desc: false,
        }
    }

    pub(crate) fn desc(expr: impl Into<String>) -> Self {
        Self {
            expr: expr.into(),
            desc: true,
        }
    }
}

/// `ORDER BY` for the keys.
pub(crate) fn order_by(keys: &[OrderKey]) -> String {
    let parts: Vec<String> = keys
        .iter()
        .map(|k| format!("{} {}", k.expr, if k.desc { "DESC" } else { "ASC" }))
        .collect();
    format!("ORDER BY {}", parts.join(", "))
}

/// The select-list tail that returns each key's value, for building the next cursor.
pub(crate) fn key_columns(keys: &[OrderKey]) -> String {
    let mut out = String::new();
    for key in keys {
        let _ = write!(out, ", {}", key.expr);
    }
    out
}

/// A condition for rows strictly after `after` in the keys' order, with its parameters in the
/// order they appear. Empty `after` means the first page.
pub(crate) fn after_condition(
    keys: &[OrderKey],
    after: &[rusqlite::types::Value],
) -> (String, Vec<rusqlite::types::Value>) {
    if after.len() != keys.len() || keys.is_empty() {
        return (String::from("1"), Vec::new());
    }
    let mut sql = String::new();
    let mut params = Vec::new();
    let mut closers = 0;
    for (i, (key, value)) in keys.iter().zip(after).enumerate() {
        let op = if key.desc { "<" } else { ">" };
        if i + 1 == keys.len() {
            let _ = write!(sql, "{} {op} ?", key.expr);
            params.push(value.clone());
        } else {
            let _ = write!(sql, "({} {op} ? OR ({} = ? AND ", key.expr, key.expr);
            params.push(value.clone());
            params.push(value.clone());
            closers += 1;
        }
    }
    for _ in 0..closers {
        sql.push_str("))");
    }
    (format!("({sql})"), params)
}

/// A keyset cursor from stored values.
pub(crate) fn encode_keyset(values: &[rusqlite::types::Value]) -> String {
    use rusqlite::types::Value;
    let json: Vec<serde_json::Value> = values
        .iter()
        .map(|v| match v {
            Value::Integer(i) => serde_json::Value::from(*i),
            Value::Real(r) => serde_json::Value::from(*r),
            Value::Text(t) => serde_json::Value::from(t.as_str()),
            Value::Null | Value::Blob(_) => serde_json::Value::Null,
        })
        .collect();
    encode_cursor(&json)
}

/// Reads a cursor from [`encode_keyset`], checking it has `len` values.
pub(crate) fn decode_keyset(
    cursor: Option<&str>,
    len: usize,
) -> Result<Vec<rusqlite::types::Value>, DbError> {
    use rusqlite::types::Value;
    let Some(cursor) = cursor.filter(|c| !c.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    let json: Vec<serde_json::Value> = decode_cursor(cursor)?;
    if json.len() != len {
        return Err(DbError::Invalid(String::from("cursor")));
    }
    json.into_iter()
        .map(|v| match v {
            serde_json::Value::Number(n) => n
                .as_i64()
                .map(Value::Integer)
                .or_else(|| n.as_f64().map(Value::Real))
                .ok_or_else(|| DbError::Invalid(String::from("cursor"))),
            serde_json::Value::String(s) => Ok(Value::Text(s)),
            _ => Err(DbError::Invalid(String::from("cursor"))),
        })
        .collect()
}

/// Reads the key values a keyset query appended after column `first`.
pub(crate) fn read_keys(
    row: &rusqlite::Row<'_>,
    first: usize,
    count: usize,
) -> rusqlite::Result<Vec<rusqlite::types::Value>> {
    (first..first + count).map(|i| row.get(i)).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::access::Role;
    use crate::ids::UserKey;

    #[test]
    fn visible_lists_granted_libraries() {
        let member = Access {
            user: UserKey(2),
            role: Role::Member,
            libraries: LibrarySet::Only(Arc::from([LibraryKey(1), LibraryKey(3)])),
            sync_epoch: 1,
        };
        assert_eq!(
            visible(&member, "s.library_id", None),
            "+s.library_id IN (1,3)"
        );
        assert_eq!(visible(&member, "x", Some(LibraryKey(2))), "0");
        assert_eq!(visible(&Access::system(), "x", None), "1");
        assert_eq!(
            visible(&Access::system(), "x", Some(LibraryKey(7))),
            "+x IN (7)"
        );
    }

    #[test]
    fn json_lists_escape_strings() {
        assert_eq!(json_strings(["a", "b\"c"]), r#"["a","b\"c"]"#);
        assert_eq!(json_ints([1, 2]), "[1,2]");
    }

    #[test]
    fn cursors_round_trip_and_reject_garbage() -> Result<(), DbError> {
        let cursor = encode_cursor(&("x", 3_i64));
        assert_eq!(
            decode_cursor::<(String, i64)>(&cursor)?,
            (String::from("x"), 3)
        );
        assert!(decode_cursor::<(String, i64)>("!!").is_err());
        Ok(())
    }

    #[test]
    fn relative_path_parts() {
        assert_eq!(rel_parent(b"a/b/c.flac"), b"a/b");
        assert_eq!(rel_parent(b"c.flac"), b"");
        assert_eq!(rel_name(b"a/b/c.flac"), b"c.flac");
    }
}
