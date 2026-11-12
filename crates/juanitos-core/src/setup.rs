// SPDX-License-Identifier: BSD-3-Clause
//! `jos setup`: the first-boot step that turns a fresh install into a
//! working system, like the QubesOS initial setup creating `sys-net`,
//! `sys-firewall` and the default qubes.
//!
//! [`plan`] is pure: from what the host has ([`Host`]) and the qubes
//! already defined, it lists the [`Step`]s still missing, so running
//! `jos setup` again after a failure resumes where it stopped. The CLI
//! executes them:
//!
//! 1. build the template `sys-net` clones from; its provisioning boot is
//!    bridged to the network card (`bru-<iface>`, layer 2: the dom0 itself
//!    never gets an address);
//! 2. create `sys-net` (the card passed through, or bridged to it: always
//!    for a USB adapter, and on a machine without IOMMU) and
//!    `sys-firewall` (the Rust unikernel when its image is installed, else
//!    a FreeBSD qube);
//! 3. when `sys-net` can run now (bridged, or its card already reserved),
//!    start it and build the other templates through it; else bridged to
//!    the card, like the first one;
//! 4. create `gpu` (the NVIDIA card reserved at install time, with the
//!    functions sharing its slot) and the app qubes;
//! 5. reserve the passed-through devices the dom0 still drives (`pptdevs`
//!    in `/boot/loader.conf`, effective at the next boot); or, when
//!    nothing waits for a reboot, start `sys-net` and `sys-firewall`.
//!
//! The dom0 downloads nothing unless it has a default route of its own:
//! the images come from its cache, where the install media put the
//! FreeBSD one ([`template::local_images`]). A system qube whose template
//! cannot be had is an error; any other qube is skipped with a note.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::config::{GlobalConfig, OsType, Qube, QubeKind, NO_NETVM};
use crate::error::{Error, Result};
use crate::pci::{self, PciAddr, PciDevice};
use crate::sys::Runner;
use crate::template::{self, Recipe};

pub const LOADER_CONF: &str = "/boot/loader.conf";
pub const SYS_NET: &str = "sys-net";
pub const SYS_FIREWALL: &str = "sys-firewall";
pub const GPU: &str = "gpu";
/// Template of `sys-net` (and of a FreeBSD `sys-firewall`).
pub const NET_TEMPLATE: &str = "fbsd15-net";
/// Template of the `gpu` qube (CUDA wants Linux).
pub const GPU_TEMPLATE: &str = "debian-13-dev";
pub const GPU_CPUS: u32 = 4;
pub const GPU_MEMORY_MB: u64 = 8192;
/// Default template of the app qubes.
pub const APP_TEMPLATE: &str = "fbsd15-dev";
pub const APP_CPUS: u32 = 2;
pub const APP_MEMORY_MB: u64 = 2048;
/// What [`Step::WaitForNetwork`] runs in `sys-net` until it succeeds.
pub const NETWORK_PROBE: &str = "fetch -q -o /dev/null -T 5 https://download.freebsd.org/";
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(180);
pub const NETWORK_INTERVAL: Duration = Duration::from_secs(5);

/// What the host has, probed by [`probe_host`].
#[derive(Debug, Clone, Default)]
pub struct Host {
    /// `pciconf -l -v`.
    pub devices: Vec<PciDevice>,
    /// Interface of the dom0's default route ([`default_route_iface`]):
    /// the dom0 itself can download.
    pub uplink: Option<String>,
    /// `hw.vmm.iommu.enable=1`: PCI passthrough works.
    pub iommu: bool,
    /// `pptdevs` in `/boot/loader.conf` ([`loader_pptdevs`]).
    pub pptdevs: Vec<PciAddr>,
    /// [`crate::bhyve::max_vcpus`].
    pub max_vcpus: Option<u32>,
    /// Recipes whose image is in the dom0's cache
    /// ([`template::local_images`]).
    pub local_templates: Vec<String>,
}

impl Host {
    /// The image of `recipe` can be had: cached, or the dom0 can download.
    pub fn can_fetch(&self, recipe: &str) -> bool {
        self.uplink.is_some() || self.local_templates.iter().any(|t| t == recipe)
    }
}

/// An app qube to create (`jos setup --app NAME[:offline]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSpec {
    pub name: String,
    /// Behind `sys-firewall`; else `netvm = "none"`.
    pub network: bool,
}

impl AppSpec {
    pub fn new(name: &str, network: bool) -> Self {
        AppSpec {
            name: name.to_string(),
            network,
        }
    }

    /// A valid qube name that is neither a system qube nor a template.
    pub fn check(&self) -> Result<()> {
        crate::config::validate_name(&self.name)?;
        if [SYS_NET, SYS_FIREWALL, GPU].contains(&self.name.as_str())
            || template::find(&self.name).is_some()
        {
            return Err(Error::Other(format!(
                "{} is the name of a system qube or a template, not an app qube",
                self.name
            )));
        }
        Ok(())
    }
}

impl FromStr for AppSpec {
    type Err = Error;

    /// `work` or `vault:offline`.
    fn from_str(s: &str) -> Result<Self> {
        let app = match s.split_once(':') {
            None => AppSpec::new(s, true),
            Some((name, "offline")) => AppSpec::new(name, false),
            Some(_) => {
                return Err(Error::Other(format!(
                    "{s:?}: expected NAME or NAME:offline"
                )))
            }
        };
        app.check()?;
        Ok(app)
    }
}

impl fmt::Display for AppSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if !self.network {
            f.write_str(":offline")?;
        }
        Ok(())
    }
}

/// The app qubes QubesOS creates: three behind the firewall, one offline.
pub fn default_apps() -> Vec<AppSpec> {
    vec![
        AppSpec::new("work", true),
        AppSpec::new("personal", true),
        AppSpec::new("untrusted", true),
        AppSpec::new("vault", false),
    ]
}

/// The operator's choices (`jos setup` flags, or the first-boot wizard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOptions {
    /// The network card's interface (default: [`Host::uplink`]): the
    /// provisioning boots and a bridged `sys-net` are bridged to it.
    pub uplink: Option<String>,
    /// Network card for `sys-net` (default: the uplink's).
    pub nic: Option<PciAddr>,
    /// Pass the uplink's PCI card through to `sys-net` when the IOMMU
    /// allows; else bridge `sys-net` to it (no reboot needed).
    pub passthrough: bool,
    /// Create the `gpu` qube when an NVIDIA card is reserved.
    pub gpu: bool,
    /// vCPUs of the `gpu` qube, chosen by the operator; else [`GPU_CPUS`]
    /// capped to what the host runs.
    pub gpu_cpus: Option<u32>,
    pub gpu_memory_mb: u64,
    /// Unikernel image for `sys-firewall`, when installed.
    pub firewall_image: Option<String>,
    /// App qubes to create.
    pub apps: Vec<AppSpec>,
    /// Recipe the app qubes clone from.
    pub app_template: String,
}

