//! Media tests with the fake transcoder, real files in temp directories and paused time
//! where waiting is involved.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use http::StatusCode;
use http::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
};
use trackside_core::records::{AudioFormat, Codec, Container};
use trackside_core::testing::TestDb;
use trackside_core::{LibraryKey, PublicId};

use super::*;
use crate::testing::{FakeHls, FakeTranscoder};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Free space that shrinks as the transcode cache grows.
#[derive(Debug)]
struct FakeFree {
    capacity: AtomicU64,
    dir: PathBuf,
}

impl FreeSpace for FakeFree {
    fn available(&self, _path: &Path) -> std::io::Result<u64> {
        let used: u64 = std::fs::read_dir(&self.dir)?
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        Ok(self.capacity.load(Ordering::SeqCst).saturating_sub(used))
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    media: Media,
    fake: Arc<FakeTranscoder>,
    free: Arc<FakeFree>,
    db: TestDb,
}

fn config() -> MediaConfig {
    let mut config = trackside_core::Config::defaults("/data").media;
    config.transcode_max_concurrent = 2;
    config.cache_min_free = 0;
    config
}

fn fixture_with(
    config: &MediaConfig,
    fake: FakeTranscoder,
) -> Result<Fixture, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = TestDb::new()?;
    let fake = Arc::new(fake);
    let free = Arc::new(FakeFree {
        capacity: AtomicU64::new(u64::MAX / 2),
        dir: dir.path().join("cache/transcodes"),
    });
    std::fs::create_dir_all(dir.path().join("music"))?;
    let media = Media::with_free_space(
        config,
        &dir.path().join("cache"),
        db.db.clone(),
        fake.clone(),
        db.clock.clone(),
        free.clone(),
    )?;
    Ok(Fixture {
        dir,
        media,
        fake,
        free,
        db,
    })
}

fn fixture() -> Result<Fixture, Box<dyn std::error::Error>> {
    fixture_with(&config(), FakeTranscoder::new())
}

impl Fixture {
    fn song(
        &self,
        name: &str,
        bytes: &[u8],
        container: Container,
        codec: Codec,
    ) -> Result<SongFile, Box<dyn std::error::Error>> {
        let path = self.dir.path().join("music").join(name);
        std::fs::write(&path, bytes)?;
        let meta = std::fs::metadata(&path)?;
        Ok(SongFile {
            song: SongId::from_hash(blake3::hash(name.as_bytes()).as_bytes()),
            library: LibraryKey(1),
            path,
            size: meta.len(),
            mtime_ns: serve::mtime_ns(&meta),
            content_version: 3,
            format: AudioFormat {
                container,
                codec,
                content_type: container.content_type(),
                lossless: codec.is_lossless(),
                bitrate_kbps: Some(900),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                channels: Some(2),
                size: meta.len(),
            },
            duration_ms: 1_000,
        })
    }

    fn flac(&self, name: &str) -> Result<SongFile, Box<dyn std::error::Error>> {
        let bytes: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        self.song(name, &bytes, Container::Flac, Codec::Flac)
    }
}

fn parts(method: &str, headers: &[(&str, &str)]) -> Parts {
    let mut builder = http::Request::builder().method(method).uri("/x");
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    builder.body(()).map_or_else(
        |_| http::Request::new(()).into_parts().0,
        |r| r.into_parts().0,
    )
}

fn get(headers: &[(&str, &str)]) -> Parts {
    parts("GET", headers)
}

async fn body(response: Response<Body>) -> Result<Bytes, Box<dyn std::error::Error>> {
    Ok(axum::body::to_bytes(response.into_body(), usize::MAX).await?)
}

