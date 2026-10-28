#!/usr/bin/env tclsh
# Mutation harness for the platform modules' override points
# (PLAN-extension-api.md E4: `irc.addressed`, `telegram.addressed`,
# declarers vs overrides). Each mutant removes one guard and must turn its
# named test RED; the source is restored after every run. Verdicts as in
# scripts/mutcheck-2026-10-10-signal-pin.tcl.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-e4.tcl [a filter]
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-e4.tcl --dry
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set IRCP     crates/veles-irc/src/points.rs
set TGP      crates/veles-telegram/src/points.rs
set IRCMOD   crates/veles-irc/src/module.rs
set TGMOD    crates/veles-telegram/src/module.rs
set SERVICES crates/veles-core/src/services.rs
set IRC  {-p veles-irc --lib}
set TG   {-p veles-telegram --lib}
set CORE {-p veles-core --lib}
set MHT  {-p veles-cli --test module_host}
set T_IRC     {points::tests::an_override_decides_which_channel_lines_reach_the_bot}
set T_TG      {points::tests::an_override_decides_which_group_messages_reach_the_bot}
set T_MULTI   {services::tests::a_point_declared_by_several_instances_is_not_overridden_by_them}
set T_IRCMOD  {the_irc_module_connects_registers_and_unloads}
set T_TGMOD   {the_telegram_module_polls_claims_its_route_and_stops_its_poll}
# name file find repl target test
set mutations [list \
    [list {IRC never asks its point} \
        $IRCP "    if !veles_core::services::overridden(ADDRESSED) \{" \
        "    if true || !veles_core::services::overridden(ADDRESSED) \{" \
        $IRC $T_IRC] \
    [list {IRC drops the override's text} \
        $IRCP "            v\[\"text\"\].as_str().map(str::to_string).unwrap_or(body)," \
        "            body, // MUTATION" \
        $IRC $T_IRC] \
    [list {Telegram never asks its point} \
        $TGP "    if !veles_core::services::overridden(ADDRESSED) \{" \
        "    if true || !veles_core::services::overridden(ADDRESSED) \{" \
        $TG $T_TG] \
    [list {a second declarer counts as an override} \
        $SERVICES "            .any(|o| !declarers.contains(o))" \
        "            .filter(|_| true).count() > 1 // MUTATION: every provider counts" \
        $CORE $T_MULTI] \
    [list {the IRC module does not declare its point} \
        $IRCMOD "            crate::points::declare().map_err(|e| anyhow::anyhow!(\"\{e\}\"))?;" \
        "            // MUTATION: no point declared" \
        $MHT $T_IRCMOD] \
    [list {the Telegram module does not declare its point} \
        $TGMOD "            crate::points::declare().map_err(|e| anyhow::anyhow!(\"\{e\}\"))?;" \
        "            // MUTATION: no point declared" \
        $MHT $T_TGMOD] \
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
    foreach {target filter} [list $::IRC "points::" $::TG "points::" $::CORE $::T_MULTI \
                                  $::MHT "irc_module telegram_module"] {
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
