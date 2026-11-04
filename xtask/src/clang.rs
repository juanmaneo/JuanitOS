// SPDX-License-Identifier: BSD-3-Clause
//! clang-format and clang-tidy on the C sources JuanitOS maintains.
//!
//! `.clang-format` (LLVM style) and `.clang-tidy` at the repository root
//! cover every `.c` and `.h` file in the tree, except vendored code: a
//! directory whose own `.clang-format` says `DisableFormat: true`
//! (`freebsd/geom_ventoy`) is upstream's and both tools leave it alone.
//! [`fmt`] is the C half of `cargo xtask fmt`, [`tidy`] of `cargo xtask
//! lint`.
//!
//! Both tools have to be the LLVM major version in `.clang-version`:
//! another version formats differently and has other checks, and
//! `cargo xtask ci` must pass or fail the same on every machine.
//!
//! clang-tidy compiles the code, and a FreeBSD kernel module needs the
//! kernel's headers. [`tidy`] fetches `src.txz` of the release named in
//! `release/juanitos.toml` (verified against FreeBSD's MANIFEST, as
//! `jos-release` does), keeps the few header directories a module build
//! includes from under `~/.cache/juanitos/freebsd-<version>/` and runs
//! clang-tidy with the flags `sys/conf/kmod.mk` and `kern.mk` give an
//! amd64 module.
//!
//! `freebsd/grub` is a GRUB module, not a kernel one: its flags are in its
//! own `compile_flags.txt`, which clang-tidy reads by itself, and [`tidy`]
//! adds GRUB's headers (the `[grub]` tarball of the profile, sha256-checked,
//! `include/` kept under `~/.cache/juanitos/grub-<version>/`).
//!
//! `freebsd/jos_vmsvga` and `freebsd/jos_efifb` are DRM drivers built
//! against drm-kmod, on top of the KMS code in `freebsd/jos_kmsfb`:
//! besides the kernel flags they get
//! the LinuxKPI and drm-kmod headers (the `[drm_kmod]` tarball, kept under
//! `~/.cache/juanitos/drm-kmod-<tag>/`), the `-DCONFIG_*` set drm-kmod's
//! `kconfig.mk` gives amd64 and `freebsd/jos_kmsfb` on the include path
//! (as `kmsfb.mk` does).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use juanitos_core::fsops::FsOps;
use juanitos_core::release::{parse_freebsd_manifest, DistManifest};
use serde::Deserialize;

/// Holds the LLVM major version both tools must have.
const VERSION_FILE: &str = ".clang-version";
/// The release profile: FreeBSD version and mirror of the kernel sources.
const PROFILE: &str = "release/juanitos.toml";
/// FreeBSD distribution set with the kernel sources.
const SRC_SET: &str = "src.txz";
/// The GRUB module's sources, compiled with its `compile_flags.txt`.
const GRUB_DIR: &str = "freebsd/grub";
/// The DRM drivers' sources, compiled against drm-kmod.
const DRM_DIRS: &[&str] = &[
    "freebsd/jos_efifb",
    "freebsd/jos_kmsfb",
    "freebsd/jos_vmsvga",
];
/// What the DRM drivers share, on their include path.
const KMSFB_DIR: &str = "freebsd/jos_kmsfb";
/// Directories of `usr/src/sys` a kernel module build includes from: the
/// kernel's own headers, the ck concurrency kit, the `machine`/`x86`
/// directories `bsd.kmod.mk` links into the build, and what LinuxKPI's
/// headers pull in.
const KERNEL_HEADER_DIRS: &[&str] = &[
    "sys",
    "amd64/include",
    "x86/include",
    "contrib/ck/include",
    "compat/linuxkpi/common/include",
    "compat/linuxkpi/dummy/include",
    "vm",
    "net",
    "net80211",
    "netinet",
    "netinet6",
    "fs/pseudofs",
    "bsm",
    "dev/pci",
];
/// kobj interfaces whose headers (`X_if.h`) modules include, generated as
/// `bsd.kmod.mk` does with `makeobjops.awk -h`.
const KOBJ_INTERFACES: &[&str] = &[
    "kern/device_if.m",
    "kern/bus_if.m",
    "dev/pci/pci_if.m",
    "dev/pci/pci_iov_if.m",
    "dev/iicbus/iicbus_if.m",
];
const MAKEOBJOPS: &str = "tools/makeobjops.awk";
/// `vnode_if.h` and its two companions come from this pair.
const VNODE_IF: &str = "kern/vnode_if.src";
const VNODE_IF_AWK: &str = "tools/vnode_if.awk";
/// What [`kernel_headers`] extracts from drm-kmod's tarball.
const DRM_KMOD_MEMBERS: &[&str] = &[
    "include",
    "linuxkpi",
    "drivers/gpu/drm/drm_os_freebsd.h",
    "kconfig.mk",
    "linuxkpi_version.mk",
];

