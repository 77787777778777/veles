//! The broker: runs the gateway's commands under proot in the rootfs.
//!
//! One thread per connection. A connection from any uid but the
//! gateway's is answered `Error` and closed before a byte of it is read;
//! a connection that sends nothing and hangs up is a probe (the
//! gateway's start checks the broker answers) and costs nothing.
//!
//! Sessions also arrive pre-connected ([`serve_handoff`]): the userland
//! service, asked over Binder, makes a socketpair and hands the broker
//! one end on `@veles-userland-handoff`, which only the userland app's
//! own uid may use. Such a session's peer is the service, not the
//! gateway, so its uid check is the service's word (`Handoff`'s
//! `caller_uid`), held to the same `allow_uid`. Either way a session is
//! [`session`] from its first frame on.
//!
//! The program runs in a session of its own (`setsid`), so a signal the
//! client forwards, or the SIGKILL its hang-up earns, reaches everything
//! the script started, not only proot.
//!
//! A request may name a sandbox ([`root_for`]): its own guest root under
//! `--sandboxes`, made from a copy of the rootfs on first use, so what one
//! sandbox writes no other sandbox, and not the rootfs, sees. That is a
//! separation of files, not a security fence: every guest runs as the
//! userland app's uid under the same ptrace-based proot, with the phone's
//! network (PLAN-phone-userland.md T3 design item 4; docs/phone.md says so
//! to the person).

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::proto::{self, Control, Reply, Request};
use crate::sock;

/// Where the broker's pieces are, and whom it serves.
#[derive(Debug, Clone)]
pub struct Config {
    /// proot itself (the userland APK's `libproot.so`).
    pub proot: PathBuf,
    /// proot's loader (`libproot_loader.so`): PROOT_LOADER. proot is
    /// built with the loader unbundled, so this is required on the phone.
    pub loader: Option<PathBuf>,
    /// The guest's root (`files/rootfs`).
    pub rootfs: PathBuf,
    /// proot's scratch directory (PROOT_TMP_DIR); Android has no /tmp.
    pub tmp: PathBuf,
    /// A host directory bound at `/shared` in the guest.
    pub shared: Option<PathBuf>,
    /// The one uid allowed to connect: the gateway app's.
    pub allow_uid: u32,
    /// The one uid allowed to hand sessions over ([`serve_handoff`]):
    /// the userland app's own, whose service makes them.
    pub handoff_uid: u32,
    /// Where the named sandboxes live (`files/sandboxes`), each its own
    /// guest root, `<sandboxes>/<name>` ([`root_for`]). `None`: a request
    /// naming one is refused.
    pub sandboxes: Option<PathBuf>,
}

/// Signals a client may forward. Anything else is dropped.
const FORWARDABLE: &[i32] = &[
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGKILL,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGWINCH,
    libc::SIGCONT,
    libc::SIGTSTP,
];

/// proot's arguments for `req` in `root` ([`root_for`]): the root, the
/// host's /dev, /proc and /sys, the shared directory (the rootfs only),
/// the working directory, then the program.
/// `-0` (fake root: apk wants uid 0), `--link2symlink` (an app's storage
/// refuses hard links) and `--kill-on-exit` (nothing outlives the
/// script) are what Termux's proot-distro runs with too.
pub fn proot_args(cfg: &Config, req: &Request, root: &Path) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--kill-on-exit".into(),
        "--link2symlink".into(),
        "-0".into(),
        "-r".into(),
        root.display().to_string(),
        "-b".into(),
        "/dev".into(),
        "-b".into(),
        "/proc".into(),
        "-b".into(),
        "/sys".into(),
    ];
    // The shared directory is the rootfs's: every sandbox seeing the same
    // `/shared` would make them one place.
    if let (Some(shared), None) = (&cfg.shared, &req.sandbox) {
        a.push("-b".into());
        a.push(format!("{}:/shared", shared.display()));
    }
    a.push("-w".into());
    a.push(if req.cwd.starts_with('/') {
        req.cwd.clone()
    } else {
        "/".into()
    });
    a.extend(req.argv.iter().cloned());
    a
}

