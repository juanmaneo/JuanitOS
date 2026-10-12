//! Filesystem operations that respect `--dry-run`.
//!
//! The release builder and the installer both populate directory trees;
//! in dry-run mode they print what they would write instead.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Result;

#[derive(Debug, Clone, Copy, Default)]
pub struct FsOps {
    pub dry_run: bool,
}

impl FsOps {
    pub fn new(dry_run: bool) -> Self {
        FsOps { dry_run }
    }

    pub fn mkdir_p(&self, path: &Path) -> Result<()> {
        if self.dry_run {
            println!("+ mkdir -p {}", path.display());
            return Ok(());
        }
        fs::create_dir_all(path)?;
        Ok(())
    }

    /// Remove a directory tree (a build's own scratch output, e.g. a
    /// previous `rootfs/`). An extracted FreeBSD base carries `schg` /
    /// `sunlink` file flags that make plain unlink fail with EPERM, so on
    /// FreeBSD the flags are cleared first, as `release/Makefile` does.
    pub fn remove_tree(&self, path: &Path) -> Result<()> {
        if self.dry_run {
            println!("+ rm -rf {}", path.display());
            return Ok(());
        }
        if !path.exists() {
            return Ok(());
        }
        #[cfg(target_os = "freebsd")]
        {
            let _ = std::process::Command::new("chflags")
                .args(["-R", "noschg,nosunlink"])
                .arg(path)
                .status();
        }
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Remove a single file; a missing file is not an error.
    pub fn remove_file(&self, path: &Path) -> Result<()> {
        if self.dry_run {
            println!("+ rm -f {}", path.display());
            return Ok(());
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write `content` to `path`, creating parent directories.
    pub fn write(&self, path: &Path, content: &str) -> Result<()> {
        self.write_bytes(path, content.as_bytes())
    }

    pub fn write_bytes(&self, path: &Path, content: &[u8]) -> Result<()> {
        if self.dry_run {
            println!("+ write {} ({} bytes)", path.display(), content.len());
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        Ok(())
    }

    /// Write a file and mark it executable (no-op bit-wise on non-Unix
    /// hosts, where only dry runs make sense anyway).
    pub fn write_executable(&self, path: &Path, content: &str) -> Result<()> {
        self.write(path, content)?;
        self.chmod(path, 0o755)
    }

    pub fn chmod(&self, path: &Path, mode: u32) -> Result<()> {
        if self.dry_run {
            println!("+ chmod {mode:o} {}", path.display());
            return Ok(());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
        }
        Ok(())
    }

    pub fn copy(&self, from: &Path, to: &Path) -> Result<()> {
        if self.dry_run {
            println!("+ cp {} {}", from.display(), to.display());
            return Ok(());
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to)?;
        Ok(())
    }

    /// Recursively copy `from` into `to`, skipping any entry whose path
    /// relative to `from` starts with one of `excludes`.
    pub fn copy_tree(&self, from: &Path, to: &Path, excludes: &[&str]) -> Result<u64> {
        if self.dry_run {
            println!(
                "+ cp -R {} {} (excluding {})",
                from.display(),
                to.display(),
                excludes.join(", ")
            );
            return Ok(0);
        }
        let mut count = 0;
        let mut stack: Vec<PathBuf> = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            let src = from.join(&rel);
            let dst = to.join(&rel);
            if is_excluded(&rel, excludes) {
                continue;
            }
            let meta = fs::symlink_metadata(&src)?;
            if meta.is_dir() {
                fs::create_dir_all(&dst)?;
                for entry in fs::read_dir(&src)? {
                    stack.push(rel.join(entry?.file_name()));
                }
            } else if meta.file_type().is_symlink() {
                copy_symlink(&src, &dst)?;
                count += 1;
            } else {
                fs::copy(&src, &dst)?;
                count += 1;
            }
        }
        Ok(count)
    }
}

fn is_excluded(rel: &Path, excludes: &[&str]) -> bool {
    let rel = rel.to_string_lossy().replace('\\', "/");
    excludes
        .iter()
        .any(|ex| rel == *ex || rel.starts_with(&format!("{ex}/")))
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
    let target = fs::read_link(src)?;
    let _ = fs::remove_file(dst);
    std::os::unix::fs::symlink(target, dst)?;
    Ok(())
}

#[cfg(not(unix))]
fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
    // Symlinks need privileges on Windows; copy the target's content
    // instead, which is fine for source trees.
    fs::copy(src, dst)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusion_matching() {
        assert!(is_excluded(Path::new("target"), &["target"]));
        assert!(is_excluded(Path::new("target/debug/x"), &["target"]));
        assert!(is_excluded(Path::new("release/out"), &["release/out"]));
        assert!(!is_excluded(
            Path::new("release/juanitos.toml"),
            &["release/out"]
        ));
        assert!(!is_excluded(Path::new("targets"), &["target"]));
        assert!(!is_excluded(Path::new(""), &["target"]));
    }

    #[test]
    fn copy_tree_respects_excludes() {
        let base = std::env::temp_dir().join(format!("jos-fsops-{}", std::process::id()));
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(src.join("target/debug")).unwrap();
        fs::create_dir_all(src.join("crates")).unwrap();
        fs::write(src.join("Cargo.toml"), "x").unwrap();
        fs::write(src.join("crates/a.rs"), "y").unwrap();
        fs::write(src.join("target/debug/bin"), "z").unwrap();

        let n = FsOps::new(false)
            .copy_tree(&src, &dst, &["target"])
            .unwrap();
        assert_eq!(n, 2);
        assert!(dst.join("Cargo.toml").is_file());
        assert!(dst.join("crates/a.rs").is_file());
        assert!(!dst.join("target").exists());

        let dry = FsOps::new(true);
        dry.write(&dst.join("never"), "no").unwrap();
        assert!(!dst.join("never").exists());
        let _ = fs::remove_dir_all(&base);
    }
}
