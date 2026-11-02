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
//!
//! Ventoy's direct kernel boot (GRUB, `freebsd/grub`) cannot fill the
//! module; the module reads the chain blob GRUB preloads instead, through
//! the `jos-ventoy-chain` crate linked in as one object. [`check_object`]
//! makes sure that object needs nothing the kernel does not have: an
//! undefined symbol would make the module unloadable, and the normal
//! Ventoy boot would break along with it.
//!
//! That boot is offered by a `.vcfg` next to the ISO on the stick
//! ([`vcfg`]), which runs the GRUB script the ISO carries
//! ([`direct_cfg`]).

use anyhow::{anyhow, bail};
use juanitos_core::dom0;

use crate::builder::VENTOY_MODULE;

/// Section holding `g_ventoy_map_data` (see `freebsd/geom_ventoy/g_ventoy.c`).
pub const MAP_SECTION: &str = ".data.ventoy_map";
/// Step of Ventoy's search for the map in the module file.
pub const FILLMAP_STEP: u64 = 65536;
/// `VENTOY_UNIX_SEG_MAGIC0..3`, little endian.
pub const MAP_MAGIC: [u32; 4] = [0x1122_3344, 0x5566_7788, 0x99aa_bbcc, 0xddee_ff00];
/// Alignment the kernel linker honours.
pub const PAGE: u64 = 4096;
/// The entry point `g_ventoy.c` calls in the chain blob reader.
pub const CHAIN_ENTRY: &str = "jos_ventoy_fill_map";
/// What the kernel provides to the chain blob reader: the `preload_*`
/// functions and `panic` it declares, and the memory routines rustc may
/// call for copies and comparisons.
pub const KERNEL_SYMBOLS: &[&str] = &[
    "preload_search_by_type",
    "preload_fetch_addr",
    "preload_fetch_size",
    "panic",
    "memcpy",
    "memmove",
    "memset",
    "memcmp",
    "bcmp",
];

/// `SHT_SYMTAB`.
const SYMTAB: u64 = 2;
/// `STB_GLOBAL` and `STB_WEAK`: the bindings the kernel linker resolves
/// against other files (locals never leave the object).
const GLOBAL: u64 = 1;
const WEAK: u64 = 2;

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

/// The NUL-terminated string at `at`.
fn name(b: &[u8], at: u64) -> anyhow::Result<String> {
    let tail = b.get(idx(at)..).unwrap_or_default();
    let len = tail
        .iter()
        .position(|&c| c == 0)
        .ok_or_else(|| anyhow!("name not terminated in the file"))?;
    Ok(String::from_utf8_lossy(tail.get(..len).unwrap_or_default()).into_owned())
}

/// A section header of interest: where its fields live in the file.
#[derive(Debug, PartialEq, Eq)]
struct Section {
    name: String,
    kind: u64,
    offset: u64,
    size: u64,
    /// `sh_link`: for a symbol table, its string table.
    link: u64,
    entsize: u64,
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
        out.push(Section {
            name: name(b, strtab.saturating_add(le(b, h, 4)))?,
            kind: le(b, h + 4, 4),
            offset: le(b, h + 0x18, 8),
            size: le(b, h + 0x20, 8),
            link: le(b, h + 0x28, 4),
            entsize: le(b, h + 0x38, 8),
            align_field: h + 0x30,
            align: le(b, h + 0x30, 8),
        });
    }
    Ok(out)
}

