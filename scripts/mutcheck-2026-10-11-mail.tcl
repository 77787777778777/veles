#!/usr/bin/env tclsh
# Mutation harness for session mail (PLAN-sessions-and-phone.md, rounds
# S-a and S-b, 2026-10-11): one session sends text into another over the shared
# queue, nudged through a unix socket per process, and the turn it starts
# runs with the lower of the sender's and the target's rights. Each mutant
# removes one guard and must turn its named test RED; the source is
# restored after every run (written back, so cargo sees a fresh mtime).
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-mail.tcl [--dry] [a filter]
#
# --dry only checks that every pattern is found in its file — run it
# first: a pattern that never matches reports PATTERN-MISSING, not RED.
# Matching is by `string first`, never a glob. The two end-to-end mutants
# rebuild the real binary, so a full run takes a while.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set STORE crates/veles-core/src/store.rs
set MAIL  crates/veles-core/src/mail.rs
set AGENT crates/veles-core/src/agent.rs
set ACL   crates/veles-core/src/acl.rs
set SOCK  crates/veles-core/src/runsock.rs
set PRES  crates/veles-core/src/presence.rs
set TOOL  crates/veles-core/src/sessions_tool.rs
set GW    crates/veles-frontends/src/gateway.rs
set DAPI  crates/veles-frontends/src/desktop_api.rs
set REPL  crates/veles-frontends/src/chat.rs
set CHAT  veles-desktop/lib/chat.tcl
set NET   crates/veles-core/src/mailnet.rs
set A2A   crates/veles-frontends/src/a2a_server.rs
set LIB  {-p veles-core --lib}
set FLIB {-p veles-frontends --lib}
set E2E  {-p veles-cli --test session_mail}
set E2ENET {-p veles-cli --test session_mail_net}

set T_PAIR  mail::tests::a_sent_mail_is_queued_as_one_row_pair_and_claimed_once
set T_TURN  mail::tests::a_mail_turn_runs_clamped_records_its_reply_and_never_runs_twice
set T_LOWER mail::tests::the_turn_runs_with_the_lower_of_sender_and_target
set T_CHAIN mail::tests::a_chain_continues_from_a_mail_turn_and_stops_at_the_cap

