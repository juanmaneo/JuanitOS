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
        fs::remove_dir_all(path).map_err(Into::into)
    }

    /// Remove a single file; a missing file is not an error.
    pub fn remove_file(&self, path: &Path) -> Result<()> {
        if self.dry_run {
            println!("+ rm -f {}", path.display());
            return Ok(());
        }
        match fs::remove_file(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            done => done.map_err(Into::into),
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
        create_parent(path)
            .and_then(|()| fs::write(path, content))
            .map_err(Into::into)
    }

    /// Write a file and mark it executable (no-op bit-wise on non-Unix
    /// hosts, where only dry runs make sense anyway).
    pub fn write_executable(&self, path: &Path, content: &str) -> Result<()> {
        self.write(path, content)
            .and_then(|()| self.chmod(path, 0o755))
    }

    pub fn chmod(&self, path: &Path, mode: u32) -> Result<()> {
        if self.dry_run {
            println!("+ chmod {mode:o} {}", path.display());
            return Ok(());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(Into::into)
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Ok(())
        }
    }

    pub fn copy(&self, from: &Path, to: &Path) -> Result<()> {
        if self.dry_run {
            println!("+ cp {} {}", from.display(), to.display());
            return Ok(());
        }
        create_parent(to)
            .and_then(|()| fs::copy(from, to))
            .map(drop)
            .map_err(Into::into)
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
                let names = fs::create_dir_all(&dst)
                    .and_then(|()| fs::read_dir(&src))
                    .and_then(|entries| {
                        entries
                            .map(|e| e.map(|e| e.file_name()))
                            .collect::<std::io::Result<Vec<_>>>()
                    })?;
                stack.extend(names.into_iter().map(|n| rel.join(n)));
            } else {
                let copied = if meta.file_type().is_symlink() {
                    copy_symlink(&src, &dst)
                } else {
                    fs::copy(&src, &dst).map(drop)
                };
                copied?;
                count += 1;
            }
        }
        Ok(count)
    }
}

/// The directory `path` goes into (none for a bare root).
pub(crate) fn create_parent(path: &Path) -> std::io::Result<()> {
    path.parent().map_or(Ok(()), fs::create_dir_all)
}

fn is_excluded(rel: &Path, excludes: &[&str]) -> bool {
    let rel = rel.to_string_lossy().replace('\\', "/");
    excludes
        .iter()
        .any(|ex| rel == *ex || rel.starts_with(&format!("{ex}/")))
}

/// A link copied as a link (replacing one copied before).
#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::read_link(src).and_then(|target| {
        let _ = fs::remove_file(dst);
        std::os::unix::fs::symlink(target, dst)
    })
}

#[cfg(not(unix))]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    // Symlinks need privileges on Windows; copy the target's content
    // instead, which is fine for source trees.
    fs::copy(src, dst).map(drop)
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
        let copied = dry.copy_tree(&src, &base.join("never"), &["target"]);
        assert_eq!(copied.unwrap(), 0);
        assert!(!base.join("never").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_copied_as_links_and_failures_surface() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("jos-fsops-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("file"), "x").unwrap();
        std::os::unix::fs::symlink("file", src.join("link")).unwrap();
        let fs_ops = FsOps::new(false);
        // Twice: the second copy replaces the link made by the first.
        for _ in 0..2 {
            assert_eq!(fs_ops.copy_tree(&src, &dst, &[]).unwrap(), 2);
        }
        assert_eq!(fs::read_link(dst.join("link")).unwrap(), Path::new("file"));

        // Where the copy cannot go: a directory in the way of a file, and
        // of a link (not removable as a file).
        let blocked = base.join("blocked");
        fs::create_dir_all(blocked.join("file/x")).unwrap();
        assert!(fs_ops.copy_tree(&src, &blocked, &["link"]).is_err());
        fs::create_dir_all(blocked.join("link/x")).unwrap();
        assert!(fs_ops.copy_tree(&src, &blocked, &["file"]).is_err());
        // A source that is missing, a destination under a file, a
        // directory that cannot be listed.
        assert!(fs_ops.copy_tree(&base.join("none"), &dst, &[]).is_err());
        let file = src.join("file");
        assert!(fs_ops.copy_tree(&src, &file.join("sub"), &[]).is_err());
        let closed = base.join("closed");
        fs::create_dir_all(&closed).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        let listed = fs_ops.copy_tree(&closed, &base.join("out"), &[]);
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(listed.is_err());

        // The single-file operations.
        assert!(fs_ops.write(&file.join("sub"), "x").is_err());
        assert!(fs_ops.copy(&file, &file.join("sub")).is_err());
        assert!(fs_ops.chmod(&base.join("none"), 0o644).is_err());
        assert!(fs_ops.write_executable(&file.join("sub"), "x").is_err());
        assert!(fs_ops.remove_file(&src).is_err(), "a directory");
        fs_ops.remove_file(&base.join("none")).unwrap();
        assert!(fs_ops.remove_tree(&file).is_err(), "a file");
        fs_ops.remove_tree(&base.join("none")).unwrap();
        fs_ops.copy(&file, &base.join("copied/file")).unwrap();
        fs_ops
            .write_executable(&base.join("run.sh"), "#!/bin/sh\n")
            .unwrap();
        let mode = fs::metadata(base.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        let _ = fs::remove_dir_all(&base);
    }
}
