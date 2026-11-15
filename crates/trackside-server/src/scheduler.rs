//! Background jobs: scheduled backups, hourly maintenance (pruning, WAL checkpoint), refreshing
//! the detected URLs, and the systemd watchdog.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use tokio_util::sync::CancellationToken;
use trackside_core::config::BackupSchedule;
use trackside_core::repo::{acquisitions, idempotency, imports, pairings, sessions};
use trackside_core::{Db, Millis};

use crate::backup::{BackupKind, Backups, next_run};
use crate::health::BackupStatus;

/// What maintenance removes and how old it must be.
#[derive(Clone, Copy, Debug)]
pub struct Retention {
    pub tombstones: Duration,
    pub missing: Duration,
}

/// Uploads nothing was sent to for this long are given up.
pub const ABANDONED_UPLOAD: Duration = Duration::from_secs(24 * 3600);

/// One round of maintenance: expired sessions, pairings, idempotency records and candidates,
/// abandoned uploads and their staging files, old tombstones, songs missing past the grace
/// period, then a WAL checkpoint.
pub async fn maintain(db: &Db, retention: Retention) -> Result<(), trackside_core::DbError> {
    let now = db.clock().now();
    let abandoned = db
        .write(move |tx| imports::prune_abandoned(tx, now.saturating_sub(ABANDONED_UPLOAD)))
        .await?;
    tokio::task::spawn_blocking(move || {
        for path in abandoned {
            if let Err(e) = std::fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(error = %e, path = %path.display(), "abandoned upload not deleted");
            }
        }
    })
    .await
    .map_err(|_| trackside_core::DbError::Closed)?;
    db.write(move |tx| {
        sessions::prune(tx, now)?;
        pairings::prune(tx, now)?;
        idempotency::prune(tx, now.saturating_sub(Duration::from_secs(86_400)))?;
        acquisitions::prune_candidates(tx, now)?;
        trackside_core::sync::prune_tombstones(tx, now.saturating_sub(retention.tombstones))?;
        trackside_core::ingest::purge_missing(tx, now.saturating_sub(retention.missing))?;
        Ok(())
    })
    .await?;
    let db = db.clone();
    tokio::task::spawn_blocking(move || db.checkpoint_and_optimize())
        .await
        .map_err(|_| trackside_core::DbError::Closed)?
}

pub async fn maintenance_loop(
    db: Db,
    media: std::sync::Arc<trackside_media::Media>,
    retention: Retention,
    stop: CancellationToken,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = ticker.tick() => {}
        }
        if let Err(e) = maintain(&db, retention).await {
            tracing::warn!(error = %e, "maintenance failed");
        }
        media.evict_caches().await;
    }
}

pub async fn backup_loop(
    db: Db,
    backups: Arc<Backups>,
    schedule: BackupSchedule,
    status: Arc<Mutex<BackupStatus>>,
    stop: CancellationToken,
) {
    let zone = TimeZone::system();
    loop {
        let now = Timestamp::from_millisecond(db.clock().now().0).unwrap_or(Timestamp::UNIX_EPOCH);
        let Some(next) = next_run(schedule, now, &zone) else {
            return;
        };
        status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next_at = Some(Millis(next.as_millisecond()));
        let wait = Duration::from_millis(
            u64::try_from(next.as_millisecond() - now.as_millisecond()).unwrap_or(0),
        );
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
        let (db2, backups2) = (db.clone(), Arc::clone(&backups));
        let at = db.clock().now();
        let result = tokio::task::spawn_blocking(move || {
            backups2.create_blocking(&db2, BackupKind::Scheduled, at)
        })
        .await;
        let error = match result {
            Ok(Ok(path)) => {
                tracing::info!(path = %path.display(), "scheduled backup written");
                None
            }
            Ok(Err(e)) => Some(e.to_string()),
            Err(e) => Some(e.to_string()),
        };
        if let Some(error) = &error {
            tracing::error!(%error, "scheduled backup failed");
        }
        status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_error = error;
    }
}

/// Re-detects LAN URLs every five minutes, since DHCP can move them.
pub async fn url_loop(refresh: impl Fn() + Send + 'static, stop: CancellationToken) {
    let mut ticker = tokio::time::interval(Duration::from_secs(300));
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = ticker.tick() => refresh(),
        }
    }
}

/// Pings the systemd watchdog at half its interval while the server runs.
pub async fn watchdog_loop(interval: Duration, stop: CancellationToken) {
    let mut ticker = tokio::time::interval(interval / 2);
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = ticker.tick() => {
                let _ = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]);
            }
        }
    }
}

/// Where the stats sampler reads its numbers.
#[derive(Clone)]
pub struct StatsSources {
    pub db: Db,
    pub playback: Arc<trackside_api::PlaybackLoad>,
    pub media: Arc<trackside_media::Media>,
    pub data_dir: std::path::PathBuf,
    pub cache_dir: std::path::PathBuf,
}

impl std::fmt::Debug for StatsSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatsSources").finish_non_exhaustive()
    }
}

