//! Listens and the play stats that follow them.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use super::sql::{opt_u32, to_u32};
use crate::db::{Change, ChangeKind, ChangeOp, DbError, WriteTx};
use crate::ids::{DeviceKey, PublicId, SongId, UserKey};
use crate::records::text_enum;
use crate::time::Millis;

text_enum!(
    /// Which API a listen came through.
    ListenSource {
        Native => "native",
        Subsonic => "subsonic",
        Report => "report",
    }
);

/// One submitted listen. `song_key` is computed from title and artist on insert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewListen {
    /// The client's id for the play; resubmitting it is a duplicate, not a second play.
    pub client_listen_id: String,
    /// `None` for a song that is not on this server.
    pub song: Option<SongId>,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// Client clock, as the client recorded it.
    pub started_at: Millis,
    pub played_ms: Option<u32>,
    pub duration_ms: Option<u32>,
    pub source: ListenSource,
}

/// Why a listen was not stored. The client drops it rather than retrying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// Started more than a few minutes after the server's now.
    Future,
    Invalid,
}

/// The fate of one submitted listen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenStatus {
    Accepted,
    /// Already stored; success for the client.
    Duplicate,
    Rejected(RejectReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenOutcome {
    pub client_listen_id: String,
    pub status: ListenStatus,
}

/// A stored listen, for export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenRow {
    pub id: i64,
    pub client_listen_id: String,
    pub song: Option<SongId>,
    pub song_key: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub started_at: Millis,
    pub played_ms: Option<u32>,
    pub duration_ms: Option<u32>,
    pub source: ListenSource,
    pub received_at: Millis,
}

/// How far past the server's clock a listen may start: clocks on phones drift, and a listen
/// from further ahead than this is not a listen that happened.
pub const FUTURE_TOLERANCE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// The longest `client_listen_id` accepted.
const MAX_CLIENT_ID: usize = 200;

fn valid(item: &NewListen) -> bool {
    let id = item.client_listen_id.trim();
    !id.is_empty()
        && id.len() <= MAX_CLIENT_ID
        && !item.title.trim().is_empty()
        && !item.artist.trim().is_empty()
        && item.started_at.0 > 0
}

/// Stores listens idempotently by `client_listen_id` and updates play stats, recording a
/// `play_stat` change for each song that moved. Items starting more than
/// [`FUTURE_TOLERANCE`] after the server's now are rejected as `future`.
pub fn submit(
    tx: &mut WriteTx<'_>,
    user: UserKey,
    device: Option<DeviceKey>,
    items: &[NewListen],
) -> Result<Vec<ListenOutcome>, DbError> {
    let now = tx.now();
    let latest = now.saturating_add(FUTURE_TOLERANCE);
    let mut outcomes = Vec::with_capacity(items.len());
    let mut played: HashMap<SongId, Millis> = HashMap::new();
    for item in items {
        let status =
            if !valid(item) {
                ListenStatus::Rejected(RejectReason::Invalid)
            } else if item.started_at > latest {
                ListenStatus::Rejected(RejectReason::Future)
            } else {
                let inserted = tx
                .prepare_cached(
                    "INSERT INTO listens (user_id, device_id, client_listen_id, song_pid, song_key,
                         title, artist, album, started_at, played_ms, duration_ms, source,
                         received_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                     ON CONFLICT (user_id, client_listen_id) DO NOTHING",
                )?
                .execute(params![
                    user,
                    device,
                    item.client_listen_id.trim(),
                    item.song,
                    tracks_model::song_key(&item.title, &item.artist),
                    item.title.trim(),
                    item.artist.trim(),
                    item.album.as_deref().map(str::trim).filter(|a| !a.is_empty()),
                    item.started_at,
                    item.played_ms,
                    item.duration_ms,
                    item.source.as_str(),
                    now
                ])?;
                if inserted == 0 {
                    ListenStatus::Duplicate
                } else {
                    if let Some(song) = &item.song {
                        tx.prepare_cached(
                            "INSERT INTO play_stats (user_id, song_pid, play_count, last_played_at)
                         VALUES (?1, ?2, 1, ?3)
                         ON CONFLICT (user_id, song_pid) DO UPDATE SET
                             play_count = play_count + 1,
                             last_played_at = max(last_played_at, excluded.last_played_at)",
                        )?
                        .execute(params![user, song, item.started_at])?;
                        let latest_play = played.entry(song.clone()).or_insert(item.started_at);
                        *latest_play = (*latest_play).max(item.started_at);
                    }
                    ListenStatus::Accepted
                }
            };
        outcomes.push(ListenOutcome {
            client_listen_id: item.client_listen_id.clone(),
            status,
        });
    }
    for song in played.keys() {
        tx.record(Change::for_user(
            ChangeKind::PlayStat,
            song.as_str(),
            user,
            ChangeOp::Upsert,
        ))?;
    }
    Ok(outcomes)
}

