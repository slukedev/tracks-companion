//! Random secrets in their wire formats, their lookup hashes, and constant-time comparison.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::AuthError;

/// Device keys and API keys start with this.
pub const KEY_PREFIX: &str = "tsk_";
/// Characters of a key or app password shown in lists.
pub const SHOWN_PREFIX_LEN: usize = 10;
/// Length of an app password.
pub const APP_PASSWORD_LEN: usize = 24;

/// Crockford base32: no I, L, O or U, so codes survive being read aloud and typed.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Lower case letters and digits without 0, 1, i, l and o, for app passwords typed into old
/// clients' password fields.
const UNAMBIGUOUS: &[u8; 31] = b"23456789abcdefghjkmnpqrstuvwxyz";

pub(crate) fn fill_random(buf: &mut [u8]) -> Result<(), AuthError> {
    getrandom::fill(buf)
        .map_err(|e| AuthError::Io(std::io::Error::other(format!("no system randomness: {e}"))))
}

fn random<const N: usize>() -> Result<Zeroizing<[u8; N]>, AuthError> {
    let mut bytes = Zeroizing::new([0u8; N]);
    fill_random(bytes.as_mut_slice())?;
    Ok(bytes)
}

/// 32 random bytes as base64url: sessions, invites, resets, poll secrets.
pub(crate) fn url_token() -> Result<Zeroizing<String>, AuthError> {
    Ok(Zeroizing::new(
        URL_SAFE_NO_PAD.encode(random::<32>()?.as_slice()),
    ))
}

/// A device key or API key: `tsk_` and 43 base64url characters (256 bits).
pub(crate) fn api_key() -> Result<Zeroizing<String>, AuthError> {
    Ok(Zeroizing::new(format!("{KEY_PREFIX}{}", *url_token()?)))
}

/// The 128-bit secret in a pairing QR payload.
pub(crate) fn qr_secret() -> Result<Zeroizing<String>, AuthError> {
    Ok(Zeroizing::new(
        URL_SAFE_NO_PAD.encode(random::<16>()?.as_slice()),
    ))
}

/// Picks `len` symbols uniformly from `alphabet` by rejection sampling.
fn random_symbols(alphabet: &[u8], len: usize) -> Result<Zeroizing<String>, AuthError> {
    let limit = 256 - (256 % alphabet.len());
    let mut out = Zeroizing::new(String::with_capacity(len));
    let mut pool = Zeroizing::new([0u8; 64]);
    while out.len() < len {
        fill_random(pool.as_mut_slice())?;
        for &byte in pool.iter() {
            if usize::from(byte) < limit
                && out.len() < len
                && let Some(&symbol) = alphabet.get(usize::from(byte) % alphabet.len())
            {
                out.push(char::from(symbol));
            }
        }
    }
    Ok(out)
}

/// A 24-character app password.
pub(crate) fn app_password() -> Result<Zeroizing<String>, AuthError> {
    random_symbols(UNAMBIGUOUS, APP_PASSWORD_LEN)
}

/// The 8-character typed pairing code, without its display dash.
pub(crate) fn pairing_code() -> Result<Zeroizing<String>, AuthError> {
    random_symbols(CROCKFORD, 8)
}

/// The 6 digits a device shows for a pairing request.
pub(crate) fn pair_request_code() -> Result<Zeroizing<String>, AuthError> {
    random_symbols(b"0123456789", 6)
}

/// The setup token: 32 random bytes in Crockford base32, in groups of four.
pub(crate) fn setup_token() -> Result<Zeroizing<String>, AuthError> {
    let bytes = random::<32>()?;
    let plain = crockford(bytes.as_slice());
    let mut grouped = Zeroizing::new(String::with_capacity(plain.len() + plain.len() / 4));
    for (i, c) in plain.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            grouped.push('-');
        }
        grouped.push(c);
    }
    Ok(grouped)
}

fn crockford(bytes: &[u8]) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::new());
    let mut buffer: u16 = 0;
    let mut bits = 0u32;
    for &byte in bytes {
        buffer = (buffer << 8) | u16::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(CROCKFORD[usize::from((buffer >> bits) & 0x1f)]));
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(char::from(
            CROCKFORD[usize::from((buffer << (5 - bits)) & 0x1f)],
        ));
    }
    out
}

