//! DHCPv4 (RFC 2131/2132): a server for the client qubes and a client for
//! the upstream link.

use alloc::vec::Vec;
use core::net::Ipv4Addr;

use jos_fwcfg::MacAddr;

use crate::config::Config;
use crate::packet::{chunk, field, ip_field};

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;

pub const BOOTREQUEST: u8 = 1;
pub const BOOTREPLY: u8 = 2;

pub const DISCOVER: u8 = 1;
pub const OFFER: u8 = 2;
pub const REQUEST: u8 = 3;
pub const ACK: u8 = 5;
pub const NAK: u8 = 6;

const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_PARAM_REQUEST: u8 = 55;
const OPT_END: u8 = 255;
const OPT_PAD: u8 = 0;

const MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const OPTIONS_OFFSET: usize = 240;
const FLAG_BROADCAST: u16 = 0x8000;
/// One option carries at most 255 bytes, i.e. 63 addresses.
const MAX_DNS: usize = 63;

/// Lease handed to client qubes (their addresses are fixed anyway).
pub const CLIENT_LEASE_SECS: u32 = 86400;
/// Mask handed to client qubes: `10.137.0.0/16`.
pub const CLIENT_MASK: Ipv4Addr = Ipv4Addr::new(255, 255, 0, 0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub op: u8,
    pub xid: u32,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr,
    pub chaddr: MacAddr,
    pub msg_type: Option<u8>,
    pub requested_ip: Option<Ipv4Addr>,
    pub server_id: Option<Ipv4Addr>,
    pub subnet_mask: Option<Ipv4Addr>,
    pub router: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub lease_secs: Option<u32>,
}

impl Default for Message {
    fn default() -> Self {
        Message {
            op: 0,
            xid: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: MacAddr::default(),
            msg_type: None,
            requested_ip: None,
            server_id: None,
            subnet_mask: None,
            router: None,
            dns: Vec::new(),
            lease_secs: None,
        }
    }
}

impl Message {
    /// `payload` is the UDP payload.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let (h, mut opts) = payload.split_at_checked(OPTIONS_OFFSET)?;
        if field(h, 236) != MAGIC || field(h, 1) != [1, 6] {
            return None; // not DHCP over Ethernet
        }
        let [op] = field(h, 0);
        let mut m = Message {
            op,
            xid: u32::from_be_bytes(field(h, 4)),
            flags: u16::from_be_bytes(field(h, 10)),
            ciaddr: ip_field(h, 12),
            yiaddr: ip_field(h, 16),
            siaddr: ip_field(h, 20),
            chaddr: MacAddr(field(h, 28)),
            ..Message::default()
        };
        let ip = |data: &[u8]| chunk(data, 0).map(Ipv4Addr::from);
        loop {
            let (code, data) = match opts {
                [OPT_END, ..] => break,
                [OPT_PAD, rest @ ..] => {
                    opts = rest;
                    continue;
                }
                [code, len, rest @ ..] => {
                    let (data, rest) = rest.split_at_checked(usize::from(*len))?;
                    opts = rest;
                    (*code, data)
                }
                _ => return None,
            };
            match code {
                OPT_MSG_TYPE => m.msg_type = data.first().copied(),
                OPT_REQUESTED_IP => m.requested_ip = ip(data),
                OPT_SERVER_ID => m.server_id = ip(data),
                OPT_SUBNET_MASK => m.subnet_mask = ip(data),
                OPT_ROUTER => m.router = ip(data),
                OPT_LEASE_TIME => m.lease_secs = chunk(data, 0).map(u32::from_be_bytes),
                OPT_DNS => {
                    m.dns = data
                        .as_chunks()
                        .0
                        .iter()
                        .map(|&c| Ipv4Addr::from(c))
                        .collect()
                }
                _ => {}
            }
        }
        Some(m)
    }

    pub fn wants_broadcast(&self) -> bool {
        self.flags & FLAG_BROADCAST != 0
    }
}

