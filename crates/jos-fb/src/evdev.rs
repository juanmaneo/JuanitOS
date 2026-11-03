// SPDX-License-Identifier: BSD-3-Clause
//! Key presses from FreeBSD's `evdev(4)` nodes, `/dev/input/event*`.
//!
//! The console delivers keys to the active terminal only. The boot splash
//! hides behind the text console on Esc and must see the next Esc to come
//! back, so it also reads the keyboards themselves: with the default
//! `kern.evdev.rcpt_mask` every keyboard driver copies its events to an
//! evdev node (GENERIC has `device evdev`), and any number of readers can
//! open one without taking anything away from the console.
//!
//! A read returns whole `struct input_event`s (`sys/dev/evdev/input.h`): a
//! 16-byte `timeval`, then `u16 type`, `u16 code`, `i32 value`, native
//! endian; 24 bytes on amd64.

/// Bytes per event.
pub const EVENT: usize = 24;
const EV_KEY: u16 = 1;
/// `KEY_ESC` from `input-event-codes.h`.
pub const KEY_ESC: u16 = 1;
/// `value` of a press (0 is a release, 2 a repeat).
const PRESSED: i32 = 1;

/// Key codes pressed (neither released nor repeated) in `bytes`, in order.
pub fn pressed(bytes: &[u8]) -> Vec<u16> {
    let (events, _partial) = bytes.as_chunks::<EVENT>();
    events
        .iter()
        .filter_map(|&[.., t0, t1, c0, c1, v0, v1, v2, v3]| {
            let kind = u16::from_ne_bytes([t0, t1]);
            let value = i32::from_ne_bytes([v0, v1, v2, v3]);
            (kind == EV_KEY && value == PRESSED).then_some(u16::from_ne_bytes([c0, c1]))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: u16, code: u16, value: i32) -> Vec<u8> {
        // The timestamp takes what the three fields leave.
        let mut e = vec![0u8; EVENT - 8];
        e.extend(kind.to_ne_bytes());
        e.extend(code.to_ne_bytes());
        e.extend(value.to_ne_bytes());
        e
    }

    #[test]
    fn only_key_presses_count() {
        let mut bytes = event(EV_KEY, KEY_ESC, PRESSED);
        bytes.extend(event(EV_KEY, KEY_ESC, 2)); // repeat
        bytes.extend(event(EV_KEY, KEY_ESC, 0)); // release
        bytes.extend(event(0, 0, 0)); // EV_SYN
        bytes.extend(event(2, 0, 5)); // EV_REL: mouse motion
        bytes.extend(event(EV_KEY, 28, PRESSED)); // KEY_ENTER
        bytes.extend(event(EV_KEY, KEY_ESC, PRESSED));
        assert_eq!(pressed(&bytes), [KEY_ESC, 28, KEY_ESC]);
    }

    #[test]
    fn partial_events_are_dropped() {
        let mut bytes = event(EV_KEY, KEY_ESC, PRESSED);
        bytes.truncate(EVENT - 1);
        assert!(pressed(&bytes).is_empty());
        assert!(pressed(&[]).is_empty());
    }
}
