//! Bounds-checked views over raw frames. Every accessor returns `Option`
//! so a truncated or malformed packet can never panic the firewall.

use alloc::vec::Vec;
use core::net::Ipv4Addr;

use jos_fwcfg::MacAddr;

pub const ETH_HDR_LEN: usize = 14;
pub const IPV4_MIN_HDR_LEN: usize = 20;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const IPPROTO_ICMP: u8 = 1;
pub const IPPROTO_TCP: u8 = 6;
pub const IPPROTO_UDP: u8 = 17;
pub const ICMP_ECHO_REPLY: u8 = 0;
pub const ICMP_ECHO_REQUEST: u8 = 8;
pub const BROADCAST_MAC: MacAddr = MacAddr([0xff; 6]);
/// Largest frame the firewall handles (Ethernet MTU 1500 + header).
pub const MAX_FRAME: usize = 1514;

// ---------------------------------------------------------------- bytes

pub fn be16(b: &[u8], off: usize) -> Option<u16> {
    let s: [u8; 2] = b.get(off..)?.get(..2)?.try_into().ok()?;
    Some(u16::from_be_bytes(s))
}

pub fn be32(b: &[u8], off: usize) -> Option<u32> {
    let s: [u8; 4] = b.get(off..)?.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(s))
}

pub fn ipv4_at(b: &[u8], off: usize) -> Option<Ipv4Addr> {
    be32(b, off).map(Ipv4Addr::from)
}

pub fn mac_at(b: &[u8], off: usize) -> Option<MacAddr> {
    let s: [u8; 6] = b.get(off..)?.get(..6)?.try_into().ok()?;
    Some(MacAddr(s))
}

pub fn put_bytes(b: &mut [u8], off: usize, src: &[u8]) -> Option<()> {
    b.get_mut(off..)?.get_mut(..src.len())?.copy_from_slice(src);
    Some(())
}

pub fn put_be16(b: &mut [u8], off: usize, v: u16) -> Option<()> {
    put_bytes(b, off, &v.to_be_bytes())
}

pub fn put_be32(b: &mut [u8], off: usize, v: u32) -> Option<()> {
    put_bytes(b, off, &v.to_be_bytes())
}

pub fn put_ipv4(b: &mut [u8], off: usize, v: Ipv4Addr) -> Option<()> {
    put_bytes(b, off, &v.octets())
}

pub fn put_mac(b: &mut [u8], off: usize, v: MacAddr) -> Option<()> {
    put_bytes(b, off, &v.0)
}

// ------------------------------------------------------------ checksums

/// Accumulate 16-bit big-endian words of `data` into a ones-complement sum.
pub fn sum16(mut acc: u32, data: &[u8]) -> u32 {
    let (words, rest) = data.as_chunks::<2>();
    for w in words {
        acc = acc.wrapping_add(u32::from(u16::from_be_bytes(*w)));
    }
    if let [last] = rest {
        acc = acc.wrapping_add(u32::from(*last) << 8);
    }
    acc
}

