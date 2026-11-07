# veles-desktop/lib/fleetdeployui.tcl — F1 operator deployment dialog.
# Review is always visible before a mutating action. All transitions live in
# fleetdeploy.tcl; this file only gathers input and presents its results.
package require json
source [file join [file dirname [file normalize [info script]]] fleetdeploy.tcl]

namespace eval ::vfleetui {
    variable w ""
    variable selected {}
    variable manifest ""
    variable preflight ""
    variable operation ""
    variable op_id ""
    variable review_text ""
    variable busy 0
    variable worker ""
    variable worker_output ""
    variable worker_action ""
    variable worker_launcher ::vfleetui::_launch_worker
    # Captured at SOURCE time. `info script` inside a button callback is
    # empty, so the worker path used to resolve to ./fleetworker.tcl
    # relative to whatever the desktop's cwd was (F1-03).
    variable libdir [file dirname [file normalize [info script]]]
    variable actions {review stage verify continue rollback finish-rollback resume abandon}
}

proc ::vfleetui::_error {message} {
    ::vstatus::set_msg $message
    if {[winfo exists $::vfleetui::w.status]} {
        $::vfleetui::w.status configure -text $message
    }
}

proc ::vfleetui::_selected_node {} {
    set selected [::vinstpane::selected]
    if {[::vinstpane::_is_cluster $selected]} {
        # _cluster_node returns the whole /api/fleet node object; the
        # dialog and the manifest take its id (FLEET-1).
        set node [::vinstpane::_cluster_node $selected]
        if {$node eq "" || ![dict exists $node id]} { return "" }
        return [dict get $node id]
    }
    set spec [::vinst::get $selected]
    if {$spec eq ""} { return "" }
    return [::vapi::field $spec node_id]
}

# Mutating actions run in a separate tclsh worker over a command pipe,
# which AndroWish cannot start; the dialog is refused there rather than
# failing after a journal was written.
proc ::vfleetui::available {} {
    if {[llength [info commands ::vandroid::on]] && [::vandroid::on]} { return 0 }
    return 1
}

