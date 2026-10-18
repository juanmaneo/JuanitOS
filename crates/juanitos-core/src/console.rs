//! Running a command inside a qube over its serial console: what `jos
//! run` falls back to when the qube has no agent channel
//! ([`crate::agent`]): Redox qubes, clones made before the daemon
//! existed, or `jos run --console`.
//!
//! What every qube does have is a serial console on an `nmdm(4)` pair,
//! with the templates logging a shell in on it automatically. `jos run
//! <qube> <command>` types the command there, bracketed by a marker, and
//! prints what comes back.
//!
//! The protocol — framing, prompt synchronisation, exit status — is pure
//! and tested here; the host side is a few `read`/`write`s on the device.
//!
//! With a POSIX shell (`sh`, `bash`: the FreeBSD and Linux templates) a
//! single line is typed: a no-op tag so the echo is recognisable, the
//! command, then a `printf` of the marker with `$?`. One line, because a
//! command that reads the terminal (`sudo` with `use_pty`, `cat`, an
//! editor) would otherwise swallow a second typed line, marker included,
//! and `jos run` would wait for nothing. The price is that a command
//! ending in a `#` comment hides the marker: not a use case.
//!
//! Redox's `ion` on a serial console has no job control: it cannot take
//! the terminal for an external command, aborts the rest of the line and
//! lets the command run detached. What it does track is a background
//! job, reporting `ion: ([0] PID) exited with N` when it ends; so the
//! [`Dialect::Ion`] script runs the command with `&` and that report is
//! the marker.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// Marker the guest echoes after the command, with its exit status.
pub const MARKER: &str = "JOS_RUN_END";

/// The guest shell's flavour, which decides how a command is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    /// `sh`-compatible: tagged command line, then the marker `printf`.
    #[default]
    Posix,
    /// Redox's `ion`: background job, ion's own exit report as marker.
    Ion,
}

/// What is typed for `dialect` (see [`script`] and the module docs).
pub fn script_for(dialect: Dialect, command: &str) -> String {
    match dialect {
        Dialect::Posix => script(command),
        Dialect::Ion => format!("{command} &\r"),
    }
}

/// Find the end of the command in `text` for `dialect`.
pub fn parse_for(dialect: Dialect, text: &str) -> Option<Outcome> {
    match dialect {
        Dialect::Posix => parse(text),
        Dialect::Ion => parse_ion(text),
    }
}

/// ion: output between `ion: bg [0] PID` and `ion: ([0] PID) exited with
/// N`; the prompt ion prints right after starting the job is dropped.
fn parse_ion(text: &str) -> Option<Outcome> {
    let (before_bg, from_bg) = text.split_once("ion: bg [")?;
    let (_, body) = from_bg.split_once('\n')?;
    let (output, report) = body.split_once("ion: ([")?;
    let (line, _) = report.split_once('\n')?;
    let status = line.rsplit("exited with ").next().unwrap_or(line);
    Some(Outcome {
        output: drop_ion_prompt(&output.replace('\r', ""), ion_prompt(before_bg)),
        status: status.trim().parse().ok(),
    })
}

/// The prompt ion draws, taken from the echo of the typed command: what
/// precedes ` &` on the last line before the job report.
fn ion_prompt(before_bg: &str) -> Option<&str> {
    let line = before_bg.trim_end_matches(['\r', '\n']);
    let line = line.rsplit(['\r', '\n']).next().unwrap_or(line);
    let (prompt, _) = line.trim_end().rsplit_once(" &")?;
    // The command may itself contain spaces: the prompt is the part up to
    // the first `$ ` or `# ` on the line.
    let end = prompt.find("$ ").or_else(|| prompt.find("# "))? + 2;
    // A bare `$ ` would match inside ordinary output; a real prompt has
    // a user name (and a path) in front of its sigil.
    prompt.get(..end).filter(|p| p.len() > 2)
}

/// ion redraws its prompt (preceded by its `\u{23ce}` missing-newline
/// mark and padding) while the job's output arrives; take those off.
fn drop_ion_prompt(output: &str, prompt: Option<&str>) -> String {
    let mut out = String::with_capacity(output.len());
    let mut rest = output;
    while let Some(i) = rest.find('\u{23ce}') {
        out.push_str(rest.get(..i).unwrap_or(""));
        rest = rest
            .get(i + '\u{23ce}'.len_utf8()..)
            .unwrap_or("")
            .trim_start_matches(' ');
    }
    out.push_str(rest);
    match prompt {
        Some(p) => out.replace(p, ""),
        None => out,
    }
}

