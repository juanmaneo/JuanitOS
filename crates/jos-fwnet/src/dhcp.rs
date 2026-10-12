//! DHCPv4 (RFC 2131/2132): a server for the client qubes and a client for
//! the upstream link.

use alloc::vec::Vec;
use core::net::Ipv4Addr;

use jos_fwcfg::MacAddr;

use crate::config::Config;
use crate::packet::{be16, be32, ipv4_at, mac_at, put_be16, put_be32, put_bytes, put_ipv4};

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
        if payload.get(236..OPTIONS_OFFSET)? != MAGIC {
            return None;
        }
        if *payload.get(1)? != 1 || *payload.get(2)? != 6 {
            return None; // not Ethernet
        }
        let mut m = Message {
            op: *payload.first()?,
            xid: be32(payload, 4)?,
            flags: be16(payload, 10)?,
            ciaddr: ipv4_at(payload, 12)?,
            yiaddr: ipv4_at(payload, 16)?,
            siaddr: ipv4_at(payload, 20)?,
            chaddr: mac_at(payload, 28)?,
            ..Message::default()
        };
        let mut off = OPTIONS_OFFSET;
        loop {
            let code = *payload.get(off)?;
            match code {
                OPT_END => break,
                OPT_PAD => {
                    off = off.checked_add(1)?;
                    continue;
                }
                _ => {}
            }
            let len = usize::from(*payload.get(off.checked_add(1)?)?);
            let start = off.checked_add(2)?;
            let end = start.checked_add(len)?;
            let data = payload.get(start..end)?;
            match code {
                OPT_MSG_TYPE => m.msg_type = data.first().copied(),
                OPT_REQUESTED_IP => m.requested_ip = ipv4_at(data, 0),
                OPT_SERVER_ID => m.server_id = ipv4_at(data, 0),
                OPT_SUBNET_MASK => m.subnet_mask = ipv4_at(data, 0),
                OPT_ROUTER => m.router = ipv4_at(data, 0),
                OPT_LEASE_TIME => m.lease_secs = be32(data, 0),
                OPT_DNS => {
                    m.dns = data
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| Ipv4Addr::from(*c))
                        .collect();
                }
                _ => {}
            }
            off = end;
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
    fn new(op: u8, xid: u32, flags: u16, chaddr: MacAddr) -> Option<Self> {
        let mut buf = alloc::vec![0u8; OPTIONS_OFFSET];
        put_bytes(&mut buf, 0, &[op, 1, 6, 0])?;
        put_be32(&mut buf, 4, xid)?;
        put_be16(&mut buf, 10, flags)?;
        put_bytes(&mut buf, 28, &chaddr.0)?;
        put_bytes(&mut buf, 236, &MAGIC)?;
        Some(Builder { buf })
    }

    fn addr(mut self, off: usize, ip: Ipv4Addr) -> Option<Self> {
        put_ipv4(&mut self.buf, off, ip)?;
        Some(self)
    }

    fn opt(mut self, code: u8, data: &[u8]) -> Option<Self> {
        let len = u8::try_from(data.len()).ok()?;
        self.buf.push(code);
        self.buf.push(len);
        self.buf.extend_from_slice(data);
        Some(self)
    }

    fn opt_ip(self, code: u8, ip: Ipv4Addr) -> Option<Self> {
        self.opt(code, &ip.octets())
    }

    fn finish(mut self) -> Vec<u8> {
        self.buf.push(OPT_END);
        // BOOTP minimum size keeps picky clients happy.
        if self.buf.len() < 300 {
            self.buf.resize(300, 0);
        }
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
    let base = |kind: u8| {
        Builder::new(BOOTREPLY, req.xid, req.flags, req.chaddr)?
            .opt(OPT_MSG_TYPE, &[kind])?
            .opt_ip(OPT_SERVER_ID, cfg.gateway)
    };
    match req.msg_type? {
        DISCOVER => Some((OFFER, lease(base(OFFER)?, cfg, params, client.ip)?.finish())),
        REQUEST => {
            if req.server_id.is_some_and(|s| s != cfg.gateway) {
                return None; // talking to another server
            }
            let wanted = req.requested_ip.unwrap_or(req.ciaddr);
            if wanted == client.ip {
                Some((ACK, lease(base(ACK)?, cfg, params, client.ip)?.finish()))
            } else {
                Some((NAK, base(NAK)?.finish()))
            }
        }
        _ => None,
    }
}

fn lease(b: Builder, cfg: &Config, params: &ServerParams, ip: Ipv4Addr) -> Option<Builder> {
    let mut b = b
        .addr(16, ip)?
        .addr(20, cfg.gateway)?
        .opt(OPT_LEASE_TIME, &CLIENT_LEASE_SECS.to_be_bytes())?
        .opt_ip(OPT_SUBNET_MASK, CLIENT_MASK)?
        .opt_ip(OPT_ROUTER, cfg.gateway)?;
    if !params.dns.is_empty() {
        let bytes: Vec<u8> = params.dns.iter().flat_map(|d| d.octets()).collect();
        b = b.opt(OPT_DNS, &bytes)?;
    }
    Some(b)
}

