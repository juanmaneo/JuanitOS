//! `jos-boot` — the JuanitOS graphical pre-loader.
//!
//! Installed as `EFI/BOOT/BOOTX64.EFI`, it paints the boot card on the
//! firmware framebuffer the instant the machine starts, waits briefly for
//! a key (any key opens a five-entry menu), then chain-loads FreeBSD's
//! `EFI/FREEBSD/LOADER.EFI` from the same partition with arguments that
//! make it boot silently. Everything the user sees afterwards is drawn by
//! the kernel (`splash=`) and then by `jos-splash`.
//!
//! Where `loader.efi` is unchanged and does the actual work, this program
//! stays tiny: no file system parsing (the firmware loads the file from a
//! device path), no configuration, no fonts beyond `jos-gfx`. On any
//! error it prints the reason on the text console, waits, and resets; a
//! machine whose ESP only has `loader.efi` boots exactly as before.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::time::Duration;

use jos_boot::{
    countdown_scene, default_entry, load_options, marks_pending, on_countdown_key, Action,
    Countdown, Key, MenuOutcome, MenuState, COUNTDOWN_MS, DEFAULT_ENTRY, ENTRIES, HEADLESS_ARGS,
    LOADER_PATH, PENDING_NAME, PENDING_VENDOR,
};
use jos_gfx::scene::{card_origin, Scene, CARD_HEIGHT, CARD_WIDTH};
use jos_gfx::Image;
use uefi::boot::{self, LoadImageSource};
use uefi::proto::console::gop::{BltOp, BltPixel, BltRegion, GraphicsOutput, PixelFormat};
use uefi::proto::console::text::{Key as UefiKey, ScanCode};
use uefi::proto::device_path::build::{media, DevicePathBuilder};
use uefi::proto::device_path::DevicePath;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::BootPolicy;
use uefi::runtime::{self, ResetType, VariableAttributes, VariableVendor};
use uefi::{CString16, Status};

const POLL: Duration = Duration::from_millis(20);
const ERROR_PAUSE: Duration = Duration::from_secs(8);

#[uefi::entry]
fn main() -> Status {
    let outcome = run();
    match outcome {
        Ok(()) => Status::SUCCESS,
        Err(e) => {
            report(&e);
            boot::stall(ERROR_PAUSE);
            runtime::reset(ResetType::COLD, Status::LOAD_ERROR, None)
        }
    }
}