/// Argument of the no-op `:` that opens the typed line, identifying its
/// echo.
pub const TAG: &str = "JOS_RUN";

/// The marker as it appears in the typed text (split, so the echo of the
/// line does not match the real one).
fn typed_marker() -> String {
    let (a, b) = MARKER.split_at(4);
    format!("{a}''{b}")
}

/// What is typed on the console: `: TAG; command; printf marker $?`.
pub fn script(command: &str) -> String {
    format!(
        ": {TAG}; {command}; printf '\\n{} %s\\n' $?\r",
        typed_marker()
    )
}

/// Result of a command run over the console.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Guest output between the typed line and the marker.
    pub output: String,
    pub status: Option<i32>,
}

/// Find the marker in `text`; returns the output before it and the
/// status after it. `None` until the marker is complete (its newline
/// has arrived).
pub fn parse(text: &str) -> Option<Outcome> {
    let (output, after) = text.split_once(&format!("\n{MARKER} "))?;
    let (status, _) = after.split_once('\n')?;
    let status = status.trim().parse().ok();
    // Whatever precedes the echo of the typed line is stale (a prompt,
    // the end of a banner); the echo itself goes too, and a line editor
    // may render it more than once (redraws after a wrap, or the tail of
    // the line alone: no tag, but the split marker), so every line
    // showing either goes.
    let tag = format!(": {TAG};");
    let echo = typed_marker();
    let output = match output.find(&tag) {
        Some(i) => output.get(i..).unwrap_or(""),
        None => output,
    };
    let output: String = output
        .split_inclusive('\n')
        .filter(|line| !(line.contains(&tag) || line.contains(&echo)))
        .collect();
    Some(Outcome {
        output: output.replace('\r', ""),
        status,
    })
}

/// Credentials typed when the console shows a `login:` prompt instead of
/// a shell (guests without autologin, such as Redox).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub user: String,
    pub password: String,
}

/// Collect console output for `window`, returning it with escapes removed.
/// Only used to see what the console shows before the command is typed:
/// a read error counts as nothing shown, and the same error surfaces when
/// the command is typed or its output read.
fn drain<D: Read + Write>(
    dev: &mut D,
    poll: &impl Fn(&mut D) -> std::io::Result<Vec<u8>>,
    window: Duration,
) -> String {
    let mut buf = Vec::new();
    let start = Instant::now();
    while start.elapsed() < window {
        let chunk = poll(dev).unwrap_or_default();
        if chunk.is_empty() {
            std::thread::sleep(Duration::from_millis(50));
        } else {
            buf.extend_from_slice(&chunk);
        }
    }
    strip_ansi(&String::from_utf8_lossy(&buf))
}

fn send(dev: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    dev.write_all(bytes).and_then(|()| dev.flush())
}

/// When `seen` ends with `prompt`, type `reply` and collect what follows.
fn answer<D: Read + Write>(
    dev: &mut D,
    poll: &impl Fn(&mut D) -> std::io::Result<Vec<u8>>,
    seen: String,
    prompt: &str,
    reply: &str,
) -> std::io::Result<String> {
    if !wants(&seen, prompt) {
        return Ok(seen);
    }
    send(dev, format!("{reply}\n").as_bytes()).map(|()| drain(dev, poll, Duration::from_secs(2)))
}

/// True when the console is waiting at a `login:` / `password:` prompt.
pub fn wants(text: &str, prompt: &str) -> bool {
    text.trim_end().to_ascii_lowercase().ends_with(prompt)
}

/// True when the text ends with something that looks like a shell prompt.
pub fn at_prompt(text: &str) -> bool {
    text.trim_end().ends_with(['$', '#', '>', '%'])
}

