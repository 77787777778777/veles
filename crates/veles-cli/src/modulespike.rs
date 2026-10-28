//! R1 spike: load a shared-core module and run the go/no-go checks
//! (PLAN-plugins-installer.md R1). The harness only loads, calls and
//! observes; every behaviour a check is about runs in the module
//! (crates/veles-module-echo has one probe per check). R3's module host
//! replaces this.
//!
//! `veles module-spike --check <name> [--so <path>]`. Checks that may
//! crash the process by design (the hazards) run in a child process, and
//! the parent reports how the child ended.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use veles_runtime::veles_core;

/// The default module: the echo module next to this binary. Build both
/// in ONE cargo invocation (`cargo build -p veles-cli -p
/// veles-module-echo`): a module built on its own links a different
/// runtime, and the load is refused.
fn default_module_so() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().unwrap_or_else(|| Path::new("target/debug"));
    dir.join("deps/libveles_module_echo.so")
}

/// A loaded module. Dropping it is `dlclose`.
struct Module {
    lib: libloading::Library,
    path: PathBuf,
}

impl Module {
    /// Inspect the FILE, refuse it if it comes from another build, and
    /// only then dlopen it.
    fn load(path: &Path) -> anyhow::Result<Self> {
        let file = veles_core::module::inspect(path).map_err(|r| anyhow::anyhow!("{r}"))?;
        veles_core::module::check(&file).map_err(|r| anyhow::anyhow!("{r}"))?;
        // SAFETY: the file passed the key and symbol checks: same core,
        // same build. Its initialisers run here.
        let lib = unsafe { libloading::Library::new(path) }?;
        Ok(Self {
            lib,
            path: path.canonicalize()?,
        })
    }

    /// A probe by name. The caller names its exact Rust type; module
    /// and harness are compiled by the same build, which `load` checked.
    fn probe<T: Copy>(&self, name: &str) -> anyhow::Result<T> {
        let sym: libloading::Symbol<T> = unsafe { self.lib.get(name.as_bytes()) }?;
        Ok(*sym)
    }

    fn version(&self) -> anyhow::Result<String> {
        Ok(self.probe::<fn() -> String>("veles_spike_version")?())
    }

    fn manifest_name(&self) -> anyhow::Result<String> {
        let sym: libloading::Symbol<*const veles_core::module::ExportedModule> =
            unsafe { self.lib.get(veles_core::module::EXPORT_SYMBOL.as_bytes()) }?;
        Ok(unsafe { &**sym }.manifest().name.clone())
    }
}

/// Is `path` (canonical) mapped into this process?
fn mapped(path: &Path) -> bool {
    let want = path.to_string_lossy();
    std::fs::read_to_string("/proc/self/maps")
        .map(|m| m.lines().any(|l| l.ends_with(want.as_ref())))
        .unwrap_or(false)
}

/// A host-built instance runtime (M3): 1 worker, named threads tagged
/// with the owner, a live-thread count.
struct InstanceRuntime {
    rt: tokio::runtime::Runtime,
    threads: Arc<AtomicUsize>,
}

fn instance_runtime(name: &str) -> anyhow::Result<InstanceRuntime> {
    let threads = Arc::new(AtomicUsize::new(0));
    let (up, down) = (threads.clone(), threads.clone());
    let owner: &'static str = Box::leak(format!("module:{name}").into_boxed_str());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name(owner)
        .on_thread_start(move || {
            veles_core::lifecycle::set_thread_owner(owner);
            up.fetch_add(1, Ordering::SeqCst);
        })
        .on_thread_stop(move || {
            down.fetch_sub(1, Ordering::SeqCst);
        })
        .enable_all()
        .build()?;
    Ok(InstanceRuntime { rt, threads })
}

/// The echo module's `veles_spike_contained` future (the harness cannot
/// name the module's own alias without linking it).
type Contained =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(String, String), String>> + Send>>;

/// Records the target of every event the scoped subscriber sees.
struct Capture(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if let Ok(mut v) = self.0.lock() {
            v.push(e.metadata().target().to_string());
        }
    }
}

fn say(line: &str) {
    println!("{line}");
}

