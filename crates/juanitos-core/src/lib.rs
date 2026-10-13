//! `juanitos-core` — the library behind the `jos` CLI.
//!
//! JuanitOS runs isolated virtual machines ("qubes") on FreeBSD/bhyve with
//! ZFS zvol storage, modelled on QubesOS. This crate is pure Rust on `std`,
//! free of Unix-only APIs so it builds and tests on any development host;
//! all host interaction goes through [`sys::Runner`].

pub mod bhyve;
pub mod config;
pub mod console;
pub mod doctor;
pub mod dom0;
pub mod error;
pub mod firewall;
pub mod fsops;
pub mod net;
pub mod pci;
pub mod release;
pub mod supervisor;
pub mod sys;
pub mod template;
pub mod zfs;

pub use bhyve::GuestExit;
pub use config::{GlobalConfig, OsType, Qube, QubeKind, QubeStore};
pub use error::{Error, Result};
pub use pci::{PciAddr, PciDevice};
pub use sys::{CmdOutput, DryRunner, MockRunner, RealRunner, Runner};
