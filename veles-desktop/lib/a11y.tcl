# veles-desktop/lib/a11y.tcl — accessibility knobs (l10n/i18n/a11y plan).
#
# ui.font_scale from the config dir's config.toml (default 1.0) scales every
# stock Tk font at boot, before any widget is created — the whole UI
# (chat transcript, composer, dialogs, sidebar) follows.
#
#   [ui]
#   font_scale = 1.25
#
# Pairs with the built-in `high-contrast` / `large-print` themes.
#
# ── accessible names ─────────────────────────────────────────────────
# Icon-only buttons (★ ✎ ✕ ⟳ ↑ ↓ …) carry no text for assistive tech
# to read. ::va11y::name attaches a human description to such a widget:
#   - stored as an array entry (::va11y::names) — introspectable by any
#     AT bridge / test harness walking the widget tree
#   - surfaced as a tooltip-style message on <Enter> and on keyboard
#     focus-in, so sighted keyboard users get the same information
#   - keyboard activation: Space/Return "press" the button even when
#     its class binding doesn't (belt-and-braces; ttk usually does)
#
# Usage: ::va11y::name $widget "Delete selected session"
#
# ── keep-out zones ───────────────────────────────────────────────────
# A tip is an interruption; some pixels must never be interrupted. The
# composer band is the case in point (USER 2026-09-06: "when I hover over
# the conversation transcript the 'conversation transcript' tooltip
# obscures the beginning of the message composer"). A tip that covers
# where you are typing is worse than no tip, so ::va11y::keepout marks
# widgets a floating tip may never overlap, and _show_tip refuses every
# candidate position that intersects one — landing on no free spot means
# no floating tip at all (the status bar still carries the text, so the
# information itself never depends on placement).

namespace eval ::va11y {
    variable font_scale 1.0
    variable names
    ;# The `after` id that hides the floating tip — cancelled on every
    ;# show and hide, so a quick hover-over never stacks timers.
    variable tip_after ""
    ;# What the status bar said before a FALLBACK tip borrowed it. The
    ;# bar is only ever written when no floating tip could be placed
    ;# (see _show_tip), and _hide_tip puts the original back — a hover
    ;# must not leave the status line reading a tooltip until the next
    ;# 15s poll happens to overwrite it.
    variable bar_saved ""
    ;# Paths registered as keep-out zones (see above). Dead and unmapped
    ;# ones are filtered at query time, so a rebuild need not unregister.
    variable keepouts {}
}

# Read + apply font scaling. Call once at boot, before widgets.
proc ::va11y::init {} {
    variable font_scale
    set cfg [::vpaths::config_file]
    if {[file exists $cfg]} {
        if {![catch {
            set fh [open $cfg r]; set data [read $fh]; close $fh
        }]} {
            if {[regexp {(?n)^\s*font_scale\s*=\s*([0-9.]+)} $data -> s]} {
                if {$s >= 0.75 && $s <= 3.0} { set font_scale $s }
            }
        }
    }
    if {$font_scale == 1.0} { return 1.0 }
    foreach fam {TkDefaultFont TkTextFont TkFixedFont TkMenuFont \
                 TkHeadingFont TkTooltipFont TkSmallCaptionFont \
                 TkIconFont TkCaptionFont} {
        if {[catch {font configure $fam -size} cur]} { continue }
        font configure $fam -size [expr {int(round($cur * $font_scale))}]
    }
    return $font_scale
}

