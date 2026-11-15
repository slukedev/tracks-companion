//! Reading credentials from requests, the CSRF and Origin check, and query string redaction.

use std::borrow::Cow;

use http::request::Parts;
use http::{Method, Uri};
use percent_encoding::percent_decode_str;

use crate::AuthError;

/// The session cookie over HTTPS (and from non-local addresses).
pub const SESSION_COOKIE: &str = "__Host-trackside_session";
/// The session cookie over plain HTTP from a private or loopback address, where browsers
/// refuse `Secure` and the `__Host-` prefix.
pub const PLAIN_SESSION_COOKIE: &str = "trackside_session";
/// A request that arrived over HTTPS (directly, or through a trusted proxy that said so).
/// The server inserts this extension; without it requests count as plain HTTP.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Https;

/// The header session requests that change state must carry, with the value `1`.
pub const CSRF_HEADER: &str = "x-trackside-csrf";

/// Query parameters whose values are credentials, matched without regard to case. They are
/// redacted from logs, and outside media routes their presence is an error.
const SECRET_PARAMS: &[&str] = &[
    "mt",
    "apikey",
    "api_key",
    "t",
    "s",
    "p",
    "token",
    "access_token",
    "key",
    "password",
    "poll_secret",
    "setup_token",
];

/// Parameters that would carry a native credential, refused in query strings off media
/// routes. Subsonic's `t`, `s` and `p` are not here: `/rest` takes them by design.
const NATIVE_CREDENTIAL_PARAMS: &[&str] =
    &["mt", "apikey", "api_key", "token", "access_token", "key"];

/// `Authorization: Bearer <token>`.
pub(crate) fn bearer(parts: &Parts) -> Option<&str> {
    let value = parts
        .headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// The session cookie value, preferring the `__Host-` cookie.
pub(crate) fn session_cookie(parts: &Parts) -> Option<&str> {
    // Over HTTPS only the `__Host-` cookie counts: the plain one can be planted by anyone
    // who can answer the browser's plain HTTP requests, fixing a session they know.
    let over_tls = parts.extensions.get::<Https>().is_some();
    let mut plain = None;
    for header in parts.headers.get_all(http::header::COOKIE) {
        let Ok(header) = header.to_str() else {
            continue;
        };
        for pair in header.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches('"');
            if value.is_empty() {
                continue;
            }
            if name == SESSION_COOKIE {
                return Some(value);
            }
            if name == PLAIN_SESSION_COOKIE && !over_tls {
                plain = Some(value);
            }
        }
    }
    plain
}

/// Query pairs, percent-decoded, `+` read as a space.
pub(crate) fn query_pairs(query: &str) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, str>)> {
    query.split('&').filter(|p| !p.is_empty()).map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (decode(name), decode(value))
    })
}

fn decode(text: &str) -> Cow<'_, str> {
    if text.contains('+') {
        Cow::Owned(
            percent_decode_str(&text.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned(),
        )
    } else {
        percent_decode_str(text).decode_utf8_lossy()
    }
}

pub(crate) fn query_param(parts: &Parts, name: &str) -> Option<String> {
    query_pairs(parts.uri.query()?)
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.into_owned())
}

/// Whether the query string carries something that looks like a native credential.
pub(crate) fn credential_in_query(parts: &Parts) -> bool {
    let Some(query) = parts.uri.query() else {
        return false;
    };
    query_pairs(query).any(|(name, value)| {
        NATIVE_CREDENTIAL_PARAMS
            .iter()
            .any(|p| name.eq_ignore_ascii_case(p))
            || value.starts_with(crate::token::KEY_PREFIX)
    })
}

fn changes_state(method: &Method) -> bool {
    !matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    )
}

