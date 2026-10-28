//! Sandboxed script execution for unprivileged callers — pluggable
//! backends.
//!
//! Reference: slopdrop's `SafeTclInterp` (`src/tcl_wrapper.rs`) — a
//! Tcl-LEVEL sandbox, not an OS jail: banned command families, a
//! bracket-balance pre-check, recursion caps, and a privilege split
//! (sandboxed `tcl` for everyone, full power only for the operator).
//!
//! Since 2026-09-03 the execution half is a BACKEND trait (user
//! decision: raw linux / podman / nspawn / firecracker / freebsd jails
//! — not all in one round). Shipped now:
//!
//! - **native** (default; `host` is the spelled-out alias): the
//!   original design — a real tclsh subprocess ON THE HOST running the
//!   SAFE INTERP driver, fresh scratch dir per run (HOME/TMPDIR/cwd),
//!   RLIMIT_AS best-effort on unix, timeout kill switch. Defense layer
//!   one is the safe interp; the OS process limits are a hedge. This is
//!   the option that stays available for jobs that only make sense with
//!   real host access. NOTE the split — sandboxed surfaces (any
//!   backend) stay Tcl-only for untrusted callers; a real HOST SHELL is
//!   the `terminal` tool (bash/sh -c, ACL-gated to trusted callers),
//!   and fully-unfenced Tcl is the trusted `execute_code` path without
//!   the sandbox flag. Both live outside this fence on purpose.
//! - **podman**: the SAME safe-interp driver, but the tclsh process
//!   runs inside a rootless container. Defense in depth: the jail
//!   bounds the blast radius, the safe interp bounds capability.
//!   Policy knobs (config): network default ON (dev-sandbox posture:
//!   models install packages) — `network = false` is the benchmark /
//!   untrusted switch (`--network none`); capabilities default DROPPED
//!   (`--cap-drop ALL`, hardened pure-compute fence) —
//!   `capabilities = "default"` is the package-install posture inside
//!   the image; `[[sandbox.mounts]]` is default-DENY explicit host
//!   access (`ro`/`rw` binds, validated loudly).
//!
//! The other three fences (shipped the same day, construction-gated):
//! **nspawn** (systemd-nspawn namespace jail — root-gated, so it runs
//! where veles runs as root, e.g. inside a VM), **firecracker**
//! (microVM, the strongest fence — needs kernel + rootfs artifacts, no
//! networking/mounts this round), **jail** (FreeBSD `jail -c` — needs
//! a BSD host). Their command/config construction is pinned by tests;
//! their RUNTIME is verified by the opt-in scripts/sandbox_vm_check.sh
//! inside throwaway VMs, never by the daemon-less cargo gate.
//!
//! A new backend implements `SandboxBackend::run` and gets a name in
//! `backend_from`; an unknown `[sandbox] backend` fails LOUDLY at run
//! time, the api_mode precedent — a config line that does nothing is a
//! lie. Same for every knob it cannot honor: firecracker + network
//! fails instead of silently dropping the expectation.
//!
//! Semantics fixed across all backends (the slopdrop decisions):
//!
//! - Sandboxed runs are PURE-COMPUTE at the Tcl level: no tool-bridge
//!   procs (a bridge routing through the caller's ACL tier lets a
//!   script try every command the caller could; absent procs cannot be
//!   called at all), no secrets injected, stdout is the only channel
//!   back.
//! - Workspace isolation: fresh scratch dir per run for HOME/TMPDIR/
//!   cwd, deleted after the run. Host access beyond that is ONLY the
//!   explicit `[[sandbox.mounts]]` list (container backends); native is
//!   the host already.
//! - The script path arrives by env/mount, never the container's argv,
//!   and the driver does a bracket-balance pre-check before evaluating.
//! - Timeout/cap values: per-surface constants + `[sandbox]` config,
//!   never the caller's discretion beyond clamping.

/// The safe-interp driver tclsh executes. The script path arrives via
/// env, never the command line, so argv stays out of the script's
/// hands. The safe base interpreter already has no exec/file/socket/
/// open/load/source/package/glob/cd/encoding — the extra `hide` calls
/// close the loop holes (event-loop spinning, vwait-style blocking)
/// and `recursionlimit` caps runaway recursion instead of waiting for
/// the process to exhaust its C stack.
///
/// Identical bytes run on every backend: the podman path copies this
/// into the container and executes it there, so capability posture is
/// one code path, tested once.
pub const SANDBOX_DRIVER: &str = r#"# veles sandbox driver — safe-interp wrapper (slopdrop parity)
global env
if {![info exists env(VELES_SANDBOX_SCRIPT)]} {
    puts stderr "sandbox: no script to run"
    exit 2
}
if {![catch {set _sf [open $env(VELES_SANDBOX_SCRIPT) r]} _serr]} {
    set _code [read $_sf]
    close $_sf
} else {
    puts stderr "sandbox: cannot read script: $_serr"
    exit 2
}
# Bracket/quote balance BEFORE anything evaluates — a mismatched script
# must be refused, not half-parsed (slopdrop's pre-check).
if {![info complete $_code]} {
    puts stderr "sandbox: unbalanced brackets or quotes — refused"
    exit 2
}
set _i [interp create -safe]
$_i recursionlimit 128
# exit must not kill the driver; event-loop commands would let a
# script idle forever inside the timeout window.
foreach _hole {exit after update vwait pause} {
    catch {$_i hide $_hole}
}
# The safe base hides the stdout CHANNEL, so plain puts fails with
# "can not find channel named stdout". Alias a puts that writes to OUR
# (the driver's) stdio — stderr stays stderr so errors are labelled.
proc _sb_puts {args} {
    set n [llength $args]
    switch -- $n {
        0 { puts "" ; return }
        1 { puts [lindex $args 0] ; return }
        2 {
            if {[lindex $args 0] eq "-nonewline"} {
                puts -nonewline [lindex $args 1]
            } else {
                if {[lindex $args 0] eq "stderr"} {
                    puts stderr [lindex $args 1]
                } else {
                    puts [lindex $args 1]
                }
            }
            return
        }
        3 {
            set text [lindex $args end]
            set np [lindex $args 0]
            set no_nl [expr {$np eq "-nonewline"}]
            set chan "stdout"
            if {$no_nl && [llength $args] == 4} {
                set chan [lindex $args 1]
            } elseif {!$no_nl} {
                set chan [lindex $args 1]
                set no_nl [expr {[lindex $args 0] eq "-nonewline"}]
            }
            if {$chan eq "stderr"} {
                if {$no_nl} { puts -nonewline stderr $text } else { puts stderr $text }
            } else {
                if {$no_nl} { puts -nonewline $text } else { puts $text }
            }
            return
        }
        default { error "puts: wrong # args" }
    }
}
interp alias $_i puts {} _sb_puts
set _rc [catch {$_i eval $_code} _res]
interp delete $_i
if {$_rc} {
    puts stderr "sandbox error: $_res"
    exit 1
}
if {$_res ne ""} {
    puts $_res
}
exit 0
"#;

/// Defaults for a sandboxed run. Constants on purpose (see the module
/// docs): the trusted path keeps its own numbers; these describe what
/// an untrusted caller's compute job may consume.
pub const SANDBOX_DEFAULT_TIMEOUT_SECS: u64 = 120;
/// RLIMIT_AS for the NATIVE backend's process: pure-compute Tcl rarely
/// passes a few tens of MB; 256MB is generous and still bounds runaway
/// `string repeat`. Container backends enforce their own cap via
/// `[sandbox] memory_mb` (default matches this). RLIMIT_AS is
/// best-effort: restricted environments may refuse it, and macOS never
/// kernel-enforces it — the timeout stays the kill switch everywhere.
pub const SANDBOX_MEM_LIMIT_BYTES: u64 = 256 * 1024 * 1024;

/// One sandboxed run, as requested by a caller. Policy (backend,
/// memory, network) lives in `[sandbox]` config, NOT here — a caller
/// gets script + timeout and nothing else to abuse.
#[derive(Debug, Clone)]
pub struct SandboxRequest {
    pub script_path: std::path::PathBuf,
    pub timeout_secs: u64,
}

/// What a backend produced. `timed_out` is separate from exit code: a
/// killed run has no meaningful status, and callers format the two
/// cases differently (the tool says "timeout after Ns", cron alerts).
#[derive(Debug, Clone, Default)]
pub struct SandboxOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

impl SandboxOutput {
    /// Clean completion: finished in time, zero exit.
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == 0
    }
}

/// A way to run an untrusted script with bounded blast radius.
///
/// Contract: `run` OWNS the timeout (returns `timed_out` rather than
/// hanging) and all workspace cleanup; the caller keeps the script
/// file's lifecycle (wrote it, removes it afterwards). Stdout is the
/// only result channel by policy; stderr carries diagnostics and is
/// merged in by the FORMATTERS, not by backends.
#[async_trait::async_trait]
pub trait SandboxBackend: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, req: &SandboxRequest) -> anyhow::Result<SandboxOutput>;
}

// ── native backend ──────────────────────────────────────────────────

/// The original backend: tclsh subprocess + safe-interp driver +
/// scratch workspace + RLIMIT_AS. Zero added isolation beyond the
/// process itself — fine for pure-compute Tcl on a single-user host,
/// the reason container backends exist for multi-tenant surfaces.
pub struct NativeBackend {
    tclsh: String,
}

impl NativeBackend {
    pub fn new(tclsh: String) -> Self {
        Self { tclsh }
    }
}

