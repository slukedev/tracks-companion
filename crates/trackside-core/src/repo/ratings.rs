//! Ratings from 1 to 5.

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{Change, ChangeKind, ChangeOp, DbError, WriteTx};
use crate::ids::UserKey;
use crate::records::ItemRef;

/// A rating as sync returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rating {
    pub item: ItemRef,
    pub rating: u8,
}

/// Fails with `Invalid` outside 1 to 5.
pub fn set(tx: &mut WriteTx<'_>, user: UserKey, item: &ItemRef, rating: u8) -> Result<(), DbError> {
    if !(1..=5).contains(&rating) {
        return Err(DbError::Invalid(String::from("rating")));
    }
    let now = tx.now();
    tx.prepare_cached(
        "INSERT INTO ratings (user_id, item_kind, item_pid, rating, rated_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (user_id, item_kind, item_pid) DO UPDATE SET
             rating = excluded.rating, rated_at = excluded.rated_at",
    )?
    .execute(params![user, item.kind().as_str(), item.pid(), rating, now])?;
    tx.record(Change::for_user(
        ChangeKind::Rating,
        item.change_entity(),
        user,
        ChangeOp::Upsert,
    ))?;
    Ok(())
}

pub fn clear(tx: &mut WriteTx<'_>, user: UserKey, item: &ItemRef) -> Result<(), DbError> {
    let removed = tx
        .prepare_cached(
            "DELETE FROM ratings WHERE user_id = ?1 AND item_kind = ?2 AND item_pid = ?3",
        )?
        .execute(params![user, item.kind().as_str(), item.pid()])?;
    if removed > 0 {
        tx.record(Change::for_user(
            ChangeKind::Rating,
            item.change_entity(),
            user,
            ChangeOp::Delete,
        ))?;
    }
    Ok(())
}

/// The rating of one item, if any.
pub fn get(c: &Connection, user: UserKey, item: &ItemRef) -> Result<Option<Rating>, DbError> {
    let rating: Option<i64> = c
        .prepare_cached(
            "SELECT rating FROM ratings WHERE user_id = ?1 AND item_kind = ?2 AND item_pid = ?3",
        )?
        .query_row(params![user, item.kind().as_str(), item.pid()], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(rating
        .and_then(|r| u8::try_from(r).ok())
        .map(|rating| Rating {
            item: item.clone(),
            rating,
        }))
}

/// All of a user's ratings, for sync snapshots, after `(kind, pid)` in key order.
pub(crate) fn all_after(
    c: &Connection,
    user: UserKey,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<(String, Rating)>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT item_kind, item_pid, rating FROM ratings
         WHERE user_id = ?1 AND item_kind || ':' || item_pid > ?2
         ORDER BY item_kind || ':' || item_pid LIMIT ?3",
    )?;
    let mut rows = statement.query(params![user, after.unwrap_or(""), i64::from(limit)])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let pid: String = row.get(1)?;
        let rating: i64 = row.get(2)?;
        if let (Some(item), Ok(rating)) =
            (super::stars::item_ref(&kind, &pid), u8::try_from(rating))
        {
            out.push((format!("{kind}:{pid}"), Rating { item, rating }));
        }
    }
    Ok(out)
}