/// Check 2: the module finds a command the HOST registered, and its log
/// lines (its own `tracing` callsite, and the line it logs through core)
/// reach the subscriber the host installed.
fn check_statics(path: &Path) -> anyhow::Result<()> {
    use tracing_subscriber::layer::SubscriberExt;
    let m = Module::load(path)?;
    say(&format!(
        "statics: manifest {:?} read after the checks",
        m.manifest_name()?
    ));
    let marker = format!("r1-marker-{}", std::process::id());
    veles_core::commands::register_command(veles_core::commands::PluginCommand {
        name: marker.clone(),
        description: "R1 spike marker".into(),
        body: veles_core::commands::CommandBody::Native(Arc::new(|_| String::new())),
    })
    .map_err(|e| anyhow::anyhow!(e))?;
    let statics: fn(&str) -> bool = m.probe("veles_spike_statics")?;
    let via_core: fn(&str) = m.probe("veles_spike_log_via_core")?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sub = tracing_subscriber::registry().with(Capture(seen.clone()));
    let found = tracing::subscriber::with_default(sub, || {
        let f = statics(&marker);
        via_core(&marker);
        f
    });
    veles_core::commands::purge_owner_commands(None);
    let seen = seen.lock().map(|v| v.clone()).unwrap_or_default();
    let own = seen.iter().any(|t| t == "veles_module_echo");
    let core = seen.iter().any(|t| t == "veles::module");
    say(&format!(
        "statics: module found the host's command: {found}; host subscriber got the module's own line: {own}, its line through core: {core}"
    ));
    if !(found && own && core) {
        anyhow::bail!("check 2 FAILED");
    }
    Ok(())
}

/// Checks 3 and 4: module code on the host-built runtime does loopback
/// TCP and a raw `tokio::spawn`; `shutdown_timeout` drops the stray
/// task; after `dlclose` the module is gone from /proc/self/maps; core
/// still works.
fn check_contained(path: &Path) -> anyhow::Result<()> {
    let m = Module::load(path)?;
    let inst = instance_runtime("echo")?;
    let probe: fn(Arc<AtomicBool>) -> Contained = m.probe("veles_spike_contained")?;
    let dropped = Arc::new(AtomicBool::new(false));
    let task = inst.rt.spawn(probe(dropped.clone()));
    let (here, stray) = inst
        .rt
        .block_on(task)?
        .map_err(|e| anyhow::anyhow!("module probe: {e}"))?;
    let live = inst.threads.load(Ordering::SeqCst);
    say(&format!(
        "contained: TCP round trip ran on {here:?}, the raw spawn started on {stray:?}; {live} instance thread(s) live"
    ));
    let on_instance = here.starts_with("module:echo") && stray.starts_with("module:echo");
    let threads = inst.threads.clone();
    let t0 = std::time::Instant::now();
    inst.rt.shutdown_timeout(std::time::Duration::from_secs(5));
    let after = threads.load(Ordering::SeqCst);
    let stray_dropped = dropped.load(Ordering::SeqCst);
    say(&format!(
        "contained: shutdown_timeout took {} ms; stray task dropped: {stray_dropped}; instance threads left: {after}",
        t0.elapsed().as_millis()
    ));
    let so = m.path.clone();
    drop(m);
    let still = mapped(&so);
    say(&format!(
        "unload: {} mapped after dlclose: {still}",
        so.display()
    ));
    let core_ok = core_still_works();
    if let Err(e) = &core_ok {
        say(&format!("unload: core after unload: {e}"));
    }
    if !on_instance || !stray_dropped || after != 0 || still || core_ok.is_err() {
        anyhow::bail!("checks 3/4 FAILED");
    }
    Ok(())
}

/// After an unload: a log line, a core registry, a core task on a fresh
/// runtime, a TLS client and an HTTP request through core's stack.
fn core_still_works() -> anyhow::Result<()> {
    tracing::info!(target: "veles::spike", "post-unload log line");
    let name = format!("post-unload-{}", std::process::id());
    veles_core::commands::register_command(veles_core::commands::PluginCommand {
        name: name.clone(),
        description: String::new(),
        body: veles_core::commands::CommandBody::Native(Arc::new(|_| String::new())),
    })
    .map_err(|e| anyhow::anyhow!(e))?;
    anyhow::ensure!(veles_core::commands::plugin_command(&name).is_some());
    let rt = tokio::runtime::Runtime::new()?;
    let status = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        veles_core::rt::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            if let Ok((mut s, _)) = listener.accept().await {
                // Read the request head before answering: closing on
                // unread bytes resets the connection.
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = s
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        // reqwest is core's HTTP client; building it initialises rustls.
        let client = veles_runtime::reqwest::Client::builder()
            .use_rustls_tls()
            .build()?;
        let r = client.get(format!("http://{addr}/")).send().await?;
        anyhow::Ok(r.status().as_u16())
    })?;
    say(&format!(
        "unload: after dlclose: log line, core registry, core spawn, rustls client, HTTP {status}"
    ));
    anyhow::ensure!(status == 204, "HTTP status {status}");
    Ok(())
}

