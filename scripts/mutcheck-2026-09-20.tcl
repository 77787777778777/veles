#!/usr/bin/env Tcl
# Mutation harness for the 2026-09-20 round: each mutation reverts one
# covered behavior; the named tests must go RED. Run from the repo root
# inside the dev shell.
#
#   direnv exec . tclsh scripts/mutcheck-2026-09-20.tcl
#
# Written as a FILE rather than a heredoc: the patterns are full of
# braces and brackets that a shell heredoc mangles before tclsh sees
# them. (Same lesson scripts/i18n-keys.tcl records.)
#
# 2026-09-20 — this file's first two cuts both failed, and both failures
# are the lesson:
#
#   1. It COULD NOT PARSE. `set mutations [list …]` with patterns like
#      `…== label) {\n continue\n }}` desyncs Tcl's brace COUNTER — the
#      `{` inside the Rust `if` opens a group that the pattern's own `}`
#      is expected to close. tclsh answered "missing close-brace" at
#      line 16 and NOTHING RAN. Patterns are double-quoted here, with
#      `\{`/`\}` escaped so the counter stays honest.
#   2. It MATCHED WITH GLOBS. `string match` treats `[` from the pattern
#      as a character class, so `vec![r.provider.clone()]` searched for
#      something else entirely — and the same trap the 2026-09-21 round
#      hit in the transcript ("a prompt's glob characters are not
#      treated as a pattern") is exactly this one. Matching is by
#      `string first` now; no pattern is ever a pattern.
#
# And the safety property: the file is restored before the verdict is
# even computed, and a failure to run a test is its own verdict — a
# harness that dies mid-mutation must not leave the tree dirty, and a
# green-from-nothing filter must not pass a mutation.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

# name  file  find  replace  test-filter
set mutations [list \
    [list "kimi: the ratio half wins (the lie)" \
        crates/veles-core/src/tokenplan.rs \
        "            if windows.iter().any(|w| w.label == label) {\n                continue;\n            }" \
        "" \
        "tokenplan::quota_tests::kimi_reads_the_absolute_half_where_the_two_halves_disagree"] \
    [list "kimi: drop the month ratio windows" \
        crates/veles-core/src/tokenplan.rs \
        "    if let Some(usages) = v.get(\"usages\").and_then(Value::as_object) {" \
        "    if let Some(usages) = Option::<&serde_json::Map<String, Value>>::None {" \
        "tokenplan::quota_tests::kimi_serves_the_month_ratios_under_their_own_labels"] \
    [list "ledger: fold unmapped providers into a real plan" \
        crates/veles-core/src/tokenplan.rs \
        "            plan: None," \
        "            plan: Some(Plan::Zai)," \
        "tokenplan::quota_tests::a_planless_provider_keeps_its_own_row_rather_than_joining_one"] \
    [list "plan_for_provider: ali-responses reads as personal" \
        crates/veles-core/src/tokenplan.rs \
        "    if p.contains(\"team\") || p.contains(\"responses\") {" \
        "    if p.contains(\"team2\") {" \
        "tokenplan::quota_tests::a_provider_names_the_pool_it_draws_from"] \
    [list "kimi: an empty payload becomes 0% instead of no reading" \
        crates/veles-core/src/tokenplan.rs \
        "    if windows.is_empty() {\n        return None;\n    }" \
        "    if windows.is_empty() {\n        windows.push(Window { label: \"5-hour\".to_string(), used_fraction: Some(0.0), resets_at: None });\n    }" \
        "tokenplan::quota_tests::a_kimi_payload_naming_no_windows_is_no_quota_not_zero_quota"] \
    [list "desktop: the status field drops the plan name" \
        veles-desktop/lib/statusbar.tcl \
        {    .statusbar.quota configure -text "$plan ${pct}%"} \
        {    .statusbar.quota configure -text "${pct}%"} \
        "statustest"] \
    [list "desktop: the status field goes back to the cross-plan tightest" \
        veles-desktop/lib/statusbar.tcl \
        "        catch { set f \[dict get \$q active_plan_fraction\] }" \
        "        catch { set f \[dict get \$q tightest_used_fraction\] }" \
        "statustest"] \
    [list "desktop: the usage ledger drops the plan split" \
        veles-desktop/lib/overlays.tcl \
        "    set row \[::voverlay::_uplan_fill \$w \$row \$by_plan allplan \\\n        \[::vi18n::mc {by plan — all sessions}\]\]" \
        "    # MUTATION: the split block is not drawn" \
        "quotatest"] \
]

