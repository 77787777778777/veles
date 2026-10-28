# veles-desktop/lib/ctxmenu.tcl — right-click menus, in one place.
#
# The app had exactly ONE context menu in 8k lines of Tcl (the
# transcript's), so right-clicking anything else did nothing at all.
#
# `::vctx::attach` takes a widget and a list of entries and wires the
# Button-3 handling, including the two things every hand-rolled version
# forgets:
#
#   - the click POSITION. Menu commands almost always need to know what
#     was clicked, not just that a click happened. The widget-relative
#     x/y and the item under the pointer are captured before the menu
#     pops and are readable via ::vctx::x / ::vctx::y / ::vctx::index.
#
#   - selecting under the cursor first. Right-clicking a row in a list
#     should act on THAT row, not on whatever happened to be selected
#     beforehand — which is how "delete" ends up deleting the wrong
#     thing.
#
# Entry forms:
#   {label command}     a menu item; the label is an English key that
#                       mc() translates when the menu opens
#   {-}                 a separator
#   {label command ?enabled_cmd?}
#                       enabled_cmd runs at popup time; a false result
#                       greys the item out.

namespace eval ::vctx {
    variable x 0        ;# widget-relative click position
    variable y 0
    variable index ""   ;# list/tree item under the pointer, "" if none
    variable widget ""  ;# the widget that was clicked
}

# Attach a context menu to $w. Re-attaching replaces the previous menu.
proc ::vctx::attach {w entries} {
    if {![winfo exists $w]} {
        # Attaching before the widget is built is a programming error,
        # not a runtime condition — and silently returning hides it.
        # It caught me immediately: the composer menu was attached above
        # the `text` that creates the composer, so right-click did
        # nothing and nothing said why.
        catch { ::vstatus::set_msg [format [::vi18n::mc {ctxmenu: no widget %s (attached too early?)}] $w] }
        return ""
    }
    set m $w.vctxmenu
    catch { destroy $m }
    menu $m -tearoff 0
    # Store the spec on the menu so the popup handler can re-evaluate
    # the enable predicates each time it opens.
    set ::vctx::spec($m) $entries
    bind $w <Button-3> [list ::vctx::_popup $w $m %x %y %X %Y]
    # On a phone a long press on text selects before the menu opens
    # (lib/android.tcl).
    if {[winfo class $w] eq "Text" && [llength [info commands ::vandroid::hold_bind]]} {
        ::vandroid::hold_bind $w $m
    }
    # Keyboard equivalent: the Menu key, and Shift-F10 for keyboards
    # without one. A right-click-only affordance is unreachable without
    # a mouse.
    bind $w <Key-Menu>    [list ::vctx::_popup_at_focus $w $m]
    bind $w <Shift-F10>   [list ::vctx::_popup_at_focus $w $m]
    return $m
}

proc ::vctx::_popup {w m wx wy sx sy} {
    variable x
    variable y
    variable index
    variable widget
    set x $wx
    set y $wy
    set widget $w
    set index [_item_at $w $wx $wy]
    # Right-clicking a row acts on THAT row.
    _select_at $w $index
    _rebuild $m
    tk_popup $m $sx $sy
}

# Menu-key / Shift-F10: pop at the widget's current selection instead of
# at the pointer.
proc ::vctx::_popup_at_focus {w m} {
    variable x
    variable y
    variable index
    variable widget
    set widget $w
    set index [_current_item $w]
    set x 0
    set y 0
    _rebuild $m
    tk_popup $m [expr {[winfo rootx $w] + 20}] [expr {[winfo rooty $w] + 20}]
}

proc ::vctx::_rebuild {m} {
    if {![info exists ::vctx::spec($m)]} { return }
    $m delete 0 end
    # Selected text in a read-only view is copied from its menu, which
    # did not say so: the transcript's offered only the whole message
    # (USER 2026-10-01: "select arbitrary parts of the transcript and
    # copy"). Editable text has its own Copy, and so may a view.
    set w [winfo parent $m]
    if {[winfo class $w] eq "Text"
            && [llength [info commands ::vtext::is_readonly]] && [::vtext::is_readonly $w]
            && [llength [$w tag ranges sel]]
            && [lsearch -index 0 -exact $::vctx::spec($m) Copy] < 0} {
        $m add command -label [::vi18n::mc {Copy}] \
            -command [list ::vctx::copy_selection $w]
        $m add separator
    }
    foreach e $::vctx::spec($m) {
        if {[lindex $e 0] eq "-"} {
            $m add separator
            continue
        }
        lassign $e label command enabled
        set state normal
        if {$enabled ne ""} {
            set okp 0
            catch { set okp [expr {[uplevel #0 $enabled] ? 1 : 0}] }
            if {!$okp} { set state disabled }
        }
        # The chord goes in -accelerator, where Tk right-aligns it —
        # a menu that names its own shortcut is how every other app
        # teaches its keyboard, and the string comes from the registry
        # via the command itself, so it cannot drift.
        set accel ""
        catch { set accel [::vkeymap::key_for $command] }
        # Clicking a menu item is a pointer action, so it coaches too
        # (for anything the accelerator column could not show, and to
        # count against the same per-command tally).
        # The label is a key: a menu in English on a German phone is the
        # one thing the touch help could not explain away.
        $m add command -label [::vi18n::mc $label] -state $state -accelerator $accel \
            -command [list ::vctx::_run $m $command]
    }
}

# The selection of read-only text, onto the clipboard.
proc ::vctx::copy_selection {w} {
    if {[catch { $w get sel.first sel.last } text] || $text eq ""} { return 0 }
    clipboard clear
    clipboard append $text
    set shown [string map {\n " "} $text]
    if {[string length $shown] > 40} { set shown "[string range $shown 0 38]…" }
    catch { ::vstatus::set_msg [format [::vi18n::mc {copied: %s}] $shown] }
    return 1
}

# Run a menu entry's command and let the coach say which key does it.
proc ::vctx::_run {m command} {
    variable widget
    set anchor $widget
    if {$anchor eq "" || ![winfo exists $anchor]} { set anchor . }
    set code [catch { uplevel #0 $command } result opts]
    catch { ::vcoach::fired_by_pointer $anchor $command }
    if {$code} { return -options $opts $result }
    return $result
}

# The item under a widget-relative point, for the widget kinds we use.
proc ::vctx::_item_at {w wx wy} {
    switch -- [winfo class $w] {
        Listbox  { return [$w index @$wx,$wy] }
        Treeview { return [$w identify row $wx $wy] }
        Text     { return [$w index @$wx,$wy] }
    }
    return ""
}

proc ::vctx::_current_item {w} {
    switch -- [winfo class $w] {
        Listbox  { return [lindex [$w curselection] 0] }
        Treeview { return [lindex [$w selection] 0] }
        Text     { return [$w index insert] }
    }
    return ""
}

proc ::vctx::_select_at {w item} {
    if {$item eq ""} { return }
    switch -- [winfo class $w] {
        Listbox {
            $w selection clear 0 end
            $w selection set $item
            $w activate $item
        }
        Treeview {
            $w selection set [list $item]
            $w focus $item
        }
    }
}
