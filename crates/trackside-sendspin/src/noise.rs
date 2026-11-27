//! Noise `KKpsk2` as Sendspin uses it (connection.md, Encryption): the server initiates, the
//! client responds, both static keys are known up front, and the PSK is mixed in at the end of
//! message 2. Trackside never pairs, so the PSK is always the published Sentinel.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use snow::{HandshakeState, TransportState};
use zeroize::Zeroizing;

use crate::b64;
use crate::identity::Identity;

/// The only suite this client offers. Servers must support both.
pub const SUITE: &str = "25519_ChaChaPoly_SHA256";
pub(crate) const PATTERN: &str = "Noise_KKpsk2_25519_ChaChaPoly_SHA256";

/// `SHA-256("sendspin-sentinel-psk-v1")`, published by the spec.
pub const SENTINEL_PSK: [u8; 32] = [
    0x1b, 0x5e, 0x24, 0xdb, 0xc1, 0xae, 0xd9, 0x5f, 0xc2, 0xa5, 0xa3, 0x38, 0xa9, 0x0c, 0x05, 0xdf,
    0x44, 0xbd, 0x10, 0xf5, 0xec, 0x1f, 0x4c, 0xd6, 0x6c, 0xbf, 0x86, 0x27, 0x27, 0x67, 0xb9, 0xd3,
];

/// The largest Noise message, and so the largest WebSocket binary message after the handshake.
pub const MAX_NOISE_MESSAGE: usize = 65_535;
const TAG_LEN: usize = 16;
/// The most plaintext one transport message carries.
pub const MAX_PLAINTEXT: usize = MAX_NOISE_MESSAGE - TAG_LEN;

/// `base64url(SHA-256("sendspin-psk-id-v1" || psk))`.
pub fn psk_id_for(psk: &[u8; 32]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"sendspin-psk-id-v1");
    hash.update(psk);
    b64::encode_url(&hash.finalize())
}

/// Why the handshake or a transport message failed. All of them close the connection silently.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NoiseError {
    #[error("the handshake could not be set up")]
    Setup,
    #[error("a handshake message failed authentication")]
    Handshake,
    #[error("Noise message 1 carried a malformed payload")]
    Payload,
    #[error("the server referenced a PSK this client does not hold")]
    UnknownPsk,
    #[error("a transport message failed authentication")]
    Transport,
    #[error("a message is larger than one Noise message")]
    TooLarge,
}

#[derive(Deserialize)]
struct Message1Payload {
    psk_id: String,
    psk_category: String,
}

/// Whether the PSK the server referenced is one this client has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PskMatch {
    Sentinel,
    /// A long-term or pairing PSK: an earlier pairing this client does not know about. The
    /// first handshake falls back to the Sentinel; a re-handshake fails.
    Miss,
}

/// The client side of one handshake.
pub(crate) struct Responder {
    state: HandshakeState,
}

/// A finished handshake.
pub(crate) struct Established {
    pub transport: Transport,
    /// The handshake hash `h`, the prologue of the next re-handshake.
    pub hash: [u8; 32],
}

impl Responder {
    pub(crate) fn new(
        identity: &Identity,
        server_static: &[u8; 32],
        prologue: &[u8],
    ) -> Result<Self, NoiseError> {
        let params = PATTERN.parse().map_err(|_| NoiseError::Setup)?;
        let state = snow::Builder::new(params)
            .local_private_key(identity.private())
            .and_then(|b| b.remote_public_key(server_static))
            .and_then(|b| b.prologue(prologue))
            .and_then(|b| b.psk(2, &SENTINEL_PSK))
            .and_then(snow::Builder::build_responder)
            .map_err(|_| NoiseError::Setup)?;
        Ok(Self { state })
    }

    /// Reads Noise message 1 and checks its payload.
    pub(crate) fn read_message_1(&mut self, message: &[u8]) -> Result<PskMatch, NoiseError> {
        let mut payload = Zeroizing::new(vec![0u8; message.len()]);
        let len = self
            .state
            .read_message(message, &mut payload)
            .map_err(|_| NoiseError::Handshake)?;
        let parsed: Message1Payload =
            serde_json::from_slice(&payload[..len]).map_err(|_| NoiseError::Payload)?;
        if parsed.psk_id.len() != 43 || b64::decode_id(&parsed.psk_id).is_none() {
            return Err(NoiseError::Payload);
        }
        match parsed.psk_category.as_str() {
            "sn" if parsed.psk_id == psk_id_for(&SENTINEL_PSK) => Ok(PskMatch::Sentinel),
            "sn" | "lt" | "pr" => Ok(PskMatch::Miss),
            _ => Err(NoiseError::Payload),
        }
    }

    /// Writes Noise message 2 (payload: the literal bytes `{}`) and switches to transport mode.
    pub(crate) fn write_message_2(mut self) -> Result<(Vec<u8>, Established), NoiseError> {
        let mut message = vec![0u8; 256];
        let len = self
            .state
            .write_message(b"{}", &mut message)
            .map_err(|_| NoiseError::Handshake)?;
        message.truncate(len);
        let hash: [u8; 32] = self
            .state
            .get_handshake_hash()
            .try_into()
            .map_err(|_| NoiseError::Setup)?;
        let state = self
            .state
            .into_transport_mode()
            .map_err(|_| NoiseError::Setup)?;
        Ok((
            message,
            Established {
                transport: Transport { state },
                hash,
            },
        ))
    }
}

/// Transport mode: one Noise message per WebSocket binary message, with its own nonces.
pub(crate) struct Transport {
    state: TransportState,
}

impl Transport {
    pub(crate) fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(NoiseError::TooLarge);
        }
        let mut out = vec![0u8; plaintext.len() + TAG_LEN];
        let len = self
            .state
            .write_message(plaintext, &mut out)
            .map_err(|_| NoiseError::Transport)?;
        out.truncate(len);
        Ok(out)
    }

    pub(crate) fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if ciphertext.len() > MAX_NOISE_MESSAGE {
            return Err(NoiseError::TooLarge);
        }
        let mut out = vec![0u8; ciphertext.len()];
        let len = self
            .state
            .read_message(ciphertext, &mut out)
            .map_err(|_| NoiseError::Transport)?;
        out.truncate(len);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sentinel_matches_its_published_derivation() {
        let derived: [u8; 32] = Sha256::digest(b"sendspin-sentinel-psk-v1").into();
        assert_eq!(derived, SENTINEL_PSK);
    }

    #[test]
    fn the_sentinel_psk_id_matches_the_spec() {
        assert_eq!(
            psk_id_for(&SENTINEL_PSK),
            "GFsV9tLaSQm9HcFWpKsgYQOr7wFTvNUtkmFwuVz3zoo"
        );
    }

    #[test]
    fn the_pattern_parses() {
        assert!(PATTERN.parse::<snow::params::NoiseParams>().is_ok());
    }
}
