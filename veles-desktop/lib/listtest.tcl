# veles-desktop/lib/listtest.tcl — behavioural tests for lib/vlist.tcl.
#
# These drive REAL Tk widgets and send REAL key events, because the bug
# they exist to catch is an interaction between two bindings (KeyPress
# moves the cursor, KeyRelease refilled and threw it away). Reading the
# source cannot see that; only running it can.
#
# Run:  scripts/tcl_tests.sh   (under Xvfb, with the other suites)

set _dir [file dirname [file normalize [info script]]]
source [file join $_dir vlist.tcl]

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

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

wm geometry . 500x400+0+0
catch { wm attributes . -topmost 0 }

# ── a picker built exactly the way the real ones are ────────────────
set ::refills 0
proc build_picker {} {
    catch { destroy .p }
    frame .p
    entry .p.entry
    listbox .p.list -exportselection 0
    pack .p.entry .p.list -fill both -expand 1
    pack .p
    set ::refills 0
    set ::ran 0
    ::vlist::manage .p.entry .p.list \
        -refill {refill_picker} \
        -run    {set ::ran 1} \
        -close  {set ::closed 1}
    refill_picker
    focus -force .p.entry
    update
}

# Repopulates from the entry's text, like every real -refill does.
proc refill_picker {} {
    incr ::refills
    set q [string tolower [.p.entry get]]
    .p.list delete 0 end
    foreach item {alpha beta gamma delta epsilon} {
        if {$q eq "" || [string match *$q* $item]} { .p.list insert end $item }
    }
    if {![info exists ::last_q] || $q ne $::last_q} {
        set ::last_q $q
        ::vlist::reset .p.list
    } else {
        ::vlist::keep .p.list
    }
}

proc press {keysym} {
    event generate .p.entry <KeyPress-$keysym>
    event generate .p.entry <KeyRelease-$keysym>
    update
}

# ── THE regression ──────────────────────────────────────────────────
puts "cursor survives navigation"

build_picker
ok "starts at the first row" {[::vlist::get .p.list] == 0}

press Down
ok "one Down moves to row 1" {[::vlist::get .p.list] == 1}
press Down
ok "two Downs reach row 2" {[::vlist::get .p.list] == 2}
press Down
ok "three Downs reach row 3" {[::vlist::get .p.list] == 3}

# The whole point: walking must not have rebuilt the list. Every refill
# during navigation is a chance to lose the cursor, which is exactly how
# the completion popup "always scrolled back to the first item".
ok "navigation caused no refill" {$::refills == 1}

press Up
ok "Up walks back" {[::vlist::get .p.list] == 2}

# Wrap-around at both ends.
::vlist::set_cursor .p.list 4
press Down
ok "wraps past the end to the top" {[::vlist::get .p.list] == 0}
press Up
ok "wraps past the top to the end" {[::vlist::get .p.list] == 4}

# The Tk selection must agree with our cursor, or Enter runs the wrong row.
ok "listbox selection tracks the cursor" \
    {[lindex [.p.list curselection] 0] == [::vlist::get .p.list]}

# A picker whose -refill resets unconditionally — exactly what
# complete.tcl did (`variable sel 0` on every refilter). vlist cannot
# stop a callback from resetting; what it CAN do is never invoke the
# callback for a key that did not change the query. This models the
# original bug end to end.
puts "\nnaive refill callback still survives navigation"
proc refill_naive {} {
    incr ::refills
    .p.list delete 0 end
    foreach item {alpha beta gamma delta epsilon} { .p.list insert end $item }
    ::vlist::reset .p.list
}
catch { destroy .p }
frame .p
entry .p.entry
listbox .p.list -exportselection 0
pack .p.entry .p.list -fill both -expand 1
pack .p
set ::refills 0
::vlist::manage .p.entry .p.list -refill {refill_naive} -run {} -close {}
refill_naive
focus -force .p.entry
update
press Down
press Down
press Down
ok "cursor survives even a reset-happy refill callback" \
    {[::vlist::get .p.list] == 3}

