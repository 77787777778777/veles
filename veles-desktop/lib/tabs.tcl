# veles-desktop/lib/tabs.tcl — a tab is a SAVED LAYOUT, not a pane.
#
# emacs `tab-bar-mode` semantics, and the distinction matters: a browser
# tab is one document filling the window, while an emacs tab is a whole
# window configuration. Picking the browser model here would have cost
# the thing the desktop is built around — watching the transcript while
# you work in a pane beside it.
#
# So the Chat tab is sidebar + transcript + right pane, the Dev tab is
# the editor with the whole window to itself, and each remembers its own
# splits, its own sash positions and where focus was. Switching between
# them is not "show a different thing", it is "put the window back the
# way it was".
#
# WHY THIS IS CHEAP
#
# ::vdock::undock has to REBUILD a region, because Tk cannot reparent a
# widget across toplevels. A tab switch never crosses a toplevel: the
# regions stay children of the same window and only their membership in
# the .mid panedwindow changes. `.mid forget` + `::vdock::dock` is the
# whole mechanism, widgets keep their contents, and nothing is
# destroyed — so a tab you have not looked at in an hour comes back
# scrolled exactly where you left it.
#
# WHAT A TAB OWNS
#
#   regions   which regions are in .mid, left to right
#   state     per-region state to reapply, e.g. {.panes {tab dev}} —
#             so the Chat tab's right pane and the Dev tab's right pane
#             are the same widget showing different things
#   sashes    the divider positions, captured on the way out
#   focus     the buffer that had focus, restored on the way in

namespace eval ::vtabs {
    variable order {}       ;# tab ids, left to right
    variable spec           ;# id -> {title regions state}
    variable saved          ;# id -> {sashes ... focus ... state ...}
    variable current ""
    variable bar .tabbar
    array set spec {}
    array set saved {}
}

# ── definition ──────────────────────────────────────────────────────

# Define (or redefine) a tab. `regions` is the left-to-right list of
# region names; `state` maps a region to the state it should be put in
# when this tab is shown.
# `initial` is which of `regions` are docked the first time the tab is
# shown; it defaults to all of them. They differ because the right-hand
# pane has always been revealed on demand — listing it in `regions`
# says "this tab's layout has a slot for it", not "force it open on
# startup". Which regions are actually docked is state, captured and
# restored like the sashes.
proc ::vtabs::define {id title regions {state {}} {initial ""}} {
    variable order
    variable spec
    variable saved
    if {$initial eq ""} { set initial $regions }
    set spec($id) [dict create title $title regions $regions state $state]
    if {![info exists saved($id)]} {
        set saved($id) [dict create state $state docked $initial]
    }
    if {$id ni $order} { lappend order $id }
}

# Which of this tab's regions are on screen right now.
proc ::vtabs::_capture_docked {id} {
    variable saved
    if {![winfo exists .mid]} { return }
    set live [.mid panes]
    set out {}
    foreach w [regions $id] {
        # A bottom strip never sits in .mid, so "in the panedwindow"
        # would drop it from the remembered set and a reload would
        # come back stripless. Ask ::vdock, whose answer for a strip
        # is "packed into the main window".
        if {[::vdock::is_strip $w]} {
            if {[::vdock::docked $w]} { lappend out $w }
            continue
        }
        if {[lsearch -exact $live $w] >= 0 || [::vdock::floating $w]} { lappend out $w }
    }
    dict set saved($id) docked $out
}

proc ::vtabs::_docked {id} {
    variable saved
    if {[info exists saved($id)] && [dict exists $saved($id) docked]} {
        return [dict get $saved($id) docked]
    }
    return [regions $id]
}

# The region that absorbs slack on resize: the transcript when this tab
# has one, the detail pane on the Instances tab (the controls strip
# would otherwise soak up the slack, which is wasted space), otherwise
# the last region, so a tab never leaves dead space. A bottom strip
# (`.instctl`) is never a candidate: it is not a column and cannot
# stretch vertically.
proc ::vtabs::_stretcher {id shown} {
    if {".mid.chatwrap" in $shown} { return .mid.chatwrap }
    if {".instdetail" in $shown} { return .instdetail }
    set cols {}
    foreach w $shown { if {![::vdock::is_strip $w]} { lappend cols $w } }
    if {[llength $cols] == 0} { set cols $shown }
    return [lindex $cols end]
}

