//! Reading a file with lofty: strict first, then relaxed, then the file name. Mapping lofty's
//! generic tag to [`ScannedTags`] is pure and tested without files.

use std::fmt::Write as _;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use lofty::config::{ParseOptions, ParsingMode};
use lofty::error::FileParseError;
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::id3::v2::{Frame, Id3v2Tag, SynchronizedTextFrame, TimestampFormat};
use lofty::mp4::{Mp4Codec, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::picture::PictureType;
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag};

use trackside_core::ingest::{ScannedLyrics, ScannedProperties, ScannedTags};
use trackside_core::records::{Codec, Container, Explicit, LyricsSource, ReplayGain};

use crate::lyrics;
use crate::paths::{container_for_extension, extension, nfc, os_bytes, stem};
use crate::{ParsedFile, Picture, PictureKind, ReadError};

/// The artist recorded when a file names none.
pub const UNKNOWN_ARTIST: &str = "Unknown Artist";

/// What the walker already knows about a file's neighbours, so reading it needs no extra
/// directory lookups.
#[derive(Clone, Debug, Default)]
pub struct ReadHints {
    /// The sidecar `.lrc`, when the listing has one.
    pub lyrics_sidecar: Option<PathBuf>,
    /// Whether `lyrics_sidecar` came from a listing (true) or should be looked up (false).
    pub listing_known: bool,
}

/// [`crate::read_file`] with hints from the folder listing.
pub fn read_file_with(path: &Path, hints: &ReadHints) -> Result<ParsedFile, ReadError> {
    let name = path.file_name().map(os_bytes).unwrap_or_default();
    let ext = extension(&name);
    let (file_type, container) = detect(path, ext.as_deref())?;

    let mut problem = None;
    let loaded = match load(path, file_type, ParsingMode::Strict, true) {
        Ok(loaded) => loaded,
        Err(strict) => match load(path, file_type, ParsingMode::Relaxed, true) {
            Ok(loaded) => {
                tracing::debug!(path = %path.display(), error = %strict, "tags read in relaxed mode");
                loaded
            }
            Err(relaxed) => {
                problem = Some(format!("unreadable_tags: {relaxed}"));
                load(path, file_type, ParsingMode::Relaxed, false)
                    .map_err(|e| ReadError::Tags(e.to_string()))?
            }
        },
    };

    let properties = properties(&loaded, container)?;
    let tag = loaded
        .file
        .primary_tag()
        .or_else(|| loaded.file.first_tag());
    let file_stem = stem(&name);
    let (mut tags, missing) = map_tags(tag, &file_stem);
    if problem.is_none() {
        problem = missing;
    }
    if tags.track.is_none() {
        tags.track = leading_track_number(&file_stem);
    }

    let mut pictures = Vec::new();
    for tag in loaded.file.tags() {
        for picture in tag.pictures() {
            if picture.data().is_empty() {
                continue;
            }
            let kind = if picture.pic_type() == PictureType::CoverFront {
                PictureKind::FrontCover
            } else {
                PictureKind::Other
            };
            pictures.push(Picture {
                kind,
                mime: picture.mime_type().map(|m| m.as_str().to_owned()),
                bytes: picture.data().to_vec(),
            });
        }
    }

    let mut found = loaded.lyrics;
    if let Some(tag) = tag {
        for key in [ItemKey::Lyrics, ItemKey::UnsyncLyrics] {
            for text in tag.get_strings(key) {
                if let Some(l) = lyrics::from_text(text, None, LyricsSource::Embedded) {
                    push_lyrics(&mut found, l);
                }
            }
        }
    }
    let sidecar = if hints.listing_known {
        hints.lyrics_sidecar.clone()
    } else {
        lyrics::sidecar_path(path, None)
    };
    if let Some(l) = sidecar.as_deref().and_then(lyrics::read_sidecar) {
        push_lyrics(&mut found, l);
    }

    Ok(ParsedFile {
        tags,
        properties,
        pictures,
        lyrics: found,
        tag_problem: problem,
    })
}

fn push_lyrics(found: &mut Vec<ScannedLyrics>, l: ScannedLyrics) {
    if !found
        .iter()
        .any(|f| f.kind == l.kind && f.lang == l.lang && f.source == l.source)
    {
        found.push(l);
    }
}

