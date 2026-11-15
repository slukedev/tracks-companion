//! Resumable uploads into the uploads library (api.md section 14). The importer itself is the
//! scanner's, reached through `ScanControl::process_upload`.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path as FsPath, PathBuf};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http::{HeaderMap, Method, StatusCode, header};
use trackside_auth::extract::Authed;
use trackside_auth::{Principal, Scopes};
use trackside_core::records::{ImportSource, ImportState};
use trackside_core::repo::imports::{self, ImportRow, NewImport};
use trackside_core::repo::libraries;
use trackside_core::repo::settings::{self as stored, keys};
use trackside_core::{ImportId, LibraryId, PublicId};

use super::{idempotency, read, settings, write};
use crate::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::extract::{json_with, parse_json, path_id};
use crate::wire::social::{NewUpload, Upload, UploadProgress};

const WRITE: u8 = Scopes::WRITE.bits();
/// Uploads one user may have receiving at once.
pub const RECEIVING_PER_USER: usize = 8;
/// Free space the data volume keeps after an upload is staged.
const STAGING_RESERVE: u64 = 512 << 20;
/// The most one `PUT` may carry.
pub const CHUNK_MAX: u64 = 16 << 20;

pub(super) fn routes(_state: &ApiState) -> Router<ApiState> {
    let chunk_limit = usize::try_from(CHUNK_MAX).unwrap_or(usize::MAX) + 4096;
    Router::new()
        .route("/uploads", post(create))
        .route(
            "/uploads/{id}",
            get(status)
                .put(receive)
                .layer(DefaultBodyLimit::max(chunk_limit))
                .delete(abandon),
        )
        .route("/uploads/{id}/complete", post(complete))
}

fn upload_body(row: &ImportRow) -> Upload {
    let receiving = row.state == ImportState::Receiving;
    Upload {
        id: row.id.to_string(),
        state: row.state.as_str().to_owned(),
        received: receiving.then_some(row.received_bytes),
        chunk_max: receiving.then_some(CHUNK_MAX),
        song_id: row.song.as_ref().map(ToString::to_string),
        error: row.error.clone(),
    }
}

/// Uploads need the setting and the user's permission, checked on every request so a
/// permission taken away stops an upload already under way.
async fn may_upload(state: &ApiState, principal: &Principal) -> ApiResult<()> {
    if !settings::effective_bool(state, "uploads.enabled").await {
        return Err(ApiError::not_found());
    }
    if !(principal.access.is_admin() || principal.permissions.can_upload) {
        return Err(ApiError::forbidden());
    }
    Ok(())
}

fn insufficient_storage() -> ApiError {
    ApiError::new(
        StatusCode::INSUFFICIENT_STORAGE,
        "insufficient_storage",
        "The data volume is full.",
    )
}

/// Refuses a new upload when the user already has too many receiving, or the staging
/// volume would not keep its reserve.
async fn check_room(state: &ApiState, principal: &Principal, size: u64) -> ApiResult<()> {
    let filter = imports::ImportFilter {
        user: Some(principal.access.user),
        state: Some(ImportState::Receiving),
        cursor: None,
        limit: 500,
    };
    let receiving = read(state, move |c| imports::list(c, &filter)).await?;
    if receiving.items.len() >= RECEIVING_PER_USER {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            "Finish or cancel an upload before starting another.",
        ));
    }
    let pending: u64 = receiving
        .items
        .iter()
        .map(|r| {
            r.expected_bytes
                .unwrap_or(0)
                .saturating_sub(r.received_bytes)
        })
        .sum();
    let dir = state.config.data_dir.clone();
    let free = tokio::task::spawn_blocking(move || {
        trackside_media::FreeSpace::available(&trackside_media::FsFreeSpace, &dir)
    })
    .await
    .map_err(|e| ApiError::internal(&e))?;
    match free {
        Ok(free) if free < size.saturating_add(pending).saturating_add(STAGING_RESERVE) => {
            Err(insufficient_storage())
        }
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::debug!(error = %e, "free space unknown; upload allowed");
            Ok(())
        }
    }
}

