//! `jos-release` end to end. With `--dry-run` every subcommand prints the
//! host commands it would run, so the whole release pipeline is exercised
//! on a development machine without FreeBSD tools, root or network. The
//! paths that do run host tools (QEMU, `id`) get fake programs as their
//! whole `PATH`.

// Test helpers outside #[test] functions are test code too.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "common/gpt.rs"]
mod gpt;

use std::path::{Path, PathBuf};
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

    /// `rel` under the work directory, as an argument.
    fn path(&self, rel: &str) -> String {
        self.0.join(rel).to_string_lossy().into_owned()
    }

    /// `jos-release --dry-run --work <dir> args`.
    fn run(&self, args: &[&str]) -> (bool, String) {
        let work = self.path("");
        jos(
            Path::new(env!("CARGO_MANIFEST_DIR")),
            None,
            &[],
            &[&["--dry-run", "--work", &work], args].concat(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (ok, text) = self.run(args);
        assert!(ok, "jos-release {args:?} failed:\n{text}");
        text
    }

    /// A fake OVMF code file: `fw/OVMF_CODE.fd` with the `OVMF_VARS.fd`
    /// template next to it, or a lone `fw/edk2-x86_64-code.fd`.
    fn firmware(&self, with_vars: bool) -> String {
        let dir = self.0.join("fw");
        std::fs::create_dir_all(&dir).unwrap();
        let code = if with_vars {
            std::fs::write(dir.join("OVMF_VARS.fd"), b"template").unwrap();
            dir.join("OVMF_CODE.fd")
        } else {
            dir.join("edk2-x86_64-code.fd")
        };
        std::fs::write(&code, [1u8; 64]).unwrap();
        code.to_string_lossy().into_owned()
    }

    /// A symlink at `rel` to a file that cannot exist: what is written
    /// through it fails, while `exists()`/`is_file()` say there is nothing.
    fn dangling(&self, rel: &str) {
        let link = self.0.join(rel);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(self.0.join("nowhere/file"), link).unwrap();
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `jos-release args` from `cwd`, with `PATH` replaced by `path` if given.
fn jos(cwd: &Path, path: Option<&Path>, env: &[(&str, &str)], args: &[&str]) -> (bool, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_jos-release"));
    cmd.current_dir(cwd).args(args).envs(env.iter().copied());
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// `jos-release` without `--dry-run`, from `cwd`. Only for paths that stop
/// before running anything on the host.
fn real(cwd: &Path, args: &[&str]) -> (bool, String) {
    jos(cwd, None, &[], args)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A QEMU whose serial console is a FreeBSD login and a root shell that
/// answers every command as if it succeeded, without running it. With
/// stdin closed (an interactive run under the tests) it just exits.
const QEMU: (&str, &str) = (
    "qemu-system-x86_64",
    r#"#!/bin/sh
[ "$FAKE_FAIL" = qemu-system-x86_64 ] && exit 3
printf 'login: '
read -r u || exit 0
printf '# '
while read -r line; do
    case $line in
        poweroff) exit 0 ;;
        reboot) printf 'login: '; read -r u; printf 'Password:'; read -r p; printf '# ' ;;
        *) eval "echo ${line##*; echo }" ;;
    esac
done
"#,
);

/// `create` makes an empty disk, `convert` copies, the rest succeeds;
/// the subcommand named by `FAKE_FAIL` fails.
const QEMU_IMG: (&str, &str) = (
    "qemu-img",
    r#"#!/bin/sh
PATH=/usr/bin:/bin
[ "$1" = "$FAKE_FAIL" ] && exit 1
case $1 in
    create) : > "$4" ;;
    convert) cat "$4" > "$5" ;;
esac
"#,
);

const ID: (&str, &str) = ("id", "#!/bin/sh\necho \"${FAKE_UID}\"\n");

const MTOOLS: &[&str] = &["mdir", "mmd", "mcopy"];

/// A fresh directory to use as the whole `PATH`: the `fakes` scripts, and
/// links to the `real` programs found on the test's own `PATH`.
fn fake_path(dir: &Path, fakes: &[(&str, &str)], real: &[&str]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    for (name, script) in fakes {
        let p = dir.join(name);
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::var_os("PATH").unwrap();
    for name in real {
        let found = std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("this test needs {name} on PATH"));
        std::os::unix::fs::symlink(found, dir.join(name)).unwrap();
    }
    dir.to_path_buf()
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
    let bins = w.ok(&["bins"]);
    assert!(bins.contains("cargo build --release"));
    assert!(bins.contains("--remap-path-prefix="), "{bins}");
    assert!(w.ok(&["rootfs"]).contains("release.toml"));
    assert!(w.ok(&["pack"]).contains("tar -C"));
    assert!(w.ok(&["iso"]).contains("Done:"));
    let out = w.ok(&["clean"]);
    assert!(out.contains("+ rm -rf"), "{out}");
    assert!(w.0.is_dir(), "a dry run removes nothing");
    let work = w.path("");
    let (ok, out) = real(&repo_root(), &["--work", &work, "clean"]);
    assert!(ok, "{out}");
    assert!(!w.0.exists());
}

