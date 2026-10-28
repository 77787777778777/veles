//! The Windows bodies (x86_64-pc-windows-msvc). Raw `windows-sys`
//! calls, each wrapped at once in a safe function with the same name
//! and meaning as its unix twin in `unix.rs`.
//!
//! Where Windows has no counterpart the function says so instead of
//! pretending: there is no signal to ask a process to stop
//! ([`request_stop`] is `false`, the IPC `shutdown` request is the
//! way), and a named pipe has no read timeout ([`IpcStream::set_timeout`]
//! is a no-op; the pipe's DACL limits who can hold a handler thread).

use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ACCESS_DENIED, ERROR_MOD_NOT_FOUND,
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, HMODULE,
    INVALID_HANDLE_VALUE, STILL_ACTIVE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, ACL, DACL_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, STD_INPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::ProcessStatus::{K32GetModuleInformation, MODULEINFO};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, TerminateProcess, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
};

/// What a model used to Linux reaches for and will not find here.
pub const ABSENT: &[&str] = &[
    "systemctl",
    "apt/pip as system package managers (winget is the one here)",
    "xdg-open and notify-send (the device tool's open/notify do that)",
    "podman user boxes",
    "bash by default (VELES_SHELL can name one)",
];

pub const NOTES: &str =
    "Paths use backslashes and drive letters; the Tcl fence is the native safe \
                         interpreter under a Job Object memory limit; the gateway runs in the \
                         logged-on user's desktop session.";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: a handle this process opened and still owns.
            unsafe { CloseHandle(self.0) };
        }
    }
}

// ── processes ──────────────────────────────────────────────────────

fn open_process(access: u32, pid: u32) -> Option<Handle> {
    // SAFETY: OpenProcess has no preconditions beyond a valid access mask.
    let h = unsafe { OpenProcess(access, 0, pid) };
    (!h.is_null()).then_some(Handle(h))
}

/// A process with that id exists and has not exited. Access denied is
/// alive (someone else's), as EPERM is on unix.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let Some(h) = open_process(PROCESS_QUERY_LIMITED_INFORMATION, pid) else {
        // SAFETY: GetLastError reads thread-local state.
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    };
    let mut code = 0u32;
    // SAFETY: h is an open process handle; code is an out-param.
    unsafe { GetExitCodeProcess(h.0, &mut code) != 0 && code == STILL_ACTIVE as u32 }
}

/// The process's executable is `veles.exe`: the nearest thing to
/// unix's "its arguments include `gateway`" without reading another
/// process's command line.
pub fn runs_a_gateway(pid: u32) -> bool {
    match image_name(pid) {
        Some(p) => p
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("veles")),
        None => false,
    }
}

fn image_name(pid: u32) -> Option<PathBuf> {
    let h = open_process(PROCESS_QUERY_LIMITED_INFORMATION, pid)?;
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    // SAFETY: buf is writable for len u16s; len is in/out.
    let ok =
        unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) };
    (ok != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}

/// Windows has no SIGTERM: a gateway is asked to stop over the IPC
/// (`runsock` `shutdown`), which `lifecycle::request_stop` does first on
/// every platform. `false` tells the caller nothing was sent.
pub fn request_stop(_pid: u32) -> bool {
    false
}

/// End `pid` now (`TerminateProcess`).
pub fn kill(pid: u32) {
    if let Some(h) = open_process(PROCESS_TERMINATE, pid) {
        // SAFETY: a process handle opened for termination.
        unsafe { TerminateProcess(h.0, 1) };
    }
}