fn header(response: &Response<Body>, name: http::header::HeaderName) -> &str {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

#[tokio::test]
async fn serve_file_answers_ranges_head_and_conditionals() -> TestResult {
    let f = fixture()?;
    let song = f.flac("a.flac")?;
    let original = std::fs::read(&song.path)?;
    let etag = format!("\"{}.3\"", song.song);

    let full = f
        .media
        .serve_file(&song, &get(&[]), Disposition::Inline)
        .await?;
    assert_eq!(full.status(), StatusCode::OK);
    assert_eq!(header(&full, CONTENT_TYPE), "audio/flac");
    assert_eq!(header(&full, ACCEPT_RANGES), "bytes");
    assert_eq!(header(&full, CONTENT_LENGTH), "1000");
    assert_eq!(header(&full, ETAG), etag);
    assert_eq!(body(full).await?, original);

    let probe = f
        .media
        .serve_file(&song, &get(&[("range", "bytes=0-1")]), Disposition::Inline)
        .await?;
    assert_eq!(probe.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(header(&probe, CONTENT_RANGE), "bytes 0-1/1000");
    assert_eq!(header(&probe, CONTENT_LENGTH), "2");
    assert_eq!(body(probe).await?, original[..2]);

    let tail = f
        .media
        .serve_file(
            &song,
            &get(&[("range", "bytes=-10"), ("if-range", &etag)]),
            Disposition::Inline,
        )
        .await?;
    assert_eq!(body(tail).await?, original[990..]);

    let stale = f
        .media
        .serve_file(
            &song,
            &get(&[("range", "bytes=-10"), ("if-range", "\"other\"")]),
            Disposition::Inline,
        )
        .await?;
    assert_eq!(stale.status(), StatusCode::OK);

    let head = f
        .media
        .serve_file(
            &song,
            &parts("HEAD", &[("range", "bytes=10-19")]),
            Disposition::Inline,
        )
        .await?;
    assert_eq!(head.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(header(&head, CONTENT_LENGTH), "10");
    assert!(body(head).await?.is_empty());

    let cached = f
        .media
        .serve_file(
            &song,
            &get(&[("if-none-match", &etag)]),
            Disposition::Inline,
        )
        .await?;
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(header(&cached, ETAG), etag);

    let unsatisfiable = f
        .media
        .serve_file(
            &song,
            &get(&[("range", "bytes=5000-")]),
            Disposition::Inline,
        )
        .await;
    assert!(matches!(
        unsatisfiable,
        Err(MediaError::RangeNotSatisfiable { len: 1000 })
    ));

    let download = f
        .media
        .serve_file(
            &song,
            &get(&[]),
            Disposition::Attachment {
                file_name: "Artist - Song.flac".into(),
            },
        )
        .await?;
    assert!(
        header(&download, CONTENT_DISPOSITION)
            .starts_with("attachment; filename=\"Artist - Song.flac\"")
    );
    Ok(())
}

#[tokio::test]
async fn large_files_stream_in_chunks_to_the_exact_length() -> TestResult {
    let f = fixture()?;
    let bytes: Vec<u8> = (0..(3 * serve::CHUNK + 17))
        .map(|i| (i % 253) as u8)
        .collect();
    let song = f.song("big.flac", &bytes, Container::Flac, Codec::Flac)?;
    let response = f
        .media
        .serve_file(&song, &get(&[("range", "bytes=100-")]), Disposition::Inline)
        .await?;
    assert_eq!(body(response).await?, bytes[100..]);
    Ok(())
}

#[tokio::test]
async fn serve_file_rejects_a_file_changed_since_indexing() -> TestResult {
    let f = fixture()?;
    let song = f.flac("a.flac")?;
    std::fs::write(&song.path, b"retagged and longer than before")?;
    let changed = f
        .media
        .serve_file(&song, &get(&[]), Disposition::Inline)
        .await;
    assert!(
        matches!(changed, Err(MediaError::ContentChanged)),
        "{changed:?}"
    );
    std::fs::remove_file(&song.path)?;
    let gone = f
        .media
        .serve_file(&song, &get(&[]), Disposition::Inline)
        .await;
    assert!(matches!(gone, Err(MediaError::ContentChanged)), "{gone:?}");
    Ok(())
}

fn apple(max: Option<u32>, fallback: bool) -> ClientProfile {
    ClientProfile {
        direct: vec![
            DirectPlay {
                container: Container::Flac,
                codecs: vec![Codec::Flac],
            },
            DirectPlay {
                container: Container::M4a,
                codecs: vec![Codec::Aac, Codec::Alac],
            },
        ],
        hls: true,
        max_bitrate_kbps: max,
        preferred_profile: None,
        allow_original_fallback: fallback,
    }
}

#[tokio::test]
async fn without_ffmpeg_everything_is_direct_and_transcodes_are_refused() -> TestResult {
    let f = fixture_with(&config(), FakeTranscoder::missing())?;
    let song = f.flac("a.flac")?;
    let caps = f.media.capabilities();
    assert_eq!(
        (caps.ffmpeg, caps.hls, caps.profiles.len()),
        (None, false, 0)
    );
    assert_eq!(caps.suggested_cellular_profile, None);
    assert_eq!(
        f.media.decide(&song, &apple(Some(256), false), None),
        Decision::Direct {
            reason: Some(DecisionReason::NoTranscoder)
        }
    );
    assert!(matches!(
        f.media
            .ensure_cached(&song, Profile::Aac256, Priority::Download)
            .await,
        Err(MediaError::NoTranscoder)
    ));
    assert!(matches!(
        f.media
            .progressive_transcode(&song, Profile::Mp3_320, None, false)
            .await,
        Err(MediaError::NoTranscoder)
    ));
    assert!(matches!(
        f.media
            .start_hls(
                HlsInput::File(song.clone()),
                Profile::Aac256,
                Priority::Interactive,
                HlsOwner::default(),
            )
            .await,
        Err(MediaError::NoTranscoder)
    ));
    assert_eq!(f.media.prepare(&[song], Profile::Aac256).await, Vec::new());
    Ok(())
}

#[tokio::test]
async fn ensure_cached_makes_a_range_servable_file() -> TestResult {
    let f = fixture()?;
    let song = f.flac("a.flac")?;
    let made = f
        .media
        .ensure_cached(&song, Profile::Aac256, Priority::Download)
        .await?;
    assert_eq!(made.size, 4096);
    assert_eq!(made.format.container, Container::M4a);
    assert_eq!(made.format.content_type, "audio/mp4");
    assert!(
        made.etag
            .starts_with(&format!("\"{}.3.aac-256.", song.song))
    );
    assert_eq!(
        f.media.cached(&song, Profile::Aac256).await,
        Some(made.clone())
    );
    assert!(matches!(
        f.media.decide(&song, &apple(Some(320), true), None),
        Decision::File {
            reason: DecisionReason::OverBitrate,
            ..
        }
    ));
    let spec = &f.fake.spawned()[0];
    let expected = FakeTranscoder::output_for(spec, 4096);
    let partial = f
        .media
        .serve_transcode(
            &made,
            &get(&[("range", "bytes=10-19"), ("if-range", &made.etag)]),
            Disposition::Inline,
        )
        .await?;
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(header(&partial, ETAG), made.etag);
    assert_eq!(body(partial).await?, expected[10..20]);
    assert!(!made.path.with_extension("m4a.part").exists());
    Ok(())
}

#[tokio::test]
async fn cache_survives_a_restart_and_drops_partial_files() -> TestResult {
    let f = fixture()?;
    let song = f.flac("a.flac")?;
    let made = f
        .media
        .ensure_cached(&song, Profile::Aac128, Priority::Download)
        .await?;
    let stray = made.path.with_file_name("junk.part");
    std::fs::write(&stray, b"half")?;
    let again = Media::with_free_space(
        &config(),
        &f.dir.path().join("cache"),
        f.db.db.clone(),
        f.fake.clone(),
        f.db.clock.clone(),
        f.free.clone(),
    )?;
    assert_eq!(
        again.cached(&song, Profile::Aac128).await.map(|c| c.etag),
        Some(made.etag)
    );
    assert!(!stray.exists());
    Ok(())
}

#[tokio::test]
async fn concurrent_requests_share_one_job() -> TestResult {
    let f = fixture()?;
    let song = f.flac("a.flac")?;
    f.fake.hold();
    let (a, b, release) = tokio::join!(
        f.media
            .ensure_cached(&song, Profile::Aac256, Priority::Download),
        f.media
            .ensure_cached(&song, Profile::Aac256, Priority::Prefetch),
        async {
            while f.fake.spawned().is_empty() {
                tokio::task::yield_now().await;
            }
            f.fake.release(1);
        }
    );
    let () = release;
    assert_eq!(a?.etag, b?.etag);
    assert_eq!(f.fake.spawned().len(), 1);
    Ok(())
}

#[tokio::test]
async fn priority_order_is_respected() -> TestResult {
    let mut cfg = config();
    cfg.transcode_max_concurrent = 1;
    let f = fixture_with(&cfg, FakeTranscoder::new())?;
    let first = f.flac("first.flac")?;
    let download = f.flac("download.flac")?;
    let prefetch = f.flac("prefetch.flac")?;
    f.fake.hold();
    let (first_done, download_done, prefetch_done, ()) = tokio::join!(
        f.media
            .ensure_cached(&first, Profile::Aac256, Priority::Download),
        async {
            while f.fake.spawned().is_empty() {
                tokio::task::yield_now().await;
            }
            f.media
                .ensure_cached(&download, Profile::Aac256, Priority::Download)
                .await
        },
        async {
            while f.media.shared.slots.waiting() < 1 {
                tokio::task::yield_now().await;
            }
            f.media
                .ensure_cached(&prefetch, Profile::Aac256, Priority::Prefetch)
                .await
        },
        async {
            while f.media.shared.slots.waiting() < 2 {
                tokio::task::yield_now().await;
            }
            f.fake.release(3);
        }
    );
    first_done?;
    download_done?;
    prefetch_done?;
    let order: Vec<PathBuf> = f
        .fake
        .spawned()
        .into_iter()
        .filter_map(|s| match s.input {
            TranscodeInput::Path(p) => Some(p),
            TranscodeInput::Stdin => None,
        })
        .collect();
    assert_eq!(order, [first.path, prefetch.path, download.path]);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn busy_transcoder_falls_back_or_refuses() -> TestResult {
    let mut cfg = config();
    cfg.transcode_max_concurrent = 1;
    let f = fixture_with(&cfg, FakeTranscoder::new())?;
    let running = f.flac("running.flac")?;
    let other = f.flac("other.flac")?;
    f.fake.hold();
    let background = f
        .media
        .prepare(std::slice::from_ref(&running), Profile::Aac256)
        .await;
    assert_eq!(background.len(), 1);
    while f.fake.spawned().is_empty() {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        f.media.decide(&other, &apple(Some(320), true), None),
        Decision::Direct {
            reason: Some(DecisionReason::TranscoderBusy)
        }
    );
    assert!(matches!(
        f.media.decide(&other, &apple(Some(320), false), None),
        Decision::Hls {
            profile: Profile::Aac256,
            ..
        }
    ));
    let refused = f
        .media
        .start_hls(
            HlsInput::File(other.clone()),
            Profile::Aac256,
            Priority::Interactive,
            HlsOwner::default(),
        )
        .await;
    assert!(
        matches!(refused, Err(MediaError::TranscoderBusy)),
        "{refused:?}"
    );
    let refused = f
        .media
        .ensure_cached(&other, Profile::Aac256, Priority::Interactive)
        .await;
    assert!(
        matches!(refused, Err(MediaError::TranscoderBusy)),
        "{refused:?}"
    );
    f.fake.release(10);
    Ok(())
}

#[tokio::test]
async fn repeated_failures_hold_a_song_back() -> TestResult {
    let f = fixture()?;
    let song = f.flac("bad.flac")?;
    f.fake.fail_next(3);
    for _ in 0..3 {
        let failed = f
            .media
            .ensure_cached(&song, Profile::Aac256, Priority::Download)
            .await;
        assert!(matches!(failed, Err(MediaError::Process(_))), "{failed:?}");
    }
    assert_eq!(
        f.media.decide(&song, &apple(Some(320), true), None),
        Decision::Direct {
            reason: Some(DecisionReason::NoTranscoder)
        }
    );
    let held = f
        .media
        .ensure_cached(&song, Profile::Aac256, Priority::Download)
        .await;
    assert!(matches!(held, Err(MediaError::Process(_))));
    assert_eq!(f.fake.spawned().len(), 3);
    f.db.clock.advance(Duration::from_secs(3601));
    assert!(
        f.media
            .ensure_cached(&song, Profile::Aac256, Priority::Download)
            .await
            .is_ok()
    );
    Ok(())
}

#[tokio::test]
async fn cache_evicts_least_recently_used() -> TestResult {
    let mut cfg = config();
    cfg.transcode_cache_max = 3 * 4096 + 100;
    let f = fixture_with(&cfg, FakeTranscoder::new())?;
    let songs: Vec<SongFile> = (0..4)
        .map(|i| f.flac(&format!("{i}.flac")))
        .collect::<Result<_, _>>()?;
    for song in &songs[..3] {
        f.media
            .ensure_cached(song, Profile::Flac, Priority::Download)
            .await?;
    }
    assert!(f.media.cached(&songs[0], Profile::Flac).await.is_some());
    f.media
        .ensure_cached(&songs[3], Profile::Flac, Priority::Download)
        .await?;
    assert!(f.media.cached(&songs[1], Profile::Flac).await.is_none());
    for kept in [0, 2, 3] {
        assert!(
            f.media.cached(&songs[kept], Profile::Flac).await.is_some(),
            "song {kept}"
        );
    }
    assert_eq!(f.media.transcode_cache_bytes(), 3 * 4096);
    assert_eq!(
        std::fs::read_dir(f.dir.path().join("cache/transcodes"))?.count(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn cache_stays_above_min_free() -> TestResult {
    let mut cfg = config();
    cfg.cache_min_free = 1 << 20;
    let f = fixture_with(&cfg, FakeTranscoder::new())?;
    // aac-256 for one second is estimated at 32000 bytes; the fake writes 4096.
    f.free
        .capacity
        .store((1 << 20) + 32_000 + 6_000, Ordering::SeqCst);
    let songs: Vec<SongFile> = (0..3)
        .map(|i| f.flac(&format!("{i}.flac")))
        .collect::<Result<_, _>>()?;
    f.media
        .ensure_cached(&songs[0], Profile::Aac256, Priority::Download)
        .await?;
    f.media
        .ensure_cached(&songs[1], Profile::Aac256, Priority::Download)
        .await?;
    f.media
        .ensure_cached(&songs[2], Profile::Aac256, Priority::Download)
        .await?;
    assert!(f.media.cached(&songs[0], Profile::Aac256).await.is_none());
    assert!(f.media.cached(&songs[2], Profile::Aac256).await.is_some());

    f.free.capacity.store(1 << 20, Ordering::SeqCst);
    let full = f
        .media
        .ensure_cached(&songs[0], Profile::Aac256, Priority::Download)
        .await;
    assert!(
        matches!(full, Err(MediaError::InsufficientStorage)),
        "{full:?}"
    );
    let streamed = f
        .media
        .progressive_transcode(&songs[0], Profile::Mp3_320, None, false)
        .await?;
    assert_eq!(body(streamed).await?.len(), 4096);
    assert!(f.media.cached(&songs[0], Profile::Mp3_320).await.is_none());
    Ok(())
}

#[tokio::test]
async fn prepare_reports_cached_and_queued() -> TestResult {
    let f = fixture()?;
    let a = f.flac("a.flac")?;
    let b = f.flac("b.flac")?;
    f.media
        .ensure_cached(&a, Profile::Aac256, Priority::Download)
        .await?;
    f.fake.hold();
    let states = f
        .media
        .prepare(&[a.clone(), b.clone()], Profile::Aac256)
        .await;
    assert_eq!(
        states[0],
        PrepareState {
            song: a.song.clone(),
            state: PrepareStateKind::Cached
        }
    );
    assert_eq!(states[1].song, b.song);
    assert_ne!(states[1].state, PrepareStateKind::Cached);
    f.fake.release(1);
    let done = f
        .media
        .ensure_cached(&b, Profile::Aac256, Priority::Prefetch)
        .await?;
    assert_eq!(done.size, 4096);
    Ok(())
}

#[tokio::test]
async fn progressive_mp3_streams_while_it_is_cached() -> TestResult {
    let f = fixture()?;
    let song = f.flac("a.flac")?;
    let response = f
        .media
        .progressive_transcode(&song, Profile::Mp3_320, None, false)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, CONTENT_TYPE), "audio/mpeg");
    assert_eq!(header(&response, ACCEPT_RANGES), "none");
    let streamed = body(response).await?;
    let cached = f
        .media
        .ensure_cached(&song, Profile::Mp3_320, Priority::Interactive)
        .await?;
    assert_eq!(std::fs::read(&cached.path)?, streamed);
    assert_eq!(f.fake.spawned().len(), 1);

    let estimated = f
        .media
        .progressive_transcode(&song, Profile::Mp3_320, None, true)
        .await?;
    assert_eq!(header(&estimated, CONTENT_LENGTH), "4096");
    Ok(())
}

#[tokio::test]
async fn progressive_with_an_offset_is_not_cached() -> TestResult {
    let f = fixture()?;
    let song = f.flac("a.flac")?;
    let response = f
        .media
        .progressive_transcode(&song, Profile::Mp3_128, Some(Duration::from_secs(30)), true)
        .await?;
    assert_eq!(header(&response, CONTENT_LENGTH), "16000");
    let streamed = body(response).await;
    assert!(
        streamed.is_err(),
        "a body shorter than its estimate ends with an error"
    );
    let spec = &f.fake.spawned()[0];
    assert_eq!(spec.output, TranscodeOutput::Stdout);
    assert_eq!(spec.offset, Some(Duration::from_secs(30)));
    assert!(f.media.cached(&song, Profile::Mp3_128).await.is_none());
    Ok(())
}

#[tokio::test]
async fn progressive_aac_is_fragmented_stdout() -> TestResult {
    let f = fixture()?;
    let song = f.flac("a.flac")?;
    let response = f
        .media
        .progressive_transcode(&song, Profile::Aac256, None, false)
        .await?;
    assert_eq!(header(&response, CONTENT_TYPE), "audio/mp4");
    assert_eq!(body(response).await?.len(), 4096);
    assert_eq!(f.fake.spawned()[0].output, TranscodeOutput::Stdout);
    Ok(())
}

#[tokio::test]
async fn a_job_that_dies_fails_its_waiters_and_the_next_request_starts_afresh() -> TestResult {
    let f = fixture()?;
    let song = f.flac("a.flac")?;
    f.fake.panic_next();
    let first = tokio::time::timeout(
        Duration::from_secs(5),
        f.media
            .ensure_cached(&song, Profile::Aac128, Priority::Download),
    )
    .await?;
    assert!(first.is_err());
    let second = f
        .media
        .ensure_cached(&song, Profile::Aac128, Priority::Download)
        .await?;
    assert!(second.size > 0);
    assert_eq!(f.media.transcode_load().0, 0);
    Ok(())
}

#[tokio::test]
async fn a_stuck_progressive_transcode_gives_its_slot_back_when_the_client_leaves() -> TestResult {
    let f = fixture()?;
    let song = f.flac("a.flac")?;
    f.fake.stall_stdout();
    let response = f
        .media
        .progressive_transcode(&song, Profile::Aac256, None, false)
        .await?;
    assert_eq!(f.media.transcode_load().0, 1);
    drop(response);
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.media.transcode_load().0 != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

fn hls_song(
    f: &Fixture,
    name: &str,
    duration_ms: u32,
) -> Result<SongFile, Box<dyn std::error::Error>> {
    let mut song = f.flac(name)?;
    song.duration_ms = duration_ms;
    Ok(song)
}

#[tokio::test(start_paused = true)]
async fn hls_playlist_has_all_segments_before_encoding_finishes() -> TestResult {
    let f = fixture()?;
    f.fake.set_hls(FakeHls::Manual);
    let song = hls_song(&f, "long.flac", 61_000)?;
    let job = f
        .media
        .start_hls(
            HlsInput::File(song),
            Profile::Aac256,
            Priority::Interactive,
            HlsOwner::default(),
        )
        .await?;
    let playlist = f.media.hls_playlist(&job, "?mt=abc").await?;
    assert!(playlist.starts_with("#EXTM3U\n#EXT-X-VERSION:7\n"));
    assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
    assert!(playlist.contains("#EXT-X-MAP:URI=\"init.mp4?mt=abc\""));
    assert_eq!(playlist.matches(".m4s?mt=abc").count(), 11);
    assert!(playlist.contains("#EXTINF:1.000,\nseg-10.m4s?mt=abc"));
    assert!(playlist.ends_with("#EXT-X-ENDLIST\n"));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn segment_wait_times_out_with_503() -> TestResult {
    let f = fixture()?;
    f.fake.set_hls(FakeHls::Manual);
    let song = hls_song(&f, "a.flac", 30_000)?;
    let job = f
        .media
        .start_hls(
            HlsInput::File(song),
            Profile::Aac256,
            Priority::Interactive,
            HlsOwner::default(),
        )
        .await?;
    let started = tokio::time::Instant::now();
    let waited = f.media.hls_part(&job, HlsPart::Segment(0), &get(&[])).await;
    assert!(matches!(waited, Err(MediaError::Timeout)), "{waited:?}");
    assert!(started.elapsed() >= hls::PART_WAIT);
    let past = f
        .media
        .hls_part(&job, HlsPart::Segment(99), &get(&[]))
        .await;
    assert!(matches!(past, Err(MediaError::NotFound)));
    let unknown = f.media.hls_playlist(&HlsJobId("hnope".into()), "").await;
    assert!(matches!(unknown, Err(MediaError::JobExpired)));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn seek_far_ahead_starts_a_second_job() -> TestResult {
    let f = fixture()?;
    f.fake.set_hls(FakeHls::Manual);
    let song = hls_song(&f, "a.flac", 120_000)?;
    let job = f
        .media
        .start_hls(
            HlsInput::File(song),
            Profile::Aac256,
            Priority::Interactive,
            HlsOwner::default(),
        )
        .await?;
    let request = get(&[]);
    let (served, ()) = tokio::join!(
        f.media.hls_part(&job, HlsPart::Segment(8), &request),
        async {
            while f.fake.spawned().len() < 2 {
                tokio::task::yield_now().await;
            }
            let spec = &f.fake.spawned()[1];
            assert_eq!(spec.offset, Some(Duration::from_secs(48)));
            if let TranscodeOutput::Hls {
                dir, start_number, ..
            } = &spec.output
            {
                assert_eq!(*start_number, 8);
                let _ = std::fs::write(dir.join("seg-8.m4s"), b"segment 8");
            }
        }
    );
    let served = served?;
    assert_eq!(header(&served, CONTENT_TYPE), "audio/mp4");
    assert_eq!(body(served).await?, &b"segment 8"[..]);
    let near = tokio::time::timeout(
        Duration::from_secs(1),
        f.media.hls_part(&job, HlsPart::Segment(2), &request),
    )
    .await;
    assert!(near.is_err(), "segment 2 waits for the first run");
    assert_eq!(f.fake.spawned().len(), 2);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_complete_hls_run_caches_a_file_transcode_with_its_edit_list() -> TestResult {
    let f = fixture()?;
    f.fake.set_hls(FakeHls::Produce(3));
    let song = hls_song(&f, "a.flac", 15_000)?;
    let job = f
        .media
        .start_hls(
            HlsInput::File(song.clone()),
            Profile::Aac256,
            Priority::Interactive,
            HlsOwner::default(),
        )
        .await?;
    let init = f.media.hls_part(&job, HlsPart::Init, &get(&[])).await?;
    assert_eq!(body(init).await?, &b"init"[..]);
    let seg = f
        .media
        .hls_part(&job, HlsPart::Segment(2), &get(&[]))
        .await?;
    assert_eq!(body(seg).await?, &b"segment 2"[..]);
    let mut cached = None;
    for _ in 0..100 {
        cached = f.media.cached(&song, Profile::Aac256).await;
        if cached.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let cached = cached.ok_or("never cached")?;
    assert!(!cached.etag.ends_with("h\""), "{}", cached.etag);
    let spawned = f.fake.spawned();
    assert!(
        spawned
            .iter()
            .any(|s| matches!(s.output, TranscodeOutput::File(_)) && s.offset.is_none()),
        "{spawned:?}"
    );
    assert!(
        !spawned
            .iter()
            .any(|s| matches!(s.output, TranscodeOutput::Remux(_)))
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_run_still_finishing_after_its_last_segment_is_not_stopped() -> TestResult {
    let f = fixture()?;
    f.fake.hold();
    f.fake.set_hls(FakeHls::ProduceThenLinger(3));
    let song = hls_song(&f, "a.flac", 15_000)?;
    let job = f
        .media
        .start_hls(
            HlsInput::File(song.clone()),
            Profile::Aac256,
            Priority::Interactive,
            HlsOwner::default(),
        )
        .await?;
    let seg = f
        .media
        .hls_part(&job, HlsPart::Segment(2), &get(&[]))
        .await?;
    assert_eq!(body(seg).await?, &b"segment 2"[..]);
    // Many polls see every segment while the encoder is still running.
    tokio::time::sleep(Duration::from_secs(5)).await;
    f.fake.release(tokio::sync::Semaphore::MAX_PERMITS / 2);
    let mut cached = None;
    for _ in 0..100 {
        cached = f.media.cached(&song, Profile::Aac256).await;
        if cached.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    cached.ok_or("the finished run was not cached")?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn idle_hls_jobs_expire() -> TestResult {
    let f = fixture()?;
    f.fake.set_hls(FakeHls::Manual);
    let song = hls_song(&f, "a.flac", 30_000)?;
    let job = f
        .media
        .start_hls(
            HlsInput::File(song),
            Profile::Aac256,
            Priority::Interactive,
            HlsOwner::default(),
        )
        .await?;
    let dir = f.dir.path().join("cache/hls").join(&*job.0);
    assert!(dir.exists());
    tokio::time::sleep(hls::JOB_IDLE + Duration::from_secs(1)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        f.media.hls_playlist(&job, "").await,
        Err(MediaError::JobExpired)
    ));
    assert_eq!(f.media.shared.slots.free(), 2);
    Ok(())
}

#[tokio::test]
async fn growing_files_stream_without_length_until_complete() -> TestResult {
    let f = fixture()?;
    let path = f.dir.path().join("growing.mp3");
    std::fs::write(&path, b"first ")?;
    let (tx, rx) = tokio::sync::watch::channel(GrowState::Growing { bytes: 6 });
    let growing = GrowingFile {
        path: path.clone(),
        state: rx,
    };
    let response = f
        .media
        .serve_growing(&growing, &get(&[("range", "bytes=3-")]), "audio/mpeg")
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, ACCEPT_RANGES), "none");
    assert_eq!(header(&response, CONTENT_LENGTH), "");
    let (streamed, ()) = tokio::join!(body(response), async {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&path) {
            let _ = file.write_all(b"second");
        }
        let _ = tx.send(GrowState::Complete { bytes: 12 });
    });
    assert_eq!(streamed?, &b"first second"[..]);

    let complete = f
        .media
        .serve_growing(&growing, &get(&[("range", "bytes=6-")]), "audio/mpeg")
        .await?;
    assert_eq!(complete.status(), StatusCode::PARTIAL_CONTENT);
    assert!(header(&complete, ETAG).starts_with("\"g"));
    assert_eq!(body(complete).await?, &b"second"[..]);
    Ok(())
}

#[tokio::test]
async fn growing_file_failure_ends_the_body_with_an_error() -> TestResult {
    let f = fixture()?;
    let path = f.dir.path().join("growing.mp3");
    std::fs::write(&path, b"partial")?;
    let (tx, rx) = tokio::sync::watch::channel(GrowState::Growing { bytes: 7 });
    let growing = GrowingFile { path, state: rx };
    let response = f
        .media
        .serve_growing(&growing, &get(&[]), "audio/mpeg")
        .await?;
    let (streamed, ()) = tokio::join!(body(response), async {
        let _ = tx.send(GrowState::Failed);
    });
    assert!(streamed.is_err());
    assert!(matches!(
        f.media
            .serve_growing(&growing, &get(&[]), "audio/mpeg")
            .await,
        Err(MediaError::NotFound)
    ));
    Ok(())
}

#[tokio::test]
async fn hls_from_a_growing_file_feeds_stdin() -> TestResult {
    let f = fixture()?;
    f.fake.set_hls(FakeHls::Produce(2));
    let path = f.dir.path().join("growing.webm");
    std::fs::write(&path, b"bytes")?;
    let (_tx, rx) = tokio::sync::watch::channel(GrowState::Complete { bytes: 5 });
    let job = f
        .media
        .start_hls(
            HlsInput::Growing {
                file: GrowingFile { path, state: rx },
                duration_ms: 12_000,
            },
            Profile::Aac256,
            Priority::Interactive,
            HlsOwner::default(),
        )
        .await?;
    let seg = f
        .media
        .hls_part(&job, HlsPart::Segment(1), &get(&[]))
        .await?;
    assert_eq!(body(seg).await?, &b"segment 1"[..]);
    assert_eq!(f.fake.spawned()[0].input, TranscodeInput::Stdin);
    Ok(())
}

#[tokio::test]
async fn capabilities_report_the_transcoder() -> TestResult {
    let f = fixture()?;
    let caps = f.media.capabilities();
    assert_eq!(caps.ffmpeg.as_deref(), Some("fake 1.0"));
    assert!(caps.hls);
    assert_eq!(caps.profiles, Profile::ALL.to_vec());
    assert_eq!(caps.suggested_cellular_profile, Some(Profile::Aac256));
    assert_eq!(
        f.media.artwork_store().dir(),
        f.dir.path().join("cache/art")
    );
    Ok(())
}

#[test]
fn profiles_round_trip_their_names() {
    for profile in Profile::ALL {
        assert_eq!(Profile::parse(profile.as_str()), Some(profile));
    }
    assert_eq!(Profile::parse("AAC-256"), Some(Profile::Aac256));
    assert_eq!(Profile::parse("wma"), None);
    assert_eq!(Profile::Flac.bitrate_kbps(), None);
}

fn tiny_png() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let img = image::RgbImage::from_pixel(8, 8, image::Rgb([200, 30, 90]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

#[tokio::test]
async fn lost_artwork_is_restored_from_its_source_file() -> TestResult {
    use trackside_core::repo::artwork::{
        self as art_repo, ArtworkSourceKind as RowKind, NewArtwork,
    };
    let f = fixture()?;
    let png = tiny_png()?;
    let cover = f.dir.path().join("music/cover.png");
    std::fs::write(&cover, &png)?;
    let meta = f.media.artwork_store().put_original(
        &png,
        ArtworkSource {
            kind: ArtworkSourceKind::Sidecar,
            path: Some(cover.clone()),
        },
    )?;
    let row = NewArtwork {
        id: meta.id.clone(),
        mime: meta.mime.to_owned(),
        width: meta.width,
        height: meta.height,
        bytes: meta.bytes,
        source: RowKind::Sidecar,
        source_path: Some(cover.clone()),
    };
    f.db.db.write(move |tx| art_repo::put(tx, &row)).await?;
    std::fs::remove_file(f.media.artwork_store().original_path(&meta.id))?;

    let response = f.media.artwork(&meta.id, None, &get(&[])).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await?.as_ref(), png.as_slice());
    assert!(f.media.artwork_store().has(&meta.id));

    std::fs::remove_file(f.media.artwork_store().original_path(&meta.id))?;
    std::fs::write(&cover, b"a different picture now")?;
    let gone = f.media.artwork(&meta.id, None, &get(&[])).await;
    assert!(matches!(gone, Err(MediaError::NotFound)), "{gone:?}");
    Ok(())
}

/// Reading a lost original back from a library file goes through that library's semaphore, so
/// a hung share answers "not found" quickly instead of taking another blocking thread.
#[tokio::test]
async fn restoring_artwork_waits_its_turn_on_the_library() -> TestResult {
    use trackside_core::repo::artwork::{
        self as art_repo, ArtworkSourceKind as RowKind, NewArtwork,
    };
    use trackside_core::repo::libraries::{self, NewLibrary};
    let f = fixture()?;
    let root = f.dir.path().join("music");
    let library =
        f.db.db
            .write({
                let root = root.clone();
                move |tx| {
                    libraries::create(
                        tx,
                        &NewLibrary {
                            name: String::from("Music"),
                            root,
                            writable: false,
                            watch: false,
                            grant_all_users: true,
                            granted_by: None,
                        },
                    )
                }
            })
            .await?;
    let png = tiny_png()?;
    let cover = root.join("cover.png");
    std::fs::write(&cover, &png)?;
    assert_eq!(f.media.library_of(&cover).await, Some(library.key));
    assert_eq!(f.media.library_of(f.dir.path()).await, None);
    let meta = f.media.artwork_store().put_original(
        &png,
        ArtworkSource {
            kind: ArtworkSourceKind::Sidecar,
            path: Some(cover.clone()),
        },
    )?;
    let row = NewArtwork {
        id: meta.id.clone(),
        mime: meta.mime.to_owned(),
        width: meta.width,
        height: meta.height,
        bytes: meta.bytes,
        source: RowKind::Sidecar,
        source_path: Some(cover.clone()),
    };
    f.db.db.write(move |tx| art_repo::put(tx, &row)).await?;
    std::fs::remove_file(f.media.artwork_store().original_path(&meta.id))?;

    let stuck = f
        .media
        .libraries
        .semaphore(library.key)
        .acquire_many_owned(u32::try_from(gate::LIBRARY_PERMITS)?)
        .await?;
    let started = std::time::Instant::now();
    let refused = f.media.artwork(&meta.id, None, &get(&[])).await;
    assert!(matches!(refused, Err(MediaError::NotFound)), "{refused:?}");
    assert!(started.elapsed() < gate::LIBRARY_WAIT * 3);
    assert!(!f.media.artwork_store().has(&meta.id));
    drop(stuck);
    let response = f.media.artwork(&meta.id, None, &get(&[])).await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_hls_job_names_its_owner_and_gives_back_the_hold_when_the_encoder_stops() -> TestResult {
    let f = fixture()?;
    f.fake.set_hls(FakeHls::Produce(3));
    let song = hls_song(&f, "owned.flac", 15_000)?;
    let slot = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = Arc::clone(&slot).try_acquire_owned()?;
    let job = f
        .media
        .start_hls(
            HlsInput::File(song.clone()),
            Profile::Aac256,
            Priority::Interactive,
            HlsOwner {
                user: Some(trackside_core::UserKey(9)),
                hold: Some(Box::new(permit)),
            },
        )
        .await?;
    assert_eq!(
        f.media.hls_job(&job),
        Some(HlsJobInfo {
            user: Some(trackside_core::UserKey(9)),
            song: Some(song.song.clone()),
        })
    );
    assert_eq!(slot.available_permits(), 0);
    let init = f.media.hls_part(&job, HlsPart::Init, &get(&[])).await?;
    assert_eq!(body(init).await?, &b"init"[..]);
    let seg = f
        .media
        .hls_part(&job, HlsPart::Segment(2), &get(&[]))
        .await?;
    assert_eq!(body(seg).await?, &b"segment 2"[..]);
    for _ in 0..100 {
        if slot.available_permits() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(slot.available_permits(), 1);
    assert_eq!(f.media.hls_job(&HlsJobId("hnope".into())), None);
    Ok(())
}

#[tokio::test]
async fn a_held_guard_lives_until_the_body_is_read_or_dropped() -> TestResult {
    let slot = Arc::new(tokio::sync::Semaphore::new(1));
    let read = hold(
        Response::new(Body::from("abc")),
        Arc::clone(&slot).try_acquire_owned()?,
    );
    assert_eq!(slot.available_permits(), 0);
    assert_eq!(body(read).await?, &b"abc"[..]);
    assert_eq!(slot.available_permits(), 1);
    let dropped = hold(
        Response::new(Body::from("abc")),
        Arc::clone(&slot).try_acquire_owned()?,
    );
    assert_eq!(slot.available_permits(), 0);
    drop(dropped);
    assert_eq!(slot.available_permits(), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_segment_asked_for_before_the_init_waits_for_the_finished_run() -> TestResult {
    let f = fixture()?;
    f.fake.set_hls(FakeHls::Produce(3));
    let song = hls_song(&f, "first.flac", 15_000)?;
    let job = f
        .media
        .start_hls(
            HlsInput::File(song),
            Profile::Aac256,
            Priority::Interactive,
            HlsOwner::default(),
        )
        .await?;
    let seg = tokio::time::timeout(
        Duration::from_secs(5),
        f.media.hls_part(&job, HlsPart::Segment(2), &get(&[])),
    )
    .await??;
    assert_eq!(body(seg).await?, &b"segment 2"[..]);
    assert_eq!(f.media.hls_run_count(&job), Some(1));
    Ok(())
}
