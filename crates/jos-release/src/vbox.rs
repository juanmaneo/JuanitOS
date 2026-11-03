//! Install the ISO in a VirtualBox VM and boot what it installed, driven
//! over the VM's serial port. This is the graphical path that the QEMU
//! harness (`selfhost`, no display) never takes: VirtualBox's own EFI
//! firmware, a Graphics Output Protocol for `jos-boot` (VMSVGA with 128 MB
//! of VRAM, which offers modes up to 7680x4320), the kernel splash, the
//! graphical installer and the login screen on the framebuffer, the disk
//! on AHCI (`ada0`), PS/2 input. The graphical installer owns the console
//! until it exits (`rc.local` runs it before the gettys start), so the
//! harness screenshots it, quits it through the keyboard, logs in on the
//! serial getty that then appears, runs the unattended install and the
//! checks in the installed dom0; screenshots of the installer and of the
//! login screen land in the output directory.
//!
//! Everything goes through `VBoxManage` via the [`Runner`], so the whole
//! sequence can be dry-run and unit-tested.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context};

use juanitos_core::sys::Runner;

use crate::console::{Console, Guest};
use crate::qemu::{power_off, root_shell, run_steps, SELFHOST_PASSWORD};

#[derive(Debug, Clone)]
pub struct VBox {
    /// VM name; also the VM's folder under `out`.
    pub name: String,
    pub iso: PathBuf,
    /// VM folder, serial socket, console.log and screenshots.
    pub out: PathBuf,
    pub disk_size_gb: u64,
    pub memory_mb: u64,
    pub cpus: u32,
    pub vram_mb: u32,
    /// Leave the VM registered afterwards (to look at it in the GUI).
    pub keep: bool,
}

impl VBox {
    /// Where VirtualBox puts the VM (`--basefolder out`) and where its disk
    /// is created.
    fn disk(&self) -> PathBuf {
        self.out.join(&self.name).join("disk.vdi")
    }

    /// The serial port, a local socket VirtualBox serves.
    fn serial(&self) -> PathBuf {
        self.out.join("serial.sock")
    }
}

/// How long to wait for VirtualBox to serve the serial socket after
/// `startvm`.
const SERIAL_PATIENCE: Duration = Duration::from_secs(60);

/// How long a boot may take to reach its login (the live medium's
/// installer, then its serial getty; the installed dom0's getty).
const BOOT_PATIENCE: Duration = Duration::from_secs(20 * 60);

/// The serial port stays silent from the loader's banner until the
/// installer, on the screen, has been quit: the quit keys are pressed
/// again this often until the login appears (keys sent before the
/// installer listens are lost).
const QUIT_ROUND: Duration = Duration::from_secs(15);

/// PS/2 scancodes (make and break) that quit the graphical installer
/// whatever its state: Enter on *Start* opens the next page and Esc comes
/// back, which puts the focus on *Start* again; Shift+Tab moves it to
/// *Quit* and Enter presses it. With the focus on *Quit* already, the
/// first Enter quits and the rest lands on the console's login.
pub const QUIT_INSTALLER_KEYS: &[&str] = &["1c 9c", "01 81", "2a 0f 8f aa", "1c 9c"];