/// A user's listens oldest first, after internal id `after`. Returns the page and the id to
/// continue after.
pub fn export(
    c: &Connection,
    user: UserKey,
    after: Option<i64>,
    limit: u32,
) -> Result<(Vec<ListenRow>, Option<i64>), DbError> {
    let limit = limit.clamp(1, 10_000);
    let mut statement = c.prepare_cached(
        "SELECT id, client_listen_id, song_pid, song_key, title, artist, album, started_at,
                played_ms, duration_ms, source, received_at
         FROM listens WHERE user_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3",
    )?;
    let mut rows = statement.query(params![user, after.unwrap_or(0), i64::from(limit) + 1])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let song: Option<String> = row.get(2)?;
        let source: String = row.get(10)?;
        out.push(ListenRow {
            id: row.get(0)?,
            client_listen_id: row.get(1)?,
            song: song.and_then(|s| SongId::parse(&s).ok()),
            song_key: row.get(3)?,
            title: row.get(4)?,
            artist: row.get(5)?,
            album: row.get(6)?,
            started_at: row.get(7)?,
            played_ms: opt_u32(row.get(8)?),
            duration_ms: opt_u32(row.get(9)?),
            source: ListenSource::parse(&source).unwrap_or(ListenSource::Native),
            received_at: row.get(11)?,
        });
    }
    let more = out.len() > limit as usize;
    out.truncate(limit as usize);
    let next = if more {
        out.last().map(|row| row.id)
    } else {
        None
    };
    Ok((out, next))
}

/// A user's play count and last play of one song.
pub fn play_stat(
    c: &Connection,
    user: UserKey,
    song: &SongId,
) -> Result<Option<(u32, Millis)>, DbError> {
    Ok(c.prepare_cached(
        "SELECT play_count, last_played_at FROM play_stats WHERE user_id = ?1 AND song_pid = ?2",
    )?
    .query_row(params![user, song], |row| {
        Ok((to_u32(row.get(0)?), row.get(1)?))
    })
    .optional()?)
}

/// A user's play stats after song id `after`, by song id. For sync snapshots.
pub(crate) fn play_stats_after(
    c: &Connection,
    user: UserKey,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<(SongId, u32, Millis)>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT song_pid, play_count, last_played_at FROM play_stats
         WHERE user_id = ?1 AND song_pid > ?2 ORDER BY song_pid LIMIT ?3",
    )?;
    let mut rows = statement.query(params![user, after.unwrap_or(""), i64::from(limit)])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let pid: String = row.get(0)?;
        if let Ok(song) = SongId::parse(&pid) {
            out.push((song, to_u32(row.get(1)?), row.get(2)?));
        }
    }
    Ok(out)
}

/// How many listens a user has.
pub fn count(c: &Connection, user: UserKey) -> Result<u64, DbError> {
    let n: i64 = c
        .prepare_cached("SELECT count(*) FROM listens WHERE user_id = ?1")?
        .query_row([user], |row| row.get(0))?;
    Ok(super::sql::to_u64(n))
}
