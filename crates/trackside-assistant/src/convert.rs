//! Music Assistant players and queues as Trackside speakers (contract 1.1 to 1.4).

use std::collections::{BTreeSet, HashMap};

use trackside_core::assistant::{
    Availability, Capability, CatalogItem, GroupRole, LibrarySong, MediaInfo, NowPlaying, Origin,
    PlayContext, PlayState, Playback, Repeat, Speaker, SpeakerGroup, SpeakerKind,
};
use trackside_core::catalog_ref::{CatalogRef, RefKind};
use trackside_core::{
    ArtworkId, CatalogArtId, LibraryKey, Millis, PublicId, ServiceId, SongId, SpeakerId,
};
use trackside_mass::models::{
    MediaItem, MediaType, PlaybackState, Player, PlayerFeature, PlayerQueue, PlayerType,
    ProviderInstance, QueueItem, RepeatMode,
};
use trackside_mass::{Endpoint, ImageSize, image_url};

/// Where a song lives, from [`trackside_core::repo::songs::placement`].
pub(crate) type Placement = (LibraryKey, Option<ArtworkId>);

/// Everything a conversion reads.
pub(crate) struct View<'a> {
    pub server_id: &'a str,
    pub players: &'a HashMap<String, Player>,
    pub queues: &'a HashMap<String, PlayerQueue>,
    pub providers: &'a [ProviderInstance],
    /// Trackside's own `OpenSubsonic` instance.
    pub ours: Option<&'a str>,
    pub songs: &'a HashMap<SongId, Placement>,
    pub contexts: &'a HashMap<String, PlayContext>,
    pub endpoint: Option<&'a Endpoint>,
}

/// Catalog art found while converting: the `m` id and the address it names.
pub(crate) type ArtSeen = Vec<(CatalogArtId, String)>;

pub(crate) fn speaker_id(server_id: &str, player_id: &str) -> SpeakerId {
    SpeakerId::derive(server_id, player_id)
}

/// Players Trackside never shows: disabled, hidden in Music Assistant, or protocol-only.
pub(crate) fn visible(player: &Player) -> bool {
    player.enabled && !player.hide_in_ui && player.player_type != PlayerType::Protocol
}

fn protocol(domain: &str) -> &'static str {
    match domain {
        "airplay" => "airplay",
        "chromecast" | "cast" => "cast",
        "sonos" | "sonos_s1" => "sonos",
        "dlna" => "dlna",
        "snapcast" => "snapcast",
        "squeezelite" | "slimproto" => "squeezelite",
        "sendspin" => "sendspin",
        "hass_players" | "hass" => "home_assistant",
        _ => "other",
    }
}

