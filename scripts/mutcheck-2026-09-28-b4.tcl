#!/usr/bin/env tclsh
# mutcheck-2026-09-28-b4.tcl — mutation harness for the B4 round
# (work placement: load metrics, placement resolver, remote delegate).
#
# House rules (learned the hard way, see PENDING 2026-09-27):
#   - cargo stderr goes through `2>@1`, NEVER `2>&1` (in Tcl that
#     writes a FILE named &1 and a non-compiling mutation looks like
#     "no tests ran", which we would have scored RED = detected).
#   - a mutation that fails to COMPILE is NOCOMPILE — the harness says
#     so instead of pretending a tally proved anything.
#   - every mutation must make at least one targeted test go RED while
#     the unmutated tree is GREEN (run once at start).
#   - QUOTING, round two (this file's first run died on it): inside a
#     Tcl double-quoted word, `\[` does NOT save you — the bracket
#     still starts a command substitution (the `\"agent\"` in
#     args[\"agent\"] was executed as a command). A BRACED word cannot
#     hold an unbalanced `{` (and keeps `\` literal, so Rust's `"\n"`
#     stops matching). Rule: brace-free patterns → braced word;
#     patterns with `{` or `"` → double-quoted word with every `[`,
#     `$` and `\` escaped AND real newlines written as `\n`.

set repo [file normalize [file dirname [file dirname [info script]]]]

set targets [dict create]

set t load-normalization
dict set targets $t file    "crates/veles-core/src/node.rs"
dict set targets $t find    {let normalized = (one / cpus * 100.0).clamp(0.0, 100.0);}
dict set targets $t replace {let normalized = one.clamp(0.0, 100.0);}
dict set targets $t test    "load_is_normalized_by_cpu_count_and_never_invents_idle"
dict set targets $t why     "raw loadavg cannot compare a 16-thread laptop with a 2-vCPU VPS"

set t heartbeat-keeps-last-sample
dict set targets $t file    "crates/veles-core/src/store.rs"
dict set targets $t find    {load1 = COALESCE(?2, load1),}
dict set targets $t replace {load1 = ?2,}
dict set targets $t test    "a_heartbeat_carries_load_and_none_keeps_the_last_sample"
dict set targets $t why     "an unreadable load must not zero the sample — unknown is not idle"

set t served-bump-only-on-insert
dict set targets $t file    "crates/veles-core/src/store.rs"
dict set targets $t find    "        if n > 0 {"
dict set targets $t replace "        if true {"
dict set targets $t test    "served_counts_only_new_sessions_and_never_resets"
dict set targets $t why     "a session RESUME is not a new session; the counter must not inflate"

set t any-excludes-stale
dict set targets $t file    "crates/veles-core/src/placement.rs"
dict set targets $t find    "                .filter(|n| !crate::node::is_stale(&n.heartbeat_at, stale_after))\n                .collect();\n            if live.is_empty() {"
dict set targets $t replace "                .collect();\n            if live.is_empty() {"
dict set targets $t test    "any_is_least_loaded_with_unknown_last_and_stale_excluded"
dict set targets $t why     "placing work on a node we have not heard from is hope, not placement"

set t unknown-load-ranks-last
dict set targets $t file    "crates/veles-core/src/placement.rs"
dict set targets $t find    {    let ka = a.load1.unwrap_or(i64::MAX);
    let kb = b.load1.unwrap_or(i64::MAX);}
dict set targets $t replace {    let ka = a.load1.unwrap_or(0);
    let kb = b.load1.unwrap_or(0);}
dict set targets $t test    "any_is_least_loaded_with_unknown_last_and_stale_excluded"
dict set targets $t why     "unknown load ranking FIRST is unknown-read-as-idle — the lie load1 exists to prevent"

set t named-node-stale-refusal
dict set targets $t file    "crates/veles-core/src/placement.rs"
dict set targets $t find    "if crate::node::is_stale(&full.heartbeat_at, stale_after) \{"
dict set targets $t replace "if false \{"
dict set targets $t test    "named_node_resolves_and_refuses"
dict set targets $t why     "naming a node does not make a dead one a placement target"

set t gpu-filter
dict set targets $t file    "crates/veles-core/src/placement.rs"
dict set targets $t find    {.filter(|n| gpu_name(n).is_some())}
dict set targets $t replace {.filter(|_| true)}
dict set targets $t test    "gpu_picks_a_gpu_node_or_refuses"
dict set targets $t why     "backend=gpu must never land on a CPU node"

set t remote-allowlist
dict set targets $t file    "crates/veles-frontends/src/a2a_server.rs"
dict set targets $t find    "                if !allowed.iter().any(|p| p == peer) {\n                    (\n                        STATE_REJECTED,\n                        format!(\"remote task refused: '{peer}' is not in \[cluster\] memsync_peers\"),"
dict set targets $t replace "                if false {\n                    (\n                        STATE_REJECTED,\n                        format!(\"remote task refused: '{peer}' is not in \[cluster\] memsync_peers\"),"
dict set targets $t test    "a_remote_task_from_an_unlisted_peer_is_refused"
dict set targets $t why     "a bearer token alone must not buy compute on this node"

