//! The agent channel: a virtio-console port between dom0 and each FreeBSD
//! or Linux qube, served inside the guest by `jos-agentd`
//! (`templates/agent/jos-agentd`). `jos run` uses it instead of typing
//! into the serial console: no login, no echo, no prompt detection, no
//! fight with `jos console`, and a command can read its stdin
//! (`/dev/null`) without eating the end marker.
//!
//! bhyve side: `-s 30,virtio-console,jos=<run_dir>/<qube>.agent` makes
//! bhyve listen on that UNIX socket; dom0 connects to talk to the guest,
//! one client at a time. bhyve does not remove a stale socket before it
//! binds, so [`Agent::prepare`] does, before every boot. The guest sees
//! the port as `/dev/ttyV0.0` (FreeBSD) or `/dev/virtio-ports/jos`
//! (Linux, from the port name).
//!
//! Protocol (lines of text, one request at a time, see the daemon):
//!
//! ```text
//! dom0  -> guest   PING <id>
//! guest -> dom0    PONG <id>
//! dom0  -> guest   RUN <id> <user> <command...>
//! guest -> dom0    <output>
//!                  END <id> <status>
//! ```
//!
//! The ping lets `jos run` tell a qube without the daemon (a clone made
//! before it existed, or booted elsewhere) from a slow one, and fall back
//! to the console. The framing and parsing are pure and tested here; the
//! socket I/O is a few reads in the CLI, generic over the stream.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crate::config::{GlobalConfig, Qube};
use crate::console::Outcome;
use crate::sys::Runner;
use crate::Result;

/// Guest PCI slot of the virtio-console device.
pub const SLOT: u32 = 30;
/// Name of the port, which Linux turns into `/dev/virtio-ports/<name>`.
pub const PORT_NAME: &str = "jos";
/// User a command runs as unless `jos run --user` says otherwise: the
/// unprivileged user of the templates (with `doas`/`sudo` to root).
pub const DEFAULT_USER: &str = "user";
/// How long a qube gets to answer the ping before `jos run` concludes
/// there is no daemon (a Linux daemon may sleep a second between polls).
pub const PING_TIMEOUT: Duration = Duration::from_secs(3);

/// bhyve `-s` argument for the qube's agent channel.
pub fn device_arg(cfg: &GlobalConfig, q: &Qube) -> String {
    format!(
        "{SLOT},virtio-console,{PORT_NAME}={}",
        cfg.agent_socket(&q.name)
    )
}

/// A request id nobody else is using: pid and nanoseconds.
pub fn new_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{nanos:x}", std::process::id())
}

/// Reject what would break the line protocol (the user is one word, the
/// command one line).
pub fn validate_user(user: &str) -> Result<()> {
    if user.is_empty()
        || !user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(crate::Error::Other(format!(
            "invalid user name {user:?} (letters, digits, `_`, `-` and `.`)"
        )));
    }
    Ok(())
}

pub fn validate_command(command: &str) -> Result<()> {
    if command.contains('\n') || command.contains('\r') {
        return Err(crate::Error::Other(
            "the command must be a single line (join it with `;`)".into(),
        ));
    }
    Ok(())
}

pub fn ping(id: &str) -> String {
    format!("PING {id}\n")
}

pub fn request(id: &str, user: &str, command: &str) -> String {
    format!("RUN {id} {user} {command}\n")
}

/// True when `text` holds the pong for `id` (a complete line).
pub fn ponged(id: &str, text: &str) -> bool {
    let pong = format!("PONG {id}");
    text.split_inclusive('\n')
        .filter(|l| l.ends_with('\n'))
        .any(|l| l.trim_end() == pong)
}

/// Output before the end line for `id` and the status on it; `None`
/// until the end line is complete. The newline the daemon prints before
/// `END` (so the marker starts a line whatever the output ended with)
/// is not part of the output.
pub fn parse(id: &str, text: &str) -> Option<Outcome> {
    let marker = format!("END {id} ");
    // At the very start (nothing was printed, not even the daemon's
    // newline yet) or at the start of a line.
    let (body_end, marker_at) = if text.starts_with(&marker) {
        (0, 0)
    } else {
        let at = text.find(&format!("\n{marker}"))?;
        (at, at + 1)
    };
    let after = text.get(marker_at + marker.len()..)?;
    let line_end = after.find('\n')?;
    let status = after.get(..line_end)?.trim().parse().ok();
    Some(Outcome {
        output: text.get(..body_end)?.to_string(),
        status,
    })
}

