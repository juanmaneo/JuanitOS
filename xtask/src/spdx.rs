// SPDX-License-Identifier: BSD-3-Clause
//! Every file in the repository names its license: an
//! `SPDX-License-Identifier:` comment in its first lines, or an entry in
//! `REUSE.toml` (REUSE 3.2 format) for the files that cannot carry one
//! (binary, generated, vendored as upstream has it). The identifiers must be
//! ones LICENSE and THIRD_PARTY.md account for.
//!
//! The files are the ones git knows about, untracked ones included (minus
//! the ignored), so a new file is checked before it is added.

use std::io::Read as _;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};
use serde::Deserialize;

const TAG: &str = "SPDX-License-Identifier:";
/// The tag goes on line 1, or line 2 after a `#!` or `#cloud-config` line.
const HEAD_LINES: usize = 3;
/// Bytes read from each file to find the tag.
const HEAD_BYTES: u64 = 1024;
const REUSE_FILE: &str = "REUSE.toml";
const REUSE_VERSION: u32 = 1;

/// License identifiers that may appear, and why.
const ALLOWED: &[&str] = &[
    // JuanitOS itself (LICENSE).
    "BSD-3-Clause",
    // freebsd/geom_ventoy, vendored from Ventoy; its C headers keep the
    // deprecated FreeBSD variant of the identifier.
    "BSD-2-Clause",
    "BSD-2-Clause-FreeBSD",
    // freebsd/grub: a GRUB module.
    "GPL-3.0-or-later",
    // tools/dev/cosmic-patches for the GPL COSMIC components.
    "GPL-3.0-only",
    // tools/dev/cosmic-patches for pipewire-rs.
    "MIT",
    // The Open Sans font in jos-gfx.
    "OFL-1.1",
];

#[derive(Deserialize)]
struct Reuse {
    version: u32,
    #[serde(default)]
    annotations: Vec<Annotation>,
}