/// The `VBoxManage` invocations that create the VM, in order: EFI, the
/// serial port as a socket server, VMSVGA, a NAT virtio NIC, the disk on
/// AHCI port 0 and the ISO on port 1, booting the disk first (blank, the
/// firmware falls through to the DVD; installed, the reboot lands in the
/// dom0 with the ISO still attached).
pub fn create_commands(v: &VBox) -> Vec<Vec<String>> {
    let disk = v.disk().display().to_string();
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    vec![
        args(&[
            "createvm",
            "--name",
            &v.name,
            "--ostype",
            "FreeBSD_64",
            "--basefolder",
            &v.out.display().to_string(),
            "--register",
        ]),
        args(&[
            "modifyvm",
            &v.name,
            "--firmware",
            "efi",
            "--memory",
            &v.memory_mb.to_string(),
            "--cpus",
            &v.cpus.to_string(),
            "--ioapic",
            "on",
            "--graphicscontroller",
            "vmsvga",
            "--vram",
            &v.vram_mb.to_string(),
            "--audio-enabled",
            "off",
            "--nic1",
            "nat",
            "--nictype1",
            "virtio",
            "--uart1",
            "0x3F8",
            "4",
            "--uartmode1",
            "server",
            &v.serial().display().to_string(),
            "--boot1",
            "disk",
            "--boot2",
            "dvd",
            "--boot3",
            "none",
            "--boot4",
            "none",
        ]),
        args(&[
            "createmedium",
            "disk",
            "--filename",
            &disk,
            "--size",
            &(v.disk_size_gb * 1024).to_string(),
            "--format",
            "VDI",
        ]),
        args(&[
            "storagectl",
            &v.name,
            "--name",
            "SATA",
            "--add",
            "sata",
            "--controller",
            "IntelAhci",
            "--portcount",
            "2",
        ]),
        args(&[
            "storageattach",
            &v.name,
            "--storagectl",
            "SATA",
            "--port",
            "0",
            "--device",
            "0",
            "--type",
            "hdd",
            "--medium",
            &disk,
        ]),
        args(&[
            "storageattach",
            &v.name,
            "--storagectl",
            "SATA",
            "--port",
            "1",
            "--device",
            "0",
            "--type",
            "dvddrive",
            "--medium",
            &v.iso.display().to_string(),
        ]),
    ]
}

/// The unattended install from the live medium's serial login. The VM has
/// no IOMMU, hence `--ignore-doctor`; the desktop services stay on so the
/// reboot shows the login screen.
pub fn install_command() -> String {
    format!(
        "jos-install --non-interactive --ignore-doctor --disk ada0 --no-pci \
         --root-password {SELFHOST_PASSWORD}"
    )
}

/// What is checked in the installed dom0, each tied to something this VM
/// is known to get wrong without its fix (`docs/boot-splash.md`).
pub fn checks() -> Vec<(&'static str, String, u64)> {
    vec![
        (
            "the kernel runs on the framebuffer the pre-loader chose",
            "dmesg | grep 'VT(efifb): resolution'".to_string(),
            1,
        ),
        (
            "the login screen is up",
            "pgrep -x jos-greet".to_string(),
            1,
        ),
        (
            "power-off goes through ACPI (the EFI runtime service hangs here)",
            "sysctl -n hw.efi.poweroff | grep -x 0".to_string(),
            1,
        ),
    ]
}

/// The VM as a [`Guest`]: gone once VirtualBox reports it powered off.
struct VBoxGuest<'a> {
    runner: &'a dyn Runner,
    name: String,
}

/// `VMState="..."` from `showvminfo --machinereadable`.
pub fn vm_state(info: &str) -> Option<&str> {
    info.lines()
        .find_map(|l| l.strip_prefix("VMState=\""))
        .and_then(|rest| rest.strip_suffix('"'))
}

impl Guest for VBoxGuest<'_> {
    fn finished(&mut self) -> Option<anyhow::Result<()>> {
        let info = self
            .runner
            .run(
                "VBoxManage",
                &["showvminfo", &self.name, "--machinereadable"],
            )
            .ok()?;
        match vm_state(&info.stdout)? {
            "running" | "starting" | "stopping" | "paused" => None,
            "poweroff" => Some(Ok(())),
            other => Some(Err(anyhow::anyhow!("the VM is {other}"))),
        }
    }

    fn kill(&mut self) {
        let _ = self
            .runner
            .run("VBoxManage", &["controlvm", &self.name, "poweroff"]);
    }
}

/// The one serial connection VirtualBox serves, used from both sides of
/// the console.
#[cfg(unix)]
struct Serial(std::sync::Arc<std::os::unix::net::UnixStream>);

#[cfg(unix)]
impl std::io::Read for Serial {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&*self.0).read(buf)
    }
}

#[cfg(unix)]
impl std::io::Write for Serial {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self.0).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        (&*self.0).flush()
    }
}

