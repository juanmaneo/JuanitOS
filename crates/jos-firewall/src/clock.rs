//! Millisecond clock from the TSC, calibrated once against the firmware's
//! `stall` (UEFI offers no monotonic clock API while in boot services).

use core::time::Duration;

use log::info;

pub struct Clock {
    start: u64,
    ticks_per_ms: u64,
}

fn rdtsc() -> u64 {
    // SAFETY: reading the time-stamp counter has no side effects.
    unsafe { core::arch::x86_64::_rdtsc() }
}

impl Clock {
    /// Calibrate against a 50 ms stall. bhyve exposes an invariant TSC,
    /// so this holds for the life of the VM.
    pub fn calibrate() -> Clock {
        const WINDOW_MS: u64 = 50;
        let t0 = rdtsc();
        uefi::boot::stall(Duration::from_millis(WINDOW_MS));
        let t1 = rdtsc();
        let ticks_per_ms = t1
            .saturating_sub(t0)
            .checked_div(WINDOW_MS)
            .unwrap_or(1)
            .max(1);
        info!("tsc: {} ticks/ms", ticks_per_ms);
        Clock {
            start: t1,
            ticks_per_ms,
        }
    }

    pub fn now_ms(&self) -> u64 {
        rdtsc().saturating_sub(self.start) / self.ticks_per_ms
    }
}
