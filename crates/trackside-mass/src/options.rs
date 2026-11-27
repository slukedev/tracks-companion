use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use crate::backoff::Backoff;
use crate::endpoint::Endpoint;
use crate::token::Token;

/// How to reach Music Assistant and how patient to be. Start from [`Options::new`] and adjust
/// the fields; the defaults suit a server on the same LAN.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Options {
    pub endpoint: Endpoint,
    pub token: Token,
    /// TCP, TLS and the WebSocket upgrade.
    pub connect_timeout: Duration,
    /// The greeting and the answer to `auth`, each.
    pub handshake_timeout: Duration,
    /// The default for a command, from the call until its last chunk.
    pub request_timeout: Duration,
    /// Commands sent and not yet answered. Music Assistant drops clients that let more than
    /// 512 messages queue, so keep this well below that.
    pub max_in_flight: usize,
    /// How often to ping. Music Assistant pings every 25 s itself.
    pub ping_interval: Duration,
    /// A connection that delivers nothing, not even a pong, for this long is dead.
    pub dead_after: Duration,
    /// How long a single write may take before the connection counts as dead.
    pub write_timeout: Duration,
    pub backoff: Backoff,
    /// The wait before trying again after the token was refused.
    pub auth_retry: Duration,
    /// The wait before trying again with a server of an incompatible schema.
    pub incompatible_retry: Duration,
    /// A connection that stays up this long resets the backoff.
    pub stable_after: Duration,
    /// Updates buffered per subscriber before it lags and gets a resync.
    pub event_capacity: usize,
    /// The largest single WebSocket message accepted.
    pub max_message_bytes: usize,
    /// The most items a chunked answer may assemble.
    pub max_result_items: usize,
    /// For `wss://` with a private CA or a self-signed certificate: trust only these
    /// certificates instead of the platform's store.
    pub custom_ca: Option<CustomCa>,
}

/// Certificate authorities (or a self-signed certificate) to trust for `wss://`.
#[derive(Clone, PartialEq, Eq)]
pub struct CustomCa(pub(crate) Arc<Vec<CertificateDer<'static>>>);

/// Why a PEM bundle cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CaError {
    #[error("the PEM text holds no certificate")]
    Empty,
    #[error("the PEM text is not valid: {0}")]
    Invalid(String),
    #[error("a certificate in the PEM text is not one rustls accepts: {0}")]
    Rejected(String),
}

impl CustomCa {
    /// Every `CERTIFICATE` block of a PEM text, checked to load as trust anchors.
    pub fn from_pem(pem: &[u8]) -> Result<Self, CaError> {
        let certs = CertificateDer::pem_slice_iter(pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| CaError::Invalid(e.to_string()))?;
        if certs.is_empty() {
            return Err(CaError::Empty);
        }
        let mut roots = rustls::RootCertStore::empty();
        for cert in &certs {
            roots
                .add(cert.clone())
                .map_err(|e| CaError::Rejected(e.to_string()))?;
        }
        Ok(Self(Arc::new(certs)))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for CustomCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CustomCa({} certificates)", self.0.len())
    }
}

impl Options {
    pub fn new(endpoint: Endpoint, token: Token) -> Self {
        Self {
            endpoint,
            token,
            connect_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_in_flight: 64,
            ping_interval: Duration::from_secs(20),
            dead_after: Duration::from_secs(50),
            write_timeout: Duration::from_secs(10),
            backoff: Backoff::default(),
            auth_retry: Duration::from_mins(5),
            incompatible_retry: Duration::from_mins(15),
            stable_after: Duration::from_secs(30),
            event_capacity: 1024,
            max_message_bytes: 64 << 20,
            max_result_items: 1_000_000,
            custom_ca: None,
        }
    }
}
