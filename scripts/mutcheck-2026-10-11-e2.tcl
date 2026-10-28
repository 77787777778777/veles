#!/usr/bin/env tclsh
# Mutation harness for the `failover.decide` override point
# (PLAN-extension-api.md E2). Each mutant removes one guard and must turn
# its named test RED; the source is restored after every run. Verdicts as
# in scripts/mutcheck-2026-10-10-signal-pin.tcl: a timeout-killed run, a
# process that ends without a verdict, and a non-zero exit are all RED.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-e2.tcl [a filter]
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-e2.tcl --dry
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set AGENT    crates/veles-core/src/agent.rs
set FAILOVER crates/veles-core/src/failover.rs
set EXAMPLE  examples/plugins/failover-backoff/plugin.ts
set CORE {-p veles-core --lib}
set MHT  {-p veles-cli --test module_host}
set T_ADVANCE {agent::tests::a_failover_override_retries_then_advances_with_no_rule_at_all}
set T_GOTO    {agent::tests::a_failover_goto_takes_a_configured_route_and_refuses_another}
set T_FOREVER {agent::tests::a_failover_override_stops_with_its_reason_and_cannot_hold_a_turn_forever}
set T_NONSTR  {agent::tests::the_nonstream_chain_asks_failover_decide_too}
set T_PARSE   {failover::tests::every_action_the_point_allows_parses}
set T_CORE    {failover::tests::core_answers_its_own_point_with_the_configured_policy}
set T_EXAMPLE {the_failover_example_plugin_decides_as_documented}
# name file find repl target test
set mutations [list \
    [list {the override is never asked} \
        $AGENT "        if crate::services::overridden(crate::failover::POINT) \{" \
        "        if false && crate::services::overridden(crate::failover::POINT) \{" \
        $CORE $T_ADVANCE] \
    [list {an override's hop goes through the policy's entry gate} \
        $AGENT "                    override_hop = from_override;" \
        "                    override_hop = false && from_override; // MUTATION" \
        $CORE $T_ADVANCE] \
    [list {the retry cap is ten times higher} \
        $AGENT "            if tries >= crate::failover::MAX_TRIES \{" \
        "            if tries >= crate::failover::MAX_TRIES * 10 \{" \
        $CORE $T_FOREVER] \
    [list {a goto to an unconfigured route is taken as route 0} \
        $AGENT "                            PointAnswer::Policy\n" \
        "                            PointAnswer::Act(FailoverAction::Goto(0)) // MUTATION\n" \
        $CORE $T_GOTO] \
    [list {the non-streaming chain ignores the point} \
        $AGENT "            if crate::services::overridden(crate::failover::POINT)\n" \
        "            if false && crate::services::overridden(crate::failover::POINT)\n" \
        $CORE $T_NONSTR] \
    [list {a wait is not capped} \
        $FAILOVER "            Verdict::Wait(secs.min(MAX_WAIT_SECS))" "            Verdict::Wait(secs)" \
        $CORE $T_PARSE] \
    [list {core's own answer is not policy} \
        $FAILOVER "                Ok(serde_json::json!(\{\"action\": \"policy\"\}))" \
        "                Ok(serde_json::json!(\{\"action\": \"stop\"\}))" \
        $CORE $T_CORE] \
    [list {a rate limit is named a server error} \
        $FAILOVER "        E::RateLimited(_) => \"rate_limited\"," "        E::RateLimited(_) => \"server\"," \
        $CORE $T_ADVANCE] \
    [list {the example waits a third time} \
        $EXAMPLE "const WAITS = \[30, 60\];" "const WAITS = \[30, 60, 90\];" \
        $MHT $T_EXAMPLE] \
]
proc run_cmd {argv} {
    set rc 0
    if {[catch {exec timeout -s KILL 900 sh -c "direnv exec . $argv 2>&1" </dev/null} out]} {
        if {[lindex $::errorCode 0] eq "CHILDSTATUS"} {
            set rc [lindex $::errorCode 2]
        } elseif {[lindex $::errorCode 0] eq "CHILDKILLED"} {
            set rc 137
        } else {
            set rc -1
        }
    }
    return [list $rc $out]
}
proc run_test {target filter} {
    lassign [run_cmd "cargo test $target -- $filter"] rc out
    if {[regexp {error\[E[0-9]+\]|could not compile} $out]} { return "NOCOMPILE" }
    if {$rc == 124 || $rc == 137} { puts -nonewline "(killed by the timeout) "; return "RED" }
    if {[regexp {process didn't exit successfully|\(signal: 6|SIGABRT|\(exit status: [1-9]} $out]} { return "RED" }
    if {![regexp {running ([1-9][0-9]*) tests?} $out]} { return "NO-SUCH-TEST-OR-KILLED" }
    if {[string match "*test result: FAILED*" $out]} { return "RED" }
    if {![string match "*test result: ok*" $out]} {
        puts -nonewline "(the test process ended without a verdict) "
        return "RED"
    }
    return "GREEN"
}
proc apply_and_run {file find repl target filter} {
    set fh [open $file r]
    fconfigure $fh -encoding utf-8
    set orig [read $fh]
    close $fh
    set at [string first $find $orig]
    if {$at < 0} { return PATTERN-MISSING }
    set mutated "[string range $orig 0 [expr {$at - 1}]]$repl[string range $orig [expr {$at + [string length $find]}] end]"
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh $mutated
    close $fh
    set verdict [catch {run_test $target $filter} r]
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh $orig
    close $fh
    if {$verdict} { return "ERROR: $r" }
    return $r
}
proc baseline {} {
    foreach {target filter} [list $::CORE "agent::tests::a_failover agent::tests::the_nonstream failover::" \
                                  $::MHT $::T_EXAMPLE] {
        set v [run_test $target $filter]
        if {$v ne "GREEN"} {
            puts "baseline is $v for [list $target $filter]: fix the tree first"
            exit 2
        }
    }
    puts "baseline: every check GREEN"
}
set filter ""
set dry 0
foreach a $argv {
    if {$a eq "--dry"} { set dry 1 } else { set filter $a }
}
if {!$dry} { baseline }
set red 0
set total 0
foreach m $mutations {
    lassign $m name file find repl target test
    if {$filter ne "" && [string first $filter $name] < 0} { continue }
    incr total
    if {$dry} {
        set fh [open $file r]
        fconfigure $fh -encoding utf-8
        set src [read $fh]
        close $fh
        set ok [expr {[string first $find $src] >= 0}]
        puts [format "%-66s %s" $name [expr {$ok ? "ok" : "PATTERN-MISSING"}]]
        if {$ok} { incr red }
        continue
    }
    puts -nonewline [format "%-66s " $name]
    flush stdout
    set v [apply_and_run $file $find $repl $target $test]
    puts $v
    if {$v eq "RED"} { incr red }
}
puts "$red/$total [expr {$dry ? "patterns match" : "RED"}]"
exit [expr {$red == $total ? 0 : 1}]
