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
 * through an index/value port pair (BAR 0). This driver gives that device
 * what a Wayland compositor needs from a KMS driver: one connector, one
 * CRTC, one primary plane, dumb buffers that user space can map, atomic
 * commits with damage. There is no acceleration and no hardware cursor;
 * Mesa renders with kms_swrast.
 *
 * Buffers live in system memory: a dumb buffer is a page array with a
 * permanent kernel mapping, and a page fault on a user mapping inserts
 * the page into the mapping's VM object the way drm-kmod's TTM does on
 * FreeBSD. On every atomic commit the damaged rows of the plane's buffer
 * are copied to VRAM and one UPDATE per damaged rectangle goes to the
 * FIFO.
 *
 * The fbdev emulation (vt(4)'s console, and what jos-splash and jos-greet
 * map through /dev/ttyv*) draws into a shadow buffer in system memory;
 * drm-kmod flushes it on the console's own drawing calls only. A mapped
 * shadow changes without those, so while the console's frame buffer is
 * the one on screen a timer marks the whole screen damaged a few times a
 * second, which copies shadow to buffer to VRAM.
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/kernel.h>
#include <sys/module.h>

#include <machine/cpufunc.h>

#include <vm/vm.h>
#include <vm/vm_object.h>
#include <vm/vm_page.h>

#include <linux/delay.h>
#include <linux/gfp.h>
#include <linux/io.h>
#include <linux/iosys-map.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/pci.h>
#include <linux/slab.h>
#include <linux/vmalloc.h>
#include <linux/workqueue.h>

#include <drm/drm_aperture.h>
#include <drm/drm_atomic.h>
#include <drm/drm_atomic_helper.h>
#include <drm/drm_atomic_state_helper.h>
#include <drm/drm_client_setup.h>
#include <drm/drm_connector.h>
#include <drm/drm_crtc.h>
#include <drm/drm_damage_helper.h>
#include <drm/drm_drv.h>
#include <drm/drm_edid.h>
#include <drm/drm_encoder.h>
#include <drm/drm_fb_helper.h>
#include <drm/drm_fbdev_ttm.h>
#include <drm/drm_file.h>
#include <drm/drm_fourcc.h>
#include <drm/drm_framebuffer.h>
#include <drm/drm_gem.h>
#include <drm/drm_gem_framebuffer_helper.h>
#include <drm/drm_ioctl.h>
#include <drm/drm_managed.h>
#include <drm/drm_mode_config.h>
#include <drm/drm_modes.h>
#include <drm/drm_modeset_helper_vtables.h>
#include <drm/drm_plane.h>
#include <drm/drm_print.h>
#include <drm/drm_probe_helper.h>
#include <drm/drm_rect.h>
#include <drm/drm_vblank.h>
#include <drm/drm_vma_manager.h>

#include "svga_reg.h"

#define DRIVER_NAME "jos_vmsvga"
#define DRIVER_DESC "VMware SVGA II display (JuanitOS)"
#define DRIVER_DATE "20261103"
#define DRIVER_MAJOR 1
#define DRIVER_MINOR 0

/* The one pixel format: 32 bits per pixel, 4 bytes, XRGB. */
#define JOS_VMSVGA_BPP 32
#define JOS_VMSVGA_CPP 4
#define JOS_VMSVGA_DEPTH 24

/* The mode the console and the greeter come up in, when VRAM allows. */
#define JOS_VMSVGA_PREFERRED_WIDTH 1920
#define JOS_VMSVGA_PREFERRED_HEIGHT 1080

/* How often a mapped console shadow is pushed to the screen. */
#define JOS_VMSVGA_FLUSH_MS 50

/* Waiting for the host to drain the FIFO: polls of this many µs, at most. */
#define JOS_VMSVGA_SYNC_POLL_US 20
#define JOS_VMSVGA_SYNC_POLLS 5000

/* Pages inserted into a user mapping per fault, from the faulting one. */
#define JOS_VMSVGA_PREFAULT 16

/* 16.16 fixed point of a plane's source rectangle. */
#define JOS_VMSVGA_FIXED_SHIFT 16