#[test]
fn qemu_boots_the_iso_or_the_disk() {
    let w = Work::new("qemu");
    let fw = w.firmware(true);
    let out = w.ok(&["qemu", "--firmware", &fw]);
    assert!(out.contains("qemu-system-x86_64"));
    assert!(out.contains("media=cdrom"));
    let out = w.ok(&["--firmware", &fw, "qemu", "--no-cdrom"]);
    assert!(!out.contains("media=cdrom"));
    let missing = w.path("missing.fd");
    for cmd in [
        &["qemu"][..],
        &["builder", "--image", "/i.qcow2", "--auto"],
        &["selfhost"],
    ] {
        let (ok, out) = w.run(&[&["--firmware", &missing][..], cmd].concat());
        assert!(!ok);
        assert!(
            out.contains(
                "OVMF firmware not found; install the ovmf/edk2 package or pass --firmware"
            ),
            "{out}"
        );
    }
}

#[test]
fn qemu_runs_the_host_tools() {
    let w = Work::new("qemu-host");
    let qemu = fake_path(&w.0.join("bin"), &[QEMU, QEMU_IMG], &[]);
    let no_qemu = fake_path(&w.0.join("bin-img"), &[QEMU_IMG], &[]);
    let fw = w.firmware(true);
    for dir in ["a", "b"] {
        std::fs::create_dir_all(w.0.join(dir)).unwrap();
    }
    let qemu_in = |work: &str, path: &Path, fail: &str| {
        let work = w.path(work);
        jos(
            &repo_root(),
            Some(path),
            &[("FAKE_FAIL", fail)],
            &["--work", &work, "--firmware", &fw, "qemu", "--no-cdrom"],
        )
    };

    // A fresh disk and variable store, then both are reused.
    let (ok, out) = qemu_in("a", &qemu, "");
    assert!(ok, "{out}");
    assert!(out.contains(&format!(
        "+ qemu-img create -f qcow2 {} 40G",
        w.path("a/qemu-disk.qcow2")
    )));
    assert!(out.contains(&format!(
        "+ cp {} {}",
        w.path("fw/OVMF_VARS.fd"),
        w.path("a/qemu-vars.fd")
    )));
    assert_eq!(
        std::fs::read(w.0.join("a/qemu-vars.fd")).unwrap(),
        b"template"
    );
    let (ok, out) = qemu_in("a", &qemu, "");
    assert!(ok, "{out}");
    assert!(!out.contains("qemu-img"), "{out}");
    assert!(!out.contains("+ cp"), "{out}");

    let (ok, out) = qemu_in("a", &qemu, "qemu-system-x86_64");
    assert!(!ok);
    assert!(
        out.contains("jos-release: qemu-system-x86_64 exited with exit status: 3"),
        "{out}"
    );
    let (ok, out) = qemu_in("a", &no_qemu, "");
    assert!(!ok);
    assert!(
        out.contains("jos-release: running qemu-system-x86_64 (is QEMU installed?)"),
        "{out}"
    );
    let (ok, out) = qemu_in("b", &qemu, "create");
    assert!(!ok);
    assert!(
        out.contains("jos-release: qemu-img exited with exit status: 1"),
        "{out}"
    );
    w.dangling("c/qemu-vars.fd");
    let (ok, out) = qemu_in("c", &qemu, "");
    assert!(!ok);
    assert!(out.contains("No such file or directory"), "{out}");
}

#[test]
fn builder_vm_commands() {
    let w = Work::new("builder");
    let out = w.ok(&["builder", "--image", "/images/freebsd.qcow2"]);
    assert!(out.contains("mount_tag=juanitos"), "{out}");
    let fw = w.firmware(true);
    let b = w.path("b");
    let out = w.ok(&[
        "--firmware",
        &fw,
        "builder",
        "--image",
        "/images/freebsd.qcow2",
        "--auto",
        "--out",
        &b,
        "--cpus",
        "2",
        "--memory-mb",
        "2048",
    ]);
    assert!(out.contains("qemu-img convert"));
    assert!(out.contains("guest: build jos-release"));
    assert!(out.contains("-smp 2 -m 2048"));
}

