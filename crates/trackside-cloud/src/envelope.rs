//! Client-encrypted setting values. The server stores them as opaque blobs and never sees
//! the key or the plaintext. A value is an envelope when it is an object with the single key
//! `$encrypted`:
//!
//! ```json
//! {"$encrypted": {"v": 1, "alg": "chachapoly", "kid": "icloud-1", "nonce": "...", "ct": "..."}}
//! ```
//!
//! `alg` and `kid` are the client's own labels (open strings). `ct` (and `nonce`, when sent)
//! are base64, standard or URL-safe, with or without padding. `ct` must hold at least
//! [`MIN_CIPHERTEXT`] bytes, since every AEAD adds a tag; a 32-byte `ct` for a 32-byte secret
//! is plaintext in disguise and refused.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use serde_json::Value;

/// The key that marks an envelope.
pub const MARKER: &str = "$encrypted";
/// A 16-byte tag plus at least one byte of plaintext.
pub const MIN_CIPHERTEXT: usize = 17;
/// The largest ciphertext, decoded.
pub const MAX_CIPHERTEXT: usize = 16 * 1024;

/// Settings the server refuses to store unless encrypted by the client: `nearbyKey` is the
/// 32-byte TLS secret a user's nearby devices share, and the server must never be able to
/// read it.
pub const ENCRYPTED_ONLY: &[&str] = &["nearbyKey"];

/// Whether `value` claims to be an envelope (well-formed or not).
pub fn is_envelope(value: &Value) -> bool {
    value.as_object().is_some_and(|o| o.contains_key(MARKER))
}

fn decode(text: &str) -> Option<Vec<u8>> {
    [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD]
        .iter()
        .find_map(|engine| engine.decode(text).ok())
}

/// Checks an envelope's shape. Returns what is wrong, for the rejection.
pub fn check(value: &Value) -> Result<(), &'static str> {
    let outer = value.as_object().ok_or("not an object")?;
    if outer.len() != 1 {
        return Err("an envelope holds only $encrypted");
    }
    let inner = outer
        .get(MARKER)
        .and_then(Value::as_object)
        .ok_or("$encrypted is not an object")?;
    let label = |name: &str, max: usize, required: bool| match inner.get(name) {
        None if !required => Ok(()),
        Some(Value::String(s)) if !s.is_empty() && s.len() <= max => Ok(()),
        _ => Err("alg and kid are short text"),
    };
    label("alg", 64, true)?;
    label("kid", 128, false)?;
    let ct = inner
        .get("ct")
        .and_then(Value::as_str)
        .and_then(decode)
        .ok_or("ct is not base64")?;
    if ct.len() < MIN_CIPHERTEXT || ct.len() > MAX_CIPHERTEXT || ct.len() == 32 {
        return Err("ct is not a ciphertext");
    }
    if let Some(nonce) = inner.get("nonce") {
        let nonce = nonce.as_str().and_then(decode).ok_or("nonce is not base64")?;
        if nonce.len() > 64 {
            return Err("nonce is too long");
        }
    }
    for (name, field) in inner {
        let ok = match field {
            Value::String(s) => s.len() <= 4 * MAX_CIPHERTEXT,
            Value::Number(_) => true,
            _ => false,
        };
        if !ok || name.len() > 64 {
            return Err("envelope fields are text or numbers");
        }
    }
    Ok(())
}

/// Whether `key` may only hold an envelope (or a cleared value).
pub fn encrypted_only(key: &str) -> bool {
    ENCRYPTED_ONLY.contains(&key)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn b64(n: usize) -> String {
        STANDARD.encode(vec![7_u8; n])
    }

    #[test]
    fn well_formed_envelopes_pass() {
        let env = json!({"$encrypted": {"v": 1, "alg": "chachapoly", "kid": "k1",
                         "nonce": b64(12), "ct": b64(48)}});
        assert!(is_envelope(&env));
        assert_eq!(check(&env), Ok(()));
        let url_safe = json!({"$encrypted": {"alg": "x", "ct": URL_SAFE_NO_PAD.encode([255_u8; 49])}});
        assert_eq!(check(&url_safe), Ok(()));
    }

    #[test]
    fn plaintext_in_disguise_and_junk_are_refused() {
        assert!(check(&json!({"$encrypted": {"alg": "x", "ct": b64(32)}})).is_err());
        assert!(check(&json!({"$encrypted": {"alg": "x", "ct": b64(8)}})).is_err());
        assert!(check(&json!({"$encrypted": {"ct": b64(48)}})).is_err());
        assert!(check(&json!({"$encrypted": {"alg": "x", "ct": "not base64!"}})).is_err());
        assert!(check(&json!({"$encrypted": {"alg": "x", "ct": b64(48)}, "extra": 1})).is_err());
        assert!(check(&json!({"$encrypted": {"alg": "x", "ct": b64(48), "aad": {"a": 1}}})).is_err());
        assert!(!is_envelope(&json!(b64(32))));
    }
}
