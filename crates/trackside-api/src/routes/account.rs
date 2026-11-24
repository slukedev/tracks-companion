//! Setup, sign-in, pairing, invites, resets, and the signed-in user's account, devices and
//! keys (api.md sections 6 and 7).

use std::net::IpAddr;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use base64::Engine;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use sha2::{Digest, Sha256};
use trackside_auth::extract::{Authed, ClientIp, SecureTransport, SessionUser};
use trackside_auth::{
    IssuedDeviceKey, KeyKind, NewDevice, NewUser, PairPoll, Principal, Scopes, SessionCookie,
};
use trackside_core::control::AcquireFeature;
use trackside_core::records::Library;
use trackside_core::repo::devices::{self, Device};
use trackside_core::repo::users::{self, User, UserPatch};
use trackside_core::repo::{keys, libraries, listens};
use trackside_core::{DeviceId, KeyId, PairingId, Permissions, PublicId, Role, UserKey};

use super::{api_url, idempotency, read, require_recent_auth, server_summary, settings, write};
use crate::error::{ApiError, ApiResult};
use crate::extract::{Json, OptionalJson, json_with, parse_json, path_id};
use crate::wire::Library as WireLibrary;
use crate::wire::Time;
use crate::wire::auth::{
    AcceptInvite, Approved, ApprovedDevice, ClaimRequest, Device as WireDevice, DeviceInfo,
    DeviceRename, Devices, InviteInfo, Keys, LoginRequest, MeFeatures, MePatch, MeResponse,
    MediaToken, NewKey, NewPassword, PairRequest, PairRequestBody, PairResult, PairState,
    PairingCode, PairingCodeRequest, PasswordChange, QrPayload, ReauthRequest, ResetInfo,
    ServerSummary, SessionRequest, SetupRequest,
};
use crate::{ApiState, map};

const LIBRARY: u8 = Scopes::LIBRARY.bits();
const STREAM: u8 = Scopes::STREAM.bits();
const WRITE: u8 = Scopes::WRITE.bits();

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/setup", post(setup))
        .route(
            "/auth/session",
            get(session_get).post(session_create).delete(session_end),
        )
        .route("/auth/reauth", post(reauth))
        .route("/auth/login", post(login))
        .route("/me", get(me).patch(me_patch))
        .route("/me/password", post(change_password))
        .route("/media-token", post(media_token))
        .route("/me/pairing-codes", post(pairing_code))
        .route(
            "/me/pairing-codes/{id}",
            axum::routing::delete(pairing_code_cancel),
        )
        .route("/me/pairing-codes/{id}/qr.svg", get(pairing_code_qr))
        .route("/pair/claim", post(claim))
        .route("/pair/requests", post(pair_request))
        .route("/pair/requests/approve", post(approve))
        .route("/pair/requests/{id}", get(pair_poll))
        .route("/invites/{token}", get(invite_info))
        .route("/invites/{token}/accept", post(invite_accept))
        .route("/password-resets/{token}", get(reset_info).post(reset_use))
        .route("/me/devices", get(my_devices))
        .route(
            "/me/devices/{id}",
            patch(rename_device).delete(revoke_device),
        )
        .route("/me/api-keys", get(my_keys).post(create_key))
        .route("/me/api-keys/{id}", axum::routing::delete(revoke_key))
        .route("/me/listens/export", get(export_listens))
}

pub use trackside_auth::Https;

fn is_https(parts_extensions: &http::Extensions) -> bool {
    parts_extensions.get::<Https>().is_some()
}

/// Whether the session cookie must be `Secure` (and `__Host-`): always, except over plain HTTP
/// from a local client that reached the server directly or through a trusted proxy.
fn cookie_secure(
    state: &ApiState,
    headers: &HeaderMap,
    extensions: &http::Extensions,
    client: IpAddr,
) -> bool {
    let peer = extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip());
    is_https(extensions) || !state.auth.is_local_client(headers, peer, client)
}

fn with_cookie(mut response: Response, cookie: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// The session cookie's value from the request, under either name.
pub(crate) fn cookie_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| {
            *name == trackside_auth::SESSION_COOKIE || *name == trackside_auth::PLAIN_SESSION_COOKIE
        })
        .map(|(_, value)| value.to_owned())
}

