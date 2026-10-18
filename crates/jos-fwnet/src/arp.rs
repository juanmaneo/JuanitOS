//! ARP over Ethernet/IPv4 (RFC 826), just enough for a gateway.

use alloc::vec::Vec;
use core::net::Ipv4Addr;

use jos_fwcfg::MacAddr;

use crate::packet::{field, ip_field, EthHeader, BROADCAST_MAC, ETHERTYPE_ARP, ETH_HDR_LEN};

pub const ARP_REQUEST: u16 = 1;
pub const ARP_REPLY: u16 = 2;
const ARP_LEN: usize = 28;
/// Hardware type Ethernet, protocol IPv4, address lengths 6 and 4.
const ETHER_IPV4: [u8; 6] = [0, 1, 8, 0, 6, 4];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arp {
    pub op: u16,
    pub sender_mac: MacAddr,
    pub sender_ip: Ipv4Addr,
    pub target_mac: MacAddr,
    pub target_ip: Ipv4Addr,
}

impl Arp {
    /// `payload` starts after the Ethernet header.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let a = payload.get(..ARP_LEN)?;
        if field(a, 0) != ETHER_IPV4 {
            return None;
        }
        Some(Arp {
            op: u16::from_be_bytes(field(a, 6)),
            sender_mac: MacAddr(field(a, 8)),
            sender_ip: ip_field(a, 14),
            target_mac: MacAddr(field(a, 18)),
            target_ip: ip_field(a, 24),
        })
    }

    /// Parse directly from an Ethernet frame, checking the ethertype.
    pub fn parse_frame(frame: &[u8]) -> Option<Self> {
        let eth = EthHeader::parse(frame)?;
        if eth.ethertype != ETHERTYPE_ARP {
            return None;
        }
        Self::parse(frame.get(ETH_HDR_LEN..).unwrap_or_default())
    }

    /// Serialise as a complete Ethernet frame to `dst_mac`.
    pub fn to_frame(&self, dst_mac: MacAddr) -> Vec<u8> {
        let eth = EthHeader {
            dst: dst_mac,
            src: self.sender_mac,
            ethertype: ETHERTYPE_ARP,
        };
        [
            eth.to_bytes().as_slice(),
            &ETHER_IPV4,
            &self.op.to_be_bytes(),
            &self.sender_mac.0,
            &self.sender_ip.octets(),
            &self.target_mac.0,
            &self.target_ip.octets(),
        ]
        .concat()
    }
}

/// Broadcast "who has `target_ip`?" from `(mac, ip)`.
pub fn request(mac: MacAddr, ip: Ipv4Addr, target_ip: Ipv4Addr) -> Vec<u8> {
    Arp {
        op: ARP_REQUEST,
        sender_mac: mac,
        sender_ip: ip,
        target_mac: MacAddr::default(),
        target_ip,
    }
    .to_frame(BROADCAST_MAC)
}

/// Answer `req` claiming `ip` is at `mac`.
pub fn reply(req: &Arp, mac: MacAddr, ip: Ipv4Addr) -> Vec<u8> {
    Arp {
        op: ARP_REPLY,
        sender_mac: mac,
        sender_ip: ip,
        target_mac: req.sender_mac,
        target_ip: req.sender_ip,
    }
    .to_frame(req.sender_mac)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::tests::parsing_prefixes;

    const ME: MacAddr = MacAddr([2, 0, 0, 0, 0, 9]);
    const MY_IP: Ipv4Addr = Ipv4Addr::new(10, 137, 0, 2);
    const PEER_IP: Ipv4Addr = Ipv4Addr::new(10, 137, 0, 3);

    #[test]
    fn request_reply_roundtrip() {
        let f = request(ME, MY_IP, PEER_IP);
        assert_eq!(f.len(), 42);
        let a = Arp::parse_frame(&f).unwrap();
        assert_eq!(a.op, ARP_REQUEST);
        assert_eq!(a.target_ip, PEER_IP);
        let peer = MacAddr([2, 0, 0, 0, 0, 3]);
        let r = reply(&a, peer, PEER_IP);
        let eth = EthHeader::parse(&r).unwrap();
        assert_eq!(eth.dst, ME);
        assert_eq!(eth.src, peer);
        let b = Arp::parse_frame(&r).unwrap();
        assert_eq!(b.op, ARP_REPLY);
        assert_eq!(b.target_mac, ME);
        assert_eq!(b.target_ip, MY_IP);
    }

    #[test]
    fn truncated_or_foreign_frames_are_rejected() {
        let f = request(ME, MY_IP, PEER_IP);
        assert_eq!(
            parsing_prefixes(&f, &|b| Arp::parse_frame(b).is_some()),
            [42]
        );
        let ipv4 = [&f[..12], &[8, 0], &f[14..]].concat();
        let ipv6 = [&f[..16], &[0x86, 0xdd], &f[18..]].concat();
        assert_eq!(Arp::parse_frame(&ipv4), None);
        assert_eq!(Arp::parse_frame(&ipv6), None);
    }
}
