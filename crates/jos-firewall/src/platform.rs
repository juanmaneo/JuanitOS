//! Firmware glue: device lookup by PCI slot, the config disk, reset and
//! idle halt. The only `unsafe` in the unikernel lives here.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use log::info;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams};
use uefi::proto::device_path::{DevicePath, DeviceSubType, DeviceType};
use uefi::proto::media::block::BlockIO;
use uefi::proto::ProtocolPointer;
use uefi::runtime::{self, ResetType};
use uefi::{Handle, Status};

/// Borrow a protocol on `handle` without claiming exclusivity, so the
/// firmware driver that produced it keeps working.
fn get_protocol<P: ProtocolPointer + ?Sized>(handle: Handle) -> Option<boot::ScopedProtocol<P>> {
    let params = OpenProtocolParams {
        handle,
        agent: boot::image_handle(),
        controller: None,
    };
    // SAFETY: GetProtocol does not take ownership and we never hold the
    // returned reference across a call that could reinstall the protocol.
    unsafe { boot::open_protocol::<P>(params, OpenProtocolAttributes::GetProtocol) }.ok()
}

/// PCI slot (device number) of the device behind `handle`, from the
/// first `Hardware/PCI` node of its device path.
pub fn pci_slot_of(handle: Handle) -> Option<u8> {
    let path = get_protocol::<DevicePath>(handle)?;
    for node in path.node_iter() {
        if node.full_type() == (DeviceType::HARDWARE, DeviceSubType::HARDWARE_PCI) {
            // Node layout: header (4 bytes), function u8, device u8.
            return node.data().get(1).copied();
        }
    }
    None
}

/// Read the NUL-terminated config text from the raw disk at PCI `slot`.
pub fn read_config_disk(slot: u32) -> Result<String, String> {
    let want = u8::try_from(slot).map_err(|_| format!("bad config slot {slot}"))?;
    let handles =
        boot::find_handles::<BlockIO>().map_err(|e| format!("enumerating BlockIO handles: {e}"))?;
    let handle = handles
        .iter()
        .copied()
        .find(|h| pci_slot_of(*h) == Some(want))
        .ok_or_else(|| format!("no config disk at PCI slot {slot}"))?;
    let mut blk = get_protocol::<BlockIO>(handle).ok_or("config disk: cannot open BlockIO")?;
    let media = blk.media();
    let media_id = media.media_id();
    let block_size = usize::try_from(media.block_size()).map_err(|_| "block size")?;
    if block_size == 0 {
        return Err(String::from("config disk: zero block size"));
    }
    let disk_bytes = media
        .last_block()
        .saturating_add(1)
        .saturating_mul(u64::from(media.block_size()));
    let want_bytes = u64::try_from(jos_fwcfg::MAX_CONFIG_BYTES).unwrap_or(u64::MAX);
    let len = usize::try_from(disk_bytes.min(want_bytes)).map_err(|_| "disk size")?;
    let len = len.div_ceil(block_size).saturating_mul(block_size);
    let mut buf: Vec<u8> = alloc::vec![0u8; len];
    blk.read_blocks(media_id, 0, &mut buf)
        .map_err(|e| format!("config disk: read: {e}"))?;
    let text_len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    buf.truncate(text_len);
    info!("config disk: {} bytes of text at slot {slot}", buf.len());
    String::from_utf8(buf).map_err(|_| String::from("config disk: not UTF-8"))
}

/// Turn off the boot-services watchdog. The firmware arms it for five
/// minutes before starting a boot option and resets the machine when it
/// fires, unless the application exits boot services, which this one
/// never does.
pub fn disable_watchdog() -> Result<(), String> {
    boot::set_watchdog_timer(0, 0x10000, None).map_err(|e| format!("disabling the watchdog: {e}"))
}

/// Log, wait so the console can be read, then cold-reset the VM. bhyve
/// reports the reset as a reboot and the `jos` supervisor restarts us.
pub fn reboot_after(delay: Duration) -> ! {
    boot::stall(delay);
    runtime::reset(ResetType::COLD, Status::SUCCESS, None)
}

/// Sleep until the next interrupt (the firmware timer tick at the latest).
pub fn halt_until_interrupt() {
    // SAFETY: `hlt` only pauses the CPU; UEFI boot services keep the
    // timer interrupt enabled so execution resumes within one tick.
    unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
}
