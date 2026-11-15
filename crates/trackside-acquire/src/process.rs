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
            result => return result,
        }
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
