//! `release/juanitos.toml`.

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub version: String,
    pub freebsd_version: String,
    pub arch: String,
    pub mirror: String,
    pub dists: Vec<String>,
    pub label: String,
    pub bios: bool,
    pub packages: Packages,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Packages {
    pub gui: bool,
    pub selfhost: bool,
    pub extra: Vec<String>,
    /// FreeBSD package branch the dom0 installs from: `quarterly` (what
    /// FreeBSD ships configured) or `latest`. The COSMIC packages are
    /// built against one of them and must use the same shared libraries.
    pub repo: String,
    /// A pkg(8) repository directory (`pkg repo` output) with the COSMIC
    /// packages, relative to the source tree; shipped in the dom0 at
    /// `dom0::COSMIC_REPO_DIR`. Produced by `tools/dev/cosmic-build.sh`.
    pub local_repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    pub include: bool,
    pub vendor: bool,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            version: env!("CARGO_PKG_VERSION").to_string(),
            freebsd_version: "15.1-RELEASE".into(),
            arch: "amd64".into(),
            mirror: "https://download.freebsd.org/releases".into(),
            dists: vec!["base.txz".into(), "kernel.txz".into()],
            label: "JUANITOS".into(),
            bios: true,
            packages: Packages::default(),
            source: Source::default(),
        }
    }
}

impl Default for Packages {
    fn default() -> Self {
        Packages {
            gui: true,
            selfhost: true,
            extra: Vec::new(),
            repo: "quarterly".into(),
            local_repo: None,
        }
    }
}

impl Default for Source {
    fn default() -> Self {
        Source {
            include: true,
            vendor: true,
        }
    }
}

impl Profile {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// `https://mirror/arch/version/`
    pub fn dist_url(&self, file: &str) -> String {
        format!(
            "{}/{}/{}/{}",
            self.mirror.trim_end_matches('/'),
            self.arch,
            self.freebsd_version,
            file
        )
    }

    pub fn iso_name(&self) -> String {
        format!("JuanitOS-{}-{}.iso", self.version, self.arch)
    }

    /// The package branch, validated: pkg would silently 404 on a typo.
    pub fn package_branch(&self) -> anyhow::Result<&str> {
        match self.packages.repo.as_str() {
            b @ ("quarterly" | "latest") => Ok(b),
            other => {
                anyhow::bail!("[packages] repo = {other:?}: expected \"quarterly\" or \"latest\"")
            }
        }
    }

    /// Everything to install with `pkg`, split into (required, best-effort).
    pub fn package_sets(&self) -> (Vec<&str>, Vec<&str>) {
        use juanitos_core::dom0::{GUI_PACKAGES, REQUIRED_PACKAGES, SELFHOST_PACKAGES};
        let mut required: Vec<&str> = REQUIRED_PACKAGES.to_vec();
        let mut optional: Vec<&str> = Vec::new();
        if self.packages.selfhost {
            required.extend_from_slice(SELFHOST_PACKAGES);
        }
        if self.packages.gui {
            optional.extend_from_slice(GUI_PACKAGES);
        }
        required.extend(self.packages.extra.iter().map(String::as_str));
        (required, optional)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_profile_parses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../release/juanitos.toml");
        let p = Profile::load(&path).unwrap();
        assert_eq!(p.arch, "amd64");
        assert_eq!(p.dists, vec!["base.txz", "kernel.txz"]);
        assert_eq!(
            p.dist_url("MANIFEST"),
            format!(
                "https://download.freebsd.org/releases/amd64/{}/MANIFEST",
                p.freebsd_version
            )
        );
        assert!(p.iso_name().starts_with("JuanitOS-"));
        // The dom0 tracks the branch the COSMIC packages are built on.
        assert_eq!(p.package_branch().unwrap(), "latest");
        assert_eq!(
            p.packages.local_repo.as_deref(),
            Some("target/work/cosmic/repo")
        );
    }

    #[test]
    fn package_branch_is_validated() {
        let mut p = Profile::default();
        assert_eq!(p.package_branch().unwrap(), "quarterly");
        p.packages.repo = "lastest".into();
        let err = p.package_branch().unwrap_err().to_string();
        assert!(err.contains("lastest"), "{err}");
    }

    #[test]
    fn package_sets_follow_flags() {
        let mut p = Profile::default();
        p.packages.extra = vec!["tmux".into()];
        let (req, opt) = p.package_sets();
        assert!(req.contains(&"edk2-bhyve"));
        assert!(req.contains(&"rust"));
        assert!(req.contains(&"tmux"));
        assert!(opt.contains(&"cosmic-comp"));

        p.packages.gui = false;
        p.packages.selfhost = false;
        let (req, opt) = p.package_sets();
        assert!(!req.contains(&"rust"));
        assert!(opt.is_empty());
    }

    #[test]
    fn partial_toml_uses_defaults() {
        let p: Profile = toml::from_str("freebsd_version = \"15.0-RELEASE\"\n").unwrap();
        assert_eq!(p.freebsd_version, "15.0-RELEASE");
        assert_eq!(p.label, "JUANITOS");
        assert!(p.source.vendor);
        assert_eq!(p.packages.repo, "quarterly");
        assert!(p.packages.local_repo.is_none());
    }
}
