//! The daemon: a scene, a display, and a message loop.
//!
//! [`Splash`] is the pure part (state transitions, redraw policy) and is
//! tested with a [`NullDisplay`](jos_fb::NullDisplay). [`serve`] wires
//! it to a Unix datagram socket and the keyboard on hosts that have them.

use std::time::Duration;

use anyhow::Context;
use jos_gfx::scene::render_screen;
use jos_gfx::{Scene, Stage};

use crate::proto::{Finish, Message};
use jos_fb::keys::{Decoder, Key};
use jos_fb::Display;

/// Poll period for the socket and the keyboard.
pub const TICK: Duration = Duration::from_millis(50);

/// What the loop should do after handling an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Leave the frame on screen and exit.
    ExitKeep,
    /// Restore the console and exit.
    ExitConsole,
}

pub struct Splash<D: Display> {
    display: D,
    scene: Scene,
    dirty: bool,
    /// Frames presented so far (for tests and `--dry-run` reporting).
    pub frames: usize,
}

impl<D: Display> Splash<D> {
    pub fn new(display: D, reached: Option<Stage>) -> Self {
        let mut scene = Scene::new();
        if let Some(stage) = reached {
            scene.reach(stage);
        }
        Splash {
            display,
            scene,
            dirty: true,
            frames: 0,
        }
    }

    #[cfg(test)]
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    #[cfg(test)]
    pub fn display(&self) -> &D {
        &self.display
    }

    pub fn into_display(self) -> D {
        self.display
    }

    /// Apply a message from a client.
    pub fn handle(&mut self, msg: Message) -> Flow {
        match msg {
            Message::Stage(stage) => {
                if !self.scene.is_lit(stage) {
                    self.scene.reach(stage);
                    self.dirty = true;
                }
                Flow::Continue
            }
            Message::Status(text) => {
                let status = if text.is_empty() {
                    None
                } else {
                    Some(text.to_uppercase())
                };
                if self.scene.status != status {
                    self.scene.status = status;
                    self.dirty = true;
                }
                Flow::Continue
            }
            Message::Finish(Finish::Keep) => Flow::ExitKeep,
            Message::Finish(Finish::Console) => Flow::ExitConsole,
        }
    }

    /// Consume pending keys: Esc ends the splash on the console. Escape
    /// sequences (arrows, function keys) are decoded and ignored, so only
    /// the Escape key itself counts.
    pub fn handle_keys(&mut self) -> Flow {
        let mut decoder = Decoder::new();
        let mut keys = Vec::new();
        while let Some(b) = self.display.poll_key() {
            keys.extend(decoder.feed(b));
        }
        keys.extend(decoder.flush());
        if keys.contains(&Key::Esc) {
            Flow::ExitConsole
        } else {
            Flow::Continue
        }
    }

    /// Redraw if anything changed since the last frame.
    pub fn redraw(&mut self) -> anyhow::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let (w, h) = self.display.size();
        let frame = render_screen(&self.scene, w, h);
        self.display.present(&frame).context("presenting frame")?;
        self.frames += 1;
        self.dirty = false;
        Ok(())
    }

    /// Final housekeeping for a given exit mode.
    pub fn finish(&mut self, flow: Flow) -> anyhow::Result<()> {
        match flow {
            Flow::ExitKeep => {
                // Light everything: the desktop is about to appear.
                self.scene.reach(Stage::Desktop);
                self.scene.status = None;
                self.dirty = true;
                self.redraw()
            }
            Flow::ExitConsole => self.display.restore_console(),
            Flow::Continue => Ok(()),
        }
    }
}

