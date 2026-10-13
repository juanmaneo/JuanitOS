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

#[test]
fn bad_arguments_are_rejected() {
    let w = Work::new("bad");
    let (ok, _) = w.run(&["no-such-step"]);
    assert!(!ok);
    let (ok, out) = w.run(&["--profile", "/nonexistent/profile.toml", "fetch"]);
    assert!(!ok, "{out}");
}
