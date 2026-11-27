//! `GET /admin/stats/{listening,streams,storage,requests}`: the dashboard's charts. Every
//! series is aligned with its buckets, oldest first, ending with the current bucket; `null`
//! means nothing was recorded (the server was not running), `0` a measured zero.

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use http::StatusCode;
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde::Deserialize;
use trackside_auth::extract::AdminSession;
use trackside_core::repo::stats::{self, DayStorage, HourStats};
use trackside_core::repo::users::{self, User};
use trackside_core::{Millis, UserKey};

use super::read;
use crate::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Query, json_with};
use crate::wire::Time;
use crate::wire::admin::{StatsListening, StatsRequests, StatsStorage, StatsStreams, StatsUser};

const HOUR_MS: i64 = 3_600_000;

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/stats/listening", get(listening))
        .route("/stats/streams", get(streams))
        .route("/stats/storage", get(storage))
        .route("/stats/requests", get(requests))
}

#[derive(Debug, Default, Deserialize)]
struct StatsQuery {
    days: Option<i64>,
    hours: Option<i64>,
    tz: Option<String>,
}

fn in_range(value: Option<i64>, default: i64, min: i64, max: i64, field: &str) -> ApiResult<usize> {
    let value = value.unwrap_or(default);
    if (min..=max).contains(&value) {
        Ok(value as usize)
    } else {
        Err(ApiError::bad_field(
            field,
            format!("{field} is {min} to {max}."),
        ))
    }
}

/// Calendar days in one zone: their names and the instants they start at, with one more
/// boundary at the end (tomorrow's start).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayBuckets {
    pub tz: String,
    pub days: Vec<String>,
    pub starts: Vec<i64>,
}

/// The zone a browser named, or the server's own; with its name as echoed back.
fn zone(name: Option<&str>) -> (TimeZone, String) {
    if let Some(name) = name.filter(|n| !n.is_empty())
        && let Ok(tz) = TimeZone::get(name)
    {
        return (tz, name.to_owned());
    }
    let system = TimeZone::system();
    match system.iana_name().map(str::to_owned) {
        Some(name) => (system, name),
        None => (TimeZone::UTC, String::from("UTC")),
    }
}

fn day_start(date: Date, tz: &TimeZone) -> Option<i64> {
    date.to_zoned_datetime(tz.clone())
        .ok()
        .map(|z| z.timestamp().as_millisecond())
}

/// The `count` days ending today in `tz` (or the server's zone).
pub fn day_buckets(tz: Option<&str>, now: Millis, count: usize) -> DayBuckets {
    let (zone, name) = zone(tz);
    let today = Timestamp::from_millisecond(now.0)
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .to_zoned_datetime(zone.clone())
        .date();
    let mut dates = vec![today];
    while dates.len() < count {
        let Some(earlier) = dates.last().and_then(|d| d.yesterday().ok()) else {
            break;
        };
        dates.push(earlier);
    }
    dates.reverse();
    let mut starts: Vec<i64> = dates.iter().filter_map(|d| day_start(*d, &zone)).collect();
    let end = today
        .tomorrow()
        .ok()
        .and_then(|d| day_start(d, &zone))
        .unwrap_or(i64::MAX);
    starts.push(end);
    DayBuckets {
        tz: name,
        days: dates.iter().map(ToString::to_string).collect(),
        starts,
    }
}

impl DayBuckets {
    fn len(&self) -> usize {
        self.days.len()
    }

    fn index(&self, at: i64) -> Option<usize> {
        let first = *self.starts.first()?;
        let last = *self.starts.last()?;
        if at < first || at >= last {
            return None;
        }
        Some(self.starts.partition_point(|s| *s <= at).saturating_sub(1))
    }

    /// Which days the server was running on (any recorded hour overlaps), always including
    /// today.
    fn recorded(&self, running_hours: &[i64]) -> Vec<bool> {
        let mut out = vec![false; self.len()];
        for (i, flag) in out.iter_mut().enumerate() {
            let (start, end) = (self.starts[i], self.starts[i + 1]);
            *flag = running_hours
                .iter()
                .any(|h| h.saturating_add(HOUR_MS) > start && *h < end);
        }
        if let Some(today) = out.last_mut() {
            *today = true;
        }
        out
    }

    fn series(recorded: &[bool], counts: &[u32]) -> Vec<Option<u32>> {
        counts
            .iter()
            .zip(recorded)
            .map(|(n, up)| (*up || *n > 0).then_some(*n))
            .collect()
    }
}

