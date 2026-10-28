#!/usr/bin/env tclsh
# Mutation harness for the gateway's pinned signal handlers (2026-10-10:
# the VPS gateway, pid 1 of its container, ignored podman's stop for 35 s
# with Racket up). Each mutant removes one guard and must turn its named
# test RED; the source is restored after every run. Same verdict rules as
# scripts/mutcheck-2026-10-10-r9-racket.tcl: a timeout-killed run, a
# process that ends without a verdict, and a non-zero exit are all RED.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-10-signal-pin.tcl [a filter]
#   direnv exec . tclsh scripts/mutcheck-2026-10-10-signal-pin.tcl --dry
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set LIFECYCLE crates/veles-core/src/lifecycle.rs
set MODHOST   crates/veles-core/src/modulehost.rs
set ENGINE    crates/veles-lang-racket/src/engine.rs
set CORE {-p veles-core --lib}
set MHT  {-p veles-cli --test module_host}
set RK   {-p veles-lang-racket --lib}
set T_PIN     {lifecycle::tests::pinned_signal_handlers_are_caught_and_keep_an_early_stop}
set T_START   {the_gateways_signal_handlers_are_pinned_before_modules_start}
set T_RKCONF  {the_racket_script_tier_passes_the_conformance_suite}
set T_SIGNALS {engine::tests::the_boot_leaves_the_gateways_signal_handlers_alone}
# name file find repl target test
set mutations [list \
    [list {the pin registers nothing (tokio never hears of the signals)} \
        $LIFECYCLE \
        "pub fn pin_signal_handlers() -> std::io::Result<()> \{\n    use tokio::signal::unix::\{signal, SignalKind\};\n" \
        "pub fn pin_signal_handlers() -> std::io::Result<()> \{\n    use tokio::signal::unix::\{signal, SignalKind\};\n    if true \{\n        return Ok(()); // MUTATION: nothing registered\n    \}\n" \
        $CORE $T_PIN] \
    [list {the pin parks nothing (a stop during the start is lost)} \
        $LIFECYCLE \
        "        *parked = Some((\n            signal(SignalKind::terminate())?,\n            signal(SignalKind::interrupt())?,\n        ));\n" \
        "        let _ = (signal(SignalKind::terminate())?, signal(SignalKind::interrupt())?); // MUTATION: nothing parked\n" \
        $CORE $T_PIN] \
    [list {the module host starts modules without pinning the handlers} \
        $MODHOST \
        "        #\[cfg(unix)\]\n        if let Err(e) = crate::lifecycle::pin_signal_handlers() \{\n" \
        "        #\[cfg(any())\] // MUTATION: no pin before the modules\n        if let Err(e) = crate::lifecycle::pin_signal_handlers() \{\n" \
        $MHT $T_START] \
    [list {the module host starts modules without pinning (Racket up: conformance-racket)} \
        $MODHOST \
        "        #\[cfg(unix)\]\n        if let Err(e) = crate::lifecycle::pin_signal_handlers() \{\n" \
        "        #\[cfg(any())\] // MUTATION: no pin before the modules\n        if let Err(e) = crate::lifecycle::pin_signal_handlers() \{\n" \
        $MHT $T_RKCONF] \
    [list {the Racket test compares SIG_DFL with SIG_DFL (its pin is gone)} \
        $ENGINE \
        "        veles_core::lifecycle::pin_signal_handlers().expect(\"pinned\");\n" \
        "        // MUTATION: the test compares SIG_DFL with SIG_DFL\n" \
        $RK $T_SIGNALS] \
    [list {the boot's signal dispositions are not put back} \
        $ENGINE \
        "    restore_signal_dispositions(&before);\n" \
        "    let _ = &before; // MUTATION: Racket keeps the handlers\n" \
        $RK $T_SIGNALS] \
]
proc run_cmd {argv} {
    set rc 0
    if {[catch {exec timeout -s KILL 600 sh -c "direnv exec . $argv 2>&1" </dev/null} out]} {
        if {[lindex $::errorCode 0] eq "CHILDSTATUS"} {
            set rc [lindex $::errorCode 2]
        } elseif {[lindex $::errorCode 0] eq "CHILDKILLED"} {
            # `timeout -s KILL` kills its whole group, itself included:
            # Tcl reports a child killed by a signal, not an exit status.
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
    if {$rc == 124 || $rc == 137} { puts -nonewline "(killed by the 600 s timeout) "; return "RED" }
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
    if {[string first $find $orig] < 0} { return PATTERN-MISSING }
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh [string map [list $find $repl] $orig]
    close $fh
    set verdict [catch {run_test $target $filter} r]
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh $orig
    close $fh
    if {$verdict} { return "ERROR: $r" }
    return $r
}
set filter ""
set dry 0
foreach a $argv {
    if {$a eq "--dry"} { set dry 1 } else { set filter $a }
}
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
        puts [format "%-78s %s" $name [expr {$ok ? "ok" : "PATTERN-MISSING"}]]
        if {$ok} { incr red }
        continue
    }
    puts -nonewline [format "%-78s " $name]
    flush stdout
    set v [apply_and_run $file $find $repl $target $test]
    puts $v
    if {$v eq "RED"} { incr red }
}
puts "$red/$total [expr {$dry ? "patterns match" : "RED"}]"
exit [expr {$red == $total ? 0 : 1}]