/// How often concurrency is sampled, how often the samples are written, and how often the
/// day's storage snapshot is refreshed.
const SAMPLE_EVERY: Duration = Duration::from_secs(10);
const SAMPLES_PER_WRITE: u32 = 6;
const STORAGE_EVERY: Duration = Duration::from_secs(600);
/// Hourly rows also mark when the server ran, which daily charts use for "not recorded", so
/// they are kept as long as the longest daily chart.
const HOURLY_KEEP: Duration = Duration::from_secs(90 * 86_400);
const DAILY_KEEP_DAYS: i64 = 400;

/// Samples playback load into `stats_hourly` and storage into `stats_daily` until `stop`.
/// The first sample is written at once, so the current hour counts as recorded.
pub async fn stats_loop(sources: StatsSources, stop: CancellationToken) {
    let mut ticker = tokio::time::interval(SAMPLE_EVERY);
    let mut tick: u32 = 0;
    let mut peak_transcodes: u32 = 0;
    let mut last_storage: Option<tokio::time::Instant> = None;
    loop {
        let stopping = tokio::select! {
            () = stop.cancelled() => true,
            _ = ticker.tick() => false,
        };
        let (running, _) = sources.media.transcode_load();
        peak_transcodes = peak_transcodes.max(running as u32);
        if stopping || tick.is_multiple_of(SAMPLES_PER_WRITE) {
            let (streams, failed) = sources.playback.take();
            let sample = trackside_core::repo::stats::HourStats {
                hour_start: trackside_core::repo::stats::hour_of(sources.db.clock().now()),
                streams,
                transcodes: peak_transcodes,
                failed,
            };
            peak_transcodes = running as u32;
            if let Err(e) = sources
                .db
                .write(move |tx| trackside_core::repo::stats::record_hour(tx, &sample))
                .await
            {
                tracing::debug!(error = %e, "playback stats not written");
            }
        }
        if stopping {
            return;
        }
        if last_storage.is_none_or(|t| t.elapsed() >= STORAGE_EVERY) {
            last_storage = Some(tokio::time::Instant::now());
            if let Err(e) = snapshot_storage(&sources).await {
                tracing::debug!(error = %e, "storage stats not written");
            }
        }
        tick = tick.wrapping_add(1);
    }
}

async fn snapshot_storage(sources: &StatsSources) -> Result<(), trackside_core::DbError> {
    let s = sources.clone();
    let day = tokio::task::spawn_blocking(move || {
        let database = s.db.path().map_or(0, |p| {
            let wal = std::path::PathBuf::from(format!("{}-wal", p.display()));
            file_len(p) + file_len(&wal)
        });
        trackside_core::repo::stats::DayStorage {
            day: today_local(s.db.clock().now()),
            data_free_bytes: fs4::available_space(&s.data_dir).ok(),
            database_bytes: Some(database),
            cache_bytes: Some(crate::health::dir_size(&s.cache_dir)),
            volume_bytes: fs4::total_space(&s.data_dir).ok(),
        }
    })
    .await
    .map_err(|_| trackside_core::DbError::Closed)?;
    let now = sources.db.clock().now();
    let hour_before = trackside_core::repo::stats::hour_of(now.saturating_sub(HOURLY_KEEP));
    let day_before = local_date(now)
        .checked_sub(jiff::Span::new().days(DAILY_KEEP_DAYS))
        .map(|d| d.to_string())
        .unwrap_or_default();
    sources
        .db
        .write(move |tx| {
            trackside_core::repo::stats::put_day(tx, &day)?;
            trackside_core::repo::stats::prune(tx, hour_before, &day_before)
        })
        .await
}

fn file_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

fn local_date(now: Millis) -> jiff::civil::Date {
    Timestamp::from_millisecond(now.0)
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .to_zoned(TimeZone::system())
        .date()
}

/// Today in the server's zone, as `stats_daily` keys it.
pub fn today_local(now: Millis) -> String {
    local_date(now).to_string()
}

#[cfg(test)]
mod tests {
    use trackside_core::access::Role;
    use trackside_core::records::ImportSource;
    use trackside_core::repo::imports::NewImport;
    use trackside_core::testing::TestDb;

    use super::*;

    #[tokio::test]
    async fn maintenance_deletes_abandoned_uploads_and_their_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let test = TestDb::new()?;
        let staging = tempfile::tempdir()?;
        let user = test.add_user("luke", Role::Member, &[])?.key;
        let staged = staging.path().join("left.part");
        std::fs::write(&staged, b"half")?;
        let new = NewImport {
            user: Some(user),
            source: ImportSource::Upload,
            original_name: String::from("left.flac"),
            staged_path: Some(staged.clone()),
            expected_bytes: Some(10),
            target_library: None,
        };
        let row = test.db.write(move |tx| imports::create(tx, &new)).await?;
        test.clock
            .advance(ABANDONED_UPLOAD + Duration::from_secs(60));
        let retention = Retention {
            tombstones: Duration::from_secs(86_400 * 90),
            missing: Duration::from_secs(86_400 * 30),
        };
        maintain(&test.db, retention).await?;
        assert!(!staged.exists());
        let id = row.id;
        assert!(test.db.read(move |c| imports::get(c, &id)).await?.is_none());
        Ok(())
    }
}