/// Check 6: a second copy at another path loads while the first is held;
/// both are mapped and the new one answers; each unmaps on its own
/// dlclose.
fn check_reload(path: &Path) -> anyhow::Result<()> {
    let dir = scratch_dir("reload")?;
    let (a, b) = (dir.join("a.so"), dir.join("b.so"));
    std::fs::copy(path, &a)?;
    std::fs::copy(path, &b)?;
    let old = Module::load(&a)?;
    let new = Module::load(&b)?;
    let both = mapped(&old.path) && mapped(&new.path);
    let v = new.version()?;
    let (pa, pb) = (old.path.clone(), new.path.clone());
    drop(old);
    let a_gone = !mapped(&pa) && mapped(&pb);
    drop(new);
    let b_gone = !mapped(&pb);
    say(&format!(
        "reload: both mapped while the old copy is held: {both}; the new copy answers {v:?}; old unmapped on its dlclose while the new stays: {a_gone}; new unmapped on its own: {b_gone}"
    ));
    let _ = std::fs::remove_dir_all(&dir);
    if !(both && a_gone && b_gone) {
        anyhow::bail!("check 6 FAILED");
    }
    Ok(())
}

/// Check 8: a panic in module code on a module thread marks the instance
/// faulted and the process lives, with the fatal hook installed.
fn check_panic(path: &Path) -> anyhow::Result<()> {
    veles_core::lifecycle::install_fatal_panic_hook();
    let m = Module::load(path)?;
    let panic_probe: fn() = m.probe("veles_spike_panic")?;
    let inst = instance_runtime("echo-panic")?;
    let joined = inst
        .rt
        .block_on(inst.rt.spawn(async move { panic_probe() }));
    let faulted = veles_core::module::faulted_instances();
    let entry = faulted.get("module:echo-panic").cloned();
    inst.rt.shutdown_timeout(std::time::Duration::from_secs(5));
    drop(m);
    say(&format!(
        "panic: the task ended in a JoinError: {}; faulted entry: {entry:?}; this process is still running",
        joined.is_err()
    ));
    let in_module = entry
        .as_ref()
        .is_some_and(|(loc, _)| loc.contains("veles-module-echo"));
    if joined.is_ok() || !in_module {
        anyhow::bail!("check 8 FAILED");
    }
    Ok(())
}

/// The hazards (checks 4b and 5) in a child each; returns how it ended.
fn child(check: &str, so: &Path) -> anyhow::Result<String> {
    use std::os::unix::process::ExitStatusExt;
    let out = std::process::Command::new(std::env::current_exe()?)
        .args(["module-spike", "--check", check, "--so"])
        .arg(so)
        .stdin(std::process::Stdio::null())
        .output()?;
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        if !l.contains(" INFO ") {
            say(&format!("  [{check}] {l}"));
        }
    }
    Ok(match out.status.signal() {
        Some(sig) => format!("killed by signal {sig}"),
        None => format!("exit {}", out.status.code().unwrap_or(-1)),
    })
}

/// Child: the module logs through ITS OWN callsite, is unloaded, then
/// the process makes a new Dispatch (what a scoped subscriber or a
/// filter reload does).
fn child_dispatch(path: &Path, own_callsite: bool) -> anyhow::Result<()> {
    let m = Module::load(path)?;
    // Under a scoped subscriber that wants everything, so the callsite
    // registers (behind a WARN filter an `info!` never does).
    tracing::subscriber::with_default(tracing_subscriber::registry(), || {
        if own_callsite {
            let statics: fn(&str) -> bool = m.probe("veles_spike_statics")?;
            statics("dispatch-child");
        } else {
            let via_core: fn(&str) = m.probe("veles_spike_log_via_core")?;
            via_core("dispatch-child");
        }
        anyhow::Ok(())
    })?;
    let so = m.path.clone();
    drop(m);
    say(&format!("unloaded, mapped: {}", mapped(&so)));
    let d = tracing::Dispatch::new(tracing_subscriber::registry());
    tracing::dispatcher::with_default(&d, || tracing::info!("after"));
    say("a new Dispatch after the unload: fine");
    Ok(())
}

