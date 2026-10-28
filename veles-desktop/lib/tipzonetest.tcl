#!/usr/bin/env wish
# veles-desktop/lib/tipzonetest.tcl — where a hover tip is allowed to land.
#
# USER (2026-09-06): "when I hover over the conversation transcript the
# 'conversation transcript' tooltip obscures the beginning of the message
# composer".
#
# The mechanism: ::va11y::_show_tip placed the tip below the ANCHORED
# WIDGET'S BOTTOM EDGE. The transcript is a ~full-height pane, so that
# edge is the seam with the composer — hovering it at ANY height parked a
# topmost card on the composer's first line for four seconds. (An earlier
# pass fixed a different symptom in the same function: geometry was set
# before the tip had a measured size, so the WM dropped it dead centre.
# Measuring fixed the dump, not the overlap.)
#
# The promise now:
#   * The tip is ANCHORED TO THE POINTER, not to the widget's extent.
#   * A registered keep-out zone (the composer band) is never covered:
#     candidates are rejected until one is clear.
#   * No clear spot means NO floating tip — the status bar still carries
#     the text, so the information never depends on placement.
#   * A protected SURFACE gets no tip of its own ("message composer" on
#     the composer is the same bug in a different label), while the
#     controls INSIDE the band keep theirs, placed clear of it.
#
# Geometry comes from the live screen, never from a hardcoded y: tiptest's
# "bottom edge" case pins {120 780} and goes red on any monitor taller
# than 1200 — a fixture bug dressed up as a regression.
#
# Run: scripts/tcl_tests.sh

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
source [file join $_dir a11y.tcl]

namespace eval ::vstatus {
    variable last ""
    proc set_msg {m} { set ::vstatus::last $m }
}

set sw [winfo screenwidth .]
set sh [winfo screenheight .]

# ── the stage ────────────────────────────────────────────────────────
# .band is the composer band (a bottom bar the height of input + button
# row); .pane is the transcript, whose bottom edge sits INSIDE that band
# — exactly the transcript's relationship to the composer in the app,
# where pack order puts the log's last pixels across the seam. .pane.send
# is a control living inside the band, the bar's own Send button.
set band_h 80
set band_y [expr {$sh - $band_h}]

toplevel .band
wm overrideredirect .band 1
wm geometry .band ${sw}x${band_h}+0+$band_y
frame .band.send
place .band.send -x 40 -y 8 -width 120 -height 30

toplevel .pane
wm overrideredirect .pane 1
wm geometry .pane ${sw}x[expr {$band_y + 20}]+0+0
update idletasks

::va11y::name .pane "conversation transcript"
::va11y::name .band "message composer"
::va11y::name .band.send "send the message"
::va11y::keepout .band
update idletasks

set failed 0
set passed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ok  $name"
        incr ::passed
    } else {
        puts "  FAIL $name"
        incr ::failed
    }
}

# `show` = settle, drive, settle.
#
# TWO reasons for the updates, both learned live:
#  - Without the trailing `update` the assertions read the off-screen
#    +10000+10000 staging position, because the geometry request has not
#    round-tripped. "Does it cover the band" then passes vacuously and
#    "is it on screen" fails for a tip that is placed correctly. Both
#    directions of that lie bit this file.
#  - Xvfb parks the POINTER AT SCREEN CENTRE, so resizing a stage window
#    over it fires a real <Enter> on whatever `va11y::name` installed, and
#    that handler re-announces over the message under test. So: drain first
#    (`update`), clear the latch AFTER the drain, then drive. Anything else
#    asserts on an event, not on the call.
proc show {args} {
    update
    ::va11y::_hide_tip
    set ::vstatus::last ""
    eval ::va11y::_show_tip $args
    update
}

# Does the mapped tip overlap the band?
proc covers_band {} {
    if {![winfo exists .va11y_tip]} { return 0 }
    set x [winfo rootx .va11y_tip]
    set y [winfo rooty .va11y_tip]
    expr {$x < $::sw && $x + [winfo width .va11y_tip] > 0
          && $y < $::band_y + $::band_h && $y + [winfo height .va11y_tip] > $::band_y}
}
proc on_screen {} {
    set x [winfo rootx .va11y_tip]
    set y [winfo rooty .va11y_tip]
    expr {$x >= 0 && $y >= 0
          && $x + [winfo width .va11y_tip] <= $::sw
          && $y + [winfo height .va11y_tip] <= $::sh}
}
proc cleared {} { ::va11y::_hide_tip; set ::vstatus::last "" }

