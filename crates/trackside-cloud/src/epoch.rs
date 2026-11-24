//! The sync epoch: a random id every cursor carries, so a cursor from before a restore is
//! refused instead of silently skipping what the restore lost.
//!
//! The database keeps the epoch (`cloud.epoch`) and an instance token (`cloud.instance`) that
//! changes at every start and after every backup. A sidecar file next to the database
//! (`<db>.cloud-instance`) holds the token the server last wrote, and the highest count of
//! changes (`cloud_meta.issued`) it has seen committed. At start, a database whose token
//! differs from the sidecar's is a different copy (a restored backup, a file put in place by
//! hand), and one whose count is below the sidecar's was rolled back underneath the server
//! (a Litestream restore keeps the token): either gets a new epoch, and every client then
//! pulls again from an empty cursor. Restoring the whole data folder restores the sidecar
//! too, so that restore must delete the sidecar; the ops docs say so.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use rusqlite::Connection;
use trackside_core::repo::settings;
use trackside_core::{Db, DbError};

/// The settings keys. Not dashboard settings: `PATCH /admin/settings` cannot reach them.
pub const EPOCH_KEY: &str = "cloud.epoch";
pub const INSTANCE_KEY: &str = "cloud.instance";
/// The epoch of a database that was never reconciled (tests, a server not started yet).
pub const UNSET: &str = "0";

/// A pull cursor: the epoch it belongs to and the user's change seq. Wire form
/// `<epoch>.<seq>`, opaque to clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: i64,
}

impl Cursor {
    pub fn encode(&self) -> String {
        format!("{}.{}", self.epoch, self.seq)
    }

    /// `None` for anything that is not `<epoch>.<seq>`.
    pub fn parse(text: &str) -> Option<Self> {
        let (epoch, seq) = text.trim().rsplit_once('.')?;
        let epoch_ok = !epoch.is_empty()
            && epoch.len() <= 64
            && epoch.chars().all(|c| c.is_ascii_alphanumeric());
        let seq = seq.parse::<i64>().ok().filter(|s| *s >= 0)?;
        epoch_ok.then(|| Self {
            epoch: epoch.to_owned(),
            seq,
        })
    }
}

/// The current epoch.
pub fn current(c: &Connection) -> Result<String, DbError> {
    Ok(settings::get::<String>(c, EPOCH_KEY)?.unwrap_or_else(|| String::from(UNSET)))
}

pub async fn read(db: &Db) -> Result<String, DbError> {
    db.read(current).await
}

fn fresh() -> String {
    let mut id = uuid::Uuid::new_v4().simple().to_string();
    id.truncate(16);
    id
}

fn sidecar(db: &Db) -> Option<PathBuf> {
    db.path().map(|p| {
        let mut name = p.as_os_str().to_owned();
        name.push(".cloud-instance");
        PathBuf::from(name)
    })
}

/// What a sidecar holds: the instance token and the highest `issued` count seen.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Sidecar {
    token: String,
    issued: i64,
}

/// The sidecars this process keeps up to date, by path, and the lock every write takes, so
/// a token rotated after a backup and a count noted after a push never overwrite each other.
static SIDECARS: Mutex<Option<HashMap<PathBuf, Sidecar>>> = Mutex::new(None);

fn read_sidecar(path: &Path) -> Option<Sidecar> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines().map(str::trim);
    let token = lines.next().filter(|t| !t.is_empty())?.to_owned();
    let issued = lines.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    Some(Sidecar { token, issued })
}

fn write_sidecar(path: &Path, sidecar: &Sidecar) -> std::io::Result<()> {
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    std::fs::write(&partial, format!("{}\n{}\n", sidecar.token, sidecar.issued))?;
    std::fs::rename(&partial, path)
}

/// What [`reconcile`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconciled {
    pub epoch: String,
    /// The database is not the one this server last ran with: cursors start over.
    pub new_epoch: bool,
}

/// Run once at start, before serving. Gives the database a new epoch when it is not the copy
/// the server last ran with, or was rolled back, then a new instance token.
pub fn reconcile(db: &Db) -> Result<Reconciled, DbError> {
    let path = sidecar(db);
    let mut sidecars = SIDECARS.lock().unwrap_or_else(PoisonError::into_inner);
    let on_disk = path.as_deref().and_then(read_sidecar);
    let token = fresh();
    let written = token.clone();
    let (reconciled, issued) = db.write_blocking(move |tx| {
        let epoch: Option<String> = settings::get(tx, EPOCH_KEY)?;
        let instance: Option<String> = settings::get(tx, INSTANCE_KEY)?;
        let issued = crate::repo::issued(tx)?;
        let replaced = match (&on_disk, &instance) {
            (None, None) => false,
            (Some(file), Some(stored)) => file.token != *stored || file.issued > issued,
            _ => true,
        };
        let (epoch, new_epoch) = match epoch {
            Some(epoch) if !replaced => (epoch, false),
            _ => (fresh(), true),
        };
        settings::put(tx, EPOCH_KEY, &epoch)?;
        settings::put(tx, INSTANCE_KEY, &written)?;
        Ok((Reconciled { epoch, new_epoch }, issued))
    })?;
    if let Some(path) = path {
        let sidecar = Sidecar { token, issued };
        write_sidecar(&path, &sidecar)?;
        sidecars.get_or_insert_default().insert(path, sidecar);
    }
    Ok(reconciled)
}

/// Run after every backup, so no backup shares the live database's instance token and a
/// restore of it is recognised at the next start.
pub fn rotate(db: &Db) -> Result<(), DbError> {
    let Some(path) = sidecar(db) else {
        return Ok(());
    };
    let mut sidecars = SIDECARS.lock().unwrap_or_else(PoisonError::into_inner);
    let token = fresh();
    let written = token.clone();
    let issued = db.write_blocking(move |tx| {
        settings::put(tx, INSTANCE_KEY, &written)?;
        crate::repo::issued(tx)
    })?;
    let known = sidecars.get_or_insert_default();
    let issued = known.get(&path).map_or(issued, |s| s.issued.max(issued));
    let sidecar = Sidecar { token, issued };
    write_sidecar(&path, &sidecar)?;
    known.insert(path, sidecar);
    Ok(())
}

/// Run after a write that issued seqs has committed: raises the sidecar's count to what the
/// database now holds. Only for a database [`reconcile`] has seen; a failure is logged and
/// costs only the detection of a later rollback.
pub async fn note_issued(db: &Db) {
    let Some(path) = sidecar(db) else { return };
    let tracked = SIDECARS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|known| known.contains_key(&path));
    if !tracked {
        return;
    }
    let issued = match db.read(crate::repo::issued).await {
        Ok(issued) => issued,
        Err(e) => {
            tracing::debug!(error = %e, "cannot read the cloud change count");
            return;
        }
    };
    let mut sidecars = SIDECARS.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(sidecar) = sidecars.as_mut().and_then(|known| known.get_mut(&path)) else {
        return;
    };
    if issued <= sidecar.issued {
        return;
    }
    sidecar.issued = issued;
    if let Err(e) = write_sidecar(&path, sidecar) {
        tracing::warn!(error = %e, "cannot write the cloud instance file");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_refuse_junk() {
        let cursor = Cursor {
            epoch: String::from("3f9a0c1d7e2b4a65"),
            seq: 918,
        };
        assert_eq!(Cursor::parse(&cursor.encode()), Some(cursor));
        for junk in ["918", "", ".5", "a.", "a.-1", "a b.1", "a.1x"] {
            assert_eq!(Cursor::parse(junk), None, "{junk}");
        }
    }
}
