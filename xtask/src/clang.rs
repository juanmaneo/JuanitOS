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
/// Directories of `usr/src/sys` an amd64 kernel module build includes
/// from (the kernel's own headers, the ck concurrency kit and the
/// `machine`/`x86` directories `bsd.kmod.mk` links into the build).
const KERNEL_HEADER_DIRS: &[&str] = &["sys", "amd64/include", "x86/include", "contrib/ck/include"];

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
/// flags and GRUB's headers, the rest as FreeBSD kernel modules.
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
    let (grub, kmod): (Vec<PathBuf>, Vec<PathBuf>) =
        files.into_iter().partition(|f| f.starts_with(GRUB_DIR));
    if !kmod.is_empty() {
        if profile.arch != "amd64" {
            bail!(
                "clang-tidy runs with amd64 kernel flags; {PROFILE} says {}",
                profile.arch
            );
        }
        let headers = kernel_headers(&profile)?;
        let mut args = tidy_args(&kmod);
        args.push("--".into());
        args.extend(kmod_flags(&profile, &headers));
        run(root, "clang-tidy", &args)?;
    }
    if !grub.is_empty() {
        let include = grub_headers(&profile)?;
        let mut args = tidy_args(&grub);
        args.push(grub_flag(&include));
        run(root, "clang-tidy", &args)?;
    }
    Ok(())
}

/// `--quiet` (no statistics about the system headers' suppressed
/// findings) and the files.
fn tidy_args(files: &[PathBuf]) -> Vec<String> {
    let mut args: Vec<String> = vec!["--quiet".into()];
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
    grub: Grub,
}

/// `[grub]`: the release whose headers build `freebsd/grub`.
#[derive(Debug, Deserialize)]
struct Grub {
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

/// The release's kernel headers, fetched on first use:
/// `<cache>/freebsd-<version>/usr/src/sys/{sys,contrib/ck/include}` and
/// `kmod/` with what `bsd.kmod.mk` puts in a module's build directory
/// (an empty `opt_global.h`, `machine` and `x86` holding amd64's
/// headers). `HEADERS` records what was extracted; a different list
/// fetches again.
fn kernel_headers(profile: &Profile) -> anyhow::Result<PathBuf> {
    let cache = cache_dir(
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
    )?;
    let dir = cache.join(format!("freebsd-{}", profile.freebsd_version));
    let stamp = dir.join("HEADERS");
    let want = KERNEL_HEADER_DIRS.join("\n");
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
    args.extend(
        KERNEL_HEADER_DIRS
            .iter()
            .map(|d| format!("usr/src/sys/{d}")),
    );
    run(&dir, "tar", &args)?;
    std::fs::remove_file(&src)?;
    let sys = dir.join("usr/src/sys");
    let kmod = dir.join("kmod");
    std::fs::create_dir_all(&kmod)?;
    std::fs::write(kmod.join("opt_global.h"), "")?;
    std::fs::rename(sys.join("amd64/include"), kmod.join("machine"))?;
    std::fs::rename(sys.join("x86/include"), kmod.join("x86"))?;
    std::fs::write(&stamp, want)?;
    Ok(dir)
}

/// GRUB's `include/` directory, fetched on first use into
/// `<cache>/grub-<version>/`, with `grub/cpu` and `grub/machine` copied
/// from the x86_64 EFI headers as GRUB's `configure` would link them (and
/// as `freebsd/grub/Makefile` does). `HEADERS` records the tarball's
/// checksum; another tarball fetches again.
fn grub_headers(profile: &Profile) -> anyhow::Result<PathBuf> {
    let cache = cache_dir(
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
    )?;
    let grub = &profile.grub;
    let name = format!("grub-{}", grub.version);
    let dir = cache.join(&name);
    let include = dir.join("include");
    let stamp = dir.join("HEADERS");
    if std::fs::read_to_string(&stamp).is_ok_and(|have| have == grub.sha256) {
        return Ok(include);
    }
    println!(
        "Fetching the GRUB {} headers for clang-tidy into {} (first use only)",
        grub.version,
        dir.display()
    );
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let tarball = dir.join("grub.tar");
    download(&grub.url, &tarball)?;
    DistManifest::verify_sha256(&tarball, &grub.sha256)?;
    let args = [
        "-xf".to_string(),
        tarball.display().to_string(),
        "-C".to_string(),
        cache.display().to_string(),
        format!("{name}/include"),
    ];
    run(&dir, "tar", &args)?;
    std::fs::remove_file(&tarball)?;
    let fs = FsOps::new(false);
    let x86_64 = include.join("grub/x86_64");
    fs.copy_tree(&x86_64, &include.join("grub/cpu"), &[])?;
    fs.copy_tree(&x86_64.join("efi"), &include.join("grub/machine"), &[])?;
    std::fs::write(&stamp, &grub.sha256)?;
    Ok(include)
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
/// (`sys/conf/kmod.mk` and `kern.mk`), minus optimisation and debug
/// flags. The kernel's headers are system headers to clang-tidy:
/// findings in them are FreeBSD's business, not ours.
fn kmod_flags(profile: &Profile, headers: &Path) -> Vec<String> {
    let sys = headers.join("usr/src/sys");
    let kmod = headers.join("kmod");
    let mut flags: Vec<String> = [
        "-std=gnu17",
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
    fn the_grub_module_is_linted_with_its_own_flags_and_headers() {
        let files = [
            PathBuf::from("freebsd/grub/jos_efi.c"),
            PathBuf::from("freebsd/jos_bootwatch/jos_bootwatch.c"),
            PathBuf::from("freebsd/grubby/alice.c"),
        ];
        let (grub, kmod): (Vec<PathBuf>, Vec<PathBuf>) =
            files.into_iter().partition(|f| f.starts_with(GRUB_DIR));
        assert_eq!(grub, [PathBuf::from("freebsd/grub/jos_efi.c")]);
        assert_eq!(kmod.len(), 2);
        assert_eq!(
            tidy_args(&grub),
            ["--quiet".to_string(), "freebsd/grub/jos_efi.c".into()]
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
            grub: Grub {
                version: "2.04".into(),
                url: "https://g/grub-2.04.tar.xz".into(),
                sha256: "0".repeat(64),
            },
        }
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
        assert!(text.starts_with("-std=gnu17 -D_KERNEL -DKLD_MODULE -nostdinc "));
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
