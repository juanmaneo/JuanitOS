//! The firewall proper. Frames come in from the upstream NIC (towards
//! `sys-net`) or the downstream NIC (the client qubes' bridge); the engine
//! answers ARP/DHCP itself, filters and NATs everything else, and hands
//! back the frames to transmit. No I/O, no clocks: the caller supplies
//! `now_ms` and sends the [`Output`]s, which keeps this fully testable.

use alloc::vec::Vec;
use core::net::Ipv4Addr;

use jos_fwcfg::{Action, MacAddr, Proto};

use crate::arp::{self, Arp, ARP_REQUEST};
use crate::config::{Client, Config};
use crate::dhcp;
use crate::nat::Nat;
use crate::packet::{
    udp_frame, EthHeader, Ipv4Frame, BROADCAST_MAC, ETHERTYPE_ARP, ETHERTYPE_IPV4, ETH_HDR_LEN,
    ICMP_ECHO_REPLY, ICMP_ECHO_REQUEST, L4,
};

/// A frame the caller must transmit on the given side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Upstream(Vec<u8>),
    Downstream(Vec<u8>),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub forwarded_out: u64,
    pub forwarded_in: u64,
    pub denied: u64,
    pub unknown_client: u64,
    pub spoofed: u64,
    pub no_route: u64,
    pub nat_full: u64,
    pub unsolicited_in: u64,
    pub malformed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpState {
    Init,
    Discovering {
        since: u64,
    },
    Requesting {
        since: u64,
        offered: Ipv4Addr,
        server: Ipv4Addr,
    },
    Bound,
}

const DHCP_RETRY_MS: u64 = 5_000;
const ARP_RETRY_MS: u64 = 1_000;
const NAT_EXPIRE_EVERY_MS: u64 = 10_000;
const BROADCAST_IP: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 255);

/// State of the upstream (WAN-side) interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub mac: MacAddr,
    pub ip: Option<Ipv4Addr>,
    pub router: Option<Ipv4Addr>,
    pub router_mac: Option<MacAddr>,
    pub dns: Vec<Ipv4Addr>,
    pub lease_expires_ms: Option<u64>,
    state: UpState,
    xid: u32,
    last_arp_ms: Option<u64>,
}

impl Upstream {
    pub fn is_bound(&self) -> bool {
        self.ip.is_some() && self.router_mac.is_some()
    }
}

pub struct Engine {
    pub cfg: Config,
    pub nat: Nat,
    pub up: Upstream,
    pub down_mac: MacAddr,
    pub stats: Stats,
    last_expire_ms: u64,
}

impl Engine {
    pub fn new(cfg: Config, up_mac: MacAddr, down_mac: MacAddr) -> Self {
        let xid = u32::from_be_bytes([up_mac.0[2], up_mac.0[3], up_mac.0[4], up_mac.0[5]]);
        let mut up = Upstream {
            mac: up_mac,
            ip: None,
            router: None,
            router_mac: None,
            dns: Vec::new(),
            lease_expires_ms: None,
            state: UpState::Init,
            xid,
            last_arp_ms: None,
        };
        if let Some(ip) = cfg.upstream_static {
            // Stopgap until the grammar carries a router: assume the
            // first host of the /24.
            let [a, b, c, _] = ip.octets();
            up.ip = Some(ip);
            up.router = Some(Ipv4Addr::new(a, b, c, 1));
            up.state = UpState::Bound;
        }
        Engine {
            cfg,
            nat: Nat::new(),
            up,
            down_mac,
            stats: Stats::default(),
            last_expire_ms: 0,
        }
    }