#[derive(Deserialize)]
struct Annotation {
    path: Paths,
    #[serde(rename = "SPDX-License-Identifier")]
    license: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Paths {
    One(String),
    Many(Vec<String>),
}

impl Paths {
    fn list(&self) -> &[String] {
        match self {
            Paths::One(p) => std::slice::from_ref(p),
            Paths::Many(ps) => ps,
        }
    }
}

/// Fail unless every file names an allowed license.
pub fn check(root: &Path) -> anyhow::Result<()> {
    let Some(files) = repository_files(root) else {
        println!("note: not a git checkout, SPDX license tags not checked");
        return Ok(());
    };
    let annotations = match std::fs::read_to_string(root.join(REUSE_FILE)) {
        Ok(text) => parse_reuse(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).context(format!("reading {REUSE_FILE}")),
    };
    let mut heads = Vec::with_capacity(files.len());
    for file in files {
        match head(&root.join(&file)) {
            Ok(text) => heads.push((file, text)),
            // Deleted in the working tree, not yet staged.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context(format!("reading {file}")),
        }
    }
    let found = problems(&heads, &annotations);
    if found.is_empty() {
        return Ok(());
    }
    for p in &found {
        eprintln!("  {p}");
    }
    bail!(
        "{} SPDX license problem(s): put `{TAG} <id>` in the first {HEAD_LINES} lines \
         of the file, or list it in {REUSE_FILE}",
        found.len()
    )
}

/// Tracked and untracked, not ignored, files; `None` outside a checkout.
fn repository_files(root: &Path) -> Option<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let list = String::from_utf8_lossy(&out.stdout);
    Some(
        list.split('\0')
            .filter(|f| !f.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

fn head(path: &Path) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(HEAD_BYTES)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn parse_reuse(text: &str) -> anyhow::Result<Vec<Annotation>> {
    let reuse: Reuse = toml::from_str(text).with_context(|| format!("parsing {REUSE_FILE}"))?;
    if reuse.version != REUSE_VERSION {
        bail!("{REUSE_FILE}: version must be {REUSE_VERSION}");
    }
    Ok(reuse.annotations)
}

/// Everything wrong with `files` (path, first bytes) and the annotations.
fn problems(files: &[(String, String)], annotations: &[Annotation]) -> Vec<String> {
    let mut out = Vec::new();
    for (path, head) in files {
        match tag(head) {
            Some(expr) => out.extend(license_problems(expr).map(|p| format!("{path}: {p}"))),
            None if annotations
                .iter()
                .any(|a| a.path.list().iter().any(|g| glob(g, path))) => {}
            None => out.push(format!("{path}: no license")),
        }
    }
    for a in annotations {
        out.extend(license_problems(&a.license).map(|p| format!("{REUSE_FILE}: {p}")));
        for g in a.path.list() {
            if !files.iter().any(|(path, _)| glob(g, path)) {
                out.push(format!("{REUSE_FILE}: {g:?} matches no file"));
            }
        }
    }
    out
}

/// The license expression of the first tag in the leading lines, without
/// the closing of the comment around it.
fn tag(head: &str) -> Option<&str> {
    let (_, rest) = head
        .lines()
        .take(HEAD_LINES)
        .find_map(|line| line.split_once(TAG))?;
    let rest = rest.trim();
    let rest = rest.strip_suffix("-->").unwrap_or(rest);
    let rest = rest.strip_suffix("*/").unwrap_or(rest);
    Some(rest.trim())
}

fn license_problems(expr: &str) -> impl Iterator<Item = String> + '_ {
    let spaced = expr.replace(['(', ')'], " ");
    let ids: Vec<String> = spaced
        .split_whitespace()
        .filter(|w| !matches!(*w, "OR" | "AND" | "WITH"))
        .map(str::to_owned)
        .collect();
    let empty = ids
        .is_empty()
        .then(|| "empty license expression".to_owned());
    let unknown = ids
        .into_iter()
        .filter(|id| !ALLOWED.contains(&id.as_str()))
        .map(|id| format!("license {id:?} is not one LICENSE or THIRD_PARTY.md accounts for"));
    empty.into_iter().chain(unknown)
}

/// REUSE globbing: `*` matches within a path component, `**` across them.
fn glob(pattern: &str, path: &str) -> bool {
    if let Some(rest) = pattern.strip_prefix("**") {
        return tails(path).any(|tail| glob(rest, tail));
    }
    if let Some(rest) = pattern.strip_prefix('*') {
        return tails(path).any(|tail| {
            path.strip_suffix(tail)
                .is_some_and(|skipped| !skipped.contains('/'))
                && glob(rest, tail)
        });
    }
    let mut p = pattern.chars();
    let mut s = path.chars();
    match p.next() {
        None => path.is_empty(),
        Some(c) => s.next() == Some(c) && glob(p.as_str(), s.as_str()),
    }
}

/// `path` and each of its suffixes, down to the empty one.
fn tails(path: &str) -> impl Iterator<Item = &str> {
    path.char_indices()
        .map(|(i, _)| i)
        .chain([path.len()])
        .filter_map(|i| path.get(i..))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(p, h)| (p.to_string(), h.to_string()))
            .collect()
    }

    #[test]
    fn globs_follow_reuse() {
        assert!(glob("docs/images/*.png", "docs/images/alice.png"));
        assert!(!glob("docs/images/*.png", "docs/images/old/alice.png"));
        assert!(!glob("docs/images/*.png", "docs/alice.png"));
        assert!(!glob("docs/images/*.png", "docs/images/alice.pngx"));
        assert!(glob("crates/*/Cargo.lock", "crates/bob/Cargo.lock"));
        assert!(!glob("crates/*/Cargo.lock", "Cargo.lock"));
        assert!(glob("vendor/**", "vendor/chuck/deep/x.c"));
        assert!(glob("**/x.c", "a/b/x.c"));
        assert!(glob("LICENSE", "LICENSE"));
        assert!(!glob("LICENSE", "LICENSE.md"));
        assert!(!glob("*", "a/b"));
        assert!(glob("*", ""));
        assert!(glob("é*", "élan"));
    }

    #[test]
    fn the_tag_is_found_in_any_comment_style_near_the_top() {
        assert_eq!(tag("// SPDX-License-Identifier: MIT\n"), Some("MIT"));
        assert_eq!(
            tag("#!/bin/sh\n# SPDX-License-Identifier: BSD-3-Clause\n"),
            Some("BSD-3-Clause")
        );
        assert_eq!(
            tag("<!-- SPDX-License-Identifier: BSD-3-Clause -->\n# Title"),
            Some("BSD-3-Clause")
        );
        assert_eq!(
            tag("/*\n * SPDX-License-Identifier: GPL-3.0-or-later */"),
            Some("GPL-3.0-or-later")
        );
        assert_eq!(tag("a\nb\nc\n// SPDX-License-Identifier: MIT\n"), None);
        assert_eq!(tag("\u{89}PNG\r\n\u{1a}\n"), None);
    }

