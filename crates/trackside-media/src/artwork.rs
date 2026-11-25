//! Content-addressed artwork originals and resized JPEG copies.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::{Body, Bytes};
use http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use http::request::Parts;
use http::{Response, StatusCode};
use image::{ImageFormat, ImageReader, Limits};
use trackside_core::{ArtworkId, ExternalArtId, PublicId};

use crate::MediaError;
use crate::range::{RangeOutcome, evaluate};
use crate::serve::{http_error, is_head};

/// Sizes resized copies are snapped up to, so the cache stays bounded.
pub const ART_SIZES: [u32; 6] = [64, 128, 256, 300, 600, 1200];
/// Originals larger than this are refused before anything is decoded.
pub const MAX_ORIGINAL_BYTES: usize = 32 * 1024 * 1024;
/// Neither side of an original may exceed this.
pub const MAX_DIMENSION: u32 = 8192;
/// Covers fetched for suggestions larger than this are refused.
pub const MAX_EXTERNAL_BYTES: usize = 2 * 1024 * 1024;
/// Neither side of a fetched cover may be smaller than this, or larger than
/// [`MAX_EXTERNAL_DIMENSION`].
pub const MIN_EXTERNAL_DIMENSION: u32 = 16;
pub const MAX_EXTERNAL_DIMENSION: u32 = 4096;
const JPEG_QUALITY: u8 = 85;
const IMMUTABLE: &str = "private, max-age=31536000, immutable";

/// Where artwork came from (`artwork.source_kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtworkSourceKind {
    Embedded,
    Sidecar,
    Upload,
    Acquired,
}

impl ArtworkSourceKind {
    /// The value stored in `artwork.source_kind`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Sidecar => "sidecar",
            Self::Upload => "upload",
            Self::Acquired => "acquired",
        }
    }
}

/// Where artwork came from, and the file to re-extract it from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtworkSource {
    pub kind: ArtworkSourceKind,
    pub path: Option<PathBuf>,
}

/// A stored original.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtworkMeta {
    pub id: ArtworkId,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

#[derive(Debug, Default)]
struct ResizedIndex {
    loaded: bool,
    entries: HashMap<PathBuf, (u64, u64)>,
    total: u64,
    tick: u64,
}

#[derive(Debug)]
struct Inner {
    dir: PathBuf,
    max_bytes: u64,
    resized: Mutex<ResizedIndex>,
    flights: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    cpu: tokio::sync::Semaphore,
}

/// Content-addressed artwork originals and resized copies under `<cache>/art`.
///
/// Originals live at `<dir>/<id[1..3]>/<id>`, resized JPEGs next to them as `<id>-<size>.jpg`.
/// Only resized copies and covers fetched for suggestions (`<id>.img`) count toward
/// `max_bytes` and are evicted; originals are what the scanner put in.
#[derive(Clone, Debug)]
pub struct ArtworkStore {
    inner: Arc<Inner>,
}

fn mime_of(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Png => Some("image/png"),
        ImageFormat::WebP => Some("image/webp"),
        ImageFormat::Gif => Some("image/gif"),
        _ => None,
    }
}

/// Reads the type and dimensions from the header only, refusing anything too large to decode.
fn inspect(bytes: &[u8]) -> Result<(ImageFormat, &'static str, u32, u32), MediaError> {
    if bytes.len() > MAX_ORIGINAL_BYTES {
        return Err(MediaError::TooLarge);
    }
    let format =
        image::guess_format(bytes).map_err(|_| MediaError::Unsupported("not an image".into()))?;
    let mime =
        mime_of(format).ok_or_else(|| MediaError::Unsupported(format!("image type {format:?}")))?;
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|e| MediaError::Unsupported(format!("unreadable image: {e}")))?;
    if width == 0 || height == 0 {
        return Err(MediaError::Unsupported("empty image".into()));
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(MediaError::TooLarge);
    }
    Ok((format, mime, width, height))
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes through a temporary name and renames, so readers never see half a file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{n}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Snaps a requested size up to the next standard size.
pub fn snap_size(requested: u32) -> u32 {
    ART_SIZES
        .into_iter()
        .find(|s| *s >= requested)
        .unwrap_or(ART_SIZES[ART_SIZES.len() - 1])
}