/// Fold a [`sum16`] accumulator into the final inverted 16-bit checksum.
pub fn finish_checksum(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

pub fn checksum(data: &[u8]) -> u16 {
    finish_checksum(sum16(0, data))
}

/// TCP/UDP checksum including the IPv4 pseudo header.
pub fn l4_checksum(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, segment: &[u8]) -> u16 {
    let [s0, s1, s2, s3] = src.octets();
    let [d0, d1, d2, d3] = dst.octets();
    let len = u16::try_from(segment.len())
        .unwrap_or(u16::MAX)
        .to_be_bytes();
    let pseudo = [s0, s1, s2, s3, d0, d1, d2, d3, 0, proto, len[0], len[1]];
    finish_checksum(sum16(sum16(0, &pseudo), segment))
}

// ------------------------------------------------------------- ethernet

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EthHeader {
    pub dst: MacAddr,
    pub src: MacAddr,
    pub ethertype: u16,
}

impl EthHeader {
    pub fn parse(frame: &[u8]) -> Option<Self> {
        Some(EthHeader {
            dst: mac_at(frame, 0)?,
            src: mac_at(frame, 6)?,
            ethertype: be16(frame, 12)?,
        })
    }

    pub fn write(&self, frame: &mut [u8]) -> Option<()> {
        put_mac(frame, 0, self.dst)?;
        put_mac(frame, 6, self.src)?;
        put_be16(frame, 12, self.ethertype)
    }
}

// ----------------------------------------------------------------- ipv4

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Header {
    pub header_len: usize,
    pub total_len: usize,
    pub ttl: u8,
    pub proto: u8,
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    /// More-fragments set or non-zero offset: we do not reassemble.
    pub fragmented: bool,
}

impl Ipv4Header {
    /// `pkt` starts at the IP header.
    pub fn parse(pkt: &[u8]) -> Option<Self> {
        let vihl = *pkt.first()?;
        if vihl >> 4 != 4 {
            return None;
        }
        let header_len = usize::from(vihl & 0x0f) * 4;
        if header_len < IPV4_MIN_HDR_LEN {
            return None;
        }
        let total_len = usize::from(be16(pkt, 2)?);
        if total_len < header_len || total_len > pkt.len() {
            return None;
        }
        let frag = be16(pkt, 6)?;
        Some(Ipv4Header {
            header_len,
            total_len,
            ttl: *pkt.get(8)?,
            proto: *pkt.get(9)?,
            src: ipv4_at(pkt, 12)?,
            dst: ipv4_at(pkt, 16)?,
            fragmented: frag & 0x3fff != 0,
        })
    }
}

/// Transport-layer summary of an IPv4 packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L4 {
    Tcp { src_port: u16, dst_port: u16 },
    Udp { src_port: u16, dst_port: u16 },
    Icmp { kind: u8, code: u8, id: u16 },
    Other(u8),
}

impl L4 {
    /// `seg` starts at the transport header.
    pub fn parse(proto: u8, seg: &[u8]) -> Option<Self> {
        Some(match proto {
            IPPROTO_TCP => L4::Tcp {
                src_port: be16(seg, 0)?,
                dst_port: be16(seg, 2)?,
            },
            IPPROTO_UDP => L4::Udp {
                src_port: be16(seg, 0)?,
                dst_port: be16(seg, 2)?,
            },
            IPPROTO_ICMP => L4::Icmp {
                kind: *seg.first()?,
                code: *seg.get(1)?,
                id: be16(seg, 4).unwrap_or(0),
            },
            other => L4::Other(other),
        })
    }

    pub fn src_port(&self) -> Option<u16> {
        match self {
            L4::Tcp { src_port, .. } | L4::Udp { src_port, .. } => Some(*src_port),
            L4::Icmp { id, .. } => Some(*id),
            L4::Other(_) => None,
        }
    }

    pub fn dst_port(&self) -> Option<u16> {
        match self {
            L4::Tcp { dst_port, .. } | L4::Udp { dst_port, .. } => Some(*dst_port),
            L4::Icmp { id, .. } => Some(*id),
            L4::Other(_) => None,
        }
    }
}

/// Offsets of a parsed IPv4-over-Ethernet frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Frame {
    pub eth: EthHeader,
    pub ip: Ipv4Header,
    pub l4: L4,
    /// Offset of the transport header within the frame.
    pub l4_off: usize,
    /// End of the IP packet within the frame (padding after is ignored).
    pub end: usize,
}

impl Ipv4Frame {
    pub fn parse(frame: &[u8]) -> Option<Self> {
        let eth = EthHeader::parse(frame)?;
        if eth.ethertype != ETHERTYPE_IPV4 {
            return None;
        }
        let pkt = frame.get(ETH_HDR_LEN..)?;
        let ip = Ipv4Header::parse(pkt)?;
        let l4_off = ETH_HDR_LEN.checked_add(ip.header_len)?;
        let end = ETH_HDR_LEN.checked_add(ip.total_len)?;
        let l4 = if ip.fragmented {
            L4::Other(ip.proto)
        } else {
            L4::parse(ip.proto, frame.get(l4_off..end)?)?
        };
        Some(Ipv4Frame {
            eth,
            ip,
            l4,
            l4_off,
            end,
        })
    }

    pub fn set_src_ip(&self, frame: &mut [u8], ip: Ipv4Addr) -> Option<()> {
        put_ipv4(frame, ETH_HDR_LEN + 12, ip)
    }

    pub fn set_dst_ip(&self, frame: &mut [u8], ip: Ipv4Addr) -> Option<()> {
        put_ipv4(frame, ETH_HDR_LEN + 16, ip)
    }

