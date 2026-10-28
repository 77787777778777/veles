#!/usr/bin/env python3
# mutcheck — the owner joining someone else's session (`/join`,
# veles_core::join, 2026-10-13). USER: the owner's words show in that
# person's chat, the owner's lines run with the owner's rights, the model
# is told who said what. Each mutant removes one piece and must turn its
# named test RED; the source is restored after every run (written fresh,
# never cp -p).
#
# Run: CARGO_TARGET_DIR=<target> TMPDIR=$(mktemp -d /dev/shm/x.XXXXXX) \
#      direnv exec . python3 scripts/mutcheck-2026-10-13-join.py [name…]
# Prints one line per mutant and a total.
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = f"{ROOT}/crates/veles-core/src"
MAIL = f"{SRC}/mail.rs"
AGENT = f"{SRC}/agent.rs"
ASK = f"{SRC}/ask.rs"
GW = f"{ROOT}/crates/veles-frontends/src/gateway.rs"
CORE = ["-p", "veles-core", "--all-features", "--lib"]
FRONT = ["-p", "veles-frontends", "--all-features", "--lib"]
M_ = "mail::tests::"
G_ = "gateway::tests::"
TURN = M_ + "a_joined_line_runs_with_the_owners_rights_and_asks_the_owner"
LOOP = G_ + "the_owner_joins_a_users_session_and_speaks_in_it"


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
    # ── only the owner's own line, written here, is raised ──
    ("relayed-join-raises", MAIL,
     "                && row.via_peer.is_none()\n                && sender.as_ref().is_some_and(is_owner);",
     "                && sender.as_ref().is_some_and(is_owner);",
     CORE, M_ + "a_join_row_that_is_not_the_owners_own_runs_as_mail"),
    ("anyones-join-raises", MAIL,
     "                && row.via_peer.is_none()\n                && sender.as_ref().is_some_and(is_owner);",
     "                && row.via_peer.is_none();",
     CORE, M_ + "a_join_row_that_is_not_the_owners_own_runs_as_mail"),
    ("anyone-sends-join", MAIL,
     "    if !is_owner(owner) {\n        bail!(\"only the owner can write into a session they joined\");\n    }\n",
     "",
     CORE, M_ + "only_the_owner_writes_a_join_line"),
    # ── the turn: the owner's rights, the owner's asks, undone after ──
    ("join-runs-as-mail", AGENT,
     "        if m.join {\n            if let Some(owner) = &m.sender {",
     "        if false {\n            if let Some(owner) = &m.sender {",
     CORE, TURN),
    ("asks-stay-with-the-user", AGENT,
     "                crate::ask::set_turn_asker(&agent.session_id, asker);\n                agent.set_turn_principal(Some(owner.clone()));",
     "                let _ = asker;\n                agent.set_turn_principal(Some(owner.clone()));",
     CORE, TURN),
    ("agent-asker-ignores-the-turn", AGENT,
     "        if let Some(a) = crate::ask::turn_asker(&self.session_id) {\n            return Some(a);\n        }\n        self.asker.read_ok().clone().map(|(_, a)| a)",
     "        self.asker.read_ok().clone().map(|(_, a)| a)",
     CORE, TURN),
    ("re-resolve-takes-it-back", AGENT,
     "        *self.principal.write_ok() = p.map(std::sync::Arc::new);\n        self.publish_principal();",
     "        *self.turn_rights.write_ok() = None;\n        *self.principal.write_ok() = p.map(std::sync::Arc::new);\n        self.publish_principal();",
     CORE, TURN),
    ("rebuild-with-the-owners-prompt", AGENT,
     "        } else {\n            self.standing_principal()\n        }\n    }",
     "        } else {\n            self.principal()\n        }\n    }",
     CORE, TURN),
    ("rights-outlive-the-turn", AGENT,
     "            crate::ask::clear_turn_asker(&self.agent.session_id);\n            self.agent.set_turn_principal(None);",
     "            crate::ask::clear_turn_asker(&self.agent.session_id);",
     CORE, TURN),
    ("asks-outlive-the-turn", AGENT,
     "            crate::ask::clear_turn_asker(&self.agent.session_id);\n            self.agent.set_turn_principal(None);",
     "            self.agent.set_turn_principal(None);",
     CORE, TURN),
    ("gone-owner-asks-someone", AGENT,
     "                    None => Arc::new(crate::ask::PolicyAsker { approve: false }),",
     "                    None => match agent.asker() { Some(a) => a, None => Arc::new(crate::ask::PolicyAsker { approve: false }) },",
     CORE, M_ + "a_joined_line_with_the_owner_gone_asks_nobody"),
    ("ask-session-asks-the-user", ASK,
     "    if let Some(a) = turn_asker(session_id) {\n        if !a.interactive() || (secret && !a.private_input()) {",
     "    if let Some(a) = turn_asker(session_id).filter(|_| false) {\n        if !a.interactive() || (secret && !a.private_input()) {",
     CORE, "ask::tests::a_turns_questions_go_where_the_turn_says"),
    # ── who said what ──
    ("owner-line-unlabelled", AGENT,
     "        let joined_line = mail_row.as_ref().filter(|_| self.turn_is_joined());",
     "        let joined_line = mail_row.as_ref().filter(|_| false);",
     CORE, TURN),
    ("user-line-unnamed", AGENT,
     "            None => match crate::join::active(&self.session_id) {",
     "            None => match None::<std::sync::Arc<crate::join::Joined>> {",
     CORE, TURN),
    # ── the gateway ──
    ("anyone-joins", GW,
     "                        if principal.role != veles_core::acl::Role::Owner\n                            || !principal.has(veles_core::acl::Cap::All)\n                        {",
     "                        if false {",
     FRONT, G_ + "join_is_the_owners_alone"),
    ("owners-line-stays-home", GW,
     "            if let Some(b) = self.join_of(&turn_key) {",
     "            if let Some(b) = self.join_of(&turn_key).filter(|_| false) {",
     FRONT, LOOP),
    ("no-echo-in-their-chat", GW,
     "        if (platform.as_str(), chat.as_str()) != (msg.platform.as_str(), msg.chat_id.as_str()) {",
     "        if false {",
     FRONT, LOOP),
    ("no-copy-for-the-owner", GW,
     "            v.tees = self.tees_for(sid);",
     "            let _ = sid;",
     FRONT, LOOP),
    ("owner-reads-own-line-back", GW,
     "                Some(r) if r.kind == veles_core::mail::KIND_JOIN => return,",
     "                Some(r) if r.kind == \"never\" => return,",
     FRONT, LOOP),
]

want = set(sys.argv[1:])
picked = [m for m in M if not want or m[0] in want]
red = sum(run(*m) for m in picked)
print(f"{red}/{len(picked)} RED")
sys.exit(0 if red == len(picked) else 1)
