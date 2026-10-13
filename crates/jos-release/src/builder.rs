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
    fn boot_efi(&self) -> Option<PathBuf> {
        self.boot_efi.clone().or_else(|| {
            let p = self.uefi_release_dir().join("jos-boot.efi");
            p.is_file().then_some(p)
        })
    }
    fn firewall_image(&self) -> Option<PathBuf> {
        self.firewall_image.clone().or_else(|| {
            let p = self.firewall_image_path();
            p.is_file().then_some(p)
        })
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

    fn install_packages(&self) -> anyhow::Result<()> {
        let root = self.rootfs();
        let root_s = Self::s(&root);
        let (required, optional) = self.profile.package_sets();
        self.step(&format!(
            "Installing packages ({} required, {} best-effort)",
            required.len(),
            optional.len()
        ));

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
        self.fs
            .copy_tree(&self.repo, &dst, &[".git", "target", "release/out"])?;
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
        let (built_on, stage) = match std::fs::read_to_string(dom0::RELEASE_INFO_PATH)
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
        };
        self.fs
            .write(&root.join("etc/fstab"), &dom0::live_fstab(&p.label))?;
        self.fs.write(
            &root.join("etc/rc.conf"),
            &dom0::rc_conf(Flavor::Live, &opts),
        )?;
        self.fs.write(
            &root.join("boot/loader.conf"),
            &dom0::loader_conf(Flavor::Live, &opts),
        )?;
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
        for (src, dst) in dom0::esp_files(has_jos_boot) {
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

        let args = iso_args(p, &root, &esp_img, &iso, root.join("boot/cdboot").is_file());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run_checked("makefs", &args)?;

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

/// `.cargo/config.toml` of the installed source tree: the repository's
/// own settings (the `cargo xtask` alias) plus the vendored crates as
/// the source for crates.io, so stage 2 builds offline.
pub fn vendored_cargo_config(repo_config: &str) -> String {
    let mut out = repo_config.trim_end().to_string();
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
        };
        let info = b.release_info().unwrap();
        // The test host is not a JuanitOS dom0.
        assert_eq!(info.built_on, "freebsd");
        assert_eq!(info.stage, 1);
        assert!(info.packages.contains(&"seatd".to_string()));
    }
}
