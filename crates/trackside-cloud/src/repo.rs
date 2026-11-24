//! The SQL behind Trackside Cloud (migration 4). Every statement names the user, so no
//! query can reach another user's rows.

use rusqlite::{Connection, OptionalExtension, Row, params};
use trackside_core::{DbError, DeviceId, DeviceKey, Millis, UserKey};

use crate::devices::{CloudDevice, Registry};
use crate::entity::Entity;
use crate::merge::{Meta, Next, Stored};
use crate::model::{Fields, Usage, UsageEntry};

fn json_error(e: &serde_json::Error) -> DbError {
    DbError::Invalid(format!("stored cloud JSON: {e}"))
}

/// The user's change seq: the version of their latest change, 0 before any.
pub(crate) fn seq(c: &Connection, user: UserKey) -> Result<i64, DbError> {
    Ok(c.prepare_cached("SELECT seq FROM cloud_state WHERE user_id = ?1")?
        .query_row([user], |row| row.get(0))
        .optional()?
        .unwrap_or(0))
}

pub(crate) fn set_seq(c: &Connection, user: UserKey, seq: i64) -> Result<(), DbError> {
    c.prepare_cached(
        "INSERT INTO cloud_state (user_id, seq) VALUES (?1, ?2)
         ON CONFLICT (user_id) DO UPDATE SET seq = excluded.seq",
    )?
    .execute(params![user, seq])?;
    Ok(())
}

pub(crate) fn pruned_at(c: &Connection, user: UserKey) -> Result<Millis, DbError> {
    Ok(c.prepare_cached("SELECT pruned_at FROM cloud_state WHERE user_id = ?1")?
        .query_row([user], |row| row.get(0))
        .optional()?
        .unwrap_or(Millis(0)))
}

const COLUMNS: &str = "item_id, version, fields, meta, parent, device_id, modified_at, deleted_at,
    merged_into, bytes";

fn map_row(row: &Row<'_>) -> rusqlite::Result<(Stored, Option<String>, Option<String>)> {
    Ok((
        Stored {
            id: row.get(0)?,
            version: row.get(1)?,
            fields: None,
            meta: Meta::default(),
            parent: row.get(4)?,
            device_id: row.get(5)?,
            modified_at: row.get(6)?,
            deleted_at: row.get(7)?,
            merged_into: row.get(8)?,
            bytes: row.get(9)?,
        },
        row.get(2)?,
        row.get(3)?,
    ))
}

fn finish(raw: (Stored, Option<String>, Option<String>)) -> Result<Stored, DbError> {
    let (mut stored, fields, meta) = raw;
    stored.fields = fields
        .map(|f| serde_json::from_str::<Fields>(&f))
        .transpose()
        .map_err(|e| json_error(&e))?;
    stored.meta = meta
        .map(|m| serde_json::from_str::<Meta>(&m))
        .transpose()
        .map_err(|e| json_error(&e))?
        .unwrap_or_default();
    Ok(stored)
}

pub(crate) fn load(
    c: &Connection,
    user: UserKey,
    entity: Entity,
    id: &str,
) -> Result<Option<Stored>, DbError> {
    c.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM cloud_items WHERE user_id = ?1 AND entity = ?2 AND item_id = ?3"
    ))?
    .query_row(params![user, entity.as_str(), id], map_row)
    .optional()?
    .map(finish)
    .transpose()
}

/// The bytes a row counts against the quota: its text plus a fixed overhead.
pub(crate) fn row_bytes(id: &str, fields: Option<&str>, meta: Option<&str>) -> i64 {
    (64 + id.len() + fields.map_or(0, str::len) + meta.map_or(0, str::len)) as i64
}

/// The JSON a row stores.
pub(crate) struct Encoded {
    pub fields: Option<String>,
    pub meta: Option<String>,
    pub bytes: i64,
}

pub(crate) fn encode(id: &str, next: &Next) -> Result<Encoded, DbError> {
    let fields = next
        .fields
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| json_error(&e))?;
    let meta = (!next.meta.is_empty())
        .then(|| serde_json::to_string(&next.meta))
        .transpose()
        .map_err(|e| json_error(&e))?;
    let bytes = row_bytes(id, fields.as_deref(), meta.as_deref());
    Ok(Encoded {
        fields,
        meta,
        bytes,
    })
}

