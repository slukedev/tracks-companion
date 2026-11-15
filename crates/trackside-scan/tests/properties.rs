//! Property tests over hostile input: tag reading on arbitrary bytes, path components built
//! from tag text, relative paths from clients, and lyrics text.

use std::path::{Component, Path, PathBuf};

use proptest::prelude::*;
use trackside_core::records::LyricsSource;
use trackside_scan::lyrics::from_text;
use trackside_scan::paths::{MAX_COMPONENT_BYTES, sanitize_component};
use trackside_scan::safe_rel;

const EXTENSIONS: &[&str] = &[
    "mp3", "flac", "m4a", "ogg", "opus", "wav", "aiff", "wv", "ape", "aac",
];
const MAGIC: &[&[u8]] = &[
    b"",
    b"ID3\x03\x00\x00\x00\x00\x00\x0a",
    b"\xff\xfb\x90\x64",
    b"fLaC\x00\x00\x00\x22",
    b"OggS\x00\x02\x00\x00",
    b"RIFF\x24\x00\x00\x00WAVEfmt ",
    b"FORM\x00\x00\x00\x10AIFFCOMM",
    b"\x00\x00\x00\x18ftypM4A ",
    b"wvpk\x10\x00\x00\x00",
    b"MAC \x96\x0f\x00\x00",
];

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// The scanner reads whatever a library holds; corrupt files end as errors (or a song
    /// with a tag problem), never as a panic or a hang.
    #[test]
    fn tag_reading_survives_arbitrary_bytes(
        magic in 0usize..MAGIC.len(),
        ext in 0usize..EXTENSIONS.len(),
        body in proptest::collection::vec(any::<u8>(), 0..4096),
    ) {
        let dir = tempfile::tempdir().map_err(|e| TestCaseError::fail(e.to_string()))?;
        let path = dir.path().join(format!("file.{}", EXTENSIONS[ext]));
        let mut bytes = MAGIC[magic].to_vec();
        bytes.extend_from_slice(&body);
        std::fs::write(&path, &bytes).map_err(|e| TestCaseError::fail(e.to_string()))?;
        if let Ok(parsed) = trackside_scan::read_file(&path) {
            prop_assert!(!parsed.tags.title.is_empty() || parsed.tag_problem.is_some());
        }
    }

    #[test]
    fn components_from_tag_text_are_always_safe(text in any::<String>()) {
        let c = sanitize_component(&text);
        prop_assert!(!c.is_empty());
        prop_assert!(c.len() <= MAX_COMPONENT_BYTES + 1);
        prop_assert!(!c.starts_with('.'));
        prop_assert!(!c.ends_with(' ') && !c.ends_with('.'));
        prop_assert!(!c.contains(['/', '\\', '\0', ':']));
        prop_assert!(!c.chars().any(char::is_control));
        let only: Vec<Component<'_>> = Path::new(&c).components().collect();
        prop_assert_eq!(only.len(), 1);
        prop_assert!(matches!(only[0], Component::Normal(_)));
    }

    /// A relative path from a client (an import or a folder scan) never climbs out.
    #[test]
    fn relative_paths_never_leave_the_library(parts in proptest::collection::vec("[a-z]{1,4}|\\.\\.|\\.|/|", 0..8)) {
        let joined: PathBuf = parts.iter().collect();
        match safe_rel(&joined) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                prop_assert!(!text.starts_with('/'));
                prop_assert!(!text.split('/').any(|p| p == ".." || p.is_empty()), "{text}");
            }
            Err(_) => prop_assert!(
                joined.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
                    || joined.components().all(|c| matches!(c, Component::CurDir))
            ),
        }
    }

    #[test]
    fn lyrics_parsing_never_panics(text in "(\\[[0-9:.]{0,9}\\])?[ -~\\n]{0,200}|\\PC{0,200}") {
        if let Some(lyrics) = from_text(&text, Some("eng"), LyricsSource::Sidecar) {
            prop_assert!(!lyrics.content.trim().is_empty());
        }
    }
}
