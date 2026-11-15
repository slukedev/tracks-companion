//! Tests against the real ffmpeg. They skip with a message when ffmpeg is not installed, and
//! fail instead when `TRACKSIDE_TEST_FFMPEG=1` says it must be (CI).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use trackside_core::records::{AudioFormat, Codec, Container, SongFile};
use trackside_core::testing::TestDb;
use trackside_core::{LibraryKey, PublicId, SongId};
use trackside_media::{FfmpegTranscoder, HlsInput, HlsPart, Media, Priority, Profile, Transcoder};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn ffmpeg() -> Option<FfmpegTranscoder> {
    let found = FfmpegTranscoder::detect(None);
    if found.is_none() {
        assert!(
            std::env::var("TRACKSIDE_TEST_FFMPEG").as_deref() != Ok("1"),
            "TRACKSIDE_TEST_FFMPEG=1 but ffmpeg was not found"
        );
        eprintln!("skipping: ffmpeg not found");
    }
    found
}

fn generate(
    ffmpeg: &FfmpegTranscoder,
    out: &Path,
    seconds: u32,
    extra: &[&str],
) -> std::io::Result<()> {
    let source = format!("sine=frequency=440:duration={seconds}:sample_rate=44100");
    let status = Command::new(ffmpeg.path())
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &source,
        ])
        .args(extra)
        .arg(out)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "ffmpeg could not generate {}",
            out.display()
        )))
    }
}

fn song_file(path: &Path, duration_ms: u32) -> std::io::Result<SongFile> {
    let meta = std::fs::metadata(path)?;
    let mtime_ns = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64);
    Ok(SongFile {
        song: SongId::from_hash(blake3::hash(path.as_os_str().as_encoded_bytes()).as_bytes()),
        library: LibraryKey(1),
        path: path.to_path_buf(),
        size: meta.len(),
        mtime_ns,
        content_version: 1,
        format: AudioFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            content_type: "audio/flac",
            lossless: true,
            bitrate_kbps: None,
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            size: meta.len(),
        },
        duration_ms,
    })
}

fn media(
    cache: &Path,
    transcoder: FfmpegTranscoder,
) -> Result<(Media, TestDb), Box<dyn std::error::Error>> {
    let db = TestDb::new()?;
    let mut config = trackside_core::Config::defaults("/data").media;
    config.cache_min_free = 0;
    let media = Media::new(
        &config,
        cache,
        db.db.clone(),
        Arc::new(transcoder),
        db.clock.clone(),
    )?;
    Ok((media, db))
}

fn parts() -> http::request::Parts {
    http::Request::new(()).into_parts().0
}

struct Setup {
    dir: tempfile::TempDir,
    flac: PathBuf,
    song: SongFile,
    ffmpeg: FfmpegTranscoder,
}

fn setup(seconds: u32) -> Result<Option<Setup>, Box<dyn std::error::Error>> {
    let Some(ffmpeg) = ffmpeg() else {
        return Ok(None);
    };
    let dir = tempfile::tempdir()?;
    let flac = dir.path().join("tone.flac");
    generate(&ffmpeg, &flac, seconds, &["-ac", "2", "-c:a", "flac"])?;
    let song = song_file(&flac, seconds * 1000)?;
    Ok(Some(Setup {
        dir,
        flac,
        song,
        ffmpeg,
    }))
}

#[tokio::test]
async fn aac_256_output_is_bit_exact_across_runs() -> TestResult {
    let Some(s) = setup(8)? else { return Ok(()) };
    let (first, _db1) = media(&s.dir.path().join("cache1"), s.ffmpeg.clone())?;
    let (second, _db2) = media(&s.dir.path().join("cache2"), s.ffmpeg.clone())?;
    let a = first
        .ensure_cached(&s.song, Profile::Aac256, Priority::Download)
        .await?;
    let b = second
        .ensure_cached(&s.song, Profile::Aac256, Priority::Download)
        .await?;
    assert_eq!(a.etag, b.etag);
    assert_eq!(std::fs::read(&a.path)?, std::fs::read(&b.path)?);
    let probe = first.probe(&a.path).await?;
    assert_eq!((probe.container, probe.codec), (Container::M4a, Codec::Aac));
    assert!(
        probe.duration_ms.abs_diff(8000) < 100,
        "{}",
        probe.duration_ms
    );
    assert_eq!(probe.sample_rate, Some(44_100));
    Ok(())
}

