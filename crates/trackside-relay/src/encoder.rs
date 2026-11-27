//! The cellular tier's encoder: an ffmpeg child that reads FLAC on stdin and writes ADTS AAC on
//! stdout. Pipes only, nothing on disk; a bounded input channel; stderr drained and kept short;
//! killed when dropped.
//!
//! The FLAC goes in as fragmented MP4, one fragment per frame, from the relay's own muxer.
//! Raw FLAC would cost about a second more: ffmpeg's FLAC parser looks ahead several frame
//! headers before it hands on a frame, while the MP4 demuxer hands on each sample as it
//! arrives (measured with ffmpeg 6.1: about 1.4 s against 0.45 s behind real time).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::adts::{AacConfig, AdtsParser};
use crate::flac::StreamInfo;
use crate::mp4::{self, Codec, Sample};

/// FLAC fragments that may wait for ffmpeg (about 6.7 s of 104 ms frames). A full queue means
/// ffmpeg fell behind real time.
const INPUT_FRAMES: usize = 64;
/// Kept from stderr for the log.
const STDERR_KEEP: usize = 2048;

/// The fixed argument vector. No paths, nothing from users.
pub fn ffmpeg_args(bitrate_kbps: u32) -> Vec<String> {
    [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-probesize",
        "32",
        "-analyzeduration",
        "0",
        "-f",
        "mp4",
        "-i",
        "pipe:0",
        "-map",
        "0:a:0",
        "-c:a",
        "aac",
        "-b:a",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .chain([
        format!("{bitrate_kbps}k"),
        "-flush_packets".into(),
        "1".into(),
        "-f".into(),
        "adts".into(),
        "pipe:1".into(),
    ])
    .collect()
}

/// Where ffmpeg is, when it is.
pub fn find_ffmpeg(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return path.is_file().then(|| path.to_owned());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("ffmpeg"))
            .find(|p| p.is_file())
    })
}

/// Why the encoder stopped taking input.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncoderError {
    #[error("ffmpeg did not start: {0}")]
    Spawn(String),
    #[error("ffmpeg fell behind")]
    Behind,
    #[error("ffmpeg stopped")]
    Gone,
}

/// A running encoder. Dropping it kills ffmpeg.
#[derive(Debug)]
pub struct Encoder {
    input: mpsc::Sender<Bytes>,
    stop: CancellationToken,
    sequence: u32,
    decode_time: u64,
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Encoder {
    /// Starts ffmpeg, writes the FLAC init segment, and calls `on_frame` for each AAC frame
    /// and `on_exit` once when it ends.
    pub fn spawn(
        ffmpeg: &Path,
        bitrate_kbps: u32,
        info: &StreamInfo,
        on_frame: impl Fn(AacConfig, Bytes) + Send + 'static,
        on_exit: impl FnOnce(Result<(), String>) + Send + 'static,
    ) -> Result<Self, EncoderError> {
        let mut child = Command::new(ffmpeg)
            .args(ffmpeg_args(bitrate_kbps))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| EncoderError::Spawn(e.to_string()))?;
        let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(EncoderError::Spawn("no pipes".into()));
        };
        let stream_header = mp4::init_segment(&Codec::Flac(info.clone()));
        let (input, mut frames) = mpsc::channel::<Bytes>(INPUT_FRAMES);
        let stop = CancellationToken::new();

        let writer = async move {
            if stdin.write_all(&stream_header).await.is_err() {
                return;
            }
            while let Some(frame) = frames.recv().await {
                if stdin.write_all(&frame).await.is_err() {
                    return;
                }
            }
            let _ = stdin.shutdown().await;
        };
        let reader = async move {
            let mut parser = AdtsParser::default();
            let mut buffer = vec![0u8; 8192];
            loop {
                match stdout.read(&mut buffer).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        for (config, frame) in parser.push(&buffer[..n]) {
                            on_frame(config, frame);
                        }
                    }
                }
            }
        };
        let errors = async move {
            let mut kept = Vec::new();
            let mut buffer = [0u8; 512];
            while let Ok(n) = stderr.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                kept.extend_from_slice(&buffer[..n]);
                if kept.len() > STDERR_KEEP {
                    kept.drain(..kept.len() - STDERR_KEEP);
                }
            }
            String::from_utf8_lossy(&kept).trim().to_owned()
        };
        let cancel = stop.clone();
        tokio::spawn(async move {
            let run = async {
                let ((), (), stderr, status) = tokio::join!(writer, reader, errors, child.wait());
                match status {
                    Ok(status) if status.success() => Ok(()),
                    Ok(status) => Err(format!("{status}: {stderr}")),
                    Err(e) => Err(e.to_string()),
                }
            };
            tokio::select! {
                result = run => on_exit(result),
                () = cancel.cancelled() => {}
            }
        });
        Ok(Self {
            input,
            stop,
            sequence: 0,
            decode_time: 0,
        })
    }

    /// Closes ffmpeg's input, so it finishes what it has and exits.
    pub fn finish_input(&mut self) {
        self.input = mpsc::channel(1).0;
    }

    /// Queues one FLAC frame of `samples` samples. Never waits: a full queue is an error.
    pub fn feed(&mut self, frame: &[u8], samples: u32) -> Result<(), EncoderError> {
        self.sequence = self.sequence.wrapping_add(1);
        let fragment = mp4::media_segment(
            self.sequence,
            self.decode_time,
            &[Sample {
                duration: samples,
                data: frame,
            }],
        );
        self.decode_time += u64::from(samples);
        self.input
            .try_send(Bytes::from(fragment))
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => EncoderError::Behind,
                mpsc::error::TrySendError::Closed(_) => EncoderError::Gone,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_read_flac_and_write_adts_through_pipes() {
        let args = ffmpeg_args(256).join(" ");
        assert!(args.contains("-f mp4 -i pipe:0"), "{args}");
        assert!(args.contains("-c:a aac -b:a 256k"), "{args}");
        assert!(args.ends_with("-f adts pipe:1"), "{args}");
    }

    #[test]
    fn a_missing_configured_ffmpeg_is_not_found() {
        assert_eq!(find_ffmpeg(Some(Path::new("/nonexistent/ffmpeg"))), None);
    }
}
