//! The config disk written by `jos` (see `jos_fwcfg`) turned into an
//! owned client table.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::net::Ipv4Addr;

use jos_fwcfg::{evaluate, Action, Directive, MacAddr, ParseError, Proto, Rule};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub name: String,
    pub ip: Ipv4Addr,
    pub mac: MacAddr,
    pub rules: Vec<Rule>,
    pub policy: Action,
}

impl Client {
    /// First matching rule, else the client's policy.
    pub fn decide(&self, proto: Proto, dst: Ipv4Addr, port: u16) -> Action {
        evaluate(&self.rules, self.policy, proto, dst, port)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub name: String,
    /// Our address on the downstream side (what clients use as router).
    pub gateway: Ipv4Addr,
    /// `true` for `upstream dhcp`; a literal address pins it statically.
    pub upstream_dhcp: bool,
    pub upstream_static: Option<Ipv4Addr>,
    pub clients: Vec<Client>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Parse { line: usize, error: ParseError },
    MissingGateway,
    UnknownClient { line: usize, client: String },
    DuplicateClient(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Parse { line, error } => write!(f, "line {line}: {error}"),
            ConfigError::MissingGateway => f.write_str("config has no `gateway` line"),
            ConfigError::UnknownClient { line, client } => {
                write!(f, "line {line}: rule for unknown client `{client}`")
            }
            ConfigError::DuplicateClient(c) => write!(f, "client `{c}` listed twice"),
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let text = text.split('\0').next().unwrap_or("");
        let mut name = String::new();
        let mut gateway = None;
        let mut upstream_dhcp = true;
        let mut upstream_static = None;
        let mut clients: Vec<Client> = Vec::new();
        for (idx, raw) in text.lines().enumerate() {
            let line = idx.saturating_add(1);
            let Some(d) =
                jos_fwcfg::parse_line(raw).map_err(|error| ConfigError::Parse { line, error })?
            else {
                continue;
            };
            match d {
                Directive::Firewall(n) => name = String::from(n),
                Directive::Gateway(ip) => gateway = Some(ip),
                Directive::Upstream(mode) => match mode.parse::<Ipv4Addr>() {
                    Ok(ip) => {
                        upstream_dhcp = false;
                        upstream_static = Some(ip);
                    }
                    Err(_) => upstream_dhcp = true,
                },
                Directive::Client {
                    name: cname,
                    ip,
                    mac,
                } => {
                    if clients.iter().any(|c| c.name == cname) {
                        return Err(ConfigError::DuplicateClient(String::from(cname)));
                    }
                    clients.push(Client {
                        name: String::from(cname),
                        ip,
                        mac,
                        rules: Vec::new(),
                        policy: Action::Allow,
                    });
                }
                Directive::Rule { client, rule } => {
                    Self::client_mut(&mut clients, client, line)?
                        .rules
                        .push(rule);
                }
                Directive::Policy { client, policy } => {
                    Self::client_mut(&mut clients, client, line)?.policy = policy;
                }
            }
        }
        Ok(Config {
            name,
            gateway: gateway.ok_or(ConfigError::MissingGateway)?,
            upstream_dhcp,
            upstream_static,
            clients,
        })
    }

    fn client_mut<'a>(
        clients: &'a mut [Client],
        name: &str,
        line: usize,
    ) -> Result<&'a mut Client, ConfigError> {
        clients
            .iter_mut()
            .find(|c| c.name == name)
            .ok_or_else(|| ConfigError::UnknownClient {
                line,
                client: String::from(name),
            })
    }

    pub fn client_by_mac(&self, mac: MacAddr) -> Option<&Client> {
        self.clients.iter().find(|c| c.mac == mac)
    }

    pub fn client_by_ip(&self, ip: Ipv4Addr) -> Option<&Client> {
        self.clients.iter().find(|c| c.ip == ip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    const TEXT: &str = "# rendered by jos\n\
firewall sys-firewall\n\
gateway 10.137.0.2\n\
upstream dhcp\n\
client work 10.137.0.3 02:4a:5b:98:d2:60\n\
rule work allow tcp any 443\n\
rule work deny any 10.0.0.0/8\n\
policy work deny\n\
client personal 10.137.0.4 02:4a:00:00:00:04\n\
\0\0\0garbage after NUL";

    #[test]
    fn parses_rendered_config() {
        let c = Config::parse(TEXT).unwrap();
        assert_eq!(c.name, "sys-firewall");
        assert_eq!(c.gateway, Ipv4Addr::new(10, 137, 0, 2));
        assert!(c.upstream_dhcp);
        assert_eq!(c.clients.len(), 2);
        let work = c.client_by_ip(Ipv4Addr::new(10, 137, 0, 3)).unwrap();
        assert_eq!(work.name, "work");
        assert_eq!(work.rules.len(), 2);
        assert_eq!(work.policy, Action::Deny);
        assert_eq!(
            work.decide(Proto::Tcp, Ipv4Addr::new(1, 1, 1, 1), 443),
            Action::Allow
        );
        assert_eq!(
            work.decide(Proto::Tcp, Ipv4Addr::new(1, 1, 1, 1), 80),
            Action::Deny
        );
        let personal = c.client_by_mac(MacAddr([2, 0x4a, 0, 0, 0, 4])).unwrap();
        assert_eq!(personal.policy, Action::Allow);
        assert_eq!(
            personal.decide(Proto::Udp, Ipv4Addr::new(8, 8, 8, 8), 53),
            Action::Allow
        );
    }

    #[test]
    fn errors_are_located_and_explained() {
        let unknown = ConfigError::UnknownClient {
            line: 2,
            client: String::from("nobody"),
        };
        assert_eq!(
            Config::parse("gateway 10.0.0.1\nrule nobody allow\n"),
            Err(unknown.clone())
        );
        assert_eq!(
            Config::parse("gateway 10.0.0.1\npolicy nobody deny\n"),
            Err(unknown.clone())
        );
        assert_eq!(
            unknown.to_string(),
            "line 2: rule for unknown client `nobody`"
        );
        assert_eq!(
            Config::parse("client a 1.2.3.4 00:00:00:00:00:01\n"),
            Err(ConfigError::MissingGateway)
        );
        assert_eq!(
            ConfigError::MissingGateway.to_string(),
            "config has no `gateway` line"
        );
        let error = jos_fwcfg::parse_line("bogus line").unwrap_err();
        let bogus = Config::parse("gateway 1.2.3.4\nbogus line\n").unwrap_err();
        assert_eq!(bogus.to_string(), alloc::format!("line 2: {error}"));
        assert_eq!(bogus, ConfigError::Parse { line: 2, error });
        let twice = Config::parse(
            "gateway 1.2.3.4\nclient a 1.2.3.5 00:00:00:00:00:01\nclient a 1.2.3.6 00:00:00:00:00:02\n",
        )
        .unwrap_err();
        assert_eq!(twice, ConfigError::DuplicateClient(String::from("a")));
        assert_eq!(twice.to_string(), "client `a` listed twice");
    }

    #[test]
    fn static_upstream() {
        let c = Config::parse("gateway 10.137.0.2\nupstream 192.168.1.40\n").unwrap();
        assert!(!c.upstream_dhcp);
        assert_eq!(c.upstream_static, Some(Ipv4Addr::new(192, 168, 1, 40)));
    }
}
