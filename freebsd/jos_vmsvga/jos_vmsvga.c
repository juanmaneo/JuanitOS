/*-
 * SPDX-License-Identifier: BSD-3-Clause
 *
 * Copyright (c) 2026, Juanito and the JuanitOS contributors (see AUTHORS)
 *
 * jos_vmsvga: a KMS driver for the VMware SVGA II display device
 * (VirtualBox's VMSVGA adapter, QEMU's -vga vmware) on FreeBSD's drm-kmod.
 *
 * The device is a dumb frame buffer with a command FIFO: the guest writes
 * pixels into VRAM (BAR 1) and tells the host which rectangles changed
 * with SVGA_CMD_UPDATE through the FIFO (BAR 2); registers are reached
 * through an index/value port pair (BAR 0). The KMS side (buffers, planes,
 * the console) is jos_kmsfb's: it copies the damaged rows of a commit to
 * VRAM, and this driver sends one UPDATE per damaged rectangle.
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/kernel.h>
#include <sys/module.h>

#include <machine/cpufunc.h>

#include <linux/delay.h>
#include <linux/io.h>
#include <linux/module.h>
#include <linux/pci.h>

#include <drm/drm_drv.h>
#include <drm/drm_print.h>

#include "jos_kmsfb.h"
#include "svga_reg.h"

#define DRIVER_NAME "jos_vmsvga"
#define DRIVER_DESC "VMware SVGA II display (JuanitOS)"
#define DRIVER_DATE "20261104"
#define DRIVER_MAJOR 1
#define DRIVER_MINOR 1

/* The mode the console and the greeter come up in, when VRAM allows. */
#define JOS_VMSVGA_PREFERRED_WIDTH 1920
#define JOS_VMSVGA_PREFERRED_HEIGHT 1080

/* Waiting for the host to drain the FIFO: polls of this many µs, at most. */
#define JOS_VMSVGA_SYNC_POLL_US 20
#define JOS_VMSVGA_SYNC_POLLS 5000

struct jos_vmsvga {
  struct jos_kmsfb kms;
  u_int io;
  u8 __iomem* vram;
  u32 __iomem* fifo;
  u32 vram_size;
  u32 fifo_size;
  u32 caps;
  bool host_stuck;
};

static struct jos_vmsvga* to_jos_vmsvga(struct jos_kmsfb* kms) {
  return container_of(kms, struct jos_vmsvga, kms);
}

/*
 * Registers and FIFO.
 */

/*
 * linuxkpi's <asm-generic/io.h> turns outl() into Linux's: value first, then
 * port (FreeBSD's <machine/cpufunc.h> has the port first).
 */
static u32 jos_vmsvga_rd(const struct jos_vmsvga* sc, u32 reg) {
  outl(reg, sc->io + SVGA_INDEX_PORT);
  return inl(sc->io + SVGA_VALUE_PORT);
}

static void jos_vmsvga_wr(const struct jos_vmsvga* sc, u32 reg, u32 value) {
  outl(reg, sc->io + SVGA_INDEX_PORT);
  outl(value, sc->io + SVGA_VALUE_PORT);
}

static u32 jos_vmsvga_fifo_rd(const struct jos_vmsvga* sc, u32 index) {
  return ioread32(sc->fifo + index);
}

static void jos_vmsvga_fifo_wr(const struct jos_vmsvga* sc, u32 index,
                               u32 value) {
  iowrite32(value, sc->fifo + index);
}

/*
 * Ask the host to process the FIFO and wait until it has. False when it
 * did not within the bound; said once, then the driver keeps going
 * without waiting (the screen is late, the kernel is not stuck).
 */
static bool jos_vmsvga_sync(struct jos_vmsvga* sc) {
  int polls;

  jos_vmsvga_wr(sc, SVGA_REG_SYNC, 1);
  for (polls = 0; polls < JOS_VMSVGA_SYNC_POLLS; polls++) {
    if (0 == jos_vmsvga_rd(sc, SVGA_REG_BUSY)) {
      return true;
    }
    udelay(JOS_VMSVGA_SYNC_POLL_US);
  }
  if (!sc->host_stuck) {
    sc->host_stuck = true;
    drm_warn(&sc->kms.drm, "the host does not drain the FIFO\n");
  }
  return false;
}

