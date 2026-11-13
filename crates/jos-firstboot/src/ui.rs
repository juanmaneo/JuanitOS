// SPDX-License-Identifier: BSD-3-Clause
//! Drawing the first-boot wizard in the installer's layout and palette:
//! the steps in a sidebar on the left, the page on the right, the buttons
//! at the bottom, so install → first boot → login look like one product.
//!
//! Everything is laid out on a logical 1280×800 canvas, scaled by an
//! integer factor and centred on bigger screens, and rendered into a
//! [`jos_gfx::Image`]: the same code produces the frames on the
//! framebuffer and the PNG screenshots in `docs/`.

use jos_gfx::color::THEME;
use jos_gfx::font;
use jos_gfx::icons;
use jos_gfx::image::{Image, Rect};
use jos_gfx::Rgba;
use juanitos_core::ifaces::{IfaceKind, NetIface};
use juanitos_core::pci::PciAddr;

use crate::wizard::{Page, RunState, Target, Widget, Wizard, PAGES};

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
/// Height of a list row, and the distance between two rows.
const ROW_H: i32 = 54;
const ROW_PITCH: i32 = 64;
/// Network cards shown at once; the list scrolls past that.
const IFACE_ROWS: usize = 4;
/// The Qubes page's two columns.
const HALF_W: i32 = (CONTENT_W - 30) / 2;
const RIGHT_X: i32 = CONTENT_X + HALF_W + 30;

const SIDEBAR_BG: Rgba = Rgba::rgb(0x0e, 0x11, 0x16);
const OK: Rgba = Rgba::rgb(0x4c, 0xc3, 0x7a);
const FAIL: Rgba = Rgba::rgb(0xff, 0x5c, 0x5c);

/// What a click can land on: the screen rectangles of the widgets of the
/// last rendered page, in drawing order.
#[derive(Debug, Clone, Default)]
pub struct Hits {
    regions: Vec<(Rect, Target)>,
}

impl Hits {
    /// What is under the screen pixel (x, y), if anything.
    pub fn target(&self, x: i32, y: i32) -> Option<Target> {
        self.regions
            .iter()
            .find(|(r, _)| (r.x..r.right()).contains(&x) && (r.y..r.bottom()).contains(&y))
            .map(|(_, t)| *t)
    }
}

/// Integer scale and offset mapping the logical canvas onto the screen.
struct Canvas {
    img: Image,
    s: i32,
    ox: i32,
    oy: i32,
    hits: Hits,
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
            hits: Hits::default(),
        }
    }

    /// Remember that the logical box (x, y, w, h) is `target`.
    fn hit(&mut self, x: i32, y: i32, w: i32, h: i32, target: Target) {
        let r = self.rect(x, y, w, h);
        self.hits.regions.push((r, target));
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
    render_page(w, width, height).0
}

/// [`render`], with where the page's widgets ended up on screen.
pub fn render_page(w: &Wizard, width: u32, height: u32) -> (Image, Hits) {
    let mut c = Canvas::new(width, height);
    // The sidebar runs the full height of the screen, also outside the
    // logical canvas.
    let side = Rect::new(0, 0, c.ox + SIDEBAR_W * c.s, height as i32);
    c.img.fill_rect(side, SIDEBAR_BG);
    sidebar(&mut c, w);
    match w.page {
        Page::Welcome => welcome(&mut c),
        Page::Network => network(&mut c, w),
        Page::Qubes => qubes(&mut c, w),
        Page::Summary => summary(&mut c, w),
        Page::Progress => progress(&mut c, w),
        Page::Done => done(&mut c, w),
    }
    bottom_bar(&mut c, w);
    (c.img, c.hits)
}

