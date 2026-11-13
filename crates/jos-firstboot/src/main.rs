// SPDX-License-Identifier: BSD-3-Clause
//! `jos-firstboot` — the JuanitOS first-boot setup.
//!
//! Started once by `rc.d/jos-firstboot`, before the login screen, on the
//! first boot of an installed system. Like QubesOS's initial setup it
//! asks which network card `sys-net` gets, which app qubes to create and
//! from which template, then runs `jos setup` and shows its progress. The
//! dom0 itself never gets an address: the network only reaches the qubes
//! through `sys-net` and `sys-firewall`.
//!
//! A marker file records that the wizard ran (or was skipped), so the next
//! boots go straight to the login screen. Pure parts (wizard, renderer)
//! are tested on any host; `screenshots` renders the pages to PNGs.

// Only the FreeBSD build has a framebuffer to drive; the loop is still
// compiled and tested everywhere.
#[cfg_attr(not(target_os = "freebsd"), allow(dead_code))]
mod gui;
mod ui;
mod wizard;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use juanitos_core::dom0;
use juanitos_core::sys::Runner;

use wizard::Outcome;

#[derive(Parser, Debug)]
#[command(name = "jos-firstboot", version, about = "JuanitOS first-boot setup")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the wizard on the framebuffer (FreeBSD, as root).
    Run(RunArgs),
    /// Render the wizard's pages into PNGs.
    Screenshots {
        dir: PathBuf,
        #[arg(long, default_value_t = 1280)]
        width: u32,
        #[arg(long, default_value_t = 800)]
        height: u32,
    },
}

#[derive(clap::Args, Debug)]
struct RunArgs {
    /// Virtual terminal (1-based; the login screen's, which it takes over
    /// once the wizard is done).
    #[arg(long, default_value_t = dom0::LOGIN_VT)]
    vt: i32,
    /// Run even though the marker says the wizard already ran.
    #[arg(long)]
    force: bool,
    /// Pass `--dry-run` to `jos setup` and leave the marker alone.
    #[arg(long)]
    dry_run: bool,
    /// Written once the wizard is done or skipped.
    #[arg(long, default_value = dom0::FIRSTBOOT_MARKER)]
    marker: PathBuf,
    /// The `jos` command that creates the qubes.
    #[arg(long, default_value = "/usr/local/bin/jos")]
    jos: String,
}

fn main() {
    let runner = juanitos_core::sys::RealRunner;
    if let Err(e) = run(Cli::parse(), &runner) {
        eprintln!("jos-firstboot: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli, runner: &dyn Runner) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::Screenshots { dir, width, height } => {
            for f in ui::screenshots(&dir, width, height)? {
                println!("{f}");
            }
            Ok(())
        }
        Cmd::Run(a) => {
            if a.marker.exists() && !a.force {
                println!(
                    "jos-firstboot: already done ({} exists)",
                    a.marker.display()
                );
                return Ok(());
            }
            run_screen(&a, runner)
        }
    }
}

#[cfg_attr(not(target_os = "freebsd"), allow(dead_code))]
fn setup_cmd(a: &RunArgs) -> gui::SetupCmd {
    gui::SetupCmd {
        program: a.jos.clone(),
        prefix: if a.dry_run {
            vec!["--dry-run".to_string()]
        } else {
            Vec::new()
        },
    }
}

/// Record that the wizard is done (unless the user wants it again next
/// boot, or nothing was really done), then reboot if `jos setup` reserved
/// devices for passthrough.
#[cfg_attr(not(target_os = "freebsd"), allow(dead_code))]
fn finish(
    outcome: Outcome,
    marker: &Path,
    runner: &dyn Runner,
    dry_run: bool,
) -> anyhow::Result<()> {
    println!("jos-firstboot: {outcome:?}");
    if outcome == Outcome::Later || dry_run {
        return Ok(());
    }
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(marker, format!("{outcome:?}\n"))?;
    if outcome == Outcome::Reboot {
        runner.run_checked("shutdown", &["-r", "now"])?;
    }
    Ok(())
}

