//! Command execution abstraction.
//!
//! Everything that talks to the host (`bhyve`, `zfs`, `ifconfig`, ...) goes
//! through a [`Runner`] so that the CLI can run in `--dry-run` mode and the
//! library can be unit-tested on any platform.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::error::{Error, Result};

/// Captured output of a finished process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdOutput {
    /// Exit code, `None` if the process was killed by a signal.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn ok(stdout: impl Into<String>) -> Self {
        CmdOutput {
            status: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    pub fn failed(code: i32, stderr: impl Into<String>) -> Self {
        CmdOutput {
            status: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

/// Executes external programs.
pub trait Runner: Send + Sync {
    /// Run `program` with `args`, capturing output. A non-zero exit status is
    /// *not* an error here; inspect [`CmdOutput::success`].
    fn run(&self, program: &str, args: &[&str]) -> Result<CmdOutput>;

    /// Like [`Runner::run`] but returns [`Error::Command`] on non-zero exit.
    fn run_checked(&self, program: &str, args: &[&str]) -> Result<CmdOutput> {
        check(program, args, self.run(program, args)?)
    }

    /// Run `program` feeding `input` on its standard input (used for
    /// secrets such as passwords, which must not appear in argv).
    fn run_with_input(&self, program: &str, args: &[&str], input: &str) -> Result<CmdOutput>;

    /// Run `program` with the caller's standard input inherited instead of
    /// `/dev/null`, for tools that act on the terminal itself (`stty`).
    /// Mocks record it like a plain [`Runner::run`].
    fn run_inheriting_stdin(&self, program: &str, args: &[&str]) -> Result<CmdOutput> {
        self.run(program, args)
    }

    fn run_with_input_checked(
        &self,
        program: &str,
        args: &[&str],
        input: &str,
    ) -> Result<CmdOutput> {
        self.run_with_input(program, args, input)
            .and_then(|out| check(program, args, out))
    }
}

fn check(program: &str, args: &[&str], out: CmdOutput) -> Result<CmdOutput> {
    if out.success() {
        Ok(out)
    } else {
        Err(Error::Command {
            program: shell_join(program, args),
            code: out.status,
            stderr: out.stderr.trim().to_string(),
        })
    }
}

/// Runs commands for real with `std::process::Command`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealRunner;

fn convert(out: std::process::Output) -> CmdOutput {
    CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Name the program in an I/O error from starting, feeding or waiting on
/// it (a missing tool otherwise reads "No such file or directory"). The
/// kind is kept: callers fall back on `NotFound`.
fn io_error(program: &str) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("running {program}: {e}"),
        ))
    }
}

impl Runner for RealRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CmdOutput> {
        Ok(convert(
            std::process::Command::new(program)
                .args(args)
                .output()
                .map_err(io_error(program))?,
        ))
    }

    fn run_inheriting_stdin(&self, program: &str, args: &[&str]) -> Result<CmdOutput> {
        use std::process::Stdio;
        Ok(convert(
            std::process::Command::new(program)
                .args(args)
                .stdin(Stdio::inherit())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .map_err(io_error(program))?,
        ))
    }

    fn run_with_input(&self, program: &str, args: &[&str], input: &str) -> Result<CmdOutput> {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(io_error(program))?;
        // A program that exits without reading all its input makes the
        // write fail (EPIPE) or not, depending on timing: its exit status
        // is what tells whether it did its job, so the write's own result
        // is left out. Dropping the pipe here is the end of input.
        let _ = child
            .stdin
            .take()
            .map(|mut stdin| stdin.write_all(input.as_bytes()));
        child
            .wait_with_output()
            .map(convert)
            .map_err(io_error(program))
    }
}

/// Prints mutating commands instead of executing them and records them for
/// later inspection; they "succeed" with empty output.
///
/// Read-only queries (`zfs list`, `ifconfig <iface>`, `kldstat`, ...) are
/// executed for real so that existence checks reflect the host; if the tool
/// is not available (e.g. on a development machine) the query fails, which
/// callers treat as "does not exist".
#[derive(Debug, Default)]
pub struct DryRunner {
    commands: Mutex<Vec<String>>,
}

