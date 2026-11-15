//! RFC 9110 range and conditional request evaluation, without I/O.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::HeaderMap;
use http::header::{
    IF_MATCH, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, IF_UNMODIFIED_SINCE, RANGE,
};

/// The outcome of evaluating range and conditional headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeOutcome {
    Full,
    Partial { start: u64, end_inclusive: u64 },
    Unsatisfiable,
    NotModified,
    PreconditionFailed,
}

/// Evaluates the conditional and range headers of a `GET` or `HEAD` for an entity of `len`
/// bytes with the strong `etag` (quoted) and `last_modified`.
///
/// Preconditions are checked in the RFC 9110 order (`If-Match`, `If-Unmodified-Since`,
/// `If-None-Match`, `If-Modified-Since`), then `If-Range` and `Range`. Several ranges are
/// answered with the whole entity, unless none of them can be satisfied.
pub fn evaluate(
    headers: &HeaderMap,
    len: u64,
    etag: &str,
    last_modified: SystemTime,
) -> RangeOutcome {
    let modified = whole_seconds(last_modified);

    if let Some(value) = header_str(headers, &IF_MATCH) {
        if !etag_list_matches(value, etag, true) {
            return RangeOutcome::PreconditionFailed;
        }
    } else if let Some(since) = header_date(headers, &IF_UNMODIFIED_SINCE)
        && modified > since
    {
        return RangeOutcome::PreconditionFailed;
    }

    if let Some(value) = header_str(headers, &IF_NONE_MATCH) {
        if etag_list_matches(value, etag, false) {
            return RangeOutcome::NotModified;
        }
    } else if let Some(since) = header_date(headers, &IF_MODIFIED_SINCE)
        && modified <= since
    {
        return RangeOutcome::NotModified;
    }

    let Some(range) = header_str(headers, &RANGE) else {
        return RangeOutcome::Full;
    };
    if let Some(if_range) = header_str(headers, &IF_RANGE)
        && !if_range_matches(if_range, etag, modified)
    {
        return RangeOutcome::Full;
    }
    parse_range(range, len)
}

fn header_str<'h>(headers: &'h HeaderMap, name: &http::HeaderName) -> Option<&'h str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

fn header_date(headers: &HeaderMap, name: &http::HeaderName) -> Option<SystemTime> {
    header_str(headers, name).and_then(|v| httpdate::parse_http_date(v).ok())
}

/// HTTP dates have one-second resolution, so comparisons use whole seconds.
pub(crate) fn whole_seconds(t: SystemTime) -> SystemTime {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// `If-Match` uses the strong comparison, `If-None-Match` the weak one.
fn etag_list_matches(list: &str, etag: &str, strong: bool) -> bool {
    if list == "*" {
        return true;
    }
    list.split(',').map(str::trim).any(|candidate| {
        if let Some(weak) = candidate.strip_prefix("W/") {
            !strong && weak == etag
        } else {
            candidate == etag
        }
    })
}

/// `If-Range` holds one strong `ETag` or one date; a weak `ETag` never matches.
fn if_range_matches(value: &str, etag: &str, modified: SystemTime) -> bool {
    if value.starts_with('"') {
        return value == etag;
    }
    if value.starts_with("W/") {
        return false;
    }
    httpdate::parse_http_date(value).is_ok_and(|date| date == modified)
}

#[derive(Clone, Copy)]
enum Spec {
    FromTo(u64, Option<u64>),
    Suffix(u64),
}

fn parse_spec(spec: &str) -> Option<Spec> {
    let (first, last) = spec.trim().split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        return last.parse().ok().map(Spec::Suffix);
    }
    let start: u64 = first.parse().ok()?;
    if last.is_empty() {
        return Some(Spec::FromTo(start, None));
    }
    let end: u64 = last.parse().ok()?;
    (end >= start).then_some(Spec::FromTo(start, Some(end)))
}

fn resolve(spec: Spec, len: u64) -> Option<(u64, u64)> {
    match spec {
        Spec::FromTo(start, end) => {
            if start >= len {
                return None;
            }
            let end = end.map_or(len - 1, |e| e.min(len - 1));
            Some((start, end))
        }
        Spec::Suffix(n) => {
            if n == 0 || len == 0 {
                return None;
            }
            Some((len - n.min(len), len - 1))
        }
    }
}

