//! `jos setup`: the first-boot step that turns a fresh install into a
//! working system, like the QubesOS firstboot creating `sys-net`,
//! `sys-firewall` and the default qubes.
//!
//! [`plan`] is pure: from what the host has ([`Host`]: PCI devices, the
//! dom0's uplink, IOMMU, the current `pptdevs`) and the qubes already
//! defined, it lists the [`Step`]s still missing, so running `jos setup`
//! again after a failure resumes where it stopped. The CLI executes them:
//!
//! 1. build the templates the system qubes clone from, downloading through
//!    the dom0's own network card (`jos template install --uplink`);
//! 2. create `sys-net` (that card passed through, or bridged to it when
//!    the machine has no IOMMU), `sys-firewall` (the Rust unikernel when
//!    its image is installed, else a FreeBSD qube) and `gpu` (the NVIDIA
//!    card reserved at install time, with the functions sharing its slot);
//! 3. last, reserve the passed-through devices that the dom0 still drives
//!    (`pptdevs` in `/boot/loader.conf`): the dom0 keeps its network card
//!    until the reboot that hands it to `sys-net`.

use std::fmt;

use crate::config::{OsType, Qube, QubeKind};
use crate::error::{Error, Result};
use crate::pci::{self, PciAddr, PciDevice};
use crate::sys::Runner;
use crate::template;

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

/// What the host has, probed by the CLI.
#[derive(Debug, Clone, Default)]
pub struct Host {
    /// `pciconf -l -v`.
    pub devices: Vec<PciDevice>,
    /// Interface of the dom0's default route ([`default_route_iface`]).
    pub uplink: Option<String>,
    /// `hw.vmm.iommu.enable=1`: PCI passthrough works.
    pub iommu: bool,
    /// `pptdevs` in `/boot/loader.conf` ([`loader_pptdevs`]).
    pub pptdevs: Vec<PciAddr>,
}

/// The operator's choices (`jos setup` flags).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOptions {
    /// Interface to download through (default: [`Host::uplink`]).
    pub uplink: Option<String>,
    /// Network card for `sys-net` (default: the uplink's).
    pub nic: Option<PciAddr>,
    /// Create the `gpu` qube when an NVIDIA card is reserved.
    pub gpu: bool,
    pub gpu_cpus: u32,
    pub gpu_memory_mb: u64,
    /// Unikernel image for `sys-firewall`, when installed.
    pub firewall_image: Option<String>,
}