/// The names of the object's global (or weak) symbols: the defined ones
/// and the undefined ones.
fn global_symbols(b: &[u8]) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let sections = sections(b)?;
    let symtab = sections
        .iter()
        .find(|s| s.kind == SYMTAB)
        .ok_or_else(|| anyhow!("no symbol table"))?;
    let strtab = sections
        .get(idx(symtab.link))
        .ok_or_else(|| anyhow!("the symbol table names no string table"))?;
    if symtab.entsize < 24 {
        bail!("bad symbol entry size {}", symtab.entsize);
    }
    if symtab.offset.saturating_add(symtab.size) > b.len() as u64 {
        bail!("truncated symbol table");
    }
    let mut defined = Vec::new();
    let mut undefined = Vec::new();
    // Entry 0 is the null symbol.
    for i in 1..symtab.size / symtab.entsize {
        let at = symtab.offset + i * symtab.entsize;
        if !matches!(le(b, at + 4, 1) >> 4, GLOBAL | WEAK) {
            continue;
        }
        let name = name(b, strtab.offset.saturating_add(le(b, at, 4)))?;
        // `st_shndx` 0 is SHN_UNDEF.
        if le(b, at + 6, 2) == 0 {
            undefined.push(name);
        } else {
            defined.push(name);
        }
    }
    Ok((defined, undefined))
}