#[async_trait::async_trait]
impl SandboxBackend for NativeBackend {
    fn name(&self) -> &str {
        "native"
    }
    async fn run(&self, req: &SandboxRequest) -> anyhow::Result<SandboxOutput> {
        let scratch = new_scratch()?;
        let result = async {
            let cmd = build_sandbox(&self.tclsh, &req.script_path, &scratch)?;
            // kill_on_drop IS the timeout: tokio::time::timeout below
            // cancels by DROPPING this future, and a dropped Child only
            // kills its process when this flag is set. Without it a
            // timed-out run is merely ABANDONED — the interpreter keeps
            // spinning (this exact leak shipped in d17a8ed and burned a
            // whole core per leaked run until the orphans were killed).
            let mut command = tokio::process::Command::from(cmd);
            command.kill_on_drop(true);
            let child = command
                .spawn()
                .map_err(|e| anyhow::anyhow!("tclsh launch failed: {e}"))?;
            // The memory ceiling where the platform sets it on a running
            // process (Windows); already set before exec on unix.
            if let Some(pid) = child.id() {
                limit_child(pid);
            }
            let out = child
                .wait_with_output()
                .await
                .map_err(|e| anyhow::anyhow!("tclsh did not finish: {e}"))?;
            Ok::<std::process::Output, anyhow::Error>(out)
        };
        let res =
            tokio::time::timeout(std::time::Duration::from_secs(req.timeout_secs), result).await;
        let _ = std::fs::remove_dir_all(&scratch);
        match res {
            Ok(Ok(out)) => Ok(SandboxOutput {
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
                exit_code: out.status.code().unwrap_or(-1),
                timed_out: false,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(SandboxOutput {
                timed_out: true,
                exit_code: -1,
                ..Default::default()
            }),
        }
    }
}

/// Write the driver into the scratch dir and return the Command that
/// runs it. (Indirection kept so the driver bytes live in exactly one
/// place.) Native-backend private: the podman backend copies the same
/// bytes into its own mount instead.
fn build_sandbox(
    tclsh: &str,
    script_path: &std::path::Path,
    scratch: &std::path::Path,
) -> std::io::Result<std::process::Command> {
    let driver = scratch.join("driver.tcl");
    std::fs::write(&driver, SANDBOX_DRIVER)?;
    let mut cmd = mem_limited_command(tclsh, &driver);
    cmd.current_dir(scratch)
        .env("VELES_SANDBOX_SCRIPT", script_path)
        .env("HOME", scratch)
        .env("TMPDIR", scratch)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    Ok(cmd)
}

/// `tclsh driver.tcl` with RLIMIT_AS applied — and applied WITHOUT a
/// `pre_exec` closure, which is the whole point of this function.
///
/// `pre_exec` is the obvious way to set a limit on a child and it is
/// also a permanent hang. It disables std's `posix_spawn` fast path, so
/// every launch goes through `fork(2)` — and libtcl registers a
/// `pthread_atfork` CHILD handler (`Tcl_InitNotifier` →
/// `Tcl_GetThreadData` → `Tcl_Alloc`) that takes the Tcl allocator
/// mutex. Fork while any other thread holds that mutex and the child,
/// now single-threaded, waits on a lock nobody will ever release. The
/// parent then waits forever too, in the `recv()` std performs on the
/// exec-status socket inside `Command::spawn` — and because that recv
/// is a blocking syscall in the middle of `poll`, the
/// `tokio::time::timeout` wrapped around the run cannot cancel it. The
/// runtime thread is simply gone.
///
/// Found 2026-09-04 the way these things are found: a `cargo test` run
/// sat wedged for twelve hours, and `gdb` on the orphan showed the
/// atfork stack above. veles links libtcl and runs Tcl on its own
/// thread, so the collision is not exotic here — it is the normal
/// shape of the process.
///
/// So: no closure in the child. A shell sets the same limit and then
/// REPLACES itself with tclsh, which keeps one pid (`kill_on_drop`
/// still reaches the interpreter, which the timeout contract needs) and
/// lets std use `posix_spawn`, which never runs atfork handlers.
#[cfg(unix)]
fn mem_limited_command(tclsh: &str, driver: &std::path::Path) -> std::process::Command {
    // `ulimit` counts KILOBYTES; POSIX makes a bare `ulimit -v` set the
    // hard limit as well as the soft one, which is what the setrlimit
    // call it replaces did.
    let kb = SANDBOX_MEM_LIMIT_BYTES / 1024;
    // The shell every other spawn runs (`crate::shell`), not a literal
    // `/bin/sh`: `ulimit -v`/`-d` are in bash, ash and mksh alike, and on
    // the phone this is the APK's bash (PLAN-phone-userland.md T1).
    let mut cmd = std::process::Command::new(crate::shell::resolve());
    // Best-effort, exactly as setrlimit was: environments that refuse
    // the request still run the script, under the timeout that is the
    // real kill switch. `;` not `&&` for that reason. `"$0" "$@"`
    // passes the real argv through, so nothing here has to quote a
    // path.
    cmd.arg("-c")
        .arg(format!(
            "ulimit {} {kb} 2>/dev/null; exec \"$0\" \"$@\"",
            MEM_LIMIT_FLAG
        ))
        .arg(tclsh)
        .arg(driver);
    cmd
}

/// Which rlimit the wrapper sets. Address space (`-v`, RLIMIT_AS)
/// everywhere but Android: there bionic's allocator and the runtime
/// reserve virtual ranges far beyond what they touch, and a process
/// under ANY address-space limit — the fence's 256 MB, or 1 GB — dies
/// with SIGABRT before its first line (measured on the phone,
/// 2026-10-14, after `doctor --sandbox` reported exit -1 with an empty
/// stdout). The data segment (`-d`, RLIMIT_DATA, which Linux >= 4.7
/// applies to private anonymous mappings too) caps the heap the same
/// way, and the interpreter runs.
#[cfg(all(unix, not(target_os = "android")))]
const MEM_LIMIT_FLAG: &str = "-v";
#[cfg(target_os = "android")]
const MEM_LIMIT_FLAG: &str = "-d";

/// Off unix the limit goes on AFTER the spawn, through the platform's
/// job object (`platform::limit_memory`, see `limit_child`); here the
/// command is plain.
#[cfg(not(unix))]
fn mem_limited_command(tclsh: &str, driver: &std::path::Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(tclsh);
    cmd.arg(driver);
    cmd
}

/// The memory ceiling on a child the native fence just spawned, where
/// the platform sets it on a running process (a Windows job object).
/// On unix `mem_limited_command` already set it before exec and this
/// is a no-op.
fn limit_child(pid: u32) {
    if cfg!(unix) {
        return;
    }
    if !crate::platform::limit_memory(pid, SANDBOX_MEM_LIMIT_BYTES) {
        tracing::warn!("sandbox: no memory limit on tclsh {pid}; the timeout is the only ceiling");
    }
}

/// A fresh scratch workspace for one run. The backend deletes it
/// afterwards; a unique slice per invocation means no two runs share a
/// HOME even under concurrency.
fn new_scratch() -> std::io::Result<std::path::PathBuf> {
    let base = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = base.join(format!("veles_sandbox_{}_{}", std::process::id(), stamp));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

// ── podman backend ──────────────────────────────────────────────────

/// Rootless-container backend. The safe-interp driver still runs —
/// the container is the SECOND fence, not a replacement: caps are
/// dropped, network is off by default, memory is cgroup-enforced.
///
/// Image contract: tclsh on PATH inside the image, nothing else
/// required. `flake.nix` grows a `sandboxImage` output for exactly
/// that (PENDING); until then operators point `[sandbox] image` at any
/// tclsh image they trust, e.g. one built with
/// `scripts/build-image.sh`-style machinery.
pub struct PodmanBackend {
    pub binary: String,
    pub image: String,
    pub memory_mb: u64,
    pub network: bool,
    /// `true` = the hardened posture: `--cap-drop ALL`. `false` = the
    /// runtime's rootless default set — package installation INSIDE the
    /// image (dpkg/pip) needs CAP_CHOWN & co within the userns. Host
    /// safety never comes from this flag; it comes from rootless
    /// isolation (userns, no-new-privileges stays on either way).
    pub drop_caps: bool,
    /// Explicit host binds, default deny. Validated by `backend_from`
    /// (mode ro|rw, absolute host path) before they reach here.
    pub mounts: Vec<crate::config::SandboxMount>,
    /// Operator escape hatch appended before the image — power users
    /// get `--cpus`, `--pids-limit`, volume overlays without a code
    /// change. Trust boundary: whoever edits config.toml already owns
    /// the machine. Also the extension point for restrictions this
    /// backend has no knob for yet.
    pub extra_args: Vec<String>,
    /// tclsh path INSIDE the image.
    pub tclsh: String,
    /// The podman service is on the other side of a socket
    /// ([`crate::user_envs::podman_remote`]): a path on this side means
    /// nothing to it, so the scratch dir travels by `podman cp` instead
    /// of a bind mount.
    pub remote: bool,
}

impl PodmanBackend {
    /// The flags that ARE the fence, the same in both modes.
    fn fence_args(&self, cmd: &mut std::process::Command) {
        cmd.arg("--pull=never");
        // `network = true` is the isolated posture, as it is for user
        // envs: no flag, so a rootless podman gives the container a
        // namespace of its own (pasta) — outbound, but not the host's
        // loopback. It used to be `--network host`, and driven through
        // the VPS's sandbox socket that is the VPS's own network.
        if !self.network {
            cmd.arg("--network").arg("none");
        }
        cmd.arg("--memory")
            .arg(format!("{}m", self.memory_mb))
            .arg("--memory-swap")
            .arg(format!("{}m", self.memory_mb));
        if self.drop_caps {
            cmd.arg("--cap-drop").arg("ALL");
        }
        cmd.arg("--security-opt").arg("no-new-privileges");
    }

    /// Everything after the fence: the workspace, the explicit binds,
    /// the operator's extras, and the driver as the entrypoint.
    fn tail_args(&self, cmd: &mut std::process::Command) {
        cmd.arg("-w")
            .arg("/work")
            .arg("-e")
            .arg("HOME=/work")
            .arg("-e")
            .arg("TMPDIR=/work")
            .arg("-e")
            .arg("VELES_SANDBOX_SCRIPT=/work/user.tcl");
        // Explicit host binds (validated ro|rw by backend_from). The
        // scratch /work is the ONLY implicit one. Remote, these name
        // paths on the service's host, which is what an operator who
        // lists them there means.
        for m in &self.mounts {
            cmd.arg("-v")
                .arg(format!("{}:{}:{}", m.host, m.container, m.mode));
        }
        for a in &self.extra_args {
            cmd.arg(a);
        }
        cmd.arg(&self.image)
            .arg(&self.tclsh)
            .arg("/work/driver.tcl")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    }

    /// The `podman run` argv, separated out so tests can pin the
    /// security flags without a daemon. Scratch dir is mounted at
    /// /work (driver + script + cwd + HOME + TMPDIR all live there);
    /// the cidfile must NOT live inside that mount — a script that
    /// escaped the safe interp could rewrite it and dodge the timeout
    /// cleanup.
    fn build_command(
        &self,
        scratch: &std::path::Path,
        cidfile: &std::path::Path,
    ) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.binary);
        cmd.arg("run").arg("--rm");
        self.fence_args(&mut cmd);
        cmd.arg("--cidfile")
            .arg(cidfile)
            .arg("-v")
            .arg(format!("{}:/work", scratch.display()));
        self.tail_args(&mut cmd);
        cmd
    }

    /// The remote mode's `podman create` argv: the same fence and no
    /// host path at all. Its stdout is the container id; the scratch dir
    /// then goes in with `podman cp` and the run is `start --attach`.
    fn build_create_command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.binary);
        cmd.arg("create").arg("--rm");
        self.fence_args(&mut cmd);
        self.tail_args(&mut cmd);
        cmd
    }

    /// One run through a remote podman: create, copy the scratch dir in,
    /// start attached. The timeout covers all three; a container left
    /// behind by a timeout or a failed copy is removed by id.
    async fn run_remote(
        &self,
        scratch: &std::path::Path,
        timeout: std::time::Duration,
    ) -> anyhow::Result<SandboxOutput> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut create = tokio::process::Command::from(self.build_create_command());
        create.kill_on_drop(true);
        let made = tokio::time::timeout_at(deadline, create.output())
            .await
            .map_err(|_| anyhow::anyhow!("podman create timed out"))?
            .map_err(|e| {
                anyhow::anyhow!(
                    "podman launch failed (is podman installed? CONTAINER_HOST reachable?): {e}"
                )
            })?;
        if !made.status.success() {
            anyhow::bail!(
                "podman create failed (image \"{}\" present on the service?): {}",
                self.image,
                String::from_utf8_lossy(&made.stderr).trim()
            );
        }
        let id = String::from_utf8_lossy(&made.stdout).trim().to_string();
        let remove = || async {
            let _ = tokio::process::Command::new(&self.binary)
                .arg("rm")
                .arg("-f")
                .arg(&id)
                .output()
                .await;
        };
        // A DIRECTORY copied to a path that does not exist yet becomes
        // it. A file copied to /work/… would fail here: a container that
        // was created and never started has no /work yet (measured on
        // the VPS, 2026-10-02).
        let mut cp = tokio::process::Command::new(&self.binary);
        cp.arg("cp")
            .arg(scratch)
            .arg(format!("{id}:/work"))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let copied = tokio::time::timeout_at(deadline, cp.output()).await;
        match copied {
            Ok(Ok(o)) if o.status.success() => {}
            Ok(Ok(o)) => {
                remove().await;
                anyhow::bail!(
                    "podman cp into the sandbox failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Ok(Err(e)) => {
                remove().await;
                anyhow::bail!("podman cp into the sandbox failed: {e}");
            }
            Err(_) => {
                remove().await;
                return Ok(SandboxOutput {
                    timed_out: true,
                    exit_code: -1,
                    ..Default::default()
                });
            }
        }
        let mut start = tokio::process::Command::new(&self.binary);
        start
            .arg("start")
            .arg("--attach")
            .arg(&id)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout_at(deadline, start.output()).await {
            Ok(Ok(o)) => Ok(SandboxOutput {
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).to_string(),
                exit_code: o.status.code().unwrap_or(-1),
                timed_out: false,
            }),
            Ok(Err(e)) => {
                remove().await;
                Err(anyhow::anyhow!("podman start failed: {e}"))
            }
            Err(_) => {
                remove().await;
                Ok(SandboxOutput {
                    timed_out: true,
                    exit_code: -1,
                    ..Default::default()
                })
            }
        }
    }
}

#[async_trait::async_trait]
impl SandboxBackend for PodmanBackend {
    fn name(&self) -> &str {
        "podman"
    }
    async fn run(&self, req: &SandboxRequest) -> anyhow::Result<SandboxOutput> {
        let scratch = new_scratch()?;
        // One mount covers script + driver; copy both in so no host
        // path besides scratch crosses the container boundary.
        let setup = std::fs::copy(&req.script_path, scratch.join("user.tcl"))
            .map_err(|e| anyhow::anyhow!("sandbox: cannot stage script: {e}"))
            .and_then(|_| {
                std::fs::write(scratch.join("driver.tcl"), SANDBOX_DRIVER)
                    .map_err(|e| anyhow::anyhow!("sandbox: cannot stage driver: {e}"))
            });
        if let Err(e) = setup {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(e);
        }
        if self.remote {
            let out = self
                .run_remote(&scratch, std::time::Duration::from_secs(req.timeout_secs))
                .await;
            let _ = std::fs::remove_dir_all(&scratch);
            return out;
        }
        let cidfile = std::env::temp_dir().join(format!(
            "veles_podman_cid_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let mut cmd = tokio::process::Command::from(self.build_command(&scratch, &cidfile));
        // Dropping the output future kills the podman client — but the
        // container's conmon survives that, so the timeout path also
        // does an explicit `podman rm -f` via the cidfile below.
        cmd.kill_on_drop(true);
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(req.timeout_secs),
            cmd.output(),
        )
        .await;
        let out = match res {
            Ok(Ok(o)) => SandboxOutput {
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).to_string(),
                exit_code: o.status.code().unwrap_or(-1),
                timed_out: false,
            },
            Ok(Err(e)) => {
                let _ = std::fs::remove_dir_all(&scratch);
                let _ = std::fs::remove_file(&cidfile);
                return Err(anyhow::anyhow!(
                    "podman launch failed (is podman installed? image \"{}\" present?): {e}",
                    self.image
                ));
            }
            Err(_) => {
                // The client is dead; the container may not be. Kill
                // it by recorded id — best effort, same as rlimit.
                if let Ok(cid) = std::fs::read_to_string(&cidfile) {
                    let cid = cid.trim().to_string();
                    if !cid.is_empty() {
                        let _ = tokio::process::Command::new(&self.binary)
                            .arg("rm")
                            .arg("-f")
                            .arg(&cid)
                            .output()
                            .await;
                    }
                }
                SandboxOutput {
                    timed_out: true,
                    exit_code: -1,
                    ..Default::default()
                }
            }
        };
        let _ = std::fs::remove_dir_all(&scratch);
        let _ = std::fs::remove_file(&cidfile);
        Ok(out)
    }
}

