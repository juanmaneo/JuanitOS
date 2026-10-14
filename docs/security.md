# Security model and its sources

JuanitOS borrows from three projects. Each one solves a different part of
the problem, and the table below records what is taken, what is already
in the tree, and what is still a plan — so that design choices can be
traced back rather than rediscovered.

| Project       | Idea taken                                      | Status in JuanitOS |
|---------------|-------------------------------------------------|--------------------|
| QubesOS       | security by compartmentalisation: every workload is a VM; `sys-net` / `sys-firewall` chain; disposable, template-based qubes; a minimal, trusted dom0 | the whole `jos` model: templates + clones, NetVM/FirewallVM, dom0 with no network of its own |
| QubesOS (mirage-firewall) | the firewall as a unikernel without a general-purpose OS | `crates/jos-firewall`, in Rust |
| HardenedBSD   | kernel/userland exploit mitigations on the host  | `freebsd/sysctl.conf.sample` (ASLR, W^X, `see_other_uids`, no core dumps, no forwarding on the host) |
| CLIP OS       | hardened, *immutable* system image; multi-level security; a build that is fully reproducible from source; strict separation between the system and user data | partially: read-only live medium, single-artifact dom0, LFS-style bootstrap; see below |

## What CLIP OS adds to the Qubes picture

[CLIP OS](https://clip-os.org) (ANSSI) is not a hypervisor-based system;
it hardens a single Linux and splits it into *cages*. Its contribution to
JuanitOS is less the architecture than the engineering discipline:

1. **The system is an image, not a package set.** CLIP OS builds the whole
   OS from source into a signed, read-only image; updates replace the image.
   JuanitOS already builds dom0 as one artifact (`juanitos-dom0.txz`), the
   live medium runs that exact tree read-only, and the installer lays the
   same tree onto ZFS. The remaining step — booting the *installed* dom0
   from a read-only boot environment with `/usr/local/etc/juanitos` and
   `/var` as the only writable datasets — is planned (`jos-install` already
   creates separate datasets for them; `bectl`-style A/B updates follow
   naturally from ZFS boot environments).
2. **Reproducible, self-hosting build.** CLIP OS insists the build be
   reproducible and run inside the project's own tooling. JuanitOS does the
   same with its LFS-style bootstrap: stage 1 builds the ISO on FreeBSD
   from the FreeBSD release sets (SHA-256 verified against the release
   `MANIFEST`) and vendored Rust crates; stage 2 rebuilds JuanitOS on
   JuanitOS (`release.toml` records `built_on` and the stage number).
   `jos-release` is the only build path, and `cargo vendor` makes stage 2
   offline.
3. **Minimal, memory-safe privileged code.** CLIP OS carefully bounds what
   runs privileged. JuanitOS pushes the same way with Rust everywhere in
   dom0 (`jos`, the supervisor, the installer, the splash — all under
   clippy rules that forbid `unwrap`/`panic` outside tests) and a Rust
   unikernel for the most exposed component, the firewall.
4. **Multi-level flows.** CLIP OS enforces that data flows between its
   security levels only through controlled channels. In JuanitOS the
   inter-qube channels are the network through `sys-firewall`, whose
   per-client rules (`jos firewall add work deny any any`) are the
   equivalent of CLIP's flow policy, and the share, which is explicit
   and dom0-mediated: each qube only ever sees its own share, and files
   cross from one share to another only when dom0 moves them, either on
   the administrator's request (`jos copy`) or on a qube's request
   (`jos-copy`) that the policy allows (`jos policy`: `copy SRC DST
   allow|deny|ask`, first match wins, `ask` by default, settled by the
   operator with `jos requests`). That is qrexec's policy model, with
   files in place of a socket protocol; see `docs/inter-qube.md`. A
   qube names a file in its own outbox and a destination, nothing more:
   it cannot address another qube's share or any path on dom0. The
   agent channel (`jos run`) only goes the other way: dom0 sends
   commands, the qube answers; a qube cannot reach dom0 or another qube
   through it, and its daemon takes orders from the channel alone, which
   only bhyve on dom0 holds the other end of. The one thing a qube may
   initiate is a wake-up: a line on its `events` port makes `jos serve`
   look at the request directories sooner. The line is never parsed,
   only logged (cut to printable ASCII), and wake-ups are rate-limited,
   so the port carries no authority and little denial-of-service
   potential (`docs/inter-qube.md`).

## Where JuanitOS deliberately differs

- **dom0 is FreeBSD, not a minimal Linux.** Reusing an unmodified FreeBSD
  release (bhyve, ZFS, `vt(4)`, `pkg`) is what makes the project feasible
  for one person; the price is a larger TCB than CLIP OS or Qubes' dom0.
  The mitigations in `sysctl.conf.sample` and a HardenedBSD base (supported
  but untested) narrow the gap.
- **No secure boot chain yet.** `jos-boot.efi` and `loader.efi` are not
  signed. UEFI Secure Boot with a project key, and ZFS-level integrity
  (`sha512` checksums are on by default; `bectl` snapshots before updates)
  are future work.
- **GPU passthrough is a trade-off.** A Linux qube owning an NVIDIA card
  is exactly the kind of large driver surface CLIP OS avoids. It lives in
  its own qube with no network by default for that reason.

## Host hardening checklist (installed dom0)

- `freebsd/sysctl.conf.sample` applied (the installer ships it under
  `/usr/local/share/juanitos/`; copy the lines you want into
  `/etc/sysctl.conf`).
- `hw.vmm.iommu.enable=1` and every passthrough device bound to `ppt(4)`
  at boot (`jos doctor` checks both).
- `sshd_enable="NO"` and `sendmail_enable="NONE"` (generated `rc.conf`);
  dom0 has no network interface besides the bridges to `sys-net`.
- Keep `jos_enable="YES"` as the only service that starts VMs; app qubes
  get their network only through `sys-firewall`.
