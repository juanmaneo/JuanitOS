//! `jos-greet` — the JuanitOS login screen.
//!
//! Started by `rc.d/jos-login` (under daemon(8), restarted if it dies) on
//! a virtual terminal no getty uses (`ttyv8`, VT 9). It draws on the
//! `vt(4)` framebuffer with `jos-fb`, authenticates with PAM, then hands
//! the same VT to the desktop session (COSMIC, through seatd) and comes
//! back when it ends. Pure parts (state machine, renderer, session plan)
//! are tested on any host; `screenshots` renders the screen to PNGs.

#[cfg(target_os = "freebsd")]
mod freebsd;
mod greeter;
// Only the FreeBSD build has a framebuffer to drive; the loop is still
// compiled and tested everywhere.
#[cfg_attr(not(target_os = "freebsd"), allow(dead_code))]
mod gui;
#[cfg_attr(not(target_os = "freebsd"), allow(dead_code))]
mod launch;
mod sessions;
mod ui;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use juanitos_core::sys::Runner;

#[derive(Parser, Debug)]
#[command(name = "jos-greet", version, about = "JuanitOS login screen")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the login screen on the framebuffer (FreeBSD, as root).
    Run(RunArgs),
    /// Render the login screen into PNGs.
    Screenshots {
        dir: PathBuf,
        #[arg(long, default_value_t = 1280)]
        width: u32,
        #[arg(long, default_value_t = 800)]
        height: u32,
    },
    /// List the desktop sessions the login screen would offer.
    Sessions {
        #[arg(long, default_value = sessions::SESSIONS_DIR)]
        dir: PathBuf,
    },
    /// Log in without the screen: password on stdin, then run COMMAND as
    /// the session (checks PAM and the session setup; as root).
    TryLogin {
        #[arg(long)]
        user: String,
        #[arg(long, default_value_t = juanitos_core::dom0::LOGIN_VT)]
        vt: i32,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(clap::Args, Debug)]
struct RunArgs {
    /// Virtual terminal (1-based; 9 = /dev/ttyv8, which has no getty).
    #[arg(long, default_value_t = juanitos_core::dom0::LOGIN_VT)]
    vt: i32,
    /// Accept any non-empty password and start nothing (UI testing).
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value = sessions::SESSIONS_DIR)]
    sessions_dir: PathBuf,
    /// Where the accounts offered in the user field come from.
    #[arg(long, default_value = "/etc/passwd")]
    passwd: PathBuf,
}

fn main() {
    if let Err(e) = run(Cli::parse(), &mut std::io::stdin().lock()) {
        eprintln!("jos-greet: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli, input: &mut dyn std::io::BufRead) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::Screenshots { dir, width, height } => {
            for f in ui::screenshots(&dir, width, height)? {
                println!("{f}");
            }
            Ok(())
        }
        Cmd::Sessions { dir } => {
            print!("{}", list_sessions(&sessions::discover(&dir)));
            Ok(())
        }
        Cmd::TryLogin { user, vt, command } => {
            let mut line = String::new();
            input.read_line(&mut line)?;
            let password = line.trim_end_matches(['\n', '\r']).to_string();
            greeter::wipe(&mut line);
            let session = sessions::Session {
                name: command.first().cloned().unwrap_or_default(),
                exec: command,
                desktop: "COSMIC".to_string(),
            };
            try_login(&user, password, &session, vt)
        }
        Cmd::Run(a) => {
            let runner = juanitos_core::sys::RealRunner;
            let passwd = std::fs::read_to_string(&a.passwd).unwrap_or_default();
            let mut g = greeter::Greeter::new(
                &hostname(&runner),
                greeter::human_users(&passwd),
                sessions::discover(&a.sessions_dir),
            );
            run_screen(&a, &runner, &mut g)
        }
    }
}

fn list_sessions(list: &[sessions::Session]) -> String {
    list.iter()
        .map(|s| format!("{}\t{}\t{}\n", s.name, s.desktop, s.exec.join(" ")))
        .collect()
}

