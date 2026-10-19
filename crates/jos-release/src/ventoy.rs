//! `geom_ventoy.ko` for the live ISO.
//!
//! Ventoy boots a FreeBSD ISO by injecting a prebuilt `geom_ventoy.ko` for
//! the ISO's FreeBSD version; a Ventoy older than the FreeBSD release has
//! none, the image never appears as a disk and the root mount fails with
//! error 19. When the ISO ships `/boot/kernel/geom_ventoy.ko` itself,
//! every Ventoy since 1.0.62 skips the injection and instead writes the
//! image's disk map straight into that file (`vt_unix_ko_fillmap`): it
//! looks for the map's magic at 64 KiB steps of the file.
//!
//! The vendored source (`freebsd/geom_ventoy`) puts the map in its own
//! section aligned to 64 KiB so `ld -r` places it at such an offset. The
//! kernel's object linker only page-aligns a module's mapping, so a 64 KiB
//! section alignment would place the section past the allocation (a page
//! fault in `link_elf_load_file`); [`patch_module`] lowers the recorded
//! alignment to a page once the file layout is fixed.

use anyhow::{anyhow, bail};

/// Section holding `g_ventoy_map_data` (see `freebsd/geom_ventoy/g_ventoy.c`).
pub const MAP_SECTION: &str = ".data.ventoy_map";
/// Step of Ventoy's search for the map in the module file.
pub const FILLMAP_STEP: u64 = 65536;
/// `VENTOY_UNIX_SEG_MAGIC0..3`, little endian.
pub const MAP_MAGIC: [u32; 4] = [0x1122_3344, 0x5566_7788, 0x99aa_bbcc, 0xddee_ff00];
/// Alignment the kernel linker honours.
pub const PAGE: u64 = 4096;

/// A file offset as an index (past any slice when it does not fit).
fn idx(at: u64) -> usize {
    usize::try_from(at).unwrap_or(usize::MAX)
}

/// The little-endian integer of `n` bytes at `at`; bytes past the end of
/// `b` read as 0 (callers bound what they read first).
fn le(b: &[u8], at: u64, n: usize) -> u64 {
    b.iter()
        .skip(idx(at))
        .take(n)
        .rev()
        .fold(0, |v, &c| v << 8 | u64::from(c))
}

/// A section header of interest: where its fields live in the file.
#[derive(Debug, PartialEq, Eq)]
struct Section {
    name: String,
    offset: u64,
    size: u64,
    /// File offset of `sh_addralign`.
    align_field: u64,
    align: u64,
}

/// Section headers of a little-endian ELF64 relocatable object.
fn sections(b: &[u8]) -> anyhow::Result<Vec<Section>> {
    if b.get(..4) != Some(b"\x7fELF".as_slice()) {
        bail!("not an ELF file");
    }
    // EI_CLASS 2 = ELF64, EI_DATA 1 = little endian.
    if b.get(4..6) != Some([2, 1].as_slice()) {
        bail!("not a little-endian ELF64 file");
    }
    let shoff = le(b, 0x28, 8);
    let shentsize = le(b, 0x3a, 2);
    let shnum = le(b, 0x3c, 2);
    let shstrndx = le(b, 0x3e, 2);
    if shentsize < 64 {
        bail!("bad section header size {shentsize}");
    }
    // Every header read below is inside the file (and cannot overflow).
    if shoff.saturating_add(shnum * shentsize) > b.len() as u64 {
        bail!("truncated ELF file");
    }
    let header = |i: u64| shoff + i * shentsize;
    let strtab = le(b, header(shstrndx) + 0x18, 8);
    let mut out = Vec::new();
    for i in 0..shnum {
        let h = header(i);
        let tail = b
            .get(idx(strtab.saturating_add(le(b, h, 4)))..)
            .unwrap_or_default();
        let len = tail
            .iter()
            .position(|&c| c == 0)
            .ok_or_else(|| anyhow!("section name not terminated in the file"))?;
        out.push(Section {
            name: String::from_utf8_lossy(tail.get(..len).unwrap_or_default()).into_owned(),
            offset: le(b, h + 0x18, 8),
            size: le(b, h + 0x20, 8),
            align_field: h + 0x30,
            align: le(b, h + 0x30, 8),
        });
    }
    Ok(out)
}

/// Check that Ventoy will find the disk map in this module and make the
/// module loadable: the map section must start on a [`FILLMAP_STEP`]
/// boundary with the magic in place (otherwise Ventoy silently fills
/// nothing and the ISO does not boot), and its alignment is lowered to
/// [`PAGE`]. Returns the map's file offset.
pub fn patch_module(b: &mut [u8]) -> anyhow::Result<u64> {
    let map = sections(b)?
        .into_iter()
        .find(|s| s.name == MAP_SECTION)
        .ok_or_else(|| anyhow!("no {MAP_SECTION} section"))?;
    if map.offset % FILLMAP_STEP != 0 {
        bail!(
            "{MAP_SECTION} at file offset {:#x}, not a multiple of {FILLMAP_STEP:#x}: \
             Ventoy would not find it",
            map.offset
        );
    }
    // Past the end of the file the magic reads as zeros.
    let magic: Vec<u64> = (0..4)
        .map(|i| le(b, map.offset.saturating_add(4 * i), 4))
        .collect();
    if magic != MAP_MAGIC.map(u64::from) || map.size < 16 {
        bail!("{MAP_SECTION} does not start with the Ventoy map magic");
    }
    if map.align > PAGE {
        // Inside the section header table, which is inside the file.
        for (d, s) in b
            .iter_mut()
            .skip(idx(map.align_field))
            .zip(PAGE.to_le_bytes())
        {
            *d = s;
        }
    }
    Ok(map.offset)
}

