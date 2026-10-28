# veles-desktop/lib/cmdtree.tcl — every command, one tap at a time.
#
# USER 2026-10-01: "I just want one button to do that plus literally
# every other command so that it's all reachable from touching
# buttons", and "then which-key tells you what keybinding to use in the
# future if you're on a real keyboard".
#
# The ⋮ at the end of the tab strip opens this on a laptop and on a
# phone alike: the categories, then a category's commands, each run by a
# tap or a click. On a keyboard every row shows its key the way
# which-key does, and running a row teaches that key for next time
# (lib/coach.tcl) until it is learned. A phone shows no chords.
#
# NO LIST OF ITS OWN. The rows come from what already exists, joined on
# the command string as every hint surface is (lib/bindings.tcl):
#   - the palette's actions (lib/palette.tcl), which name a category;
#   - every mode's key bindings and prefix chords, so a command only a
#     key reached (delete the session, dock the sidebar, the Dev
#     editor's C-c chords) has a row too, whatever mode is active;
#   - the tabs there are;
#   - the engine's slash commands and the desktop's own (lib/cmds.tcl).
#
# The rows are lines of a read-only text, not a button each: on a phone
# a finger's drag scrolls read-only text (lib/android.tcl), and a slow
# drag across a column of buttons ends on one and runs it. A tap runs a
# row only if it did not move (::vtext::click).

namespace eval ::vcmdtree {
    variable w .cmdtree
    variable level ""        ;# "" = the top; else a category id
    variable shown {}        ;# the rows on the screen, one per line
    variable cur -1          ;# the keyboard's row
    variable prev_focus ""
}

# The categories, in order. Ids are stable; the heading is the catalog's.
proc ::vcmdtree::categories {} {
    return {sessions view panes agent dev settings help slash}
}

proc ::vcmdtree::heading {id} {
    switch -- $id {
        sessions { return [::vi18n::mc {Sessions}] }
        view     { return [::vi18n::mc {View}] }
        panes    { return [::vi18n::mc {Panes & tabs}] }
        agent    { return [::vi18n::mc {Agent & engine}] }
        dev      { return [::vi18n::mc {Dev}] }
        settings { return [::vi18n::mc {Settings}] }
        help     { return [::vi18n::mc {Help & keys}] }
        slash    { return [::vi18n::mc {Slash commands}] }
    }
    return $id
}

# Where a registry row goes: its own category, read as a place in the
# tree. Composer and IRC rows are editing keys, not commands; the tabs
# are listed by name (rows_tabs).
proc ::vcmdtree::_registry_cat {category prefix} {
    if {$category eq "prefix"} {
        return [expr {$prefix eq "M-o" ? "agent" : "panes"}]
    }
    switch -- $category {
        session    { return sessions }
        view       { return view }
        panes      { return panes }
        navigation { return panes }
        manage     { return agent }
        dev        { return dev }
        general    { return settings }
    }
    return ""
}

# A registry help text ("delete session") as a row's label.
proc ::vcmdtree::_capital {s} {
    return [string toupper $s 0 0]
}

proc ::vcmdtree::_row {cat label action args} {
    return [dict create cat $cat label $label action $action {*}$args]
}

