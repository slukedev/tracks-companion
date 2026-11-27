//! Typed commands. Argument names are the Python parameter names of the server's
//! `@api_command` handlers, checked against both 2.10 stable and 2.11 beta. Only arguments both
//! accept are sent; the server ignores unknown ones and an omitted one takes its default.

use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::client::Client;
use crate::error::RequestError;
use crate::image::{ImageFormat, ImageSize, image_url};
use crate::models::{
    FlowStepType, Image, MediaItem, MediaType, Player, PlayerQueue, ProviderConfig,
    ProviderInstance, ProviderManifest, ProviderType, QueueItem, QueueOption, RepeatMode,
    SearchResults, ServerInfo, SetupFlowStep,
};
use crate::token::Token;

/// Library listings and other long answers may arrive in many chunks.
const LISTING_TIMEOUT: Duration = Duration::from_secs(120);
/// Setting up a provider can install its requirements first (`NEXT_STEP_TIMEOUT` is 120 s).
const SETUP_TIMEOUT: Duration = Duration::from_secs(150);

/// A small builder for command arguments that leaves out what is `None`.
#[derive(Default)]
struct Args(Map<String, Value>);

impl Args {
    fn new() -> Self {
        Self::default()
    }

    fn set(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.0.insert(key.to_owned(), value.into());
        self
    }

    fn opt(self, key: &str, value: Option<impl Into<Value>>) -> Self {
        match value {
            Some(value) => self.set(key, value),
            None => self,
        }
    }

    fn build(self) -> Value {
        Value::Object(self.0)
    }
}

impl From<Args> for Option<Value> {
    fn from(args: Args) -> Self {
        Some(args.build())
    }
}

fn text<T: Serialize + ?Sized>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// A player command (`players/cmd/*`).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PlayerCommand {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    /// Seconds from the start.
    Seek(u32),
    /// 0 to 100.
    VolumeSet(u8),
    VolumeMute(bool),
    Power(bool),
    /// Join this player to `target`'s group.
    Group {
        target: String,
    },
    Ungroup,
}

/// What to search for.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct SearchQuery {
    pub query: String,
    /// Empty searches every type.
    pub media_types: Vec<MediaType>,
    pub limit: u32,
    pub library_only: bool,
    /// Provider instance ids or domains to limit the search to.
    pub providers: Vec<String>,
}

impl SearchQuery {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            media_types: Vec::new(),
            limit: 25,
            library_only: false,
            providers: Vec::new(),
        }
    }

    #[must_use]
    pub fn media_types(mut self, media_types: impl IntoIterator<Item = MediaType>) -> Self {
        self.media_types = media_types.into_iter().collect();
        self
    }

    #[must_use]
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }
}

/// A page of library items (`music/<type>s/library_items`).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct LibraryQuery {
    pub favorite: Option<bool>,
    pub search: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub order_by: Option<String>,
    pub provider: Option<String>,
}

impl LibraryQuery {
    #[must_use]
    pub fn page(limit: u32, offset: u32) -> Self {
        Self {
            limit: Some(limit),
            offset: Some(offset),
            ..Self::default()
        }
    }
}

/// What `player_queues/play_media` plays.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PlayMedia {
    /// Item URIs.
    pub media: Vec<String>,
    pub option: Option<QueueOption>,
    pub radio_mode: bool,
    /// A URI to start an album or playlist from.
    pub start_item: Option<String>,
}

impl PlayMedia {
    pub fn uri(uri: impl Into<String>) -> Self {
        Self::uris([uri.into()])
    }

    pub fn uris(uris: impl IntoIterator<Item = String>) -> Self {
        Self {
            media: uris.into_iter().collect(),
            option: None,
            radio_mode: false,
            start_item: None,
        }
    }

    #[must_use]
    pub fn option(mut self, option: QueueOption) -> Self {
        self.option = Some(option);
        self
    }

