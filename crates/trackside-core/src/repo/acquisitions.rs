//! The durable acquisition queue and its candidates.

use std::path::PathBuf;
use std::time::Duration;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::sql::{
    OrderKey, after_condition, bytes_path, clamp_limit, decode_keyset, encode_keyset, from_u64,
    opt_u8, opt_u32, order_by, path_bytes, read_keys, to_u32, to_u64,
};
use crate::control::{AcquireFailure, AcquisitionFilter, AcquisitionState, AcquisitionStateFilter};
use crate::db::{Change, ChangeKind, ChangeOp, DbError, WriteTx};
use crate::ids::{AcquisitionId, CandidateToken, PublicId, SongId, UserKey, random_id};
use crate::records::{Codec, Container, Page};
use crate::time::Millis;

/// An acquisition as stored. `provider` is internal and never sent to Tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcquisitionRow {
    pub key: i64,
    pub id: AcquisitionId,
    pub user: Option<UserKey>,
    /// The active request this one joined instead of running itself.
    pub joined_to: Option<AcquisitionId>,
    /// `tracks_model::song_key` of the request.
    pub request_key: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
    pub candidate_token: Option<CandidateToken>,
    pub allow_versions: bool,
    pub instant: bool,
    /// 0 instant play, 1 interactive, 2 batch.
    pub priority: u8,
    pub state: AcquisitionState,
    pub failure: Option<AcquireFailure>,
    pub failure_detail: Option<String>,
    pub provider: Option<String>,
    pub candidate: Option<CandidateToken>,
    pub attempts: u32,
    pub next_attempt_at: Millis,
    pub lease_until: Option<Millis>,
    pub progress_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub staging_dir: Option<PathBuf>,
    pub song: Option<SongId>,
    pub existing: bool,
    pub created_at: Millis,
    pub updated_at: Millis,
    pub finished_at: Option<Millis>,
}

/// A request to queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewAcquisition {
    pub user: Option<UserKey>,
    pub joined_to: Option<AcquisitionId>,
    pub request_key: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
    pub candidate_token: Option<CandidateToken>,
    pub allow_versions: bool,
    pub instant: bool,
    pub priority: u8,
    /// Usually `Queued`; `Done` for a request the library already satisfies.
    pub state: AcquisitionState,
    pub song: Option<SongId>,
    pub existing: bool,
}

/// A change to an acquisition. `None` leaves a field unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcquisitionUpdate {
    pub state: Option<AcquisitionState>,
    pub failure: Option<AcquireFailure>,
    pub failure_detail: Option<String>,
    pub provider: Option<String>,
    pub candidate: Option<CandidateToken>,
    pub add_attempt: bool,
    pub next_attempt_at: Option<Millis>,
    /// `Some(None)` releases the lease.
    pub lease_until: Option<Option<Millis>>,
    pub progress: Option<(u64, Option<u64>)>,
    /// `Some(None)` clears it after cleanup.
    pub staging_dir: Option<Option<PathBuf>>,
    pub song: Option<SongId>,
    pub existing: Option<bool>,
}

/// A ranked candidate. `acquisition: None` for candidates from a search not yet requested.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateRow {
    pub token: CandidateToken,
    pub acquisition: Option<AcquisitionId>,
    pub provider: String,
    pub provider_ref: serde_json::Value,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
    pub container: Option<Container>,
    pub codec: Option<Codec>,
    pub bitrate_kbps: Option<u32>,
    pub lossless: bool,
    pub size_bytes: Option<u64>,
    pub score: f32,
    pub tried_at: Option<Millis>,
    pub failure: Option<String>,
    pub created_at: Millis,
    pub expires_at: Millis,
}