/// The container a file really is, by content first and then by extension.
pub fn detect(path: &Path, ext: Option<&str>) -> Result<(FileType, Container), ReadError> {
    let file = File::open(path)?;
    let probe = Probe::new(BufReader::new(file)).guess_file_type()?;
    let by_ext = ext.and_then(container_for_extension);
    let file_type = match probe.file_type() {
        Some(ft) => ft,
        None => by_ext.map(file_type_for).ok_or(ReadError::Unsupported)?,
    };
    let container = match file_type {
        FileType::Flac => Container::Flac,
        FileType::Mp4 => Container::M4a,
        FileType::Mpeg => Container::Mp3,
        FileType::Aac => Container::Aac,
        FileType::Vorbis => Container::Ogg,
        FileType::Opus => Container::Opus,
        FileType::Wav => Container::Wav,
        FileType::Aiff => Container::Aiff,
        FileType::WavPack => Container::WavPack,
        FileType::Ape => Container::Ape,
        _ => return Err(ReadError::Unsupported),
    };
    Ok((file_type, container))
}

fn file_type_for(container: Container) -> FileType {
    match container {
        Container::Flac => FileType::Flac,
        Container::M4a => FileType::Mp4,
        Container::Mp3 => FileType::Mpeg,
        Container::Aac => FileType::Aac,
        Container::Ogg => FileType::Vorbis,
        Container::Opus => FileType::Opus,
        Container::Wav => FileType::Wav,
        Container::Aiff => FileType::Aiff,
        Container::WavPack => FileType::WavPack,
        Container::Ape => FileType::Ape,
    }
}

struct Loaded {
    file: TaggedFile,
    lyrics: Vec<ScannedLyrics>,
    mp4_codec: Option<Mp4Codec>,
}

fn load(
    path: &Path,
    file_type: FileType,
    mode: ParsingMode,
    tags: bool,
) -> Result<Loaded, FileParseError> {
    let options = ParseOptions::new()
        .parsing_mode(mode)
        .read_tags(tags)
        .read_cover_art(tags);
    let mut reader = BufReader::new(File::open(path)?);
    match file_type {
        FileType::Mpeg => {
            let file = MpegFile::read_from(&mut reader, options)?;
            let lyrics = file.id3v2().map(id3_lyrics).unwrap_or_default();
            Ok(Loaded {
                file: file.into(),
                lyrics,
                mp4_codec: None,
            })
        }
        FileType::Mp4 => {
            let file = Mp4File::read_from(&mut reader, options)?;
            let mp4_codec = file.properties().codec();
            Ok(Loaded {
                file: file.into(),
                lyrics: Vec::new(),
                mp4_codec,
            })
        }
        other => {
            let file = Probe::with_file_type(reader, other)
                .options(options)
                .read()?;
            Ok(Loaded {
                file,
                lyrics: Vec::new(),
                mp4_codec: None,
            })
        }
    }
}

/// `USLT` with its language, and `SYLT` converted to LRC.
fn id3_lyrics(tag: &Id3v2Tag) -> Vec<ScannedLyrics> {
    let mut out = Vec::new();
    for frame in tag {
        match frame {
            Frame::UnsynchronizedText(uslt) => {
                let lang = String::from_utf8_lossy(&uslt.language).into_owned();
                if let Some(l) =
                    lyrics::from_text(&uslt.content, Some(&lang), LyricsSource::Embedded)
                {
                    push_lyrics(&mut out, l);
                }
            }
            Frame::Binary(binary) if frame.id_str() == "SYLT" => {
                let Ok(sylt) = SynchronizedTextFrame::parse(&binary.data, frame.flags()) else {
                    continue;
                };
                if sylt.timestamp_format != TimestampFormat::MS {
                    continue;
                }
                let lang = String::from_utf8_lossy(&sylt.language).into_owned();
                if let Some(l) = lyrics::from_synced(&sylt.content, Some(&lang)) {
                    push_lyrics(&mut out, l);
                }
            }
            _ => {}
        }
    }
    out
}

