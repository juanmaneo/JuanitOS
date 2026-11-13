// SPDX-License-Identifier: BSD-3-Clause
//! The first-boot wizard's event loop: keys and mouse packets from the
//! display go to the [`Wizard`], frames go back to the display, and
//! `jos setup` runs as a child process on a background thread whose
//! output lines come back over a channel, so the screen stays alive while
//! the templates are built.
//!
//! The page is rendered only when the wizard changed; the pointer is
//! composited onto a copy of that frame, so moving the mouse is cheap.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use jos_fb::keys::Decoder;
use jos_fb::mouse::Pointer;
use jos_fb::Display;
use juanitos_core::ifaces;
use juanitos_core::pci::PciDevice;
use juanitos_core::sys::Runner;

use crate::ui;
use crate::wizard::{Action, Outcome, Wizard};

/// The program the wizard runs `setup` with, and the arguments that go
/// before the wizard's (`--dry-run`).
#[derive(Debug, Clone)]
pub struct SetupCmd {
    pub program: String,
    pub prefix: Vec<String>,
}

enum Event {
    Line(String),
    Finished(Result<(), String>),
}

/// Loop knobs; tests shorten the tick and bound the number of iterations.
#[derive(Debug, Clone, Copy)]
pub struct LoopOptions {
    pub tick: Duration,
    pub max_ticks: Option<usize>,
}

impl Default for LoopOptions {
    fn default() -> Self {
        LoopOptions {
            tick: Duration::from_millis(30),
            max_ticks: None,
        }
    }
}

/// Run the wizard on `display` until the user leaves it. `devices` name
/// the PCI network cards when the list is scanned again.
pub fn run<D: Display>(
    display: &mut D,
    wizard: &mut Wizard,
    runner: &dyn Runner,
    devices: &[PciDevice],
    setup: &SetupCmd,
    opts: LoopOptions,
) -> anyhow::Result<Outcome> {
    let (width, height) = display.size();
    let mut decoder = Decoder::new();
    let mut pointer = Pointer::new(width, height);
    let (tx, rx) = mpsc::channel::<Event>();

    std::thread::scope(|scope| {
        // The page changed (re-render) / the pointer moved or the kernel
        // painted over the frame (re-present).
        let mut dirty = true;
        let mut moved = false;
        let (mut frame, mut hits) = ui::render_page(wizard, width, height);
        let mut ticks = 0usize;
        loop {
            if dirty {
                (frame, hits) = ui::render_page(wizard, width, height);
            }
            moved |= display.needs_redraw();
            if dirty || moved {
                display.present(&pointer.composite(&frame))?;
                dirty = false;
                moved = false;
            }
            if opts.max_ticks.is_some_and(|max| ticks >= max) {
                anyhow::bail!("first-boot UI stopped after {ticks} ticks");
            }
            ticks += 1;

            let mut actions = Vec::new();
            let mut keys = Vec::new();
            while let Some(b) = display.poll_key() {
                keys.extend(decoder.feed(b));
            }
            keys.extend(decoder.flush());
            for key in keys {
                dirty = true;
                actions.push(wizard.handle(key));
            }
            for m in display.poll_mouse() {
                moved = true;
                if pointer.update(m, width, height) {
                    if let Some(target) = hits.target(pointer.x, pointer.y) {
                        dirty = true;
                        actions.push(wizard.click(target));
                    }
                }
            }
            for action in actions {
                match action {
                    Action::None => {}
                    // A USB adapter plugged in since: its driver may not
                    // be loaded yet.
                    Action::Rescan => {
                        ifaces::load_usb_drivers(runner);
                        wizard.set_ifaces(ifaces::list(runner, devices));
                    }
                    Action::Leave(outcome) => return Ok(outcome),
                    Action::Start(args) => {
                        let tx = tx.clone();
                        scope.spawn(move || {
                            let result = run_setup(setup, &args, &mut |line| {
                                let _ = tx.send(Event::Line(line));
                            });
                            let _ = tx.send(Event::Finished(result));
                        });
                        // Input that came after Create must not land on
                        // the Done page.
                        break;
                    }
                }
            }

            while let Ok(event) = rx.try_recv() {
                dirty = true;
                match event {
                    // The whole output also goes to the log (rc.d).
                    Event::Line(line) => {
                        println!("{line}");
                        wizard.on_line(&line);
                    }
                    Event::Finished(r) => wizard.finished(r),
                }
            }

            if !dirty && !moved {
                std::thread::sleep(opts.tick);
            }
        }
    })
}

