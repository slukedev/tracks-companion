//! The transcode cache: complete files under `<cache>/transcodes`, least recently used out
//! first, capped in size and kept above a free-space floor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use trackside_core::records::SongFile;
use trackside_core::{PublicId, SongId};

use crate::{CachedTranscode, Profile, profile_format};

/// Free space on the volume holding a path. The seam for disk-full tests.
pub trait FreeSpace: Send + Sync + std::fmt::Debug + 'static {
    fn available(&self, path: &Path) -> std::io::Result<u64>;
}

/// The real filesystem's free space.
#[derive(Debug, Default, Clone, Copy)]
pub struct FsFreeSpace;

impl FreeSpace for FsFreeSpace {
    fn available(&self, path: &Path) -> std::io::Result<u64> {
        fs4::available_space(path)
    }
}

/// One transcode: a song at a content version in a profile.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey {
    pub song: SongId,
    pub version: i64,
    pub profile: Profile,
}

impl CacheKey {
    pub(crate) fn of(file: &SongFile, profile: Profile) -> Self {
        Self {
            song: file.song.clone(),
            version: file.content_version,
            profile,
        }
    }
}

/// How a cached file was made. Each route is deterministic on its own but the two produce
/// different bytes, so the route is part of the `ETag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Direct,
}

impl Route {
    fn suffix(self) -> &'static str {
        match self {
            Self::Direct => "",
        }
    }
}

#[derive(Debug)]
struct Entry {
    file_name: String,
    size: u64,
    tick: u64,
    tag: String,
}

#[derive(Debug, Default)]
struct Index {
    entries: HashMap<CacheKey, Entry>,
    total: u64,
    tick: u64,
}

#[derive(Debug)]
pub(crate) struct TranscodeCache {
    dir: PathBuf,
    encoder: String,
    max_bytes: u64,
    min_free: u64,
    free: Arc<dyn FreeSpace>,
    index: Mutex<Index>,
}

fn extension(profile: Profile) -> &'static str {
    profile.output().0.as_str()
}

