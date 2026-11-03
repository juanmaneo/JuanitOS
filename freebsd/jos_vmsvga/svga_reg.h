/*-
 * SPDX-License-Identifier: BSD-3-Clause
 *
 * Copyright (c) 2026, Juanito and the JuanitOS contributors (see AUTHORS)
 *
 * The part of the VMware SVGA II device interface jos_vmsvga uses: the
 * register numbers, capability bits, FIFO layout and the one FIFO command
 * it issues. The names and values follow VMware's svga_reg.h (MIT, as
 * shipped with xf86-video-vmware), which documents the whole interface.
 */

#ifndef JOS_VMSVGA_SVGA_REG_H
#define JOS_VMSVGA_SVGA_REG_H

/* PCI identity. */
#define SVGA_PCI_VENDOR 0x15ad
#define SVGA_PCI_DEVICE 0x0405

/* BAR 0 holds two I/O ports; registers are read and written through them. */
#define SVGA_INDEX_PORT 0x0
#define SVGA_VALUE_PORT 0x1

/* Negotiated through SVGA_REG_ID: write the version wanted, read it back. */
#define SVGA_MAGIC 0x900000U
#define SVGA_MAKE_ID(ver) ((SVGA_MAGIC << 8) | (ver))
#define SVGA_ID_2 SVGA_MAKE_ID(2)
#define SVGA_ID_INVALID 0xFFFFFFFFU

/* Registers (index port values). */
enum {
  SVGA_REG_ID = 0,
  SVGA_REG_ENABLE = 1,
  SVGA_REG_WIDTH = 2,
  SVGA_REG_HEIGHT = 3,
  SVGA_REG_MAX_WIDTH = 4,
  SVGA_REG_MAX_HEIGHT = 5,
  SVGA_REG_DEPTH = 6,
  SVGA_REG_BITS_PER_PIXEL = 7,
  SVGA_REG_BYTES_PER_LINE = 12,
  SVGA_REG_FB_OFFSET = 14,
  SVGA_REG_VRAM_SIZE = 15,
  SVGA_REG_FB_SIZE = 16,
  SVGA_REG_CAPABILITIES = 17,
  SVGA_REG_MEM_SIZE = 19,
  SVGA_REG_CONFIG_DONE = 20,
  SVGA_REG_SYNC = 21,
  SVGA_REG_BUSY = 22,
  SVGA_REG_GUEST_ID = 23,
  SVGA_REG_MEM_REGS = 30,
  SVGA_REG_NUM_GUEST_DISPLAYS = 34,
  SVGA_REG_DISPLAY_ID = 35,
  SVGA_REG_DISPLAY_IS_PRIMARY = 36,
  SVGA_REG_DISPLAY_POSITION_X = 37,
  SVGA_REG_DISPLAY_POSITION_Y = 38,
  SVGA_REG_DISPLAY_WIDTH = 39,
  SVGA_REG_DISPLAY_HEIGHT = 40,
  SVGA_REG_TRACES = 45,
};

#define SVGA_REG_ENABLE_DISABLE 0
#define SVGA_REG_ENABLE_ENABLE 1

/* SVGA_REG_CAPABILITIES bits. */
#define SVGA_CAP_EXTENDED_FIFO 0x00008000
#define SVGA_CAP_DISPLAY_TOPOLOGY 0x00080000

/*
 * The FIFO (BAR 2) starts with 32-bit registers, then holds commands;
 * MIN/MAX/NEXT_CMD/STOP are byte offsets into the FIFO. The command area
 * starts after the registers: these four, or SVGA_REG_MEM_REGS of them
 * when SVGA_CAP_EXTENDED_FIFO is set.
 */
enum {
  SVGA_FIFO_MIN = 0,
  SVGA_FIFO_MAX,
  SVGA_FIFO_NEXT_CMD,
  SVGA_FIFO_STOP,
};
#define SVGA_FIFO_LEGACY_REGS 4

/* Command: SVGA_CMD_UPDATE x y width height (all 32-bit). */
#define SVGA_CMD_UPDATE 1
#define SVGA_CMD_UPDATE_WORDS 5

#endif /* JOS_VMSVGA_SVGA_REG_H */