# Every leaf row, in category order of appearance. A command is listed
# once per category; the palette's label wins over the registry's.
proc ::vcmdtree::rows {} {
    set out {}
    set seen {}
    set have {}
    foreach a [::vpalette::actions] {
        lassign $a label action cat
        if {$cat eq ""} { set cat settings }
        set n [::vbindings::_norm $action]
        if {[dict exists $seen "$cat $n"]} { continue }
        dict set seen "$cat $n" 1
        dict set have $n 1
        lappend out [_row $cat [::vi18n::mc $label] $action]
    }
    # The tabs by name, ahead of the registry's "tab 1".."tab 9".
    set i 0
    foreach id $::vtabs::order {
        incr i
        set action [list ::vtabs::select_index $i]
        dict set have [::vbindings::_norm $action] 1
        lappend out [_row panes [_capital [format [::vi18n::mc {switch to the %s tab}] [::vtabs::title $id]]] $action]
    }
    if {[llength [info commands ::vandroid::on]] && [::vandroid::on]} {
        dict set have ::vandroid::drawer_toggle 1
        lappend out [_row sessions [_capital [::vi18n::mc {show or hide the session list}]] ::vandroid::drawer_toggle]
    }
    foreach mode {normie emacs vi} {
        foreach r [::vbindings::table $mode] {
            lassign $r keys target action help category
            if {$category eq "prefix"} {
                foreach p [::vbindings::prefix_continuations $keys] {
                    lassign $p k pa ph
                    set n [::vbindings::_norm $pa]
                    if {$n eq "" || [dict exists $have $n]} { continue }
                    dict set have $n 1
                    lappend out [_row [_registry_cat prefix $keys] [_capital [::vi18n::mc $ph]] $pa]
                }
                continue
            }
            set n [::vbindings::_norm $action]
            if {$n eq "" || [dict exists $have $n]} { continue }
            if {$category eq "tabs"} { continue }
            if {$n eq "::vpalette::open"} { continue }
            set cat [_registry_cat $category ""]
            if {$cat eq ""} { continue }
            dict set have $n 1
            lappend out [_row $cat [_capital [::vi18n::mc $help]] $action]
        }
    }
    foreach r [::vcmds::all] {
        lassign $r name args desc cap
        # Spelled as the registry spells it ({::vchat::slash_command
        # "/agents"}), so the join finds a slash command's chord.
        set action "::vchat::slash_command \"$name\""
        # One that needs its argument is written into the composer, not
        # run bare.
        if {[string first "<" $args] >= 0} {
            set action [list ::vcmdtree::compose "$name "]
        }
        set label [::vi18n::mc $desc]
        if {$args ne ""} { set label "$args — $label" }
        lappend out [_row slash $label $action word $name]
    }
    return $out
}

# What a row shows in the key column: a slash command's name, the chord
# on a keyboard, nothing on a phone.
proc ::vcmdtree::_key {row} {
    if {[dict exists $row word]} { return [dict get $row word] }
    if {[dict get $row kind] ne "leaf"} { return "" }
    if {[_phone]} { return "" }
    return [::vkeymap::key_for [dict get $row action]]
}

proc ::vcmdtree::_phone {} {
    expr {[llength [info commands ::vandroid::on]] && [::vandroid::on]}
}

# The rows of one level: the top's categories, or a category's commands.
proc ::vcmdtree::level_rows {level} {
    set leaves {}
    foreach r [rows] { dict set r kind leaf ; lappend leaves $r }
    if {$level ne ""} {
        set out {}
        foreach r $leaves {
            if {[dict get $r cat] eq $level} { lappend out $r }
        }
        return $out
    }
    set out [list [_row top [::vi18n::mc {Search commands and sessions…}] ::vpalette::open kind leaf]]
    foreach id [categories] {
        set n 0
        foreach r $leaves { if {[dict get $r cat] eq $id} { incr n } }
        if {$n == 0} { continue }
        lappend out [_row top [heading $id] "" kind cat to $id count $n]
    }
    foreach r $leaves {
        if {[dict get $r cat] eq "top"} { lappend out $r }
    }
    return $out
}

# ── the window ───────────────────────────────────────────────────────

proc ::vcmdtree::open {{level ""}} {
    variable w
    variable prev_focus
    if {![winfo exists $w]} {
        set prev_focus [focus -displayof .]
        if {$prev_focus eq "" || $prev_focus eq "."} {
            set prev_focus .mid.chatwrap.chat.input
        }
        _build
    }
    show $level
    raise $w
    focus $w.body
    return $w
}

