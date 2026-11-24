//! The client address behind trusted reverse proxies (architecture section 10): forwarding
//! headers are believed only when the peer is in `server.trusted_proxies`, and the client is
//! the rightmost hop that is not itself a trusted proxy.

use std::net::{IpAddr, SocketAddr};

use http::HeaderMap;

/// IPv4-mapped IPv6 addresses (from a dual-stack socket) as plain IPv4.
pub(crate) fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 @ IpAddr::V4(_) => v4,
    }
}

pub(crate) fn is_trusted(trusted: &[ipnet::IpNet], ip: IpAddr) -> bool {
    let ip = canonical(ip);
    trusted.iter().any(|net| net.contains(&ip))
}

/// The client address for a request that arrived from `peer`.
pub(crate) fn client_ip(trusted: &[ipnet::IpNet], headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    let peer = canonical(peer.ip());
    if !is_trusted(trusted, peer) {
        return peer;
    }
    let hops = forwarded_for(headers);
    let mut client = peer;
    for hop in hops.iter().rev() {
        let Some(hop) = hop else {
            // An obfuscated or unparseable hop: nothing to its left can be believed.
            return client;
        };
        client = canonical(*hop);
        if !is_trusted(trusted, client) {
            return client;
        }
    }
    client
}

/// The `X-Forwarded-For` addresses when present, else the `for` addresses of `Forwarded`,
/// from the first hop to the last. `None` marks a hop that is not an address.
///
/// `X-Forwarded-For` wins because nginx, Caddy, Traefik and Cloudflare append to it and pass a
/// client's own `Forwarded` through untouched, so a `Forwarded` beside it may be forged.
fn forwarded_for(headers: &HeaderMap) -> Vec<Option<IpAddr>> {
    if headers.contains_key("x-forwarded-for") {
        return headers
            .get_all("x-forwarded-for")
            .iter()
            .map(|v| v.to_str().ok())
            .flat_map(|v| match v {
                Some(v) => v.split(',').map(|hop| parse_node(hop.trim())).collect(),
                None => vec![None],
            })
            .collect();
    }
    let forwarded: Vec<&str> = headers
        .get_all(http::header::FORWARDED)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    if !forwarded.is_empty() {
        return forwarded
            .iter()
            .flat_map(|v| v.split(','))
            .filter_map(|element| {
                element.split(';').find_map(|pair| {
                    let (name, value) = pair.split_once('=')?;
                    name.trim()
                        .eq_ignore_ascii_case("for")
                        .then(|| parse_node(value.trim().trim_matches('"')))
                })
            })
            .collect();
    }
    Vec::new()
}

/// `192.0.2.1`, `192.0.2.1:80`, `2001:db8::1`, `[2001:db8::1]` or `[2001:db8::1]:80`.
fn parse_node(text: &str) -> Option<IpAddr> {
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Some(ip);
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Some(addr.ip());
    }
    text.strip_prefix('[')
        .and_then(|rest| rest.split(']').next())
        .and_then(|inner| inner.parse().ok())
}

/// The first value of a forwarding header such as `X-Forwarded-Host`, believed only from a
/// trusted peer.
pub(crate) fn forwarded_value<'h>(
    trusted: &[ipnet::IpNet],
    headers: &'h HeaderMap,
    peer: Option<SocketAddr>,
    name: &str,
) -> Option<&'h str> {
    let peer = peer?;
    if !is_trusted(trusted, peer.ip()) {
        return None;
    }
    headers
        .get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// Whether the forwarding headers say the client used HTTPS: the first `X-Forwarded-Proto`
/// value, or the first `proto` of `Forwarded` when the proxy sent no `X-Forwarded-For`. Only
/// meaningful from a proxy whose word counts.
pub(crate) fn forwarded_https(headers: &HeaderMap) -> bool {
    let https = |v: &str| v.trim().trim_matches('"').eq_ignore_ascii_case("https");
    if let Some(proto) = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
    {
        return https(proto);
    }
    if headers.contains_key("x-forwarded-for") {
        // The proxy speaks `X-Forwarded-*`, so a `Forwarded` beside it is the client's own.
        return false;
    }
    headers
        .get(http::header::FORWARDED)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .and_then(|element| {
            element.split(';').find_map(|pair| {
                let (name, value) = pair.split_once('=')?;
                name.trim()
                    .eq_ignore_ascii_case("proto")
                    .then(|| https(value))
            })
        })
        .unwrap_or(false)
}

/// Whether a request carries any forwarding header, as a reverse proxy adds.
pub(crate) fn has_forwarding(headers: &HeaderMap) -> bool {
    [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-real-ip",
    ]
    .iter()
    .any(|name| headers.contains_key(*name))
}

