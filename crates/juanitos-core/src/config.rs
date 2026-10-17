//! Qube definitions and global configuration.
//!
//! Each qube is stored as one TOML file under `GlobalConfig::qubes_dir()`.
//! All paths are FreeBSD paths kept as plain strings so the library still
//! builds and tests on non-Unix development machines.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::pci::PciAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QubeKind {
    /// Read-only base image other qubes are cloned from.
    Template,
    /// Regular application qube.
    App,
    /// Owns the physical NIC(s); equivalent of Qubes' `sys-net`.
    Net,
    /// Sits between app qubes and the NetVM; equivalent of `sys-firewall`.
    Firewall,
    /// Linux qube with GPU passthrough (CUDA).
    Gpu,
}

impl QubeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            QubeKind::Template => "template",
            QubeKind::App => "app",
            QubeKind::Net => "net",
            QubeKind::Firewall => "firewall",
            QubeKind::Gpu => "gpu",
        }
    }
}

impl std::str::FromStr for QubeKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "template" => Ok(QubeKind::Template),
            "app" => Ok(QubeKind::App),
            "net" => Ok(QubeKind::Net),
            "firewall" => Ok(QubeKind::Firewall),
            "gpu" => Ok(QubeKind::Gpu),
            other => Err(Error::Other(format!("unknown qube kind: {other}"))),
        }
    }
}

impl std::fmt::Display for QubeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsType {
    Freebsd,
    Linux,
    /// Redox OS (the Relibc-based guest). Redox's virtio drivers need the
    /// modern (1.0) transport that bhyve does not emulate, so these qubes
    /// get an NVMe disk and an e1000 NIC instead of virtio devices.
    Redox,
    /// A single-purpose Rust unikernel booted from a read-only image
    /// (`Qube::kernel`) instead of a zvol. See `docs/unikernel-firewall.md`.
    Unikernel,
}

impl OsType {
    pub fn as_str(&self) -> &'static str {
        match self {
            OsType::Freebsd => "freebsd",
            OsType::Linux => "linux",
            OsType::Redox => "redox",
            OsType::Unikernel => "unikernel",
        }
    }

    /// bhyve emulation of the boot disk (`-s N,<emulation>,<path>`).
    pub fn disk_emulation(&self) -> &'static str {
        match self {
            OsType::Redox => "nvme",
            _ => "virtio-blk",
        }
    }

    /// bhyve emulation of the network interfaces.
    pub fn nic_emulation(&self) -> &'static str {
        match self {
            OsType::Redox => "e1000",
            _ => "virtio-net",
        }
    }

    /// True when the guest can have a shell on its serial console for
    /// `jos run` to type commands into (everything but a unikernel; Redox's
    /// `ion` understands the marker script too).
    pub fn has_console_shell(&self) -> bool {
        !matches!(self, OsType::Unikernel)
    }

    /// True when the guest can mount the dom0 share (a virtio-9p client:
    /// FreeBSD's `virtio_p9fs`, Linux's `9pnet_virtio`); see
    /// [`crate::share`].
    pub fn has_share(&self) -> bool {
        matches!(self, OsType::Freebsd | OsType::Linux)
    }

    /// True when the guest gets an agent channel (a virtio-console port;
    /// FreeBSD's `virtio_console`, Linux's `virtio_console`) for the
    /// `jos-agentd` daemon the templates ship; see [`crate::agent`].
    pub fn has_agent(&self) -> bool {
        matches!(self, OsType::Freebsd | OsType::Linux)
    }

    /// What `jos stop` asks the agent to run (as root) to power the guest
    /// off cleanly; `None` for guests without an agent.
    pub fn poweroff_command(&self) -> Option<&'static str> {
        match self {
            OsType::Freebsd => Some("shutdown -p now"),
            OsType::Linux => Some("poweroff"),
            OsType::Redox | OsType::Unikernel => None,
        }
    }

    /// How `jos run` frames a command for the guest's shell.
    pub fn console_dialect(&self) -> crate::console::Dialect {
        match self {
            OsType::Redox => crate::console::Dialect::Ion,
            _ => crate::console::Dialect::Posix,
        }
    }

    /// Credentials `jos run` types when the console shows a login prompt:
    /// the stock Redox image has a password-less `user`; the FreeBSD and
    /// Linux templates log their user in automatically.
    pub fn console_login(&self) -> Option<crate::console::Login> {
        match self {
            OsType::Redox => Some(crate::console::Login {
                user: "user".into(),
                password: String::new(),
            }),
            _ => None,
        }
    }
}

