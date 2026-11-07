#!/usr/bin/env tclsh
# Mutation checks for the Anthropic Message Threads acceptance invariants.
# Each mutation must make its real regression test RED; restore immediately.
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set TCLDRIVER plugins/examples/tcl-anthropic-driver/plugin.tcl
set NATIVE crates/veles-plugin-anthropic-so/src/lib.rs
set PROVIDERS crates/veles-core/src/providers.rs
set THREADS crates/veles-core/src/message_threads.rs
set AGENT crates/veles-core/src/agent.rs
set mutations [list \
  [list {beta header can be dropped from both drivers} \
    $NATIVE \
    {message-threads-2026-08-12} \
    {# MUTATION: omit beta header} \
    {-p veles-core --test driver_parity} {message_threads_create_and_continue_have_the_documented_wire_shape}] \
  [list {system and tools can be repeated on continue} \
    $TCLDRIVER \
    {set parts [lsearch -all -inline -not -index 0 $parts "system"]
            set parts [lsearch -all -inline -not -index 0 $parts "tools"]} \
    {# MUTATION: repeat create-only fields} \
    {-p veles-core --test driver_parity} {message_threads_create_and_continue_have_the_documented_wire_shape}] \
  [list {a consumed cursor remains reusable} \
    $THREADS \
    {if prepared.is_some() {
            self.previous_message_id.clear();
        }} \
    {// MUTATION: do not consume the continuation id} \
    {-p veles-core --lib} {message_threads::tests::a_cursor_is_consumed_once_and_records_the_full_request_length}] \
  [list {a lost live thread is not replayed} \
    $AGENT \
    {|| why.contains("No thread state was found for the requested `previous_message_id`")} \
    {|| false} \
    {-p veles-core --lib} {agent::tests::a_failed_message_thread_replays_then_falls_back_without_threads}] \
]

proc run_test {target filter} {
    set f [open "| direnv exec . cargo test $target $filter -- --exact --test-threads=1 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch {set out [read $f]}
    catch {close $f}
    if {[regexp {error\[E[0-9]+\]|could not compile} $out]} {return "NOCOMPILE"}
    if {![regexp {running ([1-9][0-9]*) tests?} $out]} {return "NO-SUCH-TEST"}
    return [expr {[string match "*test result: FAILED*" $out] ? "RED" : "GREEN"}]
}
proc apply_and_run {file find repl target filter} {
    set fh [open $file r]; fconfigure $fh -encoding utf-8
    set orig [read $fh]; close $fh
    if {[string first $find $orig] < 0} {return PATTERN-MISSING}
    set fh [open $file w]; fconfigure $fh -encoding utf-8
    puts -nonewline $fh [string map [list $find $repl] $orig]; close $fh
    if {$file eq $::NATIVE} {
        set build_rc [catch {exec direnv exec . cargo build -p veles-plugin-anthropic-so >@stdout 2>@stderr}]
        if {$build_rc} {set fh [open $file w]; puts -nonewline $fh $orig; close $fh; return NOCOMPILE}
    }
    set verdict [catch {run_test $target $filter} result]
    set fh [open $file w]; fconfigure $fh -encoding utf-8
    puts -nonewline $fh $orig; close $fh
    if {$verdict} {return "ERROR: $result"}
    return $result
}
set ::env(VELES_REQUIRE_PLUGIN_SO) 1
set filter [lindex $argv 0]
set verified 0
set failures 0
foreach m $mutations {
    lassign $m name file find repl target test
    if {$filter ne "" && [string first $filter $name] < 0} {continue}
    puts -nonewline "[format %-64s $name] "
    flush stdout
    set r [apply_and_run $file $find $repl $target $test]
    puts $r
    if {$r eq "RED"} {incr verified} else {incr failures}
}
puts "message-thread mutations: $verified verified red, $failures not"
exit [expr {$failures > 0}]
