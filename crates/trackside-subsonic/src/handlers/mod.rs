//! Endpoint handlers, grouped as the Subsonic documentation groups them.

use rusqlite::Connection;
use trackside_core::records::{Album, Artist, ItemRef, Library, Page, Song};
use trackside_core::repo;
use trackside_core::{Access, DbError, LibraryId, LibraryKey, UserKey};

use crate::error::ApiError;
use crate::ids;
use crate::map::Notes;
use crate::{Call, Reply};

pub mod annotate;
pub mod browse;
pub mod lists;
pub mod media;
pub mod other;
pub mod playlists;
pub mod queue;
pub mod search;
pub mod system;

/// The largest page any list endpoint returns.
pub const MAX_PAGE: u32 = 500;

/// Whether `name` (lowercase, without `.view`) is an endpoint this server answers.
pub fn is_known(name: &str) -> bool {
    matches!(
        name,
        "ping"
            | "getlicense"
            | "getopensubsonicextensions"
            | "tokeninfo"
            | "getmusicfolders"
            | "getindexes"
            | "getmusicdirectory"
            | "getgenres"
            | "getartists"
            | "getartist"
            | "getalbum"
            | "getsong"
            | "getartistinfo"
            | "getartistinfo2"
            | "getalbuminfo"
            | "getalbuminfo2"
            | "getsimilarsongs"
            | "getsimilarsongs2"
            | "gettopsongs"
            | "getalbumlist"
            | "getalbumlist2"
            | "getrandomsongs"
            | "getsongsbygenre"
            | "getnowplaying"
            | "getstarred"
            | "getstarred2"
            | "search"
            | "search2"
            | "search3"
            | "getplaylists"
            | "getplaylist"
            | "createplaylist"
            | "updateplaylist"
            | "deleteplaylist"
            | "stream"
            | "download"
            | "hls"
            | "getcoverart"
            | "getlyrics"
            | "getlyricsbysongid"
            | "getavatar"
            | "star"
            | "unstar"
            | "setrating"
            | "scrobble"
            | "reportplayback"
            | "getplayqueue"
            | "saveplayqueue"
            | "getplayqueuebyindex"
            | "saveplayqueuebyindex"
            | "getbookmarks"
            | "createbookmark"
            | "deletebookmark"
            | "getscanstatus"
            | "startscan"
            | "getuser"
            | "getusers"
    ) || other::is_unsupported(name)
}

/// Runs the endpoint `name` for an authenticated call.
pub async fn handle(name: &str, call: &Call<'_>) -> Result<Reply, ApiError> {
    match name {
        "ping" => Ok(system::ping()),
        "getlicense" => Ok(system::license()),
        "tokeninfo" => Ok(system::token_info(call)),
        "getmusicfolders" => browse::music_folders(call).await,
        "getindexes" => browse::indexes(call).await,
        "getartists" => browse::artists(call).await,
        "getmusicdirectory" => browse::music_directory(call).await,
        "getgenres" => browse::genres(call).await,
        "getartist" => browse::artist(call).await,
        "getalbum" => browse::album(call).await,
        "getsong" => browse::song(call).await,
        "getartistinfo" => browse::artist_info(call, "artistInfo").await,
        "getartistinfo2" => browse::artist_info(call, "artistInfo2").await,
        "getalbuminfo" | "getalbuminfo2" => browse::album_info(call, "albumInfo").await,
        "getsimilarsongs" => browse::similar_songs(call, "similarSongs").await,
        "getsimilarsongs2" => browse::similar_songs(call, "similarSongs2").await,
        "gettopsongs" => browse::top_songs(call).await,
        "getalbumlist" => lists::album_list(call, false).await,
        "getalbumlist2" => lists::album_list(call, true).await,
        "getrandomsongs" => lists::random_songs(call).await,
        "getsongsbygenre" => lists::songs_by_genre(call).await,
        "getnowplaying" => lists::now_playing(call).await,
        "getstarred" => lists::starred(call, "starred").await,
        "getstarred2" => lists::starred(call, "starred2").await,
        "search" => search::search1(call).await,
        "search2" => search::search(call, search::Version::Two).await,
        "search3" => search::search(call, search::Version::Three).await,
        "getplaylists" => playlists::list(call).await,
        "getplaylist" => playlists::get(call).await,
        "createplaylist" => playlists::create(call).await,
        "updateplaylist" => playlists::update(call).await,
        "deleteplaylist" => playlists::delete(call).await,
        "stream" => media::stream(call).await,
        "download" => media::download(call).await,
        "hls" => media::hls(call).await,
        "getcoverart" => media::cover_art(call).await,
        "getlyrics" => media::lyrics(call).await,
        "getlyricsbysongid" => media::lyrics_by_song_id(call).await,
        "star" => annotate::star(call, true).await,
        "unstar" => annotate::star(call, false).await,
        "setrating" => annotate::set_rating(call).await,
        "scrobble" => annotate::scrobble(call).await,
        "reportplayback" => annotate::report_playback(call).await,
        "getplayqueue" => queue::get(call, false).await,
        "getplayqueuebyindex" => queue::get(call, true).await,
        "saveplayqueue" => queue::save(call, false).await,
        "saveplayqueuebyindex" => queue::save(call, true).await,
        "getbookmarks" => queue::bookmarks(call).await,
        "createbookmark" => queue::create_bookmark(call).await,
        "deletebookmark" => queue::delete_bookmark(call).await,
        "getscanstatus" => other::scan_status(call).await,
        "startscan" => other::start_scan(call).await,
        "getuser" => other::user(call).await,
        "getusers" => other::users(call).await,
        unsupported => other::unsupported(unsupported),
    }
}