    #[test]
    fn expressions_only_use_allowed_identifiers() {
        assert_eq!(license_problems("BSD-3-Clause").count(), 0);
        assert_eq!(license_problems("(MIT OR BSD-2-Clause)").count(), 0);
        let bad: Vec<String> = license_problems("MIT AND GPL-2.0-only").collect();
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert!(bad.iter().all(|p| p.contains("GPL-2.0-only")), "{bad:?}");
        let empty: Vec<String> = license_problems(" ").collect();
        assert_eq!(empty, ["empty license expression"]);
    }

    #[test]
    fn files_need_a_tag_or_an_annotation() {
        let annotations = parse_reuse(
            "version = 1\n\
             [[annotations]]\npath = \"art/*.png\"\nSPDX-License-Identifier = \"BSD-3-Clause\"\n\
             [[annotations]]\npath = [\"gone/*\", \"LICENSE\"]\n\
             SPDX-License-Identifier = \"WTFPL\"\n",
        )
        .unwrap();
        let found = problems(
            &files(&[
                ("alice.rs", "// SPDX-License-Identifier: BSD-3-Clause\n"),
                ("art/bob.png", "\u{89}PNG"),
                ("chuck.sh", "#!/bin/sh\necho hi\n"),
                ("mallory.rs", "// SPDX-License-Identifier: Proprietary\n"),
                ("LICENSE", "BSD 3-Clause License\n"),
            ]),
            &annotations,
        );
        assert_eq!(
            found,
            [
                "chuck.sh: no license",
                "mallory.rs: license \"Proprietary\" is not one LICENSE or THIRD_PARTY.md accounts for",
                "REUSE.toml: license \"WTFPL\" is not one LICENSE or THIRD_PARTY.md accounts for",
                "REUSE.toml: \"gone/*\" matches no file",
            ]
        );
    }

    #[test]
    fn reuse_files_are_validated() {
        assert!(parse_reuse("version = 2\n").is_err());
        assert!(parse_reuse("not toml [").is_err());
        assert!(parse_reuse("version = 1\n[[annotations]]\npath = \"x\"\n").is_err());
        assert!(parse_reuse("version = 1\n").unwrap().is_empty());
        let ours = parse_reuse(include_str!("../../REUSE.toml")).unwrap();
        assert!(ours
            .iter()
            .all(|a| license_problems(&a.license).count() == 0));
    }

    #[test]
    fn a_checkout_is_checked_and_anything_else_skipped() {
        let dir = std::env::temp_dir().join(format!("xtask-spdx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No checkout, no file list: skipped with a note.
        let nowhere = dir.join("nowhere");
        assert!(repository_files(&nowhere).is_none());
        check(&nowhere).unwrap();
        std::fs::write(dir.join("alice.rs"), "fn main() {}\n").unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if git(&["init", "-q"]) {
            std::fs::write(
                dir.join(".gitignore"),
                "# SPDX-License-Identifier: MIT\nbob\n",
            )
            .unwrap();
            std::fs::write(dir.join("bob"), "ignored, so not checked\n").unwrap();
            assert!(check(&dir).is_err());
            std::fs::write(
                dir.join("alice.rs"),
                "// SPDX-License-Identifier: BSD-3-Clause\nfn main() {}\n",
            )
            .unwrap();
            check(&dir).unwrap();
            std::fs::write(
                dir.join(REUSE_FILE),
                "version = 1\n[[annotations]]\npath = \"x\"\nSPDX-License-Identifier = \"MIT\"\n",
            )
            .unwrap();
            // An untagged REUSE.toml whose only path matches nothing.
            std::fs::write(
                dir.join(REUSE_FILE),
                "version = 1\n[[annotations]]\npath = \"x\"\nSPDX-License-Identifier = \"MIT\"\n",
            )
            .unwrap();
            let err = check(&dir).unwrap_err().to_string();
            assert!(err.contains("2 SPDX license problem"), "{err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
