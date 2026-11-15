//! Building range-aware responses for files on disk, and the streaming bodies behind them.

use std::fmt::Write as _;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use futures_util::stream;
use http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
    ETAG, LAST_MODIFIED,
};
use http::request::Parts;
use http::{HeaderValue, Method, Response, StatusCode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tokio::sync::{mpsc, watch};

use crate::range::{RangeOutcome, evaluate, whole_seconds};
use crate::{Disposition, GrowState, MediaError};

/// Bytes read from disk per chunk. A slow client holds one chunk in flight, not the file.
pub(crate) const CHUNK: usize = 256 * 1024;
/// A response that makes no write progress for this long is abandoned.
pub(crate) const STALL: Duration = Duration::from_secs(120);

pub(crate) type Chunk = Result<Bytes, std::io::Error>;

/// A body that keeps a guard alive until it is finished or dropped.
pub(crate) struct Held<G> {
    body: Body,
    guard: Option<G>,
}

impl<G> Held<G> {
    pub(crate) fn new(body: Body, guard: G) -> Self {
        Self {
            body,
            guard: Some(guard),
        }
    }
}

impl<G: Unpin> http_body::Body for Held<G> {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        let polled = std::pin::Pin::new(&mut self.body).poll_frame(cx);
        if matches!(polled, std::task::Poll::Ready(None | Some(Err(_)))) {
            self.guard = None;
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

/// What a range response needs to know about the bytes it serves.
#[derive(Debug)]
pub(crate) struct Entity<'a> {
    pub len: u64,
    pub etag: &'a str,
    pub last_modified: SystemTime,
    pub content_type: &'static str,
    pub cache_control: &'static str,
    pub disposition: Option<&'a Disposition>,
}

pub(crate) fn is_head(parts: &Parts) -> bool {
    parts.method == Method::HEAD
}

fn header(value: &str) -> Result<HeaderValue, MediaError> {
    HeaderValue::from_str(value)
        .map_err(|_| MediaError::Unsupported(format!("header value {value:?}")))
}

pub(crate) fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

pub(crate) fn system_time_from_ns(ns: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(u64::try_from(ns).unwrap_or(0))
}

/// Answers a `GET` or `HEAD` for an open file with ranges and conditional requests.
pub(crate) async fn respond_file(
    mut file: tokio::fs::File,
    entity: &Entity<'_>,
    parts: &Parts,
) -> Result<Response<Body>, MediaError> {
    let outcome = evaluate(
        &parts.headers,
        entity.len,
        entity.etag,
        entity.last_modified,
    );
    let (status, start, len) = match outcome {
        RangeOutcome::Full => (StatusCode::OK, 0, entity.len),
        RangeOutcome::Partial {
            start,
            end_inclusive,
        } => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end_inclusive - start + 1,
        ),
        RangeOutcome::Unsatisfiable => {
            return Err(MediaError::RangeNotSatisfiable { len: entity.len });
        }
        RangeOutcome::NotModified => {
            let mut builder = Response::builder().status(StatusCode::NOT_MODIFIED);
            builder = validators(builder, entity)?;
            return builder.body(Body::empty()).map_err(|e| http_error(&e));
        }
        RangeOutcome::PreconditionFailed => {
            return Response::builder()
                .status(StatusCode::PRECONDITION_FAILED)
                .header(CONTENT_LENGTH, 0)
                .body(Body::empty())
                .map_err(|e| http_error(&e));
        }
    };

    let mut builder = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, entity.content_type)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, len);
    builder = validators(builder, entity)?;
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            CONTENT_RANGE,
            format!("bytes {start}-{}/{}", start + len - 1, entity.len),
        );
    }
    if let Some(disposition) = entity.disposition {
        builder = builder.header(CONTENT_DISPOSITION, content_disposition(disposition)?);
    }
    let body = if is_head(parts) || len == 0 {
        Body::empty()
    } else {
        if start > 0 {
            file.seek(SeekFrom::Start(start)).await?;
        }
        reader_body(file, Some(len))
    };
    builder.body(body).map_err(|e| http_error(&e))
}

