//! `jos-release compare`: are two dom0 tarballs the same system?
//!
//! Stage 3 of the bootstrap (`docs/bootstrap.md`) rebuilds JuanitOS on a
//! JuanitOS built by JuanitOS and expects the same tarball contents. The
//! archives themselves never match byte for byte (mtimes, member order),
//! so both are extracted and their trees compared entry by entry: path,
//! type, permission bits, owner, link target and SHA-256 of the contents.
//! A short list of files is expected to differ, each for a stated reason.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;

use juanitos_core::dom0;
use juanitos_core::fsops::FsOps;
use juanitos_core::release::sha256_file;
use juanitos_core::sys::Runner;

use crate::builder::unix;

/// Files that differ between two correct builds, and why.
pub const EXPECTED: &[(&str, &str)] = &[
    (
        dom0::RELEASE_INFO_PATH,
        "the build stamp: stage, built_on and built_at differ by design",
    ),
    (
        "/var/db/pkg/local.sqlite",
        "pkg's database records when each package was installed",
    ),
    ("/var/log/userlog", "pkg logs every install with its time"),
    (
        "/usr/local/share/info/dir",
        "install-info rewrites the index as packages are installed",
    ),
];

/// What a path is in an extracted tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A regular file and the SHA-256 of its contents.
    File(String),
    Dir,
    /// A symbolic link and its target.
    Symlink(String),
    /// Anything else (device, fifo, socket).
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    /// Permission bits, set-id and sticky included.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// Every path under `root` (relative, `/`-separated, with a leading `/`),
/// without following symbolic links.
pub fn snapshot(root: &Path) -> anyhow::Result<BTreeMap<String, Entry>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
        for e in entries {
            let path = e?.path();
            let meta = std::fs::symlink_metadata(&path)
                .with_context(|| format!("inspecting {}", path.display()))?;
            let ft = meta.file_type();
            let kind = if ft.is_dir() {
                stack.push(path.clone());
                Kind::Dir
            } else if ft.is_symlink() {
                Kind::Symlink(unix(&std::fs::read_link(&path)?))
            } else if ft.is_file() {
                Kind::File(
                    sha256_file(&path).with_context(|| format!("hashing {}", path.display()))?,
                )
            } else {
                Kind::Other
            };
            let (mode, uid, gid) = owner_and_mode(&meta);
            let rel = path.strip_prefix(root).unwrap_or(&path);
            out.insert(
                format!("/{}", unix(rel)),
                Entry {
                    kind,
                    mode,
                    uid,
                    gid,
                },
            );
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn owner_and_mode(m: &std::fs::Metadata) -> (u32, u32, u32) {
    use std::os::unix::fs::MetadataExt;
    (m.mode() & 0o7777, m.uid(), m.gid())
}

#[cfg(not(unix))]
fn owner_and_mode(_: &std::fs::Metadata) -> (u32, u32, u32) {
    (0, 0, 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Difference {
    OnlyInA(String),
    OnlyInB(String),
    /// The path and what differs about it.
    Changed(String, Vec<&'static str>),
}

impl Difference {
    /// Why this one is fine, if it is.
    pub fn expected(&self) -> Option<&'static str> {
        match self {
            Difference::Changed(p, _) => EXPECTED
                .iter()
                .find(|(path, _)| path == p)
                .map(|(_, why)| *why),
            _ => None,
        }
    }
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Difference::OnlyInA(p) => write!(f, "only in A: {p}"),
            Difference::OnlyInB(p) => write!(f, "only in B: {p}"),
            Difference::Changed(p, what) => write!(f, "{}: {p}", what.join(", ")),
        }
    }
}

/// `b` against `a`. Ownership is skipped when the trees were extracted by
/// someone other than root (everything then belongs to that user).
pub fn diff(
    a: &BTreeMap<String, Entry>,
    b: &BTreeMap<String, Entry>,
    owners: bool,
) -> Vec<Difference> {
    let mut out = Vec::new();
    for (path, ea) in a {
        let Some(eb) = b.get(path) else {
            out.push(Difference::OnlyInA(path.clone()));
            continue;
        };
        let mut what = Vec::new();
        match (&ea.kind, &eb.kind) {
            (Kind::File(x), Kind::File(y)) if x != y => what.push("contents"),
            (Kind::Symlink(x), Kind::Symlink(y)) if x != y => what.push("link target"),
            (x, y) if std::mem::discriminant(x) != std::mem::discriminant(y) => what.push("type"),
            _ => {}
        }
        if ea.mode != eb.mode {
            what.push("mode");
        }
        if owners && (ea.uid, ea.gid) != (eb.uid, eb.gid) {
            what.push("owner");
        }
        if !what.is_empty() {
            out.push(Difference::Changed(path.clone(), what));
        }
    }
    out.extend(
        b.keys()
            .filter(|p| !a.contains_key(*p))
            .map(|p| Difference::OnlyInB(p.clone())),
    );
    out
}

/// The outcome of [`compare_tarballs`].
#[derive(Debug)]
pub struct Report {
    pub entries: (usize, usize),
    pub owners_compared: bool,
    pub differences: Vec<Difference>,
}

impl Report {
    pub fn unexpected(&self) -> impl Iterator<Item = &Difference> {
        self.differences.iter().filter(|d| d.expected().is_none())
    }

    pub fn render(&self) -> String {
        let mut s = format!(
            "A: {} entries, B: {} entries\n",
            self.entries.0, self.entries.1
        );
        if !self.owners_compared {
            s.push_str("note: not root, owners were not compared\n");
        }
        for d in &self.differences {
            match d.expected() {
                Some(why) => s.push_str(&format!("  expected  {d} ({why})\n")),
                None => s.push_str(&format!("  DIFFERS   {d}\n")),
            }
        }
        let n = self.unexpected().count();
        if n == 0 {
            s.push_str("same system: no unexpected differences\n");
        } else {
            s.push_str(&format!("{n} unexpected difference(s)\n"));
        }
        s
    }
}

/// Extract `a` and `b` under `scratch` and compare the trees. The scratch
/// directory is removed afterwards unless `keep` is set. A dry run only
/// prints the commands and compares nothing.
pub fn compare_tarballs(
    runner: &dyn Runner,
    fs: &FsOps,
    a: &Path,
    b: &Path,
    scratch: &Path,
    keep: bool,
) -> anyhow::Result<Report> {
    let owners = runner
        .run("id", &["-u"])
        .map(|o| o.success() && o.stdout.trim() == "0")
        .unwrap_or(false);
    let mut trees = Vec::new();
    for (tarball, side) in [(a, "a"), (b, "b")] {
        let dir = scratch.join(side);
        clear(fs, &dir)?;
        fs.mkdir_p(&dir)?;
        println!("==> Extracting {}", tarball.display());
        runner.run_checked("tar", &["-xpf", &unix(tarball), "-C", &unix(&dir)])?;
        trees.push(if fs.dry_run {
            BTreeMap::new()
        } else {
            snapshot(&dir)?
        });
    }
    let (ta, tb) = match trees.as_slice() {
        [ta, tb] => (ta, tb),
        _ => anyhow::bail!("internal error: expected two trees"),
    };
    let report = Report {
        entries: (ta.len(), tb.len()),
        owners_compared: owners,
        differences: diff(ta, tb, owners),
    };
    if !keep {
        clear(fs, scratch)?;
    }
    Ok(report)
}

/// Remove an extracted tree. Directories extracted without write
/// permission (`/var/empty`, 0555) would stop a non-root removal, so they
/// are made writable first.
fn clear(fs: &FsOps, dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    if !fs.dry_run && dir.is_dir() {
        use std::os::unix::fs::PermissionsExt;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755));
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(e.path());
                }
            }
        }
    }
    fs.remove_tree(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use juanitos_core::sys::{CmdOutput, MockRunner, RealRunner};
    use std::path::PathBuf;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("jos-compare-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn put(&self, rel: &str, content: &[u8]) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = clear(&FsOps::new(false), &self.0);
        }
    }

    fn file(sha: &str, mode: u32) -> Entry {
        Entry {
            kind: Kind::File(sha.into()),
            mode,
            uid: 0,
            gid: 0,
        }
    }

    #[test]
    fn differences_are_classified() {
        let mut a = BTreeMap::new();
        a.insert("/same".to_string(), file("1", 0o644));
        a.insert("/gone".to_string(), file("1", 0o644));
        a.insert("/edited".to_string(), file("1", 0o644));
        a.insert("/chmod".to_string(), file("1", 0o755));
        a.insert(
            "/link".to_string(),
            Entry {
                kind: Kind::Symlink("x".into()),
                ..file("", 0o777)
            },
        );
        a.insert("/retyped".to_string(), file("1", 0o755));
        a.insert("/chown".to_string(), file("1", 0o644));
        a.insert(dom0::RELEASE_INFO_PATH.to_string(), file("1", 0o644));
        let mut b = a.clone();
        b.remove("/gone");
        b.insert("/new".to_string(), file("1", 0o644));
        b.insert("/edited".to_string(), file("2", 0o644));
        b.insert("/chmod".to_string(), file("1", 0o4755));
        b.insert(
            "/link".to_string(),
            Entry {
                kind: Kind::Symlink("y".into()),
                ..file("", 0o777)
            },
        );
        b.insert(
            "/retyped".to_string(),
            Entry {
                kind: Kind::Dir,
                ..file("", 0o755)
            },
        );
        b.insert(
            "/chown".to_string(),
            Entry {
                uid: 1001,
                ..file("1", 0o644)
            },
        );
        b.insert(dom0::RELEASE_INFO_PATH.to_string(), file("2", 0o644));

        let d = diff(&a, &b, true);
        let shown: Vec<String> = d.iter().map(|d| d.to_string()).collect();
        assert_eq!(
            shown,
            [
                "mode: /chmod",
                "owner: /chown",
                "contents: /edited",
                "only in A: /gone",
                "link target: /link",
                "type: /retyped",
                "contents: /usr/local/share/juanitos/release.toml",
                "only in B: /new",
            ]
        );
        assert_eq!(d.iter().filter(|d| d.expected().is_some()).count(), 1);

        // Without root, owners say nothing.
        assert!(!diff(&a, &b, false)
            .iter()
            .any(|d| d.to_string().contains("owner")));
    }

    #[test]
    fn the_report_separates_expected_from_unexpected() {
        let r = Report {
            entries: (3, 3),
            owners_compared: false,
            differences: vec![Difference::Changed(
                "/var/log/userlog".into(),
                vec!["contents"],
            )],
        };
        let text = r.render();
        assert!(text.contains("A: 3 entries, B: 3 entries"));
        assert!(text.contains("owners were not compared"));
        assert!(text.contains("expected  contents: /var/log/userlog (pkg logs"));
        assert!(text.ends_with("same system: no unexpected differences\n"));

        let r = Report {
            differences: vec![Difference::OnlyInB("/x".into())],
            owners_compared: true,
            ..r
        };
        assert_eq!(r.unexpected().count(), 1);
        let text = r.render();
        assert!(text.contains("DIFFERS   only in B: /x"));
        assert!(text.ends_with("1 unexpected difference(s)\n"));
        assert!(!text.contains("owners"));
        // Every expected path is absolute, like the snapshot's keys.
        assert!(EXPECTED
            .iter()
            .all(|(p, why)| p.starts_with('/') && !why.is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_record_what_matters() {
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new("snap");
        let f = s.put("t/usr/bin/tool", b"hello");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o4555)).unwrap();
        std::os::unix::fs::symlink("tool", s.0.join("t/usr/bin/alias")).unwrap();
        std::fs::create_dir_all(s.0.join("t/var/empty")).unwrap();
        std::fs::set_permissions(
            s.0.join("t/var/empty"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        let snap = snapshot(&s.0.join("t")).unwrap();
        let keys: Vec<&str> = snap.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "/usr",
                "/usr/bin",
                "/usr/bin/alias",
                "/usr/bin/tool",
                "/var",
                "/var/empty"
            ]
        );
        assert_eq!(
            snap["/usr/bin/tool"].kind,
            Kind::File(juanitos_core::release::sha256_bytes(b"hello"))
        );
        assert_eq!(snap["/usr/bin/tool"].mode, 0o4555);
        assert_eq!(snap["/usr/bin/alias"].kind, Kind::Symlink("tool".into()));
        assert_eq!(snap["/var/empty"].mode, 0o555);
        assert!(snapshot(&s.0.join("missing")).is_err());

        // A fifo is neither file, directory nor link.
        let fifo = s.0.join("t/fifo");
        if std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|st| st.success())
            .unwrap_or(false)
        {
            assert_eq!(snapshot(&s.0.join("t")).unwrap()["/fifo"].kind, Kind::Other);
        }
    }

    /// Two archives of the same tree differ in member order and mtimes; the
    /// comparison sees through both, and through to a real change. Needs
    /// the host's `tar`.
    #[cfg(unix)]
    #[test]
    fn real_tarballs_compare_by_contents() {
        let s = Scratch::new("tar");
        s.put("root/etc/rc.conf", b"hostname=x\n");
        s.put("root/usr/local/bin/jos", b"\x7fELF");
        s.put(
            &format!("root{}", "/var/log/userlog"),
            b"installed at 10:00\n",
        );
        std::os::unix::fs::symlink("jos", s.0.join("root/usr/local/bin/j")).unwrap();
        let tar = |name: &str, members: &[&str]| {
            let out = s.0.join(name);
            let st = std::process::Command::new("tar")
                .arg("-C")
                .arg(s.0.join("root"))
                .arg("-cf")
                .arg(&out)
                .args(members)
                .status()
                .expect("this test needs tar on PATH");
            assert!(st.success());
            out
        };
        let a = tar("a.tar", &["./usr", "./etc", "./var"]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        s.put("root/var/log/userlog", b"installed at 11:00\n");
        let b = tar("b.tar", &["./var", "./etc", "./usr"]);

        let scratch = s.0.join("cmp");
        let fs = FsOps::new(false);
        let r = compare_tarballs(&RealRunner, &fs, &a, &b, &scratch, false).unwrap();
        assert_eq!(r.entries.0, r.entries.1);
        assert_eq!(r.unexpected().count(), 0, "{}", r.render());
        assert_eq!(r.differences.len(), 1, "{}", r.render());
        assert!(!scratch.exists(), "scratch removed");

        s.put("root/usr/local/bin/jos", b"\x7fELF2");
        let c = tar("c.tar", &["."]);
        let r = compare_tarballs(&RealRunner, &fs, &a, &c, &scratch, true).unwrap();
        let unexpected: Vec<String> = r.unexpected().map(|d| d.to_string()).collect();
        assert_eq!(unexpected, ["contents: /usr/local/bin/jos"]);
        assert!(scratch.join("b/usr/local/bin/jos").is_file(), "kept");
    }

    #[test]
    fn extraction_goes_through_the_runner() {
        let s = Scratch::new("mock");
        let m = MockRunner::new().respond("id", &["-u"], CmdOutput::ok("0\n"));
        let fs = FsOps::new(false);
        let r = compare_tarballs(
            &m,
            &fs,
            Path::new("/a.txz"),
            Path::new("/b.txz"),
            &s.0.join("cmp"),
            false,
        )
        .unwrap();
        assert!(r.owners_compared);
        assert!(r.differences.is_empty());
        let calls = m.calls();
        assert_eq!(calls[0], "id -u");
        assert_eq!(
            calls[1],
            format!("tar -xpf /a.txz -C {}", unix(&s.0.join("cmp/a")))
        );
        assert!(calls[2].starts_with("tar -xpf /b.txz -C "));

        let m = MockRunner::new().respond("tar", &[], CmdOutput::failed(1, "not an archive"));
        let err = compare_tarballs(
            &m,
            &fs,
            Path::new("/a.txz"),
            Path::new("/b.txz"),
            &s.0.join("cmp"),
            false,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("not an archive"), "{err:#}");
    }
}
