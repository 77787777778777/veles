//! What a fleet node says about its machine when the command center asks
//! (0.2.0, owner ask 287976.1: "everything else a command center needs").
//!
//! The registry heartbeat carries one normalized load figure, gossiped
//! every 30 s — enough for placement, not enough to answer "why is the
//! VPS slow". This is the on-demand half: CPU load and core count,
//! memory, free disk where the data home lives, IO pressure and host
//! uptime, sampled when the Instances tab (or `/fleet metrics`) asks, and
//! sent back over the node's own owner-authenticated call
//! (`veles/fleet` in veles-frontends' A2A server). Nothing here is
//! stored or gossiped.
//!
//! Every source is read from KERNEL state, no subprocess. Each OS has
//! its own reader, and one pure step ([`assemble`]) shapes what they
//! read, so the answer has the same keys everywhere:
//!
//! - Linux and Android: /proc (`loadavg`, `meminfo`, `pressure/io`,
//!   `uptime`) and `statvfs(3)`.
//! - Windows (0.2.2, audit FLEET-7/COMP-3): there is no load average, so
//!   the CPU figure is the busy share from `GetSystemTimes` deltas
//!   ([`cpu_busy`], `cpu.busy_pct`); memory from `GlobalMemoryStatusEx`,
//!   free disk from `GetDiskFreeSpaceExW`, uptime from `GetTickCount64`.
//!   The heartbeat's load figure reads the SAME sampler
//!   (`node::probe_load_one`), so `any` placement ranks a Windows node
//!   by its CPU instead of last as unknown. No IO pressure figure.
//! - macOS: `getloadavg(3)`, `statvfs(3)`, memory from `hw.memsize` and
//!   `host_statistics64`, uptime from `kern.boottime`. No IO pressure.
//! - The other unixes: whatever /proc they mount, `getloadavg(3)` and
//!   `statvfs(3)`.
//!
//! A figure that cannot be read is absent and NAMED in `unavailable` —
//! an unknown is never reported as zero (the same rule `node::LoadOne`
//! keeps for placement).

use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Sample this machine. `disk_path` is the filesystem to report (the
/// data home: where the store, the logs and the inboxes grow).
pub fn sample(disk_path: &Path) -> Value {
    assemble(&readings(disk_path), disk_path)
}

/// [`sample`] of a Linux-shaped machine, reading /proc from `proc_root`
/// (tests point it at a fixture tree).
pub fn sample_with(proc_root: &Path, disk_path: &Path) -> Value {
    assemble(&proc_readings(proc_root, disk_path), disk_path)
}

/// What one sample read, before it is shaped. `None` is unknown, never
/// zero; `unavailable` names, per figure, the source that was missing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Readings {
    pub cores: Option<usize>,
    /// 1-, 5- and 15-minute load averages.
    pub load: Option<(f64, f64, f64)>,
    /// The CPU busy share, where the OS keeps no load average.
    pub busy: Option<CpuBusy>,
    pub memory: Option<Memory>,
    /// (total, available-to-us) bytes of the filesystem at `disk_path`.
    pub disk: Option<(u64, u64)>,
    pub io: Option<Pressure>,
    pub uptime_secs: Option<f64>,
    pub unavailable: Vec<String>,
}

/// Physical memory, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
    pub swap_total: Option<u64>,
    pub swap_free: Option<u64>,
}

impl From<MemInfo> for Memory {
    fn from(m: MemInfo) -> Self {
        Memory {
            total: m.total_kib * 1024,
            available: m.available_kib * 1024,
            swap_total: m.swap_total_kib.map(|v| v * 1024),
            swap_free: m.swap_free_kib.map(|v| v * 1024),
        }
    }
}

