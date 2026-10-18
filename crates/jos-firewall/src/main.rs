//! `jos-firewall` — the JuanitOS `sys-firewall` unikernel.
//!
//! A UEFI application that never leaves boot services: the firmware keeps
//! owning the virtio devices (EDK2's `VirtioNetDxe`/`VirtioBlkDxe`) and
//! exposes them as `SimpleNetwork` and `BlockIO` protocols, so this binary
//! contains no DMA code and almost no `unsafe`. All packet logic lives in
//! `jos-fwnet`, which is tested on the development host.
//!
//! Guest layout (see `docs/unikernel-firewall.md`):
//!
//! - PCI slot 3: upstream NIC (towards `sys-net`)
//! - PCI slot 4: downstream NIC (the clients' bridge)
//! - PCI slot 30: raw config disk rendered by `jos`
//! - `com1`: this log, readable with `jos console sys-firewall`
//!
//! A panic or a fatal initialisation error resets the VM; the `jos`
//! supervisor then restarts the qube, which is also how configuration
//! changes are applied.

#![no_std]
#![no_main]

extern crate alloc;

mod clock;
mod nic;
mod platform;

use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use jos_fwnet::config::Config;
use jos_fwnet::engine::{Engine, Output};
use jos_fwnet::packet::MAX_FRAME;
use log::{error, info, warn};
use uefi::prelude::*;

use crate::nic::Nic;

const UPSTREAM_SLOT: u8 = 3;
const DOWNSTREAM_SLOT: u8 = 4;
/// Halt the CPU after this long without traffic (keeps a 1-vCPU qube from
/// burning a host core while still polling at line rate under load).
const IDLE_BEFORE_HALT_MS: u64 = 2;
const STATS_EVERY_MS: u64 = 60_000;
/// During the first minute report more often: that is when a
/// misconfiguration shows (`jos console sys-firewall` right after start).
const EARLY_STATS_EVERY_MS: u64 = 10_000;

#[entry]
fn main() -> Status {
    if uefi::helpers::init().is_err() {
        return Status::LOAD_ERROR;
    }
    info!("jos-firewall {} starting", env!("CARGO_PKG_VERSION"));
    if let Err(e) = platform::disable_watchdog() {
        warn!("{e}: the firmware will reset this qube in five minutes");
    }
    match run() {
        Ok(()) => Status::SUCCESS,
        Err(e) => {
            error!("fatal: {e}");
            platform::reboot_after(Duration::from_secs(5))
        }
    }
}

fn run() -> Result<(), String> {
    let config_text = platform::read_config_disk(jos_fwcfg::CONFIG_DISK_SLOT)?;
    let cfg = Config::parse(&config_text).map_err(|e| alloc::format!("config disk: {e}"))?;
    info!(
        "config: firewall `{}`, gateway {}, {} client(s), upstream {}",
        cfg.name,
        cfg.gateway,
        cfg.clients.len(),
        if cfg.upstream_dhcp { "dhcp" } else { "static" }
    );
    for c in &cfg.clients {
        info!(
            "  client {} {} {} policy {} ({} rules)",
            c.name,
            c.ip,
            c.mac,
            c.policy,
            c.rules.len()
        );
    }

    let mut up = Nic::open(UPSTREAM_SLOT)?;
    let mut down = Nic::open(DOWNSTREAM_SLOT)?;
    info!("upstream NIC {} (slot {UPSTREAM_SLOT})", up.mac());
    info!("downstream NIC {} (slot {DOWNSTREAM_SLOT})", down.mac());

    let mut engine = Engine::new(cfg, up.mac(), down.mac());
    let clock = clock::Clock::calibrate();
    let mut out: Vec<Output> = Vec::new();
    let mut rx = alloc::vec![0u8; MAX_FRAME + 64];
    let mut last_activity = clock.now_ms();
    let mut last_stats = last_activity;
    let mut was_bound = false;

    loop {
        let now = clock.now_ms();
        let mut busy = false;

        engine.tick(now, &mut out);
        while let Some(n) = up.receive(&mut rx)? {
            busy = true;
            if let Some(frame) = rx.get(..n) {
                engine.handle_upstream(frame, now, &mut out);
            }
        }
        while let Some(n) = down.receive(&mut rx)? {
            busy = true;
            if let Some(frame) = rx.get(..n) {
                engine.handle_downstream(frame, now, &mut out);
            }
        }
        for o in out.drain(..) {
            busy = true;
            match o {
                Output::Upstream(f) => up.transmit(f)?,
                Output::Downstream(f) => down.transmit(f)?,
            }
        }
        up.reclaim()?;
        down.reclaim()?;

        if engine.up.is_bound() != was_bound {
            was_bound = engine.up.is_bound();
            if was_bound {
                info!(
                    "upstream bound: {} via {} ({}), dns {:?}",
                    engine
                        .up
                        .ip
                        .map(|ip| alloc::format!("{ip}"))
                        .unwrap_or_default(),
                    engine
                        .up
                        .router
                        .map(|ip| alloc::format!("{ip}"))
                        .unwrap_or_default(),
                    engine
                        .up
                        .router_mac
                        .map(|m| alloc::format!("{m}"))
                        .unwrap_or_default(),
                    engine.up.dns
                );
            } else {
                warn!("upstream lost; re-acquiring lease");
            }
        }
        let stats_every = if now < STATS_EVERY_MS {
            EARLY_STATS_EVERY_MS
        } else {
            STATS_EVERY_MS
        };
        if now.saturating_sub(last_stats) >= stats_every {
            last_stats = now;
            let s = engine.stats;
            info!(
                "stats: out {} in {} denied {} spoofed {} unknown {} unsolicited {} \
                 no_route {} nat_full {} malformed {} nat {} flows",
                s.forwarded_out,
                s.forwarded_in,
                s.denied,
                s.spoofed,
                s.unknown_client,
                s.unsolicited_in,
                s.no_route,
                s.nat_full,
                s.malformed,
                engine.nat.len()
            );
        }

        if busy {
            last_activity = now;
        } else if now.saturating_sub(last_activity) >= IDLE_BEFORE_HALT_MS {
            platform::halt_until_interrupt();
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    error!("panic: {info}");
    platform::reboot_after(Duration::from_secs(5))
}
