//! Firewall rule grammar and configuration-disk format shared by the `jos`
//! host tooling and the `jos-firewall` unikernel.
//!
//! `no_std`, no `alloc`: everything parses borrowed `&str` so the
//! unikernel can read the config straight out of a disk sector buffer.
//!
//! Rule grammar (one rule per string, whitespace separated):
//!
//! ```text
//! <allow|deny> [tcp|udp|icmp|any] [<ip>|<ip>/<prefix>|any] [<port>|<a>-<b>]
//! ```
//!
//! Config disk (text, `#` comments, NUL-terminated):
//!
//! ```text
//! firewall sys-firewall
//! gateway 10.137.0.2
//! upstream dhcp
//! client work 10.137.0.3 02:4a:5b:98:d2:60
//! rule work allow tcp any 443
//! policy work deny
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

use core::fmt;
use core::net::Ipv4Addr;
use core::str::FromStr;

/// Guest PCI slot where `jos` attaches the raw config disk.
pub const CONFIG_DISK_SLOT: u32 = 30;
/// The config disk is padded to a multiple of this.
pub const SECTOR: usize = 512;
/// Upper bound `jos` enforces on the rendered config (64 KiB), so the
/// unikernel can read it into a fixed buffer.
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub what: &'static str,
}

impl ParseError {
    const fn new(what: &'static str) -> Self {
        ParseError { what }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.what)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ParseError {}

pub type Result<T> = core::result::Result<T, ParseError>;

/// An IPv4 network in CIDR notation; a bare address means `/32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ipv4Net {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl Ipv4Net {
    pub const fn new(addr: Ipv4Addr, prefix: u8) -> Self {
        Ipv4Net { addr, prefix }
    }

    /// Host bits; a prefix above 32 is treated as 32.
    fn host_bits(&self) -> u32 {
        32 - u32::from(self.prefix.min(32))
    }

    fn mask(&self) -> u32 {
        u32::MAX.checked_shl(self.host_bits()).unwrap_or(0)
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        (u32::from(ip) & self.mask()) == (u32::from(self.addr) & self.mask())
    }

    /// Number of addresses in the network.
    pub fn size(&self) -> u64 {
        1u64 << self.host_bits()
    }
}

impl FromStr for Ipv4Net {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self> {
        const BAD: ParseError = ParseError::new("invalid IPv4 address or network");
        let (ip, prefix) = match s.split_once('/') {
            Some((ip, p)) => (ip, p.parse::<u8>().map_err(|_| BAD)?),
            None => (s, 32),
        };
        if prefix > 32 {
            return Err(BAD);
        }
        let addr: Ipv4Addr = ip.parse().map_err(|_| BAD)?;
        Ok(Ipv4Net { addr, prefix })
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// A 48-bit Ethernet address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MacAddr(pub [u8; 6]);

impl FromStr for MacAddr {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self> {
        const BAD: ParseError = ParseError::new("invalid MAC address");
        let mut out = [0u8; 6];
        let mut parts = s.split(':');
        for slot in out.iter_mut() {
            let part = parts.next().ok_or(BAD)?;
            if part.len() != 2 {
                return Err(BAD);
            }
            *slot = u8::from_str_radix(part, 16).map_err(|_| BAD)?;
        }
        if parts.next().is_some() {
            return Err(BAD);
        }
        Ok(MacAddr(out))
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Action {
    #[default]
    Allow,
    Deny,
}

impl Action {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Deny => "deny",
        }
    }
}

impl FromStr for Action {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "allow" | "accept" => Ok(Action::Allow),
            "deny" | "drop" => Ok(Action::Deny),
            _ => Err(ParseError::new("unknown firewall action (allow|deny)")),
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Proto {
    #[default]
    Any,
    Tcp,
    Udp,
    Icmp,
}

impl Proto {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Proto::Any => "any",
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
            Proto::Icmp => "icmp",
        }
    }

    pub fn parse_keyword(s: &str) -> Option<Proto> {
        match s {
            "any" => Some(Proto::Any),
            "tcp" => Some(Proto::Tcp),
            "udp" => Some(Proto::Udp),
            "icmp" => Some(Proto::Icmp),
            _ => None,
        }
    }

