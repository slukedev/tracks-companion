//! Batch catalog matching (api.md 21.10): many song identities found at the services in one
//! request, for apps that play a song from a service when the user prefers it there.
//!
//! Each identity is looked up with one track search per service, limited to that service,
//! under the catalog's server-wide request cap and within [`MATCH_WAIT`]. What a lookup found,
//! or that it found nothing, is kept in memory and shared between users (the services are
//! the household's accounts, and owned songs are named per caller by the API), so asking
//! again costs Music Assistant nothing. A lookup that is not in memory spends one of the
//! caller's lookups; once those run out the rest are `deferred` rather than sent.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use tokio::time::Instant;
use trackside_core::ServiceId;
use trackside_core::catalog::{
    AccountState, CatalogError, CatalogKind, CatalogTrack, LookupBudget, MATCH_WAIT, MatchIdentity,
    MatchQuery, MatchResult, MatchState, Matches, ServiceCapability, ServiceMatch,
};
use trackside_mass::{RequestError, SearchQuery};

use super::{Service, convert, request_error};
use crate::{Inner, lock};

/// Lookups kept, each one identity at one service.
pub(super) const KEPT: usize = 20_000;
/// How long a found track is kept, and that nothing was found.
const FOUND_TTL: Duration = Duration::from_hours(24);
const NONE_TTL: Duration = Duration::from_hours(6);
/// Tracks a lookup asks the service for.
const CANDIDATES: u32 = 10;
/// How far apart two durations may be for the same recording.
const DURATION_SLACK_MS: u64 = 10_000;

/// One identity at one service instance, as the cache knows it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    instance: String,
    isrc: Option<String>,
    song: String,
    /// Tens of seconds, so durations a little apart share a lookup.
    duration: Option<u64>,
}

fn key(instance: &str, identity: &MatchIdentity) -> Key {
    Key {
        instance: instance.to_owned(),
        isrc: isrc(identity),
        song: tracks_model::song_key(&identity.title, &identity.artist),
        duration: identity.duration_ms.map(|d| d / DURATION_SLACK_MS),
    }
}

fn isrc(identity: &MatchIdentity) -> Option<String> {
    identity
        .isrc
        .as_deref()
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .map(str::to_ascii_uppercase)
}

/// The candidate that is the same recording: the same ISRC first, else the same song key with
/// durations close enough when both are known.
pub(super) fn best(identity: &MatchIdentity, candidates: &[CatalogTrack]) -> Option<CatalogTrack> {
    if let Some(isrc) = isrc(identity)
        && let Some(found) = candidates
            .iter()
            .find(|t| t.isrc.as_deref() == Some(isrc.as_str()))
    {
        return Some(found.clone());
    }
    let song_key = tracks_model::song_key(&identity.title, &identity.artist);
    candidates
        .iter()
        .filter(|t| t.playable)
        .find(|t| {
            t.song_key.as_deref() == Some(song_key.as_str())
                && match (identity.duration_ms, t.duration_ms) {
                    (Some(a), Some(b)) => a.abs_diff(b) <= DURATION_SLACK_MS,
                    _ => true,
                }
        })
        .cloned()
}

/// The services a match asks, and those it reports without asking.
fn pick(services: &[Service], wanted: Option<&[ServiceId]>) -> Vec<(Service, Option<MatchState>)> {
    let searches = |s: &Service| s.public.has(ServiceCapability::Search);
    let state = |s: &Service| {
        if s.public.account != AccountState::Ok {
            Some(MatchState::SignedOut)
        } else if searches(s) {
            None
        } else {
            Some(MatchState::Failed)
        }
    };
    match wanted {
        Some(ids) => {
            let mut out: Vec<(Service, Option<MatchState>)> = Vec::new();
            for id in ids {
                if out.iter().any(|(s, _)| &s.public.id == id) {
                    continue;
                }
                if let Some(service) = services.iter().find(|s| &s.public.id == id) {
                    out.push((service.clone(), state(service)));
                }
            }
            out
        }
        None => services
            .iter()
            .filter(|s| s.public.account == AccountState::Ok && searches(s))
            .map(|s| (s.clone(), None))
            .collect(),
    }
}