impl std::str::FromStr for OsType {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "freebsd" => Ok(OsType::Freebsd),
            "linux" => Ok(OsType::Linux),
            "redox" => Ok(OsType::Redox),
            "unikernel" => Ok(OsType::Unikernel),
            other => Err(Error::Other(format!("unknown os type: {other}"))),
        }
    }
}

impl std::fmt::Display for OsType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

fn default_cpus() -> u32 {
    2
}
fn default_memory_mb() -> u64 {
    2048
}
fn default_disk_gb() -> u64 {
    20
}
fn default_true() -> bool {
    true
}

/// Defaults for unikernel qubes: they are tiny.
pub const UNIKERNEL_CPUS: u32 = 1;
pub const UNIKERNEL_MEMORY_MB: u64 = 128;

/// A virtual machine definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Qube {
    pub name: String,
    pub kind: QubeKind,
    pub os: OsType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Bootable read-only image for `os = "unikernel"` qubes (an ESP image
    /// with the unikernel at `EFI/BOOT/BOOTX64.EFI`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    #[serde(default = "default_cpus")]
    pub cpus: u32,
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u64,
    #[serde(default = "default_disk_gb")]
    pub disk_gb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub netvm: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pci_passthrough: Vec<PciAddr>,
    /// Host interface bridged into the qube's upstream NIC instead of a
    /// NetVM: how a `net` qube reaches the world on a machine without
    /// IOMMU (no NIC passthrough), and how a template is provisioned
    /// before any NetVM exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uplink: Option<String>,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default = "default_true")]
    pub uefi: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_bhyve_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Address on the NetVM's bridge, allocated by `jos` from
    /// `firewall::CLIENT_NET`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<std::net::Ipv4Addr>,
    /// MAC of the upstream NIC (derived from the name; the firewall keys
    /// clients by it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Outbound rules, first match wins (see `firewall.rs` for the grammar).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub firewall: Vec<crate::firewall::Rule>,
    /// What happens when no rule matches.
    #[serde(default)]
    pub firewall_policy: crate::firewall::Action,
}

impl Qube {
    pub fn new(name: impl Into<String>, kind: QubeKind, os: OsType) -> Self {
        let unikernel = os == OsType::Unikernel;
        Qube {
            name: name.into(),
            kind,
            os,
            template: None,
            kernel: None,
            cpus: if unikernel {
                UNIKERNEL_CPUS
            } else {
                default_cpus()
            },
            memory_mb: if unikernel {
                UNIKERNEL_MEMORY_MB
            } else {
                default_memory_mb()
            },
            disk_gb: if unikernel { 0 } else { default_disk_gb() },
            netvm: None,
            pci_passthrough: Vec::new(),
            uplink: None,
            autostart: false,
            uefi: true,
            extra_bhyve_args: Vec::new(),
            label: None,
            ip: None,
            mac: None,
            firewall: Vec::new(),
            firewall_policy: crate::firewall::Action::Allow,
        }
    }

