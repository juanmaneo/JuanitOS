//! Line-oriented questions for the text installer, over any reader and
//! writer so they are tested with in-memory input.

use std::io::{BufRead, Write};

use anyhow::bail;
use juanitos_core::sys::Runner;

use crate::disks::Disk;

pub struct Prompter<'a, R, W> {
    input: R,
    out: W,
    /// Used for `stty -echo` around passwords.
    runner: &'a dyn Runner,
}

impl<'a, R: BufRead, W: Write> Prompter<'a, R, W> {
    pub fn new(input: R, out: W, runner: &'a dyn Runner) -> Self {
        Prompter { input, out, runner }
    }

    pub fn say(&mut self, text: &str) -> anyhow::Result<()> {
        self.out.write_all(text.as_bytes())?;
        self.out.flush()?;
        Ok(())
    }

    fn read_line(&mut self) -> anyhow::Result<String> {
        self.out.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            bail!("end of input");
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    }

    pub fn prompt_default(&mut self, question: &str, default: &str) -> anyhow::Result<String> {
        if default.is_empty() {
            self.say(&format!("{question}: "))?;
        } else {
            self.say(&format!("{question} [{default}]: "))?;
        }
        let answer = self.read_line()?;
        Ok(if answer.is_empty() {
            default.to_string()
        } else {
            answer
        })
    }

    pub fn ask_yes_no(&mut self, question: &str, default: bool) -> anyhow::Result<bool> {
        let hint = if default { "Y/n" } else { "y/N" };
        loop {
            self.say(&format!("{question} [{hint}] "))?;
            match self.read_line()?.to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => {}
            }
        }
    }

    pub fn choose_disk(&mut self, disks: &[Disk]) -> anyhow::Result<String> {
        if disks.is_empty() {
            bail!("no disks found");
        }
        self.say("Disks:\n")?;
        for (i, d) in disks.iter().enumerate() {
            self.say(&format!(
                "  {}) {:<8} {:>8.1} GiB  {}\n",
                i + 1,
                d.name,
                d.size_gib(),
                d.description
            ))?;
        }
        loop {
            let answer = self.prompt_default("Install to disk (number or name)", "")?;
            if let Some(d) = answer
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| disks.get(i))
            {
                return Ok(d.name.clone());
            }
            if disks.iter().any(|d| d.name == answer) {
                return Ok(answer);
            }
            self.say("  no such disk\n")?;
        }
    }

    /// Read a password twice with echo off (via `stty`, so no libc
    /// dependency). `stty` must see the installer's own standard input:
    /// with the usual captured run its stdin is `/dev/null` and the
    /// password shows up in clear on the console.
    pub fn password(&mut self, label: &str) -> anyhow::Result<String> {
        loop {
            self.say(&format!("{label}: "))?;
            let _ = self.runner.run_inheriting_stdin("stty", &["-echo"]);
            let first = self.read_line();
            self.say("\nRepeat: ")?;
            let second = self.read_line();
            let _ = self.runner.run_inheriting_stdin("stty", &["echo"]);
            self.say("\n")?;
            let (a, b) = (first?, second?);
            if a.is_empty() {
                self.say("  password must not be empty\n")?;
            } else if a != b {
                self.say("  passwords do not match\n")?;
            } else {
                return Ok(a);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use juanitos_core::sys::MockRunner;

    fn with<T>(
        input: &str,
        f: impl FnOnce(&mut Prompter<'_, &[u8], &mut Vec<u8>>) -> T,
    ) -> (T, String, Vec<String>) {
        let m = MockRunner::new();
        let mut out = Vec::new();
        let r = {
            let mut p = Prompter::new(input.as_bytes(), &mut out, &m);
            f(&mut p)
        };
        (r, String::from_utf8_lossy(&out).into_owned(), m.calls())
    }

    #[test]
    fn defaults_and_answers() {
        let (r, out, _) = with("\nother\n", |p| {
            (
                p.prompt_default("Name", "dflt").unwrap(),
                p.prompt_default("Name", "").unwrap(),
            )
        });
        assert_eq!(r, ("dflt".to_string(), "other".to_string()));
        assert!(out.contains("Name [dflt]: "));
        assert!(out.contains("Name: "));
    }

    #[test]
    fn yes_no_retries_until_understood() {
        let (r, out, _) = with("maybe\nYES\n\nno\n", |p| {
            (
                p.ask_yes_no("Go?", false).unwrap(),
                p.ask_yes_no("Go?", true).unwrap(),
                p.ask_yes_no("Go?", true).unwrap(),
            )
        });
        assert_eq!(r, (true, true, false));
        assert_eq!(out.matches("[y/N]").count(), 2);
    }

    #[test]
    fn end_of_input_is_an_error() {
        let (r, _, _) = with("", |p| p.ask_yes_no("Go?", true));
        assert!(r.unwrap_err().to_string().contains("end of input"));
    }

    #[test]
    fn disk_by_number_or_name() {
        let disks = vec![
            Disk {
                name: "ada0".into(),
                size_bytes: 1 << 30,
                description: "A".into(),
            },
            Disk {
                name: "nda0".into(),
                size_bytes: 2 << 30,
                description: "B".into(),
            },
        ];
        let (r, out, _) = with("7\nzz\n2\n", |p| p.choose_disk(&disks).unwrap());
        assert_eq!(r, "nda0");
        assert_eq!(out.matches("no such disk").count(), 2);
        let (r, _, _) = with("ada0\n", |p| p.choose_disk(&disks).unwrap());
        assert_eq!(r, "ada0");
        let (r, _, _) = with("", |p| p.choose_disk(&[]));
        assert!(r.is_err());
    }

    #[test]
    fn passwords_twice_with_echo_off() {
        let (r, out, calls) = with("\n\na\nb\nok\nok\n", |p| p.password("Root").unwrap());
        assert_eq!(r, "ok");
        assert!(out.contains("must not be empty"));
        assert!(out.contains("do not match"));
        assert_eq!(calls.iter().filter(|c| *c == "stty -echo").count(), 3);
        assert_eq!(calls.iter().filter(|c| *c == "stty echo").count(), 3);
    }
}
