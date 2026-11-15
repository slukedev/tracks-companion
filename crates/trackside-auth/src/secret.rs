//! The server's secret key, the subkeys derived from it, and sealing with XChaCha20-Poly1305.

use std::io::Write;
use std::path::Path;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use crate::AuthError;
use crate::token::fill_random;

const SEAL_VERSION: u8 = 1;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

/// The server's 32-byte secret key. Zeroed on drop, never printed.
pub struct SecretKey(Zeroizing<[u8; 32]>);

impl SecretKey {
    /// Reads the key file, or creates it (mode 0600) with fresh random bytes. The `bool` is
    /// true when the key was created now. The file holds the 32 raw bytes, or 64 hex digits
    /// for keys written by hand.
    pub fn load_or_create(path: &Path) -> Result<(Self, bool), AuthError> {
        match std::fs::read(path) {
            Ok(bytes) => {
                warn_if_readable_by_others(path);
                Ok((Self::parse(&Zeroizing::new(bytes))?, false))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut bytes = Zeroizing::new([0u8; 32]);
                fill_random(bytes.as_mut_slice())?;
                match write_new_private_file(path, bytes.as_slice()) {
                    Ok(()) => Ok((Self(bytes), true)),
                    // Another process created it between our read and write; use theirs.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let bytes = Zeroizing::new(std::fs::read(path)?);
                        Ok((Self::parse(&bytes)?, false))
                    }
                    Err(e) => Err(e.into()),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// A key from raw bytes, for tests and tools.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    fn parse(bytes: &[u8]) -> Result<Self, AuthError> {
        if let Ok(raw) = <[u8; 32]>::try_from(bytes) {
            return Ok(Self::from_bytes(raw));
        }
        let text = std::str::from_utf8(bytes)
            .map(str::trim)
            .unwrap_or_default();
        if let Some(raw) = crate::token::unhex(text)
            && let Ok(raw) = <[u8; 32]>::try_from(raw.as_slice())
        {
            return Ok(Self::from_bytes(raw));
        }
        Err(bad_key_file())
    }

    /// A 32-byte subkey for one purpose. Different purposes give unrelated keys.
    pub(crate) fn subkey(&self, purpose: &str) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(blake3::derive_key(
            &format!("trackside 2026-11 {purpose}"),
            self.0.as_slice(),
        ))
    }

    /// A short public fingerprint, to notice at start that the key file was replaced.
    pub fn fingerprint(&self) -> String {
        let id = self.subkey("key fingerprint");
        crate::token::hex(id.get(..8).unwrap_or_default())
    }
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

fn bad_key_file() -> AuthError {
    AuthError::Invalid(String::from(
        "the secret key file must hold 32 bytes (or 64 hex digits)",
    ))
}

/// Writes `bytes` to a new file only the owner can read, without ever replacing an existing
/// file. The data goes to a temporary file first so a crash never leaves a short key behind.
pub(crate) fn write_new_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".partial");
    let temporary = std::path::PathBuf::from(temporary);
    let _ = std::fs::remove_file(&temporary);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::hard_link(&temporary, path)
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

/// Replaces `path` with `bytes` in a file only the owner can read.
pub(crate) fn replace_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    write_new_private_file(path, bytes)
}

