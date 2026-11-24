//! The database: one writer connection and a pool of readers over WAL, as Engine's
//! `tracks-store` does it. The API is synchronous underneath; async callers use [`Db::read`]
//! and [`Db::write`], which run the closure on tokio's blocking pool.
//!
//! Every write runs in an immediate transaction wrapped in a [`WriteTx`]. Changes recorded on
//! it go to `change_log` in the same transaction and are published on the [`EventBus`] as
//! [`ServerEvent::Changed`] only after the commit succeeds.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use rusqlite_migration::{M, Migrations};
use tokio::sync::Semaphore;

use crate::events::{Audience, EventBus, ServerEvent};
use crate::ids::{LibraryKey, Seq, UserKey};
use crate::time::{Clock, Millis};

/// The schema version this build creates and understands (`PRAGMA user_version`).
pub const SCHEMA_VERSION: u32 = 3;

const MIGRATION_1: &str = include_str!("../migrations/0001_initial.sql");
const MIGRATION_2: &str = include_str!("../migrations/0002_stats_and_reauth.sql");
const MIGRATION_3: &str = include_str!("../migrations/0003_enrichment.sql");

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(MIGRATION_1),
        M::up(MIGRATION_2),
        M::up(MIGRATION_3),
    ])
}

/// How [`Db::open`] decides whether the database sits on a network filesystem.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FilesystemCheck {
    /// Look at the mount table (Linux). Other systems are treated as local.
    #[default]
    Detect,
    /// Skip the check.
    AssumeLocal,
    /// Behave as if the directory were on NFS or SMB. For tests.
    AssumeNetwork,
}

/// Where and how to open the database.
#[derive(Clone, Debug)]
pub struct DbOptions {
    pub path: PathBuf,
    /// Idle reader connections kept open, and the number of reads that run at once.
    pub readers: usize,
    /// Page cache per connection.
    pub cache_bytes: u64,
    pub mmap_bytes: u64,
    /// Permit a database on NFS or SMB, where SQLite's locking is unreliable.
    pub allow_network_fs: bool,
    pub filesystem: FilesystemCheck,
}

impl DbOptions {
    /// The defaults of architecture section 11 for a database at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Self {
            path: path.into(),
            readers: cpus.min(8),
            cache_bytes: 32 * 1024 * 1024,
            mmap_bytes: 256 * 1024 * 1024,
            allow_network_fs: false,
            filesystem: FilesystemCheck::Detect,
        }
    }
}

/// A database failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("database is newer than this server (schema {found}, known {known})")]
    TooNew { found: u32, known: u32 },
    #[error("database is on a network filesystem")]
    NetworkFilesystem,
    #[error("disk full")]
    Full,
    #[error("database closed")]
    Closed,
    #[error("not found")]
    NotFound,
    #[error("forbidden")]
    Forbidden,
    #[error("precondition failed")]
    PreconditionFailed,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("migration failed: {0}")]
    Migration(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

impl DbError {
    /// Recognises SQLite's "disk full" so callers can answer `507`.
    fn classify(self) -> Self {
        match self {
            Self::Sqlite(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::DiskFull => {
                Self::Full
            }
            other => other,
        }
    }
}

/// What kind of entity a change is about, as stored in `change_log.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Song,
    Album,
    Artist,
    Library,
    Playlist,
    Star,
    Rating,
    PlayStat,
    PlayQueue,
    Acquisition,
}

impl ChangeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Song => "song",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Library => "library",
            Self::Playlist => "playlist",
            Self::Star => "star",
            Self::Rating => "rating",
            Self::PlayStat => "play_stat",
            Self::PlayQueue => "play_queue",
            Self::Acquisition => "acquisition",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "song" => Self::Song,
            "album" => Self::Album,
            "artist" => Self::Artist,
            "library" => Self::Library,
            "playlist" => Self::Playlist,
            "star" => Self::Star,
            "rating" => Self::Rating,
            "play_stat" => Self::PlayStat,
            "play_queue" => Self::PlayQueue,
            "acquisition" => Self::Acquisition,
            _ => return None,
        })
    }
}

