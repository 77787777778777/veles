#!/usr/bin/env tclsh
# Mutation harness for R7 (PLAN-plugins-installer.md): the layer slots,
# the Tcl tier's ownership and unload, the hosts' unload. Each mutant
# removes one guard and must turn its named test RED; the source is
# restored after every run.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-09-r7.tcl [a filter]
#
# Matching is by `string first`, never a glob. Every field is
# BRACE-quoted or double-quoted with escaped braces.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set LAYERS     crates/veles-core/src/layers.rs
set OWNED      crates/veles-core/src/owned.rs
set AGENT      crates/veles-core/src/agent.rs
set PROMPT     crates/veles-core/src/prompt.rs
set TOOLS      crates/veles-core/src/tools.rs
set GATEWAY    crates/veles-frontends/src/gateway.rs
set TCLRT      crates/veles-core/src/tclrt.rs
set MODULEHOST crates/veles-core/src/modulehost.rs
set PLUGINHOST crates/veles-core/src/pluginhost.rs

set ::env(VELES_PODMAN_BIN) /nonexistent/podman

set CORE   {-p veles-core --lib}
set FRONT  {-p veles-frontends --lib}
set MHOST  {-p veles-cli --test module_host}
set PHOST  {-p veles-plugin-echo --test host}

set mutations [list \
    [list {a stack read answers only its top (layers below are skipped)} \
        $OWNED \
        {        self.lock().iter().map(|e| e.value.clone()).collect()} \
        {        self.lock().last().map(|e| e.value.clone()).into_iter().collect() // MUTATION} \
        $CORE {layers_stack_wrap_replace_and_purge_in_owner_order}] \
    [list {the drain drops every layer event (the caller's closure sees nothing)} \
        $LAYERS \
        "        while let Ok(ev) = this.rx.try_recv() \{" \
        "        while let Ok(ev) = this.rx.try_recv().and_then(|_| Err(std::sync::mpsc::TryRecvError::Empty)) \{ // MUTATION" \
        $CORE {drain_into_hands_layer_events_to_the_callers_closure}] \
    [list {the turn runner ignores installed layers} \
        $AGENT \
        "        let layers = crate::layers::turn_layers();\n        if layers.is_empty() \{" \
        "        let layers = crate::layers::turn_layers();\n        if true \{ // MUTATION: layers ignored" \
        $CORE {a_turn_layer_replaces_or_wraps_the_runner_and_a_purge_restores_it}] \
    [list {the prompt builder ignores installed layers} \
        $PROMPT \
        "    let layers = crate::layers::prompt_layers();\n    if layers.is_empty() \{" \
        "    let layers = crate::layers::prompt_layers();\n    if true \{ // MUTATION: layers ignored" \
        $CORE {a_prompt_layer_wraps_the_builder_and_a_purge_restores_it}] \
    [list {tool dispatch ignores installed layers} \
        $TOOLS \
        "        let layers = crate::layers::tool_layers();\n        if layers.is_empty() \{" \
        "        let layers = crate::layers::tool_layers();\n        if true \{ // MUTATION: layers ignored" \
        $CORE {a_dispatch_layer_answers_or_wraps_and_a_purge_restores_core}] \
    [list {inbound routing ignores installed layers} \
        $GATEWAY \
        "        let layers = veles_core::layers::inbound_layers();\n        if layers.is_empty() \{" \
        "        let layers = veles_core::layers::inbound_layers();\n        if true \{ // MUTATION: layers ignored" \
        $FRONT {an_inbound_layer_sees_a_message_before_routing_and_a_purge_restores_it}] \
    [list {a module's teardown never purges its layers} \
        $MODULEHOST \
        {    n += crate::layers::purge_owner(owner);} \
        {    n += 0; // MUTATION: layers never purged} \
        $MHOST {a_module_loads_runs_and_unloads}] \
    [list {a Tcl plugin is sourced untagged (nothing it registers is its own)} \
        $TCLRT \
        {    let sourced = crate::lifecycle::with_thread_owner(tag, || interp.source(&entry));} \
        {    let sourced = interp.source(&entry); let _ = tag; // MUTATION: untagged} \
        $CORE {unloading_a_tcl_plugin_drops_its_registrations_and_load_brings_them_back}] \
    [list {a reload re-sources an unloaded Tcl plugin} \
        $TCLRT \
        "        if is_unloaded(&p.manifest.name) \{" \
        "        if false \{ // MUTATION: the unloaded set is ignored" \
        $CORE {unloading_a_tcl_plugin_drops_its_registrations_and_load_brings_them_back}] \
    [list {a Tcl hook handler is not attributed to its plugin (unload leaves it)} \
        $TCLRT \
        {            set ::veles::hook_owner([::veles::_off_key $hookname $procname]) $::veles::plugin_current} \
        {            set ::veles::hook_owner([::veles::_off_key $hookname $procname]) ""} \
        $CORE {unloading_a_tcl_plugin_drops_its_registrations_and_load_brings_them_back}] \
    [list {a module unload keeps the row (the listing still shows it)} \
        $MODULEHOST \
        "        let out = self.control(target, Ctl::Unload).await?;\n        self.forget(&labels);" \
        "        let out = self.control(target, Ctl::Unload).await?;\n        let _ = &labels; // MUTATION: the row stays" \
        $MHOST {a_module_loads_runs_and_unloads}] \
    [list {a process-plugin unload keeps the row} \
        $PLUGINHOST \
        "        let out = self.control(target, Ctl::Unload).await?;\n        self.forget(&labels);" \
        "        let out = self.control(target, Ctl::Unload).await?;\n        let _ = &labels; // MUTATION: the row stays" \
        $PHOST {an_unloaded_instance_leaves_the_listing_and_its_process_ends}] \
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
puts "mutcheck r7 2026-10-09: $verified verified red, $failures not"
exit [expr {$failures > 0}]