/// Lines of the on-screen error card.
fn error_lines(msg: &str) -> Vec<String> {
    let mut lines = Vec::new();
    // Wrap the message at ~70 characters on word boundaries.
    let mut cur = String::new();
    for word in msg.split_whitespace() {
        if cur.len() + word.len() + 1 > 70 && !cur.is_empty() {
            lines.push(core::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.push(String::new());
    lines.push(String::from("Use the firmware boot menu to start"));
    lines.push(String::from(
        "EFI/FREEBSD/LOADER.EFI (FreeBSD loader) directly.",
    ));
    lines.push(alloc::format!(
        "Resetting in {} seconds.",
        ERROR_PAUSE.as_secs()
    ));
    lines
}

fn run() -> Result<(), String> {
    if boot::get_handle_for_protocol::<GraphicsOutput>().is_err() {
        // Headless machine: nothing to draw on, so do not loop on a
        // "no graphics output" reset; boot the loader on the text console.
        uefi::system::with_stdout(|out| {
            let _ = writeln!(out, "jos-boot: no graphics output, booting {LOADER_PATH}");
        });
        return chain_load(HEADLESS_ARGS);
    }
    let pending = boot_pending();
    let choice = {
        let mut screen = Screen::open()?;
        choose(&mut screen, pending)
        // `screen` is dropped here: the GOP protocol must be closed before
        // another image takes the firmware over.
    };
    let entry = ENTRIES
        .get(choice)
        .copied()
        .unwrap_or(ENTRIES[DEFAULT_ENTRY]);
    if marks_pending(&entry) {
        set_boot_pending();
    }
    match entry.action {
        Action::Reboot => runtime::reset(ResetType::COLD, Status::SUCCESS, None),
        Action::Chain => chain_load(entry.args),
    }
}

fn pending_name() -> Option<CString16> {
    CString16::try_from(PENDING_NAME).ok()
}

/// Is the previous muted boot still marked as not having reached rc(8)?
fn boot_pending() -> bool {
    let Some(name) = pending_name() else {
        return false;
    };
    let mut buf = [0u8; 8];
    runtime::get_variable(&name, &VariableVendor(PENDING_VENDOR), &mut buf).is_ok()
}

/// Mark a muted boot as in progress. Best effort: firmware that refuses
/// the write simply never gets the verbose fallback.
fn set_boot_pending() {
    let Some(name) = pending_name() else {
        return;
    };
    let attrs = VariableAttributes::NON_VOLATILE
        | VariableAttributes::BOOTSERVICE_ACCESS
        | VariableAttributes::RUNTIME_ACCESS;
    let _ = runtime::set_variable(&name, &VariableVendor(PENDING_VENDOR), attrs, &[1]);
}

/// Show the card, open the menu on a key, return the chosen entry.
fn choose(screen: &mut Screen, pending: bool) -> usize {
    let default = default_entry(pending);
    screen.draw_full(&countdown_scene(pending));
    let mut waited = Duration::ZERO;
    let total = Duration::from_millis(COUNTDOWN_MS);
    while waited < total {
        if let Some(key) = read_key() {
            match on_countdown_key(key) {
                Countdown::BootDefault => return default,
                Countdown::OpenMenu => return menu(screen, default),
            }
        }
        boot::stall(POLL);
        waited += POLL;
    }
    default
}

fn menu(screen: &mut Screen, default: usize) -> usize {
    let mut state = MenuState::with_default(default);
    screen.draw_card(&state.scene());
    loop {
        if let Some(key) = read_key() {
            match state.on_key(key) {
                MenuOutcome::Redraw => screen.draw_card(&state.scene()),
                MenuOutcome::Nothing => {}
                MenuOutcome::Choose(i) => {
                    // Show the chosen boot as "in progress" before the
                    // loader blanks the screen.
                    screen.draw_card(&countdown_scene(false));
                    return i;
                }
            }
        }
        boot::stall(POLL);
    }
}

fn read_key() -> Option<Key> {
    let key = uefi::system::with_stdin(|input| input.read_key().ok().flatten())?;
    Some(match key {
        UefiKey::Special(ScanCode::UP) => Key::Up,
        UefiKey::Special(ScanCode::DOWN) => Key::Down,
        UefiKey::Special(ScanCode::ESCAPE) => Key::Escape,
        UefiKey::Printable(c) if u16::from(c) == u16::from(b'\r') => Key::Enter,
        _ => Key::Other,
    })
}

/// Chain-load `loader.efi` from the partition this image was loaded from.
fn chain_load(args: &str) -> Result<(), String> {
    let this = boot::image_handle();
    let device = {
        let image = boot::open_protocol_exclusive::<LoadedImage>(this)
            .map_err(|e| alloc::format!("LoadedImage: {e:?}"))?;
        image
            .device()
            .ok_or_else(|| String::from("this image has no device handle"))?
    };

    // Full device path = our partition's path + \EFI\FREEBSD\LOADER.EFI.
    // The firmware resolves the device handle from it, so the loader sees
    // a normal "booted from this ESP" situation and finds its root.
    let mut path_buf = Vec::new();
    let full_path = {
        let device_path = boot::open_protocol_exclusive::<DevicePath>(device)
            .map_err(|e| alloc::format!("DevicePath: {e:?}"))?;
        let mut builder = DevicePathBuilder::with_vec(&mut path_buf);
        for node in device_path.node_iter() {
            builder = builder
                .push(&node)
                .map_err(|e| alloc::format!("device path: {e:?}"))?;
        }
        let file = CString16::try_from(LOADER_PATH)
            .map_err(|_| String::from("loader path is not UCS-2"))?;
        builder = builder
            .push(&media::FilePath { path_name: &file })
            .map_err(|e| alloc::format!("device path: {e:?}"))?;
        let path = builder
            .finalize()
            .map_err(|e| alloc::format!("device path: {e:?}"))?;
        path.to_boxed()
    };

    let child = boot::load_image(
        this,
        LoadImageSource::FromDevicePath {
            device_path: &full_path,
            boot_policy: BootPolicy::ExactMatch,
        },
    )
    .map_err(|e| alloc::format!("loading {LOADER_PATH}: {e:?}"))?;

    // LoadOptions must stay valid while the child runs.
    let options = load_options(args);
    if !args.is_empty() {
        let mut child_image = boot::open_protocol_exclusive::<LoadedImage>(child)
            .map_err(|e| alloc::format!("child LoadedImage: {e:?}"))?;
        let size =
            u32::try_from(options.len() * 2).map_err(|_| String::from("load options too long"))?;
        // SAFETY: `options` outlives `start_image` below and is never
        // modified while the child image can read it.
        unsafe { child_image.set_load_options(options.as_ptr().cast(), size) };
    }

    boot::start_image(child).map_err(|e| alloc::format!("{LOADER_PATH} returned: {e:?}"))?;
    drop(options);
    Err(String::from("loader.efi exited without booting a kernel"))
}

/// Show an error: on the graphics screen (what the user sees) and on the
/// firmware text console (serial, logs).
fn report(msg: &str) {
    if let Ok(mut screen) = Screen::open() {
        let card = jos_gfx::scene::render_message_card("BOOT FAILED", &error_lines(msg));
        screen.draw_full_image(&card);
    }
    uefi::system::with_stdout(|out| {
        let _ = out.clear();
        let _ = writeln!(out, "jos-boot: {msg}");
        let _ = writeln!(
            out,
            "Boot {LOADER_PATH} directly (firmware boot menu) to use FreeBSD's loader."
        );
        let _ = writeln!(out, "Resetting in {} seconds...", ERROR_PAUSE.as_secs());
    });
}

/// The GOP framebuffer, drawn with `Blt` so the pixel format never
/// matters.
struct Screen {
    gop: boot::ScopedProtocol<GraphicsOutput>,
    width: usize,
    height: usize,
    buf: Vec<BltPixel>,
}

impl Screen {
    fn open() -> Result<Self, String> {
        let handle = boot::get_handle_for_protocol::<GraphicsOutput>()
            .map_err(|e| alloc::format!("no graphics output: {e:?}"))?;
        let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle)
            .map_err(|e| alloc::format!("graphics output: {e:?}"))?;

        // Pick the largest mode with a monitor-like aspect ratio, which is
        // close to what loader.efi's `efi-autoresizecons` will choose
        // (largest mode within the EDID limits): one mode switch less.
        // Virtual firmware lists odd modes such as 2048x2048; those are
        // skipped, and a small current mode is left alone only if nothing
        // better exists.
        let current = gop.current_mode_info().resolution();
        let best = gop
            .modes()
            .filter(|m| {
                matches!(
                    m.info().pixel_format(),
                    PixelFormat::Rgb | PixelFormat::Bgr | PixelFormat::Bitmask
                )
            })
            .filter(|m| jos_boot::is_display_like(m.info().resolution()))
            .max_by_key(|m| jos_boot::mode_score(m.info().resolution()));
        if let Some(mode) = best {
            if mode.info().resolution() != current {
                let _ = gop.set_mode(&mode);
            }
        }
        let (width, height) = gop.current_mode_info().resolution();
        if width == 0 || height == 0 {
            return Err(String::from("graphics output reports an empty mode"));
        }
        Ok(Screen {
            gop,
            width,
            height,
            buf: Vec::new(),
        })
    }

    fn blt_image(&mut self, img: &Image, x: usize, y: usize) {
        self.buf.clear();
        self.buf
            .extend(img.pixels().iter().map(|p| BltPixel::new(p.r, p.g, p.b)));
        let dims = (img.width() as usize, img.height() as usize);
        let _ = self.gop.blt(BltOp::BufferToVideo {
            buffer: &self.buf,
            src: BltRegion::Full,
            dest: (x, y),
            dims,
        });
    }

    /// Clear to the theme background and draw the card.
    fn draw_full(&mut self, scene: &Scene) {
        self.clear();
        self.draw_card(scene);
    }

    /// Clear and draw an arbitrary card-sized image.
    fn draw_full_image(&mut self, card: &Image) {
        self.clear();
        self.draw_card_image(card);
    }

    fn clear(&mut self) {
        let bg = jos_gfx::color::THEME.background;
        let _ = self.gop.blt(BltOp::VideoFill {
            color: BltPixel::new(bg.r, bg.g, bg.b),
            dest: (0, 0),
            dims: (self.width, self.height),
        });
    }

    /// Redraw only the card (the background is already in place).
    fn draw_card(&mut self, scene: &Scene) {
        self.draw_card_image(&scene.render_card());
    }

    fn draw_card_image(&mut self, card: &Image) {
        let (w, h) = (
            u32::try_from(self.width).unwrap_or(u32::MAX),
            u32::try_from(self.height).unwrap_or(u32::MAX),
        );
        let (x, y) = card_origin(w, h);
        if x >= 0 && y >= 0 && CARD_WIDTH <= w && CARD_HEIGHT <= h {
            self.blt_image(card, x as usize, y as usize);
        } else {
            // Screen smaller than the card: draw what fits from the
            // top-left (a rare 640x480 firmware mode).
            let mut frame = Image::new(w, h, jos_gfx::color::THEME.background);
            frame.draw_image(card, 0, 0);
            self.blt_image(&frame, 0, 0);
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    uefi::system::with_stdout(|out| {
        let _ = writeln!(out, "jos-boot panic: {info}");
    });
    boot::stall(ERROR_PAUSE);
    runtime::reset(ResetType::COLD, Status::ABORTED, None)
}
