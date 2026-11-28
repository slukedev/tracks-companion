use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::CertificateDer;

use zeroize::Zeroizing;

use crate::identity::Identity;
use crate::wire::{Format, HelloInfo, PlayerTiming};

/// A Music Assistant access token for the `/sendspin` proxy. `Debug` never shows it and its
/// memory is zeroed on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(Zeroizing<String>);

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into().trim().to_owned()))
    }

    /// The token itself. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Exponential reconnect backoff with jitter: attempt `n` waits between half and all of
/// `min(max, initial * 2^n)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(30),
        }
    }
}

impl Backoff {
    pub fn ceiling(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
        self.initial.saturating_mul(factor).min(self.max)
    }

    pub(crate) fn delay(&self, attempt: u32, rng: &mut fastrand::Rng) -> Duration {
        let ceiling = self.ceiling(attempt);
        let half = ceiling / 2;
        let spread = u64::try_from(ceiling.saturating_sub(half).as_millis()).unwrap_or(u64::MAX);
        half + Duration::from_millis(rng.u64(0..=spread))
    }
}

/// How the client connects and what it says about itself. Start from [`Config::new`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Config {
    /// `ws(s)://host:port[/prefix]/sendspin`: Music Assistant's authenticated proxy.
    pub url: String,
    /// The token for the proxy's `auth` message. `None` connects to a Sendspin server
    /// directly, with no auth step.
    pub token: Option<Token>,
    pub identity: Identity,
    pub hello: HelloInfo,
    pub timing: PlayerTiming,
    /// TCP, TLS and the WebSocket upgrade.
    pub connect_timeout: Duration,
    /// The proxy's `auth_ok` (Music Assistant allows 10 s for the `auth` message).
    pub auth_timeout: Duration,
    /// Each message of the init exchange and the Noise handshake (the spec suggests 30 s).
    pub handshake_timeout: Duration,
    /// Requests per time-sync burst (the spec's minimum is 8).
    pub burst_size: u32,
    /// From the start of one burst to the next (at most 10 s by the spec).
    pub burst_interval: Duration,
    /// How long a `client/time` waits for its reply before the next one goes out.
    pub time_reply_timeout: Duration,
    pub ping_interval: Duration,
    /// A connection that delivers nothing, not even a pong, for this long is dead.
    pub dead_after: Duration,
    pub write_timeout: Duration,
    pub backoff: Backoff,
    /// A connection that stays up this long resets the backoff.
    pub stable_after: Duration,
    /// How long Music Assistant keeps a dropped player (aiosendspin `CLIENT_CLEANUP_DELAY`).
    /// A reconnect inside it is warm: the same player, its queue and group.
    pub retention: Duration,
    /// The client stops after failing to connect for this long.
    pub give_up_after: Duration,
    /// The largest message, reassembled from fragments.
    pub max_message_bytes: usize,
    /// For `wss://` with a private CA or a self-signed certificate: trust only these
    /// certificates instead of the platform's store, as the Music Assistant client does.
    pub custom_ca: Option<Arc<[CertificateDer<'static>]>>,
}

impl Config {
    /// The relay's defaults: one FLAC format, a 1 MiB buffer, a 4 s output delay and the
    /// device info Music Assistant reads as a private, session-only app player.
    pub fn new(
        url: impl Into<String>,
        token: Option<Token>,
        identity: Identity,
        name: &str,
    ) -> Self {
        Self {
            url: url.into(),
            token,
            identity,
            hello: HelloInfo {
                name: name.to_owned(),
                product_name: "Mobile Application".into(),
                manufacturer: "Tracks".into(),
                software_version: env!("CARGO_PKG_VERSION").into(),
                format: Format::flac_cd(),
                buffer_capacity: 1 << 20,
            },
            timing: PlayerTiming {
                output_delay_ms: 4000,
                required_lead_time_ms: 1000,
                min_buffer_ms: 300,
            },
            connect_timeout: Duration::from_secs(10),
            auth_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(30),
            burst_size: 8,
            burst_interval: Duration::from_secs(5),
            time_reply_timeout: Duration::from_secs(1),
            ping_interval: Duration::from_secs(20),
            dead_after: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
            backoff: Backoff::default(),
            stable_after: Duration::from_secs(30),
            retention: Duration::from_secs(180),
            give_up_after: Duration::from_mins(10),
            max_message_bytes: 4 << 20,
            custom_ca: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_token() {
        let token = Token::new("eyJhbGciOiJIUzI1NiJ9.secret-part");
        assert!(!format!("{token:?}").contains("secret"));
    }

    #[test]
    fn backoff_doubles_up_to_the_cap_and_jitters_within_it() {
        let backoff = Backoff::default();
        assert_eq!(backoff.ceiling(0), Duration::from_millis(500));
        assert_eq!(backoff.ceiling(3), Duration::from_secs(4));
        assert_eq!(backoff.ceiling(40), Duration::from_secs(30));
        let mut rng = fastrand::Rng::with_seed(3);
        for attempt in 0..20 {
            let delay = backoff.delay(attempt, &mut rng);
            assert!(delay >= backoff.ceiling(attempt) / 2 && delay <= backoff.ceiling(attempt));
        }
    }

    #[test]
    fn frame_dumps_from_the_websocket_library_are_compiled_out() {
        assert!(log::STATIC_MAX_LEVEL <= log::LevelFilter::Debug);
    }
}
