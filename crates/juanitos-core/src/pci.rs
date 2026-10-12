//! PCI addresses and `pciconf(8)` parsing for bhyve passthrough.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};
use crate::sys::Runner;

/// A PCI bus/slot/function triple, as used by bhyve's `passthru` device and
/// the `pptdevs` loader tunable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PciAddr {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
}

impl PciAddr {
    pub fn new(bus: u8, slot: u8, func: u8) -> Self {
        PciAddr { bus, slot, func }
    }
}

impl fmt::Display for PciAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.bus, self.slot, self.func)
    }
}

impl FromStr for PciAddr {
    type Err = Error;

    /// Accepts `B/S/F`, `B:S:F`, `pciN:B:S:F` (pciconf style) and
    /// `DDDD:BB:SS.F` (Linux / lspci style).
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let bad = || Error::InvalidPciAddr(s.to_string());

        // Linux style: 0000:01:00.0
        if let Some((rest, func)) = s.rsplit_once('.') {
            let parts: Vec<&str> = rest.split(':').collect();
            if let [dom, bus, slot] = parts.as_slice() {
                if !dom.is_empty() && !bus.is_empty() && !slot.is_empty() {
                    let bus = u8::from_str_radix(bus, 16).map_err(|_| bad())?;
                    let slot = u8::from_str_radix(slot, 16).map_err(|_| bad())?;
                    let func = u8::from_str_radix(func, 16).map_err(|_| bad())?;
                    return Ok(PciAddr { bus, slot, func });
                }
            }
        }

        let parts: Vec<&str> = s.split(['/', ':']).collect();
        let (bus, slot, func) = match parts.as_slice() {
            [b, sl, f] => (*b, *sl, *f),
            [dom, b, sl, f] if dom.starts_with("pci") => (*b, *sl, *f),
            _ => return Err(bad()),
        };
        let parse = |p: &str| p.parse::<u8>().map_err(|_| bad());
        Ok(PciAddr {
            bus: parse(bus)?,
            slot: parse(slot)?,
            func: parse(func)?,
        })
    }
}