/// One turn of the server loop: what `recv` returned (a datagram, or the
/// error), then the keyboard, then a redraw. `Some` ends the loop. Kept
/// apart from the socket so every outcome is tested directly; whether a
/// real `recv` times out depends on scheduling.
pub fn step<D: Display>(
    splash: &mut Splash<D>,
    received: std::io::Result<&[u8]>,
) -> anyhow::Result<Option<Flow>> {
    match received {
        Ok(data) => match Message::parse(&String::from_utf8_lossy(data)) {
            Ok(msg) => {
                let flow = splash.handle(msg);
                if flow != Flow::Continue {
                    return Ok(Some(flow));
                }
            }
            Err(e) => eprintln!("jos-splash: {e}"),
        },
        Err(e) if is_idle(e.kind()) => {}
        Err(e) => return Err(e).context("reading splash socket"),
    }
    let flow = splash.handle_keys();
    if flow != Flow::Continue {
        return Ok(Some(flow));
    }
    splash.redraw()?;
    Ok(None)
}

/// A `recv` that only means "nothing arrived within the tick".
fn is_idle(kind: std::io::ErrorKind) -> bool {
    use std::io::ErrorKind::{Interrupted, TimedOut, WouldBlock};
    matches!(kind, WouldBlock | TimedOut | Interrupted)
}

