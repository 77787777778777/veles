# veles-desktop/lib/fleetjournal.tcl — local deployment operation journal.
#
# Plans and credentials live elsewhere. This file records only reviewed
# plan identity, declared targets, sanitized stage transitions and their
# timestamps. It never contains bearer tokens, SSH passwords or a shell.
source [file join [file dirname [file normalize [info script]]] api.tcl]
package require json
package require sha256

namespace eval ::vfleetjournal {
    variable root ""
}

# Owner-only on unix. Windows Tcl has no `file attributes -permissions`
# (FLEET-1: the unguarded call failed every deployment on the Windows
# desktop before its first stage); there the journal relies on the
# per-user data directory's ACL, like the session exports (chat.tcl).
proc ::vfleetjournal::_owner_only {path mode} {
    if {$::tcl_platform(platform) eq "unix"} {
        file attributes $path -permissions $mode
    }
}

proc ::vfleetjournal::configure {directory} {
    variable root
    set root $directory
    file mkdir $root
    _owner_only $root 0700
}

proc ::vfleetjournal::root {} {
    variable root
    return $root
}

proc ::vfleetjournal::_id {id} {
    if {![regexp {^[A-Za-z0-9_-]{1,80}$} $id]} {
        return -code error "invalid operation id"
    }
    return $id
}

proc ::vfleetjournal::_path {id} {
    variable root
    if {$root eq ""} { return -code error "deployment journal is not configured" }
    return [file join $root "[_id $id].json"]
}

proc ::vfleetjournal::_new_id {} {
    return [format "fd-%s-%s" [clock seconds] [string range [::sha2::sha256 -hex "[pid]-[clock clicks]"] 0 15]]
}

proc ::vfleetjournal::_write {op} {
    set path [_path [dict get $op id]]
    set tmp "$path.new[pid]"
    if {[catch {
        set f [open $tmp w]
        fconfigure $f -encoding utf-8
        puts -nonewline $f [::vfleetjson::encode [::vfleetjson::operation_value $op]]
        close $f
        _owner_only $tmp 0600
        file rename -force $tmp $path
    } error]} {
        catch {close $f}
        catch {file delete -force $tmp}
        return -code error "could not persist deployment journal: $error"
    }
    return $op
}

namespace eval ::vfleetjson {}
proc ::vfleetjson::obj {pairs} { return [list object $pairs] }
proc ::vfleetjson::arr {items} { return [list array $items] }
proc ::vfleetjson::str {value} { return [list string $value] }
proc ::vfleetjson::scalar {value} {
    if {$value in {true false}} { return [list raw $value] }
    if {[string is integer -strict $value] || [string is double -strict $value]} {
        return [list raw $value]
    }
    return [str $value]
}
proc ::vfleetjson::encode {value} {
    set type [lindex $value 0]
    set body [lindex $value 1]
    switch -- $type {
        object {
            set parts {}
            foreach {key item} $body {
                lappend parts "\"[::vapi::json_escape $key]\":[encode $item]"
            }
            return "\{[join $parts ,]\}"
        }
        array {
            set parts {}
            foreach item $body { lappend parts [encode $item] }
            return "\[[join $parts ,]\]"
        }
        string { return "\"[::vapi::json_escape $body]\"" }
        raw { return $body }
        default { error "invalid typed JSON value" }
    }
}

proc ::vfleetjson::is_dict {value} {
    if {[catch {dict size $value} size] || $size == 0} { return 0 }
    foreach {key item} $value {
        if {![regexp {^[A-Za-z_][A-Za-z0-9_.-]*$} $key]} { return 0 }
    }
    return 1
}

proc ::vfleetjson::string_keys {d keys} {
    set pairs {}
    foreach {key value} $d {
        if {$key in $keys} {
            lappend pairs $key [str $value]
        } else {
            lappend pairs $key [scalar $value]
        }
    }
    return [obj $pairs]
}

