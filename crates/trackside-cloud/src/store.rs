//! Pushes, pulls, settings and the device registry over [`Db`]. A push runs its merge rules
//! on the single writer in chunks of at most [`CHUNK_ITEMS`] items or [`CHUNK_TIME`], each
//! its own transaction, so an import of 100k plays never holds the writer for long and other
//! users' writes interleave between chunks (ADR 0002).

use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::Value;
use trackside_core::{Db, DbError, DeviceKey, ServerEvent, UserKey, WriteTx};

use crate::devices::{CloudDevice, Registry};
use crate::entity::{Entity, Rule};
use crate::epoch::{self, Cursor};
use crate::limits::{
    CHUNK_ITEMS, CHUNK_TIME, Limits, MAX_BATCH, MAX_DEVICE_ID_BYTES, MAX_DEVICES, MAX_PREDECESSORS,
    MAX_SETTINGS, PURGE_CHUNK, TOMBSTONE_DAYS,
};
use crate::merge::{self, Decision, Next, Stored};
use crate::model::{Body, CloudError, Page, PushItem, PushOutcome, PushResult, Reject, Usage};
use crate::repo;
use crate::settings::{
    self, Setting, SettingOutcome, SettingResult, SettingWrite, SettingsPage, Verdict,
};
use crate::validate::{check_item, is_label};

/// The `cloud` event for `entity`: tells the user's clients to pull it. Never carries data.
fn signal(db: &Db, user: UserKey, entity: &'static str) {
    db.events()
        .publish(ServerEvent::CloudChanged { user, entity });
}

/// What one chunk did.
struct Chunk {
    results: Vec<PushResult>,
    wrote: bool,
    /// Deleted playlists whose entries are still to be deleted.
    deleted_playlists: Vec<String>,
}

/// The user's running totals inside one chunk.
struct Budget {
    limits: Limits,
    items: i64,
    bytes: i64,
}

impl Budget {
    fn allows(&self, new_items: i64, more_bytes: i64) -> bool {
        let items_ok = new_items <= 0 || self.items + new_items <= self.limits.max_items as i64;
        let bytes_ok = more_bytes <= 0 || self.bytes + more_bytes <= self.limits.max_bytes as i64;
        items_ok && bytes_ok
    }
}

/// Applies a batch for `user`. Every item gets a result, in order; the whole call fails only
/// when the database does. `device` is the device id bound to the sending device key: an
/// item naming another is `device_mismatch`. `None` checks nothing (server-side callers).
pub async fn push(
    db: &Db,
    user: UserKey,
    entity: Entity,
    items: Vec<PushItem>,
    device: Option<String>,
    limits: Limits,
) -> Result<Vec<PushResult>, CloudError> {
    if entity == Entity::Settings {
        return Err(CloudError::Invalid(String::from("settings use PATCH")));
    }
    if items.len() > MAX_BATCH {
        return Err(CloudError::TooMany);
    }
    prune(db, user, false).await?;
    let items = Arc::new(items);
    let mut results = Vec::with_capacity(items.len());
    let mut wrote = false;
    let mut deleted_playlists = Vec::new();
    while results.len() < items.len() {
        let start = results.len();
        let batch = Arc::clone(&items);
        let device = device.clone();
        let chunk = db
            .write(move |tx| apply_chunk(tx, user, entity, &batch[start..], device, limits))
            .await?;
        wrote |= chunk.wrote;
        deleted_playlists.extend(chunk.deleted_playlists);
        results.extend(chunk.results);
    }
    let mut cascaded = false;
    for playlist in deleted_playlists {
        cascaded |= cascade(db, user, playlist).await?;
    }
    if wrote || cascaded {
        epoch::note_issued(db).await;
    }
    if wrote {
        signal(db, user, entity.as_str());
    }
    if cascaded {
        signal(db, user, Entity::PlaylistEntries.as_str());
    }
    Ok(results)
}

