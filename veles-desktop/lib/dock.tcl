# veles-desktop/lib/dock.tcl — resizable + undockable regions.
#
# Doctrine: the main window is three dockable regions inside ONE
# horizontal panedwindow (.mid) — sidebar (left), chatwrap (findbar +
# chat), panes (right Files/Review/Preview/Term). The panedwindow
# sashes make every region drag-resizable; sidebar and panes remember
# their width and can be UNDOCKED into their own toplevel and redocked.
#
# Tk hard rule: a widget cannot be reparented across toplevels. So
# undock is a REBUILD — each region exposes `rebuild {root}` that
# destroys and reconstructs itself at the new root path (docked:
# .sidebar / .panes; floating: the body frame of its toplevel). State
# that matters (sidebar pins+filter, panes active tab) lives in
# namespace variables and survives the rebuild.
#
# Keyboard-only operation:
#   palette → "Dock/undock sidebar / right panes"
#   emacs: C-x s sidebar · C-x p panes · C-x o other-window (which-key
#   popup shows the prefix menu)
#   all modes: Alt-Left / Alt-Right walk focus between regions;
#   Shift-Left / Shift-Right the same.

namespace eval ::vdock {
    variable sizes  {}
    variable floats {}
    variable strips {}
    # region -> rebuild proc (rebuilds the region at a new widget root).
    # Undockable regions only; .mid.chatwrap is a region but is never
    # torn off, so it has no rebuilder.
    variable rebuilders {
        .sidebar      ::vsidebar::rebuild
        .panes        ::vpanes::rebuild
    }
    # region -> the BUFFERS it shows, in visual order. What `C-x o`
    # walks. A region can show more than one (chatwrap holds both the
    # transcript and the composer), which is why this is not the same
    # as focus_target.
    #
    # Values are SCRIPTS, evaluated on demand: the right-hand pane's
    # content depends on which tab it is showing, and a region can be
    # rebuilt at a new widget path when it is undocked.
    variable buffers {
        .sidebar      {list $::vsidebar::R.list}
        .mid.chatwrap {::vdock::_chat_buffers}
        .panes        {list [::vpanes::content_widget]}
    }
    # region -> script yielding its CURRENT widget root, for regions
    # whose docked and floating paths differ.
    variable roots {}
}

# Register a region so tabs, undocking and the focus ring all know
# about it. `rebuilder` may be "" for a region that is never torn off.
#
# NB the parameter is `bufs`, not `buffers`: `variable buffers` in a
# proc whose argument is also named `buffers` is an error in Tcl
# ("variable already exists"), the same collision place.tcl hit with
# its margin.
proc ::vdock::register {w bufs {rebuilder ""}} {
    variable rebuilders
    variable buffers
    dict set buffers $w $bufs
    if {$rebuilder ne ""} { dict set rebuilders $w $rebuilder }
}

# Declare a region a BOTTOM STRIP: full-width under .mid, never a
# column. The Instances controls strip is the reason this exists —
# packed as a column its separators stretched the full window height
# and its buttons floated in dead space (2026-08-28 user report).
# Registered like any region (buffers, rebuilder, float); only its
# DOCKED geometry differs.
proc ::vdock::register_strip {w} {
    variable strips
    dict set strips $w 1
}

proc ::vdock::is_strip {w} {
    variable strips
    return [expr {[dict exists $strips $w] ? 1 : 0}]
}

# Every registered strip region, any state.
proc ::vdock::strip_list {} {
    variable strips
    return [dict keys $strips]
}

# Is this region currently torn off into its own toplevel? A floating
# region must not be dragged back into .mid by a tab switch.
proc ::vdock::floating {w} {
    variable floats
    return [expr {[dict exists $floats $w] ? 1 : 0}]
}

