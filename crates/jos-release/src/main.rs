//! `jos-release` — build the JuanitOS dom0 tarball and live/install ISO.
//!
//! Stage 1 runs on a FreeBSD host (a VM is fine, see `builder`); stage 2
//! runs the same command on an installed JuanitOS. See `docs/bootstrap.md`.

mod builder;
mod console;
mod disk;
mod profile;
mod qemu;
mod ventoy;

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};

use juanitos_core::fsops::FsOps;
use juanitos_core::sys::{DryRunner, RealRunner, Runner};

use crate::builder::Builder;
use crate::profile::Profile;

#[derive(Parser, Debug)]
#[command(name = "jos-release", version, about = "Build JuanitOS release media")]
struct Cli {
    /// Print the host commands instead of running them.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Release profile (defaults to <repo>/release/juanitos.toml).
    #[arg(long, global = true)]
    profile: Option<PathBuf>,

    /// Repository root (defaults to the directory containing release/).
    #[arg(long, global = true)]
    repo: Option<PathBuf>,

    /// Scratch/output directory (defaults to <repo>/release/out).
    #[arg(long, global = true)]
    work: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Download and verify the FreeBSD distribution sets.
    Fetch,
    /// Build the Rust binaries (cargo build --release --workspace).
    Bins,
    /// Extract FreeBSD, install packages and JuanitOS into work/rootfs.
    Rootfs(BuildArgs),
    /// Pack work/rootfs into juanitos-dom0.txz + MANIFEST.
    Pack,
    /// Assemble the live/install ISO from work/rootfs and the tarball.
    Iso(BuildArgs),
    /// fetch + bins + rootfs + pack + iso.
    All(BuildArgs),
    /// Remove the work directory.
    Clean,
    /// Boot the ISO (or the disk it installed to) under QEMU with OVMF.
    Qemu(QemuArgs),
    /// Run a stock FreeBSD VM image under QEMU with this repo shared in,
    /// to build the ISO from a Linux host.
    Builder(BuilderArgs),
}

#[derive(clap::Args, Debug, Clone)]
struct BuildArgs {
    /// Directory with the release binaries (default <repo>/target/release).
    #[arg(long)]
    bins: Option<PathBuf>,
    /// Ship this jos-firewall unikernel image (default: the one `bins`
    /// built in <work>/jos-firewall.img, if any).
    #[arg(long)]
    firewall_image: Option<PathBuf>,
    /// Ship this jos-boot.efi graphical pre-loader (default: the one
    /// `bins` built in <repo>/target/uefi, if any).
    #[arg(long)]
    boot_efi: Option<PathBuf>,
}

#[derive(clap::Args, Debug, Clone)]
struct QemuArgs {
    /// ISO to boot (default: the one this profile produces). Omit with
    /// --no-cdrom to boot the installed disk.
    #[arg(long)]
    iso: Option<PathBuf>,
    #[arg(long)]
    no_cdrom: bool,
    /// qcow2 disk the installer targets (created when missing).
    #[arg(long)]
    disk: Option<PathBuf>,
    #[arg(long, default_value_t = 40)]
    disk_size_gb: u64,
    #[arg(long, default_value_t = 4096)]
    memory_mb: u64,
    #[arg(long, default_value_t = 2)]
    cpus: u32,
    /// OVMF code file (auto-detected).
    #[arg(long)]
    firmware: Option<PathBuf>,
    /// No window; serial console on stdio only.
    #[arg(long)]
    headless: bool,
    /// Do not pass the host CPU through (disables bhyve inside the VM).
    #[arg(long)]
    no_nested: bool,
    /// Emulate an IOMMU (VT-d or AMD-Vi, matching the host CPU) and a
    /// second NIC at PCI 0/9/0, so `jos create sys-net --pci 0/9/0` can be
    /// exercised inside the VM.
    #[arg(long)]
    iommu: bool,
}

#[derive(clap::Args, Debug, Clone)]
struct BuilderArgs {
    /// FreeBSD VM image (qcow2), e.g. FreeBSD-15.1-RELEASE-amd64-zfs.qcow2
    /// from download.freebsd.org/releases/VM-IMAGES/ (tools/fetch-freebsd.sh vm).
    #[arg(long)]
    image: PathBuf,
    /// Unattended: drive the guest over its serial console through the
    /// whole stage-1 build and copy the ISO back out (see --out).
    #[arg(long)]
    auto: bool,
    /// Results directory for --auto (default <work>/builder): working
    /// disk copy, console.log, the ISO, the dom0 tarball.
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long, default_value_t = 60)]
    disk_size_gb: u64,
    #[arg(long, default_value_t = 6144)]
    memory_mb: u64,
    #[arg(long, default_value_t = 4)]
    cpus: u32,
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("jos-release: {e:#}");
        std::process::exit(1);
    }
}

