//! Authentication: passwords (Argon2id), sessions with CSRF, device keys, API keys, app
//! passwords, media tokens, the setup token, invites, password resets, pairing, rate limits,
//! trusted-proxy client addresses, Subsonic credential checks, and axum extractors.
//!
//! SQL stays in `trackside-core`; this crate makes and checks secrets and calls the core
//! repositories through [`Db::read`] and [`Db::write`].

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use http::request::Parts;
use tokio::sync::broadcast::error::RecvError;
use trackside_core::config::AuthConfig;
use trackside_core::repo::keys::ApiKeyRow;
use trackside_core::{
    Access, Clock, Db, DbError, DeviceId, DeviceKey, EventBus, InviteId, KeyId, Millis, PairingId,
    Permissions, Role, ServerEvent, UserId, UserKey, WriteTx,
};
use zeroize::Zeroizing;

mod account;
mod authn;
mod cache;
mod credentials;
pub mod extract;
mod limit;
mod media_token;
mod pairing;
mod password;
pub mod proof;
mod proxy;
mod request;
mod secret;
mod setup;
mod subsonic;
mod token;

pub use password::PASSWORD_BYTES;
pub use proxy::is_local_address;
pub use request::{
    CSRF_HEADER, Https, PLAIN_SESSION_COOKIE, SESSION_COOKIE, redact_path, redact_query, redact_uri,
};
pub use secret::{SecretKey, open, seal};
pub use setup::setup_banner;
pub use trackside_core::repo::devices::NewDevice;
pub use trackside_core::repo::keys::KeyKind;
pub use trackside_core::repo::users::UserPatch;

/// How far the server clock may step back without a fresh media token being refused as
/// minted in the future. Clients' clocks are never consulted.
pub const CLOCK_SKEW: Duration = Duration::from_mins(5);
/// Concurrent streams per user.
pub const STREAMS_PER_USER: usize = 8;
/// Concurrent transcodes per user.
pub const TRANSCODES_PER_USER: usize = 3;
/// Concurrent event streams per user: each holds a connection and a broadcast receiver for as
/// long as it is open, so one account cannot open them without end.
pub const EVENT_STREAMS_PER_USER: usize = 16;

/// Why authentication or an auth operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    #[error("unauthenticated")]
    Unauthenticated,
    #[error("forbidden")]
    Forbidden,
    #[error("csrf")]
    Csrf,
    /// A key was sent in a query string outside media routes.
    #[error("credential in query string")]
    CredentialInQuery,
    #[error("rate limited")]
    RateLimited { retry_after: Duration },
    #[error("media token expired")]
    MediaTokenExpired,
    #[error("setup required")]
    SetupRequired,
    /// A password or key sent over plain HTTP from an address that is not nearby.
    #[error(
        "Use the server's https address. Passwords and keys aren't accepted over plain HTTP from outside your network."
    )]
    HttpsRequired,
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

impl AuthError {
    /// Repository errors that are answers rather than faults become the matching auth error.
    fn lift(error: DbError) -> Self {
        match error {
            DbError::NotFound => Self::NotFound,
            DbError::Forbidden => Self::Forbidden,
            DbError::Conflict(reason) => Self::Conflict(reason),
            DbError::Invalid(reason) => Self::Invalid(reason),
            other => Self::Db(other),
        }
    }

    fn rate_limited(retry_after: Duration) -> Self {
        Self::RateLimited { retry_after }
    }
}

/// What a credential may do (architecture section 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Scopes(u8);

impl Scopes {
    pub const LIBRARY: Self = Self(1);
    pub const STREAM: Self = Self(1 << 1);
    pub const DOWNLOAD: Self = Self(1 << 2);
    pub const WRITE: Self = Self(1 << 3);
    pub const REQUEST: Self = Self(1 << 4);
    pub const ALL: Self = Self(0b1_1111);
    pub const NONE: Self = Self(0);
    /// What API keys and app passwords get unless an admin chooses otherwise.
    pub const KEY_DEFAULT: Self = Self(Self::LIBRARY.0 | Self::STREAM.0 | Self::WRITE.0);

