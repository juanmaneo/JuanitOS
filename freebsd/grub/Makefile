# SPDX-License-Identifier: GPL-3.0-or-later
#
# jos_efi.mod, a GRUB module (see README.md). Out-of-tree build against an
# unpacked GRUB source tree (only its include/ directory is read):
#
#   make GRUB=/path/to/grub-2.04
#
# Works with bmake (FreeBSD) and GNU make, with FreeBSD's base clang, lld
# and llvm-objcopy as well as gcc and GNU binutils; CC, LD, OBJCOPY and
# STRIP may be overridden. The compiler flags are in compile_flags.txt,
# which clang-tidy (cargo xtask lint) reads too.

GRUB?=		../grub-2.04
CC?=		cc
LD?=		ld
OBJCOPY?=	objcopy
STRIP?=		strip
CFLAGS=		-Os
MOD=		jos_efi

all: $(MOD).mod

# GRUB's configure points include/grub/cpu and include/grub/machine at the
# target's directories; done here, leaving the GRUB tree untouched.
include/grub/machine:
	mkdir -p include/grub
	cp -R $(GRUB)/include/grub/x86_64 include/grub/cpu
	cp -R $(GRUB)/include/grub/x86_64/efi include/grub/machine

$(MOD).o: $(MOD).c config.h compile_flags.txt include/grub/machine
	$(CC) $$(cat compile_flags.txt) $(CFLAGS) -isystem include \
	    -isystem $(GRUB)/include -c -o $(MOD).o $(MOD).c

# What GRUB's genmod.sh does: a relocatable object with the module's name
# and its dependencies in their own sections, stripped of what GRUB's
# loader does not read.
$(MOD).mod: $(MOD).o
	$(LD) -r -o $(MOD).module $(MOD).o
	printf '$(MOD)\0' > modname.bin
	printf 'bsd\0video\0' > moddeps.bin
	$(OBJCOPY) --add-section .modname=modname.bin \
	    --add-section .moddeps=moddeps.bin $(MOD).module $(MOD).mod
	$(STRIP) --strip-unneeded -K grub_mod_init -K grub_mod_fini \
	    -R .note.GNU-stack -R .note.gnu.property -R .note -R .comment \
	    -R .eh_frame -R .rela.eh_frame -R .llvm_addrsig $(MOD).mod

clean:
	rm -rf $(MOD).o $(MOD).module $(MOD).mod modname.bin moddeps.bin include

.PHONY: all clean
