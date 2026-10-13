//! Turning the bytes a `vt(4)` terminal produces in raw mode into keys.
//!
//! With the keyboard in its default `K_XLATE` mode the console sends
//! characters as UTF-8 and special keys as xterm-style escape sequences
//! (`ESC [ A` for Up, `ESC [ 3 ~` for Delete, ...). A lone `ESC` is the
//! Escape key itself, which can only be told apart from the start of a
//! sequence by timing: the decoder is fed whatever bytes arrived during one
//! poll tick and [`Decoder::flush`] at the end of the tick turns a dangling
//! `ESC` into [`Key::Esc`].

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
        [] => Step::Incomplete,
        [0x1b] => Step::Incomplete,
        [0x1b, b'[' | b'O'] => Step::Incomplete,
        [0x1b, b'[' | b'O', rest @ ..] => csi(rest),
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

fn utf8(buf: &[u8], first: u8) -> Step {
    let need = match first {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Step::Invalid,
    };
    if buf.len() < need {
        return Step::Incomplete;
    }
    match std::str::from_utf8(buf).ok().and_then(|s| s.chars().next()) {
        Some(c) => Step::Key(Key::Char(c)),
        None => Step::Invalid,
    }
}

/// After `ESC [` (or `ESC O`): parameters then a final byte.
fn csi(rest: &[u8]) -> Step {
    let Some((&last, params)) = rest.split_last() else {
        return Step::Incomplete;
    };
    if last.is_ascii_digit() || last == b';' {
        return if rest.len() > 8 {
            Step::Invalid
        } else {
            Step::Incomplete
        };
    }
    let key = match (params, last) {
        ([], b'A') => Key::Up,
        ([], b'B') => Key::Down,
        ([], b'C') => Key::Right,
        ([], b'D') => Key::Left,
        ([], b'H') => Key::Home,
        ([], b'F') => Key::End,
        ([], b'Z') => Key::BackTab,
        // vt(4) F1..F4: ESC [ M .. ESC [ P; xterm: ESC O P .. ESC O S
        ([], b'M') => Key::F(1),
        ([], b'N') => Key::F(2),
        ([], b'O') => Key::F(3),
        ([], b'P') => Key::F(4),
        ([], b'Q') => Key::F(2),
        ([], b'R') => Key::F(3),
        ([], b'S') => Key::F(4),
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
        assert_eq!(
            keys(b"\x1b[M\x1b[N\x1b[O\x1b[P\x1bOQ\x1bOR\x1bOS"),
            vec![
                Key::F(1),
                Key::F(2),
                Key::F(3),
                Key::F(4),
                Key::F(2),
                Key::F(3),
                Key::F(4)
            ]
        );
        assert_eq!(
            keys(b"\x1b[15~\x1b[17~\x1b[18~\x1b[19~\x1b[20~\x1b[21~\x1b[23~\x1b[24~"),
            (5..=12).map(Key::F).collect::<Vec<_>>()
        );
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
    }
}
