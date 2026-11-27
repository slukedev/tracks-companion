//! The server-info greeting, the authenticated user, providers, provider configs and setup flow
//! steps (models `api.py`, `auth.py`, `provider.py`, `config_entries.py`, `setup_flow.py`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::enums::{FlowStepType, ProviderType};
use crate::de;

/// The first message on every connection (`ServerInfoMessage`), also the answer to `info`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ServerInfo {
    #[serde(default, deserialize_with = "de::nullable")]
    pub server_id: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub server_version: String,
    pub schema_version: u32,
    pub min_supported_schema_version: u32,
    #[serde(default)]
    pub name: Option<String>,
    /// `starting`, `running`, `stopping` or `stopped`.
    #[serde(default)]
    pub status: Option<String>,
    /// Deprecated alias of `internal_url`.
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub internal_url: Option<String>,
    #[serde(default)]
    pub external_url: Option<String>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub homeassistant_addon: bool,
    #[serde(default, deserialize_with = "de::nullable")]
    pub onboard_done: bool,
    #[serde(default, deserialize_with = "de::nullable")]
    pub has_remote_access: bool,
}

/// The Music Assistant account a token belongs to, from the `auth` answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct User {
    #[serde(default, deserialize_with = "de::nullable")]
    pub user_id: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// `admin`, `user`, `guest`, `service` or a custom role.
    #[serde(default)]
    pub role: Option<String>,
}

/// A loaded provider instance (`ProviderInstance`), from `providers`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderInstance {
    #[serde(rename = "type", default, deserialize_with = "de::nullable")]
    pub provider_type: ProviderType,
    #[serde(default, deserialize_with = "de::nullable")]
    pub domain: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub name: String,
    pub instance_id: String,
    /// `ProviderFeature` values such as `search` or `library_tracks`.
    #[serde(default, deserialize_with = "de::string_list")]
    pub supported_features: Vec<String>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub available: bool,
    /// Music providers only.
    #[serde(default)]
    pub is_streaming_provider: Option<bool>,
    #[serde(default)]
    pub lookup_key: Option<String>,
}

/// A summary of a provider manifest, from `providers/manifests`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderManifest {
    #[serde(rename = "type", default, deserialize_with = "de::nullable")]
    pub provider_type: ProviderType,
    pub domain: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub description: String,
    /// `alpha`, `beta`, `stable`, `experimental`, `unmaintained` or `deprecated`.
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub multi_instance: bool,
    #[serde(default, deserialize_with = "de::nullable")]
    pub builtin: bool,
    #[serde(default, deserialize_with = "de::nullable")]
    pub has_setup_flow: bool,
    #[serde(default)]
    pub icon: Option<String>,
}

/// A stored provider configuration (`ProviderConfig`), from `config/providers`. The values the
/// setup flow collected (`setup_data`, which holds credentials) are never sent by the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderConfig {
    #[serde(rename = "type", default, deserialize_with = "de::nullable")]
    pub provider_type: ProviderType,
    #[serde(default, deserialize_with = "de::nullable")]
    pub domain: String,
    pub instance_id: String,
    #[serde(default = "de::yes", deserialize_with = "de::nullable_true")]
    pub enabled: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub default_name: Option<String>,
    #[serde(default, deserialize_with = "de::lenient")]
    pub last_error: Option<ProviderError>,
    /// `loaded`, `loading`, `disabled`, `auth_required`, `incompatible` or `error`.
    #[serde(default)]
    pub status: Option<String>,
    /// Config entries by key, as sent (each an object with at least `value`).
    #[serde(default, deserialize_with = "de::nullable")]
    pub values: Map<String, Value>,
}

impl ProviderConfig {
    /// The current value of a config entry.
    pub fn value(&self, key: &str) -> Option<&Value> {
        self.values
            .get(key)
            .and_then(|entry| entry.get("value"))
            .filter(|v| !v.is_null())
    }

    /// The name shown for the instance.
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .or(self.default_name.as_deref())
            .unwrap_or(&self.domain)
    }
}

/// Why a provider could not load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderError {
    #[serde(default)]
    pub error_code: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub message: String,
}

/// One step of a provider setup flow (`SetupFlowStep`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SetupFlowStep {
    #[serde(default, deserialize_with = "de::nullable")]
    pub flow_id: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub step_id: String,
    #[serde(rename = "type", default, deserialize_with = "de::nullable")]
    pub step_type: FlowStepType,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// The form's config entries, as sent.
    #[serde(default, deserialize_with = "de::nullable")]
    pub entries: Vec<Value>,
    /// Field key (or `base`) to error text.
    #[serde(default, deserialize_with = "string_map")]
    pub errors: BTreeMap<String, String>,
    #[serde(default)]
    pub last_step: Option<bool>,
    #[serde(default)]
    pub url: Option<String>,
    /// On a finish step, what was created, for example `{"instance_id": "..."}`.
    #[serde(default, deserialize_with = "string_map")]
    pub result: BTreeMap<String, String>,
    /// On an abort step, why.
    #[serde(default)]
    pub reason: Option<String>,
}

fn string_map<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Object(map) => map
            .into_iter()
            .filter_map(|(k, v)| match v {
                Value::String(s) => Some((k, s)),
                Value::Null => None,
                other => Some((k, other.to_string())),
            })
            .collect(),
        _ => BTreeMap::new(),
    })
}
