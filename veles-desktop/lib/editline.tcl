# veles-desktop/lib/editline.tcl — readline-like composer behavior:
# editing keys, input history, completion, and the minibuffer helper.
#
# Emacs-on-a-rust-core doctrine: the composer is the minibuffer. Text
# widget class bindings already give emacs line editing (C-a/C-e/C-b/
# C-f/M-b/M-f/C-k/C-y/C-w); this module adds what the class lacks:
#   - input history    M-p / M-n   (persisted in desktop_state.json)
#   - completion       Tab         (slash commands, then file paths)
#   - undo             C-/  C-_    (snapshot per edit burst)
#   - kill-whole-line  C-u
# and registers the abort/help conventions on every mode:
#   C-g  quit whatever is active (minibuffer > palette > findbar > ask)
#   C-h  help (emacs), F1 help (all modes)
#
# Overlays get keyboard-only filters via ::vedit::minibuffer — every
# pane is fully operable without touching the mouse.

namespace eval ::vedit {
    variable hist {}
    variable hist_pos -1
    variable hist_stash ""
    variable undo_snap {}
    variable undo_last 0
    variable comp_state {}
}

# ── history (persisted) ──────────────────────────────────────────────

proc ::vedit::hist_load {} {
    variable hist
    set st [::vstate::all]
    if {[dict exists $st input_history]} {
        set hist [dict get $st input_history]
    }
    # An older build encoded this list as a JSON object and shredded the
    # prompts into key/value pairs. ::vstate salvages what survived;
    # saying so beats letting the user wonder where their history went.
    foreach r [::vstate::repairs] {
        catch { ::vstatus::set_msg [format [::vi18n::mc {repaired desktop state — %s}] $r] }
    }
}

proc ::vedit::hist_push {line} {
    variable hist
    variable hist_pos -1
    variable hist_stash ""
    set line [string trim $line]
    if {$line eq ""} { return }
    if {[lindex $hist end] eq $line} { return }
    lappend hist $line
    if {[llength $hist] > 200} { set hist [lrange $hist end-199 end] }
    _hist_save
}

proc ::vedit::_hist_save {} {
    variable hist
    if {![::vstate::put input_history $hist]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {history not saved: %s}] $::vstate::error] }
    }
}

# Older entries FIRST for M-p walking (newest last in the list).
proc ::vedit::hist_prev {} {
    variable hist
    variable hist_pos
    variable hist_stash
    if {[llength $hist] == 0} { return }
    if {$hist_pos == -1} {
        set hist_stash [.mid.chatwrap.chat.input get 1.0 end-1c]
        set hist_pos [llength $hist]
    }
    if {$hist_pos > 0} { incr hist_pos -1 }
    _hist_show
}

proc ::vedit::hist_next {} {
    variable hist
    variable hist_pos
    variable hist_stash
    if {$hist_pos == -1} { return }
    incr hist_pos
    if {$hist_pos >= [llength $hist]} {
        set hist_pos -1
        _set_text $hist_stash
        return
    }
    _hist_show
}

proc ::vedit::_hist_show {} {
    variable hist
    variable hist_pos
    _set_text [lindex $hist $hist_pos]
}

proc ::vedit::_set_text {text} {
    .mid.chatwrap.chat.input delete 1.0 end
    .mid.chatwrap.chat.input insert end $text
    .mid.chatwrap.chat.input mark set insert end
}

# ── undo (simple: snapshot at edit-burst boundaries) ────────────────

proc ::vedit::_maybe_snap {} {
    variable undo_snap
    variable undo_last
    set now [clock milliseconds]
    if {$now - $undo_last > 800} {
        set undo_snap [list [.mid.chatwrap.chat.input get 1.0 end-1c] [.mid.chatwrap.chat.input index insert]]
    }
    set undo_last $now
}

proc ::vedit::undo {} {
    variable undo_snap
    if {$undo_snap eq ""} { ::vstatus::set_msg [::vi18n::mc {nothing to undo}]; return }
    lassign $undo_snap text pos
    .mid.chatwrap.chat.input delete 1.0 end
    .mid.chatwrap.chat.input insert end $text
    catch { .mid.chatwrap.chat.input mark set insert $pos }
    set undo_snap {}
    ::vstatus::set_msg [::vi18n::mc {undone}]
}

# A paste/cut must land a SNAPSHOT before it mutates: _maybe_snap
# fires on KeyPress, and a menu/C-v paste types no key — so C-/ after
# a paste resurrected a pre-TYPING state or said "nothing to undo"
# (RM3 #11: "doesn't seem to be able to undo pasting"). Widget-level
# bindings run BEFORE the Text class's tk_textPaste/tk_textCut; the
# explicit call plus `return -code break` keeps exactly one paste/cut,
# with the snapshot on the right side of it.
proc ::vedit::_snap_then {w op} {
    variable undo_snap
    variable undo_last
    set undo_snap [list [$w get 1.0 end-1c] [$w index insert]]
    set undo_last [clock milliseconds]
    if {$op eq "paste"} {
        tk_textPaste $w
    } else {
        tk_textCut $w
    }
    return -code break
}