#[tokio::test]
async fn m4a_has_an_edit_list_for_priming() -> TestResult {
    let Some(s) = setup(3)? else { return Ok(()) };
    let (media, _db) = media(&s.dir.path().join("cache"), s.ffmpeg.clone())?;
    let made = media
        .ensure_cached(&s.song, Profile::Aac128, Priority::Download)
        .await?;
    let bytes = std::fs::read(&made.path)?;
    let moov = bytes
        .windows(4)
        .position(|w| w == b"moov")
        .ok_or("no moov")?;
    let mdat = bytes
        .windows(4)
        .position(|w| w == b"mdat")
        .ok_or("no mdat")?;
    assert!(moov < mdat, "faststart puts moov first");
    let (segment_ms, media_time) = edit_list(&bytes).ok_or("no edit list")?;
    assert_eq!(media_time, 1024, "the edit skips the AAC encoder's priming");
    assert_eq!(
        segment_ms, 3000,
        "the edit ends where the source does, before the padding"
    );
    Ok(())
}

/// The first entry of the first version 0 `elst` box: (segment duration in the movie
/// timescale, media time in the track timescale).
fn edit_list(bytes: &[u8]) -> Option<(u32, i32)> {
    let at = bytes.windows(4).position(|w| w == b"elst")?;
    let word = |o: usize| -> Option<[u8; 4]> { bytes.get(at + o..at + o + 4)?.try_into().ok() };
    if bytes.get(at + 4) != Some(&0) || u32::from_be_bytes(word(8)?) == 0 {
        return None;
    }
    Some((u32::from_be_bytes(word(12)?), i32::from_be_bytes(word(16)?)))
}

