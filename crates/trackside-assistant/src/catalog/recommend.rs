//! The services' recommendations (api.md 21.8): Music Assistant's recommendation rows of the
//! listed streaming services, each with its items, as catalog shelves.
//!
//! One `music/recommendations` call lists every provider's rows; the rows of Music
//! Assistant's own library (recently played, random, favourites and the like), of metadata
//! and plugin providers, of hidden services and of services not asked for are dropped. The
//! items of the rows left are read one row per request, in parallel under the catalog's
//! request cap, all within [`RECOMMENDATIONS_WAIT`]: a row that has not answered by then is
//! left out and its service reported `timed_out`. Answers are kept per user, long when
//! complete and briefly when not, and forgotten whenever the services change.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::future::join_all;
use tokio::time::Instant;
use trackside_core::catalog::{
    AccountState, CatalogEntry, CatalogError, MAX_SHELVES, RECOMMENDATIONS_LIMIT_MAX,
    RECOMMENDATIONS_WAIT, Recommendations, RecommendationsQuery, ServiceCapability, ServiceOutcome,
    ServiceSearchState, Shelf, ShelfKind,
};
use trackside_core::{Access, ServiceId, ShelfId, UserKey};
use trackside_mass::models::{RecommendationFolder, RecommendationFolderType, RecommendationItem};
use trackside_mass::{Client, RequestError};

use super::convert::Converter;
use super::{Service, request_error};
use crate::{Inner, lock};

/// Answers kept, and the items they may hold in all.
pub(super) const KEPT: usize = 128;
pub(super) const ITEMS_KEPT: usize = 20_000;
/// How long an answer is kept: a complete one, and one with a row missing.
const TTL: Duration = Duration::from_mins(20);
const PARTIAL_TTL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    user: UserKey,
    services: Vec<ServiceId>,
    limit: u32,
}

/// The services recommendations are read from, and those reported without asking: signed
/// out, or without recommendations. Without a list, every signed-in service that
/// recommends; unknown ids are ignored.
pub(super) fn pick(
    services: &[Service],
    wanted: Option<&[ServiceId]>,
) -> (Vec<Service>, Vec<(Service, ServiceSearchState)>) {
    let recommends = |s: &Service| s.public.has(ServiceCapability::Recommendations);
    let mut asked = Vec::new();
    let mut outcomes = Vec::new();
    match wanted {
        Some(ids) => {
            let mut seen = HashSet::new();
            for id in ids {
                let Some(service) = services.iter().find(|s| &s.public.id == id) else {
                    continue;
                };
                if !seen.insert(id) {
                    continue;
                }
                if service.public.account != AccountState::Ok {
                    outcomes.push((service.clone(), ServiceSearchState::SignedOut));
                } else if recommends(service) {
                    asked.push(service.clone());
                } else {
                    outcomes.push((service.clone(), ServiceSearchState::Failed));
                }
            }
        }
        None => asked.extend(
            services
                .iter()
                .filter(|s| s.public.account == AccountState::Ok && recommends(s))
                .cloned(),
        ),
    }
    (asked, outcomes)
}

/// The asked service that owns a row: the instance it names, else the first asked service of
/// the domain it names (Music Assistant serves a domain's row from a visible account of it).
/// Rows of anything else (Music Assistant's library, metadata and plugin providers, services
/// not asked or hidden) have none.
pub(super) fn owner<'s>(row: &RecommendationFolder, asked: &'s [Service]) -> Option<&'s Service> {
    asked
        .iter()
        .find(|s| s.instance_id == row.provider)
        .or_else(|| asked.iter().find(|s| s.domain == row.provider))
}

/// The rows to read, each with the index of its service in `asked`, in Music Assistant's
/// order: owned by an asked service, named, on by default (Music Assistant's own client
/// hides the others until asked), once each, at most [`MAX_SHELVES`].
pub(super) fn rows(
    all: Vec<RecommendationFolder>,
    asked: &[Service],
) -> Vec<(usize, RecommendationFolder)> {
    let mut seen = HashSet::new();
    all.into_iter()
        .filter(|row| row.enabled_by_default && !row.item_id.is_empty() && !row.name.is_empty())
        .filter_map(|row| {
            let service = owner(&row, asked)?;
            let index = asked
                .iter()
                .position(|s| s.instance_id == service.instance_id)?;
            seen.insert((index, row.item_id.clone()))
                .then_some((index, row))
        })
        .take(MAX_SHELVES)
        .collect()
}

