# Unikernel qubes: a Rust `sys-firewall`

QubesOS ships an optional `sys-firewall` built as a MirageOS unikernel
(`qubes-mirage-firewall`, OCaml): a single-purpose VM with no shell, no
package manager and a ~30 MB footprint. The attack surface of the most
exposed component in the system shrinks to a few thousand lines of
memory-safe code. JuanitOS adopts the concept, implemented in Rust.

## Why not reuse qubes-mirage-firewall directly

- It is Xen-specific: `netfront`/`netback`, XenStore and QubesDB have no
  bhyve equivalents, so the networking and configuration layers would have
  to be rewritten anyway.
- MirageOS runs on bhyve only through the Solo5 `hvt` tender, which is a C
  program that drives `/dev/vmm` directly and would be a second launcher
  path next to `bhyve(8)`.
- The project standard is Rust. OCaml would be a second toolchain for one
  component, and the firewall logic is small enough that porting it is not
  much harder than integrating it.

What *is* reused is the design: one tiny VM with two NICs, an upstream link
to `sys-net` and a downstream bridge for app qubes, running a NAT + stateful
filter whose rules come from the host.

## Guest ABI

A unikernel qube is an ordinary bhyve guest, so no new hypervisor plumbing
is needed:

- Boots via the standard `edk2-bhyve` bootrom from a FAT image
  (`Qube::kernel`) that contains the unikernel as `EFI/BOOT/BOOTX64.EFI`.
  The image is copied to `<run_dir>/<name>.boot.img` before every boot
  and that copy is what the VM gets: the bhyve bootrom refuses to boot
  from a `virtio-blk,...,ro` device ("UEFI Misc Device: Not Found"), so
  the disk is writable and the shipped image stays pristine instead.
- Two `virtio-net` devices: slot 3 upstream (`tap-<name>` on the NetVM's
  bridge), slot 4 downstream (`tapd-<name>` on `br-<name>`).
- `com1` on the `nmdm` console for logs; `jos console sys-firewall` works
  like for any other qube.
- 1 vCPU, 128 MB by default (`UNIKERNEL_CPUS` / `UNIKERNEL_MEMORY_MB`).

`jos` side (implemented):

```sh
jos create sys-firewall --kind firewall --os unikernel \
    --kernel /usr/local/share/juanitos/jos-firewall.img --netvm sys-net --autostart
```

No zvol is provisioned; `jos remove` leaves the image alone.

## The unikernel itself (`crates/jos-firewall`)

Target: `x86_64-unknown-uefi` (tier-2 Rust target, `no_std`), running as a
UEFI application that never exits boot services. That keeps the kernel to
*application* code: the bhyve bootrom (`edk2-bhyve`) keeps owning
the virtio devices through EDK2's `VirtioNetDxe` / `VirtioBlkDxe` drivers
and exposes them as `SimpleNetwork` and `BlockIO` protocols, so the
unikernel contains no DMA code, no driver, and almost no `unsafe`.

| Crate              | Builds for | Contents                                             |
|--------------------|------------|------------------------------------------------------|
| `jos-fwnet`        | any host   | packet parsing, ARP, DHCP (client up, server down), NAT + conntrack, rule evaluation, the `Engine` — all `no_std`, unit-tested |
| `jos-fwcfg`        | any host   | the config-disk text format shared with `juanitos-core::firewall` |
| `jos-firewall`     | UEFI only  | `main.rs` (poll loop), `nic.rs` (`SimpleNetwork` by PCI slot), `platform.rs` (config disk via `BlockIO`, reset, `hlt`), `clock.rs` (TSC calibrated against the firmware timer) |

Packet path: downstream RX → lookup source qube by MAC/IP → evaluate rules
→ NAT → upstream TX, and the reverse for replies via the conntrack table.
Everything is single-threaded and poll-driven; after 2 ms without traffic
the CPU halts until the next firmware timer tick so an idle firewall does
not burn a host core. Stats and the DHCP state go to `com1` (`jos console
sys-firewall`); a fatal error or panic resets the VM after 5 s and the
`jos` supervisor restarts it.

Build and package:

```sh
rustup target add x86_64-unknown-uefi
(cd crates/jos-firewall && cargo build --release)        # → jos-firewall.efi
jos-release bins   # builds it too and wraps it into release/out/jos-firewall.img
```

