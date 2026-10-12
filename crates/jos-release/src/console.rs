//! A minimal "expect" over a child process's stdio: wait for a pattern,
//! send a line. Used to drive the FreeBSD builder VM's serial console so
//! stage 1 runs unattended from a Linux host (`jos-release builder --auto`).
//!
//! Everything the guest prints is echoed to our stdout (and optionally a
//! log file), so a hung build is visible rather than silent.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};

pub struct Console {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Vec<u8>>,
    /// Unconsumed output, searched by `expect`.
    buf: Vec<u8>,
    log: Option<std::fs::File>,
}

impl Console {
    /// Take over a child's piped stdin/stdout.
    pub fn attach(mut child: Child, log: Option<std::fs::File>) -> anyhow::Result<Self> {
        let stdin = child.stdin.take().context("child stdin not piped")?;
        let stdout: ChildStdout = child.stdout.take().context("child stdout not piped")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stdout = stdout;
            let mut chunk = [0u8; 4096];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(chunk.get(..n).unwrap_or(&[]).to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Console {
            child,
            stdin,
            rx,
            buf: Vec::new(),
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
                // Keep the search window bounded.
                if self.buf.len() > 1 << 20 {
                    let cut = self.buf.len() - (1 << 19);
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
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pos) = find(&self.buf, pattern.as_bytes()) {
                let before: Vec<u8> = self.buf.drain(..pos + pattern.len()).collect();
                let before = before.get(..pos).unwrap_or(&[]);
                return Ok(String::from_utf8_lossy(before).into_owned());
            }
            let now = Instant::now();
            if now >= deadline {
                bail!(
                    "timed out after {}s waiting for {pattern:?}",
                    timeout.as_secs()
                );
            }
            self.pump((deadline - now).min(Duration::from_millis(500)))?;
        }
    }

    pub fn send_line(&mut self, line: &str) -> anyhow::Result<()> {
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
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
            if let Some(st) = self.child.try_wait()? {
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
        c.send_line("exit 0").unwrap();
        c.wait_exit(Duration::from_secs(5)).unwrap();
    }
}
