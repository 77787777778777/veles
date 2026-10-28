//! The unix bodies. Raw `libc::` is this repo's idiom for real unix
//! work (Cargo.toml's note where `nix` was dropped); each function here
//! is the code that was inline at its caller before the layer existed.

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// What a model used to Linux will not find on the phone, with the way
/// around it where there is one (PLAN-phone-userland.md T1). Nothing in
/// the app's own storage may execute at targetSdk 36, so what the APK
/// ships is all there is: no package manager can add to it.
const ANDROID_ABSENT: &[&str] = &[
    "a package manager (no apt/apk/pip: what ships is what there is)",
    "python and node",
    "a container runtime (sandboxed calls run on the cluster's sandbox_home, or a peer that has one)",
    "/usr/bin/env (a `#!/usr/bin/env` script fails: run scripts as `bash file`)",
    "systemctl",
    "sudo",
    "a desktop (no xdg-open, no notify-send)",
];

const ANDROID_NOTES: &str =
    "The shell is bash, with busybox ahead of toybox on PATH; git, rg, jq, \
     curl, sqlite3 and ssh are there too (curl and git reach the internet through the engine's \
     proxy; ssh by address only); the home is the app's private files dir (run-as); the engine \
     stops after 5 idle minutes.";

/// The phone with the userland package answering (PLAN-phone-userland.md
/// T3 design item 5): the terminal's commands run in its Alpine guest,
/// where `apk` installs anything and there is an FHS with /usr/bin/env.
const ANDROID_ABSENT_USERLAND: &[&str] = &[
    "python and node (not installed)",
    "a container runtime (sandboxed calls run on the cluster's sandbox_home when one is set, \
     else in the userland's own proot guests, the default here unless [sandbox] backend = \
     \"native\", or on a peer that has a runtime)",
    "systemctl",
    "sudo (the terminal is root already)",
    "a desktop (no xdg-open, no notify-send)",
];

const ANDROID_NOTES_USERLAND: &str =
    "The terminal's shell is ulsh: every command runs in an Alpine Linux guest \
     (the Veles userland app, under proot) as its root, with `apk add <package>` for anything \
     missing and an FHS with /usr/bin/env; Alpine's sh until `apk add bash`. The guest's home \
     is /root, and the guest is a different filesystem from the app's: read_file, write_file \
     and search_files work on the app's own files dir (with the APK's git, rg, jq, curl, sqlite3 \
     and ssh), which the guest cannot see, nor they the guest's — pass content through tool \
     arguments or do the work in the terminal. Sandboxed boxes are guests of their own \
     (files/sandboxes/<box> in the userland, each a copy of its rootfs): apart in their files, \
     on the phone's network. The engine stops after 5 idle minutes.";

/// What a model used to Linux will not find on `os` (an [`super::OS`]
/// value): nothing on Linux — this IS Linux, or near enough (the BSDs
/// differ only in package managers) — and the phone's list on Android.
/// `userland`: the userland package answers (`crate::userland::in_use`,
/// passed in so the phone's facts are testable on a host).
pub fn absent_for(os: &str, userland: bool) -> &'static [&'static str] {
    match (os, userland) {
        ("android", false) => ANDROID_ABSENT,
        ("android", true) => ANDROID_ABSENT_USERLAND,
        _ => &[],
    }
}

/// The one sentence of what is different on `os`, not missing.
pub fn notes_for(os: &str, userland: bool) -> &'static str {
    match (os, userland) {
        ("android", false) => ANDROID_NOTES,
        ("android", true) => ANDROID_NOTES_USERLAND,
        _ => "",
    }
}

// ── processes ──────────────────────────────────────────────────────

/// `kill(pid, 0)`: 0 is alive, EPERM is alive and someone else's, ESRCH
/// is gone.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence/permission check.
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The process's arguments include `gateway`, as `veles gateway run`'s
/// do under every launcher. A process /proc does not show us (another
/// user's, under hidepid, as on Android) is not a gateway we could own.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn runs_a_gateway(pid: u32) -> bool {
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(argv) => argv.split(|b| *b == 0).any(|arg| arg == b"gateway"),
        Err(_) => false,
    }
}

