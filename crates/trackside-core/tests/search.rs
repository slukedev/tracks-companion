//! Library search: Tracks' folding, prefixes and ranking.

use trackside_core::records::Song;
use trackside_core::search::{SearchKinds, SearchQuery, SearchResults, search};
use trackside_core::testing::{TestDb, scanned};
use trackside_core::{Access, DbError};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn library() -> Result<(TestDb, tempfile::TempDir), Box<dyn std::error::Error>> {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = test.add_library("Music", dir.path())?.key;
    let files = [
        ("Don't Stop", "AC/DC", "Live"),
        ("Halo", "Beyoncé", "I Am... Sasha Fierce"),
        ("Halo (Live)", "Beyoncé", "Live at Wembley"),
        ("Halogen Lights", "Someone", "Bright"),
        ("The Boxer", "Simon & Garfunkel", "Bridge"),
        ("Driftwood", "Mara Solís", "Coast"),
    ];
    let scanned_files: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(i, (title, artist, album))| {
            scanned(&format!("{i}.flac"), title, artist, Some(album))
        })
        .collect();
    test.ingest(lib, &scanned_files)?;
    Ok((test, dir))
}

fn find(test: &TestDb, query_text: &str) -> Result<SearchResults, DbError> {
    let query = SearchQuery {
        text: query_text.to_owned(),
        kinds: SearchKinds::ALL,
        limit: 20,
        cursor: None,
        library: None,
    };
    test.read_blocking(move |c| search(c, &Access::system(), &query))
}

fn titles(songs: &[Song]) -> Vec<&str> {
    songs.iter().map(|s| s.title.as_str()).collect()
}

#[test]
fn search_matches_tracks_folding() -> TestResult {
    let (test, _dir) = library()?;
    assert_eq!(titles(&find(&test, "dont")?.songs.items), ["Don't Stop"]);
    assert_eq!(titles(&find(&test, "ac dc")?.songs.items), ["Don't Stop"]);
    assert_eq!(
        titles(&find(&test, "ac dc dont")?.songs.items),
        ["Don't Stop"]
    );
    let artists = find(&test, "simon and garfunkel")?.artists.items;
    assert_eq!(
        artists.first().map(|a| a.name.as_str()),
        Some("Simon & Garfunkel")
    );
    assert_eq!(
        titles(&find(&test, "simon & garfunkel")?.songs.items),
        ["The Boxer"]
    );
    assert_eq!(titles(&find(&test, "BEYONCE halo")?.songs.items).len(), 2);
    let solis = find(&test, "Mara Solís")?.artists.items;
    assert_eq!(solis.first().map(|a| a.name.as_str()), Some("Mara Solís"));
    assert_eq!(find(&test, "mara solis")?.artists.items.len(), 1);
    Ok(())
}

#[test]
fn every_word_matches_as_a_prefix() -> TestResult {
    let (test, _dir) = library()?;
    assert_eq!(titles(&find(&test, "drift")?.songs.items), ["Driftwood"]);
    assert_eq!(
        titles(&find(&test, "bey hal liv")?.songs.items),
        ["Halo (Live)"]
    );
    assert_eq!(find(&test, "zzzz")?.songs.items.len(), 0);
    assert_eq!(find(&test, "!!!")?.songs.items.len(), 0);
    Ok(())
}

#[test]
fn exact_folded_matches_rank_first() -> TestResult {
    let (test, _dir) = library()?;
    let found = find(&test, "halo")?.songs.items;
    assert_eq!(found.first().map(|s| s.title.as_str()), Some("Halo"));
    assert_eq!(found.len(), 3);
    let albums = find(&test, "live")?.albums.items;
    assert_eq!(albums.first().map(|a| a.name.as_str()), Some("Live"));
    Ok(())
}

#[test]
fn a_cursor_pages_one_kind() -> TestResult {
    let (test, _dir) = library()?;
    let query = |cursor: Option<String>| SearchQuery {
        text: String::from("halo"),
        kinds: SearchKinds {
            songs: true,
            albums: false,
            artists: false,
        },
        limit: 2,
        cursor,
        library: None,
    };
    let first_query = query(None);
    let first = test.read_blocking(move |c| search(c, &Access::system(), &first_query))?;
    assert_eq!(first.songs.items.len(), 2);
    assert_eq!(first.albums.items.len(), 0);
    let second_query = query(first.songs.next.clone());
    let second = test.read_blocking(move |c| search(c, &Access::system(), &second_query))?;
    assert_eq!(second.songs.items.len(), 1);
    assert!(second.songs.next.is_none());
    let mut all = titles(&first.songs.items);
    all.extend(titles(&second.songs.items));
    all.sort_unstable();
    assert_eq!(all, ["Halo", "Halo (Live)", "Halogen Lights"]);
    Ok(())
}

/// More matches than are ranked in libraries a member cannot see must not hide the one they
/// can.
#[test]
fn matches_in_hidden_libraries_do_not_crowd_out_visible_ones() -> TestResult {
    let test = TestDb::new()?;
    let (hidden_dir, kids_dir) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let hidden = test.add_library("Big", hidden_dir.path())?.key;
    let kids = test.add_library("Kids", kids_dir.path())?.key;
    let crowd: Vec<_> = (0..2100)
        .map(|i| {
            scanned(
                &format!("{i}.flac"),
                &format!("Lullaby {i}"),
                "Many",
                Some("Crowd"),
            )
        })
        .collect();
    test.ingest(hidden, &crowd)?;
    test.ingest(
        kids,
        &[scanned("k.flac", "Lullaby For Mia", "Nursery", Some("Bed"))],
    )?;
    let user = test.add_user("mia", trackside_core::Role::Member, &[kids])?;
    let access = test.access(user.key)?;
    let query = SearchQuery {
        text: String::from("lullaby"),
        kinds: SearchKinds::ALL,
        limit: 20,
        cursor: None,
        library: None,
    };
    let found = test.read_blocking(move |c| search(c, &access, &query))?;
    assert_eq!(titles(&found.songs.items), ["Lullaby For Mia"]);
    Ok(())
}
