# Graphical boot: Haiku-style, in Rust, on stock FreeBSD

JuanitOS boots like BeOS/Haiku: a dark screen, the logo, and a row of
icons that light up one after the other as the system comes up. No GRUB,
no Plymouth, no text scrolling by, and no Lua beyond what FreeBSD's own
loader already runs. Three small Rust programs share one drawing library
and hand the picture to each other.

![boot card](images/boot-splash.png)

| Phase            | Who draws                       | Lit icons          |
|------------------|---------------------------------|--------------------|
| firmware → loader| `jos-boot.efi` (UEFI app)       | boot               |
| kernel           | `vt(4)` with `splash=` PNG      | boot, kernel       |
| `rc(8)`          | `jos-splash` daemon             | + storage, network, qubes, desktop |
| login            | COSMIC greeter (Wayland)        | —                  |

The card is 720×420 pixels and is always placed with the kernel's own
centring formula `((screen - card) / 2)`, so it does not move between
phases. Background is pure black, which is what `vt(4)` blanks to.

## Components

### `crates/jos-gfx` — the artwork

`no_std` + `alloc`, `#![forbid(unsafe_code)]`. Colours, a 5×7 bitmap font,
procedurally drawn icons (power, kernel, storage, network, qubes, desktop)
and logo, an anti-aliased rasteriser (4×4 supersampling), and `Scene`:
which stages are lit, an optional status line, an optional menu. With the
`png` feature it also writes PNGs (`jos-release` uses that for the kernel
splash; `jos-splash render` for previews). Fully unit-tested on any host.

### `crates/jos-boot` — the pre-loader (`EFI/BOOT/BOOTX64.EFI`)

A UEFI application (`x86_64-unknown-uefi`, `uefi` crate, 67 KB). It:

1. opens the Graphics Output Protocol, switches to the largest mode (the
   same choice `loader.efi`'s `efi-autoresizecons` makes, so there is no
   second mode switch), fills the screen black and `Blt`s the card;
2. polls the keyboard for 1.5 s. Enter boots at once; any other key opens
   a menu: *Boot JuanitOS*, *Boot verbose*, *Single user*, *FreeBSD loader
   menu*, *Reboot*;
3. chain-loads `EFI/FREEBSD/LOADER.EFI` from the partition it was loaded
   from. The full device path (partition + file) is handed to
   `LoadImage`, so the loader sees a normal "booted from this ESP" setup
   and finds the ZFS pool / live CD exactly as if the firmware had started
   it. The chosen entry's arguments become the child's `LoadOptions`.

The pure parts (entries, menu state, UCS-2 encoding) live in
`src/lib.rs` and are tested on the host (`cargo test --lib --target
x86_64-unknown-linux-gnu` from the crate directory). On any error the
program prints the reason on the firmware text console and resets after
8 s; an ESP with only `loader.efi` boots as plain FreeBSD.

Without a Graphics Output Protocol at all (a serial-only server, `qemu
-vga none`) there is nothing to draw on, so the card and the menu are
skipped: `loader.efi` is chain-loaded immediately with `autoboot_delay=-1
beastie_disable=YES` and *without* `-m`/`console=nullconsole`, so the
loader and the kernel talk on the firmware's text console, i.e. the
serial port. The live installer then runs in its text mode there.

### `crates/jos-splash` — the daemon

`rc.d/jos-splash` starts it right after `FILESYSTEMS`. It opens
`/dev/ttyv9` (VT 10 — `/etc/ttys` runs gettys on `ttyv0-7` and the Wayland
seat picks `ttyv8`), switches to it, sets `KD_GRAPHICS` so console output
never touches the picture, `mmap`s the framebuffer and draws the card with
*storage* lit. Then it listens on the Unix datagram socket
`/var/run/jos-splash.sock`:

```text
stage <boot|kernel|storage|network|qubes|desktop>   light that icon (and earlier ones)
status <text>                                      one line under the icons
finish [keep|console]                              end: leave the frame / restore the console
```

Clients are the same binary: `jos-splash stage network`. A missing socket
is not an error for them, so the rc scripts work on a text boot too. Esc
on the keyboard ends the splash on the text console.

Hooks:

| rc.d script           | Runs after      | Sends                                   |
|-----------------------|-----------------|-----------------------------------------|
| `jos-splash`          | `FILESYSTEMS`   | starts the daemon (`--reached storage`) |
| `jos-splash-network`  | `NETWORKING`    | `stage network`                         |
| `jos`                 | `LOGIN`         | `status Starting qubes`, `stage qubes`  |
| `jos-login`           | `LOGIN seatd dbus` | nothing; makes `/var/run/user` for the desktop sessions |
| `jos-splash-finish`   | `LOGIN jos jos-login` | `stage desktop`, then `finish` if the login screen is enabled and installed, else `finish --console` |

`finish` (keep) lights every icon and leaves the frame on screen; the
login screen (`ly` on `ttyv1`, `docs/relibc-cosmic.md`) then switches
to its terminal and the compositor takes the display after login. The installed system enables all of
this with `jos_splash_enable="YES"` and `rc_startmsgs="NO"` in `rc.conf`.
The live medium does the same but always ends on the console, where the
installer runs.