    const NAMES: [(Self, &'static str); 5] = [
        (Self::LIBRARY, "library"),
        (Self::STREAM, "stream"),
        (Self::DOWNLOAD, "download"),
        (Self::WRITE, "write"),
        (Self::REQUEST, "request"),
    ];

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The scopes with these bits; unknown bits are dropped.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & Self::ALL.0)
    }

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Parses scope names separated by spaces or commas, as stored and sent. `None` for an
    /// unknown name.
    pub fn parse(s: &str) -> Option<Self> {
        let mut scopes = Self::NONE;
        for name in s.split([' ', ',']).filter(|n| !n.is_empty()) {
            let (scope, _) = Self::NAMES.iter().find(|(_, n)| *n == name)?;
            scopes = scopes.union(*scope);
        }
        Some(scopes)
    }

    /// The scope names, in a fixed order.
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .filter(|(scope, _)| self.contains(*scope))
            .map(|(_, name)| *name)
            .collect()
    }
}

impl std::fmt::Display for Scopes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.names().join(" "))
    }
}

/// How a request was authenticated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    Session,
    DeviceKey,
    ApiKey,
    AppPassword,
    MediaToken,
}

impl Via {
    /// The name in the fail2ban log line and the audit log.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::DeviceKey => "device_key",
            Self::ApiKey => "api_key",
            Self::AppPassword => "app_password",
            Self::MediaToken => "media_token",
        }
    }
}

/// The authenticated caller.
#[derive(Clone, Debug)]
pub struct Principal {
    pub access: Access,
    pub user: UserId,
    pub username: String,
    pub role: Role,
    /// The user's effective permissions: every flag is on for admins.
    pub permissions: Permissions,
    pub scopes: Scopes,
    pub via: Via,
    pub device: Option<(DeviceKey, DeviceId)>,
    /// The key the request was authenticated with (device key, API key or app password, or
    /// the key a media token was minted for). `None` for sessions.
    pub key: Option<KeyId>,
    /// The dashboard session, for session requests.
    pub session: Option<SessionInfo>,
}

/// A dashboard session behind a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub key: trackside_core::repo::sessions::SessionKey,
    /// Until when sensitive actions go ahead without asking for the password again.
    pub recent_auth_until: Millis,
}

/// How long a password check counts as recent for sensitive actions.
pub const RECENT_AUTH: std::time::Duration = std::time::Duration::from_mins(10);

impl Principal {
    /// Whether a sensitive action may go ahead without a fresh password check: always for
    /// keys, and for sessions within [`RECENT_AUTH`] of the last check.
    pub fn recently_authenticated(&self, now: Millis) -> bool {
        self.session.is_none_or(|s| now < s.recent_auth_until)
    }

    /// Whether the caller may use `scopes`: the credential must carry them, `request` also
    /// needs the user's `can_request`, and `download` the user's `can_download`.
    pub fn allows(&self, scopes: Scopes) -> bool {
        self.scopes.contains(scopes)
            && (!scopes.contains(Scopes::REQUEST) || self.permissions.can_request)
            && (!scopes.contains(Scopes::DOWNLOAD) || self.permissions.can_download)
    }

    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

/// Every flag is on for admins (architecture section 7); members get what is stored.
pub(crate) fn effective_permissions(role: Role, stored: Permissions) -> Permissions {
    match role {
        Role::Admin => Permissions {
            can_download: true,
            can_upload: true,
            can_request: true,
            legacy_subsonic: true,
            max_bitrate_kbps: stored.max_bitrate_kbps,
        },
        Role::Member => stored,
    }
}

/// Which rules a route follows: media routes accept `mt`, admin routes need a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteClass {
    Api,
    Media,
    Admin,
}

/// Subsonic credential parameters (`u`, `t`, `s`, `p`, `apiKey`).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SubsonicCredentials {
    pub username: Option<String>,
    pub token: Option<String>,
    pub salt: Option<String>,
    pub password: Option<String>,
    pub api_key: Option<String>,
}