# Attach an accessible description to (usually icon-only) widgets.
# Safe to call on any widget path; no-ops when the widget doesn't exist.
proc ::va11y::name {w desc} {
    variable names
    if {![winfo exists $w]} { return }
    set names($w) $desc
    # Tooltip on hover — APPENDED to whatever is already bound, so a
    # caller's own Enter binding (the statusbar's hand cursor) is not
    # replaced by ours. `bind` REPLACES; this is the only way two
    # concerns share one event.
    # %X,%Y: the pointer's ROOT coordinates at Enter. The tip is placed
    # against the pointer, not against the widget's bottom edge — an
    # Enter on the transcript at the seam with the composer used to park
    # the tip on the composer's first line (see _show_tip).
    bind $w <Enter> {+ if {[llength [info commands ::va11y::_show_tip]]} { ::va11y::_show_tip %W %X %Y } }
    bind $w <Leave> {+ ::va11y::_hide_tip }
    # Announce on keyboard focus-in too (focus follows tab).
    bind $w <FocusIn> [list ::va11y::_show_tip $w]
    bind $w <FocusOut> [list ::va11y::_hide_tip]
    # Keyboard activation: ttk's own TButton class binding already maps
    # <space> → activate → invoke once the widget can take focus (which
    # -takefocus 1 above guarantees). No extra bind here — adding one
    # caused DOUBLE invocation alongside the class binding.
    # Take focus so Tab reaches icon buttons (ttk buttons are
    # takefocus-capable but some styles disable it).
    catch { $w configure -takefocus 1 }
}

# Re-resolve every stored accessible name through the current catalog.
#
# Same problem as widget -text: ::va11y::name stores the RENDERED string
# at call time (85 call sites pass [::vi18n::mc {…}]), so after a swap
# every accessible name and tooltip still described the old language. The
# fix mirrors the widget sweep — reverse-map each stored value back to its
# key and re-render — and lives here rather than in a11y.tcl so the
# reverse map stays in one place.
proc ::va11y::relabel_names {} {
    variable names
    if {![info exists names]} { return 0 }
    set changed 0
    foreach w [array names names] {
        set cur $names($w)
        set key [::vi18n::key_for $cur]
        if {$key eq ""} { continue }
        set want [::vi18n::mc $key]
        if {$want ne $cur} { set names($w) $want ; incr changed }
    }
    return $changed
}

proc ::va11y::describe {w} {
    variable names
    if {[info exists names($w)]} { return $names($w) }
    return ""
}

# Borrow the status bar for a hint, and give it back.
#
# Only the fallback paths call these: a tip that landed on screen has
# said its piece and must leave the bar alone (USER 2026-09-18 — a hint
# written into .statusbar.msg grows that label, and the packer answers
# by unmapping the right-hand group, including the very item being
# hovered). When there is nowhere legal to put a card the bar is still
# how the information arrives, which is what it was built for.
proc ::va11y::_bar_borrow {msg} {
    variable bar_saved
    if {$bar_saved eq ""} {
        catch { set bar_saved [.statusbar.msg cget -text] }
    }
    catch { ::vstatus::set_msg $msg }
}

proc ::va11y::_bar_return {} {
    variable bar_saved
    if {$bar_saved eq ""} { return }
    set was $bar_saved
    set bar_saved ""
    catch { ::vstatus::set_msg $was }
}

# Mark a widget a floating tip may never overlap. Idempotent; safe to
# call on a widget that does not exist yet (tests and panes built later
# both register at build time).
proc ::va11y::keepout {w} {
    variable keepouts
    if {$w in $keepouts} { return }
    lappend keepouts $w
}

# The registered zones as {x0 y0 x1 y1} in ROOT coordinates, mapped and
# non-empty only. Recomputed per tip — a pane that resized or a bar that
# grew a second row is reflected immediately, and no invalidation hook is
# needed (there are at most a handful of zones).
proc ::va11y::_keepout_rects {} {
    variable keepouts
    set out {}
    foreach w $keepouts {
        if {![winfo exists $w] || ![winfo ismapped $w]} { continue }
        set x [winfo rootx $w]
        set y [winfo rooty $w]
        set wt [winfo width $w]
        set ht [winfo height $w]
        if {$wt < 2 || $ht < 2} { continue }
        lappend out [list $x $y [expr {$x + $wt}] [expr {$y + $ht}]]
    }
    return $out
}

