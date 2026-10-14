//! The per-qube share: a dom0 directory exported to the guest over
//! virtio-9p, the channel for everything dom0 tells a qube and for
//! `jos copy` (the inter-qube file transfer).
//!
//! Layout of `<share_dir>/<qube>/` on dom0, seen as `/mnt/jos` in the guest:
//!
//! | entry  | who writes       | purpose                                      |
//! |--------|------------------|----------------------------------------------|
//! | `name` | dom0, every boot | the qube's name; the guest agent makes it the |
//! |        |                  | hostname (clones all boot the template's)     |
//! | `in/`  | dom0             | inbox: `in/<source>/...` for files `jos copy` |
//! |        |                  | brought from another qube or from dom0        |
//! | `out/` | the guest        | outbox: `jos copy` has the source qube drop   |
//! |        |                  | files here, dom0 moves them to the inbox      |
//!
//! Only dom0 moves files between qubes, and only when told to (`jos copy`),
//! so a qube never sees another qube's share: the same shape as the
//! `qvm-copy-to-vm` service of QubesOS, without a policy daemon yet.
//! Inboxes live on dom0 storage, not in the zvol, so they survive the
//! qube being recreated from its template.
//!
//! Redox and unikernel qubes have no share (no 9p client); templates do,
//! so a maintenance boot exercises the guest agent.

use std::path::Path;

use crate::config::{GlobalConfig, Qube};
use crate::error::{Error, Result};
use crate::sys::{shell_quote, Runner};

/// bhyve mount tag (`-s N,virtio-9p,<tag>=<dir>`), what the guest mounts.
pub const MOUNT_TAG: &str = "jos";
/// Where the guest agent mounts the share.
pub const GUEST_MOUNT: &str = "/mnt/jos";
/// File holding the qube's name, written by dom0 before every boot.
pub const NAME_FILE: &str = "name";
/// Inbox directory, filled by dom0.
pub const INBOX: &str = "in";
/// Outbox directory, filled by the guest on dom0's request.
pub const OUTBOX: &str = "out";
/// Inbox subdirectory for files coming from dom0 itself.
pub const DOM0: &str = "dom0";

/// Where a `jos copy` takes its files from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A path inside a qube (`qube:/path`).
    Qube { name: String, path: String },
    /// A path on dom0 (`dom0:/path`, or a bare path).
    Dom0 { path: String },
}

impl Source {
    /// Parse the `SRC` argument of `jos copy`: `NAME:PATH` is a path inside
    /// qube `NAME` (or dom0 when `NAME` is `dom0`); anything that does not
    /// start with a valid qube name and a colon is a dom0 path.
    pub fn parse(s: &str) -> Result<Source> {
        if let Some((name, path)) = s.split_once(':') {
            if name == DOM0 {
                return Self::dom0(path);
            }
            if crate::config::validate_name(name).is_ok() {
                if path.is_empty() {
                    return Err(Error::Other(format!(
                        "{s}: missing path after the qube name"
                    )));
                }
                return Ok(Source::Qube {
                    name: name.to_string(),
                    path: path.to_string(),
                });
            }
        }
        Self::dom0(s)
    }

    fn dom0(path: &str) -> Result<Source> {
        if path.is_empty() {
            return Err(Error::Other("empty source path".into()));
        }
        Ok(Source::Dom0 {
            path: path.to_string(),
        })
    }

    /// The qube the files come from, if any.
    pub fn qube(&self) -> Option<&str> {
        match self {
            Source::Qube { name, .. } => Some(name),
            Source::Dom0 { .. } => None,
        }
    }

    /// Name of the inbox subdirectory the files land in.
    pub fn label(&self) -> &str {
        self.qube().unwrap_or(DOM0)
    }

    pub fn path(&self) -> &str {
        match self {
            Source::Qube { path, .. } | Source::Dom0 { path } => path,
        }
    }

    /// The last component of the source path, which names the copy in the
    /// inbox. Trailing slashes are ignored; `.`, `..` and the root are
    /// refused because the inbox entry must be a single, new name.
    pub fn basename(&self) -> Result<String> {
        let trimmed = self.path().trim_end_matches('/');
        let base = trimmed.rsplit('/').next().unwrap_or("");
        if base.is_empty() || base == "." || base == ".." {
            return Err(Error::Other(format!(
                "{}: cannot copy that path itself; name a file or directory",
                self.path()
            )));
        }
        Ok(base.to_string())
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.label(), self.path())
    }
}

