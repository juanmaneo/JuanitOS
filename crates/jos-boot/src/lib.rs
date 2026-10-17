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
//!
//! # When a quiet boot never finishes
//!
//! `RB_MUTE` also mutes the kernel console *input* (`cn_mute` in
//! `kern_cons.c`): a root mount failure leaves an invisible `mountroot>`
//! prompt that ignores the keyboard. So before chain-loading a muted
//! entry this program sets the non-volatile UEFI variable
//! [`PENDING_NAME`] (vendor [`PENDING_VENDOR`]); `rc.d/jos-bootok` deletes
//! it once the file systems are mounted. If it is still there on the next
//! start, the previous quiet boot died before `rc(8)`, and the default
//! becomes [`VERBOSE_ENTRY`] with a note on the card. Only userland ever
//! clears it, so a failing verbose boot stays verbose until one succeeds.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use jos_gfx::scene::Menu;
use jos_gfx::{Scene, Stage};
use uguid::{guid, Guid};

/// Where the installer and the ISO put FreeBSD's loader (see
/// `juanitos_core::dom0::ESP_LOADER`).
pub const LOADER_PATH: &str = "\\EFI\\FREEBSD\\LOADER.EFI";

/// How long the card is shown before the default entry boots.
pub const COUNTDOWN_MS: u64 = 3000;

/// Vendor GUID of the JuanitOS UEFI variables.
pub const PENDING_VENDOR: Guid = guid!("62c99c6c-b34c-4e15-9e6c-fe06ff08543b");

/// "A muted boot was started and has not reached `rc(8)` yet."
pub const PENDING_NAME: &str = "JuanitOSBootPending";

/// The same variable as `efivar(8)` names it (`GUID-Name`), used by
/// `freebsd/rc.d/jos-bootok`.
pub const PENDING_EFIVAR: &str = "62c99c6c-b34c-4e15-9e6c-fe06ff08543b-JuanitOSBootPending";

/// Shown under the icons when the previous boot did not finish.
pub const PENDING_NOTE: &str = "The last boot did not finish - booting verbose";

/// The entry booted by default after a quiet boot that never finished.
pub const VERBOSE_ENTRY: usize = 1;

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

/// The entry Enter, Esc and the timeout boot: verbose if the previous
/// quiet boot is still marked pending, the quiet one otherwise.
pub fn default_entry(pending: bool) -> usize {
    if pending {
        VERBOSE_ENTRY
    } else {
        DEFAULT_ENTRY
    }
}

/// Does booting `entry` mute the console (and so need the pending mark)?
pub fn marks_pending(entry: &Entry) -> bool {
    entry.action == Action::Chain && entry.args.split(' ').any(|w| w == "-m")
}

/// Arguments for a machine without a graphics output (serial-only servers,
/// `qemu -vga none`): there is nothing to draw the card on and no splash
/// to show, so `loader.efi` is chain-loaded straight away and keeps its
/// console — the firmware's text console, which is the serial port there.
pub const HEADLESS_ARGS: &str = "autoboot_delay=-1 beastie_disable=YES";

/// `EFI_CONSOLE_CONTROL_PROTOCOL`, from the EFI 1.x / EDK I era. Intel
/// Macs' firmware still implements it and starts applications in its
/// *graphics* mode, where everything written to `ConOut` is swallowed:
/// a text console stays invisible until someone selects text mode. rEFInd,
/// GRUB and OpenCore all switch it; FreeBSD's `loader.efi` does not, so
/// `jos-boot` sets the mode its child needs before chain-loading. Other
/// firmware does not install the protocol and the calls are skipped.
pub const CONSOLE_CONTROL_GUID: Guid = guid!("f42f7782-012e-4c12-9956-49f94304f721");

/// `EFI_CONSOLE_CONTROL_SCREEN_MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleMode {
    Text,
    Graphics,
}

impl ConsoleMode {
    /// The protocol's value for this mode.
    pub fn raw(self) -> u32 {
        match self {
            ConsoleMode::Text => 0,
            ConsoleMode::Graphics => 1,
        }
    }
}

