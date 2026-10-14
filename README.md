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
and a Linux `gpu` qube that owns an NVIDIA card for CUDA. Qubes are
cloned from templates built from the distributions' official images:
ready-made development templates with Docker (Debian 13) and Podman
(FreeBSD 15), and Redox OS as the Relibc-based guest. All management
tooling is written in Rust.

See `docs/` for details:

- `docs/architecture.md` — domains, storage layout, networking, lifecycle
- `docs/bootstrap.md` — LFS-style stages: build on FreeBSD, then on JuanitOS
- `docs/installer.md` — the live/install ISO and the graphical installer
- `docs/templates.md` — guest templates: Debian 13 + Docker, FreeBSD 15 + Podman, Redox
- `docs/inter-qube.md` — the per-qube share, the guest agent, `jos copy`, and
  `jos-copy` requests from a qube under `jos policy` (the qrexec idea)
- `docs/gpu-passthrough.md` — NVIDIA + CUDA inside a Linux qube
- `docs/unikernel-firewall.md` — single-purpose Rust unikernel qubes
- `docs/boot-splash.md` — the graphical boot: pre-loader, kernel splash, daemon
- `docs/relibc-cosmic.md` — where Relibc and COSMIC fit (host vs. guest)
- `docs/security.md` — what is borrowed from QubesOS, CLIP OS and HardenedBSD

## Repository layout

```text
crates/juanitos-core   library: config, bhyve, zfs, pci, net, doctor, dom0, release, firewall, share, requests
crates/jos             management CLI
crates/jos-install     installer on the live ISO: graphical wizard (Calamares/Anaconda-style) + text mode
crates/jos-release     builds the binaries, the dom0 tarball and the ISO; QEMU harness
crates/jos-fwnet       no_std firewall engine (ARP, DHCP, NAT, rules), host-tested
crates/jos-fwcfg       config-disk format shared by host and unikernel
crates/jos-firewall    the sys-firewall unikernel (UEFI application)
crates/jos-gfx         no_std boot artwork: icons, font, rasteriser, PNG export
crates/jos-boot        graphical UEFI pre-loader that chain-loads loader.efi
crates/jos-splash      boot splash daemon on the vt(4) framebuffer + rc.d clients
crates/jos-fb          vt(4) framebuffer display and keyboard decoder (splash, installer)
xtask/                 `cargo xtask`: CI pipeline, coverage gate, screenshots
coverage.toml          per-crate line-coverage floors (ratcheted, only go up)
templates/             cloud-init user-data of the guest template recipes; agent/ the guest agent and jos-copy
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

## Installing

Boot the ISO: after the splash, a graphical installer takes the screen,
the way QubesOS greets you with Anaconda. It is our own Rust program
drawing straight on the FreeBSD console framebuffer (no X11, no Qt), laid
out like Calamares: steps on the left, the current page on the right.

![installer](docs/images/installer-3-disk.png)

Welcome → **Hardware** (the `jos doctor` checks: VT-x/AMD-V, IOMMU, bhyve
modules, passthrough devices) → **Disk** → **System** (hostname, swap,
COSMIC at boot) → **Users** → **Summary** (nothing is written before you
tick "erase the disk" here) → **Install** (progress) → **Finish**
(reboot). The whole disk becomes GPT + EFI + swap + a ZFS pool laid out
like `bsdinstall zfsboot`. Serial consoles get the same steps as text
prompts, and `jos-install --non-interactive --disk nda0 ...` installs
unattended. Details and all screens: `docs/installer.md`.

Once installed, `jos-release all` works on JuanitOS itself (sources and
vendored crates are in `/usr/local/src/juanitos`), which is how the
bootstrap closes: stage 1 on FreeBSD, stage 2 on JuanitOS.

## Quick start (on the FreeBSD host)

```sh
pkg install edk2-bhyve
cargo install --path crates/jos          # or cargo build --release

# load vmm/nmdm/if_bridge and enable IOMMU: see freebsd/loader.conf.sample
jos doctor
jos init

# Templates from the distributions' cloud images (see "Guest templates")
jos template install fbsd15-net           # FreeBSD 15.1 as sys-net (dnsmasq + pf NAT)
jos template install fbsd15-dev           # FreeBSD 15.1 + Podman
jos template install debian-13-dev        # Debian 13 + Docker

jos create sys-net     --kind net      --os freebsd --template fbsd15-net --pci 5/0/0 --autostart
jos create sys-firewall --kind firewall --os unikernel --netvm sys-net --autostart \
    --kernel /usr/local/share/juanitos/jos-firewall.img   # or --os freebsd --template fbsd15-dev
jos create work        --kind app      --os linux   --template debian-13-dev
jos create gpu         --kind gpu      --os linux   --template debian-13-dev \
    --cpus 8 --memory-mb 16384 --pci 1/0/0 --pci 1/0/1
