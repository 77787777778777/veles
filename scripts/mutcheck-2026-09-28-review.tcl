#!/usr/bin/env tclsh
# mutcheck-2026-09-28-review.tcl — mutation harness for the review of
# the merged B3a/B4 work, the background-waiting fix, memory `replace`
# and the TMPDIR leak fixes (2026-09-28).
#
#   nix develop -c tclsh scripts/mutcheck-2026-09-28-review.tcl
#   tclsh scripts/mutcheck-2026-09-28-review.tcl --dry   (patterns only)
#
# The mutations live as RAW TEXT after the __DATA__ line at the end of
# this file, so no pattern is ever a Tcl word. Twice before, the
# quoting was the bug: a braced word keeps `\{`'s backslash (the B3a
# harness matched nothing and scored its total skip as a pass), and in
# a double-quoted word `\[` still starts a command substitution (the
# B4 harness executed `"agent"` as a command). Here there is nothing
# to quote.
#
# Block format:
#   @@ <name> | <file> | <test filter>
#   why: <one line>
#   --- find
#   <exact text, any number of lines>
#   --- replace
#   <exact text>
#   @@ next ...
#
# Rules kept from the earlier harnesses:
#   - every pattern must occur EXACTLY once in its file (checked first,
#     `--dry` stops there); a pattern that occurs twice would mutate two
#     places and prove nothing about either;
#   - the unmutated tree must be green for every target test;
#   - a mutation that does not compile is NOCOMPILE, not a kill;
#   - only red == targets is a pass.
#   - cargo's stderr goes through `2>@1` (in Tcl `2>&1` writes a FILE
#     named &1).
#   - every cargo run gets a per-run TMPDIR under /dev/shm, removed at
#     the end: killed runs leak, and this harness is a killer.

set repo [file normalize [file dirname [file dirname [info script]]]]
set dry [expr {"--dry" in $argv}]

# ── parse the data section ───────────────────────────────────────────
set fh [open [info script] r]
set self [read $fh]
close $fh
set data [string range $self [expr {[string first "\n__DATA__\n" $self] + 10}] end]

set targets [dict create]
set name ""
set mode ""
foreach line [split $data \n] {
    if {[string match "@@ *" $line]} {
        lassign [split [string range $line 3 end] |] n f t
        set name [string trim $n]
        dict set targets $name file [string trim $f]
        dict set targets $name test [string trim $t]
        dict set targets $name find {}
        dict set targets $name replace {}
        dict set targets $name why {}
        set mode ""
        continue
    }
    if {$name eq ""} continue
    if {[string match "why: *" $line] && $mode eq ""} {
        dict set targets $name why [string range $line 5 end]
        continue
    }
    if {$line eq "--- find"} { set mode find; continue }
    if {$line eq "--- replace"} { set mode replace; continue }
    if {$mode ne ""} {
        # (`dict append` takes ONE key; the entry is two levels deep.)
        dict set targets $name $mode "[dict get $targets $name $mode]$line\n"
    }
}
# Blocks are separated by a blank line before the next @@: strip the
# trailing newline(s) the parser appended so the pattern ends where the
# author's text ends.
foreach name [dict keys $targets] {
    foreach k {find replace} {
        set v [dict get $targets $name $k]
        dict set targets $name $k [string trimright $v \n]
    }
}

puts "== review mutation harness =="
puts "repo: $repo"
cd $repo

# ── dry check: every pattern exactly once ────────────────────────────
set bad 0
foreach name [dict keys $targets] {
    set path [file join $repo [dict get $targets $name file]]
    set fh [open $path r]; set orig [read $fh]; close $fh
    set find [dict get $targets $name find]
    set n 0
    set at 0
    while {[set i [string first $find $orig $at]] >= 0} {
        incr n
        set at [expr {$i + 1}]
    }
    if {$find eq "" || $n != 1} {
        puts "PATTERN    $name — found $n time(s) in [dict get $targets $name file]"
        incr bad
    }
}
if {$bad} {
    puts "$bad pattern(s) do not occur exactly once — fix the harness first."
    exit 1
}
puts "dry check: [dict size $targets] patterns, each found exactly once."
if {$dry} { exit 0 }

# ── per-run TMPDIR ───────────────────────────────────────────────────
set base /dev/shm
if {![file writable $base]} { set base /tmp }
set run [file join $base "veles-mutcheck-[pid]"]
file mkdir $run
set ::env(TMPDIR) $run

proc green {t} {
    set out [open "|cargo test -q -p veles-core --lib -- $t 2>@1" r]
    set text [read $out]
    if {[catch {close $out}]} {
        return [list 0 $text]
    }
    return [list [string match "*test result: ok*" $text] $text]
}