/// Drive a console: wake the shell (logging in first when a `login:`
/// prompt answers and credentials are given), type the script, collect
/// until the marker or the deadline. Generic over the device so it is
/// tested with in-memory streams.
pub fn run<D: Read + Write>(
    dev: &mut D,
    command: &str,
    timeout: Duration,
    login: Option<&Login>,
    dialect: Dialect,
    poll: impl Fn(&mut D) -> std::io::Result<Vec<u8>>,
) -> std::io::Result<Outcome> {
    // Wake the line and let any pending output (a login banner) drain.
    send(dev, b"\r")?;
    let mut seen = drain(dev, &poll, Duration::from_millis(500));
    if let Some(l) = login {
        seen = answer(dev, &poll, seen, "login:", &l.user)
            .and_then(|seen| answer(dev, &poll, seen, "password:", &l.password))?;
    }
    // A shell that is still starting (autologin right after boot) would
    // lose what is typed now: give it a moment to show its prompt.
    let patience = Instant::now();
    while !at_prompt(&seen) && patience.elapsed() < Duration::from_secs(3) {
        seen.push_str(&drain(dev, &poll, Duration::from_millis(250)));
    }
    send(dev, script_for(dialect, command).as_bytes())?;
    let mut buf = Vec::new();
    let deadline = Instant::now() + timeout;
    loop {
        let chunk = poll(dev)?;
        if chunk.is_empty() {
            if Instant::now() > deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "no end marker within {}s; the qube may have no shell on its console",
                        timeout.as_secs()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        buf.extend_from_slice(&chunk);
        let text = strip_ansi(&String::from_utf8_lossy(&buf));
        if let Some(outcome) = parse_for(dialect, &text) {
            return Ok(outcome);
        }
    }
}

/// Remove ANSI escape sequences (prompts and `ls` colours).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                // CSI: parameters, then a final letter (or `~`).
                Some('[') => {
                    chars.next();
                    for d in chars.by_ref() {
                        if d.is_ascii_alphabetic() || d == '~' {
                            break;
                        }
                    }
                }
                // OSC (window titles): up to BEL or ESC `\`.
                Some(']') => {
                    chars.next();
                    while let Some(d) = chars.next() {
                        if d == '\u{7}' || (d == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_splits_the_marker_and_parse_finds_it() {
        let s = script("uname -a");
        assert_eq!(
            s,
            ": JOS_RUN; uname -a; printf '\\nJOS_''RUN_END %s\\n' $?\r"
        );
        assert!(
            !s.contains(MARKER),
            "typed text must not contain the marker"
        );
        // A plain shell: the typed line is echoed, then the output.
        let text = "\r\nuser@jail:~ $ : JOS_RUN; uname -a; printf '\\nJOS_''RUN_END %s\\n' $?\r\n\
                    FreeBSD jail 15.1\r\n\nJOS_RUN_END 0\nuser@jail:~ $ ";
        let o = parse(text).unwrap();
        assert_eq!(o.output, "FreeBSD jail 15.1\n");
        assert_eq!(o.status, Some(0));
        assert_eq!(parse("partial\nJOS_RUN_END 1").map(|o| o.status), None);
        assert_eq!(parse("x\n\nJOS_RUN_END 7\n").unwrap().status, Some(7));
        assert_eq!(parse("\nJOS_RUN_END ?\n").unwrap().status, None);
        assert_eq!(parse("\nJOS_RUN_END 0\n").unwrap().output, "");
        // Without an echo (a quiet console) the output is kept whole.
        assert_eq!(parse("out\n\nJOS_RUN_END 0\n").unwrap().output, "out\n");
        // The typed line rendered twice (libedit redraw after a wrap),
        // then only its tail.
        let twice = "$ : JOS_RUN; long cmd; printf '\r\n$ : JOS_RUN; long cmd; printf '\r\n\
                     $ printf '\\nJOS_''RUN_END %s\\n' $?\r\nout\r\n\nJOS_RUN_END 0\n";
        assert_eq!(parse(twice).unwrap().output, "out\n");
        // Output that itself mentions `printf '` is kept.
        let quoted = "$ : JOS_RUN; cat s; printf '...\r\nprintf 'hi'\r\n\nJOS_RUN_END 0\n";
        assert_eq!(parse(quoted).unwrap().output, "printf 'hi'\n");
        // ion's line editor redraws the typed line on every character
        // (with `\r`), and prints the prompt before the output.
        let ion =
            "user:~$ i\ruser:~$ id\ruser:~$ : JOS_RUN; id; printf '\\nJOS_''RUN_END %s\\n' $?\r\n\
                   ion: pipeline execution error: could not grab the terminal\r\n\
                   \u{23ce}          user:~$ uid=1000(user)\r\n\nJOS_RUN_END 1\n";
        let o = parse(ion).unwrap();
        assert_eq!(
            o.output,
            "ion: pipeline execution error: could not grab the terminal\n\u{23ce}          user:~$ uid=1000(user)\n"
        );
        assert_eq!(o.status, Some(1));
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(strip_ansi("a\u{1b}[32mb\u{1b}[0mc\u{1b}"), "abc");
        assert_eq!(strip_ansi("\u{1b}[?2004h$ "), "$ ");
        // OSC window titles (ion), terminated by BEL or ESC \.
        assert_eq!(strip_ansi("\u{1b}]0;user: /home\u{7}user:~$ "), "user:~$ ");
        assert_eq!(strip_ansi("\u{1b}]2;t\u{1b}\\x\u{1b}]unterminated"), "x");
    }

    /// A scripted console: what is written is recorded, and a write that
    /// starts with a rule's key queues that rule's reply, the way a shell
    /// (or a getty) answers each typed line. Writes are refused once
    /// `writes_ok` were made, reads once `fail_reads_after` writes were.
    struct Console {
        written: Vec<u8>,
        pending: Vec<u8>,
        rules: Vec<(&'static str, &'static [u8])>,
        writes: usize,
        writes_ok: usize,
        fail_reads_after: Option<usize>,
    }
    impl Console {
        fn new(rules: &[(&'static str, &'static [u8])]) -> Self {
            Console {
                written: Vec::new(),
                pending: Vec::new(),
                rules: rules.to_vec(),
                writes: 0,
                writes_ok: usize::MAX,
                fail_reads_after: None,
            }
        }
    }
    impl Read for Console {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if self.fail_reads_after.is_some_and(|w| self.writes >= w) {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            let n = self.pending.len().min(b.len());
            b[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            Ok(n)
        }
    }
    impl Write for Console {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.writes >= self.writes_ok {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.writes += 1;
            self.written.extend_from_slice(b);
            let line = String::from_utf8_lossy(b);
            if let Some((_, reply)) = self.rules.iter().find(|(k, _)| line.starts_with(k)) {
                self.pending.extend_from_slice(reply);
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn poll(d: &mut Console) -> std::io::Result<Vec<u8>> {
        let mut b = [0u8; 16];
        d.read(&mut b).map(|n| b[..n].to_vec())
    }

    #[test]
    fn a_console_that_breaks_at_any_point() {
        let login = Login {
            user: "user".into(),
            password: "pw".into(),
        };
        let run_on = |mut dev: Console| {
            run(
                &mut dev,
                "id",
                Duration::from_millis(100),
                Some(&login),
                Dialect::Posix,
                poll,
            )
        };
        let refusing = |writes_ok, wake: &'static [u8]| Console {
            writes_ok,
            ..Console::new(&[("\r", wake)])
        };
        // Waking it; answering its login prompt; typing the command.
        assert!(run_on(refusing(0, b"")).is_err());
        assert!(run_on(refusing(1, b"login: ")).is_err());
        assert!(run_on(refusing(1, b"$ ")).is_err());
        // Reading the answer: unreadable from the start, then only once
        // the command was typed.
        for fail_reads_after in [1, 2] {
            let dev = Console {
                fail_reads_after: Some(fail_reads_after),
                ..Console::new(&[("\r", b"$ ")])
            };
            let err = run_on(dev).unwrap_err();
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::BrokenPipe,
                "{fail_reads_after}"
            );
        }
    }

    #[test]
    fn run_collects_output_and_status() {
        let mut dev = Console::new(&[
            ("\r", b"$ "),
            (
                ": JOS_RUN",
                b"\r\n: JOS_RUN; id; printf '...\r\nuid=1001(user)\r\n\nJOS_RUN_END 3\n$ ",
            ),
        ]);
        let o = run(
            &mut dev,
            "id",
            Duration::from_secs(5),
            None,
            Dialect::Posix,
            poll,
        )
        .unwrap();
        assert_eq!(o.output, "uid=1001(user)\n");
        assert_eq!(o.status, Some(3));
        let typed = String::from_utf8(dev.written).unwrap();
        assert!(typed.starts_with('\r'));
        assert!(typed.contains(": JOS_RUN; id; printf"));
    }

    /// A console at a login prompt.
    const GETTY: (&str, &[u8]) = ("\r", b"\r\nredox login: ");

    #[test]
    fn run_logs_in_when_a_getty_answers() {
        let login = Login {
            user: "user".into(),
            password: String::new(),
        };
        let mut dev = Console::new(&[
            GETTY,
            ("user\n", b"\r\npassword: "),
            ("\n", b"\r\nWelcome\r\nuser:~$ "),
            (
                ": JOS_RUN",
                b": JOS_RUN; id; printf '\r\nuid=1000\r\n\nJOS_RUN_END 0\n",
            ),
        ]);
        let o = run(
            &mut dev,
            "id",
            Duration::from_secs(5),
            Some(&login),
            Dialect::Posix,
            poll,
        )
        .unwrap();
        assert_eq!(o.status, Some(0));
        assert_eq!(o.output, "uid=1000\n");
        let typed = String::from_utf8(dev.written).unwrap();
        assert!(
            typed.starts_with("\ruser\n\n: JOS_RUN; id; printf"),
            "{typed:?}"
        );
        // Without credentials the prompt is left alone, and nothing ever
        // answers the command.
        let mut dev = Console::new(&[GETTY]);
        let err = run(
            &mut dev,
            "id",
            Duration::from_millis(100),
            None,
            Dialect::Posix,
            poll,
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("no shell"));
        assert!(!String::from_utf8(dev.written).unwrap().contains("user\n"));
        assert!(wants("\r\nRedox Login: ", "login:"));
        assert!(!wants("user@host:~ $ ", "login:"));
        assert!(at_prompt("user@host:~ $ ") && at_prompt("root:~# "));
        assert!(!at_prompt("redox login: ") && !at_prompt(""));
    }

    #[test]
    fn ion_dialect_uses_a_background_job_and_its_report() {
        assert_eq!(script_for(Dialect::Ion, "uname -a"), "uname -a &\r");
        assert_eq!(script_for(Dialect::Posix, "x"), script("x"));
        // What the console shows, after ANSI stripping.
        let text = "user:~$ u\ruser:~$ uname -a &\r\nion: bg [0] 88\r\n\
                    \u{23ce}        user:~$ user:~$ Redox redox 0.5.12 x86_64 Redox\r\n\
                    ion: ([0] 88) exited with 0\r\nuser:~$ ";
        let o = parse_for(Dialect::Ion, text).unwrap();
        assert_eq!(o.output, "Redox redox 0.5.12 x86_64 Redox\n");
        assert_eq!(o.status, Some(0));
        let failed = "x &\r\nion: bg [0] 91\r\nion: ([0] 91) exited with 7\r\n";
        let o = parse_for(Dialect::Ion, failed).unwrap();
        assert_eq!(o.output, "");
        assert_eq!(o.status, Some(7));
        // Incomplete: no report yet, or the report line not finished.
        assert!(parse_for(Dialect::Ion, "ion: bg [0] 5\r\nout\r\n").is_none());
        assert!(parse_for(Dialect::Ion, "ion: bg [0] 5\r\nion: ([0] 5) exited with 1").is_none());
        assert!(parse_for(Dialect::Ion, "nothing").is_none());
        assert!(parse_for(Dialect::Ion, "ion: bg [0] 5").is_none());
        // Root prompt, a command with spaces, the prompt redrawn in the
        // middle of the output, no echo at all, and a `$` in the output.
        let root = "root:~# ls -l /tmp &\r\nion: bg [0] 3\r\n\u{23ce}  root:~# hi\r\n\
                    ion: ([0] 3) exited with 0\r\n";
        assert_eq!(parse_for(Dialect::Ion, root).unwrap().output, "hi\n");
        let mid = "user:/tmp$ ping &\r\nion: bg [0] 3\r\nPING\r\n\u{23ce}      user:/tmp$ user:/tmp$ From\r\n\
                   ion: ([0] 3) exited with 0\r\n";
        assert_eq!(parse_for(Dialect::Ion, mid).unwrap().output, "PING\nFrom\n");
        let none = "ion: bg [0] 3\r\nhi\r\nion: ([0] 3) exited with 0\r\n";
        assert_eq!(parse_for(Dialect::Ion, none).unwrap().output, "hi\n");
        let dollar =
            "u:~$ x &\r\nion: bg [0] 3\r\ncost: 5 $ each\r\nion: ([0] 3) exited with 0\r\n";
        assert_eq!(
            parse_for(Dialect::Ion, dollar).unwrap().output,
            "cost: 5 $ each\n"
        );
        let empty = "ion: bg [0] 3\r\nion: ([0] 3) exited with 0\r\n";
        assert_eq!(parse_for(Dialect::Ion, empty).unwrap().output, "");
        assert_eq!(
            ion_prompt("user:~$ u\ruser:~$ uname -a &\r\n"),
            Some("user:~$ ")
        );
        assert_eq!(ion_prompt("garbage\n"), None);
        assert_eq!(ion_prompt("$ x &"), None, "an empty prompt is no prompt");
        assert_eq!(Dialect::default(), Dialect::Posix);
    }
}
