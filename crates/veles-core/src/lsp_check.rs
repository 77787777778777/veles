//! One-shot Tcl LSP checks from chat: `/lint` and `/fmt`.
//!
//! The plan item (2026-09-12, §2): "A one-shot `lsp_check.rs`
//! (initialize → didOpen → collect publishDiagnostics → kill) reusing
//! `lsp_proxy::frame`/`take_frame`. It must spawn its OWN short-lived
//! child: `lsp-lsp::start` reuses a server only when `(cmd, args,
//! root)` all match and otherwise REPLACES it, so borrowing it would
//! evict the desktop's live Dev-pane session."
//! `veles-desktop/scripts/tcl-lsp-check.tcl` is the working reference,
//! including the trap that `tcl-lsp` logs to stderr and a merged
//! stream breaks `Content-Length` framing — hence `stderr(Null)` here.
//!
//! NO-PYTHON/NO-LSP POLICY (veles-runtime-policy): the ban on LSP is
//! about the AGENT's editing loop (headless clangd, token waste,
//! rate-limit amplifier). This is the OPERATOR asking, by name, for a
//! lint of a file they chose — the same standing the tcl-lsp Dev pane
//! has on this box. It runs tcl-lsp only, never a code LSP.
//!
//! The deadline is a READ deadline, not a wall-clock check between
//! messages (PENDING residual 1, 2026-09-18: the old loop consulted
//! `Instant::now() < deadline` only after `read_msg` returned, and
//! `read_msg` blocks — a server that answers nothing outran the
//! timeout entirely and `a_real_file_lints_end_to_end` hung the gate
//! for 20 minutes). So stdout is drained by a dedicated reader thread
//! that hands complete frames over a channel, and every wait on that
//! channel is `recv_deadline`. The thread can only block inside the
//! kernel `read`; killing the child (guaranteed by `Drop`) gives it
//! EOF, so it exits on every path, including the timeout path.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::Instant;

use serde_json::{json, Value};

/// A server that never answers within the deadline is a FAILURE, not a
/// clean lint — returning an empty `Ok` here would let a hung `tcl-lsp`
/// report every file as lint-clean.
fn timed_out(secs: u64) -> String {
    format!("tcl-lsp did not answer within {secs}s")
}

fn deadline_after(secs: u64) -> Instant {
    Instant::now() + std::time::Duration::from_secs(secs)
}

/// `lint`: one file's diagnostics, `file:line:col SEVERITY message`
/// per line. None when the server could not be started or never
/// answered.
pub fn lint_file(path: &str, timeout_secs: u64) -> Result<Vec<String>, String> {
    lint_file_cmd("tcl-lsp", path, timeout_secs)
}

/// The real body, with the server binary as a parameter so the tests
/// can drive a deliberately mute server without touching a global
/// PATH (which would race the other tests in the same process).
fn lint_file_cmd(cmd: &str, path: &str, timeout_secs: u64) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let root = std::path::Path::new(path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let session = Session::spawn(cmd)?;
    let deadline = deadline_after(timeout_secs);
    let uri = format!(
        "file://{}",
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.into())
            .display()
    );

    session.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"processId": null,
                   "rootUri": format!("file://{}", root.display()),
                   "capabilities": {}}
    }));
    // Wait for the initialize response before didOpen — the reference
    // driver sequences the same way.
    session.wait_id(1, timeout_secs, deadline)?;
    session.send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
    session.send(&json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": {"textDocument": {"uri": uri, "languageId": "tcl",
                                    "version": 1, "text": text}}
    }));

    let mut out = Vec::new();
    while Instant::now() < deadline {
        let Some(msg) = session.next_msg(timeout_secs, deadline)? else {
            break;
        };
        if msg.get("method").and_then(Value::as_str) != Some("textDocument/publishDiagnostics") {
            continue;
        }
        let diags = msg["params"]["diagnostics"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for d in &diags {
            let line = d["range"]["start"]["line"].as_u64().unwrap_or(0) + 1;
            let col = d["range"]["start"]["character"].as_u64().unwrap_or(0);
            let sev = match d["severity"].as_u64().unwrap_or(1) {
                1 => "ERROR",
                2 => "WARNING",
                3 => "INFO",
                _ => "HINT",
            };
            out.push(format!(
                "{path}:{line}:{col} {sev} {}",
                d["message"].as_str().unwrap_or("?")
            ));
        }
        break;
    }
    // `Drop` kills the child, which EOFs the reader thread.
    Ok(out)
}

