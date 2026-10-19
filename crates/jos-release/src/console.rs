//! A minimal "expect" over a child process's stdio: wait for a pattern,
//! send a line. Used to drive the FreeBSD builder VM's serial console so
//! stage 1 runs unattended from a Linux host (`jos-release builder --auto`).
//!
//! Everything the guest prints is echoed to our stdout (and optionally a
//! log file), so a hung build is visible rather than silent.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};

/// Serial input pacing for [`Console::send_line`]: measured on a FreeBSD
/// 15.1 guest under QEMU/KVM, a 500-byte line sent at once loses bytes,
/// 2000 bytes in 32-byte chunks 10 ms apart arrive intact.
const SEND_CHUNK: usize = 32;
const SEND_PAUSE: Duration = Duration::from_millis(10);

pub struct Console {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Vec<u8>>,
    /// Unconsumed output, searched by `expect`.
    buf: Vec<u8>,
    /// Bound on `buf`: past it, the older half is dropped.
    window: usize,
    log: Option<std::fs::File>,
}

impl Console {
    /// Take over a child's piped stdin/stdout.
    pub fn attach(mut child: Child, log: Option<std::fs::File>) -> anyhow::Result<Self> {
        let (Some(stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
            bail!("the child's stdin and stdout must be piped");
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                // A read error ends the console like end of file does.
                let n = stdout.read(&mut chunk).unwrap_or(0);
                if n == 0 || tx.send(chunk.get(..n).unwrap_or(&[]).to_vec()).is_err() {
                    break;
                }
            }
        });
        Ok(Console {
            child,
            stdin,
            rx,
            buf: Vec::new(),
            window: 1 << 20,
            log,
        })
    }

    fn pump(&mut self, timeout: Duration) -> anyhow::Result<bool> {
        match self.rx.recv_timeout(timeout) {
            Ok(data) => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&data);
                let _ = out.flush();
                if let Some(log) = &mut self.log {
                    let _ = log.write_all(&data);
                }
                self.buf.extend_from_slice(&data);
                if self.buf.len() > self.window {
                    let cut = self.buf.len() - self.window / 2;
                    self.buf.drain(..cut);
                }
                Ok(true)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(false),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("the guest's console closed")
            }
        }
    }

    /// Wait until `pattern` appears in the output, consuming through it.
    /// Returns what preceded the match (since the previous `expect`).
    pub fn expect(&mut self, pattern: &str, timeout: Duration) -> anyhow::Result<String> {
        self.wait_for(&[pattern], timeout).map(|(_, before)| before)
    }

    /// Wait for whichever of `patterns` appears first in the output and
    /// return its index, consuming through it.
    pub fn expect_any(&mut self, patterns: &[&str], timeout: Duration) -> anyhow::Result<usize> {
        self.wait_for(patterns, timeout).map(|(i, _)| i)
    }

    fn wait_for(
        &mut self,
        patterns: &[&str],
        timeout: Duration,
    ) -> anyhow::Result<(usize, String)> {
        let deadline = Instant::now() + timeout;
        loop {
            let first = patterns
                .iter()
                .enumerate()
                .filter_map(|(i, p)| find(&self.buf, p.as_bytes()).map(|pos| (pos, i, p.len())))
                .min();
            if let Some((pos, i, len)) = first {
                let before: Vec<u8> = self.buf.drain(..pos + len).collect();
                let before = before.get(..pos).unwrap_or(&[]);
                return Ok((i, String::from_utf8_lossy(before).into_owned()));
            }
            let now = Instant::now();
            if now >= deadline {
                bail!(
                    "timed out after {}s waiting for {patterns:?}",
                    timeout.as_secs()
                );
            }
            self.pump((deadline - now).min(Duration::from_millis(500)))?;
        }
    }

    /// Send bytes as they are (control characters such as ^D included).
    /// (`ChildStdin` is unbuffered: there is nothing to flush.)
    pub fn send_raw(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.stdin
            .write_all(data)
            .context("writing to the guest's console")
    }

    /// Send a line, paced: written in one burst, a line longer than a few
    /// hundred bytes overruns the guest's emulated 16550 and the shell
    /// never sees its end (the command then hangs instead of failing).
    pub fn send_line(&mut self, line: &str) -> anyhow::Result<()> {
        let mut data = line.as_bytes().to_vec();
        data.push(b'\n');
        for chunk in data.chunks(SEND_CHUNK) {
            self.send_raw(chunk)?;
            std::thread::sleep(SEND_PAUSE);
        }
        Ok(())
    }

    /// Run a shell command in the guest and wait for it to finish; the
    /// guest's exit status becomes ours. `marker` must not occur in the
    /// command's own output.
    pub fn run(&mut self, cmd: &str, timeout: Duration) -> anyhow::Result<()> {
        // The shell prints `<marker>=<status>.` once the command is done.
        // The marker is split in the command line (`JOS_""DONE`) so the
        // echoed command itself cannot match it.
        let marker = format!("JOS_DONE_{:x}", self.buf.len() ^ 0x5a5a);
        let split = marker.replacen("JOS_DONE", "JOS_\"\"DONE", 1);
        self.send_line(&format!("{cmd}; echo {split}=$?."))?;
        self.expect(&format!("{marker}="), timeout)?;
        let status = self.expect(".", Duration::from_secs(10))?;
        match status.trim().parse::<i32>() {
            Ok(0) => Ok(()),
            Ok(code) => bail!("guest command failed with status {code}: {cmd}"),
            Err(_) => bail!("could not read the status of: {cmd} (got {status:?})"),
        }
    }

    /// Wait for the guest to exit on its own (after `poweroff`).
    pub fn wait_exit(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            // A failing try_wait is retried until the deadline.
            if let Ok(Some(st)) = self.child.try_wait() {
                return if st.success() {
                    Ok(())
                } else {
                    bail!("qemu exited with {st}")
                };
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                bail!("the guest did not power off in time; killed");
            }
            let _ = self.pump(Duration::from_millis(500));
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_bytes() {
        assert_eq!(find(b"hello login: ", b"login:"), Some(6));
        assert_eq!(find(b"abc", b"abcd"), None);
        assert_eq!(find(b"abc", b""), None);
    }

    #[cfg(unix)]
    #[test]
    fn drives_a_shell() {
        use std::process::{Command, Stdio};
        let child = Command::new("sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut c = Console::attach(child, None).unwrap();
        c.send_line("echo ready").unwrap();
        c.expect("ready", Duration::from_secs(5)).unwrap();
        c.run("true", Duration::from_secs(5)).unwrap();
        assert!(c.run("false", Duration::from_secs(5)).is_err());
        // A line spanning many send chunks arrives whole.
        let long = format!("test {} = {}", "y".repeat(700), "y".repeat(700));
        c.run(&long, Duration::from_secs(10)).unwrap();
        c.send_line("exit 0").unwrap();
        c.wait_exit(Duration::from_secs(5)).unwrap();
    }

    #[cfg(unix)]
    fn shell(log: Option<std::fs::File>) -> Console {
        use std::process::{Command, Stdio};
        let child = Command::new("sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Console::attach(child, log).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn timeouts_failures_and_logging() {
        let path = std::env::temp_dir().join(format!("jos-console-{}.log", std::process::id()));
        let mut c = shell(Some(std::fs::File::create(&path).unwrap()));
        let err = c
            .expect("never printed", Duration::from_millis(300))
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        let err = c.run(
            "exit_with() { return 3; }; exit_with",
            Duration::from_secs(5),
        );
        assert!(err.unwrap_err().to_string().contains("status 3"));
        // Everything the guest printed went to the log as well.
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("JOS_DONE_"));
        let _ = std::fs::remove_file(&path);

        // A guest that exits on its own with an error status.
        c.send_line("exit 4").unwrap();
        let err = c.wait_exit(Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().contains("qemu exited with"), "{err}");
        let err = c.expect("x", Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().contains("console closed"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_first_of_several_patterns_wins() {
        let mut c = shell(None);
        c.send_line("echo two; echo one").unwrap();
        let t = Duration::from_secs(5);
        assert_eq!(c.expect_any(&["one", "two"], t).unwrap(), 1);
        assert_eq!(c.expect_any(&["one", "two"], t).unwrap(), 0);
        let err = c
            .expect_any(&["a", "b"], Duration::from_millis(200))
            .unwrap_err();
        assert!(err.to_string().contains(r#"["a", "b"]"#), "{err}");
        // Raw bytes go through as they are, no newline added.
        c.send_raw(b"printf 'raw\\n'\n").unwrap();
        c.expect("raw", t).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn only_a_piped_child_can_be_attached() {
        use std::process::{Command, Stdio};
        let child = Command::new("true").stdin(Stdio::piped()).spawn().unwrap();
        let err = Console::attach(child, None).err().unwrap();
        assert_eq!(
            err.to_string(),
            "the child's stdin and stdout must be piped"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_guest_that_is_gone_cannot_be_written_to() {
        let mut c = shell(None);
        c.send_line("exit 0").unwrap();
        c.wait_exit(Duration::from_secs(5)).unwrap();
        let err = c.run("true", Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.to_string(), "writing to the guest's console");
    }

    /// The guest's end of [`Console::run`]: `echo <marker>=<status>.`,
    /// replaced by a shell function to play a broken guest.
    #[cfg(unix)]
    #[test]
    fn a_status_that_never_comes_or_is_garbled() {
        let t = Duration::from_secs(5);
        let mut c = shell(None);
        let err = c.run("exit 0", t).unwrap_err();
        assert_eq!(err.to_string(), "the guest's console closed");

        let mut c = shell(None);
        c.send_line(r#"echo() { printf '%s' "${1%%=*}="; exit 0; }"#)
            .unwrap();
        let err = c.run("true", t).unwrap_err();
        assert_eq!(err.to_string(), "the guest's console closed");

        let mut c = shell(None);
        c.send_line(r#"echo() { printf '%s' "${1%%=*}=x."; }"#)
            .unwrap();
        let err = c.run("true", t).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"could not read the status of: true (got "x")"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_search_window_is_bounded() {
        let mut c = shell(None);
        c.window = 64;
        // 103 bytes before the match: the front is dropped on the way.
        c.send_line("printf '%0100dEND'").unwrap();
        let before = c.expect("END", Duration::from_secs(5)).unwrap();
        assert!(before.len() < 100);
        assert!(before.bytes().all(|b| b == b'0'));
    }

    #[cfg(unix)]
    #[test]
    fn a_guest_that_does_not_power_off_is_killed() {
        let mut c = shell(None);
        let err = c.wait_exit(Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("killed"), "{err}");
        // And one still running when the console is dropped.
        drop(shell(None));
    }
}
