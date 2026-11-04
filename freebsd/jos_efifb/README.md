<!-- SPDX-License-Identifier: BSD-3-Clause -->
# jos_efifb

KMS driver for the frame buffer the UEFI firmware left on screen (its
Graphics Output Protocol, which the loader hands to the kernel and on
which vt(4)'s efifb draws the console). drm-kmod has no simpledrm or
efidrm, so a GPU that no drm-kmod driver drives (too new for drm-kmod,
firmware missing, an attach that failed) would leave `cosmic-comp` with
no KMS device and no desktop. With this module it gets one, on
`freebsd/jos_kmsfb`: a single fixed mode (the firmware's), dumb buffers,
no render node; `jos-session` sets `GBM_ALWAYS_SOFTWARE=1` and Mesa
renders with `kms_swrast`. Design notes are at the top of `jos_efifb.c`.

It is a fallback, with the limits of one:

- the firmware's resolution, on the monitor the firmware lit;
- rendering on the CPU;
- UEFI boots only (a BIOS boot has no GOP frame buffer);
- the frame buffer must be XRGB8888 (what x86 firmware uses);
- a real driver loaded later does not take the display over: that needs
  a reboot.

`rc.d/jos-kms` loads it (`jos_kms_fallback="jos_efifb"`, which the
installer writes for a system with the desktop) when the driver named by
`jos_kms_module` has not made `/dev/dri/card0` within a few seconds, or
when there is none; `jos doctor` then warns that the desktop renders in
software. At load it finds the display device whose memory BAR holds the
frame buffer and attaches to that device only, below any other driver.

Original code (BSD-3-Clause). `jos-release` builds it against the
drm-kmod tag in `[drm_kmod]` of `release/juanitos.toml` and ships it as
`/boot/modules/jos_efifb.ko`. By hand, on FreeBSD:

```sh
make SYSDIR=/usr/src/sys DRMKMOD=/path/to/drm-kmod-drm_v6.12.110_1
kldload ./jos_efifb.ko
```
