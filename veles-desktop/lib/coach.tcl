# veles-desktop/lib/coach.tcl — the app teaches you its own keyboard.
#
# The desktop is fully keyboard-operable and bindings.tcl is the single
# registry for it, so help.tcl, whichkey.tcl and the palette hints all
# answer "what does this key do". Nothing answered the other direction:
# you click a button, it works, and the key that would have done it
# stays invisible forever. USER: "remind what keyboard shortcuts they
# could have used to perform any action … so that they may learn and
# remember to use the KB in the future".
#
# HOW IT KNOWS THE KEY
#
# It does not keep a table. The command string an affordance runs is
# already byte-identical to the registry's action column:
#
#     registry   {Ctrl-D all {::vsidebar::delete_selected} …}
#     button     ttk::button … -command ::vsidebar::delete_selected
#     ctxmenu    {"Delete" {::vsidebar::delete_selected}}
#
# so `$w cget -command` IS the lookup key, and ::vbindings::keys_for
# turns it into the chord for the ACTIVE keymap mode. Nothing to
# register, nothing to keep in sync, and a hint cannot outlive the
# binding it describes.
#
# WHEN IT SHUTS UP
#
#   - once you have used that key `threshold` times (it is learned);
#   - when the action was triggered by the keyboard, not the pointer —
#     a coach that congratulates you for doing it right is noise;
#   - when the action has no key in this mode (nothing to teach);
#   - when [ui] coach = false in the config dir's config.toml.
#
# The tally is per COMMAND, not per widget: reaching "delete session"
# from the toolbar, the context menu or the palette all teach Ctrl-D,
# and using Ctrl-D once counts however you found it.

namespace eval ::vcoach {
    variable enabled 1
    # Uses of the KEY before a command is considered learned.
    variable threshold 3
    variable seen {}          ;# command -> times the key was used
    variable toast_after ""
    # Set by a real ButtonRelease on the widget, consumed by _fire.
    # This is what separates "clicked it" from "focused it and pressed
    # Space", which is already keyboard use and needs no coaching.
    variable pointer_fire 0
    variable attached
    array set attached {}
}

# ── configuration + persistence ─────────────────────────────────────

proc ::vcoach::init {} {
    variable enabled
    variable seen
    variable threshold
    # [ui] coach = false — read the same way ::va11y::init reads
    # font_scale, because that is where the desktop's UI knobs live and
    # a second mechanism for one boolean is not worth it.
    set cfg [::vpaths::config_file]
    if {[file exists $cfg]} {
        if {![catch { set fh [open $cfg r] ; set data [read $fh] ; close $fh }]} {
            if {[regexp {(?n)^\s*coach\s*=\s*(\w+)} $data -> v]} {
                set enabled [expr {$v in {true 1 yes on} ? 1 : 0}]
            }
            if {[regexp {(?n)^\s*coach_threshold\s*=\s*(\d+)} $data -> n]} {
                if {$n > 0} { set threshold $n }
            }
        }
    }
    set seen [::vstate::get coach_seen {}]
    if {[catch { dict size $seen }]} { set seen {} }
    return $enabled
}

proc ::vcoach::_persist {} {
    variable seen
    if {![::vstate::put coach_seen $seen]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {coach progress not saved: %s}] $::vstate::error] }
    }
}

# ── what the coach knows ────────────────────────────────────────────

# The chord for a command in the active mode, "" when it has none.
proc ::vcoach::hint {command} {
    if {[catch { ::vkeymap::key_for $command } k]} { return "" }
    return $k
}

proc ::vcoach::times {command} {
    variable seen
    set c [::vbindings::_norm $command]
    if {[dict exists $seen $c]} { return [dict get $seen $c] }
    return 0
}

proc ::vcoach::learned {command} {
    variable threshold
    return [expr {[times $command] >= $threshold}]
}

# The keyboard was used for this command — count it, and stop coaching
# it once the count reaches the threshold. Called from keymap.tcl's row
# runners, so it fires for the real binding and for prefix chords, and
# NOT for a command reached by clicking.
proc ::vcoach::note_key {command} {
    variable seen
    variable enabled
    if {!$enabled} { return }
    set c [::vbindings::_norm $command]
    if {$c eq ""} { return }
    set n [expr {[times $c] + 1}]
    dict set seen $c $n
    # Only write when the number still matters. Past the threshold the
    # tally never changes an answer, so counting to a thousand would be
    # a thousand pointless writes to the state blob.
    if {$n <= [expr {$::vcoach::threshold}]} { _persist }
}

# ── the coaching moment ─────────────────────────────────────────────

# Called after a POINTER-driven action has run. Shows the key once, if
# there is one and it is not learned yet. `label` names the action when
# the widget that ran it cannot (a row of the command tree, gone with
# the tree by the time this runs).
proc ::vcoach::note_mouse {w command {label ""}} {
    variable enabled
    if {!$enabled} { return }
    set k [hint $command]
    if {$k eq ""} { return }
    if {[learned $command]} { return }
    if {$label eq ""} { set label [_label_for $w $command] }
    if {$label eq ""} {
        _toast $w "this has a key — $k"
    } else {
        _toast $w "$label — $k"
    }
}

# A human name for the action, best effort: the a11y name if the widget
# has one, else the button's own text, else nothing.
proc ::vcoach::_label_for {w command} {
    if {[winfo exists $w]} {
        set d ""
        catch { set d [::va11y::describe $w] }
        if {$d ne ""} { return $d }
        set t ""
        catch { set t [$w cget -text] }
        if {$t ne ""} { return [string trim $t] }
    }
    return ""
}

# ── the toast ───────────────────────────────────────────────────────
#
# NOT ::vstatus::set_msg. That line is transient — ::vstatus::poll
# overwrites it every 15 seconds with "gateway up Ns", which is exactly
# why the route got its own label — and it sits at the bottom of the
# window, nowhere near the thing you just clicked.

