#!/usr/bin/env tclsh
# Mutation harness for the 2026-10-08 review of R1 + R2
# (PLAN-plugins-installer.md): the module loader's refusals (key, no
# runtime, another build), the owner-tracked registries (registration
# order, purge, overrides restored, poison), the late-bound session tool,
# the rt spawn hop and the registry-taxonomy scan. Replaces the R2
# harness, whose anchors the registry rewrite moved. Each mutant removes
# one guard and must turn its named test RED; the source is restored
# after every run (written fresh from memory, never cp -p).
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-08-review.tcl [a filter]
#
# Matching is by `string first`, never a glob.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set OWNED crates/veles-core/src/owned.rs
set RT crates/veles-core/src/rt.rs
set TAX crates/veles-core/tests/registry_taxonomy.rs
set MOD crates/veles-core/src/module.rs
set TOOLS crates/veles-core/src/tools.rs

# Each mutation: name, file, find, repl, cargo test target, test filter.
set mutations [list \
    [list "purge(None) purges CORE (core must never be purgeable)" \
        $OWNED \
        "        let Some(owner) = owner else {\n            return 0;\n        };" \
        "        let Some(owner) = owner else {\n            self.with_write(|v| v.retain(|(_, e)| e.owner().is_some()));\n            return 1; // MUTATION: purge(None) purges core\n        };" \
        "-p veles-core --lib" "owned::tests::purge_none_removes_nothing"] \
    [list "purge keeps ONLY the target's entries" \
        $OWNED \
        "            v.retain(|(_, e)| e.owner() != Some(owner));" \
        "            v.retain(|(_, e)| e.owner() == Some(owner)); // MUTATION" \
        "-p veles-core --lib" "owned::tests::purge_removes_exactly_one_owners_entries"] \
    [list "everything registers as core (the thread tag is ignored)" \
        $OWNED \
        "            owner: crate::lifecycle::thread_owner()," \
        "            owner: None, // MUTATION" \
        "-p veles-core --lib" "owned::tests::a_reinsert_after_purge_is_owned_by_the_new_owner"] \
    [list "an OwnedSlot purge clears every install" \
        $OWNED \
        "        g.retain(|e| e.owner() != Some(owner));" \
        "        g.clear(); // MUTATION" \
        "-p veles-core --lib" "owned::tests::a_slot_purges_only_its_own_owner"] \
    [list "registration order lost (newest first)" \
        $OWNED \
        "            v.push((key, item));" \
        "            v.insert(0, (key, item)); // MUTATION" \
        "-p veles-core --lib" "owned::tests::iteration_is_registration_order_and_a_reregistration_moves_last"] \
    [list "an override REPLACES what it covers (a purge cannot restore it)" \
        $OWNED \
        "            v.retain(|(k, e)| !(k == &key && e.owner() == owner));" \
        "            v.retain(|(k, _)| k != &key); // MUTATION" \
        "-p veles-core --lib" "owned::tests::a_purged_override_restores_what_it_covered"] \
    [list "a poisoned lock reads as empty" \
        $OWNED \
        "        self.read().is_empty()" \
        "        self.entries.read().map(|r| r.is_empty()).unwrap_or(true) // MUTATION" \
        "-p veles-core --lib" "owned::tests::a_poisoned_lock_still_counts"] \
    [list "a session holds a module's tool directly (no late binding)" \
        $TOOLS \
        "    if owner.is_none() {\n        return tool.for_session(session_id).unwrap_or(tool);\n    }" \
        "    if true || owner.is_none() {\n        return tool.for_session(session_id).unwrap_or(tool); // MUTATION\n    }" \
        "-p veles-core --lib" "tools::tests::a_late_bound_module_tool_follows_a_reload"] \
    [list "rt: a module thread spawns on the AMBIENT runtime, not core's" \
        $RT \
        "    if crate::lifecycle::thread_owner().is_some() {\n        if let Some(h) = core_handle() {\n            return h.spawn(fut);\n        }" \
        "    if false && crate::lifecycle::thread_owner().is_some() {\n        if let Some(h) = core_handle() {\n            return h.spawn(fut);\n        } // MUTATION" \
        "-p veles-core --lib" "rt::tests::a_module_thread_spawn_hops_to_cores_runtime"] \
    [list "taxonomy: a static holding dyn is NOT flagged" \
        $TAX \
        "    if t.starts_with(\"static \")\n        && (t.contains(\"OnceLock\") || t.contains(\"LazyLock\"))" \
        "    if false && t.starts_with(\"static \")\n        && (t.contains(\"OnceLock\") || t.contains(\"LazyLock\"))" \
        "-p veles-core --test registry_taxonomy" "the_scanner_flags_an_untracked_registry"] \
    [list "module: another key is accepted" \
        $MOD \
        "        Some(k) if k != BUILD_KEY => {" \
        "        Some(k) if false && k != BUILD_KEY => { // MUTATION" \
        "-p veles-cli --test module_load" "another_key_is_refused_from_the_file"] \
    [list "module: a module without the runtime is accepted" \
        $MOD \
        "    if !file.needed.iter().any(|n| n == RUNTIME_SONAME) {" \
        "    if false && !file.needed.iter().any(|n| n == RUNTIME_SONAME) { // MUTATION" \
        "-p veles-core --lib" "module::tests::a_module_without_a_key_another_key_or_no_runtime_is_refused"] \
    [list "module: a module from another cargo build is accepted" \
        $MOD \
        "    if let Some(first) = missing.first() {" \
        "    if let Some(first) = missing.first().filter(|_| false) { // MUTATION" \
        "-p veles-cli --test module_load" "a_module_from_another_build_is_refused_from_the_file"] \
    [list "module: the key section is not read from the file" \
        $MOD \
        "        .section_by_name(KEY_SECTION)" \
        "        .section_by_name(\".veles_module_kez\") // MUTATION" \
        "-p veles-cli --test module_load" "the_echo_module_of_this_build_passes"] \
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
puts "mutcheck review: $verified verified red, $failures not"
exit [expr {$failures > 0 ? 1 : 0}]