proc ::vtabs::ids {}   { variable order   ; return $order }
proc ::vtabs::title {id} {
    variable spec
    if {![info exists spec($id)]} { return $id }
    set raw [dict get $spec($id) title]
    # The stored title is the English SOURCE ("Chat"), translated here —
    # the one choke point every consumer (_label's "C-1 Chat", the a11y
    # name) reads through. Translating at define() would bake one language
    # into the strip; doing it here means a live swap re-reads the catalog.
    if {[llength [info commands ::vi18n::mc]]} {
        return [::vi18n::mc $raw]
    }
    return $raw
}

# The regions of a tab (the ACTIVE one by default). ::vdock::tab_regions
# calls this with no argument to scope the focus ring.
proc ::vtabs::regions {{id ""}} {
    variable spec
    variable current
    if {$id eq ""} { set id $current }
    if {$id eq "" || ![info exists spec($id)]} { return {} }
    return [dict get $spec($id) regions]
}

proc ::vtabs::current {} { variable current ; return $current }

# ── per-region state ────────────────────────────────────────────────
#
# A region that has state a tab should remember declares two procs:
# `capture_state` and `apply_state`. Only .panes has any today (which
# of Files/Review/Preview/Term/Dev it is showing), which is exactly why
# the same right-hand pane can be Files in the Chat tab and the editor
# in the Dev tab without a second widget.

proc ::vtabs::_state_ns {w} {
    switch -- $w {
        .panes { return ::vpanes }
    }
    variable state_ns
    if {[info exists state_ns($w)]} { return $state_ns($w) }
    return ""
}

# A region with tab-remembered state registers its namespace here.
proc ::vtabs::register_state {w ns} {
    variable state_ns
    set state_ns($w) $ns
}

proc ::vtabs::_capture_state {id} {
    variable saved
    set st {}
    foreach w [regions $id] {
        set ns [_state_ns $w]
        if {$ns eq ""} { continue }
        if {[llength [info commands ${ns}::capture_state]]} {
            if {![catch { ${ns}::capture_state } v]} { dict set st $w $v }
        }
    }
    if {[dict size $st]} { dict set saved($id) state $st }
}

proc ::vtabs::_apply_state {id} {
    variable saved
    set st {}
    if {[info exists saved($id)] && [dict exists $saved($id) state]} {
        set st [dict get $saved($id) state]
    }
    foreach w [regions $id] {
        if {![dict exists $st $w]} { continue }
        set ns [_state_ns $w]
        if {$ns eq ""} { continue }
        if {[llength [info commands ${ns}::apply_state]]} {
            catch { ${ns}::apply_state [dict get $st $w] }
        }
    }
}

# ── sashes ──────────────────────────────────────────────────────────

proc ::vtabs::_capture_sashes {id} {
    variable saved
    if {![winfo exists .mid]} { return }
    # Sashes are x positions. A phone in portrait stacks the regions
    # (lib/android.tcl), and its sashes' x, kept, would squash the tab
    # to nothing when the phone turns.
    if {[.mid cget -orient] ne "horizontal"} { return }
    set n [llength [.mid panes]]
    set xs {}
    for {set i 0} {$i < $n - 1} {incr i} {
        if {[catch { lindex [.mid sash coord $i] 0 } x]} { break }
        lappend xs $x
    }
    if {[llength $xs]} { dict set saved($id) sashes $xs }
}

proc ::vtabs::_restore_sashes {id} {
    variable saved
    if {![winfo exists .mid]} { return }
    if {![info exists saved($id)] || ![dict exists $saved($id) sashes]} { return }
    set i 0
    foreach x [dict get $saved($id) sashes] {
        if {[string is integer -strict $x]} { ::vdock::_place_sash $i $x }
        incr i
    }
}

# ── switching ───────────────────────────────────────────────────────