/// The line the live `loader.conf` gets when the module is shipped.
pub fn loader_conf_lines() -> String {
    "# Ventoy: map the ISO file on a Ventoy stick to a disk (no-op elsewhere)\n\
     geom_ventoy_load=\"YES\"\n"
        .to_string()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A minimal ELF64 object: null section, `.data`, the map section at
    /// `map_offset` with `align`, and the section name table.
    pub fn fake_module(map_offset: u64, align: u64, magic: bool) -> Vec<u8> {
        let names = b"\0.data\0.data.ventoy_map\0.shstrtab\0";
        let map_size = 0x50u64;
        let names_at = map_offset + map_size;
        let shoff = (names_at + names.len() as u64 + 7) & !7;
        let mut b = vec![0u8; (shoff + 4 * 64) as usize];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        b[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        b[0x3c..0x3e].copy_from_slice(&4u16.to_le_bytes());
        b[0x3e..0x40].copy_from_slice(&3u16.to_le_bytes());
        let mut sh = |i: u64, name: u32, off: u64, size: u64, align: u64| {
            let h = (shoff + i * 64) as usize;
            b[h..h + 4].copy_from_slice(&name.to_le_bytes());
            b[h + 0x18..h + 0x20].copy_from_slice(&off.to_le_bytes());
            b[h + 0x20..h + 0x28].copy_from_slice(&size.to_le_bytes());
            b[h + 0x30..h + 0x38].copy_from_slice(&align.to_le_bytes());
        };
        sh(1, 1, 0x40, 0x10, 16);
        sh(2, 7, map_offset, map_size, align);
        sh(3, 24, names_at, names.len() as u64, 1);
        let n = names_at as usize;
        b[n..n + names.len()].copy_from_slice(names);
        if magic {
            for (i, m) in MAP_MAGIC.iter().enumerate() {
                let at = map_offset as usize + 4 * i;
                b[at..at + 4].copy_from_slice(&m.to_le_bytes());
            }
        }
        b
    }

    #[test]
    fn the_map_is_found_and_its_alignment_lowered() {
        let mut b = fake_module(0x10000, 65536, true);
        assert_eq!(patch_module(&mut b).unwrap(), 0x10000);
        let map = sections(&b)
            .unwrap()
            .into_iter()
            .find(|s| s.name == MAP_SECTION)
            .unwrap();
        assert_eq!(map.align, PAGE);
        // Already loadable: untouched, still accepted.
        let before = b.clone();
        assert_eq!(patch_module(&mut b).unwrap(), 0x10000);
        assert_eq!(b, before);
    }

    #[test]
    fn modules_ventoy_cannot_fill_are_refused() {
        let err = patch_module(&mut fake_module(0x11000, 4096, true)).unwrap_err();
        assert!(err.to_string().contains("not a multiple"), "{err}");
        let err = patch_module(&mut fake_module(0x10000, 65536, false)).unwrap_err();
        assert!(err.to_string().contains("magic"), "{err}");

        let mut renamed = fake_module(0x10000, 65536, true);
        let at = renamed
            .windows(MAP_SECTION.len())
            .position(|w| w == MAP_SECTION.as_bytes())
            .unwrap();
        renamed[at + 1] = b'X';
        let err = patch_module(&mut renamed).unwrap_err();
        assert!(err.to_string().contains("no .data.ventoy_map"), "{err}");
    }

    #[test]
    fn malformed_files_are_errors_not_panics() {
        assert!(patch_module(&mut b"MZ not elf".to_vec()).is_err());
        let mut b32 = fake_module(0x10000, 65536, true);
        b32[4] = 1;
        assert!(patch_module(&mut b32)
            .unwrap_err()
            .to_string()
            .contains("ELF64"));
        let full = fake_module(0x10000, 65536, true);
        for cut in [0x30, 0x3e, 0x10008, full.len() - 40] {
            assert!(
                patch_module(&mut full[..cut].to_vec()).is_err(),
                "cut {cut:#x}"
            );
        }
        let mut small = full.clone();
        small[0x3a..0x3c].copy_from_slice(&8u16.to_le_bytes());
        assert!(patch_module(&mut small).is_err());
        let mut unterminated = full.clone();
        let len = unterminated.len();
        let shoff = u64::from_le_bytes(unterminated[0x28..0x30].try_into().unwrap()) as usize;
        // Point a section name at the last byte of the file (non-zero).
        unterminated[shoff + 64..shoff + 68]
            .copy_from_slice(&((len - 1 - (0x10000 + 0x50)) as u32).to_le_bytes());
        unterminated[len - 1] = b'x';
        assert!(patch_module(&mut unterminated).is_err());
    }

    #[test]
    fn loader_conf_loads_the_module() {
        assert!(loader_conf_lines().contains("geom_ventoy_load=\"YES\""));
    }
}
