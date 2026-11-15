//! Genres with counts over the caller's libraries.

use rusqlite::Connection;

use super::sql::{to_u64, visible};
use crate::access::Access;
use crate::db::DbError;
use crate::records::GenreCount;

/// Genres with at least one visible song, by name.
pub fn list(c: &Connection, a: &Access) -> Result<Vec<GenreCount>, DbError> {
    let sql = format!(
        "SELECT g.name, count(DISTINCT s.id), count(DISTINCT s.album_id)
         FROM genres g
         JOIN song_genres sg ON sg.genre_id = g.id
         JOIN songs s ON s.id = sg.song_id
         WHERE s.missing_since IS NULL AND {}
         GROUP BY g.id
         ORDER BY g.name_key",
        visible(a, "s.library_id", None)
    );
    let mut statement = c.prepare_cached(&sql)?;
    let rows = statement.query_map([], |row| {
        Ok(GenreCount {
            name: row.get(0)?,
            song_count: to_u64(row.get(1)?),
            album_count: to_u64(row.get(2)?),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}