    /// Map an IPv4 protocol number (6, 17, 1) to a [`Proto`]; `None` for
    /// anything the firewall does not classify.
    pub const fn from_ip_protocol(n: u8) -> Option<Proto> {
        match n {
            6 => Some(Proto::Tcp),
            17 => Some(Proto::Udp),
            1 => Some(Proto::Icmp),
            _ => None,
        }
    }
}

impl fmt::Display for Proto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    pub fn contains(&self, port: u16) -> bool {
        (self.start..=self.end).contains(&port)
    }
}

impl FromStr for PortRange {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self> {
        const BAD: ParseError = ParseError::new("invalid port or port range");
        let (a, b) = match s.split_once('-') {
            Some((a, b)) => (a, b),
            None => (s, s),
        };
        let start: u16 = a.parse().map_err(|_| BAD)?;
        let end: u16 = b.parse().map_err(|_| BAD)?;
        if start == 0 || end < start {
            return Err(BAD);
        }
        Ok(PortRange { start, end })
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

/// One outbound rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub action: Action,
    pub proto: Proto,
    /// Destination; `None` means any.
    pub dst: Option<Ipv4Net>,
    /// Destination port(s); only meaningful for tcp/udp/any.
    pub port: Option<PortRange>,
}

impl Rule {
    pub const fn new(action: Action) -> Self {
        Rule {
            action,
            proto: Proto::Any,
            dst: None,
            port: None,
        }
    }

