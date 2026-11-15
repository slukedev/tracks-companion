//! Rollups behind the dashboard's charts: hourly playback load, daily storage snapshots, and
//! cheap grouped counts of listens and finished acquisitions.

use rusqlite::{Connection, params};

use crate::db::{DbError, WriteTx};
use crate::ids::UserKey;
use crate::time::Millis;

/// An hour's playback load.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HourStats {
    /// UTC milliseconds at the start of the hour.
    pub hour_start: i64,
    pub streams: u32,
    pub transcodes: u32,
    pub failed: u32,
}

const HOUR_MS: i64 = 3_600_000;
/// Grouping step for counts that are later bucketed into local days: every UTC offset in use
/// is a multiple of 15 minutes.
pub const QUARTER_MS: i64 = 900_000;

/// The start of the UTC hour `at` falls in.
pub fn hour_of(at: Millis) -> i64 {
    at.0.div_euclid(HOUR_MS) * HOUR_MS
}

/// Merges a sample into its hour: the highest concurrency wins and failures add up.
pub fn record_hour(tx: &mut WriteTx<'_>, sample: &HourStats) -> Result<(), DbError> {
    tx.prepare_cached(
        "INSERT INTO stats_hourly (hour_start, streams, transcodes, failed) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (hour_start) DO UPDATE SET
             streams = max(streams, excluded.streams),
             transcodes = max(transcodes, excluded.transcodes),
             failed = failed + excluded.failed",
    )?
    .execute(params![
        sample.hour_start,
        sample.streams,
        sample.transcodes,
        sample.failed
    ])?;
    Ok(())
}

/// Recorded hours in `[from, to)`, oldest first.
pub fn hours(c: &Connection, from: i64, to: i64) -> Result<Vec<HourStats>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT hour_start, streams, transcodes, failed FROM stats_hourly
         WHERE hour_start >= ?1 AND hour_start < ?2 ORDER BY hour_start",
    )?;
    let rows = statement.query_map([from, to], |row| {
        Ok(HourStats {
            hour_start: row.get(0)?,
            streams: row.get(1)?,
            transcodes: row.get(2)?,
            failed: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A day's storage snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DayStorage {
    /// `YYYY-MM-DD` in the server's zone.
    pub day: String,
    pub data_free_bytes: Option<u64>,
    pub database_bytes: Option<u64>,
    pub cache_bytes: Option<u64>,
    pub volume_bytes: Option<u64>,
}

fn to_i64(v: Option<u64>) -> Option<i64> {
    v.map(|v| i64::try_from(v).unwrap_or(i64::MAX))
}

fn to_u64(v: Option<i64>) -> Option<u64> {
    v.map(|v| u64::try_from(v).unwrap_or(0))
}

/// Replaces the day's snapshot with a newer one.
pub fn put_day(tx: &mut WriteTx<'_>, day: &DayStorage) -> Result<(), DbError> {
    let now = tx.now();
    tx.prepare_cached(
        "INSERT INTO stats_daily (day, data_free_bytes, database_bytes, cache_bytes,
             volume_bytes, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (day) DO UPDATE SET data_free_bytes = excluded.data_free_bytes,
             database_bytes = excluded.database_bytes, cache_bytes = excluded.cache_bytes,
             volume_bytes = excluded.volume_bytes, updated_at = excluded.updated_at",
    )?
    .execute(params![
        day.day,
        to_i64(day.data_free_bytes),
        to_i64(day.database_bytes),
        to_i64(day.cache_bytes),
        to_i64(day.volume_bytes),
        now
    ])?;
    Ok(())
}

/// Snapshots for days in `[from, to]` (inclusive, `YYYY-MM-DD`), oldest first.
pub fn days(c: &Connection, from: &str, to: &str) -> Result<Vec<DayStorage>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT day, data_free_bytes, database_bytes, cache_bytes, volume_bytes FROM stats_daily
         WHERE day >= ?1 AND day <= ?2 ORDER BY day",
    )?;
    let rows = statement.query_map([from, to], |row| {
        Ok(DayStorage {
            day: row.get(0)?,
            data_free_bytes: to_u64(row.get(1)?),
            database_bytes: to_u64(row.get(2)?),
            cache_bytes: to_u64(row.get(3)?),
            volume_bytes: to_u64(row.get(4)?),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Listens per user per quarter hour (start of the quarter in UTC ms) in `[from, to)`.
pub fn plays_by_quarter(
    c: &Connection,
    from: Millis,
    to: Millis,
) -> Result<Vec<(UserKey, i64, u32)>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT user_id, (started_at / 900000) * 900000 AS q, count(*) FROM listens
         WHERE started_at >= ?1 AND started_at < ?2 GROUP BY user_id, q",
    )?;
    let rows = statement.query_map([from, to], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Acquisitions that reached `done`, `failed` or `cancelled`, per state and quarter hour of
/// `finished_at`, in `[from, to)`.
pub fn finished_acquisitions_by_quarter(
    c: &Connection,
    from: Millis,
    to: Millis,
) -> Result<Vec<(String, i64, u32)>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT state, (coalesce(finished_at, updated_at) / 900000) * 900000 AS q, count(*)
         FROM acquisitions
         WHERE state IN ('done', 'failed', 'cancelled')
             AND coalesce(finished_at, updated_at) >= ?1 AND coalesce(finished_at, updated_at) < ?2
         GROUP BY state, q",
    )?;
    let rows = statement.query_map([from, to], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Drops hourly rows before `hour_before` and daily rows before `day_before`.
pub fn prune(tx: &mut WriteTx<'_>, hour_before: i64, day_before: &str) -> Result<(), DbError> {
    tx.prepare_cached("DELETE FROM stats_hourly WHERE hour_start < ?1")?
        .execute([hour_before])?;
    tx.prepare_cached("DELETE FROM stats_daily WHERE day < ?1")?
        .execute([day_before])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestDb;

    #[tokio::test]
    async fn hours_merge_by_max_and_sum() -> Result<(), Box<dyn std::error::Error>> {
        let t = TestDb::new()?;
        for (streams, failed) in [(2, 1), (5, 0), (3, 2)] {
            let sample = HourStats {
                hour_start: 7_200_000,
                streams,
                transcodes: 1,
                failed,
            };
            t.db.write(move |tx| record_hour(tx, &sample)).await?;
        }
        let rows = t.db.read(|c| hours(c, 0, 10_800_000)).await?;
        assert_eq!(
            rows,
            vec![HourStats {
                hour_start: 7_200_000,
                streams: 5,
                transcodes: 1,
                failed: 3
            }]
        );
        assert_eq!(hour_of(Millis(7_299_999)), 7_200_000);
        Ok(())
    }

    #[tokio::test]
    async fn days_keep_the_latest_snapshot_and_prune() -> Result<(), Box<dyn std::error::Error>> {
        let t = TestDb::new()?;
        for free in [10, 20] {
            let day = DayStorage {
                day: String::from("2026-11-14"),
                data_free_bytes: Some(free),
                ..DayStorage::default()
            };
            t.db.write(move |tx| put_day(tx, &day)).await?;
        }
        let rows = t.db.read(|c| days(c, "2026-11-01", "2026-11-30")).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].data_free_bytes, Some(20));
        t.db.write(|tx| prune(tx, 0, "2026-11-15")).await?;
        let left = t.db.read(|c| days(c, "2026-11-01", "2026-11-30")).await?;
        assert_eq!(left, Vec::new());
        Ok(())
    }
}
