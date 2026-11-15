//! Media: `stream`, `download`, `hls`, `getCoverArt`, and lyrics.
//!
//! Failed media requests answer the error envelope with a non-200 status, never an error body
//! with `200`. Streaming never counts a play; only `scrobble` and `reportPlayback` do.

use std::time::Duration;

use trackside_auth::Scopes;
use trackside_core::records::{AudioFormat, Codec, LyricsKind, Song, SongFile};
use trackside_core::{ArtworkId, SongId, repo};
use trackside_media::{Disposition, HlsInput, HlsOwner, MediaError, Priority, Profile};

use crate::error::ApiError;
use crate::ids::{self, AnyId};
use crate::lyrics;
use crate::response::{Body, Node};
use crate::{Call, LIBRARY_RETRY, Reply, raw};

/// How a song is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Original,
    Transcode(Profile),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Mp3,
    Aac,
    Opus,
    Flac,
}

impl Family {
    fn parse(format: &str) -> Option<Self> {
        match format {
            "mp3" => Some(Self::Mp3),
            "aac" | "m4a" => Some(Self::Aac),
            "opus" | "ogg" => Some(Self::Opus),
            "flac" => Some(Self::Flac),
            _ => None,
        }
    }

    fn codec(self) -> Codec {
        match self {
            Self::Mp3 => Codec::Mp3,
            Self::Aac => Codec::Aac,
            Self::Opus => Codec::Opus,
            Self::Flac => Codec::Flac,
        }
    }

    fn profile(self, limit: Option<u32>) -> Profile {
        match (self, limit) {
            (Self::Mp3, Some(kbps)) if kbps < 320 => Profile::Mp3_128,
            (Self::Mp3, _) => Profile::Mp3_320,
            (Self::Aac, Some(kbps)) if kbps < 256 => Profile::Aac128,
            (Self::Aac, _) => Profile::Aac256,
            (Self::Opus, _) => Profile::Opus128,
            (Self::Flac, None) => Profile::Flac,
            (Self::Flac, Some(kbps)) => Self::Mp3.profile(Some(kbps)),
        }
    }
}

/// Picks original or transcode for a `stream` request (architecture section 8): `raw`, or no
/// format and no bitrate, is the original; the user's `max_bitrate_kbps` always applies.
pub fn choose(
    format: Option<&str>,
    max_bitrate: Option<u32>,
    user_limit: Option<u32>,
    original: &AudioFormat,
    can_transcode: bool,
) -> Delivery {
    if !can_transcode {
        return Delivery::Original;
    }
    let format = format.map(str::to_ascii_lowercase);
    let client_limit = if format.as_deref() == Some("raw") {
        None
    } else {
        max_bitrate.filter(|k| *k > 0)
    };
    let limit = match (client_limit, user_limit.filter(|k| *k > 0)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let within = |limit: Option<u32>| match (limit, original.bitrate_kbps) {
        (None, _) | (Some(_), None) => true,
        (Some(limit), Some(bitrate)) => bitrate <= limit,
    };
    let family = format.as_deref().and_then(Family::parse);
    match family {
        Some(family) if original.codec == family.codec() && within(limit) => Delivery::Original,
        Some(family) => Delivery::Transcode(family.profile(limit)),
        None if within(limit) => Delivery::Original,
        None => Delivery::Transcode(Family::Mp3.profile(limit)),
    }
}

/// The song and where its bytes are, or error 70; error 0 with `503` while its library is
/// offline.
async fn song_file(call: &Call<'_>) -> Result<(Song, SongFile), ApiError> {
    let id: SongId = ids::parse(call.params.required("id")?, "Song")?;
    let (song, file) = call
        .read(move |c, a| {
            let song = repo::songs::get(c, a, &id)?.ok_or_else(|| ApiError::not_found("Song"))?;
            let file = repo::songs::file(c, a, &id)?;
            Ok((song, file))
        })
        .await?;
    if !call.state().scan.is_available(&song.library) {
        return Err(ApiError::unavailable(
            "The song's library is offline",
            LIBRARY_RETRY,
        ));
    }
    let file = file.ok_or_else(|| ApiError::not_found("Song file"))?;
    Ok((song, file))
}

/// One of the user's stream or transcode slots, or a busy error when all are taken.
fn slot(
    taken: Result<tokio::sync::OwnedSemaphorePermit, trackside_auth::AuthError>,
) -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    taken.map_err(|_| {
        ApiError::unavailable("Too many streams at once", Duration::from_secs(1))
            .with_status(http::StatusCode::TOO_MANY_REQUESTS)
    })
}

