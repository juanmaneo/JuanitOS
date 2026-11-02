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
    pub grub: Grub,
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

/// The GRUB release whose headers build `freebsd/grub/jos_efi.mod` (the
/// module Ventoy's GRUB loads for its direct kernel boot).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Grub {
    pub version: String,
    /// The source tarball; only its `include/` directory is used.
    pub url: String,
    pub sha256: String,
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
            grub: Grub::default(),
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

impl Default for Grub {
    fn default() -> Self {
        Grub {
            version: "2.04".into(),
            url: "https://ftp.gnu.org/gnu/grub/grub-2.04.tar.xz".into(),
            sha256: "e5292496995ad42dabe843a0192cf2a2c502e7ffcc7479398232b10a472df77d".into(),
        }
    }
}

impl Grub {
    /// The tarball's file name (`grub-2.04.tar.xz`).
    pub fn tarball(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or(&self.url)
    }

    /// The directory the tarball unpacks to (`grub-2.04`).
    pub fn source_dir(&self) -> String {
        format!("grub-{}", self.version)
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
        assert_eq!(p.grub, Grub::default());
    }

    #[test]
    fn grub_names_follow_the_url_and_version() {
        let g = Grub::default();
        assert_eq!(g.tarball(), "grub-2.04.tar.xz");
        assert_eq!(g.source_dir(), "grub-2.04");
        let g = Grub {
            version: "2.12".into(),
            url: "grub-2.12.tar.gz".into(),
            sha256: String::new(),
        };
        assert_eq!(g.tarball(), "grub-2.12.tar.gz");
        assert_eq!(g.source_dir(), "grub-2.12");
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
    fn load_names_the_file_it_cannot_parse() {
        let path = std::env::temp_dir().join(format!("jos-profile-{}.toml", std::process::id()));
        std::fs::write(&path, "arch = ").unwrap();
        let err = Profile::load(&path).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(err.to_string(), format!("parsing {}", path.display()));
    }

    #[test]
    fn partial_toml_uses_defaults() {
        let p: Profile = toml::from_str("freebsd_version = \"15.0-RELEASE\"\n").unwrap();
        assert_eq!(p.freebsd_version, "15.0-RELEASE");
        assert_eq!(p.label, "JUANITOS");
        assert!(p.source.vendor);
        assert_eq!(p.packages.repo, "quarterly");
        assert!(p.packages.local_repo.is_none());
        assert_eq!(p.grub.version, "2.04");
    }
}