# How far the tip sits from the POINTER, in pixels (0 = it touches). The
# suite needs this because "did not cover the composer" is satisfiable by
# any placement that happens to be elsewhere: a mutant that ignored the
# pointer and used the keyboard branch's edge-clamp still cleared the band
# and stayed on screen, and the suite stayed green while the feature was
# gone. Anchoring to the pointer is the other half of the fix, so it is
# asserted on its own.
proc pointer_gap {px py} {
    if {![winfo exists .va11y_tip]} { return -1 }
    set x [winfo rootx .va11y_tip]
    set y [winfo rooty .va11y_tip]
    set w [winfo width .va11y_tip]
    set h [winfo height .va11y_tip]
    set dx [expr {$px < $x ? $x - $px : ($px > $x + $w ? $px - $x - $w : 0)}]
    set dy [expr {$py < $y ? $y - $py : ($py > $y + $h ? $py - $y - $h : 0)}]
    return [expr {int(sqrt($dx * $dx + $dy * $dy))}]
}

# ── the registration ────────────────────────────────────────────────
puts "\nprotected surfaces"
# The binding is half the mechanism: every placement test above passes
# explicit px/py, so a regression that drops them from the <Enter> script
# (back to pointer-less widget anchoring) would stay green here. Assert the
# real handler asks X for the root coordinates.
ok "the hover binding carries the pointer" \
    {[string match "*_show_tip %W %X %Y*" [bind .pane <Enter>]]}
# Coexistence, the other direction: a handler installed BEFORE va11y::name
# must survive it. Ours is bound with a leading `+`; `bind` without it
# REPLACES, which is how the statusbar once lost its hand cursor (that
# crash is statusbuildtest's reason for existing).
#
# Asserted on the STORED SCRIPT, not by generating an <Enter>. The first
# attempt here did `event generate .coexist <Enter> -rootx 300 -rooty 300`
# and read the handler's side effect: it went red, because a fresh toplevel
# is unmapped and 1x1 at the origin, so the coordinates are outside it and
# Tk discards the event. The companion "both ran" assertion went GREEN for
# the same reason there was no event to lose — it saw a .va11y_tip left over
# from an earlier case. A synthetic event that never arrives tests nothing in
# either direction; the append semantics are fully visible in the binding.
toplevel .coexist
bind .coexist <Enter> {set ::coexist_ran "theirs"}
::va11y::name .coexist "a coexisting control"
set _b [bind .coexist <Enter>]
ok "…a handler installed first survives"   {[string match "*theirs*" $_b]}
ok "…and ours is appended to it"           {[string match "*_show_tip*" $_b]}
destroy .coexist
ok "the band is protected"             {[::va11y::_is_zone .band]}
ok "the transcript is not"              {![::va11y::_is_zone .pane]}
ok "a control inside the band is not"    {![::va11y::_is_zone .band.send]}
# A refused tip must not leave the PREVIOUS tip's hide timer armed: that
# stale timer fires seconds later and destroys an unrelated tip still
# wanted. Driven RAW, not through `show` — `show` pre-clears, which would
# cancel the timer for the test rather than proving the refusal path does.
# Both calls run with the geometry untouched: no window resize between
# them, so no stray pointer event can re-arm anything (see `show`).
# Is a timer id still PENDING? `after info` RAISES for a cancelled id
# rather than answering empty, so the honest test needs the catch — and
# the first assertion is what proves the second means anything: without a
# live timer to begin with, "it is gone now" passes for free.
proc timer_live {id} {
    if {$id eq ""} { return 0 }
    if {[catch {after info $id} r]} { return 0 }
    return [expr {$r ne ""}]
}
::va11y::_show_tip .pane 200 60
update
set _timer $::va11y::tip_after
ok "a shown tip arms its hide"           {[timer_live $_timer]}
::va11y::_show_tip .band 300 [expr {$band_y + 10}]
update
ok "the refusal cancelled the pending hide" {![timer_live $_timer]}
ok "…and left no tip window"              {![winfo exists .va11y_tip]}
cleared

