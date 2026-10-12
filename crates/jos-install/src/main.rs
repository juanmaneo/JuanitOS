//! `jos-install` — the JuanitOS installer.
//!
//! Runs on the live ISO. First it runs the same host checks as `jos doctor`:
//! the live medium *is* the dom0 that will be installed, so if bhyve, the
//! IOMMU and the kernel modules work here, they work after installation.
//! Then it partitions a disk, creates a ZFS pool and extracts the dom0
//! tarball the medium booted from.

mod disks;
mod install;

use std::io::{self, BufRead, Write};

use anyhow::{bail, Context};
use clap::Parser;

use juanitos_core::doctor::{self, Check};
use juanitos_core::sys::{DryRunner, RealRunner, Runner};

use crate::disks::{list_disks, Disk};
use crate::install::{dist_available, InstallPlan, Installer};

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
    let dry = DryRunner::new();
    let real = RealRunner;
    let runner: &dyn Runner = if cli.dry_run { &dry } else { &real };
    let interactive = !cli.non_interactive;
    let live = cli.live;

    banner();

    // 1. Doctor: this is the dom0 compatibility validation.
    let cfg = juanitos_core::GlobalConfig {
        pool: cli.pool.clone(),
        ..Default::default()
    };
    let checks = doctor::run_host_checks(runner, &cfg);
    let report = render_checks(&checks);
    print!("{report}");
    if !doctor::all_passed(&checks) {
        println!(
            "Some checks FAILED: this machine (or its firmware settings) cannot run\n\
             JuanitOS qubes as-is. Typical fixes: enable VT-x/AMD-V and VT-d/AMD-Vi\n\
             in the firmware setup.\n"
        );
        if cli.ignore_doctor {
            println!("Continuing anyway (--ignore-doctor).\n");
        } else if interactive {
            if !ask_yes_no("Install anyway?", false)? {
                bail!("aborted: host checks failed");
            }
        } else {
            bail!("host checks failed (use --ignore-doctor to proceed)");
        }
    } else {
        println!("All host checks passed: this hardware can run JuanitOS.\n");
    }

    // 2. Plan.
    let user = match (cli.user, cli.user_password) {
        (Some(u), Some(p)) => Some((u, p)),
        (Some(u), None) if !interactive => bail!("--user {u} requires --user-password"),
        (Some(u), None) => Some((u, String::new())),
        (None, _) => None,
    };
    let mut plan = InstallPlan {
        disk: cli.disk.unwrap_or_default(),
        pool: cli.pool,
        hostname: cli.hostname,
        swap_gb: cli.swap_gb,
        gui: !cli.no_gui,
        serial_console: !cli.no_serial,
        root_password: cli.root_password,
        user,
        dist_dir: cli.dist,
        target: cli.target,
        ..InstallPlan::default()
    };

    if !cli.dry_run && !dist_available(&plan.dist_dir) {
        bail!(
            "{} does not contain the dom0 distribution; run this from the JuanitOS live medium \
             or pass --dist",
            plan.dist_dir
        );
    }

    let disks = list_disks(runner).context("listing disks")?;
    if plan.disk.is_empty() {
        if !interactive {
            bail!("--disk is required with --non-interactive");
        }
        plan.disk = choose_disk(&disks)?;
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
        plan.hostname = prompt_default("Hostname", &plan.hostname)?;
        plan.swap_gb = prompt_default("Swap size in GiB (0 = none)", &plan.swap_gb.to_string())?
            .parse()
            .context("swap size must be a number")?;
        plan.gui = ask_yes_no("Start the COSMIC desktop at boot?", plan.gui)?;
        if plan.root_password.is_none() {
            plan.root_password = Some(prompt_password(runner, "Root password")?);
        }
        match &mut plan.user {
            Some((name, pw)) if pw.is_empty() => {
                *pw = prompt_password(runner, &format!("Password for {name}"))?;
            }
            Some(_) => {}
            None => {
                let name = prompt_default("Create a user (empty to skip)", "")?;
                if !name.is_empty() {
                    let pw = prompt_password(runner, &format!("Password for {name}"))?;
                    plan.user = Some((name, pw));
                }
            }
        }
    }

    plan.validate()?;
    println!("\nInstallation plan:\n{}", plan.summary());
    if interactive && !ask_yes_no("Proceed? This erases the disk.", false)? {
        bail!("aborted");
    }

    // 3. Install.
    Installer::new(runner, cli.dry_run, &plan).run(&report)?;

    if live && interactive && ask_yes_no("Reboot now?", true)? {
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

fn choose_disk(disks: &[Disk]) -> anyhow::Result<String> {
    if disks.is_empty() {
        bail!("no disks found");
    }
    println!("Disks:");
    for (i, d) in disks.iter().enumerate() {
        println!(
            "  {}) {:<8} {:>8.1} GiB  {}",
            i + 1,
            d.name,
            d.size_gib(),
            d.description
        );
    }
    loop {
        let answer = prompt_default("Install to disk (number or name)", "")?;
        if let Some(d) = answer
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| disks.get(i))
        {
            return Ok(d.name.clone());
        }
        if disks.iter().any(|d| d.name == answer) {
            return Ok(answer);
        }
        println!("  no such disk");
    }
}

fn read_line() -> anyhow::Result<String> {
    let mut line = String::new();
    io::stdout().flush()?;
    let n = io::stdin().lock().read_line(&mut line)?;
    if n == 0 {
        bail!("end of input");
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn prompt_default(question: &str, default: &str) -> anyhow::Result<String> {
    if default.is_empty() {
        print!("{question}: ");
    } else {
        print!("{question} [{default}]: ");
    }
    let answer = read_line()?;
    Ok(if answer.is_empty() {
        default.to_string()
    } else {
        answer
    })
}

fn ask_yes_no(question: &str, default: bool) -> anyhow::Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    loop {
        print!("{question} [{hint}] ");
        match read_line()?.to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {}
        }
    }
}

/// Read a password with echo off (via `stty`, so no libc dependency).
fn prompt_password(runner: &dyn Runner, label: &str) -> anyhow::Result<String> {
    loop {
        print!("{label}: ");
        let _ = runner.run("stty", &["-echo"]);
        let first = read_line();
        println!();
        print!("Repeat: ");
        let second = read_line();
        let _ = runner.run("stty", &["echo"]);
        println!();
        let (a, b) = (first?, second?);
        if a.is_empty() {
            println!("  password must not be empty");
        } else if a != b {
            println!("  passwords do not match");
        } else {
            return Ok(a);
        }
    }
}
