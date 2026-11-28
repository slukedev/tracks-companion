//! Music Assistant as the HTTP crates see it (ADR 0013): speakers, the admin connection and
//! Trackside's own registration in it. `trackside-assistant` implements [`AssistantControl`]
//! and `trackside-server` wires it in; [`NoAssistant`] stands in when it is compiled out.
//!
//! Nothing here downloads, stores or exports audio: speakers play streaming services in Music
//! Assistant itself, and Trackside only tells them what to play.

use std::sync::Arc;
use std::time::Duration;

use crate::access::Access;
use crate::config::Secret;
use crate::control::BoxFuture;
use crate::db::DbError;
use crate::ids::{
    ArtworkId, CatalogArtId, DeviceId, DeviceKey, KeyId, LibraryKey, RelaySessionId, ServiceId,
    SongId, SpeakerCommandId, SpeakerId, UserId, UserKey,
};
use crate::relay::{RelayControl, RelayView};
use crate::time::Millis;

/// The newest Music Assistant schema Trackside was tested with (2.11 beta).
pub const TESTED_SCHEMA: u32 = 128;
/// The schema of Music Assistant 2.10 stable, the oldest Trackside calls compatible.
pub const STABLE_SCHEMA: u32 = 65;

/// How long a command waits for its outcome before the client re-reads.
pub const SETTLE: Duration = Duration::from_secs(5);
/// The same for `play` and `transfer`.
pub const SETTLE_SLOW: Duration = Duration::from_secs(15);

macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

wire_enum!(
    /// What a speaker is, for its glyph.
    SpeakerKind {
        Speaker => "speaker",
        Group => "group",
        StereoPair => "stereo_pair",
        Phone => "phone",
        Unknown => "unknown",
    }
);

wire_enum!(
    /// Whether Music Assistant can reach a speaker.
    Availability {
        Available => "available",
        Unavailable => "unavailable",
        PoweredOff => "powered_off",
    }
);

wire_enum!(
    /// A control a speaker offers (contract 1.3). Sorted by name, as on the wire.
    Capability {
        Group => "group",
        Mute => "mute",
        NextPrevious => "next_previous",
        Pause => "pause",
        Play => "play",
        Power => "power",
        Queue => "queue",
        Repeat => "repeat",
        Seek => "seek",
        Shuffle => "shuffle",
        Stop => "stop",
        Transfer => "transfer",
        Volume => "volume",
    }
);

wire_enum!(
    /// What a speaker is doing.
    PlayState {
        Playing => "playing",
        Paused => "paused",
        Idle => "idle",
        Buffering => "buffering",
        Unknown => "unknown",
    }
);

wire_enum!(
    Repeat {
        Off => "off",
        One => "one",
        All => "all",
    }
);

wire_enum!(
    GroupRole {
        Leader => "leader",
        Member => "member",
    }
);

wire_enum!(
    /// Whether what plays came from the queue Trackside controls.
    Origin {
        Queue => "queue",
        Other => "other",
    }
);

wire_enum!(
    /// Where the speakers list stands (contract 1.2).
    SpeakersState {
        Ok => "ok",
        Connecting => "connecting",
        Unavailable => "unavailable",
    }
);

wire_enum!(
    /// The connection as the dashboard shows it (contract 4.1).
    ConnectionState {
        NotSetUp => "not_set_up",
        Disabled => "disabled",
        Connecting => "connecting",
        Connected => "connected",
        Unreachable => "unreachable",
        AuthFailed => "auth_failed",
        Incompatible => "incompatible",
        SetupRequired => "setup_required",
    }
);

wire_enum!(
    /// Trackside's own registration in Music Assistant (contract 4.4).
    RegistrationState {
        NotRegistered => "not_registered",
        Registering => "registering",
        Registered => "registered",
        NeedsAdmin => "needs_admin",
        Failed => "failed",
        Stale => "stale",
    }
);

wire_enum!(
    PlaybackAction {
        Play => "play",
        Pause => "pause",
        Toggle => "toggle",
        Stop => "stop",
        Next => "next",
        Previous => "previous",
    }
);