fn apply_chunk(
    tx: &mut WriteTx<'_>,
    user: UserKey,
    entity: Entity,
    items: &[PushItem],
    device: Option<String>,
    limits: Limits,
) -> Result<Chunk, DbError> {
    let started = Instant::now();
    let registry = if entity.rule() == Rule::Owned {
        repo::registry(tx, user)?
    } else {
        Registry::default()
    };
    let before = repo::seq(tx, user)?;
    let (items_now, bytes_now) = repo::totals(tx, user)?;
    let mut state = State {
        user,
        entity,
        device,
        seq: before,
        budget: Budget {
            limits,
            items: items_now,
            bytes: bytes_now,
        },
        added_items: 0,
        added_bytes: 0,
        deleted_playlists: Vec::new(),
    };
    let mut results = Vec::new();
    for item in items {
        if !results.is_empty() && (results.len() >= CHUNK_ITEMS || started.elapsed() >= CHUNK_TIME)
        {
            break;
        }
        results.push(apply_one(tx, &mut state, &registry, item)?);
    }
    if state.seq != before {
        repo::set_seq(tx, user, state.seq)?;
    }
    repo::add_usage(tx, user, entity, state.added_items, state.added_bytes)?;
    Ok(Chunk {
        results,
        wrote: state.seq != before,
        deleted_playlists: state.deleted_playlists,
    })
}

struct State {
    user: UserKey,
    entity: Entity,
    device: Option<String>,
    seq: i64,
    budget: Budget,
    added_items: i64,
    added_bytes: i64,
    deleted_playlists: Vec<String>,
}

fn apply_one(
    tx: &WriteTx<'_>,
    state: &mut State,
    registry: &Registry,
    item: &PushItem,
) -> Result<PushResult, DbError> {
    let now = tx.now();
    if state.device.as_ref().is_some_and(|d| *d != item.device_id) {
        return Ok(PushResult {
            id: item.id.clone(),
            outcome: PushOutcome::Rejected {
                reason: Reject::DeviceMismatch,
            },
        });
    }
    let id = match check_item(state.entity, item, now) {
        Ok(id) => id,
        Err(reason) => {
            return Ok(PushResult {
                id: item.id.clone(),
                outcome: PushOutcome::Rejected { reason },
            });
        }
    };
    let stored = repo::load(tx, state.user, state.entity, &id)?;
    let outcome = match merge::decide(state.entity, stored.as_ref(), item, registry, now) {
        Decision::Write(next) => {
            let encoded = repo::encode(&id, &next)?;
            let new_items = i64::from(stored.is_none());
            let more_bytes = encoded.bytes - stored.as_ref().map_or(0, |s| s.bytes);
            if state.budget.allows(new_items, more_bytes) {
                state.seq += 1;
                repo::put(
                    tx,
                    state.user,
                    state.entity,
                    &id,
                    state.seq,
                    &item.device_id,
                    &next,
                    &encoded,
                    now,
                )?;
                state.budget.items += new_items;
                state.budget.bytes += more_bytes;
                state.added_items += new_items;
                state.added_bytes += more_bytes;
                let version = state.seq;
                if state.entity == Entity::Playlists && next.fields.is_none() {
                    state.deleted_playlists.push(id.clone());
                }
                PushOutcome::Accepted { version }
            } else {
                PushOutcome::Rejected {
                    reason: Reject::QuotaExceeded,
                }
            }
        }
        Decision::Duplicate { touch } => {
            if let Some((meta, at)) = touch {
                repo::touch(tx, state.user, state.entity, &id, &meta, at)?;
            }
            // A replayed delete finishes a cascade that a restart cut short.
            if state.entity == Entity::Playlists && stored.as_ref().is_some_and(|s| s.fields.is_none())
            {
                state.deleted_playlists.push(id.clone());
            }
            PushOutcome::Duplicate {
                version: stored.as_ref().map_or(0, |s| s.version),
            }
        }
        Decision::Conflict => PushOutcome::Conflict {
            current: stored.as_ref().map(Stored::record),
        },
        Decision::Reject(reason) => PushOutcome::Rejected { reason },
    };
    Ok(PushResult { id, outcome })
}

/// A deleted playlist takes its entries with it, as tombstones the clients pull. In
/// transactions of [`CHUNK_ITEMS`] entries, like a push, so a playlist of 50k songs never
/// holds the writer for long; each one stops if the playlist was brought back meanwhile.
/// True when any entry was deleted.
async fn cascade(db: &Db, user: UserKey, playlist: String) -> Result<bool, CloudError> {
    let mut any = false;
    loop {
        let playlist = playlist.clone();
        let deleted = db
            .write(move |tx| cascade_some(tx, user, &playlist, CHUNK_ITEMS))
            .await?;
        any |= deleted > 0;
        if deleted < CHUNK_ITEMS {
            return Ok(any);
        }
    }
}