impl Default for SetupOptions {
    fn default() -> Self {
        SetupOptions {
            uplink: None,
            nic: None,
            passthrough: true,
            gpu: true,
            gpu_cpus: None,
            gpu_memory_mb: GPU_MEMORY_MB,
            firewall_image: None,
            apps: Vec::new(),
            app_template: APP_TEMPLATE.to_string(),
        }
    }
}

/// How a template's provisioning boot reaches the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Via {
    /// Bridged to this host interface (`jos template install --uplink`).
    Uplink(String),
    /// Through the running `sys-net` (`netvm = "sys-net"`).
    Chain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `jos template install <recipe>`.
    InstallTemplate { recipe: &'static str, via: Via },
    /// `jos create` of this qube.
    Create(Box<Qube>),
    /// `jos start` (nothing when it runs already).
    Start(String),
    /// Run [`NETWORK_PROBE`] in the qube until it succeeds.
    WaitForNetwork(String),
    /// Write this whole list as `pptdevs` in `/boot/loader.conf`.
    ReservePci(Vec<PciAddr>),
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::InstallTemplate {
                recipe,
                via: Via::Uplink(iface),
            } => write!(
                f,
                "install template {recipe} (provisioned bridged to {iface})"
            ),
            Step::InstallTemplate {
                recipe,
                via: Via::Chain,
            } => write!(
                f,
                "install template {recipe} (provisioned through {SYS_NET})"
            ),
            Step::Create(q) => {
                let mut s = format!("create {} ({} qube, {}", q.name, q.kind, q.os);
                if let Some(t) = &q.template {
                    s.push_str(&format!(", from {t}"));
                }
                if let Some(k) = &q.kernel {
                    s.push_str(&format!(", booting {k}"));
                }
                if !q.pci_passthrough.is_empty() {
                    s.push_str(&format!(", PCI {}", pci::addr_list(&q.pci_passthrough)));
                }
                if let Some(u) = &q.uplink {
                    s.push_str(&format!(", bridged to {u}"));
                }
                if q.netvm.as_deref() == Some(NO_NETVM) {
                    s.push_str(", offline");
                }
                if q.autostart {
                    s.push_str(", autostart");
                }
                s.push(')');
                f.write_str(&s)
            }
            Step::Start(name) => write!(f, "start {name}"),
            Step::WaitForNetwork(name) => write!(f, "wait for {name} to reach the network"),
            Step::ReservePci(addrs) => write!(
                f,
                "reserve devices for qubes: {} in {LOADER_CONF}",
                pci::pptdevs_line(addrs)
            ),
        }
    }
}

/// The line `jos setup` prints before running step `i` (1-based) of `n`;
/// the first-boot wizard follows the progress with [`parse_progress`].
pub fn progress_line(i: usize, n: usize, step: &Step) -> String {
    format!("==> [{i}/{n}] {step}")
}

/// `(i, n, step)` of a [`progress_line`].
pub fn parse_progress(line: &str) -> Option<(usize, usize, &str)> {
    let rest = line.strip_prefix("==> [")?;
    let (counts, step) = rest.split_once("] ")?;
    let (i, n) = counts.split_once('/')?;
    Some((i.parse().ok()?, n.parse().ok()?, step))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// What is skipped and why.
    pub notes: Vec<String>,
    /// Some system qube's device is still driven by the dom0: it only goes
    /// to the qube after a reboot.
    pub reboot: bool,
}

/// Why a template cannot be built.
fn unobtainable(recipe: &str) -> String {
    format!(
        "the dom0 has no network of its own and no local image of {recipe} (copy it to a USB \
         stick, run `jos template install {recipe} --image FILE --checksums FILE`, then \
         `jos setup` again)"
    )
}

/// The steps still missing for the system qubes, `gpu` and the apps.
pub fn plan(host: &Host, existing: &[Qube], opts: &SetupOptions) -> Result<Plan> {
    let has = |name: &str| existing.iter().any(|q| q.name == name);
    let uplink = opts.uplink.clone().or_else(|| host.uplink.clone());
    let mut notes = Vec::new();
    let mut system = Vec::new();

    if has(SYS_NET) {
        notes.push(format!("{SYS_NET} exists"));
    } else {
        system.push(sys_net(host, uplink.as_deref(), opts)?);
    }
    if has(SYS_FIREWALL) {
        notes.push(format!("{SYS_FIREWALL} exists"));
    } else {
        system.push(sys_firewall(opts.firewall_image.as_deref()));
    }
    let mut later: Vec<Qube> = gpu_plan(host, &has, opts, &mut notes).into_iter().collect();
    later.extend(app_qubes(host, &has, opts, &mut notes)?);
    for q in system.iter_mut().chain(later.iter_mut()) {
        fit(q, host, opts, &mut notes)?;
    }

    // The system qubes' template first, bridged to the card: nothing
    // else can reach the network yet.
    let system_recipes = missing_recipes(&system, &has, &[]);
    let mut steps = Vec::new();
    if !system_recipes.is_empty() {
        let names: Vec<&str> = system_recipes.iter().map(|r| r.name).collect();
        let Some(iface) = &uplink else {
            return Err(Error::Other(format!(
                "building {} needs the network card {SYS_NET} will use: pass --uplink IFACE \
                 (bridged; the dom0 gets no address)",
                names.join(", ")
            )));
        };
        if let Some(r) = system_recipes.iter().find(|r| !host.can_fetch(r.name)) {
            return Err(Error::Other(format!(
                "{SYS_NET} needs template {}, but {}",
                r.name,
                unobtainable(r.name)
            )));
        }
        steps.extend(system_recipes.iter().map(|r| Step::InstallTemplate {
            recipe: r.name,
            via: Via::Uplink(iface.clone()),
        }));
    }

    // sys-net (new or defined) can run now: bridged, or its card bound
    // to ppt already.
    let chain = system
        .iter()
        .chain(existing)
        .find(|q| q.name == SYS_NET)
        .is_some_and(|q| {
            q.pci_passthrough.iter().all(|a| {
                host.devices
                    .iter()
                    .any(|d| d.addr == *a && d.is_passthrough_ready())
            })
        });
    let later_recipes = missing_recipes(&later, &has, &system_recipes);
    let mut later_steps = Vec::new();
    let mut started = false;
    if !later_recipes.is_empty() {
        let via = match chain {
            true => Some(Via::Chain),
            false => uplink.clone().map(Via::Uplink),
        };
        match via {
            Some(via) => {
                if via == Via::Chain {
                    later_steps.push(Step::Start(SYS_NET.to_string()));
                    later_steps.push(Step::WaitForNetwork(SYS_NET.to_string()));
                    started = true;
                }
                later_steps.extend(later_recipes.iter().map(|r| Step::InstallTemplate {
                    recipe: r.name,
                    via: via.clone(),
                }));
            }
            None => {
                let waits = |q: &Qube| {
                    later_recipes
                        .iter()
                        .any(|r| q.template.as_deref() == Some(r.name))
                };
                let names: Vec<String> = later
                    .iter()
                    .filter(|q| waits(q))
                    .map(|q| q.name.clone())
                    .collect();
                later.retain(|q| !waits(q));
                notes.push(format!(
                    "not yet {}: their templates are built through {SYS_NET}, which gets its \
                     card at the next reboot; run `jos setup` again then (or pass --uplink IFACE)",
                    names.join(", ")
                ));
            }
        }
    }

    // Devices of the system qubes (existing or new) the dom0 still drives.
    let managed = [SYS_NET, SYS_FIREWALL, GPU];
    let mut unbound: Vec<PciAddr> = existing
        .iter()
        .filter(|q| managed.contains(&q.name.as_str()))
        .chain(system.iter())
        .chain(later.iter())
        .flat_map(|q| q.pci_passthrough.iter().copied())
        .filter(|a| {
            host.devices
                .iter()
                .any(|d| d.addr == *a && !d.is_passthrough_ready())
        })
        .collect();
    let reboot = !unbound.is_empty();
    unbound.retain(|a| !host.pptdevs.contains(a));

    steps.extend(system.into_iter().map(|q| Step::Create(Box::new(q))));
    steps.extend(later_steps);
    steps.extend(later.into_iter().map(|q| Step::Create(Box::new(q))));
    if !unbound.is_empty() {
        let mut all = host.pptdevs.clone();
        all.extend(unbound);
        all.sort();
        all.dedup();
        steps.push(Step::ReservePci(all));
    }
    // Nothing waits for a reboot: bring the network up now, sys-firewall
    // last so it knows every client defined above.
    if chain && !reboot && !steps.is_empty() {
        if !started {
            steps.push(Step::Start(SYS_NET.to_string()));
        }
        steps.push(Step::Start(SYS_FIREWALL.to_string()));
    }
    Ok(Plan {
        steps,
        notes,
        reboot,
    })
}

