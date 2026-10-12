//! The firmware-independent half of `jos-boot`: what the menu offers, how
//! keys move through it, and how the chosen entry becomes `loader.efi`
//! arguments. Everything here is plain data so it can be unit-tested on
//! the development host; `main.rs` wires it to UEFI.
//!
//! # How a quiet boot works (FreeBSD 15.1 `stand/efi/loader`)
//!
//! `loader.efi` splits its `LoadOptions` on spaces and feeds every word to
//! `boot_parse_arg`: `-m` sets `RB_MUTE`, `-v`/`-s` the usual flags, and
//! `name=value` becomes a loader environment variable. Three variables
//! make the loader go straight to the kernel without drawing anything:
//!
//! - `autoboot_delay=-1` — boot immediately, no countdown;
//! - `beastie_disable=YES` — never show the Lua menu;
//! - `console=nullconsole` — the loader's own output goes nowhere. This
//!   is applied *after* the firmware console was probed, so the loader
//!   still records the GOP framebuffer for the kernel (`MODINFOMD_EFI_FB`)
//!   and `bi_getboothowto` also derives `RB_MUTE` from it.
//!
//! With `RB_MUTE` the kernel's `vt(4)` paints the `splash=` PNG from
//! `loader.conf` instead of console text, and `jos-splash` takes over once
//! `rc(8)` runs. The loader does clear the screen once while it starts
//! (its console init erases the terminal area), so there is a short black
//! gap between this program and the kernel splash; the stock loader offers
//! no way around that.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use jos_gfx::scene::Menu;
use jos_gfx::{Scene, Stage};

/// Where the installer and the ISO put FreeBSD's loader (see
/// `juanitos_core::dom0::ESP_LOADER`).
pub const LOADER_PATH: &str = "\\EFI\\FREEBSD\\LOADER.EFI";

/// How long the card is shown before the default entry boots.
pub const COUNTDOWN_MS: u64 = 1500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Chain-load `loader.efi` with the entry's arguments.
    Chain,
    /// Cold reset.
    Reboot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub title: &'static str,
    pub args: &'static str,
    pub action: Action,
}

pub const ENTRIES: [Entry; 5] = [
    Entry {
        title: "Boot JuanitOS",
        args: "-m autoboot_delay=-1 beastie_disable=YES console=nullconsole",
        action: Action::Chain,
    },
    Entry {
        title: "Boot verbose",
        args: "-v autoboot_delay=-1 beastie_disable=YES",
        action: Action::Chain,
    },
    Entry {
        title: "Single user",
        args: "-s autoboot_delay=-1 beastie_disable=YES",
        action: Action::Chain,
    },
    Entry {
        title: "FreeBSD loader menu",
        args: "",
        action: Action::Chain,
    },
    Entry {
        title: "Reboot",
        args: "",
        action: Action::Reboot,
    },
];

pub const DEFAULT_ENTRY: usize = 0;

/// Keys the pre-loader reacts to, already decoded from UEFI input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Escape,
    Other,
}

/// What the countdown screen does with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Countdown {
    /// Boot the default entry now.
    BootDefault,
    /// Open the menu.
    OpenMenu,
}

pub fn on_countdown_key(key: Key) -> Countdown {
    match key {
        Key::Enter => Countdown::BootDefault,
        _ => Countdown::OpenMenu,
    }
}

/// The menu: a selected index over [`ENTRIES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuState {
    pub selected: usize,
}

/// What the menu asks the caller to do after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuOutcome {
    Redraw,
    Nothing,
    Choose(usize),
}

impl Default for MenuState {
    fn default() -> Self {
        Self::new()
    }
}

impl MenuState {
    pub fn new() -> Self {
        MenuState {
            selected: DEFAULT_ENTRY,
        }
    }

    pub fn on_key(&mut self, key: Key) -> MenuOutcome {
        match key {
            Key::Up => {
                self.selected = self.selected.checked_sub(1).unwrap_or(ENTRIES.len() - 1);
                MenuOutcome::Redraw
            }
            Key::Down => {
                self.selected = (self.selected + 1) % ENTRIES.len();
                MenuOutcome::Redraw
            }
            Key::Enter => MenuOutcome::Choose(self.selected),
            Key::Escape => MenuOutcome::Choose(DEFAULT_ENTRY),
            Key::Other => MenuOutcome::Nothing,
        }
    }

    pub fn entry(&self) -> Entry {
        ENTRIES
            .get(self.selected)
            .copied()
            .unwrap_or(ENTRIES[DEFAULT_ENTRY])
    }

    /// The card to draw for this menu state.
    pub fn scene(&self) -> Scene {
        let mut scene = Scene::new();
        scene.reach(Stage::Boot);
        scene.menu = Some(Menu {
            items: ENTRIES.iter().map(|e| String::from(e.title)).collect(),
            selected: self.selected,
            hint: String::from("Up/Down select   Enter boot   Esc default"),
        });
        scene
    }
}

