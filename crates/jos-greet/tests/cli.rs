//! `jos-greet` as a program: the exit status and error report of `main`.

// Test helpers outside #[test] functions are test code too.
#![allow(clippy::unwrap_used)]

use std::process::Command;

#[test]
fn success_and_failure_exit_codes() {
    let dir = std::env::temp_dir().join(format!("jos-greet-cli-{}", std::process::id()));
    let ok = Command::new(env!("CARGO_BIN_EXE_jos-greet"))
        .args(["sessions", "--dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(ok.status.success());
    assert!(String::from_utf8_lossy(&ok.stdout).starts_with("COSMIC\t"));

    let failed = Command::new(env!("CARGO_BIN_EXE_jos-greet"))
        .args(["try-login", "--user", "alice", "--", "true"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&failed.stderr).starts_with("jos-greet: "));
}
