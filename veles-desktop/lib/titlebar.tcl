# veles-desktop/lib/titlebar.tcl — in-app window controls.
#
# USER: "it would be very nice to have the display manager controls on
# everything since the native title bar looks really ugly and I would
# like to turn it off".
#
# `wm overrideredirect 1` removes the decoration — and with it every
# affordance the window manager was providing: no drag to move, no
# minimise, no maximise, no close. So a window cannot lose its title bar
# until it carries its own. This provides that strip, and only then is
# it safe to turn the native one off.
#
#   ::vtitle::attach $toplevel "Title" ?-close CMD? ?-min 0|1?
#
# What the strip does:
#   drag            move the window
#   double-click    toggle maximised (remembers the pre-maximise geometry)
#   ─               minimise (iconify; withdraw under overrideredirect,
#                   since an undecorated window has no taskbar entry to
#                   come back from — see below)
#   ▢               maximise / restore
#   ✕               close
#
# ── the honest caveat ───────────────────────────────────────────────
#
# An overrideredirect window is invisible to the window manager. On KDE
# Plasma that means no alt-tab entry, no window-list entry, no snapping,
# and iconify() does not work — which is why minimise WITHDRAWS instead,
# and the app must offer its own way back. For the main window that is
# the point of `::vtitle::restore_all`, bound in the palette.
#
# Because of that the native decorations stay ON by default; turning
# them off is a deliberate choice (palette → "Toggle native title bars").
# The preference is durable — it rides the desktop state blob, like the
# theme — so "I would like to turn it off" only has to be said once.
#
# This comment used to cite `[ui] native_titlebar` in config.toml. There
# is no [ui] section in the config at all and never was; the key was
# documented and never implemented.

namespace eval ::vtitle {
    variable maximised     ;# toplevel -> pre-maximise geometry
    variable hidden {}     ;# toplevels withdrawn by our minimise
    array set maximised {}
}

# Tk child paths: the root window is "." and its children are ".foo",
# NOT "..foo". Every path built from a toplevel has to go through this
# or attaching to the main window fails with `bad window path name`.
proc ::vtitle::_child {w name} {
    return [expr {$w eq "." ? ".$name" : "$w.$name"}]
}

proc ::vtitle::attach {w title args} {
    if {![winfo exists $w]} { return "" }
    set close_cmd [_opt $args -close [list destroy $w]]
    set want_min  [_opt $args -min 1]

    # On a phone AndroWish frames the window itself, with the title and a
    # close box: a strip of ours was a second title bar under it
    # (emulator, 2026-09-30). Its close goes where that box and the Back
    # key send theirs.
    if {[llength [info commands ::vandroid::on]] && [::vandroid::on]} {
        wm protocol $w WM_DELETE_WINDOW $close_cmd
        return ""
    }

    set bar [_child $w vtitlebar]
    catch { destroy $bar }
    ttk::frame $bar
    ttk::label $bar.t -text $title -font {TkDefaultFont 10 bold} \
        -foreground $::vt::T(accent)
    pack $bar.t -side left -padx 8

    # Right to left, so the order reads ─ ▢ ✕ left to right.
    ttk::button $bar.close -text "✕" -width 3 -command $close_cmd
    ::va11y::name $bar.close [::vi18n::mc {close this window}]
    pack $bar.close -side right -padx {2 4}

    ttk::button $bar.max -text "▢" -width 3 -command [list ::vtitle::toggle_max $w]
    ::va11y::name $bar.max [::vi18n::mc {maximise or restore this window}]
    pack $bar.max -side right -padx 2

    if {$want_min} {
        ttk::button $bar.min -text "─" -width 3 -command [list ::vtitle::minimise $w]
        ::va11y::name $bar.min [::vi18n::mc {minimise this window}]
        pack $bar.min -side right -padx 2
    }

    # Drag the strip (or its label) to move the window.
    foreach target [list $bar $bar.t] {
        bind $target <ButtonPress-1>   [list ::vtitle::_grab $w %X %Y]
        bind $target <B1-Motion>       [list ::vtitle::_drag $w %X %Y]
        bind $target <Double-Button-1> [list ::vtitle::toggle_max $w]
    }
    # Always first, above whatever the window packs next.
    pack $bar -fill x -side top
    return $bar
}

