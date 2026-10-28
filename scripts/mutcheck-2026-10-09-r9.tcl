#!/usr/bin/env tclsh
# Mutation harness for R9 (PLAN-plugins-installer.md): the script tier's
# seam and the JS host. Each mutant removes one guard and must turn its
# named test RED; the source is restored after every run. A test that
# HANGS on a mutant (the runaway-script one, were the watchdog gone)
# is killed by the per-run timeout, which counts as RED too.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-09-r9.tcl [a filter]
#
# Matching is by `string first`, never a glob. Every field is
# BRACE-quoted or double-quoted with escaped braces.
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set SEAM   crates/veles-core/src/scripthost.rs
set ENGINE crates/veles-lang-js/src/engine.rs
set TRANS  crates/veles-lang-js/src/transpile.rs
set TCLRT  crates/veles-core/src/tclrt.rs
set CORE {-p veles-core --lib}
set JS   {-p veles-lang-js --lib}
# name file find repl target test
set mutations [list \
    [list {a reload does not purge the earlier load (registrations double)} \
        $SEAM \
        "    let purged = forget(&name, owner);\n    if !entry.exists() \{" \
        "    let purged = 0; // MUTATION: nothing purged first\n    if !entry.exists() \{" \
        $CORE {scripthost::tests::a_script_registers_under_its_own_owner_and_an_unload_purges_it}] \
    [list {an unloaded script is loaded again by the next load-all} \
        $SEAM \
        "        if is_unloaded(&p.manifest.name) \{\n            out.push(" \
        "        if false \{ // MUTATION: the hold-out is ignored\n            out.push(" \
        $CORE {scripthost::tests::a_script_registers_under_its_own_owner_and_an_unload_purges_it}] \
    [list {a failed load keeps its partial registrations} \
        $SEAM \
        {            let _ = crate::modulehost::purge_owner_everywhere(Some(owner));
            hooks_table()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&name);
            let msg = format!("{owner}: load failed: {e}");} \
        {            let msg = format!("{owner}: load failed: {e}"); // MUTATION: no purge} \
        $CORE {scripthost::tests::a_failed_load_leaves_nothing_registered_and_says_error}] \
    [list {a host's purge leaves its scripts' registrations behind} \
        $SEAM \
        {    let mut n = hosts().purge(Some(owner));
    if kinds.is_empty() {
        return n;
    }} \
        {    let mut n = hosts().purge(Some(owner));
    if kinds.is_empty() || n < usize::MAX { // MUTATION: the scripts are not swept
        return n;
    }} \
        $CORE {scripthost::tests::a_hosts_purge_takes_its_scripts_registrations_with_it}] \
    [list {a null answer is the text "null"} \
        $SEAM \
        {        Ok(Value::Null) => String::new(),} \
        {        Ok(Value::Null) => "null".to_string(), // MUTATION} \
        $CORE {scripthost::tests::a_hosts_answer_decodes_to_the_text_a_tool_returns}] \
    [list {an empty hook answer counts as a verdict} \
        $SEAM \
        {                        Ok(v) if !v.is_empty() => Some(v),
                        Ok(_) => None,} \
        {                        Ok(v) => Some(v), // MUTATION: "" decides} \
        $CORE {scripthost::tests::an_empty_answer_from_the_hook_shell_is_no_opinion}] \
    [list {a hook or tool registered by a script carries no owner} \
        $SEAM \
        {        crate::lifecycle::with_thread_owner(self.owner, || {
            crate::tools::register_global_tool(tool);
        });} \
        {        crate::tools::register_global_tool(tool); // MUTATION: the caller's owner} \
        $CORE {scripthost::tests::a_script_registers_under_its_own_owner_and_an_unload_purges_it}] \
    [list {the watchdog never interrupts (a runaway callback hangs the host)} \
        $ENGINE \
        {        if now.saturating_sub(start) > self.slice_ms {
            self.tripped.store(true, Ordering::SeqCst);
            return true;
        }} \
        {        let _ = start; // MUTATION: never trips} \
        $JS {engine::tests::timers_fire_and_a_runaway_callback_is_interrupted}] \
    [list {an unload keeps the script's context} \
        $ENGINE \
        "                .remove(&name);\n            match gone \{" \
        "                .get(&name).cloned(); // MUTATION: the context stays\n            match gone \{" \
        $JS {engine::tests::a_typescript_plugin_registers_and_answers_through_the_seam}] \
    [list {TypeScript is handed to the engine as it is} \
        $TRANS \
        {    if !source_type.is_typescript() {
        return Ok(source.to_string());
    }} \
        {    if true { // MUTATION: nothing is stripped
        return Ok(source.to_string());
    }} \
        $JS {transpile::tests::typescript_types_are_stripped_and_enums_lowered}] \
    [list {the watcher drops a script source's edit event (only Tcl is a source)} \
        $TCLRT \
        {        Some(ext) => crate::scripthost::SCRIPT_SOURCE_EXTS.contains(&ext),} \
        {        Some(_) => false, // MUTATION: only Tcl is a plugin source} \
        $CORE {tclrt::tests::editing_a_script_plugin_does_trigger_a_reload}] \
    [list {the change digest skips the script tier's sources} \
        $TCLRT \
        "        \} else if is_plugin_source(&p) \{\n            out.push(p);" \
        "        \} else if matches!(p.extension().and_then(|x| x.to_str()), Some(\"tcl\") | Some(\"toml\")) \{ // MUTATION: the digest skips scripts\n            out.push(p);" \
        $CORE {tclrt::tests::the_change_digest_covers_the_script_tier_sources}] \
]
proc run_cmd {argv} {
    # `timeout` bounds a mutant that hangs (the runaway script with no
    # watchdog would wait for the 300 s tool deadline).
    set f [open "| direnv exec . timeout 900 $argv 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    set rc 0
    if {[catch { close $f } err]} {
        set rc [expr {[lindex $::errorCode 0] eq "CHILDSTATUS" ? [lindex $::errorCode 2] : -1}]
    }
    return [list $rc $out]
}
proc run_test {target filter} {
    lassign [run_cmd "cargo test $target $filter"] rc out
    if {[regexp {error\[E[0-9]+\]|could not compile} $out]} { return "NOCOMPILE" }
    # `timeout` exits 124 when it killed the run: the mutant hung (the
    # watchdog mutant did, for the full 900 s, and the partial output has
    # no FAILED line — it read GREEN on 2026-10-10 until this check).
    if {$rc == 124} { puts -nonewline "(killed by the 900 s timeout) "; return "RED" }
    if {[regexp {process didn't exit successfully|\(signal: 6|SIGABRT} $out]} { return "RED" }
    if {![regexp {running ([1-9][0-9]*) tests?} $out]} { return "NO-SUCH-TEST-OR-KILLED" }
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
if {$filter eq "--dry"} {
    # Every pattern must match its file exactly once, before any cargo run.
    set bad 0
    foreach m $mutations {
        lassign $m name file find repl target test
        set fh [open $file r]; fconfigure $fh -encoding utf-8; set src [read $fh]; close $fh
        set first [string first $find $src]
        set again [expr {$first < 0 ? -1 : [string first $find $src [expr {$first + 1}]]}]
        set v [expr {$first < 0 ? "PATTERN-MISSING" : ($again >= 0 ? "PATTERN-TWICE" : "ok")}]
        puts "[format %-80s $name] $v"
        if {$v ne "ok"} { incr bad }
    }
    exit [expr {$bad > 0}]
}
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
puts "mutcheck r9 2026-10-09: $verified verified red, $failures not"
exit [expr {$failures > 0}]
