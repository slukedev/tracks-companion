//! The WebSocket envelope (models `api.py`). Client to server: `{message_id, command, args}`.
//! Server to client: the server info (no `message_id`), `{message_id, result, partial}`,
//! `{message_id, error_code, details}` or `{event, object_id, data}`.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::{ErrorCode, ServerError};
use crate::event::RawEvent;
use crate::models::ServerInfo;

#[derive(Serialize)]
pub(crate) struct CommandFrame<'a> {
    pub message_id: &'a str,
    pub command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<&'a Value>,
}

#[derive(Debug)]
pub(crate) enum Incoming {
    Result {
        message_id: String,
        result: Value,
        partial: bool,
    },
    Error {
        message_id: String,
        error: ServerError,
    },
    Event(RawEvent),
    ServerInfo(Box<ServerInfo>),
    Unknown,
}

pub(crate) fn parse(text: &str) -> Result<Incoming, serde_json::Error> {
    let mut map: Map<String, Value> = serde_json::from_str(text)?;
    if let Some(event) = map.remove("event") {
        return Ok(Incoming::Event(RawEvent {
            event: match event {
                Value::String(s) => s,
                other => other.to_string(),
            },
            object_id: match map.remove("object_id") {
                Some(Value::String(s)) => Some(s),
                _ => None,
            },
            data: map.remove("data").unwrap_or(Value::Null),
        }));
    }
    let message_id = match map.get("message_id") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    if let Some(code) = map.get("error_code") {
        let code = code.as_i64().unwrap_or(0);
        let details = match map.remove("details") {
            Some(Value::String(s)) => Some(s),
            _ => None,
        };
        let data = map.remove("data").filter(Value::is_object);
        return Ok(Incoming::Error {
            message_id: message_id.unwrap_or_default(),
            error: ServerError {
                code: ErrorCode::from_code(code),
                details,
                data,
            },
        });
    }
    if let (Some(message_id), Some(result)) = (message_id, map.remove("result")) {
        let partial = map.get("partial").and_then(Value::as_bool).unwrap_or(false);
        return Ok(Incoming::Result {
            message_id,
            result,
            partial,
        });
    }
    if map.contains_key("server_version") || map.contains_key("server_id") {
        return Ok(
            serde_json::from_value(Value::Object(map)).map_or(Incoming::Unknown, |info| {
                Incoming::ServerInfo(Box::new(info))
            }),
        );
    }
    Ok(Incoming::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_result_is_still_a_result() {
        let parsed = parse(r#"{"message_id":"4","result":null}"#).expect("parses");
        assert!(matches!(
            parsed,
            Incoming::Result {
                result: Value::Null,
                partial: false,
                ..
            }
        ));
    }

    #[test]
    fn errors_carry_their_code() {
        let parsed =
            parse(r#"{"message_id":"4","error_code":23,"details":"Invalid or expired token"}"#)
                .expect("parses");
        let Incoming::Error { error, .. } = parsed else {
            panic!("not an error")
        };
        assert_eq!(error.code, ErrorCode::InvalidToken);
        assert_eq!(error.data, None);
    }

    #[test]
    fn a_held_stream_names_its_holder() {
        let parsed = parse(
            r#"{"message_id":"9","error_code":29,"details":"Spotify is already playing on Kitchen",
                "data":{"queue_id":"kitchen","player_name":"Kitchen","provider_instance":"spotify--a"}}"#,
        )
        .expect("parses");
        let Incoming::Error { error, .. } = parsed else {
            panic!("not an error")
        };
        assert_eq!(error.code, ErrorCode::ProviderStreamHeld);
        assert_eq!(
            error.data.as_ref().and_then(|d| d["queue_id"].as_str()),
            Some("kitchen")
        );
    }

    #[test]
    fn events_win_over_other_keys() {
        let parsed =
            parse(r#"{"event":"player_updated","object_id":"p1","data":{}}"#).expect("parses");
        assert!(matches!(parsed, Incoming::Event(e) if e.event == "player_updated"));
    }

    #[test]
    fn something_else_is_unknown() {
        assert!(matches!(
            parse(r#"{"hello":1}"#).expect("parses"),
            Incoming::Unknown
        ));
    }
}
