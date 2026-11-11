#!/usr/bin/env wish
# veles-desktop/lib/instwindowtest.tcl — the Instances tab as a person
# meets it: the REAL desktop (main.tcl), booted unattached on a scratch
# home, measured in its own mapped window.
#
# Why a booted window and not a fixture. insttest drives the pane's
# procs on bare widgets, and these shipped green past it:
#
#   SMOKE-4   the control strip needed 2227 px. At the default
#             1080x720 eleven of its twenty actions (Start, Stop,
#             Restart, Fleet deploy…, Add…, Edit…, Remove among them)
#             were never mapped, and at 1440 px eight still were not.
#             The wrapping existed, on Android only.
#   SMOKE-5   the cluster registry came back collapsed on every refresh:
#             no node showed by itself, and a group the person opened
#             closed again 15 s later.
#   SMOKE-6   entering the tab fetched nothing: the list stayed empty, or
#             showed a node that had been down for minutes as fresh,
#             until the next 15 s tick.
#   SMOKE-10  the Fleet deploy dialog was fixed at 760x620 and asked
#             1349x726: four actions, its status line and Close unmapped,
#             and its release tag v0.2.0 on a 0.2.1 fleet.
#   SMOKE-16  the Add instance dialog was forced to 380 px wide: the url
#             showed eight characters and Cancel read "Can".
#   SMOKE-21  (with it) on a phone the list's fixed columns left the
#             name column 20 px: two letters of each node.
#
# So every check here measures a mapped widget's rectangle inside its
# toplevel, at the sizes people use: the default 1080x720, 1440x1000, a
# longer locale, and a phone's 360 dp portrait. Nothing asserts a width
# constant; the strip's controls are found through the pane's
# dispatchers (`_ctl_<id>`), so a control added later is checked too.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]
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

if {![info exists ::env(XVFB_RUNNING)] && [info exists ::env(DISPLAY)]} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

# A scratch person: no config, no saved instances, no gateway. The boot
# is the unattached one (no VELES_API), which issues no request.
set _tmp /tmp
if {[info exists ::env(TMPDIR)] && [file isdirectory $::env(TMPDIR)]} { set _tmp $::env(TMPDIR) }
set _scratch [file join $_tmp veles-instwindow-[pid]]
foreach {var sub} {HOME home XDG_CONFIG_HOME config XDG_DATA_HOME data
                   XDG_CACHE_HOME cache VELES_HOME veles} {
    file mkdir [file join $_scratch $sub]
    set ::env($var) [file join $_scratch $sub]
}
foreach var {VELES_API VELES_TOKEN VELES_INSTANCE VELES_DESKTOP_PORT VELES_EXE VELES_LAYOUT_PROBE} {
    unset -nocomplain ::env($var)
}
set ::env(VELES_LOCALE) en

namespace eval ::_iw {}

proc ::_iw::settle {} {
    # A reflow runs from <Configure>, which the first pass delivers.
    for {set i 0} {$i < 4} {incr i} { update idletasks ; update }
}

# The window at a size, the way a person drags it there. The test
# server's screen may be smaller than the window (the gate's is
# 1024x768): Tk would hold the window to it.
proc ::_iw::size {w h} {
    wm maxsize . 4000 4000
    wm geometry . ${w}x$h
    settle
}

# Mapped, viewable, not squeezed below what it asks for, and its whole
# rectangle inside its toplevel: on screen, in other words.
proc ::_iw::on_screen {w} {
    if {![winfo exists $w] || ![winfo ismapped $w] || ![winfo viewable $w]} { return 0 }
    set top [winfo toplevel $w]
    set x [expr {[winfo rootx $w] - [winfo rootx $top]}]
    set y [expr {[winfo rooty $w] - [winfo rooty $top]}]
    return [expr {$x >= 0 && $y >= 0
        && $x + [winfo width $w] <= [winfo width $top]
        && $y + [winfo height $w] <= [winfo height $top]
        && [winfo width $w] >= [winfo reqwidth $w] - 1}]
}

# Every action the pane dispatches (`_ctl_<id>`) and the control that
# offers it. A control nobody can see is a control that does not exist.
proc ::_iw::actions {} {
    set ids {}
    foreach p [info procs ::vinstpane::_ctl_*] {
        lappend ids [string range [namespace tail $p] 5 end]
    }
    return [lsort $ids]
}
proc ::_iw::offscreen {} {
    set bad {}
    foreach id [actions] {
        if {![on_screen $::vinstpane::R_ctl.$id]} { lappend bad $id }
    }
    return $bad
}

# VELES_IW_SHOTS=<dir>: a picture of each state checked, to look at.
proc ::_iw::shot {name {w .}} {
    if {![info exists ::env(VELES_IW_SHOTS)]} { return }
    settle
    catch { exec import -display $::env(DISPLAY) -window [winfo id $w] \
        [file join $::env(VELES_IW_SHOTS) $name.png] }
}

