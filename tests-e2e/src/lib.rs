//! A black-box harness for the `trackside` binary: start it against a temporary data
//! directory, drive it over HTTP the way Tracks and Subsonic apps do, and generate music
//! libraries of real audio files to scan.
//!
//! The scenarios live in `tests/`, the performance harness in `src/main.rs`.

pub mod client;
pub mod fakes;
pub mod library;
pub mod perf;
pub mod server;

pub use client::{Auth, Client, Reply};
pub use server::{Server, ServerSpec, binary};

/// Installs ring as the TLS provider once, which reqwest's `rustls-no-provider` needs before
/// any client is built.
pub fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Milliseconds since the Unix epoch, now.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// RFC 3339 UTC with milliseconds, the format of the API's times.
#[expect(
    clippy::many_single_char_names,
    reason = "the civil-from-days algorithm's own names"
)]
pub fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(super::rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            super::rfc3339(1_794_705_199_893),
            "2026-11-15T01:13:19.893Z"
        );
        assert_eq!(super::rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }
}
