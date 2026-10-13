//! `jos-splash` — BeOS/Haiku-style boot splash for the JuanitOS dom0.
//!
//! `jos-splash run` is started by `rc.d/jos-splash` right after the file
//! systems are mounted; it draws the boot card on the `vt(4)` framebuffer
//! and lights up one icon per stage as the other rc.d hooks report
//! progress with `jos-splash stage <name>`. `finish` hands the screen to
//! the COSMIC greeter (keeping the picture) or back to the text console.
//! See `docs/boot-splash.md`.

mod proto;
// Only the `render` subcommand works without Unix sockets; the daemon
// code still compiles (and its tests run) on Windows.
#[cfg_attr(not(unix), allow(dead_code))]
mod server;

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use jos_gfx::scene::render_screen;
use jos_gfx::{Scene, Stage};

use crate::proto::{Finish, Message};
#[cfg(unix)]
use crate::server::Splash;
#[cfg(unix)]
use jos_fb::NullDisplay;

#[derive(Parser, Debug)]
#[command(name = "jos-splash", version, about = "JuanitOS boot splash")]
struct Cli {
    /// Socket the daemon listens on / clients talk to.
    #[arg(long, global = true, default_value = proto::SOCKET_PATH)]
    socket: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the daemon on the framebuffer until `finish` or Esc.
    Run(RunArgs),
    /// Report that a boot stage was reached (lights its icon).
    Stage { name: String },
    /// Show a one-line status under the icons (empty clears it).
    Status { text: Vec<String> },
    /// End the splash: keep the picture for the greeter, or restore the
    /// text console.
    Finish {
        #[arg(long)]
        console: bool,
    },
    /// Render the card into a PNG (previews, and the kernel splash).
    Render(RenderArgs),
}

#[derive(clap::Args, Debug)]
struct RunArgs {
    /// Virtual terminal to draw on (1-based; 10 = /dev/ttyv9).
    #[arg(long, default_value_t = 10)]
    vt: i32,
    /// Stage already reached when the daemon starts.
    #[arg(long, default_value = "storage")]
    reached: String,
    /// Do not touch the console: serve on the socket with a fake display.
    #[arg(long)]
    dry_run: bool,
}

#[derive(clap::Args, Debug)]
struct RenderArgs {
    out: PathBuf,
    #[arg(long, default_value_t = 1920)]
    width: u32,
    #[arg(long, default_value_t = 1080)]
    height: u32,
    /// Highest stage to light (`none` for all dark).
    #[arg(long, default_value = "kernel")]
    stage: String,
    #[arg(long)]
    status: Option<String>,
    /// Only the card, not a full screen (the kernel centres it itself).
    #[arg(long)]
    card: bool,
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("jos-splash: {e:#}");
        std::process::exit(1);
    }
}

fn parse_stage(name: &str) -> anyhow::Result<Option<Stage>> {
    if name == "none" {
        return Ok(None);
    }
    match Stage::from_name(name) {
        Some(s) => Ok(Some(s)),
        None => bail!(
            "unknown stage {name:?}; one of: {}",
            jos_gfx::STAGES
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::Run(a) => run_daemon(&a, &cli.socket),
        Cmd::Stage { name } => {
            let stage = parse_stage(&name)?.context("stage name required")?;
            send(&Message::Stage(stage), &cli.socket)
        }
        Cmd::Status { text } => send(&Message::Status(text.join(" ")), &cli.socket),
        Cmd::Finish { console } => {
            let mode = if console {
                Finish::Console
            } else {
                Finish::Keep
            };
            send(&Message::Finish(mode), &cli.socket)
        }
        Cmd::Render(a) => render(&a),
    }
}

fn send(msg: &Message, socket: &std::path::Path) -> anyhow::Result<()> {
    // No daemon (text boot, or already finished) is not an error for the
    // rc.d scripts that call us.
    let _ = server::send(msg, socket)?;
    Ok(())
}

fn render(a: &RenderArgs) -> anyhow::Result<()> {
    let mut scene = Scene::new();
    if let Some(stage) = parse_stage(&a.stage)? {
        scene.reach(stage);
    }
    scene.status = a.status.as_ref().map(|s| s.to_uppercase());
    let img = if a.card {
        scene.render_card()
    } else {
        render_screen(&scene, a.width, a.height)
    };
    jos_gfx::png::write(&img, &a.out)?;
    println!(
        "wrote {} ({}x{})",
        a.out.display(),
        img.width(),
        img.height()
    );
    Ok(())
}

#[cfg(target_os = "freebsd")]
fn run_daemon(a: &RunArgs, socket: &std::path::Path) -> anyhow::Result<()> {
    let reached = parse_stage(&a.reached)?;
    if a.dry_run {
        return run_dry(reached, socket);
    }
    let display = jos_fb::vt::VtDisplay::open(a.vt)?;
    eprintln!("jos-splash: drawing on VT {}", a.vt);
    server::serve(Splash::new(display, reached), socket)?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "freebsd")))]
fn run_daemon(a: &RunArgs, socket: &std::path::Path) -> anyhow::Result<()> {
    let reached = parse_stage(&a.reached)?;
    if a.dry_run {
        return run_dry(reached, socket);
    }
    bail!("the framebuffer backend only exists on FreeBSD; use --dry-run here")
}

#[cfg(not(unix))]
fn run_daemon(a: &RunArgs, _socket: &std::path::Path) -> anyhow::Result<()> {
    let _ = parse_stage(&a.reached)?;
    bail!("the splash daemon needs Unix sockets and a FreeBSD console")
}

#[cfg(unix)]
fn run_dry(reached: Option<Stage>, socket: &std::path::Path) -> anyhow::Result<()> {
    println!("jos-splash: dry run, listening on {}", socket.display());
    let display = server::serve(Splash::new(NullDisplay::new(1280, 800), reached), socket)?;
    println!(
        "jos-splash: {} frame(s) presented, console {}",
        display.presented,
        if display.restored { "restored" } else { "kept" }
    );
    Ok(())
}
