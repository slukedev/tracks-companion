//! Which Music Assistant API schemas this client works with.
//!
//! Music Assistant raises `schema_version` whenever it adds to the API and
//! `min_supported_schema_version` only when it breaks older clients. The models mark the
//! fields added in schema 32 as "MA v2.10", stable 2.10.13 sends 65 and the 2.11 betas send
//! 128, all with a minimum of 28.

use crate::error::{Incompatibility, Mismatch};
use crate::models::ServerInfo;

/// The oldest server schema this client works with: Music Assistant 2.10.
pub const MIN_SERVER_SCHEMA: u32 = 32;

/// The schema this client was written against (Music Assistant 2.10.13). A server that no
/// longer supports clients of this schema is refused.
pub const CLIENT_SCHEMA: u32 = 65;

/// The newest schema this client has been checked against (Music Assistant 2.11 beta). Newer
/// servers are still used; the difference is only logged.
pub const TESTED_SCHEMA: u32 = 128;

/// How a compatible server relates to what was tested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compatibility {
    Tested,
    /// Newer than [`TESTED_SCHEMA`]; decoding is tolerant, so this should still work.
    Newer,
}

pub fn check(info: &ServerInfo) -> Result<Compatibility, Incompatibility> {
    let mismatch = if info.schema_version < MIN_SERVER_SCHEMA {
        Some(Mismatch::ServerTooOld)
    } else if info.min_supported_schema_version > CLIENT_SCHEMA {
        Some(Mismatch::ClientTooOld)
    } else {
        None
    };
    match mismatch {
        Some(mismatch) => Err(Incompatibility {
            server_version: info.server_version.clone(),
            schema_version: info.schema_version,
            min_supported_schema_version: info.min_supported_schema_version,
            mismatch,
        }),
        None if info.schema_version > TESTED_SCHEMA => Ok(Compatibility::Newer),
        None => Ok(Compatibility::Tested),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(schema: u32, min: u32) -> ServerInfo {
        serde_json::from_value(serde_json::json!({
            "server_id": "x",
            "server_version": "2.10.13",
            "schema_version": schema,
            "min_supported_schema_version": min,
        }))
        .expect("decodes")
    }

    #[test]
    fn stable_and_beta_are_supported() {
        assert_eq!(check(&info(65, 28)), Ok(Compatibility::Tested));
        assert_eq!(check(&info(128, 28)), Ok(Compatibility::Tested));
    }

    #[test]
    fn a_newer_server_is_used() {
        assert_eq!(check(&info(140, 28)), Ok(Compatibility::Newer));
    }

    #[test]
    fn a_server_older_than_2_10_is_refused() {
        let err = check(&info(29, 28)).expect_err("too old");
        assert_eq!(err.mismatch, Mismatch::ServerTooOld);
    }

    #[test]
    fn a_server_that_dropped_our_schema_is_refused() {
        let err = check(&info(200, 150)).expect_err("client too old");
        assert_eq!(err.mismatch, Mismatch::ClientTooOld);
    }
}
