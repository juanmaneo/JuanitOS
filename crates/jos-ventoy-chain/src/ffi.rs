// SPDX-License-Identifier: BSD-3-Clause
//! The kernel side: `jos_ventoy_fill_map`, called from `g_ventoy.c`, and
//! what it needs from the kernel. Only built for `x86_64-unknown-none`
//! (`jos-release` emits the crate as one object for `geom_ventoy.ko`);
//! the parsing it wraps is tested on the host.

#![allow(unsafe_code)]

use core::ffi::{c_char, c_void};

use crate::{fill, Error, Map, PRELOAD_TYPE};

// FreeBSD's <sys/linker.h>: the loader's preloaded files, by MODINFO_TYPE.
extern "C" {
    fn preload_search_by_type(ty: *const c_char) -> *mut c_char;
    fn preload_fetch_addr(entry: *mut c_char) -> *mut c_void;
    fn preload_fetch_size(entry: *mut c_char) -> usize;
    #[link_name = "panic"]
    fn kernel_panic(fmt: *const c_char, ...) -> !;
}

/// Fill the module's map from the preloaded chain blob. Returns the
/// number of segments, 0 when GRUB preloaded no blob, and an
/// [`Error::code`](crate::Error::code) (negative) for a malformed one.
///
/// # Safety
///
/// `map` must point to the module's `g_ventoy_map_data`.
#[no_mangle]
pub unsafe extern "C" fn jos_ventoy_fill_map(map: *mut Map) -> i32 {
    let entry = preload_search_by_type(PRELOAD_TYPE.as_ptr());
    if entry.is_null() {
        return 0;
    }
    let addr = preload_fetch_addr(entry).cast::<u8>();
    let size = preload_fetch_size(entry);
    if addr.is_null() || size == 0 {
        return 0;
    }
    let blob = core::slice::from_raw_parts(addr, size);
    match fill(blob, &mut *map) {
        Ok(n) => n as i32,
        Err(e) => e.code(),
    }
}

/// The message for a negative return of [`jos_ventoy_fill_map`] (a static
/// C string), NULL for anything else.
#[no_mangle]
pub extern "C" fn jos_ventoy_chain_error(code: i32) -> *const c_char {
    Error::from_code(code).map_or(core::ptr::null(), |e| e.message().as_ptr())
}

/// Nothing in [`fill`] can panic; should that ever change, the kernel's
/// own panic is the only sensible outcome.
#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { kernel_panic(c"jos_ventoy_chain: unexpected panic".as_ptr()) }
}
