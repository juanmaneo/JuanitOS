// SPDX-License-Identifier: BSD-3-Clause
//! The dom0 userland under podman on FreeBSD, where a container is a jail
//! (ocijail): Linux → QEMU → FreeBSD → podman → JuanitOS, or podman
//! straight on a FreeBSD host. The dom0 tarball is a complete FreeBSD root,
//! so it is the whole image (`release/oci/Containerfile`), and the checks
//! run the JuanitOS tools inside it. A jail shares its host's kernel:
//! bhyve, KMS, pool creation and the boot chain stay with the `qemu`,
//! `selfhost` and `vbox` harnesses.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use juanitos_core::dom0::{DIST_TARBALL, REQUIRED_PACKAGES};
use juanitos_core::sys::shell_quote;

use crate::qemu;

/// The image's name in podman's local storage.
pub const IMAGE: &str = "localhost/juanitos-dom0";

/// The Containerfile, relative to the repository.
pub const CONTAINERFILE: &str = "release/oci/Containerfile";

/// The overlay disk the VM runs on, under [`Oci::out`].
pub const OVERLAY: &str = "oci.qcow2";

#[derive(Debug, Clone)]
pub struct Oci {
    pub repo: PathBuf,
    pub tarball: PathBuf,
    /// The build context (a link to the tarball), the console log and the
    /// VM's disk.
    pub out: PathBuf,
    /// The release the userland must report (`freebsd-version -u`).
    pub freebsd_version: String,
    /// Where podman runs: this FreeBSD host when `None`.
    pub vm: Option<Vm>,
}

#[derive(Debug, Clone)]
pub struct Vm {
    /// A FreeBSD disk with its serial console on, such as the builder's
    /// working disk. Never written: the VM runs on a qcow2 overlay.
    pub image: PathBuf,
    /// OVMF code file.
    pub firmware: PathBuf,
    pub memory_mb: u64,
    pub cpus: u32,
    /// Leave the overlay (and the image built in it) afterwards.
    pub keep: bool,
}

/// What the VM needs before the [`podman_steps`]: podman, a ZFS dataset
/// for its storage and fdescfs for conmon (the package's post-install
/// notes). No pf: the containers have no network. The dataset is
/// compressed whatever the pool's default: the unpacked root is several
/// gigabytes, written twice (container, then commit), and every block the
/// VM writes grows its overlay on the host.
pub const PODMAN_SETUP: (&str, &str, u64) = (
    "install podman and give it a ZFS dataset",
    "env ASSUME_ALWAYS_YES=yes pkg bootstrap -y >/dev/null && \
     pkg install -y podman-suite >/dev/null && podman --version && \
     { zfs list zroot/containers >/dev/null 2>&1 || \
     zfs create -o compression=zstd -o mountpoint=/var/db/containers zroot/containers; } && \
     { mount | grep -q ' on /dev/fd ' || mount -t fdescfs fdesc /dev/fd; }",
    30,
);

/// The build context inside the VM, and buildah's scratch space next to
/// it on the compressed dataset rather than in `/var/tmp`.
pub const VM_CONTEXT: &str = "/var/db/containers/context";
pub const VM_TMPDIR: &str = "/var/db/containers/tmp";

/// The tarball as the VM hands it to buildah. buildah reads a context on
/// the 9p share at a crawl, and unpacks xz far slower than zstd (an hour
/// against six minutes for the dom0), so the VM recompresses it locally.
pub const VM_TARBALL: &str = "juanitos-dom0.tar.zst";