fn validators(
    mut builder: http::response::Builder,
    entity: &Entity<'_>,
) -> Result<http::response::Builder, MediaError> {
    builder = builder
        .header(ETAG, header(entity.etag)?)
        .header(
            LAST_MODIFIED,
            httpdate::fmt_http_date(whole_seconds(entity.last_modified)),
        )
        .header(CACHE_CONTROL, entity.cache_control);
    Ok(builder)
}

pub(crate) fn http_error(e: &http::Error) -> MediaError {
    MediaError::Unsupported(format!("response: {e}"))
}

/// `attachment` with an ASCII fallback name and the exact UTF-8 name (RFC 6266).
pub(crate) fn content_disposition(disposition: &Disposition) -> Result<HeaderValue, MediaError> {
    match disposition {
        Disposition::Inline => Ok(HeaderValue::from_static("inline")),
        Disposition::Attachment { file_name } => {
            let cleaned: String = file_name
                .chars()
                .map(|c| {
                    if c.is_control() || matches!(c, '/' | '\\') {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let ascii: String = cleaned
                .chars()
                .map(|c| if c.is_ascii() && c != '"' { c } else { '_' })
                .collect();
            let mut encoded = String::with_capacity(cleaned.len() * 3);
            for byte in cleaned.bytes() {
                if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                    encoded.push(char::from(byte));
                } else {
                    let _ = write!(encoded, "%{byte:02X}");
                }
            }
            header(&format!(
                "attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
            ))
        }
    }
}

fn channel_body(rx: mpsc::Receiver<Chunk>) -> Body {
    let stream = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    });
    Body::from_stream(stream)
}

/// Streams `limit` bytes (or everything) from a reader through a task, so a reader that the
/// client stops consuming is dropped after [`STALL`]. A short read ends the body with an
/// error, so the connection is closed instead of hanging on a promised length.
pub(crate) fn reader_body<R>(reader: R, limit: Option<u64>) -> Body
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let (tx, rx) = mpsc::channel(2);
    tokio::spawn(async move {
        pump_reader(reader, limit, tx).await;
    });
    channel_body(rx)
}

/// Returns true when the reader was drained or the limit reached, false when the client
/// went away or reading failed.
pub(crate) async fn pump_reader<R>(
    mut reader: R,
    limit: Option<u64>,
    tx: mpsc::Sender<Chunk>,
) -> bool
where
    R: AsyncRead + Send + Unpin,
{
    let mut remaining = limit;
    loop {
        let want = remaining.map_or(CHUNK, |r| CHUNK.min(usize::try_from(r).unwrap_or(CHUNK)));
        if want == 0 {
            return true;
        }
        let mut buf = Vec::with_capacity(want);
        let read = reader.read_buf(&mut buf).await;
        let chunk = match read {
            Ok(0) => {
                if remaining.is_some() {
                    let _ = tx.send(Err(std::io::ErrorKind::UnexpectedEof.into())).await;
                }
                return true;
            }
            Ok(n) => {
                if let Some(r) = remaining.as_mut() {
                    *r -= n as u64;
                }
                Ok(Bytes::from(buf))
            }
            Err(e) => Err(e),
        };
        let failed = chunk.is_err();
        if tx.send_timeout(chunk, STALL).await.is_err() || failed {
            return false;
        }
    }
}

/// Where a growing body gets its bytes and when it is finished.
pub(crate) trait GrowSource: Send + 'static {
    /// The current state; `None` when the producer went away without finishing.
    fn state(&mut self) -> Option<GrowState>;
    /// Waits for the state to change, or for a short poll interval for producers that cannot
    /// signal progress.
    fn changed(&mut self) -> impl std::future::Future<Output = ()> + Send;
    /// Where the bytes are once complete, when the producer renames its file at the end.
    fn final_path(&self) -> Option<PathBuf> {
        None
    }
}

/// A growing file whose producer reports progress through a watch channel.
pub(crate) struct WatchSource(pub watch::Receiver<GrowState>);

