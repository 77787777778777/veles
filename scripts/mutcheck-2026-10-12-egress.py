#!/usr/bin/env python3
# mutcheck — D5 of PLAN-challenge-relay.md (2026-10-12): other people's
# searches and fetches leave the way `[web] others` says (egress.rs). Each
# mutant removes one piece and must turn its named test RED; the source
# is restored after every run (written fresh, never cp -p).
#
# Run: CARGO_TARGET_DIR=target/split-check TMPDIR=$(mktemp -d /dev/shm/x.XXXXXX) \
#      direnv exec . python3 scripts/mutcheck-2026-10-12-egress.py [name…]
# Prints one line per mutant and a total.
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = f"{ROOT}/crates/veles-core/src"
EGRESS = f"{SRC}/egress.rs"
TOOLS = f"{SRC}/tools.rs"
WEB = f"{SRC}/webtools.rs"
GUARD = f"{SRC}/urlguard.rs"
REMOTE = f"{SRC}/remotetool.rs"
CORE = ["-p", "veles-core", "--lib"]
T = "egress::tests::"


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
    # ── whose call it is ──
    ("scope-never", EGRESS,
     "    if !session.is_empty() && !crate::ask::owner_session(session) {\n        Box::pin(OTHERS",
     "    if false {\n        Box::pin(OTHERS",
     CORE, T + "only_a_session_that_is_not_the_owners_is_in_scope"),
    ("dispatch-unscoped", TOOLS,
     "crate::egress::scope(&self.session_id, self.dispatch_unscoped(name, args)).await",
     "crate::egress::scope(\"\", self.dispatch_unscoped(name, args)).await",
     CORE, T + "someone_elses_search_is_not_sent_home"),
    ("peer-call-unscoped", REMOTE,
     "let run = crate::egress::scope(&session, async {",
     "let run = crate::egress::scope(\"\", async {",
     CORE, T + "a_peers_call_for_someone_else_is_refused_on_a_refusing_node"),
    # ── where it runs ──
    ("sent-home-anyway", TOOLS,
     "        if crate::egress::stays_here(&self.session_id, tool) {",
     "        if false && crate::egress::stays_here(&self.session_id, tool) {",
     CORE, T + "someone_elses_search_is_not_sent_home"),
    ("typo-means-direct", EGRESS,
     "            _ => Others::Refuse,",
     "            _ => Others::Direct,",
     CORE, T + "the_modes_parse_and_an_unknown_word_refuses"),
    # ── how it leaves ──
    ("search-ignores-others", WEB,
     "        if crate::egress::others().is_some() {",
     "        if false {",
     CORE, T + "a_proxying_node_searches_for_others_only_through_its_searxng"),
    ("search-keeps-direct-backends", WEB,
     "                    backends = vec![\"searxng\".into()];",
     "                    backends.insert(0, \"searxng\".into());",
     CORE, T + "a_proxying_node_searches_for_others_only_through_its_searxng"),
    ("refusal-ignored", WEB,
     "                crate::egress::Others::Refuse => return crate::egress::refusal(\"web_extract\"),",
     "                crate::egress::Others::Refuse => None,",
     CORE, T + "a_refusing_node_carries_nothing_for_others"),
    ("fetch-not-proxied", GUARD,
     "        b = match proxy {",
     "        b = match None::<&str> {",
     CORE, T + "a_proxying_node_fetches_for_others_through_its_proxy"),
    ("one-circuit-for-all", EGRESS,
     "    let tag = blake3::hash(session.as_bytes()).to_hex();",
     "    let tag = blake3::hash(b\"\").to_hex();",
     CORE, T + "each_session_gets_its_own_socks_user"),
]

only = set(sys.argv[1:])
results = [run(*m) for m in M if not only or m[0] in only]
ok = sum(results)
print(f"== mutants: {ok}/{len(results)} red-correct, {len(results) - ok} problems ==", flush=True)
