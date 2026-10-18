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
    accel_for(std::env::consts::OS, kvm_usable())
}

fn kvm_usable() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

/// The accelerator for an OS (`std::env::consts::OS`) and whether
/// `/dev/kvm` can be opened.
pub fn accel_for(os: &str, kvm: bool) -> &'static str {
    match (os, kvm) {
        ("linux", true) => "kvm",
        ("macos", _) => "hvf",
        ("freebsd", _) => "nvmm",
        _ => "tcg",
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
        create_qcow2(&o.disk, o.disk_size_gb, dry_run)?;
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

/// An empty (sparse) qcow2 disk.
fn create_qcow2(disk: &Path, size_gb: u64, dry_run: bool) -> anyhow::Result<()> {
    let size = format!("{size_gb}G");
    let args = ["create", "-f", "qcow2", &disk.to_string_lossy(), &size];
    println!("+ qemu-img {}", args.join(" "));
    if !dry_run {
        let st = Command::new("qemu-img")
            .args(args)
            .status()
            .context("running qemu-img")?;
        if !st.success() {
            bail!("qemu-img failed");
        }
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
    // The profile's `local_repo` (the COSMIC packages) usually lives under
    // target/, which the tree copy leaves out: carry it over on its own.
    (
        "copy the source tree into the guest",
        "mkdir -p /root/juanitos && (cd /mnt/repo && tar --exclude ./target --exclude ./.git \
         --exclude ./work --exclude ./release/out -cf - .) | tar -C /root/juanitos -xf - && \
         r=$(sed -n 's/^local_repo *= *\"\\(.*\\)\"/\\1/p' /root/juanitos/release/juanitos.toml) && \
         { [ -z \"$r\" ] || [ ! -d \"/mnt/repo/$r\" ] || { mkdir -p \"/root/juanitos/$r\" && \
         (cd \"/mnt/repo/$r\" && tar -cf - .) | tar -C \"/root/juanitos/$r\" -xf - && \
         echo \"local package repository: $r\"; }; } && \
         ls /root/juanitos/crates",
        30,
    ),
    // Only the builder: `jos-release all` builds the shipped binaries
    // itself, with the path remapping that makes them comparable.
    (
        "build jos-release",
        "cd /root/juanitos && cargo build --release -p jos-release",
        120,
    ),
    // FreeBSD's rust package has no x86_64-unknown-uefi std, so the UEFI
    // applications come from the host (built on Linux, see
    // tools/docker-build.sh); `jos-release` itself warns and goes on when
    // they are absent.
    (
        "jos-release all (fetch, bins, rootfs, pack, iso)",
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
    let mut con = spawn_console(&args, &b.out)?;
    con.expect("login:", Duration::from_secs(20 * 60))?;
    root_shell(&mut con, None)?;
    run_steps(&mut con, "builder", AUTO_STEPS.iter().copied())?;
    power_off(&mut con, "builder")?;
    println!("\nDone: results in {}", b.out.display());
    Ok(())
}

/// Start QEMU with its serial console on a pipe we drive, logged to
/// `<out>/console.log`.
fn spawn_console(args: &[String], out: &Path) -> anyhow::Result<crate::console::Console> {
    use std::process::Stdio;
    if accel() == "tcg" {
        println!("note: no usable /dev/kvm, running under TCG (slow; hours rather than minutes)");
    }
    let log = std::fs::File::create(out.join("console.log"))?;
    let child = Command::new("qemu-system-x86_64")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("running qemu-system-x86_64")?;
    crate::console::Console::attach(child, Some(log))
}

/// Log in as root at a `login:` prompt that has just been seen, and make
/// the shell quiet and predictable.
fn root_shell(con: &mut crate::console::Console, password: Option<&str>) -> anyhow::Result<()> {
    use std::time::Duration;
    con.send_line("root")?;
    if let Some(pw) = password {
        con.expect("Password:", Duration::from_secs(60))?;
        con.send_line(pw)?;
    }
    con.expect("# ", Duration::from_secs(120))?;
    con.run(
        "export ASSUME_ALWAYS_YES=yes TERM=dumb; stty -echo 2>/dev/null; true",
        Duration::from_secs(30),
    )
}

/// Run `(what, command, timeout in minutes)` steps in order; the first
/// failure stops the run.
fn run_steps<'s>(
    con: &mut crate::console::Console,
    who: &str,
    steps: impl IntoIterator<Item = (&'s str, &'s str, u64)>,
) -> anyhow::Result<()> {
    for (what, cmd, minutes) in steps {
        println!("\n==> {who}: {what}");
        con.run(cmd, std::time::Duration::from_secs(minutes * 60))
            .with_context(|| format!("{who} step failed: {what}"))?;
    }
    Ok(())
}

fn power_off(con: &mut crate::console::Console, who: &str) -> anyhow::Result<()> {
    println!("\n==> {who}: powering off");
    con.send_line("poweroff")?;
    con.wait_exit(std::time::Duration::from_secs(10 * 60))
}

/// Unattended bootstrap round (stage N → N+1, `docs/bootstrap.md`): boot
/// the stage-N ISO, install it, reboot into the installed dom0, rebuild
/// JuanitOS there and compare the new dom0 tarball with the one the ISO
/// carried.
#[derive(Debug, Clone)]
pub struct SelfHost {
    pub iso: PathBuf,
    /// Working disk, console log, and what the guest copies out.
    pub out: PathBuf,
    pub disk_size_gb: u64,
    pub memory_mb: u64,
    pub cpus: u32,
}

/// Root password of the throwaway installed system.
pub const SELFHOST_PASSWORD: &str = "jos";

/// The unattended install from the live medium. The VM has no IOMMU, hence
/// `--ignore-doctor`; nothing is reserved for passthrough and the desktop
/// stays off so the reboot lands on the serial login.
pub fn selfhost_install() -> String {
    format!(
        "jos-install --non-interactive --ignore-doctor --disk vtbd0 --no-gui --no-pci \
         --root-password {SELFHOST_PASSWORD}"
    )
}

/// Steps run in the installed dom0. `/mnt/out` is [`SelfHost::out`]; the
/// install medium stays attached as `cd0`, so the stage-N tarball is read
/// from it rather than copied through the host.
pub fn selfhost_steps() -> Vec<(&'static str, String, u64)> {
    use crate::builder::INSTALLED_SOURCE;
    use juanitos_core::dom0::{DIST_DIR, DIST_TARBALL, RELEASE_INFO_PATH};
    let previous = format!("/mnt/cd{DIST_DIR}/{DIST_TARBALL}");
    let built = format!("{INSTALLED_SOURCE}/release/out/{DIST_TARBALL}");
    vec![
        (
            "temporary uplink (the dom0 has no network by design)",
            "dhclient vtnet0 >/dev/null && route -n get default | grep gateway".to_string(),
            5,
        ),
        (
            "mount the results share and the install medium",
            format!(
                "kldload -n virtio_p9fs && mkdir -p /mnt/out /mnt/cd && \
                 mount -t p9fs -o trans=virtio out /mnt/out && \
                 mount_cd9660 /dev/cd0 /mnt/cd && ls -l {previous}"
            ),
            5,
        ),
        // The tarball is what is compared; the ISO (8 GB with COSMIC) would
        // only cost disk and time.
        (
            "build the stage N+1 dom0 tarball (fetch, bins, rootfs, pack)",
            format!(
                "cd {INSTALLED_SOURCE} && jos-release fetch && jos-release bins && \
                 jos-release rootfs && jos-release pack"
            ),
            300,
        ),
        (
            "copy the results to the host",
            format!(
                "cd {INSTALLED_SOURCE}/release/out && \
                 cp -v {DIST_TARBALL} MANIFEST /mnt/out/ && \
                 cp -v rootfs{RELEASE_INFO_PATH} /mnt/out/ && \
                 cp -v {RELEASE_INFO_PATH} /mnt/out/release-previous.toml"
            ),
            30,
        ),
        (
            "compare stage N with stage N+1",
            format!(
                "jos-release --repo {INSTALLED_SOURCE} compare {previous} {built} \
                 > /mnt/out/compare.txt 2>&1; s=$?; tail -n 40 /mnt/out/compare.txt; [ $s -eq 0 ]"
            ),
            60,
        ),
    ]
}

/// The disk boots first (`bootindex`): while it is blank OVMF falls through
/// to the ISO, and once installed the reboot lands in the new dom0 with the
/// ISO still attached. No display device at all: without a Graphics Output
/// Protocol `jos-boot` hands over to the serial console, where both the
/// live installer and the login prompt then run.
pub fn selfhost_args(s: &SelfHost, disk: &Path, firmware: &Path, vars: &Path) -> Vec<String> {
    vec![
        "-machine".into(),
        format!("q35,accel={}", accel()),
        "-cpu".into(),
        "max".into(),
        "-smp".into(),
        s.cpus.to_string(),
        "-m".into(),
        s.memory_mb.to_string(),
        "-drive".into(),
        format!(
            "if=pflash,format=raw,readonly=on,file={}",
            firmware.display()
        ),
        "-drive".into(),
        format!("if=pflash,format=raw,file={}", vars.display()),
        "-drive".into(),
        format!("file={},if=none,id=hd,format=qcow2", disk.display()),
        "-device".into(),
        "virtio-blk-pci,drive=hd,bootindex=0".into(),
        "-drive".into(),
        format!(
            "file={},if=none,id=cd,media=cdrom,readonly=on",
            s.iso.display()
        ),
        "-device".into(),
        "ide-cd,drive=cd,bootindex=1".into(),
        "-virtfs".into(),
        format!(
            "local,path={},mount_tag=out,security_model=none,id=out",
            s.out.display()
        ),
        "-nic".into(),
        "user,model=virtio-net-pci".into(),
        "-vga".into(),
        "none".into(),
        "-display".into(),
        "none".into(),
        "-monitor".into(),
        "none".into(),
        "-serial".into(),
        "stdio".into(),
    ]
}

/// What the live installer may show first on the serial console: the
/// failed host checks' question, or the disk list. ^D (end of input) at
/// either ends it, and the live medium goes on to its login prompt.
pub const LIVE_INSTALLER_PROMPTS: &[&str] = &["Install anyway?", "Disks:"];

pub fn run_selfhost(s: &SelfHost, dry_run: bool) -> anyhow::Result<()> {
    if !s.iso.is_file() && !dry_run {
        bail!("{} does not exist; build it first", s.iso.display());
    }
    std::fs::create_dir_all(&s.out)?;
    // Always a blank disk: an installed one would boot instead of the ISO.
    let disk = s.out.join("selfhost.qcow2");
    let vars = s.out.join("OVMF_VARS.fd");
    for stale in [&disk, &vars] {
        if stale.exists() {
            println!("+ rm {}", stale.display());
            if !dry_run {
                std::fs::remove_file(stale)?;
            }
        }
    }
    create_qcow2(&disk, s.disk_size_gb, dry_run)?;
    let firmware = find_ovmf().context("OVMF firmware not found; install the ovmf/edk2 package")?;
    if !dry_run {
        create_vars(&firmware, &vars)?;
    }
    let args = selfhost_args(s, &disk, &firmware, &vars);
    println!("+ qemu-system-x86_64 {}", args.join(" "));
    println!(
        "   live: ^D at the installer, log in as root\n      {}",
        selfhost_install()
    );
    let steps = selfhost_steps();
    for (what, cmd, minutes) in &steps {
        println!("   dom0: {what} (timeout {minutes} min)\n      {cmd}");
    }
    if dry_run {
        return Ok(());
    }

    let mut con = spawn_console(&args, &s.out)?;
    drive_selfhost(&mut con, &selfhost_install(), &steps)?;
    println!(
        "\nSelf-hosting: stage N+1 matches stage N ({})",
        s.out.join("compare.txt").display()
    );
    Ok(())
}

/// The console side of [`run_selfhost`]: live medium, install, reboot,
/// `steps` in the installed dom0, power off (also after a failed step, so
/// the disk and the results stay consistent).
fn drive_selfhost(
    con: &mut crate::console::Console,
    install: &str,
    steps: &[(&str, String, u64)],
) -> anyhow::Result<()> {
    use std::time::Duration;
    println!("\n==> selfhost: booting the live medium");
    let first = con.expect_any(
        &[LIVE_INSTALLER_PROMPTS, &["login:"]].concat(),
        Duration::from_secs(20 * 60),
    )?;
    if first < LIVE_INSTALLER_PROMPTS.len() {
        con.send_raw(b"\x04")?;
        con.expect("login:", Duration::from_secs(10 * 60))?;
    }
    root_shell(con, None)?;
    run_steps(con, "selfhost", [("install to vtbd0", install, 30)])?;

    println!("\n==> selfhost: rebooting into the installed dom0");
    con.send_line("reboot")?;
    con.expect("login:", Duration::from_secs(20 * 60))?;
    root_shell(con, Some(SELFHOST_PASSWORD))?;
    let result = run_steps(
        con,
        "selfhost",
        steps.iter().map(|(w, c, m)| (*w, c.as_str(), *m)),
    );
    power_off(con, "selfhost")?;
    result
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
    fn accelerator_per_host() {
        assert_eq!(accel_for("linux", true), "kvm");
        assert_eq!(accel_for("linux", false), "tcg");
        assert_eq!(accel_for("macos", false), "hvf");
        assert_eq!(accel_for("freebsd", true), "nvmm");
        assert_eq!(accel_for("windows", false), "tcg");
        assert!(["kvm", "tcg", "hvf", "nvmm"].contains(&accel()));
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
        // The local (COSMIC) repository is under the excluded target/.
        assert!(all.contains("--exclude ./target"));
        assert!(all.contains("s/^local_repo *= *\"\\(.*\\)\"/\\1/p"));
        assert!(all.contains("tar -C \"/root/juanitos/$r\" -xf -"));
    }

    #[test]
    fn selfhost_boots_the_disk_first_with_the_iso_attached() {
        let s = SelfHost {
            iso: PathBuf::from("/w/JuanitOS.iso"),
            out: PathBuf::from("/w/selfhost"),
            disk_size_gb: 40,
            memory_mb: 6144,
            cpus: 4,
        };
        let a = selfhost_args(
            &s,
            Path::new("/w/selfhost/selfhost.qcow2"),
            Path::new("/f/OVMF_CODE.fd"),
            Path::new("/w/selfhost/vars.fd"),
        )
        .join(" ");
        assert!(a.contains("file=/w/selfhost/selfhost.qcow2,if=none,id=hd,format=qcow2"));
        assert!(a.contains("-device virtio-blk-pci,drive=hd,bootindex=0"));
        assert!(a.contains("file=/w/JuanitOS.iso,if=none,id=cd,media=cdrom,readonly=on"));
        assert!(a.contains("-device ide-cd,drive=cd,bootindex=1"));
        assert!(a.contains("path=/w/selfhost,mount_tag=out,security_model=none,id=out"));
        // Serial only: no GOP, so jos-boot and the installer use the console.
        assert!(a.contains("-vga none -display none -monitor none -serial stdio"));
        assert!(a.contains("-smp 4 -m 6144"));

        let install = selfhost_install();
        assert!(install.contains("--non-interactive --ignore-doctor --disk vtbd0"));
        assert!(install.ends_with(&format!("--root-password {SELFHOST_PASSWORD}")));

        let steps = selfhost_steps();
        let all: String = steps.iter().map(|s| s.1.as_str()).collect();
        assert!(all.contains("mount -t p9fs -o trans=virtio out /mnt/out"));
        assert!(all.contains("mount_cd9660 /dev/cd0 /mnt/cd"));
        assert!(all.contains("cd /usr/local/src/juanitos && jos-release fetch && "));
        assert!(all.contains("jos-release rootfs && jos-release pack"));
        assert!(!all.contains("jos-release all"), "no ISO needed");
        // The new tarball against the one the ISO installed from; the
        // comparison's status is the step's.
        let (what, compare, _) = steps.last().unwrap();
        assert!(what.starts_with("compare"));
        assert!(compare.contains(
            "compare /mnt/cd/usr/local/share/juanitos/dist/juanitos-dom0.txz \
             /usr/local/src/juanitos/release/out/juanitos-dom0.txz"
        ));
        assert!(compare.ends_with("[ $s -eq 0 ]"));
        // Results leave the guest before the comparison can fail.
        let copy = steps.iter().position(|s| s.0.starts_with("copy")).unwrap();
        assert_eq!(copy, steps.len() - 2);
        assert!(steps.iter().all(|s| s.2 > 0));
    }

    /// A guest played by `sh`: the live installer's disk list (ended by
    /// one byte, the ^D), a password-less live login, `reboot` to a login
    /// with a password, `poweroff`.
    #[cfg(unix)]
    fn fake_guest() -> crate::console::Console {
        use std::process::Stdio;
        let script = r#"
            reboot() { printf 'booting\nlogin: '; read u; printf 'Password:'; read p; printf '# '; }
            poweroff() { exit 0; }
            printf 'Disks:\n'; dd bs=1 count=1 >/dev/null 2>&1
            printf 'login: '; read u; printf '# '
            while read -r line; do eval "$line"; done
        "#;
        let child = Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        crate::console::Console::attach(child, None).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn selfhost_dialogue_with_a_guest() {
        let mut con = fake_guest();
        let steps = vec![
            ("first", "true".to_string(), 1),
            ("second", "echo hi".to_string(), 1),
        ];
        drive_selfhost(&mut con, "true", &steps).unwrap();

        // A failing step is reported, after the guest was powered off.
        let mut con = fake_guest();
        let steps = vec![("compare", "false".to_string(), 1)];
        let err = drive_selfhost(&mut con, "true", &steps).unwrap_err();
        assert!(
            format!("{err:#}").contains("selfhost step failed: compare"),
            "{err:#}"
        );

        // So is a failed install, before any reboot.
        let mut con = fake_guest();
        let err = drive_selfhost(&mut con, "false", &steps).unwrap_err();
        assert!(format!("{err:#}").contains("install to vtbd0"), "{err:#}");
    }

    #[test]
    fn selfhost_dry_run_starts_from_a_blank_disk() {
        let dir = std::env::temp_dir().join(format!("jos-selfhost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = SelfHost {
            iso: dir.join("missing.iso"),
            out: dir.clone(),
            disk_size_gb: 40,
            memory_mb: 2048,
            cpus: 2,
        };
        // A previous run's disk is left alone by a dry run.
        std::fs::write(dir.join("selfhost.qcow2"), b"old").unwrap();
        match run_selfhost(&s, true) {
            Ok(()) => assert!(dir.join("selfhost.qcow2").is_file()),
            Err(e) => assert!(format!("{e:#}").contains("OVMF"), "{e:#}"),
        }
        let err = run_selfhost(&s, false).unwrap_err();
        assert!(err.to_string().contains("build it first"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
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
