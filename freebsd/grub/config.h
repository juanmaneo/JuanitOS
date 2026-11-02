/*
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * Copyright (c) 2026, Juanito and the JuanitOS contributors (see AUTHORS)
 *
 * What GRUB's configure writes into config.h for the x86_64-efi target,
 * reduced to what the headers jos_efi.c includes read. The module is built
 * against GRUB's pristine source tree (its include/ directory), which has
 * no configured config.h.
 */

#ifndef JOS_EFI_CONFIG_H
#define JOS_EFI_CONFIG_H 1

#define GRUB_TARGET_CPU "x86_64"
#define GRUB_PLATFORM "efi"

/* No leading underscore on C symbols (ELF). */
#define HAVE_ASM_USCORE 0
/* The linker script's symbols; unused by a module. */
#define BSS_START_SYMBOL
#define END_SYMBOL

#define BOOT_TIME_STATS 0
#define DISK_CACHE_STATS 0
#define HAVE_FONT_SOURCE 0

#define PACKAGE "grub"
#define PACKAGE_NAME "GRUB"
#define PACKAGE_VERSION "2.04"
#define PACKAGE_STRING "GRUB 2.04"
#define VERSION "2.04"

#endif /* JOS_EFI_CONFIG_H */