/// `fmt`: one file, formatted through `textDocument/formatting`.
/// Returns (formatted_text, changed) or an error string. The file on
/// disk is NOT touched — the caller (the command arm) writes it back
/// after showing a diff summary, because silently rewriting a file is
/// the hermes bargain this round exists to stop.
pub fn fmt_file(path: &str, timeout_secs: u64) -> Result<Option<String>, String> {
    fmt_file_cmd("tcl-lsp", path, timeout_secs)
}

fn fmt_file_cmd(cmd: &str, path: &str, timeout_secs: u64) -> Result<Option<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let root = std::path::Path::new(path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let session = Session::spawn(cmd)?;
    let deadline = deadline_after(timeout_secs);
    let uri = format!(
        "file://{}",
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.into())
            .display()
    );

    session.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"processId": null,
                   "rootUri": format!("file://{}", root.display()),
                   "capabilities": {"textDocument": {"formatting": {}}}}
    }));
    session.wait_id(1, timeout_secs, deadline)?;
    session.send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
    session.send(&json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": {"textDocument": {"uri": uri, "languageId": "tcl",
                                    "version": 1, "text": text}}
    }));
    session.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "textDocument/formatting",
        "params": {"textDocument": {"uri": uri},
                   "options": {"tabSize": 4, "insertSpaces": true}}
    }));

    let mut result = None;
    while Instant::now() < deadline {
        let Some(msg) = session.next_msg(timeout_secs, deadline)? else {
            break;
        };
        if msg.get("id").and_then(Value::as_u64) != Some(2) {
            continue;
        }
        // The response is an array of TextEdits or null.
        let edits = msg["result"].as_array().cloned().unwrap_or_default();
        if edits.is_empty() {
            result = Some(None);
        } else {
            // Apply the edits (they are line-based ranges) — the honest
            // approach is to apply them against the ORIGINAL, ordered
            // by start descending so earlier offsets stay valid.
            let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
            let mut sorted = edits;
            sorted.sort_by_key(|e| {
                (
                    std::cmp::Reverse(e["range"]["start"]["line"].as_u64().unwrap_or(0)),
                    std::cmp::Reverse(e["range"]["start"]["character"].as_u64().unwrap_or(0)),
                )
            });
            let mut ok = true;
            for e in &sorted {
                let (sl, sc) = (
                    e["range"]["start"]["line"].as_u64().unwrap_or(0) as usize,
                    e["range"]["start"]["character"].as_u64().unwrap_or(0) as usize,
                );
                let (el, ec) = (
                    e["range"]["end"]["line"].as_u64().unwrap_or(0) as usize,
                    e["range"]["end"]["character"].as_u64().unwrap_or(0) as usize,
                );
                let new = e["newText"].as_str().unwrap_or_default();
                if sl >= lines.len() || el >= lines.len() {
                    ok = false;
                    break;
                }
                // Splice: replace [sl.sc, el.ec] with `new`.
                let line = lines[sl].clone();
                if sl == el {
                    let keep_head = line.chars().take(sc).collect::<String>();
                    let keep_tail = line.chars().skip(ec).collect::<String>();
                    lines[sl] = format!("{keep_head}{new}{keep_tail}");
                } else {
                    let keep_head = line.chars().take(sc).collect::<String>();
                    let tail = lines[el].chars().skip(ec).collect::<String>();
                    let joined = format!("{keep_head}{new}{tail}");
                    lines[sl] = joined;
                    lines.drain(sl + 1..=el);
                }
            }
            if ok {
                let joined = lines.join("\n");
                result = Some(if joined == text { None } else { Some(joined) });
            } else {
                result = Some(None);
            }
        }
        break;
    }
    result.ok_or_else(|| "tcl-lsp never answered the formatting request".into())
}

