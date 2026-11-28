//! The streaming catalog over the same Music Assistant connection as speakers (contract
//! section 2): the services connected in Music Assistant, search across them, item detail,
//! each service's saved library, their recommendations ([`recommend`]) and the images Music
//! Assistant names for them.
//!
//! A search asks every service on its own, at most [`MAX_CALLS`] Music Assistant requests at
//! once across all callers, and waits until [`SEARCH_WAIT`] for the slowest; a service that
//! has not answered by then is listed as `timed_out` and the rest are returned. Everything
//! runs inside the caller's request, so a client that goes away cancels its searches.
//! Results are kept in memory for a short while per user and query, bounded by count and by
//! the items they hold. Nothing that comes from a service is written to disk here; images
//! go to the artwork cache, which keeps them re-encoded.

pub(crate) mod art;
mod cache;
pub(crate) mod convert;
mod recommend;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::future::join_all;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use trackside_core::assistant::SpeakersState;
use trackside_core::catalog::{
    ARTIST_ALBUMS, ARTIST_TOP_TRACKS, AccountState, CatalogDetail, CatalogEntry, CatalogError,
    CatalogKind, CatalogKinds, CatalogPage, CatalogResults, CatalogSearch, CatalogService,
    CatalogTrack, LibraryPage, PAGE_LIMIT_MAX, Recommendations, SEARCH_LIMIT_MAX, SEARCH_WAIT,
    ServiceCapability, ServiceList, ServiceOutcome, ServiceSearchState,
};
use trackside_core::catalog_ref::CatalogRef;
use trackside_core::{Access, CatalogArtId, ServiceId, UserKey};
use trackside_mass::models::{MediaItem, ProviderConfig, ProviderInstance, ProviderType};
use trackside_mass::{Client, ErrorCode, LibraryQuery, RequestError, SearchQuery};

use self::cache::{ArtBook, TtlCache};
use self::convert::{Converter, Source};
use crate::{Inner, lock};

/// Music Assistant requests the catalog sends at once, across every caller.
pub(crate) const MAX_CALLS: usize = 8;
/// Searches kept, and the items they may hold in all.
const SEARCHES_KEPT: usize = 256;
const SEARCH_ITEMS_KEPT: usize = 20_000;
/// How long a search is kept: a complete one, and one with a service missing.
const SEARCH_TTL: Duration = Duration::from_mins(1);
const PARTIAL_TTL: Duration = Duration::from_secs(10);
/// Playlist track lists kept for paging, and the tracks they may hold in all.
const LISTS_KEPT: usize = 32;
const LIST_ITEMS_KEPT: usize = 50_000;
const LIST_TTL: Duration = Duration::from_mins(5);
/// Image addresses remembered per generation (see [`ArtBook`]).
const ART_REMEMBERED: usize = 20_000;
/// How long the services' sign-in states are kept before Music Assistant is asked again.
const CONFIGS_TTL: Duration = Duration::from_secs(30);
/// How long asking for them may take.
const CONFIGS_WAIT: Duration = Duration::from_secs(2);

/// Provider domains that are never streaming services, whatever they declare.
const NOT_SERVICES: &[&str] = &[
    "library",
    "builtin",
    "opensubsonic",
    "subsonic",
    "jellyfin",
    "plex",
    "filesystem_local",
    "filesystem_smb",
    "filesystem_nfs",
];

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SearchKey {
    user: UserKey,
    text: String,
    kinds: CatalogKinds,
    services: Vec<ServiceId>,
    limit: u32,
}

/// The catalog's own state inside the integration.
pub(crate) struct State {
    searches: Mutex<TtlCache<SearchKey, Arc<CatalogResults>>>,
    lists: Mutex<TtlCache<String, Arc<Vec<MediaItem>>>>,
    recommendations: Mutex<TtlCache<recommend::Key, Arc<Recommendations>>>,
    /// Counts [`State::services_changed`], so an answer read before a change is not kept.
    services_generation: AtomicU64,
    art: Mutex<ArtBook>,
    configs: tokio::sync::Mutex<Option<(Instant, HashMap<String, ProviderConfig>)>>,
    calls: Semaphore,
    fetches: Arc<Semaphore>,
    http: Mutex<Option<(String, reqwest::Client)>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            searches: Mutex::new(TtlCache::new(SEARCHES_KEPT, SEARCH_ITEMS_KEPT)),
            lists: Mutex::new(TtlCache::new(LISTS_KEPT, LIST_ITEMS_KEPT)),
            recommendations: Mutex::new(TtlCache::new(recommend::KEPT, recommend::ITEMS_KEPT)),
            services_generation: AtomicU64::new(0),
            art: Mutex::new(ArtBook::new(ART_REMEMBERED)),
            configs: tokio::sync::Mutex::new(None),
            calls: Semaphore::new(MAX_CALLS),
            fetches: Arc::new(Semaphore::new(art::MAX_FETCHES)),
            http: Mutex::new(None),
        }
    }
}

