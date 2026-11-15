//! The hash-checked yt-dlp updater (architecture section 9.4).

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::process;

/// yt-dlp's latest release downloads.
pub const DEFAULT_RELEASE_BASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
const MAX_BINARY_BYTES: usize = 128 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(300);
const VERSION_TIMEOUT: Duration = Duration::from_secs(60);

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
    #[error("this install can't run yt-dlp; it needs the acquire image")]
    CannotRun,
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
/// installs it at `target`. It is written beside the target first and moved into place only
/// once `--version` runs, with `tmp_dir` as `TMPDIR` as in normal runs.
pub async fn install(
    client: &reqwest::Client,
    base: &str,
    asset: &str,
    target: &Path,
    tmp_dir: &Path,
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
    let partial = target.with_extension("partial");
    let path = partial.clone();
    tokio::task::spawn_blocking(move || write_executable(&path, &binary))
        .await
        .map_err(|e| UpdateError::Io(std::io::Error::other(e)))??;
    if let Err(e) = check_runs(&partial, tmp_dir).await {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(e);
    }
    tokio::fs::rename(&partial, target).await?;
    Ok(found)
}

async fn check_runs(program: &Path, tmp_dir: &Path) -> Result<(), UpdateError> {
    let args = [OsString::from("--version")];
    let cancel = CancellationToken::new();
    match process::run(
        program,
        &args,
        VERSION_TIMEOUT,
        4096,
        &cancel,
        Some(tmp_dir),
    )
    .await
    {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            tracing::warn!(stderr = %output.stderr.trim(), "the downloaded yt-dlp failed to run");
            Err(UpdateError::CannotRun)
        }
        Err(process::RunError::Io(e)) if !process::cannot_exec(&e) => Err(UpdateError::Io(e)),
        Err(e) => {
            tracing::warn!(error = %e, "the downloaded yt-dlp failed to run");
            Err(UpdateError::CannotRun)
        }
    }
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

fn write_executable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    {
        let mut file = std::fs::File::create(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
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
        install(
            &client(),
            &base,
            "yt-dlp_test",
            &target,
            &dir.path().join("tmp"),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), binary);
    }

    #[tokio::test]
    async fn checks_the_download_with_the_usual_tmpdir() {
        let binary: &[u8] = b"#!/bin/sh\necho \"$TMPDIR\" > \"$(dirname \"$0\")/../seen\"\n";
        let sums = format!("{}  yt-dlp_test\n", hex(&Sha256::digest(binary)));
        let base = serve(binary, sums).await;
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("tmp");
        let target = dir.path().join("bin").join("yt-dlp");
        install(&client(), &base, "yt-dlp_test", &target, &tmp)
            .await
            .unwrap();
        let seen = std::fs::read_to_string(dir.path().join("seen")).unwrap();
        assert_eq!(seen.trim(), tmp.display().to_string());
    }

    #[tokio::test]
    async fn refuses_a_binary_this_system_cannot_run() {
        let binary: &[u8] = b"#!/nonexistent/sh\n";
        let sums = format!("{}  yt-dlp_test\n", hex(&Sha256::digest(binary)));
        let base = serve(binary, sums).await;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let target = bin.join("yt-dlp");
        let result = install(
            &client(),
            &base,
            "yt-dlp_test",
            &target,
            &dir.path().join("tmp"),
        )
        .await;
        assert!(matches!(result, Err(UpdateError::CannotRun)));
        assert_eq!(
            std::fs::read_dir(&bin).unwrap().count(),
            0,
            "nothing is left in bin"
        );
    }

    #[tokio::test]
    async fn refuses_a_binary_that_fails_its_version_check() {
        let binary: &[u8] = b"#!/bin/sh\nexit 1\n";
        let sums = format!("{}  yt-dlp_test\n", hex(&Sha256::digest(binary)));
        let base = serve(binary, sums).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("bin").join("yt-dlp");
        let result = install(
            &client(),
            &base,
            "yt-dlp_test",
            &target,
            &dir.path().join("tmp"),
        )
        .await;
        assert!(matches!(result, Err(UpdateError::CannotRun)));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn refuses_a_binary_whose_hash_differs() {
        let sums = format!("{}  yt-dlp_test\n", hex(&Sha256::digest(b"something else")));
        let base = serve(b"tampered", sums).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("yt-dlp");
        let result = install(&client(), &base, "yt-dlp_test", &target, dir.path()).await;
        assert!(matches!(result, Err(UpdateError::HashMismatch { .. })));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn refuses_an_unlisted_asset() {
        let base = serve(b"x", String::from("abc  another\n")).await;
        let dir = tempfile::tempdir().unwrap();
        let result = install(
            &client(),
            &base,
            "yt-dlp_test",
            &dir.path().join("y"),
            dir.path(),
        )
        .await;
        assert!(matches!(result, Err(UpdateError::NotListed(_))));
    }
}
