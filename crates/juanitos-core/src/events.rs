//! Qube-initiated events: the guest-to-dom0 half of the agent channel.
//!
//! The virtio-console at slot 30 ([`crate::agent`]) carries a second
//! port, `events` (`<run_dir>/<qube>.events` on the host,
//! `/dev/vtcon/events` in a FreeBSD guest, `/dev/virtio-ports/events` in
//! a Linux one, writable by the qube's users), on which a guest writes one
//! line when it has something for dom0: `jos-copy` writes `REQUEST <id>`
//! after dropping a request on the share. `jos serve` holds that port of
//! every running qube open and wakes up on any line instead of waiting
//! for its next poll. The poll stays, every `--interval` seconds, for
//! guests without the port (Redox, clones made before it existed) and
//! for anything a lost line would leave behind: the share is the queue,
//! the port is only the doorbell.
//!
//! Nothing a guest writes is interpreted. A line is a wake-up call,
//! logged for the operator after being cut to printable ASCII; the batch
//! a wake-up returns is bounded and two wake-ups are at least [`SPACING`]
//! apart, so the worst a hostile qube can do is make the broker scan the
//! request directories a few times a second.
//!
//! bhyve only listens: [`Events::connect`] dials the sockets of the qubes
//! it is told about (one reader thread per connection) and bhyve closes
//! the connection when the qube stops, which ends the thread; the next
//! `connect` after a restart dials again.

use std::collections::BTreeSet;
use std::io::Read;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use crate::config::GlobalConfig;

/// Longest line kept from a guest (the rest of it is dropped).
pub const MAX_LINE: usize = 80;
/// Bytes a guest may send without a newline before they count as a line.
pub const MAX_PENDING: usize = 4096;
/// Lines a wake-up returns at most (the rest of the queue is dropped).
pub const MAX_BATCH: usize = 64;
/// Minimum time between two wake-ups.
pub const SPACING: Duration = Duration::from_millis(200);
/// Lines queued between two waits at most; beyond that they are dropped.
const QUEUE: usize = 1024;

/// One line a qube wrote on its events port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub qube: String,
    pub line: String,
}

enum Message {
    Line(Event),
    Closed(String),
}

/// How a port is reached: given the socket path, something to read from.
/// The default dials the UNIX socket; tests substitute their own.
pub type Dialer = Box<dyn Fn(&str) -> std::io::Result<Box<dyn Read + Send>>>;

#[cfg(unix)]
fn dial(path: &str) -> std::io::Result<Box<dyn Read + Send>> {
    Ok(Box::new(std::os::unix::net::UnixStream::connect(path)?))
}

#[cfg(not(unix))]
fn dial(_path: &str) -> std::io::Result<Box<dyn Read + Send>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "UNIX sockets",
    ))
}

/// The events ports of the running qubes, as seen by `jos serve`.
pub struct Events<'a> {
    cfg: &'a GlobalConfig,
    dial: Dialer,
    tx: SyncSender<Message>,
    rx: Receiver<Message>,
    connected: BTreeSet<String>,
    last_wake: Option<Instant>,
}

impl<'a> Events<'a> {
    pub fn new(cfg: &'a GlobalConfig) -> Self {
        Self::with_dialer(cfg, Box::new(dial))
    }

