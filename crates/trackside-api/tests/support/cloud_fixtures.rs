//! Trackside Cloud shapes (api.md section 19), built with the handlers' own builders. The app
//! mirrors these with the other top-level fixtures.

use serde_json::{Map, Value, json};
use trackside_api::build;
use trackside_api::wire::{Time, cloud as wire};
use trackside_cloud::{
    Body, CloudDevice, Cursor, Entity, Limits, Page, PushOutcome, PushResult, Record, Reject,
    Setting, SettingOutcome, SettingResult, SettingsPage, Usage, UsageEntry,
};
use trackside_core::{DeviceId, Millis};

use super::{at, id};

const IPHONE: &str = "6F1D9C2A-3B4E-4F5A-8B7C-1D2E3F4A5B6C";
const MAC: &str = "0C7B5E3A-9D1F-4E2B-8A6C-5F4E3D2C1B0A";
/// The sync epoch the fixtures' cursors and capabilities carry.
pub const EPOCH: &str = "3f9a0c1d7e2b4a65";

fn cursor(seq: i64) -> Cursor {
    Cursor {
        epoch: EPOCH.to_owned(),
        seq,
    }
}

const OLD_IPHONE: &str = "E2D4C6B8-1A3F-4C5E-9B7D-0F2E4D6C8A1B";

