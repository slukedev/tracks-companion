//! Lyrics found by the scanner.

use rusqlite::Connection;

use super::sql::visible;
use crate::access::Access;
use crate::db::DbError;
use crate::ids::SongId;
use crate::records::{Lyrics, LyricsKind, LyricsSource};

/// Synced lyrics first, then plain; sidecar files before embedded tags. Empty when the song has
/// none or is invisible.
pub fn for_song(c: &Connection, a: &Access, song: &SongId) -> Result<Vec<Lyrics>, DbError> {
    let sql = format!(
        "SELECT ly.kind, ly.lang, ly.source, ly.content
         FROM lyrics ly JOIN songs s ON s.id = ly.song_id
         WHERE s.pid = ?1 AND {}
         ORDER BY ly.kind = 'plain', ly.source = 'embedded', ly.lang",
        visible(a, "s.library_id", None)
    );
    let mut statement = c.prepare_cached(&sql)?;
    let mut rows = statement.query([song])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let (Some(kind), Some(source)) = (
            LyricsKind::parse(&row.get::<_, String>(0)?),
            LyricsSource::parse(&row.get::<_, String>(2)?),
        ) else {
            continue;
        };
        out.push(Lyrics {
            kind,
            lang: row.get(1)?,
            source,
            content: row.get(3)?,
        });
    }
    Ok(out)
}
