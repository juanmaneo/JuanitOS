//! The firewall proper. Frames come in from the upstream NIC (towards
//! `sys-net`) or the downstream NIC (the client qubes' bridge); the engine
//! answers ARP/DHCP itself, filters and NATs everything else, and hands
//! back the frames to transmit. No I/O, no clocks: the caller supplies
//! `now_ms` and sends the [`Output`]s, which keeps this fully testable.

use alloc::vec::Vec;
use core::net::Ipv4Addr;

use jos_fwcfg::{Action, MacAddr, Proto};

use crate::arp::{self, Arp, ARP_REQUEST};
use crate::config::Config;
use crate::dhcp;
use crate::nat::Nat;
use crate::packet::{
    put_bytes, udp_frame, EthHeader, Ipv4Frame, BROADCAST_MAC, ETHERTYPE_ARP, ETHERTYPE_IPV4,
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
                let ip = self.up.ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
                out.push(Output::Upstream(arp::request(self.up.mac, ip, router)));
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
        self.send_dhcp_upstream(&dhcp::discover(self.up.xid, self.up.mac), out);
        self.up.state = UpState::Discovering { since: now_ms };
    }

    fn send_dhcp_upstream(&self, payload: &[u8], out: &mut Vec<Output>) {
        let frame = udp_frame(
            self.up.mac,
            BROADCAST_MAC,
            Ipv4Addr::UNSPECIFIED,
            BROADCAST_IP,
            dhcp::CLIENT_PORT,
            dhcp::SERVER_PORT,
            payload,
        );
        out.extend(frame.map(Output::Upstream));
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
            out.push(Output::Upstream(arp::reply(&a, self.up.mac, a.target_ip)));
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
            let payload = frame.get(p.l4_off + 8..p.end).unwrap_or_default();
            self.dhcp_client_input(payload, now_ms, out);
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
            out.push(Output::Upstream(echo_reply(frame, &p, self.up.mac)));
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
        let eth = EthHeader {
            dst: client.mac,
            src: self.down_mac,
            ethertype: ETHERTYPE_IPV4,
        };
        let Some(f) = forward(frame, &p, eth, &|f| {
            p.set_dst_ip(f, client_ip);
            p.set_dst_port(f, client_port);
        }) else {
            self.stats.malformed += 1;
            return;
        };
        self.stats.forwarded_in += 1;
        out.push(Output::Downstream(f));
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
                let request = dhcp::request(self.up.xid, self.up.mac, m.yiaddr, server);
                self.send_dhcp_upstream(&request, out);
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
            out.push(Output::Downstream(arp::reply(
                &a,
                self.down_mac,
                self.cfg.gateway,
            )));
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
            let payload = frame.get(p.l4_off + 8..p.end).unwrap_or_default();
            let (mac, ip) = (client.mac, client.ip);
            self.serve_dhcp(payload, mac, ip, out);
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
                out.push(Output::Downstream(echo_reply(frame, &p, self.down_mac)));
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
        let eth = EthHeader {
            dst: router_mac,
            src: self.up.mac,
            ethertype: ETHERTYPE_IPV4,
        };
        let Some(f) = forward(frame, &p, eth, &|f| {
            p.set_src_ip(f, public_ip);
            p.set_src_port(f, public_port);
        }) else {
            self.stats.malformed += 1;
            return;
        };
        self.stats.forwarded_out += 1;
        out.push(Output::Upstream(f));
    }

    /// Answer the DHCP request in `payload` from the client at
    /// `(client_mac, client_ip)`: unicast when the client can take it,
    /// broadcast when it asked to or is being refused.
    fn serve_dhcp(
        &mut self,
        payload: &[u8],
        client_mac: MacAddr,
        client_ip: Ipv4Addr,
        out: &mut Vec<Output>,
    ) {
        let Some(req) = dhcp::Message::parse(payload) else {
            self.stats.malformed += 1;
            return;
        };
        let params = dhcp::ServerParams {
            dns: self.up.dns.clone(),
        };
        let Some((kind, reply)) = dhcp::serve(&self.cfg, &params, &req) else {
            return;
        };
        let (dst_mac, dst_ip) = if kind != dhcp::NAK && !req.wants_broadcast() {
            (client_mac, client_ip)
        } else {
            (BROADCAST_MAC, BROADCAST_IP)
        };
        let frame = udp_frame(
            self.down_mac,
            dst_mac,
            self.cfg.gateway,
            dst_ip,
            dhcp::SERVER_PORT,
            dhcp::CLIENT_PORT,
            &reply,
        );
        out.extend(frame.map(Output::Downstream));
    }
}

