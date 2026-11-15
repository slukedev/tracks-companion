//! TLS modes (architecture section 10): certificate files reloaded on change, a self-signed
//! certificate made at first start, and built-in ACME behind the `acme` feature. rustls with
//! the ring provider; HTTP/2 through ALPN.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use axum_server::tls_rustls::RustlsConfig;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use trackside_core::config::{TlsConfig, TlsMode};

/// Installs ring as the process's rustls provider. Safe to call more than once.
pub fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

const ALPN: [&[u8]; 2] = [b"h2", b"http/1.1"];

/// The TLS side of the listeners: the config to serve with, and the self-signed fingerprint.
#[derive(Clone)]
pub struct Tls {
    pub config: RustlsConfig,
    pub fingerprint: Option<String>,
}

impl std::fmt::Debug for Tls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tls")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// SHA-256 of a certificate, uppercase hex pairs joined by colons.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn server_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<ServerConfig> {
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the certificate and key do not match or cannot be used")?;
    config.alpn_protocols = ALPN.iter().map(|p| p.to_vec()).collect();
    Ok(config)
}

fn load_files(cert: &Path, key: &Path) -> anyhow::Result<ServerConfig> {
    let certs = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("cannot read {}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("{} is not a PEM certificate chain", cert.display()))?;
    if certs.is_empty() {
        anyhow::bail!("{} holds no certificate", cert.display());
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("{} is not a PEM private key", key.display()))?;
    server_config(certs, key)
}

/// A self-signed certificate under `<data>/tls`, made once and kept so its fingerprint stays
/// the same for paired devices.
pub fn self_signed(dir: &Path, names: &[String]) -> anyhow::Result<(ServerConfig, String)> {
    std::fs::create_dir_all(dir)?;
    let cert_path = dir.join("self-signed.crt");
    let key_path = dir.join("self-signed.key");
    if !cert_path.exists() || !key_path.exists() {
        let mut subject = vec![String::from("localhost")];
        subject.extend(names.iter().cloned());
        let made = rcgen::generate_simple_self_signed(subject)
            .context("cannot make a self-signed certificate")?;
        std::fs::write(&cert_path, made.cert.pem())?;
        std::fs::write(&key_path, made.signing_key.serialize_pem())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let first = CertificateDer::pem_file_iter(&cert_path)?
        .next()
        .context("the self-signed certificate file is empty")??;
    let print = fingerprint(&first);
    Ok((load_files(&cert_path, &key_path)?, print))
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Reloads certificate files when either changes, checking every `every`.
async fn watch_files(
    config: RustlsConfig,
    cert: PathBuf,
    key: PathBuf,
    every: Duration,
    stop: CancellationToken,
) {
    let mut seen = (modified(&cert), modified(&key));
    let mut ticker = tokio::time::interval(every);
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let now = (modified(&cert), modified(&key));
        if now == seen {
            continue;
        }
        seen = now;
        let (c, k) = (cert.clone(), key.clone());
        match tokio::task::spawn_blocking(move || load_files(&c, &k)).await {
            Ok(Ok(fresh)) => {
                config.reload_from_config(Arc::new(fresh));
                tracing::info!(cert = %cert.display(), "certificate reloaded");
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "new certificate not loaded; keeping the old one");
            }
            Err(e) => tracing::warn!(error = %e, "certificate reload failed"),
        }
    }
}

/// The TLS setup for the configured mode, or `None` for plain HTTP.
pub fn setup(
    config: &TlsConfig,
    data_dir: &Path,
    stop: &CancellationToken,
) -> anyhow::Result<Option<Tls>> {
    install_provider();
    match config.mode {
        TlsMode::Off => Ok(None),
        TlsMode::Files => {
            let cert = config
                .cert_file
                .clone()
                .context("tls.mode is files but tls.cert_file is not set")?;
            let key = config
                .key_file
                .clone()
                .context("tls.mode is files but tls.key_file is not set")?;
            let loaded = load_files(&cert, &key)?;
            let rustls = RustlsConfig::from_config(Arc::new(loaded));
            tokio::spawn(watch_files(
                rustls.clone(),
                cert,
                key,
                Duration::from_secs(30),
                stop.clone(),
            ));
            Ok(Some(Tls {
                config: rustls,
                fingerprint: None,
            }))
        }
        TlsMode::SelfSigned => {
            let names = crate::discovery::local_host_name()
                .map(|h| vec![h.clone(), format!("{h}.local")])
                .unwrap_or_default();
            let (loaded, print) = self_signed(&data_dir.join("tls"), &names)?;
            Ok(Some(Tls {
                config: RustlsConfig::from_config(Arc::new(loaded)),
                fingerprint: Some(print),
            }))
        }
        TlsMode::Acme => acme(config, stop),
    }
}

#[cfg(feature = "acme")]
fn acme(config: &TlsConfig, stop: &CancellationToken) -> anyhow::Result<Option<Tls>> {
    use futures_util::StreamExt;
    if config.acme_domains.is_empty() {
        anyhow::bail!("tls.mode is acme but tls.acme_domains is empty");
    }
    let mut acme = rustls_acme::AcmeConfig::new(config.acme_domains.clone())
        .directory(&config.acme_directory)
        .cache(rustls_acme::caches::DirCache::new(
            config.acme_cache.clone(),
        ));
    if let Some(email) = &config.acme_email {
        acme = acme.contact_push(format!("mailto:{email}"));
    }
    let mut state = acme.state();
    let mut server = (*state.default_rustls_config()).clone();
    server.alpn_protocols = ALPN
        .iter()
        .map(|p| p.to_vec())
        .chain(std::iter::once(b"acme-tls/1".to_vec()))
        .collect();
    let stop = stop.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                event = state.next() => match event {
                    Some(Ok(ok)) => tracing::info!(event = ?ok, "acme"),
                    Some(Err(err)) => tracing::warn!(error = ?err, "acme"),
                    None => return,
                },
            }
        }
    });
    Ok(Some(Tls {
        config: RustlsConfig::from_config(Arc::new(server)),
        fingerprint: None,
    }))
}

#[cfg(not(feature = "acme"))]
fn acme(_config: &TlsConfig, _stop: &CancellationToken) -> anyhow::Result<Option<Tls>> {
    anyhow::bail!("tls.mode is acme but this build has no ACME support (the acme feature)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_certificate_is_made_once_and_keeps_its_fingerprint() -> anyhow::Result<()> {
        install_provider();
        let dir = tempfile::tempdir()?;
        let (_, first) = self_signed(dir.path(), &[String::from("home.local")])?;
        let (_, second) = self_signed(dir.path(), &[])?;
        assert_eq!(first, second);
        assert_eq!(first.len(), 32 * 3 - 1);
        Ok(())
    }

    #[test]
    fn mismatched_files_are_refused() -> anyhow::Result<()> {
        install_provider();
        let dir = tempfile::tempdir()?;
        let cert = dir.path().join("c.pem");
        std::fs::write(&cert, "not a certificate")?;
        assert!(load_files(&cert, &cert).is_err());
        Ok(())
    }
}
