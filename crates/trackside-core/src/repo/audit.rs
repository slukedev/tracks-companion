//! The audit log of admin and security-relevant actions.

use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};

use super::sql::{
    OrderKey, after_condition, clamp_limit, decode_keyset, encode_keyset, order_by, read_keys,
};
use crate::db::{DbError, WriteTx};
use crate::ids::UserKey;
use crate::records::Page;
use crate::time::Millis;

/// An action to log.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditEntry {
    pub actor: Option<UserKey>,
    /// Username at the time, or `cli` or `system`.
    pub actor_name: String,
    pub ip: Option<String>,
    /// Such as `user.create`, `key.revoke`, `acquire.request`.
    pub action: String,
    pub target: Option<String>,
    pub detail: Option<serde_json::Value>,
}

/// A logged action.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditRow {
    pub at: Millis,
    pub entry: AuditEntry,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditFilter {
    pub action: Option<String>,
    pub actor: Option<String>,
    pub cursor: Option<String>,
    pub limit: u32,
}

pub fn log(tx: &mut WriteTx<'_>, entry: &AuditEntry) -> Result<(), DbError> {
    tx.prepare_cached(
        "INSERT INTO audit_log (at, actor_id, actor, ip, action, target, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![
        tx.now(),
        entry.actor,
        entry.actor_name,
        entry.ip,
        entry.action,
        entry.target,
        entry.detail.as_ref().map(serde_json::Value::to_string)
    ])?;
    Ok(())
}

/// Newest first. `action` matches exactly or as a prefix ending in a dot (`user.` finds
/// `user.create`); `actor` matches the stored name.
pub fn list(c: &Connection, filter: &AuditFilter) -> Result<Page<AuditRow>, DbError> {
    let limit = clamp_limit(filter.limit, 100, 500);
    let keys = [OrderKey::desc("at"), OrderKey::desc("id")];
    let after = decode_keyset(filter.cursor.as_deref(), keys.len())?;
    let (after_sql, after_params) = after_condition(&keys, &after);
    let action = filter.action.clone().filter(|a| !a.is_empty());
    let mut params = vec![
        action.clone().map_or(Value::Null, Value::Text),
        action
            .filter(|a| a.ends_with('.'))
            .map_or(Value::Null, |a| Value::Text(format!("{a}%"))),
        filter
            .actor
            .clone()
            .filter(|a| !a.is_empty())
            .map_or(Value::Null, Value::Text),
    ];
    params.extend(after_params);
    let sql = format!(
        "SELECT at, actor_id, actor, ip, action, target, detail, at, id FROM audit_log
         WHERE (?1 IS NULL OR action = ?1 OR (?2 IS NOT NULL AND action LIKE ?2))
           AND (?3 IS NULL OR actor = ?3) AND {after_sql} {} LIMIT {}",
        order_by(&keys),
        limit + 1
    );
    let mut statement = c.prepare_cached(&sql)?;
    let mut rows: Vec<(AuditRow, Vec<Value>)> = statement
        .query_map(params_from_iter(params), |row| {
            let detail: Option<String> = row.get(6)?;
            Ok((
                AuditRow {
                    at: row.get(0)?,
                    entry: AuditEntry {
                        actor: row.get(1)?,
                        actor_name: row.get(2)?,
                        ip: row.get(3)?,
                        action: row.get(4)?,
                        target: row.get(5)?,
                        detail: detail.and_then(|d| serde_json::from_str(&d).ok()),
                    },
                },
                read_keys(row, 7, 2)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = if more {
        rows.last().map(|(_, keys)| encode_keyset(keys))
    } else {
        None
    };
    Ok(Page {
        items: rows.into_iter().map(|(row, _)| row).collect(),
        next,
    })
}

/// Deletes entries older than `older_than`.
pub fn prune(tx: &mut WriteTx<'_>, older_than: Millis) -> Result<u64, DbError> {
    let n = tx
        .prepare_cached("DELETE FROM audit_log WHERE at < ?1")?
        .execute([older_than])?;
    Ok(n as u64)
}
