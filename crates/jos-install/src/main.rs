//! `jos-install` — the JuanitOS installer.
//!
//! Runs on the live ISO. First it runs the same host checks as `jos doctor`:
//! the live medium *is* the dom0 that will be installed, so if bhyve, the
//! IOMMU and the kernel modules work here, they work after installation.
//! Then it partitions a disk, creates a ZFS pool and extracts the dom0
//! tarball the medium booted from.
//!
//! Two front ends drive the same [`install::Installer`]:
//!
//! * a graphical wizard on the `vt(4)` framebuffer (`--ui gui`, the
//!   default on the live medium), modelled on Calamares and Anaconda:
//!   see `wizard.rs`, `ui.rs` and `gui.rs`;
//! * line-oriented prompts (`--ui text`), for serial consoles, and flags
//!   only (`--non-interactive`) for automated installs.

mod disks;
mod gui;
mod install;
mod prompt;
mod ui;
mod wizard;

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Parser, ValueEnum};

use juanitos_core::doctor::{self, Check};
use juanitos_core::pci::{self, PciAddr, PciDevice};
use juanitos_core::sys::{DryRunner, RealRunner, Runner};

use crate::disks::{list_disks, Disk};
use crate::install::{dist_available, InstallPlan, Installer};
use crate::prompt::Prompter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Ui {
    /// The graphical wizard when a framebuffer console is available,
    /// otherwise the text prompts.
    Auto,
    Gui,
    Text,
}

#[derive(Parser, Debug)]
#[command(name = "jos-install", version, about = "Install JuanitOS")]
struct Cli {
    /// Print the host commands instead of running them.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Running from /etc/rc.local on the live medium: offer to reboot when
    /// done and never exit with an error status.
    #[arg(long)]
    live: bool,

    /// Answer no questions; every value must come from flags.
    #[arg(long)]
    non_interactive: bool,

    /// Front end for interactive installs.
    #[arg(long, value_enum, default_value_t = Ui::Auto)]
    ui: Ui,

    /// Virtual terminal the graphical installer draws on (10 = ttyv9).
    #[arg(long, default_value_t = 10)]
    vt: i32,

    /// Render every page of the graphical installer as PNG files into DIR
    /// and exit (works on any host; used for the documentation).
    #[arg(long, value_name = "DIR")]
    screenshots: Option<PathBuf>,

    /// Target disk (e.g. ada0, nda0, vtbd0). Erased completely.
    #[arg(long)]
    disk: Option<String>,

    #[arg(long, default_value = "zroot")]
    pool: String,

    #[arg(long, default_value = "juanitos")]
    hostname: String,

    /// Swap partition size; 0 disables swap.
    #[arg(long, default_value_t = 4)]
    swap_gb: u64,

    /// Console-only dom0 (no COSMIC services at boot).
    #[arg(long)]
    no_gui: bool,

    /// Do not add a serial console to loader.conf.
    #[arg(long)]
    no_serial: bool,

    /// Root password (prompted when omitted and interactive).
    #[arg(long)]
    root_password: Option<String>,

    /// First user (member of wheel/video); password via --user-password.
    #[arg(long)]
    user: Option<String>,

    #[arg(long)]
    user_password: Option<String>,

    /// Reserve this PCI device for passthrough (`pptdevs`); repeatable.
    /// Default: the NVIDIA GPU (and its audio) when another GPU drives the
    /// dom0, plus devices already bound to ppt.
    #[arg(long = "pci", value_name = "B/S/F", value_parser = parse_pci)]
    pci: Vec<PciAddr>,

    /// Reserve no PCI device.
    #[arg(long, conflicts_with = "pci")]
    no_pci: bool,

    /// Proceed even when the host checks report failures.
    #[arg(long)]
    ignore_doctor: bool,

    /// Directory with juanitos-dom0.txz and MANIFEST.
    #[arg(long, default_value = juanitos_core::dom0::DIST_DIR)]
    dist: String,

    /// Mount point for the new pool during installation.
    #[arg(long, default_value = "/mnt")]
    target: String,
}

fn parse_pci(s: &str) -> Result<PciAddr, String> {
    s.parse().map_err(|e: juanitos_core::Error| e.to_string())
}

