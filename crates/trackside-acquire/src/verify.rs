//! Verification of staged files (architecture section 9.2) and safe library file names.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use trackside_core::BoxFuture;
use trackside_core::records::Container;
use trackside_media::{DecodeCheck, Media, MediaError, Probe};
use unicode_normalization::UnicodeNormalization;

use crate::rank::tolerance_ms;

/// What verification needs from media: properties and, when ffmpeg is present, a full decode.
/// [`Media`] implements it; tests use fakes.
pub trait AudioInspector: Send + Sync + std::fmt::Debug + 'static {
    fn probe<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Probe, MediaError>>;

    /// `None` when no decoder is available, in which case the probe has to do.
    fn decode_check<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, Result<Option<DecodeCheck>, MediaError>>;
}

impl AudioInspector for Media {
    fn probe<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Probe, MediaError>> {
        Box::pin(Media::probe(self, path))
    }

    fn decode_check<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, Result<Option<DecodeCheck>, MediaError>> {
        Box::pin(async move {
            if self.capabilities().ffmpeg.is_none() {
                return Ok(None);
            }
            match Media::decode_check(self, path).await {
                Ok(check) => Ok(Some(check)),
                Err(MediaError::NoTranscoder) => Ok(None),
                Err(e) => Err(e),
            }
        })
    }
}

/// What a verified file must match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expectation {
    pub duration_ms: Option<u32>,
    pub max_bytes: u64,
}

/// A staged file that passed verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub probe: Probe,
}