/// The console mode `loader.efi` should start in with `args`: graphics
/// for the quiet boot (nothing is printed and the card stays up until the
/// kernel splash), text for everything that talks to the user on the
/// firmware console (verbose, single user, the loader menu, headless).
pub fn console_mode_for(args: &str) -> ConsoleMode {
    if args.split(' ').any(|w| w == "console=nullconsole") {
        ConsoleMode::Graphics
    } else {
        ConsoleMode::Text
    }
}

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
    /// What Esc chooses (see [`default_entry`]).
    pub default: usize,
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
        Self::with_default(DEFAULT_ENTRY)
    }

    /// A menu that starts on, and falls back to, `default`.
    pub fn with_default(default: usize) -> Self {
        let default = if default < ENTRIES.len() {
            default
        } else {
            DEFAULT_ENTRY
        };
        MenuState {
            selected: default,
            default,
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
            Key::Escape => MenuOutcome::Choose(self.default),
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
/// else; the kernel splash that follows lights the second one. After a
/// boot that never finished it says why the default is verbose.
pub fn countdown_scene(pending: bool) -> Scene {
    let mut scene = Scene::new();
    scene.reach(Stage::Boot);
    if pending {
        scene.status = Some(String::from(PENDING_NOTE));
    }
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
    fn console_mode_follows_what_the_loader_prints() {
        assert_eq!(
            alloc::format!("{CONSOLE_CONTROL_GUID}"),
            "f42f7782-012e-4c12-9956-49f94304f721"
        );
        assert_eq!(ConsoleMode::Text.raw(), 0);
        assert_eq!(ConsoleMode::Graphics.raw(), 1);
        assert_eq!(console_mode_for(ENTRIES[0].args), ConsoleMode::Graphics);
        for e in &ENTRIES[1..4] {
            assert_eq!(console_mode_for(e.args), ConsoleMode::Text, "{}", e.title);
        }
        assert_eq!(console_mode_for(HEADLESS_ARGS), ConsoleMode::Text);
        assert_eq!(console_mode_for(""), ConsoleMode::Text);
    }

    #[test]
    fn headless_boot_is_immediate_and_not_muted() {
        for needle in ["autoboot_delay=-1", "beastie_disable=YES"] {
            assert!(
                HEADLESS_ARGS.split(' ').any(|w| w == needle),
                "missing {needle}"
            );
        }
        // Output must reach the serial console: no RB_MUTE, no nullconsole.
        assert!(!HEADLESS_ARGS.split(' ').any(|w| w == "-m"));
        assert!(!HEADLESS_ARGS.contains("nullconsole"));
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
    fn pending_boot_defaults_to_verbose() {
        assert_eq!(default_entry(false), DEFAULT_ENTRY);
        assert_eq!(default_entry(true), VERBOSE_ENTRY);
        let verbose = ENTRIES[VERBOSE_ENTRY];
        assert!(verbose.args.split(' ').any(|w| w == "-v"));
        assert!(!marks_pending(&verbose));

        let mut m = MenuState::with_default(default_entry(true));
        assert_eq!(m.selected, VERBOSE_ENTRY);
        m.on_key(Key::Down);
        assert_eq!(m.on_key(Key::Escape), MenuOutcome::Choose(VERBOSE_ENTRY));
        assert_eq!(MenuState::with_default(99), MenuState::new());
    }

    #[test]
    fn only_muted_boots_are_marked() {
        assert!(marks_pending(&ENTRIES[DEFAULT_ENTRY]));
        for e in ENTRIES.iter().skip(1) {
            assert!(!marks_pending(e), "{}", e.title);
        }
        // "-m" must be a word, not a substring of another argument.
        let e = Entry {
            title: "x",
            args: "-v mode=-mx",
            action: Action::Chain,
        };
        assert!(!marks_pending(&e));
        let r = Entry {
            title: "x",
            args: "-m",
            action: Action::Reboot,
        };
        assert!(!marks_pending(&r));
    }

    #[test]
    fn pending_variable_names_agree() {
        let guid = alloc::format!("{PENDING_VENDOR}");
        assert_eq!(PENDING_EFIVAR, alloc::format!("{guid}-{PENDING_NAME}"));
        assert!(PENDING_NAME.is_ascii());
        // The rc.d script that clears the mark must use the same name.
        let rc = include_str!("../../../freebsd/rc.d/jos-bootok");
        assert!(rc.contains(PENDING_EFIVAR));
        assert!(rc.contains("REQUIRE: FILESYSTEMS"));
    }

    #[test]
    fn countdown_keys() {
        assert_eq!(on_countdown_key(Key::Enter), Countdown::BootDefault);
        assert_eq!(on_countdown_key(Key::Escape), Countdown::OpenMenu);
        assert_eq!(on_countdown_key(Key::Other), Countdown::OpenMenu);
    }

    #[test]
    fn scenes() {
        let c = countdown_scene(false);
        assert!(c.is_lit(Stage::Boot));
        assert!(!c.is_lit(Stage::Kernel));
        assert!(c.menu.is_none());
        assert!(c.status.is_none());
        let p = countdown_scene(true);
        assert_eq!(p.status.as_deref(), Some(PENDING_NOTE));
        assert!(p.is_lit(Stage::Boot) && !p.is_lit(Stage::Kernel));
        assert!(COUNTDOWN_MS >= 3000);
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
