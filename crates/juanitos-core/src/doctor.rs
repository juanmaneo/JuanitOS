//! Host readiness checks (`jos doctor`).

use std::path::Path;

use crate::config::GlobalConfig;
use crate::pci::parse_pciconf;
use crate::sys::Runner;
use crate::zfs::Zfs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

/// One line of the `jos doctor` table: `[FAIL] name  detail`.
impl std::fmt::Display for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{:<4}] {:<40} {}",
            self.status.as_str(),
            self.name,
            self.detail
        )
    }
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>) -> Self {
        Check {
            name: name.to_string(),
            status,
            detail: detail.into(),
        }
    }
}

/// Value from `sysctl name` output (`hw.vmm.iommu.enable: 1` -> `1`),
/// or the raw trimmed text when `sysctl -n` style output is given.
pub fn parse_sysctl_value(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    match line.split_once(':') {
        Some((_, v)) => v.trim().to_string(),
        None => line.to_string(),
    }
}

/// Value of `hw.vmm.iommu.enable`, `None` when the sysctl is missing (vmm
/// not loaded).
pub fn iommu_setting(runner: &dyn Runner) -> Option<String> {
    match runner.run("sysctl", &["hw.vmm.iommu.enable"]) {
        Ok(out) if out.success() => Some(parse_sysctl_value(&out.stdout)),
        _ => None,
    }
}

/// True when PCI passthrough can work: `hw.vmm.iommu.enable=1`.
pub fn iommu_enabled(runner: &dyn Runner) -> bool {
    iommu_setting(runner).as_deref() == Some("1")
}

/// True when `sysctl hw.model` names an AMD processor.
pub fn is_amd_cpu(runner: &dyn Runner) -> bool {
    match runner.run("sysctl", &["hw.model"]) {
        Ok(out) if out.success() => parse_sysctl_value(&out.stdout).contains("AMD"),
        _ => false,
    }
}

/// Pool names from `zpool list -H -o name`.
pub fn parse_zpool_list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// Boot messages, where identcpu(9) lists the CPU's virtualisation features
/// whether or not vmm is loaded.
pub const DMESG_BOOT: &str = "/var/run/dmesg.boot";

/// What bhyve needs from the CPU, judged from the `VT-x:` / `SVM:` line
/// identcpu prints into `dmesg.boot`. vmx.c refuses to initialise without
/// EPT and bhyve's UEFI (ROM) boot exits without unrestricted guest; svm.c
/// requires nested paging. Pre-Nehalem Intel CPUs (Core 2) have VT-x but
/// neither, so vmm cannot run any qube there even though VT-x is present.
pub fn cpu_virt_check(dmesg: Option<&str>) -> Check {
    const NAME: &str = "cpu virtualization";
    let Some(dmesg) = dmesg else {
        return Check::new(
            NAME,
            Status::Warn,
            format!("could not read {DMESG_BOOT}; cannot tell whether the CPU supports bhyve"),
        );
    };
    // identcpu indents its feature lines; vmm.ko's own complaints
    // (`SVM: not available.`) start in column 0 and must not match.
    let line = dmesg.lines().find_map(|l| {
        let l = l.strip_prefix(char::is_whitespace)?.trim_start();
        l.strip_prefix("VT-x:")
            .map(|r| ("VT-x", r))
            .or_else(|| l.strip_prefix("SVM:").map(|r| ("SVM", r)))
    });
    let Some((kind, rest)) = line else {
        return Check::new(
            NAME,
            Status::Fail,
            "the CPU has neither Intel VT-x nor AMD-V (SVM); bhyve cannot run",
        );
    };
    let rest = rest.trim();
    if rest.starts_with("(disabled in BIOS)") {
        return Check::new(
            NAME,
            Status::Fail,
            format!("{kind} is disabled in the firmware; enable it in the BIOS/UEFI setup"),
        );
    }
    let features: Vec<&str> = rest.split(',').map(str::trim).collect();
    let required: &[(&str, &str)] = if kind == "VT-x" {
        &[("EPT", "EPT"), ("UG", "unrestricted guest")]
    } else {
        &[("NP", "nested paging")]
    };
    let missing: Vec<&str> = required
        .iter()
        .filter(|(flag, _)| !features.contains(flag))
        .map(|(_, what)| *what)
        .collect();
    if missing.is_empty() {
        Check::new(NAME, Status::Ok, format!("{kind}: {rest}"))
    } else {
        Check::new(
            NAME,
            Status::Fail,
            format!(
                "{kind} lacks {}, which bhyve requires (Intel needs Nehalem/Core i 2008+); no qube can start",
                missing.join(" and ")
            ),
        )
    }
}

