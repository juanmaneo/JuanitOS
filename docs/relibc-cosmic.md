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
| Linux guests        | app/gpu qubes  | **Relibc** where the image allows; glibc for the CUDA image |

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
- **Relibc-based userland lives inside guests.** A Linux template built on
  Relibc (e.g. a minimal Rust-centric rootfs) is a supported template type;
  the CUDA `gpu` qube uses a conventional glibc distribution because NVIDIA's
  driver and toolkit are only validated there.

## Future goals

1. Track Relibc's Linux ABI maturity and ship a Relibc-based Linux template
   as the default for app qubes.
2. Evaluate a Relibc FreeBSD port (platform layer: `src/platform/`), starting
   with what `jos` and COSMIC actually need.
3. Experiment with Redox itself as a guest OS once bhyve + Redox's drivers
   are a stable combination; it would be the natural home for a
   Relibc-only qube.

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
