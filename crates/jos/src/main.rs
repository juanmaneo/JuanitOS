//! `jos` — JuanitOS qube management CLI.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use juanitos_core::bhyve::{self, Vm};
use juanitos_core::config::load_global_config;
use juanitos_core::doctor::{self, Status};
use juanitos_core::firewall::{self, Action, Rule};
use juanitos_core::fsops::FsOps;
use juanitos_core::pci::{self, PciAddr};
use juanitos_core::supervisor::{self, Supervisor};
use juanitos_core::zfs::Storage;
use juanitos_core::{
    DryRunner, GlobalConfig, OsType, Qube, QubeKind, QubeStore, RealRunner, Runner,
};

#[derive(Parser)]
#[command(
    name = "jos",
    version,
    about = "JuanitOS qube manager (FreeBSD + bhyve)"
)]
struct Cli {
    /// Print commands instead of executing them.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Path to the global config (default: /usr/local/etc/juanitos/jos.toml).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the ZFS datasets and config directory.
    Init,
    /// Check host readiness (kernel modules, IOMMU, ZFS, firmware, ppt).
    Doctor,
    /// List qubes.
    Ls,
    /// Define a new qube and provision its disk.
    Create(CreateArgs),
    /// Delete a qube definition and its disk.
    Remove {
        name: String,
        /// Also destroy the ZFS dataset even if the qube is running.
        #[arg(long)]
        force: bool,
    },
    /// Start a qube under a background supervisor.
    Start {
        name: String,
        /// Run the supervisor in the foreground instead of detaching it.
        #[arg(long)]
        foreground: bool,
    },
    /// Run a qube's supervisor loop in the foreground (what `start` spawns).
    Supervise {
        name: String,
        /// Give up after this many guest-initiated reboots.
        #[arg(long)]
        max_restarts: Option<u32>,
    },
    /// Power off a qube (its supervisor then exits on its own).
    Stop { name: String },
    /// Forcefully destroy a qube's VM, its supervisor and tap interfaces.
    Kill { name: String },
    /// Attach to a qube's serial console.
    Console { name: String },
    /// PCI passthrough helpers.
    Pci {
        #[command(subcommand)]
        cmd: PciCmd,
    },
    /// Template helpers.
    Template {
        #[command(subcommand)]
        cmd: TemplateCmd,
    },
    /// Per-qube outbound firewall rules.
    Firewall {
        #[command(subcommand)]
        cmd: FirewallCmd,
    },
    /// Start every qube marked `autostart` (used by the rc script).
    Autostart,
}

#[derive(Args)]
struct CreateArgs {
    name: String,
    #[arg(long, value_parser = parse_kind)]
    kind: QubeKind,
    #[arg(long, value_parser = parse_os)]
    os: OsType,
    /// Template qube to clone from.
    #[arg(long)]
    template: Option<String>,
    /// Bootable read-only image for `--os unikernel` qubes.
    #[arg(long, value_name = "PATH")]
    kernel: Option<String>,
    #[arg(long)]
    cpus: Option<u32>,
    #[arg(long)]
    memory_mb: Option<u64>,
    /// Disk size for a fresh zvol (ignored when cloning a template).
    #[arg(long)]
    disk_gb: Option<u64>,
    /// NetVM/FirewallVM providing network; `none` for no network.
    #[arg(long)]
    netvm: Option<String>,
    /// PCI devices to pass through (B/S/F), repeatable.
    #[arg(long = "pci", value_name = "B/S/F", value_parser = parse_pci)]
    pci: Vec<PciAddr>,
    #[arg(long)]
    autostart: bool,
    /// Boot with the legacy bhyveload path instead of UEFI.
    #[arg(long)]
    no_uefi: bool,
    #[arg(long)]
    label: Option<String>,
    /// Fixed client address (default: next free one in 10.137.0.0/16).
    #[arg(long)]
    ip: Option<std::net::Ipv4Addr>,
    /// Outbound rule, repeatable, e.g. `--firewall "allow tcp any 443"`.
    #[arg(long = "firewall", value_name = "RULE", value_parser = parse_rule)]
    firewall: Vec<Rule>,
    /// Verdict when no rule matches.
    #[arg(long, value_parser = parse_action)]
    firewall_policy: Option<Action>,
}