The image is a 2 MB unpartitioned FAT volume with the unikernel at
`EFI/BOOT/BOOTX64.EFI` (`makefs -t msdos`, as the ISO's `efiboot.img`);
the release installs it as `/usr/local/share/juanitos/jos-firewall.img`.

### Verified under QEMU + OVMF

`tools/uefi-test.sh firewall` boots the unikernel on any Linux host with
the bhyve PCI layout (two `virtio-net-pci` at slots 3 and 4, the config
disk as `virtio-blk-pci` at slot 30, boot image at slot 2; OVMF's
`VirtioNetDxe`/`VirtioBlkDxe` stand in for the bhyve bootrom's). Serial
log of a run:

```text
jos-firewall 0.1.0 starting
config disk: 114 bytes of text at slot 30
config: firewall `sys-firewall`, gateway 10.137.0.2, 1 client(s), upstream dhcp
  client work 10.137.0.3 02:4a:5b:98:d2:60 policy allow (0 rules)
slot 3: SimpleNetwork ready, max packet 1500
slot 4: SimpleNetwork ready, max packet 1500
upstream NIC 02:4a:00:00:00:01 (slot 3)
downstream NIC 02:4a:00:00:00:02 (slot 4)
tsc: 3194595 ticks/ms
upstream bound: 10.0.2.15 via 10.0.2.2 (52:55:0a:00:02:02), dns [10.0.2.3]
```

The upstream DHCP lease comes from QEMU's user-mode network. The
downstream NIC is a `-netdev socket`, and `tools/fw-client.py` connects to
it as the `work` qube (MAC from the config) and checks the whole path:

```text
DHCP OFFER: 10.137.0.3 from 10.137.0.2
ARP: 10.137.0.2 is at 02:4a:00:00:00:02
ICMP echo reply from the gateway: 10.137.0.2 -> 10.137.0.3
ICMP echo reply via NAT: 10.0.2.2 -> 10.137.0.3
OK: DHCP, ARP, gateway ping and NAT forwarding all work
```

with the firewall reporting `stats: out 1 in 1 ... unsolicited 0`. This
harness found the first real bug: ICMP replies carry the *translated*
identifier in both directions, and the NAT keyed flows on the original
one, so every reply was dropped as unsolicited (`out 20 in 0 unsolicited
20`). Fixed in `jos-fwnet::nat` with an engine-level regression test.

On bhyve itself (JuanitOS dom0 under nested KVM, `BHYVE_UEFI.fd`), the
same image boots as `sys-firewall` behind a `sys-net` built from the
`fbsd15-net` template: both bootrom `VirtioNetDxe` NICs come up, the
config disk is read, and the upstream side takes its lease from
sys-net's dnsmasq (`upstream bound: 10.138.0.35 via 10.138.0.1`). The
bhyve test caught one difference from OVMF: the read-only flag on the
boot image (see "Guest ABI").

### Configuration delivery

Qubes uses QubesDB for this. JuanitOS has no inter-VM database yet, so the
configuration travels on a second, raw virtio-blk disk (implemented in
`crates/juanitos-core/src/firewall.rs`):

1. Each qube's TOML carries `ip`, `mac`, `firewall = ["<rule>", ...]` and
   `firewall_policy`. Rules are one-liners,
   `<allow|deny> [tcp|udp|icmp|any] [<ip|cidr|any>] [<port|a-b>]`, first
   match wins, the policy applies otherwise (default allow, as in Qubes).
   `jos create --firewall "..." --firewall-policy deny` and
   `jos firewall add|clear|policy|show` edit them.
2. Before every boot of a unikernel firewall, the supervisor's pre-boot
   hook renders the clients of that firewall into a flat text file
   (`jos firewall render <name>` shows it):

   ```text
   firewall sys-firewall
   gateway 10.137.0.2
   upstream dhcp
   client work 10.137.0.3 02:4a:5b:98:d2:60
   rule work allow tcp any 443
   policy work deny
   ```

   The text is NUL-terminated, padded to a 512-byte multiple, written to
   `<run_dir>/<name>.fwcfg` (regenerated before every boot) and attached
   at guest PCI slot 30.
   The unikernel reads the first sectors of that disk with the virtio-blk
   driver it already has; no filesystem on either side.
3. Changing rules for a running firewall means restarting it (sub-second
   for a unikernel). A live control channel over `virtio-console` can come
   later.

IP assignment: `jos create` allocates the lowest free `/32` from
`10.137.0.0/16` (`.0.1` reserved, as Qubes does) and a deterministic
locally-administered MAC (`02:4a:` + FNV-1a of the name) that bhyve pins on
the upstream NIC. The unikernel serves DHCP keyed by MAC so FreeBSD/Linux
guests get their address without any guest-side configuration.

## Beyond the firewall

The same `os = "unikernel"` mechanism suits other single-purpose domains:

- a DNS-over-TLS resolver qube,
- a WireGuard/VPN qube (`boringtun`-style, Rust),
- a `sys-usb`-like proxy once USB passthrough is wired up.

Each is just a different boot image; `jos` does not need to know what runs
inside.

## Alternatives considered

- **Hermit (RustyHermit)**: a full Rust unikernel with `std` support. More
  capable, but its bhyve support is unproven and it brings its own loader
  and build flow. Worth revisiting if a unikernel ever needs threads or
  `std`.
- **Solo5 + Rust guest**: Solo5's hypercall ABI is small and a Rust guest
  layer is feasible, but the tender is C and bhyve-side integration would
  diverge from the plain `bhyve(8)` path all other qubes use.
- **A FreeBSD VM with `pf`**: the v0.1 fallback (`--os freebsd --kind
  firewall`), fully supported by `jos` today; it just carries a complete OS
  for the job.
