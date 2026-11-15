//! `search`, `search2` and `search3`. Library items only, with every offset and count pair
//! honoured. An empty query lists everything: artists and albums by name, songs in stable
//! internal-id order, which is how older Tracks builds and other clients page a whole library.

use std::sync::Arc;

use rusqlite::Connection;
use trackside_auth::Scopes;
use trackside_core::records::{Album, Artist, Song};
use trackside_core::repo::albums::{AlbumFilter, AlbumSort};
use trackside_core::search::{SearchKinds, SearchQuery};
use trackside_core::{Access, LibraryKey, repo, search as core_search};

use super::browse::song_nodes;
use super::{MAX_PAGE, id_window, library_filter, notes, window};
use crate::error::ApiError;
use crate::map;
use crate::response::{Body, Node};
use crate::state::Inner;
use crate::{Call, Reply};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    Two,
    Three,
}

/// The text to search for. Clients ask for "everything" with an empty query, `""` or `*`.
pub fn normalize_query(raw: &str) -> String {
    let trimmed = raw.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(trimmed);
    unquoted.trim().trim_end_matches('*').trim().to_owned()
}

/// One kind's offset and count.
#[derive(Clone, Copy, Debug)]
struct Slice {
    offset: usize,
    count: u32,
}

fn slice(call: &Call<'_>, kind: &str) -> Result<Slice, ApiError> {
    Ok(Slice {
        offset: call.params.offset(&format!("{kind}Offset"))?,
        count: call.params.count(&format!("{kind}Count"), 20, MAX_PAGE)?,
    })
}

fn only(songs: bool, albums: bool, artists: bool) -> SearchKinds {
    SearchKinds {
        songs,
        albums,
        artists,
    }
}

/// Every song in internal-id order from `offset`, continuing from where the previous page
/// ended when this caller asked for that page before.
fn all_songs(
    inner: &Inner,
    c: &Connection,
    a: &Access,
    library: Option<LibraryKey>,
    window: Slice,
) -> Result<Vec<Song>, ApiError> {
    if window.count == 0 {
        return Ok(Vec::new());
    }
    let key = |offset| (a.user, a.sync_epoch, library, offset);
    let (page, next) = id_window(
        window.offset,
        window.count,
        inner.cursors.get(key(window.offset)),
        |after, limit| repo::songs::all_paged_in(c, a, library, after, limit),
    )?;
    if let Some(next) = next {
        inner.cursors.put(key(window.offset + page.len()), next);
    }
    Ok(page)
}

struct Found {
    artists: Vec<Artist>,
    albums: Vec<Album>,
    songs: Vec<Song>,
}

fn find(
    inner: &Inner,
    c: &Connection,
    a: &Access,
    text: &str,
    library: Option<LibraryKey>,
    slices: [Slice; 3],
) -> Result<Found, ApiError> {
    let [artist_slice, album_slice, song_slice] = slices;
    if text.is_empty() {
        let filter = AlbumFilter {
            library,
            ..AlbumFilter::default()
        };
        return Ok(Found {
            artists: window(artist_slice.offset, artist_slice.count, |cursor, limit| {
                repo::artists::list(c, a, library, cursor, limit)
            })?,
            albums: window(album_slice.offset, album_slice.count, |cursor, limit| {
                repo::albums::list(c, a, AlbumSort::Alphabetical, cursor, limit, &filter)
            })?,
            songs: all_songs(inner, c, a, library, song_slice)?,
        });
    }
    let query = |kinds: SearchKinds, cursor: Option<&str>, limit: u32| SearchQuery {
        text: text.to_owned(),
        kinds,
        limit,
        cursor: cursor.map(str::to_owned),
        library,
    };
    Ok(Found {
        artists: window(artist_slice.offset, artist_slice.count, |cursor, limit| {
            core_search::search(c, a, &query(only(false, false, true), cursor, limit))
                .map(|r| r.artists)
        })?,
        albums: window(album_slice.offset, album_slice.count, |cursor, limit| {
            core_search::search(c, a, &query(only(false, true, false), cursor, limit))
                .map(|r| r.albums)
        })?,
        songs: window(song_slice.offset, song_slice.count, |cursor, limit| {
            core_search::search(c, a, &query(only(true, false, false), cursor, limit))
                .map(|r| r.songs)
        })?,
    })
}

pub async fn search(call: &Call<'_>, version: Version) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let text = normalize_query(call.params.raw("query").unwrap_or(""));
    let slices = [
        slice(call, "artist")?,
        slice(call, "album")?,
        slice(call, "song")?,
    ];
    let folder = call.params.get("musicFolderId").map(str::to_owned);
    let inner = Arc::clone(call.inner);
    let node = call
        .read(move |c, a| {
            let library = library_filter(c, a, folder.as_deref())?;
            let found = find(&inner, c, a, &text, library, slices)?;
            let songs: Vec<&Song> = found.songs.iter().collect();
            let albums: Vec<&Album> = found.albums.iter().collect();
            let artists: Vec<&Artist> = found.artists.iter().collect();
            let notes = notes(c, a.user, &songs, &albums, &artists)?;
            let album_nodes = found
                .albums
                .iter()
                .map(|al| match version {
                    Version::Three => map::album_id3(al, &notes),
                    Version::Two => map::album_child(al, &notes),
                })
                .collect();
            Ok(Node::new()
                .many(
                    "artist",
                    found
                        .artists
                        .iter()
                        .map(|r| map::artist(r, &notes))
                        .collect(),
                )
                .many("album", album_nodes)
                .many(
                    "song",
                    found.songs.iter().map(|s| map::song(s, &notes)).collect(),
                ))
        })
        .await?;
    let element = match version {
        Version::Two => "searchResult2",
        Version::Three => "searchResult3",
    };
    Ok(Body::one(element, node).into())
}

/// The deprecated `search`: songs matching `any`, `artist`, `album` or `title`.
pub async fn search1(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let params = call.params;
    let text = ["any", "artist", "album", "title"]
        .iter()
        .filter_map(|name| params.get(name))
        .collect::<Vec<_>>()
        .join(" ");
    let text = normalize_query(&text);
    let count = params.count("count", 20, MAX_PAGE)?;
    let offset = params.offset("offset")?;
    let songs = call
        .read(move |c, a| {
            if text.is_empty() {
                return Ok(Vec::new());
            }
            let songs = window(offset, count, |cursor, limit| {
                core_search::search(
                    c,
                    a,
                    &SearchQuery {
                        text: text.clone(),
                        kinds: only(true, false, false),
                        limit,
                        cursor: cursor.map(str::to_owned),
                        library: None,
                    },
                )
                .map(|r| r.songs)
            })?;
            song_nodes(c, a, &songs)
        })
        .await?;
    let hits = offset + songs.len();
    Ok(Body::one(
        "searchResult",
        Node::new()
            .attr("offset", offset)
            .attr("totalHits", hits)
            .many("match", songs),
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_queries_normalize_to_empty() {
        for raw in ["", "  ", "\"\"", "*", "\"*\""] {
            assert_eq!(normalize_query(raw), "", "{raw:?}");
        }
        assert_eq!(normalize_query(" halo* "), "halo");
        assert_eq!(normalize_query("\"AC/DC\""), "AC/DC");
    }
}