# ── typing DOES refilter, and resets ────────────────────────────────
puts "\ntyping refilters"

build_picker
press Down
press Down
set before $::refills
.p.entry insert end "a"
press a
ok "a printable key refilters" {$::refills > $before}
ok "a changed query resets to the top" {[::vlist::get .p.list] == 0}

# ── same query re-rendered keeps the cursor ─────────────────────────
puts "\nre-render keeps the cursor"

build_picker
press Down
press Down
set keep [::vlist::get .p.list]
refill_picker            ;# async source landing late, same query
ok "re-render with an unchanged query keeps the row" \
    {[::vlist::get .p.list] == $keep}

# ── run / close ─────────────────────────────────────────────────────
puts "\nactivation"

build_picker
press Down
set ::ran 0
event generate .p.entry <KeyPress-Return>
update
ok "Return runs" {$::ran == 1}

set ::closed 0
event generate .p.entry <KeyPress-Escape>
update
ok "Escape closes" {$::closed == 1}

# ── the predicate other pickers share ───────────────────────────────
puts "\nnav keysym predicate"
foreach k {Up Down Prior Next Home End Return Escape Tab} {
    ok "$k is navigation" {[::vlist::is_nav_keysym $k]}
}
foreach k {a z 1 space slash BackSpace} {
    ok "$k is not navigation" {![::vlist::is_nav_keysym $k]}
}

# ── popup placement (lib/place.tcl) ─────────────────────────────────
#
# clamp() is pure arithmetic, so it can be fed the exact geometries that
# used to escape the screen. The old palette code clamped `y < 0` only
# and never touched x, which is how M-x ended up out of bounds.
source [file join $_dir place.tcl]
puts "\npopup placement clamps"

proc inside {res w h sw sh {m 8}} {
    lassign $res x y
    return [expr {$x >= $m && $y >= $m
                  && ($x + $w) <= ($sw - $m) + 1
                  && ($y + $h) <= ($sh - $m) + 1}]
}

set SW 1920 ; set SH 1080 ; set PW 640 ; set PH 380

ok "a sane position is left alone" \
    {[::vplace::clamp 100 100 $PW $PH $SW $SH] eq {100 100}}
ok "off the right edge is pulled back" \
    {[inside [::vplace::clamp 1900 100 $PW $PH $SW $SH] $PW $PH $SW $SH]}
ok "off the bottom edge is pulled back" \
    {[inside [::vplace::clamp 100 1050 $PW $PH $SW $SH] $PW $PH $SW $SH]}
ok "negative x is pulled back" \
    {[inside [::vplace::clamp -300 100 $PW $PH $SW $SH] $PW $PH $SW $SH]}
ok "negative y is pulled back (the only case the old code handled)" \
    {[inside [::vplace::clamp 100 -300 $PW $PH $SW $SH] $PW $PH $SW $SH]}
ok "both axes off at once" \
    {[inside [::vplace::clamp 5000 5000 $PW $PH $SW $SH] $PW $PH $SW $SH]}
ok "both axes negative at once" \
    {[inside [::vplace::clamp -5000 -5000 $PW $PH $SW $SH] $PW $PH $SW $SH]}

# A popup bigger than the screen cannot fit; it must still start at the
# top-left rather than being pushed off it.
lassign [::vplace::clamp 400 400 3000 3000 $SW $SH] bx by
ok "a popup larger than the screen sits at the top-left" \
    {$bx == 8 && $by == 8}

# Multi-monitor: the reported virtual screen is wide, and a window on
# the far monitor must not push the popup past the far edge.
ok "clamps against a wide virtual screen" \
    {[inside [::vplace::clamp 3800 900 $PW $PH 3840 1080] $PW $PH 3840 1080]}

puts "\nlisttest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
