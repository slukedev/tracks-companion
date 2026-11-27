//! The admin endpoints the dashboard uses (api.md section 16). All need an admin's session;
//! the extractor checks CSRF on changes. Every change is written to the audit log.

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde::Deserialize;
use serde_json::Value;
use trackside_auth::extract::{AdminSession, ClientIp};
use trackside_auth::{KeyKind, NewInvite as AuthInvite, NewUser as AuthUser};
use trackside_core::config::Secret;
use trackside_core::control::{
    AcquisitionFilter, AcquisitionStateFilter, LibraryScanStatus, ProviderChange, ScanKind,
    ScanTrigger, WatchMode,
};
use trackside_core::records::{ImportState, Library, ScanCounts, ScanRun, ScanRunKind, ScanState};
use trackside_core::repo::audit::{self, AuditFilter};
use trackside_core::repo::imports::{self, ImportFilter};
use trackside_core::repo::invites::{self, InviteRow};
use trackside_core::repo::libraries::{self, LibraryPatch, NewLibrary};
use trackside_core::repo::settings::{self as stored, keys};
use trackside_core::repo::users::{self, User, UserPatch};
use trackside_core::repo::{devices, health, keys as key_repo, scan_runs, sessions};
use trackside_core::{
    Access, AcquisitionId, DbError, DeviceId, InviteId, KeyId, LibraryId, LibraryKey, Millis,
    Permissions, Role, ScanRunId, UserId, UserKey,
};

use super::account::{Https, cookie_value, sha256, svg_response};
use super::{
    api_url, audit as audit_log, candidate_urls, read, require_recent_auth, settings, write,
};
use crate::error::{ApiError, ApiResult};
use crate::extract::{Json, OptionalJson, Query, body_id, json_with, path_id};
use crate::wire::Time;
use crate::wire::admin::{
    AcquirePatch, AdminAcquisitions, AdminDevice, AdminDevices, AdminImport, AdminImports,
    AdminInvite, AdminInvites, AdminKey, AdminKeys, AdminLibraries, AdminLibrary,
    AdminSession as SessionRecord, AdminSessions, AdminUser, AdminUsers, Audit, AuditEntry,
    Backups, DuplicateGroup, DuplicateSong, Duplicates, LibraryIds, LibraryProblems,
    OverviewAcquisition, OverviewLibrary, OverviewUsers, PermissionsPatch, ProviderPatch,
    ProviderTestRequest, ProviderTestResult, Providers, Quarantined, ResetLink, ScanDetail,
    ScanError, ScanRequest, Scans, ServerUrlStatus, ServerUrls, Setting, SignOut, TagProblem,
    Targets, UnreadableFolder, UserCounts, UserRef,
};
use crate::wire::auth::{ApproveRequest, Approved, ApprovedDevice, UrlKind};
use crate::{ApiState, OpsError, map};

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/overview", get(overview))
        .route("/users", get(list_users).post(create_user))
        .route(
            "/users/{id}",
            get(get_user).patch(patch_user).delete(delete_user),
        )
        .route("/users/{id}/libraries", put(set_user_libraries))
        .route("/users/{id}/password-reset", post(password_reset))
        .route("/users/{id}/sign-out", post(sign_out))
        .route("/invites", get(list_invites).post(create_invite))
        .route("/invites/{id}", delete(revoke_invite))
        .route("/invites/{id}/qr.svg", get(invite_qr))
        .route("/libraries", get(list_libraries).post(create_library))
        .route(
            "/libraries/{id}",
            patch(patch_library).delete(delete_library),
        )
        .route("/libraries/{id}/scan", post(start_scan))
        .route("/libraries/{id}/scans", get(list_scans))
        .route("/libraries/{id}/problems", get(library_problems))
        .route("/scans/{id}", get(scan_detail))
        .route("/duplicates", get(duplicates))
        .route("/settings", get(get_settings).patch(patch_settings))
        .route("/settings/targets", put(set_targets))
        .route("/server/urls", get(server_urls))
        .route("/pairing/approve", post(approve_pairing))
        .route("/devices", get(list_devices))
        .route("/devices/{id}", delete(revoke_device))
        .route("/api-keys", get(list_keys))
        .route("/api-keys/{id}", delete(revoke_key))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", delete(end_session))
        .route("/backups", get(list_backups).post(create_backup))
        .route(
            "/backups/{name}",
            get(download_backup).delete(delete_backup),
        )
        .route("/audit", get(audit_entries))
        .route("/imports", get(list_imports))
        .route("/acquire", patch(patch_acquire))
        .route("/acquire/providers", get(providers))
        .route("/acquire/providers/{name}", patch(patch_provider))
        .route("/acquire/providers/{name}/test", post(test_provider))
        .route("/acquire/providers/ytdlp/update", post(update_ytdlp))
        .route("/acquisitions", get(list_acquisitions))
        .route("/acquisitions/{id}", delete(cancel_acquisition))
        .route("/acquisitions/{id}/retry", post(retry_acquisition))
}

fn system() -> Access {
    Access::system()
}

/// Where invite and reset links point: the configured public URL when there is one, else
/// the address this request reached when it is one the server knows (one of its own
/// addresses, or the host a trusted proxy names), else the first detected address. A raw
/// `Host` header is never trusted on its own, since anyone can send one.
async fn origin(state: &ApiState, parts: &http::request::Parts) -> String {
    let candidates = super::candidate_urls(state).await;
    if let Some(public) = candidates.iter().find(|u| u.kind == UrlKind::Public) {
        return public.url.clone();
    }
    let https = parts.extensions.get::<Https>().is_some();
    let forwarded = state.auth.trusted_forwarded(parts, "x-forwarded-host");
    let host = forwarded
        .or_else(|| {
            parts
                .headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
        })
        .map(str::to_ascii_lowercase)
        .filter(|h| !h.is_empty() && !h.contains(['/', ' ', '@', '\\']));
    let known = |host: &str| {
        candidates
            .iter()
            .any(|u| authority_of(&u.url).is_some_and(|a| a.eq_ignore_ascii_case(host)))
    };
    match host {
        Some(host) if forwarded.is_some() || known(&host) => format!(
            "{}://{host}{}",
            if https { "https" } else { "http" },
            state.server.base_path
        ),
        _ => candidates
            .first()
            .map_or_else(|| state.server.base_path.clone(), |u| u.url.clone()),
    }
}

/// `host:port` of a URL such as `http://192.168.1.10:4533/music`.
fn authority_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    Some(rest.split(['/', '?', '#']).next().unwrap_or(rest))
}

// Overview

pub fn overview_library(
    library: &Library,
    status: Option<&LibraryScanStatus>,
    available: bool,
) -> OverviewLibrary {
    OverviewLibrary {
        id: library.id.to_string(),
        name: library.name.clone(),
        state: map::library_state(library.available && available),
        song_count: library.song_count,
        last_scan: status.and_then(|s| s.last_run.as_ref()).map(map::scan_run),
        scanning: status.is_some_and(|s| s.scanning.is_some()),
    }
}

/// Problems the libraries have: offline, or watched by polling only.
pub fn library_problems_for(
    all: &[Library],
    statuses: &[trackside_core::control::LibraryScanStatus],
    available: impl Fn(&trackside_core::LibraryId) -> bool,
) -> Vec<crate::wire::admin::Problem> {
    let mut out = Vec::new();
    for library in all {
        let status = statuses.iter().find(|s| s.library == library.id);
        if !(library.available && available(&library.id)) {
            out.push(crate::wire::admin::Problem {
                severity: String::from("warning"),
                code: String::from("library_unavailable"),
                title: short_title(&library.name, "offline"),
                message: format!("{} is unavailable.", library.name),
                help: Some(format!(
                    "Check that the disk or share is mounted at {}. Its songs stay in the library until it is back.",
                    library.root.display()
                )),
                target: Some(library_target(library)),
            });
        } else if status.is_some_and(|s| s.watching == trackside_core::control::WatchMode::Polling)
        {
            out.push(crate::wire::admin::Problem {
                severity: String::from("warning"),
                code: String::from("inotify_limit"),
                title: short_title(&library.name, "not watched live"),
                message: format!("Watching {} for changes fell back to polling.", library.name),
                help: Some(String::from(
                    "Raise fs.inotify.max_user_watches to 524288 on the host so new files appear within seconds.",
                )),
                target: Some(library_target(library)),
            });
        }
    }
    out
}