/// clang-format every C file we maintain: check (the default) or apply.
pub fn fmt(root: &Path, fix: bool) -> anyhow::Result<()> {
    let files = sources(root)?;
    if files.is_empty() {
        return Ok(());
    }
    check_version(root, "clang-format")?;
    let mut args: Vec<String> = if fix {
        vec!["-i".into()]
    } else {
        vec!["--dry-run".into(), "--Werror".into()]
    };
    args.extend(files.iter().map(|f| f.display().to_string()));
    run(root, "clang-format", &args)
}

/// clang-tidy every C file we maintain: the GRUB module with its own
/// flags and GRUB's headers, the DRM drivers against drm-kmod, the rest as
/// plain FreeBSD kernel modules.
pub fn tidy(root: &Path) -> anyhow::Result<()> {
    let files: Vec<PathBuf> = sources(root)?
        .into_iter()
        .filter(|f| f.extension().is_some_and(|e| e == "c"))
        .collect();
    if files.is_empty() {
        return Ok(());
    }
    check_version(root, "clang-tidy")?;
    let profile = Profile::load(root)?;
    let groups = Groups::of(files);
    if !groups.kmod.is_empty() || !groups.drm.is_empty() {
        if profile.arch != "amd64" {
            bail!(
                "clang-tidy runs with amd64 kernel flags; {PROFILE} says {}",
                profile.arch
            );
        }
        let headers = kernel_headers(&profile)?;
        let flags = kmod_flags(&profile, &headers);
        if !groups.kmod.is_empty() {
            let mut args = tidy_args(&groups.kmod);
            args.push("--".into());
            args.extend(flags.iter().cloned());
            run(root, "clang-tidy", &args)?;
        }
        if !groups.drm.is_empty() {
            let drm = drm_headers(&profile)?;
            for file in &groups.drm {
                let mut args = tidy_args(std::slice::from_ref(file));
                args.push("--".into());
                args.extend(flags.iter().cloned());
                args.push(format!("-I{KMSFB_DIR}"));
                args.extend(drm_flags(&drm, &headers, &module_name(file))?);
                run(root, "clang-tidy", &args)?;
            }
        }
    }
    if !groups.grub.is_empty() {
        let include = grub_headers(&profile)?;
        let mut args = tidy_args(&groups.grub);
        args.push(grub_flag(&include));
        run(root, "clang-tidy", &args)?;
    }
    Ok(())
}

/// The `.c` files by how clang-tidy compiles them.
#[derive(Debug, Default, PartialEq)]
struct Groups {
    grub: Vec<PathBuf>,
    drm: Vec<PathBuf>,
    kmod: Vec<PathBuf>,
}

impl Groups {
    fn of(files: Vec<PathBuf>) -> Self {
        let mut g = Groups::default();
        for f in files {
            if f.starts_with(GRUB_DIR) {
                g.grub.push(f);
            } else if DRM_DIRS.iter().any(|d| f.starts_with(d)) {
                g.drm.push(f);
            } else {
                g.kmod.push(f);
            }
        }
        g
    }
}

/// A module is named after its directory (`KMOD` in its Makefile).
fn module_name(file: &Path) -> String {
    file.parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `--quiet` (no statistics about the system headers' suppressed
/// findings), the `CustomChecks` of `.clang-tidy` enabled, and the files.
fn tidy_args(files: &[PathBuf]) -> Vec<String> {
    let mut args: Vec<String> = vec!["--quiet".into(), "--experimental-custom-checks".into()];
    args.extend(files.iter().map(|f| f.display().to_string()));
    args
}

/// GRUB's headers as system headers, on top of `compile_flags.txt`.
fn grub_flag(include: &Path) -> String {
    format!("--extra-arg=-isystem{}", include.display())
}

/// Our `.c` and `.h` files, relative to `root`, sorted. Skips `target`
/// directories, hidden ones and vendored code (see [`vendored`]).
pub fn sources(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if vendored(dir) {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            if !name.starts_with('.') && name != "target" {
                walk(root, &path, out)?;
            }
        } else if path.extension().is_some_and(|e| e == "c" || e == "h") {
            out.push(path.strip_prefix(root)?.to_path_buf());
        }
    }
    Ok(())
}

/// A directory with a `.clang-format` of its own that turns formatting
/// off holds third-party code (THIRD_PARTY.md).
fn vendored(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(".clang-format"))
        .map(|text| text.lines().any(|l| l.trim() == "DisableFormat: true"))
        .unwrap_or(false)
}

/// Fail unless `tool --version` reports the major version in
/// `.clang-version`.
fn check_version(root: &Path, tool: &str) -> anyhow::Result<()> {
    let want = std::fs::read_to_string(root.join(VERSION_FILE))
        .with_context(|| format!("reading {VERSION_FILE}"))?;
    let want = want.trim();
    let reported = match Command::new(tool).arg("--version").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => bail!(
            "{tool} is missing: install LLVM {want}'s {tool} \
             (what CI uses, see .github/workflows/ci.yml)"
        ),
    };
    if !version_matches(&reported, want) {
        bail!(
            "{tool} must be LLVM {want} ({VERSION_FILE}); found:\n  {}",
            reported.trim()
        );
    }
    Ok(())
}

