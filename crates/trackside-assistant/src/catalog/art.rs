//! Fetching catalog images (contract 2.6). Only addresses Music Assistant gave for an item
//! are fetched, never one from a client: Music Assistant's own image proxy, or an `https`
//! address whose host resolves only to public addresses. No redirects, no proxies, a time
//! limit, a size limit and a bound on fetches at once. The bytes go to the artwork cache,
//! which checks them by their magic bytes and keeps them re-encoded.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use trackside_core::catalog::{CatalogError, MAX_ART_BYTES};
use trackside_mass::{CustomCa, Endpoint};

/// How long one image may take, from the first byte sent to the last received.
pub(crate) const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Upstream fetches at once, server-wide.
pub(crate) const MAX_FETCHES: usize = 4;

/// Why an address is not fetched.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Refusal {
    #[error("not a valid address")]
    Invalid,
    #[error("only https is fetched from hosts other than Music Assistant")]
    Scheme,
    #[error("the address carries a user name or password")]
    Credentials,
    #[error("the address is not public")]
    Private,
}

/// Where Music Assistant is, the one host that may be on a private network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Home {
    pub scheme: &'static str,
    pub host: String,
    pub port: u16,
}

impl Home {
    pub(crate) fn of(endpoint: &Endpoint) -> Self {
        Self {
            scheme: if endpoint.is_secure() {
                "https"
            } else {
                "http"
            },
            host: endpoint
                .host()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_ascii_lowercase(),
            port: endpoint.port(),
        }
    }

    fn is(&self, url: &Url) -> bool {
        let host = url
            .host_str()
            .unwrap_or_default()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        url.scheme() == self.scheme
            && host == self.host
            && url.port_or_known_default() == Some(self.port)
    }
}

/// An address on the public internet: not loopback, private, link-local, shared (CGNAT),
/// unique-local, multicast, documentation, benchmarking or unspecified.
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => public_v4(v4),
            None => public_v6(v6),
        },
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (b & 0xc0) == 64)
        || (a == 198 && (b & 0xfe) == 18)
        || (a == 192 && b == 0 && c == 0))
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first == 0x2001 && ip.segments()[1] == 0x0db8)
        || (first == 0x0064 && ip.segments()[1] == 0xff9b))
}

/// Checks an address Music Assistant gave before anything is sent to it. Names other than
/// Music Assistant's own are checked again when they resolve ([`PublicOnly`]).
pub(crate) fn check(source: &str, home: Option<&Home>) -> Result<Url, Refusal> {
    let url = Url::parse(source).map_err(|_| Refusal::Invalid)?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Refusal::Credentials);
    }
    if home.is_some_and(|h| h.is(&url)) {
        return Ok(url);
    }
    if url.scheme() != "https" {
        return Err(Refusal::Scheme);
    }
    let host = url
        .host_str()
        .ok_or(Refusal::Invalid)?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return Err(Refusal::Private);
    }
    match host.parse::<IpAddr>() {
        Ok(ip) if !is_public(ip) => Err(Refusal::Private),
        _ => Ok(url),
    }
}