/// No /proc to ask: alive has to do.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn runs_a_gateway(_pid: u32) -> bool {
    true
}

/// Ask `pid` to stop the way its service manager would: SIGTERM. `true`
/// when the platform has such a signal and it was sent.
pub fn request_stop(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: a plain kill(2) on a pid the caller read from the pid file.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) == 0 }
}

/// End `pid` now (SIGKILL).
pub fn kill(pid: u32) {
    if pid == 0 || pid > i32::MAX as u32 {
        return;
    }
    // SAFETY: as above.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// Make the child lead its own process group, so a cancel or a timeout
/// can end the whole tree it starts ([`Tree::kill`]).
pub fn own_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
}

/// [`own_group`] for a std `Command`.
pub fn own_group_std(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

/// A spawned child's process tree, by its group id (the child's own
/// pid once [`own_group`] made it a leader).
pub struct Tree {
    pgid: u32,
}

/// The tree a child started with [`own_group`] leads. Never fails here.
pub fn adopt(child_pid: u32) -> Option<Tree> {
    (child_pid != 0).then_some(Tree { pgid: child_pid })
}

impl Tree {
    /// SIGKILL the whole group. ESRCH (already gone) is the normal race.
    pub fn kill(&self) {
        if self.pgid == 0 || self.pgid > i32::MAX as u32 {
            return;
        }
        // SAFETY: kill(2) with a negative pgid signals the group; the pid
        // came from a child this process spawned.
        unsafe {
            libc::kill(-(self.pgid as libc::pid_t), libc::SIGKILL);
        }
    }
}

/// A memory ceiling on an already-running child. On unix the limit is
/// set BEFORE exec by the `ulimit -v` wrapper (`sandbox.rs`, which also
/// says why no closure may run in the child), so there is nothing to do
/// here; `false` tells the caller the wrapper is the mechanism.
pub fn limit_memory(_pid: u32, _bytes: u64) -> bool {
    false
}

/// `gethostname`, or `localhost`.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: a writable buffer of the length given.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return "localhost".to_string();
    }
    let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..nul])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

/// Something stable per user, for names that must not collide between
/// users of one machine: the uid.
pub fn user_tag() -> String {
    // SAFETY: getuid never fails.
    unsafe { libc::getuid() }.to_string()
}

// ── files ──────────────────────────────────────────────────────────

/// Create `dir` (and parents) owner-only; an existing one is narrowed
/// to 0700 whatever made it.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = std::fs::metadata(dir)?;
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Narrow an existing file to the owner (0600).
pub fn restrict_file(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Options that create a file owner-only from the first byte.
pub fn private_open_options(opts: &mut std::fs::OpenOptions) {
    opts.mode(0o600);
}

/// Write `bytes` to `path` so that no moment exists where the file holds
/// them at the umask's bits, including when it already existed wider.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    private_open_options(&mut opts);
    let mut f = opts.open(path)?;
    // `mode` only applies to a file this call CREATES; an existing one
    // keeps its bits until this.
    restrict_file(path)?;
    f.write_all(bytes)
}

/// Publish a completed file beside its destination with one rename.
/// The caller owns staging, permissions and fsync policy.
pub fn replace_file(staged: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(staged, destination)
}

// ── the module loader's view ───────────────────────────────────────

/// The file whose image contains the code at `addr` (`dladdr`). An
/// address, not a pointer: the loader only looks it up in its tables.
pub fn image_of(addr: usize) -> Option<PathBuf> {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: dladdr only reads the loader's tables; `info` is a plain
    // out-parameter; the address is never dereferenced.
    if unsafe { libc::dladdr(addr as *const std::ffi::c_void, &mut info) } == 0
        || info.dli_fname.is_null()
    {
        return None;
    }
    Some(PathBuf::from(
        unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }
            .to_string_lossy()
            .into_owned(),
    ))
}

