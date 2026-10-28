#!/usr/bin/env python3
# mutcheck — Phase 2b of PLAN-plugins-installer.md: the plugin host wired
# into the gateway (standby routes, the fence, the graceful stop, the
# orphan's exit) and its surfaces (/plugins, the desktop routes, the
# CLI). Each mutant removes one guard and must turn its named test RED;
# the source is restored after every run (written fresh, never cp -p).
#
# Run: direnv exec . python3 scripts/mutcheck-2026-10-07-pluginhost-2b.py [name…]
# with a per-run TMPDIR; prints one line per mutant and a total.
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HOST = f"{ROOT}/crates/veles-core/src/pluginhost.rs"
BUS = f"{ROOT}/crates/veles-core/src/bus.rs"
SDK = f"{ROOT}/crates/veles-plugin-sdk/src/lib.rs"
CMDS = f"{ROOT}/crates/veles-frontends/src/commands.rs"
DESK = f"{ROOT}/crates/veles-frontends/src/desktop_api.rs"
CLI_LIB = f"{ROOT}/crates/veles-cli/src/lib.rs"
CLI_PP = f"{ROOT}/crates/veles-cli/src/procplugins.rs"
ECHO_TESTS = ["-p", "veles-plugin-echo", "--test", "host"]
FRONT = ["-p", "veles-frontends", "--all-features", "--lib"]
CLI = ["-p", "veles-cli", "--all-features", "--lib"]


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
    # ── a standby node does not claim the platform ──
    ("routes-claimed-at-spawn", HOST,
     "        for (platform, rx) in queues {\n",
     "        inst.claim_routes(&self.inner.bus);\n        for (platform, rx) in queues {\n",
     ECHO_TESTS, "a_standby_instance_does_not_claim_its_platform"),
    ("routes-kept-off-duty", HOST,
     "        self.0.release_routes(&self.1);\n",
     "",
     ECHO_TESTS, "a_standby_instance_does_not_claim_its_platform"),
    ("unregister-removes-nothing", BUS,
     "            routes.remove(route);\n",
     "",
     ECHO_TESTS, "a_standby_instance_does_not_claim_its_platform"),
    # ── a refusing gate is reported as refused ──
    ("refusal-silent", HOST,
     "                inst.set_state(\"refused\", None, Some(format!(\"{e:#}\")));\n",
     "",
     ECHO_TESTS, "a_refusing_gate_says_why"),
    # ── the gateway's stop ──
    ("quit-reads-as-stop", HOST,
     "                    Some(Ctl::Quit(_)) | None => \"the gateway is stopping\",",
     "                    None => \"the gateway is stopping\",",
     ECHO_TESTS, "stopping_the_gateway_stops_every_plugin_gracefully"),
    ("standby-starts-while-quitting", HOST,
     "        self.inner.quitting.store(true, Ordering::Release);\n",
     "",
     ECHO_TESTS, "a_standby_instance_stays_down_once_the_gateway_stops"),
    # ── the orphan exits ──
    ("orphan-skips-shutdown", SDK,
     "            if !sdk.stopping.swap(true, std::sync::atomic::Ordering::AcqRel) {\n                let why",
     "            if false {\n                let why",
     ECHO_TESTS, "a_plugin_exits_when_the_gateway_goes_away"),
    ("orphan-shutdown-unlimited", SDK,
     "tokio::time::timeout(SHUTDOWN_LIMIT, sdk.plugin.shutdown(&why))",
     "tokio::time::timeout(std::time::Duration::from_secs(3600), sdk.plugin.shutdown(&why))",
     ECHO_TESTS, "a_hung_shutdown_does_not_keep_an_orphan_alive"),
    # ── request / logs ──
    ("request-not-sent", HOST,
     "            if i.ctl_tx.send(make(tx)).is_ok() {",
     "            if { drop(make(tx)); true } {",
     ECHO_TESTS, "a_requested_reload_returns_at_once_and_happens"),
    ("log-not-kept", HOST,
     "                log.push_back(line);\n",
     "",
     ECHO_TESTS, "a_plugins_log_lines_are_kept"),
    # ── surfaces ──
    ("plugins-verbs-ungated", CMDS,
     "    if !caller_is(principal, Cap::CmdAdmin) {\n        return Some(denied(\n            principal,\n            &format!(\"`/plugins {verb}`\"),",
     "    if false {\n        return Some(denied(\n            principal,\n            &format!(\"`/plugins {verb}`\"),",
     FRONT, "commands::tests::plugins_reload_is_an_admin_act"),
    ("last-error-for-everyone", CMDS,
     "        if admin {\n            if let Some(e) = &s.last_error {",
     "        if true {\n            if let Some(e) = &s.last_error {",
     FRONT, "commands::tests::a_process_plugins_last_error_is_for_admins"),
    ("desktop-any-action", DESK,
     "    if !matches!(action, \"reload\" | \"stop\" | \"start\") {",
     "    if false {",
     FRONT, "desktop_api::tests::process_plugin_controls_answer_without_a_host"),
    ("fence-bypassed", CLI_LIB,
     "        match may_start_singleton(&self.fence, key, &self.me, what).await {",
     "        match SingletonStart::Ungated {",
     CLI, "tests::a_process_plugin_obeys_the_singleton_fence"),
    ("target-any-chars", CLI_PP,
     "            .all(|c| c.is_ascii_alphanumeric() || \"._-:\".contains(c));",
     "            .all(|_| true);",
     CLI, "procplugins::tests::a_target_is_a_name_not_a_path"),
    ("target-dotdot", CLI_PP,
     "        && target != \"..\"\n",
     "",
     CLI, "procplugins::tests::a_target_is_a_name_not_a_path"),
    # ── a restart comes back to the state dir the host was made with ──
    ("state-root-looked-up-per-run", HOST,
     "        let mut state = host.state_root.join(&self.plugin);\n",
     "        let mut state = crate::config::data_dir().join(\"plugin-state\").join(&self.plugin);\n",
     ECHO_TESTS, "a_restart_keeps_the_state_dir_the_host_was_made_with"),
    # ── a restart runs the version installed now, on its own manifest ──
    ("describe-skipped-before-spawn", HOST,
     "    if let Err(e) = describe(&inst.exe).await.and_then(|m| inst.adopt(m)) {\n"
     "        return Ended::Exited(e);\n    }\n",
     "",
     ECHO_TESTS, "a_reload_takes_the_new_versions_manifest"),
    ("platforms-unchecked", HOST,
     "        if now != had {\n",
     "        if false && now != had {\n",
     ECHO_TESTS, "a_reload_that_moves_the_platforms_says_restart"),
    ("lease-unchecked", HOST,
     "        if lease != self.lease {\n",
     "        if false && lease != self.lease {\n",
     ECHO_TESTS, "a_reload_that_moves_the_lease_says_restart"),
]

only = set(sys.argv[1:])
results = [run(*m) for m in M if not only or m[0] in only]
ok = sum(results)
print(f"== mutants: {ok}/{len(results)} red-correct, {len(results) - ok} problems ==", flush=True)
sys.exit(0 if ok == len(results) else 1)