fn find_repo() -> anyhow::Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("release/juanitos.toml").is_file() && dir.join("Cargo.toml").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("not inside the JuanitOS repository (no release/juanitos.toml); pass --repo");
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let repo = match cli.repo {
        Some(r) => r,
        None => find_repo()?,
    };
    let profile_path = cli
        .profile
        .unwrap_or_else(|| repo.join("release/juanitos.toml"));
    let profile = Profile::load(&profile_path)?;
    let work = cli.work.unwrap_or_else(|| repo.join("release/out"));

    let dry = DryRunner::new();
    let real = RealRunner;
    let runner: &dyn Runner = if cli.dry_run { &dry } else { &real };

    let make_builder = |args: &BuildArgs| Builder {
        runner,
        fs: FsOps::new(cli.dry_run),
        profile: &profile,
        repo: repo.clone(),
        work: work.clone(),
        bins: args
            .bins
            .clone()
            .unwrap_or_else(|| repo.join("target/release")),
        firewall_image: args.firewall_image.clone(),
        boot_efi: args.boot_efi.clone(),
        installed_root: PathBuf::from("/"),
    };
    let plain = BuildArgs {
        bins: None,
        firewall_image: None,
        boot_efi: None,
    };

    match cli.cmd {
        Cmd::Fetch => make_builder(&plain).fetch(),
        Cmd::Bins => make_builder(&plain).bins(),
        Cmd::Rootfs(a) => {
            require_root(cli.dry_run)?;
            make_builder(&a).rootfs_build()
        }
        Cmd::Pack => {
            require_root(cli.dry_run)?;
            make_builder(&plain).pack()
        }
        Cmd::Iso(a) => {
            require_root(cli.dry_run)?;
            make_builder(&a).iso_build()
        }
        Cmd::All(a) => {
            require_root(cli.dry_run)?;
            make_builder(&a).all()
        }
        Cmd::Clean => {
            FsOps::new(cli.dry_run).remove_tree(&work)?;
            Ok(())
        }
        Cmd::Qemu(a) => {
            let firmware = match a.firmware.or_else(qemu::find_ovmf) {
                Some(f) => f,
                None => bail!(
                    "OVMF firmware not found; install the ovmf/edk2 package or pass --firmware"
                ),
            };
            let iso = if a.no_cdrom {
                None
            } else {
                Some(a.iso.unwrap_or_else(|| work.join(profile.iso_name())))
            };
            if let Some(iso) = &iso {
                if !iso.is_file() && !cli.dry_run {
                    bail!("{} does not exist; build it first", iso.display());
                }
            }
            let opts = qemu::QemuOptions {
                iso,
                disk: a.disk.unwrap_or_else(|| work.join("qemu-disk.qcow2")),
                disk_size_gb: a.disk_size_gb,
                memory_mb: a.memory_mb,
                cpus: a.cpus,
                firmware,
                vars: work.join("qemu-vars.fd"),
                headless: a.headless,
                nested: !a.no_nested,
                iommu: a.iommu.then(qemu::IommuVendor::host),
            };
            if let Some(v) = opts.iommu {
                println!(
                    "# {v:?} vIOMMU; inside the dom0: pptdevs=\"{nic}\" (and hw.vmm.amdvi.enable=1 on AMD) \
                     in /boot/loader.conf, then `jos create sys-net ... --pci {nic}`",
                    nic = qemu::PASSTHROUGH_NIC_ADDR
                );
            }
            qemu::run_qemu(&opts, cli.dry_run)
        }
        Cmd::Builder(a) if a.auto => qemu::run_auto_builder(
            &qemu::AutoBuilder {
                image: a.image,
                repo,
                out: a.out.unwrap_or_else(|| work.join("builder")),
                disk_size_gb: a.disk_size_gb,
                memory_mb: a.memory_mb,
                cpus: a.cpus,
            },
            cli.dry_run,
        ),
        Cmd::Builder(a) => qemu::run_builder(&a.image, &repo, cli.dry_run),
    }
}

/// The FreeBSD release steps need root (devfs mounts, pkg -c, ownership
/// inside the tarball). Checked via `id -u` to stay libc-free.
fn require_root(dry_run: bool) -> anyhow::Result<()> {
    if dry_run {
        return Ok(());
    }
    let out = std::process::Command::new("id")
        .arg("-u")
        .output()
        .context("running id -u")?;
    if String::from_utf8_lossy(&out.stdout).trim() != "0" {
        bail!("this step must run as root (or use --dry-run)");
    }
    Ok(())
}
