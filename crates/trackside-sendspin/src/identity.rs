//! The player's static X25519 identity. Its public key is the Sendspin `client_id`, and so the
//! Music Assistant player id: keep one per phone and it brings back the same player and queue.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use snow::params::DHChoice;
use snow::resolvers::{CryptoResolver, DefaultResolver};
use zeroize::Zeroizing;

use crate::b64;

pub const KEY_LEN: usize = 32;

/// A static keypair. `Debug` shows only the public half; the private half is zeroed on drop
/// and leaves this type only through [`Identity::secret_bytes`], for sealing.
#[derive(Clone)]
pub struct Identity {
    private: Zeroizing<[u8; KEY_LEN]>,
    public: [u8; KEY_LEN],
}

/// Why a key could not be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdentityError {
    #[error("a private key is 32 bytes")]
    Length,
    #[error("no random number generator")]
    Rng,
    #[error("the key store failed: {0}")]
    Store(String),
}

impl Identity {
    /// A fresh keypair from the operating system's CSPRNG.
    pub fn generate() -> Result<Self, IdentityError> {
        let mut private = Zeroizing::new([0u8; KEY_LEN]);
        let mut rng = DefaultResolver.resolve_rng().ok_or(IdentityError::Rng)?;
        rng.try_fill_bytes(private.as_mut_slice())
            .map_err(|_| IdentityError::Rng)?;
        Self::from_secret_bytes(private.as_slice())
    }

    /// The keypair for a stored private key.
    pub fn from_secret_bytes(bytes: &[u8]) -> Result<Self, IdentityError> {
        let private: [u8; KEY_LEN] = bytes.try_into().map_err(|_| IdentityError::Length)?;
        let private = Zeroizing::new(private);
        let mut dh = DefaultResolver
            .resolve_dh(&DHChoice::Curve25519)
            .ok_or(IdentityError::Rng)?;
        dh.set(private.as_slice());
        let public: [u8; KEY_LEN] = dh.pubkey().try_into().map_err(|_| IdentityError::Length)?;
        Ok(Self { private, public })
    }

    /// The private key, for the caller to seal and store. Never log it.
    pub fn secret_bytes(&self) -> Zeroizing<[u8; KEY_LEN]> {
        self.private.clone()
    }

    pub(crate) fn private(&self) -> &[u8; KEY_LEN] {
        &self.private
    }

    pub fn public_key(&self) -> &[u8; KEY_LEN] {
        &self.public
    }

    /// The Sendspin `client_id`: the public key, base64url without padding (43 characters).
    pub fn client_id(&self) -> String {
        b64::encode_url(&self.public)
    }
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("client_id", &self.client_id())
            .finish_non_exhaustive()
    }
}

pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, IdentityError>> + Send + 'a>>;

/// Where identities live between runs. The server implements it over its sealed settings, so
/// private keys are stored encrypted and never in plain text. `owner` is an opaque key, one
/// per phone (the app device).
pub trait IdentityStore: Send + Sync {
    fn load<'a>(&'a self, owner: &'a str) -> StoreFuture<'a, Option<Identity>>;
    fn save<'a>(&'a self, owner: &'a str, identity: &'a Identity) -> StoreFuture<'a, ()>;
}

/// The stored identity for `owner`, or a new one saved first.
pub async fn load_or_create(
    store: &dyn IdentityStore,
    owner: &str,
) -> Result<Identity, IdentityError> {
    if let Some(identity) = store.load(owner).await? {
        return Ok(identity);
    }
    let identity = Identity::generate()?;
    store.save(owner, &identity).await?;
    Ok(identity)
}

/// An in-memory [`IdentityStore`], for tests and for servers that accept a new player per
/// restart.
#[derive(Default)]
pub struct MemoryIdentityStore {
    keys: Mutex<HashMap<String, Identity>>,
}

impl std::fmt::Debug for MemoryIdentityStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MemoryIdentityStore")
    }
}

impl IdentityStore for MemoryIdentityStore {
    fn load<'a>(&'a self, owner: &'a str) -> StoreFuture<'a, Option<Identity>> {
        let found = self
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(owner)
            .cloned();
        Box::pin(async move { Ok(found) })
    }

    fn save<'a>(&'a self, owner: &'a str, identity: &'a Identity) -> StoreFuture<'a, ()> {
        self.keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(owner.to_owned(), identity.clone());
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_id_is_43_base64url_characters() {
        let identity = Identity::generate().expect("key");
        let id = identity.client_id();
        assert_eq!(id.len(), 43);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn a_stored_key_gives_back_the_same_client_id() {
        let identity = Identity::generate().expect("key");
        let again = Identity::from_secret_bytes(identity.secret_bytes().as_slice()).expect("key");
        assert_eq!(identity.client_id(), again.client_id());
    }

    #[test]
    fn the_rfc_7748_public_key_is_derived() {
        let private =
            b64::decode_hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let identity = Identity::from_secret_bytes(&private).expect("key");
        assert_eq!(
            identity.public_key().as_slice(),
            b64::decode_hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a")
        );
    }

    #[test]
    fn debug_never_shows_the_private_key() {
        let identity = Identity::generate().expect("key");
        let private = b64::encode_url(identity.secret_bytes().as_slice());
        let shown = format!("{identity:?} {identity:#?}");
        assert!(!shown.contains(&private), "{shown}");
    }

    #[test]
    fn a_short_key_is_refused() {
        assert_eq!(
            Identity::from_secret_bytes(&[1; 31]).err(),
            Some(IdentityError::Length)
        );
    }

    #[tokio::test]
    async fn load_or_create_keeps_one_identity_per_owner() {
        let store = MemoryIdentityStore::default();
        let a = load_or_create(&store, "phone-a").await.expect("a");
        let again = load_or_create(&store, "phone-a").await.expect("a again");
        let b = load_or_create(&store, "phone-b").await.expect("b");
        assert_eq!(a.client_id(), again.client_id());
        assert_ne!(a.client_id(), b.client_id());
    }
}
