//! Drawing the installer: a Calamares-style layout (steps in a sidebar on
//! the left, the current page on the right, Back/Next at the bottom) in
//! the boot splash's palette and font, so boot → installer → desktop look
//! like one product.
//!
//! Everything is laid out on a logical 1280×800 canvas, scaled by an
//! integer factor and centred on bigger screens (the bitmap font stays
//! crisp), and rendered into a [`jos_gfx::Image`]: the same code produces
//! the frames on the framebuffer and the PNG screenshots in `docs/`.

use jos_gfx::color::THEME;
use jos_gfx::font;
use jos_gfx::icons;
use jos_gfx::image::{Image, Rect};
use jos_gfx::Rgba;
use juanitos_core::doctor::Status;

use crate::wizard::{InstallState, Page, Widget, Wizard, PAGES};

pub const LOGICAL_W: i32 = 1280;
pub const LOGICAL_H: i32 = 800;

const SIDEBAR_W: i32 = 300;
const CONTENT_X: i32 = 360;
const CONTENT_R: i32 = 1220;
const CONTENT_W: i32 = CONTENT_R - CONTENT_X;
const BODY_Y: i32 = 170;
const BAR_Y: i32 = 690;
const BUTTON_W: i32 = 200;
const BUTTON_H: i32 = 48;
const FIELD_W: i32 = 520;
const FIELD_H: i32 = 44;

const SIDEBAR_BG: Rgba = Rgba::rgb(0x0e, 0x11, 0x16);
const OK: Rgba = Rgba::rgb(0x4c, 0xc3, 0x7a);
const FAIL: Rgba = Rgba::rgb(0xff, 0x5c, 0x5c);