/// Why a staged file was rejected (architecture section 9.2).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Rejection {
    #[error("not an allowed audio file: {0}")]
    NotAudio(String),
    #[error("symlink")]
    Symlink,
    #[error("outside the staging directory")]
    OutsideStaging,
    #[error("has a video track")]
    VideoTrack,
    #[error("does not decode to the end")]
    Truncated,
    #[error("duration {found_ms} ms does not match {expected_ms} ms")]
    DurationMismatch { expected_ms: u32, found_ms: u32 },
    #[error("too large")]
    TooLarge,
    #[error(transparent)]
    Media(#[from] MediaError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

/// Checks a staged file: inside staging, a regular file, an allowed audio container with no
/// video, decodes fully, and has the expected duration and size.
pub async fn verify(
    path: &Path,
    staging_root: &Path,
    expected: &Expectation,
    media: &Media,
) -> Result<VerifiedFile, Rejection> {
    verify_with(path, staging_root, expected, media).await
}

/// [`verify`] with any [`AudioInspector`].
pub async fn verify_with(
    path: &Path,
    staging_root: &Path,
    expected: &Expectation,
    inspector: &dyn AudioInspector,
) -> Result<VerifiedFile, Rejection> {
    let (path, bytes, container) = {
        let path = path.to_path_buf();
        let root = staging_root.to_path_buf();
        let max = expected.max_bytes;
        tokio::task::spawn_blocking(move || check_file(&path, &root, max))
            .await
            .map_err(|e| Rejection::Io(std::io::Error::other(e)))??
    };
    let probe = inspector.probe(&path).await?;
    if probe.has_video {
        return Err(Rejection::VideoTrack);
    }
    if let Some(expected_ms) = expected.duration_ms
        && probe.duration_ms.abs_diff(expected_ms) > tolerance_ms(expected_ms)
    {
        return Err(Rejection::DurationMismatch {
            expected_ms,
            found_ms: probe.duration_ms,
        });
    }
    if let Some(check) = inspector.decode_check(&path).await?
        && !check.ok
    {
        return Err(Rejection::Truncated);
    }
    let probe = Probe {
        container: if probe.container == Container::Ogg && container == Container::Opus {
            Container::Opus
        } else {
            probe.container
        },
        ..probe
    };
    Ok(VerifiedFile { path, bytes, probe })
}

/// The synchronous checks: location, file type, size and magic bytes.
fn check_file(
    path: &Path,
    root: &Path,
    max_bytes: u64,
) -> Result<(PathBuf, u64, Container), Rejection> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(Rejection::Symlink);
    }
    let canonical = path.canonicalize()?;
    let root = root.canonicalize()?;
    if !canonical.starts_with(&root) || canonical == root {
        return Err(Rejection::OutsideStaging);
    }
    if !meta.is_file() {
        return Err(Rejection::NotAudio(String::from("not a regular file")));
    }
    if meta.len() > max_bytes {
        return Err(Rejection::TooLarge);
    }
    let mut file = File::open(&canonical)?;
    let mut head = [0u8; 64];
    let read = read_up_to(&mut file, &mut head)?;
    let container =
        sniff(&head[..read]).ok_or_else(|| Rejection::NotAudio(describe(&head[..read])))?;
    if container == Container::M4a && mp4_has_video(&mut file, meta.len())? {
        return Err(Rejection::VideoTrack);
    }
    Ok((canonical, meta.len(), container))
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// The container its first bytes say a file is, if it is one we accept.
pub fn sniff(head: &[u8]) -> Option<Container> {
    if head.starts_with(b"fLaC") {
        return Some(Container::Flac);
    }
    if head.starts_with(b"ID3") {
        return Some(Container::Mp3);
    }
    if head.len() >= 2 && head[0] == 0xFF && head[1] & 0xE0 == 0xE0 {
        // Bits 2-1 of the second byte are the MPEG layer; 00 there is ADTS AAC, not MP3.
        return (head[1] & 0x06 != 0).then_some(Container::Mp3);
    }
    if head.len() >= 8 && &head[4..8] == b"ftyp" {
        return Some(Container::M4a);
    }
    if head.starts_with(b"OggS") {
        if contains(head, b"OpusHead") {
            return Some(Container::Opus);
        }
        if contains(head, b"\x01vorbis") {
            return Some(Container::Ogg);
        }
        return None;
    }
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WAVE" {
        return Some(Container::Wav);
    }
    if head.len() >= 12 && head.starts_with(b"FORM") && matches!(&head[8..12], b"AIFF" | b"AIFC") {
        return Some(Container::Aiff);
    }
    None
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn describe(head: &[u8]) -> String {
    let kind = if head.starts_with(b"PK\x03\x04") {
        "an archive"
    } else if head.starts_with(b"\x7fELF") || head.starts_with(b"MZ") {
        "an executable"
    } else if head.starts_with(b"#!") {
        "a script"
    } else if head.is_empty() {
        "empty"
    } else {
        "unknown content"
    };
    kind.to_owned()
}

/// Walks MP4 boxes looking for a track whose handler is `vide`.
fn mp4_has_video(file: &mut File, len: u64) -> Result<bool, Rejection> {
    let Some((moov_start, moov_len)) = find_box(file, 0, len, *b"moov")? else {
        return Err(Rejection::NotAudio(String::from(
            "an MP4 without a movie box",
        )));
    };
    let mut at = moov_start;
    let end = moov_start + moov_len;
    while let Some((kind, body, size)) = box_header(file, at, end)? {
        if kind == *b"trak"
            && let Some((mdia, mdia_len)) = find_box(file, body, at + size - body, *b"mdia")?
            && let Some((hdlr, _)) = find_box(file, mdia, mdia_len, *b"hdlr")?
        {
            // hdlr: version and flags (4), pre_defined (4), then the handler type.
            let mut handler = [0u8; 4];
            file.seek(SeekFrom::Start(hdlr + 8))?;
            file.read_exact(&mut handler)?;
            if &handler == b"vide" {
                return Ok(true);
            }
        }
        at += size;
    }
    Ok(false)
}

/// The body offset and length of the first box of `kind` in `[start, start + len)`.
fn find_box(
    file: &mut File,
    start: u64,
    len: u64,
    kind: [u8; 4],
) -> Result<Option<(u64, u64)>, Rejection> {
    let end = start + len;
    let mut at = start;
    while let Some((found, body, size)) = box_header(file, at, end)? {
        if found == kind {
            return Ok(Some((body, at + size - body)));
        }
        at += size;
    }
    Ok(None)
}

