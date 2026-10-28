//! The broker's wire: frames, the messages in them, and descriptors
//! riding along.
//!
//! A frame is a 4-byte big-endian length and that many bytes of JSON.
//! Descriptors travel as `SCM_RIGHTS` ancillary data on the frame's
//! first `sendmsg`, so they arrive with the frame's header and nothing
//! else. A conversation:
//!
//! ```text
//! client → broker   Request {argv, env, cwd, pty, sandbox}   + stdin, stdout, stderr (pty: none)
//! broker → client   Reply::Started {pid}                     (pty: + the pty master)
//! client → broker   Control::Signal / Control::Resize, any number of times
//! broker → client   Reply::Exit(status)                      status = code, or 128 + signal
//! ```
//!
//! A broker that will not run the request answers `Reply::Error` and
//! hangs up. A client that hangs up before `Exit` gets its process
//! group killed.
//!
//! A request runs in the rootfs, or in a named sandbox: a guest root of
//! its own, `<sandboxes>/<name>` on the broker's side, made from a copy
//! of the rootfs the first time the name is used (PLAN-phone-userland.md
//! T3 design item 4, `[sandbox] backend = "userland"`). The client sends a
//! NAME ([`valid_sandbox_name`]: a word, never a path); the broker alone
//! turns it into a directory, and tells the guest which one it is in
//! ([`SANDBOX_MARKER`]).
//!
//! How a client gets that conversation's socket. Directly: connect to
//! the broker's `@veles-userland`, which serves only the gateway's uid.
//! On the phone that connect is EACCES (the two apps' SELinux domains,
//! PLAN-phone-userland.md T3 §2 R1b), so the socket comes over Binder
//! instead, in two hops the conversation itself never sees:
//!
//! ```text
//! gateway Java → userland service   Binder openSession()           (signature permission, calling uid)
//! userland service                  socketpair(); one end is the session
//! service → broker                  Handoff {caller_uid}           + the broker's end, on @veles-userland-handoff (own uid only)
//! service → gateway Java            the other end, as a ParcelFileDescriptor in the Binder reply
//! ulsh → gateway Java               connect @veles-userland-local  (the gateway's own uid only)
//! gateway Java → ulsh               Relay::Opened                  + the session's end (or Relay::Refused, why)
//! ```
//!
//! ulsh then speaks the conversation above on that end, exactly as on a
//! direct connection.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use serde::{Deserialize, Serialize};

/// The broker's abstract socket name (`@veles-userland`).
pub const DEFAULT_SOCKET: &str = "veles-userland";

/// Where the userland service hands the broker sessions it made
/// (`@veles-userland-handoff`; the userland app's own uid only).
pub const HANDOFF_SOCKET: &str = "veles-userland-handoff";

/// The gateway's relay, where ulsh asks for a session
/// (`@veles-userland-local`; the gateway app's own uid only).
pub const RELAY_SOCKET: &str = "veles-userland-local";

/// No frame is bigger: a request is argv + env, and a gateway's env is
/// a few KiB. Refusing more keeps a confused peer from making the other
/// side allocate whatever it claims.
pub const MAX_FRAME: usize = 1 << 20;

/// The most descriptors a frame may carry (stdin, stdout, stderr).
pub const MAX_FDS: usize = 3;

/// The variable the broker sets in a sandbox's guest to the sandbox's
/// name, and removes from every other session's environment whatever
/// the client sent: the guest's, and the client's, way to know where a
/// command really ran. A broker that predates sandboxes runs a request
/// naming one in the rootfs, without it — which is how a newer client
/// tells (veles-core's user envs check it before a box's first use).
pub const SANDBOX_MARKER: &str = "VELES_SANDBOX";

/// The longest sandbox name.
pub const MAX_SANDBOX_NAME: usize = 64;

/// Whether `name` may name a sandbox: 1–[`MAX_SANDBOX_NAME`] bytes of
/// ASCII letters, digits, `_`, `-` and `.`, not starting with `.` or
/// `-`. So never a path (no `/`), never `.` or `..`, never hidden (the
/// broker's own scratch names start with a dot), never an option.
pub fn valid_sandbox_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_SANDBOX_NAME
        && b[0] != b'.'
        && b[0] != b'-'
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}

/// What to run in the rootfs, or in a named sandbox.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The program and its arguments, as seen INSIDE the rootfs.
    pub argv: Vec<String>,
    /// The program's whole environment (the broker adds proot's own).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The working directory inside the rootfs.
    #[serde(default = "default_cwd")]
    pub cwd: String,
    /// Run on a pty the broker makes; its master comes back with
    /// `Started`, and the request carries no descriptors.
    #[serde(default)]
    pub pty: bool,
    /// The pty's size, rows and columns (pty only).
    #[serde(default)]
    pub winsize: Option<(u16, u16)>,
    /// Run in this named sandbox instead of the rootfs (the module doc).
    /// Absent from the JSON when `None`, so a request that names no
    /// sandbox is byte for byte what an older broker knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
}

