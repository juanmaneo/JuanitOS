//! Guest templates built from upstream cloud images.
//!
//! QubesOS ships TemplateVMs; JuanitOS builds them on the host from the
//! distributions' own signed-off cloud images, so nothing opaque is
//! downloaded from us:
//!
//! 1. fetch the image and the distribution's checksum list into the cache,
//!    and verify the image (SHA-256 / SHA-512, in Rust);
//! 2. define the template qube and create its zvol;
//! 3. stream the image onto the zvol (`xz`/`tar` → `dd conv=sparse`);
//! 4. build a NoCloud seed ISO (`cidata`: `user-data` from
//!    `templates/<recipe>/user-data`, plus `meta-data`);
//! 5. boot the template once with the seed attached: cloud-init (Linux) or
//!    nuageinit (FreeBSD) installs the toolchain and powers off;
//! 6. snapshot `@base`. `jos create <qube> --template <name>` clones it.
//!
//! Recipes without `user_data` (Redox, which has no cloud-init) skip steps
//! 4 and 5: the image is the template. Recipes whose image name changes
//! with every build (Redox nightlies) use a `*` wildcard in `image_url`;
//! the checksum list, fetched first, resolves it.
//!
//! Each step goes through [`Runner`], so the whole pipeline is unit-tested
//! with mocked commands and visible with `--dry-run`.

use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256, Sha512};

use crate::bhyve::Vm;
use crate::config::{GlobalConfig, OsType, Qube, QubeKind, QubeStore};
use crate::error::{Error, Result};
use crate::fsops::FsOps;
use crate::sys::{shell_quote, Runner};
use crate::zfs::Storage;

/// Checksum algorithm of a distribution's checksum list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Digest {
    Sha256,
    Sha512,
}

/// How the disk image is packed in the download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packing {
    /// `xz`-compressed raw image (FreeBSD's `*.raw.xz`).
    Xz,
    /// A `.tar.xz` holding a raw image under this name (Debian's
    /// `disk.raw`).
    TarXz { member: &'static str },
    /// `zstd`-compressed raw image (Redox's `*.img.zst`).
    Zst,
}

/// A template that `jos template install` knows how to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    /// Default name of the template qube.
    pub name: &'static str,
    pub summary: &'static str,
    pub os: OsType,
    /// URL of the image. A single `*` in the file name is resolved
    /// against the checksum list (for builds whose names carry a date).
    pub image_url: &'static str,
    /// The distribution's checksum list covering `image_url`.
    pub checksums_url: &'static str,
    pub digest: Digest,
    pub packing: Packing,
    pub disk_gb: u64,
    pub cpus: u32,
    pub memory_mb: u64,
    /// cloud-init / nuageinit `#cloud-config` applied on the first boot;
    /// `None` for guests without cloud-init (no provisioning boot).
    pub user_data: Option<&'static str>,
}

pub const RECIPES: &[Recipe] = &[
    Recipe {
        name: "debian-13-dev",
        summary: "Debian 13 (trixie) for development, with Docker, Compose and Buildx",
        os: OsType::Linux,
        image_url: "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.tar.xz",
        checksums_url: "https://cloud.debian.org/images/cloud/trixie/latest/SHA512SUMS",
        digest: Digest::Sha512,
        packing: Packing::TarXz { member: "disk.raw" },
        disk_gb: 40,
        cpus: 4,
        memory_mb: 4096,
        user_data: Some(include_str!("../../../templates/debian-13-dev/user-data")),
    },
    Recipe {
        name: "fbsd15-dev",
        summary: "FreeBSD 15.1 for development, with Podman (ocijail) and Buildah",
        os: OsType::Freebsd,
        image_url: "https://download.freebsd.org/releases/VM-IMAGES/15.1-RELEASE/amd64/Latest/FreeBSD-15.1-RELEASE-amd64-BASIC-CLOUDINIT-zfs.raw.xz",
        checksums_url: "https://download.freebsd.org/releases/VM-IMAGES/15.1-RELEASE/amd64/Latest/CHECKSUM.SHA256",
        digest: Digest::Sha256,
        packing: Packing::Xz,
        disk_gb: 40,
        cpus: 4,
        memory_mb: 4096,
        user_data: Some(include_str!("../../../templates/fbsd15-dev/user-data")),
    },
    Recipe {
        name: "fbsd15-net",
        summary: "FreeBSD 15.1 as sys-net: DHCP client upstream, dnsmasq DHCP/DNS and pf NAT downstream",
        os: OsType::Freebsd,
        image_url: "https://download.freebsd.org/releases/VM-IMAGES/15.1-RELEASE/amd64/Latest/FreeBSD-15.1-RELEASE-amd64-BASIC-CLOUDINIT-zfs.raw.xz",
        checksums_url: "https://download.freebsd.org/releases/VM-IMAGES/15.1-RELEASE/amd64/Latest/CHECKSUM.SHA256",
        digest: Digest::Sha256,
        packing: Packing::Xz,
        disk_gb: 10,
        cpus: 1,
        memory_mb: 1024,
        user_data: Some(include_str!("../../../templates/fbsd15-net/user-data")),
    },
    Recipe {
        name: "redox",
        summary: "Redox OS server nightly (Relibc userland); experimental, see docs/relibc-cosmic.md",
        os: OsType::Redox,
        image_url: "https://static.redox-os.org/img/x86_64/redox_server_x86_64_*_harddrive.img.zst",
        checksums_url: "https://static.redox-os.org/img/x86_64/SHA256SUM",
        digest: Digest::Sha256,
        packing: Packing::Zst,
        disk_gb: 4,
        cpus: 2,
        memory_mb: 2048,
        user_data: None,
    },
];

