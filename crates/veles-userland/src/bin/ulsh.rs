//! ulsh — the gateway's shell when the userland package answers
//! (lib/arm64-v8a/libulsh.so in the gateway APK; Engine.java points
//! VELES_SHELL at it).
//!
//!   ulsh -c SCRIPT [ARG0 ARGS…]   SCRIPT under the rootfs's bash, else sh
//!   ulsh                          a login shell
//!   ulsh -t …                     on a pty the broker makes
//!   ulsh --exec PROG ARGS…        PROG, no shell
//!
//! Environment: ULSH_RELAY (the gateway's relay socket: take the session
//! from there — the phone's way, PLAN-phone-userland.md T3 §4; Userland.java
//! sets it), else ULSH_SOCKET (connect to the broker directly, default
//! veles-userland), ULSH_BROKER_UID (refuse a broker running as anyone
//! else), ULSH_CWD (the guest's working directory, default /root),
//! ULSH_SANDBOX (run in that named sandbox, a guest root of its own,
//! instead of the rootfs — veles' user envs, `[sandbox] backend =
//! "userland"`), ULSH_HOME (the guest's HOME, default /root).
//!
//! Exit status: the program's (128 + signal when one ended it); 127
//! when no broker answers, 126 when the broker refuses.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

use veles_userland::client;
use veles_userland::proto::{self, Request};

static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = sig as u8;
        unsafe {
            libc::write(fd, (&b as *const u8).cast(), 1);
        }
    }
}

/// Signals that ulsh passes on instead of dying of them, reported
/// through a pipe (a handler may only write).
fn forward_signals(sigs: &[i32]) -> io::Result<RawFd> {
    let mut p = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    SIGNAL_PIPE.store(p[1], Ordering::Relaxed);
    for &s in sigs {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(s, &sa, std::ptr::null_mut());
        }
    }
    Ok(p[0])
}

fn winsize(fd: RawFd) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) } == 0 && ws.ws_row > 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}

/// The outer terminal in raw mode for a pty session, restored on drop.
struct Raw(Option<libc::termios>);

impl Raw {
    fn new(fd: RawFd) -> Raw {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::isatty(fd) == 0 || libc::tcgetattr(fd, &mut t) != 0 {
                return Raw(None);
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(fd, libc::TCSANOW, &t);
            Raw(Some(saved))
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, t);
            }
        }
    }
}

fn copy(from: RawFd, to: RawFd) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = unsafe { libc::read(from, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        // 0 is EOF; EIO is a pty whose program side has all closed.
        if n <= 0 {
            return;
        }
        let mut off = 0;
        while off < n as usize {
            let w = unsafe { libc::write(to, buf[off..].as_ptr().cast(), n as usize - off) };
            if w < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            off += w as usize;
        }
    }
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let inv = match client::parse_args(&args) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("ulsh: {e}");
            return 2;
        }
    };
    let pty = inv
        .pty
        .unwrap_or_else(|| args.is_empty() && unsafe { libc::isatty(0) } == 1);
    let socket = std::env::var("ULSH_SOCKET").unwrap_or_else(|_| proto::DEFAULT_SOCKET.into());
    let broker_uid = match std::env::var("ULSH_BROKER_UID") {
        Ok(s) if !s.trim().is_empty() => match s.trim().parse::<u32>() {
            Ok(u) => Some(u),
            Err(_) => {
                eprintln!("ulsh: ULSH_BROKER_UID={s:?} is not a uid");
                return 2;
            }
        },
        _ => None,
    };
    let sandbox = match client::sandbox_from(std::env::var("ULSH_SANDBOX").ok()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ulsh: {e}");
            return 2;
        }
    };
    let req = Request {
        argv: inv.argv,
        env: client::guest_env(std::env::vars(), pty),
        cwd: std::env::var("ULSH_CWD").unwrap_or_else(|_| "/root".into()),
        pty,
        winsize: if pty { winsize(0) } else { None },
        sandbox,
    };
    let mut sigs = vec![libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];
    if pty {
        sigs.push(libc::SIGWINCH);
    }
    let sig_rx = match forward_signals(&sigs) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("ulsh: {e}");
            return 126;
        }
    };
    let started = match std::env::var("ULSH_RELAY") {
        Ok(relay) if !relay.trim().is_empty() => {
            client::open_relay(relay.trim(), veles_userland::sock::my_uid())
                .and_then(|conn| client::start_on(conn, broker_uid, &req, [0, 1, 2]))
        }
        _ => client::start(&socket, broker_uid, &req, [0, 1, 2]),
    };
    let session = match started {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ulsh: {e}");
            return match e.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => 127,
                _ => 126,
            };
        }
    };
    let ctl = session.controller();
    std::thread::spawn(move || loop {
        let mut b = 0u8;
        let n = unsafe { libc::read(sig_rx, (&mut b as *mut u8).cast(), 1) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if n <= 0 {
            return;
        }
        let sig = b as i32;
        let sent = if sig == libc::SIGWINCH {
            match winsize(0) {
                Some((r, c)) => ctl.resize(r, c),
                None => Ok(()),
            }
        } else {
            ctl.signal(sig)
        };
        if sent.is_err() {
            return;
        }
    });
    let _raw;
    if let Some(m) = &session.master {
        _raw = Raw::new(0);
        let m = m.as_raw_fd();
        std::thread::spawn(move || copy(0, m));
        copy(m, 1);
    }
    match session.wait() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ulsh: {e}");
            255
        }
    }
}

fn main() {
    std::process::exit(run());
}