/// Serialiser for outgoing messages.
struct Builder {
    buf: Vec<u8>,
}

impl Builder {
    /// The fixed BOOTP header; `ciaddr` and `giaddr` stay zero.
    fn new(
        op: u8,
        xid: u32,
        flags: u16,
        chaddr: MacAddr,
        yiaddr: Ipv4Addr,
        siaddr: Ipv4Addr,
    ) -> Self {
        let zero = [0u8; 4];
        let buf = [
            [op, 1, 6, 0].as_slice(),
            &xid.to_be_bytes(),
            &[0, 0], // secs
            &flags.to_be_bytes(),
            &zero,
            &yiaddr.octets(),
            &siaddr.octets(),
            &zero,
            &chaddr.0,
            &[0; 10 + 64 + 128], // chaddr padding, sname, file
            &MAGIC,
        ]
        .concat();
        Builder { buf }
    }

    /// Append an option, truncating `data` to the 255 bytes it can hold.
    fn opt(mut self, code: u8, data: &[u8]) -> Self {
        let len = u8::try_from(data.len()).unwrap_or(u8::MAX);
        self.buf.extend_from_slice(&[code, len]);
        self.buf.extend(data.iter().take(usize::from(len)));
        self
    }

    fn opt_ip(self, code: u8, ip: Ipv4Addr) -> Self {
        self.opt(code, &ip.octets())
    }

    fn finish(mut self) -> Vec<u8> {
        self.buf.push(OPT_END);
        // BOOTP minimum size keeps picky clients happy.
        self.buf.resize(self.buf.len().max(300), 0);
        self.buf
    }
}

// ---------------------------------------------------------------- server

/// Parameters we hand out to client qubes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerParams {
    pub dns: Vec<Ipv4Addr>,
}

/// Reply to a client qube's DHCP request, or `None` to stay silent.
/// Returns `(msg_type, payload)`.
pub fn serve(cfg: &Config, params: &ServerParams, req: &Message) -> Option<(u8, Vec<u8>)> {
    if req.op != BOOTREQUEST {
        return None;
    }
    let client = cfg.client_by_mac(req.chaddr)?;
    let kind = match req.msg_type? {
        DISCOVER => OFFER,
        // Talking to another server.
        REQUEST if req.server_id.is_some_and(|s| s != cfg.gateway) => return None,
        REQUEST if req.requested_ip.unwrap_or(req.ciaddr) == client.ip => ACK,
        REQUEST => NAK,
        _ => return None,
    };
    let reply = |yiaddr, siaddr| {
        Builder::new(BOOTREPLY, req.xid, req.flags, req.chaddr, yiaddr, siaddr)
            .opt(OPT_MSG_TYPE, &[kind])
            .opt_ip(OPT_SERVER_ID, cfg.gateway)
    };
    let b = if kind == NAK {
        reply(Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED)
    } else {
        lease(reply(client.ip, cfg.gateway), cfg, params)
    };
    Some((kind, b.finish()))
}

fn lease(b: Builder, cfg: &Config, params: &ServerParams) -> Builder {
    let b = b
        .opt(OPT_LEASE_TIME, &CLIENT_LEASE_SECS.to_be_bytes())
        .opt_ip(OPT_SUBNET_MASK, CLIENT_MASK)
        .opt_ip(OPT_ROUTER, cfg.gateway);
    if params.dns.is_empty() {
        return b;
    }
    let dns: Vec<u8> = params
        .dns
        .iter()
        .take(MAX_DNS)
        .flat_map(|d| d.octets())
        .collect();
    b.opt(OPT_DNS, &dns)
}

// ---------------------------------------------------------------- client

const PARAMS: [u8; 3] = [OPT_SUBNET_MASK, OPT_ROUTER, OPT_DNS];