/// A new process group and no console window of its own: the tree a
/// cancel can end ([`Tree::kill`]) without a cmd window flashing on the
/// desktop the gateway runs in.
pub fn own_group(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

/// [`own_group`] for a std `Command`.
pub fn own_group_std(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

/// A Job Object the child was put into right after it was spawned:
/// everything it starts inherits the job, and `TerminateJobObject` ends
/// them all. (A process group on Windows cannot be killed as a group.)
pub struct Tree {
    job: Handle,
}

// SAFETY: a job handle is a kernel object reference; it may be used
// from any thread and the kernel serialises the calls.
unsafe impl Send for Tree {}
unsafe impl Sync for Tree {}

/// Put the spawned child into a fresh job. `None` when the process is
/// already gone or cannot be assigned (then only the child itself can
/// be killed, which is what `kill_on_drop` does anyway).
pub fn adopt(child_pid: u32) -> Option<Tree> {
    let proc_h = open_process(PROCESS_SET_QUOTA | PROCESS_TERMINATE, child_pid)?;
    // SAFETY: an anonymous job with default security.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return None;
    }
    let job = Handle(job);
    // SAFETY: both handles are open; a child of this process may be
    // assigned (nested jobs are allowed since Windows 8).
    if unsafe { AssignProcessToJobObject(job.0, proc_h.0) } == 0 {
        return None;
    }
    Some(Tree { job })
}

impl Tree {
    /// End every process in the job.
    pub fn kill(&self) {
        // SAFETY: an open job handle.
        unsafe { TerminateJobObject(self.job.0, 1) };
    }
}

/// A per-process memory ceiling on `pid`, through a job with
/// `JOB_OBJECT_LIMIT_PROCESS_MEMORY` (the unix `ulimit -v`). The job
/// outlives this handle while its processes do, so the limit holds.
pub fn limit_memory(pid: u32, bytes: u64) -> bool {
    let Some(proc_h) = open_process(PROCESS_SET_QUOTA | PROCESS_TERMINATE, pid) else {
        return false;
    };
    // SAFETY: an anonymous job with default security.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return false;
    }
    let job = Handle(job);
    // SAFETY: a zeroed limit struct is a valid "no limits" value.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    info.ProcessMemoryLimit = bytes as usize;
    // SAFETY: info is the struct the class names, of its own size.
    let set = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    // SAFETY: both handles are open.
    set != 0 && unsafe { AssignProcessToJobObject(job.0, proc_h.0) } != 0
}

/// `COMPUTERNAME`, or `localhost`.
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

/// The user name: stable per user, safe in a file name.
pub fn user_tag() -> String {
    std::env::var("USERNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "user".to_string())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

// ── security descriptors ───────────────────────────────────────────

/// This process's user SID as a string (`S-1-5-21-…`).
fn own_sid() -> std::io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo-handle of the current process; token is an out-param.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error());
    }
    let token = Handle(token);
    let mut len = 0u32;
    // SAFETY: a size query; a null buffer with length 0 is allowed.
    unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    let mut buf = vec![0u8; len.max(1) as usize];
    // SAFETY: buf has the length the first call asked for.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buf.as_mut_ptr() as *mut c_void,
            len,
            &mut len,
        )
    } == 0
    {
        return Err(last_error());
    }
    // SAFETY: the buffer holds a TOKEN_USER as the call documents.
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    let mut s: *mut u16 = std::ptr::null_mut();
    // SAFETY: a valid SID from the token; s receives a LocalAlloc'd string.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut s) } == 0 {
        return Err(last_error());
    }
    let mut n = 0;
    // SAFETY: s is NUL-terminated.
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: n u16s are readable before the NUL.
    let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, n) });
    // SAFETY: LocalAlloc'd by ConvertSidToStringSidW.
    unsafe { LocalFree(s as *mut c_void) };
    Ok(out)
}

/// A LocalAlloc'd security descriptor from SDDL.
struct Sd(PSECURITY_DESCRIPTOR);
impl Drop for Sd {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

fn sddl(s: &str) -> std::io::Result<Sd> {
    let w = wide(s);
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated SDDL string; sd is an out-param.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error());
    }
    Ok(Sd(sd))
}

/// Owner-only (plus SYSTEM) DACL on a file or directory, inherited by
/// what is created below a directory, protected from the parent's.
fn restrict_path(path: &Path) -> std::io::Result<()> {
    let sid = own_sid()?;
    let sd = sddl(&format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)"))?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: sd is a valid descriptor; the rest are out-params.
    if unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) } == 0 {
        return Err(last_error());
    }
    let mut name = wide(&path.to_string_lossy());
    // SAFETY: name is NUL-terminated and writable (the API takes PWSTR).
    let rc = unsafe {
        SetNamedSecurityInfoW(
            name.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

// ── files ──────────────────────────────────────────────────────────

/// Create `dir` (and parents) and narrow it to the owner.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    restrict_path(dir)
}

/// Narrow an existing file to the owner.
pub fn restrict_file(path: &Path) -> std::io::Result<()> {
    restrict_path(path)
}

/// Nothing to set at open time: the DACL is applied by
/// [`restrict_file`] right after creation (the file inherits its
/// directory's ACL until then, which for `%APPDATA%` is already the
/// user's own).
pub fn private_open_options(_opts: &mut std::fs::OpenOptions) {}

/// Write `bytes` to `path` and narrow it to the owner.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        // Truncation happens below only after the owner-only DACL is set.
        .truncate(false)
        .open(path)?;
    // Restrict BEFORE truncation or writing: an existing broad DACL
    // must not expose either the old secret or its replacement.
    restrict_file(path)?;
    f.set_len(0)?;
    f.write_all(bytes)
}