// ── nspawn backend ──────────────────────────────────────────────────

/// systemd-nspawn backend: the same safe-interp driver inside an OS
/// namespace jail. `[sandbox] image` is a rootfs here — a container
/// tree DIRECTORY or a `.raw` image (tclsh on PATH inside it, same
/// contract as podman). Root-gated: systemd-nspawn wants uid 0, so on
/// a laptop it is the VM-hosted backend (see scripts/sandbox_vm_check.sh),
/// not the default.
pub struct NspawnBackend {
    pub binary: String,
    pub rootfs: String,
    pub memory_mb: u64,
    pub network: bool,
    /// drop posture → `--drop-capability=all`; default posture leaves
    /// nspawn's already-limited set alone.
    pub drop_caps: bool,
    pub mounts: Vec<crate::config::SandboxMount>,
    pub extra_args: Vec<String>,
    pub tclsh: String,
}

impl NspawnBackend {
    /// The `systemd-nspawn` argv — separated for argv-pinning tests,
    /// which is all a root-less CI box can honestly do with nspawn.
    fn build_command(&self, scratch: &std::path::Path) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.binary);
        cmd.arg("--quiet").arg("--register=no");
        if self.rootfs.ends_with(".raw") {
            cmd.arg("--image").arg(&self.rootfs);
        } else {
            cmd.arg("--directory").arg(&self.rootfs);
        }
        cmd.arg("--bind")
            .arg(format!("{}:/work", scratch.display()));
        if !self.network {
            // The benchmark switch: no network namespace sharing.
            cmd.arg("--private-network");
        }
        // nspawn has no cgroup memory flag from the CLI; RLIMIT_AS is
        // the same best-effort ceiling native uses (the timeout stays
        // the kill switch).
        cmd.arg(format!(
            "--rlimit=AS={}",
            self.memory_mb.saturating_mul(1024 * 1024)
        ));
        if self.drop_caps {
            // LOWERCASE. systemd-nspawn parses this as a capability
            // NAME list where `all` is the special value; `ALL` is
            // rejected outright ("Failed to parse capability ALL.") and
            // the run exits 1 having done nothing.
            //
            // It shipped uppercase on 2026-09-03 — copied from podman's
            // `--cap-drop ALL`, where uppercase IS right — and no test
            // could catch it, because the only nspawn coverage was an
            // argv-pinning test asserting the flag the code emits. The
            // first time anything actually ran the backend
            // (`veles doctor --sandbox` under sudo, 2026-09-04) it
            // failed immediately. That is the whole argument for the
            // doctor probe.
            cmd.arg("--drop-capability=all");
        }
        cmd.arg("--setenv=VELES_SANDBOX_SCRIPT=/work/user.tcl")
            .arg("--setenv=HOME=/work")
            .arg("--setenv=TMPDIR=/work")
            // PATH, explicitly. The backend's contract is "tclsh on
            // PATH inside the rootfs", but nspawn does not give the
            // exec'd command a PATH that satisfies it — without this
            // the run dies with `execv(tclsh) failed: No such file or
            // directory` while /usr/bin/tclsh is sitting right there.
            // Found the first time the backend was actually run
            // (2026-09-04); the argv-pinning test could not see it,
            // because the flag it was missing was the one nobody wrote.
            .arg("--setenv=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        for m in &self.mounts {
            let flag = if m.mode == "ro" {
                "--bind-ro"
            } else {
                "--bind"
            };
            cmd.arg(flag).arg(format!("{}:{}", m.host, m.container));
        }
        for a in &self.extra_args {
            cmd.arg(a);
        }
        cmd.arg(&self.tclsh)
            .arg("/work/driver.tcl")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd
    }
}

#[async_trait::async_trait]
impl SandboxBackend for NspawnBackend {
    fn name(&self) -> &str {
        "nspawn"
    }
    async fn run(&self, req: &SandboxRequest) -> anyhow::Result<SandboxOutput> {
        if !std::path::Path::new(&self.rootfs).exists() {
            anyhow::bail!(
                "nspawn rootfs {:?} does not exist — point [sandbox] image at a \
                 container tree directory or .raw image",
                self.rootfs
            );
        }
        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            anyhow::bail!(
                "systemd-nspawn requires root — run veles as root (e.g. inside a VM) \
                 or choose another [sandbox] backend"
            );
        }
        let scratch = new_scratch()?;
        let setup = std::fs::copy(&req.script_path, scratch.join("user.tcl"))
            .map_err(|e| anyhow::anyhow!("sandbox: cannot stage script: {e}"))
            .and_then(|_| {
                std::fs::write(scratch.join("driver.tcl"), SANDBOX_DRIVER)
                    .map_err(|e| anyhow::anyhow!("sandbox: cannot stage driver: {e}"))
            });
        if let Err(e) = setup {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(e);
        }
        let mut cmd = tokio::process::Command::from(self.build_command(&scratch));
        // Killing the nspawn supervisor takes the container with it.
        cmd.kill_on_drop(true);
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(req.timeout_secs),
            cmd.output(),
        )
        .await;
        let out = match res {
            Ok(Ok(o)) => SandboxOutput {
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).to_string(),
                exit_code: o.status.code().unwrap_or(-1),
                timed_out: false,
            },
            Ok(Err(e)) => {
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(anyhow::anyhow!(
                    "nspawn launch failed (is systemd-nspawn installed? root?): {e}"
                ));
            }
            Err(_) => SandboxOutput {
                timed_out: true,
                exit_code: -1,
                ..Default::default()
            },
        };
        let _ = std::fs::remove_dir_all(&scratch);
        Ok(out)
    }
}

// ── firecracker backend ─────────────────────────────────────────────

/// Firecracker microVM backend. The STRONGEST fence: a real kernel,
/// its own address space, no shared namespaces at all. Costs artifacts
/// — a kernel image (`[sandbox] kernel_path`) and a rootfs (`[sandbox]
/// image`) whose init mounts the read-only work drive at `/work` and
/// runs `tclsh /work/driver.tcl` (the scratch dir becomes that drive:
/// mkfs.ext4 -d bakes it in per run). No network this round
/// (firecracker needs tap devices; `network = true` fails loudly), no
/// host mounts (use podman/nspawn for those).
pub struct FirecrackerBackend {
    pub binary: String,
    pub kernel: String,
    pub rootfs: String,
    pub memory_mb: u64,
    pub extra_args: Vec<String>,
}

impl FirecrackerBackend {
    /// The VM config JSON — pinned by tests without a firecracker
    /// binary present.
    ///
    /// Two drive postures, both corrected 2026-09-04:
    ///
    /// - **root is READ-ONLY.** It shipped writable, which cannot work:
    ///   the rootfs is a shared artifact (a nix store path in the
    ///   supported setup), so opening it O_RDWR either fails outright or
    ///   lets concurrent runs corrupt each other. The guest gets its
    ///   writable surfaces from tmpfs, mounted by the init.
    /// - **work is WRITABLE.** It shipped read-only while the driver
    ///   sets `HOME=/work TMPDIR=/work`, so any script that wrote a temp
    ///   file died. It is a per-run image built by `mkfs.ext4 -d` and
    ///   deleted afterwards, so writability costs nothing.
    fn build_config(&self, work_drive: &std::path::Path) -> serde_json::Value {
        serde_json::json!({
            "boot-source": {
                "kernel_image_path": self.kernel,
                // `init=` is not set: the rootfs artifact owns its own
                // /sbin/init, which is what prints the exit sentinel
                // (see EXIT_SENTINEL) and powers the VM off.
                "boot_args": "console=ttyS0 reboot=k panic=1 quiet root=/dev/vda ro",
            },
            "drives": [
                {
                    "drive_id": "root",
                    "path_on_host": self.rootfs,
                    "is_root_device": true,
                    "is_read_only": true,
                },
                {
                    "drive_id": "work",
                    "path_on_host": work_drive,
                    "is_root_device": false,
                    "is_read_only": false,
                },
            ],
            "machine-config": {
                "vcpu_count": 1,
                "mem_size_mib": self.memory_mb,
            },
        })
    }

    fn build_command(&self, config_path: &std::path::Path) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.binary);
        cmd.arg("--no-api").arg("--config-file").arg(config_path);
        for a in &self.extra_args {
            cmd.arg(a);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd
    }
}

#[async_trait::async_trait]
impl SandboxBackend for FirecrackerBackend {
    fn name(&self) -> &str {
        "firecracker"
    }
    async fn run(&self, req: &SandboxRequest) -> anyhow::Result<SandboxOutput> {
        for p in [&self.kernel, &self.rootfs] {
            if !std::path::Path::new(p).exists() {
                anyhow::bail!(
                    "firecracker artifact {p:?} does not exist — the backend needs a \
                     kernel ([sandbox] kernel_path) and a rootfs ([sandbox] image) \
                     whose init mounts the work drive at /work and runs tclsh /work/driver.tcl"
                );
            }
        }
        let scratch = new_scratch()?;
        // The staged payload lives in a SUBDIRECTORY, not in the
        // scratch root. `mkfs.ext4 -d <dir>` copies the whole tree, so
        // building the image inside the very directory being copied
        // makes the image a member of its own contents — a 32MB file
        // copied into a 32MB filesystem, which fails on no space (or
        // worse, half-succeeds). Payload in, image beside.
        let payload = scratch.join("payload");
        let setup = std::fs::create_dir_all(&payload)
            .map_err(|e| anyhow::anyhow!("sandbox: cannot stage payload dir: {e}"))
            .and_then(|_| {
                std::fs::copy(&req.script_path, payload.join("user.tcl"))
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!("sandbox: cannot stage script: {e}"))
            })
            .and_then(|_| {
                std::fs::write(payload.join("driver.tcl"), SANDBOX_DRIVER)
                    .map_err(|e| anyhow::anyhow!("sandbox: cannot stage driver: {e}"))
            });
        if let Err(e) = setup {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(e);
        }
        // Bake the payload into an ext4 drive (mkfs.ext4 -d; runs
        // unprivileged — it writes a file, not a device). 8192 x 4K
        // blocks = 32MB: plenty for a driver, a script, and the scratch
        // space HOME/TMPDIR point at inside the guest.
        let work_drive = scratch.join("work.img");
        let mkfs = std::process::Command::new("mkfs.ext4")
            .arg("-q")
            .arg("-d")
            .arg(&payload)
            .arg("-b")
            .arg("4096")
            .arg(&work_drive)
            .arg("8192")
            .output();
        match mkfs {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let _ = std::fs::remove_dir_all(&scratch);
                anyhow::bail!(
                    "mkfs.ext4 failed building the work drive: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&scratch);
                anyhow::bail!("mkfs.ext4 unavailable (e2fsprogs): {e}");
            }
        }
        let config_path = scratch.join("vm.json");
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&self.build_config(&work_drive))?,
        )?;
        let mut cmd = tokio::process::Command::from(self.build_command(&config_path));
        // Killing the VMM kills the VM.
        cmd.kill_on_drop(true);
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(req.timeout_secs),
            cmd.output(),
        )
        .await;
        let out = match res {
            Ok(Ok(o)) => {
                // The VMM's own status says whether the VM ran, not
                // whether the SCRIPT succeeded — the console sentinel
                // says that. See `split_console`.
                let console = String::from_utf8_lossy(&o.stdout).to_string();
                let (stdout, exit_code) = split_console(&console);
                let mut stderr = String::from_utf8_lossy(&o.stderr).to_string();
                if exit_code == -1 && !o.status.success() {
                    // No sentinel AND the VMM itself failed: say so,
                    // rather than leaving the caller to infer it from a
                    // bare -1.
                    stderr.push_str(&format!(
                        "\nfirecracker exited {} without the guest reporting {EXIT_SENTINEL} — \
                         the rootfs init did not finish (check the console output above)",
                        o.status.code().unwrap_or(-1)
                    ));
                }
                SandboxOutput {
                    stdout,
                    stderr,
                    exit_code,
                    timed_out: false,
                }
            }
            Ok(Err(e)) => {
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(anyhow::anyhow!(
                    "firecracker launch failed (is firecracker installed? KVM available?): {e}"
                ));
            }
            Err(_) => SandboxOutput {
                timed_out: true,
                exit_code: -1,
                ..Default::default()
            },
        };
        let _ = std::fs::remove_dir_all(&scratch);
        Ok(out)
    }
}