impl View<'_> {
    fn provider(&self, instance_id: &str) -> Option<&ProviderInstance> {
        self.providers.iter().find(|p| p.instance_id == instance_id)
    }

    fn domain_of<'b>(&'b self, instance_id: &'b str) -> &'b str {
        self.provider(instance_id).map_or_else(
            || instance_id.split("--").next().unwrap_or(instance_id),
            |p| p.domain.as_str(),
        )
    }

    fn streaming(&self, instance_id: &str) -> Option<&ProviderInstance> {
        self.provider(instance_id)
            .filter(|p| p.is_streaming_provider == Some(true))
    }

    /// The queue a player plays from: its active source when that is a queue, else its own.
    pub fn queue_of(&self, player: &Player) -> Option<&PlayerQueue> {
        player
            .active_source
            .as_deref()
            .and_then(|source| self.queues.get(source))
            .or_else(|| self.queues.get(&player.player_id))
    }

    /// The player that leads this one's group, or itself.
    pub fn leader_of<'p>(&'p self, player: &'p Player) -> &'p Player {
        player
            .synced_to
            .as_deref()
            .and_then(|leader| self.players.get(leader))
            .unwrap_or(player)
    }

    /// Whether what plays came from the player's Music Assistant queue.
    fn origin(&self, player: &Player) -> Origin {
        let source = player
            .current_media
            .as_ref()
            .and_then(|m| m.source_id.as_deref())
            .or(player.active_source.as_deref());
        match source {
            Some(source) if !self.queues.contains_key(source) && source != player.player_id => {
                Origin::Other
            }
            _ => Origin::Queue,
        }
    }

    /// The Trackside song a queue item plays from Trackside's own registration.
    pub fn our_song(&self, item: &QueueItem) -> Option<SongId> {
        let ours = self.ours?;
        let media = item.media_item.as_ref();
        let id = item
            .streamdetails
            .as_ref()
            .filter(|s| s.provider == ours)
            .map(|s| s.item_id.as_str())
            .or_else(|| {
                media.and_then(|m| {
                    m.provider_mappings
                        .iter()
                        .find(|p| p.provider_instance == ours)
                        .map(|p| p.item_id.as_str())
                })
            })
            .or_else(|| {
                media
                    .filter(|m| m.provider == ours)
                    .map(|m| m.item_id.as_str())
            })?;
        SongId::parse(id).ok()
    }

    fn catalog(&self, item: &QueueItem, art: &mut ArtSeen) -> Option<CatalogItem> {
        let media = item.media_item.as_ref()?;
        let (instance, item_id) = if self.streaming(&media.provider).is_some() {
            (media.provider.as_str(), media.item_id.as_str())
        } else if let Some(mapping) = media
            .provider_mappings
            .iter()
            .find(|m| self.streaming(&m.provider_instance).is_some())
        {
            (mapping.provider_instance.as_str(), mapping.item_id.as_str())
        } else {
            let stream = item
                .streamdetails
                .as_ref()
                .filter(|s| self.streaming(&s.provider).is_some())?;
            (stream.provider.as_str(), stream.item_id.as_str())
        };
        let kind = match media.media_type {
            MediaType::Radio => RefKind::Radio,
            _ => RefKind::Track,
        };
        let domain = self.domain_of(instance);
        let reference = CatalogRef::new(kind, domain, item_id, Some(instance)).encode()?;
        let artwork = item
            .image
            .as_ref()
            .or_else(|| media.thumb())
            .and_then(|image| {
                self.endpoint
                    .and_then(|e| image_url(e, image, ImageSize::Original, None))
            })
            .map(|url| {
                let id = CatalogArtId::derive(&url);
                art.push((id.clone(), url));
                id
            });
        Some(CatalogItem {
            reference,
            service: ServiceId::derive(self.server_id, instance),
            artwork,
        })
    }

    /// One queue item as Trackside shows it.
    pub fn media(&self, item: &QueueItem, art: &mut ArtSeen) -> MediaInfo {
        let media = item.media_item.as_ref();
        let title = media
            .map(|m| m.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| item.name.clone());
        let artist = media.map(MediaItem::artist_names).filter(|a| !a.is_empty());
        let album = media
            .and_then(|m| m.album.as_deref())
            .map(|a| a.name.clone())
            .filter(|a| !a.is_empty());
        let duration_ms = item
            .duration
            .or_else(|| media.and_then(|m| m.duration))
            .filter(|d| d.is_finite() && *d > 0.0)
            .map(|d| (d * 1000.0).round() as u64);
        let song = self.our_song(item).and_then(|id| {
            self.songs.get(&id).map(|(library, artwork)| LibrarySong {
                id: id.clone(),
                library: *library,
                artwork: artwork.clone(),
            })
        });
        let catalog = if song.is_some() {
            None
        } else {
            self.catalog(item, art)
        };
        let song_key = artist
            .as_deref()
            .filter(|_| !title.is_empty())
            .map(|artist| tracks_model::song_key(&title, artist));
        MediaInfo {
            title,
            artist,
            album,
            duration_ms,
            song,
            catalog,
            song_key,
        }
    }

    fn now_playing(
        &self,
        player: &Player,
        origin: Origin,
        art: &mut ArtSeen,
    ) -> Option<NowPlaying> {
        if origin == Origin::Queue
            && let Some(item) = self.queue_of(player).and_then(|q| q.current_item.as_ref())
        {
            return Some(NowPlaying {
                media: self.media(item, art),
                item_id: Some(item.queue_item_id.clone()),
                origin,
            });
        }
        let current = player.current_media.as_ref()?;
        let title = current.title.clone().filter(|t| !t.is_empty())?;
        let artist = current.artist.clone().filter(|a| !a.is_empty());
        Some(NowPlaying {
            media: MediaInfo {
                song_key: artist.as_deref().map(|a| tracks_model::song_key(&title, a)),
                title,
                artist,
                album: current.album.clone().filter(|a| !a.is_empty()),
                duration_ms: current
                    .duration
                    .filter(|d| d.is_finite() && *d > 0.0)
                    .map(|d| (d * 1000.0).round() as u64),
                song: None,
                catalog: None,
            },
            item_id: (origin == Origin::Queue)
                .then(|| current.queue_item_id.clone())
                .flatten(),
            origin,
        })
    }

    fn playback(&self, player: &Player, origin: Origin) -> Option<Playback> {
        let queue = (origin == Origin::Queue)
            .then(|| self.queue_of(player))
            .flatten();
        let loaded =
            queue.is_some_and(|q| q.current_item.is_some()) || player.current_media.is_some();
        if !loaded {
            return None;
        }
        let state = queue.map_or_else(|| player.state(), |q| q.state.clone());
        let (elapsed, at) = match queue {
            Some(q) if q.current_item.is_some() => {
                (Some(q.elapsed_time), q.elapsed_time_last_updated)
            }
            _ => (player.elapsed_time, player.elapsed_time_last_updated),
        };
        let position_ms = elapsed
            .filter(|e| e.is_finite() && *e >= 0.0)
            .map_or(0, |e| (e * 1000.0).round() as u64);
        let position_at = at
            .filter(|t| t.is_finite() && *t > 0.0)
            .map_or(Millis(0), |t| Millis((t * 1000.0).round() as i64));
        Some(Playback {
            state: match state {
                PlaybackState::Playing => PlayState::Playing,
                PlaybackState::Paused => PlayState::Paused,
                PlaybackState::Idle => PlayState::Idle,
                _ => PlayState::Unknown,
            },
            position_ms,
            position_at,
            shuffle: queue.is_some_and(|q| q.shuffle_enabled),
            repeat: match queue.map(|q| &q.repeat_mode) {
                Some(RepeatMode::One) => Repeat::One,
                Some(RepeatMode::All) => Repeat::All,
                _ => Repeat::Off,
            },
        })
    }

    fn can_group_with(&self, player: &Player) -> Vec<SpeakerId> {
        let mut out = BTreeSet::new();
        for entry in &player.can_group_with {
            if let Some(other) = self.players.get(entry) {
                if other.player_id != player.player_id && visible(other) {
                    out.insert(speaker_id(self.server_id, &other.player_id));
                }
                continue;
            }
            for other in self.players.values() {
                if other.provider == *entry && other.player_id != player.player_id && visible(other)
                {
                    out.insert(speaker_id(self.server_id, &other.player_id));
                }
            }
        }
        out.into_iter().collect()
    }

    fn group(&self, player: &Player, kind: SpeakerKind) -> Option<SpeakerGroup> {
        let ids = |members: &[String]| {
            members
                .iter()
                .map(|m| speaker_id(self.server_id, m))
                .collect::<Vec<_>>()
        };
        if let Some(leader) = player.synced_to.as_deref() {
            let members = self
                .players
                .get(leader)
                .map(|l| l.group_members.clone())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| vec![leader.to_owned(), player.player_id.clone()]);
            return Some(SpeakerGroup {
                leader_id: speaker_id(self.server_id, leader),
                member_ids: ids(&members),
                role: GroupRole::Member,
                fixed: false,
            });
        }
        let leads = player.group_members.len() > 1
            && player.group_members.first() == Some(&player.player_id);
        if kind == SpeakerKind::Group || leads {
            let mut members = player.group_members.clone();
            if kind != SpeakerKind::Group && members.first() != Some(&player.player_id) {
                members.insert(0, player.player_id.clone());
            }
            return Some(SpeakerGroup {
                leader_id: speaker_id(self.server_id, &player.player_id),
                member_ids: ids(&members),
                role: GroupRole::Leader,
                fixed: !player.static_group_members.is_empty(),
            });
        }
        None
    }

    /// The speaker of a visible player, at revision 0. Members mirror their leader's
    /// playback, now playing and context.
    #[expect(clippy::too_many_lines, reason = "one field after another")]
    pub fn speaker(&self, player: &Player, art: &mut ArtSeen) -> Speaker {
        let kind = match player.player_type {
            PlayerType::Group => SpeakerKind::Group,
            PlayerType::StereoPair => SpeakerKind::StereoPair,
            PlayerType::Player => SpeakerKind::Speaker,
            _ => SpeakerKind::Unknown,
        };
        let has = |f: PlayerFeature| player.supports(&f);
        let powered = has(PlayerFeature::Power).then(|| player.powered.unwrap_or(true));
        let availability = if !player.available {
            Availability::Unavailable
        } else if powered == Some(false) {
            Availability::PoweredOff
        } else {
            Availability::Available
        };
        let source = self.leader_of(player);
        let origin = self.origin(source);
        let mut capabilities = BTreeSet::new();
        match availability {
            Availability::Unavailable => {}
            Availability::PoweredOff => {
                capabilities.insert(Capability::Play);
                if has(PlayerFeature::Power) {
                    capabilities.insert(Capability::Power);
                }
            }
            Availability::Available => {
                capabilities.extend([Capability::Play, Capability::Stop]);
                if has(PlayerFeature::Pause) {
                    capabilities.insert(Capability::Pause);
                }
                if has(PlayerFeature::VolumeSet) {
                    capabilities.insert(Capability::Volume);
                }
                if has(PlayerFeature::VolumeMute) {
                    capabilities.insert(Capability::Mute);
                }
                if has(PlayerFeature::Power) {
                    capabilities.insert(Capability::Power);
                }
                if has(PlayerFeature::SetMembers) || kind == SpeakerKind::Group {
                    capabilities.insert(Capability::Group);
                }
                if origin == Origin::Queue {
                    capabilities.extend([
                        Capability::NextPrevious,
                        Capability::Queue,
                        Capability::Shuffle,
                        Capability::Repeat,
                        Capability::Transfer,
                    ]);
                    if has(PlayerFeature::Seek) {
                        capabilities.insert(Capability::Seek);
                    }
                }
            }
        }
        let group = self.group(player, kind);
        let leader = group.as_ref().is_some_and(|g| g.role == GroupRole::Leader);
        let reachable = availability == Availability::Available;
        let playback = reachable.then(|| self.playback(source, origin)).flatten();
        let now_playing = reachable
            .then(|| self.now_playing(source, origin, art))
            .flatten();
        let volume_set = capabilities.contains(&Capability::Volume);
        Speaker {
            id: speaker_id(self.server_id, &player.player_id),
            name: player
                .display_name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| player.name.clone()),
            kind,
            protocol: protocol(self.domain_of(&player.provider)).to_owned(),
            manufacturer: player
                .device_info
                .as_ref()
                .and_then(|d| d.manufacturer.clone())
                .filter(|m| !m.is_empty()),
            model: player
                .device_info
                .as_ref()
                .and_then(|d| d.model.clone())
                .filter(|m| !m.is_empty()),
            availability,
            powered: (availability != Availability::Unavailable)
                .then_some(powered)
                .flatten(),
            volume: (reachable && volume_set)
                .then(|| player.volume_level.map(|v| v.min(100) as u8))
                .flatten(),
            muted: (reachable && capabilities.contains(&Capability::Mute))
                .then(|| player.volume_muted.unwrap_or(false)),
            group_volume: (reachable && leader && volume_set)
                .then(|| player.group_volume.map(|v| v.min(100) as u8))
                .flatten(),
            can_group_with: if reachable {
                self.can_group_with(player)
            } else {
                Vec::new()
            },
            group: reachable.then_some(group).flatten(),
            context: playback
                .as_ref()
                .and_then(|_| self.contexts.get(&source.player_id).cloned()),
            playback,
            now_playing,
            capabilities: capabilities.into_iter().collect(),
            revision: 0,
        }
    }

    /// Song ids the current queue items name that are not resolved yet.
    pub fn unresolved_songs(&self) -> Vec<SongId> {
        let mut wanted = BTreeSet::new();
        for queue in self.queues.values() {
            for item in [&queue.current_item, &queue.next_item]
                .into_iter()
                .flatten()
            {
                if let Some(id) = self.our_song(item)
                    && !self.songs.contains_key(&id)
                {
                    wanted.insert(id);
                }
            }
        }
        wanted.into_iter().collect()
    }
}

