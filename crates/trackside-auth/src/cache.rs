//! A short-lived cache of authenticated keys and media tokens, so that a phone's burst of
//! requests does not read the database for each one. Entries live at most [`TTL`], which
//! bounds how long a revocation made by another process (the CLI) takes to apply; revocations
//! in this process and `CredentialRevoked` events drop entries at once.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use trackside_core::{DeviceKey, KeyId, Millis, UserKey};

use crate::Principal;
use crate::limit::lock;

pub(crate) const TTL: Duration = Duration::from_secs(1);
const MAX_ENTRIES: usize = 20_000;

#[derive(Clone, Debug)]
pub(crate) struct Cached {
    pub principal: Principal,
    pub key: Option<KeyId>,
    pub media_expires: Option<Millis>,
    stored_at: Millis,
}

#[derive(Debug, Default)]
pub(crate) struct CredentialCache {
    entries: Mutex<HashMap<[u8; 32], Cached>>,
}

impl CredentialCache {
    pub(crate) fn get(&self, hash: &[u8; 32], now: Millis) -> Option<Cached> {
        let mut entries = lock(&self.entries);
        match entries.get(hash) {
            Some(entry)
                if now.since(entry.stored_at) < TTL
                    && entry.stored_at <= now
                    && entry.media_expires.is_none_or(|x| now <= x) =>
            {
                Some(entry.clone())
            }
            Some(_) => {
                entries.remove(hash);
                None
            }
            None => None,
        }
    }

    pub(crate) fn put(
        &self,
        hash: [u8; 32],
        principal: Principal,
        key: Option<KeyId>,
        media_expires: Option<Millis>,
        now: Millis,
    ) {
        let mut entries = lock(&self.entries);
        if entries.len() >= MAX_ENTRIES {
            entries.retain(|_, e| now.since(e.stored_at) < TTL);
            if entries.len() >= MAX_ENTRIES {
                entries.clear();
            }
        }
        entries.insert(
            hash,
            Cached {
                principal,
                key,
                media_expires,
                stored_at: now,
            },
        );
    }

    pub(crate) fn forget_user(&self, user: UserKey) {
        lock(&self.entries).retain(|_, e| e.principal.access.user != user);
    }

    pub(crate) fn forget_device(&self, device: DeviceKey) {
        lock(&self.entries).retain(|_, e| {
            e.principal
                .device
                .as_ref()
                .is_none_or(|(d, _)| *d != device)
        });
    }

    pub(crate) fn forget_key(&self, key: &KeyId) {
        lock(&self.entries).retain(|_, e| e.key.as_ref() != Some(key));
    }

    pub(crate) fn clear(&self) {
        lock(&self.entries).clear();
    }
}