impl TranscodeCache {
    /// Opens the cache directory, removing partial files and files from another encoder.
    pub(crate) fn open(
        dir: PathBuf,
        encoder: &str,
        max_bytes: u64,
        min_free: u64,
        free: Arc<dyn FreeSpace>,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata()?;
            match parse_name(&name) {
                // Remuxed HLS files (tag suffix `h`) from earlier versions lack the edit list
                // gapless playback needs, so they go with the other stale files.
                Some((key, tag)) if meta.is_file() && tag == encoder => {
                    found.push((meta.modified().ok(), key, name, meta.len(), tag));
                }
                _ => {
                    if meta.is_file() {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
        found.sort_by_key(|f| f.0);
        let mut index = Index::default();
        for (_, key, file_name, size, tag) in found {
            index.tick += 1;
            index.total += size;
            let tick = index.tick;
            index.entries.insert(
                key,
                Entry {
                    file_name,
                    size,
                    tick,
                    tag,
                },
            );
        }
        Ok(Self {
            dir,
            encoder: encoder.to_owned(),
            max_bytes,
            min_free,
            free,
            index: Mutex::new(index),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn file_name(&self, key: &CacheKey, route: Route) -> String {
        format!(
            "{}.{}.{}.{}{}.{}",
            key.song,
            key.version,
            key.profile.as_str(),
            self.encoder,
            route.suffix(),
            extension(key.profile)
        )
    }

    pub(crate) fn part_path(&self, key: &CacheKey, route: Route) -> PathBuf {
        self.dir
            .join(format!("{}.part", self.file_name(key, route)))
    }

    /// The cached transcode for this file, marking it used.
    pub(crate) fn get(&self, file: &SongFile, profile: Profile) -> Option<CachedTranscode> {
        let key = CacheKey::of(file, profile);
        let mut index = self.lock();
        index.tick += 1;
        let tick = index.tick;
        let entry = index.entries.get_mut(&key)?;
        entry.tick = tick;
        Some(CachedTranscode {
            path: self.dir.join(&entry.file_name),
            size: entry.size,
            etag: format!(
                "\"{}.{}.{}.{}\"",
                key.song,
                key.version,
                profile.as_str(),
                entry.tag
            ),
            profile,
            format: profile_format(profile, file, entry.size),
        })
    }

    /// Forgets an entry whose file disappeared.
    pub(crate) fn forget(&self, key: &CacheKey) {
        let mut index = self.lock();
        if let Some(entry) = index.entries.remove(key) {
            index.total -= entry.size;
        }
    }

    fn free_space(&self) -> u64 {
        self.free.available(&self.dir).unwrap_or(u64::MAX)
    }

    /// Evicts least recently used files until `need` more bytes fit under the cap and above
    /// the free-space floor. Blocking; false when that cannot be reached.
    pub(crate) fn make_room(&self, need: u64, keep: Option<&CacheKey>) -> bool {
        loop {
            let victim = {
                let mut index = self.lock();
                let over_cap = index.total.saturating_add(need) > self.max_bytes;
                let low_space = self.free_space() < self.min_free.saturating_add(need);
                if !over_cap && !low_space {
                    return true;
                }
                let oldest = index
                    .entries
                    .iter()
                    .filter(|(k, _)| Some(*k) != keep)
                    .min_by_key(|(_, e)| e.tick)
                    .map(|(k, _)| k.clone());
                let Some(key) = oldest else {
                    return false;
                };
                index.entries.remove(&key).inspect(|entry| {
                    index.total -= entry.size;
                })
            };
            if let Some(entry) = victim {
                tracing::debug!(file = %entry.file_name, "evicting cached transcode");
                if let Err(e) = std::fs::remove_file(self.dir.join(&entry.file_name)) {
                    tracing::warn!(error = %e, "could not remove cached transcode");
                }
            }
        }
    }

    /// Moves a finished part file into the cache. Blocking.
    pub(crate) fn commit(&self, key: &CacheKey, route: Route, part: &Path) -> std::io::Result<u64> {
        let name = self.file_name(key, route);
        let size = std::fs::metadata(part)?.len();
        std::fs::rename(part, self.dir.join(&name))?;
        let replaced = {
            let mut index = self.lock();
            index.tick += 1;
            index.total += size;
            let tick = index.tick;
            let tag = format!("{}{}", self.encoder, route.suffix());
            let old = index.entries.insert(
                key.clone(),
                Entry {
                    file_name: name.clone(),
                    size,
                    tick,
                    tag,
                },
            );
            if let Some(old) = &old {
                index.total -= old.size;
            }
            old.filter(|o| o.file_name != name)
        };
        if let Some(old) = replaced {
            let _ = std::fs::remove_file(self.dir.join(old.file_name));
        }
        self.make_room(0, Some(key));
        Ok(size)
    }

    pub(crate) fn total(&self) -> u64 {
        self.lock().total
    }

    /// Whether `need` bytes can be written without going below the floor, after eviction.
    pub(crate) fn has_space_for(&self, need: u64) -> bool {
        self.make_room(need, None)
    }

    pub(crate) fn free_now(&self) -> u64 {
        self.free_space()
    }

    pub(crate) fn min_free(&self) -> u64 {
        self.min_free
    }
}

/// The key a cache file name stands for.
pub(crate) fn key_from_path(path: &Path) -> Option<CacheKey> {
    parse_name(path.file_name()?.to_str()?).map(|(key, _)| key)
}

fn parse_name(name: &str) -> Option<(CacheKey, String)> {
    let mut parts = name.split('.');
    let song = SongId::parse(parts.next()?).ok()?;
    let version = parts.next()?.parse().ok()?;
    let profile = Profile::parse(parts.next()?)?;
    let tag = parts.next()?.to_owned();
    let ext = parts.next()?;
    if parts.next().is_some() || ext != extension(profile) {
        return None;
    }
    Some((
        CacheKey {
            song,
            version,
            profile,
        },
        tag,
    ))
}
