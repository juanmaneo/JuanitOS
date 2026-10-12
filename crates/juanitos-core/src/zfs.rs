//! ZFS dataset / zvol management.
//!
//! Layout (defaults):
//!
//! ```text
//! zroot/juanitos
//! zroot/juanitos/templates/<name>   zvol, snapshotted as @base, @<date>...
//! zroot/juanitos/qubes/<name>       zvol, cloned from a template snapshot
//! ```

use crate::config::{GlobalConfig, Qube};
use crate::error::{Error, Result};
use crate::sys::Runner;

pub struct Zfs<'a> {
    pub runner: &'a dyn Runner,
}

impl<'a> Zfs<'a> {
    pub fn new(runner: &'a dyn Runner) -> Self {
        Zfs { runner }
    }

    pub fn exists(&self, dataset: &str) -> Result<bool> {
        Ok(self
            .runner
            .run("zfs", &["list", "-H", "-o", "name", dataset])?
            .success())
    }

    /// `zfs create -p` is idempotent for filesystems.
    pub fn ensure_filesystem(&self, dataset: &str) -> Result<()> {
        if !self.exists(dataset)? {
            self.runner.run_checked("zfs", &["create", "-p", dataset])?;
        }
        Ok(())
    }

    /// Sparse zvol exposed only as `/dev/zvol/...` (no GEOM tasting).
    pub fn create_zvol(&self, dataset: &str, size_gb: u64) -> Result<()> {
        let size = format!("{size_gb}G");
        self.runner.run_checked(
            "zfs",
            &["create", "-V", &size, "-s", "-o", "volmode=dev", dataset],
        )?;
        Ok(())
    }

    pub fn snapshot(&self, dataset: &str, name: &str) -> Result<String> {
        let snap = format!("{dataset}@{name}");
        self.runner.run_checked("zfs", &["snapshot", &snap])?;
        Ok(snap)
    }

    pub fn clone(&self, snapshot: &str, target: &str) -> Result<()> {
        self.runner
            .run_checked("zfs", &["clone", "-o", "volmode=dev", snapshot, target])?;
        Ok(())
    }

    pub fn destroy(&self, dataset: &str) -> Result<()> {
        self.runner
            .run_checked("zfs", &["destroy", "-r", dataset])?;
        Ok(())
    }

    /// Direct children of `dataset` (names only, without the parent prefix).
    pub fn list_children(&self, dataset: &str) -> Result<Vec<String>> {
        let out = self.runner.run_checked(
            "zfs",
            &["list", "-H", "-r", "-d", "1", "-o", "name", dataset],
        )?;
        let prefix = format!("{dataset}/");
        Ok(out
            .stdout
            .lines()
            .map(str::trim)
            .filter_map(|l| l.strip_prefix(&prefix))
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect())
    }

    /// Most recently created snapshot of `dataset`, as `dataset@snap`.
    /// A failed listing (e.g. the dataset does not exist) yields `None`.
    pub fn latest_snapshot(&self, dataset: &str) -> Result<Option<String>> {
        let out = self.runner.run(
            "zfs",
            &[
                "list", "-H", "-t", "snapshot", "-o", "name", "-s", "creation", "-d", "1", dataset,
            ],
        )?;
        if !out.success() {
            return Ok(None);
        }
        let prefix = format!("{dataset}@");
        Ok(out
            .stdout
            .lines()
            .map(str::trim)
            .rfind(|l| l.starts_with(&prefix))
            .map(String::from))
    }
}

/// Qube-level storage operations on top of [`Zfs`].
pub struct Storage<'a> {
    pub zfs: Zfs<'a>,
    pub cfg: &'a GlobalConfig,
}

impl<'a> Storage<'a> {
    pub fn new(runner: &'a dyn Runner, cfg: &'a GlobalConfig) -> Self {
        Storage {
            zfs: Zfs::new(runner),
            cfg,
        }
    }

    /// Create the root, templates and qubes datasets.
    pub fn init(&self) -> Result<()> {
        self.zfs.ensure_filesystem(&self.cfg.root_dataset())?;
        self.zfs.ensure_filesystem(&self.cfg.templates_dataset())?;
        self.zfs.ensure_filesystem(&self.cfg.qubes_dataset())?;
        Ok(())
    }

    /// Create the disk for `q`. If `q.template` is set, clone the template's
    /// latest snapshot (creating `@base` first if it has none); otherwise
    /// create a fresh empty zvol. Unikernel qubes boot from a read-only
    /// image and get no zvol.
    pub fn provision(
        &self,
        q: &Qube,
        template_lookup: impl Fn(&str) -> Option<Qube>,
    ) -> Result<()> {
        if q.is_unikernel() {
            return Ok(());
        }
        let target = self.cfg.dataset_for(q);
        if self.zfs.exists(&target)? {
            return Err(Error::Other(format!("dataset {target} already exists")));
        }
        match q.template.as_deref() {
            Some(tname) => {
                let template =
                    template_lookup(tname).ok_or_else(|| Error::QubeNotFound(tname.to_string()))?;
                if !template.is_template() {
                    return Err(Error::Other(format!("{tname} is not a template")));
                }
                let tds = self.cfg.dataset_for(&template);
                let snap = match self.zfs.latest_snapshot(&tds)? {
                    Some(s) => s,
                    None => self.zfs.snapshot(&tds, "base")?,
                };
                self.zfs.clone(&snap, &target)
            }
            None => self.zfs.create_zvol(&target, q.disk_gb),
        }
    }

