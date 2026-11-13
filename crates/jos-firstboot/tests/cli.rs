// SPDX-License-Identifier: BSD-3-Clause
//! `jos-firstboot` as a program: the exit status and error report of `main`.

// Test helpers outside #[test] functions are test code too.
#![allow(clippy::unwrap_used)]

use std::process::Command;

#[test]
fn success_and_failure_exit_codes() {
    let dir = std::env::temp_dir().join(format!("jos-firstboot-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("firstboot.done");
    std::fs::write(&marker, "Done\n").unwrap();

    let ok = Command::new(env!("CARGO_BIN_EXE_jos-firstboot"))
        .args(["run", "--marker"])
        .arg(&marker)
        .output()
        .unwrap();
    assert!(ok.status.success());
    assert!(String::from_utf8_lossy(&ok.stdout).contains("already done"));

    // A file where the screenshots directory should be.
    let failed = Command::new(env!("CARGO_BIN_EXE_jos-firstboot"))
        .arg("screenshots")
        .arg(&marker)
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&failed.stderr).starts_with("jos-firstboot: "));
    let _ = std::fs::remove_dir_all(&dir);
}
