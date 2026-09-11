//! Peer IP allowlist (Gateway security backstop).
//!
//! `[security].allowed_node_ips` restricts which peer IPs may connect to
//! the Gateway's HTTP API and MQTT broker. An empty list means "allow
//! everyone" (default — matches the historical permissive behavior).
//!
//! Design constraints:
//! - The allowlist is a **boot-time-only** setting read from `gateway.toml`
//!   or the `ACOWORK_GATEWAY_ALLOWED_NODE_IPS` environment variable. It is
//!   intentionally **not** exposed through `PUT /api/config`, any Tauri
//!   command, or the Desktop UI — it is an out-of-band security backstop
//!   that only an operator with filesystem access to the Gateway config
//!   can change.
//! - Loopback (`127.0.0.1` / `::1`) is **always** allowed, regardless of
//!   the list. This keeps local Desktop / Runtime / Node connections alive
//!   even when an operator locks the Gateway down to a set of LAN peers.
//!
//! Supported entry formats:
//! - Exact IP:      `"192.168.1.20"`, `"::1"`
//! - IPv4 CIDR:     `"10.0.0.0/24"`
//! - IPv6 CIDR:     `"fd00::/8"` (not commonly needed, but accepted)

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// One allowlist entry: either an exact address or a CIDR prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowlistEntry {
    /// Exact IP address.
    Exact(IpAddr),
    /// IPv4 network prefix (addr, prefix_len). `prefix_len` ∈ 0..=32.
    Ipv4Cidr(Ipv4Addr, u8),
    /// IPv6 network prefix (addr, prefix_len). `prefix_len` ∈ 0..=128.
    Ipv6Cidr(Ipv6Addr, u8),
}

impl AllowlistEntry {
    /// Parse a single allowlist entry string.
    ///
    /// Accepts `"1.2.3.4"`, `"1.2.3.0/24"`, `"::1"`, `"fd00::/8"`.
    /// Returns `None` on malformed input (caller decides whether to
    /// treat that as a config error or a warn-and-skip).
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }
        // CIDR form (contains '/')
        if let Some((net, prefix)) = input.split_once('/') {
            let prefix: u8 = prefix.trim().parse().ok()?;
            // Parse the network part as IPv4 or IPv6.
            if let Ok(v4) = net.trim().parse::<Ipv4Addr>() {
                if prefix > 32 {
                    return None;
                }
                return Some(AllowlistEntry::Ipv4Cidr(v4, prefix));
            }
            if let Ok(v6) = net.trim().parse::<Ipv6Addr>() {
                if prefix > 128 {
                    return None;
                }
                return Some(AllowlistEntry::Ipv6Cidr(v6, prefix));
            }
            return None;
        }
        // Exact form
        input.parse::<IpAddr>().ok().map(AllowlistEntry::Exact)
    }

    /// Does this entry match the given IP?
    pub fn matches(&self, ip: IpAddr) -> bool {
        match *self {
            AllowlistEntry::Exact(expected) => expected == ip,
            AllowlistEntry::Ipv4Cidr(net, prefix_len) => {
                let net_u32 = u32::from(net);
                let ip_u32 = match ip {
                    IpAddr::V4(v4) => u32::from(v4),
                    IpAddr::V6(_) => return false,
                };
                if prefix_len == 0 {
                    return true;
                }
                let mask = if prefix_len >= 32 {
                    u32::MAX
                } else {
                    u32::MAX << (32 - prefix_len)
                };
                (net_u32 & mask) == (ip_u32 & mask)
            }
            AllowlistEntry::Ipv6Cidr(net, prefix_len) => {
                let net_octets = net.octets();
                let ip_octets = match ip {
                    IpAddr::V6(v6) => v6.octets(),
                    IpAddr::V4(_) => return false,
                };
                if prefix_len == 0 {
                    return true;
                }
                let full_bytes = (prefix_len / 8) as usize;
                let rem_bits = prefix_len % 8;
                for i in 0..full_bytes {
                    if net_octets[i] != ip_octets[i] {
                        return false;
                    }
                }
                if rem_bits > 0 && full_bytes < 16 {
                    let mask = 0xFFu8 << (8 - rem_bits);
                    if (net_octets[full_bytes] & mask) != (ip_octets[full_bytes] & mask) {
                        return false;
                    }
                }
                true
            }
        }
    }
}

/// A parsed, ready-to-test allowlist.
#[derive(Debug, Clone, Default)]
pub struct IpAllowlist {
    /// Parsed entries (in config order; order is irrelevant for matching).
    entries: Vec<AllowlistEntry>,
    /// Whether the allowlist is empty (= allow everyone).
    empty_means_allow_all: bool,
    /// Whether loopback is always permitted (always true; kept for clarity).
    allow_loopback: bool,
}

impl IpAllowlist {
    /// Build from a parsed entry list. `allow_loopback` is always true by
    /// design — see module docs.
    pub fn new(entries: Vec<AllowlistEntry>) -> Self {
        Self {
            entries,
            empty_means_allow_all: true,
            allow_loopback: true,
        }
    }

