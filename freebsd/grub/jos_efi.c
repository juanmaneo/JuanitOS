/*
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * Copyright (c) 2026, Juanito and the JuanitOS contributors (see AUTHORS)
 *
 * jos_efi: boot a FreeBSD kernel from GRUB on UEFI.
 *
 * GRUB's bsd loader (kfreebsd) hands the kernel BIOS-style metadata only:
 * the SMAP, the boot flags, the environment and the end of the loaded
 * modules. A kernel booted on UEFI also expects what loader.efi passes --
 * the EFI memory map, the firmware's system table, the framebuffer and the
 * ACPI RSDP -- and without them it has no ACPI, no console and panics
 * before printing anything. GRUB 2.04 also tags a 64-bit kernel
 * "elf64 kernel", which FreeBSD 14 and later do not look for ("unable to
 * find kernel metadata").
 *
 * This module fixes the tag when it loads and adds the UEFI metadata with
 * the `jos_efi_meta` command, to be run after `kfreebsd` and
 * `kfreebsd_loadenv` and before any `kfreebsd_module*` (the kernel reads
 * the metadata in load order). Ventoy's GRUB loads it from the JuanitOS
 * ISO for the stick's "direct kernel boot" entry.
 *
 * Licensed under the GPL like GRUB: it is linked into GRUB and uses its
 * module interface. See THIRD_PARTY.md.
 */

#include <grub/command.h>
#include <grub/dl.h>
#include <grub/efi/api.h>
#include <grub/efi/efi.h>
#include <grub/efi/graphics_output.h>
#include <grub/env.h>
#include <grub/err.h>
#include <grub/file.h>
#include <grub/i386/bsd.h>
#include <grub/misc.h>
#include <grub/mm.h>
#include <grub/types.h>
#include <grub/video.h>

GRUB_MOD_LICENSE("GPLv3+");

/* sys/sys/linker.h and sys/x86/include/metadata.h. */
#define MODINFOMD_FW_HANDLE 0x000c
#define MODINFOMD_EFI_MAP 0x1004
#define MODINFOMD_EFI_FB 0x1005
#define MODINFOMD_EFI_ARCH 0x1008

/* FreeBSD reads the memory map 16 bytes after its header. */
#define EFI_MAP_HEADER_ALIGN 16
/* Descriptors the firmware may add between sizing the map and reading it. */
#define EFI_MAP_SLACK 8

/* Room for "0x" and a 64-bit address in hexadecimal. */
#define RSDP_BUF 32

/* Pixel masks of the two fixed GOP formats. */
#define MASK_BYTE0 0x000000ffU
#define MASK_BYTE1 0x0000ff00U
#define MASK_BYTE2 0x00ff0000U
#define MASK_BYTE3 0xff000000U

/* What the kernel expects behind MODINFOMD_EFI_MAP. */
struct efi_map_header {
  grub_uint64_t memory_size;
  grub_uint64_t descriptor_size;
  grub_uint32_t descriptor_version;
};

/* What the kernel expects behind MODINFOMD_EFI_FB. */
struct efi_fb {
  grub_uint64_t fb_addr;
  grub_uint64_t fb_size;
  grub_uint32_t fb_height;
  grub_uint32_t fb_width;
  grub_uint32_t fb_stride;
  grub_uint32_t fb_mask_red;
  grub_uint32_t fb_mask_green;
  grub_uint32_t fb_mask_blue;
  grub_uint32_t fb_mask_reserved;
};

/* grub_bsd_add_meta is bsd.mod's (grub/i386/bsd.h); GRUB resolves a
   module's undefined symbols against every loaded module. */
static grub_err_t add_meta(grub_uint32_t type, const void* data,
                           grub_uint32_t len) {
  return grub_bsd_add_meta(FREEBSD_MODINFO_METADATA | type, data, len);
}

static grub_err_t add_fw_handle() {
  grub_uint64_t systab = (grub_addr_t)grub_efi_system_table;

  grub_printf("jos_efi: system table at 0x%llx\n", (unsigned long long)systab);
  return add_meta(MODINFOMD_FW_HANDLE, &systab, sizeof(systab));
}

static grub_err_t add_arch() {
  return add_meta(MODINFOMD_EFI_ARCH, "amd64", sizeof("amd64"));
}

/* The configuration table is packed: compare its GUID in place. */
static int guid_eq(const void* a, const grub_efi_guid_t* b) {
  return 0 == grub_memcmp(a, b, sizeof(*b));
}