impl std::fmt::Debug for SubsonicCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubsonicCredentials")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// Subsonic authentication failures, each with its Subsonic error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SubsonicAuthError {
    #[error("required parameter is missing")]
    Missing,
    #[error("wrong username or password")]
    WrongCredentials,
    #[error("token authentication not supported")]
    TokenNotSupported,
    #[error("authentication mechanism not supported")]
    MechanismNotSupported,
    #[error("multiple conflicting authentication mechanisms provided")]
    Conflicting,
    #[error("invalid API key")]
    InvalidApiKey,
    #[error("rate limited")]
    RateLimited { retry_after: Duration },
    #[error("internal error")]
    Internal,
}

impl SubsonicAuthError {
    /// The Subsonic error code.
    pub const fn code(self) -> u16 {
        match self {
            Self::Missing => 10,
            Self::WrongCredentials | Self::RateLimited { .. } => 40,
            Self::TokenNotSupported => 41,
            Self::MechanismNotSupported => 42,
            Self::Conflicting => 43,
            Self::InvalidApiKey => 44,
            Self::Internal => 0,
        }
    }
}

/// A signed-in browser session.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionCookie {
    /// The cookie value; only its hash is stored.
    pub value: String,
    pub expires_at: Millis,
}

impl SessionCookie {
    /// The `Set-Cookie` value. `secure` is false only for plain HTTP from a local address
    /// (see [`session_cookie_secure`]), where the cookie drops `Secure` and the `__Host-`
    /// prefix so a LAN-only install still works.
    pub fn set_cookie(&self, secure: bool, now: Millis) -> String {
        let max_age = self.expires_at.since(now).as_secs();
        if secure {
            format!(
                "{SESSION_COOKIE}={}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Strict",
                self.value
            )
        } else {
            format!(
                "{PLAIN_SESSION_COOKIE}={}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict",
                self.value
            )
        }
    }

    /// The `Set-Cookie` value that signs the browser out.
    pub fn clear_cookie(secure: bool) -> String {
        if secure {
            format!("{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict")
        } else {
            format!("{PLAIN_SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict")
        }
    }
}

/// Whether a session cookie must be `Secure`: always, except over plain HTTP from a private
/// or loopback client.
pub fn session_cookie_secure(over_tls: bool, client: IpAddr) -> bool {
    over_tls || !is_local_address(client)
}

impl std::fmt::Debug for SessionCookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionCookie")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// A device key handed out once, with the principal it authenticates as (for the
/// **`PairResult`** response).
#[derive(Clone)]
pub struct IssuedDeviceKey {
    /// `tsk_...`, shown exactly once.
    pub secret: String,
    pub key: KeyId,
    pub device: DeviceId,
    pub device_name: String,
    pub principal: Principal,
}

impl std::fmt::Debug for IssuedDeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedDeviceKey")
            .field("key", &self.key)
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

/// An API key or app password handed out once.
#[derive(Clone)]
pub struct IssuedKey {
    pub secret: String,
    pub key: ApiKeyRow,
    /// The username to type next to an app password.
    pub username: String,
}

impl std::fmt::Debug for IssuedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedKey")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// A token for `mt=` on media URLs.
#[derive(Clone, PartialEq, Eq)]
pub struct MediaToken {
    pub token: String,
    pub expires_at: Millis,
}

impl std::fmt::Debug for MediaToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaToken")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Whether the first admin exists yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupState {
    NeedsAdmin,
    Done,
}

/// A user to create, with the chosen password in plain text.
#[derive(Clone)]
pub struct NewUser {
    pub username: String,
    pub display_name: String,
    pub password: zeroize::Zeroizing<String>,
    pub role: Role,
    pub permissions: Permissions,
    pub libraries: Vec<trackside_core::LibraryId>,
}

impl std::fmt::Debug for NewUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewUser")
            .field("username", &self.username)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

/// `POST /admin/invites`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewInvite {
    pub role: Role,
    pub permissions: Permissions,
    pub libraries: Vec<trackside_core::LibraryId>,
    pub note: Option<String>,
    pub expires_in: Duration,
}

/// An invite link token, shown once.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedInvite {
    pub id: InviteId,
    pub token: String,
    pub expires_at: Millis,
}

impl std::fmt::Debug for IssuedInvite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedInvite")
            .field("id", &self.id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// A password reset token, shown once.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedReset {
    pub token: String,
    pub expires_at: Millis,
}