proc ::vfleetjson::schema_value {d {context operation}} {
    if {$context eq "string_map"} { return [string_keys $d {}] }
    if {$context in {target review download release observation event registry_spec}} {
        set pairs {}
        foreach {key value} $d {
            if {$key in {preview confirm_required exact prior_revision_verified recovery_observed install_started heartbeat_stale stale}} {
                lappend pairs $key [scalar $value]
            } elseif {$key eq "download" || $key eq "release"} {
                lappend pairs $key [schema_value $value release]
            } elseif {$key eq "preflight" || $key eq "observations"} {
                lappend pairs $key [schema_value $value observation]
            } else {
                lappend pairs $key [str $value]
            }
        }
        return [obj $pairs]
    }
    set pairs {}
    foreach {key value} $d {
        switch -- $key {
            release {
                lappend pairs $key [schema_value $value release]
            }
            operation - manifest - preflight {
                if {$key eq "manifest"} {
                    set nested {}
                    foreach {mk mv} $value {
                        if {$mk eq "targets"} {
                            set rows {}
                            foreach row $mv { lappend rows [schema_value $row target] }
                            lappend nested $mk [arr $rows]
                        } elseif {$mk eq "release"} {
                            lappend nested $mk [schema_value $mv release]
                        } else {
                            lappend nested $mk [scalar $mv]
                        }
                    }
                    lappend pairs $key [obj $nested]
                } elseif {$key eq "preflight"} {
                    set nested {}
                    foreach {pk pv} $value {
                        if {$pk eq "observations"} {
                            set rows {}
                            foreach row $pv { lappend rows [schema_value $row observation] }
                            lappend nested $pk [arr $rows]
                        } elseif {![dict exists $value kind] && ![dict exists $value observations]} {
                            # apply() maps preflight observations by node ID;
                            # the fresh preflight record has kind/observations.
                            lappend nested $pk [schema_value $pv observation]
                        } else {
                            lappend nested $pk [scalar $pv]
                        }
                    }
                    lappend pairs $key [obj $nested]
                } else {
                    lappend pairs $key [schema_value $value operation]
                }
            }
            targets - reviews {
                set nested {}
                foreach {nk nv} $value {
                    lappend nested $nk [schema_value $nv [expr {$key eq "targets" ? "target" : "review"}]]
                }
                lappend pairs $key [obj $nested]
            }
            registry {
                set nested {}
                foreach {nk nv} $value { lappend nested $nk [schema_value $nv registry_spec] }
                lappend pairs $key [obj $nested]
            }
            events {
                set rows {}
                foreach row $value { lappend rows [schema_value $row event] }
                lappend pairs $key [arr $rows]
            }
            target_order {
                lappend pairs $key [arr [lmap row $value {str $row}]]
            }
            observations {
                set rows {}
                foreach row $value { lappend rows [schema_value $row observation] }
                lappend pairs $key [arr $rows]
            }
            ok - ready - exact - preview - confirm_required - prior_revision_verified - recovery_observed - install_started - heartbeat_stale - stale {
                lappend pairs $key [scalar $value]
            }
            default {
                lappend pairs $key [str $value]
            }
        }
    }
    return [obj $pairs]
}

proc ::vfleetjson::dict_value {d} { return [schema_value $d operation] }
proc ::vfleetjson::operation_value {op} { return [dict_value $op] }

proc ::vfleetjournal::begin {manifest {preflight {}}} {
    set targets {}
    foreach target [dict get $manifest targets] {
        set id [dict get $target node_id]
        dict set targets $id [dict create node_id $id status reviewed]
    }
    set op [dict create \
        version 1 id [_new_id] kind fleet-deployment-operation \
        plan_sig [dict get $manifest plan_sig] \
        release [dict get $manifest release] \
        service_model [dict get $manifest service_model] \
        manifest $manifest \
        preflight $preflight \
        status reviewed targets $targets events {}]
    dict lappend op events [dict create \
        status reviewed detail {reviewed manifest; no changes made} \
        at [clock format [clock seconds] -format {%Y-%m-%dT%H:%M:%SZ} -gmt 1]]
    dict set op target_order [lmap target [dict get $manifest targets] {dict get $target node_id}]
    return [_write $op]
}