# Is this widget ITSELF a protected surface? Then it gets no floating tip
# at all: a "message composer" card dumped on the composer is the same
# bug with a different label, and you are already looking at the thing.
#
# This is an IDENTITY test, not containment. The Send/attach/reasoning
# buttons live inside the bar's rectangle but are not zones themselves,
# so they keep their hints — they just get placed clear of the band
# (candidate 2, above the pointer) instead of into it.
proc ::va11y::_is_zone {w} {
    variable keepouts
    return [expr {$w in $keepouts}]
}

# The band nearest (x,y), as {top bottom}; {"" ""} only when there are no
# rects at all. Distance is vertical+horizontal to the rect, so a control
# INSIDE a band scores 0 and always wins over a band elsewhere on screen.
#
# Why not the extreme over all zones (a global min-top/max-bottom): split
# IRC views register one input per pane, and the chat band is registered
# too, so hovering a statusbar button would be flung to the top of the
# tallest pane on the monitor. The band you are pointing at is the one that
# has to be cleared.
proc ::va11y::_nearest_zone {x y rects} {
    set best ""; set bestd ""
    foreach r $rects {
        lassign $r rx0 ry0 rx1 ry1
        set dy [expr {$y < $ry0 ? $ry0 - $y : ($y > $ry1 ? $y - $ry1 : 0)}]
        set dx [expr {$x < $rx0 ? $rx0 - $x : ($x > $rx1 ? $x - $rx1 : 0)}]
        set d [expr {$dx + $dy}]
        if {$bestd eq "" || $d < $bestd} {
            set bestd $d
            set best [list $ry0 $ry1]
        }
    }
    return $best
}

# Does a tw x th box at root (x,y) fit on screen and clear every keep-out
# rect? Shared edges do not count as covering — two pixels of overlap do.
proc ::va11y::_spot_ok {x y tw th sw sh rects} {
    if {$x < 4 || $y < 4} { return 0 }
    if {$x + $tw > $sw - 4 || $y + $th > $sh - 4} { return 0 }
    foreach r $rects {
        lassign $r rx0 ry0 rx1 ry1
        if {$x < $rx1 - 2 && $x + $tw > $rx0 + 2
            && $y < $ry1 - 2 && $y + $th > $ry0 + 2} { return 0 }
    }
    return 1
}

