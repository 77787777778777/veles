#!/usr/bin/env Tcl
# Mutation harness for the 2026-09-27 round: the secrets vault, the
# resolver, what the model may do with a key, and the setup wizard's
# secret entry. Every gate below is broken on purpose and must turn its
# pin RED; a mutation that leaves the suite green is a gate nothing
# checks.
#
#   direnv exec . tclsh scripts/mutcheck-2026-09-27.tcl
#
# A file for the same reason mutcheck-2026-09-20.tcl records: the
# patterns are full of braces and brackets a shell heredoc mangles.
# Matching is by `string first`, never a glob. The Rust mutations run
# through `cargo test` with a filter, the Tcl ones through their suite
# on Xvfb.
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set V crates/veles-core/src/vault.rs
set S crates/veles-core/src/secrets.rs
set T crates/veles-core/src/secret_tools.rs
set A crates/veles-core/src/ask.rs
set D crates/veles-frontends/src/desktop_api.rs
set U crates/veles-core/src/setup.rs
set W veles-desktop/lib/setup.tcl
set K veles-desktop/lib/ask.tcl

set mutations [list \
    [list "vault: a row's signature is not checked (anyone could forge a key)" \
        $V \
        {.is_some_and(|ks| ks.iter().any(|k| verify(k, msg, sig)))} \
        {.is_some_and(|_ks| true)} \
        "vault::tests::a_write_not_signed_by_a_member_is_refused"] \
    [list "vault: a revoked node's rows count whether vouched or not" \
        $V \
        {self.revoked.get(by).is_some_and(|v| v.contains(id))} \
        {self.revoked.get(by).is_some_and(|_v| true)} \
        "vault::tests::a_revoked_nodes_later_rows_are_refused_and_its_earlier_ones_kept"] \
    [list "vault: a first sync adopts any root, no fingerprint asked" \
        $V \
        "        (None, Some(i)) => \{" \
        "        (None, Some(i)) if false => \{" \
        "vault::tests::the_first_sync_needs_the_roots_fingerprint"] \
    [list "vault: admit takes any fingerprint" \
        $V \
        "if !same_fingerprint(fingerprint_seen, &fp) \{" \
        "if false \{" \
        "vault::tests::admit_needs_the_fingerprint_the_operator_saw"] \
    [list "vault: a revoke vouches for none of the node's writes" \
        $V \
        "                .records\n                .iter()\n                .filter(|r| r.by == node)" \
        "                .records\n                .iter()\n                .filter(|_r| false)" \
        "vault::tests::a_revoked_nodes_later_rows_are_refused_and_its_earlier_ones_kept"] \
    [list "resolver: a .env copy in the environment outranks the vault" \
        $S \
        "if !crate::config::env_copied_from_file(name, v) \{" \
        "if true \{" \
        "secrets::tests::the_vault_outranks_env_files_and_yields_to_a_real_export"] \
    [list "resolver: an unset key_env NAME is sent as the key again" \
        $S \
        "    if name_shaped \{" \
        "    if false \{" \
        "secrets::tests::an_unset_key_env_name_is_an_error_not_the_key"] \
    [list "model: a guest session gets \$NAME injected" \
        $S \
        {Some(p) => p.has(crate::acl::Cap::All),} \
        {Some(_p) => true,} \
        "secrets::tests::a_guest_session_gets_no_secrets"] \
    [list "model: an owner's command keeps the inherited bot token" \
        $S \
        {.filter(|k| !policy_of(k, &cfg).shell)} \
        {.filter(|_k| false)} \
        "secrets::tests::an_owner_command_never_carries_a_platform_credential"] \
    [list "model: encoded forms of a key are not scrubbed" \
        $S \
        "for (enc, how) in encodings(v) \{" \
        "for (enc, how) in Vec::<(String, &str)>::new() \{" \
        "secrets::tests::encoded_forms_of_a_value_are_scrubbed"] \
    [list "model: api_request sends a key to any host" \
        $T \
        "if !crate::secrets::url_allowed(&url, &policy.hosts) \{" \
        "if false \{" \
        "secret_tools::tests::api_request_refuses_a_host_the_secret_is_not_bound_to"] \
    [list "ask: a secret ask is shown on a chat surface" \
        $A \
        {.filter(|(_, a, _)| !secret || a.private_input())} \
        {.filter(|(_, _a, _)| true || secret)} \
        "secret_tools::tests::a_secret_ask_skips_surfaces_without_private_input"] \
    [list "ask: a secret ask's answer keeps the text the operator typed" \
        $A \
        "let answer = if p.req.kind == AskKind::Secret \{" \
        "let answer = if false \{" \
        "secret_tools::tests::a_secret_asks_answer_never_carries_typed_text"] \
    [list "api: /api/secrets/set takes a value from any network" \
        $D \
        {crate::secrets::accepts_secret_values(a.ip(), &crate::config::live())} \
        {crate::secrets::accepts_secret_values(a.ip(), &crate::config::live()) || true} \
        "frontends::desktop_api::tests::a_secret_is_stored_only_from_a_trusted_path_and_never_echoed"] \
    [list "api: setup apply takes secret values from any network" \
        $D \
        "&& !accepts_values(&peer) \{" \
        "&& false \{" \
        "frontends::desktop_api::tests::setup_apply_takes_secret_values_only_from_this_machine"] \
    [list "setup: the wizard stores a name its answers do not use" \
        $U \
        "if !plan.secrets.contains(name) \{" \
        "if false \{" \
        "setup::tests::a_secret_name_the_answers_do_not_use_is_refused_before_any_write"] \
    [list "tcl: Skip on the provider step still stores its key" \
        $W \
        "if \{\$v(provider_idx) != 2 && \"provider\" ni \$skipped" \
        "if \{\$v(provider_idx) != 2" \
        "setuptest"] \
    [list "tcl: the wizard keeps typed keys after sending them" \
        $W \
        "    unset -nocomplain body vals\n    _forget_secrets" \
        "    unset -nocomplain body vals" \
        "setuptest"] \
    [list "tcl: the dialog keeps the typed value after storing it" \
        $K \
        "    # it again rather than keeping it.\n    set secret_value \"\"\n    if \{\$rc\} \{" \
        "    # it again rather than keeping it.\n    if \{\$rc\} \{" \
        "asktest"] \
    [list "tcl: the dialog offers a field without asking the gateway" \
        $K \
        "if \{\[::vapi::secrets_accepted\]\} \{" \
        "if \{1\} \{" \
        "asktest"] \
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
