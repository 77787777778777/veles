//! execute_code tool — run Tcl scripts with multi-tool orchestration.
//!
//! #28 in the feature matrix. Writes the code to a temp file, runs it
//! with tclsh (the wrapped binary's PATH always carries it), returns
//! stdout+stderr. The harness has access to tool-bridge procs
//! (terminal, read_file, write_file, patch, search_files) that call back
//! into the veles binary's `tool` subcommand — so the model can write
//! scripts that chain multiple tool calls with branching logic.
//!
//! Tcl, not Python: user directive 2026-08-21 (no python tooling on this
//! machine; Tcl is the veles native scripting language anyway). The
//! harness contract mirrors hermes' execute_code: print your final
//! result to stdout, 300s cap, 50KB output cap.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::Tool;

pub struct ExecuteCodeTool {
    /// Whose session this runs for — the F3 env strip is per-principal
    /// (non-owners do not inherit secret-bearing env). The trusted
    /// path is already Admin+ by the agent's sandbox-forcing gate; this
    /// closes the remaining Admin-not-Owner case.
    pub session_id: String,
}

#[async_trait]
impl Tool for ExecuteCodeTool {
    fn name(&self) -> &str {
        "execute_code"
    }
    fn description(&self) -> &str {
        "Run a Tcl script in tclsh. Use for multi-step tool orchestration (3+ calls with logic between them), filtering large outputs, or loops. Tool-bridge procs: terminal, read_file, write_file, patch, search_files. puts your final result to stdout. Returns stdout + stderr."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "Tcl code to execute"},
                "timeout": {"type": "integer", "description": "Max seconds (default 120)"}
            },
            "required": ["code"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let code = args["code"].as_str().unwrap_or_default().to_string();
        if code.is_empty() {
            return "error: code is required".into();
        }
        let timeout = args["timeout"].as_u64().unwrap_or(120).clamp(1, 300);
        // WHERE this runs is the framework's decision, not the model's
        // (`user_envs::zone_for`), and for this tool the location and
        // the FENCE are one axis: a sandboxed run gets the pure-compute
        // safe interp, never the harness. A connector principal is
        // fenced; the local surfaces are on the host; `host: true` is
        // the only way out and only for someone allowed to take it.
        let zone = crate::user_envs::zone_for(
            Some(&self.session_id),
            crate::user_envs::ZoneArg::from_args(&args),
        );

        // Write the code to a temp file so errors carry line numbers.
        // NamedTempFile creates its randomized name atomically, and its
        // guard deletes only this call's file. A wall-clock timestamp was
        // neither unique nor safe: under Nix's frozen build clock, parallel
        // calls wrote the same path, executed each other's code, and deleted
        // the script out from under a still-running tclsh.
        let tmpdir = std::env::temp_dir();

        // ── sandboxed: pure-compute safe interp (slopdrop parity) ──
        //
        // No harness preamble, no tool bridge, no secrets: the caller
        // tier this exists for cannot be trusted with any of them, and
        // a script with no procs to call cannot call anything.
        //
        // The fence is the caller's own persistent env container, so
        // scripts keep state in /work across calls. There used to be a
        // per-run `sandbox::execute` fallback here "for cron jobs and
        // the bare CLI" — dead since the zone became the authority,
        // because every branch of `zone_for` that is not `Host` names a
        // container.
        if let crate::user_envs::Zone::Sandbox { key, name } = &zone {
            let timeout = timeout.min(crate::sandbox::SANDBOX_DEFAULT_TIMEOUT_SECS);
            return match crate::user_envs::exec_tcl(key, &code, timeout).await {
                Ok(out) if out.timed_out => {
                    crate::user_envs::annotate(name, format!("error: timeout after {timeout}s"))
                }
                Ok(out) => crate::user_envs::annotate(
                    name,
                    format_pieces(&out.stdout, &out.stderr, out.exit_code),
                ),
                Err(e) => crate::user_envs::annotate(name, format!("error: user env: {e:#}")),
            };
        }

        // ── trusted: full harness (unchanged path) ──
        // S4 secrets: same contract as the terminal tool — $NAME
        // references become env vars for the tclsh process (in
        // Tcl, `$env(NAME)` reads them), and every known value is
        // scrubbed from the output. Without this a Tcl script
        // echoing a secret would leak it into model context. Bare
        // `$NAME` tokens are rewritten to the Tcl env-array form
        // so shell habits work in Tcl source too.
        // Policy and principal decide, as for `terminal` (see
        // `secrets::child_env`).
        let child = crate::secrets::child_env(&self.session_id, &code);
        let refs = child.inject.clone();
        let names: Vec<String> = refs.iter().map(|(k, _)| k.clone()).collect();
        let code_expanded = crate::secrets::expand_for_tcl(&code, &names);
        let wrapped = format!("{HARNESS_PREAMBLE}\n{code_expanded}\n");
        let mut script = match tempfile::Builder::new()
            .prefix("veles_exec_")
            .suffix(".tcl")
            .tempfile_in(tmpdir)
        {
            Ok(script) => script,
            Err(_) => return "error: failed to create temp script".into(),
        };
        {
            use std::io::Write;
            if script
                .as_file_mut()
                .write_all(wrapped.as_bytes())
                .and_then(|()| script.as_file_mut().flush())
                .is_err()
            {
                return "error: failed to write temp script".into();
            }
        }
        let script_path = script.path();

        // The harness procs shell out to our own binary's `tool`
        // subcommand — resolve it here so PATH is irrelevant (dev builds
        // run from ./result/bin, installed builds from the profile).
        let self_bin = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "veles".to_string());

        let mut command = tokio::process::Command::new(tclsh_path());
        command
            .arg(script_path)
            .env("VELES_BIN", &self_bin)
            // A timed-out run must DIE, not be abandoned: the timeout
            // below cancels by dropping the output future, and a dropped
            // Child only kills its process with this flag. Without it the
            // tclsh spins on forever at 100% CPU (same leak the sandbox
            // native backend shipped — see sandbox.rs).
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // F3: same per-principal inherited-env strip as `terminal` —
        // `puts $env(IRC_SASL_PASSWORD)` from a compromised turn must
        // find nothing. The tool-bridge subprocesses (VELES_BIN) inherit
        // this already-stripped env, so they are covered transitively.
        for k in &child.strip {
            command.env_remove(k);
        }
        if !refs.is_empty() {
            for (k, v) in &refs {
                command.env(k, v);
            }
            tracing::info!(
                "secrets: injected {} value(s) into execute_code env",
                refs.len()
            );
        }

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(timeout), command.output()).await;

        // The NamedTempFile guard removes this call's script on every exit
        // path, including timeout/cancellation; never unlink a shared name.
        let body = match result {
            Ok(Ok(out)) => format_tcl_output(out),
            Ok(Err(e)) => format!("error launching tclsh: {e}"),
            Err(_) => "error: timeout".into(),
        };
        match child.note() {
            Some(n) => format!("{body}\n{n}"),
            None => body,
        }
    }
}

