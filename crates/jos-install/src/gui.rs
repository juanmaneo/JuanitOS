//! The graphical installer's event loop: keys from the display go to the
//! [`Wizard`], frames go back to the display, and the installation itself
//! runs on a background thread that reports [`Progress`] over a channel so
//! the screen stays alive while the dom0 is extracted.

// Only the FreeBSD build has a framebuffer to run this on; the loop is
// still compiled and tested everywhere.
#![cfg_attr(not(target_os = "freebsd"), allow(dead_code))]

use std::sync::mpsc;
use std::time::Duration;

use jos_fb::keys::Decoder;
use jos_fb::Display;
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
    let (tx, rx) = mpsc::channel::<Event>();

    std::thread::scope(|scope| {
        let mut dirty = true;
        let mut ticks = 0usize;
        loop {
            if dirty {
                display.present(&ui::render(wizard, width, height))?;
                dirty = false;
            }
            if opts.max_ticks.is_some_and(|max| ticks >= max) {
                anyhow::bail!("installer UI stopped after {ticks} ticks");
            }
            ticks += 1;

            // Keyboard: everything typed since the last tick.
            let mut keys = Vec::new();
            while let Some(b) = display.poll_key() {
                keys.extend(decoder.feed(b));
            }
            keys.extend(decoder.flush());
            for key in keys {
                dirty = true;
                match wizard.handle(key) {
                    Action::None => {}
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
                        // Keys typed ahead must not land on the Finish
                        // page (an Enter there reboots).
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

            if !dirty {
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
    use jos_fb::NullDisplay;
    use juanitos_core::sys::{CmdOutput, MockRunner};
    use std::collections::VecDeque;

    /// A display that releases one scripted burst of input at a time, and
    /// only after the previous one has been consumed and a few idle ticks
    /// have passed (so the background installer gets to finish).
    struct Scripted {
        inner: NullDisplay,
        bursts: VecDeque<Vec<u8>>,
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
                if let Some(burst) = self.bursts.pop_front() {
                    self.idle = 0;
                    self.inner.type_bytes(&burst);
                }
            }
            None
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
            bursts: bursts.iter().map(|b| b.to_vec()).collect(),
            idle: 0,
        }
    }

    // Welcome -> ... -> Summary, ticking the confirmation, then Install.
    const TO_INSTALL: &[u8] = b"\r\r\r\t\t\t\t\r\rsecret\rsecret\r\t\t\t\t\r \t\t\r";

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
    fn failed_install_exits_to_console() {
        let mut d = script(&[TO_INSTALL, b"\r"]);
        let mut w = sample_wizard();
        let m = MockRunner::new().respond("gpart", &["create"], CmdOutput::failed(1, "no disk"));
        let out = run(&mut d, &mut w, &m, true, "", opts()).unwrap();
        assert_eq!(out, Outcome::Exit);
        assert_eq!(w.page, Page::Finish);
        assert!(matches!(&w.install, InstallState::Failed(e) if e.contains("no disk")));
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

        let mut d = script(&[]);
        let err = run(
            &mut d,
            &mut sample_wizard(),
            &m,
            true,
            "",
            LoopOptions {
                tick: Duration::from_millis(1),
                max_ticks: Some(3),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("3 ticks"));
        assert_eq!(LoopOptions::default().max_ticks, None);
    }
}