pub(crate) fn sha256(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

fn clean_name(name: &str, field: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(ApiError::bad_field(
            field,
            format!("{field} must be 1 to 100 characters."),
        ));
    }
    Ok(name.to_owned())
}

/// What a client says about itself, cleaned up. Unknown platforms are `other`.
pub(crate) fn new_device(info: DeviceInfo) -> ApiResult<NewDevice> {
    let client_device_id = info.client_device_id.trim().to_owned();
    if client_device_id.is_empty() || client_device_id.len() > 200 {
        return Err(ApiError::bad_field(
            "device.client_device_id",
            "device.client_device_id must be 1 to 200 characters.",
        ));
    }
    let platform = info
        .platform
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| ["ios", "macos", "android", "web", "other"].contains(&p.as_str()))
        .unwrap_or_else(|| String::from("other"));
    let short = |v: Option<String>| v.map(|v| v.trim().chars().take(100).collect::<String>());
    Ok(NewDevice {
        client_device_id,
        name: clean_name(&info.name, "device.name")?,
        platform,
        model: short(info.model).filter(|v| !v.is_empty()),
        app_name: short(info.app_name).filter(|v| !v.is_empty()),
        app_version: short(info.app_version).filter(|v| !v.is_empty()),
    })
}

pub(crate) async fn load_user(state: &ApiState, key: UserKey) -> ApiResult<User> {
    read(state, move |c| users::get(c, key))
        .await?
        .ok_or_else(ApiError::unauthenticated)
}

/// The user's response, with a new session cookie.
async fn signed_in(
    state: &ApiState,
    user: UserKey,
    ip: IpAddr,
    headers: &HeaderMap,
    extensions: &http::Extensions,
    status: StatusCode,
) -> ApiResult<Response> {
    let agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.chars().take(200).collect::<String>());
    let session = state
        .auth
        .create_session(user, ip, agent.as_deref())
        .await?;
    let record = load_user(state, user).await?;
    let now = state.db.clock().now();
    let secure = cookie_secure(state, headers, extensions, ip);
    let cookie = session.set_cookie(secure, now);
    Ok(with_cookie(json_with(status, &map::user(&record)), &cookie))
}

async fn setup(
    _secure: SecureTransport,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    extensions: http::Extensions,
    Json(body): Json<SetupRequest>,
) -> ApiResult<Response> {
    if !super::public::setup_required(&state).await? {
        return Err(ApiError::conflict(
            "setup_done",
            "The server already has an admin.",
        ));
    }
    let username = body.username.trim().to_owned();
    let admin = NewUser {
        display_name: body
            .display_name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| username.clone()),
        username,
        password: zeroize_string(body.password),
        role: Role::Admin,
        permissions: all_permissions(),
        libraries: Vec::new(),
    };
    let user = state
        .auth
        .complete_setup(&body.setup_token, admin, ip)
        .await?;
    super::audit(&state, None, Some(ip), "setup.complete", None, None).await;
    signed_in(&state, user, ip, &headers, &extensions, StatusCode::CREATED).await
}

fn zeroize_string(value: String) -> zeroize::Zeroizing<String> {
    zeroize::Zeroizing::new(value)
}

fn all_permissions() -> Permissions {
    Permissions {
        can_download: true,
        can_upload: true,
        can_request: true,
        legacy_subsonic: true,
        max_bitrate_kbps: None,
    }
}

async fn session_get(
    SessionUser(principal): SessionUser,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let user = load_user(&state, principal.access.user).await?;
    let mut body = map::user(&user);
    body.recent_auth_until = principal.session.map(|s| Time(s.recent_auth_until));
    Ok(json_with(StatusCode::OK, &body))
}