/// A syntactically invalid `Range` is ignored, as RFC 9110 requires.
fn parse_range(value: &str, len: u64) -> RangeOutcome {
    let Some((unit, set)) = value.split_once('=') else {
        return RangeOutcome::Full;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return RangeOutcome::Full;
    }
    let mut specs = Vec::new();
    for part in set.split(',') {
        if part.trim().is_empty() {
            continue;
        }
        match parse_spec(part) {
            Some(spec) => specs.push(spec),
            None => return RangeOutcome::Full,
        }
    }
    match specs.as_slice() {
        [] => RangeOutcome::Full,
        [one] => match resolve(*one, len) {
            Some((start, end_inclusive)) => RangeOutcome::Partial {
                start,
                end_inclusive,
            },
            None => RangeOutcome::Unsatisfiable,
        },
        many => {
            if many.iter().any(|s| resolve(*s, len).is_some()) {
                RangeOutcome::Full
            } else {
                RangeOutcome::Unsatisfiable
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    const ETAG: &str = "\"s123.4\"";
    const LEN: u64 = 1000;

    fn modified() -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(1_794_648_600_250)
    }

    fn eval(pairs: &[(http::HeaderName, &str)]) -> RangeOutcome {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                name.clone(),
                HeaderValue::from_str(value).unwrap_or(HeaderValue::from_static("")),
            );
        }
        evaluate(&headers, LEN, ETAG, modified())
    }

    fn date(t: SystemTime) -> String {
        httpdate::fmt_http_date(t)
    }

    fn partial(start: u64, end_inclusive: u64) -> RangeOutcome {
        RangeOutcome::Partial {
            start,
            end_inclusive,
        }
    }

    type Case<'a> = (&'a str, Vec<(http::HeaderName, &'a str)>, RangeOutcome);

    #[test]
    #[expect(clippy::too_many_lines, reason = "a table with one entry per case")]
    fn range_table() {
        let later = date(modified() + Duration::from_secs(60));
        let earlier = date(modified() - Duration::from_secs(60));
        let same = date(modified());
        let cases: Vec<Case<'_>> = vec![
            ("no range", vec![], RangeOutcome::Full),
            ("avplayer probe", vec![(RANGE, "bytes=0-1")], partial(0, 1)),
            ("closed", vec![(RANGE, "bytes=100-199")], partial(100, 199)),
            ("open", vec![(RANGE, "bytes=100-")], partial(100, 999)),
            ("suffix", vec![(RANGE, "bytes=-500")], partial(500, 999)),
            (
                "suffix longer than file",
                vec![(RANGE, "bytes=-5000")],
                partial(0, 999),
            ),
            (
                "end past length clamps",
                vec![(RANGE, "bytes=900-5000")],
                partial(900, 999),
            ),
            (
                "last byte",
                vec![(RANGE, "bytes=999-999")],
                partial(999, 999),
            ),
            (
                "multiple ranges as full",
                vec![(RANGE, "bytes=0-1,5-9")],
                RangeOutcome::Full,
            ),
            (
                "multiple with one satisfiable",
                vec![(RANGE, "bytes=0-1,5000-")],
                RangeOutcome::Full,
            ),
            (
                "multiple none satisfiable",
                vec![(RANGE, "bytes=1000-,2000-")],
                RangeOutcome::Unsatisfiable,
            ),
            (
                "start at length",
                vec![(RANGE, "bytes=1000-")],
                RangeOutcome::Unsatisfiable,
            ),
            (
                "start past length",
                vec![(RANGE, "bytes=5000-6000")],
                RangeOutcome::Unsatisfiable,
            ),
            (
                "zero suffix",
                vec![(RANGE, "bytes=-0")],
                RangeOutcome::Unsatisfiable,
            ),
            (
                "reversed is ignored",
                vec![(RANGE, "bytes=9-5")],
                RangeOutcome::Full,
            ),
            (
                "garbage is ignored",
                vec![(RANGE, "bytes=abc")],
                RangeOutcome::Full,
            ),
            (
                "other unit is ignored",
                vec![(RANGE, "items=0-5")],
                RangeOutcome::Full,
            ),
            (
                "unit is case-insensitive",
                vec![(RANGE, "Bytes=0-5")],
                partial(0, 5),
            ),
            ("spaces", vec![(RANGE, "bytes= 10 - 20 ")], partial(10, 20)),
            (
                "if-range matching etag",
                vec![(RANGE, "bytes=10-"), (IF_RANGE, ETAG)],
                partial(10, 999),
            ),
            (
                "if-range stale etag",
                vec![(RANGE, "bytes=10-"), (IF_RANGE, "\"s123.3\"")],
                RangeOutcome::Full,
            ),
            (
                "if-range weak etag",
                vec![(RANGE, "bytes=10-"), (IF_RANGE, "W/\"s123.4\"")],
                RangeOutcome::Full,
            ),
            (
                "if-range matching date",
                vec![(RANGE, "bytes=10-"), (IF_RANGE, &same)],
                partial(10, 999),
            ),
            (
                "if-range stale date",
                vec![(RANGE, "bytes=10-"), (IF_RANGE, &earlier)],
                RangeOutcome::Full,
            ),
            (
                "if-none-match match",
                vec![(IF_NONE_MATCH, ETAG)],
                RangeOutcome::NotModified,
            ),
            (
                "if-none-match weak match",
                vec![(IF_NONE_MATCH, "W/\"s123.4\"")],
                RangeOutcome::NotModified,
            ),
            (
                "if-none-match in list",
                vec![(IF_NONE_MATCH, "\"a\", \"s123.4\"")],
                RangeOutcome::NotModified,
            ),
            (
                "if-none-match star",
                vec![(IF_NONE_MATCH, "*")],
                RangeOutcome::NotModified,
            ),
            (
                "if-none-match other",
                vec![(IF_NONE_MATCH, "\"s123.3\""), (RANGE, "bytes=0-1")],
                partial(0, 1),
            ),
            (
                "if-none-match beats if-modified-since",
                vec![(IF_NONE_MATCH, "\"x\""), (IF_MODIFIED_SINCE, &later)],
                RangeOutcome::Full,
            ),
            (
                "if-modified-since later",
                vec![(IF_MODIFIED_SINCE, &later)],
                RangeOutcome::NotModified,
            ),
            (
                "if-modified-since same second",
                vec![(IF_MODIFIED_SINCE, &same)],
                RangeOutcome::NotModified,
            ),
            (
                "if-modified-since earlier",
                vec![(IF_MODIFIED_SINCE, &earlier)],
                RangeOutcome::Full,
            ),
            (
                "if-match match",
                vec![(IF_MATCH, ETAG), (RANGE, "bytes=0-1")],
                partial(0, 1),
            ),
            (
                "if-match weak never matches",
                vec![(IF_MATCH, "W/\"s123.4\"")],
                RangeOutcome::PreconditionFailed,
            ),
            (
                "if-match other",
                vec![(IF_MATCH, "\"s123.3\"")],
                RangeOutcome::PreconditionFailed,
            ),
            (
                "if-unmodified-since earlier",
                vec![(IF_UNMODIFIED_SINCE, &earlier)],
                RangeOutcome::PreconditionFailed,
            ),
            (
                "if-unmodified-since later",
                vec![(IF_UNMODIFIED_SINCE, &later)],
                RangeOutcome::Full,
            ),
        ];
        for (name, headers, expected) in cases {
            assert_eq!(eval(&headers), expected, "{name}");
        }
    }

    #[test]
    fn empty_entity_cannot_satisfy_any_range() {
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=0-1"));
        assert_eq!(
            evaluate(&headers, 0, ETAG, modified()),
            RangeOutcome::Unsatisfiable
        );
        headers.insert(RANGE, HeaderValue::from_static("bytes=-1"));
        assert_eq!(
            evaluate(&headers, 0, ETAG, modified()),
            RangeOutcome::Unsatisfiable
        );
    }

    proptest::proptest! {
        #[test]
        fn partial_ranges_stay_inside_the_entity(len in 0u64..10_000, a in 0u64..20_000, b in 0u64..20_000, suffix: bool) {
            let value = if suffix { format!("bytes=-{a}") } else { format!("bytes={a}-{b}") };
            let mut headers = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(&value) {
                headers.insert(RANGE, v);
            }
            if let RangeOutcome::Partial { start, end_inclusive } = evaluate(&headers, len, ETAG, modified()) {
                proptest::prop_assert!(start <= end_inclusive);
                proptest::prop_assert!(end_inclusive < len);
            }
        }
    }
}