# Each cluster node's name whole in the tree column: the column is at
# least as wide as the name's text, two indents (its depth and the
# group's indicator) and the status dot's room.
proc ::_iw::names_check {label} {
    settle
    set t $::vinstpane::R_list.list
    set font [ttk::style lookup Treeview -font]
    if {$font eq ""} { set font TkDefaultFont }
    set indent [ttk::style lookup Treeview -indent]
    if {![string is integer -strict $indent]} { set indent 20 }
    set cut {}
    foreach id {cluster:node-a cluster:node-b cluster:node-c} {
        set need [expr {[font measure $font [$t item $id -text]] + 2 * $indent + 8}]
        if {[$t column #0 -width] < $need} { lappend cut $id }
    }
    ok "$label: every node's name is whole in the list (SMOKE-21)" { $cut eq {} }
    if {[llength $cut]} { puts "    cut: $cut (name column [$t column #0 -width] px of [winfo width $t])" }
}

proc ::_iw::strip_check {label} {
    settle
    shot [string map {{ } - , {}} $label]
    set bad [offscreen]
    ok "$label: every Instances action is on screen ([llength [actions]] of them)" {
        [llength [actions]] >= 20 && $bad eq {}
    }
    if {[llength $bad]} {
        puts "    off screen: $bad (window [wm geometry .], strip [winfo width .instctl]x[winfo height .instctl], needs [winfo reqwidth .instctl])"
    }
    return $bad
}

# The registry as a cluster's gateway answers /api/fleet: `nodes` lists
# {id name version stale self}.
proc ::_iw::fleet_body {nodes} {
    return [::json::json2dict [::fleetfix::fleet_json $nodes]]
}

# Only the transport is replaced: ::vapi::get_json_async's callers run as
# they do against a gateway, with the answer the cluster gives NOW.
proc ::_iw::serve_fleet {} {
    rename ::vapi::get_json_async ::_iw::real_get_json_async
    proc ::vapi::get_json_async {path cb args} {
        lappend ::_iw::asked $path
        if {$path eq "/api/fleet"} {
            after 0 [list {*}$cb 1 [::_iw::fleet_body $::_iw::nodes]]
        } else {
            after 0 [list {*}$cb 0 "HTTP 404"]
        }
        return ""
    }
}

# The node-management dialogs (lib/fleetmanage.tcl, 0.2.2): Config…,
# Audit log… and Sync all…, each opened as the strip opens it and
# measured mapped: every button and entry on screen, the dialog as large
# as it asks to be and no larger than the screen it is on (`maxw`x`maxh`;
# on a phone, the window).
proc ::_iw::controls {w} {
    set out {}
    foreach c [winfo children $w] {
        if {[winfo class $c] in {TButton TRadiobutton TEntry}} { lappend out $c }
        lappend out {*}[controls $c]
    }
    return $out
}
proc ::_iw::dialogs_check {label maxw maxh} {
    set node [lindex $::vinstpane::fleet_nodes 0]
    foreach {open dlg} [list \
        [list ::vfleetmanage::open_config [dict get $node id] [dict get $node name]] .fleetconfig \
        ::vfleetmanage::open_audit .fleetaudit \
        ::vfleetmanage::open_bulk_sync .fleetbulksync] {
        catch { destroy $dlg }
        {*}$open
        settle
        shot "[string trimleft $dlg .]-[winfo width .]" $dlg
        set bad {}
        foreach c [controls $dlg] {
            if {![on_screen $c]} { lappend bad [winfo name $c] }
        }
        set fits [expr {[winfo width $dlg] >= [winfo reqwidth $dlg] - 1
            && [winfo height $dlg] >= [winfo reqheight $dlg] - 1
            && [winfo width $dlg] <= $maxw && [winfo height $dlg] <= $maxh}]
        ok "$label: [string trimleft $dlg .] has every control on screen, at the size it asks for" {
            $bad eq {} && $fits && [llength [controls $dlg]] >= 2
        }
        if {$bad ne {} || !$fits} {
            puts "    $dlg [winfo width $dlg]x[winfo height $dlg], asks [winfo reqwidth $dlg]x[winfo reqheight $dlg], at most ${maxw}x$maxh; off screen: $bad"
        }
        destroy $dlg
    }
}

# Rows of the list a person can see: in the tree, under open groups, and
# inside the list's visible area.
proc ::_iw::visible {ids} {
    set t $::vinstpane::R_list.list
    set shown {}
    foreach id $ids {
        if {[$t exists $id] && [$t bbox $id] ne ""} { lappend shown $id }
    }
    return $shown
}

# The Add instance dialog is modal (tkwait): it is measured from a timer
# while it waits, then cancelled.
proc ::_iw::add_check {label} {
    set ::_iw::add {}
    after 300 ::_iw::measure_add
    ::vinstpane::add_entry
    lassign $::_iw::add fits fields cancel
    ok "$label: the Add instance dialog is as wide as it asks to be (SMOKE-16)" { $fits == 1 }
    ok "$label: …every field is on screen at its full width" { $fields eq {} }
    ok "$label: …and OK and Cancel are on screen" { $cancel == 1 }
    if {$fields ne {}} { puts "    clipped: $fields" }
}
proc ::_iw::measure_add {} {
    set d .instentry
    if {![winfo exists $d] || ![winfo ismapped $d]} { after 100 ::_iw::measure_add ; return }
    settle
    shot add-[winfo width .] $d
    set fields {}
    foreach e [winfo children $d] {
        if {[winfo class $e] eq "TEntry" && ![on_screen $e]} { lappend fields [winfo name $e] }
    }
    set ::_iw::add [list \
        [expr {[winfo width $d] >= [winfo reqwidth $d] && [winfo height $d] >= [winfo reqheight $d]}] \
        $fields [expr {[on_screen $d.btns.ok] && [on_screen $d.btns.cancel]}]]
    set ::vinstpane::_dlg_rc cancel
}

proc ::_iw::run {} {
    package require json
    source [file join $::_dir fleetfixtures.tcl]
    ::_iw::size 1080 720
    set t $::vinstpane::R_list.list
    set group $::vinstpane::registry_group
    set three {cluster:node-a cluster:node-b cluster:node-c}

    # ── entering the tab ────────────────────────────────────────────
    puts "entering the Instances tab"
    set ::_iw::nodes {{node-a alpha 0.2.1} {node-b beta 0.2.1} {node-c gamma 0.2.1 false true}}
    serve_fleet
    ::vtabs::select chat
    settle
    set ::_iw::asked {}
    ::vtabs::select instances
    update
    ok "entering the tab asks for the registry at once, not at the next 15 s tick (SMOKE-6)" {
        "/api/fleet" in $::_iw::asked
    }
    settle
    ok "every cluster node is on screen without a click (SMOKE-5)" {
        [visible $three] eq $three
    }
    ok "…under a group the tree shows open" {
        [$t exists $group] && [$t item $group -open]
    }
    shot entered

    puts "\na refresh leaves the list where the person put it"
    ::vinstpane::_select cluster:node-b
    if {[$t exists $group]} { $t item $group -open 0 }
    ::vinstpane::refresh_fleet
    settle
    ok "a group the person closed stays closed across a refresh (SMOKE-5)" {
        [$t exists $group] && ![$t item $group -open]
    }
    if {[$t exists $group]} { $t item $group -open 1 }
    ::vinstpane::refresh_fleet
    settle
    ok "…and one opened again stays open" {
        [$t exists $group] && [$t item $group -open]
    }
    ok "the selected node stays selected" {
        [$t selection] eq {cluster:node-b} && [::vinstpane::selected] eq {cluster:node-b}
    }
    # Enough nodes to scroll, the view moved down, and a refresh.
    set many {}
    for {set i 10} {$i < 70} {incr i} { lappend many [list node-$i n$i 0.2.1] }
    set ::_iw::nodes [concat $::_iw::nodes $many]
    ::vinstpane::refresh_fleet
    settle
    $t yview moveto 0.5
    settle
    set at [lindex [$t yview] 0]
    ::vinstpane::refresh_fleet
    settle
    ok "the list stays scrolled where it was" {
        $at > 0 && [lindex [$t yview] 0] == $at
    }
    # A window small enough that the detail scrolls.
    ::_iw::size 760 480
    set d $::vinstpane::R_detail.body
    $d yview moveto 1.0
    settle
    set dat [lindex [$d yview] 0]
    ::vinstpane::refresh_fleet
    ::vinstpane::poll_detail_once
    settle
    ok "the node's detail stays scrolled where the reader was" {
        $dat > 0 && [lindex [$d yview] 0] == $dat
    }
    ::_iw::size 1080 720
    set ::_iw::nodes [lrange $::_iw::nodes 0 2]
    $t yview moveto 0

    puts "\na node that went down while the tab was not shown"
    ::vtabs::select chat
    settle
    set ::_iw::nodes {{node-a alpha 0.2.1} {node-b beta 0.2.1 true} {node-c gamma 0.2.1 false true}}
    ::vtabs::select instances
    settle
    ok "on return its row is stale, not the snapshot from when the tab was left (SMOKE-6)" {
        "never" in [$t item cluster:node-b -tags]
        && [string match "○*" [$t item cluster:node-b -text]]
    }
    ok "…and so is its detail" {
        [string match "*[::vi18n::mc {Stale}]: true*" [$d get 1.0 end]]
    }
    ok "…and Forget node… is offered for it" { ![.instctl.forget instate disabled] }
    ::vinstpane::_select cluster:node-a

    # ── the Fleet deploy dialog ─────────────────────────────────────
    puts "\nthe Fleet deploy dialog"
    foreach lang {en de} {
        ::vi18n::use $lang
        set ::vfleetui::release_tag ""
        ::vinstpane::_ctl_deploy
        settle
        set dlg .fleetdeploy
        set bad {}
        set n 0
        if {[winfo exists $dlg]} {
            set n [llength [winfo children $dlg.body.actions]]
            foreach w [concat [winfo children $dlg.body.actions] [list $dlg.status $dlg.body.close]] {
                if {![on_screen $w]} { lappend bad [winfo name $w] }
            }
            shot deploy-$lang $dlg
        }
        ok "$lang: every deploy action, the status line and Close are on screen (SMOKE-10)" {
            $n == [llength $::vfleetui::actions] && $bad eq {}
        }
        if {[llength $bad]} { puts "    off screen: $bad ([wm geometry $dlg], asks [winfo reqwidth $dlg]x[winfo reqheight $dlg])" }
        ok "$lang: the dialog asks no more than a 1080x720 desktop" {
            [winfo exists $dlg] && [winfo reqwidth $dlg] <= 1080 && [winfo reqheight $dlg] <= 720
        }
        catch { destroy $dlg }
    }
    ::vi18n::use en
    ok "the release tag is the running version (the attached gateway's own registry row)" {
        $::vfleetui::release_tag eq "v0.2.1"
    }
    set ::vfleetui::release_tag v0.2.0-rc1
    ::vinstpane::_ctl_deploy
    settle
    ok "…and a tag someone typed is kept" { $::vfleetui::release_tag eq "v0.2.0-rc1" }
    catch { destroy .fleetdeploy }

    puts "\nthe Add instance dialog"
    add_check "1080x720"

    # ── the control strip ───────────────────────────────────────────
    puts "the control strip at the default size"
    ok "the window is 1080x720" {
        [winfo width .] == 1080 && [winfo height .] == 720
    }
    strip_check "1080x720"
    names_check "1080x720"
    dialogs_check "1080x720" [winfo screenwidth .] [winfo screenheight .]
    ok "1080x720: the strip leaves the list and the detail most of the window" {
        [winfo height .mid] >= [winfo height .] / 2
    }

    puts "\nthe control strip at 1440x1000"
    ::_iw::size 1440 1000
    strip_check "1440x1000"
    dialogs_check "1440x1000" [winfo screenwidth .] [winfo screenheight .]

    puts "\nthe control strip at the window's smallest"
    ::_iw::size 760 480
    strip_check "760x480"

    puts "\nthe control strip in a longer locale"
    ::_iw::size 1080 720
    ::vi18n::use de
    strip_check "1080x720 in German"
    ::vi18n::use en
    settle

    # ── the phone ───────────────────────────────────────────────────
    #
    # AndroWish is `borg` and `sdltk`; with them the desktop takes its
    # phone layout (lib/android.tcl): the touch theme's padding and the
    # regions stacked in portrait. Density 1, so one desktop pixel is
    # one dp, and the window is a 360 dp phone, the narrowest common.
    # On a phone the window is the screen: no desktop minimum size.
    puts "\nthe control strip on a phone"
    proc ::borg {args} {
        if {[lindex $args 0] eq "displaymetrics"} {
            return {density 1.0 densitydpi 160 width 360 height 780 xdpi 160.0 ydpi 160.0}
        }
        return ""
    }
    proc ::sdltk {args} {}
    set ::vandroid::density 1.0
    ::vandroid::touch_theme
    wm minsize . 1 1
    ::_iw::size 360 780
    ::vandroid::arrange
    settle
    ok "the window is a 360 dp phone" { [winfo width .] == 360 }
    strip_check "a 360x780 phone in portrait"
    names_check "a 360x780 phone in portrait"
    add_check "phone"
    dialogs_check "phone" [winfo width .] [winfo height .]
    ok "phone: the list and detail keep a third of the screen" {
        [winfo height .mid] >= [winfo height .] / 3
    }

    file delete -force $::_scratch
    puts "\ninstwindowtest: $::passed passed, $::failed failed"
    exit [expr {$::failed > 0 ? 1 : 0}]
}

after 120000 { puts "instwindowtest: timed out" ; exit 1 }
after 1500 {
    if {[catch ::_iw::run err]} {
        puts "  ✗ the run stopped: $err"
        puts $::errorInfo
        puts "\ninstwindowtest: $::passed passed, [incr ::failed] failed"
        exit 1
    }
}
source [file join $_dir .. main.tcl]
