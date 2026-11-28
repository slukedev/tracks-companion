//! Trackside's own registration in Music Assistant (contract 4.4): a service account, a key
//! scoped `library stream` and nothing else, and an `OpenSubsonic` provider instance holding
//! that key, so the library plays on every speaker with its metadata.
//!
//! The key goes only into Music Assistant's provider setup; it is never stored in Trackside
//! (only its hash, as for every key) and never returned. Registering again makes a new key,
//! replaces the instance and revokes the old key.

use std::sync::Arc;

use trackside_auth::Scopes;
use trackside_core::assistant::{
    AssistantError, RegistrationKey, RegistrationState, RegistrationView, ServiceUser,
};
use trackside_core::repo::{keys, users};
use trackside_core::{KeyId, Millis};
use trackside_mass::{
    ErrorCode, OPENSUBSONIC_DOMAIN, OpenSubsonicSource, RequestError, SetupError, Token,
};

use crate::settings::{self, Registration};
use crate::{Inner, SERVICE, SERVICE_DISPLAY_NAME, SERVICE_USERNAME, lock};

/// The service account's name, and how the key is labelled.

#[derive(Debug)]
pub(crate) struct RegState {
    pub state: RegistrationState,
    pub last_error: Option<(Millis, String)>,
    /// Music Assistant reports Trackside's source as available.
    pub reachable: bool,
    pub checked_at: Option<Millis>,
}

impl Default for RegState {
    fn default() -> Self {
        Self {
            state: RegistrationState::NotRegistered,
            last_error: None,
            reachable: false,
            checked_at: None,
        }
    }
}

/// `baseURL`, `port` and `path` as Music Assistant's setup flow wants them.
pub(crate) fn source_parts(base_url: &str) -> Result<(String, u16, Option<String>), &'static str> {
    let uri = settings::valid_http_url(base_url)?;
    let scheme = uri.scheme_str().unwrap_or("http");
    let host = uri.host().ok_or("has no host")?;
    let port = uri
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    let path = uri.path().trim_end_matches('/');
    Ok((
        format!("{scheme}://{host}"),
        port,
        (!path.is_empty()).then(|| path.to_owned()),
    ))
}

fn needs_admin(error: &RequestError) -> bool {
    matches!(
        error.code(),
        Some(ErrorCode::InsufficientPermissions | ErrorCode::AuthenticationRequired)
    )
}

impl Inner {
    fn set_reg(&self, state: RegistrationState, error: Option<String>) {
        let now = self.now();
        let mut reg = lock(&self.reg);
        reg.state = state;
        reg.last_error = error.map(|e| (now, e));
    }

    fn not_admin_message(&self) -> String {
        let user = self
            .session()
            .and_then(|s| s.user.as_ref().map(|u| u.username.clone()))
            .unwrap_or_else(|| String::from("of the token"));
        format!("The Music Assistant account {user} is not an admin, so it cannot add providers.")
    }

    pub(crate) async fn registration_view(&self) -> RegistrationView {
        let record = settings::load_registration(&self.db).await.ok().flatten();
        let key_id = record.as_ref().map(|r| r.key_id.clone());
        let (key, user) = self
            .db
            .read(move |c| {
                let key = match &key_id {
                    Some(id) => keys::by_id(c, id)?.filter(|k| k.revoked_at.is_none()),
                    None => None,
                };
                Ok((key, users::service_account(c, SERVICE)?))
            })
            .await
            .unwrap_or_default();
        let lan = self.lan_url().map(|l| l.url);
        let base = self.ui_url();
        let reg = lock(&self.reg);
        let mut state = reg.state;
        if state == RegistrationState::NotRegistered && record.is_some() {
            state = RegistrationState::Registered;
        }
        let instance = record.as_ref().map(|r| r.instance_id.clone());
        RegistrationView {
            state,
            base_url: record.as_ref().map(|r| r.base_url.clone()).or(lan),
            key: key.map(|k| RegistrationKey {
                scopes: k
                    .scopes
                    .split_whitespace()
                    .filter_map(|s| match s {
                        "library" => Some("library"),
                        "stream" => Some("stream"),
                        "download" => Some("download"),
                        "write" => Some("write"),
                        "request" => Some("request"),
                        _ => None,
                    })
                    .collect(),
                id: k.id,
                prefix: k.prefix,
                created_at: k.created_at,
                last_used_at: k.last_used_at,
            }),
            ui_url: match (&base, &instance) {
                (Some(base), Some(instance)) => {
                    Some(format!("{base}/#/settings/editprovider/{instance}"))
                }
                _ => None,
            },
            provider_instance_id: instance,
            registered_at: record.as_ref().map(|r| Millis(r.registered_at)),
            service_user: user.filter(|_| record.is_some()).map(|u| ServiceUser {
                id: u.id,
                username: u.username,
                display_name: u.display_name,
            }),
            last_error: reg.last_error.clone(),
        }
    }

