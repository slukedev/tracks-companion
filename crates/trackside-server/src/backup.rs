//! Backups (architecture section 13): `VACUUM INTO` a `.partial` file then rename, the secret
//! key copied beside it, scheduled ones kept to `backup.keep`, a free-space check first.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use jiff::Timestamp;
use jiff::civil::Weekday;
use jiff::tz::TimeZone;
use trackside_api::wire::admin::Backup;
use trackside_api::{OpsError, ServerOps};
use trackside_core::config::BackupSchedule;
use trackside_core::{BoxFuture, Db, Millis};

/// Why a backup was taken; part of its file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupKind {
    Scheduled,
    Manual,
    PreMigrate { from: u32, to: u32 },
}

/// `trackside-<UTC timestamp>[-manual].db`, or `pre-migrate-v<from>-v<to>-<UTC timestamp>.db`.
pub fn backup_name(kind: BackupKind, at: Millis) -> String {
    let stamp = Timestamp::from_millisecond(at.0)
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string();
    match kind {
        BackupKind::Scheduled => format!("trackside-{stamp}.db"),
        BackupKind::Manual => format!("trackside-{stamp}-manual.db"),
        BackupKind::PreMigrate { from, to } => format!("pre-migrate-v{from}-v{to}-{stamp}.db"),
    }
}

/// The kind and time a backup file name says, or `None` for files that are not backups.
pub fn parse_name(name: &str) -> Option<(&'static str, Millis)> {
    let (kind, stamp) = if let Some(rest) = name.strip_prefix("pre-migrate-v") {
        let rest = rest.strip_suffix(".db")?;
        ("pre_migrate", rest.get(rest.len().checked_sub(16)?..)?)
    } else {
        let rest = name.strip_prefix("trackside-")?.strip_suffix(".db")?;
        let stamp = rest.get(..16)?;
        let kind = match &rest[16..] {
            "" => "scheduled",
            "-manual" => "manual",
            // The name pre-migration backups had before they matched the documentation.
            tail if tail.starts_with("-pre-migrate") => "pre_migrate",
            _ => return None,
        };
        (kind, stamp)
    };
    let at = jiff::civil::DateTime::strptime("%Y%m%dT%H%M%SZ", stamp)
        .ok()?
        .to_zoned_datetime(TimeZone::UTC)
        .ok()?
        .timestamp();
    Some((kind, Millis(at.as_millisecond())))
}

/// Backups of one data directory.
#[derive(Debug)]
pub struct Backups {
    pub dir: PathBuf,
    pub secret_key: Option<PathBuf>,
    pub keep: u32,
    running: AtomicBool,
}

impl Backups {
    pub fn new(dir: PathBuf, secret_key: Option<PathBuf>, keep: u32) -> Self {
        Self {
            dir,
            secret_key,
            keep,
            running: AtomicBool::new(false),
        }
    }

    /// Deletes `.partial` files a crash left behind.
    pub fn clean_partials(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().ends_with(".partial") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Every backup, newest first.
    pub fn list(&self) -> std::io::Result<Vec<Backup>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((kind, at)) = parse_name(&name) else {
                continue;
            };
            let meta = entry.metadata().ok();
            // Backups written before they were made private, or copied in by hand.
            if meta.as_ref().is_some_and(is_shared) {
                restrict(&entry.path());
                let key = self.dir.join(format!("{name}.key"));
                if key.exists() {
                    restrict(&key);
                }
            }
            let bytes = meta.map_or(0, |m| m.len());
            out.push(Backup {
                has_key: self.dir.join(format!("{name}.key")).exists(),
                name,
                kind: kind.to_owned(),
                at: at.into(),
                bytes,
            });
        }
        out.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.name.cmp(&a.name)));
        Ok(out)
    }

    /// The file of a backup by name.
    pub fn path(&self, name: &str) -> Option<PathBuf> {
        parse_name(name)?;
        let path = self.dir.join(name);
        path.is_file().then_some(path)
    }

    /// Writes one backup now, on the calling thread. Refuses when another is running or the
    /// volume lacks the database size plus 10 %.
    pub fn create_blocking(
        &self,
        db: &Db,
        kind: BackupKind,
        now: Millis,
    ) -> Result<PathBuf, OpsError> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(OpsError::Busy);
        }
        let result = self.create_inner(db, kind, now);
        self.running.store(false, Ordering::SeqCst);
        result
    }

    fn create_inner(&self, db: &Db, kind: BackupKind, now: Millis) -> Result<PathBuf, OpsError> {
        create_private_dir(&self.dir).map_err(OpsError::Io)?;
        if let Some(size) = db
            .path()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
        {
            let free = fs4::available_space(&self.dir).map_err(OpsError::Io)?;
            if free < size + size / 10 {
                return Err(OpsError::NotEnoughSpace);
            }
        }
        let target = self.dir.join(backup_name(kind, now));
        db.backup_into(&target)
            .map_err(|e| OpsError::Io(std::io::Error::other(e.to_string())))?;
        // A restore of this copy must look like a different database to cloud sync.
        if let Err(e) = trackside_cloud::epoch::rotate(db) {
            tracing::warn!(error = %e, "cannot rotate the cloud instance token after a backup");
        }
        // The copy holds password and key hashes and sealed secrets, like the database.
        restrict(&target);
        if let Some(key) = &self.secret_key
            && key.exists()
        {
            let copy = PathBuf::from(format!("{}.key", target.display()));
            std::fs::copy(key, &copy).map_err(OpsError::Io)?;
            restrict(&copy);
        }
        if kind == BackupKind::Scheduled {
            self.prune();
        }
        Ok(target)
    }

    /// Keeps the newest `keep` scheduled backups; others are kept until deleted.
    pub fn prune(&self) {
        let Ok(list) = self.list() else { return };
        let keep = usize::try_from(self.keep).unwrap_or(usize::MAX);
        for old in list.iter().filter(|b| b.kind == "scheduled").skip(keep) {
            let _ = self.delete(&old.name);
        }
    }

    pub fn delete(&self, name: &str) -> Result<(), OpsError> {
        let path = self.path(name).ok_or(OpsError::NotFound)?;
        std::fs::remove_file(&path).map_err(OpsError::Io)?;
        let _ = std::fs::remove_file(format!("{}.key", path.display()));
        Ok(())
    }
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &Path) {}

