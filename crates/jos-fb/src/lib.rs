//! Full-screen graphics on FreeBSD's `vt(4)` console, shared by the boot
//! splash (`jos-splash`) and the graphical installer (`jos-install`).
//!
//! The [`Display`] trait, pixel packing, ioctl numbers and the keyboard
//! decoder ([`keys`]) are pure and unit-tested on any host; the system
//! calls live in [`vt`] (FreeBSD only).

pub mod keys;
#[cfg(target_os = "freebsd")]
pub mod vt;

use jos_gfx::{Image, Rgba};

/// Something a full-screen program can be presented on.
pub trait Display {
    fn size(&self) -> (u32, u32);
    fn present(&mut self, frame: &Image) -> anyhow::Result<()>;
    /// Restore the text console (end of the splash, console mode).
    fn restore_console(&mut self) -> anyhow::Result<()>;
    /// Next byte typed on the terminal, if any (Esc → `Some(0x1b)`;
    /// arrows etc. arrive as escape sequences, see [`keys::Decoder`]).
    fn poll_key(&mut self) -> Option<u8>;
}

/// Bit offsets of the colour channels inside a 32-bit pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbOffsets {
    pub red: u32,
    pub green: u32,
    pub blue: u32,
}

impl RgbOffsets {
    /// What `efifb` almost always is (and what `FBIO_GETRGBOFFS` reports
    /// for it): B G R X in memory on a little-endian machine.
    pub const BGRX: RgbOffsets = RgbOffsets {
        red: 16,
        green: 8,
        blue: 0,
    };
}

/// Pack one row of RGBA pixels into native-endian 32-bit words with the
/// channel layout `offs`. Writes `min(row.len(), out.len()/4)` pixels.
pub fn pack_row(row: &[Rgba], offs: RgbOffsets, out: &mut [u8]) {
    let (chunks, _) = out.as_chunks_mut::<4>();
    for (px, chunk) in row.iter().zip(chunks) {
        let v = ((px.r as u32) << offs.red)
            | ((px.g as u32) << offs.green)
            | ((px.b as u32) << offs.blue);
        *chunk = v.to_ne_bytes();
    }
}

/// FreeBSD `ioccom.h` encoding.
pub mod ioc {
    pub const IOC_VOID: u32 = 0x2000_0000;
    pub const IOC_OUT: u32 = 0x4000_0000;
    const IOCPARM_MASK: u32 = (1 << 13) - 1;

    pub const fn ioc(inout: u32, group: u8, num: u8, len: u32) -> u32 {
        inout | ((len & IOCPARM_MASK) << 16) | ((group as u32) << 8) | num as u32
    }

    /// `struct fbtype { int fb_type, fb_height, fb_width, fb_depth, fb_cmsize, fb_size; }`
    pub const FBTYPE_LEN: u32 = 6 * 4;
    /// `struct fb_rgboffs { int red, green, blue; }`
    pub const RGBOFFS_LEN: u32 = 3 * 4;

    // sys/fbio.h
    pub const FBIOGTYPE: u32 = ioc(IOC_OUT, b'F', 0, FBTYPE_LEN);
    pub const FBIO_GETLINEWIDTH: u32 = ioc(IOC_OUT, b'F', 111, 4);
    pub const FBIO_GETRGBOFFS: u32 = ioc(IOC_OUT, b'F', 116, RGBOFFS_LEN);
    // sys/consio.h
    pub const KDSETMODE: u32 = ioc(IOC_VOID, b'K', 10, 4);
    pub const KD_TEXT: i32 = 0;
    pub const KD_GRAPHICS: i32 = 1;
    pub const VT_ACTIVATE: u32 = ioc(IOC_VOID, b'v', 5, 4);
    pub const VT_WAITACTIVE: u32 = ioc(IOC_VOID, b'v', 6, 4);
    pub const VT_GETACTIVE: u32 = ioc(IOC_OUT, b'v', 7, 4);
}