/// Shared output formatting for both execution paths: stdout first,
/// stderr labelled, exit code reported, 50KB cap, secret scrub.
fn format_tcl_output(out: std::process::Output) -> String {
    let exit = out.status.code().unwrap_or(-1);
    format_pieces(
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
        exit,
    )
}

/// The formatting half without a `std::process::Output` — this is what
/// the sandbox backends' `SandboxOutput` feeds through (their stdout/
/// stderr are already decoded strings).
fn format_pieces(stdout: &str, stderr: &str, exit: i32) -> String {
    let mut s = String::new();
    if !stdout.is_empty() {
        s.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str("[stderr]\n");
        s.push_str(stderr);
    }
    if exit != 0 {
        s.push_str(&format!("\n[exit code: {exit}]"));
    }
    // Cap output to 50KB
    if s.len() > 50_000 {
        s.truncate(50_000);
        s.push_str("\n[truncated at 50KB]");
    }
    // S4: scrub known secret values before the model sees them.
    crate::secrets::scrub(&s, &crate::secrets::scrubbable())
}

/// Locate tclsh. Order: VELES_TCLSH env (explicit) → PATH (the flake
/// wrapper prefixes it for installed builds) → derive from TCL_LIBRARY
/// (set by the wrapper, and by the dev shell): <tcl-lib>/../bin/tclsh.
/// NixOS has no system tclsh on PATH, so a bare cargo-built binary
/// relies on the last two.
pub fn tclsh_path() -> String {
    if let Ok(p) = std::env::var("VELES_TCLSH") {
        if !p.is_empty() {
            return p;
        }
    }
    if std::env::var_os("PATH").is_some() {
        for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            let cand = dir.join("tclsh");
            if cand.is_file() {
                return cand.to_string_lossy().to_string();
            }
        }
    }
    if let Ok(lib) = std::env::var("TCL_LIBRARY") {
        // /nix/store/<hash>-tcl-8.6/lib/tcl8.6 → /nix/store/<hash>-tcl-8.6/bin/tclsh
        let p = std::path::PathBuf::from(lib);
        if let Some(store) = p.parent().and_then(|libdir| libdir.parent()) {
            let cand = store.join("bin").join("tclsh");
            if cand.is_file() {
                return cand.to_string_lossy().to_string();
            }
        }
    }
    "tclsh".to_string()
}