/// Run `setup.program setup.prefix.. args..`, handing each line of its
/// standard output to `on_line`. A failure is the last lines of its
/// standard error (which also goes to ours), else its exit status.
pub fn run_setup(
    setup: &SetupCmd,
    args: &[String],
    on_line: &mut dyn FnMut(String),
) -> Result<(), String> {
    let mut child = Command::new(&setup.program)
        .args(&setup.prefix)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", setup.program))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    std::thread::scope(|scope| {
        // Drained on its own thread: a full stderr pipe would block the
        // child while stdout is read.
        let errors = scope.spawn(move || {
            stderr
                .map(|mut e| {
                    let mut text = String::new();
                    let _ = e.read_to_string(&mut text);
                    text
                })
                .unwrap_or_default()
        });
        let lines = stdout
            .into_iter()
            .flat_map(|out| BufReader::new(out).lines().map_while(Result::ok));
        for line in lines {
            on_line(line);
        }
        let status = child.wait().ok();
        let errors = errors.join().unwrap_or_default();
        eprint!("{errors}");
        if status.is_some_and(|s| s.success()) {
            return Ok(());
        }
        let status = status.map(|s| s.to_string()).unwrap_or_default();
        Err(failure(&errors, &status))
    })
}

/// The last lines of `stderr`, or the exit status when it said nothing.
fn failure(stderr: &str, status: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return format!("jos setup ended with {status}");
    }
    let tail: Vec<&str> = lines.iter().rev().take(4).rev().copied().collect();
    tail.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::sample_wizard;
    use crate::wizard::{Page, RunState};
    use jos_fb::mouse::Mouse;
    use jos_fb::NullDisplay;
    use juanitos_core::sys::{CmdOutput, MockRunner};
    use std::collections::VecDeque;

    /// One burst of scripted input: bytes typed, or mouse packets.
    #[derive(Clone)]
    enum Burst {
        Keys(Vec<u8>),
        Mouse(Vec<Mouse>),
    }

    /// A display that releases one scripted burst of input at a time, and
    /// only after the previous one has been consumed and a few idle ticks
    /// have passed (so `jos setup` gets to finish).
    struct Scripted {
        inner: NullDisplay,
        bursts: VecDeque<Burst>,
        idle: usize,
    }

    impl Display for Scripted {
        fn size(&self) -> (u32, u32) {
            self.inner.size()
        }
        fn present(&mut self, frame: &jos_gfx::Image) -> anyhow::Result<()> {
            self.inner.present(frame)
        }
        fn restore_console(&mut self) -> anyhow::Result<()> {
            self.inner.restore_console()
        }
        fn poll_key(&mut self) -> Option<u8> {
            if let Some(b) = self.inner.poll_key() {
                return Some(b);
            }
            self.idle += 1;
            if self.idle > 200 {
                match self.bursts.pop_front() {
                    Some(Burst::Keys(bytes)) => self.inner.type_bytes(&bytes),
                    Some(Burst::Mouse(packets)) => self.inner.mouse.push_back(packets),
                    None => return None,
                }
                self.idle = 0;
            }
            None
        }
        fn poll_mouse(&mut self) -> Vec<Mouse> {
            self.inner.poll_mouse()
        }
        fn needs_redraw(&mut self) -> bool {
            self.inner.needs_redraw()
        }
    }

    fn opts() -> LoopOptions {
        LoopOptions {
            tick: Duration::from_millis(1),
            max_ticks: Some(5000),
        }
    }

    fn script(bursts: &[&[u8]]) -> Scripted {
        Scripted {
            inner: NullDisplay::new(640, 400),
            bursts: bursts.iter().map(|b| Burst::Keys(b.to_vec())).collect(),
            idle: 0,
        }
    }

    /// `sh -c SCRIPT sh setup ...`: the wizard's arguments are `$@`.
    fn shell(script: &str) -> SetupCmd {
        SetupCmd {
            program: "sh".into(),
            prefix: vec!["-c".into(), script.into(), "sh".into()],
        }
    }

    fn mouse(dx: i32, dy: i32, left: bool) -> Mouse {
        Mouse { dx, dy, left }
    }

    // Welcome -> Network -> Qubes -> Summary -> Create qubes.
    const TO_SETUP: &[u8] = b"\r\r\r\r";

    #[test]
    fn the_keyboard_creates_the_qubes_then_reboots() {
        let mut d = script(&[TO_SETUP, b"\r"]);
        let mut w = sample_wizard();
        let m = MockRunner::new();
        let setup = shell(
            "echo \"$@\" > /dev/null; echo '==> [1/2] create sys-net'; \
             echo; echo '==> [2/2] start sys-net'; echo 'reboot to hand the devices'; \
             echo 'note' >&2",
        );
        let out = run(&mut d, &mut w, &m, &[], &setup, opts()).unwrap();
        assert_eq!(out, Outcome::Reboot);
        assert_eq!(w.page, Page::Done);
        assert_eq!(w.run, RunState::Succeeded);
        assert_eq!(w.log, ["create sys-net", "start sys-net"]);
        assert!(m.calls().is_empty(), "nothing run through the runner");
    }

    #[test]
    fn a_failed_setup_shows_its_error_and_can_be_left() {
        let mut d = script(&[TO_SETUP, b"\x1b[Z\r"]);
        let mut w = sample_wizard();
        let m = MockRunner::new();
        let setup = shell("echo 'jos: sys-net: no image' >&2; exit 3");
        let out = run(&mut d, &mut w, &m, &[], &setup, opts()).unwrap();
        assert_eq!(out, Outcome::Later);
        assert_eq!(w.run, RunState::Failed("jos: sys-net: no image".into()));
    }

    #[test]
    fn rescan_lists_the_cards_again_then_skip() {
        // Network, Shift+Tab twice to Rescan, Enter; Esc; Skip.
        let mut d = script(&[b"\r\x1b[Z\x1b[Z\r", b"\x1b", b"\x1b[Z\r"]);
        let mut w = sample_wizard();
        let m = MockRunner::new().respond("ifconfig", &["-a"], CmdOutput::failed(1, ""));
        let out = run(&mut d, &mut w, &m, &[], &shell("exit 0"), opts()).unwrap();
        assert_eq!(out, Outcome::Done);
        assert!(w.ifaces.is_empty(), "the cards went away");
        let calls = m.calls();
        assert!(
            calls.iter().any(|c| c.starts_with("kldload -n")),
            "{calls:?}"
        );
        assert_eq!(calls.last().map(String::as_str), Some("ifconfig -a"));
    }

    #[test]
    fn the_mouse_clicks_buttons() {
        // At 1280x800 the Welcome page's Next button is centred on
        // (1120, 738), Skip on (904, 738).
        let bursts = [
            // Nothing at the top left; then move onto Next and press.
            Burst::Mouse(vec![
                mouse(-2000, -2000, true),
                mouse(0, 0, false),
                mouse(1120, 738, false),
                mouse(0, 0, true),
                mouse(0, 0, false),
            ]),
            // Back to Welcome, then Skip.
            Burst::Mouse(vec![mouse(-216, 0, true), mouse(0, 0, false)]),
            Burst::Mouse(vec![mouse(0, 0, true)]),
        ];
        let mut d = Scripted {
            inner: NullDisplay::new(1280, 800),
            bursts: bursts.into_iter().collect(),
            idle: 0,
        };
        // The kernel painted over the frame while idle: presented again.
        d.inner.redraws.extend([false, true]);
        let mut w = sample_wizard();
        let m = MockRunner::new();
        let out = run(&mut d, &mut w, &m, &[], &shell("exit 0"), opts()).unwrap();
        assert_eq!(out, Outcome::Done);
        assert_eq!(w.page, Page::Welcome);
    }

    #[test]
    fn the_loop_stops_at_its_tick_limit_or_a_dead_display() {
        let m = MockRunner::new();
        let mut d = script(&[]);
        let err = run(
            &mut d,
            &mut sample_wizard(),
            &m,
            &[],
            &shell("exit 0"),
            LoopOptions {
                tick: Duration::from_millis(1),
                max_ticks: Some(300),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("300 ticks"));
        assert_eq!(LoopOptions::default().max_ticks, None);
        d.restore_console().unwrap();
        assert!(d.inner.restored);

        let mut d = script(&[]);
        d.inner.fail_after_frames = Some(0);
        let err = run(
            &mut d,
            &mut sample_wizard(),
            &m,
            &[],
            &shell("exit 0"),
            opts(),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "framebuffer gone");
    }

    fn ignore(_: String) {}

    #[test]
    fn setup_runs_as_a_child_process() {
        let mut lines = Vec::new();
        let args = ["setup".to_string(), "--app".to_string(), "work".to_string()];
        let ok = run_setup(&shell("echo \"$@\"; echo done"), &args, &mut |l| {
            lines.push(l)
        });
        assert_eq!(ok, Ok(()));
        assert_eq!(lines, ["setup --app work", "done"]);
        // The last lines of stderr, else the status.
        let err = run_setup(
            &shell("echo out; for i in 1 2 3 4 5; do echo \"e$i\" >&2; done; exit 1"),
            &[],
            &mut ignore,
        );
        assert_eq!(err, Err("e2 e3 e4 e5".into()));
        let err = run_setup(&shell("exit 7"), &[], &mut ignore);
        assert_eq!(err, Err("jos setup ended with exit status: 7".into()));
        let missing = SetupCmd {
            program: "/nonexistent/jos".into(),
            prefix: Vec::new(),
        };
        let err = run_setup(&missing, &[], &mut ignore).unwrap_err();
        assert!(err.starts_with("cannot run /nonexistent/jos: "), "{err}");
    }
}