proc ::vcmdtree::_build {} {
    variable w
    toplevel $w
    wm title $w [::vi18n::mc {All commands}]
    ::vplace::centered $w 560 620
    # The phone's Back key and the window's close box go up a level
    # before they close it.
    wm protocol $w WM_DELETE_WINDOW ::vcmdtree::back
    ttk::frame $w.head
    ttk::button $w.head.up -text "‹ [::vi18n::mc {Back}]" -width 0 -command ::vcmdtree::back
    ttk::label $w.head.title -foreground $::vt::T(accent) -font {TkDefaultFont 11 bold}
    pack $w.head.title -side left -padx 8
    set phone [_phone]
    set gap [expr {$phone ? [::vandroid::dp 9] : 3}]
    text $w.body -wrap word -cursor hand2 -relief flat -bd 0 -highlightthickness 0 \
        -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -font TkDefaultFont -spacing1 $gap -spacing3 $gap -padx 6 \
        -yscrollcommand [list $w.sb set]
    ttk::scrollbar $w.sb -orient vertical -command [list $w.body yview]
    # The fixed font at the labels' size: on a phone AndroWish's fixed
    # font is smaller than its default one, and a slash command's name
    # was the smallest thing on its row.
    $w.body tag configure key -foreground $::vt::T(gold) \
        -font [list [font actual TkFixedFont -family] [font actual TkDefaultFont -size]]
    $w.body tag configure cat -foreground $::vt::T(accent) -font {TkDefaultFont 10 bold}
    $w.body tag configure count -foreground $::vt::T(fg_dim)
    $w.body tag configure cur -background $::vt::T(select_bg)
    ::vtext::readonly $w.body
    # A list, not a page: no caret blinking in it.
    $w.body configure -insertwidth 0
    ttk::label $w.hint -foreground $::vt::T(fg_dim) \
        -text [::vi18n::mc {↑↓ select · Enter run · Esc close}]
    pack $w.head -side top -fill x -pady {6 2}
    if {!$phone} { pack $w.hint -side bottom -fill x -padx 8 -pady {0 6} }
    pack $w.sb -side right -fill y -pady 2
    pack $w.body -side left -fill both -expand 1 -padx {8 0} -pady 2
    set t $w.body
    foreach {ev script} {
        <Up>        {::vcmdtree::move -1}
        <Down>      {::vcmdtree::move 1}
        <Home>      {::vcmdtree::move_to 0}
        <End>       {::vcmdtree::move_to end}
        <Return>    {::vcmdtree::run_cur}
        <KP_Enter>  {::vcmdtree::run_cur}
        <space>     {::vcmdtree::run_cur}
        <Right>     {::vcmdtree::run_cur 1}
        <Left>      {::vcmdtree::back 1}
        <BackSpace> {::vcmdtree::back 1}
        <Escape>    {::vcmdtree::close}
    } {
        bind $t $ev "$script ; break"
    }
    bind $t <Configure> ::vcmdtree::_columns
}

# Draw a level. The rows are lines 1..n of the text, in order.
proc ::vcmdtree::show {lv} {
    variable w
    variable level
    variable shown
    variable cur
    set level $lv
    set t $w.body
    if {$lv eq ""} {
        $w.head.title configure -text [::vi18n::mc {All commands}]
        pack forget $w.head.up
    } else {
        $w.head.title configure -text "[::vi18n::mc {All commands}] › [heading $lv]"
        pack $w.head.up -side left -before $w.head.title -padx {8 0}
    }
    set shown [level_rows $lv]
    foreach tag [$t tag names] {
        if {[string match row* $tag]} { $t tag delete $tag }
    }
    $t delete 1.0 end
    set i 0
    foreach r $shown {
        if {$i > 0} { $t insert end "\n" }
        set kind [dict get $r kind]
        if {$kind eq "cat"} {
            $t insert end "›\t" [list key row$i] [dict get $r label] [list cat row$i] \
                "  [dict get $r count]" [list count row$i]
        } else {
            $t insert end "[_key $r]\t" [list key row$i] [dict get $r label] row$i
        }
        ::vtext::click $t row$i [list ::vcmdtree::run $i]
        if {![_phone]} {
            $t tag bind row$i <Enter> [list ::vcmdtree::move_to $i]
        }
        incr i
    }
    _columns
    set cur -1
    move_to 0
    $t yview moveto 0
}

