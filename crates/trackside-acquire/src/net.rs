//! The HTTP client shared by everything here that fetches over the network.

use std::time::Duration;

use crate::provider::ProviderError;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Installs ring as the process's rustls provider if none is, then builds a client. reqwest
/// is built without a provider of its own (build-plan section 6).
pub(crate) fn http_client() -> Result<reqwest::Client, ProviderError> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("trackside/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| ProviderError::Other(format!("cannot build an HTTP client: {e}")))
}
