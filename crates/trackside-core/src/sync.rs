//! Delta sync (architecture section 5, api.md section 10).

use std::collections::HashSet;

use base64::Engine;
use rusqlite::{Connection, params};

use crate::access::Access;
use crate::db::{ChangeKind, DbError, WriteTx};
use crate::ids::{AlbumId, ArtistId, LibraryId, PlaylistId, PublicId, Seq, SongId};
use crate::records::{Album, Artist, ItemRef, Library, Song};
use crate::repo;
use crate::repo::play_queue::PlayQueue;
use crate::repo::playlists::Playlist;
use crate::repo::ratings::Rating;
use crate::repo::settings;
use crate::repo::sql::visible;
use crate::repo::stars::Star;
use crate::time::Millis;

/// Snapshot kinds, in the order snapshot pages go through them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    Libraries,
    Artists,
    Albums,
    Songs,
    Playlists,
    Stars,
    Ratings,
    PlayStats,
    PlayQueue,
}

impl SnapshotKind {
    pub const ORDER: [Self; 9] = [
        Self::Libraries,
        Self::Artists,
        Self::Albums,
        Self::Songs,
        Self::Playlists,
        Self::Stars,
        Self::Ratings,
        Self::PlayStats,
        Self::PlayQueue,
    ];

    /// The kind after this one, or `None` after the last.
    pub fn next(self) -> Option<Self> {
        let index = Self::ORDER.iter().position(|&k| k == self)?;
        Self::ORDER.get(index + 1).copied()
    }
}

/// Where a snapshot continues: the kind being paged and the internal id to continue after.
/// Per-user kinds without an internal id (stars, ratings, play stats) continue after a text
/// key instead.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SnapshotPosition {
    pub kind: SnapshotKind,
    pub after: i64,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "k")]
    pub after_key: Option<String>,
}

/// An opaque sync cursor: base64url of `{v:1, epoch, seq}`, plus the snapshot position while
/// a snapshot is in progress (then `seq` is the snapshot's pinned seq).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncCursor {
    pub epoch: i64,
    pub seq: Seq,
    pub snapshot: Option<SnapshotPosition>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WireCursor {
    v: u8,
    e: i64,
    s: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    p: Option<SnapshotPosition>,
}

impl SyncCursor {
    pub fn encode(&self) -> String {
        let wire = WireCursor {
            v: 1,
            e: self.epoch,
            s: self.seq.0,
            p: self.snapshot.clone(),
        };
        // Serializing a struct of integers and enums cannot fail.
        let json = serde_json::to_vec(&wire).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    pub fn decode(s: &str) -> Result<Self, SyncError> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.trim())
            .map_err(|_| SyncError::BadCursor)?;
        let wire: WireCursor = serde_json::from_slice(&bytes).map_err(|_| SyncError::BadCursor)?;
        if wire.v != 1 || wire.s < 0 {
            return Err(SyncError::BadCursor);
        }
        Ok(Self {
            epoch: wire.e,
            seq: Seq(wire.s),
            snapshot: wire.p,
        })
    }
}

/// A user's play count and last play of one song.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayStat {
    pub song: SongId,
    pub play_count: u32,
    pub last_played_at: Millis,
}

/// A playlist with its entries' song ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistWithEntries {
    pub playlist: Playlist,
    pub entries: Vec<SongId>,
}

