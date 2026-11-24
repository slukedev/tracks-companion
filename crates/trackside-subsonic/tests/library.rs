//! Endpoints against a seeded library over real HTTP: the requests Tracks' `SubsonicClient`
//! sends today, shapes other clients send, access control, and play and star semantics.
//!
//! Seeded through the real repositories, ingest, auth and media, with a fake transcoder.

use std::fmt::Write as _;

mod support;

use support::{Fixture, TestResult, assert_tracks_song, ids_in, pid};
use trackside_core::{Access, repo};
use trackside_media::Profile;
use trackside_media::testing::FakeTranscoder;

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one request after another, as Tracks makes them"
)]
async fn tracks_requests_get_the_fields_tracks_decodes() -> TestResult {
    let fx = Fixture::start().await?;
    fx.ok(&fx.tracks_url("ping", &[], true)).await?;

    let artists = fx.ok(&fx.tracks_url("getArtists", &[], true)).await?;
    let index = artists["artists"]["index"].as_array().ok_or("no index")?;
    let all: Vec<&serde_json::Value> = index
        .iter()
        .flat_map(|i| i["artist"].as_array().into_iter().flatten())
        .collect();
    assert!(
        all.iter()
            .all(|a| a["id"].is_string() && a["name"].is_string() && a["albumCount"].is_i64())
    );
    let names: Vec<&str> = all.iter().filter_map(|a| a["name"].as_str()).collect();
    assert!(names.contains(&"Beyoncé") && !names.contains(&"Hidden Artist"));

    let artist_id = fx.halo.artist.as_ref().ok_or("no artist")?;
    let artist = fx
        .ok(&fx.tracks_url("getArtist", &[("id", pid(artist_id))], true))
        .await?;
    assert_eq!(artist["artist"]["name"], "Beyoncé");
    assert!(
        artist["artist"]["album"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );

    let album_id = fx.halo.album.as_ref().ok_or("no album")?;
    let album = fx
        .ok(&fx.tracks_url("getAlbum", &[("id", pid(album_id))], true))
        .await?;
    let songs = album["album"]["song"].as_array().ok_or("no songs")?;
    assert_eq!(songs.len(), 2);
    songs.iter().for_each(assert_tracks_song);
    assert!(album["album"]["songCount"].is_i64() && album["album"]["created"].is_string());

    for kind in [
        "newest",
        "frequent",
        "random",
        "alphabeticalByName",
        "recent",
        "highest",
        "starred",
    ] {
        let list = fx
            .ok(&fx.tracks_url(
                "getAlbumList2",
                &[("type", kind), ("size", "15"), ("offset", "0")],
                true,
            ))
            .await?;
        assert!(list["albumList2"]["album"].is_array(), "{kind}");
    }

    let page = |offset: &'static str| {
        fx.tracks_url(
            "search3",
            &[
                ("query", ""),
                ("artistCount", "0"),
                ("albumCount", "0"),
                ("songCount", "2"),
                ("songOffset", offset),
            ],
            true,
        )
    };
    let first = fx.ok(&page("0")).await?;
    let second = fx.ok(&page("2")).await?;
    let mut paged = ids_in(&first["searchResult3"]["song"]);
    paged.extend(ids_in(&second["searchResult3"]["song"]));
    paged.sort();
    let mut expected = vec![
        fx.halo.id.to_string(),
        fx.boy.id.to_string(),
        fx.dog_days.id.to_string(),
    ];
    expected.sort();
    assert_eq!(
        paged, expected,
        "empty search3 pages every visible song once"
    );

    let quick = fx
        .ok(&fx.tracks_url(
            "search3",
            &[
                ("query", "halo"),
                ("artistCount", "10"),
                ("albumCount", "20"),
                ("songCount", "12"),
                ("songOffset", "0"),
            ],
            true,
        ))
        .await?;
    assert_eq!(
        ids_in(&quick["searchResult3"]["song"]),
        vec![fx.halo.id.to_string()]
    );
    quick["searchResult3"]["song"]
        .as_array()
        .into_iter()
        .flatten()
        .for_each(assert_tracks_song);

    let similar = fx
        .ok(&fx.tracks_url(
            "getSimilarSongs2",
            &[("id", pid(&fx.halo.id)), ("count", "15")],
            true,
        ))
        .await?;
    assert!(similar["similarSongs2"]["song"].is_array());
    let top = fx
        .ok(&fx.tracks_url(
            "getTopSongs",
            &[("artist", "Beyoncé"), ("count", "20")],
            true,
        ))
        .await?;
    assert_ne!(ids_in(&top["topSongs"]["song"]), Vec::<String>::new());
    let info = fx
        .ok(&fx.tracks_url(
            "getArtistInfo2",
            &[("id", pid(artist_id)), ("count", "15")],
            true,
        ))
        .await?;
    assert!(info["artistInfo2"]["similarArtist"].is_array());
    let random = fx
        .ok(&fx.tracks_url("getRandomSongs", &[("size", "40")], true))
        .await?;
    assert!(!ids_in(&random["randomSongs"]["song"]).contains(&fx.secret.id.to_string()));

    fx.ok(&fx.tracks_url("star", &[("id", pid(&fx.halo.id))], true))
        .await?;
    fx.ok(&fx.tracks_url("unstar", &[("id", pid(&fx.halo.id))], true))
        .await?;
    Ok(())
}

