//! The installation itself: GPT + ESP + ZFS pool, extract the dom0 tarball
//! the live medium booted from, configure, done.
//!
//! The layout follows what FreeBSD's `bsdinstall zfsboot` produces so that
//! the result is a boring, well-understood FreeBSD-on-ZFS system.

use std::path::{Path, PathBuf};

use juanitos_core::dom0::{self, Dom0Options, Flavor};
use juanitos_core::fsops::FsOps;
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
             user      {user}\n  \
             dist      {dist}\n",
            dev = self.dev(),
            pool = self.pool,
            host = self.hostname,
            swap = self.swap_gb,
            gui = if self.gui { "COSMIC" } else { "console only" },
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
        self.plan.validate()?;
        self.verify_dist()?;
        self.partition()?;
        self.create_pool()?;
        self.extract()?;
        self.configure(doctor_report)?;
        self.install_bootloader()?;
        self.accounts()?;
        self.finish()
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
        self.step(&format!("Partitioning {dev} (GPT, UEFI)"));
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
        self.step("Installing UEFI boot loader");
        let esp_s = self.target("boot/efi");
        self.runner
            .run_checked("mount", &["-t", "msdosfs", "/dev/gpt/efiboot0", &esp_s])?;
        // Same layout as the live medium's ESP: the graphical pre-loader
        // (when the release ships it) as the default boot file, FreeBSD's
        // loader in EFI/FREEBSD for it to chain-load.
        let has_jos_boot = self
            .target_path(dom0::JOS_BOOT_EFI.trim_start_matches('/'))
            .is_file();
        let result = (|| {
            for (src, dst) in dom0::esp_files(has_jos_boot) {
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
            "gpart add -a 1m -s 4G -t freebsd-swap -l swap0 vtbd0"
        );
        assert_eq!(calls[4], "gpart add -a 1m -t freebsd-zfs -l zfs0 vtbd0");
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
    fn summary_mentions_disk_and_gui() {
        let s = plan().summary();
        assert!(s.contains("/dev/vtbd0"));
        assert!(s.contains("COSMIC"));
        assert!(s.contains("juan"));
    }
}