impl State {
    /// Forgets what was read through a connection that is gone.
    pub(crate) fn reset(&self) {
        lock(&self.searches).clear();
        lock(&self.lists).clear();
        self.services_changed();
    }

    /// Forgets the recommendations, which depend on the services listed and their sign-in
    /// states: called whenever `catalog_services` is announced.
    pub(crate) fn services_changed(&self) {
        let mut kept = lock(&self.recommendations);
        self.services_generation.fetch_add(1, Ordering::SeqCst);
        kept.clear();
    }
}

/// A service as the catalog works with it.
#[derive(Clone, Debug)]
pub(crate) struct Service {
    pub instance_id: String,
    pub domain: String,
    pub public: CatalogService,
}

/// Whether a provider instance is a streaming service the catalog lists: a streaming music
/// provider that is not Trackside's own registration, not a file or server source, and not
/// hidden by the admin.
pub(crate) fn is_service(p: &ProviderInstance, ours: Option<&str>, hidden: &[String]) -> bool {
    p.provider_type == ProviderType::Music
        && p.is_streaming_provider == Some(true)
        && ours != Some(p.instance_id.as_str())
        && !NOT_SERVICES.contains(&p.domain.as_str())
        && !p.domain.starts_with("filesystem")
        && !hidden.contains(&p.instance_id)
}

/// The instance a ref plays and reads from: the one it names when that is still a listed,
/// available service, else any available one of its domain (contract 0.2).
pub(crate) fn resolve<'p>(
    decoded: &CatalogRef,
    providers: &'p [ProviderInstance],
    ours: Option<&str>,
    hidden: &[String],
) -> Option<&'p ProviderInstance> {
    let usable = |p: &&ProviderInstance| p.available && is_service(p, ours, hidden);
    decoded
        .instance_id
        .as_deref()
        .and_then(|i| providers.iter().filter(usable).find(|p| p.instance_id == i))
        .or_else(|| {
            providers
                .iter()
                .filter(usable)
                .find(|p| p.domain == decoded.domain)
        })
}

fn capabilities(p: &ProviderInstance) -> Vec<ServiceCapability> {
    let has = |name: &str| p.supported_features.iter().any(|f| f == name);
    let mut out = Vec::new();
    if [
        "library_tracks",
        "library_albums",
        "library_artists",
        "library_playlists",
    ]
    .iter()
    .any(|f| has(f))
    {
        out.push(ServiceCapability::Library);
    }
    if has("library_playlists") || has("playlist_tracks_edit") {
        out.push(ServiceCapability::Playlists);
    }
    if has("library_radios") || has("radio") {
        out.push(ServiceCapability::Radio);
    }
    if has("recommendations") {
        out.push(ServiceCapability::Recommendations);
    }
    if has("search") {
        out.push(ServiceCapability::Search);
    }
    if has("similar_tracks") || has("similar_artists") {
        out.push(ServiceCapability::Similar);
    }
    if has("artist_toptracks") {
        out.push(ServiceCapability::TopTracks);
    }
    out
}

fn account(
    p: &ProviderInstance,
    config: Option<&ProviderConfig>,
) -> (AccountState, Option<String>) {
    let detail = config
        .and_then(|c| c.last_error.as_ref())
        .map(|e| e.message.trim().to_owned())
        .filter(|m| !m.is_empty() && m.len() <= 500);
    let status = config.and_then(|c| c.status.as_deref());
    match status {
        Some("auth_required") => (AccountState::SignedOut, detail),
        Some("error") => (AccountState::Error, detail),
        _ if p.available => (AccountState::Ok, None),
        _ => (AccountState::Unavailable, detail),
    }
}

/// The opaque cursor of a catalog page: `{"o": offset}`.
pub(crate) fn encode_cursor(offset: usize) -> String {
    URL_SAFE_NO_PAD.encode(format!("{{\"o\":{offset}}}"))
}

