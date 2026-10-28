# veles-desktop/lib/android.tcl — the same tree on a phone.
#
# USER 2026-09-26: "Android gateway AND android frontend. (maybe
# androidwish/tk on android to keep single codebase)". On Android the
# desktop runs on AndroWish's Tk (the APK's DesktopActivity, started by
# android/desktop.tcl), where Tk draws through SDL and the Tcl commands
# `borg` (Android) and `sdltk` (the SDL screen) exist. Everything here is
# a no-op anywhere else.
#
# What differs on a phone, measured on the emulator (1080x2400 at 420
# dpi, 2026-09-30):
#
#   - Size. AndroWish sets `tk scaling` from the true dpi, so the 8 pt
#     default font was 56 px tall, twice a phone's own text. A desktop
#     pixel counts as an Android dp here (the unit Android sizes phone
#     text in for the distance a phone is read at): 1 pt is 96/72 dp,
#     and ui.font_scale scales on top, as on a laptop. Sizes the desktop
#     gives in pixels (dialogs, paddings, the banner) go through dp.
#   - The window is the screen. The desktop's 1080x720 geometry left
#     two thirds of it black, and the in-app title strip's minimise
#     withdraws the window, with no taskbar to bring it back.
#   - The first gateway is the one beside it: the same app runs one
#     (GatewayService), so a fresh install attaches to it instead of
#     opening unattached (see seed_registry).
#   - Width. 411 dp is not a laptop's 1080 px: in portrait the regions
#     stack instead of sitting side by side, the session list is a
#     drawer over the chat (USER 2026-09-30: "Drawer"), and rows of
#     buttons wrap (arrange, flow).
#   - Touch. A finger drags read-only text instead of selecting it, a
#     link opens on a tap and not at the start of a drag, and Back closes
#     what is on top instead of quitting (touch_bindings, back).
#   - Windows. AndroWish frames every toplevel itself, so a dialog has
#     that frame's title and close box, not a second strip of ours, and
#     it fits in what the keyboard leaves of the screen (fit).
#   - Life. Android freezes the app in the background; its streams are
#     reopened when it comes back (foreground).

namespace eval ::vandroid {
    # Android's density (px per dp), once scale has read it; 1 elsewhere.
    variable density 1.0
    # The screen's dots per inch (the lesser axis), which sizes the frame
    # AndroWish draws round each window (decor).
    variable dpi 160
    # 1 while the session list covers the chat (portrait).
    variable drawer 0
    # portrait or landscape, as .mid was last arranged for.
    variable shape ""
    # Dialogs placed by fit: window -> {width height frac}. They are
    # refitted when the window changes size, which is what the keyboard
    # does to it.
    variable fitted [dict create]
    # A fitted dialog's labels: label -> its own -wraplength, in desktop
    # pixels (_wraps).
    variable wraps
    array set wraps {}
    # Frames laid out by flow: frame -> {kids end rows}.
    variable flows [dict create]
    # The press a tagged range's click waits on: {window x y ms}.
    variable tap {}
    # When the app went to the background, in ms; 0 while it is not.
    variable away_since 0
    # The sessions whose event streams `background` closed, for
    # `foreground` to reopen.
    variable quiet_streams {}
    # The app's id (android/AndroidManifest.xml), for its own activities.
    variable app chat.janitor.veles
    # What .brand was packed before, while it is hidden (banner).
    variable banner_before ""
    # A long press selecting text (hold): {w m x y mode a b fx fy}, or
    # {} when none is.
    variable held {}
    # Its scroll at the window's edge: {after-id direction}.
    variable held_scroll {}
    # Whether the finger on read-only text has dragged it since it went
    # down, and the tap's clearing of the selection, pending.
    variable dragged 0
    variable tap_clear ""
    # The menu on the screen, if one is (a context menu, a menubutton's).
    variable menu_up ""
}

# True on AndroWish.
proc ::vandroid::on {} {
    expr {[llength [info commands ::borg]] && [llength [info commands ::sdltk]]}
}

# Before any widget exists.
proc ::vandroid::scale {} {
    variable density
    variable dpi
    if {![on]} { return 0 }
    if {[catch { borg displaymetrics } m] || ![dict exists $m density]} { return 0 }
    set density [dict get $m density]
    if {[dict exists $m xdpi] && [dict exists $m ydpi]} {
        set dpi [expr {min([dict get $m xdpi], [dict get $m ydpi])}]
    }
    tk scaling [expr {$density * 96.0 / 72.0}]
    return 1
}

# A size in desktop pixels, in this screen's pixels: a desktop pixel is
# a dp on a phone, a pixel everywhere else. For the sizes the desktop
# gives in pixels, like its dialogs' (::vplace::centered).
proc ::vandroid::dp {n} {
    variable density
    return [expr {int(round($n * $density))}]
}

# The main window fills the screen. 1 on Android (the caller then sets
# no geometry of its own).
proc ::vandroid::window {} {
    if {![on]} { return 0 }
    wm attributes . -fullscreen 1
    return 1
}

# A fresh install's registry: the gateway in this app, attached. Only
# when there is no registry yet, so a phone set up to use another
# gateway keeps it. No token in the row: it is the same origin as the
# default gateway, so it inherits the token android/desktop.tcl read
# (::vapi::bind_instance_inheriting).
proc ::vandroid::seed_registry {} {
    if {![on]} { return 0 }
    set p [::vinst::path]
    if {$p eq "" || [file exists $p]} { return 0 }
    ::vinst::add phone [::vapi::base]
    ::vinst::set_last_attached phone
    return 1
}