/// Shape what a reader got into the answer: `cpu`, `memory`, `disk`,
/// `io_pressure`, `host_uptime_secs` and `unavailable`. Pure.
pub fn assemble(r: &Readings, disk_path: &Path) -> Value {
    let mut out = Map::new();
    let mut cpu = Map::new();
    cpu.insert("cores".into(), json!(r.cores));
    if let Some((one, five, fifteen)) = r.load {
        cpu.insert("load1".into(), json!(one));
        cpu.insert("load5".into(), json!(five));
        cpu.insert("load15".into(), json!(fifteen));
        if let Some(c) = r.cores.filter(|&c| c > 0) {
            cpu.insert(
                "load1_pct".into(),
                json!(((one / c as f64) * 100.0).round() as i64),
            );
        }
    }
    if let Some(b) = r.busy {
        cpu.insert("busy_pct".into(), json!((b.pct * 10.0).round() / 10.0));
        cpu.insert(
            "busy_window_secs".into(),
            json!((b.window_secs * 10.0).round() / 10.0),
        );
    }
    out.insert("cpu".into(), Value::Object(cpu));

    if let Some(m) = r.memory {
        let used_pct = (m.total > 0).then(|| {
            let used = m.total.saturating_sub(m.available) as f64;
            (used / m.total as f64 * 1000.0).round() / 10.0
        });
        out.insert(
            "memory".into(),
            json!({
                "total_bytes": m.total,
                "available_bytes": m.available,
                "used_pct": used_pct,
                "swap_total_bytes": m.swap_total,
                "swap_free_bytes": m.swap_free,
            }),
        );
    }

    if let Some((total, free)) = r.disk {
        let used_pct = (total > 0)
            .then(|| ((total.saturating_sub(free)) as f64 / total as f64 * 1000.0).round() / 10.0);
        out.insert(
            "disk".into(),
            json!({
                "path": disk_path.display().to_string(),
                "total_bytes": total,
                "free_bytes": free,
                "used_pct": used_pct,
            }),
        );
    }

    if let Some(p) = r.io {
        let mut io = Map::new();
        for (kind, avgs) in [("some", Some(p.some)), ("full", p.full)] {
            if let Some((a10, a60, a300)) = avgs {
                io.insert(format!("{kind}_avg10"), json!(a10));
                io.insert(format!("{kind}_avg60"), json!(a60));
                io.insert(format!("{kind}_avg300"), json!(a300));
            }
        }
        out.insert("io_pressure".into(), Value::Object(io));
    }

    if let Some(secs) = r.uptime_secs {
        out.insert("host_uptime_secs".into(), json!(secs as u64));
    }
    out.insert("unavailable".into(), json!(r.unavailable));
    Value::Object(out)
}

/// This OS's reader.
fn readings(disk_path: &Path) -> Readings {
    #[cfg(windows)]
    {
        windows::readings(disk_path)
    }
    #[cfg(target_os = "macos")]
    {
        macos::readings(disk_path)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        proc_readings(Path::new("/proc"), disk_path)
    }
}

fn cores() -> Option<usize> {
    std::thread::available_parallelism().ok().map(|n| n.get())
}

/// Linux, Android and the other unixes: /proc under `proc_root`, with
/// `getloadavg(3)` behind a missing `loadavg` when it is the real /proc.
fn proc_readings(proc_root: &Path, disk_path: &Path) -> Readings {
    let read = |name: &str| std::fs::read_to_string(proc_root.join(name)).ok();
    let mut r = Readings {
        cores: cores(),
        ..Readings::default()
    };
    r.load = read("loadavg")
        .as_deref()
        .and_then(parse_loadavg)
        .or_else(|| (proc_root == Path::new("/proc")).then(getloadavg).flatten());
    if r.load.is_none() {
        r.unavailable
            .push("load: no /proc/loadavg and no getloadavg(3)".into());
    }
    r.memory = read("meminfo")
        .as_deref()
        .and_then(parse_meminfo)
        .map(Memory::from);
    if r.memory.is_none() {
        r.unavailable.push("memory: no /proc/meminfo".into());
    }
    r.disk = disk_of(disk_path, &mut r.unavailable);
    r.io = read("pressure/io").as_deref().and_then(parse_pressure);
    if r.io.is_none() {
        r.unavailable
            .push("io_pressure: no /proc/pressure/io (a kernel without PSI, or not Linux)".into());
    }
    r.uptime_secs = read("uptime").as_deref().and_then(parse_uptime);
    if r.uptime_secs.is_none() {
        r.unavailable.push("host uptime: no /proc/uptime".into());
    }
    r
}