/* The RSDP goes through the environment, as loader.efi's acpi.rsdp. */
static grub_err_t set_rsdp() {
  grub_efi_guid_t acpi20 = GRUB_EFI_ACPI_20_TABLE_GUID;
  grub_efi_guid_t acpi10 = GRUB_EFI_ACPI_TABLE_GUID;
  grub_efi_configuration_table_t* table =
      grub_efi_system_table->configuration_table;
  grub_efi_uintn_t n = grub_efi_system_table->num_table_entries;
  void* rsdp = NULL;
  void* rsdp10 = NULL;
  char buf[RSDP_BUF];

  for (grub_efi_uintn_t i = 0; i < n; i++) {
    const void* guid = &table[i];
    if (guid_eq(guid, &acpi20)) {
      rsdp = table[i].vendor_table;
    } else if (guid_eq(guid, &acpi10)) {
      rsdp10 = table[i].vendor_table;
    }
  }
  if (NULL == rsdp) {
    rsdp = rsdp10;
  }
  if (NULL == rsdp) {
    return grub_error(GRUB_ERR_BAD_OS,
                      "no ACPI RSDP in the EFI configuration table");
  }
  grub_snprintf(buf, sizeof(buf), "0x%llx",
                (unsigned long long)(grub_addr_t)rsdp);
  grub_printf("jos_efi: acpi.rsdp=%s\n", buf);
  if (GRUB_ERR_NONE != grub_env_set("kFreeBSD.acpi.rsdp", buf)) {
    return grub_errno;
  }
  return grub_env_set("kFreeBSD.hint.acpi.0.rsdp", buf);
}

static grub_err_t add_framebuffer() {
  grub_efi_guid_t gop_guid = GRUB_EFI_GOP_GUID;
  struct grub_efi_gop* gop;
  struct grub_efi_gop_mode_info* info;
  struct efi_fb fb;

  /* GRUB's video driver puts the firmware's mode back when the kernel
     boots: describe that mode, not the menu's. */
  grub_video_restore();
  gop = grub_efi_locate_protocol(&gop_guid, NULL);
  if (NULL == gop || NULL == gop->mode || NULL == gop->mode->info) {
    return grub_error(GRUB_ERR_BAD_DEVICE, "no graphics output protocol");
  }
  info = gop->mode->info;
  grub_memset(&fb, 0, sizeof(fb));
  fb.fb_addr = gop->mode->fb_base;
  fb.fb_size = gop->mode->fb_size;
  fb.fb_height = info->height;
  fb.fb_width = info->width;
  fb.fb_stride = info->pixels_per_scanline;
  switch (info->pixel_format) {
  case GRUB_EFI_GOT_RGBA8:
    fb.fb_mask_red = MASK_BYTE0;
    fb.fb_mask_green = MASK_BYTE1;
    fb.fb_mask_blue = MASK_BYTE2;
    fb.fb_mask_reserved = MASK_BYTE3;
    break;
  case GRUB_EFI_GOT_BGRA8:
    fb.fb_mask_red = MASK_BYTE2;
    fb.fb_mask_green = MASK_BYTE1;
    fb.fb_mask_blue = MASK_BYTE0;
    fb.fb_mask_reserved = MASK_BYTE3;
    break;
  case GRUB_EFI_GOT_BITMASK:
    fb.fb_mask_red = info->pixel_bitmask.r;
    fb.fb_mask_green = info->pixel_bitmask.g;
    fb.fb_mask_blue = info->pixel_bitmask.b;
    fb.fb_mask_reserved = info->pixel_bitmask.a;
    break;
  default:
    return grub_error(GRUB_ERR_BAD_DEVICE, "framebuffer is blt-only");
  }
  grub_printf("jos_efi: framebuffer %ux%u stride %u at 0x%llx (%llu bytes)\n",
              fb.fb_width, fb.fb_height, fb.fb_stride,
              (unsigned long long)fb.fb_addr, (unsigned long long)fb.fb_size);
  return add_meta(MODINFOMD_EFI_FB, &fb, sizeof(fb));
}

/* Runtime services stay physically addressed (GRUB never calls
   SetVirtualAddressMap, as loader.efi with efi_disable_vmap): record that,
   since efirt(4) checks the runtime table against the virtual ranges. */
static void identity_map_runtime(grub_uint8_t* descriptors,
                                 grub_efi_uintn_t size,
                                 grub_efi_uintn_t desc_size) {
  for (grub_efi_uintn_t off = 0; off + desc_size <= size; off += desc_size) {
    grub_efi_memory_descriptor_t* m =
        (grub_efi_memory_descriptor_t*)(descriptors + off);
    if (0 != (m->attribute & GRUB_EFI_MEMORY_RUNTIME)) {
      m->virtual_start = m->physical_start;
    }
  }
}

