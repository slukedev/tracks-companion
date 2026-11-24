//! Trackside Cloud (api.md section 19). Thin: parsing, the device-key rule, the push rate
//! limit and quotas from settings here; every merge rule is in `trackside-cloud`.
//!
//! Only device keys reach these routes, and every call works on the caller's own data. There
//! is no admin view of anyone's records; admins see only item and byte counts.

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::response::Response;
use axum::routing::{get, put};
use http::StatusCode;
use serde::Deserialize;
use trackside_auth::extract::Authed;
use trackside_auth::{Principal, Scopes, Via};
use trackside_cloud::limits::{DEFAULT_PAGE, MAX_BATCH, MAX_PAGE};
use trackside_cloud::store::{self, Registration};
use trackside_cloud::{
    Body, CloudDevice, CloudError, Cursor, Entity, Limits, Page, PushItem, PushOutcome, PushResult,
    Record, Setting, SettingOutcome, SettingResult, SettingWrite, SettingsPage, Usage,
};
use trackside_core::DeviceId;
use trackside_core::config::parse_size;

use super::settings;
use crate::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Json, Query, json_with};
use crate::wire::Time;
use crate::wire::cloud as wire;

const LIBRARY: u8 = Scopes::LIBRARY.bits();
const WRITE: u8 = Scopes::WRITE.bits();

/// A full batch of 1000 items at the 16 KiB field limit would be 16 MiB; 8 MiB is plenty for
/// real plays and keeps one request's memory bounded.
const BATCH_BODY_LIMIT: usize = 8 << 20;

pub(crate) const ENABLED: &str = "cloud.enabled";
pub(crate) const QUOTA_ITEMS: &str = "cloud.quota_items";
pub(crate) const QUOTA_BYTES: &str = "cloud.quota_bytes";

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/cloud/settings", get(get_settings).patch(patch_settings))
        .route("/cloud/devices", get(list_devices))
        .route("/cloud/devices/{device_id}", put(register_device))
        .route("/cloud/usage", get(usage))
        .route(
            "/cloud/{entity}",
            get(pull)
                .post(push)
                .layer(DefaultBodyLimit::max(BATCH_BODY_LIMIT)),
        )
}

impl From<CloudError> for ApiError {
    fn from(error: CloudError) -> Self {
        match error {
            CloudError::Db(db) => db.into(),
            CloudError::SyncReset => Self::new(
                StatusCode::GONE,
                "sync_reset",
                "This cursor can no longer be served; pull again from an empty cursor.",
            ),
            CloudError::Invalid(message) => Self::bad_request(message),
            CloudError::TooMany => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                format!("At most {MAX_BATCH} items per batch."),
            ),
            CloudError::DeviceMismatch => Self::new(
                StatusCode::FORBIDDEN,
                "device_mismatch",
                "This device key is bound to another device id.",
            ),
            CloudError::UnknownPredecessor(id) => {
                Self::bad_field("predecessors", format!("{id} is not one of your devices."))
            }
        }
    }
}

/// Cloud data is for the apps: a device key, never a session or a general API key, so a
/// third-party client given an API key cannot read the history and the dashboard never
/// shows it. A disabled feature is `404`.
async fn caller(state: &ApiState, principal: &Principal) -> ApiResult<()> {
    if !settings::effective_bool(state, ENABLED).await {
        return Err(ApiError::not_found());
    }
    if principal.via != Via::DeviceKey {
        return Err(ApiError::forbidden());
    }
    Ok(())
}

/// The quotas in effect: the dashboard's settings or the defaults.
pub(crate) async fn limits(state: &ApiState) -> Limits {
    let defaults = Limits::default();
    let items = match settings::effective(state, QUOTA_ITEMS).await {
        Some(value) => value.as_u64().unwrap_or(defaults.max_items),
        None => defaults.max_items,
    };
    let bytes = match settings::effective(state, QUOTA_BYTES).await {
        Some(value) => value
            .as_str()
            .and_then(parse_size)
            .unwrap_or(defaults.max_bytes),
        None => defaults.max_bytes,
    };
    Limits {
        max_items: items,
        max_bytes: bytes,
    }
}

fn entity(raw: &str) -> ApiResult<Entity> {
    Entity::parse(raw)
        .filter(|e| *e != Entity::Settings)
        .ok_or_else(ApiError::not_found)
}

fn cursor(raw: Option<&str>) -> ApiResult<Option<Cursor>> {
    match raw.map(str::trim).filter(|c| !c.is_empty()) {
        None => Ok(None),
        Some(text) => Cursor::parse(text)
            .map(Some)
            .ok_or_else(|| ApiError::bad_field("cursor", "The cursor is not valid.")),
    }
}