#[tokio::test]
async fn hls_segments_play_back_to_the_full_duration() -> TestResult {
    let Some(s) = setup(20)? else { return Ok(()) };
    let (media, _db) = media(&s.dir.path().join("cache"), s.ffmpeg.clone())?;
    let job = media
        .start_hls(
            HlsInput::File(s.song.clone()),
            Profile::Aac256,
            Priority::Interactive,
            trackside_media::HlsOwner::default(),
        )
        .await?;
    let playlist = media.hls_playlist(&job, "").await?;
    let count = playlist.matches(".m4s").count() as u32;
    assert_eq!(count, 4);
    let mut joined = Vec::new();
    let init = media.hls_part(&job, HlsPart::Init, &parts()).await?;
    joined.extend(axum::body::to_bytes(init.into_body(), usize::MAX).await?);
    for n in 0..count {
        let seg = media.hls_part(&job, HlsPart::Segment(n), &parts()).await?;
        joined.extend(axum::body::to_bytes(seg.into_body(), usize::MAX).await?);
    }
    let path = s.dir.path().join("joined.mp4");
    std::fs::write(&path, joined)?;
    let check = media.decode_check(&path).await?;
    assert!(check.ok, "{:?}", check.message);
    let decoded = check.decoded_ms.ok_or("no progress")?;
    assert!(decoded.abs_diff(20_000) < 150, "decoded {decoded} ms");

    let mut cached = None;
    for _ in 0..200 {
        cached = media.cached(&s.song, Profile::Aac256).await;
        if cached.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let cached = cached.ok_or("the finished run was not remuxed into the cache")?;
    let probe = media.probe(&cached.path).await?;
    assert!(
        probe.duration_ms.abs_diff(20_000) < 150,
        "{}",
        probe.duration_ms
    );
    Ok(())
}

#[tokio::test]
async fn seeking_ahead_produces_the_requested_segment() -> TestResult {
    let Some(s) = setup(60)? else { return Ok(()) };
    let (media, _db) = media(&s.dir.path().join("cache"), s.ffmpeg.clone())?;
    let job = media
        .start_hls(
            HlsInput::File(s.song.clone()),
            Profile::Aac256,
            Priority::Interactive,
            trackside_media::HlsOwner::default(),
        )
        .await?;
    let seg = media.hls_part(&job, HlsPart::Segment(8), &parts()).await?;
    let bytes = axum::body::to_bytes(seg.into_body(), usize::MAX).await?;
    assert!(bytes.windows(4).any(|w| w == b"moof"));
    let first = media.hls_part(&job, HlsPart::Segment(0), &parts()).await?;
    assert_eq!(first.status(), http::StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn progressive_mp3_is_playable() -> TestResult {
    let Some(s) = setup(5)? else { return Ok(()) };
    let (media, _db) = media(&s.dir.path().join("cache"), s.ffmpeg.clone())?;
    let response = media
        .progressive_transcode(&s.song, Profile::Mp3_320, None, false)
        .await?;
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let path = s.dir.path().join("streamed.mp3");
    std::fs::write(&path, &bytes)?;
    let check = media.decode_check(&path).await?;
    assert!(check.ok, "{:?}", check.message);
    let cached = media
        .cached(&s.song, Profile::Mp3_320)
        .await
        .ok_or("not cached")?;
    assert_eq!(cached.size as usize, bytes.len());
    Ok(())
}

#[tokio::test]
async fn probe_reads_each_container() -> TestResult {
    let Some(s) = setup(2)? else { return Ok(()) };
    let (media, _db) = media(&s.dir.path().join("cache"), s.ffmpeg.clone())?;
    let cases: [(&str, &[&str], Container, Codec); 6] = [
        (
            "a.mp3",
            &["-c:a", "libmp3lame", "-b:a", "192k"],
            Container::Mp3,
            Codec::Mp3,
        ),
        ("a.m4a", &["-c:a", "aac"], Container::M4a, Codec::Aac),
        ("b.m4a", &["-c:a", "alac"], Container::M4a, Codec::Alac),
        ("a.opus", &["-c:a", "libopus"], Container::Opus, Codec::Opus),
        ("a.wav", &[], Container::Wav, Codec::Pcm),
        ("a.aiff", &[], Container::Aiff, Codec::Pcm),
    ];
    for (name, args, container, codec) in cases {
        let path = s.dir.path().join(name);
        generate(&s.ffmpeg, &path, 2, args)?;
        let probe = media.probe(&path).await?;
        assert_eq!((probe.container, probe.codec), (container, codec), "{name}");
        assert!(
            probe.duration_ms.abs_diff(2000) < 100,
            "{name}: {}",
            probe.duration_ms
        );
        assert!(!probe.has_video, "{name}");
    }
    let flac = media.probe(&s.flac).await?;
    assert_eq!(
        (flac.container, flac.codec, flac.channels),
        (Container::Flac, Codec::Flac, Some(2))
    );
    Ok(())
}

#[tokio::test]
async fn probe_notices_a_video_track() -> TestResult {
    let Some(s) = setup(1)? else { return Ok(()) };
    let (media, _db) = media(&s.dir.path().join("cache"), s.ffmpeg.clone())?;
    let path = s.dir.path().join("video.mp4");
    let status = Command::new(s.ffmpeg.path())
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x64:rate=5",
        ])
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=duration=1",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(&path)
        .status()?;
    assert!(status.success());
    let probe = media.probe(&path).await?;
    assert!(probe.has_video);
    assert_eq!(probe.codec, Codec::Aac);
    Ok(())
}

#[tokio::test]
async fn decode_check_rejects_a_truncated_flac() -> TestResult {
    let Some(s) = setup(10)? else { return Ok(()) };
    let (media, _db) = media(&s.dir.path().join("cache"), s.ffmpeg.clone())?;
    let good = media.decode_check(&s.flac).await?;
    assert!(good.ok, "{:?}", good.message);
    assert!(good.decoded_ms.is_some_and(|ms| ms.abs_diff(10_000) < 100));
    let bytes = std::fs::read(&s.flac)?;
    let cut = s.dir.path().join("cut.flac");
    std::fs::write(&cut, &bytes[..bytes.len() / 2])?;
    let bad = media.decode_check(&cut).await?;
    assert!(!bad.ok);
    assert!(bad.message.is_some());
    Ok(())
}

#[test]
fn detect_reads_the_version_and_encoders() {
    let Some(ffmpeg) = ffmpeg() else { return };
    assert!(ffmpeg.version().is_some_and(|v| !v.is_empty()));
    assert_eq!(ffmpeg.encoder_id().map(|e| e.len()), Some(6));
    assert!(ffmpeg.profiles().contains(&Profile::Aac256));
}

#[tokio::test]
async fn a_last_segment_asked_for_alongside_the_init_starts_no_second_encoder() -> TestResult {
    let Some(s) = setup(20)? else { return Ok(()) };
    let (media, _db) = media(&s.dir.path().join("cache"), s.ffmpeg.clone())?;
    let job = media
        .start_hls(
            HlsInput::File(s.song.clone()),
            Profile::Aac256,
            Priority::Interactive,
            trackside_media::HlsOwner::default(),
        )
        .await?;
    let request = parts();
    let (init, last) = tokio::join!(
        media.hls_part(&job, HlsPart::Init, &request),
        media.hls_part(&job, HlsPart::Segment(3), &request),
    );
    assert!(
        !axum::body::to_bytes(init?.into_body(), usize::MAX)
            .await?
            .is_empty()
    );
    assert!(
        !axum::body::to_bytes(last?.into_body(), usize::MAX)
            .await?
            .is_empty()
    );
    assert_eq!(media.hls_run_count(&job), Some(1));
    Ok(())
}