fn properties(loaded: &Loaded, container: Container) -> Result<ScannedProperties, ReadError> {
    let p = loaded.file.properties();
    let duration_ms = u32::try_from(p.duration().as_millis()).unwrap_or(u32::MAX);
    let sample_rate = p.sample_rate().filter(|r| *r > 0);
    if duration_ms == 0 && sample_rate.is_none() {
        return Err(ReadError::NoAudio);
    }
    let codec = match loaded.file.file_type() {
        FileType::Flac => Codec::Flac,
        FileType::Mpeg => Codec::Mp3,
        FileType::Aac => Codec::Aac,
        FileType::Vorbis => Codec::Vorbis,
        FileType::Opus => Codec::Opus,
        FileType::Wav | FileType::Aiff => Codec::Pcm,
        FileType::WavPack => Codec::WavPack,
        FileType::Ape => Codec::Ape,
        FileType::Mp4 => match loaded.mp4_codec {
            Some(Mp4Codec::ALAC) => Codec::Alac,
            Some(Mp4Codec::MP3) => Codec::Mp3,
            Some(Mp4Codec::FLAC) => Codec::Flac,
            _ => Codec::Aac,
        },
        _ => return Err(ReadError::Unsupported),
    };
    let bitrate_kbps = p
        .audio_bitrate()
        .or_else(|| p.overall_bitrate())
        .filter(|b| *b > 0);
    Ok(ScannedProperties {
        duration_ms,
        bitrate_kbps,
        sample_rate,
        bit_depth: p.bit_depth().filter(|d| *d > 0),
        channels: p.channels().filter(|c| *c > 0),
        codec,
        container,
    })
}

/// Text from a tag: NUL-free, trimmed, NFC, `None` when empty.
pub fn text(value: &str) -> Option<String> {
    let cleaned = value.replace('\0', "");
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| nfc(trimmed))
}

fn get(tag: &Tag, key: ItemKey) -> Option<String> {
    tag.get_strings(key).find_map(text)
}

fn all(tag: &Tag, key: ItemKey) -> Vec<String> {
    tag.get_strings(key).filter_map(text).collect()
}

/// Maps a lofty tag to [`ScannedTags`]. The second value says why the file name had to
/// stand in for the title.
pub fn map_tags(tag: Option<&Tag>, file_stem: &str) -> (ScannedTags, Option<String>) {
    let fallback_title = || {
        let stripped = strip_track_prefix(file_stem);
        text(stripped)
            .or_else(|| text(file_stem))
            .unwrap_or_else(|| String::from("Untitled"))
    };
    let Some(tag) = tag else {
        let tags = ScannedTags {
            title: fallback_title(),
            artist: UNKNOWN_ARTIST.to_owned(),
            ..ScannedTags::default()
        };
        return (tags, Some(String::from("no_tags")));
    };

    let mut problem = None;
    let title = match get(tag, ItemKey::TrackTitle) {
        Some(t) => t,
        None => {
            problem = Some(String::from("missing_title"));
            fallback_title()
        }
    };
    let artists = all(tag, ItemKey::TrackArtist);
    let artist = if artists.is_empty() {
        UNKNOWN_ARTIST.to_owned()
    } else {
        artists.join(" & ")
    };
    let album_artists = all(tag, ItemKey::AlbumArtist);
    let album_artist = (!album_artists.is_empty()).then(|| album_artists.join(" & "));

    let recording = get(tag, ItemKey::RecordingDate).or_else(|| get(tag, ItemKey::Year));
    let release = get(tag, ItemKey::ReleaseDate);
    let release_date = recording.as_deref().or(release.as_deref()).and_then(date);
    let year = release_date
        .as_deref()
        .and_then(year_of)
        .or_else(|| tag.date().map(|d| i32::from(d.year)).filter(|y| *y > 0));
    let original_date = get(tag, ItemKey::OriginalReleaseDate)
        .as_deref()
        .and_then(date);

    let tags = ScannedTags {
        title,
        artist,
        album: get(tag, ItemKey::AlbumTitle),
        album_artist,
        track: tag.track().filter(|n| *n > 0).or_else(|| {
            get(tag, ItemKey::TrackNumber)
                .as_deref()
                .and_then(leading_number)
        }),
        disc: tag.disk().filter(|n| *n > 0).or_else(|| {
            get(tag, ItemKey::DiscNumber)
                .as_deref()
                .and_then(leading_number)
        }),
        disc_subtitle: get(tag, ItemKey::SetSubtitle),
        year,
        release_date,
        original_date,
        genres: genres(&all(tag, ItemKey::Genre)),
        compilation: get(tag, ItemKey::FlagCompilation)
            .as_deref()
            .is_some_and(truthy),
        release_type: get(tag, ItemKey::MusicBrainzReleaseType).map(|t| t.to_lowercase()),
        mbz_recording: get(tag, ItemKey::MusicBrainzRecordingId),
        mbz_track: get(tag, ItemKey::MusicBrainzTrackId),
        mbz_release: get(tag, ItemKey::MusicBrainzReleaseId),
        mbz_release_group: get(tag, ItemKey::MusicBrainzReleaseGroupId),
        mbz_artist: get(tag, ItemKey::MusicBrainzArtistId),
        mbz_album_artist: get(tag, ItemKey::MusicBrainzReleaseArtistId),
        isrc: get(tag, ItemKey::Isrc).map(|i| i.to_ascii_uppercase()),
        bpm: get(tag, ItemKey::IntegerBpm)
            .or_else(|| get(tag, ItemKey::Bpm))
            .as_deref()
            .and_then(bpm),
        comment: get(tag, ItemKey::Comment),
        explicit: get(tag, ItemKey::ParentalAdvisory)
            .as_deref()
            .and_then(explicit),
        replay_gain: replay_gain(tag),
        sort_title: get(tag, ItemKey::TrackTitleSortOrder),
        sort_artist: get(tag, ItemKey::TrackArtistSortOrder),
        sort_album: get(tag, ItemKey::AlbumTitleSortOrder),
    };
    (tags, problem)
}

