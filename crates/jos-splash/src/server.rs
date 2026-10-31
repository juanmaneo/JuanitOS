//! The daemon: a scene, a display, and a message loop.
//!
//! [`Splash`] is the pure part (state transitions, redraw policy) and is
//! tested with a [`NullDisplay`](jos_fb::NullDisplay). [`serve`] wires
//! it to a Unix datagram socket and the keyboard on hosts that have them.

use std::ops::Range;
use std::time::Duration;

use anyhow::Context;
use jos_gfx::scene::render_screen;
use jos_gfx::{Image, Rgba, Scene, Stage};

use crate::proto::{Finish, Message};
use jos_fb::keys::{Decoder, Key};
use jos_fb::Display;

/// Poll period for the socket and the keyboard.
pub const TICK: Duration = Duration::from_millis(50);

/// Rows the vt(4) mouse pointer can dirty. When `rc.d/moused` runs (in the
/// middle of the boot) it turns the pointer on, and vt draws it once at the centre of the
/// screen, right over the card title, together with the blank text cells
/// under it (up to 16x32 pixels each). vt ignores pointer motion on a
/// graphics-mode terminal, so that one drawing stays until repainted: the
/// daemon re-presents these rows every [`TICK`].
pub fn pointer_rows(height: u32) -> Range<u32> {
    let cy = height / 2;
    cy.saturating_sub(64)..(cy + 128).min(height)
}

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
    /// The last frame presented.
    frame: Image,
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
            frame: Image::new(0, 0, Rgba::TRANSPARENT),
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
            Message::Leave(stage) => {
                if self.scene.is_lit(stage) {
                    self.scene.leave(stage);
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

    /// Draw a new frame if anything changed since the last one, otherwise
    /// repair the rows under the vt pointer ([`pointer_rows`]).
    pub fn redraw(&mut self) -> anyhow::Result<()> {
        let (w, h) = self.display.size();
        if !self.dirty {
            return self
                .display
                .present_rows(&self.frame, pointer_rows(h))
                .context("repairing frame");
        }
        self.frame = render_screen(&self.scene, w, h);
        self.display
            .present(&self.frame)
            .context("presenting frame")?;
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
        .and_then(|s| s.set_read_timeout(Some(TICK)).map(|()| s))
        .with_context(|| format!("binding {}", socket_path.display()))?;

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

/// Send one message to a running daemon. A missing socket (the send
/// ends in `NotFound` or `ConnectionRefused`) means no splash is running
/// (text boot, already finished, or the stale socket of a crash): that is
/// not an error for the rc.d scripts, so `Ok(false)` is returned instead.
#[cfg(unix)]
pub fn send(msg: &Message, socket_path: &std::path::Path) -> anyhow::Result<bool> {
    use std::os::unix::net::UnixDatagram;

    let sent =
        UnixDatagram::unbound().and_then(|sock| sock.send_to(msg.encode().as_bytes(), socket_path));
    match sent {
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
    fn the_rows_under_the_pointer_are_repaired_between_frames() {
        let mut s = Splash::new(NullDisplay::new(800, 600), None);
        s.redraw().unwrap();
        assert!(s.display().repaired.is_empty(), "a new frame covers it");
        s.redraw().unwrap();
        s.redraw().unwrap();
        assert_eq!(s.frames, 1);
        assert_eq!(s.display().repaired, vec![236..428, 236..428]);
        // The band covers the card title and never leaves the screen.
        assert_eq!(pointer_rows(40), 0..40);
        assert_eq!(pointer_rows(1080), 476..668);

        let mut s = Splash::new(
            NullDisplay {
                fail_repair: true,
                ..NullDisplay::new(64, 40)
            },
            None,
        );
        s.redraw().unwrap();
        let err = s.redraw().unwrap_err();
        assert!(format!("{err:#}").contains("repairing frame"), "{err:#}");
    }

    #[test]
    fn shutdown_turns_the_icons_off_from_the_right() {
        let mut s = Splash::new(NullDisplay::new(320, 200), Some(Stage::Desktop));
        s.redraw().unwrap();
        assert_eq!(s.handle(Message::Leave(Stage::Qubes)), Flow::Continue);
        s.redraw().unwrap();
        assert_eq!(s.frames, 2);
        assert!(s.scene().is_lit(Stage::Network));
        assert!(!s.scene().is_lit(Stage::Desktop));
        // Already dark: nothing to draw.
        s.handle(Message::Leave(Stage::Desktop));
        s.redraw().unwrap();
        assert_eq!(s.frames, 2);
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

    #[test]
    fn the_same_status_twice_draws_once() {
        let mut s = Splash::new(NullDisplay::new(64, 40), None);
        s.handle(Message::Status("Starting".into()));
        s.redraw().unwrap();
        s.handle(Message::Status("Starting".into()));
        s.redraw().unwrap();
        assert_eq!(s.frames, 1);
    }

    /// A scratch directory for sockets, removed on drop.
    #[cfg(unix)]
    struct Dir(std::path::PathBuf);

    #[cfg(unix)]
    impl Dir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("jos-splash-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
    }

    #[cfg(unix)]
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run `serve` on `display` in a thread, send `msgs` once it listens,
    /// and return its result. The server thread waits for a signal before
    /// it starts, so while it is held back the socket is stale or absent
    /// and the client's first send always fails: the retry body below
    /// always runs at least once, whatever the scheduler does, which
    /// keeps its coverage deterministic.
    #[cfg(unix)]
    fn serve_with(
        display: NullDisplay,
        path: &std::path::Path,
        msgs: &[&str],
    ) -> anyhow::Result<NullDisplay> {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let p = path.to_path_buf();
        let server = std::thread::spawn(move || {
            let _ = wait.recv();
            serve(Splash::new(display, None), &p)
        });
        let probe = std::os::unix::net::UnixDatagram::unbound().unwrap();
        for msg in msgs {
            while probe.send_to(msg.as_bytes(), path).is_err() {
                std::thread::sleep(Duration::from_millis(5));
                // Let the server start (the channel carries one message;
                // the later sends are dropped with the receiver).
                let _ = go.send(());
            }
        }
        // Also start the server when there is nothing to send.
        let _ = go.send(());
        server.join().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn server_failures_are_reported() {
        let d = Dir::new("fail");
        // A stale path that cannot be removed (a non-empty directory).
        let stuck = d.0.join("stuck");
        std::fs::create_dir_all(stuck.join("x")).unwrap();
        let err = serve(Splash::new(NullDisplay::new(8, 8), None), &stuck).unwrap_err();
        assert!(format!("{err:#}").contains("removing stale"), "{err:#}");
        // A socket in a directory that does not exist.
        let nowhere = d.0.join("missing/s.sock");
        let err = serve(Splash::new(NullDisplay::new(8, 8), None), &nowhere).unwrap_err();
        assert!(format!("{err:#}").contains("binding"), "{err:#}");

        let gone = |frames| NullDisplay {
            fail_after_frames: Some(frames),
            ..NullDisplay::new(8, 8)
        };
        let sock = d.0.join("s.sock");
        // The first frame, a frame after a message, the last frame.
        for (display, msgs) in [
            (gone(0), &[][..]),
            (gone(1), &["stage network"][..]),
            (gone(1), &["finish keep"][..]),
        ] {
            let err = serve_with(display, &sock, msgs).unwrap_err();
            assert!(
                format!("{err:#}").contains("framebuffer gone"),
                "{msgs:?}: {err:#}"
            );
        }
        // The console cannot be restored.
        let stuck_vt = NullDisplay {
            fail_release: true,
            ..NullDisplay::new(8, 8)
        };
        let err = serve_with(stuck_vt, &sock, &["finish console"]).unwrap_err();
        assert!(err.to_string().contains("restore"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_we_may_not_write_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let d = Dir::new("perm");
        let path = d.0.join("s.sock");
        let _listener = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = send(&Message::Status("x".into()), &path).unwrap_err();
        assert!(format!("{err:#}").contains("sending to"), "{err:#}");
    }

    #[cfg(unix)]
    #[test]
    fn socket_roundtrip_ends_the_server() {
        let dir = std::env::temp_dir().join(format!("jos-splash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        // A crashed daemon's socket is replaced; until then nobody listens.
        drop(std::os::unix::net::UnixDatagram::bind(&path).unwrap());
        assert!(!send(&Message::Status("early".into()), &path).unwrap());
        let path2 = path.clone();
        // The server thread waits for a signal, so the first send below
        // meets the stale socket and fails: the retry body always runs,
        // whatever the scheduler does.
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let _ = wait.recv();
            serve(Splash::new(NullDisplay::new(320, 200), None), &path2)
        });
        // Wait for the server to replace the stale socket.
        while !send(&Message::Stage(Stage::Network), &path).unwrap() {
            std::thread::sleep(Duration::from_millis(10));
            let _ = go.send(());
        }
        assert!(send(&Message::Finish(Finish::Keep), &path).unwrap());
        let display = server.join().unwrap().unwrap();
        assert!(display.presented >= 2);
        assert!(!display.restored);
        assert!(!send(&Message::Status("late".into()), &path).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
