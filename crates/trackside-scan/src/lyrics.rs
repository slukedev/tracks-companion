//! Lyrics: embedded `USLT`, `SYLT` and `LYRICS` tags, and sidecar `.lrc` files.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use trackside_core::ingest::ScannedLyrics;
use trackside_core::records::{LyricsKind, LyricsSource};

use crate::paths::nfc;

/// The largest sidecar the scanner reads.
const MAX_SIDECAR_BYTES: u64 = 512 * 1024;

/// Whether `text` is LRC: at least one line starting with a `[mm:ss]` or `[mm:ss.xx]` stamp.
pub fn is_lrc(text: &str) -> bool {
    text.lines()
        .any(|line| parse_stamp(line.trim_start()).is_some())
}

fn parse_stamp(line: &str) -> Option<u32> {
    let rest = line.strip_prefix('[')?;
    let close = rest.find(']')?;
    let stamp = &rest[..close];
    let (minutes, seconds) = stamp.split_once(':')?;
    let minutes: u32 = minutes.parse().ok()?;
    let (whole, frac) = match seconds.split_once(['.', ':']) {
        Some((w, f)) => (w, Some(f)),
        None => (seconds, None),
    };
    let whole: u32 = whole.parse().ok()?;
    if whole >= 60 || minutes > 999 {
        return None;
    }
    let ms = match frac {
        Some(f) if !f.is_empty() && f.len() <= 3 && f.bytes().all(|b| b.is_ascii_digit()) => {
            let n: u32 = f.parse().ok()?;
            n * 10u32.pow(3 - f.len() as u32)
        }
        Some(_) => return None,
        None => 0,
    };
    Some(minutes * 60_000 + whole * 1000 + ms)
}

/// Text found in a tag, classified as synced (LRC) or plain.
pub fn from_text(text: &str, lang: Option<&str>, source: LyricsSource) -> Option<ScannedLyrics> {
    let content = clean(text)?;
    let kind = if is_lrc(&content) {
        LyricsKind::Synced
    } else {
        LyricsKind::Plain
    };
    Some(ScannedLyrics {
        kind,
        lang: language(lang),
        source,
        content,
    })
}

/// `SYLT` content (timestamps in milliseconds) as LRC text.
pub fn from_synced(lines: &[(u32, String)], lang: Option<&str>) -> Option<ScannedLyrics> {
    let mut out = String::new();
    for (ms, text) in lines {
        let minutes = ms / 60_000;
        let seconds = (ms / 1000) % 60;
        let hundredths = (ms % 1000) / 10;
        let text = text.trim_matches(['\r', '\n']);
        let _ = writeln!(out, "[{minutes:02}:{seconds:02}.{hundredths:02}]{text}");
    }
    let content = clean(&out)?;
    Some(ScannedLyrics {
        kind: LyricsKind::Synced,
        lang: language(lang),
        source: LyricsSource::Embedded,
        content,
    })
}

fn clean(text: &str) -> Option<String> {
    let text = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\0', "");
    let text = nfc(text.trim());
    (!text.is_empty()).then_some(text)
}

fn language(lang: Option<&str>) -> String {
    match lang.map(str::trim) {
        Some(l) if l.len() == 3 && l.bytes().all(|b| b.is_ascii_alphabetic()) => {
            let lower = l.to_ascii_lowercase();
            if lower == "xxx" {
                String::from("und")
            } else {
                lower
            }
        }
        _ => String::from("und"),
    }
}

/// The sidecar `.lrc` beside an audio file: `<stem>.lrc`, matched case-insensitively among the
/// folder's names when they are known.
pub fn sidecar_path(audio: &Path, folder_names: Option<&[Vec<u8>]>) -> Option<PathBuf> {
    let stem = audio.file_stem()?;
    let wanted = crate::paths::os_bytes(stem);
    match folder_names {
        Some(names) => names
            .iter()
            .find(|name| {
                name.len() == wanted.len() + 4
                    && name.starts_with(&wanted)
                    && name[wanted.len()..].eq_ignore_ascii_case(b".lrc")
            })
            .map(|name| audio.with_file_name(crate::paths::os_from_bytes(name))),
        None => {
            let candidate = audio.with_extension("lrc");
            candidate.is_file().then_some(candidate)
        }
    }
}

/// Reads a sidecar `.lrc` file. Missing, unreadable or oversized files give nothing.
pub fn read_sidecar(path: &Path) -> Option<ScannedLyrics> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SIDECAR_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    from_text(
        &String::from_utf8_lossy(&bytes),
        None,
        LyricsSource::Sidecar,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc_is_recognised_by_its_time_stamps() {
        assert!(is_lrc("[ar:Someone]\n[00:12.34]Hello\n[00:15.00]World"));
        assert!(is_lrc("[01:02]Line"));
        assert!(!is_lrc("Just words\nand more words"));
        assert!(!is_lrc("[Chorus]\nla la"));
        assert!(!is_lrc("[00:61.00]bad seconds"));
    }

    #[test]
    fn stamps_parse_to_milliseconds() {
        assert_eq!(parse_stamp("[01:02.5]x"), Some(62_500));
        assert_eq!(parse_stamp("[00:00.123]x"), Some(123));
        assert_eq!(parse_stamp("[10:00]x"), Some(600_000));
    }

    #[test]
    fn tag_text_is_classified_and_cleaned() {
        let synced = from_text(
            "\u{feff}[00:01.00]Hi\r\n",
            Some("ENG"),
            LyricsSource::Embedded,
        );
        assert_eq!(
            synced,
            Some(ScannedLyrics {
                kind: LyricsKind::Synced,
                lang: "eng".into(),
                source: LyricsSource::Embedded,
                content: "[00:01.00]Hi".into(),
            })
        );
        let plain = from_text("Words", Some("xxx"), LyricsSource::Embedded);
        assert_eq!(
            plain.map(|l| (l.kind, l.lang)),
            Some((LyricsKind::Plain, "und".into()))
        );
        assert_eq!(from_text("   ", None, LyricsSource::Embedded), None);
    }

    #[test]
    fn sylt_becomes_lrc() {
        let lyrics = from_synced(
            &[(1_500, "One".into()), (61_020, "Two".into())],
            Some("eng"),
        );
        assert_eq!(
            lyrics.map(|l| l.content),
            Some("[00:01.50]One\n[01:01.02]Two".into())
        );
    }

    #[test]
    fn sidecar_matches_the_stem_case_insensitively() {
        let names = vec![b"01 Song.flac".to_vec(), b"01 Song.LRC".to_vec()];
        let found = sidecar_path(Path::new("/m/a/01 Song.flac"), Some(&names));
        assert_eq!(found, Some(PathBuf::from("/m/a/01 Song.LRC")));
        let names = vec![b"01 Song.flac".to_vec(), b"02 Song.lrc".to_vec()];
        assert_eq!(
            sidecar_path(Path::new("/m/a/01 Song.flac"), Some(&names)),
            None
        );
    }
}