    /// Periodic work: DHCP client timers, router ARP, NAT expiry.
    pub fn tick(&mut self, now_ms: u64, out: &mut Vec<Output>) {
        if now_ms.saturating_sub(self.last_expire_ms) >= NAT_EXPIRE_EVERY_MS {
            self.nat.expire(now_ms);
            self.last_expire_ms = now_ms;
        }
        if self.cfg.upstream_dhcp {
            self.dhcp_client_tick(now_ms, out);
        }
        if let (Some(router), None) = (self.up.router, self.up.router_mac) {
            let due = self
                .up
                .last_arp_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= ARP_RETRY_MS);
            if due {
                if let Some(f) = arp::request(
                    self.up.mac,
                    self.up.ip.unwrap_or(Ipv4Addr::UNSPECIFIED),
                    router,
                ) {
                    out.push(Output::Upstream(f));
                }
                self.up.last_arp_ms = Some(now_ms);
            }
        }
    }

    fn dhcp_client_tick(&mut self, now_ms: u64, out: &mut Vec<Output>) {
        let restart = match self.up.state {
            UpState::Init => true,
            UpState::Discovering { since } | UpState::Requesting { since, .. } => {
                now_ms.saturating_sub(since) >= DHCP_RETRY_MS
            }
            UpState::Bound => self.up.lease_expires_ms.is_some_and(|t| now_ms >= t),
        };
        if !restart {
            return;
        }
        self.up.xid = self.up.xid.wrapping_add(1);
        if let Some(p) = dhcp::discover(self.up.xid, self.up.mac) {
            if let Some(f) = self.upstream_dhcp_frame(&p) {
                out.push(Output::Upstream(f));
            }
        }
        self.up.state = UpState::Discovering { since: now_ms };
    }

    fn upstream_dhcp_frame(&self, payload: &[u8]) -> Option<Vec<u8>> {
        udp_frame(
            self.up.mac,
            BROADCAST_MAC,
            Ipv4Addr::UNSPECIFIED,
            BROADCAST_IP,
            dhcp::CLIENT_PORT,
            dhcp::SERVER_PORT,
            payload,
        )
    }

    // ---------------------------------------------------------- upstream

    /// A frame received on the upstream NIC.
    pub fn handle_upstream(&mut self, frame: &[u8], now_ms: u64, out: &mut Vec<Output>) {
        let Some(eth) = EthHeader::parse(frame) else {
            self.stats.malformed += 1;
            return;
        };
        match eth.ethertype {
            ETHERTYPE_ARP => self.upstream_arp(frame, out),
            ETHERTYPE_IPV4 => self.upstream_ipv4(frame, now_ms, out),
            _ => {}
        }
    }

    fn upstream_arp(&mut self, frame: &[u8], out: &mut Vec<Output>) {
        let Some(a) = Arp::parse_frame(frame) else {
            self.stats.malformed += 1;
            return;
        };
        if self.up.router == Some(a.sender_ip) {
            self.up.router_mac = Some(a.sender_mac);
        }
        if a.op == ARP_REQUEST && self.up.ip == Some(a.target_ip) {
            if let Some(f) = arp::reply(&a, self.up.mac, a.target_ip) {
                out.push(Output::Upstream(f));
            }
        }
    }

    fn upstream_ipv4(&mut self, frame: &[u8], now_ms: u64, out: &mut Vec<Output>) {
        let Some(p) = Ipv4Frame::parse(frame) else {
            self.stats.malformed += 1;
            return;
        };
        if let L4::Udp {
            src_port: dhcp::SERVER_PORT,
            dst_port: dhcp::CLIENT_PORT,
        } = p.l4
        {
            if let Some(payload) = frame.get(p.l4_off + 8..p.end) {
                self.dhcp_client_input(payload, now_ms, out);
            }
            return;
        }
        if self.up.ip.is_none() || Some(p.ip.dst) != self.up.ip {
            return; // not for us (broadcasts, other hosts on the link)
        }
        if let L4::Icmp {
            kind: ICMP_ECHO_REQUEST,
            ..
        } = p.l4
        {
            if let Some(f) = echo_reply(frame, &p, self.up.mac) {
                out.push(Output::Upstream(f));
            }
            return;
        }
        let (Some(remote_port), Some(public_port)) = (p.l4.src_port(), p.l4.dst_port()) else {
            self.stats.unsolicited_in += 1;
            return;
        };
        let Some((client_ip, client_port)) =
            self.nat
                .inbound(now_ms, p.ip.proto, p.ip.src, remote_port, public_port)
        else {
            self.stats.unsolicited_in += 1;
            return;
        };
        let Some(client) = self.cfg.client_by_ip(client_ip) else {
            self.stats.unknown_client += 1;
            return;
        };
        let mut f = frame.get(..p.end).unwrap_or(frame).to_vec();
        let rewritten = (|| {
            p.set_dst_ip(&mut f, client_ip)?;
            p.set_dst_port(&mut f, client_port)?;
            if !p.decrement_ttl(&mut f)? {
                return None;
            }
            p.fix_checksums(&mut f)?;
            EthHeader {
                dst: client.mac,
                src: self.down_mac,
                ethertype: ETHERTYPE_IPV4,
            }
            .write(&mut f)
        })();
        if rewritten.is_some() {
            self.stats.forwarded_in += 1;
            out.push(Output::Downstream(f));
        } else {
            self.stats.malformed += 1;
        }
    }

    fn dhcp_client_input(&mut self, payload: &[u8], now_ms: u64, out: &mut Vec<Output>) {
        let Some(m) = dhcp::Message::parse(payload) else {
            return;
        };
        if m.op != dhcp::BOOTREPLY || m.xid != self.up.xid || m.chaddr != self.up.mac {
            return;
        }
        match (self.up.state, m.msg_type) {
            (UpState::Discovering { .. }, Some(dhcp::OFFER)) => {
                let Some(server) = m.server_id else { return };
                self.up.state = UpState::Requesting {
                    since: now_ms,
                    offered: m.yiaddr,
                    server,
                };
                if let Some(p) = dhcp::request(self.up.xid, self.up.mac, m.yiaddr, server) {
                    if let Some(f) = self.upstream_dhcp_frame(&p) {
                        out.push(Output::Upstream(f));
                    }
                }
            }
            (UpState::Requesting { .. }, Some(dhcp::ACK)) => {
                self.up.ip = Some(m.yiaddr);
                self.up.router = m.router;
                self.up.router_mac = None;
                self.up.last_arp_ms = None;
                self.up.dns = m.dns;
                self.up.lease_expires_ms = m.lease_secs.map(|s| {
                    // Renew at half the lease, as DHCP clients do.
                    now_ms.saturating_add(u64::from(s).saturating_mul(500))
                });
                self.up.state = UpState::Bound;
            }
            (UpState::Requesting { .. }, Some(dhcp::NAK)) => self.up.state = UpState::Init,
            _ => {}
        }
    }

    // -------------------------------------------------------- downstream

    /// A frame received on the downstream NIC (from a client qube).
    pub fn handle_downstream(&mut self, frame: &[u8], now_ms: u64, out: &mut Vec<Output>) {
        let Some(eth) = EthHeader::parse(frame) else {
            self.stats.malformed += 1;
            return;
        };
        match eth.ethertype {
            ETHERTYPE_ARP => self.downstream_arp(frame, out),
            ETHERTYPE_IPV4 => self.downstream_ipv4(frame, now_ms, out),
            _ => {}
        }
    }

    fn downstream_arp(&mut self, frame: &[u8], out: &mut Vec<Output>) {
        let Some(a) = Arp::parse_frame(frame) else {
            self.stats.malformed += 1;
            return;
        };
        if a.op == ARP_REQUEST && a.target_ip == self.cfg.gateway {
            if let Some(f) = arp::reply(&a, self.down_mac, self.cfg.gateway) {
                out.push(Output::Downstream(f));
            }
        }
    }

    fn downstream_ipv4(&mut self, frame: &[u8], now_ms: u64, out: &mut Vec<Output>) {
        let Some(p) = Ipv4Frame::parse(frame) else {
            self.stats.malformed += 1;
            return;
        };
        let Some(client) = self.cfg.client_by_mac(p.eth.src) else {
            self.stats.unknown_client += 1;
            return;
        };
        if let L4::Udp {
            src_port: dhcp::CLIENT_PORT,
            dst_port: dhcp::SERVER_PORT,
        } = p.l4
        {
            if let Some(payload) = frame.get(p.l4_off + 8..p.end) {
                let client_mac = client.mac;
                self.serve_dhcp(payload, client_mac, out);
            }
            return;
        }
        if p.ip.src != client.ip {
            self.stats.spoofed += 1;
            return;
        }
        if p.ip.dst == self.cfg.gateway {
            if let L4::Icmp {
                kind: ICMP_ECHO_REQUEST,
                ..
            } = p.l4
            {
                if let Some(f) = echo_reply(frame, &p, self.down_mac) {
                    out.push(Output::Downstream(f));
                }
            }
            return;
        }
        let (Some(proto), Some(src_port), Some(dst_port)) = (
            Proto::from_ip_protocol(p.ip.proto),
            p.l4.src_port(),
            p.l4.dst_port(),
        ) else {
            self.stats.denied += 1; // protocols we cannot NAT are never forwarded
            return;
        };
        if let L4::Icmp { kind, .. } = p.l4 {
            if kind != ICMP_ECHO_REQUEST {
                self.stats.denied += 1;
                return;
            }
        }
        let rule_port = if proto == Proto::Icmp { 0 } else { dst_port };
        if client.decide(proto, p.ip.dst, rule_port) == Action::Deny {
            self.stats.denied += 1;
            return;
        }
        let (Some(public_ip), Some(router_mac)) = (self.up.ip, self.up.router_mac) else {
            self.stats.no_route += 1;
            return;
        };
        let Some(public_port) = self
            .nat
            .outbound(now_ms, p.ip.proto, client.ip, src_port, p.ip.dst, dst_port)
        else {
            self.stats.nat_full += 1;
            return;
        };
        let mut f = frame.get(..p.end).unwrap_or(frame).to_vec();
        let rewritten = (|| {
            p.set_src_ip(&mut f, public_ip)?;
            p.set_src_port(&mut f, public_port)?;
            if !p.decrement_ttl(&mut f)? {
                return None;
            }
            p.fix_checksums(&mut f)?;
            EthHeader {
                dst: router_mac,
                src: self.up.mac,
                ethertype: ETHERTYPE_IPV4,
            }
            .write(&mut f)
        })();
        if rewritten.is_some() {
            self.stats.forwarded_out += 1;
            out.push(Output::Upstream(f));
        } else {
            self.stats.malformed += 1;
        }
    }

    fn serve_dhcp(&mut self, payload: &[u8], client_mac: MacAddr, out: &mut Vec<Output>) {
        let Some(req) = dhcp::Message::parse(payload) else {
            self.stats.malformed += 1;
            return;
        };
        let params = dhcp::ServerParams {
            dns: self.up.dns.clone(),
        };
        let Some((_, reply)) = dhcp::serve(&self.cfg, &params, &req) else {
            return;
        };
        let yiaddr = dhcp::Message::parse(&reply).map(|m| m.yiaddr);
        let (dst_mac, dst_ip) = match yiaddr {
            Some(ip) if !req.wants_broadcast() && !ip.is_unspecified() => (client_mac, ip),
            _ => (BROADCAST_MAC, BROADCAST_IP),
        };
        if let Some(f) = udp_frame(
            self.down_mac,
            dst_mac,
            self.cfg.gateway,
            dst_ip,
            dhcp::SERVER_PORT,
            dhcp::CLIENT_PORT,
            &reply,
        ) {
            out.push(Output::Downstream(f));
        }
    }

    pub fn client_for_mac(&self, mac: MacAddr) -> Option<&Client> {
        self.cfg.client_by_mac(mac)
    }
}

