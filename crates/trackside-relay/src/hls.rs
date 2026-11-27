//! Live HLS playlists for one epoch: a multivariant playlist with its single variant, so the
//! player knows the codec before it loads anything, and a sliding media playlist.

use std::fmt::Write as _;

/// The playlist's `EXT-X-TARGETDURATION`. Segments are cut to stay at or under 1.4 s, so every
/// `EXTINF` rounds to at most this.
pub const TARGET_DURATION: u32 = 1;

/// A segment as the playlist lists it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    pub sequence: u64,
    pub duration_secs: f64,
    /// Wall-clock time of the segment's first sample on Music Assistant's timeline, Unix ms.
    pub program_date_ms: Option<i64>,
}

fn suffix(query: Option<&str>) -> String {
    query.map_or_else(String::new, |q| format!("?{q}"))
}

/// The multivariant playlist. `query` (the media token) goes on the media playlist URL.
pub fn multivariant(codecs: &str, bandwidth: u32, query: Option<&str>) -> String {
    format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n\
         #EXT-X-STREAM-INF:BANDWIDTH={bandwidth},CODECS=\"{codecs}\"\nindex.m3u8{}\n",
        suffix(query)
    )
}

/// The live media playlist. Without entries it lists nothing yet, which players re-poll.
pub fn media(entries: &[Entry], next_sequence: u64, ended: bool, query: Option<&str>) -> String {
    let q = suffix(query);
    let first = entries.first().map_or(next_sequence, |e| e.sequence);
    let mut out = String::with_capacity(128 + entries.len() * 80);
    let _ = write!(
        out,
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{TARGET_DURATION}\n\
         #EXT-X-MEDIA-SEQUENCE:{first}\n#EXT-X-INDEPENDENT-SEGMENTS\n\
         #EXT-X-START:TIME-OFFSET=0,PRECISE=YES\n#EXT-X-MAP:URI=\"init.mp4{q}\"\n"
    );
    for entry in entries {
        if let Some(ms) = entry.program_date_ms {
            let _ = writeln!(out, "#EXT-X-PROGRAM-DATE-TIME:{}", rfc3339_ms(ms));
        }
        let _ = write!(
            out,
            "#EXTINF:{:.3},\n{}.m4s{q}\n",
            entry.duration_secs, entry.sequence
        );
    }
    if ended {
        out.push_str("#EXT-X-ENDLIST\n");
    }
    out
}

/// Unix milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn rfc3339_ms(unix_ms: i64) -> String {
    let days = unix_ms.div_euclid(86_400_000);
    let ms_of_day = unix_ms.rem_euclid(86_400_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        ms_of_day / 3_600_000,
        ms_of_day / 60_000 % 60,
        ms_of_day / 1000 % 60,
        ms_of_day % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_format_as_rfc3339_with_milliseconds() {
        assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_ms(1_763_112_600_123), "2025-11-14T09:30:00.123Z");
        assert_eq!(rfc3339_ms(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn the_multivariant_playlist_names_the_codec() {
        assert_eq!(
            multivariant("fLaC", 1_411_200, Some("mt=abc")),
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n\
             #EXT-X-STREAM-INF:BANDWIDTH=1411200,CODECS=\"fLaC\"\nindex.m3u8?mt=abc\n"
        );
    }

    #[test]
    fn the_media_playlist_slides_and_ends() {
        let entries = [
            Entry {
                sequence: 4,
                duration_secs: 1.0449,
                program_date_ms: Some(0),
            },
            Entry {
                sequence: 5,
                duration_secs: 0.5,
                program_date_ms: None,
            },
        ];
        assert_eq!(
            media(&entries, 6, true, Some("mt=t")),
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:4\n\
             #EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-START:TIME-OFFSET=0,PRECISE=YES\n\
             #EXT-X-MAP:URI=\"init.mp4?mt=t\"\n\
             #EXT-X-PROGRAM-DATE-TIME:1970-01-01T00:00:00.000Z\n#EXTINF:1.045,\n4.m4s?mt=t\n\
             #EXTINF:0.500,\n5.m4s?mt=t\n#EXT-X-ENDLIST\n"
        );
    }

    #[test]
    fn an_empty_live_playlist_has_no_endlist() {
        let text = media(&[], 0, false, None);
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
        assert!(!text.contains("ENDLIST"));
        assert!(text.contains("URI=\"init.mp4\""));
    }
}
