//! Opens, finds and ends relay sessions within the limits: one per phone, a few per user, a
//! few per server (each is a separate stream pipeline inside Music Assistant).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use trackside_sendspin::{self as sendspin, IdentityStore, PlayerTiming, Token};

use crate::epoch::Shape;
use crate::session::{RelaySession, Settings};
use crate::{EndReason, PlayerControl, Tier};

/// The relay's settings. Start from [`RelayConfig::new`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RelayConfig {
    /// `ws(s)://host:port[/prefix]/sendspin`.
    pub sendspin_url: String,
    /// The Music Assistant token for the proxy; `None` for a direct Sendspin server.
    pub token: Option<Token>,
    /// ffmpeg, for the cellular tier. Without it only lossless is offered.
    pub ffmpeg: Option<PathBuf>,
    pub timing: PlayerTiming,
    /// Declared to Music Assistant as `buffer_capacity`.
    pub buffer_capacity: u32,
    pub shape: Shape,
    /// Audio waiting for publication, per session. Music Assistant never has more than
    /// `buffer_capacity` outstanding; past this the oldest goes and it counts as a violation.
    pub pending_cap: usize,
    /// Published segments, per session, across the current and the previous epoch.
    pub published_cap: usize,
    /// Playing with no playlist or segment fetch for this long pauses Music Assistant.
    pub fetch_timeout: Duration,
    /// Paused or ready for this long ends the session.
    pub paused_timeout: Duration,
    /// Music Assistant has this long to activate the player role.
    pub activation_timeout: Duration,
    /// A new epoch's playlist and init segment wait this long for audio.
    pub first_segment_wait: Duration,
    /// A segment just past the live edge waits this long.
    pub segment_wait: Duration,
    pub max_sessions: usize,
    pub max_per_user: usize,
    pub cellular_kbps: u32,
    /// Where playlists are served: `{prefix}/{session}/{epoch}/master.m3u8`.
    pub path_prefix: String,
    pub software_version: String,
    pub backoff: sendspin::Backoff,
}

impl RelayConfig {
    pub fn new(sendspin_url: impl Into<String>, token: Option<Token>) -> Self {
        Self {
            sendspin_url: sendspin_url.into(),
            token,
            ffmpeg: None,
            timing: PlayerTiming {
                output_delay_ms: 4000,
                required_lead_time_ms: 1000,
                min_buffer_ms: 300,
            },
            buffer_capacity: 1 << 20,
            shape: Shape::default(),
            pending_cap: (1 << 20) + (256 << 10),
            published_cap: 1536 << 10,
            fetch_timeout: Duration::from_secs(30),
            paused_timeout: Duration::from_mins(10),
            activation_timeout: Duration::from_secs(30),
            first_segment_wait: Duration::from_secs(8),
            segment_wait: Duration::from_secs(3),
            max_sessions: 4,
            max_per_user: 3,
            cellular_kbps: 256,
            path_prefix: "/api/v1/relay".into(),
            software_version: env!("CARGO_PKG_VERSION").into(),
            backoff: sendspin::Backoff::default(),
        }
    }

    /// The shape with the output delay and encoder lead derived from the timing, so the
    /// publication rule matches what Music Assistant was told.
    fn effective_shape(&self) -> Shape {
        let lead_ms = self
            .timing
            .min_buffer_ms
            .max(self.timing.required_lead_time_ms);
        Shape {
            output_delay_us: i64::from(self.timing.output_delay_ms.min(5000)) * 1000,
            encode_lead_us: (i64::from(lead_ms) * 1000 - 200_000).max(0),
            ..self.shape
        }
    }
}

/// Why a session could not be opened.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RelayError {
    /// `409 relay_limit` with `details.limit`.
    #[error("the relay limit ({limit}) is reached")]
    Limit { limit: usize },
    /// `422 unsupported`: the cellular tier needs ffmpeg.
    #[error("this server cannot relay {0}")]
    Unsupported(&'static str),
    #[error("the player identity could not be loaded: {0}")]
    Identity(String),
}

/// A request for a phone's session.
#[derive(Clone, Debug)]
pub struct OpenRequest {
    /// The new session's id, used only if one is created.
    pub id: String,
    pub user: String,
    /// The app device: one session and one Music Assistant player per device.
    pub device: String,
    /// Shown in Music Assistant.
    pub name: String,
    pub tier: Tier,
}

/// An opened session, and whether it is new.
#[derive(Debug)]
pub struct Opened {
    pub session: Arc<RelaySession>,
    pub created: bool,
}

pub struct RelayManager {
    config: RelayConfig,
    identities: Arc<dyn IdentityStore>,
    control: Arc<dyn PlayerControl>,
    sessions: Mutex<Vec<Arc<RelaySession>>>,
    /// Creating an identity is load, generate, save: one at a time, so a phone never gets two.
    identity_lock: tokio::sync::Mutex<()>,
    cancel: CancellationToken,
}

