//! Speaker commands (contract 1.5 to 1.7) and the queue read (1.9).
//!
//! A command is checked against the speaker's capabilities, answered with the state it
//! should lead to, and sent to Music Assistant in its own task; the outcome is an event to the
//! sender. Volume and seek are coalesced per speaker so a dragged slider sends the latest
//! value at most every [`COALESCE`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use trackside_core::assistant::{
    Availability, Capability, CatalogItem, CommandAccepted, CommandFailure, GroupRole, LibrarySong,
    MediaInfo, NowPlaying, Origin, PlayItem, PlayMode, PlayRequest, PlayState, Playback,
    PlaybackAction, QueueEntry, QueuePage, Repeat, Speaker, SpeakerCommand, SpeakerError,
    SpeakerEvent, SpeakerGroup, SpeakerPatch, SpeakerQueue,
};
use trackside_core::catalog_ref::CatalogRef;
use trackside_core::{
    Access, Millis, PublicId, ServiceId, SongId, SpeakerCommandId, SpeakerId, random_id,
};
use trackside_mass::{Client, ErrorCode, RequestError};

use crate::{Inner, Pending, lock};

/// Volume and seek are sent at most this often per speaker; the latest value wins.
pub(crate) const COALESCE: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Slot {
    Volume,
    GroupVolume,
    Seek,
}

#[derive(Debug, Default)]
pub(crate) struct SlotState {
    latest: Option<Call>,
    waiting: Vec<SpeakerCommandId>,
    running: bool,
}

/// One Music Assistant command.
#[derive(Clone, Debug)]
pub(crate) struct Call {
    command: &'static str,
    args: Value,
}

fn call(command: &'static str, args: Value) -> Call {
    Call { command, args }
}

/// What Music Assistant knows the speaker by.
#[derive(Clone, Debug)]
struct Target {
    player: String,
    /// The player whose queue plays: the group leader for a member.
    leader: String,
    queue: String,
    speaker: Arc<Speaker>,
    /// A phone that is still joining: Music Assistant does not list this player yet.
    joining: bool,
}

fn unsupported(capability: Capability) -> SpeakerError {
    SpeakerError::Unsupported {
        capability: capability.as_str(),
        speaker: None,
    }
}

fn need(speaker: &Speaker, capability: Capability) -> Result<(), SpeakerError> {
    if speaker.has(capability) {
        Ok(())
    } else {
        Err(unsupported(capability))
    }
}

/// Where playback is at `now`, as clients interpolate it.
fn position_now(speaker: &Speaker, now: Millis) -> u64 {
    let Some(p) = &speaker.playback else { return 0 };
    let mut position = p.position_ms;
    if p.state == PlayState::Playing {
        position += u64::try_from(now.0 - p.position_at.0).unwrap_or(0);
    }
    if let Some(duration) = speaker
        .now_playing
        .as_ref()
        .and_then(|n| n.media.duration_ms)
    {
        position = position.min(duration);
    }
    position
}

fn with_playback(speaker: &mut Speaker, now: Millis, change: impl FnOnce(&mut Playback)) {
    let position = position_now(speaker, now);
    let playback = speaker.playback.get_or_insert(Playback {
        state: PlayState::Idle,
        position_ms: 0,
        position_at: now,
        shuffle: false,
        repeat: Repeat::Off,
    });
    playback.position_ms = position;
    playback.position_at = now;
    change(playback);
}

