//! The installation itself: GPT + ESP + ZFS pool, extract the dom0 tarball
//! the live medium booted from, configure, done.
//!
//! The layout follows what FreeBSD's `bsdinstall zfsboot` produces so that
//! the result is a boring, well-understood FreeBSD-on-ZFS system.

use std::path::{Path, PathBuf};

use juanitos_core::dom0::{self, Dom0Options, Flavor};
use juanitos_core::fsops::FsOps;
use juanitos_core::pci::{self, PciAddr};
use juanitos_core::release::DistManifest;
use juanitos_core::sys::Runner;
use juanitos_core::zfs::Storage;
use juanitos_core::{Error, GlobalConfig, Result};

/// Everything the installer needs to know; filled in by the prompts or by
/// command-line flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// Target disk without `/dev/`.
    pub disk: String,
    pub pool: String,
    pub hostname: String,
    pub swap_gb: u64,
    pub esp_mb: u64,
    pub gui: bool,
    pub serial_console: bool,
    /// KMS driver for the dom0's GPU (`dom0::gpu_kernel_module`), detected
    /// on the live medium.
    pub gpu_module: Option<String>,
    /// PCI devices reserved for qubes (`pptdevs`), sorted.
    pub pptdevs: Vec<PciAddr>,
    pub root_password: Option<String>,
    /// `(name, password)` of the first unprivileged (wheel) user.
    pub user: Option<(String, String)>,
    /// Directory holding `juanitos-dom0.txz` and `MANIFEST`.
    pub dist_dir: String,
    /// Where the new pool is mounted while installing.
    pub target: String,
}

impl Default for InstallPlan {
    fn default() -> Self {
        InstallPlan {
            disk: String::new(),
            pool: "zroot".into(),
            hostname: "juanitos".into(),
            swap_gb: 4,
            esp_mb: 260,
            gui: true,
            serial_console: true,
            gpu_module: None,
            pptdevs: Vec::new(),
            root_password: None,
            user: None,
            dist_dir: dom0::DIST_DIR.to_string(),
            target: "/mnt".to_string(),
        }
    }
}

impl InstallPlan {
    pub fn validate(&self) -> Result<()> {
        if self.disk.is_empty() || self.disk.contains('/') {
            return Err(Error::Other(format!(
                "invalid disk name {:?} (expected e.g. ada0)",
                self.disk
            )));
        }
        juanitos_core::config::validate_name(&self.pool)
            .map_err(|_| Error::Other(format!("invalid pool name {:?}", self.pool)))?;
        if self.hostname.is_empty() {
            return Err(Error::Other("hostname must not be empty".into()));
        }
        if let Some((name, _)) = &self.user {
            if name.is_empty() || name == "root" {
                return Err(Error::Other(format!("invalid user name {name:?}")));
            }
        }
        Ok(())
    }

    pub fn dev(&self) -> String {
        format!("/dev/{}", self.disk)
    }

    pub fn global_config(&self) -> GlobalConfig {
        GlobalConfig {
            pool: self.pool.clone(),
            ..GlobalConfig::default()
        }
    }

    pub fn dom0_options(&self) -> Dom0Options {
        Dom0Options {
            hostname: self.hostname.clone(),
            gui: self.gui,
            serial_console: self.serial_console,
            gpu_module: self.gpu_module.clone(),
            pptdevs: self.pptdevs.clone(),
        }
    }

    /// The reserved devices as shown to the user: `1/0/0 1/0/1` or `none`.
    pub fn passthrough_text(&self) -> String {
        if self.pptdevs.is_empty() {
            "none".into()
        } else {
            pci::addr_list(&self.pptdevs)
        }
    }

    /// Human-readable summary shown before the point of no return.
    pub fn summary(&self) -> String {
        let user = self
            .user
            .as_ref()
            .map(|(n, _)| n.as_str())
            .unwrap_or("(none)");
        format!(
            "  disk      {dev}  (ALL DATA WILL BE ERASED)\n  \
             pool      {pool}\n  \
             hostname  {host}\n  \
             swap      {swap} GiB\n  \
             desktop   {gui}\n  \
             passthru  {ppt}\n  \
             user      {user}\n  \
             dist      {dist}\n",
            dev = self.dev(),
            pool = self.pool,
            host = self.hostname,
            swap = self.swap_gb,
            gui = if self.gui { "COSMIC" } else { "console only" },
            ppt = self.passthrough_text(),
            dist = self.dist_dir,
        )
    }
}

