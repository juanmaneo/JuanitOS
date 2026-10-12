//! FreeBSD `vt(4)` framebuffer backend.
//!
//! The daemon takes a virtual terminal nobody uses (`ttyv9` by default:
//! `/etc/ttys` runs gettys on `ttyv0`–`ttyv7` and the greeter picks the
//! first free one, `ttyv8`), switches to it, puts it in graphics mode so
//! console output never dirties the shown picture, and maps the
//! framebuffer. Keys typed on that terminal are read in raw mode so Esc can
//! drop the user on the text console.
//!
//! All `unsafe` is in this file and limited to the system calls themselves
//! (`ioctl`, `mmap`, `munmap`, `tcgetattr`/`tcsetattr`, `fcntl`); every
//! argument is a plain integer, a pointer to a local `#[repr(C)]` struct,
//! or the mapping returned by the kernel.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{bail, Context};
use jos_gfx::Image;

use crate::fb::{ioc, pack_row, Display, RgbOffsets};

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

pub struct VtDisplay {
    tty: File,
    vt: i32,
    previous_vt: i32,
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

        let previous_vt: i32 = ioctl_out(fd, ioc::VT_GETACTIVE, "VT_GETACTIVE").unwrap_or(1);
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

        ioctl_int(fd, ioc::VT_ACTIVATE, vt, "VT_ACTIVATE")?;
        ioctl_int(fd, ioc::VT_WAITACTIVE, vt, "VT_WAITACTIVE")?;
        ioctl_int(fd, ioc::KDSETMODE, ioc::KD_GRAPHICS, "KDSETMODE")?;

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
            let err = std::io::Error::last_os_error();
            let _ = ioctl_int(fd, ioc::KDSETMODE, ioc::KD_TEXT, "KDSETMODE");
            let _ = ioctl_int(fd, ioc::VT_ACTIVATE, previous_vt, "VT_ACTIVATE");
            return Err(err).context("mmap framebuffer");
        }

        let mut raw = saved_termios;
        // SAFETY: cfmakeraw only writes into the struct it is given.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: fd is valid; failure only means Esc is not detected.
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };

        Ok(VtDisplay {
            tty,
            vt,
            previous_vt,
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
        })
    }

    fn fd(&self) -> libc::c_int {
        self.tty.as_raw_fd()
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
}

impl Display for VtDisplay {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn present(&mut self, frame: &Image) -> anyhow::Result<()> {
        anyhow::ensure!(
            frame.width() == self.width && frame.height() == self.height,
            "frame {}x{} does not match the framebuffer {}x{}",
            frame.width(),
            frame.height(),
            self.width,
            self.height
        );
        let row_bytes = (self.width as usize) * 4;
        for y in 0..self.height {
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
        let fd = self.fd();
        ioctl_int(fd, ioc::KDSETMODE, ioc::KD_TEXT, "KDSETMODE")?;
        let target = if self.previous_vt >= 1 && self.previous_vt != self.vt {
            self.previous_vt
        } else {
            1
        };
        ioctl_int(fd, ioc::VT_ACTIVATE, target, "VT_ACTIVATE")?;
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
            let _ = ioctl_int(self.fd(), ioc::KDSETMODE, ioc::KD_TEXT, "KDSETMODE");
            let _ = ioctl_int(self.fd(), ioc::VT_ACTIVATE, 1, "VT_ACTIVATE");
        }
    }
}
