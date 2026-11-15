//! One process per data directory: an exclusive lock on `<data>/trackside.lock`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Held for the life of the server. The lock is released when this is dropped or the process
/// ends, however it ends.
#[derive(Debug)]
pub struct DataDirLock {
    _file: File,
    path: PathBuf,
}

/// Why the lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another Trackside server is using {0}")]
    Held(PathBuf),
    #[error("cannot open {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub const LOCK_FILE: &str = "trackside.lock";

impl DataDirLock {
    /// Takes the lock, failing at once if another process holds it.
    pub fn acquire(data_dir: &Path) -> Result<Self, LockError> {
        let path = data_dir.join(LOCK_FILE);
        let io = |source| LockError::Io {
            path: path.clone(),
            source,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(io)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(LockError::Held(data_dir.to_path_buf()));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(io(e)),
        }
        file.set_len(0).map_err(io)?;
        writeln!(file, "{}", std::process::id()).map_err(io)?;
        Ok(Self { _file: file, path })
    }

    /// Whether some process holds the lock now, without keeping it.
    pub fn is_held(data_dir: &Path) -> bool {
        matches!(Self::acquire(data_dir), Err(LockError::Held(_)))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_on_the_same_dir_is_refused_until_the_first_drops()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let first = DataDirLock::acquire(dir.path())?;
        assert!(matches!(
            DataDirLock::acquire(dir.path()),
            Err(LockError::Held(_))
        ));
        assert!(DataDirLock::is_held(dir.path()));
        drop(first);
        assert!(!DataDirLock::is_held(dir.path()));
        DataDirLock::acquire(dir.path())?;
        Ok(())
    }
}
