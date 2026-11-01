//! `cargo xtask` — project automation in Rust (no Makefiles, no shell
//! scripts to keep portable).
//!
//! ```text
//! cargo xtask ci                  everything CI runs, in order
//! cargo xtask security-audit      cargo-audit + cargo-deny (deny.toml), all workspaces
//! cargo xtask fmt [--fix]         rustfmt, all three workspaces
//! cargo xtask lint                clippy -D warnings: host, FreeBSD, UEFI
//! cargo xtask test                tests: workspace + jos-boot lib on the host
//! cargo xtask coverage [--ratchet] [--html]
//!                                 line coverage per crate, gated by coverage.toml
//! cargo xtask screenshots         regenerate docs/images (installer, splash)
//! ```
//!
//! Extra arguments after `--` go to the underlying cargo commands (e.g.
//! `cargo xtask ci -- --offline`).

mod coverage;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};

/// Standalone UEFI workspaces (they only build for x86_64-unknown-uefi).
const UEFI_CRATES: &[&str] = &["crates/jos-boot", "crates/jos-firewall"];
/// Crates that run in the FreeBSD dom0, cross-checked with clippy.
const DOM0_CRATES: &[&str] = &[
    "juanitos-core",
    "jos",
    "jos-install",
    "jos-release",
    "jos-splash",
    "jos-greet",
    "jos-fb",
];
const FREEBSD: &str = "x86_64-unknown-freebsd";
const UEFI: &str = "x86_64-unknown-uefi";

fn main() {
    if let Err(e) = run() {
        eprintln!("xtask: {e:#}");
        std::process::exit(1);
    }
}

struct Ctx {
    root: PathBuf,
    /// Arguments after `--`, passed to cargo.
    extra: Vec<String>,
}

fn run() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let extra = match args.iter().position(|a| a == "--") {
        Some(i) => {
            let tail = args.split_off(i);
            tail.into_iter().skip(1).collect()
        }
        None => Vec::new(),
    };
    let ctx = Ctx {
        root: root()?,
        extra,
    };
    let flag = |name: &str| args.iter().any(|a| a == name);
    match args.first().map(String::as_str) {
        Some("ci") => ci(&ctx),
        Some("security-audit") => security_audit(&ctx),
        Some("fmt") => fmt(&ctx, flag("--fix")),
        Some("lint") => lint(&ctx),
        Some("test") => test(&ctx),
        Some("coverage") => coverage(&ctx, flag("--ratchet"), flag("--html")),
        Some("screenshots") => screenshots(&ctx),
        Some("help") | Some("--help") | Some("-h") | None => {
            print!("{}", HELP);
            Ok(())
        }
        Some(other) => bail!("unknown task {other:?}\n\n{HELP}"),
    }
}

const HELP: &str = "\
cargo xtask <task> [options] [-- <cargo args>]

  ci                     security audit, fmt, lint, coverage gate, UEFI builds
  security-audit         RustSec advisories (cargo-audit) and the deny.toml policy
                         (cargo-deny: advisories, licenses, sources) on every lockfile
  fmt [--fix]            check (or apply) rustfmt in all workspaces
  lint                   clippy -D warnings for host, FreeBSD and UEFI targets
  test                   workspace tests + jos-boot's library tests on the host
  coverage [--ratchet] [--html]
                         line coverage per crate, checked against coverage.toml;
                         --ratchet raises the floors to the measured values
  screenshots            regenerate docs/images (installer pages, boot card)
";

fn root() -> anyhow::Result<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.parent()
        .map(Path::to_path_buf)
        .context("xtask is not inside the repository")
}

fn cargo(ctx: &Ctx, dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(dir).args(with_extra(args, &ctx.extra));
    println!("\n$ (cd {}) cargo {}", rel(ctx, dir), args.join(" "));
    let st = cmd
        .status()
        .with_context(|| format!("running cargo {}", args.join(" ")))?;
    if !st.success() {
        bail!("cargo {} failed ({st})", args.join(" "));
    }
    Ok(())
}

/// `args` with the pass-through arguments inserted before a `--` (whose
/// tail belongs to the tool cargo runs). `cargo fmt` and the audit tools
/// have their own command lines and take none of them.
fn with_extra(args: &[&str], extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    if matches!(args.first(), Some(&("fmt" | "audit" | "deny"))) {
        return out;
    }
    let at = args.iter().position(|a| *a == "--").unwrap_or(args.len());
    for (i, e) in extra.iter().enumerate() {
        out.insert(at + i, e.clone());
    }
    out
}

fn rel(ctx: &Ctx, dir: &Path) -> String {
    match dir.strip_prefix(&ctx.root) {
        Ok(p) if p.as_os_str().is_empty() => ".".into(),
        Ok(p) => p.display().to_string(),
        Err(_) => dir.display().to_string(),
    }
}

/// The three workspaces, each with its own Cargo.lock: the host one and
/// the standalone UEFI ones.
fn workspaces(ctx: &Ctx) -> Vec<PathBuf> {
    std::iter::once(ctx.root.clone())
        .chain(UEFI_CRATES.iter().map(|dir| ctx.root.join(dir)))
        .collect()
}

