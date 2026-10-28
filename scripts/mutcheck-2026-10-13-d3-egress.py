#!/usr/bin/env python3
# mutcheck — D3 and the owner's /egress of PLAN-challenge-relay.md
# (2026-10-13): other people's browser runs in their own box or not at
# all; whose call a peer's is comes from the principal it carries; the
# owner's /egress tor goes the way other people's calls go; image URLs
# are fetched like web_extract's. Each mutant removes one piece and must
# turn its named test RED; the source is restored after every run
# (written fresh, never cp -p).
#
# Every run sets VELES_PODMAN_BIN to a path that does not exist and puts
# a stub agent-browser first on PATH: a mutant that skips a refusal
# reaches the box code, which would create a real container on this
# machine, or the agent's own browser, which the live gateway shares.
#
# Run: CARGO_TARGET_DIR=<target> TMPDIR=$(mktemp -d /dev/shm/x.XXXXXX) \
#      direnv exec . python3 scripts/mutcheck-2026-10-13-d3-egress.py [name…]
# Prints one line per mutant and a total.
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = f"{ROOT}/crates/veles-core/src"
EGRESS = f"{SRC}/egress.rs"
TOOLS = f"{SRC}/tools.rs"
BROWSER = f"{SRC}/browsertools.rs"
REMOTE = f"{SRC}/remotetool.rs"
ACL = f"{SRC}/acl.rs"
VISION = f"{SRC}/vision.rs"
HOOK = f"{ROOT}/crates/veles-irc/src/triggers_hook.rs"
SD = f"{ROOT}/crates/veles-slopdrop/src"
CORE = ["-p", "veles-core", "--lib"]
IRC = ["-p", "veles-irc", "--lib"]
SDLIB = ["-p", "veles-slopdrop", "--lib"]
E = "egress::tests::"
B = "browsertools::tests::"
R = "remotetool::tests::"


STUB = os.path.join(os.environ.get("TMPDIR", "/dev/shm"), "mutcheck-d3-bin")
os.makedirs(STUB, exist_ok=True)
with open(os.path.join(STUB, "agent-browser"), "w") as f:
    f.write("#!/bin/sh\necho '{\"success\":false,\"error\":\"mutcheck stub\"}'\nexit 1\n")
os.chmod(os.path.join(STUB, "agent-browser"), 0o755)