/// `YYYY`, `YYYY-MM` or `YYYY-MM-DD` from a tagged date, as far as it is valid.
pub fn date(value: &str) -> Option<String> {
    let v = value.trim();
    let year = v
        .get(..4)
        .filter(|y| y.bytes().all(|b| b.is_ascii_digit()))?;
    if year == "0000" {
        return None;
    }
    let mut out = year.to_owned();
    let rest = &v[4..];
    let two = |s: &str, lo: u32, hi: u32| {
        s.get(..2)
            .filter(|d| d.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|d| d.parse::<u32>().ok())
            .filter(|n| (lo..=hi).contains(n))
    };
    if let Some(month_part) = rest.strip_prefix('-')
        && let Some(month) = two(month_part, 1, 12)
    {
        let _ = write!(out, "-{month:02}");
        if let Some(day_part) = month_part[2..].strip_prefix('-')
            && let Some(day) = two(day_part, 1, 31)
        {
            let _ = write!(out, "-{day:02}");
        }
    }
    Some(out)
}

fn year_of(date: &str) -> Option<i32> {
    date.get(..4)?.parse().ok()
}

/// The number at the start of `3`, `3/12` or `03 of 12`.
pub fn leading_number(value: &str) -> Option<u32> {
    let digits: String = value
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok().filter(|n| *n > 0)
}

/// A track number at the start of a file name: `01 Song`, `01 - Song`, `1-03 Song`.
pub fn leading_track_number(file_stem: &str) -> Option<u32> {
    let s = file_stem.trim_start();
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let rest = &s[digits.len()..];
    if let Some(after) = rest.strip_prefix('-') {
        let track: String = after.chars().take_while(char::is_ascii_digit).collect();
        if !track.is_empty()
            && track.len() <= 3
            && after[track.len()..].starts_with([' ', '.', '_'])
        {
            return track.parse().ok().filter(|n| *n > 0);
        }
    }
    if rest.is_empty() || !rest.starts_with([' ', '.', '-', '_']) {
        return None;
    }
    digits.parse().ok().filter(|n| *n > 0)
}

