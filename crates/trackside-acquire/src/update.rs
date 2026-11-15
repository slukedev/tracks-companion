//! The hash-checked yt-dlp updater (architecture section 9.4).

use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// yt-dlp's latest release downloads.
pub const DEFAULT_RELEASE_BASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
const MAX_BINARY_BYTES: usize = 128 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("download failed: {0}")]
    Http(String),
    #[error("{0} is not listed in SHA2-256SUMS")]
    NotListed(String),
    #[error("hash mismatch: expected {expected}, got {found}")]
    HashMismatch { expected: String, found: String },
    #[error("the download is too large")]
    TooLarge,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The release asset for this platform: the standalone binary where one exists.
pub fn asset_name() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "yt-dlp_linux",
        ("linux", "aarch64") => "yt-dlp_linux_aarch64",
        ("macos", _) => "yt-dlp_macos",
        _ => "yt-dlp",
    }
}

/// Downloads the release asset and its checksum list from `base`, checks the SHA-256, and
/// installs it at `target` (written beside it first, then renamed).
pub async fn install(
    client: &reqwest::Client,
    base: &str,
    asset: &str,
    target: &Path,
) -> Result<String, UpdateError> {
    let base = base.trim_end_matches('/');
    let sums = get(client, &format!("{base}/SHA2-256SUMS"), 1024 * 1024).await?;
    let sums = String::from_utf8_lossy(&sums);
    let expected = sums
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?;
            let name = parts.next()?.trim_start_matches('*');
            (name == asset).then(|| hash.to_ascii_lowercase())
        })
        .ok_or_else(|| UpdateError::NotListed(asset.to_owned()))?;
    let binary = get(client, &format!("{base}/{asset}"), MAX_BINARY_BYTES).await?;
    let found = hex(&Sha256::digest(&binary));
    if found != expected {
        return Err(UpdateError::HashMismatch { expected, found });
    }
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || write_executable(&target, &binary))
        .await
        .map_err(|e| UpdateError::Io(std::io::Error::other(e)))??;
    Ok(found)
}

async fn get(client: &reqwest::Client, url: &str, max: usize) -> Result<Vec<u8>, UpdateError> {
    let mut response = client
        .get(url)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| UpdateError::Http(e.without_url().to_string()))?;
    if !response.status().is_success() {
        return Err(UpdateError::Http(format!(
            "{} from {url}",
            response.status()
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| UpdateError::Http(e.without_url().to_string()))?
    {
        if body.len() + chunk.len() > max {
            return Err(UpdateError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn write_executable(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial: PathBuf = target.with_extension("partial");
    {
        let mut file = std::fs::File::create(&partial)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&partial, target)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get as route;

    async fn serve(binary: &'static [u8], sums: String) -> String {
        let app = Router::new()
            .route("/SHA2-256SUMS", route(move || async move { sums }))
            .route("/yt-dlp_test", route(move || async move { binary }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{addr}")
    }

    fn client() -> reqwest::Client {
        crate::slskd::http_client().unwrap()
    }

    #[tokio::test]
    async fn installs_a_binary_whose_hash_matches() {
        let binary: &[u8] = b"#!/bin/sh\necho 2026.11.03\n";
        let sums = format!(
            "{}  yt-dlp_test\nabc  other\n",
            hex(&Sha256::digest(binary))
        );
        let base = serve(binary, sums).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("bin").join("yt-dlp");
        install(&client(), &base, "yt-dlp_test", &target)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), binary);
    }

    #[tokio::test]
    async fn refuses_a_binary_whose_hash_differs() {
        let sums = format!("{}  yt-dlp_test\n", hex(&Sha256::digest(b"something else")));
        let base = serve(b"tampered", sums).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("yt-dlp");
        let result = install(&client(), &base, "yt-dlp_test", &target).await;
        assert!(matches!(result, Err(UpdateError::HashMismatch { .. })));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn refuses_an_unlisted_asset() {
        let base = serve(b"x", String::from("abc  another\n")).await;
        let dir = tempfile::tempdir().unwrap();
        let result = install(&client(), &base, "yt-dlp_test", &dir.path().join("y")).await;
        assert!(matches!(result, Err(UpdateError::NotListed(_))));
    }
}
