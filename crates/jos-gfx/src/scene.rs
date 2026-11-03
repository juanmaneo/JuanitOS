// SPDX-License-Identifier: BSD-3-Clause
//! Layout of the boot "card": what the three boot programs all draw.
//!
//! The card is a fixed-size picture centred on the screen. The kernel's
//! `vt(4)` centres the loader-provided splash PNG with
//! `(screen - image) / 2`, so [`card_origin`] uses the very same integer
//! formula and the picture does not move between the pre-loader, the
//! kernel and the splash daemon.

use alloc::string::String;
use alloc::vec::Vec;

use crate::color::THEME;
use crate::font;
use crate::icons::{self, Icon};
use crate::image::{Image, Rect};

pub const CARD_WIDTH: u32 = 720;
pub const CARD_HEIGHT: u32 = 420;

const LOGO_CY: i32 = 120;
const LOGO_RADIUS: i32 = 64;
const TITLE_Y: i32 = 210;
const ICON_SIZE: i32 = 44;
const ICON_PITCH: i32 = 88;
const ICON_Y: i32 = 270;
/// The progress bar under the icons, as wide as their row.
const BAR_Y: i32 = 340;
const BAR_H: i32 = 6;
const STATUS_Y: i32 = 362;
const TEXT_SCALE: i32 = 2;
const TITLE_SCALE: i32 = 3;

/// Boot stages, in order. Each lights one icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    Boot,
    Kernel,
    Storage,
    Network,
    Qubes,
    Desktop,
}

pub const STAGES: [Stage; 6] = [
    Stage::Boot,
    Stage::Kernel,
    Stage::Storage,
    Stage::Network,
    Stage::Qubes,
    Stage::Desktop,
];

impl Stage {
    /// Name used on the command line (`jos-splash stage <name>`).
    pub fn name(self) -> &'static str {
        match self {
            Stage::Boot => "boot",
            Stage::Kernel => "kernel",
            Stage::Storage => "storage",
            Stage::Network => "network",
            Stage::Qubes => "qubes",
            Stage::Desktop => "desktop",
        }
    }

    pub fn from_name(s: &str) -> Option<Stage> {
        STAGES.iter().copied().find(|st| st.name() == s)
    }

    pub fn icon(self) -> Icon {
        match self {
            Stage::Boot => Icon::Power,
            Stage::Kernel => Icon::Kernel,
            Stage::Storage => Icon::Storage,
            Stage::Network => Icon::Network,
            Stage::Qubes => Icon::Qubes,
            Stage::Desktop => Icon::Desktop,
        }
    }

    pub fn index(self) -> usize {
        STAGES.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// What to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scene {
    /// Stages reached so far (their icons are lit).
    pub lit: [bool; 6],
    /// The icons are going out, right to left (the shutdown): the status
    /// line names what is stopping rather than what is starting.
    pub shutting_down: bool,
    /// Optional one-line status under the icons; without one the step in
    /// progress is named ([`Scene::step_label`]).
    pub status: Option<String>,
    /// Optional menu (pre-loader): items and the selected index.
    pub menu: Option<Menu>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub items: Vec<String>,
    pub selected: usize,
    pub hint: String,
}

impl Default for Scene {
    fn default() -> Self {
        Scene::new()
    }
}

impl Scene {
    pub fn new() -> Self {
        Scene {
            lit: [false; 6],
            shutting_down: false,
            status: None,
            menu: None,
        }
    }

    /// How many icons are lit: `reach` and `leave` keep them a prefix of
    /// [`STAGES`].
    fn lit_count(&self) -> usize {
        self.lit.iter().filter(|lit| **lit).count()
    }

    /// The step in progress, for the status line when nothing more
    /// specific was said: the stage after the lit ones ("STARTING
    /// NETWORK"), or on the way down the last lit one going out
    /// ("STOPPING QUBES"; "SHUTTING DOWN" once only storage and the
    /// kernel's part are left). None when there is no step left.
    pub fn step_label(&self) -> Option<String> {
        let mut stages = STAGES.iter().zip(self.lit);
        let (verb, stage) = if self.shutting_down {
            match stages.rfind(|(_, lit)| *lit)?.0 {
                Stage::Boot | Stage::Kernel | Stage::Storage => {
                    return Some("SHUTTING DOWN".into())
                }
                stage => ("STOPPING", stage),
            }
        } else {
            ("STARTING", stages.find(|(_, lit)| !*lit)?.0)
        };
        Some(alloc::format!("{verb} {}", stage.name().to_uppercase()))
    }