/// Datasets created under the pool, after FreeBSD's `zfsboot` layout.
/// `(name, properties)`; `ROOT/default` is handled separately.
pub const DATASETS: &[(&str, &[&str])] = &[
    ("home", &["-o", "mountpoint=/home"]),
    (
        "tmp",
        &["-o", "mountpoint=/tmp", "-o", "exec=on", "-o", "setuid=off"],
    ),
    ("usr", &["-o", "mountpoint=/usr", "-o", "canmount=off"]),
    ("usr/ports", &["-o", "setuid=off"]),
    ("usr/src", &[]),
    ("var", &["-o", "mountpoint=/var", "-o", "canmount=off"]),
    ("var/audit", &["-o", "exec=off", "-o", "setuid=off"]),
    ("var/crash", &["-o", "exec=off", "-o", "setuid=off"]),
    ("var/log", &["-o", "exec=off", "-o", "setuid=off"]),
    ("var/mail", &["-o", "atime=on"]),
    ("var/tmp", &["-o", "setuid=off"]),
];

/// What [`Installer::run_with`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Step `index` (0-based) of `total` is starting.
    Step {
        index: usize,
        total: usize,
        title: String,
    },
    Done,
}

/// Installation steps, as shown to the user.
/// The `freebsd-boot` partition holding `gptzfsboot` (bsdinstall's size)
/// and its GPT index, right after the ESP.
const BIOS_BOOT_SIZE: &str = "512k";
const BIOS_BOOT_INDEX: &str = "2";

pub const STEP_TITLES: [&str; 8] = [
    "Verifying the dom0 distribution",
    "Partitioning the disk",
    "Creating the ZFS pool",
    "Extracting the dom0",
    "Writing the configuration",
    "Installing the boot loader",
    "Creating the accounts",
    "Finishing",
];

pub struct Installer<'a> {
    pub runner: &'a dyn Runner,
    pub fs: FsOps,
    pub plan: &'a InstallPlan,
}

impl<'a> Installer<'a> {
    pub fn new(runner: &'a dyn Runner, dry_run: bool, plan: &'a InstallPlan) -> Self {
        Installer {
            runner,
            fs: FsOps::new(dry_run),
            plan,
        }
    }

    /// `rel` inside the target root, as a FreeBSD path string.
    fn target(&self, rel: &str) -> String {
        format!("{}/{}", self.plan.target, rel.trim_start_matches('/'))
    }

    fn target_path(&self, rel: &str) -> PathBuf {
        PathBuf::from(self.target(rel))
    }

    fn step(&self, msg: &str) {
        println!("==> {msg}");
    }

    /// Run the whole installation.
    pub fn run(&self, doctor_report: &str) -> Result<()> {
        self.run_with(doctor_report, &mut |_| {})
    }

    /// Run the whole installation, reporting each step to `progress`
    /// before it starts (the graphical installer draws a progress bar
    /// from these).
    pub fn run_with(&self, doctor_report: &str, progress: &mut dyn FnMut(Progress)) -> Result<()> {
        self.plan.validate()?;
        let steps: [(&str, &dyn Fn() -> Result<()>); 8] = [
            (STEP_TITLES[0], &|| self.verify_dist()),
            (STEP_TITLES[1], &|| self.partition()),
            (STEP_TITLES[2], &|| self.create_pool()),
            (STEP_TITLES[3], &|| self.extract()),
            (STEP_TITLES[4], &|| self.configure(doctor_report)),
            (STEP_TITLES[5], &|| self.install_bootloader()),
            (STEP_TITLES[6], &|| self.accounts()),
            (STEP_TITLES[7], &|| self.finish()),
        ];
        let total = steps.len();
        for (index, (title, step)) in steps.iter().enumerate() {
            progress(Progress::Step {
                index,
                total,
                title: (*title).to_string(),
            });
            step()?;
        }
        progress(Progress::Done);
        Ok(())
    }

    fn tarball(&self) -> String {
        format!("{}/{}", self.plan.dist_dir, dom0::DIST_TARBALL)
    }