impl std::fmt::Debug for IssuedReset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedReset")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// A pairing offer: the typed code and the QR secret, shown once. The API builds the QR
/// payload around `qr_secret`.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingOffer {
    pub id: PairingId,
    /// `XXXX-XXXX`.
    pub code: String,
    pub qr_secret: String,
    pub expires_at: Millis,
}

impl std::fmt::Debug for PairingOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingOffer")
            .field("id", &self.id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// A device-initiated pairing request.
#[derive(Clone, PartialEq, Eq)]
pub struct PairRequest {
    pub id: PairingId,
    /// Six digits shown on the device, as `123 456`.
    pub code: String,
    pub poll_secret: String,
    pub expires_at: Millis,
    pub poll_interval: Duration,
}

impl std::fmt::Debug for PairRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairRequest")
            .field("id", &self.id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// What the approver sees after approving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequestApproved {
    pub device_name: Option<String>,
    pub platform: Option<String>,
}

/// A poll of a pairing request. The approved key is returned exactly once; later polls are
/// `AuthError::NotFound`.
#[derive(Clone, Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "polled every two seconds; boxing would change the shared signature"
)]
pub enum PairPoll {
    Pending,
    Approved(IssuedDeviceKey),
    Expired,
}

/// Authentication and credential management. One per server, shared as `Arc<Auth>`.
pub struct Auth {
    db: Db,
    config: AuthConfig,
    secret: Arc<SecretKey>,
    media_key: Zeroizing<[u8; 32]>,
    trusted_proxies: Vec<ipnet::IpNet>,
    /// `server.local_networks`: more networks that count as nearby, beside the private,
    /// loopback, link-local and Tailscale ranges.
    local_networks: Vec<ipnet::IpNet>,
    /// `server.tailscale_is_local`: whether 100.64.0.0/10 counts as nearby.
    tailscale_is_local: bool,
    clock: Arc<dyn Clock>,
    hasher: password::Hasher,
    ip_failures: limit::FailureLimiter<limit::IpKey>,
    backoff: limit::Backoff,
    approvals: limit::FailureLimiter<UserKey>,
    key_requests: limit::RequestLimiter<KeyId>,
    cache: Arc<cache::CredentialCache>,
    /// Media epochs of devices seen authenticating, so a media token can be minted without
    /// a database read. The epoch is checked against the database when the token is used.
    epochs: Mutex<HashMap<DeviceKey, i64>>,
    touched: Mutex<HashMap<KeyId, Millis>>,
    setup_done: AtomicBool,
    untrusted_proxy: AtomicBool,
    /// `auth.legacy_subsonic`: starts from the configuration, and the dashboard's setting
    /// changes it while the server runs.
    legacy_subsonic: AtomicBool,
    setup_token_file: Mutex<Option<PathBuf>>,
    streams: limit::Slots,
    event_streams: limit::Slots,
    transcodes: limit::Slots,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("trusted_proxies", &self.trusted_proxies)
            .finish_non_exhaustive()
    }
}

/// Wrong pairing-request codes a signed-in user may enter in 10 minutes.
const APPROVAL_ATTEMPTS: u32 = 5;

impl Auth {
    /// Builds the service. When called inside a tokio runtime it also starts listening for
    /// `CredentialRevoked` events, so revocations made elsewhere in the process drop cached
    /// credentials at once.
    pub fn new(
        db: Db,
        config: &AuthConfig,
        secret: SecretKey,
        trusted_proxies: Vec<ipnet::IpNet>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let cache = Arc::new(cache::CredentialCache::default());
        spawn_revocation_listener(db.events(), &cache);
        let rate = config.login_rate;
        Self {
            media_key: secret.subkey("media token"),
            secret: Arc::new(secret),
            hasher: password::Hasher::new(config.argon2_memory, config.argon2_iterations),
            ip_failures: limit::FailureLimiter::per(
                rate.count,
                rate.per,
                limit::IP_BURST,
                Arc::clone(&clock),
            ),
            backoff: limit::Backoff::new(Arc::clone(&clock)),
            // The fifth wrong code takes more than the burst holds, which blocks what follows.
            approvals: limit::FailureLimiter::per(
                APPROVAL_ATTEMPTS,
                Duration::from_secs(600),
                APPROVAL_ATTEMPTS - 1,
                Arc::clone(&clock),
            ),
            key_requests: limit::RequestLimiter::new(50, 200, Arc::clone(&clock)),
            cache,
            epochs: Mutex::new(HashMap::new()),
            touched: Mutex::new(HashMap::new()),
            setup_done: AtomicBool::new(false),
            untrusted_proxy: AtomicBool::new(false),
            legacy_subsonic: AtomicBool::new(config.legacy_subsonic),
            setup_token_file: Mutex::new(None),
            streams: limit::Slots::new(STREAMS_PER_USER),
            event_streams: limit::Slots::new(EVENT_STREAMS_PER_USER),
            transcodes: limit::Slots::new(TRANSCODES_PER_USER),
            db,
            config: config.clone(),
            trusted_proxies,
            local_networks: Vec::new(),
            tailscale_is_local: true,
            clock,
        }
    }