/// Whether the entity now exists or is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeOp {
    Upsert,
    Delete,
}

impl ChangeOp {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upsert => "upsert",
            Self::Delete => "delete",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "upsert" => Some(Self::Upsert),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }
}

/// One change for delta sync. `entity` is the public id, or `<item_kind>:<item_pid>` for
/// stars and ratings. `user` is `None` for library-wide entities (stored as 0), `library` is
/// set for library-scoped kinds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    pub entity: String,
    pub user: Option<UserKey>,
    pub library: Option<LibraryKey>,
    pub op: ChangeOp,
}

impl Change {
    /// A change to a library-wide entity (song, album, artist, library, public playlist).
    pub fn shared(
        kind: ChangeKind,
        entity: impl Into<String>,
        library: Option<LibraryKey>,
        op: ChangeOp,
    ) -> Self {
        Self {
            kind,
            entity: entity.into(),
            user: None,
            library,
            op,
        }
    }

    /// A change only `user` sees (stars, ratings, play stats, play queue, private playlists,
    /// acquisitions).
    pub fn for_user(
        kind: ChangeKind,
        entity: impl Into<String>,
        user: UserKey,
        op: ChangeOp,
    ) -> Self {
        Self {
            kind,
            entity: entity.into(),
            user: Some(user),
            library: None,
            op,
        }
    }

    fn audience(&self) -> Audience {
        match self.user {
            Some(user) => Audience::User(user),
            None => Audience::Everyone {
                library: self.library,
            },
        }
    }
}

/// A write transaction. Changes recorded here go to `change_log` in the same transaction and
/// are published on the [`EventBus`] after commit. Dropping it without returning `Ok` from the
/// [`Db::write`] closure rolls everything back.
pub struct WriteTx<'c> {
    tx: Transaction<'c>,
    changes: Vec<Change>,
    now: Millis,
    next_seq: i64,
}

impl WriteTx<'_> {
    /// The connection, inside the transaction.
    pub fn conn(&self) -> &Connection {
        &self.tx
    }

    /// The time this transaction started, used for every `*_at` it writes.
    pub fn now(&self) -> Millis {
        self.now
    }

    /// Records a change in `change_log`, replacing the entity's previous row with a new, higher
    /// seq.
    ///
    /// An entity that was recorded under one library and is now recorded under another (a song
    /// moved between libraries) keeps `library_id` NULL from then on, so every user's delta
    /// sees its later changes: the ones who can no longer see it get a delete instead of
    /// silence.
    pub fn record(&mut self, change: Change) -> Result<Seq, DbError> {
        let seq = Seq(self.next_seq);
        self.tx
            .prepare_cached(
                "INSERT INTO change_log (seq, kind, entity_pid, user_id, library_id, op, changed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (kind, entity_pid, user_id) DO UPDATE SET
                     seq = excluded.seq,
                     library_id = CASE WHEN change_log.library_id IS excluded.library_id
                                       THEN excluded.library_id ELSE NULL END,
                     op = excluded.op,
                     changed_at = excluded.changed_at",
            )?
            .execute(rusqlite::params![
                seq,
                change.kind.as_str(),
                change.entity,
                change.user.unwrap_or(UserKey(0)),
                change.library,
                change.op.as_str(),
                self.now,
            ])?;
        self.next_seq += 1;
        self.changes.push(change);
        Ok(seq)
    }

    /// A savepoint inside this transaction, for work that may be undone on its own.
    pub fn savepoint(&mut self) -> Result<rusqlite::Savepoint<'_>, DbError> {
        Ok(self.tx.savepoint()?)
    }
}

impl std::ops::Deref for WriteTx<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.tx
    }
}

impl std::fmt::Debug for WriteTx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteTx")
            .field("now", &self.now)
            .field("changes", &self.changes.len())
            .finish_non_exhaustive()
    }
}

