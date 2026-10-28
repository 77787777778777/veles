#!/usr/bin/env tclsh
# mutcheck-2026-09-28-b6.tcl — mutation harness for the B6 round
# (session migration: quiesce → copy → adopt → fence).
#
# House rules (b3a/b4/c3 harnesses):
#   - cargo stderr goes through `2>@1`, NEVER `2>&1`.
#   - a mutation that fails to COMPILE is NOCOMPILE, not a detection.
#   - every mutation must make its targeted test go RED while the
#     unmutated tree is GREEN (preflight).
#   - QUOTING: every multi-line pattern is a DOUBLE-QUOTED word —
#     `[`, `$`, `\` escaped, real newlines as `\n`, Rust `"` as `\"`.
#     A braced word cannot hold an unbalanced `{` or a line ending in
#     `}` without adding one.
#   - targets carry a `pkg` because B6 spans veles-core and
#     veles-frontends.

set repo [file normalize [file dirname [file dirname [info script]]]]

set targets [dict create]

set t fence-refuses-parked
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/agent.rs"
dict set targets $t find    "        if crate::migration::is_parked(Some(&node)) {"
dict set targets $t replace "        if false {"
dict set targets $t test "a_parked_session_refuses_every_turn_and_parks_the_text"
dict set targets $t why  "the fence IS the single-writer invariant: drop it and both nodes run turns mid-handoff"

set t fence-parks-the-text
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/agent.rs"
dict set targets $t find    "            if let Some(text) = park_text {\n                let _ = self.store.lock_ok().queue_push(&self.session_id, text);\n            }"
dict set targets $t replace ""
dict set targets $t test "a_parked_session_refuses_every_turn_and_parks_the_text"
dict set targets $t why  "a refused turn that DROPS the text loses the user's message; it must park and travel"

set t fence-refuses-retired
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/agent.rs"
dict set targets $t find    "        if crate::migration::is_retired(Some(&node)) {"
dict set targets $t replace "        if false {"
dict set targets $t test "a_retired_session_refuses_turns_as_readable_history"
dict set targets $t why  "the source must not resume a session it handed off — that is the double-writer"

set t fence-on-the-wake-path
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/agent.rs"
dict set targets $t find    "        self.migration_fence(None)?;\n        let (on_event, display_reasoning) = self.wrap_for_bus(on_event);\n        let res = self.run_turn_inner_empty(history, cancel, on_event).await;"
dict set targets $t replace "        let (on_event, display_reasoning) = self.wrap_for_bus(on_event);\n        let res = self.run_turn_inner_empty(history, cancel, on_event).await;"
dict set targets $t test "a_wake_turn_under_a_parked_session_is_refused_too"
dict set targets $t why  "the wake path is a second writer: a background completion could run a turn under a parked session"

set t adopt-is-idempotent
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/migration.rs"
dict set targets $t find    "            if already.contains(&m.session_id) {\n                continue; // history of an already-live session stays as-is\n            }"
dict set targets $t replace ""
dict set targets $t test "adopt_is_idempotent_on_a_re_push"
dict set targets $t why  "the lost-reply re-push is a NORMAL event; doubling history corrupts the transcript"

set t adopt-verifies-addressee
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/migration.rs"
dict set targets $t find    "    if bundle.to_node != me_node {\n        anyhow::bail!("
dict set targets $t replace "    if false {\n        anyhow::bail!("
dict set targets $t test "adopt_refuses_a_bundle_addressed_elsewhere"
dict set targets $t why  "without the addressee check, whoever catches a bundle adopts it — a session moves to a node no operator named"

set t adopt-rejects-cross-file-parent
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/migration.rs"
dict set targets $t find    "                        if in_bundle || live_here > 0 {\n                            Some(p.clone())\n                        } else {\n                            None\n                        }"
dict set targets $t replace "                        Some(p.clone())"
dict set targets $t test "a_dangling_parent_is_orphaned_honestly_at_adopt"
dict set targets $t why  "a parent link crossing files points at a row this store does not have — the exact breakage question 1 refused"

set t blobs-travel-and-are-rewritten
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/migration.rs"
dict set targets $t find    "                    let dest = blob_root.join(rel);"
dict set targets $t replace "                    let dest = std::path::PathBuf::from(&c.path);"
dict set targets $t test "adopt_rewrites_checkpoint_blobs_under_its_own_root"
dict set targets $t why  "the source's absolute blob path is meaningless on the target — a restore would read the wrong node's disk"

set t bundle-carries-blob-bytes
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/migration.rs"
dict set targets $t find    "                    match std::fs::read(file) {\n                        Ok(b) => (\n                            Some(base64::engine::general_purpose::STANDARD.encode(&b)),\n                            key,\n                        ),"
dict set targets $t replace "                    match std::fs::read(file) {\n                        Ok(_b) => (None, key),"
dict set targets $t test "the_bundle_carries_every_travelling_table_and_blobs"
dict set targets $t why  "question 4: the bytes ride INSIDE the bundle; shipping a path ships nothing"

