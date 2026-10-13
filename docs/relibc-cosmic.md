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

- The **host stays on FreeBSD libc**. COSMIC is built from the FreeBSD ports
  effort (`x11-wm/cosmic-*` and friends) against it.
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

COSMIC is Rust (iced + smithay) and has no systemd dependency, so it runs
in dom0 on the FreeBSD host as-is. What it needs is in ports:

| Need                 | Linux usual     | FreeBSD host                          |
|----------------------|-----------------|---------------------------------------|
| compositor + shell   | `cosmic-*`      | `x11-wm/cosmic-*` ports                |
| seat / VT handling   | `logind`        | `sysutils/seatd` (+ `libseat`)         |
| desktop IPC          | `dbus`          | `devel/dbus`                           |
| login                | `cosmic-greeter`| `greetd` + `cosmic-greeter`, or `cosmic-session` from a TTY |
| audio (optional)     | PipeWire        | `multimedia/pipewire` or none in dom0  |

`rc.conf` on the host enables `seatd_enable`, `dbus_enable` and the
chosen greeter; `jos` runs independently of all of it.

On the host COSMIC is the *management* UI only: the long-term plan is a
`jos`-backed applet (qube list, start/stop, colour labels) and
window-forwarding from guests, similar to Qubes' GUI daemon. Nothing in
v0.1 depends on COSMIC; `jos` is fully usable from a TTY.