All FreeBSD-specific calls (`ioctl`, `mmap`, termios) are in
`crates/jos-fb/src/vt.rs` behind `cfg(target_os = "freebsd")`, shared with
the graphical installer; the state machine, protocol, pixel packing and
keyboard decoding are tested on any host. `jos-splash run --dry-run` serves
the socket with a fake display for testing the rc.d plumbing.

After switching to its virtual terminal the daemon waits a few `vt(4)`
timer ticks before drawing: the kernel repaints a freshly activated
terminal (blank cells and border) from its 25 Hz timer *after*
`VT_WAITACTIVE` returns, which otherwise wipes the first frame (seen on a
real FreeBSD 15.1 boot under QEMU: a black screen until the next stage
redrew).

## What the loader and kernel do (verified against FreeBSD 15.1 sources)

- `stand/efi/loader/efi_main.c` splits `LoadOptions` on spaces into
  `argv`; `main.c` runs `boot_parse_arg` on each: `-m` → `RB_MUTE`, `-v`,
  `-s`, and `name=value` → `setenv`. This happens *after* `cons_probe()`
  (which always initialises the EFI console, clearing the screen) and
  *before* any `printf`.
- `console=nullconsole` is a valid loader console on amd64. Set from the
  arguments, it switches output away from the screen before the banner is
  printed, and `bi_getboothowto` turns it into `RB_MUTE` for the kernel.
  Because the EFI console was initialised first, `gfx_state` still holds
  the GOP framebuffer and `bi_load_efi_data` passes `MODINFOMD_EFI_FB` to
  the kernel — `vt(4)` keeps working.
- `autoboot_delay=-1` and `beastie_disable=YES` make `loader.lua` skip the
  menu and boot immediately. `/boot/defaults/loader.conf` leaves
  `autoboot_delay`, `beastie_disable`, `console` and `boot_mute` commented
  out, so values given as arguments survive `config.load()`. Our generated
  `loader.conf` therefore never sets `console=`; the serial console is
  requested kernel-side with `boot_multicons` + `hw.uart.console`.
- `splash=<png>` / `shutdown_splash=<png>` in `loader.conf`: the loader
  decodes the PNG to 32-bit RGBA (`build_splash_module`) and
  `vt_core.c` draws it centred — only when `RB_MUTE` is set and the console
  is not in text mode. While `VDF_SPLASH` is set, `vt_flush` draws no
  text; the flag is cleared by a key press or when a `ttyv` is opened, which
  is why the daemon starts as early as `rc(8)` allows.
- `vt(4)` on `/dev/ttyv*`: `FBIOGTYPE` (geometry/depth), `FBIO_GETLINEWIDTH`
  (stride), `FBIO_GETRGBOFFS` (channel offsets; BGRX assumed if `ENOTTY`),
  `mmap` of the framebuffer, `KDSETMODE` (`KD_GRAPHICS` / `KD_TEXT`),
  `VT_ACTIVATE` / `VT_WAITACTIVE` / `VT_GETACTIVE`. The ioctl numbers are
  computed from `ioccom.h` and checked against the C values in tests.

## Verified under QEMU + OVMF (`tools/uefi-test.sh`)

- `boot`: `jos-boot.efi` as `BOOTX64.EFI` on a FAT image. OVMF starts it,
  the card is drawn at the largest display-like mode (2560×1600 on QEMU
  `-vga std`; the 2048×2048 mode is skipped), the countdown runs, and with
  no `LOADER.EFI` present the error path shows and resets.
- `boot-loader`: same plus FreeBSD 15.1's real `loader.efi` at
  `EFI/FREEBSD/LOADER.EFI`. The chain-load succeeds, the arguments arrive:
  the serial log shows only `Consoles: EFI console` (printed by
  `cons_probe()` before arguments are parsed) and then nothing — no
  banner, no `BootOrder`, no "Failed to find bootable partition" — until
  the loader returns `NOT_FOUND` (the test image has no kernel) and
  `jos-boot` reports it. `loader-only` is the control: the same loader
  booted directly prints its full banner and countdown.

![boot card under OVMF](images/boot-loader-14.png)

## Known limits

- **One black gap (accepted).** The loader's console initialisation
  erases the screen before it parses its arguments, so the pre-loader's
  picture goes black until the kernel draws the splash (the time the
  loader needs to read the kernel and modules, 1–3 s). There is no knob in
  the stock loader to avoid it; patching `loader.efi` is out of scope while
  the project reuses FreeBSD binaries unchanged, and the gap is considered
  acceptable.
- **Silent loader failures.** With `console=nullconsole` a loader that
  cannot find a kernel waits at an invisible prompt. Use the *FreeBSD
  loader menu* entry (no arguments) to see it.
- **BIOS boot** (CD `cdboot`, legacy machines) is the classic text boot.
- A daemon crash would leave the console in graphics mode; the panic
  path restores text mode, and `jos-splash finish --console` (what
  `service jos-splash stop` runs) can be typed blind.
- `vt_fb_bitblt_argb` pixel order and the exact moment the greeter takes
  over have not yet been exercised on hardware; the QEMU/OVMF path is the
  first test target (`jos-release qemu`).
