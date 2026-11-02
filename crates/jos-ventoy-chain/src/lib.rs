//! Fills `geom_ventoy`'s disk map from the chain blob Ventoy's GRUB builds.
//!
//! Ventoy's normal FreeBSD boot writes the ISO's disk map into
//! `geom_ventoy.ko` on the fly (`vt_unix_ko_fillmap`). Its direct kernel
//! boot (GRUB's `kfreebsd`, see `freebsd/grub`) cannot touch the module,
//! but GRUB can preload the chain data `vt_unix_chain_data` builds as a
//! module of type [`PRELOAD_TYPE`]. This crate turns that blob into the
//! module's `struct g_ventoy_map`: [`fill`] is plain `no_std` Rust, tested
//! on the host, and the `ffi` module (built for `x86_64-unknown-none`
//! only) is the `jos_ventoy_fill_map` entry point `g_ventoy.c` calls, with
//! the kernel's `preload_*` functions behind it. `jos-release` emits the
//! crate as one object and links it into the module (`CHAIN_OBJ` in
//! `freebsd/geom_ventoy/Makefile`).
//!
//! Chain blob layout (Ventoy's `ventoy_chain_head`, `#pragma pack(4)`;
//! with `ventoy_compatible=YES` it is the head and the image chunks only):
//!
//! | offset | size | field                                                  |
//! |-------:|-----:|--------------------------------------------------------|
//! | 0      | 16   | GUID `"  www.ventoy.net"`                              |
//! | 16     | 1    | checksum: the 512 bytes of `ventoy_os_param` sum to 0  |
//! | 17     | 16   | disk GUID                                              |
//! | 33     | 8    | disk size in bytes                                     |
//! | 2592   | 4    | `img_chunk_offset`                                     |
//! | 2596   | 4    | `img_chunk_num`                                        |
//!
//! Each chunk is 24 bytes: `img_start_sector`, `img_end_sector` (u32, 2 KiB
//! sectors of the image), then `disk_start_sector`, `disk_end_sector`
//! (u64, 512-byte sectors, end included). The map wants byte ranges, so a
//! segment is `[disk_start * 512, (disk_end + 1) * 512)`, exactly what
//! Ventoy's `ventoy_unix_fill_map_data` writes.

#![no_std]
#![deny(unsafe_code)]

use core::ffi::CStr;

#[cfg(target_os = "none")]
mod ffi;

/// `MODINFO_TYPE` GRUB tags the blob with (`kfreebsd_module ... type=`).
pub const PRELOAD_TYPE: &CStr = c"ventoy_chain";
/// `VENTOY_GUID`, the first 16 bytes of every Ventoy parameter block.
pub const VENTOY_GUID: [u8; 16] = *b"  www.ventoy.net";
/// `VENTOY_UNIX_SEG_MAGIC0..3`: a map is valid once `magic2` holds them.
pub const MAGIC: [u32; 4] = [0x1122_3344, 0x5566_7788, 0x99aa_bbcc, 0xddee_ff00];
/// `VENTOY_UNIX_MAX_SEGNUM`: the map's capacity.
pub const MAX_SEGMENTS: usize = 40960;
/// Size of `ventoy_os_param`, the checksummed first part of the blob.
pub const OS_PARAM_SIZE: usize = 512;
/// Size of `ventoy_chain_head`.
pub const HEAD_SIZE: usize = 2616;
/// Size of `ventoy_img_chunk`.
pub const CHUNK_SIZE: usize = 24;
/// Disk sectors in the chunks are this big (GRUB's unit).
pub const SECTOR: u64 = 512;

/// `struct g_ventoy_seg`: a byte range of the disk, end excluded.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
}

/// `struct g_ventoy_map` of `freebsd/geom_ventoy/g_ventoy.h`, the module's
/// `g_ventoy_map_data`. Upstream packs it, but every field is naturally
/// aligned so the C layout is the `repr(C)` one (checked below).
#[repr(C)]
pub struct Map {
    pub magic1: [u32; 4],
    pub magic2: [u32; 4],
    pub segnum: u64,
    pub disksize: u64,
    pub diskuuid: [u8; 16],
    pub seglist: [Segment; MAX_SEGMENTS],
    pub magic3: [u32; 4],
}

/// `sizeof(struct g_ventoy_map)`.
pub const MAP_SIZE: usize = 80 + MAX_SEGMENTS * 16;
const _: () = assert!(core::mem::size_of::<Map>() == MAP_SIZE);
const _: () = assert!(core::mem::offset_of!(Map, seglist) == 64);
const _: () = assert!(core::mem::offset_of!(Map, magic3) == MAP_SIZE - 16);

/// Why a blob was refused (the map is left as it was, so still invalid).
///
/// No `Display`: the object must only need symbols the kernel has, and
/// `core::fmt` is not among them. [`Error::message`] is a C string the
/// module can print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Shorter than a chain head, or the chunk list runs past the end.
    Truncated,
    /// Does not start with [`VENTOY_GUID`].
    NotVentoy,
    /// The parameter block's checksum is wrong.
    Checksum,
    /// No chunk at all: nothing to map.
    Empty,
    /// More chunks than [`MAX_SEGMENTS`].
    TooManySegments,
    /// A chunk's disk range does not fit in bytes.
    Overflow,
}

