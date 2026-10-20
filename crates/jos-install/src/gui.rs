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
use jos_fb::mouse::Mouse;
use jos_fb::Display;
use jos_gfx::{icons, Image};
use juanitos_core::sys::Runner;

use crate::install::{Installer, Progress};
use crate::ui;
use crate::wizard::{Action, Wizard};

/// Pointer height, as a fraction of the screen height.
const POINTER_SIZE: f32 = 1.0 / 40.0;

/// The mouse pointer: hidden until the mouse first moves, then clamped to
/// the screen. Clicks are the press edge of the left button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pointer {
    pub shown: bool,
    pub x: i32,
    pub y: i32,
    left: bool,
}

impl Pointer {
    /// Starts hidden at the centre of a `width` x `height` screen.
    pub fn new(width: u32, height: u32) -> Self {
        Pointer {
            shown: false,
            x: width as i32 / 2,
            y: height as i32 / 2,
            left: false,
        }
    }

    /// Apply a packet; `true` when the left button was just pressed.
    pub fn update(&mut self, m: Mouse, width: u32, height: u32) -> bool {
        self.shown = true;
        self.x = (self.x + m.dx).clamp(0, width as i32 - 1);
        self.y = (self.y + m.dy).clamp(0, height as i32 - 1);
        let pressed = m.left && !self.left;
        self.left = m.left;
        pressed
    }

    /// `frame` with the pointer on it (the frame itself when hidden).
    pub fn composite(&self, frame: &Image) -> Image {
        let mut img = frame.clone();
        if self.shown {
            let scale = frame.height() as f32 * POINTER_SIZE / 20.0;
            icons::draw_pointer(&mut img, self.x, self.y, scale.max(1.0));
        }
        img
    }
}

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
        // The page changed (re-render) / the pointer moved (re-present).
        let mut dirty = true;
        let mut moved = false;
        let (mut frame, mut hits) = ui::render_page(wizard, width, height);
        let mut ticks = 0usize;
        loop {
            if dirty {
                (frame, hits) = ui::render_page(wizard, width, height);
            }
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
        assert_eq!(
            run(&mut d, &mut w, &m, true, "", opts()).unwrap(),
            Outcome::Exit
        );
        assert_eq!(w.page, Page::Welcome);
        // The first frame, then one per burst: Quit leaves before drawing.
        assert_eq!(d.inner.presented, 3);
    }

    #[test]
    fn the_pointer_is_clamped_and_clicks_on_the_press_edge() {
        let mut p = Pointer::new(100, 50);
        assert_eq!((p.shown, p.x, p.y), (false, 50, 25));
        let frame = jos_gfx::Image::new(100, 50, jos_gfx::Rgba::rgb(9, 9, 9));
        assert_eq!(p.composite(&frame).pixels(), frame.pixels());
        assert!(!p.update(mouse(-500, 10, false), 100, 50));
        assert_eq!((p.shown, p.x, p.y), (true, 0, 35));
        assert!(p.update(mouse(500, 500, true), 100, 50));
        assert_eq!((p.x, p.y), (99, 49));
        assert!(!p.update(mouse(-20, -20, true), 100, 50), "still held");
        assert!(!p.update(mouse(0, 0, false), 100, 50));
        assert!(p.update(mouse(0, 0, true), 100, 50));
        let shown = p.composite(&frame);
        assert_ne!(shown.pixels(), frame.pixels());
        assert_ne!(shown.get(80, 30), frame.get(80, 30));
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
