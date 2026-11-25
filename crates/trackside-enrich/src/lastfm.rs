//! A read-only Last.fm client: six unsigned methods that take only the API key and artist,
//! track or album names. Nothing here signs a request, holds a session or writes to Last.fm.

use std::fmt::Write as _;
use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Map, Value};
use trackside_core::repo::enrichment::Related;

pub const ENDPOINT: &str = "https://ws.audioscrobbler.com/2.0/";

/// A biography summary is cut to about this many characters, at a sentence or word end.
const BIO_MAX: usize = 600;

/// Why a call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LastFmError {
    #[error("Last.fm error {code}: {message}")]
    Service { code: i64, message: String },
    #[error("Last.fm answered HTTP {status}")]
    Http { status: u16 },
    #[error("Last.fm's answer could not be read")]
    Malformed,
    #[error("could not reach Last.fm: {0}")]
    Transport(String),
}

impl LastFmError {
    /// 6: no such artist or track.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Service { code: 6, .. })
    }

    /// 10 invalid key, 26 suspended key, 4 authentication failed: no call can work.
    pub fn is_key_problem(&self) -> bool {
        matches!(
            self,
            Self::Service {
                code: 4 | 10 | 26,
                ..
            }
        )
    }

    /// 29 rate limit, 8 operation failed, 11 and 16 temporarily unavailable, HTTP 429 and
    /// 5xx, and transport failures.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Service { code, .. } => matches!(code, 8 | 11 | 16 | 29),
            Self::Http { status } => *status == 429 || *status >= 500,
            Self::Transport(_) => true,
            Self::Malformed => false,
        }
    }
}

/// One of the methods this client calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    ArtistSimilar,
    ArtistTopTracks,
    ArtistInfo,
    TrackSimilar,
    TrackInfo,
    AlbumInfo,
}

impl Method {
    pub const fn name(self) -> &'static str {
        match self {
            Self::ArtistSimilar => "artist.getSimilar",
            Self::ArtistTopTracks => "artist.getTopTracks",
            Self::ArtistInfo => "artist.getInfo",
            Self::TrackSimilar => "track.getSimilar",
            Self::TrackInfo => "track.getInfo",
            Self::AlbumInfo => "album.getInfo",
        }
    }
}

/// The URL for `method` with `params` (artist and track names, limits).
pub fn url(endpoint: &str, key: &str, method: Method, params: &[(&str, &str)]) -> String {
    let mut out = format!("{endpoint}?method={}", method.name());
    for (name, value) in
        params
            .iter()
            .chain(&[("autocorrect", "1"), ("api_key", key), ("format", "json")])
    {
        let _ = write!(
            out,
            "&{name}={}",
            utf8_percent_encode(value, NON_ALPHANUMERIC)
        );
    }
    out
}

/// Sends one call. Errors never carry the URL, which holds the key.
pub async fn get(
    http: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<Map<String, Value>, LastFmError> {
    let transport = |e: reqwest::Error| LastFmError::Transport(e.without_url().to_string());
    let response = http
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(transport)?;
    let status = response.status().as_u16();
    let body = response.bytes().await.map_err(transport)?;
    object(status, &body)
}

/// The body as a JSON object, or the error it stands for. Last.fm sends its errors with a 4xx
/// and sometimes with a 200, so the body is read first.
pub fn object(status: u16, body: &[u8]) -> Result<Map<String, Value>, LastFmError> {
    let json = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(object)) => Some(object),
        _ => None,
    };
    if let Some(code) = json.as_ref().and_then(|j| integer(j.get("error"))) {
        let message = json
            .as_ref()
            .and_then(|j| j.get("message")?.as_str())
            .unwrap_or_default()
            .to_owned();
        return Err(LastFmError::Service { code, message });
    }
    if !(200..300).contains(&status) {
        return Err(LastFmError::Http { status });
    }
    json.ok_or(LastFmError::Malformed)
}

