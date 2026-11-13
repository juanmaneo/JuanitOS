// SPDX-License-Identifier: BSD-3-Clause
//! Secure Boot end to end under QEMU: `jos secureboot`'s keys enrolled in
//! a Secure Boot build of OVMF, then three boots from a small ESP. A
//! `jos-boot.efi` signed with the db key must start and chain-load a
//! signed `LOADER.EFI`; the same pre-loader must be refused an unsigned
//! `LOADER.EFI`; and the firmware must refuse an unsigned pre-loader.
//!
//! The keys are made and the images signed by `juanitos_core::secureboot`,
//! exactly as on a JuanitOS machine. Only the enrollment differs: instead
//! of authenticated writes from a running system (`jos secureboot
//! enroll`, which needs efivar), the signature lists are written straight
//! into a copy of OVMF's variable store, which is what the firmware's own
//! setup menu would leave there.
//!
//! The machine has no graphics, so `jos-boot` reports on the serial port
//! and goes straight to the loader; a signed `jos-boot.efi` stands in for
//! FreeBSD's `loader.efi`, so the chain shows on the console twice.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use juanitos_core::secureboot::efi::{self, Guid, Var};
use juanitos_core::secureboot::SecureBoot;
use juanitos_core::sys::Runner;

use crate::console::Console;
use crate::qemu;

/// Secure Boot builds of OVMF, as distributions install them; the
/// variable store template is found with [`vars_for`].
pub const SECBOOT_CODE: &[&str] = &[
    "/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd",
    "/usr/share/OVMF/OVMF_CODE_4M.secboot.fd",
    "/usr/share/edk2/ovmf/OVMF_CODE.secboot.fd",
];

/// What `jos-boot` prints when it chain-loads the loader without a screen.
pub const BOOTING: &str = "jos-boot: no graphics output, booting \\EFI\\FREEBSD\\LOADER.EFI";
/// What it prints when the firmware does not load the loader, followed
/// by the status, one of [`REFUSALS`] for an image that fails
/// verification.
pub const REFUSED: &str = "jos-boot: loading \\EFI\\FREEBSD\\LOADER.EFI:";
pub const REFUSALS: [&str; 2] = ["SECURITY_VIOLATION", "ACCESS_DENIED"];
/// The firmware's boot manager when it refuses a boot option (`%r` of
/// the two statuses image verification returns).
pub const DENIED: [&str; 2] = ["Access Denied", "Security Violation"];

/// Under TCG the firmware alone takes minutes to come up.
const FIRST_MARKER: Duration = Duration::from_secs(300);
const NEXT_MARKER: Duration = Duration::from_secs(120);

/// The images and variable store the cases share, under [`Test::out`].
const UNSIGNED: &str = "unsigned.efi";
const SIGNED: &str = "signed.efi";
const VARS: &str = "vars.fd";

/// The variable store in an OVMF `VARS` image follows the firmware
/// volume header; its own header is 28 bytes (signature GUID, size,
/// format, state, reserved).
const STORE: usize = 0x48;
const STORE_HEADER: usize = 28;
/// gEfiAuthenticatedVariableGuid: a store of authenticated variables.
const AUTH_STORE: Guid = Guid(
    0xaaf32c78,
    0x947b,
    0x439a,
    [0xa1, 0x80, 0x2e, 0x14, 0x4e, 0xc3, 0x77, 0x92],
);
const FORMATTED: u8 = 0x5a;
const HEALTHY: u8 = 0xfe;
/// AUTHENTICATED_VARIABLE_HEADER: start id, state, reserved, attributes,
/// monotonic count, time stamp, public key index, name and data sizes,
/// vendor GUID (packed, 60 bytes). The name (UCS-2, terminated) and the
/// data follow; the next header starts on a 4-byte boundary.
const VAR_HEADER: usize = 60;
const START_ID: [u8; 2] = [0xaa, 0x55];
const VAR_ADDED: u8 = 0x3f;
const VAR_DELETED: u8 = 0xfd;
const ERASED: u8 = 0xff;

