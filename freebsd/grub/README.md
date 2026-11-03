<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# jos_efi: a GRUB module that boots the FreeBSD kernel on UEFI

Ventoy boots a FreeBSD ISO by chain-loading its `loader.efi` ("normal
mode"). On some firmware -- Intel Macs in particular -- that chain-load
fails for every ISO while Ventoy's GRUB itself runs fine, which is why
Ventoy offers a "GRUB2 mode" for Linux ISOs: GRUB loads the kernel
directly. GRUB has a FreeBSD loader too (`kfreebsd`, in `bsd.mod`), but
on UEFI it is incomplete:

- it tags a 64-bit kernel `"elf64 kernel"`; FreeBSD 14 and later look for
  `"elf kernel"` only and panic before the console exists (`unable to
  find kernel metadata`, which shows as a silent reset);
- it passes none of what `loader.efi` passes a UEFI kernel: the EFI
  memory map, the firmware's system table, the framebuffer and the ACPI
  RSDP. Without them the kernel has no ACPI and no console.

`jos_efi.mod` fixes the tag when it is loaded and adds the metadata with
one command, `jos_efi_meta`, run after `kfreebsd` and `kfreebsd_loadenv`
and before any `kfreebsd_module*` (the kernel reads the metadata in load
order). The live ISO ships it as `/boot/grub/x86_64-efi/jos_efi.mod`,
and the `.vcfg` next to the ISO on a Ventoy stick loads it for the
"direct kernel boot" entry (see `docs/installer.md`).

Known limit: the kernel's splash card (`MODINFOMD_SPLASH`) needs a
pointer into the loader's preload area, which a module cannot provide,
so this path boots with the kernel messages on screen until `jos-splash`
starts.

## Build

Against an unpacked GRUB 2.04 source tree (the release Ventoy's GRUB is
based on; only its `include/` directory is read), with the base
toolchain of FreeBSD (clang, lld, llvm-objcopy) or gcc and binutils:

```sh
make GRUB=/path/to/grub-2.04
```

`jos-release` does this for the ISO: it fetches the tarball named in
`release/juanitos.toml` (`[grub]`, sha256-checked), extracts
`include/` and runs this Makefile in a copy under its work directory.
`config.h` stands in for what GRUB's `configure` would generate;
`compile_flags.txt` holds the compiler flags, which clang-tidy
(`cargo xtask lint`) reads as well.

## License

GPL-3.0-or-later, like GRUB: the module is linked into GRUB and written
to its module interface (`THIRD_PARTY.md`). The rest of JuanitOS stays
under `LICENSE`.