    #[must_use]
    pub fn radio_mode(mut self, radio_mode: bool) -> Self {
        self.radio_mode = radio_mode;
        self
    }

    #[must_use]
    pub fn start_item(mut self, uri: impl Into<String>) -> Self {
        self.start_item = Some(uri.into());
        self
    }
}

/// An `OpenSubsonic` source for Music Assistant's `opensubsonic` provider. Field meanings follow
/// its setup flow (`providers/opensubsonic/setup_flow.py`): `baseURL` is scheme and host,
/// `port` and `path` are separate.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct OpenSubsonicSource {
    /// For example `http://192.168.1.10`.
    pub base_url: String,
    pub port: Option<u16>,
    /// A path prefix before `/rest`, if any.
    pub path: Option<String>,
    pub api_key: Token,
    /// The name the instance shows in Music Assistant.
    pub name: Option<String>,
}

impl OpenSubsonicSource {
    pub fn new(base_url: impl Into<String>, api_key: Token) -> Self {
        Self {
            base_url: base_url.into(),
            port: None,
            path: None,
            api_key,
            name: None,
        }
    }
}

/// The `OpenSubsonic` provider's domain.
pub const OPENSUBSONIC_DOMAIN: &str = "opensubsonic";

/// Why registering a provider instance did not finish.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SetupError {
    #[error(transparent)]
    Request(#[from] RequestError),
    #[error("Music Assistant aborted the setup: {0}")]
    Aborted(String),
    #[error("Music Assistant rejected the values: {0}")]
    Rejected(String),
    #[error("the setup flow asked for something unexpected ({0})")]
    Unexpected(String),
}

impl Client {
    pub async fn server_info(&self) -> Result<ServerInfo, RequestError> {
        self.call("info", None).await
    }

    /// Loaded provider instances, optionally of one type.
    pub async fn providers(
        &self,
        provider_type: Option<ProviderType>,
    ) -> Result<Vec<ProviderInstance>, RequestError> {
        let args = Args::new().opt("provider_type", provider_type.as_ref().map(text));
        self.list("providers", args.into()).await
    }

    pub async fn provider_manifests(&self) -> Result<Vec<ProviderManifest>, RequestError> {
        self.list("providers/manifests", None).await
    }

    pub async fn search(&self, query: &SearchQuery) -> Result<SearchResults, RequestError> {
        let args = Args::new()
            .set("search_query", query.query.as_str())
            .set("limit", query.limit)
            .set("library_only", query.library_only)
            .opt(
                "media_types",
                (!query.media_types.is_empty()).then(|| text(&query.media_types)),
            )
            .opt(
                "providers",
                (!query.providers.is_empty()).then(|| text(&query.providers)),
            );
        self.call("music/search", args.into()).await
    }

    pub async fn item_by_uri(&self, uri: &str) -> Result<MediaItem, RequestError> {
        self.call("music/item_by_uri", Args::new().set("uri", uri).into())
            .await
    }

    pub async fn album_tracks(
        &self,
        item_id: &str,
        provider: &str,
        in_library_only: bool,
    ) -> Result<Vec<MediaItem>, RequestError> {
        let args = Args::new()
            .set("item_id", item_id)
            .set("provider_instance_id_or_domain", provider)
            .set("in_library_only", in_library_only);
        self.listing("music/albums/album_tracks", args.into()).await
    }

    pub async fn artist_tracks(
        &self,
        item_id: &str,
        provider: &str,
    ) -> Result<Vec<MediaItem>, RequestError> {
        let args = Args::new()
            .set("item_id", item_id)
            .set("provider_instance_id_or_domain", provider);
        self.listing("music/artists/artist_tracks", args.into())
            .await
    }

    pub async fn artist_albums(
        &self,
        item_id: &str,
        provider: &str,
    ) -> Result<Vec<MediaItem>, RequestError> {
        let args = Args::new()
            .set("item_id", item_id)
            .set("provider_instance_id_or_domain", provider);
        self.listing("music/artists/artist_albums", args.into())
            .await
    }

