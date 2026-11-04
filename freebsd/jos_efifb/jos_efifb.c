/*-
 * SPDX-License-Identifier: BSD-3-Clause
 *
 * Copyright (c) 2026, Juanito and the JuanitOS contributors (see AUTHORS)
 *
 * jos_efifb: a KMS driver for the frame buffer the UEFI firmware left on
 * screen (its Graphics Output Protocol, handed to the kernel by the loader
 * as MODINFOMD_EFI_FB, the one vt(4)'s efifb draws the console on). It is
 * the fallback for a display that no drm-kmod driver drives: the GPU is
 * too new for drm-kmod, its firmware is missing, its driver failed to
 * attach. The compositor then gets a KMS device all the same, rendered in
 * software, at the firmware's resolution, on the monitor the firmware lit.
 *
 * The KMS side is jos_kmsfb's: one fixed mode (the firmware's), and every
 * commit copies its damaged rows to the frame buffer, mapped
 * write-combining. Nothing else is needed: the firmware already scans the
 * frame buffer out, and it stays that way until a real driver reprograms
 * the display, which is why a real driver cannot take over until the next
 * boot.
 *
 * LinuxKPI matches a driver to a device by its PCI identity, so at load
 * time the module looks for the display device whose memory BAR holds the
 * frame buffer and names exactly that device in its ID table; the probe
 * then checks it is that very device (twin GPUs share their identity). A
 * LinuxKPI driver loaded later goes first in LinuxKPI's list, so a real
 * driver loaded while this one is (by hand) does not attach; rc.d/jos-kms
 * loads this one only when the real one has not given a KMS device.
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/bus.h>
#include <sys/kernel.h>
#include <sys/linker.h>
#include <sys/module.h>

#include <machine/metadata.h>

#include <linux/io.h>
#include <linux/module.h>
#include <linux/pci.h>

#include <drm/drm_drv.h>
#include <drm/drm_print.h>

#include "jos_kmsfb.h"

#define DRIVER_NAME "jos_efifb"
#define DRIVER_DESC "UEFI firmware frame buffer (JuanitOS)"
#define DRIVER_DATE "20261104"
#define DRIVER_MAJOR 1
#define DRIVER_MINOR 0

/* The only layout kept: XRGB8888, what x86 firmware uses. */
#define JOS_EFIFB_MASK_RED 0x00ff0000U
#define JOS_EFIFB_MASK_GREEN 0x0000ff00U
#define JOS_EFIFB_MASK_BLUE 0x000000ffU

/* The firmware's frame buffer, from the loader's metadata. */
struct jos_efifb_mode {
  u64 addr;
  u64 size;
  u32 width;
  u32 height;
  u32 pitch;
};

struct jos_efifb {
  struct jos_kmsfb kms;
  u8 __iomem* fb;
};

/* Set at load: the frame buffer and the display device that holds it. */
static struct jos_efifb_mode jos_efifb_mode;
static device_t jos_efifb_display;

/* That device's identity; LinuxKPI stops at the zero vendor. */
static struct pci_device_id jos_efifb_pci_ids[] = {
    {0},
    {0},
};

/*
 * The firmware's frame buffer: 0, or ENXIO when the loader passed none
 * that this driver can show.
 */
static int jos_efifb_read_mode(struct jos_efifb_mode* mode) {
  const struct efi_fb* efifb = (const struct efi_fb*)preload_search_info(
      preload_kmdp, MODINFO_METADATA | MODINFOMD_EFI_FB);
  u64 size;

  if (NULL == efifb || 0 == efifb->fb_addr) {
    printf(DRIVER_NAME ": the loader passed no UEFI frame buffer\n");
    return ENXIO;
  }
  if (JOS_EFIFB_MASK_RED != efifb->fb_mask_red ||
      JOS_EFIFB_MASK_GREEN != efifb->fb_mask_green ||
      JOS_EFIFB_MASK_BLUE != efifb->fb_mask_blue) {
    printf(DRIVER_NAME ": the frame buffer is not XRGB8888 (red %#x, "
                       "green %#x, blue %#x)\n",
           efifb->fb_mask_red, efifb->fb_mask_green, efifb->fb_mask_blue);
    return ENXIO;
  }
  size = (u64)efifb->fb_stride * JOS_KMSFB_CPP * efifb->fb_height;
  if (0 == efifb->fb_width || 0 == efifb->fb_height ||
      efifb->fb_stride < efifb->fb_width ||
      (0 != efifb->fb_size && size > efifb->fb_size)) {
    printf(DRIVER_NAME ": the frame buffer's geometry is inconsistent "
                       "(%ux%u, stride %u, %ju bytes)\n",
           efifb->fb_width, efifb->fb_height, efifb->fb_stride,
           (uintmax_t)efifb->fb_size);
    return ENXIO;
  }
  mode->addr = efifb->fb_addr;
  mode->size = size;
  mode->width = efifb->fb_width;
  mode->height = efifb->fb_height;
  mode->pitch = efifb->fb_stride * JOS_KMSFB_CPP;
  return 0;
}

/* Does one of the memory BARs of `dev` hold the address `addr`? */
static bool jos_efifb_holds(device_t dev, u64 addr) {
  rman_res_t start;
  rman_res_t count;
  int bar;

  for (bar = 0; bar <= PCIR_MAX_BAR_0; bar++) {
    if (0 == bus_get_resource(dev, SYS_RES_MEMORY, PCIR_BAR(bar), &start,
                              &count) &&
        addr >= start && addr - start < count) {
      return true;
    }
  }
  return false;
}