impl GrowSource for WatchSource {
    fn state(&mut self) -> Option<GrowState> {
        Some(*self.0.borrow_and_update())
    }

    async fn changed(&mut self) {
        if self.0.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Streams a file that is still being written. Reads to the current end, then waits for the
/// source to report more bytes, completion or failure. `cap` truncates the body.
pub(crate) fn growing_body<S: GrowSource>(path: PathBuf, source: S, cap: Option<u64>) -> Body {
    let (tx, rx) = mpsc::channel(2);
    tokio::spawn(pump_growing(path, source, cap, tx));
    channel_body(rx)
}

pub(crate) async fn pump_growing<S: GrowSource>(
    path: PathBuf,
    mut source: S,
    cap: Option<u64>,
    tx: mpsc::Sender<Chunk>,
) {
    if let Err(e) = pump_growing_inner(&path, &mut source, cap, &tx).await {
        let _ = tx.send(Err(e)).await;
    }
}

async fn open_when_present<S: GrowSource>(
    path: &Path,
    source: &mut S,
) -> std::io::Result<tokio::fs::File> {
    let deadline = tokio::time::Instant::now() + STALL;
    loop {
        match tokio::fs::File::open(path).await {
            Ok(file) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match source.state() {
                    None | Some(GrowState::Failed) => return Err(failed()),
                    Some(GrowState::Complete { .. }) => {
                        if let Some(done) = source.final_path() {
                            return tokio::fs::File::open(done).await;
                        }
                    }
                    Some(GrowState::Growing { .. }) => {}
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                source.changed().await;
            }
            Err(e) => return Err(e),
        }
    }
}

fn failed() -> std::io::Error {
    std::io::Error::other("the producer failed")
}

async fn pump_growing_inner<S: GrowSource>(
    path: &Path,
    source: &mut S,
    cap: Option<u64>,
    tx: &mpsc::Sender<Chunk>,
) -> std::io::Result<()> {
    let mut file = open_when_present(path, source).await?;
    let mut sent: u64 = 0;
    let mut idle_since = tokio::time::Instant::now();
    loop {
        let want = cap.map_or(CHUNK as u64, |c| (CHUNK as u64).min(c - sent));
        if want == 0 {
            return Ok(());
        }
        let mut buf = Vec::with_capacity(usize::try_from(want).unwrap_or(CHUNK));
        let n = file.read_buf(&mut buf).await?;
        if n > 0 {
            sent += n as u64;
            idle_since = tokio::time::Instant::now();
            if tx.send_timeout(Ok(Bytes::from(buf)), STALL).await.is_err() {
                return Ok(());
            }
            continue;
        }
        match source.state() {
            None | Some(GrowState::Failed) => return Err(failed()),
            Some(GrowState::Complete { bytes }) if sent >= bytes => return Ok(()),
            Some(GrowState::Complete { .. }) => {
                // Complete but our handle is at its end: the bytes are in a renamed file
                // with the same inode, so one more read pass finds them, or nothing will.
                let mut rest = Vec::with_capacity(usize::try_from(want).unwrap_or(CHUNK));
                let n = file.read_buf(&mut rest).await?;
                if n == 0 {
                    return Ok(());
                }
                sent += n as u64;
                if tx.send_timeout(Ok(Bytes::from(rest)), STALL).await.is_err() {
                    return Ok(());
                }
            }
            Some(GrowState::Growing { .. }) => {
                if idle_since.elapsed() >= STALL {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                source.changed().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_names_are_sanitised_and_encoded() -> Result<(), MediaError> {
        let value = content_disposition(&Disposition::Attachment {
            file_name: "Beyoncé/\"Halo\".m4a".into(),
        })?;
        assert_eq!(
            value.to_str().unwrap_or_default(),
            "attachment; filename=\"Beyonc___Halo_.m4a\"; filename*=UTF-8''Beyonc%C3%A9_%22Halo%22.m4a"
        );
        Ok(())
    }
}