#[derive(Debug, Clone)]
pub struct Test {
    /// The pre-loader to sign and boot.
    pub boot_efi: PathBuf,
    /// Keys, images, variable stores and one directory per case.
    pub out: PathBuf,
    /// OVMF code, a Secure Boot build.
    pub code: PathBuf,
    /// The variable store template that goes with [`Test::code`].
    pub vars: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    /// A signed pre-loader chain-loads a signed loader.
    Signed,
    /// A signed pre-loader is refused an unsigned loader.
    UnsignedLoader,
    /// The firmware refuses an unsigned pre-loader.
    UnsignedBoot,
}

impl Case {
    pub const ALL: [Case; 3] = [Case::Signed, Case::UnsignedLoader, Case::UnsignedBoot];

    /// Its directory under [`Test::out`].
    pub fn name(self) -> &'static str {
        match self {
            Case::Signed => "signed",
            Case::UnsignedLoader => "unsigned-loader",
            Case::UnsignedBoot => "unsigned-boot",
        }
    }

    pub fn what(self) -> &'static str {
        match self {
            Case::Signed => "a signed jos-boot.efi chain-loads a signed loader",
            Case::UnsignedLoader => "a signed jos-boot.efi is refused an unsigned loader",
            Case::UnsignedBoot => "the firmware refuses an unsigned jos-boot.efi",
        }
    }

    /// What goes to `\EFI\BOOT\BOOTX64.EFI` and `\EFI\FREEBSD\LOADER.EFI`.
    fn images(self) -> [&'static str; 2] {
        match self {
            Case::Signed => [SIGNED, SIGNED],
            Case::UnsignedLoader => [SIGNED, UNSIGNED],
            Case::UnsignedBoot => [UNSIGNED, SIGNED],
        }
    }
}

/// The first Secure Boot OVMF build installed here.
pub fn find_code() -> Option<PathBuf> {
    SECBOOT_CODE.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// The variable store template that goes with the OVMF code file `code`:
/// the `VARS` file next to it, named like `code` without `.secboot`.
pub fn vars_for(code: &Path) -> Option<PathBuf> {
    let plain = code.file_name()?.to_string_lossy().replace(".secboot", "");
    qemu::sibling_vars(&code.with_file_name(plain))
}

fn le32(n: usize) -> [u8; 4] {
    u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes()
}

/// A variable's name as the store keeps it: UCS-2, terminated.
fn var_name(var: Var) -> Vec<u8> {
    [efi::ucs2(var.name()), vec![0, 0]].concat()
}

/// `var` holding `data`, as an entry of the store.
fn variable(var: Var, data: &[u8]) -> Vec<u8> {
    entry(&var_name(var), var.guid(), data)
}

fn entry(name: &[u8], guid: Guid, data: &[u8]) -> Vec<u8> {
    [
        &START_ID[..],
        &[VAR_ADDED, 0],
        &efi::AUTH_ATTRIBUTES.to_le_bytes(),
        // Monotonic count, time stamp, public key index: unused by
        // time-based variables, and any later write is newer.
        &[0; 8],
        &[0; 16],
        &[0; 4],
        &le32(name.len()),
        &le32(data.len()),
        &guid.bytes(),
        name,
        data,
    ]
    .concat()
}

/// The entry at `at` in `store`, if there is one: its state, whether it
/// is one of `vars`, and where the next one starts.
fn variable_at(store: &[u8], at: usize, vars: &[(Var, Vec<u8>)]) -> Option<(u8, bool, usize)> {
    let h = store
        .get(at..)
        .and_then(<[u8]>::first_chunk::<VAR_HEADER>)
        .filter(|h| h.starts_with(&START_ID))?;
    let name_size = u32::from_le_bytes([h[36], h[37], h[38], h[39]]) as usize;
    let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]) as usize;
    let name = at + VAR_HEADER;
    let ours = store.get(name..name + name_size).is_some_and(|n| {
        vars.iter()
            .any(|(v, _)| n == var_name(*v) && h.ends_with(&v.guid().bytes()))
    });
    Some((
        h[2],
        ours,
        (name + name_size + data_size).next_multiple_of(4),
    ))
}