    pub fn with_dialer(cfg: &'a GlobalConfig, dial: Dialer) -> Self {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        Events {
            cfg,
            dial,
            tx,
            rx,
            connected: BTreeSet::new(),
            last_wake: None,
        }
    }

    /// Dial the ports of the `qubes` not connected yet (a qube that is not
    /// running has no socket, or a stale one nobody listens on: both fail
    /// at once and are tried again next time). The names newly connected.
    pub fn connect<'n>(&mut self, qubes: impl IntoIterator<Item = &'n str>) -> Vec<String> {
        let mut fresh = Vec::new();
        for name in qubes {
            if self.connected.contains(name) {
                continue;
            }
            let Ok(stream) = (self.dial)(&self.cfg.events_socket(name)) else {
                continue;
            };
            self.spawn_reader(name, stream);
            self.connected.insert(name.to_string());
            fresh.push(name.to_string());
        }
        fresh
    }

    /// Qubes whose port is held open right now.
    pub fn connected(&self) -> Vec<String> {
        self.connected.iter().cloned().collect()
    }

    fn spawn_reader(&self, name: &str, mut stream: Box<dyn Read + Send>) {
        let tx = self.tx.clone();
        let qube = name.to_string();
        std::thread::spawn(move || {
            let mut pending: Vec<u8> = Vec::new();
            let mut buf = [0u8; 512];
            loop {
                let n = match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                pending.extend_from_slice(buf.get(..n).unwrap_or_default());
                for line in take_lines(&mut pending) {
                    let event = Event {
                        qube: qube.clone(),
                        line,
                    };
                    // A full queue drops the line: the poll covers for it.
                    if let Err(mpsc::TrySendError::Disconnected(_)) =
                        tx.try_send(Message::Line(event))
                    {
                        return;
                    }
                }
            }
            // Must get through, or the qube would never be dialed again.
            let _ = tx.send(Message::Closed(qube));
        });
    }

    /// Block until a qube writes a line or `timeout` passes: the lines
    /// received, in order, empty on timeout. Two wake-ups are at least
    /// [`SPACING`] apart, whatever the guests do.
    pub fn wait(&mut self, timeout: Duration) -> Vec<Event> {
        let deadline = Instant::now() + timeout;
        if let Some(last) = self.last_wake {
            // Zero when the last wake-up is long enough ago.
            let pause = (last + SPACING).saturating_duration_since(Instant::now());
            std::thread::sleep(pause.min(timeout));
        }
        let mut batch = Vec::new();
        loop {
            let message = if batch.is_empty() {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.rx.recv_timeout(left) {
                    Ok(m) => m,
                    Err(_) => return batch,
                }
            } else {
                match self.rx.try_recv() {
                    Ok(m) => m,
                    Err(_) => break,
                }
            };
            match message {
                Message::Line(event) => {
                    if batch.len() < MAX_BATCH {
                        batch.push(event);
                    }
                }
                Message::Closed(qube) => {
                    self.connected.remove(&qube);
                }
            }
        }
        self.last_wake = Some(Instant::now());
        batch
    }
}

/// The complete lines at the front of `pending`, removed from it and
/// sanitized; a guest that never sends a newline gets cut at
/// [`MAX_PENDING`] bytes.
fn take_lines(pending: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(at) = pending.iter().position(|&b| b == b'\n') {
        let raw: Vec<u8> = pending.drain(..=at).collect();
        lines.push(sanitize(&raw));
    }
    if pending.len() >= MAX_PENDING {
        lines.push(sanitize(&std::mem::take(pending)));
    }
    lines
}