# Once the window is built. The composer gets its own row above the
# buttons, as wide as the chat, and the rows of buttons wrap.
proc ::vandroid::layout {} {
    if {![on]} { return 0 }
    set c .mid.chatwrap.chat
    if {[winfo exists $c.input] && [winfo exists $c.bar]} {
        # -expand 0: packed beside the buttons it expanded to fill their
        # row; on a row of its own it split the column's height with the
        # transcript, and a gap opened under it (emulator).
        pack $c.input -in $c -after $c.bar -side bottom -fill x -expand 0 \
            -padx 4 -pady {4 0}
        # Send, Stop and the rest stay together on the last row, under
        # the thumb; the two switches go above them when it is narrow.
        flow $c.bar -from end
    }
    if {[info exists ::vsidebar::R] && [winfo exists $::vsidebar::R.list]} {
        # A listbox row is its font's height: 13 dp at the desktop's size,
        # a hard target for a finger. (Its selection border would add
        # height too, drawn as a bevel; its focus box, as dashes.)
        $::vsidebar::R.list configure -font {TkDefaultFont 13} \
            -selectborderwidth 0 -activestyle none
        flow $::vsidebar::R.btns
    }
    arrange
    return 1
}

# Once, after layout: what a finger, the Back key and Android's app life
# cycle need.
proc ::vandroid::install {} {
    if {![on]} { return 0 }
    # Fast wipes scroll (Button-2) and a held finger is Button-3, as
    # AndroWish does by default; two and three fingers no longer zoom
    # and pan the whole window, which nothing here could undo.
    catch { sdltk touchtranslate 9 }
    touch_bindings
    # Dialogs the desktop sizes itself, in pixels (`wm geometry $w
    # 860x520`, some thirty of them), or not at all (placed at the top
    # left at their own size): fitted like the ones ::vplace places.
    if {![llength [info commands ::_vandroid_wm]]} {
        rename ::wm ::_vandroid_wm
        proc ::wm {args} { ::vandroid::_wm {*}$args }
    }
    # On `all`, not Toplevel: Tk's own dialogs have a class of their own
    # (tk_messageBox is a Dialog), and Toplevel's binding never saw them:
    # a confirmation ran off the side of the screen.
    bind all <Map> {+ ::vandroid::_mapped %W }
    bind TLabel <Map> {+ ::vandroid::_label_mapped %W }
    bind Label <Map> {+ ::vandroid::_label_mapped %W }
    # The coach names the key for what was just tapped; a phone has none
    # to learn, and it named ⋮ for ⋮.
    catch { set ::vcoach::enabled 0 }
    # What the app's other activities send the desktop (attachments).
    catch { borg onintent ::vandroid::intent }
    bind all <Key-Break> { ::vandroid::back ; break }
    # A posted menu has the focus, and the Menu class takes every key
    # and breaks: Back never reached `all`, and the menu stayed (or the
    # app left under it). It is the menu's Back first.
    bind Menu <Key-Break> { ::vandroid::back ; break }
    bind Menu <Map> {+ set ::vandroid::menu_up %W }
    bind Menu <Unmap> {+ if {$::vandroid::menu_up eq "%W"} { set ::vandroid::menu_up "" } }
    bind . <<WillEnterBackground>> { ::vandroid::background }
    bind . <<DidEnterForeground>> { ::vandroid::foreground }
    bind . <Configure> {+ ::vandroid::_root_configure %W }
    # Picking a session is the end of a trip to the list.
    foreach cmd {::vsidebar::on_select ::vsidebar::new_session} {
        if {[llength [info commands $cmd]]} {
            trace add execution $cmd leave ::vandroid::_picked
        }
    }
    return 1
}

# ── the screen's shape ────────────────────────────────────────────────

# The display's shape, not the window's: the keyboard takes its height
# from the window (DesktopActivity pads the content above it), and a
# 1080x1920 screen with the keyboard up left a 1080x1017 window, wider
# than tall, so the session list docked beside the chat while you typed
# (emulator, 2026-10-01). Android's display metrics are the app window
# in the current rotation, keyboard or not.
proc ::vandroid::portrait {} {
    if {![catch { borg displaymetrics } m]
            && [dict exists $m width] && [dict exists $m height]} {
        return [expr {[dict get $m width] < [dict get $m height]}]
    }
    expr {[winfo width .] < [winfo height .]}
}

# .mid for the screen's shape: side by side in landscape; in portrait
# stacked, with the session list a drawer over the chat. Run after
# every tab switch (tabs dock their regions afresh) and when the screen
# turns. The sidebar stays one of .mid's panes, hidden or not, so the
# tab's saved layout still has it.
proc ::vandroid::arrange {} {
    variable shape
    variable drawer
    if {![on] || ![winfo exists .mid]} { return 0 }
    set shape [expr {[portrait] ? "portrait" : "landscape"}]
    set tall [expr {$shape eq "portrait"}]
    .mid configure -orient [expr {$tall ? "vertical" : "horizontal"}] -sashwidth [dp 10]
    if {!$tall} { set drawer 0 }
    set panes [.mid panes]
    set listed [expr {".sidebar" in $panes}]
    foreach p $panes {
        if {!$tall} {
            set hide 0
        } elseif {$p eq ".sidebar"} {
            set hide [expr {!$drawer}]
        } else {
            set hide [expr {$drawer && $listed}]
        }
        catch { .mid paneconfigure $p -hide $hide }
    }
    # The chat is what takes the height left over, and it is hidden
    # while the list is open: the list takes it then, or it sits at its
    # own height over an empty screen.
    if {$listed} {
        catch { .mid paneconfigure .sidebar -stretch [expr {$drawer ? "always" : "never"}] }
        # Side by side, the list's desktop 140 px were 53 dp: titles cut
        # to a word. A width someone drags it to above this is kept.
        if {!$tall} {
            catch { .mid paneconfigure .sidebar -minsize [dp 200] }
        }
    }
    banner
    return 1
}