struct Writer {
    conn: Connection,
    /// The seq the next recorded change gets: `max(seq) + 1` at open, kept in memory because
    /// only this connection writes.
    next_seq: i64,
}

struct Inner {
    writer: Mutex<Writer>,
    idle_readers: Mutex<Vec<Connection>>,
    /// `None` in memory, where readers would each see their own empty database, so reads go
    /// to the writer.
    path: Option<PathBuf>,
    pragmas: Pragmas,
    max_idle_readers: usize,
    read_slots: Arc<Semaphore>,
    /// One async write at a time takes a blocking thread; the rest wait here without one, so
    /// a long write (or a slow disk) cannot fill the blocking pool with queued writers.
    write_slot: Arc<Semaphore>,
    events: EventBus,
    clock: Arc<dyn Clock>,
}

#[derive(Clone, Copy)]
struct Pragmas {
    cache_kib: u64,
    mmap_bytes: u64,
}

/// The database handle. Cheap to clone; every clone shares the same connections.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves SQLite itself consistent (the transaction rolls
    // back on drop), so a poisoned lock is safe to keep using.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Db {
    /// Opens the database file, creating it and its directory if needed, refusing a network
    /// filesystem unless allowed, and migrating it to [`SCHEMA_VERSION`].
    pub fn open(
        options: &DbOptions,
        events: EventBus,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DbError> {
        let dir = match options.path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&dir)?;
        if !options.allow_network_fs {
            let network = match options.filesystem {
                FilesystemCheck::Detect => {
                    is_network_filesystem(&dir)? || hooks::assumed_network(&dir)
                }
                FilesystemCheck::AssumeLocal => false,
                FilesystemCheck::AssumeNetwork => true,
            };
            if network {
                return Err(DbError::NetworkFilesystem);
            }
        }
        let pragmas = Pragmas {
            cache_kib: options.cache_bytes / 1024,
            mmap_bytes: options.mmap_bytes,
        };
        let conn = Connection::open(&options.path)?;
        Self::finish_open(
            conn,
            Some(options.path.clone()),
            pragmas,
            options.readers,
            events,
            clock,
        )
    }

    /// A private in-memory database with migration 1 applied. For tests.
    pub fn open_in_memory(events: EventBus, clock: Arc<dyn Clock>) -> Result<Self, DbError> {
        let pragmas = Pragmas {
            cache_kib: 8 * 1024,
            mmap_bytes: 0,
        };
        let conn = Connection::open_in_memory()?;
        Self::finish_open(conn, None, pragmas, 1, events, clock)
    }

    fn finish_open(
        mut conn: Connection,
        path: Option<PathBuf>,
        pragmas: Pragmas,
        readers: usize,
        events: EventBus,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DbError> {
        configure(&conn, pragmas, true)?;
        let found = user_version(&conn)?;
        if found > SCHEMA_VERSION {
            return Err(DbError::TooNew {
                found,
                known: SCHEMA_VERSION,
            });
        }
        hooks::before_migration(&mut conn, path.as_deref(), found)?;
        migrations().to_latest(&mut conn).map_err(|e| match e {
            rusqlite_migration::Error::RusqliteError { err, .. } => DbError::Sqlite(err).classify(),
            other => DbError::Migration(other.to_string()),
        })?;
        write_first_start_settings(&conn, clock.now())?;
        check_folding_version(&conn)?;
        // Pruned tombstones took their seqs with them; never hand those out again.
        let next_seq: i64 = conn.query_row(
            "SELECT max(coalesce((SELECT max(seq) FROM change_log), 0),
                        coalesce((SELECT CAST(value AS INTEGER) FROM settings
                                  WHERE key = 'sync.pruned_through'), 0)) + 1",
            [],
            |row| row.get(0),
        )?;
        let readers = readers.max(1);
        Ok(Self {
            inner: Arc::new(Inner {
                writer: Mutex::new(Writer { conn, next_seq }),
                idle_readers: Mutex::new(Vec::new()),
                path,
                pragmas,
                max_idle_readers: readers,
                read_slots: Arc::new(Semaphore::new(readers)),
                write_slot: Arc::new(Semaphore::new(1)),
                events,
                clock,
            }),
        })
    }

    /// The seq of the last committed change (0 before any), for `ETag`s and the overview. Read
    /// from the writer's counter, so it costs no query.
    pub fn current_seq(&self) -> Seq {
        Seq(lock(&self.inner.writer).next_seq - 1)
    }

    /// Runs SQLite's `quick_check` (or the slower, thorough `integrity_check` with `full`) on
    /// the open database. An empty list means no problems.
    pub fn integrity_check(&self, full: bool) -> Result<Vec<String>, DbError> {
        self.with_reader(|conn| integrity(conn, full))
    }

    /// The same check on a database file that is not open, such as a backup about to be
    /// restored. The file is opened read-only and never migrated.
    pub fn check_file(path: &Path, full: bool) -> Result<Vec<String>, DbError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        integrity(&conn, full)
    }

    /// The schema version of the open database.
    pub fn schema_version(&self) -> Result<u32, DbError> {
        self.read_blocking(user_version)
    }

    /// The schema version of the database file at `path` without migrating it, or `None` when
    /// there is no file yet. Lets the server take a pre-migration backup.
    pub fn inspect_schema_version(path: &Path) -> Result<Option<u32>, DbError> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Some(user_version(&conn)?))
    }

    /// The file path, or `None` in memory.
    pub fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    /// The bus changes are published on.
    pub fn events(&self) -> &EventBus {
        &self.inner.events
    }

    /// The clock write transactions take their time from.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.inner.clock
    }

    /// Runs a read on the blocking pool, in one snapshot.
    pub async fn read<T, F>(&self, f: F) -> Result<T, DbError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, DbError> + Send + 'static,
    {
        let permit = Arc::clone(&self.inner.read_slots)
            .acquire_owned()
            .await
            .map_err(|_| DbError::Closed)?;
        let db = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let result = db.read_blocking(f);
            drop(permit);
            result
        })
        .await;
        join(result)
    }

    /// Runs a write transaction on the blocking pool.
    pub async fn write<T, F>(&self, f: F) -> Result<T, DbError>
    where
        T: Send + 'static,
        F: FnOnce(&mut WriteTx<'_>) -> Result<T, DbError> + Send + 'static,
    {
        let permit = Arc::clone(&self.inner.write_slot)
            .acquire_owned()
            .await
            .map_err(|_| DbError::Closed)?;
        let db = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let result = db.write_blocking(f);
            drop(permit);
            result
        })
        .await;
        join(result)
    }

    /// Runs a read on the calling thread, in one snapshot. Only for threads that may block.
    pub fn read_blocking<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        self.with_reader(|conn| in_snapshot(conn, f))
    }

    /// Lends a reader connection outside any transaction (the writer, in memory).
    fn with_reader<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let Some(path) = &self.inner.path else {
            let writer = lock(&self.inner.writer);
            return f(&writer.conn);
        };
        let idle = lock(&self.inner.idle_readers).pop();
        let conn = match idle {
            Some(conn) => conn,
            None => {
                let conn = Connection::open_with_flags(
                    path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY
                        | OpenFlags::SQLITE_OPEN_NO_MUTEX
                        | OpenFlags::SQLITE_OPEN_URI,
                )?;
                configure(&conn, self.inner.pragmas, false)?;
                conn
            }
        };
        let result = f(&conn);
        let mut idle = lock(&self.inner.idle_readers);
        if idle.len() < self.inner.max_idle_readers {
            idle.push(conn);
        }
        result
    }

    /// Runs a write transaction on the calling thread. Only for threads that may block.
    pub fn write_blocking<T>(
        &self,
        f: impl FnOnce(&mut WriteTx<'_>) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let (value, changes, last_seq) = {
            let mut writer = lock(&self.inner.writer);
            let Writer { conn, next_seq } = &mut *writer;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| DbError::from(e).classify())?;
            let mut write = WriteTx {
                tx,
                changes: Vec::new(),
                now: self.inner.clock.now(),
                next_seq: *next_seq,
            };
            let value = f(&mut write).map_err(DbError::classify)?;
            let WriteTx {
                tx,
                changes,
                next_seq: after,
                ..
            } = write;
            tx.commit().map_err(|e| DbError::from(e).classify())?;
            *next_seq = after;
            (value, changes, Seq(after - 1))
        };
        if !changes.is_empty() {
            let mut audiences: Vec<Audience> = Vec::new();
            for change in &changes {
                let audience = change.audience();
                if !audiences.contains(&audience) {
                    audiences.push(audience);
                }
            }
            self.inner.events.publish(ServerEvent::Changed {
                seq: last_seq,
                audiences,
            });
        }
        Ok(value)
    }

    /// Writes a consistent copy of the database to `target` with `VACUUM INTO`, first to
    /// `<target>.partial` and then renamed, so a crash never leaves a half-written backup under
    /// the real name.
    pub fn backup_into(&self, target: &Path) -> Result<(), DbError> {
        let mut partial = target.as_os_str().to_owned();
        partial.push(".partial");
        let partial = PathBuf::from(partial);
        match std::fs::remove_file(&partial) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let Some(partial_text) = partial.to_str() else {
            return Err(DbError::Invalid(String::from(
                "backup path is not valid UTF-8",
            )));
        };
        let partial_text = partial_text.to_owned();
        self.with_reader(move |conn| {
            conn.execute("VACUUM INTO ?1", [partial_text])?;
            Ok(())
        })?;
        // VACUUM INTO does not sync; without this a power cut after the rename can leave a
        // file under the final name whose pages never reached the disk.
        std::fs::File::open(&partial)?.sync_all()?;
        std::fs::rename(&partial, target)?;
        sync_parent(target)?;
        Ok(())
    }

    /// Updates the query planner's statistics and truncates the WAL. Run at shutdown and on a
    /// schedule.
    pub fn checkpoint_and_optimize(&self) -> Result<(), DbError> {
        let writer = lock(&self.inner.writer);
        writer.conn.execute_batch("PRAGMA optimize;")?;
        writer
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .optional()?;
        Ok(())
    }
}

