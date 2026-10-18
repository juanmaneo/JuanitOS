//! Drawing the login screen in the boot splash's palette and font, so the
//! boot card, the installer and the greeter look like one product. Laid
//! out on the installer's logical 1280×800 canvas, scaled by an integer
//! factor and centred on bigger screens.

use jos_gfx::color::THEME;
use jos_gfx::font;
use jos_gfx::icons;
use jos_gfx::image::{Image, Rect};
use jos_gfx::Rgba;

use crate::greeter::{Focus, Greeter, Message};

pub const LOGICAL_W: i32 = 1280;
pub const LOGICAL_H: i32 = 800;

const CARD_X: i32 = 420;
const CARD_W: i32 = 440;
const FIELD_X: i32 = CARD_X + 20;
const FIELD_W: i32 = CARD_W - 40;
const FIELD_H: i32 = 44;

const CARD_BG: Rgba = Rgba::rgb(0x0e, 0x11, 0x16);
const FAIL: Rgba = Rgba::rgb(0xff, 0x5c, 0x5c);

struct Canvas {
    img: Image,
    s: i32,
    ox: i32,
    oy: i32,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Self {
        let s = (width as i32 / LOGICAL_W)
            .min(height as i32 / LOGICAL_H)
            .max(1);
        Canvas {
            img: Image::new(width, height, THEME.background),
            s,
            ox: (width as i32 - LOGICAL_W * s) / 2,
            oy: (height as i32 - LOGICAL_H * s) / 2,
        }
    }

    fn rect(&self, x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(
            self.ox + x * self.s,
            self.oy + y * self.s,
            w * self.s,
            h * self.s,
        )
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: Rgba) {
        let r = self.rect(x, y, w, h);
        self.img.fill_rect(r, c);
    }

    fn rounded(&mut self, x: i32, y: i32, w: i32, h: i32, radius: i32, c: Rgba) {
        let r = self.rect(x, y, w, h);
        self.img.rounded_rect(r, (radius * self.s) as f32, c);
    }

    fn boxed(&mut self, x: i32, y: i32, w: i32, h: i32, border: Rgba, fill: Rgba) {
        self.rounded(x, y, w, h, 6, border);
        self.rounded(x + 2, y + 2, w - 4, h - 4, 5, fill);
    }

    fn text(&mut self, x: i32, y: i32, text: &str, scale: i32, c: Rgba) {
        font::draw_text(
            &mut self.img,
            self.ox + x * self.s,
            self.oy + y * self.s,
            text,
            scale * self.s,
            c,
        );
    }

    fn text_centered(&mut self, cx: i32, y: i32, text: &str, scale: i32, c: Rgba) {
        let shown = font::ellipsize(text, LOGICAL_W - 40, scale);
        let w = font::text_width(&shown, scale);
        self.text(cx - w / 2, y, &shown, scale, c);
    }

    fn logo(&mut self, cx: i32, cy: i32, r: i32) {
        icons::draw_logo(
            &mut self.img,
            self.ox + cx * self.s,
            self.oy + cy * self.s,
            r * self.s,
            THEME.text,
            THEME.accent,
        );
    }
}

/// Render the whole screen for the greeter's state.
pub fn render(g: &Greeter, width: u32, height: u32) -> Image {
    let mut c = Canvas::new(width, height);
    let cx = LOGICAL_W / 2;
    c.logo(cx, 150, 52);
    c.text_centered(cx, 228, "JUANITOS", 4, THEME.text);
    c.text_centered(cx, 272, &g.hostname, 2, THEME.text_muted);

    c.rounded(CARD_X, 320, CARD_W, 340, 12, CARD_BG);
    let shown_password = "*".repeat(g.password.chars().count());
    field(&mut c, g, Focus::User, 340, "User", &g.user);
    field(&mut c, g, Focus::Password, 428, "Password", &shown_password);
    session(&mut c, g, 516);

    if let Some(m) = &g.message {
        let (text, colour) = match m {
            Message::Info(t) => (t, THEME.accent),
            Message::Error(t) => (t, FAIL),
        };
        c.text_centered(cx, 628, text, 2, colour);
    }

    c.fill(0, LOGICAL_H - 70, LOGICAL_W, 2, THEME.highlight);
    c.text_centered(
        cx,
        LOGICAL_H - 46,
        "Tab: next field   Enter: log in   F1: power off   F2: reboot   Alt+F1: console",
        2,
        THEME.text_muted,
    );
    c.img
}

fn label(c: &mut Canvas, g: &Greeter, which: Focus, y: i32, text: &str) -> bool {
    let focused = g.focus == which;
    let colour = if focused { THEME.accent } else { THEME.text };
    c.text(FIELD_X, y, text, 2, colour);
    focused
}

