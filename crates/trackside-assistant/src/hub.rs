//! The speakers' state: Music Assistant's players and queues as last seen, the speakers made
//! from them, and which changes to announce. Pure and synchronous, so it is held under a
//! plain mutex for microseconds and tested without a server.
//!
//! Every change Trackside sees that is not just the position moving gives the speaker a new
//! revision. `speaker_updated` goes out at most [`UPDATE_EVERY`] per speaker, carrying the
//! latest state; bursts in between collapse into it. `speaker_progress` goes out on play,
//! pause, seek and item changes, when the position drifts from where it should be, and every
//! [`HEARTBEAT`] while playing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;
use trackside_core::assistant::{
    PlayContext, PlayState, Speaker, SpeakerEvent, SpeakerRelay, SpeakersState,
};
use trackside_core::{CatalogArtId, Millis, SongId, SpeakerId};
use trackside_mass::models::{Player, PlayerQueue, ProviderInstance};
use trackside_mass::{Endpoint, Event, Snapshot};

use crate::convert::{self, Placement, View};

/// The most `speaker_updated` events per speaker: four a second.
pub(crate) const UPDATE_EVERY: Duration = Duration::from_millis(250);
/// `speaker_progress` while playing, even when nothing else happens.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(30);
/// How far the real position may drift from the interpolated one before clients are told.
pub(crate) const DRIFT_MS: u64 = 1500;
/// Songs and art addresses remembered; beyond this the oldest are forgotten.
const MAX_CACHED: usize = 4096;

/// Revisions only ever grow within one run, across every speaker.
static REVISIONS: AtomicU64 = AtomicU64::new(1);

fn next_revision() -> u64 {
    REVISIONS.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug)]
struct Entry {
    current: Arc<Speaker>,
    published: u64,
    next_update: Instant,
    progress: Option<Progress>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Progress {
    state: PlayState,
    position_ms: u64,
    position_at: Millis,
    item: Option<String>,
    sent: Instant,
}

/// The state of every speaker.
#[derive(Debug, Default)]
pub(crate) struct Hub {
    pub server_id: Option<String>,
    pub endpoint: Option<Endpoint>,
    pub players: HashMap<String, Player>,
    pub queues: HashMap<String, PlayerQueue>,
    pub providers: Vec<ProviderInstance>,
    /// Trackside's own `OpenSubsonic` instance in Music Assistant.
    pub ours: Option<String>,
    pub songs: HashMap<SongId, Placement>,
    pub contexts: HashMap<String, PlayContext>,
    pub art: HashMap<CatalogArtId, String>,
    /// Grows whenever a queue's items change, for queue cursors.
    pub queue_items: HashMap<String, u64>,
    entries: HashMap<String, Entry>,
    by_speaker: HashMap<SpeakerId, String>,
    pub loaded: bool,
    /// Relay players with a live session, shown to their owner only.
    pub relays: HashMap<String, SpeakerRelay>,
    /// Every relay player Trackside has a key for, live or not: never shown to anyone else
    /// and never offered for grouping.
    pub relay_players: HashSet<String>,
}

impl Hub {
    pub fn view(&self) -> Option<View<'_>> {
        Some(View {
            server_id: self.server_id.as_deref()?,
            players: &self.players,
            queues: &self.queues,
            providers: &self.providers,
            ours: self.ours.as_deref(),
            songs: &self.songs,
            contexts: &self.contexts,
            endpoint: self.endpoint.as_ref(),
            relay_players: &self.relay_players,
        })
    }

    /// Forgets everything: a new connection, or none.
    pub fn reset(&mut self, server_id: Option<String>, endpoint: Option<Endpoint>) {
        let songs = std::mem::take(&mut self.songs);
        let art = std::mem::take(&mut self.art);
        let contexts = std::mem::take(&mut self.contexts);
        let relays = std::mem::take(&mut self.relays);
        let relay_players = std::mem::take(&mut self.relay_players);
        let same_server = server_id.is_some() && server_id == self.server_id;
        *self = Self {
            server_id,
            endpoint,
            relay_players,
            ..Self::default()
        };
        if same_server {
            self.relays = relays;
        }
        if same_server {
            self.songs = songs;
            self.art = art;
            self.contexts = contexts;
        }
    }

