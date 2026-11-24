//! A fake transcoder for tests in this crate and others. Enabled by the `testing` feature.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use crate::{
    BoxFuture, MediaError, TranscodeInput, TranscodeOutput, TranscodeProcess, TranscodeSpec,
    Transcoder,
};

/// How the fake answers HLS runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeHls {
    /// Write the init section and every segment from the start number up to this count,
    /// then exit.
    Produce(u32),
    /// Write nothing and run until stopped; the test writes segments itself.
    Manual,
}

#[derive(Debug)]
struct State {
    spawned: Vec<TranscodeSpec>,
    fail_next: u32,
    hls: FakeHls,
    output_len: usize,
    stdout_stalls: bool,
    panic_next: bool,
}

/// Records every spec and writes deterministic bytes instead of encoding.
///
/// File and stdout transcodes finish only when the gate has a permit; the gate starts open.
#[derive(Debug)]
pub struct FakeTranscoder {
    version: Option<String>,
    state: Mutex<State>,
    gate: Arc<Semaphore>,
}

impl Default for FakeTranscoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeTranscoder {
    /// A working fake with an open gate.
    pub fn new() -> Self {
        Self {
            version: Some("fake 1.0".into()),
            state: Mutex::new(State {
                spawned: Vec::new(),
                fail_next: 0,
                hls: FakeHls::Manual,
                output_len: 4096,
                stdout_stalls: false,
                panic_next: false,
            }),
            gate: Arc::new(Semaphore::new(Semaphore::MAX_PERMITS)),
        }
    }

    /// A transcoder that is not there, like a host without ffmpeg.
    pub fn missing() -> Self {
        Self {
            version: None,
            ..Self::new()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Closes the gate: transcodes wait until [`FakeTranscoder::release`].
    pub fn hold(&self) {
        self.gate.forget_permits(Semaphore::MAX_PERMITS);
    }

    /// Lets `n` held transcodes finish.
    pub fn release(&self, n: usize) {
        self.gate.add_permits(n);
    }

    pub fn fail_next(&self, n: u32) {
        self.lock().fail_next = n;
    }

    pub fn set_hls(&self, hls: FakeHls) {
        self.lock().hls = hls;
    }

    /// Bytes written for every file or stdout transcode.
    pub fn set_output_len(&self, len: usize) {
        self.lock().output_len = len;
    }

    /// Stdout transcodes write nothing and never end, like ffmpeg stuck on a hung input.
    pub fn stall_stdout(&self) {
        self.lock().stdout_stalls = true;
    }

    /// The next spawn panics, as a bug in the job task would.
    pub fn panic_next(&self) {
        self.lock().panic_next = true;
    }

    /// Every spec spawned so far, in order.
    pub fn spawned(&self) -> Vec<TranscodeSpec> {
        self.lock().spawned.clone()
    }

    /// The bytes the fake writes for a spec.
    pub fn output_for(spec: &TranscodeSpec, len: usize) -> Vec<u8> {
        let seed = format!("{:?}|{}", spec.input, spec.profile.as_str());
        let block = blake3::hash(seed.as_bytes());
        block.as_bytes().iter().copied().cycle().take(len).collect()
    }
}

struct FakeProcess {
    spec: TranscodeSpec,
    fail: bool,
    hls: FakeHls,
    len: usize,
    gate: Arc<Semaphore>,
    stdout: Option<tokio::io::DuplexStream>,
}

async fn write_hls(dir: &Path, start: u32, count: u32) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    tokio::fs::write(dir.join("init.mp4"), b"init").await?;
    for n in start..count {
        tokio::fs::write(dir.join(format!("seg-{n}.m4s")), format!("segment {n}")).await?;
    }
    Ok(())
}

impl TranscodeProcess for FakeProcess {
    fn stdin(&mut self) -> Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>> {
        matches!(self.spec.input, TranscodeInput::Stdin)
            .then(|| Box::new(tokio::io::sink()) as Box<dyn tokio::io::AsyncWrite + Send + Unpin>)
    }

    fn stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }

    fn wait(self: Box<Self>) -> BoxFuture<'static, Result<(), MediaError>> {
        let this = *self;
        Box::pin(async move {
            if this.fail {
                return Err(MediaError::Process("fake failure".into()));
            }
            match &this.spec.output {
                TranscodeOutput::File(path) => {
                    let _permit = this.gate.acquire().await;
                    tokio::fs::write(path, FakeTranscoder::output_for(&this.spec, this.len))
                        .await?;
                }
                TranscodeOutput::Remux(path) => {
                    if let TranscodeInput::Path(input) = &this.spec.input {
                        tokio::fs::copy(input, path).await?;
                    }
                }
                TranscodeOutput::Hls {
                    dir, start_number, ..
                } => match this.hls {
                    FakeHls::Produce(count) => write_hls(dir, *start_number, count).await?,
                    FakeHls::Manual => std::future::pending::<()>().await,
                },
                TranscodeOutput::Stdout | TranscodeOutput::Null => {
                    let _permit = this.gate.acquire().await;
                }
            }
            Ok(())
        })
    }

    fn kill(&mut self) {}
}

impl Transcoder for FakeTranscoder {
    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    fn spawn(&self, spec: TranscodeSpec) -> Result<Box<dyn TranscodeProcess>, MediaError> {
        if self.version.is_none() {
            return Err(MediaError::NoTranscoder);
        }
        let (fail, hls, len, stalls) = {
            let mut state = self.lock();
            assert!(
                !std::mem::take(&mut state.panic_next),
                "injected transcoder panic"
            );
            state.spawned.push(spec.clone());
            let fail = state.fail_next > 0;
            if fail {
                state.fail_next -= 1;
            }
            (fail, state.hls, state.output_len, state.stdout_stalls)
        };
        let stdout = if matches!(spec.output, TranscodeOutput::Stdout) {
            let (mut writer, reader) = tokio::io::duplex(64 * 1024);
            let bytes = Self::output_for(&spec, len);
            tokio::spawn(async move {
                if stalls {
                    // The writer stays open, so the reader never sees an end.
                    std::future::pending::<()>().await;
                }
                let _ = writer.write_all(&bytes).await;
            });
            Some(reader)
        } else {
            None
        };
        Ok(Box::new(FakeProcess {
            spec,
            fail,
            hls,
            len,
            gate: self.gate.clone(),
            stdout,
        }))
    }
}