/// Shell command `jos copy` runs inside the source qube: refresh the
/// outbox copy of `path` under its basename, failing clearly when the
/// share is not mounted (the guest agent is missing from the template).
/// Symbolic links are followed: the share cannot hold them (bhyve's 9p
/// server refuses to create them), and `/etc/motd` is one on FreeBSD.
pub fn guest_export_command(path: &str, basename: &str) -> String {
    let out = format!("{GUEST_MOUNT}/{OUTBOX}");
    let target = shell_quote(&format!("{out}/{basename}"));
    format!(
        "[ -d {out} ] || {{ echo 'jos share not mounted at {GUEST_MOUNT}' >&2; exit 2; }}; \
         rm -rf {target} && cp -RL {} {target}",
        shell_quote(path)
    )
}

/// The bhyve device argument exporting `q`'s share.
pub fn device_arg(cfg: &GlobalConfig, q: &Qube) -> String {
    format!(
        "{},virtio-9p,{MOUNT_TAG}={}",
        crate::bhyve::SHARE_SLOT,
        cfg.share_dir_for(&q.name)
    )
}

/// What `jos copy` did, for the caller to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// Where the files are on dom0.
    pub dom0_path: String,
    /// Where the destination qube sees them.
    pub guest_path: String,
}

/// Share operations on dom0, all through the [`Runner`].
pub struct Share<'a> {
    pub runner: &'a dyn Runner,
    pub cfg: &'a GlobalConfig,
}

impl<'a> Share<'a> {
    pub fn new(runner: &'a dyn Runner, cfg: &'a GlobalConfig) -> Self {
        Share { runner, cfg }
    }

    /// `<share_dir>/<qube>`.
    pub fn dir(&self, name: &str) -> String {
        self.cfg.share_dir_for(name)
    }

    /// `<share_dir>/<qube>/in/<label>`: where files from `label` land.
    pub fn inbox(&self, name: &str, label: &str) -> String {
        format!("{}/{INBOX}/{label}", self.dir(name))
    }

    /// `<share_dir>/<qube>/out`.
    pub fn outbox(&self, name: &str) -> String {
        format!("{}/{OUTBOX}", self.dir(name))
    }

    /// Create the share before a boot and refresh the `name` file. The
    /// inbox and outbox are world-writable with the sticky bit so the
    /// guest's unprivileged user can use them.
    pub fn prepare(&self, q: &Qube) -> Result<()> {
        if !q.os.has_share() {
            return Ok(());
        }
        let dir = self.dir(&q.name);
        let inbox = format!("{dir}/{INBOX}");
        let outbox = format!("{dir}/{OUTBOX}");
        self.runner.run_checked("mkdir", &["-p", &inbox, &outbox])?;
        self.runner
            .run_checked("chmod", &["1777", &inbox, &outbox])?;
        let name_file = format!("{dir}/{NAME_FILE}");
        self.runner
            .run_with_input_checked("tee", &[&name_file], &format!("{}\n", q.name))?;
        Ok(())
    }

    /// Delete a qube's share (with its inbox) when the qube is removed.
    pub fn destroy(&self, name: &str) -> Result<()> {
        crate::config::validate_name(name)?;
        self.runner.run_checked("rm", &["-rf", &self.dir(name)])?;
        Ok(())
    }

    /// Copy `src` into `dst`'s inbox. `run_in_qube(qube, command)` runs a
    /// shell command inside a qube (the CLI does it over the serial
    /// console); it is only called when the source is a qube, to have it
    /// fill its outbox. Everything on dom0 goes through the runner, so a
    /// dry run prints the plan.
    ///
    /// An inbox entry of the same name is replaced. Copies from dom0 are
    /// made writable for everyone (the guest user cannot otherwise delete
    /// what root put there); copies between qubes keep their owner, the
    /// same `user` uid in every template.
    pub fn copy(
        &self,
        src: &Source,
        dst: &Qube,
        run_in_qube: &mut dyn FnMut(&str, &str) -> Result<()>,
    ) -> Result<Delivery> {
        if !dst.os.has_share() {
            return Err(Error::Other(format!(
                "{} runs {} which has no dom0 share; cannot copy into it",
                dst.name, dst.os
            )));
        }
        if src.qube() == Some(dst.name.as_str()) {
            return Err(Error::Other(format!(
                "{}: source and destination are the same qube",
                dst.name
            )));
        }
        let base = src.basename()?;
        let inbox = self.inbox(&dst.name, src.label());
        let target = format!("{inbox}/{base}");
        self.runner.run_checked("mkdir", &["-p", &inbox])?;
        self.runner.run_checked("chmod", &["777", &inbox])?;
        self.runner.run_checked("rm", &["-rf", &target])?;
        match src {
            Source::Qube { name, path } => {
                run_in_qube(name, &guest_export_command(path, &base))?;
                let staged = format!("{}/{base}", self.outbox(name));
                self.runner.run_checked("mv", &[&staged, &target])?;
            }
            Source::Dom0 { path } => {
                self.runner.run_checked("cp", &["-RL", path, &target])?;
                self.runner
                    .run_checked("chmod", &["-R", "a+rwX", &target])?;
            }
        }
        Ok(Delivery {
            dom0_path: target,
            guest_path: format!("{GUEST_MOUNT}/{INBOX}/{}/{base}", src.label()),
        })
    }
}