/// For a session request that changes state: the CSRF header must be `1` and `Origin` must
/// name the host the request was sent to (`forwarded_host` when a trusted proxy set one).
pub(crate) fn check_csrf(parts: &Parts, forwarded_host: Option<&str>) -> Result<(), AuthError> {
    if !changes_state(&parts.method) {
        return Ok(());
    }
    let header_ok = parts
        .headers
        .get(CSRF_HEADER)
        .is_some_and(|v| v.as_bytes() == b"1");
    if !header_ok {
        return Err(AuthError::Csrf);
    }
    let origin = parts
        .headers
        .get(http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<Uri>().ok())
        .filter(|uri| matches!(uri.scheme_str(), Some("http" | "https")))
        .and_then(|uri| uri.authority().map(|a| a.as_str().to_ascii_lowercase()))
        .ok_or(AuthError::Csrf)?;
    let host = forwarded_host
        .map(str::to_owned)
        .or_else(|| {
            parts
                .headers
                .get(http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        })
        .or_else(|| parts.uri.authority().map(|a| a.as_str().to_owned()))
        .ok_or(AuthError::Csrf)?
        .to_ascii_lowercase();
    if same_authority(&origin, &host) {
        Ok(())
    } else {
        Err(AuthError::Csrf)
    }
}

/// Compares `host[:port]`, treating a missing port as matching the default port either way.
fn same_authority(origin: &str, host: &str) -> bool {
    if origin == host {
        return true;
    }
    let strip = |a: &str| -> String {
        a.strip_suffix(":80")
            .or_else(|| a.strip_suffix(":443"))
            .unwrap_or(a)
            .to_owned()
    };
    strip(origin) == strip(host)
}

/// The query string with every credential value replaced by `REDACTED`, for logs.
pub fn redact_query(query: &str) -> Cow<'_, str> {
    let secret = |pair: &str| {
        let name = pair.split_once('=').map_or(pair, |(n, _)| n);
        let name = percent_decode_str(name).decode_utf8_lossy();
        SECRET_PARAMS.iter().any(|p| name.eq_ignore_ascii_case(p))
    };
    if !query.split('&').any(secret) {
        return Cow::Borrowed(query);
    }
    Cow::Owned(
        query
            .split('&')
            .map(|pair| {
                if secret(pair) {
                    let name = pair.split_once('=').map_or(pair, |(n, _)| n);
                    format!("{name}=REDACTED")
                } else {
                    pair.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("&"),
    )
}

/// The path and query of `uri` with credentials redacted, for logs.
pub fn redact_uri(uri: &Uri) -> String {
    let path = redact_path(uri.path());
    match uri.query() {
        Some(query) => format!("{path}?{}", redact_query(query)),
        None => path,
    }
}

/// A path with the secret segment of token pages and token endpoints replaced:
/// `/invite/<token>`, `/reset/<token>`, `/api/v1/invites/<token>[/accept]` and
/// `/api/v1/password-resets/<token>`.
pub fn redact_path(path: &str) -> String {
    let mut out = Vec::new();
    let mut hide_next = false;
    for segment in path.split('/') {
        if hide_next && !segment.is_empty() {
            out.push("REDACTED");
            hide_next = false;
            continue;
        }
        hide_next = matches!(segment, "invite" | "reset" | "password-resets")
            || (segment == "invites" && !path.contains("/admin/"));
        out.push(segment);
    }
    out.join("/")
}

/// A user name as it appears in the fail2ban line: printable, no spaces, at most 64
/// characters, so a crafted name cannot forge log fields.
pub(crate) fn log_safe(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .take(64)
        .map(|c| {
            if c.is_control() || c.is_whitespace() || c == '=' {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned.is_empty() {
        String::from("-")
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_paths_are_redacted() {
        assert_eq!(redact_path("/invite/abc123"), "/invite/REDACTED");
        assert_eq!(redact_path("/base/reset/abc123"), "/base/reset/REDACTED");
        assert_eq!(
            redact_path("/api/v1/invites/abc123/accept"),
            "/api/v1/invites/REDACTED/accept"
        );
        assert_eq!(
            redact_path("/api/v1/password-resets/xyz"),
            "/api/v1/password-resets/REDACTED"
        );
        assert_eq!(
            redact_path("/api/v1/admin/invites/i123"),
            "/api/v1/admin/invites/i123"
        );
        assert_eq!(redact_path("/invite.js"), "/invite.js");
        assert_eq!(redact_path("/api/v1/songs/s1"), "/api/v1/songs/s1");
    }

    fn parts(method: Method, uri: &str, headers: &[(&'static str, &'static str)]) -> Parts {
        let mut builder = http::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let (parts, ()) = builder
            .body(())
            .unwrap_or_else(|_| http::Request::new(()))
            .into_parts();
        parts
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        let p = parts(Method::GET, "/", &[("authorization", "bearer tsk_abc")]);
        assert_eq!(bearer(&p), Some("tsk_abc"));
        let basic = parts(Method::GET, "/", &[("authorization", "Basic dXNlcg==")]);
        assert_eq!(bearer(&basic), None);
    }

    #[test]
    fn host_prefixed_session_cookie_wins() {
        let p = parts(
            Method::GET,
            "/",
            &[(
                "cookie",
                "trackside_session=plain; theme=dark; __Host-trackside_session=secure",
            )],
        );
        assert_eq!(session_cookie(&p), Some("secure"));
        let only_plain = parts(Method::GET, "/", &[("cookie", "trackside_session=plain")]);
        assert_eq!(session_cookie(&only_plain), Some("plain"));
    }

    #[test]
    fn the_plain_session_cookie_is_ignored_over_https() {
        let mut planted = parts(Method::GET, "/", &[("cookie", "trackside_session=planted")]);
        planted.extensions.insert(Https);
        assert_eq!(session_cookie(&planted), None);
    }

    #[test]
    fn credentials_in_query_strings_are_spotted() {
        for uri in [
            "/api/v1/me?apiKey=x",
            "/api/v1/me?API_KEY=x",
            "/api/v1/me?mt=abc",
            "/api/v1/me?q=1&token=x",
            "/api/v1/me?anything=tsk_secret",
            "/api/v1/me?anything=tsk%5Fsecret",
        ] {
            assert!(credential_in_query(&parts(Method::GET, uri, &[])), "{uri}");
        }
        assert!(!credential_in_query(&parts(
            Method::GET,
            "/api/v1/search?q=tsk&limit=5",
            &[]
        )));
    }

    #[test]
    fn safe_methods_skip_csrf() {
        assert!(check_csrf(&parts(Method::GET, "/", &[]), None).is_ok());
    }

    #[test]
    fn session_write_without_csrf_header_is_rejected() {
        let p = parts(
            Method::POST,
            "/",
            &[
                ("host", "music.local:4747"),
                ("origin", "http://music.local:4747"),
            ],
        );
        assert!(matches!(check_csrf(&p, None), Err(AuthError::Csrf)));
    }

    #[test]
    fn foreign_origin_is_rejected() {
        let p = parts(
            Method::DELETE,
            "/",
            &[
                ("host", "music.local:4747"),
                ("origin", "https://evil.example"),
                (CSRF_HEADER, "1"),
            ],
        );
        assert!(matches!(check_csrf(&p, None), Err(AuthError::Csrf)));
        let null = parts(
            Method::POST,
            "/",
            &[
                ("host", "music.local"),
                ("origin", "null"),
                (CSRF_HEADER, "1"),
            ],
        );
        assert!(matches!(check_csrf(&null, None), Err(AuthError::Csrf)));
        let missing = parts(
            Method::POST,
            "/",
            &[("host", "music.local"), (CSRF_HEADER, "1")],
        );
        assert!(matches!(check_csrf(&missing, None), Err(AuthError::Csrf)));
    }

    #[test]
    fn matching_origin_with_header_passes() {
        let p = parts(
            Method::PATCH,
            "/",
            &[
                ("host", "Music.Local:4747"),
                ("origin", "http://music.local:4747"),
                (CSRF_HEADER, "1"),
            ],
        );
        assert!(check_csrf(&p, None).is_ok());
        let proxied = parts(
            Method::POST,
            "/",
            &[
                ("host", "127.0.0.1:4747"),
                ("origin", "https://music.example.com"),
                (CSRF_HEADER, "1"),
            ],
        );
        assert!(check_csrf(&proxied, Some("music.example.com")).is_ok());
        assert!(check_csrf(&proxied, None).is_err());
    }

    #[test]
    fn redaction_hides_every_credential() {
        let query = "u=luke&t=abc&s=salt&p=enc:7365&apiKey=k&mt=tok&id=s1&f=json";
        let redacted = redact_query(query);
        assert_eq!(
            redacted,
            "u=luke&t=REDACTED&s=REDACTED&p=REDACTED&apiKey=REDACTED&mt=REDACTED&id=s1&f=json"
        );
        assert!(matches!(redact_query("id=s1&f=json"), Cow::Borrowed(_)));
        let uri: Uri = "/rest/stream.view?id=s1&T=x".parse().unwrap_or_default();
        assert_eq!(redact_uri(&uri), "/rest/stream.view?id=s1&T=REDACTED");
    }

    #[test]
    fn log_names_cannot_forge_fields() {
        assert_eq!(log_safe("luke ip=1.2.3.4\n"), "luke_ip_1.2.3.4_");
        assert_eq!(log_safe(""), "-");
    }
}