/// proot's environment: the request's, minus any PROOT_* it carries
/// (the guest does not configure the tracer) and any sandbox marker
/// (the broker's word, [`proto::SANDBOX_MARKER`], set here for a
/// sandbox's session only), plus the broker's own PROOT_* knobs (e.g.
/// PROOT_NO_SECCOMP, set by whoever started it), plus the loader and
/// scratch directory from `cfg`.
pub fn proot_env(
    cfg: &Config,
    req: &Request,
    own: impl Iterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = req
        .env
        .iter()
        .filter(|(k, _)| !k.starts_with("PROOT_") && k.as_str() != proto::SANDBOX_MARKER)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Some(name) = &req.sandbox {
        env.insert(proto::SANDBOX_MARKER.into(), name.clone());
    }
    env.extend(own.filter(|(k, _)| k.starts_with("PROOT_")));
    env.insert("PROOT_TMP_DIR".into(), cfg.tmp.display().to_string());
    if let Some(loader) = &cfg.loader {
        env.insert("PROOT_LOADER".into(), loader.display().to_string());
    }
    env
}

// ── named sandboxes ─────────────────────────────────────────────────

/// The directory a sandbox name stands for, or why there is none. The
/// client sends a name; this, not the client, makes it a path — a word
/// ([`proto::valid_sandbox_name`]) under `cfg.sandboxes`, never anywhere
/// else.
pub fn sandbox_dir(cfg: &Config, name: &str) -> Result<PathBuf, String> {
    if !proto::valid_sandbox_name(name) {
        return Err(format!(
            "{name:?} is not a sandbox name (letters, digits, `_`, `-` and `.`, \
             up to {} bytes, not starting with `.` or `-`)",
            proto::MAX_SANDBOX_NAME
        ));
    }
    match &cfg.sandboxes {
        Some(d) => Ok(d.join(name)),
        None => Err(format!(
            "this broker keeps no sandboxes (it was started without --sandboxes), \
             so it cannot run in sandbox {name:?}"
        )),
    }
}

/// The guest root `req` runs in: the rootfs, or its sandbox's own root,
/// made from a copy of the rootfs the first time the name is used and
/// the person's (the guest's) from then on — nothing here touches it
/// again but its DNS servers ([`refresh_resolv_conf`]).
pub fn root_for(cfg: &Config, req: &Request) -> Result<PathBuf, String> {
    let Some(name) = &req.sandbox else {
        return Ok(cfg.rootfs.clone());
    };
    let dir = sandbox_dir(cfg, name)?;
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.is_dir() => {}
        // A link or a file where the sandbox should be is not followed:
        // the guest root is a directory this broker made.
        Ok(_) => {
            return Err(format!(
                "sandbox {name:?}: {} is not a directory",
                dir.display()
            ))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            seed(cfg, name, &dir)
                .map_err(|e| format!("cannot make sandbox {name:?} from the rootfs: {e}"))?;
        }
        Err(e) => return Err(format!("sandbox {name:?}: {e}")),
    }
    refresh_resolv_conf(&cfg.rootfs, &dir);
    Ok(dir)
}

/// What one seeding did, for the log (the measurement the plan's table
/// wants: how long a first use waits).
#[derive(Debug, Default)]
struct CopyStats {
    entries: u64,
    bytes: u64,
    cloned: u64,
    copied: u64,
    linked: u64,
}