/// Replace an existing file without a remove-then-create window. This
/// is the native same-volume replace operation, not Unix rename claims.
pub fn replace_file(staged: &Path, destination: &Path) -> std::io::Result<()> {
    let from = wide(&staged.to_string_lossy());
    let to = wide(&destination.to_string_lossy());
    // SAFETY: both strings are NUL terminated and remain alive for the call.
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(last_error())
    } else {
        Ok(())
    }
}

// ── the module loader's view ───────────────────────────────────────

fn module_file_name(h: HMODULE) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32768];
    // SAFETY: buf is writable for its length.
    let n = unsafe { GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) };
    (n > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])))
}

/// The file whose image contains the code at `addr`.
pub fn image_of(addr: usize) -> Option<PathBuf> {
    let mut h: HMODULE = std::ptr::null_mut();
    // SAFETY: with FROM_ADDRESS the "name" is an address; h is an out-param.
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            addr as *const u16,
            &mut h,
        )
    };
    if ok == 0 || h.is_null() {
        return None;
    }
    module_file_name(h)
}

/// `[base, base + SizeOfImage)` of the loaded module at `path`; an
/// empty list when no module with that path is loaded; `None` when the
/// loader will not say.
pub(super) fn mapped_ranges_impl(path: &Path) -> Option<Vec<(usize, usize)>> {
    let shown = path.to_string_lossy();
    let shown = shown.strip_prefix(r"\\?\").unwrap_or(&shown);
    let w = wide(shown);
    let mut h: HMODULE = std::ptr::null_mut();
    // SAFETY: a NUL-terminated path; h is an out-param; the refcount is
    // left alone so this probe never pins the module.
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            w.as_ptr(),
            &mut h,
        )
    };
    if ok == 0 || h.is_null() {
        // SAFETY: GetLastError reads thread-local state.
        return (unsafe { GetLastError() } == ERROR_MOD_NOT_FOUND).then(Vec::new);
    }
    // SAFETY: a zeroed MODULEINFO is a valid out-param.
    let mut mi: MODULEINFO = unsafe { std::mem::zeroed() };
    // SAFETY: h is a module of this process; mi has the size given.
    let ok = unsafe {
        K32GetModuleInformation(
            GetCurrentProcess(),
            h,
            &mut mi,
            std::mem::size_of::<MODULEINFO>() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let base = mi.lpBaseOfDll as usize;
    Some(vec![(base, base + mi.SizeOfImage as usize)])
}

// ── the local IPC: a named pipe per process ────────────────────────

/// The pipe name a marker file stands for: `\\.\pipe\veles-<user>-<dir
/// tag>-<pid>`, so two data dirs (or two users) never share one.
fn pipe_name_for(marker: &Path) -> String {
    use sha2::{Digest, Sha256};
    let dir = marker
        .parent()
        .map(|p| p.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let tag: String = Sha256::digest(dir.as_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect();
    let stem = marker
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(r"\\.\pipe\veles-{}-{tag}-{stem}", user_tag())
}

/// A pipe name waiting for clients; each accept is one instance.
pub struct IpcListener {
    name: Vec<u16>,
    sd: Sd,
}

// SAFETY: the descriptor is a LocalAlloc'd blob this listener owns and
// only reads; the accept thread may hold it like any other heap data.
unsafe impl Send for IpcListener {}

/// One connection: a pipe handle read and written like a file.
pub struct IpcStream {
    inner: std::fs::File,
}

/// Write the pipe's name into the marker file (what [`ipc_connect`]
/// reads) and get ready to accept. The pipe's DACL admits this user
/// only, which is the `SO_PEERCRED` check done by the kernel instead.
pub fn ipc_bind(marker: &Path) -> std::io::Result<IpcListener> {
    let name = pipe_name_for(marker);
    let sid = own_sid()?;
    let sd = sddl(&format!("D:P(A;;GA;;;{sid})"))?;
    std::fs::write(marker, &name)?;
    Ok(IpcListener {
        name: wide(&name),
        sd,
    })
}

/// Remove the marker (orderly exits).
pub fn ipc_unbind(marker: &Path) {
    let _ = std::fs::remove_file(marker);
}

impl IpcListener {
    /// Make one instance and wait for a client on it.
    pub fn accept(&mut self) -> std::io::Result<IpcStream> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd.0,
            bInheritHandle: 0,
        };
        // SAFETY: a NUL-terminated name; sa points at a live descriptor.
        let h = unsafe {
            CreateNamedPipeW(
                self.name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                &sa,
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return Err(last_error());
        }
        let h = Handle(h);
        // SAFETY: a pipe instance handle; blocking connect.
        if unsafe { ConnectNamedPipe(h.0, std::ptr::null_mut()) } == 0 {
            // SAFETY: GetLastError reads thread-local state.
            if unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                return Err(last_error());
            }
        }
        let raw = h.0;
        std::mem::forget(h);
        // SAFETY: the File takes ownership of an open pipe handle.
        Ok(IpcStream {
            inner: unsafe { std::fs::File::from_raw_handle(raw as _) },
        })
    }
}

/// Open the pipe the marker names. A busy pipe (the server between two
/// instances) is waited for, briefly.
pub fn ipc_connect(marker: &Path) -> std::io::Result<IpcStream> {
    let name = std::fs::read_to_string(marker)?;
    let name = name.trim();
    if !name.starts_with(r"\\.\pipe\") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the marker names no pipe",
        ));
    }
    let w = wide(name);
    for _ in 0..5 {
        // SAFETY: a NUL-terminated name; no security attributes, no template.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE && !h.is_null() {
            // SAFETY: the File takes ownership of the open handle.
            return Ok(IpcStream {
                inner: unsafe { std::fs::File::from_raw_handle(h as _) },
            });
        }
        // SAFETY: GetLastError reads thread-local state.
        if unsafe { GetLastError() } != ERROR_PIPE_BUSY {
            return Err(last_error());
        }
        // SAFETY: a NUL-terminated name.
        unsafe { WaitNamedPipeW(w.as_ptr(), 2000) };
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "the pipe stayed busy",
    ))
}