pub(crate) fn decode_cursor(cursor: Option<&str>) -> Result<usize, CatalogError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let bad = || CatalogError::Invalid {
        field: "cursor",
        message: String::from("The cursor is not valid; start again."),
    };
    let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| bad())?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    value["o"]
        .as_u64()
        .and_then(|o| usize::try_from(o).ok())
        .ok_or_else(bad)
}

fn request_error(error: &RequestError) -> CatalogError {
    match error.code() {
        Some(ErrorCode::MediaNotFound | ErrorCode::InvalidProviderUri) => CatalogError::NotFound,
        Some(ErrorCode::ProviderPermissionDenied | ErrorCode::LoginFailed) => {
            CatalogError::SignedOut { service: None }
        }
        _ if error.is_transient() => CatalogError::Unavailable,
        _ => CatalogError::Failed(String::from("The music service could not answer.")),
    }
}

/// Interleaves the services' lists, best of each first, without repeating a ref.
fn interleave<T: Clone>(lists: &[Vec<T>], reference: impl Fn(&T) -> &str) -> Vec<T> {
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for rank in 0..longest {
        for list in lists {
            if let Some(item) = list.get(rank)
                && seen.insert(reference(item).to_owned())
            {
                out.push(item.clone());
            }
        }
    }
    out
}

/// The services a search asks, and those it reports without asking: signed out, or
/// unable to search. Without a list, every signed-in service that searches.
fn pick(
    services: &[Service],
    wanted: Option<&[ServiceId]>,
) -> (Vec<Service>, Vec<(Service, ServiceSearchState)>) {
    let mut asked = Vec::new();
    let mut outcomes = Vec::new();
    let searches = |s: &Service| s.public.has(ServiceCapability::Search);
    match wanted {
        Some(ids) => {
            for id in ids {
                let Some(service) = services.iter().find(|s| &s.public.id == id) else {
                    continue;
                };
                if service.public.account != AccountState::Ok {
                    outcomes.push((service.clone(), ServiceSearchState::SignedOut));
                } else if searches(service) {
                    asked.push(service.clone());
                } else {
                    outcomes.push((service.clone(), ServiceSearchState::Failed));
                }
            }
        }
        None => asked.extend(
            services
                .iter()
                .filter(|s| s.public.account == AccountState::Ok && searches(s))
                .cloned(),
        ),
    }
    (asked, outcomes)
}

/// The services' answers as one result: each section interleaved in the order the services
/// were asked, the outcomes in that order too, complete unless one timed out.
fn merge(
    kinds: CatalogKinds,
    order: &[ServiceId],
    found: &[Found],
    outcomes: &mut [(Service, ServiceSearchState)],
) -> CatalogResults {
    outcomes.sort_by_key(|(s, _)| order.iter().position(|id| *id == s.public.id));
    CatalogResults {
        kinds,
        tracks: interleave(
            &found.iter().map(|f| f.tracks.clone()).collect::<Vec<_>>(),
            |t| &t.reference,
        ),
        albums: interleave(
            &found.iter().map(|f| f.albums.clone()).collect::<Vec<_>>(),
            |a| &a.reference,
        ),
        artists: interleave(
            &found.iter().map(|f| f.artists.clone()).collect::<Vec<_>>(),
            |a| &a.reference,
        ),
        playlists: interleave(
            &found
                .iter()
                .map(|f| f.playlists.clone())
                .collect::<Vec<_>>(),
            |p| &p.reference,
        ),
        radio: interleave(
            &found.iter().map(|f| f.radio.clone()).collect::<Vec<_>>(),
            |r| &r.reference,
        ),
        services: outcomes
            .iter()
            .map(|(s, state)| ServiceOutcome {
                service: s.public.id.clone(),
                state: *state,
            })
            .collect(),
        complete: outcomes
            .iter()
            .all(|(_, state)| *state != ServiceSearchState::TimedOut),
    }
}

/// One service's answer to a search, converted.
struct Found {
    tracks: Vec<CatalogTrack>,
    albums: Vec<trackside_core::catalog::CatalogAlbum>,
    artists: Vec<trackside_core::catalog::CatalogArtist>,
    playlists: Vec<trackside_core::catalog::CatalogPlaylist>,
    radio: Vec<trackside_core::catalog::CatalogRadio>,
}