/// How a failed Music Assistant command is reported.
fn failure(error: &RequestError) -> CommandFailure {
    match error {
        RequestError::NotConnected | RequestError::ConnectionLost | RequestError::ShutDown => {
            CommandFailure {
                code: "speakers_unavailable",
                message: String::from("Speakers aren't reachable right now."),
            }
        }
        RequestError::Timeout(_) => CommandFailure {
            code: "timed_out",
            message: String::from("The speaker did not answer in time."),
        },
        RequestError::Server(e) if e.code == ErrorCode::PlayerUnavailable => CommandFailure {
            code: "conflict",
            message: String::from("The speaker is unavailable."),
        },
        _ => CommandFailure {
            code: "command_failed",
            message: String::from("The speaker did not accept the command."),
        },
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Cursor {
    o: u32,
    v: u64,
}

impl Inner {
    /// What Music Assistant knows the speaker by. Another user's relay speaker does not exist
    /// for the caller; their own whose session ended is `RelayEnded`.
    fn target(&self, caller: &Access, id: &SpeakerId) -> Result<Target, SpeakerError> {
        let owner = self.relay.owner_of(id);
        if owner.is_some_and(|u| u != caller.user) {
            return Err(SpeakerError::NotFound);
        }
        let hub = lock(&self.hub);
        let Some(speaker) = hub.speaker(id) else {
            drop(hub);
            return match (self.relay.live(id), owner) {
                (Some(live), _) => Ok(Target {
                    speaker: Arc::new(crate::convert::relay_placeholder(
                        id.clone(),
                        &live.name,
                        &live.tag,
                    )),
                    leader: live.player.clone(),
                    queue: live.player.clone(),
                    player: live.player,
                    joining: true,
                }),
                (None, Some(_)) => Err(SpeakerError::RelayEnded),
                (None, None) => Err(SpeakerError::NotFound),
            };
        };
        let player = hub.player_of(id).ok_or(SpeakerError::NotFound)?;
        let view = hub.view().ok_or(SpeakerError::Unavailable)?;
        let leader = view.leader_of(player);
        let queue = view
            .queue_of(leader)
            .map_or_else(|| leader.player_id.clone(), |q| q.queue_id.clone());
        Ok(Target {
            player: player.player_id.clone(),
            leader: leader.player_id.clone(),
            queue,
            speaker,
            joining: false,
        })
    }

    pub(crate) async fn queue(
        &self,
        caller: &Access,
        id: &SpeakerId,
        page: QueuePage,
    ) -> Result<SpeakerQueue, SpeakerError> {
        let client = self.connected()?;
        let target = self.target(caller, id)?;
        need(&target.speaker, Capability::Queue)?;
        let (total, current, shuffle, repeat, version) = {
            let hub = lock(&self.hub);
            let queue = hub.queues.get(&target.queue);
            (
                queue.map_or(0, |q| q.items),
                queue.and_then(|q| q.current_index),
                queue.is_some_and(|q| q.shuffle_enabled),
                match queue.map(|q| &q.repeat_mode) {
                    Some(trackside_mass::models::RepeatMode::One) => Repeat::One,
                    Some(trackside_mass::models::RepeatMode::All) => Repeat::All,
                    _ => Repeat::Off,
                },
                hub.queue_items.get(&target.queue).copied().unwrap_or(0),
            )
        };
        let limit = page.limit.clamp(1, 500);
        let offset = match &page.cursor {
            Some(cursor) => {
                let bad = || SpeakerError::Invalid {
                    field: "cursor",
                    message: String::from("The cursor is not valid; start again."),
                };
                let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| bad())?;
                let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| bad())?;
                if cursor.v != version {
                    return Err(bad());
                }
                cursor.o
            }
            None => current.unwrap_or(0).saturating_sub(page.history.min(50)),
        };
        let items = client
            .queue_items(&target.queue, limit, offset)
            .await
            .map_err(|e| request_error(&e))?;
        let mut art = Vec::new();
        let entries = {
            let hub = lock(&self.hub);
            let view = hub.view().ok_or(SpeakerError::Unavailable)?;
            items
                .iter()
                .enumerate()
                .map(|(i, item)| QueueEntry {
                    index: offset + u32::try_from(i).unwrap_or(0),
                    item_id: item.queue_item_id.clone(),
                    media: view.media(item, &mut art),
                })
                .collect::<Vec<_>>()
        };
        lock(&self.hub).art.extend(art);
        let end = offset + u32::try_from(entries.len()).unwrap_or(0);
        let next_cursor = (end < total && !entries.is_empty()).then(|| {
            URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&Cursor { o: end, v: version }).unwrap_or_default())
        });
        Ok(SpeakerQueue {
            id: id.clone(),
            current_index: current,
            items: entries,
            next_cursor,
            repeat,
            shuffle,
            revision: target.speaker.revision,
            total,
        })
    }

    #[expect(clippy::too_many_lines, reason = "one arm per command")]
    pub(crate) async fn command(
        self: &Arc<Self>,
        caller: &Access,
        id: &SpeakerId,
        command: SpeakerCommand,
    ) -> Result<CommandAccepted, SpeakerError> {
        let client = self.connected()?;
        let target = self.target(caller, id)?;
        let mut joining: Vec<String> = Vec::new();
        if target.joining {
            joining.push(target.player.clone());
        }
        let now = self.now();
        let speaker = &target.speaker;
        if speaker.availability == Availability::Unavailable {
            return Err(SpeakerError::Offline);
        }
        let action = command.action();
        let settle = command.settle();
        let mut expected = (**speaker).clone();
        let (calls, slot, shown): (Vec<Call>, Option<Slot>, Arc<Speaker>) = match command {
            SpeakerCommand::Playback(action) => {
                let (needed, name) = match action {
                    PlaybackAction::Play => (Capability::Pause, "players/cmd/play"),
                    PlaybackAction::Pause => (Capability::Pause, "players/cmd/pause"),
                    PlaybackAction::Toggle => (Capability::Pause, "players/cmd/play_pause"),
                    PlaybackAction::Stop => (Capability::Stop, "players/cmd/stop"),
                    PlaybackAction::Next => (Capability::NextPrevious, "players/cmd/next"),
                    PlaybackAction::Previous => (Capability::NextPrevious, "players/cmd/previous"),
                };
                need(speaker, needed)?;
                if expected.playback.is_some() {
                    with_playback(&mut expected, now, |p| {
                        p.state = match action {
                            PlaybackAction::Pause => PlayState::Paused,
                            PlaybackAction::Toggle if p.state == PlayState::Playing => {
                                PlayState::Paused
                            }
                            PlaybackAction::Play | PlaybackAction::Toggle => PlayState::Playing,
                            PlaybackAction::Stop => PlayState::Idle,
                            PlaybackAction::Next | PlaybackAction::Previous => {
                                p.position_ms = 0;
                                p.state
                            }
                        };
                    });
                }
                (
                    vec![call(name, json!({"player_id": target.leader}))],
                    None,
                    Arc::new(expected),
                )
            }
            SpeakerCommand::Seek { position_ms } => {
                need(speaker, Capability::Seek)?;
                if speaker.playback.is_none() {
                    return Err(unsupported(Capability::Seek));
                }
                if let Some(duration) = speaker
                    .now_playing
                    .as_ref()
                    .and_then(|n| n.media.duration_ms)
                    && position_ms > duration
                {
                    return Err(SpeakerError::Invalid {
                        field: "position_ms",
                        message: String::from("position_ms is past the end of the item."),
                    });
                }
                with_playback(&mut expected, now, |p| p.position_ms = position_ms);
                (
                    vec![call(
                        "players/cmd/seek",
                        json!({"player_id": target.leader, "position": position_ms / 1000}),
                    )],
                    Some(Slot::Seek),
                    Arc::new(expected),
                )
            }
            SpeakerCommand::Jump { item_id } => {
                need(speaker, Capability::Queue)?;
                if item_id.is_empty() || item_id.len() > 200 {
                    return Err(SpeakerError::Invalid {
                        field: "item_id",
                        message: String::from("item_id is not a queue item."),
                    });
                }
                with_playback(&mut expected, now, |p| {
                    p.position_ms = 0;
                    p.state = PlayState::Playing;
                });
                (
                    vec![call(
                        "player_queues/play_index",
                        json!({"queue_id": target.queue, "index": item_id}),
                    )],
                    None,
                    Arc::new(expected),
                )
            }
            SpeakerCommand::Set(patch) => {
                let (calls, slot) = Self::patch_calls(&target, &patch, &mut expected, now)?;
                (calls, slot, Arc::new(expected))
            }
            SpeakerCommand::Members(members) => {
                let calls = members_calls(&target, &members, &mut expected)?;
                (calls, None, Arc::new(expected))
            }
            SpeakerCommand::LeaveGroup => {
                need(speaker, Capability::Group)?;
                expected.group = None;
                expected.group_volume = None;
                (
                    vec![call(
                        "players/cmd/ungroup",
                        json!({"player_id": target.player}),
                    )],
                    None,
                    Arc::new(expected),
                )
            }
            SpeakerCommand::Play(request) => {
                need(speaker, Capability::Play)?;
                let calls = self
                    .play_calls(&client, caller, &target, request, &mut expected, now)
                    .await?;
                (calls, None, Arc::new(expected))
            }
            SpeakerCommand::Transfer {
                to,
                keep_playing_here,
            } => {
                need(speaker, Capability::Transfer)?;
                let destination = self.target(caller, &to)?;
                if destination.joining {
                    joining.push(destination.player.clone());
                }
                if destination.speaker.availability == Availability::Unavailable {
                    return Err(SpeakerError::Offline);
                }
                if !destination.speaker.has(Capability::Play) {
                    return Err(SpeakerError::Unsupported {
                        capability: Capability::Play.as_str(),
                        speaker: Some(to),
                    });
                }
                if destination.queue == target.queue {
                    return Err(SpeakerError::Invalid {
                        field: "to",
                        message: String::from("to is the speaker that plays already."),
                    });
                }
                let mut moved = (*destination.speaker).clone();
                moved.playback.clone_from(&speaker.playback);
                if let Some(p) = moved.playback.as_mut() {
                    p.position_ms = position_now(speaker, now);
                    p.position_at = now;
                }
                moved.now_playing.clone_from(&speaker.now_playing);
                moved.context.clone_from(&speaker.context);
                let mut calls = vec![call(
                    "player_queues/transfer",
                    json!({"source_queue_id": target.queue, "target_queue_id": destination.queue}),
                )];
                if keep_playing_here {
                    calls.push(call(
                        "player_queues/play",
                        json!({"queue_id": target.queue}),
                    ));
                }
                if let Some(context) = speaker.context.clone() {
                    lock(&self.hub)
                        .contexts
                        .insert(destination.leader.clone(), context);
                }
                (calls, None, Arc::new(moved))
            }
        };
        let command_id: SpeakerCommandId = random_id();
        lock(&self.pending).insert(
            command_id.clone(),
            Pending {
                user: caller.user,
                speaker: shown.id.clone(),
            },
        );
        match slot {
            Some(slot) if joining.is_empty() => {
                self.coalesce(&client, id, slot, calls, command_id.clone());
            }
            _ => {
                let me = Arc::clone(self);
                let command = command_id.clone();
                let timeout = settle.min(self.tuning.command_timeout.max(Duration::from_secs(1)));
                let player_wait = self.relay.tuning().player_wait;
                self.tasks.spawn(async move {
                    let deadline = tokio::time::Instant::now() + player_wait;
                    for player in &joining {
                        if !me.wait_for_player(player, deadline).await {
                            me.finish(
                                &command,
                                Err(CommandFailure {
                                    code: "timed_out",
                                    message: String::from("The phone did not join in time."),
                                }),
                            );
                            return;
                        }
                    }
                    let outcome = run(&client, &calls, timeout).await;
                    me.finish(&command, outcome);
                });
            }
        }
        Ok(CommandAccepted {
            action,
            command_id,
            settle_by: Millis(now.0 + i64::try_from(settle.as_millis()).unwrap_or(0)),
            speaker: shown,
        })
    }

    fn patch_calls(
        target: &Target,
        patch: &SpeakerPatch,
        expected: &mut Speaker,
        now: Millis,
    ) -> Result<(Vec<Call>, Option<Slot>), SpeakerError> {
        let speaker = &target.speaker;
        if patch.is_empty() {
            return Err(SpeakerError::Invalid {
                field: "body",
                message: String::from("Nothing to change."),
            });
        }
        if patch.volume.is_some() {
            need(speaker, Capability::Volume)?;
        }
        if patch.muted.is_some() {
            need(speaker, Capability::Mute)?;
        }
        if patch.powered.is_some() {
            need(speaker, Capability::Power)?;
        }
        if patch.shuffle.is_some() {
            need(speaker, Capability::Shuffle)?;
        }
        if patch.repeat.is_some() {
            need(speaker, Capability::Repeat)?;
        }
        if patch.group_volume.is_some() {
            need(speaker, Capability::Volume)?;
            if speaker
                .group
                .as_ref()
                .is_none_or(|g| g.role != GroupRole::Leader)
            {
                return Err(unsupported(Capability::Group));
            }
        }
        let mut calls = Vec::new();
        if let Some(powered) = patch.powered {
            calls.push(call(
                "players/cmd/power",
                json!({"player_id": target.player, "powered": powered}),
            ));
            expected.powered = Some(powered);
            if !powered {
                expected.availability = Availability::PoweredOff;
                if expected.playback.is_some() {
                    with_playback(expected, now, |p| p.state = PlayState::Idle);
                }
            }
        }
        if let Some(volume) = patch.volume {
            calls.push(call(
                "players/cmd/volume_set",
                json!({"player_id": target.player, "volume_level": volume}),
            ));
            expected.volume = Some(volume);
        }
        if let Some(volume) = patch.group_volume {
            calls.push(call(
                "players/cmd/group_volume",
                json!({"player_id": target.player, "volume_level": volume}),
            ));
            expected.group_volume = Some(volume);
        }
        if let Some(muted) = patch.muted {
            calls.push(call(
                "players/cmd/volume_mute",
                json!({"player_id": target.player, "muted": muted}),
            ));
            expected.muted = Some(muted);
        }
        if let Some(shuffle) = patch.shuffle {
            calls.push(call(
                "player_queues/shuffle",
                json!({"queue_id": target.queue, "shuffle_enabled": shuffle}),
            ));
            if let Some(p) = expected.playback.as_mut() {
                p.shuffle = shuffle;
            }
        }
        if let Some(repeat) = patch.repeat {
            calls.push(call(
                "player_queues/repeat",
                json!({"queue_id": target.queue, "repeat_mode": repeat.as_str()}),
            ));
            if let Some(p) = expected.playback.as_mut() {
                p.repeat = repeat;
            }
        }
        let only = |field: bool| field && calls.len() == 1;
        let slot = if only(patch.volume.is_some()) {
            Some(Slot::Volume)
        } else if only(patch.group_volume.is_some()) {
            Some(Slot::GroupVolume)
        } else {
            None
        };
        Ok((calls, slot))
    }

    #[expect(clippy::too_many_lines, reason = "one request, checked in order")]
    async fn play_calls(
        &self,
        client: &Client,
        caller: &Access,
        target: &Target,
        request: PlayRequest,
        expected: &mut Speaker,
        now: Millis,
    ) -> Result<Vec<Call>, SpeakerError> {
        if request.mode != PlayMode::Replace {
            need(&target.speaker, Capability::Queue)?;
        }
        let (ours, providers, server_id) = {
            let hub = lock(&self.hub);
            (
                hub.ours.clone(),
                hub.providers.clone(),
                hub.server_id.clone().unwrap_or_default(),
            )
        };
        let hidden = self.effective().hidden_services;
        let ours_instance = ours.as_deref();
        let song_ids: Vec<SongId> = request
            .items
            .iter()
            .filter_map(|item| match item {
                PlayItem::Song(id) => Some(id.clone()),
                PlayItem::Ref(_) => None,
            })
            .collect();
        if let (Some(first), None) = (song_ids.first(), &ours) {
            return Err(SpeakerError::NotPlayable {
                song: Some(first.clone()),
                reference: None,
            });
        }
        let access = caller.clone();
        let lookup = song_ids.clone();
        let (songs, placement) = self
            .db
            .read(move |c| {
                let songs = trackside_core::repo::songs::get_many(c, &access, &lookup)?;
                let placement = trackside_core::repo::songs::placement(c, &lookup)?;
                Ok((songs, placement))
            })
            .await
            .map_err(|e| SpeakerError::Failed(e.to_string()))?;
        let visible: std::collections::HashMap<SongId, &trackside_core::records::Song> =
            songs.iter().map(|s| (s.id.clone(), s)).collect();
        let mut uris = Vec::new();
        let mut kinds = Vec::new();
        let mut originals = Vec::new();
        let mut first_failure = None;
        let mut signed_out = None;
        for (index, item) in request.items.iter().enumerate() {
            match item {
                PlayItem::Song(id) => {
                    if visible.contains_key(id)
                        && let Some(ours) = &ours
                    {
                        uris.push(format!("{ours}://track/{id}"));
                        kinds.push(true);
                        originals.push(index);
                    } else if first_failure.is_none() {
                        first_failure = Some((Some(id.clone()), None));
                    }
                }
                PlayItem::Ref(text) => {
                    let decoded = CatalogRef::decode(text).map_err(|e| SpeakerError::Invalid {
                        field: "items",
                        message: format!("items[{index}].ref: {e}."),
                    })?;
                    if !request.allow_catalog {
                        first_failure.get_or_insert((None, Some(text.clone())));
                        continue;
                    }
                    let instance =
                        crate::catalog::resolve(&decoded, &providers, ours_instance, &hidden);
                    match instance {
                        Some(provider) => {
                            uris.push(decoded.uri_on(&provider.instance_id));
                            kinds.push(decoded.kind == trackside_core::catalog_ref::RefKind::Track);
                            originals.push(index);
                        }
                        None => {
                            let service = decoded
                                .instance_id
                                .as_deref()
                                .map(|i| ServiceId::derive(&server_id, i));
                            signed_out.get_or_insert(service);
                            first_failure.get_or_insert((None, Some(text.clone())));
                        }
                    }
                }
            }
        }
        if uris.is_empty() {
            if let Some(service) = signed_out {
                return Err(SpeakerError::ServiceSignedOut { service });
            }
            let (song, reference) = first_failure.unwrap_or((None, None));
            return Err(SpeakerError::NotPlayable { song, reference });
        }
        let start = originals
            .iter()
            .position(|&i| i >= request.start_index)
            .unwrap_or(0);
        let mut calls = Vec::new();
        let option = request.mode.as_str();
        if request.mode == PlayMode::Replace {
            if let Some(shuffle) = request.shuffle {
                calls.push(call(
                    "player_queues/shuffle",
                    json!({"queue_id": target.queue, "shuffle_enabled": shuffle}),
                ));
            }
            if let Some(repeat) = request.repeat {
                calls.push(call(
                    "player_queues/repeat",
                    json!({"queue_id": target.queue, "repeat_mode": repeat.as_str()}),
                ));
            }
        }
        calls.push(call(
            "player_queues/play_media",
            json!({"queue_id": target.queue, "media": uris, "option": option, "radio_mode": false}),
        ));
        if request.mode == PlayMode::Replace {
            // Albums and playlists expand in place, so an index past one is not known before
            // Music Assistant expands it; a start after one starts at the first item.
            let indexable =
                kinds[..start].iter().all(|track| *track) && request.shuffle != Some(true);
            let position_s = request.position_ms / 1000;
            if start > 0 && indexable {
                calls.push(call(
                    "player_queues/play_index",
                    json!({"queue_id": target.queue, "index": start, "seek_position": position_s}),
                ));
            } else if position_s > 0 && (start == 0 || indexable) {
                calls.push(call(
                    "players/cmd/seek",
                    json!({"player_id": target.leader, "position": position_s}),
                ));
            }
            let first = &request.items[originals[start]];
            let media = match first {
                PlayItem::Song(id) => visible.get(id).map(|song| MediaInfo {
                    title: song.title.clone(),
                    artist: Some(song.artist.clone()),
                    album: song.album.clone(),
                    duration_ms: Some(u64::from(song.duration_ms)),
                    song: placement.get(id).map(|(library, artwork)| LibrarySong {
                        id: id.clone(),
                        library: *library,
                        artwork: artwork.clone(),
                    }),
                    catalog: None,
                    song_key: Some(song.song_key.clone()),
                }),
                PlayItem::Ref(text) => {
                    catalog_media(client, &uris[start], text, &server_id, &providers).await
                }
            };
            if let Some(media) = media {
                expected.now_playing = Some(NowPlaying {
                    media,
                    item_id: None,
                    origin: Origin::Queue,
                });
            }
            expected.availability = Availability::Available;
            if expected.capabilities.contains(&Capability::Power) {
                expected.powered = Some(true);
            }
            expected.playback = Some(Playback {
                state: PlayState::Playing,
                position_ms: request.position_ms,
                position_at: now,
                shuffle: request
                    .shuffle
                    .unwrap_or_else(|| target.speaker.playback.as_ref().is_some_and(|p| p.shuffle)),
                repeat: request.repeat.unwrap_or_else(|| {
                    target
                        .speaker
                        .playback
                        .as_ref()
                        .map_or(Repeat::Off, |p| p.repeat)
                }),
            });
            expected.context.clone_from(&request.context);
            let mut hub = lock(&self.hub);
            match &request.context {
                Some(context) => {
                    hub.contexts.insert(target.leader.clone(), context.clone());
                }
                None => {
                    hub.contexts.remove(&target.leader);
                }
            }
        }
        Ok(calls)
    }

    fn coalesce(
        self: &Arc<Self>,
        client: &Client,
        id: &SpeakerId,
        slot: Slot,
        calls: Vec<Call>,
        command: SpeakerCommandId,
    ) {
        let Some(latest) = calls.into_iter().next() else {
            return;
        };
        let start = {
            let mut slots = lock(&self.slots);
            let state = slots.entry((id.clone(), slot)).or_default();
            state.latest = Some(latest);
            state.waiting.push(command);
            !std::mem::replace(&mut state.running, true)
        };
        if !start {
            return;
        }
        let me = Arc::clone(self);
        let client = client.clone();
        let key = (id.clone(), slot);
        let timeout = self.tuning.command_timeout;
        self.tasks.spawn(async move {
            loop {
                let (call, waiting) = {
                    let mut slots = lock(&me.slots);
                    let Some(state) = slots.get_mut(&key) else {
                        return;
                    };
                    match state.latest.take() {
                        Some(call) => (call, std::mem::take(&mut state.waiting)),
                        None => {
                            slots.remove(&key);
                            return;
                        }
                    }
                };
                let outcome = run(&client, std::slice::from_ref(&call), timeout).await;
                for command in &waiting {
                    me.finish(command, outcome.clone());
                }
                tokio::time::sleep(COALESCE).await;
            }
        });
    }

    /// Announces a command's outcome, once.
    pub(crate) fn finish(&self, command: &SpeakerCommandId, outcome: Result<(), CommandFailure>) {
        let Some(pending) = lock(&self.pending).remove(command) else {
            return;
        };
        if let Err(failure) = &outcome {
            tracing::info!(code = failure.code, "a speaker command failed");
        }
        self.events.publish(trackside_core::ServerEvent::Speaker(
            SpeakerEvent::Command {
                user: pending.user,
                command: command.clone(),
                speaker: pending.speaker,
                outcome,
            },
        ));
    }
}

