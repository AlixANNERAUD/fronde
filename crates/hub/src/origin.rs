//! Origins and addresses: the rules the gate applies to every outgoing request, in one place.
//!
//! An [`Origin`] is parsed once from the configuration and once from each request, and both go
//! through the URL standard's host parser, so two spellings of the same host (`0x7f.1` and
//! `127.0.0.1`, `[0:0:0:0:0:0:0:1]` and `[::1]`, upper and lower case) are the same origin on
//! both sides.

use anyhow::{Context, bail};
use serde::Deserialize;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use url::{Host, Url};

/// `scheme://host[:port]` over http or https: the host normalised by the URL standard (lower
/// case, punycode, IPv4 in dotted decimal, IPv6 compressed) and the port the scheme's default
/// when left out.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin {
    https: bool,
    host: Host,
    port: u16,
}

/// Where an origin's host lies, as far as its name or address tells without resolving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locality {
    /// This machine: `localhost` or a loopback address.
    Loopback,
    /// This machine's networks: a private, shared or link-local address, or a name that only a
    /// local resolver knows (no dot, `.local`, `.lan`).
    Local,
    Public,
}

impl Origin {
    /// An origin as the configuration writes it: `scheme://host[:port]`, with no user, path,
    /// query or fragment.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let url = Url::parse(s).with_context(|| format!("origin {s:?} is not a URL"))?;
        let origin = Self::of_url(&url).with_context(|| format!("origin {s:?}"))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("origin {s:?} must be scheme://host[:port], with no path or user");
        }
        Ok(origin)
    }

    /// The origin of an http or https URL, whatever its path.
    pub fn of_url(url: &Url) -> anyhow::Result<Self> {
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            other => bail!("the scheme must be http or https, not {other:?}"),
        };
        let host = url.host().context("no host")?.to_owned();
        let port = url.port_or_known_default().context("no port")?;
        Ok(Self { https, host, port })
    }

    /// The origin a request goes to, or `None` when it has none the gate can allow. The host goes
    /// through the same parser as the configuration's.
    pub fn of_request(uri: &http::Uri) -> Option<Self> {
        let https = match uri.scheme_str()? {
            "https" => true,
            "http" => false,
            _ => return None,
        };
        let authority = uri.authority()?;
        // The connection goes to the parsed host: refuse user information rather than trust the
        // parse to agree with whatever reads the raw authority.
        if authority.as_str().contains('@') {
            return None;
        }
        let host = authority.host();
        // A port that is not a u16 is no port at all, not the scheme's default one.
        let port = match &authority.as_str()[host.len()..] {
            "" => default_port(https),
            rest => rest
                .strip_prefix(':')
                .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))?
                .parse()
                .ok()?,
        };
        let host = Host::parse(host).ok()?;
        Some(Self { https, host, port })
    }

    pub fn is_https(&self) -> bool {
        self.https
    }

    pub fn scheme(&self) -> &'static str {
        if self.https { "https" } else { "http" }
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The host when it is a name.
    pub fn domain(&self) -> Option<&str> {
        match &self.host {
            Host::Domain(d) => Some(d),
            _ => None,
        }
    }

    /// The host when it is an address.
    pub fn ip(&self) -> Option<IpAddr> {
        match self.host {
            Host::Domain(_) => None,
            Host::Ipv4(ip) => Some(ip.into()),
            Host::Ipv6(ip) => Some(ip.into()),
        }
    }

    /// The host to resolve and to name in TLS: the name, or the address without brackets.
    pub fn bare_host(&self) -> String {
        match &self.host {
            Host::Domain(d) => d.clone(),
            Host::Ipv4(ip) => ip.to_string(),
            Host::Ipv6(ip) => ip.to_string(),
        }
    }

    /// `host[:port]`, as the `Host` header names it: the port only when it is not the scheme's
    /// default.
    pub fn authority(&self) -> String {
        if self.port == default_port(self.https) {
            self.host.to_string()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn locality(&self) -> Locality {
        match &self.host {
            // Only the exact name: glibc does not match "localhost." against /etc/hosts, so that
            // name may go to DNS, which can answer with any address.
            Host::Domain(d) if d == "localhost" => Locality::Loopback,
            Host::Domain(d) => {
                // A fully qualified name ("nas.lan.") is the same host.
                let d = d.strip_suffix('.').unwrap_or(d);
                if !d.contains('.') || d.ends_with(".local") || d.ends_with(".lan") {
                    Locality::Local
                } else {
                    Locality::Public
                }
            }
            Host::Ipv4(ip) => locality_of((*ip).into()),
            Host::Ipv6(ip) => locality_of((*ip).into()),
        }
    }

    /// Whether the host may send a secret there: over https, or over plain http to this machine,
    /// where it never crosses a network.
    pub fn carries_credentials(&self) -> bool {
        self.https || self.locality() == Locality::Loopback
    }
}

fn default_port(https: bool) -> u16 {
    if https { 443 } else { 80 }
}

fn locality_of(ip: IpAddr) -> Locality {
    if is_loopback(ip) {
        Locality::Loopback
    } else if is_local(ip) {
        Locality::Local
    } else {
        Locality::Public
    }
}

impl FromStr for Origin {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Self::parse(s)
    }
}

