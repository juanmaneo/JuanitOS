//! The `jos-install` binary end to end: what `main` exits with and says.
//! `PATH` is an empty directory, so no tool of the machine running the
//! tests is ever started: every host query fails, as on a machine
//! without FreeBSD's tools.

// Test helpers outside #[test] functions are test code too.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A scratch directory for one test, created empty.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jos-install-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("empty-path")).unwrap();
    dir
}

fn jos_install(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_jos-install"))
        .args(args)
        .env("PATH", dir.join("empty-path"))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn errors_exit_1_except_on_the_live_medium() {
    let dir = scratch("errors");
    let args = ["--dry-run", "--non-interactive", "--disk", "vtbd0"];
    let out = jos_install(&dir, &args);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.starts_with("jos-install: host checks failed"), "{err}");
    // rc.local goes on to the console login either way.
    let live = jos_install(&dir, &[&args[..], &["--live"]].concat());
    assert_eq!(live.status.code(), Some(0));
    assert_eq!(stderr(&live), err);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dry_run_only_prints_what_it_would_do() {
    let dir = scratch("dry");
    let target = dir.join("target");
    let out = jos_install(
        &dir,
        &[
            "--dry-run",
            "--non-interactive",
            "--ignore-doctor",
            "--disk",
            "vtbd0",
            "--root-password",
            "pw",
            "--target",
            &target.to_string_lossy(),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("JuanitOS installer"), "{stdout}");
    assert!(stdout.contains("+ gpart create -s gpt vtbd0"), "{stdout}");
    assert!(!target.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_real_run_needs_the_dom0_distribution() {
    let dir = scratch("real");
    let none = dir.join("no-dist");
    let out = jos_install(
        &dir,
        &["--non-interactive", "--dist", &none.to_string_lossy()],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("does not contain the dom0 distribution"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn screenshots_of_every_page() {
    let dir = scratch("shots");
    let shots = dir.join("shots");
    let out = jos_install(&dir, &["--screenshots", &shots.to_string_lossy()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.lines().count(), 9, "{stdout}");
    assert!(shots.join("installer-1-welcome.png").is_file());
    // Not into a file.
    let file = dir.join("a-file");
    std::fs::write(&file, b"").unwrap();
    let out = jos_install(&dir, &["--screenshots", &file.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).starts_with("jos-install: "));
    let _ = std::fs::remove_dir_all(&dir);
}
