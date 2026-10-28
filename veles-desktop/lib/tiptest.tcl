#!/usr/bin/env wish
# veles-desktop/lib/tiptest.tcl — the hover tooltip's reading ergonomics.
#
# USER (2026-08-31): "the tooltip already scrolls (although I have to do a
# jank movement right-clicking to get it to focus - maybe make it stay on
# screen if user holds shift or something) so it would be good if it didnt
# also clip".
#
# Three promises, all broken before this:
#   * NO CLIPPING. A forty-line hover gets a forty-line-tall tip (up to
#     65% of the screen) with a scrollbar; the tip is never placed so far
#     off an edge that content falls outside the monitor.
#   * NO FOCUS GAMES. The wheel scrolls wherever the pointer is: no click,
#     no right-click ritual.
#   * SHIFT HOLDS IT OPEN. A bare Shift press pins the tip (and motion
#     stops replacing it); releasing hands it back to the hide timer. The
#     pointer moving INTO the tip holds it too; a click pins; Escape is
#     enough to dismiss.
#
# These tests drive the real machinery against a real Tk on Xvfb — the
# same pattern as devtest.tcl. A regression that re-hardcodes a fixed
# height, drops the scrollbar, or binds a wheel to focus goes red here.

set _dir [file dirname [file normalize [info script]]]

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T

# The tooltip's collaborators, stubbed to the same surface devtest uses.
namespace eval ::vsyn {
    variable hides 0
    proc hide_tip {} { incr ::vsyn::hides }
    proc hover {w x y} {}
}
namespace eval ::vstatus { proc set_msg {m} {} }
namespace eval ::vlsp {
    variable last_text ""
    proc running {} { return 1 }
    proc render_hover {result} { variable last_text ; return $last_text }
    proc hover_at {l c cb} {}
    proc doc_path {} { return "" }
}
namespace eval ::vpanes { variable R .devroot }
source [file join $_dir i18n.tcl]
::vi18n::init
source [file join $_dir dev.tcl]

wm geometry . 800x600+0+0
frame .devroot
frame .devroot.dev
frame .devroot.dev.edit
text .devroot.dev.edit.body
pack .devroot.dev.edit.body
update idletasks
::vdev::attach .devroot.dev.edit.body

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

proc set_hover {n} {
    set ::vlsp::last_text ""
    for {set i 1} {$i <= $n} {incr i} {
        append ::vlsp::last_text \
            "documentation line number $i with enough words to wrap at the column boundary\n"
    }
}

proc drop_tip {} {
    after cancel $::vdev::tip_hide_after
    catch { destroy .vsyn_tip }
}

# ── no clipping ───────────────────────────────────────────────────────
puts "\na long hover is shown in full, not chopped at ten lines"
set_hover 40
set ::vdev::lsp_hover_pos {120 200}
::vdev::_hover_result "" ""
ok "the tip exists"                {[winfo exists .vsyn_tip]}
ok "…with a scrollbar"             {[winfo exists .vsyn_tip.sb]}
ok "…wired to the text"            {[.vsyn_tip.t cget -yscrollcommand] ne ""}
set h [.vsyn_tip.t cget -height]
ok "…tall enough for the content (got $h)" {$h > 10}

ok "placed fully inside the screen" \
   {[winfo rootx .vsyn_tip] >= 0
    && [winfo rooty .vsyn_tip] >= 0
    && [winfo rootx .vsyn_tip] + [winfo width .vsyn_tip] <= [winfo screenwidth .]
    && [winfo rooty .vsyn_tip] + [winfo height .vsyn_tip] <= [winfo screenheight .]}

drop_tip
puts "\nnear a screen edge it moves instead of spilling off"
# Geometry from the LIVE screen, never hardcoded pixels: {120 780} only
# sat at the bottom edge of an 800-tall screen and tested nothing on a
# taller one — a fixture bug dressed up as environment-proofing (the
# same trap tipzonetest's header documents).
set ::vdev::lsp_hover_pos [list 120 [expr {[winfo screenheight .] - 10}]]
::vdev::_hover_result "" ""
update idletasks
ok "bottom edge: stays on screen" \
   {[winfo rooty .vsyn_tip] >= 0
    && [winfo rooty .vsyn_tip] + [winfo height .vsyn_tip] <= [winfo screenheight .]}
drop_tip
set ::vdev::lsp_hover_pos [list [expr {[winfo screenwidth .] - 10}] 200]
::vdev::_hover_result "" ""
update idletasks
ok "right edge: stays on screen" \
   {[winfo rootx .vsyn_tip] >= 0
    && [winfo rootx .vsyn_tip] + [winfo width .vsyn_tip] <= [winfo screenwidth .]}
drop_tip
puts "\na short hover is sized to its content"
set ::vlsp::last_text "one short line"
set ::vdev::lsp_hover_pos {120 200}
::vdev::_hover_result "" ""
ok "…compact (got [.vsyn_tip.t cget -height])" {[.vsyn_tip.t cget -height] <= 4}

