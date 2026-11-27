//! The two Base64 forms Sendspin uses: base64url without padding for keys, ids and Noise bytes,
//! and standard Base64 with padding for `codec_header`.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

pub(crate) fn encode_url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes base64url without padding, refusing any other spelling of the same bytes.
pub(crate) fn decode_url(text: &str) -> Option<Vec<u8>> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    (URL_SAFE_NO_PAD.encode(&bytes) == text).then_some(bytes)
}

/// Decodes a 43-character base64url id into its 32 bytes.
pub(crate) fn decode_id(text: &str) -> Option<[u8; 32]> {
    if text.len() != 43 {
        return None;
    }
    decode_url(text)?.try_into().ok()
}

pub(crate) fn decode_standard(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text).ok()
}

#[cfg(feature = "testing")]
pub(crate) fn encode_standard(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

#[cfg(test)]
pub(crate) fn decode_hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_must_be_canonical() {
        let id = encode_url(&[7; 32]);
        assert_eq!(decode_id(&id), Some([7; 32]));
        assert_eq!(decode_id(&format!("{id}=")), None);
        assert_eq!(decode_id(&id[..42]), None);
    }
}