/// The listening chart: plays per user per day, from `(user, quarter hour, count)` rows.
pub fn listening_body(
    buckets: &DayBuckets,
    users: &[User],
    plays: &[(UserKey, i64, u32)],
    running_hours: &[i64],
) -> StatsListening {
    let recorded = buckets.recorded(running_hours);
    let users = users
        .iter()
        .map(|user| {
            let mut counts = vec![0u32; buckets.len()];
            for (who, quarter, n) in plays {
                if *who == user.key
                    && let Some(i) = buckets.index(*quarter)
                {
                    counts[i] += n;
                }
            }
            StatsUser {
                id: user.id.to_string(),
                username: user.username.clone(),
                display_name: user.display_name.clone(),
                plays: DayBuckets::series(&recorded, &counts),
            }
        })
        .collect();
    StatsListening {
        tz: buckets.tz.clone(),
        bucket: String::from("day"),
        days: buckets.days.clone(),
        users,
    }
}

/// The requests chart, from `(final state, quarter hour, count)` rows.
pub fn requests_body(
    buckets: &DayBuckets,
    finished: &[(String, i64, u32)],
    running_hours: &[i64],
) -> StatsRequests {
    let recorded = buckets.recorded(running_hours);
    let mut added = vec![0u32; buckets.len()];
    let mut failed = vec![0u32; buckets.len()];
    let mut cancelled = vec![0u32; buckets.len()];
    for (state, quarter, n) in finished {
        let Some(i) = buckets.index(*quarter) else {
            continue;
        };
        match state.as_str() {
            "done" => added[i] += n,
            "failed" => failed[i] += n,
            "cancelled" => cancelled[i] += n,
            _ => {}
        }
    }
    StatsRequests {
        tz: buckets.tz.clone(),
        bucket: String::from("day"),
        days: buckets.days.clone(),
        added: DayBuckets::series(&recorded, &added),
        failed: DayBuckets::series(&recorded, &failed),
        cancelled: DayBuckets::series(&recorded, &cancelled),
    }
}

/// The streams chart for the `count` UTC hours ending with the current one. `live` fills the
/// current hour until the sampler has written it.
pub fn streams_body(
    now: Millis,
    count: usize,
    rows: &[HourStats],
    live: Option<HourStats>,
    transcode_limit: Option<u32>,
) -> StatsStreams {
    let current = stats::hour_of(now);
    let starts: Vec<i64> = (0..count as i64)
        .rev()
        .map(|back| current - back * HOUR_MS)
        .collect();
    let found = |start: i64| -> Option<HourStats> {
        let stored = rows.iter().find(|r| r.hour_start == start).copied();
        if start == current {
            let live = live.map(|mut l| {
                if let Some(s) = stored {
                    l.streams = l.streams.max(s.streams);
                    l.transcodes = l.transcodes.max(s.transcodes);
                    l.failed += s.failed;
                }
                l
            });
            return live.or(stored);
        }
        stored
    };
    let hours: Vec<Option<HourStats>> = starts.iter().map(|s| found(*s)).collect();
    StatsStreams {
        bucket: String::from("hour"),
        hours: starts.iter().map(|s| Time(Millis(*s))).collect(),
        streams: hours.iter().map(|h| h.map(|h| h.streams)).collect(),
        transcodes: hours.iter().map(|h| h.map(|h| h.transcodes)).collect(),
        failed: hours.iter().map(|h| h.map(|h| h.failed)).collect(),
        transcode_limit,
    }
}

/// The storage chart for the `count` server-local days ending today.
pub fn storage_body(buckets: &DayBuckets, rows: &[DayStorage]) -> StatsStorage {
    let pick = |day: &str| rows.iter().find(|r| r.day == day);
    let column = |f: fn(&DayStorage) -> Option<u64>| -> Vec<Option<u64>> {
        buckets.days.iter().map(|d| pick(d).and_then(f)).collect()
    };
    StatsStorage {
        bucket: String::from("day"),
        days: buckets.days.clone(),
        data_free_bytes: column(|r| r.data_free_bytes),
        database_bytes: column(|r| r.database_bytes),
        cache_bytes: column(|r| r.cache_bytes),
        volume_bytes: rows.iter().rev().find_map(|r| r.volume_bytes),
    }
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response
}

async fn running_hours(state: &ApiState, from: i64, to: i64) -> ApiResult<Vec<i64>> {
    let rows = read(state, move |c| stats::hours(c, from, to)).await?;
    Ok(rows.into_iter().map(|r| r.hour_start).collect())
}

