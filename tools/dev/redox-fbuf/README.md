# Redox OS under bhyve: `fbuf` kills the VM

A Redox OS guest with bhyve's `fbuf` display device aborts the whole VM a
few seconds into the boot:

```
Assertion failed: (error == 0), function modify_bar_registration, file /usr/src/usr.sbin/bhyve/pci_emul.c, line 708.
```

This directory has what is needed to show it again and to report it:

| File | What |
|------|------|
| `repro.sh` | `fetch` the Redox `server` nightly, `run` it with `fbuf` (aborts), `control` without (boots to `login:`), `trace` with an instrumented bhyve |
| `instrument-bhyve.pl` | the prints added to `usr.sbin/bhyve` (`pci_emul.c`, `mem.c`) for `trace` |
| `trace-2026-10-15.txt` | the trace captured on 2026-10-15 (FreeBSD 15.1-RELEASE, edk2-bhyve g202508_3, Redox nightly 2026-10-14 build 553) |

Runs on FreeBSD as root: `tools/dev/freebsd-vm.sh` gives a build VM with
bhyve working under nested KVM.

```sh
tools/dev/freebsd-vm.sh scp freebsd -r tools/dev/redox-fbuf root@127.0.0.1:/root/
tools/dev/freebsd-vm.sh ssh freebsd 'pkg install -y edk2-bhyve zstd; cd /root/redox-fbuf && ./repro.sh all'
tools/dev/freebsd-vm.sh ssh freebsd 'cd /root/redox-fbuf && ./repro.sh trace'   # needs /usr/src (fetched if absent)
```

Expected: `run` ends with `REPRODUCED:` and the assertion; `control` ends
with `CONTROL OK`; `trace` prints the BAR traffic of `fbuf@pci.0.4.0`
ending in `overlap detected ... 'kern-hpet-mmio'`.

## What happens

bhyve's `fbuf` is a PCI display with two 32-bit memory BARs: BAR0, 128
bytes of mode registers, and BAR1, the 32 MiB framebuffer. The trace shows
the UEFI firmware (edk2) sizing both BARs the standard way — command
register cleared first (`coff=0x4 val=0x0`), `0xffffffff` written and the
value read back, the BAR restored — and then assigning BAR0 at
`0xc2002000`, BAR1 at `0xc0000000`, with decoding enabled (`cmd=0x17`).

Redox's `pcid` then enumerates the bus and sizes the BARs again, through
`pci_types::EndpointHeader::bar()`, **without disabling I/O and memory
decoding in the command register**:

```
235  cfg-debug: fbuf@pci.0.4.0 write coff=0x10 bytes=4 val=0xffffffff   <- size BAR0, cmd still 0x17
236  bar-debug: fbuf@pci.0.4.0 idx=0 unregister base=0xc2002000 size=0x80 cmd=0x17 error=0
237  bar-debug: fbuf@pci.0.4.0 idx=0 register base=0xffffff80 size=0x80 cmd=0x17 error=0
238  cfg-debug: fbuf@pci.0.4.0 write coff=0x10 bytes=4 val=0xc2002000   <- restore BAR0
...
241  cfg-debug: fbuf@pci.0.4.0 write coff=0x14 bytes=4 val=0xffffffff   <- size BAR1
242  bar-debug: fbuf@pci.0.4.0 idx=1 unregister base=0xc0000000 size=0x2000000 cmd=0x17 error=0
243  overlap detected: new fe000000:ffffffff, tree fed00000:fed003ff, 'fbuf@pci.0.4.0' claims region already claimed for 'kern-hpet-mmio'
245  bar-debug: fbuf@pci.0.4.0 idx=1 register base=0xfe000000 size=0x2000000 cmd=0x17 error=17
246  Assertion failed: (error == 0), function modify_bar_registration, ...
```

