//! Boot the ISO (or the disk it installed to) under QEMU with OVMF, and
//! run a FreeBSD builder VM on a Linux box. Both are plain command-line
//! assembly so they work unchanged on Linux, FreeBSD and macOS hosts.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};

/// Known OVMF code-file locations (Debian/Ubuntu, Fedora, Arch, FreeBSD,
/// Homebrew).
pub const OVMF_CANDIDATES: &[&str] = &[
    "/usr/share/OVMF/OVMF_CODE_4M.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
    "/usr/share/edk2/ovmf/OVMF_CODE.fd",
    "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd",
    "/usr/share/qemu/OVMF.fd",
    "/usr/local/share/edk2-qemu/QEMU_UEFI_CODE-x86_64.fd",
    "/usr/local/share/qemu/edk2-x86_64-code.fd",
    "/opt/homebrew/share/qemu/edk2-x86_64-code.fd",
];

pub fn find_ovmf() -> Option<PathBuf> {
    OVMF_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// Hardware acceleration flag for this host. On Linux, KVM only when
/// `/dev/kvm` is usable by this user (otherwise QEMU would refuse to
/// start); TCG is slow but always works.
pub fn accel() -> &'static str {
    if cfg!(target_os = "linux") {
        if std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok()
        {
            "kvm"
        } else {
            "tcg"
        }
    } else if cfg!(target_os = "macos") {
        "hvf"
    } else if cfg!(target_os = "freebsd") {
        "nvmm"
    } else {
        "tcg"
    }
}

#[derive(Debug, Clone)]
pub struct QemuOptions {
    pub iso: Option<PathBuf>,
    pub disk: PathBuf,
    pub disk_size_gb: u64,
    pub memory_mb: u64,
    pub cpus: u32,
    pub firmware: PathBuf,
    /// Writable copy of the UEFI variable store.
    pub vars: PathBuf,
    pub headless: bool,
    /// Expose VT-x to the guest so bhyve works inside (requires nested
    /// virtualization on the host).
    pub nested: bool,
    /// Emulate an IOMMU (matching the host CPU's vendor, see
    /// [`IommuVendor`]) plus a second NIC at `0/9/0` that the dom0 inside can
    /// hand to `sys-net` with `--pci 0/9/0`: the whole passthrough path
    /// (`pptdevs`, `vm_assign_pptdev`, DMA remapping) exercised in a nest.
    pub iommu: Option<IommuVendor>,
}

/// Which vIOMMU QEMU should emulate. bhyve picks its IOMMU driver from the
/// CPU vendor it sees (VT-d on Intel, AMD-Vi on AMD), so with `-cpu host`
/// the emulated IOMMU has to match the host, not the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IommuVendor {
    Intel,
    Amd,
}

impl IommuVendor {
    /// From `/proc/cpuinfo`'s `vendor_id` (Linux); Intel elsewhere.
    pub fn host() -> Self {
        std::fs::read_to_string("/proc/cpuinfo")
            .map(|s| Self::from_cpuinfo(&s))
            .unwrap_or(IommuVendor::Intel)
    }

    pub fn from_cpuinfo(text: &str) -> Self {
        if text.contains("AuthenticAMD") {
            IommuVendor::Amd
        } else {
            IommuVendor::Intel
        }
    }

    /// The `-device` spec. Both need `kernel-irqchip=split` and interrupt
    /// remapping; `dma-remap=on` makes QEMU's AMD-Vi actually translate
    /// (its default is identity mapping, which the `-S`-wired bhyve guest
    /// would not survive). QEMU's VT-d translates by default; its
    /// `caching-mode` (needed only for vfio/vhost devices) must stay off:
    /// FreeBSD's vmm panics on a DRHD with the CM bit set
    /// (`vtd_init: invalid caching mode`). The Intel variant follows the
    /// vmm source; it has not been booted (no Intel host at hand).
    pub fn device(self) -> &'static str {
        match self {
            IommuVendor::Intel => "intel-iommu,intremap=on",
            IommuVendor::Amd => "amd-iommu,intremap=on,dma-remap=on",
        }
    }
}

/// PCI slot of the NIC meant for passthrough inside the nested dom0.
pub const PASSTHROUGH_NIC_ADDR: &str = "0/9/0";