/// Turn an ICMP echo request frame into the matching reply from `our_mac`.
fn echo_reply(frame: &[u8], p: &Ipv4Frame, our_mac: MacAddr) -> Option<Vec<u8>> {
    let mut f = frame.get(..p.end)?.to_vec();
    EthHeader {
        dst: p.eth.src,
        src: our_mac,
        ethertype: ETHERTYPE_IPV4,
    }
    .write(&mut f)?;
    p.set_src_ip(&mut f, p.ip.dst)?;
    p.set_dst_ip(&mut f, p.ip.src)?;
    *f.get_mut(ETH_HDR_LEN + 8)? = 64;
    *f.get_mut(p.l4_off)? = ICMP_ECHO_REPLY;
    p.fix_checksums(&mut f)?;
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arp::ARP_REPLY;
    use crate::packet::{checksum, l4_checksum, IPPROTO_ICMP, IPPROTO_UDP};
    use jos_fwcfg::MacAddr;

    const UP_MAC: MacAddr = MacAddr([2, 0x4a, 0, 0, 0, 0xf0]);
    const DOWN_MAC: MacAddr = MacAddr([2, 0x4a, 0, 0, 0, 0xf1]);
    const ROUTER_MAC: MacAddr = MacAddr([0x52, 0x54, 0, 0, 0, 1]);
    const WORK_MAC: MacAddr = MacAddr([2, 0x4a, 0x5b, 0x98, 0xd2, 0x60]);
    const WORK_IP: Ipv4Addr = Ipv4Addr::new(10, 137, 0, 3);
    const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 137, 0, 2);
    const WAN_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 40);
    const WAN_ROUTER: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 1);
    const REMOTE: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

    fn engine() -> Engine {
        let cfg = Config::parse(
            "firewall fw\ngateway 10.137.0.2\nupstream dhcp\n\
             client work 10.137.0.3 02:4a:5b:98:d2:60\n\
             rule work allow udp any 53\nrule work allow icmp\npolicy work deny\n",
        )
        .unwrap();
        Engine::new(cfg, UP_MAC, DOWN_MAC)
    }

    /// The upstream "sys-net" DHCP server, modelled with our own server code.
    fn wan_server() -> Config {
        Config::parse("gateway 192.168.1.1\nclient fw 192.168.1.40 02:4a:00:00:00:f0\n").unwrap()
    }

    fn only_upstream(out: &[Output]) -> Vec<&[u8]> {
        out.iter()
            .filter_map(|o| match o {
                Output::Upstream(f) => Some(f.as_slice()),
                Output::Downstream(_) => None,
            })
            .collect()
    }

    fn dhcp_payload(frame: &[u8]) -> dhcp::Message {
        let p = Ipv4Frame::parse(frame).unwrap();
        dhcp::Message::parse(&frame[p.l4_off + 8..p.end]).unwrap()
    }

    /// Run the upstream DHCP + ARP handshake; returns the engine bound.
    fn bound_engine() -> Engine {
        let mut e = engine();
        let wan = wan_server();
        let params = dhcp::ServerParams {
            dns: alloc::vec![Ipv4Addr::new(9, 9, 9, 9)],
        };
        let mut out = Vec::new();
        e.tick(0, &mut out);
        let up = only_upstream(&out);
        assert_eq!(up.len(), 1);
        let discover = dhcp_payload(up[0]);
        assert_eq!(discover.msg_type, Some(dhcp::DISCOVER));

        let (_, offer) = dhcp::serve(&wan, &params, &discover).unwrap();
        let offer_frame = udp_frame(
            ROUTER_MAC,
            BROADCAST_MAC,
            WAN_ROUTER,
            BROADCAST_IP,
            67,
            68,
            &offer,
        )
        .unwrap();
        out.clear();
        e.handle_upstream(&offer_frame, 100, &mut out);
        let up = only_upstream(&out);
        assert_eq!(up.len(), 1);
        let request = dhcp_payload(up[0]);
        assert_eq!(request.msg_type, Some(dhcp::REQUEST));
        assert_eq!(request.requested_ip, Some(WAN_IP));

        let (kind, ack) = dhcp::serve(&wan, &params, &request).unwrap();
        assert_eq!(kind, dhcp::ACK);
        let ack_frame = udp_frame(ROUTER_MAC, UP_MAC, WAN_ROUTER, WAN_IP, 67, 68, &ack).unwrap();
        e.handle_upstream(&ack_frame, 200, &mut out);
        assert_eq!(e.up.ip, Some(WAN_IP));
        assert_eq!(e.up.router, Some(WAN_ROUTER));
        assert_eq!(e.up.dns, params.dns);
        assert!(!e.up.is_bound()); // router MAC still unknown

        out.clear();
        e.tick(300, &mut out);
        let up = only_upstream(&out);
        assert_eq!(up.len(), 1);
        let who_has = Arp::parse_frame(up[0]).unwrap();
        assert_eq!(who_has.op, ARP_REQUEST);
        assert_eq!(who_has.target_ip, WAN_ROUTER);
        assert_eq!(who_has.sender_ip, WAN_IP);
        let reply = arp::reply(&who_has, ROUTER_MAC, WAN_ROUTER).unwrap();
        out.clear();
        e.handle_upstream(&reply, 400, &mut out);
        assert!(e.up.is_bound());
        assert_eq!(e.up.router_mac, Some(ROUTER_MAC));
        e
    }

    #[test]
    fn upstream_dhcp_and_arp_handshake() {
        let e = bound_engine();
        assert_eq!(e.stats, Stats::default());
    }

    #[test]
    fn dhcp_discover_is_retried() {
        let mut e = engine();
        let mut out = Vec::new();
        e.tick(0, &mut out);
        e.tick(1000, &mut out);
        assert_eq!(only_upstream(&out).len(), 1);
        e.tick(DHCP_RETRY_MS, &mut out);
        assert_eq!(only_upstream(&out).len(), 2);
    }

    #[test]
    fn nat_roundtrip_udp() {
        let mut e = bound_engine();
        let mut out = Vec::new();
        let query = udp_frame(WORK_MAC, DOWN_MAC, WORK_IP, REMOTE, 5000, 53, b"q").unwrap();
        e.handle_downstream(&query, 1000, &mut out);
        assert_eq!(e.stats.forwarded_out, 1);
        let up = only_upstream(&out);
        assert_eq!(up.len(), 1);
        let fwd = Ipv4Frame::parse(up[0]).unwrap();
        assert_eq!(fwd.eth.src, UP_MAC);
        assert_eq!(fwd.eth.dst, ROUTER_MAC);
        assert_eq!(fwd.ip.src, WAN_IP);
        assert_eq!(fwd.ip.dst, REMOTE);
        assert_eq!(fwd.ip.ttl, 63);
        let public_port = fwd.l4.src_port().unwrap();
        assert!(public_port >= crate::nat::PORT_MIN);
        assert_eq!(fwd.l4.dst_port(), Some(53));
        assert_eq!(checksum(&up[0][14..34]), 0);
        assert_eq!(
            l4_checksum(
                fwd.ip.src,
                fwd.ip.dst,
                IPPROTO_UDP,
                &up[0][fwd.l4_off..fwd.end]
            ),
            0
        );

        // The answer comes back and is mapped to the client.
        let answer = udp_frame(ROUTER_MAC, UP_MAC, REMOTE, WAN_IP, 53, public_port, b"a").unwrap();
        out.clear();
        e.handle_upstream(&answer, 1100, &mut out);
        assert_eq!(e.stats.forwarded_in, 1);
        let Some(Output::Downstream(f)) = out.first() else {
            panic!("expected downstream frame, got {out:?}");
        };
        let back = Ipv4Frame::parse(f).unwrap();
        assert_eq!(back.eth.dst, WORK_MAC);
        assert_eq!(back.eth.src, DOWN_MAC);
        assert_eq!(back.ip.src, REMOTE);
        assert_eq!(back.ip.dst, WORK_IP);
        assert_eq!(back.l4.dst_port(), Some(5000));
        assert_eq!(checksum(&f[14..34]), 0);

        // Unsolicited inbound traffic is dropped.
        let stray = udp_frame(
            ROUTER_MAC,
            UP_MAC,
            Ipv4Addr::new(2, 2, 2, 2),
            WAN_IP,
            53,
            public_port,
            b"x",
        )
        .unwrap();
        out.clear();
        e.handle_upstream(&stray, 1200, &mut out);
        assert!(out.is_empty());
        assert_eq!(e.stats.unsolicited_in, 1);
    }

    #[test]
    fn rules_spoofing_and_unknown_clients() {
        let mut e = bound_engine();
        let mut out = Vec::new();
        // Policy deny: UDP 123 is not in the allow list.
        let ntp = udp_frame(WORK_MAC, DOWN_MAC, WORK_IP, REMOTE, 5000, 123, b"").unwrap();
        e.handle_downstream(&ntp, 1, &mut out);
        assert_eq!(e.stats.denied, 1);
        // Wrong source IP for that MAC.
        let spoof = udp_frame(
            WORK_MAC,
            DOWN_MAC,
            Ipv4Addr::new(10, 137, 0, 99),
            REMOTE,
            5000,
            53,
            b"",
        )
        .unwrap();
        e.handle_downstream(&spoof, 1, &mut out);
        assert_eq!(e.stats.spoofed, 1);
        // Unknown MAC.
        let stranger = udp_frame(
            MacAddr([2, 0, 0, 0, 0, 0x77]),
            DOWN_MAC,
            WORK_IP,
            REMOTE,
            5000,
            53,
            b"",
        )
        .unwrap();
        e.handle_downstream(&stranger, 1, &mut out);
        assert_eq!(e.stats.unknown_client, 1);
        assert!(out.is_empty());
    }

    #[test]
    fn no_route_before_upstream_is_bound() {
        let mut e = engine();
        let mut out = Vec::new();
        let query = udp_frame(WORK_MAC, DOWN_MAC, WORK_IP, REMOTE, 5000, 53, b"q").unwrap();
        e.handle_downstream(&query, 1, &mut out);
        assert_eq!(e.stats.no_route, 1);
        assert!(out.is_empty());
    }

    #[test]
    fn downstream_arp_and_dhcp_are_answered() {
        let mut e = bound_engine();
        let mut out = Vec::new();
        let who = arp::request(WORK_MAC, WORK_IP, GATEWAY).unwrap();
        e.handle_downstream(&who, 1, &mut out);
        let Some(Output::Downstream(f)) = out.first() else {
            panic!("expected ARP reply");
        };
        let a = Arp::parse_frame(f).unwrap();
        assert_eq!(a.op, ARP_REPLY);
        assert_eq!(a.sender_mac, DOWN_MAC);
        assert_eq!(a.sender_ip, GATEWAY);

        out.clear();
        let discover = dhcp::discover(42, WORK_MAC).unwrap();
        let f = udp_frame(
            WORK_MAC,
            BROADCAST_MAC,
            Ipv4Addr::UNSPECIFIED,
            BROADCAST_IP,
            68,
            67,
            &discover,
        )
        .unwrap();
        e.handle_downstream(&f, 2, &mut out);
        let Some(Output::Downstream(f)) = out.first() else {
            panic!("expected DHCP offer");
        };
        let p = Ipv4Frame::parse(f).unwrap();
        assert_eq!(p.eth.dst, BROADCAST_MAC); // client set the broadcast flag
        let offer = dhcp_payload(f);
        assert_eq!(offer.msg_type, Some(dhcp::OFFER));
        assert_eq!(offer.yiaddr, WORK_IP);
        assert_eq!(offer.router, Some(GATEWAY));
        assert_eq!(offer.dns, alloc::vec![Ipv4Addr::new(9, 9, 9, 9)]);
    }

    #[test]
    fn ping_gateway_and_ping_through() {
        let mut e = bound_engine();
        let mut out = Vec::new();
        let echo = icmp_echo(WORK_MAC, DOWN_MAC, WORK_IP, GATEWAY, 0x1234);
        e.handle_downstream(&echo, 1, &mut out);
        let Some(Output::Downstream(f)) = out.first() else {
            panic!("expected echo reply");
        };
        let p = Ipv4Frame::parse(f).unwrap();
        assert_eq!(p.ip.src, GATEWAY);
        assert_eq!(p.ip.dst, WORK_IP);
        assert!(matches!(
            p.l4,
            L4::Icmp {
                kind: ICMP_ECHO_REPLY,
                id: 0x1234,
                ..
            }
        ));
        assert_eq!(checksum(&f[p.l4_off..p.end]), 0);

        out.clear();
        let echo = icmp_echo(WORK_MAC, DOWN_MAC, WORK_IP, REMOTE, 0x1234);
        e.handle_downstream(&echo, 2, &mut out);
        let up = only_upstream(&out);
        assert_eq!(up.len(), 1);
        let p = Ipv4Frame::parse(up[0]).unwrap();
        let L4::Icmp { id, .. } = p.l4 else { panic!() };
        assert_eq!(p.ip.src, WAN_IP);
        assert!(id >= crate::nat::PORT_MIN);
        assert_eq!(checksum(&up[0][p.l4_off..p.end]), 0);

        // The reply comes back with the *translated* identifier and must
        // reach the client with its original one (found under QEMU: the
        // flow was keyed on the untranslated id and every reply was
        // dropped as unsolicited).
        let mut reply = icmp_echo(ROUTER_MAC, UP_MAC, REMOTE, WAN_IP, id);
        reply[34] = ICMP_ECHO_REPLY;
        reply[36..38].copy_from_slice(&[0, 0]);
        let c = checksum(&reply[34..]);
        reply[36..38].copy_from_slice(&c.to_be_bytes());
        out.clear();
        e.handle_upstream(&reply, 3, &mut out);
        let Some(Output::Downstream(f)) = out.first() else {
            panic!("expected the echo reply to be forwarded downstream, got {out:?}");
        };
        let p = Ipv4Frame::parse(f).unwrap();
        assert_eq!(p.eth.dst, WORK_MAC);
        assert_eq!(p.ip.src, REMOTE);
        assert_eq!(p.ip.dst, WORK_IP);
        assert!(matches!(
            p.l4,
            L4::Icmp {
                kind: ICMP_ECHO_REPLY,
                id: 0x1234,
                ..
            }
        ));
        assert_eq!(checksum(&f[p.l4_off..p.end]), 0);
        assert_eq!(e.stats.forwarded_in, 1);
        assert_eq!(e.stats.unsolicited_in, 0);
    }

    fn icmp_echo(
        src_mac: MacAddr,
        dst_mac: MacAddr,
        src: Ipv4Addr,
        dst: Ipv4Addr,
        id: u16,
    ) -> Vec<u8> {
        let mut f = alloc::vec![0u8; 14 + 20 + 8 + 4];
        EthHeader {
            dst: dst_mac,
            src: src_mac,
            ethertype: ETHERTYPE_IPV4,
        }
        .write(&mut f)
        .unwrap();
        f[14] = 0x45;
        f[16..18].copy_from_slice(&32u16.to_be_bytes());
        f[22] = 64;
        f[23] = IPPROTO_ICMP;
        f[26..30].copy_from_slice(&src.octets());
        f[30..34].copy_from_slice(&dst.octets());
        let c = checksum(&f[14..34]);
        f[24..26].copy_from_slice(&c.to_be_bytes());
        f[34] = ICMP_ECHO_REQUEST;
        f[38..40].copy_from_slice(&id.to_be_bytes());
        let c = checksum(&f[34..]);
        f[36..38].copy_from_slice(&c.to_be_bytes());
        f
    }
}
