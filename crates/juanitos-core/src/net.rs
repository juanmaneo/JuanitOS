//! Host-side networking: one bridge per NetVM/FirewallVM, one tap per qube.
//!
//! Interface names on FreeBSD are limited to `IFNAMSIZ - 1` = 15 characters.

use crate::error::Result;
use crate::sys::Runner;

pub const IFNAME_MAX: usize = 15;

fn truncate_ifname(prefix: &str, name: &str) -> String {
    let mut s = format!("{prefix}{name}");
    s.truncate(IFNAME_MAX);
    s
}

/// `tap-<qube>`, truncated to a valid interface name. This is the qube's
/// *upstream* NIC, attached to its NetVM's bridge.
pub fn tap_name(qube: &str) -> String {
    truncate_ifname("tap-", qube)
}

/// `tapd-<qube>`: the *downstream* NIC of a net-providing qube, attached to
/// the qube's own bridge where its clients plug in.
pub fn downstream_tap_name(qube: &str) -> String {
    truncate_ifname("tapd-", qube)
}

/// `br-<netvm>`, truncated to a valid interface name.
pub fn bridge_name(netvm: &str) -> String {
    truncate_ifname("br-", netvm)
}

/// `bru-<iface>`: the bridge joining a host interface (`uplink`) to the
/// upstream taps of the qubes using it. Per interface, not per qube: an
/// interface can only be a member of one bridge.
pub fn uplink_bridge_name(iface: &str) -> String {
    truncate_ifname("bru-", iface)
}

pub struct Net<'a> {
    pub runner: &'a dyn Runner,
}

impl<'a> Net<'a> {
    pub fn new(runner: &'a dyn Runner) -> Self {
        Net { runner }
    }

    fn iface_exists(&self, name: &str) -> Result<bool> {
        Ok(self.runner.run("ifconfig", &[name])?.success())
    }

    /// Create (if needed) and bring up the bridge for `netvm`.
    pub fn ensure_bridge(&self, netvm: &str) -> Result<String> {
        let br = bridge_name(netvm);
        if !self.iface_exists(&br)? {
            self.runner
                .run_checked("ifconfig", &["bridge", "create", "name", &br])?;
        }
        self.runner.run_checked("ifconfig", &[&br, "up"])?;
        Ok(br)
    }

    /// `addm` of a member already present fails ("File exists"): check
    /// the bridge's member list first.
    fn add_member(&self, bridge: &str, iface: &str) -> Result<()> {
        let members = self.runner.run("ifconfig", &[bridge])?.stdout;
        let present = members.lines().any(|l| {
            let l = l.trim();
            l.strip_prefix("member:")
                .is_some_and(|rest| rest.split_whitespace().next() == Some(iface))
        });
        if !present {
            self.runner
                .run_checked("ifconfig", &[bridge, "addm", iface])?;
        }
        Ok(())
    }

    fn attach_tap(&self, tap: &str, bridge: &str) -> Result<()> {
        if !self.iface_exists(tap)? {
            self.runner
                .run_checked("ifconfig", &["tap", "create", "name", tap])?;
        }
        self.runner.run_checked("ifconfig", &[tap, "up"])?;
        self.add_member(bridge, tap)
    }

    /// Create the upstream tap for `qube`, attach it to `netvm`'s bridge and
    /// return the tap name.
    pub fn create_tap(&self, qube: &str, netvm: &str) -> Result<String> {
        let br = self.ensure_bridge(netvm)?;
        let tap = tap_name(qube);
        self.attach_tap(&tap, &br)?;
        Ok(tap)
    }

    /// Create the upstream tap for `qube` on a bridge shared with the host
    /// interface `iface` (no NetVM in between) and return the tap name.
    pub fn create_uplink_tap(&self, qube: &str, iface: &str) -> Result<String> {
        let br = uplink_bridge_name(iface);
        if !self.iface_exists(&br)? {
            self.runner
                .run_checked("ifconfig", &["bridge", "create", "name", &br])?;
        }
        self.runner.run_checked("ifconfig", &[iface, "up"])?;
        self.add_member(&br, iface)?;
        self.runner.run_checked("ifconfig", &[&br, "up"])?;
        let tap = tap_name(qube);
        self.attach_tap(&tap, &br)?;
        Ok(tap)
    }

