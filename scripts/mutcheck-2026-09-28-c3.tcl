#!/usr/bin/env tclsh
# mutcheck-2026-09-28-c3.tcl — mutation harness for the C3 round
# (remote-task cancel crosses the wire + the ownership gates + the
# cancel_turn fix the round's red-first pin caught).
#
# House rules (b3a/b4 harnesses, learned the hard way):
#   - cargo stderr goes through `2>@1`, NEVER `2>&1`.
#   - a mutation that fails to COMPILE is NOCOMPILE, not a detection.
#   - every mutation must make its targeted test go RED while the
#     unmutated tree is GREEN (preflight).
#   - QUOTING (the b3a lesson, applied up front this time): a braced
#     word cannot hold an unbalanced `{` OR represent a line ending in
#     `}` without adding one. Every multi-line pattern here is a
#     DOUBLE-QUOTED word: `[`, `$`, `\` escaped, real newlines as `\n`,
#     Rust `"` as `\"`. `{`/`}` need no escaping inside quotes.
#   - targets carry a `pkg` (veles-core or veles-frontends) because C3
#     spans both crates.

set repo [file normalize [file dirname [file dirname [info script]]]]

set targets [dict create]

set t cancel-ownership-gate
dict set targets $t pkg  "veles-frontends"
dict set targets $t file "crates/veles-frontends/src/a2a_server.rs"
dict set targets $t find    "match srv.tasks.get(tid).filter(|rec| rec.peer == identity) {\n                Some(rec) => {\n                    let terminal = matches!("
dict set targets $t replace "match srv.tasks.get(tid) {\n                Some(rec) => {\n                    let terminal = matches!("
dict set targets $t test "cancel_from_a_foreign_peer_is_not_found"
dict set targets $t why  "without the gate any authenticated peer cancels any task by (sender-minted, externally known) id"

set t get-ownership-gate
dict set targets $t pkg  "veles-frontends"
dict set targets $t file "crates/veles-frontends/src/a2a_server.rs"
dict set targets $t find    "match srv.tasks.get(tid).filter(|rec| rec.peer == identity) {\n                Some(rec) => {\n                    let task = build_task(&rec.id, &rec.context_id, &rec.state, &rec.reply_text);"
dict set targets $t replace "match srv.tasks.get(tid) {\n                Some(rec) => {\n                    let task = build_task(&rec.id, &rec.context_id, &rec.state, &rec.reply_text);"
dict set targets $t test "tasks_get_and_list_are_peer_scoped"
dict set targets $t why  "tasks/get is the sibling leak: a foreign peer reads another peer's task state"

set t list-peer-filter
dict set targets $t pkg  "veles-frontends"
dict set targets $t file "crates/veles-frontends/src/a2a_server.rs"
dict set targets $t find    "                .filter(|r| r.peer == identity)\n                .map(|r| {"
dict set targets $t replace "                .map(|r| {"
dict set targets $t test "tasks_get_and_list_are_peer_scoped"
dict set targets $t why  "tasks/list must show a peer only its own rows"

set t cancel-params-id-spelling
dict set targets $t pkg  "veles-frontends"
dict set targets $t file "crates/veles-frontends/src/a2a_server.rs"
dict set targets $t find    "            let tid = params\n                .get(\"id\")\n                .or(params.get(\"name\"))\n                .or(params.get(\"taskId\"))\n                .and_then(|t| t.as_str())\n                .unwrap_or(\"\");\n            // C3: ownership gate."
dict set targets $t replace "            let tid = params\n                .get(\"name\")\n                .or(params.get(\"taskId\"))\n                .and_then(|t| t.as_str())\n                .unwrap_or(\"\");\n            // C3: ownership gate."
dict set targets $t test "a_wire_task_registers_working_and_cancel_kills_the_child"
dict set targets $t why  "the spec spelling is params.id; a client that follows the spec must reach the record"

set t put-keeps-one-order-entry
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/a2a.rs"
dict set targets $t find    "        let existed = self.tasks.lock_ok().insert(id.clone(), rec).is_some();\n        if !existed {\n            self.order.lock_ok().push_back(id);\n        }"
dict set targets $t replace "        self.tasks.lock_ok().insert(id.clone(), rec);\n        self.order.lock_ok().push_back(id);"
dict set targets $t test "a_re_put_updates_in_place_without_evicting_an_innocent_task"
dict set targets $t why  "the WORKING→terminal update-in-place must not duplicate the order entry (an innocent eviction at the cap)"