fn cascade_some(
    tx: &mut WriteTx<'_>,
    user: UserKey,
    playlist: &str,
    limit: usize,
) -> Result<usize, DbError> {
    let Some(tombstone) = repo::load(tx, user, Entity::Playlists, playlist)?
        .filter(|p| p.fields.is_none())
    else {
        return Ok(0);
    };
    let entity = Entity::PlaylistEntries;
    let now = tx.now();
    let entries = repo::live_children(tx, user, entity, playlist, limit)?;
    let mut seq = repo::seq(tx, user)?;
    let mut bytes = 0;
    for entry in &entries {
        let next = Next {
            fields: None,
            meta: merge::Meta::default(),
            parent: entry.parent.clone(),
            modified_at: tombstone.modified_at,
            deleted_at: tombstone.deleted_at,
            merged_into: None,
        };
        let encoded = repo::encode(&entry.id, &next)?;
        seq += 1;
        repo::put(
            tx,
            user,
            entity,
            &entry.id,
            seq,
            &tombstone.device_id,
            &next,
            &encoded,
            now,
        )?;
        bytes += encoded.bytes - entry.bytes;
    }
    if !entries.is_empty() {
        repo::set_seq(tx, user, seq)?;
        repo::add_usage(tx, user, entity, 0, bytes)?;
    }
    Ok(entries.len())
}

/// Pages through `entity` after `cursor` (`None` for a full snapshot from the start).
pub async fn pull(
    db: &Db,
    user: UserKey,
    entity: Entity,
    cursor: Option<Cursor>,
    limit: u32,
) -> Result<Page, CloudError> {
    db.read(move |c| page(c, user, entity, cursor.as_ref(), limit))
        .await?
}

fn page(
    c: &Connection,
    user: UserKey,
    entity: Entity,
    cursor: Option<&Cursor>,
    limit: u32,
) -> Result<Result<Page, CloudError>, DbError> {
    let current = repo::seq(c, user)?;
    let epoch = epoch::current(c)?;
    let after = cursor.map_or(0, |c| c.seq);
    if let Some(cursor) = cursor
        && (cursor.epoch != epoch
            || after > current
            || after < repo::pruned_through(c, user, entity)?)
    {
        return Ok(Err(CloudError::SyncReset));
    }
    let limit = limit.max(1);
    let mut rows = repo::after(c, user, entity, after, limit + 1)?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let cursor = Cursor {
        epoch,
        seq: match rows.last() {
            Some(last) if has_more => last.version,
            _ => current.max(after),
        },
    };
    let (mut upserts, mut deletes) = (Vec::new(), Vec::new());
    for row in rows {
        let record = row.record();
        match record.body {
            Body::Live(_) => upserts.push(record),
            Body::Deleted { .. } => deletes.push(record),
        }
    }
    Ok(Ok(Page {
        upserts,
        deletes,
        cursor,
        has_more,
    }))
}

/// Deletes tombstones older than [`TOMBSTONE_DAYS`] (never plays'), at most once a day per
/// user unless `force`, a few thousand per entity per run so the transaction stays short.
pub async fn prune(db: &Db, user: UserKey, force: bool) -> Result<usize, CloudError> {
    const DAY: Duration = Duration::from_hours(24);
    let now = db.clock().now();
    let last = db.read(move |c| repo::pruned_at(c, user)).await?;
    if !force && last <= now && now.since(last) < DAY {
        return Ok(0);
    }
    let before = now.saturating_sub(DAY * TOMBSTONE_DAYS as u32);
    let mut pruned = 0;
    for entity in Entity::ALL.into_iter().filter(|e| !e.keeps_tombstones()) {
        pruned += db
            .write(move |tx| repo::prune(tx, user, entity, before, 5_000))
            .await?;
    }
    db.write(move |tx| repo::set_pruned_at(tx, user, now))
        .await?;
    Ok(pruned)
}

fn setting(stored: &Stored) -> Setting {
    let value = stored
        .fields
        .as_ref()
        .and_then(|f| f.get("value").cloned())
        .unwrap_or(Value::Null);
    Setting {
        key: stored.id.clone(),
        value,
        modified_at: stored.modified_at,
        device_id: stored.device_id.clone(),
        version: stored.version,
    }
}

/// Every setting changed after `cursor`, or all of them.
pub async fn settings(
    db: &Db,
    user: UserKey,
    cursor: Option<Cursor>,
) -> Result<SettingsPage, CloudError> {
    db.read(move |c| {
        let current = repo::seq(c, user)?;
        let epoch = epoch::current(c)?;
        let after = cursor.as_ref().map_or(0, |c| c.seq);
        if cursor.is_some_and(|c| c.epoch != epoch || c.seq > current) {
            return Ok(Err(CloudError::SyncReset));
        }
        let limit = u32::try_from(MAX_SETTINGS).unwrap_or(u32::MAX) * 2;
        let rows = repo::after(c, user, Entity::Settings, after, limit)?;
        Ok(Ok(SettingsPage {
            settings: rows.iter().map(setting).collect(),
            cursor: Cursor {
                epoch,
                seq: current.max(after),
            },
        }))
    })
    .await?
}