/// The harness preamble defines the tool-bridge procs. Each shells out to
/// `veles tool --name <tool> --args <json>` (the VELES_BIN env var),
/// which routes to the real tool implementations in-process there. This
/// keeps the harness zero-dependency (no IPC channel or shared state)
/// while giving the model the same orchestration power as hermes'
/// execute_code. Pure Tcl 8.6: hand-rolled JSON string escaping, no
/// tcllib requirement (the launched tclsh may not have TCLLIBPATH).
const HARNESS_PREAMBLE: &str = r#"# veles execute_code harness — auto-generated Tcl wrapper.
# Core Tcl 8.6 only; puts your final result to stdout.

# Minimal JSON string escaper (the harness builds all JSON itself).
proc _jesc {s} {
    set s [string map [list "\\" "\\\\" "\"" "\\\"" \n "\\n" \r "\\r" \t "\\t" \b "\\b" \f "\\f"] $s]
    # Remaining control characters -> \u00XX
    set out ""
    foreach ch [split $s ""] {
        scan $ch %c code
        if {$code < 32} {
            append out [format "\\u%04x" $code]
        } else {
            append out $ch
        }
    }
    return $out
}

# Call a veles tool via the CLI bridge; returns its string output.
proc _veles_tool {name args_json} {
    global env
    set self "veles"
    if {[info exists env(VELES_BIN)]} { set self $env(VELES_BIN) }
    if {[catch {exec $self tool --name $name --args $args_json} out]} {
        # exec raises on non-zero exit; the error message carries output
        return "error: $out"
    }
    return $out
}

# ── tool-bridge procs ────────────────────────────────────────────────
proc terminal {command {timeout 60}} {
    _veles_tool terminal "\{\"command\": \"[_jesc $command]\", \"timeout\": $timeout\}"
}

proc read_file {path {offset 1} {limit 2000}} {
    _veles_tool read_file "\{\"path\": \"[_jesc $path]\", \"offset\": $offset, \"limit\": $limit\}"
}

proc write_file {path content} {
    _veles_tool write_file "\{\"path\": \"[_jesc $path]\", \"content\": \"[_jesc $content]\"\}"
}

proc patch {path old_string new_string {replace_all false}} {
    _veles_tool patch "\{\"path\": \"[_jesc $path]\", \"old_string\": \"[_jesc $old_string]\", \"new_string\": \"[_jesc $new_string]\", \"replace_all\": $replace_all\}"
}