    /// Rewrite the source port (TCP/UDP) or echo id (ICMP).
    pub fn set_src_port(&self, frame: &mut [u8], port: u16) -> Option<()> {
        match self.l4 {
            L4::Tcp { .. } | L4::Udp { .. } => put_be16(frame, self.l4_off, port),
            L4::Icmp { .. } => put_be16(frame, self.l4_off.checked_add(4)?, port),
            L4::Other(_) => None,
        }
    }

    /// Rewrite the destination port (TCP/UDP) or echo id (ICMP).
    pub fn set_dst_port(&self, frame: &mut [u8], port: u16) -> Option<()> {
        match self.l4 {
            L4::Tcp { .. } | L4::Udp { .. } => put_be16(frame, self.l4_off.checked_add(2)?, port),
            L4::Icmp { .. } => put_be16(frame, self.l4_off.checked_add(4)?, port),
            L4::Other(_) => None,
        }
    }

    /// Decrement TTL; `Some(false)` means the packet must be dropped.
    pub fn decrement_ttl(&self, frame: &mut [u8]) -> Option<bool> {
        let ttl = frame.get_mut(ETH_HDR_LEN + 8)?;
        if *ttl <= 1 {
            return Some(false);
        }
        *ttl -= 1;
        Some(true)
    }

    /// Recompute the IPv4 header checksum and the transport checksum from
    /// the (possibly rewritten) bytes in `frame`.
    pub fn fix_checksums(&self, frame: &mut [u8]) -> Option<()> {
        put_be16(frame, ETH_HDR_LEN + 10, 0)?;
        let hdr_sum = checksum(frame.get(ETH_HDR_LEN..self.l4_off)?);
        put_be16(frame, ETH_HDR_LEN + 10, hdr_sum)?;
        if self.ip.fragmented {
            return Some(());
        }
        let src = ipv4_at(frame, ETH_HDR_LEN + 12)?;
        let dst = ipv4_at(frame, ETH_HDR_LEN + 16)?;
        match self.l4 {
            L4::Tcp { .. } => {
                let off = self.l4_off.checked_add(16)?;
                put_be16(frame, off, 0)?;
                let sum = l4_checksum(src, dst, IPPROTO_TCP, frame.get(self.l4_off..self.end)?);
                put_be16(frame, off, sum)
            }
            L4::Udp { .. } => {
                let off = self.l4_off.checked_add(6)?;
                if be16(frame, off)? == 0 {
                    return Some(()); // checksum disabled by the sender
                }
                put_be16(frame, off, 0)?;
                let sum = l4_checksum(src, dst, IPPROTO_UDP, frame.get(self.l4_off..self.end)?);
                // RFC 768: a computed zero is transmitted as all ones.
                put_be16(frame, off, if sum == 0 { 0xffff } else { sum })
            }
            L4::Icmp { .. } => {
                let off = self.l4_off.checked_add(2)?;
                put_be16(frame, off, 0)?;
                let sum = checksum(frame.get(self.l4_off..self.end)?);
                put_be16(frame, off, sum)
            }
            L4::Other(_) => Some(()),
        }
    }
}

// ------------------------------------------------------------- builders