wire_enum!(
    PlayMode {
        Replace => "replace",
        Next => "next",
        Add => "add",
    }
);

/// The playback position at a server time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playback {
    pub state: PlayState,
    pub position_ms: u64,
    pub position_at: Millis,
    pub shuffle: bool,
    pub repeat: Repeat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeakerGroup {
    pub leader_id: SpeakerId,
    /// Leader first, as Music Assistant lists them.
    pub member_ids: Vec<SpeakerId>,
    pub role: GroupRole,
    /// The members are fixed in Music Assistant.
    pub fixed: bool,
}

/// What a queue was started from, echoed from the last `play`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayContext {
    /// `album`, `artist`, `playlist`, `mix`, `station`, `search`, `catalog` or `other`: open.
    pub kind: String,
    pub id: Option<String>,
    pub name: Option<String>,
}

/// A library song Music Assistant plays from Trackside's own registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibrarySong {
    pub id: SongId,
    /// Who may be told: callers who see this library.
    pub library: LibraryKey,
    pub artwork: Option<ArtworkId>,
}

/// A streaming-service item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogItem {
    /// The ref (see [`crate::catalog_ref`]).
    pub reference: String,
    pub service: ServiceId,
    pub artwork: Option<CatalogArtId>,
}

/// One playable item as a speaker or its queue shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaInfo {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub song: Option<LibrarySong>,
    pub catalog: Option<CatalogItem>,
    /// `tracks_model::song_key(title, artist)`.
    pub song_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowPlaying {
    pub media: MediaInfo,
    /// Music Assistant's queue item id, for `jump`.
    pub item_id: Option<String>,
    pub origin: Origin,
}

/// The phone relay behind a `kind: phone` speaker. Only its owner sees the speaker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeakerRelay {
    pub session: RelaySessionId,
    pub user: UserKey,
    pub device: DeviceKey,
    pub device_id: DeviceId,
}

/// A speaker (contract 1.1). Shared between every caller; per-caller visibility (a song in a
/// library the caller does not see) is applied when it is put on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Speaker {
    pub id: SpeakerId,
    pub name: String,
    pub kind: SpeakerKind,
    /// `airplay`, `cast`, `sonos`, ...: open.
    pub protocol: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub availability: Availability,
    /// Sorted, no duplicates.
    pub capabilities: Vec<Capability>,
    pub powered: Option<bool>,
    pub volume: Option<u8>,
    pub muted: Option<bool>,
    pub group: Option<SpeakerGroup>,
    pub group_volume: Option<u8>,
    pub can_group_with: Vec<SpeakerId>,
    pub playback: Option<Playback>,
    pub now_playing: Option<NowPlaying>,
    pub context: Option<PlayContext>,
    /// Relay speakers only.
    pub relay: Option<SpeakerRelay>,
    pub revision: u64,
}

impl Speaker {
    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.binary_search(&capability).is_ok()
    }
}

/// `GET /speakers`.
#[derive(Clone, Debug, Default)]
pub struct SpeakerList {
    pub speakers: Vec<Arc<Speaker>>,
    pub state: Option<SpeakersState>,
    /// When the state became `unavailable`.
    pub since: Option<Millis>,
}

/// The patchable controls of `PATCH /speakers/{id}`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpeakerPatch {
    pub volume: Option<u8>,
    pub muted: Option<bool>,
    pub powered: Option<bool>,
    pub shuffle: Option<bool>,
    pub repeat: Option<Repeat>,
    pub group_volume: Option<u8>,
}

impl SpeakerPatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayItem {
    Song(SongId),
    /// A catalog ref, decoded by the implementation.
    Ref(String),
}

/// `POST /speakers/{id}/play` (contract 1.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayRequest {
    pub items: Vec<PlayItem>,
    pub mode: PlayMode,
    pub start_index: usize,
    pub position_ms: u64,
    pub shuffle: Option<bool>,
    pub repeat: Option<Repeat>,
    pub context: Option<PlayContext>,
    /// The caller may play streaming-service items (`can_play_services`); without it, refs
    /// are skipped like any item the caller cannot play.
    pub allow_catalog: bool,
}

