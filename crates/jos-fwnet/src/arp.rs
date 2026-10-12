//! ARP over Ethernet/IPv4 (RFC 826), just enough for a gateway.

use alloc::vec::Vec;
use core::net::Ipv4Addr;

use jos_fwcfg::MacAddr;

use crate::packet::{
    be16, ipv4_at, mac_at, put_be16, put_bytes, put_ipv4, put_mac, EthHeader, ETHERTYPE_ARP,
    ETH_HDR_LEN,
};

pub const ARP_REQUEST: u16 = 1;
pub const ARP_REPLY: u16 = 2;
const ARP_LEN: usize = 28;

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
        if be16(payload, 0)? != 1 || be16(payload, 2)? != 0x0800 {
            return None;
        }
        if *payload.get(4)? != 6 || *payload.get(5)? != 4 {
            return None;
        }
        Some(Arp {
            op: be16(payload, 6)?,
            sender_mac: mac_at(payload, 8)?,
            sender_ip: ipv4_at(payload, 14)?,
            target_mac: mac_at(payload, 18)?,
            target_ip: ipv4_at(payload, 24)?,
        })
    }

    /// Parse directly from an Ethernet frame, checking the ethertype.
    pub fn parse_frame(frame: &[u8]) -> Option<Self> {
        let eth = EthHeader::parse(frame)?;
        if eth.ethertype != ETHERTYPE_ARP {
            return None;
        }
        Self::parse(frame.get(ETH_HDR_LEN..)?)
    }

    /// Serialise as a complete Ethernet frame to `dst_mac`.
    pub fn to_frame(&self, dst_mac: MacAddr) -> Option<Vec<u8>> {
        let mut f = alloc::vec![0u8; ETH_HDR_LEN + ARP_LEN];
        EthHeader {
            dst: dst_mac,
            src: self.sender_mac,
            ethertype: ETHERTYPE_ARP,
        }
        .write(&mut f)?;
        let a = ETH_HDR_LEN;
        put_be16(&mut f, a, 1)?;
        put_be16(&mut f, a + 2, 0x0800)?;
        put_bytes(&mut f, a + 4, &[6, 4])?;
        put_be16(&mut f, a + 6, self.op)?;
        put_mac(&mut f, a + 8, self.sender_mac)?;
        put_ipv4(&mut f, a + 14, self.sender_ip)?;
        put_mac(&mut f, a + 18, self.target_mac)?;
        put_ipv4(&mut f, a + 24, self.target_ip)?;
        Some(f)
    }
}

/// Broadcast "who has `target_ip`?" from `(mac, ip)`.
pub fn request(mac: MacAddr, ip: Ipv4Addr, target_ip: Ipv4Addr) -> Option<Vec<u8>> {
    Arp {
        op: ARP_REQUEST,
        sender_mac: mac,
        sender_ip: ip,
        target_mac: MacAddr::default(),
        target_ip,
    }
    .to_frame(crate::packet::BROADCAST_MAC)
}

/// Answer `req` claiming `ip` is at `mac`.
pub fn reply(req: &Arp, mac: MacAddr, ip: Ipv4Addr) -> Option<Vec<u8>> {
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

    #[test]
    fn request_reply_roundtrip() {
        let me = MacAddr([2, 0, 0, 0, 0, 9]);
        let ip = Ipv4Addr::new(10, 137, 0, 2);
        let f = request(me, ip, Ipv4Addr::new(10, 137, 0, 3)).unwrap();
        assert_eq!(f.len(), 42);
        let a = Arp::parse_frame(&f).unwrap();
        assert_eq!(a.op, ARP_REQUEST);
        assert_eq!(a.target_ip, Ipv4Addr::new(10, 137, 0, 3));
        let peer = MacAddr([2, 0, 0, 0, 0, 3]);
        let r = reply(&a, peer, Ipv4Addr::new(10, 137, 0, 3)).unwrap();
        let eth = EthHeader::parse(&r).unwrap();
        assert_eq!(eth.dst, me);
        assert_eq!(eth.src, peer);
        let b = Arp::parse_frame(&r).unwrap();
        assert_eq!(b.op, ARP_REPLY);
        assert_eq!(b.target_mac, me);
        assert_eq!(b.target_ip, ip);
        assert!(Arp::parse_frame(&f[..30]).is_none());
    }
}