/// Lock a recording mutex. A poisoned lock only means another thread
/// panicked while holding it; the `Vec` inside is still usable, so recover
/// rather than propagate the panic.
fn lock_log(m: &Mutex<Vec<String>>) -> MutexGuard<'_, Vec<String>> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Commands that only inspect state and are safe to run during a dry run.
fn is_query(program: &str, args: &[&str]) -> bool {
    match program {
        "zfs" | "zpool" => matches!(args.first(), Some(&"list") | Some(&"get")),
        "kldstat" | "sysctl" | "pciconf" | "uname" | "freebsd-version" => true,
        // `kenv name=value` sets and `kenv -u name` unsets a variable.
        "kenv" => !args.iter().any(|a| *a == "-u" || a.contains('=')),
        "gpart" => args.first() == Some(&"show"),
        "acpidump" => args == ["-t"],
        "mount" => args == ["-p"],
        "ifconfig" => args.len() == 1,
        "route" => args.contains(&"get"),
        "sysrc" => args.contains(&"-n"),
        // Signal 0 only asks whether the process exists.
        "kill" => args.first() == Some(&"-0"),
        _ => false,
    }
}

impl DryRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// All commands recorded so far, in execution order.
    pub fn commands(&self) -> Vec<String> {
        lock_log(&self.commands).clone()
    }
}

impl Runner for DryRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CmdOutput> {
        if is_query(program, args) {
            return Ok(RealRunner
                .run(program, args)
                .unwrap_or_else(|e| CmdOutput::failed(127, e.to_string())));
        }
        let line = shell_join(program, args);
        println!("+ {line}");
        lock_log(&self.commands).push(line);
        Ok(CmdOutput::ok(""))
    }

    fn run_with_input(&self, program: &str, args: &[&str], _input: &str) -> Result<CmdOutput> {
        let line = format!("{} <<< (stdin hidden)", shell_join(program, args));
        println!("+ {line}");
        lock_log(&self.commands).push(line);
        Ok(CmdOutput::ok(""))
    }
}

/// Test runner with canned responses.
///
/// Responses are keyed by the program name plus an argument *prefix*; the
/// longest matching prefix wins. Unmatched commands succeed with empty output.
#[derive(Debug, Default)]
pub struct MockRunner {
    responses: HashMap<String, Vec<(Vec<String>, CmdOutput)>>,
    /// Programs that cannot be started at all (not installed).
    missing: Vec<String>,
    calls: Mutex<Vec<String>>,
}

impl MockRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a canned response for `program` when its arguments start
    /// with `args_prefix`.
    pub fn respond(mut self, program: &str, args_prefix: &[&str], output: CmdOutput) -> Self {
        self.responses
            .entry(program.to_string())
            .or_default()
            .push((args_prefix.iter().map(|s| s.to_string()).collect(), output));
        self
    }

    /// Make `program` fail to start, like a tool that is not installed:
    /// [`Runner::run`] returns an error instead of an exit status.
    pub fn missing(mut self, program: &str) -> Self {
        self.missing.push(program.to_string());
        self
    }

    /// Every command executed, as shell-joined strings.
    pub fn calls(&self) -> Vec<String> {
        lock_log(&self.calls).clone()
    }
}

impl Runner for MockRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CmdOutput> {
        lock_log(&self.calls).push(shell_join(program, args));
        if self.missing.iter().any(|m| m == program) {
            return Err(io_error(program)(std::io::ErrorKind::NotFound.into()));
        }

        let best = self
            .responses
            .get(program)
            .into_iter()
            .flatten()
            .filter(|(prefix, _)| {
                prefix.len() <= args.len() && prefix.iter().zip(args).all(|(p, a)| p == a)
            })
            .max_by_key(|(prefix, _)| prefix.len());

        Ok(best
            .map(|(_, out)| out.clone())
            .unwrap_or_else(|| CmdOutput::ok("")))
    }

    fn run_with_input(&self, program: &str, args: &[&str], _input: &str) -> Result<CmdOutput> {
        self.run(program, args)
    }
}

/// Render a command line with POSIX-shell-safe quoting.
pub fn shell_join(program: &str, args: &[&str]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(shell_quote(program));
    parts.extend(args.iter().map(|a| shell_quote(a)));
    parts.join(" ")
}