fn disk_of(disk_path: &Path, unavailable: &mut Vec<String>) -> Option<(u64, u64)> {
    let d = disk(disk_path);
    if d.is_none() {
        unavailable.push(format!(
            "disk: the free space of {} could not be read",
            disk_path.display()
        ));
    }
    d
}

// ── the CPU busy share (Windows) ──────────────────────────────────────

/// CPU time counters as Windows' `GetSystemTimes` reports them, summed
/// over every CPU: `kernel` INCLUDES `idle`. Any one unit (100 ns there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub idle: u64,
    pub kernel: u64,
    pub user: u64,
}

/// The share of CPU time spent busy between two samples, 0-100: the
/// non-idle part of the kernel + user time that passed. `None` when no
/// time passed or a counter went backwards.
pub fn cpu_busy_percent(prev: CpuTimes, now: CpuTimes) -> Option<f64> {
    let idle = now.idle.checked_sub(prev.idle)?;
    let total = now
        .kernel
        .checked_sub(prev.kernel)?
        .checked_add(now.user.checked_sub(prev.user)?)?;
    if total == 0 || idle > total {
        return None;
    }
    Some((total - idle) as f64 / total as f64 * 100.0)
}

/// A busy share and the span it was measured over.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuBusy {
    pub pct: f64,
    pub window_secs: f64,
}

/// What the busy sampler keeps between asks: the counters of the last
/// sample, when they were read, and the figure computed then.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuWindow {
    at: Instant,
    times: CpuTimes,
    last: Option<CpuBusy>,
}

/// The shortest span a busy share is computed over. An ask sooner than
/// this after the previous one (a metrics request just before the
/// heartbeat) is answered with the previous figure rather than a few
/// milliseconds of noise.
pub const MIN_CPU_WINDOW: Duration = Duration::from_millis(500);

/// One sample against the sampler's state: the next state and the busy
/// share. Pure.
pub fn cpu_window_step(
    prev: Option<CpuWindow>,
    at: Instant,
    times: CpuTimes,
) -> (CpuWindow, Option<CpuBusy>) {
    match prev {
        None => (
            CpuWindow {
                at,
                times,
                last: None,
            },
            None,
        ),
        Some(w) if at.saturating_duration_since(w.at) < MIN_CPU_WINDOW => (w, w.last),
        Some(w) => {
            let busy = cpu_busy_percent(w.times, times).map(|pct| CpuBusy {
                pct,
                window_secs: at.saturating_duration_since(w.at).as_secs_f64(),
            });
            (
                CpuWindow {
                    at,
                    times,
                    last: busy,
                },
                busy,
            )
        }
    }
}

fn cpu_window() -> &'static Mutex<Option<CpuWindow>> {
    static W: OnceLock<Mutex<Option<CpuWindow>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

/// The machine's CPU busy share since the previous ask, where the OS
/// keeps CPU time counters but no load average (Windows). ONE sampler
/// per process, shared by the heartbeat's load figure and the fleet
/// metrics: on the heartbeat cadence it is the busy share of the last
/// interval. The first ask in a process has nothing to measure against
/// and takes two samples [`MIN_CPU_WINDOW`] apart (it runs on a blocking
/// thread in both callers). `None` elsewhere, or when the counters
/// cannot be read.
pub fn cpu_busy() -> Option<CpuBusy> {
    cpu_busy_with(
        cpu_window(),
        read_cpu_times,
        Instant::now,
        std::thread::sleep,
    )
}

fn cpu_busy_with(
    state: &Mutex<Option<CpuWindow>>,
    read: impl Fn() -> Option<CpuTimes>,
    mut clock: impl FnMut() -> Instant,
    pause: impl Fn(Duration),
) -> Option<CpuBusy> {
    let mut w = state.lock().unwrap_or_else(|p| p.into_inner());
    if w.is_none() {
        let first = read()?;
        *w = Some(cpu_window_step(None, clock(), first).0);
        pause(MIN_CPU_WINDOW);
    }
    let times = read()?;
    let (next, busy) = cpu_window_step(*w, clock(), times);
    *w = Some(next);
    busy
}

