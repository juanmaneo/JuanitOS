//! A NIC behind the firmware's `SimpleNetwork` protocol, selected by PCI
//! slot so the upstream/downstream roles follow the bhyve `-s` layout.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use jos_fwcfg::MacAddr;
use log::{info, warn};
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::network::snp::{NetworkState, ReceiveFlags, SimpleNetwork};
use uefi::{Handle, Status};

use crate::platform::pci_slot_of;

pub struct Nic {
    slot: u8,
    snp: ScopedProtocol<SimpleNetwork>,
    mac: MacAddr,
    /// Frames handed to the firmware; it owns them until it reports them
    /// recycled, so they must stay allocated (and unmoved) until then.
    in_flight: Vec<Vec<u8>>,
}

impl Nic {
    pub fn open(slot: u8) -> Result<Nic, String> {
        let handle = find_handle(slot)?;
        let mut snp = boot::open_protocol_exclusive::<SimpleNetwork>(handle)
            .map_err(|e| format!("slot {slot}: open SimpleNetwork: {e}"))?;
        let state = snp.mode().state;
        if state == NetworkState::STOPPED {
            snp.start()
                .map_err(|e| format!("slot {slot}: start: {e}"))?;
        }
        if state != NetworkState::INITIALIZED {
            snp.initialize(0, 0)
                .map_err(|e| format!("slot {slot}: initialize: {e}"))?;
        }
        if let Err(e) = snp.receive_filters(
            ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST,
            ReceiveFlags::empty(),
            false,
            None,
        ) {
            warn!("slot {slot}: receive_filters: {e} (continuing)");
        }
        let mode = snp.mode();
        let mut mac = [0u8; 6];
        if let Some(bytes) = mode.current_address.0.get(..6) {
            mac.copy_from_slice(bytes);
        }
        if mode.media_header_size != 14 {
            return Err(format!(
                "slot {slot}: unexpected media header size {}",
                mode.media_header_size
            ));
        }
        info!(
            "slot {slot}: SimpleNetwork ready, max packet {}",
            mode.max_packet_size
        );
        Ok(Nic {
            slot,
            snp,
            mac: MacAddr(mac),
            in_flight: Vec::new(),
        })
    }

    pub fn mac(&self) -> MacAddr {
        self.mac
    }

    /// Non-blocking receive into `buf`; `Ok(None)` when nothing is pending.
    pub fn receive(&mut self, buf: &mut [u8]) -> Result<Option<usize>, String> {
        match self.snp.receive(buf, None, None, None, None) {
            Ok(n) => Ok(Some(n)),
            Err(e) if e.status() == Status::NOT_READY => Ok(None),
            Err(e) => Err(format!("slot {}: receive: {e}", self.slot)),
        }
    }

    /// Queue a frame for transmission. Ownership stays here until the
    /// firmware recycles the buffer (see [`Nic::reclaim`]).
    pub fn transmit(&mut self, mut frame: Vec<u8>) -> Result<(), String> {
        let min = usize::try_from(self.snp.mode().media_header_size).unwrap_or(14);
        if frame.len() < min {
            return Ok(());
        }
        match self.snp.transmit(0, &mut frame, None, None, None) {
            Ok(()) => {
                self.in_flight.push(frame);
                Ok(())
            }
            Err(e) if e.status() == Status::NOT_READY => {
                // Transmit ring full: drop the frame rather than block the
                // other direction. Upper layers retransmit.
                Ok(())
            }
            Err(e) => Err(format!("slot {}: transmit: {e}", self.slot)),
        }
    }

    /// Free buffers the firmware has finished transmitting.
    pub fn reclaim(&mut self) -> Result<(), String> {
        if self.in_flight.is_empty() {
            return Ok(());
        }
        loop {
            match self.snp.get_recycled_transmit_buffer_status() {
                Ok(Some(ptr)) => {
                    let p = ptr.as_ptr().cast_const();
                    if let Some(i) = self.in_flight.iter().position(|b| b.as_ptr() == p) {
                        self.in_flight.swap_remove(i);
                    }
                }
                Ok(None) => return Ok(()),
                Err(e) => return Err(format!("slot {}: recycle: {e}", self.slot)),
            }
        }
    }
}

/// Handle of the `SimpleNetwork` instance sitting at PCI `slot`.
fn find_handle(slot: u8) -> Result<Handle, String> {
    let handles = boot::find_handles::<SimpleNetwork>()
        .map_err(|e| format!("enumerating SimpleNetwork handles: {e}"))?;
    if handles.is_empty() {
        return Err(String::from(
            "no SimpleNetwork devices: is the bootrom built with VirtioNetDxe?",
        ));
    }
    for h in &handles {
        match pci_slot_of(*h) {
            Some(s) if s == slot => return Ok(*h),
            Some(s) => info!("SimpleNetwork at slot {s} (not {slot})"),
            None => warn!("SimpleNetwork handle without a PCI device path"),
        }
    }
    Err(format!("no NIC at PCI slot {slot}"))
}