/// The most items one `play` may carry.
pub const MAX_PLAY_ITEMS: usize = 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpeakerCommand {
    Playback(PlaybackAction),
    Seek {
        position_ms: u64,
    },
    Jump {
        item_id: String,
    },
    Set(SpeakerPatch),
    /// The full list of other speakers to group with this one as leader.
    Members(Vec<SpeakerId>),
    LeaveGroup,
    Play(PlayRequest),
    Transfer {
        to: SpeakerId,
        keep_playing_here: bool,
    },
}

impl SpeakerCommand {
    /// The `action` of the command's `202`.
    pub fn action(&self) -> &'static str {
        match self {
            Self::Playback(action) => action.as_str(),
            Self::Seek { .. } => "seek",
            Self::Jump { .. } => "jump",
            Self::Set(_) => "update",
            Self::Members(_) => "members",
            Self::LeaveGroup => "ungroup",
            Self::Play(_) => "play",
            Self::Transfer { .. } => "transfer",
        }
    }

    pub fn settle(&self) -> Duration {
        match self {
            Self::Play(_) | Self::Transfer { .. } => SETTLE_SLOW,
            _ => SETTLE,
        }
    }
}

/// The `202` of a command.
#[derive(Clone, Debug)]
pub struct CommandAccepted {
    pub action: &'static str,
    pub command_id: SpeakerCommandId,
    pub settle_by: Millis,
    /// The speaker as Trackside expects it after the command, at the revision the
    /// expectation is based on.
    pub speaker: Arc<Speaker>,
}

/// A page of a speaker's queue.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueuePage {
    pub cursor: Option<String>,
    pub limit: u32,
    pub history: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueEntry {
    pub index: u32,
    pub item_id: String,
    pub media: MediaInfo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeakerQueue {
    pub id: SpeakerId,
    pub current_index: Option<u32>,
    pub items: Vec<QueueEntry>,
    pub next_cursor: Option<String>,
    pub repeat: Repeat,
    pub shuffle: bool,
    pub revision: u64,
    pub total: u32,
}

/// Why a speaker request was refused before anything was sent, or failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SpeakerError {
    #[error("no such speaker")]
    NotFound,
    #[error("Music Assistant is not reachable")]
    Unavailable,
    #[error("the speaker is unavailable")]
    Offline,
    #[error("the speaker is off")]
    PoweredOff,
    #[error("the speaker cannot {capability}")]
    Unsupported {
        capability: &'static str,
        /// Set when the offending speaker is not the one in the path.
        speaker: Option<SpeakerId>,
    },
    #[error("nothing in the request can play")]
    NotPlayable {
        song: Option<SongId>,
        reference: Option<String>,
    },
    #[error("the service is signed out in Music Assistant")]
    ServiceSignedOut { service: Option<ServiceId> },
    #[error("{message}")]
    Invalid {
        field: &'static str,
        message: String,
    },
    #[error("Music Assistant refused: {0}")]
    Failed(String),
    /// The relay session behind this speaker ended.
    #[error("the relay session ended")]
    RelayEnded,
}

/// The outcome of a `202` command, as its SSE event says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandFailure {
    /// One of contract 0.4's codes, `command_failed` or `timed_out`.
    pub code: &'static str,
    pub message: String,
}

/// What speakers publish on the event bus (contract 1.8).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum SpeakerEvent {
    Added(Arc<Speaker>),
    Updated(Arc<Speaker>),
    Removed(SpeakerId),
    Progress {
        id: SpeakerId,
        position_ms: u64,
        position_at: Millis,
        revision: u64,
        state: PlayState,
    },
    Queue {
        id: SpeakerId,
        revision: u64,
    },
    /// Only to the user who sent the command.
    Command {
        user: UserKey,
        command: SpeakerCommandId,
        speaker: SpeakerId,
        outcome: Result<(), CommandFailure>,
    },
    State {
        state: SpeakersState,
        since: Option<Millis>,
    },
    /// An event about a relay speaker: only for its owner.
    Owned {
        user: UserKey,
        event: Box<SpeakerEvent>,
    },
    /// A relay session changed (SSE `relay`): only for its owner.
    Relay(Box<RelayView>),
}