#[cfg(windows)]
fn read_cpu_times() -> Option<CpuTimes> {
    windows::cpu_times()
}

#[cfg(not(windows))]
fn read_cpu_times() -> Option<CpuTimes> {
    None
}

// ── the /proc parsers ─────────────────────────────────────────────────

/// `/proc/loadavg`: the 1-, 5- and 15-minute figures.
pub fn parse_loadavg(text: &str) -> Option<(f64, f64, f64)> {
    let mut it = text.split_whitespace().map(finite);
    Some((it.next()??, it.next()??, it.next()??))
}

fn finite(s: &str) -> Option<f64> {
    let v = s.parse::<f64>().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    pub total_kib: u64,
    pub available_kib: u64,
    pub swap_total_kib: Option<u64>,
    pub swap_free_kib: Option<u64>,
}

/// `/proc/meminfo`. `MemAvailable` (Linux 3.14+) is the kernel's own
/// estimate of what a new workload can use; without it, free + buffers
/// + cached is the old approximation.
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut fields = std::collections::HashMap::new();
    for line in text.lines() {
        let Some((k, rest)) = line.split_once(':') else {
            continue;
        };
        if let Some(v) = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        {
            fields.insert(k.trim().to_string(), v);
        }
    }
    let total = *fields.get("MemTotal")?;
    let available = match fields.get("MemAvailable") {
        Some(v) => *v,
        None => {
            fields.get("MemFree")?
                + fields.get("Buffers").copied().unwrap_or(0)
                + fields.get("Cached").copied().unwrap_or(0)
        }
    };
    Some(MemInfo {
        total_kib: total,
        available_kib: available.min(total),
        swap_total_kib: fields.get("SwapTotal").copied(),
        swap_free_kib: fields.get("SwapFree").copied(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pressure {
    /// avg10, avg60, avg300 of `some` — the share of time at least one
    /// task waited on IO.
    pub some: (f64, f64, f64),
    /// The same for `full` (every non-idle task waited), when reported.
    pub full: Option<(f64, f64, f64)>,
}

/// `/proc/pressure/io` (PSI): `some avg10=… avg60=… avg300=… total=…`
/// and, on most kernels, a `full` line.
pub fn parse_pressure(text: &str) -> Option<Pressure> {
    let line = |kind: &str| -> Option<(f64, f64, f64)> {
        let l = text
            .lines()
            .find(|l| l.split_whitespace().next() == Some(kind))?;
        let get = |name: &str| {
            l.split_whitespace()
                .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
                .and_then(finite)
        };
        Some((get("avg10")?, get("avg60")?, get("avg300")?))
    };
    Some(Pressure {
        some: line("some")?,
        full: line("full"),
    })
}

/// `/proc/uptime`: seconds since boot.
pub fn parse_uptime(text: &str) -> Option<f64> {
    text.split_whitespace().next().and_then(finite)
}

#[cfg(all(unix, not(target_os = "android")))]
fn getloadavg() -> Option<(f64, f64, f64)> {
    let mut v = [0.0f64; 3];
    // getloadavg returns how many of the requested samples it wrote.
    let got = unsafe { libc::getloadavg(v.as_mut_ptr(), 3) };
    (got == 3 && v.iter().all(|x| x.is_finite() && *x >= 0.0)).then_some((v[0], v[1], v[2]))
}

// The `libc` crate declares no getloadavg for Android (node.rs says so):
// /proc/loadavg is the only source there. Windows has no load average.
#[cfg(any(not(unix), target_os = "android"))]
fn getloadavg() -> Option<(f64, f64, f64)> {
    None
}

/// (total, available-to-us) bytes of the filesystem holding `path`.
// The statvfs field types differ by platform (u64 on Linux, u32 block
// counts on macOS), so a cast that is a no-op here is not one elsewhere.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
pub fn disk(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = if st.f_frsize > 0 {
        st.f_frsize as u64
    } else {
        st.f_bsize as u64
    };
    Some((
        (st.f_blocks as u64).saturating_mul(frsize),
        (st.f_bavail as u64).saturating_mul(frsize),
    ))
}

/// (total, available-to-us) bytes of the volume holding `path`.
#[cfg(windows)]
pub fn disk(path: &Path) -> Option<(u64, u64)> {
    windows::disk(path)
}

#[cfg(not(any(unix, windows)))]
pub fn disk(_path: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(windows)]
mod windows {
    use super::{cores, disk_of, CpuTimes, Memory, Readings};
    use std::path::Path;
    use windows_sys::Win32::Foundation::FILETIME;

    pub(super) fn readings(disk_path: &Path) -> Readings {
        let mut r = Readings {
            cores: cores(),
            busy: super::cpu_busy(),
            ..Readings::default()
        };
        r.unavailable.push(match r.busy {
            Some(_) => {
                "load: Windows keeps no load average (cpu.busy_pct is the CPU busy share instead)"
                    .into()
            }
            None => "load: Windows keeps no load average, and GetSystemTimes failed".into(),
        });
        r.memory = memory();
        if r.memory.is_none() {
            r.unavailable
                .push("memory: GlobalMemoryStatusEx failed".into());
        }
        r.disk = disk_of(disk_path, &mut r.unavailable);
        r.unavailable
            .push("io_pressure: Windows has no pressure-stall figure".into());
        // Milliseconds since boot; a u64, so no 49-day wrap.
        r.uptime_secs = Some(
            unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() } as f64
                / 1000.0,
        );
        r
    }

    fn ft(t: FILETIME) -> u64 {
        (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
    }

    pub(super) fn cpu_times() -> Option<CpuTimes> {
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut idle, mut kernel, mut user) = (zero, zero, zero);
        let ok = unsafe {
            windows_sys::Win32::System::Threading::GetSystemTimes(&mut idle, &mut kernel, &mut user)
        };
        (ok != 0).then(|| CpuTimes {
            idle: ft(idle),
            kernel: ft(kernel),
            user: ft(user),
        })
    }

    fn memory() -> Option<Memory> {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 || m.ullTotalPhys == 0 {
            return None;
        }
        // The page file is not swap (its total includes physical memory,
        // the commit limit), so no swap figure is claimed.
        Some(Memory {
            total: m.ullTotalPhys,
            available: m.ullAvailPhys.min(m.ullTotalPhys),
            swap_total: None,
            swap_free: None,
        })
    }

    pub(super) fn disk(path: &Path) -> Option<(u64, u64)> {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut avail,
                &mut total,
                &mut free,
            )
        };
        (ok != 0 && total > 0).then_some((total, avail))
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{cores, disk_of, getloadavg, Memory, Readings};
    use std::path::Path;

    pub(super) fn readings(disk_path: &Path) -> Readings {
        let mut r = Readings {
            cores: cores(),
            load: getloadavg(),
            ..Readings::default()
        };
        if r.load.is_none() {
            r.unavailable.push("load: getloadavg(3) failed".into());
        }
        r.memory = memory();
        if r.memory.is_none() {
            r.unavailable
                .push("memory: hw.memsize or host_statistics64 failed".into());
        }
        r.disk = disk_of(disk_path, &mut r.unavailable);
        r.unavailable
            .push("io_pressure: macOS has no pressure-stall figure".into());
        r.uptime_secs = uptime();
        if r.uptime_secs.is_none() {
            r.unavailable
                .push("host uptime: kern.boottime failed".into());
        }
        r
    }

    /// One fixed-size sysctl value by name.
    fn sysctl<T: Copy>(name: &std::ffi::CStr) -> Option<T> {
        let mut v = std::mem::MaybeUninit::<T>::zeroed();
        let mut len = std::mem::size_of::<T>();
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                v.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && len == std::mem::size_of::<T>()).then(|| unsafe { v.assume_init() })
    }

    // `mach_host_self` is deprecated in `libc` in favour of the mach2
    // crate; one call does not justify a dependency.
    #[allow(deprecated)]
    fn memory() -> Option<Memory> {
        let total: u64 = sysctl(c"hw.memsize")?;
        let mut stat: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let rc = unsafe {
            libc::host_statistics64(
                libc::mach_host_self(),
                libc::HOST_VM_INFO64,
                (&mut stat as *mut libc::vm_statistics64).cast(),
                &mut count,
            )
        };
        if rc != libc::KERN_SUCCESS || total == 0 {
            return None;
        }
        let page = unsafe { libc::vm_page_size } as u64;
        // What a new workload can have without paging: free, inactive,
        // speculative and purgeable pages (the vm_stat(1) reading).
        let pages = u64::from(stat.free_count)
            + u64::from(stat.inactive_count)
            + u64::from(stat.speculative_count)
            + u64::from(stat.purgeable_count);
        Some(Memory {
            total,
            available: pages.saturating_mul(page).min(total),
            swap_total: None,
            swap_free: None,
        })
    }

    fn uptime() -> Option<f64> {
        let boot: libc::timeval = sysctl(c"kern.boottime")?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        let booted = boot.tv_sec as f64 + boot.tv_usec as f64 / 1e6;
        let up = now.as_secs_f64() - booted;
        (up.is_finite() && up >= 0.0).then_some(up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_proc_files_parse_and_garbage_does_not() {
        assert_eq!(
            parse_loadavg("0.52 1.10 2.00 3/812 12345\n"),
            Some((0.52, 1.10, 2.00))
        );
        assert_eq!(parse_loadavg("nan 1 2"), None);
        assert_eq!(parse_loadavg("1 2"), None);
        let m = parse_meminfo(
            "MemTotal:       16000000 kB\nMemFree:  1000 kB\nMemAvailable:   4000000 kB\n\
             SwapTotal: 2000 kB\nSwapFree: 500 kB\n",
        )
        .unwrap();
        assert_eq!(m.total_kib, 16_000_000);
        assert_eq!(m.available_kib, 4_000_000);
        assert_eq!(m.swap_free_kib, Some(500));
        // A kernel before MemAvailable: free + buffers + cached.
        let old = parse_meminfo("MemTotal: 100 kB\nMemFree: 10 kB\nBuffers: 5 kB\nCached: 20 kB\n")
            .unwrap();
        assert_eq!(old.available_kib, 35);
        assert_eq!(
            parse_meminfo("MemFree: 10 kB\n"),
            None,
            "no total, no figure"
        );
        let p = parse_pressure(
            "some avg10=1.50 avg60=0.75 avg300=0.10 total=123\n\
             full avg10=0.50 avg60=0.25 avg300=0.05 total=45\n",
        )
        .unwrap();
        assert_eq!(p.some, (1.5, 0.75, 0.1));
        assert_eq!(p.full, Some((0.5, 0.25, 0.05)));
        let some_only = parse_pressure("some avg10=2.00 avg60=1.00 avg300=0.50 total=1\n").unwrap();
        assert_eq!(some_only.full, None);
        assert_eq!(parse_pressure("full avg10=1 avg60=1 avg300=1\n"), None);
        assert_eq!(parse_uptime("12345.67 9999.0\n"), Some(12345.67));
    }

    /// A fixture /proc: every figure lands under its name, the load is
    /// normalized by the core count, and a missing source is NAMED in
    /// `unavailable` instead of reading as zero.
    #[test]
    fn a_sample_reports_what_it_read_and_names_what_it_could_not() {
        let proc_dir = tempfile::tempdir().unwrap();
        let p = proc_dir.path();
        std::fs::write(p.join("loadavg"), "2.00 1.00 0.50 1/100 42\n").unwrap();
        std::fs::write(
            p.join("meminfo"),
            "MemTotal: 1000 kB\nMemAvailable: 250 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        )
        .unwrap();
        std::fs::write(p.join("uptime"), "3600.5 100.0\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let v = sample_with(p, data.path());
        assert_eq!(v["cpu"]["load1"], 2.0, "{v}");
        let cores = v["cpu"]["cores"].as_u64().unwrap();
        assert_eq!(
            v["cpu"]["load1_pct"].as_i64().unwrap(),
            (200.0 / cores as f64).round() as i64
        );
        assert_eq!(v["memory"]["total_bytes"], 1000 * 1024);
        assert_eq!(v["memory"]["used_pct"], 75.0);
        assert_eq!(v["host_uptime_secs"], 3600);
        assert!(v.get("io_pressure").is_none(), "{v}");
        let missing = v["unavailable"].as_array().unwrap();
        assert!(
            missing
                .iter()
                .any(|m| m.as_str().unwrap().starts_with("io_pressure")),
            "{v}"
        );
        if cfg!(unix) {
            assert!(v["disk"]["total_bytes"].as_u64().unwrap() > 0, "{v}");
            assert!(
                v["disk"]["free_bytes"].as_u64().unwrap()
                    <= v["disk"]["total_bytes"].as_u64().unwrap()
            );
        }
        std::fs::create_dir(p.join("pressure")).unwrap();
        std::fs::write(
            p.join("pressure/io"),
            "some avg10=3.00 avg60=2.00 avg300=1.00 total=9\nfull avg10=1.00 avg60=0.50 avg300=0.25 total=3\n",
        )
        .unwrap();
        let v = sample_with(p, data.path());
        assert_eq!(v["io_pressure"]["some_avg10"], 3.0, "{v}");
        assert_eq!(v["io_pressure"]["full_avg300"], 0.25);
    }

    /// FLEET-7/COMP-3: Windows keeps CPU time counters, not a load
    /// average. `GetSystemTimes` reports idle, kernel (which includes the
    /// idle time) and user; the busy share between two samples is the
    /// non-idle part of the kernel + user time that passed.
    #[test]
    fn the_cpu_busy_share_comes_from_two_system_time_samples() {
        let t = |idle, kernel, user| CpuTimes { idle, kernel, user };
        // 1000 ticks passed (800 kernel incl. 500 idle, 200 user): half busy.
        assert_eq!(
            cpu_busy_percent(t(1000, 2000, 1000), t(1500, 2800, 1200)),
            Some(50.0)
        );
        // All idle, all busy.
        assert_eq!(cpu_busy_percent(t(0, 0, 0), t(400, 400, 0)), Some(0.0));
        assert_eq!(cpu_busy_percent(t(0, 0, 0), t(0, 100, 300)), Some(100.0));
        // No time passed, a counter went backwards, or more idle than time:
        // unknown, never a figure.
        assert_eq!(cpu_busy_percent(t(5, 5, 5), t(5, 5, 5)), None);
        assert_eq!(cpu_busy_percent(t(10, 10, 10), t(5, 20, 20)), None);
        assert_eq!(cpu_busy_percent(t(0, 0, 0), t(500, 100, 100)), None);
    }

    /// The sampler both callers share: the first ask takes two samples
    /// a window apart; later asks measure since the previous one; an ask
    /// inside the minimum window answers with the previous figure
    /// instead of milliseconds of noise; unreadable counters are unknown.
    #[test]
    fn the_cpu_sampler_measures_since_the_previous_ask() {
        let state = Mutex::new(None);
        let base = Instant::now();
        let clock_at = std::cell::Cell::new(base);
        let samples = std::cell::RefCell::new(
            vec![
                CpuTimes {
                    idle: 0,
                    kernel: 0,
                    user: 0,
                },
                // +1000: 250 busy.
                CpuTimes {
                    idle: 750,
                    kernel: 900,
                    user: 100,
                },
                // +1000: 800 busy.
                CpuTimes {
                    idle: 950,
                    kernel: 1500,
                    user: 500,
                },
                // Read inside the minimum window: not used for a figure.
                CpuTimes {
                    idle: 951,
                    kernel: 1502,
                    user: 500,
                },
            ]
            .into_iter(),
        );
        let paused = std::cell::Cell::new(Duration::ZERO);
        let read = || samples.borrow_mut().next();
        let clock = || {
            let now = clock_at.get();
            clock_at.set(now + Duration::from_secs(1));
            now
        };
        let pause = |d: Duration| paused.set(paused.get() + d);
        let first = cpu_busy_with(&state, read, clock, pause).unwrap();
        assert_eq!(first.pct, 25.0);
        assert_eq!(first.window_secs, 1.0);
        assert_eq!(paused.get(), MIN_CPU_WINDOW, "the first ask primes");
        let second = cpu_busy_with(&state, read, clock, pause).unwrap();
        assert_eq!(second.pct, 80.0);
        assert_eq!(paused.get(), MIN_CPU_WINDOW, "later asks do not wait");
        // Inside the window: the previous figure, and the window stays.
        // (The second sample was read at base + 2 s.)
        clock_at.set(base + Duration::from_millis(2100));
        let quick = || {
            let now = clock_at.get();
            clock_at.set(now + Duration::from_millis(1));
            now
        };
        let again = cpu_busy_with(&state, read, quick, pause).unwrap();
        assert_eq!(again, second);
        // Nothing left to read: unknown.
        assert_eq!(cpu_busy_with(&state, read, clock, pause), None);
        let fresh = Mutex::new(None);
        assert_eq!(cpu_busy_with(&fresh, || None, clock, pause), None);
    }

    /// Whatever the OS, the answer has the same keys: a Windows-shaped
    /// reading (busy share, no load average, no PSI) lands under `cpu`,
    /// `memory`, `disk` and `host_uptime_secs`, with the gaps named.
    #[test]
    fn a_reading_without_a_load_average_still_answers_cpu_memory_disk_and_uptime() {
        let r = Readings {
            cores: Some(8),
            busy: Some(CpuBusy {
                pct: 37.25,
                window_secs: 30.04,
            }),
            memory: Some(Memory {
                total: 16 << 30,
                available: 12 << 30,
                swap_total: None,
                swap_free: None,
            }),
            disk: Some((500 << 30, 100 << 30)),
            uptime_secs: Some(86_400.9),
            unavailable: vec!["load: Windows keeps no load average".into()],
            ..Readings::default()
        };
        let v = assemble(&r, Path::new(r"C:\Users\x\AppData\Local\veles"));
        assert_eq!(v["cpu"]["cores"], 8);
        assert_eq!(v["cpu"]["busy_pct"], 37.3, "{v}");
        assert_eq!(v["cpu"]["busy_window_secs"], 30.0);
        assert!(v["cpu"].get("load1").is_none(), "no invented load: {v}");
        assert_eq!(v["memory"]["total_bytes"], 16u64 << 30);
        assert_eq!(v["memory"]["used_pct"], 25.0);
        assert!(v["memory"]["swap_total_bytes"].is_null());
        assert_eq!(v["disk"]["used_pct"], 80.0);
        assert_eq!(v["host_uptime_secs"], 86_400);
        assert!(v.get("io_pressure").is_none());
        assert_eq!(v["unavailable"].as_array().unwrap().len(), 1);
    }

    /// The real sampler on the machine running the tests. On Windows it
    /// is the only place the Win32 calls run (the gate is Linux-only):
    /// run on the Windows build host. Elsewhere the figures the OS keeps
    /// must be there too.
    #[test]
    fn this_machine_reports_cpu_memory_disk_and_uptime() {
        let home = tempfile::tempdir().unwrap();
        let v = sample(home.path());
        assert!(v["cpu"]["cores"].as_u64().is_some_and(|c| c > 0), "{v}");
        if cfg!(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            windows
        )) {
            assert!(v["memory"]["total_bytes"].as_u64().unwrap_or(0) > 0, "{v}");
            assert!(v["host_uptime_secs"].as_u64().unwrap_or(0) > 0, "{v}");
            assert!(v["disk"]["total_bytes"].as_u64().unwrap_or(0) > 0, "{v}");
        }
        if cfg!(windows) {
            let busy = v["cpu"]["busy_pct"].as_f64().expect("a busy share");
            assert!((0.0..=100.0).contains(&busy), "{v}");
            // A second ask measures since the first.
            std::thread::sleep(MIN_CPU_WINDOW);
            assert!(cpu_busy().is_some());
        } else {
            assert!(cpu_busy().is_none(), "no busy sampler off Windows");
        }
    }
}
