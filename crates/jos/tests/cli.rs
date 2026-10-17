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
    // The agent channel by default, the console on request.
    let run = e.ok(&["run", "work", "docker", "ps", "-a"]);
    assert!(run.contains("ask the agent on"), "{run}");
    assert!(run.contains("work.agent"), "{run}");
    assert!(run.contains(" user docker ps -a"), "{run}");
    let run = e.ok(&["run", "work", "--user", "root", "id"]);
    assert!(run.contains(" root id"), "{run}");
    assert!(e
        .err(&["run", "work", "--user", "a b", "id"])
        .contains("invalid user"));
    let run = e.ok(&["run", "work", "--console", "docker", "ps", "-a"]);
    assert!(run.contains("nmdm-work.1B"), "{run}");
    assert!(run.contains(": JOS_RUN; docker ps -a; printf"), "{run}");
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
    assert!(out.contains("rm -rf /var/db/juanitos/share/gpu"), "{out}");
    assert!(!e.qube_file("gpu").exists());
    assert!(e.err(&["remove", "gpu"]).contains("gpu"));
    // A unikernel has no share to remove.
    let out = e.ok(&["remove", "sys-firewall"]);
    assert!(!out.contains("share"), "{out}");
}

#[test]
fn start_exports_the_share_and_copy_moves_files() {
    let e = Env::new("copy");
    e.quick_start();
    // Booting prepares the share and attaches it on slot 28.
    let start = e.ok(&["start", "work"]);
    assert!(
        start.contains("mkdir -p /var/db/juanitos/share/work/in /var/db/juanitos/share/work/out"),
        "{start}"
    );
    assert!(
        start.contains("tee /var/db/juanitos/share/work/name"),
        "{start}"
    );
    assert!(
        start.contains("28,virtio-9p,jos=/var/db/juanitos/share/work"),
        "{start}"
    );
    // The unikernel firewall gets none of it.
    let fw = e.ok(&["start", "sys-firewall"]);
    assert!(!fw.contains("virtio-9p"), "{fw}");

    // Qube to qube: the source exports through its agent, dom0 moves.
    let out = e.ok(&["copy", "work:/etc/motd", "gpu"]);
    assert!(
        out.contains("ask the agent on /var/run/juanitos/work.agent"),
        "{out}"
    );
    assert!(out.contains("cp -RL /etc/motd /mnt/jos/out/motd"), "{out}");
    assert!(
        out.contains(
            "mv /var/db/juanitos/share/work/out/motd /var/db/juanitos/share/gpu/in/work/motd"
        ),
        "{out}"
    );
    assert!(
        out.contains("copied work:/etc/motd to gpu:/mnt/jos/in/work/motd"),
        "{out}"
    );

    // dom0 to qube, with and without the dom0: prefix.
    let out = e.ok(&["copy", "dom0:/root/notes", "work"]);
    assert!(
        out.contains("cp -RL /root/notes /var/db/juanitos/share/work/in/dom0/notes"),
        "{out}"
    );
    assert!(!out.contains("ask the agent"), "{out}");
    let out = e.ok(&["copy", "/root/notes", "work"]);
    assert!(
        out.contains("copied dom0:/root/notes to work:/mnt/jos/in/dom0/notes"),
        "{out}"
    );

    // Refusals: unknown qubes, qubes without a share, same qube, bad paths.
    assert!(e.err(&["copy", "ghost:/x", "work"]).contains("ghost"));
    assert!(e.err(&["copy", "work:/x", "ghost"]).contains("ghost"));
    assert!(e
        .err(&["copy", "sys-firewall:/x", "work"])
        .contains("no dom0 share"));
    assert!(e
        .err(&["copy", "work:/x", "sys-firewall"])
        .contains("no dom0 share"));
    assert!(e.err(&["copy", "work:/x", "work"]).contains("same qube"));
    assert!(e.err(&["copy", "work:/", "gpu"]).contains("name a file"));
    assert!(e.err(&["copy", "work:", "gpu"]).contains("missing path"));
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
    assert!(out.contains("debian-13-generic-amd64.tar.xz"), "{out}");
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

/// Stop a `jos serve` daemon the way `rc.d/jos` does (SIGTERM, which the
/// daemon turns into a clean exit within half a second) and return its
/// log; SIGKILL only if it does not oblige.
fn terminate(mut daemon: std::process::Child) -> String {
    let _ = Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .status();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while daemon.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if daemon.try_wait().unwrap().is_none() {
        daemon.kill().unwrap();
    }
    let out = daemon.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A pseudo-terminal standing in for a qube's nmdm console: the master
/// side is returned for the test to play the guest shell on, the slave is
/// linked as `nmdm-<name>.1B` under `consoles/`. The second file keeps
/// the slave open (on Linux a master without any slave open reads EIO).
#[cfg(unix)]
fn fake_console(e: &Env, name: &str) -> (std::fs::File, std::fs::File) {
    use std::os::fd::FromRawFd;
    // SAFETY: plain libc pty calls on fresh descriptors; the fds are owned
    // by the File created from them below.
    let (master, keepalive, slave_path) = unsafe {
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
        (
            std::fs::File::from_raw_fd(m),
            std::fs::File::from_raw_fd(s),
            path,
        )
    };
    let consoles = e.dir.join("consoles");
    std::fs::create_dir_all(&consoles).unwrap();
    std::os::unix::fs::symlink(&slave_path, consoles.join(format!("nmdm-{name}.1B"))).unwrap();
    (master, keepalive)
}

/// Play a guest shell on `master`: wait for the script `jos run` types,
/// hand the command line to `on_command` (which returns the exit status
/// to report), echo it like a terminal, print `output`, then the end
/// marker. Returns everything received, for the test to check that the
/// host did not echo the guest's output back.
#[cfg(unix)]
fn fake_guest(
    mut guest: std::fs::File,
    output: &'static str,
    on_command: impl FnOnce(&str) -> i32 + Send + 'static,
) -> std::thread::JoinHandle<String> {
    use std::io::{Read, Write};
    std::thread::spawn(move || {
        let mut seen = String::new();
        let mut buf = [0u8; 256];
        loop {
            let n = guest.read(&mut buf).unwrap();
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            if seen.contains("$?\r") {
                break;
            }
        }
        let (line, _) = seen.trim_start_matches('\r').split_once('\r').unwrap();
        let line = line.to_string();
        let status = on_command(&line);
        guest.write_all(line.as_bytes()).unwrap();
        guest.write_all(format!("\r\n{output}").as_bytes()).unwrap();
        guest
            .write_all(format!("\nJOS_RUN_END {status}\r\n").as_bytes())
            .unwrap();
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
        // Blocking again: the flag lives on the open file description,
        // shared with the next guest's clone of the master.
        // SAFETY: as above.
        unsafe {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&guest);
            libc::fcntl(fd, libc::F_SETFL, 0);
        }
        seen
    })
}

/// `jos run` for real, with a pseudo-terminal standing in for the nmdm
/// console: the test plays the guest shell on the master side.
#[cfg(unix)]
#[test]
fn run_talks_to_the_console_device() {
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
    let (master, _keepalive) = fake_console(&e, "work");
    let consoles = e.dir.join("consoles");
    let cfg = e.dir.join("jos.toml");
    std::fs::write(
        &cfg,
        format!("console_dir = {:?}\n", consoles.to_string_lossy()),
    )
    .unwrap();

    // The "guest" answers with output and exit status 3.
    let shell = fake_guest(
        master.try_clone().unwrap(),
        "hello from the guest\r\n",
        |_| 3,
    );

    let out = Command::new(env!("CARGO_BIN_EXE_jos"))
        .arg("--config")
        .arg(&cfg)
        .args(["run", "work", "--timeout", "20", "echo", "hello"])
        .env("JOS_CONFIG_DIR", &e.dir)
        .output()
        .unwrap();
    let typed = shell.join().unwrap();
    assert!(typed.contains(": JOS_RUN; echo hello; printf"), "{typed}");
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

/// Play `jos-agentd` on a UNIX socket standing in for the one bhyve
/// listens on: accept dom0, answer the ping (or not), run `on_command`
/// for the RUN line, print `output` and the end line. Returns what dom0
/// sent.
#[cfg(unix)]
fn fake_agent(
    listener: std::os::unix::net::UnixListener,
    answer_ping: bool,
    output: &'static str,
    on_command: impl FnOnce(&str) -> i32 + Send + 'static,
) -> std::thread::JoinHandle<String> {
    use std::io::{BufRead, Write};
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut stream = stream;
        let mut seen = String::new();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        seen.push_str(&line);
        let id = line.trim().strip_prefix("PING ").unwrap().to_string();
        if !answer_ping {
            // Like bhyve with no daemon behind it: the connection stays
            // open and nothing ever comes back.
            std::thread::sleep(std::time::Duration::from_secs(5));
            return seen;
        }
        stream.write_all(format!("PONG {id}\n").as_bytes()).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        seen.push_str(&line);
        let rest = line.trim().strip_prefix("RUN ").unwrap();
        let (rid, rest) = rest.split_once(' ').unwrap();
        assert_eq!(rid, id);
        let (_user, command) = rest.split_once(' ').unwrap();
        let status = on_command(command);
        stream.write_all(output.as_bytes()).unwrap();
        stream
            .write_all(format!("\nEND {id} {status}\n").as_bytes())
            .unwrap();
        seen
    })
}

