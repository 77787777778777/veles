#!/usr/bin/env tclsh
# Mutation harness for the overlay module scaffold (PLAN-extension-api.md
# E5: `veles plugins new <name> --kind overlay`, examples/overlay-module).
# Each mutant removes one guard and must turn its named test RED; the
# source is restored after every run. Verdicts as in
# scripts/mutcheck-2026-10-10-signal-pin.tcl.
#
# The flake's own guards (a bad module name stops the evaluation; a crate
# the lock lacks stops the build with veles' message) are nix, not cargo,
# and were checked by hand (PLAN-extension-api.md, E5's status row).
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-e5.tcl [a filter]
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-e5.tcl --dry
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set PLUGINS crates/veles-core/src/plugins.rs
set CORE {-p veles-core --lib}
set T_EXAMPLE {plugins::tests::the_overlay_scaffold_is_the_example}
set T_NAME    {plugins::tests::the_overlay_scaffold_checks_its_name_and_never_overwrites}
# name file find repl target test
set mutations [list \
    [list {the name may hold `_` and uppercase} \
        $PLUGINS "                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')," \
        "                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')," \
        $CORE $T_NAME] \
    [list {the name may start with a digit or `-`} \
        $PLUGINS "            && name.starts_with(|c: char| c.is_ascii_lowercase())" \
        "            && !name.is_empty()" \
        $CORE $T_NAME] \
    [list {a scaffold writes over a directory that exists} \
        $PLUGINS "    anyhow::ensure!(!dir.exists(), \"{} already exists\", dir.display());" \
        "    // MUTATION: a scaffold may write over a directory" \
        $CORE $T_NAME] \
    [list {a quote in the description reaches the Rust string} \
        $PLUGINS ".replace(\['\"', '\\\\'\], \"'\")" \
        ".replace(\['\\\\'\], \"'\")" \
        $CORE $T_NAME] \
    [list {the template drifts from the example} \
        $PLUGINS "        \"say hello from @NAME@\"" \
        "        \"say hi from @NAME@\"" \
        $CORE $T_EXAMPLE] \
    [list {the scaffold drops a file the example has} \
        $PLUGINS "        (\"README.md\", fill(OVERLAY_README)),\n" \
        "" \
        $CORE $T_EXAMPLE] \
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
    foreach filter [list $::T_EXAMPLE $::T_NAME] {
        set v [run_test $::CORE $filter]
        if {$v ne "GREEN"} {
            puts "baseline is $v for $filter: fix the tree first"
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