fn kind(row: &RecommendationFolder) -> ShelfKind {
    match row.kind {
        RecommendationFolderType::Timeline => ShelfKind::Timeline,
        _ => ShelfKind::Default,
    }
}

/// A row and its items as a shelf of `service`: the items the catalog shows (tracks, albums,
/// artists, playlists and radio of this service; folders and other kinds are skipped), each
/// ref once, at most `limit`. `None` when nothing is left.
pub(super) fn shelf(
    converter: &mut Converter<'_>,
    server_id: &str,
    row: &RecommendationFolder,
    items: &[RecommendationItem],
    limit: usize,
) -> Option<Shelf> {
    let mut seen = HashSet::new();
    let items: Vec<CatalogEntry> = items
        .iter()
        .filter_map(|item| match item {
            RecommendationItem::Media(media) => converter.entry(media),
            _ => None,
        })
        .filter(|entry| seen.insert(entry.reference().to_owned()))
        .take(limit)
        .collect();
    if items.is_empty() {
        return None;
    }
    let source = &converter.source;
    Some(Shelf {
        id: ShelfId::derive(server_id, source.instance_id, &row.item_id),
        service: source.service.clone(),
        title: row.name.trim().to_owned(),
        subtitle: row
            .subtitle
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        kind: kind(row),
        items,
    })
}

/// How reading one row's items went.
pub(super) enum RowAnswer {
    Items(Vec<RecommendationItem>),
    TimedOut,
    Failed(ServiceSearchState),
}

/// A service's state from its rows' answers: `timed_out` if any row ran out of time, else
/// the failure when every row failed, else `ok` (a service without rows is `ok`: it has
/// nothing to recommend now).
pub(super) fn state_of<'a>(answers: impl Iterator<Item = &'a RowAnswer>) -> ServiceSearchState {
    let mut any = false;
    let mut ok = false;
    let mut failure = None;
    for answer in answers {
        any = true;
        match answer {
            RowAnswer::TimedOut => return ServiceSearchState::TimedOut,
            RowAnswer::Items(_) => ok = true,
            RowAnswer::Failed(state) => failure = failure.or(Some(*state)),
        }
    }
    match failure {
        Some(state) if any && !ok => state,
        _ => ServiceSearchState::Ok,
    }
}

fn failure_state(error: &RequestError) -> ServiceSearchState {
    match request_error(error) {
        CatalogError::SignedOut { .. } => ServiceSearchState::SignedOut,
        _ => ServiceSearchState::Failed,
    }
}

/// Outcomes in the order the services were asked for (or listed), complete unless one
/// timed out.
fn finish(
    order: &[ServiceId],
    shelves: Vec<Shelf>,
    mut outcomes: Vec<(ServiceId, ServiceSearchState)>,
) -> Recommendations {
    outcomes.sort_by_key(|(id, _)| order.iter().position(|o| o == id));
    let complete = outcomes
        .iter()
        .all(|(_, state)| *state != ServiceSearchState::TimedOut);
    Recommendations {
        shelves,
        services: outcomes
            .into_iter()
            .map(|(service, state)| ServiceOutcome { service, state })
            .collect(),
        complete,
    }
}