pub fn find(name: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|r| r.name == name)
}

impl Recipe {
    /// File name of the download (last URL segment), possibly a `*`
    /// pattern; see [`Recipe::resolve_image_file`].
    pub fn image_file(&self) -> &'static str {
        self.image_url.rsplit('/').next().unwrap_or(self.image_url)
    }

    /// The actual file name: the pattern itself when it has no wildcard,
    /// else the (single) entry of the checksum list matching it.
    pub fn resolve_image_file(&self, checksum_list: &str) -> Result<String> {
        let pattern = self.image_file();
        if !pattern.contains('*') {
            return Ok(pattern.to_string());
        }
        let mut matches: Vec<String> = checksum_list
            .lines()
            .filter_map(|l| checksum_entry(l).map(|(_, name)| name.to_string()))
            .filter(|name| glob_match(pattern, name))
            .collect();
        matches.sort();
        matches.dedup();
        match matches.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(Error::Other(format!(
                "nothing matching {pattern} in {}",
                self.checksums_url
            ))),
            many => Err(Error::Other(format!(
                "{pattern} is ambiguous in {}: {}",
                self.checksums_url,
                many.join(", ")
            ))),
        }
    }

    /// `image_url` with the wildcard replaced by the resolved file name.
    pub fn image_url_for(&self, file: &str) -> String {
        match self.image_url.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{file}"),
            None => file.to_string(),
        }
    }

    /// True when the first boot provisions the template with cloud-init.
    pub fn provisions(&self) -> bool {
        self.user_data.is_some()
    }

    /// The kind of qube this template is meant to be cloned into.
    pub fn kind(&self) -> QubeKind {
        if self.name.ends_with("-net") {
            QubeKind::Net
        } else {
            QubeKind::App
        }
    }

    /// What to type next once template `name` is ready.
    pub fn clone_hint(&self, name: &str) -> String {
        match self.kind() {
            QubeKind::Net => format!(
                "Create the network qube from it with\n  \
                 jos create sys-net --kind net --os {} --template {name} --pci B/S/F --autostart\n  \
                 (or --uplink IFACE instead of --pci on a machine without IOMMU)",
                self.os
            ),
            _ => format!(
                "Create qubes from it with\n  jos create work --kind app --os {} --template {name}",
                self.os
            ),
        }
    }

    /// Shell pipeline writing the unpacked image onto `device`.
    pub fn write_pipeline(&self, image: &Path, device: &str) -> String {
        let img = shell_quote(&image.to_string_lossy());
        let unpack = match self.packing {
            Packing::Xz => format!("xz -dc {img}"),
            Packing::TarXz { member } => format!("tar -xOf {img} {}", shell_quote(member)),
            Packing::Zst => format!("zstd -dc {img}"),
        };
        format!(
            "{unpack} | dd of={} bs=1m conv=sparse status=progress",
            shell_quote(device)
        )
    }
}

/// The expected digest of `file` in a checksum list: GNU style
/// (`<hex>  <file>`, Debian) or BSD style (`SHA256 (<file>) = <hex>`,
/// FreeBSD).
pub fn find_checksum(list: &str, file: &str) -> Option<String> {
    list.lines()
        .filter_map(checksum_entry)
        .find(|(_, name)| *name == file)
        .map(|(hex, _)| hex)
}

/// One line of a checksum list as (lowercase hex, file name).
fn checksum_entry(line: &str) -> Option<(String, &str)> {
    let line = line.trim();
    if let Some((head, hex)) = line.split_once(") = ") {
        let (_, name) = head.split_once(" (")?;
        return Some((hex.trim().to_ascii_lowercase(), name));
    }
    let mut parts = line.split_whitespace();
    let hex = parts.next()?;
    let name = parts.next()?.trim_start_matches('*');
    hex.chars()
        .all(|c| c.is_ascii_hexdigit())
        .then(|| (hex.to_ascii_lowercase(), name))
}

