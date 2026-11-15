//! Stars (favourites). Stars never have side effects beyond the star itself.

use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};

use super::sql::{
    OrderKey, after_condition, clamp_limit, decode_keyset, encode_keyset, order_by, read_keys,
    visible,
};
use crate::access::Access;
use crate::db::{Change, ChangeKind, ChangeOp, DbError, WriteTx};
use crate::ids::{AlbumId, ArtistId, PublicId, SongId, UserKey};
use crate::records::{ItemKind, ItemRef, Page};
use crate::time::Millis;

/// A star as sync and lists return it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Star {
    pub item: ItemRef,
    pub starred_at: Millis,
}

/// The item a stored kind and public id name, or `None` for a malformed row.
pub(crate) fn item_ref(kind: &str, pid: &str) -> Option<ItemRef> {
    Some(match ItemKind::parse(kind)? {
        ItemKind::Song => ItemRef::Song(SongId::parse(pid).ok()?),
        ItemKind::Album => ItemRef::Album(AlbumId::parse(pid).ok()?),
        ItemKind::Artist => ItemRef::Artist(ArtistId::parse(pid).ok()?),
    })
}

/// Stars an item (again), keeping the client's time for display.
pub fn set(tx: &mut WriteTx<'_>, user: UserKey, item: &ItemRef, at: Millis) -> Result<(), DbError> {
    tx.prepare_cached(
        "INSERT INTO stars (user_id, item_kind, item_pid, starred_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (user_id, item_kind, item_pid) DO UPDATE SET starred_at = excluded.starred_at",
    )?
    .execute(params![user, item.kind().as_str(), item.pid(), at])?;
    tx.record(Change::for_user(
        ChangeKind::Star,
        item.change_entity(),
        user,
        ChangeOp::Upsert,
    ))?;
    Ok(())
}

pub fn clear(tx: &mut WriteTx<'_>, user: UserKey, item: &ItemRef) -> Result<(), DbError> {
    let removed = tx
        .prepare_cached(
            "DELETE FROM stars WHERE user_id = ?1 AND item_kind = ?2 AND item_pid = ?3",
        )?
        .execute(params![user, item.kind().as_str(), item.pid()])?;
    if removed > 0 {
        tx.record(Change::for_user(
            ChangeKind::Star,
            item.change_entity(),
            user,
            ChangeOp::Delete,
        ))?;
    }
    Ok(())
}

/// The star on one item, if any.
pub fn get(c: &Connection, user: UserKey, item: &ItemRef) -> Result<Option<Star>, DbError> {
    use rusqlite::OptionalExtension;
    let at: Option<Millis> = c
        .prepare_cached(
            "SELECT starred_at FROM stars WHERE user_id = ?1 AND item_kind = ?2 AND item_pid = ?3",
        )?
        .query_row(params![user, item.kind().as_str(), item.pid()], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(at.map(|starred_at| Star {
        item: item.clone(),
        starred_at,
    }))
}

/// A condition that the item `st` names is visible to the caller.
pub(crate) fn item_visible(a: &Access, kind: ItemKind, alias: &str) -> String {
    match kind {
        ItemKind::Song => format!(
            "EXISTS (SELECT 1 FROM songs s WHERE s.pid = {alias}.item_pid AND {})",
            visible(a, "s.library_id", None)
        ),
        ItemKind::Album => format!(
            "EXISTS (SELECT 1 FROM albums al WHERE al.pid = {alias}.item_pid AND {})",
            visible(a, "al.library_id", None)
        ),
        ItemKind::Artist => format!(
            "EXISTS (SELECT 1 FROM artists r JOIN artist_libraries x ON x.artist_id = r.id
                     WHERE r.pid = {alias}.item_pid AND {})",
            visible(a, "x.library_id", None)
        ),
    }
}

/// The caller's stars of one kind that are visible to them, most recent first.
pub fn list(
    c: &Connection,
    a: &Access,
    kind: ItemKind,
    cursor: Option<&str>,
    limit: u32,
) -> Result<Page<Star>, DbError> {
    let limit = clamp_limit(limit, 100, 500);
    let keys = [
        OrderKey::desc("st.starred_at"),
        OrderKey::desc("st.item_pid"),
    ];
    let after = decode_keyset(cursor, keys.len())?;
    let (after_sql, after_params) = after_condition(&keys, &after);
    let sql = format!(
        "SELECT st.item_kind, st.item_pid, st.starred_at, st.starred_at, st.item_pid
         FROM stars st
         WHERE st.user_id = ? AND st.item_kind = ? AND {} AND {after_sql} {} LIMIT {}",
        item_visible(a, kind, "st"),
        order_by(&keys),
        limit + 1
    );
    let mut params = vec![
        Value::Integer(a.user.0),
        Value::Text(kind.as_str().to_owned()),
    ];
    params.extend(after_params);
    let mut statement = c.prepare_cached(&sql)?;
    let mut rows = statement.query(params_from_iter(params))?;
    let mut items: Vec<(Star, Vec<Value>)> = Vec::new();
    while let Some(row) = rows.next()? {
        let Some(item) = item_ref(&row.get::<_, String>(0)?, &row.get::<_, String>(1)?) else {
            continue;
        };
        items.push((
            Star {
                item,
                starred_at: row.get(2)?,
            },
            read_keys(row, 3, 2)?,
        ));
    }
    let more = items.len() > limit as usize;
    items.truncate(limit as usize);
    let next = if more {
        items.last().map(|(_, keys)| encode_keyset(keys))
    } else {
        None
    };
    Ok(Page {
        items: items.into_iter().map(|(star, _)| star).collect(),
        next,
    })
}

/// All of a user's stars, for sync snapshots, after `(kind, pid)` in key order.
pub(crate) fn all_after(
    c: &Connection,
    user: UserKey,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<(String, Star)>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT item_kind, item_pid, starred_at FROM stars
         WHERE user_id = ?1 AND item_kind || ':' || item_pid > ?2
         ORDER BY item_kind || ':' || item_pid LIMIT ?3",
    )?;
    let mut rows = statement.query(params![user, after.unwrap_or(""), i64::from(limit)])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let pid: String = row.get(1)?;
        if let Some(item) = item_ref(&kind, &pid) {
            out.push((
                format!("{kind}:{pid}"),
                Star {
                    item,
                    starred_at: row.get(2)?,
                },
            ));
        }
    }
    Ok(out)
}