fn default_cwd() -> String {
    "/root".into()
}

/// Client → broker after the request.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    /// Deliver this signal to the program's process group.
    Signal(i32),
    /// The pty's new size.
    Resize { rows: u16, cols: u16 },
}

/// Broker → client.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Started {
        pid: i32,
    },
    Error(String),
    /// The exit code, or 128 + the signal that ended it.
    Exit(i32),
}

/// The userland service → the broker, on [`HANDOFF_SOCKET`]: the
/// descriptor riding on this frame is one end of a socketpair, a session
/// the service opened for a Binder caller. The service checked the
/// caller (its uid, the signature permission); `caller_uid` is that uid,
/// which the broker holds to the one it serves, as it would a peer's.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    pub caller_uid: u32,
}

/// The gateway's relay → ulsh, on [`RELAY_SOCKET`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Relay {
    /// The session's socket rides on this frame.
    Opened,
    /// No session, and why (the userland package not bound, its
    /// service's error).
    Refused(String),
}

/// Writes one frame, `fds` riding on its first byte.
pub fn write_frame(sock: RawFd, body: &[u8], fds: &[RawFd]) -> io::Result<()> {
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame of {} bytes is over {MAX_FRAME}", body.len()),
        ));
    }
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many descriptors for one frame",
        ));
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(body);
    let mut sent = send_with_fds(sock, &buf, fds)?;
    while sent < buf.len() {
        sent += send_with_fds(sock, &buf[sent..], &[])?;
    }
    Ok(())
}

/// Reads one frame and the descriptors that came with it. `None` is a
/// clean end of stream before a frame began.
pub fn read_frame(sock: RawFd, max_fds: usize) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut head = [0u8; 4];
    let (n, fds) = recv_with_fds(sock, &mut head, max_fds)?;
    if n == 0 {
        return Ok(None);
    }
    read_exact(sock, &mut head[n..])?;
    let len = u32::from_be_bytes(head) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("peer announced a frame of {len} bytes (max {MAX_FRAME})"),
        ));
    }
    let mut body = vec![0u8; len];
    read_exact(sock, &mut body)?;
    Ok(Some((body, fds)))
}

/// [`write_frame`] of a message.
pub fn send<T: Serialize>(sock: RawFd, msg: &T, fds: &[RawFd]) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    write_frame(sock, &body, fds)
}

/// [`read_frame`] of a message.
pub fn recv<T: for<'de> Deserialize<'de>>(
    sock: RawFd,
    max_fds: usize,
) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    match read_frame(sock, max_fds)? {
        None => Ok(None),
        Some((body, fds)) => {
            let msg = serde_json::from_slice(&body)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(Some((msg, fds)))
        }
    }
}

fn read_exact(sock: RawFd, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = unsafe { libc::read(sock, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "peer hung up inside a frame",
            ));
        }
        buf = &mut buf[n as usize..];
    }
    Ok(())
}

fn cmsg_space(nfds: usize) -> usize {
    unsafe { libc::CMSG_SPACE((nfds * std::mem::size_of::<RawFd>()) as u32) as usize }
}

/// One `sendmsg`; `fds` as `SCM_RIGHTS` when there are any.
pub fn send_with_fds(sock: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    // u64-aligned, as cmsghdr wants.
    let mut control = vec![0u64; cmsg_space(fds.len()).div_ceil(8)];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = cmsg_space(fds.len()) as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr(),
                libc::CMSG_DATA(c).cast::<RawFd>(),
                fds.len(),
            );
        }
    }
    loop {
        let n = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// One `recvmsg`, taking up to `max_fds` descriptors (close-on-exec).
/// More than that, or a truncated control message, is an error, and
/// whatever did arrive is closed.
pub fn recv_with_fds(
    sock: RawFd,
    buf: &mut [u8],
    max_fds: usize,
) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let space = cmsg_space(max_fds.max(1));
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let n = loop {
        let n = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    let mut fds = Vec::new();
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let bytes = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(c).cast::<RawFd>();
                for i in 0..bytes / std::mem::size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if fds.len() > max_fds {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("peer sent more descriptors than the {max_fds} this frame takes"),
        ));
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        // Too many for the buffer, or the kernel refused to install some
        // (an LSM's file_receive hook: SELinux between two app domains).
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "descriptors were dropped in transit ({} of at most {max_fds} arrived; \
                 too many, or the system's policy refused them)",
                fds.len()
            ),
        ));
    }
    Ok((n, fds))
}