fn fields(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn halo() -> Map<String, Value> {
    fields(json!({
        "title": "Halo",
        "artistName": "Beyoncé",
        "albumTitle": "I Am... Sasha Fierce",
        "artworkURL": "https://is1-ssl.mzstatic.com/image/thumb/Music/halo/600x600bb.jpg",
        "playedAtMs": at(-3600).0,
        "kind": "onDemand",
        "source": "appleMusic",
        "songKey": "halo\u{1f}beyonce",
        "catalogID": "1440769385",
        "serverSongID": "s7m2k4q9x1v8b3n6c0de",
        "sessionID": "9B2C4D6E-8F0A-4B1C-9D3E-5F7A9B1C3D5E",
        "platform": "ios",
        "recordedByDeviceID": IPHONE,
        "playedMs": 254_000,
        "durationMs": 261_640,
        "scrobbledAt": at(-3300).0
    }))
}

fn lastfm_play() -> Map<String, Value> {
    fields(json!({
        "title": "Don't Stop Me Now",
        "artistName": "Queen",
        "albumTitle": "Jazz",
        "playedAtMs": 1_700_000_000_000_i64,
        "kind": "lastFM",
        "source": "appleMusic",
        "songKey": "don't stop me now\u{1f}queen",
        "platform": "macos",
        "recordedByDeviceID": MAC
    }))
}

fn play_record() -> Record {
    Record {
        id: String::from("6A8C0E2F-4B6D-4F8A-9C1E-3B5D7F9A1C3E"),
        version: 917,
        device_id: IPHONE.to_owned(),
        client_modified_at: at(-3300),
        body: Body::Live(halo()),
    }
}

fn entry_record() -> Record {
    Record {
        id: String::from("5D7F9B1C-3E5A-4C7E-8F1B-3D5F7A9C1E3B"),
        version: 1204,
        device_id: MAC.to_owned(),
        client_modified_at: at(-600),
        body: Body::Live(fields(json!({
            "playlistID": "C3E5A7C9-1B3D-4F5A-8C7E-9A1B3C5D7E9F",
            "order": "a0V",
            "addedAtMs": at(-600).0,
            "track": {
                "title": "Halo",
                "artist": "Beyoncé",
                "album": "I Am... Sasha Fierce",
                "ids": {"appleMusic": "1440769385", "trackside": "s7m2k4q9x1v8b3n6c0de",
                        "spotify": "4JehYebiI9JE8sR8MisGVb"}
            }
        }))),
    }
}

fn setting(key: &str, value: Value, at: Millis, device: &str, version: i64) -> Setting {
    Setting {
        key: key.to_owned(),
        value,
        modified_at: at,
        device_id: device.to_owned(),
        version,
    }
}

fn nearby_envelope() -> Value {
    json!({"$encrypted": {"v": 1, "alg": "chachapoly", "kid": "icloud-keychain-1",
        "nonce": "3q2+7wABAgMEBQYH", "ct": "kJ4w0bQ2Xx9lY8cC1dRk3mXz5Q7pN6vT2sH8gF4aE0iL9oK3uJ1yW6rD5bM7nC2Z"}})
}

#[expect(clippy::too_many_lines, reason = "one list of the cloud shapes")]
pub fn fixtures() -> Result<Vec<(&'static str, Value)>, serde_json::Error> {
    let lastfm_id = "lastfm:1700000000:5f2b9c1d";
    let push = wire::PushBatch {
        items: vec![
            wire::PushItem {
                id: play_record().id,
                base_version: None,
                fields: Some(halo()),
                client_modified_at: Time(at(-3300)),
                device_id: IPHONE.to_owned(),
                deleted_at: None,
                merged_into: None,
                undelete: false,
            },
            wire::PushItem {
                id: lastfm_id.to_owned(),
                base_version: None,
                fields: Some(lastfm_play()),
                client_modified_at: Time(at(-60)),
                device_id: MAC.to_owned(),
                deleted_at: None,
                merged_into: None,
                undelete: false,
            },
            wire::PushItem {
                id: String::from("1F3B5D7F-9A1C-4E3B-8D5F-7A9C1E3B5D7F"),
                base_version: Some(902),
                fields: None,
                client_modified_at: Time(at(-30)),
                device_id: MAC.to_owned(),
                deleted_at: Some(Time(at(-30))),
                merged_into: Some(play_record().id),
                undelete: false,
            },
        ],
    };
    let results = [
        PushResult {
            id: play_record().id,
            outcome: PushOutcome::Accepted { version: 918 },
        },
        PushResult {
            id: lastfm_id.to_owned(),
            outcome: PushOutcome::Duplicate { version: 640 },
        },
        PushResult {
            id: String::from("1F3B5D7F-9A1C-4E3B-8D5F-7A9C1E3B5D7F"),
            outcome: PushOutcome::Conflict {
                current: Some(Record {
                    id: String::from("1F3B5D7F-9A1C-4E3B-8D5F-7A9C1E3B5D7F"),
                    version: 911,
                    device_id: IPHONE.to_owned(),
                    client_modified_at: at(-200),
                    body: Body::Live(fields(json!({
                        "title": "Halo", "artistName": "Beyoncé", "playedAtMs": at(-3600).0,
                        "kind": "onDemand", "source": "appleMusic",
                        "songKey": "halo\u{1f}beyonce", "recordedByDeviceID": IPHONE
                    }))),
                }),
            },
        },
        PushResult {
            id: String::from("8E0A2C4E-6F8A-4B0C-9E2A-4C6E8A0C2E4A"),
            outcome: PushOutcome::Rejected {
                reason: Reject::Deleted,
            },
        },
    ];
    let plays = Page {
        upserts: vec![
            play_record(),
            Record {
                id: lastfm_id.to_owned(),
                version: 640,
                device_id: MAC.to_owned(),
                client_modified_at: at(-86_400),
                body: Body::Live(lastfm_play()),
            },
        ],
        deletes: vec![Record {
            id: String::from("1F3B5D7F-9A1C-4E3B-8D5F-7A9C1E3B5D7F"),
            version: 918,
            device_id: MAC.to_owned(),
            client_modified_at: at(-30),
            body: Body::Deleted {
                deleted_at: at(-30),
                merged_into: Some(play_record().id),
            },
        }],
        cursor: cursor(918),
        has_more: false,
    };
    let entries = Page {
        upserts: vec![entry_record()],
        deletes: Vec::new(),
        cursor: cursor(1204),
        has_more: true,
    };
    let settings = SettingsPage {
        settings: vec![
            setting(
                "playlistName",
                json!("Heard on the radio"),
                at(-86_400),
                MAC,
                12,
            ),
            setting(
                "forgottenSongs",
                json!(["halo\u{1f}beyonce", "lion\u{1f}elevation"]),
                at(-7200),
                IPHONE,
                305,
            ),
            setting("playlistID", Value::Null, at(-3600), IPHONE, 310),
            setting("autoAddToPlaylist", json!(true), Millis(0), MAC, 3),
            setting("nearbyKey", nearby_envelope(), at(-600), IPHONE, 412),
        ],
        cursor: cursor(918),
    };
    let patch = wire::SettingsPatch {
        settings: [
            (
                String::from("playlistName"),
                wire::SettingChange {
                    value: Some(json!("Tracks")),
                    modified_at: Time(at(-10)),
                    device_id: IPHONE.to_owned(),
                },
            ),
            (
                String::from("playlistID"),
                wire::SettingChange {
                    value: Some(Value::Null),
                    modified_at: Time(at(-10)),
                    device_id: IPHONE.to_owned(),
                },
            ),
            (
                String::from("nearbyKey"),
                wire::SettingChange {
                    value: Some(nearby_envelope()),
                    modified_at: Time(at(-10)),
                    device_id: IPHONE.to_owned(),
                },
            ),
        ]
        .into_iter()
        .collect(),
    };
    let setting_results = [
        SettingResult {
            key: String::from("playlistName"),
            outcome: SettingOutcome::Accepted,
            current: Some(setting(
                "playlistName",
                json!("Tracks"),
                at(-10),
                IPHONE,
                919,
            )),
        },
        SettingResult {
            key: String::from("autoAddToPlaylist"),
            outcome: SettingOutcome::Conflict,
            current: Some(setting("autoAddToPlaylist", json!(true), at(-60), MAC, 640)),
        },
        SettingResult {
            key: String::from("forgottenSongs"),
            outcome: SettingOutcome::Accepted,
            current: Some(setting(
                "forgottenSongs",
                json!([
                    "halo\u{1f}beyonce",
                    "lion\u{1f}elevation",
                    "tides\u{1f}umbra"
                ]),
                at(-10),
                IPHONE,
                920,
            )),
        },
        SettingResult {
            key: String::from("nearbyKey"),
            outcome: SettingOutcome::Rejected(Reject::EncryptionRequired),
            current: None,
        },
    ];
    let server_device: DeviceId = id("device iphone");
    let devices = [
        CloudDevice {
            device_id: IPHONE.to_owned(),
            predecessors: vec![OLD_IPHONE.to_owned()],
            name: Some(String::from("Luke's iPhone")),
            platform: Some(String::from("ios")),
            server_device: Some(server_device.clone()),
            registered_at: at(-86_400 * 3),
            updated_at: at(-86_400),
        },
        CloudDevice {
            device_id: MAC.to_owned(),
            predecessors: Vec::new(),
            name: Some(String::from("Mac Studio")),
            platform: Some(String::from("macos")),
            server_device: None,
            registered_at: at(-86_400 * 30),
            updated_at: at(-86_400 * 30),
        },
    ];
    let usage = Usage {
        items: 152_340,
        bytes: 98_312_448,
        entities: vec![
            UsageEntry {
                entity: Entity::Plays,
                items: 151_900,
                bytes: 97_800_000,
            },
            UsageEntry {
                entity: Entity::Settings,
                items: 21,
                bytes: 9_600,
            },
            UsageEntry {
                entity: Entity::Stations,
                items: 419,
                bytes: 502_848,
            },
        ],
    };
    Ok(vec![
        ("cloud-push-plays.json", serde_json::to_value(&push)?),
        (
            "cloud-push-results.json",
            serde_json::to_value(build::push_body(&results))?,
        ),
        (
            "cloud-plays-page.json",
            serde_json::to_value(build::cloud_page_body(&plays))?,
        ),
        (
            "cloud-playlist-entries-page.json",
            serde_json::to_value(build::cloud_page_body(&entries))?,
        ),
        (
            "cloud-settings.json",
            serde_json::to_value(build::settings_body(&settings))?,
        ),
        ("cloud-settings-patch.json", serde_json::to_value(&patch)?),
        (
            "cloud-setting-results.json",
            serde_json::to_value(build::setting_results_body(&setting_results))?,
        ),
        (
            "cloud-devices.json",
            serde_json::to_value(build::devices_body(&devices, Some(&server_device)))?,
        ),
        (
            "cloud-device-registration.json",
            serde_json::to_value(wire::DeviceRegistration {
                predecessors: vec![OLD_IPHONE.to_owned()],
                name: Some(String::from("Luke's iPhone")),
                platform: Some(String::from("ios")),
            })?,
        ),
        (
            "cloud-usage.json",
            serde_json::to_value(build::usage_body(&usage, Limits::default()))?,
        ),
        (
            "cloud-event.json",
            serde_json::to_value(wire::CloudEvent {
                entity: Entity::Plays.as_str().to_owned(),
            })?,
        ),
    ])
}
