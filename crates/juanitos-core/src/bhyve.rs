//! bhyve command construction and VM lifecycle.
//!
//! PCI slot layout inside the guest:
//!
//! | slot | device                                                      |
//! |------|-------------------------------------------------------------|
//! | 0    | hostbridge                                                  |
//! | 2    | virtio-blk: root zvol, or the read-only unikernel image     |
//! | 3    | virtio-net: upstream tap (to the NetVM or a host `uplink`)  |
//! | 4    | virtio-net: downstream tap (own bridge), for net/firewall   |
//! |      | (Redox qubes: `nvme` and `e1000` in the same slots)         |
//! | 4+   | passthru devices, after the last network slot               |
//! | 28   | virtio-9p: the qube's dom0 share (FreeBSD and Linux guests) |
//! | 30   | virtio-console: agent and events ports (FreeBSD and Linux)  |
//! | 29   | ahci-cd: cloud-init seed while a template is provisioned    |
//! | 30   | virtio-blk: raw config disk of a unikernel firewall         |
//! | 31   | lpc (com1 console, bootrom)                                 |

use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::{GlobalConfig, Qube};
use crate::error::{Error, Result};
use crate::net::Net;
use crate::sys::{shell_join, Runner};

/// A fully-built command line ready to hand to a [`Runner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BhyveCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl BhyveCommand {
    pub fn to_shell(&self) -> String {
        let args: Vec<&str> = self.args.iter().map(String::as_str).collect();
        shell_join(&self.program, &args)
    }

    fn arg_refs(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }

    pub fn run(&self, runner: &dyn Runner) -> Result<crate::sys::CmdOutput> {
        runner.run(&self.program, &self.arg_refs())
    }

    pub fn run_checked(&self, runner: &dyn Runner) -> Result<crate::sys::CmdOutput> {
        runner.run_checked(&self.program, &self.arg_refs())
    }
}

/// First guest PCI slot used for virtio-net devices.
pub const FIRST_NET_SLOT: u32 = 3;
/// Lowest guest PCI slot used for passthrough devices (higher when more
/// than one tap is attached).
pub const MIN_PASSTHRU_SLOT: u32 = 4;
/// Guest PCI slot of a unikernel firewall's raw config disk.
pub const CONFIG_DISK_SLOT: u32 = crate::firewall::CONFIG_DISK_SLOT;
/// Guest PCI slot of the virtio-9p share (see [`crate::share`]).
pub const SHARE_SLOT: u32 = 28;
/// Guest PCI slot of the cloud-init seed CD (NoCloud `cidata` ISO)
/// attached while a template is provisioned.
pub const SEED_SLOT: u32 = 29;
/// Reboots tolerated while a template is maintained (a package upgrade
/// may restart the guest) before giving up.
pub const MAX_MAINTENANCE_BOOTS: u32 = 5;

/// `cmd` with a CD drive holding `iso` (inserted before the VM name).
pub fn with_cdrom(mut cmd: BhyveCommand, iso: &str) -> BhyveCommand {
    let at = cmd.args.len().saturating_sub(1);
    cmd.args.insert(at, "-s".into());
    cmd.args
        .insert(at + 1, format!("{SEED_SLOT},ahci,cd:{iso}"));
    cmd
}

/// The image attached as the boot disk: the qube's zvol, or for unikernels
/// the per-boot copy of its `kernel` image (see [`Vm::stage_kernel`]).
///
/// Nothing is attached read-only: the bhyve UEFI firmware does not boot
/// from a `virtio-blk,...,ro` device ("UEFI Misc Device: Not Found"),
/// which is why the unikernel boots a fresh copy instead.
fn boot_disk(q: &Qube, cfg: &GlobalConfig) -> Result<String> {
    if q.is_unikernel() {
        if q.kernel.is_none() {
            return Err(Error::Other(format!(
                "{} is a unikernel qube but has no `kernel` image",
                q.name
            )));
        }
        Ok(cfg.unikernel_boot_image(&q.name))
    } else {
        Ok(cfg.zvol_path(q))
    }
}

/// True when `q` boots the Rust firewall unikernel and therefore gets a
/// generated config disk.
pub fn has_config_disk(q: &Qube) -> bool {
    q.is_unikernel() && q.kind == crate::config::QubeKind::Firewall
}

/// Most vCPUs a guest can have on this host: `hw.vmm.maxcpu` (above it
/// bhyve fails in `vm_set_topology`), else `hw.ncpu` while vmm.ko is not
/// loaded. `None` when neither can be read.
pub fn max_vcpus(runner: &dyn Runner) -> Option<u32> {
    ["hw.vmm.maxcpu", "hw.ncpu"].iter().find_map(|oid| {
        let out = runner.run("sysctl", &["-n", oid]).ok()?;
        if !out.success() {
            return None;
        }
        out.stdout.trim().parse().ok().filter(|n| *n > 0)
    })
}

/// Error unless `q`'s vCPU count fits `max` (from [`max_vcpus`]).
pub fn check_vcpus(q: &Qube, max: Option<u32>) -> Result<()> {
    match max {
        Some(max) if q.cpus > max => Err(Error::Other(format!(
            "{} has {} vCPUs but this host runs at most {max} (hw.vmm.maxcpu): \
             lower `cpus` in its definition",
            q.name, q.cpus
        ))),
        _ => Ok(()),
    }
}

/// `cpus` reduced to what the host runs (`max`), with a note saying so
/// when it had to shrink: for counts the operator did not choose.
pub fn fit_vcpus(name: &str, cpus: u32, max: Option<u32>) -> (u32, Option<String>) {
    match max {
        Some(max) if cpus > max => (
            max,
            Some(format!(
                "{name}: {cpus} vCPUs capped to {max}, the most this host runs (hw.vmm.maxcpu)"
            )),
        ),
        _ => (cpus, None),
    }
}