proc ::vfleetjournal::_event {operation event} {
    upvar 1 $operation value
    dict set event at [clock format [clock seconds] -format {%Y-%m-%dT%H:%M:%SZ} -gmt 1]
    dict lappend value events $event
}

proc ::vfleetjournal::get {id} {
    set path [_path $id]
    if {![file isfile $path]} { return -code error "no such deployment operation" }
    set f [open $path r]
    fconfigure $f -encoding utf-8
    set text [read $f]
    close $f
    set op [::json::json2dict $text]
    if {[_id [dict get $op id]] ne $id} {
        return -code error "journal operation identity does not match its filename"
    }
    return $op
}

proc ::vfleetjournal::all {} {
    variable root
    if {$root eq "" || ![file isdirectory $root]} { return {} }
    set rows {}
    foreach path [lsort [glob -nocomplain -directory $root -types f *.json]] {
        lappend rows [get [file rootname [file tail $path]]]
    }
    return $rows
}

proc ::vfleetjournal::unfinished {} {
    set rows {}
    foreach op [all] {
        if {[_field $op status] ni {completed rolled-back abandoned}} { lappend rows $op }
    }
    return $rows
}

proc ::vfleetjournal::_allowed {state} {
    set allowed [dict create \
        reviewed {staging failed} \
        staging {staged failed} \
        staged {restarting staging rollback-preview failed} \
        restarting {healthy failed} \
        healthy {remaining completed rollback-preview} \
        remaining {staging completed failed} \
        rollback-preview {rollback-staged failed} \
        rollback-staged {rollback-restarting failed} \
        rollback-restarting {rolled-back failed} \
        rolled-back {} completed {} failed {rollback-preview}]
    return [_field $allowed $state]
}

proc ::vfleetjournal::_field {d key {default ""}} {
    if {[dict exists $d $key]} { return [dict get $d $key] }
    return $default
}

proc ::vfleetjournal::transition {id node state detail {running_revision ""}} {
    set op [get $id]
    if {[_field $op status] in {completed rolled-back abandoned}} {
        return -code error "terminal deployment operation is immutable"
    }
    if {![dict exists [dict get $op targets] $node]} {
        return -code error "$node is not a declared target of this operation"
    }
    if {[regexp -nocase {bearer[[:space:]]|token[[:space:]]*=|password[[:space:]]*=|api[_-]?key[[:space:]]*=} $detail]} {
        return -code error "secret-shaped transport detail is refused in the deployment journal"
    }
    if {[string length $detail] > 2000} {
        return -code error "deployment journal detail exceeds 2000 characters"
    }
    set previous [dict get [dict get $op targets $node] status]
    if {$state ni [_allowed $previous]} {
        return -code error "illegal operation transition: $previous -> $state"
    }
    dict set op targets $node status $state
    dict set op targets $node updated_at [clock format [clock seconds] -format {%Y-%m-%dT%H:%M:%SZ} -gmt 1]
    if {$running_revision ne ""} {
        dict set op targets $node running_revision $running_revision
    }
    # A target's terminal state is not the fleet's terminal state. In
    # particular, restoring one target must not prevent repairing another.
    # Operation-level completion is reserved for ::complete after every
    # declared target has an allowed terminal state.
    if {$state ni {completed rolled-back}} {
        dict set op status $state
    }
    _event op [dict create node_id $node status $state detail $detail]
    return [_write $op]
}