static grub_err_t add_memory_map() {
  grub_efi_uintn_t size = 0;
  grub_efi_uintn_t key;
  grub_efi_uintn_t desc_size;
  grub_efi_uint32_t desc_version;
  grub_size_t hdr =
      ALIGN_UP(sizeof(struct efi_map_header), EFI_MAP_HEADER_ALIGN);
  grub_uint8_t* buf;
  struct efi_map_header* h;
  grub_err_t err;

  if (grub_efi_get_memory_map(&size, NULL, &key, &desc_size, &desc_version) <
      0) {
    return grub_error(GRUB_ERR_BAD_DEVICE, "cannot size the EFI memory map");
  }
  /* Room for what GRUB allocates between the two calls. */
  size += EFI_MAP_SLACK * desc_size;
  buf = grub_zalloc(hdr + size);
  if (NULL == buf) {
    return grub_errno;
  }
  if (1 != grub_efi_get_memory_map(&size, (void*)(buf + hdr), &key, &desc_size,
                                   &desc_version)) {
    grub_free(buf);
    return grub_error(GRUB_ERR_BAD_DEVICE, "cannot read the EFI memory map");
  }
  h = (struct efi_map_header*)buf;
  h->memory_size = size;
  h->descriptor_size = desc_size;
  h->descriptor_version = desc_version;
  identity_map_runtime(buf + hdr, size, desc_size);
  grub_printf("jos_efi: EFI memory map: %u descriptors of %u bytes (v%u)\n",
              (unsigned)(size / desc_size), (unsigned)desc_size, desc_version);
  err = add_meta(MODINFOMD_EFI_MAP, buf, hdr + size);
  grub_free(buf);
  return err;
}

static grub_err_t grub_cmd_jos_efi_meta(grub_command_t cmd
                                        __attribute__((unused)),
                                        int argc __attribute__((unused)),
                                        char** argv __attribute__((unused))) {
  grub_err_t err;

  err = add_fw_handle();
  if (GRUB_ERR_NONE != err) {
    return err;
  }
  err = add_arch();
  if (GRUB_ERR_NONE != err) {
    return err;
  }
  err = set_rsdp();
  if (GRUB_ERR_NONE != err) {
    return err;
  }
  /* No framebuffer is a text console, not a failure. */
  if (GRUB_ERR_NONE != add_framebuffer()) {
    grub_print_error();
    grub_errno = GRUB_ERR_NONE;
  }
  return add_memory_map();
}

/* GRUB tags a 64-bit kernel "elf64 kernel"; FreeBSD 14 and later only look
   for "elf kernel" and panic before the console exists ("unable to find
   kernel metadata"). The string is a literal in bsd.mod: rewrite it in the
   loaded module. */
static grub_err_t fix_kernel_type() {
  static const char old[] = "elf64 kernel";
  static const char new[] = "elf kernel";
  grub_dl_t bsd = grub_dl_get("bsd");
  int patched = 0;

  if (NULL == bsd) {
    return grub_error(GRUB_ERR_BAD_MODULE, "bsd module not loaded");
  }
  for (grub_dl_segment_t seg = bsd->segment; NULL != seg; seg = seg->next) {
    char* p = seg->addr;
    if (NULL == p || seg->size < sizeof(old)) {
      continue;
    }
    for (grub_size_t i = 0; i + sizeof(old) <= seg->size; i++) {
      if (0 != grub_memcmp(p + i, old, sizeof(old))) {
        continue;
      }
      grub_memset(p + i, 0, sizeof(old));
      grub_memcpy(p + i, new, sizeof(new));
      patched++;
    }
  }
  if (0 == patched) {
    return grub_error(GRUB_ERR_BAD_MODULE,
                      "kernel type string not found in bsd.mod");
  }
  return GRUB_ERR_NONE;
}

static grub_command_t cmd;

GRUB_MOD_INIT(jos_efi) {
  if (GRUB_ERR_NONE != fix_kernel_type()) {
    grub_print_error();
    grub_errno = GRUB_ERR_NONE;
  }
  cmd = grub_register_command(
      "jos_efi_meta", grub_cmd_jos_efi_meta, NULL,
      "Add UEFI boot metadata to the loaded FreeBSD kernel.");
}

GRUB_MOD_FINI(jos_efi) { grub_unregister_command(cmd); }
