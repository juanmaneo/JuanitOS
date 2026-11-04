/*-
 * SPDX-License-Identifier: BSD-3-Clause
 *
 * Copyright (c) 2026, Juanito and the JuanitOS contributors (see AUTHORS)
 *
 * jos_kmsfb: what JuanitOS's frame buffer KMS drivers (jos_vmsvga)
 * have in common. It gives a dumb frame buffer what a Wayland
 * compositor needs from a KMS driver: one connector, one CRTC, one
 * primary plane (XRGB8888), dumb buffers that user space can map, atomic
 * commits with damage, the fbdev console. There is no acceleration, no
 * hardware cursor and no render node; Mesa renders with kms_swrast.
 *
 * Buffers live in system memory: a dumb buffer is a page array with a
 * permanent kernel mapping, and a page fault on a user mapping inserts
 * the page into the mapping's VM object the way drm-kmod's TTM does on
 * FreeBSD. On every atomic commit the damaged rows of the plane's buffer
 * are copied to the screen (`screen`, `pitch`), and the driver hears of
 * each damaged rectangle and of the end of the commit.
 *
 * The fbdev emulation (vt(4)'s console, and what jos-splash and jos-greet
 * map through /dev/ttyv*) draws into a shadow buffer in system memory;
 * drm-kmod flushes it on the console's own drawing calls only. A mapped
 * shadow changes without those, so while the console's frame buffer is
 * the one on screen a timer marks the whole screen damaged a few times a
 * second, which copies shadow to buffer to screen.
 *
 * A driver embeds a struct jos_kmsfb named `kms`, allocates it with
 * JOS_KMSFB_PCI_ALLOC(), fills in the limits and the screen, and calls
 * jos_kmsfb_register(); its remove calls jos_kmsfb_unregister().
 */

#ifndef JOS_KMSFB_H
#define JOS_KMSFB_H

#include <linux/mutex.h>
#include <linux/pci.h>
#include <linux/workqueue.h>

#include <drm/drm_connector.h>
#include <drm/drm_crtc.h>
#include <drm/drm_device.h>
#include <drm/drm_drv.h>
#include <drm/drm_encoder.h>
#include <drm/drm_fbdev_ttm.h>
#include <drm/drm_file.h>
#include <drm/drm_plane.h>
#include <drm/drm_rect.h>

/* The one pixel format: XRGB8888, 4 bytes a pixel. */
#define JOS_KMSFB_BPP 32
#define JOS_KMSFB_CPP 4

struct jos_kmsfb;

/*
 * What the hardware does; every hook is optional and runs with `hw_lock`
 * held, inside drm_dev_enter().
 */
struct jos_kmsfb_funcs {
  /*
   * Show a width x height mode and set `screen` and `pitch` for it: 0, or
   * a negative errno and the screen stays off.
   */
  int (*enable)(struct jos_kmsfb* kms, u32 width, u32 height);
  /* Turn the screen off. */
  void (*disable)(struct jos_kmsfb* kms);
  /* This rectangle of the screen was just rewritten. */
  void (*damaged)(struct jos_kmsfb* kms, const struct drm_rect* rect);
  /* Every damaged rectangle of a commit has been rewritten. */
  void (*flushed)(struct jos_kmsfb* kms);
};

struct jos_kmsfb {
  struct drm_device drm;
  struct drm_plane plane;
  struct drm_crtc crtc;
  struct drm_encoder encoder;
  struct drm_connector connector;
  const struct jos_kmsfb_funcs* funcs;
  /* Serialises the hooks and the mode fields below. */
  struct mutex hw_lock;
  struct delayed_work flush;
  /* The frame buffer the plane last put on screen; NULL when none. */
  struct drm_framebuffer* scanout_fb;
  /* Set by the driver before jos_kmsfb_register(). */
  u32 max_width;
  u32 max_height;
  /* Bytes the screen may take (a mode needs width * height * CPP). */
  u64 mem_size;
  /* The mode offered first; with `fixed`, the only one (max_width x
   * max_height). */
  u32 preferred_width;
  u32 preferred_height;
  bool fixed;
  /* The mode in effect: width and height are 0 while the screen is off. */
  u8 __iomem* screen;
  u32 pitch;
  u32 width;
  u32 height;
};

/* The DRM driver fields every jos_kmsfb driver shares. */
extern const struct file_operations jos_kmsfb_fops;
int jos_kmsfb_dumb_create(struct drm_file* file, struct drm_device* dev,
                          struct drm_mode_create_dumb* args);
#define JOS_KMSFB_DRIVER_OPS                                                   \
  .driver_features = DRIVER_ATOMIC | DRIVER_GEM | DRIVER_MODESET,              \
  .fops = &jos_kmsfb_fops, .dumb_create = jos_kmsfb_dumb_create,               \
  DRM_FBDEV_TTM_DRIVER_OPS

/*
 * Release the PCI device's firmware frame buffers, allocate the driver's
 * structure (`size` bytes, its struct jos_kmsfb's drm_device at `offset`)
 * and enable the device: the structure, or an ERR_PTR().
 */
void* jos_kmsfb_pci_alloc(struct pci_dev* pdev, const struct drm_driver* driver,
                          size_t size, size_t offset);
#define JOS_KMSFB_PCI_ALLOC(pdev, driver, type)                                \
  ((type*)jos_kmsfb_pci_alloc((pdev), (driver), sizeof(type),                  \
                              offsetof(type, kms.drm)))

/* The struct jos_kmsfb of a device JOS_KMSFB_PCI_ALLOC() set up. */
struct jos_kmsfb* jos_kmsfb_of_pci(struct pci_dev* pdev);

/* Set up mode setting, register the device and start the console. */
int jos_kmsfb_register(struct jos_kmsfb* kms,
                       const struct jos_kmsfb_funcs* funcs);

/* Stop the console, unplug the device and turn the screen off. */
void jos_kmsfb_unregister(struct jos_kmsfb* kms);

#endif /* JOS_KMSFB_H */
