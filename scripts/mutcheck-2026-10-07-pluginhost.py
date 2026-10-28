#!/usr/bin/env python3
# mutcheck — the process-plugin host (veles-core/src/pluginhost.rs) and
# SDK, PLAN-plugins-installer.md Phase 2. Each mutant removes one guard
# and must turn its named test RED; the source is restored after every
# run (written fresh, so cargo sees a new mtime — never a cp -p restore).
#
# The host's tests are integration tests of veles-plugin-echo: they run
# the real host against the real echo binary.
#
# Run: direnv exec . python3 scripts/mutcheck-2026-10-07-pluginhost.py [name…]
# with a per-run TMPDIR; prints one line per mutant and a total.
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HOST = f"{ROOT}/crates/veles-core/src/pluginhost.rs"
SDK = f"{ROOT}/crates/veles-plugin-sdk/src/lib.rs"
ECHO_TESTS = ["-p", "veles-plugin-echo", "--test", "host"]
CORE_LIB = ["-p", "veles-core", "--lib"]
SDK_LIB = ["-p", "veles-plugin-sdk", "--lib"]


def run(name, path, old, new, where, test):
    src = open(path, encoding="utf-8").read()
    if src.count(old) != 1:
        print(f"MUTANT {name}: ANCHOR {'MISSING' if old not in src else 'AMBIGUOUS'}", flush=True)
        return False
    open(path, "w", encoding="utf-8").write(src.replace(old, new, 1))
    try:
        time.sleep(1.2)
        p = subprocess.run(
            ["cargo", "test", *where, test, "--", "--exact"],
            cwd=ROOT, capture_output=True, text=True, timeout=1500,
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


M = [
    # ── identity: a plugin speaks for its own platform only ──
    ("vouch-for-any-platform", HOST,
     "            if !self.may_vouch_for(id) {\n                tracing::warn!(\n                    \"plugin {label}: vouched for",
     "            if false {\n                tracing::warn!(\n                    \"plugin {label}: vouched for",
     ECHO_TESTS, "a_plugin_cannot_speak_for_another_platform"),
    ("foreign-platform-delivered", HOST,
     "        if !self.inst.declares_platform(&msg.platform) {",
     "        if false {",
     ECHO_TESTS, "a_plugin_cannot_speak_for_another_platform"),
    ("plugin-picks-the-route", HOST,
     "        msg.route = if self.inst.instance.is_empty() {\n            None\n        } else {\n            Some(self.inst.instance.clone())\n        };",
     "",
     ECHO_TESTS, "a_plugin_cannot_speak_for_another_platform"),
    ("context-platform-trusted", HOST,
     "        msg.sender_ctx.platform = Some(msg.platform.clone());",
     "",
     ECHO_TESTS, "a_plugin_cannot_speak_for_another_platform"),
    ("resolve-any-platform", HOST,
     "                    if !self.may_vouch_for(id) {\n                        return Err(RpcError::refused(format!(",
     "                    if false {\n                        return Err(RpcError::refused(format!(",
     ECHO_TESTS, "a_plugin_cannot_speak_for_another_platform"),
    # ── secrets ──
    ("undeclared-secret-given", HOST,
     "                if !declared {",
     "                if false {",
     ECHO_TESTS, "only_declared_secrets_reach_the_plugin"),
    ("environment-inherited", HOST,
     "    let mut c = tokio::process::Command::new(exe);\n    c.env_clear();",
     "    let mut c = tokio::process::Command::new(exe);",
     ECHO_TESTS, "only_declared_secrets_reach_the_plugin"),
    # ── lifecycle ──
    ("no-backoff", HOST,
     "                    _ = tokio::time::sleep(backoff) => {}",
     "                    _ = tokio::time::sleep(Duration::ZERO) => {}",
     ECHO_TESTS, "a_plugin_that_fails_to_start_backs_off"),
    ("reload-without-shutdown", HOST,
     "                let _ = peer\n                    .call_timeout(\n                        method::SHUTDOWN,",
     "                let _ = peer\n                    .call_timeout(\n                        \"not.shutdown\",",
     ECHO_TESTS, "a_reload_replaces_the_process"),
    ("child-outlives-its-supervisor", HOST,
     "        .stderr(std::process::Stdio::piped())\n        .kill_on_drop(true);\n    let mut child = match cmd.spawn() {",
     "        .stderr(std::process::Stdio::piped())\n        .kill_on_drop(false);\n    let mut child = match cmd.spawn() {",
     ECHO_TESTS, "the_process_runs_only_while_its_lease_is_held"),
    ("protocol-unchecked", HOST,
     "            if manifest.protocol != PROTOCOL {\n                self.refuse(",
     "            if false {\n                self.refuse(",
     ECHO_TESTS, "another_protocol_is_refused_by_name"),
    # ── the queue ──
    ("queue-without-cap", HOST,
     "        if q.len() >= cap {",
     "        if false {",
     ECHO_TESTS, "a_full_queue_drops_its_oldest"),
    ("queue-without-age-limit", HOST,
     "        q.retain(|(at, _)| at.elapsed() <= host.opts.queue_ttl);",
     "",
     ECHO_TESTS, "a_stale_message_is_dropped_not_delivered_late"),
    # ── names and leases (unit) ──
    ("any-instance-name", HOST,
     "                if !proto::valid_instance_name(n) {",
     "                if false {",
     CORE_LIB, "pluginhost::tests::per_entry_instances_come_from_the_config"),
    ("any-lease-key", HOST,
     "        Some(k) if k.starts_with(\"singleton:\") => Ok(Some(k.replace(\"{instance}\", instance))),",
     "        Some(k) => Ok(Some(k.replace(\"{instance}\", instance))),",
     CORE_LIB, "pluginhost::tests::a_lease_must_be_an_arbiter_singleton"),
    # ── the SDK ──
    ("sdk-any-protocol", SDK,
     "                if init.protocol != PROTOCOL {",
     "                if false {",
     SDK_LIB, "tests::another_protocol_is_refused_before_start"),
    ("sdk-delivers-uninitialized", SDK,
     "            method::DELIVER => {\n                if self.host.get().is_none() {",
     "            method::DELIVER => {\n                if false {",
     SDK_LIB, "tests::delivery_before_initialize_is_refused"),
]

only = set(sys.argv[1:])
results = [run(*m) for m in M if not only or m[0] in only]
ok = sum(results)
print(f"== mutants: {ok}/{len(results)} red-correct, {len(results) - ok} problems ==", flush=True)
sys.exit(0 if ok == len(results) else 1)