/// Uploads need the setting, the user's permission, and a writable uploads library they see.
async fn target_library(
    state: &ApiState,
    principal: &Principal,
) -> ApiResult<trackside_core::LibraryKey> {
    may_upload(state, principal).await?;
    let access = principal.access.clone();
    let library = read(state, move |c| {
        let Some(pid) = stored::get::<String>(c, keys::UPLOADS_LIBRARY)? else {
            return Ok(None);
        };
        let Ok(id) = LibraryId::parse(&pid) else {
            return Ok(None);
        };
        libraries::get(c, &access, &id)
    })
    .await?;
    match library {
        Some(library) if library.writable && library.enabled => Ok(library.key),
        _ => Err(ApiError::conflict(
            "no_uploads_library",
            "There is no writable uploads library you can see.",
        )),
    }
}

fn staging_dir(state: &ApiState) -> PathBuf {
    state.config.data_dir.join("uploads").join("staging")
}

/// Only file names with an audio extension the scanner reads are accepted.
pub fn accepted_name(name: &str, extensions: &[String]) -> bool {
    let name = name.trim();
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && !name.contains(['/', '\\', '\0'])
        && extensions.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

async fn create(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let request: NewUpload = parse_json(&body)?;
    let user = principal.access.user;
    let state_ref = &state;
    let principal_ref = &principal;
    idempotency::once(
        &state,
        user,
        &headers,
        &method,
        uri.path(),
        &body,
        || async move {
            let target = target_library(state_ref, principal_ref).await?;
            if request.size > state_ref.config.uploads.max_file {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "too_large",
                    "The file is over the upload limit.",
                ));
            }
            if !accepted_name(&request.file_name, &state_ref.config.scanner.extensions) {
                return Err(ApiError::new(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "unsupported_media_type",
                    "Only audio files can be uploaded.",
                ));
            }
            check_room(state_ref, principal_ref, request.size).await?;
            let dir = staging_dir(state_ref);
            let path = dir.join(format!("{}.part", uuid::Uuid::new_v4().simple()));
            let create_path = path.clone();
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                std::fs::create_dir_all(&dir)?;
                std::fs::File::create(&create_path).map(drop)
            })
            .await
            .map_err(|e| ApiError::internal(&e))?
            .map_err(|e| ApiError::internal(&e))?;
            let new = NewImport {
                user: Some(user),
                source: ImportSource::Upload,
                original_name: request.file_name.trim().to_owned(),
                staged_path: Some(path),
                expected_bytes: Some(request.size),
                target_library: Some(target),
            };
            let row = write(state_ref, move |tx| imports::create(tx, &new)).await?;
            Ok((
                StatusCode::CREATED,
                serde_json::to_value(upload_body(&row))?,
            ))
        },
    )
    .await
}

async fn own_import(state: &ApiState, principal: &Principal, raw: &str) -> ApiResult<ImportRow> {
    let id: ImportId = path_id(raw)?;
    let row = read(state, move |c| imports::get(c, &id))
        .await?
        .ok_or_else(ApiError::not_found)?;
    if row.user != Some(principal.access.user) {
        return Err(ApiError::not_found());
    }
    Ok(row)
}

/// `bytes <start>-<end>/<size>`.
pub fn content_range(headers: &HeaderMap) -> Option<(u64, u64, u64)> {
    let text = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let rest = text.trim().strip_prefix("bytes ")?;
    let (range, size) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, size) = (start.parse().ok()?, end.parse().ok()?, size.parse().ok()?);
    (start <= end && end < size).then_some((start, end, size))
}

fn write_at(path: &FsPath, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(bytes)?;
    file.set_len(offset + bytes.len() as u64)?;
    file.sync_data()
}

