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
    let nowhere = dir.join("missing/card.png");
    let out = splash(&["render", nowhere.to_str().unwrap(), "--card"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("jos-splash: "));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clients_without_a_daemon_are_no_ops() {
    let sock = std::env::temp_dir().join(format!("jos-splash-none-{}.sock", std::process::id()));
    let s = sock.to_str().unwrap();
    for args in [
        vec!["--socket", s, "stage", "network"],
        vec!["--socket", s, "leave", "qubes"],
        vec!["--socket", s, "status", "Starting", "qubes"],
        vec!["--socket", s, "finish"],
        vec!["--socket", s, "finish", "--console"],
    ] {
        let out = splash(&args);
        assert!(out.status.success(), "{args:?}: {:?}", out);
    }
    for verb in ["stage", "leave"] {
        for bad in ["bogus", "none"] {
            let out = splash(&["--socket", s, verb, bad]);
            assert!(!out.status.success(), "{verb} {bad}");
        }
    }
}

#[cfg(unix)]
#[test]
fn a_socket_we_may_not_write_fails_the_client() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("jos-splash-perm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("s.sock");
    let _listener = std::os::unix::net::UnixDatagram::bind(&sock).unwrap();
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o000)).unwrap();
    let out = splash(&["--socket", sock.to_str().unwrap(), "stage", "network"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("sending to"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn the_daemon_refuses_what_it_cannot_do() {
    let dir = std::env::temp_dir().join(format!("jos-splash-refuse-{}", std::process::id()));
    let sock = dir.join("missing/s.sock");
    let s = sock.to_str().unwrap();
    for (args, why) in [
        (vec!["run", "--reached", "warp"], "unknown stage"),
        (vec!["--socket", s, "run", "--dry-run"], "binding"),
        #[cfg(not(target_os = "freebsd"))]
        (vec!["--socket", s, "run"], "only exists on FreeBSD"),
    ] {
        let out = splash(&args);
        assert!(!out.status.success(), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(why), "{args:?}: {err}");
    }
}

#[cfg(unix)]
#[test]
fn dry_run_daemon_serves_clients() {
    assert!(dry_run_daemon("console", &["finish", "--console"]).contains("console restored"));
    assert!(dry_run_daemon("keep", &["finish"]).contains("console kept"));
}

/// Start `run --dry-run`, light a stage, end it with `finish`, and return
/// what the daemon printed.
#[cfg(unix)]
fn dry_run_daemon(tag: &str, finish: &[&str]) -> String {
    let sock =
        std::env::temp_dir().join(format!("jos-splash-dry-{tag}-{}.sock", std::process::id()));
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
    assert!(splash(&["--socket", &s, "leave", "qubes"]).status.success());
    let finish: Vec<&str> = ["--socket", &s]
        .into_iter()
        .chain(finish.iter().copied())
        .collect();
    assert!(splash(&finish).status.success());
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}
