//! Port-restricted cone NAT (the mirage-nat model): every (client, port,
//! remote, port) flow gets its own public port; replies are accepted only
//! from the remote endpoint the client talked to.
//!
//! ICMP echo has no ports: the identifier plays the client/public port
//! role and is rewritten like one, while the remote side has no port at
//! all. Callers may pass whatever the packet carries as `remote_port`; it
//! is normalised to 0 for ICMP so a reply (which carries the *translated*
//! identifier in both directions) still matches the flow.

use alloc::vec::Vec;
use core::net::Ipv4Addr;

use crate::packet::{IPPROTO_ICMP, IPPROTO_TCP};

pub const PORT_MIN: u16 = 10000;
pub const PORT_MAX: u16 = 65535;
pub const MAX_FLOWS: usize = 65536 - 10000;

/// Idle timeouts in milliseconds.
pub const TCP_TIMEOUT_MS: u64 = 30 * 60 * 1000;
pub const UDP_TIMEOUT_MS: u64 = 60 * 1000;
pub const ICMP_TIMEOUT_MS: u64 = 30 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flow {
    pub proto: u8,
    pub client: Ipv4Addr,
    pub client_port: u16,
    pub remote: Ipv4Addr,
    pub remote_port: u16,
    pub public_port: u16,
    pub last_seen_ms: u64,
}

impl Flow {
    fn timeout(&self) -> u64 {
        match self.proto {
            IPPROTO_TCP => TCP_TIMEOUT_MS,
            IPPROTO_ICMP => ICMP_TIMEOUT_MS,
            _ => UDP_TIMEOUT_MS,
        }
    }

    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_seen_ms) > self.timeout()
    }
}

/// The remote-port part of a flow key (see the module docs).
fn remote_port_key(proto: u8, remote_port: u16) -> u16 {
    if proto == IPPROTO_ICMP {
        0
    } else {
        remote_port
    }
}

#[derive(Debug, Default)]
pub struct Nat {
    flows: Vec<Flow>,
    next_port: u16,
}

impl Nat {
    pub fn new() -> Self {
        Nat {
            flows: Vec::new(),
            next_port: PORT_MIN,
        }
    }

    pub fn len(&self) -> usize {
        self.flows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.flows.is_empty()
    }

    /// Public port for an outbound packet, allocating a flow if needed.
    /// `None` when the table is full.
    pub fn outbound(
        &mut self,
        now_ms: u64,
        proto: u8,
        client: Ipv4Addr,
        client_port: u16,
        remote: Ipv4Addr,
        remote_port: u16,
    ) -> Option<u16> {
        let remote_port = remote_port_key(proto, remote_port);
        if let Some(f) = self.flows.iter_mut().find(|f| {
            f.proto == proto
                && f.client == client
                && f.client_port == client_port
                && f.remote == remote
                && f.remote_port == remote_port
        }) {
            f.last_seen_ms = now_ms;
            return Some(f.public_port);
        }
        if self.flows.len() >= MAX_FLOWS {
            self.expire(now_ms);
            if self.flows.len() >= MAX_FLOWS {
                return None;
            }
        }
        let public_port = self.allocate_port(proto);
        self.flows.push(Flow {
            proto,
            client,
            client_port,
            remote,
            remote_port,
            public_port,
            last_seen_ms: now_ms,
        });
        Some(public_port)
    }

    /// Translate an inbound packet addressed to `public_port` from
    /// `(remote, remote_port)` back to `(client, client_port)`.
    pub fn inbound(
        &mut self,
        now_ms: u64,
        proto: u8,
        remote: Ipv4Addr,
        remote_port: u16,
        public_port: u16,
    ) -> Option<(Ipv4Addr, u16)> {
        let remote_port = remote_port_key(proto, remote_port);
        let f = self.flows.iter_mut().find(|f| {
            f.proto == proto
                && f.public_port == public_port
                && f.remote == remote
                && f.remote_port == remote_port
        })?;
        if f.expired(now_ms) {
            return None;
        }
        f.last_seen_ms = now_ms;
        Some((f.client, f.client_port))
    }

    /// Drop idle flows.
    pub fn expire(&mut self, now_ms: u64) {
        self.flows.retain(|f| !f.expired(now_ms));
    }