# ── scrolling without focus ───────────────────────────────────────────
drop_tip
set_hover 40
::vdev::_hover_result "" ""
puts "\nthe wheel scrolls from wherever the pointer is"
ok "wheel bound on the text"       {[string length [bind .vsyn_tip.t <MouseWheel>]] > 0}
ok "…and the tip needs no focus for it" \
   {![string match "*focus*" [bind .vsyn_tip.t <MouseWheel>]]}
set y0 [.vsyn_tip.t yview]
.vsyn_tip.t yview scroll 3 units
update
ok "the text scrolls while the tip is unfocused" [expr {$y0 ne [.vsyn_tip.t yview]}]

# ── lifetime: pin / enter / leave ─────────────────────────────────────
drop_tip
set_hover 5
::vdev::_hover_result "" ""
puts "\nShift holds the tip open; releasing hands it back"
ok "a fresh tip starts unpinned"   {$::vdev::tip_pin == 0}
ok "…with the hide timer armed"    {$::vdev::tip_hide_after ne ""}
::vdev::_tip_shift 1
ok "Shift press pins"              {$::vdev::tip_pin == 1}
ok "…cancelling the armed hide"    {$::vdev::tip_hide_after eq ""}
::vdev::_tip_schedule_hide 50
ok "a pinned tip refuses to arm a hide" {$::vdev::tip_hide_after eq ""}
set ::vdev::lsp_hover_after ""
::vdev::_hover_schedule 10 10
ok "…and motion does not replace it" {$::vdev::lsp_hover_after eq ""}
::vdev::_tip_shift 0
ok "release unpins and re-arms the hide" \
   {$::vdev::tip_pin == 0 && $::vdev::tip_hide_after ne ""}
drop_tip
puts "\nparking the pointer inside keeps it alive; clicking survives leaving"
set_hover 5
::vdev::_hover_result "" ""
after cancel $::vdev::tip_hide_after
::vdev::_tip_enter
ok "enter cancels the hide"        {$::vdev::tip_hide_after eq ""}
::vdev::_tip_pin
::vdev::_tip_leave
ok "a clicked/pinned tip survives the pointer leaving" \
   {[winfo exists .vsyn_tip] && $::vdev::tip_hide_after eq ""}
::vdev::_tip_shift 0
ok "after unpinning, leaving re-arms a short hide" \
   {$::vdev::tip_hide_after ne ""}
drop_tip
puts "\nEscape dismisses; the bindings are real"
set ::vdev::lsp_hover_pos {120 200}
set ::vlsp::last_text "final tip"
::vdev::_hover_result "" ""
ok "Escape on the tip destroys it" \
   {[string match "*_tip_destroy*" [bind .vsyn_tip <Escape>]]}
ok "the editor's Leave calls the qualified handler" \
   {[string match "*::vdev::_leave_editor*" [bind .devroot.dev.edit.body <Leave>]]}
# Shift pinning lives on `all`, NOT the editor widget: hover never moves
# keyboard focus, so a widget-local binding only fires when that editor
# happens to hold focus — holding Shift with the caret in the composer did
# nothing, which is exactly the user-reported "shift didn't work".
ok "Shift press binding is global (works with focus anywhere)" \
   {[string match "*::vdev::_tip_shift 1*" [bind all <KeyPress-Shift_L>]]
    && [string match "*::vdev::_tip_shift 1*" [bind all <KeyPress-Shift_R>]]}
# Release must UNPIN unconditionally: X reports the modifier mask from
# BEFORE the event, so a bare release carries the shift bit SET — a %s
# guard on release could never unpin (this ate an hour on 2026-08-31).
ok "…release unpins with no state guard" \
   {[string match "*::vdev::_tip_shift 0*" [bind all <KeyRelease-Shift_L>]]
    && ![string match "*& 0x1*" [bind all <KeyRelease-Shift_L>]]}
# The walking gap: leaving the editor must not be a 350ms death sentence,
# or no pointer can ever reach the tip before it vanishes.
ok "editor-leave grace is walkable (>= 1s)" \
   {[string match "*_tip_schedule_hide 1500*" [info body ::vdev::_leave_editor]]}
# a binding that names a missing proc raises inside the event loop: wish
# answers with a bgerror dialog and every later `update` blocks forever
# (a suite that hangs, not a suite that fails). Prove the handler exists.
ok "…and the handler exists"       {[llength [info procs ::vdev::_leave_editor]] == 1}
ok "…tip_destroy restores editor focus" \
   {[string match "*focus -force*" [info body ::vdev::_tip_destroy]]}
::vdev::_tip_destroy
ok "_tip_destroy tears everything down" {![winfo exists .vsyn_tip]}

puts "\ntiptest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