    /// A playlist's tracks; the server streams these in chunks of 500.
    pub async fn playlist_tracks(
        &self,
        item_id: &str,
        provider: &str,
    ) -> Result<Vec<MediaItem>, RequestError> {
        let args = Args::new()
            .set("item_id", item_id)
            .set("provider_instance_id_or_domain", provider);
        self.listing("music/playlists/playlist_tracks", args.into())
            .await
    }

    pub async fn library_items(
        &self,
        media_type: &MediaType,
        query: &LibraryQuery,
    ) -> Result<Vec<MediaItem>, RequestError> {
        if matches!(media_type, MediaType::Other(_) | MediaType::Unknown) {
            return Err(RequestError::InvalidArgument("not a library media type"));
        }
        let args = Args::new()
            .opt("favorite", query.favorite)
            .opt("search", query.search.as_deref())
            .opt("limit", query.limit)
            .opt("offset", query.offset)
            .opt("order_by", query.order_by.as_deref())
            .opt("provider", query.provider.as_deref());
        let command = format!("music/{}/library_items", media_type.plural());
        self.listing(&command, args.into()).await
    }

    pub async fn players(&self) -> Result<Vec<Player>, RequestError> {
        let args = Args::new().set("return_unavailable", true);
        self.list("players/all", args.into()).await
    }

    pub async fn player(&self, player_id: &str) -> Result<Option<Player>, RequestError> {
        self.call(
            "players/get",
            Args::new().set("player_id", player_id).into(),
        )
        .await
    }

    pub async fn player_command(
        &self,
        player_id: &str,
        command: PlayerCommand,
    ) -> Result<(), RequestError> {
        let args = Args::new().set("player_id", player_id);
        let (name, args) = match command {
            PlayerCommand::Play => ("players/cmd/play", args),
            PlayerCommand::Pause => ("players/cmd/pause", args),
            PlayerCommand::PlayPause => ("players/cmd/play_pause", args),
            PlayerCommand::Stop => ("players/cmd/stop", args),
            PlayerCommand::Next => ("players/cmd/next", args),
            PlayerCommand::Previous => ("players/cmd/previous", args),
            PlayerCommand::Seek(position) => ("players/cmd/seek", args.set("position", position)),
            PlayerCommand::VolumeSet(level) => {
                if level > 100 {
                    return Err(RequestError::InvalidArgument("volume is 0 to 100"));
                }
                ("players/cmd/volume_set", args.set("volume_level", level))
            }
            PlayerCommand::VolumeMute(muted) => {
                ("players/cmd/volume_mute", args.set("muted", muted))
            }
            PlayerCommand::Power(powered) => ("players/cmd/power", args.set("powered", powered)),
            PlayerCommand::Group { target } => {
                ("players/cmd/group", args.set("target_player", target))
            }
            PlayerCommand::Ungroup => ("players/cmd/ungroup", args),
        };
        self.unit(name, args.into()).await
    }