/// Does a `--version` banner name the wanted major version?
fn version_matches(reported: &str, want: &str) -> bool {
    reported.contains(&format!("version {want}."))
}

/// The fields of `release/juanitos.toml` the header fetches need.
#[derive(Debug, Deserialize)]
struct Profile {
    freebsd_version: String,
    arch: String,
    mirror: String,
    /// The release whose headers build `freebsd/grub`.
    grub: Tarball,
    /// The drm-kmod tag `freebsd/jos_vmsvga` builds against.
    drm_kmod: Tarball,
}

/// A pinned source tarball of the profile.
#[derive(Debug, Deserialize)]
struct Tarball {
    version: String,
    url: String,
    sha256: String,
}

impl Profile {
    fn load(root: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(root.join(PROFILE))
            .with_context(|| format!("reading {PROFILE}"))?;
        toml::from_str(&text).with_context(|| format!("parsing {PROFILE}"))
    }

    /// `https://mirror/arch/version/file`, as `jos-release` builds it.
    fn dist_url(&self, file: &str) -> String {
        format!(
            "{}/{}/{}/{file}",
            self.mirror.trim_end_matches('/'),
            self.arch,
            self.freebsd_version
        )
    }

    /// The clang target of the release, e.g. `x86_64-unknown-freebsd15.1`.
    fn target(&self) -> String {
        let version = self.freebsd_version.split('-').next().unwrap_or_default();
        format!("x86_64-unknown-freebsd{version}")
    }
}

/// `$XDG_CACHE_HOME/juanitos`, else `~/.cache/juanitos`: outside
/// `target/`, so `cargo clean` does not cost a 250 MB download.
fn cache_dir(xdg: Option<OsString>, home: Option<OsString>) -> anyhow::Result<PathBuf> {
    if let Some(dir) = xdg.filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("juanitos"));
    }
    let home = home.context("neither XDG_CACHE_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".cache").join("juanitos"))
}

/// [`cache_dir`] of this process's environment.
fn cache() -> anyhow::Result<PathBuf> {
    cache_dir(
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
    )
}

/// The members of `usr/src/sys` [`kernel_headers`] extracts: the header
/// directories, the interface sources they do not already hold, and the
/// awk scripts. None lies inside another (tar fails on that).
fn kernel_members() -> Vec<String> {
    let inside_a_dir = |f: &str| {
        KERNEL_HEADER_DIRS
            .iter()
            .any(|d| f.starts_with(&format!("{d}/")))
    };
    let mut members: Vec<String> = KERNEL_HEADER_DIRS.iter().map(|d| d.to_string()).collect();
    members.extend(
        KOBJ_INTERFACES
            .iter()
            .chain(&[VNODE_IF, MAKEOBJOPS, VNODE_IF_AWK])
            .filter(|f| !inside_a_dir(f))
            .map(|f| f.to_string()),
    );
    members
}

/// The release's kernel headers, fetched on first use:
/// `<cache>/freebsd-<version>/usr/src/sys/` with [`kernel_members`], and
/// `kmod/` with what `bsd.kmod.mk` puts in a module's build directory
/// (an empty `opt_global.h`, `machine` and `x86` holding amd64's headers,
/// the generated interface headers). `HEADERS` records what was
/// extracted; a different list fetches again.
fn kernel_headers(profile: &Profile) -> anyhow::Result<PathBuf> {
    let dir = cache()?.join(format!("freebsd-{}", profile.freebsd_version));
    let stamp = dir.join("HEADERS");
    let members = kernel_members();
    let want = members.join("\n");
    if std::fs::read_to_string(&stamp).is_ok_and(|have| have == want) {
        return Ok(dir);
    }
    println!(
        "Fetching the FreeBSD {} kernel headers for clang-tidy into {} (first use only)",
        profile.freebsd_version,
        dir.display()
    );
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let manifest = dir.join("MANIFEST");
    download(&profile.dist_url("MANIFEST"), &manifest)?;
    let src = dir.join(SRC_SET);
    download(&profile.dist_url(SRC_SET), &src)?;
    let sha256 = parse_freebsd_manifest(&std::fs::read_to_string(&manifest)?)
        .into_iter()
        .find(|e| e.file == SRC_SET)
        .with_context(|| format!("{SRC_SET} is not listed in FreeBSD's MANIFEST"))?
        .sha256;
    DistManifest::verify_sha256(&src, &sha256)?;
    let mut args = vec![
        "-xf".to_string(),
        src.display().to_string(),
        "-C".to_string(),
        dir.display().to_string(),
    ];
    args.extend(members.iter().map(|m| format!("usr/src/sys/{m}")));
    run(&dir, "tar", &args)?;
    std::fs::remove_file(&src)?;
    let sys = dir.join("usr/src/sys");
    let kmod = dir.join("kmod");
    std::fs::create_dir_all(&kmod)?;
    std::fs::write(kmod.join("opt_global.h"), "")?;
    std::fs::rename(sys.join("amd64/include"), kmod.join("machine"))?;
    std::fs::rename(sys.join("x86/include"), kmod.join("x86"))?;
    for (program, args) in interface_commands(&sys) {
        run(&kmod, program, &args)?;
    }
    std::fs::write(&stamp, want)?;
    Ok(dir)
}