/// Makes a rename in `path`'s folder durable. Only Unix can open a folder to sync it.
fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn join<T>(result: Result<Result<T, DbError>, tokio::task::JoinError>) -> Result<T, DbError> {
    match result {
        Ok(inner) => inner,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(DbError::Closed),
    }
}

/// Problems `quick_check` or `integrity_check` reports; their single `ok` row means none.
fn integrity(conn: &Connection, full: bool) -> Result<Vec<String>, DbError> {
    let pragma = if full {
        "PRAGMA integrity_check"
    } else {
        "PRAGMA quick_check"
    };
    let mut statement = conn.prepare(pragma)?;
    let rows: Vec<String> = statement
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(rows.into_iter().filter(|row| row != "ok").collect())
}

/// Failure injection for other crates' tests, under the `testing` feature only. Without it
/// these do nothing.
pub(crate) mod hooks {
    use std::path::Path;

    use rusqlite::Connection;

    use super::DbError;

    #[cfg(feature = "testing")]
    pub(crate) static NETWORK_DIRS: std::sync::Mutex<Vec<std::path::PathBuf>> =
        std::sync::Mutex::new(Vec::new());
    #[cfg(feature = "testing")]
    pub(crate) static CRASH_MIGRATION: std::sync::Mutex<Vec<std::path::PathBuf>> =
        std::sync::Mutex::new(Vec::new());

