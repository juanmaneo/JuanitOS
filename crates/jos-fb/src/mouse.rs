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

#[cfg(test)]
mod tests {
    use super::*;

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