/// `template`, an OVMF variable store, with `vars` set to the signature
/// lists given: earlier values of them are marked deleted, the new ones
/// appended to the free space.
pub fn enroll(template: &[u8], vars: &[(Var, Vec<u8>)]) -> anyhow::Result<Vec<u8>> {
    let size = template
        .get(STORE..)
        .and_then(<[u8]>::first_chunk::<STORE_HEADER>)
        .filter(|h| h.starts_with(&AUTH_STORE.bytes()) && h[20] == FORMATTED && h[21] == HEALTHY)
        .map(|h| u32::from_le_bytes([h[16], h[17], h[18], h[19]]) as usize)
        .context(
            "not a variable store of authenticated variables \
             (use the VARS file of a Secure Boot OVMF build)",
        )?;
    let end = (STORE + size).min(template.len());
    let mut out = template.to_vec();
    let mut at = STORE + STORE_HEADER;
    while let Some((state, ours, next)) = variable_at(&out, at, vars) {
        if ours && state == VAR_ADDED {
            out.get_mut(at + 2)
                .into_iter()
                .for_each(|s| *s &= VAR_DELETED);
        }
        at = next;
    }
    for (var, data) in vars {
        let entry = variable(*var, data);
        out.get_mut(at..end)
            .and_then(|free| free.get_mut(..entry.len()))
            .filter(|slot| slot.iter().all(|&b| b == ERASED))
            .context("the variable store is full")?
            .copy_from_slice(&entry);
        at = (at + entry.len()).next_multiple_of(4);
    }
    Ok(out)
}

/// Wait for what `case` must show on the console.
pub fn judge(case: Case, con: &mut Console, first: Duration, next: Duration) -> anyhow::Result<()> {
    match case {
        Case::Signed => con
            .expect(BOOTING, first)
            .and_then(|_| con.expect(BOOTING, next))
            .map(drop),
        Case::UnsignedLoader => {
            con.expect(BOOTING, first)?;
            if con.expect_any(&[REFUSED, BOOTING], next)? != 0 {
                bail!("the firmware started an unsigned loader");
            }
            let why = con.expect("\n", next)?;
            if !REFUSALS.iter().any(|r| why.contains(r)) {
                bail!("the loader failed for another reason:{}", why.trim_end());
            }
            Ok(())
        }
        Case::UnsignedBoot => match con.expect_any(&[DENIED[0], DENIED[1], BOOTING], first)? {
            2 => bail!(
                "the firmware started an unsigned jos-boot.efi \
                 (is --firmware a Secure Boot build of OVMF?)"
            ),
            _ => Ok(()),
        },
    }
}

/// mtools commands that make `esp` a FAT image holding `boot` as the
/// removable-media boot file and `loader` as FreeBSD's loader.
pub fn esp_commands(esp: &Path, boot: &Path, loader: &Path) -> Vec<(&'static str, Vec<String>)> {
    let esp = esp.display().to_string();
    let image = |args: &[&str]| -> Vec<String> {
        ["-i", &esp]
            .iter()
            .chain(args)
            .map(|a| a.to_string())
            .collect()
    };
    let (boot, loader) = (boot.display().to_string(), loader.display().to_string());
    vec![
        (
            "mformat",
            image(&["-C", "-T", "65536", "-h", "64", "-s", "32", "::"]),
        ),
        ("mmd", image(&["::/EFI", "::/EFI/BOOT", "::/EFI/FREEBSD"])),
        ("mcopy", image(&[&boot, "::/EFI/BOOT/BOOTX64.EFI"])),
        ("mcopy", image(&[&loader, "::/EFI/FREEBSD/LOADER.EFI"])),
    ]
}