    pub fn verify_dist(&self) -> Result<()> {
        self.step("Verifying dom0 distribution");
        let dist_dir = Path::new(&self.plan.dist_dir);
        let manifest = dist_dir.join(dom0::DIST_MANIFEST);
        if !manifest.is_file() {
            if self.fs.dry_run {
                println!("   (no {} here; skipped in dry run)", manifest.display());
                return Ok(());
            }
            return Err(Error::Other(format!(
                "{} not found: is this the JuanitOS live medium?",
                manifest.display()
            )));
        }
        DistManifest::read(&manifest)?.verify(dist_dir, dom0::DIST_TARBALL)?;
        println!("   {} OK", self.tarball());
        Ok(())
    }

    pub fn partition(&self) -> Result<()> {
        let p = self.plan;
        let dev = p.dev();
        self.step(&format!("Partitioning {dev} (GPT, UEFI + BIOS)"));
        // Fails when there is no partition table yet; that is fine.
        let _ = self.runner.run("gpart", &["destroy", "-F", &p.disk]);
        self.runner
            .run_checked("gpart", &["create", "-s", "gpt", &p.disk])?;
        let esp = format!("{}M", p.esp_mb);
        self.runner.run_checked(
            "gpart",
            &[
                "add", "-a", "4k", "-s", &esp, "-t", "efi", "-l", "efiboot0", &p.disk,
            ],
        )?;
        // Index 2, after the ESP so the ESP stays p1: legacy BIOS boot
        // (firmware without UEFI, or 32-bit EFI Macs booting via CSM).
        self.runner.run_checked(
            "gpart",
            &[
                "add",
                "-a",
                "4k",
                "-s",
                BIOS_BOOT_SIZE,
                "-t",
                "freebsd-boot",
                "-l",
                "gptboot0",
                &p.disk,
            ],
        )?;
        if p.swap_gb > 0 {
            let swap = format!("{}G", p.swap_gb);
            self.runner.run_checked(
                "gpart",
                &[
                    "add",
                    "-a",
                    "1m",
                    "-s",
                    &swap,
                    "-t",
                    "freebsd-swap",
                    "-l",
                    "swap0",
                    &p.disk,
                ],
            )?;
        }
        self.runner.run_checked(
            "gpart",
            &[
                "add",
                "-a",
                "1m",
                "-t",
                "freebsd-zfs",
                "-l",
                "zfs0",
                &p.disk,
            ],
        )?;
        self.runner
            .run_checked("newfs_msdos", &["-F", "32", "-c", "1", "/dev/gpt/efiboot0"])?;
        Ok(())
    }

    pub fn create_pool(&self) -> Result<()> {
        let p = self.plan;
        self.step(&format!("Creating ZFS pool {}", p.pool));
        let altroot = format!("altroot={}", p.target);
        self.runner.run_checked(
            "zpool",
            &[
                "create",
                "-f",
                "-o",
                &altroot,
                "-o",
                "cachefile=/tmp/zpool.cache",
                "-O",
                "compression=lz4",
                "-O",
                "atime=off",
                "-O",
                "mountpoint=none",
                "-m",
                "none",
                &p.pool,
                "/dev/gpt/zfs0",
            ],
        )?;
        let root = format!("{}/ROOT", p.pool);
        let be = format!("{root}/default");
        self.runner
            .run_checked("zfs", &["create", "-o", "mountpoint=none", &root])?;
        self.runner.run_checked(
            "zfs",
            &["create", "-o", "mountpoint=/", "-o", "canmount=noauto", &be],
        )?;
        self.runner.run_checked("zfs", &["mount", &be])?;
        for (name, props) in DATASETS {
            let ds = format!("{}/{name}", p.pool);
            let mut args = vec!["create"];
            args.extend_from_slice(props);
            args.push(&ds);
            self.runner.run_checked("zfs", &args)?;
        }
        let bootfs = format!("bootfs={be}");
        self.runner
            .run_checked("zpool", &["set", &bootfs, &p.pool])?;
        self.runner.run_checked(
            "chmod",
            &["1777", &self.target("tmp"), &self.target("var/tmp")],
        )?;

        // JuanitOS datasets (templates/qubes) — same code path as `jos init`.
        let cfg = p.global_config();
        Storage::new(self.runner, &cfg).init()?;
        Ok(())
    }