impl Error {
    /// Every variant, in discriminant order.
    pub const ALL: [Error; 6] = [
        Error::Truncated,
        Error::NotVentoy,
        Error::Checksum,
        Error::Empty,
        Error::TooManySegments,
        Error::Overflow,
    ];

    /// What `jos_ventoy_fill_map` returns for this error: a negative
    /// number, distinct per variant (0 and positive values are counts).
    pub fn code(self) -> i32 {
        -1 - self as i32
    }

    /// The error for a [`code`](Error::code); `None` for a count.
    pub fn from_code(code: i32) -> Option<Error> {
        usize::try_from(-1 - i64::from(code))
            .ok()
            .and_then(|i| Error::ALL.get(i).copied())
    }

    pub fn message(self) -> &'static CStr {
        match self {
            Error::Truncated => c"chain blob truncated",
            Error::NotVentoy => c"no Ventoy GUID at the start of the chain blob",
            Error::Checksum => c"chain blob checksum mismatch",
            Error::Empty => c"chain blob maps no disk range",
            Error::TooManySegments => c"more disk ranges than the map holds",
            Error::Overflow => c"disk range does not fit in bytes",
        }
    }
}

/// The little-endian integer of `n` bytes at `at`; bytes past the end of
/// `b` read as 0 (callers bound what they read first).
fn le(b: &[u8], at: usize, n: usize) -> u64 {
    b.iter()
        .skip(at)
        .take(n)
        .rev()
        .fold(0, |v, &c| v << 8 | u64::from(c))
}