/// Makes `dir`, the sandbox `name`, from a copy of the rootfs: into a
/// scratch name beside it, then renamed into place, so a half-made
/// sandbox is never used (a broker killed mid-copy leaves a `.seed-*`
/// that the next seeding of that name removes). One at a time: a second
/// session naming the same new sandbox waits and finds it made.
fn seed(cfg: &Config, name: &str, dir: &Path) -> io::Result<()> {
    static SEEDING: Mutex<()> = Mutex::new(());
    let _one = SEEDING.lock().unwrap_or_else(|e| e.into_inner());
    if std::fs::symlink_metadata(dir).is_ok() {
        return Ok(());
    }
    if !cfg.rootfs.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("the rootfs {} is not there yet", cfg.rootfs.display()),
        ));
    }
    let parent = dir
        .parent()
        .ok_or_else(|| io::Error::other("a sandbox directory with no parent"))?;
    std::fs::create_dir_all(parent)?;
    let scratch = parent.join(format!(".seed-{name}"));
    let _ = std::fs::remove_dir_all(&scratch);
    let t0 = std::time::Instant::now();
    let mut st = CopyStats::default();
    let into = std::path::absolute(dir)?;
    let copied = copy_tree(
        &cfg.rootfs,
        &scratch,
        &Repoint::of(&cfg.rootfs, &into),
        &mut st,
        &mut std::collections::HashMap::new(),
    )
    .and_then(|()| std::fs::rename(&scratch, dir));
    if let Err(e) = copied {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(e);
    }
    eprintln!(
        "veles-userland-broker: sandbox {name}: made from the rootfs, {} entries, {} KiB \
         ({} files cloned, {} copied, {} hard links kept) in {} ms",
        st.entries,
        st.bytes >> 10,
        st.cloned,
        st.copied,
        st.linked,
        t0.elapsed().as_millis()
    );
    Ok(())
}

/// Symlink targets in a copied rootfs. proot's `--link2symlink` turns a
/// guest's hard link into symlinks holding ABSOLUTE HOST paths into the
/// rootfs (git's helpers, after `apk add git`, are made that way); copied
/// verbatim they would point back at the rootfs, so a write through one
/// would land there. Those are re-pointed into the copy. A relative
/// target, or a guest-absolute one (`/bin/busybox`), is the guest's own
/// spelling and is kept.
struct Repoint {
    /// The rootfs as configured, and as the kernel spells it (Android's
    /// `/data/user/0/<app>` is `/data/data/<app>` underneath).
    from: Vec<PathBuf>,
    to: PathBuf,
}

impl Repoint {
    fn of(rootfs: &Path, to: &Path) -> Repoint {
        let mut from = vec![std::path::absolute(rootfs).unwrap_or_else(|_| rootfs.to_path_buf())];
        if let Ok(c) = rootfs.canonicalize() {
            if !from.contains(&c) {
                from.push(c);
            }
        }
        Repoint {
            from,
            to: to.to_path_buf(),
        }
    }

    fn repoint(&self, target: &Path) -> PathBuf {
        if target.is_absolute() {
            for f in &self.from {
                if let Ok(rest) = target.strip_prefix(f) {
                    return self.to.join(rest);
                }
            }
        }
        target.to_path_buf()
    }
}

/// `src` copied to `dst` (which must not exist): directories, regular
/// files and symlinks; devices, fifos and sockets are left out (the
/// guest has the host's /dev, and Rootfs.java never unpacks them).
/// Directories are made owner-writable while filled and get their own
/// mode (plus the owner's rwx, so the tree can always be removed) once
/// full, the way Rootfs.java unpacks the tar.
fn copy_tree(
    src: &Path,
    dst: &Path,
    rp: &Repoint,
    st: &mut CopyStats,
    links: &mut std::collections::HashMap<(u64, u64), PathBuf>,
) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mode = std::fs::symlink_metadata(src)?.mode();
    std::fs::create_dir(dst)?;
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o700))?;
    st.entries += 1;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (from, to) = (e.path(), dst.join(e.file_name()));
        let m = std::fs::symlink_metadata(&from)?;
        let ft = m.file_type();
        if ft.is_dir() {
            copy_tree(&from, &to, rp, st, links)?;
        } else if ft.is_symlink() {
            std::os::unix::fs::symlink(rp.repoint(&std::fs::read_link(&from)?), &to)?;
            st.entries += 1;
        } else if ft.is_file() {
            copy_file(&from, &to, &m, st, links)?;
        }
    }
    std::fs::set_permissions(
        dst,
        std::fs::Permissions::from_mode((mode & 0o7777) | 0o700),
    )
}