    /// Create the downstream tap for a net-providing `qube`, attached to the
    /// qube's own bridge, and return the tap name.
    pub fn create_downstream_tap(&self, qube: &str) -> Result<String> {
        let br = self.ensure_bridge(qube)?;
        let tap = downstream_tap_name(qube);
        self.attach_tap(&tap, &br)?;
        Ok(tap)
    }

    fn destroy_iface(&self, name: &str) -> Result<()> {
        if self.iface_exists(name)? {
            self.runner.run_checked("ifconfig", &[name, "destroy"])?;
        }
        Ok(())
    }

    /// Remove the upstream tap for `qube`; a missing tap is not an error.
    pub fn destroy_tap(&self, qube: &str) -> Result<()> {
        self.destroy_iface(&tap_name(qube))
    }

    /// Remove both taps of `qube` (upstream and, if any, downstream).
    pub fn destroy_taps(&self, qube: &str) -> Result<()> {
        self.destroy_iface(&tap_name(qube))?;
        self.destroy_iface(&downstream_tap_name(qube))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{CmdOutput, MockRunner};

    #[test]
    fn names_are_bounded() {
        assert_eq!(tap_name("work"), "tap-work");
        assert_eq!(bridge_name("sys-net"), "br-sys-net");
        assert_eq!(downstream_tap_name("sys-net"), "tapd-sys-net");
        let long = "a-very-long-qube-name-indeed";
        assert_eq!(tap_name(long).len(), IFNAME_MAX);
        assert_eq!(bridge_name(long).len(), IFNAME_MAX);
        assert_eq!(downstream_tap_name(long).len(), IFNAME_MAX);
        assert_eq!(tap_name(long), "tap-a-very-long");
        assert_ne!(tap_name(long), downstream_tap_name(long));
    }

    #[test]
    fn uplink_tap_bridges_the_host_interface_once() {
        let m = MockRunner::new()
            .respond("ifconfig", &["bru-em0"], CmdOutput::failed(1, ""))
            .respond("ifconfig", &["bru-em0", "up"], CmdOutput::ok(""))
            .respond("ifconfig", &["bru-em0", "addm"], CmdOutput::ok(""))
            .respond("ifconfig", &["tap-sys-net"], CmdOutput::failed(1, ""))
            .respond("ifconfig", &["tap-sys-net", "up"], CmdOutput::ok(""));
        let net = Net::new(&m);
        assert_eq!(
            net.create_uplink_tap("sys-net", "em0").unwrap(),
            "tap-sys-net"
        );
        let calls = m.calls();
        assert!(calls.contains(&"ifconfig bridge create name bru-em0".to_string()));
        assert!(calls.contains(&"ifconfig em0 up".to_string()));
        assert!(calls.contains(&"ifconfig bru-em0 addm em0".to_string()));
        assert!(calls.contains(&"ifconfig bru-em0 addm tap-sys-net".to_string()));

        // A second qube on the same uplink: the bridge exists with em0 in
        // it (not added again), only the new tap joins.
        let m = MockRunner::new().respond(
            "ifconfig",
            &["bru-em0"],
            CmdOutput::ok("bru-em0: flags=...\n\tmember: em0 flags=143<LEARNING>\n"),
        );
        assert_eq!(
            Net::new(&m).create_uplink_tap("work", "em0").unwrap(),
            "tap-work"
        );
        assert!(!m.calls().contains(&"ifconfig bru-em0 addm em0".to_string()));
        assert!(m
            .calls()
            .contains(&"ifconfig bru-em0 addm tap-work".to_string()));
        assert_eq!(
            uplink_bridge_name("a-very-long-qube-name").len(),
            IFNAME_MAX
        );
    }

    #[test]
    fn downstream_tap_uses_own_bridge() {
        let m = MockRunner::new()
            .respond("ifconfig", &["br-sys-fw"], CmdOutput::failed(1, ""))
            .respond("ifconfig", &["br-sys-fw", "up"], CmdOutput::ok(""))
            .respond("ifconfig", &["br-sys-fw", "addm"], CmdOutput::ok(""));
        let net = Net::new(&m);
        assert_eq!(net.create_downstream_tap("sys-fw").unwrap(), "tapd-sys-fw");
        let calls = m.calls();
        assert!(calls.contains(&"ifconfig bridge create name br-sys-fw".to_string()));
        assert!(calls.contains(&"ifconfig br-sys-fw addm tapd-sys-fw".to_string()));
    }

    #[test]
    fn create_tap_commands() {
        let m = MockRunner::new()
            .respond(
                "ifconfig",
                &["br-sys-net"],
                CmdOutput::failed(1, "no such iface"),
            )
            .respond(
                "ifconfig",
                &["tap-work"],
                CmdOutput::failed(1, "no such iface"),
            )
            // `ifconfig X up` etc. match the longer prefixes below and succeed.
            .respond("ifconfig", &["br-sys-net", "up"], CmdOutput::ok(""))
            .respond("ifconfig", &["br-sys-net", "addm"], CmdOutput::ok(""))
            .respond("ifconfig", &["tap-work", "up"], CmdOutput::ok(""));
        let net = Net::new(&m);
        assert_eq!(net.create_tap("work", "sys-net").unwrap(), "tap-work");
        assert_eq!(
            m.calls(),
            vec![
                "ifconfig br-sys-net",
                "ifconfig bridge create name br-sys-net",
                "ifconfig br-sys-net up",
                "ifconfig tap-work",
                "ifconfig tap create name tap-work",
                "ifconfig tap-work up",
                "ifconfig br-sys-net",
                "ifconfig br-sys-net addm tap-work",
            ]
        );
    }

    #[test]
    fn tap_already_on_the_bridge_is_not_added_again() {
        // A tap left behind by an interrupted run is still a member; a
        // second `addm` would fail with "File exists". Names are matched
        // whole: `tap-work` is not `tap-work2`.
        let m = MockRunner::new().respond(
            "ifconfig",
            &["br-sys-net"],
            CmdOutput::ok(
                "br-sys-net: flags=...\n\tmember: tap-work2 flags=143<LEARNING>\n\
                 \tmember: tap-work flags=143<LEARNING>\n",
            ),
        );
        Net::new(&m).create_tap("work", "sys-net").unwrap();
        assert!(
            !m.calls().iter().any(|c| c.contains("addm")),
            "{:?}",
            m.calls()
        );
        let m = MockRunner::new().respond(
            "ifconfig",
            &["br-sys-net"],
            CmdOutput::ok("br-sys-net: flags=...\n\tmember: tap-work2 flags=143<LEARNING>\n"),
        );
        Net::new(&m).create_tap("work", "sys-net").unwrap();
        assert!(m
            .calls()
            .contains(&"ifconfig br-sys-net addm tap-work".to_string()));
    }

    #[test]
    fn destroy_missing_tap_is_noop() {
        let m = MockRunner::new().respond("ifconfig", &["tap-work"], CmdOutput::failed(1, ""));
        Net::new(&m).destroy_tap("work").unwrap();
        assert_eq!(m.calls(), vec!["ifconfig tap-work"]);
    }

    #[test]
    fn destroy_taps_removes_both() {
        let m = MockRunner::new();
        Net::new(&m).destroy_taps("sys-net").unwrap();
        assert_eq!(
            m.calls(),
            vec![
                "ifconfig tap-sys-net",
                "ifconfig tap-sys-net destroy",
                "ifconfig tapd-sys-net",
                "ifconfig tapd-sys-net destroy",
            ]
        );
    }
}