    /// Names must match `[a-z0-9][a-z0-9-]{0,30}` and not end with `-`.
    pub fn validate_name(name: &str) -> Result<()> {
        validate_name(name)
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let q: Qube = toml::from_str(text)?;
        validate_name(&q.name)?;
        Ok(q)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn is_template(&self) -> bool {
        self.kind == QubeKind::Template
    }

    /// Unikernel qubes boot from `kernel` and have no zvol.
    pub fn is_unikernel(&self) -> bool {
        self.os == OsType::Unikernel
    }

    /// True for qubes that other qubes can use as their `netvm`.
    pub fn provides_network(&self) -> bool {
        matches!(self.kind, QubeKind::Net | QubeKind::Firewall)
    }

    /// The qube's NetVM: its own `netvm`, else `default` (the global
    /// `default_netvm`). NetVMs own physical NICs instead, and a qube set
    /// to [`NO_NETVM`] has no network at all.
    pub fn resolve_netvm<'a>(&'a self, default: Option<&'a str>) -> Option<&'a str> {
        if self.kind == QubeKind::Net {
            return None;
        }
        match self.netvm.as_deref() {
            Some(NO_NETVM) => None,
            Some(n) => Some(n),
            None => default,
        }
    }
}

/// `netvm` value of a qube without network. Stored explicitly, because an
/// absent `netvm` means "the default NetVM". Reserved as a qube name.
pub const NO_NETVM: &str = "none";

pub fn validate_name(name: &str) -> Result<()> {
    let bad = || Error::InvalidName(name.to_string());
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 31 || name == NO_NETVM {
        return Err(bad());
    }
    let ok_first = bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !ok_first {
        return Err(bad());
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err(bad());
    }
    if name.ends_with('-') {
        return Err(bad());
    }
    Ok(())
}

fn default_pool() -> String {
    "zroot".into()
}
fn default_dataset_root() -> String {
    "juanitos".into()
}
fn default_config_dir() -> String {
    "/usr/local/etc/juanitos".into()
}
fn default_run_dir() -> String {
    "/var/run/juanitos".into()
}
fn default_cache_dir() -> String {
    "/var/cache/juanitos".into()
}
fn default_uefi_firmware() -> String {
    "/usr/local/share/uefi-firmware/BHYVE_UEFI.fd".into()
}
fn default_netvm() -> Option<String> {
    Some("sys-firewall".into())
}
fn default_console_dir() -> String {
    "/dev".into()
}
fn default_share_dir() -> String {
    "/var/db/juanitos/share".into()
}
fn default_vmm_dir() -> String {
    "/dev/vmm".into()
}

/// Host-wide settings, normally loaded from `/usr/local/etc/juanitos/jos.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default = "default_pool")]
    pub pool: String,
    #[serde(default = "default_dataset_root")]
    pub dataset_root: String,
    #[serde(default = "default_config_dir")]
    pub config_dir: String,
    #[serde(default = "default_run_dir")]
    pub run_dir: String,
    /// Downloaded guest images (`jos template install`).
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    #[serde(default = "default_uefi_firmware")]
    pub uefi_firmware: String,
    #[serde(default = "default_netvm", skip_serializing_if = "Option::is_none")]
    pub default_netvm: Option<String>,
    /// Where the `nmdm(4)` console pairs live (`/dev`; tests point it at
    /// a directory of pseudo-terminals).
    #[serde(default = "default_console_dir")]
    pub console_dir: String,
    /// Per-qube directories exported to the guests over virtio-9p (see
    /// [`crate::share`]).
    #[serde(default = "default_share_dir")]
    pub share_dir: String,
    /// Where `vmm(4)` lists the running VMs, one node per VM (`/dev/vmm`;
    /// tests point it at a plain directory).
    #[serde(default = "default_vmm_dir")]
    pub vmm_dir: String,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        GlobalConfig {
            pool: default_pool(),
            dataset_root: default_dataset_root(),
            config_dir: default_config_dir(),
            run_dir: default_run_dir(),
            cache_dir: default_cache_dir(),
            uefi_firmware: default_uefi_firmware(),
            default_netvm: default_netvm(),
            console_dir: default_console_dir(),
            share_dir: default_share_dir(),
            vmm_dir: default_vmm_dir(),
        }
    }
}

impl GlobalConfig {
    pub fn qubes_dir(&self) -> PathBuf {
        Path::new(&self.config_dir).join("qubes")
    }

    pub fn qube_file(&self, name: &str) -> PathBuf {
        self.qubes_dir().join(format!("{name}.toml"))
    }

    pub fn root_dataset(&self) -> String {
        format!("{}/{}", self.pool, self.dataset_root)
    }

    pub fn templates_dataset(&self) -> String {
        format!("{}/templates", self.root_dataset())
    }

    pub fn qubes_dataset(&self) -> String {
        format!("{}/qubes", self.root_dataset())
    }

    /// The zvol dataset backing a qube's disk.
    pub fn dataset_for(&self, q: &Qube) -> String {
        let parent = if q.is_template() {
            self.templates_dataset()
        } else {
            self.qubes_dataset()
        };
        format!("{parent}/{}", q.name)
    }

    pub fn zvol_path(&self, q: &Qube) -> String {
        format!("/dev/zvol/{}", self.dataset_for(q))
    }

    /// Guest side of the nmdm console pair (passed to bhyve).
    pub fn console_device(&self, name: &str) -> String {
        format!("{}/nmdm-{name}.1A", self.console_dir)
    }

    /// Host side of the nmdm console pair (used by `cu` and `jos run`).
    pub fn console_host_device(&self, name: &str) -> String {
        format!("{}/nmdm-{name}.1B", self.console_dir)
    }