/// The awk runs that write the interface headers into the current
/// directory, as `sys/conf/kmod.mk` makes them.
fn interface_commands(sys: &Path) -> Vec<(&'static str, Vec<String>)> {
    let path = |rel: &str| sys.join(rel).display().to_string();
    let mut commands: Vec<(&str, Vec<String>)> = KOBJ_INTERFACES
        .iter()
        .map(|m| {
            let args = vec!["-f".into(), path(MAKEOBJOPS), path(m), "-h".into()];
            ("awk", args)
        })
        .collect();
    for opt in ["-h", "-p", "-q"] {
        let args = vec!["-f".into(), path(VNODE_IF_AWK), path(VNODE_IF), opt.into()];
        commands.push(("awk", args));
    }
    commands
}

/// `<cache>/<name>/` with `members` of a pinned tarball whose top
/// directory is `name`, fetched and sha256-checked on first use, then
/// `finish`ed. `HEADERS` records the tarball's checksum; another tarball
/// fetches again.
fn cached_tarball(
    tarball: &Tarball,
    name: &str,
    members: &[&str],
    finish: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<PathBuf> {
    let cache = cache()?;
    let dir = cache.join(name);
    let stamp = dir.join("HEADERS");
    if std::fs::read_to_string(&stamp).is_ok_and(|have| have == tarball.sha256) {
        return Ok(dir);
    }
    println!(
        "Fetching the {name} headers for clang-tidy into {} (first use only)",
        dir.display()
    );
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("source.tar");
    download(&tarball.url, &file)?;
    DistManifest::verify_sha256(&file, &tarball.sha256)?;
    let mut args = vec![
        "-xf".to_string(),
        file.display().to_string(),
        "-C".to_string(),
        cache.display().to_string(),
    ];
    args.extend(members.iter().map(|m| format!("{name}/{m}")));
    run(&dir, "tar", &args)?;
    std::fs::remove_file(&file)?;
    finish(&dir)?;
    std::fs::write(&stamp, &tarball.sha256)?;
    Ok(dir)
}

/// GRUB's `include/` directory, under `<cache>/grub-<version>/`, with
/// `grub/cpu` and `grub/machine` copied from the x86_64 EFI headers as
/// GRUB's `configure` would link them (and as `freebsd/grub/Makefile`
/// does).
fn grub_headers(profile: &Profile) -> anyhow::Result<PathBuf> {
    let grub = &profile.grub;
    let name = format!("grub-{}", grub.version);
    let dir = cached_tarball(grub, &name, &["include"], |dir| {
        let include = dir.join("include");
        let fs = FsOps::new(false);
        let x86_64 = include.join("grub/x86_64");
        fs.copy_tree(&x86_64, &include.join("grub/cpu"), &[])?;
        fs.copy_tree(&x86_64.join("efi"), &include.join("grub/machine"), &[])?;
        Ok(())
    })?;
    Ok(dir.join("include"))
}

/// drm-kmod's headers and build settings, under
/// `<cache>/drm-kmod-<tag>/` (the tarball's top directory).
fn drm_headers(profile: &Profile) -> anyhow::Result<PathBuf> {
    let drm = &profile.drm_kmod;
    let name = format!("drm-kmod-{}", drm.version);
    cached_tarball(drm, &name, DRM_KMOD_MEMBERS, |_| Ok(()))
}

/// What drm-kmod's module Makefiles add to the kernel flags for
/// `module` (as `freebsd/jos_kmsfb/kmsfb.mk` does): the LinuxKPI and drm
/// headers, as system headers, the module's names and drm-kmod's
/// `CONFIG_*` set. `compiler_flags.mk`'s `-include compiler_types.h` is
/// left out: clang-tidy would take that kernel header for our code.
fn drm_flags(drm: &Path, headers: &Path, module: &str) -> anyhow::Result<Vec<String>> {
    let read = |file: &str| {
        std::fs::read_to_string(drm.join(file))
            .with_context(|| format!("reading drm-kmod's {file}"))
    };
    let linuxkpi = read("linuxkpi_version.mk")?;
    let version = make_var(&linuxkpi, "LINUXKPI_VERSION_NUMBER")
        .context("drm-kmod's linuxkpi_version.mk has no LINUXKPI_VERSION_NUMBER")?;
    let sys = headers.join("usr/src/sys");
    let mut flags = Vec::new();
    for dir in [
        drm.join("linuxkpi/gplv2/include"),
        drm.join("linuxkpi/bsd/include"),
        sys.join("compat/linuxkpi/common/include"),
        sys.join("compat/linuxkpi/dummy/include"),
        drm.join("include"),
        drm.join("include/drm"),
        drm.join("include/uapi"),
        drm.join("drivers/gpu"),
    ] {
        flags.push("-isystem".into());
        flags.push(dir.display().to_string());
    }
    flags.push(format!("-DKBUILD_MODNAME=\"{module}\""));
    flags.push(format!("-DLINUXKPI_PARAM_PREFIX={module}_"));
    flags.push(format!("-DDRM_SYSCTL_PARAM_PREFIX=_{module}"));
    flags.push(format!("-DLINUXKPI_VERSION={version}"));
    flags.extend(kconfig_defines(&read("kconfig.mk")?)?);
    Ok(flags)
}

/// The value of a plain `NAME=value` line of a make file.
fn make_var<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let (lhs, rhs) = l.split_once('=')?;
        (lhs.trim() == name).then(|| rhs.trim())
    })
}