proc search_files {pattern {target content} {path .} {file_glob ""} {limit 50}} {
    if {$file_glob eq ""} {
        _veles_tool search_files "\{\"pattern\": \"[_jesc $pattern]\", \"target\": \"[_jesc $target]\", \"path\": \"[_jesc $path]\", \"limit\": $limit\}"
    } else {
        _veles_tool search_files "\{\"pattern\": \"[_jesc $pattern]\", \"target\": \"[_jesc $target]\", \"path\": \"[_jesc $path]\", \"file_glob\": \"[_jesc $file_glob]\", \"limit\": $limit\}"
    }
}

# ── helpers (hermes execute_code compat) ─────────────────────────────
# Shell-quote a value (single-quote wrap, safe for bash).
proc shell_quote {s} {
    return "'[string map {' "'\\''"} $s]'"
}

# Retry a script body with exponential backoff.
proc retry {body {max_attempts 3} {delay_ms 2000}} {
    for {set i 0} {$i < $max_attempts} {incr i} {
        if {![catch {uplevel 1 $body} result]} {
            return $result
        }
        if {$i == $max_attempts - 1} {
            error $result
        }
        after [expr {$delay_ms * (2 ** $i)}]
    }
}

# Tolerant JSON parse: returns a Tcl value when tcllib's json is
# available, otherwise the raw text (scripts can string-match on it).
proc json_parse {text} {
    if {![catch {package require json}]} {
        if {![catch {json::json2dict $text} d]} {
            return $d
        }
    }
    return $text
}