/// Send the ping and wait for the pong. `poll` reads whatever is
/// available without blocking (empty when nothing is).
pub fn handshake<D: Read + Write>(
    dev: &mut D,
    id: &str,
    timeout: Duration,
    poll: &impl Fn(&mut D) -> std::io::Result<Vec<u8>>,
) -> std::io::Result<bool> {
    dev.write_all(ping(id).as_bytes())?;
    dev.flush()?;
    let mut buf = Vec::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let chunk = poll(dev)?;
        if chunk.is_empty() {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        buf.extend_from_slice(&chunk);
        if ponged(id, &String::from_utf8_lossy(&buf)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Send the command and collect its output until the end line or the
/// deadline.
pub fn run<D: Read + Write>(
    dev: &mut D,
    id: &str,
    user: &str,
    command: &str,
    timeout: Duration,
    poll: &impl Fn(&mut D) -> std::io::Result<Vec<u8>>,
) -> std::io::Result<Outcome> {
    dev.write_all(request(id, user, command).as_bytes())?;
    dev.flush()?;
    let mut buf = Vec::new();
    let deadline = Instant::now() + timeout;
    loop {
        let chunk = poll(dev)?;
        if chunk.is_empty() {
            if Instant::now() > deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("no answer from the agent within {}s", timeout.as_secs()),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        buf.extend_from_slice(&chunk);
        if let Some(outcome) = parse(id, &String::from_utf8_lossy(&buf)) {
            return Ok(outcome);
        }
    }
}

/// dom0-side housekeeping for the channel.
pub struct Agent<'a> {
    runner: &'a dyn Runner,
    cfg: &'a GlobalConfig,
}

impl<'a> Agent<'a> {
    pub fn new(runner: &'a dyn Runner, cfg: &'a GlobalConfig) -> Self {
        Agent { runner, cfg }
    }

    /// Remove the socket a previous boot left behind: bhyve's `bindat`
    /// fails on an existing path.
    pub fn prepare(&self, q: &Qube) -> Result<()> {
        if !q.os.has_agent() {
            return Ok(());
        }
        self.runner
            .run_checked("mkdir", &["-p", &self.cfg.run_dir])?;
        self.runner
            .run_checked("rm", &["-f", &self.cfg.agent_socket(&q.name)])?;
        Ok(())
    }

    /// Whether the qube's channel exists on the host (the VM is up with a
    /// virtio-console device).
    pub fn available(&self, q: &Qube) -> bool {
        q.os.has_agent() && std::path::Path::new(&self.cfg.agent_socket(&q.name)).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OsType, QubeKind};
    use crate::sys::MockRunner;

    #[test]
    fn device_and_socket() {
        let cfg = GlobalConfig::default();
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        assert_eq!(
            device_arg(&cfg, &q),
            "30,virtio-console,jos=/var/run/juanitos/work.agent"
        );
    }

    #[test]
    fn framing() {
        assert_eq!(ping("7"), "PING 7\n");
        assert_eq!(request("7", "user", "ls -l /"), "RUN 7 user ls -l /\n");
        assert!(ponged("7", "junk\nPONG 7\n"));
        assert!(!ponged("7", "PONG 77\n"));
        assert!(!ponged("7", "PONG 7"));
        let id = new_id();
        assert!(id.contains('-'));
        assert_ne!(id, new_id());
    }

    #[test]
    fn validation() {
        assert!(validate_user("user").is_ok());
        assert!(validate_user("root").is_ok());
        assert!(validate_user("_svc-1.a").is_ok());
        assert!(validate_user("").is_err());
        assert!(validate_user("a b").is_err());
        assert!(validate_command("echo a; echo b").is_ok());
        assert!(validate_command("echo a\necho b").is_err());
    }

    #[test]
    fn parse_waits_for_the_whole_end_line() {
        assert!(parse("1", "hello\n").is_none());
        assert!(parse("1", "hello\n\nEND 1 ").is_none());
        assert!(parse("1", "hello\n\nEND 1 0").is_none());
        let o = parse("1", "hello\n\nEND 1 0\n").unwrap();
        assert_eq!(o.output, "hello\n");
        assert_eq!(o.status, Some(0));
        // Output without a final newline keeps its last line intact.
        let o = parse("1", "partial\nEND 1 3\n").unwrap();
        assert_eq!(o.output, "partial");
        assert_eq!(o.status, Some(3));
        // Nothing printed at all.
        let o = parse("1", "\nEND 1 0\n").unwrap();
        assert_eq!(o.output, "");
        let o = parse("1", "END 1 0\n").unwrap();
        assert_eq!(o.output, "");
        // Another request's end line is not ours; a bad status is None.
        assert!(parse("1", "\nEND 2 0\n").is_none());
        let o = parse("1", "\nEND 1 x\n").unwrap();
        assert_eq!(o.status, None);
        // The ping's answer may still be in the buffer.
        let o = parse("1", "PONG 1\nout\n\nEND 1 0\n").unwrap();
        assert_eq!(o.output, "PONG 1\nout\n");
    }

    /// In-memory stream: what the host writes is recorded, what it reads
    /// comes from a script of chunks.
    struct Fake {
        written: Vec<u8>,
        chunks: std::collections::VecDeque<Vec<u8>>,
    }

    impl Read for Fake {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.chunks.pop_front() {
                Some(c) => {
                    let n = c.len().min(buf.len());
                    buf[..n].copy_from_slice(&c[..n]);
                    Ok(n)
                }
                None => Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
            }
        }
    }

    impl Write for Fake {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn poll(f: &mut Fake) -> std::io::Result<Vec<u8>> {
        let mut buf = [0u8; 64];
        match f.read(&mut buf) {
            Ok(n) => Ok(buf[..n].to_vec()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    #[test]
    fn handshake_and_run_over_a_stream() {
        let mut dev = Fake {
            written: Vec::new(),
            chunks: vec![b"PO".to_vec(), b"NG 9\n".to_vec()].into(),
        };
        assert!(handshake(&mut dev, "9", Duration::from_secs(1), &poll).unwrap());
        assert_eq!(dev.written, b"PING 9\n");

        dev.chunks = vec![b"a\nb".to_vec(), b"\n\nEND 9 2\n".to_vec()].into();
        dev.written.clear();
        let o = run(
            &mut dev,
            "9",
            "user",
            "printf 'a\\nb\\n'",
            Duration::from_secs(1),
            &poll,
        )
        .unwrap();
        assert_eq!(dev.written, b"RUN 9 user printf 'a\\nb\\n'\n");
        assert_eq!(o.output, "a\nb\n");
        assert_eq!(o.status, Some(2));
    }

    #[test]
    fn silence_is_no_agent_or_a_timeout() {
        let mut dev = Fake {
            written: Vec::new(),
            chunks: vec![b"noise\n".to_vec()].into(),
        };
        assert!(!handshake(&mut dev, "9", Duration::from_millis(60), &poll).unwrap());
        let err = run(
            &mut dev,
            "9",
            "user",
            "true",
            Duration::from_millis(60),
            &poll,
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn prepare_clears_the_stale_socket_for_agent_guests_only() {
        let cfg = GlobalConfig::default();
        let m = MockRunner::new();
        let a = Agent::new(&m, &cfg);
        let q = Qube::new("work", QubeKind::App, OsType::Linux);
        a.prepare(&q).unwrap();
        assert!(m
            .calls()
            .iter()
            .any(|c| c == "rm -f /var/run/juanitos/work.agent"));
        assert!(!a.available(&q));

        let m = MockRunner::new();
        let a = Agent::new(&m, &cfg);
        let rx = Qube::new("rx", QubeKind::App, OsType::Redox);
        a.prepare(&rx).unwrap();
        assert!(m.calls().is_empty());
        assert!(!a.available(&rx));
    }
}