/// What the live medium found on this machine.
struct Detected {
    checks: Vec<Check>,
    /// `checks` rendered for the console and the install report.
    report: String,
    disks: Vec<Disk>,
    /// Passthrough candidates (`pci::passthrough_candidates`).
    devices: Vec<PciDevice>,
}

fn main() {
    let cli = Cli::parse();
    let live = cli.live;
    let code = match run(cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("jos-install: {e:#}");
            1
        }
    };
    if live {
        // rc.local continues to the console login either way.
        std::process::exit(0);
    }
    std::process::exit(code);
}

fn run(cli: Cli) -> anyhow::Result<()> {
    if let Some(dir) = &cli.screenshots {
        for file in ui::screenshots(dir, ui::LOGICAL_W as u32, ui::LOGICAL_H as u32)? {
            println!("wrote {file}");
        }
        return Ok(());
    }

    let dry = DryRunner::new();
    let real = RealRunner;
    let runner: &dyn Runner = if cli.dry_run { &dry } else { &real };
    let interactive = !cli.non_interactive;

    banner();

    // 1. Doctor: this is the dom0 compatibility validation.
    let cfg = juanitos_core::GlobalConfig {
        pool: cli.pool.clone(),
        ..Default::default()
    };
    let checks = doctor::run_host_checks(runner, &cfg);
    let report = render_checks(&checks);
    let pci_devices = pci_devices(runner);

    // 2. Defaults for the plan, from the flags.
    let user = match (cli.user.clone(), cli.user_password.clone()) {
        (Some(u), Some(p)) => Some((u, p)),
        (Some(u), None) if !interactive => bail!("--user {u} requires --user-password"),
        (Some(u), None) => Some((u, String::new())),
        (None, _) => None,
    };
    let plan = InstallPlan {
        disk: cli.disk.clone().unwrap_or_default(),
        pool: cli.pool.clone(),
        hostname: cli.hostname.clone(),
        swap_gb: cli.swap_gb,
        gui: !cli.no_gui,
        serial_console: !cli.no_serial,
        gpu_module: gpu_module_from(&pci_devices),
        pptdevs: passthrough_from(&cli, &pci_devices),
        root_password: cli.root_password.clone(),
        user,
        dist_dir: cli.dist.clone(),
        target: cli.target.clone(),
        ..InstallPlan::default()
    };

    if !cli.dry_run && !dist_available(&plan.dist_dir) {
        bail!(
            "{} does not contain the dom0 distribution; run this from the JuanitOS live medium \
             or pass --dist",
            plan.dist_dir
        );
    }
    let hw = Detected {
        checks,
        report,
        disks: list_disks(runner).context("listing disks")?,
        devices: pci::passthrough_candidates(&pci_devices),
    };

    if interactive && wants_gui(cli.ui, cli.dry_run)? {
        match run_graphical(&cli, runner, plan.clone(), &hw) {
            Ok(()) => return Ok(()),
            Err(e) if cli.ui == Ui::Auto => {
                eprintln!("jos-install: graphical installer unavailable ({e:#}); using text mode");
            }
            Err(e) => return Err(e),
        }
    }

    let stdin = std::io::stdin();
    let mut prompter = Prompter::new(stdin.lock(), std::io::stdout(), runner);
    text_install(&cli, runner, plan, &hw, &mut prompter)
}

/// Every PCI device; none where `pciconf` is missing (a development host).
fn pci_devices(runner: &dyn Runner) -> Vec<PciDevice> {
    pci::list_devices(runner).unwrap_or_default()
}

/// Devices to reserve: `--no-pci`, else `--pci`, else the default proposal.
fn passthrough_from(cli: &Cli, devices: &[PciDevice]) -> Vec<PciAddr> {
    if cli.no_pci {
        return Vec::new();
    }
    if cli.pci.is_empty() {
        return pci::default_passthrough(devices);
    }
    let mut addrs = cli.pci.clone();
    addrs.sort();
    addrs.dedup();
    addrs
}

