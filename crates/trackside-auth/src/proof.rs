//! The endpoint proof (ADR 0011): before Tracks sends a device key or a QR pairing secret to
//! one of a server's addresses, the address answers `GET /ping?challenge=` with an HMAC that
//! only the real server can make, bound to the address the client used.
//!
//! The HMAC key is a hash both sides already hold and that never crosses the network: the
//! SHA-256 of the device key (what the server stores to look the key up), or the stored hash
//! of an offer's QR secret. An address that is a local name or a private, link-local or
//! Tailscale address is only proven to a client that reached the server directly from such
//! an address, so a stranger's network cannot relay the challenge to the server's public
//! address and pass the answer back.

use std::net::IpAddr;

use sha2::{Digest, Sha256};
use trackside_core::repo::{keys, pairings};
use trackside_core::{DeviceId, PairingId};

use crate::{Auth, AuthError};

/// What the client proves the server knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProofKey {
    /// The device's key, named by its public id.
    Device(DeviceId),
    /// An offer's QR secret, named by the offer's id from the pairing payload.
    Offer(PairingId),
}

const LABEL: &[u8] = b"trackside endpoint proof v1";
const BLOCK: usize = 64;

/// HMAC-SHA256 (RFC 2104).
pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// The proof for one challenge, as lowercase hex.
pub fn endpoint_proof(key: &[u8; 32], server_id: &str, host: &str, nonce: &str) -> String {
    let mut message = Vec::with_capacity(LABEL.len() + server_id.len() + host.len() + 64);
    for part in [
        LABEL,
        server_id.as_bytes(),
        host.as_bytes(),
        nonce.as_bytes(),
    ] {
        if !message.is_empty() {
            message.push(0);
        }
        message.extend_from_slice(part);
    }
    crate::token::hex(&hmac_sha256(key, &message))
}

/// A nonce of 16 to 128 characters from the base64url alphabet.
pub fn valid_nonce(nonce: &str) -> bool {
    (16..=128).contains(&nonce.len())
        && nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `host` or `host:port` as the client wrote its address, lowercased.
pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.:[]_%".contains(&b))
}

fn host_part(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    }
}

/// Private, loopback, link-local, unique-local or Tailscale's 100.64.0.0/10.
pub fn is_nearby_address(ip: IpAddr) -> bool {
    let ip = crate::proxy::canonical(ip);
    crate::proxy::is_local_address(ip)
        || matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
}

/// Whether an address names something only reachable nearby: a private, link-local or
/// Tailscale address, a name without a dot, or one under `.local`, `.lan`, `.internal`,
/// `.home.arpa` or `.ts.net`.
pub fn is_nearby_host(authority: &str) -> bool {
    let host = host_part(authority).to_ascii_lowercase();
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_nearby_address(ip);
    }
    let Some((_, tld)) = host.rsplit_once('.') else {
        return true;
    };
    matches!(tld, "local" | "lan" | "internal")
        || host.ends_with(".home.arpa")
        || host.ends_with(".ts.net")
}

impl Auth {
    /// The proof for a `/ping` challenge, or `None` when the key is unknown, revoked or
    /// expired, or the address is a nearby one the client did not reach from nearby.
    pub async fn endpoint_proof(
        &self,
        key: ProofKey,
        server_id: &str,
        host: &str,
        nonce: &str,
        nearby_client: bool,
    ) -> Result<Option<String>, AuthError> {
        let host = host.to_ascii_lowercase();
        if !valid_nonce(nonce) || !valid_host(&host) || (is_nearby_host(&host) && !nearby_client) {
            return Ok(None);
        }
        let now = self.now();
        let secret = self
            .read(move |c| match &key {
                ProofKey::Device(device) => keys::device_secret_hash(c, device, now),
                ProofKey::Offer(offer) => pairings::offer_qr_hash(c, offer, now),
            })
            .await?;
        Ok(secret.map(|k| endpoint_proof(&k, server_id, &host, nonce)))
    }

    /// Whether a client is on a nearby network and reached the server directly or through
    /// a trusted proxy that named it: the condition for proving a nearby address.
    pub fn is_nearby_client(&self, headers: &http::HeaderMap, client: IpAddr) -> bool {
        is_nearby_address(client)
            && (!crate::proxy::has_forwarding(headers) || !self.trusted_proxies.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231() {
        let key = [0x0b; 20];
        assert_eq!(
            crate::token::hex(&hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            crate::token::hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let long = [0xaa; 131];
        assert_eq!(
            crate::token::hex(&hmac_sha256(
                &long,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn the_proof_is_bound_to_server_address_and_nonce() {
        let key = [7u8; 32];
        let base = endpoint_proof(&key, "server", "192.168.1.10:4533", "nonce-aaaaaaaaaaaa");
        assert_eq!(base.len(), 64);
        for other in [
            endpoint_proof(
                &[8u8; 32],
                "server",
                "192.168.1.10:4533",
                "nonce-aaaaaaaaaaaa",
            ),
            endpoint_proof(&key, "other", "192.168.1.10:4533", "nonce-aaaaaaaaaaaa"),
            endpoint_proof(&key, "server", "music.example.com", "nonce-aaaaaaaaaaaa"),
            endpoint_proof(&key, "server", "192.168.1.10:4533", "nonce-bbbbbbbbbbbb"),
        ] {
            assert_ne!(other, base);
        }
    }

    #[test]
    fn the_proof_has_a_fixed_vector_both_apps_check() {
        let key: [u8; 32] = Sha256::digest(b"tsk_example").into();
        assert_eq!(
            endpoint_proof(
                &key,
                "5f0c2b9e-8a3d-4c1e-9b7a-2d6e4f8a1c3b",
                "192.168.1.10:4533",
                "AAAAAAAAAAAAAAAAAAAAAA"
            ),
            PROOF_VECTOR
        );
    }

    const PROOF_VECTOR: &str = "682d7ce06987ceed051cff6a1f7f067f67a95506ce02dd709e0675240bbf6c13";

    #[test]
    fn nearby_hosts_are_told_from_public_ones() {
        for host in [
            "192.168.1.10:4533",
            "10.0.0.2",
            "[fd12::1]:4533",
            "100.101.102.103:4533",
            "home.local:4533",
            "nas",
            "box.tail1234.ts.net",
            "nas.home.arpa",
            "nas.lan",
            "169.254.1.1",
        ] {
            assert!(is_nearby_host(host), "{host}");
        }
        for host in ["music.example.com", "203.0.113.5:4533", "[2001:db8::1]:443"] {
            assert!(!is_nearby_host(host), "{host}");
        }
    }

    #[test]
    fn nonces_and_hosts_are_checked() {
        assert!(valid_nonce("AAAAAAAAAAAAAAAAAAAAAA"));
        assert!(!valid_nonce("short"));
        assert!(!valid_nonce("has spaces in it here"));
        assert!(valid_host("[fd12::1]:4533"));
        assert!(!valid_host("evil.example/x"));
        assert!(!valid_host(""));
    }
}
