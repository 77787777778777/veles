# veles-desktop/lib/taskpanel.tcl — interactive shells and monitors.
#
# The registry is authoritative. This module owns presentation and input
# state only: selected task, pending request generations and the poll timer.
# No task or output registry is duplicated in the desktop process.

namespace eval ::voverlay {
    variable tasks_selected ""
    variable tasks_rows {}
    # One entry per LISTBOX row: the task id, or "" for a section header
    # or a message row. Listbox indices are never tasks_rows indices.
    variable tasks_row_ids {}
    variable tasks_generation 0
    variable tasks_output_generation 0
}

proc ::voverlay::_tasks_session {} {
    if {[info exists ::vchat::session] && $::vchat::session ne ""} {
        return $::vchat::session
    }
    return "desktop"
}

proc ::voverlay::_tasks_path {path} {
    return "$path?session=[::vapi::urlencode [_tasks_session]]"
}

proc ::voverlay::_tasks {w} {
    variable tasks_generation
    variable tasks_rows
    variable tasks_selected
    ::vmanage::window $w [::vi18n::mc {Shells and monitors}] 980x640 {520 360}
    incr tasks_generation
    set tasks_rows {}
    set tasks_selected ""

    ::vmanage::button $w.bar reload [::vi18n::mc {Reload}] \
        [list ::voverlay::_tasks_fill $w] [::vi18n::mc {refresh the list}]
    ::vmanage::button $w.bar output [::vi18n::mc {Output}] \
        [list ::voverlay::_tasks_selected_output $w] [::vi18n::mc {show selected shell output}]
    ::vmanage::button $w.bar attach [::vi18n::mc {Attach}] \
        [list ::voverlay::_tasks_selected_attach $w] [::vi18n::mc {open the selected child session}]
    ::vmanage::button $w.bar detach [::vi18n::mc {Detach foreground}] \
        [list ::voverlay::_tasks_detach $w] [::vi18n::mc {move this session's running foreground command to a background shell; keeps running}]
    ::vmanage::button $w.bar cancel [::vi18n::mc {Cancel}] \
        [list ::voverlay::_tasks_selected_cancel $w] [::vi18n::mc {cancel the selected task}]

    ttk::panedwindow $w.main -orient horizontal
    ttk::frame $w.list
    # browse, not single: in single mode the arrow keys only move the
    # active cursor and nothing is selected without Space.
    listbox $w.list.rows -exportselection 0 -takefocus 1 -selectmode browse \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(accent) -selectforeground $::vt::T(bg)
    ttk::scrollbar $w.list.sb -orient vertical -command [list $w.list.rows yview]
    $w.list.rows configure -yscrollcommand [list $w.list.sb set]
    pack $w.list.sb -side right -fill y
    pack $w.list.rows -side left -fill both -expand 1
    ttk::frame $w.detail
    text $w.detail.t -wrap word -state normal \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -font {monospace 10}
    ttk::scrollbar $w.detail.sb -orient vertical -command [list $w.detail.t yview]
    $w.detail.t configure -yscrollcommand [list $w.detail.sb set]
    pack $w.detail.sb -side right -fill y
    pack $w.detail.t -side left -fill both -expand 1
    $w.main add $w.list -weight 1
    $w.main add $w.detail -weight 2
    pack $w.main -fill both -expand 1 -padx 8 -pady {0 8}

    bind $w.list.rows <<ListboxSelect>> [list ::voverlay::_tasks_list_select $w]
    bind $w.list.rows <Return> [list ::voverlay::_tasks_selected_output $w]
    bind $w <F5> [list ::voverlay::_tasks_fill $w]
    bind $w <Escape> [list destroy $w]
    bind $w <Destroy> [list ::voverlay::_tasks_stop %W $w]
    catch { ::va11y::name $w.list.rows [::vi18n::mc {shells, monitors and subagents}] }
    catch { ::va11y::name $w.detail.t [::vi18n::mc {selected task details and shell output}] }
    ::vtext::readonly $w.detail.t
    catch { ::va11y::tab_order [list $w.bar.reload $w.bar.output $w.bar.attach $w.bar.detach $w.bar.cancel $w.list.rows $w.detail.t] }
    focus $w.list.rows
    _tasks_fill $w
    _tasks_poll $w
}

proc ::voverlay::_tasks_stop {which w} {
    variable tasks_after
    variable tasks_generation
    variable tasks_output_generation
    if {$which ne $w} { return }
    incr tasks_generation
    incr tasks_output_generation
    if {[info exists tasks_after] && $tasks_after ne ""} {
        after cancel $tasks_after
        set tasks_after ""
    }
}

proc ::voverlay::_tasks_poll {w} {
    variable tasks_after
    if {![winfo exists $w]} { set tasks_after ""; return }
    _tasks_fill $w
    set tasks_after [after 3000 [list ::voverlay::_tasks_poll $w]]
}

proc ::voverlay::_tasks_fill {w} {
    variable tasks_generation
    if {![winfo exists $w]} { return }
    set generation [incr tasks_generation]
    ::vapi::get_json_async [_tasks_path /api/tasks] \
        [list ::voverlay::_tasks_loaded $w $generation]
}

proc ::voverlay::_tasks_loaded {w generation ok resp} {
    variable tasks_generation
    variable tasks_rows
    variable tasks_selected
    variable tasks_output_generation
    variable tasks_row_ids
    if {$generation != $tasks_generation || ![winfo exists $w]} { return }
    # List content, selection and detail output form one view. A new
    # list response makes every previously requested output stale.
    incr tasks_output_generation
    _tasks_detach_state $w
    # A transport failure and a 200 without a `tasks` list are the same
    # to the person: no list, so no per-task action may stay armed
    # (TSG-8). tasks_selected is kept only as the row to re-select when
    # a later poll brings the list back; actions require a listed task.
    if {!$ok || [catch {dict get $resp tasks} tasks]} {
        set tasks_rows {}
        set tasks_row_ids {}
        $w.list.rows delete 0 end
        $w.list.rows insert end "tasks unavailable: $resp"
        _tasks_detail $w ""
        foreach button {output attach cancel} { $w.bar.$button state disabled }
        return
    }
    set tasks_rows {}
    set tasks_row_ids {}
    $w.list.rows delete 0 end
    set previous $tasks_selected
    set detail_view [$w.detail.t yview]
    set selected_index -1
    set titles [dict create \
        shell [::vi18n::mc {Shells}] \
        monitor [::vi18n::mc {Monitors}] \
        subagent [::vi18n::mc {Subagents}]]
    foreach key {shell monitor subagent} {
        set title [dict get $titles $key]
        set found 0
        foreach task $tasks {
            if {[dict get $task kind] ne $key} { continue }
            if {!$found} {
                $w.list.rows insert end "── $title ──"
                lappend tasks_row_ids ""
                set found 1
            }
            set id [dict get $task id]
            lappend tasks_rows $task
            set status [dict get $task status]
            set label [dict get $task label]
            set secs [dict get $task elapsed_s]
            if {$previous ne "" && $previous == $id} {
                set selected_index [llength $tasks_row_ids]
            }
            $w.list.rows insert end [format "\[%s\] %-9s %ss  %s" $id $status $secs $label]
            lappend tasks_row_ids $id
        }
    }
    if {![llength $tasks_rows]} {
        set tasks_selected ""
        $w.list.rows insert end [::vi18n::mc {no shells or monitors to show}]
        _tasks_detail $w [::vi18n::mc {no task selected}]
        foreach button {output attach cancel} { $w.bar.$button state disabled }
        return
    }
    if {$selected_index < 0} {
        set previous [dict get [lindex $tasks_rows 0] id]
        set selected_index [lsearch -exact $tasks_row_ids $previous]
    }
    set tasks_selected $previous
    _tasks_highlight $w $selected_index
    _tasks_show_selected $w [lindex $detail_view 0]
}

# Highlight one listbox row and put the keyboard cursor on it, so the
# arrow keys continue from the selected task after every refill.
proc ::voverlay::_tasks_highlight {w index} {
    $w.list.rows selection clear 0 end
    $w.list.rows selection set $index
    $w.list.rows activate $index
    $w.list.rows see $index
}

# The listbox row of task `id`, or -1.
proc ::voverlay::_tasks_row_of {id} {
    variable tasks_row_ids
    set index 0
    foreach row $tasks_row_ids {
        if {$row ne "" && $row == $id} { return $index }
        incr index
    }
    return -1
}

proc ::voverlay::_tasks_list_select {w} {
    variable tasks_row_ids
    variable tasks_selected
    set index [$w.list.rows curselection]
    if {![llength $index]} { return }
    set index [lindex $index 0]
    if {$index < 0 || $index >= [llength $tasks_row_ids]} { return }
    set id [lindex $tasks_row_ids $index]
    if {$id eq ""} {
        # A section header. Move on in the direction of travel (from
        # the selected row towards the header), so an arrow key steps
        # over it instead of sticking; the other way when that side has
        # no task.
        set from [_tasks_row_of $tasks_selected]
        set step [expr {$from >= 0 && $from > $index ? -1 : 1}]
        foreach dir [list $step [expr {-$step}]] {
            for {set i [expr {$index + $dir}]} \
                {$i >= 0 && $i < [llength $tasks_row_ids]} {incr i $dir} {
                if {[lindex $tasks_row_ids $i] ne ""} {
                    _tasks_select $w [lindex $tasks_row_ids $i]
                    return
                }
            }
        }
        return
    }
    _tasks_select $w $id
}

proc ::voverlay::_tasks_select {w id} {
    variable tasks_selected
    set index [_tasks_row_of $id]
    if {$index < 0} { return }
    set tasks_selected $id
    _tasks_highlight $w $index
    _tasks_show_selected $w
}

# Detach follows the composer's Detach button: armed while this
# session's turn runs. Whether that turn is inside a foreground command
# right now is engine state; the endpoint answers honestly when not.
proc ::voverlay::_tasks_detach_state {w} {
    set running 0
    catch { set running [::vchat::is_running [_tasks_session]] }
    $w.bar.detach state [expr {$running ? "!disabled" : "disabled"}]
}

# The terminal tool adopts the live process within one ~100ms poll;
# re-read the list shortly after so the new shell row shows without
# waiting for the 3s poll.
proc ::voverlay::_tasks_detach {w} {
    ::vchat::detach_current
    after 400 [list ::voverlay::_tasks_fill $w]
}

proc ::voverlay::_tasks_show_selected {w {fraction 0}} {
    variable tasks_rows
    variable tasks_selected
    variable tasks_output_generation
    if {$tasks_selected eq ""} { return }
    foreach task $tasks_rows {
        if {[dict get $task id] != $tasks_selected} { continue }
        set text "task $tasks_selected — [dict get $task kind] — [dict get $task status]\n"
        append text "condition/command: [dict get $task label]\n"
        append text "elapsed: [dict get $task elapsed_s]s\n"
        set child [::vapi::field $task child_session]
        if {$child ne ""} { append text "\[attach\] child session: $child\n" }
        _tasks_detail $w $text
        $w.bar.output state [expr {[dict get $task kind] eq "shell" ? "!disabled" : "disabled"}]
        $w.bar.attach state [expr {$child ne "" ? "!disabled" : "disabled"}]
        $w.bar.cancel state [expr {[dict get $task status] eq "running" ? "!disabled" : "disabled"}]
        # Refresh output in place. A later selection invalidates this
        # generation, so an old large response cannot replace a newer one.
        if {[dict get $task kind] eq "shell"} {
            _tasks_output $w $tasks_selected 1 $fraction
        } else {
            incr tasks_output_generation
        }
        return
    }
}

proc ::voverlay::_tasks_detail {w text} {
    if {![winfo exists $w.detail.t]} { return }
    ::vtext::set_text $w.detail.t $text
}

proc ::voverlay::_tasks_selected_output {w} {
    variable tasks_selected
    if {$tasks_selected eq "" || [_tasks_row_of $tasks_selected] < 0} { return }
    _tasks_output $w $tasks_selected 0
}

proc ::voverlay::_tasks_output {w id preserve {fraction 0}} {
    variable tasks_output_generation
    set generation [incr tasks_output_generation]
    ::vapi::get_json_async [_tasks_path /api/tasks/$id/output] \
        [list ::voverlay::_tasks_output_loaded $w $generation $id $preserve $fraction]
}

proc ::voverlay::_tasks_output_loaded {w generation id preserve fraction ok resp} {
    variable tasks_output_generation
    variable tasks_selected
    if {$generation != $tasks_output_generation || $id != $tasks_selected || ![winfo exists $w]} {
        return
    }
    set saved_view [$w.detail.t yview]
    if {!$ok} {
        if {!$preserve} { ::vstatus::set_msg [format [::vi18n::mc {no output for task %1$s}] $id] }
        return
    }
    set current [$w.detail.t get 1.0 end-1c]
    set marker [string first "\n\n--- task " $current]
    if {$marker >= 0} { set current [string range $current 0 [expr {$marker - 1}]] }
    if {![dict exists $resp output]} {
        ::vstatus::set_msg [format [::vi18n::mc {no output for task %1$s}] $id]
        return
    }
    set text "$current\n\n--- task $id output ---\n[dict get $resp output]\n"
    _tasks_detail $w $text
    if {$preserve} {
        # Text layout is lazy: immediately after replacing the buffer Tk
        # may still report one unwrapped display line and clamp yview to
        # zero. Restore after that layout pass, but never past a newer
        # selection/output generation.
        after idle [list ::voverlay::_tasks_restore_view $w $generation $fraction]
    } else {
        $w.detail.t see end
    }
}

proc ::voverlay::_tasks_restore_view {w generation fraction} {
    variable tasks_output_generation
    if {$generation != $tasks_output_generation || ![winfo exists $w.detail.t]} { return }
    $w.detail.t yview moveto $fraction
}

proc ::voverlay::_tasks_selected_attach {w} {
    variable tasks_rows
    variable tasks_selected
    foreach task $tasks_rows {
        if {[dict get $task id] != $tasks_selected} { continue }
        set child [::vapi::field $task child_session]
        if {$child ne ""} { _tasks_action $w.main $tasks_selected attach:$child }
        return
    }
}

proc ::voverlay::_tasks_selected_cancel {w} {
    variable tasks_selected
    # Only a task the list currently shows: after a failed refresh the
    # remembered id is a restore hint, not a target (TSG-8).
    if {$tasks_selected ne "" && [_tasks_row_of $tasks_selected] >= 0} {
        _tasks_action $w.main $tasks_selected cancel
    }
}

proc ::voverlay::_tasks_action {t id action} {
    set w [winfo toplevel $t]
    if {$action eq "output"} {
        _tasks_output $w $id 0
        return
    }
    if {$action eq "cancel"} {
        if {![::vmanage::confirm $w [::vi18n::mc {Cancel task}] \
            [format [::vi18n::mc {Cancel task %1$s? This cannot be undone.}] $id]]} {
            return
        }
        ::vapi::post_async [_tasks_path /api/tasks/$id/cancel] "" \
            [list ::voverlay::_tasks_cancel_failed $id] \
            [list ::voverlay::_tasks_cancelled $w $id]
        return
    }
    if {[string match "attach:*" $action]} {
        set session [string range $action 7 end]
        if {$session eq "" || $session eq "null"} { return }
        destroy $w
        if {[catch {::vchat::open_session $session} error]} {
            ::vstatus::set_msg [format [::vi18n::mc {attach failed: %1$s}] $error]
        } else {
            ::vstatus::set_msg [format [::vi18n::mc {attached to child session %1$s}] $session]
        }
    }
}

proc ::voverlay::_tasks_cancel_failed {id error} {
    ::vstatus::set_msg [format [::vi18n::mc {cancel failed: %1$s}] $error]
}

proc ::voverlay::_tasks_cancelled {w id response} {
    ::vstatus::set_msg [format [::vi18n::mc {cancelled task %1$s}] $id]
    if {[winfo exists $w]} { _tasks_fill $w }
}
