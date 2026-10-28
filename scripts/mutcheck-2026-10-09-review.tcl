#!/usr/bin/env tclsh
# Mutation harness for the 2026-10-09 review of R3/R4
# (PLAN-plugins-installer.md): every guard the review added or fixed.
# Each mutant removes one and must turn its named test RED; the source is
# restored after every run (written fresh from memory, never cp -p).
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-09-review.tcl [a filter]
#
# Matching is by `string first`, never a glob. Fields are BRACE-quoted
# (no Tcl substitution inside the Rust anchors) unless the anchor's own
# braces do not balance — then double-quoted, with [ ] " { escaped. Run it
# alone: it rewrites sources and rebuilds target/ from them.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set CLIENT    crates/veles-irc/src/client.rs
set MODULE    crates/veles-irc/src/module.rs
set HOST      crates/veles-core/src/modulehost.rs
set LOGBUF    crates/veles-core/src/logbuf.rs
set CHALLENGE crates/veles-core/src/challenge.rs
set IRCTOML   crates/veles-irc/Cargo.toml
set TESTENV   crates/veles-core/src/testenv.rs

# The offline-switch mutant lets a test reach podman: point it at none,
# so the mutant fails on "no container runtime" instead of making a box.
set ::env(VELES_PODMAN_BIN) /nonexistent/podman

# Each mutation: name, file, find, repl, cargo test target, test filter.
set mutations [list \
    [list {the QUIT flush polls past the stream's end (the 2026-10-08 spin)} \
        $CLIENT \
        "        while let Some(Ok(msg)) = stream.next().await \{" \
        "        loop \{ let Some(Ok(msg)) = stream.next().await else \{ continue \}; // MUTATION" \
        {-p veles-irc --lib} {the_quit_flush_ends_when_the_server_closes}] \
    [list {the QUIT flush stops at the end but not at an error} \
        $CLIENT \
        "        while let Some(Ok(msg)) = stream.next().await \{" \
        "        while let Some(r) = stream.next().await \{ let Ok(msg) = r else \{ continue \}; // MUTATION" \
        {-p veles-irc --lib} {the_quit_flush_ends_at_a_stream_error}] \
    [list {the QUIT flush keeps no closing line} \
        $CLIENT \
        {                closing = Some(text);} \
        {                let _ = text; // MUTATION} \
        {-p veles-irc --lib} {the_quit_flush_returns_the_servers_closing_line}] \
    [list {a requested QUIT counts as a server close (the loop reconnects)} \
        $CLIENT \
        {                    return Ok(ConnEnd::Quit);} \
        {                    return Ok(ConnEnd::Closed); // MUTATION} \
        {-p veles-irc --lib} {a_requested_quit_ends_the_reconnect_loop}] \
    [list {stop sends an empty QUIT (the reported symptom)} \
        $MODULE \
        {client::quit_network(&net, client::bot_presence(), &reason).await;} \
        {client::quit_network(&net, client::bot_presence(), "").await; // MUTATION} \
        {-p veles-cli --test module_host} {the_irc_module_connects_registers_and_unloads}] \
    [list {the thread leg reads 0 (a spinning instance thread is unmapped under)} \
        $HOST \
        {        threads.load(Ordering::SeqCst),} \
        {        0, // MUTATION} \
        {-p veles-cli --test module_host} {a_thread_that_never_yields_keeps_the_module_mapped}] \
    [list {the callsite leg reads 0 (a module with its own callsites unmaps)} \
        $HOST \
        {        module::callsites_within(&mapped_ranges(&canonical)),} \
        {        0, // MUTATION} \
        {-p veles-cli --test module_host} {the_irc_module_connects_registers_and_unloads}] \
    [list {the subscriber does not record callsites} \
        $LOGBUF \
        {        .with(RingLayer)
        .with(crate::module::CallsiteRecorder);} \
        {        .with(RingLayer); // MUTATION} \
        {-p veles-cli --test module_host} {the_irc_module_connects_registers_and_unloads}] \
    [list {veles-irc gets its dylib form back (the gateway links the module)} \
        $IRCTOML \
        {[dependencies]
veles-core = { path = "../veles-core" }} \
        {[lib]
crate-type = ["rlib", "dylib"] # MUTATION

[dependencies]
veles-core = { path = "../veles-core" }} \
        {-p veles-cli --test module_load} {the_gateway_links_no_module}] \
    [list {the visible-text count slices the page as str again} \
        $CHALLENGE \
        "        if bytes\[i..\].starts_with(b\"<script\") \{" \
        "        if lower\[i..\].starts_with(\"<script\") \{ // MUTATION" \
        {-p veles-core --lib} {multibyte_text_in_a_gated_page_does_not_panic}] \
    [list {test envs reach the real podman (the leaked boxes)} \
        $TESTENV \
        {    crate::user_envs::set_test_offline();} \
        {    // MUTATION: no offline switch} \
        {-p veles-frontends --lib} {test_user_envs_stay_off_the_real_podman}] \
    [list {a callsite-pinned reload is a fault again (last error on a healthy instance)} \
        $HOST \
        {        return UnloadCheck::Pinned(format!(} \
        {        return UnloadCheck::KeptMapped(format!( // MUTATION} \
        {-p veles-core --lib} {the_quiescence_check_refuses_a_pinned_library}] \
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
puts "mutcheck review 2026-10-09: $verified verified red, $failures not"
exit [expr {$failures > 0 ? 1 : 0}]