proc ::vfleetui::open {} {
    variable w
    variable selected
    variable manifest
    variable preflight
    variable operation
    variable op_id
    if {![available]} {
        _error [::vi18n::mc {Fleet deployment runs from a desktop computer; it is not available on Android.}]
        return
    }
    set node [_selected_node]
    if {$node eq ""} {
        _error "select a cluster node or an explicitly bound saved instance first"
        return
    }
    set selected $node
    set manifest ""
    set preflight ""
    set operation ""
    set op_id ""
    if {[winfo exists .fleetdeploy]} { destroy .fleetdeploy }
    set w [toplevel .fleetdeploy]
    wm title $w [::vi18n::mc {Fleet deployment}]
    wm geometry $w 760x620
    ttk::frame $w.body -padding 12
    pack $w.body -fill both -expand 1
    ttk::label $w.body.title -text [::vi18n::mc {Reviewed fleet deployment}] -font TkHeadingFont
    ::vi18n::label $w.body.title {Reviewed fleet deployment}
    pack $w.body.title -anchor w
    ttk::label $w.body.hint -wraplength 700 -justify left -text \
        [::vi18n::mc {A signed release is staged only after preflight. Restart/verify, continuation and rollback are separate actions. A healthy process must report the exact reviewed source revision.}]
    ::vi18n::label $w.body.hint {A signed release is staged only after preflight. Restart/verify, continuation and rollback are separate actions. A healthy process must report the exact reviewed source revision.}
    pack $w.body.hint -anchor w -pady {4 10}
    ttk::frame $w.body.release
    pack $w.body.release -fill x
    ttk::label $w.body.release.targetl -text [::vi18n::mc {Target node IDs (canary first, comma separated)}]
    ::vi18n::label $w.body.release.targetl {Target node IDs (canary first, comma separated)}
    grid $w.body.release.targetl -row 0 -column 0 -columnspan 4 -sticky w
    ttk::entry $w.body.release.targets -textvariable ::vfleetui::target_ids
    grid $w.body.release.targets -row 1 -column 0 -columnspan 4 -sticky ew -pady {2 6}
    ttk::label $w.body.release.l -text [::vi18n::mc {Release tag}]
    ::vi18n::label $w.body.release.l {Release tag}
    grid $w.body.release.l -row 2 -column 0 -sticky w
    ttk::entry $w.body.release.tag -width 18 -textvariable ::vfleetui::release_tag
    grid $w.body.release.tag -row 2 -column 1 -sticky ew -padx 6
    ttk::label $w.body.release.cl -text [::vi18n::mc {Exact commit}]
    ::vi18n::label $w.body.release.cl {Exact commit}
    grid $w.body.release.cl -row 2 -column 2 -sticky w
    ttk::entry $w.body.release.commit -width 44 -textvariable ::vfleetui::release_commit
    grid $w.body.release.commit -row 2 -column 3 -sticky ew -padx 6
    ttk::label $w.body.release.wl -text [::vi18n::mc {Workflow commit}]
    ::vi18n::label $w.body.release.wl {Workflow commit}
    grid $w.body.release.wl -row 3 -column 0 -sticky w -pady 5
    ttk::entry $w.body.release.workflow -width 44 -textvariable ::vfleetui::workflow_commit
    grid $w.body.release.workflow -row 3 -column 1 -columnspan 3 -sticky ew -padx 6 -pady 5
    grid columnconfigure $w.body.release 3 -weight 1
    ttk::label $w.body.release.repo -text [::vi18n::mc {Repository (owner/name)}]
    ::vi18n::label $w.body.release.repo {Repository (owner/name)}
    grid $w.body.release.repo -row 4 -column 0 -sticky w
    ttk::entry $w.body.release.repository -textvariable ::vfleetui::repository
    grid $w.body.release.repository -row 4 -column 1 -columnspan 3 -sticky ew -padx 6
    ttk::frame $w.body.actions
    pack $w.body.actions -fill x -pady 8
    ttk::button $w.body.actions.review -text [::vi18n::mc {Build manifest + preflight}] -command ::vfleetui::review
    ::vi18n::label $w.body.actions.review {Build manifest + preflight}
    pack $w.body.actions.review -side left
    ttk::button $w.body.actions.stage -text [::vi18n::mc {Stage canary}] -command [list ::vfleetui::mutate stage]
    ::vi18n::label $w.body.actions.stage {Stage canary}
    pack $w.body.actions.stage -side left -padx 4
    ttk::button $w.body.actions.verify -text [::vi18n::mc {Restart + verify canary}] -command [list ::vfleetui::mutate verify]
    ::vi18n::label $w.body.actions.verify {Restart + verify canary}
    pack $w.body.actions.verify -side left -padx 4
    ttk::button $w.body.actions.continue -text [::vi18n::mc {Continue to next target}] -command [list ::vfleetui::mutate continue]
    ::vi18n::label $w.body.actions.continue {Continue to next target}
    pack $w.body.actions.continue -side left -padx 4
    ttk::button $w.body.actions.rollback -text [::vi18n::mc {Roll back selected target}] -command [list ::vfleetui::mutate rollback]
    ::vi18n::label $w.body.actions.rollback {Roll back selected target}
    pack $w.body.actions.rollback -side left -padx 4
    ttk::button $w.body.actions.finish-rollback -text [::vi18n::mc {Finish rollback}] -command [list ::vfleetui::mutate finish-rollback]
    ::vi18n::label $w.body.actions.finish-rollback {Finish rollback}
    pack $w.body.actions.finish-rollback -side left -padx 4
    ttk::button $w.body.actions.resume -text [::vi18n::mc {Resume unfinished operation}] -command ::vfleetui::resume
    ::vi18n::label $w.body.actions.resume {Resume unfinished operation}
    pack $w.body.actions.resume -side left -padx 4
    ttk::button $w.body.actions.abandon -text [::vi18n::mc {Abandon untouched operation}] -command [list ::vfleetui::mutate abandon]
    ::vi18n::label $w.body.actions.abandon {Abandon untouched operation}
    pack $w.body.actions.abandon -side left -padx 4
    ttk::label $w.status -text [::vi18n::mc {No deployment started.}] -wraplength 720 -justify left
    ::vi18n::label $w.status {No deployment started.}
    pack $w.status -anchor w -pady 5
    text $w.body.review -wrap none -height 25 -width 90 -state disabled
    pack $w.body.review -fill both -expand 1
    ttk::button $w.body.close -text [::vi18n::mc Close] -command [list destroy $w]
    ::vi18n::label $w.body.close Close
    pack $w.body.close -anchor e -pady 5
    set ::vfleetui::target_ids $node
    ::vfleetjournal::configure [file join [::vpaths::data_dir] fleet-deployments]
    _render "Select the release identity, then build the manifest and preflight. No changes have been made."
}