// ---------------------------------------------------------------- client

const PARAMS: [u8; 3] = [OPT_SUBNET_MASK, OPT_ROUTER, OPT_DNS];

pub fn discover(xid: u32, mac: MacAddr) -> Option<Vec<u8>> {
    Some(
        Builder::new(BOOTREQUEST, xid, FLAG_BROADCAST, mac)?
            .opt(OPT_MSG_TYPE, &[DISCOVER])?
            .opt(OPT_PARAM_REQUEST, &PARAMS)?
            .finish(),
    )
}

pub fn request(xid: u32, mac: MacAddr, offered: Ipv4Addr, server: Ipv4Addr) -> Option<Vec<u8>> {
    Some(
        Builder::new(BOOTREQUEST, xid, FLAG_BROADCAST, mac)?
            .opt(OPT_MSG_TYPE, &[REQUEST])?
            .opt_ip(OPT_REQUESTED_IP, offered)?
            .opt_ip(OPT_SERVER_ID, server)?
            .opt(OPT_PARAM_REQUEST, &PARAMS)?
            .finish(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK_MAC: MacAddr = MacAddr([2, 0x4a, 0x5b, 0x98, 0xd2, 0x60]);

    fn cfg() -> Config {
        Config::parse(
            "firewall fw\ngateway 10.137.0.2\nupstream dhcp\nclient work 10.137.0.3 02:4a:5b:98:d2:60\n",
        )
        .unwrap()
    }

    #[test]
    fn client_messages_parse_back() {
        let d = Message::parse(&discover(0xabcd, WORK_MAC).unwrap()).unwrap();
        assert_eq!(d.op, BOOTREQUEST);
        assert_eq!(d.xid, 0xabcd);
        assert_eq!(d.msg_type, Some(DISCOVER));
        assert_eq!(d.chaddr, WORK_MAC);
        assert!(d.wants_broadcast());
        let r = Message::parse(
            &request(
                1,
                WORK_MAC,
                Ipv4Addr::new(192, 168, 1, 7),
                Ipv4Addr::new(192, 168, 1, 1),
            )
            .unwrap(),
        )
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
        let d = Message::parse(&discover(7, WORK_MAC).unwrap()).unwrap();
        let (kind, payload) = serve(&cfg, &params, &d).unwrap();
        assert_eq!(kind, OFFER);
        let offer = Message::parse(&payload).unwrap();
        assert_eq!(offer.op, BOOTREPLY);
        assert_eq!(offer.xid, 7);
        assert_eq!(offer.yiaddr, Ipv4Addr::new(10, 137, 0, 3));
        assert_eq!(offer.server_id, Some(cfg.gateway));
        assert_eq!(offer.router, Some(cfg.gateway));
        assert_eq!(offer.subnet_mask, Some(CLIENT_MASK));
        assert_eq!(offer.dns, params.dns);
        assert_eq!(offer.lease_secs, Some(CLIENT_LEASE_SECS));
        assert!(payload.len() >= 300);

        let r = Message::parse(&request(7, WORK_MAC, offer.yiaddr, cfg.gateway).unwrap()).unwrap();
        let (kind, _) = serve(&cfg, &params, &r).unwrap();
        assert_eq!(kind, ACK);

        // Asking for the wrong address is refused.
        let bad = Message::parse(
            &request(7, WORK_MAC, Ipv4Addr::new(10, 137, 0, 9), cfg.gateway).unwrap(),
        )
        .unwrap();
        assert_eq!(serve(&cfg, &params, &bad).map(|(k, _)| k), Some(NAK));
        // Requests meant for another server are ignored.
        let other = Message::parse(
            &request(7, WORK_MAC, offer.yiaddr, Ipv4Addr::new(192, 168, 1, 1)).unwrap(),
        )
        .unwrap();
        assert_eq!(serve(&cfg, &params, &other), None);
    }

    #[test]
    fn unknown_mac_is_ignored() {
        let cfg = cfg();
        let d = Message::parse(&discover(1, MacAddr([2, 0, 0, 0, 0, 0x99])).unwrap()).unwrap();
        assert_eq!(serve(&cfg, &ServerParams::default(), &d), None);
    }

    #[test]
    fn malformed_options_do_not_panic() {
        let mut p = discover(1, WORK_MAC).unwrap();
        p.truncate(OPTIONS_OFFSET + 1); // option code without length
        assert!(Message::parse(&p).is_none());
        let mut q = discover(1, WORK_MAC).unwrap();
        q[OPTIONS_OFFSET + 1] = 200; // length overruns the buffer
        assert!(Message::parse(&q).is_none());
    }
}
