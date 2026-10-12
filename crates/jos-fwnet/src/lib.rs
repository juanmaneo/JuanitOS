//! Packet-level logic for the `jos-firewall` unikernel.
//!
//! Everything in here is `no_std` + `alloc`, free of `unsafe`, and takes
//! plain byte slices, so the unikernel is reduced to "driver glue + this
//! crate" and the interesting code is unit-tested on the development host.
//!
//! - [`packet`]: Ethernet / IPv4 / TCP / UDP / ICMP views, checksums and
//!   in-place header rewrites.
//! - [`arp`], [`dhcp`]: the two control protocols a gateway needs.
//! - [`config`]: the config disk ([`jos_fwcfg`]) turned into a client table.
//! - [`nat`]: a port-restricted cone NAT table with expiry.
//! - [`engine`]: the actual firewall: feeds frames from either side through
//!   filter + NAT and produces frames for the other side.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod arp;
pub mod config;
pub mod dhcp;
pub mod engine;
pub mod nat;
pub mod packet;

pub use jos_fwcfg as fwcfg;