async fn reauth(
    _secure: SecureTransport,
    SessionUser(principal): SessionUser,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<ReauthRequest>,
) -> ApiResult<Response> {
    state.auth.reauth(&principal, &body.password, ip).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn session_create(
    _secure: SecureTransport,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    extensions: http::Extensions,
    Json(body): Json<SessionRequest>,
) -> ApiResult<Response> {
    let user = state.auth.login(&body.username, &body.password, ip).await?;
    signed_in(&state, user, ip, &headers, &extensions, StatusCode::OK).await
}

async fn session_end(
    SessionUser(_): SessionUser,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    extensions: http::Extensions,
) -> ApiResult<Response> {
    if let Some(value) = cookie_value(&headers) {
        state.auth.end_session(&value).await?;
    }
    let secure = cookie_secure(&state, &headers, &extensions, ip);
    Ok(with_cookie(
        StatusCode::NO_CONTENT.into_response(),
        &SessionCookie::clear_cookie(secure),
    ))
}

/// **`PairResult`** for a key just issued.
async fn pair_result(state: &ApiState, issued: IssuedDeviceKey) -> ApiResult<PairResult> {
    let user = load_user(state, issued.principal.access.user).await?;
    let token = state.auth.mint_media_token(&issued.principal)?;
    let server = server_summary(state, false).await;
    Ok(pair_result_body(&issued, &user, &token, server))
}

/// The `PairResult` shape. Pure, for the fixture test.
pub fn pair_result_body(
    issued: &IssuedDeviceKey,
    user: &User,
    token: &trackside_auth::MediaToken,
    server: ServerSummary,
) -> PairResult {
    PairResult {
        api_key: issued.secret.clone(),
        device: WireDevice {
            id: issued.device.to_string(),
            name: issued.device_name.clone(),
        },
        user: map::user(user),
        media_token: MediaToken {
            token: token.token.clone(),
            expires_at: token.expires_at.into(),
        },
        server,
    }
}

async fn login(
    _secure: SecureTransport,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<LoginRequest>,
) -> ApiResult<Response> {
    let device = new_device(body.device)?;
    let user = state.auth.login(&body.username, &body.password, ip).await?;
    let name = device.name.clone();
    let issued = state.auth.issue_device_key(user, device).await?;
    let principal = issued.principal.clone();
    let result = pair_result(&state, issued).await?;
    super::audit(
        &state,
        Some(&principal),
        Some(ip),
        "device.create",
        Some(name),
        None,
    )
    .await;
    Ok(json_with(StatusCode::CREATED, &result))
}

/// The `/me` shape. Pure, for the fixture test.
pub fn me_body(
    user: &User,
    libraries: &[(Library, bool)],
    device: Option<&Device>,
    server: ServerSummary,
    feature: Option<AcquireFeature>,
) -> MeResponse {
    MeResponse {
        user: map::user(user),
        libraries: libraries
            .iter()
            .map(|(library, available)| map::library(library, *available))
            .collect::<Vec<WireLibrary>>(),
        device: device.map(|d| WireDevice {
            id: d.id.to_string(),
            name: d.name.clone(),
        }),
        server,
        features: MeFeatures {
            acquisition: feature.map(|f| crate::wire::auth::AcquisitionFeature {
                instant_play: f.instant_play,
                external_play: f.external_play,
            }),
        },
    }
}

/// `features.acquisition` for this caller, or `None`.
pub(crate) fn acquisition_feature(
    state: &ApiState,
    principal: &Principal,
) -> Option<AcquireFeature> {
    state.acquire.feature_for(
        &principal.access,
        &principal.permissions,
        principal.scopes.contains(Scopes::REQUEST),
    )
}

async fn me(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let user = load_user(&state, principal.access.user).await?;
    let access = principal.access.clone();
    let device_key = principal.device.as_ref().map(|(key, _)| *key);
    let (libraries, device) = read(&state, move |c| {
        let libraries = libraries::list(c, &access)?;
        let device = match device_key {
            Some(key) => devices::get(c, key)?,
            None => None,
        };
        Ok((libraries, device))
    })
    .await?;
    let libraries: Vec<(Library, bool)> = libraries
        .into_iter()
        .map(|l| {
            let available = super::available(&state, &l.id);
            (l, available)
        })
        .collect();
    let server = server_summary(&state, true).await;
    let feature = acquisition_feature(&state, &principal);
    Ok(json_with(
        StatusCode::OK,
        &me_body(&user, &libraries, device.as_ref(), server, feature),
    ))
}

async fn me_patch(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<MePatch>,
) -> ApiResult<Response> {
    let key = principal.access.user;
    let patch = UserPatch {
        display_name: body.display_name,
        ..UserPatch::default()
    };
    let user = state
        .auth
        .update_user(Some(key), key, patch, Some(ip))
        .await?;
    Ok(json_with(StatusCode::OK, &map::user(&user)))
}

async fn change_password(
    _secure: SecureTransport,
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Json(body): Json<PasswordChange>,
) -> ApiResult<Response> {
    require_recent_auth(&state, &principal)?;
    let cookie = cookie_value(&headers);
    state
        .auth
        .change_password(
            principal.access.user,
            &body.current_password,
            &body.new_password,
            cookie.as_deref(),
            body.sign_out_devices,
            ip,
        )
        .await
        .map_err(|e| match e {
            trackside_auth::AuthError::Unauthenticated => {
                ApiError::bad_field("current_password", "The current password is not right.")
            }
            other => other.into(),
        })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn media_token(
    Authed(principal): Authed<STREAM>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let token = state.auth.mint_media_token(&principal)?;
    Ok(json_with(
        StatusCode::OK,
        &MediaToken {
            token: token.token,
            expires_at: token.expires_at.into(),
        },
    ))
}

/// The `trackside://pair?d=` link of an offer.
pub fn qr_link(payload: &QrPayload) -> String {
    let json = serde_json::to_vec(payload).unwrap_or_default();
    format!(
        "trackside://pair?d={}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    )
}

async fn pairing_code(
    SessionUser(principal): SessionUser,
    State(state): State<ApiState>,
    OptionalJson(body): OptionalJson<PairingCodeRequest>,
) -> ApiResult<Response> {
    let device_name = body
        .device_name
        .map(|n| clean_name(&n, "device_name"))
        .transpose()?;
    let offer = state
        .auth
        .create_pairing_offer(principal.access.user, device_name.as_deref())
        .await?;
    let summary = server_summary(&state, false).await;
    let payload = QrPayload {
        v: 1,
        server_id: summary.server_id,
        name: summary.name,
        urls: summary.urls,
        offer: offer.id.to_string(),
        code: offer.qr_secret.clone(),
        exp: offer.expires_at.0.div_euclid(1000),
    };
    let link = qr_link(&payload);
    if let Some(svg) = crate::qr::qr_svg(&link) {
        state.local.put_qr(
            format!("pair:{}", offer.id),
            Some(principal.access.user),
            svg,
            offer.expires_at,
        );
    }
    let body = PairingCode {
        id: offer.id.to_string(),
        code: offer.code,
        qr_payload: link,
        qr_svg_url: api_url(&state, &format!("/me/pairing-codes/{}/qr.svg", offer.id)),
        expires_at: offer.expires_at.into(),
    };
    Ok(json_with(StatusCode::CREATED, &body))
}

pub(crate) fn svg_response(svg: String) -> Response {
    let mut response = Body::from(svg).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

async fn pairing_code_qr(
    SessionUser(principal): SessionUser,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: PairingId = path_id(&id)?;
    let now = state.db.clock().now();
    state
        .local
        .qr(&format!("pair:{id}"), Some(principal.access.user), now)
        .map(svg_response)
        .ok_or_else(ApiError::not_found)
}

async fn pairing_code_cancel(
    SessionUser(principal): SessionUser,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: PairingId = path_id(&id)?;
    state
        .auth
        .cancel_pairing_offer(principal.access.user, &id)
        .await?;
    state.local.drop_qr(&format!("pair:{id}"));
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn claim(
    _secure: SecureTransport,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<ClaimRequest>,
) -> ApiResult<Response> {
    let device = new_device(body.device)?;
    let name = device.name.clone();
    let issued = state
        .auth
        .claim_pairing(body.code.trim(), device, ip)
        .await
        .map_err(|e| match e {
            trackside_auth::AuthError::NotFound | trackside_auth::AuthError::Conflict(_) => {
                ApiError::unauthenticated()
            }
            other => other.into(),
        })?;
    let principal = issued.principal.clone();
    let result = pair_result(&state, issued).await?;
    super::audit(
        &state,
        Some(&principal),
        Some(ip),
        "device.pair",
        Some(name),
        None,
    )
    .await;
    Ok(json_with(StatusCode::CREATED, &result))
}

/// Six digits as the device shows them: `481 207`.
pub fn spaced_code(code: &str) -> String {
    let digits: String = code.chars().filter(char::is_ascii_digit).collect();
    if digits.len() == 6 {
        format!("{} {}", &digits[..3], &digits[3..])
    } else {
        code.to_owned()
    }
}

async fn pair_request(
    _secure: SecureTransport,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<PairRequestBody>,
) -> ApiResult<Response> {
    let device = new_device(body.device)?;
    let request = state.auth.start_pair_request(device, ip).await?;
    let body = PairRequest {
        id: request.id.to_string(),
        code: spaced_code(&request.code),
        poll_secret: request.poll_secret,
        expires_at: request.expires_at.into(),
        poll_interval_ms: u64::try_from(request.poll_interval.as_millis()).unwrap_or(2000),
    };
    Ok(json_with(StatusCode::CREATED, &body))
}

async fn pair_poll(
    _secure: SecureTransport,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let id: PairingId = path_id(&id)?;
    let secret = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Pair "))
        .map(str::trim)
        .ok_or_else(ApiError::unauthenticated)?;
    let poll = state.auth.poll_pair_request(&id, secret).await?;
    let body = match poll {
        PairPoll::Pending => crate::wire::auth::PairPoll {
            state: PairState::Pending,
            result: None,
        },
        PairPoll::Expired => crate::wire::auth::PairPoll {
            state: PairState::Expired,
            result: None,
        },
        PairPoll::Approved(issued) => crate::wire::auth::PairPoll {
            state: PairState::Approved,
            result: Some(pair_result(&state, issued).await?),
        },
    };
    Ok(json_with(StatusCode::OK, &body))
}

/// Binds a device-initiated pairing to the signed-in user (also `/admin/pairing/approve`).
pub(crate) async fn approve(
    SessionUser(principal): SessionUser,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<crate::wire::auth::ApproveRequest>,
) -> ApiResult<Response> {
    let code: String = body.code.chars().filter(char::is_ascii_digit).collect();
    if code.len() != 6 {
        return Err(ApiError::bad_field("code", "The code has six digits."));
    }
    let approved = state
        .auth
        .approve_pair_request(principal.access.user, &code)
        .await?;
    super::audit(
        &state,
        Some(&principal),
        Some(ip),
        "pairing.approve",
        approved.device_name.clone(),
        approved.platform.clone().map(serde_json::Value::String),
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

async fn invite_info(
    State(state): State<ApiState>,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let invite = state.auth.invite_info(&token).await?;
    let invited_by = match invite.created_by {
        Some(key) => read(&state, move |c| users::get(c, key))
            .await?
            .map(|u| u.display_name),
        None => None,
    };
    Ok(json_with(
        StatusCode::OK,
        &InviteInfo {
            valid: true,
            role: invite.role.as_str().to_owned(),
            expires_at: invite.expires_at.into(),
            invited_by,
            note: invite
                .note
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty()),
        },
    ))
}

async fn invite_accept(
    _secure: SecureTransport,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(token): Path<String>,
    headers: HeaderMap,
    extensions: http::Extensions,
    Json(body): Json<AcceptInvite>,
) -> ApiResult<Response> {
    let username = body.username.trim().to_owned();
    let user = NewUser {
        display_name: body
            .display_name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| username.clone()),
        username,
        password: zeroize_string(body.password),
        role: Role::Member,
        permissions: Permissions::default(),
        libraries: Vec::new(),
    };
    let key = state.auth.accept_invite(&token, user, ip).await?;
    signed_in(&state, key, ip, &headers, &extensions, StatusCode::CREATED).await
}

async fn reset_info(
    State(state): State<ApiState>,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let reset = state.auth.password_reset_info(&token).await?;
    Ok(json_with(
        StatusCode::OK,
        &ResetInfo {
            valid: true,
            expires_at: reset.expires_at.into(),
        },
    ))
}

async fn reset_use(
    _secure: SecureTransport,
    State(state): State<ApiState>,
    Path(token): Path<String>,
    Json(body): Json<NewPassword>,
) -> ApiResult<Response> {
    state
        .auth
        .use_password_reset(&token, &body.password)
        .await
        .map_err(|e| match e {
            trackside_auth::AuthError::Unauthenticated | trackside_auth::AuthError::Conflict(_) => {
                ApiError::not_found()
            }
            other => other.into(),
        })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn my_devices(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let user = principal.access.user;
    let current = principal.device.as_ref().map(|(key, _)| *key);
    let list = read(&state, move |c| devices::list(c, Some(user))).await?;
    let devices = list
        .iter()
        .filter(|d| d.revoked_at.is_none())
        .map(|d| map::device_record(d, Some(d.key) == current))
        .collect();
    Ok(json_with(StatusCode::OK, &Devices { devices }))
}

async fn rename_device(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<DeviceRename>,
) -> ApiResult<Response> {
    let id: DeviceId = path_id(&id)?;
    let name = clean_name(&body.name, "name")?;
    let user = principal.access.user;
    let current = principal.device.as_ref().map(|(key, _)| *key);
    let device = write(&state, move |tx| devices::rename(tx, user, &id, &name)).await?;
    Ok(json_with(
        StatusCode::OK,
        &map::device_record(&device, Some(device.key) == current),
    ))
}

async fn revoke_device(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: DeviceId = path_id(&id)?;
    state
        .auth
        .revoke_device(Some(principal.access.user), &id)
        .await?;
    super::audit(
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

async fn my_keys(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let user = principal.access.user;
    let list = read(&state, move |c| keys::list(c, Some(user))).await?;
    let keys = list
        .iter()
        .filter(|k| k.kind != KeyKind::Device && k.revoked_at.is_none())
        .map(map::key_record)
        .collect();
    Ok(json_with(StatusCode::OK, &Keys { keys }))
}

async fn create_key(
    _secure: SecureTransport,
    SessionUser(principal): SessionUser,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    parts: http::request::Parts,
    body: Bytes,
) -> ApiResult<Response> {
    let http::request::Parts {
        method,
        uri,
        headers,
        ..
    } = parts;
    require_recent_auth(&state, &principal)?;
    let request: NewKey = parse_json(&body)?;
    let user = principal.access.user;
    let path = uri.path().to_owned();
    let state_ref = &state;
    idempotency::once(
        &state,
        user,
        &headers,
        &method,
        &path,
        &body,
        || async move {
            let kind = match request.kind.as_str() {
                "api_key" => KeyKind::ApiKey,
                "app_password" => KeyKind::AppPassword,
                _ => {
                    return Err(ApiError::bad_field(
                        "kind",
                        "kind is api_key or app_password.",
                    ));
                }
            };
            if kind == KeyKind::AppPassword {
                let allowed_here =
                    settings::effective_bool(state_ref, "auth.legacy_subsonic").await;
                if !allowed_here || !principal.permissions.legacy_subsonic {
                    return Err(ApiError::forbidden());
                }
            }
            let scopes = match &request.scopes {
                Some(names) => Scopes::parse(&names.join(" "))
                    .ok_or_else(|| ApiError::bad_field("scopes", "An unknown scope was named."))?,
                None => Scopes::KEY_DEFAULT,
            };
            let name = clean_name(&request.name, "name")?;
            let issued = state_ref.auth.create_key(user, kind, &name, scopes).await?;
            let mut record = map::key_record(&issued.key);
            record.secret = Some(issued.secret.clone());
            if kind == KeyKind::AppPassword {
                record.username = Some(issued.username.clone());
            }
            super::audit(
                state_ref,
                Some(&principal),
                Some(ip),
                "key.create",
                Some(name),
                None,
            )
            .await;
            Ok((StatusCode::CREATED, serde_json::to_value(&record)?))
        },
    )
    .await
}

async fn revoke_key(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: KeyId = path_id(&id)?;
    state
        .auth
        .revoke_key(Some(principal.access.user), &id)
        .await?;
    super::audit(
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

/// One listen as a ListenBrainz-compatible JSON line.
pub fn listen_line(row: &listens::ListenRow) -> serde_json::Value {
    let mut info = serde_json::Map::new();
    if let Some(ms) = row.duration_ms {
        info.insert(String::from("duration_ms"), ms.into());
    }
    info.insert(String::from("submission_client"), "Trackside".into());
    if let Some(song) = &row.song {
        info.insert(String::from("trackside_song_id"), song.as_str().into());
    }
    let mut track = serde_json::Map::new();
    track.insert(String::from("artist_name"), row.artist.clone().into());
    track.insert(String::from("track_name"), row.title.clone().into());
    if let Some(album) = &row.album {
        track.insert(String::from("release_name"), album.clone().into());
    }
    track.insert(String::from("additional_info"), info.into());
    serde_json::json!({
        "listened_at": row.started_at.0.div_euclid(1000),
        "track_metadata": track,
    })
}

async fn export_listens(
    SessionUser(principal): SessionUser,
    State(state): State<ApiState>,
) -> Response {
    let user = principal.access.user;
    let pages = futures_util::stream::unfold(Some(None::<i64>), move |after| {
        let state = state.clone();
        async move {
            let after = after?;
            let page = state
                .db
                .read(move |c| listens::export(c, user, after, 1000))
                .await;
            match page {
                Ok((rows, next)) => {
                    let mut chunk = Vec::new();
                    for row in &rows {
                        if let Ok(line) = serde_json::to_vec(&listen_line(row)) {
                            chunk.extend_from_slice(&line);
                            chunk.push(b'\n');
                        }
                    }
                    let following = next.map(Some);
                    Some((Ok::<_, std::io::Error>(Bytes::from(chunk)), following))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "listen export stopped");
                    None
                }
            }
        }
    });
    let mut response = Body::from_stream(pages).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"listens.jsonl\""),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_values_are_found_under_either_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; trackside_session=xyz; b=2"),
        );
        assert_eq!(cookie_value(&headers), Some(String::from("xyz")));
    }

    #[test]
    fn devices_are_cleaned() -> Result<(), ApiError> {
        let device = new_device(DeviceInfo {
            client_device_id: String::from(" 8C2E "),
            name: String::from(" Luke's iPhone "),
            platform: Some(String::from("iOS")),
            model: Some(String::new()),
            app_name: Some(String::from("Tracks")),
            app_version: None,
        })?;
        assert_eq!(device.client_device_id, "8C2E");
        assert_eq!(device.name, "Luke's iPhone");
        assert_eq!(device.platform, "ios");
        assert_eq!(device.model, None);
        let unknown = new_device(DeviceInfo {
            client_device_id: String::from("x"),
            name: String::from("Box"),
            platform: Some(String::from("toaster")),
            model: None,
            app_name: None,
            app_version: None,
        })?;
        assert_eq!(unknown.platform, "other");
        Ok(())
    }

    #[test]
    fn pair_request_codes_are_spaced() {
        assert_eq!(spaced_code("481207"), "481 207");
        assert_eq!(spaced_code("12"), "12");
    }

    #[test]
    fn qr_links_decode_to_the_payload() -> Result<(), Box<dyn std::error::Error>> {
        let payload = QrPayload {
            v: 1,
            server_id: String::from("5f0c2b9e-8a3d-4c1e-9b7a-2d6e4f8a1c3b"),
            name: String::from("Home"),
            urls: Vec::new(),
            offer: String::from("g4k2m9q7x1v8b3n6c0de"),
            code: String::from("Q7RM4X2K9T"),
            exp: 1_794_649_200,
        };
        let link = qr_link(&payload);
        let encoded = link
            .strip_prefix("trackside://pair?d=")
            .ok_or("no prefix")?;
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded)?;
        let back: QrPayload = serde_json::from_slice(&json)?;
        assert_eq!(back, payload);
        Ok(())
    }
}