/// `/proc/self/maps` lines that end in the path. `None` when the file
/// cannot be read (a kernel without procfs, a hidepid mount): the unload
/// proof then has no answer and keeps the library mapped.
pub(super) fn mapped_ranges_impl(path: &Path) -> Option<Vec<(usize, usize)>> {
    let want = path.to_string_lossy();
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    Some(
        maps.lines()
            .filter(|l| l.ends_with(want.as_ref()))
            .filter_map(|l| {
                let (lo, hi) = l.split_whitespace().next()?.split_once('-')?;
                Some((
                    usize::from_str_radix(lo, 16).ok()?,
                    usize::from_str_radix(hi, 16).ok()?,
                ))
            })
            .collect(),
    )
}

// ── the local IPC socket ───────────────────────────────────────────

/// A bound unix socket at the marker path itself.
pub struct IpcListener {
    inner: UnixListener,
}

/// One connection.
pub struct IpcStream {
    inner: UnixStream,
}

/// Bind the socket at `marker` (the `<pid>.sock` file), 0600. A stale
/// file at the path belongs to a dead process with the same pid.
pub fn ipc_bind(marker: &Path) -> std::io::Result<IpcListener> {
    let _ = std::fs::remove_file(marker);
    let inner = UnixListener::bind(marker)?;
    let _ = std::fs::set_permissions(marker, std::fs::Permissions::from_mode(0o600));
    Ok(IpcListener { inner })
}

/// Unlink the marker (orderly exits).
pub fn ipc_unbind(marker: &Path) {
    let _ = std::fs::remove_file(marker);
}

/// Connect to the socket at `marker`.
pub fn ipc_connect(marker: &Path) -> std::io::Result<IpcStream> {
    Ok(IpcStream {
        inner: UnixStream::connect(marker)?,
    })
}

impl IpcListener {
    pub fn accept(&mut self) -> std::io::Result<IpcStream> {
        let (inner, _) = self.inner.accept()?;
        Ok(IpcStream { inner })
    }
}

impl IpcStream {
    /// One request's whole budget, read and write.
    pub fn set_timeout(&self, d: std::time::Duration) -> std::io::Result<()> {
        self.inner.set_read_timeout(Some(d))?;
        self.inner.set_write_timeout(Some(d))
    }

    /// The peer's uid matches ours (`SO_PEERCRED`). Linux and Android
    /// only; elsewhere the directory's 0700 is the whole guard.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn peer_same_user(&self) -> bool {
        use std::os::unix::io::AsRawFd;
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: cred and len are valid for the call; the fd is open.
        let r = unsafe {
            libc::getsockopt(
                self.inner.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut libc::ucred as *mut libc::c_void,
                &mut len,
            )
        };
        // SAFETY: getuid never fails.
        r == 0 && cred.uid == unsafe { libc::getuid() }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub fn peer_same_user(&self) -> bool {
        true
    }
}

impl Read for IpcStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}
impl Read for &IpcStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&self.inner).read(buf)
    }
}
impl Write for IpcStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
impl Write for &IpcStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&self.inner).write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (&self.inner).flush()
    }
}

// ── the shell ──────────────────────────────────────────────────────

/// The shell when `VELES_SHELL` says nothing: bash, else the POSIX
/// floor (`-c` is POSIX-specified, so sh is safe where bash is absent).
pub fn default_shell() -> String {
    if crate::shell::which("bash").is_some() {
        "bash".to_string()
    } else {
        "sh".to_string()
    }
}

/// `<shell> -c <script>`.
pub fn shell_args(_shell: &str, script: &str) -> Vec<String> {
    vec!["-c".to_string(), script.to_string()]
}

