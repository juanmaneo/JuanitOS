// SPDX-License-Identifier: BSD-3-Clause
//! `jos-release` — build the JuanitOS dom0 tarball and live/install ISO.
//!
//! Stage 1 runs on a FreeBSD host (a VM is fine, see `builder`); stage 2
//! runs the same command on an installed JuanitOS. See `docs/bootstrap.md`.

mod builder;
mod compare;
mod console;
mod disk;
mod oci;
mod profile;
mod qemu;
mod secureboot;
#[cfg(test)]
#[path = "../tests/common/gpt.rs"]
mod test_gpt;
mod vbox;
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

    /// OVMF code file for `qemu`, `builder --auto`, `selfhost`, `oci` and
    /// (a Secure Boot build) `secureboot-test` (auto-detected).
    #[arg(long, global = true)]
    firmware: Option<PathBuf>,

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
    /// Compare two dom0 tarballs by contents (bootstrap stage 3): exits
    /// non-zero on any difference not known to be harmless.
    Compare(CompareArgs),
    /// Unattended bootstrap round under QEMU: install the ISO, rebuild
    /// JuanitOS on the installed dom0, compare the two dom0 tarballs.
    Selfhost(SelfhostArgs),
    /// Unattended install test in a VirtualBox VM (EFI, VMSVGA, AHCI): the
    /// graphical boot, the installer, the login screen and a clean
    /// power-off, driven over the VM's serial port.
    Vbox(VboxArgs),
    /// Test the dom0 userland as an OCI image under podman on FreeBSD: in
    /// a FreeBSD VM under QEMU (the builder's disk, under an overlay), or
    /// on this FreeBSD host with --on-host.
    Oci(OciArgs),
    /// Secure Boot under QEMU: enroll fresh keys in a Secure Boot OVMF,
    /// then check that a signed jos-boot.efi starts and chain-loads a
    /// signed loader, and that unsigned images are refused.
    SecurebootTest(SecurebootArgs),
}

#[derive(clap::Args, Debug, Clone)]
struct SecurebootArgs {
    /// The pre-loader to sign and boot (default: the one `bins` built in
    /// <repo>/target/uefi).
    #[arg(long)]
    boot_efi: Option<PathBuf>,
    /// Keys, signed images, variable stores and console logs (default
    /// <work>/secureboot).
    #[arg(long)]
    out: Option<PathBuf>,
    /// The variable store template of --firmware (default: the VARS file
    /// next to it).
    #[arg(long)]
    vars: Option<PathBuf>,
}

#[derive(clap::Args, Debug, Clone)]
struct OciArgs {
    /// The dom0 tarball (default: <work>/builder/juanitos-dom0.txz, the
    /// builder's, or <work>/juanitos-dom0.txz with --on-host).
    #[arg(long)]
    tarball: Option<PathBuf>,
    /// FreeBSD VM disk with its serial console on (default
    /// <work>/builder/builder.raw); only read.
    #[arg(long, conflicts_with = "on_host")]
    image: Option<PathBuf>,
    /// Run podman on this host (FreeBSD, as root, podman installed).
    #[arg(long)]
    on_host: bool,
    /// Build context, console.log and the VM's overlay disk (default
    /// <work>/oci).
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long, default_value_t = 4096)]
    memory_mb: u64,
    #[arg(long, default_value_t = 4)]
    cpus: u32,
    /// Leave the VM's overlay disk, with the image in it, afterwards.
    #[arg(long, conflicts_with = "on_host")]
    keep: bool,
}

#[derive(clap::Args, Debug, Clone)]
struct VboxArgs {
    /// ISO to install (default: the one this profile produces).
    #[arg(long)]
    iso: Option<PathBuf>,
    /// VM folder, serial socket, console.log and screenshots (default
    /// <work>/vbox).
    #[arg(long)]
    out: Option<PathBuf>,
    /// VM name (replaced if it exists).
    #[arg(long, default_value = "jos-vbox")]
    name: String,
    #[arg(long, default_value_t = 20)]
    disk_size_gb: u64,
    #[arg(long, default_value_t = 6144)]
    memory_mb: u64,
    #[arg(long, default_value_t = 4)]
    cpus: u32,
    /// Video memory; 128 MB makes VMSVGA offer modes up to 7680x4320.
    #[arg(long, default_value_t = 128)]
    vram_mb: u32,
    /// Leave the VM registered afterwards.
    #[arg(long)]
    keep: bool,
}

#[derive(clap::Args, Debug, Clone)]
struct SelfhostArgs {
    /// Stage-N ISO (default: the one this profile produces).
    #[arg(long)]
    iso: Option<PathBuf>,
    /// Working disk, console.log and results (default <work>/selfhost).
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long, default_value_t = 40)]
    disk_size_gb: u64,
    #[arg(long, default_value_t = 6144)]
    memory_mb: u64,
    #[arg(long, default_value_t = 4)]
    cpus: u32,
}

