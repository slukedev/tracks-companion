//! Streaming, downloads, playback decisions, HLS, artwork and Subsonic, the way `AVPlayer`,
//! Tracks' download queue and the current Tracks Subsonic client ask for them.

#![expect(clippy::too_many_lines, reason = "a scenario reads best as one story")]

mod common;

use std::time::{Duration, Instant};

use common::boot;
use md5::{Digest, Md5};
use serde_json::{Value, json};
use trackside_e2e::{Auth, Client, Reply};

async fn ranged(
    client: &Client,
    url: &str,
    key: &Auth,
    range: Option<&str>,
) -> anyhow::Result<Reply> {
    let mut request = client.request(reqwest::Method::GET, &client.api(url), key);
    if let Some(range) = range {
        request = request.header("range", range);
    }
    trackside_e2e::client::send(request).await
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_answer_avplayer_range_probes_and_conditionals() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let halo = env.song("Halo").await?;
    let id = halo["id"].as_str().unwrap_or_default();
    let path = env.music().join(
        env.lib
            .track("Halo")
            .map(|t| t.rel.clone())
            .unwrap_or_default(),
    );
    let original = std::fs::read(&path)?;
    let url = format!("/songs/{id}/stream");

    let full = ranged(c, &url, &env.key, None).await?;
    assert_eq!(full.status, 200);
    assert_eq!(full.body, original, "the original bytes");
    assert_eq!(full.header("accept-ranges"), Some("bytes"));
    assert_eq!(full.header("content-type"), Some("audio/flac"));
    assert_eq!(
        full.header("content-length")
            .and_then(|v| v.parse::<usize>().ok()),
        Some(original.len())
    );
    let etag = full.header("etag").unwrap_or_default().to_owned();
    assert!(etag.starts_with('"'), "a strong ETag: {etag}");

    let probe = ranged(c, &url, &env.key, Some("bytes=0-1")).await?;
    assert_eq!(probe.status, 206);
    assert_eq!(probe.body, &original[..2]);
    assert_eq!(
        probe.header("content-range"),
        Some(format!("bytes 0-1/{}", original.len()).as_str())
    );
    let middle = ranged(c, &url, &env.key, Some("bytes=100-199")).await?;
    assert_eq!(middle.body, &original[100..200]);
    let suffix = ranged(c, &url, &env.key, Some("bytes=-50")).await?;
    assert_eq!(suffix.body, &original[original.len() - 50..]);
    let open = ranged(
        c,
        &url,
        &env.key,
        Some(&format!("bytes={}-", original.len() - 10)),
    )
    .await?;
    assert_eq!(open.body, &original[original.len() - 10..]);
    let past = ranged(
        c,
        &url,
        &env.key,
        Some(&format!("bytes={}-", original.len() + 5)),
    )
    .await?;
    assert_eq!(past.status, 416);
    assert_eq!(
        past.header("content-range"),
        Some(format!("bytes */{}", original.len()).as_str())
    );

    let unchanged = trackside_e2e::client::send(
        c.request(reqwest::Method::GET, &c.api(&url), &env.key)
            .header("if-none-match", &etag),
    )
    .await?;
    assert_eq!(unchanged.status, 304);
    let stale_if_range = trackside_e2e::client::send(
        c.request(reqwest::Method::GET, &c.api(&url), &env.key)
            .header("range", "bytes=0-9")
            .header("if-range", "\"something-else\""),
    )
    .await?;
    assert_eq!(
        stale_if_range.status, 200,
        "a stale If-Range sends the whole file"
    );
    let head =
        trackside_e2e::client::send(c.request(reqwest::Method::HEAD, &c.api(&url), &env.key))
            .await?;
    assert_eq!(head.status, 200);
    assert_eq!(head.body, Vec::<u8>::new());

    let token = c.post("/media-token", &env.key, &json!({})).await?.json();
    let mt = token["token"].as_str().unwrap_or_default();
    let with_token = ranged(c, &format!("{url}?mt={mt}"), &Auth::None, Some("bytes=0-1")).await?;
    assert_eq!(with_token.status, 206, "AVPlayer sends only the URL");
    let bad_token = ranged(c, &format!("{url}?mt=nope"), &Auth::None, None).await?;
    assert_eq!(bad_token.status, 401);
    assert!(full.headers_after < Duration::from_millis(500));

    let art = c
        .get(&format!("/art/{id}?size=64&mt={mt}"), &Auth::None)
        .await?;
    assert_eq!(art.status, 200, "the sidecar cover, resized");
    assert_eq!(art.header("content-type"), Some("image/jpeg"));
    let lyrics = c
        .get(&format!("/songs/{id}/lyrics"), &env.key)
        .await?
        .json();
    assert_eq!(lyrics["lyrics"][0]["kind"], "synced", "{lyrics}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_resume_and_transcoded_downloads_wait_for_the_whole_file() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let song = env.song("Hoppípolla").await?;
    let id = song["id"].as_str().unwrap_or_default();
    let url = format!("/songs/{id}/download");
    let whole = ranged(c, &url, &env.key, None).await?;
    assert_eq!(whole.status, 200);
    let disposition = whole
        .header("content-disposition")
        .unwrap_or_default()
        .to_owned();
    assert!(disposition.starts_with("attachment"), "{disposition}");
    assert!(disposition.contains(".wav"), "{disposition}");
    let half = whole.body.len() / 2;
    let rest = trackside_e2e::client::send(
        c.request(reqwest::Method::GET, &c.api(&url), &env.key)
            .header("range", format!("bytes={half}-"))
            .header("if-range", whole.header("etag").unwrap_or_default()),
    )
    .await?;
    assert_eq!(
        rest.status, 206,
        "a background download resumes where it stopped"
    );
    assert_eq!(rest.body, &whole.body[half..]);

    let started = Instant::now();
    let aac = ranged(c, &format!("{url}?profile=aac-128"), &env.key, None).await?;
    assert_eq!(aac.status, 200, "{}", aac.text());
    let len: usize = aac
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or_default();
    assert_eq!(len, aac.body.len(), "the complete file with its length");
    assert!(
        aac.header("content-disposition")
            .unwrap_or_default()
            .contains(".m4a")
    );
    let (segment, priming) = edit_list(&aac.body).ok_or_else(|| anyhow::anyhow!("no edit list"))?;
    assert_eq!(
        priming, 1024,
        "the edit list skips AAC priming, for gapless playback"
    );
    // ffmpeg 9.1 counts the movie header in the sample rate (44100), earlier versions in 1000.
    let timescale = movie_timescale(&aac.body).ok_or_else(|| anyhow::anyhow!("no movie header"))?;
    let segment_ms = u64::from(segment) * 1000 / u64::from(timescale.max(1));
    assert!(
        (1990..=2010).contains(&segment_ms),
        "the edit ends at the source's end: {segment} at timescale {timescale}"
    );
    eprintln!(
        "aac-128 download of a 2 s song took {:?}",
        started.elapsed()
    );
    Ok(())
}

/// The `mvhd` timescale, the unit of edit list segment durations.
fn movie_timescale(bytes: &[u8]) -> Option<u32> {
    let at = bytes.windows(4).position(|w| w == b"mvhd")?;
    // version 0 has 32-bit creation and modification times, version 1 64-bit ones
    let offset = match bytes.get(at + 4)? {
        0 => 16,
        1 => 24,
        _ => return None,
    };
    Some(u32::from_be_bytes(
        bytes.get(at + offset..at + offset + 4)?.try_into().ok()?,
    ))
}

/// The first `elst` entry: (segment duration in the movie timescale, media time).
fn edit_list(bytes: &[u8]) -> Option<(u32, i32)> {
    let at = bytes.windows(4).position(|w| w == b"elst")?;
    let word = |o: usize| -> Option<[u8; 4]> { bytes.get(at + o..at + o + 4)?.try_into().ok() };
    (bytes.get(at + 4) == Some(&0)).then_some(())?;
    Some((u32::from_be_bytes(word(12)?), i32::from_be_bytes(word(16)?)))
}

fn apple_client(network_max: Option<u32>) -> Value {
    let mut client = json!({
        "direct": [
            {"container": "m4a", "codecs": ["aac", "alac"]},
            {"container": "mp3", "codecs": ["mp3"]},
            {"container": "wav", "codecs": ["pcm"]}
        ],
        "hls": true,
        "preferred_profile": "aac-256",
        "allow_original_fallback": false
    });
    if let Some(max) = network_max {
        client["max_bitrate_kbps"] = json!(max);
    }
    client
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cellular_decision_transcodes_through_hls_then_serves_the_cached_file()
-> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let halo = env.song("Halo").await?;
    let id = halo["id"].as_str().unwrap_or_default();
    let ask = |network: &str, max: Option<u32>, song: &str| json!({"song_id": song, "network": network, "client": apple_client(max)});
    let mp3 = env.song("If I Were a Boy").await?;
    let direct = c
        .post(
            "/playback/decisions",
            &env.key,
            &ask("wifi", None, mp3["id"].as_str().unwrap_or_default()),
        )
        .await?
        .json();
    assert_eq!(direct["mode"], "direct", "{direct}");

    let first = c
        .post(
            "/playback/decisions",
            &env.key,
            &ask("cellular", Some(256), id),
        )
        .await?;
    assert_eq!(first.status, 200, "{}", first.text());
    let decision = first.json();
    assert_eq!(
        decision["mode"], "hls",
        "FLAC is not on the list: {decision}"
    );
    assert_eq!(decision["delivered"]["codec"], "aac");
    let playlist_url = decision["url"].as_str().unwrap_or_default().to_owned();
    let started = Instant::now();
    let playlist = c.get(&playlist_url, &env.key).await?;
    assert_eq!(playlist.status, 200, "{}", playlist.text());
    assert_eq!(
        playlist.header("content-type"),
        Some("application/vnd.apple.mpegurl")
    );
    let text = playlist.text();
    assert!(
        text.contains("#EXT-X-VERSION:7") && text.contains("#EXT-X-MAP:URI=\"init.mp4"),
        "{text}"
    );
    assert!(
        text.contains("#EXT-X-ENDLIST"),
        "a VOD playlist lists every segment: {text}"
    );
    let base = playlist_url
        .rsplit_once('/')
        .map_or("", |(b, _)| b)
        .to_owned();
    let init = c.get(&format!("{base}/init.mp4"), &env.key).await?;
    assert_eq!(init.status, 200);
    let segment_name = text
        .lines()
        .find(|l| l.starts_with("seg-"))
        .map(|l| l.split('?').next().unwrap_or(l).to_owned())
        .unwrap_or_default();
    let segment = c.get(&format!("{base}/{segment_name}"), &env.key).await?;
    assert_eq!(segment.status, 200, "{}", segment.text());
    assert_ne!(segment.body, Vec::<u8>::new());
    eprintln!("HLS first segment after {:?}", started.elapsed());

    let deadline = Instant::now() + Duration::from_secs(30);
    let cached = loop {
        let again = c
            .post(
                "/playback/decisions",
                &env.key,
                &ask("cellular", Some(256), id),
            )
            .await?
            .json();
        if again["mode"] == "file" {
            break again;
        }
        anyhow::ensure!(Instant::now() < deadline, "never cached: {again}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let file_url = cached["url"].as_str().unwrap_or_default();
    assert!(file_url.contains("profile=aac-256"), "{cached}");
    let probe = ranged(c, file_url, &env.key, Some("bytes=0-1")).await?;
    assert_eq!(probe.status, 206);
    let whole = ranged(c, file_url, &env.key, None).await?;
    let (_, priming) = edit_list(&whole.body).ok_or_else(|| anyhow::anyhow!("no edit list"))?;
    assert_eq!(priming, 1024, "the cached copy keeps the gapless edit list");
    let prepared = c
        .post(
            "/transcodes/prepare",
            &env.key,
            &json!({"songs": [id], "profile": "aac-256"}),
        )
        .await?;
    assert_eq!(prepared.status, 202);
    assert_eq!(prepared.json()["jobs"][0]["state"], "cached");
    Ok(())
}

/// A Subsonic URL signed the way Tracks' `SubsonicClient` signs it.
struct Tracks {
    client: Client,
    username: String,
    password: String,
}

impl Tracks {
    fn url(&self, endpoint: &str, params: &[(&str, &str)], json: bool) -> String {
        let salt = format!("{:012x}", trackside_e2e::now_ms());
        let token = hex(&Md5::digest(format!("{}{salt}", self.password).as_bytes()));
        let mut items = vec![
            (String::from("u"), self.username.clone()),
            (String::from("t"), token),
            (String::from("s"), salt),
            (String::from("v"), String::from("1.16.1")),
            (String::from("c"), String::from("Tracks")),
        ];
        if json {
            items.push((String::from("f"), String::from("json")));
        }
        let mut sorted: Vec<(&str, &str)> = params.to_vec();
        sorted.sort_by_key(|(k, _)| *k);
        items.extend(
            sorted
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        let query: Vec<String> = items
            .iter()
            .map(|(k, v)| format!("{k}={}", encode(v)))
            .collect();
        self.client
            .url(&format!("/rest/{endpoint}?{}", query.join("&")))
    }

    async fn call(&self, endpoint: &str, params: &[(&str, &str)]) -> anyhow::Result<Value> {
        let reply =
            trackside_e2e::client::send(self.client.http.get(self.url(endpoint, params, true)))
                .await?;
        let body = reply.json();
        anyhow::ensure!(
            body["subsonic-response"]["status"] == "ok",
            "{endpoint}: {} {}",
            reply.status,
            reply.text()
        );
        Ok(body["subsonic-response"].clone())
    }

    async fn media(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
        range: Option<&str>,
    ) -> anyhow::Result<Reply> {
        let mut request = self.client.http.get(self.url(endpoint, params, false));
        if let Some(range) = range {
            request = request.header("range", range);
        }
        trackside_e2e::client::send(request).await
    }
}

/// `URLComponents` encoding plus Tracks' own `+` to `%2B`.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn subsonic_answers_every_request_the_tracks_app_sends() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let c = &env.client;
    let created = c
        .post(
            "/me/api-keys",
            &env.admin,
            &json!({"kind": "app_password", "name": "Tracks (Subsonic)", "scopes": ["library", "stream", "write", "download"]}),
        )
        .await?;
    assert_eq!(created.status, 201, "{}", created.text());
    let body = created.json();
    let tracks = Tracks {
        client: c.clone(),
        username: body["username"].as_str().unwrap_or("luke").to_owned(),
        password: body["secret"].as_str().unwrap_or_default().to_owned(),
    };
    tracks.call("ping", &[]).await?;
    let artists = tracks.call("getArtists", &[]).await?;
    let all: Vec<&Value> = artists["artists"]["index"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|i| i["artist"].as_array().into_iter().flatten())
        .collect();
    let beyonce = all
        .iter()
        .find(|a| a["name"] == "Beyoncé")
        .ok_or_else(|| anyhow::anyhow!("no Beyoncé in {artists}"))?;
    let artist_id = beyonce["id"].as_str().unwrap_or_default();
    let artist = tracks.call("getArtist", &[("id", artist_id)]).await?;
    let album_id = artist["artist"]["album"][0]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let album = tracks.call("getAlbum", &[("id", &album_id)]).await?;
    let songs = album["album"]["song"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        songs.len(),
        5,
        "four tracks and the duplicate copy: {album}"
    );
    let song_id = songs[0]["id"].as_str().unwrap_or_default().to_owned();
    for kind in [
        "newest",
        "alphabeticalByName",
        "random",
        "frequent",
        "recent",
        "starred",
    ] {
        tracks
            .call(
                "getAlbumList2",
                &[("type", kind), ("size", "20"), ("offset", "0")],
            )
            .await?;
    }
    let found = tracks
        .call(
            "search3",
            &[
                ("query", "sigur ros"),
                ("artistCount", "5"),
                ("albumCount", "5"),
                ("songCount", "20"),
                ("songOffset", "0"),
            ],
        )
        .await?;
    assert!(
        found["searchResult3"]["artist"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "{found}"
    );
    let plus = tracks
        .call(
            "search3",
            &[
                ("query", "The + Lovers"),
                ("artistCount", "5"),
                ("albumCount", "5"),
                ("songCount", "5"),
                ("songOffset", "0"),
            ],
        )
        .await;
    assert!(plus.is_ok(), "a + in a query is not an error: {plus:?}");
    tracks
        .call("getSimilarSongs2", &[("id", artist_id), ("count", "20")])
        .await?;
    tracks
        .call("getTopSongs", &[("artist", "Beyoncé"), ("count", "20")])
        .await?;
    tracks
        .call("getArtistInfo2", &[("id", artist_id), ("count", "20")])
        .await?;
    tracks
        .call("getRandomSongs", &[("size", "10"), ("genre", "Pop")])
        .await?;
    let now = trackside_e2e::now_ms().to_string();
    tracks
        .call(
            "scrobble",
            &[("id", &song_id), ("time", &now), ("submission", "false")],
        )
        .await?;
    let playing = tracks.call("getNowPlaying", &[]).await?;
    assert_eq!(
        playing["nowPlaying"]["entry"][0]["id"],
        song_id.as_str(),
        "{playing}"
    );
    tracks
        .call(
            "scrobble",
            &[("id", &song_id), ("time", &now), ("submission", "true")],
        )
        .await?;
    tracks.call("star", &[("id", &song_id)]).await?;
    let starred = c.get("/me/stars?kind=song", &env.key).await?.json();
    assert_eq!(starred["items"][0]["id"], song_id.as_str(), "{starred}");
    tracks.call("unstar", &[("id", &song_id)]).await?;
    let animated = trackside_e2e::client::send(c.http.get(tracks.url(
        "getAnimatedArt",
        &[("artist", "Beyoncé"), ("album", "I Am... Sasha Fierce")],
        true,
    )))
    .await?;
    assert!(
        animated.status < 500,
        "a Subsonic extension Trackside lacks is a clean error"
    );

    let native_song = env.song("Sigur Song").await?;
    let now_playing = c
        .post(
            "/now-playing",
            &env.key,
            &json!({"song_id": native_song["id"], "position_ms": 500, "state": "playing"}),
        )
        .await?;
    assert_eq!(now_playing.status, 204);
    let playing = tracks.call("getNowPlaying", &[]).await?;
    let ids: Vec<&str> = playing["nowPlaying"]["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert!(
        ids.contains(&native_song["id"].as_str().unwrap_or_default()),
        "native now playing shows in Subsonic: {playing}"
    );

    let raw = tracks
        .media("stream", &[("id", &song_id), ("format", "raw")], None)
        .await?;
    assert_eq!(raw.status, 200);
    let original = std::fs::read(
        env.music().join(
            env.lib
                .track("Halo")
                .map(|t| t.rel.clone())
                .unwrap_or_default(),
        ),
    )?;
    assert_eq!(raw.body, original);
    let probe = tracks
        .media(
            "stream",
            &[("id", &song_id), ("format", "raw")],
            Some("bytes=0-1"),
        )
        .await?;
    assert_eq!(probe.status, 206);
    let mp3 = tracks
        .media(
            "stream",
            &[("id", &song_id), ("maxBitRate", "128"), ("format", "mp3")],
            None,
        )
        .await?;
    assert_eq!(mp3.status, 200, "{}", mp3.text());
    assert!(
        mp3.header("content-type")
            .unwrap_or_default()
            .contains("mpeg"),
        "{:?}",
        mp3.header("content-type")
    );
    assert!(mp3.body.len() > 1000);
    let download = tracks.media("download", &[("id", &song_id)], None).await?;
    assert_eq!(download.body, original);
    let cover = tracks
        .media("getCoverArt", &[("id", &album_id), ("size", "300")], None)
        .await?;
    assert_eq!(cover.status, 200);
    assert!(
        cover
            .header("content-type")
            .unwrap_or_default()
            .starts_with("image/")
    );
    let missing = tracks
        .media(
            "stream",
            &[("id", "s0000000000000000000"), ("format", "raw")],
            None,
        )
        .await?;
    assert_eq!(
        missing.status, 404,
        "media errors are not 200 with an error body"
    );

    let key = c
        .post(
            "/me/api-keys",
            &env.admin,
            &json!({"kind": "api_key", "name": "HLS", "scopes": ["library", "stream"]}),
        )
        .await?
        .json();
    let api_key = key["secret"].as_str().unwrap_or_default();
    let hls = trackside_e2e::client::send(c.http.get(c.url(&format!(
        "/rest/hls.m3u8?id={song_id}&apiKey={api_key}&v=1.16.1&c=e2e"
    ))))
    .await?;
    assert_eq!(hls.status, 200, "{}", hls.text());
    let text = hls.text();
    assert!(text.starts_with("#EXTM3U"), "{text}");
    let first = text
        .lines()
        .find(|l| !l.starts_with('#') && !l.is_empty())
        .unwrap_or_default();
    let segment_url = if first.starts_with("http") {
        first.to_owned()
    } else {
        let resolved = first
            .strip_prefix("../")
            .map_or_else(|| format!("/rest/{first}"), |rest| format!("/{rest}"));
        c.url(&resolved)
    };
    let segment = trackside_e2e::client::send(c.http.get(&segment_url)).await?;
    assert_eq!(segment.status, 200, "{segment_url}: {}", segment.text());
    Ok(())
}

/// A household at once: several players streaming and seeking the same song, others asking
/// for the same transcode, listens arriving, and a full scan running underneath. Every
/// stream must carry the exact bytes, the transcode must be made once and agree for everyone,
/// and nothing may fail or wait on the scan.
#[tokio::test(flavor = "multi_thread")]
async fn streams_transcodes_listens_and_a_scan_run_together() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let halo = env.song("Halo").await?;
    let id = halo["id"].as_str().unwrap_or_default().to_owned();
    let original = std::fs::read(
        env.music().join(
            env.lib
                .track("Halo")
                .map(|t| t.rel.clone())
                .unwrap_or_default(),
        ),
    )?;
    let library = env.library_id().await?;
    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    // One account is one person, allowed 8 streams at once: 4 players and the 4 downloads
    // below fill them. The 4 downloads of one transcode share one encoder and so take one
    // transcode slot of 3.
    for player in 0..4usize {
        let (c, key, id, original) = (
            env.client.clone(),
            env.key.clone(),
            id.clone(),
            original.clone(),
        );
        tasks.spawn(async move {
            let url = format!("/songs/{id}/stream");
            for round in 0..5usize {
                let from = (player * 997 + round * 131) % (original.len() - 1);
                let part = ranged(&c, &url, &key, Some(&format!("bytes={from}-"))).await?;
                anyhow::ensure!(part.status == 206, "player {player}: {}", part.status);
                anyhow::ensure!(
                    part.body == original[from..],
                    "player {player}: wrong bytes"
                );
            }
            let whole = ranged(&c, &url, &key, None).await?;
            anyhow::ensure!(whole.body == original, "player {player}: wrong file");
            Ok(None)
        });
    }
    for listener in 0..4usize {
        let (c, key, id) = (env.client.clone(), env.key.clone(), id.clone());
        tasks.spawn(async move {
            let now = trackside_e2e::now_ms();
            let items: Vec<Value> = (0..50i64)
                .map(|i| {
                    json!({"client_listen_id": format!("HOUSE-{listener}-{i}"), "song_id": id,
                        "started_at": trackside_e2e::rfc3339(now - 60_000 * (i + 1)),
                        "played_ms": 2000, "title": "Halo", "artist": "Beyoncé",
                        "duration_ms": 2000})
                })
                .collect();
            let reply = c
                .post(
                    "/listens",
                    &key,
                    &json!({"client_time": trackside_e2e::rfc3339(now), "listens": items}),
                )
                .await?;
            anyhow::ensure!(reply.status == 200, "listens: {}", reply.text());
            Ok(None)
        });
    }
    for _ in 0..4 {
        let (c, key, id) = (env.client.clone(), env.key.clone(), id.clone());
        tasks.spawn(async move {
            let url = format!("/songs/{id}/download?profile=aac-128");
            let reply = ranged(&c, &url, &key, None).await?;
            anyhow::ensure!(reply.status == 200, "transcode: {}", reply.text());
            Ok(Some((reply.header("etag").map(str::to_owned), reply.body)))
        });
    }
    let scan = env
        .client
        .post(
            &format!("/admin/libraries/{library}/scan"),
            &env.admin,
            &json!({"kind": "full"}),
        )
        .await?;
    assert_eq!(scan.status, 202, "{}", scan.text());
    let mut transcodes = Vec::new();
    while let Some(done) = tasks.join_next().await {
        if let Some(transcode) = done?? {
            transcodes.push(transcode);
        }
    }
    let finished = common::wait_scan(&env.client, &env.admin, &library).await?;
    assert_eq!(finished["state"], "done", "{finished}");
    assert_eq!(transcodes.len(), 4);
    assert!(
        transcodes.windows(2).all(|w| w[0] == w[1]),
        "every caller gets the same transcode"
    );
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "took {:?}: {}",
        started.elapsed(),
        env.server.log_text()
    );
    let after = env.song("Halo").await?;
    assert_eq!(after["id"], halo["id"], "the scan kept the id");
    Ok(())
}