async fn listening(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Response> {
    let count = in_range(q.days, 30, 7, 90, "days")?;
    let buckets = day_buckets(q.tz.as_deref(), state.db.clock().now(), count);
    let (from, to) = (buckets.starts[0], buckets.starts[count]);
    let (users, plays) = read(&state, move |c| {
        Ok((
            users::list(c)?,
            stats::plays_by_quarter(c, Millis(from), Millis(to))?,
        ))
    })
    .await?;
    let running = running_hours(&state, from - HOUR_MS, to).await?;
    let body = listening_body(&buckets, &users, &plays, &running);
    Ok(no_store(json_with(StatusCode::OK, &body)))
}

async fn requests(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Response> {
    if state.acquire.providers().await.is_empty() {
        return Err(ApiError::not_found());
    }
    let count = in_range(q.days, 30, 7, 90, "days")?;
    let buckets = day_buckets(q.tz.as_deref(), state.db.clock().now(), count);
    let (from, to) = (buckets.starts[0], buckets.starts[count]);
    let finished = read(&state, move |c| {
        stats::finished_acquisitions_by_quarter(c, Millis(from), Millis(to))
    })
    .await?;
    let running = running_hours(&state, from - HOUR_MS, to).await?;
    let body = requests_body(&buckets, &finished, &running);
    Ok(no_store(json_with(StatusCode::OK, &body)))
}

async fn streams(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Response> {
    let count = in_range(q.hours, 24, 6, 48, "hours")?;
    let now = state.db.clock().now();
    let current = stats::hour_of(now);
    let from = current - (count as i64 - 1) * HOUR_MS;
    let rows = read(&state, move |c| stats::hours(c, from, current + HOUR_MS)).await?;
    let (running, _) = state.media.transcode_load();
    let live = HourStats {
        hour_start: current,
        streams: state.local.playback().active(),
        transcodes: running as u32,
        failed: 0,
    };
    let limit = state.media.transcode_limit().map(|n| n as u32);
    let body = streams_body(now, count, &rows, Some(live), limit);
    Ok(no_store(json_with(StatusCode::OK, &body)))
}

async fn storage(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Response> {
    let count = in_range(q.days, 90, 7, 365, "days")?;
    let buckets = day_buckets(None, state.db.clock().now(), count);
    let (first, last) = (
        buckets.days.first().cloned().unwrap_or_default(),
        buckets.days.last().cloned().unwrap_or_default(),
    );
    let rows = read(&state, move |c| stats::days(c, &first, &last)).await?;
    let body = storage_body(&buckets, &rows);
    Ok(no_store(json_with(StatusCode::OK, &body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc: &str) -> Millis {
        Millis(
            rfc.parse::<Timestamp>()
                .map_or(0, Timestamp::as_millisecond),
        )
    }

    #[test]
    fn days_follow_the_zone_across_a_dst_change() {
        let b = day_buckets(Some("Europe/London"), at("2026-10-26T12:00:00Z"), 3);
        assert_eq!(b.tz, "Europe/London");
        assert_eq!(b.days, ["2026-10-24", "2026-10-25", "2026-10-26"]);
        let lengths: Vec<i64> = b
            .starts
            .windows(2)
            .map(|w| (w[1] - w[0]) / HOUR_MS)
            .collect();
        assert_eq!(lengths, [24, 25, 24]);
    }

    #[test]
    fn unknown_zones_fall_back_and_say_which() {
        let b = day_buckets(Some("Mars/Olympus"), at("2026-11-14T12:00:00Z"), 7);
        assert_ne!(b.tz, "Mars/Olympus");
        assert_eq!(b.days.len(), 7);
        assert_eq!(b.starts.len(), 8);
    }

    #[test]
    fn days_without_uptime_are_null_and_plays_count_anyway() {
        let b = day_buckets(Some("UTC"), at("2026-11-14T12:00:00Z"), 7);
        let running = [at("2026-11-12T05:00:00Z").0];
        let user = User {
            key: UserKey(1),
            id: trackside_core::random_id(),
            username: String::from("luke"),
            display_name: String::from("Luke"),
            role: trackside_core::Role::Admin,
            permissions: trackside_core::Permissions::default(),
            sync_epoch: 1,
            created_at: Millis(0),
            updated_at: Millis(0),
            password_changed_at: Millis(0),
            disabled_at: None,
            must_change_password: false,
            service: None,
        };
        let plays = [
            (UserKey(1), at("2026-11-12T05:15:00Z").0, 3),
            (UserKey(1), at("2026-11-10T23:45:00Z").0, 2),
        ];
        let body = listening_body(&b, &[user], &plays, &running);
        assert_eq!(
            body.users[0].plays,
            [None, None, Some(2), None, Some(3), None, Some(0)]
        );
    }

    #[test]
    fn hours_are_aligned_and_the_current_one_is_live() {
        let now = at("2026-11-14T10:20:00Z");
        let rows = [HourStats {
            hour_start: at("2026-11-14T08:00:00Z").0,
            streams: 4,
            transcodes: 1,
            failed: 2,
        }];
        let live = HourStats {
            hour_start: at("2026-11-14T10:00:00Z").0,
            streams: 1,
            ..HourStats::default()
        };
        let body = streams_body(now, 6, &rows, Some(live), Some(4));
        assert_eq!(body.hours.len(), 6);
        assert_eq!(body.streams, [None, None, None, Some(4), None, Some(1)]);
        assert_eq!(body.failed[3], Some(2));
        assert_eq!(body.transcode_limit, Some(4));
    }
}