/// A Secure Boot machine (SMM, secure flash) with no screen and no
/// network, booting `esp`; the console is the serial port.
pub fn qemu_args(code: &Path, vars: &Path, esp: &Path) -> Vec<String> {
    [
        "-machine",
        &format!("q35,smm=on,accel={}", qemu::accel()),
        "-global",
        "driver=cfi.pflash01,property=secure,value=on",
        "-drive",
        &format!(
            "if=pflash,format=raw,unit=0,readonly=on,file={}",
            code.display()
        ),
        "-drive",
        &format!("if=pflash,format=raw,unit=1,file={}", vars.display()),
        "-drive",
        &format!("file={},format=raw,if=virtio", esp.display()),
        "-m",
        "256",
        "-vga",
        "none",
        "-display",
        "none",
        "-nic",
        "none",
        "-serial",
        "stdio",
        "-monitor",
        "none",
        "-no-reboot",
    ]
    .map(String::from)
    .to_vec()
}

pub fn run(t: &Test, runner: &dyn Runner, dry_run: bool) -> anyhow::Result<()> {
    if !dry_run && !t.boot_efi.is_file() {
        bail!(
            "{} does not exist; build it with `jos-release bins`",
            t.boot_efi.display()
        );
    }
    let keys = t.out.join("keys");
    let sb = SecureBoot::new(runner, dry_run, keys.clone());
    println!("==> secureboot: PK, KEK and db keys in {}", keys.display());
    sb.keygen(false, true)?;
    let boot = t.boot_efi.display();
    let (unsigned, signed) = (t.out.join(UNSIGNED), t.out.join(SIGNED));
    println!("+ cp {boot} {}", unsigned.display());
    println!("+ cp {boot} {}", signed.display());
    if dry_run {
        println!("+ sign {}", signed.display());
    } else {
        std::fs::copy(&t.boot_efi, &unsigned)
            .and_then(|_| std::fs::copy(&t.boot_efi, &signed))
            .with_context(|| format!("copying {boot}"))?;
        sb.sign_file(&signed)?;
    }
    println!(
        "+ enroll {}/{{PK,KEK,db}}.esl: {} -> {}",
        keys.display(),
        t.vars.display(),
        t.out.join(VARS).display()
    );
    if !dry_run {
        write_vars(t, &keys)?;
    }
    for case in Case::ALL {
        run_case(t, case, dry_run)
            .with_context(|| format!("Secure Boot test failed: {}", case.what()))?;
    }
    println!("\nSecure Boot works: the firmware runs what the db key signed, and nothing else");
    Ok(())
}

/// [`VARS`]: the template with the keys' signature lists enrolled.
fn write_vars(t: &Test, keys: &Path) -> anyhow::Result<()> {
    let template =
        std::fs::read(&t.vars).with_context(|| format!("reading {}", t.vars.display()))?;
    let lists = Var::ALL
        .iter()
        .map(|&v| std::fs::read(keys.join(format!("{}.esl", v.name()))).map(|l| (v, l)))
        .collect::<std::io::Result<Vec<_>>>();
    lists
        .map_err(anyhow::Error::from)
        .and_then(|lists| enroll(&template, &lists))
        .and_then(|store| Ok(std::fs::write(t.out.join(VARS), store)?))
        .with_context(|| format!("enrolling the keys into a copy of {}", t.vars.display()))
}