    /// Light `stage` and every stage before it (a late notification must
    /// not leave an earlier icon dark).
    pub fn reach(&mut self, stage: Stage) {
        for (i, lit) in self.lit.iter_mut().enumerate() {
            if i <= stage.index() {
                *lit = true;
            }
        }
    }

    /// Turn `stage` and every later stage off: the shutdown runs the boot
    /// backwards.
    pub fn leave(&mut self, stage: Stage) {
        for (i, lit) in self.lit.iter_mut().enumerate() {
            if i >= stage.index() {
                *lit = false;
            }
        }
    }

    pub fn is_lit(&self, stage: Stage) -> bool {
        self.lit.get(stage.index()).copied().unwrap_or(false)
    }

    /// Render the card.
    pub fn render_card(&self) -> Image {
        let mut img = Image::new(CARD_WIDTH, CARD_HEIGHT, THEME.background);
        let cx = CARD_WIDTH as i32 / 2;

        icons::draw_logo(&mut img, cx, LOGO_CY, LOGO_RADIUS, THEME.text, THEME.accent);
        font::draw_text_centered(&mut img, cx, TITLE_Y, "JUANITOS", TITLE_SCALE, THEME.text);

        if let Some(menu) = &self.menu {
            self.render_menu(&mut img, menu);
            return img;
        }

        let row_w = row_width(STAGES.len());
        let x0 = cx - row_w / 2;
        for (i, stage) in STAGES.iter().enumerate() {
            let lit = self.is_lit(*stage);
            let colour = if lit { THEME.accent } else { THEME.dim };
            let x = x0 + i as i32 * ICON_PITCH;
            icons::draw_icon(
                &mut img,
                stage.icon(),
                x,
                ICON_Y,
                ICON_SIZE,
                colour,
                THEME.background,
            );
            if lit {
                // A small glow dot under a lit icon.
                img.fill_circle(
                    (x + ICON_SIZE / 2) as f32,
                    (ICON_Y + ICON_SIZE + 12) as f32,
                    3.0,
                    THEME.accent_soft,
                );
            }
        }

        // The progress bar reaches the last lit icon.
        img.rounded_rect(Rect::new(x0, BAR_Y, row_w, BAR_H), 3.0, THEME.highlight);
        let fill = row_width(self.lit_count());
        if fill > 0 {
            img.rounded_rect(Rect::new(x0, BAR_Y, fill, BAR_H), 3.0, THEME.accent);
        }

        if let Some(status) = self.status.clone().or_else(|| self.step_label()) {
            font::draw_text_centered(
                &mut img,
                cx,
                STATUS_Y,
                &status,
                TEXT_SCALE,
                THEME.text_muted,
            );
        }
        img
    }

    fn render_menu(&self, img: &mut Image, menu: &Menu) {
        let cx = CARD_WIDTH as i32 / 2;
        let line_h = font::text_height(TEXT_SCALE) + 14;
        let top = ICON_Y - 10;
        for (i, item) in menu.items.iter().enumerate() {
            let y = top + i as i32 * line_h;
            if i == menu.selected {
                let w = font::text_width(item, TEXT_SCALE) + 40;
                img.rounded_rect(
                    Rect::new(cx - w / 2, y - 7, w, line_h),
                    6.0,
                    THEME.highlight,
                );
                font::draw_text_centered(img, cx, y, item, TEXT_SCALE, THEME.accent);
            } else {
                font::draw_text_centered(img, cx, y, item, TEXT_SCALE, THEME.text);
            }
        }
        let hint_y = CARD_HEIGHT as i32 - font::text_height(1) - 12;
        font::draw_text_centered(img, cx, hint_y, &menu.hint, 1, THEME.text_muted);
    }
}

/// Width of a row of `n` icons, from the first one's left edge to the
/// last one's right edge (0 for none).
fn row_width(n: usize) -> i32 {
    match n as i32 {
        0 => 0,
        n => (n - 1) * ICON_PITCH + ICON_SIZE,
    }
}