/// True when `path` looks like something `jos copy` can read on dom0 (a
/// plain existence check; the copy itself reports the real error).
pub fn dom0_path_exists(path: &str) -> bool {
    Path::new(path).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OsType, QubeKind};
    use crate::sys::{CmdOutput, MockRunner};

    #[test]
    fn source_parsing() {
        assert_eq!(
            Source::parse("work:/etc/motd").unwrap(),
            Source::Qube {
                name: "work".into(),
                path: "/etc/motd".into()
            }
        );
        assert_eq!(
            Source::parse("dom0:/root/x").unwrap(),
            Source::Dom0 {
                path: "/root/x".into()
            }
        );
        assert_eq!(
            Source::parse("/root/x").unwrap(),
            Source::Dom0 {
                path: "/root/x".into()
            }
        );
        // Not a qube name before the colon: a dom0 path with a colon in it.
        assert_eq!(
            Source::parse("./odd:name").unwrap(),
            Source::Dom0 {
                path: "./odd:name".into()
            }
        );
        assert!(Source::parse("work:").is_err());
        assert!(Source::parse("").is_err());
        assert!(Source::parse("dom0:").is_err());
    }

    #[test]
    fn source_accessors_and_display() {
        let q = Source::parse("work:/home/user/notes/").unwrap();
        assert_eq!(q.qube(), Some("work"));
        assert_eq!(q.label(), "work");
        assert_eq!(q.basename().unwrap(), "notes");
        assert_eq!(q.to_string(), "work:/home/user/notes/");
        let d = Source::parse("/root/file.txt").unwrap();
        assert_eq!(d.qube(), None);
        assert_eq!(d.label(), DOM0);
        assert_eq!(d.basename().unwrap(), "file.txt");
        assert_eq!(d.to_string(), "dom0:/root/file.txt");
    }

    #[test]
    fn basename_refuses_root_and_dots() {
        for p in ["/", "work:/", ".", "..", "work:/x/..", "./"] {
            let s = Source::parse(p).unwrap();
            assert!(s.basename().is_err(), "{p}");
        }
    }

    #[test]
    fn guest_command_quotes_and_checks_the_mount() {
        let c = guest_export_command("/home/user/my file", "my file");
        assert!(c.starts_with("[ -d /mnt/jos/out ] ||"));
        assert!(c.contains(
            "rm -rf '/mnt/jos/out/my file' && cp -RL '/home/user/my file' '/mnt/jos/out/my file'"
        ));
        let c = guest_export_command("/etc/motd", "motd");
        assert!(c.ends_with("rm -rf /mnt/jos/out/motd && cp -RL /etc/motd /mnt/jos/out/motd"));
    }

    #[test]
    fn prepare_creates_boxes_and_name_file() {
        let runner = MockRunner::new();
        let cfg = GlobalConfig::default();
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        Share::new(&runner, &cfg).prepare(&q).unwrap();
        assert_eq!(
            runner.calls(),
            vec![
                "mkdir -p /var/db/juanitos/share/work/in /var/db/juanitos/share/work/out",
                "chmod 1777 /var/db/juanitos/share/work/in /var/db/juanitos/share/work/out",
                "tee /var/db/juanitos/share/work/name",
            ]
        );
    }

    #[test]
    fn prepare_skips_qubes_without_share() {
        let runner = MockRunner::new();
        let cfg = GlobalConfig::default();
        for os in [OsType::Redox, OsType::Unikernel] {
            let q = Qube::new("x", QubeKind::App, os);
            Share::new(&runner, &cfg).prepare(&q).unwrap();
        }
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn prepare_reports_failures() {
        let runner = MockRunner::new().respond("mkdir", &[], CmdOutput::failed(1, "ro"));
        let cfg = GlobalConfig::default();
        let q = Qube::new("work", QubeKind::App, OsType::Linux);
        assert!(Share::new(&runner, &cfg).prepare(&q).is_err());
    }

    #[test]
    fn device_arg_and_destroy() {
        let runner = MockRunner::new();
        let cfg = GlobalConfig {
            share_dir: "/srv/share".into(),
            ..Default::default()
        };
        let share = Share::new(&runner, &cfg);
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        assert_eq!(device_arg(&cfg, &q), "28,virtio-9p,jos=/srv/share/work");
        share.destroy("work").unwrap();
        assert_eq!(runner.calls(), vec!["rm -rf /srv/share/work"]);
        assert!(share.destroy("../etc").is_err());
    }

    #[test]
    fn copy_between_qubes() {
        let runner = MockRunner::new();
        let cfg = GlobalConfig::default();
        let share = Share::new(&runner, &cfg);
        let dst = Qube::new("jail", QubeKind::App, OsType::Freebsd);
        let src = Source::parse("work:/etc/motd").unwrap();
        let mut ran: Vec<(String, String)> = Vec::new();
        let d = share
            .copy(&src, &dst, &mut |q, c| {
                ran.push((q.into(), c.into()));
                Ok(())
            })
            .unwrap();
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].0, "work");
        assert!(ran[0].1.ends_with("cp -RL /etc/motd /mnt/jos/out/motd"));
        assert_eq!(
            runner.calls(),
            vec![
                "mkdir -p /var/db/juanitos/share/jail/in/work",
                "chmod 777 /var/db/juanitos/share/jail/in/work",
                "rm -rf /var/db/juanitos/share/jail/in/work/motd",
                "mv /var/db/juanitos/share/work/out/motd /var/db/juanitos/share/jail/in/work/motd",
            ]
        );
        assert_eq!(d.dom0_path, "/var/db/juanitos/share/jail/in/work/motd");
        assert_eq!(d.guest_path, "/mnt/jos/in/work/motd");
    }

    #[test]
    fn copy_from_dom0() {
        let runner = MockRunner::new();
        let cfg = GlobalConfig::default();
        let share = Share::new(&runner, &cfg);
        let dst = Qube::new("gpu", QubeKind::Gpu, OsType::Linux);
        let src = Source::parse("/root/data set/").unwrap();
        let d = share
            .copy(&src, &dst, &mut |_, _| {
                panic!("no guest command for dom0 sources")
            })
            .unwrap();
        assert_eq!(
            runner.calls(),
            vec![
                "mkdir -p /var/db/juanitos/share/gpu/in/dom0",
                "chmod 777 /var/db/juanitos/share/gpu/in/dom0",
                "rm -rf '/var/db/juanitos/share/gpu/in/dom0/data set'",
                "cp -RL '/root/data set/' '/var/db/juanitos/share/gpu/in/dom0/data set'",
                "chmod -R a+rwX '/var/db/juanitos/share/gpu/in/dom0/data set'",
            ]
        );
        assert_eq!(d.guest_path, "/mnt/jos/in/dom0/data set");
    }

    #[test]
    fn copy_refusals() {
        let runner = MockRunner::new();
        let cfg = GlobalConfig::default();
        let share = Share::new(&runner, &cfg);
        let mut never = |_: &str, _: &str| -> Result<()> { panic!("not reached") };
        let redox = Qube::new("rx", QubeKind::App, OsType::Redox);
        let src = Source::parse("work:/etc/motd").unwrap();
        assert!(share.copy(&src, &redox, &mut never).is_err());
        let work = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let err = share.copy(&src, &work, &mut never).unwrap_err().to_string();
        assert!(err.contains("same qube"), "{err}");
        let bad = Source::parse("work:/").unwrap();
        assert!(share.copy(&bad, &redox, &mut never).is_err());
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn copy_stops_when_the_guest_fails() {
        let runner = MockRunner::new();
        let cfg = GlobalConfig::default();
        let share = Share::new(&runner, &cfg);
        let dst = Qube::new("jail", QubeKind::App, OsType::Freebsd);
        let src = Source::parse("work:/nope").unwrap();
        let err = share
            .copy(&src, &dst, &mut |_, _| Err(Error::Other("exit 1".into())))
            .unwrap_err();
        assert!(err.to_string().contains("exit 1"));
        // The inbox was prepared but nothing was moved.
        assert!(!runner.calls().iter().any(|c| c.starts_with("mv ")));
    }

    #[test]
    fn copy_reports_dom0_failures() {
        let runner = MockRunner::new().respond("cp", &[], CmdOutput::failed(1, "no such file"));
        let cfg = GlobalConfig::default();
        let share = Share::new(&runner, &cfg);
        let dst = Qube::new("jail", QubeKind::App, OsType::Freebsd);
        let src = Source::parse("/missing").unwrap();
        let err = share
            .copy(&src, &dst, &mut |_, _| Ok(()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such file"), "{err}");
    }

    #[test]
    fn dom0_path_check() {
        assert!(dom0_path_exists(&std::env::temp_dir().to_string_lossy()));
        assert!(!dom0_path_exists("/definitely/not/here/jos"));
    }
}