    /// Is this list empty (i.e. allow all peers)?
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of configured entries (diagnostics).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check whether a peer IP is permitted.
    ///
    /// - Empty list → allow everyone.
    /// - Loopback → always allowed.
    /// - Otherwise → allowed iff any entry matches.
    pub fn allows(&self, ip: IpAddr) -> bool {
        if self.empty_means_allow_all && self.entries.is_empty() {
            return true;
        }
        if self.allow_loopback && ip.is_loopback() {
            return true;
        }
        self.entries.iter().any(|e| e.matches(ip))
    }

    /// Convenience: check a `SocketAddr`'s IP (ignores the port).
    pub fn allows_socket(&self, addr: std::net::SocketAddr) -> bool {
        self.allows(addr.ip())
    }
}

/// Parse an allowlist string in the config's array form.
///
/// Each element may be an exact IP or a CIDR. Invalid elements are
/// skipped with a warn (the operator gets a log line telling them which
/// entry failed to parse, rather than the whole config being rejected).
pub fn parse_ip_allowlist(values: &[String]) -> Vec<AllowlistEntry> {
    let mut out = Vec::with_capacity(values.len());
    for raw in values {
        match AllowlistEntry::parse(raw) {
            Some(entry) => out.push(entry),
            None => {
                tracing::warn!(
                    entry = %raw,
                    "security.allowed_node_ips: skipping unparseable entry"
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }
    fn v6(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn empty_allowlist_allows_everything() {
        let l = IpAllowlist::new(vec![]);
        assert!(l.is_empty());
        assert!(l.allows(v4("203.0.113.5")));
        assert!(l.allows(v4("10.1.2.3")));
        assert!(l.allows(v6("2001:db8::1")));
    }

    #[test]
    fn exact_ipv4_matches_only_itself() {
        let l = IpAllowlist::new(vec![AllowlistEntry::Exact(v4("192.168.1.20"))]);
        assert!(l.allows(v4("192.168.1.20")));
        assert!(!l.allows(v4("192.168.1.21")));
    }

    #[test]
    fn exact_ipv6_matches_only_itself() {
        let l = IpAllowlist::new(vec![AllowlistEntry::Exact(v6("fd00::1"))]);
        assert!(l.allows(v6("fd00::1")));
        assert!(!l.allows(v6("fd00::2")));
    }

    #[test]
    fn cidr_v4_range() {
        let l = IpAllowlist::new(vec![AllowlistEntry::parse("10.0.0.0/24").unwrap()]);
        assert!(l.allows(v4("10.0.0.1")));
        assert!(l.allows(v4("10.0.0.254")));
        assert!(!l.allows(v4("10.0.1.1")));
        assert!(!l.allows(v4("10.1.0.1")));
    }

    #[test]
    fn cidr_v4_zero_prefix_allows_all_v4() {
        let l = IpAllowlist::new(vec![AllowlistEntry::parse("0.0.0.0/0").unwrap()]);
        assert!(l.allows(v4("8.8.8.8")));
        assert!(l.allows(v4("192.168.1.1")));
        // IPv6 still not matched by an IPv4 CIDR.
        assert!(!l.allows(v6("2001:db8::1")));
    }

    #[test]
    fn cidr_v6_range() {
        let l = IpAllowlist::new(vec![AllowlistEntry::parse("fd00::/8").unwrap()]);
        assert!(l.allows(v6("fd00::1")));
        assert!(l.allows(v6("fd00:abcd::1")));
        assert!(!l.allows(v6("fe80::1")));
    }

    #[test]
    fn loopback_always_allowed_even_when_list_is_restrictive() {
        let l = IpAllowlist::new(vec![AllowlistEntry::Exact(v4("192.168.1.20"))]);
        assert!(l.allows(v4("127.0.0.1")));
        assert!(l.allows(v6("::1")));
        // But loopback *CIDR* entries are not special — 127.0.0.0/8 is an
        // ordinary entry like any other.
        let l2 = IpAllowlist::new(vec![AllowlistEntry::parse("127.0.0.0/8").unwrap()]);
        assert!(l2.allows(v4("127.0.0.1")));
        assert!(!l2.allows(v4("10.0.0.1")));
    }

    #[test]
    fn parse_rejects_garbage_but_accepts_valid() {
        assert!(AllowlistEntry::parse("not-an-ip").is_none());
        assert!(AllowlistEntry::parse("").is_none());
        assert!(AllowlistEntry::parse("10.0.0.0/33").is_none());
        assert!(AllowlistEntry::parse("10.0.0.0/24").is_some());
        assert!(AllowlistEntry::parse("::1").is_some());
    }

    #[test]
    fn parse_allowlist_skips_invalid_entries() {
        let parsed = parse_ip_allowlist(&[
            "192.168.1.20".to_string(),
            "not-an-ip".to_string(),
            "10.0.0.0/24".to_string(),
        ]);
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn socket_addr_matching_ignores_port() {
        let l = IpAllowlist::new(vec![AllowlistEntry::Exact(v4("192.168.1.20"))]);
        let sa: std::net::SocketAddr = "192.168.1.20:19876".parse().unwrap();
        assert!(l.allows_socket(sa));
        let sa2: std::net::SocketAddr = "192.168.1.21:19876".parse().unwrap();
        assert!(!l.allows_socket(sa2));
    }
}
