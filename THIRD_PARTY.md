# Third-party code, inspirations and redistribution

JuanitOS's own code is under the BSD 3-Clause license (`LICENSE`). This
file lists what in the repository, or in the images built from it, comes
from somewhere else, and under which terms.

## Code in this repository

| Path | Origin | License |
|------|--------|---------|
| `freebsd/geom_ventoy/` | [Ventoy](https://github.com/ventoy/Ventoy) `Unix/ventoy_unix_src/FreeBSD/geom_ventoy_src/15.x`, commit 5fae0ca (local changes listed in its README) | BSD-2-Clause, © longpanda, © Pawel Jakub Dawidek. File headers kept. |
| `crates/jos-gfx/fonts/OpenSans-Regular.ttf` | [Open Sans](https://github.com/googlefonts/opensans) `fonts/ttf/OpenSans-Regular.ttf`, commit bd7e376, unmodified; embedded in every binary that draws text | SIL Open Font License 1.1 (`crates/jos-gfx/fonts/OFL.txt`, shipped next to it). © The Open Sans Project Authors. |
| `tools/dev/cosmic-patches/*.patch` | Patches to COSMIC components (pop-launcher) | Same license as the patched project (GPL-3.0); applied at build time, not linked into JuanitOS code |

Everything else (Rust crates, kernel module `jos_bootwatch`, rc.d scripts,
templates, boot artwork, the icons) is original work under
`LICENSE`.

## Rust dependencies

Resolved from `Cargo.lock` (host workspace) and the two UEFI workspaces
(`crates/jos-boot`, `crates/jos-firewall`). All are permissive or
file-level copyleft and compatible with BSD-3-Clause distribution:

| License | Crates (examples) |
|---------|-------------------|
| MIT OR Apache-2.0 | anyhow, clap, serde, toml, sha2, uefi, log, skrifa, read-fonts, … |
| Apache-2.0 | ab_glyph_rasterizer |
| MIT | generic-array, strsim, winnow, ptr_meta, libm, core_maths |
| Zlib / MIT OR Zlib OR Apache-2.0 | zlib-rs, miniz_oxide, bytemuck |
| Unlicense OR MIT | memchr |
| 0BSD OR MIT OR Apache-2.0 | adler2 |
| Unicode-3.0 (with MIT OR Apache-2.0) | unicode-ident, yoke, zerofrom |
| MPL-2.0 | ucs2 (UEFI crates only; file-level copyleft: modifications to its own files must stay MPL, linking is unrestricted) |

Binary distributions must carry these crates' notices. The dom0 ships the
JuanitOS source with every crate vendored under
`/usr/local/src/juanitos/vendor`, each with its own license file, which
covers that. To list them again:

```sh
cargo metadata --format-version 1 | jq -r '.packages[] | "\(.license)\t\(.name)"' | sort
```

## Inspirations (ideas only, no code taken)

| Project | License | What was borrowed |
|---------|---------|-------------------|
| QubesOS | GPL-2.0 | Security-by-compartmentalisation model, sys-net / sys-firewall, templates |
| mirage-firewall | BSD-2-Clause | Firewall as a unikernel qube |
| CLIP OS | LGPL-2.1 and others | Hardening and update model |
| HardenedBSD, FreeBSD | BSD-2-Clause | Hardening sysctls, base system |
| TrueNAS Core | BSD / GPL | ZFS-centric appliance layout |
| Hellfire | — | Overall concept |
| Haiku / BeOS | MIT | Boot screen with a row of lighting icons |
| Redox, relibc | MIT | Rust OS and libc for the Redox guest template |
| Calamares, Anaconda | GPL-3.0, GPL-2.0 | Installer flow (pages, plan, summary before writing) |
| rEFInd, GRUB, OpenCore | GPL-3.0, GPL-3.0, BSD-3-Clause | Knowledge of Apple EFI quirks (console control) |

Re-implementing an idea is not a derivative work; no source from these
projects is in this repository. Keep it that way: code copied from a GPL
project would have to be isolated and licensed accordingly.

## Built images (ISO, dom0 tarball)

The images contain third-party binaries under their own licenses:

- **FreeBSD base and kernel** (BSD-2-Clause, with some files under other
  permissive licenses): unmodified release sets.
- **FreeBSD packages** (`pkg`, rust, git, vim, tmux, …): each under its
  own license; several are GPL (e.g. git, GPL-2.0).
- **COSMIC desktop** (GPL-3.0), built from source by
  `tools/dev/cosmic-build.sh` and shipped as a local pkg repository.
- **Ventoy module** (`geom_ventoy.ko`, BSD-2-Clause), built from
  `freebsd/geom_ventoy`.

Before publishing an image, meet the GPL source obligations for the GPL
binaries it ships: publish, next to the ISO, the exact sources of the
COSMIC components (the checked-out trees plus `tools/dev/cosmic-patches`)
and point to the FreeBSD ports/packages sources of the package set
(`pkg info` in the dom0 lists them with their licenses). Distributing the
ISO privately to yourself has no such obligation.