    pub fn load(&mut self, snapshot: &Snapshot) {
        self.players = snapshot
            .players
            .iter()
            .map(|p| (p.player_id.clone(), p.clone()))
            .collect();
        self.queues = snapshot
            .queues
            .iter()
            .map(|q| (q.queue_id.clone(), q.clone()))
            .collect();
        for id in self.queues.keys() {
            self.queue_items.insert(id.clone(), next_revision());
        }
        self.loaded = true;
    }

    /// Applies an event; `true` when the queue's items changed.
    pub fn apply(&mut self, event: &Event, now_epoch_s: f64) -> Option<String> {
        match event {
            Event::PlayerAdded(player) | Event::PlayerUpdated(player) => {
                self.players
                    .insert(player.player_id.clone(), player.clone());
            }
            Event::PlayerRemoved { player_id } => {
                self.players.remove(player_id);
            }
            Event::QueueAdded(queue) | Event::QueueUpdated(queue) => {
                self.queues.insert(queue.queue_id.clone(), queue.clone());
            }
            Event::QueueItemsUpdated(queue) => {
                self.queues.insert(queue.queue_id.clone(), queue.clone());
                self.queue_items
                    .insert(queue.queue_id.clone(), next_revision());
                return Some(queue.queue_id.clone());
            }
            Event::QueueTimeUpdated {
                queue_id,
                elapsed_time,
            } => {
                if let Some(queue) = self.queues.get_mut(queue_id) {
                    queue.elapsed_time = *elapsed_time;
                    queue.elapsed_time_last_updated = Some(now_epoch_s);
                }
            }
            Event::ProvidersUpdated(providers) => {
                self.providers.clone_from(providers);
            }
            _ => {}
        }
        None
    }

    pub fn remember_songs(&mut self, found: HashMap<SongId, Placement>, asked: &[SongId]) {
        if self.songs.len() + asked.len() > MAX_CACHED {
            self.songs.clear();
        }
        self.songs.extend(found);
    }

    fn remember_art(&mut self, seen: convert::ArtSeen) {
        if self.art.len() + seen.len() > MAX_CACHED {
            self.art.clear();
        }
        self.art.extend(seen);
    }

    pub fn player_of(&self, id: &SpeakerId) -> Option<&Player> {
        self.by_speaker.get(id).and_then(|p| self.players.get(p))
    }

    pub fn speaker(&self, id: &SpeakerId) -> Option<Arc<Speaker>> {
        let player = self.by_speaker.get(id)?;
        self.entries.get(player).map(|e| Arc::clone(&e.current))
    }

    /// Whether a player is a relay's (any phone's).
    pub fn is_relay(&self, player_id: &str) -> bool {
        self.relay_players.contains(player_id) || self.relays.contains_key(player_id)
    }

    pub fn speakers(&self) -> Vec<Arc<Speaker>> {
        let mut list: Vec<Arc<Speaker>> = self
            .entries
            .values()
            .map(|e| Arc::clone(&e.current))
            .collect();
        convert::order(&mut list);
        list
    }

