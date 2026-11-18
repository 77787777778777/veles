#!/usr/bin/env tclsh
# Mutation harness for the delegate child's time limit (USER 2026-11-17:
# "this is a priority 0 idiocy, fix this as soon as possible"). Every
# child was killed at a fixed 900 s; now a child runs until it is done
# unless a limit was set, is warned by mail before a set limit, is ASKED
# to stop (and ends its background shells) before it is killed, and its
# last message goes back. Each mutant removes one of those guards and
# must turn its named test RED; the source is restored after every run.
#
#   direnv exec . tclsh scripts/mutcheck-2026-11-17-child-time-limit.tcl [--dry] [a filter]
#
# --dry only checks that every pattern is found in its file — run it
# first: a pattern that never matches reports PATTERN-MISSING, not RED.
# Matching is by `string first`, never a glob.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set DEL   crates/veles-core/src/delegate.rs
set RT    crates/veles-core/src/remotetask.rs
set PLACE crates/veles-core/src/placement.rs
set SH    crates/veles-core/src/shells.rs
set CLI   crates/veles-frontends/src/cli.rs
set LIB {-p veles-core --lib}
set E2E {-p veles-cli --test a_stopped_child_ends_its_shells}

set T_NONE   delegate::tests::a_child_has_no_time_limit_unless_one_is_set
set T_DESC   delegate::tests::the_description_and_schema_say_a_child_has_no_time_limit_by_default
set T_REMOTE delegate::tests::a_remote_ask_carries_the_limit_and_an_unlimited_one_has_no_deadline
set T_LIMIT  delegate::tests::a_child_at_its_limit_is_warned_asked_to_stop_and_its_last_message_returned
set T_GRACE  delegate::tests::a_child_that_ignores_the_stop_is_killed_with_its_group_after_the_grace
set T_CANCEL delegate::tests::a_cancelled_child_is_asked_to_stop_before_it_is_killed
set T_PRUNE  placement::tests::a_long_running_child_keeps_its_own_row_when_it_prunes
set T_SHELLS shells::tests::stopping_a_session_ends_its_shells_trees_and_no_one_elses
set T_END    a_childs_background_shell_ends_with_its_turn
set T_TERM   a_child_asked_to_stop_ends_its_turn_and_its_shells

# name file find repl target test
set mutations [list \
    [list "delegate: a default limit comes back" \
        $DEL \
        "            None => config_minutes.unwrap_or(0)," \
        "            None => config_minutes.unwrap_or(15), // MUTATION" \
        $LIB $T_NONE] \
    [list "delegate: the call's 0 does not win over the config" \
        $DEL \
        "            Some(m) => m," \
        "            Some(m) => if m == 0 \{ config_minutes.unwrap_or(0) \} else \{ m \}, // MUTATION" \
        $LIB $T_NONE] \
    [list "delegate: the description does not say there is no limit" \
        $DEL \
        "limit unless 'time_limit_minutes' sets one. " \
        "" \
        $LIB $T_DESC] \
    [list "delegate: the remote ask drops the limit" \
        $DEL \
        "            time_limit_secs: limit.map(|l| l.limit.as_secs())," \
        "            time_limit_secs: \{ let _ = limit; None \}, // MUTATION" \
        $LIB $T_REMOTE] \
    [list "remotetask: an unlimited remote child gets the old deadline" \
        $RT \
        "    ask.time_limit_secs.map(|s| \{" \
        "    ask.time_limit_secs.or(Some(900)).map(|s| \{ // MUTATION" \
        $LIB $T_REMOTE] \
    [list "delegate: a stop is a kill" \
        $DEL \
        "                            if crate::lifecycle::request_stop(child.id()) \{" \
        "                            if false && crate::lifecycle::request_stop(child.id()) \{" \
        $LIB $T_LIMIT] \
    [list "delegate: a cancel is a kill" \
        $DEL \
        "                            if crate::lifecycle::request_stop(child.id()) \{" \
        "                            if false && crate::lifecycle::request_stop(child.id()) \{" \
        $LIB $T_CANCEL] \
    [list "delegate: no wrap-up mail before the limit" \
        $DEL \
        "                                warn_of_limit(child_session, parent, principal_env.as_deref(), &l);" \
        "                                let _ = parent; // MUTATION" \
        $LIB $T_LIMIT] \
    [list "delegate: a stopped child's last message is not returned" \
        $DEL \
        "            if let Some(m) = last_words(child_session, &stdout) \{" \
        "            if let Some(m) = None::<String> \{ // MUTATION" \
        $LIB $T_LIMIT] \
    [list "delegate: the kill after the grace spares the child's group" \
        $DEL \
        "                        if now.duration_since(since) >= grace \{
                            if let Some(t) = &tree \{
                                t.kill();
                            \}" \
        "                        if now.duration_since(since) >= grace \{
                            // MUTATION: the group is spared" \
        $LIB $T_GRACE] \
    [list "delegate: the verdict waits for the end of the child's pipes" \
        $DEL \
        "        while !self.done.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline" \
        "        while !self.done.load(std::sync::atomic::Ordering::SeqCst)
            && (std::time::Instant::now() < deadline || true) // MUTATION" \
        $LIB $T_LIMIT] \
    [list "placement: a child's prune ages out its own row" \
        $PLACE \
        "    (child_depth == 0).then(std::process::id)" \
        "    \{ let _ = child_depth; Some(std::process::id()) \} // MUTATION" \
        $LIB $T_PRUNE] \
    [list "shells: stopping a session ends every session's shells" \
        $SH \
        "        .filter(|(id, _)| crate::tasks::get(**id).is_some_and(|t| t.session == session_id))" \
        "        // MUTATION: every session's" \
        $LIB $T_SHELLS] \
    [list "shells: stopping a session cancels nothing" \
        $SH \
        "    for (id, _) in &running \{
        crate::tasks::cancel(*id);
    \}" \
        "    // MUTATION: nothing cancelled" \
        $LIB $T_SHELLS] \
    [list "chat -q: a child's turn end leaves its shells running" \
        $CLI \
        "    if veles_core::delegate::child_depth() >= 1 || stop.load(" \
        "    if false && veles_core::delegate::child_depth() >= 1 || stop.load(" \
        $E2E $T_END] \
    [list "chat -q: SIGTERM kills the child where it stands" \
        $CLI \
        "    let stop = spawn_stop_handler(&rt, &boot.agent.session_id);" \
        "    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)); // MUTATION" \
        $E2E $T_TERM] \
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
puts "mutcheck child time limit 2026-11-17: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