    /// Raw disk image carrying a unikernel firewall's generated config.
    pub fn firewall_config_disk(&self, name: &str) -> String {
        format!("{}/{name}.fwcfg", self.run_dir)
    }

    /// The copy of a unikernel's `kernel` image it boots from (refreshed
    /// before every boot).
    pub fn unikernel_boot_image(&self, name: &str) -> String {
        format!("{}/{name}.boot.img", self.run_dir)
    }

    /// The directory exported to qube `name` (its share).
    pub fn share_dir_for(&self, name: &str) -> String {
        format!("{}/{name}", self.share_dir)
    }

    /// UNIX socket bhyve listens on for qube `name`'s agent channel (the
    /// host end of its virtio-console port; see [`crate::agent`]).
    pub fn agent_socket(&self, name: &str) -> String {
        format!("{}/{name}.agent", self.run_dir)
    }

    /// UNIX socket bhyve listens on for qube `name`'s events port, the
    /// guest-to-dom0 half of the channel (see [`crate::events`]).
    pub fn events_socket(&self, name: &str) -> String {
        format!("{}/{name}.events", self.run_dir)
    }
}

/// Load the global config, falling back to defaults when the file is absent.
pub fn load_global_config(path: Option<&Path>) -> Result<GlobalConfig> {
    let default_path = PathBuf::from(default_config_dir()).join("jos.toml");
    let path = path.unwrap_or(default_path.as_path());
    match fs::read_to_string(path) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(GlobalConfig::default()),
        Err(e) => Err(e.into()),
    }
}

/// Directory of per-qube TOML files.
#[derive(Debug, Clone)]
pub struct QubeStore {
    pub dir: PathBuf,
}