# ☰: the session list over the chat, or the chat again. From another
# tab it goes to the tab the list is in first.
proc ::vandroid::drawer_toggle {} {
    variable drawer
    if {![on] || ![winfo exists .mid]} { return 0 }
    if {".sidebar" ni [.mid panes]} {
        foreach id [::vtabs::ids] {
            if {".sidebar" in [::vtabs::regions $id]} {
                set drawer 1
                ::vtabs::select $id
                break
            }
        }
    } else {
        set drawer [expr {!$drawer}]
    }
    arrange
    if {$drawer && [info exists ::vsidebar::R]} {
        catch { focus $::vsidebar::R.list }
    }
    return $drawer
}

proc ::vandroid::drawer_close {} {
    variable drawer
    if {!$drawer} { return 0 }
    set drawer 0
    arrange
    return 1
}

proc ::vandroid::_picked {args} {
    drawer_close
}

# The window changed size: turned, or the keyboard came or went.
proc ::vandroid::_root_configure {W} {
    if {$W ne "."} { return }
    after cancel ::vandroid::_settle
    after 50 ::vandroid::_settle
}

proc ::vandroid::_settle {} {
    variable shape
    variable fitted
    if {[portrait] != ($shape eq "portrait")} { arrange }
    foreach w [dict keys $fitted] {
        if {[winfo exists $w]} { catch { _fit $w } }
    }
    banner
}

# The banner only while there is height to spare: in landscape, or with
# the keyboard up, the transcript needs it more.
proc ::vandroid::banner {} {
    variable banner_before
    if {![on] || ![winfo exists .brand]} { return 0 }
    set tall [expr {[winfo height .] >= [dp 600]}]
    set shown [winfo ismapped .brand]
    if {!$tall && [winfo manager .brand] eq "pack"} {
        set slaves [pack slaves .]
        set banner_before [lindex $slaves [expr {[lsearch -exact $slaves .brand] + 1}]]
        pack forget .brand
        return 0
    }
    if {$tall && [winfo manager .brand] eq "" && [winfo exists $banner_before]} {
        pack .brand -side top -fill x -before $banner_before
    }
    return $tall
}

# ── rows that wrap ────────────────────────────────────────────────────

# A row of buttons that wraps at the frame's width instead of running
# off the screen: its children go into rows, in the order they showed
# (left-packed first, then right-packed from the left). Separators go.
# -from end fills the last row first, so a bar's final buttons (Send)
# stay together at the bottom right, under the thumb.
proc ::vandroid::flow {f args} {
    variable flows
    if {![on] || ![winfo exists $f]} { return 0 }
    set end 0
    set i [lsearch -exact $args -from]
    if {$i >= 0 && [lindex $args [expr {$i + 1}]] eq "end"} { set end 1 }
    # Already in rows: the frame's own children are the rows now.
    if {[dict exists $flows $f] && [lsearch -glob [pack slaves $f] "$f.vrow*"] >= 0} {
        return [_reflow $f]
    }
    set lefts {}
    set rights {}
    foreach k [pack slaves $f] {
        if {[winfo class $k] in {TSeparator Separator}} {
            pack forget $k
            continue
        }
        if {[dict get [pack info $k] -side] eq "right"} {
            set rights [linsert $rights 0 $k]
        } else {
            lappend lefts $k
        }
    }
    set kids [concat $lefts $rights]
    if {![llength $kids]} { return 0 }
    foreach k $kids {
        pack forget $k
        if {"VFlowKid" ni [bindtags $k]} { bindtags $k [linsert [bindtags $k] 0 VFlowKid] }
    }
    dict set flows $f [dict create kids $kids end $end rows {}]
    if {"VFlow" ni [bindtags $f]} { bindtags $f [linsert [bindtags $f] 0 VFlow] }
    bind VFlow <Configure> { ::vandroid::_reflow %W }
    bind VFlow <Destroy> { dict unset ::vandroid::flows %W }
    # A button whose label grows (Send becomes Steer, a language swap)
    # may no longer fit its row.
    bind VFlowKid <Configure> { ::vandroid::_reflow_soon [winfo parent %W] }
    _reflow $f
    return 1
}

proc ::vandroid::_reflow_soon {f} {
    after cancel [list ::vandroid::_reflow $f]
    after idle [list ::vandroid::_reflow $f]
}

proc ::vandroid::_reflow {f} {
    variable flows
    if {![dict exists $flows $f] || ![winfo exists $f]} { return 0 }
    set spec [dict get $flows $f]
    set kids {}
    foreach k [dict get $spec kids] {
        if {[winfo exists $k]} { lappend kids $k }
    }
    set width [winfo width $f]
    if {$width <= 1} { set width [winfo width [winfo toplevel $f]] }
    if {$width <= 1} { set width [winfo screenwidth $f] }
    set pad [dp 2]
    set order [expr {[dict get $spec end] ? [lreverse $kids] : $kids}]
    set rows {}
    set row {}
    set used 0
    foreach k $order {
        set w [expr {[winfo reqwidth $k] + 2 * $pad}]
        if {[llength $row] && $used + $w > $width} {
            lappend rows $row
            set row {}
            set used 0
        }
        lappend row $k
        incr used $w
    }
    if {[llength $row]} { lappend rows $row }
    if {[dict get $spec end]} {
        set back {}
        foreach r [lreverse $rows] { lappend back [lreverse $r] }
        set rows $back
    }
    if {$rows eq [dict get $spec rows]} { return 0 }
    dict set flows $f rows $rows
    foreach c [winfo children $f] {
        if {[string match "$f.vrow*" $c]} { destroy $c }
    }
    set n 0
    foreach r $rows {
        set rf $f.vrow$n
        ttk::frame $rf
        pack $rf -side top -anchor [expr {[dict get $spec end] ? "e" : "w"}]
        foreach k $r {
            pack $k -in $rf -side left -padx $pad -pady $pad
            # A later sibling is drawn over an earlier one: the row
            # would hide the buttons it holds.
            raise $k $rf
        }
        incr n
    }
    return [llength $rows]
}

# ── dialogs ───────────────────────────────────────────────────────────