/// `jos run` for real through the agent channel, then its fallback to the
/// console when nothing answers the ping.
#[cfg(unix)]
#[test]
fn run_talks_to_the_agent_socket() {
    let e = Env::new("agent");
    e.ok(&["init"]);
    e.ok(&["create", "work", "--kind", "app", "--os", "linux"]);
    let run_dir = e.dir.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let consoles = e.dir.join("consoles");
    let cfg = e.dir.join("jos.toml");
    std::fs::write(
        &cfg,
        format!(
            "run_dir = {:?}\nconsole_dir = {:?}\n",
            run_dir.to_string_lossy(),
            consoles.to_string_lossy()
        ),
    )
    .unwrap();
    let jos = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_jos"))
            .arg("--config")
            .arg(&cfg)
            .args(args)
            .env("JOS_CONFIG_DIR", &e.dir)
            .output()
            .unwrap()
    };

    // Not running: no socket, a clear error.
    let out = jos(&["run", "work", "true"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is the qube running?"));

    let sock = run_dir.join("work.agent");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let agent = fake_agent(listener, true, "hello from the agent\n", |cmd| {
        assert_eq!(cmd, "echo hello");
        3
    });
    let out = jos(&[
        "run",
        "work",
        "--user",
        "root",
        "--timeout",
        "20",
        "echo",
        "hello",
    ]);
    let sent = agent.join().unwrap();
    assert!(sent.contains(" root echo hello\n"), "{sent}");
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello from the agent\n"
    );

    // No daemon behind the socket: the console is used instead.
    std::fs::remove_file(&sock).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let agent = fake_agent(listener, false, "", |_| 0);
    let (master, _keepalive) = fake_console(&e, "work");
    let shell = fake_guest(master.try_clone().unwrap(), "via console\r\n", |_| 0);
    let out = jos(&["run", "work", "--timeout", "20", "echo", "hello"]);
    agent.join().unwrap();
    let typed = shell.join().unwrap();
    assert!(typed.contains(": JOS_RUN; echo hello; printf"), "{typed}");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "via console\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no agent answered"));
}

