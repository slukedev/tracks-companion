//! Playback decisions, streams, downloads, HLS and artwork (api.md section 11). Media gets
//! already authorised requests; access is checked here through the repositories.

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http::request::Parts;
use http::{HeaderValue, StatusCode, header};
use serde::Deserialize;
use trackside_auth::extract::{Authed, MediaAuthed};
use trackside_auth::{Principal, Scopes};
use trackside_core::control::{ScanKind, ScanTrigger};
use trackside_core::records::{AudioFormat, Codec, Container, Song, SongFile};
use trackside_core::repo::{albums, artists, folders, songs};
use trackside_core::{Access, AlbumId, ArtistId, ArtworkId, PublicId, SongId};
use trackside_media::{
    ClientProfile, Decision, DecisionReason, DirectPlay, Disposition, HlsInput, HlsJobId, HlsOwner,
    HlsPart, MediaError, Priority, Profile,
};

use super::{api_url, read};
use crate::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Json, Query, body_id, json_with, path_id};
use crate::wire::playback::{
    Decision as WireDecision, DecisionMode, DecisionRequest, Delivered, PrepareRequest, Prepared,
    PreparedJob,
};

const STREAM: u8 = Scopes::STREAM.bits();

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/playback/decisions", post(decide))
        .route("/transcodes/prepare", post(prepare))
        .route("/songs/{id}/stream", get(stream))
        .route("/songs/{id}/download", get(download))
        .route("/transcodes/{job}/index.m3u8", get(hls_playlist))
        .route("/transcodes/{job}/{part}", get(hls_part))
        .route("/art/{id}", get(artwork))
}

/// The caller as an HLS job's owner, holding one of their transcode slots for the encoder.
pub(crate) fn hls_owner(state: &ApiState, principal: &Principal) -> ApiResult<HlsOwner> {
    Ok(HlsOwner {
        user: Some(principal.access.user),
        hold: Some(Box::new(state.auth.transcode_slot(principal)?)),
    })
}

/// Why a transcode that may need one of the caller's slots did not finish.
#[derive(Debug)]
enum Admit {
    Media(MediaError),
    Refused(ApiError),
}

impl From<MediaError> for Admit {
    fn from(error: MediaError) -> Self {
        Self::Media(error)
    }
}

impl From<Admit> for ApiError {
    fn from(error: Admit) -> Self {
        match error {
            Admit::Media(e) => e.into(),
            Admit::Refused(e) => e,
        }
    }
}

