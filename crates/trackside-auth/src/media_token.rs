//! Media tokens: `base64url(JSON {u, d, e, x}) . base64url(BLAKE3 keyed MAC)`. Nothing is
//! stored. A device's token is checked against the device's current `media_epoch`; a token
//! minted for an API key or app password (`k` instead of `d`) is checked against that key
//! still being active.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use crate::token::ct_eq;

/// Longest token accepted, well above what the payload needs.
const MAX_LEN: usize = 512;

/// What a media token says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Claims {
    /// User pid.
    pub u: String,
    /// Device pid, for tokens minted by a device key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d: Option<String>,
    /// Key pid, for tokens minted by an API key or app password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<String>,
    /// The device's media epoch when minted; 0 for key tokens.
    pub e: i64,
    /// Expiry, Unix milliseconds by the server clock.
    pub x: i64,
}

pub(crate) fn mint(key: &[u8; 32], claims: &Claims) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap_or_default());
    let mac = blake3::keyed_hash(key, payload.as_bytes());
    format!("{payload}.{}", URL_SAFE_NO_PAD.encode(mac.as_bytes()))
}

/// The claims of a token this key minted, or `None` for anything else. Expiry is the
/// caller's check.
pub(crate) fn verify(key: &[u8; 32], token: &str) -> Option<Claims> {
    if token.len() > MAX_LEN {
        return None;
    }
    let (payload, mac) = token.split_once('.')?;
    let mac = URL_SAFE_NO_PAD.decode(mac).ok()?;
    let expected = blake3::keyed_hash(key, payload.as_bytes());
    if !ct_eq(&mac, expected.as_bytes()) {
        return None;
    }
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> Claims {
        Claims {
            u: String::from("u0123456789abcdefghj"),
            d: Some(String::from("d0123456789abcdefghj")),
            k: None,
            e: 3,
            x: 1_795_253_400_000,
        }
    }

    #[test]
    fn minted_token_verifies() {
        let key = [4u8; 32];
        let token = mint(&key, &claims());
        assert_eq!(verify(&key, &token), Some(claims()));
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        );
    }

    #[test]
    fn token_from_another_key_is_rejected() {
        let token = mint(&[4u8; 32], &claims());
        assert_eq!(verify(&[5u8; 32], &token), None);
    }

    #[test]
    fn edited_payload_is_rejected() {
        let key = [4u8; 32];
        let token = mint(&key, &claims());
        let (_, mac) = token.split_once('.').unwrap_or_default();
        let mut longer = claims();
        longer.x += 1_000_000;
        let forged_payload =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&longer).unwrap_or_default());
        assert_eq!(verify(&key, &format!("{forged_payload}.{mac}")), None);
        assert_eq!(verify(&key, "garbage"), None);
        assert_eq!(verify(&key, &"a".repeat(600)), None);
    }
}