/// What the guest init prints, on the console, as its last act before
/// powering the VM off. `<n>` is the driver tclsh's exit status.
///
/// Firecracker gives the host exactly one channel out of a microVM
/// without networking: the serial console, multiplexed onto the VMM's
/// own stdout. So the VMM's exit code is the VMM's, not the script's,
/// and the console carries whatever the kernel and init said as well as
/// what the script said. Reporting the VMM's status as the sandbox's
/// (which is what shipped 2026-09-03) made `SandboxOutput::success()`
/// answer a different question than it was asked: firecracker exits 0
/// after cleanly running a script that failed.
///
/// The sentinel is the fix. `sandbox/firecracker/init.sh` writes it;
/// `split_console` finds it.
pub const EXIT_SENTINEL: &str = "VELES_SANDBOX_EXIT:";

/// What the guest init prints immediately BEFORE running the driver.
///
/// The guest kernel and the script share one serial line, so without a
/// start marker the "stdout" handed back to a caller begins with boot
/// messages. `quiet` thins those; it does not remove them, and a tool
/// result that opens with kernel chatter is a bad answer even when the
/// script's own output is in there somewhere.
pub const BEGIN_SENTINEL: &str = "VELES_SANDBOX_BEGIN";

/// Split a firecracker console stream into (script stdout, exit code).
///
/// Everything before the sentinel line is the script's output; the
/// number after it is its status. A stream with NO sentinel means the
/// guest never reached the end of init — kernel panic, missing rootfs
/// contract, a hang the VMM outlived — and that is reported as `-1`,
/// never as success, with the whole console kept as evidence.
pub fn split_console(console: &str) -> (String, i32) {
    // Search from the END: a script is free to print the sentinel text
    // itself, and the init's copy is always last.
    let Some(idx) = console.rfind(EXIT_SENTINEL) else {
        return (console.to_string(), -1);
    };
    // Drop everything up to and including the BEGIN marker: that is the
    // kernel's boot output, not the script's. A guest that never
    // printed one (an older rootfs) keeps the whole stream — losing
    // output is worse than including some noise.
    let head = &console[..idx];
    let before = match head.rfind(BEGIN_SENTINEL) {
        Some(b) => head[b + BEGIN_SENTINEL.len()..].trim_start_matches(['\n', '\r']),
        None => head,
    };
    let after = &console[idx + EXIT_SENTINEL.len()..];
    let code = after
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse::<i32>().ok())
        // The sentinel is there but carries no number: the init is
        // broken, which is a failure, not a zero.
        .unwrap_or(-1);
    // The sentinel sits on its own line; drop the newline that led into
    // it so a clean run's stdout is exactly what the script printed.
    (before.trim_end_matches(['\n', '\r']).to_string(), code)
}

// ── jail backend (FreeBSD) ──────────────────────────────────────────

/// FreeBSD `jail -c` backend. `[sandbox] image` is the jail ROOT tree
/// (tclsh inside it, same contract). The scratch dir rides in via
/// mount_nullfs at `<root>/work`; host mounts use nullfs too (`-o ro`
/// for read-only). Network off = `ip4=disable` + `ip6=disable` (jails
/// share the host stack unless told otherwise).
pub struct JailBackend {
    pub binary: String,
    pub root: String,
    pub network: bool,
    pub mounts: Vec<crate::config::SandboxMount>,
    pub extra_args: Vec<String>,
    pub tclsh: String,
}

/// A unique-per-run suffix. The jail ROOT is shared across runs (a
/// per-run copy of a FreeBSD base tree is not affordable), so every
/// path this backend creates INSIDE it must carry one — otherwise two
/// concurrent sandboxed runs stack nullfs mounts on the same
/// `<root>/work` and each sees the other's script. That was the shape
/// shipped 2026-09-03 and fixed 2026-09-04.
fn run_tag() -> String {
    format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

impl JailBackend {
    /// The `jail -c` argv — pinned by tests without a BSD host.
    ///
    /// `work` is the per-run directory NAME inside the jail root (see
    /// `run_tag`); the guest sees it at that path, so the env the
    /// command sets has to name it too. Everything the script is told
    /// about (`VELES_SANDBOX_SCRIPT`, `HOME`, `TMPDIR`) points there.
    ///
    /// The tail of this argv is a `command=` parameter followed by the
    /// command's own arguments, ONE PER ELEMENT — see the comment on
    /// that push. `command=` must stay last, because jail(8) treats
    /// everything after it as the command.
    fn build_args(&self, name: &str, work: &str, tclsh: &str) -> Vec<String> {
        let mut args = vec![
            "-c".to_string(),
            format!("path={}", self.root),
            format!("name={name}"),
            "host.hostname=veles-sandbox".to_string(),
        ];
        if !self.network {
            // `ip4=disable`, NOT `ip4.disable`. These are jail(8)
            // PARAMETERS with values; the dotted spelling is a
            // parameter NAME, and jail rejects it outright:
            //   jail: unknown parameter: ip4.disable
            // Shipped dotted, and pinned dotted by the test below, until
            // the fence was run for the first time on 2026-09-04.
            args.push("ip4=disable".to_string());
            args.push("ip6=disable".to_string());
        }
        for a in &self.extra_args {
            args.push(a.clone());
        }
        // jail(8) does NOT word-split `command=`. It takes the value as
        // the program, and every argv element AFTER it on the jail
        // command line as that program's arguments. Packing the whole
        // line into one `command=…` string asks the kernel to exec a
        // file literally named
        //   "env VELES_SANDBOX_SCRIPT=/… HOME=/… … /…/driver.tcl"
        // and jail reports it as
        //   jail: exec env …: No such file or directory
        // which reads like a missing interpreter and is a missing
        // SPLIT. The old comment here asserted the opposite; nothing
        // had ever run the backend to find out.
        //
        // /usr/bin/env by absolute path for the same reason nspawn now
        // sets its own PATH: the jail's PATH is not this process's to
        // assume.
        args.push("command=/usr/bin/env".to_string());
        args.push(format!("VELES_SANDBOX_SCRIPT=/{work}/user.tcl"));
        args.push(format!("HOME=/{work}"));
        args.push(format!("TMPDIR=/{work}"));
        args.push(tclsh.to_string());
        args.push(format!("/{work}/driver.tcl"));
        args
    }

    /// Which interpreter to run INSIDE the jail root.
    ///
    /// FreeBSD's `lang/tcl86` installs `/usr/local/bin/tclsh8.6` and
    /// never an unversioned `tclsh` — so the name this backend used to
    /// hardcode does not exist on the one platform the backend is for.
    /// jail(8) reports that as
    ///
    ///   jail: exec env … tclsh …/driver.tcl: No such file or directory
    ///
    /// which names the whole command line and not the missing file, and
    /// looks like a broken jail rather than a missing symlink. Found by
    /// running the fence on FreeBSD 15 for the first time, 2026-09-04.
    ///
    /// The jail root is an ordinary host path, so the interpreter can be
    /// FOUND rather than guessed. An absolute answer also drops any
    /// dependence on the jail's PATH, which is the other half of the
    /// same class of defect (nspawn shipped with no PATH at all).
    fn resolve_tclsh(&self) -> anyhow::Result<String> {
        let root = std::path::Path::new(&self.root);
        let mut candidates: Vec<String> = Vec::new();
        // An absolute configured name wins if it is really there.
        if self.tclsh.starts_with('/') {
            candidates.push(self.tclsh.clone());
        }
        candidates.extend(
            [
                "/usr/local/bin/tclsh8.6",
                "/usr/local/bin/tclsh86",
                "/usr/local/bin/tclsh",
                "/usr/bin/tclsh8.6",
                "/usr/bin/tclsh",
            ]
            .into_iter()
            .map(str::to_string),
        );
        for c in &candidates {
            if root.join(c.trim_start_matches('/')).exists() {
                return Ok(c.clone());
            }
        }
        anyhow::bail!(
            "no tclsh inside the jail root {:?} — looked for {}. \
             Build the root with `pkg -r {} install -y tcl86` (FreeBSD names it \
             tclsh8.6, not tclsh).",
            self.root,
            candidates.join(", "),
            self.root
        )
    }
}

#[async_trait::async_trait]
impl SandboxBackend for JailBackend {
    fn name(&self) -> &str {
        "jail"
    }
    async fn run(&self, req: &SandboxRequest) -> anyhow::Result<SandboxOutput> {
        if !std::path::Path::new(&self.root).is_dir() {
            anyhow::bail!(
                "jail root {:?} does not exist — point [sandbox] image at a FreeBSD \
                 jail tree with tclsh inside",
                self.root
            );
        }
        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            anyhow::bail!("jail(8) requires root — this backend runs on a BSD host as root");
        }
        // Everything this run creates inside the SHARED jail root
        // carries this tag — see `run_tag`.
        let tag = run_tag();
        let work_name = format!(".veles-work-{tag}");
        let scratch = new_scratch()?;
        let setup = std::fs::copy(&req.script_path, scratch.join("user.tcl"))
            .map_err(|e| anyhow::anyhow!("sandbox: cannot stage script: {e}"))
            .and_then(|_| {
                std::fs::write(scratch.join("driver.tcl"), SANDBOX_DRIVER)
                    .map_err(|e| anyhow::anyhow!("sandbox: cannot stage driver: {e}"))
            })
            .and_then(|_| {
                std::fs::create_dir_all(std::path::Path::new(&self.root).join(&work_name))
                    .map_err(|e| anyhow::anyhow!("sandbox: cannot prepare jail work dir: {e}"))
            });
        if let Err(e) = setup {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(e);
        }
        // nullfs the scratch in at <root>/work, plus every explicit
        // mount (ro mode rides `-o ro`).
        let mut mounted: Vec<std::path::PathBuf> = Vec::new();
        let nullfs = |src: &std::path::Path, dst: &std::path::Path, ro: bool| {
            let mut c = std::process::Command::new("mount_nullfs");
            if ro {
                c.arg("-o").arg("ro");
            }
            let out = c.arg(src).arg(dst).output()?;
            if !out.status.success() {
                return Err(std::io::Error::other(format!(
                    "mount_nullfs {} -> {} failed: {}",
                    src.display(),
                    dst.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok::<(), std::io::Error>(())
        };
        let work_in_root = std::path::Path::new(&self.root).join(&work_name);
        if let Err(e) = nullfs(&scratch, &work_in_root, false) {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(anyhow::anyhow!("{e}"));
        }
        mounted.push(work_in_root);
        // Explicit mounts KEEP their configured container path — that
        // path is the contract the script is written against, so it
        // cannot carry a run tag the way the work dir does. Concurrent
        // runs therefore stack nullfs on the same target, which is safe
        // here precisely because they mount the SAME host source: the
        // stack is idempotent and each run's umount pops exactly its
        // own layer. (`work` was NOT safe that way — each run's source
        // is a different scratch dir, so run B's mount hid run A's
        // script. That is the collision the tag fixes.)
        for m in &self.mounts {
            let dst = std::path::Path::new(&self.root).join(m.container.trim_start_matches('/'));
            let _ = std::fs::create_dir_all(&dst);
            if let Err(e) = nullfs(std::path::Path::new(&m.host), &dst, m.mode == "ro") {
                for d in mounted.iter().rev() {
                    let _ = std::process::Command::new("umount").arg(d).output();
                }
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(anyhow::anyhow!("{e}"));
            }
            mounted.push(dst);
        }
        let name = format!("veles_sb_{tag}");
        let tclsh = match self.resolve_tclsh() {
            Ok(t) => t,
            Err(e) => {
                for d in mounted.iter().rev() {
                    let _ = std::process::Command::new("umount").arg(d).output();
                }
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(e);
            }
        };
        let mut cmd = tokio::process::Command::new(&self.binary);
        for a in self.build_args(&name, &work_name, &tclsh) {
            cmd.arg(a);
        }
        cmd.kill_on_drop(true);
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(req.timeout_secs),
            cmd.output(),
        )
        .await;
        let out = match res {
            Ok(Ok(o)) => SandboxOutput {
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).to_string(),
                exit_code: o.status.code().unwrap_or(-1),
                timed_out: false,
            },
            Ok(Err(e)) => {
                for d in mounted.iter().rev() {
                    let _ = std::process::Command::new("umount").arg(d).output();
                }
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(anyhow::anyhow!(
                    "jail launch failed (is this a BSD host?): {e}"
                ));
            }
            Err(_) => {
                // The jail(8) client may be dead while the jail lives:
                // remove it by name, podman-cidfile style.
                let _ = tokio::process::Command::new(&self.binary)
                    .arg("-r")
                    .arg(&name)
                    .output()
                    .await;
                SandboxOutput {
                    timed_out: true,
                    exit_code: -1,
                    ..Default::default()
                }
            }
        };
        for d in mounted.iter().rev() {
            let _ = std::process::Command::new("umount").arg(d).output();
        }
        // The tagged mountpoint is ours alone, so remove it — a shared
        // jail root must not accumulate one empty `.veles-work-*` per
        // sandboxed run forever. `remove_dir` (not `_all`) on purpose:
        // it refuses a non-empty directory, which is exactly the signal
        // that the umount above did not take.
        let _ = std::fs::remove_dir(std::path::Path::new(&self.root).join(&work_name));
        let _ = std::fs::remove_dir_all(&scratch);
        Ok(out)
    }
}

// ── userland backend (the phone) ────────────────────────────────────

/// The phone's userland package (`[sandbox] backend = "userland"`,
/// PLAN-phone-userland.md T3 design item 4): the same safe-interp driver,
/// run by the userland's own tclsh under proot, in the userland's rootfs.
///
/// Shaped like [`JailBackend`]: the root is shared — here the userland's
/// rootfs, the jail root's counterpart, where `apk add tcl` puts the
/// interpreter — and every run works in a directory of its own inside it,
/// tagged ([`run_tag`]) so two concurrent runs never share one, and
/// removed after. The gateway cannot write into the other app's files, so
/// the script travels on the guest command's stdin and the driver as an
/// argument; nothing of the engine's environment crosses
/// (`userland::command`). The named boxes of user envs are the per-name
/// rootfs copies (`user_envs::Runtime::Userland`); this fence has no name.
///
/// Refused at resolution, because nothing here could keep them:
/// `[[sandbox.mounts]]` (the gateway's paths mean nothing in the other
/// app), `network = false` (proot has no network namespace), and
/// `extra_args` (there is no command line of a runtime to extend).
/// `memory_mb` is a `ulimit -v` inside the guest, best effort, like the
/// native fence's.
pub struct UserlandBackend {
    pub ulsh: std::path::PathBuf,
    pub memory_mb: u64,
}

/// The fence's runner in the guest. `$1` the run's directory, `$2` the
/// memory ceiling in KiB, `$3` the driver; the script on stdin.
pub const USERLAND_FENCE_RUNNER: &str = r#"w=$1 kb=$2 d=$3
t=$(command -v tclsh || command -v tclsh8.6 || command -v tclsh9.0) || { echo "no tclsh in the userland: apk add tcl (the terminal runs there)" >&2; exit 127; }
rm -rf "$w" && mkdir -p "$w" && cat > "$w/user.tcl" && printf '%s' "$d" > "$w/driver.tcl" || exit 97
cd "$w" || exit 97
( ulimit -v "$kb" 2>/dev/null; HOME="$w" TMPDIR="$w" VELES_SANDBOX_SCRIPT="$w/user.tcl" exec "$t" "$w/driver.tcl" ) </dev/null
c=$?
cd / && rm -rf "$w"
exit $c"#;

impl UserlandBackend {
    /// The ulsh command for one run in `work` — pinned by tests. The
    /// rootfs, not a named box; `/` as the cwd (the runner makes `work`).
    fn build_command(
        &self,
        work: &str,
        ambient: impl Fn(&str) -> Option<String>,
    ) -> std::process::Command {
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            USERLAND_FENCE_RUNNER.to_string(),
            "veles-fence".to_string(),
            work.to_string(),
            (self.memory_mb * 1024).to_string(),
            SANDBOX_DRIVER.to_string(),
        ];
        let call = crate::userland::GuestCall {
            sandbox: None,
            cwd: "/",
            home: None,
            env: &[],
            argv: &argv,
        };
        crate::userland::command(&self.ulsh, &call, ambient)
    }
}

#[async_trait::async_trait]
impl SandboxBackend for UserlandBackend {
    fn name(&self) -> &str {
        "userland"
    }
    async fn run(&self, req: &SandboxRequest) -> anyhow::Result<SandboxOutput> {
        let script = std::fs::read(&req.script_path)
            .map_err(|e| anyhow::anyhow!("sandbox: cannot stage script: {e}"))?;
        let work = format!("/tmp/.veles-work-{}", run_tag());
        let mut cmd =
            tokio::process::Command::from(self.build_command(&work, |k| std::env::var(k).ok()));
        // kill_on_drop IS the timeout here too (NativeBackend says why):
        // a dropped ulsh hangs up, and the broker kills the guest's group.
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("userland launch failed ({}): {e}", self.ulsh.display())
        })?;
        let feed = child.stdin.take();
        let run = async move {
            if let Some(mut w) = feed {
                use tokio::io::AsyncWriteExt;
                let _ = w.write_all(&script).await;
            }
            child.wait_with_output().await
        };
        match tokio::time::timeout(std::time::Duration::from_secs(req.timeout_secs), run).await {
            Ok(Ok(o)) => Ok(SandboxOutput {
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).to_string(),
                exit_code: o.status.code().unwrap_or(-1),
                timed_out: false,
            }),
            Ok(Err(e)) => Err(anyhow::anyhow!("the userland run did not finish: {e}")),
            Err(_) => {
                // The guest was killed mid-run, before its own cleanup:
                // the run's directory goes now, best effort.
                let argv = vec!["rm".to_string(), "-rf".to_string(), work.clone()];
                let call = crate::userland::GuestCall {
                    sandbox: None,
                    cwd: "/",
                    home: None,
                    env: &[],
                    argv: &argv,
                };
                let mut rm = tokio::process::Command::from(crate::userland::command(
                    &self.ulsh,
                    &call,
                    |k| std::env::var(k).ok(),
                ));
                rm.stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true);
                let _ = tokio::time::timeout(std::time::Duration::from_secs(30), rm.status()).await;
                Ok(SandboxOutput {
                    timed_out: true,
                    exit_code: -1,
                    ..Default::default()
                })
            }
        }
    }
}