struct jos_vmsvga {
  struct drm_device drm;
  struct drm_plane plane;
  struct drm_crtc crtc;
  struct drm_encoder encoder;
  struct drm_connector connector;
  /* Serialises register and FIFO access and the mode fields below. */
  struct mutex hw_lock;
  struct delayed_work flush;
  /* The frame buffer the plane last put on screen; NULL when none. */
  struct drm_framebuffer* scanout_fb;
  u_int io;
  u8 __iomem* vram;
  u32 __iomem* fifo;
  u32 vram_size;
  u32 fifo_size;
  u32 caps;
  u32 max_width;
  u32 max_height;
  /* The mode in effect; width and height are 0 while the screen is off. */
  u32 width;
  u32 height;
  u32 pitch;
  u32 fb_offset;
  bool host_stuck;
};

struct jos_vmsvga_bo {
  struct drm_gem_object base;
  struct page** pages;
  unsigned int npages;
  void* vaddr;
};

static struct jos_vmsvga* to_jos_vmsvga(struct drm_device* dev) {
  return container_of(dev, struct jos_vmsvga, drm);
}

static struct jos_vmsvga_bo* to_jos_vmsvga_bo(struct drm_gem_object* obj) {
  return container_of(obj, struct jos_vmsvga_bo, base);
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
    drm_warn(&sc->drm, "the host does not drain the FIFO\n");
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
    drm_err(&sc->drm, "the device does not speak SVGA II\n");
    return -ENODEV;
  }
  sc->caps = jos_vmsvga_rd(sc, SVGA_REG_CAPABILITIES);
  sc->max_width = jos_vmsvga_rd(sc, SVGA_REG_MAX_WIDTH);
  sc->max_height = jos_vmsvga_rd(sc, SVGA_REG_MAX_HEIGHT);

  min = SVGA_FIFO_LEGACY_REGS;
  if (0 != (sc->caps & SVGA_CAP_EXTENDED_FIFO)) {
    min = jos_vmsvga_rd(sc, SVGA_REG_MEM_REGS);
  }
  min *= sizeof(u32);
  if (min < PAGE_SIZE) {
    min = PAGE_SIZE;
  }
  if (sc->fifo_size <= min) {
    drm_err(&sc->drm, "FIFO of %u bytes is too small\n", sc->fifo_size);
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

/* Program a mode and learn the pitch and offset the device chose. */
static void jos_vmsvga_set_mode(struct jos_vmsvga* sc, u32 width, u32 height) {
  u64 end;

  jos_vmsvga_wr(sc, SVGA_REG_WIDTH, width);
  jos_vmsvga_wr(sc, SVGA_REG_HEIGHT, height);
  jos_vmsvga_wr(sc, SVGA_REG_BITS_PER_PIXEL, JOS_VMSVGA_BPP);
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
  sc->pitch = jos_vmsvga_rd(sc, SVGA_REG_BYTES_PER_LINE);
  sc->fb_offset = jos_vmsvga_rd(sc, SVGA_REG_FB_OFFSET);
  end = (u64)sc->fb_offset + ((u64)sc->pitch * height);
  if (sc->pitch < (u64)width * JOS_VMSVGA_CPP || end > sc->vram_size) {
    drm_err(&sc->drm, "%ux%u: pitch %u at %u does not fit %u bytes of VRAM\n",
            width, height, sc->pitch, sc->fb_offset, sc->vram_size);
    jos_vmsvga_wr(sc, SVGA_REG_ENABLE, SVGA_REG_ENABLE_DISABLE);
    sc->width = 0;
    sc->height = 0;
    return;
  }
  sc->width = width;
  sc->height = height;
}

static void jos_vmsvga_screen_off(struct jos_vmsvga* sc) {
  jos_vmsvga_wr(sc, SVGA_REG_ENABLE, SVGA_REG_ENABLE_DISABLE);
  sc->width = 0;
  sc->height = 0;
}

/*
 * GEM objects: page arrays in system memory.
 */

static void jos_vmsvga_gem_free(struct drm_gem_object* obj) {
  struct jos_vmsvga_bo* bo = to_jos_vmsvga_bo(obj);
  unsigned int i;

  if (NULL != bo->vaddr) {
    vunmap(bo->vaddr);
  }
  for (i = 0; i < bo->npages; i++) {
    if (NULL != bo->pages[i]) {
      __free_page(bo->pages[i]);
    }
  }
  kvfree((void*)bo->pages);
  drm_gem_object_release(obj);
  kfree(bo);
}

static int jos_vmsvga_gem_vmap(struct drm_gem_object* obj,
                               struct iosys_map* map) {
  iosys_map_set_vaddr(map, to_jos_vmsvga_bo(obj)->vaddr);
  return 0;
}

static void jos_vmsvga_gem_vunmap(struct drm_gem_object* obj __unused,
                                  struct iosys_map* map __unused) {}

/*
 * Insert the faulting page and a few after it into the mapping's VM
 * object, as drm-kmod's TTM does: the pages become managed (the VM
 * tracks their mappings) and stay wired, so they outlive the object.
 * The mapping starts at the object's fake offset; vm_pgoff holds it.
 */
static int jos_vmsvga_gem_fault(struct vm_fault* vmf) {
  struct vm_area_struct* vma = vmf->vma;
  struct drm_gem_object* obj = vma->vm_private_data;
  struct jos_vmsvga_bo* bo = to_jos_vmsvga_bo(obj);
  unsigned long address = vmf->address;
  unsigned long node = drm_vma_node_start(&obj->vma_node);
  unsigned long page_offset;
  unsigned long page_last;
  int ret = VM_FAULT_NOPAGE;
  int i;

  page_offset =
      ((address - vma->vm_start) >> PAGE_SHIFT) + vma->vm_pgoff - node;
  page_last = vma_pages(vma) + vma->vm_pgoff - node;
  if (page_offset >= bo->npages) {
    return VM_FAULT_SIGBUS;
  }
  if (page_last > bo->npages) {
    page_last = bo->npages;
  }

  VM_OBJECT_WLOCK(vma->vm_obj);
  for (i = 0; i < JOS_VMSVGA_PREFAULT && page_offset < page_last; i++) {
    struct page* page = bo->pages[page_offset];

    page->oflags &= ~VPO_UNMANAGED;
    ret = lkpi_vmf_insert_pfn_prot_locked(vma, address, page_to_pfn(page),
                                          vma->vm_page_prot);
    if (0 != (ret & VM_FAULT_ERROR)) {
      /* The first page is retried by the caller; the rest were extra. */
      if (0 == i) {
        ret = VM_FAULT_NOPAGE;
      }
      break;
    }
    address += PAGE_SIZE;
    page_offset++;
  }
  VM_OBJECT_WUNLOCK(vma->vm_obj);
  return ret;
}

static const struct vm_operations_struct jos_vmsvga_gem_vm_ops = {
    .fault = jos_vmsvga_gem_fault,
    .open = drm_gem_vm_open,
    .close = drm_gem_vm_close,
};

/* Cached, like the kernel's own mapping of the pages. */
static int jos_vmsvga_gem_mmap(struct drm_gem_object* obj __unused,
                               struct vm_area_struct* vma) {
  vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
  vma->vm_page_prot = vm_get_page_prot(vma->vm_flags);
  return 0;
}

static const struct drm_gem_object_funcs jos_vmsvga_gem_funcs = {
    .free = jos_vmsvga_gem_free,
    .vmap = jos_vmsvga_gem_vmap,
    .vunmap = jos_vmsvga_gem_vunmap,
    .mmap = jos_vmsvga_gem_mmap,
    .vm_ops = &jos_vmsvga_gem_vm_ops,
};

static struct jos_vmsvga_bo* jos_vmsvga_bo_create(struct drm_device* dev,
                                                  size_t size) {
  struct jos_vmsvga_bo* bo;
  unsigned int i;

  size = PAGE_ALIGN(size);
  bo = kzalloc(sizeof(*bo), GFP_KERNEL);
  if (NULL == bo) {
    return ERR_PTR(-ENOMEM);
  }
  bo->npages = size >> PAGE_SHIFT;
  bo->pages = kvcalloc(bo->npages, sizeof(*bo->pages), GFP_KERNEL);
  if (NULL == bo->pages) {
    kfree(bo);
    return ERR_PTR(-ENOMEM);
  }
  drm_gem_private_object_init(dev, &bo->base, size);
  bo->base.funcs = &jos_vmsvga_gem_funcs;
  for (i = 0; i < bo->npages; i++) {
    bo->pages[i] = alloc_page(GFP_KERNEL | __GFP_ZERO);
    if (NULL == bo->pages[i]) {
      jos_vmsvga_gem_free(&bo->base);
      return ERR_PTR(-ENOMEM);
    }
  }
  bo->vaddr = vmap(bo->pages, bo->npages, VM_MAP, PAGE_KERNEL);
  if (NULL == bo->vaddr) {
    jos_vmsvga_gem_free(&bo->base);
    return ERR_PTR(-ENOMEM);
  }
  return bo;
}

static int jos_vmsvga_dumb_create(struct drm_file* file, struct drm_device* dev,
                                  struct drm_mode_create_dumb* args) {
  struct jos_vmsvga* sc = to_jos_vmsvga(dev);
  struct jos_vmsvga_bo* bo;
  int ret;

  if (JOS_VMSVGA_BPP != args->bpp || 0 == args->width || 0 == args->height ||
      args->width > sc->max_width || args->height > sc->max_height) {
    return -EINVAL;
  }
  args->pitch = args->width * JOS_VMSVGA_CPP;
  args->size = PAGE_ALIGN((u64)args->pitch * args->height);
  bo = jos_vmsvga_bo_create(dev, args->size);
  if (IS_ERR(bo)) {
    return (int)PTR_ERR(bo);
  }
  ret = drm_gem_handle_create(file, &bo->base, &args->handle);
  drm_gem_object_put(&bo->base);
  return ret;
}

/*
 * Mode setting.
 */

/* Copy a damaged rectangle of the plane's buffer to VRAM and show it. */
static void jos_vmsvga_blit(struct jos_vmsvga* sc,
                            const struct drm_plane_state* state,
                            const struct drm_rect* clip) {
  struct drm_framebuffer* fb = state->fb;
  const struct jos_vmsvga_bo* bo = to_jos_vmsvga_bo(drm_gem_fb_get_obj(fb, 0));
  u32 src_x1 = (u32)(state->src.x1 >> JOS_VMSVGA_FIXED_SHIFT);
  u32 src_y1 = (u32)(state->src.y1 >> JOS_VMSVGA_FIXED_SHIFT);
  u32 x;
  u32 y;
  u32 w;
  u32 h;
  u32 row;
  const u8* src;
  u8 __iomem* dst;
  size_t len;

  if (clip->x1 < 0 || clip->y1 < 0 || clip->x2 <= clip->x1 ||
      clip->y2 <= clip->y1) {
    return;
  }
  x = (u32)clip->x1 - src_x1;
  y = (u32)clip->y1 - src_y1;
  if (x >= sc->width || y >= sc->height) {
    return;
  }
  w = min((u32)drm_rect_width(clip), sc->width - x);
  h = min((u32)drm_rect_height(clip), sc->height - y);
  src = (const u8*)bo->vaddr + fb->offsets[0] +
        ((size_t)clip->y1 * fb->pitches[0]) +
        ((size_t)clip->x1 * JOS_VMSVGA_CPP);
  dst = sc->vram + sc->fb_offset + ((size_t)y * sc->pitch) +
        ((size_t)x * JOS_VMSVGA_CPP);
  len = (size_t)w * JOS_VMSVGA_CPP;
  for (row = 0; row < h; row++) {
    memcpy_toio(dst, src, len);
    src += fb->pitches[0];
    dst += sc->pitch;
  }
  jos_vmsvga_fifo_update(sc, x, y, w, h);
}

static int jos_vmsvga_plane_atomic_check(struct drm_plane* plane,
                                         struct drm_atomic_state* state) {
  struct drm_plane_state* new_state =
      drm_atomic_get_new_plane_state(state, plane);
  struct drm_crtc_state* crtc_state = NULL;

  if (NULL != new_state->crtc) {
    crtc_state = drm_atomic_get_new_crtc_state(state, new_state->crtc);
  }
  return drm_atomic_helper_check_plane_state(
      new_state, crtc_state, DRM_PLANE_NO_SCALING, DRM_PLANE_NO_SCALING, false,
      false);
}

static void jos_vmsvga_plane_atomic_update(struct drm_plane* plane,
                                           struct drm_atomic_state* state) {
  struct jos_vmsvga* sc = to_jos_vmsvga(plane->dev);
  struct drm_plane_state* old_state =
      drm_atomic_get_old_plane_state(state, plane);
  struct drm_plane_state* new_state =
      drm_atomic_get_new_plane_state(state, plane);
  struct drm_atomic_helper_damage_iter iter;
  struct drm_rect clip;
  int idx;

  if (NULL == new_state->fb || !new_state->visible) {
    WRITE_ONCE(sc->scanout_fb, NULL);
    return;
  }
  WRITE_ONCE(sc->scanout_fb, new_state->fb);
  if (!drm_dev_enter(&sc->drm, &idx)) {
    return;
  }
  mutex_lock(&sc->hw_lock);
  if (0 != sc->width) {
    drm_atomic_helper_damage_iter_init(&iter, old_state, new_state);
    drm_atomic_for_each_plane_damage(&iter, &clip) {
      jos_vmsvga_blit(sc, new_state, &clip);
    }
    jos_vmsvga_sync(sc);
  }
  mutex_unlock(&sc->hw_lock);
  drm_dev_exit(idx);
}

static void
jos_vmsvga_plane_atomic_disable(struct drm_plane* plane,
                                struct drm_atomic_state* state __unused) {
  struct jos_vmsvga* sc = to_jos_vmsvga(plane->dev);

  WRITE_ONCE(sc->scanout_fb, NULL);
}

static const struct drm_plane_helper_funcs jos_vmsvga_plane_helper_funcs = {
    .atomic_check = jos_vmsvga_plane_atomic_check,
    .atomic_update = jos_vmsvga_plane_atomic_update,
    .atomic_disable = jos_vmsvga_plane_atomic_disable,
};

static const struct drm_plane_funcs jos_vmsvga_plane_funcs = {
    .update_plane = drm_atomic_helper_update_plane,
    .disable_plane = drm_atomic_helper_disable_plane,
    .destroy = drm_plane_cleanup,
    .reset = drm_atomic_helper_plane_reset,
    .atomic_duplicate_state = drm_atomic_helper_plane_duplicate_state,
    .atomic_destroy_state = drm_atomic_helper_plane_destroy_state,
};

static const u32 jos_vmsvga_formats[] = {
    DRM_FORMAT_XRGB8888,
};

static enum drm_mode_status jos_vmsvga_mode_fits(const struct jos_vmsvga* sc,
                                                 u32 width, u32 height) {
  if (width > sc->max_width || height > sc->max_height) {
    return MODE_BAD;
  }
  if ((u64)width * height * JOS_VMSVGA_CPP > sc->vram_size) {
    return MODE_MEM;
  }
  return MODE_OK;
}

static enum drm_mode_status
jos_vmsvga_crtc_mode_valid(struct drm_crtc* crtc,
                           const struct drm_display_mode* mode) {
  return jos_vmsvga_mode_fits(to_jos_vmsvga(crtc->dev), mode->hdisplay,
                              mode->vdisplay);
}

static int jos_vmsvga_crtc_atomic_check(struct drm_crtc* crtc,
                                        struct drm_atomic_state* state) {
  struct drm_crtc_state* crtc_state =
      drm_atomic_get_new_crtc_state(state, crtc);

  if (!crtc_state->enable) {
    return 0;
  }
  return drm_atomic_helper_check_crtc_primary_plane(crtc_state);
}

static void jos_vmsvga_crtc_atomic_enable(struct drm_crtc* crtc,
                                          struct drm_atomic_state* state) {
  struct jos_vmsvga* sc = to_jos_vmsvga(crtc->dev);
  struct drm_crtc_state* crtc_state =
      drm_atomic_get_new_crtc_state(state, crtc);
  const struct drm_display_mode* mode = &crtc_state->mode;
  int idx;

  if (!drm_dev_enter(&sc->drm, &idx)) {
    return;
  }
  mutex_lock(&sc->hw_lock);
  jos_vmsvga_set_mode(sc, mode->hdisplay, mode->vdisplay);
  mutex_unlock(&sc->hw_lock);
  drm_dev_exit(idx);
}

static void
jos_vmsvga_crtc_atomic_disable(struct drm_crtc* crtc,
                               struct drm_atomic_state* state __unused) {
  struct jos_vmsvga* sc = to_jos_vmsvga(crtc->dev);
  int idx;

  if (!drm_dev_enter(&sc->drm, &idx)) {
    return;
  }
  mutex_lock(&sc->hw_lock);
  jos_vmsvga_screen_off(sc);
  mutex_unlock(&sc->hw_lock);
  drm_dev_exit(idx);
}

/* No vertical blank interrupt: a commit's event is sent when it is done. */
static void jos_vmsvga_crtc_atomic_flush(struct drm_crtc* crtc,
                                         struct drm_atomic_state* state) {
  struct drm_crtc_state* crtc_state =
      drm_atomic_get_new_crtc_state(state, crtc);
  struct drm_pending_vblank_event* event = crtc_state->event;

  if (NULL == event) {
    return;
  }
  crtc_state->event = NULL;
  spin_lock_irq(&crtc->dev->event_lock);
  drm_crtc_send_vblank_event(crtc, event);
  spin_unlock_irq(&crtc->dev->event_lock);
}

static const struct drm_crtc_helper_funcs jos_vmsvga_crtc_helper_funcs = {
    .mode_valid = jos_vmsvga_crtc_mode_valid,
    .atomic_check = jos_vmsvga_crtc_atomic_check,
    .atomic_enable = jos_vmsvga_crtc_atomic_enable,
    .atomic_disable = jos_vmsvga_crtc_atomic_disable,
    .atomic_flush = jos_vmsvga_crtc_atomic_flush,
};

static const struct drm_crtc_funcs jos_vmsvga_crtc_funcs = {
    .reset = drm_atomic_helper_crtc_reset,
    .destroy = drm_crtc_cleanup,
    .set_config = drm_atomic_helper_set_config,
    .page_flip = drm_atomic_helper_page_flip,
    .atomic_duplicate_state = drm_atomic_helper_crtc_duplicate_state,
    .atomic_destroy_state = drm_atomic_helper_crtc_destroy_state,
};

static int jos_vmsvga_connector_get_modes(struct drm_connector* connector) {
  struct jos_vmsvga* sc = to_jos_vmsvga(connector->dev);
  int count;

  count =
      drm_add_modes_noedid(connector, (int)sc->max_width, (int)sc->max_height);
  if (MODE_OK == jos_vmsvga_mode_fits(sc, JOS_VMSVGA_PREFERRED_WIDTH,
                                      JOS_VMSVGA_PREFERRED_HEIGHT)) {
    drm_set_preferred_mode(connector, JOS_VMSVGA_PREFERRED_WIDTH,
                           JOS_VMSVGA_PREFERRED_HEIGHT);
  }
  return count;
}

static enum drm_mode_status
jos_vmsvga_connector_mode_valid(struct drm_connector* connector,
                                struct drm_display_mode* mode) {
  return jos_vmsvga_mode_fits(to_jos_vmsvga(connector->dev), mode->hdisplay,
                              mode->vdisplay);
}

static const struct drm_connector_helper_funcs
    jos_vmsvga_connector_helper_funcs = {
        .get_modes = jos_vmsvga_connector_get_modes,
        .mode_valid = jos_vmsvga_connector_mode_valid,
};

static const struct drm_connector_funcs jos_vmsvga_connector_funcs = {
    .reset = drm_atomic_helper_connector_reset,
    .fill_modes = drm_helper_probe_single_connector_modes,
    .destroy = drm_connector_cleanup,
    .atomic_duplicate_state = drm_atomic_helper_connector_duplicate_state,
    .atomic_destroy_state = drm_atomic_helper_connector_destroy_state,
};

static const struct drm_encoder_funcs jos_vmsvga_encoder_funcs = {
    .destroy = drm_encoder_cleanup,
};

static const struct drm_framebuffer_funcs jos_vmsvga_fb_funcs = {
    .destroy = drm_gem_fb_destroy,
    .create_handle = drm_gem_fb_create_handle,
    .dirty = drm_atomic_helper_dirtyfb,
};

/*
 * Linux's drm_gem_fb_create_with_dirty(), which drm-kmod declares but its
 * drm.ko does not have (its GEM framebuffer helper only brings
 * drm_gem_fb_init_with_funcs()). That helper also skips Linux's check that
 * the buffer object holds the framebuffer, which jos_vmsvga_blit() relies
 * on: done here. drm_internal_framebuffer_create() has already checked the
 * format (XRGB8888, one plane) and the pitch.
 */
static struct drm_framebuffer*
jos_vmsvga_fb_create(struct drm_device* dev, struct drm_file* file,
                     const struct drm_mode_fb_cmd2* mode_cmd) {
  struct drm_framebuffer* fb;
  int ret;

  fb = kzalloc(sizeof(*fb), GFP_KERNEL);
  if (NULL == fb) {
    return ERR_PTR(-ENOMEM);
  }
  ret =
      drm_gem_fb_init_with_funcs(dev, fb, file, mode_cmd, &jos_vmsvga_fb_funcs);
  if (0 != ret) {
    kfree(fb);
    return ERR_PTR(ret);
  }
  if ((u64)mode_cmd->offsets[0] +
          ((u64)mode_cmd->pitches[0] * mode_cmd->height) >
      fb->obj[0]->size) {
    drm_framebuffer_put(fb);
    return ERR_PTR(-EINVAL);
  }
  return fb;
}

static const struct drm_mode_config_funcs jos_vmsvga_mode_config_funcs = {
    .fb_create = jos_vmsvga_fb_create,
    .atomic_check = drm_atomic_helper_check,
    .atomic_commit = drm_atomic_helper_commit,
};

static int jos_vmsvga_modeset_init(struct jos_vmsvga* sc) {
  struct drm_device* dev = &sc->drm;
  int ret;

  ret = drmm_mode_config_init(dev);
  if (0 != ret) {
    return ret;
  }
  dev->mode_config.max_width = (int)sc->max_width;
  dev->mode_config.max_height = (int)sc->max_height;
  dev->mode_config.preferred_depth = JOS_VMSVGA_DEPTH;
  dev->mode_config.funcs = &jos_vmsvga_mode_config_funcs;

  ret = drm_universal_plane_init(dev, &sc->plane, 0, &jos_vmsvga_plane_funcs,
                                 jos_vmsvga_formats, nitems(jos_vmsvga_formats),
                                 NULL, DRM_PLANE_TYPE_PRIMARY, NULL);
  if (0 != ret) {
    return ret;
  }
  drm_plane_helper_add(&sc->plane, &jos_vmsvga_plane_helper_funcs);
  drm_plane_enable_fb_damage_clips(&sc->plane);

  ret = drm_crtc_init_with_planes(dev, &sc->crtc, &sc->plane, NULL,
                                  &jos_vmsvga_crtc_funcs, NULL);
  if (0 != ret) {
    return ret;
  }
  drm_crtc_helper_add(&sc->crtc, &jos_vmsvga_crtc_helper_funcs);

  ret = drm_encoder_init(dev, &sc->encoder, &jos_vmsvga_encoder_funcs,
                         DRM_MODE_ENCODER_VIRTUAL, NULL);
  if (0 != ret) {
    return ret;
  }
  sc->encoder.possible_crtcs = drm_crtc_mask(&sc->crtc);

  ret = drm_connector_init(dev, &sc->connector, &jos_vmsvga_connector_funcs,
                           DRM_MODE_CONNECTOR_VIRTUAL);
  if (0 != ret) {
    return ret;
  }
  drm_connector_helper_add(&sc->connector, &jos_vmsvga_connector_helper_funcs);
  ret = drm_connector_attach_encoder(&sc->connector, &sc->encoder);
  if (0 != ret) {
    return ret;
  }
  drm_mode_config_reset(dev);
  return 0;
}

/*
 * The console shadow: pushed to the screen while the console's frame
 * buffer is the one shown (see the top of the file).
 */
static void jos_vmsvga_flush_work(struct work_struct* work) {
  struct jos_vmsvga* sc = container_of(work, struct jos_vmsvga, flush.work);
  struct drm_fb_helper* helper = sc->drm.fb_helper;

  if (NULL != helper && NULL != helper->info && NULL != helper->fb &&
      READ_ONCE(sc->scanout_fb) == helper->fb) {
    drm_fb_helper_damage_area(helper->info, 0, 0, helper->fb->width,
                              helper->fb->height);
  }
  schedule_delayed_work(&sc->flush, msecs_to_jiffies(JOS_VMSVGA_FLUSH_MS));
}

/*
 * The driver.
 */

DEFINE_DRM_GEM_FOPS(jos_vmsvga_fops);

static const struct drm_driver jos_vmsvga_driver = {
    .driver_features = DRIVER_ATOMIC | DRIVER_GEM | DRIVER_MODESET,
    .fops = &jos_vmsvga_fops,
    .dumb_create = jos_vmsvga_dumb_create,
    DRM_FBDEV_TTM_DRIVER_OPS,
    .name = DRIVER_NAME,
    .desc = DRIVER_DESC,
    .date = DRIVER_DATE,
    .major = DRIVER_MAJOR,
    .minor = DRIVER_MINOR,
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
    drm_err(&sc->drm, "the device lacks its ports, VRAM or FIFO BAR\n");
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
  struct drm_device* dev;
  int ret;

  ret = drm_aperture_remove_conflicting_pci_framebuffers(pdev,
                                                         &jos_vmsvga_driver);
  if (0 != ret) {
    return ret;
  }
  sc = devm_drm_dev_alloc(&pdev->dev, &jos_vmsvga_driver, struct jos_vmsvga,
                          drm);
  if (IS_ERR(sc)) {
    return (int)PTR_ERR(sc);
  }
  dev = &sc->drm;
  pci_set_drvdata(pdev, dev);
  ret = pcim_enable_device(pdev);
  if (0 != ret) {
    return ret;
  }
  ret = jos_vmsvga_map(sc, pdev);
  if (0 != ret) {
    return ret;
  }
  mutex_init(&sc->hw_lock);
  ret = jos_vmsvga_hw_init(sc);
  if (0 == ret) {
    ret = jos_vmsvga_modeset_init(sc);
  }
  if (0 == ret) {
    ret = drm_dev_register(dev, 0);
  }
  if (0 != ret) {
    jos_vmsvga_unmap(sc);
    mutex_destroy(&sc->hw_lock);
    return ret;
  }
  drm_info(dev, "%u MB of VRAM, up to %ux%u, FIFO of %u KB\n",
           sc->vram_size >> 20, sc->max_width, sc->max_height,
           sc->fifo_size >> 10);
  drm_client_setup(dev, NULL);
  INIT_DELAYED_WORK(&sc->flush, jos_vmsvga_flush_work);
  schedule_delayed_work(&sc->flush, msecs_to_jiffies(JOS_VMSVGA_FLUSH_MS));
  return 0;
}

static void jos_vmsvga_pci_remove(struct pci_dev* pdev) {
  struct drm_device* dev = pci_get_drvdata(pdev);
  struct jos_vmsvga* sc = to_jos_vmsvga(dev);

  cancel_delayed_work_sync(&sc->flush);
  drm_dev_unplug(dev);
  drm_atomic_helper_shutdown(dev);
  mutex_lock(&sc->hw_lock);
  jos_vmsvga_screen_off(sc);
  jos_vmsvga_unmap(sc);
  mutex_unlock(&sc->hw_lock);
  mutex_destroy(&sc->hw_lock);
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