Because decoding is still enabled, bhyve treats each sizing write as the
device moving: it unregisters the BAR's MMIO range and registers the new
one (`update_bar_address` → `modify_bar_registration`). For the 32 MiB
BAR1 the sizing value decodes to `0xfe000000`, so the device transiently
claims `[0xfe000000, 0xffffffff]`, which contains the HPET at
`0xfed00000`. `register_mem` refuses the overlap (`EEXIST`) and
`modify_bar_registration` asserts on it, taking the VM down. The 128-byte
BAR0 gets away with it only because `[0xffffff80, 0xffffffff]` happens to
overlap nothing bhyve tracks in user space.

Nothing else in the VM does this: FreeBSD's `pci_add_map()` ("Disable
decoding via the command register before determining the BAR's length
since we will be placing it in a weird state"), Linux's `__pci_read_base()`
(`PCI_COMMAND_DECODE_ENABLE` cleared, "No printks while decoding is
disabled!") and edk2 (seen in the trace) all clear the decode bits first,
as the PCI Local Bus Specification 3.0 §6.2.5.1 implementation note on
sizing a Base Address Register spells out: "Decode (I/O or memory) of a
register is disabled via the command register before sizing a Base
Address register."

## Whose bug

The sizing sequence is the guest's. `pci_types` (the `pci_types` crate,
`EndpointHeader::bar()`) writes `0xffffffff` to a live BAR with decoding
on; on real hardware the device would, for a moment, answer for the top
32 MiB of the 4 GiB space over the HPET and the local APIC. The fix
belongs there (and so in Redox, which uses the crate unpatched in `pcid`):
clear `CommandRegister::MEMORY_ENABLE | IO_ENABLE` around the sizing
writes and restore the original command afterwards, the way
`__pci_read_base()` does. A one-function change; `pcid` already imports
`CommandRegister`.

bhyve has a smaller, secondary issue: `modify_bar_registration()` aborts
the hypervisor process on a failed registration instead of tolerating a
guest-chosen overlap (a guest can kill its own VM this way, nothing more;
QEMU's memory API resolves such overlaps by priority and carries on).
That is a robustness improvement for bhyve, not a fix for Redox: with the
guest corrected the assertion is never reached.

Decision for JuanitOS: leave Redox console-only until `pci_types`/`pcid`
disable decoding while sizing; report upstream with this directory. bhyve
is not patched (JuanitOS ships FreeBSD's binary packages, and the
hypervisor is not at fault).

## Also seen (not analysed)

In the same boot, before `pcid` runs, Redox's `vesad` (the driver for the
framebuffer the bootloader found at `0xc0000000`, i.e. BAR1) panics —
`called Option::unwrap() on a None value`, `drivers/graphics/vesad/src/main.rs:61`
in the 2026-10-14 nightly — and aborts (`Invalid opcode fault` is relibc's
`abort()`). `fbbootlogd` then reports "No display present yet". That is
the `server` image, which may simply not expect a display; it is separate
from the BAR issue and is recorded in `repro.sh run`'s console log.

## Upstream report (ready to paste)

**Title:** pcid sizes BARs with memory/IO decoding enabled; kills bhyve VMs

`pci_types::EndpointHeader::bar()` (used by `pcid` for every device) sizes
a BAR by writing `0xffffffff` and reading it back while the function's
command register still has Memory Space and I/O Space enabled. The PCI
spec (3.0 §6.2.5.1, implementation note) and every other OS (FreeBSD
`pci_add_map`, Linux `__pci_read_base`, edk2) disable decoding first,
because the BAR transiently decodes a huge range at the top of the
address space.

Under FreeBSD bhyve this is fatal: the hypervisor tracks a decoded BAR as
a registered MMIO range, the 32 MiB framebuffer BAR of the `fbuf` display
device transiently claims `0xfe000000-0xffffffff` over the HPET, the
registration fails and bhyve aborts (`Assertion failed: (error == 0),
function modify_bar_registration, pci_emul.c`), so the Redox VM dies as
soon as `pcid` reaches the display device. Reproducer, trace of the config
writes and analysis: this directory (`repro.sh`, `trace-2026-10-15.txt`).

Suggested fix: in `bar()` read the command register, clear
`MEMORY_ENABLE | IO_ENABLE`, do the sizing writes, write the original
command back (`pci_types` already has `update_command`).