/// `-DCONFIG_*` for every `KCONFIG` entry of drm-kmod's `kconfig.mk` that
/// applies to amd64 (`${KCONFIG:C/(.*)/-DCONFIG_\1/}` in its Makefiles).
/// Conditions are read the simple way the file needs: a test true for
/// `amd64` or an `empty(...)` of a variable nobody sets holds, any other
/// test is another architecture's.
fn kconfig_defines(text: &str) -> anyhow::Result<Vec<String>> {
    let mut active: Vec<bool> = Vec::new();
    let mut defines = Vec::new();
    for line in logical_lines(text) {
        let line = line.trim();
        let on = active.iter().all(|a| *a);
        if let Some(cond) = line.strip_prefix(".if ") {
            active.push(cond.split("||").any(|t| {
                let t = t.trim();
                t.starts_with("empty(") || t.ends_with("== \"amd64\"")
            }));
        } else if line == ".else" {
            let last = active.pop().context("kconfig.mk: .else without .if")?;
            active.push(!last);
        } else if line == ".endif" {
            active.pop().context("kconfig.mk: .endif without .if")?;
        } else if line.starts_with('.') {
            bail!("kconfig.mk: unexpected directive {line:?}");
        } else if let Some(rest) = line.strip_prefix("KCONFIG") {
            let rest = rest.trim_start();
            let (append, values) = match rest.strip_prefix("+=") {
                Some(v) => (true, v),
                None => match rest.strip_prefix('=') {
                    Some(v) => (false, v),
                    None => continue,
                },
            };
            if on {
                if !append {
                    defines.clear();
                }
                defines.extend(
                    values
                        .split_whitespace()
                        .map(|v| format!("-DCONFIG_{}", shell_unquote(v))),
                );
            }
        }
    }
    if !active.is_empty() {
        bail!("kconfig.mk: .if without .endif");
    }
    Ok(defines)
}

/// Lines with their `\` continuations joined; comment lines dropped.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if current.is_empty() && line.trim_start().starts_with('#') {
            continue;
        }
        match line.strip_suffix('\\') {
            Some(head) => {
                current.push_str(head);
                current.push(' ');
            }
            None => {
                current.push_str(line);
                out.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// A word as the shell that runs make's compiler command passes it on:
/// `X="0x1"` is `X=0x1`, `X="\"user\""` and `X='"*"'` keep their inner
/// double quotes.
fn shell_unquote(word: &str) -> String {
    let mut out = String::new();
    let mut chars = word.chars();
    let (mut single, mut double) = (false, false);
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '\\' if !single => out.extend(chars.next()),
            c => out.push(c),
        }
    }
    out
}

/// `fetch(1)` where there is one (FreeBSD), else `curl`.
fn download(url: &str, dst: &Path) -> anyhow::Result<()> {
    let dst_s = dst.display().to_string();
    let fetched = Command::new("fetch")
        .args(["-o", &dst_s, url])
        .status()
        .is_ok_and(|st| st.success());
    if fetched {
        return Ok(());
    }
    run(
        Path::new("."),
        "curl",
        &["-fL".to_string(), "-o".to_string(), dst_s, url.to_string()],
    )
    .with_context(|| format!("downloading {url}"))
}

