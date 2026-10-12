//! Release artifacts: FreeBSD distribution manifests, our own dom0 tarball
//! manifest, and the `release.toml` record stamped into every dom0.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// One line of FreeBSD's `MANIFEST` file shipped next to `base.txz` etc.
///
/// Format (tab separated): `name  sha256  file-count  short-name
/// description  default(on/off)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistEntry {
    pub file: String,
    pub sha256: String,
    pub files: u64,
    pub short: String,
    pub description: String,
    pub default_on: bool,
}

pub fn parse_freebsd_manifest(text: &str) -> Vec<DistEntry> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let [file, sha256, ..] = f.as_slice() else {
                return None;
            };
            Some(DistEntry {
                file: file.to_string(),
                sha256: sha256.to_string(),
                files: f.get(2).and_then(|s| s.parse().ok()).unwrap_or(0),
                short: f.get(3).unwrap_or(&"").to_string(),
                description: f
                    .get(4)
                    .map(|s| s.trim_matches('"').to_string())
                    .unwrap_or_default(),
                default_on: f.get(5).map(|s| s.trim() == "on").unwrap_or(false),
            })
        })
        .collect()
}

/// SHA-256 of a file, hex encoded.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = BufReader::new(File::open(path)?);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        match buf.get(..n) {
            Some(chunk) if n > 0 => hasher.update(chunk),
            _ => break,
        }
    }
    Ok(hex(&hasher.finalize()))
}

pub fn sha256_bytes(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `MANIFEST` next to our dom0 tarball: `sha256  size  file` per line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DistManifest {
    pub entries: Vec<(String, u64, String)>,
}

impl DistManifest {
    pub fn add(&mut self, sha256: String, size: u64, file: String) {
        self.entries.push((sha256, size, file));
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        for (sha, size, file) in &self.entries {
            s.push_str(&format!("{sha}  {size}  {file}\n"));
        }
        s
    }

    pub fn parse(text: &str) -> Self {
        let mut m = DistManifest::default();
        for line in text.lines() {
            let mut it = line.split_whitespace();
            if let (Some(sha), Some(size), Some(file)) = (it.next(), it.next(), it.next()) {
                if let Ok(size) = size.parse() {
                    m.add(sha.to_string(), size, file.to_string());
                }
            }
        }
        m
    }

    pub fn read(path: &Path) -> Result<Self> {
        let mut text = String::new();
        BufReader::new(File::open(path)?).read_to_string(&mut text)?;
        Ok(Self::parse(&text))
    }

    pub fn expected(&self, file: &str) -> Option<(&str, u64)> {
        self.entries
            .iter()
            .find(|(_, _, f)| f == file)
            .map(|(sha, size, _)| (sha.as_str(), *size))
    }

    /// Verify `dir/file` against the manifest.
    pub fn verify(&self, dir: &Path, file: &str) -> Result<()> {
        let (want_sha, want_size) = self
            .expected(file)
            .ok_or_else(|| Error::Other(format!("{file} not in MANIFEST")))?;
        let path = dir.join(file);
        let size = std::fs::metadata(&path)?.len();
        if size != want_size {
            return Err(Error::Other(format!(
                "{file}: size {size} != {want_size} (truncated download?)"
            )));
        }
        let got = sha256_file(&path)?;
        if got != want_sha {
            return Err(Error::Other(format!(
                "{file}: sha256 mismatch\n  got  {got}\n  want {want_sha}"
            )));
        }
        Ok(())
    }

    /// Check a file against a hash the caller already has (FreeBSD's own
    /// `MANIFEST`).
    pub fn verify_sha256(path: &Path, want: &str) -> Result<()> {
        let got = sha256_file(path)?;
        if got != want {
            return Err(Error::Other(format!(
                "{}: sha256 mismatch\n  got  {got}\n  want {want}",
                path.display()
            )));
        }
        Ok(())
    }
}

/// `/usr/local/share/juanitos/release.toml`: what this dom0 was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub version: String,
    pub freebsd_version: String,
    pub arch: String,
    pub built_at: String,
    /// Where it was built: `"freebsd"` for a plain FreeBSD builder,
    /// `"juanitos"` once JuanitOS rebuilds itself (see `docs/bootstrap.md`).
    pub built_on: String,
    pub stage: u32,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub git_rev: Option<String>,
}

impl ReleaseInfo {
    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freebsd_manifest_parsing() {
        let text = "base.txz\tabc123\t28120\tbase\t\"Base system (MANDATORY)\"\ton\n\
kernel.txz\tdef456\t2100\tkernel\t\"Kernel (MANDATORY)\"\ton\n\
src.txz\t789\t100\tsrc\t\"System source tree\"\toff\n";
        let m = parse_freebsd_manifest(text);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].file, "base.txz");
        assert_eq!(m[0].sha256, "abc123");
        assert_eq!(m[0].files, 28120);
        assert_eq!(m[0].description, "Base system (MANDATORY)");
        assert!(m[0].default_on);
        assert!(!m[2].default_on);
    }

    #[test]
    fn dist_manifest_roundtrip_and_verify() {
        let dir = std::env::temp_dir().join(format!("jos-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data = b"hello juanitos\n";
        std::fs::write(dir.join("x.txz"), data).unwrap();

        let mut m = DistManifest::default();
        m.add(sha256_bytes(data), data.len() as u64, "x.txz".into());
        let text = m.render();
        let parsed = DistManifest::parse(&text);
        assert_eq!(parsed, m);
        parsed.verify(&dir, "x.txz").unwrap();

        std::fs::write(dir.join("x.txz"), b"tampered").unwrap();
        assert!(parsed.verify(&dir, "x.txz").is_err());
        assert!(parsed.verify(&dir, "missing").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn release_info_roundtrip() {
        let r = ReleaseInfo {
            version: "0.1.0".into(),
            freebsd_version: "15.1-RELEASE".into(),
            arch: "amd64".into(),
            built_at: "2026-10-12T00:00:00Z".into(),
            built_on: "freebsd".into(),
            stage: 1,
            packages: vec!["seatd".into()],
            git_rev: None,
        };
        let text = r.to_toml().unwrap();
        assert_eq!(ReleaseInfo::from_toml(&text).unwrap(), r);
    }
}