impl std::fmt::Debug for RelayManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayManager")
            .field("active", &self.active())
            .finish_non_exhaustive()
    }
}

impl RelayManager {
    pub fn new(
        config: RelayConfig,
        identities: Arc<dyn IdentityStore>,
        control: Arc<dyn PlayerControl>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            identities,
            control,
            sessions: Mutex::new(Vec::new()),
            identity_lock: tokio::sync::Mutex::new(()),
            cancel,
        }
    }

    pub fn config(&self) -> &RelayConfig {
        &self.config
    }

    pub fn cellular_available(&self) -> bool {
        self.config.ffmpeg.is_some()
    }

    fn live(&self) -> std::sync::MutexGuard<'_, Vec<Arc<RelaySession>>> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.retain(|s| !s.is_ended());
        sessions
    }

    pub fn active(&self) -> usize {
        self.live().len()
    }

    pub fn get(&self, id: &str) -> Option<Arc<RelaySession>> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|s| s.id() == id)
            .cloned()
    }

    pub fn for_user(&self, user: &str) -> Vec<Arc<RelaySession>> {
        self.live()
            .iter()
            .filter(|s| s.user() == user)
            .cloned()
            .collect()
    }

    /// The device's running session, or a new one within the limits.
    pub async fn open(&self, request: OpenRequest) -> Result<Opened, RelayError> {
        if request.tier == Tier::Cellular && !self.cellular_available() {
            return Err(RelayError::Unsupported("cellular"));
        }
        if let Some(session) = self.find(&request) {
            session.set_tier(request.tier);
            return Ok(Opened {
                session,
                created: false,
            });
        }
        let identity = {
            let _one = self.identity_lock.lock().await;
            sendspin::load_or_create(self.identities.as_ref(), &request.device)
                .await
                .map_err(|e| RelayError::Identity(e.to_string()))?
        };
        let mut sessions = self.live();
        if let Some(session) = sessions
            .iter()
            .find(|s| s.user() == request.user && s.device() == request.device)
        {
            return Ok(Opened {
                session: Arc::clone(session),
                created: false,
            });
        }
        if sessions.len() >= self.config.max_sessions {
            return Err(RelayError::Limit {
                limit: self.config.max_sessions,
            });
        }
        if sessions.iter().filter(|s| s.user() == request.user).count() >= self.config.max_per_user
        {
            return Err(RelayError::Limit {
                limit: self.config.max_per_user,
            });
        }
        let mut sendspin_config = sendspin::Config::new(
            self.config.sendspin_url.clone(),
            self.config.token.clone(),
            identity,
            &request.name,
        );
        sendspin_config.timing = self.config.timing;
        sendspin_config.hello.buffer_capacity = self.config.buffer_capacity;
        sendspin_config
            .hello
            .software_version
            .clone_from(&self.config.software_version);
        sendspin_config.backoff = self.config.backoff;
        let settings = Settings {
            shape: self.config.effective_shape(),
            pending_cap: self.config.pending_cap,
            published_cap: self.config.published_cap,
            fetch_timeout: self.config.fetch_timeout,
            paused_timeout: self.config.paused_timeout,
            activation_timeout: self.config.activation_timeout,
            first_segment_wait: self.config.first_segment_wait,
            segment_wait: self.config.segment_wait,
            cellular_kbps: self.config.cellular_kbps,
            ffmpeg: self.config.ffmpeg.clone(),
            path_prefix: self.config.path_prefix.clone(),
        };
        let session = RelaySession::start(
            request.id,
            request.user,
            request.device,
            request.tier,
            settings,
            sendspin_config,
            Arc::clone(&self.control),
            self.cancel.child_token(),
        );
        sessions.push(Arc::clone(&session));
        Ok(Opened {
            session,
            created: true,
        })
    }

    fn find(&self, request: &OpenRequest) -> Option<Arc<RelaySession>> {
        self.live()
            .iter()
            .find(|s| s.user() == request.user && s.device() == request.device)
            .cloned()
    }

    /// Ends one session. Returns whether it existed.
    pub async fn close(&self, id: &str, reason: EndReason) -> bool {
        let Some(session) = self.get(id) else {
            return false;
        };
        session.close(reason).await;
        true
    }

    /// Ends every session of a device (it was revoked or removed).
    pub async fn close_device(&self, device: &str, reason: EndReason) {
        let sessions: Vec<_> = self
            .live()
            .iter()
            .filter(|s| s.device() == device)
            .cloned()
            .collect();
        for session in sessions {
            session.close(reason).await;
        }
    }

    /// Ends every session (server shutdown or Music Assistant removed).
    pub async fn shutdown(&self, reason: EndReason) {
        let sessions: Vec<_> = self.live().clone();
        for session in sessions {
            session.close(reason).await;
        }
    }
}