impl Inner {
    fn catalog_hidden(&self) -> Vec<String> {
        self.effective().hidden_services
    }

    /// The listed services, with sign-in states from Music Assistant's configs when it
    /// gives them (an admin token can read them).
    async fn catalog_services(&self, client: Option<&Client>) -> Vec<Service> {
        let configs = match client {
            Some(client) => self.provider_configs(client).await,
            None => HashMap::new(),
        };
        let hidden = self.catalog_hidden();
        let hub = lock(&self.hub);
        let server_id = hub.server_id.clone().unwrap_or_default();
        hub.providers
            .iter()
            .filter(|p| is_service(p, hub.ours.as_deref(), &hidden))
            .map(|p| {
                let (state, detail) = account(p, configs.get(&p.instance_id));
                Service {
                    instance_id: p.instance_id.clone(),
                    domain: p.domain.clone(),
                    public: CatalogService {
                        id: ServiceId::derive(&server_id, &p.instance_id),
                        name: if p.name.is_empty() {
                            p.domain.clone()
                        } else {
                            p.name.clone()
                        },
                        logo_key: p.domain.clone(),
                        capabilities: capabilities(p),
                        account: state,
                        account_detail: detail,
                        quality: convert::service_quality(&p.domain),
                    },
                }
            })
            .collect()
    }

    async fn provider_configs(&self, client: &Client) -> HashMap<String, ProviderConfig> {
        let mut kept = self.catalog.configs.lock().await;
        if let Some((at, configs)) = kept.as_ref()
            && at.elapsed() < CONFIGS_TTL
        {
            return configs.clone();
        }
        let fresh = tokio::time::timeout(
            CONFIGS_WAIT,
            client.provider_configs(Some(ProviderType::Music), None),
        )
        .await;
        let configs: HashMap<String, ProviderConfig> = match fresh {
            Ok(Ok(list)) => list
                .into_iter()
                .map(|c| (c.instance_id.clone(), c))
                .collect(),
            _ => HashMap::new(),
        };
        *kept = Some((Instant::now(), configs.clone()));
        configs
    }

    pub(crate) async fn service_list(&self) -> ServiceList {
        let list = lock(&self.conn).list;
        if list.state == SpeakersState::Unavailable {
            return ServiceList {
                services: Vec::new(),
                state: list.state,
                since: list.since,
            };
        }
        let client = self.connected().ok();
        ServiceList {
            services: self
                .catalog_services(client.as_ref())
                .await
                .into_iter()
                .map(|s| s.public)
                .collect(),
            state: list.state,
            since: list.since,
        }
    }

    fn endpoint(&self) -> Option<trackside_mass::Endpoint> {
        lock(&self.hub).endpoint.clone()
    }

    fn remember_art(&self, seen: crate::convert::ArtSeen) {
        let mut book = lock(&self.catalog.art);
        for (id, source) in seen {
            book.remember(id, source);
        }
    }

    /// Converts off the async runtime: song keys fold Unicode, and a search can hold a few
    /// thousand items.
    async fn convert<T: Send + 'static>(
        &self,
        service: &Service,
        work: impl FnOnce(&mut Converter<'_>) -> T + Send + 'static,
    ) -> Result<T, CatalogError> {
        let endpoint = self.endpoint();
        let instance = service.instance_id.clone();
        let domain = service.domain.clone();
        let id = service.public.id.clone();
        let (out, art) = tokio::task::spawn_blocking(move || {
            let mut converter = Converter::new(Source {
                instance_id: &instance,
                domain: &domain,
                service: &id,
                endpoint: endpoint.as_ref(),
            });
            let out = work(&mut converter);
            (out, converter.art)
        })
        .await
        .map_err(|e| CatalogError::Failed(format!("conversion: {e}")))?;
        self.remember_art(art);
        Ok(out)
    }