# Rename a window that already carries a strip.
#
# `attach` took a title and then nothing could ever change it, so the
# main window's strip said "veles" from boot to quit while `wm title`
# — the NATIVE bar, which is hidden by default here — carried the only
# name that moved. The visible label and the invisible one now say the
# same thing.
#
# Tolerates a window with no strip (the native-decorations path, and
# every test that builds a bare toplevel): there is simply nothing to
# write, which is not an error.
proc ::vtitle::set_title {w title} {
    if {![winfo exists $w]} { return 0 }
    set bar [_child $w vtitlebar]
    if {![winfo exists $bar.t]} { return 0 }
    $bar.t configure -text $title
    return 1
}

proc ::vtitle::_opt {args name default} {
    set i [lsearch -exact $args $name]
    if {$i < 0} { return $default }
    return [lindex $args [expr {$i + 1}]]
}

# ── move ────────────────────────────────────────────────────────────

proc ::vtitle::_grab {w X Y} {
    variable drag
    set drag($w) [list [expr {$X - [winfo rootx $w]}] [expr {$Y - [winfo rooty $w]}]]
    return
}

proc ::vtitle::_drag {w X Y} {
    variable drag
    if {![info exists drag($w)]} { return }
    lassign $drag($w) dx dy
    wm geometry $w "+[expr {$X - $dx}]+[expr {$Y - $dy}]"
    return
}

# ── maximise ────────────────────────────────────────────────────────
#
# USER: "the maximize button functions differently than the OS maximize,
# doesn't seem to respect size of my current screen".
#
# The comment here used to claim `winfo screenwidth` was "the usable
# screen, not the virtual desktop". It is the opposite: on X11 with
# Xinerama — i.e. any multi-monitor setup — the X screen IS every
# monitor glued into one rectangle, so maximising spanned all of them
# and ignored panels as well.
#
# Two cases, because they have different amounts of information:
#
#   decorated    hand it to the window manager (`wm attributes -zoomed`).
#                The WM knows which monitor the window is on and where
#                the panels are; nothing computed here can beat that,
#                and it makes our button agree with the OS one.
#
#   undecorated  there is no WM to ask (that is what overrideredirect
#                means), so find the monitor containing the window's
#                centre ourselves.

proc ::vtitle::_decorated {w} {
    if {[catch { wm overrideredirect $w } od]} { return 1 }
    return [expr {!$od}]
}

# Monitor rectangles as {x y width height}. Cached: this costs a process
# spawn and monitors rarely move. ::vtitle::forget_monitors drops it.
proc ::vtitle::monitors {} {
    variable monitor_cache
    if {[info exists monitor_cache]} { return $monitor_cache }
    set out {}
    # xrandr comes from the user's environment, not the flake closure —
    # veles-desktop execs wish without resetting PATH. Absence is normal
    # (Wayland with no XWayland xrandr, a stripped container), so this
    # degrades rather than erroring.
    if {![catch { exec xrandr --listmonitors } raw]} {
        # " 0: +*eDP-1 1920/344x1200/215+0+0  eDP-1"
        foreach line [split $raw \n] {
            if {[regexp {([0-9]+)/[0-9]+x([0-9]+)/[0-9]+\+([0-9]+)\+([0-9]+)} \
                        $line -> mw mh mx my]} {
                lappend out [list $mx $my $mw $mh]
            }
        }
    }
    set monitor_cache $out
    return $out
}

proc ::vtitle::forget_monitors {} {
    variable monitor_cache
    unset -nocomplain monitor_cache
}

# The monitor containing a root-window point, or {} when unknown.
proc ::vtitle::monitor_at {x y} {
    foreach m [monitors] {
        lassign $m mx my mw mh
        if {$x >= $mx && $x < $mx + $mw && $y >= $my && $y < $my + $mh} {
            return $m
        }
    }
    return {}
}