/* The display device (vgapci) whose BAR holds `addr`, or NULL. */
static device_t jos_efifb_find_display(u64 addr) {
  devclass_t dc;
  device_t found = NULL;
  int unit;

  bus_topo_lock();
  dc = devclass_find("vgapci");
  if (NULL != dc) {
    for (unit = 0; unit < devclass_get_maxunit(dc) && NULL == found; unit++) {
      device_t dev = devclass_get_device(dc, unit);

      if (NULL != dev && jos_efifb_holds(dev, addr)) {
        found = dev;
      }
    }
  }
  bus_topo_unlock();
  return found;
}

static int jos_efifb_setup() {
  device_t dev;
  int error;

  error = jos_efifb_read_mode(&jos_efifb_mode);
  if (0 != error) {
    return error;
  }
  dev = jos_efifb_find_display(jos_efifb_mode.addr);
  if (NULL == dev) {
    printf(DRIVER_NAME ": no display device holds the frame buffer at "
                       "%#jx\n",
           (uintmax_t)jos_efifb_mode.addr);
    return ENXIO;
  }
  jos_efifb_display = dev;
  jos_efifb_pci_ids[0].vendor = pci_get_vendor(dev);
  jos_efifb_pci_ids[0].device = pci_get_device(dev);
  jos_efifb_pci_ids[0].subvendor = pci_get_subvendor(dev);
  jos_efifb_pci_ids[0].subdevice = pci_get_subdevice(dev);
  return 0;
}

/*
 * The driver.
 */

/*
 * No hooks: the firmware scans the frame buffer out in its own mode, and
 * jos_kmsfb's copy is all a commit needs.
 */
static const struct jos_kmsfb_funcs jos_efifb_funcs;

static const struct drm_driver jos_efifb_driver = {
    JOS_KMSFB_DRIVER_OPS, .name = DRIVER_NAME,   .desc = DRIVER_DESC,
    .date = DRIVER_DATE,  .major = DRIVER_MAJOR, .minor = DRIVER_MINOR,
};

static int jos_efifb_pci_probe(struct pci_dev* pdev,
                               const struct pci_device_id* id __unused) {
  const struct jos_efifb_mode* mode = &jos_efifb_mode;
  struct jos_efifb* sc;
  int ret;

  /* The drmn device LinuxKPI probes is a child of the vgapci one. */
  if (device_get_parent(pdev->dev.bsddev) != jos_efifb_display) {
    return -ENODEV;
  }
  sc = JOS_KMSFB_PCI_ALLOC(pdev, &jos_efifb_driver, struct jos_efifb);
  if (IS_ERR(sc)) {
    return (int)PTR_ERR(sc);
  }
  sc->fb = ioremap_wc(mode->addr, mode->size);
  if (NULL == sc->fb) {
    return -ENOMEM;
  }
  sc->kms.max_width = mode->width;
  sc->kms.max_height = mode->height;
  sc->kms.mem_size = mode->size;
  sc->kms.fixed = true;
  sc->kms.screen = sc->fb;
  sc->kms.pitch = mode->pitch;
  ret = jos_kmsfb_register(&sc->kms, &jos_efifb_funcs);
  if (0 != ret) {
    iounmap(sc->fb);
    return ret;
  }
  drm_info(&sc->kms.drm,
           "%ux%u UEFI frame buffer at %#jx, rendered in "
           "software\n",
           mode->width, mode->height, (uintmax_t)mode->addr);
  return 0;
}

static void jos_efifb_pci_remove(struct pci_dev* pdev) {
  struct jos_efifb* sc =
      container_of(jos_kmsfb_of_pci(pdev), struct jos_efifb, kms);

  jos_kmsfb_unregister(&sc->kms);
  iounmap(sc->fb);
}

static struct pci_driver jos_efifb_pci_driver = {
    /* LinuxKPI attaches a driver of this name to the display device. */
    .name = "drmn",
    .id_table = jos_efifb_pci_ids,
    .probe = jos_efifb_pci_probe,
    .remove = jos_efifb_pci_remove,
    /* Below any other driver of the display device (nvidia, say). */
    .bsd_probe_return = BUS_PROBE_LOW_PRIORITY,
};

/*
 * No PNP information: the module must never load on its own (see the
 * top of the file); rc.d/jos-kms loads it from jos_kms_fallback.
 */
static int jos_efifb_modevent(module_t mod __unused, int type,
                              void* data __unused) {
  int error;

  switch (type) {
  case MOD_LOAD:
    error = jos_efifb_setup();
    if (0 != error) {
      return error;
    }
    return -pci_register_driver(&jos_efifb_pci_driver);
  case MOD_UNLOAD:
    pci_unregister_driver(&jos_efifb_pci_driver);
    return 0;
  default:
    return EOPNOTSUPP;
  }
}

static moduledata_t jos_efifb_mod = {DRIVER_NAME, jos_efifb_modevent, NULL};
DECLARE_MODULE(jos_efifb, jos_efifb_mod, SI_SUB_DRIVERS, SI_ORDER_ANY);
MODULE_VERSION(jos_efifb, 1);
MODULE_DEPEND(jos_efifb, drmn, 2, 2, 2);
MODULE_DEPEND(jos_efifb, linuxkpi, 1, 1, 1);
