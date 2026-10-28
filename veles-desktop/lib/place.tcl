# veles-desktop/lib/place.tcl — ONE rule for where popups appear.
#
# Three popups place themselves (command palette, which-key, completion)
# and each had its own arithmetic. All three could land off-screen,
# because none of them clamped in more than one direction.
#
# TWO DIFFERENT JOBS, DELIBERATELY SEPARATED
#
#   centered  — for popups whose position carries NO meaning. The
#               command palette is the whole app's command list; it
#               should appear in the same place every time so muscle
#               memory works. Anchoring it to the focused widget (which
#               a previous change did) makes it jump around for no
#               reason and, with focus in the transcript, off the edge.
#
#   near      — for popups that ANNOTATE something. which-key describes
#               the chord you just pressed and the completion popup
#               describes the word you are typing, so both belong next
#               to the thing they are about.
#
# Both clamp to the screen on BOTH axes in BOTH directions. The previous
# code clamped `y < 0` only and never touched x at all.

namespace eval ::vplace {
    # Space kept between a popup and the screen edge.
    variable margin 8
}

# The usable screen box for the display $w is on. Multi-monitor Tk
# reports one virtual screen, so this is the whole desktop; the clamp
# below still keeps popups inside it, which is what went wrong before.
proc ::vplace::screen_box {w} {
    return [list 0 0 [winfo vrootwidth $w] [winfo vrootheight $w]]
}

# Clamp a proposed x/y so a width x height box sits fully on screen.
# Pure arithmetic, so it can be tested without mapping anything.
proc ::vplace::clamp {x y width height sw sh {override -1}} {
    # NB: the parameter is not called `margin` — `variable margin` would
    # collide with it and Tcl reports "variable already exists".
    set m [expr {$override >= 0 ? $override : $::vplace::margin}]
    # Right/bottom first, then left/top — so a popup LARGER than the
    # screen ends up flush at the top-left rather than pushed off it.
    set maxx [expr {$sw - $width - $m}]
    set maxy [expr {$sh - $height - $m}]
    if {$x > $maxx} { set x $maxx }
    if {$y > $maxy} { set y $maxy }
    if {$x < $m} { set x $m }
    if {$y < $m} { set y $m }
    return [list $x $y]
}

# Position a toplevel centred horizontally on the main window, `frac`
# of the way down it. For popups whose location should never surprise.
proc ::vplace::centered {w width height {frac 0.15}} {
    variable margin
    # The sizes are desktop pixels. On a phone a desktop pixel is an
    # Android dp, and the dialog fits in what the keyboard leaves of the
    # screen, AndroWish's frame round it included (lib/android.tcl): a
    # 380 px dialog was a third of what it needed on the emulator, and
    # the setup wizard's buttons sat under the keyboard.
    if {[llength [info commands ::vandroid::on]] && [::vandroid::on]} {
        return [::vandroid::fit $w $width $height $frac]
    }
    set px [winfo rootx .]
    set py [winfo rooty .]
    set pw [winfo width .]
    set ph [winfo height .]
    # Before the main window is mapped these are 1x1; fall back to the
    # screen so the popup is still somewhere sane.
    lassign [screen_box $w] _ _ sw sh
    if {$pw <= 1 || $ph <= 1} {
        set px 0 ; set py 0 ; set pw $sw ; set ph $sh
    }
    # Never larger than the screen, or the clamp below has nowhere to put it.
    set width [expr {min($width, $sw - 2 * $margin)}]
    set height [expr {min($height, $sh - 2 * $margin)}]
    set x [expr {$px + ($pw - $width) / 2}]
    set y [expr {$py + int($ph * $frac)}]
    lassign [clamp $x $y $width $height $sw $sh] x y
    wm geometry $w "${width}x${height}+${x}+${y}"
    return [list $x $y]
}

# Position a toplevel next to $anchor — above it by default, below when
# there is no room above. For popups that annotate a specific widget.
# Place a popup that sizes itself from its own contents. Call AFTER the
# contents are packed: it asks Tk for the requested size and clamps
# against that, rather than against a guess.
#
# Guessing is not good enough. which-key estimated `rows * 20 + 34` and
# was placed before its labels existed; the real window came out 86px
# tall against a 74px estimate, so the clamp let it hang 4px off the
# bottom of the screen. Measure, then clamp.
proc ::vplace::near_measured {w anchor {gap 6}} {
    update idletasks
    set width [winfo reqwidth $w]
    set height [winfo reqheight $w]
    return [near $w $anchor $width $height $gap move]
}

# `mode` is "full" (set size and position) or "move" (position only,
# letting Tk keep its requested size — which-key sizes itself from its
# contents and must not be pinned to an estimate).
proc ::vplace::near {w anchor width height {gap 6} {mode full}} {
    if {$anchor eq "" || ![winfo exists $anchor]} { set anchor . }
    lassign [screen_box $w] _ _ sw sh
    set ax [winfo rootx $anchor]
    set ay [winfo rooty $anchor]
    set ah 30
    catch { set ah [winfo height $anchor] }

    set x [expr {$ax + 12}]
    set y [expr {$ay - $height - $gap}]
    # No room above → go below the widget instead of overlapping it.
    if {$y < $::vplace::margin} { set y [expr {$ay + $ah + $gap}] }
    lassign [clamp $x $y $width $height $sw $sh] x y
    if {$mode eq "move"} {
        wm geometry $w "+${x}+${y}"
    } else {
        wm geometry $w "${width}x${height}+${x}+${y}"
    }
    return [list $x $y]
}
