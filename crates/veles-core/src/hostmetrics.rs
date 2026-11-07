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
//! stored or gossiped, and the heartbeat is untouched.
//!
//! Every source is read from KERNEL state, no subprocess: /proc on Linux
//! and Android, `getloadavg(3)` and `statvfs(3)` where they exist. A
//! figure that cannot be read is absent and NAMED in `unavailable` — an
//! unknown is never reported as zero (the same rule `node::LoadOne`
//! keeps for placement).

use serde_json::{json, Map, Value};
use std::path::Path;

/// Sample this machine. `disk_path` is the filesystem to report (the
/// data home: where the store, the logs and the inboxes grow).
pub fn sample(disk_path: &Path) -> Value {
    sample_with(Path::new("/proc"), disk_path)
}

/// [`sample`], reading /proc from `proc_root` (tests point it at a
/// fixture tree).
pub fn sample_with(proc_root: &Path, disk_path: &Path) -> Value {
    let read = |name: &str| std::fs::read_to_string(proc_root.join(name)).ok();
    let mut out = Map::new();
    let mut unavailable: Vec<String> = Vec::new();

    let cores = std::thread::available_parallelism().ok().map(|n| n.get());
    let load = read("loadavg")
        .as_deref()
        .and_then(parse_loadavg)
        .or_else(|| (proc_root == Path::new("/proc")).then(getloadavg).flatten());
    let mut cpu = Map::new();
    cpu.insert("cores".into(), json!(cores));
    match load {
        Some((one, five, fifteen)) => {
            cpu.insert("load1".into(), json!(one));
            cpu.insert("load5".into(), json!(five));
            cpu.insert("load15".into(), json!(fifteen));
            if let Some(c) = cores.filter(|&c| c > 0) {
                cpu.insert(
                    "load1_pct".into(),
                    json!(((one / c as f64) * 100.0).round() as i64),
                );
            }
        }
        None => unavailable.push("load: no /proc/loadavg and no getloadavg(3)".into()),
    }
    out.insert("cpu".into(), Value::Object(cpu));

    match read("meminfo").as_deref().and_then(parse_meminfo) {
        Some(m) => {
            let used_pct = (m.total_kib > 0).then(|| {
                let used = m.total_kib.saturating_sub(m.available_kib) as f64;
                (used / m.total_kib as f64 * 1000.0).round() / 10.0
            });
            out.insert(
                "memory".into(),
                json!({
                    "total_bytes": m.total_kib * 1024,
                    "available_bytes": m.available_kib * 1024,
                    "used_pct": used_pct,
                    "swap_total_bytes": m.swap_total_kib.map(|v| v * 1024),
                    "swap_free_bytes": m.swap_free_kib.map(|v| v * 1024),
                }),
            );
        }
        None => unavailable.push("memory: no /proc/meminfo".into()),
    }

    match disk(disk_path) {
        Some((total, free)) => {
            let used_pct = (total > 0).then(|| {
                ((total.saturating_sub(free)) as f64 / total as f64 * 1000.0).round() / 10.0
            });
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
        None => unavailable.push(format!(
            "disk: statvfs({}) failed or is not available here",
            disk_path.display()
        )),
    }

    match read("pressure/io").as_deref().and_then(parse_pressure) {
        Some(p) => {
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
        None => unavailable
            .push("io_pressure: no /proc/pressure/io (a kernel without PSI, or not Linux)".into()),
    }

    match read("uptime").as_deref().and_then(parse_uptime) {
        Some(secs) => {
            out.insert("host_uptime_secs".into(), json!(secs as u64));
        }
        None => unavailable.push("host uptime: no /proc/uptime".into()),
    }
    out.insert("unavailable".into(), json!(unavailable));
    Value::Object(out)
}

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
// /proc/loadavg is the only source there.
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

#[cfg(not(unix))]
pub fn disk(_path: &Path) -> Option<(u64, u64)> {
    None
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
}