/// One regular file. A file the rootfs holds under several names stays
/// one file under those names in the copy (a hard link WITHIN the copy;
/// never one between the copy and the rootfs, which would let a write in
/// place in one show in the other). Otherwise a reflink where the
/// filesystem has them (FICLONE: btrfs, XFS; the phone's f2fs has not),
/// else a plain copy.
fn copy_file(
    from: &Path,
    to: &Path,
    m: &std::fs::Metadata,
    st: &mut CopyStats,
    links: &mut std::collections::HashMap<(u64, u64), PathBuf>,
) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let id = (m.dev(), m.ino());
    if m.nlink() > 1 {
        if let Some(first) = links.get(&id) {
            if std::fs::hard_link(first, to).is_ok() {
                st.linked += 1;
                st.entries += 1;
                return Ok(());
            }
        }
    }
    let src = std::fs::File::open(from)?;
    let dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)?;
    if reflink(&src, &dst) {
        st.cloned += 1;
    } else {
        std::io::copy(&mut &src, &mut &dst)?;
        st.copied += 1;
    }
    st.bytes += m.len();
    st.entries += 1;
    let perms = std::fs::Permissions::from_mode(m.mode() & 0o7777);
    if std::fs::set_permissions(to, perms).is_err() {
        // A set-id bit the filesystem will not take: the rest of the mode.
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(m.mode() & 0o777))?;
    }
    if m.nlink() > 1 {
        links.insert(id, to.to_path_buf());
    }
    Ok(())
}

/// `ioctl(FICLONE)`: `dst` shares `src`'s blocks until either is written.
/// `false` where the filesystem cannot (and then `dst` is still empty).
fn reflink(src: &std::fs::File, dst: &std::fs::File) -> bool {
    // _IOW(0x94, 9, int); the libc crate does not name it on every target.
    const FICLONE: u32 = 0x4004_9409;
    unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) == 0 }
}

/// The rootfs's `/etc/resolv.conf` into a sandbox, when it differs: the
/// userland service rewrites the rootfs's at each start from the active
/// network's DNS servers (Rootfs.java), and a sandbox made yesterday on
/// another network must not keep yesterday's. A sandbox whose
/// resolv.conf is a link or a directory made its own choice.
fn refresh_resolv_conf(rootfs: &Path, sandbox: &Path) {
    let Ok(want) = std::fs::read(rootfs.join("etc/resolv.conf")) else {
        return;
    };
    let f = sandbox.join("etc/resolv.conf");
    match std::fs::symlink_metadata(&f) {
        Ok(m) if m.is_file() => {
            if std::fs::read(&f).is_ok_and(|have| have == want) {
                return;
            }
        }
        Ok(_) => return,
        Err(_) if !sandbox.join("etc").is_dir() => return,
        Err(_) => {}
    }
    let _ = std::fs::write(&f, &want);
}

fn validate(req: &Request, nfds: usize) -> Result<(), String> {
    match req.argv.first() {
        None => return Err("empty argv".into()),
        Some(p) if p.is_empty() || p.starts_with('-') => {
            return Err(format!("argv[0] {p:?} is not a program"))
        }
        _ => {}
    }
    if let Some(name) = &req.sandbox {
        if !proto::valid_sandbox_name(name) {
            return Err(format!(
                "{name:?} is not a sandbox name (a word, never a path)"
            ));
        }
    }
    let want = if req.pty { 0 } else { 3 };
    if nfds != want {
        return Err(format!(
            "a {} request carries {want} descriptors, this one {nfds}",
            if req.pty { "pty" } else { "plain" }
        ));
    }
    Ok(())
}