    #[cfg(feature = "testing")]
    fn listed(list: &std::sync::Mutex<Vec<std::path::PathBuf>>, path: &Path) -> bool {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        list.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|p| canonical.starts_with(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())))
    }

    /// Whether a test asked for `dir` to look like a network filesystem.
    pub(crate) fn assumed_network(dir: &Path) -> bool {
        #[cfg(feature = "testing")]
        {
            listed(&NETWORK_DIRS, dir)
        }
        #[cfg(not(feature = "testing"))]
        {
            let _ = dir;
            false
        }
    }

    /// For a database a test marked, runs the first part of the migration in a transaction
    /// and then fails, as a crash between statements would: the transaction rolls back and
    /// the file keeps its previous schema.
    #[cfg_attr(
        not(feature = "testing"),
        expect(clippy::unnecessary_wraps, reason = "fails only when tests ask it to")
    )]
    pub(crate) fn before_migration(
        conn: &mut Connection,
        path: Option<&Path>,
        found: u32,
    ) -> Result<(), DbError> {
        #[cfg(feature = "testing")]
        if let Some(path) = path
            && found < super::SCHEMA_VERSION
            && listed(&CRASH_MIGRATION, path)
        {
            let tx = conn.transaction()?;
            let half = super::MIGRATION_1
                .find("CREATE TABLE songs")
                .unwrap_or(super::MIGRATION_1.len());
            tx.execute_batch(&super::MIGRATION_1[..half])?;
            drop(tx);
            return Err(DbError::Migration(String::from(
                "simulated crash during migration",
            )));
        }
        let _ = (conn, path, found);
        Ok(())
    }
}

