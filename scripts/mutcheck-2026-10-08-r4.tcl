#!/usr/bin/env tclsh
# Mutation harness for R4 (PLAN-plugins-installer.md): the IRC module's
# guards. Each mutant removes one and must turn its named test RED; the
# source is restored after every run (written fresh from memory, never
# cp -p).
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-08-r4.tcl [a filter]
#
# Matching is by `string first`, never a glob.
#
# Tcl NOTE: the Rust anchors below contain `vec![…]`, and Tcl substitutes
# […] inside DOUBLE quotes — so every field here is BRACE-quoted ({…},
# no substitution). The anchors' own braces balance, which is what a
# brace-quoted field needs.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set CLIENT crates/veles-irc/src/client.rs
set MODULE crates/veles-irc/src/module.rs
set HOST   crates/veles-core/src/modulehost.rs

# Each mutation: name, file, find, repl, cargo test target, test filter.
set mutations [list \
    [list {stop does not purge the tap presence (a stopped network stays sendable)} \
        $HOST \
        {    n += crate::irc_tap::purge_owner_presences(owner);} \
        {    // MUTATION: presences not purged} \
        {-p veles-cli --test module_host} {the_irc_module_connects_registers_and_unloads}] \
    [list {the irc tool registers behind a Once (a reload never re-registers it)} \
        $MODULE \
        {veles_core::tools::register_global_tool(Arc::new(crate::tools::IrcTool::default()));} \
        {static ONCE: std::sync::Once = std::sync::Once::new(); ONCE.call_once(|| { veles_core::tools::register_global_tool(Arc::new(crate::tools::IrcTool::default())); }); // MUTATION} \
        {-p veles-cli --test module_host} {the_irc_module_connects_registers_and_unloads}] \
    [list {the module never launches the adapter loop (no connection, no route)} \
        $MODULE \
        {let bus = cx.bus.clone();} \
        {let bus = cx.bus.clone(); if true { return Ok(()); } // MUTATION: the loop never launches} \
        {-p veles-cli --test module_host} {the_irc_module_connects_registers_and_unloads}] \
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
puts "mutcheck R4: $verified verified red, $failures not"
exit [expr {$failures > 0 ? 1 : 0}]