/// One of the caller's transcode slots, for a request that starts the encoder.
fn transcode_slot(
    state: &ApiState,
    principal: &Principal,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, Admit> {
    state
        .auth
        .transcode_slot(principal)
        .map(Some)
        .map_err(|e| Admit::Refused(e.into()))
}

/// An HLS job may be read only by the user it was made for (or an admin), and only while
/// they can still see its song.
pub(crate) async fn check_hls_job(
    state: &ApiState,
    principal: &Principal,
    job: &HlsJobId,
) -> ApiResult<()> {
    let Some(info) = state.media.hls_job(job) else {
        return Err(MediaError::JobExpired.into());
    };
    if !principal.access.is_admin() && info.user != Some(principal.access.user) {
        return Err(ApiError::not_found());
    }
    if let Some(song) = info.song {
        let access = principal.access.clone();
        let visible = read(state, move |c| songs::get(c, &access, &song))
            .await?
            .is_some();
        if !visible {
            return Err(ApiError::not_found());
        }
    }
    Ok(())
}

/// A visible song and its file, or `404`; a song on an offline library is `503`.
async fn song_and_file(
    state: &ApiState,
    principal: &Principal,
    id: SongId,
) -> ApiResult<(Song, SongFile)> {
    let access = principal.access.clone();
    let (song, file) = read(state, move |c| {
        let song = songs::get(c, &access, &id)?;
        let file = songs::file(c, &access, &id)?;
        Ok((song, file))
    })
    .await?;
    let song = song.ok_or_else(ApiError::not_found)?;
    if !super::available(state, &song.library) {
        return Err(ApiError::library_unavailable());
    }
    let file = file.ok_or_else(ApiError::not_found)?;
    Ok((song, file))
}

/// The user's own limit: none for admins.
fn user_limit(principal: &Principal) -> Option<u32> {
    if principal.access.is_admin() {
        None
    } else {
        principal.permissions.max_bitrate_kbps
    }
}

/// The client's profile in media's types. Unknown containers and codecs are dropped, since a
/// client cannot be offered what the server does not know.
pub fn client_profile(client: &crate::wire::playback::ClientProfile) -> ApiResult<ClientProfile> {
    let preferred_profile = client
        .preferred_profile
        .as_deref()
        .map(|p| {
            Profile::parse(p)
                .ok_or_else(|| ApiError::bad_field("client.preferred_profile", "Unknown profile."))
        })
        .transpose()?;
    Ok(ClientProfile {
        direct: client
            .direct
            .iter()
            .filter_map(|d| {
                Some(DirectPlay {
                    container: Container::parse(&d.container.to_ascii_lowercase())?,
                    codecs: d
                        .codecs
                        .iter()
                        .filter_map(|c| Codec::parse(&c.to_ascii_lowercase()))
                        .collect(),
                })
            })
            .collect(),
        hls: client.hls,
        max_bitrate_kbps: client.max_bitrate_kbps,
        preferred_profile,
        allow_original_fallback: client.allow_original_fallback,
    })
}

pub fn reason_text(reason: DecisionReason) -> &'static str {
    match reason {
        DecisionReason::OverBitrate => "over_bitrate",
        DecisionReason::UnsupportedCodec => "unsupported_codec",
        DecisionReason::TranscoderBusy => "transcoder_busy",
        DecisionReason::NoTranscoder => "no_transcoder",
    }
}

fn delivered(format: &AudioFormat, duration_ms: u32, size: Option<u64>) -> Delivered {
    Delivered {
        container: format.container.as_str().to_owned(),
        codec: format.codec.as_str().to_owned(),
        bitrate_kbps: format.bitrate_kbps,
        sample_rate: format.sample_rate,
        bit_depth: format.bit_depth,
        lossless: format.lossless,
        size,
        duration_ms,
    }
}

/// The decision's wire shape. `hls_job` is the job started for an HLS decision.
pub fn decision_body(
    base: &str,
    file: &SongFile,
    decision: &Decision,
    hls_job: Option<&HlsJobId>,
) -> WireDecision {
    let song = file.song.as_str();
    match decision {
        Decision::Direct { reason } => WireDecision {
            mode: DecisionMode::Direct,
            url: format!("{base}/songs/{song}/stream"),
            delivered: delivered(&file.format, file.duration_ms, Some(file.size)),
            reason: reason
                .filter(|r| *r == DecisionReason::TranscoderBusy)
                .map(|r| reason_text(r).to_owned()),
            etag: None,
        },
        Decision::File { transcode, reason } => WireDecision {
            mode: DecisionMode::File,
            url: format!(
                "{base}/songs/{song}/stream?profile={}",
                transcode.profile.as_str()
            ),
            delivered: delivered(&transcode.format, file.duration_ms, Some(transcode.size)),
            reason: Some(reason_text(*reason).to_owned()),
            etag: Some(transcode.etag.clone()),
        },
        Decision::Hls { profile, reason } => {
            let (container, codec) = profile.output();
            let format = AudioFormat {
                container,
                codec,
                content_type: container.content_type(),
                lossless: codec.is_lossless(),
                bitrate_kbps: profile.bitrate_kbps(),
                sample_rate: file.format.sample_rate,
                bit_depth: None,
                channels: file.format.channels,
                size: 0,
            };
            WireDecision {
                mode: DecisionMode::Hls,
                url: format!(
                    "{base}/transcodes/{}/index.m3u8",
                    hls_job.map_or("", |j| &j.0)
                ),
                delivered: delivered(&format, file.duration_ms, None),
                reason: Some(reason_text(*reason).to_owned()),
                etag: None,
            }
        }
    }
}