/// Is `cargo <subcommand>` installed?
fn has_subcommand(subcommand: &str) -> bool {
    Command::new("cargo")
        .args([subcommand, "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Make sure `cargo <subcommand>` is available, installing it on first
/// use: `install` holds the commands that install it, one per line. CI
/// preinstalls the tools, so this only runs on a fresh machine.
fn require(subcommand: &str, install: &str) -> anyhow::Result<()> {
    if has_subcommand(subcommand) {
        return Ok(());
    }
    println!("cargo-{subcommand} is missing; installing it (first use only):");
    for line in install.lines().map(str::trim).filter(|l| !l.is_empty()) {
        println!("$ {line}");
        let mut words = line.split_whitespace();
        let program = words.next().unwrap_or_default();
        let st = Command::new(program)
            .args(words)
            .status()
            .with_context(|| format!("running `{line}`"))?;
        if !st.success() {
            bail!("`{line}` failed ({st})");
        }
    }
    if !has_subcommand(subcommand) {
        bail!("cargo-{subcommand} is still missing after:\n  {install}");
    }
    Ok(())
}

/// Is a rustup target installed? (Missing ones are skipped with a note,
/// so a fresh checkout without them can still run the host tasks.)
fn has_target(target: &str) -> bool {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim() == target)
        })
        .unwrap_or(false)
}

fn ci(ctx: &Ctx) -> anyhow::Result<()> {
    println!("🔍 Running security audit...");
    security_audit(ctx)?;

    println!("🔍 Running format check...");
    fmt(ctx, false)?;

    println!("🔍 Running clippy linting...");
    lint(ctx)?;

    println!("🔍 Running tests and coverage...");
    // The coverage run is the test run (once, instrumented).
    coverage(ctx, false, false)?;

    println!("🔍 Testing jos-boot library...");
    let boot = ctx.root.join("crates/jos-boot");
    cargo(ctx, &boot, &["test", "--lib", "--target", host_target()])?;

    if has_target(UEFI) {
        println!("🔍 Building UEFI crates...");
        for dir in UEFI_CRATES {
            cargo(ctx, &ctx.root.join(dir), &["build", "--release"])?;
        }
    } else {
        println!("⚠️  Skipping UEFI builds (target not installed)");
    }

    println!("\n✅ xtask ci: all good");
    Ok(())
}

fn host_target() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "freebsd", target_arch = "x86_64")) {
        "x86_64-unknown-freebsd"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else {
        "x86_64-pc-windows-msvc"
    }
}

fn fmt(ctx: &Ctx, fix: bool) -> anyhow::Result<()> {
    let args: &[&str] = if fix {
        &["fmt", "--all"]
    } else {
        &["fmt", "--all", "--check"]
    };
    for dir in workspaces(ctx) {
        cargo(ctx, &dir, args)?;
    }
    Ok(())
}

/// Known vulnerabilities, unmaintained or yanked crates (both tools read
/// the RustSec database), then the deny.toml policy on licenses and
/// sources, for every lockfile.
fn security_audit(ctx: &Ctx) -> anyhow::Result<()> {
    require("audit", "cargo install --locked cargo-audit")?;
    require("deny", "cargo install --locked cargo-deny")?;
    let policy = ctx.root.join("deny.toml");
    let policy = policy.to_string_lossy();
    for dir in workspaces(ctx) {
        cargo(ctx, &dir, &["audit", "--deny", "warnings"])?;
        cargo(ctx, &dir, &["deny", "--config", &policy, "check"])?;
    }
    Ok(())
}