# The frame AndroWish draws round every toplevel but the main window,
# {side title} in pixels, by the screen's dpi (sdl2tk's SdlTkX.c,
# AndroWish 8e9d986cd1). An override-redirect window has none.
proc ::vandroid::decor {{w ""}} {
    variable dpi
    if {$w ne "" && [winfo exists $w] && [_realwm overrideredirect $w]} { return {0 0} }
    if {$dpi < 140} { return {6 20} }
    if {$dpi < 190} { return {9 30} }
    if {$dpi < 240} { return {12 38} }
    if {$dpi < 320} { return {15 46} }
    if {$dpi < 420} { return {21 60} }
    return {27 78}
}

# A dialog the desktop sizes in pixels, on a phone: dp, within what the
# main window has left (the keyboard takes the bottom of it), frame
# included. It is fitted again whenever that changes.
proc ::vandroid::fit {w width height {frac 0.15}} {
    variable fitted
    if {![on] || ![winfo exists $w]} { return {} }
    dict set fitted $w [list $width $height $frac]
    if {"VFitted" ni [bindtags $w]} { bindtags $w [linsert [bindtags $w] 0 VFitted] }
    bind VFitted <Destroy> {
        dict unset ::vandroid::fitted %W
        array unset ::vandroid::wraps %W.*
    }
    return [_fit $w]
}

# `wm`, as the desktop calls it on a phone: a dialog given a size is
# fitted instead (the size is desktop pixels, and a 980 px window is
# wider than the phone). The main window and frameless popups, which sit
# where they are put, are left alone.
proc ::vandroid::_wm {args} {
    if {[lindex $args 0] eq "geometry" && [llength $args] == 3} {
        lassign $args - w spec
        if {$w ne "." && [winfo exists $w] && [winfo toplevel $w] eq $w
            && ![::_vandroid_wm overrideredirect $w]
            && [regexp {^(\d+)x(\d+)} $spec -> width height]} {
            fit $w $width $height
            return ""
        }
    }
    return [::_vandroid_wm {*}$args]
}

# The real `wm`, whether or not install has wrapped it.
proc ::vandroid::_realwm {args} {
    if {[llength [info commands ::_vandroid_wm]]} {
        return [::_vandroid_wm {*}$args]
    }
    return [::wm {*}$args]
}

# A dialog on the screen that nothing placed: fitted at the size it
# took (in desktop pixels, which fit makes dp again).
proc ::vandroid::_mapped {w} {
    variable fitted
    variable density
    if {$w eq "." || ![winfo exists $w] || [winfo toplevel $w] ne $w} { return 0 }
    if {[_realwm overrideredirect $w]} { return 0 }
    if {[dict exists $fitted $w]} {
        # Placed before it showed: its labels were wrapped before what is
        # beside them (a scrollbar) had a size.
        after idle [list catch [list ::vandroid::_wraps $w [winfo width $w]]]
        return 0
    }
    set k [expr {double($density)}]
    fit $w [expr {int([winfo reqwidth $w] / $k)}] [expr {int([winfo reqheight $w] / $k)}]
    # AndroWish places a window it has just mapped after this binding
    # runs, keeping the size and dropping the position: the approval
    # dialog sat in the top left corner (emulator). Once more after it.
    after idle [list catch [list ::vandroid::_fit $w]]
    return 1
}

proc ::vandroid::_fit {w} {
    variable fitted
    lassign [dict get $fitted $w] width height frac
    lassign [decor $w] side top
    set sw [winfo width .]
    set sh [winfo height .]
    if {$sw <= 1 || $sh <= 1} {
        set sw [winfo screenwidth .]
        set sh [winfo screenheight .]
    }
    set width [expr {max(1, min([dp $width], $sw - 2 * $side))}]
    set height [expr {max(1, min([dp $height], $sh - $top - $side))}]
    # The frame's corner is what `wm geometry` places.
    set x [expr {max(0, ($sw - $width - 2 * $side) / 2)}]
    set y [expr {max(0, int(($sh - $top - $side - $height) * $frac))}]
    # A minimum size in desktop pixels must not push it off the screen.
    lassign [_realwm minsize $w] mw mh
    _realwm minsize $w [expr {min($mw, $width)}] [expr {min($mh, $height)}]
    _realwm geometry $w ${width}x${height}+${x}+${y}
    _wraps $w $width
    return [list $x $y]
}

# A label's -wraplength is in desktop pixels, like the dialog's size: dp
# here, and never wider than the dialog was fitted to (an approval's
# heading ran off a 320 px screen, and a 900 px one would off a phone).
proc ::vandroid::_wraps {w width} {
    foreach k [winfo children $w] {
        if {[winfo toplevel $k] ne [winfo toplevel $w]} { continue }
        if {[winfo class $k] in {TLabel Label}} { _wrap $k $width }
        _wraps $k $width
    }
}

# One label: from its own length, remembered the first time, so a refit
# does not shrink it again. A label that never wrapped (a hint, a path)
# wraps at the dialog's width: on a phone one line is all a dialog has.
# The room is what lies between what is either side of it: tk_messageBox's
# message sits beside its icon, and the wizard's step beside a
# scrollbar, and wrapped at the dialog's width their lines ran off the
# edge or under the bar (emulator, 2026-10-01).
proc ::vandroid::_wrap {k width} {
    variable wraps
    if {![info exists wraps($k)]} {
        # A ttk::label's default is "", a Tk label's 0.
        set wl [$k cget -wraplength]
        set wraps($k) [expr {$wl eq "" ? 0 : [winfo pixels $k $wl]}]
    }
    lassign [_insets $k] left right
    set room [expr {max([dp 80], $width - max([dp 20], $left) - max([dp 20], $right))}]
    if {$wraps($k) > 0} {
        $k configure -wraplength [expr {min([dp $wraps($k)], $room)}]
    } else {
        $k configure -wraplength $room
    }
}