/// Build the `bhyve` invocation for `q`. `taps` are the host tap interfaces
/// to attach as virtio-net devices, in slot order (upstream first); empty
/// for an isolated qube. The upstream NIC gets the qube's fixed MAC.
pub fn build_run_command(q: &Qube, cfg: &GlobalConfig, taps: &[&str]) -> Result<BhyveCommand> {
    fn add(args: &mut Vec<String>, items: &[&str]) {
        args.extend(items.iter().map(|s| s.to_string()));
    }

    let disk = boot_disk(q, cfg)?;
    let mut args: Vec<String> = Vec::new();
    add(&mut args, &["-c", &q.cpus.to_string()]);
    add(&mut args, &["-m", &format!("{}M", q.memory_mb)]);
    add(&mut args, &["-A", "-H", "-P"]);
    if !q.pci_passthrough.is_empty() {
        // Passthrough requires guest memory to be wired.
        add(&mut args, &["-S"]);
    }
    add(&mut args, &["-s", "0,hostbridge"]);
    add(&mut args, &["-s", "31,lpc"]);
    add(
        &mut args,
        &["-s", &format!("2,{},{disk}", q.os.disk_emulation())],
    );
    let has_upstream = q.kind != crate::config::QubeKind::Net || q.uplink.is_some();
    for (i, tap) in taps.iter().enumerate() {
        let slot = FIRST_NET_SLOT + i as u32;
        let mut dev = format!("{slot},{},{tap}", q.os.nic_emulation());
        if i == 0 && has_upstream {
            if let Some(mac) = &q.mac {
                dev.push_str(&format!(",mac={mac}"));
            }
        }
        add(&mut args, &["-s", &dev]);
    }
    if has_config_disk(q) {
        add(
            &mut args,
            &[
                "-s",
                &format!(
                    "{CONFIG_DISK_SLOT},virtio-blk,{}",
                    cfg.firewall_config_disk(&q.name)
                ),
            ],
        );
    }
    if q.os.has_share() {
        add(&mut args, &["-s", &crate::share::device_arg(cfg, q)]);
    }
    if q.os.has_agent() {
        add(&mut args, &["-s", &crate::agent::device_arg(cfg, q)]);
    }
    let first_passthru = MIN_PASSTHRU_SLOT.max(FIRST_NET_SLOT + taps.len() as u32);
    for (i, addr) in q.pci_passthrough.iter().enumerate() {
        let slot = first_passthru + i as u32;
        add(&mut args, &["-s", &format!("{slot},passthru,{addr}")]);
    }
    add(
        &mut args,
        &["-l", &format!("com1,{}", cfg.console_device(&q.name))],
    );
    if q.uefi {
        add(
            &mut args,
            &["-l", &format!("bootrom,{}", cfg.uefi_firmware)],
        );
    }
    args.extend(q.extra_bhyve_args.iter().cloned());
    args.push(q.name.clone());
    Ok(BhyveCommand {
        program: "bhyve".into(),
        args,
    })
}

/// How a guest left bhyve, decoded from the exit status.
///
/// bhyve(8) exits 0 after a guest reset, 1 after poweroff, 2 after halt,
/// 3 on a triple fault and 4 on an internal error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestExit {
    Reboot,
    Poweroff,
    Halt,
    TripleFault,
    Error(Option<i32>),
}

impl GuestExit {
    pub fn from_status(code: Option<i32>) -> Self {
        match code {
            Some(0) => GuestExit::Reboot,
            Some(1) => GuestExit::Poweroff,
            Some(2) => GuestExit::Halt,
            Some(3) => GuestExit::TripleFault,
            other => GuestExit::Error(other),
        }
    }

    /// True when the supervisor should boot the guest again.
    pub fn wants_restart(&self) -> bool {
        matches!(self, GuestExit::Reboot)
    }

    /// True for the orderly ways of leaving a VM (anything but an error).
    pub fn is_clean(&self) -> bool {
        !matches!(self, GuestExit::Error(_))
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            GuestExit::Reboot => "reboot",
            GuestExit::Poweroff => "poweroff",
            GuestExit::Halt => "halt",
            GuestExit::TripleFault => "triple fault",
            GuestExit::Error(_) => "error",
        }
    }
}

impl std::fmt::Display for GuestExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuestExit::Error(Some(c)) => write!(f, "error (exit code {c})"),
            GuestExit::Error(None) => write!(f, "error (killed by signal)"),
            other => f.write_str(other.as_str()),
        }
    }
}

pub fn build_destroy_command(name: &str) -> BhyveCommand {
    BhyveCommand {
        program: "bhyvectl".into(),
        args: vec!["--destroy".into(), format!("--vm={name}")],
    }
}

pub fn build_poweroff_command(name: &str) -> BhyveCommand {
    BhyveCommand {
        program: "bhyvectl".into(),
        args: vec!["--force-poweroff".into(), format!("--vm={name}")],
    }
}

pub fn build_console_command(cfg: &GlobalConfig, name: &str) -> BhyveCommand {
    BhyveCommand {
        program: "cu".into(),
        args: vec![
            "-l".into(),
            cfg.console_host_device(name),
            "-s".into(),
            "115200".into(),
        ],
    }
}

/// A VM is running when its vmm device node exists.
pub fn is_running(cfg: &GlobalConfig, name: &str) -> bool {
    Path::new(&cfg.vmm_dir).join(name).exists()
}

/// How often [`Vm::await_poweroff`] looks for the VMs.
pub const POWEROFF_POLL: Duration = Duration::from_millis(250);

/// The order in which to shut `qubes` down: tiers, each to be stopped
/// (together) before the next, so that a qube goes before the NetVM it
/// depends on (app qubes, then firewalls, then `sys-net`). Within a tier
/// the qubes keep their order.
pub fn shutdown_tiers<'a>(qubes: &'a [Qube], default_netvm: Option<&str>) -> Vec<Vec<&'a Qube>> {
    let find = |name: &str| qubes.iter().find(|q| q.name == name);
    // Hops from a qube to the end of its NetVM chain; a loop ends it.
    let depth = |q: &'a Qube| {
        let mut seen = vec![q.name.as_str()];
        let mut cur = q;
        while let Some(up) = cur.resolve_netvm(default_netvm).and_then(find) {
            if seen.contains(&up.name.as_str()) {
                break;
            }
            seen.push(&up.name);
            cur = up;
        }
        seen.len() - 1
    };
    let mut ranked: Vec<(usize, &Qube)> = qubes.iter().map(|q| (depth(q), q)).collect();
    ranked.sort_by_key(|(d, _)| std::cmp::Reverse(*d));
    let mut tiers: Vec<(usize, Vec<&Qube>)> = Vec::new();
    for (d, q) in ranked {
        match tiers.last_mut() {
            Some((td, tier)) if *td == d => tier.push(q),
            _ => tiers.push((d, vec![q])),
        }
    }
    tiers.into_iter().map(|(_, tier)| tier).collect()
}

