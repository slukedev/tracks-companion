//! JSON messages. Every message is `{"type", "payload"}`; unknown fields and types are ignored
//! on the way in, and nothing beyond what the spec defines is sent.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::b64;

/// An audio format, as listed in `client/hello` and chosen in `stream/start`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Format {
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub bit_depth: u32,
}

impl Format {
    /// FLAC 44.1 kHz, 16 bit, stereo: what the relay asks for.
    pub fn flac_cd() -> Self {
        Self {
            codec: "flac".into(),
            channels: 2,
            sample_rate: 44_100,
            bit_depth: 16,
        }
    }
}

/// What the client says about itself in `client/hello` and `client/state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelloInfo {
    pub name: String,
    pub product_name: String,
    pub manufacturer: String,
    pub software_version: String,
    pub format: Format,
    pub buffer_capacity: u32,
}

/// The player timing the client reports in `client/state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerTiming {
    /// 0 to 5000.
    pub output_delay_ms: u32,
    pub required_lead_time_ms: u32,
    /// 0 to 5000.
    pub min_buffer_ms: u32,
}

/// Cuts a string to at most `max` bytes on a character boundary (names and device info are
/// limited to 64 bytes).
pub(crate) fn clip(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn envelope(kind: &str, payload: &Value) -> String {
    json!({"type": kind, "payload": payload}).to_string()
}

pub(crate) fn client_init(client_id: &str, suite: &str) -> String {
    envelope(
        "client/init",
        &json!({"client_id": client_id, "version": 1, "suite": suite}),
    )
}

pub(crate) fn noise_handshake(message: &[u8]) -> String {
    envelope(
        "noise/handshake",
        &json!({"data": b64::encode_url(message)}),
    )
}

pub(crate) fn client_hello(info: &HelloInfo) -> String {
    envelope(
        "client/hello",
        &json!({
            "name": clip(&info.name, 64),
            "device_info": {
                "product_name": clip(&info.product_name, 64),
                "manufacturer": clip(&info.manufacturer, 64),
                "software_version": clip(&info.software_version, 64),
            },
            "supported_roles": ["player@v1"],
            "player@v1_support": {
                "supported_formats": [info.format],
                "buffer_capacity": info.buffer_capacity,
            },
            "supported_pair_methods": {"pairing_psk": {}},
            "unpaired_access": {"enabled": true},
        }),
    )
}

pub(crate) fn client_time(now_us: i64) -> String {
    envelope("client/time", &json!({"client_transmitted": now_us}))
}

pub(crate) fn client_state(timing: PlayerTiming) -> String {
    envelope(
        "client/state",
        &json!({
            "available": true,
            "player": {
                "output_delay_ms": timing.output_delay_ms.min(5000),
                "required_lead_time_ms": timing.required_lead_time_ms,
                "min_buffer_ms": timing.min_buffer_ms.min(5000),
                "supported_commands": [],
            },
        }),
    )
}

pub(crate) fn client_goodbye(reason: &str) -> String {
    envelope("client/goodbye", &json!({"reason": reason}))
}

pub(crate) fn pair_abort(reason: &str) -> String {
    envelope("pair/abort", &json!({"reason": reason}))
}

/// The envelope of any JSON message.
#[derive(Deserialize)]
pub(crate) struct Envelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub payload: serde_json::Map<String, Value>,
}

pub(crate) fn parse_envelope(text: &[u8]) -> Option<Envelope> {
    serde_json::from_slice(text).ok()
}

/// The `type` of a JSON object, payload or not (the proxy's `auth_ok` has none).
pub(crate) fn parse_envelope_kind(text: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Kind {
        #[serde(rename = "type")]
        kind: String,
    }
    serde_json::from_slice::<Kind>(text).ok().map(|k| k.kind)
}

#[derive(Deserialize)]
pub(crate) struct ServerInit {
    pub server_id: String,
    pub version: Value,
}

#[derive(Deserialize)]
pub(crate) struct NoiseHandshake {
    pub data: String,
}

