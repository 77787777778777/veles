#!/usr/bin/env tclsh
# Mutation harness for the Telegram mention gate
# (crates/veles-telegram/src/points.rs `mentions`): a mention is the bot's
# username in any case, and only as the whole name. Each mutant removes
# one guard and must turn its named test RED; the source is restored
# after every run. Verdicts as in scripts/mutcheck-2026-10-10-signal-pin.tcl.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-telegram-mention.tcl [a filter]
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-telegram-mention.tcl --dry
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set POINTS crates/veles-telegram/src/points.rs
set TG {-p veles-telegram --lib}
set T_MENTION {points::tests::a_mention_ignores_case_and_must_be_the_whole_username}
# name file find repl target test
set mutations [list \
    [list {the username keeps its case} \
        $POINTS "format!(\"@{}\", username.to_lowercase())" \
        "format!(\"@{}\", username)" \
        $TG $T_MENTION] \
    [list {the text keeps its case} \
        $POINTS "    let text = text.to_lowercase();" \
        "    let text = text.to_string();" \
        $TG $T_MENTION] \
    [list {a longer username counts as a mention} \
        $POINTS "            && !matches!(text\[at + needle.len()..\].chars().next(), Some(c) if name_char(c))" \
        "" \
        $TG $T_MENTION] \
    [list {an address mid-word counts as a mention} \
        $POINTS "        !matches!(text\[..at\].chars().next_back(), Some(c) if name_char(c))\n            && " \
        "        " \
        $TG $T_MENTION] \
    [list {an empty username matches any `@`} \
        $POINTS "    if username.is_empty() \{\n        return false;\n    \}\n" \
        "" \
        $TG $T_MENTION] \
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
    foreach filter [list $::T_MENTION] {
        set v [run_test $::TG $filter]
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
