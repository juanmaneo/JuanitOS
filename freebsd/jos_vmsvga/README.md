<!-- SPDX-License-Identifier: BSD-3-Clause -->
# jos_vmsvga

KMS driver for the VMware SVGA II display device: VirtualBox's VMSVGA
adapter and QEMU's `-vga vmware`. drm-kmod has no driver for it, so
without this module `cosmic-comp` finds no DRM device in those VMs and
the desktop cannot start. It is a plain frame buffer driver (one
connector, CRTC and primary plane, dumb buffers, atomic commits with
damage); Mesa renders with `kms_swrast`. Design notes are at the top of
`jos_vmsvga.c`.

Original code (BSD-3-Clause), not a port of Linux's vmwgfx; the register
and FIFO constants in `svga_reg.h` follow VMware's MIT-licensed
`svga_reg.h` (THIRD_PARTY.md).

Status: builds against drm-kmod `drm_v6.12.110_1` with the base system's
clang and passes `cargo xtask lint`. Loaded in a FreeBSD 15.1 VM under
QEMU (`-vga vmware`), it attaches as `drmn0` and takes over the console
from efifb (1920x1080, 32 bpp), and it unloads cleanly. It is not yet
built or shipped by `jos-release`.

By hand, on FreeBSD, against the drm-kmod tag in `[drm_kmod]` of
`release/juanitos.toml` (the one the dom0's drm-kmod package is built
from):

```sh
make SYSDIR=/usr/src/sys DRMKMOD=/path/to/drm-kmod-drm_v6.12.110_1
kldload ./jos_vmsvga.ko
```

The module has no PNP information on purpose: devmatch would load it
with a console message that ends the boot splash. `rc.d/jos-kms` loads
the module named by `jos_kms_module`.
