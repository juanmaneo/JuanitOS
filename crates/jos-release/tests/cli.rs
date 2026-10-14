//! `jos-release --dry-run` end to end: every subcommand prints the host
//! commands it would run, so the whole release pipeline is exercised on a
//! development machine without FreeBSD tools, root or network.

// Test helpers outside #[test] functions are test code too.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;
use std::process::Command;

struct Work(PathBuf);

impl Work {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("jos-release-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Work(dir)
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_jos-release"))
            .arg("--dry-run")
            .arg("--work")
            .arg(&self.0)
            .args(args)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    fn ok(&self, args: &[&str]) -> String {
        let (ok, text) = self.run(args);
        assert!(ok, "jos-release {args:?} failed:\n{text}");
        text
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn fetch_verifies_against_the_manifest() {
    let w = Work::new("fetch");
    let out = w.ok(&["fetch"]);
    assert!(out.contains("15.1-RELEASE/base.txz"), "{out}");
    assert!(out.contains("kernel.txz"));
}

#[test]
fn full_pipeline() {
    let w = Work::new("all");
    let out = w.ok(&["all"]);
    for needle in [
        "Fetching FreeBSD",
        "Building release binaries",
        "Building dom0 root",
        "Installing packages",
        "Packing",
        "juanitos-dom0.txz",
        "Building",
        "Done:",
    ] {
        assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
    }
}

#[test]
fn individual_steps() {
    let w = Work::new("steps");
    assert!(w.ok(&["bins"]).contains("cargo build --release"));
    assert!(w.ok(&["rootfs"]).contains("release.toml"));
    assert!(w.ok(&["pack"]).contains("tar -C"));
    assert!(w.ok(&["iso"]).contains("Done:"));
    let out = w.ok(&["clean"]);
    assert!(out.contains("+ rm -rf"), "{out}");
    assert!(w.0.is_dir(), "a dry run removes nothing");
}

#[test]
fn qemu_boots_the_iso_or_the_disk() {
    let w = Work::new("qemu");
    let (ok, out) = w.run(&["qemu"]);
    if !ok && out.contains("OVMF") {
        eprintln!("skipped: no OVMF firmware on this host");
        return;
    }
    assert!(ok, "{out}");
    assert!(out.contains("qemu-system-x86_64"));
    assert!(out.contains("media=cdrom"));
    let out = w.ok(&["qemu", "--no-cdrom"]);
    assert!(!out.contains("media=cdrom"));
}

#[test]
fn builder_vm_commands() {
    let w = Work::new("builder");
    let out = w.ok(&["builder", "--image", "/images/freebsd.qcow2"]);
    assert!(out.contains("mount_tag=juanitos"), "{out}");
    let b = w.0.join("b");
    let b_s = b.to_string_lossy().into_owned();
    let (ok, out) = w.run(&[
        "builder",
        "--image",
        "/images/freebsd.qcow2",
        "--auto",
        "--out",
        &b_s,
        "--cpus",
        "2",
        "--memory-mb",
        "2048",
    ]);
    if !ok && out.contains("OVMF") {
        eprintln!("skipped: no OVMF firmware on this host");
        return;
    }
    assert!(ok, "{out}");
    assert!(out.contains("qemu-img convert"));
    assert!(out.contains("guest: build the release binaries"));
    assert!(out.contains("-smp 2 -m 2048"));
}

/// `jos-release` without `--dry-run`, from `cwd`. Only for paths that stop
/// before running anything on the host.
fn real(cwd: &std::path::Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_jos-release"))
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn qemu_with_a_viommu_explains_the_passthrough_setup() {
    let w = Work::new("viommu");
    let repo = repo_root();
    let out = w.ok(&[
        "--repo",
        &repo.to_string_lossy(),
        "qemu",
        "--iommu",
        "--headless",
        "--firmware",
        "/f/OVMF_CODE.fd",
    ]);
    assert!(
        out.contains("vIOMMU; inside the dom0: pptdevs=\"0/9/0\""),
        "{out}"
    );
    assert!(out.contains("--pci 0/9/0"));
    assert!(out.contains("kernel-irqchip=split"));
    assert!(out.contains("-display none"));
    // Nothing exists yet: the disk and the variable store would be created.
    assert!(out.contains("+ qemu-img create -f qcow2"));
    assert!(out.contains("+ cp (empty)"));
}

#[test]
fn the_repository_is_found_or_must_be_given() {
    let w = Work::new("norepo");
    let (ok, out) = real(&w.0, &["--dry-run", "fetch"]);
    assert!(!ok);
    assert!(out.contains("not inside the JuanitOS repository"), "{out}");
}

#[test]
fn qemu_refuses_an_iso_that_was_not_built() {
    let w = Work::new("noiso");
    let work = w.0.to_string_lossy().into_owned();
    let (ok, out) = real(
        &repo_root(),
        &["--work", &work, "qemu", "--firmware", "/f/OVMF_CODE.fd"],
    );
    assert!(!ok);
    assert!(out.contains("does not exist; build it first"), "{out}");
    assert!(!out.contains("qemu-system"), "nothing was started: {out}");
}

#[test]
fn release_steps_need_root() {
    let id = Command::new("id").arg("-u").output();
    if id.map(|o| o.stdout.starts_with(b"0\n")).unwrap_or(true) {
        eprintln!("skipped: running as root (or no id), the step would run for real");
        return;
    }
    let w = Work::new("root");
    let work = w.0.to_string_lossy().into_owned();
    for step in ["rootfs", "pack", "iso", "all"] {
        let (ok, out) = real(&repo_root(), &["--work", &work, step]);
        assert!(!ok, "{step}");
        assert!(out.contains("must run as root"), "{step}: {out}");
    }
}

#[test]
fn bad_arguments_are_rejected() {
    let w = Work::new("bad");
    let (ok, _) = w.run(&["no-such-step"]);
    assert!(!ok);
    let (ok, out) = w.run(&["--profile", "/nonexistent/profile.toml", "fetch"]);
    assert!(!ok, "{out}");
}
