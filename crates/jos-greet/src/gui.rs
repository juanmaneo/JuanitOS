//! The greeter's event loop: keys go to the [`Greeter`], frames to the
//! display, login attempts to a [`Backend`]. Around a desktop session the
//! display is released (text mode on the same VT, for seatd and the
//! compositor) and opened again when the session ends.

use std::time::Duration;

use jos_fb::keys::Decoder;
use jos_fb::Display;
use juanitos_core::sys::Runner;

use crate::greeter::{wipe, Action, Greeter, Power, Request};
use crate::sessions::Session;
use crate::ui;

/// What the greeter needs from the system.
pub trait Backend {
    /// Check `user`'s password (PAM `auth` + `account`). `Err` is the
    /// message shown on the login screen.
    fn authenticate(&mut self, user: &str, password: &str) -> Result<(), String>;
    /// Run `session` for the user just authenticated, until it exits.
    fn run_session(&mut self, user: &str, session: &Session) -> Result<(), String>;
    fn power(&mut self, p: Power) -> anyhow::Result<()>;
}

/// Power off / reboot with shutdown(8), for every backend.
pub fn shutdown(runner: &dyn Runner, p: Power) -> anyhow::Result<()> {
    let flag = match p {
        Power::Off => "-p",
        Power::Reboot => "-r",
    };
    runner.run_checked("shutdown", &[flag, "now"])?;
    Ok(())
}

/// A backend for `--dry-run`: any non-empty password is accepted and the
/// session "runs" for no time; nothing is authenticated or started.
pub struct DryBackend<'a> {
    pub runner: &'a dyn Runner,
}

impl Backend for DryBackend<'_> {
    fn authenticate(&mut self, user: &str, password: &str) -> Result<(), String> {
        if password.is_empty() {
            return Err("Login incorrect".to_string());
        }
        eprintln!("jos-greet: [dry-run] would authenticate {user} with PAM");
        Ok(())
    }

    fn run_session(&mut self, user: &str, session: &Session) -> Result<(), String> {
        eprintln!(
            "jos-greet: [dry-run] would start {} for {user}: {}",
            session.name,
            session.exec.join(" ")
        );
        Ok(())
    }

    fn power(&mut self, p: Power) -> anyhow::Result<()> {
        shutdown(self.runner, p)
    }
}

/// How the loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Power(Power),
    /// [`LoopOptions::max_sessions`] reached (tests).
    SessionsDone,
}

#[derive(Debug, Clone, Copy)]
pub struct LoopOptions {
    pub tick: Duration,
    /// Pause after a refused password (slows down guessing).
    pub fail_delay: Duration,
    pub max_ticks: Option<usize>,
    pub max_sessions: Option<usize>,
}

impl Default for LoopOptions {
    fn default() -> Self {
        LoopOptions {
            tick: Duration::from_millis(30),
            fail_delay: Duration::from_secs(2),
            max_ticks: None,
            max_sessions: None,
        }
    }
}

enum Stop {
    Login(Request),
    Power(Power),
}

/// Run the login screen until the machine is powered off or rebooted.
/// `open` (re)opens the display: once at start and after every session.
pub fn run<D: Display>(
    open: &mut dyn FnMut() -> anyhow::Result<D>,
    greeter: &mut Greeter,
    backend: &mut dyn Backend,
    opts: LoopOptions,
) -> anyhow::Result<Outcome> {
    let mut display: Option<D> = None;
    let mut ticks = 0usize;
    let mut sessions = 0usize;
    loop {
        let d = match display.as_mut() {
            Some(d) => d,
            None => display.insert(open()?),
        };
        let (width, height) = d.size();
        match serve(d, greeter, &opts, &mut ticks)? {
            Stop::Power(p) => {
                d.present(&ui::render(greeter, width, height))?;
                backend.power(p)?;
                return Ok(Outcome::Power(p));
            }
            Stop::Login(mut req) => {
                let auth = backend.authenticate(&req.user, &req.password);
                wipe(&mut req.password);
                if let Err(why) = auth {
                    greeter.login_failed(&why);
                    d.present(&ui::render(greeter, width, height))?;
                    std::thread::sleep(opts.fail_delay);
                    // Keys typed while waiting are not a new attempt.
                    while d.poll_key().is_some() {}
                    continue;
                }
                d.release()?;
                display = None;
                let result = backend.run_session(&req.user, &req.session);
                greeter.session_ended(result);
                sessions += 1;
                if opts.max_sessions.is_some_and(|max| sessions >= max) {
                    return Ok(Outcome::SessionsDone);
                }
            }
        }
    }
}

