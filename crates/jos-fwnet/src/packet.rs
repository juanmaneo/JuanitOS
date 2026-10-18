//! Bounds-checked views over raw frames. Parsers check a header's length
//! once and then read fixed fields, so a truncated or malformed packet can
//! never panic the firewall.

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

/// The `N` bytes of `b` at `off`, if they are all there.
pub(crate) fn chunk<const N: usize>(b: &[u8], off: usize) -> Option<[u8; N]> {
    b.get(off..).and_then(<[u8]>::first_chunk::<N>).copied()
}

/// [`chunk`] for a field whose header length the caller has checked.
pub(crate) fn field<const N: usize>(b: &[u8], off: usize) -> [u8; N] {
    chunk(b, off).unwrap_or([0; N])
}

/// The IPv4 address field at `off` (see [`field`]).
pub(crate) fn ip_field(b: &[u8], off: usize) -> Ipv4Addr {
    Ipv4Addr::from(field::<4>(b, off))
}

/// Copy `src` into `b` at `off`; a write past the end is dropped.
pub fn put_bytes(b: &mut [u8], off: usize, src: &[u8]) {
    if let Some(d) = b.get_mut(off..).and_then(|d| d.get_mut(..src.len())) {
        d.copy_from_slice(src);
    }
}

pub fn put_be16(b: &mut [u8], off: usize, v: u16) {
    put_bytes(b, off, &v.to_be_bytes());
}

pub fn put_ipv4(b: &mut [u8], off: usize, v: Ipv4Addr) {
    put_bytes(b, off, &v.octets());
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
    let [l0, l1] = u16::try_from(segment.len())
        .unwrap_or(u16::MAX)
        .to_be_bytes();
    let pseudo = [s0, s1, s2, s3, d0, d1, d2, d3, 0, proto, l0, l1];
    finish_checksum(sum16(sum16(0, &pseudo), segment))
}