/// The checks, each a shell script run in a fresh container.
pub fn checks(freebsd_version: &str) -> Vec<(&'static str, String)> {
    let packages = REQUIRED_PACKAGES.join(" ");
    vec![
        (
            "the userland is the release",
            format!(
                "v=$(freebsd-version -u) && echo $v && case $v in {freebsd_version}*) ;; \
                 *) exit 1 ;; esac"
            ),
        ),
        (
            "the dom0 packages are installed with their dependencies",
            format!(
                "for p in {packages}; do pkg info -e $p || {{ echo missing $p; exit 1; }}; done; \
                 pkg check -d -n -a"
            ),
        ),
        (
            "every program finds its shared libraries",
            "! ldd /bin/* /sbin/* /usr/bin/* /usr/sbin/* /usr/local/bin/* /usr/local/sbin/* \
             2>/dev/null | grep -B 20 'not found'"
                .to_string(),
        ),
        (
            "jos doctor runs the host checks",
            "jos doctor > /tmp/doctor.txt; cat /tmp/doctor.txt; grep -q /dev/vmm /tmp/doctor.txt"
                .to_string(),
        ),
        (
            "jos plans a template and an app qube",
            "export JOS_CONFIG_DIR=/tmp/jos && jos --dry-run init && \
             jos --dry-run create fbsd15 --kind template --os freebsd --disk-gb 10 && \
             jos --dry-run create work --kind app --os freebsd --template fbsd15 && \
             jos --dry-run start work | grep 'bhyve '"
                .to_string(),
        ),
        (
            "jos-install plans an install",
            "d=$(sysctl -n kern.disks | tr ' ' '\n' | grep -v '^cd' | head -n 1) && echo disk $d && \
             jos-install --dry-run --non-interactive --ignore-doctor --disk $d --no-pci \
             --root-password jos --user alice --user-password jos | grep 'zpool create'"
                .to_string(),
        ),
        (
            "alice logs in through PAM",
            "pw useradd alice -m -G wheel,video && echo jos | pw usermod alice -h 0 && \
             echo jos | jos-greet try-login --user alice -- /usr/bin/id"
                .to_string(),
        ),
        (
            "jos-splash renders the boot card",
            "jos-splash render /tmp/card.png --stage network --status Starting && \
             test -s /tmp/card.png"
                .to_string(),
        ),
        (
            "the rc scripts parse and order",
            "for f in /usr/local/etc/rc.d/jos*; do sh -n $f || exit 1; done && \
             ! rcorder /etc/rc.d/* /usr/local/etc/rc.d/* 2>&1 >/dev/null | grep ."
                .to_string(),
        ),
    ]
}

/// Build the image from `tarball` in `context` with `containerfile`, then
/// run the [`checks`]; podman's commands, as run on its host.
pub fn podman_steps(
    containerfile: &str,
    context: &str,
    tarball: &str,
    freebsd_version: &str,
) -> Vec<(&'static str, String, u64)> {
    let build = format!(
        "podman rmi -i -f {IMAGE} >/dev/null && \
         podman build --network none --build-arg TARBALL={} -f {} -t {IMAGE} {} && \
         podman image ls {IMAGE}",
        shell_quote(tarball),
        shell_quote(containerfile),
        shell_quote(context)
    );
    let mut steps = vec![("build the image from the dom0 tarball", build, 60)];
    steps.extend(checks(freebsd_version).into_iter().map(|(what, script)| {
        let run = format!(
            "podman run --rm --network none {IMAGE} /bin/sh -c {}",
            shell_quote(&script)
        );
        (what, run, 10)
    }));
    steps
}

pub fn run_oci(o: &Oci, dry_run: bool) -> anyhow::Result<()> {
    if !o.tarball.is_file() && !dry_run {
        bail!("{} does not exist; build it first", o.tarball.display());
    }
    let context = o.out.join("context");
    let link = context.join(DIST_TARBALL);
    println!("+ ln {} {}", o.tarball.display(), link.display());
    if !dry_run {
        std::fs::create_dir_all(&context)?;
        link_tarball(&o.tarball, &link)?;
    }
    let result = match &o.vm {
        Some(vm) => in_vm(o, vm, dry_run),
        None => on_host(o, &context, dry_run),
    };
    if !dry_run {
        // Only a link, which the next run replaces anyway.
        let _ = std::fs::remove_file(&link);
    }
    result?;
    println!("\nThe dom0 userland passed its checks under podman");
    Ok(())
}

/// A hard link: the tarball is a gigabyte or more. Whatever a previous
/// run left is replaced.
fn link_tarball(tarball: &Path, link: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(link);
    std::fs::hard_link(tarball, link).with_context(|| {
        format!(
            "linking {} to {} (--out must be on the tarball's file system)",
            tarball.display(),
            link.display()
        )
    })
}

