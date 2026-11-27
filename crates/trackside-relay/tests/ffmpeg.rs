//! What the relay serves, read back by the real ffprobe and ffmpeg: the playlists parse as HLS,
//! the init and media segments as FLAC or AAC in fMP4, and the lossless tier decodes to the
//! exact samples it was given. They skip when ffmpeg is missing, and fail instead when
//! `TRACKSIDE_TEST_FFMPEG=1` says it must be there (CI). The files are the synthetic test tone,
//! written to a temporary directory only so the tools can read them.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use bytes::Bytes;
use support::{TONE, connect, eventually, request, rig, send_frames, tone};
use trackside_relay::{
    EndReason, Epoch, Shape, StreamInfo, Tier, Timeline, find_ffmpeg, multivariant, split_frames,
};

fn tools() -> Option<(PathBuf, PathBuf)> {
    let ffmpeg = find_ffmpeg(None);
    let ffprobe = ffmpeg
        .as_ref()
        .map(|f| f.with_file_name("ffprobe"))
        .filter(|p| p.is_file());
    match (ffmpeg, ffprobe) {
        (Some(ffmpeg), Some(ffprobe)) => Some((ffmpeg, ffprobe)),
        _ => {
            assert!(
                std::env::var("TRACKSIDE_TEST_FFMPEG").as_deref() != Ok("1"),
                "TRACKSIDE_TEST_FFMPEG=1 but ffmpeg or ffprobe was not found"
            );
            eprintln!("skipping: ffmpeg not found");
            None
        }
    }
}