const SELECT: &str =
    "SELECT q.id, q.pid, q.user_id, j.pid, q.request_key, q.title, q.artist, q.album,
        q.duration_ms, q.candidate_token, q.allow_versions, q.instant, q.priority, q.state,
        q.failure, q.failure_detail, q.provider, c.token, q.attempts, q.next_attempt_at,
        q.lease_until, q.progress_bytes, q.total_bytes, q.staging_dir, q.song_pid, q.existing,
        q.created_at, q.updated_at, q.finished_at
    FROM acquisitions q
    LEFT JOIN acquisitions j ON j.id = q.joined_to
    LEFT JOIN acquisition_candidates c ON c.id = q.candidate_id";

const COLUMNS: usize = 29;

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AcquisitionRow> {
    let state: String = row.get(13)?;
    let failure: Option<String> = row.get(14)?;
    let staging: Option<Vec<u8>> = row.get(23)?;
    let song: Option<String> = row.get(24)?;
    let token: Option<String> = row.get(9)?;
    Ok(AcquisitionRow {
        key: row.get(0)?,
        id: row.get(1)?,
        user: row.get(2)?,
        joined_to: row.get(3)?,
        request_key: row.get(4)?,
        title: row.get(5)?,
        artist: row.get(6)?,
        album: row.get(7)?,
        duration_ms: opt_u32(row.get(8)?),
        candidate_token: token.and_then(|t| CandidateToken::parse(&t).ok()),
        allow_versions: row.get(10)?,
        instant: row.get(11)?,
        priority: opt_u8(row.get(12)?).unwrap_or(1),
        state: AcquisitionState::parse(&state).unwrap_or(AcquisitionState::Failed),
        failure: failure.as_deref().and_then(AcquireFailure::parse),
        failure_detail: row.get(15)?,
        provider: row.get(16)?,
        candidate: row.get(17)?,
        attempts: to_u32(row.get(18)?),
        next_attempt_at: row.get(19)?,
        lease_until: row.get(20)?,
        progress_bytes: row.get::<_, Option<i64>>(21)?.map(to_u64),
        total_bytes: row.get::<_, Option<i64>>(22)?.map(to_u64),
        staging_dir: staging.as_deref().map(bytes_path),
        song: song.and_then(|s| SongId::parse(&s).ok()),
        existing: row.get(25)?,
        created_at: row.get(26)?,
        updated_at: row.get(27)?,
        finished_at: row.get(28)?,
    })
}

fn one(
    c: &Connection,
    tail: &str,
    param: &dyn rusqlite::ToSql,
) -> Result<Option<AcquisitionRow>, DbError> {
    Ok(c.prepare_cached(&format!("{SELECT} {tail}"))?
        .query_row([param], map_row)
        .optional()?)
}

fn record(tx: &mut WriteTx<'_>, row: &AcquisitionRow) -> Result<(), DbError> {
    if let Some(user) = row.user {
        tx.record(Change::for_user(
            ChangeKind::Acquisition,
            row.id.as_str(),
            user,
            ChangeOp::Upsert,
        ))?;
    }
    Ok(())
}

/// Inserts a request and records an `acquisition` change for its user.
pub fn insert(tx: &mut WriteTx<'_>, new: &NewAcquisition) -> Result<AcquisitionRow, DbError> {
    let id: AcquisitionId = random_id();
    let now = tx.now();
    let joined: Option<i64> = match &new.joined_to {
        Some(job) => Some(
            tx.prepare_cached("SELECT id FROM acquisitions WHERE pid = ?1")?
                .query_row([job], |row| row.get(0))
                .optional()?
                .ok_or(DbError::NotFound)?,
        ),
        None => None,
    };
    tx.prepare_cached(
        "INSERT INTO acquisitions (pid, user_id, joined_to, request_key, title, artist, album,
             duration_ms, candidate_token, allow_versions, instant, priority, state, song_pid,
             existing, next_attempt_at, created_at, updated_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16, ?16, ?17)",
    )?
    .execute(params![
        id,
        new.user,
        joined,
        new.request_key,
        new.title,
        new.artist,
        new.album,
        new.duration_ms,
        new.candidate_token,
        new.allow_versions,
        new.instant,
        new.priority,
        new.state.as_str(),
        new.song,
        new.existing,
        now,
        new.state.is_finished().then_some(now)
    ])?;
    let row = get(tx, &id)?.ok_or(DbError::NotFound)?;
    record(tx, &row)?;
    Ok(row)
}