#[cfg(unix)]
fn is_shared(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o077 != 0
}

#[cfg(not(unix))]
fn is_shared(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Creates the backups folder readable by the server's user only, so a backup is never
/// readable by others, even for the moment before [`restrict`] runs on it.
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// The next time `schedule` fires after `now`, in the server's local zone. Skipped and repeated
/// DST times resolve to the later instant.
pub fn next_run(schedule: BackupSchedule, now: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
    let (weekday, hour, minute): (Option<Weekday>, u8, u8) = match schedule {
        BackupSchedule::Off => return None,
        BackupSchedule::Daily { hour, minute } => (None, hour, minute),
        BackupSchedule::Weekly {
            weekday,
            hour,
            minute,
        } => (Some(weekday), hour, minute),
    };
    let today = now.to_zoned_datetime(zone.clone()).date();
    for offset in 0..9 {
        let date = today.checked_add(jiff::Span::new().days(offset)).ok()?;
        if weekday.is_some_and(|w| date.weekday() != w) {
            continue;
        }
        let time =
            jiff::civil::Time::new(i8::try_from(hour).ok()?, i8::try_from(minute).ok()?, 0, 0)
                .ok()?;
        let at = zone
            .to_ambiguous_zoned_datetime(date.to_datetime(time))
            .later()
            .ok()?
            .timestamp();
        if at > now {
            return Some(at);
        }
    }
    None
}

/// The server's [`ServerOps`]: backups and URL checks.
#[derive(Debug)]
pub struct Ops {
    pub backups: Arc<Backups>,
    /// What the backup scheduler last did, shared with the overview.
    pub status: Arc<std::sync::Mutex<crate::health::BackupStatus>>,
    pub db: Db,
    pub http: reqwest::Client,
    pub server_id: uuid::Uuid,
}

impl ServerOps for Ops {
    fn next_backup(&self) -> Option<Millis> {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next_at
    }

    fn backups(&self) -> BoxFuture<'_, Result<Vec<Backup>, OpsError>> {
        let backups = Arc::clone(&self.backups);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || backups.list())
                .await
                .map_err(|e| OpsError::Io(std::io::Error::other(e)))?
                .map_err(OpsError::Io)
        })
    }

    fn start_backup(&self) -> BoxFuture<'_, Result<(), OpsError>> {
        let backups = Arc::clone(&self.backups);
        let db = self.db.clone();
        Box::pin(async move {
            if backups.running.load(Ordering::SeqCst) {
                return Err(OpsError::Busy);
            }
            let now = db.clock().now();
            tokio::task::spawn_blocking(move || {
                match backups.create_blocking(&db, BackupKind::Manual, now) {
                    Ok(path) => tracing::info!(path = %path.display(), "backup written"),
                    Err(e) => tracing::error!(error = %e, "backup failed"),
                }
            });
            Ok(())
        })
    }

    fn backup_path(&self, name: &str) -> Option<PathBuf> {
        self.backups.path(name)
    }

    fn delete_backup<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), OpsError>> {
        let backups = Arc::clone(&self.backups);
        let name = name.to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || backups.delete(&name))
                .await
                .map_err(|e| OpsError::Io(std::io::Error::other(e)))?
        })
    }

    fn check_url<'a>(&'a self, url: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let ping = format!("{}/api/v1/ping", url.trim_end_matches('/'));
            let Ok(response) = self.http.get(&ping).send().await else {
                return false;
            };
            let Ok(body) = response.json::<trackside_api::wire::Ping>().await else {
                return false;
            };
            body.server_id == self.server_id.to_string()
        })
    }
}