fn probe(ffprobe: &Path, input: &Path) -> serde_json::Value {
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(input)
        .output()
        .expect("ffprobe runs");
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

/// Decodes to 16-bit stereo PCM.
fn decode(ffmpeg: &Path, input: &Path) -> Vec<u8> {
    let out = Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(input)
        .args(["-f", "s16le", "-ac", "2", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

struct Past;

impl Timeline for Past {
    fn to_local(&self, server_us: i64) -> Option<i64> {
        Some(server_us)
    }
    fn now_local(&self) -> i64 {
        i64::MAX / 4
    }
    fn wall_ms(&self) -> i64 {
        1_763_112_600_000
    }
}

fn write_epoch(dir: &Path, master: &str, playlist: &str, init: &[u8], segments: &[(u64, Bytes)]) {
    std::fs::write(dir.join("master.m3u8"), master).expect("write");
    std::fs::write(dir.join("index.m3u8"), playlist).expect("write");
    std::fs::write(dir.join("init.mp4"), init).expect("write");
    for (sequence, bytes) in segments {
        std::fs::write(dir.join(format!("{sequence}.m4s")), bytes).expect("write");
    }
}

#[test]
fn the_lossless_tier_is_flac_in_fmp4_and_decodes_bit_exact() {
    let Some((ffmpeg, ffprobe)) = tools() else {
        return;
    };
    let (header, frames) = tone();
    let info = StreamInfo::from_codec_header(&header).expect("info");
    let mut epoch = Epoch::new(1, Tier::Lossless, info, Shape::default(), 256_000);
    let mut ts = 0;
    for frame in &frames {
        let parsed = split_frames(frame).expect("frame");
        epoch.push_flac(ts, parsed);
        ts += support::FRAME_US;
    }
    let published = epoch.publish_due(&Past);
    assert_eq!(published, 5);
    epoch.end();
    let dir = tempfile::tempdir().expect("tempdir");
    let segments: Vec<_> = (0..5)
        .map(|s| (s, epoch.segment(s).expect("segment")))
        .collect();
    write_epoch(
        dir.path(),
        &multivariant(epoch.codecs(), epoch.bandwidth(), None),
        &epoch.playlist(None),
        &epoch.init().expect("init"),
        &segments,
    );

    let streams = probe(&ffprobe, &dir.path().join("master.m3u8"));
    let stream = &streams["streams"][0];
    assert_eq!(stream["codec_name"], "flac");
    assert_eq!(stream["sample_rate"], "44100");
    assert_eq!(stream["channels"], 2);

    let relayed = decode(&ffmpeg, &dir.path().join("index.m3u8"));
    let source_file = dir.path().join("tone.flac");
    std::fs::write(&source_file, TONE).expect("write");
    let source = decode(&ffmpeg, &source_file);
    assert_eq!(relayed.len(), 50 * 4608 * 4);
    assert!(relayed == source[..relayed.len()], "decoded audio differs");
}

#[tokio::test]
async fn the_cellular_tier_is_aac_in_fmp4_and_plays_as_hls() {
    let Some((ffmpeg, ffprobe)) = tools() else {
        return;
    };
    let mut rig = rig(|c| c.ffmpeg = Some(ffmpeg.clone())).await;
    let session = rig
        .manager
        .open(request("phone", Tier::Cellular))
        .await
        .expect("open")
        .session;
    let mut conn = connect(&mut rig.server).await;
    let (header, frames) = tone();
    conn.start_stream(&header).await;
    let past = conn.server_now_us() - 30_000_000;
    send_frames(&mut conn, past, &frames).await;
    eventually(|| {
        session
            .playlist_snapshot(1, None)
            .is_ok_and(|p| p.matches("#EXTINF").count() >= 4)
    })
    .await;
    let master = session.master_playlist(1, None).expect("master");
    assert!(master.contains("CODECS=\"mp4a.40.2\""), "{master}");
    let init = session.init_segment(1).await.expect("init");
    let mut segments = Vec::new();
    // ffmpeg keeps the last second or so until its input ends; four segments are out.
    for sequence in 0..4 {
        segments.push((
            sequence,
            session.segment(1, sequence).await.expect("segment"),
        ));
    }
    session.close(EndReason::Closed).await;
    let playlist = session.playlist_snapshot(1, None).expect("playlist");
    assert!(playlist.ends_with("#EXT-X-ENDLIST\n"));
    let listed: Vec<u64> = playlist
        .lines()
        .filter_map(|l| l.strip_suffix(".m4s"))
        .filter_map(|n| n.parse().ok())
        .collect();
    let dir = tempfile::tempdir().expect("tempdir");
    let wanted: Vec<_> = segments
        .into_iter()
        .filter(|(s, _)| listed.contains(s))
        .collect();
    let playlist = drop_unfetched(&playlist, 4);
    write_epoch(dir.path(), &master, &playlist, &init, &wanted);

    let streams = probe(&ffprobe, &dir.path().join("master.m3u8"));
    let stream = &streams["streams"][0];
    assert_eq!(stream["codec_name"], "aac");
    assert_eq!(stream["profile"], "LC");
    assert_eq!(stream["sample_rate"], "44100");

    let pcm = decode(&ffmpeg, &dir.path().join("master.m3u8"));
    let seconds = pcm.len() as f64 / 4.0 / 44_100.0;
    let listed_seconds: f64 = playlist
        .lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
        .sum();
    assert!(
        (seconds - listed_seconds).abs() < 0.05,
        "{seconds} s decoded, {listed_seconds} s listed"
    );
    assert!(seconds > 3.5, "{seconds}");
}

/// Keeps only the segments below `limit` (the ones the test fetched) in a playlist.
fn drop_unfetched(playlist: &str, limit: u64) -> String {
    let lines: Vec<&str> = playlist.lines().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let next = lines.get(i + 1).copied().unwrap_or("");
        let over = |l: &str| {
            l.strip_suffix(".m4s")
                .and_then(|n| n.parse::<u64>().ok())
                .is_some_and(|n| n >= limit)
        };
        if line.starts_with("#EXT-X-PROGRAM-DATE-TIME")
            && lines.get(i + 2).copied().is_some_and(over)
        {
            i += 3;
            continue;
        }
        if line.starts_with("#EXTINF") && over(next) {
            i += 2;
            continue;
        }
        out.push_str(line);
        out.push('\n');
        i += 1;
    }
    out
}

#[tokio::test]
async fn cellular_segments_are_encoded_in_time_for_their_publication() {
    let Some((ffmpeg, _)) = tools() else {
        return;
    };
    let mut rig = rig(|c| c.ffmpeg = Some(ffmpeg.clone())).await;
    let session = rig
        .manager
        .open(request("phone", Tier::Cellular))
        .await
        .expect("open")
        .session;
    let mut conn = connect(&mut rig.server).await;
    let (header, frames) = tone();
    conn.start_stream(&header).await;
    // The first segment is due 1.5 s from now; its audio reaches the encoder 0.8 s before.
    let start = conn.server_now_us() + 4_500_000;
    let sent = tokio::time::Instant::now();
    send_frames(&mut conn, start, &frames[..30]).await;
    eventually(|| {
        session
            .playlist_snapshot(1, None)
            .is_ok_and(|p| p.contains("0.m4s"))
    })
    .await;
    let took = sent.elapsed();
    assert!(
        took < std::time::Duration::from_millis(2000),
        "published after {took:?}"
    );
    assert_eq!(session.info().tier, Tier::Cellular);
    rig.cancel.cancel();
}