pub fn get(c: &Connection, id: &AcquisitionId) -> Result<Option<AcquisitionRow>, DbError> {
    one(c, "WHERE q.pid = ?1", id)
}

/// The unfinished, unjoined request with this key, if any.
pub fn active_by_key(c: &Connection, request_key: &str) -> Result<Option<AcquisitionRow>, DbError> {
    one(
        c,
        "WHERE q.request_key = ?1 AND q.state NOT IN ('done', 'failed', 'cancelled')
           AND q.joined_to IS NULL ORDER BY q.id LIMIT 1",
        &request_key,
    )
}

/// Leases the next ready job (by priority, then `next_attempt_at`) whose lease is free or
/// expired.
pub fn lease_next(
    tx: &mut WriteTx<'_>,
    now: Millis,
    lease: Duration,
) -> Result<Option<AcquisitionRow>, DbError> {
    let next: Option<i64> = tx
        .prepare_cached(
            "SELECT id FROM acquisitions
             WHERE state NOT IN ('done', 'failed', 'cancelled') AND joined_to IS NULL
               AND next_attempt_at <= ?1 AND (lease_until IS NULL OR lease_until <= ?1)
             ORDER BY priority, next_attempt_at, id LIMIT 1",
        )?
        .query_row([now], |row| row.get(0))
        .optional()?;
    let Some(key) = next else { return Ok(None) };
    tx.prepare_cached("UPDATE acquisitions SET lease_until = ?2 WHERE id = ?1")?
        .execute(params![key, now.saturating_add(lease)])?;
    one(tx, "WHERE q.id = ?1", &key)
}

