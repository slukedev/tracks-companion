//! Starting, stopping and killing the real `trackside` process.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_json::Value;

use crate::client::Client;

/// Where the `trackside` binary is: `TRACKSIDE_BIN`, else in the profile directory that holds
/// the running executable (`target/<profile>/trackside`). Test executables sit below it, in
/// `deps/` or, since Cargo 1.100, in `build/<package>/<hash>/out/`, so look up the ancestors.
pub fn binary() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("TRACKSIDE_BIN") {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe()?;
    let name = format!("trackside{}", std::env::consts::EXE_SUFFIX);
    let dir = exe.parent().context("the executable has no directory")?;
    if let Some(found) = dir
        .ancestors()
        .map(|ancestor| ancestor.join(&name))
        .find(|candidate| candidate.is_file())
    {
        return Ok(found);
    }
    bail!(
        "no {name} above {}; build it with `cargo build -p trackside-server --bin trackside` (add --release for the performance run) or set TRACKSIDE_BIN",
        dir.display()
    )
}

/// How to start a server.
#[derive(Clone, Debug)]
pub struct ServerSpec {
    pub bin: PathBuf,
    pub data_dir: PathBuf,
    /// Settings on top of the test defaults, as `TRACKSIDE_*` environment variables.
    pub env: Vec<(String, String)>,
}

impl ServerSpec {
    pub fn new(bin: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            bin,
            data_dir,
            env: Vec::new(),
        }
    }

    #[must_use]
    pub fn env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.to_owned(), value.into()));
        self
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Defaults that keep tests fast and private: loopback on an ephemeral port, no mDNS, no
/// scheduled backups, cheap password hashing, no settle delay for freshly written files.
const DEFAULTS: &[(&str, &str)] = &[
    ("TRACKSIDE_LOG__FORMAT", "json"),
    ("TRACKSIDE_LOG__LEVEL", "info"),
    ("TRACKSIDE_SERVER__LISTEN", "127.0.0.1:0"),
    ("TRACKSIDE_SERVER__NAME", "E2E"),
    ("TRACKSIDE_SERVER__SHUTDOWN_GRACE", "5s"),
    ("TRACKSIDE_DISCOVERY__MDNS", "false"),
    ("TRACKSIDE_BACKUP__SCHEDULE", "off"),
    ("TRACKSIDE_SCANNER__SETTLE", "0s"),
    ("TRACKSIDE_SCANNER__WATCH_DEBOUNCE", "1s"),
    ("TRACKSIDE_AUTH__ARGON2_MEMORY", "1MiB"),
    ("TRACKSIDE_AUTH__ARGON2_ITERATIONS", "1"),
    ("TRACKSIDE_MEDIA__CACHE_MIN_FREE", "0"),
];

/// Which pipe a log line came from. The two are read by separate threads, so lines of one
/// can land between consecutive lines of the other; anything read as a sequence (the setup
/// banner) is read from its own stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

/// The process's log lines, as they arrive.
#[derive(Debug, Default)]
struct Logs {
    lines: Mutex<Vec<(Stream, String)>>,
    changed: Condvar,
    /// Output streams still open; none once the process has exited.
    open: std::sync::atomic::AtomicUsize,
}

impl Logs {
    fn push(&self, stream: Stream, line: String) {
        if std::env::var_os("TRACKSIDE_E2E_LOGS").is_some() {
            eprintln!("[trackside] {line}");
        }
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((stream, line));
        self.changed.notify_all();
    }