#[tokio::test]
async fn plus_sign_in_query_is_decoded() -> TestResult {
    let fx = Fixture::start().await?;
    let found = fx
        .ok(&fx.tracks_url(
            "search3",
            &[
                ("query", "Florence + the Machine"),
                ("artistCount", "6"),
                ("albumCount", "12"),
                ("songCount", "0"),
            ],
            true,
        ))
        .await?;
    let artists = found["searchResult3"]["artist"]
        .as_array()
        .ok_or("no artists")?;
    assert!(
        artists
            .iter()
            .any(|a| a["name"] == "Florence + the Machine")
    );
    Ok(())
}

#[tokio::test]
async fn stream_and_download_serve_the_original_without_counting_a_play() -> TestResult {
    let fx = Fixture::start().await?;
    for params in [
        vec![("id", pid(&fx.halo.id)), ("format", "raw")],
        vec![
            ("id", pid(&fx.halo.id)),
            ("format", "mp3"),
            ("maxBitRate", "320"),
        ],
    ] {
        let reply = fx.get(&fx.tracks_url("stream", &params, false)).await?;
        assert_eq!(reply.status, 200);
        assert_eq!(
            reply.body, fx.halo.bytes,
            "without ffmpeg the original is served"
        );
    }
    let download = fx
        .get(&fx.tracks_url("download", &[("id", pid(&fx.halo.id))], false))
        .await?;
    assert_eq!(download.status, 200);
    assert!(
        download
            .header("content-disposition")
            .is_some_and(|d| d.contains("attachment"))
    );
    assert_eq!(fx.count("SELECT count(*) FROM listens").await?, 0);
    assert_eq!(fx.count("SELECT count(*) FROM play_stats").await?, 0);
    let song = fx
        .ok(&fx.tracks_url("getSong", &[("id", pid(&fx.halo.id))], true))
        .await?;
    assert_eq!(song["song"]["playCount"], 0);
    Ok(())
}

#[tokio::test]
async fn cover_art_missing_is_404_with_error_70() -> TestResult {
    let fx = Fixture::start().await?;
    for id in [
        pid(&fx.halo.id),
        "tracks-no-such-cover",
        "ext-album-tracks-no-such-cover",
    ] {
        let reply = fx
            .get(&fx.tracks_url("getCoverArt", &[("id", id), ("size", "600")], false))
            .await?;
        assert_eq!(reply.status, 404, "{id}");
        assert_eq!(reply.error_code(), Some(70), "{id}");
        assert!(
            reply
                .header("content-type")
                .is_some_and(|t| t.starts_with("text/xml"))
        );
    }
    Ok(())
}

