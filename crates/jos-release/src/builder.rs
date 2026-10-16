//! The build steps: fetch → bins → rootfs → pack → iso.
//!
//! Must run as root on FreeBSD (or JuanitOS): it uses the FreeBSD release
//! tooling — `tar`, `pkg -c`, `makefs` — exactly the way
//! `release/amd64/mkisoimages.sh` and `release/tools/vmimage.subr` do in
//! the FreeBSD source tree. `--dry-run` prints every step and works on any
//! host.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};

use juanitos_core::dom0::{self, Dom0Options, Flavor};
use juanitos_core::fsops::FsOps;
use juanitos_core::release::{parse_freebsd_manifest, sha256_file, DistManifest, ReleaseInfo};
use juanitos_core::sys::Runner;
use juanitos_core::Error;

use crate::profile::Profile;
use crate::ventoy;

/// FreeBSD source set, fetched only to build kernel modules.
pub const SRC_SET: &str = "src.txz";
/// Ventoy's GEOM class, shipped on the live ISO.
pub const VENTOY_KO: &str = "geom_ventoy.ko";

pub struct Builder<'a> {
    pub runner: &'a dyn Runner,
    pub fs: FsOps,
    pub profile: &'a Profile,
    /// Repository root (contains `Cargo.toml`, `release/`).
    pub repo: PathBuf,
    /// Scratch and output directory.
    pub work: PathBuf,
    /// Directory holding the release binaries to ship.
    pub bins: PathBuf,
    /// `jos-firewall.img` unikernel image to ship; built from
    /// `jos-firewall.efi` when not given.
    pub firewall_image: Option<PathBuf>,
    /// `jos-boot.efi` graphical pre-loader to ship; taken from the UEFI
    /// build output when not given.
    pub boot_efi: Option<PathBuf>,
    /// Root of the running system, where a dom0 rebuilding itself finds
    /// the UEFI artifacts it was installed with and its own `release.toml`
    /// (`/`; tests use a scratch directory).
    pub installed_root: PathBuf,
}

/// Rust target of the UEFI applications.
pub const UEFI_TARGET: &str = "x86_64-unknown-uefi";

/// The UEFI crates live outside the host workspace (they only build for
/// [`UEFI_TARGET`]).
pub const UEFI_CRATES: &[&str] = &["jos-boot", "jos-firewall"];

impl<'a> Builder<'a> {
    pub fn dist_dir(&self) -> PathBuf {
        self.work.join("dist")
    }
    /// Where `cargo build --target x86_64-unknown-uefi` puts the `.efi`s.
    pub fn uefi_release_dir(&self) -> PathBuf {
        self.repo
            .join("target/uefi")
            .join(UEFI_TARGET)
            .join("release")
    }
    pub fn firewall_image_path(&self) -> PathBuf {
        self.work.join("jos-firewall.img")
    }
    /// A UEFI artifact to ship: the explicit flag, else the one this build
    /// produced, else the one the running system was installed with. The
    /// last case is stage 2: FreeBSD's `rust` has no `x86_64-unknown-uefi`
    /// target, so a dom0 rebuilding itself inherits the pre-loader and the
    /// firewall it boots with rather than dropping them.
    fn uefi_artifact(
        &self,
        explicit: &Option<PathBuf>,
        built: PathBuf,
        installed: &str,
    ) -> Option<PathBuf> {
        explicit.clone().or_else(|| {
            [
                built,
                self.installed_root.join(installed.trim_start_matches('/')),
            ]
            .into_iter()
            .find(|p| p.is_file())
        })
    }
    fn boot_efi(&self) -> Option<PathBuf> {
        self.uefi_artifact(
            &self.boot_efi,
            self.uefi_release_dir().join("jos-boot.efi"),
            dom0::JOS_BOOT_EFI,
        )
    }
    fn firewall_image(&self) -> Option<PathBuf> {
        self.uefi_artifact(
            &self.firewall_image,
            self.firewall_image_path(),
            dom0::FIREWALL_IMAGE,
        )
    }
    pub fn rootfs(&self) -> PathBuf {
        self.work.join("rootfs")
    }
    pub fn tarball(&self) -> PathBuf {
        self.work.join(dom0::DIST_TARBALL)
    }
    pub fn manifest(&self) -> PathBuf {
        self.work.join(dom0::DIST_MANIFEST)
    }
    pub fn iso(&self) -> PathBuf {
        self.work.join(self.profile.iso_name())
    }

    fn step(&self, msg: &str) {
        println!("==> {msg}");
    }

    fn s(p: &Path) -> String {
        unix(p)
    }

    /// Everything, in order.
    pub fn all(&self) -> anyhow::Result<()> {
        self.fetch()?;
        self.bins()?;
        self.rootfs_build()?;
        self.pack()?;
        self.iso_build()
    }

    // ------------------------------------------------------------------
    // fetch
    // ------------------------------------------------------------------

    /// Download the FreeBSD distribution sets and verify them against the
    /// release `MANIFEST`.
    pub fn fetch(&self) -> anyhow::Result<()> {
        let p = self.profile;
        let dir = self.dist_dir();
        self.step(&format!(
            "Fetching FreeBSD {} {} distribution sets into {}",
            p.freebsd_version,
            p.arch,
            dir.display()
        ));
        self.fs.mkdir_p(&dir)?;
        self.download(&p.dist_url("MANIFEST"), &dir.join("MANIFEST"))?;
        for file in &p.dists {
            let dst = dir.join(file);
            if dst.is_file() {
                println!("   {file}: already present");
            } else {
                self.download(&p.dist_url(file), &dst)?;
            }
        }
        if self.fs.dry_run && !dir.join("MANIFEST").is_file() {
            return Ok(());
        }
        let manifest = parse_freebsd_manifest(&std::fs::read_to_string(dir.join("MANIFEST"))?);
        for file in &p.dists {
            let entry = manifest
                .iter()
                .find(|e| &e.file == file)
                .ok_or_else(|| anyhow!("{file} not listed in FreeBSD MANIFEST"))?;
            DistManifest::verify_sha256(&dir.join(file), &entry.sha256)?;
            println!("   {file}: sha256 OK");
        }
        Ok(())
    }