fn library_target(library: &Library) -> crate::wire::admin::ProblemTarget {
    crate::wire::admin::ProblemTarget {
        kind: String::from("library"),
        id: library.id.to_string(),
    }
}

/// "<name> <what>", keeping the title to six words by shortening a long name.
fn short_title(name: &str, what: &str) -> String {
    let room = 6usize
        .saturating_sub(what.split_whitespace().count())
        .max(1);
    let words: Vec<&str> = name.split_whitespace().collect();
    let name = if words.len() > room {
        format!("{}…", words[..room].join(" "))
    } else {
        words.join(" ")
    };
    format!("{name} {what}")
}

/// A short sentence for common audit actions.
pub fn audit_summary(action: &str, target: Option<&str>) -> Option<String> {
    let target = target.filter(|t| !t.is_empty());
    let text = match (action, target) {
        ("pairing.approve", Some(t)) => format!("Approved {t}"),
        ("invite.create", _) => String::from("Created an invite"),
        ("invite.revoke", _) => String::from("Revoked an invite"),
        ("user.create", Some(t)) => format!("Added {t}"),
        ("user.delete", Some(t)) => format!("Removed {t}"),
        ("user.libraries", _) => String::from("Changed library access"),
        ("backup.create", _) => String::from("Started a backup"),
        ("backup.delete", Some(t)) => format!("Deleted backup {t}"),
        ("backup.download", Some(t)) => format!("Downloaded backup {t}"),
        ("library.create", Some(t)) => format!("Added library {t}"),
        ("acquisition.request", Some(t)) => format!("Requested {t}"),
        ("settings.update", _) => String::from("Changed settings"),
        ("apple_music.update", _) => String::from("Saved an Apple Music key"),
        ("apple_music.remove", _) => String::from("Removed the Apple Music key"),
        _ => return None,
    };
    Some(text)
}

async fn overview(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let mut body = state.health.overview().await;
    let (all, user_count, device_count) = read(&state, |c| {
        let all = libraries::list(c, &system())?;
        let user_count = users::count(c)?;
        let device_count = devices::list(c, None)?
            .iter()
            .filter(|d| d.revoked_at.is_none())
            .count() as u64;
        Ok((all, user_count, device_count))
    })
    .await?;
    let statuses = state.scan.status();
    body.libraries = all
        .iter()
        .map(|l| {
            let status = statuses.iter().find(|s| s.library == l.id);
            overview_library(l, status, super::available(&state, &l.id))
        })
        .collect();
    body.problems
        .extend(library_problems_for(&all, &statuses, |id| {
            super::available(&state, id)
        }));
    body.users = OverviewUsers {
        count: user_count,
        devices: device_count,
    };
    let providers = state.acquire.providers().await;
    body.acquisition = if providers.is_empty() {
        None
    } else {
        let active = state
            .acquire
            .admin_list(AcquisitionFilter {
                user: None,
                state: Some(AcquisitionStateFilter::Active),
                cursor: None,
                limit: 500,
            })
            .await
            .map(|p| p.items.len())
            .unwrap_or_default();
        let since = state
            .db
            .clock()
            .now()
            .saturating_sub(Duration::from_hours(24));
        let today = state
            .acquire
            .admin_list(AcquisitionFilter {
                user: None,
                state: None,
                cursor: None,
                limit: 500,
            })
            .await
            .map(|p| {
                p.items
                    .iter()
                    .filter(|a| a.view.created_at >= since)
                    .count()
            })
            .unwrap_or_default();
        Some(OverviewAcquisition {
            enabled: settings::effective_bool(&state, "acquire.enabled").await,
            providers: providers.iter().map(map::provider).collect(),
            active: u32::try_from(active).unwrap_or(u32::MAX),
            today: u32::try_from(today).unwrap_or(u32::MAX),
            fetched: state.acquire.fetched_usage().map(map::fetched_usage),
        })
    };
    Ok(json_with(StatusCode::OK, &body))
}

// Users

/// What the admin pages show about everyone: library ids by key, and last activity.
struct Directory {
    library_ids: HashMap<LibraryKey, LibraryId>,
    last_seen: HashMap<UserKey, Millis>,
    users: HashMap<UserKey, User>,
}

async fn directory(state: &ApiState) -> ApiResult<Directory> {
    read(state, |c| {
        let library_ids = libraries::list(c, &system())?
            .into_iter()
            .map(|l| (l.key, l.id))
            .collect();
        let mut last_seen: HashMap<UserKey, Millis> = HashMap::new();
        for device in devices::list(c, None)? {
            let entry = last_seen.entry(device.user).or_insert(device.last_seen_at);
            *entry = (*entry).max(device.last_seen_at);
        }
        for session in sessions::list(c, None)? {
            let entry = last_seen
                .entry(session.user)
                .or_insert(session.last_seen_at);
            *entry = (*entry).max(session.last_seen_at);
        }
        let users = users::list(c)?.into_iter().map(|u| (u.key, u)).collect();
        Ok(Directory {
            library_ids,
            last_seen,
            users,
        })
    })
    .await
}

impl Directory {
    fn user_ref(&self, key: UserKey) -> Option<UserRef> {
        self.users.get(&key).map(map::user_ref)
    }

    fn username(&self, key: Option<UserKey>) -> Option<String> {
        key.and_then(|k| self.users.get(&k))
            .map(|u| u.username.clone())
    }
}

/// `AdminUser`. Admins see every library, so their `library_ids` lists all of them.
pub fn admin_user(
    user: &User,
    library_ids: Vec<String>,
    last_seen_at: Option<Millis>,
    counts: Option<UserCounts>,
) -> AdminUser {
    AdminUser {
        id: user.id.to_string(),
        username: user.username.clone(),
        display_name: user.display_name.clone(),
        role: user.role.as_str().to_owned(),
        permissions: map::permissions(user.role, &user.permissions),
        library_ids,
        created_at: user.created_at.into(),
        last_seen_at: last_seen_at.map(Time),
        disabled: user.disabled_at.is_some(),
        counts,
    }
}

async fn user_libraries(
    state: &ApiState,
    user: &User,
    directory: &Directory,
) -> ApiResult<Vec<String>> {
    if user.role == Role::Admin {
        let mut all: Vec<String> = directory
            .library_ids
            .values()
            .map(ToString::to_string)
            .collect();
        all.sort();
        return Ok(all);
    }
    let key = user.key;
    let granted = read(state, move |c| users::libraries(c, key)).await?;
    Ok(granted
        .iter()
        .filter_map(|k| directory.library_ids.get(k).map(ToString::to_string))
        .collect())
}

async fn list_users(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let directory = directory(&state).await?;
    let mut list: Vec<&User> = directory.users.values().collect();
    list.sort_by(|a, b| a.username.cmp(&b.username));
    let mut out = Vec::new();
    for user in list {
        let libraries = user_libraries(&state, user, &directory).await?;
        out.push(admin_user(
            user,
            libraries,
            directory.last_seen.get(&user.key).copied(),
            None,
        ));
    }
    Ok(json_with(StatusCode::OK, &AdminUsers { users: out }))
}

async fn find_user(state: &ApiState, raw: &str) -> ApiResult<User> {
    let id: UserId = path_id(raw)?;
    read(state, move |c| users::by_pid(c, &id))
        .await?
        .ok_or_else(ApiError::not_found)
}