    pub fn destroy(&self, q: &Qube) -> Result<()> {
        if q.is_unikernel() {
            return Ok(());
        }
        let ds = self.cfg.dataset_for(q);
        if self.zfs.exists(&ds)? {
            self.zfs.destroy(&ds)?;
        }
        Ok(())
    }

    /// Snapshot a template so new qubes clone from it.
    pub fn snapshot_template(&self, name: &str, snap: &str) -> Result<String> {
        let ds = format!("{}/{name}", self.cfg.templates_dataset());
        self.zfs.snapshot(&ds, snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OsType, QubeKind};
    use crate::sys::{CmdOutput, MockRunner};

    #[test]
    fn zfs_arg_construction() {
        let m = MockRunner::new()
            .respond(
                "zfs",
                &["list", "-H", "-o", "name", "zroot/x"],
                CmdOutput::failed(1, ""),
            )
            .respond(
                "zfs",
                &["list", "-H", "-r", "-d", "1", "-o", "name", "zroot/p"],
                CmdOutput::ok("zroot/p\nzroot/p/a\nzroot/p/b\n"),
            )
            .respond(
                "zfs",
                &["list", "-H", "-t", "snapshot"],
                CmdOutput::ok("zroot/t@base\nzroot/t@2026-01-01\n"),
            );
        let z = Zfs::new(&m);
        assert!(!z.exists("zroot/x").unwrap());
        assert!(z.exists("zroot/y").unwrap());
        z.ensure_filesystem("zroot/x").unwrap();
        z.create_zvol("zroot/x/v", 20).unwrap();
        assert_eq!(z.snapshot("zroot/t", "base").unwrap(), "zroot/t@base");
        z.clone("zroot/t@base", "zroot/q/w").unwrap();
        z.destroy("zroot/q/w").unwrap();
        assert_eq!(z.list_children("zroot/p").unwrap(), vec!["a", "b"]);
        assert_eq!(
            z.latest_snapshot("zroot/t").unwrap().as_deref(),
            Some("zroot/t@2026-01-01")
        );

        let calls = m.calls();
        assert!(calls.contains(&"zfs create -p zroot/x".to_string()));
        assert!(calls.contains(&"zfs create -V 20G -s -o volmode=dev zroot/x/v".to_string()));
        assert!(calls.contains(&"zfs snapshot zroot/t@base".to_string()));
        assert!(calls.contains(&"zfs clone -o volmode=dev zroot/t@base zroot/q/w".to_string()));
        assert!(calls.contains(&"zfs destroy -r zroot/q/w".to_string()));
    }

    #[test]
    fn provision_clones_template_creating_base_snapshot() {
        let cfg = GlobalConfig::default();
        let m = MockRunner::new()
            .respond(
                "zfs",
                &["list", "-H", "-o", "name", "zroot/juanitos/qubes/work"],
                CmdOutput::failed(1, "does not exist"),
            )
            .respond("zfs", &["list", "-H", "-t", "snapshot"], CmdOutput::ok(""));
        let st = Storage::new(&m, &cfg);
        let mut q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        q.template = Some("fbsd14".into());
        let lookup = |n: &str| {
            (n == "fbsd14").then(|| Qube::new("fbsd14", QubeKind::Template, OsType::Freebsd))
        };
        st.provision(&q, lookup).unwrap();
        let calls = m.calls();
        assert!(calls.contains(&"zfs snapshot zroot/juanitos/templates/fbsd14@base".to_string()));
        assert!(calls.contains(
            &"zfs clone -o volmode=dev zroot/juanitos/templates/fbsd14@base zroot/juanitos/qubes/work"
                .to_string()
        ));
    }

    #[test]
    fn provision_fresh_zvol_and_missing_template() {
        let cfg = GlobalConfig::default();
        let m = MockRunner::new().respond("zfs", &["list", "-H", "-o"], CmdOutput::failed(1, ""));
        let st = Storage::new(&m, &cfg);
        let mut q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        q.disk_gb = 40;
        st.provision(&q, |_| None).unwrap();
        assert!(m.calls().contains(
            &"zfs create -V 40G -s -o volmode=dev zroot/juanitos/qubes/work".to_string()
        ));

        q.template = Some("nope".into());
        assert!(matches!(
            st.provision(&q, |_| None),
            Err(Error::QubeNotFound(_))
        ));
    }

    #[test]
    fn unikernel_has_no_storage() {
        let cfg = GlobalConfig::default();
        let m = MockRunner::new();
        let st = Storage::new(&m, &cfg);
        let q = Qube::new("sys-firewall", QubeKind::Firewall, OsType::Unikernel);
        st.provision(&q, |_| None).unwrap();
        st.destroy(&q).unwrap();
        assert!(m.calls().is_empty());
    }

    #[test]
    fn init_creates_three_datasets() {
        let cfg = GlobalConfig::default();
        let m = MockRunner::new().respond("zfs", &["list"], CmdOutput::failed(1, ""));
        Storage::new(&m, &cfg).init().unwrap();
        let calls = m.calls();
        assert!(calls.contains(&"zfs create -p zroot/juanitos".to_string()));
        assert!(calls.contains(&"zfs create -p zroot/juanitos/templates".to_string()));
        assert!(calls.contains(&"zfs create -p zroot/juanitos/qubes".to_string()));
    }
}
