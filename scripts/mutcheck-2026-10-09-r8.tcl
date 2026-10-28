#!/usr/bin/env tclsh
# Mutation harness for R8 (PLAN-plugins-installer.md): the ABI 4 host
# and SDK. Each mutant removes one guard and must turn its named test
# RED; the source is restored after every run. A mutant in the SDK or
# the sample plugin REBUILDS the cdylib first (cargo has no dependency
# edge from the test to it), and a panic that crosses the boundary
# ABORTS the test process, which counts as RED too.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-09-r8.tcl [a filter]
#
# Matching is by `string first`, never a glob. Every field is
# BRACE-quoted or double-quoted with escaped braces.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set RUST   crates/veles-core/src/rustplugins.rs
set SERV   crates/veles-core/src/soservices.rs
set SDK    crates/veles-plugin-abi-sdk/src/lib.rs

set ::env(VELES_REQUIRE_PLUGIN_SO) 1
set ::env(VELES_PODMAN_BIN) /nonexistent/podman

set SO     {-p veles-core --test plugin_so}
set LIB    {-p veles-core --lib}
set PLUGINS {-p veles-plugin-sample-so -p veles-plugin-badabi-so -p veles-plugin-anthropic-so}

# name file find repl target test ?prebuild?
set mutations [list \
    [list {the version check accepts any export (an ABI 3 plugin's init would run)} \
        $RUST \
        {        Some(VELES_PLUGIN_ABI_VERSION) => {}} \
        {        Some(_) => {} // MUTATION: any version} \
        $SO {an_abi_3_plugin_is_refused_before_its_init_runs}] \
    [list {the refusal no longer names the rebuild} \
        $RUST \
        "            \"{}: ABI {v} plugin; veles {} loads ABI {} only: rebuild against veles-plugin-abi \\" \
        "            \"{}: ABI {v} plugin; veles {} loads ABI {} only: see the docs for the crate \\" \
        $SO {an_abi_3_plugin_is_refused_before_its_init_runs}] \
    [list {an unloaded plugin keeps its platform route} \
        $SERV \
        {                bus.unregister_outbound_route(&key, DEFAULT_ROUTE);} \
        {                let _ = (&key, DEFAULT_ROUTE); // MUTATION: the route stays} \
        $SO {the_abi4_services_round_trip_and_unload_releases_them}] \
    [list {a cancelled timer still fires (the fire-time check is gone)} \
        $SERV \
        "    if take(handle, plugin).is_none() \{\n        return;\n    \}" \
        "    let _ = take(handle, plugin); // MUTATION: fires regardless" \
        $LIB {soservices::tests::a_cancelled_timer_that_is_already_due_does_not_fire}] \
    [list {a purged socket's task is not aborted (the library stays held)} \
        $SERV \
        "        Resource::Socket \{ tx, task \} => \{\n            drop(tx);\n            if let Some(t) = task \{\n                t.abort();\n            \}\n        \}" \
        "        Resource::Socket \{ tx, task \} => \{\n            drop(tx);\n            let _ = task; // MUTATION: the task lives on\n        \}" \
        $SO {the_abi4_services_round_trip_and_unload_releases_them}] \
    [list {unregister leaves a late hook in place} \
        $RUST \
        {                .retain(|(h, _, _)| *h != handle);} \
        {                .retain(|_| true); // MUTATION: nothing dropped} \
        $SO {the_abi4_services_round_trip_and_unload_releases_them}] \
    [list {the watcher's reload resurrects an unloaded plugin} \
        $RUST \
        "        if is_so_unloaded(&p.manifest.name) \{" \
        "        if false \{ // MUTATION: the unloaded set is ignored" \
        $SO {unload_purges_every_registration_and_start_brings_it_back}] \
    [list {unload purges no registry} \
        $RUST \
        {    let purged = crate::modulehost::purge_owner_everywhere(Some(owner));} \
        {    let purged = 0; let _ = owner; // MUTATION: registries untouched} \
        $SO {unload_purges_every_registration_and_start_brings_it_back}] \
    [list {the SDK lets a tool's panic cross the boundary (an abort)} \
        $SDK \
        {    give(match catch_unwind(AssertUnwindSafe(|| (t.f)(&args))) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => format!("error: {e}"),
        Err(e) => t.host.report_panic("a tool call", e),
    })} \
        {    give(match (t.f)(&args) {
        Ok(s) => s,
        Err(e) => format!("error: {e}"),
    }) // MUTATION: no catch_unwind} \
        $SO {the_abi4_services_round_trip_and_unload_releases_them} $PLUGINS] \
    [list {the SDK answers a panicking async future as success} \
        $SDK \
        {            Err(e) => {
                w.result = Some(w.host.report_panic("an async tool's future", e));
                w.fut = None;
                1
            }} \
        {            Err(_) => {
                w.result = Some("so_wait: woke after 40 ms".into()); // MUTATION
                w.fut = None;
                1
            }} \
        $SO {the_abi4_services_round_trip_and_unload_releases_them} $PLUGINS] \
]

proc run_cmd {argv} {
    set f [open "| direnv exec . $argv 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
    return $out
}

proc run_test {target filter prebuild} {
    if {$prebuild ne ""} {
        set b [run_cmd "cargo build $prebuild"]
        if {[regexp {error\[E[0-9]+\]|could not compile} $b]} { return "NOCOMPILE(prebuild)" }
    }
    set out [run_cmd "cargo test $target $filter"]
    if {[regexp {error\[E[0-9]+\]|could not compile} $out]} { return "NOCOMPILE" }
    if {[regexp {process didn't exit successfully|\(signal: 6|SIGABRT} $out]} { return "RED" }
    if {![regexp {running ([1-9][0-9]*) tests?} $out]} { return "NO-SUCH-TEST" }
    return [expr {[string match "*test result: FAILED*" $out] ? "RED" : "GREEN"}]
}

proc apply_and_run {file find repl target filter prebuild} {
    set fh [open $file r]
    fconfigure $fh -encoding utf-8
    set orig [read $fh]
    close $fh
    if {[string first $find $orig] < 0} { return PATTERN-MISSING }
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh [string map [list $find $repl] $orig]
    close $fh
    set verdict [catch {run_test $target $filter $prebuild} r]
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh $orig
    close $fh
    if {$prebuild ne ""} { run_cmd "cargo build $prebuild" }
    if {$verdict} { return "ERROR: $r" }
    return $r
}

set filter [lindex $argv 0]
set verified 0
set failures 0
foreach m $mutations {
    lassign $m name file find repl target test prebuild
    if {$filter ne "" && [string first $filter $name] < 0} { continue }
    puts -nonewline "[format %-80s $name] "
    flush stdout
    set r [apply_and_run $file $find $repl $target $test $prebuild]
    puts $r
    if {$r eq "RED"} { incr verified } else { incr failures }
}
puts ""
puts "mutcheck r8 2026-10-09: $verified verified red, $failures not"
exit [expr {$failures > 0}]
