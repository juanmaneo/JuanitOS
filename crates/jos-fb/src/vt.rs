// SPDX-License-Identifier: BSD-3-Clause
//! FreeBSD `vt(4)` framebuffer backend.
//!
//! The daemon takes a virtual terminal nobody uses (`ttyv9` by default:
//! `/etc/ttys` runs gettys on `ttyv0`–`ttyv7` and the greeter picks the
//! first free one, `ttyv8`), maps the framebuffer, and with the first
//! frame switches to it in graphics mode, so console output never dirties
//! the shown picture. Keys typed on that terminal are read in raw mode so Esc can
//! drop the user on the text console; the keyboards' evdev nodes bring the
//! Esc that takes the screen back ([`VtDisplay::listen_keyboards`]).
//!
//! All `unsafe` is in this file and limited to the system calls themselves
//! (`ioctl`, `mmap`, `munmap`, `tcgetattr`/`tcsetattr`, `fcntl`); every
//! argument is a plain integer, a pointer to a local `#[repr(C)]` struct,
//! or the mapping returned by the kernel.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::thread::{self, ScopedJoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use jos_gfx::{Image, Rgba};

use crate::mouse::{self, Mouse};
use crate::{evdev, ioc, last_pixel, pack_pixel, pack_row, Display, RgbOffsets};

/// Where `moused` publishes every mouse and touchpad.
pub const SYSMOUSE: &str = "/dev/sysmouse";

/// Where `evdev(4)` publishes the keyboards (and mice): `event*` nodes.
pub const INPUT_DIR: &str = "/dev/input";

/// The text console, `ttyv0`, as a 1-based terminal number.
pub const CONSOLE_VT: i32 = 1;

/// `struct fbtype` from `sys/fbio.h`.
#[repr(C)]
#[derive(Default)]
struct FbType {
    fb_type: i32,
    fb_height: i32,
    fb_width: i32,
    fb_depth: i32,
    fb_cmsize: i32,
    fb_size: i32,
}

/// `struct fb_rgboffs` from `sys/fbio.h`.
#[repr(C)]
#[derive(Default)]
struct FbRgbOffs {
    red: i32,
    green: i32,
    blue: i32,
}

/// How long the kernel is given for its post-switch repaint of the
/// terminal (vt(4) timer: 25 Hz, so a few ticks, then the repaint itself)
/// once the switch calls have returned, before the rows it has not
/// reached are given up on.
pub const POST_SWITCH_SETTLE: Duration = Duration::from_millis(200);

/// How long [`Display::needs_redraw`] keeps asking for the frame again
/// after the user switched back to the terminal, or after the display
/// opened on a terminal that was on screen already (the login screen
/// after a desktop session): the kernel repaints its terminal from the
/// 25 Hz timer in both cases, and a modeset restored by the GPU driver
/// when the compositor exits can take a good while longer than a plain
/// switch ([`POST_SWITCH_SETTLE`]).
pub const RETURN_SETTLE: Duration = Duration::from_millis(1000);

/// Planted in the last pixel of every row before a switch: the repaint
/// paints black over it (rows of blank cells, then the border, top to
/// bottom), and a row whose marker is gone is rewritten at once. Nearly
/// black, so invisible for the moment it is there.
const MARKER: Rgba = Rgba::rgb(1, 1, 1);

/// A row waiting for the repaint, with its marker's offset.
type Pending = Vec<(u32, usize)>;

pub struct VtDisplay {
    tty: File,
    vt: i32,
    width: u32,
    height: u32,
    stride: usize,
    offs: RgbOffsets,
    map: *mut u8,
    map_len: usize,
    saved_termios: libc::termios,
    saved_flags: libc::c_int,
    row_buf: Vec<u8>,
    console_restored: bool,
    mouse: Option<File>,
    keyboards: Vec<File>,
    /// The terminal is on screen with a frame of ours. Cleared by `open`,
    /// [`Display::restore_console`] and [`Display::reclaim`]: the next
    /// `present` switches to it, with that frame ([`VtDisplay::switch_under`]).
    switched: bool,
    /// The user switched to another terminal (Alt+F1) while `switched`:
    /// frames are held back until they come back.
    away: bool,
    /// Until when [`Display::needs_redraw`] asks for the frame again.
    repaint_until: Option<Instant>,
}