/// The file name with a leading track number removed: `01 - Song` gives `Song`.
pub fn strip_track_prefix(file_stem: &str) -> &str {
    if leading_track_number(file_stem).is_none() {
        return file_stem;
    }
    let s = file_stem.trim_start();
    let rest = s.trim_start_matches(|c: char| c.is_ascii_digit());
    let rest = match rest.strip_prefix('-') {
        Some(after) if after.starts_with(|c: char| c.is_ascii_digit()) => {
            after.trim_start_matches(|c: char| c.is_ascii_digit())
        }
        _ => rest,
    };
    let rest = rest.trim_start_matches([' ', '.', '-', '_']);
    if rest.is_empty() { file_stem } else { rest }
}

/// Genres split on `;` and deduplicated without regard to case.
pub fn genres(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        for part in value.split(';') {
            let Some(g) = text(part) else {
                continue;
            };
            if !out.iter().any(|o| o.to_lowercase() == g.to_lowercase()) {
                out.push(g);
            }
        }
    }
    out
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

/// iTunes advisory values (1 or 4 explicit, 2 clean) and their spelled-out forms.
pub fn explicit(value: &str) -> Option<Explicit> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "4" | "explicit" | "e" => Some(Explicit::Explicit),
        "2" | "clean" | "c" => Some(Explicit::Clean),
        _ => None,
    }
}

fn bpm(value: &str) -> Option<u32> {
    let n: f64 = value.trim().replace(',', ".").parse().ok()?;
    (n.is_finite() && n > 0.0 && n < 1000.0).then(|| n.round() as u32)
}

/// A gain such as `-6.54 dB` or a peak such as `0.98`.
pub fn gain_value(value: &str) -> Option<f64> {
    let v = value.trim();
    let v = v
        .strip_suffix("dB")
        .or_else(|| v.strip_suffix("db"))
        .or_else(|| v.strip_suffix("DB"))
        .unwrap_or(v)
        .trim();
    let n: f64 = v.replace(',', ".").parse().ok()?;
    n.is_finite().then_some(n)
}

fn replay_gain(tag: &Tag) -> Option<ReplayGain> {
    let gain = |key| get(tag, key).as_deref().and_then(gain_value);
    let rg = ReplayGain {
        track_gain: gain(ItemKey::ReplayGainTrackGain),
        track_peak: gain(ItemKey::ReplayGainTrackPeak),
        album_gain: gain(ItemKey::ReplayGainAlbumGain),
        album_peak: gain(ItemKey::ReplayGainAlbumPeak),
    };
    (rg != ReplayGain::default()).then_some(rg)
}

#[cfg(test)]
mod tests {
    use lofty::tag::{TagItem, TagType};

    use super::*;

    fn tag_with(items: &[(ItemKey, &str)]) -> Tag {
        let mut tag = Tag::new(TagType::VorbisComments);
        for (key, value) in items {
            tag.push_unchecked(TagItem::new(
                *key,
                lofty::tag::ItemValue::Text((*value).to_owned()),
            ));
        }
        tag
    }

    #[test]
    fn full_tags_map_to_every_field() {
        let tag = tag_with(&[
            (ItemKey::TrackTitle, " Halo "),
            (ItemKey::TrackArtist, "Beyonce\u{301}"),
            (ItemKey::AlbumTitle, "I Am... Sasha Fierce"),
            (ItemKey::AlbumArtist, "Beyonc\u{e9}"),
            (ItemKey::TrackNumber, "3"),
            (ItemKey::DiscNumber, "1"),
            (ItemKey::RecordingDate, "2008-11-12"),
            (ItemKey::OriginalReleaseDate, "2008"),
            (ItemKey::Genre, "Pop; R&B"),
            (ItemKey::Genre, "pop"),
            (ItemKey::FlagCompilation, "1"),
            (ItemKey::MusicBrainzTrackId, "mbt"),
            (ItemKey::MusicBrainzReleaseType, "Album"),
            (ItemKey::Isrc, "usqx90800001"),
            (ItemKey::IntegerBpm, "79.6"),
            (ItemKey::ParentalAdvisory, "2"),
            (ItemKey::ReplayGainTrackGain, "-6.54 dB"),
            (ItemKey::ReplayGainTrackPeak, "0.98"),
        ]);
        let (tags, problem) = map_tags(Some(&tag), "03 Halo");
        assert_eq!(problem, None);
        assert_eq!(tags.title, "Halo");
        assert_eq!(tags.artist, "Beyonc\u{e9}");
        assert_eq!(tags.album_artist.as_deref(), Some("Beyonc\u{e9}"));
        assert_eq!(tags.track, Some(3));
        assert_eq!(tags.disc, Some(1));
        assert_eq!(tags.year, Some(2008));
        assert_eq!(tags.release_date.as_deref(), Some("2008-11-12"));
        assert_eq!(tags.original_date.as_deref(), Some("2008"));
        assert_eq!(tags.genres, vec!["Pop".to_owned(), "R&B".to_owned()]);
        assert!(tags.compilation);
        assert_eq!(tags.mbz_track.as_deref(), Some("mbt"));
        assert_eq!(tags.release_type.as_deref(), Some("album"));
        assert_eq!(tags.isrc.as_deref(), Some("USQX90800001"));
        assert_eq!(tags.bpm, Some(80));
        assert_eq!(tags.explicit, Some(Explicit::Clean));
        let rg = tags.replay_gain.unwrap_or_default();
        assert_eq!(rg.track_gain, Some(-6.54));
        assert_eq!(rg.track_peak, Some(0.98));
        assert_eq!(rg.album_gain, None);
    }

