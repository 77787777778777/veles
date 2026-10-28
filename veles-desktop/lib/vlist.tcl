# veles-desktop/lib/vlist.tcl — ONE selection-list behaviour.
#
# Every filter-entry + listbox picker in this app (command palette,
# completion popup, skills overlay, theme picker, keymap picker) needs
# the same six things: type to filter, ↑↓ to walk, wrap at the ends,
# scroll the cursor into view, click to select, Enter to run. Each one
# had grown its own copy, and they did not agree.
#
# THE BUG THIS EXISTS TO KILL
#
# Arrow keys fire KeyPress and then KeyRelease. Pickers bound the filter
# to <KeyRelease> and the cursor to <Down>, so every ↓ moved the cursor
# (KeyPress) and then rebuilt the list from scratch (KeyRelease), which
# reset the cursor to 0. The list "always scrolls back to the first
# item" when you try to walk it.
#
# palette.tcl worked around it by remembering the last query and only
# resetting when the query CHANGED — correct, but bolted on after the
# fact and never applied to the other four pickers.
#
# The root fix is here instead: `_on_keyrelease` ignores navigation
# keysyms outright, so a refill can only ever be caused by a key that
# actually edits the query. Cursor position then survives by
# construction rather than by remembering things.
#
# USE
#
#   ::vlist::manage $entry $list \
#       -refill  {mycmd}          ;# repopulate $list from the query
#       -run     {mycmd}          ;# Enter / double-click / click
#       -close   {mycmd}          ;# Escape (optional)
#
# The -refill command owns WHAT goes in the list; ::vlist owns the
# cursor. Call ::vlist::keep or ::vlist::reset from inside -refill to
# say whether the new contents are a continuation of the old ones.

namespace eval ::vlist {
    # per-listbox state: cursor index and the callbacks
    variable cursor
    variable opts
    array set cursor {}
    array set opts {}
}

# Keys that move the cursor or close the widget. These must NEVER cause
# a refill: they do not change the query, and refilling on them is what
# threw the cursor away.
proc ::vlist::is_nav_keysym {keysym} {
    return [expr {$keysym in {
        Up Down Left Right Prior Next Home End
        Return KP_Enter Escape Tab ISO_Left_Tab
        Shift_L Shift_R Control_L Control_R Alt_L Alt_R
        Meta_L Meta_R Super_L Super_R Caps_Lock
    }}]
}

# Wire an entry + listbox pair. Idempotent per listbox.
proc ::vlist::manage {entry list args} {
    variable cursor
    variable opts
    set cursor($list) 0
    set opts($list) $args

    bind $entry <KeyRelease> [list ::vlist::_on_keyrelease $list %K]
    bind $entry <Down>       [list ::vlist::_move $list 1]
    bind $entry <Up>         [list ::vlist::_move $list -1]
    bind $entry <Next>       [list ::vlist::_move $list 10]
    bind $entry <Prior>      [list ::vlist::_move $list -10]
    bind $entry <Return>     [list ::vlist::_run $list]
    bind $entry <Escape>     [list ::vlist::_close $list]

    # Clicking the list moves the cursor there and runs. ButtonRelease,
    # not ButtonPress: the listbox class binding settles the selection
    # on press, so acting on press reads the PREVIOUS selection.
    # `; break` is load-bearing on every one of these.
    #
    # These handlers RUN the selected action, which normally destroys
    # the picker — and Tk's own Listbox class bindings fire AFTER a
    # widget binding. `bind Listbox <ButtonRelease-1>` ends with
    # `%W activate @%x,%y`, so without `break` it lands on a widget that
    # no longer exists:
    #
    #     invalid command name ".palette.list"
    #         while executing ".palette.list activate @60,226"
    #
    # Note that stripping bindtags inside the handler does NOT help:
    # Tk resolves the whole chain when dispatch starts, so a mid-dispatch
    # change is ignored. `break` is what actually stops it.
    bind $list <ButtonRelease-1> "[list ::vlist::_click $list]; break"
    bind $list <Double-Button-1> "[list ::vlist::_run $list]; break"
    bind $list <Return>          "[list ::vlist::_run $list]; break"
    bind $list <Escape>          "[list ::vlist::_close $list]; break"
    bind $list <Destroy>         [list ::vlist::_forget $list]
}

proc ::vlist::_forget {list} {
    variable cursor
    variable opts
    array unset cursor $list
    array unset opts $list
}

proc ::vlist::_opt {list name} {
    variable opts
    if {![info exists opts($list)]} { return "" }
    set a $opts($list)
    set i [lsearch -exact $a $name]
    if {$i < 0} { return "" }
    return [lindex $a [expr {$i + 1}]]
}

proc ::vlist::_on_keyrelease {list keysym} {
    # THE fix: navigation never refills.
    if {[is_nav_keysym $keysym]} { return }
    if {![winfo exists $list]} { return }
    set cmd [_opt $list -refill]
    if {$cmd ne ""} { uplevel #0 $cmd }
}

# Move the cursor, wrapping at both ends, and scroll it into view.
proc ::vlist::_move {list delta} {
    variable cursor
    if {![winfo exists $list]} { return }
    set n [$list size]
    if {$n == 0} { return }
    set i [expr {([get $list] + $delta) % $n}]
    if {$i < 0} { incr i $n }
    set_cursor $list $i
}

proc ::vlist::_click {list} {
    variable cursor
    set sel [$list curselection]
    if {$sel eq ""} { return }
    set cursor($list) [lindex $sel 0]
    _run $list
}

proc ::vlist::_run {list} {
    if {![winfo exists $list]} { return }
    set sel [$list curselection]
    if {$sel ne ""} {
        variable cursor
        set cursor($list) [lindex $sel 0]
    }
    set cmd [_opt $list -run]
    if {$cmd ne ""} { uplevel #0 $cmd }
}

proc ::vlist::_close {list} {
    set cmd [_opt $list -close]
    if {$cmd ne ""} { uplevel #0 $cmd }
}

# ── cursor API, for use from a -refill callback ─────────────────────

proc ::vlist::get {list} {
    variable cursor
    if {![info exists cursor($list)]} { return 0 }
    return $cursor($list)
}

proc ::vlist::set_cursor {list i} {
    variable cursor
    if {![winfo exists $list]} { return }
    set n [$list size]
    if {$n == 0} { set cursor($list) 0 ; return }
    if {$i < 0} { set i 0 }
    if {$i >= $n} { set i [expr {$n - 1}] }
    set cursor($list) $i
    $list selection clear 0 end
    $list selection set $i
    $list activate $i
    $list see $i
}

# The contents changed but represent the SAME query — keep the cursor
# where it was, clamped to the new length.
proc ::vlist::keep {list} {
    set_cursor $list [get $list]
}

# The query changed — start from the top.
proc ::vlist::reset {list} {
    variable cursor
    set cursor($list) 0
    set_cursor $list 0
}