/// vCPUs of a new qube: the operator's choice for `gpu` must fit, the
/// defaults are capped.
fn fit(q: &mut Qube, host: &Host, opts: &SetupOptions, notes: &mut Vec<String>) -> Result<()> {
    if q.name == GPU && opts.gpu_cpus.is_some() {
        return crate::bhyve::check_vcpus(q, host.max_vcpus);
    }
    let (cpus, note) = crate::bhyve::fit_vcpus(&q.name, q.cpus, host.max_vcpus);
    q.cpus = cpus;
    notes.extend(note);
    Ok(())
}

/// The recipes `qubes` clone from that are neither defined nor in `skip`.
fn missing_recipes(
    qubes: &[Qube],
    has: &dyn Fn(&str) -> bool,
    skip: &[&'static Recipe],
) -> Vec<&'static Recipe> {
    let mut out: Vec<&'static Recipe> = Vec::new();
    for t in qubes.iter().filter_map(|q| q.template.as_deref()) {
        if has(t) || skip.iter().chain(out.iter()).any(|r| r.name == t) {
            continue;
        }
        // Every template the plan clones from has a recipe (tested).
        out.extend(template::find(t));
    }
    out
}

/// A qube cloned from a recipe's template, sized like the recipe.
fn clone_of(name: &str, kind: QubeKind, template_name: &str) -> Qube {
    let recipe = template::find(template_name);
    let mut q = Qube::new(name, kind, recipe.map_or(OsType::Freebsd, |r| r.os));
    q.template = Some(template_name.to_string());
    q.cpus = recipe.map_or(q.cpus, |r| r.cpus);
    q.memory_mb = recipe.map_or(q.memory_mb, |r| r.memory_mb);
    q
}

fn sys_net(host: &Host, uplink: Option<&str>, opts: &SetupOptions) -> Result<Qube> {
    let mut q = clone_of(SYS_NET, QubeKind::Net, NET_TEMPLATE);
    q.autostart = true;
    let card = match opts.nic {
        Some(addr) => {
            if !host.iommu {
                return Err(Error::Other(format!(
                    "--nic {addr} needs PCI passthrough, but hw.vmm.iommu.enable is not 1 \
                     (see `jos doctor`)"
                )));
            }
            let dev = host
                .devices
                .iter()
                .find(|d| d.addr == addr)
                .ok_or_else(|| {
                    Error::Other(format!("no PCI device at {addr} (see `jos pci list`)"))
                })?;
            if !dev.is_nic() {
                return Err(Error::Other(format!(
                    "{addr} ({}) is not a network controller",
                    dev.label()
                )));
            }
            Some(addr)
        }
        // The card behind the uplink (its driver unit is the interface
        // name), else one already reserved at install time.
        None if host.iommu && opts.passthrough => uplink
            .and_then(|u| host.devices.iter().find(|d| d.is_nic() && d.driver == u))
            .or_else(|| {
                host.devices
                    .iter()
                    .find(|d| d.is_nic() && d.is_passthrough_ready())
            })
            .map(|d| d.addr),
        None => None,
    };
    match (card, uplink) {
        (Some(addr), _) => q.pci_passthrough = vec![addr],
        (None, Some(iface)) => q.uplink = Some(iface.to_string()),
        (None, None) => {
            return Err(Error::Other(format!(
                "{SYS_NET} needs a network card: pass --nic B/S/F or --uplink IFACE"
            )))
        }
    }
    Ok(q)
}

fn sys_firewall(image: Option<&str>) -> Qube {
    let mut q = match image {
        Some(img) => {
            let mut q = Qube::new(SYS_FIREWALL, QubeKind::Firewall, OsType::Unikernel);
            q.kernel = Some(img.to_string());
            q
        }
        None => clone_of(SYS_FIREWALL, QubeKind::Firewall, NET_TEMPLATE),
    };
    q.netvm = Some(SYS_NET.to_string());
    q.autostart = true;
    q
}