    #[test]
    fn multiple_artists_are_joined_for_the_credit_parser() {
        let tag = tag_with(&[
            (ItemKey::TrackTitle, "T"),
            (ItemKey::TrackArtist, "A"),
            (ItemKey::TrackArtist, "B"),
        ]);
        assert_eq!(map_tags(Some(&tag), "x").0.artist, "A & B");
    }

    #[test]
    fn missing_tags_fall_back_to_the_file_name() {
        let (tags, problem) = map_tags(None, "07 - Some Song");
        assert_eq!(tags.title, "Some Song");
        assert_eq!(tags.artist, UNKNOWN_ARTIST);
        assert_eq!(problem.as_deref(), Some("no_tags"));
        let tag = tag_with(&[(ItemKey::TrackArtist, "Artist")]);
        let (tags, problem) = map_tags(Some(&tag), "Just A Name");
        assert_eq!(tags.title, "Just A Name");
        assert_eq!(tags.artist, "Artist");
        assert_eq!(problem.as_deref(), Some("missing_title"));
    }

    #[test]
    fn dates_keep_only_valid_parts() {
        assert_eq!(date("2008-11-12T00:00:00").as_deref(), Some("2008-11-12"));
        assert_eq!(date("2008-13-01").as_deref(), Some("2008"));
        assert_eq!(date("1999-05").as_deref(), Some("1999-05"));
        assert_eq!(date("0000"), None);
        assert_eq!(date("abc"), None);
    }

    #[test]
    fn track_numbers_from_file_names() {
        assert_eq!(leading_track_number("01 Song"), Some(1));
        assert_eq!(leading_track_number("12. Song"), Some(12));
        assert_eq!(leading_track_number("1-03 Song"), Some(3));
        assert_eq!(leading_track_number("1999 Song"), None);
        assert_eq!(leading_track_number("99 Luftballons"), Some(99));
        assert_eq!(leading_track_number("Song"), None);
        assert_eq!(strip_track_prefix("1-03 Song"), "Song");
        assert_eq!(strip_track_prefix("01 - Song"), "Song");
        assert_eq!(strip_track_prefix("Song"), "Song");
        assert_eq!(strip_track_prefix("01"), "01");
    }

    #[test]
    fn numbers_and_flags_parse_leniently() {
        assert_eq!(leading_number("3/12"), Some(3));
        assert_eq!(leading_number("0"), None);
        assert_eq!(explicit("4"), Some(Explicit::Explicit));
        assert_eq!(explicit("0"), None);
        assert_eq!(gain_value("+1,5 dB"), Some(1.5));
        assert_eq!(gain_value("loud"), None);
        assert_eq!(bpm("nan"), None);
    }

    #[test]
    fn text_is_trimmed_nul_free_and_nfc() {
        assert_eq!(text(" a\0b "), Some("ab".to_owned()));
        assert_eq!(text("  "), None);
        assert_eq!(text("e\u{301}"), Some("\u{e9}".to_owned()));
    }
}