/// Show the greeter and feed it keys until it asks for something.
fn serve<D: Display>(
    d: &mut D,
    g: &mut Greeter,
    opts: &LoopOptions,
    ticks: &mut usize,
) -> anyhow::Result<Stop> {
    let (width, height) = d.size();
    let mut decoder = Decoder::new();
    let mut dirty = true;
    loop {
        if dirty {
            d.present(&ui::render(g, width, height))?;
            dirty = false;
        }
        if opts.max_ticks.is_some_and(|max| *ticks >= max) {
            anyhow::bail!("greeter stopped after {ticks} ticks");
        }
        *ticks += 1;
        let mut keys = Vec::new();
        while let Some(b) = d.poll_key() {
            keys.extend(decoder.feed(b));
        }
        keys.extend(decoder.flush());
        for key in keys {
            dirty = true;
            match g.handle(key) {
                Action::None => {}
                Action::Login(req) => {
                    d.present(&ui::render(g, width, height))?;
                    return Ok(Stop::Login(req));
                }
                Action::Power(p) => return Ok(Stop::Power(p)),
            }
        }
        if !dirty {
            std::thread::sleep(opts.tick);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::greeter::Message;
    use crate::ui::sample_greeter;
    use jos_fb::NullDisplay;
    use juanitos_core::sys::{CmdOutput, MockRunner};

    #[derive(Default)]
    struct Mock {
        log: Vec<String>,
        session_result: Option<String>,
        power_fails: bool,
    }

    impl Backend for Mock {
        fn authenticate(&mut self, user: &str, password: &str) -> Result<(), String> {
            self.log.push(format!("auth {user}"));
            if password == "right" {
                Ok(())
            } else {
                Err("Login incorrect".into())
            }
        }
        fn run_session(&mut self, user: &str, session: &Session) -> Result<(), String> {
            self.log.push(format!("session {user} {}", session.name));
            match &self.session_result {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
        fn power(&mut self, p: Power) -> anyhow::Result<()> {
            self.log.push(format!("power {p:?}"));
            anyhow::ensure!(!self.power_fails, "shutdown refused");
            Ok(())
        }
    }

    /// One display typed `keys` on and set up by `tweak`; `run` with it.
    fn run_once(
        keys: &'static [u8],
        tweak: impl Fn(&mut NullDisplay),
        m: &mut Mock,
    ) -> anyhow::Result<Outcome> {
        let mut open = || {
            let mut d = NullDisplay::new(320, 200);
            d.type_bytes(keys);
            tweak(&mut d);
            Ok(d)
        };
        run(&mut open, &mut sample_greeter(), m, opts())
    }

    #[test]
    fn display_and_backend_failures_end_the_loop() {
        let f1_twice: &[u8] = b"\x1b[M\x1b[M";
        let gone = |n: usize| move |d: &mut NullDisplay| d.fail_after_frames = Some(n);
        // The very first frame, the confirmation of a power key, the
        // "Logging in" frame, and the refusal frame.
        for (keys, frames) in [
            (b"" as &[u8], 0),
            (f1_twice, 1),
            (b"wrong\r", 1),
            (b"wrong\r", 2),
        ] {
            let err = run_once(keys, gone(frames), &mut Mock::default()).unwrap_err();
            assert_eq!(err.to_string(), "framebuffer gone", "{keys:?} {frames}");
        }
        // shutdown(8) refusing.
        let mut m = Mock {
            power_fails: true,
            ..Mock::default()
        };
        let err = run_once(f1_twice, |_| {}, &mut m).unwrap_err();
        assert_eq!(err.to_string(), "shutdown refused");
        // The VT cannot be handed to the session.
        let mut m = Mock::default();
        let err = run_once(b"right\r", |d| d.fail_release = true, &mut m).unwrap_err();
        assert_eq!(err.to_string(), "cannot release the display");
        assert_eq!(m.log, ["auth alice"], "no session without the VT");
        // Typing that asks for nothing: the screen is redrawn, then the
        // loop idles until it runs out of ticks.
        let err = run_once(b"x", |_| {}, &mut Mock::default()).unwrap_err();
        assert!(err.to_string().contains("ticks"), "{err}");
    }

    #[test]
    fn keys_typed_during_the_refusal_pause_are_dropped() {
        // Frames: the screen, "Logging in", the refusal (3rd): typed then.
        let mut m = Mock::default();
        let during_pause = |d: &mut NullDisplay| d.typed_at_frame = Some((3, b"right\r".to_vec()));
        let err = run_once(b"wrong\r", during_pause, &mut m).unwrap_err();
        assert!(err.to_string().contains("ticks"), "{err}");
        assert_eq!(m.log, ["auth alice"], "one attempt only");
    }

    fn opts() -> LoopOptions {
        LoopOptions {
            tick: Duration::from_millis(1),
            fail_delay: Duration::from_millis(1),
            max_ticks: Some(500),
            max_sessions: None,
        }
    }

    /// Each opened display gets the next burst of typing.
    fn opener(bursts: Vec<&'static [u8]>) -> impl FnMut() -> anyhow::Result<NullDisplay> {
        let mut bursts = bursts.into_iter();
        move || {
            let mut d = NullDisplay::new(640, 400);
            d.type_bytes(bursts.next().unwrap_or(b""));
            Ok(d)
        }
    }

    #[test]
    fn wrong_then_right_password_runs_the_session() {
        // A refused password keeps the display; nothing else is typed, so
        // the loop runs out of ticks.
        let mut open = opener(vec![b"wrong\r"]);
        let mut g = sample_greeter();
        let mut m = Mock::default();
        let err = run(&mut open, &mut g, &mut m, opts()).unwrap_err();
        assert!(err.to_string().contains("ticks"));
        assert_eq!(m.log, vec!["auth alice"]);
        assert_eq!(g.message, Some(Message::Error("Login incorrect".into())));
        assert!(g.password.is_empty());

        let mut open = opener(vec![b"right\r"]);
        let mut o = opts();
        o.max_sessions = Some(1);
        let out = run(&mut open, &mut g, &mut m, o).unwrap();
        assert_eq!(out, Outcome::SessionsDone);
        assert_eq!(
            m.log,
            vec!["auth alice", "auth alice", "session alice COSMIC"]
        );
        assert_eq!(g.message, Some(Message::Info("alice logged out".into())));
    }

    #[test]
    fn display_is_reopened_after_a_session_then_power_off() {
        let mut opened = 0;
        // vt(4) sends F1 as ESC [ M.
        let mut bursts = vec![&b"right\r"[..], b"\x1b[M\x1b[M"].into_iter();
        let mut open = || {
            opened += 1;
            let mut d = NullDisplay::new(320, 200);
            d.type_bytes(bursts.next().unwrap_or(b""));
            Ok(d)
        };
        let mut g = sample_greeter();
        let mut m = Mock {
            session_result: Some("COSMIC exited with status 1".into()),
            ..Default::default()
        };
        let out = run(&mut open, &mut g, &mut m, opts()).unwrap();
        assert_eq!(out, Outcome::Power(Power::Off));
        assert_eq!(opened, 2);
        assert_eq!(m.log.last().map(String::as_str), Some("power Off"));
        assert!(matches!(&g.message, Some(Message::Info(t)) if t.contains("power off")));
    }

    #[test]
    fn open_failures_propagate() {
        let mut open = || -> anyhow::Result<NullDisplay> { anyhow::bail!("no framebuffer") };
        let err = run(
            &mut open,
            &mut sample_greeter(),
            &mut Mock::default(),
            opts(),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "no framebuffer");
        assert_eq!(LoopOptions::default().max_ticks, None);
    }

    #[test]
    fn dry_backend_and_shutdown() {
        let m = MockRunner::new().respond("shutdown", &[], CmdOutput::ok(""));
        let mut b = DryBackend { runner: &m };
        assert!(b.authenticate("alice", "").is_err());
        assert!(b.authenticate("alice", "x").is_ok());
        assert!(b.run_session("alice", &Session::cosmic_default()).is_ok());
        b.power(Power::Reboot).unwrap();
        b.power(Power::Off).unwrap();
        assert_eq!(m.calls(), vec!["shutdown -r now", "shutdown -p now"]);
        let failing = MockRunner::new().respond("shutdown", &[], CmdOutput::failed(1, "denied"));
        assert!(shutdown(&failing, Power::Off).is_err());
    }
}