/// What the userland cannot keep of this section, as `(key, refusal)`:
/// `None` when it can keep all of it. The one list, for the named backend
/// (which refuses with it) and the default (which stays native on it).
fn userland_refusal(
    section: &crate::config::SandboxSection,
) -> Option<(&'static str, &'static str)> {
    if !section.mounts.is_empty() {
        Some((
            "mounts",
            "[sandbox] userland cannot mount host paths: the gateway's files are another \
             app's to the userland — drop [[sandbox.mounts]], or use podman/nspawn",
        ))
    } else if !section.network {
        Some((
            "network",
            "[sandbox] userland has no network namespace (proot shares the phone's network), \
             so network = false cannot be kept — use podman/nspawn for that posture",
        ))
    } else if !section.extra_args.is_empty() {
        Some((
            "extra_args",
            "[sandbox] userland takes no extra_args: there is no runtime command line to \
             extend",
        ))
    } else {
        None
    }
}

/// `[sandbox] backend = "userland"` with the ulsh the userland answers
/// through (`userland::ulsh`, injected so tests need no phone).
fn userland_backend(
    section: &crate::config::SandboxSection,
    ulsh: Option<std::path::PathBuf>,
) -> anyhow::Result<Box<dyn SandboxBackend>> {
    if let Some((_, refusal)) = userland_refusal(section) {
        anyhow::bail!("{refusal}");
    }
    let ulsh = ulsh.ok_or_else(|| {
        anyhow::anyhow!(
            "[sandbox] backend = \"userland\", but the userland package does not answer (the \
             engine's shell is not ulsh): install Veles userland, then veles, and restart the \
             gateway (docs/phone.md \"The userland package\")"
        )
    })?;
    Ok(Box::new(UserlandBackend {
        ulsh,
        memory_mb: section.memory_mb,
    }))
}

// ── selection ───────────────────────────────────────────────────────

/// Validate the explicit-mount list. Shared by every backend that
/// accepts mounts: mode ro|rw, absolute host AND container paths. The
/// error names the offender — a config line that fails silently at
/// container start instead of at resolution is the api_mode lie.
fn validate_mounts(mounts: &[crate::config::SandboxMount]) -> anyhow::Result<()> {
    for m in mounts {
        if m.host.trim().is_empty() || !std::path::Path::new(&m.host).is_absolute() {
            anyhow::bail!(
                "[sandbox] mounts: host path must be absolute (got {:?})",
                m.host
            );
        }
        if m.container.trim().is_empty() || !m.container.starts_with('/') {
            anyhow::bail!(
                "[sandbox] mounts: container path must be absolute (got {:?})",
                m.container
            );
        }
        if m.mode != "ro" && m.mode != "rw" {
            anyhow::bail!(
                "[sandbox] mounts: mode must be \"ro\" or \"rw\" (got {:?})",
                m.mode
            );
        }
    }
    Ok(())
}

/// The backend a `[sandbox] backend` value comes to on this node, and
/// why — what `veles doctor --sandbox` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBackend {
    /// The name `backend_from` builds from.
    pub name: String,
    /// `configured` for a name the config gave; for `auto`, what decided.
    pub why: String,
}

impl std::fmt::Display for ResolvedBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name, self.why)
    }
}

/// `[sandbox] backend = "auto"` — what an Android build's config reads
/// when nobody set the key (`config::default_sandbox_backend`; the user's
/// call 2026-10-20: the userland is the phone's default sandbox): the
/// userland where this is Android and it answers (`ulsh`: the engine's
/// shell is the userland's, `userland::ulsh`) and keeps what the section
/// asks of it, else `native`, the default everywhere else. Any other
/// value is the config's own word and wins. `android` and `ulsh` are
/// parameters so a test can be the phone.
pub fn resolve_backend(
    section: &crate::config::SandboxSection,
    android: bool,
    ulsh: Option<&std::path::Path>,
) -> ResolvedBackend {
    let native = |why: &str| ResolvedBackend {
        name: "native".into(),
        why: why.into(),
    };
    if section.backend.trim() != "auto" {
        return ResolvedBackend {
            name: section.backend.clone(),
            why: "configured".into(),
        };
    }
    if !android {
        return native("default");
    }
    if ulsh.is_none() {
        return native("default on android, but the userland does not answer");
    }
    // The boxes' own network posture is refused when a box is made, not
    // at resolution (user_envs), so the default looks at it here too.
    let refused = userland_refusal(section)
        .map(|(key, _)| key)
        .or_else(|| (section.user_env_network.trim() == "none").then_some("user_env_network"));
    if let Some(key) = refused {
        return native(&format!(
            "default on android, but the userland cannot keep [sandbox] {key}"
        ));
    }
    ResolvedBackend {
        name: "userland".into(),
        why: "default on android, broker answers".into(),
    }
}

/// [`resolve_backend`] on this node.
pub fn effective_backend(section: &crate::config::SandboxSection) -> ResolvedBackend {
    resolve_backend(
        section,
        cfg!(target_os = "android"),
        crate::userland::ulsh().as_deref(),
    )
}

/// Resolve the configured backend. Unknown names FAIL — the api_mode
/// precedent: a config line that silently does nothing is a lie.
pub fn backend_from(
    section: &crate::config::SandboxSection,
) -> anyhow::Result<Box<dyn SandboxBackend>> {
    backend_from_with(
        section,
        cfg!(target_os = "android"),
        crate::userland::ulsh(),
    )
}

/// [`backend_from`] with the node's facts given (see [`resolve_backend`]).
fn backend_from_with(
    section: &crate::config::SandboxSection,
    android: bool,
    ulsh: Option<std::path::PathBuf>,
) -> anyhow::Result<Box<dyn SandboxBackend>> {
    match resolve_backend(section, android, ulsh.as_deref())
        .name
        .as_str()
    {
        // `host` is the spelled-out alias: a tclsh subprocess ON THE
        // HOST (safe-interp restricted at the Tcl level, RLIMIT_AS
        // hedge). Some jobs only make sense with real host access, so
        // this stays the default and always available.
        "native" | "host" => Ok(Box::new(NativeBackend::new(
            crate::executecode::tclsh_path(),
        ))),
        "podman" => {
            let drop_caps = capabilities_flag(&section.capabilities)?;
            validate_mounts(&section.mounts)?;
            Ok(Box::new(PodmanBackend {
                binary: std::env::var("VELES_PODMAN_BIN").unwrap_or_else(|_| "podman".into()),
                image: section.image.clone(),
                memory_mb: section.memory_mb,
                network: section.network,
                drop_caps,
                mounts: section.mounts.clone(),
                extra_args: section.extra_args.clone(),
                tclsh: "tclsh".into(),
                remote: crate::user_envs::podman_remote(),
            }))
        }
        "nspawn" => {
            let drop_caps = capabilities_flag(&section.capabilities)?;
            validate_mounts(&section.mounts)?;
            Ok(Box::new(NspawnBackend {
                binary: std::env::var("VELES_NSPAWN_BIN")
                    .unwrap_or_else(|_| "systemd-nspawn".into()),
                rootfs: section.image.clone(),
                memory_mb: section.memory_mb,
                network: section.network,
                drop_caps,
                mounts: section.mounts.clone(),
                extra_args: section.extra_args.clone(),
                tclsh: "tclsh".into(),
            }))
        }
        "firecracker" => {
            // The artifact contract is enforced at run() with messages
            // that say what is missing; the things resolution CAN see
            // fail here.
            if section.network {
                anyhow::bail!(
                    "[sandbox] firecracker has no networking this round (it needs tap \
                     devices) — set network = false or use podman/nspawn"
                );
            }
            if !section.mounts.is_empty() {
                anyhow::bail!(
                    "[sandbox] firecracker cannot mount host paths (no virtio-fs) — \
                     use podman or nspawn for explicit mounts"
                );
            }
            let kernel = section.kernel_path.clone().filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "[sandbox] backend = \"firecracker\" needs kernel_path = \"/path/to/vmlinux\" \
                         (plus image = \"/path/to/rootfs\" whose init mounts the work drive)"
                    )
                })?;
            Ok(Box::new(FirecrackerBackend {
                binary: std::env::var("VELES_FIRECRACKER_BIN")
                    .unwrap_or_else(|_| "firecracker".into()),
                kernel,
                rootfs: section.image.clone(),
                memory_mb: section.memory_mb,
                extra_args: section.extra_args.clone(),
            }))
        }
        "jail" => {
            validate_mounts(&section.mounts)?;
            Ok(Box::new(JailBackend {
                binary: std::env::var("VELES_JAIL_BIN").unwrap_or_else(|_| "jail".into()),
                root: section.image.clone(),
                network: section.network,
                mounts: section.mounts.clone(),
                extra_args: section.extra_args.clone(),
                tclsh: "tclsh".into(),
            }))
        }
        // The phone's userland package (PLAN-phone-userland.md T3 design
        // item 4): this fence in its rootfs, and user envs' named boxes
        // as rootfs copies of their own (user_envs::Runtime::Userland).
        "userland" => userland_backend(section, ulsh),
        other => anyhow::bail!(
            "[sandbox] backend = \"{other}\" is not implemented — this build ships \
             \"native\"/\"host\", \"podman\", \"nspawn\", \"firecracker\", \"jail\" and \
             \"userland\" (see docs/plugin-plan.md for the runtime each needs)"
        ),
    }
}

