# veles-desktop/lib/whichkey.tcl — which-key popups, out of the box.
#
# When a prefix key arms (C-x, M-o), a popup near the status bar lists
# every continuation with its action — data comes from the authoritative
# registry (bindings.tcl), never a second copy. The breadcrumb row shows
# the armed chain ("C-x-" → "C-x s  sidebar"), exactly like emacs'
# which-key. Shown IMMEDIATELY on arm (no timeout to learn), dismissed
# on the next keypress or after 4s of abandonment.
#
# The popup is advisory: it never steals focus and never eats keys.

namespace eval ::vwhichkey {
    variable w ""
    variable after_id ""
    # `bind . <Key>` as it was before the sheet borrowed it, and whether
    # we are currently holding it. The flag is not redundant: an empty
    # saved script and "never borrowed" are different states, and
    # conflating them let show_all's own leading hide() wipe the caller's
    # binding before it had been captured.
    variable prev_root_key ""
    variable borrowed 0
}

# arm PREFIX — display-string prefix exactly as bindings.tcl spells it
# ("C-x", "M-o"). The continuation list comes from the registry.
proc ::vwhichkey::arm {prefix} {
    set conts [::vbindings::prefix_continuations $prefix]
    if {$conts eq ""} { return }
    set entries {}
    foreach row $conts {
        lassign $row key action help
        lappend entries [list $key $help]
    }
    show "$prefix-" $entries
}

# show TITLE entries — entries is a list of {key label} pairs.
proc ::vwhichkey::show {title entries} {
    variable w
    variable after_id
    hide
    set w .whichkey
    toplevel $w
    wm overrideredirect $w 1
    # Positioned AFTER the contents are built — see below.
    ttk::frame $w.f -padding 6
    # `$title` is the CALLER's string: whichkey::show is handed a title the
    # caller already resolved through the catalog, so wrapping it here
    # would be a second lookup of an already-translated string.
    ttk::label $w.f.title -text "$title" -foreground $::vt::T(accent) -font {TkDefaultFont 10 bold} ;# i18n-exempt: caller-supplied title, already catalog text
    grid $w.f.title -row 0 -column 0 -columnspan 2 -sticky w -pady {0 2}
    set row 1
    foreach pair $entries {
        lassign $pair key label
        ttk::label $w.f.k$row -text $key -foreground $::vt::T(gold) \
            -font {monospace 10 bold}
        ttk::label $w.f.l$row -text $label -foreground $::vt::T(fg) \
            -font {TkDefaultFont 10}
        grid $w.f.k$row -row $row -column 0 -sticky w -padx {0 8}
        grid $w.f.l$row -row $row -column 1 -sticky w
        incr row
    }
    pack $w.f -fill both -expand 1
    # which-key ANNOTATES the chord you just pressed, so it belongs next
    # to the focused widget — unlike the palette, whose position carries
    # no meaning and is therefore fixed. Placed here, once the labels
    # exist, so the clamp works on the real size: it used to be placed
    # first against a row-count estimate and hung off the screen edge
    # whenever the estimate was low.
    ::vplace::near_measured $w [focus -displayof .]
    raise $w
    # Auto-hide after 4s in case the user abandons the prefix.
    set after_id [after 4000 ::vwhichkey::hide]
}

proc ::vwhichkey::hide {} {
    variable w
    variable after_id
    variable prev_root_key
    variable borrowed
    if {$after_id ne ""} {
        catch { after cancel $after_id }
        set after_id ""
    }
    # Give `.` its binding back before anything else — leaving ours in
    # place would dismiss a popup that no longer exists on every key.
    # Only when we actually took it: `bind . <Key> {}` on a hide we did
    # not borrow for would delete somebody else's binding, which is
    # exactly what show_all's own leading hide() used to do.
    if {$borrowed} {
        catch { bind . <Key> $prev_root_key }
        set prev_root_key ""
        set borrowed 0
    }
    if {$w ne "" && [winfo exists $w]} {
        destroy $w
    }
    set w ""
}

# ── the whole sheet, on demand ──────────────────────────────────────
#
# USER: "Did which-key and reverse-which-key just silently disappear?"
#
# Nothing disappeared. `arm` is the only entry point, and it only fires
# from ::vkeymap::_arm_prefix — which is only bound when the active mode
# has a row with category `prefix`. Those rows live in
# ::vbindings::_emacs_prefixes, appended to emacs and vi ONLY, because
# normie was deliberately made "pure Ctrl" (commit 267d355). normie is
# the shipped default. So in the default keymap there has never been a
# prefix to arm, and the popup could not appear.
#
# Rather than putting a prefix back — that decision stands — which-key
# gains the other direction: F2 shows EVERY binding in the active mode,
# grouped by category, wrapped into as many columns as it takes. The
# armed popup keeps meaning "a chord is in progress"; this one means
# "show me what I can press".