fn resize_to_jpeg(bytes: &[u8], format: ImageFormat, size: u32) -> Result<Vec<u8>, MediaError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|e| MediaError::Unsupported(format!("undecodable image: {e}")))?;
    let resized = image.resize(size, size, image::imageops::FilterType::Lanczos3);
    let rgb = resized.to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|e| MediaError::Unsupported(format!("jpeg encoding: {e}")))?;
    Ok(out)
}

impl ArtworkStore {
    /// A store under `dir` keeping at most `max_bytes` of resized copies.
    pub fn new(dir: impl Into<PathBuf>, max_bytes: u64) -> Self {
        let workers = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1));
        Self {
            inner: Arc::new(Inner {
                dir: dir.into(),
                max_bytes,
                resized: Mutex::default(),
                flights: Mutex::default(),
                cpu: tokio::sync::Semaphore::new(workers),
            }),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    fn shard(&self, id: &str) -> PathBuf {
        self.inner.dir.join(id.get(1..3).unwrap_or("00"))
    }

    /// Where the original for `id` is stored.
    pub fn original_path(&self, id: &ArtworkId) -> PathBuf {
        self.shard(id.as_str()).join(id.as_str())
    }

    /// Where a cover fetched for suggestions is stored.
    pub fn external_path(&self, id: &ExternalArtId) -> PathBuf {
        self.shard(id.as_str()).join(format!("{id}.img"))
    }

    fn resized_path(&self, id: &str, size: u32) -> PathBuf {
        self.shard(id).join(format!("{id}-{size}.jpg"))
    }

    /// Checks and stores a cover fetched for a suggestion: at most [`MAX_EXTERNAL_BYTES`],
    /// a JPEG, PNG, WebP or GIF by its magic bytes, and sides between
    /// [`MIN_EXTERNAL_DIMENSION`] and [`MAX_EXTERNAL_DIMENSION`]. It counts toward the cache
    /// limit and is evicted like a resized copy. Blocking.
    pub fn put_external(&self, id: &ExternalArtId, bytes: &[u8]) -> Result<(), MediaError> {
        if bytes.len() > MAX_EXTERNAL_BYTES {
            return Err(MediaError::TooLarge);
        }
        let (_, _, width, height) = inspect(bytes)?;
        if width.min(height) < MIN_EXTERNAL_DIMENSION {
            return Err(MediaError::Unsupported(String::from("image too small")));
        }
        if width.max(height) > MAX_EXTERNAL_DIMENSION {
            return Err(MediaError::TooLarge);
        }
        let path = self.external_path(id);
        write_atomic(&path, bytes)?;
        self.record_resized(&path, bytes.len() as u64);
        Ok(())
    }

    /// Whether a fetched cover is held.
    pub fn has_external(&self, id: &ExternalArtId) -> bool {
        self.external_path(id).is_file()
    }

    /// Stores an original image, checking its type and dimensions before decoding it.
    /// Blocking; the scanner calls it from its own threads.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the shared signature takes the source by value; the scanner records it in the database"
    )]
    pub fn put_original(
        &self,
        bytes: &[u8],
        source: ArtworkSource,
    ) -> Result<ArtworkMeta, MediaError> {
        let (_, mime, width, height) = inspect(bytes)?;
        let id = ArtworkId::of_bytes(bytes);
        let path = self.original_path(&id);
        if !path.exists() {
            write_atomic(&path, bytes)?;
            tracing::debug!(%id, kind = source.kind.as_str(), "stored artwork");
        }
        Ok(ArtworkMeta {
            id,
            mime,
            width,
            height,
            bytes: bytes.len() as u64,
        })
    }

    pub fn has(&self, id: &ArtworkId) -> bool {
        self.original_path(id).is_file()
    }

    fn index(&self) -> std::sync::MutexGuard<'_, ResizedIndex> {
        self.inner
            .resized
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn touch(&self, path: &Path) {
        let mut index = self.index();
        index.tick += 1;
        let tick = index.tick;
        if let Some(entry) = index.entries.get_mut(path) {
            entry.1 = tick;
        }
    }

    /// Walks the store once to learn the resized copies already on disk. Blocking.
    fn load_index(&self) {
        if self.index().loaded {
            return;
        }
        let mut found = Vec::new();
        if let Ok(shards) = std::fs::read_dir(&self.inner.dir) {
            for shard in shards.flatten() {
                let Ok(files) = std::fs::read_dir(shard.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    let path = file.path();
                    if path.extension().is_some_and(|e| e == "jpg" || e == "img")
                        && let Ok(meta) = file.metadata()
                    {
                        found.push((meta.modified().ok(), path, meta.len()));
                    }
                }
            }
        }
        found.sort_by_key(|f| f.0);
        let mut index = self.index();
        if index.loaded {
            return;
        }
        index.loaded = true;
        for (_, path, len) in found {
            index.tick += 1;
            let tick = index.tick;
            if index.entries.insert(path, (len, tick)).is_none() {
                index.total += len;
            }
        }
    }

    /// Records a new resized copy and evicts the least recently used ones over the cap.
    /// Blocking.
    fn record_resized(&self, path: &Path, len: u64) {
        self.load_index();
        let victims = {
            let mut index = self.index();
            index.tick += 1;
            let tick = index.tick;
            if let Some((old, _)) = index.entries.insert(path.to_path_buf(), (len, tick)) {
                index.total -= old;
            }
            index.total += len;
            let mut victims = Vec::new();
            while index.total > self.inner.max_bytes {
                let oldest = index
                    .entries
                    .iter()
                    .filter(|(p, _)| p.as_path() != path)
                    .min_by_key(|(_, (_, t))| *t)
                    .map(|(p, _)| p.clone());
                let Some(victim) = oldest else { break };
                if let Some((size, _)) = index.entries.remove(&victim) {
                    index.total -= size;
                }
                victims.push(victim);
            }
            victims
        };
        for victim in victims {
            let _ = std::fs::remove_file(victim);
        }
    }

    /// Bytes of resized copies currently tracked.
    pub fn resized_bytes(&self) -> u64 {
        self.index().total
    }

    fn flight(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .flights
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(path.to_path_buf())
            .or_default()
            .clone()
    }

    fn land(&self, path: &Path) {
        let mut flights = self
            .inner
            .flights
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if flights.get(path).is_some_and(|f| Arc::strong_count(f) <= 1) {
            flights.remove(path);
        }
    }

    /// Answers an artwork request: the original, or a JPEG at the snapped size made on first
    /// request (single-flight). Missing artwork is [`MediaError::NotFound`].
    pub(crate) async fn respond(
        &self,
        id: &ArtworkId,
        size: Option<u32>,
        parts: &Parts,
    ) -> Result<Response<Body>, MediaError> {
        self.respond_at(id.as_str(), &self.original_path(id), size, parts)
            .await
    }

    /// [`Self::respond`] for a cover fetched for suggestions, resized the same way.
    pub(crate) async fn respond_external(
        &self,
        id: &ExternalArtId,
        size: Option<u32>,
        parts: &Parts,
    ) -> Result<Response<Body>, MediaError> {
        let path = self.external_path(id);
        let response = self.respond_at(id.as_str(), &path, size, parts).await?;
        self.touch(&path);
        Ok(response)
    }

    async fn respond_at(
        &self,
        id: &str,
        original_path: &Path,
        size: Option<u32>,
        parts: &Parts,
    ) -> Result<Response<Body>, MediaError> {
        if let Some(requested) = size {
            let snapped = snap_size(requested);
            let resized = self.resized_path(id, snapped);
            if let Ok(bytes) = tokio::fs::read(&resized).await {
                self.touch(&resized);
                return image_response(
                    bytes.into(),
                    "image/jpeg",
                    &format!("\"{id}-{snapped}\""),
                    parts,
                );
            }
            let original = read_original(original_path).await?;
            let (format, mime, width, height) = inspect(&original)?;
            if width.max(height) <= snapped {
                return image_response(original, mime, &format!("\"{id}\""), parts);
            }
            let flight = self.flight(&resized);
            let made = {
                let _guard = flight.lock().await;
                self.resize_once(&resized, original, format, snapped).await
            };
            drop(flight);
            self.land(&resized);
            return image_response(made?, "image/jpeg", &format!("\"{id}-{snapped}\""), parts);
        }
        let original = read_original(original_path).await?;
        let (_, mime, _, _) = inspect(&original)?;
        image_response(original, mime, &format!("\"{id}\""), parts)
    }

    async fn resize_once(
        &self,
        resized: &Path,
        original: Bytes,
        format: ImageFormat,
        size: u32,
    ) -> Result<Bytes, MediaError> {
        if let Ok(bytes) = tokio::fs::read(resized).await {
            return Ok(bytes.into());
        }
        let _permit = self
            .inner
            .cpu
            .acquire()
            .await
            .map_err(|_| MediaError::Process("artwork workers closed".into()))?;
        let store = self.clone();
        let path = resized.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let jpeg = resize_to_jpeg(&original, format, size)?;
            write_atomic(&path, &jpeg)?;
            store.record_resized(&path, jpeg.len() as u64);
            Ok(Bytes::from(jpeg))
        })
        .await
        .map_err(|e| MediaError::Process(format!("resize task: {e}")))?
    }
}

