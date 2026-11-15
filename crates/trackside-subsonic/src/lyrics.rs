//! LRC parsing for `getLyricsBySongId` and plain text for `getLyrics`.

/// One lyrics line; `start` in milliseconds for synced lyrics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub start: Option<u64>,
    pub value: String,
}

/// Parsed lyrics: lines in time order when synced, and the `[offset:]` tag in milliseconds.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Parsed {
    pub lines: Vec<Line>,
    pub offset_ms: i64,
}

/// `mm:ss`, `mm:ss.xx` or `mm:ss.xxx` in milliseconds.
fn timestamp(tag: &str) -> Option<u64> {
    let (minutes, rest) = tag.split_once(':')?;
    let minutes: u64 = minutes.trim().parse().ok()?;
    let (secs, fraction) = rest.split_once(['.', ':']).unwrap_or((rest, ""));
    let secs: u64 = secs.trim().parse().ok()?;
    let fraction_ms = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()? * 100,
        2 => fraction.parse::<u64>().ok()? * 10,
        _ => fraction.get(..3)?.parse::<u64>().ok()?,
    };
    minutes
        .checked_mul(60_000)?
        .checked_add(secs.checked_mul(1000)?)?
        .checked_add(fraction_ms)
}

/// Parses LRC text. A line may carry several timestamps (`[00:12.00][01:30.00]chorus`);
/// metadata tags other than `offset` are dropped.
pub fn parse_lrc(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut starts = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some((tag, after)) = stripped.split_once(']') else {
                break;
            };
            if let Some(at) = timestamp(tag) {
                starts.push(at);
            } else if let Some(offset) = tag.strip_prefix("offset:") {
                parsed.offset_ms = offset.trim().parse().unwrap_or(0);
            }
            rest = after;
        }
        for start in starts {
            parsed.lines.push(Line {
                start: Some(start),
                value: rest.trim().to_owned(),
            });
        }
    }
    parsed.lines.sort_by_key(|l| l.start);
    parsed
}

/// Plain lines, one per text line.
pub fn plain_lines(text: &str) -> Vec<Line> {
    text.lines()
        .map(|l| Line {
            start: None,
            value: l.trim_end().to_owned(),
        })
        .collect()
}

/// Synced lyrics as plain text, for `getLyrics`.
pub fn lrc_to_plain(text: &str) -> String {
    parse_lrc(text)
        .lines
        .into_iter()
        .map(|l| l.value)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc_lines_with_several_times_and_an_offset() {
        let parsed = parse_lrc(
            "[ar:Someone]\n[offset:-250]\n[00:12.50][01:02.003]Chorus\n[00:05.1]Intro\nno time here\n",
        );
        assert_eq!(parsed.offset_ms, -250);
        let starts: Vec<_> = parsed.lines.iter().map(|l| l.start).collect();
        assert_eq!(starts, vec![Some(5100), Some(12_500), Some(62_003)]);
        assert_eq!(parsed.lines[1].value, "Chorus");
    }

    #[test]
    fn synced_lyrics_become_plain_text() {
        assert_eq!(lrc_to_plain("[00:01.00]One\n[00:02.00]Two"), "One\nTwo");
    }

    #[test]
    fn bad_timestamps_are_not_lines() {
        assert_eq!(parse_lrc("[xx:yy]Nope").lines, Vec::new());
    }

    #[test]
    fn huge_timestamps_are_refused_rather_than_overflowing() {
        assert_eq!(timestamp("18446744073709551615:00"), None);
        assert_eq!(timestamp("00:18446744073709551615"), None);
    }
}