async fn decide(
    Authed(principal): Authed<STREAM>,
    State(state): State<ApiState>,
    Json(body): Json<DecisionRequest>,
) -> ApiResult<Response> {
    let id: SongId = body_id(&body.song_id, "song_id")?;
    let client = client_profile(&body.client)?;
    let (_, file) = song_and_file(&state, &principal, id).await?;
    let mut decision = state.media.decide(&file, &client, user_limit(&principal));
    if let Decision::Hls { profile, reason } = decision
        && !client.hls
    {
        let transcode = state
            .media
            .ensure_cached_admitting(&file, profile, Priority::Interactive, || {
                transcode_slot(&state, &principal)
            })
            .await
            .map_err(ApiError::from)?;
        decision = Decision::File { transcode, reason };
    }
    tracing::debug!(
        network = body.network.as_deref().unwrap_or("unknown"),
        "playback decision"
    );
    let job = match &decision {
        Decision::Hls { profile, .. } => Some(
            state
                .media
                .start_hls(
                    HlsInput::File(file.clone()),
                    *profile,
                    Priority::Interactive,
                    hls_owner(&state, &principal)?,
                )
                .await?,
        ),
        _ => None,
    };
    let base = api_url(&state, "");
    Ok(json_with(
        StatusCode::OK,
        &decision_body(&base, &file, &decision, job.as_ref()),
    ))
}

async fn prepare(
    Authed(principal): Authed<STREAM>,
    State(state): State<ApiState>,
    Json(body): Json<PrepareRequest>,
) -> ApiResult<Response> {
    if body.songs.len() > 5 {
        return Err(ApiError::bad_field("songs", "At most five songs at once."));
    }
    let profile = Profile::parse(&body.profile)
        .ok_or_else(|| ApiError::bad_field("profile", "Unknown profile."))?;
    let ids = body
        .songs
        .iter()
        .map(|s| body_id::<SongId>(s, "songs"))
        .collect::<ApiResult<Vec<_>>>()?;
    let access = principal.access.clone();
    let files = read(&state, move |c| {
        let mut files = Vec::new();
        for id in &ids {
            if let Some(file) = songs::file(c, &access, id)? {
                files.push(file);
            }
        }
        Ok(files)
    })
    .await?;
    let prepared = state.media.prepare(&files, profile).await;
    let jobs = prepared
        .iter()
        .map(|p| PreparedJob {
            song_id: p.song.to_string(),
            state: match p.state {
                trackside_media::PrepareStateKind::Cached => "cached",
                trackside_media::PrepareStateKind::Queued => "queued",
                trackside_media::PrepareStateKind::Running => "running",
            }
            .to_owned(),
        })
        .collect();
    Ok(json_with(StatusCode::ACCEPTED, &Prepared { jobs }))
}

#[derive(Debug, Deserialize)]
struct ProfileQuery {
    profile: Option<String>,
}

fn parse_profile(raw: Option<&str>) -> ApiResult<Option<Profile>> {
    raw.filter(|p| !p.is_empty())
        .map(|p| {
            Profile::parse(p).ok_or_else(|| ApiError::bad_field("profile", "Unknown profile."))
        })
        .transpose()
}

/// A media error, after asking for a scan of the song's folder when its file changed under
/// the index, so the next attempt finds the new state.
fn changed_file(state: &ApiState, song: &Song, error: MediaError) -> ApiError {
    if matches!(error, MediaError::ContentChanged) {
        let db = state.db.clone();
        let scan = std::sync::Arc::clone(&state.scan);
        let library = song.library.clone();
        let folder = song.folder.clone();
        tokio::spawn(async move {
            let rel = db
                .read(move |c| folders::rel_path(c, &Access::system(), &folder))
                .await
                .ok()
                .flatten();
            let queued = match rel {
                Some(rel) => scan.request_folder_scan(&library, &rel),
                None => scan.request_scan(&library, ScanKind::Quick, ScanTrigger::Watch, false),
            };
            if let Err(e) = queued {
                tracing::debug!(error = %e, "rescan after a changed file not queued");
            }
        });
    }
    error.into()
}

/// A download's file name from the tags, without anything a file system or header could
/// misread.
pub fn download_name(song: &Song, container: Container) -> String {
    let raw = format!("{} - {}", song.artist, song.title);
    let clean: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = clean.trim().trim_start_matches('.');
    let stem: String = if trimmed.is_empty() {
        String::from("song")
    } else {
        trimmed.chars().take(150).collect()
    };
    format!("{stem}.{}", container.as_str())
}