set _before [lsearch -all -exact $::va11y::keepouts .band]
::va11y::keepout .band
::va11y::keepout .band
ok "keepout is idempotent" \
    {[lsearch -all -exact $::va11y::keepouts .band] eq $_before}
# A registered but unmapped zone must not veto the screen with an
# origin-sized rect: that is what "no tooltip anywhere" looks like.
toplevel .unmapped
wm withdraw .unmapped
::va11y::keepout .unmapped
update idletasks
ok "an unmapped zone contributes no rect" \
    {[llength [::va11y::_keepout_rects]] == 1}
destroy .unmapped
# A rebuild destroys and recreates widgets, so nobody unregisters. A path
# with no widget behind it must simply contribute nothing.
::va11y::keepout .gone
update idletasks
ok "a dead path is inert, not a veto" \
    {[llength [::va11y::_keepout_rects]] == 1}

# ── the reported bug ────────────────────────────────────────────────
puts "\nhovering the transcript near the composer"
set px 200
set py [expr {$band_y - 10}]              ;# pointer just above the band
show .pane $px $py
ok "the tip still appears"               {[winfo exists .va11y_tip]}
ok "…and never covers the composer"      {![covers_band]}
ok "…nor leaves the screen"               {[on_screen]}
set gap [pointer_gap $px $py]
ok "…stays by the pointer it hovered ($gap px away)" {$gap <= 60}
# Calibration: the OLD rule (below the anchored widget's bottom edge) must
# be refused on its own merits. Without this the suite passes vacuously on
# a monitor tall enough that the old placement also happens to clear.
set old_y [expr {[winfo rooty .pane] + [winfo height .pane] + 4}]
ok "the old widget-bottom spot is what is rejected" \
    {![::va11y::_spot_ok 12 $old_y 300 40 $sw $sh [::va11y::_keepout_rects]]}
cleared

puts "\nhovering it at the top of the pane"
show .pane 200 60
ok "a tip mid-pane appears"              {[winfo exists .va11y_tip]}
ok "…and does not travel to the composer" {![covers_band]}
cleared

# ── a tip that IS shown does not also take the status bar ───────────
#
# The bar write began as the whole feature and stayed on as a
# belt-and-braces extra once the floating tip existed — so the failure
# mode the tip replaced went on running IN ADDITION to it. USER
# 2026-09-18: hovering "plan 55%" put a 90-character hint into
# .statusbar.msg, and the packer answered by unmapping the label being
# hovered (statusbuildtest measures that half). The bar is the FALLBACK
# for a tip that cannot be placed honestly — the two cases below still
# pin that — and nothing more.
puts "\na placed tip leaves the bar alone"
show .pane 200 60
ok "the tip is up"                        {[winfo exists .va11y_tip]}
ok "…and the status bar was not written"  {$::vstatus::last eq ""}
cleared

# ── a protected surface gets no card of its own ─────────────────────
puts "\nthe composer's own name"
show .band 300 [expr {$band_y + 10}]
ok "no floating tip on the band"          {![winfo exists .va11y_tip]}
ok "the status bar still says it"         {$::vstatus::last eq "message composer"}
cleared

# ── controls inside the band keep their hints ───────────────────────
puts "\na button inside the band"
set sx [expr {[winfo rootx .band.send] + 60}]
set sy [expr {[winfo rooty .band.send] + 15}]
show .band.send $sx $sy
ok "the tip appears"                      {[winfo exists .va11y_tip]}
ok "…placed clear of the band"            {![covers_band]}
ok "…and still on screen"                 {[on_screen]}
cleared

# ── keyboard focus-in has no pointer ────────────────────────────────
puts "\nfocus-in placement"
show .band.send
ok "a tip with no pointer lands somewhere legal" \
    {![winfo exists .va11y_tip] || (![covers_band] && [on_screen])}
cleared
show .pane
ok "and the transcript keeps a keyboard tip" \
    {![winfo exists .va11y_tip] || (![covers_band] && [on_screen])}
cleared

# ── nowhere legal: the tip is dropped, not forced ───────────────────
puts "\nno free spot"
wm geometry .band ${sw}x${sh}+0+0        ;# the band IS the screen now
update idletasks
set ::vstatus::last ""
show .pane 40 40
ok "no tip when nothing is clear"          {![winfo exists .va11y_tip]}
ok "but the message is not lost"           {$::vstatus::last eq "conversation transcript"}
cleared

