# veles-desktop/lib/findbar.tcl — Ctrl-F incremental search in the
# transcript (hermes find-bar parity): highlight all matches, step
# through with Return / Shift-Return (or ↑↓), Esc dismisses.
#
# Scope: rendered transcript only (hermes also covers editor panels —
# we have none; the FTS search in the palette covers all sessions).

namespace eval ::vfind {
    variable active 0
    variable matches {}
    variable cur -1
}

proc ::vfind::show {} {
    variable active
    set active 1
    .mid.chatwrap.findbar.entry delete 0 end
    # .mid.chatwrap.chat exists by now (built after findbar in main.tcl); anchor
    # above it.
    pack .mid.chatwrap.findbar -before .mid.chatwrap.chat -side top -fill x -padx 4 -pady {2 0}
    focus .mid.chatwrap.findbar.entry
}

proc ::vfind::hide {} {
    variable active
    variable matches
    variable cur
    set active 0
    set matches {}
    set cur -1
    reset_marks
    pack forget .mid.chatwrap.findbar
    focus .mid.chatwrap.chat.input
}

proc ::vfind::reset_marks {} {
    catch {
        [::vchat::log] tag remove find_hit 1.0 end
        [::vchat::log] tag remove find_cur 1.0 end
    }
}

proc ::vfind::run {} {
    variable matches
    variable cur
    set q [.mid.chatwrap.findbar.entry get]
    reset_marks
    set matches {}
    if {$q eq ""} {
        .mid.chatwrap.findbar.count configure -text ""
        return
    }
    set w [::vchat::log]
    set matches [match_all $w $q]
    if {![llength $matches]} {
        .mid.chatwrap.findbar.count configure -text "0/0"
        return
    }
    highlight $w $matches
    set cur 0
    _goto
}

# The reusable search core: every `{start end}` range of Q in widget W,
# case-insensitive. The IRC buffer's /lastlog calls this too, so find-bar
# search and buffer search cannot drift apart.
proc ::vfind::match_all {w q} {
    set out {}
    set idx 1.0
    set qlen [string length $q]
    while {1} {
        set hit [$w search -nocase -- $q $idx end]
        if {$hit eq ""} break
        set end_idx [$w index "$hit + $qlen chars"]
        lappend out [list $hit $end_idx]
        set idx $end_idx
    }
    return $out
}

# Tag every range as a hit and leave the first one focused.
proc ::vfind::highlight {w ranges} {
    if {![llength $ranges]} { return }
    $w tag configure find_hit -background $::vt::T(select_bg) -foreground $::vt::T(fg)
    $w tag configure find_cur -background $::vt::T(accent) -foreground $::vt::T(accent_fg)
    foreach m $ranges {
        lassign $m a b
        $w tag add find_hit $a $b
    }
    lassign [lindex $ranges 0] a b
    $w tag add find_cur $a $b
    $w see $a
}

proc ::vfind::_goto {} {
    variable matches
    variable cur
    if {![llength $matches]} return
    set w [::vchat::log]
    $w tag remove find_cur 1.0 end
    lassign [lindex $matches $cur] a b
    $w tag add find_cur $a $b
    $w see $a
    ::vi18n::label .mid.chatwrap.findbar.count {%1$s/%2$s} \
        [expr {$cur + 1}] [llength $matches]
}

proc ::vfind::next {} {
    variable matches
    variable cur
    if {![llength $matches]} return
    set cur [expr {($cur + 1) % [llength $matches]}]
    _goto
}

proc ::vfind::prev {} {
    variable matches
    variable cur
    if {![llength $matches]} return
    set cur [expr {($cur - 1 + [llength $matches]) % [llength $matches]}]
    _goto
}

proc ::vfind::build {} {
    ttk::frame .mid.chatwrap.findbar
    ttk::entry .mid.chatwrap.findbar.entry -width 30
    ttk::label .mid.chatwrap.findbar.count -text "" -foreground $::vt::T(fg_dim)
    ttk::button .mid.chatwrap.findbar.prev -text "↑" -width 3 -command ::vfind::prev
    ttk::button .mid.chatwrap.findbar.next -text "↓" -width 3 -command ::vfind::next
    ttk::button .mid.chatwrap.findbar.close -text "✕" -width 3 -command ::vfind::hide
    # a11y: icon buttons need names; explicit tab cycle entry→next→prev→close.
    ::va11y::name .mid.chatwrap.findbar.prev [::vi18n::mc {previous match}]
    ::va11y::name .mid.chatwrap.findbar.next [::vi18n::mc {next match}]
    ::va11y::name .mid.chatwrap.findbar.close [::vi18n::mc {close find bar}]
    foreach _b {prev next close} {
        catch { ::vcoach::attach .mid.chatwrap.findbar.$_b }
    }
    ::va11y::tab_order [list \
        .mid.chatwrap.findbar.entry \
        .mid.chatwrap.findbar.next \
        .mid.chatwrap.findbar.prev \
        .mid.chatwrap.findbar.close]
    pack .mid.chatwrap.findbar.close -side right -padx 2
    pack .mid.chatwrap.findbar.next .mid.chatwrap.findbar.prev -side right -padx 1
    pack .mid.chatwrap.findbar.count -side right -padx 6
    pack .mid.chatwrap.findbar.entry -side left -fill x -expand 1 -padx {0 6} -pady 2
    # Built unmanaged; ::vfind::show packs it (-before .mid.chatwrap.chat),
    # ::vfind::hide forgets it.

    bind .mid.chatwrap.findbar.entry <KeyRelease> {::vfind::run}
    bind .mid.chatwrap.findbar.entry <Return> {::vfind::next}
    bind .mid.chatwrap.findbar.entry <Shift-Return> {::vfind::prev}
    bind .mid.chatwrap.findbar.entry <Escape> {::vfind::hide}
}