proc ::vfleetui::_operation_order {a b} {
    set aid [dict get $a id]
    set bid [dict get $b id]
    if {$aid < $bid} { return -1 }
    if {$aid > $bid} { return 1 }
    return 0
}

proc ::vfleetui::resume {} {
    variable manifest
    variable preflight
    variable operation
    variable op_id
    if {$::vfleetui::busy} { return }
    set pending [::vfleetjournal::unfinished]
    set pending [lsort -command ::vfleetui::_operation_order $pending]
    if {![llength $pending]} {
        _render "No unfinished deployment operation was found."
        return 0
    }
    # The oldest unfinished operation is loaded; once it is finished,
    # rolled back or abandoned, Resume loads the next. It used to refuse
    # outright whenever two existed, which a declined confirmation alone
    # could cause (F1-05).
    set journal [lindex $pending 0]
    set id [dict get $journal id]
    set others [lmap op [lrange $pending 1 end] {dict get $op id}]
    _busy
    if {[catch {
        _nodes_and_instances
        set observations {}
        foreach node_id [dict get $journal target_order] {
            # A target that stopped before any installation needs no
            # observation; one that cannot be reached is refused below by
            # the executor, which names it.
            if {[catch {lassign [::vfleetdeploy::_binding $node_id] instance spec}]} { continue }
            if {[catch {::vfleetdeploy::health_observation $instance} observation]} { continue }
            dict set observations $node_id $observation
        }
        set operation [::vfleetdeploy::resume $id $observations]
        set manifest [dict get $operation manifest]
        set preflight [dict create kind fleet-preflight plan_sig [dict get $manifest plan_sig] ready 1 observations {}]
        dict for {node_id observation} [dict get $operation preflight] {
            dict lappend preflight observations $observation
        }
        set op_id $id
    } error]} {
        _render "Resume refused: $error"
        _finish
        return 0
    }
    set message "Operation $id loaded from its durable journal. No interrupted install was replayed; inspect live health before continuing."
    if {[llength $others]} {
        append message " Also unfinished, resumed next once this one is closed: [join $others {, }]."
    }
    _render $message
    _finish
    return 1
}

proc ::vfleetui::_busy {} {
    set ::vfleetui::busy 1
    foreach button $::vfleetui::actions {
        catch {$::vfleetui::w.body.actions.$button configure -state disabled}
    }
}

proc ::vfleetui::_release {} {
    return [dict create repository $::vfleetui::repository tag $::vfleetui::release_tag \
        commit $::vfleetui::release_commit workflow_sha $::vfleetui::workflow_commit]
}

proc ::vfleetui::_nodes_and_instances {} {
    set nodes [::vfleetdeploy::nodes_from_inventory [::vapi::get_json /api/fleet 5000]]
    set ::vfleetdeploy::nodes $nodes
    set registry {}
    foreach name [::vinst::names] {
        dict set registry $name [::vinst::get $name]
    }
    set ::vfleetdeploy::instances $registry
    return $nodes
}