jos create vault       --kind app      --os freebsd --template fbsd15-dev --netvm none

jos start work
jos console work                         # serial console (nmdm), autologin shell
jos run work docker ps                   # run a command there, get its exit status
jos copy work:/home/user/report.pdf jail # qube to qube, lands in jail:/mnt/jos/in/work/
jos copy /root/notes.txt work            # dom0 to qube, lands in work:/mnt/jos/in/dom0/
jos policy add work jail allow           # let work push files to jail by itself...
jos run work jos-copy /tmp/build.log jail   # ...with jos-copy inside the qube
jos requests                             # what the policy left for you to decide
jos ls
```

`jos serve` (started by `rc.d/jos`) answers the qubes' requests according
to the policy; anything the policy does not settle waits for
`jos requests approve|deny`.

Every command accepts `--dry-run` to print the host commands it would run,
which also makes the CLI usable on a non-FreeBSD development machine
(set `JOS_CONFIG_DIR` to keep qube definitions out of `/usr/local/etc`).
On a machine without IOMMU (no NIC passthrough for `sys-net`), or before
`sys-net` exists, `--uplink IFACE` bridges a host interface straight into
a qube.

## Guest templates

As in QubesOS, software is installed once in a template and app qubes run
copies of it: here a template is a ZFS zvol snapshotted as `@base`, and
every app qube is a `zfs clone` of that snapshot. `jos template install`
builds templates from the distributions' **official cloud images**,
verified against their published SHA-512/SHA-256 lists, and provisions
them on a first boot with cloud-init (Debian) or nuageinit (FreeBSD)
from a NoCloud seed; the recipes are plain `#cloud-config` files in
`templates/`.

Four templates ship today:

| template        | base                       | what for                                        |
|-----------------|----------------------------|-------------------------------------------------|
| `debian-13-dev` | Debian 13 "trixie"         | development with **Docker** Engine, Compose v2, Buildx (Debian packages) |
| `fbsd15-dev`    | FreeBSD 15.1 (ZFS)         | development with **Podman** + Buildah on jails (`ocijail`), ZFS storage, pf NAT, Linux images via Linux emulation |
| `fbsd15-net`    | FreeBSD 15.1 (ZFS)         | **`sys-net`**: DHCP on the physical NIC, dnsmasq DHCP/DNS and pf NAT for `sys-firewall` |
| `redox`         | Redox OS `server` nightly  | the **Relibc** guest: a whole userland on Redox's Rust libc (console-only, see `docs/templates.md`) |

The two development templates come with git, a compiler toolchain and
the usual tools, an unprivileged `user` with password-less `sudo`/`doas`,
and autologin on the serial console (`jos console`, reachable from dom0
only), which is also what `jos run <qube> <command>` uses. Redox qubes
have no autologin, so `jos run` logs in as Redox's `user` first, and
get NVMe + e1000 devices instead of virtio (Redox's virtio drivers need
the modern transport bhyve lacks).

```sh
jos template recipes
jos template install debian-13-dev
jos create web --kind app --os linux --template debian-13-dev
jos start web && jos console web       # user@web:~$ docker run --rm hello-world

jos template run debian-13-dev         # maintenance boot for updates, then:
jos template snapshot debian-13-dev 2026-10
```

The development recipes were verified on the real images (provisioning,
then Docker and Podman running containers on the next boot); `fbsd15-dev`,
`fbsd15-net` and `redox` also on bhyve itself; details in
`docs/templates.md`.

## Development