#[derive(Deserialize)]
pub(crate) struct ServerError {
    pub reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ServerHello {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Activate {
    #[serde(default)]
    pub activities: Vec<String>,
    pub active_roles: Option<Vec<String>>,
    pub pairing: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ServerTime {
    pub client_transmitted: i64,
    pub server_received: i64,
    pub server_transmitted: i64,
}

/// The `player` object of `stream/start`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct StreamFormat {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub bit_depth: u32,
    /// Standard Base64 of the codec header: for FLAC, `fLaC` and the STREAMINFO block.
    pub codec_header: Option<String>,
}

impl StreamFormat {
    /// The decoded codec header.
    pub fn header_bytes(&self) -> Option<Vec<u8>> {
        self.codec_header.as_deref().and_then(b64::decode_standard)
    }

    pub fn format(&self) -> Format {
        Format {
            codec: self.codec.clone(),
            channels: self.channels,
            sample_rate: self.sample_rate,
            bit_depth: self.bit_depth,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamStart {
    pub player: Option<StreamFormat>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamClear {
    pub from_timestamp: Option<i64>,
    pub roles: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamEnd {
    pub roles: Option<Vec<String>>,
}

/// `group/update`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct GroupUpdate {
    pub playback_state: String,
    pub group_id: String,
    #[serde(default)]
    pub group_name: String,
}

impl GroupUpdate {
    pub fn is_playing(&self) -> bool {
        self.playback_state == "playing"
    }
}

pub(crate) fn payload<T: serde::de::DeserializeOwned>(
    payload: serde_json::Map<String, Value>,
) -> Option<T> {
    serde_json::from_value(Value::Object(payload)).ok()
}

/// Whether a `roles` list (absent means all) names the player.
pub(crate) fn targets_player(roles: Option<&[String]>) -> bool {
    roles.is_none_or(|roles| roles.iter().any(|r| r == "player"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> HelloInfo {
        HelloInfo {
            name: "Luke's iPhone (Tracks)".into(),
            product_name: "Mobile Application".into(),
            manufacturer: "Tracks".into(),
            software_version: "0.1.0".into(),
            format: Format::flac_cd(),
            buffer_capacity: 1 << 20,
        }
    }

    #[test]
    fn hello_has_no_client_id_or_version_and_offers_unpaired_access() {
        let hello: Value = serde_json::from_str(&client_hello(&info())).expect("json");
        let payload = &hello["payload"];
        assert_eq!(hello["type"], "client/hello");
        assert!(payload.get("client_id").is_none());
        assert!(payload.get("version").is_none());
        assert_eq!(payload["unpaired_access"], json!({"enabled": true}));
        assert_eq!(
            payload["supported_pair_methods"],
            json!({"pairing_psk": {}})
        );
        assert_eq!(payload["supported_roles"], json!(["player@v1"]));
        assert_eq!(
            payload["player@v1_support"],
            json!({"supported_formats": [{"codec": "flac", "channels": 2, "sample_rate": 44100, "bit_depth": 16}],
                   "buffer_capacity": 1_048_576})
        );
        assert_eq!(payload["device_info"]["product_name"], "Mobile Application");
    }

    #[test]
    fn names_are_clipped_to_64_bytes_on_a_character_boundary() {
        let long = "é".repeat(40);
        let clipped = clip(&long, 64);
        assert_eq!(clipped.len(), 64);
        assert_eq!(clip("ab", 64), "ab");
        assert_eq!(clip("aé", 2), "a");
    }

    #[test]
    fn state_carries_every_required_player_field() {
        let state: Value = serde_json::from_str(&client_state(PlayerTiming {
            output_delay_ms: 9000,
            required_lead_time_ms: 1000,
            min_buffer_ms: 300,
        }))
        .expect("json");
        assert_eq!(
            state["payload"],
            json!({"available": true, "player": {"output_delay_ms": 5000, "required_lead_time_ms": 1000,
                   "min_buffer_ms": 300, "supported_commands": []}})
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let env =
            parse_envelope(br#"{"type":"stream/clear","payload":{"from_timestamp":5,"extra":1}}"#)
                .expect("envelope");
        let clear: StreamClear = payload(env.payload).expect("clear");
        assert_eq!(clear.from_timestamp, Some(5));
    }

    #[test]
    fn a_message_without_a_payload_object_is_not_an_envelope() {
        assert!(parse_envelope(br#"{"type":"x","payload":[]}"#).is_none());
        assert!(parse_envelope(br#"{"payload":{}}"#).is_none());
    }

    #[test]
    fn only_listed_roles_are_targeted() {
        assert!(targets_player(None));
        assert!(targets_player(Some(&["player".to_owned()])));
        assert!(!targets_player(Some(&["visualizer".to_owned()])));
    }
}