proc ::vfleetui::_text {text} {
    set w $::vfleetui::w.body.review
    $w configure -state normal
    $w delete 1.0 end
    $w insert end $text
    $w configure -state disabled
}

proc ::vfleetui::_render {message} {
    if {$message ne ""} { _error $message }
    set text $message
    if {$::vfleetui::manifest ne ""} {
        append text "\n\nMANIFEST\n[::vapi::_json_value [::vapi::json_object $::vfleetui::manifest]]"
    }
    if {$::vfleetui::preflight ne ""} {
        append text "\n\nPREFLIGHT\n[::vapi::_json_value [::vapi::json_object $::vfleetui::preflight]]"
        append text "\n\nDIFF\n"
        foreach row [::vfleetdeploy::diff $::vfleetui::manifest] {
            append text "[dict get $row node_id] ([dict get $row stage]): [dict get $row current_revision] -> [dict get $row desired_commit]\n"
        }
    }
    if {$::vfleetui::operation ne ""} {
        if {[dict exists $::vfleetui::operation id]} {
            append text "\n\nOPERATION [dict get $::vfleetui::operation id]\n"
        }
        if {[dict exists $::vfleetui::operation targets]} {
            foreach {id target} [dict get $::vfleetui::operation targets] {
                append text "$id: [dict get $target status]\n"
            }
        }
        if {[dict exists $::vfleetui::operation failure]} {
            append text "Failure: [dict get $::vfleetui::operation failure]\n"
        }
    }
    _text $text
}

proc ::vfleetui::review {} {
    variable manifest
    variable preflight
    variable operation
    variable op_id
    if {$::vfleetui::busy} { return }
    set new_manifest ""
    set new_preflight ""
    # The health probes are synchronous HTTP inside this callback; keep
    # the actions disabled meanwhile so Stage cannot run against the
    # previous review mid-probe (F1-12).
    _busy
    if {[catch {
        _nodes_and_instances
        set ids {}
        foreach raw [split $::vfleetui::target_ids ,] {
            set id [string trim $raw]
            if {$id eq ""} { continue }
            if {[lsearch -exact $ids $id] >= 0} { error "target node IDs must be unique" }
            lappend ids $id
        }
        if {![llength $ids]} { error "at least one target node ID is required" }
        set m [::vfleetdeploy::manifest $ids [_release] native-bundle]
        set observations {}
        foreach target [dict get $m targets] {
            dict set observations [dict get $target node_id] \
                [::vfleetdeploy::health_observation [dict get $target instance]]
        }
        set new_manifest $m
        set new_preflight [::vfleetdeploy::preflight $m $observations]
    } error]} {
        _render "Review refused: $error"
        _finish
        return 0
    }
    set manifest $new_manifest
    set preflight $new_preflight
    set operation ""
    set op_id ""
    _render "Review complete. No artifact was installed and no service was restarted."
    _finish
    return 1
}

proc ::vfleetui::_finish {} {
    set ::vfleetui::busy 0
    foreach button $::vfleetui::actions {
        catch {$::vfleetui::w.body.actions.$button configure -state normal}
    }
}

proc ::vfleetui::_action_target {operation action} {
    if {$action in {continue finish-rollback abandon}} { return "" }
    set selected $::vfleetui::selected
    if {$action eq "stage"} {
        foreach target [dict get $operation manifest targets] {
            set id [dict get $target node_id]
            set state [dict get $operation targets $id status]
            if {$state ni {healthy completed rolled-back}} { return $id }
        }
        _error "no target is eligible for staging"
        return ""
    }
    set candidates {}
    set healthy {}
    foreach target [dict get $operation manifest targets] {
        set id [dict get $target node_id]
        set state [dict get $operation targets $id status]
        if {$action eq "verify" && $state eq "staged"} {
            lappend candidates $id
        } elseif {$action eq "rollback"} {
            if {$state in {restarting failed rollback-preview rollback-staged rollback-restarting}} {
                lappend candidates $id
            } elseif {$state eq "healthy"} {
                lappend healthy $id
            }
        }
    }
    if {$action eq "rollback" && ![llength $candidates]} { set candidates $healthy }
    if {[llength $candidates] == 1} { return [lindex $candidates 0] }
    if {$action in {verify rollback}} {
        _error "$action requires exactly one eligible target; inspect the operation and select an unambiguous state"
        return ""
    }
    return $selected
}