/// Resolves names to public addresses only; Music Assistant's own name resolves as usual.
/// The connection goes to the addresses checked here, so a name cannot pass the check and
/// then point somewhere private.
struct PublicOnly {
    home: Option<String>,
}

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_ascii_lowercase();
        let home = self.home.as_deref() == Some(host.as_str());
        Box::pin(async move {
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| home || is_public(addr.ip()))
                .collect();
            if found.is_empty() {
                return Err(Box::new(std::io::Error::other(
                    "the name resolves to no public address",
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(found.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The client for one Music Assistant: no redirects, no proxies, its certificate authority
/// when it has one.
pub(crate) fn client(
    home: Option<&Home>,
    ca: Option<&CustomCa>,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("Trackside/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(Arc::new(PublicOnly {
            home: home.map(|h| h.host.clone()),
        }));
    if let Some(ca) = ca {
        let certs = ca
            .certificates()
            .iter()
            .filter_map(|der| reqwest::Certificate::from_der(der).ok());
        builder = builder.tls_certs_merge(certs);
    }
    builder.build().map_err(|e| e.without_url().to_string())
}

/// Downloads an image: a `200` with an `image/*` type, at most [`MAX_ART_BYTES`], within
/// [`FETCH_TIMEOUT`].
pub(crate) async fn download(http: &reqwest::Client, url: Url) -> Result<Vec<u8>, CatalogError> {
    let failed =
        |what: String| CatalogError::Failed(format!("the image could not be fetched: {what}"));
    let transport = |e: reqwest::Error| failed(e.without_url().to_string());
    let fetch = async {
        let mut response = http.get(url).send().await.map_err(transport)?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
            return Err(CatalogError::NotFound);
        }
        if status != reqwest::StatusCode::OK {
            return Err(failed(format!("HTTP {}", status.as_u16())));
        }
        let kind = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !kind.starts_with("image/") && !kind.starts_with("application/octet-stream") {
            return Err(failed(format!("not an image ({kind})")));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_ART_BYTES as u64)
        {
            return Err(failed(String::from("too large")));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            if bytes.len() + chunk.len() > MAX_ART_BYTES {
                return Err(failed(String::from("too large")));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    };
    tokio::time::timeout(FETCH_TIMEOUT, fetch)
        .await
        .map_err(|_| failed(String::from("timed out")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Home {
        Home {
            scheme: "http",
            host: String::from("192.168.1.20"),
            port: 8095,
        }
    }

    #[test]
    fn music_assistant_itself_may_be_private() {
        assert!(
            check(
                "http://192.168.1.20:8095/imageproxy/ab?size=0",
                Some(&home())
            )
            .is_ok()
        );
        for (source, why) in [
            ("http://192.168.1.20:8096/imageproxy/ab", Refusal::Scheme),
            ("https://192.168.1.20:8095/imageproxy/ab", Refusal::Private),
            ("http://192.168.1.21:8095/imageproxy/ab", Refusal::Scheme),
        ] {
            assert_eq!(check(source, Some(&home())).err(), Some(why), "{source}");
        }
    }

    #[test]
    fn other_hosts_must_be_public_and_https() {
        assert!(check("https://i.scdn.co/image/ab67616d0000b273", Some(&home())).is_ok());
        assert!(check("https://93.184.216.34/a.jpg", None).is_ok());
        for (source, why) in [
            ("http://i.scdn.co/image/ab", Refusal::Scheme),
            ("file:///etc/passwd", Refusal::Scheme),
            ("ftp://example.com/a.jpg", Refusal::Scheme),
            ("https://user:pw@i.scdn.co/a.jpg", Refusal::Credentials),
            ("https://127.0.0.1/a.jpg", Refusal::Private),
            ("https://localhost/a.jpg", Refusal::Private),
            ("https://10.0.0.8/a.jpg", Refusal::Private),
            ("https://169.254.169.254/latest/meta-data", Refusal::Private),
            ("https://100.100.1.1/a.jpg", Refusal::Private),
            ("https://[::1]/a.jpg", Refusal::Private),
            ("https://[fe80::1]/a.jpg", Refusal::Private),
            ("https://[fd00::5]/a.jpg", Refusal::Private),
            ("https://[::ffff:10.0.0.1]/a.jpg", Refusal::Private),
            ("https://0.0.0.0/a.jpg", Refusal::Private),
            ("not a url", Refusal::Invalid),
        ] {
            assert_eq!(check(source, Some(&home())).err(), Some(why), "{source}");
        }
    }

    #[tokio::test]
    async fn names_that_resolve_privately_are_refused_when_they_resolve() {
        let resolver = PublicOnly { home: None };
        let name: reqwest::dns::Name = "localhost".parse().unwrap_or_else(|_| panic!("a name"));
        let outcome = reqwest::dns::Resolve::resolve(&resolver, name).await;
        assert!(outcome.is_err(), "localhost has no public address");

        let home = PublicOnly {
            home: Some(String::from("localhost")),
        };
        let name: reqwest::dns::Name = "localhost".parse().unwrap_or_else(|_| panic!("a name"));
        assert!(
            reqwest::dns::Resolve::resolve(&home, name).await.is_ok(),
            "Music Assistant's own name resolves as usual"
        );
    }
}