proc ::vtabs::select {id} {
    variable spec
    variable saved
    variable current
    if {![info exists spec($id)]} { return 0 }
    if {$id eq $current} { _highlight ; return 1 }

    if {$current ne "" && [info exists spec($current)]} {
        _capture_sashes $current
        _capture_state $current
        _capture_docked $current
        set f [focus]
        if {$f ne ""} { dict set saved($current) focus $f }
    }

    # Unmanage everything currently in .mid, then add this tab's
    # regions in order. Order matters and `.mid add` appends, so a
    # partial forget would leave the previous tab's regions to the left
    # of this one's. Bottom strips are not .mid panes — they pack under
    # .mid and are forgotten the same way — but they follow the same
    # rule: unmanage ALL of the outgoing tab's regions before adding
    # this tab's, so no region survives on screen across a switch.
    if {[winfo exists .mid]} {
        foreach w [.mid panes] { catch { .mid forget $w } }
    }
    # A strip of the OUTGOING tab is not a .mid pane — forget it the
    # pack way, or it would sit under the next tab's layout too.
    foreach w [::vdock::strip_list] {
        if {[winfo exists $w] && ![::vdock::floating $w]} { catch { pack forget $w } }
    }
    set current $id
    set shown {}
    foreach w [_docked $id] {
        if {![winfo exists $w]} { continue }
        # A region torn off into its own window stays torn off. Docking
        # it back because a tab mentions it would yank a window out
        # from under someone who deliberately floated it.
        if {[::vdock::floating $w]} { continue }
        lappend shown $w
    }
    # Strips first (columns are added to .mid below), because a strip
    # packs -after .mid and pack resolves -after against windows that
    # exist — the ordering below keeps one canonical pass.
    set strips {}
    set cols {}
    foreach w $shown {
        if {[::vdock::is_strip $w]} { lappend strips $w } else { lappend cols $w }
    }
    set stretcher [_stretcher $id $shown]
    foreach w $cols {
        ::vdock::dock $w [expr {$w eq $stretcher ? "always" : "never"}]
    }
    foreach w $strips { ::vdock::dock_strip $w }
    _apply_state $id
    # ONE geometry pass, here, before anything reads the new layout.
    #
    # It was inside _restore_sashes, which returns early for a tab with
    # no remembered sashes — so the first switch to a tab left its
    # regions unmapped, ::vdock::buffer_ring (which filters on
    # `ismapped`) came back empty, and focus was never restored. C-x o
    # in a freshly opened tab did nothing at all.
    #
    # Synchronous on purpose: an `after` would race a second tab switch
    # and place one tab's sashes into another's layout.
    update idletasks
    _restore_sashes $id
    # A phone stacks the regions in portrait and makes the session list
    # a drawer; the regions were just docked afresh (lib/android.tcl).
    if {[llength [info commands ::vandroid::arrange]]} { ::vandroid::arrange }
    _restore_focus $id
    _highlight
    _persist
    return 1
}

proc ::vtabs::_restore_focus {id} {
    variable saved
    set target ""
    if {[info exists saved($id)] && [dict exists $saved($id) focus]} {
        set target [dict get $saved($id) focus]
    }
    if {$target ne "" && [winfo exists $target] && [winfo ismapped $target]} {
        catch { focus $target }
        return
    }
    # No remembered focus, or it is gone: land on the tab's first
    # buffer rather than leaving focus on a dead widget or the root.
    set ring [::vdock::buffer_ring]
    if {[llength $ring]} { catch { focus [lindex $ring 0] } }
}

# `C-1`..`C-9`. Out-of-range is a no-op, not an error — pressing C-7
# with three tabs open should do nothing at all.
proc ::vtabs::select_index {n} {
    variable order
    set i [expr {$n - 1}]
    if {$i < 0 || $i >= [llength $order]} { return 0 }
    return [select [lindex $order $i]]
}

# Focus the Nth region of the ACTIVE tab, counting only what is on
# screen. This is what Alt-N means in a tab that has no right-hand pane:
# "the Nth pane of what I am looking at".
#
# USER (spot test, 2026-08-25): "pressing M-\d combinations opens the
# corresponding side pane even in irc mode". Alt-1..5 ran ::vpanes::show
# unconditionally, and ::vpanes::show docks .panes when it is hidden — so
# a chord that means "pane within the current tab" reached across and
# forced a region from ANOTHER tab's layout into this one. A tab is a
# saved layout; a key that rewrites the layout it is used in is not
# a key that respects it.
# Returns the widget focused, or "" — see ::vdock::focus_region.
proc ::vtabs::focus_nth {n} {
    variable current
    if {$current eq "" || $n < 1} { return "" }
    set rs [_docked $current]
    if {$n > [llength $rs]} { return "" }
    return [::vdock::focus_region [lindex $rs [expr {$n - 1}]]]
}

