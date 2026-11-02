# Out-of-tree build: make SYSDIR=/path/to/usr/src/sys
KMOD=	geom_ventoy
SRCS=	g_ventoy.c

# JuanitOS: the chain blob reader for Ventoy's direct kernel boot
# (crates/jos-ventoy-chain), linked in when jos-release passes its object.
.if defined(CHAIN_OBJ)
OBJS+=		jos_ventoy_chain.o
CFLAGS+=	-DJOS_VENTOY_CHAIN

jos_ventoy_chain.o: ${CHAIN_OBJ}
	cp ${CHAIN_OBJ} ${.TARGET}
.endif

.include <bsd.kmod.mk>