/// A card with a message instead of the icons: the pre-loader's error
/// screen. `lines` are drawn small (scale 1) so a long firmware error fits;
/// each is clipped to the card width.
pub fn render_message_card(title: &str, lines: &[String]) -> Image {
    let mut img = Image::new(CARD_WIDTH, CARD_HEIGHT, THEME.background);
    let cx = CARD_WIDTH as i32 / 2;
    icons::draw_logo(&mut img, cx, LOGO_CY, LOGO_RADIUS, THEME.text, THEME.accent);
    font::draw_text_centered(&mut img, cx, TITLE_Y, title, TEXT_SCALE, THEME.accent);
    let line_h = font::text_height(1) + 6;
    let max_chars = (CARD_WIDTH as usize - 40) / font::text_width("M", 1).max(1) as usize;
    for (i, line) in lines.iter().enumerate() {
        let y = ICON_Y - 10 + i as i32 * line_h;
        if y + line_h > CARD_HEIGHT as i32 {
            break;
        }
        let shown: String = line.chars().take(max_chars).collect();
        font::draw_text_centered(&mut img, cx, y, &shown, 1, THEME.text);
    }
    img
}

/// Top-left corner of the card on a `screen_w`×`screen_h` display, with
/// the kernel's centring formula. Negative on screens smaller than the
/// card; callers clip.
pub fn card_origin(screen_w: u32, screen_h: u32) -> (i32, i32) {
    (
        (screen_w as i32 - CARD_WIDTH as i32) / 2,
        (screen_h as i32 - CARD_HEIGHT as i32) / 2,
    )
}