/// Where each connection setting comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingSource {
    Default,
    Dashboard,
    Config,
}

impl SettingSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Dashboard => "dashboard",
            Self::Config => "config",
        }
    }
}

/// Facts from Music Assistant's server info.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerFacts {
    pub server_id: String,
    pub name: Option<String>,
    pub version: String,
    pub schema_version: u32,
    pub min_supported_schema_version: u32,
    pub compatible: bool,
    pub tested: bool,
    pub relay_supported: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistantUser {
    pub username: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanUrl {
    pub url: String,
    pub configured: bool,
    /// Music Assistant reports Trackside's source as available.
    pub reachable: bool,
    pub checked_at: Option<Millis>,
    /// Why the address may not work (loopback while Music Assistant is elsewhere).
    pub warning: Option<String>,
}

/// `GET /admin/music-assistant`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(clippy::struct_excessive_bools, reason = "independent flags")]
pub struct AssistantStatus {
    pub configured: bool,
    pub enabled: bool,
    pub auto_register: bool,
    pub state: ConnectionState,
    pub since: Option<Millis>,
    pub last_error: Option<(Millis, String)>,
    pub hidden_services: Vec<String>,
    pub lan_url: Option<LanUrl>,
    pub relays_active: u32,
    pub relays_max: u32,
    pub server: Option<ServerFacts>,
    pub services: Option<u32>,
    pub speakers: Option<u32>,
    pub url_source: SettingSource,
    pub token_source: SettingSource,
    pub token_set: bool,
    pub token_set_at: Option<Millis>,
    pub custom_ca: bool,
    pub ui_url: Option<String>,
    pub url: Option<String>,
    pub user: Option<AssistantUser>,
}

/// `PATCH /admin/music-assistant`. The outer `Option` is "leave as it is"; an inner `None`
/// clears the value.
#[derive(Clone, Debug, Default)]
pub struct AssistantChange {
    pub auto_register: Option<bool>,
    pub enabled: Option<bool>,
    pub hidden_services: Option<Vec<String>>,
    pub lan_url: Option<Option<String>>,
    pub relay_max_sessions: Option<u32>,
    pub token: Option<Option<Secret>>,
    pub url: Option<Option<String>>,
    /// A PEM certificate authority for `https`.
    pub ca_pem: Option<Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AssistantError {
    #[error("{message}")]
    Invalid {
        field: &'static str,
        message: String,
    },
    #[error("{field} is set in the configuration")]
    SetByConfig { field: &'static str },
    #[error("Music Assistant is not set up")]
    NotConfigured,
    #[error("Music Assistant is not reachable")]
    Unavailable,
    #[error("{0}")]
    Failed(String),
    #[error("database: {0}")]
    Db(String),
}

impl From<DbError> for AssistantError {
    fn from(error: DbError) -> Self {
        Self::Db(error.to_string())
    }
}

/// `POST /admin/music-assistant/test`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistantTest {
    pub ok: bool,
    pub detail: String,
    pub elapsed: Duration,
    pub players: Option<u32>,
    pub providers: Option<u32>,
    pub server: Option<ServerFacts>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrationKey {
    pub id: KeyId,
    pub prefix: String,
    pub scopes: Vec<&'static str>,
    pub created_at: Millis,
    pub last_used_at: Option<Millis>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceUser {
    pub id: UserId,
    pub username: String,
    pub display_name: String,
}

/// `GET /admin/music-assistant/registration`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrationView {
    pub state: RegistrationState,
    pub base_url: Option<String>,
    pub key: Option<RegistrationKey>,
    pub provider_instance_id: Option<String>,
    pub registered_at: Option<Millis>,
    pub service_user: Option<ServiceUser>,
    pub last_error: Option<(Millis, String)>,
    pub ui_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(clippy::struct_excessive_bools, reason = "independent flags")]
pub struct MusicProvider {
    pub instance_id: String,
    pub domain: String,
    pub name: String,
    pub available: bool,
    pub streaming: bool,
    pub hidden: bool,
    pub service_id: Option<ServiceId>,
    pub trackside: bool,
    pub ui_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerProvider {
    pub instance_id: String,
    pub domain: String,
    pub name: String,
    pub available: bool,
    pub speakers: u32,
    pub ui_url: Option<String>,
}

/// `GET /admin/music-assistant/providers`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderLists {
    pub music: Vec<MusicProvider>,
    pub players: Vec<PlayerProvider>,
    pub ui_url: Option<String>,
}

/// `music_assistant` in `GET /admin/overview`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistantOverview {
    pub state: ConnectionState,
    pub registration: Option<RegistrationState>,
    pub relays: Option<u32>,
    pub services: Option<u32>,
    pub speakers: Option<u32>,
}

pub use crate::catalog::CatalogControl;

/// Music Assistant as the HTTP crates see it.
pub trait AssistantControl: Send + Sync + 'static {
    /// Music Assistant is configured and enabled: `/me` offers `features.speakers` and
    /// `features.catalog`, even while it is unreachable.
    fn enabled(&self) -> bool;

    /// Whether this server could relay streaming-service audio to a phone right now.
    fn relay_supported(&self) -> bool {
        false
    }

    /// Every speaker the caller may see, groups first, then by folded name.
    fn speakers<'a>(&'a self, caller: &'a Access) -> BoxFuture<'a, SpeakerList>;

    fn speaker<'a>(
        &'a self,
        caller: &'a Access,
        id: &'a SpeakerId,
    ) -> BoxFuture<'a, Result<Arc<Speaker>, SpeakerError>>;

    fn queue<'a>(
        &'a self,
        caller: &'a Access,
        id: &'a SpeakerId,
        page: QueuePage,
    ) -> BoxFuture<'a, Result<SpeakerQueue, SpeakerError>>;

    /// Checks and accepts a command; the outcome follows as [`SpeakerEvent::Command`].
    fn command<'a>(
        &'a self,
        caller: &'a Access,
        id: &'a SpeakerId,
        command: SpeakerCommand,
    ) -> BoxFuture<'a, Result<CommandAccepted, SpeakerError>>;

    fn status(&self) -> BoxFuture<'_, AssistantStatus>;

    /// Saves a change and reconnects in the background.
    fn update(
        &self,
        change: AssistantChange,
    ) -> BoxFuture<'_, Result<AssistantStatus, AssistantError>>;

    /// Unregisters, forgets the address and token, and disconnects.
    fn remove(&self) -> BoxFuture<'_, Result<(), AssistantError>>;

    /// Connects once with `url` and `token`, or the saved values, and disconnects.
    fn test(&self, url: Option<String>, token: Option<Secret>) -> BoxFuture<'_, AssistantTest>;

    fn registration(&self) -> BoxFuture<'_, RegistrationView>;

    /// Registers again in the background; the view is in `registering`.
    fn register(&self) -> BoxFuture<'_, Result<RegistrationView, AssistantError>>;

    fn unregister(&self) -> BoxFuture<'_, Result<(), AssistantError>>;

    fn providers(&self) -> BoxFuture<'_, Result<ProviderLists, AssistantError>>;

    fn overview(&self) -> BoxFuture<'_, AssistantOverview>;

    /// The streaming catalog, once there is one.
    fn catalog(&self) -> Option<&dyn CatalogControl> {
        None
    }

    /// The phone playback relay, when Music Assistant support is built in.
    fn relay(&self) -> Option<&dyn RelayControl> {
        None
    }
}