    /// Recomputes every speaker and returns what to announce. `now` is the server clock,
    /// `instant` the monotonic one the rate limits run on.
    pub fn refresh(&mut self, now: Millis, instant: Instant) -> Vec<SpeakerEvent> {
        let mut events = Vec::new();
        if !self.loaded {
            return events;
        }
        let Some(view) = self.view() else {
            return events;
        };
        let mut art = Vec::new();
        let fresh: Vec<(String, Speaker)> = self
            .players
            .values()
            .filter_map(|p| {
                let speaker = match self.relays.get(&p.player_id) {
                    Some(relay) => convert::as_relay(view.speaker(p, &mut art), relay),
                    None if view.shown(p) => view.speaker(p, &mut art),
                    None => return None,
                };
                Some((p.player_id.clone(), speaker))
            })
            .collect();
        self.remember_art(art);
        let mut seen = std::collections::HashSet::new();
        for (player_id, mut speaker) in fresh {
            seen.insert(player_id.clone());
            match self.entries.get_mut(&player_id) {
                None => {
                    speaker.revision = next_revision();
                    let speaker = Arc::new(speaker);
                    self.by_speaker
                        .insert(speaker.id.clone(), player_id.clone());
                    events.push(SpeakerEvent::Added(Arc::clone(&speaker)));
                    let progress = progress_of(&speaker, instant);
                    self.entries.insert(
                        player_id,
                        Entry {
                            published: speaker.revision,
                            current: speaker,
                            next_update: instant + UPDATE_EVERY,
                            progress,
                        },
                    );
                }
                Some(entry) => {
                    if convert::same_but_position(&entry.current, &speaker) {
                        speaker.revision = entry.current.revision;
                    } else {
                        speaker.revision = next_revision();
                    }
                    entry.current = Arc::new(speaker);
                    if let Some(event) = progress_event(entry, now, instant) {
                        events.push(event);
                    }
                }
            }
        }
        let gone: Vec<String> = self
            .entries
            .keys()
            .filter(|p| !seen.contains(*p))
            .cloned()
            .collect();
        for player in gone {
            if let Some(entry) = self.entries.remove(&player) {
                self.by_speaker.remove(&entry.current.id);
                events.push(SpeakerEvent::Removed(entry.current.id.clone()));
            }
        }
        events.extend(self.flush(instant));
        events
    }

    /// `speaker_updated` for every speaker whose latest state is unannounced and whose rate
    /// allows it now.
    pub fn flush(&mut self, instant: Instant) -> Vec<SpeakerEvent> {
        let mut events = Vec::new();
        for entry in self.entries.values_mut() {
            if entry.current.revision > entry.published && instant >= entry.next_update {
                entry.published = entry.current.revision;
                entry.next_update = instant + UPDATE_EVERY;
                events.push(SpeakerEvent::Updated(Arc::clone(&entry.current)));
            }
        }
        events
    }

    /// When the next flush or heartbeat is due, if any.
    pub fn next_due(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter_map(|entry| {
                let update =
                    (entry.current.revision > entry.published).then_some(entry.next_update);
                let heartbeat = entry
                    .progress
                    .as_ref()
                    .filter(|p| p.state == PlayState::Playing)
                    .map(|p| p.sent + HEARTBEAT);
                match (update, heartbeat) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            })
            .min()
    }

    /// Heartbeats due now, for speakers that kept playing.
    pub fn heartbeats(&mut self, now: Millis, instant: Instant) -> Vec<SpeakerEvent> {
        self.entries
            .values_mut()
            .filter_map(|entry| progress_event(entry, now, instant))
            .collect()
    }

    /// Bumps a speaker's revision without another change (its queue's items changed).
    pub fn touch_queue(&mut self, queue_id: &str) -> Vec<SpeakerEvent> {
        let mut events = Vec::new();
        let players: Vec<String> = self
            .entries
            .keys()
            .filter(|player| {
                self.players.get(*player).is_some_and(|p| {
                    p.active_source.as_deref() == Some(queue_id) || p.player_id == queue_id
                })
            })
            .cloned()
            .collect();
        for player in players {
            if let Some(entry) = self.entries.get_mut(&player) {
                let mut speaker = (*entry.current).clone();
                speaker.revision = next_revision();
                events.push(SpeakerEvent::Queue {
                    id: speaker.id.clone(),
                    revision: speaker.revision,
                });
                entry.current = Arc::new(speaker);
            }
        }
        events
    }
}

fn progress_of(speaker: &Speaker, instant: Instant) -> Option<Progress> {
    speaker.playback.as_ref().map(|p| Progress {
        state: p.state,
        position_ms: p.position_ms,
        position_at: p.position_at,
        item: speaker.now_playing.as_ref().and_then(|n| n.item_id.clone()),
        sent: instant,
    })
}