/* Bytes free in the command ring; one dword always stays unused. */
static u32 jos_vmsvga_fifo_free(const struct jos_vmsvga* sc) {
  u32 min = jos_vmsvga_fifo_rd(sc, SVGA_FIFO_MIN);
  u32 max = jos_vmsvga_fifo_rd(sc, SVGA_FIFO_MAX);
  u32 next = jos_vmsvga_fifo_rd(sc, SVGA_FIFO_NEXT_CMD);
  u32 stop = jos_vmsvga_fifo_rd(sc, SVGA_FIFO_STOP);
  u32 used;

  if (next >= stop) {
    used = next - stop;
  } else {
    used = (max - stop) + (next - min);
  }
  return (max - min) - used - sizeof(u32);
}

/* Append one command; a ring too full for it is drained first. */
static void jos_vmsvga_fifo_cmd(struct jos_vmsvga* sc, const u32* words,
                                u32 count) {
  u32 min;
  u32 max;
  u32 next;
  u32 i;

  while (jos_vmsvga_fifo_free(sc) < count * sizeof(u32)) {
    if (!jos_vmsvga_sync(sc)) {
      return;
    }
  }
  min = jos_vmsvga_fifo_rd(sc, SVGA_FIFO_MIN);
  max = jos_vmsvga_fifo_rd(sc, SVGA_FIFO_MAX);
  next = jos_vmsvga_fifo_rd(sc, SVGA_FIFO_NEXT_CMD);
  for (i = 0; i < count; i++) {
    jos_vmsvga_fifo_wr(sc, next / sizeof(u32), words[i]);
    next += sizeof(u32);
    if (next >= max) {
      next = min;
    }
  }
  wmb();
  jos_vmsvga_fifo_wr(sc, SVGA_FIFO_NEXT_CMD, next);
}

static void jos_vmsvga_fifo_update(struct jos_vmsvga* sc, u32 x, u32 y, u32 w,
                                   u32 h) {
  const u32 cmd[SVGA_CMD_UPDATE_WORDS] = {SVGA_CMD_UPDATE, x, y, w, h};

  jos_vmsvga_fifo_cmd(sc, cmd, SVGA_CMD_UPDATE_WORDS);
}

/*
 * Negotiate the interface, read the device's limits and set the FIFO up.
 * Memory maps are the caller's.
 */
static int jos_vmsvga_hw_init(struct jos_vmsvga* sc) {
  u32 min;

  jos_vmsvga_wr(sc, SVGA_REG_ID, SVGA_ID_2);
  if (SVGA_ID_2 != jos_vmsvga_rd(sc, SVGA_REG_ID)) {
    drm_err(&sc->kms.drm, "the device does not speak SVGA II\n");
    return -ENODEV;
  }
  sc->caps = jos_vmsvga_rd(sc, SVGA_REG_CAPABILITIES);
  sc->kms.max_width = jos_vmsvga_rd(sc, SVGA_REG_MAX_WIDTH);
  sc->kms.max_height = jos_vmsvga_rd(sc, SVGA_REG_MAX_HEIGHT);

  min = SVGA_FIFO_LEGACY_REGS;
  if (0 != (sc->caps & SVGA_CAP_EXTENDED_FIFO)) {
    min = jos_vmsvga_rd(sc, SVGA_REG_MEM_REGS);
  }
  min *= sizeof(u32);
  if (min < PAGE_SIZE) {
    min = PAGE_SIZE;
  }
  if (sc->fifo_size <= min) {
    drm_err(&sc->kms.drm, "FIFO of %u bytes is too small\n", sc->fifo_size);
    return -ENODEV;
  }
  jos_vmsvga_fifo_wr(sc, SVGA_FIFO_MIN, min);
  jos_vmsvga_fifo_wr(sc, SVGA_FIFO_MAX, sc->fifo_size);
  jos_vmsvga_fifo_wr(sc, SVGA_FIFO_NEXT_CMD, min);
  jos_vmsvga_fifo_wr(sc, SVGA_FIFO_STOP, min);
  wmb();
  jos_vmsvga_wr(sc, SVGA_REG_CONFIG_DONE, 1);
  return 0;
}

/*
 * The jos_kmsfb hooks.
 */

