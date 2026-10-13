//! End-to-end tests of the `jos` binary in `--dry-run` mode, against a
//! throwaway `JOS_CONFIG_DIR`: the README's quick start, the firewall
//! commands and the error paths. Host commands are only printed, so this
//! runs on any development machine.

// Test helpers outside #[test] functions are test code too.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;
use std::process::{Command, Output};

struct Env {
    dir: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("jos-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Env { dir }
    }

    fn jos(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_jos"))
            .arg("--dry-run")
            .args(args)
            .env("JOS_CONFIG_DIR", &self.dir)
            .output()
            .unwrap()
    }

    /// Run, require success, return stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.jos(args);
        assert!(
            out.status.success(),
            "jos {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Run, require failure, return stderr.
    fn err(&self, args: &[&str]) -> String {
        let out = self.jos(args);
        assert!(!out.status.success(), "jos {args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn qube_file(&self, name: &str) -> PathBuf {
        self.dir.join("qubes").join(format!("{name}.toml"))
    }

    /// The README's qubes: a template, sys-net, a unikernel firewall,
    /// an app qube and a gpu qube.
    fn quick_start(&self) {
        self.ok(&["init"]);
        let out = self.ok(&[
            "create",
            "fbsd15",
            "--kind",
            "template",
            "--os",
            "freebsd",
            "--disk-gb",
            "10",
        ]);
        assert!(out.contains("zfs create -V 10G"), "{out}");
        self.ok(&[
            "create",
            "sys-net",
            "--kind",
            "net",
            "--os",
            "freebsd",
            "--template",
            "fbsd15",
            "--pci",
            "5/0/0",
            "--autostart",
        ]);
        self.ok(&[
            "create",
            "sys-firewall",
            "--kind",
            "firewall",
            "--os",
            "unikernel",
            "--netvm",
            "sys-net",
            "--autostart",
            "--kernel",
            "/usr/local/share/juanitos/jos-firewall.img",
        ]);
        self.ok(&[
            "create",
            "work",
            "--kind",
            "app",
            "--os",
            "freebsd",
            "--template",
            "fbsd15",
            "--firewall",
            "allow tcp any 443",
            "--firewall-policy",
            "deny",
            "--label",
            "blue",
        ]);
        self.ok(&[
            "create",
            "gpu",
            "--kind",
            "gpu",
            "--os",
            "linux",
            "--cpus",
            "8",
            "--memory-mb",
            "16384",
            "--pci",
            "1/0/0",
            "--pci",
            "1/0/1",
            "--ip",
            "10.137.0.50",
            "--no-uefi",
        ]);
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn quick_start_and_listing() {
    let e = Env::new("qs");
    e.quick_start();
    for q in ["fbsd15", "sys-net", "sys-firewall", "work", "gpu"] {
        assert!(e.qube_file(q).is_file(), "{q}");
    }
    let ls = e.ok(&["ls"]);
    assert!(ls.contains("NAME"));
    assert!(ls.contains("sys-firewall"));
    assert!(ls.contains("5/0/0"));
    assert!(ls.contains("10.137.0.50"));
    assert!(ls.contains("1/0/0,1/0/1"));
}

#[test]
fn empty_listing_hints_at_create() {
    let e = Env::new("empty");
    assert!(e.ok(&["ls"]).contains("no qubes defined"));
    let out = e.ok(&["init"]);
    assert!(out.contains("initialized datasets under zroot/juanitos"));
    assert!(e.dir.join("qubes").is_dir());
}

#[test]
fn firewall_rules_and_config() {
    let e = Env::new("fw");
    e.quick_start();
    let show = e.ok(&["firewall", "show", "work"]);
    assert!(show.contains("allow tcp any 443"), "{show}");
    assert!(show.contains("policy deny"));
    let add = e.ok(&["firewall", "add", "work", "allow", "udp", "any", "53"]);
    assert!(add.contains("+ allow udp any 53"));
    let added = e.jos(&["firewall", "add", "work", "allow", "udp", "any", "53"]);
    assert!(String::from_utf8_lossy(&added.stderr).contains("jos stop sys-firewall"));
    let show = e.ok(&["firewall", "show", "work"]);
    assert!(show.contains("udp"));
    e.ok(&["firewall", "policy", "work", "allow"]);
    assert!(e.ok(&["firewall", "show", "work"]).contains("policy allow"));
    e.ok(&["firewall", "clear", "work"]);
    assert!(!e.ok(&["firewall", "show", "work"]).contains("443"));
    assert!(e
        .ok(&["firewall", "show", "sys-net"])
        .contains("no network address"));
    // Templates are clients too (for updates).
    assert!(e.ok(&["firewall", "show", "fbsd15"]).contains("ip 10.137."));

    let cfg = e.ok(&["firewall", "render", "sys-firewall"]);
    assert!(cfg.contains("firewall sys-firewall"));
    assert!(cfg.contains("client work"));
    assert!(e
        .err(&["firewall", "render", "work"])
        .contains("not a firewall qube"));
    assert!(e
        .err(&["firewall", "add", "work", "allow", "bogus"])
        .contains("rror"));
}

#[test]
fn start_supervise_stop_kill_console_in_dry_run() {
    let e = Env::new("run");
    e.quick_start();
    let start = e.ok(&["start", "work"]);
    assert!(start.contains("work:"), "{start}");
    let sup = e.ok(&["supervise", "work", "--max-restarts", "0"]);
    assert!(sup.contains("boot(s)"));
    // The unikernel firewall gets its config disk written before boot.
    let fw = e.ok(&["start", "sys-firewall"]);
    assert!(fw.contains("sys-firewall:"), "{fw}");
    e.ok(&["stop", "work"]);
    e.ok(&["kill", "work"]);
    let console = e.ok(&["console", "work"]);
    assert!(console.contains("+ cu"), "{console}");
    let run = e.ok(&["run", "work", "docker", "ps", "-a"]);
    assert!(run.contains("nmdm-work.1B"), "{run}");
    assert!(run.contains("docker ps -a #JOS_RUN"), "{run}");
    assert!(!run.contains("logging in"), "{run}");
    assert!(e.err(&["run", "ghost", "id"]).contains("ghost"));
    // autostart starts sys-net and sys-firewall.
    let auto = e.ok(&["autostart"]);
    assert!(auto.contains("sys-net:"));
    assert!(auto.contains("sys-firewall:"));
}

#[test]
fn templates_pci_and_removal() {
    let e = Env::new("tpl");
    e.quick_start();
    let snap = e.ok(&["template", "snapshot", "fbsd15"]);
    assert!(snap.contains("fbsd15@base"), "{snap}");
    let snap = e.ok(&["template", "snapshot", "fbsd15", "v2"]);
    assert!(snap.contains("@v2"));
    assert!(e
        .err(&["template", "snapshot", "work"])
        .contains("not a template"));

    let line = e.ok(&["pci", "pptdevs", "1/0/0", "1/0/1"]);
    assert_eq!(line.trim(), "pptdevs=\"1/0/0 1/0/1\"");
    assert!(e.err(&["pci", "pptdevs", "nope"]).contains("nope"));

    let out = e.ok(&["remove", "gpu"]);
    assert!(out.contains("removed gpu"));
    assert!(!e.qube_file("gpu").exists());
    assert!(e.err(&["remove", "gpu"]).contains("gpu"));
}

#[test]
fn dev_templates_from_cloud_images() {
    let e = Env::new("recipes");
    let list = e.ok(&["template", "recipes"]);
    assert!(
        list.contains("debian-13-dev") && list.contains("Docker"),
        "{list}"
    );
    assert!(list.contains("fbsd15-dev") && list.contains("Podman"));

    let out = e.ok(&["template", "install", "debian-13-dev"]);
    assert!(out.contains("debian-13-genericcloud-amd64.tar.xz"), "{out}");
    assert!(out.contains("makefs -t cd9660"));
    assert!(out.contains("29,ahci,cd:"));
    assert!(out.contains("templates/debian-13-dev@base"));
    // Its app qubes are Docker dev boxes.
    let out = e.ok(&[
        "create",
        "docker-dev",
        "--kind",
        "app",
        "--os",
        "linux",
        "--template",
        "debian-13-dev",
    ]);
    assert!(out.contains("created docker-dev"), "{out}");

    let out = e.ok(&[
        "template",
        "install",
        "fbsd15-dev",
        "--name",
        "podman-dev",
        "--disk-gb",
        "60",
        "--no-boot",
    ]);
    assert!(out.contains("BASIC-CLOUDINIT-zfs.raw.xz"), "{out}");
    assert!(out.contains("zfs create -V 60G"));
    assert!(out.contains("--no-boot"));
    assert!(!out.contains("@base"));
    let run = e.ok(&["template", "run", "podman-dev", "--seed", "/tmp/s.iso"]);
    assert!(run.contains("29,ahci,cd:/tmp/s.iso podman-dev"), "{run}");

    let ls = e.ok(&["ls"]);
    let line = ls.lines().find(|l| l.starts_with("podman-dev")).unwrap();
    assert!(
        line.contains("template") && line.contains("sys-firewall"),
        "{line}"
    );

    assert!(e
        .err(&["template", "install", "windows-11"])
        .contains("unknown template recipe"));
    assert!(e
        .err(&["template", "install", "debian-13-dev"])
        .contains("exists"));
    assert!(e
        .err(&["template", "run", "docker-dev"])
        .contains("not a template"));

    // Redox: nightly image resolved through the checksum list, no
    // cloud-init boot, NVMe + e1000 instead of virtio, no console shell.
    assert!(list.contains("redox") && list.contains("Relibc"), "{list}");
    let out = e.ok(&["template", "install", "redox"]);
    assert!(out.contains("static.redox-os.org"), "{out}");
    assert!(out.contains("zfs create -V 4G"));
    assert!(!out.contains("makefs"));
    assert!(!out.contains("ahci"));
    assert!(out.contains("templates/redox@base"));
    let out = e.ok(&[
        "create",
        "rx",
        "--kind",
        "app",
        "--os",
        "redox",
        "--template",
        "redox",
    ]);
    assert!(out.contains("created rx"), "{out}");
    let start = e.ok(&["start", "rx", "--foreground"]);
    assert!(
        start.contains("2,nvme,") && start.contains("3,e1000,tap-rx"),
        "{start}"
    );
    assert!(!start.contains("virtio"));
    let run = e.ok(&["run", "rx", "uname", "-a"]);
    assert!(run.contains("nmdm-rx.1B: uname -a &"), "{run}");
    assert!(run.contains("logging in as user"), "{run}");
    let run = e.ok(&["run", "rx", "--login", "root:password", "id"]);
    assert!(run.contains("logging in as root"), "{run}");
    let ls = e.ok(&["ls"]);
    assert!(ls
        .lines()
        .any(|l| l.starts_with("rx ") && l.contains("redox")));
}

#[test]
fn create_rejects_inconsistent_qubes() {
    let e = Env::new("bad");
    e.quick_start();
    let cases: &[(&[&str], &str)] = &[
        (
            &["create", "work", "--kind", "app", "--os", "freebsd"],
            "exists",
        ),
        (
            &["create", "u1", "--kind", "app", "--os", "unikernel"],
            "--kernel",
        ),
        (
            &[
                "create",
                "u2",
                "--kind",
                "app",
                "--os",
                "unikernel",
                "--kernel",
                "/k",
                "--template",
                "fbsd15",
            ],
            "template",
        ),
        (
            &[
                "create", "u3", "--kind", "app", "--os", "freebsd", "--kernel", "/k",
            ],
            "only valid",
        ),
        (
            &[
                "create",
                "u4",
                "--kind",
                "template",
                "--os",
                "unikernel",
                "--kernel",
                "/k",
            ],
            "cannot be templates",
        ),
        (
            &[
                "create",
                "c1",
                "--kind",
                "app",
                "--os",
                "freebsd",
                "--template",
                "work",
            ],
            "not a template",
        ),
        (
            &[
                "create", "c2", "--kind", "app", "--os", "freebsd", "--netvm", "work",
            ],
            "not a net or firewall",
        ),
        (
            &[
                "create",
                "c3",
                "--kind",
                "app",
                "--os",
                "freebsd",
                "--ip",
                "192.168.1.5",
            ],
            "outside the client network",
        ),
        (
            &[
                "create",
                "c4",
                "--kind",
                "app",
                "--os",
                "freebsd",
                "--ip",
                "10.137.0.50",
            ],
            "already used by gpu",
        ),
        (
            &["create", "Bad Name", "--kind", "app", "--os", "freebsd"],
            "name",
        ),
        (
            &["create", "c5", "--kind", "spaceship", "--os", "freebsd"],
            "spaceship",
        ),
        (
            &[
                "create",
                "c6",
                "--kind",
                "app",
                "--os",
                "freebsd",
                "--template",
                "missing",
            ],
            "template",
        ),
    ];
    for (args, needle) in cases {
        let err = e.err(args);
        assert!(err.contains(needle), "jos {args:?}: {err}");
    }
    // A netvm that is not defined yet is only a warning.
    let out = e.jos(&[
        "create",
        "later",
        "--kind",
        "app",
        "--os",
        "freebsd",
        "--netvm",
        "sys-later",
    ]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not defined yet"));
    // --netvm none: no network at all.
    let out = e.ok(&[
        "create", "offline", "--kind", "app", "--os", "freebsd", "--netvm", "none",
    ]);
    assert!(!out.contains("ip "), "{out}");
    let ls = e.ok(&["ls"]);
    let line = ls.lines().find(|l| l.starts_with("offline")).unwrap();
    assert!(line.contains("none"), "{line}");
    // ...and it is not a client of the firewall.
    let cfg = e.ok(&["firewall", "render", "sys-firewall"]);
    assert!(!cfg.contains("offline"), "{cfg}");
}

#[test]
fn unknown_qubes_and_host_only_commands() {
    let e = Env::new("unk");
    for args in [
        &["start", "ghost"][..],
        &["stop", "ghost"],
        &["kill", "ghost"],
        &["console", "ghost"],
        &["firewall", "show", "ghost"],
    ] {
        assert!(e.err(args).contains("ghost"), "{args:?}");
    }
    // `jos doctor` runs the read-only checks for real; on a development
    // machine they fail, which is reported rather than hidden.
    let out = e.jos(&["doctor"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("kernel module vmm"));
    if !out.status.success() {
        assert!(String::from_utf8_lossy(&out.stderr).contains("some checks failed"));
    }
    // A broken --config file is an error (a missing one means defaults).
    let bad = e.dir.join("bad.toml");
    std::fs::write(&bad, "pool = [").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_jos"))
        .arg("--config")
        .arg(&bad)
        .arg("ls")
        .env("JOS_CONFIG_DIR", &e.dir)
        .output()
        .unwrap();
    assert!(!out.status.success());
}

/// `jos run` for real, with a pseudo-terminal standing in for the nmdm
/// console: the test plays the guest shell on the master side.
#[cfg(unix)]
#[test]
fn run_talks_to_the_console_device() {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;

    let e = Env::new("pty");
    e.ok(&["init"]);
    e.ok(&["create", "work", "--kind", "app", "--os", "freebsd"]);
    e.ok(&[
        "create",
        "fw",
        "--kind",
        "firewall",
        "--os",
        "unikernel",
        "--kernel",
        "/k.img",
    ]);

    // SAFETY: plain libc pty calls on fresh descriptors; the fds are owned
    // by the File created from them below.
    let (master, _slave_keepalive, slave_path) = unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0, "posix_openpt");
        assert_eq!(libc::grantpt(m), 0);
        assert_eq!(libc::unlockpt(m), 0);
        let name = libc::ptsname(m);
        assert!(!name.is_null());
        let path = std::ffi::CStr::from_ptr(name)
            .to_string_lossy()
            .into_owned();
        // The slave is left in its default cooked mode (echo on): like an
        // nmdm device it would echo the guest's output back to the guest
        // unless `jos run` switches it to raw mode itself.
        let s = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
        assert!(s >= 0);
        // Keeping the slave open: on Linux a master without any slave
        // open reads EIO.
        (
            std::fs::File::from_raw_fd(m),
            std::fs::File::from_raw_fd(s),
            path,
        )
    };
    let consoles = e.dir.join("consoles");
    std::fs::create_dir_all(&consoles).unwrap();
    std::os::unix::fs::symlink(&slave_path, consoles.join("nmdm-work.1B")).unwrap();
    let cfg = e.dir.join("jos.toml");
    std::fs::write(
        &cfg,
        format!("console_dir = {:?}\n", consoles.to_string_lossy()),
    )
    .unwrap();

    // The "guest": wait for the typed script, answer with output and the
    // end marker carrying exit status 3.
    let mut guest = master.try_clone().unwrap();
    let shell = std::thread::spawn(move || {
        let mut seen = String::new();
        let mut buf = [0u8; 256];
        loop {
            let n = guest.read(&mut buf).unwrap();
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            if seen.contains("$?\r") {
                break;
            }
        }
        // Echo the typed lines like a terminal, the command's output in
        // between, then the marker.
        let (cmd_line, marker_line) = seen.trim_start_matches('\r').split_once('\r').unwrap();
        let (cmd_line, marker_line) = (cmd_line.to_string(), marker_line.to_string());
        guest.write_all(cmd_line.as_bytes()).unwrap();
        guest.write_all(b"\r\nhello from the guest\r\n$ ").unwrap();
        guest.write_all(marker_line.as_bytes()).unwrap();
        guest.write_all(b"\nJOS_RUN_END 3\r\n").unwrap();
        // Keep listening briefly: anything arriving now would be the
        // host side echoing our output back at us.
        // SAFETY: fcntl on a descriptor this thread owns.
        unsafe {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&guest);
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
        let until = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < until {
            match guest.read(&mut buf) {
                Ok(n) if n > 0 => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
                _ => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        seen
    });

    let out = Command::new(env!("CARGO_BIN_EXE_jos"))
        .arg("--config")
        .arg(&cfg)
        .args(["run", "work", "--timeout", "20", "echo", "hello"])
        .env("JOS_CONFIG_DIR", &e.dir)
        .output()
        .unwrap();
    let typed = shell.join().unwrap();
    assert!(typed.contains("echo hello #JOS_RUN\rprintf"), "{typed}");
    assert!(
        !typed.contains("hello from the guest"),
        "guest output was echoed back as input:\n{typed}"
    );
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello from the guest\n"
    );

    // No device: a clear error. A unikernel: refused before any I/O.
    std::fs::remove_file(consoles.join("nmdm-work.1B")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_jos"))
        .arg("--config")
        .arg(&cfg)
        .args(["run", "work", "true"])
        .env("JOS_CONFIG_DIR", &e.dir)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is the qube running?"));
    let out = Command::new(env!("CARGO_BIN_EXE_jos"))
        .arg("--config")
        .arg(&cfg)
        .args(["run", "fw", "true"])
        .env("JOS_CONFIG_DIR", &e.dir)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("no shell on its serial console"));
}