async fn stream(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<ProfileQuery>,
    request: Request,
) -> ApiResult<Response> {
    let id: SongId = path_id(&id)?;
    let profile = parse_profile(query.profile.as_deref())?;
    let (song, file) = song_and_file(&state, &principal, id).await?;
    let slot = state.auth.stream_slot(&principal)?;
    let (parts, _) = request.into_parts();
    let response = match profile {
        None => state
            .media
            .serve_file(&file, &parts, Disposition::Inline)
            .await
            .map_err(|e| changed_file(&state, &song, e)),
        Some(profile) => {
            let Some(transcode) = state.media.cached(&file, profile).await else {
                return Err(ApiError::conflict(
                    "not_ready",
                    "This transcode is not ready; ask for a playback decision.",
                ));
            };
            Ok(state
                .media
                .serve_transcode(&transcode, &parts, Disposition::Inline)
                .await?)
        }
    }?;
    Ok(trackside_media::hold(response, slot))
}

async fn download(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<ProfileQuery>,
    request: Request,
) -> ApiResult<Response> {
    let id: SongId = path_id(&id)?;
    let allowed = principal.scopes.contains(Scopes::DOWNLOAD)
        && (principal.access.is_admin() || principal.permissions.can_download);
    if !allowed {
        return Err(ApiError::forbidden());
    }
    let profile = parse_profile(query.profile.as_deref())?;
    let (song, file) = song_and_file(&state, &principal, id).await?;
    let (parts, _) = request.into_parts();
    let response = match profile {
        None => {
            let slot = state.auth.stream_slot(&principal)?;
            let disposition = Disposition::Attachment {
                file_name: download_name(&song, file.format.container),
            };
            let response = state
                .media
                .serve_file(&file, &parts, disposition)
                .await
                .map_err(|e| changed_file(&state, &song, e))?;
            trackside_media::hold(response, slot)
        }
        Some(profile) => {
            // Only a request that starts the encoder takes a transcode slot: a household's
            // phones downloading the same album share one job each song.
            let transcode = state
                .media
                .ensure_cached_admitting(&file, profile, Priority::Download, || {
                    transcode_slot(&state, &principal)
                })
                .await
                .map_err(|e| match e {
                    Admit::Media(e) => changed_file(&state, &song, e),
                    Admit::Refused(e) => e,
                })?;
            // The stream slot is for sending bytes, so it is taken once the file is ready.
            let slot = state.auth.stream_slot(&principal)?;
            let disposition = Disposition::Attachment {
                file_name: download_name(&song, profile.output().0),
            };
            let response = state
                .media
                .serve_transcode(&transcode, &parts, disposition)
                .await?;
            trackside_media::hold(response, slot)
        }
    };
    Ok(response)
}

/// `?mt=...` from the request, carried onto each segment URL of a playlist.
fn media_token_suffix(parts: &Parts) -> String {
    parts
        .uri
        .query()
        .unwrap_or_default()
        .split('&')
        .find(|pair| pair.starts_with("mt="))
        .map(|pair| format!("?{pair}"))
        .unwrap_or_default()
}

fn playlist_response(text: String) -> Response {
    let mut response = text.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.apple.mpegurl"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    response
}

/// HLS job ids are opaque path segments.
fn job_id(raw: &str) -> ApiResult<HlsJobId> {
    if raw.is_empty()
        || raw.len() > 100
        || !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::not_found());
    }
    Ok(HlsJobId(raw.into()))
}

async fn hls_playlist(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path(job): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let job = job_id(&job)?;
    check_hls_job(&state, &principal, &job).await?;
    let (parts, _) = request.into_parts();
    let text = state
        .media
        .hls_playlist(&job, &media_token_suffix(&parts))
        .await?;
    Ok(playlist_response(text))
}