fn sidebar(c: &mut Canvas, w: &Wizard) {
    let cx = SIDEBAR_W / 2;
    c.logo(cx, 92, 42);
    c.text_centered(cx, 152, "JUANITOS", 3, THEME.text);
    c.text_centered(cx, 186, "First boot", 2, THEME.text_muted);
    for (i, page) in PAGES.iter().enumerate() {
        let y = 250 + i as i32 * 46;
        let dot_y = y + 7;
        let colour = if *page == w.page {
            c.rounded(22, y - 12, SIDEBAR_W - 44, 40, 8, THEME.highlight);
            c.circle(52, dot_y, 7, THEME.accent);
            THEME.accent
        } else if *page < w.page {
            c.circle(52, dot_y, 7, THEME.text_muted);
            THEME.text
        } else {
            c.ring(52, dot_y, 7, THEME.dim);
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
fn wrap(text: &str, max_chars: usize) -> Vec<String> {
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
        "Let's create your qubes before the first login.",
    );
    paragraph(
        c,
        BODY_Y,
        &[
            "Every workload runs in its own qube (a bhyve VM):",
            "",
            "  - sys-net owns the network card: the dom0 never gets an address;",
            "  - sys-firewall stands between sys-net and the app qubes;",
            "  - work, personal and untrusted reach the network through it;",
            "  - vault has no network at all.",
            "",
            "Templates whose image is on this disk are built from it. Anything",
            "else is downloaded by the qubes, through sys-net, never by the dom0.",
            "",
            "Skip to do it later from a terminal, with jos setup.",
        ],
    );
}

/// Border of a list row: the selection stands out, more so with focus.
fn row_border(selected: bool, focused: bool) -> Rgba {
    if selected && focused {
        THEME.accent
    } else if selected {
        THEME.text_muted
    } else {
        THEME.highlight
    }
}

/// A radio button: a dot when `on`, a ring otherwise.
fn radio(c: &mut Canvas, cx: i32, cy: i32, on: bool) {
    if on {
        c.circle(cx, cy, 8, THEME.accent);
    } else {
        c.ring(cx, cy, 8, THEME.text_muted);
    }
}

/// A tick box at (x, y), ticked when `on`.
fn tick_box(c: &mut Canvas, x: i32, y: i32, on: bool, border: Rgba) {
    c.boxed(x, y, 26, 26, border, THEME.highlight);
    if on {
        c.rounded(x + 6, y + 6, 14, 14, 3, THEME.accent);
    }
}

fn checkbox(c: &mut Canvas, w: &Wizard, widget: Widget, on: bool, y: i32, label: &str) {
    let focused = w.focused() == Some(widget);
    let (border, text) = if focused {
        (THEME.accent, THEME.accent)
    } else {
        (THEME.text_muted, THEME.text)
    };
    tick_box(c, CONTENT_X, y - 6, on, border);
    c.text_clip(CONTENT_X + 42, y, label, 2, CONTENT_W - 42, text);
    let label_w = font::text_width(label, 2).min(CONTENT_W - 42);
    c.hit(CONTENT_X, y - 6, 42 + label_w, 26, Target::Widget(widget));
}

/// What the cable says: `None` when the driver does not tell.
fn link_label(link: Option<bool>) -> Option<(&'static str, Rgba)> {
    match link {
        Some(true) => Some(("connected", OK)),
        Some(false) => Some(("no cable", THEME.text_muted)),
        None => None,
    }
}

/// How `sys-net` will get the selected card, when it cannot be passed
/// through.
fn network_note(w: &Wizard) -> &'static str {
    match w.selected_iface().map(|i| i.kind) {
        Some(IfaceKind::Wifi) => "Wi-Fi cannot be bridged to sys-net yet: pick an Ethernet card.",
        Some(IfaceKind::Pci(_)) => {
            "No IOMMU: sys-net is bridged to the card; the dom0 has no address on it."
        }
        Some(IfaceKind::Usb) => {
            "USB adapters are bridged to sys-net; the dom0 has no address on them."
        }
        Some(IfaceKind::Other) => "sys-net is bridged to this card; the dom0 has no address on it.",
        None => "",
    }
}

fn iface_row(c: &mut Canvas, y: i32, nic: &NetIface, selected: bool, focused: bool, i: usize) {
    let border = row_border(selected, focused);
    c.boxed(CONTENT_X, y, CONTENT_W, ROW_H, border, THEME.highlight);
    c.hit(CONTENT_X, y, CONTENT_W, ROW_H, Target::Iface(i));
    radio(c, CONTENT_X + 28, y + 27, selected);
    let text = if selected { THEME.accent } else { THEME.text };
    c.text_clip(CONTENT_X + 56, y + 18, &nic.name, 2, 110, text);
    c.text_clip(
        CONTENT_X + 180,
        y + 18,
        &nic.description,
        2,
        CONTENT_W - 360,
        THEME.text_muted,
    );
    if let Some((label, colour)) = link_label(nic.link) {
        c.text(CONTENT_R - 150, y + 18, label, 2, colour);
    }
}

fn network(c: &mut Canvas, w: &Wizard) {
    title(
        c,
        "Network",
        "The card sys-net takes. The dom0 itself stays offline.",
    );
    let mut y = BODY_Y;
    if w.ifaces.is_empty() {
        y = paragraph(
            c,
            y,
            &[
                "No network card found.",
                "Plug in a USB Ethernet adapter, then Rescan.",
            ],
        ) + 10;
    } else {
        let focused = w.focused() == Some(Widget::Ifaces);
        let first = w.iface.unwrap_or(0).saturating_sub(IFACE_ROWS - 1);
        for (i, nic) in w.ifaces.iter().enumerate().skip(first).take(IFACE_ROWS) {
            iface_row(c, y, nic, w.iface == Some(i), focused, i);
            y += ROW_PITCH;
        }
    }
    let rescan = w.focused() == Some(Widget::Rescan);
    button(
        c,
        (CONTENT_X, y),
        Widget::Rescan,
        "Rescan",
        rescan,
        false,
        true,
    );
    let y = y + BUTTON_H + 36;
    if w.can_passthrough() {
        checkbox(
            c,
            w,
            Widget::Passthrough,
            w.passthrough,
            y,
            "Pass the card through to sys-net (after a reboot)",
        );
    } else {
        c.text_clip(
            CONTENT_X,
            y,
            network_note(w),
            2,
            CONTENT_W,
            THEME.text_muted,
        );
    }
}

fn qubes(c: &mut Canvas, w: &Wizard) {
    title(
        c,
        "Qubes",
        "The app qubes to create, and the template they are cloned from.",
    );
    c.text(CONTENT_X, BODY_Y, "App qubes", 2, THEME.accent);
    c.text(RIGHT_X, BODY_Y, "Template", 2, THEME.accent);
    let apps_focused = w.focused() == Some(Widget::Apps);
    let mut y = BODY_Y + 36;
    for (i, (app, on)) in w.apps.iter().enumerate() {
        let cursor = i == w.app;
        let border = row_border(cursor && apps_focused, false);
        c.boxed(CONTENT_X, y, HALF_W, ROW_H, border, THEME.highlight);
        c.hit(CONTENT_X, y, HALF_W, ROW_H, Target::App(i));
        tick_box(c, CONTENT_X + 14, y + 14, *on, THEME.text_muted);
        c.text_clip(CONTENT_X + 56, y + 18, &app.name, 2, 160, THEME.text);
        let net = if app.network {
            "via sys-firewall"
        } else {
            "offline"
        };
        c.text_clip(
            CONTENT_X + 220,
            y + 18,
            net,
            2,
            HALF_W - 230,
            THEME.text_muted,
        );
        y += ROW_PITCH;
    }
    let gpu_y = y + 30;

    let templates_focused = w.focused() == Some(Widget::Templates);
    let mut y = BODY_Y + 36;
    for (i, t) in w.templates.iter().enumerate() {
        let selected = i == w.template;
        let border = row_border(selected, templates_focused);
        c.boxed(RIGHT_X, y, HALF_W, ROW_H, border, THEME.highlight);
        c.hit(RIGHT_X, y, HALF_W, ROW_H, Target::Template(i));
        radio(c, RIGHT_X + 28, y + 27, selected);
        c.text_clip(RIGHT_X + 56, y + 18, t.name, 2, 190, THEME.text);
        let (label, colour) = if t.local {
            ("on this disk", OK)
        } else {
            ("to download", THEME.text_muted)
        };
        c.text_clip(RIGHT_X + 260, y + 18, label, 2, HALF_W - 270, colour);
        y += ROW_PITCH;
    }
    if let Some(t) = w.templates.get(w.template) {
        for line in wrap(t.summary, 32).iter().take(4) {
            c.text(RIGHT_X, y + 6, line, 2, THEME.text_muted);
            y += 26;
        }
    }

    checkbox(
        c,
        w,
        Widget::Gpu,
        w.gpu,
        gpu_y,
        "A gpu qube for a reserved NVIDIA card (Debian, CUDA)",
    );
    c.text_clip(
        CONTENT_X,
        gpu_y + 40,
        "Without a reserved NVIDIA card there is no gpu qube either way.",
        2,
        CONTENT_W,
        THEME.text_muted,
    );
}

fn summary(c: &mut Canvas, w: &Wizard) {
    title(
        c,
        "Summary",
        "Check the plan. Nothing has been created yet.",
    );
    let mut y = BODY_Y;
    for (k, v) in w.summary_rows() {
        c.text(CONTENT_X, y, k, 2, THEME.text_muted);
        let lines = wrap(&v, 54);
        for (i, line) in lines.iter().take(3).enumerate() {
            c.text(CONTENT_X + 180, y + i as i32 * 28, line, 2, THEME.text);
        }
        y += 28 * lines.len().clamp(1, 3) as i32 + 12;
    }
    paragraph(
        c,
        y + 10,
        &[
            "sys-net and sys-firewall then start at every boot. Building the",
            "templates takes a few minutes.",
        ],
    );
}

fn progress_bar(c: &mut Canvas, y: i32, permille: u32) {
    c.rounded(CONTENT_X, y, CONTENT_W, 20, 10, THEME.dim);
    let done = (CONTENT_W as i64 * permille.min(1000) as i64 / 1000) as i32;
    if done > 0 {
        c.rounded(CONTENT_X, y, done.max(20), 20, 10, THEME.accent);
    }
}

fn progress(c: &mut Canvas, w: &Wizard) {
    let (index, total, step) = match &w.run {
        RunState::Running { index, total, step } => (*index, *total, step.as_str()),
        _ => (0, 0, ""),
    };
    title(c, "Creating the qubes", step);
    progress_bar(c, BODY_Y + 10, w.progress_permille());
    if total > 0 {
        let counter = format!("Step {index} of {total}");
        c.text(CONTENT_X, BODY_Y + 50, &counter, 2, THEME.text_muted);
    }
    let mut y = BODY_Y + 100;
    for done in w.log.iter().rev().take(8).rev() {
        c.circle(CONTENT_X + 8, y + 7, 6, OK);
        c.text_clip(CONTENT_X + 28, y, done, 2, CONTENT_W - 28, THEME.text);
        y += 30;
    }
    if !step.is_empty() {
        c.ring(CONTENT_X + 8, y + 7, 7, THEME.accent);
        c.text_clip(CONTENT_X + 28, y, step, 2, CONTENT_W - 28, THEME.accent);
    }
    c.text_clip(
        CONTENT_X,
        BAR_Y - 76,
        &w.detail,
        2,
        CONTENT_W,
        THEME.text_muted,
    );
    c.text(
        CONTENT_X,
        BAR_Y - 40,
        "A template takes a few minutes to build. Please do not power off.",
        2,
        THEME.text_muted,
    );
}

fn done(c: &mut Canvas, w: &Wizard) {
    if let RunState::Failed(e) = &w.run {
        title(
            c,
            "Setup failed",
            "What was created is kept: Retry goes on from there.",
        );
        let mut y = BODY_Y;
        for line in wrap(e, 70).iter().take(10) {
            c.text(CONTENT_X, y, line, 2, FAIL);
            y += 26;
        }
        paragraph(
            c,
            y + 20,
            &[
                "Later logs in: this wizard comes back at the next boot, and",
                "jos setup does the same from a terminal.",
            ],
        );
        return;
    }
    let subtitle = if w.reboot {
        "Reboot to hand the reserved devices to their qubes."
    } else {
        "sys-net and sys-firewall start at every boot."
    };
    title(c, "Your qubes are ready", subtitle);
    c.logo(CONTENT_X + CONTENT_W / 2, BODY_Y + 90, 70);
    paragraph(
        c,
        BODY_Y + 200,
        &[
            "From a terminal:",
            "",
            "  jos ls                 the qubes and their state",
            "  jos start work         start an app qube",
            "  jos run work uname -a  run a command in it",
            "  jos console work       its serial console",
            "",
            "jos setup can run again at any time: it only adds what is missing.",
        ],
    );
}

fn button(
    c: &mut Canvas,
    (x, y): (i32, i32),
    b: Widget,
    label: &str,
    focused: bool,
    primary: bool,
    enabled: bool,
) {
    c.hit(x, y, BUTTON_W, BUTTON_H, Target::Widget(b));
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
    // (widget, label, enabled) of the page's buttons; the last one is the
    // page's primary action.
    let buttons: Vec<(Widget, &str, bool)> = widgets
        .iter()
        .filter_map(|&b| match b {
            Widget::Next => Some((b, w.next_label(), w.can_advance())),
            _ => b.button_label().map(|label| (b, label, true)),
        })
        .collect();
    let n = buttons.len() as i32;
    for (i, (b, label, enabled)) in buttons.into_iter().enumerate() {
        let i = i as i32;
        let x = CONTENT_R - (n - i) * (BUTTON_W + 16) + 16;
        let focused = w.focused() == Some(b);
        button(c, (x, BAR_Y + 24), b, label, focused, i == n - 1, enabled);
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
    std::fs::create_dir_all(dir)?;
    sample_pages()
        .iter()
        .map(|(name, w)| {
            let file = dir.join(format!("{name}.png"));
            jos_gfx::png::write(&render(w, width, height), &file)?;
            Ok(file.display().to_string())
        })
        .collect()
}

/// [`sample_wizard`] on every page, walked through as a user would.
fn sample_pages() -> Vec<(&'static str, Wizard)> {
    use jos_fb::keys::Key;

    let mut w = sample_wizard();
    let mut pages = vec![("firstboot-1-welcome", w.clone())];
    w.handle(Key::Enter);
    pages.push(("firstboot-2-network", w.clone()));
    w.handle(Key::Enter);
    pages.push(("firstboot-3-qubes", w.clone()));
    w.handle(Key::Enter);
    pages.push(("firstboot-4-summary", w.clone()));
    w.handle(Key::Enter);
    for line in [
        "==> [1/9] install template fbsd15-net (provisioned bridged to ue0)",
        "==> [2/9] create sys-net (net qube, freebsd, from fbsd15-net, bridged to ue0, autostart)",
        "==> [3/9] start sys-net",
        "==> [4/9] wait for sys-net to reach the network",
        "waiting for sys-net: DHCP on ue0",
    ] {
        w.on_line(line);
    }
    pages.push(("firstboot-5-progress", w.clone()));
    w.finished(Ok(()));
    pages.push(("firstboot-6-done", w));
    pages
}

/// The wizard on a laptop without an IOMMU: its own Ethernet chip with no
/// cable, a USB adapter with one, and Wi-Fi.
pub fn sample_wizard() -> Wizard {
    let nic = |name: &str, kind: IfaceKind, link: Option<bool>, description: &str| NetIface {
        name: name.to_string(),
        mac: Some("00:a0:98:12:34:56".to_string()),
        link,
        kind,
        description: description.to_string(),
    };
    Wizard::new(
        vec![
            nic(
                "em0",
                IfaceKind::Pci(PciAddr::new(0, 31, 6)),
                Some(false),
                "PCI Ethernet controller",
            ),
            nic("ue0", IfaceKind::Usb, Some(true), "USB Ethernet adapter"),
            nic(
                "wlan0",
                IfaceKind::Wifi,
                None,
                "Wi-Fi (not supported as an uplink yet)",
            ),
        ],
        false,
        &["fbsd15-dev".to_string(), "fbsd15-net".to_string()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wizard::tests::nic;
    use jos_fb::keys::Key;

    fn differs(a: &Image, b: &Image) -> bool {
        a.pixels() != b.pixels()
    }

    fn frame(w: &Wizard) -> Image {
        render(w, 1280, 800)
    }

    #[test]
    fn every_page_renders_and_pages_differ() {
        let pages = sample_pages();
        assert_eq!(pages.len(), PAGES.len());
        let frames: Vec<Image> = pages.iter().map(|(_, w)| frame(w)).collect();
        for pair in frames.windows(2) {
            assert!(differs(&pair[0], &pair[1]));
        }
        for (f, (_, w)) in frames.iter().zip(&pages) {
            assert_eq!((f.width(), f.height()), (1280, 800));
            // Rendering is deterministic.
            assert!(!differs(f, &frame(w)));
        }
    }

    #[test]
    fn scales_and_centres_on_big_screens() {
        let small = Canvas::new(1024, 600);
        assert_eq!((small.s, small.ox, small.oy), (1, -128, -100));
        let big = Canvas::new(2560, 1600);
        assert_eq!((big.s, big.ox, big.oy), (2, 0, 0));
        let w = sample_wizard();
        let (img, hits) = render_page(&w, 2560, 1600);
        assert_eq!((img.width(), img.height()), (2560, 1600));
        assert_eq!(hits.target(2240, 1476), Some(Target::Widget(Widget::Next)));
        // A tiny screen clips instead of panicking.
        assert_eq!(render(&w, 64, 48).width(), 64);
    }

    #[test]
    fn clicks_find_the_widget_under_them() {
        let mut w = sample_wizard();
        // Welcome: Skip and Next at the bottom right.
        let (_, hits) = render_page(&w, 1280, 800);
        assert_eq!(hits.target(1120, 738), Some(Target::Widget(Widget::Next)));
        assert_eq!(hits.target(904, 738), Some(Target::Widget(Widget::Skip)));
        assert_eq!(hits.target(700, 640), None);
        assert_eq!(hits.target(10, 10), None);
        // Network: the cards, Rescan under them.
        w.handle(Key::Enter);
        let (_, hits) = render_page(&w, 1280, 800);
        assert_eq!(
            hits.target(CONTENT_X + 300, BODY_Y + ROW_PITCH + 20),
            Some(Target::Iface(1))
        );
        let rescan_y = BODY_Y + 3 * ROW_PITCH + 20;
        assert_eq!(
            hits.target(CONTENT_X + 20, rescan_y),
            Some(Target::Widget(Widget::Rescan))
        );
        // Qubes: the apps on the left, the templates on the right, the
        // gpu tick under the apps.
        w.handle(Key::Enter);
        let (_, hits) = render_page(&w, 1280, 800);
        let row = |n: i32| BODY_Y + 36 + n * ROW_PITCH + 20;
        assert_eq!(hits.target(CONTENT_X + 20, row(3)), Some(Target::App(3)));
        assert_eq!(hits.target(RIGHT_X + 20, row(1)), Some(Target::Template(1)));
        assert_eq!(
            hits.target(CONTENT_X + 60, BODY_Y + 36 + 4 * ROW_PITCH + 30),
            Some(Target::Widget(Widget::Gpu))
        );
    }

    #[test]
    fn the_network_page_follows_the_cards() {
        let mut w = sample_wizard();
        w.handle(Key::Enter);
        // ue0 (USB) is preselected: a note, no passthrough tick.
        let usb = frame(&w);
        assert!(network_note(&w).contains("USB"));
        // em0 without IOMMU: bridged.
        w.click(Target::Iface(0));
        assert!(network_note(&w).contains("No IOMMU"));
        let pci = frame(&w);
        assert!(differs(&usb, &pci));
        // Wi-Fi: refused, Next disabled, the message on a refused Next.
        w.click(Target::Iface(2));
        assert!(network_note(&w).contains("Wi-Fi"));
        let wifi = frame(&w);
        w.click(Target::Widget(Widget::Next));
        assert!(differs(&wifi, &frame(&w)), "the error shows");
        // With an IOMMU, the PCI card gets the passthrough tick.
        w.iommu = true;
        w.click(Target::Iface(0));
        let ticked = frame(&w);
        let (_, hits) = render_page(&w, 1280, 800);
        let tick_y = BODY_Y + 3 * ROW_PITCH + BUTTON_H + 36;
        assert_eq!(
            hits.target(CONTENT_X + 10, tick_y),
            Some(Target::Widget(Widget::Passthrough))
        );
        w.click(Target::Widget(Widget::Passthrough));
        assert!(differs(&ticked, &frame(&w)));
        // A card of another kind, cards with and without a link status.
        w.set_ifaces(vec![nic("vtnet0", IfaceKind::Other, None)]);
        assert!(network_note(&w).contains("this card"));
        let _ = frame(&w);
        // More cards than rows: the list scrolls with the selection.
        let many: Vec<NetIface> = (0..8)
            .map(|i| nic(&format!("ue{i}"), IfaceKind::Usb, Some(i % 2 == 0)))
            .collect();
        w.set_ifaces(many);
        w.focus = 0;
        let top = frame(&w);
        for _ in 0..7 {
            w.handle(Key::Down);
        }
        assert_eq!(w.iface, Some(7));
        assert!(differs(&top, &frame(&w)));
        // Only Wi-Fi: nothing selected, no note. No card at all.
        w.set_ifaces(vec![nic("wlan0", IfaceKind::Wifi, None)]);
        assert_eq!(w.iface, None);
        assert_eq!(network_note(&w), "");
        let _ = frame(&w);
        w.set_ifaces(Vec::new());
        assert_eq!(network_note(&w), "");
        let _ = frame(&w);
    }

    #[test]
    fn the_qubes_page_shows_ticks_and_the_focus() {
        let mut w = sample_wizard();
        w.page = crate::wizard::Page::Qubes;
        let before = frame(&w);
        w.click(Target::App(0));
        let unticked = frame(&w);
        assert!(differs(&before, &unticked));
        w.click(Target::Template(0));
        assert!(differs(&unticked, &frame(&w)));
        w.click(Target::Widget(Widget::Gpu));
        let _ = frame(&w);
        // No template recipe at all.
        w.templates.clear();
        let _ = frame(&w);
    }

    #[test]
    fn progress_and_the_end_pages() {
        let mut w = sample_wizard();
        w.page = crate::wizard::Page::Summary;
        w.handle(Key::Enter);
        // Before the first step: no counter.
        let planning = frame(&w);
        w.on_line("==> [1/2] start sys-net");
        let first = frame(&w);
        assert!(differs(&planning, &first));
        // A Progress page without a run (cannot happen) still renders.
        w.run = RunState::Idle;
        let _ = frame(&w);
        // Failed: the error, Later and Retry.
        w.finished(Err("sys-net: no image ".repeat(20)));
        let failed = frame(&w);
        // Succeeded, with and without a reboot.
        w.finished(Ok(()));
        let ok = frame(&w);
        assert!(differs(&failed, &ok));
        w.reboot = true;
        assert!(differs(&ok, &frame(&w)));
    }

    #[test]
    fn buttons_show_focus_and_state() {
        let mut w = sample_wizard();
        // Next focused; then Skip focused (Next not).
        let next = frame(&w);
        w.handle(Key::BackTab);
        assert!(differs(&next, &frame(&w)));
        // Rescan focused on the Network page.
        w.handle(Key::Tab);
        w.handle(Key::Enter);
        w.click(Target::Widget(Widget::Rescan));
        assert_eq!(w.focused(), Some(Widget::Rescan));
        let _ = frame(&w);
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("a bb ccc dddd", 6), ["a bb", "ccc", "dddd"]);
        assert!(wrap("   ", 10).is_empty());
        assert_eq!(wrap("toolongword x", 4), ["toolongword", "x"]);
        assert_eq!(link_label(None), None);
    }

    #[test]
    fn screenshots_cover_every_page() {
        let dir = std::env::temp_dir().join(format!("jos-firstboot-shots-{}", std::process::id()));
        let files = screenshots(&dir, 640, 400).unwrap();
        assert_eq!(files.len(), PAGES.len());
        for f in &files {
            assert!(std::fs::metadata(f).unwrap().len() > 100, "{f}");
        }
        // A directory in the way of a picture; a file in the way of the
        // directory.
        let first = dir.join("firstboot-1-welcome.png");
        std::fs::remove_file(&first).unwrap();
        std::fs::create_dir(&first).unwrap();
        assert!(screenshots(&dir, 640, 400).is_err());
        assert!(screenshots(&dir.join("firstboot-2-network.png"), 640, 400).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
