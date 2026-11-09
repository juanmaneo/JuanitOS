// SPDX-License-Identifier: BSD-3-Clause
//! Mouse packets from FreeBSD's `/dev/sysmouse`.
//!
//! `moused` feeds every mouse and touchpad into `sysmouse(4)`. Opened at
//! the default level 0 the device speaks the 5-byte MouseSystems protocol,
//! and `read` only ever returns whole packets (`vt_sysmouse.c`), so a read
//! buffer decodes on its own:
//!
//! ```text
//! byte 0   1000 0LMR   sync bits, then left / middle / right *released*
//! byte 1,3 dx          signed, summed (right is positive)
//! byte 2,4 dy          signed, summed (up is positive)
//! ```
//!
//! [`Pointer`] turns the packets into a position on the screen and draws
//! the arrow over a frame, for the installer and the login screen.

use jos_gfx::{icons, Image};

/// Motion since the previous packet, in screen directions (y grows
/// downwards), and whether the left button is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mouse {
    pub dx: i32,
    pub dy: i32,
    pub left: bool,
}

/// Bytes per level-0 packet.
pub const PACKET: usize = 5;
const SYNC_MASK: u8 = 0xf8;
const SYNC: u8 = 0x80;
const LEFT_UP: u8 = 0x04;

/// Decode whole packets; one that does not start with the sync bits is
/// skipped.
pub fn decode(bytes: &[u8]) -> Vec<Mouse> {
    let (packets, _partial) = bytes.as_chunks::<PACKET>();
    packets
        .iter()
        .filter(|[b0, ..]| b0 & SYNC_MASK == SYNC)
        .map(|&[b0, x1, y1, x2, y2]| Mouse {
            dx: i32::from(x1 as i8) + i32::from(x2 as i8),
            dy: -(i32::from(y1 as i8) + i32::from(y2 as i8)),
            left: b0 & LEFT_UP == 0,
        })
        .collect()
}

/// Pointer height, as a fraction of the screen height.
const POINTER_SIZE: f32 = 1.0 / 40.0;

/// The mouse pointer: hidden until the mouse first moves, then clamped to
/// the screen. Clicks are the press edge of the left button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pointer {
    pub shown: bool,
    pub x: i32,
    pub y: i32,
    left: bool,
}

impl Pointer {
    /// Starts hidden at the centre of a `width` x `height` screen.
    pub fn new(width: u32, height: u32) -> Self {
        Pointer {
            shown: false,
            x: width as i32 / 2,
            y: height as i32 / 2,
            left: false,
        }
    }

    /// Apply a packet; `true` when the left button was just pressed.
    pub fn update(&mut self, m: Mouse, width: u32, height: u32) -> bool {
        self.shown = true;
        self.x = (self.x + m.dx).clamp(0, width as i32 - 1);
        self.y = (self.y + m.dy).clamp(0, height as i32 - 1);
        let pressed = m.left && !self.left;
        self.left = m.left;
        pressed
    }

    /// `frame` with the pointer on it (the frame itself when hidden).
    pub fn composite(&self, frame: &Image) -> Image {
        let mut img = frame.clone();
        if self.shown {
            let scale = frame.height() as f32 * POINTER_SIZE / 20.0;
            icons::draw_pointer(&mut img, self.x, self.y, scale.max(1.0));
        }
        img
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jos_gfx::Rgba;

    fn mouse(dx: i32, dy: i32, left: bool) -> Mouse {
        Mouse { dx, dy, left }
    }

    #[test]
    fn the_pointer_is_clamped_and_clicks_on_the_press_edge() {
        let mut p = Pointer::new(100, 50);
        assert_eq!((p.shown, p.x, p.y), (false, 50, 25));
        let frame = Image::new(100, 50, Rgba::rgb(9, 9, 9));
        assert_eq!(p.composite(&frame).pixels(), frame.pixels());
        assert!(!p.update(mouse(-500, 10, false), 100, 50));
        assert_eq!((p.shown, p.x, p.y), (true, 0, 35));
        assert!(p.update(mouse(500, 500, true), 100, 50));
        assert_eq!((p.x, p.y), (99, 49));
        assert!(!p.update(mouse(-20, -20, true), 100, 50), "still held");
        assert!(!p.update(mouse(0, 0, false), 100, 50));
        assert!(p.update(mouse(0, 0, true), 100, 50));
        let shown = p.composite(&frame);
        assert_ne!(shown.pixels(), frame.pixels());
        assert_ne!(shown.get(80, 30), frame.get(80, 30));
    }

    #[test]
    fn packets_decode_to_screen_motion_and_buttons() {
        // Nothing pressed (all "up" bits set), 3+2 right, 1+1 up.
        let idle_move = [0x87, 3, 1, 2, 1];
        // Left pressed, 1 left and 4 down.
        let press = [0x83, 0xff, 0xfe, 0, 0xfe];
        let mut bytes = idle_move.to_vec();
        bytes.extend(press);
        assert_eq!(
            decode(&bytes),
            [
                Mouse {
                    dx: 5,
                    dy: -2,
                    left: false
                },
                Mouse {
                    dx: -1,
                    dy: 4,
                    left: true
                },
            ]
        );
    }

    #[test]
    fn out_of_sync_and_partial_packets_are_skipped() {
        let mut bytes = vec![0x00, 1, 1, 1, 1];
        bytes.extend([0x86, 0, 0, 0, 0]);
        bytes.extend([0x87, 1]);
        assert_eq!(decode(&bytes), [Mouse::default()]);
        assert!(decode(&[]).is_empty());
    }
}