/// `init.mp4` or `seg-<n>.m4s`.
pub fn hls_part_name(name: &str) -> Option<HlsPart> {
    if name == "init.mp4" {
        return Some(HlsPart::Init);
    }
    let number = name.strip_prefix("seg-")?.strip_suffix(".m4s")?;
    number.parse().ok().map(HlsPart::Segment)
}

async fn hls_part(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path((job, part)): Path<(String, String)>,
    request: Request,
) -> ApiResult<Response> {
    let job = job_id(&job)?;
    check_hls_job(&state, &principal, &job).await?;
    let part = hls_part_name(&part).ok_or_else(ApiError::not_found)?;
    let (parts, _) = request.into_parts();
    Ok(state.media.hls_part(&job, part, &parts).await?)
}

#[derive(Debug, Deserialize)]
struct ArtQuery {
    size: Option<u32>,
}

/// An artwork id, or the artwork of a song, album or artist id.
async fn resolve_artwork(
    state: &ApiState,
    principal: &Principal,
    raw: &str,
) -> ApiResult<ArtworkId> {
    let access = principal.access.clone();
    let found = match raw.chars().next().map(|c| c.to_ascii_lowercase()) {
        Some('c') => {
            let id: ArtworkId = path_id(raw)?;
            let shown = id.clone();
            let visible = read(state, move |c| {
                trackside_core::repo::artwork::visible_to(c, &access, &shown)
            })
            .await?;
            visible.then_some(id)
        }
        Some('s') => {
            let id: SongId = path_id(raw)?;
            read(state, move |c| {
                Ok(songs::get(c, &access, &id)?.and_then(|s| s.artwork))
            })
            .await?
        }
        Some('a') => {
            let id: AlbumId = path_id(raw)?;
            read(state, move |c| {
                Ok(albums::get(c, &access, &id)?.and_then(|a| a.artwork))
            })
            .await?
        }
        Some('r') => {
            let id: ArtistId = path_id(raw)?;
            read(state, move |c| {
                Ok(artists::get(c, &access, &id)?.and_then(|a| a.artwork))
            })
            .await?
        }
        _ => None,
    };
    found.ok_or_else(ApiError::not_found)
}

async fn artwork(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<ArtQuery>,
    request: Request,
) -> ApiResult<Response> {
    let artwork = resolve_artwork(&state, &principal, &id).await?;
    let (parts, _) = request.into_parts();
    Ok(state.media.artwork(&artwork, query.size, &parts).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hls_part_names_parse() {
        assert_eq!(hls_part_name("init.mp4"), Some(HlsPart::Init));
        assert_eq!(hls_part_name("seg-12.m4s"), Some(HlsPart::Segment(12)));
        assert_eq!(hls_part_name("seg-x.m4s"), None);
        assert_eq!(hls_part_name("../etc/passwd"), None);
    }

    #[test]
    fn job_ids_are_plain() {
        assert!(job_id("abc-123_x").is_ok());
        assert!(job_id("a/b").is_err());
        assert!(job_id("").is_err());
    }

    #[test]
    fn unknown_formats_are_dropped_from_the_client_profile() -> Result<(), ApiError> {
        let client = crate::wire::playback::ClientProfile {
            direct: vec![
                crate::wire::playback::DirectPlay {
                    container: String::from("FLAC"),
                    codecs: vec![String::from("flac"), String::from("dts")],
                },
                crate::wire::playback::DirectPlay {
                    container: String::from("mkv"),
                    codecs: vec![],
                },
            ],
            hls: true,
            max_bitrate_kbps: Some(320),
            preferred_profile: Some(String::from("aac-256")),
            allow_original_fallback: true,
        };
        let profile = client_profile(&client)?;
        assert_eq!(profile.direct.len(), 1);
        assert_eq!(profile.direct[0].codecs, vec![Codec::Flac]);
        assert_eq!(profile.preferred_profile, Some(Profile::Aac256));
        Ok(())
    }

    #[test]
    fn mt_is_carried_to_segments() -> Result<(), http::Error> {
        let (parts, ()) = http::Request::builder()
            .uri("/api/v1/transcodes/j/index.m3u8?x=1&mt=abc.def")
            .body(())?
            .into_parts();
        assert_eq!(media_token_suffix(&parts), "?mt=abc.def");
        Ok(())
    }
}