fn user_limit(call: &Call<'_>) -> Option<u32> {
    if call.is_admin() {
        None
    } else {
        call.principal.permissions.max_bitrate_kbps
    }
}

pub async fn stream(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::STREAM)?;
    let (_, file) = song_file(call).await?;
    let auth = &call.state().auth;
    let stream = slot(auth.stream_slot(&call.principal))?;
    let media = &call.state().media;
    let delivery = choose(
        call.params.get("format"),
        call.params.int("maxBitRate")?,
        user_limit(call),
        &file.format,
        media.capabilities().ffmpeg.is_some(),
    );
    let response = match delivery {
        Delivery::Original => {
            media
                .serve_file(&file, call.parts, Disposition::Inline)
                .await
        }
        Delivery::Transcode(profile) => {
            let offset = call
                .params
                .int::<u64>("timeOffset")?
                .map(Duration::from_secs)
                .filter(|o| !o.is_zero());
            let estimate = call.params.bool("estimateContentLength")?.unwrap_or(false);
            // AAC streams as fragmented MP4 and is never cached; a complete MP3, Opus or FLAC
            // transcode in the cache is served with ranges.
            let cached = if offset.is_none() && !is_aac(profile) {
                media.cached(&file, profile).await
            } else {
                None
            };
            let transcoded = match cached {
                Some(transcode) => {
                    media
                        .serve_transcode(&transcode, call.parts, Disposition::Inline)
                        .await
                }
                None => {
                    let transcoding = slot(auth.transcode_slot(&call.principal))?;
                    media
                        .progressive_transcode(&file, profile, offset, estimate)
                        .await
                        .map(|r| trackside_media::hold(r, transcoding))
                }
            };
            match transcoded {
                // Subsonic lets a server ignore a transcode request; the original is better
                // than no music.
                Err(MediaError::TranscoderBusy | MediaError::NoTranscoder) => {
                    media
                        .serve_file(&file, call.parts, Disposition::Inline)
                        .await
                }
                other => other,
            }
        }
    };
    Ok(Reply::Raw(trackside_media::hold(response?, stream)))
}

fn is_aac(profile: Profile) -> bool {
    matches!(profile, Profile::Aac256 | Profile::Aac128)
}

fn file_name(song: &Song, file: &SongFile) -> String {
    file.path.file_name().map_or_else(
        || format!("{}.{}", song.title, file.format.container.as_str()),
        |n| n.to_string_lossy().into_owned(),
    )
}

pub async fn download(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::DOWNLOAD)?;
    if !call.is_admin() && !call.principal.permissions.can_download {
        return Err(ApiError::not_authorized(
            "Downloads are turned off for this account",
        ));
    }
    let (song, file) = song_file(call).await?;
    let stream = slot(call.state().auth.stream_slot(&call.principal))?;
    let disposition = Disposition::Attachment {
        file_name: file_name(&song, &file),
    };
    let response = call
        .state()
        .media
        .serve_file(&file, call.parts, disposition)
        .await?;
    Ok(Reply::Raw(trackside_media::hold(response, stream)))
}