/// The order of contract 1.2: groups first (each leader followed by its members), then the
/// rest, by folded name.
pub(crate) fn order(speakers: &mut [std::sync::Arc<Speaker>]) {
    let names: HashMap<SpeakerId, String> = speakers
        .iter()
        .map(|s| (s.id.clone(), tracks_model::folded(&s.name)))
        .collect();
    speakers.sort_by_cached_key(|s| match &s.group {
        Some(group) => {
            let leader = names.get(&group.leader_id).cloned().unwrap_or_default();
            let rank = u8::from(group.role == GroupRole::Member);
            (
                0_u8,
                leader,
                rank,
                names.get(&s.id).cloned().unwrap_or_default(),
            )
        }
        None if s.kind == SpeakerKind::Group => (
            0,
            names.get(&s.id).cloned().unwrap_or_default(),
            0,
            String::new(),
        ),
        None => (
            1,
            names.get(&s.id).cloned().unwrap_or_default(),
            0,
            String::new(),
        ),
    });
}

/// Equal apart from the revision and where playback is now.
pub(crate) fn same_but_position(a: &Speaker, b: &Speaker) -> bool {
    let strip = |s: &Speaker| {
        let mut s = s.clone();
        s.revision = 0;
        if let Some(p) = s.playback.as_mut() {
            p.position_ms = 0;
            p.position_at = Millis(0);
        }
        s
    };
    strip(a) == strip(b)
}