# The floating tip itself. (This began life as a status-line-only
# announcement and the header never followed the window; the status bar is
# still written, now as the fallback for a tip that cannot be placed
# honestly and is dropped.)
#
# px/py: the pointer's ROOT coordinates when the tip came from a hover
# (%X %Y, passed by the Enter binding). Keyboard focus-in has no pointer
# and passes nothing, which anchors beside the widget instead.
proc ::va11y::_show_tip {w {px ""} {py ""}} {
    # A REAL floating tip — a small auto-hiding toplevel near the
    # widget. The statusbar variant it replaced was trampled by the
    # 15s status poll before anyone could read it, which is exactly
    # why hovering the "reasoning" checkbox told you nothing.
    # USER: "there don't seem to be tooltips/status bar hints for
    # everything".
    set msg [::va11y::announce $w]
    if {$msg eq ""} { return }
    if {![winfo exists $w]} { return }
    # A protected surface gets no floating tip of its own (see _is_zone).
    # Checked before the window is built: no mapping, measuring and
    # discarding a card that was never going to show. Nor does a touch
    # screen, which has no hover: a tap is an Enter with no Leave, and
    # the card sat over what was just tapped (lib/android.tcl).
    if {[::va11y::_is_zone $w]
        || ([llength [info commands ::vandroid::on]] && [::vandroid::on])} {
        ::va11y::_hide_tip
        ::va11y::_bar_borrow $msg
        return
    }
    set t .va11y_tip
    if {[winfo exists $t]} { destroy $t }
    toplevel $t
    wm overrideredirect $t 1
    wm attributes $t -topmost 1
    label $t.l -text $msg -foreground $::vt::T(bg_input) \
        -background $::vt::T(accent) -padx 6 -pady 3 \
        -font {TkDefaultFont 10} -wraplength 360 -justify left
    pack $t.l
    # Render OFF-SCREEN first, measure, THEN place. Setting geometry
    # before the window has a size gets the size ignored and the tip
    # lands wherever the WM feels like — on this machine, dead centre
    # over the composer's first line (USER: "the tooltip renders inside
    # the message composer and obscures the first few words").
    wm geometry $t +10000+10000
    update idletasks
    set tw [winfo reqwidth $t]
    set th [winfo reqheight $t]
    # ── placement ────────────────────────────────────────────────────
    # Pointer-anchored, and blind to anything a tip must never cover.
    #
    # The old rule was "below the widget's bottom edge". For a tall pane
    # that edge IS the seam with the composer, so hovering the transcript
    # at any height parked "conversation transcript" on the composer's
    # first line, topmost, for four seconds (USER 2026-09-06). Now: four
    # candidates around the pointer (below-right, above-right, below-left,
    # above-left), each rejected if off-screen or intersecting a keep-out
    # zone. No free candidate means NO floating tip — the status bar below
    # still carries the text, so the information never depends on
    # placement, while a tip over what you are typing always costs you
    # the sentence.
    set rects [::va11y::_keepout_rects]
    set sw [winfo screenwidth $w]
    set sh [winfo screenheight $w]
    if {$px eq "" || $py eq ""} {
        # Keyboard focus-in: no pointer, so sit beside the widget and let
        # the same reject-if-covered rule move or drop the tip.
        set ax [expr {[winfo rootx $w] + [winfo width $w]}]
        set ay [expr {[winfo rooty $w] + 2}]
        set cands [list $ax $ay [expr {[winfo rootx $w] - $tw - 6}] $ay]
    } else {
        set ax $px
        set ay $py
        set cands [list [expr {$px + 16}] [expr {$py + 20}] \
                       [expr {$px + 16}] [expr {$py - $th - 12}] \
                       [expr {$px - $tw - 16}] [expr {$py + 20}] \
                       [expr {$px - $tw - 16}] [expr {$py - $th - 12}]]
    }
    # Last resort for a control that lives INSIDE a forbidden band (the
    # bar's own Send/Stop/attach/reasoning): clear the band it is in, above
    # it first. Without this the candidates all start inside the band and
    # every one is rejected - which is correct for the transcript and wrong
    # for a button whose hint you actually asked for by hovering it.
    #
    # The band is the one nearest the pointer, NOT the extreme across all
    # zones: split IRC views register one input per pane (and the chat band
    # too), so a global min would fling a statusbar hint to the top of the
    # tallest pane on screen.
    if {[llength $rects]} {
        lassign [::va11y::_nearest_zone $ax $ay $rects] _ntop _nbot
        if {$_ntop ne ""} {
            lappend cands [expr {$ax + 16}] [expr {$_ntop - $th - 8}] \
                          [expr {$ax + 16}] [expr {$_nbot + 8}]
        }
    }
    set spot {}
    foreach {cx cy} $cands {
        if {[::va11y::_spot_ok $cx $cy $tw $th $sw $sh $rects]} {
            set spot [list $cx $cy]; break
        }
        # Off an edge: the same corner pushed back onto the monitor, but
        # never by driving it into a keep-out zone.
        set nx [expr {$cx < 4 ? 4 : ($cx + $tw > $sw - 4 ? $sw - $tw - 4 : $cx)}]
        set ny [expr {$cy < 4 ? 4 : ($cy + $th > $sh - 4 ? $sh - $th - 4 : $cy)}]
        if {$nx != $cx || $ny != $cy} {
            if {[::va11y::_spot_ok $nx $ny $tw $th $sw $sh $rects]} {
                set spot [list $nx $ny]; break
            }
        }
    }
    if {$spot eq ""} {
        # _hide_tip, not a bare destroy: the tip this one replaced may
        # still have a hide timer armed, and a stale timer that fires
        # later would tear down an unrelated tip that is still wanted.
        ::va11y::_hide_tip
        ::va11y::_bar_borrow $msg
        return
    }
    lassign $spot x y
    wm geometry $t +$x+$y
    raise $t
    # Auto-hide: long enough to read, short enough to not linger over
    # whatever you do next. Re-focus re-shows it.
    catch { after cancel $::va11y::tip_after }
    set ::va11y::tip_after [after 4000 ::va11y::_hide_tip]
    # NO status-bar write here. This used to announce in the bar "too",
    # on the theory that the tip can land behind another window — but
    # the bar is not a free surface: .statusbar.msg is packed -side
    # left ahead of the right-hand group, so a 90-character hint takes
    # the cavity first and the packer UNMAPS .route and .quota. Hovering
    # "plan 55%" therefore erased "plan 55%" (USER 2026-09-18). The bar
    # is the fallback for a tip that could not be placed, and this tip
    # was placed.
}

