//! Serde models for what Music Assistant sends. Decoding is tolerant throughout: unknown fields
//! are ignored, unknown enum values are kept as `Other`, `null` becomes the default, and an
//! entry of a list that does not decode is dropped rather than failing the list.

pub(crate) mod enums;
mod media;
mod player;
mod server;

pub use enums::{
    FlowStepType, ImageType, MediaType, PlaybackState, PlayerFeature, PlayerType, ProviderType,
    QueueOption, RepeatMode,
};
pub use media::{
    AudioFormat, ExternalId, Image, MediaItem, Metadata, ProviderMapping, SearchResults,
};
pub use player::{DeviceInfo, Player, PlayerMedia, PlayerQueue, QueueItem};
pub use server::{
    ProviderConfig, ProviderError, ProviderInstance, ProviderManifest, ServerInfo, SetupFlowStep,
    User,
};