/// `drop` → true (--cap-drop ALL / --drop-capability=all), `default`
/// → false. Anything else fails loudly — the posture is a security
/// decision, not a place for typos to pick a default.
fn capabilities_flag(capabilities: &str) -> anyhow::Result<bool> {
    match capabilities {
        "drop" => Ok(true),
        "default" => Ok(false),
        other => anyhow::bail!(
            "[sandbox] capabilities = \"{other}\" is not implemented — \
             \"drop\" (hardened) or \"default\" (package-install posture)"
        ),
    }
}

/// Run a script through the CONFIGURED backend. This is the one door
/// every sandboxed caller (execute_code's sandbox flag, cron no_agent
/// jobs, IRC unprivileged `tcl`) goes through.
pub async fn execute(
    script_path: &std::path::Path,
    timeout_secs: u64,
) -> anyhow::Result<SandboxOutput> {
    let section = crate::config::SandboxSection::current();
    let backend = backend_from(&section)?;
    backend
        .run(&SandboxRequest {
            script_path: script_path.to_path_buf(),
            timeout_secs,
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The native fence's memory ceiling is set by `ulimit` before exec:
    /// address space everywhere but Android, where that limit aborts
    /// every bionic process and the data segment is the cap instead
    /// (measured on the phone, 2026-10-14).
    #[cfg(unix)]
    #[test]
    fn mem_limited_command_uses_the_rlimit_this_os_survives() {
        let cmd = mem_limited_command("tclsh", std::path::Path::new("/d/driver.tcl"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let kb = SANDBOX_MEM_LIMIT_BYTES / 1024;
        let flag = if cfg!(target_os = "android") {
            "-d"
        } else {
            "-v"
        };
        assert!(
            args[1].starts_with(&format!("ulimit {flag} {kb} ")),
            "the limit this OS survives: {}",
            args[1]
        );
        assert!(args[1].ends_with("exec \"$0\" \"$@\""), "{}", args[1]);
        assert_eq!(
            &args[2..],
            ["tclsh", "/d/driver.tcl"],
            "argv passes through"
        );
    }

    /// The wrapper runs the shell `crate::shell` resolves, not a literal
    /// `/bin/sh`: on the phone that was mksh while every other spawn ran
    /// the APK's bash (PLAN-phone-userland.md T1), and a node with no
    /// `/bin/sh` at all lost execute_code entirely. `VELES_SHELL` points
    /// at a scratch shell (a pass-through, so a neighbour that resolves
    /// its shell meanwhile still gets one that works).
    #[cfg(unix)]
    #[test]
    fn mem_limited_command_runs_the_resolved_shell() {
        use std::os::unix::fs::PermissionsExt;
        // The process-wide cache first, so this test's override cannot
        // become every other test's shell.
        let _ = crate::shell::shell_binary();
        let _g = crate::testenv::lock();
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("scratch-shell");
        std::fs::write(&fake, "#!/bin/sh\nexec sh \"$@\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let prev = std::env::var(crate::shell::SHELL_ENV).ok();
        std::env::set_var(crate::shell::SHELL_ENV, &fake);
        let cmd = mem_limited_command("tclsh", std::path::Path::new("/d/driver.tcl"));
        match prev {
            Some(v) => std::env::set_var(crate::shell::SHELL_ENV, v),
            None => std::env::remove_var(crate::shell::SHELL_ENV),
        }
        assert_eq!(
            cmd.get_program(),
            fake.as_os_str(),
            "the memory wrapper must run the resolved shell"
        );
    }
    use crate::config::SandboxSection;

    /// Run through the NATIVE backend directly — tests must not depend
    /// on whatever `[sandbox]` section the surrounding config happens
    /// to carry.
    async fn drive(code: &str, timeout_secs: u64) -> SandboxOutput {
        let dir = new_scratch().unwrap();
        let script = dir.join("user.tcl");
        std::fs::write(&script, code).unwrap();
        let backend = NativeBackend::new(crate::executecode::tclsh_path());
        let out = backend
            .run(&SandboxRequest {
                script_path: script,
                timeout_secs,
            })
            .await
            .expect("native backend run");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[tokio::test]
    async fn pure_computes_value() {
        let out = drive(
            "set x 40; set y 2; puts [expr {$x + $y}]",
            SANDBOX_DEFAULT_TIMEOUT_SECS,
        )
        .await;
        assert!(out.success(), "clean exit: {:?}", out);
        assert!(out.stdout.contains("42"), "the result arrives: {:?}", out);
    }

    #[tokio::test]
    async fn dangerous_families_are_gone() {
        for probe in [
            "puts [exec /bin/echo pwned]",
            "set f [open /etc/passwd r]",
            "global env; puts [exec ls]",
            "load /usr/lib/libc.so",
        ] {
            let out = drive(probe, SANDBOX_DEFAULT_TIMEOUT_SECS).await;
            assert!(
                !out.success() && !out.stdout.contains("pwned") && !out.stdout.contains("root:"),
                "{probe:?} must fail, got {out:?}"
            );
        }
    }

    #[tokio::test]
    async fn unbalanced_brackets_are_refused() {
        let out = drive("proc x {} { puts [string length hi", 10).await;
        assert_eq!(out.exit_code, 2, "explicit refusal exit: {out:?}");
        assert!(
            out.stdout.contains("unbalanced") || out.stderr.contains("unbalanced"),
            "the refusal says why: {out:?}"
        );
    }

    #[tokio::test]
    async fn runaway_recursion_is_capped() {
        let out = drive("proc boom {} { boom }; boom", SANDBOX_DEFAULT_TIMEOUT_SECS).await;
        assert!(!out.success(), "recursion cap must stop it: {out:?}");
    }

    #[tokio::test]
    async fn timeout_kills_the_process_not_just_reports_it() {
        // Regression for the leak shipped in d17a8ed: a timed-out native
        // run used to be merely ABANDONED (no kill_on_drop), so this very
        // test leaked one core-burning tclsh orphan per suite run.
        //
        // `after` is hidden from the safe interp, but the driver's OWN
        // eval would still return; the timeout machinery must fire for
        // anything that genuinely hangs the tclsh process. Busy-loop Tcl
        // without event-loop commands: `while 1 {}` spins the interpreter
        // until the kill switch lands.
        let dir = new_scratch().unwrap();
        let script = dir.join("user.tcl");
        std::fs::write(&script, "while 1 {}").unwrap();
        let marker = dir.to_string_lossy().to_string();
        let backend = NativeBackend::new(crate::executecode::tclsh_path());
        let started = std::time::Instant::now();
        let out = backend
            .run(&SandboxRequest {
                script_path: script,
                timeout_secs: 2,
            })
            .await
            .expect("native backend run");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.timed_out, "the backend reports the timeout: {out:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the kill switch is not ceremonial"
        );
        // The report is half the contract; the other half is that nothing
        // SURVIVES. SIGKILL delivery + reap is not instantaneous, so poll
        // with a deadline — a leaked spinner would never leave, so any
        // deadline miss is the regression itself.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !proc_cmdlines().any(|c| c.contains(marker.as_str())) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the timed-out sandbox process SURVIVED — it would spin at 100% CPU forever"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// cmdline of every process on the box (NUL separators become spaces).
    fn proc_cmdlines() -> impl Iterator<Item = String> {
        std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .chars()
                    .all(|c| c.is_ascii_digit())
            })
            .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
            .map(|raw| String::from_utf8_lossy(&raw).replace('\0', " "))
    }

    #[test]
    fn podman_command_pins_the_security_flags() {
        let b = PodmanBackend {
            binary: "podman".into(),
            image: "veles-sandbox:latest".into(),
            memory_mb: 256,
            network: false,
            drop_caps: true,
            mounts: vec![crate::config::SandboxMount {
                host: "/home/user/data".into(),
                container: "/data".into(),
                mode: "ro".into(),
            }],
            extra_args: vec!["--cpus".into(), "1".into()],
            tclsh: "tclsh".into(),
            remote: false,
        };
        let scratch = std::path::Path::new("/tmp/veles_sandbox_test");
        let cidfile = std::path::Path::new("/tmp/veles_cid_test");
        let cmd = b.build_command(scratch, cidfile);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let joined = args.join(" ");
        assert_eq!(cmd.get_program(), "podman");
        assert!(
            joined.contains("--network none"),
            "benchmark posture: {joined}"
        );
        assert!(joined.contains("--memory 256m"), "memory cap: {joined}");
        assert!(joined.contains("--cap-drop ALL"), "caps dropped: {joined}");
        assert!(
            joined.contains("no-new-privileges"),
            "no setuid escalation: {joined}"
        );
        assert!(joined.contains("--pull=never"), "no remote pull: {joined}");
        assert!(
            joined.contains("/tmp/veles_sandbox_test:/work"),
            "scratch mount: {joined}"
        );
        assert!(
            joined.contains("-v /home/user/data:/data:ro"),
            "explicit mounts ride with their mode: {joined}"
        );
        assert!(joined.contains("--cidfile /tmp/veles_cid_test"), "{joined}");
        assert!(
            joined.contains("--cpus 1"),
            "extra_args pass through: {joined}"
        );
        assert!(
            args.ends_with(&["tclsh".to_string(), "/work/driver.tcl".to_string()]),
            "driver is the container entrypoint: {joined}"
        );
        // Network flips with config, and the dev posture keeps caps the
        // runtime grants (package installs need them inside the userns).
        let b2 = PodmanBackend {
            binary: "podman".into(),
            image: "i".into(),
            memory_mb: 1,
            network: true,
            drop_caps: false,
            mounts: vec![],
            extra_args: vec![],
            tclsh: "tclsh".into(),
            remote: false,
        };
        let joined2 = b2
            .build_command(scratch, cidfile)
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        // `network = true` is the isolated posture: no flag at all, never
        // the host's namespace.
        assert!(!joined2.contains("--network"), "{joined2}");
        assert!(
            !joined2.contains("--cap-drop"),
            "default caps kept: {joined2}"
        );
        assert!(
            joined2.contains("no-new-privileges"),
            "…but setuid escalation stays blocked in BOTH postures: {joined2}"
        );
    }

    /// Through a remote podman (CONTAINER_HOST) the run carries no host
    /// path: the same fence, `create` instead of `run`, and the scratch
    /// dir goes in by `podman cp` (`run_remote`).
    #[test]
    fn a_remote_podman_run_mounts_no_host_path() {
        let b = PodmanBackend {
            binary: "podman".into(),
            image: "veles-sandbox:latest".into(),
            memory_mb: 256,
            network: true,
            drop_caps: true,
            mounts: vec![],
            extra_args: vec![],
            tclsh: "tclsh".into(),
            remote: true,
        };
        let args: Vec<String> = b
            .build_create_command()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let joined = args.join(" ");
        assert!(joined.starts_with("create --rm --pull=never"), "{joined}");
        assert!(
            !joined.contains(":/work"),
            "no bind mount of /work: {joined}"
        );
        assert!(
            !joined.contains("--cidfile"),
            "the id is create's stdout: {joined}"
        );
        assert!(
            !joined.contains("--network"),
            "isolated, not host: {joined}"
        );
        assert!(joined.contains("--memory 256m"), "{joined}");
        assert!(joined.contains("--cap-drop ALL"), "{joined}");
        assert!(joined.contains("no-new-privileges"), "{joined}");
        assert!(joined.contains("-w /work"), "{joined}");
        assert!(
            args.ends_with(&["tclsh".to_string(), "/work/driver.tcl".to_string()]),
            "{joined}"
        );
    }

    #[test]
    fn unknown_backend_name_fails_loudly() {
        let s = SandboxSection {
            backend: "qemu".into(),
            ..Default::default()
        };
        // anyhow::Error is not Debug — .err().unwrap() sidesteps the
        // unwrap_err bound.
        let err = backend_from(&s).err().expect("must refuse unknown backend");
        let msg = err.to_string();
        assert!(
            msg.contains("not implemented") && msg.contains("qemu"),
            "the error names the offender: {msg}"
        );
    }

    /// `userland` is the phone's own sandbox runtime (the userland
    /// package's proot, PLAN-phone-userland.md T3 design item 4): it
    /// resolves where the userland answers, says what is missing where it
    /// does not, and refuses the knobs it could not keep — the firecracker
    /// precedent: a config line that silently does nothing is a lie.
    #[test]
    fn userland_backend_resolves_where_the_userland_answers_and_refuses_what_it_cannot_keep() {
        let base = SandboxSection {
            backend: "userland".into(),
            ..Default::default()
        };
        let ulsh = || Some(std::path::PathBuf::from("/lib/libulsh.so"));
        let b = userland_backend(&base, ulsh()).expect("resolves where ulsh answers");
        assert_eq!(b.name(), "userland");
        let msg = userland_backend(&base, None).err().unwrap().to_string();
        assert!(
            msg.contains("does not answer") && msg.contains("docs/phone.md"),
            "{msg}"
        );
        // backend_from names it (no "not yet built" stub any more); this
        // test machine's shell is not ulsh, so it is the same refusal.
        let msg = backend_from(&base).err().unwrap().to_string();
        assert!(msg.contains("does not answer"), "{msg}");

        let mut s = base.clone();
        s.mounts = vec![crate::config::SandboxMount {
            host: "/etc".into(),
            container: "/etc".into(),
            mode: "ro".into(),
        }];
        let msg = userland_backend(&s, ulsh()).err().unwrap().to_string();
        assert!(msg.contains("mount"), "{msg}");
        let mut s = base.clone();
        s.network = false;
        let msg = userland_backend(&s, ulsh()).err().unwrap().to_string();
        assert!(msg.contains("network namespace"), "{msg}");
        let mut s = base.clone();
        s.extra_args = vec!["--privileged".into()];
        let msg = userland_backend(&s, ulsh()).err().unwrap().to_string();
        assert!(msg.contains("extra_args"), "{msg}");
    }

    /// The phone's default (the user's call, 2026-10-20: "the userland
    /// backend for the phone is probably good to use by default"): with
    /// `[sandbox] backend` unset — which on an Android build reads
    /// `auto` — the fence is the userland where it answers, and the old
    /// default where it does not. An explicit name always wins, and off
    /// Android nothing changes. `android` and `ulsh` are injected so the
    /// laptop can say what the phone would.
    #[test]
    fn the_default_is_the_userland_on_android_when_it_answers() {
        let auto = SandboxSection {
            backend: "auto".into(),
            ..Default::default()
        };
        let ulsh = || Some(std::path::PathBuf::from("/lib/libulsh.so"));

        // Android and the userland answers: userland, and it says why.
        let r = resolve_backend(&auto, true, ulsh().as_deref());
        assert_eq!(r.name, "userland");
        assert_eq!(r.why, "default on android, broker answers");
        assert_eq!(
            r.to_string(),
            "userland (default on android, broker answers)"
        );
        let b = backend_from_with(&auto, true, ulsh()).expect("resolves");
        assert_eq!(b.name(), "userland");

        // Android and it does not answer: the old default, with the reason.
        let r = resolve_backend(&auto, true, None);
        assert_eq!(r.name, "native");
        assert!(r.why.contains("userland does not answer"), "{}", r.why);
        assert_eq!(
            backend_from_with(&auto, true, None).unwrap().name(),
            "native"
        );

        // Off Android the same key is the old default, ulsh or not.
        for u in [None, ulsh()] {
            let r = resolve_backend(&auto, false, u.as_deref());
            assert_eq!(r.name, "native");
            assert_eq!(backend_from_with(&auto, false, u).unwrap().name(), "native");
        }

        // An explicit value wins, on Android with the userland answering.
        for (set, want) in [
            ("native", "native"),
            ("host", "native"),
            ("userland", "userland"),
        ] {
            let s = SandboxSection {
                backend: set.into(),
                ..Default::default()
            };
            let r = resolve_backend(&s, true, ulsh().as_deref());
            assert_eq!(r.why, "configured", "{set}");
            assert_eq!(
                backend_from_with(&s, true, ulsh()).unwrap().name(),
                want,
                "{set}"
            );
        }
        // An explicit unknown name is still refused, never defaulted over.
        let bad = SandboxSection {
            backend: "qemu".into(),
            ..Default::default()
        };
        assert!(backend_from_with(&bad, true, ulsh()).is_err());
    }

    /// What the userland cannot keep (mounts, no network, extra_args)
    /// makes the DEFAULT stay native with the reason, rather than fail
    /// every sandboxed call on a phone whose owner asked for a posture
    /// the default never knew about; naming `userland` still refuses.
    #[test]
    fn the_default_does_not_override_a_posture_the_userland_cannot_keep() {
        let ulsh = std::path::PathBuf::from("/lib/libulsh.so");
        let s = SandboxSection {
            backend: "auto".into(),
            network: false,
            ..Default::default()
        };
        let r = resolve_backend(&s, true, Some(&ulsh));
        assert_eq!(r.name, "native");
        assert!(r.why.contains("network"), "{}", r.why);
        let s = SandboxSection {
            backend: "auto".into(),
            user_env_network: "none".into(),
            ..Default::default()
        };
        let r = resolve_backend(&s, true, Some(&ulsh));
        assert_eq!(r.name, "native");
        assert!(r.why.contains("user_env_network"), "{}", r.why);
        let named = SandboxSection {
            backend: "userland".into(),
            network: false,
            ..Default::default()
        };
        assert!(backend_from_with(&named, true, Some(ulsh)).is_err());
    }

    /// The default is spelled `auto` only where it can mean something.
    #[test]
    fn the_section_default_is_auto_on_android_and_native_elsewhere() {
        let want = if cfg!(target_os = "android") {
            "auto"
        } else {
            "native"
        };
        assert_eq!(SandboxSection::default().backend, want);
    }

    /// The fence's command, as `jail_args_pin_the_bsd_shape` pins the
    /// jail's: ulsh with nothing of the engine's environment but the
    /// transport, the shared root (no named box), a per-run directory in
    /// it, the script on stdin, the driver as an argument, the memory
    /// ceiling, and the run's directory removed after.
    #[test]
    fn userland_command_pins_the_fence_shape() {
        let b = UserlandBackend {
            ulsh: "/lib/libulsh.so".into(),
            memory_mb: 256,
        };
        let c = b.build_command("/tmp/.veles-work-1_2", |k| match k {
            "ULSH_RELAY" => Some("veles-userland-local".into()),
            "OPENAI_API_KEY" => Some("secret".into()),
            _ => None,
        });
        assert_eq!(c.get_program(), "/lib/libulsh.so");
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--exec",
                "/bin/sh",
                "-c",
                USERLAND_FENCE_RUNNER,
                "veles-fence",
                "/tmp/.veles-work-1_2",
                "262144",
                SANDBOX_DRIVER
            ]
        );
        let env: Vec<(String, String)> = c
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(
            env,
            [
                ("ULSH_CWD".to_string(), "/".to_string()),
                ("ULSH_RELAY".to_string(), "veles-userland-local".to_string()),
            ],
            "the rootfs (no ULSH_SANDBOX), and no secret"
        );
        for part in [
            r#"cat > "$w/user.tcl""#,
            r#"VELES_SANDBOX_SCRIPT="$w/user.tcl""#,
            r#"ulimit -v "$kb""#,
            r#"cd / && rm -rf "$w""#,
            "apk add tcl",
            "</dev/null",
        ] {
            assert!(USERLAND_FENCE_RUNNER.contains(part), "{part}");
        }
        // A run's directory carries the run tag: two runs never share
        // one (the collision `run_tag` exists for, in the jail).
        assert_ne!(run_tag(), {
            std::thread::sleep(std::time::Duration::from_millis(1));
            run_tag()
        });
    }

    #[test]
    fn nspawn_command_pins_the_isolation_flags() {
        let b = NspawnBackend {
            binary: "systemd-nspawn".into(),
            rootfs: "/var/lib/veles/rootfs".into(),
            memory_mb: 256,
            network: false,
            drop_caps: true,
            mounts: vec![
                crate::config::SandboxMount {
                    host: "/srv/projects".into(),
                    container: "/projects".into(),
                    mode: "ro".into(),
                },
                crate::config::SandboxMount {
                    host: "/srv/scratch".into(),
                    container: "/scratch".into(),
                    mode: "rw".into(),
                },
            ],
            extra_args: vec![],
            tclsh: "tclsh".into(),
        };
        let scratch = std::path::Path::new("/tmp/veles_sandbox_test");
        let joined = b
            .build_command(scratch)
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("--directory /var/lib/veles/rootfs"),
            "{joined}"
        );
        assert!(
            joined.contains("--private-network"),
            "benchmark posture: {joined}"
        );
        assert!(
            joined.contains("--rlimit=AS=268435456"),
            "memory cap maps to RLIMIT_AS: {joined}"
        );
        // LOWERCASE `all`: nspawn parses a capability NAME list and
        // rejects `ALL`. Shipped uppercase (podman's spelling) and
        // failed the first time the backend was actually run.
        assert!(joined.contains("--drop-capability=all"), "{joined}");
        // …and a PATH, or the backend's own "tclsh on PATH inside the
        // image" contract cannot be satisfied: nspawn gives the exec'd
        // command no usable PATH of its own, and the run dies with
        // `execv(tclsh) failed`. Both of these were found by running
        // the backend, not by reading it.
        assert!(
            joined.contains("--setenv=PATH=") && joined.contains("/usr/bin"),
            "the container needs a PATH that finds tclsh: {joined}"
        );
        assert!(
            !joined.contains("--drop-capability=ALL"),
            "podman's uppercase spelling is a hard parse error for nspawn: {joined}"
        );
        assert!(
            joined.contains("--bind /tmp/veles_sandbox_test:/work"),
            "scratch rides in: {joined}"
        );
        assert!(
            joined.contains("--bind-ro /srv/projects:/projects"),
            "ro mode: {joined}"
        );
        assert!(
            joined.contains("--bind /srv/scratch:/scratch"),
            "rw mode: {joined}"
        );
        assert!(
            joined.contains("--register=no"),
            "no machined chatter: {joined}"
        );
        assert!(
            joined.contains("VELES_SANDBOX_SCRIPT=/work/user.tcl"),
            "script path arrives by env: {joined}"
        );
        // .raw rootfs switches to --image; network on drops the flag.
        let b2 = NspawnBackend {
            binary: "systemd-nspawn".into(),
            rootfs: "/var/lib/veles/rootfs.raw".into(),
            memory_mb: 1,
            network: true,
            drop_caps: false,
            mounts: vec![],
            extra_args: vec![],
            tclsh: "tclsh".into(),
        };
        let joined2 = b2
            .build_command(scratch)
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined2.contains("--image /var/lib/veles/rootfs.raw"),
            "{joined2}"
        );
        assert!(!joined2.contains("--private-network"), "{joined2}");
        assert!(!joined2.contains("--drop-capability"), "{joined2}");
    }

    #[test]
    fn firecracker_config_pins_the_vm_shape() {
        let b = FirecrackerBackend {
            binary: "firecracker".into(),
            kernel: "/opt/veles/vmlinux".into(),
            rootfs: "/opt/veles/rootfs.ext4".into(),
            memory_mb: 128,
            extra_args: vec![],
        };
        let cfg = b.build_config(std::path::Path::new("/tmp/work.img"));
        assert_eq!(
            cfg["boot-source"]["kernel_image_path"],
            "/opt/veles/vmlinux"
        );
        let drives = cfg["drives"].as_array().expect("drives");
        assert_eq!(drives.len(), 2, "rootfs + work drive");
        assert_eq!(drives[0]["path_on_host"], "/opt/veles/rootfs.ext4");
        assert!(drives[0]["is_root_device"].as_bool().unwrap());
        // The rootfs is a SHARED artifact (a nix store path in the
        // supported setup). Writable, as it shipped 2026-09-03, either
        // fails to open or lets concurrent runs corrupt each other.
        assert!(
            drives[0]["is_read_only"].as_bool().unwrap(),
            "root drive is RO — the guest writes to tmpfs"
        );
        assert_eq!(drives[1]["path_on_host"], "/tmp/work.img");
        // …and the per-run work drive is writable, because the driver
        // points HOME and TMPDIR at it.
        assert!(
            !drives[1]["is_read_only"].as_bool().unwrap(),
            "work drive is RW — HOME/TMPDIR live there"
        );
        assert_eq!(cfg["machine-config"]["mem_size_mib"], 128);
        let cmd = b.build_command(std::path::Path::new("/tmp/vm.json"));
        let joined = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("--no-api --config-file /tmp/vm.json"),
            "{joined}"
        );
        // Resolution refuses firecracker without a kernel line.
        let err = backend_from(&SandboxSection {
            backend: "firecracker".into(),
            network: false,
            ..Default::default()
        })
        .err()
        .expect("firecracker without kernel_path must fail");
        assert!(err.to_string().contains("kernel_path"), "{err:#}");
        // …and refuses networking/mounts it cannot deliver.
        let err = backend_from(&SandboxSection {
            backend: "firecracker".into(),
            network: true,
            kernel_path: Some("/k".into()),
            ..Default::default()
        })
        .err()
        .expect("firecracker with network must fail");
        assert!(err.to_string().contains("network"), "{err:#}");
    }

    /// FreeBSD ships `tclsh8.6` and no `tclsh`, so the backend has to
    /// FIND its interpreter in the jail root rather than assume a name.
    /// Before this, `veles doctor --sandbox` on FreeBSD 15 died with
    ///   jail: exec env … tclsh …/driver.tcl: No such file or directory
    /// against a jail root that had a perfectly good tclsh in it.
    #[test]
    fn the_jail_finds_the_interpreter_freebsd_actually_installs() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut b = JailBackend {
            binary: "jail".into(),
            root: root.path().to_string_lossy().to_string(),
            network: false,
            mounts: vec![],
            extra_args: vec![],
            tclsh: "tclsh".into(),
        };

        // Nothing installed: say what was looked for, and where — not
        // "No such file or directory" about a whole command line.
        let err = b.resolve_tclsh().expect_err("an empty root has no tclsh");
        let msg = err.to_string();
        assert!(msg.contains("no tclsh inside the jail root"), "{msg}");
        assert!(msg.contains("tclsh8.6"), "the search list is named: {msg}");

        // What `pkg -r <root> install -y tcl86` actually leaves behind.
        let bin = root.path().join("usr/local/bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        std::fs::write(bin.join("tclsh8.6"), b"#!/bin/sh\n").expect("write");
        assert_eq!(
            b.resolve_tclsh().expect("found"),
            "/usr/local/bin/tclsh8.6",
            "the versioned name is the one FreeBSD installs"
        );

        // An absolute configured interpreter wins when it is real…
        std::fs::write(bin.join("tclsh"), b"#!/bin/sh\n").expect("write");
        b.tclsh = "/usr/local/bin/tclsh".into();
        assert_eq!(b.resolve_tclsh().expect("found"), "/usr/local/bin/tclsh");

        // …and is not taken on faith when it is not.
        b.tclsh = "/opt/nowhere/tclsh".into();
        assert_eq!(
            b.resolve_tclsh().expect("falls back to what is there"),
            "/usr/local/bin/tclsh8.6"
        );
    }

    #[test]
    fn jail_args_pin_the_bsd_shape() {
        let b = JailBackend {
            binary: "jail".into(),
            root: "/jails/veles".into(),
            network: false,
            mounts: vec![],
            extra_args: vec![],
            tclsh: "/usr/local/bin/tclsh86".into(),
        };
        let args = b.build_args("veles_sb_test", ".veles-work-7_99", &b.tclsh);
        let joined = args.join(" ");
        assert!(joined.contains("path=/jails/veles"), "{joined}");
        assert!(joined.contains("name=veles_sb_test"), "{joined}");
        // `ip4=disable`, with an EQUALS: jail(8) takes a value here, and
        // `ip4.disable` is not a parameter it knows. This assertion
        // pinned the dotted spelling for as long as the backend had
        // never run — the argv was checked against itself.
        assert!(
            joined.contains("ip4=disable"),
            "benchmark posture: {joined}"
        );
        assert!(joined.contains("ip6=disable"), "{joined}");
        // Every path the guest is told about names the PER-RUN work
        // directory. It was the literal `/work` until 2026-09-04, which
        // is the collision `run_tag` exists to prevent: the jail root is
        // shared, so two concurrent runs nullfs-mounted DIFFERENT
        // scratch dirs onto the same `<root>/work` and the second hid
        // the first's script.
        let w = ".veles-work-7_99";
        // ONE ARGV ELEMENT PER WORD, and `command=` last. jail(8) execs
        // the `command=` value and passes the rest as its arguments; a
        // single packed string is a filename with spaces in it, and the
        // fence dies with "No such file or directory" naming the whole
        // line. This test pinned the packed shape until the fence was
        // first run on FreeBSD, 2026-09-04.
        let tail = &args[args.len() - 6..];
        assert_eq!(
            tail,
            [
                "command=/usr/bin/env".to_string(),
                format!("VELES_SANDBOX_SCRIPT=/{w}/user.tcl"),
                format!("HOME=/{w}"),
                format!("TMPDIR=/{w}"),
                "/usr/local/bin/tclsh86".to_string(),
                format!("/{w}/driver.tcl"),
            ],
            "{joined}"
        );
        assert!(
            !joined.contains("command=env "),
            "a packed command= string does not exec: {joined}"
        );
        assert!(
            !joined.contains("=/work/"),
            "no shared /work may survive anywhere in the argv: {joined}"
        );
        // Network on drops the ip disables.
        let b2 = JailBackend {
            binary: "jail".into(),
            root: "/jails/veles".into(),
            network: true,
            mounts: vec![],
            extra_args: vec![],
            tclsh: "tclsh".into(),
        };
        let joined2 = b2.build_args("n", ".veles-work-1_1", &b2.tclsh).join(" ");
        assert!(!joined2.contains("ip4=disable"), "{joined2}");
        // Two runs never agree on a work directory — the whole point.
        assert_ne!(run_tag(), run_tag(), "run tags must be unique");
    }

    /// The firecracker console → (stdout, exit code) split.
    ///
    /// The VMM's exit status describes the VMM. Before 2026-09-04 it was
    /// reported as the sandbox's, so `success()` said true for a script
    /// that failed inside a VM that shut down cleanly. The guest init
    /// prints `VELES_SANDBOX_EXIT:<n>` instead, and this is the parser.
    #[test]
    fn the_console_sentinel_carries_the_scripts_own_status() {
        // Clean run: output, then the sentinel.
        let (out, code) = split_console("42\nVELES_SANDBOX_EXIT:0\n");
        assert_eq!(out, "42");
        assert_eq!(code, 0);

        // The kernel shares the serial line with the script, so a real
        // console opens with boot output. Everything before BEGIN is
        // the kernel's and must not be handed back as the script's.
        let (out, code) = split_console(
            "[    0.00] Linux version 7.2.2\n\
             [    0.31] virtio_blk virtio1: [vdb] 8192 512-byte logical blocks\n\
             VELES_SANDBOX_BEGIN\n\
             42\n\
             VELES_SANDBOX_EXIT:0\n",
        );
        assert_eq!(out, "42", "boot output must not reach the caller");
        assert_eq!(code, 0);

        // An older rootfs prints no BEGIN. Keep the whole stream —
        // losing a script's output is worse than including some noise.
        let (out, _) = split_console("[ 0.00] Linux\nhello\nVELES_SANDBOX_EXIT:0\n");
        assert!(out.contains("hello"), "no BEGIN: nothing is dropped: {out}");

        // A failing script keeps its status.
        let (out, code) = split_console("partial\nVELES_SANDBOX_EXIT:1\n");
        assert_eq!(out, "partial");
        assert_eq!(code, 1);

        // No sentinel = the guest never finished (panic, bad rootfs,
        // hang). NEVER success, and the console is kept as evidence.
        let (out, code) = split_console("[    0.42] Kernel panic - not syncing\n");
        assert_eq!(code, -1, "a missing sentinel is a failure, not a zero");
        assert!(out.contains("Kernel panic"), "the evidence survives: {out}");

        // A script that prints the sentinel text itself must not be
        // able to forge its own exit code — the init's copy is last.
        let (out, code) = split_console("VELES_SANDBOX_EXIT:0\nVELES_SANDBOX_EXIT:3\n");
        assert_eq!(code, 3, "the LAST sentinel wins");
        assert_eq!(out, "VELES_SANDBOX_EXIT:0");

        // Sentinel present but malformed: broken init, so a failure.
        let (_, code) = split_console("hi\nVELES_SANDBOX_EXIT:\n");
        assert_eq!(code, -1);
    }

    #[test]
    fn section_defaults_match_the_docs() {
        let s = SandboxSection::default();
        assert_eq!(s.backend, "native");
        assert_eq!(s.memory_mb, 256);
        assert!(s.network, "network is ON by default (dev posture)");
        assert_eq!(s.capabilities, "drop", "caps dropped by default");
        assert!(s.mounts.is_empty(), "host access is default DENY");
    }

    #[test]
    fn section_parses_from_toml() {
        let s: SandboxSection = toml::from_str(
            r#"
             backend = "podman"
             image = "localhost/my-tcl:1"
             memory_mb = 128
             network = false
             capabilities = "default"
             extra_args = ["--cpus", "1"]
             [[mounts]]
             host = "/srv/projects"
             container = "/projects"
             mode = "rw"
         "#,
        )
        .unwrap();
        assert_eq!(s.backend, "podman");
        assert_eq!(s.image, "localhost/my-tcl:1");
        assert_eq!(s.memory_mb, 128);
        assert!(!s.network, "benchmark posture parses");
        assert_eq!(s.capabilities, "default");
        assert_eq!(s.extra_args, vec!["--cpus", "1"]);
        assert_eq!(s.mounts.len(), 1);
        assert_eq!(s.mounts[0].host, "/srv/projects");
        assert_eq!(s.mounts[0].container, "/projects");
        assert_eq!(s.mounts[0].mode, "rw");
    }

    #[test]
    fn host_alias_resolves_to_the_native_backend() {
        let s = SandboxSection {
            backend: "host".into(),
            ..Default::default()
        };
        let b = backend_from(&s).expect("host alias resolves");
        assert_eq!(b.name(), "native", "host is the native backend spelled out");
    }

    /// The shape that matters in a REAL config.toml: `[sandbox]` table +
    /// `[[sandbox.mounts]]` array-of-tables, parsed at VelesConfig level.
    /// podman mounts name unix host paths; the backend is unix-only.
    #[cfg(unix)]
    #[test]
    fn mounts_parse_at_the_full_config_level() {
        let cfg: crate::config::VelesConfig = toml::from_str(
            r#"
            [sandbox]
            backend = "podman"
            network = false
            capabilities = "default"
            [[sandbox.mounts]]
            host = "/srv/projects"
            container = "/projects"
            mode = "rw"
            [[sandbox.mounts]]
            host = "/etc/veles"
            container = "/cfg"
            mode = "ro"
        "#,
        )
        .expect("full-config parse");
        assert_eq!(cfg.sandbox.mounts.len(), 2, "both mounts land");
        assert_eq!(cfg.sandbox.mounts[0].mode, "rw");
        assert_eq!(cfg.sandbox.mounts[1].mode, "ro");
        assert!(!cfg.sandbox.network, "benchmark posture parses");
        // …and the whole thing still resolves to a backend.
        let b = backend_from(&cfg.sandbox).expect("mounts validate");
        assert_eq!(b.name(), "podman");
    }

    /// podman mounts name unix host paths; the backend is unix-only.
    #[cfg(unix)]
    #[test]
    fn bad_capabilities_and_mounts_fail_loudly() {
        let err = backend_from(&SandboxSection {
            backend: "podman".into(),
            capabilities: "yolo".into(),
            ..Default::default()
        })
        .err()
        .expect("unknown capabilities must fail");
        assert!(err.to_string().contains("capabilities"), "{err:#}");

        let err = backend_from(&SandboxSection {
            backend: "podman".into(),
            mounts: vec![crate::config::SandboxMount {
                host: "/data".into(),
                container: "/data".into(),
                mode: "wr".into(),
            }],
            ..Default::default()
        })
        .err()
        .expect("a bad mount mode must fail");
        assert!(err.to_string().contains("ro"), "{err:#}");

        let err = backend_from(&SandboxSection {
            backend: "podman".into(),
            mounts: vec![crate::config::SandboxMount {
                host: "relative/path".into(),
                container: "/data".into(),
                mode: "ro".into(),
            }],
            ..Default::default()
        })
        .err()
        .expect("a relative host mount must fail");
        assert!(err.to_string().contains("absolute"), "{err:#}");
    }

    /// The native fence's memory cap has to BITE, not just be
    /// documented — and this is the test that lets its mechanism
    /// change safely. The cap used to be `setrlimit` inside a
    /// `pre_exec` closure, which deadlocks against libtcl's atfork
    /// handler (see `mem_limited_command`, and
    /// tests/sandbox_fork_safety.rs). Moving it into a shell that
    /// execs tclsh is only a fix if the limit still stops an
    /// allocation past it. Nothing checked that before; the constant
    /// had been asserted about, never exercised.
    #[tokio::test]
    async fn the_native_fence_refuses_an_allocation_past_its_cap() {
        // Comfortably over SANDBOX_MEM_LIMIT_BYTES (256MB), and cheap
        // to refuse: an uncapped tclsh answers this in well under a
        // second, so a pass here is the cap working, not a timeout.
        let out = drive("puts [string length [string repeat x 400000000]]", 60).await;
        assert!(
            !out.stdout.contains("400000000"),
            "a 400MB allocation went through under a 256MB cap: {out:?}"
        );
        assert!(
            !out.success(),
            "the run reported success despite being over the cap: {out:?}"
        );
    }
}