    /// Does this rule apply to a packet?
    pub fn matches(&self, proto: Proto, dst: Ipv4Addr, port: u16) -> bool {
        let proto_ok = self.proto == Proto::Any || self.proto == proto;
        let dst_ok = match self.dst {
            Some(n) => n.contains(dst),
            None => true,
        };
        let port_ok = match self.port {
            Some(p) => proto != Proto::Icmp && p.contains(port),
            None => true,
        };
        proto_ok && dst_ok && port_ok
    }
}

impl FromStr for Rule {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self> {
        let mut words = s.split_whitespace();
        let action: Action = words
            .next()
            .ok_or(ParseError::new("empty firewall rule"))?
            .parse()?;
        let mut rule = Rule::new(action);
        let mut seen_proto = false;
        let mut seen_dst = false;
        for w in words {
            if !seen_proto {
                if let Some(p) = Proto::parse_keyword(w) {
                    rule.proto = p;
                    seen_proto = true;
                    continue;
                }
            }
            if !seen_dst && rule.port.is_none() {
                if w == "any" {
                    seen_dst = true;
                    continue;
                }
                if w.contains('.') {
                    rule.dst = Some(w.parse()?);
                    seen_dst = true;
                    continue;
                }
            }
            if rule.port.is_none() && w.chars().all(|c| c.is_ascii_digit() || c == '-') {
                rule.port = Some(w.parse()?);
                continue;
            }
            return Err(ParseError::new("unexpected token in firewall rule"));
        }
        if rule.port.is_some() && rule.proto == Proto::Icmp {
            return Err(ParseError::new("icmp rules cannot have a port"));
        }
        Ok(rule)
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.action, self.proto)?;
        match self.dst {
            Some(d) => write!(f, " {d}")?,
            None => f.write_str(" any")?,
        }
        if let Some(p) = self.port {
            write!(f, " {p}")?;
        }
        Ok(())
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Rule {
    fn serialize<S: serde::Serializer>(&self, s: S) -> core::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Rule {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> core::result::Result<Self, D::Error> {
        from_string(d)
    }
}

/// Deserialize a string and parse it. One straight path, with no early
/// return: llvm reports a generic function's coverage from its best
/// single instantiation, and a test deserializer that cannot produce a
/// string is a different instantiation from the one that can.
#[cfg(feature = "serde")]
fn from_string<'de, T, D>(d: D) -> core::result::Result<T, D::Error>
where
    T: FromStr<Err = ParseError>,
    D: serde::Deserializer<'de>,
{
    <String as serde::Deserialize>::deserialize(d)
        .and_then(|s| s.parse().map_err(serde::de::Error::custom))
}

#[cfg(feature = "serde")]
impl serde::Serialize for Action {
    fn serialize<S: serde::Serializer>(&self, s: S) -> core::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Action {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> core::result::Result<Self, D::Error> {
        from_string(d)
    }
}

/// First-match evaluation; `policy` when nothing matches.
pub fn evaluate<'a>(
    rules: impl IntoIterator<Item = &'a Rule>,
    policy: Action,
    proto: Proto,
    dst: Ipv4Addr,
    port: u16,
) -> Action {
    rules
        .into_iter()
        .find(|r| r.matches(proto, dst, port))
        .map(|r| r.action)
        .unwrap_or(policy)
}

/// One line of the config disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Directive<'a> {
    Firewall(&'a str),
    Gateway(Ipv4Addr),
    /// How the upstream side gets its address (`dhcp` for now).
    Upstream(&'a str),
    Client {
        name: &'a str,
        ip: Ipv4Addr,
        mac: MacAddr,
    },
    Rule {
        client: &'a str,
        rule: Rule,
    },
    Policy {
        client: &'a str,
        policy: Action,
    },
}

/// Parse a single config line; `None` for blank lines and comments.
pub fn parse_line(line: &str) -> Result<Option<Directive<'_>>> {
    const BAD: ParseError = ParseError::new("bad firewall config line");
    let line = line.trim_end_matches('\0').trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let mut w = line.split_whitespace();
    let d = match w.next() {
        Some("firewall") => Directive::Firewall(w.next().ok_or(BAD)?),
        Some("gateway") => Directive::Gateway(w.next().and_then(|s| s.parse().ok()).ok_or(BAD)?),
        Some("upstream") => Directive::Upstream(w.next().ok_or(BAD)?),
        Some("client") => Directive::Client {
            name: w.next().ok_or(BAD)?,
            ip: w.next().and_then(|s| s.parse().ok()).ok_or(BAD)?,
            mac: w.next().ok_or(BAD)?.parse()?,
        },
        Some("rule") => {
            let client = w.next().ok_or(BAD)?;
            let rest = w.remainder_str(line).ok_or(BAD)?;
            Directive::Rule {
                client,
                rule: rest.parse()?,
            }
        }
        Some("policy") => Directive::Policy {
            client: w.next().ok_or(BAD)?,
            policy: w.next().ok_or(BAD)?.parse()?,
        },
        _ => return Err(BAD),
    };
    Ok(Some(d))
}

/// Helper so `rule <client> <rest...>` can hand the untouched tail of the
/// line to the rule parser without allocating.
trait Remainder {
    fn remainder_str<'a>(&self, line: &'a str) -> Option<&'a str>;
}

impl Remainder for core::str::SplitWhitespace<'_> {
    fn remainder_str<'a>(&self, line: &'a str) -> Option<&'a str> {
        // `SplitWhitespace::remainder` is unstable; recompute from the
        // original line: skip the first two words.
        let mut rest = line.trim_start();
        for _ in 0..2 {
            let end = rest.find(char::is_whitespace)?;
            rest = rest[end..].trim_start();
        }
        Some(rest).filter(|r| !r.is_empty())
    }
}