async fn library_keys(
    state: &ApiState,
    ids: &[String],
    field: &'static str,
) -> ApiResult<Vec<LibraryKey>> {
    let ids = ids
        .iter()
        .map(|raw| body_id::<LibraryId>(raw, field))
        .collect::<ApiResult<Vec<_>>>()?;
    let found = read(state, move |c| {
        let mut keys = Vec::new();
        for id in &ids {
            match libraries::get(c, &system(), id)? {
                Some(library) => keys.push(library.key),
                None => return Ok(None),
            }
        }
        Ok(Some(keys))
    })
    .await?;
    found.ok_or_else(|| ApiError::bad_field(field, "An unknown library was named."))
}

fn apply_permissions(base: &Permissions, patch: Option<&PermissionsPatch>) -> Permissions {
    let Some(patch) = patch else {
        return base.clone();
    };
    Permissions {
        can_download: patch.can_download.unwrap_or(base.can_download),
        can_upload: patch.can_upload.unwrap_or(base.can_upload),
        can_request: patch.can_request.unwrap_or(base.can_request),
        legacy_subsonic: patch.legacy_subsonic.unwrap_or(base.legacy_subsonic),
        max_bitrate_kbps: patch.max_bitrate_kbps.unwrap_or(base.max_bitrate_kbps),
    }
}

fn parse_role(raw: Option<&str>) -> ApiResult<Option<Role>> {
    raw.map(|r| {
        Role::parse(r).ok_or_else(|| ApiError::bad_field("role", "role is admin or member."))
    })
    .transpose()
}

async fn create_user(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<crate::wire::admin::NewUser>,
) -> ApiResult<Response> {
    let username = body.username.trim().to_owned();
    let role = parse_role(body.role.as_deref())?.unwrap_or(Role::Member);
    if role == Role::Admin {
        require_recent_auth(&state, &principal)?;
    }
    let permissions = apply_permissions(&Permissions::default(), body.permissions.as_ref());
    let libraries = body
        .library_ids
        .iter()
        .map(|raw| body_id::<LibraryId>(raw, "library_ids"))
        .collect::<ApiResult<Vec<_>>>()?;
    let new = AuthUser {
        display_name: body
            .display_name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| username.clone()),
        username,
        password: zeroize::Zeroizing::new(body.password),
        role,
        permissions,
        libraries,
    };
    let key = state
        .auth
        .create_user(principal.access.user, new, Some(ip))
        .await?;
    let created = read(&state, move |c| users::get(c, key))
        .await?
        .ok_or_else(ApiError::not_found)?;
    let directory = directory(&state).await?;
    let libraries = user_libraries(&state, &created, &directory).await?;
    Ok(json_with(
        StatusCode::CREATED,
        &admin_user(&created, libraries, None, None),
    ))
}

async fn get_user(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let user = find_user(&state, &id).await?;
    let key = user.key;
    let counts = read(&state, move |c| {
        Ok(UserCounts {
            devices: devices::list(c, Some(key))?
                .iter()
                .filter(|d| d.revoked_at.is_none())
                .count() as u64,
            api_keys: key_repo::list(c, Some(key))?
                .iter()
                .filter(|k| k.kind != KeyKind::Device && k.revoked_at.is_none())
                .count() as u64,
            sessions: sessions::list(c, Some(key))?.len() as u64,
            cloud: trackside_cloud::store::usage_in(c, key)
                .map(|u| crate::wire::cloud::Amount {
                    items: u.items,
                    bytes: u.bytes,
                })
                .ok()
                .filter(|a| a.items > 0),
        })
    })
    .await?;
    let directory = directory(&state).await?;
    let libraries = user_libraries(&state, &user, &directory).await?;
    Ok(json_with(
        StatusCode::OK,
        &admin_user(
            &user,
            libraries,
            directory.last_seen.get(&key).copied(),
            Some(counts),
        ),
    ))
}

async fn patch_user(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
    Json(body): Json<crate::wire::admin::UserPatch>,
) -> ApiResult<Response> {
    if body.role.is_some() || body.disabled.is_some() {
        require_recent_auth(&state, &principal)?;
    }
    let user = find_user(&state, &id).await?;
    let patch = UserPatch {
        display_name: body
            .display_name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty()),
        role: parse_role(body.role.as_deref())?,
        permissions: body
            .permissions
            .as_ref()
            .map(|p| apply_permissions(&user.permissions, Some(p))),
        disabled: body.disabled,
    };
    let key = user.key;
    let updated = state
        .auth
        .update_user(Some(principal.access.user), key, patch, Some(ip))
        .await?;
    let directory = directory(&state).await?;
    let libraries = user_libraries(&state, &updated, &directory).await?;
    Ok(json_with(
        StatusCode::OK,
        &admin_user(
            &updated,
            libraries,
            directory.last_seen.get(&key).copied(),
            None,
        ),
    ))
}

async fn set_user_libraries(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
    Json(body): Json<LibraryIds>,
) -> ApiResult<Response> {
    let user = find_user(&state, &id).await?;
    let ids = body
        .library_ids
        .iter()
        .map(|raw| body_id::<LibraryId>(raw, "library_ids"))
        .collect::<ApiResult<Vec<_>>>()?;
    let key = user.key;
    state
        .auth
        .set_libraries(principal.access.user, key, ids, Some(ip))
        .await?;
    let directory = directory(&state).await?;
    let fresh = read(&state, move |c| users::get(c, key))
        .await?
        .ok_or_else(ApiError::not_found)?;
    let libraries = user_libraries(&state, &fresh, &directory).await?;
    Ok(json_with(
        StatusCode::OK,
        &admin_user(
            &fresh,
            libraries,
            directory.last_seen.get(&key).copied(),
            None,
        ),
    ))
}

async fn password_reset(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
    parts: http::request::Parts,
) -> ApiResult<Response> {
    require_recent_auth(&state, &principal)?;
    let user = find_user(&state, &id).await?;
    let reset = state
        .auth
        .create_password_reset(principal.access.user, user.key)
        .await?;
    let url = format!("{}/reset/{}", origin(&state, &parts).await, reset.token);
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "user.reset_link",
        Some(user.username.clone()),
        None,
    )
    .await;
    Ok(json_with(
        StatusCode::CREATED,
        &ResetLink {
            url,
            expires_at: reset.expires_at.into(),
        },
    ))
}

