//! The saved play queue (by song id and, with `indexBasedQueue`, by index) and bookmarks.

use trackside_auth::Scopes;
use trackside_core::records::Song;
use trackside_core::repo::play_queue::PlayQueueUpdate;
use trackside_core::{PublicId, SongId, repo};

use super::browse::song_nodes;
use super::notes;
use crate::error::ApiError;
use crate::ids;
use crate::map::{self, time};
use crate::response::{Body, Node};
use crate::{Call, Reply};

/// The most entries a saved queue holds.
const MAX_QUEUE: usize = 2000;

pub async fn get(call: &Call<'_>, by_index: bool) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let user = call.principal.access.user;
    let username = call.principal.username.clone();
    let node = call
        .read(move |c, a| {
            let Some(queue) = repo::play_queue::get(c, user)? else {
                return Ok(None);
            };
            let found = repo::songs::get_many(c, a, &queue.entries)?;
            let songs: Vec<Song> = queue
                .entries
                .iter()
                .filter_map(|id| found.iter().find(|s| s.id == *id).cloned())
                .collect();
            let entries = song_nodes(c, a, &songs)?;
            let current = queue.current.map(|i| i as usize);
            let node = Node::new()
                .attr("position", queue.position_ms)
                .attr("username", username.as_str())
                .attr("changed", time(queue.updated_at))
                .attr("changedBy", queue.changed_by.as_str());
            let node = if by_index {
                node.attr_opt("currentIndex", current)
            } else {
                let id = current
                    .and_then(|i| queue.entries.get(i))
                    .map(|id| id.as_str().to_owned());
                node.attr_opt("current", id)
            };
            Ok(Some(node.many("entry", entries)))
        })
        .await?;
    let element = if by_index {
        "playQueueByIndex"
    } else {
        "playQueue"
    };
    Ok(match node {
        Some(node) => Body::one(element, node),
        None => Body::Empty,
    }
    .into())
}

pub async fn save(call: &Call<'_>, by_index: bool) -> Result<Reply, ApiError> {
    call.require(Scopes::WRITE)?;
    let params = call.params;
    let entries: Vec<SongId> = params
        .all("id")
        .into_iter()
        .map(|raw| ids::parse(raw, "Song"))
        .collect::<Result<_, _>>()?;
    if entries.len() > MAX_QUEUE {
        return Err(ApiError::generic(format!(
            "A play queue holds at most {MAX_QUEUE} songs"
        )));
    }
    let current = if by_index {
        let index: Option<u32> = params.int("currentIndex")?;
        if entries.is_empty() {
            None
        } else {
            let index = index.ok_or_else(|| ApiError::missing("currentIndex"))?;
            if index as usize >= entries.len() {
                return Err(ApiError::invalid("currentIndex"));
            }
            Some(index)
        }
    } else {
        match params.get("current") {
            Some(raw) => {
                let id: SongId = ids::parse(raw, "Song")?;
                let index = entries
                    .iter()
                    .position(|e| *e == id)
                    .ok_or_else(|| ApiError::invalid("current"))?;
                Some(index as u32)
            }
            None => None,
        }
    };
    let position_ms: u64 = params.int("position")?.unwrap_or(0);
    let update = PlayQueueUpdate {
        entries,
        current,
        position_ms,
        changed_by: call.client(),
        device: call.principal.device.as_ref().map(|(key, _)| *key),
    };
    let user = call.principal.access.user;
    call.write(move |tx, a| {
        let found = repo::songs::get_many(tx.conn(), a, &update.entries)?;
        if update
            .entries
            .iter()
            .any(|id| !found.iter().any(|s| s.id == *id))
        {
            return Err(ApiError::not_found("Song"));
        }
        repo::play_queue::put(tx, user, &update)?;
        Ok(())
    })
    .await?;
    Ok(Body::Empty.into())
}

pub async fn bookmarks(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let user = call.principal.access.user;
    let username = call.principal.username.clone();
    let nodes = call
        .read(move |c, a| {
            let marks = repo::bookmarks::list(c, user)?;
            let ids: Vec<SongId> = marks.iter().map(|b| b.song.clone()).collect();
            let songs = repo::songs::get_many(c, a, &ids)?;
            let refs: Vec<&Song> = songs.iter().collect();
            let notes = notes(c, user, &refs, &[], &[])?;
            Ok(marks
                .iter()
                .filter_map(|b| {
                    let song = songs.iter().find(|s| s.id == b.song)?;
                    Some(
                        Node::new()
                            .attr("position", b.position_ms)
                            .attr("username", username.as_str())
                            .attr("comment", b.comment.as_deref().unwrap_or(""))
                            .attr("created", time(b.created_at))
                            .attr("changed", time(b.updated_at))
                            .one("entry", map::song(song, &notes)),
                    )
                })
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(Body::one("bookmarks", Node::new().many("bookmark", nodes)).into())
}

pub async fn create_bookmark(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::WRITE)?;
    let id: SongId = ids::parse(call.params.required("id")?, "Song")?;
    let position: u64 = call
        .params
        .int("position")?
        .ok_or_else(|| ApiError::missing("position"))?;
    let comment = call.params.get("comment").map(str::to_owned);
    let user = call.principal.access.user;
    call.write(move |tx, a| {
        if repo::songs::get(tx.conn(), a, &id)?.is_none() {
            return Err(ApiError::not_found("Song"));
        }
        repo::bookmarks::put(tx, user, &id, position, comment.as_deref())?;
        Ok(())
    })
    .await?;
    Ok(Body::Empty.into())
}

pub async fn delete_bookmark(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::WRITE)?;
    let id: SongId = ids::parse(call.params.required("id")?, "Song")?;
    let user = call.principal.access.user;
    call.write(move |tx, _| Ok(repo::bookmarks::delete(tx, user, &id)?))
        .await?;
    Ok(Body::Empty.into())
}
