//! Per-library limits on blocking filesystem calls.
//!
//! A hung NFS hard mount blocks `open` and `stat` forever, each on a blocking-pool thread. If
//! every stream request did that freely, a few hundred of them would take the whole pool and
//! stall the database with it. So calls on a library's paths run behind that library's
//! semaphore; once all its permits are stuck, further requests fail fast with
//! [`MediaError::LibraryUnavailable`] instead of taking another thread.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::Semaphore;
use trackside_core::LibraryKey;

use crate::MediaError;

/// Blocking calls in flight on one library's paths.
pub(crate) const LIBRARY_PERMITS: usize = 16;
/// How long a request waits for a permit before the library counts as unavailable.
pub(crate) const LIBRARY_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub(crate) struct LibraryGate {
    permits: usize,
    wait: Duration,
    libraries: Mutex<HashMap<LibraryKey, Arc<Semaphore>>>,
}

impl LibraryGate {
    pub(crate) fn new(permits: usize, wait: Duration) -> Self {
        Self {
            permits: permits.max(1),
            wait,
            libraries: Mutex::default(),
        }
    }

    fn semaphore(&self, library: LibraryKey) -> Arc<Semaphore> {
        let mut libraries = self
            .libraries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            libraries
                .entry(library)
                .or_insert_with(|| Arc::new(Semaphore::new(self.permits))),
        )
    }

    /// Runs `call` on the blocking pool under `library`'s semaphore. The permit moves into the
    /// blocking task, so it is held until the call returns even when the request is dropped
    /// first: a stuck call keeps its permit, and the count of stuck threads stays bounded.
    pub(crate) async fn run<T, F>(&self, library: LibraryKey, call: F) -> Result<T, MediaError>
    where
        T: Send + 'static,
        F: FnOnce() -> std::io::Result<T> + Send + 'static,
    {
        let semaphore = self.semaphore(library);
        let Ok(Ok(permit)) = tokio::time::timeout(self.wait, semaphore.acquire_owned()).await
        else {
            tracing::warn!(?library, "library filesystem calls are stuck");
            return Err(MediaError::LibraryUnavailable);
        };
        tokio::task::spawn_blocking(move || {
            let result = call();
            drop(permit);
            result
        })
        .await
        .map_err(|e| MediaError::Process(format!("filesystem task: {e}")))?
        .map_err(MediaError::from)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[tokio::test]
    async fn a_library_with_every_permit_stuck_is_unavailable_and_others_are_not()
    -> Result<(), Box<dyn std::error::Error>> {
        let gate = Arc::new(LibraryGate::new(2, Duration::from_millis(100)));
        let stuck = LibraryKey(1);
        let other = LibraryKey(2);
        let (release, wait) = mpsc::channel::<()>();
        let wait = Arc::new(Mutex::new(wait));
        let mut hung = Vec::new();
        for _ in 0..2 {
            let gate = Arc::clone(&gate);
            let wait = Arc::clone(&wait);
            hung.push(tokio::spawn(async move {
                gate.run(stuck, move || {
                    let _ = wait.lock().unwrap_or_else(PoisonError::into_inner).recv();
                    Ok(())
                })
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = std::time::Instant::now();
        let blocked = gate.run(stuck, || Ok(())).await;
        assert!(matches!(blocked, Err(MediaError::LibraryUnavailable)));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(gate.run(other, || Ok(7)).await.is_ok_and(|n| n == 7));

        // A dropped request does not give back a permit its call still holds.
        hung.remove(0).abort();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(matches!(
            gate.run(stuck, || Ok(())).await,
            Err(MediaError::LibraryUnavailable)
        ));

        release.send(())?;
        release.send(())?;
        for task in hung {
            task.await??;
        }
        gate.run(stuck, || Ok(())).await?;
        Ok(())
    }
}