    pub(crate) async fn catalog_search(
        &self,
        caller: &Access,
        query: CatalogSearch,
    ) -> Result<CatalogResults, CatalogError> {
        let client = self.connected().map_err(|_| CatalogError::Unavailable)?;
        let deadline = Instant::now() + SEARCH_WAIT;
        let text = query.text.trim().to_owned();
        let limit = query.limit.clamp(1, SEARCH_LIMIT_MAX);
        let kinds = query.kinds;
        let services = self.catalog_services(Some(&client)).await;
        let (asked, mut outcomes) = pick(&services, query.services.as_deref());
        if text.is_empty() || kinds.is_empty() {
            return Ok(CatalogResults::empty(kinds));
        }
        let key = SearchKey {
            user: caller.user,
            text: tracks_model::search_folded(&text),
            kinds,
            services: asked
                .iter()
                .chain(outcomes.iter().map(|(s, _)| s))
                .map(|s| s.public.id.clone())
                .collect(),
            limit,
        };
        if let Some(kept) = lock(&self.catalog.searches).get(&key, Instant::now()) {
            return Ok((*kept).clone());
        }
        let media_types: Vec<_> = kinds.kinds().into_iter().map(convert::media_type).collect();
        let searches = asked.iter().map(|service| {
            let client = client.clone();
            let mut search = SearchQuery::new(text.clone())
                .media_types(media_types.clone())
                .limit(limit);
            search.providers = vec![service.instance_id.clone(), String::from("library")];
            async move {
                let answer = tokio::time::timeout_at(deadline, async {
                    let _permit = self.catalog.calls.acquire().await.ok()?;
                    Some(client.search(&search).await)
                })
                .await;
                match answer {
                    Err(_) | Ok(Some(Err(RequestError::Timeout(_)))) => {
                        Err(ServiceSearchState::TimedOut)
                    }
                    Ok(None) => Err(ServiceSearchState::Failed),
                    Ok(Some(Err(e))) => {
                        tracing::debug!(service = %service.public.id, error = %e, "a catalog search failed");
                        Err(match request_error(&e) {
                            CatalogError::SignedOut { .. } => ServiceSearchState::SignedOut,
                            _ => ServiceSearchState::Failed,
                        })
                    }
                    Ok(Some(Ok(found))) => Ok(found),
                }
            }
        });
        let answers = join_all(searches).await;
        let mut found = Vec::new();
        for (service, answer) in asked.iter().zip(answers) {
            match answer {
                Ok(results) => {
                    let converted = self
                        .convert(service, move |c| Found {
                            tracks: results.tracks.iter().filter_map(|i| c.track(i)).collect(),
                            albums: results.albums.iter().filter_map(|i| c.album(i)).collect(),
                            artists: results.artists.iter().filter_map(|i| c.artist(i)).collect(),
                            playlists: results
                                .playlists
                                .iter()
                                .filter_map(|i| c.playlist(i))
                                .collect(),
                            radio: results.radio.iter().filter_map(|i| c.radio(i)).collect(),
                        })
                        .await?;
                    found.push(converted);
                    outcomes.push((service.clone(), ServiceSearchState::Ok));
                }
                Err(state) => outcomes.push((service.clone(), state)),
            }
        }
        let results = merge(kinds, &key.services, &found, &mut outcomes);
        let complete = results.complete;
        let weight = results.tracks.len()
            + results.albums.len()
            + results.artists.len()
            + results.playlists.len()
            + results.radio.len()
            + 1;
        let ttl = if complete { SEARCH_TTL } else { PARTIAL_TTL };
        lock(&self.catalog.searches).put(
            key,
            Arc::new(results.clone()),
            weight,
            ttl,
            Instant::now(),
        );
        Ok(results)
    }

    /// The service a ref names, or why it cannot be read.
    async fn service_of(
        &self,
        client: &Client,
        reference: &str,
    ) -> Result<(CatalogRef, Service), CatalogError> {
        let decoded = CatalogRef::decode(reference).map_err(|e| CatalogError::Invalid {
            field: "ref",
            message: format!("ref: {e}."),
        })?;
        let hidden = self.catalog_hidden();
        let (instance, server_id) = {
            let hub = lock(&self.hub);
            (
                resolve(&decoded, &hub.providers, hub.ours.as_deref(), &hidden)
                    .map(|p| p.instance_id.clone()),
                hub.server_id.clone().unwrap_or_default(),
            )
        };
        let Some(instance) = instance else {
            return Err(CatalogError::SignedOut {
                service: decoded
                    .instance_id
                    .as_deref()
                    .map(|i| ServiceId::derive(&server_id, i)),
            });
        };
        let services = self.catalog_services(Some(client)).await;
        let service = services
            .into_iter()
            .find(|s| s.instance_id == instance)
            .ok_or(CatalogError::SignedOut { service: None })?;
        if service.public.account == AccountState::SignedOut {
            return Err(CatalogError::SignedOut {
                service: Some(service.public.id),
            });
        }
        Ok((decoded, service))
    }

