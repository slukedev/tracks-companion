//! `ping`, `getLicense`, `getOpenSubsonicExtensions` and `tokenInfo`.

use crate::response::{Body, Node, Value};
use crate::{Call, Reply};

/// The `OpenSubsonic` extensions advertised, with their versions (api.md section 17).
pub const EXTENSIONS: [(&str, u32); 7] = [
    ("apiKeyAuthentication", 1),
    ("formPost", 1),
    ("songLyrics", 1),
    ("transcodeOffset", 1),
    ("indexBasedQueue", 1),
    ("topSongsByArtistId", 1),
    ("playbackReport", 1),
];

pub fn ping() -> Reply {
    Body::Empty.into()
}

pub fn license() -> Reply {
    Body::one(
        "license",
        Node::new()
            .attr("valid", true)
            .attr("email", "")
            .attr("licenseExpires", "2099-12-31T23:59:59.000Z"),
    )
    .into()
}

/// Answered without authentication, as `OpenSubsonic` requires.
pub fn extensions() -> Body {
    Body::many(
        "openSubsonicExtensions",
        EXTENSIONS
            .iter()
            .map(|(name, version)| {
                Node::new()
                    .attr("name", *name)
                    .texts("versions", vec![Value::from(*version)])
            })
            .collect(),
    )
}

pub fn token_info(call: &Call<'_>) -> Reply {
    Body::one(
        "tokenInfo",
        Node::new().attr("username", call.principal.username.as_str()),
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::{Format, render};

    #[test]
    fn extensions_match_golden() {
        insta::assert_snapshot!(render(&extensions(), Format::Json), @r#"{"subsonic-response":{"openSubsonic":true,"openSubsonicExtensions":[{"name":"apiKeyAuthentication","versions":[1]},{"name":"formPost","versions":[1]},{"name":"songLyrics","versions":[1]},{"name":"transcodeOffset","versions":[1]},{"name":"indexBasedQueue","versions":[1]},{"name":"topSongsByArtistId","versions":[1]},{"name":"playbackReport","versions":[1]}],"serverVersion":"0.1.0","status":"ok","type":"trackside","version":"1.16.1"}}"#);
        insta::assert_snapshot!(render(&extensions(), Format::Xml), @r#"<?xml version="1.0" encoding="UTF-8"?><subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="trackside" serverVersion="0.1.0" openSubsonic="true"><openSubsonicExtensions name="apiKeyAuthentication"><versions>1</versions></openSubsonicExtensions><openSubsonicExtensions name="formPost"><versions>1</versions></openSubsonicExtensions><openSubsonicExtensions name="songLyrics"><versions>1</versions></openSubsonicExtensions><openSubsonicExtensions name="transcodeOffset"><versions>1</versions></openSubsonicExtensions><openSubsonicExtensions name="indexBasedQueue"><versions>1</versions></openSubsonicExtensions><openSubsonicExtensions name="topSongsByArtistId"><versions>1</versions></openSubsonicExtensions><openSubsonicExtensions name="playbackReport"><versions>1</versions></openSubsonicExtensions></subsonic-response>"#);
    }
}