    /// `POST .../registration`: registers again in the background.
    pub(crate) async fn register_now(self: &Arc<Self>) -> Result<RegistrationView, AssistantError> {
        if !self.effective().active() {
            return Err(AssistantError::NotConfigured);
        }
        self.connected().map_err(|_| AssistantError::Unavailable)?;
        self.set_reg(RegistrationState::Registering, None);
        let me = Arc::clone(self);
        self.tasks
            .spawn(async move { me.check_registration(true).await });
        let mut view = self.registration_view().await;
        view.state = RegistrationState::Registering;
        Ok(view)
    }

    /// Checks the registration and, with `auto_register` or `force`, makes or repairs it.
    #[expect(
        clippy::too_many_lines,
        reason = "each case of the registration, in order"
    )]
    pub(crate) async fn check_registration(self: Arc<Self>, force: bool) {
        let _guard = self.reg_lock.lock().await;
        let effective = self.effective();
        let Ok(client) = self.connected() else { return };
        let Some(session) = self.session() else {
            return;
        };
        let record = settings::load_registration(&self.db).await.ok().flatten();
        let record = match record {
            Some(r) if r.server_id != session.server.server_id => {
                // Another Music Assistant now: the old one must not keep a working key.
                tracing::info!("Music Assistant changed; revoking the old server's key");
                self.revoke_managed_keys(None).await;
                if let Err(e) = settings::save_registration(&self.db, None).await {
                    tracing::warn!(error = %e, "could not forget the old registration");
                    return;
                }
                lock(&self.hub).ours = None;
                None
            }
            other => other,
        };
        let admin = session.user.as_ref().and_then(|u| u.role.as_deref()) == Some("admin");
        let configs = if admin {
            match client
                .provider_configs(None, Some(OPENSUBSONIC_DOMAIN))
                .await
            {
                Ok(configs) => Some(configs),
                Err(e) if needs_admin(&e) => None,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot read Music Assistant's provider configs");
                    return;
                }
            }
        } else {
            None
        };
        let present = record.as_ref().is_some_and(|r| match &configs {
            Some(configs) => configs.iter().any(|c| c.instance_id == r.instance_id),
            None => lock(&self.hub)
                .providers
                .iter()
                .any(|p| p.instance_id == r.instance_id),
        });
        let key_id = record.as_ref().map(|r| r.key_id.clone());
        let key_live = match key_id {
            Some(id) => self
                .db
                .read(move |c| keys::by_id(c, &id))
                .await
                .ok()
                .flatten()
                .is_some_and(|k| k.revoked_at.is_none()),
            None => false,
        };
        let lan = self.lan_url();
        let moved = record
            .as_ref()
            .zip(lan.as_ref())
            .is_some_and(|(r, l)| r.base_url != l.url);
        if let Some(record) = &record
            && present
            && key_live
            && !moved
            && !force
        {
            lock(&self.hub).ours = Some(record.instance_id.clone());
            self.set_reg(RegistrationState::Registered, None);
            self.note_reachable(&record.instance_id);
            return;
        }
        if configs.is_none() {
            let state = if record.is_some() && present {
                RegistrationState::Registered
            } else {
                RegistrationState::NeedsAdmin
            };
            if let Some(record) = record.as_ref().filter(|_| present) {
                lock(&self.hub).ours = Some(record.instance_id.clone());
            }
            let error = (state == RegistrationState::NeedsAdmin).then(|| self.not_admin_message());
            self.set_reg(state, error);
            return;
        }
        if record.is_some() && !(effective.auto_register || force) {
            self.set_reg(
                RegistrationState::Stale,
                Some(String::from(
                    "Music Assistant no longer has Trackside's source, or its key was revoked.",
                )),
            );
            return;
        }
        if record.is_none() && !(effective.auto_register || force) {
            self.set_reg(RegistrationState::NotRegistered, None);
            return;
        }
        let Some(lan) = lan else {
            self.set_reg(
                RegistrationState::Failed,
                Some(String::from("Trackside does not know an address Music Assistant can reach it at; set the LAN address.")),
            );
            return;
        };
        self.set_reg(RegistrationState::Registering, None);
        match self
            .register(&client, &lan.url, record.as_ref(), present)
            .await
        {
            Ok(record) => {
                lock(&self.hub).ours = Some(record.instance_id.clone());
                self.set_reg(RegistrationState::Registered, None);
                tracing::info!(instance = %record.instance_id, "registered Trackside in Music Assistant");
                // Music Assistant loads the source in the background; look again shortly.
                let me = Arc::clone(&self);
                let instance = record.instance_id.clone();
                self.tasks.spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    if let Ok(client) = me.connected()
                        && let Ok(providers) = client.providers(None).await
                    {
                        lock(&me.hub).providers = providers;
                    }
                    me.note_reachable(&instance);
                });
            }
            Err(RegisterError::NeedsAdmin) => {
                let message = self.not_admin_message();
                self.set_reg(RegistrationState::NeedsAdmin, Some(message));
            }
            Err(RegisterError::Failed(message)) => {
                tracing::warn!(%message, "could not register Trackside in Music Assistant");
                self.set_reg(RegistrationState::Failed, Some(message));
            }
        }
    }

    fn note_reachable(&self, instance: &str) {
        let reachable = lock(&self.hub)
            .providers
            .iter()
            .any(|p| p.instance_id == instance && p.available);
        let now = self.now();
        let mut reg = lock(&self.reg);
        reg.reachable = reachable;
        reg.checked_at = Some(now);
    }

    async fn register(
        &self,
        client: &trackside_mass::Client,
        base_url: &str,
        previous: Option<&Registration>,
        previous_present: bool,
    ) -> Result<Registration, RegisterError> {
        let (base, port, path) = source_parts(base_url)
            .map_err(|e| RegisterError::Failed(format!("The LAN address {e}.")))?;
        let user = self
            .auth
            .ensure_service_user(SERVICE, SERVICE_USERNAME, SERVICE_DISPLAY_NAME)
            .await
            .map_err(|e| {
                RegisterError::Failed(format!("Could not make the service account: {e}."))
            })?;
        let issued = self
            .auth
            .create_managed_key(
                user.key,
                SERVICE,
                SERVICE_DISPLAY_NAME,
                Scopes::LIBRARY.union(Scopes::STREAM),
            )
            .await
            .map_err(|e| RegisterError::Failed(format!("Could not make the key: {e}.")))?;
        let key_id = issued.key.id.clone();
        let revoke_new = || async {
            if let Err(e) = self.auth.revoke_key(None, &key_id).await {
                tracing::warn!(error = %e, "could not revoke an unused Music Assistant key");
            }
        };
        if let Some(previous) = previous.filter(|_| previous_present)
            && let Err(e) = client.remove_provider_config(&previous.instance_id).await
        {
            revoke_new().await;
            return Err(if needs_admin(&e) {
                RegisterError::NeedsAdmin
            } else {
                RegisterError::Failed(format!(
                    "Music Assistant did not remove the old source: {e}."
                ))
            });
        }
        let mut source = OpenSubsonicSource::new(base, Token::new(issued.secret.as_str()));
        source.port = Some(port);
        source.path = path;
        source.name = Some(format!("Trackside ({})", self.server_name));
        let instance_id = match client.register_opensubsonic(&source).await {
            Ok(id) => id,
            Err(e) => {
                revoke_new().await;
                return Err(match &e {
                    SetupError::Request(r) if needs_admin(r) => RegisterError::NeedsAdmin,
                    other => RegisterError::Failed(format!(
                        "Music Assistant did not add the source: {other}."
                    )),
                });
            }
        };
        let record = Registration {
            instance_id,
            key_id: issued.key.id.clone(),
            base_url: base_url.to_owned(),
            registered_at: self.now().0,
            server_id: self
                .session()
                .map(|s| s.server.server_id.clone())
                .unwrap_or_default(),
        };
        if let Err(e) = settings::save_registration(&self.db, Some(&record)).await {
            revoke_new().await;
            return Err(RegisterError::Failed(format!(
                "Could not save the registration: {e}."
            )));
        }
        self.revoke_managed_keys(Some(&record.key_id)).await;
        Ok(record)
    }

    /// Revokes every live key made for Music Assistant except `keep`: one registration has
    /// one key, and keys of earlier ones (another server, a lost record) must not keep working.
    async fn revoke_managed_keys(&self, keep: Option<&KeyId>) {
        let keep = keep.cloned();
        let stale = self
            .db
            .read(move |c| {
                Ok(keys::list(c, None)?
                    .into_iter()
                    .filter(|k| {
                        k.revoked_at.is_none()
                            && k.managed_by.as_deref() == Some(SERVICE)
                            && Some(&k.id) != keep.as_ref()
                    })
                    .map(|k| k.id)
                    .collect::<Vec<_>>())
            })
            .await;
        match stale {
            Ok(stale) => {
                for id in stale {
                    if let Err(e) = self.auth.revoke_key(None, &id).await {
                        tracing::warn!(error = %e, "could not revoke an old Music Assistant key");
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "cannot list the Music Assistant keys"),
        }
    }

    /// Removes the source from Music Assistant (when it can be reached) and revokes the key.
    pub(crate) async fn unregister(&self) -> Result<(), AssistantError> {
        let _guard = self.reg_lock.lock().await;
        let Some(record) = settings::load_registration(&self.db).await? else {
            self.set_reg(RegistrationState::NotRegistered, None);
            return Ok(());
        };
        if let Ok(client) = self.connected() {
            match client.remove_provider_config(&record.instance_id).await {
                Ok(()) => {}
                Err(e) if e.code() == Some(ErrorCode::InvalidProviderId) => {}
                Err(e) if needs_admin(&e) => {
                    tracing::warn!(
                        "the Music Assistant token cannot remove Trackside's source; remove it in Music Assistant"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not remove Trackside's source from Music Assistant");
                }
            }
        } else {
            tracing::warn!(
                "Music Assistant is not reachable; remove Trackside's source there by hand"
            );
        }
        if let Err(e) = self.auth.revoke_key(None, &record.key_id).await {
            tracing::debug!(error = %e, "the Music Assistant key was already revoked");
        }
        settings::save_registration(&self.db, None).await?;
        lock(&self.hub).ours = None;
        let mut reg = lock(&self.reg);
        *reg = super::registration::RegState::default();
        Ok(())
    }
}

enum RegisterError {
    NeedsAdmin,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_split_into_what_the_setup_flow_wants() {
        assert_eq!(
            source_parts("http://192.168.1.20:4747"),
            Ok((String::from("http://192.168.1.20"), 4747, None))
        );
        assert_eq!(
            source_parts("https://music.example.com/trackside/"),
            Ok((
                String::from("https://music.example.com"),
                443,
                Some(String::from("/trackside"))
            ))
        );
        assert_eq!(
            source_parts("http://nas.local"),
            Ok((String::from("http://nas.local"), 80, None))
        );
        assert!(source_parts("ftp://nas").is_err());
    }
}