    /// `fetch(1)` on FreeBSD, `curl` elsewhere.
    fn download(&self, url: &str, dst: &Path) -> anyhow::Result<()> {
        let dst_s = Self::s(dst);
        match self.runner.run("fetch", &["-o", &dst_s, url]) {
            Ok(out) if out.success() => return Ok(()),
            Ok(out) => {
                if out.status != Some(127) {
                    bail!("fetch {url}: {}", out.stderr.trim());
                }
            }
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.runner
            .run_checked("curl", &["-fL", "-o", &dst_s, url])
            .with_context(|| format!("downloading {url}"))?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // bins
    // ------------------------------------------------------------------

    /// Build the Rust binaries that ship in the dom0: the host workspace,
    /// then the UEFI applications (best effort: a missing
    /// `x86_64-unknown-uefi` target only costs the graphical boot and the
    /// unikernel firewall image), then the firewall boot image.
    pub fn bins(&self) -> anyhow::Result<()> {
        self.step("Building release binaries");
        let manifest = self.repo.join("Cargo.toml");
        self.runner.run_checked(
            "cargo",
            &[
                "build",
                "--release",
                "--workspace",
                "--manifest-path",
                &Self::s(&manifest),
            ],
        )?;

        self.step("Building UEFI applications (jos-boot, jos-firewall)");
        let target_dir = Self::s(&self.repo.join("target/uefi"));
        for name in UEFI_CRATES {
            let manifest = self.repo.join("crates").join(name).join("Cargo.toml");
            let out = self.runner.run(
                "cargo",
                &[
                    "build",
                    "--release",
                    "--manifest-path",
                    &Self::s(&manifest),
                    "--target",
                    UEFI_TARGET,
                    "--target-dir",
                    &target_dir,
                ],
            )?;
            if !out.success() {
                let last = out.stderr.lines().last().unwrap_or("").trim();
                println!(
                    "   warning: {name} not built ({last}); \
                     `rustup target add {UEFI_TARGET}` enables it"
                );
            }
        }
        self.firewall_image_build()?;
        Ok(())
    }

    /// Wrap `jos-firewall.efi` in the FAT image the bhyve UEFI bootrom
    /// boots (`EFI/BOOT/BOOTX64.EFI` on an unpartitioned volume, like the
    /// ISO's `efiboot.img`).
    pub fn firewall_image_build(&self) -> anyhow::Result<()> {
        let efi = self.uefi_release_dir().join("jos-firewall.efi");
        if !efi.is_file() && !self.fs.dry_run {
            println!("   no jos-firewall.efi; the unikernel image is not built");
            return Ok(());
        }
        let img = self.firewall_image_path();
        self.step(&format!("Building {}", img.display()));
        let dir = self.work.join("fwimg");
        self.fs.remove_tree(&dir)?;
        self.fs.copy(&efi, &dir.join(dom0::ESP_DEFAULT))?;
        self.runner.run_checked(
            "makefs",
            &[
                "-t",
                "msdos",
                "-o",
                "fat_type=12",
                "-o",
                "sectors_per_cluster=1",
                "-o",
                "volume_label=JOSFW",
                "-s",
                "2048k",
                &Self::s(&img),
                &Self::s(&dir),
            ],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // rootfs
    // ------------------------------------------------------------------

    /// Extract FreeBSD, add packages, add JuanitOS.
    pub fn rootfs_build(&self) -> anyhow::Result<()> {
        let root = self.rootfs();
        let root_s = Self::s(&root);
        self.step(&format!("Building dom0 root at {root_s}"));
        self.fs.remove_tree(&root)?;
        self.fs.mkdir_p(&root)?;

        for file in &self.profile.dists {
            let src = self.dist_dir().join(file);
            self.runner
                .run_checked("tar", &["-xpf", &Self::s(&src), "-C", &root_s, "--unlink"])?;
        }

        self.install_packages()?;
        self.install_juanitos()?;
        Ok(())
    }

    /// pkg(8) repository configuration inside the root: the FreeBSD branch
    /// the profile asks for, and the local COSMIC repository when the
    /// profile points at one (copied into the root so it is usable both
    /// from the chroot now and from the installed system later).
    fn install_repositories(&self) -> anyhow::Result<()> {
        let root = self.rootfs();
        let branch = self.profile.package_branch()?;
        if branch != "quarterly" {
            self.fs.write(
                &root.join(dom0::FREEBSD_REPO_CONF.trim_start_matches('/')),
                &dom0::freebsd_repo_conf(branch),
            )?;
        }
        let Some(rel) = &self.profile.packages.local_repo else {
            return Ok(());
        };
        // The profile's directory, else the one this dom0 was installed
        // with: stage 2 has no build VM output but keeps its desktop.
        let has_catalogue =
            |d: &Path| d.join("packagesite.pkg").is_file() || d.join("packagesite.txz").is_file();
        let from_profile = self.repo.join(rel);
        let from_install = self
            .installed_root
            .join(dom0::COSMIC_REPO_DIR.trim_start_matches('/'));
        let Some(src) = [&from_profile, &from_install]
            .into_iter()
            .find(|d| has_catalogue(d))
        else {
            println!(
                "   warning: no package catalogue in {} (run tools/dev/cosmic-build.sh); \
                 COSMIC packages will not be installed",
                from_profile.display()
            );
            return Ok(());
        };
        let dst = root.join(dom0::COSMIC_REPO_DIR.trim_start_matches('/'));
        self.step(&format!(
            "Shipping the COSMIC package repository from {}",
            src.display()
        ));
        self.fs.copy_tree(src, &dst, &[])?;
        self.fs.write(
            &root.join(dom0::COSMIC_REPO_CONF.trim_start_matches('/')),
            &dom0::cosmic_repo_conf(),
        )?;
        Ok(())
    }

    fn install_packages(&self) -> anyhow::Result<()> {
        let root = self.rootfs();
        let root_s = Self::s(&root);
        let (required, optional) = self.profile.package_sets();
        self.step(&format!(
            "Installing packages ({} required, {} best-effort)",
            required.len(),
            optional.len()
        ));
        self.install_repositories()?;

        // pkg needs DNS and /dev inside the chroot (as release/tools/vmimage.subr).
        if Path::new("/etc/resolv.conf").is_file() {
            self.fs
                .copy(Path::new("/etc/resolv.conf"), &root.join("etc/resolv.conf"))?;
        }
        let devfs = format!("{root_s}/dev");
        self.runner
            .run_checked("mount", &["-t", "devfs", "devfs", &devfs])?;

        let result = (|| {
            // Every pkg call runs with the chroot's own database and
            // catalogue; `ASSUME_ALWAYS_YES` also answers the bootstrap
            // prompt. A fresh root has no catalogue, so `update` must come
            // before the first `install` (as release/tools/vmimage.subr).
            let pkg = |args: &[&str]| -> Vec<String> {
                let mut v = vec![
                    "ASSUME_ALWAYS_YES=yes".to_string(),
                    "pkg".to_string(),
                    "-c".to_string(),
                    root_s.clone(),
                ];
                v.extend(args.iter().map(|s| s.to_string()));
                v
            };
            let run_pkg = |args: &[&str]| {
                let argv = pkg(args);
                let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
                self.runner.run("env", &argv)
            };
            let run_pkg_checked = |args: &[&str]| {
                let argv = pkg(args);
                let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
                self.runner.run_checked("env", &argv)
            };
            run_pkg_checked(&["bootstrap", "-y"])?;
            run_pkg_checked(&["update", "-f"])?;
            let mut args = vec!["install", "-y"];
            args.extend(required.iter().copied());
            run_pkg_checked(&args)?;
            for name in &optional {
                let out = run_pkg(&["install", "-y", name])?;
                if !out.success() {
                    println!("   warning: optional package {name} not installed");
                }
            }
            run_pkg_checked(&["clean", "-ay"])?;
            Ok::<(), anyhow::Error>(())
        })();

        let _ = self.runner.run("umount", &[&devfs]);
        result
    }

    fn install_juanitos(&self) -> anyhow::Result<()> {
        let root = self.rootfs();
        self.step("Installing JuanitOS into the root");

        for bin in dom0::BINARIES {
            let src = self.bins.join(bin);
            if !src.is_file() && !self.fs.dry_run {
                bail!(
                    "{} not found; run `jos-release bins` or pass --bins",
                    src.display()
                );
            }
            let dst = root.join("usr/local/bin").join(bin);
            self.fs.copy(&src, &dst)?;
            self.fs.chmod(&dst, 0o755)?;
        }

        for (path, content, exec) in dom0::static_files() {
            let dst = root.join(path.trim_start_matches('/'));
            if exec {
                self.fs.write_executable(&dst, &content)?;
            } else {
                self.fs.write(&dst, &content)?;
            }
        }

        match self.firewall_image() {
            Some(img) => self.fs.copy(
                &img,
                &root.join(dom0::FIREWALL_IMAGE.trim_start_matches('/')),
            )?,
            None => println!("   no unikernel firewall image: sys-firewall will need --os freebsd"),
        }
        match self.boot_efi() {
            Some(efi) => self
                .fs
                .copy(&efi, &root.join(dom0::JOS_BOOT_EFI.trim_start_matches('/')))?,
            None => {
                println!("   no jos-boot.efi: the ESP will boot loader.efi directly (text boot)")
            }
        }
        self.install_splash()?;

        // Base has no /etc/fstab; the installer and the ISO step write
        // their own, but an empty one keeps rc quiet in between.
        self.fs.write(&root.join("etc/fstab"), "")?;
        self.fs
            .write(&root.join("etc/motd.template"), &motd(self.profile))?;
        self.fs
            .mkdir_p(&root.join("usr/local/etc/juanitos/qubes"))?;

        if self.profile.source.include {
            self.install_source()?;
        }

        let info = self.release_info()?;
        self.fs.write(
            &root.join(dom0::RELEASE_INFO_PATH.trim_start_matches('/')),
            &info.to_toml()?,
        )?;
        Ok(())
    }

    /// The kernel splash: the boot card with the first two stages lit,
    /// which is exactly what `jos-boot` left on the screen.
    fn install_splash(&self) -> anyhow::Result<()> {
        let dst = self
            .rootfs()
            .join(dom0::BOOT_SPLASH_PNG.trim_start_matches('/'));
        let png = jos_gfx::png::encode(&kernel_splash_card())?;
        self.fs.write_bytes(&dst, &png)?;
        Ok(())
    }

    /// Copy this repository into the root (for stage 2: building JuanitOS
    /// on JuanitOS) and vendor the crates so that works offline.
    fn install_source(&self) -> anyhow::Result<()> {
        let dst = self.rootfs().join("usr/local/src/juanitos");
        self.step(&format!("Copying source tree to {}", dst.display()));
        self.fs.remove_tree(&dst)?;
        let excludes = source_excludes(self.profile.source.vendor);
        let excludes: Vec<&str> = excludes.iter().map(String::as_str).collect();
        self.fs.copy_tree(&self.repo, &dst, &excludes)?;
        if self.profile.source.vendor {
            let manifest = dst.join("Cargo.toml");
            let vendor = dst.join("vendor");
            self.runner.run_checked(
                "cargo",
                &[
                    "vendor",
                    "--manifest-path",
                    &Self::s(&manifest),
                    &Self::s(&vendor),
                ],
            )?;
            // Keep the repository's own config (the `cargo xtask` alias)
            // and add the vendored source on top.
            let repo_config =
                std::fs::read_to_string(self.repo.join(".cargo/config.toml")).unwrap_or_default();
            self.fs.write(
                &dst.join(".cargo/config.toml"),
                &vendored_cargo_config(&repo_config),
            )?;
        }
        Ok(())
    }

    /// Stamp: what was built, where, and which bootstrap stage this is.
    pub fn release_info(&self) -> anyhow::Result<ReleaseInfo> {
        let p = self.profile;
        let previous = self
            .installed_root
            .join(dom0::RELEASE_INFO_PATH.trim_start_matches('/'));
        let (built_on, stage) = match std::fs::read_to_string(previous)
            .ok()
            .and_then(|t| ReleaseInfo::from_toml(&t).ok())
        {
            Some(prev) => ("juanitos".to_string(), prev.stage + 1),
            None => ("freebsd".to_string(), 1),
        };
        let git_rev = self
            .runner
            .run(
                "git",
                &["-C", &Self::s(&self.repo), "rev-parse", "--short", "HEAD"],
            )
            .ok()
            .filter(|o| o.success())
            .map(|o| o.stdout.trim().to_string())
            .filter(|s| !s.is_empty());
        let (required, optional) = p.package_sets();
        Ok(ReleaseInfo {
            version: p.version.clone(),
            freebsd_version: p.freebsd_version.clone(),
            arch: p.arch.clone(),
            built_at: now_utc(),
            built_on,
            stage,
            packages: required
                .iter()
                .chain(optional.iter())
                .map(|s| s.to_string())
                .collect(),
            git_rev,
        })
    }

    // ------------------------------------------------------------------
    // pack
    // ------------------------------------------------------------------

    /// Tar the root into `juanitos-dom0.txz` and write its MANIFEST. This
    /// single artifact is what both the live medium and the installer use.
    pub fn pack(&self) -> anyhow::Result<()> {
        let tarball = self.tarball();
        self.step(&format!("Packing {}", tarball.display()));
        // bsdtar: multi-threaded xz (a single thread takes half an hour
        // for a 2 GB root in a VM).
        self.runner.run_checked(
            "tar",
            &[
                "-C",
                &Self::s(&self.rootfs()),
                "-cJf",
                &Self::s(&tarball),
                "--options",
                "xz:threads=0",
                "--numeric-owner",
                ".",
            ],
        )?;
        if self.fs.dry_run && !tarball.is_file() {
            self.fs.write(&self.manifest(), "")?;
            return Ok(());
        }
        let mut m = DistManifest::default();
        m.add(
            sha256_file(&tarball)?,
            std::fs::metadata(&tarball)?.len(),
            dom0::DIST_TARBALL.to_string(),
        );
        self.fs.write(&self.manifest(), &m.render())?;
        println!("   {}", m.render().trim_end());
        Ok(())
    }

    // ------------------------------------------------------------------
    // iso
    // ------------------------------------------------------------------

    /// Turn the root into a live/install ISO (mirrors FreeBSD's
    /// `release/amd64/mkisoimages.sh`).
    pub fn iso_build(&self) -> anyhow::Result<()> {
        let p = self.profile;
        let root = self.rootfs();
        let iso = self.iso();
        self.step(&format!("Building {}", iso.display()));

        // Live overlay.
        let opts = Dom0Options {
            hostname: "juanitos-live".into(),
            gui: p.packages.gui,
            serial_console: true,
            // The live medium runs the installer on the text console; the
            // KMS driver is chosen for the installed system.
            gpu_module: None,
        };
        self.fs
            .write(&root.join("etc/fstab"), &dom0::live_fstab(&p.label))?;
        self.fs.write(
            &root.join("etc/rc.conf"),
            &dom0::rc_conf(Flavor::Live, &opts),
        )?;
        let mut loader_conf = dom0::loader_conf(Flavor::Live, &opts);
        match self.ventoy_module() {
            Ok(true) => loader_conf.push_str(&ventoy::loader_conf_lines()),
            Ok(false) => {}
            Err(e) => println!(
                "   warning: geom_ventoy.ko not built ({e:#}); \
                 the ISO needs Ventoy >= 1.1.08 or a dd-written stick"
            ),
        }
        self.fs
            .write(&root.join("boot/loader.conf"), &loader_conf)?;
        self.fs
            .write_executable(&root.join("etc/rc.local"), &dom0::live_rc_local())?;

        // The dom0 tarball the installer extracts.
        let dist = root.join(dom0::DIST_DIR.trim_start_matches('/'));
        self.fs
            .copy(&self.tarball(), &dist.join(dom0::DIST_TARBALL))?;
        self.fs
            .copy(&self.manifest(), &dist.join(dom0::DIST_MANIFEST))?;

        // EFI system partition image: jos-boot.efi as the default boot
        // file when shipped, FreeBSD's loader next to it.
        let esp_dir = self.work.join("efiboot");
        let esp_img = self.work.join("efiboot.img");
        self.fs.remove_tree(&esp_dir)?;
        let has_jos_boot = self.boot_efi().is_some()
            || root
                .join(dom0::JOS_BOOT_EFI.trim_start_matches('/'))
                .is_file();
        let has_ia32 = root
            .join(dom0::LOADER_IA32_EFI.trim_start_matches('/'))
            .is_file();
        for (src, dst) in dom0::esp_files(has_jos_boot, has_ia32) {
            self.fs
                .copy(&root.join(src.trim_start_matches('/')), &esp_dir.join(dst))?;
        }
        self.runner.run_checked(
            "makefs",
            &[
                "-t",
                "msdos",
                "-o",
                "fat_type=16",
                "-o",
                "sectors_per_cluster=1",
                "-o",
                "volume_label=EFISYS",
                "-s",
                "8192k",
                &Self::s(&esp_img),
                &Self::s(&esp_dir),
            ],
        )?;

        let bios = p.bios && root.join("boot/cdboot").is_file();
        let args = iso_args(p, &root, &esp_img, &iso, bios);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run_checked("makefs", &args)?;
        self.make_hybrid(&root, &iso, bios)?;

        if !self.fs.dry_run {
            let sha = sha256_file(&iso)?;
            self.fs.write(
                &iso.with_extension("iso.sha256"),
                &format!("{sha}  {}\n", p.iso_name()),
            )?;
            println!("   sha256 {sha}");
        }
        println!("\nDone: {}", iso.display());
        Ok(())
    }
}

impl Builder<'_> {
    /// FreeBSD kernel sources for building modules: the running system's
    /// `/usr/src/sys` when it matches, else `src.txz` of the profile's
    /// release (verified against its MANIFEST), extracted under the work
    /// directory.
    fn kernel_sources(&self) -> anyhow::Result<PathBuf> {
        let installed = self.installed_root.join("usr/src/sys");
        if installed.join("conf/kmod.mk").is_file() {
            return Ok(installed);
        }
        let dir = self.dist_dir();
        let src = dir.join(SRC_SET);
        if !src.is_file() {
            self.fs.mkdir_p(&dir)?;
            if !dir.join("MANIFEST").is_file() {
                self.download(&self.profile.dist_url("MANIFEST"), &dir.join("MANIFEST"))?;
            }
            self.download(&self.profile.dist_url(SRC_SET), &src)?;
        }
        if !self.fs.dry_run {
            let manifest = parse_freebsd_manifest(&std::fs::read_to_string(dir.join("MANIFEST"))?);
            let entry = manifest
                .iter()
                .find(|e| e.file == SRC_SET)
                .ok_or_else(|| anyhow!("{SRC_SET} not listed in FreeBSD MANIFEST"))?;
            DistManifest::verify_sha256(&src, &entry.sha256)?;
        }
        let out = self.work.join("src");
        let sys = out.join("usr/src/sys");
        if !sys.join("conf/kmod.mk").is_file() {
            self.fs.mkdir_p(&out)?;
            self.runner.run_checked(
                "tar",
                &["-xf", &Self::s(&src), "-C", &Self::s(&out), "usr/src/sys"],
            )?;
        }
        Ok(sys)
    }

    /// Build `freebsd/geom_ventoy` against the release's kernel sources
    /// and put it in the live root (see [`ventoy`]). `Ok(false)` when
    /// there is nothing to build (no vendored source).
    fn ventoy_module(&self) -> anyhow::Result<bool> {
        let vendored = self.repo.join("freebsd/geom_ventoy");
        if !vendored.join("Makefile").is_file() {
            return Ok(false);
        }
        self.step("Building geom_ventoy.ko (boot from any Ventoy version)");
        let sys = self.kernel_sources()?;
        let build = self.work.join("geom_ventoy");
        self.fs.remove_tree(&build)?;
        self.fs.copy_tree(&vendored, &build, &[])?;
        self.runner.run_checked(
            "make",
            &["-C", &Self::s(&build), &format!("SYSDIR={}", Self::s(&sys))],
        )?;
        let ko = build.join(VENTOY_KO);
        let dst = self.rootfs().join("boot/kernel").join(VENTOY_KO);
        if self.fs.dry_run {
            self.fs.copy(&ko, &dst)?;
        } else {
            let mut bytes = std::fs::read(&ko)?;
            let at = ventoy::patch_module(&mut bytes)?;
            println!("   Ventoy disk map at file offset {at:#x}");
            self.fs.write_bytes(&dst, &bytes)?;
        }
        // linker.hints; the loader also finds the module by file name.
        let _ = self
            .runner
            .run("kldxref", &[&Self::s(&self.rootfs().join("boot/kernel"))]);
        Ok(true)
    }

    /// Make the ISO bootable when written to a USB stick, as FreeBSD's
    /// `mkisoimages.sh` does: a protective MBR plus a GPT in the ISO's
    /// system area, with `isoboot` for BIOS and a partition pointing at
    /// the El Torito EFI image for UEFI firmware (which only honours El
    /// Torito on optical media).
    fn make_hybrid(&self, root: &Path, iso: &Path, bios: bool) -> anyhow::Result<()> {
        let pmbr = root.join(dom0::PMBR.trim_start_matches('/'));
        if !pmbr.is_file() && !self.fs.dry_run {
            println!("   no boot/pmbr: the ISO boots from optical media only");
            return Ok(());
        }
        let iso_s = Self::s(iso);
        let out = self
            .runner
            .run_checked("etdump", &["--format", "shell", &iso_s])?;
        let (esp, capacity) = if self.fs.dry_run {
            (etdump_efi_entry(&out.stdout).unwrap_or((0, 0)), 0)
        } else {
            let esp = etdump_efi_entry(&out.stdout)
                .ok_or_else(|| anyhow::anyhow!("{iso_s}: no EFI El Torito entry"))?;
            (esp, std::fs::metadata(iso)?.len())
        };
        let img = self.work.join("hybrid.img");
        let isoboot = root.join("boot/isoboot");
        let args = hybrid_args(
            &pmbr,
            (bios && isoboot.is_file()).then_some(isoboot.as_path()),
            esp,
            capacity,
            &img,
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run_checked("mkimg", &args)?;
        self.runner.run_checked(
            "dd",
            &[
                &format!("if={}", Self::s(&img)),
                &format!("of={iso_s}"),
                "bs=32k",
                "count=1",
                "conv=notrunc",
            ],
        )?;
        self.fs.remove_file(&img)?;
        Ok(())
    }
}

/// `(lba, sectors)` of the EFI boot image from `etdump --format shell`:
/// one `et_platform=efi;et_system=i386;et_lba=20;et_sectors=4096` line
/// per El Torito entry (LBA in 2048-byte blocks, sectors in 512 bytes).
pub fn etdump_efi_entry(text: &str) -> Option<(u64, u64)> {
    text.lines().find_map(|line| {
        let field = |name: &str| {
            line.split(';')
                .find_map(|kv| kv.trim().strip_prefix(name)?.strip_prefix('='))
        };
        if field("et_platform")? != "efi" {
            return None;
        }
        Some((
            field("et_lba")?.parse().ok()?,
            field("et_sectors")?.parse().ok()?,
        ))
    })
}

/// `mkimg` arguments for the hybrid boot area (see `make_hybrid`).
pub fn hybrid_args(
    pmbr: &Path,
    isoboot: Option<&Path>,
    (lba, sectors): (u64, u64),
    capacity: u64,
    out: &Path,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-s".into(),
        "gpt".into(),
        "--capacity".into(),
        capacity.to_string(),
        "-b".into(),
        unix(pmbr),
    ];
    if let Some(isoboot) = isoboot {
        a.push("-p".into());
        a.push(format!("freebsd-boot:={}", unix(isoboot)));
    }
    a.push("-p".into());
    a.push(format!("efi::{}:{}", sectors * 512, lba * 2048));
    a.push("-o".into());
    a.push(unix(out));
    a
}

/// What `install_source` leaves out of the copied repository: VCS data and
/// everything `.gitignore` marks as build output or scratch, including the
/// UEFI crates' own `target/` (they are separate workspaces). `vendor/` is
/// skipped too when `cargo vendor` is about to regenerate it: a dom0
/// rebuilding itself would otherwise copy its existing one first, and a
/// symbolic link there would be followed by `cargo vendor` and shipped as
/// a link.
pub fn source_excludes(vendor: bool) -> Vec<String> {
    let mut v: Vec<String> = [".git", "target", "release/out", "work"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    v.extend(UEFI_CRATES.iter().map(|c| format!("crates/{c}/target")));
    if vendor {
        v.push("vendor".into());
    }
    v
}

/// `.cargo/config.toml` of the installed source tree: the repository's
/// own settings (the `cargo xtask` alias) plus the vendored crates as
/// the source for crates.io, so stage 2 builds offline.
pub fn vendored_cargo_config(repo_config: &str) -> String {
    let mut out = repo_config.trim_end().to_string();
    if out.contains("[source.vendored-sources]") {
        // Already a vendored copy (stage 2 building stage 3): adding the
        // tables again would make cargo reject the file as a duplicate.
        out.push('\n');
        return out;
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(
        "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n\
         [source.vendored-sources]\ndirectory = \"vendor\"\n",
    );
    out
}

/// The picture `vt(4)` centres on the screen while the kernel boots muted.
pub fn kernel_splash_card() -> jos_gfx::Image {
    let mut scene = jos_gfx::Scene::new();
    scene.reach(jos_gfx::Stage::Kernel);
    scene.render_card()
}

/// Render a path with forward slashes: these are FreeBSD paths even when
/// a dry run happens on Windows.
pub fn unix(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `makefs -t cd9660 ...` arguments, as in `mkisoimages.sh`.
pub fn iso_args(p: &Profile, root: &Path, esp_img: &Path, iso: &Path, bios: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["-t".into(), "cd9660".into()];
    if bios && p.bios {
        a.push("-o".into());
        a.push(format!(
            "bootimage=i386;{}",
            unix(&root.join("boot/cdboot"))
        ));
        a.push("-o".into());
        a.push("no-emul-boot".into());
    }
    a.push("-o".into());
    a.push(format!("bootimage=i386;{}", unix(esp_img)));
    a.push("-o".into());
    a.push("no-emul-boot".into());
    a.push("-o".into());
    a.push("platformid=efi".into());
    a.push("-o".into());
    a.push("rockridge".into());
    a.push("-o".into());
    a.push(format!("label={}", p.label));
    a.push("-o".into());
    a.push("publisher=JuanitOS".into());
    a.push(unix(iso));
    a.push(unix(root));
    a
}

fn motd(p: &Profile) -> String {
    format!(
        "JuanitOS {} (FreeBSD {} {})\n\
         FreeBSD + bhyve, QubesOS style. `jos doctor`, `jos ls`, `jos-install`.\n",
        p.version, p.freebsd_version, p.arch
    )
}

/// `YYYY-MM-DDTHH:MM:SSZ` without pulling in a date crate.
pub fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

pub fn format_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use juanitos_core::sys::MockRunner;

    #[test]
    fn vendored_config_keeps_the_repo_aliases() {
        let c = vendored_cargo_config("[alias]\nxtask = \"run --package xtask --\"\n");
        assert!(c.starts_with("[alias]\nxtask"));
        assert!(c.contains("\n\n[source.crates-io]\nreplace-with = \"vendored-sources\"\n"));
        assert!(c.ends_with("directory = \"vendor\"\n"));
        assert!(vendored_cargo_config("").starts_with("[source.crates-io]"));
        // Idempotent: a dom0 rebuilding itself starts from the vendored copy
        // (found by comparing stage-1 and stage-2 tarballs: the only
        // unintended difference was a second pair of [source] tables).
        let again = vendored_cargo_config(&c);
        assert_eq!(again, c);
        assert_eq!(again.matches("[source.crates-io]").count(), 1);
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_767_225_599), "2025-12-31T23:59:59Z");
    }

    #[test]
    fn iso_arguments_mirror_mkisoimages() {
        let p = Profile::default();
        let args = iso_args(
            &p,
            Path::new("/w/rootfs"),
            Path::new("/w/efiboot.img"),
            Path::new("/w/out.iso"),
            true,
        );
        let joined = args.join(" ");
        assert!(
            joined.starts_with("-t cd9660 -o bootimage=i386;/w/rootfs/boot/cdboot -o no-emul-boot")
        );
        assert!(
            joined.contains("-o bootimage=i386;/w/efiboot.img -o no-emul-boot -o platformid=efi")
        );
        assert!(joined.contains("-o label=JUANITOS"));
        assert!(joined.ends_with("/w/out.iso /w/rootfs"));

        let no_bios = iso_args(
            &p,
            Path::new("/w/rootfs"),
            Path::new("/w/efiboot.img"),
            Path::new("/w/out.iso"),
            false,
        );
        assert!(!no_bios.join(" ").contains("cdboot"));
    }

    #[test]
    fn dry_run_pipeline_issues_expected_commands() {
        let profile = Profile::default();
        let m = MockRunner::new();
        let b = Builder {
            runner: &m,
            fs: FsOps::new(true),
            profile: &profile,
            repo: PathBuf::from("/repo"),
            work: PathBuf::from("/work"),
            bins: PathBuf::from("/repo/target/release"),
            firewall_image: None,
            boot_efi: Some(PathBuf::from("/repo/target/uefi/jos-boot.efi")),
            installed_root: PathBuf::from("/nonexistent"),
        };
        b.bins().unwrap();
        b.rootfs_build().unwrap();
        b.pack().unwrap();
        b.iso_build().unwrap();
        let calls = m.calls();
        let has = |s: &str| calls.iter().any(|c| c.contains(s));
        assert!(has(
            "cargo build --release --workspace --manifest-path /repo/Cargo.toml"
        ));
        assert!(has(
            "cargo build --release --manifest-path /repo/crates/jos-boot/Cargo.toml \
             --target x86_64-unknown-uefi --target-dir /repo/target/uefi"
        ));
        assert!(has("--manifest-path /repo/crates/jos-firewall/Cargo.toml"));
        assert!(has("volume_label=JOSFW"));
        assert!(has("tar -xpf /work/dist/base.txz -C /work/rootfs --unlink"));
        assert!(has("mount -t devfs devfs /work/rootfs/dev"));
        assert!(has("pkg -c /work/rootfs bootstrap -y"));
        assert!(has("pkg -c /work/rootfs update -f"));
        assert!(has("pkg -c /work/rootfs install -y edk2-bhyve"));
        assert!(has("etdump --format shell /work/"));
        assert!(has("mkimg -s gpt --capacity 0 -b /work/rootfs/boot/pmbr"));
        assert!(has("bs=32k count=1 conv=notrunc"));
        // A fresh chroot has no catalogue: update must precede install.
        let update = calls.iter().position(|c| c.contains("update -f")).unwrap();
        let install = calls
            .iter()
            .position(|c| c.contains("install -y edk2-bhyve"))
            .unwrap();
        assert!(update < install);
        assert!(has("umount /work/rootfs/dev"));
        assert!(has("cargo vendor"));
        assert!(has(
            "tar -C /work/rootfs -cJf /work/juanitos-dom0.txz --options xz:threads=0 --numeric-owner ."
        ));
        assert!(has("makefs -t msdos"));
        assert!(has("makefs -t cd9660"));
        assert!(has("JuanitOS-"));
    }

    #[test]
    fn kernel_splash_has_two_lit_stages() {
        let card = kernel_splash_card();
        assert_eq!(card.width(), jos_gfx::CARD_WIDTH);
        let png = jos_gfx::png::encode(&card).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
    }

    #[test]
    fn release_info_defaults_to_stage_one_on_freebsd() {
        let profile = Profile::default();
        let m = MockRunner::new();
        let b = Builder {
            runner: &m,
            fs: FsOps::new(true),
            profile: &profile,
            repo: PathBuf::from("/repo"),
            work: PathBuf::from("/work"),
            bins: PathBuf::from("/bins"),
            firewall_image: None,
            boot_efi: None,
            installed_root: PathBuf::from("/nonexistent"),
        };
        let info = b.release_info().unwrap();
        // The test host is not a JuanitOS dom0.
        assert_eq!(info.built_on, "freebsd");
        assert_eq!(info.stage, 1);
        assert!(info.packages.contains(&"seatd".to_string()));
    }

    #[test]
    fn stage_two_inherits_installed_uefi_artifacts() {
        let tmp = std::env::temp_dir().join(format!("jos-stage2-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("root");
        let profile = Profile::default();
        let m = MockRunner::new();
        let mut b = Builder {
            runner: &m,
            fs: FsOps::new(true),
            profile: &profile,
            repo: tmp.join("repo"),
            work: tmp.join("work"),
            bins: tmp.join("bins"),
            firewall_image: None,
            boot_efi: None,
            installed_root: root.clone(),
        };
        // Nothing built, nothing installed: ship without them.
        assert_eq!(b.boot_efi(), None);
        assert_eq!(b.firewall_image(), None);

        // The running dom0's copies are the fallback ...
        let efi = root.join(dom0::JOS_BOOT_EFI.trim_start_matches('/'));
        let img = root.join(dom0::FIREWALL_IMAGE.trim_start_matches('/'));
        for p in [&efi, &img] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        assert_eq!(b.boot_efi(), Some(efi));
        assert_eq!(b.firewall_image(), Some(img));

        // ... behind what this build produced ...
        let built = b.uefi_release_dir().join("jos-boot.efi");
        std::fs::create_dir_all(built.parent().unwrap()).unwrap();
        std::fs::write(&built, b"y").unwrap();
        assert_eq!(b.boot_efi(), Some(built));

        // ... and an explicit flag wins over both.
        b.boot_efi = Some(PathBuf::from("/flag/jos-boot.efi"));
        assert_eq!(b.boot_efi(), Some(PathBuf::from("/flag/jos-boot.efi")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    use crate::profile::Source;
    use juanitos_core::sys::CmdOutput;

    /// A scratch directory removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("jos-builder-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn put(&self, rel: &str, content: &[u8]) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A builder working for real inside `dir` (repo, work, bins and the
    /// installed root all underneath it), host commands mocked.
    fn real_builder<'a>(runner: &'a dyn Runner, profile: &'a Profile, dir: &Path) -> Builder<'a> {
        Builder {
            runner,
            fs: FsOps::new(false),
            profile,
            repo: dir.join("repo"),
            work: dir.join("work"),
            bins: dir.join("bins"),
            firewall_image: None,
            boot_efi: None,
            installed_root: dir.join("root"),
        }
    }

    /// `fetch(1)` is missing on Linux: the runner reports the program as
    /// not found, or a shell says 127.
    struct NoFetch {
        inner: MockRunner,
        kind: std::io::ErrorKind,
    }

    impl Runner for NoFetch {
        fn run(&self, program: &str, args: &[&str]) -> juanitos_core::Result<CmdOutput> {
            if program == "fetch" {
                return Err(Error::Io(std::io::Error::new(self.kind, "fetch")));
            }
            self.inner.run(program, args)
        }
        fn run_with_input(
            &self,
            program: &str,
            args: &[&str],
            _input: &str,
        ) -> juanitos_core::Result<CmdOutput> {
            self.run(program, args)
        }
    }

    #[test]
    fn downloads_fall_back_from_fetch_to_curl() {
        let profile = Profile::default();
        let dst = Path::new("/w/MANIFEST");
        let curl = "curl -fL -o /w/MANIFEST https://m/MANIFEST";

        let m = MockRunner::new().respond("fetch", &[], CmdOutput::failed(127, "not found"));
        let b = real_builder(&m, &profile, Path::new("/nonexistent"));
        b.download("https://m/MANIFEST", dst).unwrap();
        assert_eq!(
            m.calls(),
            vec![
                "fetch -o /w/MANIFEST https://m/MANIFEST".to_string(),
                curl.into()
            ]
        );

        let missing = NoFetch {
            inner: MockRunner::new(),
            kind: std::io::ErrorKind::NotFound,
        };
        let b = real_builder(&missing, &profile, Path::new("/nonexistent"));
        b.download("https://m/MANIFEST", dst).unwrap();
        assert_eq!(missing.inner.calls(), vec![curl.to_string()]);

        // fetch exists but the download failed: no second try with curl.
        let m = MockRunner::new().respond("fetch", &[], CmdOutput::failed(1, "404 Not Found"));
        let b = real_builder(&m, &profile, Path::new("/nonexistent"));
        let err = b.download("https://m/MANIFEST", dst).unwrap_err();
        assert!(err.to_string().contains("404 Not Found"), "{err}");
        assert_eq!(m.calls().len(), 1);

        let denied = NoFetch {
            inner: MockRunner::new(),
            kind: std::io::ErrorKind::PermissionDenied,
        };
        let b = real_builder(&denied, &profile, Path::new("/nonexistent"));
        assert!(b.download("https://m/MANIFEST", dst).is_err());
        assert!(denied.inner.calls().is_empty());

        let m = MockRunner::new()
            .respond("fetch", &[], CmdOutput::failed(127, ""))
            .respond("curl", &[], CmdOutput::failed(22, "HTTP 500"));
        let b = real_builder(&m, &profile, Path::new("/nonexistent"));
        let err = b.download("https://m/MANIFEST", dst).unwrap_err();
        assert!(format!("{err:#}").contains("downloading https://m/MANIFEST"));
    }

    #[test]
    fn fetch_checks_the_sets_against_the_manifest() {
        let s = Scratch::new("fetch");
        let profile = Profile::default();
        let m = MockRunner::new();
        let b = real_builder(&m, &profile, &s.0);
        let base = s.put("work/dist/base.txz", b"base");
        let kernel = s.put("work/dist/kernel.txz", b"kernel");
        let line = |p: &Path| {
            format!(
                "{}\t{}\t1\tset\t\"Set\"\ton\n",
                p.file_name().unwrap().to_string_lossy(),
                sha256_file(p).unwrap()
            )
        };
        let manifest = format!("{}{}", line(&base), line(&kernel));
        s.put("work/dist/MANIFEST", manifest.as_bytes());

        // Sets already there are not downloaded again; only the MANIFEST is.
        b.fetch().unwrap();
        assert_eq!(m.calls().len(), 1);
        assert!(m.calls()[0].ends_with("/amd64/15.1-RELEASE/MANIFEST"));

        std::fs::write(&kernel, b"tampered").unwrap();
        assert!(b.fetch().is_err(), "a modified set is refused");

        s.put("work/dist/MANIFEST", line(&base).as_bytes());
        let err = b.fetch().unwrap_err();
        assert!(err.to_string().contains("kernel.txz not listed"), "{err}");
    }

    #[test]
    fn uefi_builds_are_best_effort() {
        let s = Scratch::new("bins");
        let profile = Profile::default();
        let m = MockRunner::new().respond(
            "cargo",
            &["build", "--release", "--manifest-path"],
            CmdOutput::failed(101, "error[E0463]: can't find crate for `core`"),
        );
        let b = real_builder(&m, &profile, &s.0);
        b.bins().unwrap();
        let calls = m.calls();
        assert_eq!(calls.iter().filter(|c| c.contains(UEFI_TARGET)).count(), 2);
        // No jos-firewall.efi was produced, so no firewall image either.
        assert!(!calls.iter().any(|c| c.starts_with("makefs")), "{calls:?}");

        // The host workspace itself is not optional.
        let m = MockRunner::new().respond(
            "cargo",
            &["build", "--release", "--workspace"],
            CmdOutput::failed(101, "error"),
        );
        let b = real_builder(&m, &profile, &s.0);
        assert!(b.bins().is_err());
        assert_eq!(m.calls().len(), 1);
    }

    #[test]
    fn optional_packages_may_fail_required_ones_may_not() {
        let profile = Profile::default();
        let (required, optional) = profile.package_sets();
        let failing = |name: &str| {
            MockRunner::new().respond(
                "env",
                &[
                    "ASSUME_ALWAYS_YES=yes",
                    "pkg",
                    "-c",
                    "/work/rootfs",
                    "install",
                    "-y",
                    name,
                ],
                CmdOutput::failed(1, "no such package"),
            )
        };
        let dry = |m: &MockRunner| {
            Builder {
                fs: FsOps::new(true),
                work: PathBuf::from("/work"),
                ..real_builder(m, &profile, Path::new("/x"))
            }
            .install_packages()
        };

        let m = failing(optional.first().unwrap());
        dry(&m).unwrap();
        assert!(m.calls().iter().any(|c| c.contains("clean -ay")));

        let m = failing(required.first().unwrap());
        assert!(dry(&m).is_err());
        let calls = m.calls();
        assert!(!calls.iter().any(|c| c.contains("clean -ay")));
        // devfs is unmounted whatever happened inside the chroot.
        assert_eq!(calls.last().unwrap(), "umount /work/rootfs/dev");
    }

    #[test]
    fn repositories_follow_the_profile() {
        // Default profile: FreeBSD's quarterly branch, no local repository:
        // nothing to write.
        let s = Scratch::new("repos");
        let profile = Profile::default();
        let m = MockRunner::new();
        real_builder(&m, &profile, &s.0)
            .install_repositories()
            .unwrap();
        assert!(!s.0.join("work/rootfs").exists());

        // `latest` is written as an override of /etc/pkg/FreeBSD.conf.
        let mut profile = Profile::default();
        profile.packages.repo = "latest".into();
        profile.packages.local_repo = Some("cosmic/repo".into());
        let b = real_builder(&m, &profile, &s.0);
        b.install_repositories().unwrap();
        let conf = std::fs::read_to_string(
            s.0.join("work/rootfs")
                .join(dom0::FREEBSD_REPO_CONF.trim_start_matches('/')),
        )
        .unwrap();
        assert!(conf.contains("/latest\""));
        // No catalogue in the local repository yet: skipped with a warning.
        assert!(!s
            .0
            .join("work/rootfs")
            .join(dom0::COSMIC_REPO_CONF.trim_start_matches('/'))
            .exists());

        // Stage 2: no build VM output, but the running dom0 was installed
        // with the repository; it is handed on.
        let installed =
            s.0.join("root")
                .join(dom0::COSMIC_REPO_DIR.trim_start_matches('/'));
        s.put(
            "root/usr/local/share/juanitos/pkg/packagesite.pkg",
            b"catalogue",
        );
        s.put(
            "root/usr/local/share/juanitos/pkg/cosmic-term-1.10.0.pkg",
            b"pkg",
        );
        assert!(installed.join("packagesite.pkg").is_file());
        b.install_repositories().unwrap();
        let root = s.0.join("work/rootfs");
        let shipped = root.join(dom0::COSMIC_REPO_DIR.trim_start_matches('/'));
        assert!(shipped.join("cosmic-term-1.10.0.pkg").is_file());

        // With a catalogue in the profile's directory, that one wins.
        s.put("repo/cosmic/repo/packagesite.pkg", b"catalogue");
        s.put("repo/cosmic/repo/cosmic-comp-1.10.0.pkg", b"pkg");
        b.install_repositories().unwrap();
        assert!(shipped.join("cosmic-comp-1.10.0.pkg").is_file());
        let conf =
            std::fs::read_to_string(root.join(dom0::COSMIC_REPO_CONF.trim_start_matches('/')))
                .unwrap();
        assert!(conf.contains("file:///usr/local/share/juanitos/pkg"));

        // A bad branch is refused before anything runs.
        profile.packages.repo = "nightly".into();
        assert!(real_builder(&m, &profile, &s.0)
            .install_repositories()
            .is_err());
    }

    #[test]
    fn install_needs_the_release_binaries() {
        let s = Scratch::new("nobins");
        let profile = Profile::default();
        let m = MockRunner::new();
        let err = real_builder(&m, &profile, &s.0)
            .install_juanitos()
            .unwrap_err();
        assert!(err.to_string().contains("run `jos-release bins`"), "{err}");
        assert!(!s.0.join("work/rootfs").exists(), "nothing written");
    }

    #[test]
    fn source_tree_and_vendoring_follow_the_profile() {
        let run = |include: bool, vendor: bool| {
            let profile = Profile {
                source: Source { include, vendor },
                ..Profile::default()
            };
            let m = MockRunner::new();
            let b = Builder {
                fs: FsOps::new(true),
                firewall_image: Some(PathBuf::from("/fw.img")),
                ..real_builder(&m, &profile, Path::new("/x"))
            };
            b.install_juanitos().unwrap();
            m.calls()
                .iter()
                .filter(|c| c.starts_with("cargo vendor"))
                .count()
        };
        assert_eq!(run(true, true), 1);
        assert_eq!(run(true, false), 0);
        assert_eq!(run(false, true), 0);
    }

    /// Found by the stage-2 rebuild on the nested dom0: its source tree had
    /// `vendor` as a symbolic link and the UEFI crates' build output, and
    /// both ended up in the new root (the link followed by `cargo vendor`).
    #[cfg(unix)]
    #[test]
    fn the_source_copy_leaves_build_output_and_old_vendoring_behind() {
        let s = Scratch::new("source");
        s.put("repo/Cargo.toml", b"[workspace]\n");
        s.put("repo/.cargo/config.toml", b"[alias]\nxtask = \"run\"\n");
        s.put("repo/crates/jos-boot/src/main.rs", b"fn main() {}\n");
        s.put("repo/crates/jos-boot/target/big.efi", b"x");
        s.put("repo/crates/jos-firewall/target/big.efi", b"x");
        s.put("repo/target/release/jos", b"x");
        s.put("repo/work/dom0.qcow2", b"x");
        s.put("elsewhere/vendor/adler2/Cargo.toml", b"x");
        std::os::unix::fs::symlink(s.0.join("elsewhere/vendor"), s.0.join("repo/vendor")).unwrap();

        let profile = Profile::default();
        let m = MockRunner::new();
        let b = real_builder(&m, &profile, &s.0);
        b.install_source().unwrap();
        let dst = b.rootfs().join("usr/local/src/juanitos");
        assert!(dst.join("crates/jos-boot/src/main.rs").is_file());
        for gone in [
            "crates/jos-boot/target",
            "crates/jos-firewall/target",
            "target",
            "work",
            "vendor",
        ] {
            assert!(
                std::fs::symlink_metadata(dst.join(gone)).is_err(),
                "{gone} copied"
            );
        }
        assert_eq!(
            m.calls(),
            vec![format!(
                "cargo vendor --manifest-path {0}/Cargo.toml {0}/vendor",
                unix(&dst)
            )]
        );
        let config = std::fs::read_to_string(dst.join(".cargo/config.toml")).unwrap();
        assert!(config.starts_with("[alias]") && config.contains("vendored-sources"));

        // Without `cargo vendor` the existing vendor/ is all there is: keep it.
        assert!(!source_excludes(true).iter().all(|e| e != "vendor"));
        assert!(source_excludes(false).iter().all(|e| e != "vendor"));
    }

    #[test]
    fn pack_writes_the_tarball_manifest() {
        let s = Scratch::new("pack");
        let profile = Profile::default();
        let m = MockRunner::new();
        let b = real_builder(&m, &profile, &s.0);
        let tarball = s.put("work/juanitos-dom0.txz", b"not really xz");
        b.pack().unwrap();
        let manifest = std::fs::read_to_string(b.manifest()).unwrap();
        assert!(
            manifest.contains(&sha256_file(&tarball).unwrap()),
            "{manifest}"
        );
        assert!(manifest.contains(dom0::DIST_TARBALL));
        assert!(m.calls()[0].starts_with("tar -C "));
    }

    #[test]
    fn iso_build_records_the_checksum() {
        let s = Scratch::new("iso");
        let profile = Profile::default();
        let m = MockRunner::new();
        let b = real_builder(&m, &profile, &s.0);
        s.put("work/juanitos-dom0.txz", b"tarball");
        s.put("work/MANIFEST", b"manifest");
        s.put(
            &format!("work/rootfs/{}", dom0::LOADER_EFI.trim_start_matches('/')),
            b"loader",
        );
        // makefs is mocked: stand in for the image it would write.
        let iso = s.put(&format!("work/{}", profile.iso_name()), b"iso9660");
        b.iso_build().unwrap();

        let sum = std::fs::read_to_string(iso.with_extension("iso.sha256")).unwrap();
        assert_eq!(
            sum,
            format!("{}  {}\n", sha256_file(&iso).unwrap(), profile.iso_name())
        );
        let root = s.0.join("work/rootfs");
        let dist = root.join(dom0::DIST_DIR.trim_start_matches('/'));
        assert!(dist.join(dom0::DIST_TARBALL).is_file());
        assert!(root.join("etc/rc.local").is_file());
        // No jos-boot.efi anywhere: the ESP boots FreeBSD's loader.
        let esp = s.0.join("work/efiboot");
        assert_eq!(
            std::fs::read(esp.join(dom0::ESP_DEFAULT)).unwrap(),
            b"loader"
        );
        // No boot/cdboot in the root: UEFI only.
        let cd = m
            .calls()
            .into_iter()
            .find(|c| c.contains("cd9660"))
            .unwrap();
        assert!(!cd.contains("cdboot"), "{cd}");
        // No boot/pmbr either: no hybrid boot area.
        assert!(!m.calls().iter().any(|c| c.starts_with("mkimg")));
    }

    #[test]
    fn the_live_iso_ships_geom_ventoy_when_it_builds() {
        let s = Scratch::new("ventoy");
        let profile = Profile::default();
        s.put("work/juanitos-dom0.txz", b"tarball");
        s.put("work/MANIFEST", b"manifest");
        s.put(&format!("work/{}", profile.iso_name()), b"iso9660");
        s.put(&format!("work/rootfs{}", dom0::LOADER_EFI), b"loader");
        s.put("repo/freebsd/geom_ventoy/Makefile", b"KMOD=geom_ventoy\n");
        s.put("root/usr/src/sys/conf/kmod.mk", b"");
        // `make` is mocked: the module it would build.
        s.put(
            "repo/freebsd/geom_ventoy/geom_ventoy.ko",
            &crate::ventoy::tests::fake_module(0x10000, 65536, true),
        );
        let m = MockRunner::new();
        let b = real_builder(&m, &profile, &s.0);
        b.iso_build().unwrap();

        let root = s.0.join("work/rootfs");
        let conf = std::fs::read_to_string(root.join("boot/loader.conf")).unwrap();
        assert!(conf.contains("geom_ventoy_load=\"YES\""), "{conf}");
        let mut shipped = std::fs::read(root.join("boot/kernel/geom_ventoy.ko")).unwrap();
        let before = shipped.clone();
        // Already patched to a page alignment: a second patch changes nothing.
        crate::ventoy::patch_module(&mut shipped).unwrap();
        assert_eq!(shipped, before);
        let calls = m.calls();
        assert!(calls.contains(&format!(
            "make -C {} SYSDIR={}",
            unix(&s.0.join("work/geom_ventoy")),
            unix(&s.0.join("root/usr/src/sys"))
        )));
        assert!(calls.iter().any(|c| c.starts_with("kldxref ")));
        // The installed system does not need it: the tarball was packed before.
        assert!(!calls.iter().any(|c| c.starts_with("tar ")));

        // A module Ventoy could not fill is not shipped.
        s.put(
            "repo/freebsd/geom_ventoy/geom_ventoy.ko",
            &crate::ventoy::tests::fake_module(0x11000, 4096, true),
        );
        std::fs::remove_file(root.join("boot/kernel/geom_ventoy.ko")).unwrap();
        b.iso_build().unwrap();
        let conf = std::fs::read_to_string(root.join("boot/loader.conf")).unwrap();
        assert!(!conf.contains("geom_ventoy"), "{conf}");
        assert!(!root.join("boot/kernel/geom_ventoy.ko").exists());

        // Neither does a failed build stop the ISO.
        let m = MockRunner::new().respond("make", &[], CmdOutput::failed(2, "cc: error"));
        let b = real_builder(&m, &profile, &s.0);
        b.iso_build().unwrap();
        let conf = std::fs::read_to_string(root.join("boot/loader.conf")).unwrap();
        assert!(!conf.contains("geom_ventoy"), "{conf}");
    }

    #[test]
    fn kernel_sources_come_from_the_verified_src_set() {
        let s = Scratch::new("ksrc");
        let profile = Profile::default();
        let m = MockRunner::new();
        let b = real_builder(&m, &profile, &s.0);
        let src = s.put("work/dist/src.txz", b"sources");
        let entry = format!(
            "src.txz\t{}\t1\tsrc\t\"Source\"\toff\n",
            sha256_file(&src).unwrap()
        );
        s.put("work/dist/MANIFEST", entry.as_bytes());
        let sys = b.kernel_sources().unwrap();
        assert_eq!(sys, s.0.join("work/src/usr/src/sys"));
        assert_eq!(
            m.calls(),
            vec![format!(
                "tar -xf {} -C {} usr/src/sys",
                unix(&src),
                unix(&s.0.join("work/src"))
            )]
        );

        // Extracted already: no second tar.
        s.put("work/src/usr/src/sys/conf/kmod.mk", b"");
        let m = MockRunner::new();
        let b = real_builder(&m, &profile, &s.0);
        b.kernel_sources().unwrap();
        assert!(m.calls().is_empty());

        std::fs::write(&src, b"tampered").unwrap();
        assert!(b.kernel_sources().is_err());
        s.put("work/dist/MANIFEST", b"");
        let err = b.kernel_sources().unwrap_err();
        assert!(err.to_string().contains("src.txz not listed"), "{err}");

        // Missing set: MANIFEST and src.txz are downloaded (here they
        // fail, as nothing is really fetched).
        std::fs::remove_dir_all(s.0.join("work/dist")).unwrap();
        let m = MockRunner::new();
        let b = real_builder(&m, &profile, &s.0);
        assert!(b.kernel_sources().is_err());
        let calls = m.calls();
        assert!(calls[0].ends_with("/15.1-RELEASE/MANIFEST"), "{calls:?}");
        assert!(calls[1].ends_with("/15.1-RELEASE/src.txz"), "{calls:?}");
    }

    #[test]
    fn iso_build_makes_a_hybrid_usb_image() {
        let s = Scratch::new("hybrid");
        let profile = Profile::default();
        let etdump = "et_platform=default;et_system=i386;et_lba=1044;et_sectors=4\n\
                      et_platform=efi;et_system=i386;et_lba=20;et_sectors=16384\n";
        let m = MockRunner::new().respond("etdump", &[], CmdOutput::ok(etdump));
        let b = real_builder(&m, &profile, &s.0);
        s.put("work/juanitos-dom0.txz", b"tarball");
        s.put("work/MANIFEST", b"manifest");
        for f in [dom0::LOADER_EFI, dom0::LOADER_IA32_EFI, dom0::PMBR] {
            s.put(&format!("work/rootfs{f}"), b"x");
        }
        s.put("work/rootfs/boot/cdboot", b"x");
        s.put("work/rootfs/boot/isoboot", b"x");
        let iso = s.put(&format!("work/{}", profile.iso_name()), &[0u8; 4096]);
        b.iso_build().unwrap();

        assert!(s.0.join("work/efiboot").join(dom0::ESP_IA32).is_file());
        let calls = m.calls();
        let cd = calls.iter().position(|c| c.contains("cd9660")).unwrap();
        assert!(calls[cd].contains("cdboot"));
        let mkimg = calls.iter().position(|c| c.starts_with("mkimg")).unwrap();
        let w = s.0.join("work");
        assert_eq!(
            calls[mkimg],
            format!(
                "mkimg -s gpt --capacity 4096 -b {r}/boot/pmbr -p freebsd-boot:={r}/boot/isoboot \
                 -p efi::8388608:40960 -o {w}/hybrid.img",
                r = unix(&w.join("rootfs")),
                w = unix(&w)
            )
        );
        assert!(cd < mkimg);
        assert_eq!(
            calls[mkimg + 1],
            format!(
                "dd if={}/hybrid.img of={} bs=32k count=1 conv=notrunc",
                unix(&w),
                unix(&iso)
            )
        );

        // makefs that wrote no EFI entry is an error, not a silent CD-only ISO.
        let m = MockRunner::new();
        let b = real_builder(&m, &profile, &s.0);
        let err = b.iso_build().unwrap_err().to_string();
        assert!(err.contains("no EFI El Torito entry"), "{err}");
    }

    #[test]
    fn etdump_and_mkimg_arguments() {
        assert_eq!(
            etdump_efi_entry("et_platform=efi;et_system=i386;et_lba=20;et_sectors=4096\n"),
            Some((20, 4096))
        );
        assert_eq!(
            etdump_efi_entry("et_platform=default;et_lba=1;et_sectors=4\n"),
            None
        );
        assert_eq!(
            etdump_efi_entry("et_platform=efi;et_lba=x;et_sectors=4\n"),
            None
        );
        assert_eq!(etdump_efi_entry(""), None);
        let uefi_only = hybrid_args(
            Path::new("/r/boot/pmbr"),
            None,
            (20, 4096),
            1 << 20,
            Path::new("/w/h.img"),
        )
        .join(" ");
        assert_eq!(
            uefi_only,
            "-s gpt --capacity 1048576 -b /r/boot/pmbr -p efi::2097152:40960 -o /w/h.img"
        );
    }

    #[test]
    fn a_dom0_rebuilding_itself_is_the_next_stage() {
        let s = Scratch::new("stage");
        let profile = Profile::default();
        let m = MockRunner::new().respond("git", &[], CmdOutput::ok("f826e4e\n"));
        let b = real_builder(&m, &profile, &s.0);
        let first = b.release_info().unwrap();
        assert_eq!((first.built_on.as_str(), first.stage), ("freebsd", 1));
        assert_eq!(first.git_rev.as_deref(), Some("f826e4e"));

        s.put(
            &format!("root/{}", dom0::RELEASE_INFO_PATH.trim_start_matches('/')),
            first.to_toml().unwrap().as_bytes(),
        );
        let second = b.release_info().unwrap();
        assert_eq!((second.built_on.as_str(), second.stage), ("juanitos", 2));

        // Not a git checkout (a release tarball): no revision recorded.
        let m =
            MockRunner::new().respond("git", &[], CmdOutput::failed(128, "not a git repository"));
        let b = real_builder(&m, &profile, &s.0);
        assert_eq!(b.release_info().unwrap().git_rev, None);
    }
}
