//! The few parts of ECL's C API the engine uses (include/ecl/external.h
//! and object.h of ECL 26.5.5), declared by hand: the engine passes
//! strings in and out and never touches an object's layout.

use std::os::raw::{c_char, c_int, c_void};

/// `cl_object`: a tagged word. Immediates (fixnums, characters, NIL)
/// carry their tag in the low two bits; everything else is a pointer
/// into the GC heap the engine never dereferences.
pub type Obj = *mut c_void;

/// `ECL_NIL` (object.h: `((cl_object)t_list)`, t_list = 1).
pub const NIL: Obj = 1 as Obj;

/// `ecl_make_fixnum` (object.h): `(n << 2) | t_fixnum`, t_fixnum = 3.
pub fn fixnum(n: isize) -> Obj {
    ((n << 2) | 3) as Obj
}

/// `ecl_fixnum`, for an object known to be one.
pub fn fixnum_value(o: Obj) -> Option<isize> {
    let w = o as isize;
    (w & 3 == 3).then_some(w >> 2)
}

/// The `ecl_option` indices the engine sets: the order of the enum in
/// external.h, which build.rs checks against the install it links.
pub mod opt {
    pub const TRAP_SIGSEGV: i32 = 1;
    pub const TRAP_SIGFPE: i32 = 2;
    pub const TRAP_SIGINT: i32 = 3;
    pub const TRAP_SIGILL: i32 = 4;
    pub const TRAP_SIGBUS: i32 = 5;
    pub const TRAP_SIGPIPE: i32 = 6;
    pub const SIGNAL_HANDLING_THREAD: i32 = 8;
    pub const BOOTED: i32 = 10;
    pub const HEAP_SIZE: i32 = 19;
}

extern "C" {
    pub fn cl_boot(argc: c_int, argv: *mut *mut c_char) -> c_int;
    pub fn ecl_set_option(option: c_int, value: isize);
    pub fn ecl_get_option(option: c_int) -> isize;
    /// Define `sym` as a Lisp function of `narg` arguments implemented by
    /// `f` (a `cl_objectfn_fixed`).
    pub fn ecl_def_c_function(sym: Obj, f: *const c_void, narg: c_int);
    /// Intern `name` in `package` (which must exist).
    pub fn ecl_make_symbol(name: *const c_char, package: *const c_char) -> Obj;
    /// `(si:safe-eval form env error-value)`: the value, or `error-value`
    /// when the evaluation signalled an error it did not handle.
    pub fn si_safe_eval(narg: c_int, form: Obj, env: Obj, ...) -> Obj;
    /// `(si:string-to-object string)`: the reader on a string.
    pub fn si_string_to_object(narg: c_int, s: Obj, ...) -> Obj;
    /// A fresh simple base string holding a copy of `len` bytes of `s`.
    pub fn ecl_make_simple_base_string(s: *const c_char, len: isize) -> Obj;
    /// The bytes of a simple base string (NUL-terminated by ECL).
    pub fn ecl_base_string_pointer_safe(s: Obj) -> *mut c_char;
    /// T when `x` is a base string.
    pub fn si_base_string_p(x: Obj) -> Obj;
    /// glibc: the stack size (among others) of threads created with
    /// default attributes, ECL's own among them.
    pub fn pthread_setattr_default_np(attr: *const libc::pthread_attr_t) -> c_int;
}
