//! Playlists: the caller's own and everyone's public ones. Only owners change them.

use rusqlite::Connection;
use trackside_auth::Scopes;
use trackside_core::records::Song;
use trackside_core::repo::playlists::{NewPlaylist, PlaylistPatch};
use trackside_core::{Access, PlaylistId, PublicId, SongId, repo};

use super::browse::song_nodes;
use crate::error::ApiError;
use crate::ids;
use crate::map;
use crate::response::{Body, Node};
use crate::{Call, Reply};

pub async fn list(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let nodes = call
        .read(move |c, a| {
            Ok(repo::playlists::list(c, a)?
                .iter()
                .map(|p| map::playlist(p, &p.owner_username, None))
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(Body::one("playlists", Node::new().many("playlist", nodes)).into())
}

/// A playlist with its visible entries, as `getPlaylist` and `createPlaylist` answer.
fn playlist_node(c: &Connection, a: &Access, id: &PlaylistId) -> Result<Node, ApiError> {
    let (playlist, entries) =
        repo::playlists::get(c, a, id)?.ok_or_else(|| ApiError::not_found("Playlist"))?;
    let ids: Vec<SongId> = entries.into_iter().map(|e| e.song).collect();
    let found = repo::songs::get_many(c, a, &ids)?;
    let songs: Vec<Song> = ids
        .iter()
        .filter_map(|id| found.iter().find(|s| s.id == *id).cloned())
        .collect();
    let cover = songs
        .iter()
        .find_map(|s| s.artwork.as_ref().map(|c| c.as_str().to_owned()));
    let nodes = song_nodes(c, a, &songs)?;
    Ok(map::playlist(&playlist, &playlist.owner_username, cover.as_deref()).many("entry", nodes))
}

pub async fn get(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let id: PlaylistId = ids::parse(call.params.required("id")?, "Playlist")?;
    let node = call.read(move |c, a| playlist_node(c, a, &id)).await?;
    Ok(Body::one("playlist", node).into())
}

/// Song ids from the request that the caller can see, in order. Others are dropped.
fn visible_songs(c: &Connection, a: &Access, raw: &[String]) -> Result<Vec<SongId>, ApiError> {
    let parsed: Vec<SongId> = raw
        .iter()
        .filter_map(|s| ids::parse::<SongId>(s, "Song").ok())
        .collect();
    let found = repo::songs::get_many(c, a, &parsed)?;
    Ok(parsed
        .into_iter()
        .filter(|id| found.iter().any(|s| s.id == *id))
        .collect())
}

fn owned(values: Vec<&str>) -> Vec<String> {
    values.into_iter().map(str::to_owned).collect()
}

pub async fn create(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::WRITE)?;
    let existing = call
        .params
        .get("playlistId")
        .map(|raw| ids::parse::<PlaylistId>(raw, "Playlist"))
        .transpose()?;
    let name = match existing {
        Some(_) => call.params.get("name").map(str::to_owned),
        None => Some(call.params.required("name")?.to_owned()),
    };
    let songs = owned(call.params.all("songId"));
    let user = call.principal.access.user;
    let id = call
        .write(move |tx, a| {
            let entries = visible_songs(tx.conn(), a, &songs)?;
            match existing {
                Some(id) => {
                    if name.is_some() {
                        let patch = PlaylistPatch {
                            name,
                            ..PlaylistPatch::default()
                        };
                        repo::playlists::update(tx, user, &id, &patch)?;
                    }
                    repo::playlists::replace_entries(tx, user, &id, None, &entries)?;
                    Ok(id)
                }
                None => {
                    let new = NewPlaylist {
                        name: name.unwrap_or_default(),
                        comment: None,
                        is_public: false,
                        entries,
                    };
                    Ok(repo::playlists::create(tx, user, &new)?.id)
                }
            }
        })
        .await?;
    let node = call.read(move |c, a| playlist_node(c, a, &id)).await?;
    Ok(Body::one("playlist", node).into())
}

pub async fn update(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::WRITE)?;
    let params = call.params;
    let id: PlaylistId = ids::parse(params.required("playlistId")?, "Playlist")?;
    let patch = PlaylistPatch {
        name: params.get("name").map(str::to_owned),
        comment: params.raw("comment").map(str::to_owned),
        is_public: params.bool("public")?,
    };
    let add = owned(params.all("songIdToAdd"));
    let mut remove: Vec<usize> = params.ints("songIndexToRemove")?;
    remove.sort_unstable();
    remove.dedup();
    let user = call.principal.access.user;
    call.write(move |tx, a| {
        if patch != PlaylistPatch::default() {
            repo::playlists::update(tx, user, &id, &patch)?;
        }
        let add = visible_songs(tx.conn(), a, &add)?;
        if remove.is_empty() {
            if !add.is_empty() {
                repo::playlists::append(tx, user, &id, &add)?;
            }
            return Ok(());
        }
        let (playlist, entries) = repo::playlists::get(tx.conn(), a, &id)?
            .ok_or_else(|| ApiError::not_found("Playlist"))?;
        let mut songs: Vec<SongId> = entries
            .into_iter()
            .enumerate()
            .filter(|(i, _)| remove.binary_search(i).is_err())
            .map(|(_, e)| e.song)
            .collect();
        songs.extend(add);
        repo::playlists::replace_entries(tx, user, &id, Some(playlist.revision), &songs)?;
        Ok(())
    })
    .await?;
    Ok(Body::Empty.into())
}

pub async fn delete(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::WRITE)?;
    let id: PlaylistId = ids::parse(call.params.required("id")?, "Playlist")?;
    let user = call.principal.access.user;
    call.write(move |tx, _| Ok(repo::playlists::delete(tx, user, &id)?))
        .await?;
    Ok(Body::Empty.into())
}
