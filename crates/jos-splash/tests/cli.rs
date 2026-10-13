//! The `jos-splash` binary: PNG rendering (used by jos-release for the
//! kernel splash) and the client commands, which must be harmless when no
//! daemon is running (text boot, or splash already finished).

// Test helpers outside #[test] functions are test code too.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::process::Command;

fn splash(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_jos-splash"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn renders_screens_and_cards() {
    let dir = std::env::temp_dir().join(format!("jos-splash-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let screen = dir.join("screen.png");
    let out = splash(&[
        "render",
        screen.to_str().unwrap(),
        "--width",
        "800",
        "--height",
        "600",
        "--stage",
        "network",
        "--status",
        "Starting qubes",
    ]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("(800x600)"));
    assert!(std::fs::metadata(&screen).unwrap().len() > 100);

    let card = dir.join("card.png");
    let out = splash(&[
        "render",
        card.to_str().unwrap(),
        "--card",
        "--stage",
        "none",
    ]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("(720x420)"));

    let out = splash(&["render", card.to_str().unwrap(), "--stage", "warp"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown stage"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clients_without_a_daemon_are_no_ops() {
    let sock = std::env::temp_dir().join(format!("jos-splash-none-{}.sock", std::process::id()));
    let s = sock.to_str().unwrap();
    for args in [
        vec!["--socket", s, "stage", "network"],
        vec!["--socket", s, "status", "Starting", "qubes"],
        vec!["--socket", s, "finish"],
        vec!["--socket", s, "finish", "--console"],
    ] {
        let out = splash(&args);
        assert!(out.status.success(), "{args:?}: {:?}", out);
    }
    let out = splash(&["--socket", s, "stage", "bogus"]);
    assert!(!out.status.success());
}

#[cfg(unix)]
#[test]
fn dry_run_daemon_serves_clients() {
    if std::os::unix::net::UnixDatagram::unbound().is_err() {
        eprintln!("skipped: Unix sockets are not permitted here");
        return;
    }
    let sock = std::env::temp_dir().join(format!("jos-splash-dry-{}.sock", std::process::id()));
    let s = sock.to_str().unwrap().to_string();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_jos-splash"))
        .args(["--socket", &s, "run", "--dry-run", "--reached", "storage"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(splash(&["--socket", &s, "stage", "qubes"]).status.success());
    assert!(splash(&["--socket", &s, "finish", "--console"])
        .status
        .success());
    // Never hang the test suite on a daemon that missed the message.
    for _ in 0..500 {
        if daemon.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if daemon.try_wait().unwrap().is_none() {
        let _ = daemon.kill();
        panic!("the dry-run daemon did not exit after `finish`");
    }
    let out = daemon.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("console restored"), "{text}");
}
