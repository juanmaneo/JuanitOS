# SPDX-License-Identifier: BSD-3-Clause
# The build of a jos_kmsfb driver (freebsd/jos_vmsvga):
# its Makefile sets KMOD, SRCS and KMSFB (this directory, by default its
# sibling; jos-release passes it when it builds a copy), then includes
# this. Out of tree, against the kernel's sources and drm-kmod's (the tag
# the dom0's drm-kmod package was built from; see [drm_kmod] in
# release/juanitos.toml):
#   make SYSDIR=/usr/src/sys DRMKMOD=/path/to/drm-kmod-drm_v6.12.110_1

.include "${DRMKMOD}/kconfig.mk"

.PATH:	${KMSFB}
SRCS+=	jos_kmsfb.c
SRCS+=	device_if.h \
	vnode_if.h \
	bus_if.h \
	pci_if.h \
	pci_iov_if.h \
	iicbus_if.h \
	opt_acpi.h \
	opt_drm.h \
	opt_platform.h \
	opt_splash.h \
	opt_syscons.h \
	opt_teken.h \
	opt_vm.h
# C23: `f()` is a prototype, as `f(void)` was (see .clang-tidy).
CSTD=	gnu23

.include <bsd.kmod.mk>

# As drm-kmod's own module Makefiles: included after <bsd.kmod.mk> because
# they expand variables it defines.
.include "${DRMKMOD}/linuxkpi_version.mk"
.include "${DRMKMOD}/compiler_flags.mk"

CFLAGS+= -I${KMSFB}
# System headers (-isystem), as `cargo xtask lint` sees them: their own
# warnings (linuxkpi's iosys-map.h does arithmetic on void pointers) are
# not ours, while every warning in our sources stays an error.
CFLAGS+= -isystem ${DRMKMOD}/linuxkpi/gplv2/include
CFLAGS+= -isystem ${DRMKMOD}/linuxkpi/bsd/include
CFLAGS+= -isystem ${SYSDIR}/compat/linuxkpi/common/include
CFLAGS+= -isystem ${SYSDIR}/compat/linuxkpi/dummy/include
CFLAGS+= -isystem ${DRMKMOD}/include
CFLAGS+= -isystem ${DRMKMOD}/include/drm
CFLAGS+= -isystem ${DRMKMOD}/include/uapi
# <drm/drm_os_freebsd.h> lives with the drm sources.
CFLAGS+= -isystem ${DRMKMOD}/drivers/gpu

CFLAGS+= '-DKBUILD_MODNAME="${KMOD}"'
CFLAGS+= '-DLINUXKPI_PARAM_PREFIX=${KMOD}_' -DDRM_SYSCTL_PARAM_PREFIX=_${KMOD}
CFLAGS+= ${KCONFIG:C/(.*)/-DCONFIG_\1/}