#[tokio::test]
async fn members_see_only_their_own_now_playing_and_admins_see_all() -> TestResult {
    let fx = Fixture::start().await?;
    let id = pid(&fx.halo.id);
    let now_playing = |key: &str| fx.key_url(key, "getNowPlaying", &[]);
    fx.ok(&fx.key_url(
        &fx.admin_key,
        "scrobble",
        &[("id", id), ("submission", "false")],
    ))
    .await?;
    let member = fx.ok(&now_playing(&fx.member_key)).await?;
    assert!(ids_in(&member["nowPlaying"]["entry"]).is_empty());
    fx.ok(&fx.tracks_url("scrobble", &[("id", id), ("submission", "false")], true))
        .await?;
    let member = fx.ok(&now_playing(&fx.member_key)).await?;
    assert_eq!(ids_in(&member["nowPlaying"]["entry"]), vec![id.to_owned()]);
    let admin = fx.ok(&now_playing(&fx.admin_key)).await?;
    assert_eq!(ids_in(&admin["nowPlaying"]["entry"]).len(), 2);
    Ok(())
}

#[tokio::test]
async fn scrobble_submission_counts_once_per_time() -> TestResult {
    let fx = Fixture::start().await?;
    let id = pid(&fx.halo.id);
    let scrobble = |time: &'static str, submission: &'static str| {
        fx.tracks_url(
            "scrobble",
            &[("id", id), ("time", time), ("submission", submission)],
            true,
        )
    };
    fx.ok(&scrobble("1794600000000", "false")).await?;
    let playing = fx
        .ok(&fx.key_url(&fx.member_key, "getNowPlaying", &[]))
        .await?;
    assert_eq!(ids_in(&playing["nowPlaying"]["entry"]), vec![id.to_owned()]);
    assert_eq!(fx.count("SELECT count(*) FROM listens").await?, 0);
    fx.ok(&scrobble("1794600000000", "true")).await?;
    fx.ok(&scrobble("1794600000000", "true")).await?;
    let song = fx
        .ok(&fx.tracks_url("getSong", &[("id", id)], true))
        .await?;
    assert_eq!(song["song"]["playCount"], 1);
    fx.ok(&scrobble("1794600300000", "true")).await?;
    let song = fx
        .ok(&fx.tracks_url("getSong", &[("id", id)], true))
        .await?;
    assert_eq!(song["song"]["playCount"], 2);
    assert_eq!(
        fx.count("SELECT count(*) FROM listens WHERE source = 'subsonic'")
            .await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn star_has_no_side_effects() -> TestResult {
    let fx = Fixture::start().await?;
    let before = fx
        .count("SELECT coalesce(max(seq), 0) FROM change_log")
        .await?;
    fx.ok(&fx.tracks_url("star", &[("id", pid(&fx.dog_days.id))], true))
        .await?;
    assert_eq!(fx.count("SELECT count(*) FROM acquisitions").await?, 0);
    let changes = fx
        .server
        .test
        .db
        .read(move |c| {
            let mut statement = c.prepare("SELECT kind FROM change_log WHERE seq > ?1")?;
            let kinds = statement
                .query_map([before], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(kinds)
        })
        .await?;
    assert_eq!(changes, vec![String::from("star")]);
    let starred = fx
        .ok(&fx.key_url(&fx.member_key, "getStarred2", &[]))
        .await?;
    assert_eq!(
        ids_in(&starred["starred2"]["song"]),
        vec![fx.dog_days.id.to_string()]
    );
    Ok(())
}

#[tokio::test]
async fn member_cannot_see_ungranted_library_through_any_endpoint() -> TestResult {
    let fx = Fixture::start().await?;
    let hidden_song = pid(&fx.secret.id);
    let hidden_album = fx
        .secret
        .album
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let hidden_artist = fx
        .secret
        .artist
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let hidden_library = fx.library_b.to_string();
    let hidden = [
        hidden_song.to_owned(),
        hidden_album.clone(),
        hidden_artist.clone(),
        hidden_library.clone(),
    ];
    let key = &fx.member_key;
    let by_id: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("getSong", vec![("id", hidden_song)]),
        ("getAlbum", vec![("id", &hidden_album)]),
        ("getArtist", vec![("id", &hidden_artist)]),
        ("getMusicDirectory", vec![("id", &hidden_album)]),
        ("getMusicDirectory", vec![("id", &hidden_artist)]),
        ("getMusicDirectory", vec![("id", &hidden_library)]),
        ("getArtistInfo2", vec![("id", &hidden_artist)]),
        ("getAlbumInfo2", vec![("id", &hidden_album)]),
        ("getSimilarSongs2", vec![("id", hidden_song)]),
        ("getLyricsBySongId", vec![("id", hidden_song)]),
        ("star", vec![("id", hidden_song)]),
        ("setRating", vec![("id", hidden_song), ("rating", "5")]),
        ("scrobble", vec![("id", hidden_song)]),
        (
            "createBookmark",
            vec![("id", hidden_song), ("position", "1000")],
        ),
        ("savePlayQueue", vec![("id", hidden_song)]),
        ("getIndexes", vec![("musicFolderId", &hidden_library)]),
        (
            "getAlbumList2",
            vec![("type", "newest"), ("musicFolderId", &hidden_library)],
        ),
    ];
    for (endpoint, params) in &by_id {
        let reply = fx.get(&fx.key_url(key, endpoint, params)).await?;
        assert_eq!(
            reply.error_code(),
            Some(70),
            "{endpoint} {params:?}: {}",
            reply.text()
        );
    }
    for endpoint in ["stream", "download", "getCoverArt"] {
        let reply = fx
            .get(&fx.key_url(key, endpoint, &[("id", hidden_song)]))
            .await?;
        assert_eq!(reply.status, 404, "{endpoint}");
    }
    let listings: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("getMusicFolders", vec![]),
        ("getIndexes", vec![]),
        ("getArtists", vec![]),
        (
            "getAlbumList2",
            vec![("type", "alphabeticalByName"), ("size", "500")],
        ),
        ("getAlbumList", vec![("type", "newest")]),
        ("getRandomSongs", vec![("size", "500")]),
        ("getSongsByGenre", vec![("genre", "Ambient")]),
        ("search3", vec![("query", "")]),
        ("search3", vec![("query", "Secret")]),
        ("search2", vec![("query", "Hidden")]),
        ("search", vec![("any", "Secret")]),
        ("getTopSongs", vec![("artist", "Hidden Artist")]),
        ("getStarred2", vec![]),
        ("getStarred", vec![]),
        ("getNowPlaying", vec![]),
        ("getPlayQueue", vec![]),
        ("getBookmarks", vec![]),
        ("getGenres", vec![]),
        ("getScanStatus", vec![]),
        ("getUser", vec![("username", "luke+music@example.com")]),
    ];
    for (endpoint, params) in &listings {
        let reply = fx.get(&fx.key_url(key, endpoint, params)).await?;
        let text = reply.text();
        assert_eq!(reply.envelope()?["status"], "ok", "{endpoint}: {text}");
        for id in &hidden {
            assert!(!text.contains(id.as_str()), "{endpoint} leaks {id}: {text}");
        }
        assert!(
            !text.contains("Hidden Album") && !text.contains("Secret Song"),
            "{endpoint}: {text}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn dsub_style_xml_folder_browsing_and_hex_password() -> TestResult {
    let fx = Fixture::start().await?;
    let hex = fx
        .member_password
        .bytes()
        .fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        });
    let auth = format!(
        "u={}&p=enc:{hex}&v=1.2.0&c=DSub",
        support::encode(&fx.member_name)
    );
    let folders = fx
        .get(&format!("/rest/getMusicFolders.view?{auth}"))
        .await?
        .text();
    assert!(folders.contains(&format!(
        "<musicFolder id=\"{}\" name=\"Music\"/>",
        fx.library_a
    )));
    let indexes = fx
        .get(&format!(
            "/rest/getIndexes.view?{auth}&musicFolderId={}",
            fx.library_a
        ))
        .await?
        .text();
    assert!(indexes.contains("<index name=\"B\">"), "{indexes}");
    let root = fx
        .get(&format!(
            "/rest/getMusicDirectory.view?{auth}&id={}",
            fx.library_a
        ))
        .await?
        .text();
    assert!(root.contains("isDir=\"true\""), "{root}");
    let stream = fx
        .get(&format!(
            "/rest/stream.view?{auth}&id={}&maxBitRate=0",
            fx.halo.id
        ))
        .await?;
    assert_eq!(stream.status, 200);
    assert_eq!(stream.body, fx.halo.bytes);
    let plain = fx
        .get(&format!(
            "/rest/ping.view?u={}&p={}&v=1.2.0&c=DSub",
            support::encode(&fx.member_name),
            support::encode(&fx.member_password)
        ))
        .await?;
    assert!(plain.text().contains("status=\"ok\""));
    Ok(())
}

#[tokio::test]
async fn symfonium_style_playlists_and_paging() -> TestResult {
    let fx = Fixture::start().await?;
    let key = &fx.member_key;
    let everything = fx
        .ok(&fx.key_url(
            key,
            "search3",
            &[
                ("query", "\"\""),
                ("artistCount", "500"),
                ("albumCount", "500"),
                ("songCount", "500"),
            ],
        ))
        .await?;
    assert_eq!(ids_in(&everything["searchResult3"]["song"]).len(), 3);
    let created = fx
        .ok(&fx.key_url(
            key,
            "createPlaylist",
            &[
                ("name", "Late Drive"),
                ("songId", pid(&fx.halo.id)),
                ("songId", pid(&fx.boy.id)),
                ("songId", pid(&fx.secret.id)),
            ],
        ))
        .await?;
    let playlist = &created["playlist"];
    assert_eq!(
        ids_in(&playlist["entry"]),
        vec![fx.halo.id.to_string(), fx.boy.id.to_string()]
    );
    assert_eq!(playlist["owner"], fx.member_name.as_str());
    let id = playlist["id"].as_str().ok_or("no id")?.to_owned();
    fx.ok(&fx.key_url(
        key,
        "updatePlaylist",
        &[
            ("playlistId", &id),
            ("songIndexToRemove", "0"),
            ("songIdToAdd", pid(&fx.dog_days.id)),
            ("name", "Later Drive"),
        ],
    ))
    .await?;
    let fetched = fx
        .ok(&fx.key_url(key, "getPlaylist", &[("id", &id)]))
        .await?;
    assert_eq!(fetched["playlist"]["name"], "Later Drive");
    assert_eq!(
        ids_in(&fetched["playlist"]["entry"]),
        vec![fx.boy.id.to_string(), fx.dog_days.id.to_string()]
    );
    let lists = fx.ok(&fx.key_url(key, "getPlaylists", &[])).await?;
    assert_eq!(ids_in(&lists["playlists"]["playlist"]), vec![id.clone()]);
    fx.ok(&fx.key_url(key, "deletePlaylist", &[("id", &id)]))
        .await?;
    let missing = fx
        .get(&fx.key_url(key, "getPlaylist", &[("id", &id)]))
        .await?;
    assert_eq!(missing.error_code(), Some(70));
    Ok(())
}

#[tokio::test]
async fn amperfy_style_form_post_play_queue() -> TestResult {
    let fx = Fixture::start().await?;
    let auth = format!("apiKey={}&v=1.16.1&c=Amperfy&f=json", fx.member_key);
    let save = fx
        .server
        .post_form(
            "/rest/savePlayQueue.view",
            &format!(
                "{auth}&id={}&id={}&current={}&position=81000",
                fx.halo.id, fx.boy.id, fx.boy.id
            ),
        )
        .await?;
    assert_eq!(save.envelope()?["status"], "ok");
    let queue = fx
        .server
        .post_form("/rest/getPlayQueue.view", &auth)
        .await?
        .envelope()?;
    assert_eq!(queue["playQueue"]["current"], fx.boy.id.to_string());
    assert_eq!(queue["playQueue"]["position"], 81000);
    assert_eq!(queue["playQueue"]["changedBy"], "Amperfy");
    let by_index = fx
        .ok(&fx.key_url(&fx.member_key, "getPlayQueueByIndex", &[]))
        .await?;
    assert_eq!(by_index["playQueueByIndex"]["currentIndex"], 1);
    let by_get = fx
        .get(&format!(
            "/rest/getAlbum.view?{auth}&id={}",
            fx.halo.album.as_ref().ok_or("no album")?
        ))
        .await?;
    let by_post = fx
        .server
        .post_form(
            "/rest/getAlbum.view",
            &format!("{auth}&id={}", fx.halo.album.as_ref().ok_or("no album")?),
        )
        .await?;
    assert_eq!(by_get.body, by_post.body);
    Ok(())
}

#[tokio::test]
async fn feishin_style_playback_reports_count_a_finished_play() -> TestResult {
    let fx = Fixture::start().await?;
    let key = &fx.member_key;
    let info = fx.ok(&fx.key_url(key, "tokenInfo", &[])).await?;
    assert_eq!(info["tokenInfo"]["username"], fx.member_name.as_str());
    let id = pid(&fx.boy.id);
    let report = |state: &'static str, position: &'static str| {
        fx.key_url(
            key,
            "reportPlayback",
            &[
                ("mediaId", id),
                ("mediaType", "song"),
                ("positionMs", position),
                ("state", state),
            ],
        )
    };
    fx.ok(&report("starting", "0")).await?;
    fx.ok(&report("playing", "0")).await?;
    fx.server
        .test
        .clock
        .advance(std::time::Duration::from_secs(150));
    fx.ok(&report("stopped", "150000")).await?;
    assert_eq!(
        fx.count("SELECT count(*) FROM listens WHERE source = 'report'")
            .await?,
        1
    );
    let lyrics = fx
        .ok(&fx.key_url(key, "getLyricsBySongId", &[("id", id)]))
        .await?;
    assert!(lyrics["lyricsList"]["structuredLyrics"].is_array());
    Ok(())
}

#[tokio::test]
async fn admin_only_operations_are_error_50_for_members() -> TestResult {
    let fx = Fixture::start().await?;
    let member = fx
        .get(&fx.key_url(&fx.member_key, "startScan", &[]))
        .await?;
    assert_eq!(member.error_code(), Some(50));
    assert_eq!(fx.server.scan.requested_scans().len(), 0);
    let other = fx
        .get(&fx.key_url(&fx.member_key, "getUser", &[("username", "admin")]))
        .await?;
    assert_eq!(other.error_code(), Some(50));
    let admin = fx.ok(&fx.key_url(&fx.admin_key, "startScan", &[])).await?;
    assert_eq!(admin["scanStatus"]["scanning"], true);
    assert_eq!(fx.server.scan.requested_scans().len(), 2);
    let users = fx.ok(&fx.key_url(&fx.admin_key, "getUsers", &[])).await?;
    assert_eq!(users["users"]["user"].as_array().map(Vec::len), Some(2));
    for endpoint in ["createUser", "changePassword", "deleteUser"] {
        let reply = fx
            .get(&fx.key_url(&fx.admin_key, endpoint, &[("username", "x")]))
            .await?;
        assert_eq!(reply.error_code(), Some(50), "{endpoint}");
    }
    Ok(())
}

#[tokio::test]
async fn mp3_transcodes_stream_then_serve_ranges_from_the_cache() -> TestResult {
    let fx = Fixture::start_with(FakeTranscoder::new()).await?;
    let id = fx.halo.id.clone();
    let url = fx.tracks_url(
        "stream",
        &[("id", pid(&id)), ("format", "mp3"), ("maxBitRate", "320")],
        false,
    );
    let first = fx.get(&url).await?;
    assert_eq!(first.status, 200);
    assert_ne!(first.body, fx.halo.bytes, "a transcode, not the original");
    assert_eq!(first.header("content-type"), Some("audio/mpeg"));
    let file = fx
        .server
        .test
        .db
        .read(move |c| repo::songs::file(c, &Access::system(), &id))
        .await?
        .ok_or("no file")?;
    let media = std::sync::Arc::clone(&fx.server.media);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while media.cached(&file, Profile::Mp3_320).await.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let ranged = support::send(fx.server.addr, "GET", &url, &[("Range", "bytes=0-1")], &[]).await?;
    assert_eq!(ranged.status, 206);
    assert_eq!(ranged.body, first.body[..2].to_vec());
    let aac = fx
        .get(&fx.tracks_url(
            "stream",
            &[("id", pid(&fx.halo.id)), ("format", "aac")],
            false,
        ))
        .await?;
    assert_eq!(aac.status, 200);
    assert_eq!(fx.count("SELECT count(*) FROM listens").await?, 0);
    Ok(())
}