    async fn call<T>(
        &self,
        work: impl std::future::Future<Output = Result<T, RequestError>>,
    ) -> Result<T, CatalogError> {
        let _permit = self
            .catalog
            .calls
            .acquire()
            .await
            .map_err(|_| CatalogError::Unavailable)?;
        work.await.map_err(|e| request_error(&e))
    }

    /// A playlist's tracks, kept a few minutes so paging through it asks Music Assistant
    /// once.
    async fn playlist_tracks(
        &self,
        client: &Client,
        uri: &str,
        item_id: &str,
        instance: &str,
    ) -> Result<Arc<Vec<MediaItem>>, CatalogError> {
        if let Some(kept) = lock(&self.catalog.lists).get(&uri.to_owned(), Instant::now()) {
            return Ok(kept);
        }
        let tracks = Arc::new(self.call(client.playlist_tracks(item_id, instance)).await?);
        lock(&self.catalog.lists).put(
            uri.to_owned(),
            Arc::clone(&tracks),
            tracks.len() + 1,
            LIST_TTL,
            Instant::now(),
        );
        Ok(tracks)
    }

    pub(crate) async fn catalog_item(
        &self,
        reference: &str,
        page: CatalogPage,
    ) -> Result<CatalogDetail, CatalogError> {
        let client = self.connected().map_err(|_| CatalogError::Unavailable)?;
        let (decoded, service) = self.service_of(&client, reference).await?;
        let instance = service.instance_id.clone();
        let uri = decoded.uri_on(&instance);
        let kind = convert::catalog_kind(decoded.kind);
        let id = decoded.item_id.clone();
        let item = self.call(client.item_by_uri(&uri)).await?;
        if item.name.is_empty() {
            return Err(CatalogError::NotFound);
        }
        let not_found = || CatalogError::NotFound;
        match kind {
            CatalogKind::Track => self
                .convert(&service, move |c| c.track(&item))
                .await?
                .map(CatalogDetail::Track)
                .ok_or_else(not_found),
            CatalogKind::Radio => self
                .convert(&service, move |c| c.radio(&item))
                .await?
                .map(CatalogDetail::Radio)
                .ok_or_else(not_found),
            CatalogKind::Album => {
                let mut tracks = self
                    .call(client.album_tracks(&id, &instance, false))
                    .await?;
                tracks.sort_by_key(|t| (t.disc_number.unwrap_or(0), t.track_number.unwrap_or(0)));
                let (album, tracks) = self
                    .convert(&service, move |c| {
                        let tracks: Vec<_> = tracks.iter().filter_map(|t| c.track(t)).collect();
                        let album = c.album(&item).map(|mut a| {
                            a.track_count = u32::try_from(tracks.len()).ok();
                            a
                        });
                        (album, tracks)
                    })
                    .await?;
                Ok(CatalogDetail::Album {
                    album: album.ok_or_else(not_found)?,
                    tracks,
                })
            }
            CatalogKind::Artist => {
                let (top, albums) = tokio::join!(
                    self.call(client.artist_tracks(&id, &instance)),
                    self.call(client.artist_albums(&id, &instance)),
                );
                let (top, albums) = (top?, albums?);
                let (artist, top_tracks, albums) = self
                    .convert(&service, move |c| {
                        (
                            c.artist(&item),
                            top.iter()
                                .filter_map(|t| c.track(t))
                                .take(ARTIST_TOP_TRACKS)
                                .collect(),
                            albums
                                .iter()
                                .filter_map(|a| c.album(a))
                                .take(ARTIST_ALBUMS)
                                .collect(),
                        )
                    })
                    .await?;
                Ok(CatalogDetail::Artist {
                    artist: artist.ok_or_else(not_found)?,
                    top_tracks,
                    albums,
                })
            }
            CatalogKind::Playlist => {
                let offset = decode_cursor(page.cursor.as_deref())?;
                let limit = page.limit.clamp(1, PAGE_LIMIT_MAX) as usize;
                let all = self.playlist_tracks(&client, &uri, &id, &instance).await?;
                let total = all.len();
                let window: Vec<MediaItem> = all.iter().skip(offset).take(limit).cloned().collect();
                let next_cursor = (offset + limit < total).then(|| encode_cursor(offset + limit));
                let (playlist, tracks) = self
                    .convert(&service, move |c| {
                        let playlist = c.playlist(&item).map(|mut p| {
                            p.track_count = u32::try_from(total).ok();
                            p
                        });
                        (playlist, window.iter().filter_map(|t| c.track(t)).collect())
                    })
                    .await?;
                Ok(CatalogDetail::Playlist {
                    playlist: playlist.ok_or_else(not_found)?,
                    tracks,
                    next_cursor,
                })
            }
        }
    }