/// How one lookup went.
enum Lookup {
    Kept(Arc<Option<CatalogTrack>>),
    Ask,
    Deferred,
    Told(MatchState),
}

impl Inner {
    pub(crate) async fn catalog_match(
        &self,
        query: MatchQuery,
        budget: &LookupBudget,
    ) -> Result<Matches, CatalogError> {
        let client = self.connected().map_err(|_| CatalogError::Unavailable)?;
        let deadline = Instant::now() + MATCH_WAIT;
        let services = pick(
            &self.catalog_services(Some(&client)).await,
            query.services.as_deref(),
        );
        let mut retry_after: Option<Duration> = None;
        let mut plan: Vec<Vec<Lookup>> = Vec::with_capacity(query.items.len());
        {
            let mut kept = lock(&self.catalog.matches);
            let now = Instant::now();
            for identity in &query.items {
                let mut row = Vec::with_capacity(services.len());
                for (service, told) in &services {
                    row.push(match told {
                        Some(state) => Lookup::Told(*state),
                        None => match kept.get(&key(&service.instance_id, identity), now) {
                            Some(found) => Lookup::Kept(found),
                            None if retry_after.is_some() => Lookup::Deferred,
                            None => match budget() {
                                Ok(()) => Lookup::Ask,
                                Err(wait) => {
                                    retry_after = Some(wait);
                                    Lookup::Deferred
                                }
                            },
                        },
                    });
                }
                plan.push(row);
            }
        }
        let asks = plan.iter().enumerate().flat_map(|(i, row)| {
            row.iter()
                .enumerate()
                .filter(|(_, l)| matches!(l, Lookup::Ask))
                .map(move |(s, _)| (i, s))
        });
        let asked: Vec<(usize, usize)> = asks.collect();
        let answers = join_all(asked.iter().map(|&(item, service)| {
            self.look_up(&client, &services[service].0, &query.items[item], deadline)
        }))
        .await;
        let mut complete = retry_after.is_none();
        let mut results: Vec<MatchResult> = query
            .items
            .iter()
            .zip(&plan)
            .map(|(identity, row)| MatchResult {
                id: identity.id.clone(),
                matches: row
                    .iter()
                    .zip(&services)
                    .map(|(lookup, (service, _))| {
                        let (state, track) = match lookup {
                            Lookup::Kept(found) => match found.as_ref() {
                                Some(track) => (MatchState::Found, Some(track.clone())),
                                None => (MatchState::None, None),
                            },
                            Lookup::Deferred => (MatchState::Deferred, None),
                            Lookup::Told(state) => (*state, None),
                            Lookup::Ask => (MatchState::TimedOut, None),
                        };
                        ServiceMatch {
                            service: service.public.id.clone(),
                            state,
                            track,
                        }
                    })
                    .collect(),
            })
            .collect();
        for ((item, service), answer) in asked.into_iter().zip(answers) {
            let slot = &mut results[item].matches[service];
            match answer {
                Ok(found) => {
                    slot.state = if found.is_some() {
                        MatchState::Found
                    } else {
                        MatchState::None
                    };
                    slot.track = found;
                }
                Err(state) => {
                    complete &= state != MatchState::TimedOut;
                    slot.state = state;
                }
            }
        }
        Ok(Matches {
            results,
            complete,
            retry_after,
        })
    }