/// Inserts or replaces a record at `version`.
#[expect(clippy::too_many_arguments, reason = "one row's columns")]
pub(crate) fn put(
    c: &Connection,
    user: UserKey,
    entity: Entity,
    id: &str,
    version: i64,
    device_id: &str,
    next: &Next,
    encoded: &Encoded,
    now: Millis,
) -> Result<(), DbError> {
    c.prepare_cached(
        "INSERT INTO cloud_items (user_id, entity, item_id, version, fields, meta, parent,
             device_id, modified_at, deleted_at, merged_into, changed_at, bytes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT (user_id, entity, item_id) DO UPDATE SET
             version = excluded.version, fields = excluded.fields, meta = excluded.meta,
             parent = excluded.parent, device_id = excluded.device_id,
             modified_at = excluded.modified_at, deleted_at = excluded.deleted_at,
             merged_into = excluded.merged_into, changed_at = excluded.changed_at,
             bytes = excluded.bytes",
    )?
    .execute(params![
        user,
        entity.as_str(),
        id,
        version,
        encoded.fields,
        encoded.meta,
        next.parent,
        device_id,
        next.modified_at,
        next.deleted_at,
        next.merged_into,
        now,
        encoded.bytes,
    ])?;
    Ok(())
}

/// Keeps newer per-field times without a new version (nothing a client sees changed).
pub(crate) fn touch(
    c: &Connection,
    user: UserKey,
    entity: Entity,
    id: &str,
    meta: &Meta,
    modified_at: Millis,
) -> Result<(), DbError> {
    let meta = (!meta.is_empty())
        .then(|| serde_json::to_string(meta))
        .transpose()
        .map_err(|e| json_error(&e))?;
    c.prepare_cached(
        "UPDATE cloud_items SET meta = ?4, modified_at = ?5
         WHERE user_id = ?1 AND entity = ?2 AND item_id = ?3",
    )?
    .execute(params![user, entity.as_str(), id, meta, modified_at])?;
    Ok(())
}

/// What the user stores in total.
pub(crate) fn totals(c: &Connection, user: UserKey) -> Result<(i64, i64), DbError> {
    Ok(c.prepare_cached(
        "SELECT coalesce(sum(items), 0), coalesce(sum(bytes), 0) FROM cloud_entities
         WHERE user_id = ?1",
    )?
    .query_row([user], |row| Ok((row.get(0)?, row.get(1)?)))?)
}

pub(crate) fn add_usage(
    c: &Connection,
    user: UserKey,
    entity: Entity,
    items: i64,
    bytes: i64,
) -> Result<(), DbError> {
    if items == 0 && bytes == 0 {
        return Ok(());
    }
    c.prepare_cached(
        "INSERT INTO cloud_entities (user_id, entity, items, bytes) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (user_id, entity) DO UPDATE SET
             items = max(0, items + excluded.items), bytes = max(0, bytes + excluded.bytes)",
    )?
    .execute(params![user, entity.as_str(), items, bytes])?;
    Ok(())
}

/// The highest version whose tombstone was pruned: cursors below it cannot be served.
pub(crate) fn pruned_through(c: &Connection, user: UserKey, entity: Entity) -> Result<i64, DbError> {
    Ok(c.prepare_cached(
        "SELECT pruned_through FROM cloud_entities WHERE user_id = ?1 AND entity = ?2",
    )?
    .query_row(params![user, entity.as_str()], |row| row.get(0))
    .optional()?
    .unwrap_or(0))
}

/// Records after `after`, in version order, at most `limit`.
pub(crate) fn after(
    c: &Connection,
    user: UserKey,
    entity: Entity,
    after: i64,
    limit: u32,
) -> Result<Vec<Stored>, DbError> {
    let mut statement = c.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM cloud_items
         WHERE user_id = ?1 AND entity = ?2 AND version > ?3
         ORDER BY version LIMIT ?4"
    ))?;
    let rows = statement.query_map(params![user, entity.as_str(), after, limit], map_row)?;
    rows.map(|row| finish(row?)).collect()
}

/// The live records whose parent is `parent` (a playlist's entries).
pub(crate) fn live_children(
    c: &Connection,
    user: UserKey,
    entity: Entity,
    parent: &str,
) -> Result<Vec<Stored>, DbError> {
    let mut statement = c.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM cloud_items
         WHERE user_id = ?1 AND entity = ?2 AND parent = ?3 AND deleted_at IS NULL"
    ))?;
    let rows = statement.query_map(params![user, entity.as_str(), parent], map_row)?;
    rows.map(|row| finish(row?)).collect()
}

/// Deletes up to `limit` of `entity`'s tombstones written before `before`, keeping the
/// usage numbers and the pruned-through mark right. Returns how many went.
pub(crate) fn prune(
    c: &Connection,
    user: UserKey,
    entity: Entity,
    before: Millis,
    limit: u32,
) -> Result<usize, DbError> {
    let (count, bytes, highest): (i64, i64, Option<i64>) = c
        .prepare_cached(
            "SELECT count(*), coalesce(sum(bytes), 0), max(version) FROM (
                 SELECT bytes, version FROM cloud_items
                 WHERE user_id = ?1 AND entity = ?2 AND deleted_at IS NOT NULL AND changed_at < ?3
                 ORDER BY changed_at LIMIT ?4)",
        )?
        .query_row(params![user, entity.as_str(), before, limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    let Some(highest) = highest else {
        return Ok(0);
    };
    c.prepare_cached(
        "DELETE FROM cloud_items WHERE id IN (
             SELECT id FROM cloud_items
             WHERE user_id = ?1 AND entity = ?2 AND deleted_at IS NOT NULL AND changed_at < ?3
             ORDER BY changed_at LIMIT ?4)",
    )?
    .execute(params![user, entity.as_str(), before, limit])?;
    add_usage(c, user, entity, -count, -bytes)?;
    // A tombstone pruned at version v means a cursor below v could miss that delete.
    c.prepare_cached(
        "UPDATE cloud_entities SET pruned_through = max(pruned_through, ?3)
         WHERE user_id = ?1 AND entity = ?2",
    )?
    .execute(params![user, entity.as_str(), highest])?;
    Ok(count as usize)
}