/// Applies settings changes with the decide rule, in one transaction. `device` as for
/// [`push`].
pub async fn patch_settings(
    db: &Db,
    user: UserKey,
    writes: Vec<SettingWrite>,
    device: Option<String>,
    limits: Limits,
) -> Result<Vec<SettingResult>, CloudError> {
    if writes.len() > MAX_BATCH {
        return Err(CloudError::TooMany);
    }
    let (results, wrote) = db
        .write(move |tx| apply_settings(tx, user, &writes, device.as_deref(), limits))
        .await?;
    if wrote {
        epoch::note_issued(db).await;
        signal(db, user, Entity::Settings.as_str());
    }
    Ok(results)
}

fn apply_settings(
    tx: &mut WriteTx<'_>,
    user: UserKey,
    writes: &[SettingWrite],
    device: Option<&str>,
    limits: Limits,
) -> Result<(Vec<SettingResult>, bool), DbError> {
    let entity = Entity::Settings;
    let now = tx.now();
    let before = repo::seq(tx, user)?;
    let mut seq = before;
    let (items, bytes) = repo::totals(tx, user)?;
    let mut budget = Budget {
        limits,
        items,
        bytes,
    };
    let mut keys = repo::live_count(tx, user, entity)?;
    let (mut added_items, mut added_bytes) = (0, 0);
    let mut results = Vec::with_capacity(writes.len());
    for write in writes {
        let result = |outcome, current: Option<&Stored>| SettingResult {
            key: write.key.clone(),
            outcome,
            current: current.map(setting),
        };
        if device.is_some_and(|d| d != write.device_id) {
            results.push(result(
                SettingOutcome::Rejected(Reject::DeviceMismatch),
                None,
            ));
            continue;
        }
        if let Err(reason) = settings::check(write, now) {
            results.push(result(SettingOutcome::Rejected(reason), None));
            continue;
        }
        let stored = repo::load(tx, user, entity, &write.key)?;
        let current = stored.as_ref().map(setting);
        match settings::decide(current.as_ref(), write) {
            Verdict::Store { value, modified_at } => {
                let mut fields = serde_json::Map::new();
                fields.insert(String::from("value"), value);
                let next = Next {
                    fields: Some(fields),
                    meta: merge::Meta::default(),
                    parent: None,
                    modified_at,
                    deleted_at: None,
                    merged_into: None,
                };
                let encoded = repo::encode(&write.key, &next)?;
                let new_items = i64::from(stored.is_none());
                let more_bytes = encoded.bytes - stored.as_ref().map_or(0, |s| s.bytes);
                if (stored.is_none() && keys >= MAX_SETTINGS)
                    || !budget.allows(new_items, more_bytes)
                {
                    results.push(result(
                        SettingOutcome::Rejected(Reject::QuotaExceeded),
                        stored.as_ref(),
                    ));
                    continue;
                }
                seq += 1;
                repo::put(
                    tx,
                    user,
                    entity,
                    &write.key,
                    seq,
                    &write.device_id,
                    &next,
                    &encoded,
                    now,
                )?;
                keys += u64::from(stored.is_none());
                budget.items += new_items;
                budget.bytes += more_bytes;
                added_items += new_items;
                added_bytes += more_bytes;
                let written = repo::load(tx, user, entity, &write.key)?;
                results.push(result(SettingOutcome::Accepted, written.as_ref()));
            }
            Verdict::Duplicate => results.push(result(SettingOutcome::Duplicate, stored.as_ref())),
            Verdict::Conflict => results.push(result(SettingOutcome::Conflict, stored.as_ref())),
            Verdict::Reject(reason) => {
                results.push(result(SettingOutcome::Rejected(reason), stored.as_ref()));
            }
        }
    }
    if seq != before {
        repo::set_seq(tx, user, seq)?;
    }
    repo::add_usage(tx, user, entity, added_items, added_bytes)?;
    Ok((results, seq != before))
}

/// The user's registered devices.
pub async fn devices(db: &Db, user: UserKey) -> Result<Vec<CloudDevice>, CloudError> {
    Ok(db.read(move |c| repo::devices(c, user)).await?)
}