fn ioctl_int(fd: libc::c_int, req: u32, value: libc::c_int, what: &str) -> anyhow::Result<()> {
    // SAFETY: `_IOWINT` ioctls take the integer argument by value.
    let rc = unsafe { libc::ioctl(fd, req as libc::c_ulong, value) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("ioctl {what}"));
    }
    Ok(())
}

fn ioctl_out<T: Default>(fd: libc::c_int, req: u32, what: &str) -> anyhow::Result<T> {
    let mut out = T::default();
    // SAFETY: `req` encodes `size_of::<T>()` as the length and the kernel
    // writes exactly that many bytes into `out`, which outlives the call.
    let rc = unsafe { libc::ioctl(fd, req as libc::c_ulong, &mut out as *mut T) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("ioctl {what}"));
    }
    Ok(out)
}

/// Switch to terminal `vt` (the one `fd` is open on) in graphics mode.
/// vt(4) marks the screen invalid on the switch and repaints it (blank
/// cells top to bottom, border, cursor) from its 25 Hz timer, regardless
/// of `KD_GRAPHICS`, wiping a frame drawn before. The timer is armed
/// inside `VT_ACTIVATE`, which goes on to update the keyboard and can
/// take longer than that (30–50 ms under QEMU), so the repaint may well
/// be over before this returns.
fn take_terminal(fd: libc::c_int, vt: i32) -> anyhow::Result<()> {
    ioctl_int(fd, ioc::VT_ACTIVATE, vt, "VT_ACTIVATE")?;
    ioctl_int(fd, ioc::VT_WAITACTIVE, vt, "VT_WAITACTIVE")?;
    ioctl_int(fd, ioc::KDSETMODE, ioc::KD_GRAPHICS, "KDSETMODE")
}

fn join(switch: ScopedJoinHandle<'_, anyhow::Result<()>>) -> anyhow::Result<()> {
    switch
        .join()
        .map_err(|_| anyhow::anyhow!("the terminal switch thread panicked"))?
}

/// Text mode on `fd`'s terminal, then the console on screen.
fn show_console(fd: libc::c_int) -> anyhow::Result<()> {
    ioctl_int(fd, ioc::KDSETMODE, ioc::KD_TEXT, "KDSETMODE")?;
    ioctl_int(fd, ioc::VT_ACTIVATE, CONSOLE_VT, "VT_ACTIVATE")
}

impl VtDisplay {
    /// Open `/dev/ttyv{vt-1}`, switch to it and map its framebuffer.
    pub fn open(vt: i32) -> anyhow::Result<Self> {
        if !(1..=16).contains(&vt) {
            bail!("virtual terminal number must be in 1..=16, got {vt}");
        }
        let path = format!("/dev/ttyv{}", vt - 1);
        Self::open_path(Path::new(&path), vt)
    }