/// The card shown during the countdown: the first stage is lit, nothing
/// else; the kernel splash that follows lights the second one.
pub fn countdown_scene() -> Scene {
    let mut scene = Scene::new();
    scene.reach(Stage::Boot);
    scene
}

/// Does `(w, h)` look like a monitor (4:3 … 21:9, landscape) rather than
/// a synthetic firmware mode such as 2048x2048 or 320x200?
pub fn is_display_like((w, h): (usize, usize)) -> bool {
    if w < 800 || h < 600 || w < h {
        return false;
    }
    // 1.2 <= w/h <= 2.5 without floats.
    w * 10 >= h * 12 && w * 10 <= h * 25
}

/// Rank for choosing among display-like modes: area, with a cap so a
/// 4K panel does not lose to a 5K oddity and the card stays readable.
pub fn mode_score((w, h): (usize, usize)) -> usize {
    (w * h).min(3840 * 2160)
}

/// Encode loader arguments as the NUL-terminated UCS-2 string
/// `LoadOptions` expects. Non-BMP characters cannot occur in our
/// constants; they are replaced if they ever do.
pub fn load_options(args: &str) -> Vec<u16> {
    let mut out: Vec<u16> = args
        .chars()
        .map(|c| u16::try_from(u32::from(c)).unwrap_or(u16::from(b'?')))
        .collect();
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_entry_boots_quietly() {
        let e = ENTRIES[DEFAULT_ENTRY];
        assert_eq!(e.action, Action::Chain);
        for needle in [
            "-m",
            "autoboot_delay=-1",
            "beastie_disable=YES",
            "console=nullconsole",
        ] {
            assert!(e.args.split(' ').any(|w| w == needle), "missing {needle}");
        }
        // The loader splits on spaces and does not understand quotes.
        assert!(!e.args.contains('"'));
    }

    #[test]
    fn other_entries_keep_the_console() {
        for e in ENTRIES.iter().skip(1) {
            assert!(!e.args.contains("nullconsole"), "{}", e.title);
            assert!(!e.args.contains("-m"), "{}", e.title);
        }
        assert_eq!(ENTRIES[3].args, "");
        assert_eq!(ENTRIES[4].action, Action::Reboot);
        // Everything that boots directly skips the countdown and the menu.
        for e in ENTRIES.iter().take(3) {
            assert!(e.args.contains("autoboot_delay=-1"), "{}", e.title);
            assert!(e.args.contains("beastie_disable=YES"), "{}", e.title);
        }
    }

    #[test]
    fn menu_wraps_and_chooses() {
        let mut m = MenuState::new();
        assert_eq!(m.on_key(Key::Up), MenuOutcome::Redraw);
        assert_eq!(m.selected, ENTRIES.len() - 1);
        assert_eq!(m.on_key(Key::Down), MenuOutcome::Redraw);
        assert_eq!(m.selected, 0);
        m.on_key(Key::Down);
        m.on_key(Key::Down);
        assert_eq!(m.on_key(Key::Enter), MenuOutcome::Choose(2));
        assert_eq!(m.entry().title, "Single user");
        assert_eq!(m.on_key(Key::Escape), MenuOutcome::Choose(DEFAULT_ENTRY));
        assert_eq!(m.on_key(Key::Other), MenuOutcome::Nothing);
    }

    #[test]
    fn countdown_keys() {
        assert_eq!(on_countdown_key(Key::Enter), Countdown::BootDefault);
        assert_eq!(on_countdown_key(Key::Escape), Countdown::OpenMenu);
        assert_eq!(on_countdown_key(Key::Other), Countdown::OpenMenu);
    }

    #[test]
    fn scenes() {
        let c = countdown_scene();
        assert!(c.is_lit(Stage::Boot));
        assert!(!c.is_lit(Stage::Kernel));
        assert!(c.menu.is_none());
        let m = MenuState::new().scene();
        let menu = m.menu.unwrap();
        assert_eq!(menu.items.len(), ENTRIES.len());
        assert_eq!(menu.selected, DEFAULT_ENTRY);
    }

    #[test]
    fn mode_selection_prefers_real_displays() {
        assert!(is_display_like((1920, 1080)));
        assert!(is_display_like((1024, 768)));
        assert!(is_display_like((2560, 1080)));
        assert!(!is_display_like((2048, 2048)));
        assert!(!is_display_like((640, 480)));
        assert!(!is_display_like((1080, 1920)));
        assert!(mode_score((1920, 1080)) > mode_score((1280, 800)));
        assert_eq!(mode_score((5120, 2880)), mode_score((3840, 2160)));
    }

    #[test]
    fn load_options_are_ucs2_with_nul() {
        assert_eq!(
            load_options("-m a=1"),
            alloc::vec![0x2d, 0x6d, 0x20, 0x61, 0x3d, 0x31, 0]
        );
        assert_eq!(load_options(""), alloc::vec![0]);
    }
}