/// A device registration: who the device is and which devices it was restored from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registration {
    pub device_id: String,
    pub predecessors: Vec<String>,
    pub name: Option<String>,
    pub platform: Option<String>,
}

/// Registers a device or adds to its predecessors (which only grow: a restore stays a fact).
/// `server_device` links it to the signed-in install that sent it, which binds that install
/// to this device id: registering another id from it, or this id from another live install,
/// is `DeviceMismatch`. Every predecessor must be one of the user's own devices, or a device
/// that recorded one of their plays and is no one else's (a restore whose old device never
/// registered).
pub async fn register_device(
    db: &Db,
    user: UserKey,
    registration: Registration,
    server_device: Option<DeviceKey>,
) -> Result<CloudDevice, CloudError> {
    let Registration {
        device_id,
        predecessors,
        name,
        platform,
    } = registration;
    let label = |t: &String| is_label(t, MAX_DEVICE_ID_BYTES);
    if !label(&device_id)
        || predecessors.len() > MAX_PREDECESSORS
        || !predecessors.iter().all(label)
        || predecessors.contains(&device_id)
        || name.as_ref().is_some_and(|n| !is_label(n, 200))
        || platform.as_ref().is_some_and(|p| !is_label(p, 32))
    {
        return Err(CloudError::Invalid(String::from("device")));
    }
    let id = device_id.clone();
    let device = db
        .write(move |tx| {
            if let Some(install) = server_device {
                let mismatch = match repo::registered_to(tx, user, install)? {
                    Some(bound) => bound != device_id,
                    None => repo::live_install_of(tx, user, &device_id)?
                        .is_some_and(|other| other != install),
                };
                if mismatch {
                    return Ok(Err(CloudError::DeviceMismatch));
                }
            }
            let known = repo::devices(tx, user)?;
            let existing = known.iter().find(|d| d.device_id == device_id);
            for p in &predecessors {
                let already = existing.is_some_and(|d| d.predecessors.contains(p));
                if !already
                    && !repo::is_users_device(tx, user, p)?
                    && !repo::recorded_own_play(tx, user, p, server_device)?
                {
                    return Ok(Err(CloudError::UnknownPredecessor(p.clone())));
                }
            }
            if existing.is_none() && known.len() >= MAX_DEVICES {
                return Ok(Err(CloudError::Invalid(String::from("too many devices"))));
            }
            let mut merged = existing.map(|d| d.predecessors.clone()).unwrap_or_default();
            for p in predecessors {
                if !merged.contains(&p) {
                    merged.push(p);
                }
            }
            if merged.len() > MAX_PREDECESSORS {
                return Ok(Err(CloudError::Invalid(String::from(
                    "too many predecessors",
                ))));
            }
            repo::put_device(
                tx,
                user,
                &device_id,
                &merged,
                name.as_deref(),
                platform.as_deref(),
                server_device,
                tx.now(),
            )?;
            Ok(repo::devices(tx, user)?
                .into_iter()
                .find(|d| d.device_id == id)
                .ok_or(CloudError::Invalid(String::from("device"))))
        })
        .await??;
    signal(db, user, "devices");
    Ok(device)
}

/// The device id bound to a signed-in install of `user`: the one it registered, else the
/// `client_device_id` it signed in with.
pub async fn bound_device(
    db: &Db,
    user: UserKey,
    server_device: DeviceKey,
) -> Result<Option<String>, CloudError> {
    Ok(db
        .read(move |c| repo::bound_device(c, user, server_device))
        .await?)
}

/// Deletes all of a user's cloud data in transactions of [`PURGE_CHUNK`] records, so deleting
/// a user with 300k plays never holds the writer for long. Run before deleting the user, whose
/// cascade then has little left to do.
pub async fn purge_user(db: &Db, user: UserKey) -> Result<usize, CloudError> {
    let mut total = 0;
    loop {
        let gone = db
            .write(move |tx| repo::delete_some(tx, user, PURGE_CHUNK))
            .await?;
        total += gone;
        if gone < PURGE_CHUNK as usize {
            break;
        }
    }
    db.write(move |tx| repo::delete_rest(tx, user)).await?;
    Ok(total)
}

/// What the user stores: totals and per entity. Numbers only, for quotas and admins.
pub fn usage_in(c: &Connection, user: UserKey) -> Result<Usage, DbError> {
    repo::usage(c, user)
}

pub async fn usage(db: &Db, user: UserKey) -> Result<Usage, CloudError> {
    Ok(db.read(move |c| repo::usage(c, user)).await?)
}