/// UDP checksum: RFC 768 transmits a computed zero as all ones, since a
/// zero field means "no checksum".
pub fn udp_checksum(src: Ipv4Addr, dst: Ipv4Addr, segment: &[u8]) -> u16 {
    match l4_checksum(src, dst, IPPROTO_UDP, segment) {
        0 => 0xffff,
        sum => sum,
    }
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
        let h = frame.get(..ETH_HDR_LEN)?;
        Some(EthHeader {
            dst: MacAddr(field(h, 0)),
            src: MacAddr(field(h, 6)),
            ethertype: u16::from_be_bytes(field(h, 12)),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        [
            self.dst.0.as_slice(),
            &self.src.0,
            &self.ethertype.to_be_bytes(),
        ]
        .concat()
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
        let h = pkt.get(..IPV4_MIN_HDR_LEN)?;
        let [vihl, _] = field(h, 0);
        let header_len = usize::from(vihl & 0x0f) * 4;
        let total_len = usize::from(u16::from_be_bytes(field(h, 2)));
        if vihl >> 4 != 4
            || header_len < IPV4_MIN_HDR_LEN
            || total_len < header_len
            || total_len > pkt.len()
        {
            return None;
        }
        let [ttl, proto] = field(h, 8);
        Some(Ipv4Header {
            header_len,
            total_len,
            ttl,
            proto,
            src: ip_field(h, 12),
            dst: ip_field(h, 16),
            fragmented: u16::from_be_bytes(field(h, 6)) & 0x3fff != 0,
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
    /// `seg` starts at the transport header, which must be complete.
    pub fn parse(proto: u8, seg: &[u8]) -> Option<Self> {
        let min = match proto {
            IPPROTO_TCP => 20,
            IPPROTO_UDP | IPPROTO_ICMP => 8,
            other => return Some(L4::Other(other)),
        };
        let h = seg.get(..min)?;
        let [a, b] = [field(h, 0), field(h, 2)].map(u16::from_be_bytes);
        Some(match proto {
            IPPROTO_TCP => L4::Tcp {
                src_port: a,
                dst_port: b,
            },
            IPPROTO_UDP => L4::Udp {
                src_port: a,
                dst_port: b,
            },
            _ => {
                let [kind, code] = field(h, 0);
                L4::Icmp {
                    kind,
                    code,
                    id: u16::from_be_bytes(field(h, 4)),
                }
            }
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
        let ip = Ipv4Header::parse(frame.get(ETH_HDR_LEN..).unwrap_or_default())?;
        let l4_off = ETH_HDR_LEN + ip.header_len;
        let end = ETH_HDR_LEN + ip.total_len;
        let l4 = if ip.fragmented {
            L4::Other(ip.proto)
        } else {
            L4::parse(ip.proto, frame.get(l4_off..end).unwrap_or_default())?
        };
        Some(Ipv4Frame {
            eth,
            ip,
            l4,
            l4_off,
            end,
        })
    }

    /// The IP packet of `frame` behind a new Ethernet header, without any
    /// link-layer padding.
    pub fn rebuild(&self, frame: &[u8], eth: EthHeader) -> Vec<u8> {
        let pkt = frame.get(ETH_HDR_LEN..self.end).unwrap_or_default();
        [eth.to_bytes().as_slice(), pkt].concat()
    }

    pub fn set_src_ip(&self, frame: &mut [u8], ip: Ipv4Addr) {
        put_ipv4(frame, ETH_HDR_LEN + 12, ip);
    }

    pub fn set_dst_ip(&self, frame: &mut [u8], ip: Ipv4Addr) {
        put_ipv4(frame, ETH_HDR_LEN + 16, ip);
    }

    pub fn set_ttl(&self, frame: &mut [u8], ttl: u8) {
        put_bytes(frame, ETH_HDR_LEN + 8, &[ttl]);
    }

    /// Rewrite the source port (TCP/UDP) or echo id (ICMP).
    pub fn set_src_port(&self, frame: &mut [u8], port: u16) {
        self.put_port(frame, 0, port);
    }

    /// Rewrite the destination port (TCP/UDP) or echo id (ICMP).
    pub fn set_dst_port(&self, frame: &mut [u8], port: u16) {
        self.put_port(frame, 2, port);
    }

    fn put_port(&self, frame: &mut [u8], tcp_udp_off: usize, port: u16) {
        let off = match self.l4 {
            L4::Tcp { .. } | L4::Udp { .. } => tcp_udp_off,
            L4::Icmp { .. } => 4,
            L4::Other(_) => return,
        };
        put_be16(frame, self.l4_off + off, port);
    }

    /// Recompute the IPv4 header checksum and the transport checksum from
    /// the (possibly rewritten) bytes in `frame`.
    pub fn fix_checksums(&self, frame: &mut [u8]) {
        let ip = ETH_HDR_LEN;
        put_be16(frame, ip + 10, 0);
        let hdr_sum = checksum(frame.get(ip..self.l4_off).unwrap_or_default());
        put_be16(frame, ip + 10, hdr_sum);
        let (src, dst) = (ip_field(frame, ip + 12), ip_field(frame, ip + 16));
        let sum_off = match self.l4 {
            L4::Tcp { .. } => 16,
            // A zero UDP checksum means the sender disabled it.
            L4::Udp { .. } if field(frame, self.l4_off + 6) == [0u8; 2] => return,
            L4::Udp { .. } => 6,
            L4::Icmp { .. } => 2,
            L4::Other(_) => return,
        };
        let sum_at = self.l4_off + sum_off;
        put_be16(frame, sum_at, 0);
        let seg = frame.get(self.l4_off..self.end).unwrap_or_default();
        let sum = match self.l4 {
            L4::Tcp { .. } => l4_checksum(src, dst, IPPROTO_TCP, seg),
            L4::Udp { .. } => udp_checksum(src, dst, seg),
            _ => checksum(seg),
        };
        put_be16(frame, sum_at, sum);
    }
}

// ------------------------------------------------------------- builders

/// Build an Ethernet + IPv4 frame (DF set, TTL 64) around a transport
/// `segment`; `None` when the packet would exceed 65535 bytes.
pub fn ipv4_frame(
    src_mac: MacAddr,
    dst_mac: MacAddr,
    src: Ipv4Addr,
    dst: Ipv4Addr,
    proto: u8,
    segment: &[u8],
) -> Option<Vec<u8>> {
    let [t0, t1] = u16::try_from(IPV4_MIN_HDR_LEN + segment.len())
        .ok()?
        .to_be_bytes();
    let [s0, s1, s2, s3] = src.octets();
    let [d0, d1, d2, d3] = dst.octets();
    let mut hdr = [
        0x45, 0, t0, t1, 0, 0, 0x40, 0, 64, proto, 0, 0, s0, s1, s2, s3, d0, d1, d2, d3,
    ];
    let hdr_sum = checksum(&hdr);
    put_be16(&mut hdr, 10, hdr_sum);
    let eth = EthHeader {
        dst: dst_mac,
        src: src_mac,
        ethertype: ETHERTYPE_IPV4,
    };
    Some([eth.to_bytes().as_slice(), &hdr, segment].concat())
}

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
    let len = u16::try_from(payload.len() + 8).ok()?;
    let header = [src_port, dst_port, len, 0].map(u16::to_be_bytes);
    let mut seg = [header.as_flattened(), payload].concat();
    let sum = udp_checksum(src_ip, dst_ip, &seg);
    put_be16(&mut seg, 6, sum);
    ipv4_frame(src_mac, dst_mac, src_ip, dst_ip, IPPROTO_UDP, &seg)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const MAC_A: MacAddr = MacAddr([2, 0, 0, 0, 0, 1]);
    const MAC_B: MacAddr = MacAddr([2, 0, 0, 0, 0, 2]);
    const IP_A: Ipv4Addr = Ipv4Addr::new(10, 137, 0, 3);
    const IP_B: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

    /// The prefix lengths of `bytes` that `parses` accepts.
    pub(crate) fn parsing_prefixes(bytes: &[u8], parses: &dyn Fn(&[u8]) -> bool) -> Vec<usize> {
        (0..=bytes.len()).filter(|&n| parses(&bytes[..n])).collect()
    }

    fn udp(payload: &[u8]) -> Vec<u8> {
        udp_frame(MAC_A, MAC_B, IP_A, IP_B, 40000, 53, payload).unwrap()
    }

    /// A frame for `proto` whose transport segment is `seg`.
    fn frame(proto: u8, seg: &[u8]) -> Vec<u8> {
        ipv4_frame(MAC_A, MAC_B, IP_A, IP_B, proto, seg).unwrap()
    }

    /// `frame` with its transport checksum verified to be consistent.
    fn l4_ok(f: &[u8]) -> bool {
        let p = Ipv4Frame::parse(f).unwrap();
        let seg = &f[p.l4_off..p.end];
        let sum = match p.l4 {
            L4::Tcp { .. } => l4_checksum(p.ip.src, p.ip.dst, IPPROTO_TCP, seg),
            L4::Udp { .. } => l4_checksum(p.ip.src, p.ip.dst, IPPROTO_UDP, seg),
            _ => checksum(seg),
        };
        checksum(&f[14..p.l4_off]) == 0 && sum == 0
    }

    #[test]
    fn checksum_rfc1071_example() {
        // RFC 1071 §3 example: 0x0001 0xf203 0xf4f5 0xf6f7 -> 0x220d.
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&data), 0x220d);
        assert_eq!(checksum(&[]), 0xffff);
        assert_eq!(checksum(&[0xff]), 0x00ff);
    }

    #[test]
    fn a_computed_zero_udp_checksum_is_sent_as_all_ones() {
        // Appending the checksum of a segment to it makes the sum zero.
        let c = l4_checksum(IP_A, IP_B, IPPROTO_UDP, &[0, 0]);
        assert_eq!(l4_checksum(IP_A, IP_B, IPPROTO_UDP, &c.to_be_bytes()), 0);
        assert_eq!(udp_checksum(IP_A, IP_B, &c.to_be_bytes()), 0xffff);
    }

    #[test]
    fn udp_frame_roundtrip_and_valid_checksums() {
        let f = udp(b"hello");
        assert_eq!(f.len(), 14 + 20 + 8 + 5);
        let p = Ipv4Frame::parse(&f).unwrap();
        assert_eq!(p.eth.src, MAC_A);
        assert_eq!(p.eth.dst, MAC_B);
        assert_eq!((p.ip.src, p.ip.dst, p.ip.ttl), (IP_A, IP_B, 64));
        assert_eq!(p.ip.proto, IPPROTO_UDP);
        assert!(!p.ip.fragmented);
        assert_eq!(
            p.l4,
            L4::Udp {
                src_port: 40000,
                dst_port: 53
            }
        );
        assert!(l4_ok(&f));
    }

    #[test]
    fn oversized_packets_are_not_built() {
        assert_eq!(
            udp_frame(MAC_A, MAC_B, IP_A, IP_B, 1, 2, &[0; 65_528]),
            None
        );
        assert_eq!(
            udp_frame(MAC_A, MAC_B, IP_A, IP_B, 1, 2, &[0; 65_510]),
            None
        );
        assert_eq!(udp(&[0; 65_507]).len(), 14 + 65_535);
    }

    #[test]
    fn rewrite_then_fix_checksums_is_consistent() {
        for (proto, seg) in [
            (IPPROTO_UDP, udp(b"query")[34..].to_vec()),
            (
                IPPROTO_TCP,
                [[0x9c, 0x40, 0, 80].as_slice(), &[0; 16]].concat(),
            ),
            (
                IPPROTO_ICMP,
                [ICMP_ECHO_REQUEST, 0, 0, 0, 0x12, 0x34, 0, 1].to_vec(),
            ),
        ] {
            let mut f = frame(proto, &seg);
            let p = Ipv4Frame::parse(&f).unwrap();
            p.set_src_ip(&mut f, Ipv4Addr::new(192, 168, 1, 50));
            p.set_dst_ip(&mut f, Ipv4Addr::new(9, 9, 9, 9));
            p.set_src_port(&mut f, 10001);
            p.set_ttl(&mut f, 63);
            p.fix_checksums(&mut f);
            let q = Ipv4Frame::parse(&f).unwrap();
            assert_eq!(q.ip.src, Ipv4Addr::new(192, 168, 1, 50), "{proto}");
            assert_eq!(q.ip.dst, Ipv4Addr::new(9, 9, 9, 9), "{proto}");
            assert_eq!(q.ip.ttl, 63, "{proto}");
            assert_eq!(q.l4.src_port(), Some(10001), "{proto}");
            assert!(l4_ok(&f), "{proto}");
            p.set_dst_port(&mut f, 7);
            assert_eq!(Ipv4Frame::parse(&f).unwrap().l4.dst_port(), Some(7));
        }
    }

    #[test]
    fn disabled_udp_checksums_stay_disabled() {
        let mut f = udp(b"x");
        f[40..42].copy_from_slice(&[0, 0]);
        let p = Ipv4Frame::parse(&f).unwrap();
        p.set_src_ip(&mut f, Ipv4Addr::new(192, 168, 1, 50));
        p.fix_checksums(&mut f);
        assert_eq!(f[40..42], [0, 0]);
        assert_eq!(checksum(&f[14..34]), 0);
    }

    #[test]
    fn other_protocols_and_fragments_keep_their_payload() {
        let gre = frame(47, &[1, 2, 3, 4]);
        let mut frag = udp(b"x");
        frag[20] = 0x20; // more fragments
        for mut f in [gre, frag] {
            let p = Ipv4Frame::parse(&f).unwrap();
            assert_eq!(p.l4.src_port(), None);
            assert_eq!(p.l4.dst_port(), None);
            let before = f.clone();
            p.set_src_port(&mut f, 1);
            p.set_dst_port(&mut f, 1);
            assert_eq!(f, before);
            p.fix_checksums(&mut f);
            assert_eq!(f[34..], before[34..]);
            assert_eq!(checksum(&f[14..34]), 0);
        }
    }

    #[test]
    fn rebuild_swaps_the_ethernet_header_and_drops_padding() {
        let mut f = udp(b"x");
        f.extend_from_slice(&[0xee; 8]);
        let p = Ipv4Frame::parse(&f).unwrap();
        let eth = EthHeader {
            dst: MAC_A,
            src: MAC_B,
            ethertype: ETHERTYPE_IPV4,
        };
        let r = p.rebuild(&f, eth);
        assert_eq!(EthHeader::parse(&r), Some(eth));
        assert_eq!(r[14..], f[14..p.end]);
    }

    #[test]
    fn writes_past_the_end_are_dropped() {
        let p = Ipv4Frame::parse(&udp(b"")).unwrap();
        let mut short = [0u8; 10];
        p.set_src_ip(&mut short, IP_A);
        p.set_ttl(&mut short, 1);
        p.set_src_port(&mut short, 1);
        p.fix_checksums(&mut short);
        put_bytes(&mut short, 99, &[1]);
        assert_eq!(short, [0; 10]);
        assert_eq!(chunk::<2>(&short, 9), None);
        assert_eq!(field::<2>(&short, 11), [0, 0]);
    }

    #[test]
    fn truncated_frames_are_rejected_not_panicking() {
        let f = udp(b"x");
        let n = f.len();
        assert_eq!(
            parsing_prefixes(&f, &|b| Ipv4Frame::parse(b).is_some()),
            [n]
        );
        assert_eq!(
            parsing_prefixes(&f[..20], &|b| EthHeader::parse(b).is_some()),
            (14..=20).collect::<Vec<_>>()
        );
        let tcp = frame(IPPROTO_TCP, &[0; 20]);
        let icmp = frame(IPPROTO_ICMP, &[0; 8]);
        for g in [&tcp, &icmp] {
            assert_eq!(
                parsing_prefixes(g, &|b| Ipv4Frame::parse(b).is_some()),
                [g.len()]
            );
        }
        // Too short for the transport header, yet a well-formed IP packet.
        for (proto, len) in [(IPPROTO_TCP, 19), (IPPROTO_UDP, 7), (IPPROTO_ICMP, 7)] {
            assert_eq!(
                Ipv4Frame::parse(&frame(proto, &alloc::vec![0; len])),
                None,
                "{proto}"
            );
        }
    }

    #[test]
    fn bad_ip_headers_are_rejected() {
        let f = udp(b"x");
        let arp = [&f[..12], &[8, 6], &f[14..]].concat();
        let v6 = [&f[..14], &[0x65], &f[15..]].concat();
        let short_ihl = [&f[..14], &[0x44], &f[15..]].concat();
        let mut short_total = f.clone();
        short_total[16..18].copy_from_slice(&19u16.to_be_bytes());
        for bad in [arp, v6, short_ihl, short_total] {
            assert_eq!(Ipv4Frame::parse(&bad), None, "{bad:?}");
        }
    }
}