/// Serve on a Unix datagram socket until a `finish` message or Esc.
#[cfg(unix)]
pub fn serve<D: Display>(
    mut splash: Splash<D>,
    socket_path: &std::path::Path,
) -> anyhow::Result<D> {
    use std::os::unix::net::UnixDatagram;

    // A stale socket from a crashed daemon would make bind fail; replacing
    // it is the only file we ever remove.
    if socket_path.exists() {
        std::fs::remove_file(socket_path)
            .with_context(|| format!("removing stale {}", socket_path.display()))?;
    }
    let sock = UnixDatagram::bind(socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    sock.set_read_timeout(Some(TICK))?;

    splash.redraw()?;
    let mut buf = [0u8; 512];
    let flow = loop {
        let received = sock.recv(&mut buf).map(|n| buf.get(..n).unwrap_or(&[]));
        if let Some(flow) = step(&mut splash, received)? {
            break flow;
        }
    };
    splash.finish(flow)?;
    let _ = std::fs::remove_file(socket_path);
    Ok(splash.into_display())
}

/// Send one message to a running daemon. A missing socket means no splash
/// is running (text boot, or already finished): that is not an error for
/// the rc.d scripts, so `Ok(false)` is returned instead.
#[cfg(unix)]
pub fn send(msg: &Message, socket_path: &std::path::Path) -> anyhow::Result<bool> {
    use std::os::unix::net::UnixDatagram;

    if !socket_path.exists() {
        return Ok(false);
    }
    let sock = UnixDatagram::unbound()?;
    match sock.send_to(msg.encode().as_bytes(), socket_path) {
        Ok(_) => Ok(true),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::ConnectionRefused =>
        {
            Ok(false)
        }
        Err(e) => Err(e).with_context(|| format!("sending to {}", socket_path.display())),
    }
}

#[cfg(not(unix))]
pub fn send(_msg: &Message, _socket_path: &std::path::Path) -> anyhow::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jos_fb::NullDisplay;

    #[test]
    fn stages_light_up_and_only_changes_redraw() {
        let mut s = Splash::new(NullDisplay::new(800, 600), Some(Stage::Storage));
        s.redraw().unwrap();
        assert_eq!(s.frames, 1);
        s.redraw().unwrap();
        assert_eq!(s.frames, 1, "clean scene must not redraw");

        assert_eq!(s.handle(Message::Stage(Stage::Kernel)), Flow::Continue);
        s.redraw().unwrap();
        assert_eq!(s.frames, 1, "already-lit stage is a no-op");

        assert_eq!(s.handle(Message::Stage(Stage::Qubes)), Flow::Continue);
        s.redraw().unwrap();
        assert_eq!(s.frames, 2);
        assert!(s.scene().is_lit(Stage::Network));
        assert!(!s.scene().is_lit(Stage::Desktop));
    }

    #[test]
    fn status_is_uppercased_and_cleared() {
        let mut s = Splash::new(NullDisplay::new(640, 480), None);
        s.handle(Message::Status("Starting network".into()));
        assert_eq!(s.scene().status.as_deref(), Some("STARTING NETWORK"));
        s.handle(Message::Status(String::new()));
        assert_eq!(s.scene().status, None);
    }

    #[test]
    fn finish_modes() {
        let mut s = Splash::new(NullDisplay::new(640, 480), None);
        assert_eq!(s.handle(Message::Finish(Finish::Keep)), Flow::ExitKeep);
        s.finish(Flow::ExitKeep).unwrap();
        assert!(s.scene().is_lit(Stage::Desktop));
        assert!(!s.display().restored);
        assert_eq!(s.frames, 1);

        let mut c = Splash::new(NullDisplay::new(640, 480), None);
        assert_eq!(
            c.handle(Message::Finish(Finish::Console)),
            Flow::ExitConsole
        );
        c.finish(Flow::ExitConsole).unwrap();
        assert!(c.display().restored);
    }

    #[test]
    fn escape_key_ends_on_console() {
        let mut d = NullDisplay::new(640, 480);
        d.type_bytes(b"a\x1b");
        let mut s = Splash::new(d, None);
        assert_eq!(s.handle_keys(), Flow::ExitConsole);
        let mut d = NullDisplay::new(640, 480);
        // Arrow keys start with ESC but are not Escape.
        d.type_bytes(b"x\x1b[A\x1b[B");
        let mut s = Splash::new(d, None);
        assert_eq!(s.handle_keys(), Flow::Continue);
    }

    #[test]
    fn every_loop_outcome() {
        use std::io::{Error, ErrorKind};
        let mut s = Splash::new(NullDisplay::new(320, 200), None);
        // A message that changes the scene is drawn in the same turn.
        assert_eq!(step(&mut s, Ok(b"stage network")).unwrap(), None);
        assert_eq!(s.frames, 1);
        // Garbage is reported and ignored.
        assert_eq!(step(&mut s, Ok(b"\xffnonsense")).unwrap(), None);
        for kind in [
            ErrorKind::WouldBlock,
            ErrorKind::TimedOut,
            ErrorKind::Interrupted,
        ] {
            assert_eq!(step(&mut s, Err(Error::from(kind))).unwrap(), None);
        }
        assert_eq!(s.frames, 1, "nothing changed, nothing redrawn");
        let err = step(&mut s, Err(Error::from(ErrorKind::PermissionDenied))).unwrap_err();
        assert!(
            format!("{err:#}").contains("reading splash socket"),
            "{err:#}"
        );
        assert_eq!(
            step(&mut s, Ok(b"finish console")).unwrap(),
            Some(Flow::ExitConsole)
        );

        // Esc ends the loop even when nothing arrived.
        let mut d = NullDisplay::new(320, 200);
        d.type_bytes(b"\x1b");
        let mut s = Splash::new(d, None);
        let idle = Err(Error::from(ErrorKind::WouldBlock));
        assert_eq!(step(&mut s, idle).unwrap(), Some(Flow::ExitConsole));
        // Leaving with nothing to finish does nothing.
        s.finish(Flow::Continue).unwrap();
        assert!(!s.display().restored);
    }

    #[cfg(unix)]
    #[test]
    fn socket_roundtrip_ends_the_server() {
        // Some build sandboxes (seccomp) forbid AF_UNIX altogether.
        if std::os::unix::net::UnixDatagram::unbound().is_err() {
            eprintln!("skipped: Unix sockets are not permitted in this environment");
            return;
        }
        let dir = std::env::temp_dir().join(format!("jos-splash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        // A crashed daemon's socket is replaced; until then nobody listens.
        drop(std::os::unix::net::UnixDatagram::bind(&path).unwrap());
        assert!(!send(&Message::Status("early".into()), &path).unwrap());
        let path2 = path.clone();
        let server = std::thread::spawn(move || {
            serve(Splash::new(NullDisplay::new(320, 200), None), &path2)
        });
        // Wait for the server to replace the stale socket.
        while !send(&Message::Stage(Stage::Network), &path).unwrap() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(send(&Message::Finish(Finish::Keep), &path).unwrap());
        let display = server.join().unwrap().unwrap();
        assert!(display.presented >= 2);
        assert!(!display.restored);
        assert!(!send(&Message::Status("late".into()), &path).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