impl Inner {
    pub(crate) async fn catalog_recommendations(
        &self,
        caller: &Access,
        query: RecommendationsQuery,
    ) -> Result<Recommendations, CatalogError> {
        let client = self.connected().map_err(|_| CatalogError::Unavailable)?;
        let deadline = Instant::now() + RECOMMENDATIONS_WAIT;
        let generation = self.catalog.services_generation.load(Ordering::SeqCst);
        let limit = query.limit.clamp(1, RECOMMENDATIONS_LIMIT_MAX);
        let services = self.catalog_services(Some(&client)).await;
        let order: Vec<ServiceId> = match &query.services {
            Some(ids) => ids.clone(),
            None => services.iter().map(|s| s.public.id.clone()).collect(),
        };
        let (asked, told) = pick(&services, query.services.as_deref());
        let key = Key {
            user: caller.user,
            services: asked
                .iter()
                .chain(told.iter().map(|(s, _)| s))
                .map(|s| s.public.id.clone())
                .collect(),
            limit,
        };
        if let Some(kept) = lock(&self.catalog.recommendations).get(&key, Instant::now()) {
            return Ok((*kept).clone());
        }
        let mut outcomes: Vec<(ServiceId, ServiceSearchState)> = told
            .iter()
            .map(|(s, state)| (s.public.id.clone(), *state))
            .collect();
        if asked.is_empty() {
            return Ok(finish(&order, Vec::new(), outcomes));
        }
        let listed = tokio::time::timeout_at(deadline, async {
            let _permit = self.catalog.calls.acquire().await.ok()?;
            Some(client.recommendations().await)
        })
        .await;
        let all = match listed {
            Err(_) | Ok(Some(Err(RequestError::Timeout(_)))) => {
                outcomes.extend(
                    asked
                        .iter()
                        .map(|s| (s.public.id.clone(), ServiceSearchState::TimedOut)),
                );
                let answer = finish(&order, Vec::new(), outcomes);
                self.keep(key, &answer, generation);
                return Ok(answer);
            }
            Ok(None) => return Err(CatalogError::Unavailable),
            Ok(Some(Err(e))) => {
                if let CatalogError::Unavailable = request_error(&e) {
                    return Err(CatalogError::Unavailable);
                }
                tracing::debug!(error = %e, "Music Assistant's recommendations failed");
                let state = failure_state(&e);
                outcomes.extend(asked.iter().map(|s| (s.public.id.clone(), state)));
                return Ok(finish(&order, Vec::new(), outcomes));
            }
            Ok(Some(Ok(all))) => all,
        };
        let rows = rows(all, &asked);
        let answers = join_all(
            rows.iter()
                .map(|(index, row)| self.read_row(&client, &asked[*index], row, deadline)),
        )
        .await;
        for (index, service) in asked.iter().enumerate() {
            let state = state_of(
                rows.iter()
                    .zip(&answers)
                    .filter(|((i, _), _)| *i == index)
                    .map(|(_, answer)| answer),
            );
            outcomes.push((service.public.id.clone(), state));
        }
        let server_id = lock(&self.hub).server_id.clone().unwrap_or_default();
        let mut shelves = Vec::new();
        for ((index, row), answer) in rows.into_iter().zip(answers) {
            let RowAnswer::Items(items) = answer else {
                continue;
            };
            let server_id = server_id.clone();
            let found = self
                .convert(&asked[index], move |c| {
                    shelf(c, &server_id, &row, &items, limit as usize)
                })
                .await?;
            shelves.extend(found);
        }
        let answer = finish(&order, shelves, outcomes);
        self.keep(key, &answer, generation);
        Ok(answer)
    }

    /// One row's items, read within `deadline` under the catalog's request cap.
    async fn read_row(
        &self,
        client: &Client,
        service: &Service,
        row: &RecommendationFolder,
        deadline: Instant,
    ) -> RowAnswer {
        if !row.items.is_empty() {
            // Servers before 2.10.14 send a row's items with it.
            return RowAnswer::Items(row.items.clone());
        }
        let answer = tokio::time::timeout_at(deadline, async {
            let _permit = self.catalog.calls.acquire().await.ok()?;
            Some(
                client
                    .recommendation_items(&row.provider, &row.item_id, &[], false)
                    .await,
            )
        })
        .await;
        match answer {
            Err(_) | Ok(Some(Err(RequestError::Timeout(_)))) => RowAnswer::TimedOut,
            Ok(None) => RowAnswer::Failed(ServiceSearchState::Failed),
            Ok(Some(Err(e))) => {
                tracing::debug!(service = %service.public.id, error = %e,
                    "a recommendation row could not be read");
                RowAnswer::Failed(failure_state(&e))
            }
            Ok(Some(Ok(items))) => RowAnswer::Items(items),
        }
    }