/// The `gpu` qube, when wanted, missing, possible and its template can be
/// had.
fn gpu_plan(
    host: &Host,
    has: &dyn Fn(&str) -> bool,
    opts: &SetupOptions,
    notes: &mut Vec<String>,
) -> Option<Qube> {
    if !opts.gpu {
        notes.push(format!("no {GPU} qube (--no-gpu)"));
        return None;
    }
    if has(GPU) {
        notes.push(format!("{GPU} exists"));
        return None;
    }
    let addrs = gpu_devices(&host.devices);
    if addrs.is_empty() {
        notes.push(format!(
            "no {GPU} qube: no NVIDIA card is reserved (add it to pptdevs in {LOADER_CONF}, \
             reboot and run `jos setup` again)"
        ));
        return None;
    }
    if !has(GPU_TEMPLATE) && !host.can_fetch(GPU_TEMPLATE) {
        notes.push(format!("no {GPU} qube yet: {}", unobtainable(GPU_TEMPLATE)));
        return None;
    }
    let mut q = clone_of(GPU, QubeKind::Gpu, GPU_TEMPLATE);
    q.cpus = opts.gpu_cpus.unwrap_or(GPU_CPUS);
    q.memory_mb = opts.gpu_memory_mb;
    q.pci_passthrough = addrs;
    Some(q)
}

/// The app qubes still missing.
fn app_qubes(
    host: &Host,
    has: &dyn Fn(&str) -> bool,
    opts: &SetupOptions,
    notes: &mut Vec<String>,
) -> Result<Vec<Qube>> {
    if opts.apps.is_empty() {
        return Ok(Vec::new());
    }
    let recipe = template::find(&opts.app_template).ok_or_else(|| {
        Error::Other(format!(
            "unknown template recipe {:?} for the app qubes (see `jos template recipes`)",
            opts.app_template
        ))
    })?;
    for (i, app) in opts.apps.iter().enumerate() {
        app.check()?;
        if opts.apps.iter().take(i).any(|a| a.name == app.name) {
            return Err(Error::Other(format!(
                "app qube {} is listed twice",
                app.name
            )));
        }
    }
    if !has(recipe.name) && !host.can_fetch(recipe.name) {
        let names: Vec<&str> = opts.apps.iter().map(|a| a.name.as_str()).collect();
        notes.push(format!(
            "no app qubes yet ({}): {}",
            names.join(", "),
            unobtainable(recipe.name)
        ));
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for app in &opts.apps {
        if has(&app.name) {
            notes.push(format!("{} exists", app.name));
            continue;
        }
        let mut q = clone_of(&app.name, QubeKind::App, recipe.name);
        q.cpus = APP_CPUS;
        q.memory_mb = APP_MEMORY_MB;
        if !app.network {
            q.netvm = Some(NO_NETVM.to_string());
        }
        out.push(q);
    }
    Ok(out)
}

/// The first NVIDIA card bound to `ppt` and every function in its slot
/// (HDMI audio, USB-C...), which has to follow it into the qube. Empty
/// when no NVIDIA card is reserved.
pub fn gpu_devices(devices: &[PciDevice]) -> Vec<PciAddr> {
    let Some(gpu) = devices
        .iter()
        .find(|d| d.is_gpu() && d.is_nvidia() && d.is_passthrough_ready())
    else {
        return Vec::new();
    };
    let mut out: Vec<PciAddr> = devices
        .iter()
        .filter(|d| d.same_slot(gpu))
        .map(|d| d.addr)
        .collect();
    out.sort();
    out
}

/// `interface:` of `route -n get default`.
pub fn parse_route_iface(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("interface:"))
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
}

/// The interface of the dom0's default route, if it has one.
pub fn default_route_iface(runner: &dyn Runner) -> Option<String> {
    match runner.run("route", &["-n", "get", "default"]) {
        Ok(out) if out.success() => parse_route_iface(&out.stdout),
        _ => None,
    }
}

/// Parse a `pptdevs` value (`1/0/0 1/0/1`).
pub fn parse_pptdevs(text: &str) -> Result<Vec<PciAddr>> {
    text.split_whitespace().map(str::parse).collect()
}

/// `pptdevs` from `/boot/loader.conf`; empty when unset.
pub fn loader_pptdevs(runner: &dyn Runner) -> Result<Vec<PciAddr>> {
    match runner.run("sysrc", &["-f", LOADER_CONF, "-n", "pptdevs"]) {
        Ok(out) if out.success() => parse_pptdevs(&out.stdout),
        _ => Ok(Vec::new()),
    }
}

/// Write `pptdevs` to `/boot/loader.conf` (effective at the next boot).
pub fn reserve(runner: &dyn Runner, addrs: &[PciAddr]) -> Result<()> {
    let value = format!("pptdevs={}", pci::addr_list(addrs));
    runner.run_checked("sysrc", &["-f", LOADER_CONF, &value])?;
    Ok(())
}

/// [`Host`] with these PCI devices (the CLI plans without them in a dry
/// run on a machine without `pciconf`).
pub fn probe_host_with(
    runner: &dyn Runner,
    cfg: &GlobalConfig,
    devices: Vec<PciDevice>,
) -> Result<Host> {
    Ok(Host {
        devices,
        uplink: default_route_iface(runner),
        iommu: crate::doctor::iommu_enabled(runner),
        pptdevs: loader_pptdevs(runner)?,
        max_vcpus: crate::bhyve::max_vcpus(runner),
        local_templates: template::local_images(cfg),
    })
}

/// Everything [`plan`] needs to know about this host.
pub fn probe_host(runner: &dyn Runner, cfg: &GlobalConfig) -> Result<Host> {
    probe_host_with(runner, cfg, pci::list_devices(runner)?)
}