    /// The first port from `next_port` on (wrapping) that no `proto` flow
    /// uses. `outbound` calls it with fewer than [`MAX_FLOWS`] flows, each
    /// holding its own port, so one is always free.
    fn allocate_port(&mut self, proto: u8) -> u16 {
        let start = self.next_port;
        let in_use = |p: u16| {
            self.flows
                .iter()
                .any(|f| f.proto == proto && f.public_port == p)
        };
        let port = (start..=PORT_MAX)
            .chain(PORT_MIN..start)
            .find(|&p| !in_use(p))
            .unwrap_or(start);
        self.next_port = port.checked_add(1).unwrap_or(PORT_MIN);
        port
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::packet::IPPROTO_UDP;

    const C: Ipv4Addr = Ipv4Addr::new(10, 137, 0, 3);
    const R: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

    fn flow(proto: u8, port: u16, now_ms: u64) -> Flow {
        Flow {
            proto,
            client: C,
            client_port: port,
            remote: R,
            remote_port: 53,
            public_port: port,
            last_seen_ms: now_ms,
        }
    }

    /// A table whose UDP flows, last seen at `now_ms`, hold every public
    /// port (built directly: going through `outbound` is quadratic).
    pub(crate) fn full(now_ms: u64) -> Nat {
        let flows = (PORT_MIN..=PORT_MAX)
            .map(|port| flow(IPPROTO_UDP, port, now_ms))
            .collect::<Vec<_>>();
        assert_eq!(flows.len(), MAX_FLOWS);
        Nat {
            flows,
            next_port: PORT_MIN,
        }
    }

    #[test]
    fn a_full_table_refuses_new_flows_until_some_expire() {
        let mut nat = full(0);
        assert_eq!(nat.outbound(1, IPPROTO_UDP, C, 1, R, 1), None);
        // Existing flows keep working.
        assert_eq!(nat.outbound(1, IPPROTO_UDP, C, 20000, R, 53), Some(20000));
        // Once the idle flows are gone there is room again.
        let later = UDP_TIMEOUT_MS + 1;
        assert_eq!(nat.outbound(later, IPPROTO_UDP, C, 1, R, 1), Some(PORT_MIN));
        assert_eq!(nat.len(), 2);
    }

    #[test]
    fn timeouts_depend_on_the_protocol() {
        for (proto, timeout) in [
            (IPPROTO_TCP, TCP_TIMEOUT_MS),
            (IPPROTO_UDP, UDP_TIMEOUT_MS),
            (IPPROTO_ICMP, ICMP_TIMEOUT_MS),
            (47, UDP_TIMEOUT_MS),
        ] {
            assert_eq!(flow(proto, PORT_MIN, 0).timeout(), timeout, "{proto}");
        }
    }

    #[test]
    fn outbound_is_stable_and_inbound_is_restricted() {
        let mut nat = Nat::new();
        let p = nat.outbound(0, IPPROTO_UDP, C, 5000, R, 53).unwrap();
        assert_eq!(p, PORT_MIN);
        assert_eq!(nat.outbound(10, IPPROTO_UDP, C, 5000, R, 53), Some(p));
        assert_eq!(nat.len(), 1);
        // Reply from the right endpoint maps back.
        assert_eq!(nat.inbound(20, IPPROTO_UDP, R, 53, p), Some((C, 5000)));
        // Different remote port / host / proto: rejected.
        assert_eq!(nat.inbound(20, IPPROTO_UDP, R, 54, p), None);
        assert_eq!(
            nat.inbound(20, IPPROTO_UDP, Ipv4Addr::new(2, 2, 2, 2), 53, p),
            None
        );
        assert_eq!(nat.inbound(20, IPPROTO_TCP, R, 53, p), None);
    }

    #[test]
    fn distinct_flows_get_distinct_ports_per_proto() {
        let mut nat = Nat::new();
        let a = nat.outbound(0, IPPROTO_TCP, C, 5000, R, 443).unwrap();
        let b = nat.outbound(0, IPPROTO_TCP, C, 5001, R, 443).unwrap();
        let c = nat
            .outbound(0, IPPROTO_TCP, Ipv4Addr::new(10, 137, 0, 4), 5000, R, 443)
            .unwrap();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn flows_expire() {
        let mut nat = Nat::new();
        let p = nat.outbound(0, IPPROTO_UDP, C, 1, R, 2).unwrap();
        assert_eq!(
            nat.inbound(UDP_TIMEOUT_MS, IPPROTO_UDP, R, 2, p),
            Some((C, 1))
        );
        assert_eq!(
            nat.inbound(
                2 * UDP_TIMEOUT_MS + UDP_TIMEOUT_MS + 1,
                IPPROTO_UDP,
                R,
                2,
                p
            ),
            None
        );
        nat.expire(10 * UDP_TIMEOUT_MS);
        assert!(nat.is_empty());
        // A new flow after expiry may reuse the port.
        let q = nat
            .outbound(10 * UDP_TIMEOUT_MS, IPPROTO_UDP, C, 1, R, 2)
            .unwrap();
        assert!(q >= PORT_MIN);
    }

    #[test]
    fn port_allocation_wraps() {
        let mut nat = Nat::new();
        nat.next_port = PORT_MAX;
        let a = nat.outbound(0, IPPROTO_UDP, C, 1, R, 1).unwrap();
        let b = nat.outbound(0, IPPROTO_UDP, C, 2, R, 1).unwrap();
        assert_eq!(a, PORT_MAX);
        assert_eq!(b, PORT_MIN);
    }
}
