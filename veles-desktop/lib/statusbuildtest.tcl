#!/usr/bin/env wish
# statustest extension: ::vstatus::build must at least be CALLABLE —
# the +{...} bind syntax bug (three words to Tcl) crashed main.tcl at
# startup but no suite ever called build(), because the statustest
# stub built the widgets by hand. This maps real widgets, calls build,
# and checks the click binding landed.

set _dir [file dirname [file normalize [info script]]]
if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir i18n.tcl]
::vi18n::init
# a11y BEFORE statusbar: build() calls ::va11y::name, and the whole
# point of the third assertion is that the tooltip bind and the
# statusbar's own cursor bind COEXIST — which needs the real thing.
source [file join $_dir a11y.tcl]
namespace eval ::vchat { variable session "" }
namespace eval ::vapi { proc unattached {} { return 0 } }
source [file join $_dir statusbar.tcl]

set failed 0
set passed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

# build() creates the whole bar from scratch — this is the line that
# died with "wrong # args: should be bind window ?pattern? ?command?"
::vstatus::build
update idletasks

ok "the bar actually built" {[winfo exists .statusbar.right]}
ok "the context label takes a click handler" \
    {[bind .statusbar.right <Button-1>] ne ""}
# The va11y tooltip bind and the cursor bind must COEXIST — a plain
# second bind would have replaced the first (and a +{...} outside the
# braces would have crashed startup, which is why this suite exists).
ok "hover keeps both the tooltip and the hand cursor" \
    {[string match "*va11y*" [bind .statusbar.right <Enter>]]
     && [string match "*cursor*" [bind .statusbar.right <Enter>]]}


# ── a hint must not shove the bar it is a hint ABOUT ─────────────────
#
# USER 2026-09-18: "hovering over the plan 55% in the status line …
# instead of a tooltip it displays the tooltip message in the status
# line itself, moving all the status items and making it impossible to
# actually click it."
#
# ::va11y::_show_tip announces into .statusbar.msg. That label is packed
# -side left; .right/.route/.quota are packed -side right AFTER it, and
# Tk allocates in PACKING ORDER — so a long left label takes its width
# out of the cavity first and the whole right-hand group is squeezed and
# shifted. The quota hint is ~90 characters, so hovering "plan 55%"
# moved "plan 55%" out from under the pointer.
wm geometry . 700x220
update idletasks
.statusbar.quota configure -text "plan 55%"
.statusbar.route configure -text "zai/glm-5"
.statusbar.right configure -text "in 1.2k / out 900"
::vstatus::set_msg "gateway up 3s"
update idletasks
# MEASURED, 700x220, before the fix: .statusbar.msg grew 85px -> 582px,
# .statusbar.right shrank 95 -> 56 and slid right, and .route and .quota
# went UNMAPPED — so "plan 55%" did not merely move out from under the
# pointer, it left the bar. That is the "impossible to actually click
# it" half, exactly.
set _qw [winfo width .statusbar.quota]
set _rw [winfo width .statusbar.right]
set _barw [winfo width .statusbar]
::vstatus::set_msg "how much of the upstream plan is used — click for the full report · read 2m ago · refreshed every 15m"
update idletasks
ok "a long message leaves the quota label ON THE BAR" \
    {[winfo ismapped .statusbar.quota]}
ok "…and the route label" \
    {[winfo ismapped .statusbar.route]}
ok "…at the width they asked for, not squeezed" \
    {[winfo width .statusbar.quota] == $_qw \
        && [winfo width .statusbar.right] == $_rw}
ok "…and does not widen the bar past its window" \
    {[winfo width .statusbar] <= $_barw}

# ── the bar is BORROWED for a hint, not overwritten ──────────────────
#
# A tip that cannot be placed falls back to the bar (tipzonetest pins
# that, and it must stay). What it must not do is keep the bar: nothing
# put the previous message back, so a hover left the status line reading
# a tooltip until the 15s poll overwrote it.
::vstatus::set_msg "gateway up 3s"
::va11y::keepout .statusbar.msg          ;# force the no-floating-tip path
::va11y::name .statusbar.msg "the status message"
::va11y::_show_tip .statusbar.msg 10 10
update idletasks
ok "the fallback still says it in the bar" \
    {[.statusbar.msg cget -text] eq "the status message"}
::va11y::_hide_tip
update idletasks
ok "…and hiding it gives the previous message back" \
    {[.statusbar.msg cget -text] eq "gateway up 3s"}

puts "\nstatusbuildtest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