/// Iterate over the directives in a config text, stopping at the first
/// NUL byte (the disk is zero-padded).
pub fn directives(text: &str) -> impl Iterator<Item = Result<Directive<'_>>> {
    let text = text.split('\0').next().unwrap_or("");
    text.lines().filter_map(|l| parse_line(l).transpose())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4net_parse_display_contains() {
        let n: Ipv4Net = "10.137.0.0/16".parse().unwrap();
        assert_eq!(format!("{n}"), "10.137.0.0/16");
        assert!(n.contains(Ipv4Addr::new(10, 137, 200, 1)));
        assert!(!n.contains(Ipv4Addr::new(10, 138, 0, 1)));
        let h: Ipv4Net = "1.1.1.1".parse().unwrap();
        assert_eq!(h.prefix, 32);
        assert!(h.contains(Ipv4Addr::new(1, 1, 1, 1)));
        assert!(!h.contains(Ipv4Addr::new(1, 1, 1, 2)));
        assert!("1.1.1.1/33".parse::<Ipv4Net>().is_err());
        assert!("1.1.1.1/x".parse::<Ipv4Net>().is_err());
        assert!("foo".parse::<Ipv4Net>().is_err());
        let all: Ipv4Net = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains(Ipv4Addr::new(8, 8, 8, 8)));
        assert_eq!(all.size(), 1 << 32);
        // Out-of-range prefixes built directly behave as /32, never panic.
        let odd = Ipv4Net::new(Ipv4Addr::new(1, 2, 3, 4), 200);
        assert_eq!(odd.size(), 1);
        assert!(odd.contains(Ipv4Addr::new(1, 2, 3, 4)));
    }

    #[test]
    fn mac_roundtrip() {
        let m: MacAddr = "02:4a:5b:98:d2:60".parse().unwrap();
        assert_eq!(m.0, [0x02, 0x4a, 0x5b, 0x98, 0xd2, 0x60]);
        assert_eq!(format!("{m}"), "02:4a:5b:98:d2:60");
        assert!("02:4a:5b:98:d2".parse::<MacAddr>().is_err());
        assert!("02:4a:5b:98:d2:60:00".parse::<MacAddr>().is_err());
        assert!("zz:4a:5b:98:d2:60".parse::<MacAddr>().is_err());
        assert!("2:4a:5b:98:d2:60".parse::<MacAddr>().is_err());
    }

    #[test]
    fn rule_grammar() {
        let r: Rule = "allow tcp 1.1.1.1/32 443".parse().unwrap();
        assert_eq!(r.action, Action::Allow);
        assert_eq!(r.proto, Proto::Tcp);
        assert_eq!(r.dst.unwrap().prefix, 32);
        assert_eq!(r.port.unwrap().start, 443);
        assert_eq!(format!("{r}"), "allow tcp 1.1.1.1/32 443");
        assert_eq!(
            format!("{}", "deny".parse::<Rule>().unwrap()),
            "deny any any"
        );
        let r: Rule = "allow udp any 53".parse().unwrap();
        assert_eq!(r.proto, Proto::Udp);
        assert!(r.dst.is_none());
        let r: Rule = "allow any 8000-8100".parse().unwrap();
        assert_eq!(
            r.port.unwrap(),
            PortRange {
                start: 8000,
                end: 8100
            }
        );
        let r: Rule = "allow 192.168.1.0/24".parse().unwrap();
        assert_eq!(r.dst.unwrap().prefix, 24);
        assert_eq!("allow icmp".parse::<Rule>().unwrap().proto, Proto::Icmp);
        assert!("allow icmp 80".parse::<Rule>().is_err());
        assert!("permit tcp".parse::<Rule>().is_err());
        assert!("allow tcp 443 1.1.1.1".parse::<Rule>().is_err());
        assert!("allow tcp 0".parse::<Rule>().is_err());
        assert!("allow tcp 100-50".parse::<Rule>().is_err());
        assert!("".parse::<Rule>().is_err());
        assert!("allow tcp bogus".parse::<Rule>().is_err());
        assert!("allow tcp 1.2.3/99".parse::<Rule>().is_err());
        assert!("allow tcp -5".parse::<Rule>().is_err());
        assert!("allow tcp 8-".parse::<Rule>().is_err());
        for text in ["allow icmp 10.0.0.0/8", "deny udp any 60000-61000"] {
            assert_eq!(format!("{}", text.parse::<Rule>().unwrap()), text);
        }
    }

    /// A formatter target that refuses to take more than `room` bytes.
    struct Cramped {
        room: usize,
    }

    impl fmt::Write for Cramped {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            self.room = self.room.checked_sub(s.len()).ok_or(fmt::Error)?;
            Ok(())
        }
    }

    #[test]
    fn rule_display_stops_at_the_first_write_error() {
        use core::fmt::Write as _;
        for text in ["allow tcp 1.1.1.1/32 443", "deny any any"] {
            let rule: Rule = text.parse().unwrap();
            for room in 0..text.len() {
                let mut w = Cramped { room };
                assert!(write!(w, "{rule}").is_err(), "{text} in {room} bytes");
            }
            assert!(write!(Cramped { room: text.len() }, "{rule}").is_ok());
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn rules_and_actions_deserialize_from_strings_only() {
        use serde::de::value::{Error, StrDeserializer, U32Deserializer};
        use serde::Deserialize;
        let s = |text| StrDeserializer::<Error>::new(text);
        assert_eq!(
            Rule::deserialize(s("deny")).unwrap(),
            Rule::new(Action::Deny)
        );
        assert!(Rule::deserialize(s("permit")).is_err());
        assert!(Rule::deserialize(U32Deserializer::<Error>::new(1)).is_err());
        assert_eq!(Action::deserialize(s("drop")).unwrap(), Action::Deny);
        assert!(Action::deserialize(s("maybe")).is_err());
        assert!(Action::deserialize(U32Deserializer::<Error>::new(1)).is_err());
    }

    #[test]
    fn evaluation_first_match_then_policy() {
        let rules = [
            "deny tcp 10.0.0.0/8".parse::<Rule>().unwrap(),
            "allow tcp any 443".parse().unwrap(),
            "allow udp 1.1.1.1 53".parse().unwrap(),
            "allow icmp".parse().unwrap(),
        ];
        let ev = |proto, dst, port| evaluate(&rules, Action::Deny, proto, dst, port);
        assert_eq!(
            ev(Proto::Tcp, Ipv4Addr::new(10, 1, 2, 3), 443),
            Action::Deny
        );
        assert_eq!(
            ev(Proto::Tcp, Ipv4Addr::new(93, 184, 216, 34), 443),
            Action::Allow
        );
        assert_eq!(
            ev(Proto::Tcp, Ipv4Addr::new(93, 184, 216, 34), 80),
            Action::Deny
        );
        assert_eq!(ev(Proto::Udp, Ipv4Addr::new(1, 1, 1, 1), 53), Action::Allow);
        assert_eq!(ev(Proto::Udp, Ipv4Addr::new(8, 8, 8, 8), 53), Action::Deny);
        assert_eq!(ev(Proto::Icmp, Ipv4Addr::new(8, 8, 8, 8), 0), Action::Allow);
        assert_eq!(
            evaluate(&[], Action::Allow, Proto::Tcp, Ipv4Addr::LOCALHOST, 1),
            Action::Allow
        );
        assert_eq!(Proto::from_ip_protocol(6), Some(Proto::Tcp));
        assert_eq!(Proto::from_ip_protocol(17), Some(Proto::Udp));
        assert_eq!(Proto::from_ip_protocol(1), Some(Proto::Icmp));
        assert_eq!(Proto::from_ip_protocol(2), None);
    }

    #[test]
    fn config_directives() {
        let text = "# generated\nfirewall sys-firewall\ngateway 10.137.0.2\nupstream dhcp\n\
                    client work 10.137.0.3 02:4a:5b:98:d2:60\nrule work allow tcp any 443\n\
                    rule work deny any 10.0.0.0/8\npolicy work deny\n\0\0\0garbage";
        let got: Vec<Directive> = directives(text).map(|d| d.unwrap()).collect();
        assert_eq!(got.len(), 7);
        assert_eq!(got[0], Directive::Firewall("sys-firewall"));
        assert_eq!(got[1], Directive::Gateway(Ipv4Addr::new(10, 137, 0, 2)));
        assert_eq!(got[2], Directive::Upstream("dhcp"));
        assert!(matches!(
            got[3],
            Directive::Client { name: "work", ip, mac }
                if ip == Ipv4Addr::new(10, 137, 0, 3) && mac.0[0] == 0x02
        ));
        assert!(
            matches!(got[4], Directive::Rule { client: "work", rule } if rule.proto == Proto::Tcp)
        );
        assert!(matches!(
            got[5],
            Directive::Rule { rule, .. } if rule.dst.unwrap().prefix == 8 && rule.action == Action::Deny
        ));
        assert_eq!(
            got[6],
            Directive::Policy {
                client: "work",
                policy: Action::Deny
            }
        );
        // Every directive with a word missing or wrong.
        for bad in [
            "bogus line",
            "firewall",
            "gateway",
            "gateway nowhere",
            "upstream",
            "client",
            "client work",
            "client work 10.0.0.1",
            "client work 10.0.0.1 zz",
            "rule",
            "rule work",
            "rule work permit",
            "policy",
            "policy work",
            "policy work maybe",
        ] {
            assert!(parse_line(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_line("   ").unwrap(), None);
        assert!(directives("nonsense\n").next().unwrap().is_err());
    }
}
