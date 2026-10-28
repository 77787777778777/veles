//! The Racket CS entry points the engine uses (racketcs.h, racketcsboot.h
//! and chezscheme.h of the install `VELES_RACKET_HOME` names), declared
//! by hand: four functions, one struct, one constant. Nothing here
//! builds Scheme data from C — a pointer Chez hands out can move at the
//! next collection, so the engine passes symbols straight into the call
//! that takes them and does everything else from Racket code.
#![allow(non_snake_case)]

use std::os::raw::{c_char, c_int, c_long, c_void};

pub type Ptr = *mut c_void;
/// Chez's `Sfalse` (chezscheme.h: `#define Sfalse ((ptr)0x6)`).
pub const SFALSE: Ptr = 0x6 as Ptr;

/// `racket_boot_arguments_t`, field for field (racketcsboot.h; 224
/// bytes, checked against the C sizeof). An all-zero struct is the
/// documented default for every field.
#[repr(C)]
pub struct BootArgs {
    pub boot1_path: *const c_char,
    pub boot1_data: *mut c_void,
    pub boot1_offset: c_long,
    pub boot1_len: c_long,
    pub boot2_path: *const c_char,
    pub boot2_data: *mut c_void,
    pub boot2_offset: c_long,
    pub boot2_len: c_long,
    pub boot3_path: *const c_char,
    pub boot3_data: *mut c_void,
    pub boot3_offset: c_long,
    pub boot3_len: c_long,
    pub argc: c_int,
    pub argv: *mut *mut c_char,
    pub exec_file: *const c_char,
    pub run_file: *const c_char,
    pub collects_dir: *const c_char,
    pub config_dir: *const c_char,
    /// `/* wchar_t * */void *dll_dir;` in the header: a field hidden
    /// behind a leading comment, which a first reading of the struct
    /// missed; without it everything after `config_dir` is read 8 bytes
    /// off (a boot that worked once by luck, then died in strlen,
    /// 2026-10-10). The C sizeof is 224.
    pub dll_dir: *mut c_void,
    pub k_file: *const c_char,
    pub cs_compiled_subdir: c_int,
    pub segment_offset: c_long,
    pub dll_open: *mut c_void,
    pub dll_find_object: *mut c_void,
    pub dll_close: *mut c_void,
    pub exit_after: c_int,
    pub is_gui: c_int,
    pub wm_is_gracket_or_x11_arg_count: c_int,
    pub gracket_guid_or_x11_args: *mut c_char,
}

extern "C" {
    pub fn racket_boot(args: *mut BootArgs);
    pub fn racket_namespace_require(module_path: Ptr);
    pub fn racket_dynamic_require(module_path: Ptr, sym_or_false: Ptr) -> Ptr;
    pub fn Sstring_to_symbol(s: *const c_char) -> Ptr;
}

/// The C sizeof of `racket_boot_arguments_t` on x86_64/aarch64 Linux
/// (`sizeof` printed by a C program against the install's header).
const _: () = assert!(std::mem::size_of::<BootArgs>() == 224);