/// Run `probe` until it succeeds, sleeping `interval` between tries, for
/// at most `timeout`. False when it never did.
pub fn wait_until(
    timeout: Duration,
    interval: Duration,
    probe: &mut dyn FnMut() -> bool,
    sleep: &mut dyn FnMut(Duration),
) -> bool {
    let mut waited = Duration::ZERO;
    loop {
        if probe() {
            return true;
        }
        if waited >= timeout {
            return false;
        }
        sleep(interval);
        waited += interval;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{CmdOutput, MockRunner};

    #[test]
    fn every_template_the_plan_uses_has_a_recipe() {
        for t in [NET_TEMPLATE, GPU_TEMPLATE, APP_TEMPLATE] {
            assert!(template::find(t).is_some(), "{t}");
        }
        let q = clone_of("x", QubeKind::App, "no-such-recipe");
        assert_eq!(q.os, OsType::Freebsd);
        assert_eq!(q.cpus, Qube::new("x", QubeKind::App, OsType::Freebsd).cpus);
    }

    fn dev(addr: &str, driver: &str, vendor: u16, class: u32) -> PciDevice {
        PciDevice {
            addr: addr.parse().unwrap(),
            driver: driver.into(),
            vendor,
            device: 0x1234,
            class,
            description: None,
        }
    }

    /// Intel iGPU driving the dom0, an NVIDIA card reserved at install
    /// time (GPU and audio), the wired NIC behind the default route.
    fn host() -> Host {
        Host {
            devices: vec![
                dev("0/2/0", "vgapci0", 0x8086, 0x030000),
                dev("1/0/0", "ppt0", pci::NVIDIA_VENDOR, 0x030000),
                dev("1/0/1", "ppt1", pci::NVIDIA_VENDOR, 0x040300),
                dev("5/0/0", "em0", 0x8086, 0x020000),
                dev("6/0/0", "iwlwifi0", 0x8086, 0x028000),
            ],
            uplink: Some("em0".into()),
            iommu: true,
            pptdevs: vec!["1/0/0".parse().unwrap(), "1/0/1".parse().unwrap()],
            max_vcpus: Some(16),
            local_templates: Vec::new(),
        }
    }

    /// A laptop without IOMMU and network card of its own: a USB adapter
    /// (`ue0`), no route on the dom0, the FreeBSD image on the install
    /// media.
    fn usb_host() -> Host {
        Host {
            devices: vec![dev("0/2/0", "vgapci0", 0x8086, 0x030000)],
            uplink: None,
            iommu: false,
            pptdevs: Vec::new(),
            max_vcpus: Some(4),
            local_templates: vec![NET_TEMPLATE.into(), APP_TEMPLATE.into()],
        }
    }

    fn usb_opts() -> SetupOptions {
        SetupOptions {
            uplink: Some("ue0".into()),
            apps: default_apps(),
            ..Default::default()
        }
    }

    fn addr(s: &str) -> PciAddr {
        s.parse().unwrap()
    }

    fn created(p: &Plan) -> Vec<Qube> {
        p.steps
            .iter()
            .filter_map(|s| match s {
                Step::Create(q) => Some((**q).clone()),
                _ => None,
            })
            .collect()
    }

    fn get<'a>(qubes: &'a [Qube], name: &str) -> &'a Qube {
        qubes.iter().find(|q| q.name == name).unwrap()
    }

    fn install(recipe: &'static str, via: Via) -> Step {
        Step::InstallTemplate { recipe, via }
    }

    fn em0() -> Via {
        Via::Uplink("em0".into())
    }

    fn text(p: &Plan) -> Vec<String> {
        p.steps.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fresh_install_plans_everything_and_reserves_the_nic_last() {
        let p = plan(&host(), &[], &SetupOptions::default()).unwrap();
        // sys-net's card is still the dom0's: everything is provisioned
        // bridged to it, and nothing starts before the reboot.
        assert_eq!(p.steps.first(), Some(&install(NET_TEMPLATE, em0())));
        assert_eq!(p.steps.get(3), Some(&install(GPU_TEMPLATE, em0())));
        assert_eq!(
            p.steps.last(),
            Some(&Step::ReservePci(vec![
                addr("1/0/0"),
                addr("1/0/1"),
                addr("5/0/0")
            ]))
        );
        assert!(!p.steps.iter().any(|s| matches!(s, Step::Start(_))));
        assert!(p.reboot);
        let qubes = created(&p);
        let names: Vec<&str> = qubes.iter().map(|q| q.name.as_str()).collect();
        assert_eq!(names, [SYS_NET, SYS_FIREWALL, GPU]);

        let net = get(&qubes, SYS_NET);
        assert_eq!(net.kind, QubeKind::Net);
        assert_eq!(net.template.as_deref(), Some(NET_TEMPLATE));
        assert_eq!(net.pci_passthrough, [addr("5/0/0")]);
        assert_eq!(net.uplink, None);
        assert!(net.autostart);
        let recipe = template::find(NET_TEMPLATE).unwrap();
        assert_eq!((net.cpus, net.memory_mb), (recipe.cpus, recipe.memory_mb));

        let fw = get(&qubes, SYS_FIREWALL);
        assert_eq!(fw.os, OsType::Freebsd);
        assert_eq!(fw.template.as_deref(), Some(NET_TEMPLATE));
        assert_eq!(fw.netvm.as_deref(), Some(SYS_NET));
        assert!(fw.autostart);

        let gpu = get(&qubes, GPU);
        assert_eq!((gpu.kind, gpu.os), (QubeKind::Gpu, OsType::Linux));
        assert_eq!(gpu.pci_passthrough, [addr("1/0/0"), addr("1/0/1")]);
        assert_eq!((gpu.cpus, gpu.memory_mb), (GPU_CPUS, GPU_MEMORY_MB));
        assert!(!gpu.autostart);
        assert!(p.notes.is_empty(), "{:?}", p.notes);
    }

    #[test]
    fn a_usb_adapter_bridges_sys_net_and_the_rest_goes_through_it() {
        let p = plan(&usb_host(), &[], &usb_opts()).unwrap();
        assert_eq!(
            text(&p),
            [
                "install template fbsd15-net (provisioned bridged to ue0)",
                "create sys-net (net qube, freebsd, from fbsd15-net, bridged to ue0, autostart)",
                "create sys-firewall (firewall qube, freebsd, from fbsd15-net, autostart)",
                "start sys-net",
                "wait for sys-net to reach the network",
                "install template fbsd15-dev (provisioned through sys-net)",
                "create work (app qube, freebsd, from fbsd15-dev)",
                "create personal (app qube, freebsd, from fbsd15-dev)",
                "create untrusted (app qube, freebsd, from fbsd15-dev)",
                "create vault (app qube, freebsd, from fbsd15-dev, offline)",
                "start sys-firewall",
            ]
        );
        assert!(!p.reboot);
        let qubes = created(&p);
        let work = get(&qubes, "work");
        assert_eq!((work.cpus, work.memory_mb), (APP_CPUS, APP_MEMORY_MB));
        assert_eq!(work.netvm, None, "the default NetVM");
        assert_eq!(get(&qubes, "vault").netvm.as_deref(), Some(NO_NETVM));
        assert!(p.notes.iter().any(|n| n.contains("no NVIDIA card")));

        // Done: nothing to do, nothing started again.
        let mut existing = qubes;
        for t in [NET_TEMPLATE, APP_TEMPLATE] {
            existing.push(Qube::new(t, QubeKind::Template, OsType::Freebsd));
        }
        let again = plan(&usb_host(), &existing, &usb_opts()).unwrap();
        assert!(again.steps.is_empty(), "{:?}", again.steps);
        assert!(again.notes.iter().any(|n| n == "vault exists"));

        // Stopped after the first template: the rest resumes, sys-net is
        // started once.
        existing.retain(|q| q.name != "untrusted" && q.name != SYS_FIREWALL);
        let p = plan(&usb_host(), &existing, &usb_opts()).unwrap();
        assert_eq!(
            text(&p),
            [
                "create sys-firewall (firewall qube, freebsd, from fbsd15-net, autostart)",
                "create untrusted (app qube, freebsd, from fbsd15-dev)",
                "start sys-net",
                "start sys-firewall",
            ]
        );
    }

    #[test]
    fn without_a_dom0_route_or_a_local_image_nothing_is_downloaded() {
        let bare = Host {
            local_templates: Vec::new(),
            ..usb_host()
        };
        let err = plan(&bare, &[], &usb_opts()).unwrap_err().to_string();
        assert!(err.contains("no local image of fbsd15-net"), "{err}");
        assert!(err.contains("--image FILE --checksums FILE"), "{err}");

        // Only the app template is missing: the apps wait, the rest goes.
        let net_only = Host {
            local_templates: vec![NET_TEMPLATE.into()],
            ..usb_host()
        };
        let p = plan(&net_only, &[], &usb_opts()).unwrap();
        let names: Vec<String> = created(&p).into_iter().map(|q| q.name).collect();
        assert_eq!(names, [SYS_NET, SYS_FIREWALL]);
        assert!(
            p.notes
                .iter()
                .any(|n| n.starts_with("no app qubes yet (work, personal, untrusted, vault)")),
            "{:?}",
            p.notes
        );
        // Neither for the gpu qube.
        let mut gpu_host = net_only;
        gpu_host.devices = host().devices;
        gpu_host.devices.retain(|d| d.addr.bus == 1);
        let ue0 = SetupOptions {
            uplink: Some("ue0".into()),
            ..Default::default()
        };
        let p = plan(&gpu_host, &[], &ue0).unwrap();
        assert!(created(&p).iter().all(|q| q.name != GPU));
        assert!(p
            .notes
            .iter()
            .any(|n| n.starts_with("no gpu qube yet: the dom0 has no network")));
    }

    #[test]
    fn app_qubes_are_checked() {
        let opts = |apps: &[&str], template: &str| SetupOptions {
            apps: apps.iter().map(|a| AppSpec::new(a, true)).collect(),
            app_template: template.into(),
            ..usb_opts()
        };
        let fails = |o: SetupOptions, why: &str| {
            let err = plan(&usb_host(), &[], &o).unwrap_err().to_string();
            assert!(err.contains(why), "{err}");
        };
        fails(opts(&["work"], "nope"), "unknown template recipe \"nope\"");
        fails(
            opts(&["sys-net"], APP_TEMPLATE),
            "system qube or a template",
        );
        fails(
            opts(&["fbsd15-dev"], APP_TEMPLATE),
            "system qube or a template",
        );
        fails(opts(&["Bad"], APP_TEMPLATE), "Bad");
        fails(opts(&["work", "work"], APP_TEMPLATE), "listed twice");

        // A Linux app template, built through sys-net like the others.
        let mut h = usb_host();
        h.local_templates.push(GPU_TEMPLATE.into());
        let p = plan(&h, &[], &opts(&["work"], GPU_TEMPLATE)).unwrap();
        assert!(p.steps.contains(&install(GPU_TEMPLATE, Via::Chain)));
        assert_eq!(get(&created(&p), "work").os, OsType::Linux);
    }

    #[test]
    fn app_specs_round_trip() {
        for s in ["work", "vault:offline"] {
            assert_eq!(s.parse::<AppSpec>().unwrap().to_string(), s);
        }
        assert!(!"vault:offline".parse::<AppSpec>().unwrap().network);
        assert!("work:online".parse::<AppSpec>().is_err());
        assert!("gpu".parse::<AppSpec>().is_err());
        assert!(":offline".parse::<AppSpec>().is_err());
        let names: Vec<String> = default_apps().iter().map(|a| a.to_string()).collect();
        assert_eq!(names, ["work", "personal", "untrusted", "vault:offline"]);
    }

    #[test]
    fn a_reserved_card_lets_sys_net_build_the_others_after_the_reboot() {
        // A first run with the card still in the dom0, without the gpu:
        // sys-net, sys-firewall and pptdevs.
        let opts = SetupOptions {
            gpu: false,
            apps: vec![AppSpec::new("work", true)],
            ..Default::default()
        };
        let mut h = host();
        h.local_templates = vec![NET_TEMPLATE.into()];
        h.uplink = None;
        let p = plan(
            &h,
            &[],
            &SetupOptions {
                uplink: Some("em0".into()),
                ..opts.clone()
            },
        )
        .unwrap();
        assert!(p.reboot);
        assert_eq!(p.steps.first(), Some(&install(NET_TEMPLATE, em0())));
        let mut existing = created(&p);
        existing.retain(|q| q.name != "work");
        existing.push(Qube::new(NET_TEMPLATE, QubeKind::Template, OsType::Freebsd));

        // After the reboot the card is sys-net's and the dom0 has no
        // interface left: the app template goes through sys-net...
        h.devices
            .iter_mut()
            .find(|d| d.addr == addr("5/0/0"))
            .unwrap()
            .driver = "ppt2".into();
        h.pptdevs.push(addr("5/0/0"));
        h.local_templates.push(APP_TEMPLATE.into());
        let p = plan(&h, &existing, &opts).unwrap();
        assert!(!p.reboot);
        assert_eq!(
            text(&p),
            [
                "start sys-net",
                "wait for sys-net to reach the network",
                "install template fbsd15-dev (provisioned through sys-net)",
                "create work (app qube, freebsd, from fbsd15-dev)",
                "start sys-firewall",
            ]
        );
        // ...which a run before the reboot cannot do: the apps wait.
        h.devices
            .iter_mut()
            .find(|d| d.addr == addr("5/0/0"))
            .unwrap()
            .driver = "em0".into();
        let p = plan(&h, &existing, &opts).unwrap();
        assert!(p.steps.is_empty(), "{:?}", p.steps);
        assert!(p.reboot);
        assert!(
            p.notes.iter().any(|n| n.starts_with("not yet work:")),
            "{:?}",
            p.notes
        );
    }

    #[test]
    fn a_nic_reserved_at_install_time_goes_to_sys_net() {
        let mut h = host();
        h.uplink = None;
        h.devices
            .iter_mut()
            .find(|d| d.addr == addr("5/0/0"))
            .unwrap()
            .driver = "ppt2".into();
        h.pptdevs.push(addr("5/0/0"));
        // Nothing to provision sys-net's template through...
        let err = plan(&h, &[], &SetupOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("pass --uplink IFACE"), "{err}");
        assert!(err.contains(NET_TEMPLATE), "{err}");
        // ...unless it is installed already; the gpu template is then
        // built through sys-net, when its image is local.
        let templates = [Qube::new(NET_TEMPLATE, QubeKind::Template, OsType::Freebsd)];
        let p = plan(&h, &templates, &SetupOptions::default()).unwrap();
        assert!(!created(&p).iter().any(|q| q.name == GPU));
        h.local_templates.push(GPU_TEMPLATE.into());
        let p = plan(&h, &templates, &SetupOptions::default()).unwrap();
        assert_eq!(get(&created(&p), SYS_NET).pci_passthrough, [addr("5/0/0")]);
        assert!(!p.reboot, "everything is bound already");
        assert!(p.steps.contains(&install(GPU_TEMPLATE, Via::Chain)));
        assert!(!p.steps.iter().any(|s| matches!(s, Step::ReservePci(_))));
        assert_eq!(p.steps.last(), Some(&Step::Start(SYS_FIREWALL.into())));
    }

    #[test]
    fn unikernel_firewall_when_its_image_is_installed() {
        let opts = SetupOptions {
            firewall_image: Some("/fw.img".into()),
            gpu: false,
            ..Default::default()
        };
        let p = plan(&host(), &[], &opts).unwrap();
        let fw = get(&created(&p), SYS_FIREWALL).clone();
        assert_eq!(fw.os, OsType::Unikernel);
        assert_eq!(fw.kernel.as_deref(), Some("/fw.img"));
        assert_eq!(fw.template, None);
        assert!(p.notes.iter().any(|n| n.contains("--no-gpu")));
        // Only the template sys-net needs.
        let installs = p
            .steps
            .iter()
            .filter(|s| matches!(s, Step::InstallTemplate { .. }))
            .count();
        assert_eq!(installs, 1);
    }

    #[test]
    fn default_vcpus_are_capped_to_the_host_but_chosen_ones_are_not() {
        let small = Host {
            max_vcpus: Some(1),
            ..host()
        };
        let p = plan(&small, &[], &SetupOptions::default()).unwrap();
        assert!(created(&p).iter().all(|q| q.cpus == 1));
        assert!(p
            .notes
            .iter()
            .any(|n| n == "gpu: 4 vCPUs capped to 1, the most this host runs (hw.vmm.maxcpu)"));

        let opts = SetupOptions {
            gpu_cpus: Some(2),
            ..Default::default()
        };
        let e = plan(&small, &[], &opts).unwrap_err().to_string();
        assert!(
            e.contains("gpu has 2 vCPUs but this host runs at most 1"),
            "{e}"
        );
        let gpu = get(&created(&plan(&host(), &[], &opts).unwrap()), GPU).clone();
        assert_eq!(gpu.cpus, 2);

        let unknown = Host {
            max_vcpus: None,
            ..host()
        };
        let p = plan(&unknown, &[], &SetupOptions::default()).unwrap();
        assert_eq!(get(&created(&p), GPU).cpus, GPU_CPUS);
    }

    #[test]
    fn without_iommu_or_passthrough_sys_net_is_bridged_to_the_uplink() {
        let h = Host {
            iommu: false,
            ..host()
        };
        let p = plan(&h, &[], &SetupOptions::default()).unwrap();
        let net = get(&created(&p), SYS_NET).clone();
        assert!(net.pci_passthrough.is_empty());
        assert_eq!(net.uplink.as_deref(), Some("em0"));
        // The NVIDIA card is still reserved (and bound): no new pptdevs,
        // sys-net runs at once and builds the gpu template.
        assert!(!p.steps.iter().any(|s| matches!(s, Step::ReservePci(_))));
        assert!(p.steps.contains(&install(GPU_TEMPLATE, Via::Chain)));
        assert!(!p.reboot);

        let err = plan(
            &h,
            &[],
            &SetupOptions {
                nic: Some(addr("5/0/0")),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("iommu"), "{err}");

        // The IOMMU works but the operator keeps the card bridged.
        let opts = SetupOptions {
            passthrough: false,
            ..Default::default()
        };
        let p = plan(&host(), &[], &opts).unwrap();
        assert_eq!(get(&created(&p), SYS_NET).uplink.as_deref(), Some("em0"));
        assert!(!p.reboot);
    }

    #[test]
    fn explicit_nic_and_uplink() {
        let opts = SetupOptions {
            nic: Some(addr("6/0/0")),
            uplink: Some("ue0".into()),
            ..Default::default()
        };
        let p = plan(&host(), &[], &opts).unwrap();
        assert_eq!(get(&created(&p), SYS_NET).pci_passthrough, [addr("6/0/0")]);
        assert!(p
            .steps
            .contains(&install(NET_TEMPLATE, Via::Uplink("ue0".into()))));

        for (nic, why) in [("9/0/0", "no PCI device"), ("0/2/0", "not a network")] {
            let opts = SetupOptions {
                nic: Some(addr(nic)),
                ..Default::default()
            };
            let err = plan(&host(), &[], &opts).unwrap_err().to_string();
            assert!(err.contains(why), "{err}");
        }
    }

    #[test]
    fn no_card_at_all_is_an_error() {
        let h = Host {
            uplink: None,
            iommu: false,
            ..host()
        };
        let err = plan(&h, &[], &SetupOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("--nic B/S/F or --uplink IFACE"), "{err}");
    }

    #[test]
    fn rerun_skips_what_exists_but_still_reserves() {
        // A first run created everything, then failed writing pptdevs.
        let first = plan(&host(), &[], &SetupOptions::default()).unwrap();
        let mut existing = created(&first);
        existing.push(Qube::new(NET_TEMPLATE, QubeKind::Template, OsType::Freebsd));
        existing.push(Qube::new(GPU_TEMPLATE, QubeKind::Template, OsType::Linux));
        let p = plan(&host(), &existing, &SetupOptions::default()).unwrap();
        assert_eq!(
            p.steps,
            [Step::ReservePci(vec![
                addr("1/0/0"),
                addr("1/0/1"),
                addr("5/0/0")
            ])]
        );
        assert_eq!(p.notes.len(), 3);
        // After writing it, only the reboot is left.
        let h = Host {
            pptdevs: vec![addr("1/0/0"), addr("1/0/1"), addr("5/0/0")],
            ..host()
        };
        let p = plan(&h, &existing, &SetupOptions::default()).unwrap();
        assert!(p.steps.is_empty());
        assert!(p.reboot);
    }

    #[test]
    fn gpu_needs_a_reserved_nvidia_card_and_takes_its_slot() {
        let mut h = host();
        for d in &mut h.devices {
            if d.addr.bus == 1 {
                d.driver = "nvidia0".into();
            }
        }
        let p = plan(&h, &[], &SetupOptions::default()).unwrap();
        assert!(created(&p).iter().all(|q| q.name != GPU));
        assert!(p.notes.iter().any(|n| n.contains("no NVIDIA card")));

        // The card is reserved but its audio function is not: it follows
        // the card and gets reserved too.
        let mut devices = host().devices;
        devices
            .iter_mut()
            .find(|d| d.addr == addr("1/0/1"))
            .unwrap()
            .driver = "hdac1".into();
        assert_eq!(gpu_devices(&devices), [addr("1/0/0"), addr("1/0/1")]);
        let h = Host {
            devices,
            pptdevs: vec![addr("1/0/0")],
            ..host()
        };
        let p = plan(&h, &[], &SetupOptions::default()).unwrap();
        assert_eq!(
            p.steps.last(),
            Some(&Step::ReservePci(vec![
                addr("1/0/0"),
                addr("1/0/1"),
                addr("5/0/0")
            ]))
        );
        assert!(gpu_devices(&[]).is_empty());
    }

    #[test]
    fn steps_read_well() {
        let p = plan(
            &host(),
            &[],
            &SetupOptions {
                firewall_image: Some("/fw.img".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            text(&p),
            [
                "install template fbsd15-net (provisioned bridged to em0)",
                "create sys-net (net qube, freebsd, from fbsd15-net, PCI 5/0/0, autostart)",
                "create sys-firewall (firewall qube, unikernel, booting /fw.img, autostart)",
                "install template debian-13-dev (provisioned bridged to em0)",
                "create gpu (gpu qube, linux, from debian-13-dev, PCI 1/0/0 1/0/1)",
                "reserve devices for qubes: pptdevs=\"1/0/0 1/0/1 5/0/0\" in /boot/loader.conf",
            ]
        );
        let bridged = Step::Create(Box::new(Qube {
            uplink: Some("em0".into()),
            ..Qube::new(SYS_NET, QubeKind::Net, OsType::Freebsd)
        }));
        assert_eq!(
            bridged.to_string(),
            "create sys-net (net qube, freebsd, bridged to em0)"
        );
    }

    #[test]
    fn progress_lines_round_trip() {
        let step = Step::Start(SYS_NET.into());
        let line = progress_line(3, 9, &step);
        assert_eq!(line, "==> [3/9] start sys-net");
        assert_eq!(parse_progress(&line), Some((3, 9, "start sys-net")));
        for bad in [
            "==> Fetching x",
            "==> [3/9]",
            "==> [39] x",
            "==> [a/9] x",
            "==> [3/b] x",
            "[3/9] x",
        ] {
            assert_eq!(parse_progress(bad), None, "{bad}");
        }
    }

    #[test]
    fn waiting_polls_until_success_or_timeout() {
        let mut tries = 0;
        let mut slept = Vec::new();
        let ok = wait_until(
            Duration::from_secs(10),
            Duration::from_secs(5),
            &mut || {
                tries += 1;
                tries == 2
            },
            &mut |d| slept.push(d),
        );
        assert!(ok);
        assert_eq!((tries, slept.len()), (2, 1));
        let mut tries = 0;
        let never = wait_until(
            Duration::from_secs(10),
            Duration::from_secs(5),
            &mut || {
                tries += 1;
                false
            },
            &mut |_| {},
        );
        assert!(!never);
        assert_eq!(tries, 3, "at 0, 5 and 10 s");
    }

    #[test]
    fn host_probes() {
        let route = "   route to: default\ndestination: default\n    gateway: 192.168.1.1\n  \
                     interface: em0\n      flags: <UP,GATEWAY,DONE,STATIC>\n";
        assert_eq!(parse_route_iface(route).as_deref(), Some("em0"));
        assert_eq!(parse_route_iface("interface:\n"), None);
        let m = MockRunner::new()
            .respond("route", &[], CmdOutput::ok(route))
            .respond("sysrc", &[], CmdOutput::ok("1/0/0 1/0/1\n"));
        assert_eq!(default_route_iface(&m).as_deref(), Some("em0"));
        assert_eq!(loader_pptdevs(&m).unwrap(), [addr("1/0/0"), addr("1/0/1")]);
        reserve(&m, &[addr("1/0/0"), addr("5/0/0")]).unwrap();
        assert_eq!(
            m.calls(),
            [
                "route -n get default",
                "sysrc -f /boot/loader.conf -n pptdevs",
                "sysrc -f /boot/loader.conf 'pptdevs=1/0/0 5/0/0'",
            ]
        );

        // No default route, pptdevs unset, sysrc failing.
        let m = MockRunner::new()
            .respond("route", &[], CmdOutput::failed(1, "not in table"))
            .respond("sysrc", &[], CmdOutput::failed(1, "unknown variable"));
        assert_eq!(default_route_iface(&m), None);
        assert!(loader_pptdevs(&m).unwrap().is_empty());
        assert!(reserve(&m, &[addr("5/0/0")]).is_err());
        let bad = MockRunner::new().respond("sysrc", &[], CmdOutput::ok("1/0\n"));
        assert!(loader_pptdevs(&bad).is_err());
    }

    #[test]
    fn the_whole_host_is_probed() {
        let cfg = GlobalConfig {
            cache_dir: "/nonexistent/juanitos-cache".into(),
            ..Default::default()
        };
        let pciconf = "em0@pci0:5:0:0:\tclass=0x020000 rev=0x10 hdr=0x00 vendor=0x8086 \
                       device=0x15bc subvendor=0x0000 subdevice=0x0000\n";
        let m = MockRunner::new()
            .respond("pciconf", &[], CmdOutput::ok(pciconf))
            .respond("sysctl", &["-n", "hw.vmm.maxcpu"], CmdOutput::ok("8\n"))
            .respond("route", &[], CmdOutput::failed(1, ""));
        let h = probe_host(&m, &cfg).unwrap();
        assert_eq!(h.devices.len(), 1);
        assert_eq!((h.uplink, h.max_vcpus), (None, Some(8)));
        assert!(h.local_templates.is_empty() && h.pptdevs.is_empty());
        let broken = MockRunner::new().respond("pciconf", &[], CmdOutput::failed(1, ""));
        assert!(probe_host(&broken, &cfg).is_err());
        let bad_ppt = MockRunner::new().respond("sysrc", &[], CmdOutput::ok("1/0\n"));
        assert!(probe_host_with(&bad_ppt, &cfg, Vec::new()).is_err());
    }
}