    /// Keeps an answer, unless the services changed since it was started.
    fn keep(&self, key: Key, answer: &Recommendations, generation: u64) {
        let weight = answer.shelves.iter().map(|s| s.items.len()).sum::<usize>() + 1;
        let ttl = if answer.complete { TTL } else { PARTIAL_TTL };
        let mut kept = lock(&self.catalog.recommendations);
        if self.catalog.services_generation.load(Ordering::SeqCst) != generation {
            return;
        }
        kept.put(key, Arc::new(answer.clone()), weight, ttl, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use trackside_core::catalog::{CatalogService, Quality};
    use trackside_core::catalog_ref::{CatalogRef, RefKind};
    use trackside_mass::Endpoint;

    use crate::catalog::convert::Source;

    const APPLE: &str = "apple_music--Hq7cT2";
    const SPOTIFY: &str = "spotify--Xb4LkP2q";

    fn service(instance: &str, account: AccountState, recommends: bool) -> Service {
        let domain = instance.split("--").next().unwrap_or(instance).to_owned();
        let mut capabilities = vec![ServiceCapability::Search];
        if recommends {
            capabilities.insert(0, ServiceCapability::Recommendations);
        }
        Service {
            instance_id: instance.to_owned(),
            domain: domain.clone(),
            public: CatalogService {
                id: ServiceId::derive("s", instance),
                name: domain.clone(),
                logo_key: domain,
                capabilities,
                account,
                account_detail: None,
                quality: Some(Quality::High),
            },
        }
    }

    fn row(value: serde_json::Value) -> RecommendationFolder {
        serde_json::from_value(value).unwrap_or_else(|e| panic!("{e}"))
    }

    fn items(value: serde_json::Value) -> Vec<RecommendationItem> {
        let serde_json::Value::Array(entries) = value else {
            return Vec::new();
        };
        entries
            .into_iter()
            .filter_map(|e| serde_json::from_value(e).ok())
            .collect()
    }

    fn convert<T>(instance: &str, f: impl FnOnce(&mut Converter<'_>) -> T) -> T {
        let domain = instance.split("--").next().unwrap_or(instance);
        let service = ServiceId::derive("s", instance);
        let endpoint = Endpoint::parse("http://ma.local:8095").ok();
        let mut converter = Converter::new(Source {
            instance_id: instance,
            domain,
            service: &service,
            endpoint: endpoint.as_ref(),
        });
        f(&mut converter)
    }

    #[test]
    fn only_signed_in_services_that_recommend_are_asked() {
        let services = vec![
            service(APPLE, AccountState::Ok, true),
            service(SPOTIFY, AccountState::SignedOut, true),
            service("tidal--x", AccountState::Ok, false),
        ];
        let (asked, told) = pick(&services, None);
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].instance_id, APPLE);
        assert!(told.is_empty());

        let ids: Vec<ServiceId> = [SPOTIFY, "tidal--x", APPLE, APPLE, "gone--1"]
            .iter()
            .map(|i| ServiceId::derive("s", i))
            .collect();
        let (asked, told) = pick(&services, Some(&ids));
        assert_eq!(asked.len(), 1, "asked once, unknown ids ignored");
        let states: Vec<ServiceSearchState> = told.iter().map(|(_, s)| *s).collect();
        assert_eq!(
            states,
            [ServiceSearchState::SignedOut, ServiceSearchState::Failed]
        );
    }

    #[test]
    fn library_and_foreign_rows_are_dropped() {
        let asked = vec![
            service(APPLE, AccountState::Ok, true),
            service(SPOTIFY, AccountState::Ok, true),
        ];
        let all = vec![
            row(
                json!({"item_id": "recently_played", "provider": "recommendations", "name": "Recently played"}),
            ),
            row(json!({"item_id": "made-for-you", "provider": APPLE, "name": "Made for You"})),
            row(
                json!({"item_id": "similar", "provider": "lastfm_recommendations", "name": "Similar"}),
            ),
            row(json!({"item_id": "mixes", "provider": "spotify", "name": "Your mixes"})),
            row(json!({"item_id": "hidden", "provider": "tidal--x", "name": "Hidden service"})),
            row(
                json!({"item_id": "noisy", "provider": APPLE, "name": "Noisy", "enabled_by_default": false}),
            ),
            row(json!({"item_id": "made-for-you", "provider": APPLE, "name": "Again"})),
            row(json!({"item_id": "untitled", "provider": APPLE, "name": ""})),
        ];
        let kept: Vec<(usize, String)> = rows(all, &asked)
            .into_iter()
            .map(|(i, r)| (i, r.item_id))
            .collect();
        assert_eq!(
            kept,
            [
                (0, String::from("made-for-you")),
                (1, String::from("mixes")),
            ],
            "a row named by its domain belongs to that service"
        );

        let many = (0..MAX_SHELVES + 5)
            .map(|n| row(json!({"item_id": format!("r{n}"), "provider": APPLE, "name": "R"})))
            .collect();
        assert_eq!(rows(many, &asked).len(), MAX_SHELVES);
    }

    #[test]
    fn stations_of_2_10_are_playable_playlists_and_of_2_11_radios() {
        let made = row(
            json!({"item_id": "made-for-you", "provider": APPLE, "name": " Made for You ",
                              "subtitle": "  "}),
        );
        let mapping = |id: &str, kind: &str| {
            json!({"item_id": id, "provider": APPLE, "name": "Station", "media_type": kind,
                   "is_dynamic": true,
                   "provider_mappings": [{"item_id": id, "provider_domain": "apple_music",
                                          "provider_instance": APPLE, "available": true}],
                   "metadata": {"images": [{"type": "thumb", "path": format!("{kind}/{id}"), "provider": APPLE,
                                            "proxy_id": "0123456789abcdef".repeat(4)}]}})
        };
        let old = items(json!([mapping("ra.1", "playlist")]));
        let new = items(json!([mapping("ra.1", "radio")]));
        let (old, new) = convert(APPLE, |c| {
            (
                shelf(c, "s", &made, &old, 20),
                shelf(c, "s", &made, &new, 20),
            )
        });
        let (old, new) = (old.expect("a shelf"), new.expect("a shelf"));
        assert_eq!(old.id, new.id, "the shelf keeps its id across versions");
        assert_eq!(old.id, ShelfId::derive("s", APPLE, "made-for-you"));
        assert_eq!(old.title, "Made for You");
        assert_eq!(old.subtitle, None);
        assert_eq!(old.kind, ShelfKind::Default);
        let CatalogEntry::Playlist(playlist) = &old.items[0] else {
            panic!("2.10 stations are playlists: {:?}", old.items[0]);
        };
        assert_eq!(
            CatalogRef::decode(&playlist.reference).map(|r| (r.kind, r.item_id)),
            Ok((RefKind::Playlist, String::from("ra.1"))),
            "2.10 plays a station as playlist/ra.…"
        );
        assert_eq!(playlist.editable, Some(false));
        assert!(playlist.artwork.is_some());
        let CatalogEntry::Radio(radio) = &new.items[0] else {
            panic!("2.11 stations are radios: {:?}", new.items[0]);
        };
        assert_eq!(
            CatalogRef::decode(&radio.reference).map(|r| (r.kind, r.uri_on(APPLE))),
            Ok((RefKind::Radio, format!("{APPLE}://radio/ra.1"),))
        );
    }

    #[test]
    fn shelves_keep_the_kinds_the_catalog_shows_once_each_up_to_the_limit() {
        let timeline = row(
            json!({"item_id": "t", "provider": SPOTIFY, "name": "Recent", "type": "timeline",
                                  "subtitle": "Picked for you"}),
        );
        let found = items(json!([
            {"item_id": "a1", "provider": SPOTIFY, "name": "Album", "media_type": "album",
             "artists": [{"item_id": "r1", "provider": SPOTIFY, "name": "Beyoncé", "media_type": "artist"}]},
            {"item_id": "t1", "provider": "spotify", "name": "Halo", "media_type": "track",
             "artists": [{"item_id": "r1", "provider": SPOTIFY, "name": "Beyoncé", "media_type": "artist"}]},
            {"item_id": "t1", "provider": SPOTIFY, "name": "Halo again", "media_type": "track"},
            {"item_id": "f", "provider": SPOTIFY, "name": "Genres", "media_type": "folder", "path": "x"},
            {"item_id": "p", "provider": SPOTIFY, "name": "Podcast", "media_type": "podcast"},
            {"item_id": "x", "provider": SPOTIFY, "name": "Source", "media_type": "audio_source"},
            {"item_id": "o", "provider": "tidal--x", "name": "Elsewhere", "media_type": "track"},
            {"item_id": "r9", "provider": SPOTIFY, "name": "Artist", "media_type": "artist"},
            {"item_id": "pl", "provider": SPOTIFY, "name": "Playlist", "media_type": "playlist"}
        ]));
        let shelf = convert(SPOTIFY, |c| shelf(c, "s", &timeline, &found, 3)).expect("a shelf");
        assert_eq!(shelf.kind, ShelfKind::Timeline);
        assert_eq!(shelf.subtitle.as_deref(), Some("Picked for you"));
        let names: Vec<&str> = shelf
            .items
            .iter()
            .map(|e| match e {
                CatalogEntry::Album(a) => a.name.as_str(),
                CatalogEntry::Track(t) => t.title.as_str(),
                CatalogEntry::Artist(a) => a.name.as_str(),
                CatalogEntry::Playlist(p) => p.name.as_str(),
                CatalogEntry::Radio(r) => r.name.as_str(),
            })
            .collect();
        assert_eq!(names, ["Album", "Halo", "Artist"]);
        let CatalogEntry::Track(halo) = &shelf.items[1] else {
            panic!("a track");
        };
        assert_eq!(
            halo.song_key.as_deref(),
            Some("halo\u{1f}beyonce"),
            "the key owned songs are matched by"
        );
        let CatalogEntry::Album(album) = &shelf.items[0] else {
            panic!("an album");
        };
        assert!(album.album_key.is_some());

        let nothing = items(
            json!([{"item_id": "f", "provider": SPOTIFY, "name": "F", "media_type": "folder"}]),
        );
        assert!(convert(SPOTIFY, |c| super::shelf(c, "s", &timeline, &nothing, 3)).is_none());
    }

    #[test]
    fn a_change_to_the_services_forgets_every_answer() {
        let state = crate::catalog::State::default();
        let key = Key {
            user: UserKey(1),
            services: vec![ServiceId::derive("s", APPLE)],
            limit: 20,
        };
        let answer = Arc::new(finish(&[], Vec::new(), Vec::new()));
        lock(&state.recommendations).put(key.clone(), answer, 1, TTL, Instant::now());
        let before = state.services_generation.load(Ordering::SeqCst);
        state.services_changed();
        assert!(
            lock(&state.recommendations)
                .get(&key, Instant::now())
                .is_none()
        );
        assert_ne!(
            state.services_generation.load(Ordering::SeqCst),
            before,
            "an answer started before the change is not kept"
        );
        state.reset();
        assert_eq!(state.services_generation.load(Ordering::SeqCst), before + 2);
    }

    #[test]
    fn a_service_state_follows_its_rows() {
        let items = || RowAnswer::Items(Vec::new());
        let failed = || RowAnswer::Failed(ServiceSearchState::Failed);
        assert_eq!(state_of([].iter()), ServiceSearchState::Ok);
        assert_eq!(state_of([items(), failed()].iter()), ServiceSearchState::Ok);
        assert_eq!(state_of([failed()].iter()), ServiceSearchState::Failed);
        assert_eq!(
            state_of([RowAnswer::Failed(ServiceSearchState::SignedOut)].iter()),
            ServiceSearchState::SignedOut
        );
        assert_eq!(
            state_of([items(), RowAnswer::TimedOut].iter()),
            ServiceSearchState::TimedOut
        );
    }

    #[test]
    fn outcomes_follow_the_order_asked_and_a_timeout_is_incomplete() {
        let a = ServiceId::derive("s", "a");
        let b = ServiceId::derive("s", "b");
        let done = finish(
            &[a.clone(), b.clone()],
            Vec::new(),
            vec![
                (b.clone(), ServiceSearchState::TimedOut),
                (a.clone(), ServiceSearchState::Ok),
            ],
        );
        assert_eq!(done.services[0].service, a);
        assert!(!done.complete);
        let done = finish(
            std::slice::from_ref(&a),
            Vec::new(),
            vec![(a.clone(), ServiceSearchState::Failed)],
        );
        assert!(done.complete, "a failure is not a timeout");
    }
}
