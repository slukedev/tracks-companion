//! The phone playback relay as the HTTP crates see it (contract section 3): Trackside joins
//! Music Assistant as a Sendspin player for one app device and serves that player's audio to
//! the device as live HLS. `trackside-assistant` implements [`RelayControl`].
//!
//! Audio is real-time only: segments live in memory for a few seconds and are never written
//! to disk, cached, added to the library or offered for download.

use bytes::Bytes;

use crate::control::BoxFuture;
use crate::ids::{DeviceId, DeviceKey, RelaySessionId, SpeakerId, UserKey};
use crate::time::Millis;

/// What the phone receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RelayTier {
    /// FLAC in fMP4: Wi-Fi and LAN.
    Lossless,
    /// AAC-LC in fMP4, encoded with ffmpeg: cellular data.
    Cellular,
}

impl RelayTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lossless => "lossless",
            Self::Cellular => "cellular",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "lossless" => Some(Self::Lossless),
            "cellular" => Some(Self::Cellular),
            _ => None,
        }
    }
}

/// Where a session stands (contract 3.2 `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RelayState {
    Starting,
    Ready,
    Playing,
    Paused,
    Ended,
}

impl RelayState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Ended => "ended",
        }
    }
}

/// `stream.format`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayFormat {
    pub codec: &'static str,
    pub container: &'static str,
    pub sample_rate: u32,
    pub channels: u32,
    /// Lossless only.
    pub bit_depth: Option<u32>,
    /// Cellular only.
    pub bitrate_kbps: Option<u32>,
}

/// A session as its owner sees it (contract 3.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayView {
    pub id: RelaySessionId,
    pub user: UserKey,
    pub device: DeviceKey,
    pub device_id: DeviceId,
    pub speaker_id: SpeakerId,
    pub created_at: Millis,
    pub state: RelayState,
    /// With [`RelayState::Ended`]: `idle`, `closed`, `replaced`, `assistant_unavailable`,
    /// `server_restart`, `revoked` or `failed`.
    pub ended_reason: Option<&'static str>,
    /// English, for logs.
    pub detail: Option<String>,
    /// The current epoch, from 1; 0 before the first audio.
    pub generation: u32,
    pub tier: RelayTier,
    pub latency_ms: u32,
    pub idle_timeout_s: u64,
    pub fetch_timeout_s: u64,
    /// The multivariant playlist of the live epoch, or of the next one, as a path under the
    /// API root (`/relay/{id}/{generation}/master.m3u8`). `None` once ended.
    pub hls: Option<String>,
    pub format: RelayFormat,
}

/// The app device asking: relays are per device and only device keys may open one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayCaller {
    pub user: UserKey,
    pub device: DeviceKey,
    pub device_id: DeviceId,
    /// Shown in Music Assistant.
    pub name: String,
}

/// `POST /relay/sessions` without its `play`, which goes through the speaker commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayOpen {
    pub tier: RelayTier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayOpened {
    pub session: RelayView,
    pub created: bool,
}

/// One file of a session's HLS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayPart {
    Master,
    Media,
    Init,
    Segment(u64),
}

impl RelayPart {
    /// `master.m3u8`, `index.m3u8`, `init.mp4` or `<n>.m4s`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "master.m3u8" => Some(Self::Master),
            "index.m3u8" => Some(Self::Media),
            "init.mp4" => Some(Self::Init),
            _ => {
                let number = name.strip_suffix(".m4s")?;
                if number.is_empty()
                    || number.len() > 19
                    || !number.bytes().all(|b| b.is_ascii_digit())
                {
                    return None;
                }
                number.parse().ok().map(Self::Segment)
            }
        }
    }

    pub fn is_playlist(self) -> bool {
        matches!(self, Self::Master | Self::Media)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayBody {
    Playlist(String),
    Media(Bytes),
}

/// Why a relay request was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RelayError {
    #[error("no such relay session")]
    NotFound,
    /// `422 unsupported`: `relay` or `relay_cellular`.
    #[error("this server cannot {0}")]
    Unsupported(&'static str),
    /// `409 relay_limit`.
    #[error("the relay limit ({0}) is reached")]
    Limit(usize),
    /// `503 catalog_unavailable`.
    #[error("Music Assistant is not reachable")]
    Unavailable,
    /// `409 relay_ended`.
    #[error("the relay session ended")]
    Ended,
    #[error("{0}")]
    Failed(String),
}

/// The relay, implemented where Music Assistant is.
pub trait RelayControl: Send + Sync {
    /// Music Assistant is connected and its Sendspin server is there.
    fn supported(&self) -> bool;

    /// The cellular tier is available (ffmpeg is installed).
    fn cellular(&self) -> bool;

    /// The device's running session, or a new one within the limits.
    fn open<'a>(
        &'a self,
        caller: &'a RelayCaller,
        open: RelayOpen,
    ) -> BoxFuture<'a, Result<RelayOpened, RelayError>>;

    /// The user's live sessions, all devices.
    fn sessions(&self, user: UserKey) -> Vec<RelayView>;

    /// One of the user's sessions, live or recently ended.
    fn session(&self, user: UserKey, id: &RelaySessionId) -> Option<RelayView>;

    /// Switches tier; only the owning device may.
    fn set_tier(
        &self,
        caller: &RelayCaller,
        id: &RelaySessionId,
        tier: RelayTier,
    ) -> Result<RelayView, RelayError>;

    /// Ends the session (`closed`): the Music Assistant player leaves.
    fn close<'a>(
        &'a self,
        user: UserKey,
        id: &'a RelaySessionId,
    ) -> BoxFuture<'a, Result<(), RelayError>>;

    /// A playlist or segment for the owning device. `query` (`?mt=...`) is carried onto the
    /// URLs inside playlists.
    fn fetch<'a>(
        &'a self,
        device: DeviceKey,
        id: &'a RelaySessionId,
        generation: u32,
        part: RelayPart,
        query: &'a str,
    ) -> BoxFuture<'a, Result<RelayBody, RelayError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_names_parse() {
        assert_eq!(RelayPart::parse("master.m3u8"), Some(RelayPart::Master));
        assert_eq!(RelayPart::parse("index.m3u8"), Some(RelayPart::Media));
        assert_eq!(RelayPart::parse("init.mp4"), Some(RelayPart::Init));
        assert_eq!(RelayPart::parse("14.m4s"), Some(RelayPart::Segment(14)));
        for bad in [
            ".m4s",
            "-1.m4s",
            "+1.m4s",
            "1.mp4",
            "seg-1.m4s",
            "99999999999999999999.m4s",
        ] {
            assert_eq!(RelayPart::parse(bad), None, "{bad}");
        }
    }
}