    pub fn extract(&self) -> Result<()> {
        self.step("Extracting dom0 (this is the same tree the live medium runs)");
        let tarball = self.tarball();
        self.runner.run_checked(
            "tar",
            &["-xpf", &tarball, "-C", &self.plan.target, "--unlink"],
        )?;
        Ok(())
    }

    pub fn configure(&self, doctor_report: &str) -> Result<()> {
        let p = self.plan;
        self.step("Writing configuration");
        let opts = p.dom0_options();
        self.fs.write(
            &self.target_path("etc/rc.conf"),
            &dom0::rc_conf(Flavor::Installed, &opts),
        )?;
        self.fs.write(
            &self.target_path("boot/loader.conf"),
            &dom0::loader_conf(Flavor::Installed, &opts),
        )?;
        self.fs.write(&self.target_path("etc/fstab"), &fstab(p))?;
        self.fs.mkdir_p(&self.target_path("boot/efi"))?;
        self.fs.mkdir_p(&self.target_path("boot/zfs"))?;
        self.runner.run_checked(
            "cp",
            &["/tmp/zpool.cache", &self.target("boot/zfs/zpool.cache")],
        )?;

        let cfg = p.global_config();
        let jos_toml = toml_pretty(&cfg)?;
        self.fs.write(
            &self.target_path("usr/local/etc/juanitos/jos.toml"),
            &jos_toml,
        )?;
        self.fs
            .mkdir_p(&self.target_path("usr/local/etc/juanitos/qubes"))?;
        self.fs.write(
            &self.target_path("usr/local/etc/juanitos/install-report.txt"),
            doctor_report,
        )?;
        Ok(())
    }

    pub fn install_bootloader(&self) -> Result<()> {
        self.step("Installing boot loaders (UEFI and BIOS)");
        self.runner.run_checked(
            "gpart",
            &[
                "bootcode",
                "-b",
                dom0::PMBR,
                "-p",
                dom0::GPTZFSBOOT,
                "-i",
                BIOS_BOOT_INDEX,
                &self.plan.disk,
            ],
        )?;
        let esp_s = self.target("boot/efi");
        self.runner
            .run_checked("mount", &["-t", "msdosfs", "/dev/gpt/efiboot0", &esp_s])?;
        // Same layout as the live medium's ESP: the graphical pre-loader
        // (when the release ships it) as the default boot file, FreeBSD's
        // loader in EFI/FREEBSD for it to chain-load.
        let has_jos_boot = self
            .target_path(dom0::JOS_BOOT_EFI.trim_start_matches('/'))
            .is_file();
        let has_ia32 = self
            .target_path(dom0::LOADER_IA32_EFI.trim_start_matches('/'))
            .is_file();
        let result = (|| {
            for (src, dst) in dom0::esp_files(has_jos_boot, has_ia32) {
                self.fs.copy(
                    &self.target_path(src.trim_start_matches('/')),
                    &self.target_path(&format!("boot/efi/{dst}")),
                )?;
            }
            Ok::<(), Error>(())
        })();
        let _ = self.runner.run("umount", &[&esp_s]);
        result?;
        // Register a boot entry; harmless if the firmware/efivars are not
        // available (e.g. under some hypervisors).
        let part = format!("{}p1:/{}", self.plan.disk, dom0::ESP_DEFAULT);
        let _ = self
            .runner
            .run("efibootmgr", &["-c", "-a", "-l", &part, "-L", "JuanitOS"]);
        Ok(())
    }