/// Normalises typed Crockford text: drops dashes and spaces, upper-cases, and reads O as 0
/// and I or L as 1. `None` when any other character is left.
pub(crate) fn normalize_crockford(text: &str) -> Option<Zeroizing<String>> {
    let mut out = Zeroizing::new(String::with_capacity(text.len()));
    for c in text.chars() {
        let c = match c.to_ascii_uppercase() {
            '-' | ' ' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        if !c.is_ascii() || !CROCKFORD.contains(&(c as u8)) {
            return None;
        }
        out.push(c);
    }
    Some(out)
}

/// Shows a typed pairing code as `XXXX-XXXX`.
pub(crate) fn display_pairing_code(code: &str) -> String {
    let (left, right) = code.split_at(code.len().min(4));
    format!("{left}-{right}")
}

/// Shows 6 digits as `123 456`.
pub(crate) fn display_digits(code: &str) -> String {
    let (left, right) = code.split_at(code.len().min(3));
    format!("{left} {right}")
}

/// SHA-256, as stored for every credential.
pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// SHA-256 with a label in front, so codes of different kinds never share a hash.
pub(crate) fn tagged_sha256(tag: &str, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(tag.as_bytes());
    hasher.update([0]);
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Constant-time equality for secrets of the same length.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

pub(crate) fn unhex(text: &str) -> Option<Zeroizing<Vec<u8>>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    let mut out = Zeroizing::new(Vec::with_capacity(text.len() / 2));
    for pair in text.as_bytes().as_chunks::<2>().0 {
        let pair = std::str::from_utf8(pair).ok()?;
        out.push(u8::from_str_radix(pair, 16).ok()?);
    }
    Some(out)
}

/// The prefix shown in key lists.
pub(crate) fn shown_prefix(secret: &str) -> String {
    secret.chars().take(SHOWN_PREFIX_LEN).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_keys_have_the_documented_format() -> Result<(), AuthError> {
        let key = api_key()?;
        assert_eq!(key.len(), 47);
        assert!(key.starts_with(KEY_PREFIX));
        assert!(
            key[4..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(*key, *api_key()?);
        Ok(())
    }

    #[test]
    fn app_passwords_use_only_unambiguous_characters() -> Result<(), AuthError> {
        for _ in 0..50 {
            let password = app_password()?;
            assert_eq!(password.len(), APP_PASSWORD_LEN);
            assert!(password.bytes().all(|b| UNAMBIGUOUS.contains(&b)));
        }
        Ok(())
    }

    #[test]
    fn setup_token_is_grouped_base32_of_256_bits() -> Result<(), AuthError> {
        let token = setup_token()?;
        let plain = normalize_crockford(&token).ok_or(AuthError::NotFound)?;
        assert_eq!(plain.len(), 52);
        assert!(token.split('-').all(|g| g.len() <= 4));
        Ok(())
    }

    #[test]
    fn typed_codes_normalize_common_misreadings() {
        assert_eq!(
            normalize_crockford("q7rm-4x2k")
                .as_deref()
                .map(String::as_str),
            Some("Q7RM4X2K")
        );
        assert_eq!(
            normalize_crockford("OIL0 ").as_deref().map(String::as_str),
            Some("0110")
        );
        assert!(normalize_crockford("ABCU").is_none());
        assert!(normalize_crockford("ÄBC").is_none());
    }

    #[test]
    fn crockford_encodes_known_vectors() {
        assert_eq!(crockford(&[0]).as_str(), "00");
        assert_eq!(crockford(&[0xff]).as_str(), "ZW");
        assert_eq!(crockford(b"f").as_str(), "CR");
    }

    #[test]
    fn digits_and_codes_display_with_separators() -> Result<(), AuthError> {
        let digits = pair_request_code()?;
        assert!(digits.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(display_digits("481207"), "481 207");
        assert_eq!(display_pairing_code("Q7RM4X2K"), "Q7RM-4X2K");
        Ok(())
    }

    #[test]
    fn tagged_hashes_differ_by_tag() {
        assert_ne!(tagged_sha256("a", b"123"), tagged_sha256("b", b"123"));
        assert_ne!(tagged_sha256("a", b"123"), sha256(b"123"));
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(
            unhex(&hex(&bytes)).as_deref().map(Vec::as_slice),
            Some(&bytes[..])
        );
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }
}