fn warn_if_readable_by_others(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path)
            && meta.permissions().mode() & 0o077 != 0
        {
            tracing::warn!(
                path = %path.display(),
                "the secret key file can be read by other users; run chmod 600 on it"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Seals `plaintext` with XChaCha20-Poly1305 under a subkey for `purpose`. The output is a
/// version byte, the random 24-byte nonce, and the ciphertext with its tag.
pub fn seal(key: &SecretKey, purpose: &str, plaintext: &[u8]) -> Result<Vec<u8>, AuthError> {
    let cipher = cipher(key, purpose);
    let mut nonce = [0u8; NONCE_LEN];
    fill_random(&mut nonce)?;
    let sealed = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad: purpose.as_bytes(),
            },
        )
        .map_err(|_| AuthError::Invalid(String::from("cannot seal this value")))?;
    let mut out = Vec::with_capacity(1 + NONCE_LEN + sealed.len());
    out.push(SEAL_VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Opens what [`seal`] sealed for the same purpose. Fails with `Invalid` for anything that
/// was tampered with, sealed for another purpose, or sealed under another key.
pub fn open(
    key: &SecretKey,
    purpose: &str,
    sealed: &[u8],
) -> Result<Zeroizing<Vec<u8>>, AuthError> {
    let unsealable = || AuthError::Invalid(String::from("cannot open this sealed value"));
    let (&version, rest) = sealed.split_first().ok_or_else(unsealable)?;
    if version != SEAL_VERSION || rest.len() < NONCE_LEN + TAG_LEN {
        return Err(unsealable());
    }
    let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
    let nonce = <[u8; NONCE_LEN]>::try_from(nonce).map_err(|_| unsealable())?;
    cipher(key, purpose)
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: ciphertext,
                aad: purpose.as_bytes(),
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| unsealable())
}

fn cipher(key: &SecretKey, purpose: &str) -> XChaCha20Poly1305 {
    let subkey = key.subkey(&format!("seal {purpose}"));
    XChaCha20Poly1305::new(&Key::from(*subkey))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn key(byte: u8) -> SecretKey {
        SecretKey::from_bytes([byte; 32])
    }

    proptest! {
        #[test]
        fn seal_then_open_round_trips(plaintext in proptest::collection::vec(any::<u8>(), 0..512), purpose in "[a-z ]{1,24}") {
            let key = key(3);
            let sealed = seal(&key, &purpose, &plaintext).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let opened = open(&key, &purpose, &sealed).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_eq!(opened.as_slice(), plaintext.as_slice());
        }

        #[test]
        fn tampered_ciphertext_fails_to_open(plaintext in proptest::collection::vec(any::<u8>(), 1..256), flip in any::<usize>(), bit in 0u8..8) {
            let key = key(3);
            let mut sealed = seal(&key, "app password", &plaintext).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let index = flip % sealed.len();
            if let Some(byte) = sealed.get_mut(index) {
                *byte ^= 1 << bit;
            }
            prop_assert!(open(&key, "app password", &sealed).is_err());
        }

        #[test]
        fn truncated_sealed_value_fails_to_open(plaintext in proptest::collection::vec(any::<u8>(), 0..64), cut in 1usize..80) {
            let key = key(3);
            let sealed = seal(&key, "p", &plaintext).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let keep = sealed.len().saturating_sub(cut);
            prop_assert!(open(&key, "p", sealed.get(..keep).unwrap_or_default()).is_err());
        }
    }

    #[test]
    fn sealing_twice_uses_fresh_nonces() -> Result<(), AuthError> {
        let key = key(1);
        assert_ne!(seal(&key, "p", b"same")?, seal(&key, "p", b"same")?);
        Ok(())
    }

    #[test]
    fn sealed_value_does_not_open_for_another_purpose() -> Result<(), AuthError> {
        let key = key(1);
        let sealed = seal(&key, "app password", b"secret")?;
        assert!(open(&key, "pairing request", &sealed).is_err());
        Ok(())
    }

    #[test]
    fn sealed_value_does_not_open_under_another_key() -> Result<(), AuthError> {
        let sealed = seal(&key(1), "app password", b"secret")?;
        assert!(open(&key(2), "app password", &sealed).is_err());
        Ok(())
    }

    #[test]
    fn subkeys_differ_by_purpose() {
        let key = key(9);
        assert_ne!(*key.subkey("media token"), *key.subkey("seal app password"));
    }

    #[test]
    fn secret_key_file_is_created_private_and_read_back() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("nested").join("secret.key");
        let (first, created) = SecretKey::load_or_create(&path)?;
        assert!(created);
        assert_eq!(std::fs::read(&path)?.len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)?.permissions().mode() & 0o777,
                0o600
            );
        }
        let (second, created) = SecretKey::load_or_create(&path)?;
        assert!(!created);
        assert_eq!(first.fingerprint(), second.fingerprint());
        Ok(())
    }

    #[test]
    fn secret_key_file_accepts_hex() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("secret.key");
        std::fs::write(&path, format!("{}\n", "ab".repeat(32)))?;
        let (key, created) = SecretKey::load_or_create(&path)?;
        assert!(!created);
        assert_eq!(
            key.fingerprint(),
            SecretKey::from_bytes([0xab; 32]).fingerprint()
        );
        Ok(())
    }

    #[test]
    fn short_secret_key_file_is_refused() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("secret.key");
        std::fs::write(&path, b"too short")?;
        assert!(matches!(
            SecretKey::load_or_create(&path),
            Err(AuthError::Invalid(_))
        ));
        Ok(())
    }
}