/// The KMS driver for the dom0's display: the first GPU `pciconf` lists
/// that `drm-kmod` has a driver for. A GPU meant for passthrough (the
/// NVIDIA card of the `gpu` qube) has none, so it is skipped.
fn gpu_module_from(devices: &[PciDevice]) -> Option<String> {
    devices
        .iter()
        .filter(|d| d.is_gpu())
        .find_map(|d| juanitos_core::dom0::gpu_kernel_module(d.vendor))
        .map(str::to_string)
}

/// Should an interactive install use the graphical front end?
fn wants_gui(ui: Ui, dry_run: bool) -> anyhow::Result<bool> {
    let framebuffer = cfg!(target_os = "freebsd") && std::path::Path::new("/dev/ttyv0").exists();
    match ui {
        Ui::Text => Ok(false),
        Ui::Auto => Ok(framebuffer && !dry_run),
        Ui::Gui if framebuffer => Ok(true),
        Ui::Gui => bail!(
            "the graphical installer needs a FreeBSD framebuffer console; \
             use --screenshots DIR to preview it on this host"
        ),
    }
}

#[cfg(target_os = "freebsd")]
fn run_graphical(
    cli: &Cli,
    runner: &dyn Runner,
    plan: InstallPlan,
    hw: &Detected,
) -> anyhow::Result<()> {
    use jos_fb::Display;

    let mut display = jos_fb::vt::VtDisplay::open(cli.vt)?;
    let mut wizard = wizard::Wizard::new(
        plan,
        hw.checks.clone(),
        hw.disks.clone(),
        hw.devices.clone(),
    );
    let outcome = gui::run(
        &mut display,
        &mut wizard,
        runner,
        cli.dry_run,
        &hw.report,
        gui::LoopOptions::default(),
    );
    display.restore_console()?;
    drop(display);
    match outcome? {
        gui::Outcome::Reboot => {
            runner.run("shutdown", &["-r", "now"])?;
        }
        gui::Outcome::Exit => {
            if let wizard::InstallState::Failed(e) = &wizard.install {
                bail!("installation failed: {e}");
            }
        }
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn run_graphical(
    _cli: &Cli,
    _runner: &dyn Runner,
    _plan: InstallPlan,
    _hw: &Detected,
) -> anyhow::Result<()> {
    bail!("no framebuffer console on this host")
}

/// The line-oriented installer (serial consoles, `--ui text`) and the
/// unattended one (`--non-interactive`).
fn text_install<R: std::io::BufRead, W: std::io::Write>(
    cli: &Cli,
    runner: &dyn Runner,
    mut plan: InstallPlan,
    hw: &Detected,
    p: &mut Prompter<'_, R, W>,
) -> anyhow::Result<()> {
    let interactive = !cli.non_interactive;
    let disks = &hw.disks;
    p.say(&hw.report)?;
    if !doctor::all_passed(&hw.checks) {
        p.say(
            "Some checks FAILED: this machine (or its firmware settings) cannot run\n\
             JuanitOS qubes as-is. Typical fixes: enable VT-x/AMD-V and VT-d/AMD-Vi\n\
             in the firmware setup.\n\n",
        )?;
        if cli.ignore_doctor {
            p.say("Continuing anyway (--ignore-doctor).\n\n")?;
        } else if interactive {
            if !p.ask_yes_no("Install anyway?", false)? {
                bail!("aborted: host checks failed");
            }
        } else {
            bail!("host checks failed (use --ignore-doctor to proceed)");
        }
    } else {
        p.say("All host checks passed: this hardware can run JuanitOS.\n\n")?;
    }

    if plan.disk.is_empty() {
        if !interactive {
            bail!("--disk is required with --non-interactive");
        }
        plan.disk = p.choose_disk(disks)?;
    } else if !cli.dry_run && !disks.iter().any(|d| d.name == plan.disk) {
        bail!(
            "disk {} not found (available: {})",
            plan.disk,
            disks
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    if interactive {
        plan.hostname = p.prompt_default("Hostname", &plan.hostname)?;
        plan.swap_gb = p
            .prompt_default("Swap size in GiB (0 = none)", &plan.swap_gb.to_string())?
            .parse()
            .context("swap size must be a number")?;
        plan.gui = p.ask_yes_no("Start the COSMIC desktop at boot?", plan.gui)?;
        if !hw.devices.is_empty() {
            plan.pptdevs = p.choose_devices(&hw.devices, &plan.pptdevs)?;
        }
        if plan.root_password.is_none() {
            plan.root_password = Some(p.password("Root password")?);
        }
        match &mut plan.user {
            Some((name, pw)) if pw.is_empty() => {
                *pw = p.password(&format!("Password for {name}"))?;
            }
            Some(_) => {}
            None => {
                let name = p.prompt_default("Create a user (empty to skip)", "")?;
                if !name.is_empty() {
                    let pw = p.password(&format!("Password for {name}"))?;
                    plan.user = Some((name, pw));
                }
            }
        }
    }

    plan.validate()?;
    p.say(&format!("\nInstallation plan:\n{}\n", plan.summary()))?;
    if interactive && !p.ask_yes_no("Proceed? This erases the disk.", false)? {
        bail!("aborted");
    }

    // 3. Install.
    Installer::new(runner, cli.dry_run, &plan).run(&hw.report)?;

    if cli.live && interactive && p.ask_yes_no("Reboot now?", true)? {
        runner.run("shutdown", &["-r", "now"])?;
    }
    Ok(())
}

fn banner() {
    println!(
        "\n  JuanitOS installer — FreeBSD + bhyve, QubesOS style\n  \
         The live system you are running is the exact dom0 that gets installed.\n"
    );
}

fn render_checks(checks: &[Check]) -> String {
    let mut s = String::from("Host checks:\n");
    for c in checks {
        s.push_str(&format!(
            "  [{:4}] {:<22} {}\n",
            c.status.as_str(),
            c.name,
            c.detail
        ));
    }
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use juanitos_core::doctor::Status;
    use juanitos_core::sys::MockRunner;

    fn cli(args: &[&str]) -> Cli {
        let mut argv = vec!["jos-install"];
        argv.extend_from_slice(args);
        Cli::parse_from(argv)
    }

    fn disks() -> Vec<Disk> {
        vec![Disk {
            name: "vtbd0".into(),
            size_bytes: 20 << 30,
            description: "VirtIO".into(),
        }]
    }

    fn checks(status: Status) -> Vec<Check> {
        vec![Check {
            name: "iommu".into(),
            status,
            detail: "x".into(),
        }]
    }

    fn plan_for(c: &Cli) -> InstallPlan {
        InstallPlan {
            disk: c.disk.clone().unwrap_or_default(),
            root_password: c.root_password.clone(),
            target: std::env::temp_dir()
                .join(format!("jos-install-main-{}", std::process::id()))
                .to_string_lossy()
                .into_owned(),
            ..Default::default()
        }
    }

    fn hw(checks: Vec<Check>, devices: Vec<PciDevice>) -> Detected {
        Detected {
            checks,
            report: "REPORT\n".into(),
            disks: disks(),
            devices,
        }
    }

    fn text_with(
        c: &Cli,
        m: &MockRunner,
        plan: InstallPlan,
        hw: &Detected,
        input: &str,
    ) -> (anyhow::Result<()>, String) {
        let mut out = Vec::new();
        let r = {
            let mut p = Prompter::new(input.as_bytes(), &mut out, m);
            text_install(c, m, plan, hw, &mut p)
        };
        (r, String::from_utf8_lossy(&out).into_owned())
    }

    fn text(
        c: &Cli,
        m: &MockRunner,
        checks: &[Check],
        input: &str,
    ) -> (anyhow::Result<()>, String) {
        text_with(c, m, plan_for(c), &hw(checks.to_vec(), vec![]), input)
    }

    const PCICONF: &str = "vgapci0@pci0:1:0:0:\tclass=0x030000 rev=0xa1 hdr=0x00 vendor=0x10de device=0x1b80 subvendor=0x1458 subdevice=0x3702\n\
                           hdac0@pci0:1:0:1:\tclass=0x040300 rev=0xa1 hdr=0x00 vendor=0x10de device=0x10f0 subvendor=0x1458 subdevice=0x3702\n\
                           vgapci1@pci0:6:0:0:\tclass=0x030000 rev=0xc8 hdr=0x00 vendor=0x1002 device=0x1638 subvendor=0x1d05 subdevice=0x1127\n\
                           igb0@pci0:5:0:0:\tclass=0x020000 rev=0x03 hdr=0x00 vendor=0x8086 device=0x1533 subvendor=0x8086 subdevice=0x0000\n";

    #[test]
    fn render_checks_table() {
        let s = render_checks(&checks(Status::Fail));
        assert!(s.starts_with("Host checks:\n"));
        assert!(s.contains("[FAIL] iommu"));
    }

    #[test]
    fn gpu_detection_picks_the_first_kms_capable_card() {
        use juanitos_core::sys::CmdOutput;
        let m = MockRunner::new().respond("pciconf", &["-l", "-v"], CmdOutput::ok(PCICONF));
        let devices = pci_devices(&m);
        assert_eq!(devices.len(), 4);
        // The NVIDIA card (for the gpu qube) is skipped, the AMD one drives the console.
        assert_eq!(gpu_module_from(&devices).as_deref(), Some("amdgpu"));
        assert_eq!(gpu_module_from(&devices[..1]), None);
        assert_eq!(gpu_module_from(&[]), None);
        // No pciconf (a development host): no device, no error.
        let m = MockRunner::new().respond("pciconf", &["-l", "-v"], CmdOutput::failed(1, ""));
        assert!(pci_devices(&m).is_empty());
    }

    #[test]
    fn passthrough_flags() {
        let devices = pci::parse_pciconf(PCICONF);
        let nvidia = [PciAddr::new(1, 0, 0), PciAddr::new(1, 0, 1)];
        assert_eq!(passthrough_from(&cli(&[]), &devices), nvidia);
        assert!(passthrough_from(&cli(&["--no-pci"]), &devices).is_empty());
        assert_eq!(
            passthrough_from(
                &cli(&["--pci", "5/0/0", "--pci", "0000:01:00.0", "--pci", "5:0:0"]),
                &devices
            ),
            [PciAddr::new(1, 0, 0), PciAddr::new(5, 0, 0)]
        );
        let mut argv = vec!["jos-install", "--pci", "1/0/0", "--no-pci"];
        assert!(Cli::try_parse_from(&argv).is_err(), "conflicting flags");
        argv.truncate(2);
        argv.push("nope");
        let err = Cli::try_parse_from(&argv).unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn interactive_text_install_reserves_devices() {
        let c = cli(&["--dry-run"]);
        let m = MockRunner::new();
        let devices = pci::passthrough_candidates(&pci::parse_pciconf(PCICONF));
        let plan = InstallPlan {
            pptdevs: pci::default_passthrough(&devices),
            ..plan_for(&c)
        };
        // disk 1; hostname; swap; desktop; devices: the NIC only; root
        // password twice; no user; proceed.
        let input = "1\n\n\n\n4\npw\npw\n\ny\n";
        let (r, out) = text_with(&c, &m, plan, &hw(checks(Status::Ok), devices), input);
        r.unwrap();
        assert!(out.contains("[1 2]: "), "NVIDIA proposed by default: {out}");
        assert!(out.contains("passthru  5/0/0\n"), "{out}");
    }

    #[test]
    fn ui_selection() {
        assert!(!wants_gui(Ui::Text, false).unwrap());
        if !cfg!(target_os = "freebsd") {
            assert!(!wants_gui(Ui::Auto, false).unwrap());
            assert!(wants_gui(Ui::Gui, false).is_err());
        }
        assert!(!wants_gui(Ui::Auto, true).unwrap());
    }

    #[test]
    fn non_interactive_install() {
        let c = cli(&[
            "--dry-run",
            "--non-interactive",
            "--disk",
            "vtbd0",
            "--root-password",
            "pw",
        ]);
        let m = MockRunner::new();
        let (r, out) = text(&c, &m, &checks(Status::Ok), "");
        r.unwrap();
        assert!(out.contains("REPORT"));
        assert!(out.contains("All host checks passed"));
        assert!(out.contains("Installation plan"));
        assert!(m.calls().iter().any(|c| c == "gpart create -s gpt vtbd0"));
    }

    #[test]
    fn non_interactive_refusals() {
        let m = MockRunner::new();
        let c = cli(&["--dry-run", "--non-interactive", "--disk", "vtbd0"]);
        let (r, _) = text(&c, &m, &checks(Status::Fail), "");
        assert!(r.unwrap_err().to_string().contains("--ignore-doctor"));

        let c = cli(&["--dry-run", "--non-interactive", "--ignore-doctor"]);
        let (r, out) = text(&c, &m, &checks(Status::Fail), "");
        assert!(r.unwrap_err().to_string().contains("--disk is required"));
        assert!(out.contains("Continuing anyway"));

        // A disk that does not exist (real run).
        let c = cli(&["--non-interactive", "--disk", "ada9"]);
        let (r, _) = text(&c, &m, &checks(Status::Ok), "");
        assert!(r.unwrap_err().to_string().contains("not found"));
        assert!(m.calls().is_empty());
    }

    #[test]
    fn interactive_text_install() {
        let c = cli(&["--dry-run", "--live"]);
        let m = MockRunner::new();
        // failed checks: install anyway; disk 1; hostname; swap; desktop;
        // root password twice; user; user password twice; proceed; reboot.
        let input = "y\n1\nbox\n0\nn\nr00t\nr00t\nalice\npw\npw\ny\ny\n";
        let (r, out) = text(&c, &m, &checks(Status::Fail), input);
        r.unwrap();
        assert!(out.contains("vtbd0"));
        assert!(out.contains("hostname  box"));
        assert!(out.contains("console only"));
        let calls = m.calls();
        assert!(calls
            .iter()
            .any(|c| c.starts_with("chroot") && c.contains("useradd alice")));
        assert!(!calls.iter().any(|c| c.contains("freebsd-swap")));
        assert_eq!(calls.last().map(String::as_str), Some("shutdown -r now"));
    }

    #[test]
    fn interactive_aborts() {
        let m = MockRunner::new();
        let c = cli(&["--dry-run"]);
        let (r, _) = text(&c, &m, &checks(Status::Fail), "n\n");
        assert!(r.unwrap_err().to_string().contains("host checks failed"));
        let (r, _) = text(&c, &m, &checks(Status::Ok), "1\n\nnot-a-number\n");
        assert!(r.unwrap_err().to_string().contains("swap size"));
        // Declining at the final question touches nothing.
        let input = "1\n\n\n\npw\npw\n\nn\n";
        let (r, _) = text(&c, &m, &checks(Status::Ok), input);
        assert_eq!(r.unwrap_err().to_string(), "aborted");
        assert!(!m.calls().iter().any(|c| c.starts_with("gpart")));
    }

    #[test]
    fn user_from_flags_gets_its_password_prompted() {
        let m = MockRunner::new();
        let c = cli(&[
            "--dry-run",
            "--disk",
            "vtbd0",
            "--root-password",
            "r",
            "--user",
            "bob",
        ]);
        let mut p = plan_for(&c);
        p.user = Some(("bob".into(), String::new()));
        let mut out = Vec::new();
        let input: &[u8] = b"\n\n\nsecret\nsecret\ny\n";
        let r = {
            let mut pr = Prompter::new(input, &mut out, &m);
            text_install(&c, &m, p, &hw(checks(Status::Ok), vec![]), &mut pr)
        };
        r.unwrap();
        assert!(String::from_utf8_lossy(&out).contains("Password for bob"));
    }

    #[test]
    fn screenshots_flag_writes_pngs() {
        let dir =
            std::env::temp_dir().join(format!("jos-install-cli-shots-{}", std::process::id()));
        let d = dir.to_string_lossy().into_owned();
        run(cli(&["--screenshots", &d])).unwrap();
        assert!(dir.join("installer-1-welcome.png").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_without_password_is_rejected_non_interactively() {
        let err = run(cli(&[
            "--dry-run",
            "--non-interactive",
            "--user",
            "alice",
            "--disk",
            "vtbd0",
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("--user-password"));
    }
}
