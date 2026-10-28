#!/usr/bin/env python3
# mutcheck — a mail turn's rights cap survives the gateway's re-resolve
# (2026-10-13, USER: "fix the mail clamp bug too"). A clamp written over
# the session's own principal was undone by the per-message re-resolve
# (`agent_for_sender`), and the rest of the mail turn ran with the
# session's higher rights. Each mutant removes one piece of the fix and
# must turn its named test RED; the source is restored after every run
# (written fresh, never cp -p).
#
# Run: CARGO_TARGET_DIR=<target> TMPDIR=$(mktemp -d /dev/shm/x.XXXXXX) \
#      direnv exec . python3 scripts/mutcheck-2026-10-13-mail-cap.py [name…]
# Prints one line per mutant and a total.
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AGENT = f"{ROOT}/crates/veles-core/src/agent.rs"
CORE = ["-p", "veles-core", "--all-features", "--lib"]
CAP = "mail::tests::a_capped_mail_turn_keeps_its_cap_when_the_session_is_re_resolved"
CLAMP = "mail::tests::a_mail_turn_runs_clamped_records_its_reply_and_never_runs_twice"


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
            cwd=ROOT, capture_output=True, text=True, timeout=2400,
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
    # the bug itself: the clamp written over the session's own principal
    ("clamp-over-the-session", AGENT,
     "                agent.cap_turn_principal(Some(p));\n                true",
     "                agent.set_principal(p);\n                true",
     CORE, CAP),
    # a cap that ignores a revocation below it
    ("cap-ignores-a-revocation", AGENT,
     "                match crate::mail::turn_principal(Some(&cap), standing.as_deref()) {\n                    Some(lower) => Some(std::sync::Arc::new(lower)),\n                    None => standing,\n                }",
     "                Some(cap)",
     CORE, CAP),
    # tools read the registry: it must say the turn's rights
    ("registry-says-the-session", AGENT,
     "    fn publish_principal(&self) {\n        match self.principal() {",
     "    fn publish_principal(&self) {\n        match self.standing_principal() {",
     CORE, CAP),
    # the prompt a capped turn is sent
    ("capped-turn-gets-the-sessions-prompt", AGENT,
     "        let capped = matches!(*self.turn_rights.read_ok(), Some(TurnRights::Capped(_)));\n        if capped {",
     "        let capped = matches!(*self.turn_rights.read_ok(), Some(TurnRights::Capped(_)));\n        if capped && false {",
     CORE, CLAMP),
    # undone after the turn
    ("cap-outlives-the-turn", AGENT,
     "        if self.capped {\n            self.agent.cap_turn_principal(None);\n        }",
     "",
     CORE, CLAMP),
    ("prompt-stays-capped", AGENT,
     "        *self.turn_rights.write_ok() = p.map(|p| TurnRights::Capped(std::sync::Arc::new(p)));\n        self.publish_principal();\n        self.rebuild_system_prompt();",
     "        let start = p.is_some();\n        *self.turn_rights.write_ok() = p.map(|p| TurnRights::Capped(std::sync::Arc::new(p)));\n        self.publish_principal();\n        if start {\n            self.rebuild_system_prompt();\n        }",
     CORE, CLAMP),
]

want = set(sys.argv[1:])
picked = [m for m in M if not want or m[0] in want]
red = sum(run(*m) for m in picked)
print(f"{red}/{len(picked)} RED")
sys.exit(0 if red == len(picked) else 1)