impl QubeStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        QubeStore { dir: dir.into() }
    }

    fn path_for(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.toml"))
    }

    pub fn exists(&self, name: &str) -> bool {
        self.path_for(name).is_file()
    }

    pub fn get(&self, name: &str) -> Result<Qube> {
        validate_name(name)?;
        match fs::read_to_string(self.path_for(name)) {
            Ok(text) => Qube::from_toml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::QubeNotFound(name.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, q: &Qube) -> Result<()> {
        validate_name(&q.name)?;
        fs::create_dir_all(&self.dir)?;
        fs::write(self.path_for(&q.name), q.to_toml()?)?;
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.path_for(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::QubeNotFound(name.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// All qubes, sorted by name. A missing directory yields an empty list.
    pub fn list(&self) -> Result<Vec<Qube>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut qubes = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            qubes.push(Qube::from_toml(&text)?);
        }
        qubes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(qubes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qube_toml_roundtrip_with_defaults() {
        let minimal = "name = \"work\"\nkind = \"app\"\nos = \"freebsd\"\n";
        let q = Qube::from_toml(minimal).unwrap();
        assert_eq!(q.cpus, 2);
        assert_eq!(q.memory_mb, 2048);
        assert_eq!(q.disk_gb, 20);
        assert!(q.uefi);
        assert!(!q.autostart);
        assert!(q.pci_passthrough.is_empty());
        assert_eq!(q, Qube::new("work", QubeKind::App, OsType::Freebsd));

        let text = q.to_toml().unwrap();
        assert!(!text.contains("template"));
        assert!(!text.contains("netvm"));
        let back = Qube::from_toml(&text).unwrap();
        assert_eq!(back, q);
    }

    #[test]
    fn qube_toml_roundtrip_full() {
        let mut q = Qube::new("gpu-cuda", QubeKind::Gpu, OsType::Linux);
        q.template = Some("debian-12".into());
        q.cpus = 8;
        q.memory_mb = 16384;
        q.netvm = Some("sys-firewall".into());
        q.pci_passthrough = vec![PciAddr::new(1, 0, 0), PciAddr::new(1, 0, 1)];
        q.autostart = true;
        q.uefi = false;
        q.extra_bhyve_args = vec!["-w".into()];
        q.label = Some("red".into());
        let text = q.to_toml().unwrap();
        assert!(text.contains("pci_passthrough"));
        assert!(text.contains("\"1/0/0\"") && text.contains("\"1/0/1\""));
        assert!(text.contains("kind = \"gpu\""));
        assert!(text.contains("os = \"linux\""));
        let back = Qube::from_toml(&text).unwrap();
        assert_eq!(back, q);
    }

    #[test]
    fn only_agent_guests_are_asked_to_power_off() {
        for os in [
            OsType::Freebsd,
            OsType::Linux,
            OsType::Redox,
            OsType::Unikernel,
        ] {
            assert_eq!(os.poweroff_command().is_some(), os.has_agent(), "{os}");
        }
        assert_eq!(OsType::Freebsd.poweroff_command(), Some("shutdown -p now"));
        assert_eq!(OsType::Linux.poweroff_command(), Some("poweroff"));
    }

    #[test]
    fn unikernel_defaults_and_roundtrip() {
        let mut q = Qube::new("sys-firewall", QubeKind::Firewall, OsType::Unikernel);
        assert!(q.is_unikernel());
        assert_eq!(q.cpus, UNIKERNEL_CPUS);
        assert_eq!(q.memory_mb, UNIKERNEL_MEMORY_MB);
        assert_eq!(q.disk_gb, 0);
        q.kernel = Some("/usr/local/share/juanitos/jos-firewall.img".into());
        let text = q.to_toml().unwrap();
        assert!(text.contains("os = \"unikernel\""));
        assert!(text.contains("kernel = "));
        assert_eq!(Qube::from_toml(&text).unwrap(), q);
        assert_eq!("unikernel".parse::<OsType>().unwrap(), OsType::Unikernel);
    }

    #[test]
    fn name_validation() {
        let max = "x".repeat(31);
        let too_long = "x".repeat(32);
        let valid: [&str; 6] = ["a", "sys-net", "work2", "0abc", "a-b-c", &max];
        for ok in valid {
            assert!(validate_name(ok).is_ok(), "{ok} should be valid");
        }
        assert!(validate_name(NO_NETVM).is_err(), "reserved");
        let invalid: [&str; 9] = [
            "",
            "-x",
            "x-",
            "Work",
            "a_b",
            "a b",
            "a.b",
            &too_long,
            "ünïcode",
        ];
        for bad in invalid {
            assert!(validate_name(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn global_config_paths() {
        let cfg = GlobalConfig::default();
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let t = Qube::new("fbsd14", QubeKind::Template, OsType::Freebsd);
        assert_eq!(cfg.templates_dataset(), "zroot/juanitos/templates");
        assert_eq!(cfg.qubes_dataset(), "zroot/juanitos/qubes");
        assert_eq!(cfg.dataset_for(&q), "zroot/juanitos/qubes/work");
        assert_eq!(cfg.dataset_for(&t), "zroot/juanitos/templates/fbsd14");
        assert_eq!(cfg.zvol_path(&q), "/dev/zvol/zroot/juanitos/qubes/work");
        assert_eq!(cfg.console_device("work"), "/dev/nmdm-work.1A");
        assert_eq!(cfg.console_host_device("work"), "/dev/nmdm-work.1B");
        assert_eq!(cfg.default_netvm.as_deref(), Some("sys-firewall"));
        assert!(cfg
            .qube_file("work")
            .to_string_lossy()
            .ends_with("work.toml"));
    }

    #[test]
    fn global_config_partial_toml() {
        let cfg: GlobalConfig = toml::from_str("pool = \"tank\"\n").unwrap();
        assert_eq!(cfg.pool, "tank");
        assert_eq!(cfg.dataset_root, "juanitos");
        assert_eq!(cfg.default_netvm.as_deref(), Some("sys-firewall"));
    }

    #[test]
    fn store_crud() {
        let dir = std::env::temp_dir().join(format!(
            "jos-store-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = QubeStore::new(&dir);
        assert!(store.list().unwrap().is_empty());
        assert!(!store.exists("work"));
        assert!(matches!(store.get("work"), Err(Error::QubeNotFound(_))));

        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        store.save(&q).unwrap();
        assert!(store.exists("work"));
        assert_eq!(store.get("work").unwrap(), q);
        store
            .save(&Qube::new("abc", QubeKind::Net, OsType::Freebsd))
            .unwrap();
        let names: Vec<String> = store.list().unwrap().into_iter().map(|q| q.name).collect();
        assert_eq!(names, vec!["abc", "work"]);

        store.remove("work").unwrap();
        assert!(!store.exists("work"));
        assert!(matches!(store.remove("work"), Err(Error::QubeNotFound(_))));
        let _ = fs::remove_dir_all(&dir);
    }
}
