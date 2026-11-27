use http::Uri;

/// Music Assistant's default webserver port.
pub const DEFAULT_PORT: u16 = 8095;

/// Where Music Assistant is: scheme, host, port and an optional path prefix (for a reverse
/// proxy). Built from what an admin types: `http://ma.local:8095`, `ws://10.0.0.5:8095/ws`,
/// `https://music.example.com/ma` or a bare `ma.local` (port 8095).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    secure: bool,
    host: String,
    port: u16,
    base_path: String,
}

/// Why an address cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EndpointError {
    #[error("not a valid address")]
    Invalid,
    #[error("the address must start with http://, https://, ws:// or wss://")]
    Scheme,
    #[error("the address has no host")]
    NoHost,
    #[error("the address must not contain a user name, password, query or fragment")]
    Extra,
}

impl Endpoint {
    pub fn parse(text: &str) -> Result<Self, EndpointError> {
        let text = text.trim();
        if text.contains('#') || text.contains('?') {
            return Err(EndpointError::Extra);
        }
        let (with_scheme, bare) = if text.contains("://") {
            (text.to_owned(), false)
        } else {
            (format!("http://{text}"), true)
        };
        let uri: Uri = with_scheme.parse().map_err(|_| EndpointError::Invalid)?;
        let secure = match uri.scheme_str() {
            Some("http" | "ws") => false,
            Some("https" | "wss") => true,
            _ => return Err(EndpointError::Scheme),
        };
        let authority = uri.authority().ok_or(EndpointError::NoHost)?;
        if authority.as_str().contains('@') {
            return Err(EndpointError::Extra);
        }
        let host = authority.host();
        if host.is_empty() {
            return Err(EndpointError::NoHost);
        }
        let port = match authority.port_u16() {
            Some(port) => port,
            None if bare => DEFAULT_PORT,
            None if secure => 443,
            None => 80,
        };
        let mut base_path = uri.path().trim_end_matches('/').to_owned();
        if let Some(stripped) = base_path.strip_suffix("/ws") {
            base_path = stripped.to_owned();
        }
        Ok(Self {
            secure,
            host: host.to_owned(),
            port,
            base_path,
        })
    }

    pub fn is_secure(&self) -> bool {
        self.secure
    }

    /// The host, with brackets for an IPv6 address.
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The WebSocket API, for example `ws://ma.local:8095/ws`.
    pub fn ws_url(&self) -> String {
        let scheme = if self.secure { "wss" } else { "ws" };
        format!(
            "{scheme}://{}:{}{}/ws",
            self.host, self.port, self.base_path
        )
    }

    /// The webserver root without a trailing slash, for example `http://ma.local:8095`.
    pub fn http_base(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        format!("{scheme}://{}:{}{}", self.host, self.port, self.base_path)
    }
}

impl std::str::FromStr for Endpoint {
    type Err = EndpointError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(text: &str) -> String {
        Endpoint::parse(text).expect("parses").ws_url()
    }

    #[test]
    fn a_bare_host_gets_the_default_port() {
        assert_eq!(ws("ma.local"), "ws://ma.local:8095/ws");
    }

    #[test]
    fn http_addresses_become_websocket_urls() {
        assert_eq!(ws("http://10.0.0.5:8095/"), "ws://10.0.0.5:8095/ws");
        assert_eq!(
            ws("https://music.example.com/ma"),
            "wss://music.example.com:443/ma/ws"
        );
    }

    #[test]
    fn a_trailing_ws_path_is_not_doubled() {
        assert_eq!(ws("ws://ma:8095/ws"), "ws://ma:8095/ws");
    }

    #[test]
    fn ipv6_hosts_keep_their_brackets() {
        assert_eq!(ws("http://[::1]:8095"), "ws://[::1]:8095/ws");
    }

    #[test]
    fn the_http_base_has_no_trailing_slash() {
        let endpoint = Endpoint::parse("http://ma:8095/").expect("parses");
        assert_eq!(endpoint.http_base(), "http://ma:8095");
    }

    #[test]
    fn credentials_and_queries_are_refused() {
        assert_eq!(
            Endpoint::parse("http://u:p@ma:8095"),
            Err(EndpointError::Extra)
        );
        assert_eq!(
            Endpoint::parse("http://ma:8095/?token=x"),
            Err(EndpointError::Extra)
        );
    }

    #[test]
    fn other_schemes_are_refused() {
        assert_eq!(Endpoint::parse("ftp://ma"), Err(EndpointError::Scheme));
    }
}