#[derive(clap::Args, Debug, Clone)]
struct CompareArgs {
    a: PathBuf,
    b: PathBuf,
    /// Leave the extracted trees in <work>/compare.
    #[arg(long)]
    keep: bool,
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
    /// Link this jos_ventoy_chain.o into geom_ventoy.ko (default: the one
    /// `bins` built in <repo>/target/kernel, if any).
    #[arg(long)]
    chain_object: Option<PathBuf>,
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
    // A working directory that is gone is outside the repository too.
    let mut dir = std::env::current_dir().unwrap_or_default();
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
        chain_object: args.chain_object.clone(),
        installed_root: PathBuf::from("/"),
    };
    let plain = BuildArgs {
        bins: None,
        firewall_image: None,
        boot_efi: None,
        chain_object: None,
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
        Cmd::Clean => FsOps::new(cli.dry_run)
            .remove_tree(&work)
            .map_err(Into::into),
        Cmd::Qemu(a) => {
            let firmware = firmware(cli.firmware)?;
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
                firmware: firmware(cli.firmware)?,
                disk_size_gb: a.disk_size_gb,
                memory_mb: a.memory_mb,
                cpus: a.cpus,
            },
            cli.dry_run,
        ),
        Cmd::Builder(a) => qemu::run_builder(&a.image, &repo, cli.dry_run),
        Cmd::Selfhost(a) => qemu::run_selfhost(
            &qemu::SelfHost {
                iso: a.iso.unwrap_or_else(|| work.join(profile.iso_name())),
                out: a.out.unwrap_or_else(|| work.join("selfhost")),
                firmware: firmware(cli.firmware)?,
                disk_size_gb: a.disk_size_gb,
                memory_mb: a.memory_mb,
                cpus: a.cpus,
            },
            cli.dry_run,
        ),
        Cmd::Vbox(a) => vbox::run_vbox(
            &vbox::VBox {
                name: a.name,
                iso: a.iso.unwrap_or_else(|| work.join(profile.iso_name())),
                out: a.out.unwrap_or_else(|| work.join("vbox")),
                disk_size_gb: a.disk_size_gb,
                memory_mb: a.memory_mb,
                cpus: a.cpus,
                vram_mb: a.vram_mb,
                keep: a.keep,
            },
            runner,
            cli.dry_run,
        ),
        Cmd::Oci(a) => {
            let builder = work.join("builder");
            let vm = if a.on_host {
                None
            } else {
                Some(oci::Vm {
                    image: a.image.unwrap_or_else(|| builder.join("builder.raw")),
                    firmware: firmware(cli.firmware)?,
                    memory_mb: a.memory_mb,
                    cpus: a.cpus,
                    keep: a.keep,
                })
            };
            let dist = if a.on_host { &work } else { &builder };
            oci::run_oci(
                &oci::Oci {
                    tarball: a
                        .tarball
                        .unwrap_or_else(|| dist.join(juanitos_core::dom0::DIST_TARBALL)),
                    out: a.out.unwrap_or_else(|| work.join("oci")),
                    freebsd_version: profile.freebsd_version.clone(),
                    repo,
                    vm,
                },
                cli.dry_run,
            )
        }
        Cmd::SecurebootTest(a) => {
            let code = cli
                .firmware
                .or_else(secureboot::find_code)
                .filter(|f| f.is_file())
                .context(
                    "no Secure Boot build of OVMF (OVMF_CODE*.secboot*.fd) found; \
                     install the ovmf/edk2 package or pass --firmware",
                )?;
            let vars = a
                .vars
                .or_else(|| secureboot::vars_for(&code))
                .with_context(|| {
                    format!(
                        "no variable store template next to {}; pass --vars",
                        code.display()
                    )
                })?;
            let test = secureboot::Test {
                boot_efi: a.boot_efi.unwrap_or_else(|| {
                    make_builder(&plain).uefi_release_dir().join("jos-boot.efi")
                }),
                out: a.out.unwrap_or_else(|| work.join("secureboot")),
                code,
                vars,
            };
            secureboot::run(&test, runner, cli.dry_run)
        }
        Cmd::Compare(a) => {
            let report = compare::compare_tarballs(
                runner,
                &FsOps::new(cli.dry_run),
                &a.a,
                &a.b,
                &work.join("compare"),
                a.keep,
            )?;
            print!("{}", report.render());
            if report.unexpected().next().is_some() {
                bail!("{} and {} differ", a.a.display(), a.b.display());
            }
            Ok(())
        }
    }
}

/// The OVMF code file: `given`, or the first known location that exists.
fn firmware(given: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    given
        .or_else(qemu::find_ovmf)
        .filter(|f| f.is_file())
        .context("OVMF firmware not found; install the ovmf/edk2 package or pass --firmware")
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