/// The app device id bound to the caller's device key: items, settings and registrations
/// must name it.
async fn bound(state: &ApiState, principal: &Principal) -> ApiResult<Option<String>> {
    let Some((install, _)) = &principal.device else {
        return Err(ApiError::forbidden());
    };
    Ok(store::bound_device(&state.db, principal.access.user, *install).await?)
}

#[derive(Debug, Default, Deserialize)]
struct PullQuery {
    cursor: Option<String>,
    limit: Option<u32>,
}

async fn pull(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(raw): Path<String>,
    Query(query): Query<PullQuery>,
) -> ApiResult<Response> {
    caller(&state, &principal).await?;
    let entity = entity(&raw)?;
    let cursor = cursor(query.cursor.as_deref())?;
    let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let page = store::pull(&state.db, principal.access.user, entity, cursor, limit).await?;
    Ok(json_with(StatusCode::OK, &page_body(&page)))
}

async fn push(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(raw): Path<String>,
    Json(batch): Json<wire::PushBatch>,
) -> ApiResult<Response> {
    caller(&state, &principal).await?;
    let entity = raw
        .strip_suffix(":batch")
        .ok_or_else(ApiError::not_found)
        .and_then(entity)?;
    if batch.items.len() > MAX_BATCH {
        return Err(CloudError::TooMany.into());
    }
    let user = principal.access.user;
    if let Err(wait) = state.local.cloud_limiter().check(user, batch.items.len()) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many cloud pushes; wait and send the rest.",
        )
        .with_retry_after(wait));
    }
    let items = batch.items.into_iter().map(push_item).collect();
    let limits = limits(&state).await;
    let device = bound(&state, &principal).await?;
    let results = store::push(&state.db, user, entity, items, device, limits).await?;
    Ok(json_with(StatusCode::OK, &push_body(&results)))
}

fn push_item(item: wire::PushItem) -> PushItem {
    PushItem {
        id: item.id,
        base_version: item.base_version,
        fields: item.fields,
        client_modified_at: item.client_modified_at.0,
        device_id: item.device_id,
        deleted_at: item.deleted_at.map(|t| t.0),
        merged_into: item.merged_into,
        undelete: item.undelete,
    }
}

/// The wire record. Pure, for the fixture test.
pub fn record_body(record: &Record) -> wire::Record {
    let (fields, deleted_at, merged_into) = match &record.body {
        Body::Live(fields) => (Some(fields.clone()), None, None),
        Body::Deleted {
            deleted_at,
            merged_into,
        } => (None, Some(Time(*deleted_at)), merged_into.clone()),
    };
    wire::Record {
        id: record.id.clone(),
        version: record.version,
        fields,
        device_id: record.device_id.clone(),
        client_modified_at: Time(record.client_modified_at),
        deleted_at,
        merged_into,
    }
}

pub fn page_body(page: &Page) -> wire::Page {
    wire::Page {
        upserts: page.upserts.iter().map(record_body).collect(),
        deletes: page.deletes.iter().map(record_body).collect(),
        cursor: page.cursor.encode(),
        has_more: page.has_more,
    }
}

pub fn push_body(results: &[PushResult]) -> wire::PushResults {
    let result = |r: &PushResult| {
        let (status, version, reason, current) = match &r.outcome {
            PushOutcome::Accepted { version } => ("accepted", Some(*version), None, None),
            PushOutcome::Duplicate { version } => ("duplicate", Some(*version), None, None),
            PushOutcome::Conflict { current } => {
                ("conflict", None, None, current.as_ref().map(record_body))
            }
            PushOutcome::Rejected { reason } => {
                ("rejected", None, Some(reason.as_str().to_owned()), None)
            }
        };
        wire::PushResult {
            id: r.id.clone(),
            status: status.to_owned(),
            version,
            reason,
            current,
        }
    };
    wire::PushResults {
        results: results.iter().map(result).collect(),
    }
}

#[derive(Debug, Default, Deserialize)]
struct SettingsQuery {
    cursor: Option<String>,
}

async fn get_settings(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Query(query): Query<SettingsQuery>,
) -> ApiResult<Response> {
    caller(&state, &principal).await?;
    let cursor = cursor(query.cursor.as_deref())?;
    let page = store::settings(&state.db, principal.access.user, cursor).await?;
    Ok(json_with(StatusCode::OK, &settings_body(&page)))
}

fn setting_value(setting: &Setting) -> wire::SettingValue {
    wire::SettingValue {
        value: setting.value.clone(),
        modified_at: Time(setting.modified_at),
        device_id: setting.device_id.clone(),
        version: setting.version,
    }
}

pub fn settings_body(page: &SettingsPage) -> wire::Settings {
    wire::Settings {
        settings: page
            .settings
            .iter()
            .map(|s| (s.key.clone(), setting_value(s)))
            .collect(),
        cursor: page.cursor.encode(),
    }
}