proc ::vcoach::_toast {anchor text} {
    variable toast_after
    set t .vcoach_toast
    catch { after cancel $toast_after }
    catch { destroy $t }
    if {![winfo exists .]} { return }
    toplevel $t
    wm overrideredirect $t 1
    catch { wm attributes $t -topmost 1 }
    $t configure -background $::vt::T(hairline)
    ttk::label $t.msg -text $text -padding {10 5} \
        -background $::vt::T(bg_panel) -foreground $::vt::T(fg)
    pack $t.msg -padx 1 -pady 1
    # Measured, not estimated — near_measured asks Tk for the real
    # requested size after packing and clamps against that.
    ::vplace::near_measured $t $anchor
    set toast_after [after 3000 { catch { destroy .vcoach_toast } }]
    return $t
}

proc ::vcoach::toast_visible {} {
    return [winfo exists .vcoach_toast]
}

# What the toast currently says, "" when there is none. An accessor
# rather than letting callers reach into .vcoach_toast.msg, because
# `cget` on a window that does not exist THROWS — and a test that
# throws aborts its whole suite: no ✗, no tally, and the flake reads a
# missing tally as a broken run rather than a red one.
proc ::vcoach::toast_text {} {
    if {![winfo exists .vcoach_toast.msg]} { return "" }
    set t ""
    catch { set t [.vcoach_toast.msg cget -text] }
    return $t
}

# ── attaching to affordances ────────────────────────────────────────

# Wrap a button so clicking it coaches. Reads the widget's OWN
# -command, so there is no second place to state what the button does.
# Idempotent: re-attaching a rebuilt widget is normal (regions are
# rebuilt on undock) and must not double-wrap the command.
proc ::vcoach::attach {w} {
    variable attached
    if {![winfo exists $w]} { return }
    set cmd ""
    if {[catch { $w cget -command } cmd]} { return }
    set cmd [string trim $cmd]
    if {$cmd eq ""} { return }
    if {[string match "::vcoach::_fire *" $cmd]} { return }
    # Scrollbars (and any -command invoked WITH ARGUMENTS by the
    # toolkit) are not coachable: ttk invokes a scrollbar's -command as
    # `moveto <frac>` / `scroll N units`, so wrapping it in a two-arg
    # _fire meant every thumb-drag died in bgerror ("wrong # args").
    # _fire is variadic now as belt-and-braces for other arg-passing
    # -commands, but a scrollbar drag is not a click to coach anyway.
    set cls [winfo class $w]
    if {$cls eq "TScrollbar" || $cls eq "Scrollbar"} { return }
    set attached($w) $cmd
    $w configure -command [list ::vcoach::_fire $w $cmd]
    # Widget-tag bindings run before class bindings, so this flag is
    # already set by the time ttk's class binding invokes -command,
    # which runs synchronously inside the same event dispatch.
    #
    # The `after idle` clears it again once the dispatch is over. Not
    # paranoia: ttk only invokes when the release lands INSIDE the
    # widget, so pressing a button and dragging off it sets the flag
    # and never consumes it — and the next Space-activation of that
    # button would then be coached as if it had been clicked.
    bind $w <ButtonRelease-1> {+
        set ::vcoach::pointer_fire 1
        after idle { set ::vcoach::pointer_fire 0 }
    }
}

# Run the wrapped command, then coach if the pointer is what ran it.
# VARIADIC: some -commands are invoked BY THE TOOLKIT with arguments
# (a scrollbar's `moveto <frac>`, a scale's `set <v>`); those are
# skipped at attach time now, but anything else that passes args must
# still round-trip them rather than die in bgerror.
proc ::vcoach::_fire {w cmd args} {
    variable pointer_fire
    set by_pointer $pointer_fire
    set pointer_fire 0
    if {[llength $args]} {
        set code [catch { uplevel #0 [linsert $args 0 {*}$cmd] } result opts]
    } else {
        set code [catch { uplevel #0 $cmd } result opts]
    }
    if {$by_pointer} { note_mouse $w $cmd }
    if {$code} { return -options $opts $result }
    return $result
}

# A menu item, or anything else that already knows both the widget it
# belongs to and the command it runs.
#
# The command PALETTE deliberately does not call this. It prints the
# chord in the row you are looking at, which teaches the same thing
# better and at the moment you are choosing — a toast repeating the key
# you just read would be noise.
proc ::vcoach::fired_by_pointer {w command} {
    note_mouse $w $command
}

# Coach every affordance under a widget tree.
#
# USER: "I don't see any hints for keyboard shortcuts when I click."
#
# The coach worked; it was attached in five places (the sidebar buttons,
# the findbar, the tab strip, the pane tabs, the instance controls) and
# nowhere else. Clicking any of the 23 buttons in the Dev pane, the 13
# in the failover blueprint, the 11 in the overlays, the 10 in the ACL
# manager, or Send/Stop/Attach in the composer taught nothing, because
# nothing had wrapped them.
#
# Walking the tree instead of naming widgets one at a time is what makes
# that stay fixed: a button added later is coached without anyone
# remembering to. `attach` is idempotent and ignores widgets with no
# -command, so re-running after a rebuild — which regions do on undock —
# is free.
proc ::vcoach::attach_all {parent} {
    if {![winfo exists $parent]} { return 0 }
    set n 0
    catch { if {[attach $parent] ne ""} {} }
    # `attach` returns early on anything without a -command, so this is
    # a cheap test for "did that widget become coached".
    variable attached
    if {[info exists attached($parent)]} { incr n }
    foreach child [winfo children $parent] {
        incr n [attach_all $child]
    }
    return $n
}