/// The raw descriptors of `fds`, for [`write_frame`].
pub fn raw(fds: &[OwnedFd]) -> Vec<RawFd> {
    fds.iter().map(AsRawFd::as_raw_fd).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    #[test]
    fn frames_round_trip_and_an_oversize_frame_is_refused() {
        let (a, b) = UnixStream::pair().unwrap();
        let req = Request {
            argv: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            env: [("A".to_string(), "1".to_string())].into(),
            cwd: "/tmp".into(),
            pty: false,
            winsize: None,
            sandbox: None,
        };
        send(a.as_raw_fd(), &req, &[]).unwrap();
        send(a.as_raw_fd(), &Control::Signal(2), &[]).unwrap();
        let (got, fds) = recv::<Request>(b.as_raw_fd(), 3).unwrap().unwrap();
        assert_eq!(got, req);
        assert!(fds.is_empty());
        let (ctl, _) = recv::<Control>(b.as_raw_fd(), 0).unwrap().unwrap();
        assert_eq!(ctl, Control::Signal(2));
        // The JSON is what the docs say it is.
        assert_eq!(
            serde_json::to_string(&Reply::Exit(3)).unwrap(),
            r#"{"exit":3}"#
        );
        // A header announcing more than MAX_FRAME is refused, not allocated.
        (&a).write_all(&((MAX_FRAME as u32) + 1).to_be_bytes())
            .unwrap();
        let err = read_frame(b.as_raw_fd(), 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        // A clean hang-up between frames is None.
        let (c, d) = UnixStream::pair().unwrap();
        drop(c);
        assert!(read_frame(d.as_raw_fd(), 0).unwrap().is_none());
    }

    #[test]
    fn scm_rights_carries_a_descriptor_that_works_on_the_far_side() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut r, w) = UnixStream::pair().unwrap();
        write_frame(a.as_raw_fd(), b"{}", &[w.as_raw_fd()]).unwrap();
        drop(w);
        let (body, fds) = read_frame(b.as_raw_fd(), 3).unwrap().unwrap();
        assert_eq!(body, b"{}");
        assert_eq!(fds.len(), 1, "the descriptor arrived");
        let mut far = std::fs::File::from(fds.into_iter().next().unwrap());
        far.write_all(b"through the socket").unwrap();
        drop(far);
        let mut got = String::new();
        r.read_to_string(&mut got).unwrap();
        assert_eq!(got, "through the socket");
    }

    #[test]
    fn the_binder_hops_frames_are_the_json_the_java_side_writes() {
        // UserlandService.java writes the handoff, UserlandRelay.java the
        // relay's answers, by hand: these strings are the contract.
        assert_eq!(
            serde_json::to_string(&Handoff { caller_uid: 10412 }).unwrap(),
            r#"{"caller_uid":10412}"#
        );
        assert_eq!(
            serde_json::to_string(&Relay::Opened).unwrap(),
            r#""opened""#
        );
        assert_eq!(
            serde_json::from_str::<Relay>(r#"{"refused":"not bound"}"#).unwrap(),
            Relay::Refused("not bound".into())
        );
    }

    #[test]
    fn a_request_may_name_a_sandbox_and_an_older_request_still_parses() {
        // An older ulsh's request: no field at all.
        let old: Request = serde_json::from_str(r#"{"argv":["/bin/true"]}"#).unwrap();
        assert_eq!(old.sandbox, None);
        assert_eq!(old.cwd, "/root");
        // None is not written: an older broker reads exactly what it knows.
        let s = serde_json::to_string(&old).unwrap();
        assert!(!s.contains("sandbox"), "{s}");
        let named: Request =
            serde_json::from_str(r#"{"argv":["/bin/true"],"sandbox":"box-1"}"#).unwrap();
        assert_eq!(named.sandbox.as_deref(), Some("box-1"));
        assert!(serde_json::to_string(&named)
            .unwrap()
            .contains(r#""sandbox":"box-1""#));
    }

    #[test]
    fn a_sandbox_name_is_a_word_never_a_path() {
        let longest = "x".repeat(MAX_SANDBOX_NAME);
        for ok in [
            "a",
            "dm_local_owner-1a2b3c4d",
            "tcl-fence",
            "A.b_c-9",
            "_x",
            longest.as_str(),
        ] {
            assert!(valid_sandbox_name(ok), "{ok:?} is a name");
        }
        let too_long = "x".repeat(MAX_SANDBOX_NAME + 1);
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            ".seed-x",
            "../rootfs",
            "a/b",
            "/abs",
            "-rf",
            "sp ace",
            "tab\t",
            "nul\0",
            "é",
            too_long.as_str(),
        ] {
            assert!(!valid_sandbox_name(bad), "{bad:?} is not a name");
        }
    }

    #[test]
    fn more_descriptors_than_the_frame_takes_is_an_error() {
        let (a, b) = UnixStream::pair().unwrap();
        let (_x, y) = UnixStream::pair().unwrap();
        write_frame(a.as_raw_fd(), b"{}", &[y.as_raw_fd(), y.as_raw_fd()]).unwrap();
        let err = read_frame(b.as_raw_fd(), 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
    }
}