set t remote-checksum
dict set targets $t file    "crates/veles-core/src/remotetask.rs"
dict set targets $t find    {    if crate::memory::MemoryStore::content_sha(&json) != sha {
        anyhow::bail!("checksum mismatch — the payload changed in transit");
    }
    serde_json::from_str(&json).context("payload is not a remote task ask")}
dict set targets $t replace {    serde_json::from_str(&json).context("payload is not a remote task ask")}
dict set targets $t test    "ask_round_trips_and_detects_tampering"
dict set targets $t why     "a tampered ask must be refused, not run"

set t preset-backend-refusal
dict set targets $t file    "crates/veles-core/src/delegate.rs"
dict set targets $t find    "if backend.is_some() && args\[\"agent\"\].as_str().is_some_and(|a| !a.trim().is_empty()) {"
dict set targets $t replace "if false {"
dict set targets $t test    "an_agent_preset_and_a_backend_are_refused_together"
dict set targets $t why     "preset + backend must be refused, not silently resolved"

set t config-default-backend
dict set targets $t file    "crates/veles-core/src/delegate.rs"
dict set targets $t find    ".and_then(|c| c.cluster.delegate_backend)"
dict set targets $t replace ".and_then(|_c| None::<String>)"
dict set targets $t test    "a_config_default_backend_is_honoured_when_the_call_omits_it"
dict set targets $t why     "the operator's placement default must actually be consulted"

set t task-reply-shape
dict set targets $t file    "crates/veles-core/src/a2a.rs"
dict set targets $t find    {        .or_else(|| {
            msg.get("artifacts")
                .and_then(|a| a.get(0))
                .and_then(|a| a.get("parts"))
                .and_then(|p| p.as_array())
        })}
dict set targets $t replace ""
dict set targets $t test    "extract_text_reads_a_task_reply"
dict set targets $t why     "the B3a bug this round found: task replies decoded as empty string"

set t gossip-carries-load
dict set targets $t file    "crates/veles-core/src/store.rs"
dict set targets $t find    {load1 = ?11, load_at = ?12, sessions_served = ?13,}
dict set targets $t replace {load1 = load1, load_at = load_at, sessions_served = sessions_served,}
dict set targets $t test    "gossip_merge_preserves_local_truths"
dict set targets $t why     "a column gossip silently drops leaves every remote node's load invisible to `any`"

# ── harness ──────────────────────────────────────────────────────────

# Every target test below is a real suite name; if a name is wrong the
# preflight goes red and the operator fixes the harness, not the code.
proc green {t} {
    global repo
    set out [open "|cargo test -q -p veles-core --lib $t 2>@1" r]
    set text [read $out]
    if {[catch {close $out}]} {
        return [list 0 $text]
    }
    return [list [string match "*test result: ok*" $text] $text]
}

puts "== B4 mutation harness =="
puts "repo: $repo"
cd $repo

# 0. the unmutated tree must be green for every target test.
set failed_prelight 0
foreach name [dict keys $targets] {
    set t [dict get $targets $name test]
    lassign [green $t] ok text
    if {!$ok} {
        puts "PRELIGHT-RED $name ($t)"
        incr failed_prelight
    }
}
if {$failed_prelight} {
    puts "$failed_prelight target test(s) red BEFORE any mutation — fix first."
    exit 1
}
puts "preflight: all target tests green."

set red 0; set green_escape 0; set nocompile 0; set notfound 0
foreach name [dict keys $targets] {
    set m [dict get $targets $name]
    set f [dict get $m file]
    set find [dict get $m find]
    set repl [dict get $m replace]
    set t [dict get $m test]
    set path [file join $repo $f]
    set orig [read [set fh [open $path r]]]; close $fh
    if {[string first $find $orig] == -1} {
        puts "NOTFOUND   $name — pattern missing (harness rot?)"
        incr notfound
        continue
    }
    # apply the mutation
    set fh [open $path w]; puts -nonewline $fh [string map [list $find $repl] $orig]; close $fh
    lassign [green $t] ok text
    if {[string match "*error\[*" $text] && ![string match "*test result:*" $text]} {
        puts "NOCOMPILE  $name — the mutation does not build; retarget the pattern"
        incr nocompile
    } elseif {!$ok} {
        puts "RED        $name — [dict get $m why]"
        incr red
    } else {
        puts "GREEN-ESC  $name — [dict get $m why]  << UNDETECTED"
        incr green_escape
    }
    # restore
    set fh [open $path w]; puts -nonewline $fh $orig; close $fh
}

set total [dict size $targets]
puts ""
puts "targets: $total  red: $red  green-escapes: $green_escape  nocompile: $nocompile  notfound: $notfound"
# A harness that skipped everything and called it a pass is worse than a
# red harness: it certifies gates nothing checks. Only a full sweep where
# every mutation turns its pin RED is a pass.
if {$green_escape > 0} {
    puts "FAILURES: a mutation survived — add the pin that kills it."
    exit 1
}
if {$nocompile > 0 || $notfound > 0 || $red != $total} {
    puts "INCOMPLETE: $total targets, only $red killed. Fix the harness."
    exit 1
}
puts "every mutation detected."