# Does the active tab's layout have a slot for this region?
proc ::vtabs::has_region {w} {
    variable current
    if {$current eq ""} { return 1 }
    return [expr {$w in [regions $current]}]
}

proc ::vtabs::step {delta} {
    variable order
    variable current
    set n [llength $order]
    if {$n == 0} { return 0 }
    set i [lsearch -exact $order $current]
    if {$i < 0} { set i 0 }
    return [select [lindex $order [expr {(($i + $delta) % $n + $n) % $n}]]]
}

proc ::vtabs::next {}     { step 1 }
proc ::vtabs::previous {} { step -1 }

# ── the strip ───────────────────────────────────────────────────────

proc ::vtabs::build {} {
    variable bar
    variable order
    if {[winfo exists $bar]} { destroy $bar }
    ttk::frame $bar
    set n 0
    foreach id $order {
        incr n
        ttk::button $bar.t$id -text [_label $id $n] -width 13 \
            -command [list ::vtabs::select $id]
        pack $bar.t$id -side left -padx 1
        # The tab strip is the most literal case the coach exists for:
        # every one of these has a key, and the strip is where someone
        # who does not know that will click.
        catch { ::vcoach::attach $bar.t$id }
        catch { ::vi18n::aname $bar.t$id {switch to the %s tab} [title $id] }
    }
    # ⋮: every command, a click or a tap away (lib/cmdtree.tcl). USER
    # 2026-10-01: "I just want one button to do that plus literally
    # every other command so that it's all reachable from touching
    # buttons".
    ttk::button $bar.all -text "⋮" -width 0 -command ::vcmdtree::open
    catch { ::vi18n::aname $bar.all {All commands} }
    pack $bar.all -side right -padx 1
    pack $bar -side top -fill x -padx 8 -pady {2 0} -before .mid
    # ☰ and ⋮ on a phone, and tabs that wrap (lib/android.tcl).
    if {[llength [info commands ::vandroid::tab_strip]]} { ::vandroid::tab_strip $bar }
    _highlight
    # A live language swap re-texts the strip: the buttons' -text is a
    # stored string, so nothing re-reads the catalog otherwise (title()
    # translates on read; this re-reads). Registered once, guarded by the
    # bar existing.
    if {[llength [info commands ::vi18n::on_locale_change]]} {
        ::vi18n::on_locale_change ::vtabs::relabel
    }
    return $bar
}

# Re-text the strip's buttons after a language swap, in place — a rebuild
# would also work but loses the current tab for no reason. The ⋮ and the
# a11y names are re-rendered by i18n's own sweep; only the chord+title
# -text needs this.
proc ::vtabs::relabel {} {
    variable bar
    variable order
    if {![winfo exists $bar]} { return }
    set n 0
    foreach id $order {
        incr n
        if {[winfo exists $bar.t$id]} {
            catch { $bar.t$id configure -text [_label $id $n] }
        }
    }
}

# "C-1 Chat" — the chord, then the name.
#
# USER (spot test, 2026-08-25): "The tabs don't say which keyboard
# combination to use when you switch them by clicking". The coach says so
# AFTER a click, which is one click too late for someone reading the
# strip to find out what it can do.
#
# The chord comes from the binding registry via the command this very
# button runs, so it cannot advertise a key it does not answer to — the
# same join that fixed the 7 wrong palette hints. A bare digit would not
# do: the PANE strip already labels itself "1 Files" for Alt-1, and two
# strips both showing "1" for two different chords is worse than no
# label at all.
#
# An unbound tab (a tenth one, past C-9) just shows its title.
proc ::vtabs::_label {id n} {
    # A phone has no chords to show: "C-1 Chat" is noise there, and the
    # strip needs the width.
    if {[llength [info commands ::vandroid::on]] && [::vandroid::on]} { return [title $id] }
    set keys ""
    catch { set keys [::vkeymap::key_for [list ::vtabs::select_index $n]] }
    if {$keys eq ""} { return [title $id] }
    return "$keys [title $id]"
}

