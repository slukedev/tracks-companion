//! Browsing: folders, indexes, directories, artists, albums, songs, genres, info and similar.

use std::collections::HashSet;

use rusqlite::Connection;
use trackside_auth::Scopes;
use trackside_core::enrich::{ON_DEMAND_WAIT, Target};
use trackside_core::records::{Album, Artist, Folder, Song};
use trackside_core::{Access, AlbumId, ArtistId, Millis, PublicId, SongId, repo, similar};

use super::{all, library_filter, notes};
use crate::error::ApiError;
use crate::ids::{self, AnyId};
use crate::map::{self, Notes};
use crate::response::{Body, Node};
use crate::{Call, Reply};

pub async fn music_folders(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let folders = call
        .read(|c, a| {
            Ok(repo::libraries::list(c, a)?
                .into_iter()
                .map(|l| {
                    Node::new()
                        .attr("id", l.id.as_str())
                        .attr("name", l.name.as_str())
                })
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(Body::one("musicFolders", Node::new().many("musicFolder", folders)).into())
}

/// Every visible artist, by sort name.
fn every_artist(
    c: &Connection,
    a: &Access,
    music_folder: Option<&str>,
) -> Result<Vec<Artist>, ApiError> {
    let library = library_filter(c, a, music_folder)?;
    all(|cursor, limit| repo::artists::list(c, a, library, cursor, limit))
}

/// When the visible library last changed, for `getIndexes ifModifiedSince`.
fn last_modified(
    c: &Connection,
    a: &Access,
    music_folder: Option<&str>,
) -> Result<Millis, ApiError> {
    let library = library_filter(c, a, music_folder)?;
    Ok(repo::libraries::list(c, a)?
        .iter()
        .filter(|l| library.is_none_or(|k| k == l.key))
        .map(|l| l.last_scan_at.unwrap_or(l.created_at))
        .max()
        .unwrap_or(Millis(0)))
}

pub async fn indexes(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let folder = call.params.get("musicFolderId").map(str::to_owned);
    let since: Option<i64> = call.params.int("ifModifiedSince")?;
    let user = call.principal.access.user;
    let node = call
        .read(move |c, a| {
            let modified = last_modified(c, a, folder.as_deref())?;
            let node = Node::new()
                .attr("lastModified", modified.0)
                .attr("ignoredArticles", "");
            if since.is_some_and(|s| s >= modified.0) {
                return Ok(node);
            }
            let artists = every_artist(c, a, folder.as_deref())?;
            let refs: Vec<&Artist> = artists.iter().collect();
            let notes = notes(c, user, &[], &[], &refs)?;
            Ok(node.many("index", map::indexes(&artists, &notes)))
        })
        .await?;
    Ok(Body::one("indexes", node).into())
}

pub async fn artists(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let folder = call.params.get("musicFolderId").map(str::to_owned);
    let user = call.principal.access.user;
    let index = call
        .read(move |c, a| {
            let artists = every_artist(c, a, folder.as_deref())?;
            let refs: Vec<&Artist> = artists.iter().collect();
            let notes = notes(c, user, &[], &[], &refs)?;
            Ok(map::indexes(&artists, &notes))
        })
        .await?;
    Ok(Body::one(
        "artists",
        Node::new().attr("ignoredArticles", "").many("index", index),
    )
    .into())
}

fn folder_child(folder: &Folder, parent: &str) -> Node {
    Node::new()
        .attr("id", folder.id.as_str())
        .attr("parent", parent)
        .attr("isDir", true)
        .attr("title", folder.name.as_str())
        .attr("name", folder.name.as_str())
}

fn folder_directory(
    c: &Connection,
    a: &Access,
    folder: &Folder,
    name: &str,
) -> Result<Node, ApiError> {
    let children = repo::folders::children(c, a, &folder.id)?;
    let songs = repo::songs::by_folder(c, a, &folder.id)?;
    let refs: Vec<&Song> = songs.iter().collect();
    let notes = notes(c, a.user, &refs, &[], &[])?;
    let id = folder.id.as_str();
    let mut entries: Vec<Node> = children.iter().map(|f| folder_child(f, id)).collect();
    entries.extend(
        songs
            .iter()
            .map(|s| map::song(s, &notes).attr("parent", id)),
    );
    Ok(Node::new()
        .attr("id", id)
        .attr_opt("parent", folder.parent.as_ref().map(PublicId::as_str))
        .attr("name", name)
        .many("child", entries))
}

pub async fn music_directory(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let id = AnyId::parse(call.params.required("id")?)
        .ok_or_else(|| ApiError::not_found("Directory"))?;
    let node = call
        .read(move |c, a| match id {
            AnyId::Folder(id) => {
                let folder = repo::folders::get(c, a, &id)?
                    .ok_or_else(|| ApiError::not_found("Directory"))?;
                let name = folder.name.clone();
                folder_directory(c, a, &folder, &name)
            }
            AnyId::Library(id) => {
                let library = repo::libraries::get(c, a, &id)?
                    .ok_or_else(|| ApiError::not_found("Directory"))?;
                let root = repo::folders::root(c, a, library.key)?
                    .ok_or_else(|| ApiError::not_found("Directory"))?;
                folder_directory(c, a, &root, &library.name)
            }
            AnyId::Artist(id) => {
                let artist = repo::artists::get(c, a, &id)?
                    .ok_or_else(|| ApiError::not_found("Directory"))?;
                let albums = repo::albums::by_artist(c, a, &id)?;
                let refs: Vec<&Album> = albums.iter().collect();
                let notes = notes(c, a.user, &[], &refs, &[&artist])?;
                let artist_node = map::artist(&artist, &notes);
                Ok(Node::new()
                    .attr("id", artist.id.as_str())
                    .attr("name", artist.name.as_str())
                    .attr_opt("starred", artist_node.get("starred").cloned())
                    .attr_opt("userRating", artist_node.get("userRating").cloned())
                    .many(
                        "child",
                        albums
                            .iter()
                            .map(|al| map::album_child(al, &notes))
                            .collect(),
                    ))
            }
            AnyId::Album(id) => {
                let album = repo::albums::get(c, a, &id)?
                    .ok_or_else(|| ApiError::not_found("Directory"))?;
                let songs = repo::songs::by_album(c, a, &id)?;
                let refs: Vec<&Song> = songs.iter().collect();
                let notes = notes(c, a.user, &refs, &[&album], &[])?;
                let album_node = map::album_child(&album, &notes);
                Ok(Node::new()
                    .attr("id", album.id.as_str())
                    .attr_opt("parent", album.artist_id.as_ref().map(PublicId::as_str))
                    .attr("name", album.name.as_str())
                    .attr_opt("starred", album_node.get("starred").cloned())
                    .attr_opt("userRating", album_node.get("userRating").cloned())
                    .attr_opt("playCount", album_node.get("playCount").cloned())
                    .many(
                        "child",
                        songs.iter().map(|s| map::song(s, &notes)).collect(),
                    ))
            }
            _ => Err(ApiError::not_found("Directory")),
        })
        .await?;
    Ok(Body::one("directory", node).into())
}

pub async fn genres(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let genres = call
        .read(|c, a| {
            Ok(repo::genres::list(c, a)?
                .into_iter()
                .map(|g| {
                    Node::new()
                        .attr("songCount", g.song_count)
                        .attr("albumCount", g.album_count)
                        .text(g.name)
                })
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(Body::one("genres", Node::new().many("genre", genres)).into())
}

pub async fn artist(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let id: ArtistId = ids::parse(call.params.required("id")?, "Artist")?;
    let node = call
        .read(move |c, a| {
            let artist =
                repo::artists::get(c, a, &id)?.ok_or_else(|| ApiError::not_found("Artist"))?;
            let albums = repo::albums::by_artist(c, a, &id)?;
            let refs: Vec<&Album> = albums.iter().collect();
            let notes = notes(c, a.user, &[], &refs, &[&artist])?;
            Ok(map::artist(&artist, &notes).many(
                "album",
                albums.iter().map(|al| map::album_id3(al, &notes)).collect(),
            ))
        })
        .await?;
    Ok(Body::one("artist", node).into())
}

pub async fn album(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let id: AlbumId = ids::parse(call.params.required("id")?, "Album")?;
    let node = call
        .read(move |c, a| {
            let album =
                repo::albums::get(c, a, &id)?.ok_or_else(|| ApiError::not_found("Album"))?;
            let songs = repo::songs::by_album(c, a, &id)?;
            let refs: Vec<&Song> = songs.iter().collect();
            let notes = notes(c, a.user, &refs, &[&album], &[])?;
            Ok(map::album_id3(&album, &notes)
                .many("song", songs.iter().map(|s| map::song(s, &notes)).collect()))
        })
        .await?;
    Ok(Body::one("album", node).into())
}

pub async fn song(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let id: SongId = ids::parse(call.params.required("id")?, "Song")?;
    let node = call
        .read(move |c, a| {
            let song = repo::songs::get(c, a, &id)?.ok_or_else(|| ApiError::not_found("Song"))?;
            let notes = notes(c, a.user, &[&song], &[], &[])?;
            Ok(map::song(&song, &notes))
        })
        .await?;
    Ok(Body::one("song", node).into())
}

/// The artist an id leads to: an artist, an album's artist or a song's lead artist.
fn artist_of(c: &Connection, a: &Access, id: AnyId) -> Result<Artist, ApiError> {
    let artist_id = match id {
        AnyId::Artist(id) => Some(id),
        AnyId::Album(id) => repo::albums::get(c, a, &id)?.and_then(|al| al.artist_id),
        AnyId::Song(id) => repo::songs::get(c, a, &id)?.and_then(|s| s.artist_id),
        _ => None,
    };
    let Some(artist_id) = artist_id else {
        return Err(ApiError::not_found("Artist"));
    };
    repo::artists::get(c, a, &artist_id)?.ok_or_else(|| ApiError::not_found("Artist"))
}

/// Starts the Last.fm lookups a request needs and waits briefly; answers come from what is
/// stored either way.
async fn look_up(call: &Call<'_>, targets: Vec<Target>) {
    if !targets.is_empty() {
        call.state().enrich.ensure(targets, ON_DEMAND_WAIT).await;
    }
}

fn artist_target(artist: &Artist) -> Target {
    Target::Artist {
        identity: artist.identity.clone(),
        name: artist.name.clone(),
    }
}

pub async fn artist_info(call: &Call<'_>, element: &'static str) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let id =
        AnyId::parse(call.params.required("id")?).ok_or_else(|| ApiError::not_found("Artist"))?;
    let count = call.params.count("count", 20, 100)?;
    let artist = call.read(move |c, a| artist_of(c, a, id)).await?;
    look_up(call, vec![artist_target(&artist)]).await;
    let node = call
        .read(move |c, a| {
            let similar: Vec<Artist> =
                similar::similar_artists_in_library(c, a, &artist.id, count)?
                    .into_iter()
                    .filter_map(|s| s.artist)
                    .collect();
            let refs: Vec<&Artist> = similar.iter().collect();
            let notes = notes(c, a.user, &[], &[], &refs)?;
            let mut node = Node::new();
            if let Some(bio) = repo::enrichment::bio(c, &artist.identity)? {
                node = node.text_child("biography", bio.as_str());
            }
            if let Some(mbz) = &artist.mbz_artist {
                node = node.text_child("musicBrainzId", mbz.as_str());
            }
            Ok(node.many(
                "similarArtist",
                similar.iter().map(|s| map::artist(s, &notes)).collect(),
            ))
        })
        .await?;
    Ok(Body::one(element, node).into())
}

pub async fn album_info(call: &Call<'_>, element: &'static str) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let id =
        AnyId::parse(call.params.required("id")?).ok_or_else(|| ApiError::not_found("Album"))?;
    let node = call
        .read(move |c, a| {
            let album_id = match id {
                AnyId::Album(id) => Some(id),
                AnyId::Song(id) => repo::songs::get(c, a, &id)?.and_then(|s| s.album_id),
                _ => None,
            };
            let album = match album_id {
                Some(id) => repo::albums::get(c, a, &id)?,
                None => None,
            }
            .ok_or_else(|| ApiError::not_found("Album"))?;
            let mut node = Node::new();
            if let Some(mbz) = &album.mbz.release {
                node = node.text_child("musicBrainzId", mbz.as_str());
            }
            Ok(node)
        })
        .await?;
    Ok(Body::one(element, node).into())
}

/// Takes from each list in turn, skipping repeats and anything in `exclude`.
fn interleave(lists: Vec<Vec<Song>>, exclude: &HashSet<SongId>, count: usize) -> Vec<Song> {
    let mut seen: HashSet<SongId> = exclude.clone();
    let mut iters: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
    let mut out = Vec::new();
    while out.len() < count && !iters.is_empty() {
        iters.retain_mut(|it| match it.next() {
            Some(song) => {
                if out.len() < count && seen.insert(song.id.clone()) {
                    out.push(song);
                }
                true
            }
            None => false,
        });
    }
    out
}

fn similar_to(c: &Connection, a: &Access, id: AnyId, count: u32) -> Result<Vec<Song>, ApiError> {
    match id {
        AnyId::Song(id) => {
            if repo::songs::get(c, a, &id)?.is_none() {
                return Err(ApiError::not_found("Song"));
            }
            Ok(similar::similar_songs(c, a, &id, count)?)
        }
        AnyId::Album(id) => {
            if repo::albums::get(c, a, &id)?.is_none() {
                return Err(ApiError::not_found("Album"));
            }
            let own = repo::songs::by_album(c, a, &id)?;
            let exclude: HashSet<SongId> = own.iter().map(|s| s.id.clone()).collect();
            let lists = own
                .iter()
                .take(3)
                .map(|seed| similar::similar_songs(c, a, &seed.id, count))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(interleave(lists, &exclude, count as usize))
        }
        AnyId::Artist(id) => {
            if repo::artists::get(c, a, &id)?.is_none() {
                return Err(ApiError::not_found("Artist"));
            }
            let mut artists = vec![id.clone()];
            artists.extend(
                similar::similar_artists_in_library(c, a, &id, 10)?
                    .into_iter()
                    .filter_map(|s| s.artist.map(|ar| ar.id)),
            );
            let per = (count / artists.len() as u32).max(3);
            let lists = artists
                .iter()
                .map(|artist| repo::songs::top_for_artist(c, a, artist, per))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(interleave(lists, &HashSet::new(), count as usize))
        }
        _ => Err(ApiError::not_found("Item")),
    }
}

pub async fn similar_songs(call: &Call<'_>, element: &'static str) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let id =
        AnyId::parse(call.params.required("id")?).ok_or_else(|| ApiError::not_found("Item"))?;
    let count = call.params.count("count", 50, super::MAX_PAGE)?;
    let seed = id.clone();
    let targets = call
        .read(move |c, a| {
            Ok(match seed {
                AnyId::Song(id) => repo::songs::get(c, a, &id)?
                    .map(|song| {
                        let lead = song
                            .credits
                            .first()
                            .map_or(song.artist.as_str(), |c| c.name.as_str());
                        vec![
                            Target::song(&song.title, &song.artist),
                            Target::artist(lead),
                        ]
                    })
                    .unwrap_or_default(),
                AnyId::Artist(id) => repo::artists::get(c, a, &id)?
                    .map(|artist| vec![artist_target(&artist)])
                    .unwrap_or_default(),
                _ => Vec::new(),
            })
        })
        .await?;
    look_up(call, targets).await;
    let songs = call
        .read(move |c, a| {
            let songs = similar_to(c, a, id, count)?;
            song_nodes(c, a, &songs)
        })
        .await?;
    Ok(Body::one(element, Node::new().many("song", songs)).into())
}

/// Songs as `Child` nodes with the caller's annotations.
pub fn song_nodes(c: &Connection, a: &Access, songs: &[Song]) -> Result<Vec<Node>, ApiError> {
    let refs: Vec<&Song> = songs.iter().collect();
    let notes: Notes = notes(c, a.user, &refs, &[], &[])?;
    Ok(songs.iter().map(|s| map::song(s, &notes)).collect())
}

pub async fn top_songs(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let count = call.params.count("count", 50, super::MAX_PAGE)?;
    let by_id = call
        .params
        .get("id")
        .map(|raw| ids::parse::<ArtistId>(raw, "Artist"))
        .transpose()?;
    let name = match by_id {
        Some(_) => None,
        None => Some(call.params.required("artist")?.to_owned()),
    };
    let artist = call
        .read(move |c, a| {
            Ok(match (by_id, name) {
                (Some(id), _) => Some(
                    repo::artists::get(c, a, &id)?.ok_or_else(|| ApiError::not_found("Artist"))?,
                ),
                (None, Some(name)) => repo::artists::by_name(c, a, &name)?,
                (None, None) => None,
            })
        })
        .await?;
    if let Some(artist) = &artist {
        look_up(call, vec![artist_target(artist)]).await;
    }
    let songs = call
        .read(move |c, a| {
            let songs = match artist {
                Some(artist) => repo::songs::top_for_artist(c, a, &artist.id, count)?,
                None => Vec::new(),
            };
            song_nodes(c, a, &songs)
        })
        .await?;
    Ok(Body::one("topSongs", Node::new().many("song", songs)).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::fixtures;
    use trackside_core::AudioDigest;

    fn song(n: u8) -> Song {
        let mut s = fixtures::song();
        s.id = SongId::derive(&AudioDigest([n; 32]));
        s
    }

    #[test]
    fn interleave_alternates_and_dedupes() {
        let lists = vec![
            vec![song(1), song(2), song(3)],
            vec![song(2), song(4)],
            vec![song(5)],
        ];
        let exclude: HashSet<SongId> = [song(5).id].into_iter().collect();
        let ids: Vec<SongId> = interleave(lists, &exclude, 10)
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec![song(1).id, song(2).id, song(4).id, song(3).id]);
        let lists = vec![vec![song(1), song(2), song(3)], vec![song(4)]];
        assert_eq!(interleave(lists, &HashSet::new(), 2).len(), 2);
    }
}
