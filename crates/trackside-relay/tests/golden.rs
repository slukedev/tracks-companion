//! Golden bytes for the muxer: the FLAC and AAC init segments and the first FLAC media segment
//! of the tone. `UPDATE_GOLDEN=1` rewrites them; `tests/ffmpeg.rs` checks that ffprobe and
//! ffmpeg read what the muxer writes.

mod support;

use std::path::PathBuf;

use support::tone;
use trackside_relay::{
    AacConfig, Codec, Sample, StreamInfo, init_segment, media_segment, split_frames,
};

fn check(name: &str, bytes: &[u8]) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var("UPDATE_GOLDEN").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&path, bytes).expect("write golden");
        return;
    }
    let golden = std::fs::read(&path).expect("golden file (UPDATE_GOLDEN=1 writes it)");
    assert!(golden == bytes, "{name} differs from its golden copy");
}

#[test]
fn the_flac_init_segment_matches() {
    let (header, _) = tone();
    let info = StreamInfo::from_codec_header(&header).expect("info");
    check("flac-init.mp4", &init_segment(&Codec::Flac(info)));
}

#[test]
fn the_aac_init_segment_matches() {
    let codec = Codec::Aac {
        config: AacConfig {
            object_type: 2,
            frequency_index: 4,
            channels: 2,
        },
        bitrate: 256_000,
    };
    check("aac-init.mp4", &init_segment(&codec));
}

#[test]
fn the_first_flac_segment_matches() {
    let (_, frames) = tone();
    let parsed: Vec<_> = frames
        .iter()
        .take(10)
        .flat_map(|f| split_frames(f).expect("frame"))
        .collect();
    let samples: Vec<Sample<'_>> = parsed
        .iter()
        .map(|f| Sample {
            duration: f.samples,
            data: &f.data,
        })
        .collect();
    check("flac-seg0.m4s", &media_segment(1, 0, &samples));
}