# The description plus the key that runs this widget's command, when it
# has one: "delete the selected session · Ctrl-D". Discovering a chord
# should not require clicking the thing first.
#
# The key comes from the widget's OWN -command through the binding
# registry, so a widget cannot advertise a chord it does not run.
proc ::va11y::announce {w} {
    set desc [describe $w]
    if {![winfo exists $w]} { return $desc }
    set cmd ""
    if {[catch { $w cget -command } cmd]} { return $desc }
    # ::vcoach::attach rewrites -command to its own wrapper; the real
    # one is the third word.
    if {[string match "::vcoach::_fire *" $cmd]} { set cmd [lindex $cmd 2] }
    if {[string trim $cmd] eq ""} { return $desc }
    set k ""
    catch { set k [::vkeymap::key_for $cmd] }
    if {$k eq ""} { return $desc }
    if {$desc eq ""} { return $k }
    return "$desc · $k"
}
proc ::va11y::_hide_tip {} {
    # Give the status bar back first: a FALLBACK tip borrowed it (see
    # _bar_borrow) and the bar is not the tip's to keep. A no-op when
    # nothing was borrowed, which is the normal case now that a placed
    # tip never writes there.
    ::va11y::_bar_return
    catch { after cancel $::va11y::tip_after }
    # Cleared, not left holding a dead id: callers reason about "is a hide
    # pending?" by reading this variable, and a stale non-empty value makes
    # every such test lie. (dev.tcl's _tip_cancel_hide does the same.)
    set ::va11y::tip_after ""
    catch { destroy .va11y_tip }
}

# ── explicit tab order for dialogs ───────────────────────────────────
# Tk's default Tab order is creation order, which is usually right but
# breaks when widgets are built out of visual order. Call with the list
# of widget paths in the desired cycle; each gets -takefocus 1, and
# Tab/Shift-Tab walk exactly this sequence.
#
# GOTCHA (hit live): the bind scripts MUST be built by string
# templating, not [list focus $next ; break] — the brackets evaluate
# at creation time, so `break` fires inside the setup loop and aborts
# it after the first widget (leaving default traversal in charge).
proc ::va11y::tab_order {widgets} {
    set n [llength $widgets]
    for {set i 0} {$i < $n} {incr i} {
        set w [lindex $widgets $i]
        if {![winfo exists $w]} { continue }
        set next [lindex $widgets [expr {($i + 1) % $n}]]
        set prev [lindex $widgets [expr {($i - 1 + $n) % $n}]]
        catch { $w configure -takefocus 1 }
        bind $w <Tab>          "focus [list $next] ; break"
        bind $w <ISO_Left_Tab> "focus [list $prev] ; break"
        bind $w <Shift-Tab>    "focus [list $prev] ; break"
    }
}