fn field(c: &mut Canvas, g: &Greeter, which: Focus, y: i32, name: &str, value: &str) {
    let focused = label(c, g, which, y, name);
    let by = y + 24;
    let border = if focused { THEME.accent } else { THEME.dim };
    c.boxed(FIELD_X, by, FIELD_W, FIELD_H, border, THEME.highlight);
    // Keep the end of a long value (where the caret is) in view.
    let max = FIELD_W - 28;
    let mut shown: &str = value;
    while font::text_width(shown, 2) > max {
        let mut chars = shown.chars();
        chars.next();
        shown = chars.as_str();
    }
    c.text(FIELD_X + 14, by + 15, shown, 2, THEME.text);
    if focused {
        let x = FIELD_X + 14 + font::text_width(shown, 2) + 1;
        c.fill(x.min(FIELD_X + FIELD_W - 12), by + 11, 2, 22, THEME.accent);
    }
}

fn session(c: &mut Canvas, g: &Greeter, y: i32) {
    let focused = label(c, g, Focus::Session, y, "Session");
    let by = y + 24;
    let border = if focused { THEME.accent } else { THEME.dim };
    c.boxed(FIELD_X, by, FIELD_W, FIELD_H, border, THEME.highlight);
    let arrows = if g.sessions.len() > 1 || focused {
        THEME.text_muted
    } else {
        THEME.dim
    };
    c.text(FIELD_X + 14, by + 15, "<", 2, arrows);
    c.text(FIELD_X + FIELD_W - 26, by + 15, ">", 2, arrows);
    let name = g.current_session().name;
    let shown = font::ellipsize(&name, FIELD_W - 80, 2);
    let w = font::text_width(&shown, 2);
    c.text(FIELD_X + (FIELD_W - w) / 2, by + 15, &shown, 2, THEME.text);
}

/// A greeter with plausible content, for screenshots and tests.
pub fn sample_greeter() -> Greeter {
    use crate::sessions::Session;
    Greeter::new(
        "juanitos",
        vec!["alice".to_string(), "bob".to_string()],
        vec![
            Session::cosmic_default(),
            Session {
                name: "Sway".into(),
                exec: vec!["sway".into()],
                desktop: "sway".into(),
            },
        ],
    )
}

/// The login screen's states as PNGs in `dir` (docs and layout checks).
pub fn screenshots(dir: &std::path::Path, width: u32, height: u32) -> anyhow::Result<Vec<String>> {
    use jos_fb::keys::Key;

    std::fs::create_dir_all(dir)?;
    let mut written = Vec::new();
    let mut shot = |g: &Greeter, name: &str| -> anyhow::Result<()> {
        let file = dir.join(format!("{name}.png"));
        jos_gfx::png::write(&render(g, width, height), &file)?;
        written.push(file.display().to_string());
        Ok(())
    };
    let mut g = sample_greeter();
    for ch in "hunter2".chars() {
        g.handle(Key::Char(ch));
    }
    shot(&g, "greeter-1-login")?;
    g.login_failed("Login incorrect");
    shot(&g, "greeter-2-failed")?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jos_fb::keys::Key;

    fn differs(a: &Image, b: &Image) -> bool {
        (0..a.height()).any(|y| a.row(y) != b.row(y))
    }

    #[test]
    fn screenshot_failures_are_reported() {
        let dir = std::env::temp_dir().join(format!("jos-greet-badshots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The directory is a file.
        let file = dir.join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(screenshots(&file, 64, 40).is_err());
        // The first picture, then the second, cannot be written.
        for name in ["greeter-1-login.png", "greeter-2-failed.png"] {
            let out = dir.join(name.replace(".png", ""));
            std::fs::create_dir_all(out.join(name)).unwrap();
            assert!(screenshots(&out, 64, 40).is_err(), "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renders_at_any_size_and_reacts_to_state() {
        let mut g = sample_greeter();
        let base = render(&g, 1280, 800);
        assert_eq!((base.width(), base.height()), (1280, 800));
        // Scaled and letterboxed, and smaller than the canvas.
        let big = render(&g, 2560, 1700);
        assert_eq!((big.width(), big.height()), (2560, 1700));
        let small = render(&g, 640, 480);
        assert_eq!(small.width(), 640);

        g.handle(Key::Char('x'));
        let typed = render(&g, 1280, 800);
        assert!(differs(&base, &typed), "password dots are drawn");
        g.login_failed("Login incorrect");
        assert!(differs(&base, &render(&g, 1280, 800)));
        g.focus = Focus::Session;
        g.message = Some(Message::Info("hello".into()));
        let one = render(&g, 1280, 800);
        g.handle(Key::Right);
        assert!(
            differs(&one, &render(&g, 1280, 800)),
            "session name changes"
        );
    }

    #[test]
    fn long_values_and_single_session() {
        let mut g = Greeter::new(&"h".repeat(400), vec![], vec![]);
        for _ in 0..crate::greeter::MAX_USER {
            g.handle(Key::Char('w'));
        }
        g.handle(Key::Tab);
        for _ in 0..200 {
            g.handle(Key::Char('w'));
        }
        let img = render(&g, 1280, 800);
        assert_eq!(img.width(), 1280);
    }

    #[test]
    fn screenshots_are_written() {
        let dir = std::env::temp_dir().join(format!("jos-greet-shots-{}", std::process::id()));
        let files = screenshots(&dir, 640, 400).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| std::path::Path::new(f).is_file()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