proc ::vfleetui::_worker_finish {} {
    variable worker
    variable worker_output
    variable worker_action
    variable operation
    variable op_id
    if {$worker eq ""} { return }
    if {![eof $worker]} {
        set chunk [read $worker]
        append worker_output $chunk
        fileevent $worker readable ::vfleetui::_worker_finish
        return
    }
    fileevent $worker readable {}
    append worker_output [read $worker]
    set close_error ""
    catch {close $worker} close_error
    set worker ""
    set output $worker_output
    set worker_output ""
    # stderr is merged into the pipe, so a Tcl error or a warning ahead of
    # the reply is shown rather than lost; the reply is the last line.
    set reply ""
    if {[catch {::json::json2dict $output} reply] || [catch {dict size $reply}]} {
        set lines [lsearch -all -inline -not -exact [split [string trim $output] \n] ""]
        if {[catch {::json::json2dict [lindex $lines end]} reply] || [catch {dict size $reply}]} {
            set shown [::vfleetdeploy::diagnostic [string trim "$close_error\n$output"]]
            _render "Deployment worker failed: $shown"
            _finish
            return
        }
    }
    if {[catch {dict get $reply ok} reply_ok]} {
        _render "Deployment worker failed: malformed structured reply"
        _finish
        return
    }
    # No `return` inside this catch: it is caught as a non-zero code, and
    # every refusal used to be shown as "reply could not be applied: "
    # with its reason lost.
    if {[catch {
        if {[dict exists $reply operation]} {
            set operation [::vfleetdeploy::_working_preflight [dict get $reply operation]]
            dict set ::vfleetdeploy::operations $op_id $operation
        }
        if {![dict exists $reply ok] || [dict get $reply ok] ni {1 true}} {
            set refusal "worker refused the action"
            if {[dict exists $reply error]} { set refusal [dict get $reply error] }
            if {[dict exists $reply diagnostic] && [dict get $reply diagnostic] ne $refusal} {
                append refusal "\nDiagnostic (shown here only, not journaled): [dict get $reply diagnostic]"
            }
            set message "Action refused or failed: $refusal"
        } else {
            set message "Action completed: $worker_action. Verify the journal and live health before continuing."
        }
    } why]} {
        set message "Deployment worker reply could not be applied: $why"
    }
    _render $message
    _finish
}

proc ::vfleetui::_launch_worker {argv json} {
    set channel [open |[list {*}$argv 2>@1] r+]
    fconfigure $channel -blocking 0 -encoding utf-8 -translation binary
    puts -nonewline $channel $json
    flush $channel
    chan close $channel write
    return $channel
}

# The worker's interpreter: VELES_TCLSH (how the Android engine and the
# packaged layouts name it), else tclsh on PATH (the nix desktop wrapper
# puts tcl-8_6 there), else a tclsh beside the running wish.
proc ::vfleetui::_tclsh {} {
    if {[info exists ::env(VELES_TCLSH)] && [file executable $::env(VELES_TCLSH)]} {
        return $::env(VELES_TCLSH)
    }
    foreach candidate {tclsh tclsh8.6 tclsh86} {
        set found [auto_execok $candidate]
        if {$found ne ""} { return [lindex $found 0] }
    }
    set beside [file dirname [info nameofexecutable]]
    foreach candidate {tclsh tclsh8.6 tclsh86.exe tclsh.exe} {
        if {[file executable [file join $beside $candidate]]} {
            return [file join $beside $candidate]
        }
    }
    return -code error "no tclsh was found for the deployment worker; set VELES_TCLSH"
}