#[derive(Subcommand)]
enum PciCmd {
    /// List PCI devices from `pciconf -l -v`.
    List {
        /// Only display controllers (class 0x03).
        #[arg(long)]
        gpus: bool,
        /// Only devices already bound to ppt(4).
        #[arg(long)]
        ready: bool,
    },
    /// Print a `pptdevs=` line for /boot/loader.conf.
    Pptdevs {
        #[arg(required = true, value_parser = parse_pci)]
        addrs: Vec<PciAddr>,
    },
}

#[derive(Subcommand)]
enum TemplateCmd {
    /// Snapshot a template so future clones use its current state.
    Snapshot {
        name: String,
        /// Snapshot name (default: base).
        snap: Option<String>,
    },
}

#[derive(Subcommand)]
enum FirewallCmd {
    /// Print a qube's rules and policy.
    Show { name: String },
    /// Append a rule, e.g. `jos firewall add work allow tcp any 443`.
    Add {
        name: String,
        #[arg(required = true, trailing_var_arg = true)]
        rule: Vec<String>,
    },
    /// Remove all rules of a qube.
    Clear { name: String },
    /// Set the default verdict (allow|deny).
    Policy {
        name: String,
        #[arg(value_parser = parse_action)]
        policy: Action,
    },
    /// Print the configuration a firewall qube would receive.
    Render { name: String },
}

fn parse_rule(s: &str) -> std::result::Result<Rule, String> {
    s.parse().map_err(|e: jos_fwcfg::ParseError| e.to_string())
}

fn parse_action(s: &str) -> std::result::Result<Action, String> {
    s.parse().map_err(|e: jos_fwcfg::ParseError| e.to_string())
}

fn parse_kind(s: &str) -> std::result::Result<QubeKind, String> {
    s.parse().map_err(|e: juanitos_core::Error| e.to_string())
}

fn parse_os(s: &str) -> std::result::Result<OsType, String> {
    s.parse().map_err(|e: juanitos_core::Error| e.to_string())
}

fn parse_pci(s: &str) -> std::result::Result<PciAddr, String> {
    s.parse().map_err(|e: juanitos_core::Error| e.to_string())
}

struct App {
    runner: Box<dyn Runner>,
    cfg: GlobalConfig,
    store: QubeStore,
    dry_run: bool,
    config_path: Option<PathBuf>,
}

impl App {
    fn new(cli: &Cli) -> Result<Self> {
        let cfg = load_global_config(cli.config.as_deref()).context("loading global config")?;
        let runner: Box<dyn Runner> = if cli.dry_run {
            Box::new(DryRunner::new())
        } else {
            Box::new(RealRunner)
        };
        // JOS_CONFIG_DIR lets the CLI be exercised on a dev box without
        // touching /usr/local/etc.
        let dir = match std::env::var_os("JOS_CONFIG_DIR") {
            Some(d) => PathBuf::from(d).join("qubes"),
            None => cfg.qubes_dir(),
        };
        Ok(App {
            runner,
            cfg,
            store: QubeStore::new(dir),
            dry_run: cli.dry_run,
            config_path: cli.config.clone(),
        })
    }