fn lint(ctx: &Ctx) -> anyhow::Result<()> {
    cargo(
        ctx,
        &ctx.root,
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    if has_target(FREEBSD) {
        let mut args = vec!["clippy", "--target", FREEBSD];
        for c in DOM0_CRATES {
            args.extend(["-p", c]);
        }
        args.extend(["--", "-D", "warnings"]);
        cargo(ctx, &ctx.root, &args)?;
    } else {
        println!("note: `rustup target add {FREEBSD}` to cross-check the dom0 crates");
    }
    if has_target(UEFI) {
        for dir in UEFI_CRATES {
            cargo(
                ctx,
                &ctx.root.join(dir),
                &["clippy", "--", "-D", "warnings"],
            )?;
        }
    } else {
        println!("note: `rustup target add {UEFI}` to lint jos-boot and jos-firewall");
    }
    Ok(())
}

fn test(ctx: &Ctx) -> anyhow::Result<()> {
    cargo(ctx, &ctx.root, &["test", "--workspace"])?;
    let boot = ctx.root.join("crates/jos-boot");
    cargo(ctx, &boot, &["test", "--lib", "--target", host_target()])
}

fn coverage(ctx: &Ctx, ratchet: bool, html: bool) -> anyhow::Result<()> {
    require(
        "llvm-cov",
        "rustup component add llvm-tools-preview\n  cargo install cargo-llvm-cov",
    )?;
    let out_dir = ctx.root.join("target/coverage");
    std::fs::create_dir_all(&out_dir)?;
    let lcov_path = out_dir.join("lcov.info");
    let lcov_s = lcov_path.to_string_lossy().into_owned();
    let summary_path = out_dir.join("summary.json");
    let summary_s = summary_path.to_string_lossy().into_owned();
    // Run the tests once, then write both reports: lcov for the CI
    // artifact, the JSON summary (lines, functions, regions) for the gate.
    // `--no-report` merges with earlier profiles, so start clean.
    cargo(ctx, &ctx.root, &["llvm-cov", "clean", "--workspace"])?;
    cargo(
        ctx,
        &ctx.root,
        &[
            "llvm-cov",
            "--workspace",
            "--exclude",
            "xtask",
            "--no-report",
        ],
    )?;
    cargo(
        ctx,
        &ctx.root,
        &["llvm-cov", "report", "--lcov", "--output-path", &lcov_s],
    )?;
    cargo(
        ctx,
        &ctx.root,
        &[
            "llvm-cov",
            "report",
            "--json",
            "--summary-only",
            "--output-path",
            &summary_s,
        ],
    )?;
    if html {
        cargo(ctx, &ctx.root, &["llvm-cov", "report", "--html"])?;
        println!("HTML report: target/llvm-cov/html/index.html");
    }

    let summary = std::fs::read_to_string(&summary_path)
        .with_context(|| format!("reading {}", summary_path.display()))?;
    let per_crate = coverage::parse_summary(&summary, &ctx.root)?;
    if per_crate.is_empty() {
        bail!("{} has no workspace sources", summary_path.display());
    }
    let floors_path = ctx.root.join("coverage.toml");
    let mut floors = match std::fs::read_to_string(&floors_path) {
        Ok(text) => coverage::Floors::parse(&text).context("parsing coverage.toml")?,
        Err(_) => coverage::Floors::default(),
    };
    let (rows, ok) = coverage::check(&per_crate, &floors);
    println!("\n{}", coverage::render_table(&rows));

    if ratchet {
        if !ok {
            bail!("coverage is below a floor; fix that before ratcheting");
        }
        if floors.ratchet(&per_crate) {
            std::fs::write(&floors_path, floors.render())?;
            println!("coverage.toml: floors raised; commit it with your tests");
        } else {
            println!("coverage.toml: already at the measured values");
        }
        return Ok(());
    }
    if !ok {
        bail!(
            "coverage dropped below the floors in coverage.toml \
             (add tests, or justify lowering the floor in review)"
        );
    }
    println!("coverage gate: OK (raise the floors with `cargo xtask coverage --ratchet`)");
    Ok(())
}

fn screenshots(ctx: &Ctx) -> anyhow::Result<()> {
    let images = ctx.root.join("docs/images");
    let images_s = images.to_string_lossy().into_owned();
    cargo(
        ctx,
        &ctx.root,
        &[
            "run",
            "-q",
            "-p",
            "jos-install",
            "--",
            "--screenshots",
            &images_s,
        ],
    )?;
    cargo(
        ctx,
        &ctx.root,
        &[
            "run",
            "-q",
            "-p",
            "jos-greet",
            "--",
            "screenshots",
            &images_s,
        ],
    )?;
    let card = images.join("boot-card.png");
    let card_s = card.to_string_lossy().into_owned();
    cargo(
        ctx,
        &ctx.root,
        &[
            "run",
            "-q",
            "-p",
            "jos-splash",
            "--",
            "render",
            &card_s,
            "--card",
            "--stage",
            "network",
            "--status",
            "Starting qubes",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_args_go_before_the_tool_separator() {
        let extra = vec!["--offline".to_string()];
        assert_eq!(
            with_extra(&["clippy", "--", "-D", "warnings"], &extra),
            vec!["clippy", "--offline", "--", "-D", "warnings"]
        );
        assert_eq!(with_extra(&["test"], &extra), vec!["test", "--offline"]);
        assert_eq!(
            with_extra(&["fmt", "--check"], &extra),
            vec!["fmt", "--check"]
        );
        assert_eq!(
            with_extra(&["audit", "--deny", "warnings"], &extra),
            vec!["audit", "--deny", "warnings"]
        );
        assert_eq!(
            with_extra(&["deny", "check"], &extra),
            vec!["deny", "check"]
        );
    }

    #[test]
    fn repository_root_and_paths() {
        let ctx = Ctx {
            root: root().unwrap(),
            extra: vec![],
        };
        assert!(ctx.root.join("Cargo.toml").is_file());
        assert_eq!(rel(&ctx, &ctx.root), ".");
        assert_eq!(
            rel(&ctx, &ctx.root.join("crates/jos-boot")),
            "crates/jos-boot"
        );
        assert_eq!(rel(&ctx, Path::new("/elsewhere")), "/elsewhere");
        let all = workspaces(&ctx);
        assert_eq!(all.len(), 1 + UEFI_CRATES.len());
        assert!(all.iter().all(|w| w.join("Cargo.lock").is_file()));
        assert!(ctx.root.join("deny.toml").is_file());
        assert!(require("no-such-subcommand", "nothing").is_err());
        assert!(!host_target().is_empty());
    }
}