# The chords for a mode, grouped in registry order: {category {{key help} …}}.
#
# Built from ::vbindings::describe so a new binding shows up here with no
# second edit — the same single-source rule the armed popup follows.
proc ::vwhichkey::groups {{mode ""}} {
    if {$mode eq ""} { set mode $::vkeymap::current }
    set order {}
    set rows {}
    foreach entry [::vbindings::describe $mode] {
        lassign $entry keys help category
        if {$category eq ""} { set category "other" }
        if {![dict exists $rows $category]} {
            lappend order $category
            dict set rows $category {}
        }
        dict lappend rows $category [list $keys $help]
    }
    set out {}
    foreach c $order { lappend out [list $c [dict get $rows $c]] }
    return $out
}

# Lay grouped rows into `cols` columns, balancing by total line count
# (a category header costs a line too). Returns a list of columns, each
# a list of {kind text-or-pair} items: {head CATEGORY} | {row {key help}}.
proc ::vwhichkey::_columns {groups cols} {
    set items {}
    foreach g $groups {
        lassign $g category rows
        lappend items [list head $category]
        foreach r $rows { lappend items [list row $r] }
    }
    set total [llength $items]
    if {$cols < 1} { set cols 1 }
    set per [expr {int(ceil(double($total) / $cols))}]
    if {$per < 1} { set per 1 }
    set out {}
    set cur {}
    foreach it $items {
        # Never end a column on a header — a category title with its
        # rows in the next column reads as a mistake.
        if {[llength $cur] >= $per && [lindex $it 0] ne "row"} {
            lappend out $cur
            set cur {}
        } elseif {[llength $cur] >= $per + 4} {
            lappend out $cur
            set cur {}
        }
        lappend cur $it
    }
    if {[llength $cur]} { lappend out $cur }
    return $out
}

# F2: every binding in the active mode.
proc ::vwhichkey::show_all {{mode ""}} {
    variable w
    variable after_id
    if {$mode eq ""} { set mode $::vkeymap::current }
    hide
    set w .whichkey
    toplevel $w
    wm overrideredirect $w 1
    catch { wm attributes $w -topmost 1 }
    ttk::frame $w.f -padding 8
    pack $w.f -fill both -expand 1

    ttk::label $w.f.title -foreground $::vt::T(accent) -font {TkDefaultFont 10 bold}
    ::vi18n::label $w.f.title {%s keys — any key closes} $mode
    grid $w.f.title -row 0 -column 0 -columnspan 12 -sticky w -pady {0 6}

    set groups [groups $mode]
    # Three columns is what fits a 1080-wide window without a scrollbar;
    # a single grid ran off the bottom of the screen for emacs, which has
    # forty-odd rows.
    set columns [_columns $groups 3]
    set col 0
    foreach column $columns {
        set row 1
        foreach item $column {
            lassign $item kind payload
            if {$kind eq "head"} {
                ttk::label $w.f.h${col}_$row -text $payload \
                    -foreground $::vt::T(fg_dim) -font {TkDefaultFont 9 bold}
                grid $w.f.h${col}_$row -row $row -column [expr {$col * 3}] \
                    -columnspan 2 -sticky w -pady {6 1}
            } else {
                lassign $payload keys help
                ttk::label $w.f.k${col}_$row -text $keys -foreground $::vt::T(gold) \
                    -font {monospace 9 bold}
                ttk::label $w.f.l${col}_$row -text $help -foreground $::vt::T(fg) \
                    -font {TkDefaultFont 9}
                grid $w.f.k${col}_$row -row $row -column [expr {$col * 3}] \
                    -sticky w -padx {0 8}
                grid $w.f.l${col}_$row -row $row -column [expr {$col * 3 + 1}] -sticky w
            }
            incr row
        }
        # A gutter between columns.
        if {$col < [llength $columns] - 1} {
            grid columnconfigure $w.f [expr {$col * 3 + 2}] -minsize 24
        }
        incr col
    }

    # The sheet is about the whole window, not about whatever has focus,
    # so it is centred like the palette rather than anchored like the
    # armed popup. ::vplace clamps to the display either way.
    update idletasks
    ::vplace::centered $w [winfo reqwidth $w] [winfo reqheight $w]
    raise $w
    # Dismiss on the next keystroke or click. Bound on `.`, NOT on `all`:
    # `all <KeyPress>` is keymap.tcl's prefix dispatcher slot, and `<Key>`
    # is the same event, so binding there would silently break C-x/M-o in
    # emacs mode. The previous binding on `.` is saved and restored, so
    # showing the sheet twice cannot stack handlers.
    variable prev_root_key
    variable borrowed
    if {!$borrowed} {
        set prev_root_key [bind . <Key>]
        set borrowed 1
    }
    bind . <Key> { ::vwhichkey::_dismiss_soon }
    bind $w <Key> { ::vwhichkey::hide ; break }
    bind $w <Button> { ::vwhichkey::hide ; break }
    set after_id [after 20000 ::vwhichkey::hide]
    return $w
}

# Dismiss WITHOUT eating the keystroke: no `break`, so whatever the key
# was actually for still happens. Deferred to idle because hiding a
# window mid-dispatch can disturb the focus the same key is about to use.
proc ::vwhichkey::_dismiss_soon {} {
    variable w
    if {$w ne "" && [winfo exists $w]} { after idle ::vwhichkey::hide }
}
