//! ffmpeg as a subprocess: detection, fixed argument vectors per profile, and the process
//! handle.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::{
    BoxFuture, MediaError, Profile, TranscodeInput, TranscodeOutput, TranscodeProcess,
    TranscodeSpec, Transcoder,
};

/// Kept from ffmpeg's stderr for logs and errors.
const STDERR_LIMIT: usize = 4096;
const DETECT_TIMEOUT: Duration = Duration::from_secs(10);

/// ffmpeg as a subprocess.
#[derive(Clone, Debug)]
pub struct FfmpegTranscoder {
    path: PathBuf,
    version: String,
    encoder_id: String,
    profiles: Vec<Profile>,
}

fn run_with_timeout(path: &Path, args: &[&str]) -> Option<String> {
    let mut child = std::process::Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut out)
            .ok()
            .map(|_| out)
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if started.elapsed() > DETECT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    reader.join().ok().flatten()
}

impl FfmpegTranscoder {
    /// Finds ffmpeg at `path` or on `PATH`, reads its version and which encoders it has.
    /// `None` when it is missing or does not run.
    pub fn detect(path: Option<&Path>) -> Option<Self> {
        let path = path.map_or_else(|| PathBuf::from("ffmpeg"), Path::to_path_buf);
        let version_output = run_with_timeout(&path, &["-hide_banner", "-version"])?;
        let first = version_output.lines().next().unwrap_or_default();
        let version = first
            .strip_prefix("ffmpeg version ")
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or(first)
            .to_owned();
        let encoder_id = blake3::hash(version_output.as_bytes()).to_hex()[..6].to_owned();
        let encoders = run_with_timeout(&path, &["-hide_banner", "-encoders"]).unwrap_or_default();
        let has = |name: &str| {
            encoders
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(name))
        };
        let profiles = Profile::ALL
            .into_iter()
            .filter(|p| match p {
                Profile::Aac256 | Profile::Aac128 => has("aac"),
                Profile::Mp3_320 | Profile::Mp3_128 => has("libmp3lame"),
                Profile::Opus128 => has("libopus"),
                Profile::Flac => has("flac"),
            })
            .collect();
        tracing::info!(%version, path = %path.display(), "found ffmpeg");
        Some(Self {
            path,
            version,
            encoder_id,
            profiles,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn secs(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64())
}

/// Resampling and encoder arguments per profile.
fn codec_args(profile: Profile) -> &'static [&'static str] {
    // AAC-LC and MP3 stay at the rates every Apple decoder handles.
    match profile {
        Profile::Aac256 => &[
            "-af",
            "aformat=sample_rates=44100|48000",
            "-c:a",
            "aac",
            "-b:a",
            "256k",
        ],
        Profile::Aac128 => &[
            "-af",
            "aformat=sample_rates=44100|48000",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
        ],
        Profile::Mp3_320 => &[
            "-af",
            "aformat=sample_rates=44100|48000",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "320k",
        ],
        Profile::Mp3_128 => &[
            "-af",
            "aformat=sample_rates=44100|48000",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "128k",
        ],
        Profile::Opus128 => &["-ar", "48000", "-c:a", "libopus", "-b:a", "128k"],
        Profile::Flac => &["-c:a", "flac"],
    }
}

/// The argument vector for a spec. Fixed per profile, never built from user text.
pub fn ffmpeg_args(spec: &TranscodeSpec) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-nostdin", "-hide_banner", "-loglevel", "error"]
        .iter()
        .map(OsString::from)
        .collect();
    let mut push = |items: &[&str]| args.extend(items.iter().map(OsString::from));
    if matches!(spec.output, TranscodeOutput::Null) {
        push(&["-xerror"]);
    }
    if let Some(offset) = spec.offset {
        push(&["-ss", &secs(offset)]);
    }
    push(&["-i"]);
    match &spec.input {
        TranscodeInput::Path(path) => args.push(path.clone().into_os_string()),
        TranscodeInput::Stdin => args.push("pipe:0".into()),
    }
    let mut push = |items: &[&str]| args.extend(items.iter().map(OsString::from));
    push(&["-map", "0:a:0", "-map_metadata", "-1"]);
    match &spec.output {
        TranscodeOutput::Null => {
            push(&["-progress", "pipe:1", "-nostats", "-f", "null", "-"]);
            return args;
        }
        TranscodeOutput::Remux(path) => {
            push(&["-c", "copy", "-movflags", "+faststart", "-f", "ipod"]);
            args.push(path.clone().into_os_string());
            return args;
        }
        _ => {}
    }
    push(&["-fflags", "+bitexact", "-flags:a", "+bitexact"]);
    push(codec_args(spec.profile));
    let file_format = match spec.profile {
        Profile::Aac256 | Profile::Aac128 => "ipod",
        Profile::Mp3_320 | Profile::Mp3_128 => "mp3",
        Profile::Opus128 => "ogg",
        Profile::Flac => "flac",
    };
    match &spec.output {
        TranscodeOutput::File(path) => {
            if file_format == "ipod" {
                push(&["-movflags", "+faststart"]);
            }
            push(&["-f", file_format]);
            args.push(path.clone().into_os_string());
        }
        TranscodeOutput::Stdout => {
            if file_format == "ipod" {
                push(&[
                    "-movflags",
                    "frag_keyframe+empty_moov+default_base_moof",
                    "-f",
                    "mp4",
                ]);
            } else {
                push(&["-f", file_format]);
            }
            push(&["pipe:1"]);
        }
        TranscodeOutput::Hls {
            dir,
            segment,
            start_number,
        } => {
            if let Some(offset) = spec.offset {
                push(&["-output_ts_offset", &secs(offset)]);
            }
            let start = start_number.to_string();
            let time = secs(*segment);
            push(&[
                "-f",
                "hls",
                "-hls_time",
                &time,
                "-hls_playlist_type",
                "vod",
                "-hls_segment_type",
                "fmp4",
                "-hls_flags",
                "temp_file",
                "-hls_fmp4_init_filename",
                "init.mp4",
                "-start_number",
                &start,
                "-hls_segment_filename",
            ]);
            args.push(dir.join("seg-%d.m4s").into_os_string());
            args.push(dir.join("ffmpeg.m3u8").into_os_string());
        }
        TranscodeOutput::Null | TranscodeOutput::Remux(_) => {}
    }
    args
}

