//! The availability monitor. Each library root is probed on its own short-lived thread with a
//! deadline, so a hung network mount marks its library unavailable instead of blocking the
//! scanner, and a probe that never returns is not started again until it does.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use trackside_core::control::{ScanKind, ScanTrigger};
use trackside_core::{LibraryId, LibraryKey};

use crate::catalog::RootHealth;
use crate::reader::dev_of;
use crate::scanner::{Inner, PROBE_INTERVAL};

/// How long a probe may take before the library counts as unavailable.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(5);

/// What a probe saw at the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootProbe {
    /// A folder on device `dev`, with or without entries.
    Present {
        dev: Option<u64>,
        empty: bool,
    },
    Missing(String),
    TimedOut,
}

/// Looks at the root: is it a folder, on which device, and is it empty.
pub fn look(root: &Path) -> RootProbe {
    match std::fs::metadata(root) {
        Ok(meta) if meta.is_dir() => match std::fs::read_dir(root) {
            Ok(mut entries) => RootProbe::Present {
                dev: dev_of(&meta),
                empty: entries.next().is_none(),
            },
            Err(e) => RootProbe::Missing(format!("the library folder cannot be listed: {e}")),
        },
        Ok(_) => RootProbe::Missing(String::from("the library path is not a folder")),
        Err(e) => RootProbe::Missing(format!("the library folder cannot be reached: {e}")),
    }
}

/// [`look`] on a separate thread, giving up after `deadline`. `done` runs when the look
/// returns, however late.
pub fn look_with_deadline(
    root: PathBuf,
    deadline: Duration,
    done: impl FnOnce() + Send + 'static,
) -> RootProbe {
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("trackside-probe".into())
        .spawn(move || {
            let result = look(&root);
            done();
            let _ = tx.send(result);
        });
    if spawned.is_err() {
        return RootProbe::Missing(String::from("the probe could not start"));
    }
    rx.recv_timeout(deadline).unwrap_or(RootProbe::TimedOut)
}

/// Why a library should be unavailable, or `None` when it looks healthy. An empty root or a
/// different device only counts when the library had songs, so a new empty library is fine.
pub fn judge(probe: &RootProbe, health: Option<RootHealth>, known_songs: u64) -> Option<String> {
    match probe {
        RootProbe::TimedOut => Some(format!(
            "the library folder did not answer within {} s",
            PROBE_DEADLINE.as_secs()
        )),
        RootProbe::Missing(reason) => Some(reason.clone()),
        RootProbe::Present { dev, empty } => {
            if known_songs > 0 && *empty {
                return Some(String::from(
                    "the library folder is empty; is the share mounted?",
                ));
            }
            if let (Some(health), Some(now)) = (health, dev)
                && known_songs > 0
                && health.dev.is_some_and(|before| before != *now)
            {
                return Some(String::from(
                    "the library folder is on a different device than at the last scan; is the share mounted?",
                ));
            }
            None
        }
    }
}

/// Starts the monitor thread. Its first round also queues the startup quick scans.
pub fn spawn(inner: Arc<Inner>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("trackside-monitor".into())
        .spawn(move || {
            probe_all(&inner, true);
            while !inner.shutdown.is_cancelled() {
                let state = inner.lock();
                let _ = inner_wait(&inner, state);
                if inner.shutdown.is_cancelled() {
                    break;
                }
                probe_all(&inner, false);
            }
        })?;
    Ok(())
}

/// Waits one probe interval, returning early (false) on shutdown.
fn inner_wait(inner: &Inner, state: std::sync::MutexGuard<'_, crate::scanner::State>) -> bool {
    let deadline = std::time::Instant::now() + PROBE_INTERVAL;
    let mut state = state;
    loop {
        if inner.shutdown.is_cancelled() {
            return false;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return true;
        }
        state = inner.wait(state, (deadline - now).min(Duration::from_secs(1)));
    }
}

/// Probes every enabled library once and applies the verdicts.
pub fn probe_all(inner: &Arc<Inner>, startup: bool) {
    let libraries = inner.refresh_libraries();
    let mut startup_scans: Vec<LibraryId> = Vec::new();
    for library in libraries.iter().filter(|l| l.enabled) {
        crate::watch::add_library(inner, library);
        let key = library.key;
        {
            let mut state = inner.lock();
            let Some(lib) = state.libs.get_mut(&key) else {
                continue;
            };
            if lib.probing {
                continue;
            }
            lib.probing = true;
        }
        let done = {
            let inner = Arc::clone(inner);
            move || clear_probing(&inner, key)
        };
        let probe = look_with_deadline(library.root.clone(), PROBE_DEADLINE, done);
        let health = inner.services.catalog.root_health(key).ok().flatten();
        let verdict = judge(&probe, health, library.song_count);
        inner.set_available(key, verdict.is_none(), verdict.as_deref());
        if startup && verdict.is_none() {
            startup_scans.push(library.id.clone());
        }
    }
    for id in startup_scans {
        if let Err(error) = inner.request_scan(&id, ScanKind::Quick, ScanTrigger::Startup, false) {
            tracing::warn!(library = %id, %error, "startup scan not queued");
        }
    }
}

fn clear_probing(inner: &Inner, key: LibraryKey) {
    if let Some(lib) = inner.lock().libs.get_mut(&key) {
        lib.probing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_present_root_with_songs_is_healthy() {
        let probe = RootProbe::Present {
            dev: Some(1),
            empty: false,
        };
        let health = Some(RootHealth {
            dev: Some(1),
            entry_count: 3,
        });
        assert_eq!(judge(&probe, health, 10), None);
    }

    #[test]
    fn an_empty_root_is_unavailable_only_when_songs_were_known() {
        let probe = RootProbe::Present {
            dev: Some(1),
            empty: true,
        };
        assert!(judge(&probe, None, 10).is_some());
        assert_eq!(judge(&probe, None, 0), None);
    }

    #[test]
    fn a_root_on_another_device_is_unavailable() {
        let probe = RootProbe::Present {
            dev: Some(2),
            empty: false,
        };
        let health = Some(RootHealth {
            dev: Some(1),
            entry_count: 3,
        });
        assert!(judge(&probe, health, 10).is_some_and(|r| r.contains("different device")));
    }

    #[test]
    fn missing_and_hung_roots_are_unavailable() {
        assert!(judge(&RootProbe::TimedOut, None, 0).is_some());
        assert!(judge(&RootProbe::Missing("gone".into()), None, 0).is_some());
    }

    #[test]
    fn look_sees_missing_empty_and_full_roots() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(matches!(
            look(&dir.path().join("nope")),
            RootProbe::Missing(_)
        ));
        assert!(matches!(
            look(dir.path()),
            RootProbe::Present { empty: true, .. }
        ));
        std::fs::write(dir.path().join("a.flac"), b"x")?;
        assert!(matches!(
            look(dir.path()),
            RootProbe::Present { empty: false, .. }
        ));
        Ok(())
    }
}
