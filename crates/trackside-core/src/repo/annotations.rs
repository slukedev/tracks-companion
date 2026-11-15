//! A user's stars, ratings and play stats on items, for the `me` object in browse responses
//! and Subsonic's `starred`, `userRating` and `playCount`.

use std::collections::HashMap;

use rusqlite::Connection;

use super::sql::{json_strings, to_u32};
use crate::db::DbError;
use crate::ids::{AlbumId, ArtistId, PublicId, SongId, UserKey};
use crate::records::{ItemKind, ItemRef, UserAnnotations};
use crate::time::Millis;

fn item(kind: ItemKind, pid: &str) -> Option<ItemRef> {
    Some(match kind {
        ItemKind::Song => ItemRef::Song(SongId::parse(pid).ok()?),
        ItemKind::Album => ItemRef::Album(AlbumId::parse(pid).ok()?),
        ItemKind::Artist => ItemRef::Artist(ArtistId::parse(pid).ok()?),
    })
}

/// Annotations for each item that has any; items without any are absent. Play stats are kept
/// for songs only.
pub fn for_items(
    c: &Connection,
    user: UserKey,
    items: &[ItemRef],
) -> Result<HashMap<ItemRef, UserAnnotations>, DbError> {
    let mut out: HashMap<ItemRef, UserAnnotations> = HashMap::new();
    if items.is_empty() {
        return Ok(out);
    }
    let keys = json_strings(items.iter().map(ItemRef::change_entity));
    let mut stars = c.prepare_cached(
        "SELECT item_kind, item_pid, starred_at FROM stars
         WHERE user_id = ?1 AND item_kind || ':' || item_pid IN (SELECT value FROM json_each(?2))",
    )?;
    let mut rows = stars.query(rusqlite::params![user, keys])?;
    while let Some(row) = rows.next()? {
        let Some(kind) = ItemKind::parse(&row.get::<_, String>(0)?) else {
            continue;
        };
        if let Some(item) = item(kind, &row.get::<_, String>(1)?) {
            out.entry(item).or_default().starred_at = Some(row.get::<_, Millis>(2)?);
        }
    }
    let mut ratings = c.prepare_cached(
        "SELECT item_kind, item_pid, rating FROM ratings
         WHERE user_id = ?1 AND item_kind || ':' || item_pid IN (SELECT value FROM json_each(?2))",
    )?;
    let mut rows = ratings.query(rusqlite::params![user, keys])?;
    while let Some(row) = rows.next()? {
        let Some(kind) = ItemKind::parse(&row.get::<_, String>(0)?) else {
            continue;
        };
        if let Some(item) = item(kind, &row.get::<_, String>(1)?) {
            let rating: i64 = row.get(2)?;
            out.entry(item).or_default().rating = u8::try_from(rating).ok();
        }
    }
    let songs = json_strings(items.iter().filter_map(|item| match item {
        ItemRef::Song(id) => Some(id.as_str().to_owned()),
        _ => None,
    }));
    let mut plays = c.prepare_cached(
        "SELECT song_pid, play_count, last_played_at FROM play_stats
         WHERE user_id = ?1 AND song_pid IN (SELECT value FROM json_each(?2))",
    )?;
    let mut rows = plays.query(rusqlite::params![user, songs])?;
    while let Some(row) = rows.next()? {
        if let Ok(song) = SongId::parse(&row.get::<_, String>(0)?) {
            let entry = out.entry(ItemRef::Song(song)).or_default();
            entry.play_count = to_u32(row.get(1)?);
            entry.last_played_at = Some(row.get(2)?);
        }
    }
    Ok(out)
}
