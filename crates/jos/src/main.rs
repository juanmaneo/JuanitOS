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
use juanitos_core::requests::{self, Broker, Disposition, Found, Policy, Verdict};
use juanitos_core::share::{self, Share};
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
    /// Run a command inside a qube and print its output: through the
    /// qube's agent channel (`jos-agentd` in the templates), or over the
    /// serial console when the qube has no agent.
    Run {
        name: String,
        /// Seconds to wait for the command to finish.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// User the agent runs the command as.
        #[arg(long, default_value = juanitos_core::agent::DEFAULT_USER)]
        user: String,
        /// Type into the serial console even when the agent is there.
        #[arg(long)]
        console: bool,
        /// Console only: log in as USER[:PASSWORD] if a login prompt shows
        /// (default: the guest OS's stock user; Redox: `user`).
        #[arg(long, value_name = "USER[:PASSWORD]")]
        login: Option<String>,
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Copy a file or directory into a qube's inbox (`/mnt/jos/in/<src>/`)
    /// from another qube (`QUBE:PATH`) or from dom0 (`dom0:PATH` or PATH).
    Copy {
        #[arg(value_name = "[QUBE:]PATH")]
        src: String,
        /// Destination qube.
        dst: String,
        /// Seconds to wait for the source qube to fill its outbox.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
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
    /// The policy for requests qubes make to dom0 (`jos-copy` in a qube).
    Policy {
        #[command(subcommand)]
        cmd: PolicyCmd,
    },
    /// Pending requests from qubes, and the operator's answers.
    Requests {
        #[command(subcommand)]
        cmd: Option<RequestsCmd>,
    },
    /// Answer qube requests according to the policy, forever (the daemon)
    /// or once.
    Serve {
        /// One pass over the pending requests, then exit.
        #[arg(long)]
        once: bool,
        /// Seconds between two passes.
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
    /// Start every qube marked `autostart` (used by the rc script).
    Autostart,
}

#[derive(Subcommand)]
enum PolicyCmd {
    /// Print the policy.
    Show,
    /// Append a rule: `jos policy add work personal allow` (`*` matches
    /// any qube; first matching rule wins; no match means `ask`).
    Add {
        src: String,
        dst: String,
        #[arg(value_parser = parse_verdict)]
        verdict: Verdict,
    },
    /// Remove every rule.
    Clear,
}

#[derive(Subcommand)]
enum RequestsCmd {
    /// List the pending requests (the default).
    Ls,
    /// Grant a pending request.
    Approve { qube: String, id: String },
    /// Refuse a pending request.
    Deny { qube: String, id: String },
}

fn parse_verdict(s: &str) -> std::result::Result<Verdict, String> {
    s.parse().map_err(|e: juanitos_core::Error| e.to_string())
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
    /// Host interface bridged straight into the qube's upstream NIC (a
    /// `net` qube on a machine without NIC passthrough).
    #[arg(long, value_name = "IFACE")]
    uplink: Option<String>,
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
    /// List the templates `jos template install` can build.
    Recipes,
    /// Build a template from its distribution's cloud image: download and
    /// verify, write to a zvol, provision on first boot, snapshot @base.
    Install {
        /// Recipe name (see `jos template recipes`).
        recipe: String,
        /// Template qube name (default: the recipe's).
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        disk_gb: Option<u64>,
        /// Write the image and the seed but skip the provisioning boot.
        #[arg(long)]
        no_boot: bool,
        /// Provision through this host interface instead of the NetVM
        /// (before sys-net exists, or on a dom0 that has the NIC).
        #[arg(long, value_name = "IFACE")]
        uplink: Option<String>,
        /// Use an already downloaded image (with --checksums) instead of
        /// fetching: a dom0 without network gets it from a USB stick.
        #[arg(long, value_name = "FILE", requires = "checksums")]
        image: Option<PathBuf>,
        /// The distribution's checksum list that covers --image.
        #[arg(long, value_name = "FILE")]
        checksums: Option<PathBuf>,
    },
    /// Boot a template for maintenance (updates) until it powers off; take
    /// a snapshot afterwards so new clones get the changes.
    Run {
        name: String,
        /// Attach a cloud-init seed ISO (NoCloud `cidata`).
        #[arg(long)]
        seed: Option<String>,
    },
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

    fn share(&self) -> Share<'_> {
        Share::new(self.runner.as_ref(), &self.cfg)
    }

    /// The policy file lives next to the qube definitions.
    fn policy_path(&self) -> PathBuf {
        let config_dir = self
            .store
            .dir
            .parent()
            .map_or_else(|| PathBuf::from(&self.cfg.config_dir), PathBuf::from);
        requests::policy_path(&config_dir)
    }

    fn broker(&self) -> Result<Broker<'_>> {
        let policy = Policy::load(&self.policy_path())
            .with_context(|| format!("reading {}", self.policy_path().display()))?;
        Ok(Broker::new(
            self.runner.as_ref(),
            &self.cfg,
            &self.store,
            policy,
        ))
    }

    fn policy(&self, cmd: PolicyCmd) -> Result<()> {
        let path = self.policy_path();
        let mut policy = Policy::load(&path)?;
        match cmd {
            PolicyCmd::Show => {
                if policy.rules.is_empty() {
                    println!("(no rules: every request waits for `jos requests approve`)");
                }
                for (i, r) in policy.rules.iter().enumerate() {
                    println!("{:>3}  {r}", i + 1);
                }
                Ok(())
            }
            PolicyCmd::Add { src, dst, verdict } => {
                let rule = requests::Rule::new(&src, &dst, verdict)?;
                println!("+ {rule}");
                policy.rules.push(rule);
                policy.save(&path)?;
                Ok(())
            }
            PolicyCmd::Clear => {
                policy.rules.clear();
                policy.save(&path)?;
                Ok(())
            }
        }
    }

    fn requests(&self, cmd: Option<RequestsCmd>) -> Result<()> {
        let broker = self.broker()?;
        match cmd.unwrap_or(RequestsCmd::Ls) {
            RequestsCmd::Ls => {
                let found = broker.scan()?;
                if found.is_empty() {
                    println!("no pending requests");
                }
                for f in &found {
                    match &f.request {
                        Ok(r) => println!(
                            "{:<16} {:<20} copy {} -> {} [{}]",
                            r.src,
                            r.id,
                            r.base,
                            r.dst,
                            broker.policy.decide(&r.src, &r.dst)
                        ),
                        Err(e) => println!("{:<16} {:<20} invalid: {e}", f.src, f.id),
                    }
                }
                Ok(())
            }
            RequestsCmd::Approve { qube, id } => {
                let found = broker.find(&qube, &id)?;
                report(&found, &broker.settle(&found, Some(Verdict::Allow))?)
            }
            RequestsCmd::Deny { qube, id } => {
                let found = broker.find(&qube, &id)?;
                report(&found, &broker.settle(&found, Some(Verdict::Deny))?)
            }
        }
    }

    fn serve(&self, once: bool, interval: u64) -> Result<()> {
        let mut broker = self.broker()?;
        if !once {
            println!(
                "serving qube requests from {} with policy {} ({} rules)",
                self.cfg.share_dir,
                self.policy_path().display(),
                broker.policy.rules.len()
            );
        }
        loop {
            for (found, disposition) in broker.serve_once()? {
                if disposition != Disposition::Pending || once {
                    report(&found, &disposition)?;
                }
            }
            if once {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_secs(interval.max(1)));
            // `jos policy add` must reach the running daemon: re-read the
            // file every pass (a few lines), keeping the last good policy
            // while an edit is half-written or wrong.
            match Policy::load(&self.policy_path()) {
                Ok(p) if p != broker.policy => {
                    println!("policy reloaded ({} rules)", p.rules.len());
                    broker.policy = p;
                }
                Ok(_) => {}
                Err(e) => eprintln!("policy not reloaded: {e}"),
            }
        }
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
            // Templates use theirs only while being provisioned or updated.
            let netvm = match &q.uplink {
                Some(iface) => format!("uplink:{iface}"),
                None => self
                    .vm()
                    .effective_netvm(&q)
                    .unwrap_or_else(|| "none".into()),
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
            // Kept explicitly: an absent netvm means the default one.
            Some(n) => Some(n.to_string()),
            None if a.kind == QubeKind::Net => None,
            None => self.cfg.default_netvm.clone(),
        };
        q.pci_passthrough = a.pci;
        q.uplink = a.uplink;
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
        if let Some(n) = q.resolve_netvm(None) {
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
        if q.os.has_share() {
            self.share().destroy(name)?;
        }
        self.store.remove(name)?;
        println!("removed {name}");
        Ok(())
    }

    /// `jos copy`: have the source qube export the path to its outbox over
    /// the console, then move it into the destination's inbox on dom0.
    fn copy(&self, src: &str, dst: &str, timeout: u64) -> Result<()> {
        let src = share::Source::parse(src)?;
        let dst = self.store.get(dst)?;
        let src_qube = match src.qube() {
            Some(name) => Some(self.store.get(name)?),
            None => None,
        };
        if let Some(q) = &src_qube {
            if !q.os.has_share() {
                bail!(
                    "{} runs {} which has no dom0 share; cannot copy from it",
                    q.name,
                    q.os
                );
            }
        } else if !self.dry_run && !share::dom0_path_exists(src.path()) {
            bail!("{}: no such file or directory on dom0", src.path());
        }
        let timeout = std::time::Duration::from_secs(timeout);
        let mut run_in_qube = |name: &str, command: &str| -> juanitos_core::Result<()> {
            let q = self.store.get(name)?;
            let Some(outcome) = self
                .run_in(
                    &q,
                    juanitos_core::agent::DEFAULT_USER,
                    command,
                    timeout,
                    None,
                    false,
                )
                .map_err(|e| juanitos_core::Error::Other(format!("{e:#}")))?
            else {
                return Ok(());
            };
            match outcome.status {
                Some(0) => Ok(()),
                Some(code) => Err(juanitos_core::Error::Other(format!(
                    "{name}: exporting {} failed (exit {code}):\n{}",
                    src.path(),
                    outcome.output.trim_end()
                ))),
                None => Err(juanitos_core::Error::Other(format!(
                    "{name}: could not read the exit status of the export"
                ))),
            }
        };
        let delivery = self.share().copy(&src, &dst, &mut run_in_qube)?;
        println!(
            "copied {src} to {}:{} ({})",
            dst.name, delivery.guest_path, delivery.dom0_path
        );
        Ok(())
    }

    /// Run `command` in `q` as `user`: through the agent channel when the
    /// qube has one and its daemon answers, else over the serial console
    /// (always, with `console`). `None` in dry-run mode, after printing
    /// what would be sent.
    fn run_in(
        &self,
        q: &Qube,
        user: &str,
        command: &str,
        timeout: std::time::Duration,
        login: Option<&juanitos_core::console::Login>,
        console: bool,
    ) -> Result<Option<juanitos_core::console::Outcome>> {
        if !console && q.os.has_agent() {
            juanitos_core::agent::validate_user(user)?;
            juanitos_core::agent::validate_command(command)?;
            let sock = self.cfg.agent_socket(&q.name);
            let id = juanitos_core::agent::new_id();
            if self.dry_run {
                println!(
                    "+ ask the agent on {sock}: {}",
                    juanitos_core::agent::request(&id, user, command).trim_end()
                );
                return Ok(None);
            }
            let agent = juanitos_core::agent::Agent::new(self.runner.as_ref(), &self.cfg);
            if agent.available(q) {
                match agent_run(&sock, &id, user, command, timeout)? {
                    Some(outcome) => return Ok(Some(outcome)),
                    None => eprintln!("{}: no agent answered; using the serial console", q.name),
                }
            }
        }
        if !q.os.has_console_shell() {
            bail!(
                "{} runs {} which has no shell on its serial console; `jos run` needs one",
                q.name,
                q.os
            );
        }
        let dev = self.cfg.console_host_device(&q.name);
        let login = login.cloned().or_else(|| q.os.console_login());
        let dialect = q.os.console_dialect();
        if self.dry_run {
            println!(
                "+ type on {dev}: {}",
                juanitos_core::console::script_for(dialect, command).trim_end()
            );
            if let Some(l) = &login {
                println!("  (logging in as {} first if asked)", l.user);
            }
            return Ok(None);
        }
        Ok(Some(console_run(
            &dev,
            command,
            timeout,
            login.as_ref(),
            dialect,
        )?))
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

    fn template_install(
        &self,
        recipe: &str,
        opts: &juanitos_core::template::InstallOptions,
    ) -> Result<()> {
        let Some(r) = juanitos_core::template::find(recipe) else {
            bail!("unknown template recipe {recipe:?} (see `jos template recipes`)");
        };
        juanitos_core::template::TemplateInstaller {
            runner: self.runner.as_ref(),
            cfg: &self.cfg,
            store: &self.store,
            dry_run: self.dry_run,
        }
        .install(r, opts)?;
        Ok(())
    }

    fn template_run(&self, name: &str, seed: Option<&str>) -> Result<()> {
        let q = self.store.get(name)?;
        if !q.is_template() {
            bail!("{name} is not a template qube (use `jos start`)");
        }
        self.check_not_running(name)?;
        if self.dry_run {
            println!("+ {}", self.vm().maintenance_command(&q, seed)?.to_shell());
            return Ok(());
        }
        let exit = self.vm().maintain(&q, seed)?;
        println!("{name}: {exit}; `jos template snapshot {name} <snap>` to publish the changes");
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

/// `jos run` on the host: the nmdm device, read without blocking.
/// `USER[:PASSWORD]` of `jos run --login`.
/// One line per settled request, for `jos serve` and `jos requests`.
fn report(found: &Found, disposition: &Disposition) -> Result<()> {
    let what = match &found.request {
        Ok(r) => r.to_string(),
        Err(_) => format!("{} {}", found.src, found.id),
    };
    match disposition {
        Disposition::Delivered(d) => println!("{what}: delivered to {}", d.guest_path),
        Disposition::Denied => println!("{what}: denied"),
        Disposition::Pending => println!("{what}: pending (policy says ask)"),
        Disposition::Failed(why) => println!("{what}: failed: {why}"),
    }
    Ok(())
}

/// Print what the guest printed and exit with the command's status.
fn finish(name: &str, outcome: juanitos_core::console::Outcome) -> Result<()> {
    print!("{}", outcome.output);
    match outcome.status {
        Some(0) => Ok(()),
        Some(code) => std::process::exit(code),
        None => bail!("{name}: could not read the exit status"),
    }
}

/// `jos run` through the agent channel: connect to the socket bhyve
/// listens on, ping, then run. `None` when nothing answers the ping (no
/// daemon in the guest).
#[cfg(unix)]
fn agent_run(
    sock: &str,
    id: &str,
    user: &str,
    command: &str,
    timeout: std::time::Duration,
) -> Result<Option<juanitos_core::console::Outcome>> {
    use std::io::Read;
    let mut stream = std::os::unix::net::UnixStream::connect(sock)
        .with_context(|| format!("connecting to {sock} (is the qube running?)"))?;
    stream.set_nonblocking(true)?;
    let poll = |s: &mut std::os::unix::net::UnixStream| -> std::io::Result<Vec<u8>> {
        let mut buf = [0u8; 4096];
        match s.read(&mut buf) {
            Ok(0) => Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "the agent channel closed (bhyve exited, or another client took it)",
            )),
            Ok(n) => Ok(buf.get(..n).unwrap_or(&[]).to_vec()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    };
    if !juanitos_core::agent::handshake(&mut stream, id, juanitos_core::agent::PING_TIMEOUT, &poll)?
    {
        return Ok(None);
    }
    Ok(Some(juanitos_core::agent::run(
        &mut stream,
        id,
        user,
        command,
        timeout,
        &poll,
    )?))
}

#[cfg(not(unix))]
fn agent_run(
    _sock: &str,
    _id: &str,
    _user: &str,
    _command: &str,
    _timeout: std::time::Duration,
) -> Result<Option<juanitos_core::console::Outcome>> {
    bail!("jos run needs a FreeBSD host (UNIX socket to bhyve)")
}

fn parse_login(s: &str) -> juanitos_core::console::Login {
    let (user, password) = s.split_once(':').unwrap_or((s, ""));
    juanitos_core::console::Login {
        user: user.to_string(),
        password: password.to_string(),
    }
}

#[cfg(unix)]
fn console_run(
    dev: &str,
    command: &str,
    timeout: std::time::Duration,
    login: Option<&juanitos_core::console::Login>,
    dialect: juanitos_core::console::Dialect,
) -> Result<juanitos_core::console::Outcome> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(dev)
        .with_context(|| format!("opening {dev} (is the qube running?)"))?;
    set_raw(&file).with_context(|| format!("setting {dev} to raw mode"))?;
    let poll = |f: &mut std::fs::File| -> std::io::Result<Vec<u8>> {
        let mut buf = [0u8; 4096];
        match f.read(&mut buf) {
            Ok(n) => Ok(buf.get(..n).unwrap_or(&[]).to_vec()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    };
    Ok(juanitos_core::console::run(
        &mut file, command, timeout, login, dialect, poll,
    )?)
}

/// The host side of an nmdm pair is a tty: without raw mode its line
/// discipline would echo the guest's output straight back to the guest
/// as input (the guest shell would then try to run its own prompt).
#[cfg(unix)]
fn set_raw(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: termios is plain data filled in by tcgetattr; the fd is open
    // for the lifetime of `file`.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut t);
        if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn console_run(
    _dev: &str,
    _command: &str,
    _timeout: std::time::Duration,
    _login: Option<&juanitos_core::console::Login>,
    _dialect: juanitos_core::console::Dialect,
) -> Result<juanitos_core::console::Outcome> {
    bail!("jos run needs a FreeBSD host (nmdm console devices)")
}

fn template_recipes() {
    println!("{:<16} {:<8} {:>5}  DESCRIPTION", "RECIPE", "OS", "DISK");
    for r in juanitos_core::template::RECIPES {
        println!(
            "{:<16} {:<8} {:>4}G  {}",
            r.name, r.os, r.disk_gb, r.summary
        );
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
        Cmd::Run {
            name,
            timeout,
            user,
            console,
            login,
            command,
        } => {
            let q = app.store.get(&name)?;
            let command = command.join(" ");
            let timeout = std::time::Duration::from_secs(timeout);
            let login = login.as_deref().map(parse_login);
            match app.run_in(&q, &user, &command, timeout, login.as_ref(), console)? {
                Some(outcome) => finish(&name, outcome),
                None => Ok(()),
            }
        }
        Cmd::Copy { src, dst, timeout } => app.copy(&src, &dst, timeout),
        Cmd::Pci { cmd } => match cmd {
            PciCmd::List { gpus, ready } => app.pci_list(gpus, ready),
            PciCmd::Pptdevs { addrs } => {
                println!("{}", pci::pptdevs_line(&addrs));
                Ok(())
            }
        },
        Cmd::Template { cmd } => match cmd {
            TemplateCmd::Recipes => {
                template_recipes();
                Ok(())
            }
            TemplateCmd::Install {
                recipe,
                name,
                disk_gb,
                no_boot,
                uplink,
                image,
                checksums,
            } => app.template_install(
                &recipe,
                &juanitos_core::template::InstallOptions {
                    name,
                    disk_gb,
                    no_boot,
                    uplink,
                    image,
                    checksums,
                },
            ),
            TemplateCmd::Run { name, seed } => app.template_run(&name, seed.as_deref()),
            TemplateCmd::Snapshot { name, snap } => app.template_snapshot(&name, snap),
        },
        Cmd::Firewall { cmd } => app.firewall(cmd),
        Cmd::Policy { cmd } => app.policy(cmd),
        Cmd::Requests { cmd } => app.requests(cmd),
        Cmd::Serve { once, interval } => app.serve(once, interval),
        Cmd::Autostart => app.autostart(),
    }
}