/// `qemu-system-x86_64` arguments for booting the install medium.
pub fn qemu_args(o: &QemuOptions) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    a.push("-machine".into());
    match o.iommu {
        Some(_) => a.push(format!("q35,accel={},kernel-irqchip=split", accel())),
        None => a.push(format!("q35,accel={}", accel())),
    }
    if let Some(v) = o.iommu {
        a.push("-device".into());
        a.push(v.device().into());
    }
    a.push("-cpu".into());
    a.push(if o.nested {
        "host".into()
    } else {
        "max".into()
    });
    a.push("-smp".into());
    a.push(o.cpus.to_string());
    a.push("-m".into());
    a.push(o.memory_mb.to_string());
    a.push("-drive".into());
    a.push(format!(
        "if=pflash,format=raw,readonly=on,file={}",
        o.firmware.display()
    ));
    a.push("-drive".into());
    a.push(format!("if=pflash,format=raw,file={}", o.vars.display()));
    a.push("-drive".into());
    a.push(format!("file={},if=virtio,format=qcow2", o.disk.display()));
    if let Some(iso) = &o.iso {
        a.push("-drive".into());
        a.push(format!("file={},media=cdrom,readonly=on", iso.display()));
    }
    a.push("-nic".into());
    a.push("user,model=virtio-net-pci".into());
    if o.iommu.is_some() {
        // A second, MSI-X capable NIC pinned to a slot the dom0 can name in
        // `pptdevs` without it moving when devices are added.
        a.push("-netdev".into());
        a.push("user,id=pt".into());
        a.push("-device".into());
        a.push("e1000e,netdev=pt,bus=pcie.0,addr=0x9".into());
    }
    a.push("-serial".into());
    a.push("mon:stdio".into());
    if o.headless {
        a.push("-display".into());
        a.push("none".into());
    }
    a
}

/// Run QEMU interactively (inherits the terminal). In dry-run mode only
/// print the command.
pub fn run_qemu(o: &QemuOptions, dry_run: bool) -> anyhow::Result<()> {
    if !o.disk.is_file() {
        let size = format!("{}G", o.disk_size_gb);
        let args = ["create", "-f", "qcow2", &o.disk.to_string_lossy(), &size];
        println!("+ qemu-img {}", args.join(" "));
        if !dry_run {
            let st = Command::new("qemu-img").args(args).status()?;
            if !st.success() {
                bail!("qemu-img failed");
            }
        }
    }
    if !o.vars.is_file() {
        // Fresh variable store: an empty file sized like the code image is
        // accepted by OVMF; prefer the matching *_VARS.fd when it exists.
        let vars_src = sibling_vars(&o.firmware);
        println!(
            "+ cp {} {}",
            vars_src
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(empty)".into()),
            o.vars.display()
        );
        if !dry_run {
            create_vars(&o.firmware, &o.vars)?;
        }
    }
    let args = qemu_args(o);
    println!("+ qemu-system-x86_64 {}", args.join(" "));
    if dry_run {
        return Ok(());
    }
    let st = Command::new("qemu-system-x86_64")
        .args(&args)
        .status()
        .context("running qemu-system-x86_64 (is QEMU installed?)")?;
    if !st.success() {
        bail!("qemu exited with {st}");
    }
    Ok(())
}

/// A fresh UEFI variable store at `vars`: a copy of the `*_VARS.fd` next
/// to `firmware`, or zeros sized like the code image (which OVMF accepts).
pub fn create_vars(firmware: &Path, vars: &Path) -> anyhow::Result<()> {
    if let Some(parent) = vars.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match sibling_vars(firmware) {
        Some(src) => {
            std::fs::copy(src, vars)?;
        }
        None => {
            let len = std::fs::metadata(firmware)
                .with_context(|| format!("reading {}", firmware.display()))?
                .len();
            std::fs::write(vars, vec![0u8; len as usize])?;
        }
    }
    Ok(())
}

/// `OVMF_CODE*.fd` → `OVMF_VARS*.fd` next to it, if present.
pub fn sibling_vars(code: &Path) -> Option<PathBuf> {
    let name = code.file_name()?.to_str()?;
    let vars = name.replace("CODE", "VARS").replace("code", "vars");
    if vars == name {
        return None;
    }
    let p = code.with_file_name(vars);
    p.is_file().then_some(p)
}