/// Whether the position moved in a way clients cannot predict, or a heartbeat is due.
fn progress_event(entry: &mut Entry, now: Millis, instant: Instant) -> Option<SpeakerEvent> {
    let speaker = &entry.current;
    let Some(playback) = &speaker.playback else {
        entry.progress = None;
        return None;
    };
    let item = speaker.now_playing.as_ref().and_then(|n| n.item_id.clone());
    let at_now = |position_ms: u64, at: Millis, state: PlayState| {
        if state == PlayState::Playing {
            position_ms + u64::try_from(now.0 - at.0).unwrap_or(0)
        } else {
            position_ms
        }
    };
    let due = match &entry.progress {
        None => true,
        Some(last) => {
            let expected = at_now(last.position_ms, last.position_at, last.state);
            let actual = at_now(playback.position_ms, playback.position_at, playback.state);
            last.state != playback.state
                || last.item != item
                || expected.abs_diff(actual) > DRIFT_MS
                || (playback.state == PlayState::Playing && instant >= last.sent + HEARTBEAT)
        }
    };
    if !due {
        return None;
    }
    // A heartbeat reports where playback is now, not where it was last measured.
    let (position_ms, position_at) = if playback.state == PlayState::Playing
        && entry
            .progress
            .as_ref()
            .is_some_and(|l| l.state == playback.state && l.item == item)
    {
        let elapsed = u64::try_from(now.0 - playback.position_at.0).unwrap_or(0);
        let mut position = playback.position_ms + elapsed;
        if let Some(duration) = speaker
            .now_playing
            .as_ref()
            .and_then(|n| n.media.duration_ms)
        {
            position = position.min(duration);
        }
        (position, now)
    } else {
        (playback.position_ms, playback.position_at)
    };
    entry.progress = Some(Progress {
        state: playback.state,
        position_ms,
        position_at,
        item,
        sent: instant,
    });
    Some(SpeakerEvent::Progress {
        id: speaker.id.clone(),
        position_ms,
        position_at,
        revision: speaker.revision,
        state: playback.state,
    })
}

