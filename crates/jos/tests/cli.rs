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

    // Qube to qube: the source exports over its console, dom0 moves.
    let out = e.ok(&["copy", "work:/etc/motd", "gpu"]);
    assert!(out.contains("type on /dev/nmdm-work.1B"), "{out}");
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
    assert!(!out.contains("type on"), "{out}");
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
    assert_eq!(reply("web", "4").unwrap(), "denied: policy\n");

    // Malformed requests are answered too, so the guest does not wait.
    req("web", "5", "exec rm -rf /\n");
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
