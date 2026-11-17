#!/usr/bin/env tclsh
# Mutation harness for session mail that reaches a RUNNING turn
# (USER 2026-11-17: "Why do I have to stop and start a turn to deliver
# session mail? ... It should arrive at the nearest tool call result
# probably but also there should be a tool for sessions to fetch their
# own mail"), and for the desktop drain that ran every queued turn after
# a Stop with the stopped turn's token. Each mutant removes one guard and
# must turn its named test RED; the source is restored after every run.
#
#   direnv exec . tclsh scripts/mutcheck-2026-11-17-midturn-mail.tcl [--dry] [a filter]
#
# --dry only checks that every pattern is found in its file — run it
# first: a pattern that never matches reports PATTERN-MISSING, not RED.
# Matching is by `string first`, never a glob.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set STORE crates/veles-core/src/store.rs
set MAIL  crates/veles-core/src/mail.rs
set AGENT crates/veles-core/src/agent.rs
set TOOL  crates/veles-core/src/sessions_tool.rs
set DAPI  crates/veles-frontends/src/desktop_api.rs
set LIB  {-p veles-core --lib}
set FLIB {-p veles-frontends --lib}

set T_TAKE   mail::tests::a_running_turn_takes_only_the_mail_whose_sender_covers_its_rights
set T_BACK   mail::tests::mail_a_stopped_turn_took_and_did_not_answer_goes_back_in_the_queue
set T_ORPH   mail::tests::mail_left_running_by_a_dead_turn_is_failed_when_the_next_turn_ends
set T_ARRIVE mail::tests::mail_reaches_a_running_turn_at_its_next_tool_result
set T_OFF    mail::tests::with_mail_injection_off_mail_waits_for_the_turn_to_end
set T_STOP   mail::tests::mail_taken_into_a_turn_that_is_stopped_is_queued_again
set T_SWEEP  mail::tests::a_turn_ending_fails_mail_an_earlier_turn_left_running
set T_INBOX  sessions_tool::tests::inbox_fetches_what_may_enter_this_turn_and_reply_answers_it_once
set T_DRAIN  desktop_api::tests::a_stop_ends_only_the_turn_it_stops_and_the_queue_still_runs

# name file find repl target test
set mutations [list \
    [list "agent: mail never reaches a running turn" \
        $AGENT \
        "                    self.take_mail_into_turn(history);" \
        "                    // MUTATION: mail waits for the turn's end" \
        $LIB $T_ARRIVE] \
    [list "agent: the switch is ignored" \
        $AGENT \
        "        if !self.cfg.agent.injections.mail \{
            return;
        \}" \
        "        // MUTATION: the switch is ignored" \
        $LIB $T_OFF] \
    [list "agent: mail enters without its provenance" \
        $AGENT \
        "ChatMessage::user(&crate::mail::arrival_origin_line(&t.row))" \
        "ChatMessage::user(\"\") /* MUTATION */" \
        $LIB $T_ARRIVE] \
    [list "agent: a stopped turn counts as answered" \
        $AGENT \
        "            Ok(r) if r.interrupted => Err(" \
        "            Ok(r) if false && r.interrupted => Err(" \
        $LIB $T_STOP] \
    [list "agent: a turn's end never sweeps" \
        $AGENT \
        "        crate::mail::sweep_orphans(&self.store, &self.session_id, &started);" \
        "        let _ = &started; // MUTATION" \
        $LIB $T_SWEEP] \
    [list "mail: any sender covers any turn" \
        $MAIL \
        "    turn_principal(sender, running).is_none()" \
        "    \{ let _ = (sender, running); true \} // MUTATION" \
        $LIB $T_ARRIVE] \
    [list "mail: a join line is taken in like mail" \
        $MAIL \
        "        if row.kind != KIND_MAIL
            || row.to_session != session_id" \
        "        if false
            || row.to_session != session_id" \
        $LIB $T_TAKE] \
    [list "mail: the chain marker never follows taken mail" \
        $MAIL \
        "            enter(session_id, &t.row);
            entry.entered = true;" \
        "            entry.entered = true; // MUTATION" \
        $LIB $T_TAKE] \
    [list "mail: a waiting mail is told every round" \
        $MAIL \
        "        h.fresh = entry.seen_held.insert(h.row.id.clone());" \
        "        h.fresh = \{ entry.seen_held.insert(h.row.id.clone()); true \}; // MUTATION" \
        $LIB $T_TAKE] \
    [list "mail: a stopped turn's mail is never put back" \
        $MAIL \
        "                let back = store.lock_ok().mail_requeue(&t.row.id, session_id, &t.body);" \
        "                let back = Ok::<bool, anyhow::Error>(false); // MUTATION" \
        $LIB $T_BACK] \
    [list "store: a take leaves the queue row" \
        $STORE \
        "            \"DELETE FROM session_queue
             WHERE session_id = ?1 AND seq = ?2 AND state = 'pending' AND kind = 'mail'\"," \
        "            \"UPDATE session_queue SET state = state
             WHERE session_id = ?1 AND seq = ?2 AND state = 'pending' AND kind = 'mail'\"," \
        $LIB $T_TAKE] \
    [list "store: a mail the queue will re-run is swept" \
        $STORE \
        "               AND NOT EXISTS (" \
        "               AND 0 = 0 OR NOT EXISTS (" \
        $LIB $T_ORPH] \
    [list "store: a mail claimed after the turn began is swept" \
        $STORE \
        "m.state = 'running' AND m.updated_at < ?2" \
        "m.state = 'running' AND (m.updated_at < ?2 OR 1)" \
        $LIB $T_ORPH] \
    [list "sessions tool: reply answers another session's mail" \
        $TOOL \
        "        if row.to_session != self.session_id \{
            return format!(\"error: mail \{id\} was not sent to this session\");" \
        "        if false && row.to_session != self.session_id \{
            return format!(\"error: mail \{id\} was not sent to this session\");" \
        $LIB $T_INBOX] \
    [list "sessions tool: reply answers mail still waiting" \
        $TOOL \
        "            \"running\" => \{\}" \
        "            \"running\" | \"queued\" => \{\} // MUTATION" \
        $LIB $T_INBOX] \
    [list "desktop: queued turns share the stopped turn's token" \
        $DAPI \
        "            let cancel = CancelToken::new();
            let _ = veles_core::i18n::scope(
                ui_locale.clone(),
                agent.run_turn(&mut history, &body, Some(cancel), |_| \{\})," \
        "            let cancel = \{
                static SHARED: std::sync::OnceLock<CancelToken> = std::sync::OnceLock::new();
                SHARED.get_or_init(CancelToken::new).clone()
            \}; // MUTATION
            let _ = veles_core::i18n::scope(
                ui_locale.clone(),
                agent.run_turn(&mut history, &body, Some(cancel), |_| \{\})," \
        $FLIB $T_DRAIN] \
]
proc run_cmd {argv} {
    set f [open "| direnv exec . $argv </dev/null 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
    return $out
}



proc run_test {target filter} {

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
puts ""
puts "mutcheck mid-turn session mail 2026-11-17: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