proc ::vfleetjournal::staged {op node review running_revision} {
    if {[catch {dict size $review} size] || $size == 0 ||
        [string trim [_field $review plan_id]] eq ""} {
        return -code error "refusing to journal an empty signed release review"
    }
    if {[_field $review preview] ni {1 true} ||
        [_field $review confirm_required] ni {1 true} ||
        [_field $review action] ne "install-signed-bundle"} {
        return -code error "refusing to journal an unconfirmed release preview"
    }
    set download [_field $review download]
    foreach key {archive_sha256 manifest_sha256} {
        if {![regexp {^[a-f0-9]{64}$} [_field $download $key]]} {
            return -code error "signed release review lacks a valid $key"
        }
    }
    foreach key {repository tag commit workflow_sha} {
        if {[_field $download $key] ne [_field [dict get $op manifest release] $key]} {
            return -code error "signed release review does not match the manifest $key"
        }
    }
    if {![regexp {^[a-f0-9]{40}$} $running_revision] &&
        $running_revision ni {unreported-development-build unreported-dirty-build unreported}} {
        return -code error "staged target has an invalid observed running revision"
    }
    set safe_review [dict create \
        preview true confirm_required true action install-signed-bundle \
        plan_id [_field $review plan_id] \
        verification [_field $review verification] \
        review_expires_seconds [_field $review review_expires_seconds] \
        previous_artifact_sha256 [_field $review previous_artifact_sha256] \
        download [dict create \
            archive_sha256 [_field $download archive_sha256] \
            manifest_sha256 [_field $download manifest_sha256] \
            repository [_field $download repository] \
            tag [_field $download tag] \
            commit [_field $download commit] \
            workflow_sha [_field $download workflow_sha]]]
    if {![regexp {^[a-f0-9]{64}$} [_field $review previous_artifact_sha256]]} {
        return -code error "signed release review lacks the original rollback artifact digest"
    }
    dict set op reviews $node $safe_review
    dict set op targets $node running_revision $running_revision
    _event op [dict create node_id $node status review detail {signed release review retained for restart confirmation}]
    return [_write $op]
}

# The signed-download request id is recorded BEFORE the preview request,
# so an operation interrupted mid-preview can still cancel its plan.
proc ::vfleetjournal::plan_requested {op node request_id} {
    if {![regexp {^[A-Za-z0-9_.-]{1,128}$} $request_id]} {
        return -code error "invalid signed-download request id"
    }
    if {[_field [dict get $op targets $node] status] ne "staging"} {
        return -code error "a signed plan can be requested only while staging"
    }
    dict set op targets $node plan_request $request_id
    _event op [dict create node_id $node status plan-requested detail {signed release preview requested}]
    return [_write $op]
}

# Terminal close of an operation that installed nothing. Distinct from
# rolled-back: nothing was restored, because nothing was changed.
proc ::vfleetjournal::abandon {id detail} {
    set op [get $id]
    if {[_field $op status] in {completed rolled-back abandoned}} {
        return -code error "terminal deployment operation is immutable"
    }
    if {[regexp -nocase {bearer[[:space:]]|token[[:space:]]*=|password[[:space:]]*=|api[_-]?key[[:space:]]*=} $detail]} {
        return -code error "secret-shaped transport detail is refused in the deployment journal"
    }
    dict for {node target} [dict get $op targets] {
        if {[dict exists $target install_started] && [dict get $target install_started] in {1 true}} {
            return -code error "target $node had an installation attempt; only rollback can close this operation"
        }
    }
    dict set op status abandoned
    _event op [dict create status abandoned detail $detail]
    return [_write $op]
}

proc ::vfleetjournal::mark_install_started {op node} {
    if {[_field [dict get $op targets $node] status] ni {staged restarting}} {
        return -code error "installation cannot start from this target state"
    }
    dict set op targets $node install_started 1
    dict set op targets $node install_started_at [clock format [clock seconds] -format {%Y-%m-%dT%H:%M:%SZ} -gmt 1]
    _event op [dict create node_id $node status install-started detail {signed installation confirmation started}]
    return [_write $op]
}

