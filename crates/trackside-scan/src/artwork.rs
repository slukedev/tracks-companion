//! Artwork: which picture stands for a song, and storing originals through the media crate's
//! [`ArtworkStore`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use trackside_core::ArtworkId;
use trackside_core::repo::artwork::{ArtworkSourceKind as RowKind, NewArtwork};
use trackside_media::{ArtworkSource, ArtworkSourceKind, ArtworkStore};

use crate::paths::{extension, os_from_bytes};
use crate::{Picture, PictureKind};

/// Sidecar names in order of preference, without extension.
const SIDECAR_STEMS: &[&str] = &["cover", "folder", "front", "album"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif"];
/// Pictures larger than this are ignored rather than copied into the cache.
pub const MAX_PICTURE_BYTES: u64 = 20 * 1024 * 1024;

/// The preferred sidecar image among a folder's file names.
pub fn pick_sidecar(names: &[Vec<u8>]) -> Option<&[u8]> {
    let mut best: Option<(usize, &[u8])> = None;
    for name in names {
        let Some(ext) = extension(name) else {
            continue;
        };
        if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            continue;
        }
        let stem = &name[..name.len() - ext.len() - 1];
        let Some(rank) = SIDECAR_STEMS
            .iter()
            .position(|s| stem.eq_ignore_ascii_case(s.as_bytes()))
        else {
            continue;
        };
        if best.is_none_or(|(r, n)| rank < r || (rank == r && name.as_slice() < n)) {
            best = Some((rank, name));
        }
    }
    best.map(|(_, name)| name)
}

/// The embedded picture to use: the front cover, else the first picture.
pub fn pick_embedded(pictures: &[Picture]) -> Option<&Picture> {
    pictures
        .iter()
        .find(|p| p.kind == PictureKind::FrontCover)
        .or_else(|| pictures.first())
        .filter(|p| !p.bytes.is_empty())
}

/// Where originals go. The real one is [`ArtworkStore`]; tests use a recorder.
pub trait ArtworkSink: Send + Sync + 'static {
    /// Stores an original and returns what to record in the `artwork` table, or `None` when it
    /// cannot be used (not an image, too large, or the store refused it).
    fn put(&self, bytes: &[u8], kind: ArtworkSourceKind, path: Option<&Path>)
    -> Option<NewArtwork>;
}

impl ArtworkSink for ArtworkStore {
    fn put(
        &self,
        bytes: &[u8],
        kind: ArtworkSourceKind,
        path: Option<&Path>,
    ) -> Option<NewArtwork> {
        let source = ArtworkSource {
            kind,
            path: path.map(Path::to_path_buf),
        };
        match self.put_original(bytes, source) {
            Ok(meta) => Some(NewArtwork {
                id: meta.id,
                mime: meta.mime.to_owned(),
                width: meta.width,
                height: meta.height,
                bytes: meta.bytes,
                source: row_kind(kind),
                source_path: path.map(Path::to_path_buf),
            }),
            Err(error) => {
                tracing::debug!(%error, "artwork not stored");
                None
            }
        }
    }
}

/// The media crate's source kind as stored in `artwork.source_kind`.
pub fn row_kind(kind: ArtworkSourceKind) -> RowKind {
    match kind {
        ArtworkSourceKind::Embedded => RowKind::Embedded,
        ArtworkSourceKind::Sidecar => RowKind::Sidecar,
        ArtworkSourceKind::Upload => RowKind::Upload,
        ArtworkSourceKind::Acquired => RowKind::Acquired,
    }
}

/// Per-run memory of what was already stored, so a folder's sidecar is read once and an
/// album's embedded cover is copied once. Newly stored images wait in `fresh` until the next
/// commit records their rows.
#[derive(Debug, Default)]
pub struct ArtworkCache {
    sidecars: Mutex<HashMap<PathBuf, Option<ArtworkId>>>,
    stored: Mutex<HashSet<ArtworkId>>,
    fresh: Mutex<Vec<NewArtwork>>,
}

impl ArtworkCache {
    /// The artwork for a song: the folder's sidecar, else the embedded picture.
    pub fn resolve(
        &self,
        sink: &dyn ArtworkSink,
        sidecar: Option<&Path>,
        audio_path: &Path,
        pictures: &[Picture],
    ) -> Option<ArtworkId> {
        if let Some(path) = sidecar
            && let Some(id) = self.file(sink, path)
        {
            return Some(id);
        }
        let picture = pick_embedded(pictures)?;
        let id = ArtworkId::of_bytes(&picture.bytes);
        if lock(&self.stored).contains(&id) {
            return Some(id);
        }
        let stored = sink.put(
            &picture.bytes,
            ArtworkSourceKind::Embedded,
            Some(audio_path),
        )?;
        Some(self.remember(stored))
    }

    /// An image file (a sidecar cover or an artist picture), stored once per run.
    pub fn file(&self, sink: &dyn ArtworkSink, path: &Path) -> Option<ArtworkId> {
        if let Some(known) = lock(&self.sidecars).get(path) {
            return known.clone();
        }
        let id = read_picture(path)
            .and_then(|bytes| sink.put(&bytes, ArtworkSourceKind::Sidecar, Some(path)))
            .map(|stored| self.remember(stored));
        lock(&self.sidecars).insert(path.to_path_buf(), id.clone());
        id
    }