proc ::vfleetui::worker_argv {} {
    variable libdir
    set script [file join $libdir fleetworker.tcl]
    if {![file isfile $script]} {
        return -code error "deployment worker script is missing: $script"
    }
    return [list [_tclsh] $script]
}

# Only the reviewed targets' registry rows, each with the credential the
# desktop itself would use: a row without its own token inherits the
# default one only when it names the same origin (bind_instance_inheriting's
# rule), so a same-origin local row no longer gets a 401 in the worker and
# no other instance's token leaves this process (F1-11).
proc ::vfleetui::_worker_registry {operation} {
    set registry {}
    foreach target [dict get $operation manifest targets] {
        set name [dict get $target instance]
        set spec [::vinst::get $name]
        if {$spec eq ""} { continue }
        set token ""
        if {[dict exists $spec token]} { set token [dict get $spec token] }
        if {$token eq "" && $::vapi::token ne "" &&
            [::vapi::_origin [dict get $spec url]] eq [::vapi::_origin $::vapi::base]} {
            set token $::vapi::token
        }
        dict set spec token $token
        dict set registry $name $spec
    }
    return $registry
}

proc ::vfleetui::_start_worker {action id} {
    variable worker
    variable worker_output
    variable worker_action
    variable operation
    variable worker_launcher
    set request [dict create operation $operation action $action node_id $id \
        journal_root [::vfleetjournal::root] registry [_worker_registry $operation]]
    set json [::vfleetjson::encode [::vfleetjson::operation_value $request]]
    if {[catch {worker_argv} argv] ||
        [catch {$worker_launcher $argv $json} channel]} {
        if {![info exists channel]} { set channel $argv }
        _render "Could not start deployment worker: $channel"
        _finish
        return 0
    }
    set worker $channel
    set worker_output ""
    set worker_action $action
    fileevent $worker readable ::vfleetui::_worker_finish
    return 1
}

proc ::vfleetui::mutate {action} {
    variable operation
    variable op_id
    if {$::vfleetui::busy} { return }
    if {$::vfleetui::manifest eq "" || $::vfleetui::preflight eq ""} {
        _error "build and review the exact manifest and preflight first"
        return
    }
    set fresh [expr {$operation eq ""}]
    if {$fresh} {
        if {$action ne "stage"} { _error "stage the canary before this action"; return }
    } else {
        set id [_action_target $operation $action]
        if {$action ni {continue finish-rollback abandon} && $id eq ""} { return }
    }
    set prompt [dict get {
        stage "Stage the canary using the reviewed signed release? This does not restart it."
        verify "Install the reviewed signed bundle, restart the configured unit, and verify its running revision?"
        continue "Revalidate health and stage the next declared target? The next target is not restarted until you verify it."
        rollback "Restore the retained prior core, restart the configured unit, and verify the recorded prior revision?"
        finish-rollback "Verify every installed target's rollback and close this operation as rolled back? Untouched targets remain unchanged."
        abandon "Close this operation without installing anything? Any previewed signed plan is cancelled. A target with an installation attempt must be rolled back instead."
    } $action]
    if {[tk_messageBox -type yesno -icon warning -title [::vi18n::mc {Review deployment}] -message $prompt] ne "yes"} { return }
    # The durable journal is created only AFTER the operator confirmed;
    # a declined prompt used to leave an orphan unfinished operation that
    # then blocked Resume (F1-05).
    if {$fresh} {
        if {[catch {::vfleetdeploy::apply $::vfleetui::manifest $::vfleetui::preflight} created]} {
            _render "Deployment refused: $created"
            return 0
        }
        set operation $created
        set op_id [dict get $operation id]
        set id [_action_target $operation $action]
        if {$id eq ""} { return 0 }
    }
    _busy
    return [_start_worker $action $id]
}

proc ::vfleetui::init {} {
    variable repository 77787777778777/veles
    variable release_tag v0.2.0
    variable release_commit ""
    variable workflow_commit ""
}

::vfleetui::init
