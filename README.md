# JuanitOS

FreeBSD + bhyve like QubesOS

Inspired by TrueNAS Core, Hellfire and HardenedBSD

Uses ZFS and has GPU passthrough to Linux host (mainly for CUDA)

This project is also inspired by Redox and uses Cosmic desktop (https://github.com/pop-os/cosmic-epoch).
The libc is Relibc (https://github.com/redox-os/relibc) instead of traditionnal libc like glibc or musl.

The project also was inspired by ClipOS (https://clip-os.org)

It boots like HaikuOS/BeOS: a logo and a row of icons lighting up, no
scrolling text (see `docs/boot-splash.md`).

## What it is

A FreeBSD host that runs every workload in an isolated bhyve VM ("qube")
backed by a ZFS zvol, with networking funnelled through `sys-net` and a
Rust-unikernel `sys-firewall` (the QubesOS mirage-firewall idea, in Rust),
and a Linux `gpu` qube that owns an NVIDIA card for CUDA. All management
tooling is written in Rust.

See `docs/` for details:

- `docs/architecture.md` — domains, storage layout, networking, lifecycle
- `docs/bootstrap.md` — LFS-style stages: build on FreeBSD, then on JuanitOS
- `docs/installer.md` — the live/install ISO and `jos-install`
- `docs/gpu-passthrough.md` — NVIDIA + CUDA inside a Linux qube
- `docs/unikernel-firewall.md` — single-purpose Rust unikernel qubes
- `docs/boot-splash.md` — the graphical boot: pre-loader, kernel splash, daemon
- `docs/relibc-cosmic.md` — where Relibc and COSMIC fit (host vs. guest)
- `docs/security.md` — what is borrowed from QubesOS, CLIP OS and HardenedBSD

## Repository layout

```text
crates/juanitos-core   library: config, bhyve, zfs, pci, net, doctor, dom0, release, firewall
crates/jos             management CLI
crates/jos-install     installer that runs on the live ISO
crates/jos-release     builds the binaries, the dom0 tarball and the ISO; QEMU harness
crates/jos-fwnet       no_std firewall engine (ARP, DHCP, NAT, rules), host-tested
crates/jos-fwcfg       config-disk format shared by host and unikernel
crates/jos-firewall    the sys-firewall unikernel (UEFI application)
crates/jos-gfx         no_std boot artwork: icons, font, rasteriser, PNG export
crates/jos-boot        graphical UEFI pre-loader that chain-loads loader.efi
crates/jos-splash      boot splash daemon on the vt(4) framebuffer + rc.d clients
release/juanitos.toml  release profile (FreeBSD version, packages, label)
freebsd/               loader.conf / sysctl.conf samples and rc.d scripts
docs/                  design notes (docs/images: rendered previews)
```

## Getting an installable image

The dom0 is a stock FreeBSD release plus packages plus the Rust tools; no
FreeBSD source is compiled. On a FreeBSD host (or the builder VM described
in `docs/bootstrap.md`), as root:

```sh
rustup target add x86_64-unknown-uefi  # for jos-boot.efi and the firewall unikernel
cargo build --release
./target/release/jos-release all      # fetch → bins → rootfs → pack → iso
./target/release/jos-release qemu     # boot release/out/JuanitOS-0.1.0-amd64.iso
```

`bins` also builds the two UEFI applications and wraps the firewall into
`release/out/jos-firewall.img`; both are optional (without them the ESP
boots `loader.efi` directly and `sys-firewall` has to be a FreeBSD qube).

The ISO boots the same dom0 it installs. `jos-install` runs the `jos doctor`
host checks (bhyve, IOMMU, kernel modules) on the live medium first, so you
learn whether the hardware can run JuanitOS before any disk is touched.

Once installed, `jos-release all` works on JuanitOS itself (sources and
vendored crates are in `/usr/local/src/juanitos`), which is how the
bootstrap closes: stage 1 on FreeBSD, stage 2 on JuanitOS.

## Quick start (on the FreeBSD host)

```sh
pkg install edk2-bhyve
cargo install --path crates/jos          # or cargo build --release

# load vmm/nmdm/if_bridge/if_tap and enable IOMMU: see freebsd/loader.conf.sample
jos doctor
jos init

jos create fbsd15      --kind template --os freebsd --disk-gb 10
jos create sys-net     --kind net      --os freebsd --template fbsd15 --pci 5/0/0 --autostart
jos create sys-firewall --kind firewall --os unikernel --netvm sys-net --autostart \
    --kernel /usr/local/share/juanitos/jos-firewall.img   # or --os freebsd --template fbsd15
jos create work        --kind app      --os freebsd --template fbsd15
jos create gpu         --kind gpu      --os linux   --template debian-12 \
    --cpus 8 --memory-mb 16384 --pci 1/0/0 --pci 1/0/1

jos start work
jos console work
jos ls
```

Every command accepts `--dry-run` to print the host commands it would run,
which also makes the CLI usable on a non-FreeBSD development machine
(set `JOS_CONFIG_DIR` to keep qube definitions out of `/usr/local/etc`).

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings

# UEFI applications (own workspaces, x86_64-unknown-uefi only)
(cd crates/jos-boot && cargo build --release && cargo test --lib --target x86_64-unknown-linux-gnu)
(cd crates/jos-firewall && cargo build --release)

# Preview the boot card on any host
cargo run -p jos-splash -- render preview.png --stage network --status "Starting qubes"
```

`juanitos-core` builds on Windows, macOS and Linux; all host interaction
goes through a `Runner` trait so the library is unit-tested with mock
command output. Clippy is configured (root `Cargo.toml`) to warn on
`unwrap`/`expect`/`panic`/indexing outside tests: an OS cannot afford a
panic in critical code, so errors are handled and returned instead.

## Status

v0.1 scaffold targeting FreeBSD 15.1-RELEASE: configuration, storage
provisioning, bhyve command generation, networking plumbing, per-qube
supervisor (restart on reboot, pidfiles and logs under
`/var/run/juanitos`), host checks, release builder and installer, the
`sys-firewall` unikernel (builds; untested on bhyve), and the graphical
boot chain (`jos-boot.efi`, kernel splash, `jos-splash`). Everything is
verified with `--dry-run` and unit tests on the development host; a real
stage-1 build and a QEMU/OVMF boot on FreeBSD are the next milestone. Not
yet done: GUI integration with COSMIC, inter-qube file transfer, a
Relibc-based guest template.
