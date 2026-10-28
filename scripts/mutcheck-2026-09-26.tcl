#!/usr/bin/env Tcl
# Mutation harness for the 2026-09-26 round: the first-run setup wizard
# (veles_core::setup, the /api/setup routes, lib/setup.tcl). Patterns
# re-pointed 2026-09-27 after the review rewrote setup.rs (edit in place,
# the .env append); the review's own mutations live in its commits.
#
#   direnv exec . tclsh scripts/mutcheck-2026-09-26.tcl
#
# A file for the same reason mutcheck-2026-09-20.tcl records: the
# patterns are full of braces and brackets a shell heredoc mangles.
# Matching is by `string first`, never a glob. Each mutation names the
# pin that must redden: the Rust ones run through `cargo test` with a
# filter, the Tcl ones through setuptest.tcl on Xvfb.
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set S crates/veles-core/src/setup.rs
set T veles-desktop/lib/setup.tcl

set mutations [list \
    [list "core: key_env regresses to api_key_env (the 60786dd dead key)" \
        $S \
        "key_env = {key_env}" \
        "api_key_env = {key_env}" \
        "setup::tests::the_provider_block_lands_as_a_custom_provider_with_key_env"] \
    [list "core: use_tls regresses to tls (a knob the adapter never reads)" \
        $S \
        {use_tls = {use_tls}} \
        {tls = {use_tls}} \
        "setup::tests::irc_uses_the_table_and_keys_the_adapter_reads"] \
    [list "core: the model pair regresses to top-level strings (unparseable)" \
        $S \
        {"[model]\nprovider = {}\ndefault = {}\n\n",} \
        {"provider = {}\ndefault = {}\n\n", // MUTATION: no [model] header} \
        "setup::tests::the_default_plan_emits_a_model_table_not_top_level_strings"] \
    [list "core: apply clobbers an existing .env (a live token dies)" \
        $S \
        "    if !env_path.exists() {\n        write_private(&env_path, &plan.env_text)?;" \
        "    if true {\n        write_private(&env_path, &plan.env_text)?;" \
        "setup::tests::apply_edits_in_place_keeps_a_backup_and_leaves_the_env_alone"] \
    [list "core: the backup is written but not REPORTED (the UI cannot say where it went)" \
        $S \
        "        config_backup,\n        env_path," \
        "        config_backup: {let _ = config_backup; None}, // MUTATION: backup unreported\n        env_path," \
        "setup::tests::apply_edits_in_place_keeps_a_backup_and_leaves_the_env_alone"] \
    [list "core: the .env loses its 0600 (world-readable secrets)" \
        $S \
        "std::fs::Permissions::from_mode(0o600)" \
        "std::fs::Permissions::from_mode(0o644)" \
        "setup::tests::apply_writes_config_and_a_private_env"] \
    [list "core: a curated ACL with no owner writes the lockout anyway" \
        $S \
        "    if t.is_empty() {\n        return Ok(None);" \
        "    if t.is_empty() {\n        return Ok(Some(Owner::Id(String::new())));" \
        "setup::tests::curated_acl_with_no_owner_stays_dormant_rather_than_locking_out"] \
    [list "api: apply serves the boot snapshot instead of reloading (R4.5)" \
        crates/veles-frontends/src/desktop_api.rs \
        "let reloaded = match srv.reload_config() {" \
        "let reloaded = match Ok::<usize, anyhow::Error>(0) {" \
        "frontends::desktop_api::tests::setup_apply_edits_in_place_and_reloads_the_live_handle"] \
    [list "api: state calls a hand-rolled config first_run (fires on every missing key)" \
        crates/veles-frontends/src/desktop_api.rs \
        "\"first_run\": !cfg_path.exists()," \
        "\"first_run\": true," \
        "frontends::desktop_api::tests::setup_state_sees_a_first_run_and_then_the_file"] \
    [list "tcl: the nav frame is not torn down (the bug setuptest found)" \
        $T \
        "    catch { destroy \$win.nav }\n    ttk::frame \$win.nav" \
        "    ttk::frame \$win.nav" \
        "setuptest"] \
    [list "tcl: the sig gate is dropped (Write it writes an unread preview)" \
        $T \
        "    if {\$preview_sig ne \[answers_sig\]} {" \
        "    if {0} {" \
        "setuptest"] \
    [list "tcl: the locale hook is never registered (a swap leaves stale text)" \
        $T \
        "    ::vi18n::on_locale_change \[list ::vsetup::retext\]" \
        "    # MUTATION: no locale hook" \
        "setuptest"] \
    [list "tcl: setup_done is never persisted (the wizard fires on every boot)" \
        $T \
        "    if {!\[::vstate::put setup_done 1\]} {" \
        "    if {0} {" \
        "setuptest"] \
    [list "tcl: first-run ignores the setup_done marker" \
        $T \
        "    if {\[string trim \[::vstate::get setup_done 0\]\] in {1 true}} { return 0 }" \
        "    # MUTATION: marker ignored" \
        "setuptest"] \
    [list "tcl: telegram rides the wire as 1/0, not a JSON bool (422)" \
        $T \
        "        telegram \[::vapi::json_bool \$v(telegram)\] \\" \
        "        telegram \$v(telegram) \\" \
        "setuptest"] \
]

proc run_test {filter} {
    set f [open "| direnv exec . cargo test -p veles-core --lib $filter 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
    # `2>@1` is Tcl's "stderr to stdout" in a pipeline; `2>&1` (as this
    # read until 2026-09-27) sends stderr to a FILE NAMED "&1" in the
    # repo root, so a mutation that did not compile printed nothing
    # here — and "no tests ran" then read as RED, a catch that never
    # happened. A build failure is its own verdict now.
    if {[regexp {error\[E[0-9]+\]|could not compile} $out]} { return "NOCOMPILE" }
    if {![regexp {running ([0-9]+) tests?} $out -> n]} { return "NO-RUN" }
    if {$n == 0} { return "NO-SUCH-TEST" }
    return [expr {[string match "*test result: ok.*" $out] ? "GREEN" : "RED"}]
}

proc run_suite {suite} {
    # one_tcl_test.sh EXITS NON-ZERO on failures and Tcl's `close` raises
    # for that — exactly the case a mutation harness aims for, so catch
    # the read and judge the captured tally regardless of status.
    set f [open "| bash scripts/one_tcl_test.sh $suite 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
    if {![regexp {([0-9]+) passed, ([0-9]+) failed} $out -> passed failed]} {
        # No tally. Two very different causes, and telling them apart is
        # the whole job:
        #   * the suite could not START (missing stub, no X, its own
        #     SKIP guard) — NOT a detection, report NO-SUITE loudly;
        #   * the suite started asserting and DIED mid-run — a mutation
        #     made a proc throw, and wish aborts before the tally line.
        #     The baseline suite is green, so a started-but-unfinished
        #     run can only be the mutation: that IS a detection.
        # "started" = at least one ✓/✗ line made it out. (wish's stderr
        # trace — "Error in startup script" — is unreliable here: through
        # xvfb-run's wrapper it often never reaches the pipe, while
        # stdout does, so the ✓-count is the signal, with the trace
        # accepted too when it does arrive.)
        set started [regexp {\u2713|\u2717} $out]
        if {$started || [string match "*Error in startup script*" $out]} {
            return "RED"
        }
        return "NO-SUITE"
    }
    return [expr {$failed > 0 ? "RED" : "GREEN"}]
}

proc apply_and_run {file find repl filter} {
    set fh [open $file r]
    fconfigure $fh -encoding utf-8
    set orig [read $fh]
    close $fh
    if {[string first $find $orig] < 0} { return [list PATTERN-MISSING $orig] }
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh [string map [list $find $repl] $orig]
    close $fh
    set verdict "TEST-ERROR: (none)"
    set caught ""
    if {[catch {
        if {[string match "*::*" $filter]} {
            set verdict [run_test $filter]
        } else {
            set verdict [run_suite "veles-desktop/lib/${filter}.tcl"]
        }
    } caught]} { set verdict "TEST-ERROR: $caught" }
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh $orig
    close $fh
    set fh [open $file r]
    fconfigure $fh -encoding utf-8
    set back [read $fh]
    close $fh
    if {$back ne $orig} { return [list RESTORE-FAILED $orig] }
    return [list $verdict $orig]
}

set failures 0
set verified 0
foreach m $mutations {
    lassign $m name file find repl filter
    lassign [apply_and_run $file $find $repl $filter] verdict _
    switch -- $verdict {
        RED { puts "  ✓ $name — goes RED ($filter)" ; incr verified }
        PATTERN-MISSING { puts "  ✗ $name — PATTERN NOT FOUND"; incr failures }
        RESTORE-FAILED { puts "  ✗ $name — RESTORE FAILED"; incr failures }
        NO-SUCH-TEST { puts "  ✗ $name — NO TEST MATCHED: $filter"; incr failures }
        NO-SUITE { puts "  ✗ $name — suite did not report"; incr failures }
        default { puts "  ✗ $name — NOT REDENED ($verdict)"; incr failures }
    }
}
puts ""
if {$failures > 0} { puts "mutcheck: $failures did NOT redden ($verified verified)"; exit 1 }
puts "mutcheck: all $verified verified"
exit 0
