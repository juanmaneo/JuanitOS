// SPDX-License-Identifier: BSD-3-Clause
//! The graphical installer's event loop: keys and mouse packets from the
//! display go to the [`Wizard`], frames go back to the display, and the
//! installation itself runs on a background thread that reports
//! [`Progress`] over a channel so the screen stays alive while the dom0 is
//! extracted.
//!
//! The page is rendered only when the wizard changed; the pointer is
//! composited onto a copy of that frame, so moving the mouse is cheap.

// Only the FreeBSD build has a framebuffer to run this on; the loop is
// still compiled and tested everywhere.
#![cfg_attr(not(target_os = "freebsd"), allow(dead_code))]

use std::sync::mpsc;
use std::time::Duration;

use jos_fb::keys::Decoder;
use jos_fb::mouse::Pointer;
use jos_fb::Display;
use juanitos_core::keyboard;
use juanitos_core::sys::Runner;

use crate::install::{Installer, Progress};
use crate::ui;
use crate::wizard::{Action, Wizard};

/// How the user left the installer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Exit,
    Reboot,
}

enum Event {
    Progress(Progress),
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

/// Run the installer UI on `display` until the user exits or reboots.
pub fn run<D: Display>(
    display: &mut D,
    wizard: &mut Wizard,
    runner: &dyn Runner,
    dry_run: bool,
    doctor_report: &str,
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
                anyhow::bail!("installer UI stopped after {ticks} ticks");
            }
            ticks += 1;