    pub(crate) async fn catalog_library(
        &self,
        service_id: &ServiceId,
        kind: CatalogKind,
        page: CatalogPage,
    ) -> Result<LibraryPage, CatalogError> {
        let client = self.connected().map_err(|_| CatalogError::Unavailable)?;
        let service = self
            .catalog_services(Some(&client))
            .await
            .into_iter()
            .find(|s| &s.public.id == service_id)
            .ok_or(CatalogError::NotFound)?;
        if !service.public.has(ServiceCapability::Library) {
            return Err(CatalogError::Unsupported {
                capability: "library",
            });
        }
        if service.public.account == AccountState::SignedOut {
            return Err(CatalogError::SignedOut {
                service: Some(service.public.id),
            });
        }
        let offset = decode_cursor(page.cursor.as_deref())?;
        let limit = page.limit.clamp(1, PAGE_LIMIT_MAX);
        let mut query = LibraryQuery::page(
            limit + 1,
            u32::try_from(offset).map_err(|_| CatalogError::Invalid {
                field: "cursor",
                message: String::from("The cursor is not valid; start again."),
            })?,
        );
        query.order_by = Some(String::from("timestamp_added_desc"));
        query.provider = Some(service.instance_id.clone());
        let mut items = self
            .call(client.library_items(&convert::media_type(kind), &query))
            .await?;
        let more = items.len() > limit as usize;
        items.truncate(limit as usize);
        let items: Vec<CatalogEntry> = self
            .convert(&service, move |c| {
                items.iter().filter_map(|i| c.entry(i)).collect()
            })
            .await?;
        Ok(LibraryPage {
            items,
            next_cursor: more.then(|| encode_cursor(offset + limit as usize)),
            synced_at: None,
        })
    }

    pub(crate) fn catalog_art_source(&self, id: &CatalogArtId) -> Option<String> {
        lock(&self.hub)
            .art
            .get(id)
            .cloned()
            .or_else(|| lock(&self.catalog.art).get(id))
    }

    fn art_client(&self) -> Result<reqwest::Client, CatalogError> {
        let endpoint = self.endpoint();
        let home = endpoint.as_ref().map(art::Home::of);
        let effective = self.effective();
        let key = format!(
            "{:?}-{}",
            home,
            effective
                .ca
                .as_ref()
                .map_or(0, trackside_mass::CustomCa::len)
        );
        let mut kept = lock(&self.catalog.http);
        if let Some((k, client)) = kept.as_ref()
            && *k == key
        {
            return Ok(client.clone());
        }
        let client =
            art::client(home.as_ref(), effective.ca.as_ref()).map_err(CatalogError::Failed)?;
        *kept = Some((key, client.clone()));
        Ok(client)
    }

