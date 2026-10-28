#!/usr/bin/env tclsh
# Mutation harness for the `prompt.build`, `tool.dispatch` and
# `inbound.route` override points (PLAN-extension-api.md E3). Each mutant
# removes one guard and must turn its named test RED; the source is
# restored after every run. Verdicts as in
# scripts/mutcheck-2026-10-10-signal-pin.tcl.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-e3.tcl [a filter]
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-e3.tcl --dry
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set PROMPT  crates/veles-core/src/prompt.rs
set TOOLS   crates/veles-core/src/tools.rs
set POINTS  crates/veles-core/src/points.rs
set GATEWAY crates/veles-frontends/src/gateway.rs
set CORE {-p veles-core --lib}
set FRONT {-p veles-frontends --lib}
set T_PROMPT  {points::tests::prompt_build_is_wrapped_replaced_and_survives_a_broken_override}
set T_TOOL    {points::tests::tool_dispatch_is_wrapped_answered_and_rewritten_through_next}
set T_IDENT   {points::tests::the_inbound_identity_check_names_the_changed_field}
set T_INBOUND {gateway::tests::an_inbound_override_rewrites_drops_and_cannot_change_the_sender}
# name file find repl target test
set mutations [list \
    [list {the prompt point is never asked} \
        $PROMPT "    if !crate::services::overridden(crate::points::PROMPT) \{" \
        "    if true || !crate::services::overridden(crate::points::PROMPT) \{" \
        $CORE $T_PROMPT] \
    [list {the tool point is never asked} \
        $TOOLS "        if !self.session_id.is_empty() && crate::services::overridden(crate::points::TOOL) \{" \
        "        if false && crate::services::overridden(crate::points::TOOL) \{" \
        $CORE $T_TOOL] \
    [list {core's tool default drops the args next was given} \
        $POINTS "    tx.send((name, input\[\"args\"\].clone(), reply))" \
        "    tx.send((name, json!(\{\}), reply))" \
        $CORE $T_TOOL] \
    [list {the identity check ignores sender_id} \
        $POINTS "pub const INBOUND_IDENTITY: &\[&str\] = &\[\"platform\", \"chat_id\", \"sender\", \"sender_id\", \"route\"\];" \
        "pub const INBOUND_IDENTITY: &\[&str\] = &\[\"platform\", \"chat_id\", \"sender\", \"route\"\];" \
        $CORE $T_IDENT] \
    [list {the gateway routes a changed sender} \
        $GATEWAY "            if let Err(why) = veles_core::points::inbound_identity_kept(&original, &got) \{" \
        "            if let Err(why) = Ok::<(), String>(()) \{" \
        $FRONT $T_INBOUND] \
    [list {the gateway ignores a drop} \
        $GATEWAY "        Ok(v) if v\[\"action\"\] == \"drop\" => \{" \
        "        Ok(v) if false && v\[\"action\"\] == \"drop\" => \{" \
        $FRONT $T_INBOUND] \
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
    foreach {target filter} [list $::CORE "points::" $::FRONT $::T_INBOUND] {
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
