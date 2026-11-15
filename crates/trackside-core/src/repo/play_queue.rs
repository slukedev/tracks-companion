//! The saved play queue, one per user. Last writer wins.

use rusqlite::{Connection, OptionalExtension, params};

use super::sql::{from_u64, to_u32, to_u64};
use crate::db::{Change, ChangeKind, ChangeOp, DbError, WriteTx};
use crate::ids::{DeviceId, DeviceKey, PublicId, SongId, UserKey};
use crate::time::Millis;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayQueue {
    pub entries: Vec<SongId>,
    pub current: Option<u32>,
    pub position_ms: u64,
    /// The client name that saved it.
    pub changed_by: String,
    pub device: Option<DeviceId>,
    pub updated_at: Millis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayQueueUpdate {
    pub entries: Vec<SongId>,
    pub current: Option<u32>,
    pub position_ms: u64,
    pub changed_by: String,
    pub device: Option<DeviceKey>,
}

/// The most entries a saved queue may hold.
pub const MAX_ENTRIES: usize = 2000;

/// The `change_log` entity of a user's queue; there is one per user.
pub(crate) const ENTITY: &str = "queue";

pub fn get(c: &Connection, user: UserKey) -> Result<Option<PlayQueue>, DbError> {
    type Head = (Option<i64>, i64, String, Option<DeviceId>, Millis);
    let head: Option<Head> = c
        .prepare_cached(
            "SELECT q.current_position, q.position_ms, q.changed_by, d.pid, q.updated_at
             FROM play_queues q LEFT JOIN devices d ON d.id = q.device_id WHERE q.user_id = ?1",
        )?
        .query_row([user], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .optional()?;
    let Some((current, position_ms, changed_by, device, updated_at)) = head else {
        return Ok(None);
    };
    let mut statement = c.prepare_cached(
        "SELECT song_pid FROM play_queue_entries WHERE user_id = ?1 ORDER BY position",
    )?;
    let entries = statement
        .query_map([user], |row| row.get::<_, String>(0))?
        .filter_map(|pid| pid.ok().and_then(|pid| SongId::parse(&pid).ok()))
        .collect();
    Ok(Some(PlayQueue {
        entries,
        current: current.map(to_u32),
        position_ms: to_u64(position_ms),
        changed_by,
        device,
        updated_at,
    }))
}

/// Replaces the queue; an empty `entries` clears it.
pub fn put(
    tx: &mut WriteTx<'_>,
    user: UserKey,
    queue: &PlayQueueUpdate,
) -> Result<PlayQueue, DbError> {
    if queue.entries.len() > MAX_ENTRIES {
        return Err(DbError::Invalid(String::from("entries")));
    }
    tx.prepare_cached("DELETE FROM play_queues WHERE user_id = ?1")?
        .execute([user])?;
    let now = tx.now();
    if queue.entries.is_empty() {
        tx.record(Change::for_user(
            ChangeKind::PlayQueue,
            ENTITY,
            user,
            ChangeOp::Delete,
        ))?;
        return Ok(PlayQueue {
            entries: Vec::new(),
            current: None,
            position_ms: 0,
            changed_by: queue.changed_by.clone(),
            device: None,
            updated_at: now,
        });
    }
    let current = queue
        .current
        .filter(|&c| (c as usize) < queue.entries.len());
    tx.prepare_cached(
        "INSERT INTO play_queues (user_id, current_position, position_ms, changed_by, device_id,
             updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        user,
        current,
        from_u64(queue.position_ms),
        queue.changed_by,
        queue.device,
        now
    ])?;
    for (position, song) in queue.entries.iter().enumerate() {
        tx.prepare_cached(
            "INSERT INTO play_queue_entries (user_id, position, song_pid) VALUES (?1, ?2, ?3)",
        )?
        .execute(params![user, position as i64, song])?;
    }
    tx.record(Change::for_user(
        ChangeKind::PlayQueue,
        ENTITY,
        user,
        ChangeOp::Upsert,
    ))?;
    get(tx, user)?.ok_or(DbError::NotFound)
}