/// Reads the original image `id` back from the file it came from: the picture embedded in an
/// audio file, or an image file. Only bytes that hash to `id` count, so a source that changed
/// since is not mistaken for the lost image. Blocking.
pub(crate) fn reextract_blocking(
    id: &ArtworkId,
    embedded: bool,
    source: &Path,
) -> Result<Option<Vec<u8>>, MediaError> {
    let matches = |bytes: &[u8]| ArtworkId::of_bytes(bytes) == *id;
    if embedded {
        use lofty::file::TaggedFileExt;
        let options = lofty::config::ParseOptions::new()
            .parsing_mode(lofty::config::ParsingMode::Relaxed)
            .read_properties(false);
        let unreadable =
            |e: &dyn std::fmt::Display| MediaError::Unsupported(format!("artwork source: {e}"));
        let tagged = lofty::probe::Probe::open(source)
            .map_err(|e| unreadable(&e))?
            .options(options)
            .guess_file_type()?
            .read()
            .map_err(|e| unreadable(&e))?;
        return Ok(tagged
            .tags()
            .iter()
            .flat_map(lofty::tag::Tag::pictures)
            .map(lofty::picture::Picture::data)
            .find(|bytes| matches(bytes))
            .map(<[u8]>::to_vec));
    }
    let len = std::fs::metadata(source)?.len();
    if len > MAX_ORIGINAL_BYTES as u64 {
        return Ok(None);
    }
    let bytes = std::fs::read(source)?;
    Ok(matches(&bytes).then_some(bytes))
}