    fn wait_for<T>(
        &self,
        timeout: Duration,
        mut find: impl FnMut(&[(Stream, String)]) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(found) = find(&lines) {
                return Some(found);
            }
            if self.open.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                return None;
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            lines = self
                .changed
                .wait_timeout(lines, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// A running `trackside serve`.
#[derive(Debug)]
pub struct Server {
    child: Option<Child>,
    logs: Arc<Logs>,
    pub spec: ServerSpec,
    pub addr: SocketAddr,
    pub started_at: Instant,
}

/// A log line's message and fields, when it is JSON.
pub fn log_fields(line: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(line).ok()?;
    value.get("fields").cloned()
}

impl Server {
    /// Starts the server and waits until it listens.
    pub fn start(spec: ServerSpec) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&spec.data_dir)?;
        let mut command = Command::new(&spec.bin);
        command
            .arg("--data-dir")
            .arg(&spec.data_dir)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, _) in std::env::vars() {
            let proxy = key.eq_ignore_ascii_case("http_proxy")
                || key.eq_ignore_ascii_case("https_proxy")
                || key.eq_ignore_ascii_case("all_proxy");
            if key.starts_with("TRACKSIDE_") || key == "RUST_LOG" || proxy {
                command.env_remove(&key);
            }
        }
        for (key, value) in DEFAULTS {
            if spec.value(key).is_none() {
                command.env(key, value);
            }
        }
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        let started_at = Instant::now();
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start {}", spec.bin.display()))?;
        let logs = Arc::new(Logs::default());
        for (kind, stream) in [
            (
                Stream::Stderr,
                child
                    .stderr
                    .take()
                    .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            ),
            (
                Stream::Stdout,
                child
                    .stdout
                    .take()
                    .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            ),
        ]
        .into_iter()
        .filter_map(|(kind, stream)| Some((kind, stream?)))
        {
            let logs = Arc::clone(&logs);
            logs.open.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines() {
                    match line {
                        Ok(line) => logs.push(kind, line),
                        Err(_) => break,
                    }
                }
                let _guard = logs.lines.lock().unwrap_or_else(PoisonError::into_inner);
                logs.open.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                logs.changed.notify_all();
            });
        }
        let addr = logs.wait_for(Duration::from_secs(60), |lines| {
            lines.iter().find_map(|(_, line)| {
                let fields = log_fields(line)?;
                if fields.get("message")?.as_str()? != "listening" {
                    return None;
                }
                fields.get("address")?.as_str()?.parse::<SocketAddr>().ok()
            })
        });
        let mut server = Self {
            child: Some(child),
            logs,
            spec,
            addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            started_at,
        };
        match addr {
            Some(addr) => {
                server.addr = addr;
                Ok(server)
            }
            None => {
                let status = server.kill();
                bail!(
                    "trackside did not start listening ({status:?}); its log:\n{}",
                    server.log_text()
                )
            }
        }
    }

    /// The origin, `http://127.0.0.1:<port>`.
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// A client for this server, honouring `server.base_path`.
    pub fn client(&self) -> Client {
        let base_path = self
            .spec
            .value("TRACKSIDE_SERVER__BASE_PATH")
            .unwrap_or("")
            .trim_end_matches('/')
            .to_owned();
        Client::new(&self.origin(), &base_path)
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    pub fn data_dir(&self) -> &Path {
        &self.spec.data_dir
    }

    /// Every log line so far.
    pub fn log_lines(&self) -> Vec<String> {
        self.logs
            .lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, line)| line.clone())
            .collect()
    }

    pub fn log_text(&self) -> String {
        self.log_lines().join("\n")
    }

    /// Waits for a log line `matches` accepts.
    pub fn wait_log(&self, timeout: Duration, matches: impl Fn(&str) -> bool) -> Option<String> {
        self.logs.wait_for(timeout, |lines| {
            lines
                .iter()
                .find(|(_, l)| matches(l))
                .map(|(_, l)| l.clone())
        })
    }

    /// The setup token from the first-run banner in the log.
    pub fn setup_token(&self) -> Option<String> {
        self.logs.wait_for(Duration::from_secs(10), setup_token_in)
    }

    /// Asks the process to stop with SIGTERM and waits for it.
    pub fn stop(&mut self) -> anyhow::Result<ExitStatus> {
        let Some(child) = self.child.as_mut() else {
            bail!("already stopped");
        };
        let pid = child.id().to_string();
        let sent = Command::new("kill").args(["-TERM", &pid]).status()?;
        if !sent.success() {
            bail!("kill -TERM {pid} failed");
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait()? {
                self.child = None;
                return Ok(status);
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                bail!("trackside did not stop within 30 s of SIGTERM");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kills the process outright (SIGKILL), as a power cut or OOM kill would.
    pub fn kill(&mut self) -> Option<ExitStatus> {
        let mut child = self.child.take()?;
        let _ = child.kill();
        child.wait().ok()
    }

    /// Whether the process is still running.
    pub fn running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    /// Starts the same data directory again on the same port.
    pub fn restart(&mut self) -> anyhow::Result<()> {
        if self.running() {
            self.stop()?;
        }
        let spec = self
            .spec
            .clone()
            .env("TRACKSIDE_SERVER__LISTEN", self.addr.to_string());
        let next = Self::start(spec)?;
        *self = next;
        Ok(())
    }

    /// The resident set size in KiB, from `/proc` (Linux only).
    pub fn rss_kib(&self) -> Option<u64> {
        self.proc_status("VmRSS:")
    }

    /// Resident anonymous memory in KiB: the RSS without file-backed pages (the binary and
    /// the memory-mapped database), which are the OS page cache's.
    pub fn anon_rss_kib(&self) -> Option<u64> {
        self.proc_status("RssAnon:")
    }

    fn proc_status(&self, field: &str) -> Option<u64> {
        let pid = self.pid()?;
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        status
            .lines()
            .find_map(|l| l.strip_prefix(field))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
    }
}

/// The line after the banner's "enter this setup token:" on stderr, where the banner is
/// printed. Log lines from stdout may sit between the two in the merged list.
fn setup_token_in(lines: &[(Stream, String)]) -> Option<String> {
    let mut stderr = lines
        .iter()
        .filter(|(stream, _)| *stream == Stream::Stderr)
        .map(|(_, line)| line);
    stderr.find(|l| l.contains("enter this setup token:"))?;
    stderr.next().map(|l| l.trim().to_owned())
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setup_token_is_read_from_stderr_whatever_stdout_interleaves() {
        let banner =
            "Trackside needs an admin account. Open http://h/setup and enter this setup token:";
        let lines = [
            (Stream::Stderr, String::from(banner)),
            (
                Stream::Stdout,
                String::from(r#"{"fields":{"message":"setup token printed"}}"#),
            ),
            (Stream::Stderr, String::from("  ABCD-EFGH")),
        ];
        assert_eq!(setup_token_in(&lines).as_deref(), Some("ABCD-EFGH"));
        assert_eq!(
            setup_token_in(&lines[..2]),
            None,
            "waits for the token line"
        );
    }
}