/// VM lifecycle operations.
pub struct Vm<'a> {
    pub runner: &'a dyn Runner,
    pub cfg: &'a GlobalConfig,
}

impl<'a> Vm<'a> {
    pub fn new(runner: &'a dyn Runner, cfg: &'a GlobalConfig) -> Self {
        Vm { runner, cfg }
    }

    /// Resolve the NetVM for `q`: explicit `netvm`, else the global default,
    /// unless `q` is itself a NetVM (which owns physical NICs instead) or
    /// has `netvm = "none"`.
    pub fn effective_netvm(&self, q: &Qube) -> Option<String> {
        q.resolve_netvm(self.cfg.default_netvm.as_deref())
            .map(String::from)
    }

    /// Create the tap interfaces `q` needs (upstream to its NetVM, plus a
    /// downstream one when `q` itself provides network) and return them in
    /// guest slot order.
    pub fn setup_taps(&self, q: &Qube) -> Result<Vec<String>> {
        let net = Net::new(self.runner);
        let mut taps: Vec<String> = Vec::new();
        if let Some(iface) = &q.uplink {
            taps.push(net.create_uplink_tap(&q.name, iface)?);
        } else if let Some(netvm) = self.effective_netvm(q) {
            taps.push(net.create_tap(&q.name, &netvm)?);
        }
        if q.provides_network() {
            taps.push(net.create_downstream_tap(&q.name)?);
        }
        Ok(taps)
    }

    /// Boot the guest once: set up taps, run bhyve *synchronously* until the
    /// guest exits, then release the VM slot. Taps are left in place so a
    /// reboot can reuse them. Non-clean exits are reported as
    /// [`Error::Command`]. The restart loop lives in
    /// [`crate::supervisor::Supervisor`].
    pub fn run_once(&self, q: &Qube) -> Result<GuestExit> {
        if q.is_template() {
            return Err(Error::Other(format!(
                "{} is a template; clone it into an app qube instead",
                q.name
            )));
        }
        check_vcpus(q, max_vcpus(self.runner))?;
        let taps = self.setup_taps(q)?;
        let tap_refs: Vec<&str> = taps.iter().map(String::as_str).collect();
        self.stage_kernel(q)?;
        self.share().prepare(q)?;
        self.agent().prepare(q)?;
        let cmd = build_run_command(q, self.cfg, &tap_refs)?;
        let out = cmd.run(self.runner)?;
        let _ = build_destroy_command(&q.name).run(self.runner);
        let exit = GuestExit::from_status(out.status);
        if exit.is_clean() {
            Ok(exit)
        } else {
            Err(Error::Command {
                program: cmd.to_shell(),
                code: out.status,
                stderr: out.stderr,
            })
        }
    }

