//! The connection settings: the configuration file and environment win and are read-only;
//! otherwise the dashboard's values, kept in the `settings` table, win over the defaults. The
//! token is sealed with the server's secret key at rest and never leaves the process except
//! towards Music Assistant.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use trackside_auth::Auth;
use trackside_core::assistant::SettingSource;
use trackside_core::config::{ConfigSource, Secret};
use trackside_core::repo::settings as stored;
use trackside_core::{Config, ConfigReport, Db, DbError, KeyId, Millis};
use trackside_mass::{CustomCa, Endpoint, Token};

/// The settings row with the dashboard's values.
pub(crate) const CONNECTION: &str = "music_assistant.connection";
/// The settings row naming Trackside's registration in Music Assistant.
pub(crate) const REGISTRATION: &str = "music_assistant.registration";
/// What the sealed token is sealed for.
const TOKEN_PURPOSE: &str = "music_assistant.token";

/// What the dashboard saved. Every field is optional: absent means "not set here".
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Stored {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The token sealed with the server's secret key, base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_set_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_register: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_services: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_max_sessions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_pem: Option<String>,
}

/// Trackside's registration in Music Assistant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Registration {
    pub instance_id: String,
    pub key_id: KeyId,
    pub base_url: String,
    pub registered_at: i64,
    /// The Music Assistant server it was made on.
    pub server_id: String,
}

/// The settings in force.
#[derive(Clone, Debug)]
pub(crate) struct Effective {
    pub url: Option<String>,
    pub url_source: SettingSource,
    pub token: Option<Token>,
    pub token_source: SettingSource,
    pub token_set_at: Option<Millis>,
    pub enabled: bool,
    pub auto_register: bool,
    pub lan_url: Option<String>,
    pub hidden_services: Vec<String>,
    pub relay_max_sessions: u32,
    pub ca: Option<CustomCa>,
    /// Why the configured CA cannot be used, if it cannot.
    pub ca_error: Option<String>,
    /// Every setting a dashboard change may not touch, by its field name.
    pub locked: Vec<&'static str>,
}

impl Effective {
    /// An address and a token are set.
    pub fn configured(&self) -> bool {
        self.url.is_some() && self.token.is_some()
    }

    pub fn endpoint(&self) -> Option<Endpoint> {
        self.url.as_deref().and_then(|u| Endpoint::parse(u).ok())
    }

    pub fn active(&self) -> bool {
        self.enabled && self.configured()
    }
}

fn from_config(report: &ConfigReport, key: &str) -> bool {
    matches!(
        report.source(key),
        ConfigSource::File | ConfigSource::Env | ConfigSource::Override
    )
}

/// The settings from the configuration and what the dashboard stored.
pub(crate) fn effective(
    config: &Config,
    report: &ConfigReport,
    auth: &Auth,
    row: &Stored,
) -> Effective {
    let ma = &config.music_assistant;
    let mut locked = Vec::new();
    let mut pick = |key: &'static str, field: &'static str| {
        let set = from_config(report, key);
        if set {
            locked.push(field);
        }
        set
    };
    let url_config = pick("music_assistant.url", "url");
    let token_config = pick("music_assistant.token", "token");
    let enabled_config = pick("music_assistant.enabled", "enabled");
    let auto_config = pick("music_assistant.auto_register", "auto_register");
    let lan_config = pick("music_assistant.lan_url", "lan_url");
    let ca_config = pick("music_assistant.ca_file", "ca_pem");
    let hidden_config = pick("music_assistant.hidden_services", "hidden_services");
    let relay_config = pick("music_assistant.relay_max_sessions", "relay_max_sessions");

    let (url, url_source) = if url_config {
        (
            ma.url.clone().filter(|u| !u.trim().is_empty()),
            SettingSource::Config,
        )
    } else if let Some(url) = &row.url {
        (Some(url.clone()), SettingSource::Dashboard)
    } else {
        (None, SettingSource::Default)
    };
    let (token, token_source, token_set_at) = if token_config {
        (
            ma.token
                .as_ref()
                .map(|t| Token::new(t.expose()))
                .filter(|t| !t.is_empty()),
            SettingSource::Config,
            None,
        )
    } else if let Some(sealed) = &row.token {
        (
            open_token(auth, sealed),
            SettingSource::Dashboard,
            row.token_set_at.map(Millis),
        )
    } else {
        (None, SettingSource::Default, None)
    };
    let (ca, ca_error) = if ca_config {
        ma.ca_file.as_deref().map_or((None, None), read_ca)
    } else if let Some(pem) = &row.ca_pem {
        match CustomCa::from_pem(pem.as_bytes()) {
            Ok(ca) => (Some(ca), None),
            Err(e) => (None, Some(e.to_string())),
        }
    } else {
        (None, None)
    };
    Effective {
        url,
        url_source,
        token,
        token_source,
        token_set_at,
        enabled: if enabled_config {
            ma.enabled
        } else {
            row.enabled.unwrap_or(ma.enabled)
        },
        auto_register: if auto_config {
            ma.auto_register
        } else {
            row.auto_register.unwrap_or(ma.auto_register)
        },
        lan_url: if lan_config {
            ma.lan_url.clone()
        } else {
            row.lan_url.clone().or_else(|| ma.lan_url.clone())
        }
        .filter(|u| !u.trim().is_empty()),
        hidden_services: if hidden_config {
            ma.hidden_services.clone()
        } else {
            row.hidden_services
                .clone()
                .unwrap_or_else(|| ma.hidden_services.clone())
        },
        relay_max_sessions: if relay_config {
            ma.relay_max_sessions
        } else {
            row.relay_max_sessions.unwrap_or(ma.relay_max_sessions)
        },
        ca,
        ca_error,
        locked,
    }
}