async fn sign_out(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
    OptionalJson(body): OptionalJson<SignOut>,
) -> ApiResult<Response> {
    let user = find_user(&state, &id).await?;
    state
        .auth
        .sign_out_user(
            Some(principal.access.user),
            user.key,
            body.devices,
            Some(ip),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn delete_user(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    require_recent_auth(&state, &principal)?;
    let user = find_user(&state, &id).await?;
    // Trackside Cloud data goes first, in short transactions, so the delete itself stays
    // short. Disabling the user first checks for the last admin in the same transaction and
    // keeps a concurrent demotion from making the final delete fail after the purge.
    state
        .auth
        .begin_delete_user(principal.access.user, user.key, Some(ip))
        .await?;
    trackside_cloud::store::purge_user(&state.db, user.key).await?;
    state
        .auth
        .delete_user(principal.access.user, user.key, Some(ip))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// Invites

pub fn invite_state(row: &InviteRow, now: Millis) -> &'static str {
    if row.revoked_at.is_some() {
        "revoked"
    } else if row.used_at.is_some() {
        "used"
    } else if row.expires_at <= now {
        "expired"
    } else {
        "open"
    }
}

fn admin_invite(row: &InviteRow, directory: &Directory, now: Millis) -> AdminInvite {
    AdminInvite {
        id: row.id.to_string(),
        role: row.role.as_str().to_owned(),
        permissions: map::permissions(row.role, &row.permissions),
        library_ids: row
            .libraries
            .iter()
            .filter_map(|k| directory.library_ids.get(k).map(ToString::to_string))
            .collect(),
        note: row.note.clone().unwrap_or_default(),
        created_by: directory.username(row.created_by),
        created_at: row.created_at.into(),
        expires_at: row.expires_at.into(),
        state: invite_state(row, now).to_owned(),
        used_by: directory.username(row.used_by),
        url: None,
        qr_svg_url: None,
    }
}

async fn list_invites(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let rows = read(&state, invites::list).await?;
    let directory = directory(&state).await?;
    let now = state.db.clock().now();
    Ok(json_with(
        StatusCode::OK,
        &AdminInvites {
            invites: rows
                .iter()
                .map(|r| admin_invite(r, &directory, now))
                .collect(),
        },
    ))
}

async fn create_invite(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    parts: http::request::Parts,
    Json(body): Json<crate::wire::admin::NewInvite>,
) -> ApiResult<Response> {
    let role = parse_role(body.role.as_deref())?.unwrap_or(Role::Member);
    if role == Role::Admin {
        require_recent_auth(&state, &principal)?;
    }
    let days = body.expires_in_days.unwrap_or(7).clamp(1, 90);
    let libraries = body
        .library_ids
        .iter()
        .map(|raw| body_id::<LibraryId>(raw, "library_ids"))
        .collect::<ApiResult<Vec<_>>>()?;
    library_keys(&state, &body.library_ids, "library_ids").await?;
    let permissions = apply_permissions(&Permissions::default(), body.permissions.as_ref());
    let note = body
        .note
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty());
    let issued = state
        .auth
        .create_invite(
            principal.access.user,
            AuthInvite {
                role,
                permissions: permissions.clone(),
                libraries,
                note: note.clone(),
                expires_in: Duration::from_secs(u64::from(days) * 86_400),
            },
        )
        .await?;
    let url = format!("{}/invite/{}", origin(&state, &parts).await, issued.token);
    if let Some(svg) = crate::qr::qr_svg(&url) {
        state.local.put_qr(
            format!("invite:{}", issued.id),
            None,
            svg,
            issued.expires_at,
        );
    }
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "invite.create",
        Some(role.as_str().to_owned()),
        note.map(Value::String),
    )
    .await;
    let id = issued.id.clone();
    let row = read(&state, move |c| {
        Ok(invites::list(c)?.into_iter().find(|r| r.id == id))
    })
    .await?;
    let directory = directory(&state).await?;
    let now = state.db.clock().now();
    let mut out = match row {
        Some(row) => admin_invite(&row, &directory, now),
        None => AdminInvite {
            id: issued.id.to_string(),
            role: role.as_str().to_owned(),
            permissions: map::permissions(role, &permissions),
            library_ids: body.library_ids.clone(),
            note: String::new(),
            created_by: Some(principal.username.clone()),
            created_at: now.into(),
            expires_at: issued.expires_at.into(),
            state: String::from("open"),
            used_by: None,
            url: None,
            qr_svg_url: None,
        },
    };
    out.url = Some(url);
    out.qr_svg_url = Some(api_url(
        &state,
        &format!("/admin/invites/{}/qr.svg", issued.id),
    ));
    Ok(json_with(StatusCode::CREATED, &out))
}

async fn invite_qr(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: InviteId = path_id(&id)?;
    let now = state.db.clock().now();
    state
        .local
        .qr(&format!("invite:{id}"), None, now)
        .map(svg_response)
        .ok_or_else(ApiError::not_found)
}

async fn revoke_invite(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: InviteId = path_id(&id)?;
    let target = id.clone();
    write(&state, move |tx| invites::revoke(tx, &target)).await?;
    state.local.drop_qr(&format!("invite:{id}"));
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "invite.revoke",
        Some(id.to_string()),
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// Libraries and scans

fn watching(mode: Option<WatchMode>) -> &'static str {
    match mode {
        Some(WatchMode::Events) => "events",
        Some(WatchMode::Polling) => "polling",
        Some(WatchMode::Off) | None => "off",
    }
}

pub fn admin_library(
    library: &Library,
    status: Option<&LibraryScanStatus>,
    available: bool,
    default_interval: Option<Duration>,
    targets: &Targets,
) -> AdminLibrary {
    let id = library.id.to_string();
    AdminLibrary {
        state: map::library_state(library.available && available),
        name: library.name.clone(),
        song_count: library.song_count,
        root: library.root.display().to_string(),
        enabled: library.enabled,
        writable: library.writable,
        watch: library.watch,
        quick_scan_interval_s: library
            .quick_scan_interval
            .or(default_interval)
            .map(|d| d.as_secs()),
        unavailable_reason: library.unavailable_reason.clone(),
        unavailable_since: library.unavailable_since.map(Time),
        watching: watching(status.map(|s| s.watching).or(Some(if library.watch {
            WatchMode::Events
        } else {
            WatchMode::Off
        })))
        .to_owned(),
        uploads_target: targets.uploads_library.as_deref() == Some(id.as_str()),
        acquire_target: targets.acquire_library.as_deref() == Some(id.as_str()),
        id,
    }
}

async fn targets(state: &ApiState) -> ApiResult<Targets> {
    read(state, |c| {
        Ok(Targets {
            uploads_library: stored::get::<String>(c, keys::UPLOADS_LIBRARY)?,
            acquire_library: stored::get::<String>(c, keys::ACQUIRE_LIBRARY)?,
        })
    })
    .await
}

async fn admin_libraries(state: &ApiState) -> ApiResult<Vec<AdminLibrary>> {
    let all = read(state, |c| libraries::list(c, &system())).await?;
    let statuses = state.scan.status();
    let targets = targets(state).await?;
    Ok(all
        .iter()
        .map(|l| {
            admin_library(
                l,
                statuses.iter().find(|s| s.library == l.id),
                super::available(state, &l.id),
                state.config.scanner.quick_interval,
                &targets,
            )
        })
        .collect())
}

async fn list_libraries(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    Ok(json_with(
        StatusCode::OK,
        &AdminLibraries {
            libraries: admin_libraries(&state).await?,
        },
    ))
}

/// The checks of `POST /admin/libraries`: an absolute, readable directory (writable if asked)
/// outside the data directory that overlaps no other library.
pub fn check_root(
    root: &FsPath,
    writable: bool,
    data_dir: &FsPath,
    others: &[PathBuf],
) -> Result<PathBuf, String> {
    if !root.is_absolute() {
        return Err(String::from("The folder must be an absolute path."));
    }
    let canonical =
        std::fs::canonicalize(root).map_err(|_| String::from("The folder does not exist."))?;
    if !canonical.is_dir() {
        return Err(String::from("That path is not a folder."));
    }
    std::fs::read_dir(&canonical)
        .map_err(|_| String::from("The server cannot read that folder."))?;
    if writable {
        let probe = canonical.join(format!(
            ".trackside-write-check-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&probe, b"")
            .map_err(|_| String::from("The server cannot write to that folder."))?;
        let _ = std::fs::remove_file(&probe);
    }
    let data = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    if canonical.starts_with(&data) || data.starts_with(&canonical) {
        return Err(String::from("The folder overlaps the data directory."));
    }
    for other in others {
        let other = std::fs::canonicalize(other).unwrap_or_else(|_| other.clone());
        if canonical.starts_with(&other) || other.starts_with(&canonical) {
            return Err(String::from("The folder overlaps another library."));
        }
    }
    Ok(canonical)
}

async fn create_library(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<crate::wire::admin::NewLibrary>,
) -> ApiResult<Response> {
    let name = body.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(ApiError::bad_field(
            "name",
            "name must be 1 to 100 characters.",
        ));
    }
    let existing = read(&state, |c| libraries::list(c, &system())).await?;
    let others: Vec<PathBuf> = existing.iter().map(|l| l.root.clone()).collect();
    let root = PathBuf::from(body.root.trim());
    let data_dir = state.config.data_dir.clone();
    let writable = body.writable;
    let root = tokio::task::spawn_blocking(move || check_root(&root, writable, &data_dir, &others))
        .await
        .map_err(|e| ApiError::internal(&e))?
        .map_err(|message| ApiError::bad_field("root", message))?;
    let new = NewLibrary {
        name,
        root,
        writable,
        watch: body.watch.unwrap_or(true),
        grant_all_users: body.grant_all_users,
        granted_by: Some(principal.access.user),
    };
    let created = write(&state, move |tx| libraries::create(tx, &new)).await?;
    if let Err(e) = state
        .scan
        .request_scan(&created.id, ScanKind::Full, ScanTrigger::Admin, false)
    {
        tracing::warn!(error = %e, library = %created.id, "first scan not started");
    }
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "library.create",
        Some(created.name.clone()),
        Some(Value::String(created.root.display().to_string())),
    )
    .await;
    let targets = targets(&state).await?;
    let body = admin_library(
        &created,
        None,
        true,
        state.config.scanner.quick_interval,
        &targets,
    );
    state.acquire.targets_changed().await;
    Ok(json_with(StatusCode::CREATED, &body))
}