            // Keyboard and mouse: everything since the last tick, in
            // order within each device.
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
                    // The keyboard is shared by every terminal, so loading
                    // the keymap on the console changes what this one reads.
                    // A failure leaves the keymap as it was; the test field
                    // shows that too.
                    Action::Keymap(name) => {
                        if let Err(e) = keyboard::load(runner, &name) {
                            eprintln!("jos-install: cannot load keymap {name}: {e}");
                        }
                    }
                    Action::Exit => return Ok(Outcome::Exit),
                    Action::Reboot => return Ok(Outcome::Reboot),
                    Action::Start(plan) => {
                        let tx = tx.clone();
                        scope.spawn(move || {
                            let installer = Installer::new(runner, dry_run, &plan);
                            let result = installer
                                .run_with(doctor_report, &mut |p| {
                                    let _ = tx.send(Event::Progress(p));
                                })
                                .map_err(|e| e.to_string());
                            let _ = tx.send(Event::Finished(result));
                        });
                        // Input that came after Install must not land on
                        // the Finish page (an Enter there reboots).
                        break;
                    }
                }
            }

            // Installer progress.
            while let Ok(event) = rx.try_recv() {
                dirty = true;
                match event {
                    Event::Progress(p) => wizard.on_progress(p),
                    Event::Finished(r) => wizard.finished(r),
                }
            }

            if !dirty && !moved {
                std::thread::sleep(opts.tick);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::sample_wizard;
    use crate::wizard::{InstallState, Page};
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
    /// have passed (so the background installer gets to finish).
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

    fn mouse(dx: i32, dy: i32, left: bool) -> Mouse {
        Mouse { dx, dy, left }
    }

    #[test]
    fn the_mouse_clicks_buttons() {
        // At 1280x800 the logical canvas is the screen: the Welcome page's
        // Next button is centred on (1120, 738), Quit and Back on (904, 738).
        let bursts = [
            // Nothing at the top left; then press on Next.
            Burst::Mouse(vec![
                mouse(-2000, -2000, true),
                mouse(0, 0, false),
                mouse(1120, 738, false),
                mouse(0, 0, true),
                mouse(0, 0, true),
                mouse(0, 0, false),
            ]),
            // Back to Welcome (moving and pressing in one packet).
            Burst::Mouse(vec![mouse(-216, 0, true), mouse(0, 0, false)]),
            // Quit.
            Burst::Mouse(vec![mouse(0, 0, true)]),
        ];
        let mut d = Scripted {
            inner: NullDisplay::new(1280, 800),
            bursts: bursts.into_iter().collect(),
            idle: 0,
        };
        let mut w = sample_wizard();
        let m = MockRunner::new();
        // The kernel painted over the frame twice while idle (the user
        // went to the console and back): the frame is presented again.
        d.inner.redraws.extend([false, true, false, true]);
        assert_eq!(
            run(&mut d, &mut w, &m, true, "", opts()).unwrap(),
            Outcome::Exit
        );
        assert_eq!(w.page, Page::Welcome);
        // The first frame, two repaints, then one per burst: Quit leaves
        // before drawing.
        assert_eq!(d.inner.presented, 5);
    }

    // Welcome -> Keyboard (the list, the test field) -> ... -> Summary,
    // ticking the confirmation, then Install.
    const TO_INSTALL: &[u8] = b"\r\r\r\r\r\r\t\t\t\t\r\rsecret\rsecret\r\t\t\t\t\t\r \t\t\r";

    #[test]
    fn full_install_then_reboot() {
        let mut d = script(&[TO_INSTALL, b"\r"]);
        let mut w = sample_wizard();
        let m = MockRunner::new().respond("zfs", &["list"], CmdOutput::failed(1, ""));
        let dir = std::env::temp_dir().join(format!("jos-gui-{}", std::process::id()));
        w = crate::wizard::Wizard::new(
            crate::install::InstallPlan {
                target: dir.to_string_lossy().into_owned(),
                ..Default::default()
            },
            w.checks.clone(),
            w.disks.clone(),
            w.devices.clone(),
            w.keymaps.clone(),
        );
        let out = run(&mut d, &mut w, &m, true, "report", opts()).unwrap();
        assert_eq!(out, Outcome::Reboot);
        assert_eq!(w.install, InstallState::Succeeded);
        assert_eq!(w.log.len(), crate::install::STEP_TITLES.len());
        assert!(d.inner.presented >= 3, "{}", d.inner.presented);
        let calls = m.calls();
        assert!(calls.iter().any(|c| c == "gpart create -s gpt nda0"));
        assert!(calls.iter().all(|c| !c.contains("secret")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alongside_install_through_the_gui() {
        // Down picks the disk with a Linux on it, where keeping it is
        // preselected; Enter goes to the choice, Enter again moves on.
        let mut d = script(&[
            b"\r\r\r\r\x1b[B\r\r\t\t\t\t\r\rsecret\rsecret\r\t\t\t\t\t\r \t\t\r",
            b"\r",
        ]);
        let mut w = sample_wizard();
        let m = MockRunner::new().respond("zfs", &["list"], CmdOutput::failed(1, ""));
        let dir = std::env::temp_dir().join(format!("jos-gui-alongside-{}", std::process::id()));
        w = crate::wizard::Wizard::new(
            crate::install::InstallPlan {
                target: dir.to_string_lossy().into_owned(),
                ..Default::default()
            },
            w.checks.clone(),
            w.disks.clone(),
            w.devices.clone(),
            w.keymaps.clone(),
        );
        let out = run(&mut d, &mut w, &m, true, "report", opts()).unwrap();
        assert_eq!(out, Outcome::Reboot);
        assert_eq!(w.install, InstallState::Succeeded);
        let calls = m.calls();
        assert!(
            calls.iter().all(|c| !c.starts_with("gpart create")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c == "gpart add -a 1m -b 921901m -s 4096m -t freebsd-swap -l swap0 ada0"),
            "{calls:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manual_install_through_the_gui() {
        let mut d = script(&[
            // Past the keyboard, then the Linux disk, partitioned by hand.
            b"\r\r\r\r\x1b[B\r\x1b[B\x1b[B\r",
            // Delete the Linux root, add swap in its place (the type
            // suggested is swap), then the pool in the rest.
            b"\x1b[B\r\r",
            b"\r",
            b"\x1b[Z\x1b[B\r\r\r\r",
            // On through System (no swap field) and the other pages.
            b"\t\t\r\t\t\t\r\rsecret\rsecret\r\t\t\t\t\t\r \t\t\r",
            b"\r",
        ]);
        let mut w = sample_wizard();
        let m = MockRunner::new().respond("zfs", &["list"], CmdOutput::failed(1, ""));
        let dir = std::env::temp_dir().join(format!("jos-gui-manual-{}", std::process::id()));
        w = crate::wizard::Wizard::new(
            crate::install::InstallPlan {
                target: dir.to_string_lossy().into_owned(),
                ..Default::default()
            },
            w.checks.clone(),
            w.disks.clone(),
            w.devices.clone(),
            w.keymaps.clone(),
        );
        let out = run(&mut d, &mut w, &m, true, "report", opts()).unwrap();
        assert_eq!(out, Outcome::Reboot);
        assert_eq!(w.install, InstallState::Succeeded);
        let calls = m.calls();
        let gpart: Vec<&String> = calls.iter().filter(|c| c.starts_with("gpart")).collect();
        assert_eq!(
            gpart,
            [
                "gpart delete -i 2 ada0",
                "gpart add -a 1m -b 301m -s 4096m -t freebsd-swap -l swap0 ada0",
                "gpart add -a 1m -b 4397m -s 1903331m -t freebsd-zfs -l zfs0 ada0",
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keymaps_are_loaded_as_they_are_picked() {
        // Welcome -> Keyboard; Up picks the layout before the default, a
        // letter jumps to a French one; nothing is installed.
        let mut d = script(&[b"\r\x1b[A", b"f"]);
        let mut w = sample_wizard();
        let m = MockRunner::new();
        let err = run(
            &mut d,
            &mut w,
            &m,
            true,
            "",
            LoopOptions {
                tick: Duration::from_millis(1),
                max_ticks: Some(800),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("800 ticks"));
        assert_eq!(w.page, Page::Keyboard);
        assert_eq!(w.plan().keymap.as_deref(), Some("fr"));
        assert_eq!(m.calls(), ["kbdcontrol -l uk.macbook", "kbdcontrol -l fr"]);
        // Without kbdcontrol the choice stands; only the console says so.
        let mut d = script(&[b"\r\x1b[B"]);
        let mut w = sample_wizard();
        let m = MockRunner::new().missing("kbdcontrol");
        let err = run(
            &mut d,
            &mut w,
            &m,
            true,
            "",
            LoopOptions {
                tick: Duration::from_millis(1),
                max_ticks: Some(600),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("600 ticks"));
        assert_eq!(w.plan().keymap.as_deref(), Some("us.dvorak"));
        assert_eq!(m.calls(), ["kbdcontrol -l us.dvorak"]);
    }

    #[test]
    fn failed_install_exits_to_console() {
        let mut d = script(&[TO_INSTALL, b"\r"]);
        let mut w = sample_wizard();
        let m = MockRunner::new().respond("gpart", &["create"], CmdOutput::failed(1, "no disk"));
        let out = run(&mut d, &mut w, &m, true, "", opts()).unwrap();
        assert_eq!(out, Outcome::Exit);
        assert_eq!(w.page, Page::Finish);
        let failed = InstallState::Failed(
            "command `gpart create -s gpt nda0` failed (exit code Some(1)): no disk".into(),
        );
        assert_eq!(w.install, failed);
    }

    #[test]
    fn quit_from_welcome_and_tick_limit() {
        let mut d = script(&[b"\x1b[D\r"]);
        let mut w = sample_wizard();
        let m = MockRunner::new();
        assert_eq!(
            run(&mut d, &mut w, &m, true, "", opts()).unwrap(),
            Outcome::Exit
        );
        assert!(m.calls().is_empty(), "nothing touched");

        // Long idle once the script has run out: the loop only stops at
        // its tick limit.
        let mut d = script(&[]);
        let err = run(
            &mut d,
            &mut sample_wizard(),
            &m,
            true,
            "",
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
    }

    #[test]
    fn a_display_that_fails_ends_the_ui() {
        let mut d = script(&[]);
        d.inner.fail_after_frames = Some(0);
        let m = MockRunner::new();
        let err = run(&mut d, &mut sample_wizard(), &m, true, "", opts()).unwrap_err();
        assert_eq!(err.to_string(), "framebuffer gone");
        assert_eq!(d.inner.presented, 0);
    }
}