    pub fn accounts(&self) -> Result<()> {
        let p = self.plan;
        let target = p.target.clone();
        if let Some(pw) = &p.root_password {
            self.step("Setting root password");
            self.runner.run_with_input_checked(
                "chroot",
                &[&target, "pw", "usermod", "root", "-h", "0"],
                &format!("{pw}\n"),
            )?;
        }
        if let Some((name, pw)) = &p.user {
            self.step(&format!("Creating user {name}"));
            self.runner.run_with_input_checked(
                "chroot",
                &[
                    &target,
                    "pw",
                    "useradd",
                    name,
                    "-m",
                    "-G",
                    "wheel,video",
                    "-s",
                    "/bin/sh",
                    "-h",
                    "0",
                ],
                &format!("{pw}\n"),
            )?;
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<()> {
        self.step("Finishing");
        self.runner.run_checked("sync", &[])?;
        println!(
            "\nJuanitOS is installed on {}. Remove the install medium and reboot.",
            self.plan.dev()
        );
        Ok(())
    }
}

fn toml_pretty(cfg: &GlobalConfig) -> Result<String> {
    Ok(toml::to_string_pretty(cfg)?)
}

/// `/etc/fstab` for the installed system: swap and the ESP (FreeBSD 14
/// keeps it mounted at `/boot/efi`).
pub fn fstab(p: &InstallPlan) -> String {
    let mut s = String::from("# Device\t\tMountpoint\tFStype\tOptions\tDump\tPass#\n");
    if p.swap_gb > 0 {
        s.push_str("/dev/gpt/swap0\t\tnone\t\tswap\tsw\t0\t0\n");
    }
    s.push_str("/dev/gpt/efiboot0\t/boot/efi\tmsdosfs\trw\t2\t2\n");
    s
}

/// Does `dir` hold a dom0 tarball and its manifest?
pub fn dist_available(dir: &str) -> bool {
    let dir = Path::new(dir);
    dir.join(dom0::DIST_TARBALL).is_file() && dir.join(dom0::DIST_MANIFEST).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use juanitos_core::sys::{CmdOutput, MockRunner};

    fn plan() -> InstallPlan {
        InstallPlan {
            disk: "vtbd0".into(),
            hostname: "box".into(),
            root_password: Some("secret".into()),
            user: Some(("juan".into(), "pw".into())),
            dist_dir: "/nonexistent".into(),
            pptdevs: vec![PciAddr::new(1, 0, 0), PciAddr::new(1, 0, 1)],
            ..Default::default()
        }
    }

    #[test]
    fn validation() {
        assert!(plan().validate().is_ok());
        let mut p = plan();
        p.disk = "/dev/ada0".into();
        assert!(p.validate().is_err());
        let mut p = plan();
        p.user = Some(("root".into(), "x".into()));
        assert!(p.validate().is_err());
        let mut p = plan();
        p.pool = "Bad Pool".into();
        assert!(p.validate().is_err());
    }

    #[test]
    fn partition_and_pool_commands() {
        let p = plan();
        let m = MockRunner::new().respond("zfs", &["list"], CmdOutput::failed(1, ""));
        let inst = Installer::new(&m, true, &p);
        inst.partition().unwrap();
        inst.create_pool().unwrap();
        let calls = m.calls();
        assert_eq!(calls[0], "gpart destroy -F vtbd0");
        assert_eq!(calls[1], "gpart create -s gpt vtbd0");
        assert_eq!(calls[2], "gpart add -a 4k -s 260M -t efi -l efiboot0 vtbd0");
        assert_eq!(
            calls[3],
            "gpart add -a 4k -s 512k -t freebsd-boot -l gptboot0 vtbd0"
        );
        assert_eq!(
            calls[4],
            "gpart add -a 1m -s 4G -t freebsd-swap -l swap0 vtbd0"
        );
        assert_eq!(calls[5], "gpart add -a 1m -t freebsd-zfs -l zfs0 vtbd0");
        assert!(calls
            .iter()
            .any(|c| c.starts_with("zpool create -f -o altroot=/mnt")
                && c.ends_with("zroot /dev/gpt/zfs0")));
        assert!(calls.contains(&"zfs mount zroot/ROOT/default".to_string()));
        assert!(calls.contains(&"zpool set bootfs=zroot/ROOT/default zroot".to_string()));
        assert!(calls.contains(&"zfs create -p zroot/juanitos/qubes".to_string()));
        assert!(calls.contains(&"zfs create -p zroot/juanitos/templates".to_string()));
    }

    #[test]
    fn no_swap_skips_swap_partition_and_fstab_entry() {
        let mut p = plan();
        p.swap_gb = 0;
        let m = MockRunner::new();
        Installer::new(&m, true, &p).partition().unwrap();
        assert!(!m.calls().iter().any(|c| c.contains("freebsd-swap")));
        assert!(!fstab(&p).contains("swap"));
        assert!(fstab(&plan()).contains("/dev/gpt/swap0"));
        assert!(fstab(&plan()).contains("/boot/efi"));
    }

    #[test]
    fn accounts_go_through_chroot_and_stdin() {
        let p = plan();
        let m = MockRunner::new();
        Installer::new(&m, true, &p).accounts().unwrap();
        let calls = m.calls();
        assert_eq!(calls[0], "chroot /mnt pw usermod root -h 0");
        assert_eq!(
            calls[1],
            "chroot /mnt pw useradd juan -m -G wheel,video -s /bin/sh -h 0"
        );
        // The password itself never shows up in argv.
        assert!(calls.iter().all(|c| !c.contains("secret")));
    }

    #[test]
    fn extract_uses_tarball_from_dist_dir() {
        let p = plan();
        let m = MockRunner::new();
        Installer::new(&m, true, &p).extract().unwrap();
        assert_eq!(
            m.calls()[0],
            "tar -xpf /nonexistent/juanitos-dom0.txz -C /mnt --unlink"
        );
    }

    #[test]
    fn run_with_reports_every_step_in_order() {
        let p = plan();
        let m = MockRunner::new().respond("zfs", &["list"], CmdOutput::failed(1, ""));
        let mut seen = Vec::new();
        let dir = std::env::temp_dir().join(format!("jos-install-run-{}", std::process::id()));
        let p = InstallPlan {
            target: dir.to_string_lossy().into_owned(),
            ..p
        };
        Installer::new(&m, true, &p)
            .run_with("report", &mut |e| seen.push(e))
            .unwrap();
        assert_eq!(seen.len(), STEP_TITLES.len() + 1);
        for (i, e) in seen.iter().take(STEP_TITLES.len()).enumerate() {
            assert_eq!(
                *e,
                Progress::Step {
                    index: i,
                    total: STEP_TITLES.len(),
                    title: STEP_TITLES[i].to_string()
                }
            );
        }
        assert_eq!(seen.last(), Some(&Progress::Done));
        let calls = m.calls();
        assert!(calls.iter().any(|c| c.starts_with("tar -xpf")));
        assert!(calls.iter().any(|c| c.starts_with("efibootmgr -c")));
        assert_eq!(calls.last().map(String::as_str), Some("sync"));
        // Same sequence without a progress sink.
        Installer::new(&MockRunner::new(), true, &p)
            .run("r")
            .unwrap();
    }

    #[test]
    fn failing_step_stops_the_run() {
        let p = plan();
        let m = MockRunner::new().respond("gpart", &["create"], CmdOutput::failed(1, "busy"));
        let mut steps = 0;
        let err = Installer::new(&m, true, &p)
            .run_with("", &mut |e| {
                if matches!(e, Progress::Step { .. }) {
                    steps += 1
                }
            })
            .unwrap_err();
        assert!(err.to_string().contains("busy"), "{err}");
        assert_eq!(steps, 2, "stopped in partitioning");
        assert!(!m.calls().iter().any(|c| c.starts_with("zpool")));
    }

    #[test]
    fn invalid_plan_runs_nothing() {
        let mut p = plan();
        p.disk.clear();
        let m = MockRunner::new();
        assert!(Installer::new(&m, true, &p).run("").is_err());
        assert!(m.calls().is_empty());
        let mut p = plan();
        p.hostname.clear();
        assert!(p.validate().is_err());
        let mut p = plan();
        p.user = Some((String::new(), "x".into()));
        assert!(p.validate().is_err());
    }

    #[test]
    fn dist_detection_and_verification() {
        let dir = std::env::temp_dir().join(format!("jos-install-dist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_string_lossy().into_owned();
        assert!(!dist_available(&d));
        // Real run without a MANIFEST: a clear error.
        let p = InstallPlan {
            dist_dir: d.clone(),
            ..plan()
        };
        let m = MockRunner::new();
        let err = Installer::new(&m, false, &p).verify_dist().unwrap_err();
        assert!(err.to_string().contains("live medium"), "{err}");
        // With a tarball and a matching manifest it verifies.
        let tarball = dir.join(dom0::DIST_TARBALL);
        std::fs::write(&tarball, b"dom0").unwrap();
        let mut manifest = DistManifest::default();
        manifest.add(
            juanitos_core::release::sha256_bytes(b"dom0"),
            4,
            dom0::DIST_TARBALL.to_string(),
        );
        std::fs::write(dir.join(dom0::DIST_MANIFEST), manifest.render()).unwrap();
        assert!(dist_available(&d));
        Installer::new(&m, false, &p).verify_dist().unwrap();
        // Tampered tarball: rejected.
        std::fs::write(&tarball, b"evil").unwrap();
        assert!(Installer::new(&m, false, &p).verify_dist().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn configure_writes_the_system_files() {
        let dir = std::env::temp_dir().join(format!("jos-install-cfg-{}", std::process::id()));
        let p = InstallPlan {
            target: dir.to_string_lossy().into_owned(),
            ..plan()
        };
        let m = MockRunner::new();
        let inst = Installer::new(&m, false, &p);
        inst.configure("doctor says ok").unwrap();
        let read = |rel: &str| std::fs::read_to_string(dir.join(rel)).unwrap();
        assert!(read("etc/rc.conf").contains("hostname=\"box\""));
        assert!(read("etc/rc.conf").contains("jos_login_enable=\"YES\""));
        assert!(read("boot/loader.conf").contains("zfs_load=\"YES\""));
        assert!(read("boot/loader.conf").contains("\npptdevs=\"1/0/0 1/0/1\"\n"));
        assert!(read("etc/fstab").contains("/boot/efi"));
        // The login screen has a VT of its own: the base's ttys/gettytab
        // stay as FreeBSD ships them.
        assert!(!dir.join("etc/ttys").exists());
        assert!(!dir.join("etc/gettytab").exists());
        assert!(read("usr/local/etc/juanitos/jos.toml").contains("zroot"));
        assert_eq!(
            read("usr/local/etc/juanitos/install-report.txt"),
            "doctor says ok"
        );
        assert!(dir.join("usr/local/etc/juanitos/qubes").is_dir());
        assert!(m
            .calls()
            .iter()
            .any(|c| c.starts_with("cp /tmp/zpool.cache")));

        // Boot loader: the ESP gets loader.efi (no jos-boot.efi in this root).
        std::fs::create_dir_all(dir.join("boot")).unwrap();
        std::fs::write(dir.join("boot/loader.efi"), b"efi").unwrap();
        inst.install_bootloader().unwrap();
        let calls = m.calls();
        assert!(calls.iter().any(|c| c.starts_with("mount -t msdosfs")));
        assert!(calls.iter().any(|c| c.starts_with("umount")));
        assert!(calls
            .iter()
            .any(|c| c == "gpart bootcode -b /boot/pmbr -p /boot/gptzfsboot -i 2 vtbd0"));
        assert!(dir.join("boot/efi").join(dom0::ESP_DEFAULT).is_file());
        assert!(!dir.join("boot/efi").join(dom0::ESP_IA32).exists());
        // A base with loader_ia32.efi also boots 32-bit UEFI firmware.
        std::fs::write(dir.join("boot/loader_ia32.efi"), b"ia32").unwrap();
        inst.install_bootloader().unwrap();
        assert!(dir.join("boot/efi").join(dom0::ESP_IA32).is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn console_only_installs_have_no_login_screen() {
        let dir = std::env::temp_dir().join(format!("jos-install-nogui-{}", std::process::id()));
        let p = InstallPlan {
            target: dir.to_string_lossy().into_owned(),
            gui: false,
            ..plan()
        };
        let m = MockRunner::new();
        Installer::new(&m, false, &p)
            .configure("doctor says ok")
            .unwrap();
        let rc = std::fs::read_to_string(dir.join("etc/rc.conf")).unwrap();
        assert!(!rc.contains("jos_login_enable"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summary_mentions_disk_and_gui() {
        let s = plan().summary();
        assert!(s.contains("/dev/vtbd0"));
        assert!(s.contains("COSMIC"));
        assert!(s.contains("juan"));
        assert!(s.contains("passthru  1/0/0 1/0/1\n"), "{s}");
        let none = InstallPlan {
            pptdevs: Vec::new(),
            ..plan()
        };
        assert!(none.summary().contains("passthru  none\n"));
    }
}