# ── completion (helm-grade popup lives in lib/complete.tcl) ─────────
# Tab opens / cycles the completion popup; while it's open, Up/Down
# navigate and Enter accepts (those keys are guarded at their binding
# sites). Typing refilters live via the KeyRelease hook below.

proc ::vedit::complete {} {
    if {[::vcomp::open]} {
        ::vcomp::move 1
        return
    }
    ::vcomp::trigger
}

# ── abort ladder (C-g / Escape) ──────────────────────────────────────
# Emacs discipline: C-g quits whatever is active. Order matters — the
# most specific surface wins.

proc ::vedit::abort {} {
    set fw [focus]
    # 0. Deactivate the mark, always and first.
    #
    # In emacs C-g deactivates the mark whatever else it does, and this
    # ladder never touched it — so the one binding the user reaches for to
    # cancel a selection was the one thing that could not cancel it. The
    # widget-level C-g in emacs.tcl does call deactivate_mark, but it only
    # exists on the three editable targets and the Text class, and it is
    # unreachable whenever `bind all <Control-g>` has been wiped (see the
    # registry row in bindings.tcl).
    if {$fw ne "" && [winfo exists $fw] && [winfo class $fw] eq "Text"} {
        catch { ::vemacs::deactivate_mark $fw }
    }
    # 1. An open minibuffer/overlay filter: return to its list or close.
    if {$fw ne "" && [string match "*.filter" $fw]} {
        set parent [winfo parent $fw]
        set listw "$parent.list"
        if {[winfo exists $listw]} {
            focus $listw
            ::vstatus::set_msg ""
            return
        }
    }
    # 2. Ask dialog open (approval/clarify): C-g = deny.
    foreach w [winfo children .] {
        if {[string match ".ask*" $w]} {
            catch { ::vask::cancel $w }
            return
        }
    }
    # 3. Palette: close.
    if {[winfo exists .palette]} { destroy .palette; return }
    # 4. Theme/keymap pickers and overlays: close the focused toplevel.
    set top [winfo toplevel $fw]
    if {$top ne "."} { destroy $top; return }
    # 5. Find bar visible: hide it.
    if {[winfo ismapped .mid.chatwrap.findbar]} { ::vfind::hide; return }
    # 5b. Completion popup open: close it.
    if {[::vcomp::open]} { ::vcomp::_close; return }
    # 6. Composer mid-edit: clear the current line (like emacs' C-g
    #    clearing the minibuffer).
    ::vstatus::set_msg [::vi18n::mc {quit}]
}

# ── install on the composer ──────────────────────────────────────────

proc ::vedit::install {} {
    set w .mid.chatwrap.chat.input
    bind $w <Tab>          { ::vedit::complete; break }
    bind $w <Alt-p>        { ::vedit::hist_prev; break }
    bind $w <Alt-n>        { ::vedit::hist_next; break }
    # Snapshot undo, for normie only. In emacs/vi lib/emacs.tcl rebinds
    # these to the widget's REAL undo stack — this version restores one
    # whole-buffer snapshot, which is why C-/ felt like undoing
    # everything rather than undoing an edit.
    bind $w <Control-slash> { ::vedit::undo; break }
    bind $w <Control-underscore> { ::vedit::undo; break }
    # C-z, EXPLICITLY — bound nowhere before, it only worked by falling
    # through to Tk's <<Undo>>, which needs -undo (the composer has it
    # since the widget is created with -undo 1). Same real undo stack
    # either way, no accident required.
    bind $w <Control-z> {
        catch { %W edit undo } e
        catch { ::vstatus::set_msg [::vi18n::mc {undone}] }
        break
    }
    bind $w <Control-u>    {
        ::vedit::_maybe_snap
        .mid.chatwrap.chat.input delete "insert linestart" "insert lineend"
        break
    }
    # Snapshot for undo on ordinary typing (burst-based) + live
    # completion refilter while the popup is open.
    bind $w <KeyPress> {+ ::vedit::_maybe_snap }
    # And on paste/cut, which type no key (RM3 #11).
    bind $w <<Paste>> { ::vedit::_snap_then %W paste }
    bind $w <<Cut>>   { ::vedit::_snap_then %W cut }
    # %K so refilter can ignore navigation keysyms — ↑/↓ fire KeyPress
    # (move) then KeyRelease, and refilling on the release is what threw
    # the cursor back to the first item. See lib/vlist.tcl.
    bind $w <KeyRelease> {+ ::vcomp::refilter %K}
    # Popup navigation: Up/Down move the highlight while open.
    bind $w <Up> {
        if {[::vcomp::open]} { ::vcomp::move -1; break }
    }
    bind $w <Down> {
        if {[::vcomp::open]} { ::vcomp::move 1; break }
    }

    # C-g + C-h (emacs help) everywhere.
    bind all <Control-g> { ::vedit::abort; break }
    bind all <Control-h> { ::vhelp::open; break }
}