/// Quote `s` for a POSIX shell (unchanged when it needs no quoting).
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@+%".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_join_quotes_when_needed() {
        assert_eq!(shell_join("zfs", &["list", "-H"]), "zfs list -H");
        assert_eq!(
            shell_join("echo", &["hello world", "it's"]),
            "echo 'hello world' 'it'\\''s'"
        );
        assert_eq!(shell_join("x", &[""]), "x ''");
    }

    #[test]
    fn mock_runner_prefers_longest_prefix() {
        let m = MockRunner::new()
            .respond("zfs", &["list"], CmdOutput::ok("generic"))
            .respond("zfs", &["list", "-H"], CmdOutput::ok("specific"));
        assert_eq!(
            m.run("zfs", &["list", "-H", "x"]).unwrap().stdout,
            "specific"
        );
        assert_eq!(m.run("zfs", &["list"]).unwrap().stdout, "generic");
        assert_eq!(m.run("zfs", &["get"]).unwrap().stdout, "");
        assert_eq!(m.calls().len(), 3);
    }

    #[test]
    fn run_checked_fails_on_nonzero() {
        let m = MockRunner::new().respond("zfs", &[], CmdOutput::failed(1, "boom"));
        let err = m.run_checked("zfs", &["destroy"]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "command `zfs destroy` failed (exit code Some(1)): boom"
        );
        let err = m
            .run_with_input_checked("zfs", &["destroy"], "")
            .unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_inherits_stdin_and_reports_status() {
        // `sh` is everywhere on unix; the point is that stdin is not
        // /dev/null, which `[ -t 0 ]` cannot tell under cargo test, so
        // check the plumbing (status, captured output) instead.
        let out = RealRunner
            .run_inheriting_stdin("sh", &["-c", "echo via-tty; exit 3"])
            .unwrap();
        assert_eq!(out.status, Some(3));
        assert_eq!(out.stdout.trim(), "via-tty");
        assert!(MockRunner::new()
            .run_inheriting_stdin("stty", &["-echo"])
            .is_ok());
    }

    #[test]
    fn missing_programs_are_named_and_stay_not_found() {
        let r = RealRunner;
        let errors = [
            r.run("jos-no-such-tool", &[]),
            r.run_inheriting_stdin("jos-no-such-tool", &[]),
            r.run_with_input("jos-no-such-tool", &[], "x"),
        ];
        for e in errors {
            let e = e.unwrap_err();
            let kind = std::error::Error::source(&e)
                .and_then(|s| s.downcast_ref::<std::io::Error>())
                .map(std::io::Error::kind);
            assert_eq!(kind, Some(std::io::ErrorKind::NotFound), "{e}");
            assert!(e.to_string().contains("running jos-no-such-tool: "), "{e}");
        }
    }

    #[test]
    fn dry_runner_records_mutations_only() {
        let d = DryRunner::new();
        d.run("bhyve", &["-c", "2"]).unwrap();
        // A query: executed (and failing on machines without zfs) but never
        // recorded as a would-be mutation.
        let _ = d
            .run("zfs", &["list", "-H", "nonexistent/dataset"])
            .unwrap();
        assert_eq!(d.commands(), vec!["bhyve -c 2".to_string()]);
    }

    #[test]
    fn query_classification() {
        assert!(is_query("zfs", &["list", "-H"]));
        assert!(!is_query("zfs", &["create", "-p", "x"]));
        assert!(is_query("ifconfig", &["tap-work"]));
        assert!(!is_query("ifconfig", &["tap-work", "destroy"]));
        assert!(is_query("kldstat", &[]));
        assert!(!is_query("bhyve", &["vm"]));
        assert!(is_query("route", &["-n", "get", "default"]));
        assert!(!is_query("route", &["add", "default", "10.0.0.1"]));
        assert!(is_query(
            "sysrc",
            &["-f", "/boot/loader.conf", "-n", "pptdevs"]
        ));
        assert!(!is_query(
            "sysrc",
            &["-f", "/boot/loader.conf", "pptdevs=1/0/0"]
        ));
        assert!(is_query("uname", &["-a"]));
        assert!(is_query("freebsd-version", &["-kru"]));
        assert!(is_query("kenv", &[]));
        assert!(is_query("kenv", &["-q", "smbios.system.product"]));
        assert!(!is_query("kenv", &["hw.dmar.enable=1"]));
        assert!(!is_query("kenv", &["-u", "hw.dmar.enable"]));
        assert!(is_query("gpart", &["show"]));
        assert!(is_query("acpidump", &["-t"]));
        assert!(!is_query("acpidump", &["-o", "/tmp/dsdt"]));
        assert!(is_query("kill", &["-0", "42"]));
        assert!(!is_query("kill", &["-TERM", "42"]));
        assert!(!is_query("gpart", &["destroy", "-F", "nda0"]));
        assert!(is_query("mount", &["-p"]));
        assert!(!is_query("mount", &["-t", "msdosfs", "/dev/da1s1", "/mnt"]));
    }
}