fn client_builder(xid: u32, mac: MacAddr, kind: u8) -> Builder {
    let none = Ipv4Addr::UNSPECIFIED;
    Builder::new(BOOTREQUEST, xid, FLAG_BROADCAST, mac, none, none).opt(OPT_MSG_TYPE, &[kind])
}

pub fn discover(xid: u32, mac: MacAddr) -> Vec<u8> {
    client_builder(xid, mac, DISCOVER)
        .opt(OPT_PARAM_REQUEST, &PARAMS)
        .finish()
}

pub fn request(xid: u32, mac: MacAddr, offered: Ipv4Addr, server: Ipv4Addr) -> Vec<u8> {
    client_builder(xid, mac, REQUEST)
        .opt_ip(OPT_REQUESTED_IP, offered)
        .opt_ip(OPT_SERVER_ID, server)
        .opt(OPT_PARAM_REQUEST, &PARAMS)
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::tests::parsing_prefixes;

    const WORK_MAC: MacAddr = MacAddr([2, 0x4a, 0x5b, 0x98, 0xd2, 0x60]);
    const WORK_IP: Ipv4Addr = Ipv4Addr::new(10, 137, 0, 3);

    fn cfg() -> Config {
        Config::parse(
            "firewall fw\ngateway 10.137.0.2\nupstream dhcp\nclient work 10.137.0.3 02:4a:5b:98:d2:60\n",
        )
        .unwrap()
    }

    /// What `serve` answers to `msg`, by message type.
    fn kind_of(params: &ServerParams, msg: &Message) -> Option<u8> {
        serve(&cfg(), params, msg).map(|(kind, _)| kind)
    }

    #[test]
    fn client_messages_parse_back() {
        let d = Message::parse(&discover(0xabcd, WORK_MAC)).unwrap();
        assert_eq!(d.op, BOOTREQUEST);
        assert_eq!(d.xid, 0xabcd);
        assert_eq!(d.msg_type, Some(DISCOVER));
        assert_eq!(d.chaddr, WORK_MAC);
        assert!(d.wants_broadcast());
        let r = Message::parse(&request(
            1,
            WORK_MAC,
            Ipv4Addr::new(192, 168, 1, 7),
            Ipv4Addr::new(192, 168, 1, 1),
        ))
        .unwrap();
        assert_eq!(r.msg_type, Some(REQUEST));
        assert_eq!(r.requested_ip, Some(Ipv4Addr::new(192, 168, 1, 7)));
        assert_eq!(r.server_id, Some(Ipv4Addr::new(192, 168, 1, 1)));
    }

    #[test]
    fn server_offers_and_acks_known_client() {
        let cfg = cfg();
        let params = ServerParams {
            dns: alloc::vec![Ipv4Addr::new(9, 9, 9, 9), Ipv4Addr::new(1, 1, 1, 1)],
        };
        let d = Message::parse(&discover(7, WORK_MAC)).unwrap();
        let (kind, payload) = serve(&cfg, &params, &d).unwrap();
        assert_eq!(kind, OFFER);
        let offer = Message::parse(&payload).unwrap();
        assert_eq!(offer.op, BOOTREPLY);
        assert_eq!(offer.xid, 7);
        assert_eq!(offer.yiaddr, WORK_IP);
        assert_eq!(offer.siaddr, cfg.gateway);
        assert_eq!(offer.server_id, Some(cfg.gateway));
        assert_eq!(offer.router, Some(cfg.gateway));
        assert_eq!(offer.subnet_mask, Some(CLIENT_MASK));
        assert_eq!(offer.dns, params.dns);
        assert_eq!(offer.lease_secs, Some(CLIENT_LEASE_SECS));
        assert!(payload.len() >= 300);

        let r = Message::parse(&request(7, WORK_MAC, offer.yiaddr, cfg.gateway)).unwrap();
        assert_eq!(kind_of(&params, &r), Some(ACK));

        // Asking for the wrong address is refused, and the refusal
        // carries no lease.
        let bad = Message::parse(&request(
            7,
            WORK_MAC,
            Ipv4Addr::new(10, 137, 0, 9),
            cfg.gateway,
        ))
        .unwrap();
        let (kind, payload) = serve(&cfg, &params, &bad).unwrap();
        assert_eq!(kind, NAK);
        let nak = Message::parse(&payload).unwrap();
        assert_eq!(nak.yiaddr, Ipv4Addr::UNSPECIFIED);
        assert_eq!(nak.siaddr, Ipv4Addr::UNSPECIFIED);
        assert_eq!(nak.server_id, Some(cfg.gateway));
        assert_eq!(nak.lease_secs, None);
        // Requests meant for another server are ignored.
        let other = Message::parse(&request(
            7,
            WORK_MAC,
            offer.yiaddr,
            Ipv4Addr::new(192, 168, 1, 1),
        ))
        .unwrap();
        assert_eq!(serve(&cfg, &params, &other), None);
        // A REQUEST without a server id (renewal) is judged on ciaddr.
        let renew = Message {
            ciaddr: WORK_IP,
            server_id: None,
            requested_ip: None,
            ..r
        };
        assert_eq!(kind_of(&params, &renew), Some(ACK));
    }

    #[test]
    fn only_discover_and_request_from_known_clients_are_answered() {
        let params = ServerParams::default();
        let d = Message::parse(&discover(1, WORK_MAC)).unwrap();
        let stranger = Message {
            chaddr: MacAddr([2, 0, 0, 0, 0, 0x99]),
            ..d.clone()
        };
        let reply = Message {
            op: BOOTREPLY,
            ..d.clone()
        };
        let untyped = Message {
            msg_type: None,
            ..d.clone()
        };
        let release = Message {
            msg_type: Some(7),
            ..d.clone()
        };
        for m in [&stranger, &reply, &untyped, &release] {
            assert_eq!(kind_of(&params, m), None, "{m:?}");
        }
        // Without upstream DNS servers the lease simply has no DNS option.
        let (_, offer) = serve(&cfg(), &params, &d).unwrap();
        assert!(Message::parse(&offer).unwrap().dns.is_empty());
    }

    #[test]
    fn the_dns_option_holds_at_most_63_servers() {
        let params = ServerParams {
            dns: (0..64).map(|i| Ipv4Addr::new(10, 0, 0, i)).collect(),
        };
        let d = Message::parse(&discover(1, WORK_MAC)).unwrap();
        let (_, offer) = serve(&cfg(), &params, &d).unwrap();
        assert_eq!(Message::parse(&offer).unwrap().dns, params.dns[..63]);
    }

    #[test]
    fn pad_options_are_skipped() {
        let p = discover(1, WORK_MAC);
        let padded = [
            &p[..OPTIONS_OFFSET],
            &[OPT_PAD, OPT_PAD],
            &p[OPTIONS_OFFSET..],
        ]
        .concat();
        assert_eq!(Message::parse(&padded), Message::parse(&p));
    }

    #[test]
    fn malformed_messages_do_not_panic() {
        let p = discover(1, WORK_MAC);
        // Parsing needs the whole header and options up to END.
        let end = OPTIONS_OFFSET + 3 + 2 + PARAMS.len();
        assert_eq!(p[end], OPT_END);
        assert_eq!(
            parsing_prefixes(&p, &|b| Message::parse(b).is_some()),
            (end + 1..=p.len()).collect::<Vec<_>>()
        );
        let mut long = p.clone();
        long[OPTIONS_OFFSET + 1] = 200; // length overruns the buffer
        let mut magic = p.clone();
        magic[236] = 0;
        let mut htype = p.clone();
        htype[1] = 6; // IEEE 802
        let mut hlen = p.clone();
        hlen[2] = 8;
        for bad in [long, magic, htype, hlen] {
            assert_eq!(Message::parse(&bad), None);
        }
    }
}