fn hostname(runner: &dyn Runner) -> String {
    runner
        .run("hostname", &[])
        .ok()
        .filter(|o| o.success())
        .map(|o| o.stdout.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "juanitos".to_string())
}

#[cfg(target_os = "freebsd")]
fn run_screen(a: &RunArgs, runner: &dyn Runner, g: &mut greeter::Greeter) -> anyhow::Result<()> {
    let vt = a.vt;
    let mut open = || jos_fb::vt::VtDisplay::open(vt);
    let mut dry = gui::DryBackend { runner };
    let mut real = freebsd::FreeBsdBackend::new(runner, vt);
    let backend: &mut dyn gui::Backend = if a.dry_run { &mut dry } else { &mut real };
    let out = gui::run(&mut open, g, backend, gui::LoopOptions::default())?;
    eprintln!("jos-greet: {out:?}");
    Ok(())
}

#[cfg(target_os = "freebsd")]
fn try_login(
    user: &str,
    mut password: String,
    session: &sessions::Session,
    vt: i32,
) -> anyhow::Result<()> {
    use gui::Backend;
    let runner = juanitos_core::sys::RealRunner;
    let mut b = freebsd::FreeBsdBackend::new(&runner, vt);
    let auth = b.authenticate(user, &password);
    greeter::wipe(&mut password);
    auth.map_err(|e| anyhow::anyhow!("authentication: {e}"))?;
    println!("authenticated {user}");
    b.run_session(user, session)
        .map_err(|e| anyhow::anyhow!("session: {e}"))?;
    println!("session {} ended", session.name);
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn try_login(_: &str, _: String, _: &sessions::Session, _: i32) -> anyhow::Result<()> {
    anyhow::bail!("try-login needs FreeBSD's PAM")
}

#[cfg(not(target_os = "freebsd"))]
fn run_screen(_: &RunArgs, _: &dyn Runner, _: &mut greeter::Greeter) -> anyhow::Result<()> {
    anyhow::bail!("the login screen needs FreeBSD's vt(4); try `jos-greet screenshots DIR`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use juanitos_core::sys::{CmdOutput, MockRunner};

    #[test]
    fn hostname_falls_back() {
        let m = MockRunner::new().respond("hostname", &[], CmdOutput::ok("box.lan\n"));
        assert_eq!(hostname(&m), "box.lan");
        let m = MockRunner::new().respond("hostname", &[], CmdOutput::ok("\n"));
        assert_eq!(hostname(&m), "juanitos");
        let m = MockRunner::new().respond("hostname", &[], CmdOutput::failed(1, ""));
        assert_eq!(hostname(&m), "juanitos");
    }

    #[test]
    fn subcommands() {
        let dir = std::env::temp_dir().join(format!("jos-greet-main-{}", std::process::id()));
        let mut no_input = std::io::empty();
        run(
            Cli::parse_from([
                "jos-greet",
                "screenshots",
                dir.to_str().unwrap(),
                "--width",
                "320",
                "--height",
                "200",
            ]),
            &mut no_input,
        )
        .unwrap();
        assert!(dir.join("greeter-1-login.png").is_file());
        run(
            Cli::parse_from(["jos-greet", "sessions", "--dir", dir.to_str().unwrap()]),
            &mut no_input,
        )
        .unwrap();
        assert_eq!(
            list_sessions(&[sessions::Session::cosmic_default()]),
            "COSMIC\tCOSMIC\t/usr/local/bin/start-cosmic\n"
        );
        #[cfg(not(target_os = "freebsd"))]
        {
            let err = run(
                Cli::parse_from([
                    "jos-greet",
                    "run",
                    "--passwd",
                    "/nonexistent",
                    "--sessions-dir",
                    "/nonexistent",
                ]),
                &mut no_input,
            )
            .unwrap_err();
            assert!(err.to_string().contains("vt(4)"));
            let err = run(
                Cli::parse_from(["jos-greet", "try-login", "--user", "juan", "--", "true"]),
                &mut std::io::Cursor::new(b"pw\n".to_vec()),
            )
            .unwrap_err();
            assert!(err.to_string().contains("PAM"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
