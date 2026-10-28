#!/usr/bin/env tclsh
# Mutation harness for R6 (PLAN-plugins-installer.md): the per-node
# switch and the Telegram module's guards. Each mutant removes one and
# must turn its named test RED; the source is restored after every run.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-09-r6.tcl [a filter]
#
# Matching is by `string first`, never a glob. Every field is
# BRACE-quoted or double-quoted with escaped braces.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set PLUGINHOST crates/veles-core/src/pluginhost.rs
set MODULEHOST crates/veles-core/src/modulehost.rs
set TGMODULE   crates/veles-telegram/src/module.rs
set TGLIB      crates/veles-telegram/src/lib.rs

set ::env(VELES_PODMAN_BIN) /nonexistent/podman

set mutations [list \
    [list {the switch ignores enabled_env (an off node spawns the platform)} \
        $PLUGINHOST \
        "            if matches!(v.as_str(), \"false\" | \"0\" | \"no\" | \"off\") \{" \
        "            if false \{ // MUTATION: never off" \
        {-p veles-core --lib} {the_node_switch_reads_the_two_env_names}] \
    [list {the switch ignores required_env (a node without the token spawns an instance)} \
        $PLUGINHOST \
        "        if resolve(var).is_none_or(|v| v.trim().is_empty()) \{" \
        "        if false \{ // MUTATION: never required" \
        {-p veles-cli --test module_host} {the_telegram_module_polls_claims_its_route_and_stops_its_poll}] \
    [list {the module host never consults the switch} \
        $MODULEHOST \
        "        if let Some(why) = pluginhost::node_switch_here(&manifest).err() \{" \
        "        if let Some(why) = None::<String> \{ // MUTATION: the switch is not consulted" \
        {-p veles-cli --test module_host} {the_telegram_module_polls_claims_its_route_and_stops_its_poll}] \
    [list {stop never uses the shutdown token (the long poll outlives the instance)} \
        $TGMODULE \
        {        let token = self.token.lock().unwrap_or_else(|e| e.into_inner()).take();} \
        {        let token: Option<teloxide::dispatching::ShutdownToken> = None; // MUTATION} \
        {-p veles-cli --test module_host} {the_telegram_module_polls_claims_its_route_and_stops_its_poll}] \
    [list {the adapter never hands the token out (stop has nothing to end)} \
        $TGLIB \
        {            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(dispatcher.shutdown_token());} \
        {            let _ = &slot; // MUTATION: no token} \
        {-p veles-cli --test module_host} {the_telegram_module_polls_claims_its_route_and_stops_its_poll}] \
]

proc run_test {target filter} {
    set f [open "| direnv exec . cargo test $target $filter 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
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
    set verdict [catch {run_test $target $filter} r]
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh $orig
    close $fh
    if {$verdict} { return "ERROR: $r" }
    return $r
}

set filter [lindex $argv 0]
set verified 0
set failures 0
foreach m $mutations {
    lassign $m name file find repl target test
    if {$filter ne "" && [string first $filter $name] < 0} { continue }
    puts -nonewline "[format %-80s $name] "
    flush stdout
    set r [apply_and_run $file $find $repl $target $test]
    puts $r
    if {$r eq "RED"} { incr verified } else { incr failures }
}
puts ""
puts "mutcheck r6 2026-10-09: $verified verified red, $failures not"
exit [expr {$failures > 0}]