fn run_case(t: &Test, case: Case, dry_run: bool) -> anyhow::Result<()> {
    println!("\n==> secureboot: {}", case.what());
    let dir = t.out.join(case.name());
    let (esp, vars) = (dir.join("esp.img"), dir.join(VARS));
    println!("+ cp {} {}", t.out.join(VARS).display(), vars.display());
    if !dry_run {
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::copy(t.out.join(VARS), &vars))
            .with_context(|| format!("preparing {}", dir.display()))?;
        // mformat does not overwrite an image.
        let _ = std::fs::remove_file(&esp);
    }
    let [boot, loader] = case.images().map(|name| t.out.join(name));
    for (program, args) in esp_commands(&esp, &boot, &loader) {
        qemu::run_tool(program, &args, dry_run)?;
    }
    let args = qemu_args(&t.code, &vars, &esp);
    println!("+ qemu-system-x86_64 {}", args.join(" "));
    if dry_run {
        return Ok(());
    }
    qemu::spawn_console(&args, &dir)
        .and_then(|mut con| judge(case, &mut con, FIRST_MARKER, NEXT_MARKER))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty store of `size` bytes after the volume header.
    fn template(size: usize) -> Vec<u8> {
        let mut t = vec![0u8; STORE];
        t.extend(AUTH_STORE.bytes());
        t.extend(le32(size));
        t.extend([FORMATTED, HEALTHY, 0, 0, 0, 0, 0, 0]);
        t.resize(STORE + size, ERASED);
        t
    }

    /// An entry: state, name, GUID, data.
    type Entry = (u8, Vec<u8>, [u8; 16], Vec<u8>);

    fn entries(store: &[u8]) -> Vec<Entry> {
        let mut at = STORE + STORE_HEADER;
        let mut out = Vec::new();
        while let Some((state, _, next)) = variable_at(store, at, &[]) {
            assert_eq!(at % 4, 0);
            let h = &store[at..at + VAR_HEADER];
            assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 0x27);
            let ns = u32::from_le_bytes(h[36..40].try_into().unwrap()) as usize;
            let ds = u32::from_le_bytes(h[40..44].try_into().unwrap()) as usize;
            let name = store[at + 60..at + 60 + ns].to_vec();
            let data = store[at + 60 + ns..at + 60 + ns + ds].to_vec();
            out.push((state, name, h[44..60].try_into().unwrap(), data));
            at = next;
        }
        out
    }

    fn lists(tag: u8) -> Vec<(Var, Vec<u8>)> {
        Var::ALL
            .iter()
            .enumerate()
            .map(|(i, &v)| (v, vec![tag; 5 + i]))
            .collect()
    }

    #[test]
    fn the_keys_go_into_an_empty_store() {
        let store = enroll(&template(0x1000), &lists(1)).unwrap();
        assert_eq!(store.len(), STORE + 0x1000);
        let found = entries(&store);
        assert_eq!(found.len(), 3);
        for ((state, name, guid, data), (var, list)) in found.iter().zip(lists(1)) {
            assert_eq!(*state, VAR_ADDED);
            assert_eq!(*name, var_name(var));
            assert_eq!(*guid, var.guid().bytes());
            assert_eq!(*data, list);
        }
        assert_eq!(found[0].1, b"P\0K\0\0\0");
        assert_eq!(found[2].2, efi::IMAGE_SECURITY_DATABASE.bytes());
        // The rest is still free.
        assert_eq!(*store.last().unwrap(), ERASED);
    }

    #[test]
    fn enrolling_again_replaces_only_the_keys() {
        let mut t = template(0x1000);
        // Other variables: another name, PK's name under db's GUID, and
        // a PK that was deleted already.
        let mut at = STORE + STORE_HEADER;
        let boot = [efi::ucs2("Boot0000"), vec![0, 0]].concat();
        let mut put = |entry: Vec<u8>| {
            t[at..at + entry.len()].copy_from_slice(&entry);
            at = (at + entry.len()).next_multiple_of(4);
        };
        put(entry(&boot, efi::GLOBAL_VARIABLE, b"boot!"));
        put(entry(
            &var_name(Var::Pk),
            efi::IMAGE_SECURITY_DATABASE,
            b"x",
        ));
        let mut deleted = variable(Var::Pk, b"gone");
        deleted[2] = VAR_ADDED & VAR_DELETED;
        put(deleted);

        let once = enroll(&t, &lists(1)).unwrap();
        let twice = enroll(&once, &lists(2)).unwrap();
        let found = entries(&twice);
        assert_eq!(found.len(), 9);
        let states: Vec<u8> = found.iter().map(|e| e.0).collect();
        assert_eq!(
            states,
            [0x3f, 0x3f, 0x3d, 0x3d, 0x3d, 0x3d, 0x3f, 0x3f, 0x3f]
        );
        assert_eq!(found[0].1, boot);
        assert_eq!(found[1].3, b"x");
        assert_eq!(found[8].3, vec![2; 7]);
    }

    #[test]
    fn only_a_store_with_room_is_written() {
        let err = |template: &[u8]| enroll(template, &lists(1)).unwrap_err().to_string();
        assert!(err(b"short").contains("not a variable store"));
        let mut plain = template(0x1000);
        plain[STORE] ^= 1;
        assert!(err(&plain).contains("authenticated variables"));
        let mut unformatted = template(0x1000);
        unformatted[STORE + 20] = ERASED;
        assert!(err(&unformatted).contains("not a variable store"));
        // Room for two of the three.
        assert_eq!(
            err(&template(STORE_HEADER + 160)),
            "the variable store is full"
        );
        // Past the store's end, even though the file goes on.
        let mut longer = template(STORE_HEADER + 160);
        longer.resize(longer.len() + 0x1000, ERASED);
        assert_eq!(err(&longer), "the variable store is full");
        // Free space that is not erased.
        let mut dirty = template(0x1000);
        dirty[STORE + STORE_HEADER + 100] = 0;
        assert_eq!(err(&dirty), "the variable store is full");
    }

    #[test]
    fn a_damaged_entry_ends_the_walk() {
        let mut t = template(0x1000);
        let mut bad = variable(Var::Pk, b"old");
        bad[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
        t[STORE + STORE_HEADER..STORE + STORE_HEADER + bad.len()].copy_from_slice(&bad);
        // Its name runs off the store, so it is no key, and nothing fits
        // after it.
        assert_eq!(
            enroll(&t, &lists(1)).unwrap_err().to_string(),
            "the variable store is full"
        );
        // A header cut short by the end of the file.
        let mut t = template(STORE_HEADER + 8);
        t[STORE + STORE_HEADER..][..2].copy_from_slice(&START_ID);
        assert!(variable_at(&t, STORE + STORE_HEADER, &[]).is_none());
    }

    #[test]
    fn the_template_goes_with_the_code_file() {
        let dir = std::env::temp_dir().join(format!("jos-sb-fw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (code, vars) in [
            ("OVMF_CODE.secboot.4m.fd", "OVMF_VARS.4m.fd"),
            ("OVMF_CODE_4M.secboot.fd", "OVMF_VARS_4M.fd"),
            ("OVMF_CODE.secboot.fd", "OVMF_VARS.fd"),
        ] {
            std::fs::write(dir.join(vars), b"").unwrap();
            assert_eq!(vars_for(&dir.join(code)), Some(dir.join(vars)));
        }
        assert_eq!(vars_for(&dir.join("edk2-code.secboot.fd")), None);
        assert_eq!(vars_for(Path::new("/")), None);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(find_code().as_deref().is_none_or(Path::is_file));
    }

    #[test]
    fn the_machine_has_secure_flash_and_no_screen() {
        let args = qemu_args(Path::new("/c.fd"), Path::new("/v.fd"), Path::new("/e.img")).join(" ");
        assert!(args.starts_with("-machine q35,smm=on,accel="), "{args}");
        for needle in [
            "-global driver=cfi.pflash01,property=secure,value=on",
            "-drive if=pflash,format=raw,unit=0,readonly=on,file=/c.fd",
            "-drive if=pflash,format=raw,unit=1,file=/v.fd",
            "-drive file=/e.img,format=raw,if=virtio",
            "-vga none -display none -nic none -serial stdio -monitor none -no-reboot",
        ] {
            assert!(args.contains(needle), "{needle}");
        }
        let esp = esp_commands(Path::new("/e.img"), Path::new("/b"), Path::new("/l"));
        let lines: Vec<String> = esp
            .iter()
            .map(|(p, a)| format!("{p} {}", a.join(" ")))
            .collect();
        assert_eq!(
            lines,
            [
                "mformat -i /e.img -C -T 65536 -h 64 -s 32 ::",
                "mmd -i /e.img ::/EFI ::/EFI/BOOT ::/EFI/FREEBSD",
                "mcopy -i /e.img /b ::/EFI/BOOT/BOOTX64.EFI",
                "mcopy -i /e.img /l ::/EFI/FREEBSD/LOADER.EFI",
            ]
        );
    }

    #[test]
    fn every_case_has_its_own_images() {
        let names: Vec<_> = Case::ALL.iter().map(|c| c.name()).collect();
        assert_eq!(names, ["signed", "unsigned-loader", "unsigned-boot"]);
        assert!(Case::ALL.iter().all(|c| !c.what().is_empty()));
        assert_eq!(Case::UnsignedLoader.images(), [SIGNED, UNSIGNED]);
        assert_eq!(Case::UnsignedBoot.images(), [UNSIGNED, SIGNED]);
    }

    /// A console that prints `output`, then closes.
    #[cfg(unix)]
    fn console(output: &str) -> Console<'static> {
        use std::process::{Command, Stdio};
        let child = Command::new("sh")
            .args(["-c", "printf '%s' \"$1\"", "sh", output])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Console::attach(child, None).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn what_each_case_must_show() {
        let t = Duration::from_secs(10);
        let judge_output = |case, output: &str| judge(case, &mut console(output), t, t);
        let judge = |case, lines: &[&str]| {
            let output: String = lines.iter().map(|l| format!("{l}\r\n")).collect();
            judge_output(case, &output)
        };
        let denied = "BdsDxe: failed to load Boot0001: Access Denied";
        let refused = "jos-boot: loading \\EFI\\FREEBSD\\LOADER.EFI: SECURITY_VIOLATION";
        judge(Case::Signed, &[BOOTING, BOOTING]).unwrap();
        judge(Case::UnsignedLoader, &[BOOTING, refused]).unwrap();
        let denied_loader = "jos-boot: loading \\EFI\\FREEBSD\\LOADER.EFI: ACCESS_DENIED";
        judge(Case::UnsignedLoader, &[BOOTING, denied_loader]).unwrap();
        judge(Case::UnsignedBoot, &[denied]).unwrap();
        judge(Case::UnsignedBoot, &["Security Violation"]).unwrap();

        let err = |case, lines: &[&str]| judge(case, lines).unwrap_err().to_string();
        assert!(err(Case::Signed, &[BOOTING]).contains("console closed"));
        assert!(err(Case::UnsignedLoader, &[]).contains("console closed"));
        assert!(err(Case::UnsignedLoader, &[BOOTING]).contains("console closed"));
        assert_eq!(
            err(Case::UnsignedLoader, &[BOOTING, BOOTING]),
            "the firmware started an unsigned loader"
        );
        let missing = "jos-boot: loading \\EFI\\FREEBSD\\LOADER.EFI: NOT_FOUND";
        assert_eq!(
            err(Case::UnsignedLoader, &[BOOTING, missing]),
            "the loader failed for another reason: NOT_FOUND"
        );
        let cut = format!("{BOOTING}\r\n{REFUSED} SECU");
        let err = judge_output(Case::UnsignedLoader, &cut).unwrap_err();
        assert!(err.to_string().contains("console closed"));
        let err = |case, lines: &[&str]| judge(case, lines).unwrap_err().to_string();
        assert!(err(Case::UnsignedBoot, &[BOOTING]).contains("unsigned jos-boot.efi"));
        assert!(err(Case::UnsignedBoot, &[]).contains("console closed"));
    }
}