/// Child: a module `thread_local!` with Drop is set on an instance
/// thread (joined before dlclose) or on a host thread that outlives the
/// dlclose and then exits.
fn child_tls(path: &Path, on_host_thread: bool) -> anyhow::Result<()> {
    let m = Module::load(path)?;
    let touch: fn() = m.probe("veles_spike_touch_tls")?;
    let so = m.path.clone();
    if on_host_thread {
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let t = std::thread::spawn(move || {
            touch();
            let _ = done_tx.send(());
            let _ = go_rx.recv();
        });
        let _ = done_rx.recv();
        drop(m);
        say(&format!(
            "dlclose with a host thread holding module TLS; mapped: {}",
            mapped(&so)
        ));
        let _ = go_tx.send(());
        let _ = t.join();
        say("the host thread exited");
    } else {
        let inst = instance_runtime("echo-tls")?;
        inst.rt.block_on(inst.rt.spawn(async move { touch() }))?;
        inst.rt.shutdown_timeout(std::time::Duration::from_secs(5));
        drop(m);
        say(&format!(
            "instance thread joined, then dlclose; mapped: {}",
            mapped(&so)
        ));
    }
    Ok(())
}

/// Check 7, interactive: load the module, then on every stdin line copy
/// the CURRENT file at `path` to a new content-addressed path, check it,
/// load it, and drop the old one. Same process throughout; a script
/// edits and rebuilds between lines (scripts/r1-live-rebuild.sh).
fn live(path: &Path) -> anyhow::Result<()> {
    let dir = scratch_dir("live")?;
    let mut current = Module::load(&install(path, &dir)?)?;
    say(&format!(
        "live: pid {} serving {:?}",
        std::process::id(),
        current.version()?
    ));
    let mut line = String::new();
    while std::io::stdin().read_line(&mut line)? > 0 {
        line.clear();
        let next = install(path, &dir)?;
        if next == current.path {
            say("live: unchanged file, nothing to reload");
            continue;
        }
        match Module::load(&next) {
            Ok(m) => {
                let old = std::mem::replace(&mut current, m);
                let old_path = old.path.clone();
                drop(old);
                say(&format!(
                    "live: pid {} now serving {:?}; old copy mapped: {}",
                    std::process::id(),
                    current.version()?,
                    mapped(&old_path)
                ));
            }
            Err(e) => say(&format!(
                "live: pid {} REFUSED the new build: {e}; still serving {:?}",
                std::process::id(),
                current.version()?
            )),
        }
    }
    drop(current);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Copy `src` to `<dir>/<hash of its bytes>.so` (M2's content-addressed
/// install: dlopen of a path still mapped returns the old mapping).
fn install(src: &Path, dir: &Path) -> anyhow::Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(src)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    let dst = dir.join(format!("{:016x}.so", h.finish()));
    if !dst.exists() {
        let tmp = dst.with_extension("tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &dst)?;
    }
    Ok(dst.canonicalize()?)
}

fn scratch_dir(what: &str) -> anyhow::Result<PathBuf> {
    let d = std::env::temp_dir().join(format!("veles-r1-{what}-{}", std::process::id()));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

pub fn run(check: &str, so: Option<PathBuf>) -> anyhow::Result<()> {
    let path = so.unwrap_or_else(default_module_so);
    anyhow::ensure!(
        path.exists(),
        "module .so not found at {}: `cargo build -p veles-cli -p veles-module-echo`",
        path.display()
    );
    match check {
        "statics" => check_statics(&path),
        "contained" | "unload" => check_contained(&path),
        "reload" => check_reload(&path),
        "panic" => check_panic(&path),
        "live" => live(&path),
        "child-dispatch-own" => child_dispatch(&path, true),
        "child-dispatch-core" => child_dispatch(&path, false),
        "child-tls-instance" => child_tls(&path, false),
        "child-tls-host" => child_tls(&path, true),
        "hazards" => {
            for c in [
                "child-dispatch-core",
                "child-dispatch-own",
                "child-tls-instance",
                "child-tls-host",
            ] {
                let end = child(c, &path)?;
                say(&format!("{c}: {end}"));
            }
            Ok(())
        }
        "all" => {
            say(&format!("module under test: {}", path.display()));
            check_statics(&path)?;
            check_contained(&path)?;
            check_reload(&path)?;
            check_panic(&path)?;
            run("hazards", Some(path))
        }
        other => anyhow::bail!(
            "unknown check {other:?}: statics|contained|reload|panic|live|hazards|all"
        ),
    }
}
