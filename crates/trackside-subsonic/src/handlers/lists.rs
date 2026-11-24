//! Lists: album lists, random songs, songs by genre, now playing and starred items.

use trackside_auth::Scopes;
use trackside_core::records::{Album, Artist, ItemKind, ItemRef, Song};
use trackside_core::repo::albums::{AlbumFilter, AlbumSort};
use trackside_core::repo::songs::SongFilter;
use trackside_core::{LibraryId, SongId, repo};

use super::browse::song_nodes;
use super::{MAX_PAGE, all, id_window, library_filter, notes, selected_library, window};
use crate::error::ApiError;
use crate::map;
use crate::response::{Body, Node};
use crate::state::random_seed;
use crate::{Call, Reply};

/// The `type` of `getAlbumList(2)` as a sort order.
pub fn album_sort(kind: &str) -> Result<AlbumSort, ApiError> {
    Ok(match kind {
        "random" => AlbumSort::Random {
            seed: random_seed(),
        },
        "newest" => AlbumSort::Newest,
        "highest" => AlbumSort::Highest,
        "frequent" => AlbumSort::Frequent,
        "recent" => AlbumSort::Recent,
        "alphabeticalByName" | "byGenre" => AlbumSort::Alphabetical,
        "alphabeticalByArtist" => AlbumSort::AlphabeticalByArtist,
        "starred" => AlbumSort::Starred,
        "byYear" => AlbumSort::Year,
        _ => return Err(ApiError::invalid("type")),
    })
}

pub async fn album_list(call: &Call<'_>, id3: bool) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let params = call.params;
    let kind = params.required("type")?;
    let sort = album_sort(kind)?;
    let size = params.count("size", 10, MAX_PAGE)?;
    let offset = params.offset("offset")?;
    let mut filter = AlbumFilter::default();
    if kind == "byYear" {
        let from: i32 = params
            .int("fromYear")?
            .ok_or_else(|| ApiError::missing("fromYear"))?;
        let to: i32 = params
            .int("toYear")?
            .ok_or_else(|| ApiError::missing("toYear"))?;
        filter.year_from = Some(from.min(to));
        filter.year_to = Some(from.max(to));
    }
    if kind == "byGenre" {
        filter.genre = Some(params.required("genre")?.to_owned());
    }
    let folder = params.get("musicFolderId").map(str::to_owned);
    let nodes = call
        .read(move |c, a| {
            filter.library = library_filter(c, a, folder.as_deref())?;
            let albums = if matches!(sort, AlbumSort::Random { .. }) {
                repo::albums::list(c, a, sort, None, size, &filter)?.items
            } else {
                window(offset, size, |cursor, limit| {
                    repo::albums::list(c, a, sort, cursor, limit, &filter)
                })?
            };
            let refs: Vec<&Album> = albums.iter().collect();
            let notes = notes(c, a.user, &[], &refs, &[])?;
            Ok(albums
                .iter()
                .map(|al| {
                    if id3 {
                        map::album_id3(al, &notes)
                    } else {
                        map::album_child(al, &notes)
                    }
                })
                .collect::<Vec<_>>())
        })
        .await?;
    let element = if id3 { "albumList2" } else { "albumList" };
    Ok(Body::one(element, Node::new().many("album", nodes)).into())
}

pub async fn random_songs(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let params = call.params;
    let size = params.count("size", 10, MAX_PAGE)?;
    let mut filter = SongFilter {
        genre: params.get("genre").map(tracks_model::folded),
        year_from: params.int("fromYear")?,
        year_to: params.int("toYear")?,
        library: None,
    };
    let folder = params.get("musicFolderId").map(str::to_owned);
    let songs = call
        .read(move |c, a| {
            filter.library = library_filter(c, a, folder.as_deref())?;
            let songs = repo::songs::random(c, a, &filter, size)?;
            song_nodes(c, a, &songs)
        })
        .await?;
    Ok(Body::one("randomSongs", Node::new().many("song", songs)).into())
}