/// A repository with a host-built `jos-boot.efi` but no firewall image,
/// and the arguments that point `jos-release` at it.
fn uefi_repo(w: &Work) -> Vec<String> {
    let efi =
        w.0.join("repo/target/uefi/x86_64-unknown-uefi/release/jos-boot.efi");
    std::fs::create_dir_all(efi.parent().unwrap()).unwrap();
    std::fs::write(&efi, b"efi").unwrap();
    let profile = repo_root().join("release/juanitos.toml");
    vec![
        "--repo".into(),
        w.path("repo"),
        "--profile".into(),
        profile.to_string_lossy().into_owned(),
    ]
}

#[test]
fn defaults_come_from_the_work_directory() {
    let w = Work::new("defaults");
    let fw = w.firmware(false);
    let repo = uefi_repo(&w);
    let repo: Vec<&str> = repo.iter().map(String::as_str).collect();
    let auto = [
        &repo[..],
        &[
            "--firmware",
            &fw,
            "builder",
            "--image",
            "/i.qcow2",
            "--auto",
        ],
    ]
    .concat();
    let out = w.ok(&auto);
    assert!(
        out.contains(&format!(
            "{},if=virtio,format=raw",
            w.path("builder/builder.raw")
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!("+ cp {}/target/uefi/", w.path("repo"))),
        "{out}"
    );
    // A dry run names the stale disk it would remove, and keeps it.
    let stale = w.0.join("selfhost/selfhost.qcow2");
    std::fs::create_dir_all(w.0.join("selfhost")).unwrap();
    std::fs::write(&stale, b"old").unwrap();
    let out = w.ok(&["--firmware", &fw, "selfhost"]);
    assert!(out.contains(&format!("+ rm {}", stale.display())), "{out}");
    assert!(stale.is_file());
    assert!(
        out.contains(&format!(
            "+ qemu-img create -f qcow2 {}",
            w.path("selfhost/selfhost.qcow2")
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!("file={}", w.path("JuanitOS-"))),
        "{out}"
    );
    let (ok, out) = real(&repo_root(), &["--dry-run", "clean"]);
    assert!(ok, "{out}");
    assert!(out.contains("/release/out"), "{out}");
}

#[test]
fn the_auto_builder_drives_a_fake_vm() {
    let w = Work::new("auto");
    let repo = uefi_repo(&w);
    let image = w.0.join("freebsd.raw");
    gpt::esp_disk(&image);
    let junk = w.0.join("junk.raw");
    std::fs::write(&junk, b"junk").unwrap();
    let qemu = fake_path(&w.0.join("bin"), &[QEMU, QEMU_IMG], MTOOLS);
    let no_qemu = fake_path(&w.0.join("bin-img"), &[QEMU_IMG], MTOOLS);
    let fw = w.firmware(true);
    let auto = |out: &str, image: &Path, path: &Path, fail: &str| {
        let (out, image) = (w.path(out), image.to_string_lossy().into_owned());
        let args = [
            "--firmware",
            &fw,
            "builder",
            "--image",
            &image,
            "--auto",
            "--out",
            &out,
        ];
        let repo: Vec<&str> = repo.iter().map(String::as_str).collect();
        jos(
            &repo_root(),
            Some(path),
            &[("FAKE_FAIL", fail)],
            &[&repo[..], &args].concat(),
        )
    };

    let (ok, out) = auto("ok", &image, &qemu, "");
    assert!(ok, "{out}");
    assert!(
        out.contains(&format!("Done: results in {}", w.path("ok"))),
        "{out}"
    );
    assert!(
        out.contains("note: no host-built jos-firewall.img"),
        "{out}"
    );
    assert_eq!(std::fs::read(w.0.join("ok/jos-boot.efi")).unwrap(), b"efi");
    assert_eq!(
        std::fs::read(w.0.join("ok/OVMF_VARS.fd")).unwrap(),
        b"template"
    );
    let log = std::fs::read_to_string(w.0.join("ok/console.log")).unwrap();
    assert!(log.starts_with("login: # "), "{log}");
    let esp = format!("{}@@{}", w.path("ok/builder.raw"), gpt::ESP_FIRST_LBA * 512);
    let env = Command::new("mtype")
        .args(["-i", &esp, "::/efi/freebsd/loader.env"])
        .output()
        .expect("this test needs mtools (mtype) on PATH");
    assert!(String::from_utf8_lossy(&env.stdout).contains("boot_serial=YES"));
    // The working disk is kept from one run to the next.
    let (ok, out) = auto("ok", &image, &qemu, "");
    assert!(ok, "{out}");
    assert!(!out.contains("qemu-img"), "{out}");

    let failed = |out: &str, image: &Path, path: &Path, fail: &str, want: &str| {
        let (ok, text) = auto(out, image, path, fail);
        assert!(!ok, "{out}: {text}");
        assert!(text.contains(want), "{out}: {text}");
    };
    let qemu_img = "jos-release: qemu-img exited with exit status: 1";
    failed("convert", &image, &qemu, "convert", qemu_img);
    failed("resize", &image, &qemu, "resize", qemu_img);
    failed(
        "junk",
        &junk,
        &qemu,
        "",
        "enabling the serial console in the VM image",
    );
    w.dangling("copy/jos-boot.efi");
    failed("copy", &image, &qemu, "", "No such file or directory");
    w.dangling("vars/OVMF_VARS.fd");
    failed("vars", &image, &qemu, "", "No such file or directory");
    w.dangling("log/console.log");
    failed("log", &image, &qemu, "", "No such file or directory");
    failed("noqemu", &image, &no_qemu, "", "running qemu-system-x86_64");
    failed(
        "dead",
        &image,
        &qemu,
        "qemu-system-x86_64",
        "the guest's console closed",
    );
    std::fs::write(w.0.join("file"), b"").unwrap();
    failed("file/out", &image, &qemu, "", "Not a directory");
}

#[test]
fn selfhost_installs_rebuilds_and_compares() {
    let w = Work::new("selfhost");
    let fw = w.firmware(true);
    let out_dir = w.path("s");
    let out = w.ok(&[
        "selfhost",
        "--firmware",
        &fw,
        "--iso",
        "/x/stage1.iso",
        "--out",
        &out_dir,
    ]);
    assert!(out.contains("+ qemu-img create -f qcow2"), "{out}");
    assert!(out.contains("file=/x/stage1.iso,if=none,id=cd"), "{out}");
    assert!(out.contains("jos-install --non-interactive"), "{out}");
    assert!(
        out.contains("dom0: compare stage N with stage N+1"),
        "{out}"
    );
}

#[test]
fn selfhost_drives_a_fake_vm() {
    let w = Work::new("selfhost-host");
    let qemu = fake_path(&w.0.join("bin"), &[QEMU, QEMU_IMG], &[]);
    let no_qemu = fake_path(&w.0.join("bin-img"), &[QEMU_IMG], &[]);
    let fw = w.firmware(true);
    std::fs::write(w.0.join("stage1.iso"), b"iso").unwrap();
    let selfhost = |out: &str, iso: &str, path: &Path, fail: &str| {
        let (out, iso, work) = (w.path(out), w.path(iso), w.path(""));
        jos(
            &repo_root(),
            Some(path),
            &[("FAKE_FAIL", fail)],
            &[
                "--work",
                &work,
                "--firmware",
                &fw,
                "selfhost",
                "--iso",
                &iso,
                "--out",
                &out,
            ],
        )
    };

    let (ok, out) = selfhost("ok", "stage1.iso", &qemu, "");
    assert!(ok, "{out}");
    assert!(
        out.contains("Self-hosting: stage N+1 matches stage N"),
        "{out}"
    );
    // A second round starts from a blank disk again.
    let (ok, out) = selfhost("ok", "stage1.iso", &qemu, "");
    assert!(ok, "{out}");
    assert!(
        out.contains(&format!("+ rm {}", w.path("ok/selfhost.qcow2"))),
        "{out}"
    );
    assert!(
        out.contains(&format!("+ rm {}", w.path("ok/OVMF_VARS.fd"))),
        "{out}"
    );

    let failed = |out: &str, iso: &str, path: &Path, fail: &str, want: &str| {
        let (ok, text) = selfhost(out, iso, path, fail);
        assert!(!ok, "{out}: {text}");
        assert!(text.contains(want), "{out}: {text}");
    };
    let iso = "stage1.iso";
    failed(
        "noiso",
        "missing.iso",
        &qemu,
        "",
        "does not exist; build it first",
    );
    std::fs::create_dir_all(w.0.join("rm/selfhost.qcow2")).unwrap();
    failed("rm", iso, &qemu, "", "Is a directory");
    failed(
        "create",
        iso,
        &qemu,
        "create",
        "jos-release: qemu-img exited with exit status: 1",
    );
    w.dangling("vars/OVMF_VARS.fd");
    failed("vars", iso, &qemu, "", "No such file or directory");
    failed("noqemu", iso, &no_qemu, "", "running qemu-system-x86_64");
    failed(
        "dead",
        iso,
        &qemu,
        "qemu-system-x86_64",
        "the guest's console closed",
    );
    std::fs::write(w.0.join("file"), b"").unwrap();
    failed("file/out", iso, &qemu, "", "Not a directory");
}

#[test]
fn qemu_with_a_viommu_explains_the_passthrough_setup() {
    let w = Work::new("viommu");
    let repo = repo_root();
    let fw = w.firmware(false);
    let out = w.ok(&[
        "--repo",
        &repo.to_string_lossy(),
        "qemu",
        "--iommu",
        "--headless",
        "--firmware",
        &fw,
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
    let work = w.path("");
    let fw = w.firmware(false);
    let (ok, out) = real(&repo_root(), &["--work", &work, "qemu", "--firmware", &fw]);
    assert!(!ok);
    assert!(out.contains("does not exist; build it first"), "{out}");
    assert!(!out.contains("qemu-system"), "nothing was started: {out}");
}

#[test]
fn compare_reports_and_fails_on_differences() {
    let w = Work::new("compare");
    let out = w.ok(&["compare", "/x/a.txz", "/x/b.txz"]);
    assert!(out.contains("+ tar -xpf /x/a.txz -C "), "{out}");
    assert!(out.contains("same system"), "{out}");
    assert!(!w.0.join("compare").exists(), "a dry run extracts nothing");

    let src = w.0.join("src");
    std::fs::create_dir_all(src.join("etc")).unwrap();
    std::fs::write(src.join("etc/rc.conf"), "a\n").unwrap();
    let tar = |name: &str| {
        let p = w.0.join(name);
        let st = Command::new("tar")
            .arg("-C")
            .arg(&src)
            .arg("-cf")
            .arg(&p)
            .arg(".")
            .status()
            .expect("this test needs tar on PATH");
        assert!(st.success());
        p.to_string_lossy().into_owned()
    };
    let a = tar("a.tar");
    let work = w.path("work");
    let (ok, out) = real(&repo_root(), &["--work", &work, "compare", &a, &a]);
    assert!(ok, "{out}");
    std::fs::write(src.join("etc/rc.conf"), "b\n").unwrap();
    let b = tar("b.tar");
    let (ok, out) = real(&repo_root(), &["--work", &work, "compare", &a, &b]);
    assert!(!ok);
    assert!(out.contains("DIFFERS   contents: /etc/rc.conf"), "{out}");
    assert!(out.contains("differ"), "{out}");
    let missing = w.path("missing.tar");
    let (ok, out) = real(&repo_root(), &["--work", &work, "compare", &missing, &a]);
    assert!(!ok);
    assert!(out.contains("missing.tar"), "{out}");
}

/// Checked with a fake `id`, so the result does not depend on who runs
/// the tests.
#[test]
fn release_steps_need_root() {
    let w = Work::new("root");
    let id = fake_path(&w.0.join("bin"), &[ID], &[]);
    let no_id = fake_path(&w.0.join("bin-none"), &[], &[]);
    let work = w.path("");
    let step = |path: &Path, uid: &str, step: &str| {
        jos(
            &repo_root(),
            Some(path),
            &[("FAKE_UID", uid)],
            &["--work", &work, step],
        )
    };
    for s in ["rootfs", "pack", "iso", "all"] {
        let (ok, out) = step(&id, "1000", s);
        assert!(!ok, "{s}");
        assert!(
            out.contains("jos-release: this step must run as root (or use --dry-run)"),
            "{s}: {out}"
        );
    }
    let (ok, out) = step(&no_id, "", "pack");
    assert!(!ok);
    assert!(out.contains("jos-release: running id -u"), "{out}");
    // As root the step goes ahead, here up to the missing tar.
    let (ok, out) = step(&id, "0", "pack");
    assert!(!ok);
    assert!(out.contains("Packing"), "{out}");
    assert!(out.contains("tar"), "{out}");
}

#[test]
fn bad_arguments_are_rejected() {
    let w = Work::new("bad");
    let (ok, _) = w.run(&["no-such-step"]);
    assert!(!ok);
    let (ok, out) = w.run(&["--profile", "/nonexistent/profile.toml", "fetch"]);
    assert!(!ok, "{out}");
}