# name file find repl target test
set mutations [list \
    [list "store: the drainer never takes a mail row" \
        $STORE \
        "                   AND kind IN ('message', 'mail')" \
        "                   AND kind = 'message' -- MUTATION" \
        $LIB $T_PAIR] \
    [list "store: the take is a DEFERRED transaction again" \
        $STORE \
        "    pub fn queue_take_next(&self, session_id: &str) -> Result<Option<QueueItem>> \{
        let tx = self.write_tx()?;" \
        "    pub fn queue_take_next(&self, session_id: &str) -> Result<Option<QueueItem>> \{
        let tx = self.conn.unchecked_transaction()?; // MUTATION" \
        $LIB store::tests::a_take_waits_out_a_concurrent_writer_instead_of_failing] \
    [list "store: a push is a DEFERRED transaction again" \
        $STORE \
        "    pub fn queue_push_kind(&self, session_id: &str, body: &str, kind: &str) -> Result<i64> \{
        let tx = self.write_tx()?;" \
        "    pub fn queue_push_kind(&self, session_id: &str, body: &str, kind: &str) -> Result<i64> \{
        let tx = self.conn.unchecked_transaction()?; // MUTATION" \
        $LIB store::tests::a_take_waits_out_a_concurrent_writer_instead_of_failing] \
    [list "store: any session may claim a mail" \
        $STORE \
        "             WHERE id = ?1 AND to_session = ?2 AND body_sha = ?3" \
        "             WHERE id = ?1 AND (to_session = ?2 OR 1) AND body_sha = ?3" \
        $LIB $T_PAIR] \
    [list "store: a tampered body still claims the mail" \
        $STORE \
        "             WHERE id = ?1 AND to_session = ?2 AND body_sha = ?3" \
        "             WHERE id = ?1 AND to_session = ?2 AND (body_sha = ?3 OR 1)" \
        $LIB $T_PAIR] \
    [list "mail: an answered body runs again as plain text" \
        $MAIL \
        "                Claim::Spent(row.id)" \
        "                { let _ = row; Claim::Plain } // MUTATION" \
        $LIB $T_TURN] \
    [list "mail: a lower sender does not clamp a higher target" \
        $MAIL \
        "    if s.role < t.role {
        return Some(s.clone());
    }" \
        "    if s.role < t.role {
        return None; // MUTATION
    }" \
        $LIB $T_LOWER] \
    [list "mail: an unparseable sender reads as the owner" \
        $MAIL \
        "serde_json::from_str::<Principal>(j).unwrap_or_else(|_| Principal::guest())" \
        "serde_json::from_str::<Principal>(j).unwrap_or_else(|_| Principal::owner())" \
        $LIB mail::tests::an_unparseable_sender_is_a_guest_never_the_owner] \
    [list "mail: a chain does not advance a hop" \
        $MAIL \
        "        Some(c) => (c.chain_id, c.hop + 1)," \
        "        Some(c) => (c.chain_id, c.hop), // MUTATION" \
        $LIB $T_CHAIN] \
    [list "mail: the hop cap is never reached" \
        $MAIL \
        "    if hop > MAX_HOPS \{" \
        "    if hop > MAX_HOPS + 100 \{ // MUTATION" \
        $LIB $T_CHAIN] \
    [list "mail: a wait leaves the waiting flag set" \
        $MAIL \
        "        let _ = self.store.lock_ok().mail_set_waiting(self.id, false);" \
        "        // MUTATION: the flag stays" \
        $LIB mail::tests::a_wait_returns_the_reply_when_the_turn_ends] \
    [list "mail: the provenance row says nothing of mail" \
        $MAIL \
        "\"\[origin\] this message is mail from \{who\}\{node\}" \
        "\"\[origin\] this message arrived from \{who\}\{node\}" \
        $LIB $T_TURN] \
    [list "agent: the mail turn is never clamped" \
        $AGENT \
        "                agent.set_principal(p);
                Some(current)" \
        "                let _ = p; // MUTATION
                Some(current)" \
        $LIB $T_TURN] \
    [list "agent: the clamp outlives the turn" \
        $AGENT \
        "            self.agent.replace_principal(prev.map(|p| (*p).clone()));" \
        "            let _ = prev; // MUTATION" \
        $LIB $T_TURN] \
    [list "acl: the sessions tool reaches a session ungated" \
        $ACL \
        "        \"sessions\" => All," \
        "        // MUTATION: unclassified" \
        $LIB tools::tests::every_registered_tool_is_acl_classified] \
    [list "runsock: a dead process's socket is kept" \
        $SOCK \
        "        if !pid_alive(pid) \{" \
        "        if false && !pid_alive(pid) \{ // MUTATION" \
        $LIB runsock::tests::a_dead_pids_socket_is_swept_and_a_live_one_is_kept] \
    [list "runsock: a wide run dir stays wide" \
        $SOCK \
        "    if meta.permissions().mode() & 0o077 != 0 \{" \
        "    if false && meta.permissions().mode() & 0o077 != 0 \{ // MUTATION" \
        $LIB runsock::tests::the_run_dir_is_private_even_when_it_existed_wider] \
    [list "presence: a nudge needs every source to hold the session" \
        $PRES \
        "    srcs.iter().any(|s| s.nudge(session_id))" \
        "    srcs.iter().all(|s| s.nudge(session_id)) // MUTATION" \
        $LIB presence::tests::a_nudge_reaches_the_source_that_holds_the_session_and_no_other] \
    [list "sessions tool: check reads anyone's mail" \
        $TOOL \
        "        if !mine \{" \
        "        if false && !mine \{ // MUTATION" \
        $LIB sessions_tool::tests::check_reads_only_this_sessions_mail] \
    [list "gateway: a parked row goes back with an empty sender" \
        $GW \
        "            sender_id: principal.and_then(|p| p.id.clone()),
            sender_ctx," \
        "            sender_id: principal.and(None), // MUTATION
            sender_ctx: \{ let _ = sender_ctx; veles_core::acl::SenderContext::default() \}," \
        $FLIB gateway::tests::a_parked_row_is_handed_back_as_the_sessions_own_sender] \
    [list "desktop: opening a session leaves its waiting mail queued" \
        $DAPI \
        "    if mail_waiting && srv.holds_for_mail(&id) \{" \
        "    if false && mail_waiting && srv.holds_for_mail(&id) \{ // MUTATION" \
        $E2E the_cli_mails_a_gateway_held_desktop_session_and_gets_its_answer] \
    [list "repl: an empty Enter runs nothing" \
        $REPL \
        "            // An empty Enter runs whatever waits in the queue.
            drain_queue(&agent, &history, &cancel, &turn_active);" \
        "            // MUTATION: an empty Enter runs nothing" \
        $E2E an_idle_repl_is_told_of_mail_and_an_empty_enter_runs_it] \
    [list "mailnet: a peer outside task_peers is served" \
        $NET \
        "    if !task_peers.iter().any(|p| p == peer) \{" \
        "    if false && !task_peers.iter().any(|p| p == peer) \{ // MUTATION" \
        $LIB mailnet::tests::a_peer_outside_task_peers_is_refused_and_a_listed_one_is_capped] \
    [list "mailnet: the lowest of two principals is the higher" \
        $NET \
        "    let (lo, hi) = if a.role <= b.role \{ (a, b) \} else \{ (b, a) \};" \
        "    let (lo, hi) = if a.role >= b.role \{ (a, b) \} else \{ (b, a) \}; // MUTATION" \
        $LIB mailnet::tests::the_lowest_of_two_principals_never_rises] \
    [list "mailnet: a sender's claim is taken as given" \
        $NET \
        "        Some(c) => lowest(c, peer_principal)," \
        "        Some(c) => \{ let _ = peer_principal; c.clone() \} // MUTATION" \
        $LIB mailnet::tests::a_claimed_principal_only_lowers_and_garbage_is_a_guest] \
    [list "mailnet: a garbled claim reads as the owner" \
        $NET \
        ".map(|j| serde_json::from_str::<Principal>(j).unwrap_or_else(|_| Principal::guest()));" \
        ".map(|j| serde_json::from_str::<Principal>(j).unwrap_or_else(|_| Principal::owner()));" \
        $LIB mailnet::tests::a_claimed_principal_only_lowers_and_garbage_is_a_guest] \
    [list "mailnet: check answers any peer" \
        $NET \
        "                Ok(Some(row)) if row.via_peer.as_deref() == Some(peer) => json!(\{" \
        "                Ok(Some(row)) if row.via_peer.is_some() || true => json!(\{ // MUTATION" \
        $LIB mailnet::tests::a_peer_outside_task_peers_is_refused_and_a_listed_one_is_capped] \
    [list "mail: a relayed hop restarts at zero" \
        $MAIL \
        "        Some(r) => (r.chain_id.clone().unwrap_or_else(|| id.clone()), r.hop)," \
        "        Some(r) => (r.chain_id.clone().unwrap_or_else(|| id.clone()), 0), // MUTATION" \
        $LIB mailnet::tests::the_hop_cap_holds_across_nodes] \
    [list "mail: a relayed sender is answered at the bare id" \
        $MAIL \
        "        Some(peer) => format!(\"\{peer\}/\{sid\}\")," \
        "        Some(peer) => \{ let _ = peer; sid.to_string() \} // MUTATION" \
        $LIB mail::tests::relayed_mail_names_its_node_and_its_reply_address] \
    [list "a2a: session mail is not dispatched" \
        $A2A \
        "        if veles_core::mailnet::is_ask(&raw_text) \{" \
        "        if false && veles_core::mailnet::is_ask(&raw_text) \{ // MUTATION" \
        $E2ENET node_a_mails_a_session_node_b_holds_and_gets_the_answer] \
    [list "desktop: the provenance row paints as a note" \
        $CHAT \
        "    if \{\$role eq \"user\" && \$inj eq \"origin:mail\"\} \{ return \}" \
        "    # MUTATION: origin:mail paints" \
        TCL veles-desktop/lib/rendertest.tcl] \
    [list "desktop: mail paints as a plain injection" \
        $CHAT \
        "        ::vchat::_mail_note \$inj \$content \$at
        return" \
        "        # MUTATION: falls through to the generic injection label" \
        TCL veles-desktop/lib/rendertest.tcl] \
]

proc run_cmd {argv} {
    set f [open "| direnv exec . $argv </dev/null 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
    return $out
}

# A private Xvfb for the TCL target, started on first use.
set ::xvfb ""
proc xvfb_display {} {
    if {$::xvfb ne ""} { return $::xvfb }
    for {set n 171} {$n < 199} {incr n} {
        if {![file exists /tmp/.X$n-lock]} { break }
    }
    set ::xvfb_pid [exec Xvfb :$n -screen 0 1024x768x24 >& /dev/null &]
    for {set i 0} {$i < 100 && ![file exists /tmp/.X$n-lock]} {incr i} { after 100 }
    set ::xvfb :$n
    return $::xvfb
}

proc run_tcl_suite {suite} {
    set f [open "| env DISPLAY=[xvfb_display] XVFB_RUNNING=1 direnv exec . wish $suite </dev/null 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    set rc [catch { close $f }]
    if {![regexp {([0-9]+) passed, ([0-9]+) failed} $out -> p fl]} {
        # No summary: the suite died before its last line. That is RED
        # only when it died of a Tcl error the mutant can explain (a
        # dialog the mutated code destroyed early, a missing window) —
        # never for a suite that could not start at all.
        set log [file join [expr {[info exists ::env(TMPDIR)] ? $::env(TMPDIR) : "/tmp"}] mutcheck-tcl-last.log]
        catch { set fh [open $log w]; puts $fh $out; close $fh }
        if {$rc && [regexp {bad window path name|invalid command name|can't read|while executing} $out]} {
            return "RED"
        }
        return "NO-SUCH-TEST (output in $log)"
    }
    return [expr {($fl > 0 || $rc) ? "RED" : "GREEN"}]
}

proc run_test {target filter} {
    if {$target eq "TCL"} { return [run_tcl_suite $filter] }
    set out [run_cmd "cargo test $target $filter"]
    if {[regexp {error\[E[0-9]+\]|could not compile} $out]} { return "NOCOMPILE" }
    if {[regexp {process didn't exit successfully|\(signal: 6|SIGABRT} $out]} { return "RED" }
    if {![regexp {running ([1-9][0-9]*) tests?} $out]} { return "NO-SUCH-TEST" }
    return [expr {[string match "*test result: FAILED*" $out] ? "RED" : "GREEN"}]
}

proc read_file {file} {
    set fh [open $file r]
    fconfigure $fh -encoding utf-8 -translation binary
    set s [read $fh]
    close $fh
    return [encoding convertfrom utf-8 $s]
}

proc write_file {file s} {
    set fh [open $file w]
    fconfigure $fh -translation binary
    puts -nonewline $fh [encoding convertto utf-8 $s]
    close $fh
}

proc apply_and_run {file find repl target filter} {
    set orig [read_file $file]
    if {[string first $find $orig] < 0} { return PATTERN-MISSING }
    if {$::dry} { return FOUND }
    write_file $file [string map [list $find $repl] $orig]
    set verdict [catch {run_test $target $filter} r]
    write_file $file $orig
    if {$verdict} { return "ERROR: $r" }
    return $r
}

set dry 0
if {[lindex $argv 0] eq "--dry"} { set dry 1; set argv [lrange $argv 1 end] }
set filter [lindex $argv 0]
set verified 0
set failures 0
foreach m $mutations {
    lassign $m name file find repl target test
    if {$filter ne "" && [string first $filter $name] < 0} { continue }
    puts -nonewline "[format %-72s $name] "
    flush stdout
    set r [apply_and_run $file $find $repl $target $test]
    puts $r
    if {$r eq "RED" || ($dry && $r eq "FOUND")} { incr verified } else { incr failures }
}
if {[info exists ::xvfb_pid]} { catch { exec kill $::xvfb_pid } }
puts ""
puts "mutcheck session mail 2026-10-11: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