/// Render a full screen: background plus the centred card.
pub fn render_screen(scene: &Scene, screen_w: u32, screen_h: u32) -> Image {
    let mut img = Image::new(screen_w, screen_h, THEME.background);
    let (x, y) = card_origin(screen_w, screen_h);
    img.draw_image(&scene.render_card(), x, y);
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_names_roundtrip() {
        for s in STAGES {
            assert_eq!(Stage::from_name(s.name()), Some(s));
        }
        assert_eq!(Stage::from_name("bogus"), None);
        assert_eq!(Stage::Desktop.index(), 5);
    }

    #[test]
    fn reaching_a_stage_lights_earlier_ones() {
        let mut s = Scene::new();
        assert!(!s.is_lit(Stage::Boot));
        s.reach(Stage::Network);
        assert!(s.is_lit(Stage::Boot));
        assert!(s.is_lit(Stage::Storage));
        assert!(s.is_lit(Stage::Network));
        assert!(!s.is_lit(Stage::Qubes));
    }

    #[test]
    fn leaving_a_stage_darkens_later_ones() {
        let mut s = Scene::new();
        s.reach(Stage::Desktop);
        s.leave(Stage::Qubes);
        assert!(s.is_lit(Stage::Network));
        assert!(!s.is_lit(Stage::Qubes));
        assert!(!s.is_lit(Stage::Desktop));
        s.leave(Stage::Boot);
        assert_eq!(s.lit, [false; 6]);
    }

    #[test]
    fn card_origin_matches_kernel_centering() {
        assert_eq!(card_origin(1920, 1080), (600, 330));
        assert_eq!(card_origin(1024, 768), (152, 174));
        assert_eq!(card_origin(640, 400), (-40, -10));
    }

    #[test]
    fn card_renders_with_lit_icons_in_accent() {
        let mut s = Scene::new();
        s.reach(Stage::Kernel);
        let img = s.render_card();
        assert_eq!(img.width(), CARD_WIDTH);
        assert_eq!(img.height(), CARD_HEIGHT);
        let accent = img.pixels().iter().filter(|p| **p == THEME.accent).count();
        let dim = img.pixels().iter().filter(|p| **p == THEME.dim).count();
        assert!(accent > 500, "accent pixels: {accent}");
        assert!(dim > 500, "dim pixels: {dim}");

        // More lit stages -> more accent pixels.
        let mut all = Scene::new();
        all.reach(Stage::Desktop);
        let all_accent = all
            .render_card()
            .pixels()
            .iter()
            .filter(|p| **p == THEME.accent)
            .count();
        assert!(all_accent > accent);
    }

    /// Pixels of colour `c` in the rows of the progress bar.
    fn bar_pixels(img: &Image, c: crate::Rgba) -> usize {
        (BAR_Y..BAR_Y + BAR_H)
            .flat_map(|y| (0..CARD_WIDTH as i32).map(move |x| (x, y)))
            .filter(|&(x, y)| img.get(x, y) == Some(c))
            .count()
    }

    #[test]
    fn the_progress_bar_reaches_the_last_lit_icon() {
        let mut s = Scene::new();
        let none = s.render_card();
        assert_eq!(bar_pixels(&none, THEME.accent), 0);
        assert!(bar_pixels(&none, THEME.highlight) > 400 * BAR_H as usize);
        s.reach(Stage::Boot);
        let one = bar_pixels(&s.render_card(), THEME.accent);
        assert!(one > 0 && one <= (ICON_SIZE * BAR_H) as usize, "{one}");
        s.reach(Stage::Desktop);
        let all = bar_pixels(&s.render_card(), THEME.accent);
        assert!(all > 5 * one, "{all}");
        assert_eq!(row_width(0), 0);
        assert_eq!(row_width(1), ICON_SIZE);
        assert_eq!(row_width(6), 5 * ICON_PITCH + ICON_SIZE);
    }

    #[test]
    fn the_step_in_progress_is_named() {
        let mut s = Scene::new();
        assert_eq!(s.step_label().as_deref(), Some("STARTING BOOT"));
        s.reach(Stage::Storage);
        assert_eq!(s.step_label().as_deref(), Some("STARTING NETWORK"));
        s.reach(Stage::Qubes);
        assert_eq!(s.step_label().as_deref(), Some("STARTING DESKTOP"));
        s.reach(Stage::Desktop);
        assert_eq!(s.step_label(), None);

        s.shutting_down = true;
        assert_eq!(s.step_label().as_deref(), Some("STOPPING DESKTOP"));
        s.leave(Stage::Desktop);
        assert_eq!(s.step_label().as_deref(), Some("STOPPING QUBES"));
        s.leave(Stage::Qubes);
        assert_eq!(s.step_label().as_deref(), Some("STOPPING NETWORK"));
        s.leave(Stage::Network);
        assert_eq!(s.step_label().as_deref(), Some("SHUTTING DOWN"));
        s.leave(Stage::Kernel);
        assert_eq!(s.step_label().as_deref(), Some("SHUTTING DOWN"));
        s.leave(Stage::Boot);
        assert_eq!(s.step_label(), None);

        // Drawn in place of an absent status, never over a given one.
        let muted = |s: &Scene| {
            s.render_card()
                .pixels()
                .iter()
                .filter(|p| **p == THEME.text_muted)
                .count()
        };
        assert_eq!(muted(&s), 0);
        s.reach(Stage::Network);
        let labelled = muted(&s);
        assert!(labelled > 50);
        s.status = Some("HI".into());
        assert!(muted(&s) < labelled);
    }

    #[test]
    fn menu_replaces_icons_and_status_is_drawn() {
        let mut s = Scene::new();
        s.status = Some("STARTING NETWORK".into());
        let with_status = s.render_card();
        let muted = with_status
            .pixels()
            .iter()
            .filter(|p| **p == THEME.text_muted)
            .count();
        assert!(muted > 50);

        s.menu = Some(Menu {
            items: alloc::vec!["Boot JuanitOS".into(), "Reboot".into()],
            selected: 0,
            hint: "Arrows select, Enter boots".into(),
        });
        let with_menu = s.render_card();
        let highlight = with_menu
            .pixels()
            .iter()
            .filter(|p| **p == THEME.highlight)
            .count();
        assert!(highlight > 100);
        let dim = with_menu
            .pixels()
            .iter()
            .filter(|p| **p == THEME.dim)
            .count();
        assert_eq!(dim, 0, "icons must not be drawn behind a menu");
    }

    #[test]
    fn message_card_draws_title_and_lines() {
        let long = "x".repeat(400);
        let img = render_message_card(
            "BOOT FAILED",
            &[String::from("loader.efi: NOT_FOUND"), long],
        );
        let accent = img.pixels().iter().filter(|p| **p == THEME.accent).count();
        let text = img.pixels().iter().filter(|p| **p == THEME.text).count();
        assert!(accent > 100);
        assert!(text > 200);
        // More lines than the card holds: the rest is left out.
        let many: Vec<String> = (0..100).map(|i| std::format!("line {i}")).collect();
        let crowded = render_message_card("BOOT FAILED", &many);
        let few = render_message_card("BOOT FAILED", &many[..20]);
        assert_eq!(crowded.height(), few.height());
        assert_eq!(Scene::default(), Scene::new());
    }

    #[test]
    fn full_screen_embeds_card() {
        let s = Scene::new();
        let screen = render_screen(&s, 800, 600);
        assert_eq!(screen.width(), 800);
        let (x, y) = card_origin(800, 600);
        let card = s.render_card();
        assert_eq!(screen.get(x + 360, y + 120 - 60), card.get(360, 60));
        assert_eq!(screen.get(0, 0), Some(THEME.background));
    }
}