/// Integer scale and offset mapping the logical canvas onto the screen.
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

    /// A rounded box with a 2-pixel border.
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

    /// Text clipped to `max_w` logical pixels.
    fn text_clip(&mut self, x: i32, y: i32, text: &str, scale: i32, max_w: i32, c: Rgba) {
        let shown = font::ellipsize(text, max_w, scale);
        self.text(x, y, &shown, scale, c);
    }

    fn text_centered(&mut self, cx: i32, y: i32, text: &str, scale: i32, c: Rgba) {
        let w = font::text_width(text, scale);
        self.text(cx - w / 2, y, text, scale, c);
    }

    fn circle(&mut self, cx: i32, cy: i32, r: i32, c: Rgba) {
        self.img.fill_circle(
            (self.ox + cx * self.s) as f32,
            (self.oy + cy * self.s) as f32,
            (r * self.s) as f32,
            c,
        );
    }

    fn ring(&mut self, cx: i32, cy: i32, r: i32, c: Rgba) {
        self.img.ring(
            (self.ox + cx * self.s) as f32,
            (self.oy + cy * self.s) as f32,
            ((r - 2) * self.s) as f32,
            (r * self.s) as f32,
            c,
        );
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

/// Render the whole screen for the wizard's current state.
pub fn render(w: &Wizard, width: u32, height: u32) -> Image {
    let mut c = Canvas::new(width, height);
    // The sidebar runs the full height of the screen, also outside the
    // logical canvas.
    let side = Rect::new(0, 0, c.ox + SIDEBAR_W * c.s, height as i32);
    c.img.fill_rect(side, SIDEBAR_BG);
    sidebar(&mut c, w);
    match w.page {
        Page::Welcome => welcome(&mut c),
        Page::Hardware => hardware(&mut c, w),
        Page::Disk => disk(&mut c, w),
        Page::System => system(&mut c, w),
        Page::Users => users(&mut c, w),
        Page::Summary => summary(&mut c, w),
        Page::Install => install(&mut c, w),
        Page::Finish => finish(&mut c, w),
    }
    bottom_bar(&mut c, w);
    c.img
}

fn sidebar(c: &mut Canvas, w: &Wizard) {
    let cx = SIDEBAR_W / 2;
    c.logo(cx, 92, 42);
    c.text_centered(cx, 152, "JUANITOS", 3, THEME.text);
    c.text_centered(cx, 186, "Installer", 2, THEME.text_muted);
    for (i, page) in PAGES.iter().enumerate() {
        let y = 250 + i as i32 * 46;
        let current = *page == w.page;
        let done = *page < w.page;
        if current {
            c.rounded(22, y - 12, SIDEBAR_W - 44, 40, 8, THEME.highlight);
        }
        let dot_y = y + 7;
        if current {
            c.circle(52, dot_y, 7, THEME.accent);
        } else if done {
            c.circle(52, dot_y, 7, THEME.text_muted);
        } else {
            c.ring(52, dot_y, 7, THEME.dim);
        }
        let colour = if current {
            THEME.accent
        } else if done {
            THEME.text
        } else {
            THEME.text_muted
        };
        c.text(76, y, page.label(), 2, colour);
    }
    let version = concat!("v", env!("CARGO_PKG_VERSION"));
    c.text_centered(
        cx,
        LOGICAL_H - 50,
        "FreeBSD + bhyve + ZFS",
        1,
        THEME.text_muted,
    );
    c.text_centered(cx, LOGICAL_H - 34, version, 1, THEME.text_muted);
}

fn title(c: &mut Canvas, title: &str, subtitle: &str) {
    c.text(CONTENT_X, 60, title, 4, THEME.text);
    c.text_clip(CONTENT_X, 112, subtitle, 2, CONTENT_W, THEME.text_muted);
}

fn paragraph(c: &mut Canvas, y: i32, lines: &[&str]) -> i32 {
    let mut y = y;
    for line in lines {
        c.text_clip(CONTENT_X, y, line, 2, CONTENT_W, THEME.text);
        y += 26;
    }
    y
}

/// Greedy word wrap to `max_chars` per line.
pub fn wrap(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > max_chars {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn welcome(c: &mut Canvas) {
    title(
        c,
        "Welcome to JuanitOS",
        "FreeBSD and bhyve, compartmentalised like QubesOS.",
    );
    let y = paragraph(
        c,
        BODY_Y,
        &[
            "This installer puts JuanitOS on a disk of this computer:",
            "",
            "  - the dom0: FreeBSD, ZFS, bhyve and the jos tools;",
            "  - every workload then runs in its own qube (a bhyve VM);",
            "  - sys-net and sys-firewall keep the network at arm's length;",
            "  - a Linux gpu qube can own the graphics card for CUDA.",
            "",
            "The live system you are running is exactly what gets installed.",
            "Nothing is written to disk before you confirm on the Summary page.",
        ],
    );
    let y = y + 24;
    c.text(CONTENT_X, y, "Keyboard", 2, THEME.accent);
    let keys = [
        ("Tab / Up / Down", "move between fields"),
        ("Enter / Space", "select, tick, press a button"),
        ("Esc", "previous page"),
    ];
    for (i, (k, what)) in keys.iter().enumerate() {
        let ky = y + 30 + i as i32 * 26;
        c.text(CONTENT_X + 20, ky, k, 2, THEME.text);
        c.text(CONTENT_X + 260, ky, what, 2, THEME.text_muted);
    }
}

fn status_pill(c: &mut Canvas, x: i32, y: i32, status: Status) {
    let (label, colour) = match status {
        Status::Ok => ("OK", OK),
        Status::Warn => ("WARN", THEME.accent),
        Status::Fail => ("FAIL", FAIL),
    };
    c.rounded(x, y - 6, 70, 26, 6, THEME.highlight);
    c.text_centered(x + 35, y, label, 2, colour);
}

fn hardware(c: &mut Canvas, w: &Wizard) {
    title(
        c,
        "Hardware check",
        "The checks of `jos doctor`: can this machine run qubes?",
    );
    let mut y = BODY_Y;
    for check in w.checks.iter().take(12) {
        status_pill(c, CONTENT_X, y, check.status);
        c.text_clip(CONTENT_X + 90, y, &check.name, 2, 300, THEME.text);
        c.text_clip(
            CONTENT_X + 410,
            y,
            &check.detail,
            2,
            CONTENT_W - 410,
            THEME.text_muted,
        );
        y += 34;
    }
    y += 16;
    let (fails, warns) = w.check_counts();
    if fails == 0 {
        let msg = if warns == 0 {
            "All checks passed: this hardware can run JuanitOS."
        } else {
            "No blocking problem. Warnings only matter for PCI passthrough."
        };
        c.text_clip(CONTENT_X, y, msg, 2, CONTENT_W, OK);
    } else {
        c.text_clip(
            CONTENT_X,
            y,
            "Some checks failed: enable VT-x/AMD-V and VT-d/AMD-Vi in the firmware.",
            2,
            CONTENT_W,
            FAIL,
        );
        checkbox(
            c,
            w,
            Widget::AcceptChecks,
            CONTENT_X,
            y + 40,
            "Install anyway (qubes may not start on this machine)",
        );
    }
}

fn disk(c: &mut Canvas, w: &Wizard) {
    title(
        c,
        "Installation disk",
        "The whole disk is used: GPT (UEFI + BIOS boot), swap and a ZFS pool.",
    );
    if w.disks.is_empty() {
        c.text(
            CONTENT_X,
            BODY_Y,
            "No disk was found on this machine.",
            2,
            FAIL,
        );
        return;
    }
    let focused = w.focused() == Some(Widget::Disks);
    for (i, d) in w.disks.iter().enumerate().take(8) {
        let y = BODY_Y + i as i32 * 64;
        let selected = i == w.disk_index;
        let border = if selected && focused {
            THEME.accent
        } else if selected {
            THEME.text_muted
        } else {
            THEME.highlight
        };
        c.boxed(CONTENT_X, y, CONTENT_W, 54, border, THEME.highlight);
        if selected {
            c.circle(CONTENT_X + 28, y + 27, 8, THEME.accent);
        } else {
            c.ring(CONTENT_X + 28, y + 27, 8, THEME.text_muted);
        }
        c.text(CONTENT_X + 54, y + 9, &d.name, 2, THEME.text);
        let size = format!("{:.1} GiB", d.size_gib());
        c.text(CONTENT_X + 180, y + 9, &size, 2, THEME.text);
        let descr = if d.description.is_empty() {
            "(no description)"
        } else {
            &d.description
        };
        c.text_clip(
            CONTENT_X + 54,
            y + 31,
            descr,
            2,
            CONTENT_W - 80,
            THEME.text_muted,
        );
    }
    if let Some(d) = w.selected_disk() {
        let y = BODY_Y + w.disks.len().min(8) as i32 * 64 + 20;
        let warn = format!("Everything on /dev/{} will be erased.", d.name);
        c.text(CONTENT_X, y, &warn, 2, THEME.accent);
    }
}

fn field(c: &mut Canvas, w: &Wizard, widget: Widget, x: i32, y: i32, label: &str, note: &str) {
    let focused = w.focused() == Some(widget);
    c.text(
        x,
        y,
        label,
        2,
        if focused { THEME.accent } else { THEME.text },
    );
    let by = y + 24;
    let border = if focused { THEME.accent } else { THEME.dim };
    c.boxed(x, by, FIELD_W, FIELD_H, border, THEME.highlight);
    if let Some(f) = w.field(widget) {
        let (shown, before) = if widget.is_password() {
            (
                "*".repeat(f.len()),
                "*".repeat(f.before_cursor().chars().count()),
            )
        } else {
            (f.value.clone(), f.before_cursor().to_string())
        };
        c.text_clip(x + 14, by + 15, &shown, 2, FIELD_W - 28, THEME.text);
        if focused {
            let cx = x + 14 + font::text_width(&before, 2) + 1;
            c.fill(cx.min(x + FIELD_W - 12), by + 11, 2, 22, THEME.accent);
        }
    }
    if !note.is_empty() {
        c.text_clip(
            x + FIELD_W + 20,
            by + 15,
            note,
            2,
            CONTENT_R - x - FIELD_W - 20,
            THEME.text_muted,
        );
    }
}

fn checkbox(c: &mut Canvas, w: &Wizard, widget: Widget, x: i32, y: i32, label: &str) {
    let focused = w.focused() == Some(widget);
    let border = if focused {
        THEME.accent
    } else {
        THEME.text_muted
    };
    c.boxed(x, y - 6, 26, 26, border, THEME.highlight);
    if w.checkbox(widget) {
        c.rounded(x + 6, y, 14, 14, 3, THEME.accent);
    }
    c.text_clip(
        x + 42,
        y,
        label,
        2,
        CONTENT_R - x - 42,
        if focused { THEME.accent } else { THEME.text },
    );
}

fn system(c: &mut Canvas, w: &Wizard) {
    title(
        c,
        "System",
        "Name this computer and choose what starts at boot.",
    );
    field(
        c,
        w,
        Widget::Hostname,
        CONTENT_X,
        BODY_Y,
        "Hostname",
        "letters, digits, - and .",
    );
    field(
        c,
        w,
        Widget::Swap,
        CONTENT_X,
        BODY_Y + 100,
        "Swap (GiB)",
        "0 for none",
    );
    checkbox(
        c,
        w,
        Widget::Desktop,
        CONTENT_X,
        BODY_Y + 210,
        "Start the COSMIC desktop at boot",
    );
    paragraph(
        c,
        BODY_Y + 260,
        &[
            "Without the desktop, dom0 boots to the text console and qubes",
            "are managed with the jos command.",
        ],
    );
}

fn users(c: &mut Canvas, w: &Wizard) {
    title(
        c,
        "Users",
        "dom0 is the trusted domain: keep its passwords strong.",
    );
    let col2 = CONTENT_X;
    field(c, w, Widget::RootPw, col2, BODY_Y, "Root password", "");
    field(c, w, Widget::RootPw2, col2, BODY_Y + 80, "Repeat", "");
    field(
        c,
        w,
        Widget::UserName,
        col2,
        BODY_Y + 180,
        "User name (optional)",
        "groups wheel, video",
    );
    field(c, w, Widget::UserPw, col2, BODY_Y + 260, "Password", "");
    field(c, w, Widget::UserPw2, col2, BODY_Y + 340, "Repeat", "");
}

fn summary(c: &mut Canvas, w: &Wizard) {
    title(
        c,
        "Summary",
        "Check the plan. Nothing has been written to disk yet.",
    );
    let mut y = BODY_Y;
    for (k, v) in w.summary_rows() {
        c.text(CONTENT_X, y, k, 2, THEME.text_muted);
        c.text_clip(CONTENT_X + 160, y, &v, 2, CONTENT_W - 160, THEME.text);
        y += 34;
    }
    y += 20;
    let disk = w
        .selected_disk()
        .map(|d| d.name.clone())
        .unwrap_or_default();
    c.boxed(CONTENT_X, y, CONTENT_W, 50, FAIL, THEME.highlight);
    let warn = format!("ALL DATA ON /dev/{disk} WILL BE ERASED");
    c.text_centered(CONTENT_X + CONTENT_W / 2, y + 18, &warn, 2, FAIL);
    checkbox(
        c,
        w,
        Widget::ConfirmErase,
        CONTENT_X,
        y + 86,
        "I understand: erase the disk and install JuanitOS",
    );
}

fn progress_bar(c: &mut Canvas, y: i32, permille: u32) {
    c.rounded(CONTENT_X, y, CONTENT_W, 20, 10, THEME.dim);
    let done = (CONTENT_W as i64 * permille.min(1000) as i64 / 1000) as i32;
    if done > 0 {
        c.rounded(CONTENT_X, y, done.max(20), 20, 10, THEME.accent);
    }
}

fn install(c: &mut Canvas, w: &Wizard) {
    let (index, total, step) = match &w.install {
        InstallState::Running {
            index,
            total,
            title,
        } => (*index, *total, title.clone()),
        _ => (0, 0, String::new()),
    };
    title(c, "Installing", &step);
    progress_bar(c, BODY_Y + 10, w.progress_permille());
    let counter = format!("Step {} of {}", (index + 1).min(total.max(1)), total.max(1));
    c.text(CONTENT_X, BODY_Y + 50, &counter, 2, THEME.text_muted);
    let mut y = BODY_Y + 100;
    for done in w.log.iter().rev().take(10).rev() {
        c.circle(CONTENT_X + 8, y + 7, 6, OK);
        c.text_clip(CONTENT_X + 28, y, done, 2, CONTENT_W - 28, THEME.text);
        y += 30;
    }
    if !step.is_empty() {
        c.ring(CONTENT_X + 8, y + 7, 7, THEME.accent);
        c.text_clip(CONTENT_X + 28, y, &step, 2, CONTENT_W - 28, THEME.accent);
    }
    c.text(
        CONTENT_X,
        BAR_Y - 40,
        "Extracting the dom0 takes a few minutes. Please do not power off.",
        2,
        THEME.text_muted,
    );
}

fn finish(c: &mut Canvas, w: &Wizard) {
    match &w.install {
        InstallState::Failed(e) => {
            title(
                c,
                "Installation failed",
                "The disk may be partially written; nothing else was changed.",
            );
            let mut y = BODY_Y;
            for line in wrap(e, 70).iter().take(12) {
                c.text(CONTENT_X, y, line, 2, FAIL);
                y += 26;
            }
            paragraph(
                c,
                y + 20,
                &[
                    "Exit to the console to investigate (the full output is there),",
                    "then run jos-install again.",
                ],
            );
        }
        _ => {
            title(
                c,
                "JuanitOS is installed",
                "Remove the installation medium, then reboot.",
            );
            c.logo(CONTENT_X + CONTENT_W / 2, BODY_Y + 120, 80);
            paragraph(
                c,
                BODY_Y + 250,
                &[
                    "After the reboot, build a template and your first qube:",
                    "",
                    "  jos template install debian-13-dev    (Docker)",
                    "  jos template install fbsd15-dev       (Podman)",
                    "  jos create work --kind app --os linux --template debian-13-dev",
                    "",
                    "jos doctor's report was saved in",
                    "/usr/local/etc/juanitos/install-report.txt.",
                ],
            );
        }
    }
}

fn button(c: &mut Canvas, x: i32, label: &str, focused: bool, primary: bool, enabled: bool) {
    let y = BAR_Y + 24;
    let (fill, text) = match (focused, primary, enabled) {
        (_, _, false) => (THEME.highlight, THEME.dim),
        (true, true, true) => (THEME.accent, THEME.background),
        (true, false, true) => (THEME.text_muted, THEME.background),
        (false, true, true) => (THEME.highlight, THEME.accent),
        (false, false, true) => (THEME.highlight, THEME.text),
    };
    c.rounded(x, y, BUTTON_W, BUTTON_H, 8, fill);
    c.text_centered(x + BUTTON_W / 2, y + 17, label, 2, text);
}

fn bottom_bar(c: &mut Canvas, w: &Wizard) {
    c.fill(CONTENT_X, BAR_Y, CONTENT_W, 2, THEME.highlight);
    if let Some(e) = &w.error {
        c.text_clip(CONTENT_X, BAR_Y - 36, e, 2, CONTENT_W, FAIL);
    }
    let widgets = w.widgets();
    let buttons: Vec<Widget> = widgets.iter().copied().filter(|b| b.is_button()).collect();
    let n = buttons.len() as i32;
    for (i, b) in buttons.iter().enumerate() {
        let x = CONTENT_R - (n - i as i32) * (BUTTON_W + 16) + 16;
        let (label, primary, enabled) = match b {
            Widget::Next => (w.next_label(), true, w.can_advance()),
            Widget::Back => ("Back", false, true),
            Widget::Quit => ("Quit", false, true),
            Widget::Console => ("Exit to console", false, true),
            Widget::Reboot => ("Reboot", true, true),
            _ => ("", false, false),
        };
        button(c, x, label, w.focused() == Some(*b), primary, enabled);
    }
    if !widgets.is_empty() {
        c.text(
            CONTENT_X,
            BAR_Y + 40,
            "Tab: move   Enter: select   Esc: back",
            2,
            THEME.text_muted,
        );
    }
}

/// Render every page with sample data into `dir` (PNG), for the
/// documentation and for eyeballing the layout on any host.
pub fn screenshots(dir: &std::path::Path, width: u32, height: u32) -> anyhow::Result<Vec<String>> {
    use jos_fb::keys::Key;

    std::fs::create_dir_all(dir)?;
    let mut w = sample_wizard();
    let mut written = Vec::new();
    let mut shot = |w: &Wizard, name: &str| -> anyhow::Result<()> {
        let file = dir.join(format!("{name}.png"));
        jos_gfx::png::write(&render(w, width, height), &file)?;
        written.push(file.display().to_string());
        Ok(())
    };
    shot(&w, "installer-1-welcome")?;
    w.handle(Key::Enter);
    shot(&w, "installer-2-hardware")?;
    w.handle(Key::Enter);
    w.handle(Key::Down);
    shot(&w, "installer-3-disk")?;
    w.handle(Key::Enter);
    shot(&w, "installer-4-system")?;
    for _ in 0..4 {
        w.handle(Key::Tab);
    }
    w.handle(Key::Enter);
    for (text, enter) in [
        ("secret", true),
        ("secret", true),
        ("juan", true),
        ("pw", false),
    ] {
        for ch in text.chars() {
            w.handle(Key::Char(ch));
        }
        if enter {
            w.handle(Key::Enter);
        }
    }
    shot(&w, "installer-5-users")?;
    w.handle(Key::Enter);
    for ch in "pw".chars() {
        w.handle(Key::Char(ch));
    }
    w.handle(Key::Enter);
    w.handle(Key::Char(' '));
    shot(&w, "installer-6-summary")?;
    w.handle(Key::Tab);
    w.handle(Key::Tab);
    w.handle(Key::Enter);
    for (i, t) in crate::install::STEP_TITLES.iter().enumerate().take(4) {
        w.on_progress(crate::install::Progress::Step {
            index: i,
            total: crate::install::STEP_TITLES.len(),
            title: (*t).to_string(),
        });
    }
    shot(&w, "installer-7-install")?;
    w.finished(Ok(()));
    shot(&w, "installer-8-finish")?;
    Ok(written)
}

/// A wizard filled with plausible hardware, for screenshots and tests.
pub fn sample_wizard() -> Wizard {
    use crate::disks::Disk;
    use juanitos_core::doctor::Check;
    let check = |name: &str, status, detail: &str| Check {
        name: name.into(),
        status,
        detail: detail.into(),
    };
    Wizard::new(
        crate::install::InstallPlan::default(),
        vec![
            check("kernel module vmm", Status::Ok, "loaded"),
            check("kernel module nmdm", Status::Ok, "loaded"),
            check("kernel module if_bridge", Status::Ok, "loaded"),
            check("kernel module if_tap", Status::Ok, "loaded"),
            check("iommu", Status::Ok, "hw.vmm.iommu.enable=1"),
            check(
                "/dev/vmm",
                Status::Warn,
                "appears after the first VM is created",
            ),
            check(
                "uefi firmware",
                Status::Ok,
                "/usr/local/share/uefi-firmware/BHYVE_UEFI.fd",
            ),
            check("ppt devices", Status::Ok, "1/0/0 1/0/1"),
        ],
        vec![
            Disk {
                name: "nda0".into(),
                size_bytes: 1_000_204_886_016,
                description: "Samsung SSD 980 PRO 1TB".into(),
            },
            Disk {
                name: "ada0".into(),
                size_bytes: 2_000_398_934_016,
                description: "WDC WD20EZBX-00AYRA0".into(),
            },
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use jos_fb::keys::Key;

    fn differs(a: &Image, b: &Image) -> bool {
        a.pixels() != b.pixels()
    }

    #[test]
    fn every_page_renders_and_pages_differ() {
        let mut w = sample_wizard();
        let mut frames = Vec::new();
        frames.push(render(&w, 1280, 800));
        w.handle(Key::Enter);
        frames.push(render(&w, 1280, 800));
        w.handle(Key::Enter);
        frames.push(render(&w, 1280, 800));
        for pair in frames.windows(2) {
            assert!(differs(&pair[0], &pair[1]));
        }
        for f in &frames {
            assert_eq!((f.width(), f.height()), (1280, 800));
        }
    }

    #[test]
    fn scales_and_centres_on_big_screens() {
        let w = sample_wizard();
        let small = Canvas::new(1024, 600);
        assert_eq!((small.s, small.ox, small.oy), (1, -128, -100));
        let big = Canvas::new(2560, 1600);
        assert_eq!((big.s, big.ox, big.oy), (2, 0, 0));
        let wide = Canvas::new(3000, 1600);
        assert_eq!((wide.s, wide.ox), (2, 220));
        let img = render(&w, 2560, 1600);
        // The accent-coloured current step marker is drawn twice as far.
        assert_eq!((img.width(), img.height()), (2560, 1600));
        // Rendering on a tiny screen clips instead of panicking.
        let tiny = render(&w, 64, 48);
        assert_eq!(tiny.width(), 64);
    }

    #[test]
    fn typing_changes_the_frame_but_passwords_are_masked() {
        let mut w = sample_wizard();
        w.page = crate::wizard::Page::Users;
        w.focus = 0;
        let before = render(&w, 1280, 800);
        w.handle(Key::Char('a'));
        let a = render(&w, 1280, 800);
        assert!(differs(&before, &a));
        // Another letter of the same length renders the same: only '*'.
        w.handle(Key::Backspace);
        w.handle(Key::Char('b'));
        let b = render(&w, 1280, 800);
        assert!(!differs(&a, &b));
    }

    #[test]
    fn error_and_failure_states_render() {
        let mut w = sample_wizard();
        w.error = Some("Something is wrong".into());
        let with_error = render(&w, 1280, 800);
        w.error = None;
        assert!(differs(&with_error, &render(&w, 1280, 800)));
        w.finished(Err("zpool create: no such device ".repeat(10)));
        let failed = render(&w, 1280, 800);
        w.finished(Ok(()));
        assert!(differs(&failed, &render(&w, 1280, 800)));
        // Disk page without disks, hardware page with failures.
        let mut empty = Wizard::new(Default::default(), vec![], vec![]);
        empty.page = Page::Disk;
        let _ = render(&empty, 1280, 800);
        let mut bad = sample_wizard();
        bad.checks[0].status = Status::Fail;
        bad.page = Page::Hardware;
        bad.focus = 0;
        let _ = render(&bad, 1280, 800);
        bad.checks.iter_mut().for_each(|c| c.status = Status::Ok);
        let _ = render(&bad, 1280, 800);
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("", 10), Vec::<String>::new());
        assert_eq!(wrap("a bb ccc dddd", 6), vec!["a bb", "ccc", "dddd"]);
        assert_eq!(wrap("toolongword x", 4), vec!["toolongword", "x"]);
    }

    #[test]
    fn screenshots_cover_every_page() {
        let dir = std::env::temp_dir().join(format!("jos-install-shots-{}", std::process::id()));
        let files = screenshots(&dir, 640, 400).unwrap();
        assert_eq!(files.len(), PAGES.len());
        for f in &files {
            assert!(std::fs::metadata(f).unwrap().len() > 100, "{f}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