# ── unit: which band is "mine" ──────────────────────────────────────
# The band-clearing fallback used to take the extreme across ALL zones.
# With split IRC views each registering an input, that flung a hint to the
# top of the tallest pane anywhere on the monitor.
puts "\n_nearest_zone"
set zr [list [list 0 944 1280 1024] [list 0 500 640 560]]
ok "a point inside a band gets that band"    {[::va11y::_nearest_zone 640 530 $zr] eq {500 560}}
ok "…not the global extreme"                 {[::va11y::_nearest_zone 640 530 $zr] ne {500 1024}}
ok "the lower band is still found"           {[::va11y::_nearest_zone 640 1000 $zr] eq {944 1024}}
ok "between two bands, the closer wins"      {[::va11y::_nearest_zone 640 700 $zr] eq {500 560}}
ok "a point far above, the nearest below"    {[::va11y::_nearest_zone 640 100 $zr] eq {500 560}}
ok "no rects means nothing to clear"         {[::va11y::_nearest_zone 1 1 {}] eq ""}

# Two bands, the pointer inside the LOWER one: the band you point at beats
# the band elsewhere on the monitor. This is the split-IRC case — each pane
# registers its own input, so a global extreme would fling the hint to the
# top of whichever pane happened to be highest.
puts "\ntwo bands at once"
# The "no free spot" case above left .band covering the whole screen and
# nothing undid it, so this section's expected rects were both wrong until
# the stage was restored. A fixture that mutates shared geometry must put
# it back, or later cases assert against a screen nobody configured.
wm geometry .band ${sw}x${band_h}+0+$band_y
set b2_y [expr {$band_y - 200}]
toplevel .band2
wm overrideredirect .band2 1
wm geometry .band2 300x60+0+$b2_y
update idletasks
::va11y::keepout .band2
set _rects [::va11y::_keepout_rects]
ok "both bands are on the list" {[llength $_rects] == 2}
# Read the band back from the SAME source the code uses, so the assertion
# tests the choice and not my arithmetic about where .band landed.
ok "the band the pointer is inside wins" \
    {[::va11y::_nearest_zone 640 [expr {$band_y + 10}] $_rects]
     eq [list $band_y [expr {$band_y + [winfo height .band]}]]}
ok "…and a point in the upper band gets the upper one" \
    {[::va11y::_nearest_zone 100 [expr {$b2_y + 10}] $_rects]
     eq [list $b2_y [expr {$b2_y + [winfo height .band2]}]]}
show .band.send $sx $sy
# The tip may or may not survive two bands; both outcomes are legal. What
# is never legal is covering one, or the assertion silently checking a
# destroyed window (which aborts the whole suite — it did on the first run
# of this case, because `covers_band` guards for existence and a raw
# `winfo rooty` does not).
ok "either no tip, or a tip that clears both bands" \
    {![winfo exists .va11y_tip]
     || (![covers_band]
         && !([winfo rooty .va11y_tip] < $b2_y + [winfo height .band2]
              && [winfo rooty .va11y_tip] + [winfo height .va11y_tip] > $b2_y))}
::va11y::_hide_tip
destroy .band2
update idletasks
ok "a destroyed zone stops mattering" \
    {[llength [::va11y::_keepout_rects]] == 1}

# ── unit: the reject test itself ────────────────────────────────────
puts "\n_spot_ok"
set r [list [list 100 100 200 200]]
ok "clear of the zone"                     {[::va11y::_spot_ok 10 10 40 20 1000 800 $r]}
ok "sharing an edge is not covering"       {[::va11y::_spot_ok 60 110 40 20 1000 800 $r]}
ok "one pixel inside is"                   {![::va11y::_spot_ok 64 110 40 20 1000 800 $r]}
ok "overlapping it is"                     {![::va11y::_spot_ok 120 120 40 20 1000 800 $r]}
ok "off the right edge"                    {![::va11y::_spot_ok 990 10 40 20 1000 800 $r]}
ok "off the bottom edge"                   {![::va11y::_spot_ok 10 795 40 20 1000 800 $r]}
ok "no zones means no veto"                {[::va11y::_spot_ok 120 120 40 20 1000 800 {}]}

puts "\ntipzonetest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