set preflight_red 0
foreach name [dict keys $targets] {
    set t [dict get $targets $name test]
    lassign [green $t] ok text
    if {!$ok || [string match "*running 0 tests*" $text]} {
        puts "PREFLIGHT  $name ($t) — red or matches no test"
        incr preflight_red
    }
}
if {$preflight_red} {
    file delete -force $run
    puts "$preflight_red target test(s) not green BEFORE any mutation — fix first."
    exit 1
}
puts "preflight: all target tests green."

set red 0; set escaped 0; set nocompile 0
foreach name [dict keys $targets] {
    set path [file join $repo [dict get $targets $name file]]
    set fh [open $path r]; set orig [read $fh]; close $fh
    set find [dict get $targets $name find]
    set i [string first $find $orig]
    set mutated [string replace $orig $i [expr {$i + [string length $find] - 1}] \
        [dict get $targets $name replace]]
    set fh [open $path w]; puts -nonewline $fh $mutated; close $fh
    lassign [green [dict get $targets $name test]] ok text
    set fh [open $path w]; puts -nonewline $fh $orig; close $fh
    if {[string match "*error\[E*" $text] || [string match "*could not compile*" $text]} {
        puts "NOCOMPILE  $name"
        incr nocompile
    } elseif {!$ok} {
        puts "RED        $name — [dict get $targets $name why]"
        incr red
    } else {
        puts "ESCAPED    $name — [dict get $targets $name why]  << UNDETECTED"
        incr escaped
    }
}
file delete -force $run

set total [dict size $targets]
puts ""
puts "targets: $total  red: $red  escaped: $escaped  nocompile: $nocompile"
if {$red != $total} {
    puts "NOT A PASS: only red == targets is."
    exit 1
}
puts "every mutation detected."
exit 0