/// Accepts connections forever, one thread each.
pub fn serve(listener: UnixListener, cfg: Arc<Config>) -> io::Result<()> {
    for conn in listener.incoming() {
        match conn {
            Ok(c) => {
                let cfg = cfg.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle(c, &cfg) {
                        eprintln!("veles-userland-broker: connection: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("veles-userland-broker: accept: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    Ok(())
}

/// One connection, start to finish: the peer must be the gateway.
pub fn handle(conn: UnixStream, cfg: &Config) -> io::Result<()> {
    let uid = sock::peer_uid(&conn)?;
    if !serves(&conn, uid, cfg) {
        return Ok(());
    }
    session(conn, cfg)
}

/// Whether `uid` (a peer's, or the uid a handoff vouches for) is the one
/// this broker serves; when not, `conn` is told why.
fn serves(conn: &UnixStream, uid: u32, cfg: &Config) -> bool {
    if uid == cfg.allow_uid {
        return true;
    }
    eprintln!(
        "veles-userland-broker: refused uid {uid} (serving uid {})",
        cfg.allow_uid
    );
    let _ = proto::send(
        conn.as_raw_fd(),
        &Reply::Error(format!(
            "uid {uid} may not use this broker (it serves uid {})",
            cfg.allow_uid
        )),
        &[],
    );
    false
}

/// Accepts the userland service's handoffs forever, one thread each.
pub fn serve_handoff(listener: UnixListener, cfg: Arc<Config>) -> io::Result<()> {
    for conn in listener.incoming() {
        match conn {
            Ok(c) => {
                let cfg = cfg.clone();
                std::thread::spawn(move || {
                    if let Err(e) = take_handoffs(c, &cfg) {
                        eprintln!("veles-userland-broker: handoff: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("veles-userland-broker: accept (handoff): {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    Ok(())
}

/// One handoff connection: from the userland app itself, else closed
/// unread (what it carried is closed with it). Each frame is a
/// [`proto::Handoff`] with one session socket, served on a thread of its
/// own; the service may send any number, or hang up after one.
fn take_handoffs(conn: UnixStream, cfg: &Config) -> io::Result<()> {
    let uid = sock::peer_uid(&conn)?;
    if uid != cfg.handoff_uid {
        eprintln!(
            "veles-userland-broker: refused a handoff from uid {uid} (only uid {} hands sessions over)",
            cfg.handoff_uid
        );
        return Ok(());
    }
    while let Some((h, mut fds)) = proto::recv::<proto::Handoff>(conn.as_raw_fd(), 1)? {
        let Some(fd) = fds.pop() else {
            eprintln!("veles-userland-broker: a handoff without its session socket");
            continue;
        };
        let s = UnixStream::from(fd);
        if !serves(&s, h.caller_uid, cfg) {
            continue;
        }
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            if let Err(e) = session(s, &cfg) {
                eprintln!("veles-userland-broker: session: {e}");
            }
        });
    }
    Ok(())
}

/// A session whose peer is already trusted (an accepted connection that
/// passed the uid check, or a handed-over socket), from its first frame:
/// a probe (nothing, then a hang-up), or a request and its run.
pub fn session(conn: UnixStream, cfg: &Config) -> io::Result<()> {
    let fd = conn.as_raw_fd();
    let (req, fds) = match proto::recv::<Request>(fd, proto::MAX_FDS) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()), // a probe
        Err(e) => {
            // Say why before hanging up: on the phone this is where a
            // policy that drops the client's descriptors shows.
            let _ = proto::send(fd, &Reply::Error(format!("broker: {e}")), &[]);
            return Err(e);
        }
    };
    if let Err(msg) = validate(&req, fds.len()) {
        let _ = proto::send(fd, &Reply::Error(msg), &[]);
        return Ok(());
    }
    let (stdio, master) = if req.pty {
        let (m, s) = open_pty(req.winsize)?;
        ([s.try_clone()?, s.try_clone()?, s], Some(m))
    } else {
        let mut it = fds.into_iter();
        let (a, b, c) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        ([a, b, c], None)
    };
    let [i, o, e] = stdio;
    let root = match root_for(cfg, &req) {
        Ok(r) => r,
        Err(msg) => {
            let _ = proto::send(fd, &Reply::Error(msg), &[]);
            return Ok(());
        }
    };
    let mut cmd = Command::new(&cfg.proot);
    cmd.args(proot_args(cfg, &req, &root))
        .env_clear()
        .envs(proot_env(cfg, &req, std::env::vars()))
        .current_dir("/")
        .stdin(Stdio::from(i))
        .stdout(Stdio::from(o))
        .stderr(Stdio::from(e));
    let pty = req.pty;
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if pty && libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let spawned = cmd.spawn();
    // The Command holds our copies of the descriptors (the pty's slave
    // among them): closed now, so the client sees EOF/EIO when the
    // program's side closes, not never.
    drop(cmd);
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let _ = proto::send(
                fd,
                &Reply::Error(format!("cannot start {}: {e}", cfg.proot.display())),
                &[],
            );
            return Ok(());
        }
    };
    let pid = child.id() as i32;
    let writer = Arc::new(Mutex::new(conn.try_clone()?));
    // Guarded: the program has been reaped. Signals are sent with it
    // held, and the waiter reaps with it held, so a forwarded signal
    // never lands on a recycled pid.
    let reaped = Arc::new(Mutex::new(false));
    let master_raw: Vec<RawFd> = master.iter().map(AsRawFd::as_raw_fd).collect();
    if let Err(e) = proto::send(fd, &Reply::Started { pid }, &master_raw) {
        kill_group(pid, libc::SIGKILL);
        let _ = child.wait();
        return Err(e);
    }
    let waiter = {
        let writer = writer.clone();
        let reaped = reaped.clone();
        std::thread::spawn(move || {
            wait_exited(pid);
            let status = {
                let mut r = reaped.lock().unwrap();
                let s = child.wait();
                *r = true;
                s
            };
            let code = match status {
                Ok(s) => s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)),
                Err(_) => 255,
            };
            let w = writer.lock().unwrap();
            let _ = proto::send(w.as_raw_fd(), &Reply::Exit(code), &[]);
            let _ = w.shutdown(std::net::Shutdown::Both);
        })
    };
    loop {
        match proto::recv::<Control>(fd, 0) {
            Ok(Some((Control::Signal(sig), _))) => {
                if FORWARDABLE.contains(&sig) {
                    let r = reaped.lock().unwrap();
                    if !*r {
                        kill_group(pid, sig);
                    }
                }
            }
            Ok(Some((Control::Resize { rows, cols }, _))) => {
                if let Some(m) = &master {
                    set_winsize(m.as_raw_fd(), rows, cols);
                }
            }
            Ok(None) | Err(_) => {
                // Hung up (or the waiter shut the socket after Exit).
                let r = reaped.lock().unwrap();
                if !*r {
                    kill_group(pid, libc::SIGKILL);
                }
                break;
            }
        }
    }
    let _ = waiter.join();
    Ok(())
}

fn kill_group(pid: i32, sig: i32) {
    unsafe {
        libc::kill(-pid, sig);
    }
}

/// Blocks until `pid` has exited, without reaping it (WNOWAIT): the
/// zombie keeps the pid, and the process group id, from being reused
/// until [`handle`]'s waiter reaps it under the lock.
fn wait_exited(pid: i32) {
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// A new pty pair, master first, both close-on-exec. POSIX's calls, not
/// the Linux ioctls behind them: the libc crate has no TIOCGPTN for
/// Android, and bionic has all four of these.
fn open_pty(winsize: Option<(u16, u16)>) -> io::Result<(OwnedFd, OwnedFd)> {
    let m = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if m < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(m) };
    let mut name = [0 as libc::c_char; 128];
    unsafe {
        if libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC) < 0
            || libc::grantpt(m) < 0
            || libc::unlockpt(m) < 0
        {
            return Err(io::Error::last_os_error());
        }
        let rc = libc::ptsname_r(m, name.as_mut_ptr(), name.len());
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
    }
    let s = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if s < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = unsafe { OwnedFd::from_raw_fd(s) };
    if let Some((rows, cols)) = winsize {
        set_winsize(m, rows, cols);
    }
    Ok((master, slave))
}

/// TIOCSWINSZ on `fd`.
pub fn set_winsize(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            proot: "/lib/libproot.so".into(),
            loader: Some("/lib/libproot_loader.so".into()),
            rootfs: "/data/files/rootfs".into(),
            tmp: "/data/cache/proot".into(),
            shared: Some("/data/files/shared".into()),
            allow_uid: 10123,
            handoff_uid: 10124,
            sandboxes: Some("/data/files/sandboxes".into()),
        }
    }

    fn req(argv: &[&str], cwd: &str) -> Request {
        Request {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: [
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("PROOT_LOADER".to_string(), "/guest/says/so".to_string()),
            ]
            .into(),
            cwd: cwd.into(),
            pty: false,
            winsize: None,
            sandbox: None,
        }
    }

    fn in_sandbox(argv: &[&str], name: &str) -> Request {
        let mut r = req(argv, "/work");
        r.sandbox = Some(name.into());
        r
    }

    #[test]
    fn proot_runs_the_program_in_the_rootfs_with_the_binds_and_cwd() {
        let rootfs = cfg().rootfs;
        let a = proot_args(&cfg(), &req(&["/bin/sh", "-c", "id"], "/root"), &rootfs);
        let s = a.join(" ");
        assert!(
            s.starts_with("--kill-on-exit --link2symlink -0 -r /data/files/rootfs "),
            "{s}"
        );
        assert!(
            s.contains("-b /dev -b /proc -b /sys -b /data/files/shared:/shared"),
            "{s}"
        );
        assert!(s.ends_with("-w /root /bin/sh -c id"), "{s}");
        // A relative cwd is not a guest path: the root instead.
        let a = proot_args(&cfg(), &req(&["/bin/true"], "relative"), &rootfs);
        assert!(a.join(" ").ends_with("-w / /bin/true"));
    }

    /// A named sandbox is its own guest root, with the host's /dev, /proc
    /// and /sys like the rootfs, but not the shared directory: two
    /// sandboxes that both saw `/shared` would not be apart.
    #[test]
    fn a_sandbox_runs_in_its_own_root_without_the_shared_bind() {
        let r = in_sandbox(&["/bin/sh", "-c", "id"], "box-1");
        let root = sandbox_dir(&cfg(), "box-1").unwrap();
        assert_eq!(root, PathBuf::from("/data/files/sandboxes/box-1"));
        let s = proot_args(&cfg(), &r, &root).join(" ");
        assert!(
            s.starts_with("--kill-on-exit --link2symlink -0 -r /data/files/sandboxes/box-1 "),
            "{s}"
        );
        assert!(s.contains("-b /dev -b /proc -b /sys -w /work"), "{s}");
        assert!(!s.contains("/shared"), "no shared bind in a sandbox: {s}");
        assert!(!s.contains("/data/files/rootfs"), "{s}");
    }

    /// The broker turns a name into a directory, never the client: a
    /// name that is a path is refused, and so is any name when this
    /// broker keeps no sandboxes.
    #[test]
    fn a_sandbox_name_is_resolved_under_sandboxes_or_refused() {
        for bad in ["../rootfs", "a/b", ".seed-x", "", "/data/files/rootfs"] {
            assert!(sandbox_dir(&cfg(), bad).is_err(), "{bad:?}");
            assert!(
                validate(&in_sandbox(&["/bin/sh"], bad), 3).is_err(),
                "{bad:?}"
            );
            assert!(root_for(&cfg(), &in_sandbox(&["/bin/sh"], bad)).is_err());
        }
        let mut none = cfg();
        none.sandboxes = None;
        let err = sandbox_dir(&none, "box-1").unwrap_err();
        assert!(err.contains("keeps no sandboxes"), "{err}");
        // No name: the rootfs, whatever sandboxes holds.
        assert_eq!(
            root_for(&cfg(), &req(&["/bin/sh"], "/")).unwrap(),
            cfg().rootfs
        );
    }

    /// The guest learns its sandbox from the broker; a client's own
    /// claim is dropped, in a sandbox and out of one.
    #[test]
    fn the_sandbox_marker_is_the_brokers_word_not_the_clients() {
        let mut forged = req(&["/bin/true"], "/");
        forged
            .env
            .insert(proto::SANDBOX_MARKER.into(), "box-9".into());
        let env = proot_env(&cfg(), &forged, std::iter::empty());
        assert!(!env.contains_key(proto::SANDBOX_MARKER), "{env:?}");
        forged.sandbox = Some("box-1".into());
        let env = proot_env(&cfg(), &forged, std::iter::empty());
        assert_eq!(env[proto::SANDBOX_MARKER], "box-1");
    }

    /// Symlinks a copied rootfs carries: proot's link2symlink writes
    /// ABSOLUTE HOST paths into the rootfs, which must point into the
    /// copy, not back at the rootfs; everything else is the guest's own
    /// spelling and is kept.
    #[test]
    fn a_copied_rootfs_link_into_the_rootfs_points_into_the_copy() {
        let rp = Repoint::of(
            Path::new("/data/files/rootfs"),
            Path::new("/data/files/sandboxes/box-1"),
        );
        assert_eq!(
            rp.repoint(Path::new("/data/files/rootfs/usr/bin/.l2s.git0001")),
            PathBuf::from("/data/files/sandboxes/box-1/usr/bin/.l2s.git0001")
        );
        for kept in [
            "/bin/busybox",
            "busybox",
            "../lib/x",
            "/data/files/rootfs2/x",
        ] {
            assert_eq!(rp.repoint(Path::new(kept)), PathBuf::from(kept), "{kept}");
        }
    }

    #[test]
    fn proot_env_is_the_requests_plus_the_brokers_proot_settings() {
        let own = vec![
            ("PROOT_NO_SECCOMP".to_string(), "1".to_string()),
            ("HOME".to_string(), "/data/user/0/x".to_string()),
        ];
        let env = proot_env(&cfg(), &req(&["/bin/true"], "/"), own.into_iter());
        assert_eq!(env["PATH"], "/usr/bin:/bin");
        assert_eq!(
            env["PROOT_LOADER"], "/lib/libproot_loader.so",
            "not the guest's"
        );
        assert_eq!(env["PROOT_TMP_DIR"], "/data/cache/proot");
        assert_eq!(env["PROOT_NO_SECCOMP"], "1");
        assert!(!env.contains_key("HOME"), "the broker's own env stays out");
    }

    #[test]
    fn a_request_must_name_a_program_and_carry_its_descriptors() {
        assert!(validate(&req(&[], "/"), 3).is_err());
        assert!(validate(&req(&["--help"], "/"), 3).is_err());
        assert!(validate(&req(&["/bin/sh"], "/"), 2).is_err());
        assert!(validate(&req(&["/bin/sh"], "/"), 3).is_ok());
        let mut p = req(&["/bin/sh"], "/");
        p.pty = true;
        assert!(validate(&p, 0).is_ok());
        assert!(validate(&p, 3).is_err());
        assert!(validate(&in_sandbox(&["/bin/sh"], "box-1"), 3).is_ok());
    }
}