Project automation is a Rust program too ([cargo xtask](https://github.com/matklad/cargo-xtask)):

```sh
cargo xtask ci            # fmt, clippy (host, FreeBSD, UEFI), tests + coverage gate, UEFI builds
cargo xtask test          # just the tests (workspace + jos-boot's library on the host)
cargo xtask lint          # clippy -D warnings on every target
cargo xtask fmt --fix
cargo xtask coverage      # line coverage per crate, checked against coverage.toml
cargo xtask coverage --html --ratchet
cargo xtask screenshots   # regenerate docs/images (installer pages, boot card)
cargo xtask ci -- --offline   # arguments after -- go to cargo
```

Coverage uses `cargo-llvm-cov` (`rustup component add llvm-tools-preview`,
`cargo install cargo-llvm-cov`). `coverage.toml` holds a floor per crate
and for the total; the gate fails when a number drops below its floor,
and `--ratchet` raises the floors to what was just measured (minus a
small slack for host differences, never lowering them). Coverage can
therefore only improve, one commit at a time: add tests, ratchet, commit
both. The same `cargo xtask ci` runs in GitHub Actions
(`.github/workflows/ci.yml`).

How the code stays testable on a Linux or macOS laptop:

- all host interaction goes through a `Runner` trait, so the libraries are
  unit-tested with mock command output, and every CLI has `--dry-run`;
- the binaries have end-to-end tests (`crates/*/tests/cli.rs`) that run
  them in dry-run mode, e.g. the README quick start against a temporary
  `JOS_CONFIG_DIR`;
- the installer and the splash are pure state machines rendered into
  images; only the thin `vt(4)` layer in `jos-fb` needs FreeBSD;
- the UEFI applications keep their logic in host-testable libraries
  (`jos-gfx`, `jos-fwnet`, `jos-fwcfg`, `jos-boot`'s lib) and boot under
  QEMU + OVMF with `tools/uefi-test.sh`.

Clippy is configured (root `Cargo.toml`) to warn on
`unwrap`/`expect`/`panic`/indexing outside tests: an OS cannot afford a
panic in critical code, so errors are handled and returned instead.

```sh
# Preview the boot card or the installer on any host
cargo run -p jos-splash -- render preview.png --stage network --status "Starting qubes"
cargo run -p jos-install -- --screenshots /tmp/installer
```

## Status

v0.1 scaffold targeting FreeBSD 15.1-RELEASE: configuration, storage
provisioning, bhyve command generation, networking plumbing, per-qube
supervisor (restart on reboot, pidfiles and logs under
`/var/run/juanitos`), host checks, release builder, the graphical
installer, guest templates from the distributions' images (Debian 13 +
Docker, FreeBSD 15 + Podman, Redox), `jos run` over the serial console,
the per-qube virtio-9p share with its guest agent (own hostname per
clone), `jos copy` on top of it and the request broker (`jos-copy` in a
qube, `jos policy`, `jos serve`), the `sys-firewall` unikernel, and the graphical boot chain
(`jos-boot.efi`, kernel splash, `jos-splash`). Everything is verified
with `--dry-run` and unit tests on the development host; on Linux,
`tools/uefi-test.sh` boots the UEFI pieces under QEMU + OVMF:
`jos-boot.efi` draws the card and chain-loads the genuine FreeBSD
`loader.efi`, and `jos-firewall.efi` serves DHCP, answers ARP and NATs a
ping end to end (untested on bhyve itself).

Stage 1 is done from a Linux host: `jos-release builder --auto` builds
the ISO inside FreeBSD's official 15.1 VM image under QEMU/KVM (about 10
minutes). That ISO boots on OVMF to the graphical installer, whose
hardware check passes in the VM and which installs onto a virtual disk;
the installed system then boots through the splash to a login. Inside
that installed dom0 (nested virtualisation), real bhyve runs the qubes:
`fbsd15-dev`, `fbsd15-net` and `redox` were installed there from a USB
stick, and the whole network chain works — `work` (clone of
`fbsd15-dev`) gets its address from the Rust `sys-firewall` unikernel,
which takes its lease from `sys-net` (clone of `fbsd15-net`, bridged to
the dom0's NIC), and `fetch https://www.freebsd.org/` succeeds from
`work` while `jos firewall` rules are enforced on the way. `jos run`
answers from FreeBSD's `sh` and Redox's `ion` alike, with exit statuses.
On the same dom0, a clone created after the guest agent was added to
`fbsd15-dev` boots as itself (`hostname` = `test`) with the dom0 share
on `/mnt/jos`, and `jos copy` moved a file and a directory from `work`
into `test`'s inbox and the agent script from dom0 into the qubes. The
`debian-13-dev` template was installed there from Debian's `generic`
image and a clone `web` boots as `web`, takes its address from
`sys-firewall`, reaches the Internet, runs Docker, and mounts the share.
With `jos serve` running, `jos-copy` inside `work`, `test` and `web`
moved files to each other as the policy allowed, was refused where it
said `deny`, and waited for `jos requests approve` where it said
nothing. The test suite reaches 94% line coverage, gated per crate in CI.

Stage 2 is done too: the stage-1 ISO was installed headless (`qemu -vga
none`, text installer on the serial port) and the installed JuanitOS
rebuilt itself with `jos-release all` in 5.5 minutes, producing an ISO
stamped `built_on = "juanitos"`, `stage = 2` that boots to the installer.
The stage-1 and stage-2 dom0 tarballs have the same 128,568 entries and
differ only in the JuanitOS binaries, `release.toml` and `pkg`'s
bookkeeping (`docs/bootstrap.md` has the comparison and what it
uncovered).

Not yet done: NIC passthrough on real hardware (the chain above used
`--uplink`; `--pci` is implemented but only dry-run tested), a display
for Redox qubes (bhyve's `fbuf` asserts when Redox programs it), COSMIC
(no FreeBSD packages yet: the dom0 falls back to the console), and an
event-driven guest agent (`jos serve` polls the shares every two
seconds and `jos run` still goes through the serial console;
`docs/inter-qube.md`).