async fn find_library(state: &ApiState, raw: &str) -> ApiResult<Library> {
    let id: LibraryId = path_id(raw)?;
    read(state, move |c| libraries::get(c, &system(), &id))
        .await?
        .ok_or_else(ApiError::not_found)
}

async fn patch_library(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
    Json(body): Json<crate::wire::admin::LibraryPatch>,
) -> ApiResult<Response> {
    if body.root.is_some() {
        return Err(ApiError::bad_field(
            "root",
            "A library's folder cannot change; add a new library.",
        ));
    }
    let library = find_library(&state, &id).await?;
    let patch = LibraryPatch {
        name: body
            .name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty()),
        enabled: body.enabled,
        writable: body.writable,
        watch: body.watch,
        quick_scan_interval: body
            .quick_scan_interval_s
            .map(|s| s.map(|s| Duration::from_secs(s.max(60)))),
    };
    let key = library.key;
    let updated = write(&state, move |tx| libraries::update(tx, key, &patch)).await?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "library.update",
        Some(updated.name.clone()),
        None,
    )
    .await;
    let statuses = state.scan.status();
    let targets = targets(&state).await?;
    let body = admin_library(
        &updated,
        statuses.iter().find(|s| s.library == updated.id),
        super::available(&state, &updated.id),
        state.config.scanner.quick_interval,
        &targets,
    );
    Ok(json_with(StatusCode::OK, &body))
}

async fn delete_library(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let library = find_library(&state, &id).await?;
    let key = library.key;
    write(&state, move |tx| libraries::delete(tx, key)).await?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "library.delete",
        Some(library.name.clone()),
        None,
    )
    .await;
    state.acquire.targets_changed().await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn start_scan(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
    OptionalJson(body): OptionalJson<ScanRequest>,
) -> ApiResult<Response> {
    let library = find_library(&state, &id).await?;
    let kind = match body.kind.as_deref().unwrap_or("quick") {
        "quick" => ScanKind::Quick,
        "full" => ScanKind::Full,
        _ => return Err(ApiError::bad_field("kind", "kind is quick or full.")),
    };
    let run = state
        .scan
        .request_scan(&library.id, kind, ScanTrigger::Admin, body.force_empty)?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "library.scan",
        Some(library.name.clone()),
        Some(Value::String(String::from(if kind == ScanKind::Full {
            "full"
        } else {
            "quick"
        }))),
    )
    .await;
    let lookup = run.clone();
    let stored = read(&state, move |c| scan_runs::get(c, &lookup))
        .await
        .ok()
        .flatten();
    let run = stored.unwrap_or_else(|| ScanRun {
        id: run,
        library: library.id.clone(),
        kind: if kind == ScanKind::Full {
            ScanRunKind::Full
        } else {
            ScanRunKind::Quick
        },
        trigger: ScanTrigger::Admin,
        state: ScanState::Running,
        started_at: state.db.clock().now(),
        finished_at: None,
        counts: ScanCounts::default(),
        message: None,
    });
    Ok(json_with(StatusCode::ACCEPTED, &map::scan_run(&run)))
}

#[derive(Debug, Deserialize)]
struct LimitQuery {
    limit: Option<u32>,
}

async fn list_scans(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<LimitQuery>,
) -> ApiResult<Response> {
    let library = find_library(&state, &id).await?;
    let limit = query.limit.unwrap_or(20).clamp(1, 200);
    let key = library.key;
    let runs = read(&state, move |c| scan_runs::list(c, key, limit)).await?;
    Ok(json_with(
        StatusCode::OK,
        &Scans {
            scans: runs.iter().map(map::scan_run).collect(),
        },
    ))
}

pub fn scan_detail_body(run: &ScanRun, errors: &[scan_runs::ScanErrorRow]) -> ScanDetail {
    let base = map::scan_run(run);
    ScanDetail {
        id: base.id,
        library_id: base.library_id,
        kind: base.kind,
        trigger: base.trigger,
        state: base.state,
        started_at: base.started_at,
        finished_at: base.finished_at,
        folders_seen: base.folders_seen,
        files_seen: base.files_seen,
        added: base.added,
        updated: base.updated,
        moved: base.moved,
        missing: base.missing,
        errors: errors
            .iter()
            .map(|e| ScanError {
                path: e.path_display.clone(),
                error: e.error.clone(),
            })
            .collect(),
        message: base.message,
    }
}

async fn scan_detail(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: ScanRunId = path_id(&id)?;
    let (run, errors) = read(&state, move |c| {
        let run = scan_runs::get(c, &id)?.ok_or(DbError::NotFound)?;
        let errors = scan_runs::errors(c, &id, 1000)?;
        Ok((run, errors))
    })
    .await?;
    Ok(json_with(StatusCode::OK, &scan_detail_body(&run, &errors)))
}

#[derive(Debug, Deserialize)]
struct PageQuery {
    limit: Option<u32>,
    cursor: Option<String>,
}

async fn library_problems(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let library = find_library(&state, &id).await?;
    let key = library.key;
    let (tags, quarantined, unreadable) = read(&state, move |c| {
        Ok((
            health::tag_problems(c, Some(key), None, 500)?,
            scan_runs::quarantined(c, key)?,
            health::unreadable_folders(c, Some(key), None, 500)?,
        ))
    })
    .await?;
    Ok(json_with(
        StatusCode::OK,
        &LibraryProblems {
            tag_problems: tags
                .items
                .iter()
                .map(|t| TagProblem {
                    song_id: t.song.id.to_string(),
                    path: t.rel_path_display.clone(),
                    problem: t.problem.clone(),
                })
                .collect(),
            quarantined: quarantined
                .iter()
                .map(|q| Quarantined {
                    path: String::from_utf8_lossy(&q.rel_path).into_owned(),
                    error: q.reason.clone(),
                    at: q.at.into(),
                })
                .collect(),
            unreadable_folders: unreadable
                .items
                .iter()
                .map(|f| UnreadableFolder {
                    path: String::from_utf8_lossy(&f.rel_path).into_owned(),
                    since: f.last_scanned_at.map(Time),
                })
                .collect(),
        },
    ))
}

async fn duplicates(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Response> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let cursor = query.cursor;
    let (page, roots) = read(&state, move |c| {
        let page = health::duplicate_groups(c, None, cursor.as_deref(), limit)?;
        let roots: HashMap<LibraryId, PathBuf> = libraries::list(c, &system())?
            .into_iter()
            .map(|l| (l.id, l.root))
            .collect();
        let mut paths = HashMap::new();
        for group in &page.items {
            for song in &group.songs {
                if let Some(file) = trackside_core::repo::songs::file(c, &system(), &song.id)? {
                    paths.insert(song.id.clone(), file.path);
                }
            }
        }
        Ok((page, (roots, paths)))
    })
    .await?;
    let (roots, paths) = roots;
    let relative = |song: &trackside_core::records::Song| {
        let path = paths.get(&song.id);
        let root = roots.get(&song.library);
        match (path, root) {
            (Some(path), Some(root)) => path
                .strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string(),
            (Some(path), None) => path.display().to_string(),
            _ => String::new(),
        }
    };
    Ok(json_with(
        StatusCode::OK,
        &Duplicates {
            groups: page
                .items
                .iter()
                .map(|g| DuplicateGroup {
                    songs: g
                        .songs
                        .iter()
                        .map(|song| DuplicateSong {
                            id: song.id.to_string(),
                            title: song.title.clone(),
                            artist: song.artist.clone(),
                            library_id: song.library.to_string(),
                            path: relative(song),
                        })
                        .collect(),
                })
                .collect(),
            next_cursor: page.next,
        },
    ))
}

