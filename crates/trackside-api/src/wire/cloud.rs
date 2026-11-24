//! Trackside Cloud (api.md section 19): per-user sync of the apps' own data. The envelope is
//! `snake_case` like the rest of the API; `fields` is the app's own object, stored and returned
//! as sent, so its names are the app's.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use super::Time;

/// `POST /cloud/{entity}:batch`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PushBatch {
    pub items: Vec<PushItem>,
}

/// One pushed item: an upsert with `fields`, or a delete with `deleted_at`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PushItem {
    /// The client's id. Stations, excluded stations and blocked artists may leave it empty
    /// and get `stationKey(name)`; likes and forgotten songs may leave it empty when `fields`
    /// has `title` and `artistName`.
    #[serde(default)]
    pub id: String,
    /// Compare-and-set: the `version` the client last saw, or 0 for "must not exist".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Map<String, Value>>,
    pub client_modified_at: Time,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_into: Option<String>,
    /// Required to upsert an id that is tombstoned (except for the newest-wins lists).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub undelete: bool,
}

/// The push response: one result per item, in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PushResults {
    pub results: Vec<PushResult>,
}

/// `status` is `accepted`, `duplicate`, `conflict` or `rejected` (open: ignore others).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PushResult {
    pub id: String,
    pub status: String,
    /// The stored version, for `accepted` and `duplicate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
    /// Why, for `rejected` (open).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The copy that stands, for `conflict`; absent when nothing is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<Record>,
}

/// A record as pulled. A delete has `deleted_at` and no `fields`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Map<String, Value>>,
    pub device_id: String,
    pub client_modified_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_into: Option<String>,
}

/// `GET /cloud/{entity}?cursor=&limit=`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub upserts: Vec<Record>,
    pub deletes: Vec<Record>,
    /// Opaque: send it back as it came.
    pub cursor: String,
    pub has_more: bool,
}

/// A setting as stored. `value` is any JSON, `null` for a cleared setting, or an encrypted
/// envelope the server never opens.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingValue {
    pub value: Value,
    pub modified_at: Time,
    pub device_id: String,
    pub version: i64,
}

/// `GET /cloud/settings?cursor=`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub settings: BTreeMap<String, SettingValue>,
    pub cursor: String,
}

fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// One key of `PATCH /cloud/settings`. `value` must be there; `null` clears.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingChange {
    #[serde(default, deserialize_with = "present")]
    pub value: Option<Value>,
    pub modified_at: Time,
    pub device_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsPatch {
    pub settings: BTreeMap<String, SettingChange>,
}

/// One key's outcome. `current` is the authoritative value after the decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingResult {
    pub key: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<SettingValue>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingResults {
    pub results: Vec<SettingResult>,
}

/// A registered device. `server_device_id` is the signed-in install that registered it;
/// `current` marks the caller's own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloudDevice {
    pub device_id: String,
    pub predecessors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_device_id: Option<String>,
    pub current: bool,
    pub registered_at: Time,
    pub updated_at: Time,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloudDevices {
    pub devices: Vec<CloudDevice>,
}

/// `PUT /cloud/devices/{device_id}`. Predecessors only grow.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRegistration {
    #[serde(default)]
    pub predecessors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// Items and bytes stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Amount {
    pub items: u64,
    pub bytes: u64,
}

/// `GET /cloud/usage`: what the caller stores and their quota.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub items: u64,
    pub bytes: u64,
    pub quota: Amount,
    pub entities: BTreeMap<String, Amount>,
}

/// `capabilities.cloud`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudInfo {
    pub version: u32,
    /// The sync epoch every cursor carries; it changes when the database is restored or
    /// replaced, and every cursor from before is then `410 sync_reset`.
    pub epoch: String,
    pub entities: Vec<String>,
    pub max_batch: u32,
    pub max_page: u32,
}

/// The `cloud` event: pull `entity` now. Never carries the data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudEvent {
    pub entity: String,
}