def run(name, path, old, new, where, test):
    src = open(path, encoding="utf-8").read()
    if src.count(old) != 1:
        print(f"MUTANT {name}: ANCHOR {'MISSING' if old not in src else 'AMBIGUOUS'}", flush=True)
        return False
    open(path, "w", encoding="utf-8").write(src.replace(old, new, 1))
    try:
        time.sleep(1.2)
        env = dict(
            os.environ,
            VELES_PODMAN_BIN="/nonexistent/veles-mutcheck-podman",
            PATH=STUB + ":" + os.environ["PATH"],
        )
        p = subprocess.run(
            ["cargo", "test", *where, test, "--", "--exact"],
            cwd=ROOT, capture_output=True, text=True, timeout=1500,
            stdin=subprocess.DEVNULL, env=env,
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
    # ── whose call a peer's is ──
    ("carried-owner-ignored", REMOTE,
     "let owners = carried_principal(ask.principal.as_deref()).is_ok_and(|p| p.role == Role::Owner);",
     "let owners = false;",
     CORE, E + "a_peers_call_is_the_owners_only_when_it_carries_the_owner"),
    ("received-flag-ignored", REMOTE,
     "let run = crate::egress::scope(&scope_as, ask.others, async {",
     "let run = crate::egress::scope(&scope_as, false, async {",
     CORE, E + "a_peers_call_is_the_owners_only_when_it_carries_the_owner"),
    ("flag-never-sent", REMOTE,
     "        others: crate::egress::others().is_some(),\n    }\n}",
     "        others: false,\n    }\n}",
     CORE, R + "a_call_says_when_it_goes_the_way_others_calls_go"),
    # ── the owner's /egress ──
    ("choice-not-read", TOOLS,
     "            self.egress_chosen(),\n            self.dispatch_unscoped(name, args),",
     "            false,\n            self.dispatch_unscoped(name, args),",
     CORE, E + "the_owners_tor_choice_goes_the_way_others_calls_go"),
    ("anyone-may-choose", EGRESS,
     "    if !owner {\n        return Ok(describe(false, false));\n    }\n",
     "",
     CORE, E + "the_owner_chooses_where_their_session_leaves"),
    ("bad-word-is-tor", EGRESS,
     "        Some(other) => anyhow::bail!(\"unknown /egress choice '{other}' (tor|home)\"),",
     "        Some(_) => true,",
     CORE, E + "the_owner_chooses_where_their_session_leaves"),
    # ── where a call goes ──
    ("refuse-ignores-via", EGRESS,
     "        Others::Refuse => via().map_or(Route::Home, Route::Via),",
     "        Others::Refuse => Route::Home,",
     CORE, E + "a_call_is_routed_by_its_scope_and_the_nodes_mode"),
    ("via-not-followed", TOOLS,
     "            crate::egress::Route::Via(node) => return Some(vec![node]),",
     "            crate::egress::Route::Via(_) => {}",
     CORE, E + "the_owners_tor_choice_goes_the_way_others_calls_go"),
    ("browser-not-governed", EGRESS,
     "    matches!(tool, \"web_search\" | \"web_extract\") || tool.starts_with(\"browser_\")",
     "    matches!(tool, \"web_search\" | \"web_extract\" | \"browser_guest\")",
     CORE, E + "a_call_is_routed_by_its_scope_and_the_nodes_mode"),
    # ── D3: whose browser ──
    ("everyone-gets-the-owners-browser", BROWSER,
     "    if crate::egress::others().is_none() {\n        return Ok(Session::Agent);",
     "    if true {\n        return Ok(Session::Agent);",
     CORE, B + "the_browser_tools_drive_the_callers_browser_or_none"),
    ("no-box-means-owners-browser", BROWSER,
     "        None => Err(crate::egress::browser_refusal()),",
     "        None => Ok(Session::Agent),",
     CORE, B + "someone_elses_browser_call_is_refused_where_it_has_no_box"),
    ("tools-ignore-the-caller", BROWSER,
     "    match caller() {\n        Ok(s) => browser_action_in(s, argv, timeout_secs).await,",
     "    match Ok::<Session, String>(Session::Agent) {\n        Ok(s) => browser_action_in(s, argv, timeout_secs).await,",
     CORE, B + "someone_elses_browser_call_is_refused_where_it_has_no_box"),
    ("guest-page-not-guarded", BROWSER,
     "        if caller() == Ok(Session::Guest) {\n            if let Err(why) = crate::urlguard::check(&url).await {",
     "        if false {\n            if let Err(why) = crate::urlguard::check(&url).await {",
     CORE, B + "someone_elses_browser_call_is_refused_where_it_has_no_box"),
    ("guest-gets-a-window", BROWSER,
     "            \"window\" | \"unwindow\" if s == Session::Guest => {",
     "            \"window\" | \"unwindow\" if false => {",
     CORE, B + "someone_elses_browser_call_is_refused_where_it_has_no_box"),
    ("users-without-browser", ACL,
     "                    Chat, Web, FileRead, Todo, CmdStatus, Shell, FileWrite, MediaGen, Tts,\n                    Browser,",
     "                    Chat, Web, FileRead, Todo, CmdStatus, Shell, FileWrite, MediaGen, Tts,",
     CORE, R + "the_guest_browser_runs_for_a_user_and_not_for_a_guest"),
    # ── image URLs ──
    ("vision-fetch-direct", VISION,
     "        let proxy = crate::egress::fetch_proxy(\"vision_analyze\")",
     "        let proxy = Ok::<Option<String>, String>(None)",
     CORE, E + "someone_elses_image_url_is_fetched_like_their_pages"),
    ("trigger-image-is-the-owners", HOOK,
     "    let theirs = ev.role != Some(Role::Owner);",
     "    let theirs = false;",
     IRC, "triggers_hook::vision_egress_tests::someone_elses_image_link_is_fetched_like_their_web_calls"),
    # ── a sandboxed session's picture, read in its box ──
    ("vision-never-in-the-box", VISION,
     "    if path.starts_with('/') && path != \"/work\" && !path.starts_with(\"/work/\") {\n        return None;",
     "    if true {\n        return None;",
     CORE, "vision::jail_tests::a_sandboxed_sessions_picture_is_read_in_its_box"),
    ("vision-box-for-owner-too", VISION,
     "    let crate::user_envs::Zone::Sandbox { key, name } =\n        crate::user_envs::zone_for(Some(session), crate::user_envs::ZoneArg::Unset)",
     "    let crate::user_envs::Zone::Sandbox { key, name } =\n        crate::user_envs::zone_for(Some(session), crate::user_envs::ZoneArg::Own)",
     CORE, "vision::jail_tests::a_sandboxed_sessions_picture_is_read_in_its_box"),
    # ── slopdrop: the owner's own line goes direct ──
    ("eval-owner-never-sent", f"{SD}/bridge.rs",
     "    if owner {\n        body[\"owner\"] = Value::Bool(true);\n    }",
     "",
     SDLIB, "bridge::tests::an_evaluation_with_no_room_sends_no_room"),
    ("event-owner-never-sent", f"{SD}/bridge.rs",
     "    if owner {\n        body[\"owner\"] = serde_json::Value::Bool(true);\n    }",
     "",
     SDLIB, "bridge::tests::the_owners_event_says_so"),
]

want = set(sys.argv[1:])
picked = [m for m in M if not want or m[0] in want]
red = sum(run(*m) for m in picked)
print(f"{red}/{len(picked)} RED")
sys.exit(0 if red == len(picked) else 1)