proc ::vtabs::_highlight {} {
    variable bar
    variable order
    variable current
    if {![winfo exists $bar]} { return }
    foreach id $order {
        if {![winfo exists $bar.t$id]} { continue }
        catch {
            $bar.t$id configure -style [expr {$id eq $current
                ? "DevTabOn.TButton" : "DevTab.TButton"}]
        }
    }
}

# ── persistence ─────────────────────────────────────────────────────
#
# Per-tab layout goes in the state blob under `tab_layout`, declared as
# `pairs` — a JSON object of string -> string. The VALUE is one opaque
# string per tab, whose contents are this module's business.
#
# Opaque on purpose. The blob's schema exists because a nested value
# fell through to a guesser and got shredded; handing it a nested
# object per tab would hand the same problem back. One escaped string
# is unambiguous on the way in and on the way out.

proc ::vtabs::_encode {id} {
    variable saved
    if {![info exists saved($id)]} { return "" }
    set out {}
    foreach k {sashes focus state docked} {
        if {[dict exists $saved($id) $k]} {
            dict set out $k [dict get $saved($id) $k]
        }
    }
    return $out
}

proc ::vtabs::_persist {} {
    variable order
    variable current
    set payload {}
    foreach id $order {
        set e [_encode $id]
        if {$e ne ""} { dict set payload $id $e }
    }
    if {![::vstate::merge [dict create tab_current $current tab_layout $payload]]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {tab layout not saved: %s}] $::vstate::error] }
    }
}

# Load remembered layouts. Call AFTER every tab is defined; a payload
# for a tab that no longer exists is dropped rather than resurrected.
proc ::vtabs::load {} {
    variable saved
    variable spec
    set st [::vstate::all]
    if {![dict size $st]} { return "" }
    if {[dict exists $st tab_layout]} {
        set payload [dict get $st tab_layout]
        if {![catch { dict size $payload }]} {
            dict for {id e} $payload {
                if {![info exists spec($id)]} { continue }
                if {[catch { dict size $e }]} { continue }
                foreach k {sashes focus state docked} {
                    if {[dict exists $e $k]} { dict set saved($id) $k [dict get $e $k] }
                }
            }
        }
    }
    if {[dict exists $st tab_current]} {
        set id [dict get $st tab_current]
        if {[info exists spec($id)]} { return $id }
    }
    return ""
}

# ── boot ────────────────────────────────────────────────────────────

# The default tabs. Chat keeps the layout the app has always had;
# Dev gives the editor the whole window, which is the reason the right
# pane's 40% was never enough for it.
proc ::vtabs::boot {} {
    variable order
    # The titles are English SOURCE strings, translated on read (title());
    # spelled as a table because a define's title is data the key
    # extractor cannot see, and a foreach table is a shape it reads
    # (scripts/i18n-keys.tcl). Inline, "Chat" never became a key, so no
    # catalog had it and the tab stayed English in every locale.
    foreach {id label} {chat "Chat" dev "Dev" irc "IRC" instances "Instances"} {
        set title($id) $label
    }
    # Chat's layout has a slot for the right-hand pane, but it opens
    # closed — the way the app has always started. Dev is the editor
    # with the whole window, which is what the right pane's 40% never
    # gave it.
    define chat $title(chat) {.sidebar .mid.chatwrap .panes} {.panes {tab files}} \
        {.sidebar .mid.chatwrap}
    define dev  $title(dev)  {.panes} {.panes {tab dev}}
    # The reason tabs exist. A buffer list, a transcript and a roster do
    # not fit in the right-hand pane's 40% beside a chat transcript;
    # here they get the window.
    if {[llength [info commands ::virc::boot]]} {
        define irc $title(irc) {.ircbufs .ircview .ircroster}
    }
    # The Instances pane — a virt-manager for bot instances. Guarded
    # like IRC so the tab exists exactly when its module loaded.
    # Regions are listed with the tree STRETCHING (last position);
    # ::vtabs::_stretcher picks the last region when there is no chat
    # transcript, which is the detail pane here.
    if {[llength [info commands ::vinstpane::boot]]} {
        define instances $title(instances) {.instlist .instdetail .instctl}
    }
    set want [load]
    build
    if {$want eq ""} { set want [lindex $order 0] }
    select $want
    return $want
}
