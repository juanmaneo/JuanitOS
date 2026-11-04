/*-
 * SPDX-License-Identifier: BSD-3-Clause
 *
 * Copyright (c) 2026, Juanito and the JuanitOS contributors (see AUTHORS)
 *
 * jos_kmsfb: the KMS side of JuanitOS's frame buffer drivers (see
 * jos_kmsfb.h). Compiled into each driver module (kmsfb.mk).
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/kernel.h>

#include <vm/vm.h>
#include <vm/vm_object.h>
#include <vm/vm_page.h>

#include <linux/gfp.h>
#include <linux/io.h>
#include <linux/iosys-map.h>
#include <linux/mm.h>
#include <linux/slab.h>
#include <linux/vmalloc.h>

#include <drm/drm_aperture.h>
#include <drm/drm_atomic.h>
#include <drm/drm_atomic_helper.h>
#include <drm/drm_atomic_state_helper.h>
#include <drm/drm_client_setup.h>
#include <drm/drm_damage_helper.h>
#include <drm/drm_edid.h>
#include <drm/drm_fb_helper.h>
#include <drm/drm_fourcc.h>
#include <drm/drm_framebuffer.h>
#include <drm/drm_gem.h>
#include <drm/drm_gem_framebuffer_helper.h>
#include <drm/drm_ioctl.h>
#include <drm/drm_managed.h>
#include <drm/drm_mode_config.h>
#include <drm/drm_modes.h>
#include <drm/drm_modeset_helper_vtables.h>
#include <drm/drm_print.h>
#include <drm/drm_probe_helper.h>
#include <drm/drm_vblank.h>
#include <drm/drm_vma_manager.h>

#include "jos_kmsfb.h"

#define JOS_KMSFB_DEPTH 24

/* How often a mapped console shadow is pushed to the screen. */
#define JOS_KMSFB_FLUSH_MS 50

/* Pages inserted into a user mapping per fault, from the faulting one. */
#define JOS_KMSFB_PREFAULT 16

/* 16.16 fixed point of a plane's source rectangle. */
#define JOS_KMSFB_FIXED_SHIFT 16

struct jos_kmsfb_bo {
  struct drm_gem_object base;
  struct page** pages;
  unsigned int npages;
  void* vaddr;
};

static struct jos_kmsfb* to_jos_kmsfb(struct drm_device* dev) {
  return container_of(dev, struct jos_kmsfb, drm);
}

static struct jos_kmsfb_bo* to_jos_kmsfb_bo(struct drm_gem_object* obj) {
  return container_of(obj, struct jos_kmsfb_bo, base);
}

/*
 * GEM objects: page arrays in system memory.
 */