    /// One identity at one service: a search limited to it, converted and matched, and kept.
    async fn look_up(
        &self,
        client: &trackside_mass::Client,
        service: &Service,
        identity: &MatchIdentity,
        deadline: Instant,
    ) -> Result<Option<CatalogTrack>, MatchState> {
        let mut search = SearchQuery::new(format!("{} {}", identity.artist, identity.title))
            .media_types([convert::media_type(CatalogKind::Track)])
            .limit(CANDIDATES);
        search.providers = vec![service.instance_id.clone()];
        let answer = tokio::time::timeout_at(deadline, async {
            let _permit = self.catalog.calls.acquire().await.ok()?;
            Some(client.search(&search).await)
        })
        .await;
        let found = match answer {
            Err(_) | Ok(Some(Err(RequestError::Timeout(_)))) => return Err(MatchState::TimedOut),
            Ok(None) => return Err(MatchState::Failed),
            Ok(Some(Err(e))) => {
                tracing::debug!(service = %service.public.id, error = %e, "a catalog match failed");
                return Err(match request_error(&e) {
                    CatalogError::SignedOut { .. } => MatchState::SignedOut,
                    _ => MatchState::Failed,
                });
            }
            Ok(Some(Ok(found))) => found,
        };
        let wanted = identity.clone();
        let track = self
            .convert(service, move |c| {
                let candidates: Vec<CatalogTrack> =
                    found.tracks.iter().filter_map(|t| c.track(t)).collect();
                best(&wanted, &candidates)
            })
            .await
            .map_err(|_| MatchState::Failed)?;
        let ttl = if track.is_some() { FOUND_TTL } else { NONE_TTL };
        lock(&self.catalog.matches).put(
            key(&service.instance_id, identity),
            Arc::new(track.clone()),
            1,
            ttl,
            Instant::now(),
        );
        Ok(track)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(
        title: &str,
        artist: &str,
        duration_ms: Option<u64>,
        isrc: Option<&str>,
    ) -> MatchIdentity {
        MatchIdentity {
            id: String::from("1"),
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: None,
            duration_ms,
            isrc: isrc.map(str::to_owned),
        }
    }

    fn track(title: &str, artist: &str, duration_ms: u64, isrc: Option<&str>) -> CatalogTrack {
        CatalogTrack {
            reference: format!("ref-{title}-{duration_ms}"),
            service: ServiceId::derive("s", "spotify--a"),
            title: title.to_owned(),
            artist: Some(artist.to_owned()),
            artists: Vec::new(),
            album: None,
            album_ref: None,
            artwork: None,
            duration_ms: Some(duration_ms),
            explicit: None,
            isrc: isrc.map(str::to_owned),
            track_number: None,
            disc_number: None,
            quality: None,
            song_key: Some(tracks_model::song_key(title, artist)),
            playable: true,
            saved: None,
        }
    }

    #[test]
    fn the_same_recording_is_found_by_isrc_then_by_song_key_and_length() {
        let candidates = vec![
            track("Halo - Live", "Beyoncé", 300_000, None),
            track("Halo", "Beyoncé", 400_000, None),
            track("Halo", "Beyoncé", 261_000, None),
            track(
                "Halo (Remastered)",
                "Beyoncé",
                262_000,
                Some("USSM10804555"),
            ),
        ];
        let by_isrc = best(
            &identity("Halo", "Beyonce", Some(261_640), Some("ussm10804555")),
            &candidates,
        );
        assert_eq!(
            by_isrc.map(|t| t.title),
            Some(String::from("Halo (Remastered)"))
        );
        let by_key = best(
            &identity("HALO", "Beyoncé", Some(261_640), None),
            &candidates,
        );
        assert_eq!(by_key.and_then(|t| t.duration_ms), Some(261_000));
        let no_length = best(&identity("Halo", "Beyoncé", None, None), &candidates);
        assert_eq!(no_length.and_then(|t| t.duration_ms), Some(400_000));
        assert_eq!(
            best(
                &identity("Halo", "Beyoncé", Some(100_000), None),
                &candidates
            ),
            None,
            "too far apart in length"
        );
        assert_eq!(
            best(
                &identity("Run the World", "Beyoncé", None, None),
                &candidates
            ),
            None
        );
    }

    #[test]
    fn close_lengths_share_a_lookup() {
        let a = key(
            "spotify--a",
            &identity("Halo", "Beyoncé", Some(261_640), None),
        );
        let b = key(
            "spotify--a",
            &identity("halo", "Beyonce", Some(262_100), None),
        );
        assert_eq!(a, b);
        let other = key(
            "spotify--b",
            &identity("Halo", "Beyoncé", Some(261_640), None),
        );
        assert_ne!(a, other, "one entry per service instance");
    }
}
