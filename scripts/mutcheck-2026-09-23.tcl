#!/usr/bin/env Tcl
# Mutation harness for the 2026-09-23 round: the personal month window
# (tokenplan.rs) and the Usage window's scrolling (overlays.tcl).
#
#   direnv exec . tclsh scripts/mutcheck-2026-09-23.tcl
#
# Written as a FILE for the same reason mutcheck-2026-09-20.tcl records:
# the patterns are full of braces and brackets a shell heredoc mangles.
# Matching is by `string first`, never a glob (the 2026-09-20 lesson).
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set mutations [list \
    [list "scroll: the wrapper is never built (clipped frame again)" \
        veles-desktop/lib/overlays.tcl \
        "    _usage_scroll \$w\n    _usage_fill \$w" \
        "    ttk::frame \$w.body\n    pack \$w.body -fill both -expand 1\n    _usage_fill \$w" \
        "quotatest"] \
    [list "scroll: the key bindings are dropped" \
        veles-desktop/lib/overlays.tcl \
        "    bind \$w <Up>    \[list ::voverlay::_uscr_key \$w -2\]" \
        "    # MUTATION: no key binding" \
        "quotatest"] \
    [list "scroll: the End key no longer reaches the bottom" \
        veles-desktop/lib/overlays.tcl \
        "        bottom { catch { \$c yview moveto 1.0 } }" \
        "        bottom { }" \
        "quotatest"] \
    [list "scroll: the canvas drops out of the Tab chain (keys go nowhere)" \
        veles-desktop/lib/overlays.tcl \
        "    canvas \$w.scroll.c -highlightthickness 0 -borderwidth 0 -takefocus 1 \\\n        -background \$::vt::T(bg_panel) -yscrollincrement 14" \
        "    canvas \$w.scroll.c -highlightthickness 0 -borderwidth 0 \\\n        -background \$::vt::T(bg_panel) -yscrollincrement 14" \
        "quotatest"] \
    [list "parser: the month family is dropped from the table" \
        crates/veles-core/src/tokenplan.rs \
        "    (\"per1Month\", \"month\")," \
        "    // MUTATION: month not readable" \
        "tokenplan::quota_tests::the_month_window_the_plan_moved_to_is_read"] \
    [list "parser: an unnamed payload invents a window name" \
        crates/veles-core/src/tokenplan.rs \
        "    out\n}\n\n// ── Path C — the z.ai coding plan" \
        "    if out.is_empty() { out.push(Window { label: \"5-hour\".into(), used_fraction: None, resets_at: None }); }\n    out\n}\n\n// ── Path C — the z.ai coding plan" \
        "tokenplan::quota_tests::a_payload_naming_no_family_returns_no_windows_rather_than_a_guess"] \
]

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

proc run_test {filter} {
    set f [open "| direnv exec . cargo test -p veles-core --lib $filter 2>&1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
    if {![regexp {running ([0-9]+) tests?} $out -> n]} { return "RED" }
    if {$n == 0} { return "NO-SUCH-TEST" }
    return [expr {[string match "*test result: ok.*" $out] ? "GREEN" : "RED"}]
}

proc run_suite {suite} {
    set f [open "| direnv exec . bash scripts/one_tcl_test.sh $suite 2>&1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
    if {![regexp {([0-9]+) passed, ([0-9]+) failed} $out -> passed failed]} { return "NO-SUITE" }
    return [expr {$failed > 0 ? "RED" : "GREEN"}]
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