    fn open_path(path: &Path, vt: i32) -> anyhow::Result<Self> {
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let fd = tty.as_raw_fd();

        let fb: FbType = ioctl_out(fd, ioc::FBIOGTYPE, "FBIOGTYPE")?;
        if fb.fb_depth != 32 {
            bail!(
                "framebuffer depth {} is not supported (need 32 bpp)",
                fb.fb_depth
            );
        }
        if fb.fb_width <= 0 || fb.fb_height <= 0 || fb.fb_size <= 0 {
            bail!("framebuffer reports an empty geometry");
        }
        let width = fb.fb_width as u32;
        let height = fb.fb_height as u32;
        let stride: u32 =
            ioctl_out(fd, ioc::FBIO_GETLINEWIDTH, "FBIO_GETLINEWIDTH").unwrap_or(width * 4);
        let stride = stride as usize;
        let offs = match ioctl_out::<FbRgbOffs>(fd, ioc::FBIO_GETRGBOFFS, "FBIO_GETRGBOFFS") {
            Ok(o) if o.red >= 0 && o.green >= 0 && o.blue >= 0 => RgbOffsets {
                red: o.red as u32,
                green: o.green as u32,
                blue: o.blue as u32,
            },
            _ => RgbOffsets::BGRX,
        };
        let map_len = (fb.fb_size as usize).max(stride * height as usize);

        // SAFETY: a fresh, zero-initialised termios is a valid out-param.
        let mut saved_termios: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd is a valid open tty.
        if unsafe { libc::tcgetattr(fd, &mut saved_termios) } < 0 {
            return Err(std::io::Error::last_os_error()).context("tcgetattr");
        }
        // SAFETY: fcntl with F_GETFL takes no further arguments.
        let saved_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };

        // SAFETY: mapping `map_len` bytes of the device at offset 0; vt(4)
        // validates the range against the framebuffer size.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap framebuffer");
        }

        let mut raw = saved_termios;
        // SAFETY: cfmakeraw only writes into the struct it is given.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: fd is valid; failure only means Esc is not detected.
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };

        Ok(VtDisplay {
            tty,
            vt,
            width,
            height,
            stride,
            offs,
            map: map as *mut u8,
            map_len,
            saved_termios,
            saved_flags,
            row_buf: vec![0u8; stride],
            console_restored: false,
            mouse: None,
            keyboards: Vec::new(),
            switched: false,
            away: false,
            repaint_until: None,
        })
    }

    /// Also read the keyboards through their evdev nodes (see
    /// [`evdev`]), so that Esc is seen while the console is on screen.
    /// Returns how many nodes were opened: none without `evdev` in the
    /// kernel, which only means the console cannot be left with Esc.
    pub fn listen_keyboards(&mut self) -> usize {
        let Ok(entries) = std::fs::read_dir(INPUT_DIR) else {
            return 0;
        };
        // Mice have nodes too; they never report KEY_ESC.
        self.keyboards = entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
            .filter_map(|e| {
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(e.path())
                    .ok()
            })
            .collect();
        self.keyboards.len()
    }

    /// Start reading the mouse from `/dev/sysmouse` (fails when `moused`
    /// is not running, which leaves the keyboard-only installer intact).
    pub fn enable_mouse(&mut self) -> anyhow::Result<()> {
        let dev = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(SYSMOUSE)
            .with_context(|| format!("opening {SYSMOUSE}"))?;
        self.mouse = Some(dev);
        Ok(())
    }

    fn fd(&self) -> libc::c_int {
        self.tty.as_raw_fd()
    }

    /// The terminal on screen, when the kernel says.
    fn active_vt(&self) -> Option<i32> {
        ioctl_out::<libc::c_int>(self.fd(), ioc::VT_GETACTIVE, "VT_GETACTIVE").ok()
    }

    /// Is another terminal on screen? Remembered (the last answer stands
    /// when the kernel gives none), so that [`Display::needs_redraw`] sees
    /// the user come back.
    fn is_away(&mut self) -> bool {
        if let Some(active) = self.active_vt() {
            self.away = active != self.vt;
        }
        self.away
    }

    fn restore_tty_settings(&self) {
        let fd = self.fd();
        // SAFETY: restoring values obtained from the same fd earlier.
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, &self.saved_termios);
            if self.saved_flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, self.saved_flags);
            }
        }
    }

    /// Write `frame`, then switch to the terminal and rewrite each row as
    /// the kernel's repaint goes over it (see [`take_terminal`]): the frame
    /// is on screen before the switch, and black only in the rows the
    /// repaint is at. The switch calls run on a thread of their own, since
    /// the repaint often lands before they return. Nothing is repainted
    /// when the terminal is on screen already (the installer draws on the
    /// splash's), so the frame is simply written then.
    fn switch_under(&mut self, frame: &Image) -> anyhow::Result<()> {
        let fd = self.fd();
        let active: libc::c_int = ioctl_out(fd, ioc::VT_GETACTIVE, "VT_GETACTIVE")?;
        self.present_rows(frame, 0..self.height)?;
        if active == self.vt {
            // Whatever left this terminal on screen (a compositor whose
            // exit restored the console's mode) may have the kernel
            // repainting it right now: keep presenting for a while.
            self.repaint_until = Some(Instant::now() + RETURN_SETTLE);
            return ioctl_int(fd, ioc::KDSETMODE, ioc::KD_GRAPHICS, "KDSETMODE");
        }
        let mut pending = self.plant_markers();
        let vt = self.vt;
        thread::scope(|s| {
            let switch = s.spawn(move || take_terminal(fd, vt));
            self.chase_repaint(frame, &mut pending, switch)
        })?;
        if pending.is_empty() {
            return Ok(());
        }
        // Rows the repaint never reached (or the whole frame once
        // POST_SWITCH_SETTLE is over): their markers go away with this.
        self.present_rows(frame, 0..self.height)
    }

    /// [`MARKER`] in the last pixel of every row.
    fn plant_markers(&mut self) -> Pending {
        let marker = pack_pixel(MARKER, self.offs);
        let rows: Pending = (0..self.height)
            .filter_map(|y| {
                last_pixel(self.width, self.stride, self.map_len, y).map(|off| (y, off))
            })
            .collect();
        for &(_, off) in &rows {
            // SAFETY: `last_pixel` keeps `off` word-aligned with
            // `off + 4 <= map_len`, and the mapping lives as long as `self`.
            unsafe { std::ptr::write_volatile(self.map.add(off) as *mut u32, marker) };
        }
        rows
    }

    /// Rewrite the rows whose marker the repaint has painted over, as it
    /// goes, until none is left in `pending` or the kernel has had
    /// [`POST_SWITCH_SETTLE`] since `switch` returned. Checked from the
    /// top on every pass, since the border rows (top one included) come
    /// after the cells.
    fn chase_repaint(
        &mut self,
        frame: &Image,
        pending: &mut Pending,
        switch: ScopedJoinHandle<'_, anyhow::Result<()>>,
    ) -> anyhow::Result<()> {
        let marker = pack_pixel(MARKER, self.offs);
        let mut switch = Some(switch);
        let mut deadline = None;
        let mut wiped = Vec::new();
        while !pending.is_empty() {
            if deadline.is_none() && switch.as_ref().is_some_and(|s| s.is_finished()) {
                if let Some(s) = switch.take() {
                    join(s)?;
                }
                deadline = Some(Instant::now() + POST_SWITCH_SETTLE);
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            pending.retain(|&(y, off)| {
                // SAFETY: as in `plant_markers`; volatile because the
                // kernel writes there.
                let now = unsafe { std::ptr::read_volatile(self.map.add(off) as *const u32) };
                if now == marker {
                    return true;
                }
                wiped.push(y);
                false
            });
            for y in wiped.drain(..) {
                self.present_rows(frame, y..y + 1)?;
            }
            thread::yield_now();
        }
        match switch.take() {
            Some(s) => join(s),
            None => Ok(()),
        }
    }
}

impl Display for VtDisplay {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn present(&mut self, frame: &Image) -> anyhow::Result<()> {
        if self.switched {
            // Another terminal is on screen: the frame would go over its
            // text. It is presented again when the user comes back.
            if self.is_away() {
                return Ok(());
            }
            return self.present_rows(frame, 0..self.height);
        }
        self.switch_under(frame)?;
        self.switched = true;
        Ok(())
    }