__DATA__
@@ batch-parallel | crates/veles-core/src/delegate.rs | a_batch_runs_its_children_in_parallel
why: a batch must run its children in parallel, not one after another
--- find
            for (goal, ctx, placement) in planned {
--- replace
            for (goal, ctx, placement) in planned {
                for h in handles.iter() {
                    while !h.is_finished() {
                        tokio::task::yield_now().await;
                    }
                }

@@ remote-ask-carries-principal | crates/veles-core/src/delegate.rs | a_remote_ask_carries_the_callers_principal
why: the placing node must send the caller's principal with the ask
--- find
            principal: crate::acl::session_principal(&self.session_id)
                .and_then(|p| serde_json::to_string(&*p).ok()),
--- replace
            principal: None,

@@ remote-child-applies-principal | crates/veles-core/src/delegate.rs | a_remote_child_runs_under_the_callers_principal
why: the peer must run the child under the carried principal, not as owner
--- find
        principal.map(str::to_string),
--- replace
        None,

@@ remote-principal-parse-refusal | crates/veles-core/src/delegate.rs | a_remote_child_runs_under_the_callers_principal
why: an unparseable principal must be refused, never widened to owner
--- find
        if let Err(e) = serde_json::from_str::<crate::acl::Principal>(p) {
--- replace
        if let Err(e) = Ok::<(), String>(()) {

@@ task-peers-gate | crates/veles-frontends/src/a2a_server.rs | a_sync_peer_is_not_a_task_peer
why: a sync peer must not be able to run work here
--- find
                    .map(|c| c.cluster.task_peers)
--- replace
                    .map(|c| c.cluster.memsync_peers)

@@ any-only-routable | crates/veles-core/src/placement.rs | any_and_gpu_choose_only_reachable_nodes
why: `any` must not pick a node the work cannot reach
--- find
            let mut live: Vec<NodeRow> = live.into_iter().filter(|n| routable(n)).collect();
--- replace
            let mut live: Vec<NodeRow> = live;

@@ gossip-last-writer-wins | crates/veles-core/src/store.rs | gossip_merge_preserves_local_truths
why: a stale relayed copy must not revert a fresher row
--- find
                     WHERE id = ?1 AND heartbeat_at <= ?14",
--- replace
                     WHERE id = ?1",

@@ binding-never-self | crates/veles-core/src/store.rs | lease_ask_refuses_a_foreign_holder
why: the echo of our own gossip must not bind a peer to this node
--- find
            "SELECT node FROM node_observations WHERE by = ?1 AND node != ?2
--- replace
            "SELECT node FROM node_observations WHERE by = ?1 AND ?2 = ?2

@@ renewal-never-gives-up | crates/veles-core/src/arbiter.rs | a_holder_that_misses_a_renewal_regains_the_lease
why: one refused renewal must not end the lease loop
--- find
                let answer = arbiter.try_acquire_lease(&guard.key, &guard.holder).await;
                guard.apply(sent, answer);
--- replace
                let answer = arbiter.try_acquire_lease(&guard.key, &guard.holder).await;
                let held = matches!(answer, LeaseState::Held(_));
                guard.apply(sent, answer);
                if !held {
                    return;
                }

@@ deadline-from-the-send | crates/veles-core/src/arbiter.rs | a_suspended_holder_resumes_refused
why: the window runs from when the granting ask LEFT, on this node's boot clock
--- find
                    Some(sent + ttl.saturating_sub(margin));
--- replace
                    Some(boot_clock() + ttl.saturating_sub(margin));

@@ lease-ask-checksum | crates/veles-core/src/gossip.rs | a_tampered_lease_ask_is_refused
why: a lease ask whose payload does not match its sha must be refused
--- find
    check_sha(&json, &sha)?;
    serde_json::from_str(&json).context("payload is not a lease ask")
--- replace
    let _ = &sha;
    serde_json::from_str(&json).context("payload is not a lease ask")

@@ sleep-guard | crates/veles-core/src/tools.rs | a_long_foreground_sleep_is_refused_and_short_ones_run
why: a long foreground sleep must be refused
--- find
    if secs < max as f64 {
--- replace
    if secs >= 0.0 {

@@ sleep-tokenizer-background | crates/veles-core/src/guards.rs | longest_foreground_sleep_reads_the_shapes_agents_use
why: a sleep backgrounded with & is not a foreground sleep
--- find
                if j > i + 1 && !backgrounded {
--- replace
                if j > i + 1 {

@@ shell-wait-blocks | crates/veles-core/src/tools.rs | shell_wait_returns_when_the_shell_finishes
why: shell wait must block until the shell finishes
--- find
                let block = action == "wait" || args["block"].as_bool().unwrap_or(false);
--- replace
                let block = false;

@@ shell-wait-drops-duplicate | crates/veles-core/src/tools.rs | shell_wait_returns_when_the_shell_finishes
why: the completion a wait already returned must not be queued again
--- find
                    if crate::delegate::discard_shell_completion(&self.session_id, id) {
--- replace
                    if true {

@@ wake-advice-is-true | crates/veles-core/src/session_wake.rs | wait_advice_follows_whether_the_session_wakes_on_its_own
why: the advice must depend on whether a wake owner holds the session
--- find
        .any(|o| o.own_sid(session_id))
--- replace
        .any(|_| false)

@@ memory-replace-is-an-edit | crates/veles-core/src/memory.rs | replace_swaps_only_old_text_like_an_edit
why: replace must change exactly old_text, not the whole entry
--- find
                                    entry.replacen(old_text, content, 1)
--- replace
                                    content.to_string()

@@ memory-replace-ambiguity | crates/veles-core/src/memory.rs | replace_whole_entries_refuses_ambiguity_and_takes_edit_spellings
why: an old_text that occurs twice in its entry must be refused, not guessed
--- find
                                    if n > 1 {
--- replace
                                    if false {

@@ claude-taskoutput-blocks | crates/veles-core/src/repair.rs | claude_codes_background_task_tools_become_shell_calls
why: Claude Code's TaskOutput blocks by default
--- find
                    .unwrap_or(name == "taskoutput");
--- replace
                    .unwrap_or(false);

@@ kept-dir-atexit | crates/veles-core/src/scratch.rs | a_processs_kept_dirs_are_gone_after_it_exits
why: a test process's kept dirs must be removed when it exits
--- find
            libc::atexit(remove_kept_root);
--- replace
            let _ = remove_kept_root as extern "C" fn();

@@ kept-dir-sweep | crates/veles-core/src/scratch.rs | a_dead_runs_root_is_swept_and_a_live_one_kept
why: a killed run's root must be swept by the next run
--- find
        if !pid_alive(pid) {
--- replace
        if false {

@@ plugin-install-guard | crates/veles-core/src/plugins.rs | install_takes_the_manifest_and_leaves_the_git_behind
why: a refused install must not leave its staging clone in TMPDIR
--- find
    let _staged = RemoveOnDrop(tmp.clone());
--- replace
    let _staged = ();

@@ lsp-private-tmpdir | crates/veles-core/src/lsp_check.rs | a_lint_leaves_its_callers_tmpdir_as_it_found_it
why: tcl-lsp's per-process dirs must not land in the caller's TMPDIR
--- find
            .env("TMPDIR", &tmp)
--- replace
            .env("VELES_UNUSED", &tmp)
