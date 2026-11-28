//! The relay's player identities (one X25519 key per app device), kept in the `settings`
//! table sealed with the server's secret key. The sealing purpose names the row, so a sealed
//! key moved to another device's row does not open. A device's key goes when the device is
//! revoked or removed; Music Assistant then forgets its player after its retention.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use trackside_auth::Auth;
use trackside_core::repo::{devices, settings};
use trackside_core::{Db, DbError, DeviceId, PublicId};
use trackside_sendspin::{Identity, IdentityError, IdentityStore, StoreFuture};
use zeroize::Zeroizing;

use crate::lock;

/// Rows are `music_assistant.relay_identity.<device id>`.
const PREFIX: &str = "music_assistant.relay_identity.";

fn row(owner: &str) -> String {
    format!("{PREFIX}{owner}")
}

fn store_error(error: impl std::fmt::Display) -> IdentityError {
    IdentityError::Store(error.to_string())
}

/// Sealed identities, by device id, and the player ids they give.
pub(crate) struct SealedIdentities {
    db: Db,
    auth: Arc<Auth>,
    /// Device id to Music Assistant player id, for every stored key.
    players: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for SealedIdentities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SealedIdentities")
    }
}

impl SealedIdentities {
    pub fn new(db: Db, auth: Arc<Auth>) -> Self {
        Self {
            db,
            auth,
            players: Mutex::new(HashMap::new()),
        }
    }

    fn open(&self, owner: &str, sealed: &str) -> Result<Identity, IdentityError> {
        let bytes = STANDARD.decode(sealed).map_err(store_error)?;
        let plain = Zeroizing::new(
            self.auth
                .open_setting(&row(owner), &bytes)
                .map_err(store_error)?,
        );
        let secret = Zeroizing::new(STANDARD.decode(plain.as_bytes()).map_err(store_error)?);
        Identity::from_secret_bytes(&secret)
    }

    /// Reads every stored key, so relay players are known (and hidden from others) before
    /// their phones come back. Keys that no longer open are dropped.
    pub async fn load_all(&self) -> Result<(), DbError> {
        let rows = self
            .db
            .read(|c| {
                Ok(settings::all(c)?
                    .into_iter()
                    .filter_map(|(key, value)| {
                        let owner = key.strip_prefix(PREFIX)?.to_owned();
                        Some((owner, value.as_str()?.to_owned()))
                    })
                    .collect::<Vec<_>>())
            })
            .await?;
        let mut players = HashMap::new();
        for (owner, sealed) in rows {
            match self.open(&owner, &sealed) {
                Ok(identity) => {
                    players.insert(owner, identity.client_id());
                }
                Err(error) => {
                    tracing::warn!(%error, "a relay player key cannot be opened; dropping it");
                    self.delete(&owner).await?;
                }
            }
        }
        *lock(&self.players) = players;
        Ok(())
    }

    /// Every player id a stored key gives.
    pub fn player_ids(&self) -> Vec<String> {
        lock(&self.players).values().cloned().collect()
    }

    pub async fn delete(&self, owner: &str) -> Result<(), DbError> {
        let key = row(owner);
        self.db.write(move |tx| settings::delete(tx, &key)).await?;
        lock(&self.players).remove(owner);
        Ok(())
    }

    /// Deletes the keys of devices that are revoked or gone. Returns their player ids.
    pub async fn sweep(&self) -> Result<Vec<String>, DbError> {
        let owners: Vec<String> = lock(&self.players).keys().cloned().collect();
        let gone = self
            .db
            .read(move |c| {
                let mut gone = Vec::new();
                for owner in owners {
                    let device = match DeviceId::parse(&owner) {
                        Ok(id) => devices::by_pid(c, &id)?,
                        Err(_) => None,
                    };
                    if device.is_none_or(|d| d.revoked_at.is_some()) {
                        gone.push(owner);
                    }
                }
                Ok(gone)
            })
            .await?;
        let mut players = Vec::new();
        for owner in gone {
            if let Some(player) = lock(&self.players).get(&owner).cloned() {
                players.push(player);
            }
            self.delete(&owner).await?;
        }
        Ok(players)
    }
}

impl IdentityStore for SealedIdentities {
    fn load<'a>(&'a self, owner: &'a str) -> StoreFuture<'a, Option<Identity>> {
        Box::pin(async move {
            let key = row(owner);
            let sealed: Option<String> = self
                .db
                .read(move |c| settings::get(c, &key))
                .await
                .map_err(store_error)?;
            let Some(sealed) = sealed else {
                return Ok(None);
            };
            let identity = self.open(owner, &sealed)?;
            lock(&self.players).insert(owner.to_owned(), identity.client_id());
            Ok(Some(identity))
        })
    }

    fn save<'a>(&'a self, owner: &'a str, identity: &'a Identity) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let key = row(owner);
            let secret = Zeroizing::new(STANDARD.encode(identity.secret_bytes().as_slice()));
            let sealed = self
                .auth
                .seal_setting(&key, &secret)
                .map_err(store_error)?;
            let value = STANDARD.encode(sealed);
            self.db
                .write(move |tx| settings::put(tx, &key, &value))
                .await
                .map_err(store_error)?;
            lock(&self.players).insert(owner.to_owned(), identity.client_id());
            Ok(())
        })
    }
}
