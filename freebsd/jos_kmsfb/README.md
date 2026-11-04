<!-- SPDX-License-Identifier: BSD-3-Clause -->
# jos_kmsfb

The KMS code of JuanitOS's frame buffer drivers (`freebsd/jos_vmsvga`):
dumb buffers in system memory that user space maps, one connector, CRTC
and XRGB8888 primary plane, atomic commits with damage, and the fbdev
console with a timer that pushes a mapped console shadow to the screen.
A driver only says how to turn its screen on and off and what to do once
damaged rows are copied (`struct jos_kmsfb_funcs`). Design notes are at
the top of `jos_kmsfb.h`.

It is not a module of its own: `kmsfb.mk` compiles `jos_kmsfb.c` into
each driver, so there is no symbol interface between modules to keep
stable. A driver's Makefile is:

```make
KMOD=	jos_vmsvga
SRCS=	jos_vmsvga.c

KMSFB?=	${.CURDIR}/../jos_kmsfb
.include "${KMSFB}/kmsfb.mk"
```

Original code (BSD-3-Clause), written against drm-kmod `drm_v6.12.110_1`.
