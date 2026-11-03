// SPDX-License-Identifier: BSD-3-Clause
//! Turning the bytes a `vt(4)` terminal produces in raw mode into keys.
//!
//! With the keyboard in its default `K_XLATE` mode the console sends
//! characters as UTF-8 and special keys as the escape sequences of the
//! terminal it emulates (`teken(4)`): xterm by default (`ESC [ A` for Up,
//! `ESC [ 3 ~` for Delete, `ESC O P` for F1, `ESC [ 15 ~` for F5), or
//! cons25 after `vidcontrol -T cons25` (`ESC [ M` .. `ESC [ X` for
//! F1..F12). A lone `ESC` is the Escape key itself, which can only be told
//! apart from the start of a sequence by timing: the decoder is fed
//! whatever bytes arrived during one poll tick and [`Decoder::flush`] at
//! the end of the tick turns a dangling `ESC` into [`Key::Esc`].

/// A decoded key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// Function key F1..F12.
    F(u8),
    /// Ctrl + letter (`Ctrl-C` is `Ctrl('c')`); raw mode delivers these
    /// instead of signals.
    Ctrl(char),
}

#[derive(Debug, Default, Clone)]
pub struct Decoder {
    pending: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one byte; returns a key once one is complete.
    pub fn feed(&mut self, b: u8) -> Option<Key> {
        self.pending.push(b);
        match decode(&self.pending) {
            Step::Key(k) => {
                self.pending.clear();
                Some(k)
            }
            Step::Incomplete => None,
            Step::Invalid => {
                self.pending.clear();
                None
            }
        }
    }

    /// End of a burst of input: a dangling lone `ESC` is the Escape key;
    /// any other partial sequence is dropped.
    pub fn flush(&mut self) -> Option<Key> {
        let key = (self.pending == [0x1b]).then_some(Key::Esc);
        self.pending.clear();
        key
    }

    /// Decode a whole burst (feed every byte, then flush).
    pub fn decode_all(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut keys: Vec<Key> = bytes.iter().filter_map(|&b| self.feed(b)).collect();
        keys.extend(self.flush());
        keys
    }
}

enum Step {
    Key(Key),
    Incomplete,
    Invalid,
}

fn decode(buf: &[u8]) -> Step {
    match buf {
        [] | [0x1b] | [0x1b, b'[' | b'O'] => Step::Incomplete,
        [0x1b, b'[', params @ .., last] => csi(params, *last),
        [0x1b, b'O', last] => ss3(*last),
        // ESC followed by something else: Alt+key, treated as Esc then
        // dropped (the installer has no Alt bindings).
        [0x1b, _] => Step::Key(Key::Esc),
        [b] => single(*b),
        [first, ..] => utf8(buf, *first),
    }
}

fn single(b: u8) -> Step {
    match b {
        b'\r' | b'\n' => Step::Key(Key::Enter),
        b'\t' => Step::Key(Key::Tab),
        0x7f | 0x08 => Step::Key(Key::Backspace),
        0x01..=0x1a => Step::Key(Key::Ctrl(char::from(b - 1 + b'a'))),
        0x20..=0x7e => Step::Key(Key::Char(char::from(b))),
        0xc0..=0xf7 => Step::Incomplete,
        _ => Step::Invalid,
    }
}

/// A multi-byte character in progress. `first` is a lead byte
/// (`0xc0..=0xf7`): any other byte was settled by [`single`] already.
fn utf8(buf: &[u8], first: u8) -> Step {
    let need = if first >= 0xf0 {
        4
    } else if first >= 0xe0 {
        3
    } else {
        2
    };
    if buf.len() < need {
        return Step::Incomplete;
    }
    match std::str::from_utf8(buf).ok().and_then(|s| s.chars().next()) {
        Some(c) => Step::Key(Key::Char(c)),
        None => Step::Invalid,
    }
}

/// The cursor keys, the same final byte after `ESC [` and after `ESC O`
/// (application mode).
fn cursor(last: u8) -> Option<Key> {
    Some(match last {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        _ => return None,
    })
}

/// After `ESC O` (SS3): xterm's F1..F4, which is what `vt(4)` sends by
/// default, or a cursor key in application mode.
fn ss3(last: u8) -> Step {
    match last {
        b'P'..=b'S' => Step::Key(Key::F(last - b'P' + 1)),
        _ => cursor(last).map_or(Step::Invalid, Step::Key),
    }
}