/// Connect to the serial socket VirtualBox serves once the VM runs; the
/// console log goes next to it.
#[cfg(unix)]
fn connect<'a>(
    v: &VBox,
    runner: &'a dyn Runner,
    patience: Duration,
) -> anyhow::Result<Console<'a>> {
    use std::os::unix::net::UnixStream;
    use std::time::Instant;
    let path = v.serial();
    let deadline = Instant::now() + patience;
    let stream = loop {
        match UnixStream::connect(&path) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("connecting to the VM's serial port at {}", path.display())
                })
            }
        }
    };
    let log = std::fs::File::create(v.out.join("console.log"))
        .with_context(|| format!("creating the console log in {}", v.out.display()))?;
    let stream = std::sync::Arc::new(stream);
    let guest = VBoxGuest {
        runner,
        name: v.name.clone(),
    };
    Ok(Console::over(
        Box::new(guest),
        Serial(stream.clone()),
        Serial(stream),
        Some(log),
    ))
}

#[cfg(not(unix))]
fn connect<'a>(_: &VBox, _: &'a dyn Runner, _: Duration) -> anyhow::Result<Console<'a>> {
    bail!("the VirtualBox harness drives the serial port over a local socket (Unix hosts only)")
}

/// Remove the VM, its disk and the ISO's registration; nothing to report
/// when there is no such VM.
fn discard(v: &VBox, runner: &dyn Runner) {
    let _ = runner.run("VBoxManage", &["controlvm", &v.name, "poweroff"]);
    let _ = runner.run("VBoxManage", &["unregistervm", &v.name, "--delete"]);
    let _ = runner.run(
        "VBoxManage",
        &["closemedium", "dvd", &v.iso.display().to_string()],
    );
}

pub fn run_vbox(v: &VBox, runner: &dyn Runner, dry_run: bool) -> anyhow::Result<()> {
    if !v.iso.is_file() && !dry_run {
        bail!("{} does not exist; build it first", v.iso.display());
    }
    std::fs::create_dir_all(&v.out)?;
    // Leftovers of an interrupted run: a VM of that name, its socket.
    discard(v, runner);
    let _ = std::fs::remove_file(v.serial());
    for cmd in create_commands(v) {
        let cmd: Vec<&str> = cmd.iter().map(String::as_str).collect();
        runner.run_checked("VBoxManage", &cmd)?;
    }
    runner.run_checked("VBoxManage", &["startvm", &v.name, "--type", "headless"])?;
    println!(
        "   live: log in as root on the serial port\n      {}",
        install_command()
    );
    let checks = checks();
    for (what, cmd, minutes) in &checks {
        println!("   dom0: {what} (timeout {minutes} min)\n      {cmd}");
    }
    if dry_run {
        return Ok(());
    }
    let screen = Screen { v, runner };
    let result = connect(v, runner, SERIAL_PATIENCE).and_then(|mut con| {
        drive_vbox(
            &mut con,
            &install_command(),
            &checks,
            &screen,
            BOOT_PATIENCE,
        )
    });
    // The console (and the VM it would kill when dropped) is gone by now:
    // the VM is powered off, or stopped by the console's drop.
    if !v.keep {
        discard(v, runner);
    }
    result?;
    println!(
        "\nVirtualBox: installed, booted to the login screen and powered off ({})",
        v.out.display()
    );
    Ok(())
}

/// The VM's screen and keyboard, through `VBoxManage controlvm`.
pub trait Display {
    /// Save a picture of the screen; a missing picture is not a failure.
    fn screenshot(&self, file: &str);
    /// Press keys (PS/2 scancodes, one string per key).
    fn press(&self, keys: &[&str]) -> anyhow::Result<()>;
}

struct Screen<'a> {
    v: &'a VBox,
    runner: &'a dyn Runner,
}

impl Display for Screen<'_> {
    fn screenshot(&self, file: &str) {
        let path = self.v.out.join(file).display().to_string();
        match self.runner.run_checked(
            "VBoxManage",
            &["controlvm", &self.v.name, "screenshotpng", &path],
        ) {
            Ok(_) => println!("   screenshot: {path}"),
            Err(e) => println!("note: no screenshot {file}: {e}"),
        }
    }

    fn press(&self, keys: &[&str]) -> anyhow::Result<()> {
        for key in keys {
            let args = [
                &["controlvm", &self.v.name, "keyboardputscancode"][..],
                &key.split(' ').collect::<Vec<_>>(),
            ]
            .concat();
            self.runner.run_checked("VBoxManage", &args)?;
            std::thread::sleep(Duration::from_millis(300));
        }
        Ok(())
    }
}