#[cfg(target_os = "freebsd")]
fn run_screen(a: &RunArgs, runner: &dyn Runner) -> anyhow::Result<()> {
    use jos_fb::Display;
    use juanitos_core::{config, doctor, ifaces, pci, template};

    let devices = pci::list_devices(runner).unwrap_or_default();
    ifaces::load_usb_drivers(runner);
    let nics = ifaces::list(runner, &devices);
    let cfg = config::load_global_config(None)?;
    let local = template::local_images(&cfg);
    let mut w = wizard::Wizard::new(nics, doctor::iommu_enabled(runner), &local);
    let mut display = jos_fb::vt::VtDisplay::open(a.vt)?;
    if let Err(e) = display.enable_mouse() {
        eprintln!("jos-firstboot: no mouse ({e:#}); the keyboard works");
    }
    let outcome = gui::run(
        &mut display,
        &mut w,
        runner,
        &devices,
        &setup_cmd(a),
        gui::LoopOptions::default(),
    );
    display.restore_console()?;
    finish(outcome?, &a.marker, runner, a.dry_run)
}

#[cfg(not(target_os = "freebsd"))]
fn run_screen(_: &RunArgs, _: &dyn Runner) -> anyhow::Result<()> {
    anyhow::bail!(
        "the first-boot wizard needs FreeBSD's vt(4); try `jos-firstboot screenshots DIR`"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use juanitos_core::sys::MockRunner;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("jos-firstboot-{name}-{}", std::process::id()))
    }

    #[test]
    fn subcommands() {
        let dir = tmp("main");
        let m = MockRunner::new();
        run(
            Cli::parse_from([
                "jos-firstboot",
                "screenshots",
                dir.to_str().unwrap(),
                "--width",
                "320",
                "--height",
                "200",
            ]),
            &m,
        )
        .unwrap();
        let png = dir.join("firstboot-1-welcome.png");
        assert!(png.is_file());
        assert!(run(
            Cli::parse_from(["jos-firstboot", "screenshots", png.to_str().unwrap()]),
            &m,
        )
        .is_err());

        // The marker exists: nothing to do, even without a framebuffer.
        run(
            Cli::parse_from(["jos-firstboot", "run", "--marker", png.to_str().unwrap()]),
            &m,
        )
        .unwrap();
        #[cfg(not(target_os = "freebsd"))]
        {
            let err = run(
                Cli::parse_from([
                    "jos-firstboot",
                    "run",
                    "--force",
                    "--marker",
                    png.to_str().unwrap(),
                ]),
                &m,
            )
            .unwrap_err();
            assert!(err.to_string().contains("vt(4)"));
        }
        assert!(m.calls().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_passes_the_flag_to_jos() {
        let parse = |v: &[&str]| match Cli::parse_from(v).cmd {
            Cmd::Run(a) => Some(a),
            Cmd::Screenshots { .. } => None,
        };
        assert!(parse(&["jos-firstboot", "screenshots", "shots"]).is_none());
        let args = |extra: &[&str]| {
            let mut v = vec!["jos-firstboot", "run", "--jos", "/opt/jos"];
            v.extend(extra);
            parse(&v)
        };
        let a = args(&[]).unwrap();
        assert_eq!(a.vt, dom0::LOGIN_VT);
        assert_eq!(a.marker, Path::new(dom0::FIRSTBOOT_MARKER));
        let s = setup_cmd(&a);
        assert_eq!(s.program, "/opt/jos");
        assert!(s.prefix.is_empty());
        assert_eq!(
            setup_cmd(&args(&["--dry-run"]).unwrap()).prefix,
            ["--dry-run"]
        );
    }

    #[test]
    fn finishing_writes_the_marker_and_reboots_when_asked() {
        let dir = tmp("finish");
        let marker = dir.join("db/firstboot.done");
        let m = MockRunner::new();
        // Later, or a dry run: asked again next boot.
        finish(Outcome::Later, &marker, &m, false).unwrap();
        finish(Outcome::Reboot, &marker, &m, true).unwrap();
        assert!(!marker.exists());
        finish(Outcome::Done, &marker, &m, false).unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "Done\n");
        assert!(m.calls().is_empty());
        finish(Outcome::Reboot, &marker, &m, false).unwrap();
        assert_eq!(m.calls(), ["shutdown -r now"]);

        let missing = MockRunner::new().missing("shutdown");
        assert!(finish(Outcome::Reboot, &marker, &missing, false).is_err());
        // The marker's directory is a file; the marker is a directory.
        assert!(finish(Outcome::Done, &marker.join("x"), &m, false).is_err());
        assert!(finish(Outcome::Done, &dir, &m, false).is_err());
        // A bare file name has the current directory as its parent.
        assert!(finish(Outcome::Done, Path::new(""), &m, false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