# What is left and right of a label in its dialog, in pixels: an
# earlier grid column, a widget packed before it, a scrollbar gridded
# after it, and the padding round each. Never the space it leaves
# itself: a centred label moves as it narrows, and the room beside a
# narrower one grows, so each refit would narrow it again. The right is
# what is gridded there asks for, which needs no layout; the left is
# where its cell starts, 0 until it is laid out.
proc ::vandroid::_insets {k} {
    set top [winfo toplevel $k]
    set left 0
    set right 0
    for {set w $k} {$w ne $top && [winfo exists $w]} {set w $m} {
        switch -- [winfo manager $w] {
            grid {
                set g [grid info $w]
                set m [dict get $g -in]
                set c [dict get $g -column]
                set r [dict get $g -row]
                set last [expr {$c + [dict get $g -columnspan] - 1}]
                set pad [dict get $g -padx]
                incr left [expr {[lindex [grid bbox $m $c $r] 0] + [lindex $pad 0]}]
                incr right [lindex $pad end]
                foreach s [grid slaves $m -row $r] {
                    set sg [grid info $s]
                    if {[dict get $sg -column] <= $last} { continue }
                    set sp [dict get $sg -padx]
                    incr right [expr {[winfo reqwidth $s] + [lindex $sp 0] + [lindex $sp end]}]
                }
            }
            pack {
                set p [pack info $w]
                set m [dict get $p -in]
                if {[dict get $p -side] in {left right}} {
                    incr left [expr {[winfo rootx $w] - [winfo rootx $m]}]
                } else {
                    set pad [dict get $p -padx]
                    incr left [lindex $pad 0]
                    incr right [lindex $pad end]
                }
            }
            default {
                set m [winfo parent $w]
                incr left [winfo x $w]
            }
        }
    }
    return [list $left $right]
}

# A label a fitted dialog shows later (the setup wizard builds each step
# when it is reached), as it maps.
proc ::vandroid::_label_mapped {k} {
    variable fitted
    set top [winfo toplevel $k]
    if {![dict exists $fitted $top]} { return }
    set width [winfo width $top]
    if {$width <= 1} { return }
    _wrap $k $width
    # Where its cell starts may be settled only after it maps (a step
    # built into a dialog already on the screen).
    after idle [list catch [list ::vandroid::_wrap $k $width]]
}

# ── size ──────────────────────────────────────────────────────────────

# The theme's pixel sizes in dp, for a finger: run at the end of
# ::vt::apply_theme, so a retheme keeps them.
proc ::vandroid::touch_theme {} {
    if {![on]} { return 0 }
    ttk::style configure TButton -padding [list [dp 10] [dp 7]]
    foreach s {DevTab.TButton DevTabOn.TButton} {
        ttk::style configure $s -padding [list [dp 6] [dp 5]]
    }
    # Their text has no -wraplength of its own, but takes the style's:
    # the screen's narrow side less a dialog's margins and the indicator
    # (the wizard's provider choices ran off a 320 px screen).
    set across [expr {min([winfo screenwidth .], [winfo screenheight .]) - [dp 100]}]
    foreach s {TCheckbutton TRadiobutton} {
        ttk::style configure $s -padding [dp 4] -indicatorsize [dp 14] \
            -indicatormargin [list 0 0 [dp 6] 0] -wraplength $across
    }
    ttk::style configure TEntry -padding [dp 4]
    ttk::style configure TCombobox -padding [dp 4] -arrowsize [dp 16]
    ttk::style configure TSpinbox -padding [dp 4] -arrowsize [dp 14]
    ttk::style configure Treeview -indent [dp 20]
    # The tree's disclosure triangle: 20 px on AndroWish, 8 dp here.
    foreach s {Item Treeview.Item} {
        ttk::style configure $s -indicatorsize [dp 14] \
            -indicatormargins [list [dp 2] [dp 2] [dp 6] [dp 2]]
    }
    ttk::style configure Treeview.Heading -padding [dp 3]
    option add *Menu.activeBorderWidth [dp 5]
    option add *Menu.borderWidth [dp 2]
    # A list row is its font's height: 13 dp at the desktop's size, a
    # hard target for a finger (the palette, the pickers).
    option add *Listbox.font {TkDefaultFont 13}
    option add *Listbox.activeStyle none
    return 1
}

# ── touch ─────────────────────────────────────────────────────────────

proc ::vandroid::touch_bindings {} {
    # Read-only text (the transcript, the panes that show text): a
    # finger drags it, as on any phone, rather than selecting; a fast
    # wipe (Button-2 here) moves it further. Selecting is the long press
    # (hold), and a tap clears what it selected. The Text class never
    # sees the press either, so a tap on the transcript does not raise
    # the keyboard.
    catch { ::vtext::_install_readonly_class }
    bind VReadonly <Button-1> { ::vandroid::_drag_mark %W %y ; break }
    bind VReadonly <B1-Motion> { ::vandroid::_drag %W %y 1 ; break }
    bind VReadonly <ButtonRelease-1> { ::vandroid::_drag_end %W ; break }
    bind VReadonly <Button-2> { ::vandroid::_drag_mark %W %y ; break }
    bind VReadonly <B2-Motion> { ::vandroid::_drag %W %y 3 ; break }
    bind VReadonly <ButtonRelease-2> { break }
    # ttk's tree has no scan: a wipe moves it by rows.
    bind Treeview <Button-2> { ::vandroid::_tree_mark %W %y }
    bind Treeview <B2-Motion> { ::vandroid::_tree_drag %W %y }
    # A frame that scrolls (::vmanage::scrollframe: the wizard's steps,
    # Mechanisms): a wipe moves it wherever it starts, on a field too,
    # but not on a list, which a wipe scrolls itself (the wizard's 46
    # languages); and a drag does where it starts on text or on nothing
    # (a field's drag is its own, a selection).
    bind VScrollKid <Button-2> { if {![::vandroid::_sf_own %W]} { ::vandroid::_sf_mark %W %Y ; break } }
    bind VScrollKid <B2-Motion> { if {![::vandroid::_sf_own %W]} { ::vandroid::_sf_drag %W %Y ; break } }
    bind VScrollKid <ButtonRelease-2> { if {![::vandroid::_sf_own %W]} break }
    bind VScrollKid <Button-1> { if {[::vandroid::_sf_dragged %W]} { ::vandroid::_sf_mark %W %Y } }
    bind VScrollKid <B1-Motion> { if {[::vandroid::_sf_dragged %W]} { ::vandroid::_sf_drag %W %Y ; break } }
    return 1
}
proc ::vandroid::_sf_own {w} {
    expr {[winfo class $w] in {Listbox Text Treeview}}
}
proc ::vandroid::_sf_dragged {w} {
    expr {[winfo class $w] in {TLabel Label TFrame Frame Canvas}}
}
proc ::vandroid::_sf_mark {w y} {
    set c [::vmanage::_sf_canvas $w]
    if {$c ne ""} { $c scan mark 0 $y }
}
proc ::vandroid::_sf_drag {w y} {
    set c [::vmanage::_sf_canvas $w]
    if {$c ne ""} { $c scan dragto 0 $y 1 }
}