pub(crate) fn set_pruned_at(c: &Connection, user: UserKey, at: Millis) -> Result<(), DbError> {
    c.prepare_cached(
        "INSERT INTO cloud_state (user_id, pruned_at) VALUES (?1, ?2)
         ON CONFLICT (user_id) DO UPDATE SET pruned_at = excluded.pruned_at",
    )?
    .execute(params![user, at])?;
    Ok(())
}

/// The user's usage, per entity.
pub(crate) fn usage(c: &Connection, user: UserKey) -> Result<Usage, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT entity, items, bytes FROM cloud_entities WHERE user_id = ?1 ORDER BY entity",
    )?;
    let rows = statement.query_map([user], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut usage = Usage::default();
    for row in rows {
        let (name, items, bytes) = row?;
        let Some(entity) = Entity::parse(&name) else {
            continue;
        };
        let (items, bytes) = (items.max(0) as u64, bytes.max(0) as u64);
        usage.items += items;
        usage.bytes += bytes;
        if items > 0 {
            usage.entities.push(UsageEntry {
                entity,
                items,
                bytes,
            });
        }
    }
    usage.entities.sort_by_key(|e| e.entity);
    Ok(usage)
}

/// Live settings count, for the per-user cap.
pub(crate) fn live_count(c: &Connection, user: UserKey, entity: Entity) -> Result<u64, DbError> {
    let n: i64 = c
        .prepare_cached(
            "SELECT count(*) FROM cloud_items WHERE user_id = ?1 AND entity = ?2",
        )?
        .query_row(params![user, entity.as_str()], |row| row.get(0))?;
    Ok(n.max(0) as u64)
}

fn device_row(row: &Row<'_>) -> rusqlite::Result<(CloudDevice, String)> {
    Ok((
        CloudDevice {
            device_id: row.get(0)?,
            predecessors: Vec::new(),
            name: row.get(2)?,
            platform: row.get(3)?,
            server_device: row.get::<_, Option<DeviceId>>(4)?,
            registered_at: row.get(5)?,
            updated_at: row.get(6)?,
        },
        row.get(1)?,
    ))
}

pub(crate) fn devices(c: &Connection, user: UserKey) -> Result<Vec<CloudDevice>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT d.device_id, d.predecessors, d.name, d.platform, s.pid, d.registered_at,
             d.updated_at
         FROM cloud_devices d LEFT JOIN devices s ON s.id = d.server_device
         WHERE d.user_id = ?1 ORDER BY d.registered_at, d.device_id",
    )?;
    let rows = statement.query_map([user], device_row)?;
    rows.map(|row| {
        let (mut device, predecessors) = row?;
        device.predecessors = serde_json::from_str(&predecessors).map_err(|e| json_error(&e))?;
        Ok(device)
    })
    .collect()
}

pub(crate) fn registry(c: &Connection, user: UserKey) -> Result<Registry, DbError> {
    Ok(Registry::new(
        devices(c, user)?
            .into_iter()
            .map(|d| (d.device_id, d.predecessors)),
    ))
}

#[expect(clippy::too_many_arguments, reason = "one row's columns")]
pub(crate) fn put_device(
    c: &Connection,
    user: UserKey,
    device_id: &str,
    predecessors: &[String],
    name: Option<&str>,
    platform: Option<&str>,
    server_device: Option<DeviceKey>,
    now: Millis,
) -> Result<(), DbError> {
    let predecessors = serde_json::to_string(predecessors).map_err(|e| json_error(&e))?;
    c.prepare_cached(
        "INSERT INTO cloud_devices (user_id, device_id, predecessors, server_device, name,
             platform, registered_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
         ON CONFLICT (user_id, device_id) DO UPDATE SET
             predecessors = excluded.predecessors,
             server_device = coalesce(excluded.server_device, server_device),
             name = coalesce(excluded.name, name),
             platform = coalesce(excluded.platform, platform),
             updated_at = excluded.updated_at",
    )?
    .execute(params![
        user,
        device_id,
        predecessors,
        server_device,
        name,
        platform,
        now
    ])?;
    Ok(())
}
