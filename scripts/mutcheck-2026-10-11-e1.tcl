#!/usr/bin/env tclsh
# Mutation harness for override points and `next` (PLAN-extension-api.md
# E1). Each mutant removes one guard and must turn its named test RED;
# the source is restored after every run. Verdicts as in
# scripts/mutcheck-2026-10-10-signal-pin.tcl: a timeout-killed run, a
# process that ends without a verdict, and a non-zero exit are all RED.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-e1.tcl [a filter]
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-e1.tcl --dry
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
# plugin_so must load the real sample .so, not skip.
set ::env(VELES_REQUIRE_PLUGIN_SO) 1
set SERVICES   crates/veles-core/src/services.rs
set OWNED      crates/veles-core/src/owned.rs
set BRIDGE     crates/veles-core/src/bridge.rs
set PLUGINHOST crates/veles-core/src/pluginhost.rs
set RUSTPLUG   crates/veles-core/src/rustplugins.rs
set CORE {-p veles-core --lib}
set ECHO {-p veles-plugin-echo --test host}
set SO   {-p veles-core --test plugin_so}
set MHT  {-p veles-cli --test module_host}
set T_WRAP  {services::tests::an_override_wraps_next_and_a_purge_restores_the_layer_below}
set T_FALL  {services::tests::a_failing_override_falls_through_or_fails_as_the_point_says}
set T_TOKEN {services::tests::a_next_token_is_per_call_and_can_be_neither_forged_nor_kept}
set T_LATE  {services::tests::a_default_declared_late_still_sits_below_the_override}
set T_TCL   {bridge::tests::a_tcl_override_wraps_next_over_rust_and_over_another_tcl_layer}
set T_PROC  {a_process_plugin_overrides_a_point_and_wraps_next}
set T_SO    {the_native_tier_passes_the_conformance_suite}
set T_JS    {the_js_script_tier_passes_the_conformance_suite}
# name file find repl target test
set mutations [list \
    [list {next jumps to the bottom layer} \
        $SERVICES "    Ok((slot, at - 1, entry))" "    Ok((slot, 0, entry)) // MUTATION" \
        $CORE $T_WRAP] \
    [list {a purge leaves the override in place} \
        $SERVICES "    let n = registry().purge(owner);" "    let n = registry().purge(None); // MUTATION" \
        $CORE $T_WRAP] \
    [list {a failing override does not fall through} \
        $SERVICES \
        "            Err(e) if point.on_error == OnError::FallThrough && idx > 0 => \{" \
        "            Err(e) if false && point.on_error == OnError::FallThrough && idx > 0 => \{" \
        $CORE $T_FALL] \
    [list {an answer without a required key passes} \
        $SERVICES "        if v.get(k).is_none() \{" "        if false && v.get(k).is_none() \{" \
        $CORE $T_FALL] \
    [list {a kept token still works after its call} \
        $SERVICES "            .remove(&self.0);" "            .get(&self.0); // MUTATION" \
        $CORE $T_TOKEN] \
    [list {a default declared late goes on top} \
        $OWNED "            v.insert(at, (key, item));" "            let _ = at;\n            v.push((key, item)); // MUTATION" \
        $CORE $T_LATE] \
    [list {Tcl's in-place next leaves the caller's request clobbered} \
        $BRIDGE "            if let Some(s) = saved \{" "            if let Some(s) = saved.filter(|_| false) \{" \
        $CORE $T_TCL] \
    [list {a process plugin's point request is not an envelope} \
        $PLUGINHOST "                let params = if crate::services::is_point(&name) \{" \
        "                let params = if false && crate::services::is_point(&name) \{" \
        $ECHO $T_PROC] \
    [list {the process host does not answer service.next} \
        $PLUGINHOST "            method::SERVICE_NEXT => \{" "            \"service.next.mutated\" => \{" \
        $ECHO $T_PROC] \
    [list {a .so service is refused} \
        $RUSTPLUG "    if kind != veles_plugin_abi::JSON_KIND_SERVICE \{" \
        "    if true || kind != veles_plugin_abi::JSON_KIND_SERVICE \{" \
        $SO $T_SO] \
    [list {a script's payload carries no next} \
        $SERVICES "        \"next\": cx.next," "        \"next\": serde_json::Value::Null, // MUTATION" \
        $MHT $T_JS] \
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
    foreach {target filter} [list $::CORE "services:: bridge::tests::a_tcl_override" \
                                  $::ECHO $::T_PROC $::SO $::T_SO $::MHT $::T_JS] {
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
