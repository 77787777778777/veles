#!/usr/bin/env python3
# mutcheck — R0 of PLAN-plugins-installer.md (revised 2026-10-07): the
# owner presence removed (a leftover block is named, the desktop snippet
# may not offer it), and a plugin's stderr kept at its own level. Each
# mutant removes one guard and must turn its named test RED; the source is
# restored after every run (written fresh, never cp -p).
#
# Run: direnv exec . python3 scripts/mutcheck-2026-10-07-r0.py [name…]
# with a per-run TMPDIR; prints one line per mutant and a total.
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HOST = f"{ROOT}/crates/veles-core/src/pluginhost.rs"
IRC = f"{ROOT}/crates/veles-irc/src/lib.rs"
PANE = f"{ROOT}/veles-desktop/lib/irc.tcl"
CORE = ["-p", "veles-core", "--lib"]
IRCT = ["-p", "veles-irc", "--lib"]


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
    # ── a plugin's stderr keeps its level ──
    ("level-always-info", HOST,
     "            if rest.starts_with(' ') {\n                return (level, rest.trim_start());",
     "            if rest.starts_with(' ') {\n                return (Level::INFO, rest.trim_start());",
     CORE, "pluginhost::tests::a_plugin_line_keeps_its_level"),
    ("level-prefix-only", HOST,
     "            if rest.starts_with(' ') {\n                return (level, rest.trim_start());\n            }",
     "            {\n                return (level, rest.trim_start());\n            }",
     CORE, "pluginhost::tests::a_plugin_line_keeps_its_level"),
    ("unparsed-is-info", HOST,
     "    (Level::WARN, line)\n}",
     "    (Level::INFO, line)\n}",
     CORE, "pluginhost::tests::a_plugin_line_keeps_its_level"),
    # ── the retired owner presence ──
    ("admin-block-silent", IRC,
     "    cfg.platforms.get(\"irc\")?.get(\"admin\")?;\n",
     "    cfg.platforms.get(\"irc\")?.get(\"no-such-block\")?;\n",
     IRCT, "config_tests::a_leftover_admin_block_is_named_not_ignored"),
    ("snippet-offers-admin-again", PANE,
     "                {    channels = \"#chan\"}\n                {}\n                {To connect several",
     "                {    channels = \"#chan\"}\n                {}\n                {    [platforms.irc.admin]}\n                {    nickname = \"you\"}\n                {}\n                {To connect several",
     IRCT, "surface_tests::the_desktop_panes_snippet_names_only_keys_the_adapter_reads"),
]

only = set(sys.argv[1:])
results = [run(*m) for m in M if not only or m[0] in only]
ok = sum(results)
print(f"== mutants: {ok}/{len(results)} red-correct, {len(results) - ok} problems ==", flush=True)
sys.exit(0 if ok == len(results) else 1)