async fn set_targets(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<Targets>,
) -> ApiResult<Response> {
    for (field, value) in [
        ("uploads_library", &body.uploads_library),
        ("acquire_library", &body.acquire_library),
    ] {
        if let Some(raw) = value {
            let library = find_library(&state, raw)
                .await
                .map_err(|_| ApiError::bad_field(field, "An unknown library was named."))?;
            if !library.writable {
                return Err(ApiError::bad_field(field, "That library is not writable."));
            }
        }
    }
    let wanted = body.clone();
    write(&state, move |tx| {
        for (key, value) in [
            (keys::UPLOADS_LIBRARY, &wanted.uploads_library),
            (keys::ACQUIRE_LIBRARY, &wanted.acquire_library),
        ] {
            match value {
                Some(id) => stored::put(tx, key, &id.to_ascii_lowercase())?,
                None => stored::delete(tx, key)?,
            }
        }
        Ok(())
    })
    .await?;
    state.acquire.targets_changed().await;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "settings.targets",
        None,
        serde_json::to_value(&body).ok(),
    )
    .await;
    Ok(json_with(StatusCode::OK, &targets(&state).await?))
}

// Settings

async fn settings_body(state: &ApiState) -> ApiResult<crate::wire::admin::Settings> {
    let saved: HashMap<String, Value> = read(state, stored::all).await?.into_iter().collect();
    let mut out = crate::wire::admin::Settings::new();
    for key in settings::KEYS {
        let locked = settings::locked(state, key.name);
        let dashboard = saved.get(key.name).filter(|v| !v.is_null() && !locked);
        let (value, source) = match dashboard {
            Some(_) if key.kind == settings::Kind::Secret => (Value::Bool(true), "dashboard"),
            Some(value) => (value.clone(), "dashboard"),
            None => (
                (key.value)(&state.config),
                if locked { "config" } else { "default" },
            ),
        };
        out.insert(
            key.name.to_owned(),
            Setting {
                value,
                source: source.to_owned(),
                editable: !locked,
            },
        );
    }
    Ok(out)
}

async fn get_settings(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    Ok(json_with(StatusCode::OK, &settings_body(&state).await?))
}

async fn patch_settings(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> ApiResult<Response> {
    if body.keys().any(|k| {
        k.starts_with("auth.")
            || settings::key(k).is_some_and(|key| key.kind == settings::Kind::Secret)
    }) {
        require_recent_auth(&state, &principal)?;
    }
    let mut changes = Vec::new();
    for (name, value) in &body {
        let key = settings::key(name)
            .ok_or_else(|| ApiError::bad_field(name, "Not a setting the dashboard can change."))?;
        if settings::locked(&state, key.name) {
            return Err(ApiError::conflict(
                "set_by_config",
                format!("{name} is set by the configuration."),
            )
            .with_details(serde_json::json!({ "reason": "set_by_config", "field": name })));
        }
        let value = if value.is_null() {
            Value::Null
        } else {
            settings::validate(key.kind, value)
                .map_err(|m| ApiError::bad_field(name, format!("{name} {m}.")))?
        };
        changes.push((key.name, value));
    }
    let mut to_store = changes.clone();
    for (name, value) in &mut to_store {
        if let (Some(plain), Some(settings::Kind::Secret)) =
            (value.as_str(), settings::key(name).map(|k| k.kind))
        {
            *value = settings::sealed_value(&state.auth, name, plain)
                .map_err(|e| ApiError::internal(&e))?;
        }
    }
    write(&state, move |tx| {
        for (name, value) in &to_store {
            if value.is_null() {
                stored::delete(tx, name)?;
            } else {
                stored::put(tx, name, value)?;
            }
        }
        Ok(())
    })
    .await?;
    apply_acquisition_settings(&state, &changes).await;
    if changes.iter().any(|(n, _)| *n == "server.public_urls") {
        super::addresses::announce(&state).await;
    }
    if changes.iter().any(|(n, _)| *n == settings::LEGACY_SUBSONIC) {
        let on = settings::effective_bool(&state, settings::LEGACY_SUBSONIC).await;
        state.auth.set_legacy_subsonic(on);
    }
    if let Some((_, key)) = changes.iter().find(|(n, _)| *n == settings::LASTFM_API_KEY) {
        state.enrich.set_key(key.as_str().map(Secret::new));
    }
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "settings.update",
        Some(
            changes
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        None,
    )
    .await;
    Ok(json_with(StatusCode::OK, &settings_body(&state).await?))
}

/// The effective limits on fetched songs: the dashboard's, else the configuration's. A value
/// set back to null returns to the configuration's.
async fn fetched_limits(state: &ApiState) -> ProviderChange {
    let keep = match settings::effective(state, settings::FETCHED_KEEP).await {
        Some(v) => v.as_u64().and_then(|n| u32::try_from(n).ok()),
        None => None,
    };
    let max_bytes = match settings::effective(state, settings::FETCHED_MAX_BYTES).await {
        Some(v) => v.as_str().and_then(trackside_core::config::parse_size),
        None => None,
    };
    ProviderChange::Fetched {
        keep: keep.unwrap_or(state.config.acquire.fetched_keep),
        max_bytes: max_bytes.unwrap_or(state.config.acquire.fetched_max_bytes),
    }
}

/// Acquisition settings take effect through the acquirer as well as the settings table.
async fn apply_acquisition_settings(state: &ApiState, changes: &[(&'static str, Value)]) {
    let get = |name: &str| changes.iter().find(|(n, _)| *n == name).map(|(_, v)| v);
    let count = |name: &str| {
        get(name)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    let global = ProviderChange::Global {
        enabled: get("acquire.enabled").and_then(Value::as_bool),
        instant_play: get("acquire.instant_play").and_then(Value::as_bool),
        member_daily: count("acquire.member_daily"),
        member_active: count("acquire.member_active"),
        global_daily: count("acquire.global_daily"),
        prefer: get("acquire.prefer")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            }),
    };
    let mut wanted = Vec::new();
    if changes.iter().any(|(n, _)| {
        n.starts_with("acquire.")
            && !n.starts_with("acquire.ytdlp")
            && !n.starts_with("acquire.fetched_")
            && *n != "acquire.library"
    }) {
        wanted.push(global);
    }
    if changes
        .iter()
        .any(|(n, _)| *n == settings::FETCHED_KEEP || *n == settings::FETCHED_MAX_BYTES)
    {
        wanted.push(fetched_limits(state).await);
    }
    if let Some(enabled) = get("acquire.ytdlp.enabled").and_then(Value::as_bool) {
        wanted.push(ProviderChange::Provider {
            name: String::from("ytdlp"),
            enabled: Some(enabled),
            concurrency: None,
        });
    }
    for change in wanted {
        if let Err(e) = state.acquire.configure(change).await {
            tracing::warn!(error = %e, "acquisition setting not applied");
        }
    }
}

async fn server_urls(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let detected: Vec<String> = state.server.urls.get().into_iter().map(|u| u.url).collect();
    let all = candidate_urls(&state).await;
    let checks = all.iter().map(|u| {
        let ops = std::sync::Arc::clone(&state.ops);
        let url = u.url.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(3), ops.check_url(&url))
                .await
                .unwrap_or(false)
        }
    });
    let reachable = futures_util::future::join_all(checks).await;
    let now = state.db.clock().now();
    let urls = all
        .into_iter()
        .zip(reachable)
        .map(|(u, reachable)| ServerUrlStatus {
            source: if detected.contains(&u.url) {
                "detected"
            } else {
                "configured"
            }
            .to_owned(),
            url: u.url,
            kind: u.kind,
            tls: u.tls,
            reachable,
            checked_at: now.into(),
        })
        .collect();
    Ok(json_with(StatusCode::OK, &ServerUrls { urls }))
}

