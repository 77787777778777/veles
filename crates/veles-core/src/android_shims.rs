//! libc functions newer than the API level veles builds Android for (24).
//!
//! Core is linked into libveles_runtime.so, a dylib, and a dylib keeps
//! every function it exports, including dependencies' wrappers around
//! libc calls nothing in veles makes. `nix::unistd::syncfs` (nix 0.28,
//! through rustyline) calls `syncfs`, which bionic only has from API 28,
//! so the engine's link fails ("undefined reference due to
//! --no-allow-shlib-undefined: syncfs"); a statically linked engine never
//! saw it, because the linker dropped the unused function. (rustix makes
//! the system call itself for the same reason.) The kernel has the call
//! on every Android, so the definition here makes it, as bionic's does.

/// `int syncfs(int fd)`, as bionic 28+ defines it.
#[no_mangle]
pub extern "C" fn syncfs(fd: libc::c_int) -> libc::c_int {
    // SAFETY: the syscall takes one integer and touches no memory of ours.
    unsafe { libc::syscall(libc::SYS_syncfs, fd) as libc::c_int }
}
