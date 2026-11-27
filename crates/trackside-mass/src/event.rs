//! Server events (`EventType`, models `enums.py`). Music Assistant sends every event the
//! account may see; there is no subscribe command.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::de;
use crate::models::{MediaItem, Player, PlayerQueue, ProviderInstance};

/// An event as sent: its name, the object it is about and its payload.
#[derive(Clone, Debug, PartialEq)]
pub struct RawEvent {
    pub event: String,
    /// A player id, queue id, item URI or provider instance id, depending on the event.
    pub object_id: Option<String>,
    pub data: Value,
}

/// A decoded event. Events this client does not know arrive as [`Event::Other`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Event {
    PlayerAdded(Player),
    PlayerUpdated(Player),
    PlayerRemoved {
        player_id: String,
    },
    QueueAdded(PlayerQueue),
    QueueUpdated(PlayerQueue),
    /// The items changed; the queue's own fields come along, the items do not. Fetch them with
    /// `player_queues/items`.
    QueueItemsUpdated(PlayerQueue),
    QueueTimeUpdated {
        queue_id: String,
        elapsed_time: f64,
    },
    MediaItemAdded {
        uri: String,
        item: Box<MediaItem>,
    },
    MediaItemUpdated {
        uri: String,
        item: Box<MediaItem>,
    },
    MediaItemDeleted {
        uri: String,
    },
    /// A library sync finished. Item events are muted during a sync, so refresh after this.
    MusicSyncCompleted,
    /// The providers the account may see.
    ProvidersUpdated(Vec<ProviderInstance>),
    /// A provider's own event, payload defined by the provider.
    ProviderEvent {
        /// The provider instance id.
        instance_id: String,
        /// The part after `/` in `object_id`, if any.
        scope: Option<String>,
        data: Value,
    },
    Other(RawEvent),
}

/// A known event whose payload did not decode.
#[derive(Debug)]
pub(crate) struct Undecoded {
    pub raw: RawEvent,
    pub error: String,
    /// Whether the event carries player or queue state, which is now unknown.
    pub state_lost: bool,
}

impl Event {
    pub(crate) fn decode(raw: RawEvent) -> Result<Self, Box<Undecoded>> {
        fn take<T: DeserializeOwned>(raw: &RawEvent) -> Result<T, String> {
            serde_json::from_value(raw.data.clone()).map_err(|e| e.to_string())
        }
        fn object(raw: &RawEvent) -> Result<String, String> {
            raw.object_id
                .clone()
                .ok_or_else(|| "no object_id".to_owned())
        }
        let decoded = match raw.event.as_str() {
            "player_added" => take(&raw).map(Self::PlayerAdded),
            "player_updated" => take(&raw).map(Self::PlayerUpdated),
            "player_removed" => object(&raw).map(|player_id| Self::PlayerRemoved { player_id }),
            "queue_added" => take(&raw).map(Self::QueueAdded),
            "queue_updated" => take(&raw).map(Self::QueueUpdated),
            "queue_items_updated" => take(&raw).map(Self::QueueItemsUpdated),
            "queue_time_updated" => object(&raw).and_then(|queue_id| {
                raw.data
                    .as_f64()
                    .map(|elapsed_time| Self::QueueTimeUpdated {
                        queue_id,
                        elapsed_time,
                    })
                    .ok_or_else(|| "elapsed time is not a number".to_owned())
            }),
            "media_item_added" | "media_item_updated" => take::<MediaItem>(&raw).map(|item| {
                let uri = raw
                    .object_id
                    .clone()
                    .or_else(|| item.uri.clone())
                    .unwrap_or_default();
                let item = Box::new(item);
                if raw.event == "media_item_added" {
                    Self::MediaItemAdded { uri, item }
                } else {
                    Self::MediaItemUpdated { uri, item }
                }
            }),
            "media_item_deleted" => object(&raw).map(|uri| Self::MediaItemDeleted { uri }),
            "music_sync_completed" => Ok(Self::MusicSyncCompleted),
            "providers_updated" => Ok(Self::ProvidersUpdated(match &raw.data {
                Value::Array(items) => de::decode_entries(items.clone()).0,
                _ => Vec::new(),
            })),
            "provider_event" => object(&raw).map(|id| {
                let (instance_id, scope) = match id.split_once('/') {
                    Some((instance, scope)) => (instance.to_owned(), Some(scope.to_owned())),
                    None => (id, None),
                };
                Self::ProviderEvent {
                    instance_id,
                    scope,
                    data: raw.data.clone(),
                }
            }),
            _ => return Ok(Self::Other(raw)),
        };
        decoded.map_err(|error| {
            let state_lost = raw.event.starts_with("player_") || raw.event.starts_with("queue_");
            Box::new(Undecoded {
                raw,
                error,
                state_lost,
            })
        })
    }
}