/// Private, loopback, link-local or unique-local, where plain HTTP sessions are allowed.
pub fn is_local_address(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn nets(list: &[&str]) -> Vec<ipnet::IpNet> {
        list.iter().filter_map(|n| n.parse().ok()).collect()
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_static(value));
        }
        map
    }

    fn peer(text: &str) -> SocketAddr {
        text.parse()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)))
    }

    fn ip(text: &str) -> IpAddr {
        text.parse()
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
    }

    #[test]
    fn forwarded_headers_are_ignored_from_untrusted_peers() {
        let spoofed = headers(&[
            ("x-forwarded-for", "10.9.9.9"),
            ("forwarded", "for=10.9.9.9"),
        ]);
        let trusted = nets(&["10.0.0.0/8"]);
        assert_eq!(
            client_ip(&trusted, &spoofed, peer("203.0.113.5:4000")),
            ip("203.0.113.5")
        );
        assert_eq!(
            client_ip(&trusted, &spoofed, peer("[2001:db8::5]:4000")),
            ip("2001:db8::5")
        );
        assert_eq!(
            client_ip(&[], &spoofed, peer("10.0.0.2:80")),
            ip("10.0.0.2")
        );
    }

    #[test]
    fn trusted_ipv4_proxy_gives_the_rightmost_untrusted_hop() {
        let trusted = nets(&["10.0.0.0/8"]);
        let chain = headers(&[("x-forwarded-for", "198.51.100.1, 203.0.113.9, 10.0.0.7")]);
        assert_eq!(
            client_ip(&trusted, &chain, peer("10.0.0.2:80")),
            ip("203.0.113.9")
        );
    }

    #[test]
    fn trusted_ipv6_proxy_gives_the_client() {
        let trusted = nets(&["fd00::/8"]);
        let chain = headers(&[("x-forwarded-for", "2001:db8::77")]);
        assert_eq!(
            client_ip(&trusted, &chain, peer("[fd00::1]:443")),
            ip("2001:db8::77")
        );
        let with_port = headers(&[("forwarded", r#"for="[2001:db8::78]:4711";proto=https"#)]);
        assert_eq!(
            client_ip(&trusted, &with_port, peer("[fd00::1]:443")),
            ip("2001:db8::78")
        );
    }

    #[test]
    fn ipv4_mapped_peer_matches_an_ipv4_trusted_range() {
        let trusted = nets(&["127.0.0.1/32"]);
        let chain = headers(&[("x-forwarded-for", "::ffff:203.0.113.4")]);
        assert_eq!(
            client_ip(&trusted, &chain, peer("[::ffff:127.0.0.1]:9000")),
            ip("203.0.113.4")
        );
    }

    #[test]
    fn x_forwarded_for_wins_over_a_forwarded_the_client_sent() {
        let trusted = nets(&["10.0.0.0/8"]);
        // nginx appended the real client; the `Forwarded` came from the client itself.
        let both = headers(&[
            ("forwarded", "for=192.168.1.5"),
            ("x-forwarded-for", "198.51.100.99"),
        ]);
        assert_eq!(
            client_ip(&trusted, &both, peer("10.0.0.2:80")),
            ip("198.51.100.99")
        );
        let only = headers(&[("forwarded", "for=192.0.2.60;proto=http, for=10.1.1.1")]);
        assert_eq!(
            client_ip(&trusted, &only, peer("10.0.0.2:80")),
            ip("192.0.2.60")
        );
    }

    #[test]
    fn a_forwarded_proto_beside_x_forwarded_for_is_not_believed() {
        let spoofed = headers(&[
            ("forwarded", "for=192.168.1.5;proto=https"),
            ("x-forwarded-for", "198.51.100.99"),
        ]);
        assert!(!forwarded_https(&spoofed));
        assert!(forwarded_https(&headers(&[(
            "forwarded",
            "for=192.0.2.1;proto=https"
        )])));
        assert!(forwarded_https(&headers(&[
            ("x-forwarded-proto", "https"),
            ("x-forwarded-for", "198.51.100.99"),
        ])));
    }

    #[test]
    fn obfuscated_hop_stops_the_walk() {
        let trusted = nets(&["10.0.0.0/8"]);
        let hidden = headers(&[("forwarded", "for=198.51.100.1, for=_hidden, for=10.0.0.9")]);
        assert_eq!(
            client_ip(&trusted, &hidden, peer("10.0.0.2:80")),
            ip("10.0.0.9")
        );
        let garbage = headers(&[("x-forwarded-for", "not-an-ip")]);
        assert_eq!(
            client_ip(&trusted, &garbage, peer("10.0.0.2:80")),
            ip("10.0.0.2")
        );
    }

    #[test]
    fn several_x_forwarded_for_headers_are_one_list() {
        let trusted = nets(&["10.0.0.0/8"]);
        let split = headers(&[
            ("x-forwarded-for", "203.0.113.1"),
            ("x-forwarded-for", "10.0.0.3"),
        ]);
        assert_eq!(
            client_ip(&trusted, &split, peer("10.0.0.2:80")),
            ip("203.0.113.1")
        );
    }

    #[test]
    fn all_trusted_hops_give_the_leftmost() {
        let trusted = nets(&["10.0.0.0/8"]);
        let chain = headers(&[("x-forwarded-for", "10.0.0.5, 10.0.0.6")]);
        assert_eq!(
            client_ip(&trusted, &chain, peer("10.0.0.2:80")),
            ip("10.0.0.5")
        );
    }

    #[test]
    fn forwarded_host_is_believed_only_from_trusted_peers() {
        let trusted = nets(&["10.0.0.0/8"]);
        let map = headers(&[("x-forwarded-host", "music.example.com")]);
        assert_eq!(
            forwarded_value(&trusted, &map, Some(peer("10.0.0.2:1")), "x-forwarded-host"),
            Some("music.example.com")
        );
        assert_eq!(
            forwarded_value(
                &trusted,
                &map,
                Some(peer("203.0.113.2:1")),
                "x-forwarded-host"
            ),
            None
        );
    }

    #[test]
    fn local_addresses() {
        assert!(is_local_address(ip("192.168.1.4")));
        assert!(is_local_address(ip("::1")));
        assert!(is_local_address(ip("fd12::1")));
        assert!(!is_local_address(ip("203.0.113.1")));
        assert!(!is_local_address(ip("2001:db8::1")));
    }
}