/// What `make` hands the compiler for an amd64 kernel module
/// (`sys/conf/kmod.mk` and `kern.mk`, with the `CSTD=gnu23` of our
/// module Makefiles), minus optimisation and debug flags. The kernel's
/// headers are system headers to clang-tidy: findings in them are
/// FreeBSD's business, not ours.
fn kmod_flags(profile: &Profile, headers: &Path) -> Vec<String> {
    let sys = headers.join("usr/src/sys");
    let kmod = headers.join("kmod");
    let mut flags: Vec<String> = [
        "-std=gnu23",
        "-D_KERNEL",
        "-DKLD_MODULE",
        "-nostdinc",
        "-ffreestanding",
        "-fwrapv",
        "-fno-common",
        "-mcmodel=kernel",
        "-mno-red-zone",
        "-mno-mmx",
        "-mno-sse",
        "-msoft-float",
        "-mno-aes",
        "-mno-avx",
        "-fno-asynchronous-unwind-tables",
        "-fno-omit-frame-pointer",
        "-mno-omit-leaf-frame-pointer",
        "-Wall",
        "-Wstrict-prototypes",
        "-Wmissing-prototypes",
        "-Wpointer-arith",
        "-Wcast-qual",
        "-Wundef",
        "-Wno-pointer-sign",
        "-Wmissing-include-dirs",
        "-Wno-unknown-pragmas",
        "-Wswitch",
    ]
    .map(String::from)
    .to_vec();
    flags.push(format!("--target={}", profile.target()));
    flags.push("-include".into());
    flags.push(kmod.join("opt_global.h").display().to_string());
    for dir in [kmod, sys.join("contrib/ck/include"), sys] {
        flags.push("-isystem".into());
        flags.push(dir.display().to_string());
    }
    flags
}