    /// Adds and removes members of `target`'s group in one command.
    pub async fn player_set_members(
        &self,
        target: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<(), RequestError> {
        let args = Args::new()
            .set("target_player", target)
            .opt("player_ids_to_add", (!add.is_empty()).then(|| text(add)))
            .opt(
                "player_ids_to_remove",
                (!remove.is_empty()).then(|| text(remove)),
            );
        self.unit("players/cmd/set_members", args.into()).await
    }

    pub async fn queues(&self) -> Result<Vec<PlayerQueue>, RequestError> {
        self.list("player_queues/all", None).await
    }

    pub async fn queue(&self, queue_id: &str) -> Result<Option<PlayerQueue>, RequestError> {
        self.call(
            "player_queues/get",
            Args::new().set("queue_id", queue_id).into(),
        )
        .await
    }

    pub async fn queue_items(
        &self,
        queue_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<QueueItem>, RequestError> {
        let args = Args::new()
            .set("queue_id", queue_id)
            .set("limit", limit)
            .set("offset", offset);
        self.listing("player_queues/items", args.into()).await
    }

    pub async fn play_media(&self, queue_id: &str, play: &PlayMedia) -> Result<(), RequestError> {
        if play.media.is_empty() {
            return Err(RequestError::InvalidArgument("nothing to play"));
        }
        let args = Args::new()
            .set("queue_id", queue_id)
            .set("media", text(&play.media))
            .set("radio_mode", play.radio_mode)
            .opt("option", play.option.as_ref().map(text))
            .opt("start_item", play.start_item.as_deref());
        self.unit("player_queues/play_media", args.into()).await
    }

    pub async fn queue_shuffle(&self, queue_id: &str, enabled: bool) -> Result<(), RequestError> {
        let args = Args::new()
            .set("queue_id", queue_id)
            .set("shuffle_enabled", enabled);
        self.unit("player_queues/shuffle", args.into()).await
    }

    pub async fn queue_repeat(
        &self,
        queue_id: &str,
        mode: &RepeatMode,
    ) -> Result<(), RequestError> {
        let args = Args::new()
            .set("queue_id", queue_id)
            .set("repeat_mode", text(mode));
        self.unit("player_queues/repeat", args.into()).await
    }

    /// Moves what `source` plays to `target`.
    pub async fn queue_transfer(
        &self,
        source: &str,
        target: &str,
        auto_play: Option<bool>,
    ) -> Result<(), RequestError> {
        let args = Args::new()
            .set("source_queue_id", source)
            .set("target_queue_id", target)
            .opt("auto_play", auto_play);
        self.unit("player_queues/transfer", args.into()).await
    }

    /// Stored provider configs, optionally of one type or domain. Needs an admin token.
    pub async fn provider_configs(
        &self,
        provider_type: Option<ProviderType>,
        domain: Option<&str>,
    ) -> Result<Vec<ProviderConfig>, RequestError> {
        let args = Args::new()
            .opt("provider_type", provider_type.as_ref().map(text))
            .opt("provider_domain", domain);
        self.list("config/providers", args.into()).await
    }

    /// Starts the setup flow that adds a provider instance (`config/providers/setup`).
    pub async fn start_provider_setup(&self, domain: &str) -> Result<SetupFlowStep, RequestError> {
        let args = Args::new().set("provider_domain", domain);
        self.call_with_timeout("config/providers/setup", args.into(), SETUP_TIMEOUT)
            .await
    }

    /// Submits a setup flow form (`config/flows/submit`).
    pub async fn submit_setup_flow(
        &self,
        flow_id: &str,
        values: Map<String, Value>,
    ) -> Result<SetupFlowStep, RequestError> {
        let args = Args::new()
            .set("flow_id", flow_id)
            .set("values", Value::Object(values));
        self.call_with_timeout("config/flows/submit", args.into(), SETUP_TIMEOUT)
            .await
    }

    pub async fn abort_setup_flow(&self, flow_id: &str) -> Result<(), RequestError> {
        self.unit(
            "config/flows/abort",
            Args::new().set("flow_id", flow_id).into(),
        )
        .await
    }

    /// Updates an existing instance's values, `name` and `enabled` included
    /// (`config/providers/save`). New instances go through the setup flow.
    pub async fn save_provider_config(
        &self,
        domain: &str,
        instance_id: &str,
        values: Map<String, Value>,
    ) -> Result<ProviderConfig, RequestError> {
        let args = Args::new()
            .set("provider_domain", domain)
            .set("instance_id", instance_id)
            .set("values", Value::Object(values));
        self.call_with_timeout("config/providers/save", args.into(), SETUP_TIMEOUT)
            .await
    }

    pub async fn remove_provider_config(&self, instance_id: &str) -> Result<(), RequestError> {
        let args = Args::new().set("instance_id", instance_id);
        self.unit("config/providers/remove", args.into()).await
    }

    /// Adds an `OpenSubsonic` provider instance through its setup flow and returns its instance
    /// id. An abandoned flow is aborted.
    pub async fn register_opensubsonic(
        &self,
        source: &OpenSubsonicSource,
    ) -> Result<String, SetupError> {
        let step = self.start_provider_setup(OPENSUBSONIC_DOMAIN).await?;
        let instance_id = match self.finish_opensubsonic_flow(step, source).await {
            Ok(id) => id,
            Err((flow_id, error)) => {
                if let Some(flow_id) = flow_id
                    && let Err(abort) = self.abort_setup_flow(&flow_id).await
                {
                    tracing::debug!(error = %abort, "could not abort the setup flow");
                }
                return Err(error);
            }
        };
        if let Some(name) = &source.name {
            let mut values = Map::new();
            values.insert("name".to_owned(), Value::String(name.clone()));
            self.save_provider_config(OPENSUBSONIC_DOMAIN, &instance_id, values)
                .await?;
        }
        Ok(instance_id)
    }

    async fn finish_opensubsonic_flow(
        &self,
        step: SetupFlowStep,
        source: &OpenSubsonicSource,
    ) -> Result<String, (Option<String>, SetupError)> {
        let flow_id = (!step.flow_id.is_empty()).then(|| step.flow_id.clone());
        let fail = |error: SetupError| (flow_id.clone(), error);
        let step = match step.step_type {
            FlowStepType::Form => {
                let mut values = Map::new();
                values.insert("baseURL".into(), json!(source.base_url));
                values.insert("api_key".into(), json!(source.api_key.expose()));
                values.insert("enable_legacy_auth".into(), json!(false));
                if let Some(port) = source.port {
                    values.insert("port".into(), json!(port));
                }
                if let Some(path) = &source.path {
                    values.insert("path".into(), json!(path));
                }
                self.submit_setup_flow(&step.flow_id, values)
                    .await
                    .map_err(|e| fail(e.into()))?
            }
            _ => step,
        };
        match step.step_type {
            FlowStepType::Finish => step
                .result
                .get("instance_id")
                .cloned()
                .ok_or_else(|| (None, SetupError::Unexpected("no instance id".into()))),
            FlowStepType::Abort => Err((
                None,
                SetupError::Aborted(step.reason.unwrap_or_else(|| "no reason".into())),
            )),
            FlowStepType::Form if !step.errors.is_empty() => {
                let errors = step
                    .errors
                    .iter()
                    .map(|(field, error)| format!("{field}: {error}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(fail(SetupError::Rejected(errors)))
            }
            other => Err(fail(SetupError::Unexpected(other.to_string()))),
        }
    }

    /// A URL for an image through Music Assistant's image proxy (resized if asked), or the
    /// image's own URL if it is public and the server sent no proxy id.
    pub fn image_url(
        &self,
        image: &Image,
        size: ImageSize,
        format: Option<ImageFormat>,
    ) -> Option<String> {
        image_url(self.endpoint(), image, size, format)
    }

    async fn list<T: serde::de::DeserializeOwned>(
        &self,
        command: &str,
        args: Option<Value>,
    ) -> Result<Vec<T>, RequestError> {
        let requester = self.requester();
        requester
            .list(command, args, requester.default_timeout())
            .await
    }

    async fn listing<T: serde::de::DeserializeOwned>(
        &self,
        command: &str,
        args: Option<Value>,
    ) -> Result<Vec<T>, RequestError> {
        let requester = self.requester();
        let timeout = requester.default_timeout().max(LISTING_TIMEOUT);
        requester.list(command, args, timeout).await
    }

    async fn unit(&self, command: &str, args: Option<Value>) -> Result<(), RequestError> {
        let requester = self.requester();
        requester
            .value(command, args, requester.default_timeout())
            .await
            .map(drop)
    }
}