# The key column is as wide as its widest entry, and a row that wraps
# carries on under its label, not under its key.
proc ::vcmdtree::_columns {} {
    variable w
    variable shown
    if {![winfo exists $w.body]} { return }
    set t $w.body
    set kf [$t tag cget key -font]
    set wide [font measure $kf "›"]
    foreach r $shown {
        set k [_key $r]
        if {$k eq ""} { continue }
        set m [font measure $kf $k]
        if {$m > $wide} { set wide $m }
    }
    # Never more than a third of the window: a long slash command's
    # arguments wrap, the labels keep their room.
    set third [expr {max(60, [winfo width $t] / 3)}]
    if {$wide > $third} { set wide $third }
    set col [expr {$wide + [font measure TkDefaultFont "  "]}]
    $t configure -tabs [list $col left]
    for {set i 0} {$i < [llength $shown]} {incr i} {
        $t tag configure row$i -lmargin2 $col
    }
}

# ── the keyboard's row ───────────────────────────────────────────────

proc ::vcmdtree::move_to {i} {
    variable w
    variable shown
    variable cur
    set n [llength $shown]
    if {$n == 0} { return }
    if {$i eq "end"} { set i [expr {$n - 1}] }
    if {$i < 0} { set i 0 }
    if {$i >= $n} { set i [expr {$n - 1}] }
    set cur $i
    set t $w.body
    $t tag remove cur 1.0 end
    set line [expr {$i + 1}]
    $t tag add cur $line.0 "$line.0 lineend +1c"
    $t see $line.0
}

proc ::vcmdtree::move {delta} {
    variable cur
    variable shown
    set n [llength $shown]
    if {$n == 0} { return }
    move_to [expr {(($cur + $delta) % $n + $n) % $n}]
}

# Enter, Space or → on the keyboard's row. → only descends.
proc ::vcmdtree::run_cur {{descend_only 0}} {
    variable cur
    variable shown
    if {$cur < 0 || $cur >= [llength $shown]} { return }
    if {$descend_only && [dict get [lindex $shown $cur] kind] ne "cat"} { return }
    run $cur
}

# ── running ──────────────────────────────────────────────────────────

# Row i of the level on the screen: a category opens, a command runs
# with the tree closed and the focus back where it was, then the coach
# names its key if it has one that is not learned yet.
proc ::vcmdtree::run {i} {
    variable shown
    if {$i < 0 || $i >= [llength $shown]} { return }
    set r [lindex $shown $i]
    if {[dict get $r kind] eq "cat"} {
        show [dict get $r to]
        return
    }
    set action [dict get $r action]
    close
    if {[catch { uplevel #0 $action } err]} {
        catch { ::vstatus::set_msg $err }
        return
    }
    if {![dict exists $r word]} {
        catch { ::vcoach::note_mouse [_anchor] $action [dict get $r label] }
    }
}

# What the coach's note sits by: the ⋮ that opened the tree.
proc ::vcmdtree::_anchor {} {
    set b $::vtabs::bar.all
    if {[winfo exists $b]} { return $b }
    return .
}

# Up a level, or close at the top. `key` is the keyboard's ← and
# Backspace, which do nothing at the top.
proc ::vcmdtree::back {{key 0}} {
    variable level
    if {$level ne ""} {
        show ""
        return up
    }
    if {$key} { return "" }
    close
    return closed
}

proc ::vcmdtree::close {} {
    variable w
    variable prev_focus
    variable level ""
    if {[winfo exists $w]} { destroy $w }
    if {$prev_focus ne "" && [winfo exists $prev_focus]} {
        catch { focus $prev_focus }
    }
}

# A slash command that needs its argument: written into the composer,
# which takes the focus (on a phone, a tap there brings the keyboard).
proc ::vcmdtree::compose {text} {
    set in .mid.chatwrap.chat.input
    catch { ::vtabs::select chat }
    if {![winfo exists $in]} { return 0 }
    $in delete 1.0 end
    $in insert end $text
    $in mark set insert end
    focus $in
    # A phone raises its keyboard on a tap, and this was not one: the
    # argument could not be typed until the composer was tapped too.
    if {[_phone]} { catch { sdltk textinput 1 } }
    return 1
}