/* Program a mode and learn the pitch and offset the device chose. */
static int jos_vmsvga_enable(struct jos_kmsfb* kms, u32 width, u32 height) {
  struct jos_vmsvga* sc = to_jos_vmsvga(kms);
  u32 fb_offset;
  u64 end;

  jos_vmsvga_wr(sc, SVGA_REG_WIDTH, width);
  jos_vmsvga_wr(sc, SVGA_REG_HEIGHT, height);
  jos_vmsvga_wr(sc, SVGA_REG_BITS_PER_PIXEL, JOS_KMSFB_BPP);
  jos_vmsvga_wr(sc, SVGA_REG_ENABLE, SVGA_REG_ENABLE_ENABLE);
  if (0 != (sc->caps & SVGA_CAP_DISPLAY_TOPOLOGY)) {
    jos_vmsvga_wr(sc, SVGA_REG_NUM_GUEST_DISPLAYS, 1);
    jos_vmsvga_wr(sc, SVGA_REG_DISPLAY_ID, 0);
    jos_vmsvga_wr(sc, SVGA_REG_DISPLAY_IS_PRIMARY, 1);
    jos_vmsvga_wr(sc, SVGA_REG_DISPLAY_POSITION_X, 0);
    jos_vmsvga_wr(sc, SVGA_REG_DISPLAY_POSITION_Y, 0);
    jos_vmsvga_wr(sc, SVGA_REG_DISPLAY_WIDTH, width);
    jos_vmsvga_wr(sc, SVGA_REG_DISPLAY_HEIGHT, height);
    jos_vmsvga_wr(sc, SVGA_REG_DISPLAY_ID, SVGA_ID_INVALID);
  }
  kms->pitch = jos_vmsvga_rd(sc, SVGA_REG_BYTES_PER_LINE);
  fb_offset = jos_vmsvga_rd(sc, SVGA_REG_FB_OFFSET);
  end = (u64)fb_offset + ((u64)kms->pitch * height);
  if (kms->pitch < (u64)width * JOS_KMSFB_CPP || end > sc->vram_size) {
    drm_err(&kms->drm, "%ux%u: pitch %u at %u does not fit %u bytes of VRAM\n",
            width, height, kms->pitch, fb_offset, sc->vram_size);
    jos_vmsvga_wr(sc, SVGA_REG_ENABLE, SVGA_REG_ENABLE_DISABLE);
    return -EINVAL;
  }
  kms->screen = sc->vram + fb_offset;
  return 0;
}

static void jos_vmsvga_disable(struct jos_kmsfb* kms) {
  jos_vmsvga_wr(to_jos_vmsvga(kms), SVGA_REG_ENABLE, SVGA_REG_ENABLE_DISABLE);
}

static void jos_vmsvga_damaged(struct jos_kmsfb* kms,
                               const struct drm_rect* rect) {
  jos_vmsvga_fifo_update(to_jos_vmsvga(kms), (u32)rect->x1, (u32)rect->y1,
                         (u32)drm_rect_width(rect), (u32)drm_rect_height(rect));
}

static void jos_vmsvga_flushed(struct jos_kmsfb* kms) {
  jos_vmsvga_sync(to_jos_vmsvga(kms));
}

static const struct jos_kmsfb_funcs jos_vmsvga_funcs = {
    .enable = jos_vmsvga_enable,
    .disable = jos_vmsvga_disable,
    .damaged = jos_vmsvga_damaged,
    .flushed = jos_vmsvga_flushed,
};

/*
 * The driver.
 */

static const struct drm_driver jos_vmsvga_driver = {
    JOS_KMSFB_DRIVER_OPS, .name = DRIVER_NAME,   .desc = DRIVER_DESC,
    .date = DRIVER_DATE,  .major = DRIVER_MAJOR, .minor = DRIVER_MINOR,
};

static void jos_vmsvga_unmap(struct jos_vmsvga* sc) {
  if (NULL != sc->fifo) {
    iounmap(sc->fifo);
    sc->fifo = NULL;
  }
  if (NULL != sc->vram) {
    iounmap(sc->vram);
    sc->vram = NULL;
  }
}

