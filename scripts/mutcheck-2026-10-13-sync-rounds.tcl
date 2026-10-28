#!/usr/bin/env tclsh
# Mutation harness for sync questions that outlive the node asking them
# (askrelay rounds, syncask's book; USER 2026-10-13: "(b)" — the node that
# put the question to the owner keeps the answer, the asker collects it
# at its next start), and for the Telegram button that says a question
# is no longer waiting instead of "not yours to answer". Each mutant
# removes one piece and must turn its named test RED; the source is
# restored after every run.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-13-sync-rounds.tcl [--dry] [a filter]

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set AR crates/veles-core/src/askrelay.rs
set SA crates/veles-core/src/syncask.rs
set TG crates/veles-telegram/src/lib.rs
set LIB {-p veles-core --lib}
set TGLIB {-p veles-telegram --lib}

set mutations [list \
    [list "an answer given while its node was away is not handed over" \
        $AR \
        "        if let Some((a, _)) = r.answered.get(&round) \{" \
        "        if let Some((a, _)) = None::<&(AskAnswer, u64)> \{ // MUTATION" \
        $LIB askrelay::tests::a_rounds_answer_waits_for_the_node_that_asked] \
    [list "an open round is put to the owner again" \
        $AR \
        "        if let Some((question, rx)) = r.open.get(&round) \{" \
        "        if let Some((question, rx)) = None::<&(String, tokio::sync::watch::Receiver<Option<AskAnswer>>)> \{ // MUTATION" \
        $LIB askrelay::tests::a_rounds_answer_waits_for_the_node_that_asked] \
    [list "the owner's answer is not kept" \
        $AR \
        "                    if !a.timed_out \{\n                        r.answered.insert" \
        "                    if false \{ // MUTATION\n                        r.answered.insert" \
        $LIB askrelay::tests::a_rounds_answer_waits_for_the_node_that_asked] \
    [list "a round out at a restart waits hours" \
        $SA \
        "                        && book.get(&id).is_some_and(|e| e.round.is_some())" \
        "                        && false // MUTATION" \
        $LIB syncask::tests::a_round_out_at_a_restart_is_asked_again_at_once_in_the_same_round] \
    [list "every ask is a new round" \
        $SA \
        "            .and_then(|e| e.round.clone())\n            .unwrap_or_else(|| \{" \
        "            .and_then(|_| None::<String>) // MUTATION\n            .unwrap_or_else(|| \{" \
        $LIB syncask::tests::a_round_out_at_a_restart_is_asked_again_at_once_in_the_same_round] \
    [list "the question does not name its round" \
        $SA \
        "            req.relay_key = Some(round);" \
        "            let _ = round; // MUTATION" \
        $LIB syncask::tests::a_round_out_at_a_restart_is_asked_again_at_once_in_the_same_round] \
    [list "a used answer does not end the round" \
        $SA \
        "            // asked again is a new question.\n            book_end_round(&id);" \
        "            // asked again is a new question. MUTATION" \
        $LIB syncask::tests::later_waits_a_day_and_no_drops_the_offer] \
    [list "a retry starts a new round" \
        $SA \
        "    let round = b.get(id).and_then(|e| e.round.clone());" \
        "    let round = None; // MUTATION" \
        $LIB syncask::tests::an_undelivered_question_is_retried_soon] \
    [list "a tap on a closed question is refused as someone else's" \
        $TG \
        "    if !open(id) \{\n        return TapAction::Gone;\n    \}" \
        "    let _ = &open; // MUTATION" \
        $TGLIB tests::every_tap_shape_is_gated_on_who_pressed_it] \
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
    if {$target eq "SH"} {
        set rc [catch { exec env TMPDIR=/dev/shm bash $filter </dev/null >& /dev/null }]
        return [expr {$rc ? "RED" : "GREEN"}]
    }
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
puts "mutcheck sync rounds 2026-10-13: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
