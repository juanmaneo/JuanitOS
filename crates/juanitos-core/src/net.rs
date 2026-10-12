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

    fn attach_tap(&self, tap: &str, bridge: &str) -> Result<()> {
        if !self.iface_exists(tap)? {
            self.runner
                .run_checked("ifconfig", &["tap", "create", "name", tap])?;
        }
        self.runner.run_checked("ifconfig", &[tap, "up"])?;
        self.runner
            .run_checked("ifconfig", &[bridge, "addm", tap])?;
        Ok(())
    }

    /// Create the upstream tap for `qube`, attach it to `netvm`'s bridge and
    /// return the tap name.
    pub fn create_tap(&self, qube: &str, netvm: &str) -> Result<String> {
        let br = self.ensure_bridge(netvm)?;
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
                "ifconfig br-sys-net addm tap-work",
            ]
        );
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