async fn approve_pairing(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<ApproveRequest>,
) -> ApiResult<Response> {
    let code: String = body.code.chars().filter(char::is_ascii_digit).collect();
    if code.len() != 6 {
        return Err(ApiError::bad_field("code", "The code has six digits."));
    }
    let approved = state
        .auth
        .approve_pair_request(principal.access.user, &code)
        .await?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "pairing.approve",
        approved.device_name.clone(),
        approved.platform.clone().map(Value::String),
    )
    .await;
    Ok(json_with(
        StatusCode::OK,
        &Approved {
            device: ApprovedDevice {
                name: approved.device_name,
                platform: approved.platform,
            },
        },
    ))
}

// Devices, keys, sessions

async fn list_devices(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let list = read(&state, |c| devices::list(c, None)).await?;
    let directory = directory(&state).await?;
    let devices = list
        .iter()
        .filter(|d| d.revoked_at.is_none())
        .filter_map(|d| {
            Some(AdminDevice {
                id: d.id.to_string(),
                name: d.name.clone(),
                platform: d.platform.clone(),
                model: d.model.clone(),
                app_name: d.app_name.clone(),
                app_version: d.app_version.clone(),
                user: directory.user_ref(d.user)?,
                created_at: d.created_at.into(),
                last_seen_at: d.last_seen_at.into(),
                last_ip: d.last_ip.clone(),
            })
        })
        .collect();
    Ok(json_with(StatusCode::OK, &AdminDevices { devices }))
}

async fn revoke_device(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: DeviceId = path_id(&id)?;
    state.auth.revoke_device(None, &id).await?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "device.revoke",
        Some(id.to_string()),
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn list_keys(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let list = read(&state, |c| key_repo::list(c, None)).await?;
    let directory = directory(&state).await?;
    let keys = list
        .iter()
        .filter(|k| k.kind != KeyKind::Device && k.revoked_at.is_none())
        .filter_map(|k| {
            let record = map::key_record(k);
            Some(AdminKey {
                id: record.id,
                kind: record.kind,
                name: record.name,
                prefix: record.prefix,
                scopes: record.scopes,
                user: directory.user_ref(k.user)?,
                created_at: record.created_at,
                last_used_at: record.last_used_at,
                last_used_ip: record.last_used_ip,
            })
        })
        .collect();
    Ok(json_with(StatusCode::OK, &AdminKeys { keys }))
}

async fn revoke_key(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: KeyId = path_id(&id)?;
    state.auth.revoke_key(None, &id).await?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "key.revoke",
        Some(id.to_string()),
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn list_sessions(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let current_hash = cookie_value(&headers).map(|v| sha256(&v));
    let (list, current) = read(&state, move |c| {
        let current = match current_hash {
            Some(hash) => sessions::by_hash(c, &hash)?.map(|s| s.key),
            None => None,
        };
        Ok((sessions::list(c, None)?, current))
    })
    .await?;
    let directory = directory(&state).await?;
    let sessions = list
        .iter()
        .filter_map(|s| {
            Some(SessionRecord {
                id: s.key.0.to_string(),
                user: directory.user_ref(s.user)?,
                user_agent: s.user_agent.clone(),
                ip: s.ip.clone(),
                created_at: s.created_at.into(),
                last_seen_at: s.last_seen_at.into(),
                current: Some(s.key) == current,
            })
        })
        .collect();
    Ok(json_with(StatusCode::OK, &AdminSessions { sessions }))
}

async fn end_session(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let key = id
        .parse::<i64>()
        .map(sessions::SessionKey)
        .map_err(|_| ApiError::not_found())?;
    write(&state, move |tx| sessions::delete(tx, key)).await?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "session.end",
        Some(id),
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// Backups

fn ops_error(error: OpsError) -> ApiError {
    match error {
        OpsError::Busy => ApiError::conflict("running", "A backup is already running."),
        OpsError::NotFound => ApiError::not_found(),
        OpsError::NotEnoughSpace => ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "insufficient_storage",
            "There is not enough free space for a backup.",
        ),
        OpsError::Io(e) => ApiError::internal(&e),
    }
}

/// Backup names are plain `trackside-*.db` file names.
pub fn backup_name_ok(name: &str) -> bool {
    (name.starts_with("trackside-") || name.starts_with("pre-migrate-"))
        && std::path::Path::new(name)
            .extension()
            .is_some_and(|e| e == "db")
        && !name.contains(['/', '\\', '\0'])
        && !name.contains("..")
}

async fn list_backups(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let backups = state.ops.backups().await.map_err(ops_error)?;
    let next_at = state.ops.next_backup().map(Into::into);
    Ok(json_with(StatusCode::OK, &Backups { backups, next_at }))
}

async fn create_backup(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
) -> ApiResult<Response> {
    state.ops.start_backup().await.map_err(ops_error)?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "backup.create",
        None,
        None,
    )
    .await;
    Ok(json_with(StatusCode::ACCEPTED, &serde_json::json!({})))
}

async fn download_backup(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(name): Path<String>,
) -> ApiResult<Response> {
    require_recent_auth(&state, &principal)?;
    if !backup_name_ok(&name) {
        return Err(ApiError::not_found());
    }
    let path = state
        .ops
        .backup_path(&name)
        .ok_or_else(ApiError::not_found)?;
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ApiError::not_found())?;
    let length = file
        .metadata()
        .await
        .map_err(|e| ApiError::internal(&e))?
        .len();
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "backup.download",
        Some(name.clone()),
        None,
    )
    .await;
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.sqlite3"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