impl Transcoder for FfmpegTranscoder {
    fn version(&self) -> Option<String> {
        Some(self.version.clone())
    }

    fn encoder_id(&self) -> Option<String> {
        Some(self.encoder_id.clone())
    }

    fn profiles(&self) -> Vec<Profile> {
        self.profiles.clone()
    }

    fn spawn(&self, spec: TranscodeSpec) -> Result<Box<dyn TranscodeProcess>, MediaError> {
        let args = ffmpeg_args(&spec);
        let needs_stdout = matches!(spec.output, TranscodeOutput::Stdout | TranscodeOutput::Null);
        let mut command = Command::new(&self.path);
        command
            .args(&args)
            .stdin(if matches!(spec.input, TranscodeInput::Stdin) {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(if needs_stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MediaError::NoTranscoder
            } else {
                MediaError::Io(e)
            }
        })?;
        let stderr = child.stderr.take().map(|mut err| {
            tokio::spawn(async move {
                let mut kept = Vec::new();
                let mut buf = [0u8; 1024];
                while let Ok(n) = err.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let room = STDERR_LIMIT.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
                String::from_utf8_lossy(&kept).trim().to_owned()
            })
        });
        Ok(Box::new(FfmpegProcess {
            child,
            stderr,
            strict: matches!(spec.output, TranscodeOutput::Null),
        }))
    }
}

/// A running ffmpeg. Dropping it, or the future from `wait`, kills the process.
#[derive(Debug)]
struct FfmpegProcess {
    child: Child,
    stderr: Option<tokio::task::JoinHandle<String>>,
    /// Decode checks fail on any logged error, since ffmpeg exits 0 after skipping bad frames.
    strict: bool,
}

impl TranscodeProcess for FfmpegProcess {
    fn stdin(&mut self) -> Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>> {
        self.child
            .stdin
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncWrite + Send + Unpin>)
    }

    fn stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }

    fn wait(self: Box<Self>) -> BoxFuture<'static, Result<(), MediaError>> {
        let mut this = *self;
        Box::pin(async move {
            let status = this.child.wait().await?;
            let stderr = match this.stderr.take() {
                Some(task) => task.await.unwrap_or_default(),
                None => String::new(),
            };
            if !status.success() {
                tracing::warn!(%status, %stderr, "ffmpeg failed");
                return Err(MediaError::Process(if stderr.is_empty() {
                    status.to_string()
                } else {
                    stderr
                }));
            }
            if this.strict && !stderr.is_empty() {
                return Err(MediaError::Process(stderr));
            }
            Ok(())
        })
    }

    fn kill(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn aac_file_arguments_match_the_design() {
        let args = ffmpeg_args(&TranscodeSpec {
            input: TranscodeInput::Path("/music/a b.flac".into()),
            profile: Profile::Aac256,
            output: TranscodeOutput::File("/cache/x.m4a.part".into()),
            offset: None,
        });
        assert_eq!(
            strings(&args).join(" "),
            "-nostdin -hide_banner -loglevel error -i /music/a b.flac -map 0:a:0 -map_metadata -1 \
             -fflags +bitexact -flags:a +bitexact -af aformat=sample_rates=44100|48000 -c:a aac \
             -b:a 256k -movflags +faststart -f ipod /cache/x.m4a.part"
        );
    }

    #[test]
    fn hls_arguments_seek_and_offset_timestamps() {
        let args = strings(&ffmpeg_args(&TranscodeSpec {
            input: TranscodeInput::Stdin,
            profile: Profile::Aac128,
            output: TranscodeOutput::Hls {
                dir: "/cache/hls/j/r1".into(),
                segment: Duration::from_secs(6),
                start_number: 10,
            },
            offset: Some(Duration::from_secs(60)),
        }));
        let joined = args.join(" ");
        assert!(joined.starts_with("-nostdin -hide_banner -loglevel error -ss 60.000 -i pipe:0"));
        assert!(joined.contains("-output_ts_offset 60.000 -f hls -hls_time 6.000"));
        assert!(
            joined.contains("-start_number 10 -hls_segment_filename /cache/hls/j/r1/seg-%d.m4s")
        );
    }

    #[test]
    fn stdout_aac_is_fragmented() {
        let args = strings(&ffmpeg_args(&TranscodeSpec {
            input: TranscodeInput::Path("/a.flac".into()),
            profile: Profile::Aac256,
            output: TranscodeOutput::Stdout,
            offset: None,
        }));
        assert!(
            args.join(" ")
                .ends_with("-movflags frag_keyframe+empty_moov+default_base_moof -f mp4 pipe:1")
        );
    }

    #[test]
    fn missing_ffmpeg_is_not_detected() {
        assert!(FfmpegTranscoder::detect(Some(Path::new("/nonexistent/ffmpeg"))).is_none());
    }
}