# The file is restored the moment the test returns, verdict or throw —
# an unhandled error mid-run must never leave a mutation in the tree.
# This was a REAL failure, twice: a `close $f` on a suite's non-zero
# exit threw out of the loop with the mutation still applied, and a
# killed run (SIGTERM from a shell timeout) left three mutations live in
# the working tree — which the next round then read as "PATTERN NOT
# FOUND" for the very hunks it was supposed to mutate. Restore in a
# catch, and prove the restore by comparing bytes.
proc apply_and_run {file find repl filter} {
    set fh [open $file r]
    fconfigure $fh -encoding utf-8
    set orig [read $fh]
    close $fh

    # LITERAL containment. `string match` would read the `[` in the
    # replace side as a character class — see the header.
    if {[string first $find $orig] < 0} {
        return [list PATTERN-MISSING $orig]
    }
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh [string map [list $find $repl] $orig]
    close $fh

    # Every path out of here restores first — including a throw.
    set verdict "TEST-ERROR: (none)"
    set caught ""
    if {[catch {
        if {[string match "*::*" $filter]} {
            set verdict [run_test $filter]
        } else {
            set verdict [run_suite "veles-desktop/lib/${filter}.tcl"]
        }
    } caught]} {
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
    if {$back ne $orig} {
        return [list RESTORE-FAILED $orig]
    }
    return [list $verdict $orig]
}

proc run_test {filter} {
    set f [open "| cargo test -p veles-core --lib $filter 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    # A reddening mutation makes the test binary exit NON-ZERO, and
    # `close` raises for that — catch it, read what came out anyway.
    catch { close $f }
    # A GREEN verdict needs BOTH "test result: ok" AND at least one test
    # actually run: a filter matching nothing prints "running 0 tests"
    # and then "test result: ok", which would otherwise read as "the
    # mutation did not redden the test" and pass a mutation that was
    # never exercised.
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
    # `-ignorestderr` is not enough: one_tcl_test.sh EXITS NON-ZERO when
    # a suite has failures, and Tcl's `close` on a pipeline raises for a
    # non-zero exit — which is exactly the case a mutation harness aims
    # for, so the first cut threw on its own success. `catch` the read
    # and read what was captured regardless of status.
    set f [open "| bash scripts/one_tcl_test.sh $suite 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
    if {![regexp {([0-9]+) passed, ([0-9]+) failed} $out -> passed failed]} {
        return "NO-SUITE"
    }
    return [expr {$failed > 0 ? "RED" : "GREEN"}]
}

set failures 0
set verified 0
foreach m $mutations {
    lassign $m name file find repl filter

    lassign [apply_and_run $file $find $repl $filter] verdict _

    switch -- $verdict {
        RED {
            puts "  ✓ $name — pinned test goes RED ($filter)"
            incr verified
        }
        PATTERN-MISSING {
            puts "  ✗ $name — PATTERN NOT FOUND in $file (harness out of sync with the source)"
            incr failures
        }
        RESTORE-FAILED {
            puts "  ✗ $name — RESTORE FAILED, $file differs from its pre-mutation bytes"
            incr failures
        }
        NO-SUCH-TEST {
            puts "  ✗ $name — NO TEST MATCHED the filter (green-from-nothing proves nothing): $filter"
            incr failures
        }
        NO-SUITE {
            puts "  ✗ $name — the suite did not run or report: $filter"
            incr failures
        }
        default {
            puts "  ✗ $name — NOT REDENED ($verdict)"
            incr failures
        }
    }
}

puts ""
if {$failures > 0} {
    puts "mutcheck: $failures mutation(s) did NOT redden their test ($verified verified)"
    exit 1
}
puts "mutcheck: all $verified mutations verified (each reddens its pinned test)"
exit 0