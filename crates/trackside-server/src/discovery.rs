//! Candidate LAN URLs and mDNS advertising (architecture section 10).

use std::collections::HashMap;
use std::net::IpAddr;

use trackside_api::wire::auth::{ServerUrl, UrlKind};

/// Whether an address belongs in a candidate URL: not loopback or unspecified, and for IPv6
/// global or unique-local (link-local needs a zone id).
pub fn usable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified() && !v4.is_link_local(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            let link_local = first & 0xffc0 == 0xfe80;
            !v6.is_loopback() && !v6.is_unspecified() && !link_local && !v6.is_multicast()
        }
    }
}

fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64,
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// The URL of an address on `port`, with the base path.
pub fn url_for(ip: IpAddr, port: u16, tls: bool, base_path: &str) -> String {
    let scheme = if tls { "https" } else { "http" };
    match ip {
        IpAddr::V4(v4) => format!("{scheme}://{v4}:{port}{base_path}"),
        IpAddr::V6(v6) => format!("{scheme}://[{v6}]:{port}{base_path}"),
    }
}

/// Candidate URLs for these interface addresses: IPv4 first, then IPv6, then `<host>.local`
/// when mDNS is on.
pub fn candidates(
    addresses: &[IpAddr],
    port: u16,
    tls: bool,
    fingerprint: Option<&str>,
    base_path: &str,
    mdns_host: Option<&str>,
) -> Vec<ServerUrl> {
    let mut ips: Vec<IpAddr> = addresses.iter().copied().filter(|ip| usable(*ip)).collect();
    ips.sort_by_key(|ip| (ip.is_ipv6(), is_tailnet(*ip), *ip));
    ips.dedup();
    let entry = |url: String, kind| ServerUrl {
        url,
        kind,
        tls,
        fingerprint: fingerprint.map(str::to_owned),
    };
    let mut out: Vec<ServerUrl> = ips
        .into_iter()
        .map(|ip| {
            let kind = if is_tailnet(ip) {
                UrlKind::Tailnet
            } else {
                UrlKind::Lan
            };
            entry(url_for(ip, port, tls, base_path), kind)
        })
        .collect();
    if let Some(host) = mdns_host {
        let scheme = if tls { "https" } else { "http" };
        out.push(entry(
            format!("{scheme}://{host}.local:{port}{base_path}"),
            UrlKind::Lan,
        ));
    }
    out
}

/// The addresses of this host's interfaces.
pub fn interface_addresses() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .map(|list| list.into_iter().map(|i| i.ip()).collect())
        .unwrap_or_default()
}

/// The host name for `<host>.local`, cleaned to a DNS label.
pub fn local_host_name() -> Option<String> {
    let raw = std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())?;
    let label: String = raw
        .trim()
        .split('.')
        .next()?
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (!label.is_empty()).then(|| label.to_ascii_lowercase())
}

/// The TXT records of `_trackside._tcp`.
pub fn txt_records(server_id: &uuid::Uuid, tls: bool, base_path: &str) -> HashMap<String, String> {
    HashMap::from([
        (String::from("id"), server_id.to_string()),
        (String::from("v"), String::from("1")),
        (
            String::from("tls"),
            String::from(if tls { "1" } else { "0" }),
        ),
        (String::from("path"), base_path.to_owned()),
    ])
}

pub const SERVICE_TYPE: &str = "_trackside._tcp.local.";

/// A running mDNS advertisement; unregisters when stopped.
pub struct Advertisement {
    daemon: mdns_sd::ServiceDaemon,
    fullname: String,
}

impl std::fmt::Debug for Advertisement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Advertisement")
            .field("fullname", &self.fullname)
            .finish_non_exhaustive()
    }
}

impl Advertisement {
    /// Advertises `name` on `port`. Errors are for the caller to log: discovery is optional.
    pub fn start(
        name: &str,
        host: &str,
        port: u16,
        txt: HashMap<String, String>,
    ) -> Result<Self, mdns_sd::Error> {
        let daemon = mdns_sd::ServiceDaemon::new()?;
        let instance: String = name.chars().filter(|c| *c != '.').take(63).collect();
        let info = mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &format!("{host}.local."),
            "",
            port,
            txt,
        )?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        daemon.register(info)?;
        Ok(Self { daemon, fullname })
    }

    pub fn stop(self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_skip_loopback_and_link_local_and_bracket_ipv6()
    -> Result<(), std::net::AddrParseError> {
        let addresses: Vec<IpAddr> = vec![
            "127.0.0.1".parse()?,
            "192.168.1.20".parse()?,
            "fe80::1".parse()?,
            "fd7a:115c:a1e0::4".parse()?,
            "2001:db8::5".parse()?,
            "100.84.12.7".parse()?,
        ];
        let urls = candidates(&addresses, 4747, false, None, "/music", Some("home"));
        let texts: Vec<&str> = urls.iter().map(|u| u.url.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "http://192.168.1.20:4747/music",
                "http://100.84.12.7:4747/music",
                "http://[2001:db8::5]:4747/music",
                "http://[fd7a:115c:a1e0::4]:4747/music",
                "http://home.local:4747/music",
            ]
        );
        assert_eq!(urls[1].kind, UrlKind::Tailnet);
        assert_eq!(urls[0].kind, UrlKind::Lan);
        Ok(())
    }

    #[test]
    fn txt_records_carry_id_version_tls_and_path() {
        let id = uuid::Uuid::nil();
        let txt = txt_records(&id, true, "/music");
        assert_eq!(txt.get("v").map(String::as_str), Some("1"));
        assert_eq!(txt.get("tls").map(String::as_str), Some("1"));
        assert_eq!(txt.get("path").map(String::as_str), Some("/music"));
        assert_eq!(txt.get("id"), Some(&id.to_string()));
    }
}
