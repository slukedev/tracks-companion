//! The phone playback relay: Trackside joins Music Assistant as a Sendspin player for one phone
//! and serves that player's audio to the phone as live HLS, in real time and in memory only.
//!
//! - Music Assistant sends FLAC 44.1 kHz/16 bit/stereo. The lossless tier remuxes the frames
//!   into fragmented MP4 (`fLaC` with `dfLa`) without decoding; the cellular tier pipes the
//!   same FLAC through an ffmpeg child that encodes AAC-LC, packaged by the same muxer as
//!   `mp4a`.
//! - Every `stream/start`, `stream/clear` without `from_timestamp`, resume or tier change opens
//!   a new epoch with its own playlist URL, init segment and media sequence; the app swaps to
//!   it, so it never plays audio buffered from before a seek or skip.
//! - A segment (about 1 s) is published once it is complete and its end, on Music Assistant's
//!   timeline mapped to the local clock, minus the reported output delay has passed. Audio a
//!   `stream/clear` with `from_timestamp` replaces is therefore never published.
//! - Pending and published audio are capped in bytes per session, the playlist window is six
//!   segments, and nothing is ever written to disk, cached or added to the library.
//! - A session nobody fetches from while playing asks Music Assistant to pause; one paused or
//!   idle for ten minutes says goodbye and ends.
//!
//! [`RelayManager`] opens sessions within the per-user and server limits. The HTTP layer calls
//! the fetch methods of [`RelaySession`]; this crate has no routes of its own.

mod adts;
mod encoder;
mod epoch;
mod flac;
mod hls;
mod manager;
mod mp4;
mod session;

use std::future::Future;
use std::pin::Pin;

pub use adts::{AacConfig, AdtsParser};
pub use encoder::{Encoder, EncoderError, ffmpeg_args, find_ffmpeg};
pub use epoch::{Epoch, Segment, Shape, Timeline};
pub use flac::{FlacError, Frame as FlacFrame, StreamInfo, split_frames};
pub use hls::{
    Entry as PlaylistEntry, TARGET_DURATION, media as media_playlist, multivariant, rfc3339_ms,
};
pub use manager::{OpenRequest, Opened, RelayConfig, RelayError, RelayManager};
pub use mp4::{Codec, Sample, init_segment, media_segment};
pub use session::{FetchError, RelaySession};

/// What the phone receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tier {
    /// FLAC in fMP4, remuxed: for Wi-Fi and LAN.
    Lossless,
    /// AAC-LC in fMP4, encoded by ffmpeg: for cellular data.
    Cellular,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lossless => "lossless",
            Self::Cellular => "cellular",
        }
    }
}

/// Where a session stands (contract 3.2 `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionState {
    /// Joining Music Assistant.
    Starting,
    /// Joined, nothing playing.
    Ready,
    Playing,
    Paused,
    Ended,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Ended => "ended",
        }
    }
}

/// Why a session ended (contract 3.2 `ended_reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndReason {
    Idle,
    Closed,
    Replaced,
    AssistantUnavailable,
    ServerRestart,
    Revoked,
    Failed,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Closed => "closed",
            Self::Replaced => "replaced",
            Self::AssistantUnavailable => "assistant_unavailable",
            Self::ServerRestart => "server_restart",
            Self::Revoked => "revoked",
            Self::Failed => "failed",
        }
    }
}

/// A session as the API reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    /// Music Assistant's player id.
    pub player_id: String,
    pub state: SessionState,
    pub ended_reason: Option<EndReason>,
    /// English, for logs.
    pub detail: Option<String>,
    /// The current epoch, from 1; 0 before the first audio.
    pub generation: u32,
    pub tier: Tier,
    /// The multivariant playlist path of the live epoch, or of the next one when none is live
    /// (it waits for audio), without the media token. `None` once ended.
    pub hls: Option<String>,
    /// What the segments carry (contract 3.2 `stream.format`).
    pub format: OutputFormat,
}

/// The wire's `stream.format`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputFormat {
    pub codec: &'static str,
    pub container: &'static str,
    pub sample_rate: u32,
    pub channels: u32,
    /// Lossless only.
    pub bit_depth: Option<u32>,
    /// Cellular only.
    pub bitrate_kbps: Option<u32>,
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self {
            codec: "flac",
            container: "mp4",
            sample_rate: 44_100,
            channels: 2,
            bit_depth: Some(16),
            bitrate_kbps: None,
        }
    }
}

pub type ControlFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// What the relay asks of Music Assistant's API, which the server owns.
pub trait PlayerControl: Send + Sync + 'static {
    /// Pause the player: nobody is listening.
    fn pause<'a>(&'a self, player_id: &'a str) -> ControlFuture<'a>;
    /// Start playing again: the connection dropped while playing, which stopped it.
    fn resume<'a>(&'a self, player_id: &'a str) -> ControlFuture<'a>;
}