/// Songs carrying `genre` (compared folded), in stable internal order. The repository has no
/// genre listing, so this walks the library; see the package report.
pub async fn songs_by_genre(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let params = call.params;
    let genre = params.required("genre")?.to_owned();
    let count = params.count("count", 10, MAX_PAGE)?;
    let skip = params.offset("offset")?;
    let folder = params.get("musicFolderId").map(str::to_owned);
    let songs = call
        .read(move |c, a| {
            let library = library_filter(c, a, folder.as_deref())?;
            let (out, _) = id_window(skip, count, None, |after, limit| {
                repo::songs::by_genre(c, a, &genre, library, after, limit)
            })?;
            song_nodes(c, a, &out)
        })
        .await?;
    Ok(Body::one("songsByGenre", Node::new().many("song", songs)).into())
}

/// What is playing: everyone's for an admin, a member's own otherwise, so members of one server
/// don't see each other's listening. Songs the caller cannot see are left out either way.
pub async fn now_playing(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let now = call.now();
    let me = call.principal.access.user;
    let admin = call.is_admin();
    let mut entries = call.inner.now_playing.list(now);
    entries.retain(|e| admin || e.user == me);
    let ids: Vec<SongId> = entries.iter().map(|e| e.song.clone()).collect();
    let nodes = call
        .read(move |c, a| {
            let songs = repo::songs::get_many(c, a, &ids)?;
            let refs: Vec<&Song> = songs.iter().collect();
            let notes = notes(c, a.user, &refs, &[], &[])?;
            Ok(entries
                .iter()
                .filter_map(|e| {
                    let song = songs.iter().find(|s| s.id == e.song)?;
                    let minutes = now.since(e.updated_at).as_secs() / 60;
                    Some(
                        map::song(song, &notes)
                            .attr("username", e.username.as_str())
                            .attr("minutesAgo", minutes)
                            .attr("playerId", 0)
                            .attr("playerName", e.client.as_str()),
                    )
                })
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(Body::one("nowPlaying", Node::new().many("entry", nodes)).into())
}

pub async fn starred(call: &Call<'_>, element: &'static str) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let id3 = element == "starred2";
    let folder = call.params.get("musicFolderId").map(str::to_owned);
    let node = call
        .read(move |c, a| {
            let library: Option<LibraryId> =
                selected_library(c, a, folder.as_deref())?.map(|l| l.id);
            let mut song_ids = Vec::new();
            let mut album_ids = Vec::new();
            let mut artist_ids = Vec::new();
            for kind in [ItemKind::Song, ItemKind::Album, ItemKind::Artist] {
                let stars = all(|cursor, limit| repo::stars::list(c, a, kind, cursor, limit))?;
                for star in stars {
                    match star.item {
                        ItemRef::Song(id) => song_ids.push(id),
                        ItemRef::Album(id) => album_ids.push(id),
                        ItemRef::Artist(id) => artist_ids.push(id),
                    }
                }
            }
            let in_library = |lib: &LibraryId| library.as_ref().is_none_or(|l| l == lib);
            let songs: Vec<Song> = repo::songs::get_many(c, a, &song_ids)?
                .into_iter()
                .filter(|s| in_library(&s.library))
                .collect();
            let albums: Vec<Album> = repo::albums::get_many(c, a, &album_ids)?
                .into_iter()
                .filter(|al| in_library(&al.library))
                .collect();
            let artists: Vec<Artist> = repo::artists::get_many(c, a, &artist_ids)?;
            let song_refs: Vec<&Song> = songs.iter().collect();
            let album_refs: Vec<&Album> = albums.iter().collect();
            let artist_refs: Vec<&Artist> = artists.iter().collect();
            let notes = notes(c, a.user, &song_refs, &album_refs, &artist_refs)?;
            Ok(Node::new()
                .many(
                    "artist",
                    artists.iter().map(|r| map::artist(r, &notes)).collect(),
                )
                .many(
                    "album",
                    albums
                        .iter()
                        .map(|al| {
                            if id3 {
                                map::album_id3(al, &notes)
                            } else {
                                map::album_child(al, &notes)
                            }
                        })
                        .collect(),
                )
                .many("song", songs.iter().map(|s| map::song(s, &notes)).collect()))
        })
        .await?;
    Ok(Body::one(element, node).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_list_type_maps_to_a_sort() {
        for kind in [
            "random",
            "newest",
            "highest",
            "frequent",
            "recent",
            "alphabeticalByName",
            "alphabeticalByArtist",
            "starred",
            "byYear",
            "byGenre",
        ] {
            assert!(album_sort(kind).is_ok(), "{kind}");
        }
        assert_eq!(album_sort("bogus").map_err(|e| e.code), Err(0));
    }
}
