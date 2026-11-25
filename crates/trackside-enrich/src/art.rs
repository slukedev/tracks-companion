//! Cover images for suggested songs: which URLs may be fetched, what id an image gets, and the
//! download itself. Only Last.fm's own image hosts are ever asked, over HTTPS, without
//! following redirects, and nothing larger than [`MAX_BYTES`] is read.

use std::time::Duration;

use trackside_core::ExternalArtId;

/// Last.fm's image hosts, as origins.
pub const IMAGE_HOSTS: [&str; 3] = [
    "https://lastfm.freetls.fastly.net",
    "https://lastfm-img2.akamaized.net",
    "https://lastfm.akamaized.net",
];

/// The largest image downloaded.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

/// Why an image was not fetched.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArtError {
    #[error("image host not allowed")]
    Host,
    #[error("image answered HTTP {0}")]
    Http(u16),
    #[error("not an image: {0}")]
    NotImage(String),
    #[error("image larger than {MAX_BYTES} bytes")]
    TooLarge,
    #[error("could not fetch the image: {0}")]
    Transport(String),
}

impl ArtError {
    /// Worth asking again soon: the host was unreachable or busy, not the image wrong.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Http(status) => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

/// `url` parsed, when its origin is one of `hosts` (scheme, host and port exactly). No user
/// name, password or fragment is allowed.
pub fn allowed(url: &str, hosts: &[String]) -> Option<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let origin = parsed.origin().ascii_serialization();
    hosts
        .iter()
        .any(|h| h.trim_end_matches('/').eq_ignore_ascii_case(&origin))
        .then_some(parsed)
}

/// The image's id: from its file name at Last.fm (a hash, the same at every size), or the
/// whole path when it has none.
pub fn image_id(url: &reqwest::Url) -> ExternalArtId {
    let path = url.path();
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
    if stem.len() >= 16 && stem.bytes().all(|b| b.is_ascii_hexdigit()) {
        ExternalArtId::derive(&stem)
    } else {
        ExternalArtId::derive(path)
    }
}

/// The client images are fetched with: no redirects, so an allowed host cannot send it
/// elsewhere.
pub fn client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(concat!("Trackside/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// Downloads `url`, refusing anything but a `200` with an `image/*` type and at most
/// [`MAX_BYTES`]. The bytes themselves are checked by the store.
pub async fn download(
    http: &reqwest::Client,
    url: reqwest::Url,
    timeout: Duration,
) -> Result<Vec<u8>, ArtError> {
    let transport = |e: reqwest::Error| ArtError::Transport(e.without_url().to_string());
    let mut response = http
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(transport)?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(ArtError::Http(status));
    }
    let kind = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !kind.starts_with("image/") {
        return Err(ArtError::NotImage(kind));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BYTES as u64)
    {
        return Err(ArtError::TooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err(ArtError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use trackside_core::PublicId;

    fn hosts() -> Vec<String> {
        IMAGE_HOSTS.iter().map(|h| (*h).to_owned()).collect()
    }

    #[test]
    fn only_last_fm_image_hosts_over_https_are_allowed() {
        let ok =
            "https://lastfm.freetls.fastly.net/i/u/300x300/4128a6eb29f94943c9d206c08e625904.jpg";
        assert!(allowed(ok, &hosts()).is_some());
        for bad in [
            "http://lastfm.freetls.fastly.net/i/u/300x300/a.jpg",
            "https://lastfm.freetls.fastly.net.evil.example/a.jpg",
            "https://evil.example/lastfm.freetls.fastly.net/a.jpg",
            "https://user@lastfm.freetls.fastly.net/a.jpg",
            "https://lastfm.freetls.fastly.net:8443/a.jpg",
            "https://169.254.169.254/latest/meta-data",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(allowed(bad, &hosts()).is_none(), "{bad}");
        }
    }

    #[test]
    fn an_image_has_one_id_at_every_size() -> Result<(), Box<dyn std::error::Error>> {
        let small = reqwest::Url::parse(
            "https://lastfm.freetls.fastly.net/i/u/34s/4128a6eb29f94943c9d206c08e625904.png",
        )?;
        let large = reqwest::Url::parse(
            "https://lastfm.freetls.fastly.net/i/u/300x300/4128A6EB29F94943C9D206C08E625904.png",
        )?;
        assert_eq!(image_id(&small), image_id(&large));
        assert_eq!(
            image_id(&small),
            ExternalArtId::derive("4128a6eb29f94943c9d206c08e625904")
        );
        assert_eq!(image_id(&small).as_str().chars().next(), Some('e'));
        Ok(())
    }
}