# The buffers a region is showing right now, existing and mapped only.
proc ::vdock::region_buffers {w} {
    variable buffers
    if {![dict exists $buffers $w]} { return {} }
    set out {}
    if {[catch { uplevel #0 [dict get $buffers $w] } ws]} { return {} }
    foreach b $ws {
        if {$b ne "" && [winfo exists $b] && [winfo ismapped $b]} { lappend out $b }
    }
    return $out
}

# ── build ────────────────────────────────────────────────────────────

proc ::vdock::build {} {
    panedwindow .mid -orient horizontal -sashwidth 6 -sashrelief flat \
        -background $::vt::T(hairline) -borderwidth 0 -opaqueresize 1 \
        -sashpad 2
    pack .mid -side left -fill both -expand 1
    _remember_loop
}

# Add a region to the panedwindow.
#
# `stretch` is which region absorbs the slack when the window resizes.
# "" keeps the historic rule (the transcript stretches, everything else
# holds its width), which is right for the Chat layout and wrong for
# any tab the transcript is not part of — a Dev tab whose only region
# is -stretch never would sit at 140px with the rest of the window
# blank. ::vtabs passes it explicitly.
proc ::vdock::dock {w {stretch ""}} {
    variable sizes
    if {[lsearch -exact [.mid panes] $w] >= 0} { return }
    set opts {}
    if {[dict exists $sizes $w]} {
        lappend opts -width [dict get $sizes $w]
    }
    if {$stretch eq ""} {
        set stretch [expr {$w eq ".mid.chatwrap" ? "always" : "never"}]
    }
    if {$stretch eq "always"} {
        lappend opts -stretch always -minsize 300
    } else {
        lappend opts -stretch never -minsize 140
    }
    # panedwindow add takes SINGLE distances, not {a b} pairs.
    if {$w ne ".mid.chatwrap"} { lappend opts -padx 4 -pady 6 }
    .mid add $w {*}$opts
}

# ── bottom strips ────────────────────────────────────────────────────
#
# A strip region docks PACKED UNDER .mid, full width — never as a
# panedwindow column. tabs.tcl calls dock_strip/undock_strip on tab
# switches the way it calls dock for columns; undock/redock/toggle and
# the float machinery work unchanged, because Tk's reparent rule makes
# undocking a rebuild anyway.

# Dock a strip region: packed -side bottom -BEFORE .mid, so the strip
# carves its full-width parcel before .mid expands into the rest —
# packed after, .mid's -expand would leave the strip a zero-height
# cavity. Always (re)issues the canonical pack: a re-pack with the same
# options is a no-op, and a strip that ended up packed anywhere else
# settles into its slot without waiting for a tab switch to notice.
proc ::vdock::dock_strip {w} {
    if {![winfo exists .mid]} { return }
    if {[winfo toplevel $w] ne "."} { return }   ;# floating: rebuilt at its float
    pack $w -side bottom -fill x -before .mid
}

proc ::vdock::undock_strip {w} {
    catch { pack forget $w }
}

# Current widget root of a region (docked name == canonical name;
# floating regions report their float body path via their namespace R).
proc ::vdock::_root_of {w} {
    variable roots
    switch -- $w {
        .sidebar { return $::vsidebar::R }
        .panes   { return $::vpanes::R }
    }
    if {[dict exists $roots $w]} {
        if {![catch { uplevel #0 [dict get $roots $w] } r]} { return $r }
    }
    return $w
}

# A region whose docked path and floating path differ registers how to
# find its current root (usually a namespace variable).
proc ::vdock::register_root {w script} {
    variable roots
    dict set roots $w $script
}

proc ::vdock::undock {w} {
    variable sizes
    variable floats
    variable rebuilders
    if {![dict exists $rebuilders $w]} { return }
    if {![docked $w]} { return }
    # Remember width from the docked geometry (columns only — a strip
    # is the width of the window, meaningless as a float size).
    if {![is_strip $w]} { dict set sizes $w [winfo width $w] }
    if {[is_strip $w]} { undock_strip $w } else { .mid forget $w }
    set t .float_[string map {. ""} $w]
    if {[winfo exists $t]} { destroy $t }
    toplevel $t
    # A %s-slot key with the region name as the argument, so the sentence
    # is translatable and the name stays itself. NOT a registered widget
    # — a window title has no registration channel; a swap leaves this
    # one in the old language until the float is rebuilt. Stated rather
    # than implied: the alternative is registering titles on a widget
    # that does not exist.
    wm title $t [format [::vi18n::mc {veles — %s}] [string trimleft $w .]]
    # The C-x prefix exists only in emacs/vi, so name it only there.
    set _k [expr {$w eq ".sidebar" ? "s" : "p"}]
    set _hint [expr {$::vkeymap::current eq "normie" ? "" : " (C-x $_k)"}]
    # Build the label FIRST: passing $_hint as a bare word to ttk::button
    # makes an EMPTY hint vanish into an empty argument, which Tk then
    # reads as an option name — "wrong # args: should be ttk::button
    # pathName ?-option value ...?" on a normie keymap, where _hint is "".
    set _label [::vi18n::mc {⇦ dock back%s} $_hint]
    ttk::button $t.dockbtn -text $_label \
        -command [list ::vdock::redock $w]
    pack $t.dockbtn -side top -anchor e -padx 2 -pady 2
    dict set floats $w $t
    wm protocol $t WM_DELETE_WINDOW [list ::vdock::redock $w]
    # Rebuild the region inside the float. The rebuild proc owns its
    # root widget (creates + packs it) — we must NOT pre-create the
    # body frame or the names collide.
    [dict get $rebuilders $w] $t.body
    if {[is_strip $w]} {
        # A strip floats at its natural size: one row of buttons, not
        # the window width it happened to be docked at.
        catch { wm geometry $t "" }
    } else {
        set width [dict get $sizes $w]
        wm geometry $t "${width}x640+[expr {[winfo rootx .] + 60}]+[expr {[winfo rooty .] + 40}]"
    }
}

proc ::vdock::redock {w} {
    variable floats
    variable rebuilders
    if {![dict exists $floats $w]} { return }
    set t [dict get $floats $w]
    dict unset floats $w
    catch { destroy $t }
    # Rebuild back at the docked root, then re-add to the pane. The
    # rebuild proc may pack itself (strips and floats do); dock_strip
    # re-packs -side bottom if it did, so the geometry stays canonical.
    [dict get $rebuilders $w] $w
    if {[is_strip $w]} { dock_strip $w } else { dock $w }
}

proc ::vdock::toggle {w} {
    variable floats
    if {[dict exists $floats $w]} {
        redock $w
        ::vstatus::set_msg [format [::vi18n::mc {docked %s}] [string trimleft $w .]]
    } elseif {[docked $w]} {
        undock $w
        ::vstatus::set_msg [format [::vi18n::mc {undocked %s}] [string trimleft $w .]]
    } else {
        # Not docked and not floating (hidden right panes) — dock it.
        if {[is_strip $w]} { dock_strip $w } else { dock $w }
        ::vstatus::set_msg [format [::vi18n::mc {docked %s}] [string trimleft $w .]]
    }
}

proc ::vdock::docked {w} {
    variable strips
    if {[dict exists $strips $w]} {
        # A strip is docked iff packed into the main window below .mid.
        # `pack info` throws when the window is not managed at all, and
        # does not report -before/-after — -in . is the parent test.
        if {![winfo exists $w]} { return 0 }
        if {[catch { pack info $w } pi] || $pi eq ""} { return 0 }
        return [expr {[lindex $pi [expr {[lsearch -exact $pi -side] + 1}]] eq "bottom"
            && [lindex $pi [expr {[lsearch -exact $pi -in] + 1}]] eq "."}]
    }
    return [expr {[lsearch -exact [.mid panes] $w] >= 0}]
}

proc ::vdock::visible {w} {
    variable floats
    if {[docked $w]} { return 1 }
    if {[dict exists $floats $w] && [winfo exists [dict get $floats $w]]} { return 1 }
    return 0
}

# ── width memory ─────────────────────────────────────────────────────

proc ::vdock::remember_sizes {} {
    variable sizes
    variable rebuilders
    # Widths of stacked regions (a phone in portrait) are the screen's,
    # and would dock them that wide when it turns.
    if {[winfo exists .mid] && [.mid cget -orient] ne "horizontal"} { return }
    foreach w [dict keys $rebuilders] {
        set root [_root_of $w]
        if {[winfo exists $root] && [winfo ismapped $root]} {
            dict set sizes $w [winfo width $root]
        }
    }
}

proc ::vdock::_remember_loop {} {
    catch remember_sizes
    after 10000 ::vdock::_remember_loop
}

# ── keyboard focus walk (C-x o / Alt-←/→ / Shift-←/→) ───────────────
# Regions cycle: sidebar list → composer → right panes tab bar.

# The content widget for a region — a BUFFER, never a toolbar.
#
# `.panes` used to answer `$::vpanes::R.tabs.files`, which is a tab
# BUTTON: `C-x o` and Shift-arrows landed on chrome. Emacs
# `other-window` moves between windows showing buffers, so ask the
# panes module which content widget the visible tab is currently
# showing.
proc ::vdock::focus_target {w} {
    switch -- $w {
        .sidebar      { return $::vsidebar::R.list }
        .mid.chatwrap { return .mid.chatwrap.chat.input }
        .panes        { return [::vpanes::content_widget] }
    }
    return ""
}

# Put focus INTO a region: its declared focus target when it has one,
# else its first registered buffer. `focus_target` only knows the three
# original regions by name, and a tab full of regions it has never heard
# of (the IRC tab) still has to be reachable by keyboard.
# Returns the widget it focused, or "" — not a boolean. Under a bare
# Xvfb with no window manager the toplevel may never hold focus, so
# `[focus]` reads back empty and a test asserting on it proves nothing.
# What is verifiable is which widget was CHOSEN.
proc ::vdock::focus_region {w} {
    set t [focus_target $w]
    if {$t eq ""} { set t [lindex [region_buffers $w] 0] }
    if {$t eq "" || ![winfo exists $t]} { return "" }
    focus $t
    return $t
}

# The buffer ring, in visual order. Only widgets that exist and are
# mapped — an undocked or hidden region is not in the ring.
#
# Scoped to the ACTIVE TAB: `C-x o` is emacs `other-window`, and the
# other windows are the ones in front of you. Walking onto a buffer
# belonging to a tab you are not looking at would move focus somewhere
# invisible. Falls back to the default region order when tabs are not
# in play, so this still works in a bare test harness.
proc ::vdock::buffer_ring {} {
    set ring {}
    foreach w [tab_regions] {
        foreach b [region_buffers $w] { lappend ring $b }
    }
    return $ring
}

# The regions of the active tab, or the default three when ::vtabs is
# absent or has not booted.
proc ::vdock::tab_regions {} {
    if {[llength [info commands ::vtabs::regions]]} {
        set r [::vtabs::regions]
        if {[llength $r]} { return $r }
    }
    return {.sidebar .mid.chatwrap .panes}
}

# emacs `other-window`: step through the buffer ring. Wraps.
proc ::vdock::focus_buffer {delta} {
    set ring [buffer_ring]
    set n [llength $ring]
    if {$n == 0} { return }
    # Where are we? Match the focused widget or any ancestor of it, so
    # focus sitting on a child (a text widget inside a frame) still
    # resolves to its buffer.
    set f [focus]
    set i -1
    for {set k 0} {$k < $n} {incr k} {
        set w [lindex $ring $k]
        if {$f eq $w || [string match "$w.*" $f]} { set i $k ; break }
    }
    if {$i < 0} { set i 0 ; set delta 0 }
    set next [lindex $ring [expr {(($i + $delta) % $n + $n) % $n}]]
    catch { focus $next }
    catch { ::vstatus::set_msg [format [::vi18n::mc {focus: %s}] [_buffer_label $next]] }
}

# The chat region's focusable buffers: the transcript ON SCREEN, and the
# composer.
#
# A proc rather than a `list` literal in the registry because the
# transcript is no longer one fixed path — sessions keep their own
# widget and only the first one ever gets `.mid.chatwrap.chat.log`. The
# fallback matters as much as the lookup: `region_buffers` evaluates
# these under `catch`, so an unresolvable widget silently empties the
# whole ring rather than failing loudly, and a bare `[::vchat::log]`
# here would do exactly that anywhere ::vchat is not loaded.
proc ::vdock::_chat_buffers {} {
    set t .mid.chatwrap.chat.log
    catch { set t [::vchat::log] }
    return [list $t .mid.chatwrap.chat.input]
}

proc ::vdock::_buffer_label {w} {
    # Any transcript, not one fixed path: the session on screen owns its
    # own widget now, and only the FIRST session ever gets the historic
    # one. Comparing against the literal labelled every later session's
    # transcript "pane: <whatever>" in the focus ring.
    if {$w eq ".mid.chatwrap.chat.log"} { return "transcript" }
    if {![catch { ::vchat::logs_all } _logs] && [lsearch -exact $_logs $w] >= 0} {
        return "transcript"
    }
    if {$w eq ".mid.chatwrap.chat.input"} { return "composer" }
    if {$w eq "$::vsidebar::R.list"} { return "sessions" }
    return "pane: $::vpanes::current"
}

proc ::vdock::_focus_cycle {delta} {
    variable floats
    set order [tab_regions]
    set present {}
    foreach w $order {
        if {[visible $w]} { lappend present $w }
    }
    if {[llength $present] == 0} { return }
    # Where is focus now?
    set cur .mid.chatwrap
    set f [focus]
    foreach w $present {
        set t [focus_target $w]
        if {$t ne "" && [string match "$t*" $f]} { set cur $w; break }
    }
    set i [lsearch -exact $present $cur]
    if {$i < 0} { set i 0 }
    set n [llength $present]
    set next [lindex $present [expr {($i + $delta + $n) % $n}]]
    # Undocked regions live in a float toplevel — raise it.
    if {[dict exists $floats $next]} {
        catch { wm deiconify [dict get $floats $next] }
        catch { raise [dict get $floats $next] }
    }
    set target [focus_target $next]
    if {$target ne "" && [winfo exists $target]} {
        focus $target
    }
}

# Bound to C-x o / C-x b and Shift-arrows. These walk BUFFERS now, not
# regions: stepping onto a tab strip is not "the other window".
proc ::vdock::focus_left {}  { focus_buffer -1 }
proc ::vdock::focus_right {} { focus_buffer 1 }

# a11y: jump straight to the transcript (read-only text widget) — the
# keyboard counterpart to "focus composer" (Ctrl-L / M-l). Scroll to
# the end so the newest content is at hand, then focus. Screen readers
# and the built-in Text widget navigation (arrows, Ctrl-Home/End) take
# over from here.
proc ::vdock::focus_transcript {} {
    set t [::vchat::log]
    if {![winfo exists $t]} { return }
    catch { $t see end }
    focus $t
    ::vstatus::set_msg [format [::vi18n::mc {transcript — arrows to read, %s for composer}] \
        [::vkeymap::key_for {focus .mid.chatwrap.chat.input}]]
}

# ── layout persistence ───────────────────────────────────────────────
# Window geometry, sash positions, and composer height survive restarts
# through the desktop state blob. Loaded at boot, saved on sash moves,
# grip drags, and window close.

proc ::vdock::save_layout {} {
    set up [dict create wm_geometry [wm geometry .]]
    # Each of these can legitimately be absent — the panedwindow may not
    # be built yet during startup — so a missing one is skipped, not an
    # error, but the WRITE itself is reported. Sashes are x positions: a
    # phone in portrait stacks the regions (lib/android.tcl) and has none
    # worth keeping.
    if {![winfo exists .mid] || [.mid cget -orient] eq "horizontal"} {
        catch { dict set up sash0 [lindex [.mid sash coord 0] 0] }
        catch { dict set up sash1 [lindex [.mid sash coord 1] 0] }
    }
    catch { dict set up composer_height [.mid.chatwrap.chat.input cget -height] }
    if {![::vstate::merge $up]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {layout not saved: %s}] $::vstate::error] }
    }
}

# Save periodically too (a crash shouldn't lose the layout). The timer
# is the app talking to itself, not a person: both requests of a save
# (GET + POST /api/desktop/state) go out passive (X-Veles-Passive,
# lib/api.tcl), so it never counts as use on the phone's idle clock,
# and a phone's desktop in the background stops it like its polls
# (lib/android.tcl). 2026-10-04: running there every 30 s, it was what
# kept the phone's engine from ever stopping. The WM_DELETE_WINDOW save
# and `background`'s own save are gestures and stay uses.
proc ::vdock::_layout_save_loop {} {
    variable save_after
    set ::vapi::passive 1
    catch save_layout
    set ::vapi::passive 0
    set save_after [after 30000 ::vdock::_layout_save_loop]
}

proc ::vdock::layout_save_stop {} {
    variable save_after
    if {[info exists save_after] && $save_after ne ""} {
        after cancel $save_after
    }
    set save_after ""
}

# Re-arm after `layout_save_stop`; the first save is a period away (the
# caller has just saved, or had nothing new to save).
proc ::vdock::layout_save_start {} {
    variable save_after
    layout_save_stop
    set save_after [after 30000 ::vdock::_layout_save_loop]
}

proc ::vdock::load_layout {} {
    set st [::vstate::all]
    if {![dict size $st]} { return }
    catch {
        set g [dict get $st wm_geometry]
        # Only accept sane geometries (a float toplevel could have left
        # a tiny/negative one behind).
        if {[regexp {^\d+x\d+\+-?\d+\+-?\d+$} $g]} { wm geometry . $g }
    }
    catch { ::vchat::set_composer_height [dict get $st composer_height] }
    # Sashes: apply after the first layout pass settles.
    #
    # This used to call `.mid sash pos N`, which is not a Tk subcommand
    # at all — a core panedwindow takes `sash coord|dragto|mark|place`.
    # Both the save and the restore threw into a catch, so the sash
    # positions have never once survived a restart. The values are baked
    # into the scheduled script rather than read from a global at fire
    # time, which is also how they used to arrive: unset.
    foreach {i key} {0 sash0 1 sash1} {
        if {![dict exists $st $key]} { continue }
        set x [dict get $st $key]
        if {![string is integer -strict $x]} { continue }
        after 300 [list ::vdock::_place_sash $i $x]
    }
}

proc ::vdock::_place_sash {i x} {
    if {![winfo exists .mid]} { return }
    if {[.mid cget -orient] ne "horizontal"} { return }
    # `sash place` wants both coordinates; a horizontal panedwindow only
    # moves in x, so keep the y it already has.
    if {[catch { lindex [.mid sash coord $i] 1 } y]} { return }
    catch { .mid sash place $i $x $y }
}