    fn storage(&self) -> Storage<'_> {
        Storage::new(self.runner.as_ref(), &self.cfg)
    }

    fn vm(&self) -> Vm<'_> {
        Vm::new(self.runner.as_ref(), &self.cfg)
    }

    fn supervisor(&self) -> Supervisor<'_> {
        Supervisor::new(self.runner.as_ref(), &self.cfg, self.dry_run)
    }

    fn init(&self) -> Result<()> {
        self.storage().init()?;
        std::fs::create_dir_all(&self.store.dir)
            .with_context(|| format!("creating {}", self.store.dir.display()))?;
        println!("initialized datasets under {}", self.cfg.root_dataset());
        println!("qube definitions live in {}", self.store.dir.display());
        Ok(())
    }

    fn doctor(&self) -> Result<()> {
        let checks = doctor::run_checks(self.runner.as_ref(), &self.cfg);
        let mut failed = false;
        for c in &checks {
            println!("[{:<4}] {:<40} {}", c.status.as_str(), c.name, c.detail);
            failed |= c.status == Status::Fail;
        }
        if failed {
            bail!("some checks failed");
        }
        Ok(())
    }

    fn ls(&self) -> Result<()> {
        let qubes = self.store.list()?;
        if qubes.is_empty() {
            println!("no qubes defined (try `jos create`)");
            return Ok(());
        }
        println!(
            "{:<20} {:<9} {:<10} {:<8} {:>4} {:>8} {:<14} {:<12} PCI",
            "NAME", "KIND", "OS", "STATE", "CPU", "MEM(MB)", "NETVM", "IP"
        );
        for q in qubes {
            let state = if q.is_template() {
                "-"
            } else if bhyve::is_running(&q.name) {
                "running"
            } else if supervisor::read_pid(&self.cfg, &q.name).is_some() {
                // Supervisor alive (or stale pidfile) but no VM: between boots.
                "booting"
            } else {
                "stopped"
            };
            let netvm = if q.is_template() {
                "-".to_string()
            } else {
                self.vm()
                    .effective_netvm(&q)
                    .unwrap_or_else(|| "none".into())
            };
            let pci: Vec<String> = q.pci_passthrough.iter().map(|a| a.to_string()).collect();
            let ip = q.ip.map(|i| i.to_string()).unwrap_or_else(|| "-".into());
            println!(
                "{:<20} {:<9} {:<10} {:<8} {:>4} {:>8} {:<14} {:<12} {}",
                q.name,
                q.kind,
                q.os,
                state,
                q.cpus,
                q.memory_mb,
                netvm,
                ip,
                pci.join(",")
            );
        }
        Ok(())
    }

    fn create(&self, a: CreateArgs) -> Result<()> {
        Qube::validate_name(&a.name)?;
        if self.store.exists(&a.name) {
            bail!(juanitos_core::Error::QubeExists(a.name));
        }
        let mut q = Qube::new(&a.name, a.kind, a.os);
        q.template = a.template;
        q.kernel = a.kernel;
        match (q.is_unikernel(), &q.kernel, &q.template) {
            (true, None, _) => bail!("--os unikernel requires --kernel <image>"),
            (true, _, Some(_)) => bail!("unikernel qubes cannot be cloned from a template"),
            (false, Some(_), _) => bail!("--kernel is only valid with --os unikernel"),
            _ => {}
        }
        if a.kind == QubeKind::Template && q.is_unikernel() {
            bail!("unikernel qubes cannot be templates");
        }
        if let Some(c) = a.cpus {
            q.cpus = c;
        }
        if let Some(m) = a.memory_mb {
            q.memory_mb = m;
        }
        if let Some(d) = a.disk_gb {
            q.disk_gb = d;
        }
        q.netvm = match a.netvm.as_deref() {
            Some("none") => None,
            Some(n) => Some(n.to_string()),
            None if a.kind == QubeKind::Net => None,
            None => self.cfg.default_netvm.clone(),
        };
        q.pci_passthrough = a.pci;
        q.autostart = a.autostart;
        q.uefi = !a.no_uefi;
        q.label = a.label;
        q.firewall = a.firewall;
        if let Some(p) = a.firewall_policy {
            q.firewall_policy = p;
        }
        let existing = self.store.list()?;
        if let Some(ip) = a.ip {
            if !firewall::CLIENT_NET.contains(ip) {
                bail!(
                    "{ip} is outside the client network {}",
                    firewall::CLIENT_NET
                );
            }
            if let Some(o) = existing.iter().find(|o| o.ip == Some(ip)) {
                bail!("{ip} is already used by {}", o.name);
            }
            q.ip = Some(ip);
        }
        firewall::assign_address(&mut q, &existing, self.cfg.default_netvm.as_deref())?;

        if let Some(t) = &q.template {
            let tq = self.store.get(t).context("looking up template")?;
            if !tq.is_template() {
                bail!("{t} is not a template qube");
            }
        }
        if let Some(n) = &q.netvm {
            if let Ok(nq) = self.store.get(n) {
                if !nq.provides_network() {
                    bail!("{n} is not a net or firewall qube");
                }
            } else {
                eprintln!("warning: netvm {n} is not defined yet");
            }
        }

        let store = &self.store;
        self.storage()
            .provision(&q, |t| store.get(t).ok())
            .context("provisioning disk")?;
        self.store.save(&q)?;
        let boot = match &q.kernel {
            Some(k) => k.clone(),
            None => self.cfg.zvol_path(&q),
        };
        match q.ip {
            Some(ip) => println!("created {} ({boot}, ip {ip})", q.name),
            None => println!("created {} ({boot})", q.name),
        }
        Ok(())
    }

    /// Write the raw config disk a unikernel firewall boots with.
    fn write_firewall_config(&self, fw: &Qube) -> Result<()> {
        let all = self.store.list()?;
        let clients = firewall::clients_of(&all, &fw.name, self.cfg.default_netvm.as_deref());
        let text = firewall::render_config(fw, &clients);
        let path = PathBuf::from(self.cfg.firewall_config_disk(&fw.name));
        FsOps::new(self.dry_run).write_bytes(&path, &firewall::config_disk_bytes(&text)?)?;
        Ok(())
    }

    fn firewall(&self, cmd: FirewallCmd) -> Result<()> {
        match cmd {
            FirewallCmd::Show { name } => {
                let q = self.store.get(&name)?;
                match (q.ip, &q.mac) {
                    (Some(ip), Some(mac)) => println!("{name}: ip {ip} mac {mac}"),
                    _ => println!("{name}: no network address"),
                }
                for (i, r) in q.firewall.iter().enumerate() {
                    println!("{:>3}  {r}", i + 1);
                }
                println!("policy {}", q.firewall_policy);
                Ok(())
            }
            FirewallCmd::Add { name, rule } => {
                let mut q = self.store.get(&name)?;
                let rule: Rule = rule.join(" ").parse()?;
                println!("{name}: + {rule}");
                q.firewall.push(rule);
                self.store.save(&q)?;
                self.note_restart(&name);
                Ok(())
            }
            FirewallCmd::Clear { name } => {
                let mut q = self.store.get(&name)?;
                q.firewall.clear();
                self.store.save(&q)?;
                self.note_restart(&name);
                Ok(())
            }
            FirewallCmd::Policy { name, policy } => {
                let mut q = self.store.get(&name)?;
                q.firewall_policy = policy;
                self.store.save(&q)?;
                self.note_restart(&name);
                Ok(())
            }
            FirewallCmd::Render { name } => {
                let fw = self.store.get(&name)?;
                if fw.kind != QubeKind::Firewall {
                    bail!("{name} is not a firewall qube");
                }
                let all = self.store.list()?;
                let clients =
                    firewall::clients_of(&all, &fw.name, self.cfg.default_netvm.as_deref());
                print!("{}", firewall::render_config(&fw, &clients));
                Ok(())
            }
        }
    }

    fn note_restart(&self, name: &str) {
        if let Ok(q) = self.store.get(name) {
            if let Some(fw) = self.vm().effective_netvm(&q) {
                eprintln!("note: rules apply after `jos stop {fw} && jos start {fw}`");
            }
        }
    }

    fn remove(&self, name: &str, force: bool) -> Result<()> {
        let q = self.store.get(name)?;
        if bhyve::is_running(name) {
            if !force {
                bail!("{name} is running; stop it first or use --force");
            }
            self.supervisor().kill(name)?;
        }
        self.storage().destroy(&q)?;
        self.store.remove(name)?;
        println!("removed {name}");
        Ok(())
    }

    fn check_not_running(&self, name: &str) -> Result<()> {
        if bhyve::is_running(name) {
            bail!("{name} is already running");
        }
        if let Some(pid) = supervisor::read_pid(&self.cfg, name) {
            if supervisor::process_alive(self.runner.as_ref(), pid) {
                bail!("{name} already has a supervisor (pid {pid})");
            }
        }
        Ok(())
    }

    /// Run the supervisor loop in this process until the guest powers off.
    fn supervise(&self, name: &str, max_restarts: Option<u32>) -> Result<()> {
        let q = self.store.get(name)?;
        self.check_not_running(name)?;
        let mut sup = self.supervisor();
        if max_restarts.is_some() {
            sup.max_restarts = max_restarts;
        }
        if bhyve::has_config_disk(&q) {
            sup.pre_boot = Some(Box::new(|fw: &Qube| {
                self.write_firewall_config(fw)
                    .map_err(|e| juanitos_core::Error::Other(format!("{e:#}")))
            }));
        }
        let report = sup.run(&q)?;
        println!("{name}: {} after {} boot(s)", report.exit, report.boots);
        Ok(())
    }

    /// Spawn `jos supervise <name>` detached, logging to `run_dir/<name>.log`.
    fn start(&self, name: &str, foreground: bool) -> Result<()> {
        if foreground || self.dry_run {
            // A dry run shows the commands a boot would issue; detaching
            // would hide them.
            return self.supervise(name, None);
        }
        let q = self.store.get(name)?;
        self.check_not_running(name)?;
        let fs = FsOps::new(false);
        fs.mkdir_p(std::path::Path::new(&self.cfg.run_dir))?;
        let log_path = supervisor::logfile(&self.cfg, name);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("opening {}", log_path.display()))?;
        let log_err = log
            .try_clone()
            .with_context(|| format!("opening {}", log_path.display()))?;

        let exe = std::env::current_exe().context("locating the jos binary")?;
        let mut cmd = std::process::Command::new(exe);
        if let Some(c) = &self.config_path {
            cmd.arg("--config").arg(c);
        }
        cmd.arg("supervise")
            .arg(&q.name)
            .stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(log_err);
        #[cfg(unix)]
        {
            // Own process group: the supervisor must outlive this shell and
            // not receive its SIGINT/SIGHUP.
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd.spawn().context("spawning supervisor")?;
        println!(
            "started {name} (supervisor pid {}, log {})",
            child.id(),
            log_path.display()
        );
        Ok(())
    }

    fn autostart(&self) -> Result<()> {
        let mut failures = 0;
        for q in self.store.list()?.into_iter().filter(|q| q.autostart) {
            if let Err(e) = self.start(&q.name, false) {
                eprintln!("error starting {}: {e:#}", q.name);
                failures += 1;
            }
        }
        if failures > 0 {
            bail!("{failures} qube(s) failed to start");
        }
        Ok(())
    }

    fn pci_list(&self, gpus: bool, ready: bool) -> Result<()> {
        let devs = pci::list_devices(self.runner.as_ref())?;
        println!(
            "{:<8} {:<10} {:<9} {:<8} DESCRIPTION",
            "ADDR", "DRIVER", "VEN:DEV", "CLASS"
        );
        for d in devs {
            if gpus && !d.is_gpu() {
                continue;
            }
            if ready && !d.is_passthrough_ready() {
                continue;
            }
            println!(
                "{:<8} {:<10} {:04x}:{:04x} {:06x}   {}",
                d.addr.to_string(),
                d.driver,
                d.vendor,
                d.device,
                d.class,
                d.description.unwrap_or_default()
            );
        }
        Ok(())
    }

    fn template_snapshot(&self, name: &str, snap: Option<String>) -> Result<()> {
        let q = self.store.get(name)?;
        if !q.is_template() {
            bail!("{name} is not a template qube");
        }
        let snap = snap.unwrap_or_else(|| "base".into());
        let full = self.storage().snapshot_template(name, &snap)?;
        println!("created {full}");
        Ok(())
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let app = App::new(&cli)?;
    match cli.cmd {
        Cmd::Init => app.init(),
        Cmd::Doctor => app.doctor(),
        Cmd::Ls => app.ls(),
        Cmd::Create(a) => app.create(a),
        Cmd::Remove { name, force } => app.remove(&name, force),
        Cmd::Start { name, foreground } => app.start(&name, foreground),
        Cmd::Supervise { name, max_restarts } => app.supervise(&name, max_restarts),
        Cmd::Stop { name } => {
            app.store.get(&name)?;
            app.vm().stop(&name)?;
            Ok(())
        }
        Cmd::Kill { name } => {
            app.store.get(&name)?;
            app.supervisor().kill(&name)?;
            Ok(())
        }
        Cmd::Console { name } => {
            app.store.get(&name)?;
            let cmd = bhyve::build_console_command(&app.cfg, &name);
            if cli.dry_run {
                println!("+ {}", cmd.to_shell());
                return Ok(());
            }
            // `cu` needs the terminal, so don't capture its output.
            let status = std::process::Command::new(&cmd.program)
                .args(&cmd.args)
                .status()
                .with_context(|| format!("running {}", cmd.to_shell()))?;
            if !status.success() {
                bail!("console exited with {status}");
            }
            Ok(())
        }
        Cmd::Pci { cmd } => match cmd {
            PciCmd::List { gpus, ready } => app.pci_list(gpus, ready),
            PciCmd::Pptdevs { addrs } => {
                println!("{}", pci::pptdevs_line(&addrs));
                Ok(())
            }
        },
        Cmd::Template { cmd } => match cmd {
            TemplateCmd::Snapshot { name, snap } => app.template_snapshot(&name, snap),
        },
        Cmd::Firewall { cmd } => app.firewall(cmd),
        Cmd::Autostart => app.autostart(),
    }
}