# ── minibuffer: keyboard-only filter for overlay listboxes ──────────
#
# Creates an entry above $list_w; typing filters the items live,
# ↑/↓ move the selection, Return fires $action with the selected item,
# C-g/Esc returns to typing. This is what makes the overlays usable
# without a mouse.

proc ::vedit::minibuffer {w list_w items_var action} {
    upvar 1 $items_var items
    ttk::entry $w.filter
    pack $w.filter -fill x -padx 4 -pady {4 2} -before $list_w
    # Keep the original list for filtering.
    set ::vedit::mb_items($w) $items
    set ::vedit::mb_action($w) $action
    set ::vedit::mb_list($w) $list_w

    bind $w.filter <KeyRelease> [list ::vedit::_mb_filter $w]
    bind $w.filter <Return>     [list ::vedit::_mb_activate $w]
    bind $w.filter <Up>         [list ::vedit::_mb_move $w -1]
    bind $w.filter <Down>       [list ::vedit::_mb_move $w 1]
    bind $w.filter <Escape>     [list focus $list_w]
    focus $w.filter
}

proc ::vedit::_mb_filter {w} {
    set q [$w.filter get]
    set list_w $::vedit::mb_list($w)
    $list_w delete 0 end
    foreach row [::vedit::fuzzy_sort $q $::vedit::mb_items($w)] {
        $list_w insert end $row
    }
    if {[$list_w size] > 0} { $list_w selection set 0 }
}

proc ::vedit::_mb_move {w delta} {
    set list_w $::vedit::mb_list($w)
    set n [$list_w size]
    if {$n == 0} { return }
    set sel [$list_w curselection]
    set cur [expr {$sel eq "" ? 0 : [lindex $sel 0]}]
    set next [expr {($cur + $delta + $n) % $n}]
    $list_w selection.clear 0 end
    $list_w selection set $next
    $list_w see $next
}

proc ::vedit::_mb_activate {w} {
    set list_w $::vedit::mb_list($w)
    set sel [$list_w curselection]
    if {$sel eq ""} { return }
    set row [$list_w get [lindex $sel 0]]
    uplevel #0 $::vedit::mb_action($w) [list $row]
}

# ── fuzzy matching: the one ranking everything ───────────────────────
#
# USER: "The dropdown selection fields everywhere should have fuzzy
# search, especially model selector". The old filter was a substring
# match — "dsv4f" found nothing in "deepseek-v4-flash-0731", so the
# only honest answer to that complaint is subsequence matching.
#
# Rules, one matcher for every surface:
#   empty query            → everything matches, neutral score
#   subsequence present    → match, scored by how tightly it matches
#   anything else          → no match
#
# Scoring rewards: consecutive query characters, a hit at a word start,
# a hit near the front, and a short haystack (the whole point is that
# typing a few letters lands the item you meant at the top).

# 1 when $q is a case-insensitive subsequence of $s, else 0.
proc ::vedit::fuzzy_match {q s} {
    if {$q eq ""} { return 1 }
    set s [string tolower $s]
    set q [string tolower $q]
    set i 0
    foreach c [split $q ""] {
        set i [string first $c $s $i]
        if {$i < 0} { return 0 }
        incr i
    }
    return 1
}

# Higher is better; only call after fuzzy_match said yes.
proc ::vedit::fuzzy_score {q s} {
    if {$q eq ""} { return 0 }
    set s [string tolower $s]
    set q [string tolower $q]
    set score 0
    set i 0
    set prev -2
    set prevchar ""
    foreach c [split $q ""] {
        set i [string first $c $s $i]
        if {$i < 0} { return -1000000 }
        # consecutive letters of the query landing on consecutive
        # letters of the haystack
        if {$prevchar ne "" && $i == $prev + 1} { incr score 3 }
        # a word start (or the string start) is worth more
        if {$i == 0} {
            incr score 4
        } elseif {[string index $s [expr {$i - 1}]] in {- _ . / }} {
            incr score 4
        }
        # nearer the front wins
        incr score [expr {20 - ($i > 20 ? 20 : $i)}]
        set prev $i
        set prevchar $c
        incr i
    }
    # a short haystack that matches the whole query beats a long one
    incr score [expr {50 - [string length $s] / 4}]
    return $score
}