fn on_host(o: &Oci, context: &Path, dry_run: bool) -> anyhow::Result<()> {
    let containerfile = o.repo.join(CONTAINERFILE).display().to_string();
    let context = context.display().to_string();
    for (what, cmd, _) in podman_steps(&containerfile, &context, DIST_TARBALL, &o.freebsd_version) {
        println!("\n==> oci: {what}");
        qemu::run_tool("sh", &["-c".into(), cmd], dry_run)
            .with_context(|| format!("oci step failed: {what}"))?;
    }
    Ok(())
}

/// The VM's steps: the shares, podman, the tarball recompressed into
/// [`VM_CONTEXT`], then the [`podman_steps`] with the shared Containerfile;
/// the context and buildah's scratch go once the image is built.
pub fn vm_steps(freebsd_version: &str) -> Vec<(&'static str, String, u64)> {
    let containerfile = format!("/mnt/repo/{CONTAINERFILE}");
    let stage = format!(
        "mkdir -p {VM_CONTEXT} {VM_TMPDIR} && xz -dc -T0 /mnt/out/context/{DIST_TARBALL} | \
         zstd -T0 -q -f -o {VM_CONTEXT}/{VM_TARBALL}"
    );
    let setup = [qemu::MOUNT_SHARES, PODMAN_SETUP]
        .into_iter()
        .map(|(what, cmd, minutes)| (what, cmd.to_string(), minutes))
        .chain([("recompress the tarball into the VM", stage, 15)]);
    let mut podman =
        podman_steps(&containerfile, VM_CONTEXT, VM_TARBALL, freebsd_version).into_iter();
    let build = podman.next().map(|(what, cmd, minutes)| {
        (
            what,
            format!("(export TMPDIR={VM_TMPDIR} && {cmd})"),
            minutes,
        )
    });
    let cleanup = (
        "drop the context",
        format!("rm -rf {VM_CONTEXT} {VM_TMPDIR}"),
        5,
    );
    setup.chain(build).chain([cleanup]).chain(podman).collect()
}

