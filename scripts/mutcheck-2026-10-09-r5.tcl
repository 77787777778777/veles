#!/usr/bin/env tclsh
# Mutation harness for R5 (PLAN-plugins-installer.md): the seams core
# opened for the slopdrop module, and the module's own guards. Each
# mutant removes one and must turn its named test RED; the source is
# restored after every run (written fresh from memory, never cp -p).
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-09-r5.tcl [a filter]
#
# Matching is by `string first`, never a glob. Every field is
# BRACE-quoted or double-quoted with escaped braces (Tcl substitutes
# […] inside double quotes).

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set TCLEVAL  crates/veles-core/src/tcleval.rs
set GATEWAY  crates/veles-frontends/src/gateway.rs
set HOST     crates/veles-core/src/modulehost.rs
set TRIGGERS crates/veles-core/src/triggers.rs
set COMMANDS crates/veles-core/src/commands.rs
set SYNCASK  crates/veles-core/src/syncask.rs
set DATA     crates/veles-slopdrop/src/data.rs
set RECEIPTS crates/veles-slopdrop/src/receipts.rs
set PUBLISH  crates/veles-slopdrop/src/publish.rs
set MODULE   crates/veles-slopdrop/src/module.rs

set ::env(VELES_PODMAN_BIN) /nonexistent/podman

# Each mutation: name, file, find, repl, cargo test target, test filter.
set mutations [list \
    [list {the backend slot is ignored (every tcl goes to the local sandbox)} \
        $TCLEVAL \
        {        if b.configured() {
            return b.eval(code, full, caller, sender).await;
        }} \
        {        let _ = b; // MUTATION: the backend is never asked} \
        {-p veles-core --lib} {a_configured_backend_takes_the_line_and_an_unconfigured_one_does_not}] \
    [list {an unconfigured backend still takes the line} \
        $TCLEVAL \
        {        if b.configured() {
            return b.eval(code, full, caller, sender).await;
        }} \
        {        return b.eval(code, full, caller, sender).await; // MUTATION} \
        {-p veles-core --lib} {a_configured_backend_takes_the_line_and_an_unconfigured_one_does_not}] \
    [list {a public /tcl with no policy evaluates and posts the output} \
        $GATEWAY \
        {                            reply_plain(
                                &msg,
                                veles_core::tcleval::PUBLIC_REPLY_UNAVAILABLE,} \
        {                            reply_plain(
                                &msg,
                                super::commands::tcl_command_as(&super::commands::AclCtx::from_agent(&agent, Some(&principal)).with_sender(&msg.sender_ctx), rest, head == "/tcladmin").await, // MUTATION: the output is posted} \
        {-p veles-frontends --lib} {a_public_tcl_is_the_policys_word_and_fails_closed_without_one}] \
    [list {the purge leaves the backend slot filled (tcl keeps going to an unloaded module)} \
        $HOST \
        {    n += crate::tcleval::purge_owner_backend(owner) as usize;} \
        {    // MUTATION: the backend is not purged} \
        {-p veles-cli --test module_host} {the_slopdrop_module_fills_the_seams_and_leaves_them_empty}] \
    [list {the purge leaves the direct command registered} \
        $HOST \
        {    n += crate::commands::purge_owner_commands(owner);} \
        {    // MUTATION: commands not purged} \
        {-p veles-cli --test module_host} {the_slopdrop_module_fills_the_seams_and_leaves_them_empty}] \
    [list {a forward with no forwarder answers an empty success instead of refusing} \
        $TRIGGERS \
        {    let Some(f) = forwarder() else {
        return Err(NO_FORWARDER.to_string());
    };
    f.forward(cfg, ev).await} \
        {    match forwarder() { Some(f) => f.forward(cfg, ev).await, None => Ok(Vec::new()) } // MUTATION} \
        {-p veles-core --lib} {without_a_forwarder_a_forward_is_refused_with_the_reason}] \
    [list {run_direct skips the command's capability floor} \
        $COMMANDS \
        "        if !p.has(cmd.cap) \{" \
        "        if false && !p.has(cmd.cap) \{ // MUTATION" \
        {-p veles-core --lib} {run_direct_enforces_the_commands_floor_before_the_body}] \
    [list {an external decision is decided by core instead of its source} \
        $SYNCASK \
        "            match src.decide(id, &raw, &text)? \{" \
        "            let _ = (&src, &raw, &text); match SourceOutcome::Later(id.clone()) \{ // MUTATION" \
        {-p veles-core --lib} {a_registered_source_is_scanned_asked_and_decided_as_its_own}] \
    [list {the migration deletes the legacy tree instead of keeping a backup} \
        $DATA \
        {    std::fs::rename(old, &backup)?;} \
        {    std::fs::remove_dir_all(old)?; // MUTATION: no backup} \
        {-p veles-slopdrop --lib} {the_legacy_tree_moves_once_and_is_kept_as_a_backup}] \
    [list {the migration runs again and merges a later legacy tree} \
        $DATA \
        "    if !old.is_dir() || new.join(MARKER).exists() \{" \
        "    if !old.is_dir() \{ // MUTATION: the marker is ignored" \
        {-p veles-slopdrop --lib} {the_legacy_tree_moves_once_and_is_kept_as_a_backup}] \
    [list {a receipt is filed on no entry (the ledger key is ignored)} \
        $RECEIPTS \
        {        let Some(id) = outbound_meta.get(meta_keys::LEDGER_ENTRY) else {
            return;
        };} \
        {        let Some(id) = outbound_meta.get(meta_keys::LEDGER_ENTRY) else { return; }; let _ = id; return; // MUTATION} \
        {-p veles-slopdrop --lib} {a_receipt_lands_on_the_entry_the_message_named}] \
    [list {the marks run before the mover check (an approve allowlists with nobody to repost)} \
        $PUBLISH \
        {    if !mover_available() {
        return Err(NO_MOVER.to_string());
    }
    match action {
        WithheldAction::Approve => approve_marks(&entry)?,
        WithheldAction::Delete => delete_marks(&entry)?,
    }} \
        {    match action {
        WithheldAction::Approve => approve_marks(&entry)?,
        WithheldAction::Delete => delete_marks(&entry)?,
    }
    if !mover_available() {
        return Err(NO_MOVER.to_string());
    } // MUTATION} \
        {-p veles-slopdrop --lib} {no_mover_marks_nothing_and_keeps_the_entry}] \
    [list {the module never fills the backend slot} \
        $MODULE \
        {            veles_core::tcleval::set_backend(Arc::new(crate::eval::Backend));} \
        {            // MUTATION: no backend} \
        {-p veles-cli --test module_host} {the_slopdrop_module_fills_the_seams_and_leaves_them_empty}] \
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
    if {$r eq "RED" || $r eq "NOCOMPILE"} { incr verified } else { incr failures }
}
puts ""
puts "mutcheck r5 2026-10-09: $verified verified red, $failures not"
exit [expr {$failures > 0}]