/// Fill `map` from the chain blob: its magics, disk size and GUID, and one
/// segment per image chunk. Returns the number of segments. On an error
/// nothing identifies the map as valid (`magic2` is written last).
pub fn fill(blob: &[u8], map: &mut Map) -> Result<usize, Error> {
    if blob.len() < HEAD_SIZE {
        return Err(Error::Truncated);
    }
    if blob.get(..VENTOY_GUID.len()) != Some(VENTOY_GUID.as_slice()) {
        return Err(Error::NotVentoy);
    }
    let sum = blob
        .iter()
        .take(OS_PARAM_SIZE)
        .fold(0u8, |sum, &c| sum.wrapping_add(c));
    if sum != 0 {
        return Err(Error::Checksum);
    }
    let offset = le(blob, 2592, 4) as usize;
    let num = le(blob, 2596, 4) as usize;
    if num == 0 {
        return Err(Error::Empty);
    }
    if num > MAX_SEGMENTS {
        return Err(Error::TooManySegments);
    }
    // Both bounded above: no overflow.
    let (chunks, _) = blob
        .get(offset..offset + num * CHUNK_SIZE)
        .ok_or(Error::Truncated)?
        .as_chunks::<CHUNK_SIZE>();
    for (seg, chunk) in map.seglist.iter_mut().zip(chunks) {
        let start = le(chunk, 8, 8);
        let end = le(chunk, 16, 8);
        seg.start = start.checked_mul(SECTOR).ok_or(Error::Overflow)?;
        seg.end = end
            .checked_add(1)
            .and_then(|e| e.checked_mul(SECTOR))
            .ok_or(Error::Overflow)?;
    }
    map.segnum = num as u64;
    map.disksize = le(blob, 33, 8);
    for (d, s) in map.diskuuid.iter_mut().zip(blob.iter().skip(17)) {
        *d = *s;
    }
    map.magic1 = MAGIC;
    map.magic2 = MAGIC;
    Ok(num)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::boxed::Box;
    use std::vec::Vec;

    /// A map as the module has it at load time (no magic in `magic2`).
    fn map() -> Box<Map> {
        Box::new(Map {
            magic1: [0; 4],
            magic2: [0; 4],
            segnum: 0,
            disksize: 0,
            diskuuid: [0; 16],
            seglist: [Segment { start: 0, end: 0 }; MAX_SEGMENTS],
            magic3: MAGIC,
        })
    }

    /// What `vt_unix_chain_data` leaves in memory for these chunks
    /// (`disk_start_sector`, `disk_end_sector` pairs), the head after it
    /// padded by `trailing` bytes.
    fn blob(disk: &[u8; 16], size: u64, chunks: &[(u64, u64)], trailing: usize) -> Vec<u8> {
        let mut b = std::vec![0u8; HEAD_SIZE];
        b[..16].copy_from_slice(&VENTOY_GUID);
        b[17..33].copy_from_slice(disk);
        b[33..41].copy_from_slice(&size.to_le_bytes());
        let sum = b[..OS_PARAM_SIZE]
            .iter()
            .fold(0u8, |s, &c| s.wrapping_add(c));
        b[16] = 0u8.wrapping_sub(sum);
        b.resize(HEAD_SIZE + trailing, 0);
        b[2592..2596].copy_from_slice(&((HEAD_SIZE + trailing) as u32).to_le_bytes());
        b[2596..2600].copy_from_slice(&(chunks.len() as u32).to_le_bytes());
        for (i, (start, end)) in chunks.iter().enumerate() {
            b.extend_from_slice(&(i as u32 * 8).to_le_bytes());
            b.extend_from_slice(&(i as u32 * 8 + 7).to_le_bytes());
            b.extend_from_slice(&start.to_le_bytes());
            b.extend_from_slice(&end.to_le_bytes());
        }
        b
    }

    const DISK: [u8; 16] = [
        0x6a, 0x6f, 0x73, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
        0x0c,
    ];

    #[test]
    fn a_chain_blob_fills_the_map() {
        let mut m = map();
        let b = blob(&DISK, 31_000_000_000, &[(2048, 4095), (8192, 8192)], 0);
        assert_eq!(fill(&b, &mut m), Ok(2));
        assert_eq!((m.magic1, m.magic2, m.magic3), (MAGIC, MAGIC, MAGIC));
        assert_eq!(m.segnum, 2);
        assert_eq!(m.disksize, 31_000_000_000);
        assert_eq!(m.diskuuid, DISK);
        assert_eq!(
            m.seglist[..3],
            [
                Segment {
                    start: 2048 * 512,
                    end: 4096 * 512
                },
                Segment {
                    start: 8192 * 512,
                    end: 8193 * 512
                },
                Segment { start: 0, end: 0 },
            ]
        );

        // The chunk list may sit anywhere after the head, and the blob may
        // go on after it; a full map is fine.
        let many: Vec<(u64, u64)> = (0..MAX_SEGMENTS as u64).map(|i| (i, i)).collect();
        let mut b = blob(&DISK, 1, &many, 4000);
        b.extend_from_slice(b"padding");
        assert_eq!(fill(&b, &mut m), Ok(MAX_SEGMENTS));
        assert_eq!(m.seglist[MAX_SEGMENTS - 1].end, MAX_SEGMENTS as u64 * 512);
    }

    #[test]
    fn the_layout_matches_the_c_struct() {
        use core::mem::{offset_of, size_of};
        assert_eq!(size_of::<Map>(), 655_440);
        assert_eq!(offset_of!(Map, magic2), 16);
        assert_eq!(offset_of!(Map, segnum), 32);
        assert_eq!(offset_of!(Map, disksize), 40);
        assert_eq!(offset_of!(Map, diskuuid), 48);
        assert_eq!(size_of::<Segment>(), 16);
        assert_eq!(offset_of!(Segment, end), 8);
        assert_eq!(PRELOAD_TYPE.to_bytes(), b"ventoy_chain");
    }

    #[test]
    fn malformed_blobs_are_refused_and_leave_the_map_invalid() {
        let good = blob(&DISK, 1, &[(1, 1)], 0);
        let mut not_ventoy = good.clone();
        not_ventoy[0] = b'x';
        let mut bad_sum = good.clone();
        bad_sum[100] ^= 1;
        let mut too_many = blob(&DISK, 1, &[(1, 1)], 0);
        too_many[2596..2600].copy_from_slice(&(MAX_SEGMENTS as u32 + 1).to_le_bytes());
        let cases: [(&[u8], Error); 8] = [
            (&[], Error::Truncated),
            (&good[..HEAD_SIZE - 1], Error::Truncated),
            (&good[..good.len() - 1], Error::Truncated),
            (&not_ventoy, Error::NotVentoy),
            (&bad_sum, Error::Checksum),
            (&blob(&DISK, 1, &[], 0), Error::Empty),
            (&too_many, Error::TooManySegments),
            (&blob(&DISK, 1, &[(u64::MAX, 1)], 0), Error::Overflow),
        ];
        for (b, want) in cases {
            let mut m = map();
            assert_eq!(fill(b, &mut m), Err(want));
            assert_eq!(m.magic2, [0; 4], "{want:?}");
            assert_eq!(m.segnum, 0, "{want:?}");
        }
        // Both ways an end sector can overflow.
        for end in [u64::MAX, u64::MAX / 512] {
            assert_eq!(
                fill(&blob(&DISK, 1, &[(0, end)], 0), &mut map()),
                Err(Error::Overflow)
            );
        }
    }

    #[test]
    fn errors_have_distinct_codes_and_messages() {
        let codes: Vec<i32> = Error::ALL.iter().map(|e| e.code()).collect();
        assert!(codes.iter().all(|&c| c < 0));
        let messages: Vec<&CStr> = Error::ALL.iter().map(|e| e.message()).collect();
        for (i, (code, msg)) in codes.iter().zip(&messages).enumerate() {
            assert!(!codes[..i].contains(code), "{code}");
            assert!(!messages[..i].contains(msg), "{msg:?}");
            assert_eq!(Error::from_code(*code), Some(Error::ALL[i]));
        }
        assert_eq!(Error::Truncated.code(), -1);
        assert_eq!(Error::from_code(0), None);
        assert_eq!(Error::from_code(3), None);
        assert_eq!(Error::from_code(-7), None);
        assert_eq!(Error::from_code(i32::MIN), None);
    }
}