fn read_ca(path: &Path) -> (Option<CustomCa>, Option<String>) {
    match std::fs::read(path) {
        Ok(bytes) => match CustomCa::from_pem(&bytes) {
            Ok(ca) => (Some(ca), None),
            Err(e) => (None, Some(format!("{}: {e}", path.display()))),
        },
        Err(e) => (None, Some(format!("cannot read {}: {e}", path.display()))),
    }
}

fn open_token(auth: &Auth, sealed: &str) -> Option<Token> {
    let bytes = STANDARD.decode(sealed).ok()?;
    match auth.open_setting(TOKEN_PURPOSE, &bytes) {
        Ok(plain) => Some(Token::new(zeroize::Zeroizing::new(plain).as_str())),
        Err(e) => {
            tracing::warn!(error = %e, "the saved Music Assistant token cannot be opened; save it again");
            None
        }
    }
}

pub(crate) fn seal_token(auth: &Auth, token: &Secret) -> Result<String, trackside_auth::AuthError> {
    auth.seal_setting(TOKEN_PURPOSE, token.expose())
        .map(|sealed| STANDARD.encode(sealed))
}

pub(crate) async fn load_stored(db: &Db) -> Result<Stored, DbError> {
    let value = db
        .read(|c| stored::get::<serde_json::Value>(c, CONNECTION))
        .await?;
    Ok(value
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

pub(crate) async fn save_stored(db: &Db, row: &Stored) -> Result<(), DbError> {
    let value = serde_json::to_value(row).map_err(|e| DbError::Invalid(e.to_string()))?;
    db.write(move |tx| stored::put(tx, CONNECTION, &value))
        .await
}

pub(crate) async fn delete_stored(db: &Db) -> Result<(), DbError> {
    db.write(|tx| {
        stored::delete(tx, CONNECTION)?;
        stored::delete(tx, REGISTRATION)
    })
    .await
}

pub(crate) async fn load_registration(db: &Db) -> Result<Option<Registration>, DbError> {
    let value = db
        .read(|c| stored::get::<serde_json::Value>(c, REGISTRATION))
        .await?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

pub(crate) async fn save_registration(db: &Db, row: Option<&Registration>) -> Result<(), DbError> {
    match row {
        Some(row) => {
            let value = serde_json::to_value(row).map_err(|e| DbError::Invalid(e.to_string()))?;
            db.write(move |tx| stored::put(tx, REGISTRATION, &value))
                .await
        }
        None => db.write(|tx| stored::delete(tx, REGISTRATION)).await,
    }
}

/// An `http` or `https` address with a host and nothing after the path.
pub(crate) fn valid_http_url(text: &str) -> Result<http::Uri, &'static str> {
    let uri: http::Uri = text.trim().parse().map_err(|_| "is not a valid address")?;
    match uri.scheme_str() {
        Some("http" | "https") => {}
        _ => return Err("must start with http:// or https://"),
    }
    let Some(authority) = uri.authority() else {
        return Err("has no host");
    };
    if authority.host().is_empty() {
        return Err("has no host");
    }
    if authority.as_str().contains('@') || uri.query().is_some() {
        return Err("must not hold a user name, password or query");
    }
    Ok(uri)
}

/// Whether an address names this machine only.
pub(crate) fn is_loopback(uri: &http::Uri) -> bool {
    let host = uri.host().unwrap_or_default().trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_addresses_with_a_host_are_accepted() {
        assert!(valid_http_url("http://192.168.1.20:8095").is_ok());
        assert!(valid_http_url("https://ma.example.com/ma").is_ok());
        for bad in [
            "ws://ma:8095",
            "ma.local",
            "http://",
            "http://u:p@ma",
            "http://ma/?a=1",
        ] {
            assert!(valid_http_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn loopback_addresses_are_recognised() {
        for text in [
            "http://localhost:4747",
            "http://127.0.0.1:4747",
            "http://[::1]:4747",
        ] {
            assert!(
                is_loopback(&valid_http_url(text).unwrap_or_default()),
                "{text}"
            );
        }
        assert!(!is_loopback(
            &valid_http_url("http://192.168.1.20:4747").unwrap_or_default()
        ));
    }
}