impl Default for SetupOptions {
    fn default() -> Self {
        SetupOptions {
            uplink: None,
            nic: None,
            gpu: true,
            gpu_cpus: GPU_CPUS,
            gpu_memory_mb: GPU_MEMORY_MB,
            firewall_image: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `jos template install <recipe> --uplink <uplink>`.
    InstallTemplate {
        recipe: &'static str,
        uplink: String,
    },
    /// `jos create` of this qube.
    Create(Box<Qube>),
    /// Write this whole list as `pptdevs` in `/boot/loader.conf`.
    ReservePci(Vec<PciAddr>),
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::InstallTemplate { recipe, uplink } => {
                write!(
                    f,
                    "install template {recipe} (downloading through {uplink})"
                )
            }
            Step::Create(q) => {
                write!(f, "create {} ({} qube, {}", q.name, q.kind, q.os)?;
                if let Some(t) = &q.template {
                    write!(f, ", from {t}")?;
                }
                if let Some(k) = &q.kernel {
                    write!(f, ", booting {k}")?;
                }
                if !q.pci_passthrough.is_empty() {
                    write!(f, ", PCI {}", pci::addr_list(&q.pci_passthrough))?;
                }
                if let Some(u) = &q.uplink {
                    write!(f, ", bridged to {u}")?;
                }
                if q.autostart {
                    write!(f, ", autostart")?;
                }
                write!(f, ")")
            }
            Step::ReservePci(addrs) => write!(
                f,
                "reserve devices for qubes: {} in {LOADER_CONF}",
                pci::pptdevs_line(addrs)
            ),
        }
    }
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

/// The steps still missing for `sys-net`, `sys-firewall` and `gpu`.
pub fn plan(host: &Host, existing: &[Qube], opts: &SetupOptions) -> Result<Plan> {
    let has = |name: &str| existing.iter().any(|q| q.name == name);
    let uplink = opts.uplink.clone().or_else(|| host.uplink.clone());
    let mut notes = Vec::new();
    let mut qubes = Vec::new();

    if has(SYS_NET) {
        notes.push(format!("{SYS_NET} exists"));
    } else {
        qubes.push(sys_net(host, uplink.as_deref(), opts.nic)?);
    }
    if has(SYS_FIREWALL) {
        notes.push(format!("{SYS_FIREWALL} exists"));
    } else {
        qubes.push(sys_firewall(opts.firewall_image.as_deref()));
    }
    if !opts.gpu {
        notes.push(format!("no {GPU} qube (--no-gpu)"));
    } else if has(GPU) {
        notes.push(format!("{GPU} exists"));
    } else {
        let addrs = gpu_devices(&host.devices);
        if addrs.is_empty() {
            notes.push(format!(
                "no {GPU} qube: no NVIDIA card is reserved (add it to pptdevs in {LOADER_CONF}, \
                 reboot and run `jos setup` again)"
            ));
        } else {
            qubes.push(gpu_qube(addrs, opts));
        }
    }

    let mut recipes: Vec<&'static template::Recipe> = Vec::new();
    for t in qubes.iter().filter_map(|q| q.template.as_deref()) {
        if has(t) || recipes.iter().any(|r| r.name == t) {
            continue;
        }
        let r = template::find(t)
            .ok_or_else(|| Error::Other(format!("no recipe builds template {t}")))?;
        recipes.push(r);
    }
    let mut steps = Vec::new();
    if !recipes.is_empty() {
        let Some(iface) = uplink else {
            return Err(Error::Other(format!(
                "the dom0 has no network to download the templates ({}): pass --uplink IFACE, \
                 or install them from local images first (`jos template install <recipe> \
                 --image FILE --checksums FILE`)",
                recipes
                    .iter()
                    .map(|r| r.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        steps.extend(recipes.iter().map(|r| Step::InstallTemplate {
            recipe: r.name,
            uplink: iface.clone(),
        }));
    }

    // Devices of the system qubes (existing or new) the dom0 still drives.
    let managed = [SYS_NET, SYS_FIREWALL, GPU];
    let mut unbound: Vec<PciAddr> = existing
        .iter()
        .filter(|q| managed.contains(&q.name.as_str()))
        .chain(qubes.iter())
        .flat_map(|q| q.pci_passthrough.iter().copied())
        .filter(|a| {
            host.devices
                .iter()
                .any(|d| d.addr == *a && !d.is_passthrough_ready())
        })
        .collect();
    let reboot = !unbound.is_empty();
    unbound.retain(|a| !host.pptdevs.contains(a));

    steps.extend(qubes.into_iter().map(|q| Step::Create(Box::new(q))));
    if !unbound.is_empty() {
        let mut all = host.pptdevs.clone();
        all.extend(unbound);
        all.sort();
        all.dedup();
        steps.push(Step::ReservePci(all));
    }
    Ok(Plan {
        steps,
        notes,
        reboot,
    })
}

/// A qube cloned from a recipe's template, sized like the recipe.
fn clone_of(name: &str, kind: QubeKind, template_name: &str) -> Qube {
    let recipe = template::find(template_name);
    let mut q = Qube::new(name, kind, recipe.map_or(OsType::Freebsd, |r| r.os));
    q.template = Some(template_name.to_string());
    if let Some(r) = recipe {
        q.cpus = r.cpus;
        q.memory_mb = r.memory_mb;
    }
    q
}

fn sys_net(host: &Host, uplink: Option<&str>, nic: Option<PciAddr>) -> Result<Qube> {
    let mut q = clone_of(SYS_NET, QubeKind::Net, NET_TEMPLATE);
    q.autostart = true;
    let card = match nic {
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
        None if host.iommu => uplink
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

fn gpu_qube(addrs: Vec<PciAddr>, opts: &SetupOptions) -> Qube {
    let mut q = clone_of(GPU, QubeKind::Gpu, GPU_TEMPLATE);
    q.cpus = opts.gpu_cpus;
    q.memory_mb = opts.gpu_memory_mb;
    q.pci_passthrough = addrs;
    q
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{CmdOutput, MockRunner};

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

    #[test]
    fn fresh_install_plans_everything_and_reserves_the_nic_last() {
        let p = plan(&host(), &[], &SetupOptions::default()).unwrap();
        assert_eq!(
            p.steps.first(),
            Some(&Step::InstallTemplate {
                recipe: NET_TEMPLATE,
                uplink: "em0".into()
            })
        );
        assert_eq!(
            p.steps.get(1),
            Some(&Step::InstallTemplate {
                recipe: GPU_TEMPLATE,
                uplink: "em0".into()
            })
        );
        assert_eq!(
            p.steps.last(),
            Some(&Step::ReservePci(vec![
                addr("1/0/0"),
                addr("1/0/1"),
                addr("5/0/0")
            ]))
        );
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
    fn without_iommu_sys_net_is_bridged_to_the_uplink() {
        let h = Host {
            iommu: false,
            ..host()
        };
        let p = plan(&h, &[], &SetupOptions::default()).unwrap();
        let net = get(&created(&p), SYS_NET).clone();
        assert!(net.pci_passthrough.is_empty());
        assert_eq!(net.uplink.as_deref(), Some("em0"));
        // The NVIDIA card is still reserved (and bound): no new pptdevs.
        assert!(!p.steps.iter().any(|s| matches!(s, Step::ReservePci(_))));
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
    }

    #[test]
    fn explicit_nic_and_uplink() {
        let opts = SetupOptions {
            nic: Some(addr("6/0/0")),
            uplink: Some("wlan0".into()),
            ..Default::default()
        };
        let p = plan(&host(), &[], &opts).unwrap();
        assert_eq!(get(&created(&p), SYS_NET).pci_passthrough, [addr("6/0/0")]);
        assert!(p.steps.contains(&Step::InstallTemplate {
            recipe: NET_TEMPLATE,
            uplink: "wlan0".into()
        }));

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
    fn a_nic_reserved_at_install_time_goes_to_sys_net() {
        let mut h = host();
        h.uplink = None;
        if let Some(d) = h.devices.iter_mut().find(|d| d.addr == addr("5/0/0")) {
            d.driver = "ppt2".into();
        }
        h.pptdevs.push(addr("5/0/0"));
        // No network left in the dom0 to download the templates...
        let err = plan(&h, &[], &SetupOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("--image FILE --checksums FILE"), "{err}");
        assert!(err.contains(NET_TEMPLATE) && err.contains(GPU_TEMPLATE));
        // ...unless they were installed from local images.
        let templates = [
            Qube::new(NET_TEMPLATE, QubeKind::Template, OsType::Freebsd),
            Qube::new(GPU_TEMPLATE, QubeKind::Template, OsType::Linux),
        ];
        let p = plan(&h, &templates, &SetupOptions::default()).unwrap();
        assert_eq!(get(&created(&p), SYS_NET).pci_passthrough, [addr("5/0/0")]);
        assert!(!p.reboot, "everything is bound already");
        assert!(!p
            .steps
            .iter()
            .any(|s| matches!(s, Step::InstallTemplate { .. } | Step::ReservePci(_))));
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
        if let Some(d) = devices.iter_mut().find(|d| d.addr == addr("1/0/1")) {
            d.driver = "hdac1".into();
        }
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
        let text: Vec<String> = p.steps.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            text,
            [
                "install template fbsd15-net (downloading through em0)",
                "install template debian-13-dev (downloading through em0)",
                "create sys-net (net qube, freebsd, from fbsd15-net, PCI 5/0/0, autostart)",
                "create sys-firewall (firewall qube, unikernel, booting /fw.img, autostart)",
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
}
