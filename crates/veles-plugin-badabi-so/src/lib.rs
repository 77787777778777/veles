//! The plugin that must be REFUSED: an ABI 3 build, as far as the host
//! can tell. It exports `veles_plugin_abi_version` answering 3 and an
//! ABI 3-shaped `veles_plugin_init` the host must never call — reading
//! an ABI 4 table through the ABI 3 layout would be UB, which is why
//! the host reads the version export first and stops there.
//!
//! The refusal path has a test (`a_plugin_that_refuses_the_abi_version_is_not_loaded`
//! in crates/veles-core/tests/plugin_so.rs): the message names the file,
//! the ABI it found, the ABI the host loads, and the rebuild. To prove
//! init never ran, it writes a marker file when `VELES_BADABI_MARK`
//! names one; the test asserts the file does not appear.
//!
//! Not on the SDK on purpose: the SDK can only build ABI 4 plugins.

use std::ffi::{c_char, c_void, CString};

/// What this plugin claims to be.
pub const CLAIMED_ABI: u32 = 3;

#[no_mangle]
pub extern "C" fn veles_plugin_abi_version() -> u32 {
    CLAIMED_ABI
}

/// ABI 3's init shape: `(host) -> i32`. Must never be reached.
///
/// # Safety
/// Never called by an ABI 4 host; an ABI 3 host would pass its table.
#[no_mangle]
pub unsafe extern "C" fn veles_plugin_init(_host: *const c_void) -> i32 {
    if let Ok(mark) = std::env::var("VELES_BADABI_MARK") {
        let _ = std::fs::write(mark, "badabi init RAN — the host read a v3 plugin's init\n");
    }
    0
}

/// # Safety
/// `s` is NULL or a string this plugin returned.
#[no_mangle]
pub unsafe extern "C" fn veles_plugin_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}