/// A display that only remembers what it was given: `--dry-run` on a
/// development machine, and tests.
#[derive(Debug, Default)]
pub struct NullDisplay {
    pub width: u32,
    pub height: u32,
    pub presented: usize,
    pub restored: bool,
    /// Bytes `poll_key` returns, in order.
    pub keys: std::collections::VecDeque<u8>,
    /// The last frame presented, when [`NullDisplay::keep_frames`] is set.
    pub last_frame: Option<Image>,
    pub keep_frames: bool,
}

impl NullDisplay {
    pub fn new(width: u32, height: u32) -> Self {
        NullDisplay {
            width,
            height,
            ..Default::default()
        }
    }

    /// Queue `bytes` as if typed.
    pub fn type_bytes(&mut self, bytes: &[u8]) {
        self.keys.extend(bytes);
    }
}

impl Display for NullDisplay {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn present(&mut self, frame: &Image) -> anyhow::Result<()> {
        anyhow::ensure!(
            frame.width() == self.width && frame.height() == self.height,
            "frame size mismatch"
        );
        self.presented += 1;
        if self.keep_frames {
            self.last_frame = Some(frame.clone());
        }
        Ok(())
    }

    fn restore_console(&mut self) -> anyhow::Result<()> {
        self.restored = true;
        Ok(())
    }

    fn poll_key(&mut self) -> Option<u8> {
        self.keys.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_freebsd_headers() {
        // Values as the C headers evaluate them on amd64.
        assert_eq!(ioc::FBIOGTYPE, 0x4018_4600);
        assert_eq!(ioc::FBIO_GETLINEWIDTH, 0x4004_466f);
        assert_eq!(ioc::FBIO_GETRGBOFFS, 0x400c_4674);
        assert_eq!(ioc::KDSETMODE, 0x2004_4b0a);
        assert_eq!(ioc::VT_ACTIVATE, 0x2004_7605);
        assert_eq!(ioc::VT_WAITACTIVE, 0x2004_7606);
        assert_eq!(ioc::VT_GETACTIVE, 0x4004_7607);
    }

    #[test]
    fn packs_bgrx_and_rgbx() {
        let row = [Rgba::rgb(0x11, 0x22, 0x33), Rgba::rgb(0xaa, 0xbb, 0xcc)];
        let mut out = [0u8; 8];
        pack_row(&row, RgbOffsets::BGRX, &mut out);
        assert_eq!(out, [0x33, 0x22, 0x11, 0, 0xcc, 0xbb, 0xaa, 0]);
        let rgbx = RgbOffsets {
            red: 0,
            green: 8,
            blue: 16,
        };
        pack_row(&row, rgbx, &mut out);
        assert_eq!(out, [0x11, 0x22, 0x33, 0, 0xaa, 0xbb, 0xcc, 0]);
        // Short output buffer: only what fits.
        let mut short = [0u8; 4];
        pack_row(&row, RgbOffsets::BGRX, &mut short);
        assert_eq!(short, [0x33, 0x22, 0x11, 0]);
    }

    #[test]
    fn null_display_checks_sizes() {
        let mut d = NullDisplay::new(4, 4);
        assert!(d.present(&Image::new(4, 4, Rgba::TRANSPARENT)).is_ok());
        assert!(d.present(&Image::new(5, 4, Rgba::TRANSPARENT)).is_err());
        assert_eq!(d.presented, 1);
        assert!(d.last_frame.is_none());
        d.keep_frames = true;
        d.present(&Image::new(4, 4, Rgba::TRANSPARENT)).unwrap();
        assert!(d.last_frame.is_some());
        assert_eq!(d.size(), (4, 4));
        d.type_bytes(b"ab");
        assert_eq!(d.poll_key(), Some(b'a'));
        assert_eq!(d.poll_key(), Some(b'b'));
        assert_eq!(d.poll_key(), None);
        d.restore_console().unwrap();
        assert!(d.restored);
    }
}