/// `pattern` with at most one `*` (matching anything) against `name`.
fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => {
            name.len() >= head.len() + tail.len() && name.starts_with(head) && name.ends_with(tail)
        }
    }
}

/// Hex digest of a file.
pub fn digest_file(path: &Path, digest: Digest) -> Result<String> {
    use std::io::Read;
    fn run<D: sha2::Digest>(mut hasher: D, path: &Path) -> Result<String> {
        let mut file = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut buf)?;
            match buf.get(..n) {
                Some(chunk) if n > 0 => hasher.update(chunk),
                _ => break,
            }
        }
        Ok(hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }
    match digest {
        Digest::Sha256 => run(Sha256::new(), path),
        Digest::Sha512 => run(Sha512::new(), path),
    }
}

/// NoCloud `meta-data`: a fresh instance id makes cloud-init treat the
/// boot as a first boot.
pub fn meta_data(name: &str) -> String {
    format!("instance-id: juanitos-{name}\nlocal-hostname: {name}\n")
}

/// `user-data` with the hostname set to the template's name (recipes are
/// written for their default name).
pub fn user_data_for(recipe: &Recipe, name: &str) -> String {
    recipe
        .user_data
        .unwrap_or_default()
        .lines()
        .map(|l| {
            if l.starts_with("hostname: ") {
                format!("hostname: {name}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Options of `jos template install`.
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Template qube name (default: the recipe's).
    pub name: Option<String>,
    pub disk_gb: Option<u64>,
    /// Stop after writing the image and the seed (no provisioning boot).
    pub no_boot: bool,
    /// Provision through this host interface instead of the NetVM.
    pub uplink: Option<String>,
    /// A local copy of the image, verified against `checksums` (no
    /// download).
    pub image: Option<PathBuf>,
    pub checksums: Option<PathBuf>,
}

pub struct TemplateInstaller<'a> {
    pub runner: &'a dyn Runner,
    pub cfg: &'a GlobalConfig,
    pub store: &'a QubeStore,
    pub dry_run: bool,
}

impl<'a> TemplateInstaller<'a> {
    fn step(&self, msg: &str) {
        println!("==> {msg}");
    }

    fn images_dir(&self) -> PathBuf {
        Path::new(&self.cfg.cache_dir).join("images")
    }

    /// Path of the seed ISO for template `name`.
    pub fn seed_path(&self, name: &str) -> PathBuf {
        Path::new(&self.cfg.run_dir).join(format!("{name}-seed.iso"))
    }

    /// Download (when not cached) and verify the recipe's image. With
    /// `local` (image, checksum list) nothing is downloaded.
    pub fn fetch(&self, r: &Recipe, local: Option<(&Path, &Path)>) -> Result<PathBuf> {
        let (image, sums, file) = match local {
            Some((image, sums)) => {
                self.step(&format!("Using {}", image.display()));
                let list = std::fs::read_to_string(sums)?;
                let file = r.resolve_image_file(&list)?;
                (image.to_path_buf(), sums.to_path_buf(), file)
            }
            None => {
                let dir = self.images_dir();
                FsOps::new(self.dry_run).mkdir_p(&dir)?;
                let sums = dir.join(format!("{}.checksums", r.name));
                self.step(&format!("Fetching {}", r.image_url));
                self.runner
                    .run_checked("fetch", &["-o", &sums.to_string_lossy(), r.checksums_url])?;
                let file = if self.dry_run && !sums.is_file() {
                    r.image_file().to_string()
                } else {
                    r.resolve_image_file(&std::fs::read_to_string(&sums)?)?
                };
                let image = dir.join(&file);
                if image.is_file() {
                    println!("   {}: cached", image.display());
                } else {
                    self.runner.run_checked(
                        "fetch",
                        &["-o", &image.to_string_lossy(), &r.image_url_for(&file)],
                    )?;
                }
                if self.dry_run && !image.is_file() {
                    println!("   (verification skipped in dry run)");
                    return Ok(image);
                }
                (image, sums, file)
            }
        };
        let list = std::fs::read_to_string(&sums)?;
        let want = find_checksum(&list, &file)
            .ok_or_else(|| Error::Other(format!("{file} is not listed in {}", r.checksums_url)))?;
        let got = digest_file(&image, r.digest)?;
        if got != want {
            // A corrupt download must not stay in the cache.
            let _ = std::fs::remove_file(&image);
            return Err(Error::Other(format!(
                "checksum mismatch for {} (expected {want}, got {got}); removed it, try again",
                image.display()
            )));
        }
        println!("   {file}: {:?} OK", r.digest);
        Ok(image)
    }

    /// Write `user-data` and `meta-data` and pack them as a `cidata` ISO.
    pub fn build_seed(&self, r: &Recipe, name: &str) -> Result<PathBuf> {
        self.step("Building the cloud-init seed (NoCloud cidata)");
        let fs = FsOps::new(self.dry_run);
        let dir = Path::new(&self.cfg.run_dir).join(format!("{name}-seed"));
        fs.mkdir_p(&dir)?;
        fs.write(&dir.join("user-data"), &user_data_for(r, name))?;
        fs.write(&dir.join("meta-data"), &meta_data(name))?;
        let iso = self.seed_path(name);
        self.runner.run_checked(
            "makefs",
            &[
                "-t",
                "cd9660",
                "-o",
                "rockridge",
                "-o",
                "label=cidata",
                &iso.to_string_lossy(),
                &dir.to_string_lossy(),
            ],
        )?;
        Ok(iso)
    }

    /// The whole pipeline (module docs). Returns the template qube.
    pub fn install(&self, r: &Recipe, opts: &InstallOptions) -> Result<Qube> {
        let name = opts.name.clone().unwrap_or_else(|| r.name.to_string());
        crate::config::validate_name(&name)?;
        if self.store.exists(&name) {
            return Err(Error::QubeExists(name));
        }
        let mut q = Qube::new(&name, QubeKind::Template, r.os);
        q.disk_gb = opts.disk_gb.unwrap_or(r.disk_gb);
        q.cpus = r.cpus;
        q.memory_mb = r.memory_mb;
        q.label = Some(format!("recipe:{}", r.name));
        q.uplink = opts.uplink.clone();
        let existing = self.store.list()?;
        crate::firewall::assign_address(&mut q, &existing, self.cfg.default_netvm.as_deref())?;

        let local = match (&opts.image, &opts.checksums) {
            (Some(i), Some(c)) => Some((i.as_path(), c.as_path())),
            (Some(_), None) => {
                return Err(Error::Other("--image needs --checksums".into()));
            }
            _ => None,
        };
        let image = self.fetch(r, local)?;

        self.step(&format!(
            "Creating template {name} ({} GiB zvol)",
            q.disk_gb
        ));
        Storage::new(self.runner, self.cfg).provision(&q, |_| None)?;
        self.store.save(&q)?;

        self.step(&format!(
            "Writing {} to the zvol",
            image.file_name().unwrap_or_default().to_string_lossy()
        ));
        let device = self.cfg.zvol_path(&q);
        self.runner
            .run_checked("sh", &["-c", &r.write_pipeline(&image, &device)])?;

        if !r.provisions() {
            // No cloud-init in the guest: the image is the template.
            let snap = Storage::new(self.runner, self.cfg).snapshot_template(&name, "base")?;
            self.step(&format!("Template ready: {snap}"));
            println!("\n{}", r.clone_hint(&name));
            return Ok(q);
        }
        let seed = self.build_seed(r, &name)?;
        let seed_s = seed.to_string_lossy().into_owned();
        if opts.no_boot {
            println!(
                "\n{name}: image written, not provisioned (--no-boot). Provision it with\n  \
                 jos template run {name} --seed {seed_s}"
            );
            return Ok(q);
        }
        self.provision(&q, &seed_s)?;
        Ok(q)
    }

    /// First boot with the seed, then `@base`.
    pub fn provision(&self, q: &Qube, seed: &str) -> Result<()> {
        self.step(&format!(
            "Provisioning {} (first boot; follow it with `jos console {}`)",
            q.name, q.name
        ));
        let vm = Vm::new(self.runner, self.cfg);
        if self.dry_run {
            println!("+ {}", vm.maintenance_command(q, Some(seed))?.to_shell());
        } else {
            vm.maintain(q, Some(seed))?;
        }
        let fs = FsOps::new(self.dry_run);
        fs.remove_file(Path::new(seed))?;
        fs.remove_tree(&Path::new(&self.cfg.run_dir).join(format!("{}-seed", q.name)))?;
        let snap = Storage::new(self.runner, self.cfg).snapshot_template(&q.name, "base")?;
        self.step(&format!("Template ready: {snap}"));
        let hint = match q.label.as_deref().and_then(|l| l.strip_prefix("recipe:")) {
            Some(recipe) => find(recipe).map(|r| r.clone_hint(&q.name)),
            None => None,
        };
        println!(
            "\n{}",
            hint.unwrap_or_else(|| format!(
                "Create qubes from it with\n  jos create work --kind app --os {} --template {}",
                q.os, q.name
            ))
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{CmdOutput, MockRunner};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jos-template-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg(dir: &Path) -> GlobalConfig {
        GlobalConfig {
            cache_dir: dir.join("cache").to_string_lossy().into_owned(),
            run_dir: dir.join("run").to_string_lossy().into_owned(),
            config_dir: dir.join("etc").to_string_lossy().into_owned(),
            ..GlobalConfig::default()
        }
    }

    #[test]
    fn recipes_are_complete() {
        assert!(RECIPES.len() >= 3);
        let debian = find("debian-13-dev").unwrap();
        assert_eq!(debian.os, OsType::Linux);
        let ud = debian.user_data.unwrap();
        assert!(ud.starts_with("#cloud-config"));
        assert!(ud.contains("docker.io"));
        assert_eq!(debian.image_file(), "debian-13-genericcloud-amd64.tar.xz");
        let fbsd = find("fbsd15-dev").unwrap();
        assert_eq!(fbsd.os, OsType::Freebsd);
        assert!(fbsd.user_data.unwrap().contains("podman-suite"));
        assert!(fbsd.image_url.contains("BASIC-CLOUDINIT"));
        let redox = find("redox").unwrap();
        assert_eq!(redox.os, OsType::Redox);
        assert!(!redox.provisions());
        assert_eq!(redox.packing, Packing::Zst);
        let net = find("fbsd15-net").unwrap();
        let ud = net.user_data.unwrap();
        assert!(ud.contains("dnsmasq") && ud.contains("gateway_enable=YES"));
        assert!(ud.contains("path: /usr/local/etc/rc.d/josnet"));
        assert_eq!(net.image_url, fbsd.image_url, "same FreeBSD image");
        assert_eq!(net.kind(), QubeKind::Net);
        assert_eq!(fbsd.kind(), QubeKind::App);
        assert!(net
            .clone_hint("mynet")
            .contains("--kind net --os freebsd --template mynet"));
        assert!(redox
            .clone_hint("redox")
            .contains("--kind app --os redox --template redox"));
        assert!(find("nope").is_none());
        for r in RECIPES {
            crate::config::validate_name(r.name).unwrap();
            assert!(r.image_url.starts_with("https://"));
            assert!(r.checksums_url.starts_with("https://"));
            if let Some(ud) = r.user_data {
                assert!(ud.contains(&format!("hostname: {}", r.name)));
            }
        }
    }

    /// Every template whose OS can mount the dom0 share ships the guest
    /// agent from `templates/agent/`, verbatim (indented as cloud-config
    /// `write_files` content) and enabled.
    #[test]
    fn templates_embed_the_guest_agent() {
        fn embedded(user_data: &str, script: &str) -> bool {
            let block: String = script
                .lines()
                .map(|l| {
                    if l.is_empty() {
                        "\n".to_string()
                    } else {
                        format!("      {l}\n")
                    }
                })
                .collect();
            user_data.contains(&block)
        }
        let freebsd_agent = include_str!("../../../templates/agent/jos_agent");
        let linux_agent = include_str!("../../../templates/agent/jos-agent");
        let linux_unit = include_str!("../../../templates/agent/jos-agent.service");
        assert!(freebsd_agent.contains("mount -t p9fs jos /mnt/jos"));
        assert!(linux_agent.contains("mount -t 9p -o trans=virtio,version=9p2000.L jos /mnt/jos"));
        for r in RECIPES {
            let Some(ud) = r.user_data else {
                assert!(!r.os.has_share(), "{}", r.name);
                continue;
            };
            match r.os {
                OsType::Freebsd => {
                    assert!(
                        ud.contains("path: /usr/local/etc/rc.d/jos_agent"),
                        "{}",
                        r.name
                    );
                    assert!(embedded(ud, freebsd_agent), "{}: stale jos_agent", r.name);
                    assert!(ud.contains("jos_agent_enable=YES"), "{}", r.name);
                }
                OsType::Linux => {
                    assert!(ud.contains("path: /usr/local/sbin/jos-agent"), "{}", r.name);
                    assert!(embedded(ud, linux_agent), "{}: stale jos-agent", r.name);
                    assert!(embedded(ud, linux_unit), "{}: stale unit", r.name);
                    assert!(
                        ud.contains(
                            "enable, containerd.service, docker.service, jos-agent.service"
                        ),
                        "{}",
                        r.name
                    );
                }
                other => panic!("{}: unexpected os {other}", r.name),
            }
        }
    }

    #[test]
    fn wildcard_image_names_resolve_through_the_checksum_list() {
        let redox = find("redox").unwrap();
        assert_eq!(
            redox.image_file(),
            "redox_server_x86_64_*_harddrive.img.zst"
        );
        let list = "aa *redox_demo_x86_64_2026-10-13_552_harddrive.img.zst\n\
                    bb *redox_server_x86_64_2026-10-13_552_harddrive.img.zst\n\
                    cc *redox_server_x86_64_2026-10-13_552_livedisk.iso.zst\n";
        let file = redox.resolve_image_file(list).unwrap();
        assert_eq!(file, "redox_server_x86_64_2026-10-13_552_harddrive.img.zst");
        assert_eq!(
            redox.image_url_for(&file),
            "https://static.redox-os.org/img/x86_64/redox_server_x86_64_2026-10-13_552_harddrive.img.zst"
        );
        assert_eq!(find_checksum(list, &file).as_deref(), Some("bb"));
        let err = redox
            .resolve_image_file("aa *other\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing matching"), "{err}");
        let two = "aa *redox_server_x86_64_1_harddrive.img.zst\n\
                   bb *redox_server_x86_64_2_harddrive.img.zst\n";
        let err = redox.resolve_image_file(two).unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
        // Fixed names resolve to themselves, whatever the list says.
        let fbsd = find("fbsd15-dev").unwrap();
        assert_eq!(fbsd.resolve_image_file("").unwrap(), fbsd.image_file());
        assert_eq!(fbsd.image_url_for(fbsd.image_file()), fbsd.image_url);
        assert!(glob_match("a*b", "ab"));
        assert!(glob_match("a*b", "axyzb"));
        assert!(!glob_match("a*b", "b"));
        assert!(!glob_match("a*bc", "abc") || glob_match("a*bc", "abc"));
        assert!(!glob_match("abc", "abd"));
        assert!(!glob_match("ab*cd", "abd"));
    }

    #[test]
    fn checksum_lists_both_styles() {
        let gnu = "abc123  other.tar.xz\nDEADBEEF  debian-13-genericcloud-amd64.tar.xz\n";
        assert_eq!(
            find_checksum(gnu, "debian-13-genericcloud-amd64.tar.xz").as_deref(),
            Some("deadbeef")
        );
        let bsd = "SHA256 (a.raw.xz) = 11\nSHA256 (FreeBSD-x.raw.xz) = AB12\n";
        assert_eq!(
            find_checksum(bsd, "FreeBSD-x.raw.xz").as_deref(),
            Some("ab12")
        );
        assert_eq!(find_checksum(bsd, "missing"), None);
        assert_eq!(find_checksum("zz  f\n", "f"), None, "not hex");
        assert_eq!(find_checksum("00  *f\n", "f").as_deref(), Some("00"));
        assert_eq!(find_checksum("\n\nlonely\n", "f"), None);
    }

    #[test]
    fn digests() {
        let d = tmp("digest");
        let f = d.join("x");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            digest_file(&f, Digest::Sha256).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(digest_file(&f, Digest::Sha512)
            .unwrap()
            .starts_with("ddaf35a193617aba"));
        assert!(digest_file(&d.join("missing"), Digest::Sha256).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pipelines_and_cloud_init_files() {
        let debian = find("debian-13-dev").unwrap();
        assert_eq!(
            debian.write_pipeline(Path::new("/c/d.tar.xz"), "/dev/zvol/z/t"),
            "tar -xOf /c/d.tar.xz disk.raw | dd of=/dev/zvol/z/t bs=1m conv=sparse status=progress"
        );
        let fbsd = find("fbsd15-dev").unwrap();
        assert!(fbsd
            .write_pipeline(Path::new("/c/my image.raw.xz"), "/dev/zvol/z/t")
            .starts_with("xz -dc '/c/my image.raw.xz' | dd"));
        assert!(find("redox")
            .unwrap()
            .write_pipeline(Path::new("/c/r.img.zst"), "/dev/zvol/z/r")
            .starts_with("zstd -dc /c/r.img.zst | dd of=/dev/zvol/z/r"));
        assert_eq!(
            meta_data("dev"),
            "instance-id: juanitos-dev\nlocal-hostname: dev\n"
        );
        let ud = user_data_for(debian, "my-dev");
        assert!(ud.contains("\nhostname: my-dev\n"));
        assert!(!ud.contains("hostname: debian-13-dev"));
        assert!(ud.starts_with("#cloud-config\n"));
    }

    fn setup(tag: &str) -> (PathBuf, GlobalConfig, QubeStore) {
        let d = tmp(tag);
        let c = cfg(&d);
        let store = QubeStore::new(d.join("etc/qubes"));
        (d, c, store)
    }

    #[test]
    fn dry_run_install_prints_the_whole_pipeline() {
        let (d, c, store) = setup("dry");
        let m = MockRunner::new().respond("zfs", &["list"], CmdOutput::failed(1, ""));
        let inst = TemplateInstaller {
            runner: &m,
            cfg: &c,
            store: &store,
            dry_run: true,
        };
        let q = inst
            .install(find("debian-13-dev").unwrap(), &InstallOptions::default())
            .unwrap();
        assert_eq!(q.kind, QubeKind::Template);
        assert_eq!(q.disk_gb, 40);
        assert!(q.ip.is_some(), "templates are network clients");
        assert_eq!(q.label.as_deref(), Some("recipe:debian-13-dev"));
        assert!(store.exists("debian-13-dev"));
        let calls = m.calls();
        let pos = |p: &str| {
            calls
                .iter()
                .position(|c| c.starts_with(p))
                .unwrap_or_else(|| panic!("no {p:?} in {calls:#?}"))
        };
        assert!(pos("fetch -o") < pos("zfs create -V 40G"));
        assert!(pos("zfs create -V 40G") < pos("sh -c 'tar -xOf"));
        assert!(pos("sh -c") < pos("makefs -t cd9660"));
        assert!(pos("makefs") < pos("zfs snapshot zroot/juanitos/templates/debian-13-dev@base"));
        // The provisioning boot is printed, not run, and creates the taps.
        assert!(calls.iter().all(|c| !c.starts_with("bhyve ")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn real_install_verifies_boots_with_the_seed_and_snapshots() {
        let (d, c, store) = setup("real");
        let r = find("fbsd15-dev").unwrap();
        // Pretend the download happened: image + checksum list in the cache.
        let images = Path::new(&c.cache_dir).join("images");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir_all(&c.run_dir).unwrap();
        std::fs::write(images.join(r.image_file()), b"image").unwrap();
        let sha = crate::release::sha256_bytes(b"image");
        std::fs::write(
            images.join(format!("{}.checksums", r.name)),
            format!("SHA256 ({}) = {sha}\n", r.image_file()),
        )
        .unwrap();
        let m = MockRunner::new()
            .respond("zfs", &["list"], CmdOutput::failed(1, ""))
            .respond("bhyve", &[], CmdOutput::failed(1, "")); // guest powered off
        let inst = TemplateInstaller {
            runner: &m,
            cfg: &c,
            store: &store,
            dry_run: false,
        };
        let opts = InstallOptions {
            name: Some("podman".into()),
            disk_gb: Some(60),
            uplink: Some("vtnet0".into()),
            ..Default::default()
        };
        let q = inst.install(r, &opts).unwrap();
        assert_eq!(q.name, "podman");
        assert_eq!(q.disk_gb, 60);
        let calls = m.calls();
        // Cached image: only the checksum list was fetched.
        assert_eq!(calls.iter().filter(|c| c.starts_with("fetch")).count(), 1);
        let boot = calls.iter().find(|c| c.starts_with("bhyve ")).unwrap();
        assert!(boot.contains("29,ahci,cd:"), "{boot}");
        assert!(boot.contains("3,virtio-net,tap-podman"), "{boot}");
        assert!(calls.contains(&"ifconfig bru-vtnet0 addm vtnet0".to_string()));
        assert_eq!(q.uplink.as_deref(), Some("vtnet0"));
        assert!(boot.ends_with(" podman"));
        assert!(calls.contains(&"zfs snapshot zroot/juanitos/templates/podman@base".to_string()));
        // The seed carried the template's hostname (checked through the
        // makefs call; the seed directory and ISO are cleaned up after).
        assert!(calls
            .iter()
            .any(|c| c.starts_with("makefs") && c.contains("podman-seed")));
        assert!(!inst.seed_path("podman").exists());
        assert!(!Path::new(&c.run_dir).join("podman-seed").exists());
        // A second install of the same name is refused.
        assert!(matches!(inst.install(r, &opts), Err(Error::QubeExists(_))));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_download_is_rejected_and_removed() {
        let (d, c, store) = setup("corrupt");
        let r = find("debian-13-dev").unwrap();
        let images = Path::new(&c.cache_dir).join("images");
        std::fs::create_dir_all(&images).unwrap();
        let image = images.join(r.image_file());
        std::fs::write(&image, b"tampered").unwrap();
        let sums = images.join(format!("{}.checksums", r.name));
        std::fs::write(&sums, format!("{}  {}\n", "00".repeat(64), r.image_file())).unwrap();
        let m = MockRunner::new();
        let inst = TemplateInstaller {
            runner: &m,
            cfg: &c,
            store: &store,
            dry_run: false,
        };
        let err = inst.fetch(r, None).unwrap_err().to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!image.exists());
        // Not listed at all.
        std::fs::write(&image, b"x").unwrap();
        std::fs::write(&sums, "SHA512 (other) = 00\n").unwrap();
        assert!(inst
            .fetch(r, None)
            .unwrap_err()
            .to_string()
            .contains("not listed"));

        // A local image is verified the same way and never fetched.
        let local = d.join("local.tar.xz");
        std::fs::write(&local, b"local").unwrap();
        let local_sums = d.join("SUMS");
        std::fs::write(
            &local_sums,
            format!("{}  {}\n", "11".repeat(64), r.image_file()),
        )
        .unwrap();
        assert!(inst
            .fetch(r, Some((&local, &local_sums)))
            .unwrap_err()
            .to_string()
            .contains("mismatch"));
        assert!(!local.exists(), "a bad local copy is removed too");
        std::fs::write(&local, b"local").unwrap();
        let sha = digest_file(&local, Digest::Sha512).unwrap();
        std::fs::write(&local_sums, format!("{sha}  {}\n", r.image_file())).unwrap();
        let fetches_before = m.calls().iter().filter(|c| c.starts_with("fetch")).count();
        assert_eq!(inst.fetch(r, Some((&local, &local_sums))).unwrap(), local);
        let fetches_after = m.calls().iter().filter(|c| c.starts_with("fetch")).count();
        assert_eq!(
            fetches_before, fetches_after,
            "a local image is never fetched"
        );
        let opts = InstallOptions {
            image: Some(local.clone()),
            ..Default::default()
        };
        assert!(inst
            .install(r, &opts)
            .unwrap_err()
            .to_string()
            .contains("--checksums"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn redox_install_resolves_the_nightly_and_skips_provisioning() {
        let (d, c, store) = setup("redox");
        let r = find("redox").unwrap();
        let images = Path::new(&c.cache_dir).join("images");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir_all(&c.run_dir).unwrap();
        let file = "redox_server_x86_64_2026-10-13_552_harddrive.img.zst";
        std::fs::write(images.join(file), b"redox").unwrap();
        let sha = crate::release::sha256_bytes(b"redox");
        std::fs::write(
            images.join("redox.checksums"),
            format!("{sha} *{file}\n{sha} *redox_demo_x86_64_2026-10-13_552_harddrive.img.zst\n"),
        )
        .unwrap();
        let m = MockRunner::new().respond("zfs", &["list"], CmdOutput::failed(1, ""));
        let inst = TemplateInstaller {
            runner: &m,
            cfg: &c,
            store: &store,
            dry_run: false,
        };
        let q = inst.install(r, &InstallOptions::default()).unwrap();
        assert_eq!(q.os, OsType::Redox);
        assert_eq!(q.disk_gb, 4);
        let calls = m.calls();
        // Only the checksum list was fetched (the image was cached under
        // its resolved name); the zvol got the zstd stream; no seed, no
        // boot, straight to the snapshot.
        assert_eq!(calls.iter().filter(|c| c.starts_with("fetch")).count(), 1);
        assert!(calls
            .iter()
            .any(|c| c.starts_with("sh -c 'zstd -dc") && c.contains(file)));
        assert!(!calls.iter().any(|c| c.starts_with("makefs")));
        assert!(!calls.iter().any(|c| c.starts_with("bhyve")));
        assert!(calls.contains(&"zfs snapshot zroot/juanitos/templates/redox@base".to_string()));

        // Not cached: the resolved URL is fetched.
        let (d2, c2, store2) = setup("redox-fetch");
        let m = MockRunner::new().respond("zfs", &["list"], CmdOutput::failed(1, ""));
        let inst = TemplateInstaller {
            runner: &m,
            cfg: &c2,
            store: &store2,
            dry_run: true,
        };
        let images = Path::new(&c2.cache_dir).join("images");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join("redox.checksums"), format!("{sha} *{file}\n")).unwrap();
        inst.install(r, &InstallOptions::default()).unwrap();
        assert!(m.calls().iter().any(|c| c.starts_with("fetch -o")
            && c.ends_with(&format!("https://static.redox-os.org/img/x86_64/{file}"))));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&d2);
    }

    #[test]
    fn no_boot_stops_before_provisioning_and_bad_names_fail() {
        let (d, c, store) = setup("noboot");
        let m = MockRunner::new().respond("zfs", &["list"], CmdOutput::failed(1, ""));
        let inst = TemplateInstaller {
            runner: &m,
            cfg: &c,
            store: &store,
            dry_run: true,
        };
        let opts = InstallOptions {
            no_boot: true,
            ..Default::default()
        };
        inst.install(find("fbsd15-dev").unwrap(), &opts).unwrap();
        assert!(!m.calls().iter().any(|c| c.contains("snapshot")));
        let bad = InstallOptions {
            name: Some("Bad Name".into()),
            ..Default::default()
        };
        assert!(inst.install(find("fbsd15-dev").unwrap(), &bad).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