/// Check that the chain blob reader (the `jos-ventoy-chain` crate emitted
/// as one object) defines [`CHAIN_ENTRY`] and needs nothing beyond
/// [`KERNEL_SYMBOLS`], so `geom_ventoy.ko` stays loadable with it.
pub fn check_object(b: &[u8]) -> anyhow::Result<()> {
    let (defined, undefined) = global_symbols(b)?;
    if !defined.iter().any(|s| s == CHAIN_ENTRY) {
        bail!("{CHAIN_ENTRY} is not defined");
    }
    let unknown: Vec<&str> = undefined
        .iter()
        .map(String::as_str)
        .filter(|s| !KERNEL_SYMBOLS.contains(s))
        .collect();
    if !unknown.is_empty() {
        bail!("the kernel has no {}", unknown.join(", "));
    }
    Ok(())
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

/// The GRUB script of Ventoy's direct kernel boot on the live ISO, run by
/// the [`vcfg`] with the ISO as `(loop)` and its path in `$jos_iso`.
pub const DIRECT_CFG: &str = "/boot/grub/juanitos-ventoy.cfg";

/// [`DIRECT_CFG`] for the ISO labelled `label`: what FreeBSD's loader does,
/// in GRUB's words, plus two things it does not need to. Ventoy's chain
/// blob for the ISO is preloaded, with the type `jos-ventoy-chain` looks
/// up, since nothing fills `geom_ventoy.ko`'s map on this path; and the
/// root is named, nothing reading `/etc/fstab`. `jos_efi_meta` comes
/// after the kernel and its environment and before any module: the kernel
/// reads the metadata in load order. `bootwatch`: the ISO ships
/// [`dom0::BOOTWATCH_MODULE`].
pub fn direct_cfg(label: &str, bootwatch: bool) -> String {
    let mut modules: Vec<String> = dom0::KERNEL_MODULES
        .iter()
        .chain([&VENTOY_MODULE])
        .map(|m| format!("/boot/kernel/{m}.ko"))
        .collect();
    if bootwatch {
        modules.push(format!("/boot/modules/{}.ko", dom0::BOOTWATCH_MODULE));
    }
    let mut s = format!(
        "# JuanitOS: Ventoy's direct kernel boot, run by the .vcfg next to the ISO
# (generated by jos-release, see docs/installer.md).
insmod bsd
set root=(loop)
insmod {grub_module}
# Where the ISO lies on the stick, for geom_ventoy.ko.
vt_unix_reset
vt_img_sector \"$jos_iso\"
set ventoy_compatible=YES
vt_unix_chain_data \"$jos_iso\"
kfreebsd /boot/kernel/kernel
kfreebsd_loadenv /boot/device.hints
kfreebsd_loadenv /boot/loader.conf
set kFreeBSD.vfs.root.mountfrom=cd9660:{root}
terminal_output console
jos_efi_meta
",
        grub_module = dom0::GRUB_MODULE_PATH,
        root = dom0::live_root_device(label),
    );
    for m in modules {
        s.push_str(&format!("kfreebsd_module_elf {m}\n"));
    }
    s.push_str(&format!(
        "kfreebsd_module \"mem:${{vtoy_chain_mem_addr}}:size:${{vtoy_chain_mem_size}}\" \
         type={}\nboot\n",
        jos_ventoy_chain::PRELOAD_TYPE.to_string_lossy()
    ));
    s
}

/// The `<iso>.vcfg` Ventoy runs instead of its own boot when it finds it
/// next to the ISO (its "custom boot menu"). The first entry is Ventoy's
/// normal boot: the steps of `iso_common_menuentry` in Ventoy's
/// `grub.cfg` after the menu, calling its functions. The second, on
/// 64-bit UEFI only (`jos_efi.mod` is an `x86_64-efi` module), is the
/// direct kernel boot, the default in Ventoy's GRUB2 mode.
pub fn vcfg() -> String {
    format!(
        "# JuanitOS on a Ventoy stick: copy this file next to the ISO, named
# like it plus .vcfg (generated by jos-release, see docs/installer.md).
menuentry \"JuanitOS\" --class=custom {{
    set vt_chosen_path=\"$vtoy_chosen_path\"
    set vt_chosen_name=\"$vtoy_chosen_file\"
    vt_parse_iso_volume \"$vtoy_iso_part$vt_chosen_path\" vt_system_id vt_volume_id vt_volume_space
    set ventoy_fs_probe=iso9660
    ventoy_reset_nojoliet
    {unloop}
    loopback loop \"$vtoy_iso_part$vt_chosen_path\"
    get_os_type (loop)
    if [ \"$grub_platform\" = \"pc\" ]; then
        legacy_iso_menu_func \"$vtoy_iso_part\" \"$vt_chosen_path\"
    else
        uefi_iso_menu_func \"$vtoy_iso_part\" \"$vt_chosen_path\"
    fi
}}
if [ \"$grub_platform\" = \"efi\" -a \"$VTOY_EFI_ARCH\" = \"x64\" ]; then
    menuentry \"JuanitOS, direct kernel boot (64-bit UEFI)\" --class=custom {{
        set jos_iso=\"$vtoy_iso_part$vtoy_chosen_path\"
        export jos_iso
        {unloop}
        loopback loop \"$jos_iso\"
        configfile (loop){DIRECT_CFG}
    }}
    if vt_check_mode 3 \"$vtoy_chosen_file\"; then
        set default=1
    fi
fi
",
        unloop = "if [ -d (loop)/ ]; then loopback -d loop; fi",
    )
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

    /// Where `needle` starts in `s` (failing the test when it is absent).
    fn at(s: &str, needle: &str) -> usize {
        s.find(needle).unwrap()
    }

    #[test]
    fn the_direct_boot_loads_what_the_loader_would() {
        let alice = direct_cfg("ALICE", true);
        let lines: Vec<&str> = alice.lines().collect();
        for line in [
            "insmod bsd",
            "set root=(loop)",
            "insmod /boot/grub/x86_64-efi/jos_efi.mod",
            "kfreebsd_loadenv /boot/device.hints",
            "set kFreeBSD.vfs.root.mountfrom=cd9660:/dev/iso9660/ALICE",
            "kfreebsd_module_elf /boot/kernel/vmm.ko",
            "kfreebsd_module_elf /boot/kernel/nmdm.ko",
            "kfreebsd_module_elf /boot/kernel/if_bridge.ko",
            "kfreebsd_module_elf /boot/modules/jos_bootwatch.ko",
            "set ventoy_compatible=YES",
        ] {
            assert!(lines.contains(&line), "no {line:?} in:\n{alice}");
        }
        // The chain blob is computed before anything is loaded, the UEFI
        // metadata comes between the kernel's environment and the modules,
        // and the blob is preloaded last, as the type the module looks up.
        let order = [
            "vt_unix_reset\n",
            "vt_img_sector \"$jos_iso\"\n",
            "vt_unix_chain_data \"$jos_iso\"\n",
            "kfreebsd /boot/kernel/kernel\n",
            "kfreebsd_loadenv /boot/loader.conf\n",
            "terminal_output console\n",
            "jos_efi_meta\n",
            "kfreebsd_module_elf /boot/kernel/vmm.ko\n",
            "kfreebsd_module_elf /boot/kernel/geom_ventoy.ko\n",
            "kfreebsd_module \"mem:${vtoy_chain_mem_addr}:size:${vtoy_chain_mem_size}\" \
             type=ventoy_chain\nboot\n",
        ];
        let starts: Vec<usize> = order.iter().map(|n| at(&alice, n)).collect();
        assert!(starts.is_sorted(), "{order:?} out of order in:\n{alice}");
        assert!(alice.ends_with("boot\n"));

        let bob = direct_cfg("BOB", false);
        assert!(!bob.contains("jos_bootwatch"), "{bob}");
        assert!(bob.contains("cd9660:/dev/iso9660/BOB\n"), "{bob}");
    }

    #[test]
    fn the_vcfg_offers_ventoys_boot_then_the_direct_one() {
        let v = vcfg();
        let normal = at(&v, "menuentry \"JuanitOS\" --class=custom {\n");
        let guard = at(
            &v,
            "if [ \"$grub_platform\" = \"efi\" -a \"$VTOY_EFI_ARCH\" = \"x64\" ]; then\n",
        );
        let direct = at(
            &v,
            "menuentry \"JuanitOS, direct kernel boot (64-bit UEFI)\"",
        );
        let default = at(
            &v,
            "if vt_check_mode 3 \"$vtoy_chosen_file\"; then\n        set default=1\n",
        );
        assert!(normal < guard && guard < direct && direct < default, "{v}");

        // Ventoy's normal boot: its own functions, on either platform.
        for call in [
            "vt_parse_iso_volume \"$vtoy_iso_part$vt_chosen_path\" vt_system_id vt_volume_id vt_volume_space\n",
            "loopback loop \"$vtoy_iso_part$vt_chosen_path\"\n",
            "get_os_type (loop)\n",
            "legacy_iso_menu_func \"$vtoy_iso_part\" \"$vt_chosen_path\"\n",
            "uefi_iso_menu_func \"$vtoy_iso_part\" \"$vt_chosen_path\"\n",
        ] {
            assert!(at(&v, call) < guard, "{call:?}");
        }
        // The direct boot hands the ISO, as (loop) and by path, to the
        // script the ISO carries.
        let tail = v.get(direct..).unwrap();
        let iso = at(
            tail,
            "set jos_iso=\"$vtoy_iso_part$vtoy_chosen_path\"\n        export jos_iso\n",
        );
        let unloop = at(tail, "if [ -d (loop)/ ]; then loopback -d loop; fi\n");
        let loop_ = at(tail, "loopback loop \"$jos_iso\"\n");
        let run = at(tail, &format!("configfile (loop){DIRECT_CFG}\n"));
        assert!(iso < unloop && unloop < loop_ && loop_ < run, "{tail}");
        assert!(direct_cfg("ALICE", false).contains("\"$jos_iso\""));

        // Every block is closed.
        assert_eq!(v.matches('{').count(), v.matches('}').count());
        assert_eq!(v.matches("; then").count(), v.matches("fi\n").count());
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

    /// A minimal ELF64 object with a symbol table: a local symbol, then
    /// the `defined` globals (in section 1) and the `undefined` ones.
    pub fn fake_object(defined: &[&str], undefined: &[&str]) -> Vec<u8> {
        let shnames = b"\0.symtab\0.strtab\0.shstrtab\0";
        let mut names = b"\0local\0".to_vec();
        let mut syms = vec![0u8; 24];
        let mut sym = |name: &str, info: u8, shndx: u16| {
            let at = names.len() as u32;
            names.extend_from_slice(name.as_bytes());
            names.push(0);
            let mut s = vec![0u8; 24];
            s[..4].copy_from_slice(&at.to_le_bytes());
            s[4] = info;
            s[6..8].copy_from_slice(&shndx.to_le_bytes());
            syms.extend_from_slice(&s);
        };
        sym("local", 0x02, 1);
        for d in defined {
            sym(d, 0x12, 1);
        }
        for (i, u) in undefined.iter().enumerate() {
            // Weak and global references alike.
            sym(u, if i % 2 == 0 { 0x10 } else { 0x20 }, 0);
        }
        let syms_at = 0x40u64;
        let names_at = syms_at + syms.len() as u64;
        let shnames_at = names_at + names.len() as u64;
        let shoff = (shnames_at + shnames.len() as u64 + 7) & !7;
        let mut b = vec![0u8; (shoff + 4 * 64) as usize];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        b[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        b[0x3c..0x3e].copy_from_slice(&4u16.to_le_bytes());
        b[0x3e..0x40].copy_from_slice(&3u16.to_le_bytes());
        let mut sh = |i: u64, name: u32, kind: u32, off: u64, size: u64, link: u32, ent: u64| {
            let h = (shoff + i * 64) as usize;
            b[h..h + 4].copy_from_slice(&name.to_le_bytes());
            b[h + 4..h + 8].copy_from_slice(&kind.to_le_bytes());
            b[h + 0x18..h + 0x20].copy_from_slice(&off.to_le_bytes());
            b[h + 0x20..h + 0x28].copy_from_slice(&size.to_le_bytes());
            b[h + 0x28..h + 0x2c].copy_from_slice(&link.to_le_bytes());
            b[h + 0x38..h + 0x40].copy_from_slice(&ent.to_le_bytes());
        };
        sh(1, 1, 2, syms_at, syms.len() as u64, 2, 24);
        sh(2, 9, 3, names_at, names.len() as u64, 0, 0);
        sh(3, 17, 3, shnames_at, shnames.len() as u64, 0, 0);
        let (s, n, h) = (syms_at as usize, names_at as usize, shnames_at as usize);
        b[s..s + syms.len()].copy_from_slice(&syms);
        b[n..n + names.len()].copy_from_slice(&names);
        b[h..h + shnames.len()].copy_from_slice(shnames);
        b
    }

    #[test]
    fn the_chain_object_only_needs_what_the_kernel_has() {
        let (defined, undefined) =
            global_symbols(&fake_object(&[CHAIN_ENTRY], &["memcpy", "panic"])).unwrap();
        assert_eq!(defined, [CHAIN_ENTRY]);
        assert_eq!(undefined, ["memcpy", "panic"]);
        check_object(&fake_object(&[CHAIN_ENTRY, "alice"], KERNEL_SYMBOLS)).unwrap();

        let err = check_object(&fake_object(&["alice"], &[])).unwrap_err();
        assert!(err
            .to_string()
            .contains("jos_ventoy_fill_map is not defined"));
        let err = check_object(&fake_object(
            &[CHAIN_ENTRY],
            &["memcpy", "rust_eh_personality", "chuck"],
        ))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "the kernel has no rust_eh_personality, chuck"
        );
    }

    #[test]
    fn malformed_objects_are_errors_not_panics() {
        let good = fake_object(&[CHAIN_ENTRY], &["panic"]);
        let symtab = u64::from_le_bytes(good[0x28..0x30].try_into().unwrap()) as usize + 64;
        let patched = |at: usize, bytes: &[u8]| {
            let mut b = good.clone();
            b[at..at + bytes.len()].copy_from_slice(bytes);
            b
        };
        let cases = [
            (b"MZ".to_vec(), "not an ELF file"),
            // Not a symbol table any more.
            (patched(symtab + 4, &[1]), "no symbol table"),
            (patched(symtab + 0x28, &[9]), "names no string table"),
            (patched(symtab + 0x38, &[8]), "bad symbol entry size 8"),
            (
                patched(symtab + 0x20, &[0xff, 0xff]),
                "truncated symbol table",
            ),
            // A symbol name pointing past every NUL.
            (patched(0x40 + 48, &[0xff, 0xff, 0, 0]), "not terminated"),
        ];
        for (b, want) in cases {
            let err = check_object(&b).unwrap_err();
            assert!(err.to_string().contains(want), "{err} / {want}");
        }
    }

    #[test]
    fn loader_conf_loads_the_module() {
        assert!(loader_conf_lines().contains("geom_ventoy_load=\"YES\""));
    }
}
