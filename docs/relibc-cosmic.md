# Relibc, COSMIC and the host/guest libc decision

The README states that JuanitOS uses Relibc instead of glibc or musl and the
COSMIC desktop. This document records how that maps onto the actual stack in
v0.1 and what is deferred.

## Where things run

| Component           | Where          | libc                              |
|---------------------|----------------|-----------------------------------|
| FreeBSD kernel/base | host (dom0)    | FreeBSD libc (unavoidable)        |
| `jos`, `juanitos-core` | host        | Rust `std`; libc-agnostic         |
| COSMIC desktop      | host           | FreeBSD libc via ports (Rust apps)|
| FreeBSD guests      | app/net/fw qubes | FreeBSD libc                    |
| Linux guests        | app/gpu qubes  | glibc (Debian; NVIDIA's CUDA stack is only validated there) |
| Redox guests        | app qubes (`redox` template) | **Relibc** — Redox's own libc, the whole userland is built on it |

## Why Relibc is not on the host (yet)

Relibc is a Rust libc written for Redox; its Linux support exists mainly to
build and test Redox userland. It has **no FreeBSD target**. Replacing
FreeBSD's libc would mean either porting Relibc's platform layer to FreeBSD
syscalls (a significant project) or rebuilding the base system around it,
which conflicts with staying close to stock FreeBSD / HardenedBSD / TrueNAS
Core tooling that the project wants to lean on.

Decision for v0.1:

- The **host stays on FreeBSD libc**. COSMIC is built from source against
  it (`tools/dev/cosmic-build.sh`, below) and packaged with `pkg`.
- Our own tooling is pure Rust on `std` and deliberately avoids libc-specific
  behaviour, so it is ready to be rebuilt against Relibc when a target exists.
- **Relibc-based userland lives inside guests, as Redox OS.** Rather than
  grafting Relibc onto a Linux rootfs, the Relibc template is Redox itself:
  the `redox` recipe (`jos template install redox`, `docs/templates.md`)
  installs the upstream `server` nightly, whose every program — `init`,
  the drivers, `ion`, `coreutils` — is linked against Relibc. The CUDA
  `gpu` qube uses a conventional glibc distribution because NVIDIA's driver
  and toolkit are only validated there.

## Redox as a guest: where it stands

`jos` knows Redox as `--os redox`: such qubes get an NVMe disk and an
e1000 NIC (bhyve's virtio is the legacy transport Redox's drivers do not
speak). The nightly boots on bhyve with the UEFI bootrom to the serial
login, gets a DHCP lease on the e1000 and answers `jos run` (which logs
in as Redox's password-less `user` and speaks `ion`'s dialect). What is
missing is a display: bhyve's `fbuf` framebuffer makes bhyve assert when
Redox's `pcid` programs it, so Redox qubes are console-only for now.
`docs/templates.md` has the details.

## Future goals

1. A Redox `desktop` recipe once bhyve's `fbuf` and Redox's `pcid` get
   along (today bhyve asserts in its BAR registration when Redox programs
   the framebuffer device); Orbital in a qube window is the natural
   COSMIC integration target.
2. Evaluate a Relibc FreeBSD port (platform layer: `src/platform/`), starting
   with what `jos` and COSMIC actually need.
3. Redox's own package manager (`pkg`) inside the qube for a Redox
   development template, mirroring `debian-13-dev` and `fbsd15-dev`.

## COSMIC notes

COSMIC is Rust (iced + smithay); its systemd and logind integration is
behind cargo features, so it runs in dom0 on the FreeBSD host once those
are switched off. FreeBSD has no COSMIC packages: the ports tree only
carries `x11-wm/cosmic-comp` at `epoch-1.0.0` (unmaintained since late
2025, no binary package for 15.x), nothing for the session, panel,
applets or applications, and no `greetd` at all. JuanitOS therefore
builds COSMIC itself.

| Need                 | Linux usual     | FreeBSD host                          |
|----------------------|-----------------|---------------------------------------|
| compositor + shell   | `cosmic-*`      | built from `pop-os/cosmic-epoch` (`epoch-1.10.0`) by `tools/dev/cosmic-build.sh`, shipped as a local `pkg` repository |
| KMS drivers          | kernel          | `graphics/drm-kmod`; the installer adds `amdgpu`/`i915kms` to `kld_list` from `pciconf` |
| seat / VT handling   | `logind`        | `sysutils/seatd` (+ `libseat`)         |
| desktop IPC          | `dbus`          | `devel/dbus`                           |
| login                | `cosmic-greeter`| `x11/ly` on `ttyv1` (`cosmic-greeter` needs greetd) |
| audio (optional)     | PipeWire        | `multimedia/pipewire` or none in dom0  |

### Building the packages

`tools/dev/cosmic-build.sh` runs inside the FreeBSD build VM
(`tools/dev/freebsd-vm.sh`, a stock 15.1 under QEMU/KVM) as root:

```sh
tools/dev/freebsd-vm.sh scp freebsd tools/dev/cosmic-build.sh root@127.0.0.1:/root/
tools/dev/freebsd-vm.sh ssh freebsd ./cosmic-build.sh all   # deps fetch patch build package repo
tools/dev/freebsd-vm.sh scp freebsd -r root@127.0.0.1:/root/cosmic/repo target/work/cosmic/
```

It clones `cosmic-epoch` with its submodules (minus the git-lfs
wallpapers), applies the FreeBSD fixes the old port carried (`/sys`
is not sysfs; fontdb's `fonts.conf` lives under `/usr/local/etc`) plus
one for the `libdisplay-info` 0.4 bindings cap, then builds every
component with `--release` into one shared target directory. Components
whose default features are Linux-only get them switched off:
`cosmic-comp` and `cosmic-session` (`systemd`, `logind`),
`cosmic-notifications` and `xdg-desktop-portal-cosmic` (journald),
`cosmic-files` (`io-uring`). `cosmic-greeter` (greetd), `cosmic-store`
(flatpak, systemd scopes) and the wallpapers are not built. Each
component is staged with its own `just install` recipe (with GNU
`install`/`sed` on the path, which the recipes assume) and turned into a
package named after it, version `1.10.0`, origin `juanitos/<name>`;
`pkg repo` writes the catalogue.

`release/juanitos.toml` points `[packages] local_repo` at the copied
repository and `repo = "latest"` at the package branch the VM built
against (the shared libraries must match). `jos-release rootfs` copies
the repository into the root at `/usr/local/share/juanitos/pkg`, enables
it through `/usr/local/etc/pkg/repos/juanitos.conf` and installs the
components best-effort; the installed system keeps the repository, so
`pkg install cosmic-player` works offline later.

### Login

`rc.conf` enables `seatd_enable`, `dbus_enable` and `jos_login_enable`;
`jos` runs independently of all of it. The installer hands `ttyv1` to
`ly` (a `Ly` gettytab entry and the `/etc/ttys` line the FreeBSD port
documents; `ttyv0` keeps its text login) and writes
`/usr/local/etc/ly/config.ini`, which lists the Wayland sessions in
`/usr/local/share/wayland-sessions` and runs
`/usr/local/libexec/jos-session` as `login_cmd`: ly does not set
`XDG_RUNTIME_DIR` on FreeBSD and a compositor needs one, so the script
makes `/var/run/user/<uid>` (the directory is created by
`rc.d/jos-login`) and execs `start-cosmic`. `rc.d/jos-splash-finish`
keeps the boot splash on the screen until ly switches to its terminal.

What has been verified: the packages build and install into the dom0
root. Running the compositor needs a KMS device, which neither the QEMU
build VM nor bhyve provide to a FreeBSD guest (`drm-kmod` has no
virtio-gpu driver), so the first COSMIC session has to be on real
hardware — a stage-1 ISO on the StarBook is the planned test.

On the host COSMIC is the *management* UI only: the long-term plan is a
`jos`-backed applet (qube list, start/stop, colour labels) and
window-forwarding from guests, similar to Qubes' GUI daemon. Nothing in
v0.1 depends on COSMIC; `jos` is fully usable from a TTY.