/// Restores a backup into place (CLI only, server stopped): runs the integrity check, checks
/// this binary knows its schema, moves the current one aside, copies the backup in.
pub fn restore(
    file: &Path,
    db_path: &Path,
    data_dir: &Path,
    now: Millis,
) -> anyhow::Result<PathBuf> {
    if crate::lock::DataDirLock::is_held(data_dir) {
        anyhow::bail!("the server is running; stop it before restoring");
    }
    let problems = Db::check_file(file, true)
        .with_context(|| format!("{} is not a readable database", file.display()))?;
    if !problems.is_empty() {
        anyhow::bail!(
            "{} failed the integrity check: {}",
            file.display(),
            problems.join("; ")
        );
    }
    let version = Db::inspect_schema_version(file)
        .with_context(|| format!("{} is not a readable database", file.display()))?
        .with_context(|| format!("{} does not exist", file.display()))?;
    if version > trackside_core::db::SCHEMA_VERSION {
        anyhow::bail!(
            "{} has schema {version}, newer than this server knows ({})",
            file.display(),
            trackside_core::db::SCHEMA_VERSION
        );
    }
    let stamp = Timestamp::from_millisecond(now.0)
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string();
    let aside = PathBuf::from(format!("{}.before-restore-{stamp}", db_path.display()));
    if db_path.exists() {
        std::fs::rename(db_path, &aside).context("cannot move the current database aside")?;
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", db_path.display()));
            if side.exists() {
                std::fs::rename(&side, format!("{}{suffix}", aside.display()))?;
            }
        }
    }
    std::fs::copy(file, db_path).context("cannot copy the backup into place")?;
    Ok(aside)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_their_kind_and_time() {
        let at = Millis(1_794_648_600_000);
        for (kind, text) in [
            (BackupKind::Scheduled, "scheduled"),
            (BackupKind::Manual, "manual"),
            (BackupKind::PreMigrate { from: 1, to: 2 }, "pre_migrate"),
        ] {
            let name = backup_name(kind, at);
            assert_eq!(parse_name(&name), Some((text, at)), "{name}");
        }
        assert_eq!(
            backup_name(BackupKind::Scheduled, at),
            "trackside-20261114T093000Z.db"
        );
        assert_eq!(parse_name("trackside.db"), None);
        assert_eq!(parse_name("trackside-20261114T093000Z.db.key"), None);
    }

    #[test]
    fn daily_schedule_fires_next_at_its_time() -> Result<(), Box<dyn std::error::Error>> {
        let now: Timestamp = "2026-11-14T09:30:00Z".parse()?;
        let next = next_run(
            BackupSchedule::Daily {
                hour: 3,
                minute: 30,
            },
            now,
            &TimeZone::UTC,
        );
        assert_eq!(next, Some("2026-11-15T03:30:00Z".parse()?));
        let weekly = next_run(
            BackupSchedule::Weekly {
                weekday: Weekday::Monday,
                hour: 3,
                minute: 30,
            },
            now,
            &TimeZone::UTC,
        );
        assert_eq!(weekly, Some("2026-11-16T03:30:00Z".parse()?));
        assert_eq!(next_run(BackupSchedule::Off, now, &TimeZone::UTC), None);
        Ok(())
    }

    #[test]
    fn skipped_dst_times_resolve_to_the_later_instant() -> Result<(), Box<dyn std::error::Error>> {
        let zone = TimeZone::get("Europe/London")?;
        let now: Timestamp = "2026-03-28T12:00:00Z".parse()?;
        let next = next_run(
            BackupSchedule::Daily {
                hour: 1,
                minute: 30,
            },
            now,
            &zone,
        );
        assert_eq!(next, Some("2026-03-29T01:30:00Z".parse()?));
        Ok(())
    }

    #[test]
    fn scheduled_backups_are_pruned_to_keep_and_others_stay()
    -> Result<(), Box<dyn std::error::Error>> {
        let test = trackside_core::testing::TestDb::new()?;
        let dir = tempfile::tempdir()?;
        let file_db = Db::open(
            &trackside_core::DbOptions::new(dir.path().join("t.db")),
            test.events.clone(),
            test.clock.clone(),
        );
        let db = match file_db {
            Ok(db) => db,
            Err(trackside_core::DbError::NetworkFilesystem) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let backups = Backups::new(dir.path().join("backups"), None, 2);
        for hour in 0..4 {
            backups.create_blocking(
                &db,
                BackupKind::Scheduled,
                Millis(1_794_648_600_000 + hour * 3_600_000),
            )?;
        }
        backups.create_blocking(&db, BackupKind::Manual, Millis(1_794_648_600_000))?;
        let list = backups.list()?;
        assert_eq!(list.iter().filter(|b| b.kind == "scheduled").count(), 2);
        assert_eq!(list.iter().filter(|b| b.kind == "manual").count(), 1);
        assert_eq!(list[0].name, "trackside-20261114T123000Z.db");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = backups.path(&list[0].name).ok_or("no backup file")?;
            assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o077, 0);
            let folder = std::fs::metadata(dir.path().join("backups"))?;
            assert_eq!(folder.permissions().mode() & 0o777, 0o700);
            // One left readable by an older version is made private when listed.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
            backups.list()?;
            assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o077, 0);
        }
        Ok(())
    }
}