/* BAR 0: ports; BAR 1: VRAM, write-combined; BAR 2: FIFO, uncached. */
static int jos_vmsvga_map(struct jos_vmsvga* sc, struct pci_dev* pdev) {
  unsigned long vram_len = pci_resource_len(pdev, 1);
  unsigned long fifo_len = pci_resource_len(pdev, 2);
  u32 vram_size;
  u32 fifo_size;

  sc->io = (u_int)pci_resource_start(pdev, 0);
  if (0 == sc->io || 0 == vram_len || 0 == fifo_len) {
    drm_err(&sc->kms.drm, "the device lacks its ports, VRAM or FIFO BAR\n");
    return -ENODEV;
  }
  vram_size = jos_vmsvga_rd(sc, SVGA_REG_VRAM_SIZE);
  fifo_size = jos_vmsvga_rd(sc, SVGA_REG_MEM_SIZE);
  sc->vram_size = (u32)min_t(unsigned long, vram_size, vram_len);
  sc->fifo_size = (u32)min_t(unsigned long, fifo_size, fifo_len);
  sc->vram = ioremap_wc(pci_resource_start(pdev, 1), sc->vram_size);
  sc->fifo = ioremap(pci_resource_start(pdev, 2), sc->fifo_size);
  if (NULL == sc->vram || NULL == sc->fifo) {
    jos_vmsvga_unmap(sc);
    return -ENOMEM;
  }
  return 0;
}

static int jos_vmsvga_pci_probe(struct pci_dev* pdev,
                                const struct pci_device_id* id __unused) {
  struct jos_vmsvga* sc;
  int ret;

  sc = JOS_KMSFB_PCI_ALLOC(pdev, &jos_vmsvga_driver, struct jos_vmsvga);
  if (IS_ERR(sc)) {
    return (int)PTR_ERR(sc);
  }
  ret = jos_vmsvga_map(sc, pdev);
  if (0 != ret) {
    return ret;
  }
  ret = jos_vmsvga_hw_init(sc);
  if (0 == ret) {
    sc->kms.mem_size = sc->vram_size;
    sc->kms.preferred_width = JOS_VMSVGA_PREFERRED_WIDTH;
    sc->kms.preferred_height = JOS_VMSVGA_PREFERRED_HEIGHT;
    ret = jos_kmsfb_register(&sc->kms, &jos_vmsvga_funcs);
  }
  if (0 != ret) {
    jos_vmsvga_unmap(sc);
    return ret;
  }
  drm_info(&sc->kms.drm, "%u MB of VRAM, up to %ux%u, FIFO of %u KB\n",
           sc->vram_size >> 20, sc->kms.max_width, sc->kms.max_height,
           sc->fifo_size >> 10);
  return 0;
}

static void jos_vmsvga_pci_remove(struct pci_dev* pdev) {
  struct jos_vmsvga* sc = to_jos_vmsvga(jos_kmsfb_of_pci(pdev));

  jos_kmsfb_unregister(&sc->kms);
  jos_vmsvga_unmap(sc);
}

static const struct pci_device_id jos_vmsvga_pci_ids[] = {
    {PCI_DEVICE(SVGA_PCI_VENDOR, SVGA_PCI_DEVICE)},
    {0},
};

static struct pci_driver jos_vmsvga_pci_driver = {
    /* LinuxKPI attaches a driver of this name to the display device. */
    .name = "drmn",
    .id_table = jos_vmsvga_pci_ids,
    .probe = jos_vmsvga_pci_probe,
    .remove = jos_vmsvga_pci_remove,
};

/*
 * No PNP information on purpose: devmatch(8) would load the module from
 * rc(8) with a message on the console, which ends the kernel splash.
 * rc.d/jos-kms loads it quietly from jos_kms_module in rc.conf.
 */
static int jos_vmsvga_modevent(module_t mod __unused, int type,
                               void* data __unused) {
  switch (type) {
  case MOD_LOAD:
    return -pci_register_driver(&jos_vmsvga_pci_driver);
  case MOD_UNLOAD:
    pci_unregister_driver(&jos_vmsvga_pci_driver);
    return 0;
  default:
    return EOPNOTSUPP;
  }
}

static moduledata_t jos_vmsvga_mod = {DRIVER_NAME, jos_vmsvga_modevent, NULL};
DECLARE_MODULE(jos_vmsvga, jos_vmsvga_mod, SI_SUB_DRIVERS, SI_ORDER_ANY);
MODULE_VERSION(jos_vmsvga, 1);
MODULE_DEPEND(jos_vmsvga, drmn, 2, 2, 2);
MODULE_DEPEND(jos_vmsvga, linuxkpi, 1, 1, 1);
