// SPDX-License-Identifier: BSD-3-Clause
//! The dom0's network interfaces, as the first-boot wizard offers them
//! for `sys-net`.
//!
//! The dom0 itself stays offline (`docs/security.md`): an interface listed
//! here is a candidate for `sys-net`, which either gets the PCI card
//! passed through or is bridged to the interface (`bru-<iface>`, layer 2,
//! no address on the dom0). A USB Ethernet adapter is not a PCI device, so
//! it can only be bridged.
//!
//! Everything goes through [`Runner`] (`ifconfig -a`, `kldload`), so it is
//! tested with canned output.

use crate::pci::{PciAddr, PciDevice};
use crate::sys::Runner;

/// What the interface is, as far as `sys-net` cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfaceKind {
    /// `ue(4)`: every USB Ethernet driver attaches as `ue<N>`.
    Usb,
    /// A PCI network controller driven by the dom0 (can be passed through).
    Pci(PciAddr),
    /// `wlan(4)`: Wi-Fi cannot be bridged, so it cannot be an uplink.
    Wifi,
    /// Any other Ethernet interface (`vtnet0` of a virtual machine...).
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetIface {
    pub name: String,
    pub mac: Option<String>,
    /// `status: active` (a cable is plugged in); `None` when the driver
    /// reports no status.
    pub link: Option<bool>,
    pub kind: IfaceKind,
    /// One line for the wizard's list.
    pub description: String,
}

impl NetIface {
    /// Can `sys-net` use it?
    pub fn usable(&self) -> bool {
        self.kind != IfaceKind::Wifi
    }
}

/// USB network drivers of the kernel. devmatch loads the right one when an
/// adapter is plugged in; the wizard loads them all when it did not.
pub const USB_NIC_MODULES: &[&str] = &[
    "if_axe",
    "if_axge",
    "if_ure",
    "if_cdce",
    "if_urndis",
    "if_aue",
    "if_cue",
    "if_kue",
    "if_mos",
    "if_muge",
    "if_rue",
    "if_smsc",
    "if_udav",
    "if_ipheth",
];

/// Groups an Ethernet interface may be in: virtual ones (`bridge`, `tap`,
/// `epair`, `lo`...) are not network cards.
const PHYSICAL_GROUPS: &[&str] = &["egress", "wlan"];

struct Block<'a> {
    name: &'a str,
    lines: Vec<&'a str>,
}

/// `ifconfig -a` is one block per interface: a header line at column 0
/// (`em0: flags=...`), then indented fields.
fn blocks(text: &str) -> Vec<Block<'_>> {
    let mut out: Vec<Block<'_>> = Vec::new();
    for line in text.lines() {
        if line.starts_with(char::is_whitespace) {
            if let Some(b) = out.last_mut() {
                b.lines.push(line.trim());
            }
        } else if let Some((name, _)) = line.split_once(':') {
            out.push(Block {
                name,
                lines: Vec::new(),
            });
        }
    }
    out
}

fn field<'a>(block: &Block<'a>, key: &str) -> Option<&'a str> {
    block
        .lines
        .iter()
        .find_map(|l| l.strip_prefix(key))
        .map(str::trim)
}

