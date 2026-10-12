//! The daemon: a scene, a display, and a message loop.
//!
//! [`Splash`] is the pure part (state transitions, redraw policy) and is
//! tested with a [`NullDisplay`](crate::fb::NullDisplay). [`serve`] wires
//! it to a Unix datagram socket and the keyboard on hosts that have them.

use std::time::Duration;

use anyhow::Context;
use jos_gfx::scene::render_screen;
use jos_gfx::{Scene, Stage};

use crate::fb::Display;
use crate::proto::{Finish, Message};

/// Escape key, the only one the splash listens to.
pub const KEY_ESC: u8 = 0x1b;

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

    /// Consume pending keys: Esc ends the splash on the console.
    pub fn handle_keys(&mut self) -> Flow {
        while let Some(key) = self.display.poll_key() {
            if key == KEY_ESC {
                return Flow::ExitConsole;
            }
        }
        Flow::Continue
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
        match sock.recv(&mut buf) {
            Ok(n) => {
                let text = String::from_utf8_lossy(buf.get(..n).unwrap_or(&[]));
                match Message::parse(&text) {
                    Ok(msg) => {
                        let flow = splash.handle(msg);
                        if flow != Flow::Continue {
                            break flow;
                        }
                    }
                    Err(e) => eprintln!("jos-splash: {e}"),
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("reading splash socket"),
        }
        let flow = splash.handle_keys();
        if flow != Flow::Continue {
            break flow;
        }
        splash.redraw()?;
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
    use crate::fb::NullDisplay;

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
        d.keys = vec![KEY_ESC, b'a'];
        let mut s = Splash::new(d, None);
        assert_eq!(s.handle_keys(), Flow::ExitConsole);
        let mut d = NullDisplay::new(640, 480);
        d.keys = vec![b'x'];
        let mut s = Splash::new(d, None);
        assert_eq!(s.handle_keys(), Flow::Continue);
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
        let path2 = path.clone();
        let server = std::thread::spawn(move || {
            serve(Splash::new(NullDisplay::new(320, 200), None), &path2)
        });
        // Wait for the socket to appear.
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(send(&Message::Stage(Stage::Network), &path).unwrap());
        assert!(send(&Message::Finish(Finish::Keep), &path).unwrap());
        let display = server.join().unwrap().unwrap();
        assert!(display.presented >= 2);
        assert!(!display.restored);
        assert!(!send(&Message::Status("late".into()), &path).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