/// Kernel modules a dom0 needs. Checked by *module* name (`kldstat -q -m`,
/// as rc.subr's `load_kld` does), which also finds modules compiled into
/// GENERIC such as `if_tap` (part of `if_tuntap`), unlike the file names a
/// plain `kldstat` lists.
const REQUIRED_MODULES: &[&str] = &["vmm", "nmdm", "if_bridge", "if_tap"];

/// All checks: hardware/kernel readiness followed by storage.
pub fn run_checks(runner: &dyn Runner, cfg: &GlobalConfig) -> Vec<Check> {
    let mut checks = run_host_checks(runner, cfg);
    checks.extend(run_storage_checks(runner, cfg));
    checks
}

/// True if no check failed outright.
pub fn all_passed(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.status != Status::Fail)
}

/// Checks that depend only on the kernel and hardware, not on whether a
/// pool exists yet. The installer runs these on the live medium: because
/// the live medium *is* the dom0 that gets installed, passing here means
/// the installed system will run qubes.
pub fn run_host_checks(runner: &dyn Runner, cfg: &GlobalConfig) -> Vec<Check> {
    let mut checks = vec![cpu_virt_check(
        std::fs::read_to_string(DMESG_BOOT).ok().as_deref(),
    )];

    // Kernel modules
    for &m in REQUIRED_MODULES {
        let name = format!("kernel module {m}");
        match runner.run("kldstat", &["-q", "-m", m]) {
            Ok(out) if out.success() => {
                checks.push(Check::new(&name, Status::Ok, "loaded"));
            }
            Ok(_) => checks.push(Check::new(
                &name,
                Status::Fail,
                format!(
                    "not loaded; run `kldload {m}` and add `{m}_load=\"YES\"` to /boot/loader.conf"
                ),
            )),
            Err(_) => checks.push(Check::new(
                &name,
                Status::Fail,
                "could not run kldstat (not FreeBSD?)",
            )),
        }
    }

    // IOMMU
    match iommu_setting(runner) {
        Some(v) if v == "1" => {
            checks.push(Check::new("iommu", Status::Ok, "hw.vmm.iommu.enable=1"));
        }
        Some(v) => checks.push(Check::new(
            "iommu",
            Status::Fail,
            format!("hw.vmm.iommu.enable={v}; set hw.vmm.iommu.enable=1 in /boot/loader.conf (needed for PCI passthrough)"),
        )),
        None => checks.push(Check::new(
            "iommu",
            Status::Fail,
            "sysctl hw.vmm.iommu.enable unavailable (vmm not loaded?)",
        )),
    }

    // AMD-Vi needs its own switch: vmm picks the IOMMU driver by CPU vendor
    // and amdvi refuses to initialise ("use hw.vmm.amdvi.enable=1") unless
    // asked, so on AMD `hw.vmm.iommu.enable=1` alone leaves `jos start`
    // failing with `vm_assign_pptdev: Device not configured`.
    if is_amd_cpu(runner) {
        match runner.run("sysctl", &["hw.vmm.amdvi.enable"]) {
            Ok(out) if out.success() && parse_sysctl_value(&out.stdout) == "1" => {
                checks.push(Check::new("amd-vi", Status::Ok, "hw.vmm.amdvi.enable=1"));
            }
            _ => checks.push(Check::new(
                "amd-vi",
                Status::Fail,
                "AMD CPU: also set hw.vmm.amdvi.enable=1 in /boot/loader.conf (needed for PCI passthrough)",
            )),
        }
    }

    // /dev/vmm
    if Path::new("/dev/vmm").exists() {
        checks.push(Check::new("/dev/vmm", Status::Ok, "present"));
    } else {
        checks.push(Check::new(
            "/dev/vmm",
            Status::Warn,
            "missing; appears after vmm is loaded and the first VM is created",
        ));
    }

    // UEFI firmware
    if Path::new(&cfg.uefi_firmware).is_file() {
        checks.push(Check::new(
            "uefi firmware",
            Status::Ok,
            cfg.uefi_firmware.clone(),
        ));
    } else {
        checks.push(Check::new(
            "uefi firmware",
            Status::Warn,
            format!(
                "{} not found; run `pkg install edk2-bhyve`",
                cfg.uefi_firmware
            ),
        ));
    }

    // Passthrough devices
    match runner.run("pciconf", &["-l"]) {
        Ok(out) if out.success() => {
            let ready: Vec<String> = parse_pciconf(&out.stdout)
                .into_iter()
                .filter(|d| d.is_passthrough_ready())
                .map(|d| d.addr.to_string())
                .collect();
            if ready.is_empty() {
                checks.push(Check::new(
                    "ppt devices",
                    Status::Warn,
                    "no ppt* devices; set pptdevs in /boot/loader.conf for GPU/NIC passthrough",
                ));
            } else {
                checks.push(Check::new("ppt devices", Status::Ok, ready.join(" ")));
            }
        }
        _ => checks.push(Check::new(
            "ppt devices",
            Status::Warn,
            "could not run pciconf",
        )),
    }

    checks
}