/// What a subscriber receives.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Update {
    Event(Arc<Event>),
    /// The full player and queue state: after every (re)connect, and after any subscriber
    /// lagged or an event could not be decoded. Replace local state with it.
    Snapshot(Arc<Snapshot>),
    /// This subscriber fell behind and missed updates. A snapshot follows once the server
    /// answers; until then local state may be stale.
    Lagged {
        missed: u64,
    },
}

/// Why a snapshot was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotReason {
    Connected,
    Resync,
}

/// Every player and queue, with the events that arrived while it was fetched already applied.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Snapshot {
    pub players: Vec<Player>,
    pub queues: Vec<PlayerQueue>,
    pub reason: SnapshotReason,
}

impl Snapshot {
    pub fn new(players: Vec<Player>, queues: Vec<PlayerQueue>, reason: SnapshotReason) -> Self {
        Self {
            players,
            queues,
            reason,
        }
    }

    /// Applies a state event, so a consumer can keep a `Snapshot` as its store of players and
    /// queues. Other events change nothing.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::PlayerAdded(player) | Event::PlayerUpdated(player) => {
                upsert(&mut self.players, player, |p| &p.player_id);
            }
            Event::PlayerRemoved { player_id } => {
                self.players.retain(|p| &p.player_id != player_id);
            }
            Event::QueueAdded(queue)
            | Event::QueueUpdated(queue)
            | Event::QueueItemsUpdated(queue) => {
                upsert(&mut self.queues, queue, |q| &q.queue_id);
            }
            Event::QueueTimeUpdated {
                queue_id,
                elapsed_time,
            } => {
                if let Some(queue) = self.queues.iter_mut().find(|q| &q.queue_id == queue_id) {
                    queue.elapsed_time = *elapsed_time;
                }
            }
            _ => {}
        }
    }

    pub fn player(&self, player_id: &str) -> Option<&Player> {
        self.players.iter().find(|p| p.player_id == player_id)
    }

    pub fn queue(&self, queue_id: &str) -> Option<&PlayerQueue> {
        self.queues.iter().find(|q| q.queue_id == queue_id)
    }
}

fn upsert<T: Clone>(items: &mut Vec<T>, item: &T, id: impl Fn(&T) -> &String) {
    match items.iter_mut().find(|existing| id(existing) == id(item)) {
        Some(existing) => existing.clone_from(item),
        None => items.push(item.clone()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn raw(event: &str, object_id: Option<&str>, data: Value) -> RawEvent {
        RawEvent {
            event: event.to_owned(),
            object_id: object_id.map(str::to_owned),
            data,
        }
    }

    #[test]
    fn unknown_events_are_kept_raw() {
        let event = Event::decode(raw("dashboard_show", None, json!({"a": 1}))).expect("decodes");
        assert!(matches!(event, Event::Other(r) if r.event == "dashboard_show"));
    }

    #[test]
    fn a_bad_player_payload_marks_state_as_lost() {
        let err = Event::decode(raw("player_updated", Some("p"), json!("nope"))).expect_err("bad");
        assert!(err.state_lost);
    }

    #[test]
    fn provider_events_split_their_scope() {
        let event = Event::decode(raw("provider_event", Some("spotify--x/login"), Value::Null))
            .expect("ok");
        assert!(matches!(
            event,
            Event::ProviderEvent { instance_id, scope: Some(scope), .. }
                if instance_id == "spotify--x" && scope == "login"
        ));
    }

    #[test]
    fn queue_time_updates_need_a_number() {
        let event = Event::decode(raw("queue_time_updated", Some("q"), json!(12.5))).expect("ok");
        assert_eq!(
            event,
            Event::QueueTimeUpdated {
                queue_id: "q".into(),
                elapsed_time: 12.5
            }
        );
    }
}