/// Printable ASCII only, line ends dropped, at most [`MAX_LINE`] chars:
/// what a guest's line is allowed to look like in dom0's log.
pub fn sanitize(raw: &[u8]) -> String {
    raw.iter()
        .filter(|b| !matches!(b, b'\r' | b'\n'))
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '?'
            }
        })
        .take(MAX_LINE)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn cfg(run_dir: &str) -> GlobalConfig {
        GlobalConfig {
            run_dir: run_dir.to_string(),
            ..GlobalConfig::default()
        }
    }

    #[test]
    fn lines_are_split_and_sanitized() {
        let mut pending = b"REQUEST 1\r\nRE".to_vec();
        assert_eq!(take_lines(&mut pending), vec!["REQUEST 1".to_string()]);
        assert_eq!(pending, b"RE");
        pending.extend_from_slice(b"QUEST 2\n\n\x1b[31mred\x00\n");
        assert_eq!(
            take_lines(&mut pending),
            vec![
                "REQUEST 2".to_string(),
                String::new(),
                "?[31mred?".to_string()
            ]
        );
        assert!(pending.is_empty());

        let long = "x".repeat(200);
        assert_eq!(sanitize(long.as_bytes()).len(), MAX_LINE);
        let mut flood = vec![b'y'; MAX_PENDING];
        assert_eq!(take_lines(&mut flood), vec!["y".repeat(MAX_LINE)]);
        assert!(flood.is_empty());
        let mut short = vec![b'y'; 10];
        assert!(take_lines(&mut short).is_empty());
        assert_eq!(short.len(), 10);
    }

    #[test]
    fn a_dialed_port_delivers_its_lines_then_closes() {
        let cfg = cfg("/run/test");
        let dial: Dialer = Box::new(|path: &str| {
            assert_eq!(path, "/run/test/work.events");
            Ok(Box::new(Cursor::new(b"REQUEST 1\nREQUEST 2\n".to_vec())) as Box<dyn Read + Send>)
        });
        let mut ev = Events::with_dialer(&cfg, dial);
        assert_eq!(ev.connect(["work"]), vec!["work".to_string()]);
        assert_eq!(ev.connected(), vec!["work".to_string()]);
        // Already connected: not dialed again (the dialer would assert on
        // another path anyway).
        assert!(ev.connect(["work"]).is_empty());
        // The reader queues the lines one by one, so a wait may return
        // before the second is queued: collect over as many as it takes.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut lines = Vec::new();
        while lines.len() < 2 && Instant::now() < deadline {
            lines.extend(ev.wait(Duration::from_secs(5)));
        }
        let work = |line: &str| Event {
            qube: "work".into(),
            line: line.into(),
        };
        assert_eq!(lines, vec![work("REQUEST 1"), work("REQUEST 2")]);
        // The cursor is at EOF: the reader reports the port closed right
        // after the lines, and a wait takes the report in (then times out,
        // nothing to deliver).
        assert!(ev.wait(Duration::from_secs(2)).is_empty());
        assert!(ev.connected().is_empty());
        // ... and it gets dialed again.
        assert_eq!(ev.connect(["work"]), vec!["work".to_string()]);
    }

    #[test]
    fn qubes_without_a_port_are_skipped() {
        let cfg = cfg("/run/test");
        let dial: Dialer = Box::new(|_: &str| Err(std::io::Error::other("no such socket")));
        let mut ev = Events::with_dialer(&cfg, dial);
        assert!(ev.connect(["rx", "work"]).is_empty());
        assert!(ev.connected().is_empty());
        let t = Instant::now();
        assert!(ev.wait(Duration::from_millis(30)).is_empty());
        assert!(t.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn wake_ups_are_bounded_and_spaced() {
        let cfg = cfg("/run/test");
        let dial: Dialer = Box::new(|_: &str| {
            let flood = "spam\n".repeat(MAX_BATCH * 3);
            Ok(Box::new(Cursor::new(flood.into_bytes())) as Box<dyn Read + Send>)
        });
        let mut ev = Events::with_dialer(&cfg, dial);
        ev.connect(["web"]);
        let first = ev.wait(Duration::from_secs(5)).len();
        assert!(first > 0 && first <= MAX_BATCH, "{first}");
        let t = Instant::now();
        let _ = ev.wait(Duration::from_secs(5));
        let elapsed = t.elapsed();
        assert!(elapsed >= SPACING, "{elapsed:?}");
    }

    #[cfg(unix)]
    #[test]
    fn dials_a_unix_socket() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("jos-events-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = cfg(dir.to_str().unwrap());
        let listener = std::os::unix::net::UnixListener::bind(dir.join("work.events")).unwrap();
        let mut ev = Events::new(&cfg);
        assert_eq!(ev.connect(["work", "absent"]), vec!["work".to_string()]);
        let (mut guest, _) = listener.accept().unwrap();
        guest.write_all(b"REQUEST 42\n").unwrap();
        let batch = ev.wait(Duration::from_secs(5));
        assert_eq!(
            batch,
            vec![Event {
                qube: "work".into(),
                line: "REQUEST 42".into()
            }]
        );
        drop(guest);
        drop(listener);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
