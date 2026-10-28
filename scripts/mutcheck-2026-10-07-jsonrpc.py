#!/usr/bin/env python3
# mutcheck — the process-plugin JSON-RPC peer (veles-types/src/jsonrpc.rs,
# PLAN-plugins-installer.md Phase 1). Each mutant removes one guard and
# must turn its named test RED; the source is restored after every run
# (written fresh, so cargo sees a new mtime — never a cp -p restore).
#
# Run: direnv exec . python3 scripts/mutcheck-2026-10-07-jsonrpc.py [name…]
# with a per-run TMPDIR; prints one line per mutant and a total. Names
# given = only those mutants.
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RPC = f"{ROOT}/crates/veles-types/src/jsonrpc.rs"


def run(name, path, old, new, crate, test):
    src = open(path, encoding="utf-8").read()
    if src.count(old) != 1:
        print(f"MUTANT {name}: ANCHOR {'MISSING' if old not in src else 'AMBIGUOUS'}", flush=True)
        return False
    open(path, "w", encoding="utf-8").write(src.replace(old, new, 1))
    try:
        time.sleep(1.2)
        p = subprocess.run(
            ["cargo", "test", "-p", crate, "--lib", test],
            cwd=ROOT, capture_output=True, text=True, timeout=900,
            stdin=subprocess.DEVNULL,
        )
        out = p.stdout + p.stderr
        ran = "running 1 test" in out
        red = p.returncode != 0 and "test result: FAILED" in out
        how = "RED (correct)" if red else "STILL GREEN (BAD)"
        if p.returncode != 0 and "error[" in out and "test result: FAILED" not in out:
            how = "RED by compile error (check the anchor)"
            red = False
        elif not ran and not red:
            how = "NO TEST RAN (BAD filter)"
        print(f"MUTANT {name}: {how}", flush=True)
        return red
    finally:
        open(path, "w", encoding="utf-8").write(src)
        time.sleep(1.2)


T = "jsonrpc::tests::"
M = [
    # Notifications handled on a task each: a slow early one is overtaken.
    ("notifications-concurrent", RPC,
     "                let _ = note_tx.send((method.to_string(), params));",
     "                let _ = &note_tx;\n"
     "                let (peer, handler, q, m) = (peer.clone(), handler.clone(), queued.clone(), method.to_string());\n"
     "                tokio::spawn(async move { handler.notification(&peer, &m, params).await; q.fetch_sub(1, Ordering::AcqRel); });",
     "veles-types", T + "notifications_keep_their_order"),
    # A call after the close registers and waits on a writer that accepts it.
    ("no-closed-check", RPC,
     "            if let Some(why) = &calls.closed {\n                return Err(RpcError::closed(why));\n            }\n            calls.waiting.insert(id, tx);",
     "            calls.waiting.insert(id, tx);",
     "veles-types", T + "a_closed_connection_fails_calls"),
    # An unterminated line buffered without a ceiling.
    ("no-line-cap", RPC,
     "                let n = avail.len();\n                if buf.len() + n > max {",
     "                let n = avail.len();\n                if false {",
     "veles-types", T + "an_oversized_line_closes_the_connection"),
    # Past the cap, requests run anyway.
    ("no-busy", RPC,
     "                if inflight.load(Ordering::Acquire) >= opts.max_inflight {",
     "                if false {",
     "veles-types", T + "requests_past_the_cap_are_busy_not_queued"),
    # A timed-out call stays in the table forever.
    ("timeout-keeps-entry", RPC,
     "                self.lock().waiting.remove(&id);\n                Err(RpcError::new(\n                    RpcError::TIMEOUT,",
     "                Err(RpcError::new(\n                    RpcError::TIMEOUT,",
     "veles-types", T + "a_timed_out_call_is_forgotten"),
    # A failed write ends the writer silently; the callers wait for an EOF.
    ("silent-write-failure", RPC,
     "            peer.close(&format!(\"write failed: {e}\"));\n            return;",
     "            let _ = e;\n            return;",
     "veles-types", T + "a_failed_write_fails_the_waiting_calls"),
    # The notification queue without a ceiling.
    ("no-backlog-cap", RPC,
     "                if queued.fetch_add(1, Ordering::AcqRel) >= opts.max_queued_notifications {",
     "                if queued.fetch_add(1, Ordering::AcqRel) >= usize::MAX {",
     "veles-types", T + "a_notification_flood_closes_the_connection"),
]

only = set(sys.argv[1:])
results = [run(*m) for m in M if not only or m[0] in only]
ok = sum(results)
print(f"== mutants: {ok}/{len(results)} red-correct, {len(results) - ok} problems ==", flush=True)
sys.exit(0 if ok == len(results) else 1)