proc ::vandroid::_drag_mark {w y} {
    variable dragged
    set dragged 0
    $w scan mark 0 $y
}

proc ::vandroid::_drag {w y gain} {
    variable dragged
    set dragged 1
    $w scan dragto 0 $y $gain
}

# The finger lifts off read-only text. A tap clears its selection, but
# only once the events already queued have run: a long press lifts the
# left button just before it presses the right (Tk sees the touch
# translation's button change as both), and hold, which comes next,
# keeps the selection it may be about to move.
proc ::vandroid::_drag_end {w} {
    variable dragged
    variable tap_clear
    if {$dragged} { return }
    after cancel $tap_clear
    set tap_clear [after idle [list catch [list $w tag remove sel 1.0 end]]]
}

proc ::vandroid::_tree_mark {w y} {
    variable tree_y
    set tree_y $y
}

proc ::vandroid::_tree_drag {w y} {
    variable tree_y
    if {![info exists tree_y]} { set tree_y $y }
    set row [dp 24]
    set n [expr {($tree_y - $y) / $row}]
    if {$n != 0} {
        $w yview scroll $n units
        set tree_y [expr {$tree_y - $n * $row}]
    }
}

# A click on a tagged range, on a phone (::vtext::click): armed by the
# press, run by a release near it soon after. A drag scrolls instead, and
# a held finger has opened the context menu.
proc ::vandroid::_tap_arm {w x y} {
    variable tap
    set tap [list $w $x $y [clock milliseconds]]
}

proc ::vandroid::_tap_fire {w x y script} {
    variable tap
    if {![llength $tap]} { return 0 }
    lassign $tap tw tx ty ms
    set tap {}
    if {$tw ne $w || abs($x - $tx) > [dp 12] || abs($y - $ty) > [dp 12]} { return 0 }
    if {[clock milliseconds] - $ms > 700} { return 0 }
    uplevel #0 $script
    return 1
}

# ── selecting text ────────────────────────────────────────────────────

# USER 2026-10-01: "it would be good if it was possible to select
# arbitrary parts of the transcript and copy, not just message
# granularity". A finger's drag scrolls read-only text, so on a phone
# the long press selects, as on the rest of the phone: the word under
# the finger, and moving the finger before it lifts carries the far end
# along, a word at a time, scrolling at the window's edge. A long press
# on either end of a selection moves that end instead, a letter at a
# time. Lifting the finger opens the text's menu, whose Copy copies the
# selection (::vctx). Held motion is <B3-Motion> by the touch
# translation (android/androwish/touch.patch).
#
# For text with a menu (::vctx::attach). Editable text keeps its menu on
# the long press itself: a drag already selects there.
proc ::vandroid::hold_bind {w m} {
    if {![on]} { return 0 }
    bind $w <Button-3> [list ::vandroid::hold $w $m %x %y %X %Y]
    bind $w <B3-Motion> [list ::vandroid::hold_drag $w %x %y]
    bind $w <ButtonRelease-3> [list ::vandroid::hold_end $w %X %Y]
    return 1
}

proc ::vandroid::hold {w m x y X Y} {
    variable held
    variable tap_clear
    after cancel $tap_clear
    if {![::vtext::is_readonly $w]} {
        set held {}
        ::vctx::_popup $w $m $x $y $X $Y
        return
    }
    set i [$w index @$x,$y]
    # The transcript seldom has the focus on a phone (the keyboard would
    # come with it), and its unfocused selection is a shade a phone's
    # screen barely shows.
    catch { $w configure -inactiveselectbackground [$w cget -selectbackground] }
    set mode ""
    if {[llength [$w tag ranges sel]]} {
        # The end a fingertip is on, if it is near enough to one to mean
        # it; of a short word both are, and the nearer one is meant.
        set best [dp 24]
        set first [$w bbox sel.first]
        set last [$w bbox "sel.last -1c"]
        if {[llength $first]} {
            set d [expr {hypot($x - [lindex $first 0],
                $y - [lindex $first 1] - [lindex $first 3] / 2)}]
            if {$d <= $best} {
                set best $d
                set mode char
                set a [$w index sel.last]
            }
        }
        if {[llength $last]} {
            set d [expr {hypot($x - [lindex $last 0] - [lindex $last 2],
                $y - [lindex $last 1] - [lindex $last 3] / 2)}]
            if {$d < $best} {
                set mode char
                set a [$w index sel.first]
            }
        }
    }
    if {$mode eq ""} {
        set mode word
        $w tag remove sel 1.0 end
        # A fingertip between two words means the one beside it.
        if {[string is space [$w get $i]]} {
            foreach near [list "$i -1c" "$i +1c"] {
                set near [$w index $near]
                if {[lindex [split $near .] 0] == [lindex [split $i .] 0]
                        && ![string is space [$w get $near]]} {
                    set i $near
                    break
                }
            }
        }
        if {[string is space [$w get $i]]} {
            set a $i
            set b $i
        } else {
            set a [$w index "$i wordstart"]
            set b [$w index "$i wordend"]
            $w tag add sel $a $b
        }
    } else {
        set b $a
    }
    set held [list $w $m $x $y $mode $a $b $x $y]
}