# ── user code starts here ─────────────────────────────────────────────
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call_with(code: &str, timeout: Option<u64>) -> impl std::future::Future<Output = String> {
        let mut args = json!({ "code": code });
        if let Some(t) = timeout {
            args["timeout"] = json!(t);
        }
        async move {
            ExecuteCodeTool {
                session_id: "executecode-test".into(),
            }
            .call(args)
            .await
        }
    }

    #[tokio::test]
    async fn empty_code_is_refused_not_executed() {
        let t = ExecuteCodeTool {
            session_id: "executecode-test".into(),
        };
        let out = t.call(json!({})).await;
        assert_eq!(out, "error: code is required");
        let out = t.call(json!({ "code": "" })).await;
        assert_eq!(out, "error: code is required");
    }

    #[tokio::test]
    async fn stdout_returns_and_exit_code_is_reported() {
        let out = call_with("puts hello-from-tcl", None).await;
        assert!(
            out.contains("hello-from-tcl"),
            "stdout must reach the model: {out}"
        );
        // A failing script carries its exit code — a silent zero would
        // make a half-run orchestration look successful.
        let out = call_with("exit 7", None).await;
        assert!(out.contains("[exit code: 7]"), "{out}");
    }

    #[tokio::test]
    async fn stderr_is_labelled_not_merged_into_stdout() {
        let out = call_with(
            "puts \"[clock format 0]\" ; puts stderr \"the error text\"",
            None,
        )
        .await;
        assert!(
            out.contains("[stderr]") && out.contains("the error text"),
            "stderr arrives labelled: {out}"
        );
    }

    #[tokio::test]
    async fn oversized_output_is_truncated_at_50kb() {
        // 60_000 chars of output must come back capped, with the marker,
        // never as the full wall.
        let out = call_with(
            &format!("puts {{}}; puts [string repeat x {}]", 60_000),
            None,
        )
        .await;
        assert!(
            out.len() < 60_000,
            "output was not truncated: {}",
            out.len()
        );
        assert!(out.contains("[truncated at 50KB]"), "{out}");
    }

    #[tokio::test]
    async fn timeout_clamps_to_the_one_second_floor() {
        // A model asking for timeout=0 (or negative via JSON) must get a
        // working script run, not an instant timeout — clamp(1,300) is
        // the contract. Run something trivially short and expect success.
        let out = call_with("puts quick", Some(1)).await;
        assert!(out.contains("quick"), "clamped timeout still runs: {out}");
    }

    #[tokio::test]
    async fn a_timed_out_run_is_killed_not_abandoned() {
        // Regression: this trusted path shipped WITHOUT kill_on_drop, so a
        // busy loop that out-ran its timeout was abandoned — the tclsh
        // spun on at 100% CPU forever (the sandbox native backend shipped
        // the same shape in d17a8ed; see sandbox.rs). The timeout reply is
        // half the contract; the other half is that nothing survives.
        let out = call_with("while 1 {}", Some(2)).await;
        assert!(out.contains("error: timeout"), "{out}");
        // SIGKILL delivery + reap is not instantaneous, so poll with a
        // deadline; a leaked spinner would never leave, so any deadline
        // miss is the regression itself.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let alive = std::fs::read_dir("/proc")
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
                .any(|raw| String::from_utf8_lossy(&raw).contains("veles_exec_"));
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a timed-out execute_code process SURVIVED — it would spin at 100% CPU forever"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn tcl_errors_surface_in_stderr_block() {
        let out = call_with("expr {1 +}", None).await;
        assert!(
            out.contains("[stderr]") || out.contains("[exit code:"),
            "a syntax error is reported, never silently empty: {out}"
        );
    }

    /// Each concurrent run gets its own atomically created script. In
    /// addition to returning its own label, each script reports its own
    /// filename: equal timestamp-based names would make this fail even if
    /// a scheduler happened to serialize the actual tclsh processes.
    #[tokio::test]
    async fn concurrent_runs_have_distinct_scripts_and_results() {
        let jobs: Vec<_> = (0..12)
            .map(|n| {
                crate::rt::spawn(call_with(
                    &format!("puts result-{n}; puts [file tail [info script]]"),
                    None,
                ))
            })
            .collect();
        let mut scripts = std::collections::HashSet::new();
        for (n, job) in jobs.into_iter().enumerate() {
            let out = job.await.unwrap();
            // Tcl writes the platform newline: CRLF on Windows, LF on
            // Unix. The output bytes are otherwise the same contract.
            // Keep the line boundary so `result-1-extra` is not accepted,
            // without rejecting the native Tcl output channel.
            assert!(
                out.lines().any(|line| line == format!("result-{n}")),
                "wrong output: {out}"
            );
            let script = out
                .lines()
                .find(|line| line.starts_with("veles_exec_") && line.ends_with(".tcl"))
                .unwrap_or_else(|| panic!("script name missing: {out}"));
            assert!(scripts.insert(script.to_owned()), "shared path: {out}");
        }
        assert_eq!(scripts.len(), 12);
    }

    /// A fenced run routes into the CALLER'S env container
    /// (user_envs) — the pure-compute fence runs INSIDE it, so scripts
    /// keep state in /work. Under `--lib` the env is offline (a unit
    /// test must not touch real podman); the annotation and the loud
    /// error are the pinned part, and the fence semantics themselves
    /// (pure compute, exec dead) stay pinned by sandbox.rs's backend
    /// tests + the IRC live suite.
    ///
    /// Both spellings are pinned: `host: false` is the flag since
    /// 2026-09-09 and `sandbox: true` is the pre-inversion one, which
    /// still parses so an old skill asking for the fence keeps getting
    /// one.
    #[tokio::test]
    async fn a_fenced_run_routes_through_the_configured_backend() {
        let t = ExecuteCodeTool {
            session_id: "executecode-test".into(),
        };
        for args in [
            json!({"code": "puts [expr {6*7}]", "host": false}),
            json!({"code": "puts [expr {6*7}]", "sandbox": true}),
        ] {
            let out = t.call(args.clone()).await;
            assert!(
                out.contains("[env: sandbox"),
                "{args} must route into the env: {out}"
            );
            assert!(
                out.contains("offline"),
                "under --lib the env refuses loudly instead of faking: {out}"
            );
            assert!(
                !out.contains("42"),
                "nothing computes outside the real fence: {out}"
            );
        }
    }
}