/// Checks on the ZFS pool and JuanitOS datasets.
pub fn run_storage_checks(runner: &dyn Runner, cfg: &GlobalConfig) -> Vec<Check> {
    let mut checks = Vec::new();

    // ZFS pool
    match runner.run("zpool", &["list", "-H", "-o", "name"]) {
        Ok(out) if out.success() => {
            let pools = parse_zpool_list(&out.stdout);
            if pools.iter().any(|p| p == &cfg.pool) {
                checks.push(Check::new(
                    "zpool",
                    Status::Ok,
                    format!("pool {} online", cfg.pool),
                ));
            } else {
                checks.push(Check::new(
                    "zpool",
                    Status::Fail,
                    format!(
                        "pool {} not found (available: {})",
                        cfg.pool,
                        pools.join(", ")
                    ),
                ));
            }
        }
        _ => checks.push(Check::new("zpool", Status::Fail, "could not run zpool")),
    }

    // Datasets
    let zfs = Zfs::new(runner);
    for ds in [cfg.templates_dataset(), cfg.qubes_dataset()] {
        match zfs.exists(&ds) {
            Ok(true) => checks.push(Check::new(&format!("dataset {ds}"), Status::Ok, "exists")),
            _ => checks.push(Check::new(
                &format!("dataset {ds}"),
                Status::Warn,
                "missing; run `jos init`",
            )),
        }
    }

    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{CmdOutput, MockRunner};

    #[test]
    fn sysctl_parsing() {
        assert_eq!(parse_sysctl_value("hw.vmm.iommu.enable: 1\n"), "1");
        assert_eq!(parse_sysctl_value("0\n"), "0");
        assert_eq!(parse_sysctl_value(""), "");
    }

    #[test]
    fn cpu_virtualization_from_dmesg() {
        let check = |text: &str| cpu_virt_check(Some(text));
        let intel_ok = "CPU: Intel(R) Core(TM) i7-8700\n  VT-x: PAT,HLT,MTF,PAUSE,EPT,UG,VPID\n";
        assert_eq!(check(intel_ok).status, Status::Ok);
        assert!(check(intel_ok).detail.contains("EPT,UG"));

        let amd_ok = "  SVM: NP,NRIP,VClean,AFlush,NAsids=16\n";
        assert_eq!(check(amd_ok).status, Status::Ok);

        // Core 2 Duo: VT-x without EPT or unrestricted guest.
        let core2 = check("  VT-x: HLT,PAUSE\n");
        assert_eq!(core2.status, Status::Fail);
        assert!(core2.detail.contains("EPT and unrestricted guest"));
        let no_ug = check("  VT-x: PAT,HLT,MTF,PAUSE,EPT,VPID\n");
        assert_eq!(no_ug.status, Status::Fail);
        assert!(no_ug.detail.contains("lacks unrestricted guest,"));
        let no_np = check("  SVM: NRIP\n");
        assert!(no_np.detail.contains("nested paging"));

        let locked = check("  VT-x: (disabled in BIOS) HLT,PAUSE\n");
        assert_eq!(locked.status, Status::Fail);
        assert!(locked.detail.contains("disabled in the firmware"));

        let none = check("CPU: Intel(R) Core(TM)2 Duo\nSVM: not available.\n");
        assert_eq!(none.status, Status::Fail);
        assert!(none.detail.contains("neither"));

        assert_eq!(cpu_virt_check(None).status, Status::Warn);
    }

    #[test]
    fn zpool_parsing() {
        assert_eq!(parse_zpool_list("zroot\ntank\n\n"), vec!["zroot", "tank"]);
    }

    #[test]
    fn run_checks_reports_failures() {
        let cfg = GlobalConfig::default();
        // Only vmm and if_tap (compiled into GENERIC) are present.
        let m = MockRunner::new()
            .respond("kldstat", &[], CmdOutput::failed(1, ""))
            .respond("kldstat", &["-q", "-m", "vmm"], CmdOutput::ok(""))
            .respond("kldstat", &["-q", "-m", "if_tap"], CmdOutput::ok(""))
            .respond("sysctl", &[], CmdOutput::ok("hw.vmm.iommu.enable: 0\n"))
            .respond("zpool", &[], CmdOutput::ok("tank\n"))
            .respond("zfs", &[], CmdOutput::failed(1, ""))
            .respond(
                "pciconf",
                &[],
                CmdOutput::ok("ppt0@pci0:1:0:0:\tclass=0x030000 vendor=0x10de device=0x1b80\n"),
            );
        let checks = run_checks(&m, &cfg);
        let get = |n: &str| checks.iter().find(|c| c.name == n).unwrap();
        assert_eq!(get("kernel module vmm").status, Status::Ok);
        assert_eq!(get("kernel module if_tap").status, Status::Ok);
        assert_eq!(get("kernel module nmdm").status, Status::Fail);
        assert_eq!(get("kernel module if_bridge").status, Status::Fail);
        assert!(m.calls().iter().any(|c| c == "kldstat -q -m if_tap"));
        assert_eq!(get("iommu").status, Status::Fail);
        assert!(checks.iter().all(|c| c.name != "amd-vi"));
        assert_eq!(get("zpool").status, Status::Fail);
        assert_eq!(get("dataset zroot/juanitos/qubes").status, Status::Warn);
        assert_eq!(get("ppt devices").status, Status::Ok);
        assert_eq!(get("ppt devices").detail, "1/0/0");
        assert!(!all_passed(&checks));

        let host = run_host_checks(&m, &cfg);
        assert!(host.iter().all(|c| c.name != "zpool"));
        assert!(host.iter().any(|c| c.name == "ppt devices"));
    }

    #[test]
    fn amd_hosts_need_the_amdvi_switch() {
        let cfg = GlobalConfig::default();
        let amd = |amdvi: &str| {
            MockRunner::new()
                .respond("kldstat", &[], CmdOutput::ok(""))
                .respond(
                    "sysctl",
                    &["hw.model"],
                    CmdOutput::ok("hw.model: AMD Ryzen 7 5800U with Radeon Graphics\n"),
                )
                .respond(
                    "sysctl",
                    &["hw.vmm.iommu.enable"],
                    CmdOutput::ok("hw.vmm.iommu.enable: 1\n"),
                )
                .respond(
                    "sysctl",
                    &["hw.vmm.amdvi.enable"],
                    CmdOutput::ok(format!("hw.vmm.amdvi.enable: {amdvi}\n")),
                )
                .respond("pciconf", &[], CmdOutput::ok(""))
        };
        let m = amd("0");
        assert!(is_amd_cpu(&m));
        let checks = run_host_checks(&m, &cfg);
        let c = checks.iter().find(|c| c.name == "amd-vi").unwrap();
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("hw.vmm.amdvi.enable=1"));
        assert!(!all_passed(&checks));

        let checks = run_host_checks(&amd("1"), &cfg);
        let c = checks.iter().find(|c| c.name == "amd-vi").unwrap();
        assert_eq!(c.status, Status::Ok);

        let intel = MockRunner::new()
            .respond("kldstat", &[], CmdOutput::ok(""))
            .respond(
                "sysctl",
                &["hw.model"],
                CmdOutput::ok("hw.model: Intel(R) Core(TM) i7-8700\n"),
            )
            .respond("sysctl", &[], CmdOutput::ok("hw.vmm.iommu.enable: 1\n"))
            .respond("pciconf", &[], CmdOutput::ok(""));
        assert!(!is_amd_cpu(&intel));
        let checks = run_host_checks(&intel, &cfg);
        assert!(checks.iter().all(|c| c.name != "amd-vi"));
    }
}