async fn read_original(path: &Path) -> Result<Bytes, MediaError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(bytes.into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(MediaError::NotFound),
        Err(e) => Err(e.into()),
    }
}

fn image_response(
    bytes: Bytes,
    mime: &'static str,
    etag: &str,
    parts: &Parts,
) -> Result<Response<Body>, MediaError> {
    let not_modified = matches!(
        evaluate(
            &parts.headers,
            bytes.len() as u64,
            etag,
            std::time::UNIX_EPOCH
        ),
        RangeOutcome::NotModified
    );
    let builder = Response::builder()
        .header(ETAG, etag)
        .header(CACHE_CONTROL, IMMUTABLE);
    if not_modified {
        return builder
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .map_err(|e| http_error(&e));
    }
    let builder = builder
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, mime)
        .header(CONTENT_LENGTH, bytes.len());
    let body = if is_head(parts) {
        Body::empty()
    } else {
        Body::from(bytes)
    };
    builder.body(body).map_err(|e| http_error(&e))
}

impl trackside_core::enrich::ExternalArtStore for ArtworkStore {
    fn has(&self, id: &ExternalArtId) -> bool {
        self.has_external(id)
    }

    fn put(&self, id: &ExternalArtId, bytes: &[u8]) -> Result<(), String> {
        self.put_external(id, bytes).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut out = Vec::new();
        let encoded = image::DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png);
        assert!(encoded.is_ok());
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// A PNG signature and header claiming huge dimensions, with no pixel data at all.
    fn huge_png_header() -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut chunk = b"IHDR".to_vec();
        chunk.extend_from_slice(&100_000u32.to_be_bytes());
        chunk.extend_from_slice(&100_000u32.to_be_bytes());
        chunk.extend_from_slice(&[8, 2, 0, 0, 0]);
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(&chunk);
        out.extend_from_slice(&crc32(&chunk).to_be_bytes());
        for (kind, data) in [(b"IDAT", &[0u8][..]), (b"IEND", &[][..])] {
            let mut chunk = kind.to_vec();
            chunk.extend_from_slice(data);
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(&chunk);
            out.extend_from_slice(&crc32(&chunk).to_be_bytes());
        }
        out
    }

    fn source() -> ArtworkSource {
        ArtworkSource {
            kind: ArtworkSourceKind::Embedded,
            path: None,
        }
    }

    fn get(headers: &[(&str, &str)]) -> Parts {
        let mut builder = http::Request::builder().uri("/art");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(()).map_or_else(
            |_| http::Request::new(()).into_parts().0,
            |r| r.into_parts().0,
        )
    }

    async fn body_bytes(response: Response<Body>) -> Result<Bytes, Box<dyn std::error::Error>> {
        Ok(axum::body::to_bytes(response.into_body(), usize::MAX).await?)
    }

    #[test]
    fn oversized_image_is_rejected_before_decode() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = ArtworkStore::new(dir.path(), 1 << 20);
        let result = store.put_original(&huge_png_header(), source());
        assert!(matches!(result, Err(MediaError::TooLarge)), "{result:?}");
        let too_many_bytes = vec![0u8; MAX_ORIGINAL_BYTES + 1];
        assert!(matches!(
            store.put_original(&too_many_bytes, source()),
            Err(MediaError::TooLarge)
        ));
        assert!(matches!(
            store.put_original(b"not an image", source()),
            Err(MediaError::Unsupported(_))
        ));
        Ok(())
    }

    #[test]
    fn put_original_is_content_addressed() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = ArtworkStore::new(dir.path(), 1 << 20);
        let bytes = png(40, 30);
        let meta = store.put_original(&bytes, source())?;
        assert_eq!(meta.id, ArtworkId::of_bytes(&bytes));
        assert_eq!((meta.mime, meta.width, meta.height), ("image/png", 40, 30));
        assert!(store.has(&meta.id));
        let again = store.put_original(&bytes, source())?;
        assert_eq!(again, meta);
        let s = meta.id.as_str();
        assert_eq!(
            store.original_path(&meta.id),
            dir.path().join(&s[1..3]).join(s)
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_artwork_is_404() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = ArtworkStore::new(dir.path(), 1 << 20);
        let id = ArtworkId::of_bytes(b"nothing");
        assert!(matches!(
            store.respond(&id, None, &get(&[])).await,
            Err(MediaError::NotFound)
        ));
        assert!(matches!(
            store.respond(&id, Some(600), &get(&[])).await,
            Err(MediaError::NotFound)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn resized_artwork_is_cached_and_immutable() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = ArtworkStore::new(dir.path(), 1 << 20);
        let meta = store.put_original(&png(1000, 500), source())?;

        let first = store.respond(&meta.id, Some(500), &get(&[])).await?;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers()[CONTENT_TYPE], "image/jpeg");
        assert_eq!(first.headers()[CACHE_CONTROL], IMMUTABLE);
        let etag = first.headers()[ETAG].to_str()?.to_owned();
        assert_eq!(etag, format!("\"{}-600\"", meta.id));
        let first_bytes = body_bytes(first).await?;
        let resized = image::load_from_memory(&first_bytes)?;
        assert_eq!((resized.width(), resized.height()), (600, 300));
        let cached = store.resized_path(meta.id.as_str(), 600);
        assert!(cached.is_file());

        let second = store.respond(&meta.id, Some(600), &get(&[])).await?;
        assert_eq!(body_bytes(second).await?, first_bytes);

        let revalidated = store
            .respond(&meta.id, Some(600), &get(&[("if-none-match", &etag)]))
            .await?;
        assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);

        let original = store.respond(&meta.id, None, &get(&[])).await?;
        assert_eq!(original.headers()[CONTENT_TYPE], "image/png");
        Ok(())
    }

    #[tokio::test]
    async fn small_original_is_served_instead_of_upscaling()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = ArtworkStore::new(dir.path(), 1 << 20);
        let meta = store.put_original(&png(100, 100), source())?;
        let response = store.respond(&meta.id, Some(600), &get(&[])).await?;
        assert_eq!(response.headers()[CONTENT_TYPE], "image/png");
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_resizes_make_one_file_and_the_cap_evicts()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = ArtworkStore::new(dir.path(), 1);
        let meta = store.put_original(&png(800, 800), source())?;
        let (pa, pb) = (get(&[]), get(&[]));
        let (a, b) = tokio::join!(
            store.respond(&meta.id, Some(128), &pa),
            store.respond(&meta.id, Some(128), &pb)
        );
        assert_eq!(body_bytes(a?).await?, body_bytes(b?).await?);
        store.respond(&meta.id, Some(64), &get(&[])).await?;
        assert!(!store.resized_path(meta.id.as_str(), 128).exists());
        assert!(store.resized_path(meta.id.as_str(), 64).exists());
        assert!(store.has(&meta.id));
        Ok(())
    }

    #[test]
    fn fetched_covers_must_be_real_images_of_sane_size() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = ArtworkStore::new(dir.path(), 1 << 30);
        let id = ExternalArtId::derive("4128a6eb29f94943c9d206c08e625904");
        for (bytes, why) in [
            (b"<html>not a picture</html>".to_vec(), "not an image"),
            (huge_png_header(), "too large to decode"),
            (png(8, 8), "too small"),
            (png(5000, 20), "too wide"),
            (vec![0u8; MAX_EXTERNAL_BYTES + 1], "too many bytes"),
        ] {
            assert!(store.put_external(&id, &bytes).is_err(), "{why}");
            assert!(!store.has_external(&id), "{why}");
        }
        store.put_external(&id, &png(300, 300))?;
        assert!(store.has_external(&id));
        assert_eq!(
            store.external_path(&id),
            dir.path()
                .join(&id.as_str()[1..3])
                .join(format!("{id}.img"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn fetched_covers_are_served_resized_and_evicted_with_the_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = ArtworkStore::new(dir.path(), 1 << 30);
        let id = ExternalArtId::derive("one");
        store.put_external(&id, &png(700, 700))?;
        let resized = store.respond_external(&id, Some(300), &get(&[])).await?;
        assert_eq!(resized.headers()[CONTENT_TYPE], "image/jpeg");
        assert_eq!(resized.headers()[ETAG], format!("\"{id}-300\"").as_str());
        assert_eq!(resized.headers()[CACHE_CONTROL], IMMUTABLE);
        let original = store.respond_external(&id, None, &get(&[])).await?;
        assert_eq!(original.headers()[CONTENT_TYPE], "image/png");
        assert!(store.resized_bytes() > 0, "counted toward the cache limit");

        // With the cache full, a new cover pushes the least recently used out.
        let dir = tempfile::tempdir()?;
        let store = ArtworkStore::new(dir.path(), 1);
        let (one, two) = (ExternalArtId::derive("one"), ExternalArtId::derive("two"));
        store.put_external(&one, &png(64, 64))?;
        store.put_external(&two, &png(64, 64))?;
        assert!(store.has_external(&two));
        assert!(!store.has_external(&one), "evicted");
        assert!(matches!(
            store.respond_external(&one, None, &get(&[])).await,
            Err(MediaError::NotFound)
        ));
        Ok(())
    }

    #[test]
    fn sizes_snap_up() {
        assert_eq!(snap_size(1), 64);
        assert_eq!(snap_size(300), 300);
        assert_eq!(snap_size(301), 600);
        assert_eq!(snap_size(5000), 1200);
    }
}
