//! Precompiled network-agnostic inbound access control (`AccessGate`).
//!
//! Replaces per-request string CIDR parsing and EasyTier/Tailscale hardcoding with:
//! - IPv4 & IPv6 CIDR matching (with automatic `::ffff:a.b.c.d` IPv4-mapped normalization)
//! - Exact & wildcard `Host` / `Origin` matching (including reverse-proxy ports 80/443/omitted)
//! - Optional named presets (`easytier`, `tailscale`) and 100% backward compat with legacy fields
//! - Scoped tokens (`read_only` viewer tokens or peer federation tokens)

use crate::config::{parse_ip_cidr, WebConfig, DEFAULT_EASYTIER_CIDR};
use std::net::IpAddr;

const TAILSCALE_V4_CIDR: &str = "100.64.0.0/10";
const TAILSCALE_V6_ULA_CIDR: &str = "fd7a:115c:a1e0::/48";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpCidr {
    V4 { net: u32, mask: u32 },
    V6 { net: u128, mask: u128 },
}

impl IpCidr {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (ip, prefix) = parse_ip_cidr(raw)?;
        Ok(match ip {
            IpAddr::V4(v4) => {
                let mask = if prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - prefix)
                };
                IpCidr::V4 {
                    net: u32::from(v4) & mask,
                    mask,
                }
            }
            IpAddr::V6(v6) => {
                let mask = if prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - prefix)
                };
                IpCidr::V6 {
                    net: u128::from(v6) & mask,
                    mask,
                }
            }
        })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (*self, ip) {
            (IpCidr::V4 { net, mask }, IpAddr::V4(v4)) => (u32::from(v4) & mask) == net,
            (IpCidr::V4 { net, mask }, IpAddr::V6(v6)) => v6
                .to_ipv4_mapped()
                .is_some_and(|v4| (u32::from(v4) & mask) == net),
            (IpCidr::V6 { net, mask }, IpAddr::V6(v6)) => (u128::from(v6) & mask) == net,
            (IpCidr::V6 { .. }, IpAddr::V4(_)) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPattern {
    Exact {
        host: String,
        port: Option<u16>,
    },
    WildcardSuffix {
        suffix: String, // e.g. ".ts.net" for "*.ts.net"
        port: Option<u16>,
    },
}

impl HostPattern {
    pub fn parse(raw: &str) -> Option<Self> {
        let (host_part, port) = split_host_port(raw)?;
        let lower = host_part.to_ascii_lowercase();
        if lower.is_empty() || lower.contains('/') || lower.contains('@') {
            return None;
        }
        if let Some(suffix) = lower.strip_prefix("*.") {
            if !suffix.is_empty() {
                return Some(HostPattern::WildcardSuffix {
                    suffix: format!(".{suffix}"),
                    port,
                });
            }
            return None;
        }
        Some(HostPattern::Exact { host: lower, port })
    }

    pub fn matches(&self, host_only_lower: &str, req_port: Option<u16>, web_port: u16) -> bool {
        let (host_ok, allowed_port) = match self {
            HostPattern::Exact { host, port } => (host_only_lower == host, *port),
            HostPattern::WildcardSuffix { suffix, port } => (
                host_only_lower.ends_with(suffix.as_str()) && host_only_lower.len() > suffix.len(),
                *port,
            ),
        };
        if !host_ok {
            return false;
        }
        match allowed_port {
            Some(p) => req_port == Some(p),
            None => req_port.is_none_or(|p| p == web_port || p == 80 || p == 443),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedScopedToken {
    pub label: String,
    pub secret: Vec<u8>,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenAccess {
    Admin,
    ReadOnly,
}

/// Precompiled inbound access policy evaluated on every Web request in O(rules) with zero allocations.
#[derive(Debug, Clone)]
pub struct AccessGate {
    port: u16,
    bind: String,
    bracketed_bind: String,
    allowed_cidrs: Vec<IpCidr>,
    allowed_hosts: Vec<HostPattern>,
    scoped_tokens: Vec<ResolvedScopedToken>,
}

impl AccessGate {
    pub fn from_config(web: &WebConfig) -> Self {
        let mut allowed_cidrs = Vec::new();
        let mut allowed_hosts = Vec::new();

        // Loopback is always permitted.
        if let Ok(c) = IpCidr::parse("127.0.0.0/8") {
            allowed_cidrs.push(c);
        }
        if let Ok(c) = IpCidr::parse("::1/128") {
            allowed_cidrs.push(c);
        }

        // Network-agnostic explicit CIDRs & hosts.
        for raw in &web.allowed_cidrs {
            if let Ok(c) = IpCidr::parse(raw) {
                allowed_cidrs.push(c);
            }
        }
        for raw in &web.allowed_hosts {
            if let Some(p) = HostPattern::parse(raw) {
                allowed_hosts.push(p);
            }
        }

        // Presets + legacy fields (100% backward compatible).
        let has_preset = |name: &str| {
            web.presets
                .iter()
                .any(|p| p.trim().eq_ignore_ascii_case(name))
        };
        if web.allow_easytier || has_preset("easytier") {
            if web.easytier_cidrs.is_empty() {
                if let Ok(c) = IpCidr::parse(DEFAULT_EASYTIER_CIDR) {
                    allowed_cidrs.push(c);
                }
            } else {
                for raw in &web.easytier_cidrs {
                    if let Ok(c) = IpCidr::parse(raw) {
                        allowed_cidrs.push(c);
                    }
                }
            }
        }
        if web.allow_tailscale || has_preset("tailscale") {
            if let Ok(c) = IpCidr::parse(TAILSCALE_V4_CIDR) {
                allowed_cidrs.push(c);
            }
            if let Ok(c) = IpCidr::parse(TAILSCALE_V6_ULA_CIDR) {
                allowed_cidrs.push(c);
            }
            if let Some(p) = HostPattern::parse("*.ts.net") {
                allowed_hosts.push(p);
            }
        }

        let bind = web.bind.trim().to_string();
        let bracketed_bind = if bind.contains(':') && !bind.starts_with('[') {
            format!("[{bind}]")
        } else {
            bind.clone()
        };

        let mut scoped_tokens = Vec::new();
        for st in &web.scoped_tokens {
            let val = if !st.token.trim().is_empty() {
                Some(st.token.trim().to_string())
            } else if !st.token_env.trim().is_empty() {
                std::env::var(st.token_env.trim())
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            };
            if let Some(secret) = val {
                scoped_tokens.push(ResolvedScopedToken {
                    label: if st.label.trim().is_empty() {
                        "scoped".into()
                    } else {
                        st.label.trim().to_string()
                    },
                    secret: secret.into_bytes(),
                    read_only: st.read_only,
                });
            }
        }

        Self {
            port: web.port,
            bind,
            bracketed_bind,
            allowed_cidrs,
            allowed_hosts,
            scoped_tokens,
        }
    }

    /// True if peer `ip` is permitted by loopback or any configured IPv4/IPv6 CIDR / preset.
    pub fn is_allowed_peer_ip(&self, ip: IpAddr) -> bool {
        self.allowed_cidrs.iter().any(|cidr| cidr.contains(ip))
    }

    /// True if `Host` (or `Origin` host:port) is permitted (loopback, bound IP, allowed CIDRs, or `allowed_hosts`).
    pub fn is_allowed_host(&self, host: &str) -> bool {
        let Some((host_only, port_opt)) = split_host_port(host) else {
            return false;
        };
        let lower = host_only.to_ascii_lowercase();

        // Explicit/preset hostnames (e.g. `console.example.com`, `console.example.com:8443`, `*.ts.net`)
        // work behind reverse proxies (80/443/omitted, explicit configured port, or direct web.port).
        if self
            .allowed_hosts
            .iter()
            .any(|p| p.matches(&lower, port_opt, self.port))
        {
            return true;
        }

        let port_matches = port_opt == Some(self.port)
            || (port_opt.is_none() && (self.port == 80 || self.port == 443));
        if !port_matches {
            return false;
        }
        if matches!(lower.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
            return true;
        }
        if self.bind != "127.0.0.1"
            && self.bind != "0.0.0.0"
            && self.bind != "::"
            && (lower == self.bind.to_ascii_lowercase()
                || lower == self.bracketed_bind.to_ascii_lowercase())
        {
            return true;
        }

        // Literal IPv4 or bracketed IPv6 in Host header matched against compiled CIDRs.
        let unbracketed = lower
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(&lower);
        if let Ok(ip) = unbracketed.parse::<IpAddr>() {
            return self.is_allowed_peer_ip(ip);
        }
        false
    }

    /// Check `candidate` against the daemon's primary token and any configured scoped tokens (constant-time).
    pub fn authorize_token(&self, candidate: &[u8], primary_token: &[u8]) -> Option<TokenAccess> {
        if ct_eq(candidate, primary_token) {
            return Some(TokenAccess::Admin);
        }
        for st in &self.scoped_tokens {
            if ct_eq(candidate, &st.secret) {
                return Some(if st.read_only {
                    TokenAccess::ReadOnly
                } else {
                    TokenAccess::Admin
                });
            }
        }
        None
    }
}

pub fn split_host_port(host: &str) -> Option<(&str, Option<u16>)> {
    let h = host.trim();
    if h.is_empty() || h.contains('/') || h.contains('@') {
        return None;
    }
    if let Some(after_open) = h.strip_prefix('[') {
        let close = after_open.find(']')?;
        let host_only = &h[..=close + 1];
        let rest = &after_open[close + 1..];
        if rest.is_empty() {
            return Some((host_only, None));
        }
        let port = rest.strip_prefix(':')?.parse::<u16>().ok()?;
        return Some((host_only, Some(port)));
    }
    if let Some((host_only, port_str)) = h.rsplit_once(':') {
        if let Ok(port) = port_str.parse::<u16>() {
            return (!host_only.is_empty()).then_some((host_only, Some(port)));
        }
    }
    Some((h, None))
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScopedTokenConfig;

    #[test]
    fn ipv4_ipv6_and_ipv4_mapped_cidrs() {
        let cfg = WebConfig {
            allow_easytier: false,
            allowed_cidrs: vec!["10.88.0.0/16".into(), "fd00:1234::/32".into()],
            presets: vec!["tailscale".into()],
            allowed_hosts: vec!["console.example.com".into(), "*.corp.internal".into()],
            ..WebConfig::default()
        };
        let gate = AccessGate::from_config(&cfg);

        // Loopback always allowed
        assert!(gate.is_allowed_peer_ip("127.0.0.1".parse().unwrap()));
        assert!(gate.is_allowed_peer_ip("::1".parse().unwrap()));
        // Custom WireGuard IPv4 & IPv6 ULA allowed
        assert!(gate.is_allowed_peer_ip("10.88.5.42".parse().unwrap()));
        assert!(gate.is_allowed_peer_ip("::ffff:10.88.5.42".parse().unwrap()));
        assert!(gate.is_allowed_peer_ip("fd00:1234::abcd".parse().unwrap()));
        // Tailscale preset allows BOTH CGNAT IPv4 AND IPv6 ULA
        assert!(gate.is_allowed_peer_ip("100.101.2.3".parse().unwrap()));
        assert!(gate.is_allowed_peer_ip("fd7a:115c:a1e0::42".parse().unwrap()));
        // LAN & public rejected
        assert!(!gate.is_allowed_peer_ip("192.168.1.5".parse().unwrap()));
        assert!(!gate.is_allowed_peer_ip("fd00:9999::1".parse().unwrap()));
        assert!(!gate.is_allowed_peer_ip("8.8.8.8".parse().unwrap()));

        // Reverse proxy custom hosts & wildcards (omitted port, 80, 443, and web.port)
        assert!(gate.is_allowed_host("console.example.com"));
        assert!(gate.is_allowed_host("console.example.com:443"));
        assert!(gate.is_allowed_host("console.example.com:27418"));
        assert!(!gate.is_allowed_host("console.example.com:9999"));
        assert!(gate.is_allowed_host("dev.corp.internal"));
        assert!(gate.is_allowed_host("mac.tail123.ts.net"));
        assert!(gate.is_allowed_host("[fd00:1234::1]:27418"));
        assert!(!gate.is_allowed_host("evil.example.com:27418"));

        // Explicit custom reverse-proxy port in allowed_hosts & default HTTP port 80 omitted-port loopback.
        let cfg_custom = WebConfig {
            port: 80,
            allowed_hosts: vec!["proxy.example.com:8443".into()],
            ..WebConfig::default()
        };
        let gate_custom = AccessGate::from_config(&cfg_custom);
        assert!(gate_custom.is_allowed_host("127.0.0.1"));
        assert!(gate_custom.is_allowed_host("localhost"));
        assert!(gate_custom.is_allowed_host("proxy.example.com:8443"));
        assert!(!gate_custom.is_allowed_host("proxy.example.com:443"));
    }

    #[test]
    fn scoped_tokens_distinguish_admin_and_readonly() {
        let cfg = WebConfig {
            scoped_tokens: vec![
                ScopedTokenConfig {
                    label: "viewer".into(),
                    token: "ro-secret".into(),
                    token_env: String::new(),
                    read_only: true,
                },
                ScopedTokenConfig {
                    label: "peer".into(),
                    token: "rw-secret".into(),
                    token_env: String::new(),
                    read_only: false,
                },
            ],
            ..WebConfig::default()
        };
        let gate = AccessGate::from_config(&cfg);
        assert_eq!(
            gate.authorize_token(b"primary", b"primary"),
            Some(TokenAccess::Admin)
        );
        assert_eq!(
            gate.authorize_token(b"ro-secret", b"primary"),
            Some(TokenAccess::ReadOnly)
        );
        assert_eq!(
            gate.authorize_token(b"rw-secret", b"primary"),
            Some(TokenAccess::Admin)
        );
        assert_eq!(gate.authorize_token(b"wrong", b"primary"), None);
    }
}