impl Serialize for PciAddr {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PciAddr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A device as reported by `pciconf -l [-v]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciDevice {
    pub addr: PciAddr,
    /// Driver name including unit number, e.g. `ppt0`, `nvidia0`, `none3`.
    pub driver: String,
    pub vendor: u16,
    pub device: u16,
    /// 24-bit PCI class code (`class:subclass:progif`).
    pub class: u32,
    /// From `-v` output: `vendor = '...'` / `device = '...'`, joined.
    pub description: Option<String>,
}

impl PciDevice {
    /// True when the device is already bound to the `ppt` driver and can be
    /// handed to bhyve.
    pub fn is_passthrough_ready(&self) -> bool {
        self.driver.starts_with("ppt")
    }

    pub fn is_gpu(&self) -> bool {
        (self.class >> 16) == 0x03
    }

    pub fn is_nic(&self) -> bool {
        (self.class >> 16) == 0x02
    }
}

/// Parse `pciconf -l` or `pciconf -l -v` output.
///
/// Device lines look like:
/// `nvidia0@pci0:1:0:0:  class=0x030000 rev=0xa1 hdr=0x00 vendor=0x10de device=0x1b80 ...`
/// and in `-v` mode are followed by indented `key = 'value'` lines.
pub fn parse_pciconf(text: &str) -> Vec<PciDevice> {
    let mut devices: Vec<PciDevice> = Vec::new();
    let mut vendor_name: Option<String> = None;
    let mut device_name: Option<String> = None;

    let flush_desc =
        |dev: Option<&mut PciDevice>, v: &mut Option<String>, d: &mut Option<String>| {
            if let Some(dev) = dev {
                let parts: Vec<String> = [v.take(), d.take()].into_iter().flatten().collect();
                if !parts.is_empty() {
                    dev.description = Some(parts.join(" "));
                }
            }
        };

    for raw in text.lines() {
        if raw.is_empty() {
            continue;
        }
        if raw.starts_with(char::is_whitespace) {
            // Verbose detail line: `    vendor     = 'NVIDIA Corporation'`
            if let Some((k, v)) = raw.split_once('=') {
                let v = v.trim().trim_matches('\'').to_string();
                match k.trim() {
                    "vendor" => vendor_name = Some(v),
                    "device" => device_name = Some(v),
                    _ => {}
                }
            }
            continue;
        }

        flush_desc(devices.last_mut(), &mut vendor_name, &mut device_name);

        let Some((head, tail)) = raw.split_once(':') else {
            continue;
        };
        // head = "nvidia0@pci0", tail = "1:0:0:  class=0x... vendor=0x..."
        let Some((driver, sel)) = head.split_once('@') else {
            continue;
        };
        // Selector continues in tail up to the double-space / tab.
        let (sel_rest, fields) = match tail.find(char::is_whitespace) {
            Some(i) => (&tail[..i], &tail[i..]),
            None => (tail, ""),
        };
        let selector = format!("{sel}:{}", sel_rest.trim_end_matches(':'));
        let Ok(addr) = selector.parse::<PciAddr>() else {
            continue;
        };

        let mut vendor = 0u16;
        let mut device = 0u16;
        let mut class = 0u32;
        for field in fields.split_whitespace() {
            let Some((k, v)) = field.split_once('=') else {
                continue;
            };
            let v = v.trim_start_matches("0x");
            match k {
                "vendor" => vendor = u16::from_str_radix(v, 16).unwrap_or(0),
                "device" => device = u16::from_str_radix(v, 16).unwrap_or(0),
                "class" => class = u32::from_str_radix(v, 16).unwrap_or(0),
                _ => {}
            }
        }

        devices.push(PciDevice {
            addr,
            driver: driver.to_string(),
            vendor,
            device,
            class,
            description: None,
        });
    }
    flush_desc(devices.last_mut(), &mut vendor_name, &mut device_name);
    devices
}

/// Build the `pptdevs` line for `/boot/loader.conf`.
pub fn pptdevs_line(addrs: &[PciAddr]) -> String {
    let list: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
    format!("pptdevs=\"{}\"", list.join(" "))
}

/// Query the host with `pciconf -l -v`.
pub fn list_devices(runner: &dyn Runner) -> Result<Vec<PciDevice>> {
    let out = runner.run_checked("pciconf", &["-l", "-v"])?;
    Ok(parse_pciconf(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all_forms() {
        let want = PciAddr::new(1, 0, 0);
        assert_eq!("1/0/0".parse::<PciAddr>().unwrap(), want);
        assert_eq!("1:0:0".parse::<PciAddr>().unwrap(), want);
        assert_eq!("pci0:1:0:0".parse::<PciAddr>().unwrap(), want);
        assert_eq!("0000:01:00.0".parse::<PciAddr>().unwrap(), want);
        assert_eq!(
            "0000:0a:00.1".parse::<PciAddr>().unwrap(),
            PciAddr::new(10, 0, 1)
        );
        assert!("1/0".parse::<PciAddr>().is_err());
        assert!("a/b/c".parse::<PciAddr>().is_err());
        assert!("".parse::<PciAddr>().is_err());
    }

    #[test]
    fn display_and_serde_roundtrip() {
        let a = PciAddr::new(3, 4, 5);
        assert_eq!(a.to_string(), "3/4/5");

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct W {
            pci: Vec<PciAddr>,
        }
        let w = W {
            pci: vec![a, PciAddr::new(1, 0, 0)],
        };
        let s = toml::to_string(&w).unwrap();
        assert!(s.contains("\"3/4/5\""));
        let back: W = toml::from_str(&s).unwrap();
        assert_eq!(back, w);
    }

    const PCICONF_L: &str = "\
hostb0@pci0:0:0:0:\tclass=0x060000 rev=0x00 hdr=0x00 vendor=0x8086 device=0x3e30 subvendor=0x1458 subdevice=0x5000
ppt0@pci0:1:0:0:\tclass=0x030000 rev=0xa1 hdr=0x00 vendor=0x10de device=0x1b80 subvendor=0x1458 subdevice=0x3702
ppt1@pci0:1:0:1:\tclass=0x040300 rev=0xa1 hdr=0x00 vendor=0x10de device=0x10f0 subvendor=0x1458 subdevice=0x3702
igb0@pci0:5:0:0:\tclass=0x020000 rev=0x03 hdr=0x00 vendor=0x8086 device=0x1533 subvendor=0x8086 subdevice=0x0000
none2@pci0:0:22:0:\tclass=0x078000 rev=0x10 hdr=0x00 vendor=0x8086 device=0xa360 subvendor=0x1458 subdevice=0x1c3a
";

    #[test]
    fn parse_pciconf_plain() {
        let devs = parse_pciconf(PCICONF_L);
        assert_eq!(devs.len(), 5);
        let gpu = &devs[1];
        assert_eq!(gpu.addr, PciAddr::new(1, 0, 0));
        assert_eq!(gpu.driver, "ppt0");
        assert_eq!(gpu.vendor, 0x10de);
        assert_eq!(gpu.device, 0x1b80);
        assert_eq!(gpu.class, 0x030000);
        assert!(gpu.is_gpu());
        assert!(gpu.is_passthrough_ready());
        assert!(!gpu.is_nic());
        assert!(gpu.description.is_none());

        let nic = &devs[3];
        assert!(nic.is_nic());
        assert!(!nic.is_passthrough_ready());
        assert_eq!(devs[4].addr, PciAddr::new(0, 22, 0));
    }

    const PCICONF_LV: &str = "\
hostb0@pci0:0:0:0:\tclass=0x060000 rev=0x00 hdr=0x00 vendor=0x8086 device=0x3e30 subvendor=0x1458 subdevice=0x5000
    vendor     = 'Intel Corporation'
    device     = '8th Gen Core Processor Host Bridge/DRAM Registers'
    class      = bridge
    subclass   = HOST-PCI
nvidia0@pci0:1:0:0:\tclass=0x030000 rev=0xa1 hdr=0x00 vendor=0x10de device=0x1b80 subvendor=0x1458 subdevice=0x3702
    vendor     = 'NVIDIA Corporation'
    device     = 'GP104 [GeForce GTX 1080]'
    class      = display
    subclass   = VGA
ppt0@pci0:1:0:1:\tclass=0x040300 rev=0xa1 hdr=0x00 vendor=0x10de device=0x10f0 subvendor=0x1458 subdevice=0x3702
    vendor     = 'NVIDIA Corporation'
    device     = 'GP104 High Definition Audio Controller'
    class      = multimedia
    subclass   = HDA
";

    #[test]
    fn parse_pciconf_verbose() {
        let devs = parse_pciconf(PCICONF_LV);
        assert_eq!(devs.len(), 3);
        assert_eq!(
            devs[0].description.as_deref(),
            Some("Intel Corporation 8th Gen Core Processor Host Bridge/DRAM Registers")
        );
        let gpu = &devs[1];
        assert_eq!(gpu.driver, "nvidia0");
        assert!(gpu.is_gpu());
        assert!(!gpu.is_passthrough_ready());
        assert_eq!(
            gpu.description.as_deref(),
            Some("NVIDIA Corporation GP104 [GeForce GTX 1080]")
        );
        assert!(devs[2].is_passthrough_ready());
        assert_eq!(devs[2].class, 0x040300);
    }

    #[test]
    fn pptdevs_formatting() {
        assert_eq!(pptdevs_line(&[]), "pptdevs=\"\"");
        assert_eq!(
            pptdevs_line(&[PciAddr::new(1, 0, 0), PciAddr::new(1, 0, 1)]),
            "pptdevs=\"1/0/0 1/0/1\""
        );
    }
}