set t receiver-threads-the-token
dict set targets $t pkg  "veles-frontends"
dict set targets $t file "crates/veles-frontends/src/a2a_server.rs"
dict set targets $t find    "                        let token = if ask.wire_task_id.is_some() {\n                            Some(cancel.clone())\n                        } else {\n                            None\n                        };"
dict set targets $t replace "                        let token = None;"
dict set targets $t test "a_wire_task_registers_working_and_cancel_kills_the_child"
dict set targets $t why  "a cancel that only relabels the record leaves the child burning to its cap — the 2026-09-03 failure mode, over the wire"

set t watch-fires-on-either-source
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/remotetask.rs"
dict set targets $t find    "        self.token.as_ref().is_some_and(|t| t.is_cancelled())\n            || self\n                .turn_session\n                .as_ref()\n                .is_some_and(|s| crate::agent::turn_cancelled(s))"
dict set targets $t replace "        self.token.as_ref().is_some_and(|t| t.is_cancelled())\n            && self\n                .turn_session\n                .as_ref()\n                .is_some_and(|s| crate::agent::turn_cancelled(s))"
dict set targets $t test "token_source_sees_a_registry_flip"
dict set targets $t why  "EITHER source must stop the remote child; AND makes a token-only watch dead"

set t cancel-turn-flips-in-place
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/agent.rs"
dict set targets $t find    "pub fn cancel_turn(session_id: &str) -> bool {\n    let map = live_turns().lock_ok();\n    match map.get(session_id) {\n        Some(ts) if !ts.is_empty() => {\n            for t in ts {\n                t.cancel();\n            }\n            true\n        }\n        _ => false,\n    }\n}"
dict set targets $t replace "pub fn cancel_turn(session_id: &str) -> bool {\n    let toks = live_turns().lock_ok().remove(session_id);\n    match toks {\n        Some(ts) => {\n            for t in &ts {\n                t.cancel();\n            }\n            true\n        }\n        None => false,\n    }\n}"
dict set targets $t test "turn_source_sees_a_cancel_turn_flip"
dict set targets $t why  "the pre-C3 shape removed the entry first: every map-reading poller (terminal fg wait, the remote watch) read false at the moment of cancel"

# ── harness ──────────────────────────────────────────────────────────

proc green {pkg t} {
    global repo
    set out [open "|cargo test -q -p $pkg --lib $t 2>@1" r]
    set text [read $out]
    if {[catch {close $out}]} {
        return [list 0 $text]
    }
    return [list [string match "*test result: ok*" $text] $text]
}

puts "== C3 mutation harness =="
puts "repo: $repo"
cd $repo

set failed_prelight 0
foreach name [dict keys $targets] {
    set m [dict get $targets $name]
    lassign [green [dict get $m pkg] [dict get $m test]] ok text
    if {!$ok} {
        puts "PRELIGHT-RED $name ([dict get $m test])"
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
    set pkg [dict get $m pkg]
    set path [file join $repo $f]
    set orig [read [set fh [open $path r]]]; close $fh
    if {[string first $find $orig] == -1} {
        puts "NOTFOUND   $name — pattern missing (harness rot?)"
        incr notfound
        continue
    }
    set fh [open $path w]; puts -nonewline $fh [string map [list $find $repl] $orig]; close $fh
    lassign [green $pkg $t] ok text
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
    set fh [open $path w]; puts -nonewline $fh $orig; close $fh
}

set total [dict size $targets]
puts ""
puts "targets: $total  red: $red  green-escapes: $green_escape  nocompile: $nocompile  notfound: $notfound"
if {$green_escape > 0} {
    puts "FAILURES: a mutation survived — add the pin that kills it."
    exit 1
}
if {$nocompile > 0 || $notfound > 0 || $red != $total} {
    puts "INCOMPLETE: $total targets, only $red killed. Fix the harness."
    exit 1
}
puts "every mutation detected."