fn is_usb_name(name: &str) -> bool {
    name.strip_prefix("ue")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The Ethernet cards among `ifconfig -a`'s interfaces, sorted by name.
pub fn parse_ifconfig(text: &str, devices: &[PciDevice]) -> Vec<NetIface> {
    let mut out: Vec<NetIface> = blocks(text)
        .iter()
        .filter_map(|b| {
            let mac = field(b, "ether ").and_then(|m| m.split_whitespace().next());
            let media = field(b, "media:")?;
            let groups: Vec<&str> = field(b, "groups:")
                .map(|g| g.split_whitespace().collect())
                .unwrap_or_default();
            if mac.is_none() || groups.iter().any(|g| !PHYSICAL_GROUPS.contains(g)) {
                return None;
            }
            let link = field(b, "status:").map(|s| s == "active" || s == "associated");
            let wifi = groups.contains(&"wlan") || media.contains("802.11");
            let pci = devices.iter().find(|d| d.is_nic() && d.driver == b.name);
            let (kind, description) = match (wifi, is_usb_name(b.name), pci) {
                (true, _, _) => (
                    IfaceKind::Wifi,
                    "Wi-Fi (not supported as an uplink yet)".to_string(),
                ),
                (false, true, _) => (IfaceKind::Usb, "USB Ethernet adapter".to_string()),
                (false, false, Some(d)) => (IfaceKind::Pci(d.addr), d.label()),
                (false, false, None) => (IfaceKind::Other, "Ethernet".to_string()),
            };
            Some(NetIface {
                name: b.name.to_string(),
                mac: mac.map(str::to_string),
                link,
                kind,
                description,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The dom0's Ethernet interfaces; none when `ifconfig` fails.
pub fn list(runner: &dyn Runner, devices: &[PciDevice]) -> Vec<NetIface> {
    match runner.run("ifconfig", &["-a"]) {
        Ok(out) if out.success() => parse_ifconfig(&out.stdout, devices),
        _ => Vec::new(),
    }
}

/// Load every USB network driver (an adapter plugged in after boot, or one
/// devmatch did not recognise). A module that is absent or already loaded
/// is not an error.
pub fn load_usb_drivers(runner: &dyn Runner) {
    for module in USB_NIC_MODULES {
        let _ = runner.run("kldload", &["-n", module]);
    }
}

/// The interface to preselect: the one behind the dom0's default route,
/// else one with a cable, else a USB adapter (what a machine with no
/// network card of its own is given), else the first usable one. An index
/// into `ifaces`.
pub fn preferred(ifaces: &[NetIface], default_route: Option<&str>) -> Option<usize> {
    ifaces
        .iter()
        .enumerate()
        .filter(|(_, i)| i.usable())
        .min_by_key(|(n, i)| {
            (
                Some(i.name.as_str()) != default_route,
                i.link != Some(true),
                i.kind != IfaceKind::Usb,
                *n,
            )
        })
        .map(|(n, _)| n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{CmdOutput, MockRunner};

    const IFCONFIG: &str = "\
lo0: flags=1008049<UP,LOOPBACK,RUNNING,MULTICAST,LOWER_UP> metric 0 mtu 16384
\toptions=680003<RXCSUM,TXCSUM,LINKSTATE,RXCSUM_IPV6,TXCSUM_IPV6>
\tinet 127.0.0.1 netmask 0xff000000
\tgroups: lo
em0: flags=1008843<UP,BROADCAST,RUNNING,SIMPLEX,MULTICAST,LOWER_UP> metric 0 mtu 1500
\toptions=4e524bb<RXCSUM,TXCSUM,VLAN_MTU>
\tether 00:11:22:33:44:55
\tmedia: Ethernet autoselect (1000baseT <full-duplex>)
\tstatus: active
ue0: flags=1008802<BROADCAST,SIMPLEX,MULTICAST,LOWER_UP> metric 0 mtu 1500
\tether 66:77:88:99:aa:bb
\tmedia: Ethernet autoselect
\tstatus: no carrier
\tgroups: egress
bridge0: flags=1008843<UP,BROADCAST,RUNNING,SIMPLEX,MULTICAST,LOWER_UP> metric 0 mtu 1500
\tether 58:9c:fc:00:00:01
\tid 00:00:00:00:00:00 priority 32768 hellotime 2 fwddelay 15
\tmember: tap-sys-net flags=143<LEARNING,DISCOVER,AUTOEDGE,AUTOPTP>
\tgroups: bridge
tap-work: flags=9903<UP,BROADCAST,RUNNING,SIMPLEX,MULTICAST> metric 0 mtu 1500
\tether 58:9c:fc:00:00:02
\tmedia: Ethernet autoselect
\tstatus: no carrier
\tgroups: tap
wlan0: flags=8843<UP,BROADCAST,RUNNING,SIMPLEX,MULTICAST> metric 0 mtu 1500
\tether aa:aa:aa:aa:aa:aa
\tmedia: IEEE 802.11 Wireless Ethernet autoselect (autoselect)
\tstatus: associated
\tgroups: wlan
vtnet0: flags=1008843<UP,BROADCAST,RUNNING,SIMPLEX,MULTICAST,LOWER_UP> metric 0 mtu 1500
\tmedia: Ethernet autoselect
\tstatus: active
ue9x: flags=0<> metric 0 mtu 1500
\tether 02:00:00:00:00:09
\tmedia: Ethernet autoselect
";

    fn nic(addr: &str, driver: &str) -> PciDevice {
        PciDevice {
            addr: addr.parse().unwrap(),
            driver: driver.into(),
            vendor: 0x8086,
            device: 0x1234,
            class: 0x020000,
            description: Some("Intel I219-V".into()),
        }
    }

    #[test]
    fn only_physical_ethernet_cards_are_listed() {
        let devices = [nic("0/25/0", "em0"), nic("2/0/0", "iwn0")];
        let ifaces = parse_ifconfig(IFCONFIG, &devices);
        let names: Vec<&str> = ifaces.iter().map(|i| i.name.as_str()).collect();
        // lo0, bridge0, tap-work have no `media` + physical group; vtnet0
        // has no MAC; `ue9x` is not a USB unit name but is Ethernet.
        assert_eq!(names, ["em0", "ue0", "ue9x", "wlan0"]);

        let em0 = &ifaces[0];
        assert_eq!(em0.kind, IfaceKind::Pci("0/25/0".parse().unwrap()));
        assert_eq!(em0.mac.as_deref(), Some("00:11:22:33:44:55"));
        assert_eq!(em0.link, Some(true));
        assert_eq!(em0.description, "Intel I219-V");
        assert!(em0.usable());

        let ue0 = &ifaces[1];
        assert_eq!(ue0.kind, IfaceKind::Usb);
        assert_eq!(ue0.link, Some(false));
        assert_eq!(ue0.description, "USB Ethernet adapter");

        let other = &ifaces[2];
        assert_eq!((other.kind, other.link), (IfaceKind::Other, None));
        assert_eq!(other.description, "Ethernet");

        let wlan = &ifaces[3];
        assert_eq!((wlan.kind, wlan.link), (IfaceKind::Wifi, Some(true)));
        assert!(!wlan.usable());
    }

    #[test]
    fn an_interface_without_a_mac_or_media_is_not_a_card() {
        let text = "em0: flags=0<> metric 0 mtu 1500\n\tmedia: Ethernet autoselect\n\
                    em1: flags=0<> metric 0 mtu 1500\n\tether 00:00:00:00:00:01\n\
                    \tgarbage before any header is ignored\n";
        assert!(parse_ifconfig(text, &[]).is_empty());
        assert!(parse_ifconfig("\tether 00:00:00:00:00:01\n", &[]).is_empty());
        assert!(parse_ifconfig("", &[]).is_empty());
        // An empty `ether` value is no MAC.
        let blank = "em0: flags=0<>\n\tether \n\tmedia: Ethernet autoselect\n";
        assert!(parse_ifconfig(blank, &[]).is_empty());
        // A line that is not `name: ...` starts no block.
        assert!(parse_ifconfig("no colon here\n\tmedia: x\n", &[]).is_empty());
    }

    #[test]
    fn usb_names_are_ue_and_a_unit() {
        assert!(is_usb_name("ue0") && is_usb_name("ue12"));
        assert!(!is_usb_name("ue") && !is_usb_name("ue0a") && !is_usb_name("em0"));
    }

    #[test]
    fn listing_and_driver_loading_go_through_the_runner() {
        let m = MockRunner::new().respond("ifconfig", &["-a"], CmdOutput::ok(IFCONFIG));
        assert_eq!(list(&m, &[]).len(), 4);
        let failing = MockRunner::new().respond("ifconfig", &["-a"], CmdOutput::failed(1, ""));
        assert!(list(&failing, &[]).is_empty());
        let missing = MockRunner::new().missing("ifconfig");
        assert!(list(&missing, &[]).is_empty());

        let m = MockRunner::new().respond("kldload", &["-n", "if_axe"], CmdOutput::failed(1, "no"));
        load_usb_drivers(&m);
        let calls = m.calls();
        assert_eq!(calls.len(), USB_NIC_MODULES.len());
        assert_eq!(calls.first().map(String::as_str), Some("kldload -n if_axe"));
        assert_eq!(
            calls.last().map(String::as_str),
            Some("kldload -n if_ipheth")
        );
    }

    fn iface(name: &str, link: Option<bool>, kind: IfaceKind) -> NetIface {
        NetIface {
            name: name.into(),
            mac: None,
            link,
            kind,
            description: String::new(),
        }
    }

    #[test]
    fn the_preselected_interface_is_the_one_in_use() {
        let pci = IfaceKind::Pci(PciAddr::new(0, 25, 0));
        let ifaces = [
            iface("em0", Some(false), pci),
            iface("ue0", Some(false), IfaceKind::Usb),
            iface("ue1", Some(true), IfaceKind::Usb),
            iface("wlan0", Some(true), IfaceKind::Wifi),
        ];
        // The default route wins, then a cable, then USB, then order.
        assert_eq!(preferred(&ifaces, Some("em0")), Some(0));
        assert_eq!(preferred(&ifaces, None), Some(2));
        assert_eq!(preferred(&ifaces[..2], None), Some(1));
        assert_eq!(preferred(&ifaces[..1], None), Some(0));
        let down = [iface("em0", None, pci), iface("em1", None, pci)];
        assert_eq!(preferred(&down, None), Some(0));
        // Wi-Fi alone is no uplink; nothing is nothing.
        assert_eq!(preferred(&ifaces[3..], Some("wlan0")), None);
        assert_eq!(preferred(&[], None), None);
    }
}