/// After `ESC [` (CSI): parameters then the byte seen last.
fn csi(params: &[u8], last: u8) -> Step {
    if last.is_ascii_digit() || last == b';' {
        return if params.len() >= 8 {
            Step::Invalid
        } else {
            Step::Incomplete
        };
    }
    let key = match (params, last) {
        ([], b'Z') => Key::BackTab,
        // cons25 F1..F12: ESC [ M .. ESC [ X.
        ([], b'M'..=b'X') => Key::F(last - b'M' + 1),
        ([], _) => match cursor(last) {
            Some(key) => key,
            None => return Step::Invalid,
        },
        (p, b'~') => match p {
            b"1" | b"7" => Key::Home,
            b"2" => return Step::Invalid, // Insert: unused
            b"3" => Key::Delete,
            b"4" | b"8" => Key::End,
            b"5" => Key::PageUp,
            b"6" => Key::PageDown,
            b"15" => Key::F(5),
            b"17" => Key::F(6),
            b"18" => Key::F(7),
            b"19" => Key::F(8),
            b"20" => Key::F(9),
            b"21" => Key::F(10),
            b"23" => Key::F(11),
            b"24" => Key::F(12),
            _ => return Step::Invalid,
        },
        _ => return Step::Invalid,
    };
    Step::Key(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Key> {
        Decoder::new().decode_all(bytes)
    }

    #[test]
    fn plain_characters_and_controls() {
        assert_eq!(
            keys(b"aZ 9\r\t\x7f\x08\x03"),
            vec![
                Key::Char('a'),
                Key::Char('Z'),
                Key::Char(' '),
                Key::Char('9'),
                Key::Enter,
                Key::Tab,
                Key::Backspace,
                Key::Backspace,
                Key::Ctrl('c'),
            ]
        );
        assert_eq!(keys(b"\n"), vec![Key::Enter]);
    }

    #[test]
    fn utf8_is_assembled() {
        assert_eq!(
            keys("é€😀".as_bytes()),
            vec![Key::Char('é'), Key::Char('€'), Key::Char('😀')]
        );
        // Truncated sequence at the end of a burst is dropped.
        assert_eq!(keys(&[0xc3]), vec![]);
        // Invalid continuation.
        assert_eq!(keys(&[0xc3, 0x28, b'x']), vec![Key::Char('x')]);
        assert_eq!(keys(&[0x80, b'y']), vec![Key::Char('y')]);
    }

    #[test]
    fn arrows_and_navigation() {
        assert_eq!(
            keys(b"\x1b[A\x1b[B\x1b[C\x1b[D\x1b[H\x1b[F\x1b[Z"),
            vec![
                Key::Up,
                Key::Down,
                Key::Right,
                Key::Left,
                Key::Home,
                Key::End,
                Key::BackTab
            ]
        );
        assert_eq!(keys(b"\x1bOA"), vec![Key::Up]);
        assert_eq!(
            keys(b"\x1b[3~\x1b[5~\x1b[6~\x1b[1~\x1b[4~\x1b[7~\x1b[8~"),
            vec![
                Key::Delete,
                Key::PageUp,
                Key::PageDown,
                Key::Home,
                Key::End,
                Key::Home,
                Key::End
            ]
        );
    }

    #[test]
    fn function_keys() {
        // xterm, vt(4)'s default: F1..F4 are SS3 sequences.
        assert_eq!(
            keys(b"\x1bOP\x1bOQ\x1bOR\x1bOS"),
            (1..=4).map(Key::F).collect::<Vec<_>>()
        );
        assert_eq!(
            keys(b"\x1b[15~\x1b[17~\x1b[18~\x1b[19~\x1b[20~\x1b[21~\x1b[23~\x1b[24~"),
            (5..=12).map(Key::F).collect::<Vec<_>>()
        );
        // cons25 (`vidcontrol -T cons25`): ESC [ M .. ESC [ X.
        assert_eq!(
            keys(b"\x1b[M\x1b[N\x1b[O\x1b[P\x1b[Q\x1b[R\x1b[S\x1b[T\x1b[U\x1b[V\x1b[W\x1b[X"),
            (1..=12).map(Key::F).collect::<Vec<_>>()
        );
        // Application-mode cursor keys share the SS3 introducer.
        assert_eq!(
            keys(b"\x1bOA\x1bOB\x1bOC\x1bOD\x1bOH\x1bOF"),
            vec![
                Key::Up,
                Key::Down,
                Key::Right,
                Key::Left,
                Key::Home,
                Key::End
            ]
        );
        // Unknown finals after either introducer are dropped.
        assert_eq!(keys(b"\x1bOz\x1b[zq"), vec![Key::Char('q')]);
    }

    #[test]
    fn lone_escape_needs_a_flush() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x1b), None);
        assert_eq!(d.flush(), Some(Key::Esc));
        assert_eq!(d.flush(), None);
        // A partial sequence is dropped at a flush.
        assert_eq!(d.feed(0x1b), None);
        assert_eq!(d.feed(b'['), None);
        assert_eq!(d.flush(), None);
        // Alt+x reads as Esc.
        assert_eq!(keys(b"\x1bx"), vec![Key::Esc]);
    }

    #[test]
    fn unknown_sequences_are_ignored() {
        assert_eq!(keys(b"\x1b[2~a"), vec![Key::Char('a')]);
        assert_eq!(keys(b"\x1b[99~b"), vec![Key::Char('b')]);
        assert_eq!(keys(b"\x1b[1;5Xc"), vec![Key::Char('c')]);
        assert_eq!(keys(b"\x1b[123456789d"), vec![Key::Char('d')]);
        assert_eq!(keys(&[0x1c, b'e']), vec![Key::Char('e')]);
        // Eight parameter bytes still wait for a final byte; the ninth
        // gives up.
        let mut d = Decoder::new();
        for &b in b"\x1b[12345678" {
            assert_eq!(d.feed(b), None);
        }
        assert_eq!(d.pending.len(), 10, "still collecting");
        assert_eq!(d.feed(b'9'), None);
        assert!(d.pending.is_empty(), "dropped");
    }
}