proc ::vfleetjournal::recovery_observed {op node running_revision} {
    if {[string trim $running_revision] eq ""} {
        return -code error "cannot record recovery without a running revision"
    }
    dict set op targets $node recovery_observed 1
    dict set op targets $node recovery_revision $running_revision
    dict set op targets $node recovery_observed_at [clock format [clock seconds] -format {%Y-%m-%dT%H:%M:%SZ} -gmt 1]
    _event op [dict create node_id $node status recovery-observed detail {fresh health reconciled after interruption}]
    return [_write $op]
}

proc ::vfleetjournal::canary_blocked {op requested canary detail} {
    if {[string length $detail] > 500} { set detail [string range $detail 0 499] }
    _event op [dict create node_id $requested status canary-blocked detail "canary $canary: $detail"]
    return [_write $op]
}

proc ::vfleetjournal::verify_target {op node running_revision} {
    if {[string trim $running_revision] eq ""} {
        return -code error "cannot journal an empty running revision"
    }
    dict set op targets $node running_revision $running_revision
    dict set op targets $node verified_at [clock format [clock seconds] -format {%Y-%m-%dT%H:%M:%SZ} -gmt 1]
    _event op [dict create node_id $node status verified detail {fresh terminal health and running revision verified}]
    return [_write $op]
}

proc ::vfleetjournal::rollback_verified {op node running_revision exact} {
    dict set op targets $node running_revision $running_revision
    dict set op targets $node prior_revision_verified [expr {$exact ? 1 : 0}]
    _event op [dict create node_id $node status rollback-verified detail {prior running revision recorded before terminal rollback}]
    return [_write $op]
}

proc ::vfleetjournal::complete {id status {verify_proc ""}} {
    if {$status ni {completed rolled-back}} {
        return -code error "terminal status must be completed or rolled-back"
    }
    set op [get $id]
    if {$status eq "rolled-back" && [_field $op status] eq "rolled-back"} {
        return $op
    }
    if {[_field $op status] in {completed rolled-back abandoned}} {
        return -code error "terminal deployment operation is immutable"
    }
    set required [expr {$status eq "completed" ? "healthy" : "rolled-back"}]
    set invalid ""
    set unproven {}
    dict for {node target} [dict get $op targets] {
        set state [dict get $target status]
        if {$state eq $required} {
            # A restored target closes only after rollback_verified ran,
            # and an unproven prior revision is named, never folded into
            # "all declared targets verified" (F1-06).
            if {$status eq "rolled-back"} {
                if {![dict exists $target prior_revision_verified]} {
                    set invalid "target $node has no recorded rollback verification"
                    break
                }
                if {[dict get $target prior_revision_verified] ni {1 true}} {
                    lappend unproven $node
                }
            }
            continue
        }
        # A rollback may finish only after an independent health check of
        # every declared target that was not itself restored. Never turn
        # an untouched target into a claimed rollback.
        if {$status eq "rolled-back" && $state eq "healthy"} {
            if {$verify_proc eq ""} {
                set invalid "target $node needs fresh rollback health verification"
                break
            }
            if {[catch {$verify_proc $op $node} verified]} {
                set invalid "target $node failed fresh rollback verification: $verified"
                break
            }
            set op $verified
            continue
        }
        if {$status eq "rolled-back" && $state in {reviewed remaining failed} &&
            (![dict exists $target install_started] ||
             [dict get $target install_started] ni {1 true})} {
            # The target was never installed (or its staging failed before
            # any installation); no rollback proof is required.
        } else {
            set invalid "target $node is not verified for $status (state $state)"
            break
        }
    }
    if {$invalid ne ""} { return -code error $invalid }
    dict set op status $status
    set detail {all declared targets verified}
    if {[llength $unproven]} {
        dict set op rollback_unproven [join $unproven ,]
        set detail "rollback closed; restored and healthy, but the prior source revision of [join $unproven {, }] was unreported and is not proven"
    }
    _event op [dict create status $status detail $detail]
    return [_write $op]
}