/// The first number in `bitRate`, which video clients send as `640x480@1000` or as a list.
fn first_bitrate(values: &[&str]) -> Option<u32> {
    let value = values.first()?;
    let audio = value.rsplit('@').next().unwrap_or(value);
    let digits: String = audio.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

pub async fn hls(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::STREAM)?;
    let (_, file) = song_file(call).await?;
    let limit = match (first_bitrate(&call.params.all("bitRate")), user_limit(call)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let profile = Family::Aac.profile(limit);
    let media = &call.state().media;
    let owner = HlsOwner {
        user: Some(call.principal.access.user),
        hold: Some(Box::new(slot(
            call.state().auth.transcode_slot(&call.principal),
        )?)),
    };
    let job = media
        .start_hls(HlsInput::File(file), profile, Priority::Interactive, owner)
        .await?;
    let token = call
        .state()
        .auth
        .mint_media_token(&call.principal)
        .map_err(|_| ApiError::generic("HLS is not available for this credential"))?;
    let playlist = media
        .hls_playlist(&job, &format!("?mt={}", token.token))
        .await?;
    let playlist = rebase_playlist(&playlist, &format!("../api/v1/transcodes/{}/", job.0));
    Ok(Reply::Raw(raw("application/vnd.apple.mpegurl", playlist)))
}

/// The media crate's playlist names its parts relative to the native playlist's folder; this
/// one is fetched from `/rest/hls.m3u8`, so each part URL gets `prefix` (relative to `/rest/`,
/// which keeps `server.base_path` working).
pub fn rebase_playlist(playlist: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(playlist.len() + 64);
    for line in playlist.lines() {
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        if line.starts_with('#') {
            out.push_str(&line.replace("URI=\"", &format!("URI=\"{prefix}")));
        } else {
            out.push_str(prefix);
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

pub async fn cover_art(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::STREAM)?;
    let id = AnyId::parse(call.params.required("id")?);
    let size: Option<u32> = call.params.int("size")?;
    let artwork: Option<ArtworkId> = match id {
        None => None,
        Some(AnyId::Artwork(id)) => {
            call.read(move |c, a| Ok(repo::artwork::visible_to(c, a, &id)?.then_some(id)))
                .await?
        }
        Some(id) => {
            call.read(move |c, a| {
                Ok(match id {
                    AnyId::Song(id) => repo::songs::get(c, a, &id)?.and_then(|s| s.artwork),
                    AnyId::Album(id) => repo::albums::get(c, a, &id)?.and_then(|al| al.artwork),
                    AnyId::Artist(id) => repo::artists::get(c, a, &id)?.and_then(|r| r.artwork),
                    AnyId::Playlist(id) => match repo::playlists::get(c, a, &id)? {
                        Some((_, entries)) => {
                            let ids: Vec<SongId> = entries.into_iter().map(|e| e.song).collect();
                            repo::songs::get_many(c, a, &ids)?
                                .into_iter()
                                .find_map(|s| s.artwork)
                        }
                        None => None,
                    },
                    _ => None,
                })
            })
            .await?
        }
    };
    let artwork = artwork.ok_or_else(|| ApiError::not_found("Cover art"))?;
    let response = call
        .state()
        .media
        .artwork(&artwork, size.filter(|s| *s > 0), call.parts)
        .await
        .map_err(|error| match error {
            MediaError::NotFound => ApiError::not_found("Cover art"),
            other => other.into(),
        })?;
    Ok(Reply::Raw(response))
}

pub async fn lyrics(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let artist = call.params.get("artist").map(str::to_owned);
    let title = call.params.get("title").map(str::to_owned);
    let (Some(artist), Some(title)) = (artist, title) else {
        return Ok(Body::one("lyrics", Node::new()).into());
    };
    let found = call
        .read({
            let (artist, title) = (artist.clone(), title.clone());
            move |c, a| {
                let key = tracks_model::song_key(&title, &artist);
                for song in repo::songs::by_key(c, a, &key)? {
                    if !song.has_lyrics {
                        continue;
                    }
                    let all = repo::lyrics::for_song(c, a, &song.id)?;
                    let plain = all.iter().find(|l| l.kind == LyricsKind::Plain);
                    if let Some(plain) = plain {
                        return Ok(Some((song, plain.content.clone())));
                    }
                    if let Some(synced) = all.first() {
                        return Ok(Some((song, lyrics::lrc_to_plain(&synced.content))));
                    }
                }
                Ok(None)
            }
        })
        .await?;
    let node = match found {
        Some((song, text)) => Node::new()
            .attr("artist", song.artist.as_str())
            .attr("title", song.title.as_str())
            .text(text),
        None => Node::new(),
    };
    Ok(Body::one("lyrics", node).into())
}

pub async fn lyrics_by_song_id(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::LIBRARY)?;
    let id: SongId = ids::parse(call.params.required("id")?, "Song")?;
    let structured = call
        .read(move |c, a| {
            let song = repo::songs::get(c, a, &id)?.ok_or_else(|| ApiError::not_found("Song"))?;
            let nodes = repo::lyrics::for_song(c, a, &id)?
                .iter()
                .map(|l| {
                    let synced = l.kind == LyricsKind::Synced;
                    let (lines, offset) = if synced {
                        let parsed = lyrics::parse_lrc(&l.content);
                        (parsed.lines, parsed.offset_ms)
                    } else {
                        (lyrics::plain_lines(&l.content), 0)
                    };
                    let lang = if l.lang.trim().is_empty() {
                        "xxx"
                    } else {
                        l.lang.as_str()
                    };
                    Node::new()
                        .attr("displayArtist", song.artist.as_str())
                        .attr("displayTitle", song.title.as_str())
                        .attr("lang", lang)
                        .attr("offset", offset)
                        .attr("synced", synced)
                        .many(
                            "line",
                            lines
                                .into_iter()
                                .map(|line| {
                                    Node::new().attr_opt("start", line.start).text(line.value)
                                })
                                .collect(),
                        )
                })
                .collect::<Vec<_>>();
            Ok(nodes)
        })
        .await?;
    Ok(Body::one(
        "lyricsList",
        Node::new().many("structuredLyrics", structured),
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::fixtures;

    fn flac() -> AudioFormat {
        fixtures::song().format
    }

    fn mp3(kbps: u32) -> AudioFormat {
        AudioFormat {
            container: trackside_core::records::Container::Mp3,
            codec: Codec::Mp3,
            content_type: "audio/mpeg",
            lossless: false,
            bitrate_kbps: Some(kbps),
            ..flac()
        }
    }

    #[test]
    fn tracks_request_shapes_choose_as_expected() {
        assert_eq!(
            choose(Some("raw"), None, None, &flac(), true),
            Delivery::Original
        );
        assert_eq!(
            choose(Some("mp3"), Some(320), None, &flac(), true),
            Delivery::Transcode(Profile::Mp3_320)
        );
        assert_eq!(
            choose(Some("mp3"), Some(128), None, &flac(), true),
            Delivery::Transcode(Profile::Mp3_128)
        );
        assert_eq!(choose(None, None, None, &flac(), true), Delivery::Original);
    }

    #[test]
    fn other_client_shapes() {
        assert_eq!(
            choose(None, Some(192), None, &flac(), true),
            Delivery::Transcode(Profile::Mp3_128)
        );
        assert_eq!(
            choose(None, Some(320), None, &mp3(256), true),
            Delivery::Original
        );
        assert_eq!(
            choose(Some("mp3"), Some(320), None, &mp3(256), true),
            Delivery::Original
        );
        assert_eq!(
            choose(Some("opus"), None, None, &flac(), true),
            Delivery::Transcode(Profile::Opus128)
        );
        assert_eq!(
            choose(Some("aac"), Some(0), None, &flac(), true),
            Delivery::Transcode(Profile::Aac256)
        );
        assert_eq!(
            choose(Some("flac"), None, None, &flac(), true),
            Delivery::Original
        );
        assert_eq!(
            choose(Some("wma"), None, None, &flac(), true),
            Delivery::Original
        );
    }

    #[test]
    fn the_user_limit_overrides_the_client() {
        assert_eq!(
            choose(Some("raw"), None, Some(128), &flac(), true),
            Delivery::Transcode(Profile::Mp3_128)
        );
        assert_eq!(
            choose(Some("mp3"), Some(320), Some(128), &mp3(320), true),
            Delivery::Transcode(Profile::Mp3_128)
        );
    }

    #[test]
    fn without_ffmpeg_everything_is_the_original() {
        assert_eq!(
            choose(Some("mp3"), Some(128), Some(64), &flac(), false),
            Delivery::Original
        );
    }

    #[test]
    fn bitrate_lists_and_video_shapes() {
        assert_eq!(first_bitrate(&["128,256"]), Some(128));
        assert_eq!(first_bitrate(&["640x480@1000"]), Some(1000));
        assert_eq!(first_bitrate(&[]), None);
    }
}

#[cfg(test)]
mod playlist_tests {
    use super::rebase_playlist;

    #[test]
    fn part_urls_point_at_the_native_transcode_routes() {
        let native = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4?mt=t\"\n#EXTINF:6.0,\nseg-0.m4s?mt=t\n#EXT-X-ENDLIST\n";
        let rebased = rebase_playlist(native, "../api/v1/transcodes/j1/");
        assert_eq!(
            rebased,
            "#EXTM3U\n#EXT-X-MAP:URI=\"../api/v1/transcodes/j1/init.mp4?mt=t\"\n#EXTINF:6.0,\n../api/v1/transcodes/j1/seg-0.m4s?mt=t\n#EXT-X-ENDLIST\n"
        );
    }
}