proc ::vtitle::toggle_max {w} {
    variable maximised
    if {[_decorated $w]} {
        # Let the WM do it, and let the WM remember the restore geometry
        # too — tracking it here goes stale the moment the user
        # maximises with the OS control instead of ours.
        set z 0
        catch { set z [wm attributes $w -zoomed] }
        if {[catch { wm attributes $w -zoomed [expr {!$z}] }]} {
            # -zoomed is X11-only; fall through to the manual path.
        } else {
            return
        }
    }
    if {[info exists maximised($w)]} {
        wm geometry $w $maximised($w)
        unset maximised($w)
        return
    }
    set maximised($w) [wm geometry $w]
    # The window's CENTRE decides which monitor it is on — a window
    # straddling a seam belongs to whichever side holds most of it.
    set cx [expr {[winfo rootx $w] + [winfo width $w] / 2}]
    set cy [expr {[winfo rooty $w] + [winfo height $w] / 2}]
    set m [monitor_at $cx $cy]
    if {$m eq ""} {
        # Nothing to go on. The full X screen is the only answer
        # available, and on multi-monitor it is the WRONG one — so say
        # so instead of silently spanning.
        set m [list 0 0 [winfo screenwidth $w] [winfo screenheight $w]]
        if {[llength [monitors]] == 0} {
            catch {
                ::vstatus::set_msg \
                    [::vi18n::mc {maximised to the whole X screen — xrandr is unavailable, so the monitor layout is unknown}]
            }
        }
    }
    lassign $m mx my mw mh
    wm geometry $w "${mw}x${mh}+${mx}+${my}"
    return
}

# ── minimise ────────────────────────────────────────────────────────

proc ::vtitle::minimise {w} {
    variable hidden
    # iconify() is a request to the window manager, and an
    # overrideredirect window has no manager — it silently does nothing.
    # Withdraw instead, and remember it so there is a way back.
    if {[catch { wm overrideredirect $w } od]} { set od 0 }
    if {$od} {
        wm withdraw $w
        if {[lsearch -exact $hidden $w] < 0} { lappend hidden $w }
        ::vstatus::set_msg [::vi18n::mc {minimised — palette → Restore hidden windows}]
    } else {
        wm iconify $w
    }
    return
}

# The way back for windows the WM cannot show in a taskbar.
proc ::vtitle::restore_all {} {
    variable hidden
    set n 0
    foreach w $hidden {
        if {[winfo exists $w]} {
            catch { wm deiconify $w ; raise $w }
            incr n
        }
    }
    set hidden {}
    ::vstatus::set_msg [expr {$n \
        ? [::vi18n::mc {restored %s window(s)} $n] \
        : [::vi18n::mc {nothing was hidden}]}]
    return
}

# ── native decorations ──────────────────────────────────────────────

# Turn the WM's own title bar off (or back on) for every window that
# carries one of our strips.
proc ::vtitle::set_native {on} {
    foreach w [concat [list .] [winfo children .]] {
        if {![winfo exists $w]} { continue }
        if {[winfo toplevel $w] ne $w} { continue }
        # Only windows that have their own controls may lose the native
        # ones — otherwise the window becomes unmovable and unclosable.
        if {![winfo exists [_child $w vtitlebar]]} { continue }
        catch {
            set geo [wm geometry $w]
            wm overrideredirect $w [expr {$on ? 0 : 1}]
            # Re-mapping resets position on some WMs; put it back.
            wm geometry $w $geo
        }
    }
    return
}

proc ::vtitle::toggle_native {} {
    variable native
    if {![info exists native]} { set native 1 }
    set native [expr {!$native}]
    set_native $native
    if {![::vstate::put native_titlebar $native]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {titlebar preference not saved: %s}] $::vstate::error] }
    }
    ::vstatus::set_msg [expr {$native \
        ? [::vi18n::mc {native title bars on}] \
        : [::vi18n::mc {native title bars off — drag our strip to move, palette → Restore hidden windows}]}]
    return
}

# Apply the saved preference at startup. Called from main.tcl after the
# windows exist — set_native skips any toplevel without its own strip,
# so calling it too early would silently do nothing.
proc ::vtitle::restore_native {} {
    variable native
    set want 1
    catch { set want [::vstate::get native_titlebar 1] }
    if {$want eq ""} { set want 1 }
    set native [expr {$want ? 1 : 0}]
    # Only act when it differs from the default; re-mapping every window
    # on every start for no reason is a visible flash.
    if {!$native} { set_native 0 }
    return
}