fn members_calls(
    target: &Target,
    members: &[SpeakerId],
    expected: &mut Speaker,
) -> Result<Vec<Call>, SpeakerError> {
    let speaker = &target.speaker;
    need(speaker, Capability::Group)?;
    if speaker.group.as_ref().is_some_and(|g| g.fixed) {
        return Err(unsupported(Capability::Group));
    }
    let mut wanted = Vec::new();
    let mut seen = HashSet::new();
    for member in members {
        if member == &speaker.id || !seen.insert(member.clone()) {
            continue;
        }
        if !speaker.can_group_with.contains(member) {
            return Err(SpeakerError::Unsupported {
                capability: Capability::Group.as_str(),
                speaker: Some(member.clone()),
            });
        }
        wanted.push(member.clone());
    }
    let current: Vec<SpeakerId> = speaker
        .group
        .as_ref()
        .filter(|g| g.role == GroupRole::Leader)
        .map(|g| {
            g.member_ids
                .iter()
                .filter(|m| **m != speaker.id)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    expected.group = (!wanted.is_empty()).then(|| SpeakerGroup {
        leader_id: speaker.id.clone(),
        member_ids: std::iter::once(speaker.id.clone())
            .chain(wanted.iter().cloned())
            .collect(),
        role: GroupRole::Leader,
        fixed: false,
    });
    Ok(vec![call(
        "players/cmd/set_members",
        json!({
            "target_player": target.player,
            "player_ids_to_add": wanted.iter().filter(|w| !current.contains(w)).map(PublicId::as_str).collect::<Vec<_>>(),
            "player_ids_to_remove": current.iter().filter(|c| !wanted.contains(c)).map(PublicId::as_str).collect::<Vec<_>>(),
        }),
    )])
}

/// What a catalog item will show, from Music Assistant, if it answers quickly.
async fn catalog_media(
    client: &Client,
    uri: &str,
    reference: &str,
    server_id: &str,
    providers: &[trackside_mass::models::ProviderInstance],
) -> Option<MediaInfo> {
    let item = tokio::time::timeout(Duration::from_secs(2), client.item_by_uri(uri))
        .await
        .ok()?
        .ok()?;
    let instance = uri.split("://").next().unwrap_or_default();
    let artist = Some(item.artist_names()).filter(|a| !a.is_empty());
    let _ = providers;
    Some(MediaInfo {
        song_key: artist
            .as_deref()
            .map(|a| tracks_model::song_key(&item.name, a)),
        title: item.name.clone(),
        artist,
        album: item
            .album
            .as_deref()
            .map(|a| a.name.clone())
            .filter(|a| !a.is_empty()),
        duration_ms: item
            .duration
            .filter(|d| d.is_finite() && *d > 0.0)
            .map(|d| (d * 1000.0) as u64),
        song: None,
        catalog: Some(CatalogItem {
            reference: reference.to_owned(),
            service: ServiceId::derive(server_id, instance),
            artwork: None,
        }),
    })
}

/// Sends the calls in order; the first failure ends it.
async fn run(client: &Client, calls: &[Call], timeout: Duration) -> Result<(), CommandFailure> {
    for call in calls {
        client
            .call_raw(call.command, Some(call.args.clone()), timeout)
            .await
            .map_err(|e| {
                tracing::debug!(command = call.command, error = %e, "Music Assistant refused a command");
                failure(&e)
            })?;
    }
    Ok(())
}

fn request_error(error: &RequestError) -> SpeakerError {
    match error {
        RequestError::NotConnected
        | RequestError::ConnectionLost
        | RequestError::Timeout(_)
        | RequestError::Busy
        | RequestError::ShutDown => SpeakerError::Unavailable,
        RequestError::Server(e) if e.code == ErrorCode::PlayerUnavailable => SpeakerError::Offline,
        other => SpeakerError::Failed(other.to_string()),
    }
}