/// `frame` behind `eth` with its TTL decremented, `nat` applied and the
/// checksums fixed; `None` when the TTL runs out.
fn forward(
    frame: &[u8],
    p: &Ipv4Frame,
    eth: EthHeader,
    nat: &dyn Fn(&mut [u8]),
) -> Option<Vec<u8>> {
    if p.ip.ttl <= 1 {
        return None;
    }
    let mut f = p.rebuild(frame, eth);
    p.set_ttl(&mut f, p.ip.ttl - 1);
    nat(&mut f);
    p.fix_checksums(&mut f);
    Some(f)
}

/// Turn an ICMP echo request frame into the matching reply from `our_mac`.
fn echo_reply(frame: &[u8], p: &Ipv4Frame, our_mac: MacAddr) -> Vec<u8> {
    let eth = EthHeader {
        dst: p.eth.src,
        src: our_mac,
        ethertype: ETHERTYPE_IPV4,
    };
    let mut f = p.rebuild(frame, eth);
    p.set_src_ip(&mut f, p.ip.dst);
    p.set_dst_ip(&mut f, p.ip.src);
    p.set_ttl(&mut f, 64);
    put_bytes(&mut f, p.l4_off, &[ICMP_ECHO_REPLY]);
    p.fix_checksums(&mut f);
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arp::ARP_REPLY;
    use crate::nat::PORT_MIN;
    use crate::packet::{checksum, ipv4_frame, l4_checksum, IPPROTO_ICMP, IPPROTO_UDP};
    use alloc::vec;

    const UP_MAC: MacAddr = MacAddr([2, 0x4a, 0, 0, 0, 0xf0]);
    const DOWN_MAC: MacAddr = MacAddr([2, 0x4a, 0, 0, 0, 0xf1]);
    const ROUTER_MAC: MacAddr = MacAddr([0x52, 0x54, 0, 0, 0, 1]);
    const OTHER_MAC: MacAddr = MacAddr([0x52, 0x54, 0, 0, 0, 2]);
    const WORK_MAC: MacAddr = MacAddr([2, 0x4a, 0x5b, 0x98, 0xd2, 0x60]);
    const WORK_IP: Ipv4Addr = Ipv4Addr::new(10, 137, 0, 3);
    const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 137, 0, 2);
    const WAN_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 40);
    const WAN_ROUTER: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 1);
    const WAN_OTHER: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 41);
    const REMOTE: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
    const DNS: Ipv4Addr = Ipv4Addr::new(9, 9, 9, 9);
    const CLIENTS: &str = "client work 10.137.0.3 02:4a:5b:98:d2:60\n\
         rule work allow udp any 53\nrule work allow icmp\npolicy work deny\n";

    /// Frames the engine sent: `(upstream, downstream)`.
    type Sent = (Vec<Vec<u8>>, Vec<Vec<u8>>);
    const NOTHING: Sent = (Vec::new(), Vec::new());

    fn sent(out: Vec<Output>) -> Sent {
        let mut s = NOTHING;
        for o in out {
            match o {
                Output::Upstream(f) => s.0.push(f),
                Output::Downstream(f) => s.1.push(f),
            }
        }
        s
    }

    fn tick(e: &mut Engine, now: u64) -> Sent {
        let mut out = Vec::new();
        e.tick(now, &mut out);
        sent(out)
    }

    fn from_up(e: &mut Engine, frame: &[u8], now: u64) -> Sent {
        let mut out = Vec::new();
        e.handle_upstream(frame, now, &mut out);
        sent(out)
    }

    fn from_down(e: &mut Engine, frame: &[u8], now: u64) -> Sent {
        let mut out = Vec::new();
        e.handle_downstream(frame, now, &mut out);
        sent(out)
    }

    fn engine_with(upstream: &str) -> Engine {
        let text =
            alloc::format!("firewall fw\ngateway 10.137.0.2\nupstream {upstream}\n{CLIENTS}");
        Engine::new(Config::parse(&text).unwrap(), UP_MAC, DOWN_MAC)
    }

    fn engine() -> Engine {
        engine_with("dhcp")
    }

    /// What the upstream "sys-net" DHCP server (our own server code)
    /// answers to `req`.
    fn wan_reply(req: &dhcp::Message) -> Vec<u8> {
        let wan = Config::parse("gateway 192.168.1.1\nclient fw 192.168.1.40 02:4a:00:00:00:f0\n")
            .unwrap();
        let params = dhcp::ServerParams { dns: vec![DNS] };
        dhcp::serve(&wan, &params, req).unwrap().1
    }

    fn wan_dhcp(payload: &[u8]) -> Vec<u8> {
        udp_frame(
            ROUTER_MAC,
            BROADCAST_MAC,
            WAN_ROUTER,
            BROADCAST_IP,
            67,
            68,
            payload,
        )
        .unwrap()
    }

    fn dhcp_from_work(payload: &[u8]) -> Vec<u8> {
        let any = Ipv4Addr::UNSPECIFIED;
        udp_frame(WORK_MAC, BROADCAST_MAC, any, BROADCAST_IP, 68, 67, payload).unwrap()
    }

    fn dhcp_payload(frame: &[u8]) -> dhcp::Message {
        let p = Ipv4Frame::parse(frame).unwrap();
        dhcp::Message::parse(&frame[p.l4_off + 8..p.end]).unwrap()
    }

    /// UDP from the client qube `work` to `dst:dst_port`.
    fn from_work(dst: Ipv4Addr, dst_port: u16) -> Vec<u8> {
        udp_frame(WORK_MAC, DOWN_MAC, WORK_IP, dst, 5000, dst_port, b"q").unwrap()
    }

    /// UDP from `src:53` to our upstream address and `public_port`.
    fn to_wan(src: Ipv4Addr, public_port: u16) -> Vec<u8> {
        udp_frame(ROUTER_MAC, UP_MAC, src, WAN_IP, 53, public_port, b"a").unwrap()
    }

    fn icmp(kind: u8, macs: (MacAddr, MacAddr), src: Ipv4Addr, dst: Ipv4Addr, id: u16) -> Vec<u8> {
        let [i0, i1] = id.to_be_bytes();
        let mut seg = [kind, 0, 0, 0, i0, i1, 0, 1, 0, 0, 0, 0];
        let c = checksum(&seg);
        seg[2..4].copy_from_slice(&c.to_be_bytes());
        ipv4_frame(macs.0, macs.1, src, dst, IPPROTO_ICMP, &seg).unwrap()
    }

    fn with_ttl(mut frame: Vec<u8>, ttl: u8) -> Vec<u8> {
        frame[14 + 8] = ttl;
        frame
    }

    /// An engine that sent DHCPDISCOVER at 0 and answered the offer at
    /// 100; returns it with its DHCPREQUEST.
    fn requesting() -> (Engine, dhcp::Message) {
        let mut e = engine();
        let (up, down) = tick(&mut e, 0);
        assert_eq!((up.len(), down.len()), (1, 0));
        let discover = dhcp_payload(&up[0]);
        assert_eq!(discover.msg_type, Some(dhcp::DISCOVER));
        let (up, _) = from_up(&mut e, &wan_dhcp(&wan_reply(&discover)), 100);
        assert_eq!(up.len(), 1);
        let request = dhcp_payload(&up[0]);
        assert_eq!(request.msg_type, Some(dhcp::REQUEST));
        assert_eq!(request.requested_ip, Some(WAN_IP));
        (e, request)
    }

    /// Run the upstream DHCP + ARP handshake; returns the engine bound.
    fn bound_engine() -> Engine {
        let (mut e, request) = requesting();
        assert_eq!(
            from_up(&mut e, &wan_dhcp(&wan_reply(&request)), 200),
            NOTHING
        );
        assert_eq!(e.up.ip, Some(WAN_IP));
        assert_eq!(e.up.router, Some(WAN_ROUTER));
        assert_eq!(e.up.dns, [DNS]);
        assert!(!e.up.is_bound()); // router MAC still unknown

        let (up, _) = tick(&mut e, 300);
        assert_eq!(up.len(), 1);
        let who_has = Arp::parse_frame(&up[0]).unwrap();
        assert_eq!(who_has.op, ARP_REQUEST);
        assert_eq!(who_has.target_ip, WAN_ROUTER);
        assert_eq!(who_has.sender_ip, WAN_IP);
        let reply = arp::reply(&who_has, ROUTER_MAC, WAN_ROUTER);
        assert_eq!(from_up(&mut e, &reply, 400), NOTHING);
        assert!(e.up.is_bound());
        assert_eq!(e.up.router_mac, Some(ROUTER_MAC));
        e
    }

    /// The public port `work`'s DNS query to `REMOTE` went out on.
    fn open_dns_flow(e: &mut Engine, now: u64) -> u16 {
        let (up, _) = from_down(e, &from_work(REMOTE, 53), now);
        Ipv4Frame::parse(&up[0]).unwrap().l4.src_port().unwrap()
    }

    #[test]
    fn upstream_dhcp_and_arp_handshake() {
        let e = bound_engine();
        assert_eq!(e.stats, Stats::default());
        assert_eq!(e.up.state, UpState::Bound);
    }

    #[test]
    fn dhcp_client_retries_and_renews() {
        let mut e = engine();
        assert_eq!(tick(&mut e, 0).0.len(), 1);
        assert_eq!(tick(&mut e, DHCP_RETRY_MS - 1), NOTHING);
        assert_eq!(tick(&mut e, DHCP_RETRY_MS).0.len(), 1);

        // An unanswered request starts over as well.
        let (mut e, _) = requesting();
        assert_eq!(tick(&mut e, 100 + DHCP_RETRY_MS - 1), NOTHING);
        let (up, _) = tick(&mut e, 100 + DHCP_RETRY_MS);
        assert_eq!(dhcp_payload(&up[0]).msg_type, Some(dhcp::DISCOVER));

        // Bound: rediscover at half the lease.
        let mut e = bound_engine();
        let renew = e.up.lease_expires_ms.unwrap();
        assert_eq!(renew, 200 + u64::from(dhcp::CLIENT_LEASE_SECS) * 500);
        assert_eq!(tick(&mut e, renew - 1), NOTHING);
        let (up, _) = tick(&mut e, renew);
        assert_eq!(dhcp_payload(&up[0]).msg_type, Some(dhcp::DISCOVER));
    }

    #[test]
    fn a_nak_restarts_discovery() {
        let (mut e, request) = requesting();
        let refused = dhcp::Message {
            requested_ip: Some(WAN_OTHER),
            ..request
        };
        assert_eq!(
            from_up(&mut e, &wan_dhcp(&wan_reply(&refused)), 200),
            NOTHING
        );
        assert_eq!(e.up.state, UpState::Init);
        assert_eq!(e.up.ip, None);
        let (up, _) = tick(&mut e, 201);
        assert_eq!(dhcp_payload(&up[0]).msg_type, Some(dhcp::DISCOVER));
    }

    #[test]
    fn dhcp_replies_that_are_not_for_us_are_ignored() {
        let mut e = engine();
        let (up, _) = tick(&mut e, 0);
        let discover = dhcp_payload(&up[0]);
        let offer = wan_reply(&discover);
        // An ACK before we asked for anything.
        let ack = wan_reply(&dhcp::Message {
            msg_type: Some(dhcp::REQUEST),
            requested_ip: Some(WAN_IP),
            ..discover
        });
        assert_eq!(
            dhcp::Message::parse(&ack).unwrap().msg_type,
            Some(dhcp::ACK)
        );
        let changed = |off: usize, byte: u8| {
            let mut p = offer.clone();
            p[off] = byte;
            p
        };
        assert_eq!(offer[243], 54, "the server id follows the message type");
        for payload in [
            b"junk".to_vec(),
            changed(0, dhcp::BOOTREQUEST),
            changed(7, !offer[7]),   // xid
            changed(33, !offer[33]), // chaddr
            changed(243, 250),       // no server id
            ack,
        ] {
            assert_eq!(from_up(&mut e, &wan_dhcp(&payload), 1), NOTHING);
            assert_eq!(e.up.state, UpState::Discovering { since: 0 });
        }
        assert_eq!(e.stats, Stats::default());
    }

    #[test]
    fn static_upstream_only_needs_the_router_mac() {
        let mut e = engine_with("192.168.1.40");
        assert_eq!((e.up.ip, e.up.router), (Some(WAN_IP), Some(WAN_ROUTER)));
        let (up, down) = tick(&mut e, 0);
        assert_eq!((up.len(), down.len()), (1, 0));
        let who_has = Arp::parse_frame(&up[0]).unwrap();
        assert_eq!((who_has.sender_ip, who_has.target_ip), (WAN_IP, WAN_ROUTER));
        assert_eq!(tick(&mut e, ARP_RETRY_MS - 1), NOTHING);
        assert_eq!(tick(&mut e, ARP_RETRY_MS).0.len(), 1);
        let reply = arp::reply(&who_has, ROUTER_MAC, WAN_ROUTER);
        assert_eq!(from_up(&mut e, &reply, ARP_RETRY_MS), NOTHING);
        assert!(e.up.is_bound());
        assert_eq!(tick(&mut e, 10 * ARP_RETRY_MS), NOTHING);
    }

    #[test]
    fn idle_nat_flows_expire_on_tick() {
        let mut e = bound_engine();
        open_dns_flow(&mut e, 500);
        assert_eq!(e.nat.len(), 1);
        tick(&mut e, 500 + crate::nat::UDP_TIMEOUT_MS);
        assert_eq!(e.nat.len(), 1);
        tick(
            &mut e,
            500 + crate::nat::UDP_TIMEOUT_MS + NAT_EXPIRE_EVERY_MS,
        );
        assert!(e.nat.is_empty());
    }

    #[test]
    fn nat_roundtrip_udp() {
        let mut e = bound_engine();
        let (up, down) = from_down(&mut e, &from_work(REMOTE, 53), 1000);
        assert_eq!(e.stats.forwarded_out, 1);
        assert_eq!((up.len(), down.len()), (1, 0));
        let fwd = Ipv4Frame::parse(&up[0]).unwrap();
        assert_eq!(fwd.eth.src, UP_MAC);
        assert_eq!(fwd.eth.dst, ROUTER_MAC);
        assert_eq!(fwd.ip.src, WAN_IP);
        assert_eq!(fwd.ip.dst, REMOTE);
        assert_eq!(fwd.ip.ttl, 63);
        let public_port = fwd.l4.src_port().unwrap();
        assert!(public_port >= PORT_MIN);
        assert_eq!(fwd.l4.dst_port(), Some(53));
        assert_eq!(checksum(&up[0][14..34]), 0);
        let seg = &up[0][fwd.l4_off..fwd.end];
        assert_eq!(l4_checksum(fwd.ip.src, fwd.ip.dst, IPPROTO_UDP, seg), 0);

        // The answer comes back and is mapped to the client.
        let (up, down) = from_up(&mut e, &to_wan(REMOTE, public_port), 1100);
        assert_eq!(e.stats.forwarded_in, 1);
        assert_eq!((up.len(), down.len()), (0, 1));
        let back = Ipv4Frame::parse(&down[0]).unwrap();
        assert_eq!(back.eth.dst, WORK_MAC);
        assert_eq!(back.eth.src, DOWN_MAC);
        assert_eq!(back.ip.src, REMOTE);
        assert_eq!(back.ip.dst, WORK_IP);
        assert_eq!(back.l4.dst_port(), Some(5000));
        assert_eq!(checksum(&down[0][14..34]), 0);

        // Unsolicited inbound traffic is dropped.
        let stray = to_wan(Ipv4Addr::new(2, 2, 2, 2), public_port);
        assert_eq!(from_up(&mut e, &stray, 1200), NOTHING);
        assert_eq!(e.stats.unsolicited_in, 1);
    }

    #[test]
    fn upstream_junk_is_counted_or_ignored() {
        let mut e = bound_engine();
        let eth = |ethertype| {
            EthHeader {
                dst: UP_MAC,
                src: ROUTER_MAC,
                ethertype,
            }
            .to_bytes()
        };
        for f in [
            vec![0; 13],
            [eth(ETHERTYPE_ARP), vec![0; 27]].concat(),
            [eth(ETHERTYPE_IPV4), vec![0x45; 19]].concat(),
            eth(0x86dd), // IPv6 is not handled at all
        ] {
            assert_eq!(from_up(&mut e, &f, 500), NOTHING);
        }
        assert_eq!(
            e.stats,
            Stats {
                malformed: 3,
                ..Stats::default()
            }
        );
    }

    #[test]
    fn upstream_arp_is_answered_only_for_our_address() {
        let mut e = bound_engine();
        let not_us = arp::request(ROUTER_MAC, WAN_ROUTER, WAN_OTHER);
        assert_eq!(from_up(&mut e, &not_us, 500), NOTHING);
        let (up, down) = from_up(&mut e, &arp::request(OTHER_MAC, WAN_OTHER, WAN_IP), 500);
        assert_eq!((up.len(), down.len()), (1, 0));
        let a = Arp::parse_frame(&up[0]).unwrap();
        assert_eq!(
            (a.op, a.sender_mac, a.sender_ip),
            (ARP_REPLY, UP_MAC, WAN_IP)
        );
        assert_eq!((a.target_mac, a.target_ip), (OTHER_MAC, WAN_OTHER));
        assert_eq!(e.up.router_mac, Some(ROUTER_MAC));
    }

    #[test]
    fn upstream_ipv4_not_meant_for_a_client() {
        // Before DHCP completes, nothing is ours.
        let mut e = engine();
        assert_eq!(from_up(&mut e, &to_wan(REMOTE, PORT_MIN), 1), NOTHING);

        let mut e = bound_engine();
        let other_host =
            udp_frame(ROUTER_MAC, UP_MAC, REMOTE, WAN_OTHER, 53, PORT_MIN, b"").unwrap();
        assert_eq!(from_up(&mut e, &other_host, 500), NOTHING);
        assert_eq!(e.stats, Stats::default());

        // Pings to our address are answered.
        let ping = icmp(ICMP_ECHO_REQUEST, (ROUTER_MAC, UP_MAC), REMOTE, WAN_IP, 7);
        let (up, _) = from_up(&mut e, &ping, 500);
        let p = Ipv4Frame::parse(&up[0]).unwrap();
        assert_eq!((p.eth.dst, p.eth.src), (ROUTER_MAC, UP_MAC));
        assert_eq!((p.ip.src, p.ip.dst, p.ip.ttl), (WAN_IP, REMOTE, 64));
        let reply = L4::Icmp {
            kind: ICMP_ECHO_REPLY,
            code: 0,
            id: 7,
        };
        assert_eq!(p.l4, reply);
        assert_eq!(checksum(&up[0][p.l4_off..p.end]), 0);

        // No ports to translate, or no flow: unsolicited.
        let gre = ipv4_frame(ROUTER_MAC, UP_MAC, REMOTE, WAN_IP, 47, &[0; 4]).unwrap();
        for f in [gre, to_wan(REMOTE, PORT_MIN)] {
            assert_eq!(from_up(&mut e, &f, 500), NOTHING);
        }
        assert_eq!(
            e.stats,
            Stats {
                unsolicited_in: 2,
                ..Stats::default()
            }
        );
    }

    #[test]
    fn inbound_replies_need_a_ttl_and_a_known_client() {
        let mut e = bound_engine();
        let public_port = open_dns_flow(&mut e, 500);
        let last_hop = with_ttl(to_wan(REMOTE, public_port), 1);
        assert_eq!(from_up(&mut e, &last_hop, 501), NOTHING);
        e.cfg.clients.clear();
        assert_eq!(from_up(&mut e, &to_wan(REMOTE, public_port), 502), NOTHING);
        assert_eq!(
            e.stats,
            Stats {
                forwarded_out: 1,
                malformed: 1,
                unknown_client: 1,
                ..Stats::default()
            }
        );
    }

    #[test]
    fn rules_spoofing_and_unknown_clients() {
        let mut e = bound_engine();
        // Policy deny: UDP 123 is not in the allow list.
        assert_eq!(from_down(&mut e, &from_work(REMOTE, 123), 1), NOTHING);
        assert_eq!(e.stats.denied, 1);
        // Wrong source IP for that MAC.
        let spoof = udp_frame(WORK_MAC, DOWN_MAC, WAN_OTHER, REMOTE, 5000, 53, b"").unwrap();
        assert_eq!(from_down(&mut e, &spoof, 1), NOTHING);
        assert_eq!(e.stats.spoofed, 1);
        // Unknown MAC.
        let stranger = udp_frame(OTHER_MAC, DOWN_MAC, WORK_IP, REMOTE, 5000, 53, b"").unwrap();
        assert_eq!(from_down(&mut e, &stranger, 1), NOTHING);
        assert_eq!(e.stats.unknown_client, 1);
    }

    #[test]
    fn no_route_before_upstream_is_bound() {
        let mut e = engine();
        assert_eq!(from_down(&mut e, &from_work(REMOTE, 53), 1), NOTHING);
        assert_eq!(e.stats.no_route, 1);
    }

    #[test]
    fn downstream_junk_is_counted_or_ignored() {
        let mut e = bound_engine();
        let eth = |ethertype| {
            EthHeader {
                dst: DOWN_MAC,
                src: WORK_MAC,
                ethertype,
            }
            .to_bytes()
        };
        let mut release = dhcp::discover(1, WORK_MAC);
        assert_eq!(release[242], dhcp::DISCOVER, "the message type comes first");
        release[242] = 7;
        let ask_other = arp::request(WORK_MAC, WORK_IP, Ipv4Addr::new(10, 137, 0, 4));
        let gateway_asks = Arp::parse_frame(&arp::request(DOWN_MAC, GATEWAY, WORK_IP)).unwrap();
        let answer = arp::reply(&gateway_asks, WORK_MAC, WORK_IP);
        for f in [
            vec![0; 13],
            [eth(ETHERTYPE_ARP), vec![0; 27]].concat(),
            [eth(ETHERTYPE_IPV4), vec![0x45; 19]].concat(),
            dhcp_from_work(b"junk"),
            eth(0x86dd),
            dhcp_from_work(&release),
            ask_other,
            answer,
            from_work(GATEWAY, 53), // only pings are answered
        ] {
            assert_eq!(from_down(&mut e, &f, 500), NOTHING);
        }
        assert_eq!(
            e.stats,
            Stats {
                malformed: 4,
                ..Stats::default()
            }
        );
    }

    #[test]
    fn outbound_traffic_that_cannot_be_forwarded() {
        let mut e = bound_engine();
        let gre = ipv4_frame(WORK_MAC, DOWN_MAC, WORK_IP, REMOTE, 47, &[0; 4]).unwrap();
        let timestamp = icmp(13, (WORK_MAC, DOWN_MAC), WORK_IP, REMOTE, 1);
        let last_hop = with_ttl(from_work(REMOTE, 53), 1);
        for f in [gre, timestamp, last_hop] {
            assert_eq!(from_down(&mut e, &f, 500), NOTHING);
        }
        e.nat = crate::nat::tests::full(500);
        assert_eq!(from_down(&mut e, &from_work(REMOTE, 53), 500), NOTHING);
        assert_eq!(
            e.stats,
            Stats {
                denied: 2,
                malformed: 1,
                nat_full: 1,
                ..Stats::default()
            }
        );
    }

    #[test]
    fn downstream_arp_and_dhcp_are_answered() {
        let mut e = bound_engine();
        let (_, down) = from_down(&mut e, &arp::request(WORK_MAC, WORK_IP, GATEWAY), 1);
        let a = Arp::parse_frame(&down[0]).unwrap();
        assert_eq!(a.op, ARP_REPLY);
        assert_eq!(a.sender_mac, DOWN_MAC);
        assert_eq!(a.sender_ip, GATEWAY);

        let (up, down) = from_down(&mut e, &dhcp_from_work(&dhcp::discover(42, WORK_MAC)), 2);
        assert_eq!((up.len(), down.len()), (0, 1));
        let p = Ipv4Frame::parse(&down[0]).unwrap();
        assert_eq!(p.eth.dst, BROADCAST_MAC); // client set the broadcast flag
        let offer = dhcp_payload(&down[0]);
        assert_eq!(offer.msg_type, Some(dhcp::OFFER));
        assert_eq!(offer.yiaddr, WORK_IP);
        assert_eq!(offer.router, Some(GATEWAY));
        assert_eq!(offer.dns, [DNS]);
    }

    #[test]
    fn dhcp_replies_are_unicast_unless_broadcast_is_needed() {
        let mut e = bound_engine();
        let unicast = |mut payload: Vec<u8>| {
            payload[10..12].copy_from_slice(&[0, 0]); // clear the broadcast flag
            dhcp_from_work(&payload)
        };
        let (_, down) = from_down(&mut e, &unicast(dhcp::discover(42, WORK_MAC)), 1);
        let p = Ipv4Frame::parse(&down[0]).unwrap();
        assert_eq!((p.eth.dst, p.ip.dst), (WORK_MAC, WORK_IP));
        // A refusal is broadcast: the client may not hold the address.
        let wrong = dhcp::request(42, WORK_MAC, Ipv4Addr::new(10, 137, 0, 9), GATEWAY);
        let (_, down) = from_down(&mut e, &unicast(wrong), 2);
        assert_eq!(dhcp_payload(&down[0]).msg_type, Some(dhcp::NAK));
        let p = Ipv4Frame::parse(&down[0]).unwrap();
        assert_eq!((p.eth.dst, p.ip.dst), (BROADCAST_MAC, BROADCAST_IP));
    }

    #[test]
    fn ping_gateway_and_ping_through() {
        let mut e = bound_engine();
        let work_macs = (WORK_MAC, DOWN_MAC);
        let echo = icmp(ICMP_ECHO_REQUEST, work_macs, WORK_IP, GATEWAY, 0x1234);
        let (_, down) = from_down(&mut e, &echo, 1);
        let p = Ipv4Frame::parse(&down[0]).unwrap();
        assert_eq!(p.ip.src, GATEWAY);
        assert_eq!(p.ip.dst, WORK_IP);
        let reply = L4::Icmp {
            kind: ICMP_ECHO_REPLY,
            code: 0,
            id: 0x1234,
        };
        assert_eq!(p.l4, reply);
        assert_eq!(checksum(&down[0][p.l4_off..p.end]), 0);

        let echo = icmp(ICMP_ECHO_REQUEST, work_macs, WORK_IP, REMOTE, 0x1234);
        let (up, _) = from_down(&mut e, &echo, 2);
        assert_eq!(up.len(), 1);
        let p = Ipv4Frame::parse(&up[0]).unwrap();
        let id = p.l4.src_port().unwrap();
        assert_eq!(p.ip.src, WAN_IP);
        assert!(id >= PORT_MIN);
        assert_eq!(checksum(&up[0][p.l4_off..p.end]), 0);

        // The reply comes back with the *translated* identifier and must
        // reach the client with its original one (found under QEMU: the
        // flow was keyed on the untranslated id and every reply was
        // dropped as unsolicited).
        let back = icmp(ICMP_ECHO_REPLY, (ROUTER_MAC, UP_MAC), REMOTE, WAN_IP, id);
        let (_, down) = from_up(&mut e, &back, 3);
        let p = Ipv4Frame::parse(&down[0]).unwrap();
        assert_eq!(p.eth.dst, WORK_MAC);
        assert_eq!(p.ip.src, REMOTE);
        assert_eq!(p.ip.dst, WORK_IP);
        assert_eq!(p.l4, reply);
        assert_eq!(checksum(&down[0][p.l4_off..p.end]), 0);
        assert_eq!(e.stats.forwarded_in, 1);
        assert_eq!(e.stats.unsolicited_in, 0);
    }
}