fn integer(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::String(text) => text.trim().parse().ok(),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

fn float(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::String(text) => text.trim().parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Last.fm turns XML into JSON: a list of one is a lone object, a list of none is missing or
/// an empty string.
fn list(value: Option<&Value>) -> Vec<&Map<String, Value>> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_object).collect(),
        Some(Value::Object(one)) => vec![one],
        _ => Vec::new(),
    }
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    let value = match object.get(key)? {
        Value::String(text) => text.as_str(),
        Value::Object(inner) => inner.get("name").or_else(|| inner.get("#text"))?.as_str()?,
        _ => return None,
    };
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn section<'a>(json: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    json.get(key)?.as_object()
}

/// `artist.getSimilar`: similar artists by name, with their match score, best first.
pub fn similar_artists(json: &Map<String, Value>) -> Vec<Related> {
    let mut out: Vec<Related> = list(section(json, "similarartists").and_then(|s| s.get("artist")))
        .into_iter()
        .filter_map(|entry| {
            Some(Related {
                title: String::new(),
                artist: text(entry, "name")?.to_owned(),
                score: float(entry.get("match")).unwrap_or(0.0).clamp(0.0, 1.0),
            })
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

/// `track.getSimilar`: similar songs with their match score, best first.
pub fn similar_tracks(json: &Map<String, Value>) -> Vec<Related> {
    let mut out: Vec<Related> = list(section(json, "similartracks").and_then(|s| s.get("track")))
        .into_iter()
        .filter_map(|entry| {
            Some(Related {
                title: text(entry, "name")?.to_owned(),
                artist: text(entry, "artist")?.to_owned(),
                score: float(entry.get("match")).unwrap_or(0.0).clamp(0.0, 1.0),
            })
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

/// `artist.getTopTracks`: song titles, most played first.
pub fn top_tracks(json: &Map<String, Value>) -> Vec<String> {
    let mut ranked: Vec<(i64, usize, String)> =
        list(section(json, "toptracks").and_then(|s| s.get("track")))
            .into_iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let rank = section(entry, "@attr")
                    .and_then(|a| integer(a.get("rank")))
                    .unwrap_or(i64::MAX);
                Some((rank, i, text(entry, "name")?.to_owned()))
            })
            .collect();
    ranked.sort();
    let mut seen = std::collections::HashSet::new();
    ranked
        .into_iter()
        .map(|(_, _, title)| title)
        .filter(|title| seen.insert(title.to_lowercase()))
        .collect()
}

/// The image Last.fm shows when it has none: a grey star, by this file name.
pub const PLACEHOLDER_IMAGE: &str = "2a96cbd8b46e442fc41c2b86b821562f";

/// The image sizes Last.fm names, largest first.
const IMAGE_SIZES: [&str; 5] = ["mega", "extralarge", "large", "medium", "small"];

/// The largest real image in an `image` list: no blank URL, no placeholder star.
fn best_image(object: &Map<String, Value>) -> Option<String> {
    let images = list(object.get("image"));
    IMAGE_SIZES.iter().find_map(|size| {
        images
            .iter()
            .filter(|i| i.get("size").and_then(Value::as_str) == Some(size))
            .find_map(|i| usable_image(i.get("#text")?.as_str()?))
    })
}

/// `url` when it names a real image.
pub fn usable_image(url: &str) -> Option<String> {
    let url = url.trim();
    (!url.is_empty() && !url.to_ascii_lowercase().contains(PLACEHOLDER_IMAGE))
        .then(|| url.to_owned())
}

/// What `track.getInfo` says about a song's cover: its album's image, and the album's title
/// and artist to ask `album.getInfo` with when the image is missing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackCover {
    pub image: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
}

/// `track.getInfo`: the song's album cover.
pub fn track_cover(json: &Map<String, Value>) -> TrackCover {
    let Some(album) = section(json, "track").and_then(|t| section(t, "album")) else {
        return TrackCover::default();
    };
    TrackCover {
        image: best_image(album),
        album: text(album, "title").map(str::to_owned),
        album_artist: text(album, "artist").map(str::to_owned),
    }
}

/// `album.getInfo`: the album's cover.
pub fn album_cover(json: &Map<String, Value>) -> Option<String> {
    best_image(section(json, "album")?)
}

/// `artist.getInfo`: the biography summary as short plain text.
pub fn artist_bio(json: &Map<String, Value>) -> Option<String> {
    let summary = section(json, "artist")
        .and_then(|a| section(a, "bio"))
        .and_then(|b| b.get("summary")?.as_str())?;
    plain_summary(summary)
}

/// Last.fm's HTML summary as plain text: the "Read more on Last.fm" link, tags and entities
/// gone, whitespace collapsed, cut to [`BIO_MAX`] characters.
pub fn plain_summary(html: &str) -> Option<String> {
    let before_link = html.rfind("<a href=").map_or(html, |at| &html[..at]);
    let mut stripped = String::with_capacity(before_link.len());
    let mut in_tag = false;
    for c in before_link.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => stripped.push(c),
            _ => {}
        }
    }
    for (entity, plain) in [
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&nbsp;", " "),
        ("&amp;", "&"),
    ] {
        stripped = stripped.replace(entity, plain);
    }
    let text = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = shorten(&text, BIO_MAX);
    (!text.is_empty()).then_some(text)
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max).collect();
    if let Some(end) = cut.rfind(". ").filter(|&end| end > max / 2) {
        return cut[..=end].to_owned();
    }
    let word = cut.rfind(' ').unwrap_or(cut.len());
    format!("{}...", cut[..word].trim_end_matches([',', ';', ':']))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_carry_only_names_and_the_key() {
        let url = url(
            "http://x/2.0/",
            "k3y",
            Method::TrackSimilar,
            &[
                ("artist", "Beyoncé"),
                ("track", "Halo & more"),
                ("limit", "50"),
            ],
        );
        assert_eq!(
            url,
            "http://x/2.0/?method=track.getSimilar&artist=Beyonc%C3%A9&track=Halo%20%26%20more\
             &limit=50&autocorrect=1&api_key=k3y&format=json"
        );
    }

    #[test]
    fn errors_are_classified() {
        let service = |code| LastFmError::Service {
            code,
            message: String::new(),
        };
        assert!(service(29).is_retryable());
        assert!(service(6).is_unknown() && !service(6).is_retryable());
        assert!(service(10).is_key_problem() && !service(10).is_retryable());
        assert!(LastFmError::Http { status: 503 }.is_retryable());
        assert!(!LastFmError::Http { status: 404 }.is_retryable());
    }

    #[test]
    fn errors_in_a_200_are_still_errors() {
        assert_eq!(
            object(200, br#"{"error":"29","message":"Rate Limit Exceeded"}"#),
            Err(LastFmError::Service {
                code: 29,
                message: String::from("Rate Limit Exceeded")
            })
        );
        assert_eq!(
            object(200, b"<html>portal</html>"),
            Err(LastFmError::Malformed)
        );
        assert_eq!(object(502, b""), Err(LastFmError::Http { status: 502 }));
    }

    #[test]
    fn summaries_lose_markup_and_stay_short() {
        assert_eq!(
            plain_summary(
                "Rock &amp; roll <b>band</b>.\n\n <a href=\"x\">Read more on Last.fm</a>"
            ),
            Some(String::from("Rock & roll band."))
        );
        assert_eq!(
            plain_summary(" <a href=\"x\">Read more on Last.fm</a>"),
            None
        );
        let long = "Word ".repeat(400);
        let short = plain_summary(&long).unwrap_or_default();
        assert!(short.chars().count() <= BIO_MAX + 3 && short.ends_with("..."));
    }
}