/// One snapshot page holds one kind.
#[derive(Clone, Debug, PartialEq)]
pub enum SnapshotItems {
    Libraries(Vec<Library>),
    Artists(Vec<Artist>),
    Albums(Vec<Album>),
    Songs(Vec<Song>),
    Playlists(Vec<PlaylistWithEntries>),
    Stars(Vec<Star>),
    Ratings(Vec<Rating>),
    PlayStats(Vec<PlayStat>),
    PlayQueue(Option<PlayQueue>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotPage {
    /// The pinned seq every page of this snapshot carries.
    pub seq: Seq,
    /// The caller's libraries, on every page.
    pub libraries: Vec<Library>,
    pub items: SnapshotItems,
    /// The next snapshot page, or after the last page the delta cursor.
    pub next_cursor: SyncCursor,
    pub complete: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeltaUpserts {
    pub songs: Vec<Song>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    pub libraries: Vec<Library>,
    pub playlists: Vec<PlaylistWithEntries>,
    pub stars: Vec<Star>,
    pub ratings: Vec<Rating>,
    pub play_stats: Vec<PlayStat>,
    pub play_queue: Option<PlayQueue>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeltaDeletes {
    pub songs: Vec<SongId>,
    pub albums: Vec<AlbumId>,
    pub artists: Vec<ArtistId>,
    pub libraries: Vec<LibraryId>,
    pub playlists: Vec<PlaylistId>,
    pub stars: Vec<ItemRef>,
    pub ratings: Vec<ItemRef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeltaPage {
    pub upserts: DeltaUpserts,
    pub deletes: DeltaDeletes,
    pub next_cursor: SyncCursor,
    pub has_more: bool,
}

#[derive(Clone, Debug, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "one page per request, moved once"
)]
pub enum SyncPage {
    Snapshot(SnapshotPage),
    Delta(DeltaPage),
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SyncError {
    /// The cursor's epoch is stale or below the pruned seq: answer `410 sync_reset`.
    #[error("reset required")]
    Reset,
    #[error("bad cursor")]
    BadCursor,
    #[error(transparent)]
    Db(#[from] DbError),
}

/// The default and largest page sizes of `GET /api/v1/sync`.
pub const DEFAULT_LIMIT: u32 = 2000;
pub const MAX_LIMIT: u32 = 5000;

/// One page of sync for the caller: a snapshot page without a cursor or with a snapshot
/// cursor, a delta page with a delta cursor. `limit` is clamped to `1..=MAX_LIMIT`.
pub fn page(
    c: &Connection,
    a: &Access,
    cursor: Option<&SyncCursor>,
    limit: u32,
) -> Result<SyncPage, SyncError> {
    let limit = if limit == 0 {
        DEFAULT_LIMIT
    } else {
        limit.min(MAX_LIMIT)
    };
    let pruned: i64 = settings::get(c, settings::keys::SYNC_PRUNED_THROUGH)?.unwrap_or(0);
    match cursor {
        None => {
            let seq = latest_seq(c)?;
            let start = SnapshotPosition {
                kind: SnapshotKind::Libraries,
                after: 0,
                after_key: None,
            };
            Ok(SyncPage::Snapshot(snapshot(c, a, seq, start, limit)?))
        }
        Some(cursor) => {
            if cursor.epoch != a.sync_epoch || cursor.seq.0 < pruned {
                return Err(SyncError::Reset);
            }
            match &cursor.snapshot {
                Some(position) => Ok(SyncPage::Snapshot(snapshot(
                    c,
                    a,
                    cursor.seq,
                    position.clone(),
                    limit,
                )?)),
                None => Ok(SyncPage::Delta(delta(c, a, cursor.seq, limit)?)),
            }
        }
    }
}

fn snapshot(
    c: &Connection,
    a: &Access,
    seq: Seq,
    mut position: SnapshotPosition,
    limit: u32,
) -> Result<SnapshotPage, DbError> {
    let libraries = repo::libraries::list(c, a)?;
    loop {
        let (items, count, resume) = snapshot_items(c, a, &position, limit, &libraries)?;
        let full = count >= limit as usize;
        if full && let Some(resume) = resume {
            return Ok(SnapshotPage {
                seq,
                libraries,
                items,
                next_cursor: SyncCursor {
                    epoch: a.sync_epoch,
                    seq,
                    snapshot: Some(resume),
                },
                complete: false,
            });
        }
        let next = position.kind.next();
        if count > 0 || next.is_none() {
            let (snapshot, complete) = match next {
                Some(kind) => (
                    Some(SnapshotPosition {
                        kind,
                        after: 0,
                        after_key: None,
                    }),
                    false,
                ),
                None => (None, true),
            };
            return Ok(SnapshotPage {
                seq,
                libraries,
                items,
                next_cursor: SyncCursor {
                    epoch: a.sync_epoch,
                    seq,
                    snapshot,
                },
                complete,
            });
        }
        position = SnapshotPosition {
            kind: next.unwrap_or(SnapshotKind::PlayQueue),
            after: 0,
            after_key: None,
        };
    }
}

/// One page of one kind: the items, how many, and where the kind continues if the page is
/// full.
#[expect(clippy::too_many_lines, reason = "one arm per snapshot kind")]
fn snapshot_items(
    c: &Connection,
    a: &Access,
    position: &SnapshotPosition,
    limit: u32,
    libraries: &[Library],
) -> Result<(SnapshotItems, usize, Option<SnapshotPosition>), DbError> {
    let resume_id = |kind: SnapshotKind, last: Option<i64>| {
        last.map(|after| SnapshotPosition {
            kind,
            after,
            after_key: None,
        })
    };
    let resume_key = |kind: SnapshotKind, last: Option<String>| {
        last.map(|key| SnapshotPosition {
            kind,
            after: 0,
            after_key: Some(key),
        })
    };
    let after = position.after;
    let key = position.after_key.as_deref();
    Ok(match position.kind {
        SnapshotKind::Libraries => (
            SnapshotItems::Libraries(libraries.to_vec()),
            libraries.len(),
            None,
        ),
        SnapshotKind::Artists => {
            let rows = repo::artists::after_id(c, a, after, limit)?;
            let last = rows.last().map(|(id, _)| *id);
            let count = rows.len();
            (
                SnapshotItems::Artists(rows.into_iter().map(|(_, r)| r).collect()),
                count,
                resume_id(SnapshotKind::Artists, last),
            )
        }
        SnapshotKind::Albums => {
            let tail = format!(
                "WHERE a.id > ?1 AND {} ORDER BY a.id LIMIT {limit}",
                visible(a, "a.library_id", None)
            );
            let rows = repo::albums::load_keyed(c, &tail, &[&after])?;
            let last = rows.last().map(|(id, _)| *id);
            let count = rows.len();
            (
                SnapshotItems::Albums(rows.into_iter().map(|(_, r)| r).collect()),
                count,
                resume_id(SnapshotKind::Albums, last),
            )
        }
        SnapshotKind::Songs => {
            let tail = format!(
                "WHERE s.id > ?1 AND {} ORDER BY s.id LIMIT {limit}",
                visible(a, "s.library_id", None)
            );
            let rows = repo::songs::load_keyed(c, &tail, &[&after])?;
            let last = rows.last().map(|(id, _)| *id);
            let count = rows.len();
            (
                SnapshotItems::Songs(rows.into_iter().map(|(_, r)| r).collect()),
                count,
                resume_id(SnapshotKind::Songs, last),
            )
        }
        SnapshotKind::Playlists => {
            let rows = repo::playlists::visible_after(c, a, after, limit)?;
            let last = rows.last().map(|(id, _)| *id);
            let count = rows.len();
            let mut items = Vec::with_capacity(count);
            for (key, playlist) in rows {
                let entries = repo::playlists::entries_for(c, a, key, playlist.owner)?
                    .into_iter()
                    .map(|e| e.song)
                    .collect();
                items.push(PlaylistWithEntries { playlist, entries });
            }
            (
                SnapshotItems::Playlists(items),
                count,
                resume_id(SnapshotKind::Playlists, last),
            )
        }
        SnapshotKind::Stars => {
            let rows = repo::stars::all_after(c, a.user, key, limit)?;
            let last = rows.last().map(|(k, _)| k.clone());
            let count = rows.len();
            (
                SnapshotItems::Stars(rows.into_iter().map(|(_, s)| s).collect()),
                count,
                resume_key(SnapshotKind::Stars, last),
            )
        }
        SnapshotKind::Ratings => {
            let rows = repo::ratings::all_after(c, a.user, key, limit)?;
            let last = rows.last().map(|(k, _)| k.clone());
            let count = rows.len();
            (
                SnapshotItems::Ratings(rows.into_iter().map(|(_, r)| r).collect()),
                count,
                resume_key(SnapshotKind::Ratings, last),
            )
        }
        SnapshotKind::PlayStats => {
            let rows = repo::listens::play_stats_after(c, a.user, key, limit)?;
            let last = rows.last().map(|(song, _, _)| song.as_str().to_owned());
            let count = rows.len();
            (
                SnapshotItems::PlayStats(
                    rows.into_iter()
                        .map(|(song, play_count, last_played_at)| PlayStat {
                            song,
                            play_count,
                            last_played_at,
                        })
                        .collect(),
                ),
                count,
                resume_key(SnapshotKind::PlayStats, last),
            )
        }
        SnapshotKind::PlayQueue => {
            let queue = repo::play_queue::get(c, a.user)?;
            let count = usize::from(queue.is_some());
            (SnapshotItems::PlayQueue(queue), count, None)
        }
    })
}

/// One `change_log` row a delta resolves.
struct ChangeRow {
    seq: i64,
    kind: String,
    entity: String,
    changed_at: Millis,
}

#[expect(clippy::too_many_lines, reason = "one arm per change kind")]
fn delta(c: &Connection, a: &Access, after: Seq, limit: u32) -> Result<DeltaPage, DbError> {
    let sql = format!(
        "SELECT seq, kind, entity_pid, changed_at FROM change_log
         WHERE seq > ?1 AND user_id IN (0, ?2) AND (library_id IS NULL OR {})
         ORDER BY seq LIMIT {}",
        visible(a, "library_id", None),
        limit + 1
    );
    let mut statement = c.prepare_cached(&sql)?;
    let mut rows: Vec<ChangeRow> = statement
        .query_map(params![after, a.user], |row| {
            Ok(ChangeRow {
                seq: row.get(0)?,
                kind: row.get(1)?,
                entity: row.get(2)?,
                changed_at: row.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let last = rows.last().map_or(after, |row| Seq(row.seq));

    let mut upserts = DeltaUpserts::default();
    let mut deletes = DeltaDeletes::default();
    let mut songs = Vec::new();
    let mut albums = Vec::new();
    let mut artists = Vec::new();
    let mut seen = HashSet::new();
    for row in &rows {
        if !seen.insert((row.kind.as_str(), row.entity.as_str())) {
            continue;
        }
        match ChangeKind::parse(&row.kind) {
            Some(ChangeKind::Song) => songs.extend(SongId::parse(&row.entity).ok()),
            Some(ChangeKind::Album) => albums.extend(AlbumId::parse(&row.entity).ok()),
            Some(ChangeKind::Artist) => artists.extend(ArtistId::parse(&row.entity).ok()),
            Some(ChangeKind::Library) => {
                if let Ok(id) = LibraryId::parse(&row.entity) {
                    match repo::libraries::get(c, a, &id)? {
                        Some(library) => upserts.libraries.push(library),
                        None => deletes.libraries.push(id),
                    }
                }
            }
            Some(ChangeKind::Playlist) => {
                if let Ok(id) = PlaylistId::parse(&row.entity) {
                    match repo::playlists::get(c, a, &id)? {
                        Some((playlist, entries)) => upserts.playlists.push(PlaylistWithEntries {
                            playlist,
                            entries: entries.into_iter().map(|e| e.song).collect(),
                        }),
                        None => deletes.playlists.push(id),
                    }
                }
            }
            Some(ChangeKind::Star) => {
                if let Some(item) = item_of(&row.entity) {
                    match repo::stars::get(c, a.user, &item)? {
                        Some(star) => upserts.stars.push(star),
                        None => deletes.stars.push(item),
                    }
                }
            }
            Some(ChangeKind::Rating) => {
                if let Some(item) = item_of(&row.entity) {
                    match repo::ratings::get(c, a.user, &item)? {
                        Some(rating) => upserts.ratings.push(rating),
                        None => deletes.ratings.push(item),
                    }
                }
            }
            Some(ChangeKind::PlayStat) => {
                if let Ok(song) = SongId::parse(&row.entity)
                    && let Some((play_count, last_played_at)) =
                        repo::listens::play_stat(c, a.user, &song)?
                {
                    upserts.play_stats.push(PlayStat {
                        song,
                        play_count,
                        last_played_at,
                    });
                }
            }
            Some(ChangeKind::PlayQueue) => {
                upserts.play_queue = Some(repo::play_queue::get(c, a.user)?.unwrap_or(PlayQueue {
                    entries: Vec::new(),
                    current: None,
                    position_ms: 0,
                    changed_by: String::new(),
                    device: None,
                    updated_at: row.changed_at,
                }));
            }
            Some(ChangeKind::Acquisition) | None => {}
        }
    }
    let found = repo::songs::get_many(c, a, &songs)?;
    let present: HashSet<SongId> = found.iter().map(|s| s.id.clone()).collect();
    deletes
        .songs
        .extend(songs.into_iter().filter(|id| !present.contains(id)));
    upserts.songs = found;
    let found = repo::albums::get_many(c, a, &albums)?;
    let present: HashSet<AlbumId> = found.iter().map(|s| s.id.clone()).collect();
    deletes
        .albums
        .extend(albums.into_iter().filter(|id| !present.contains(id)));
    upserts.albums = found;
    let found = repo::artists::get_many(c, a, &artists)?;
    let present: HashSet<ArtistId> = found.iter().map(|s| s.id.clone()).collect();
    deletes
        .artists
        .extend(artists.into_iter().filter(|id| !present.contains(id)));
    upserts.artists = found;
    Ok(DeltaPage {
        upserts,
        deletes,
        next_cursor: SyncCursor {
            epoch: a.sync_epoch,
            seq: last,
            snapshot: None,
        },
        has_more,
    })
}

fn item_of(entity: &str) -> Option<ItemRef> {
    let (kind, pid) = entity.split_once(':')?;
    repo::stars::item_ref(kind, pid)
}

/// The highest seq handed out so far, for snapshots and `ETag`s. Pruned tombstones count: a
/// snapshot pinned below them would be refused at once.
pub fn latest_seq(c: &Connection) -> Result<Seq, DbError> {
    let seq: i64 = c.query_row(
        "SELECT max(coalesce((SELECT max(seq) FROM change_log), 0),
                    coalesce((SELECT CAST(value AS INTEGER) FROM settings
                              WHERE key = 'sync.pruned_through'), 0))",
        [],
        |row| row.get(0),
    )?;
    Ok(Seq(seq))
}

/// Deletes tombstones recorded before `older_than` and raises `sync.pruned_through` to the
/// highest pruned seq, so cursors from before it take a new snapshot. Returns how many went.
pub fn prune_tombstones(tx: &mut WriteTx<'_>, older_than: Millis) -> Result<u64, DbError> {
    let highest: Option<i64> = tx
        .prepare_cached("SELECT max(seq) FROM change_log WHERE op = 'delete' AND changed_at < ?1")?
        .query_row([older_than], |row| row.get(0))?;
    let Some(highest) = highest else { return Ok(0) };
    let removed = tx
        .prepare_cached("DELETE FROM change_log WHERE op = 'delete' AND changed_at < ?1")?
        .execute([older_than])?;
    let current: i64 = settings::get(tx, settings::keys::SYNC_PRUNED_THROUGH)?.unwrap_or(0);
    settings::put(
        tx,
        settings::keys::SYNC_PRUNED_THROUGH,
        &highest.max(current),
    )?;
    Ok(removed as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_cursor_round_trips() -> Result<(), SyncError> {
        let cursor = SyncCursor {
            epoch: 3,
            seq: Seq(918_274),
            snapshot: None,
        };
        assert_eq!(SyncCursor::decode(&cursor.encode())?, cursor);
        Ok(())
    }

    #[test]
    fn snapshot_cursor_round_trips() -> Result<(), SyncError> {
        let cursor = SyncCursor {
            epoch: 1,
            seq: Seq(5),
            snapshot: Some(SnapshotPosition {
                kind: SnapshotKind::Albums,
                after: 1_200,
                after_key: None,
            }),
        };
        assert_eq!(SyncCursor::decode(&cursor.encode())?, cursor);
        Ok(())
    }

    #[test]
    fn garbage_cursor_is_bad() {
        assert!(matches!(
            SyncCursor::decode("not a cursor"),
            Err(SyncError::BadCursor)
        ));
        assert!(matches!(
            SyncCursor::decode("e30"),
            Err(SyncError::BadCursor)
        ));
    }

    #[test]
    fn snapshot_kinds_run_in_order() {
        assert_eq!(SnapshotKind::Libraries.next(), Some(SnapshotKind::Artists));
        assert_eq!(SnapshotKind::PlayQueue.next(), None);
    }
}