    /// Adds `server.local_networks`: networks whose clients may send passwords and keys over
    /// plain HTTP and get a plain session cookie, like private addresses.
    #[must_use]
    pub fn with_local_networks(mut self, networks: Vec<ipnet::IpNet>) -> Self {
        self.local_networks = networks;
        self
    }

    /// Sets `server.tailscale_is_local`. Off, Tailscale's 100.64.0.0/10 counts as the
    /// internet, as it is where an ISP uses it for carrier-grade NAT.
    #[must_use]
    pub fn with_tailscale_is_local(mut self, on: bool) -> Self {
        self.tailscale_is_local = on;
        self
    }

    /// Private, loopback, link-local, unique-local, Tailscale (unless turned off), or in
    /// `server.local_networks`.
    pub(crate) fn is_nearby(&self, ip: IpAddr) -> bool {
        is_local_address(ip)
            || (self.tailscale_is_local && proof::is_tailscale_address(ip))
            || proxy::is_trusted(&self.local_networks, ip)
    }

    /// Whether a password or key may be accepted on this request: over HTTPS (directly, or
    /// through a trusted proxy that says so), or over plain HTTP from a nearby client. A
    /// request without a peer address (a router driven in-process) counts as nearby.
    pub fn secrets_allowed(&self, parts: &Parts) -> bool {
        if parts.extensions.get::<Https>().is_some() {
            return true;
        }
        let Some(peer) = parts
            .extensions
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|info| info.0)
        else {
            return true;
        };
        let headers = &parts.headers;
        if proxy::is_trusted(&self.trusted_proxies, peer.ip()) {
            return proxy::forwarded_https(headers)
                || self.is_nearby(proxy::client_ip(&self.trusted_proxies, headers, peer));
        }
        // A peer that is not in `server.trusted_proxies` is judged by its own address only:
        // its forwarding headers could name anyone. A nearby proxy that forwards (and so may
        // forward clients from the internet over plain HTTP) is refused until the admin adds
        // it to `server.trusted_proxies`; the warning in [`Auth::client_ip`] says so.
        self.is_nearby(peer.ip()) && !proxy::has_forwarding(headers)
    }

    /// [`Auth::secrets_allowed`] as an error: [`AuthError::HttpsRequired`].
    pub fn require_secure_transport(&self, parts: &Parts) -> Result<(), AuthError> {
        if self.secrets_allowed(parts) {
            Ok(())
        } else {
            Err(AuthError::HttpsRequired)
        }
    }

    /// The client's address: the peer, or the forwarded address when the peer is a trusted
    /// proxy. A local peer that forwards for others without being trusted is noted once.
    pub fn client_ip(&self, parts: &Parts, peer: SocketAddr) -> IpAddr {
        if !proxy::is_trusted(&self.trusted_proxies, peer.ip())
            && is_local_address(peer.ip())
            && proxy::has_forwarding(&parts.headers)
            && !self
                .untrusted_proxy
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                peer = %peer.ip(),
                "requests arrive through a proxy that is not in server.trusted_proxies: every client shares its address for rate limits, none counts as local, and passwords and keys over plain HTTP through it are refused (https_required); add the proxy to server.trusted_proxies"
            );
        }
        proxy::client_ip(&self.trusted_proxies, &parts.headers, peer)
    }

    /// Whether `u`+`t`+`s` and `u`+`p` are accepted at all, and app passwords can be made.
    pub fn legacy_subsonic(&self) -> bool {
        self.legacy_subsonic
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Turns legacy Subsonic auth on or off server-wide. It applies to the next request, so
    /// turning it off stops every app password at once.
    pub fn set_legacy_subsonic(&self, on: bool) {
        self.legacy_subsonic
            .store(on, std::sync::atomic::Ordering::Release);
    }

    /// Whether requests have come through a proxy that is not trusted, for the dashboard.
    pub fn untrusted_proxy_seen(&self) -> bool {
        self.untrusted_proxy
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether a client may get the plain HTTP session cookie: a private or loopback address
    /// that reached the server directly, or through a trusted proxy that named it. A request
    /// forwarded by a peer that is not trusted never counts, since every client behind it
    /// (a tunnel on the same host, say) would, even when other proxies are trusted.
    pub fn is_local_client(
        &self,
        headers: &http::HeaderMap,
        peer: Option<IpAddr>,
        client: IpAddr,
    ) -> bool {
        (is_local_address(client) || proxy::is_trusted(&self.local_networks, client))
            && (!proxy::has_forwarding(headers)
                || peer.is_some_and(|p| proxy::is_trusted(&self.trusted_proxies, p)))
    }

    /// A forwarding header such as `X-Forwarded-Proto` or `X-Forwarded-Host`, only when the
    /// request came from a trusted proxy (the peer is read from axum's `ConnectInfo`).
    pub fn trusted_forwarded<'p>(&self, parts: &'p Parts, name: &str) -> Option<&'p str> {
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|info| info.0);
        proxy::forwarded_value(&self.trusted_proxies, &parts.headers, peer, name)
    }

    /// The poll secret of `Authorization: Pair <secret>`.
    pub fn pair_poll_secret(parts: &Parts) -> Option<&str> {
        let value = parts
            .headers
            .get(http::header::AUTHORIZATION)?
            .to_str()
            .ok()?;
        let (scheme, secret) = value.trim().split_once(' ')?;
        scheme.eq_ignore_ascii_case("pair").then(|| secret.trim())
    }

    /// Seals a secret setting under the server's key, for storing in the `settings` table.
    pub fn seal_setting(&self, name: &str, value: &str) -> Result<Vec<u8>, AuthError> {
        seal(&self.secret, &format!("setting {name}"), value.as_bytes())
    }

    /// Opens what [`Self::seal_setting`] sealed for the same setting.
    pub fn open_setting(&self, name: &str, sealed: &[u8]) -> Result<String, AuthError> {
        let plain = open(&self.secret, &format!("setting {name}"), sealed)?;
        String::from_utf8(plain.to_vec())
            .map_err(|_| AuthError::Invalid(String::from("cannot open this sealed value")))
    }

    /// Whether the secret key matches the one the database was last used with. On first use
    /// it records the key. False means app passwords and pending pairings sealed under the
    /// old key no longer open; the server logs that and the dashboard says so.
    pub async fn check_secret_key(&self) -> Result<bool, AuthError> {
        const KEY: &str = "auth.secret_key_fingerprint";
        let fingerprint = self.secret.fingerprint();
        self.db
            .write(move |tx| {
                let stored: Option<String> = trackside_core::repo::settings::get(tx.conn(), KEY)?;
                match stored {
                    Some(stored) if stored == fingerprint => Ok(true),
                    Some(_) => {
                        tracing::warn!(
                            "the secret key changed: app passwords and pending pairings made with the old key no longer work"
                        );
                        trackside_core::repo::settings::put(tx, KEY, &fingerprint)?;
                        Ok(false)
                    }
                    None => {
                        trackside_core::repo::settings::put(tx, KEY, &fingerprint)?;
                        Ok(true)
                    }
                }
            })
            .await
            .map_err(AuthError::lift)
    }

    /// Takes one of the user's 8 stream slots; the slot frees when the permit drops.
    pub fn stream_slot(
        &self,
        principal: &Principal,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, AuthError> {
        self.streams
            .try_take(principal.access.user)
            .ok_or(AuthError::rate_limited(Duration::from_secs(1)))
    }

    /// Takes one of the user's 16 event-stream slots; the slot frees when the stream ends.
    pub fn event_stream_slot(
        &self,
        principal: &Principal,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, AuthError> {
        self.event_streams
            .try_take(principal.access.user)
            .ok_or(AuthError::rate_limited(Duration::from_secs(5)))
    }

    /// Takes one of the user's 3 transcode slots.
    pub fn transcode_slot(
        &self,
        principal: &Principal,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, AuthError> {
        self.transcodes
            .try_take(principal.access.user)
            .ok_or(AuthError::rate_limited(Duration::from_secs(1)))
    }

    fn now(&self) -> Millis {
        self.clock.now()
    }

    async fn read<T, F>(&self, f: F) -> Result<T, AuthError>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection) -> Result<T, DbError> + Send + 'static,
    {
        self.db.read(f).await.map_err(AuthError::lift)
    }

    async fn write<T, F>(&self, f: F) -> Result<T, AuthError>
    where
        T: Send + 'static,
        F: FnOnce(&mut WriteTx<'_>) -> Result<T, DbError> + Send + 'static,
    {
        self.db.write(f).await.map_err(AuthError::lift)
    }

    /// Drops cached credentials and tells the rest of the server (SSE streams, other caches).
    fn revoked(&self, user: UserKey, device: Option<DeviceKey>) {
        match device {
            Some(device) => {
                self.cache.forget_device(device);
                limit::lock(&self.epochs).remove(&device);
            }
            None => self.cache.forget_user(user),
        }
        self.db
            .events()
            .publish(ServerEvent::CredentialRevoked { user, device });
    }

    /// Drops the user's cached credentials, which carry their role and grants, and tells
    /// open event streams to read them again.
    fn access_changed(&self, user: UserKey) {
        self.cache.forget_user(user);
        self.db
            .events()
            .publish(ServerEvent::AccessChanged { user });
    }

    fn remember_epoch(&self, device: DeviceKey, epoch: i64) {
        let mut epochs = limit::lock(&self.epochs);
        if epochs.len() > 100_000 {
            epochs.clear();
        }
        epochs.insert(device, epoch);
    }

    /// Refuses an attempt from an address that has used up its failures.
    fn check_ip(&self, ip: IpAddr) -> Result<(), AuthError> {
        self.ip_failures
            .check(&limit::IpKey::from(ip))
            .map_err(AuthError::rate_limited)
    }

    /// Logs a failure in the fixed fail2ban format and counts it against the address.
    /// Err when this failure was one too many.
    fn fail_ip(&self, user: &str, ip: IpAddr, via: &str) -> Result<(), AuthError> {
        tracing::warn!(
            "auth_failure user={} ip={} via={}",
            request::log_safe(user),
            ip,
            via
        );
        self.ip_failures
            .fail(&limit::IpKey::from(ip))
            .map_err(AuthError::rate_limited)
    }
}

fn spawn_revocation_listener(events: &EventBus, cache: &Arc<cache::CredentialCache>) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut events = events.subscribe();
    let cache: Weak<cache::CredentialCache> = Arc::downgrade(cache);
    runtime.spawn(async move {
        loop {
            let event = events.recv().await;
            let Some(cache) = cache.upgrade() else { break };
            match event {
                Ok(event) => match &*event {
                    ServerEvent::CredentialRevoked { user, device } => match device {
                        Some(device) => cache.forget_device(*device),
                        None => cache.forget_user(*user),
                    },
                    ServerEvent::AccessChanged { user } => cache.forget_user(*user),
                    _ => {}
                },
                Err(RecvError::Lagged(_)) => cache.clear(),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// Loads the principal for an active user. `None` for unknown or disabled users.
pub(crate) fn load_principal(
    c: &rusqlite::Connection,
    user: UserKey,
    scopes: Scopes,
    via: Via,
    device: Option<(DeviceKey, DeviceId)>,
) -> Result<Option<Principal>, DbError> {
    use trackside_core::repo::users;
    let Some(record) = users::get(c, user)? else {
        return Ok(None);
    };
    if record.disabled_at.is_some() {
        return Ok(None);
    }
    let Some(access) = users::access(c, user)? else {
        return Ok(None);
    };
    Ok(Some(Principal {
        access,
        user: record.id,
        username: record.username,
        role: record.role,
        permissions: effective_permissions(record.role, record.permissions),
        scopes,
        via,
        device,
        key: None,
        session: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_parse_and_print() {
        let scopes = Scopes::parse("library stream,write");
        assert_eq!(scopes, Some(Scopes::KEY_DEFAULT));
        assert_eq!(Scopes::KEY_DEFAULT.to_string(), "library stream write");
        assert_eq!(Scopes::parse("library admin"), None);
        assert!(Scopes::ALL.contains(Scopes::REQUEST));
        assert!(!Scopes::KEY_DEFAULT.contains(Scopes::DOWNLOAD));
        assert_eq!(Scopes::from_bits(Scopes::WRITE.bits()), Scopes::WRITE);
    }

    #[test]
    fn subsonic_errors_carry_their_codes() {
        assert_eq!(SubsonicAuthError::Conflicting.code(), 43);
        assert_eq!(SubsonicAuthError::TokenNotSupported.code(), 41);
        assert_eq!(
            SubsonicAuthError::RateLimited {
                retry_after: Duration::from_secs(3)
            }
            .code(),
            40
        );
    }

    #[test]
    fn secrets_never_show_in_debug() {
        let key = SecretKey::from_bytes([7; 32]);
        assert_eq!(format!("{key:?}"), "SecretKey(<redacted>)");
        let credentials = SubsonicCredentials {
            username: Some(String::from("luke")),
            password: Some(String::from("hunter2")),
            ..SubsonicCredentials::default()
        };
        assert!(!format!("{credentials:?}").contains("hunter2"));
        let cookie = SessionCookie {
            value: String::from("cookie-secret"),
            expires_at: Millis(5),
        };
        assert!(!format!("{cookie:?}").contains("cookie-secret"));
        let offer = PairingOffer {
            id: trackside_core::random_id(),
            code: String::from("ABCD-EFGH"),
            qr_secret: String::from("qr-secret"),
            expires_at: Millis(5),
        };
        assert!(!format!("{offer:?}").contains("qr-secret"));
    }

    #[test]
    fn session_cookie_attributes() {
        let cookie = SessionCookie {
            value: String::from("abc"),
            expires_at: Millis(90_000),
        };
        assert_eq!(
            cookie.set_cookie(true, Millis(0)),
            "__Host-trackside_session=abc; Path=/; Max-Age=90; HttpOnly; Secure; SameSite=Strict"
        );
        assert_eq!(
            cookie.set_cookie(false, Millis(0)),
            "trackside_session=abc; Path=/; Max-Age=90; HttpOnly; SameSite=Strict"
        );
        assert!(SessionCookie::clear_cookie(true).contains("Max-Age=0"));
    }

    #[test]
    fn cookies_are_secure_except_plain_http_on_the_lan() {
        let lan: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 9));
        let internet: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 9));
        assert!(!session_cookie_secure(false, lan));
        assert!(session_cookie_secure(true, lan));
        assert!(session_cookie_secure(false, internet));
    }

    #[test]
    fn admins_have_every_permission() {
        let stored = Permissions {
            can_download: false,
            can_upload: false,
            can_request: false,
            legacy_subsonic: false,
            max_bitrate_kbps: Some(128),
        };
        let admin = effective_permissions(Role::Admin, stored.clone());
        assert!(admin.can_download && admin.can_request && admin.legacy_subsonic);
        assert_eq!(admin.max_bitrate_kbps, Some(128));
        assert_eq!(effective_permissions(Role::Member, stored.clone()), stored);
    }

    #[test]
    fn request_scope_also_needs_can_request() {
        let principal = Principal {
            access: Access::system(),
            user: trackside_core::random_id(),
            username: String::from("mia"),
            role: Role::Member,
            permissions: Permissions {
                can_request: false,
                ..Permissions::default()
            },
            scopes: Scopes::ALL,
            via: Via::DeviceKey,
            device: None,
            key: None,
            session: None,
        };
        assert!(!principal.allows(Scopes::REQUEST));
        assert!(principal.allows(Scopes::WRITE));
        assert!(principal.allows(Scopes::DOWNLOAD));
    }
}