    pub(crate) async fn catalog_fetch_art(
        &self,
        id: &CatalogArtId,
    ) -> Result<Vec<u8>, CatalogError> {
        let source = self.catalog_art_source(id).ok_or(CatalogError::NotFound)?;
        let home = self.endpoint().as_ref().map(art::Home::of);
        let url = art::check(&source, home.as_ref()).map_err(|refusal| {
            tracing::debug!(%id, %refusal, "a catalog image address was refused");
            CatalogError::NotFound
        })?;
        let http = self.art_client()?;
        let _permit = tokio::time::timeout(
            art::FETCH_TIMEOUT,
            Arc::clone(&self.catalog.fetches).acquire_owned(),
        )
        .await
        .map_err(|_| CatalogError::Unavailable)?
        .map_err(|_| CatalogError::Unavailable)?;
        art::download(&http, url).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(value: serde_json::Value) -> ProviderInstance {
        serde_json::from_value(value).unwrap_or_else(|e| panic!("{e}"))
    }

    fn providers() -> Vec<ProviderInstance> {
        [
            json!({"type": "music", "domain": "spotify", "instance_id": "spotify--a", "available": true, "is_streaming_provider": true}),
            json!({"type": "music", "domain": "spotify", "instance_id": "spotify--b", "available": true, "is_streaming_provider": true}),
            json!({"type": "music", "domain": "tidal", "instance_id": "tidal--off", "available": false, "is_streaming_provider": true}),
            json!({"type": "music", "domain": "opensubsonic", "instance_id": "opensubsonic--ours", "available": true, "is_streaming_provider": true}),
            json!({"type": "music", "domain": "filesystem_local", "instance_id": "filesystem_local--x", "available": true, "is_streaming_provider": false}),
            json!({"type": "player", "domain": "sonos", "instance_id": "sonos--x", "available": true}),
        ]
        .into_iter()
        .map(provider)
        .collect()
    }

    #[test]
    fn only_streaming_services_are_listed() {
        let all = providers();
        let listed: Vec<&str> = all
            .iter()
            .filter(|p| is_service(p, Some("opensubsonic--ours"), &[String::from("spotify--b")]))
            .map(|p| p.instance_id.as_str())
            .collect();
        assert_eq!(listed, ["spotify--a", "tidal--off"]);
        let not_ours = provider(json!({"type": "music", "domain": "opensubsonic",
            "instance_id": "opensubsonic--other", "is_streaming_provider": true}));
        assert!(
            !is_service(&not_ours, None, &[]),
            "server sources never count"
        );
    }

    #[test]
    fn refs_resolve_to_their_instance_else_their_domain() {
        let all = providers();
        let named = CatalogRef::new(
            trackside_core::catalog_ref::RefKind::Album,
            "spotify",
            "x",
            Some("spotify--b"),
        );
        let found = |r: &CatalogRef, hidden: &[String]| {
            resolve(r, &all, Some("opensubsonic--ours"), hidden).map(|p| p.instance_id.clone())
        };
        assert_eq!(found(&named, &[]).as_deref(), Some("spotify--b"));
        assert_eq!(
            found(&named, &[String::from("spotify--b")]).as_deref(),
            Some("spotify--a"),
            "a hidden instance falls back to another of its domain"
        );
        let gone = CatalogRef::new(
            trackside_core::catalog_ref::RefKind::Track,
            "spotify",
            "x",
            Some("spotify--gone"),
        );
        assert_eq!(found(&gone, &[]).as_deref(), Some("spotify--a"));
        let signed_out = CatalogRef::new(
            trackside_core::catalog_ref::RefKind::Track,
            "tidal",
            "x",
            None,
        );
        assert_eq!(
            found(&signed_out, &[]),
            None,
            "an unavailable instance does not play"
        );
        let ours = CatalogRef::new(
            trackside_core::catalog_ref::RefKind::Track,
            "opensubsonic",
            "x",
            Some("opensubsonic--ours"),
        );
        assert_eq!(found(&ours, &[]), None);
    }

    #[test]
    fn results_interleave_best_first_without_repeats() {
        let lists = vec![vec!["a1", "a2", "a3"], vec!["b1"], vec!["c1", "a2", "c3"]];
        assert_eq!(
            interleave(&lists, |s| s),
            ["a1", "b1", "c1", "a2", "a3", "c3"]
        );
        assert_eq!(interleave::<&str>(&[], |s| s), Vec::<&str>::new());
    }

    #[test]
    fn cursors_round_trip_and_match_the_contract() -> Result<(), CatalogError> {
        assert_eq!(encode_cursor(100), "eyJvIjoxMDB9");
        assert_eq!(decode_cursor(Some("eyJvIjoxMDB9"))?, 100);
        assert_eq!(decode_cursor(None)?, 0);
        assert!(matches!(
            decode_cursor(Some("nope!")),
            Err(CatalogError::Invalid {
                field: "cursor",
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn capabilities_come_from_the_provider_features() {
        let p = provider(
            json!({"type": "music", "domain": "spotify", "instance_id": "s",
            "supported_features": ["search", "library_tracks", "library_playlists", "artist_toptracks", "similar_tracks", "recommendations"]}),
        );
        assert_eq!(
            capabilities(&p),
            [
                ServiceCapability::Library,
                ServiceCapability::Playlists,
                ServiceCapability::Recommendations,
                ServiceCapability::Search,
                ServiceCapability::Similar,
                ServiceCapability::TopTracks
            ]
        );
    }
}
