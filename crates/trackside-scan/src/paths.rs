//! Path policies: raw byte paths, display names, extension matching, ignore markers and the
//! file names the importer builds from tags.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use trackside_core::records::Container;

/// The exact bytes of a path, as the OS gives them. Stored this way so NFD names from macOS
/// shares and names that are not UTF-8 round-trip exactly.
#[cfg(unix)]
pub fn os_bytes(s: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}

#[cfg(not(unix))]
pub fn os_bytes(s: &OsStr) -> Vec<u8> {
    s.to_string_lossy().as_bytes().to_vec()
}

/// The inverse of [`os_bytes`].
#[cfg(unix)]
pub fn os_from_bytes(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
pub fn os_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// The absolute path of `rel` (raw bytes, `/`-separated) under `root`.
pub fn join_rel(root: &Path, rel: &[u8]) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(os_from_bytes(rel))
    }
}

/// `parent/name` as raw bytes; the root has an empty relative path.
pub fn child_rel(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        name.to_vec()
    } else {
        let mut rel = Vec::with_capacity(parent.len() + 1 + name.len());
        rel.extend_from_slice(parent);
        rel.push(b'/');
        rel.extend_from_slice(name);
        rel
    }
}

/// The folder part of a relative file path.
pub fn parent_rel(rel: &[u8]) -> &[u8] {
    match rel.iter().rposition(|b| *b == b'/') {
        Some(i) => &rel[..i],
        None => &[],
    }
}

/// The last component of a relative path.
pub fn file_name(rel: &[u8]) -> &[u8] {
    match rel.iter().rposition(|b| *b == b'/') {
        Some(i) => &rel[i + 1..],
        None => rel,
    }
}