    fn present_rows(&mut self, frame: &Image, rows: Range<u32>) -> anyhow::Result<()> {
        anyhow::ensure!(
            frame.width() == self.width && frame.height() == self.height,
            "frame {}x{} does not match the framebuffer {}x{}",
            frame.width(),
            frame.height(),
            self.width,
            self.height
        );
        let row_bytes = (self.width as usize) * 4;
        for y in rows.start..rows.end.min(self.height) {
            let Some(row) = frame.row(y) else { break };
            let Some(buf) = self.row_buf.get_mut(..row_bytes) else {
                break;
            };
            pack_row(row, self.offs, buf);
            let off = y as usize * self.stride;
            if off + row_bytes > self.map_len {
                break;
            }
            // SAFETY: `off + row_bytes <= map_len`, the mapping is live for
            // the lifetime of `self`, and `buf` is a distinct heap buffer.
            unsafe {
                std::ptr::copy_nonoverlapping(buf.as_ptr(), self.map.add(off), row_bytes);
            }
        }
        Ok(())
    }

    fn restore_console(&mut self) -> anyhow::Result<()> {
        show_console(self.fd())?;
        self.console_restored = true;
        self.switched = false;
        self.away = false;
        self.repaint_until = None;
        Ok(())
    }

    fn reclaim(&mut self) -> anyhow::Result<()> {
        self.console_restored = false;
        self.switched = false;
        self.away = false;
        self.repaint_until = None;
        Ok(())
    }

    fn release(&mut self) -> anyhow::Result<()> {
        ioctl_int(self.fd(), ioc::KDSETMODE, ioc::KD_TEXT, "KDSETMODE")?;
        self.restore_tty_settings();
        self.console_restored = true;
        Ok(())
    }

    fn poll_key(&mut self) -> Option<u8> {
        let mut b = [0u8; 1];
        match self.tty.read(&mut b) {
            Ok(1) => b.first().copied(),
            _ => None,
        }
    }

    fn poll_mouse(&mut self) -> Vec<Mouse> {
        let Some(dev) = self.mouse.as_mut() else {
            return Vec::new();
        };
        // sysmouse(4) hands out whole packets only.
        let mut buf = [0u8; mouse::PACKET * 64];
        match dev.read(&mut buf) {
            Ok(n) => mouse::decode(buf.get(..n).unwrap_or(&[])),
            Err(_) => Vec::new(),
        }
    }

    fn escape_pressed(&mut self) -> bool {
        let mut pressed = false;
        let mut buf = [0u8; evdev::EVENT * 64];
        for dev in &mut self.keyboards {
            // Every node is drained, so a press is never seen twice.
            while let Ok(n) = dev.read(&mut buf) {
                if n == 0 {
                    break;
                }
                pressed |= evdev::pressed(buf.get(..n).unwrap_or(&[])).contains(&evdev::KEY_ESC);
            }
        }
        pressed
    }

    /// vt(4) repaints a terminal the user switches back to (blank cells,
    /// border, cursor; see [`take_terminal`]) whatever its mode, which
    /// wipes the frame: once the terminal is on screen again the frame is
    /// asked for on every tick for [`RETURN_SETTLE`], and once more after
    /// it, so the last one lands after the repaint.
    fn needs_redraw(&mut self) -> bool {
        if !self.switched || self.console_restored {
            return false;
        }
        let was_away = self.away;
        if self.is_away() {
            return false;
        }
        if was_away {
            self.repaint_until = Some(Instant::now() + RETURN_SETTLE);
        }
        match self.repaint_until {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                self.repaint_until = None;
                true
            }
            None => false,
        }
    }
}

impl Drop for VtDisplay {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what mmap returned.
        unsafe {
            libc::munmap(self.map as *mut libc::c_void, self.map_len);
        }
        self.restore_tty_settings();
        // A crash must not leave the console in graphics mode on a dead VT;
        // a deliberate `finish keep` is the only case where we stay.
        if !self.console_restored && std::thread::panicking() {
            let _ = show_console(self.fd());
        }
    }
}