impl<'de> Deserialize<'de> for Origin {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// As the configuration writes it: the port only when it is not the scheme's default.
impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme(), self.authority())
    }
}

/// Cloud metadata services outside the ranges `is_local` already covers (most sit at
/// 169.254.169.254, which is link-local).
const METADATA: [IpAddr; 3] = [
    // Azure WireServer.
    IpAddr::V4(Ipv4Addr::new(168, 63, 129, 16)),
    // Alibaba Cloud.
    IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200)),
    // AWS over IPv6.
    IpAddr::V6(Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)),
];

/// An address of this machine: loopback, IPv4-mapped included. Only an address that stays on
/// the machine: an IPv4 loopback inside NAT64 or 6to4 goes out to a relay.
pub fn is_loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// An address of the host itself or of its networks: loopback, unspecified (which reaches the
/// host), private (10/8, 172.16/12, 192.168/16, fc00::/7), shared (100.64/10), link-local (where
/// most metadata services are), IETF protocol assignments (192.0.0/24), benchmarking
/// (198.18/15), NAT64 local-use (64:ff9b:1::/48) and other metadata addresses. IPv4 carried in
/// IPv6 counts as itself.
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && (18..=19).contains(&b))
                || METADATA.contains(&ip)
        }
        IpAddr::V6(v6) => match embedded_ipv4(v6) {
            Some(v4) => is_local(IpAddr::V4(v4)),
            None => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_unicast_link_local()
                    || v6.is_unique_local()
                    || matches!(v6.segments(), [0x64, 0xff9b, 1, ..])
                    || METADATA.contains(&ip)
            }
        },
    }
}

/// Addresses no origin may reach, not even one allowed as local: link-local (169.254.0.0/16,
/// fe80::/10) and cloud metadata. No CalDAV server lives there.
pub fn is_never_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local() || METADATA.contains(&ip),
        IpAddr::V6(v6) => match embedded_ipv4(v6) {
            Some(v4) => is_never_allowed(IpAddr::V4(v4)),
            None => v6.is_unicast_link_local() || METADATA.contains(&ip),
        },
    }
}