async fn delete_backup(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(name): Path<String>,
) -> ApiResult<Response> {
    require_recent_auth(&state, &principal)?;
    if !backup_name_ok(&name) {
        return Err(ApiError::not_found());
    }
    state.ops.delete_backup(&name).await.map_err(ops_error)?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "backup.delete",
        Some(name),
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// Audit and imports

#[derive(Debug, Deserialize)]
struct AuditQuery {
    action: Option<String>,
    actor: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

async fn audit_entries(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Response> {
    let filter = AuditFilter {
        action: query.action,
        actor: query.actor,
        cursor: query.cursor,
        limit: query.limit.unwrap_or(100).clamp(1, 500),
    };
    let page = read(&state, move |c| audit::list(c, &filter)).await?;
    let entries = page
        .items
        .iter()
        .map(|row| AuditEntry {
            at: row.at.into(),
            actor: row.entry.actor_name.clone(),
            ip: row.entry.ip.clone().unwrap_or_default(),
            action: row.entry.action.clone(),
            target: row.entry.target.clone().unwrap_or_default(),
            detail: row.entry.detail.clone(),
            summary: audit_summary(&row.entry.action, row.entry.target.as_deref()),
        })
        .collect();
    Ok(json_with(
        StatusCode::OK,
        &Audit {
            entries,
            next_cursor: page.next,
        },
    ))
}

#[derive(Debug, Deserialize)]
struct ImportsQuery {
    state: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

async fn list_imports(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Query(query): Query<ImportsQuery>,
) -> ApiResult<Response> {
    let wanted = query
        .state
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| {
            ImportState::parse(s).ok_or_else(|| ApiError::bad_field("state", "Unknown state."))
        })
        .transpose()?;
    let filter = ImportFilter {
        user: None,
        state: wanted,
        cursor: query.cursor,
        limit: query.limit.unwrap_or(100).clamp(1, 500),
    };
    let page = read(&state, move |c| imports::list(c, &filter)).await?;
    let directory = directory(&state).await?;
    let imports = page
        .items
        .iter()
        .map(|row| AdminImport {
            id: row.id.to_string(),
            file_name: row.original_name.clone(),
            source: row.source.as_str().to_owned(),
            user: row.user.and_then(|u| directory.user_ref(u)),
            state: row.state.as_str().to_owned(),
            song_id: row.song.as_ref().map(ToString::to_string),
            error: row.error.clone(),
            created_at: row.created_at.into(),
        })
        .collect();
    Ok(json_with(
        StatusCode::OK,
        &AdminImports {
            imports,
            next_cursor: page.next,
        },
    ))
}

// Acquisition (provider names appear only here)

async fn providers(AdminSession(_): AdminSession, State(state): State<ApiState>) -> Response {
    let list = state.acquire.providers().await;
    json_with(
        StatusCode::OK,
        &Providers {
            providers: list.iter().map(map::provider).collect(),
        },
    )
}

async fn patch_provider(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(name): Path<String>,
    Json(body): Json<ProviderPatch>,
) -> ApiResult<Response> {
    let known = state.acquire.providers().await;
    if !known.iter().any(|p| p.name == name) {
        return Err(ApiError::not_found());
    }
    state
        .acquire
        .configure(ProviderChange::Provider {
            name: name.clone(),
            enabled: body.enabled,
            concurrency: body.concurrency,
        })
        .await?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "acquire.provider",
        Some(name),
        None,
    )
    .await;
    let list = state.acquire.providers().await;
    Ok(json_with(
        StatusCode::OK,
        &Providers {
            providers: list.iter().map(map::provider).collect(),
        },
    ))
}

async fn test_provider(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Path(name): Path<String>,
    OptionalJson(body): OptionalJson<ProviderTestRequest>,
) -> ApiResult<Response> {
    let query = body
        .query
        .filter(|q| !q.trim().is_empty())
        .unwrap_or_else(|| String::from("beyonce halo"));
    let result = state.acquire.test_provider(&name, &query).await?;
    Ok(json_with(
        StatusCode::OK,
        &ProviderTestResult {
            ok: result.ok,
            elapsed_ms: u64::try_from(result.elapsed.as_millis()).unwrap_or(u64::MAX),
            candidates: result.candidates,
            detail: result.detail,
        },
    ))
}

async fn update_ytdlp(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
) -> ApiResult<Response> {
    state.acquire.update_ytdlp().await?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "acquire.update",
        None,
        None,
    )
    .await;
    Ok(json_with(StatusCode::ACCEPTED, &serde_json::json!({})))
}

async fn patch_acquire(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<AcquirePatch>,
) -> ApiResult<Response> {
    state
        .acquire
        .configure(ProviderChange::Global {
            enabled: body.enabled,
            instant_play: body.instant_play,
            member_daily: body.member_daily,
            member_active: body.member_active,
            global_daily: body.global_daily,
            prefer: body.prefer.clone(),
        })
        .await?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "acquire.settings",
        None,
        serde_json::to_value(&body).ok(),
    )
    .await;
    let list = state.acquire.providers().await;
    Ok(json_with(
        StatusCode::OK,
        &Providers {
            providers: list.iter().map(map::provider).collect(),
        },
    ))
}

#[derive(Debug, Deserialize)]
struct AdminAcquisitionsQuery {
    state: Option<String>,
    user: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

async fn list_acquisitions(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Query(query): Query<AdminAcquisitionsQuery>,
) -> ApiResult<Response> {
    let directory = directory(&state).await?;
    let user = match query.user.as_deref().filter(|u| !u.is_empty()) {
        Some(raw) => {
            let id: UserId = body_id(raw, "user")?;
            Some(
                directory
                    .users
                    .values()
                    .find(|u| u.id == id)
                    .map(|u| u.key)
                    .ok_or_else(|| ApiError::bad_field("user", "Unknown user."))?,
            )
        }
        None => None,
    };
    let filter = AcquisitionFilter {
        user,
        state: super::acquisition::state_filter(query.state.as_deref())?,
        cursor: query.cursor,
        limit: query.limit.unwrap_or(100).clamp(1, 500),
    };
    let page = state.acquire.admin_list(filter).await?;
    let by_id: HashMap<UserId, &User> = directory
        .users
        .values()
        .map(|u| (u.id.clone(), u))
        .collect();
    let base = &state.server.base_path;
    let acquisitions = page
        .items
        .iter()
        .map(|v| {
            let user = v
                .user
                .as_ref()
                .and_then(|id| by_id.get(id))
                .map(|u| map::user_ref(u));
            map::admin_acquisition(v, user, base)
        })
        .collect();
    Ok(json_with(
        StatusCode::OK,
        &AdminAcquisitions {
            acquisitions,
            next_cursor: page.next,
        },
    ))
}

async fn retry_acquisition(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: AcquisitionId = path_id(&id)?;
    state.acquire.retry(&id).await?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "acquire.retry",
        Some(id.to_string()),
        None,
    )
    .await;
    Ok(json_with(StatusCode::ACCEPTED, &serde_json::json!({})))
}

async fn cancel_acquisition(
    AdminSession(principal): AdminSession,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: AcquisitionId = path_id(&id)?;
    state.acquire.cancel(&principal.access, &id).await?;
    audit_log(
        &state,
        Some(&principal),
        Some(ip),
        "acquire.cancel",
        Some(id.to_string()),
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_cannot_escape_the_backup_folder() {
        assert!(backup_name_ok("trackside-20261114T033000Z.db"));
        assert!(!backup_name_ok("trackside-20261114T033000Z.db.key"));
        assert!(!backup_name_ok("../trackside-x.db"));
        assert!(!backup_name_ok("secret.key"));
    }

    #[test]
    fn library_roots_are_checked() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("data");
        let music = dir.path().join("music");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&data)?;
        std::fs::create_dir_all(music.join("sub"))?;
        std::fs::create_dir_all(&other)?;
        assert!(check_root(FsPath::new("relative"), false, &data, &[]).is_err());
        assert!(check_root(&dir.path().join("missing"), false, &data, &[]).is_err());
        assert!(check_root(&data.join("."), false, &data, &[]).is_err());
        assert!(
            check_root(
                &music.join("sub"),
                false,
                &data,
                std::slice::from_ref(&music)
            )
            .is_err()
        );
        assert!(check_root(&other, true, &data, std::slice::from_ref(&music)).is_ok());
        Ok(())
    }

    #[test]
    fn permission_patches_keep_unnamed_flags() {
        let base = Permissions {
            max_bitrate_kbps: Some(320),
            ..Permissions::default()
        };
        let patch = PermissionsPatch {
            can_upload: Some(true),
            max_bitrate_kbps: Some(None),
            ..PermissionsPatch::default()
        };
        let merged = apply_permissions(&base, Some(&patch));
        assert!(merged.can_upload);
        assert!(merged.can_download);
        assert_eq!(merged.max_bitrate_kbps, None);
    }

    #[test]
    fn invite_states_follow_their_row() {
        let row = InviteRow {
            id: trackside_core::random_id(),
            created_by: None,
            role: Role::Member,
            permissions: Permissions::default(),
            note: None,
            libraries: Vec::new(),
            created_at: Millis(0),
            expires_at: Millis(100),
            used_at: None,
            used_by: None,
            revoked_at: None,
        };
        assert_eq!(invite_state(&row, Millis(50)), "open");
        assert_eq!(invite_state(&row, Millis(100)), "expired");
        let used = InviteRow {
            used_at: Some(Millis(10)),
            ..row.clone()
        };
        assert_eq!(invite_state(&used, Millis(50)), "used");
        let revoked = InviteRow {
            revoked_at: Some(Millis(10)),
            ..row
        };
        assert_eq!(invite_state(&revoked, Millis(50)), "revoked");
    }
}
