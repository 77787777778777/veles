//! Abstract Unix sockets and who is on the other end of one.
//!
//! Abstract (`@name`), not a path: the two apps share no directory
//! either could put a socket file in, and an abstract name needs none.
//! Anyone in the network namespace can connect to one, or bind it first,
//! which is why both ends check the other's uid ([`peer_uid`]).

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};

#[cfg(target_os = "android")]
use std::os::android::net::SocketAddrExt;
#[cfg(target_os = "linux")]
use std::os::linux::net::SocketAddrExt;

/// Listens on `@name`.
pub fn listen(name: &str) -> io::Result<UnixListener> {
    UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes())?)
}

/// Connects to `@name`.
pub fn connect(name: &str) -> io::Result<UnixStream> {
    UnixStream::connect_addr(&SocketAddr::from_abstract_name(name.as_bytes())?)
}

/// The uid of the process at the other end (`SO_PEERCRED`): the kernel's
/// record of who connected (or who listened), which the peer cannot set.
pub fn peer_uid(s: &UnixStream) -> io::Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// This process's real uid.
pub fn my_uid() -> u32 {
    unsafe { libc::getuid() }
}