# Rank $items against $q, best first; non-matches dropped.
proc ::vedit::fuzzy_sort {q items} {
    if {$q eq ""} { return $items }
    set scored {}
    foreach it $items {
        if {![fuzzy_match $q $it]} { continue }
        lappend scored [list [fuzzy_score $q $it] $it]
    }
    set out {}
    foreach pair [lsort -decreasing -integer -index 0 $scored] {
        lappend out [lindex $pair 1]
    }
    return $out
}

# A fuzzy PICKER: a search box over a ranked list. One widget, fully
# keyboard-driven (type, ↑/↓, Return, Esc), created near the widget
# that asked for it so the choice feels local.
#
#   title    window title
#   items    the candidates
#   current  preselected row (gets the selection + scrolled into view)
#   onpick   command prefix run as `onpick $item` — or "" to return the
#            choice synchronously (the caller's event loop blocks on
#            tkwait, matching the ask-dialog convention)
#
# Returns the picked item (sync mode), or "" on cancel.
proc ::vedit::fuzzy_pick {title items current {onpick ""}} {
    set w .fuzzypick
    if {[winfo exists $w]} { destroy $w }
    toplevel $w
    wm title $w $title
    wm transient $w .
    ttk::entry $w.q
    pack $w.q -fill x -padx 6 -pady {6 3}
    listbox $w.l -activestyle none -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -selectbackground $::vt::T(accent) \
        -selectforeground $::vt::T(bg_input) -highlightthickness 0 \
        -font {TkDefaultFont 11} -height 14
    ttk::scrollbar $w.sb -orient vertical -command [list $w.l yview]
    $w.l configure -yscrollcommand [list $w.sb set]
    pack $w.sb -side right -fill y -padx {0 6} -pady {0 6}
    pack $w.l -fill both -expand 1 -padx 6 -pady {0 6}

    ::vedit::_fp_paint $w $items ""
    # Preselect the current value so Return with no typing keeps it.
    if {$current ne ""} {
        set shown -1
        catch { set shown [lsearch -exact [$w.l get 0 end] $current] }
        if {$shown >= 0} {
            $w.l selection clear 0 end
            $w.l selection set $shown
            $w.l see $shown
        }
    }

    bind $w.q <KeyRelease> [list ::vedit::_fp_paint $w $items [$w.q get]]
    bind $w.q <Down> [list focus $w.l]
    bind $w.q <Return> [list ::vedit::_fp_pick $w $onpick]
    bind $w.q <Escape> [list ::vedit::_fp_cancel $w $onpick]
    bind $w.l <Return> [list ::vedit::_fp_pick $w $onpick]
    bind $w.l <Escape> [list ::vedit::_fp_cancel $w $onpick]
    bind $w <Destroy> [list ::vedit::_fp_gone $w]

    focus $w.q
    grab set $w
    if {$onpick eq ""} {
        tkwait window $w
        return $::vedit::_fp_result
    }
    return ""
}

proc ::vedit::_fp_paint {w items q} {
    $w.l delete 0 end
    foreach row [::vedit::fuzzy_sort $q $items] {
        $w.l insert end $row
    }
    if {[$w.l size] > 0} {
        $w.l selection set 0
        $w.l see 0
    }
}

proc ::vedit::_fp_pick {w onpick} {
    set sel [$w.l curselection]
    if {$sel eq ""} {
        # Nothing highlighted (the query filtered everything away):
        # keep the dialog rather than picking nothing.
        return
    }
    set item [$w.l get [lindex $sel 0]]
    catch { grab release $w }
    destroy $w
    if {$onpick eq ""} {
        set ::vedit::_fp_result $item
    } else {
        uplevel #0 [list $onpick $item]
    }
}

proc ::vedit::_fp_cancel {w onpick} {
    catch { grab release $w }
    destroy $w
    if {$onpick eq ""} { set ::vedit::_fp_result "" }
}

proc ::vedit::_fp_gone {w} {
    # The window can die without a pick (grab lost, wm close): the
    # synchronous caller must not block forever.
    if {![info exists ::vedit::_fp_result]} { set ::vedit::_fp_result "" }
}

proc ::vedit::_mb_move {w delta} {
    set list_w $::vedit::mb_list($w)
    set n [$list_w size]
    if {$n == 0} { return }
    set sel [$list_w curselection]
    set cur [expr {$sel eq "" ? 0 : [lindex $sel 0]}]
    set next [expr {($cur + $delta + $n) % $n}]
    $list_w selection clear 0 end
    $list_w selection set $next
    $list_w see $next
}

proc ::vedit::_mb_activate {w} {
    set list_w $::vedit::mb_list($w)
    set sel [$list_w curselection]
    if {$sel eq ""} { return }
    set row [$list_w get [lindex $sel 0]]
    uplevel #0 $::vedit::mb_action($w) [list $row]
}