impl IpcStream {
    /// Named pipes opened this way have no per-read timeout; the DACL
    /// keeps the peer our own user, and each request has its own thread.
    pub fn set_timeout(&self, _d: std::time::Duration) -> std::io::Result<()> {
        Ok(())
    }

    /// The kernel already refused anyone but this user at open time.
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

/// The shell when `VELES_SHELL` says nothing (USER 2026-10-13):
/// PowerShell 7, then Windows PowerShell, then cmd.
pub fn default_shell() -> String {
    for s in ["pwsh", "powershell"] {
        if crate::shell::which(s).is_some() {
            return s.to_string();
        }
    }
    "cmd".to_string()
}

/// How this shell takes a script on its command line.
pub fn shell_args(shell: &str, script: &str) -> Vec<String> {
    let stem = Path::new(shell)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if stem == "cmd" {
        vec!["/D".into(), "/C".into(), script.to_string()]
    } else if stem == "pwsh" || stem == "powershell" {
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script.to_string(),
        ]
    } else {
        // bash.exe (Git for Windows), busybox sh, …: POSIX `-c`.
        vec!["-c".into(), script.to_string()]
    }
}

/// The file names a bare command may resolve to on PATH, in PATHEXT
/// order (`.exe`, `.cmd`, `.bat`, …), then the name itself. A name with
/// an extension is tried as given.
pub fn exe_candidates(name: &str) -> Vec<String> {
    if Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let mut v: Vec<String> = exts
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|e| format!("{name}{}", e.to_ascii_lowercase()))
        .collect();
    v.push(name.to_string());
    v
}

/// What a child needs on Windows beyond the portable allowlist: without
/// `SystemRoot` Winsock does not initialise and no plugin can open a
/// socket; the rest are what every Windows program assumes.
pub const CHILD_ENV_EXTRA: &[&str] = &[
    "SystemRoot",
    "windir",
    "SystemDrive",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "TEMP",
    "TMP",
    "PATHEXT",
    "ComSpec",
    "NUMBER_OF_PROCESSORS",
];

// ── hidden terminal input ──────────────────────────────────────────

/// Read one line from stdin with echo off when stdin is a console.
pub fn read_hidden(prompt: &str) -> std::io::Result<String> {
    use std::io::IsTerminal as _;
    let tty = std::io::stdin().is_terminal();
    let mut saved: Option<(HANDLE, u32)> = None;
    if tty {
        // SAFETY: the standard input handle; mode is an out-param.
        let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut mode = 0u32;
        if !h.is_null() && h != INVALID_HANDLE_VALUE && unsafe { GetConsoleMode(h, &mut mode) } != 0
        {
            saved = Some((h, mode));
            // SAFETY: the mode this call just read, with echo cleared.
            unsafe { SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT) };
        }
    }
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut v = String::new();
    let r = std::io::stdin().read_line(&mut v);
    if let Some((h, mode)) = saved {
        // SAFETY: restoring what was read above.
        unsafe { SetConsoleMode(h, mode) };
        eprintln!();
    }
    r?;
    Ok(v.trim().to_string())
}