# The finger moved, held: the selection's far end goes with it.
proc ::vandroid::hold_drag {w x y} {
    variable held
    if {![llength $held] || [lindex $held 0] ne $w} { return }
    lset held 7 $x
    lset held 8 $y
    _hold_extend
    # At the top or bottom edge, the text scrolls on under the finger.
    set band [dp 40]
    set dir 0
    if {$y < $band} {
        set dir -1
    } elseif {$y > [winfo height $w] - $band} {
        set dir 1
    }
    _hold_scroll $dir
}

proc ::vandroid::_hold_extend {} {
    variable held
    lassign $held w m x y mode a b fx fy
    if {![winfo exists $w]} { return }
    set fy [expr {max(0, min($fy, [winfo height $w] - 1))}]
    set i [$w index @$fx,$fy]
    if {$mode eq "word" && ![string is space [$w get $i]]} {
        set lo [$w index "$i wordstart"]
        set hi [$w index "$i wordend"]
    } else {
        set lo $i
        set hi $i
    }
    if {[$w compare $lo < $a]} {
        set from $lo
        set to $b
    } else {
        set from $a
        set to [expr {[$w compare $hi > $b] ? $hi : $b}]
    }
    $w tag remove sel 1.0 end
    if {[$w compare $from < $to]} { $w tag add sel $from $to }
}

proc ::vandroid::_hold_scroll {dir} {
    variable held
    variable held_scroll
    lassign $held_scroll id was
    if {$dir == $was && $dir != 0} { return }
    after cancel $id
    set held_scroll [list "" 0]
    if {$dir == 0 || ![llength $held]} { return }
    set held_scroll [list [after 60 [list ::vandroid::_hold_scroll_step $dir]] $dir]
}

proc ::vandroid::_hold_scroll_step {dir} {
    variable held
    variable held_scroll
    set held_scroll [list "" 0]
    set w [lindex $held 0]
    if {$w eq "" || ![winfo exists $w]} { return }
    $w yview scroll $dir units
    _hold_extend
    set held_scroll [list [after 60 [list ::vandroid::_hold_scroll_step $dir]] $dir]
}

# The finger lifted: the menu, where it lifted, for what it first held
# (the message under it, for "Copy message").
proc ::vandroid::hold_end {w X Y} {
    variable held
    _hold_scroll 0
    if {![llength $held] || [lindex $held 0] ne $w} { return }
    lassign $held w m x y
    set held {}
    ::vctx::_popup $w $m $x $y $X $Y
}

# ── Back ──────────────────────────────────────────────────────────────