/// The `musicFolderId` filter as a library key. An unknown or invisible folder is error 70.
pub fn library_filter(
    c: &Connection,
    a: &Access,
    music_folder: Option<&str>,
) -> Result<Option<LibraryKey>, ApiError> {
    Ok(selected_library(c, a, music_folder)?.map(|l| l.key))
}

/// The library `musicFolderId` names.
pub fn selected_library(
    c: &Connection,
    a: &Access,
    music_folder: Option<&str>,
) -> Result<Option<Library>, ApiError> {
    let Some(raw) = music_folder else {
        return Ok(None);
    };
    let id: LibraryId = ids::parse(raw, "Music folder")?;
    match repo::libraries::get(c, a, &id)? {
        Some(library) => Ok(Some(library)),
        None => Err(ApiError::not_found("Music folder")),
    }
}

/// The caller's annotations for everything in a response, in one query.
pub fn notes(
    c: &Connection,
    user: UserKey,
    songs: &[&Song],
    albums: &[&Album],
    artists: &[&Artist],
) -> Result<Notes, ApiError> {
    let items: Vec<ItemRef> = songs
        .iter()
        .map(|s| ItemRef::Song(s.id.clone()))
        .chain(albums.iter().map(|a| ItemRef::Album(a.id.clone())))
        .chain(artists.iter().map(|a| ItemRef::Artist(a.id.clone())))
        .collect();
    if items.is_empty() {
        return Ok(Notes::new());
    }
    Ok(repo::annotations::for_items(c, user, &items)?)
}

/// Serves an offset-and-count window from a cursor-paginated listing by walking pages.
pub fn window<T>(
    offset: usize,
    count: u32,
    mut fetch: impl FnMut(Option<&str>, u32) -> Result<Page<T>, DbError>,
) -> Result<Vec<T>, ApiError> {
    let count = count as usize;
    let mut out = Vec::with_capacity(count.min(MAX_PAGE as usize));
    if count == 0 {
        return Ok(out);
    }
    let mut skip = offset;
    let mut cursor: Option<String> = None;
    loop {
        let wanted = (skip + count - out.len()).min(MAX_PAGE as usize).max(1);
        let page = fetch(cursor.as_deref(), wanted as u32)?;
        let got = page.items.len();
        for item in page.items {
            if skip > 0 {
                skip -= 1;
            } else if out.len() < count {
                out.push(item);
            }
        }
        if out.len() >= count || got == 0 {
            break;
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(out)
}

/// One page of a listing paged by internal id, and the id to continue after.
pub type IdPage = Result<(Vec<Song>, Option<i64>), DbError>;

/// Serves an offset-and-count window from a listing paged by internal id. `resume` is the id
/// to continue after when the caller already knows where `offset` starts. Returns the window
/// and the id to continue after it.
pub fn id_window(
    offset: usize,
    count: u32,
    resume: Option<i64>,
    mut fetch: impl FnMut(Option<i64>, u32) -> IdPage,
) -> Result<(Vec<Song>, Option<i64>), ApiError> {
    if count == 0 {
        return Ok((Vec::new(), resume));
    }
    let mut after = resume;
    let mut skip = if resume.is_some() { 0 } else { offset };
    while skip > 0 {
        let (page, next) = fetch(after, (skip as u32).min(MAX_PAGE))?;
        skip = skip.saturating_sub(page.len());
        match next {
            Some(next) if !page.is_empty() => after = Some(next),
            _ => return Ok((Vec::new(), None)),
        }
    }
    Ok(fetch(after, count)?)
}

/// Every item of a cursor-paginated listing.
pub fn all<T>(
    mut fetch: impl FnMut(Option<&str>, u32) -> Result<Page<T>, DbError>,
) -> Result<Vec<T>, ApiError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = fetch(cursor.as_deref(), MAX_PAGE)?;
        let got = page.items.len();
        out.extend(page.items);
        match page.next {
            Some(next) if got > 0 => cursor = Some(next),
            _ => break,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(total: usize) -> impl FnMut(Option<&str>, u32) -> Result<Page<usize>, DbError> {
        move |cursor, limit| {
            let start: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
            let end = (start + limit as usize).min(total);
            Ok(Page {
                items: (start..end).collect(),
                next: (end < total).then(|| end.to_string()),
            })
        }
    }

    #[test]
    fn window_skips_the_offset_across_pages() -> Result<(), ApiError> {
        assert_eq!(window(1200, 3, pages(2000))?, vec![1200, 1201, 1202]);
        assert_eq!(window(0, 2, pages(2000))?, vec![0, 1]);
        assert_eq!(window(1999, 10, pages(2000))?, vec![1999]);
        assert_eq!(window(5000, 10, pages(2000))?, Vec::<usize>::new());
        assert_eq!(window(0, 0, pages(2000))?, Vec::<usize>::new());
        Ok(())
    }

    #[test]
    fn all_reads_to_the_end() -> Result<(), ApiError> {
        assert_eq!(all(pages(1234))?.len(), 1234);
        Ok(())
    }

    #[test]
    fn every_handled_name_is_known() {
        for name in [
            "ping",
            "getcoverart",
            "search3",
            "getshares",
            "getanimatedart",
            "saveplayqueuebyindex",
        ] {
            assert!(is_known(name), "{name}");
        }
        assert!(!is_known("getsonicsimilartracks"));
        assert!(!is_known("gettranscodedecision"));
    }
}