static void jos_kmsfb_gem_free(struct drm_gem_object* obj) {
  struct jos_kmsfb_bo* bo = to_jos_kmsfb_bo(obj);
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

static int jos_kmsfb_gem_vmap(struct drm_gem_object* obj,
                              struct iosys_map* map) {
  iosys_map_set_vaddr(map, to_jos_kmsfb_bo(obj)->vaddr);
  return 0;
}

static void jos_kmsfb_gem_vunmap(struct drm_gem_object* obj __unused,
                                 struct iosys_map* map __unused) {}

/*
 * Insert the faulting page and a few after it into the mapping's VM
 * object, as drm-kmod's TTM does: the pages become managed (the VM
 * tracks their mappings) and stay wired, so they outlive the object.
 * The mapping starts at the object's fake offset; vm_pgoff holds it.
 */
static int jos_kmsfb_gem_fault(struct vm_fault* vmf) {
  struct vm_area_struct* vma = vmf->vma;
  struct drm_gem_object* obj = vma->vm_private_data;
  struct jos_kmsfb_bo* bo = to_jos_kmsfb_bo(obj);
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
  for (i = 0; i < JOS_KMSFB_PREFAULT && page_offset < page_last; i++) {
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

static const struct vm_operations_struct jos_kmsfb_gem_vm_ops = {
    .fault = jos_kmsfb_gem_fault,
    .open = drm_gem_vm_open,
    .close = drm_gem_vm_close,
};

/* Cached, like the kernel's own mapping of the pages. */
static int jos_kmsfb_gem_mmap(struct drm_gem_object* obj __unused,
                              struct vm_area_struct* vma) {
  vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
  vma->vm_page_prot = vm_get_page_prot(vma->vm_flags);
  return 0;
}

static const struct drm_gem_object_funcs jos_kmsfb_gem_funcs = {
    .free = jos_kmsfb_gem_free,
    .vmap = jos_kmsfb_gem_vmap,
    .vunmap = jos_kmsfb_gem_vunmap,
    .mmap = jos_kmsfb_gem_mmap,
    .vm_ops = &jos_kmsfb_gem_vm_ops,
};

static struct jos_kmsfb_bo* jos_kmsfb_bo_create(struct drm_device* dev,
                                                size_t size) {
  struct jos_kmsfb_bo* bo;
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
  bo->base.funcs = &jos_kmsfb_gem_funcs;
  for (i = 0; i < bo->npages; i++) {
    bo->pages[i] = alloc_page(GFP_KERNEL | __GFP_ZERO);
    if (NULL == bo->pages[i]) {
      jos_kmsfb_gem_free(&bo->base);
      return ERR_PTR(-ENOMEM);
    }
  }
  bo->vaddr = vmap(bo->pages, bo->npages, VM_MAP, PAGE_KERNEL);
  if (NULL == bo->vaddr) {
    jos_kmsfb_gem_free(&bo->base);
    return ERR_PTR(-ENOMEM);
  }
  return bo;
}

int jos_kmsfb_dumb_create(struct drm_file* file, struct drm_device* dev,
                          struct drm_mode_create_dumb* args) {
  struct jos_kmsfb* kms = to_jos_kmsfb(dev);
  struct jos_kmsfb_bo* bo;
  int ret;

  if (JOS_KMSFB_BPP != args->bpp || 0 == args->width || 0 == args->height ||
      args->width > kms->max_width || args->height > kms->max_height) {
    return -EINVAL;
  }
  args->pitch = args->width * JOS_KMSFB_CPP;
  args->size = PAGE_ALIGN((u64)args->pitch * args->height);
  bo = jos_kmsfb_bo_create(dev, args->size);
  if (IS_ERR(bo)) {
    return (int)PTR_ERR(bo);
  }
  ret = drm_gem_handle_create(file, &bo->base, &args->handle);
  drm_gem_object_put(&bo->base);
  return ret;
}

const struct file_operations jos_kmsfb_fops = {
    .owner = THIS_MODULE,
    DRM_GEM_FOPS,
};

/*
 * Mode setting.
 */

/*
 * Copy a damaged rectangle of the plane's buffer to the screen. False
 * when none of it is on screen; `rect` is the screen's rectangle
 * otherwise.
 */
static bool jos_kmsfb_blit(struct jos_kmsfb* kms,
                           const struct drm_plane_state* state,
                           const struct drm_rect* clip, struct drm_rect* rect) {
  struct drm_framebuffer* fb = state->fb;
  const struct jos_kmsfb_bo* bo = to_jos_kmsfb_bo(drm_gem_fb_get_obj(fb, 0));
  u32 src_x1 = (u32)(state->src.x1 >> JOS_KMSFB_FIXED_SHIFT);
  u32 src_y1 = (u32)(state->src.y1 >> JOS_KMSFB_FIXED_SHIFT);
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
    return false;
  }
  x = (u32)clip->x1 - src_x1;
  y = (u32)clip->y1 - src_y1;
  if (x >= kms->width || y >= kms->height) {
    return false;
  }
  w = min((u32)drm_rect_width(clip), kms->width - x);
  h = min((u32)drm_rect_height(clip), kms->height - y);
  src = (const u8*)bo->vaddr + fb->offsets[0] +
        ((size_t)clip->y1 * fb->pitches[0]) +
        ((size_t)clip->x1 * JOS_KMSFB_CPP);
  dst = kms->screen + ((size_t)y * kms->pitch) + ((size_t)x * JOS_KMSFB_CPP);
  len = (size_t)w * JOS_KMSFB_CPP;
  for (row = 0; row < h; row++) {
    memcpy_toio(dst, src, len);
    src += fb->pitches[0];
    dst += kms->pitch;
  }
  drm_rect_init(rect, (int)x, (int)y, (int)w, (int)h);
  return true;
}

static int jos_kmsfb_plane_atomic_check(struct drm_plane* plane,
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

static void jos_kmsfb_plane_atomic_update(struct drm_plane* plane,
                                          struct drm_atomic_state* state) {
  struct jos_kmsfb* kms = to_jos_kmsfb(plane->dev);
  struct drm_plane_state* old_state =
      drm_atomic_get_old_plane_state(state, plane);
  struct drm_plane_state* new_state =
      drm_atomic_get_new_plane_state(state, plane);
  struct drm_atomic_helper_damage_iter iter;
  struct drm_rect clip;
  struct drm_rect rect;
  int idx;

  if (NULL == new_state->fb || !new_state->visible) {
    WRITE_ONCE(kms->scanout_fb, NULL);
    return;
  }
  WRITE_ONCE(kms->scanout_fb, new_state->fb);
  if (!drm_dev_enter(&kms->drm, &idx)) {
    return;
  }
  mutex_lock(&kms->hw_lock);
  if (0 != kms->width) {
    drm_atomic_helper_damage_iter_init(&iter, old_state, new_state);
    drm_atomic_for_each_plane_damage(&iter, &clip) {
      if (jos_kmsfb_blit(kms, new_state, &clip, &rect) &&
          NULL != kms->funcs->damaged) {
        kms->funcs->damaged(kms, &rect);
      }
    }
    if (NULL != kms->funcs->flushed) {
      kms->funcs->flushed(kms);
    }
  }
  mutex_unlock(&kms->hw_lock);
  drm_dev_exit(idx);
}

static void
jos_kmsfb_plane_atomic_disable(struct drm_plane* plane,
                               struct drm_atomic_state* state __unused) {
  struct jos_kmsfb* kms = to_jos_kmsfb(plane->dev);

  WRITE_ONCE(kms->scanout_fb, NULL);
}

static const struct drm_plane_helper_funcs jos_kmsfb_plane_helper_funcs = {
    .atomic_check = jos_kmsfb_plane_atomic_check,
    .atomic_update = jos_kmsfb_plane_atomic_update,
    .atomic_disable = jos_kmsfb_plane_atomic_disable,
};

static const struct drm_plane_funcs jos_kmsfb_plane_funcs = {
    .update_plane = drm_atomic_helper_update_plane,
    .disable_plane = drm_atomic_helper_disable_plane,
    .destroy = drm_plane_cleanup,
    .reset = drm_atomic_helper_plane_reset,
    .atomic_duplicate_state = drm_atomic_helper_plane_duplicate_state,
    .atomic_destroy_state = drm_atomic_helper_plane_destroy_state,
};

static const u32 jos_kmsfb_formats[] = {
    DRM_FORMAT_XRGB8888,
};

static enum drm_mode_status jos_kmsfb_mode_fits(const struct jos_kmsfb* kms,
                                                u32 width, u32 height) {
  if (width > kms->max_width || height > kms->max_height) {
    return MODE_BAD;
  }
  if (kms->fixed && (width != kms->max_width || height != kms->max_height)) {
    return MODE_BAD;
  }
  if ((u64)width * height * JOS_KMSFB_CPP > kms->mem_size) {
    return MODE_MEM;
  }
  return MODE_OK;
}

static enum drm_mode_status
jos_kmsfb_crtc_mode_valid(struct drm_crtc* crtc,
                          const struct drm_display_mode* mode) {
  return jos_kmsfb_mode_fits(to_jos_kmsfb(crtc->dev), mode->hdisplay,
                             mode->vdisplay);
}

static int jos_kmsfb_crtc_atomic_check(struct drm_crtc* crtc,
                                       struct drm_atomic_state* state) {
  struct drm_crtc_state* crtc_state =
      drm_atomic_get_new_crtc_state(state, crtc);

  if (!crtc_state->enable) {
    return 0;
  }
  return drm_atomic_helper_check_crtc_primary_plane(crtc_state);
}

static void jos_kmsfb_crtc_atomic_enable(struct drm_crtc* crtc,
                                         struct drm_atomic_state* state) {
  struct jos_kmsfb* kms = to_jos_kmsfb(crtc->dev);
  struct drm_crtc_state* crtc_state =
      drm_atomic_get_new_crtc_state(state, crtc);
  const struct drm_display_mode* mode = &crtc_state->mode;
  u32 width = mode->hdisplay;
  u32 height = mode->vdisplay;
  int idx;

  if (!drm_dev_enter(&kms->drm, &idx)) {
    return;
  }
  mutex_lock(&kms->hw_lock);
  if (NULL != kms->funcs->enable &&
      0 != kms->funcs->enable(kms, width, height)) {
    width = 0;
    height = 0;
  }
  kms->width = width;
  kms->height = height;
  mutex_unlock(&kms->hw_lock);
  drm_dev_exit(idx);
}

static void jos_kmsfb_screen_off(struct jos_kmsfb* kms) {
  if (NULL != kms->funcs->disable) {
    kms->funcs->disable(kms);
  }
  kms->width = 0;
  kms->height = 0;
}

static void
jos_kmsfb_crtc_atomic_disable(struct drm_crtc* crtc,
                              struct drm_atomic_state* state __unused) {
  struct jos_kmsfb* kms = to_jos_kmsfb(crtc->dev);
  int idx;

  if (!drm_dev_enter(&kms->drm, &idx)) {
    return;
  }
  mutex_lock(&kms->hw_lock);
  jos_kmsfb_screen_off(kms);
  mutex_unlock(&kms->hw_lock);
  drm_dev_exit(idx);
}

/* No vertical blank interrupt: a commit's event is sent when it is done. */
static void jos_kmsfb_crtc_atomic_flush(struct drm_crtc* crtc,
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

static const struct drm_crtc_helper_funcs jos_kmsfb_crtc_helper_funcs = {
    .mode_valid = jos_kmsfb_crtc_mode_valid,
    .atomic_check = jos_kmsfb_crtc_atomic_check,
    .atomic_enable = jos_kmsfb_crtc_atomic_enable,
    .atomic_disable = jos_kmsfb_crtc_atomic_disable,
    .atomic_flush = jos_kmsfb_crtc_atomic_flush,
};

static const struct drm_crtc_funcs jos_kmsfb_crtc_funcs = {
    .reset = drm_atomic_helper_crtc_reset,
    .destroy = drm_crtc_cleanup,
    .set_config = drm_atomic_helper_set_config,
    .page_flip = drm_atomic_helper_page_flip,
    .atomic_duplicate_state = drm_atomic_helper_crtc_duplicate_state,
    .atomic_destroy_state = drm_atomic_helper_crtc_destroy_state,
};

static int jos_kmsfb_connector_get_modes(struct drm_connector* connector) {
  struct jos_kmsfb* kms = to_jos_kmsfb(connector->dev);
  int count;

  if (kms->fixed) {
    const struct drm_display_mode mode = {
        DRM_SIMPLE_MODE((u16)kms->max_width, (u16)kms->max_height, 0, 0),
    };

    return drm_connector_helper_get_modes_fixed(connector, &mode);
  }
  count = drm_add_modes_noedid(connector, (int)kms->max_width,
                               (int)kms->max_height);
  if (MODE_OK ==
      jos_kmsfb_mode_fits(kms, kms->preferred_width, kms->preferred_height)) {
    drm_set_preferred_mode(connector, (int)kms->preferred_width,
                           (int)kms->preferred_height);
  }
  return count;
}

static enum drm_mode_status
jos_kmsfb_connector_mode_valid(struct drm_connector* connector,
                               struct drm_display_mode* mode) {
  return jos_kmsfb_mode_fits(to_jos_kmsfb(connector->dev), mode->hdisplay,
                             mode->vdisplay);
}

static const struct drm_connector_helper_funcs
    jos_kmsfb_connector_helper_funcs = {
        .get_modes = jos_kmsfb_connector_get_modes,
        .mode_valid = jos_kmsfb_connector_mode_valid,
};

static const struct drm_connector_funcs jos_kmsfb_connector_funcs = {
    .reset = drm_atomic_helper_connector_reset,
    .fill_modes = drm_helper_probe_single_connector_modes,
    .destroy = drm_connector_cleanup,
    .atomic_duplicate_state = drm_atomic_helper_connector_duplicate_state,
    .atomic_destroy_state = drm_atomic_helper_connector_destroy_state,
};

static const struct drm_encoder_funcs jos_kmsfb_encoder_funcs = {
    .destroy = drm_encoder_cleanup,
};

static const struct drm_framebuffer_funcs jos_kmsfb_fb_funcs = {
    .destroy = drm_gem_fb_destroy,
    .create_handle = drm_gem_fb_create_handle,
    .dirty = drm_atomic_helper_dirtyfb,
};

/*
 * Linux's drm_gem_fb_create_with_dirty(), which drm-kmod declares but its
 * drm.ko does not have (its GEM framebuffer helper only brings
 * drm_gem_fb_init_with_funcs()). That helper also skips Linux's check that
 * the buffer object holds the framebuffer, which jos_kmsfb_blit() relies
 * on: done here. drm_internal_framebuffer_create() has already checked the
 * format (XRGB8888, one plane) and the pitch.
 */
static struct drm_framebuffer*
jos_kmsfb_fb_create(struct drm_device* dev, struct drm_file* file,
                    const struct drm_mode_fb_cmd2* mode_cmd) {
  struct drm_framebuffer* fb;
  int ret;

  fb = kzalloc(sizeof(*fb), GFP_KERNEL);
  if (NULL == fb) {
    return ERR_PTR(-ENOMEM);
  }
  ret =
      drm_gem_fb_init_with_funcs(dev, fb, file, mode_cmd, &jos_kmsfb_fb_funcs);
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

static const struct drm_mode_config_funcs jos_kmsfb_mode_config_funcs = {
    .fb_create = jos_kmsfb_fb_create,
    .atomic_check = drm_atomic_helper_check,
    .atomic_commit = drm_atomic_helper_commit,
};

static int jos_kmsfb_modeset_init(struct jos_kmsfb* kms) {
  struct drm_device* dev = &kms->drm;
  int ret;

  ret = drmm_mode_config_init(dev);
  if (0 != ret) {
    return ret;
  }
  dev->mode_config.max_width = (int)kms->max_width;
  dev->mode_config.max_height = (int)kms->max_height;
  dev->mode_config.preferred_depth = JOS_KMSFB_DEPTH;
  dev->mode_config.funcs = &jos_kmsfb_mode_config_funcs;

  ret = drm_universal_plane_init(dev, &kms->plane, 0, &jos_kmsfb_plane_funcs,
                                 jos_kmsfb_formats, nitems(jos_kmsfb_formats),
                                 NULL, DRM_PLANE_TYPE_PRIMARY, NULL);
  if (0 != ret) {
    return ret;
  }
  drm_plane_helper_add(&kms->plane, &jos_kmsfb_plane_helper_funcs);
  drm_plane_enable_fb_damage_clips(&kms->plane);

  ret = drm_crtc_init_with_planes(dev, &kms->crtc, &kms->plane, NULL,
                                  &jos_kmsfb_crtc_funcs, NULL);
  if (0 != ret) {
    return ret;
  }
  drm_crtc_helper_add(&kms->crtc, &jos_kmsfb_crtc_helper_funcs);

  ret = drm_encoder_init(dev, &kms->encoder, &jos_kmsfb_encoder_funcs,
                         DRM_MODE_ENCODER_VIRTUAL, NULL);
  if (0 != ret) {
    return ret;
  }
  kms->encoder.possible_crtcs = drm_crtc_mask(&kms->crtc);

  ret = drm_connector_init(dev, &kms->connector, &jos_kmsfb_connector_funcs,
                           DRM_MODE_CONNECTOR_VIRTUAL);
  if (0 != ret) {
    return ret;
  }
  drm_connector_helper_add(&kms->connector, &jos_kmsfb_connector_helper_funcs);
  ret = drm_connector_attach_encoder(&kms->connector, &kms->encoder);
  if (0 != ret) {
    return ret;
  }
  drm_mode_config_reset(dev);
  return 0;
}

/*
 * The console shadow: pushed to the screen while the console's frame
 * buffer is the one shown (see jos_kmsfb.h).
 */
static void jos_kmsfb_flush_work(struct work_struct* work) {
  struct jos_kmsfb* kms = container_of(work, struct jos_kmsfb, flush.work);
  struct drm_fb_helper* helper = kms->drm.fb_helper;

  if (NULL != helper && NULL != helper->info && NULL != helper->fb &&
      READ_ONCE(kms->scanout_fb) == helper->fb) {
    drm_fb_helper_damage_area(helper->info, 0, 0, helper->fb->width,
                              helper->fb->height);
  }
  schedule_delayed_work(&kms->flush, msecs_to_jiffies(JOS_KMSFB_FLUSH_MS));
}

/*
 * The device.
 */

void* jos_kmsfb_pci_alloc(struct pci_dev* pdev, const struct drm_driver* driver,
                          size_t size, size_t offset) {
  void* sc;
  int ret;

  ret = drm_aperture_remove_conflicting_pci_framebuffers(pdev, driver);
  if (0 != ret) {
    return ERR_PTR(ret);
  }
  sc = __devm_drm_dev_alloc(&pdev->dev, driver, size, offset);
  if (IS_ERR(sc)) {
    return sc;
  }
  pci_set_drvdata(pdev, (char*)sc + offset);
  ret = pcim_enable_device(pdev);
  if (0 != ret) {
    return ERR_PTR(ret);
  }
  return sc;
}

struct jos_kmsfb* jos_kmsfb_of_pci(struct pci_dev* pdev) {
  return to_jos_kmsfb(pci_get_drvdata(pdev));
}

int jos_kmsfb_register(struct jos_kmsfb* kms,
                       const struct jos_kmsfb_funcs* funcs) {
  int ret;

  kms->funcs = funcs;
  mutex_init(&kms->hw_lock);
  ret = jos_kmsfb_modeset_init(kms);
  if (0 == ret) {
    ret = drm_dev_register(&kms->drm, 0);
  }
  if (0 != ret) {
    mutex_destroy(&kms->hw_lock);
    return ret;
  }
  drm_client_setup(&kms->drm, NULL);
  INIT_DELAYED_WORK(&kms->flush, jos_kmsfb_flush_work);
  schedule_delayed_work(&kms->flush, msecs_to_jiffies(JOS_KMSFB_FLUSH_MS));
  return 0;
}

void jos_kmsfb_unregister(struct jos_kmsfb* kms) {
  cancel_delayed_work_sync(&kms->flush);
  drm_dev_unplug(&kms->drm);
  drm_atomic_helper_shutdown(&kms->drm);
  mutex_lock(&kms->hw_lock);
  jos_kmsfb_screen_off(kms);
  mutex_unlock(&kms->hw_lock);
  mutex_destroy(&kms->hw_lock);
}
