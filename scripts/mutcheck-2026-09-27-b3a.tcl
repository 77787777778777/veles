#!/usr/bin/env tclsh
# mutcheck-2026-09-27-b3a.tcl — mutation harness for the B3a round
# (arbiter leases, gossip, singleton fence, /nodes).
#
# House rules (learned the hard way, see PENDING 2026-09-27):
#   - cargo stderr goes through `2>@1`, NEVER `2>&1` (in Tcl that
#     writes a FILE named &1 and a non-compiling mutation looks like
#     "no tests ran", which we would have scored RED = detected).
#   - a mutation that fails to COMPILE is NOCOMPILE — the harness says
#     so instead of pretending a tally proved anything.
#   - every mutation must make at least one targeted test go RED while
#     the unmutated tree is GREEN (run once at start).
#   - the find/replace patterns are Rust source that ends in a bare `{`.
#     A braced word cannot hold an unbalanced brace (and `\{` inside
#     braces keeps a literal backslash that never matches), so each
#     pattern is a top-level double-quoted word via `dict set`.

set repo [file normalize [file dirname [file dirname [info script]]]]

set targets [dict create]

set t fail-closed-grant
dict set targets $t file    "crates/veles-core/src/store.rs"
dict set targets $t find    "if held_by != holder && exp > now {"
dict set targets $t replace "if false && held_by != holder && exp > now {"
dict set targets $t test    "leases_grant_renew_refuse_and_expire"
dict set targets $t why     "the second holder must be refused — without the guard, two pollers"

set t expiry-reads-unheld
dict set targets $t file    "crates/veles-core/src/store.rs"
dict set targets $t find    "if exp > now {"
dict set targets $t replace "if true {"
dict set targets $t test    "leases_grant_renew_refuse_and_expire"
dict set targets $t why     "an expired row must read as nobody's lease"

set t guard-expiry-check
dict set targets $t file    "crates/veles-core/src/arbiter.rs"
dict set targets $t find    "Ok(exp) => chrono::Utc::now() < exp.with_timezone(&chrono::Utc),"
dict set targets $t replace "Ok(_exp) => true,"
dict set targets $t test    "a_suspended_holder_resumes_refused"
dict set targets $t why     "a suspended process must not act on a remembered lease"

set t renewal-loss-marking
dict set targets $t file    "crates/veles-core/src/arbiter.rs"
dict set targets $t find    "guard.mark_lost(&format!(\"{other:?}\"));"
dict set targets $t replace "/* mutation: swallow the loss */ let _ = other;"
dict set targets $t test    "renewal_loop_refreshes_then_loses"
dict set targets $t why     "a failed renewal must flip held() to false"

set t gossip-allowlist
dict set targets $t file    "crates/veles-core/src/gossip.rs"
dict set targets $t find    "if !cfg.cluster.memsync_peers.iter().any(|p| p == peer) {"
dict set targets $t replace "if false {"
dict set targets $t test    "an_exchange_converges_both_registries"
dict set targets $t why     "unnamed peers must not merge registry rows"

set t gossip-checksum
dict set targets $t file    "crates/veles-core/src/gossip.rs"
dict set targets $t find    "if crate::memory::MemoryStore::content_sha(json) != sha {"
dict set targets $t replace "if false {"
dict set targets $t test    "a_tampered_payload_is_refused"
dict set targets $t why     "a tampered payload must be refused"

set t observation-recorded
dict set targets $t file    "crates/veles-core/src/gossip.rs"
dict set targets $t find    "let _ = store.node_observe(&msg.from_node, client_addr, peer);"
dict set targets $t replace "/* mutation: no observation */"
dict set targets $t test    "an_exchange_converges_both_registries"
dict set targets $t why     "the reflexive address is the static-IP evidence — it must land"

set t merge-keeps-birthday
dict set targets $t file    "crates/veles-core/src/store.rs"
dict set targets $t find    "heartbeat_at = MAX(heartbeat_at, ?11)"
dict set targets $t replace "heartbeat_at = ?11, registered_at = '1999-01-01 00:00:00'"
dict set targets $t test    "gossip_merge_preserves_local_truths"
dict set targets $t why     "a peer's stale copy must not reset the birthday"

set t own-row-protection
dict set targets $t file    "crates/veles-core/src/store.rs"
dict set targets $t find    "if local_id.as_deref() == Some(row.id.as_str()) {"
dict set targets $t replace "if false {"
dict set targets $t test    "own_row_is_never_rewritten_by_gossip"
dict set targets $t why     "a peer's copy of us must not rename our row"

set t cron-lease-gate
dict set targets $t file    "crates/veles-core/src/cron.rs"
dict set targets $t find    "if !self.lease_held() {"
dict set targets $t replace "if false {"
dict set targets $t test    "cron_tick_skips_without_a_lease"
dict set targets $t why     "a lost lease must skip the tick — never fire on memory"

set t nodes-command-registered
dict set targets $t file    "crates/veles-frontends/src/commands.rs"
dict set targets $t find    "\"/nodes\" => Some({"
dict set targets $t replace "\"/nodesx\" => Some({"
dict set targets $t test    "every_dispatcher_arm_is_declared_and_tiered"
dict set targets $t why     "the /nodes arm must exist under its own name"

set t lease-answer-binding
dict set targets $t file    "crates/veles-frontends/src/a2a_server.rs"
dict set targets $t find    "Some(bound) if bound == ask.holder => {"
dict set targets $t replace "Some(bound) if true => {"
dict set targets $t test    "lease_ask_refuses_a_foreign_holder"
dict set targets $t why     "a peer must only ask for its OWN node's lease"

set t not-the-arbiter-refusal
dict set targets $t file    "crates/veles-frontends/src/a2a_server.rs"
dict set targets $t find    "let (state, reply) = if !me_arbiter {"
dict set targets $t replace "let (state, reply) = if false {"
dict set targets $t test    "lease_ask_refused_when_not_arbiter"
dict set targets $t why     "only the arbiter may serve lease asks"

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

puts "== B3a mutation harness =="
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
puts "prelight: all target tests green."

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