/// Builder VM: a stock FreeBSD VM image with this repository shared in
/// (9p/virtfs), so the ISO can be built from a Linux desktop. Inside the
/// guest: `kldload virtio_p9fs; mount -t p9fs -o trans=virtio juanitos /mnt`.
pub fn builder_args(image: &Path, repo: &Path, memory_mb: u64, cpus: u32) -> Vec<String> {
    vec![
        "-machine".into(),
        format!("q35,accel={}", accel()),
        "-cpu".into(),
        "max".into(),
        "-smp".into(),
        cpus.to_string(),
        "-m".into(),
        memory_mb.to_string(),
        "-drive".into(),
        format!("file={},if=virtio,format=qcow2", image.display()),
        "-virtfs".into(),
        format!(
            "local,path={},mount_tag=juanitos,security_model=mapped-xattr,id=juanitos",
            repo.display()
        ),
        "-nic".into(),
        "user,model=virtio-net-pci,hostfwd=tcp::2222-:22".into(),
        "-nographic".into(),
    ]
}

pub fn run_builder(image: &Path, repo: &Path, dry_run: bool) -> anyhow::Result<()> {
    let args = builder_args(image, repo, 8192, 4);
    println!("+ qemu-system-x86_64 {}", args.join(" "));
    if dry_run {
        return Ok(());
    }
    let st = Command::new("qemu-system-x86_64")
        .args(&args)
        .status()
        .context("running qemu-system-x86_64")?;
    if !st.success() {
        bail!("qemu exited with {st}");
    }
    Ok(())
}

/// Unattended stage 1: everything `run_builder` leaves to the user, driven
/// over the serial console. See [`AUTO_STEPS`] for what runs in the guest.
#[derive(Debug, Clone)]
pub struct AutoBuilder {
    /// Pristine FreeBSD VM image; it is copied, never modified.
    pub image: PathBuf,
    pub repo: PathBuf,
    /// Where the working disk copy, the console log and the results go.
    pub out: PathBuf,
    pub disk_size_gb: u64,
    pub memory_mb: u64,
    pub cpus: u32,
}

/// Shell run in the guest, in order, with a per-step timeout in minutes.
/// `/mnt/repo` is this repository (9p, read-only use), `/mnt/out` is
/// [`AutoBuilder::out`]. The tree is copied into the guest first: building
/// over 9p would be slow and leave root-owned files on the host.
pub const AUTO_STEPS: &[(&str, &str, u64)] = &[
    (
        "grow the root pool to the resized disk",
        "part=$(gpart show -p vtbd0 | awk '/freebsd-zfs/{print $3}'); \
         gpart recover vtbd0 >/dev/null 2>&1; \
         gpart resize -i ${part##*p} vtbd0 >/dev/null 2>&1; \
         zpool online -e zroot /dev/$part; zpool list -H -o name,size zroot",
        5,
    ),
    (
        "mount the host shares (9p)",
        "kldload -n virtio_p9fs && mkdir -p /mnt/repo /mnt/out && \
         mount -t p9fs -o trans=virtio juanitos /mnt/repo && \
         mount -t p9fs -o trans=virtio out /mnt/out && ls /mnt/repo/Cargo.toml",
        5,
    ),
    (
        "install the toolchain (rust, git)",
        "env ASSUME_ALWAYS_YES=yes pkg bootstrap -y >/dev/null && pkg install -y rust git && cargo --version",
        90,
    ),
    (
        "copy the source tree into the guest",
        "mkdir -p /root/juanitos && (cd /mnt/repo && tar --exclude ./target --exclude ./.git \
         --exclude ./work --exclude ./release/out -cf - .) | tar -C /root/juanitos -xf - && \
         ls /root/juanitos/crates",
        30,
    ),
    (
        "build the release binaries",
        "cd /root/juanitos && cargo build --release --workspace",
        240,
    ),
    // FreeBSD's rust package has no x86_64-unknown-uefi std, so the UEFI
    // applications come from the host (built on Linux, see
    // tools/docker-build.sh); `jos-release` itself warns and goes on when
    // they are absent.
    (
        "jos-release all (fetch, rootfs, pack, iso)",
        "cd /root/juanitos && extra=''; \
         [ -f /mnt/out/jos-boot.efi ] && extra=\"$extra --boot-efi /mnt/out/jos-boot.efi\"; \
         [ -f /mnt/out/jos-firewall.img ] && extra=\"$extra --firewall-image /mnt/out/jos-firewall.img\"; \
         ./target/release/jos-release --repo /root/juanitos all $extra",
        300,
    ),
    (
        "copy the results to the host",
        "cd /root/juanitos/release/out && cp -v *.iso *.iso.sha256 juanitos-dom0.txz MANIFEST /mnt/out/ && \
         cp -v /usr/local/share/juanitos/release.toml /mnt/out/ 2>/dev/null; \
         cp -v /root/juanitos/release/out/rootfs/usr/local/share/juanitos/release.toml /mnt/out/",
        30,
    ),
];