async fn receive(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let row = own_import(&state, &principal, &id).await?;
    may_upload(&state, &principal).await?;
    if row.state != ImportState::Receiving {
        return Err(ApiError::invalid_state(
            "The upload is no longer receiving.",
        ));
    }
    let (start, end, size) = content_range(&headers).ok_or_else(|| {
        ApiError::bad_field(
            "Content-Range",
            "Send Content-Range: bytes <start>-<end>/<size>.",
        )
    })?;
    if Some(size) != row.expected_bytes {
        return Err(ApiError::bad_field(
            "Content-Range",
            "The size does not match the upload.",
        ));
    }
    if start != row.received_bytes {
        return Err(
            ApiError::conflict("offset", "The chunk does not start where the upload is.")
                .with_details(
                    serde_json::json!({ "reason": "offset", "received": row.received_bytes }),
                ),
        );
    }
    let length = end - start + 1;
    if length > CHUNK_MAX || body.len() as u64 != length {
        return Err(ApiError::bad_field(
            "Content-Range",
            "The body length does not match the range.",
        ));
    }
    let path = row
        .staged_path
        .clone()
        .ok_or_else(|| ApiError::invalid_state("The upload has no staging file."))?;
    tokio::task::spawn_blocking(move || write_at(&path, start, &body))
        .await
        .map_err(|e| ApiError::internal(&e))?
        .map_err(|e| {
            if e.raw_os_error() == Some(28) {
                insufficient_storage()
            } else {
                ApiError::internal(&e)
            }
        })?;
    let received = end + 1;
    let import = row.id.clone();
    write(&state, move |tx| imports::progress(tx, &import, received)).await?;
    Ok(json_with(StatusCode::OK, &UploadProgress { received }))
}

async fn complete(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let row = own_import(&state, &principal, &id).await?;
    if row.state != ImportState::Receiving {
        return Ok(json_with(StatusCode::ACCEPTED, &upload_body(&row)));
    }
    may_upload(&state, &principal).await?;
    if Some(row.received_bytes) != row.expected_bytes {
        return Err(
            ApiError::conflict("incomplete", "Not every byte has arrived.").with_details(
                serde_json::json!({ "reason": "incomplete", "received": row.received_bytes }),
            ),
        );
    }
    let import = row.id.clone();
    let staged = write(&state, move |tx| {
        imports::finish(tx, &import, ImportState::Staged, None, None)
    })
    .await?;
    let scan = std::sync::Arc::clone(&state.scan);
    let import = staged.id.clone();
    tokio::spawn(async move {
        if let Err(e) = scan.process_upload(&import).await {
            tracing::warn!(error = %e, import = %import, "upload import failed");
        }
    });
    Ok(json_with(StatusCode::ACCEPTED, &upload_body(&staged)))
}

async fn status(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let row = own_import(&state, &principal, &id).await?;
    Ok(json_with(StatusCode::OK, &upload_body(&row)))
}

async fn abandon(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let row = own_import(&state, &principal, &id).await?;
    if row.state != ImportState::Receiving {
        return Err(ApiError::invalid_state(
            "Only an upload still receiving can be abandoned.",
        ));
    }
    if let Some(path) = row.staged_path.clone() {
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await;
    }
    let import = row.id.clone();
    write(&state, move |tx| imports::delete(tx, &import)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn content_ranges_parse() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_static("bytes 0-99/1000"),
        );
        assert_eq!(content_range(&headers), Some((0, 99, 1000)));
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_static("bytes 10-5/1000"),
        );
        assert_eq!(content_range(&headers), None);
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_static("bytes 0-1000/1000"),
        );
        assert_eq!(content_range(&headers), None);
    }

    #[test]
    fn only_audio_names_are_accepted() {
        let extensions = vec![String::from("flac"), String::from("mp3")];
        assert!(accepted_name("01 Halo.FLAC", &extensions));
        assert!(!accepted_name("notes.txt", &extensions));
        assert!(!accepted_name("../x.flac", &extensions));
        assert!(!accepted_name(".flac", &extensions));
    }

    #[test]
    fn chunks_are_written_at_their_offset() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("u.part");
        std::fs::File::create(&path)?;
        write_at(&path, 0, b"abc")?;
        write_at(&path, 3, b"def")?;
        assert_eq!(std::fs::read(&path)?, b"abcdef");
        Ok(())
    }
}