    fn remember(&self, stored: NewArtwork) -> ArtworkId {
        let id = stored.id.clone();
        if lock(&self.stored).insert(id.clone()) {
            lock(&self.fresh).push(stored);
        }
        id
    }

    /// Rows for images stored since the last call.
    pub fn take_fresh(&self) -> Vec<NewArtwork> {
        std::mem::take(&mut *lock(&self.fresh))
    }
}

fn read_picture(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_PICTURE_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

/// An artist picture (`artist.*`) among a folder's file names.
pub fn pick_artist(names: &[Vec<u8>]) -> Option<&[u8]> {
    names
        .iter()
        .filter(|name| {
            extension(name).is_some_and(|ext| {
                IMAGE_EXTENSIONS.contains(&ext.as_str())
                    && name[..name.len() - ext.len() - 1].eq_ignore_ascii_case(b"artist")
            })
        })
        .min()
        .map(Vec::as_slice)
}

/// The sidecar's absolute path in `folder`, if the listing has one.
pub fn sidecar_in(folder: &Path, names: &[Vec<u8>]) -> Option<PathBuf> {
    pick_sidecar(names).map(|name| folder.join(os_from_bytes(name)))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        puts: Mutex<Vec<(ArtworkSourceKind, usize)>>,
    }

    impl ArtworkSink for Recorder {
        fn put(
            &self,
            bytes: &[u8],
            kind: ArtworkSourceKind,
            path: Option<&Path>,
        ) -> Option<NewArtwork> {
            lock(&self.puts).push((kind, bytes.len()));
            Some(NewArtwork {
                id: ArtworkId::of_bytes(bytes),
                mime: "image/jpeg".into(),
                width: 1,
                height: 1,
                bytes: bytes.len() as u64,
                source: row_kind(kind),
                source_path: path.map(Path::to_path_buf),
            })
        }
    }

    fn names(list: &[&str]) -> Vec<Vec<u8>> {
        list.iter().map(|n| n.as_bytes().to_vec()).collect()
    }

    #[test]
    fn sidecar_preference_is_cover_folder_front_album() {
        let listing = names(&["album.png", "Folder.JPG", "front.jpg", "01.flac"]);
        assert_eq!(pick_sidecar(&listing), Some(b"Folder.JPG".as_slice()));
        let listing = names(&["cover.webp", "folder.jpg"]);
        assert_eq!(pick_sidecar(&listing), Some(b"cover.webp".as_slice()));
        assert_eq!(pick_sidecar(&names(&["cover.txt", "back.jpg"])), None);
    }

    #[test]
    fn front_cover_beats_other_embedded_pictures() {
        let pictures = vec![
            Picture {
                kind: PictureKind::Other,
                mime: None,
                bytes: vec![1],
            },
            Picture {
                kind: PictureKind::FrontCover,
                mime: None,
                bytes: vec![2],
            },
        ];
        assert_eq!(
            pick_embedded(&pictures).map(|p| p.bytes.clone()),
            Some(vec![2])
        );
        assert_eq!(
            pick_embedded(&pictures[..1]).map(|p| p.bytes.clone()),
            Some(vec![1])
        );
    }

    #[test]
    fn embedded_covers_are_stored_once_per_run() {
        let sink = Recorder::default();
        let cache = ArtworkCache::default();
        let pictures = vec![Picture {
            kind: PictureKind::FrontCover,
            mime: None,
            bytes: vec![9; 10],
        }];
        let a = cache.resolve(&sink, None, Path::new("/a.flac"), &pictures);
        let b = cache.resolve(&sink, None, Path::new("/b.flac"), &pictures);
        assert_eq!(a, b);
        assert_eq!(lock(&sink.puts).len(), 1);
        let rows = cache.take_fresh();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bytes, 10);
        assert_eq!(cache.take_fresh(), Vec::new());
    }

    #[test]
    fn artist_pictures_are_found_by_name() {
        let listing = names(&["Artist.JPG", "cover.jpg", "artists.png"]);
        assert_eq!(pick_artist(&listing), Some(b"Artist.JPG".as_slice()));
        assert_eq!(pick_artist(&names(&["cover.jpg"])), None);
    }

    #[test]
    fn a_sidecar_is_read_once_and_preferred() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let cover = dir.path().join("cover.jpg");
        std::fs::write(&cover, b"jpeg bytes")?;
        let sink = Recorder::default();
        let cache = ArtworkCache::default();
        let pictures = vec![Picture {
            kind: PictureKind::FrontCover,
            mime: None,
            bytes: vec![1],
        }];
        let first = cache.resolve(&sink, Some(&cover), Path::new("/a.flac"), &pictures);
        let second = cache.resolve(&sink, Some(&cover), Path::new("/b.flac"), &pictures);
        assert_eq!(first, Some(ArtworkId::of_bytes(b"jpeg bytes")));
        assert_eq!(first, second);
        assert_eq!(
            lock(&sink.puts).as_slice(),
            &[(ArtworkSourceKind::Sidecar, 10)]
        );
        Ok(())
    }
}