/// The console side of [`run_vbox`]: wait for the live medium (the
/// installer on the screen, nothing on the serial port yet), quit the
/// installer, log in on the serial getty that follows, install, reboot,
/// `checks` in the installed dom0, power off (also after a failed check,
/// so the VM's state stays consistent). `patience` bounds each boot.
fn drive_vbox(
    con: &mut Console<'_>,
    install: &str,
    checks: &[(&str, String, u64)],
    screen: &dyn Display,
    patience: Duration,
) -> anyhow::Result<()> {
    println!("\n==> vbox: booting the live medium");
    // The loader's banner is the last thing the serial port shows before
    // the installer takes the screen (the kernel boots muted).
    con.expect("Consoles:", patience)?;
    println!("\n==> vbox: quitting the installer for the serial login");
    let deadline = std::time::Instant::now() + patience;
    loop {
        // The last picture before the quit works is the installer.
        screen.screenshot("installer.png");
        screen.press(QUIT_INSTALLER_KEYS)?;
        if con.expect_within("login:", QUIT_ROUND.min(patience / 2))? {
            break;
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "no serial login after {}s of quitting the installer",
                patience.as_secs()
            );
        }
    }
    root_shell(con, None)?;
    run_steps(con, "vbox", [("install to ada0", install, 30)])?;

    println!("\n==> vbox: rebooting into the installed dom0");
    con.send_line("reboot")?;
    con.expect("login:", patience)?;
    root_shell(con, Some(SELFHOST_PASSWORD))?;
    let result = run_steps(
        con,
        "vbox",
        checks.iter().map(|(w, c, m)| (*w, c.as_str(), *m)),
    );
    screen.screenshot("login-screen.png");
    power_off(con, "vbox")?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use juanitos_core::sys::{CmdOutput, MockRunner};

    fn vbox(out: &std::path::Path) -> VBox {
        VBox {
            name: "jos-test".into(),
            iso: out.join("x.iso"),
            out: out.to_path_buf(),
            disk_size_gb: 20,
            memory_mb: 6144,
            cpus: 4,
            vram_mb: 128,
            keep: false,
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jos-vbox-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_vm_is_built_for_the_graphical_boot() {
        let v = vbox(std::path::Path::new("/w"));
        let cmds = create_commands(&v);
        let flat: Vec<String> = cmds.iter().map(|c| c.join(" ")).collect();
        assert!(flat[0].starts_with("createvm --name jos-test --ostype FreeBSD_64 --basefolder /w"));
        let modify = &flat[1];
        assert!(modify.contains("--firmware efi"), "{modify}");
        assert!(flat[1].contains("--graphicscontroller vmsvga --vram 128"));
        assert!(flat[1].contains("--uartmode1 server /w/serial.sock"));
        assert!(flat[1].contains("--boot1 disk --boot2 dvd"));
        assert!(flat[2].contains("--filename /w/jos-test/disk.vdi --size 20480"));
        assert!(flat[4].contains("--port 0 --device 0 --type hdd --medium /w/jos-test/disk.vdi"));
        assert!(flat[5].contains("--port 1 --device 0 --type dvddrive --medium /w/x.iso"));
        assert!(install_command().contains("--disk ada0"));
        assert!(!install_command().contains("--no-gui"));
        assert!(checks().iter().all(|c| c.2 > 0));
    }

    #[test]
    fn the_vm_state_decides_when_the_guest_is_gone() {
        assert_eq!(
            vm_state("name=\"x\"\nVMState=\"running\"\n"),
            Some("running")
        );
        assert_eq!(vm_state("VMState=running"), None);
        assert_eq!(vm_state("nothing"), None);
        let gone = |state: &str| {
            let m = MockRunner::new().respond(
                "VBoxManage",
                &["showvminfo"],
                CmdOutput::ok(format!("VMState=\"{state}\"\n")),
            );
            let mut g = VBoxGuest {
                runner: &m,
                name: "jos-test".into(),
            };
            g.finished().map(|r| r.map_err(|e| e.to_string()))
        };
        assert_eq!(gone("running"), None);
        assert_eq!(gone("stopping"), None);
        assert_eq!(gone("poweroff"), Some(Ok(())));
        assert_eq!(gone("aborted"), Some(Err("the VM is aborted".into())));
        // No answer, or no state in it, counts as still running (retried
        // until the deadline).
        for m in [MockRunner::new().missing("VBoxManage"), MockRunner::new()] {
            let mut g = VBoxGuest {
                runner: &m,
                name: "jos-test".into(),
            };
            assert!(g.finished().is_none());
            g.kill();
            assert_eq!(
                m.calls().last().unwrap(),
                "VBoxManage controlvm jos-test poweroff"
            );
        }
    }

    /// The loader's banner, which the live medium prints before its
    /// installer takes the screen.
    const BANNER: &str = "printf 'Consoles: EFI console\\n'";

    /// A screen that remembers what was done to it; `deaf` makes its
    /// keyboard fail.
    #[derive(Default)]
    struct FakeScreen {
        log: std::cell::RefCell<Vec<String>>,
        deaf: bool,
    }

    impl Display for FakeScreen {
        fn screenshot(&self, file: &str) {
            self.log.borrow_mut().push(file.to_string());
        }
        fn press(&self, keys: &[&str]) -> anyhow::Result<()> {
            if self.deaf {
                bail!("no keyboard");
            }
            self.log.borrow_mut().push(format!("press {}", keys.len()));
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn dialogue_with_a_guest() {
        use crate::qemu::tests::{guest, DEAF, EVAL, LOGIN, POWEROFF, REBOOT};
        let checks = vec![("first", "true".to_string(), 1)];
        let patience = Duration::from_secs(5);
        let screen = FakeScreen::default();
        drive_vbox(
            &mut guest(&[REBOOT, POWEROFF, BANNER, LOGIN, EVAL]),
            "true",
            &checks,
            &screen,
            patience,
        )
        .unwrap();
        assert_eq!(
            *screen.log.borrow(),
            ["installer.png", "press 4", "login-screen.png"]
        );

        // The installer not reacting to the keys in time (they are pressed
        // again every round), and a keyboard that does not work at all.
        let silent = [BANNER, "sleep 3"];
        let screen = FakeScreen::default();
        let err = drive_vbox(
            &mut guest(&silent),
            "true",
            &checks,
            &screen,
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "no serial login after 0s of quitting the installer"
        );
        let presses = screen
            .log
            .borrow()
            .iter()
            .filter(|l| *l == "press 4")
            .count();
        assert!(presses >= 2);
        let deaf_screen = FakeScreen {
            deaf: true,
            ..FakeScreen::default()
        };
        let err =
            drive_vbox(&mut guest(&silent), "true", &checks, &deaf_screen, patience).unwrap_err();
        assert_eq!(err.to_string(), "no keyboard");

        let failing = [("second", "false".to_string(), 1)];
        let closing = [("close", DEAF.to_string(), 1)];
        /// Name, guest script parts, install command, checks, error.
        type Case<'a> = (
            &'a str,
            &'a [&'a str],
            &'a str,
            &'a [(&'a str, String, u64)],
            &'a str,
        );
        let deaf = "writing to the guest's console";
        let cases: &[Case] = &[
            (
                "no output at all",
                &[],
                "true",
                &checks,
                "the guest's console closed",
            ),
            (
                "gone after the banner",
                &[BANNER],
                "true",
                &checks,
                "the guest's console closed",
            ),
            (
                "deaf at the live login",
                &[DEAF, BANNER, "printf 'login: '"],
                "true",
                &checks,
                deaf,
            ),
            (
                "failed install",
                &[REBOOT, BANNER, LOGIN, EVAL],
                "false",
                &checks,
                "vbox step failed: install to ada0",
            ),
            (
                "deaf after the install",
                &[REBOOT, BANNER, LOGIN, EVAL],
                DEAF,
                &checks,
                deaf,
            ),
            (
                "no reboot",
                &["reboot() { exit 0; }", BANNER, LOGIN, EVAL],
                "true",
                &checks,
                "the guest's console closed",
            ),
            (
                "deaf at the dom0 login",
                &[
                    "reboot() { exec 0<&-; printf 'login: '; }",
                    BANNER,
                    LOGIN,
                    EVAL,
                ],
                "true",
                &checks,
                deaf,
            ),
            (
                "failing check, reported after the power-off",
                &[REBOOT, POWEROFF, BANNER, LOGIN, EVAL],
                "true",
                &failing,
                "vbox step failed: second",
            ),
            (
                "deaf at power off",
                &[REBOOT, POWEROFF, BANNER, LOGIN, EVAL],
                "true",
                &closing,
                deaf,
            ),
        ];
        for (name, parts, install, checks, want) in cases {
            let screen = FakeScreen::default();
            let err =
                drive_vbox(&mut guest(parts), install, checks, &screen, patience).unwrap_err();
            assert_eq!((name, err.to_string()), (name, want.to_string()));
        }
    }

    #[test]
    fn the_screen_goes_through_vboxmanage() {
        let dir = PathBuf::from("/w");
        let v = vbox(&dir);
        let m = MockRunner::new().respond(
            "VBoxManage",
            &["controlvm", "jos-test", "screenshotpng"],
            CmdOutput::failed(1, "no screen"),
        );
        let screen = Screen { v: &v, runner: &m };
        screen.screenshot("a.png");
        screen.press(&["1c 9c"]).unwrap();
        assert_eq!(
            m.calls(),
            [
                "VBoxManage controlvm jos-test screenshotpng /w/a.png",
                "VBoxManage controlvm jos-test keyboardputscancode 1c 9c",
            ]
        );
        let broken = MockRunner::new().missing("VBoxManage");
        let screen = Screen {
            v: &v,
            runner: &broken,
        };
        assert!(screen.press(QUIT_INSTALLER_KEYS).is_err());
    }

    #[test]
    fn dry_run_prints_the_plan_and_needs_no_iso() {
        let dir = scratch("dry");
        let v = vbox(&dir);
        let m = MockRunner::new();
        run_vbox(&v, &m, true).unwrap();
        let calls = m.calls();
        // Leftovers first, then the VM, then the start; nothing after.
        assert!(calls[0].starts_with("VBoxManage controlvm jos-test poweroff"));
        assert!(calls[1].starts_with("VBoxManage unregistervm jos-test --delete"));
        assert!(calls[2].starts_with("VBoxManage closemedium dvd"));
        assert!(calls[3].starts_with("VBoxManage createvm"));
        assert_eq!(
            calls.last().unwrap(),
            "VBoxManage startvm jos-test --type headless"
        );
        assert_eq!(calls.len(), 3 + create_commands(&v).len() + 1);

        // Without --dry-run the ISO must exist, and every VBoxManage call
        // must work, the start included.
        let err = run_vbox(&v, &m, false).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
        std::fs::write(v.iso.clone(), b"iso").unwrap();
        for (call, what) in [(3, "createvm"), (calls.len() - 1, "startvm")] {
            let failing = MockRunner::new().fail_call(call);
            let err = run_vbox(&v, &failing, false).unwrap_err();
            assert!(err.to_string().contains(what), "{err}");
        }
        // An output directory that cannot be made.
        std::fs::write(dir.join("file"), b"").unwrap();
        let mut under_file = vbox(&dir);
        under_file.out = dir.join("file/out");
        assert!(run_vbox(&under_file, &m, true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Serve `out/serial.sock` with `sh` playing the guest, as VirtualBox
    /// would with the real serial port.
    #[cfg(unix)]
    fn serve_guest(sock: &std::path::Path, script: String, delay: Duration) {
        use std::os::unix::net::UnixListener;
        use std::process::{Command, Stdio};
        let sock = sock.to_path_buf();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let listener = UnixListener::bind(&sock).unwrap();
            let (stream, _) = listener.accept().unwrap();
            let mut child = Command::new("sh")
                .args(["-c", &script])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let to_child = child.stdin.take().unwrap();
            let from_child = child.stdout.take().unwrap();
            let up = stream.try_clone().unwrap();
            std::thread::spawn(move || pump(up, to_child));
            pump(from_child, &stream);
            // The guest is gone: hang up, as VirtualBox does when the VM
            // powers off.
            let _ = stream.shutdown(std::net::Shutdown::Both);
            let _ = child.wait();
        });
    }

    /// Byte-by-byte relay (`std::io::copy` would splice, which does not
    /// deliver a socket's bytes to a pipe as they come).
    #[cfg(unix)]
    fn pump(mut from: impl std::io::Read, mut to: impl std::io::Write) {
        let mut buf = [0u8; 256];
        loop {
            let n = from.read(&mut buf).unwrap_or(0);
            if n == 0 || to.write_all(&buf[..n]).is_err() {
                break;
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_whole_run_over_the_serial_socket() {
        use crate::qemu::tests::{LOGIN, REBOOT};
        let dir = scratch("run");
        let mut v = vbox(&dir);
        std::fs::write(&v.iso, b"iso").unwrap();
        // After the live login, every command the host sends is acknowledged
        // as a success without running it (they are the real installer and
        // FreeBSD commands); `reboot` and `poweroff` are played, never run.
        let ack = "while read -r line; do case $line in poweroff) exit 0;; \
                   reboot) reboot; continue;; esac; eval \"echo ${line##*; echo }\"; done";
        let script = [
            "reboot() { exit 9; }; poweroff() { exit 9; }",
            REBOOT,
            BANNER,
            LOGIN,
            ack,
        ]
        .join("\n");
        let powered_off = || {
            MockRunner::new().respond(
                "VBoxManage",
                &["showvminfo"],
                CmdOutput::ok("VMState=\"poweroff\"\n"),
            )
        };

        // The socket appears a little after startvm, as with VirtualBox
        // (and after the run removed a stale one).
        let delay = Duration::from_millis(300);
        serve_guest(&v.serial(), script.clone(), delay);
        let m = powered_off();
        run_vbox(&v, &m, false).unwrap();
        let calls = m.calls();
        assert!(calls
            .iter()
            .any(|c| c.contains("screenshotpng") && c.ends_with("installer.png")));
        assert!(calls
            .iter()
            .any(|c| c.ends_with("keyboardputscancode 2a 0f 8f aa")));
        assert!(calls.iter().any(|c| c.ends_with("login-screen.png")));
        assert!(
            calls
                .iter()
                .filter(|c| c.starts_with("VBoxManage unregistervm"))
                .count()
                == 2
        );
        assert!(std::fs::read_to_string(dir.join("console.log"))
            .unwrap()
            .contains("login:"));

        // --keep leaves the VM; a failed screenshot is only a note.
        v.keep = true;
        serve_guest(&v.serial(), script, delay);
        let m = powered_off().respond(
            "VBoxManage",
            &["controlvm", "jos-test", "screenshotpng"],
            CmdOutput::failed(1, "no screen"),
        );
        run_vbox(&v, &m, false).unwrap();
        assert_eq!(
            m.calls()
                .iter()
                .filter(|c| c.starts_with("VBoxManage unregistervm"))
                .count(),
            1
        );

        // A guest that says nothing: the failure is reported after the VM
        // was discarded.
        v.keep = false;
        serve_guest(&v.serial(), "exit 0".to_string(), delay);
        let m = powered_off();
        let err = run_vbox(&v, &m, false).unwrap_err();
        assert_eq!(err.to_string(), "the guest's console closed");
        assert!(m
            .calls()
            .last()
            .unwrap()
            .starts_with("VBoxManage closemedium dvd"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn connection_failures() {
        let dir = scratch("nosock");
        let v = vbox(&dir);
        let m = MockRunner::new();
        let failure = |patience: u64| {
            connect(&v, &m, Duration::from_millis(patience))
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        // No socket in time.
        let err = failure(300);
        assert!(err.contains("connecting to the VM's serial port"), "{err}");
        // A socket, but no place for the console log.
        std::fs::create_dir_all(dir.join("console.log")).unwrap();
        serve_guest(&v.serial(), "exit 0".to_string(), Duration::ZERO);
        let err = failure(5000);
        assert!(err.contains("creating the console log"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