fn user_version(conn: &Connection) -> Result<u32, DbError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    u32::try_from(version).map_err(|_| DbError::Invalid(format!("user_version {version}")))
}

/// SQL functions every connection has. `trackside_search_folded(text)` is
/// `tracks_model::search_folded`, so search can rank exact folded matches first.
fn register_functions(conn: &Connection) -> Result<(), DbError> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "trackside_search_folded",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: Option<String> = ctx.get(0)?;
            Ok(text.map(|t| tracks_model::search_folded(&t)))
        },
    )?;
    Ok(())
}

fn configure(conn: &Connection, pragmas: Pragmas, writer: bool) -> Result<(), DbError> {
    register_functions(conn)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.set_prepared_statement_cache_capacity(128);
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         PRAGMA cache_size = -{};
         PRAGMA mmap_size = {};
         PRAGMA temp_store = MEMORY;",
        pragmas.cache_kib, pragmas.mmap_bytes
    ))?;
    if writer {
        // NORMAL is durable across crashes in WAL mode and only risks the last commit on power
        // loss.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA journal_size_limit = 67108864;",
        )?;
    }
    Ok(())
}

/// Runs a read in one transaction, so every statement in it sees the same version of the
/// database even while a write commits.
fn in_snapshot<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T, DbError>,
) -> Result<T, DbError> {
    conn.execute_batch("BEGIN DEFERRED")?;
    let result = f(conn);
    // Nothing was written, so ending it cannot lose anything; a failure here only means the
    // transaction had already ended.
    let _ = conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" });
    result
}

/// The settings every database has from its first start (architecture section 5). Existing
/// values are kept.
fn write_first_start_settings(conn: &Connection, now: Millis) -> Result<(), DbError> {
    let has_users: bool =
        conn.query_row("SELECT EXISTS (SELECT 1 FROM users)", [], |row| row.get(0))?;
    let server_id = serde_json::Value::String(uuid::Uuid::new_v4().to_string());
    let setup_state =
        serde_json::Value::String(String::from(if has_users { "done" } else { "needs_admin" }));
    let defaults = [
        ("server.id", server_id),
        ("setup.state", setup_state),
        ("sync.pruned_through", serde_json::Value::from(0)),
        (
            "schema.tracks_model",
            serde_json::Value::from(crate::TRACKS_MODEL_FOLDING),
        ),
    ];
    let mut insert =
        conn.prepare("INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT (key) DO NOTHING")?;
    for (key, value) in defaults {
        insert.execute(rusqlite::params![key, value.to_string(), now])?;
    }
    Ok(())
}