/// The file names a bare command may resolve to on PATH: the name.
pub fn exe_candidates(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

/// Environment a plugin child needs beyond the portable allowlist.
pub const CHILD_ENV_EXTRA: &[&str] = &[];

// ── hidden terminal input ──────────────────────────────────────────

/// Read one line from stdin with echo off when stdin is a terminal (a
/// key must not land on the screen, in the scrollback, or in a shell
/// history). Echo goes off BEFORE the prompt.
pub fn read_hidden(prompt: &str) -> std::io::Result<String> {
    use std::io::IsTerminal as _;
    let tty = std::io::stdin().is_terminal();
    let mut saved: Option<libc::termios> = None;
    if tty {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd 0 is open (is_terminal said so); t is an out-param.
        if unsafe { libc::tcgetattr(0, &mut t) } == 0 {
            saved = Some(t);
            let mut off = t;
            off.c_lflag &= !libc::ECHO;
            // SAFETY: a termios this call just read, with one flag cleared.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &off) };
        }
    }
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut v = String::new();
    let r = std::io::stdin().read_line(&mut v);
    if let Some(t) = saved {
        // SAFETY: restoring what was read above.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        eprintln!();
    }
    r?;
    Ok(v.trim().to_string())
}

// ── an editor on a file ────────────────────────────────────────────

/// `$EDITOR`-style: the value may carry flags (`code --wait`), so a
/// shell parses it; the file goes in as `$1`, never spliced into the
/// string.
pub fn editor_command(editor: &str, file: &Path) -> std::process::Command {
    let mut c = std::process::Command::new("sh");
    c.arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(file);
    c
}

/// The spelling of a canonical path that compares with a plain one. A
/// no-op here: `canonicalize` on unix returns a plain absolute path.
/// (Windows returns a `\\?\` verbatim path — see windows.rs.)
pub fn plain_path(p: std::path::PathBuf) -> std::path::PathBuf {
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phone's prompt facts (PLAN-phone-userland.md T1): the shell is
    /// the APK's bash with its static toolbox, and what is still missing
    /// is said, with the way around it where there is one.
    #[test]
    fn android_facts_name_bash_the_toolbox_and_what_is_missing() {
        let notes = notes_for("android", false);
        for tool in [
            "bash", "busybox", "toybox", "git", "rg", "jq", "curl", "sqlite3", "ssh",
        ] {
            assert!(notes.contains(tool), "notes name {tool}: {notes}");
        }
        let absent = absent_for("android", false).join("; ");
        for gap in [
            "package manager",
            "python",
            "node",
            "container runtime",
            "sandbox_home",
            "/usr/bin/env",
            "bash file",
            "systemctl",
            "sudo",
            "desktop",
        ] {
            assert!(absent.contains(gap), "absent names {gap}: {absent}");
        }
    }

    /// The phone with its userland (PLAN-phone-userland.md T3 design item
    /// 5): the terminal runs in Alpine, so a package manager and
    /// /usr/bin/env are no longer missing; the notes say where the
    /// commands run, what installs things, where the guest's home is, and
    /// that the app's files are another filesystem.
    #[test]
    fn android_facts_with_the_userland_say_alpine_apk_and_the_guests_home() {
        let absent = absent_for("android", true).join("; ");
        for gone in ["package manager", "/usr/bin/env", "bash file"] {
            assert!(!absent.contains(gone), "{gone} is not missing: {absent}");
        }
        for still in [
            "python",
            "node",
            "container runtime",
            "systemctl",
            "desktop",
        ] {
            assert!(absent.contains(still), "absent names {still}: {absent}");
        }
        let notes = notes_for("android", true);
        for said in [
            "Alpine",
            "apk add",
            "/root",
            "/usr/bin/env",
            "read_file",
            "git, rg",
            "files/sandboxes",
            "5 idle minutes",
        ] {
            assert!(notes.contains(said), "notes say {said}: {notes}");
        }
    }

    /// A Linux node is Linux: no list, no sentence, userland or not.
    #[test]
    fn linux_facts_are_empty() {
        for userland in [false, true] {
            assert!(absent_for("linux", userland).is_empty());
            assert!(notes_for("linux", userland).is_empty());
        }
    }
}
