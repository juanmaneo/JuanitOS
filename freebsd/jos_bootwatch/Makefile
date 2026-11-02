# Out-of-tree build: make SYSDIR=/path/to/usr/src/sys
KMOD=	jos_bootwatch
SRCS=	jos_bootwatch.c
# C23: `f()` is a prototype, as `f(void)` was (see .clang-tidy).
CSTD=	gnu23

.include <bsd.kmod.mk>