/// `rel` of `path` under `root`, as raw bytes, or `None` when it is outside the root.
pub fn rel_of(root: &Path, path: &Path) -> Option<Vec<u8>> {
    let rest = path.strip_prefix(root).ok()?;
    let mut rel = Vec::new();
    for part in rest.components() {
        match part {
            std::path::Component::Normal(name) => {
                if !rel.is_empty() {
                    rel.push(b'/');
                }
                rel.extend_from_slice(&os_bytes(name));
            }
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(rel)
}

/// Text for logs and the dashboard: lossy UTF-8, NFC. Never used to find a file again.
pub fn display(bytes: &[u8]) -> String {
    nfc(&String::from_utf8_lossy(bytes))
}

/// NFC-normalised text.
pub fn nfc(s: &str) -> String {
    if unicode_normalization::is_nfc(s) {
        s.to_owned()
    } else {
        s.nfc().collect()
    }
}

/// The lowercase extension of a file name, if any.
pub fn extension(name: &[u8]) -> Option<String> {
    let dot = name.iter().rposition(|b| *b == b'.')?;
    if dot == 0 {
        return None;
    }
    let ext = std::str::from_utf8(&name[dot + 1..]).ok()?;
    (!ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// The file name without its extension, as text.
pub fn stem(name: &[u8]) -> String {
    let base = match name.iter().rposition(|b| *b == b'.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    };
    display(base)
}

/// Which extensions the scanner reads, compared case-insensitively.
#[derive(Clone, Debug)]
pub struct ExtensionSet(Vec<String>);

impl ExtensionSet {
    pub fn new(extensions: &[String]) -> Self {
        Self(
            extensions
                .iter()
                .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
        )
    }

    pub fn matches(&self, name: &[u8]) -> bool {
        extension(name).is_some_and(|ext| self.0.contains(&ext))
    }
}

/// The container a file name suggests, used when content sniffing is inconclusive.
pub fn container_for_extension(ext: &str) -> Option<Container> {
    Some(match ext {
        "flac" => Container::Flac,
        "m4a" | "mp4" | "alac" | "m4b" => Container::M4a,
        "mp3" => Container::Mp3,
        "aac" => Container::Aac,
        "ogg" | "oga" => Container::Ogg,
        "opus" => Container::Opus,
        "wav" => Container::Wav,
        "aif" | "aiff" | "aifc" => Container::Aiff,
        "wv" => Container::WavPack,
        "ape" => Container::Ape,
        _ => return None,
    })
}

/// Whether a folder listing contains one of the ignore markers.
pub fn has_ignore_marker(names: &[&[u8]], markers: &[String]) -> bool {
    names
        .iter()
        .any(|name| markers.iter().any(|m| m.as_bytes() == *name))
}

/// Names skipped while walking: dot files and folders, and the importer's rejected folder.
pub fn is_hidden(name: &[u8]) -> bool {
    name.first() == Some(&b'.')
}

const RESERVED_WINDOWS: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The longest a single path component may be, in bytes.
pub const MAX_COMPONENT_BYTES: usize = 180;

/// A safe path component built from tag text: NFC, no separators, NUL or control characters,
/// no leading dots, no trailing dots or spaces, not a Windows reserved name, at most
/// [`MAX_COMPONENT_BYTES`] bytes. Never empty.
pub fn sanitize_component(text: &str) -> String {
    let cleaned: String = nfc(text)
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim_start();
    let mut out = truncate_bytes(trimmed, MAX_COMPONENT_BYTES)
        .trim_end_matches(['.', ' '])
        .to_owned();
    if out.is_empty() {
        out.push('_');
    }
    let upper = out.to_ascii_uppercase();
    let base = upper.split('.').next().unwrap_or_default();
    if RESERVED_WINDOWS.contains(&base) {
        out.insert(0, '_');
    }
    out
}

fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The values a path template can use.
#[derive(Clone, Debug, Default)]
pub struct TemplateValues {
    pub artist: String,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub title: String,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub year: Option<i32>,
    pub ext: String,
}

/// Fills a path template such as `{album_artist}/{album}/{disc}-{track:02} {title}.{ext}`.
/// Each `/`-separated part is sanitised on its own, so tag text can never add a directory.
pub fn render_template(template: &str, values: &TemplateValues) -> PathBuf {
    let mut path = PathBuf::new();
    for part in template.split('/').filter(|p| !p.is_empty()) {
        let rendered = render_part(part, values);
        path.push(sanitize_component(&rendered));
    }
    if path.as_os_str().is_empty() {
        path.push(sanitize_component(&format!(
            "{}.{}",
            values.title, values.ext
        )));
    }
    path
}

fn render_part(part: &str, values: &TemplateValues) -> String {
    let mut out = String::new();
    let mut rest = part;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let spec = &after[..close];
        let (name, width) = match spec.split_once(':') {
            Some((name, width)) => (name, width.parse::<usize>().ok()),
            None => (spec, None),
        };
        out.push_str(&placeholder(name, width, values));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

fn placeholder(name: &str, width: Option<usize>, values: &TemplateValues) -> String {
    let number = |n: Option<u32>, default: u32| {
        let n = n.unwrap_or(default);
        match width {
            Some(w) => format!("{n:0w$}"),
            None => n.to_string(),
        }
    };
    match name {
        "artist" => values.artist.clone(),
        "album_artist" => values
            .album_artist
            .clone()
            .filter(|a| !a.trim().is_empty())
            .unwrap_or_else(|| values.artist.clone()),
        "album" => values
            .album
            .clone()
            .filter(|a| !a.trim().is_empty())
            .unwrap_or_else(|| String::from("Singles")),
        "title" => values.title.clone(),
        "track" => number(values.track, 0),
        "disc" => number(values.disc, 1),
        "year" => values.year.map(|y| y.to_string()).unwrap_or_default(),
        "ext" => values.ext.clone(),
        other => format!("{{{other}}}"),
    }
}

/// `path`, or `name (2).ext`, `name (3).ext`... for the first that `taken` says is free.
pub fn with_collision_suffix(path: &Path, taken: impl Fn(&Path) -> bool) -> PathBuf {
    if !taken(path) {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 2u32.. {
        let name = match &ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = path.with_file_name(name);
        if !taken(&candidate) {
            return candidate;
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_are_raw_bytes_joined_with_slashes() {
        assert_eq!(child_rel(b"", b"a"), b"a");
        assert_eq!(child_rel(b"a", b"b"), b"a/b");
        assert_eq!(parent_rel(b"a/b/c.flac"), b"a/b");
        assert_eq!(parent_rel(b"c.flac"), b"");
        assert_eq!(file_name(b"a/b/c.flac"), b"c.flac");
    }

    #[test]
    fn rel_of_rejects_paths_outside_the_root() {
        let root = Path::new("/music");
        assert_eq!(
            rel_of(root, Path::new("/music/A/b.flac")),
            Some(b"A/b.flac".to_vec())
        );
        assert_eq!(rel_of(root, Path::new("/elsewhere/b.flac")), None);
        assert_eq!(rel_of(root, Path::new("/music")), Some(Vec::new()));
    }

    #[cfg(unix)]
    #[test]
    fn nfd_names_round_trip_exactly_and_display_as_nfc() {
        let nfd = "Beyonce\u{301}".as_bytes();
        let os = os_from_bytes(nfd);
        assert_eq!(os_bytes(&os), nfd);
        assert_eq!(display(nfd), "Beyonc\u{e9}");
    }

    #[cfg(unix)]
    #[test]
    fn names_that_are_not_utf8_survive_the_round_trip() {
        let bytes = b"caf\xe9.mp3";
        assert_eq!(os_bytes(&os_from_bytes(bytes)), bytes);
        assert_eq!(display(bytes), "caf\u{fffd}.mp3");
    }

    #[test]
    fn extensions_match_case_insensitively() {
        let set = ExtensionSet::new(&["flac".into(), ".MP3".into()]);
        assert!(set.matches(b"01 Song.FLAC"));
        assert!(set.matches(b"x.mp3"));
        assert!(!set.matches(b"cover.jpg"));
        assert!(!set.matches(b".flac"));
        assert!(!set.matches(b"flac"));
    }

    #[test]
    fn sanitize_removes_separators_dots_and_reserved_names() {
        assert_eq!(sanitize_component("AC/DC"), "AC_DC");
        assert_eq!(sanitize_component("..hidden"), "hidden");
        assert_eq!(sanitize_component("trailing. "), "trailing");
        assert_eq!(sanitize_component("CON"), "_CON");
        assert_eq!(sanitize_component("con.flac"), "_con.flac");
        assert_eq!(sanitize_component("a\u{0}b\nc"), "a_b_c");
        assert_eq!(sanitize_component(""), "_");
        assert_eq!(sanitize_component("Beyonce\u{301}"), "Beyonc\u{e9}");
    }

    #[test]
    fn sanitize_truncates_on_a_character_boundary() {
        let long = "\u{e9}".repeat(200);
        let out = sanitize_component(&long);
        assert!(out.len() <= MAX_COMPONENT_BYTES);
        assert!(out.chars().all(|c| c == '\u{e9}'));
    }

    #[test]
    fn template_fills_and_pads_and_cannot_escape() {
        let values = TemplateValues {
            artist: "Artist".into(),
            album_artist: Some("../../etc".into()),
            album: Some("Album: Deluxe".into()),
            title: "Song/Name".into(),
            track: Some(3),
            disc: None,
            year: Some(2020),
            ext: "flac".into(),
        };
        let path = render_template(
            "{album_artist}/{album}/{disc}-{track:02} {title}.{ext}",
            &values,
        );
        assert_eq!(
            path,
            PathBuf::from("_.._etc/Album_ Deluxe/1-03 Song_Name.flac")
        );
        assert!(
            path.components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
        );
    }

    #[test]
    fn template_without_album_uses_singles() {
        let values = TemplateValues {
            artist: "A".into(),
            title: "T".into(),
            ext: "mp3".into(),
            ..TemplateValues::default()
        };
        let path = render_template("{album_artist}/{album}/{title}.{ext}", &values);
        assert_eq!(path, PathBuf::from("A/Singles/T.mp3"));
    }

    #[test]
    fn collisions_get_a_numbered_suffix() {
        let taken = [PathBuf::from("a/b.flac"), PathBuf::from("a/b (2).flac")];
        let free = with_collision_suffix(Path::new("a/b.flac"), |p| taken.iter().any(|t| t == p));
        assert_eq!(free, PathBuf::from("a/b (3).flac"));
    }

    #[test]
    fn ignore_markers_are_exact_names() {
        let markers = vec![".nomedia".to_owned()];
        assert!(has_ignore_marker(&[b"a.flac", b".nomedia"], &markers));
        assert!(!has_ignore_marker(&[b"x.nomedia"], &markers));
    }
}