/// The list state after a connection change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ListState {
    pub state: SpeakersState,
    pub since: Option<Millis>,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use trackside_mass::testing::{player, queue};
    use trackside_mass::{Event, Snapshot, SnapshotReason};

    use super::*;

    fn decode<T: serde::de::DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).unwrap_or_else(|e| panic!("{e}"))
    }

    fn hub_with(players: Vec<Value>) -> Hub {
        let mut hub = Hub::default();
        hub.reset(Some(String::from("server")), None);
        let queues = players
            .iter()
            .map(|p| decode(queue(p["player_id"].as_str().unwrap_or_default())))
            .collect();
        hub.load(&Snapshot::new(
            players.into_iter().map(decode).collect(),
            queues,
            SnapshotReason::Connected,
        ));
        hub
    }

    fn playing(position: f64, at: f64) -> Value {
        let mut p = player("p1", "Kitchen", "sonos--x");
        p["playback_state"] = json!("playing");
        p["state"] = json!("playing");
        p["current_media"] = json!({"uri": "x://track/1", "media_type": "track", "title": "Halo",
                                    "artist": "Beyoncé", "duration": 300, "source_id": "radio-thing",
                                    "elapsed_time": position, "elapsed_time_last_updated": at});
        p["active_source"] = json!("radio-thing");
        p["elapsed_time"] = json!(position);
        p["elapsed_time_last_updated"] = json!(at);
        p
    }

    fn names(events: &[SpeakerEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                SpeakerEvent::Added(_) => "added",
                SpeakerEvent::Updated(_) => "updated",
                SpeakerEvent::Removed(_) => "removed",
                SpeakerEvent::Progress { .. } => "progress",
                SpeakerEvent::Queue { .. } => "queue",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn a_new_connection_announces_every_speaker() {
        let mut hub = hub_with(vec![
            player("p1", "Kitchen", "sonos--x"),
            player("p2", "Den", "sonos--x"),
        ]);
        let events = hub.refresh(Millis(0), Instant::now());
        assert_eq!(names(&events), ["added", "added"]);
        let list = hub.speakers();
        assert_eq!(list[0].name, "Den", "ordered by name");
    }

    #[tokio::test(start_paused = true)]
    async fn bursts_of_changes_collapse_into_one_update_per_quarter_second() {
        let mut hub = hub_with(vec![player("p1", "Kitchen", "sonos--x")]);
        let start = Instant::now();
        hub.refresh(Millis(0), start);
        let mut updates = 0;
        for volume in 1..=20_u32 {
            let mut p = player("p1", "Kitchen", "sonos--x");
            p["volume_level"] = json!(volume);
            hub.apply(&Event::PlayerUpdated(decode(p)), 0.0);
            let at = start + Duration::from_millis(u64::from(volume) * 10);
            updates += names(&hub.refresh(Millis(0), at))
                .iter()
                .filter(|n| **n == "updated")
                .count();
        }
        let due = hub.next_due().unwrap_or(start);
        updates += names(&hub.flush(due)).len();
        assert!(updates <= 2, "{updates} updates for 20 changes in 200 ms");
        assert_eq!(hub.speakers()[0].volume, Some(20), "the latest value wins");
    }

    #[test]
    fn position_alone_moves_neither_the_revision_nor_an_update() {
        let mut hub = hub_with(vec![playing(10.0, 1000.0)]);
        let now = Instant::now();
        let first = hub.refresh(Millis(1_000_000), now);
        assert_eq!(names(&first), ["added"], "the speaker carries its playback");
        let revision = hub.speakers()[0].revision;
        hub.apply(&Event::PlayerUpdated(decode(playing(15.0, 1005.0))), 0.0);
        let events = hub.refresh(Millis(1_005_000), now + Duration::from_secs(5));
        assert!(
            names(&events).is_empty(),
            "on schedule: {:?}",
            names(&events)
        );
        assert_eq!(hub.speakers()[0].revision, revision);
        hub.apply(&Event::PlayerUpdated(decode(playing(100.0, 1006.0))), 0.0);
        let events = hub.refresh(Millis(1_006_000), now + Duration::from_secs(6));
        assert_eq!(names(&events), ["progress"], "a seek is announced");
    }

    #[test]
    fn heartbeats_come_every_thirty_seconds_while_playing() {
        let mut hub = hub_with(vec![playing(10.0, 1000.0)]);
        let now = Instant::now();
        hub.refresh(Millis(1_000_000), now);
        assert!(
            hub.heartbeats(Millis(1_010_000), now + Duration::from_secs(10))
                .is_empty()
        );
        let beat = hub.heartbeats(Millis(1_031_000), now + HEARTBEAT + Duration::from_secs(1));
        match beat.as_slice() {
            [SpeakerEvent::Progress { position_ms, .. }] => assert_eq!(*position_ms, 41_000),
            other => panic!("{:?}", names(other)),
        }
    }

    #[test]
    fn a_hidden_or_removed_player_is_removed() {
        let mut hub = hub_with(vec![player("p1", "Kitchen", "sonos--x")]);
        hub.refresh(Millis(0), Instant::now());
        let mut hidden = player("p1", "Kitchen", "sonos--x");
        hidden["hide_in_ui"] = json!(true);
        hub.apply(&Event::PlayerUpdated(decode(hidden)), 0.0);
        assert_eq!(names(&hub.refresh(Millis(0), Instant::now())), ["removed"]);
    }

    #[test]
    fn queue_changes_bump_the_revision() {
        let mut hub = hub_with(vec![player("p1", "Kitchen", "sonos--x")]);
        hub.refresh(Millis(0), Instant::now());
        let before = hub.speakers()[0].revision;
        let events = hub.touch_queue("p1");
        assert!(
            matches!(events.as_slice(), [SpeakerEvent::Queue { revision, .. }] if *revision > before)
        );
    }
}
