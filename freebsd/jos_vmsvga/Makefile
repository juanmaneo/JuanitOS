# Out-of-tree build against the kernel's sources and drm-kmod's (the tag
# the dom0's drm-kmod package was built from; see [drm_kmod] in
# release/juanitos.toml):
#   make SYSDIR=/usr/src/sys DRMKMOD=/path/to/drm-kmod-drm_v6.12.110_1
.include "${DRMKMOD}/kconfig.mk"

KMOD=	jos_vmsvga
SRCS=	jos_vmsvga.c
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

CFLAGS+= -I${DRMKMOD}/linuxkpi/gplv2/include
CFLAGS+= -I${DRMKMOD}/linuxkpi/bsd/include
CFLAGS+= -I${SYSDIR}/compat/linuxkpi/common/include
CFLAGS+= -I${SYSDIR}/compat/linuxkpi/dummy/include
CFLAGS+= -I${DRMKMOD}/include
CFLAGS+= -I${DRMKMOD}/include/drm
CFLAGS+= -I${DRMKMOD}/include/uapi
# <drm/drm_os_freebsd.h> lives with the drm sources.
CFLAGS+= -I${DRMKMOD}/drivers/gpu

CFLAGS+= '-DKBUILD_MODNAME="${KMOD}"'
CFLAGS+= '-DLINUXKPI_PARAM_PREFIX=jos_vmsvga_' -DDRM_SYSCTL_PARAM_PREFIX=_${KMOD}
CFLAGS+= ${KCONFIG:C/(.*)/-DCONFIG_\1/}