    fn share(&self) -> crate::share::Share<'a> {
        crate::share::Share::new(self.runner, self.cfg)
    }

    fn agent(&self) -> crate::agent::Agent<'a> {
        crate::agent::Agent::new(self.runner, self.cfg)
    }

    /// A unikernel boots a fresh copy of its `kernel` image under the run
    /// directory: the shipped image stays pristine whatever the guest (or
    /// the firmware, which wants the disk writable) does to its copy.
    pub fn stage_kernel(&self, q: &Qube) -> Result<()> {
        let Some(kernel) = q.kernel.as_deref().filter(|_| q.is_unikernel()) else {
            return Ok(());
        };
        self.runner
            .run_checked("mkdir", &["-p", &self.cfg.run_dir])?;
        self.runner
            .run_checked("cp", &[kernel, &self.cfg.unikernel_boot_image(&q.name)])?;
        Ok(())
    }

    /// Boot a template for maintenance (provisioning with a cloud-init
    /// `seed` ISO, or updates) until it powers off, following reboots.
    /// Templates are never started otherwise: app qubes run clones of them.
    pub fn maintain(&self, q: &Qube, seed: Option<&str>) -> Result<GuestExit> {
        let cmd = self.maintenance_command(q, seed)?;
        let mut result = Ok(GuestExit::Reboot);
        for _ in 0..MAX_MAINTENANCE_BOOTS {
            let out = cmd.run(self.runner);
            let _ = build_destroy_command(&q.name).run(self.runner);
            result = match out {
                Ok(out) => {
                    let exit = GuestExit::from_status(out.status);
                    // Only an orderly end (or a reboot, followed below)
                    // counts: a crashed template must never be snapshotted.
                    if matches!(
                        exit,
                        GuestExit::Poweroff | GuestExit::Halt | GuestExit::Reboot
                    ) {
                        Ok(exit)
                    } else {
                        Err(Error::Command {
                            program: cmd.to_shell(),
                            code: out.status,
                            stderr: out.stderr,
                        })
                    }
                }
                Err(e) => Err(e),
            };
            if !matches!(result, Ok(GuestExit::Reboot)) {
                break;
            }
        }
        let _ = Net::new(self.runner).destroy_taps(&q.name);
        match result {
            Ok(GuestExit::Reboot) => Err(Error::Other(format!(
                "{} kept rebooting ({MAX_MAINTENANCE_BOOTS} boots); see `jos console {}`",
                q.name, q.name
            ))),
            other => other,
        }
    }

    /// Set up the taps and build the command [`Vm::maintain`] runs (a dry
    /// run prints it instead of waiting for a guest that never boots).
    pub fn maintenance_command(&self, q: &Qube, seed: Option<&str>) -> Result<BhyveCommand> {
        check_vcpus(q, max_vcpus(self.runner))?;
        let taps = self.setup_taps(q)?;
        let tap_refs: Vec<&str> = taps.iter().map(String::as_str).collect();
        self.share().prepare(q)?;
        self.agent().prepare(q)?;
        let cmd = build_run_command(q, self.cfg, &tap_refs)?;
        Ok(match seed {
            Some(iso) => with_cdrom(cmd, iso),
            None => cmd,
        })
    }

    /// Boot the guest once and tear down its taps afterwards. Use the
    /// supervisor for anything long-lived.
    pub fn start(&self, q: &Qube) -> Result<GuestExit> {
        if q.is_template() {
            return self.run_once(q);
        }
        let result = self.run_once(q);
        let _ = Net::new(self.runner).destroy_taps(&q.name);
        result
    }

    /// Force the guest off (bhyve has no `bhyvectl` verb for an ACPI power
    /// button): what `jos stop` does when the guest cannot be asked to
    /// shut down from inside, or does not within the timeout.
    pub fn stop(&self, name: &str) -> Result<()> {
        build_poweroff_command(name).run_checked(self.runner)?;
        Ok(())
    }

    /// Wait up to `timeout` for the VMs `names` to go away (their guests
    /// were asked to power off), then force off those still running.
    /// Returns the ones that had to be forced.
    pub fn await_poweroff(&self, names: &[String], timeout: Duration) -> Result<Vec<String>> {
        let deadline = Instant::now() + timeout;
        let running = || -> Vec<String> {
            names
                .iter()
                .filter(|n| is_running(self.cfg, n))
                .cloned()
                .collect()
        };
        let mut left = running();
        while !left.is_empty() && Instant::now() < deadline {
            std::thread::sleep(POWEROFF_POLL);
            left = running();
        }
        let mut failure = None;
        for name in &left {
            // A guest that powered off since the last look makes bhyvectl
            // fail: not an error. The first real failure is reported.
            let real = self.stop(name).err().filter(|_| is_running(self.cfg, name));
            failure = failure.or(real);
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(left),
        }
    }

    /// Hard stop: poweroff, destroy the VM and tear down its taps.
    pub fn kill(&self, name: &str) -> Result<()> {
        let _ = build_poweroff_command(name).run(self.runner);
        let _ = build_destroy_command(name).run(self.runner);
        Net::new(self.runner).destroy_taps(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OsType, QubeKind};
    use crate::pci::PciAddr;

    #[test]
    fn run_command_basic_with_net() {
        let cfg = GlobalConfig::default();
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let cmd = build_run_command(&q, &cfg, &["tap-work"]).unwrap();
        assert_eq!(cmd.program, "bhyve");
        assert_eq!(
            cmd.args,
            vec![
                "-c",
                "2",
                "-m",
                "2048M",
                "-A",
                "-H",
                "-P",
                "-s",
                "0,hostbridge",
                "-s",
                "31,lpc",
                "-s",
                "2,virtio-blk,/dev/zvol/zroot/juanitos/qubes/work",
                "-s",
                "3,virtio-net,tap-work",
                "-s",
                "28,virtio-9p,jos=/var/db/juanitos/share/work",
                "-s",
                "30,virtio-console,jos=/var/run/juanitos/work.agent,events=/var/run/juanitos/work.events",
                "-l",
                "com1,/dev/nmdm-work.1A",
                "-l",
                "bootrom,/usr/local/share/uefi-firmware/BHYVE_UEFI.fd",
                "work",
            ]
        );
        assert!(!cmd.args.contains(&"-S".to_string()));
    }

    #[test]
    fn run_command_no_net_no_uefi() {
        let cfg = GlobalConfig::default();
        let mut q = Qube::new("isolated", QubeKind::App, OsType::Freebsd);
        q.uefi = false;
        q.extra_bhyve_args = vec!["-w".into()];
        let cmd = build_run_command(&q, &cfg, &[]).unwrap();
        assert!(!cmd.args.iter().any(|a| a.contains("virtio-net")));
        assert!(!cmd.args.iter().any(|a| a.contains("bootrom")));
        let n = cmd.args.len();
        assert_eq!(cmd.args[n - 2], "-w");
        assert_eq!(cmd.args[n - 1], "isolated");
    }

    #[test]
    fn run_command_gpu_passthrough() {
        let cfg = GlobalConfig::default();
        let mut q = Qube::new("gpu", QubeKind::Gpu, OsType::Linux);
        q.cpus = 8;
        q.memory_mb = 16384;
        q.pci_passthrough = vec![PciAddr::new(1, 0, 0), PciAddr::new(1, 0, 1)];
        let cmd = build_run_command(&q, &cfg, &["tap-gpu"]).unwrap();
        let a = &cmd.args;
        // -S must come after -P and before the first -s.
        let s_pos = a.iter().position(|x| x == "-S").expect("-S present");
        let first_slot = a.iter().position(|x| x == "-s").unwrap();
        assert!(s_pos < first_slot);
        assert!(a.contains(&"4,passthru,1/0/0".to_string()));
        assert!(a.contains(&"5,passthru,1/0/1".to_string()));
        let p4 = a.iter().position(|x| x == "4,passthru,1/0/0").unwrap();
        let p5 = a.iter().position(|x| x == "5,passthru,1/0/1").unwrap();
        let net = a.iter().position(|x| x == "3,virtio-net,tap-gpu").unwrap();
        assert!(net < p4 && p4 < p5);
        assert_eq!(a[1], "8");
        assert_eq!(a[3], "16384M");
        assert_eq!(a.last().unwrap(), "gpu");
    }

    #[test]
    fn run_command_unikernel_firewall_two_taps() {
        let cfg = GlobalConfig::default();
        let mut q = Qube::new("sys-firewall", QubeKind::Firewall, OsType::Unikernel);
        q.kernel = Some("/usr/local/share/juanitos/jos-firewall.img".into());
        let cmd = build_run_command(&q, &cfg, &["tap-sys-firewall", "tapd-sys-firewal"]).unwrap();
        let a = &cmd.args;
        assert_eq!(a[1], "1");
        assert_eq!(a[3], "128M");
        // The boot disk is the per-boot copy, and nothing is read-only
        // (the bhyve firmware would not boot it).
        assert!(a.contains(&"2,virtio-blk,/var/run/juanitos/sys-firewall.boot.img".to_string()));
        assert!(a.contains(&"3,virtio-net,tap-sys-firewall".to_string()));
        assert!(a.contains(&"4,virtio-net,tapd-sys-firewal".to_string()));
        assert!(a.contains(&"30,virtio-blk,/var/run/juanitos/sys-firewall.fwcfg".to_string()));
        assert!(!a.iter().any(|x| x.ends_with(",ro")));
        assert!(!a.iter().any(|x| x.contains("zvol")));
        assert!(!a.contains(&"-S".to_string()));
        assert!(a.iter().any(|x| x.starts_with("bootrom,")));
    }

    #[test]
    fn run_command_unikernel_requires_kernel() {
        let cfg = GlobalConfig::default();
        let q = Qube::new("sys-firewall", QubeKind::Firewall, OsType::Unikernel);
        assert!(build_run_command(&q, &cfg, &[]).is_err());
    }

    #[test]
    fn unikernel_boots_a_fresh_copy_of_its_image() {
        use crate::sys::{CmdOutput, MockRunner};
        let cfg = GlobalConfig::default();
        let mut q = Qube::new("sys-firewall", QubeKind::Firewall, OsType::Unikernel);
        q.kernel = Some("/usr/local/share/juanitos/jos-firewall.img".into());
        q.netvm = Some("sys-net".into());
        let m = MockRunner::new().respond("bhyve", &[], CmdOutput::failed(1, ""));
        Vm::new(&m, &cfg).run_once(&q).unwrap();
        let calls = m.calls();
        let cp = calls
            .iter()
            .position(|c| {
                c == "cp /usr/local/share/juanitos/jos-firewall.img /var/run/juanitos/sys-firewall.boot.img"
            })
            .expect("the image is copied");
        let boot = calls.iter().position(|c| c.starts_with("bhyve ")).unwrap();
        assert!(cp < boot, "copied before booting");
        assert!(calls[boot].contains("2,virtio-blk,/var/run/juanitos/sys-firewall.boot.img"));
        // Not a unikernel: nothing to stage.
        let app = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let m = MockRunner::new();
        Vm::new(&m, &cfg).stage_kernel(&app).unwrap();
        assert!(m.calls().is_empty());
    }

    #[test]
    fn passthru_slots_follow_net_slots() {
        let cfg = GlobalConfig::default();
        let mut q = Qube::new("sys-net", QubeKind::Net, OsType::Freebsd);
        q.pci_passthrough = vec![PciAddr::new(5, 0, 0)];
        // Net qube: no upstream tap, one downstream tap -> passthru stays at 4.
        let cmd = build_run_command(&q, &cfg, &["tapd-sys-net"]).unwrap();
        assert!(cmd.args.contains(&"3,virtio-net,tapd-sys-net".to_string()));
        assert!(cmd.args.contains(&"4,passthru,5/0/0".to_string()));
        // Two taps push passthru to slot 5.
        let cmd = build_run_command(&q, &cfg, &["tap-a", "tapd-a"]).unwrap();
        assert!(cmd.args.contains(&"5,passthru,5/0/0".to_string()));
    }

    #[test]
    fn aux_commands() {
        let cfg = GlobalConfig::default();
        assert_eq!(
            build_destroy_command("work").to_shell(),
            "bhyvectl --destroy --vm=work"
        );
        assert_eq!(
            build_poweroff_command("work").to_shell(),
            "bhyvectl --force-poweroff --vm=work"
        );
        assert_eq!(
            build_console_command(&cfg, "work").to_shell(),
            "cu -l /dev/nmdm-work.1B -s 115200"
        );
    }

    #[test]
    fn effective_netvm_rules() {
        let cfg = GlobalConfig::default();
        let m = crate::sys::MockRunner::new();
        let vm = Vm::new(&m, &cfg);
        let app = Qube::new("work", QubeKind::App, OsType::Freebsd);
        assert_eq!(vm.effective_netvm(&app).as_deref(), Some("sys-firewall"));
        let mut fw = Qube::new("sys-firewall", QubeKind::Firewall, OsType::Freebsd);
        fw.netvm = Some("sys-net".into());
        assert_eq!(vm.effective_netvm(&fw).as_deref(), Some("sys-net"));
        let net = Qube::new("sys-net", QubeKind::Net, OsType::Freebsd);
        assert_eq!(vm.effective_netvm(&net), None);
        // `netvm = "none"` is no network, not the default one.
        let mut offline = Qube::new("vault", QubeKind::App, OsType::Freebsd);
        offline.netvm = Some(crate::config::NO_NETVM.into());
        assert_eq!(vm.effective_netvm(&offline), None);
    }

    #[test]
    fn upstream_nic_gets_fixed_mac() {
        let cfg = GlobalConfig::default();
        let mut q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        q.mac = Some("02:4a:00:11:22:33".into());
        let cmd = build_run_command(&q, &cfg, &["tap-work"]).unwrap();
        assert!(cmd
            .args
            .contains(&"3,virtio-net,tap-work,mac=02:4a:00:11:22:33".to_string()));
        assert!(!cmd.args.iter().any(|a| a.contains("fwcfg")));
        // A net qube has no upstream NIC: the downstream tap keeps a random MAC.
        let mut n = Qube::new("sys-net", QubeKind::Net, OsType::Freebsd);
        n.mac = Some("02:4a:00:11:22:33".into());
        let cmd = build_run_command(&n, &cfg, &["tapd-sys-net"]).unwrap();
        assert!(cmd.args.contains(&"3,virtio-net,tapd-sys-net".to_string()));
        // A FreeBSD firewall does not get the unikernel config disk.
        let fw = Qube::new("sys-firewall", QubeKind::Firewall, OsType::Freebsd);
        let cmd = build_run_command(&fw, &cfg, &["a", "b"]).unwrap();
        assert!(!cmd.args.iter().any(|a| a.contains("fwcfg")));
    }

    #[test]
    fn guest_exit_decoding() {
        assert_eq!(GuestExit::from_status(Some(0)), GuestExit::Reboot);
        assert_eq!(GuestExit::from_status(Some(1)), GuestExit::Poweroff);
        assert_eq!(GuestExit::from_status(Some(2)), GuestExit::Halt);
        assert_eq!(GuestExit::from_status(Some(3)), GuestExit::TripleFault);
        assert_eq!(GuestExit::from_status(Some(4)), GuestExit::Error(Some(4)));
        assert_eq!(GuestExit::from_status(None), GuestExit::Error(None));
        assert!(GuestExit::Reboot.wants_restart());
        assert!(!GuestExit::Poweroff.wants_restart());
        assert!(GuestExit::TripleFault.is_clean());
        assert!(!GuestExit::Error(Some(4)).is_clean());
        assert_eq!(GuestExit::Error(Some(4)).to_string(), "error (exit code 4)");
        assert_eq!(
            GuestExit::Error(None).to_string(),
            "error (killed by signal)"
        );
        assert_eq!(GuestExit::Error(None).as_str(), "error");
        assert_eq!(GuestExit::Halt.to_string(), "halt");
        assert_eq!(GuestExit::TripleFault.to_string(), "triple fault");
    }

    #[test]
    fn run_once_releases_vm_and_reports_exit() {
        let cfg = GlobalConfig::default();
        let m = crate::sys::MockRunner::new().respond(
            "bhyve",
            &[],
            crate::sys::CmdOutput::failed(1, ""),
        );
        let mut q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        q.netvm = None;
        let exit = Vm::new(&m, &cfg).run_once(&q).unwrap();
        assert_eq!(exit, GuestExit::Poweroff);
        let calls = m.calls();
        assert!(calls.iter().any(|c| c.starts_with("bhyve ")));
        assert_eq!(calls.last().unwrap(), "bhyvectl --destroy --vm=work");
        // Default netvm applies: the upstream tap was created.
        assert!(calls.contains(&"ifconfig br-sys-firewall addm tap-work".to_string()));
    }

    #[test]
    fn vcpu_limit_comes_from_vmm_then_ncpu() {
        use crate::sys::{CmdOutput, MockRunner};
        let vmm = MockRunner::new()
            .respond("sysctl", &["-n", "hw.vmm.maxcpu"], CmdOutput::ok("3\n"))
            .respond("sysctl", &["-n", "hw.ncpu"], CmdOutput::ok("8\n"));
        assert_eq!(max_vcpus(&vmm), Some(3));
        let no_vmm = MockRunner::new()
            .respond("sysctl", &["-n", "hw.vmm.maxcpu"], CmdOutput::failed(1, ""))
            .respond("sysctl", &["-n", "hw.ncpu"], CmdOutput::ok("8\n"));
        assert_eq!(max_vcpus(&no_vmm), Some(8));
        // Unmatched mock commands print nothing: unknown, nothing checked.
        assert_eq!(max_vcpus(&MockRunner::new()), None);

        let mut q = Qube::new("dev", QubeKind::App, OsType::Freebsd);
        q.cpus = 4;
        assert!(check_vcpus(&q, None).is_ok());
        assert!(check_vcpus(&q, Some(4)).is_ok());
        let e = check_vcpus(&q, Some(3)).unwrap_err().to_string();
        assert!(
            e.contains("dev has 4 vCPUs but this host runs at most 3"),
            "{e}"
        );

        assert_eq!(fit_vcpus("dev", 4, None), (4, None));
        assert_eq!(fit_vcpus("dev", 2, Some(3)), (2, None));
        let (cpus, note) = fit_vcpus("dev", 4, Some(3));
        assert_eq!(cpus, 3);
        assert!(note.unwrap().contains("4 vCPUs capped to 3"));

        // Refused before anything is set up for the guest.
        let cfg = GlobalConfig::default();
        let e = Vm::new(&vmm, &cfg).run_once(&q).unwrap_err().to_string();
        assert!(e.contains("at most 3"), "{e}");
        let mut t = Qube::new("tpl", QubeKind::Template, OsType::Freebsd);
        t.cpus = 4;
        assert!(Vm::new(&vmm, &cfg).maintenance_command(&t, None).is_err());
        assert!(!vmm
            .calls()
            .iter()
            .any(|c| c.starts_with("ifconfig") || c.starts_with("bhyve ")));
    }

    #[test]
    fn run_once_error_exit_is_an_error() {
        let cfg = GlobalConfig::default();
        let m = crate::sys::MockRunner::new().respond(
            "bhyve",
            &[],
            crate::sys::CmdOutput::failed(4, "vm exit error"),
        );
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let err = Vm::new(&m, &cfg).run_once(&q).unwrap_err().to_string();
        assert!(
            err.starts_with("command `bhyve ")
                && err.contains("(exit code Some(4)): vm exit error"),
            "{err}"
        );
        assert!(m
            .calls()
            .contains(&"bhyvectl --destroy --vm=work".to_string()));
    }

    #[test]
    fn start_refuses_templates() {
        let cfg = GlobalConfig::default();
        let m = crate::sys::MockRunner::new();
        let t = Qube::new("tmpl", QubeKind::Template, OsType::Freebsd);
        assert!(Vm::new(&m, &cfg).start(&t).is_err());
        assert!(m.calls().is_empty());
    }

    #[test]
    fn start_and_kill_tear_the_taps_down() {
        use crate::sys::{CmdOutput, MockRunner};
        let cfg = GlobalConfig::default();
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let m = MockRunner::new().respond("bhyve", &[], CmdOutput::failed(1, ""));
        assert_eq!(Vm::new(&m, &cfg).start(&q).unwrap(), GuestExit::Poweroff);
        let calls = m.calls();
        assert!(
            calls.contains(&"ifconfig tap-work destroy".to_string()),
            "{calls:?}"
        );

        let m = MockRunner::new();
        Vm::new(&m, &cfg).kill("work").unwrap();
        let calls = m.calls();
        assert_eq!(
            calls[..2],
            [
                "bhyvectl --force-poweroff --vm=work",
                "bhyvectl --destroy --vm=work"
            ]
        );
        assert!(
            calls.contains(&"ifconfig tap-work destroy".to_string()),
            "{calls:?}"
        );
    }

    /// Every host tool a boot needs may be missing or fail; each failure
    /// is reported before the guest runs.
    #[test]
    fn a_boot_fails_at_every_missing_tool() {
        use crate::sys::MockRunner;
        let cfg = GlobalConfig::default();
        let app = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let mut fw = Qube::new("sys-firewall", QubeKind::Firewall, OsType::Unikernel);
        fw.netvm = Some("sys-net".into());
        fw.kernel = Some("/k.img".into());
        let mut net = Qube::new("sys-net", QubeKind::Net, OsType::Freebsd);
        net.uplink = Some("em0".into());
        let bare = Qube::new("sys-net2", QubeKind::Net, OsType::Freebsd);
        let tmpl = Qube::new("tpl", QubeKind::Template, OsType::Freebsd);
        let gone = |tool: &str| MockRunner::new().missing(tool);
        let failing = |tool: &str, q: &Qube| {
            let m = gone(tool);
            let vm = Vm::new(&m, &cfg);
            let err = vm.run_once(q).unwrap_err().to_string();
            assert!(err.contains(tool), "{}: {err}", q.name);
            let calls = m.calls();
            let booted = calls.iter().any(|c| c.starts_with("bhyve "));
            assert_eq!(booted, tool == "bhyve", "{}: {calls:?}", q.name);
        };
        // The upstream tap, the NetVM tap, the downstream tap.
        failing("ifconfig", &net);
        failing("ifconfig", &app);
        failing("ifconfig", &bare);
        // Staging the unikernel image: the run directory, the copy.
        failing("mkdir", &fw);
        let m = gone("cp");
        assert!(Vm::new(&m, &cfg).stage_kernel(&fw).is_err());
        failing("cp", &fw);
        // The share, the agent channel, bhyve itself.
        failing("chmod", &app);
        failing("rm", &app);
        failing("bhyve", &app);
        // A unikernel without an image never gets that far.
        let mut bare_fw = fw.clone();
        bare_fw.kernel = None;
        let m = MockRunner::new();
        let err = Vm::new(&m, &cfg)
            .run_once(&bare_fw)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no `kernel` image"), "{err}");

        // The same for a template's maintenance boot.
        for tool in ["ifconfig", "chmod", "rm"] {
            let m = gone(tool);
            let vm = Vm::new(&m, &cfg);
            let err = vm.maintenance_command(&tmpl, None).unwrap_err().to_string();
            assert!(err.contains(tool), "{err}");
            let err = vm.maintain(&tmpl, None).unwrap_err().to_string();
            assert!(err.contains(tool), "{err}");
        }
        let m = gone("bhyve");
        let err = Vm::new(&m, &cfg)
            .maintain(&tmpl, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("bhyve"), "{err}");
        let mut uni = Qube::new("tpl-uni", QubeKind::Template, OsType::Unikernel);
        uni.netvm = Some("sys-net".into());
        let m = MockRunner::new();
        let err = Vm::new(&m, &cfg)
            .maintenance_command(&uni, None)
            .unwrap_err();
        assert!(err.to_string().contains("no `kernel` image"), "{err}");
    }

    #[test]
    fn net_qube_with_uplink_gets_two_taps() {
        use crate::sys::{CmdOutput, MockRunner};
        let cfg = GlobalConfig::default();
        let mut net = Qube::new("sys-net", QubeKind::Net, OsType::Freebsd);
        net.uplink = Some("em0".into());
        // Nothing exists yet: the bare `ifconfig <name>` probes fail.
        let probes_fail = || {
            let mut m = MockRunner::new();
            for name in [
                "bru-em0",
                "tap-sys-net",
                "br-sys-net",
                "tapd-sys-net",
                "tap-fbsd15-net",
            ] {
                m = m.respond("ifconfig", &[name], CmdOutput::failed(1, ""));
                // Longer prefixes win: configuring the interfaces succeeds.
                for verb in ["up", "addm", "destroy"] {
                    m = m.respond("ifconfig", &[name, verb], CmdOutput::ok(""));
                }
            }
            m
        };
        let m = probes_fail();
        let taps = Vm::new(&m, &cfg).setup_taps(&net).unwrap();
        assert_eq!(
            taps,
            vec!["tap-sys-net".to_string(), "tapd-sys-net".to_string()]
        );
        assert!(m.calls().contains(&"ifconfig bru-em0 addm em0".to_string()));
        let refs: Vec<&str> = taps.iter().map(String::as_str).collect();
        let cmd = build_run_command(&net, &cfg, &refs).unwrap();
        assert!(cmd.args.contains(&"3,virtio-net,tap-sys-net".to_string()));
        assert!(cmd.args.contains(&"4,virtio-net,tapd-sys-net".to_string()));
        // A template provisioned over an uplink skips the NetVM.
        let mut t = Qube::new("fbsd15-net", QubeKind::Template, OsType::Freebsd);
        t.uplink = Some("em0".into());
        let m = probes_fail();
        assert_eq!(
            Vm::new(&m, &cfg).setup_taps(&t).unwrap(),
            vec!["tap-fbsd15-net"]
        );
        assert!(!m.calls().iter().any(|c| c.contains("br-sys-firewall")));
    }

    #[test]
    fn redox_gets_nvme_and_e1000_instead_of_virtio() {
        let cfg = GlobalConfig::default();
        let mut q = Qube::new("redox", QubeKind::App, OsType::Redox);
        q.mac = Some("02:4a:00:00:00:01".into());
        let cmd = build_run_command(&q, &cfg, &["tap-redox"]).unwrap();
        assert!(cmd
            .args
            .contains(&"2,nvme,/dev/zvol/zroot/juanitos/qubes/redox".to_string()));
        assert!(cmd
            .args
            .contains(&"3,e1000,tap-redox,mac=02:4a:00:00:00:01".to_string()));
        assert!(!cmd.to_shell().contains("virtio"));
        assert_eq!(OsType::Freebsd.disk_emulation(), "virtio-blk");
        assert_eq!(OsType::Linux.nic_emulation(), "virtio-net");
        assert!(OsType::Redox.has_console_shell());
        assert!(OsType::Linux.has_console_shell());
        assert!(!OsType::Unikernel.has_console_shell());
        assert_eq!(
            OsType::Redox.console_login().map(|l| l.user),
            Some("user".to_string())
        );
        assert!(OsType::Freebsd.console_login().is_none());
    }

    #[test]
    fn maintenance_boot_of_a_template() {
        use crate::sys::{CmdOutput, MockRunner};
        let cfg = GlobalConfig::default();
        let t = Qube::new("dev", QubeKind::Template, OsType::Linux);

        // Powers off: one boot, seed attached before the VM name, taps gone.
        let m = MockRunner::new().respond("bhyve", &[], CmdOutput::failed(1, ""));
        let exit = Vm::new(&m, &cfg).maintain(&t, Some("/s.iso")).unwrap();
        assert_eq!(exit, GuestExit::Poweroff);
        let calls = m.calls();
        let boots: Vec<_> = calls.iter().filter(|c| c.starts_with("bhyve ")).collect();
        let boot = boots.first().map_or("", |b| b.as_str());
        assert_eq!(boots.len(), 1);
        assert!(boot.ends_with("-s 29,ahci,cd:/s.iso dev"), "{boot}");
        assert!(calls
            .iter()
            .any(|c| c.contains("destroy") && c.contains("tap-dev")));

        // Keeps rebooting: bounded, then an error.
        let m = MockRunner::new().respond("bhyve", &[], CmdOutput::ok(""));
        let err = Vm::new(&m, &cfg).maintain(&t, None).unwrap_err();
        assert!(err.to_string().contains("kept rebooting"), "{err}");
        let boots = m.calls().iter().filter(|c| c.starts_with("bhyve ")).count();
        assert_eq!(boots, MAX_MAINTENANCE_BOOTS as usize);
        assert!(!m.calls().iter().any(|c| c.contains("ahci")));

        // Crashes: reported.
        let m = MockRunner::new().respond("bhyve", &[], CmdOutput::failed(3, "fault"));
        let err = Vm::new(&m, &cfg)
            .maintain(&t, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("(exit code Some(3)): fault"), "{err}");

        // The command alone, for dry runs.
        let m = MockRunner::new();
        let cmd = Vm::new(&m, &cfg)
            .maintenance_command(&t, Some("/s.iso"))
            .unwrap();
        assert!(cmd.to_shell().contains("29,ahci,cd:/s.iso"));
    }

    /// A config whose vmm directory is a plain one the test fills.
    fn vmm_cfg(tag: &str) -> (GlobalConfig, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("jos-vmm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = GlobalConfig {
            vmm_dir: dir.to_string_lossy().into_owned(),
            ..GlobalConfig::default()
        };
        (cfg, dir)
    }

    #[test]
    fn await_poweroff_forces_only_the_stragglers() {
        use crate::sys::{CmdOutput, MockRunner};
        let (cfg, dir) = vmm_cfg("await");
        let names = vec!["work".to_string(), "web".to_string()];
        std::fs::write(dir.join("work"), "").unwrap();
        assert!(is_running(&cfg, "work"));
        assert!(!is_running(&cfg, "web"));

        // Gone already: nothing forced.
        let m = MockRunner::new();
        let vm = Vm::new(&m, &cfg);
        assert!(vm
            .await_poweroff(&names[1..], Duration::ZERO)
            .unwrap()
            .is_empty());
        assert!(m.calls().is_empty());

        // Still there at the deadline: forced off.
        assert_eq!(vm.await_poweroff(&names, Duration::ZERO).unwrap(), ["work"]);
        assert_eq!(m.calls(), ["bhyvectl --force-poweroff --vm=work"]);

        // Powers off while we wait: nothing forced.
        let m = MockRunner::new();
        let vm = Vm::new(&m, &cfg);
        let node = dir.join("work");
        let guest = std::thread::spawn(move || {
            std::thread::sleep(POWEROFF_POLL);
            std::fs::remove_file(node).unwrap();
        });
        let forced = vm.await_poweroff(&names, Duration::from_secs(30)).unwrap();
        guest.join().unwrap();
        assert!(forced.is_empty());
        assert!(m.calls().is_empty());

        // bhyvectl fails on a VM that is still there: reported.
        std::fs::write(dir.join("work"), "").unwrap();
        let m = MockRunner::new().respond("bhyvectl", &[], CmdOutput::failed(1, "busy"));
        let err = Vm::new(&m, &cfg)
            .await_poweroff(&names, Duration::ZERO)
            .unwrap_err()
            .to_string();
        assert!(err.contains("bhyvectl --force-poweroff"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shutdown_goes_from_the_leaves_to_sys_net() {
        let net = Qube::new("sys-net", QubeKind::Net, OsType::Freebsd);
        let mut fw = Qube::new("sys-firewall", QubeKind::Firewall, OsType::Unikernel);
        fw.netvm = Some("sys-net".into());
        let work = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let mut offline = Qube::new("vault", QubeKind::App, OsType::Linux);
        offline.netvm = Some(crate::config::NO_NETVM.into());
        let mut lost = Qube::new("lost", QubeKind::App, OsType::Linux);
        lost.netvm = Some("ghost".into());
        let mut gpu = Qube::new("gpu", QubeKind::Gpu, OsType::Linux);
        gpu.netvm = Some("sys-net".into());
        let qubes = vec![net, fw, work, offline, lost, gpu];
        let names = |tiers: Vec<Vec<&Qube>>| -> Vec<Vec<String>> {
            tiers
                .into_iter()
                .map(|t| t.into_iter().map(|q| q.name.clone()).collect())
                .collect()
        };
        assert_eq!(
            names(shutdown_tiers(&qubes, Some("sys-firewall"))),
            [
                vec!["work"],
                vec!["sys-firewall", "gpu"],
                vec!["sys-net", "vault", "lost"],
            ]
        );

        // A firewall left on the default NetVM (itself) and a loop end
        // their chains instead of hanging.
        let mut a = Qube::new("a", QubeKind::Firewall, OsType::Freebsd);
        a.netvm = Some("b".into());
        let mut b = Qube::new("b", QubeKind::Firewall, OsType::Freebsd);
        b.netvm = Some("a".into());
        let fw = Qube::new("sys-firewall", QubeKind::Firewall, OsType::Freebsd);
        let looped = vec![a, b, fw];
        assert_eq!(
            names(shutdown_tiers(&looped, Some("sys-firewall"))),
            [vec!["a", "b"], vec!["sys-firewall"]]
        );
        assert!(shutdown_tiers(&[], None).is_empty());
    }
}
