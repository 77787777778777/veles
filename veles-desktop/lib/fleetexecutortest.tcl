#!/usr/bin/env wish
# F1.4 executor: fake transport only; no network, SSH or real installation.
package require json
package require sha256
set dir [file dirname [file normalize [info script]]]
source [file join $dir paths.tcl]
source [file join $dir instances.tcl]
source [file join $dir instpane.tcl]
source [file join $dir fleetdeploy.tcl]
source [file join $dir fleetfixtures.tcl]
set pass 0
set fail 0
proc ok {name condition} {
    if {[uplevel 1 [list expr $condition]]} {
        incr ::pass; puts "  ok $name"
    } else {
        incr ::fail; puts "  FAIL $name"
    }
}
proc rejects {name command fragment} {
    if {[catch {uplevel 1 $command} error]} {
        if {[string first $fragment $error] < 0} { puts "    unexpected refusal: $error" }
        ok $name {[string first $fragment $error] >= 0}
    } else {
        ok $name 0; puts "    expected refusal containing: $fragment"
    }
}
# The node map is what the dialog builds from a real /api/fleet body.
proc fixture_nodes {} {
    return [::vfleetdeploy::nodes_from_inventory [::json::json2dict \
        [::fleetfix::fleet_json {{node-a alpha 0.1.6} {node-b beta 0.1.5}}]]]
}
proc fixture_instances {} {
    return [dict create alpha [dict create node_id node-a url https://alpha.invalid token alpha-secret host alpha@example.invalid unit veles-gateway] beta [dict create node_id node-b url https://beta.invalid token beta-secret host beta@example.invalid unit veles-gateway]]
}
proc release {} {
    return [dict create repository 77787777778777/veles tag v0.1.6 commit [string repeat a 40] workflow_sha [string repeat b 40]]
}
proc observations {} {
    return [dict create node-a [dict create status ok version 0.1.6 revision [string repeat c 40]] node-b [dict create status ok version 0.1.5 revision [string repeat d 40]]]
}
namespace eval ::vfleetdeploy {
    variable nodes [fixture_nodes]
    variable instances [fixture_instances]
}
set root [file join /tmp veles-fleet-executor-[pid]-[clock clicks]]
::vfleetjournal::configure $root
set ::transport_log {}
set ::health [observations]
set ::bad_health {}
set ::install_failure 0
set ::restart_failure 0
set ::current_artifact [string repeat 8 64]
set ::previous_artifact [string repeat 9 64]
# The gateway keeps ONE pending signed plan per node and answers a second
# preview with 409 until it is confirmed or cancelled (desktop_install.rs).
# Modelled when ::model_slot is set; the older scenarios below stage many
# operations without closing them and predate the model.
set ::model_slot 0
array set ::pending {}
set ::preview_failure 0
set ::managed_core 1
proc json_text {typed} { return [string trim [lindex $typed 1] \"] }
proc fake_transport {action target payload} {
    lappend ::transport_log $action [dict get $target node_id]
    set id [dict get $target node_id]
    if {$action eq "health"} {
        if {[dict exists $::bad_health $id]} { return [dict get $::bad_health $id] }
        return [dict get $::health $id]
    }
    if {$action eq "install-state"} {
        set digest [expr {$::managed_core ? $::current_artifact : ""}]
        return [::json::json2dict [::fleetfix::install_state_json $digest]]
    }
    if {$action eq "download-preview"} {
        set release [::release]
        if {![dict exists $payload release] ||
            [lindex [dict get $payload release] 1] ne "\"https://github.com/[dict get $release repository]/releases/download/[dict get $release tag]/\""} {
            error "release URL does not match the reviewed repository and tag"
        }
        set request [json_text [dict get $payload request_id]]
        if {$::model_slot && [info exists ::pending($id)] && $::pending($id) ne ""} {
            error "HTTP 409: {\"ok\":false,\"error\":\"a signed-download review is still awaiting confirmation or cancellation\"}"
        }
        set ::pending($id) $request
        if {$::preview_failure} { error "HTTP 502: signed release preview failed" }
        return [dict create preview true confirm_required true action install-signed-bundle plan_id $request verification {fixture cosign verification} review_expires_seconds 3600 credential_debug {Bearer fixture-must-not-persist} download [dict create archive_sha256 [string repeat e 64] manifest_sha256 [string repeat f 64] repository [dict get $release repository] tag [dict get $release tag] commit [dict get $release commit] workflow_sha [dict get $release workflow_sha]]]
    }
    if {$action eq "download-cancel"} {
        set plan [json_text [dict get $payload plan_id]]
        lappend ::cancelled $plan
        if {[info exists ::pending($id)] && $::pending($id) eq $plan} { set ::pending($id) "" }
        return [::json::json2dict [::fleetfix::cancel_json]]
    }
    if {$action eq "download-confirm"} {
        if {$::install_failure} { error "simulated installer refusal" }
        set ::pending($id) ""
        set ::previous_artifact $::current_artifact
        set ::current_artifact [string repeat 6 64]
        return [dict create ok true action install-signed-bundle component [dict create name core] restart_required true]
    }
    if {$action eq "restart"} {
        if {$::restart_failure} { error "simulated SSH restart failure" }
        dict set ::health $id revision [string repeat a 40]
        return [dict create restarted 1]
    }
    if {$action eq "rollback-preview"} {
        return [dict create preview true confirm_required true component core plan_sig "rollback-$id" current_artifact_sha256 $::current_artifact previous_artifact_sha256 $::previous_artifact]
    }
    if {$action eq "rollback-confirm"} {
        return [dict create ok true action rollback component [dict create name core]]
    }
    error "unexpected transport action $action"
}
set ::vfleetdeploy::transport fake_transport
# The production wait is bounded at 30 seconds. Fixtures have no startup
# latency, so exercise the same health validation without wall-clock sleeps.
rename ::vfleetdeploy::_await_health ::vfleetdeploy::_await_health_real
proc ::vfleetdeploy::_await_health {target {attempts 30}} {
    return [_fresh_health $target]
}
set m [::vfleetdeploy::manifest [list node-a node-b] [release] native-bundle]
set before [::vfleetdeploy::preflight $m [observations]]
set op [::vfleetdeploy::apply $m $before]
set id [dict get $op id]
ok "reviewed apply creates a journal without changing a target" {
    [dict get $op status] eq "reviewed" &&
    $::transport_log eq ""
}
set op [::vfleetdeploy::stage $op node-a]
ok "canary staging performs health, the installer-state baseline, then signed preview; no install or restart" {
    $::transport_log eq {health node-a install-state node-a download-preview node-a} &&
    [dict get $op targets node-a status] eq "staged" &&
    [dict exists $op reviews node-a]
}
set staged_roundtrip [::vfleetdeploy::load [dict get $op id]]
ok "staged journal round trip preserves manifest identity and mapped preflight" {
    [dict get $staged_roundtrip manifest plan_sig] eq [dict get $m plan_sig] &&
    [dict get $staged_roundtrip preflight node-a revision] eq [string repeat c 40] &&
    [dict get $staged_roundtrip release commit] eq [string repeat a 40]
}
ok "signed preview retention drops unexpected credential-shaped fields" {
    ![dict exists $op reviews node-a credential_debug] &&
    ![string match *fixture-must-not-persist* [::vfleetjson::encode [::vfleetjson::operation_value $op]]]
}
rejects "continuation cannot bypass canary verification" {
    ::vfleetdeploy::continue $op
} "canary must be healthy"
set order_op [::vfleetdeploy::apply $m $before]
rejects "later target cannot be staged before canary health" {
    ::vfleetdeploy::stage $order_op node-b
} "canary target node-a must be healthy"
set failed_order_op [::vfleetdeploy::apply $m $before]
set failed_order_op [::vfleetdeploy::stage $failed_order_op node-a]
set failed_order_op [::vfleetdeploy::verify $failed_order_op node-a]
set ::bad_health [dict create node-a [dict create status fail stale 0 revision [string repeat a 40]]]
rejects "a failed canary cannot be bypassed by direct later staging" {
    ::vfleetdeploy::stage $failed_order_op node-b
} "target health verification failed"
set failed_saved [::vfleetjournal::get [dict get $failed_order_op id]]
ok "failed canary does not mark the untouched later target failed" {
    [dict get $failed_saved targets node-b status] eq "reviewed" &&
    ![dict exists $failed_saved targets node-b install_started]
}
set ::bad_health {}
set redacted [::vfleetdeploy::_safe_error "ssh password authentication failed token=super-secret"]
ok "transport diagnostics are redacted before journaling" {
    $redacted eq "transport authentication or credential diagnostic (redacted)" &&
    ![string match *super-secret* $redacted]
}
set op [::vfleetdeploy::verify $op node-a]
ok "canary verify installs, restarts and requires the exact reviewed revision" {
    [lrange $::transport_log end-5 end] eq {download-confirm node-a restart node-a health node-a} &&
    [dict get $op targets node-a status] eq "healthy"
}
set ::health [observations]
set drift_artifact_op [::vfleetdeploy::apply $m $before]
set drift_artifact_op [::vfleetdeploy::stage $drift_artifact_op node-a]
set drift_artifact_op [::vfleetdeploy::verify $drift_artifact_op node-a]
set ::current_artifact [string repeat 5 64]
set ::previous_artifact [string repeat 7 64]
set before_artifact_drift [llength $::transport_log]
rejects "rollback refuses a retained artifact retained by a later installation" {
    ::vfleetdeploy::rollback $drift_artifact_op node-a
} "retained rollback artifact changed"
ok "rollback artifact drift never confirms or restarts" {
    [lrange $::transport_log $before_artifact_drift end] eq {rollback-preview node-a}
}
set ::current_artifact [string repeat 8 64]
set ::previous_artifact [string repeat 9 64]
# The exact call sequence, from a reset log (F1-08): continue's own fresh
# canary check, the next target's health, then staging's canary and
# target checks before the baseline and the preview.
set ::transport_log {}
set op [::vfleetdeploy::continue $op]
ok "continuation revalidates health and stages only the next target" {
    [dict get $op targets node-a status] eq "healthy" &&
    [dict get $op targets node-b status] eq "staged" &&
    $::transport_log eq {health node-a health node-b health node-a health node-b install-state node-b download-preview node-b}
}
set op [::vfleetdeploy::verify $op node-b]
set op [::vfleetdeploy::continue $op]
ok "all healthy targets complete the operation" {
    [dict get $op status] eq "completed" &&
    [dict get $op targets node-b status] eq "healthy"
}
set persisted [::vfleetjournal::get $id]
ok "journal reload retains terminal operation and target states" {
    [dict get $persisted status] eq "completed" &&
    [dict get $persisted targets node-a status] eq "healthy" &&
    [dict get $persisted targets node-b status] eq "healthy"
}

# A failed canary never admits the remaining target, even if the installer
# reports that the bundle was written successfully.
set ::transport_log {}
set ::health [observations]
set op [::vfleetdeploy::apply $m $before]
set op [::vfleetdeploy::stage $op node-a]
dict set ::health node-a revision [string repeat 9 40]
# The fixture's restart deliberately assigns the desired revision, so use a
# failing health observation to exercise the verification refusal itself.
set ::bad_health [dict create node-a [dict create status error revision [string repeat a 40]]]
rejects "unhealthy canary verification refuses continuation" {
    ::vfleetdeploy::verify $op node-a
} "not fresh and healthy"
rejects "failed canary cannot stage another target" {
    ::vfleetdeploy::continue $op
} "canary must be healthy"
ok "failed canary did not stage node-b" {
    [lsearch -exact $::transport_log node-b] < 0
}

# A successful canary followed by an unhealthy remaining target cannot be
# admitted by the old canary result.
set ::bad_health {}
set ::transport_log {}
set ::health [observations]
set op [::vfleetdeploy::apply $m $before]
set op [::vfleetdeploy::stage $op node-a]
set op [::vfleetdeploy::verify $op node-a]
set ::bad_health [dict create node-b [dict create status error revision [string repeat d 40]]]
rejects "continuation revalidates the next target health" {
    ::vfleetdeploy::continue $op
} "not fresh and healthy"
ok "unhealthy next target was not staged" {
    [dict get $op targets node-b status] eq "reviewed"
}

# Rollback has its own reviewed preview/apply and verifies the old revision.
set ::bad_health {}
set ::transport_log {}
set ::health [observations]
set op [::vfleetdeploy::apply $m $before]
set op [::vfleetdeploy::stage $op node-a]
set op [::vfleetdeploy::verify $op node-a]
# Simulate the fixture service running the old artifact after rollback.
rename fake_transport fake_transport_deploy
set ::rolling_back 0
proc fake_transport {action target payload} {
    if {$action eq "restart" && $::rolling_back} {
        dict set ::health [dict get $target node_id] revision [string repeat c 40]
        set ::rolling_back 0
    }
    lappend ::transport_log $action [dict get $target node_id]
    if {$action eq "restart"} { return [dict create restarted 1] }
    return [fake_transport_deploy $action $target $payload]
}
set ::rolling_back 1
set op [::vfleetdeploy::rollback $op node-a]
ok "rollback restores and verifies the recorded prior revision" {
    [dict get $op targets node-a status] eq "rolled-back" &&
    [dict get $::health node-a revision] eq [string repeat c 40]
}
ok "rollback used reviewed preview and apply before restart" {
    [lsearch -exact $::transport_log rollback-preview] >= 0 &&
    [lsearch -exact $::transport_log rollback-confirm] >= 0 &&
    [lsearch -exact $::transport_log restart] >= 0
}

# A durable journal alone cannot supply the target's opaque signed review.
# It is retained in the working operation, and verification fails closed
# rather than treating a state row as permission to install.
set ::transport_log {}
set ::health [observations]
set op [::vfleetdeploy::apply $m $before]
set op [::vfleetdeploy::stage $op node-a]
set saved $op
dict unset saved reviews node-a
rejects "a journal without its retained signed review cannot install" {
    ::vfleetdeploy::verify $saved node-a
} "no retained signed release review"

set ::transport_log {}
set ::health [observations]
set ::vfleetdeploy::transport fake_transport_deploy
set op [::vfleetdeploy::apply $m $before]
set op [::vfleetdeploy::stage $op node-a]
set op [::vfleetdeploy::verify $op node-a]
set op [::vfleetdeploy::stage $op node-b]
set ::vfleetdeploy::transport fake_transport
set ::rolling_back 1
set op [::vfleetdeploy::rollback $op node-a]
rejects "rollback completion cannot ignore an unstaged remaining target" {
    ::vfleetjournal::complete [dict get $op id] rolled-back
} "not verified for rolled-back"
ok "restoring the canary did not mark an untouched target rolled back" {
    [dict get $op targets node-b status] eq "staged" &&
    [dict get $op targets node-a status] eq "rolled-back"
}

# Existing planning-suite refusal still guards a mismatched preflight.
rejects "apply refuses a preflight for another manifest" {
    ::vfleetdeploy::apply $m [dict merge $before [dict create plan_sig [string repeat 0 64]]]
} "no reviewed preflight"

# An installer failure is a durable target failure, not an implicit retry
# or a healthy deployment. The working operation must be recoverable from
# the executor's operation map after the caller catches the error.
set ::vfleetdeploy::transport fake_transport_deploy
set ::transport_log {}
set ::health [observations]
set op [::vfleetdeploy::apply $m $before]
set op [::vfleetdeploy::stage $op node-a]
set ::install_failure 1
rejects "installer refusal is reported as a failed verification" {
    ::vfleetdeploy::verify $op node-a
} "deployment verification failed"
set failed [dict get $::vfleetdeploy::operations [dict get $op id]]
ok "installer refusal is persisted and blocks continuation" {
    [dict get $failed targets node-a status] eq "failed" &&
    [dict exists $failed failure] &&
    [catch {::vfleetdeploy::continue $failed} refusal] &&
    [string first "canary must be healthy" $refusal] >= 0
}
set failed_resume_id [dict get $failed id]
set failed_resumed [::vfleetdeploy::resume $failed_resume_id $::health]
ok "failed installation can be resumed for explicit rollback after process restart" {
    [dict get $failed_resumed targets node-a status] eq "failed" &&
    [dict get $failed_resumed targets node-a install_started] in {1 true}
}
set ::install_failure 0

# A healthy response with the wrong source revision is not a deployment
# success. It must leave a failed journal entry and no next-target stage.
set ::transport_log {}
set ::health [observations]
set op [::vfleetdeploy::apply $m $before]
set op [::vfleetdeploy::stage $op node-a]
# Return a healthy process with a different revision after the restart.
set ::bad_health [dict create node-a [dict create status ok stale 0 revision [string repeat 9 40]]]
set ::vfleetdeploy::transport fake_transport
rejects "wrong running revision refuses deployment success" {
    ::vfleetdeploy::verify $op node-a
} "deployment verification failed"
ok "wrong revision leaves a durable failed target" {
    [dict get [dict get $::vfleetdeploy::operations [dict get $op id]] targets node-a status] eq "failed"
}

set ::bad_health {}
set ::vfleetdeploy::transport fake_transport_deploy
set ::health [observations]
set op [::vfleetdeploy::apply $m $before]
set op [::vfleetdeploy::stage $op node-a]
set op [::vfleetdeploy::verify $op node-a]
set op [::vfleetdeploy::continue $op]
set op [::vfleetdeploy::verify $op node-b]
set ::vfleetdeploy::transport fake_transport
set ::rolling_back 1
set op [::vfleetdeploy::rollback $op node-a]
rejects "mixed rollout cannot finish without fresh remaining-target verification" {
    ::vfleetjournal::complete [dict get $op id] rolled-back
} "needs fresh rollback health verification"
set op [::vfleetdeploy::complete $op rolled-back]
ok "mixed rollout terminal rollback freshly verifies healthy remaining target" {
    [dict get $op status] eq "rolled-back" &&
    [dict get $op targets node-b running_revision] eq [string repeat a 40] &&
    [lsearch -exact $::transport_log health] >= 0
}
set final_rollback [::vfleetdeploy::finish_rollback $op]
ok "finish rollback is idempotent for an already terminal rolled-back operation" {
    [dict get $final_rollback status] eq "rolled-back"
}

set ::vfleetdeploy::transport fake_transport_deploy
set ::health [observations]
set partial_op [::vfleetdeploy::apply $m $before]
set partial_op [::vfleetdeploy::stage $partial_op node-a]
set partial_op [::vfleetdeploy::verify $partial_op node-a]
set ::rolling_back 1
set ::vfleetdeploy::transport fake_transport
set partial_op [::vfleetdeploy::rollback $partial_op node-a]
set partial_done [::vfleetdeploy::finish_rollback $partial_op]
ok "finish rollback preserves an untouched reviewed target and closes the operation" {
    [dict get $partial_done status] eq "rolled-back" &&
    [dict get $partial_done targets node-a status] eq "rolled-back" &&
    [dict get $partial_done targets node-b status] eq "reviewed"
}

# Cold recovery reads the exact plan and signed preview from the journal;
# it never assumes the server's in-memory pending plan survived a restart.
set ::bad_health {}
set ::health [observations]
set ::transport_log {}
set ::rolling_back 0
set ::restart_failure 0
set ::install_failure 0
set ::vfleetdeploy::transport fake_transport_deploy
set untouched_op [::vfleetdeploy::apply $m $before]
set untouched_op [::vfleetdeploy::stage $untouched_op node-a]
set before_untouched_rollback [llength $::transport_log]
rejects "rollback cannot restart an untouched staged target" {
    ::vfleetdeploy::rollback $untouched_op node-a
} "was not installed"
ok "the refused untouched rollback left the journal staged and called nothing" {
    [dict get [::vfleetjournal::get [dict get $untouched_op id]] targets node-a status] eq "staged" &&
    [llength $::transport_log] == $before_untouched_rollback
}
set recover_op [::vfleetdeploy::apply $m $before]
set recover_op [::vfleetdeploy::stage $recover_op node-a]
set recover_id [dict get $recover_op id]
set ::vfleetdeploy::operations [dict create]
set ::transport_log {}
set recovered [::vfleetdeploy::resume $recover_id [observations]]
ok "cold resume retains exact manifest and original rollback observation" {
    [dict get $recovered manifest plan_sig] eq [dict get $m plan_sig] &&
    [dict get $recovered preflight node-a revision] eq [string repeat c 40] &&
    [string match "${recover_id}.*" [dict get $recovered reviews node-a plan_id]] &&
    $::transport_log eq ""
}
set before_recovery_calls [llength $::transport_log]
set old_recovery_plan [dict get $recovered reviews node-a plan_id]
set ::model_slot 1
set ::cancelled {}
set recovered [::vfleetdeploy::stage $recovered node-a]
set ::model_slot 0
ok "cold staged resume cancels the old pending plan, then obtains a fresh preview (no 409)" {
    [lrange $::transport_log $before_recovery_calls end] eq {health node-a install-state node-a download-cancel node-a download-preview node-a} &&
    $::cancelled eq [list $old_recovery_plan] &&
    [dict get $recovered reviews node-a plan_id] ne $old_recovery_plan &&
    [dict get $recovered targets node-a status] eq "staged"
}
set recovered [::vfleetdeploy::verify $recovered node-a]
ok "resumed signed plan can be explicitly confirmed and verified" {
    [dict get $recovered targets node-a status] eq "healthy"
}
set healthy_after_upgrade [observations]
dict set healthy_after_upgrade node-a version 0.1.7
dict set healthy_after_upgrade node-a revision [string repeat a 40]
set healthy_reload [::vfleetdeploy::resume $recover_id $healthy_after_upgrade]
ok "a healthy target may resume after its package version changes" {
    [dict get $healthy_reload targets node-a status] eq "healthy"
}
set ::vfleetdeploy::operations [dict create]
set ::health [observations]
set drift_op [::vfleetdeploy::apply $m $before]
set drift_op [::vfleetdeploy::stage $drift_op node-a]
set drift [observations]
dict set drift node-a revision [string repeat a 40]
rejects "staged resume refuses a target that already changed source" {
    ::vfleetdeploy::resume [dict get $drift_op id] $drift
} "source changed after interruption"
set ::vfleetdeploy::operations [dict create]
set ::health [observations]
set pending_op [::vfleetdeploy::apply $m $before]
set pending_op [::vfleetdeploy::stage $pending_op node-a]
::vfleetjournal::transition [dict get $pending_op id] node-a restarting {simulated installation attempt}
set pending_op [::vfleetjournal::mark_install_started [::vfleetjournal::get [dict get $pending_op id]] node-a]
set before_interrupted_resume [llength $::transport_log]
set interrupted_resumed [::vfleetdeploy::resume [dict get $pending_op id] [observations]]
ok "an interrupted restart is recoverable without replaying installation" {
    [dict get $interrupted_resumed targets node-a status] eq "restarting" &&
    [dict get $interrupted_resumed targets node-a install_started] in {1 true} &&
    [llength $::transport_log] == $before_interrupted_resume
}
set rollback_gap_op [::vfleetdeploy::apply $m $before]
set rollback_gap_op [::vfleetdeploy::stage $rollback_gap_op node-a]
set rollback_gap_op [::vfleetdeploy::verify $rollback_gap_op node-a]
set rollback_gap_id [dict get $rollback_gap_op id]
set ::rolling_back 1
set ::vfleetdeploy::transport fake_transport
dict set ::health node-a revision [string repeat c 40]
::vfleetjournal::transition $rollback_gap_id node-a rollback-preview {simulate reviewed restore}
::vfleetjournal::transition $rollback_gap_id node-a rollback-staged {simulate retained restore}
::vfleetjournal::transition $rollback_gap_id node-a rollback-restarting {simulate restored process before terminal write}
set rollback_gap [::vfleetjournal::get $rollback_gap_id]
set rollback_gap [::vfleetjournal::rollback_verified $rollback_gap node-a [string repeat c 40] 1]
set before_gap_resume [llength $::transport_log]
set gap_resumed [::vfleetdeploy::resume $rollback_gap_id [observations]]
set gap_closed [::vfleetdeploy::rollback $gap_resumed node-a]
ok "verified rollback interruption reconciles without repeating restore or restart" {
    [dict get $gap_closed targets node-a status] eq "rolled-back" &&
    [lsearch -glob [lrange $::transport_log $before_gap_resume end] {rollback-*}] < 0 &&
    [lsearch -exact [lrange $::transport_log $before_gap_resume end] restart] < 0 &&
    [dict get $gap_closed targets node-a running_revision] eq [string repeat c 40]
}

# A failed SSH restart is never converted into a healthy deployment.
set ::bad_health {}
set ::restart_failure 0
set ::vfleetdeploy::transport fake_transport_deploy
set ::health [observations]
set op [::vfleetdeploy::apply $m $before]
set op [::vfleetdeploy::stage $op node-a]
set ::restart_failure 1
rejects "SSH restart failure refuses deployment success" {
    ::vfleetdeploy::verify $op node-a
} "deployment verification failed"
ok "restart failure is durable and blocks the remaining target" {
    [dict get [dict get $::vfleetdeploy::operations [dict get $op id]] targets node-a status] eq "failed" &&
    [catch {::vfleetdeploy::continue [dict get $::vfleetdeploy::operations [dict get $op id]]} refusal] &&
    [string first "canary must be healthy" $refusal] >= 0
}
set ::restart_failure 0

set ::bad_health {}
set ::vfleetdeploy::transport fake_transport_deploy
set ::health [observations]
dict set ::health node-a revision unreported-development-build
set dev_before [::vfleetdeploy::preflight $m $::health]
set op [::vfleetdeploy::apply $m $dev_before]
set op [::vfleetdeploy::stage $op node-a]
set op [::vfleetdeploy::verify $op node-a]
set ::vfleetdeploy::transport fake_transport
set ::rolling_back 1
dict set ::health node-a revision unreported-development-build
# A development binary has no old SHA to compare. The rollback journal
# must distinguish health restoration from exact-source verification.
proc fake_transport_rollback_unknown {action target payload} {
    if {$action eq "restart"} {
        dict set ::health [dict get $target node_id] revision unreported-development-build
        return [dict create restarted 1]
    }
    return [fake_transport_deploy $action $target $payload]
}
set ::vfleetdeploy::transport fake_transport_rollback_unknown
set op [::vfleetdeploy::rollback $op node-a]
ok "unknown prior revision is reported as unverified, not exact proof" {
    [dict get $op targets node-a status] eq "rolled-back" &&
    [dict get $op targets node-a prior_revision_verified] == 0 &&
    [dict get $op targets node-a running_revision] eq "unreported-development-build" &&
    [string first "cannot be proven" [dict get [lindex [dict get $op events] end] detail]] >= 0
}
set unproven_close [::vfleetdeploy::finish_rollback $op]
ok "closing an unproven rollback names it instead of claiming every target verified (F1-06)" {
    [dict get $unproven_close status] eq "rolled-back" &&
    [dict get $unproven_close rollback_unproven] eq "node-a" &&
    [string first "not proven" [dict get [lindex [dict get $unproven_close events] end] detail]] >= 0 &&
    [string first "all declared targets verified" [dict get [lindex [dict get $unproven_close events] end] detail]] < 0
}

# ── Real wire shapes through the production transport (F1-01) ────────
# Only the HTTP layer is replaced: ::vapi::get answers with the body a
# real gateway sends, and _transport/get_json/_fresh_health do the rest.
::vapi::bind_instance alpha https://alpha.invalid alpha-secret
rename ::vapi::get ::vapi::get_real_for_executor_test
proc ::vapi::get {path {ms 0}} {
    if {$path eq "/api/health"} { return [::fleetfix::health_json [string repeat c 40]] }
    if {$path eq "/api/install/state"} { return [::fleetfix::install_state_json [string repeat 8 64]] }
    error "HTTP 404: unexpected $path"
}
set ::vfleetdeploy::transport ::vfleetdeploy::_transport
set wire_target [lindex [dict get $m targets] 0]
ok "the production health transport parses the gateway's JSON body" {
    ![catch {::vfleetdeploy::_fresh_health $wire_target} wire_health] &&
    [dict get $wire_health status] eq "ok" &&
    [dict get $wire_health revision] eq [string repeat c 40]
}
ok "the installer-state baseline is read from the gateway's JSON body" {
    [::vfleetdeploy::_core_artifact [::vfleetdeploy::_transport install-state $wire_target {}]] eq [string repeat 8 64]
}
rename ::vapi::get ""
rename ::vapi::get_real_for_executor_test ::vapi::get

# ── F1-07: a first managed upgrade stages; an unmanaged core is refused ─
set ::vfleetdeploy::transport fake_transport
set ::bad_health {}
set ::health [observations]
set ::transport_log {}
set first_upgrade [::vfleetdeploy::apply $m $before]
set first_upgrade [::vfleetdeploy::stage $first_upgrade node-a]
ok "staging needs no retained previous core: no rollback preview is asked for" {
    [dict get $first_upgrade targets node-a status] eq "staged" &&
    [lsearch -exact $::transport_log rollback-preview] < 0 &&
    [dict get $first_upgrade reviews node-a previous_artifact_sha256] eq $::current_artifact
}
set ::managed_core 0
set ::model_slot 1
array set ::pending {node-a {} node-b {}}
set ::cancelled {}
set unmanaged [::vfleetdeploy::apply $m $before]
rejects "an unmanaged core is refused before any signed preview" {
    ::vfleetdeploy::stage $unmanaged node-a
} "not installer-managed"
set ::managed_core 1
set unmanaged [::vfleetjournal::get [dict get $unmanaged id]]
ok "the unmanaged refusal is a pre-install failure with no signed plan left pending" {
    [dict get $unmanaged targets node-a status] eq "failed" &&
    ![dict exists $unmanaged targets node-a install_started] &&
    $::pending(node-a) eq ""
}

# ── F1-05: a pre-install failure is no longer a dead end ─────────────
set unmanaged_resumed [::vfleetdeploy::resume [dict get $unmanaged id] [observations]]
ok "resume loads an operation whose target failed before installation" {
    [dict get $unmanaged_resumed targets node-a status] eq "failed"
}
set abandoned [::vfleetdeploy::abandon $unmanaged_resumed]
ok "abandon closes the untouched operation as a distinct terminal state" {
    [dict get [::vfleetjournal::get [dict get $unmanaged id]] status] eq "abandoned" &&
    [lsearch -exact [lmap row [::vfleetjournal::unfinished] {dict get $row id}] [dict get $unmanaged id]] < 0
}
rejects "an abandoned operation is immutable" {
    ::vfleetjournal::transition [dict get $unmanaged id] node-a rollback-preview {late}
} "terminal"
set ::preview_failure 1
set preview_failed [::vfleetdeploy::apply $m $before]
catch {::vfleetdeploy::stage $preview_failed node-a}
set ::preview_failure 0
set preview_failed [::vfleetjournal::get [dict get $preview_failed id]]
ok "a failed signed preview cancels the plan request it journaled first" {
    [dict exists $preview_failed targets node-a plan_request] &&
    [lsearch -exact $::cancelled [dict get $preview_failed targets node-a plan_request]] >= 0 &&
    $::pending(node-a) eq ""
}
set ::model_slot 0
set ::rolling_back 0
set ::health [observations]
set ::vfleetdeploy::transport fake_transport_deploy
set installed_op [::vfleetdeploy::apply $m $before]
set installed_op [::vfleetdeploy::stage $installed_op node-a]
set installed_op [::vfleetdeploy::verify $installed_op node-a]
rejects "abandon refuses an operation with an installation attempt" {
    ::vfleetdeploy::abandon $installed_op
} "roll it back"
# A canary that was verified and a later target whose staging failed
# before any installation: rollback of the canary must be closable.
set ::managed_core 0
catch {::vfleetdeploy::stage $installed_op node-b}
set ::managed_core 1
set installed_op [dict get $::vfleetdeploy::operations [dict get $installed_op id]]
set ::vfleetdeploy::transport fake_transport
set ::rolling_back 1
dict set ::health node-a revision [string repeat a 40]
set installed_op [::vfleetdeploy::rollback $installed_op node-a]
set closed [::vfleetdeploy::finish_rollback $installed_op]
ok "finish rollback accepts a later target that failed before installation" {
    [dict get $installed_op targets node-b status] eq "failed" &&
    [dict get $closed status] eq "rolled-back" &&
    [dict get $closed targets node-b status] eq "failed"
}

# ── F1-11: exact binding phrases and an operator-only diagnostic ──────
ok "a signed preview defect is not reported as registry tampering" {
    [::vfleetdeploy::_safe_error "signed release preview lacks manifest_sha256"] ne
        "reviewed registry or operation binding changed; action refused"
}
ok "the operator diagnostic keeps the cause and scrubs credentials" {
    [string first "409" [::vfleetdeploy::diagnostic "HTTP 409: Bearer abc123 token=xyz still awaiting" {}]] >= 0 &&
    ![string match *abc123* [::vfleetdeploy::diagnostic "HTTP 401 Bearer abc123" {}]] &&
    ![string match *xyz* [::vfleetdeploy::diagnostic "token=xyz" {}]] &&
    ![string match *alpha-secret* [::vfleetdeploy::diagnostic "echo alpha-secret" {alpha-secret}]]
}

file delete -force $root
puts "fleetexecutortest: $pass passed, $fail failed"
exit [expr {$fail ? 1 : 0}]