/// The stock VM images only talk to the VGA console. Booted through UEFI
/// they read `/efi/freebsd/loader.env` from their ESP, which is how the
/// serial console is switched on without touching the ZFS root. Hence the
/// working disk is raw (so the ESP can be edited with mtools) and OVMF is
/// used.
pub fn auto_builder_args(
    b: &AutoBuilder,
    disk: &Path,
    firmware: &Path,
    vars: &Path,
) -> Vec<String> {
    vec![
        "-machine".into(),
        format!("q35,accel={}", accel()),
        "-cpu".into(),
        "max".into(),
        "-smp".into(),
        b.cpus.to_string(),
        "-m".into(),
        b.memory_mb.to_string(),
        "-drive".into(),
        format!(
            "if=pflash,format=raw,readonly=on,file={}",
            firmware.display()
        ),
        "-drive".into(),
        format!("if=pflash,format=raw,file={}", vars.display()),
        "-drive".into(),
        format!("file={},if=virtio,format=raw", disk.display()),
        "-virtfs".into(),
        format!(
            "local,path={},mount_tag=juanitos,security_model=none,readonly=on,id=juanitos",
            b.repo.display()
        ),
        "-virtfs".into(),
        format!(
            "local,path={},mount_tag=out,security_model=none,id=out",
            b.out.display()
        ),
        "-nic".into(),
        "user,model=virtio-net-pci".into(),
        "-display".into(),
        "none".into(),
        "-monitor".into(),
        "none".into(),
        "-serial".into(),
        "stdio".into(),
    ]
}

