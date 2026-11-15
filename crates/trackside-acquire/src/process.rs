//! Running helper executables: argument vectors only, killed on drop and on cancel, with
//! stderr kept short for logs and error classification.

use std::ffi::OsString;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

/// How much of stderr is kept, from the end.
pub const STDERR_KEEP: usize = 8 * 1024;

/// ETXTBSY: the executable is still open for writing somewhere, usually for a moment right
/// after it was written while another thread forked.
const TEXT_FILE_BUSY: i32 = 26;
/// ENOEXEC: not a format this kernel can run, such as a binary for another architecture.
const EXEC_FORMAT: i32 = 8;

/// Spawns `program` with `args`, retrying briefly while the file is busy. With `tmp_dir`,
/// the directory is created and passed as `TMPDIR`: a one-file yt-dlp unpacks itself there,
/// and in the container only the data volume is writable and allows execution.
pub async fn spawn(
    program: &Path,
    args: &[OsString],
    stdout: bool,
    tmp_dir: Option<&Path>,
) -> std::io::Result<Child> {
    if let Some(dir) = tmp_dir {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut tries = 0;
    loop {
        let mut command = Command::new(program);
        if let Some(dir) = tmp_dir {
            command.env("TMPDIR", dir);
        }
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(if stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match command.spawn() {
            Err(e) if e.raw_os_error() == Some(TEXT_FILE_BUSY) && tries < 20 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            result => return result.map_err(|e| exec_error(e, program)),
        }
    }
}

/// exec reports ENOENT both for a missing file and for an existing one whose interpreter or
/// ELF loader is missing, such as a glibc binary in a static image. Only the first is "not
/// found".
fn exec_error(e: std::io::Error, program: &Path) -> std::io::Error {
    if e.kind() == std::io::ErrorKind::NotFound && has_dir(program) && program.is_file() {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("{} cannot run on this system", program.display()),
        )
    } else {
        e
    }
}

fn has_dir(program: &Path) -> bool {
    program
        .parent()
        .is_some_and(|dir| !dir.as_os_str().is_empty())
}

/// Whether `e`, from [`spawn`], means the file exists but this system cannot run it.
pub fn cannot_exec(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
    ) || e.raw_os_error() == Some(EXEC_FORMAT)
}

/// Whether the kernel will start `program`: it is spawned and killed at once, so nothing it
/// does after exec matters.
pub fn can_exec(program: &Path) -> bool {
    for _ in 0..20 {
        let spawned = std::process::Command::new(program)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
                return true;
            }
            Err(e) if e.raw_os_error() == Some(TEXT_FILE_BUSY) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("tool");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn an_existing_file_that_cannot_exec_is_not_reported_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "#!/nonexistent/sh\n");
        let e = spawn(&program, &[], false, None).await.unwrap_err();
        assert_ne!(e.kind(), std::io::ErrorKind::NotFound);
        assert!(cannot_exec(&e));
        assert!(!can_exec(&program));
    }

    #[tokio::test]
    async fn a_missing_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let e = spawn(&dir.path().join("missing"), &[], false, None)
            .await
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
        assert!(!cannot_exec(&e));
    }

    #[test]
    fn a_runnable_script_can_exec() {
        let dir = tempfile::tempdir().unwrap();
        assert!(can_exec(&script(dir.path(), "#!/bin/sh\nexit 0\n")));
    }
}

/// A finished run.
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("timed out")]
    TimedOut,
    #[error("cancelled")]
    Cancelled,
}

/// Runs to completion and collects stdout (up to `max_stdout` bytes) and the end of stderr.
pub async fn run(
    program: &Path,
    args: &[OsString],
    timeout: Duration,
    max_stdout: usize,
    cancel: &CancellationToken,
    tmp_dir: Option<&Path>,
) -> Result<Output, RunError> {
    let mut child = spawn(program, args, true, tmp_dir).await?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let collect = async {
        let (out, err) = tokio::join!(read_limited(stdout, max_stdout), keep_tail(stderr));
        let status = child.wait().await?;
        Ok::<_, std::io::Error>(Output {
            status,
            stdout: out?,
            stderr: err,
        })
    };
    tokio::select! {
        result = tokio::time::timeout(timeout, collect) => match result {
            Ok(output) => Ok(output?),
            Err(_) => Err(RunError::TimedOut),
        },
        () = cancel.cancelled() => Err(RunError::Cancelled),
    }
}

async fn read_limited(
    stream: Option<impl AsyncRead + Unpin>,
    max: usize,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let Some(mut stream) = stream else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(out);
        }
        let room = max.saturating_sub(out.len());
        out.extend_from_slice(&buf[..n.min(room)]);
    }
}

/// Reads stderr to the end, keeping the last [`STDERR_KEEP`] bytes.
pub async fn keep_tail(stream: Option<impl AsyncRead + Unpin>) -> String {
    let Some(stream) = stream else {
        return String::new();
    };
    let mut lines = BufReader::new(stream).lines();
    let mut tail = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        push_tail(&mut tail, &line);
    }
    tail
}

/// Appends a line, dropping the oldest text past [`STDERR_KEEP`].
pub fn push_tail(tail: &mut String, line: &str) {
    tail.push_str(line);
    tail.push('\n');
    if tail.len() > STDERR_KEEP {
        let mut cut = tail.len() - STDERR_KEEP;
        while !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail.drain(..cut);
    }
}