/// Music Assistant support compiled out, or not wired: nothing is configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoAssistant;

fn not_set_up_status() -> AssistantStatus {
    AssistantStatus {
        configured: false,
        enabled: false,
        auto_register: false,
        state: ConnectionState::NotSetUp,
        since: None,
        last_error: None,
        hidden_services: Vec::new(),
        lan_url: None,
        relays_active: 0,
        relays_max: 0,
        server: None,
        services: None,
        speakers: None,
        url_source: SettingSource::Default,
        token_source: SettingSource::Default,
        token_set: false,
        token_set_at: None,
        custom_ca: false,
        ui_url: None,
        url: None,
        user: None,
    }
}

impl AssistantStatus {
    /// The status of a server where Music Assistant is not set up.
    pub fn not_set_up() -> Self {
        not_set_up_status()
    }
}

impl AssistantControl for NoAssistant {
    fn enabled(&self) -> bool {
        false
    }

    fn speakers<'a>(&'a self, _caller: &'a Access) -> BoxFuture<'a, SpeakerList> {
        Box::pin(async { SpeakerList::default() })
    }

    fn speaker<'a>(
        &'a self,
        _caller: &'a Access,
        _id: &'a SpeakerId,
    ) -> BoxFuture<'a, Result<Arc<Speaker>, SpeakerError>> {
        Box::pin(async { Err(SpeakerError::NotFound) })
    }

    fn queue<'a>(
        &'a self,
        _caller: &'a Access,
        _id: &'a SpeakerId,
        _page: QueuePage,
    ) -> BoxFuture<'a, Result<SpeakerQueue, SpeakerError>> {
        Box::pin(async { Err(SpeakerError::NotFound) })
    }

    fn command<'a>(
        &'a self,
        _caller: &'a Access,
        _id: &'a SpeakerId,
        _command: SpeakerCommand,
    ) -> BoxFuture<'a, Result<CommandAccepted, SpeakerError>> {
        Box::pin(async { Err(SpeakerError::NotFound) })
    }

    fn status(&self) -> BoxFuture<'_, AssistantStatus> {
        Box::pin(async { not_set_up_status() })
    }

    fn update(
        &self,
        _change: AssistantChange,
    ) -> BoxFuture<'_, Result<AssistantStatus, AssistantError>> {
        Box::pin(async { Err(AssistantError::NotConfigured) })
    }

    fn remove(&self) -> BoxFuture<'_, Result<(), AssistantError>> {
        Box::pin(async { Ok(()) })
    }

    fn test(&self, _url: Option<String>, _token: Option<Secret>) -> BoxFuture<'_, AssistantTest> {
        Box::pin(async {
            AssistantTest {
                ok: false,
                detail: String::from("This server was built without Music Assistant support."),
                elapsed: Duration::ZERO,
                players: None,
                providers: None,
                server: None,
            }
        })
    }

    fn registration(&self) -> BoxFuture<'_, RegistrationView> {
        Box::pin(async {
            RegistrationView {
                state: RegistrationState::NotRegistered,
                base_url: None,
                key: None,
                provider_instance_id: None,
                registered_at: None,
                service_user: None,
                last_error: None,
                ui_url: None,
            }
        })
    }

    fn register(&self) -> BoxFuture<'_, Result<RegistrationView, AssistantError>> {
        Box::pin(async { Err(AssistantError::NotConfigured) })
    }

    fn unregister(&self) -> BoxFuture<'_, Result<(), AssistantError>> {
        Box::pin(async { Ok(()) })
    }

    fn providers(&self) -> BoxFuture<'_, Result<ProviderLists, AssistantError>> {
        Box::pin(async { Err(AssistantError::NotConfigured) })
    }

    fn overview(&self) -> BoxFuture<'_, AssistantOverview> {
        Box::pin(async {
            AssistantOverview {
                state: ConnectionState::NotSetUp,
                registration: None,
                relays: None,
                services: None,
                speakers: None,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_sort_by_their_wire_names() {
        let mut names: Vec<&str> = Capability::ALL.iter().map(|c| c.as_str()).collect();
        let sorted = {
            let mut copy = names.clone();
            copy.sort_unstable();
            copy
        };
        assert_eq!(names, sorted);
        names.dedup();
        assert_eq!(names.len(), Capability::ALL.len());
    }

    #[test]
    fn wire_names_parse_back() {
        for state in ConnectionState::ALL {
            assert_eq!(ConnectionState::parse(state.as_str()), Some(*state));
        }
        assert_eq!(Repeat::parse("shuffle"), None);
    }
}
