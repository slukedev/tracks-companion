//! Artwork rows: what the media crate's store holds, so songs, albums and artists can point at
//! it. Ingest inserts a bare placeholder for an id it has not seen; [`put`] fills in the real
//! metadata, before or after.

use rusqlite::{Connection, OptionalExtension, params};

use super::sql::{bytes_path, from_u64, path_bytes, to_u32, to_u64, visible};
use crate::access::Access;
use crate::db::{Change, ChangeKind, ChangeOp, DbError, WriteTx};
use crate::ids::ArtworkId;
use crate::records::text_enum;
use crate::time::Millis;

text_enum!(
    /// Where an image came from.
    ArtworkSourceKind {
        Embedded => "embedded",
        Sidecar => "sidecar",
        Upload => "upload",
        Acquired => "acquired",
    }
);

/// An image to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewArtwork {
    pub id: ArtworkId,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub source: ArtworkSourceKind,
    /// The absolute file to re-extract it from, if the cache loses it.
    pub source_path: Option<std::path::PathBuf>,
}

/// A stored image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtworkRow {
    pub art: NewArtwork,
    pub created_at: Millis,
}

/// Records an image's metadata, replacing a placeholder or an older record of the same id.
pub fn put(tx: &mut WriteTx<'_>, art: &NewArtwork) -> Result<(), DbError> {
    tx.prepare_cached(
        "INSERT INTO artwork (id, mime, width, height, bytes, source_kind, source_path, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (id) DO UPDATE SET mime = excluded.mime, width = excluded.width,
             height = excluded.height, bytes = excluded.bytes,
             source_kind = excluded.source_kind,
             source_path = coalesce(excluded.source_path, source_path)",
    )?
    .execute(params![
        art.id,
        art.mime,
        art.width,
        art.height,
        from_u64(art.bytes),
        art.source.as_str(),
        art.source_path.as_deref().map(path_bytes),
        tx.now()
    ])?;
    Ok(())
}

pub fn get(c: &Connection, id: &ArtworkId) -> Result<Option<ArtworkRow>, DbError> {
    Ok(c.prepare_cached(
        "SELECT id, mime, width, height, bytes, source_kind, source_path, created_at
         FROM artwork WHERE id = ?1",
    )?
    .query_row([id], |row| {
        let source: String = row.get(5)?;
        let path: Option<Vec<u8>> = row.get(6)?;
        Ok(ArtworkRow {
            art: NewArtwork {
                id: row.get(0)?,
                mime: row.get(1)?,
                width: to_u32(row.get(2)?),
                height: to_u32(row.get(3)?),
                bytes: to_u64(row.get(4)?),
                source: ArtworkSourceKind::parse(&source).unwrap_or(ArtworkSourceKind::Embedded),
                source_path: path.as_deref().map(bytes_path),
            },
            created_at: row.get(7)?,
        })
    })
    .optional()?)
}

/// Whether the caller can see a song, album or artist that shows this image.
pub fn visible_to(c: &Connection, a: &Access, id: &ArtworkId) -> Result<bool, DbError> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM songs s WHERE s.artwork_id = ?1 AND {})
             OR EXISTS (SELECT 1 FROM albums al WHERE al.artwork_id = ?1 AND {})
             OR EXISTS (SELECT 1 FROM artists r WHERE r.artwork_id = ?1 AND {})",
        visible(a, "s.library_id", None),
        visible(a, "al.library_id", None),
        super::artists::exists(&visible(a, "x.library_id", None)),
    );
    Ok(c.prepare_cached(&sql)?.query_row([id], |row| row.get(0))?)
}

/// Sets the picture of the artist whose identity is `tracks_model::folded(name)`, recording
/// the artist for sync. Returns false when no such artist exists yet.
pub fn set_artist_artwork(
    tx: &mut WriteTx<'_>,
    name: &str,
    art: Option<&ArtworkId>,
) -> Result<bool, DbError> {
    let identity = tracks_model::folded(name);
    let pid: Option<String> = tx
        .prepare_cached(
            "UPDATE artists SET artwork_id = ?2, updated_at = ?3
             WHERE identity = ?1 AND artwork_id IS NOT ?2 RETURNING pid",
        )?
        .query_row(params![identity, art, tx.now()], |row| row.get(0))
        .optional()?;
    if let Some(pid) = pid {
        tx.record(Change::shared(
            ChangeKind::Artist,
            pid,
            None,
            ChangeOp::Upsert,
        ))?;
        return Ok(true);
    }
    Ok(tx
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM artists WHERE identity = ?1)")?
        .query_row([identity], |row| row.get(0))?)
}

/// Images no song, album or artist points at any more, for the media cache to drop.
pub fn unused(c: &Connection, limit: u32) -> Result<Vec<ArtworkId>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT id FROM artwork w
         WHERE NOT EXISTS (SELECT 1 FROM songs WHERE artwork_id = w.id)
           AND NOT EXISTS (SELECT 1 FROM albums WHERE artwork_id = w.id)
           AND NOT EXISTS (SELECT 1 FROM artists WHERE artwork_id = w.id)
         LIMIT ?1",
    )?;
    let rows = statement.query_map([i64::from(limit)], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Deletes the row of an unused image.
pub fn delete(tx: &mut WriteTx<'_>, id: &ArtworkId) -> Result<(), DbError> {
    tx.prepare_cached("DELETE FROM artwork WHERE id = ?1")?
        .execute([id])?;
    Ok(())
}