/// Run `program args` in `dir`, echoing the command line as the cargo
/// steps do.
fn run(dir: &Path, program: &str, args: &[String]) -> anyhow::Result<()> {
    println!("\n$ {program} {}", args.join(" "));
    let st = Command::new(program)
        .current_dir(dir)
        .args(args)
        .status()
        .with_context(|| format!("running {program}"))?;
    if !st.success() {
        bail!("{program} failed ({st})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throw-away directory tree, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("xtask-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn put(&self, rel: &str, content: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn sources_skip_vendored_hidden_and_target_directories() {
        let s = Scratch::new("sources");
        s.put("mod/alice.c", "");
        s.put("mod/alice.h", "");
        s.put("mod/notes.md", "");
        s.put(
            "vendor/.clang-format",
            "# upstream code\nDisableFormat: true\n",
        );
        s.put("vendor/bob.c", "");
        s.put("vendor/deeper/bob.h", "");
        s.put("styled/.clang-format", "BasedOnStyle: LLVM\n");
        s.put("styled/chuck.c", "");
        s.put(".hidden/chuck.c", "");
        s.put("target/work/chuck.c", "");
        let found: Vec<String> = sources(&s.0)
            .unwrap()
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(found, ["mod/alice.c", "mod/alice.h", "styled/chuck.c"]);
    }

    #[test]
    fn the_repository_has_our_modules_and_skips_the_vendored_one() {
        let found = sources(&repo()).unwrap();
        assert!(found.contains(&PathBuf::from("freebsd/jos_bootwatch/jos_bootwatch.c")));
        assert!(found.contains(&PathBuf::from("freebsd/grub/jos_efi.c")));
        assert!(found
            .iter()
            .all(|p| !p.to_string_lossy().contains("geom_ventoy")));
        assert!(vendored(&repo().join("freebsd/geom_ventoy")));
        assert!(!vendored(&repo()));
        // The GRUB module carries its own flags for clang-tidy.
        assert!(repo().join(GRUB_DIR).join("compile_flags.txt").is_file());
    }

    #[test]
    fn files_are_linted_by_kind_with_their_own_flags_and_headers() {
        let files = [
            "freebsd/grub/jos_efi.c",
            "freebsd/jos_bootwatch/jos_bootwatch.c",
            "freebsd/grubby/alice.c",
            "freebsd/jos_vmsvga/jos_vmsvga.c",
            "freebsd/jos_kmsfb/jos_kmsfb.c",
            "freebsd/jos_efifb/jos_efifb.c",
        ]
        .map(PathBuf::from);
        let g = Groups::of(files.to_vec());
        assert_eq!(g.grub, [files[0].clone()]);
        assert_eq!(g.kmod, [files[1].clone(), files[2].clone()]);
        assert_eq!(g.drm, files[3..]);
        assert_eq!(module_name(&files[3]), "jos_vmsvga");
        assert!(repo().join(KMSFB_DIR).join("jos_kmsfb.h").is_file());
        assert_eq!(module_name(Path::new("alice.c")), "");
        assert_eq!(
            tidy_args(&g.grub),
            [
                "--quiet".to_string(),
                "--experimental-custom-checks".into(),
                "freebsd/grub/jos_efi.c".into()
            ]
        );
        assert_eq!(
            grub_flag(Path::new("/c/grub-2.04/include")),
            "--extra-arg=-isystem/c/grub-2.04/include"
        );
    }

    #[test]
    fn version_banners_are_matched_on_the_major_version() {
        assert!(version_matches("clang-format version 23.1.1\n", "23"));
        assert!(version_matches(
            "LLVM (http://llvm.org/):\n  LLVM version 23.1.1\n",
            "23"
        ));
        assert!(version_matches(
            "Ubuntu clang-format version 23.1.1 (++20260901)\n",
            "23"
        ));
        assert!(!version_matches("clang-format version 18.1.3\n", "23"));
        assert!(!version_matches("clang-format version 2.3\n", "23"));
        let want = std::fs::read_to_string(repo().join(VERSION_FILE)).unwrap();
        assert!(want.trim().parse::<u32>().is_ok());
    }

    #[test]
    fn profile_names_the_release_sources_and_target() {
        let p = Profile::load(&repo()).unwrap();
        assert_eq!(p.arch, "amd64");
        assert_eq!(
            p.dist_url("MANIFEST"),
            format!(
                "https://download.freebsd.org/releases/amd64/{}/MANIFEST",
                p.freebsd_version
            )
        );
        assert_eq!(p.grub.version, "2.04");
        assert!(p.grub.url.ends_with(".tar.xz"), "{}", p.grub.url);
        assert_eq!(p.grub.sha256.len(), 64);
        // GitHub names the tarball's top directory drm-kmod-<tag>.
        let drm = &p.drm_kmod;
        assert!(drm.url.ends_with(&format!("/{}.tar.gz", drm.version)));
        assert_eq!(drm.sha256.len(), 64);
        let p = profile("https://m/");
        assert_eq!(
            p.dist_url("src.txz"),
            "https://m/amd64/15.1-RELEASE/src.txz"
        );
        assert_eq!(p.target(), "x86_64-unknown-freebsd15.1");
    }

    fn profile(mirror: &str) -> Profile {
        Profile {
            freebsd_version: "15.1-RELEASE".into(),
            arch: "amd64".into(),
            mirror: mirror.into(),
            grub: Tarball {
                version: "2.04".into(),
                url: "https://g/grub-2.04.tar.xz".into(),
                sha256: "0".repeat(64),
            },
            drm_kmod: Tarball {
                version: "drm_v6.12.110_1".into(),
                url: "https://d/drm_v6.12.110_1.tar.gz".into(),
                sha256: "1".repeat(64),
            },
        }
    }

    #[test]
    fn kernel_members_hold_every_interface_once() {
        let members = kernel_members();
        assert!(members.contains(&"dev/pci".to_string()));
        assert!(members.contains(&"compat/linuxkpi/common/include".to_string()));
        // pci_if.m comes with dev/pci; naming it again makes tar fail.
        assert!(!members.contains(&"dev/pci/pci_if.m".to_string()));
        for m in [
            "kern/device_if.m",
            "dev/iicbus/iicbus_if.m",
            VNODE_IF,
            MAKEOBJOPS,
        ] {
            assert!(members.contains(&m.to_string()), "{m}");
        }
    }

    #[test]
    fn interface_headers_are_generated_as_kmod_mk_does() {
        let commands = interface_commands(Path::new("/s"));
        assert_eq!(commands.len(), KOBJ_INTERFACES.len() + 3);
        assert_eq!(
            commands[0],
            (
                "awk",
                ["-f", "/s/tools/makeobjops.awk", "/s/kern/device_if.m", "-h"]
                    .map(String::from)
                    .to_vec()
            )
        );
        let last = &commands[commands.len() - 1].1;
        assert_eq!(
            last,
            &["-f", "/s/tools/vnode_if.awk", "/s/kern/vnode_if.src", "-q"].map(String::from)
        );
    }

    const KCONFIG: &str = "\
# CONFIG_DRM*
KCONFIG=\tDRM_A \\
\t\tDRM_COLOR=\"0xffffff\" \\
\t\tDRM_SCREEN=\"\\\"user\\\"\" \\
\t\tDRM_PROBE='\"*\"'

.if empty(NO_FBDEV)
KCONFIG+=\tFBDEV
.endif

.if ${MACHINE_CPUARCH} == \"aarch64\"
KCONFIG+=\tARM64
.endif

.if ${MACHINE_CPUARCH} == \"i386\" || ${MACHINE_CPUARCH} == \"amd64\"
KCONFIG+=\tX86
.if ${MACHINE_CPUARCH} == \"i386\"
KCONFIG+=\tAGP
.else
KCONFIG+=\tX86_64
.endif
.endif

.if ${MACHINE_ARCH:Mpowerpc64*} != \"\"
KCONFIG+=\tPPC64
\t\t
# comment
.endif
";

    #[test]
    fn kconfig_keeps_amd64_entries_and_unquotes_as_the_shell_does() {
        assert_eq!(
            kconfig_defines(KCONFIG).unwrap(),
            [
                "-DCONFIG_DRM_A",
                "-DCONFIG_DRM_COLOR=0xffffff",
                "-DCONFIG_DRM_SCREEN=\"user\"",
                "-DCONFIG_DRM_PROBE=\"*\"",
                "-DCONFIG_FBDEV",
                "-DCONFIG_X86",
                "-DCONFIG_X86_64",
            ]
        );
        // A plain assignment starts over; other variables are not KCONFIG.
        let text = "KCONFIG= A\nKCONFIGS= B\nKCONFIG= C \\\n";
        assert_eq!(kconfig_defines(text).unwrap(), ["-DCONFIG_C"]);
    }

    #[test]
    fn kconfig_structure_errors_are_named() {
        for (text, want) in [
            (".if empty(X)\n", ".if without .endif"),
            (".endif\n", ".endif without .if"),
            (".else\n", ".else without .if"),
            (".elif empty(X)\n.endif\n", "unexpected directive"),
        ] {
            let err = kconfig_defines(text).unwrap_err().to_string();
            assert!(err.contains(want), "{err}");
        }
    }

    #[test]
    fn drm_flags_follow_the_module_makefile() {
        let s = Scratch::new("drm");
        let err = drm_flags(&s.0, Path::new("/c/fbsd"), "alice").unwrap_err();
        assert!(err.to_string().contains("linuxkpi_version.mk"), "{err}");
        s.put("linuxkpi_version.mk", "# none\n");
        let err = drm_flags(&s.0, Path::new("/c/fbsd"), "alice").unwrap_err();
        assert!(err.to_string().contains("LINUXKPI_VERSION_NUMBER"), "{err}");
        s.put(
            "linuxkpi_version.mk",
            "LINUXKPI_VERSION_NUMBER=\t61200\n\nCFLAGS+= -DX=${Y}\n",
        );
        let err = drm_flags(&s.0, Path::new("/c/fbsd"), "alice").unwrap_err();
        assert!(err.to_string().contains("kconfig.mk"), "{err}");
        s.put("kconfig.mk", "KCONFIG= PCI\n");
        let flags = drm_flags(&s.0, Path::new("/c/fbsd"), "alice").unwrap();
        let text = flags.join(" ");
        let drm = s.0.display();
        assert!(text.starts_with(&format!("-isystem {drm}/linuxkpi/gplv2/include ")));
        assert!(text.contains("-isystem /c/fbsd/usr/src/sys/compat/linuxkpi/common/include "));
        assert!(text.contains(&format!("-isystem {drm}/drivers/gpu ")));
        assert!(text.ends_with(
            "-DKBUILD_MODNAME=\"alice\" -DLINUXKPI_PARAM_PREFIX=alice_ \
             -DDRM_SYSCTL_PARAM_PREFIX=_alice -DLINUXKPI_VERSION=61200 -DCONFIG_PCI"
        ));
        let s = Scratch::new("drm-kconfig");
        s.put("linuxkpi_version.mk", "LINUXKPI_VERSION_NUMBER=1\n");
        s.put("kconfig.mk", ".endif\n");
        assert!(drm_flags(&s.0, Path::new("/c"), "bob").is_err());
    }

    #[test]
    fn cache_dir_prefers_xdg_then_home() {
        let xdg = Some(OsString::from("/c"));
        let home = Some(OsString::from("/home/alice"));
        assert_eq!(
            cache_dir(xdg, home.clone()).unwrap(),
            PathBuf::from("/c/juanitos")
        );
        assert_eq!(
            cache_dir(Some(OsString::new()), home.clone()).unwrap(),
            PathBuf::from("/home/alice/.cache/juanitos")
        );
        assert_eq!(
            cache_dir(None, home).unwrap(),
            PathBuf::from("/home/alice/.cache/juanitos")
        );
        assert!(cache_dir(None, None).is_err());
    }

    #[test]
    fn kmod_flags_compile_as_the_kernel_does_against_the_cached_headers() {
        let p = profile("https://m");
        let flags = kmod_flags(&p, Path::new("/c/freebsd-15.1-RELEASE"));
        let text = flags.join(" ");
        assert!(text.starts_with("-std=gnu23 -D_KERNEL -DKLD_MODULE -nostdinc "));
        assert!(text.contains("--target=x86_64-unknown-freebsd15.1"));
        assert!(text.contains("-include /c/freebsd-15.1-RELEASE/kmod/opt_global.h"));
        assert!(text.contains("-isystem /c/freebsd-15.1-RELEASE/kmod "));
        assert!(text.contains("-isystem /c/freebsd-15.1-RELEASE/usr/src/sys/contrib/ck/include"));
        assert!(text.ends_with("-isystem /c/freebsd-15.1-RELEASE/usr/src/sys"));
        assert!(flags.iter().all(|f| !f.contains(' ')), "one argument each");
    }

    #[test]
    fn missing_tools_are_named() {
        let s = Scratch::new("tools");
        s.put(VERSION_FILE, "23\n");
        let err = check_version(&s.0, "clang-no-such-tool")
            .unwrap_err()
            .to_string();
        assert!(err.contains("clang-no-such-tool is missing"), "{err}");
        assert!(err.contains("LLVM 23"), "{err}");
        let err = run(&s.0, "no-such-program", &[]).unwrap_err().to_string();
        assert!(err.contains("running no-such-program"), "{err}");
    }
}