/// The box at `at`: its kind, body offset and total size, or `None` at the end.
fn box_header(
    file: &mut File,
    at: u64,
    end: u64,
) -> Result<Option<([u8; 4], u64, u64)>, Rejection> {
    if at + 8 > end {
        return Ok(None);
    }
    let mut header = [0u8; 8];
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(&mut header)?;
    let mut kind = [0u8; 4];
    kind.copy_from_slice(&header[4..8]);
    let (size, body) = match u32::from_be_bytes([header[0], header[1], header[2], header[3]]) {
        0 => (end - at, at + 8),
        1 => {
            let mut large = [0u8; 8];
            file.read_exact(&mut large)?;
            (u64::from_be_bytes(large), at + 16)
        }
        n => (u64::from(n), at + 8),
    };
    if size < body - at || at + size > end {
        return Err(Rejection::NotAudio(String::from("a malformed MP4")));
    }
    Ok(Some((kind, body, size)))
}

/// Values for a path template such as `{album_artist}/{album}/{disc}-{track:02} {title}.{ext}`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TagValues {
    pub artist: String,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub title: String,
    pub disc: Option<u32>,
    pub track: Option<u32>,
    pub year: Option<i32>,
    pub ext: String,
}

/// Longest path component, in bytes.
pub const MAX_COMPONENT_BYTES: usize = 180;

const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// A relative path from a template, with every component sanitised: no separators, NUL,
/// control characters, leading dots or reserved names, at most 180 bytes each.
pub fn safe_file_name(template: &str, tags: &TagValues) -> PathBuf {
    let parts: Vec<&str> = template
        .split(['/', '\\'])
        .filter(|p| !p.trim().is_empty())
        .collect();
    let mut path = PathBuf::new();
    let last = parts.len().saturating_sub(1);
    for (i, part) in parts.iter().enumerate() {
        let rendered = render(part, tags);
        let component = if i == last {
            file_component(&rendered, &sanitize_value(&tags.ext))
        } else {
            sanitize_component(&rendered, MAX_COMPONENT_BYTES)
        };
        path.push(component);
    }
    if parts.is_empty() {
        path.push(file_component(
            &sanitize_value(&tags.title),
            &sanitize_value(&tags.ext),
        ));
    }
    path
}

/// Fills in one template component. Values are cleaned of separators before they go in.
fn render(part: &str, tags: &TagValues) -> String {
    let mut out = String::new();
    let mut rest = part;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}').map(|i| open + i) else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let spec = &rest[open + 1..close];
        let (name, format) = spec.split_once(':').unwrap_or((spec, ""));
        let width: usize = format.parse().unwrap_or(0);
        let number = |n: Option<u32>| n.map(|n| format!("{n:0width$}")).unwrap_or_default();
        let value = match name {
            "artist" => sanitize_value(&tags.artist),
            "album_artist" => sanitize_value(tags.album_artist.as_deref().unwrap_or(&tags.artist)),
            "album" => sanitize_value(tags.album.as_deref().unwrap_or("")),
            "title" => sanitize_value(&tags.title),
            "disc" => number(tags.disc),
            "track" => number(tags.track),
            "year" => tags.year.map(|y| y.to_string()).unwrap_or_default(),
            // The extension goes on last, after the stem is cleaned.
            "ext" => String::from("\u{0}ext\u{0}"),
            _ => String::new(),
        };
        out.push_str(&value);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The final component: the stem cleaned and shortened so the extension always fits.
fn file_component(rendered: &str, ext: &str) -> String {
    let stem = rendered
        .replace("\u{0}ext\u{0}", "")
        .trim_end_matches(['.', ' '])
        .to_owned();
    let ext: String = ext
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    if ext.is_empty() {
        return sanitize_component(&stem, MAX_COMPONENT_BYTES);
    }
    let stem = sanitize_component(&stem, MAX_COMPONENT_BYTES - ext.len() - 1);
    format!("{stem}.{ext}")
}