// ── the short-lived child ────────────────────────────────────────────

/// One message off the wire, or the pipe's end. `None` means the
/// server closed stdout (or broke framing beyond repair) — the old
/// code's EOF signal.
type WireMsg = Option<Value>;

/// A live `tcl-lsp` child plus the reader thread watching its stdout.
/// Killing happens in `Drop`, so EVERY exit path — including the
/// timeout error path — ends the child, which EOFs the pipe, which
/// ends the thread. Nothing can outlive a `Session`.
struct Session {
    child: Mutex<std::process::Child>,
    stdin: Mutex<std::process::ChildStdin>,
    rx: Receiver<WireMsg>,
    /// The child's private TMPDIR. tcl-lsp keeps `tcl-lsp-<pid>/` and
    /// `tcl-lsp-db-<pid>/` (two sqlite files) there and cannot clean
    /// up when it is killed — which is how every Session ends. Removed
    /// in `Drop` after the child is reaped; before this, each lint left
    /// both behind in the caller's TMPDIR (78 pairs in /dev/shm).
    tmp: std::path::PathBuf,
}

impl Session {
    fn spawn(cmd: &str) -> Result<Session, String> {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = std::env::temp_dir().join(format!(
            "veles-lsp-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&tmp).map_err(|e| format!("lsp scratch dir: {e}"))?;
        // tcl-lsp logs to stderr; a merged stream breaks Content-Length
        // framing (the reference driver's trap, learned the hard way).
        let spawned = Command::new(crate::shell::resolve_program(cmd))
            .env("TMPDIR", &tmp)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(format!("cannot spawn {cmd} (is it on PATH?): {e}"));
            }
        };
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || reader_loop(stdout, tx));
        Ok(Session {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            rx,
            tmp,
        })
    }

    fn send(&self, v: &Value) {
        let body = v.to_string();
        // lsp_proxy::frame is the ONE definition of the wire framing.
        let framed = crate::lsp_proxy::frame(&body);
        let mut stdin = match self.stdin.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = stdin.write_all(&framed);
        let _ = stdin.flush();
    }

    /// The next complete message, or `Ok(None)` when the server closed
    /// the pipe — but never a wait longer than `deadline`: the block
    /// is on the channel, and the channel has a real deadline.
    fn next_msg(&self, timeout_secs: u64, deadline: Instant) -> Result<WireMsg, String> {
        // `recv_deadline` is still unstable, so wait only as long as is
        // LEFT on the deadline. `recv_timeout` rounds its duration up
        // to the next millisecond, hence the sub-ms headroom is not a
        // correctness concern here: this is a wall-clock budget, not a
        // hard real-time one.
        let now = Instant::now();
        let wait = deadline.saturating_duration_since(now);
        match self.rx.recv_timeout(wait) {
            Ok(m) => Ok(m),
            Err(RecvTimeoutError::Timeout) => Err(timed_out(timeout_secs)),
            // The reader thread is gone: the child died. Not an instant
            // EOF — treat it as the pipe closing.
            Err(RecvTimeoutError::Disconnected) => Ok(None),
        }
    }

    fn wait_id(&self, id: u64, timeout_secs: u64, deadline: Instant) -> Result<(), String> {
        while Instant::now() < deadline {
            match self.next_msg(timeout_secs, deadline)? {
                Some(m) if m.get("id").and_then(Value::as_u64) == Some(id) => return Ok(()),
                Some(_) => continue,
                None => return Err("tcl-lsp closed the pipe".into()),
            }
        }
        Err(timed_out(timeout_secs))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // After the reap: nothing is writing there any more.
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// Drain `stdout` into complete frames until EOF. The ONLY blocking
/// call here is the kernel `read`, and the only way out of it is the
/// child dying — which `Session::drop` guarantees.
fn reader_loop(stdout: std::process::ChildStdout, tx: Sender<WireMsg>) {
    let mut reader = std::io::BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        // Keep handing over every COMPLETE frame already buffered —
        // the old per-call buffer threw away second-and-later frames
        // that shared a `read` with the first one.
        while let Some((body, used)) = crate::lsp_proxy::take_frame(&buf) {
            buf.drain(..used);
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if tx.send(Some(v)).is_err() {
                    return; // Session gone; nothing left to deliver.
                }
            }
            // Unparseable frames are skipped, exactly as before.
        }
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => {
                let _ = tx.send(None); // EOF: the old code's break signal.
                return;
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// Path to the fake, deliberately mute server used by the timeout
    /// tests, built once in a directory that lives for the process (an
    /// ambient tempdir dropped by a losing test thread would unlink the
    /// script under a winning one).
    static MUTE_LSP: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

    /// `tcl-lsp` that accepts stdin and answers NOTHING, so the request
    /// half of the deadline path is exercised against a genuinely mute
    /// server rather than against luck. It must NOT exit on its own: an
    /// EOF would take the honest `break` path and the timeout path would
    /// go untested — hence a loop that swallows input forever until the
    /// Session's `Drop` kills it.
    fn mute_lsp() -> Option<std::path::PathBuf> {
        MUTE_LSP
            .get_or_init(|| {
                let dir = crate::testenv::kept_dir();
                if std::fs::create_dir_all(&dir).is_err() {
                    return None;
                }
                // Reads stdin forever, answers nothing, leaves at EOF: a
                // shell script on unix, a batch file on Windows (`set /p`
                // reads a line; errorlevel 1 is EOF).
                let (name, body) = if cfg!(windows) {
                    (
                        "mute-lsp.cmd",
                        "@echo off\r\n:loop\r\nset /p _=\r\nif errorlevel 1 exit /b 0\r\ngoto loop\r\n",
                    )
                } else {
                    ("mute-lsp", "#!/bin/sh\nwhile :; do read -r _ || exit 0; done\n")
                };
                let script = dir.join(name);
                if std::fs::write(&script, body).is_err() {
                    return None;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                        .is_err()
                    {
                        return None;
                    }
                    // ETXTBSY (the gate, 2026-10-14, under a parallel nix
                    // build): a child that ANOTHER test forked while this
                    // file was open for writing inherits that fd and
                    // keeps it until its own exec — O_CLOEXEC closes at
                    // exec, not at fork — and an exec of the script in
                    // that window fails with "Text file busy". The fd is
                    // closed here already, so nothing new can inherit it;
                    // prove the script execs before handing it out. With
                    // stdin at EOF it exits on its own.
                    let mut tries = 0;
                    loop {
                        match std::process::Command::new(&script)
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                        {
                            Ok(mut probe) => {
                                let _ = probe.wait();
                                break;
                            }
                            Err(e) if e.raw_os_error() == Some(26) && tries < 200 => {
                                tries += 1;
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }
                            Err(_) => return None,
                        }
                    }
                }
                Some(script)
            })
            .clone()
    }

    /// A nonexistent file is an honest error, not a server spawn.
    #[test]
    fn a_missing_file_errors_before_the_spawn() {
        let r = lint_file("/no/such/file.tcl", 5);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("cannot read"));
    }

    /// The framing helpers round-trip a message.
    #[test]
    fn frame_round_trips_through_take_frame() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"x"}"#;
        let framed = crate::lsp_proxy::frame(body);
        let (parsed, used) = crate::lsp_proxy::take_frame(&framed).expect("frame");
        assert_eq!(parsed, body);
        assert_eq!(used, framed.len());
    }

    /// When tcl-lsp is installed (it is, on this box), a clean file
    /// lints clean and a broken one reports a diagnostic naming the
    /// file and line — the end-to-end contract. Skipped when the
    /// binary is absent so the gate stays portable.
    #[test]
    fn a_real_file_lints_end_to_end() {
        if which_tcl_lsp().is_none() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("good.tcl");
        std::fs::write(&good, "set x 1\nputs $x\n").expect("write");
        let r = lint_file(good.to_str().unwrap(), 30).expect("lint");
        assert!(r.is_empty(), "a clean file lints clean: {r:?}");

        let bad = dir.path().join("bad.tcl");
        std::fs::write(&bad, "proc f {a {\n  return $a\n}\n").expect("write");
        let r = lint_file(bad.to_str().unwrap(), 30).expect("lint");
        assert!(
            !r.is_empty(),
            "an unbalanced-brace file must produce diagnostics"
        );
        assert!(r[0].contains("bad.tcl"), "names the file: {}", r[0]);
        assert!(r[0].contains(':'), "carries line:col: {}", r[0]);
    }

    /// A lint leaves its caller's TMPDIR as it found it. tcl-lsp keeps
    /// two directories per process in TMPDIR and cannot remove them when
    /// killed, which is how every Session ends — so the Session gives it
    /// a private TMPDIR and removes that. Checked in a CHILD process with
    /// a fresh TMPDIR, because the shared one is busy with other tests.
    #[test]
    fn a_lint_leaves_its_callers_tmpdir_as_it_found_it() {
        if which_tcl_lsp().is_none() {
            return;
        }
        if let Some(src) = std::env::var_os("VELES_LSP_LITTER_SRC") {
            // Child side: TMPDIR is the parent's fresh directory.
            lint_file(src.to_str().unwrap(), 30).expect("lint");
            return;
        }
        let work = tempfile::tempdir().expect("work");
        let src = work.path().join("good.tcl");
        std::fs::write(&src, "set x 1\nputs $x\n").expect("write");
        let tmp = tempfile::tempdir().expect("child TMPDIR");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "lsp_check::tests::a_lint_leaves_its_callers_tmpdir_as_it_found_it",
                "--exact",
                "--test-threads=1",
                "--quiet",
            ])
            .env("TMPDIR", tmp.path())
            .env("VELES_LSP_LITTER_SRC", &src)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run the child");
        assert!(status.success(), "child lint failed: {status}");
        let left: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "a lint littered TMPDIR: {left:?}");
    }

    /// THE DEADLINE IS REAL (PENDING residual 1): a server that never
    /// answers must return an error in about `timeout_secs`, not hang.
    /// The old loop checked the clock only BETWEEN messages, so one
    /// blocking `read_msg` outran it forever — this is the mutation
    /// case: restore the old shape and this test hangs the gate, which
    /// is exactly the failure it exists to catch.
    #[test]
    fn a_silent_server_times_out_rather_than_hanging() {
        let Some(mute) = mute_lsp() else { return };
        let Some(mute) = mute.to_str() else { return };
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("x.tcl");
        std::fs::write(&file, "set x 1\n").expect("write");
        let start = Instant::now();
        let r = lint_file_cmd(mute, file.to_str().unwrap(), 2);
        let elapsed = start.elapsed();
        let err = r.expect_err("a mute server is an error, not a clean lint");
        assert!(err.contains("did not answer"), "and says which: {err}");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the deadline fired: {elapsed:?}"
        );
    }

    /// Same on the `/fmt` arm, whose request path differs (it waits on
    /// an id-2 response, not a notification).
    #[test]
    fn fmt_on_a_silent_server_times_out_rather_than_hanging() {
        let Some(mute) = mute_lsp() else { return };
        let Some(mute) = mute.to_str() else { return };
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("x.tcl");
        std::fs::write(&file, "set x 1\n").expect("write");
        let start = Instant::now();
        let r = fmt_file_cmd(mute, file.to_str().unwrap(), 2);
        let elapsed = start.elapsed();
        assert!(r.is_err(), "a mute server is an error, not a no-op");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the deadline fired: {elapsed:?}"
        );
    }

    /// tcl-lsp has no `--version`: it ignores the flag and starts
    /// serving on stdin. An inherited stdin that never closes (a
    /// background shell's socket) then hung this probe, and the whole
    /// test binary, for good; a null stdin makes it exit at once.
    fn which_tcl_lsp() -> Option<()> {
        std::process::Command::new("tcl-lsp")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()
            .map(|_| ())
    }
}