# Android's Back (DesktopActivity hands it to Tk as the Break key):
# the menu, the drawer, the window on top, in that order; and with
# nothing open, the app goes to the background. It never quits: that
# would end the desktop's process, and with it what is on the screen.
proc ::vandroid::back {} {
    variable drawer
    variable menu_up
    foreach m [list [expr {[info exists ::tk::Priv(popup)] ? $::tk::Priv(popup) : ""}] $menu_up] {
        if {$m ne "" && [winfo exists $m] && [winfo ismapped $m]} {
            catch { tk::MenuUnpost $m }
            catch { $m unpost }
            set menu_up ""
            return menu
        }
    }
    if {$drawer} {
        drawer_close
        return drawer
    }
    foreach w [lreverse [wm stackorder .]] {
        if {$w eq "."} { break }
        if {![winfo ismapped $w]} { continue }
        # A popup without a frame (the palette, a completion list) closes
        # the way it does from the keyboard.
        if {[wm overrideredirect $w]} {
            set f [focus]
            if {$f ne "" && [winfo toplevel $f] eq $w} {
                event generate $f <Key-Escape>
                if {![winfo exists $w] || ![winfo ismapped $w]} { return window }
            }
            catch { destroy $w }
            return window
        }
        set p [wm protocol $w WM_DELETE_WINDOW]
        if {$p ne ""} { uplevel #0 $p } else { destroy $w }
        return window
    }
    catch { borg withdraw }
    return withdraw
}

# ── life cycle ────────────────────────────────────────────────────────

# Android freezes an app in the background within seconds; nothing may
# keep the radio up meanwhile: the 1.5 s and 15 s polls, the 30 s layout
# save, the banner's streaks and every session's event stream stop, so
# the gateway's follow ticks stop too, and nothing the desktop does any
# more counts as use: the engine stops five minutes after this.
# What was open reconnects in `foreground`.
#
# The streams are written down BEFORE they close: closing one forgets
# it (`_events_disconnect` unsets its entry), so asking afterwards which
# were open answers "none" — and nothing came back on screen.
proc ::vandroid::background {} {
    variable away_since
    variable quiet_streams
    if {$away_since == 0} { set quiet_streams {} }
    set away_since [clock milliseconds]
    catch {
        foreach sid [::vchat::streaming_sessions] {
            if {$sid ni $quiet_streams} { lappend quiet_streams $sid }
        }
    }
    catch { ::vdock::save_layout }
    catch { ::vdock::layout_save_stop }
    catch { ::vpanes::poll_stop }
    catch { ::vstatus::poll_stop }
    catch { ::vbrand::shoot_stop }
    catch { ::vchat::_events_disconnect_all }
    return 1
}

# Back on the screen: the polls re-arm, and the sessions whose streams
# `background` closed reconnect from their cursors, the session on
# screen among them; the bar and the list catch up.
proc ::vandroid::foreground {} {
    variable away_since
    variable quiet_streams
    if {$away_since == 0} { return 0 }
    set away_since 0
    set sids $quiet_streams
    set quiet_streams {}
    catch {
        if {$::vchat::session ne "" && $::vchat::session ni $sids} {
            lappend sids $::vchat::session
        }
    }
    catch { ::vdock::layout_save_start }
    catch { ::vpanes::poll_start }
    catch { ::vstatus::poll }
    catch { ::vbrand::redraw }
    foreach sid $sids {
        catch { ::vchat::_events_reconnect $sid }
    }
    catch { ::vsidebar::refresh }
    return 1
}

# ── what the desktop says about keys ─────────────────────────────────

# The on-screen control for a command a phone has one for, where a
# laptop would name its key (::vkeymap::key_for): every command is
# under the ⋮ at the top right (lib/cmdtree.tcl), the palette's search
# its first row.
proc ::vandroid::control_for {command} {
    if {![on]} { return "" }
    if {$command in {::vcmdtree::open ::vpalette::open}} { return "⋮" }
    return ""
}

# The tab strip (::vtabs::build) gets ☰ for the session list, first;
# its ⋮ is everyone's. The tabs lose their chords, and the strip wraps.
proc ::vandroid::tab_strip {bar} {
    if {![on] || ![winfo exists $bar]} { return 0 }
    set tabs {}
    foreach k [pack slaves $bar] {
        if {[dict get [pack info $k] -side] ne "right"} { lappend tabs $k }
    }
    foreach t $tabs { catch { $t configure -width 0 } }
    ttk::button $bar.drawer -text "☰" -width 0 -command ::vandroid::drawer_toggle
    catch { ::va11y::name $bar.drawer [::vi18n::mc {show or hide the session list}] }
    if {[llength $tabs]} {
        pack $bar.drawer -side left -before [lindex $tabs 0]
    } else {
        pack $bar.drawer -side left
    }
    flow $bar
    return 1
}

# ── Android's activities ─────────────────────────────────────────────

# 📎 on a phone (::vchat::attach_dialog): Android's own picker, in
# AttachActivity, which copies the picked files where the app reads them
# by path and sends them back here. Tk's file dialog opened at `/`, and a
# phone's files are behind Android's storage access. 1 when it opened.
proc ::vandroid::pick_files {} {
    variable app
    if {![on]} { return 0 }
    if {[catch {
        borg activity android.intent.action.MAIN {} {} {} [list $app $app.AttachActivity]
    } err]} {
        catch { ::vstatus::set_msg $err }
        return 0
    }
    return 1
}

# The word an "attach" intent must repeat (AttachNonce.java): the
# desktop's own file picker mints it into this process; an exported
# launcher activity means any app's intent can otherwise name a path to
# attach (files/home/.env). Read live from the same :desktop process;
# "" off a phone (the Tcl suites stub it) or before any picker ran.
#
# `borg queryfields` answers {fieldName value …}: the key is the Java
# FIELD's name, `nonce` (androidtest checks AttachNonce.java declares
# it). 968c0be read `nonce` from a field named `current`, and the picker
# cleared it before the intent reached Tcl — every attach was refused.
proc ::vandroid::attach_nonce {} {
    if {![on]} { return "" }
    if {[catch { borg queryfields chat.janitor.veles.AttachNonce } fields]} {
        return ""
    }
    if {[catch { dict get $fields nonce } nonce]} { return "" }
    return $nonce
}

# The word the last attach spent: the picker leaves its word set (it
# cannot know when Tcl has read it), so the desktop refuses a second
# intent naming it — a replay attaches nothing.
namespace eval ::vandroid { variable attach_spent "" }

# An intent sent to the desktop (`borg onintent`): the paths
# AttachActivity copied ("attach", one a line), or the session a
# notification is about ("session", Notifier.java). The launcher's own
# intent, which the callback is also given once, carries neither.
proc ::vandroid::intent {action uri type categories extras} {
    # A pinned app's shortcut (PinActivity.java): veles-app://<name>. It
    # names an app and nothing else; the app runs under the person's
    # grants like any other start (lib/apps.tcl).
    if {[regexp {^veles-app://([a-z0-9][a-z0-9-]{0,31})/?$} $uri -> app]} {
        after 1500 [list ::vapps::open_from_shortcut $app]
        return 1
    }
    if {![catch { dict get $extras session } sid] && $sid ne ""} {
        # A notification that started the desktop arrives before main.tcl
        # opens the newest session (600 ms in); this one is opened after.
        after 1000 [list ::vandroid::show_session $sid]
        return 1
    }
    if {[catch { dict get $extras attach } paths]} { return 0 }
    # DesktopActivity is exported: a path is attached only when the
    # intent repeats the picker's one-time word, once. Anything else — a
    # foreign app, a stale or spent word, none — attaches nothing.
    variable attach_spent
    set sent [expr {![catch { dict get $extras attach_nonce } n] ? $n : ""}]
    if {$sent eq "" || $sent eq $attach_spent || $sent ne [attach_nonce]} {
        return 0
    }
    set attach_spent $sent
    set n 0
    foreach p [split $paths \n] {
        if {$p ne "" && ![catch { ::vchat::attach $p } ok] && $ok} { incr n }
    }
    return $n
}

# A session on the screen: the chat's tab, the list closed, the session
# open (and an open ask of it drawn, as opening a session does).
proc ::vandroid::show_session {sid} {
    catch {
        foreach id [::vtabs::ids] {
            if {".mid.chatwrap" in [::vtabs::regions $id]} { ::vtabs::select $id ; break }
        }
    }
    drawer_close
    if {[catch { ::vchat::open_session $sid } err]} {
        catch { ::vstatus::set_msg $err }
        return 0
    }
    return 1
}