/// `jos policy`, `jos requests` and `jos serve --once` for real against a
/// `share_dir` on disk: requests are files a guest dropped through 9p,
/// here written on the host side.
#[test]
fn serve_answers_guest_requests_by_policy() {
    let e = Env::new("serve");
    e.ok(&["init"]);
    e.ok(&["create", "work", "--kind", "app", "--os", "freebsd"]);
    e.ok(&["create", "web", "--kind", "app", "--os", "linux"]);
    e.ok(&["create", "vault", "--kind", "app", "--os", "freebsd"]);
    let share = e.dir.join("share");
    let cfg = e.dir.join("jos.toml");
    std::fs::write(&cfg, format!("share_dir = {:?}\n", share.to_string_lossy())).unwrap();
    let jos = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_jos"))
            .arg("--config")
            .arg(&cfg)
            .args(args)
            .env("JOS_CONFIG_DIR", &e.dir)
            .output()
            .unwrap()
    };
    let ok = |args: &[&str]| {
        let out = jos(args);
        assert!(out.status.success(), "jos {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // Policy: empty, then two rules; bad rules are refused.
    assert!(ok(&["policy", "show"]).contains("no rules"));
    assert!(ok(&["policy", "add", "work", "*", "allow"]).contains("+ copy work * allow"));
    ok(&["policy", "add", "*", "vault", "deny"]);
    assert!(!jos(&["policy", "add", "work", "web", "maybe"])
        .status
        .success());
    assert!(!jos(&["policy", "add", "Work", "web", "allow"])
        .status
        .success());
    let shown = ok(&["policy", "show"]);
    assert!(shown.contains("1  copy work * allow"), "{shown}");
    assert!(shown.contains("2  copy * vault deny"), "{shown}");
    assert!(e.dir.join("policy").is_file());

    // Three requests: allowed, denied, and one nobody has a rule for.
    let req = |qube: &str, id: &str, body: &str| {
        let dir = share.join(qube).join("req");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(id), body).unwrap();
    };
    std::fs::create_dir_all(share.join("work").join("out")).unwrap();
    std::fs::write(share.join("work").join("out").join("notes"), "n\n").unwrap();
    req("work", "1", "copy notes web\n");
    req("web", "2", "copy secret vault\n");
    req("web", "3", "copy report work\n");
    let listed = ok(&["requests"]);
    assert!(listed.contains("copy notes -> web [allow]"), "{listed}");
    assert!(listed.contains("copy secret -> vault [deny]"), "{listed}");
    assert!(listed.contains("copy report -> work [ask]"), "{listed}");

    let served = ok(&["serve", "--once"]);
    assert!(
        served.contains("work 1: copy notes -> web: delivered to /mnt/jos/in/work/notes"),
        "{served}"
    );
    assert!(
        served.contains("web 2: copy secret -> vault: denied"),
        "{served}"
    );
    assert!(
        served.contains("web 3: copy report -> work: pending"),
        "{served}"
    );
    assert_eq!(
        std::fs::read_to_string(share.join("web").join("in").join("work").join("notes")).unwrap(),
        "n\n"
    );
    let reply = |qube: &str, id: &str| {
        std::fs::read_to_string(share.join(qube).join("req").join(format!("{id}.reply")))
    };
    assert_eq!(reply("work", "1").unwrap(), "ok /mnt/jos/in/work/notes\n");
    assert!(!share.join("work").join("req").join("1").exists());
    assert_eq!(reply("web", "2").unwrap(), "denied: policy\n");
    assert!(reply("web", "3").is_err(), "pending requests get no reply");

    // The operator settles the pending one; the outbox entry is missing,
    // so the guest hears about it.
    let listed = ok(&["requests", "ls"]);
    assert!(
        listed.contains("web") && !listed.contains("notes"),
        "{listed}"
    );
    let out = jos(&["requests", "approve", "web", "9"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no pending request 9"));
    let approved = ok(&["requests", "approve", "web", "3"]);
    assert!(approved.contains("failed"), "{approved}");
    assert!(reply("web", "3").unwrap().starts_with("error: "));
    req("web", "4", "copy report work\n");
    assert!(ok(&["requests", "deny", "web", "4"]).contains("denied"));
    assert_eq!(reply("web", "4").unwrap(), "denied: operator\n");

    // Malformed requests are answered too, so the guest does not wait.
    req("web", "5", "exec rm -rf /\n");
    let listed = ok(&["requests"]);
    assert!(
        listed
            .lines()
            .any(|l| l.starts_with("web") && l.contains(" 5 ") && l.contains("invalid: ")),
        "{listed}"
    );
    let served = ok(&["serve", "--once"]);
    assert!(served.contains("web 5: failed"), "{served}");
    assert!(reply("web", "5").unwrap().starts_with("error: "));
    assert!(ok(&["requests"]).contains("no pending requests"));
    assert!(ok(&["serve", "--once"]).is_empty());

    // A dry run shows the plan instead.
    req("work", "6", "copy notes web\n");
    let dry = ok(&["--dry-run", "serve", "--once"]);
    assert!(dry.contains("+ mv") && dry.contains("+ tee"), "{dry}");
    assert!(share.join("work").join("req").join("6").exists());
    ok(&["policy", "clear"]);
    assert!(ok(&["policy", "show"]).contains("no rules"));
    assert!(ok(&["requests"]).contains("[ask]"));

    // The daemon: request 6 waits (no rules); a rule added while it runs
    // is picked up on the next pass and the request delivered.
    std::fs::write(share.join("work").join("out").join("notes"), "n\n").unwrap();
    let daemon = Command::new(env!("CARGO_BIN_EXE_jos"))
        .arg("--config")
        .arg(&cfg)
        .args(["serve", "--interval", "1"])
        .env("JOS_CONFIG_DIR", &e.dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(reply("work", "6").is_err(), "still pending");
    ok(&["policy", "add", "work", "web", "allow"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while reply("work", "6").is_err() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let log = terminate(daemon);
    assert_eq!(reply("work", "6").unwrap(), "ok /mnt/jos/in/work/notes\n");
    assert!(log.contains("policy reloaded (1 rules)"), "{log}");
    assert!(
        log.contains("work 6: copy notes -> web: delivered"),
        "{log}"
    );
    assert!(log.ends_with("stopping (signal)\n"), "{log}");
}

/// The doorbell: a line on a qube's events port (a UNIX socket bhyve
/// would listen on; here a listener in the test) wakes `jos serve` long
/// before its next poll.
#[cfg(unix)]
#[test]
fn serve_wakes_up_on_a_qube_event() {
    use std::io::Write;
    let e = Env::new("serve-events");
    e.ok(&["init"]);
    e.ok(&["create", "work", "--kind", "app", "--os", "freebsd"]);
    e.ok(&["create", "web", "--kind", "app", "--os", "linux"]);
    e.ok(&["create", "rx", "--kind", "app", "--os", "redox"]);
    let share = e.dir.join("share");
    let run_dir = e.dir.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let cfg = e.dir.join("jos.toml");
    std::fs::write(
        &cfg,
        format!(
            "share_dir = {:?}\nrun_dir = {:?}\n",
            share.to_string_lossy(),
            run_dir.to_string_lossy()
        ),
    )
    .unwrap();
    let jos = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_jos"))
            .arg("--config")
            .arg(&cfg)
            .args(args)
            .env("JOS_CONFIG_DIR", &e.dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "jos {args:?}: {out:?}");
    };
    jos(&["policy", "add", "work", "web", "allow"]);
    // Only `work` is "running" with its events port; `web` has none.
    let listener = std::os::unix::net::UnixListener::bind(run_dir.join("work.events")).unwrap();
    let daemon = Command::new(env!("CARGO_BIN_EXE_jos"))
        .arg("--config")
        .arg(&cfg)
        .args(["serve", "--interval", "30"])
        .env("JOS_CONFIG_DIR", &e.dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let (mut guest, _) = listener.accept().unwrap();
    // The guest exports a file, drops the request and rings.
    std::fs::create_dir_all(share.join("work").join("out")).unwrap();
    std::fs::create_dir_all(share.join("work").join("req")).unwrap();
    std::fs::write(share.join("work").join("out").join("notes"), "n\n").unwrap();
    std::fs::write(share.join("work").join("req").join("7"), "copy notes web\n").unwrap();
    guest.write_all(b"REQUEST 7\r\n").unwrap();
    let reply = share.join("work").join("req").join("7.reply");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reply.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let woke = reply.exists();
    let stopped = std::time::Instant::now();
    let log = terminate(daemon);
    assert!(
        woke,
        "not answered within 10 s of the event (poll is 30 s): {log}"
    );
    // Stopped on the signal, not at the end of the 30 s interval.
    assert!(stopped.elapsed() < std::time::Duration::from_secs(5));
    assert!(log.ends_with("stopping (signal)\n"), "{log}");
    assert!(
        woke,
        "not answered within 10 s of the event (poll is 30 s): {log}"
    );
    assert!(log.contains("listening to work's events port"), "{log}");
    assert!(!log.contains("web's events port"), "{log}");
    assert!(log.contains("event from work: \"REQUEST 7\""), "{log}");
    assert!(
        log.contains("work 7: copy notes -> web: delivered"),
        "{log}"
    );
    assert_eq!(
        std::fs::read_to_string(&reply).unwrap(),
        "ok /mnt/jos/in/work/notes\n"
    );
}

/// `jos copy` for real: a fake guest on the source console, a real
/// `share_dir` on disk. The guest "writes" to its outbox by creating the
/// file on the host side, as it would through 9p.
#[cfg(unix)]
#[test]
fn copy_moves_files_through_the_share_for_real() {
    let e = Env::new("copy-real");
    e.ok(&["init"]);
    e.ok(&["create", "work", "--kind", "app", "--os", "freebsd"]);
    e.ok(&["create", "jail", "--kind", "app", "--os", "freebsd"]);
    let (master, _keepalive) = fake_console(&e, "work");
    let consoles = e.dir.join("consoles");
    let share = e.dir.join("share");
    let cfg = e.dir.join("jos.toml");
    std::fs::write(
        &cfg,
        format!(
            "console_dir = {:?}\nshare_dir = {:?}\n",
            consoles.to_string_lossy(),
            share.to_string_lossy()
        ),
    )
    .unwrap();
    let copy = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_jos"))
            .arg("--config")
            .arg(&cfg)
            .args(["copy", "--timeout", "20"])
            .args(args)
            .env("JOS_CONFIG_DIR", &e.dir)
            .output()
            .unwrap()
    };

    // Qube to qube: the guest fills its outbox, dom0 moves the file.
    let outbox = share.join("work").join("out");
    let outbox_for_guest = outbox.clone();
    let shell = fake_guest(master.try_clone().unwrap(), "", move |cmd| {
        assert!(cmd.contains("cp -RL /etc/motd /mnt/jos/out/motd"), "{cmd}");
        std::fs::create_dir_all(&outbox_for_guest).unwrap();
        std::fs::write(outbox_for_guest.join("motd"), "Welcome\n").unwrap();
        0
    });
    let out = copy(&["work:/etc/motd", "jail"]);
    shell.join().unwrap();
    assert!(out.status.success(), "{out:?}");
    let landed = share.join("jail").join("in").join("work").join("motd");
    assert_eq!(std::fs::read_to_string(&landed).unwrap(), "Welcome\n");
    assert!(!outbox.join("motd").exists(), "outbox entry was moved");
    assert!(String::from_utf8_lossy(&out.stdout).contains("jail:/mnt/jos/in/work/motd"));

    // The guest fails: nothing moves, its output is reported.
    let shell = fake_guest(
        master.try_clone().unwrap(),
        "cp: /nope: No such file\r\n",
        |_| 1,
    );
    let out = copy(&["work:/nope", "jail"]);
    shell.join().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("exporting /nope failed (exit 1)"), "{err}");
    assert!(err.contains("No such file"), "{err}");

    // dom0 to qube: copied, made writable for the guest user.
    let data = e.dir.join("data");
    std::fs::create_dir_all(data.join("sub")).unwrap();
    std::fs::write(data.join("sub").join("f"), "x").unwrap();
    let out = copy(&[data.to_str().unwrap(), "jail"]);
    assert!(out.status.success(), "{out:?}");
    let landed = share.join("jail").join("in").join("dom0").join("data");
    assert_eq!(
        std::fs::read_to_string(landed.join("sub").join("f")).unwrap(),
        "x"
    );
    let out = copy(&["/definitely/not/here", "jail"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no such file or directory on dom0"));
}

#[test]
fn setup_creates_the_system_qubes_and_resumes() {
    let e = Env::new("setup");
    // A dev box has no FreeBSD default route to download through.
    let err = e.err(&["setup"]);
    assert!(err.contains("--uplink IFACE"), "{err}");
    assert!(e
        .err(&["setup", "--uplink", "em0", "--firewall-image", "/no/fw.img"])
        .contains("no such firewall image"));

    let img = e.dir.join("fw.img");
    std::fs::write(&img, b"esp").unwrap();
    let out = e.ok(&[
        "setup",
        "--uplink",
        "em0",
        "--firewall-image",
        img.to_str().unwrap(),
    ]);
    assert!(out.contains("initialized datasets"), "{out}");
    assert!(
        out.contains(" 1. install template fbsd15-net (downloading through em0)"),
        "{out}"
    );
    assert!(out.contains(
        " 2. create sys-net (net qube, freebsd, from fbsd15-net, bridged to em0, autostart)"
    ));
    assert!(out.contains(" 3. create sys-firewall (firewall qube, unikernel, booting "));
    assert!(out.contains("- no gpu qube: no NVIDIA card is reserved"));
    assert!(out.contains("created sys-net") && out.contains("created sys-firewall"));
    assert!(out.contains("ready: `jos autostart`"));
    for q in ["fbsd15-net", "sys-net", "sys-firewall"] {
        assert!(e.qube_file(q).is_file(), "{q}");
    }
    let net = std::fs::read_to_string(e.qube_file("sys-net")).unwrap();
    assert!(net.contains("uplink = \"em0\"") && net.contains("autostart = true"));
    let fw = std::fs::read_to_string(e.qube_file("sys-firewall")).unwrap();
    assert!(fw.contains("netvm = \"sys-net\"") && fw.contains("os = \"unikernel\""));

    // Again, without any uplink: everything is there.
    let again = e.ok(&["setup", "--no-gpu"]);
    assert!(again.contains("- sys-net exists"), "{again}");
    assert!(again.contains("- no gpu qube (--no-gpu)"));
    assert!(again.contains("nothing to do"));
}

/// A stand-in FreeBSD host: shell scripts named after the host tools, in
/// a directory meant to be the whole `PATH` (nothing of the development
/// machine is ever run). Returns the directory.
#[cfg(unix)]
fn fake_host(e: &Env, tools: &[(&str, &str)]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = e.dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for (name, body) in tools {
        let path = bin.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// `pciconf -l -v` of a laptop: Intel iGPU for the dom0, an NVIDIA card
/// (GPU and audio) reserved at install time, the wired NIC on em0.
#[cfg(unix)]
const PCICONF: &str = r#"printf '%s\n' \
  'vgapci0@pci0:0:2:0:  class=0x030000 rev=0x00 hdr=0x00 vendor=0x8086 device=0x7d55' \
  "    vendor     = 'Intel Corporation'" \
  "    device     = 'Arc Graphics'" \
  'ppt0@pci0:1:0:0:  class=0x030000 rev=0xa1 hdr=0x00 vendor=0x10de device=0x28b8' \
  "    vendor     = 'NVIDIA Corporation'" \
  "    device     = 'RTX 500 Ada Generation Laptop GPU'" \
  'ppt1@pci0:1:0:1:  class=0x040300 rev=0xa1 hdr=0x00 vendor=0x10de device=0x22be' \
  'em0@pci0:5:0:0:  class=0x020000 rev=0x10 hdr=0x00 vendor=0x8086 device=0x15bc' \
  "    vendor     = 'Intel Corporation'" \
  "    device     = 'Ethernet Connection I219-LM'""#;

/// The read-only queries of `jos doctor` and `jos setup` answered like a
/// FreeBSD host ready for passthrough; in a dry run nothing else runs.
#[cfg(unix)]
#[test]
fn a_ready_host_passes_the_doctor_and_setup_reserves_its_cards() {
    let e = Env::new("host");
    let bin = fake_host(
        &e,
        &[
            ("kldstat", "exit 0"),
            (
                "sysctl",
                r#"case "$1" in
  hw.vmm.iommu.enable) echo 'hw.vmm.iommu.enable: 1' ;;
  hw.model) echo 'hw.model: Intel(R) Core(TM) Ultra 7 165H' ;;
  *) exit 1 ;;
esac"#,
            ),
            ("pciconf", PCICONF),
            (
                "route",
                "printf '%s\\n' '   route to: default' '  interface: em0'",
            ),
            (
                "sysrc",
                r#"case "$*" in *' -n '*) echo '1/0/0 1/0/1' ;; *) exit 1 ;; esac"#,
            ),
            ("zpool", "echo zroot"),
        ],
    );
    let run_dir = e.dir.join("run");
    let cfg = e.dir.join("jos.toml");
    std::fs::write(&cfg, format!("run_dir = {:?}\n", run_dir.to_string_lossy())).unwrap();
    let jos = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_jos"))
            .arg("--dry-run")
            .arg("--config")
            .arg(&cfg)
            .args(args)
            .env("JOS_CONFIG_DIR", &e.dir)
            .env("PATH", &bin)
            .output()
            .unwrap()
    };
    let ok = |args: &[&str]| {
        let out = jos(args);
        assert!(out.status.success(), "jos {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let doctor = ok(&["doctor"]);
    assert!(doctor.contains("hw.vmm.iommu.enable=1"), "{doctor}");
    assert!(doctor.contains("pool zroot online"), "{doctor}");
    assert!(doctor.contains("1/0/0 1/0/1"), "{doctor}");

    let all = ok(&["pci", "list"]);
    assert!(all.starts_with("ADDR"), "{all}");
    assert!(
        all.contains("1/0/0    ppt0       10de:28b8 030000   NVIDIA Corporation RTX 500"),
        "{all}"
    );
    assert!(all.contains("5/0/0    em0"), "{all}");
    let gpus = ok(&["pci", "list", "--gpus"]);
    assert!(gpus.contains("0/2/0") && gpus.contains("1/0/0"), "{gpus}");
    assert!(!gpus.contains("em0") && !gpus.contains("1/0/1"), "{gpus}");
    let ready = ok(&["pci", "list", "--ready"]);
    assert!(
        ready.contains("1/0/1") && !ready.contains("vgapci0"),
        "{ready}"
    );

    // The uplink comes from the default route, the NIC behind it goes to
    // sys-net, the reserved NVIDIA card to gpu; the NIC is reserved last.
    let out = ok(&["setup"]);
    assert!(
        out.contains(" 1. install template fbsd15-net (downloading through em0)"),
        "{out}"
    );
    assert!(out.contains("install template debian-13-dev"), "{out}");
    assert!(out.contains("created sys-net") && out.contains("created gpu"));
    assert!(
        out.contains("+ sysrc -f /boot/loader.conf 'pptdevs=1/0/0 1/0/1 5/0/0'"),
        "{out}"
    );
    assert!(out.contains("wrote pptdevs=\"1/0/0 1/0/1 5/0/0\" to /boot/loader.conf"));
    assert!(out.contains("reboot to hand the reserved devices"), "{out}");
    let net = std::fs::read_to_string(e.qube_file("sys-net")).unwrap();
    assert!(net.contains("5/0/0") && !net.contains("uplink"), "{net}");
    let gpu = std::fs::read_to_string(e.qube_file("gpu")).unwrap();
    assert!(gpu.contains("1/0/0") && gpu.contains("1/0/1"), "{gpu}");

    // A supervisor between two boots: the qube is "booting" and cannot
    // be started twice. A host interface shows as the qube's uplink.
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("sys-net.pid"), "4242\n").unwrap();
    let out = jos(&["start", "sys-net"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("sys-net already has a supervisor (pid 4242)"),
        "{err}"
    );
    ok(&[
        "create", "lab", "--kind", "net", "--os", "freebsd", "--uplink", "em0",
    ]);
    let ls = ok(&["ls"]);
    let line = |name: &str| {
        ls.lines()
            .find(|l| l.starts_with(&format!("{name} ")))
            .unwrap()
            .to_string()
    };
    assert!(line("sys-net").contains("booting"), "{ls}");
    assert!(line("gpu").contains("stopped"), "{ls}");
    assert!(line("lab").contains("uplink:em0"), "{ls}");
}

/// Without `--dry-run`: the commands really run, against the stand-in
/// host tools (bhyve powers the guest off at once).
#[cfg(unix)]
#[test]
fn real_runs_against_a_stand_in_host() {
    let e = Env::new("real");
    let bin = fake_host(
        &e,
        &[
            ("zfs", "exit 0"),
            ("kill", "exit 0"),
            ("ifconfig", "exit 0"),
            ("bhyvectl", "exit 0"),
            ("bhyve", "exit 1"),
            ("mkdir", "exit 0"),
            ("rm", "exit 0"),
            ("chmod", "exit 0"),
            ("tee", "while read -r _; do :; done"),
            ("cu", "exit 1"),
        ],
    );
    e.ok(&["init"]);
    e.ok(&["create", "fbsd15", "--kind", "template", "--os", "freebsd"]);
    e.ok(&["create", "work", "--kind", "app", "--os", "freebsd"]);
    e.ok(&["create", "web", "--kind", "app", "--os", "linux"]);
    e.ok(&[
        "create",
        "sys-net",
        "--kind",
        "net",
        "--os",
        "freebsd",
        "--autostart",
    ]);
    let dir = |name: &str| {
        let d = e.dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    let (run_dir, consoles) = (dir("run"), dir("consoles"));
    let cfg = e.dir.join("jos.toml");
    std::fs::write(
        &cfg,
        format!(
            "config_dir = {:?}\nrun_dir = {:?}\nconsole_dir = {:?}\nshare_dir = {:?}\n\
             cache_dir = {:?}\n",
            e.dir.to_string_lossy(),
            run_dir.to_string_lossy(),
            consoles.to_string_lossy(),
            dir("share").to_string_lossy(),
            dir("cache").to_string_lossy(),
        ),
    )
    .unwrap();
    // The qube definitions come from config_dir: no JOS_CONFIG_DIR.
    let jos = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_jos"))
            .arg("--config")
            .arg(&cfg)
            .args(args)
            .env_remove("JOS_CONFIG_DIR")
            .env("PATH", &bin)
            .output()
            .unwrap()
    };
    let stderr = |out: &Output| String::from_utf8_lossy(&out.stderr).into_owned();

    let ls = jos(&["ls"]);
    assert!(
        String::from_utf8_lossy(&ls.stdout).contains("work "),
        "{ls:?}"
    );

    // A detached supervisor, logging next to its pidfile.
    let out = jos(&["start", "work"]);
    assert!(out.status.success(), "{out:?}");
    let started = String::from_utf8_lossy(&out.stdout);
    assert!(
        started.contains("started work (supervisor pid "),
        "{started}"
    );
    let log = run_dir.join("work.log");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut text = String::new();
    while !text.contains("finished") && !text.contains("failed") {
        assert!(std::time::Instant::now() < deadline, "{text}");
        std::thread::sleep(std::time::Duration::from_millis(100));
        text = std::fs::read_to_string(&log).unwrap_or_default();
    }
    assert!(text.contains("[jos supervise work] boot #1"), "{text}");
    assert!(
        text.contains("finished after 1 boot(s): poweroff"),
        "{text}"
    );

    // autostart reports the qubes it could not start.
    std::fs::write(run_dir.join("sys-net.pid"), "4242\n").unwrap();
    let out = jos(&["autostart"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("error starting sys-net"), "{out:?}");
    assert!(stderr(&out).contains("1 qube(s) failed to start"));

    let out = jos(&["console", "work"]);
    assert!(stderr(&out).contains("console exited with"), "{out:?}");

    let out = jos(&["template", "run", "fbsd15"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout)
        .contains("fbsd15: poweroff; `jos template snapshot fbsd15 <snap>`"));

    // No pciconf: only a dry run plans without the PCI devices.
    let out = jos(&["setup", "--uplink", "em0"]);
    assert!(stderr(&out).contains("running pciconf: "), "{out:?}");
    assert!(!e.qube_file("sys-firewall").exists());

    // A console device that is not a terminal.
    std::fs::write(consoles.join("nmdm-work.1B"), "").unwrap();
    let out = jos(&["run", "work", "--console", "true"]);
    assert!(stderr(&out).contains("raw mode"), "{out:?}");

    // An agent socket whose other end hangs up after the ping.
    let listener = std::os::unix::net::UnixListener::bind(run_dir.join("web.agent")).unwrap();
    let peer = std::thread::spawn(move || {
        use std::io::BufRead;
        let (stream, _) = listener.accept().unwrap();
        let mut ping = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut ping)
            .unwrap();
        ping
    });
    let out = jos(&["run", "web", "true"]);
    assert!(peer.join().unwrap().contains("PING"));
    assert!(stderr(&out).contains("the agent channel closed"), "{out:?}");
}