async fn patch_settings(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Json(patch): Json<wire::SettingsPatch>,
) -> ApiResult<Response> {
    caller(&state, &principal).await?;
    let user = principal.access.user;
    if let Err(wait) = state
        .local
        .cloud_limiter()
        .check(user, patch.settings.len())
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many cloud pushes; wait and send the rest.",
        )
        .with_retry_after(wait));
    }
    let mut writes = Vec::with_capacity(patch.settings.len());
    for (key, change) in patch.settings {
        let Some(value) = change.value else {
            return Err(ApiError::bad_field(
                "value",
                format!("{key}: value is required; send null to clear"),
            ));
        };
        writes.push(SettingWrite {
            key,
            value,
            modified_at: change.modified_at.0,
            device_id: change.device_id,
        });
    }
    let limits = limits(&state).await;
    let device = bound(&state, &principal).await?;
    let results = store::patch_settings(&state.db, user, writes, device, limits).await?;
    Ok(json_with(StatusCode::OK, &setting_results_body(&results)))
}

pub fn setting_results_body(results: &[SettingResult]) -> wire::SettingResults {
    let result = |r: &SettingResult| {
        let (status, reason) = match r.outcome {
            SettingOutcome::Accepted => ("accepted", None),
            SettingOutcome::Duplicate => ("duplicate", None),
            SettingOutcome::Conflict => ("conflict", None),
            SettingOutcome::Rejected(reason) => ("rejected", Some(reason.as_str().to_owned())),
        };
        wire::SettingResult {
            key: r.key.clone(),
            status: status.to_owned(),
            reason,
            current: r.current.as_ref().map(setting_value),
        }
    };
    wire::SettingResults {
        results: results.iter().map(result).collect(),
    }
}

async fn list_devices(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    caller(&state, &principal).await?;
    let devices = store::devices(&state.db, principal.access.user).await?;
    let mine = principal.device.as_ref().map(|(_, id)| id.clone());
    Ok(json_with(
        StatusCode::OK,
        &devices_body(&devices, mine.as_ref()),
    ))
}

pub fn device_body(device: &CloudDevice, mine: Option<&DeviceId>) -> wire::CloudDevice {
    wire::CloudDevice {
        device_id: device.device_id.clone(),
        predecessors: device.predecessors.clone(),
        name: device.name.clone(),
        platform: device.platform.clone(),
        server_device_id: device.server_device.as_ref().map(ToString::to_string),
        current: mine.is_some() && device.server_device.as_ref() == mine,
        registered_at: Time(device.registered_at),
        updated_at: Time(device.updated_at),
    }
}

pub fn devices_body(devices: &[CloudDevice], mine: Option<&DeviceId>) -> wire::CloudDevices {
    wire::CloudDevices {
        devices: devices.iter().map(|d| device_body(d, mine)).collect(),
    }
}

async fn register_device(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(device_id): Path<String>,
    Json(body): Json<wire::DeviceRegistration>,
) -> ApiResult<Response> {
    caller(&state, &principal).await?;
    let registration = Registration {
        device_id,
        predecessors: body.predecessors,
        name: body.name,
        platform: body.platform,
    };
    let server_device = principal.device.as_ref().map(|(key, _)| *key);
    let device = store::register_device(
        &state.db,
        principal.access.user,
        registration,
        server_device,
    )
    .await?;
    let mine = principal.device.as_ref().map(|(_, id)| id.clone());
    Ok(json_with(
        StatusCode::OK,
        &device_body(&device, mine.as_ref()),
    ))
}

async fn usage(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    caller(&state, &principal).await?;
    let usage = store::usage(&state.db, principal.access.user).await?;
    let limits = limits(&state).await;
    Ok(json_with(StatusCode::OK, &usage_body(&usage, limits)))
}

pub fn usage_body(usage: &Usage, limits: Limits) -> wire::Usage {
    wire::Usage {
        items: usage.items,
        bytes: usage.bytes,
        quota: wire::Amount {
            items: limits.max_items,
            bytes: limits.max_bytes,
        },
        entities: usage
            .entities
            .iter()
            .map(|e| {
                (
                    e.entity.as_str().to_owned(),
                    wire::Amount {
                        items: e.items,
                        bytes: e.bytes,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
    }
}

/// `capabilities.cloud`.
pub fn cloud_info(epoch: &str) -> wire::CloudInfo {
    wire::CloudInfo {
        version: 1,
        epoch: epoch.to_owned(),
        entities: Entity::ALL.iter().map(|e| e.as_str().to_owned()).collect(),
        max_batch: MAX_BATCH as u32,
        max_page: MAX_PAGE,
    }
}