/// The IPv4 address an IPv6 one stands for: mapped (`::ffff:a.b.c.d`), translated
/// (`::ffff:0:a.b.c.d`), compatible (`::a.b.c.d`), NAT64 (`64:ff9b::/96`) or 6to4 (`2002::/16`).
fn embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let o = v6.octets();
    match v6.segments() {
        [0, 0, 0, 0, 0, 0xffff | 0, _, _]
        | [0, 0, 0, 0, 0xffff, 0, _, _]
        | [0x64, 0xff9b, 0, 0, 0, 0, _, _] => Some(Ipv4Addr::new(o[12], o[13], o[14], o[15])),
        [0x2002, ..] => Some(Ipv4Addr::new(o[2], o[3], o[4], o[5])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(s: &str) -> Origin {
        s.parse().unwrap()
    }

    fn request(uri: &str) -> Option<Origin> {
        Origin::of_request(&uri.parse().unwrap())
    }

    #[test]
    fn origins_must_be_bare() {
        for bad in [
            "https://h.example/path",
            "https://u:p@h.example",
            "https://u@h.example",
            "https://h.example?q",
            "https://h.example#f",
            "ftp://h.example",
            "h.example",
        ] {
            assert!(bad.parse::<Origin>().is_err(), "{bad}");
        }
        assert!("https://h.example/".parse::<Origin>().is_ok());
    }

    #[test]
    fn spellings_of_one_origin_are_equal() {
        for (a, b) in [
            ("http://0x7f.1:8080", "http://127.0.0.1:8080"),
            ("http://2130706433", "http://127.0.0.1"),
            ("http://[0:0:0:0:0:0:0:1]:9", "http://[::1]:9"),
            ("https://Cal.EXAMPLE", "https://cal.example"),
            ("https://cal.example:443", "https://cal.example"),
            ("http://cal.example:80/", "http://cal.example"),
        ] {
            assert_eq!(origin(a), origin(b), "{a} and {b}");
        }
        assert_ne!(origin("http://cal.example"), origin("https://cal.example"));
        assert_ne!(
            origin("https://cal.example"),
            origin("https://cal.example:8443")
        );
    }

    #[test]
    fn shown_as_the_configuration_writes_it() {
        for (written, shown) in [
            ("https://Cal.Example:443/", "https://cal.example"),
            ("http://0x7f.1:8080", "http://127.0.0.1:8080"),
            ("http://[0:0:0:0:0:0:0:1]", "http://[::1]"),
            ("https://cal.example:80", "https://cal.example:80"),
        ] {
            assert_eq!(origin(written).to_string(), shown, "{written}");
        }
    }

    #[test]
    fn requests_and_configuration_agree() {
        for (configured, uri) in [
            ("http://127.0.0.1:8080", "http://0x7f.1:8080/dav"),
            ("http://127.0.0.1:8080", "http://127.000.000.001:8080/"),
            ("http://[::1]:9", "http://[0:0:0:0:0:0:0:1]:9/x"),
            ("http://0x7f.1:8080", "http://127.0.0.1:8080/"),
            ("https://cal.example", "https://CAL.Example/dav/"),
            ("https://cal.example", "https://cal.example:443/dav/"),
            ("http://cal.example:80", "http://cal.example/"),
        ] {
            assert_eq!(request(uri), Some(origin(configured)), "{uri}");
        }
        assert_ne!(
            request("https://cal.example:8443/"),
            Some(origin("https://cal.example"))
        );
        assert_ne!(
            request("http://cal.example/"),
            Some(origin("https://cal.example"))
        );
    }

    #[test]
    fn requests_with_user_information_or_another_scheme_have_no_origin() {
        for uri in [
            "https://cal.example@evil.test/",
            "https://cal.example:443@evil.test/",
            "https://x@cal.example/",
            "ftp://cal.example/",
            "/relative",
        ] {
            assert_eq!(request(uri), None, "{uri}");
        }
    }

    #[test]
    fn requests_with_a_bad_port_have_no_origin() {
        for uri in [
            "http://cal.example:65536/",
            "http://cal.example:99999/",
            "http://cal.example:/",
            "http://[::1]:70000/",
        ] {
            assert_eq!(request(uri), None, "{uri}");
        }
        assert_eq!(
            request("http://[::1]:65535/"),
            Some(origin("http://[::1]:65535"))
        );
    }

    #[test]
    fn authority_as_the_host_header_names_it() {
        for (o, authority) in [
            ("http://0x7f.1:8080", "127.0.0.1:8080"),
            ("https://Cal.Example:443", "cal.example"),
            ("http://[0:0:0:0:0:0:0:1]:9", "[::1]:9"),
            ("https://cal.example:80", "cal.example:80"),
        ] {
            assert_eq!(origin(o).authority(), authority, "{o}");
        }
    }

    #[test]
    fn credentials_need_https_unless_loopback() {
        for o in [
            "https://cal.example",
            "https://10.0.0.2",
            "http://localhost:8080",
            "http://127.0.0.1:9",
            "http://0x7f.1:9",
            "http://[::1]:9",
            "http://[::ffff:127.0.0.1]:9",
        ] {
            assert!(origin(o).carries_credentials(), "{o}");
        }
        for o in [
            "http://cal.example",
            "http://10.0.0.2:8080",
            "http://192.168.1.10:5232",
            "http://100.64.0.1",
            "http://cal.local",
            // Not in /etc/hosts as glibc reads it: the name may go to DNS.
            "http://localhost.",
            "http://localhost.:5232",
            // Unspecified, not loopback: a socket bound there may be reached from the network.
            "http://0.0.0.0",
            "http://[::]",
            "http://[::ffff:0.0.0.0]",
            // IPv4-compatible, deprecated: not a loopback address.
            "http://[::127.0.0.1]",
            // An IPv4 loopback inside NAT64 or 6to4 goes through a relay.
            "http://[64:ff9b::7f00:1]",
            "http://[2002:7f00:1::]",
        ] {
            assert!(!origin(o).carries_credentials(), "{o}");
        }
    }

    #[test]
    fn locality_of_hosts() {
        for (o, locality) in [
            ("http://localhost:5232", Locality::Loopback),
            ("http://localhost.:5232", Locality::Local),
            ("http://nas.lan.", Locality::Local),
            ("http://127.0.0.1", Locality::Loopback),
            ("http://[::1]", Locality::Loopback),
            ("http://[::ffff:127.0.0.1]", Locality::Loopback),
            ("http://192.168.1.10:5232", Locality::Local),
            ("http://10.0.0.2", Locality::Local),
            ("http://100.64.0.1", Locality::Local),
            ("http://100.127.255.254", Locality::Local),
            ("http://[::ffff:192.168.1.10]", Locality::Local),
            ("http://[::ffff:100.64.0.1]", Locality::Local),
            ("http://[fd12::1]", Locality::Local),
            ("http://nas", Locality::Local),
            ("http://nas.local", Locality::Local),
            ("http://nas.lan", Locality::Local),
            ("https://cloud.example", Locality::Public),
            ("https://100.128.0.1", Locality::Public),
            ("https://[::ffff:8.8.8.8]", Locality::Public),
        ] {
            assert_eq!(origin(o).locality(), locality, "{o}");
        }
    }

    #[test]
    fn local_addresses() {
        for local in [
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "10.0.0.2",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.0",
            "192.0.0.255",
            "192.168.1.10",
            "198.18.0.0",
            "198.19.255.255",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.169.254",
            "169.254.170.2",
            "100.100.100.200",
            "168.63.129.16",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "fd00:ec2::254",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:100.64.0.1",
            "::ffff:0:0.0.0.0",
            "::ffff:0:127.0.0.1",
            "::ffff:0:10.0.0.1",
            "::ffff:0:192.0.0.0",
            "::ffff:0:198.19.255.255",
            "::127.0.0.1",
            "::10.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::",
            "64:ff9b:1::808:808",
            "64:ff9b:1:ffff:ffff:ffff:ffff:ffff",
            "2002:7f00:1::",
            "2002:c0a8:101::1",
        ] {
            assert!(is_local(local.parse().unwrap()), "{local}");
        }
        for remote in [
            "8.8.8.8",
            "172.32.0.1",
            "172.15.255.255",
            "191.255.255.255",
            "192.0.1.0",
            "198.17.255.255",
            "198.20.0.0",
            "100.128.0.1",
            "100.63.255.255",
            "2001:db8::1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "::fffe:ffff:ffff:ffff",
            "::ffff:0:8.8.8.8",
            "::ffff:0:255.255.255.255",
            "::ffff:1:0:0",
            "::ffff:1:127.0.0.1",
            "64:ff9b::808:808",
            "64:ff9b:0:ffff:ffff:ffff:ffff:ffff",
            "64:ff9b:2::",
            "2002:808:808::1",
        ] {
            assert!(!is_local(remote.parse().unwrap()), "{remote}");
        }
    }

    #[test]
    fn never_allowed_addresses() {
        for ip in [
            "169.254.0.1",
            "169.254.169.254",
            "fe80::1",
            "::ffff:169.254.169.254",
            "::ffff:0:169.254.169.254",
            "100.100.100.200",
            "168.63.129.16",
            "fd00:ec2::254",
        ] {
            assert!(is_never_allowed(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "192.168.1.10",
            "10.0.0.2",
            "::1",
            "fd12::1",
            "8.8.8.8",
        ] {
            assert!(!is_never_allowed(ip.parse().unwrap()), "{ip}");
        }
    }
}