#[cfg(test)]
mod header_properties {
    use http::header::{IF_RANGE, RANGE};
    use http::{HeaderMap, HeaderValue};
    use proptest::prelude::*;

    use super::{RangeOutcome, evaluate};

    proptest! {
        /// Whatever a client sends as `Range` and `If-Range`, the answer is one of the
        /// outcomes and a partial range lies inside the entity.
        #[test]
        fn any_range_header_gives_a_valid_outcome(
            range in "(bytes|BYTES|items|)=?[0-9 ,\\-]{0,24}|[ -~]{0,40}",
            if_range in proptest::option::of("[ -~]{0,30}"),
            len in 0u64..100_000,
        ) {
            let mut headers = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(&range) {
                headers.insert(RANGE, v);
            }
            if let Some(Ok(v)) = if_range.map(|s| HeaderValue::from_str(&s)) {
                headers.insert(IF_RANGE, v);
            }
            match evaluate(&headers, len, "\"etag\"", std::time::UNIX_EPOCH) {
                RangeOutcome::Partial { start, end_inclusive } => {
                    prop_assert!(start <= end_inclusive && end_inclusive < len);
                }
                RangeOutcome::Unsatisfiable => prop_assert!(headers.contains_key(RANGE)),
                _ => {}
            }
        }
    }
}
