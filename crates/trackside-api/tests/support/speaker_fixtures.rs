//! Speakers and Music Assistant admin shapes (api.md sections 16 and 20), built with the
//! handlers' own builders from core records. The top-level ones are copied into the apps.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use trackside_api::wire::{self, speakers as w};
use trackside_api::{ApiError, build};
use trackside_core::assistant::{
    AssistantOverview, AssistantStatus, AssistantTest, AssistantUser, Availability, Capability,
    CatalogItem, CommandAccepted, CommandFailure, ConnectionState, GroupRole, LanUrl, LibrarySong,
    MediaInfo, MusicProvider, NowPlaying, Origin, PlayContext, PlayState, Playback, PlayerProvider,
    ProviderLists, QueueEntry, RegistrationKey, RegistrationState, RegistrationView, Repeat,
    ServerFacts, ServiceUser, SettingSource, Speaker, SpeakerError, SpeakerEvent, SpeakerGroup,
    SpeakerKind, SpeakerList, SpeakerQueue, SpeakerRelay, SpeakersState,
};
use trackside_core::relay::{RelayFormat, RelayState, RelayTier, RelayView};
use trackside_core::{Access, ArtworkId, DeviceKey, LibraryKey, PublicId, SongId, UserKey};

use super::at;

type Out = Vec<(&'static str, Value)>;

fn pid<T: PublicId>(text: &str) -> T {
    T::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

const KITCHEN: &str = "o4j8fccgajqsqxancbfcz";
const LIVING_ROOM: &str = "o7sny3kfnvsjbbhx01f2j";
const BEDROOM: &str = "o0jjvqt740t3wm3jvetx2";
const OFFICE: &str = "osm579gnwm00ysy1hbvhq";
const SPOTIFY: &str = "bqtyy4kcexcsg6wa0y4w5";
const MA: &str = "http://192.168.1.20:8095";

fn all_capabilities() -> Vec<Capability> {
    Capability::ALL.to_vec()
}

fn halo(library: LibraryKey) -> MediaInfo {
    MediaInfo {
        title: String::from("Halo"),
        artist: Some(String::from("Beyoncé")),
        album: Some(String::from("I Am... Sasha Fierce")),
        duration_ms: Some(261_640),
        song: Some(LibrarySong {
            id: pid::<SongId>("sdjcz08vf67h7cx28snq6"),
            library,
            artwork: Some(pid::<ArtworkId>("ca81rcgy3d9pfg6saekwd")),
        }),
        catalog: None,
        song_key: Some(tracks_model::song_key("Halo", "Beyoncé")),
    }
}

fn dont_stop() -> MediaInfo {
    MediaInfo {
        title: String::from("Don't Stop Me Now"),
        artist: Some(String::from("Queen")),
        album: Some(String::from("Jazz (2011 Remaster)")),
        duration_ms: Some(209_000),
        song: None,
        catalog: Some(CatalogItem {
            reference: String::from("mc1.dHJhY2sfc3BvdGlmeR81VDhFRFVEcUtjczZPU093RXNmcUc3"),
            service: pid(SPOTIFY),
            artwork: Some(pid("mkem33c2vrhyyrdj08jqc")),
        }),
        song_key: Some(tracks_model::song_key("Don't Stop Me Now", "Queen")),
    }
}

fn album_context() -> PlayContext {
    PlayContext {
        kind: String::from("album"),
        id: Some(String::from("a3pqh4wnvzkjak75rxbft")),
        name: Some(String::from("I Am... Sasha Fierce")),
    }
}

fn sonos(id: &str, name: &str, model: &str, library: LibraryKey) -> Speaker {
    let others: Vec<_> = [BEDROOM, KITCHEN, LIVING_ROOM]
        .into_iter()
        .filter(|o| *o != id)
        .map(pid)
        .collect();
    Speaker {
        id: pid(id),
        name: name.to_owned(),
        kind: SpeakerKind::Speaker,
        protocol: String::from("sonos"),
        manufacturer: Some(String::from("Sonos")),
        model: Some(model.to_owned()),
        availability: Availability::Available,
        capabilities: all_capabilities(),
        powered: Some(true),
        volume: None,
        muted: Some(false),
        group: None,
        group_volume: None,
        can_group_with: {
            let mut sorted = others;
            sorted.sort();
            sorted
        },
        playback: Some(Playback {
            state: PlayState::Playing,
            position_ms: 81_000,
            position_at: at(0),
            shuffle: false,
            repeat: Repeat::Off,
        }),
        now_playing: Some(NowPlaying {
            media: halo(library),
            item_id: Some(String::from("6f0c3b1e9a2d4c7f8e5b0a1d2c3e4f50")),
            origin: Origin::Queue,
        }),
        context: Some(album_context()),
        relay: None,
        revision: 0,
    }
}

fn group(role: GroupRole) -> SpeakerGroup {
    SpeakerGroup {
        leader_id: pid(KITCHEN),
        member_ids: vec![pid(KITCHEN), pid(LIVING_ROOM)],
        role,
        fixed: false,
    }
}

fn bedroom_playing() -> Speaker {
    Speaker {
        id: pid(BEDROOM),
        name: String::from("Bedroom"),
        kind: SpeakerKind::Speaker,
        protocol: String::from("cast"),
        manufacturer: Some(String::from("Google")),
        model: Some(String::from("Nest Audio")),
        availability: Availability::Available,
        capabilities: vec![
            Capability::Mute,
            Capability::NextPrevious,
            Capability::Pause,
            Capability::Play,
            Capability::Queue,
            Capability::Repeat,
            Capability::Seek,
            Capability::Shuffle,
            Capability::Stop,
            Capability::Transfer,
            Capability::Volume,
        ],
        powered: None,
        volume: Some(28),
        muted: Some(false),
        group: None,
        group_volume: None,
        can_group_with: Vec::new(),
        playback: Some(Playback {
            state: PlayState::Playing,
            position_ms: 12_000,
            position_at: at(0),
            shuffle: true,
            repeat: Repeat::All,
        }),
        now_playing: Some(NowPlaying {
            media: dont_stop(),
            item_id: Some(String::from("0b1c2d3e4f5061728394a5b6c7d8e9f0")),
            origin: Origin::Queue,
        }),
        context: Some(PlayContext {
            kind: String::from("catalog"),
            id: None,
            name: Some(String::from("Late Night Chill")),
        }),
        relay: None,
        revision: 980,
    }
}

fn json<T: serde::Serialize>(value: &T) -> Result<Value, serde_json::Error> {
    serde_json::to_value(value)
}

fn event(event: &SpeakerEvent, access: &Access) -> Result<Value, serde_json::Error> {
    let (_, data) = build::speaker_event_body(event, access).unwrap_or(("", String::from("{}")));
    serde_json::from_str(&data)
}

fn error(error: SpeakerError, request_id: &str) -> Result<Value, serde_json::Error> {
    let mut envelope = ApiError::from(error).envelope();
    envelope.error.request_id = Some(request_id.to_owned());
    json(&envelope)
}

#[expect(clippy::too_many_lines, reason = "one list of shapes")]
pub fn fixtures(access: &Access, library: LibraryKey) -> Result<Out, serde_json::Error> {
    let mut out: Out = Vec::new();

    let mut kitchen = sonos(KITCHEN, "Kitchen", "Era 100", library);
    kitchen.group = Some(group(GroupRole::Leader));
    kitchen.group_volume = Some(40);
    kitchen.volume = Some(35);
    kitchen.revision = 1042;
    let mut living = sonos(LIVING_ROOM, "Living Room", "Five", library);
    living.group = Some(group(GroupRole::Member));
    living.volume = Some(45);
    living.revision = 1040;
    let bedroom_off = Speaker {
        id: pid(BEDROOM),
        name: String::from("Bedroom"),
        kind: SpeakerKind::Speaker,
        protocol: String::from("cast"),
        manufacturer: Some(String::from("Google")),
        model: Some(String::from("Nest Audio")),
        availability: Availability::PoweredOff,
        capabilities: vec![Capability::Play, Capability::Power],
        powered: Some(false),
        volume: None,
        muted: None,
        group: None,
        group_volume: None,
        can_group_with: Vec::new(),
        playback: None,
        now_playing: None,
        context: None,
        relay: None,
        revision: 977,
    };
    let office = Speaker {
        id: pid(OFFICE),
        name: String::from("Office"),
        kind: SpeakerKind::Speaker,
        protocol: String::from("airplay"),
        manufacturer: None,
        model: None,
        availability: Availability::Unavailable,
        capabilities: Vec::new(),
        powered: None,
        volume: None,
        muted: None,
        group: None,
        group_volume: None,
        can_group_with: Vec::new(),
        playback: None,
        now_playing: None,
        context: None,
        relay: None,
        revision: 512,
    };
    let list = SpeakerList {
        speakers: [kitchen.clone(), living, bedroom_off, office]
            .into_iter()
            .map(Arc::new)
            .collect(),
        state: Some(SpeakersState::Ok),
        since: None,
    };
    out.push(("speakers.json", json(&build::speakers_body(&list, access))?));
    let unavailable = SpeakerList {
        speakers: Vec::new(),
        state: Some(SpeakersState::Unavailable),
        since: Some(at(-120)),
    };
    out.push((
        "speakers-unavailable.json",
        json(&build::speakers_body(&unavailable, access))?,
    ));

    let bedroom = Arc::new(bedroom_playing());
    out.push((
        "speaker.json",
        json(&build::speaker_body(&bedroom, access))?,
    ));
    let mut paused = bedroom_playing();
    if let Some(p) = paused.playback.as_mut() {
        p.state = PlayState::Paused;
    }
    let accepted = CommandAccepted {
        action: "pause",
        command_id: pid("j7bwksg3pg57nf95cwz0b"),
        settle_by: at(5),
        speaker: Arc::new(paused),
    };
    out.push((
        "speaker-command.json",
        json(&build::speaker_command_body(&accepted, access))?,
    ));
    out.push((
        "speaker-play-request.json",
        json(&w::PlayRequest {
            context: Some(w::PlayContext {
                id: Some(String::from("a3pqh4wnvzkjak75rxbft")),
                kind: String::from("album"),
                name: Some(String::from("I Am... Sasha Fierce")),
            }),
            items: vec![
                w::PlayItem {
                    reference: None,
                    song_id: Some(String::from("sdjcz08vf67h7cx28snq6")),
                },
                w::PlayItem {
                    reference: Some(String::from(
                        "mc1.dHJhY2sfc3BvdGlmeR8wV3FkMHpBNXdOcFFuUUJHRVVIMGY0",
                    )),
                    song_id: None,
                },
                w::PlayItem {
                    reference: None,
                    song_id: Some(String::from("sqfnw07xdpvnbah038cz8")),
                },
            ],
            mode: Some(String::from("replace")),
            position_ms: Some(81_000),
            repeat: Some(String::from("off")),
            shuffle: Some(false),
            start_index: Some(0),
            hold: false,
            radio: false,
            takeover: false,
        })?,
    ));
    let one_ref = || {
        vec![w::PlayItem {
            reference: Some(String::from(
                "mc1.dHJhY2sfc3BvdGlmeR8wV3FkMHpBNXdOcFFuUUJHRVVIMGY0",
            )),
            song_id: None,
        }]
    };
    out.push((
        "speaker-play-radio-request.json",
        json(&w::PlayRequest {
            context: Some(w::PlayContext {
                id: None,
                kind: String::from("station"),
                name: Some(String::from("Halo Radio")),
            }),
            items: one_ref(),
            mode: None,
            position_ms: None,
            repeat: None,
            shuffle: None,
            start_index: None,
            hold: false,
            radio: true,
            takeover: true,
        })?,
    ));
    out.push((
        "speaker-play-held-request.json",
        json(&w::PlayRequest {
            context: None,
            items: one_ref(),
            mode: Some(String::from("replace")),
            position_ms: None,
            repeat: None,
            shuffle: None,
            start_index: None,
            hold: true,
            radio: false,
            takeover: true,
        })?,
    ));
    let queue = SpeakerQueue {
        id: pid(KITCHEN),
        current_index: Some(0),
        items: vec![
            QueueEntry {
                index: 0,
                item_id: String::from("6f0c3b1e9a2d4c7f8e5b0a1d2c3e4f50"),
                media: halo(library),
            },
            QueueEntry {
                index: 1,
                item_id: String::from("0b1c2d3e4f5061728394a5b6c7d8e9f0"),
                media: dont_stop(),
            },
        ],
        next_cursor: Some(String::from("eyJvIjoyfQ")),
        repeat: Repeat::Off,
        shuffle: false,
        revision: 1042,
        total: 12,
    };
    out.push((
        "speaker-queue.json",
        json(&build::speaker_queue_body(&queue, access))?,
    ));
    out.push((
        "speaker-removed-event.json",
        event(&SpeakerEvent::Removed(pid(OFFICE)), access)?,
    ));
    out.push((
        "speaker-progress-event.json",
        event(
            &SpeakerEvent::Progress {
                id: pid(KITCHEN),
                position_ms: 111_000,
                position_at: at(30),
                revision: 1042,
                state: PlayState::Playing,
            },
            access,
        )?,
    ));
    out.push((
        "speaker-queue-event.json",
        event(
            &SpeakerEvent::Queue {
                id: pid(KITCHEN),
                revision: 1043,
            },
            access,
        )?,
    ));
    out.push((
        "speaker-command-event.json",
        event(
            &SpeakerEvent::Command {
                user: access.user,
                command: pid("j8r94fwwwtm0mgm02w6gx"),
                speaker: pid(BEDROOM),
                outcome: Err(CommandFailure::new(
                    "command_failed",
                    "The speaker did not accept the command.",
                )),
            },
            access,
        )?,
    ));
    out.push((
        "speaker-command-failed-event.json",
        event(
            &SpeakerEvent::Command {
                user: access.user,
                command: pid("j3w5hd1t7x0gb8pkn2mqz"),
                speaker: pid(KITCHEN),
                outcome: Err(CommandFailure {
                    reason: Some("service_busy"),
                    service: Some(pid("bqtyy4kcexcsg6wa0y4w5")),
                    holder: Some(pid(LIVING_ROOM)),
                    holder_name: Some(String::from("Living Room")),
                    ..CommandFailure::new(
                        "command_failed",
                        "The music service is playing somewhere else.",
                    )
                }),
            },
            access,
        )?,
    ));
    out.push((
        "speakers-event.json",
        event(
            &SpeakerEvent::State {
                state: SpeakersState::Unavailable,
                since: Some(at(-120)),
            },
            access,
        )?,
    ));
    out.push((
        "error-speaker-unsupported.json",
        error(
            SpeakerError::Unsupported {
                capability: "seek",
                speaker: None,
            },
            "3c9a1e7b5d2f4a8c6e0b1d3f5a7c9e2b",
        )?,
    ));
    out.push((
        "error-speakers-unavailable.json",
        error(
            SpeakerError::Unavailable,
            "8b2d4f6a0c1e3a5c7e9b1d3f5a7c9e0d",
        )?,
    ));
    out.extend(relay(access)?);
    out.extend(admin()?);
    Ok(out)
}

const PHONE: &str = "dchjkb9k3cnapjw0kmhq6";
const RELAY: &str = "h9zvca9p26dymt4v7cxj5";
const RELAY_SPEAKER: &str = "o1a0psgqvec0jm734s9tv";

fn relay_view(access: &Access) -> RelayView {
    RelayView {
        id: pid(RELAY),
        user: access.user,
        device: DeviceKey(7),
        device_id: pid(PHONE),
        speaker_id: pid(RELAY_SPEAKER),
        created_at: at(0),
        state: RelayState::Playing,
        ended_reason: None,
        detail: None,
        generation: 3,
        tier: RelayTier::Cellular,
        latency_ms: 4000,
        idle_timeout_s: 600,
        fetch_timeout_s: 30,
        hls: Some(format!("/relay/{RELAY}/3/master.m3u8")),
        format: RelayFormat {
            codec: "aac",
            container: "mp4",
            sample_rate: 44_100,
            channels: 2,
            bit_depth: None,
            bitrate_kbps: Some(256),
        },
    }
}

fn lossless() -> RelayFormat {
    RelayFormat {
        codec: "flac",
        container: "mp4",
        sample_rate: 44_100,
        channels: 2,
        bit_depth: Some(16),
        bitrate_kbps: None,
    }
}

/// Contract 6.1: the relay session in its states, as `POST /relay/sessions` and SSE `relay`
/// send it, and the relay as its owner's speaker.
fn relay(access: &Access) -> Result<Out, serde_json::Error> {
    let mut out: Out = Vec::new();
    let playing = relay_view(access);
    out.push((
        "relay-session.json",
        event(&SpeakerEvent::Relay(Box::new(playing.clone())), access)?,
    ));
    let starting = RelayView {
        state: RelayState::Starting,
        generation: 0,
        tier: RelayTier::Lossless,
        hls: Some(format!("/relay/{RELAY}/1/master.m3u8")),
        format: lossless(),
        ..playing.clone()
    };
    out.push((
        "relay-session-starting.json",
        json(&build::relay_session_body(&starting, ""))?,
    ));
    let ended = RelayView {
        state: RelayState::Ended,
        ended_reason: Some("idle"),
        tier: RelayTier::Lossless,
        hls: None,
        format: lossless(),
        ..playing.clone()
    };
    out.push((
        "relay-session-ended.json",
        event(&SpeakerEvent::Relay(Box::new(ended)), access)?,
    ));
    let phone = Speaker {
        id: pid(RELAY_SPEAKER),
        name: String::from("Luke's iPhone"),
        kind: SpeakerKind::Phone,
        protocol: String::from("relay"),
        manufacturer: None,
        model: None,
        availability: Availability::Available,
        capabilities: vec![
            Capability::NextPrevious,
            Capability::Pause,
            Capability::Play,
            Capability::Queue,
            Capability::Repeat,
            Capability::Seek,
            Capability::Shuffle,
            Capability::Stop,
            Capability::Transfer,
        ],
        powered: None,
        volume: None,
        muted: None,
        group: None,
        group_volume: None,
        can_group_with: Vec::new(),
        playback: Some(Playback {
            state: PlayState::Playing,
            position_ms: 12_000,
            position_at: at(0),
            shuffle: false,
            repeat: Repeat::Off,
        }),
        now_playing: Some(NowPlaying {
            media: dont_stop(),
            item_id: Some(String::from("0b1c2d3e4f5061728394a5b6c7d8e9f0")),
            origin: Origin::Queue,
        }),
        context: None,
        relay: Some(SpeakerRelay {
            session: pid(RELAY),
            user: access.user,
            device: DeviceKey(7),
            device_id: pid(PHONE),
        }),
        revision: 7,
    };
    out.push((
        "relay-speaker.json",
        json(&build::speaker_body_on(&phone, access, Some(DeviceKey(7))))?,
    ));
    let other = Access {
        user: UserKey(access.user.0 + 1),
        ..access.clone()
    };
    assert!(
        build::speaker_event_body(
            &SpeakerEvent::Owned {
                user: access.user,
                event: Box::new(SpeakerEvent::Removed(pid(RELAY_SPEAKER))),
            },
            &other,
        )
        .is_none()
    );
    assert!(build::speaker_event_body(&SpeakerEvent::Relay(Box::new(playing)), &other).is_none());
    Ok(out)
}

fn facts(schema: u32, version: &str) -> ServerFacts {
    ServerFacts {
        server_id: String::from("8f2e4b6a1c3d5e7f9a0b2c4d6e8f0a1b"),
        name: Some(String::from("Music Assistant")),
        version: version.to_owned(),
        schema_version: schema,
        min_supported_schema_version: 28,
        compatible: true,
        tested: true,
        relay_supported: true,
    }
}

fn edit(instance: &str) -> String {
    format!("{MA}/#/settings/editprovider/{instance}")
}

/// Trackside's own registration's instance in the fixtures.
const OURS: &str = "opensubsonic--T7rk2mQa";
/// 2026-11-12T18:00:00Z.
const TOKEN_SET: i64 = -142_200;

#[expect(clippy::too_many_lines, reason = "one list of shapes")]
fn admin() -> Result<Out, serde_json::Error> {
    let mut out: Out = Vec::new();
    let status = AssistantStatus {
        configured: true,
        enabled: true,
        auto_register: true,
        state: ConnectionState::Connected,
        since: Some(at(-5400)),
        last_error: None,
        hidden_services: Vec::new(),
        lan_url: Some(LanUrl {
            url: String::from("http://192.168.1.20:4747"),
            configured: false,
            reachable: true,
            checked_at: Some(at(0)),
            warning: None,
        }),
        relays_active: 1,
        relays_max: 4,
        server: Some(facts(65, "2.10.13")),
        services: Some(2),
        speakers: Some(5),
        url_source: SettingSource::Dashboard,
        token_source: SettingSource::Dashboard,
        token_set: true,
        token_set_at: Some(at(TOKEN_SET)),
        custom_ca: false,
        ui_url: Some(String::from(MA)),
        url: Some(String::from(MA)),
        user: Some(AssistantUser {
            username: String::from("trackside"),
            display_name: Some(String::from("Trackside")),
            role: Some(String::from("admin")),
        }),
    };
    out.push((
        "admin/music-assistant.json",
        json(&build::music_assistant_body(&status))?,
    ));
    out.push((
        "admin/music-assistant-unset.json",
        json(&build::music_assistant_body(&AssistantStatus::not_set_up()))?,
    ));
    let mut test_server = facts(65, "2.10.13");
    test_server.relay_supported = false;
    out.push((
        "admin/music-assistant-test.json",
        json(&build::music_assistant_test_body(&AssistantTest {
            ok: true,
            detail: String::from("Connected to Music Assistant 2.10.13 as trackside (admin)."),
            elapsed: Duration::from_millis(312),
            players: Some(5),
            providers: Some(9),
            server: Some(test_server),
        }))?,
    ));
    out.push((
        "admin/music-assistant-test-failed.json",
        json(&build::music_assistant_test_body(&AssistantTest {
            ok: false,
            detail: String::from(
                "Music Assistant refused the token (error 23: Invalid or expired token).",
            ),
            elapsed: Duration::from_millis(140),
            players: None,
            providers: None,
            server: Some(facts(128, "2.11.0b11")),
        }))?,
    ));
    out.push((
        "admin/music-assistant-registration.json",
        json(&build::music_assistant_registration_body(
            &RegistrationView {
                state: RegistrationState::Registered,
                base_url: Some(String::from("http://192.168.1.20:4747")),
                key: Some(RegistrationKey {
                    id: pid("kwd5w7vf6ptaf3cwd1xbt"),
                    prefix: String::from("tsk_Qm3vXa"),
                    scopes: vec!["library", "stream"],
                    created_at: at(TOKEN_SET + 5),
                    last_used_at: Some(at(-60)),
                }),
                provider_instance_id: Some(String::from(OURS)),
                registered_at: Some(at(TOKEN_SET + 6)),
                service_user: Some(ServiceUser {
                    id: pid("uvxvacxf5ak9afxq4qdjj"),
                    username: String::from("music-assistant"),
                    display_name: String::from("Music Assistant"),
                }),
                last_error: None,
                ui_url: Some(edit(OURS)),
            },
        ))?,
    ));
    out.push((
        "admin/music-assistant-registration-needs-admin.json",
        json(&build::music_assistant_registration_body(&RegistrationView {
            state: RegistrationState::NeedsAdmin,
            base_url: Some(String::from("http://192.168.1.20:4747")),
            key: None,
            provider_instance_id: None,
            registered_at: None,
            service_user: None,
            last_error: Some((
                at(0),
                String::from(
                    "The Music Assistant account trackside is not an admin, so it cannot add providers.",
                ),
            )),
            ui_url: None,
        }))?,
    ));
    let music = |instance: &str,
                 domain: &str,
                 name: &str,
                 hidden: bool,
                 service: Option<&str>,
                 streaming: bool| MusicProvider {
        instance_id: instance.to_owned(),
        domain: domain.to_owned(),
        name: name.to_owned(),
        available: true,
        streaming,
        hidden,
        service_id: service.map(pid),
        trackside: instance == OURS,
        ui_url: Some(edit(instance)),
    };
    let players = |instance: &str, domain: &str, name: &str, speakers: u32| PlayerProvider {
        instance_id: instance.to_owned(),
        domain: domain.to_owned(),
        name: name.to_owned(),
        available: true,
        speakers,
        ui_url: Some(edit(instance)),
    };
    out.push((
        "admin/music-assistant-providers.json",
        json(&build::music_assistant_providers_body(&ProviderLists {
            music: vec![
                music(
                    "spotify--Xb4LkP2q",
                    "spotify",
                    "Spotify",
                    false,
                    Some(SPOTIFY),
                    true,
                ),
                music(
                    "ytmusic--P9wq1Lza",
                    "ytmusic",
                    "YouTube Music",
                    true,
                    Some("b9j3k7m1q5t8w2x6y0z4c"),
                    true,
                ),
                music(OURS, "opensubsonic", "Trackside (Home)", false, None, false),
            ],
            players: vec![
                players("sonos--Kq2m8VbR", "sonos", "Sonos", 3),
                players("airplay--Lz5n1CwT", "airplay", "AirPlay", 2),
            ],
            ui_url: Some(format!("{MA}/#/settings/providers")),
        }))?,
    ));
    Ok(out)
}

/// The overview's `music_assistant`, connected and registered.
pub fn overview() -> wire::admin::OverviewMusicAssistant {
    build::music_assistant_overview_body(&AssistantOverview {
        state: ConnectionState::Connected,
        registration: Some(RegistrationState::Registered),
        relays: Some(1),
        services: Some(2),
        speakers: Some(5),
    })
}

/// The same before Music Assistant is set up.
pub fn overview_unset() -> wire::admin::OverviewMusicAssistant {
    build::music_assistant_overview_body(&AssistantOverview {
        state: ConnectionState::NotSetUp,
        registration: None,
        relays: None,
        services: None,
        speakers: None,
    })
}