// ── an editor on a file ────────────────────────────────────────────

/// `%EDITOR%`-style: the first word is the program (resolved through
/// PATHEXT, so `code` finds `code.cmd`), the rest its flags, the file
/// last. `notepad` when nothing is set.
pub fn editor_command(editor: &str, file: &Path) -> std::process::Command {
    let mut words = editor.split_whitespace();
    let prog = words.next().unwrap_or("notepad");
    let mut c = std::process::Command::new(crate::shell::resolve_program(prog));
    c.args(words).arg(file);
    c
}

/// The spelling of a canonical path that compares with a plain one.
/// `std::fs::canonicalize` returns a verbatim path here (`\\?\C:\…`,
/// `\\?\UNC\host\share\…`), and `Path::starts_with` never matches it
/// against the plain `C:\…` a config or a caller spelled — so every
/// prefix fence that canonicalizes one side and not the other (a path
/// that does not exist yet keeps its plain form) said "outside" for a
/// file inside the media root (2026-10-13, winbox). Strip the
/// verbatim prefix; nothing veles compares needs paths over 260 chars.
pub fn plain_path(p: std::path::PathBuf) -> std::path::PathBuf {
    use std::path::{Component, Prefix};
    let mut comps = p.components();
    let Some(Component::Prefix(pre)) = comps.next() else {
        return p;
    };
    match pre.kind() {
        Prefix::VerbatimDisk(d) => {
            let mut out = std::path::PathBuf::from(format!("{}:\\", d as char));
            out.extend(comps.filter(|c| !matches!(c, Component::RootDir)));
            out
        }
        Prefix::VerbatimUNC(host, share) => {
            let mut out = std::path::PathBuf::from(format!(
                "\\\\{}\\{}\\",
                host.to_string_lossy(),
                share.to_string_lossy()
            ));
            out.extend(comps.filter(|c| !matches!(c, Component::RootDir)));
            out
        }
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
    use windows_sys::Win32::Security::{GetAce, ACCESS_ALLOWED_ACE};
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    fn sid_text(sid: windows_sys::Win32::Security::PSID) -> String {
        let mut text = std::ptr::null_mut();
        assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut text) }, 0);
        let mut len = 0;
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        unsafe { LocalFree(text.cast()) };
        out
    }

    #[test]
    fn a_backup_rewritten_in_place_has_no_broad_account_access() {
        let dir = std::env::temp_dir().join(format!("veles-backup-acl-{}", std::process::id()));
        private_dir(&dir).unwrap();
        let backup = dir.join("config.toml.bak");
        std::fs::write(&backup, "first secret").unwrap();
        write_private(&backup, b"replacement secret").unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), b"replacement secret");

        // Inspect the real DACL rather than inferring it from a working
        // read. This test runs on the native Windows host, not through
        // a Unix permission-mode approximation.
        let name = wide(&backup.to_string_lossy());
        let mut sd = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let rc = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(rc, 0, "GetNamedSecurityInfoW failed: {rc}");
        assert!(!sd.is_null());
        let descriptor = Sd(sd);
        let mut present = 0;
        let mut defaulted = 0;
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
            },
            0
        );
        assert_ne!(present, 0, "the backup must have an explicit DACL");
        assert!(!dacl.is_null());
        let mut allowed = Vec::new();
        for index in 0..unsafe { (*dacl).AceCount } {
            let mut ace = std::ptr::null_mut();
            assert_ne!(unsafe { GetAce(dacl, index.into(), &mut ace) }, 0);
            let kind = unsafe { (*ace.cast::<windows_sys::Win32::Security::ACE_HEADER>()).AceType };
            // ACE_HEADER.AceType is a byte in windows-sys; the Win32
            // ACCESS_ALLOWED_ACE_TYPE enumeration has numeric value 0.
            assert_eq!(kind, 0, "unexpected non-allow ACE");
            let entry = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            // SDDL FA is stored as the object-specific full mask, not
            // the pre-mapping GENERIC_ALL bit.
            assert_eq!(entry.Mask, FILE_ALL_ACCESS);
            let sid = sid_text(std::ptr::addr_of!(entry.SidStart).cast_mut().cast());
            for broad in ["S-1-1-0", "S-1-5-32-545", "S-1-5-32-546"] {
                assert_ne!(sid, broad, "backup grants a broad account access");
            }
            allowed.push(sid);
        }
        allowed.sort();
        allowed.dedup();
        let mut expected = vec![own_sid().unwrap(), "S-1-5-18".to_owned()];
        expected.sort();
        assert_eq!(allowed, expected, "backup must be owner+SYSTEM only");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
