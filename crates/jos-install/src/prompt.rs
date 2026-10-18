//! Line-oriented questions for the text installer, over any reader and
//! writer so they are tested with in-memory input. Both are boxed: one
//! compiled `Prompter` serves the console and the tests alike.

use std::io::{BufRead, Write};

use anyhow::bail;
use juanitos_core::pci::{PciAddr, PciDevice};
use juanitos_core::sys::Runner;

use crate::disks::Disk;

pub struct Prompter<'a> {
    input: Box<dyn BufRead + 'a>,
    out: Box<dyn Write + 'a>,
    /// Used for `stty -echo` around passwords.
    runner: &'a dyn Runner,
}

impl<'a> Prompter<'a> {
    pub fn new(input: impl BufRead + 'a, out: impl Write + 'a, runner: &'a dyn Runner) -> Self {
        Prompter {
            input: Box::new(input),
            out: Box::new(out),
            runner,
        }
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

    /// Tick passthrough devices by number. `current` is the default; its
    /// addresses that are not candidates (given with `--pci`) are kept.
    /// Returns the selection sorted.
    pub fn choose_devices(
        &mut self,
        candidates: &[PciDevice],
        current: &[PciAddr],
    ) -> anyhow::Result<Vec<PciAddr>> {
        self.say("PCI devices (reserved ones go to qubes; the dom0 never drives them):\n")?;
        let mut default = Vec::new();
        for (i, d) in candidates.iter().enumerate() {
            let ticked = current.contains(&d.addr);
            if ticked {
                default.push((i + 1).to_string());
            }
            self.say(&format!(
                "  {}) [{}] {:<8} {:<5} {}\n",
                i + 1,
                if ticked { 'x' } else { ' ' },
                d.addr.to_string(),
                d.kind(),
                d.label()
            ))?;
        }
        let default = if default.is_empty() {
            "none".to_string()
        } else {
            default.join(" ")
        };
        let mut out: Vec<PciAddr> = current
            .iter()
            .copied()
            .filter(|a| !candidates.iter().any(|d| d.addr == *a))
            .collect();
        loop {
            let answer = self.prompt_default(
                "Devices to reserve for passthrough (numbers, 'none')",
                &default,
            )?;
            if answer.trim() == "none" {
                break;
            }
            let picked: Option<Vec<PciAddr>> = answer
                .split([' ', ','])
                .filter(|t| !t.is_empty())
                .map(|t| {
                    t.parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|i| candidates.get(i))
                        .map(|d| d.addr)
                })
                .collect();
            match picked {
                Some(p) => {
                    out.extend(p);
                    break;
                }
                None => self.say("  no such device\n")?,
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
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
pub(crate) mod tests {
    use super::*;
    use juanitos_core::sys::MockRunner;

    /// Output whose `n`-th operation (write or flush, 0-based) fails, as
    /// does every one after it.
    struct FailAt {
        n: usize,
        ops: usize,
    }

    impl FailAt {
        fn op(&mut self) -> std::io::Result<()> {
            self.ops += 1;
            if self.ops > self.n {
                return Err(std::io::Error::other("disk full"));
            }
            Ok(())
        }
    }

    impl Write for FailAt {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.op().map(|()| buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.op()
        }
    }

    /// Run the dialogue `f`, which succeeds on `input`, with the output
    /// breaking at each write or flush in turn and with the input ending
    /// after each of its lines: every such run must fail with that error.
    pub(crate) fn fails_on_every_io_error(
        input: &str,
        f: &dyn Fn(&mut Prompter<'_>) -> anyhow::Result<()>,
    ) {
        let m = MockRunner::new();
        let mut n = 0;
        loop {
            let mut p = Prompter::new(input.as_bytes(), FailAt { n, ops: 0 }, &m);
            match f(&mut p) {
                Ok(()) => break,
                Err(e) => assert_eq!(e.to_string(), "disk full", "at operation {n}"),
            }
            n += 1;
        }
        assert!(n > 0, "the dialogue wrote nothing");
        let lines: Vec<&str> = input.split_inclusive('\n').collect();
        for k in 0..lines.len() {
            let cut = lines[..k].concat();
            let mut p = Prompter::new(cut.as_bytes(), std::io::sink(), &m);
            let err = f(&mut p).unwrap_err();
            assert_eq!(err.to_string(), "end of input", "after {k} lines");
        }
    }

    fn with<T>(input: &str, f: impl FnOnce(&mut Prompter<'_>) -> T) -> (T, String, Vec<String>) {
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
    fn unreadable_input_is_an_error() {
        let m = MockRunner::new();
        let mut p = Prompter::new(&b"\xff\n"[..], std::io::sink(), &m);
        let err = p.prompt_default("Name", "").unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[test]
    fn every_question_stops_on_io_errors() {
        let disks = [Disk {
            name: "ada0".into(),
            size_bytes: 1 << 30,
            description: "A".into(),
        }];
        let nics = [PciDevice {
            addr: PciAddr::new(0, 20, 3),
            driver: "none0".into(),
            vendor: 0x8086,
            device: 0x1234,
            class: 0x028000,
            description: None,
        }];
        // Each question answered wrong once (where it can be) first.
        let input = "x\ny\nzz\nada0\n9\n1\n\n\na\nb\npw\npw\n";
        fails_on_every_io_error(input, &|p| {
            p.prompt_default("Name", "dflt")?;
            p.ask_yes_no("Go?", true)?;
            p.choose_disk(&disks)?;
            p.choose_devices(&nics, &[nics[0].addr])?;
            p.password("Root")?;
            Ok(())
        });
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
    fn devices_by_number_keeping_extras() {
        let dev = |addr: PciAddr, vendor: u16, class: u32| PciDevice {
            addr,
            driver: "none0".into(),
            vendor,
            device: 0x1234,
            class,
            description: None,
        };
        let gpu = PciAddr::new(1, 0, 0);
        let audio = PciAddr::new(1, 0, 1);
        let nic = PciAddr::new(0, 20, 3);
        let extra = PciAddr::new(7, 0, 0);
        let devs = vec![
            dev(nic, 0x8086, 0x028000),
            dev(gpu, 0x10de, 0x030000),
            dev(audio, 0x10de, 0x040300),
        ];

        // Enter keeps the default (here: the GPU and its audio).
        let (r, out, _) = with("\n", |p| {
            p.choose_devices(&devs, &[gpu, audio, extra]).unwrap()
        });
        assert_eq!(r, [gpu, audio, extra]);
        assert!(
            out.contains("  1) [ ] 0/20/3   NIC   vendor 0x8086 device 0x1234\n"),
            "{out}"
        );
        assert!(out.contains("  2) [x] 1/0/0    GPU"));
        assert!(out.contains("[2 3]: "));

        // Invalid answers are asked again; commas and spaces both work.
        let (r, out, _) = with("9\nx\n1, 3\n", |p| p.choose_devices(&devs, &[]).unwrap());
        assert_eq!(r, [nic, audio]);
        assert_eq!(out.matches("no such device").count(), 2);
        assert!(out.contains("[none]: "));

        // 'none' drops the candidates but keeps the extras.
        let (r, _, _) = with("none\n", |p| {
            p.choose_devices(&devs, &[gpu, extra]).unwrap()
        });
        assert_eq!(r, [extra]);
        let (r, _, _) = with("\n", |p| p.choose_devices(&devs, &[]).unwrap());
        assert!(r.is_empty());
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
