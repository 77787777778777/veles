#!/usr/bin/env tclsh
# Mutation harness for R3 (PLAN-plugins-installer.md): the module host's
# guards. Each mutant removes one and must turn its named test RED; the
# source is restored after every run (written fresh from memory, never
# cp -p).
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-08-r3.tcl [a filter]
#
# Matching is by `string first`, never a glob.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set MH crates/veles-core/src/modulehost.rs
set OWNED crates/veles-core/src/owned.rs

# Each mutation: name, file, find, repl, cargo test target, test filter.
set mutations [list \
    [list "the gate never closes (teardown proceeds while calls run)" \
        $MH \
        "    pub fn close(&self) {\n        self.closed.store(true, Ordering::Release);" \
        "    pub fn close(&self) {\n        self.closed.store(false, Ordering::Release); // MUTATION" \
        "-p veles-core --lib" "modulehost::tests::a_closed_gate_refuses_and_drains"] \
    [list "teardown skips the registry purge (registrations survive a reload)" \
        $MH \
        "    let purged = purge_owner_everywhere(Some(owner));" \
        "    let purged = 0; // MUTATION" \
        "-p veles-cli --test module_host" "a_module_loads_runs_and_unloads"] \
    [list "the quiescence check always says Clear (a pinned library unmaps)" \
        $MH \
        "    if lib_strong > 1 {" \
        "    if false && lib_strong > 1 { // MUTATION" \
        "-p veles-core --lib" "modulehost::tests::the_quiescence_check_refuses_a_pinned_library"] \
    [list "purge_owner_everywhere ignores the module's commands" \
        $MH \
        "    n += crate::commands::purge_owner_commands(owner);" \
        "    // MUTATION: commands not purged" \
        "-p veles-cli --test module_host" "a_module_loads_runs_and_unloads"] \
]

proc run_test {target filter} {
    set f [open "| direnv exec . cargo test $target $filter 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
    # A build failure is its own verdict.
    if {[regexp {error\[E[0-9]+\]|could not compile} $out]} { return "NOCOMPILE" }
    if {![regexp {running ([1-9][0-9]*) tests?} $out]} { return "NO-SUCH-TEST" }
    return [expr {[string match "*test result: FAILED*" $out] ? "RED" : "GREEN"}]
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
    set verdict "TEST-ERROR: (none)"
    if {[catch { set verdict [run_test $target $filter] } caught]} {
        set verdict "TEST-ERROR: $caught"
    }
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh $orig
    close $fh
    set fh [open $file r]
    fconfigure $fh -encoding utf-8
    set back [read $fh]
    close $fh
    if {$back ne $orig} { return RESTORE-FAILED }
    return $verdict
}

set only [expr {[llength $argv] > 0 ? [lindex $argv 0] : ""}]
set failures 0
set verified 0
foreach m $mutations {
    lassign $m name file find repl target filter
    if {$only ne "" && [string first $only $name] < 0} { continue }
    set verdict [apply_and_run $file $find $repl $target $filter]
    switch -- $verdict {
        RED { puts "  ✓ $name — goes RED ($filter)" ; incr verified }
        GREEN { puts "  ✗ $name — STAYS GREEN (the pin does not catch it)" ; incr failures }
        PATTERN-MISSING { puts "  ? $name — PATTERN MISSING (the anchor moved; re-point it)" ; incr failures }
        RESTORE-FAILED { puts "  !! $name — RESTORE FAILED (revert $file by hand!)" ; incr failures }
        default { puts "  ? $name — $verdict" ; incr failures }
    }
    flush stdout
}
puts ""
puts "mutcheck R3: $verified verified red, $failures not"
exit [expr {$failures > 0 ? 1 : 0}]