set t subtree-cycle-terminates
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/migration.rs"
dict set targets $t find    "        if !seen.insert(id.clone()) {\n            continue;\n        }"
dict set targets $t replace ""
dict set targets $t test "a_parent_cycle_terminates"
dict set targets $t why  "the store does not promise parent is acyclic; without the seen-set the walk spins forever"

set t unpark-only-touches-parked
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/migration.rs"
dict set targets $t find    "                \"UPDATE sessions SET node = ?1 WHERE id = ?2 AND node LIKE ?3\",\n                rusqlite::params!\[me_node, s.id, format!(\"{PARK_PREFIX}%\")],"
dict set targets $t replace "                \"UPDATE sessions SET node = ?1 WHERE id = ?2\",\n                rusqlite::params!\[me_node, s.id],"
dict set targets $t test "park_retire_unpark_stamps_are_exclusive_and_named"
dict set targets $t why  "a reclaim must not resurrect a RETIRED row — that re-creates the double writer the query exists to prevent"

set t query-reads-the-store
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/migration.rs"
dict set targets $t find    "        Some(node) if !is_retired(node.as_deref()) => Ok(format!("
dict set targets $t replace "        Some(_node) => Ok(format!("
dict set targets $t test "the_query_round_trips_and_answers_from_the_store"
dict set targets $t why  "a tombstone is not an adoption: answering 'adopted' for it would make the source retire twice and hide the real owner"

set t receiver-gates-on-migrate-peers
dict set targets $t pkg  "veles-frontends"
dict set targets $t file "crates/veles-frontends/src/a2a_server.rs"
dict set targets $t find    "        let allowed = veles_core::config::load()\n            .map(|c| c.cluster.migrate_peers)\n            .unwrap_or_default();\n        let me = self_node_id(&self.store.lock_ok());"
dict set targets $t replace "        let allowed: Vec<String> = vec!\[peer.to_string()];\n        let me = self_node_id(&self.store.lock_ok());"
dict set targets $t test "a_migration_bundle_from_an_unlisted_peer_is_refused_by_name"
dict set targets $t why  "question 5: the allowlist IS the target's consent — any peer able to auth must not be able to plant history"

set t receiver-gates-the-query-too
dict set targets $t pkg  "veles-frontends"
dict set targets $t file "crates/veles-frontends/src/a2a_server.rs"
dict set targets $t find    "        let (state, reply) = if !allowed.iter().any(|p| p == peer) {\n            (\n                STATE_REJECTED,\n                format!(\"migration query refused: '{peer}' is not in \[cluster] migrate_peers\"),"
dict set targets $t replace "        let (state, reply) = if false {\n            (\n                STATE_REJECTED,\n                format!(\"migration query refused: '{peer}' is not in \[cluster] migrate_peers\"),"
dict set targets $t test "a_migration_query_is_gated_and_answers_from_the_store"
dict set targets $t why  "the query discloses which sessions a node holds — an existence oracle for an unlisted peer"

set t child-refused-without-force
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/migration.rs"
dict set targets $t find    "        if parent_local > 0 && !force_child {"
dict set targets $t replace "        if false {"
dict set targets $t test "migrating_a_child_whose_parent_stays_is_refused_unless_forced"
dict set targets $t why  "question 1: the subtree moves as a unit; a lone child silently breaks the parent link"

set t quiesce-refuses-a-live-turn
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/migration.rs"
dict set targets $t find    "    let busy = is_busy(store, &subtree, 60);\n    if !busy.is_empty() {"
dict set targets $t replace "    let busy = is_busy(store, &subtree, 60);\n    if false {"
dict set targets $t test "prepare_refuses_to_migrate_a_session_with_a_live_turn"
dict set targets $t why  "the turn boundary is the whole protocol: copying a moving session loses whatever the turn writes after the copy"

set t tamper-refused
dict set targets $t pkg  "veles-core"
dict set targets $t file "crates/veles-core/src/migration.rs"
dict set targets $t find    "    if crate::memory::MemoryStore::content_sha(&json) != sha {\n        anyhow::bail!(\"migration: checksum mismatch — the bundle changed in transit\");\n    }"
dict set targets $t replace ""
dict set targets $t test "the_bundle_carries_every_travelling_table_and_blobs"
dict set targets $t why  "the sha fence is what makes A2A inbound safe to parse; without it a modified bundle adopts modified history"

# ── harness ──────────────────────────────────────────────────────────

proc green {pkg t} {
    global repo
    set out [open "|cargo test -q -p $pkg --lib $t 2>@1" r]
    set text [read $out]
    if {[catch {close $out}]} {
        return [list 0 $text]
    }
    # "test result: ok" alone is not enough: a filter that matches NO
    # test prints "ok. 0 passed" and would score every mutation green
    # (the b3a vacuity lesson). Require a nonzero pass count.
    set ran [regexp {([1-9][0-9]*) passed} $text]
    return [list [expr {[string match "*test result: ok*" $text] && $ran}] $text]
}

puts "== B6 mutation harness =="
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