pub fn renew_lease(tx: &mut WriteTx<'_>, id: &AcquisitionId, until: Millis) -> Result<(), DbError> {
    let changed = tx
        .prepare_cached("UPDATE acquisitions SET lease_until = ?2 WHERE pid = ?1")?
        .execute(params![id, until])?;
    if changed == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Applies a change and records an `acquisition` change for its user (and joined requests).
pub fn transition(
    tx: &mut WriteTx<'_>,
    id: &AcquisitionId,
    to: AcquisitionUpdate,
) -> Result<AcquisitionRow, DbError> {
    let current = get(tx, id)?.ok_or(DbError::NotFound)?;
    let now = tx.now();
    let state = to.state.unwrap_or(current.state);
    let candidate: Option<i64> = match &to.candidate {
        Some(token) => tx
            .prepare_cached("SELECT id FROM acquisition_candidates WHERE token = ?1")?
            .query_row([token], |row| row.get(0))
            .optional()?,
        None => None,
    };
    let (progress, total) = match to.progress {
        Some((bytes, total)) => (Some(from_u64(bytes)), total.map(from_u64)),
        None => (
            current.progress_bytes.map(from_u64),
            current.total_bytes.map(from_u64),
        ),
    };
    let lease = match to.lease_until {
        Some(lease) => lease,
        None => current.lease_until,
    };
    let staging = match &to.staging_dir {
        Some(dir) => dir.as_deref().map(path_bytes),
        None => current.staging_dir.as_deref().map(path_bytes),
    };
    let finished_at = if state.is_finished() {
        current.finished_at.or(Some(now))
    } else {
        None
    };
    tx.prepare_cached(
        "UPDATE acquisitions SET state = ?2, failure = ?3, failure_detail = ?4, provider = ?5,
             candidate_id = coalesce(?6, candidate_id), attempts = attempts + ?7,
             next_attempt_at = ?8, lease_until = ?9, progress_bytes = ?10, total_bytes = ?11,
             staging_dir = ?12, song_pid = ?13, existing = ?14, updated_at = ?15, finished_at = ?16
         WHERE id = ?1",
    )?
    .execute(params![
        current.key,
        state.as_str(),
        to.failure.or(current.failure).map(AcquireFailure::as_str),
        to.failure_detail.or(current.failure_detail),
        to.provider.or(current.provider),
        candidate,
        i64::from(to.add_attempt),
        to.next_attempt_at.unwrap_or(current.next_attempt_at),
        lease,
        progress,
        total,
        staging,
        to.song.or(current.song),
        to.existing.unwrap_or(current.existing),
        now,
        finished_at
    ])?;
    let row = get(tx, id)?.ok_or(DbError::NotFound)?;
    record(tx, &row)?;
    let joined: Vec<(AcquisitionId, Option<UserKey>)> = tx
        .prepare_cached("SELECT pid, user_id FROM acquisitions WHERE joined_to = ?1")?
        .query_map([current.key], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (pid, user) in joined {
        if let Some(user) = user {
            tx.record(Change::for_user(
                ChangeKind::Acquisition,
                pid.as_str(),
                user,
                ChangeOp::Upsert,
            ))?;
        }
    }
    Ok(row)
}

/// Requests that joined this one.
pub fn joined(c: &Connection, id: &AcquisitionId) -> Result<Vec<AcquisitionRow>, DbError> {
    let mut statement = c.prepare_cached(&format!(
        "{SELECT} WHERE q.joined_to = (SELECT id FROM acquisitions WHERE pid = ?1) ORDER BY q.id"
    ))?;
    let rows = statement.query_map([id], map_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

const CANDIDATE_SELECT: &str =
    "SELECT c.token, q.pid, c.provider, c.provider_ref, c.title, c.artist,
        c.album, c.duration_ms, c.container, c.codec, c.bitrate_kbps, c.lossless, c.size_bytes,
        c.score, c.tried_at, c.failure, c.created_at, c.expires_at
    FROM acquisition_candidates c LEFT JOIN acquisitions q ON q.id = c.acquisition_id";

fn map_candidate(row: &rusqlite::Row<'_>) -> rusqlite::Result<CandidateRow> {
    let provider_ref: String = row.get(3)?;
    let container: Option<String> = row.get(8)?;
    let codec: Option<String> = row.get(9)?;
    let score: f64 = row.get(13)?;
    Ok(CandidateRow {
        token: row.get(0)?,
        acquisition: row.get(1)?,
        provider: row.get(2)?,
        provider_ref: serde_json::from_str(&provider_ref)
            .unwrap_or(serde_json::Value::String(provider_ref)),
        title: row.get(4)?,
        artist: row.get(5)?,
        album: row.get(6)?,
        duration_ms: opt_u32(row.get(7)?),
        container: container.as_deref().and_then(Container::parse),
        codec: codec.as_deref().and_then(Codec::parse),
        bitrate_kbps: opt_u32(row.get(10)?),
        lossless: row.get(11)?,
        size_bytes: row.get::<_, Option<i64>>(12)?.map(to_u64),
        score: score as f32,
        tried_at: row.get(14)?,
        failure: row.get(15)?,
        created_at: row.get(16)?,
        expires_at: row.get(17)?,
    })
}

/// A job's candidates, best first.
pub fn candidates(c: &Connection, id: &AcquisitionId) -> Result<Vec<CandidateRow>, DbError> {
    let mut statement = c.prepare_cached(&format!(
        "{CANDIDATE_SELECT} WHERE q.pid = ?1 ORDER BY c.score DESC, c.id"
    ))?;
    let rows = statement.query_map([id], map_candidate)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Upserts candidates by token, attaching them to `job` when given.
pub fn put_candidates(
    tx: &mut WriteTx<'_>,
    job: Option<&AcquisitionId>,
    rows: &[CandidateRow],
) -> Result<(), DbError> {
    let job_key: Option<i64> = match job {
        Some(job) => Some(
            tx.prepare_cached("SELECT id FROM acquisitions WHERE pid = ?1")?
                .query_row([job], |row| row.get(0))
                .optional()?
                .ok_or(DbError::NotFound)?,
        ),
        None => None,
    };
    for row in rows {
        tx.prepare_cached(
            "INSERT INTO acquisition_candidates (token, acquisition_id, provider, provider_ref,
                 title, artist, album, duration_ms, container, codec, bitrate_kbps, lossless,
                 size_bytes, score, tried_at, failure, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
             ON CONFLICT (token) DO UPDATE SET
                 acquisition_id = coalesce(excluded.acquisition_id, acquisition_id),
                 provider_ref = excluded.provider_ref, title = excluded.title,
                 artist = excluded.artist, album = excluded.album,
                 duration_ms = excluded.duration_ms, container = excluded.container,
                 codec = excluded.codec, bitrate_kbps = excluded.bitrate_kbps,
                 lossless = excluded.lossless, size_bytes = excluded.size_bytes,
                 score = excluded.score, tried_at = coalesce(excluded.tried_at, tried_at),
                 failure = coalesce(excluded.failure, failure), expires_at = excluded.expires_at",
        )?
        .execute(params![
            row.token,
            job_key,
            row.provider,
            row.provider_ref.to_string(),
            row.title,
            row.artist,
            row.album,
            row.duration_ms,
            row.container.map(Container::as_str),
            row.codec.map(Codec::as_str),
            row.bitrate_kbps,
            row.lossless,
            row.size_bytes.map(from_u64),
            f64::from(row.score),
            row.tried_at,
            row.failure,
            row.created_at,
            row.expires_at
        ])?;
    }
    Ok(())
}

/// The candidate with this token, expired or not. Expiry is checked by the caller against its
/// clock; [`candidate_by_token_at`] does it here.
pub fn candidate_by_token(
    c: &Connection,
    token: &CandidateToken,
) -> Result<Option<CandidateRow>, DbError> {
    Ok(
        c.prepare_cached(&format!("{CANDIDATE_SELECT} WHERE c.token = ?1"))?
            .query_row([token], map_candidate)
            .optional()?,
    )
}

/// The candidate with this token if it has not expired at `now`.
pub fn candidate_by_token_at(
    c: &Connection,
    token: &CandidateToken,
    now: Millis,
) -> Result<Option<CandidateRow>, DbError> {
    Ok(candidate_by_token(c, token)?.filter(|row| row.expires_at > now))
}

/// Requests that count against quotas: not joined to another, and not answered straight from
/// the library (those finish as `done` with `existing` set before any attempt).
const COUNTED: &str =
    "joined_to IS NULL AND NOT (existing = 1 AND attempts = 0 AND state = 'done')";

/// Requests a user made since `since` that count against their quota.
pub fn count_for_user_since(c: &Connection, user: UserKey, since: Millis) -> Result<u32, DbError> {
    Ok(quota_for_user_since(c, user, since)?.0)
}

/// Like [`count_for_user_since`], with the oldest counted request's time, for `resets_at`.
pub fn quota_for_user_since(
    c: &Connection,
    user: UserKey,
    since: Millis,
) -> Result<(u32, Option<Millis>), DbError> {
    Ok(c.prepare_cached(&format!(
        "SELECT count(*), min(created_at) FROM acquisitions
         WHERE user_id = ?1 AND created_at >= ?2 AND {COUNTED}"
    ))?
    .query_row(params![user, since], |row| {
        Ok((to_u32(row.get(0)?), row.get(1)?))
    })?)
}

/// Every counted request since `since`, and the oldest one's time, for the global quota.
pub fn count_all_since(c: &Connection, since: Millis) -> Result<(u32, Option<Millis>), DbError> {
    Ok(c.prepare_cached(&format!(
        "SELECT count(*), min(created_at) FROM acquisitions WHERE created_at >= ?1 AND {COUNTED}"
    ))?
    .query_row([since], |row| Ok((to_u32(row.get(0)?), row.get(1)?)))?)
}

/// A user's unfinished requests that run themselves (joined ones ride along).
pub fn count_active_for_user(c: &Connection, user: UserKey) -> Result<u32, DbError> {
    let n: i64 = c
        .prepare_cached(
            "SELECT count(*) FROM acquisitions
             WHERE user_id = ?1 AND joined_to IS NULL
               AND state NOT IN ('done', 'failed', 'cancelled')",
        )?
        .query_row([user], |row| row.get(0))?;
    Ok(to_u32(n))
}

/// Public ids of unfinished requests, for the staging janitor.
pub fn active_ids(c: &Connection) -> Result<Vec<AcquisitionId>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT pid FROM acquisitions WHERE state NOT IN ('done', 'failed', 'cancelled')",
    )?;
    let rows = statement.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The earliest time an unleased unfinished job becomes ready, for the runner's sleep.
pub fn next_ready_at(c: &Connection) -> Result<Option<Millis>, DbError> {
    Ok(c.prepare_cached(
        "SELECT min(max(next_attempt_at, coalesce(lease_until + 1, 0))) FROM acquisitions
         WHERE state NOT IN ('done', 'failed', 'cancelled') AND joined_to IS NULL",
    )?
    .query_row([], |row| row.get(0))?)
}

/// Releases every lease. At start only, since one process owns a data directory.
pub fn release_leases(tx: &mut WriteTx<'_>) -> Result<u64, DbError> {
    let n = tx
        .prepare_cached("UPDATE acquisitions SET lease_until = NULL WHERE lease_until IS NOT NULL")?
        .execute([])?;
    Ok(n as u64)
}

/// Marks a candidate tried, with why it failed.
pub fn mark_tried(
    tx: &mut WriteTx<'_>,
    token: &CandidateToken,
    failure: Option<&str>,
) -> Result<(), DbError> {
    tx.prepare_cached(
        "UPDATE acquisition_candidates SET tried_at = ?2, failure = ?3 WHERE token = ?1",
    )?
    .execute(params![token, tx.now(), failure])?;
    Ok(())
}

/// Newest first.
pub fn list(c: &Connection, filter: &AcquisitionFilter) -> Result<Page<AcquisitionRow>, DbError> {
    let limit = clamp_limit(filter.limit, 50, 500);
    let keys = [OrderKey::desc("q.created_at"), OrderKey::desc("q.id")];
    let after = decode_keyset(filter.cursor.as_deref(), keys.len())?;
    let (after_sql, after_params) = after_condition(&keys, &after);
    let state_sql = match filter.state {
        None => String::from("1"),
        Some(AcquisitionStateFilter::Active) => {
            String::from("q.state NOT IN ('done', 'failed', 'cancelled')")
        }
        Some(AcquisitionStateFilter::Finished) => {
            String::from("q.state IN ('done', 'failed', 'cancelled')")
        }
        Some(AcquisitionStateFilter::Exactly(state)) => format!("q.state = '{}'", state.as_str()),
    };
    let mut params = vec![filter.user.map_or(Value::Null, |u| Value::Integer(u.0))];
    params.extend(after_params);
    let sql = format!(
        "{} WHERE (?1 IS NULL OR q.user_id = ?1) AND {state_sql} AND {after_sql} {} LIMIT {}",
        SELECT.replacen(
            " FROM acquisitions q",
            ", q.created_at, q.id FROM acquisitions q",
            1
        ),
        order_by(&keys),
        limit + 1
    );
    let mut statement = c.prepare_cached(&sql)?;
    let mut rows: Vec<(AcquisitionRow, Vec<Value>)> = statement
        .query_map(params_from_iter(params), |row| {
            Ok((map_row(row)?, read_keys(row, COLUMNS, 2)?))
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

/// Deletes expired, unattached candidates.
pub fn prune_candidates(tx: &mut WriteTx<'_>, now: Millis) -> Result<u64, DbError> {
    let n = tx
        .prepare_cached(
            "DELETE FROM acquisition_candidates WHERE acquisition_id IS NULL AND expires_at <= ?1",
        )?
        .execute([now])?;
    Ok(n as u64)
}