fn in_vm(o: &Oci, vm: &Vm, dry_run: bool) -> anyhow::Result<()> {
    if !vm.image.is_file() && !dry_run {
        bail!(
            "{} does not exist; `jos-release builder --auto` makes it",
            vm.image.display()
        );
    }
    let overlay = o.out.join(OVERLAY);
    let vars = o.out.join("OVMF_VARS.fd");
    // The backing file is resolved from the overlay's directory.
    let backing = vm.image.canonicalize().unwrap_or_else(|_| vm.image.clone());
    if !dry_run && overlay.exists() {
        std::fs::remove_file(&overlay)?;
    }
    let create = [
        "create".into(),
        "-f".into(),
        "qcow2".into(),
        "-F".into(),
        qemu::disk_format(&vm.image).into(),
        "-b".into(),
        backing.display().to_string(),
        overlay.display().to_string(),
    ];
    qemu::run_tool("qemu-img", &create, dry_run)?;
    if !dry_run && !vars.is_file() {
        qemu::create_vars(&vm.firmware, &vars)?;
    }
    // Only the machine: the disk is the overlay, never a sized copy.
    let machine = qemu::AutoBuilder {
        image: vm.image.clone(),
        repo: o.repo.clone(),
        out: o.out.clone(),
        firmware: vm.firmware.clone(),
        disk_size_gb: 0,
        memory_mb: vm.memory_mb,
        cpus: vm.cpus,
    };
    let args = qemu::auto_builder_args(&machine, &overlay, &vars);
    println!("+ qemu-system-x86_64 {}", args.join(" "));
    let steps = vm_steps(&o.freebsd_version);
    for (what, cmd, minutes) in &steps {
        println!("   guest: {what} (timeout {minutes} min)\n      {cmd}");
    }
    if dry_run {
        return Ok(());
    }
    let result = qemu::spawn_console(&args, &o.out).and_then(|mut con| {
        qemu::drive_steps(
            &mut con,
            "oci",
            steps.iter().map(|(w, c, m)| (*w, c.as_str(), *m)),
        )
    });
    if !vm.keep {
        println!("+ rm {}", overlay.display());
        std::fs::remove_file(&overlay)
            .with_context(|| format!("removing {}", overlay.display()))?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: &str = "podman run --rm --network none localhost/juanitos-dom0 /bin/sh -c ";

    #[test]
    fn the_containerfile_is_the_tarball() {
        let file = include_str!("../../../release/oci/Containerfile");
        assert!(
            file.contains(&format!(
                "\nFROM scratch\nARG TARBALL={DIST_TARBALL}\nADD ${{TARBALL}} /\n"
            )),
            "{file}"
        );
        assert!(file.contains(&format!("TARBALL={VM_TARBALL}")));
        assert!(file.contains(&format!("-f {CONTAINERFILE} -t {IMAGE} DIR")));
    }

    #[test]
    fn podman_builds_then_checks_in_fresh_containers() {
        let steps = podman_steps("/r/Containerfile", "/ctx dir", "t.txz", "15.1-RELEASE");
        let (what, build, _) = &steps[0];
        assert_eq!(*what, "build the image from the dom0 tarball");
        assert!(build.starts_with("podman rmi -i -f localhost/juanitos-dom0 >/dev/null && "));
        assert!(build.contains(
            "podman build --network none --build-arg TARBALL=t.txz -f /r/Containerfile \
             -t localhost/juanitos-dom0 '/ctx dir'"
        ));
        let checks = checks("15.1-RELEASE");
        assert_eq!(steps.len(), checks.len() + 1);
        for ((what, run, minutes), (name, script)) in steps[1..].iter().zip(&checks) {
            assert_eq!(what, name);
            assert_eq!(*run, format!("{RUN}{}", shell_quote(script)));
            assert!(*minutes > 0);
        }
        let all: String = checks.iter().map(|c| c.1.as_str()).collect();
        assert!(all.contains("case $v in 15.1-RELEASE*) ;;"));
        for p in REQUIRED_PACKAGES {
            assert!(all.contains(p), "{p}");
        }
        for tool in [
            "jos doctor",
            "jos --dry-run start",
            "jos-install --dry-run",
            "jos-greet try-login",
            "jos-splash render",
            "rcorder",
        ] {
            assert!(all.contains(tool), "{tool}");
        }
    }

    /// The scripts survive the quoting: `sh -c` gets them verbatim.
    #[cfg(unix)]
    #[test]
    fn the_scripts_reach_the_container_verbatim() {
        for (_, script) in checks("15.1-RELEASE") {
            let quoted = shell_quote(&script);
            let out = std::process::Command::new("sh")
                .args(["-c", &format!("printf %s {quoted}")])
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), script);
        }
    }

    #[test]
    fn the_vm_mounts_the_shares_and_installs_podman_first() {
        let steps = vm_steps("15.1-RELEASE");
        assert_eq!(steps[0].0, qemu::MOUNT_SHARES.0);
        assert_eq!(steps[1].1, PODMAN_SETUP.1);
        assert_eq!(
            steps[2].1,
            "mkdir -p /var/db/containers/context /var/db/containers/tmp && \
             xz -dc -T0 /mnt/out/context/juanitos-dom0.txz | \
             zstd -T0 -q -f -o /var/db/containers/context/juanitos-dom0.tar.zst"
        );
        assert!(steps[3]
            .1
            .starts_with("(export TMPDIR=/var/db/containers/tmp && podman rmi "));
        // The scratch directory is removed next: later steps must not see it.
        assert!(steps[3].1.ends_with(")"));
        assert!(steps[3].1.contains(
            "--build-arg TARBALL=juanitos-dom0.tar.zst -f /mnt/repo/release/oci/Containerfile \
             -t localhost/juanitos-dom0 /var/db/containers/context"
        ));
        assert_eq!(
            steps[4].1,
            "rm -rf /var/db/containers/context /var/db/containers/tmp"
        );
        assert_eq!(steps[5].0, checks("")[0].0);
        assert_eq!(steps.len(), podman_steps("", "", "", "").len() + 4);
        assert!(PODMAN_SETUP.1.contains("pkg install -y podman-suite"));
        assert!(PODMAN_SETUP
            .1
            .contains("-o compression=zstd -o mountpoint=/var/db/containers zroot/containers"));
    }
}