/// Text from tags, with anything that could split or escape a path component replaced.
fn sanitize_value(value: &str) -> String {
    value
        .nfc()
        .map(|c| match c {
            '/' | '\\' => '-',
            c if c.is_control() => ' ',
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect()
}

fn sanitize_component(text: &str, max_bytes: usize) -> String {
    let cleaned = sanitize_value(text);
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed
        .trim_start_matches(['.', '-', ' ', '_'])
        .trim_end_matches(['.', '-', ' ', '_']);
    let mut out = truncate_bytes(trimmed, max_bytes)
        .trim_end_matches(['.', ' '])
        .to_owned();
    if out.is_empty() {
        out = String::from("Unknown");
    }
    let stem = out
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_lowercase();
    if RESERVED.contains(&stem.as_str()) {
        out.insert(0, '_');
        let keep = truncate_bytes(&out, max_bytes).len();
        out.truncate(keep);
    }
    out
}

fn truncate_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `path`, or `path (2)`, `path (3)`, ... for the first one `exists` says is free.
pub fn unique_path(path: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    if !exists(path) {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 2u32.. {
        let suffix = format!(" ({n})");
        let stem = truncate_bytes(
            &stem,
            MAX_COMPONENT_BYTES - suffix.len() - ext.as_ref().map_or(0, |e| e.len() + 1),
        );
        let name = match &ext {
            Some(ext) => format!("{stem}{suffix}.{ext}"),
            None => format!("{stem}{suffix}"),
        };
        let candidate = path.with_file_name(name);
        if !exists(&candidate) {
            return candidate;
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::path::Component;
    use trackside_core::records::Codec;

    #[derive(Debug)]
    struct FakeInspector {
        duration_ms: u32,
        has_video: bool,
        decodes: Option<bool>,
    }

    impl AudioInspector for FakeInspector {
        fn probe<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, Result<Probe, MediaError>> {
            Box::pin(async move {
                Ok(Probe {
                    container: Container::Flac,
                    codec: Codec::Flac,
                    duration_ms: self.duration_ms,
                    bitrate_kbps: None,
                    sample_rate: None,
                    bit_depth: None,
                    channels: None,
                    has_video: self.has_video,
                })
            })
        }

        fn decode_check<'a>(
            &'a self,
            _path: &'a Path,
        ) -> BoxFuture<'a, Result<Option<DecodeCheck>, MediaError>> {
            Box::pin(async move {
                Ok(self.decodes.map(|ok| DecodeCheck {
                    ok,
                    decoded_ms: None,
                    message: None,
                }))
            })
        }
    }

    fn inspector() -> FakeInspector {
        FakeInspector {
            duration_ms: 200_000,
            has_video: false,
            decodes: None,
        }
    }

    const EXPECT: Expectation = Expectation {
        duration_ms: Some(200_000),
        max_bytes: 1 << 20,
    };

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn mp4_box(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&kind);
        out.extend_from_slice(body);
        out
    }

    fn mp4(handlers: &[&[u8; 4]]) -> Vec<u8> {
        let mut moov = Vec::new();
        for handler in handlers {
            let mut hdlr = vec![0u8; 8];
            hdlr.extend_from_slice(*handler);
            hdlr.extend_from_slice(&[0u8; 12]);
            let mdia = mp4_box(*b"mdia", &mp4_box(*b"hdlr", &hdlr));
            moov.extend(mp4_box(*b"trak", &mdia));
        }
        let mut file = mp4_box(*b"ftyp", b"M4A \0\0\0\0isomM4A ");
        file.extend(mp4_box(*b"moov", &moov));
        file.extend(mp4_box(*b"mdat", &[0u8; 32]));
        file
    }

    fn samples() -> Vec<(&'static str, Vec<u8>, Container)> {
        let mut ogg_opus = b"OggS\0\x02".to_vec();
        ogg_opus.extend_from_slice(&[0u8; 22]);
        ogg_opus.extend_from_slice(b"OpusHead");
        let mut ogg_vorbis = b"OggS\0\x02".to_vec();
        ogg_vorbis.extend_from_slice(&[0u8; 22]);
        ogg_vorbis.extend_from_slice(b"\x01vorbis");
        vec![
            ("a.flac", b"fLaC\0\0\0\x22rest".to_vec(), Container::Flac),
            ("a.mp3", b"ID3\x04\0\0\0\0\0\0".to_vec(), Container::Mp3),
            ("b.mp3", vec![0xFF, 0xFB, 0x90, 0x00, 0, 0], Container::Mp3),
            ("a.m4a", mp4(&[b"soun"]), Container::M4a),
            ("a.opus", ogg_opus, Container::Opus),
            ("a.ogg", ogg_vorbis, Container::Ogg),
            ("a.wav", b"RIFF\0\0\0\0WAVEfmt ".to_vec(), Container::Wav),
            ("a.aiff", b"FORM\0\0\0\0AIFFCOMM".to_vec(), Container::Aiff),
        ]
    }

    #[test]
    fn sniff_knows_each_allowed_container() {
        for (name, bytes, container) in samples() {
            assert_eq!(sniff(&bytes), Some(container), "{name}");
        }
    }

    #[tokio::test]
    async fn accepts_each_allowed_container() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes, _) in samples() {
            let path = write(dir.path(), name, &bytes);
            let verified = verify_with(&path, dir.path(), &EXPECT, &inspector()).await;
            assert!(verified.is_ok(), "{name}: {verified:?}");
        }
    }

    #[tokio::test]
    async fn rejects_a_zip_an_elf_and_a_script() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("a.flac", b"PK\x03\x04zipzip".as_slice()),
            ("b.flac", b"\x7fELF\x02\x01\x01".as_slice()),
            ("c.mp3", b"#!/bin/sh\nrm -rf /".as_slice()),
            ("d.aac", [0xFF, 0xF1, 0x50, 0x80].as_slice()),
        ] {
            let path = write(dir.path(), name, bytes);
            let result = verify_with(&path, dir.path(), &EXPECT, &inspector()).await;
            assert!(
                matches!(result, Err(Rejection::NotAudio(_))),
                "{name}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = write(dir.path(), "real.flac", b"fLaC\0\0\0\x22");
        let link = dir.path().join("link.flac");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let result = verify_with(&link, dir.path(), &EXPECT, &inspector()).await;
        assert!(matches!(result, Err(Rejection::Symlink)));
    }

    #[tokio::test]
    async fn rejects_a_path_outside_staging() {
        let outer = tempfile::tempdir().unwrap();
        let staging = outer.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        let outside = write(outer.path(), "a.flac", b"fLaC\0\0\0\x22");
        let sneaky = staging.join("..").join("a.flac");
        for path in [outside, sneaky] {
            let result = verify_with(&path, &staging, &EXPECT, &inspector()).await;
            assert!(
                matches!(result, Err(Rejection::OutsideStaging)),
                "{result:?}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_an_mp4_with_a_video_track() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "a.m4a", &mp4(&[b"soun", b"vide"]));
        let result = verify_with(&path, dir.path(), &EXPECT, &inspector()).await;
        assert!(matches!(result, Err(Rejection::VideoTrack)));
    }

    #[tokio::test]
    async fn rejects_a_video_the_probe_reports() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "a.flac", b"fLaC\0\0\0\x22");
        let probe = FakeInspector {
            has_video: true,
            ..inspector()
        };
        let result = verify_with(&path, dir.path(), &EXPECT, &probe).await;
        assert!(matches!(result, Err(Rejection::VideoTrack)));
    }

    #[tokio::test]
    async fn rejects_a_duration_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "a.flac", b"fLaC\0\0\0\x22");
        let probe = FakeInspector {
            duration_ms: 240_000,
            ..inspector()
        };
        let result = verify_with(&path, dir.path(), &EXPECT, &probe).await;
        assert!(matches!(
            result,
            Err(Rejection::DurationMismatch {
                expected_ms: 200_000,
                found_ms: 240_000
            })
        ));
    }

    #[tokio::test]
    async fn rejects_a_file_over_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = b"fLaC".to_vec();
        bytes.resize(2 << 20, 0);
        let path = write(dir.path(), "a.flac", &bytes);
        let result = verify_with(&path, dir.path(), &EXPECT, &inspector()).await;
        assert!(matches!(result, Err(Rejection::TooLarge)));
    }

    #[tokio::test]
    async fn rejects_what_the_decoder_cannot_finish() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "a.flac", b"fLaC\0\0\0\x22");
        let probe = FakeInspector {
            decodes: Some(false),
            ..inspector()
        };
        let result = verify_with(&path, dir.path(), &EXPECT, &probe).await;
        assert!(matches!(result, Err(Rejection::Truncated)));
    }

    /// WP3's `Media` with real ffmpeg, or `None` (test skipped) without it.
    fn real_media(dir: &Path) -> Option<Media> {
        let ffmpeg = trackside_media::FfmpegTranscoder::detect(None)?;
        let clock = std::sync::Arc::new(trackside_core::TestClock::new(
            trackside_core::testing::TEST_START,
        ));
        let db =
            trackside_core::Db::open_in_memory(trackside_core::EventBus::default(), clock.clone())
                .unwrap();
        let config = trackside_core::Config::defaults(dir).media;
        Some(
            Media::new(
                &config,
                &dir.join("cache"),
                db,
                std::sync::Arc::new(ffmpeg),
                clock,
            )
            .unwrap(),
        )
    }

    /// Makes a file with ffmpeg from lavfi sources.
    fn make(path: &Path, args: &[&str]) {
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args(args)
            .arg(path)
            .status()
            .unwrap();
        assert!(made.success(), "ffmpeg could not make {}", path.display());
    }

    const SINE: [&str; 6] = [
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=4",
        "-ac",
        "2",
    ];

    #[tokio::test]
    async fn real_media_accepts_a_flac_and_rejects_it_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let Some(media) = real_media(dir.path()) else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let staging = dir.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        let path = staging.join("a.flac");
        make(&path, &SINE);
        let expect = Expectation {
            duration_ms: Some(4_000),
            max_bytes: 1 << 30,
        };
        let verified = verify(&path, &staging, &expect, &media).await.unwrap();
        assert_eq!(verified.probe.container, Container::Flac);
        assert!(verified.probe.duration_ms.abs_diff(4_000) < 100);

        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        let expect = Expectation {
            duration_ms: None,
            ..expect
        };
        let result = verify(&path, &staging, &expect, &media).await;
        assert!(matches!(result, Err(Rejection::Truncated)), "{result:?}");
    }

    #[tokio::test]
    async fn real_media_rejects_a_duration_mismatch_and_accepts_m4a() {
        let dir = tempfile::tempdir().unwrap();
        let Some(media) = real_media(dir.path()) else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let path = dir.path().join("a.m4a");
        make(
            &path,
            &[&SINE[..], &["-c:a", "aac", "-b:a", "128k"]].concat(),
        );
        let wrong = Expectation {
            duration_ms: Some(60_000),
            max_bytes: 1 << 30,
        };
        assert!(matches!(
            verify(&path, dir.path(), &wrong, &media).await,
            Err(Rejection::DurationMismatch {
                expected_ms: 60_000,
                ..
            })
        ));
        let right = Expectation {
            duration_ms: Some(4_000),
            ..wrong
        };
        let verified = verify(&path, dir.path(), &right, &media).await.unwrap();
        assert_eq!(verified.probe.container, Container::M4a);
    }

    #[tokio::test]
    async fn real_media_rejects_an_mp4_with_a_video_track() {
        let dir = tempfile::tempdir().unwrap();
        let Some(media) = real_media(dir.path()) else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let path = dir.path().join("a.m4a");
        make(
            &path,
            &[
                &SINE[..],
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=black:s=64x64:d=4",
                    "-c:a",
                    "aac",
                    "-c:v",
                    "mpeg4",
                    "-shortest",
                ],
            ]
            .concat(),
        );
        let expect = Expectation {
            duration_ms: Some(4_000),
            max_bytes: 1 << 30,
        };
        assert!(matches!(
            verify(&path, dir.path(), &expect, &media).await,
            Err(Rejection::VideoTrack)
        ));
    }

    fn tags(title: &str) -> TagValues {
        TagValues {
            artist: String::from("Beyoncé"),
            album_artist: None,
            album: Some(String::from("I Am... Sasha Fierce")),
            title: title.to_owned(),
            disc: Some(1),
            track: Some(3),
            year: Some(2008),
            ext: String::from("flac"),
        }
    }

    #[test]
    fn safe_file_name_fills_the_default_template() {
        let path = safe_file_name(
            "{album_artist}/{album}/{disc}-{track:02} {title}.{ext}",
            &tags("Halo"),
        );
        assert_eq!(
            path,
            PathBuf::from("Beyoncé/I Am... Sasha Fierce/1-03 Halo.flac")
        );
    }

    #[test]
    fn safe_file_name_neutralises_traversal_and_separators() {
        let mut values = tags("../../etc/passwd");
        values.album = Some(String::from(".."));
        values.artist = String::from("CON");
        let path = safe_file_name("{artist}/{album}/{title}.{ext}", &values);
        assert_eq!(path, PathBuf::from("_CON/Unknown/etc-passwd.flac"));
    }

    #[test]
    fn missing_numbers_leave_no_dangling_separators() {
        let mut values = tags("Halo");
        values.disc = None;
        values.track = None;
        let path = safe_file_name("{album}/{disc}-{track:02} {title}.{ext}", &values);
        assert_eq!(path, PathBuf::from("I Am... Sasha Fierce/Halo.flac"));
    }

    #[test]
    fn unique_path_adds_a_counter() {
        let taken = [PathBuf::from("a/b.flac"), PathBuf::from("a/b (2).flac")];
        let path = unique_path(Path::new("a/b.flac"), |p| taken.iter().any(|t| t == p));
        assert_eq!(path, PathBuf::from("a/b (3).flac"));
    }

    proptest! {
        #[test]
        fn safe_file_name_components_are_always_safe(
            template in "[{}a-z_:0-9/\\\\. -]{0,40}|\\{album_artist\\}/\\{album\\}/\\{disc\\}-\\{track:02\\} \\{title\\}.\\{ext\\}",
            artist in any::<String>(),
            album in proptest::option::of(any::<String>()),
            title in any::<String>(),
            ext in any::<String>(),
            disc in proptest::option::of(any::<u32>()),
            track in proptest::option::of(any::<u32>()),
        ) {
            let values = TagValues { artist, album_artist: None, album, title, disc, track, year: None, ext };
            let path = safe_file_name(&template, &values);
            prop_assert!(path.components().count() >= 1);
            for component in path.components() {
                let Component::Normal(name) = component else {
                    return Err(TestCaseError::fail(format!("not a normal component: {component:?}")));
                };
                let name = name.to_str().unwrap();
                prop_assert!(!name.is_empty());
                prop_assert!(name.len() <= MAX_COMPONENT_BYTES, "{} bytes", name.len());
                prop_assert!(!name.starts_with('.'));
                prop_assert!(!name.contains(['/', '\\', '\0']));
                prop_assert!(!name.chars().any(char::is_control));
                let stem = name.split('.').next().unwrap().trim_end().to_ascii_lowercase();
                prop_assert!(!RESERVED.contains(&stem.as_str()));
            }
        }
    }
}