/// Build a complete Ethernet + IPv4 + UDP frame around `payload`.
pub fn udp_frame(
    src_mac: MacAddr,
    dst_mac: MacAddr,
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let udp_len = u16::try_from(payload.len().checked_add(8)?).ok()?;
    let total_len = u16::try_from(usize::from(udp_len).checked_add(IPV4_MIN_HDR_LEN)?).ok()?;
    let mut f = alloc::vec![0u8; ETH_HDR_LEN + usize::from(total_len)];
    EthHeader {
        dst: dst_mac,
        src: src_mac,
        ethertype: ETHERTYPE_IPV4,
    }
    .write(&mut f)?;
    let ip = ETH_HDR_LEN;
    put_bytes(&mut f, ip, &[0x45, 0])?;
    put_be16(&mut f, ip + 2, total_len)?;
    put_be16(&mut f, ip + 4, 0)?; // id
    put_be16(&mut f, ip + 6, 0x4000)?; // DF
    put_bytes(&mut f, ip + 8, &[64, IPPROTO_UDP])?;
    put_ipv4(&mut f, ip + 12, src_ip)?;
    put_ipv4(&mut f, ip + 16, dst_ip)?;
    let hdr_sum = checksum(f.get(ip..ip + IPV4_MIN_HDR_LEN)?);
    put_be16(&mut f, ip + 10, hdr_sum)?;
    let udp = ip + IPV4_MIN_HDR_LEN;
    put_be16(&mut f, udp, src_port)?;
    put_be16(&mut f, udp + 2, dst_port)?;
    put_be16(&mut f, udp + 4, udp_len)?;
    put_be16(&mut f, udp + 6, 0)?;
    put_bytes(&mut f, udp + 8, payload)?;
    let sum = l4_checksum(src_ip, dst_ip, IPPROTO_UDP, f.get(udp..)?);
    put_be16(&mut f, udp + 6, if sum == 0 { 0xffff } else { sum })?;
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_A: MacAddr = MacAddr([2, 0, 0, 0, 0, 1]);
    const MAC_B: MacAddr = MacAddr([2, 0, 0, 0, 0, 2]);

    #[test]
    fn checksum_rfc1071_example() {
        // RFC 1071 §3 example: 0x0001 0xf203 0xf4f5 0xf6f7 -> 0x220d.
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&data), 0x220d);
        assert_eq!(checksum(&[]), 0xffff);
    }

    #[test]
    fn udp_frame_roundtrip_and_valid_checksums() {
        let f = udp_frame(
            MAC_A,
            MAC_B,
            Ipv4Addr::new(10, 137, 0, 3),
            Ipv4Addr::new(1, 1, 1, 1),
            12345,
            53,
            b"hello",
        )
        .unwrap();
        assert_eq!(f.len(), 14 + 20 + 8 + 5);
        let p = Ipv4Frame::parse(&f).unwrap();
        assert_eq!(p.eth.src, MAC_A);
        assert_eq!(p.ip.src, Ipv4Addr::new(10, 137, 0, 3));
        assert_eq!(p.ip.proto, IPPROTO_UDP);
        assert_eq!(
            p.l4,
            L4::Udp {
                src_port: 12345,
                dst_port: 53
            }
        );
        // Verifying a checksum over the header including its own checksum
        // field yields zero.
        assert_eq!(checksum(&f[14..34]), 0);
        assert_eq!(
            l4_checksum(p.ip.src, p.ip.dst, IPPROTO_UDP, &f[p.l4_off..p.end]),
            0
        );
    }

    #[test]
    fn rewrite_then_fix_checksums_is_consistent() {
        let mut f = udp_frame(
            MAC_A,
            MAC_B,
            Ipv4Addr::new(10, 137, 0, 3),
            Ipv4Addr::new(1, 1, 1, 1),
            40000,
            53,
            b"query",
        )
        .unwrap();
        let p = Ipv4Frame::parse(&f).unwrap();
        p.set_src_ip(&mut f, Ipv4Addr::new(192, 168, 1, 50))
            .unwrap();
        p.set_src_port(&mut f, 10001).unwrap();
        assert!(p.decrement_ttl(&mut f).unwrap());
        p.fix_checksums(&mut f).unwrap();
        let q = Ipv4Frame::parse(&f).unwrap();
        assert_eq!(q.ip.src, Ipv4Addr::new(192, 168, 1, 50));
        assert_eq!(q.ip.ttl, 63);
        assert_eq!(q.l4.src_port(), Some(10001));
        assert_eq!(checksum(&f[14..34]), 0);
        assert_eq!(
            l4_checksum(q.ip.src, q.ip.dst, IPPROTO_UDP, &f[q.l4_off..q.end]),
            0
        );
    }

    #[test]
    fn truncated_frames_are_rejected_not_panicking() {
        let f = udp_frame(
            MAC_A,
            MAC_B,
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            1,
            2,
            b"x",
        )
        .unwrap();
        for n in 0..f.len() {
            assert!(Ipv4Frame::parse(&f[..n]).is_none(), "len {n}");
        }
        assert!(Ipv4Frame::parse(&[]).is_none());
        let mut bad = f.clone();
        bad[14] = 0x60; // IPv6 version nibble
        assert!(Ipv4Frame::parse(&bad).is_none());
    }

    #[test]
    fn ttl_one_is_dropped() {
        let mut f = udp_frame(
            MAC_A,
            MAC_B,
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            1,
            2,
            b"",
        )
        .unwrap();
        f[14 + 8] = 1;
        let p = Ipv4Frame::parse(&f).unwrap();
        assert_eq!(p.decrement_ttl(&mut f), Some(false));
    }
}