pub fn run_auto_builder(b: &AutoBuilder, dry_run: bool) -> anyhow::Result<()> {
    use crate::console::Console;
    use std::process::Stdio;
    use std::time::Duration;

    std::fs::create_dir_all(&b.out)?;
    let disk = b.out.join("builder.raw");
    if !disk.is_file() {
        let size = format!("{}G", b.disk_size_gb);
        let disk_s = disk.to_string_lossy().into_owned();
        println!(
            "+ qemu-img convert -O raw {} {disk_s}\n+ qemu-img resize -f raw {disk_s} {size}\n\
             + write {}:/efi/freebsd/loader.env (serial console)",
            b.image.display(),
            disk_s
        );
        if !dry_run {
            let st = Command::new("qemu-img")
                .args(["convert", "-O", "raw", &b.image.to_string_lossy(), &disk_s])
                .status()
                .context("running qemu-img")?;
            if !st.success() {
                bail!("qemu-img convert failed");
            }
            let st = Command::new("qemu-img")
                .args(["resize", "-f", "raw", &disk_s, &size])
                .status()?;
            if !st.success() {
                bail!("qemu-img resize failed");
            }
        }
    }
    if !dry_run {
        // Idempotent, and cheap next to the conversion: always make sure
        // the console settings are there (a previous run may have been
        // interrupted between the two steps).
        crate::disk::write_into_esp(
            &disk,
            "efi/freebsd/loader.env",
            crate::disk::SERIAL_LOADER_ENV.as_bytes(),
        )
        .context("enabling the serial console in the VM image")?;
    }
    // Host-built UEFI artifacts for the guest to ship (see AUTO_STEPS).
    for (name, candidates) in [
        (
            "jos-boot.efi",
            [
                b.repo
                    .join("target/uefi/x86_64-unknown-uefi/release/jos-boot.efi"),
                b.repo
                    .join("target/docker/x86_64-unknown-uefi/release/jos-boot.efi"),
            ],
        ),
        (
            "jos-firewall.img",
            [
                b.repo.join("release/out/jos-firewall.img"),
                b.repo.join("target/docker/jos-firewall.img"),
            ],
        ),
    ] {
        if let Some(src) = candidates.iter().find(|p| p.is_file()) {
            println!("+ cp {} {}", src.display(), b.out.join(name).display());
            if !dry_run {
                std::fs::copy(src, b.out.join(name))?;
            }
        } else {
            println!("note: no host-built {name}; the ISO will do without it");
        }
    }
    let firmware = find_ovmf().context("OVMF firmware not found; install the ovmf/edk2 package")?;
    let vars = b.out.join("OVMF_VARS.fd");
    if !vars.is_file() && !dry_run {
        create_vars(&firmware, &vars)?;
    }
    let args = auto_builder_args(b, &disk, &firmware, &vars);
    println!("+ qemu-system-x86_64 {}", args.join(" "));
    for (what, cmd, minutes) in AUTO_STEPS {
        println!("   guest: {what} (timeout {minutes} min)\n      {cmd}");
    }
    if dry_run {
        return Ok(());
    }
    if accel() == "tcg" {
        println!("note: no usable /dev/kvm, running under TCG (slow; hours rather than minutes)");
    }
    let log = std::fs::File::create(b.out.join("console.log"))?;
    let child = Command::new("qemu-system-x86_64")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("running qemu-system-x86_64")?;
    let mut con = Console::attach(child, Some(log))?;

    con.expect("login:", Duration::from_secs(20 * 60))?;
    con.send_line("root")?;
    con.expect("# ", Duration::from_secs(120))?;
    // A quiet, predictable shell.
    con.run(
        "export ASSUME_ALWAYS_YES=yes TERM=dumb; stty -echo 2>/dev/null; true",
        Duration::from_secs(30),
    )?;
    for (what, cmd, minutes) in AUTO_STEPS {
        println!("\n==> builder: {what}");
        con.run(cmd, Duration::from_secs(minutes * 60))
            .with_context(|| format!("builder step failed: {what}"))?;
    }
    println!("\n==> builder: powering off");
    con.send_line("poweroff")?;
    con.wait_exit(Duration::from_secs(10 * 60))?;
    println!("\nDone: results in {}", b.out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_args_boot_cdrom_with_uefi() {
        let o = QemuOptions {
            iso: Some(PathBuf::from("/w/x.iso")),
            disk: PathBuf::from("/w/d.qcow2"),
            disk_size_gb: 40,
            memory_mb: 4096,
            cpus: 2,
            firmware: PathBuf::from("/f/OVMF_CODE.fd"),
            vars: PathBuf::from("/w/vars.fd"),
            headless: true,
            nested: true,
            iommu: None,
        };
        let a = qemu_args(&o).join(" ");
        assert!(a.contains("-cpu host"));
        assert!(!a.contains("iommu"));
        assert!(!a.contains("e1000e"));
        assert!(a.contains("if=pflash,format=raw,readonly=on,file=/f/OVMF_CODE.fd"));
        assert!(a.contains("file=/w/x.iso,media=cdrom,readonly=on"));
        assert!(a.contains("file=/w/d.qcow2,if=virtio,format=qcow2"));
        assert!(a.contains("-display none"));

        let o2 = QemuOptions {
            iso: None,
            headless: false,
            nested: false,
            ..o
        };
        let a2 = qemu_args(&o2).join(" ");
        assert!(!a2.contains("cdrom"));
        assert!(a2.contains("-cpu max"));
        assert!(!a2.contains("-display none"));
    }

    #[test]
    fn qemu_args_with_a_viommu_and_a_passthrough_nic() {
        let o = QemuOptions {
            iso: None,
            disk: PathBuf::from("/w/d.qcow2"),
            disk_size_gb: 40,
            memory_mb: 4096,
            cpus: 2,
            firmware: PathBuf::from("/f/OVMF_CODE.fd"),
            vars: PathBuf::from("/w/vars.fd"),
            headless: true,
            nested: true,
            iommu: Some(IommuVendor::Amd),
        };
        let a = qemu_args(&o).join(" ");
        assert!(a.contains("kernel-irqchip=split"));
        assert!(a.contains("-device amd-iommu,intremap=on,dma-remap=on"));
        assert!(a.contains("-device e1000e,netdev=pt,bus=pcie.0,addr=0x9"));
        // The IOMMU device is listed before the NIC it has to cover.
        assert!(a.find("amd-iommu").unwrap() < a.find("e1000e").unwrap());
        assert_eq!(PASSTHROUGH_NIC_ADDR, "0/9/0");

        let intel = QemuOptions {
            iommu: Some(IommuVendor::Intel),
            ..o
        };
        let a = qemu_args(&intel).join(" ");
        assert!(a.contains("-device intel-iommu,intremap=on "));
        // vmm's vtd_init panics on caching mode.
        assert!(!a.contains("caching-mode"));
    }

    #[test]
    fn iommu_vendor_from_cpuinfo() {
        assert_eq!(
            IommuVendor::from_cpuinfo("vendor_id\t: AuthenticAMD\n"),
            IommuVendor::Amd
        );
        assert_eq!(
            IommuVendor::from_cpuinfo("vendor_id\t: GenuineIntel\n"),
            IommuVendor::Intel
        );
        assert_eq!(IommuVendor::from_cpuinfo(""), IommuVendor::Intel);
        // Whatever the host is, the answer is one of the two.
        let _ = IommuVendor::host();
    }

    #[test]
    fn builder_shares_repo() {
        let a = builder_args(Path::new("/i.qcow2"), Path::new("/repo"), 8192, 4).join(" ");
        assert!(a.contains("mount_tag=juanitos"));
        assert!(a.contains("path=/repo"));
        assert!(a.contains("hostfwd=tcp::2222-:22"));
    }

    #[test]
    fn auto_builder_shares_repo_readonly_and_out_rw() {
        let b = AutoBuilder {
            image: PathBuf::from("/img.qcow2"),
            repo: PathBuf::from("/repo"),
            out: PathBuf::from("/out"),
            disk_size_gb: 60,
            memory_mb: 4096,
            cpus: 4,
        };
        let a = auto_builder_args(
            &b,
            Path::new("/out/builder.raw"),
            Path::new("/f/OVMF_CODE.fd"),
            Path::new("/out/vars.fd"),
        )
        .join(" ");
        assert!(a.contains("if=pflash,format=raw,readonly=on,file=/f/OVMF_CODE.fd"));
        assert!(a.contains("file=/out/builder.raw,if=virtio,format=raw"));
        assert!(a.contains("path=/repo,mount_tag=juanitos,security_model=none,readonly=on"));
        assert!(a.contains("path=/out,mount_tag=out,security_model=none,id=out"));
        assert!(a.contains("-serial stdio"));
        assert!(a.contains("-display none"));
        assert!(!a.contains("-nographic"));
        // Every step mounts/uses the shares consistently.
        let all: String = AUTO_STEPS.iter().map(|s| s.1).collect();
        assert!(all.contains("mount -t p9fs -o trans=virtio juanitos /mnt/repo"));
        assert!(all.contains("/mnt/out/"));
        assert!(all.contains("jos-release --repo /root/juanitos all"));
    }

    #[test]
    fn vars_name_derivation() {
        assert!(sibling_vars(Path::new("/nonexistent/OVMF_CODE.fd")).is_none());
        assert!(sibling_vars(Path::new("/nonexistent/edk2-x86_64-code.fd")).is_none());
        assert!(
            sibling_vars(Path::new("/f/OVMF.fd")).is_none(),
            "no CODE in the name"
        );
        assert!(sibling_vars(Path::new("/")).is_none());
    }

    #[test]
    fn vars_store_from_the_sibling_or_zeros() {
        let dir = std::env::temp_dir().join(format!("jos-vars-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Fedora/Debian layout: the matching VARS file is copied.
        let code = dir.join("OVMF_CODE.fd");
        std::fs::write(&code, [1u8; 64]).unwrap();
        std::fs::write(dir.join("OVMF_VARS.fd"), b"template").unwrap();
        assert_eq!(sibling_vars(&code), Some(dir.join("OVMF_VARS.fd")));
        let vars = dir.join("a/b/vars.fd");
        create_vars(&code, &vars).unwrap();
        assert_eq!(std::fs::read(&vars).unwrap(), b"template");

        // A lone combined image: an empty store of the same size.
        let lone = dir.join("edk2-x86_64-code.fd");
        std::fs::write(&lone, [1u8; 32]).unwrap();
        let vars = dir.join("lone-vars.fd");
        create_vars(&lone, &vars).unwrap();
        assert_eq!(std::fs::read(&vars).unwrap(), vec![0u8; 32]);

        let err = create_vars(&dir.join("missing-code.fd"), &dir.join("v.fd")).unwrap_err();
        assert!(format!("{err:#}").contains("missing-code.fd"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