/// Refuses a database whose stored keys were built by a different `tracks-model` folding than
/// this build's: every `song_key`, `album_key`, identity and search text would disagree with
/// what Tracks computes, and a migration that recomputes them must ship with the change.
fn check_folding_version(conn: &Connection) -> Result<(), DbError> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'schema.tracks_model'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let stored: Option<u32> = stored.and_then(|v| v.trim().parse().ok());
    match stored {
        Some(version) if version == crate::TRACKS_MODEL_FOLDING => Ok(()),
        other => Err(DbError::Migration(format!(
            "stored keys use tracks-model folding {}, this server uses {}; a migration that \
             recomputes them is needed",
            other.map_or_else(|| String::from("unknown"), |v| v.to_string()),
            crate::TRACKS_MODEL_FOLDING
        ))),
    }
}

const NETWORK_FILESYSTEMS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "ncpfs",
    "afs",
    "9p",
    "ceph",
    "glusterfs",
    "davfs",
    "fuse.sshfs",
    "fuse.rclone",
    "fuse.davfs2",
    "fuse.glusterfs",
    "fuse.cephfs",
];

/// Whether `dir` is on a network filesystem, from the mount table on Linux. Other systems
/// report `false`.
pub fn is_network_filesystem(dir: &Path) -> std::io::Result<bool> {
    if !cfg!(target_os = "linux") {
        return Ok(false);
    }
    let dir = std::fs::canonicalize(dir)?;
    match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(mountinfo) => Ok(network_fs_in_mountinfo(&mountinfo, &dir)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Finds the mount holding `dir` (the longest mount point that is a prefix of it) in
/// `/proc/self/mountinfo` text and says whether its type is a network filesystem.
pub fn network_fs_in_mountinfo(mountinfo: &str, dir: &Path) -> bool {
    let mut best: Option<(usize, &str)> = None;
    for line in mountinfo.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let Some(mount_point) = before.split(' ').nth(4) else {
            continue;
        };
        let Some(fs_type) = after.split(' ').next() else {
            continue;
        };
        let mount_point = mount_point
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\134", "\\");
        if dir.starts_with(&mount_point) && best.is_none_or(|(len, _)| mount_point.len() >= len) {
            best = Some((mount_point.len(), fs_type));
        }
    }
    best.is_some_and(|(_, fs_type)| NETWORK_FILESYSTEMS.contains(&fs_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 0:50 / /mnt/nas rw,relatime shared:20 - nfs4 nas:/export rw,vers=4.2
41 22 0:51 / /mnt/nas/local\\040disk rw,relatime shared:21 - ext4 /dev/sdb1 rw
42 22 0:52 / /srv/smb rw,relatime shared:22 - cifs //host/share rw
";

    #[test]
    fn local_root_is_not_network() {
        assert!(!network_fs_in_mountinfo(
            MOUNTINFO,
            Path::new("/var/lib/trackside")
        ));
    }

    #[test]
    fn nfs_and_cifs_mounts_are_network() {
        assert!(network_fs_in_mountinfo(
            MOUNTINFO,
            Path::new("/mnt/nas/data")
        ));
        assert!(network_fs_in_mountinfo(MOUNTINFO, Path::new("/srv/smb")));
    }

    #[test]
    fn nested_local_mount_inside_nfs_is_local() {
        assert!(!network_fs_in_mountinfo(
            MOUNTINFO,
            Path::new("/mnt/nas/local disk/db")
        ));
    }

    #[test]
    fn change_kinds_round_trip() {
        for kind in [
            ChangeKind::Song,
            ChangeKind::Album,
            ChangeKind::Artist,
            ChangeKind::Library,
            ChangeKind::Playlist,
            ChangeKind::Star,
            ChangeKind::Rating,
            ChangeKind::PlayStat,
            ChangeKind::PlayQueue,
            ChangeKind::Acquisition,
        ] {
            assert_eq!(ChangeKind::parse(kind.as_str()), Some(kind));
        }
    }
}
