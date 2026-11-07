#!/usr/bin/env wish
# Fleet deployment dialog: construction, review, confirmation and worker
# hand-off. No real gateway or SSH call is made.
#
# WHAT IS REAL HERE. The Instances pane's own procs (_is_cluster,
# _cluster_node, selected), the instance registry, ::vapi::get_json and
# the JSON parser all run unmodified; only the HTTP layer (::vapi::get)
# is replaced, and it answers with bodies captured from a real gateway
# (fleetfixtures.tcl). The previous version stubbed _cluster_node to
# return a bare id, /api/fleet to return an id-keyed map and the health
# call to be absent, so every real-shape defect passed (FLEET-1, F1-01,
# F1-02, F1-08).
set ::env(VELES_HOME) [file join /tmp veles-fleet-ui-test-[pid]]
file mkdir $::env(VELES_HOME)
package require json
set dir [file dirname [file normalize [info script]]]
source [file join $dir paths.tcl]
source [file join $dir i18n.tcl]
source [file join $dir instances.tcl]
source [file join $dir instpane.tcl]
source [file join $dir fleetdeployui.tcl]
source [file join $dir fleetfixtures.tcl]
set pass 0
set fail 0
proc ok {name condition} {
    if {[uplevel 1 [list expr $condition]]} { incr ::pass; puts "  ok $name" } else { incr ::fail; puts "  FAIL $name" }
}
namespace eval ::vstatus { proc set_msg {message} { set ::status_message $message } }

# The pane's state, as a refresh from a real /api/fleet body leaves it.
set inventory [::json::json2dict [::fleetfix::fleet_json {{node-a alpha 0.1.6}}]]
set ::vinstpane::fleet_nodes [dict get $inventory nodes]
set ::vinstpane::selected cluster:node-a
set ::vinst::instances [dict create \
    alpha [dict create node_id node-a url https://alpha.invalid token alpha-secret host alpha@example.invalid unit veles-gateway] \
    other [dict create url https://other.invalid token other-secret]]
::vapi::bind_instance alpha https://alpha.invalid alpha-secret

# The HTTP layer only.
set ::health_status ok
set ::busy_during_http {}
proc ::vapi::get {path {ms 0}} {
    lappend ::busy_during_http $::vfleetui::busy
    switch -- $path {
        /api/fleet { return [::fleetfix::fleet_json {{node-a alpha 0.1.6}}] }
        /api/health { return [::fleetfix::health_json [string repeat c 40] 0.1.6 $::health_status] }
    }
    error "HTTP 404: unexpected API path $path"
}
set ::confirm_answer yes
proc tk_messageBox {args} { return $::confirm_answer }
set ::vfleetui::release_commit [string repeat a 40]
set ::vfleetui::workflow_commit [string repeat b 40]

::vfleetui::open
ok "the dialog opens on the selected cluster row with its node id, not the node object" {
    [winfo exists .fleetdeploy] && $::vfleetui::target_ids eq "node-a"
}
ok "opening the dialog does not create an operation" {
    $::vfleetui::operation eq "" && $::vfleetui::manifest eq ""
}
set ::busy_during_http {}
set reviewed [::vfleetui::review]
ok "review builds a manifest and preflight from the real /api/fleet and /api/health bodies" {
    $reviewed == 1 &&
    [dict get [lindex [dict get $::vfleetui::manifest targets] 0] node_id] eq "node-a" &&
    [dict get $::vfleetui::preflight ready] == 1 &&
    [dict get [lindex [dict get $::vfleetui::preflight observations] 0] revision] eq [string repeat c 40]
}
ok "review keeps the actions disabled while its health probes run" {
    [llength $::busy_during_http] > 0 && [lsearch -exact $::busy_during_http 0] < 0 &&
    $::vfleetui::busy == 0 &&
    [$::vfleetui::w.body.actions.stage cget -state] eq "normal"
}
set ::health_status error
destroy .fleetdeploy
::vfleetui::open
set ::vfleetui::operation [dict create id stale-op]
set ::vfleetui::op_id stale-op
ok "an unhealthy target refuses the review" {
    ![::vfleetui::review] && [string match "*Review refused*not healthy*" $::status_message]
}
ok "a failed fresh review preserves the active operation" {
    $::vfleetui::operation ne "" && $::vfleetui::op_id eq "stale-op" && $::vfleetui::busy == 0
}
set ::health_status ok
set ::vfleetui::operation ""
set ::vfleetui::op_id ""
::vfleetui::review

# A declined confirmation must not leave a durable operation behind; it
# used to, and the orphan then blocked Resume (F1-05).
set before_decline [llength [::vfleetjournal::unfinished]]
set ::confirm_answer no
::vfleetui::mutate stage
set ::confirm_answer yes
ok "declining the stage confirmation creates no journal" {
    [llength [::vfleetjournal::unfinished]] == $before_decline && $::vfleetui::operation eq ""
}

set ::vfleetui::busy 0
set ::vfleetui::worker ""
set ::vfleetui::worker_launcher ::fake_worker_launch
set ::fake_worker_argv {}
set ::fake_worker_json "{}"
set ::fake_worker_read ""
set ::fake_worker_write ""
proc fake_worker_launch {argv json} {
    set ::fake_worker_argv $argv
    set ::fake_worker_json $json
    lassign [chan pipe] ::fake_worker_read ::fake_worker_write
    fconfigure $::fake_worker_read -blocking 0
    return $::fake_worker_read
}
rename ::vfleetdeploy::stage ::vfleetdeploy::stage_real_for_ui_test
proc ::vfleetdeploy::stage {args} { error "synchronous executor was used in UI callback" }
# A button callback runs from the event loop after main.tcl finished,
# where `info script` is empty; the worker path used to be computed from
# it there and became ./fleetworker.tcl (F1-03).
set this_script [info script]
info script ""
set started [::vfleetui::mutate stage]
info script $this_script
ok "a confirmed stage starts a worker instead of the synchronous executor" {
    $started == 1 && $::vfleetui::worker eq $::fake_worker_read && $::vfleetui::busy == 1 &&
    [llength [::vfleetjournal::unfinished]] == $before_decline + 1
}
ok "the worker argv names an existing worker script and interpreter, from a callback" {
    [file isfile [lindex $::fake_worker_argv end]] &&
    [file tail [lindex $::fake_worker_argv end]] eq "fleetworker.tcl" &&
    [file pathtype [lindex $::fake_worker_argv end]] eq "absolute" &&
    [auto_execok [lindex $::fake_worker_argv 0]] ne ""
}
set parsed_worker_request [::json::json2dict $::fake_worker_json]
set first_worker_target [lindex [dict get $parsed_worker_request operation manifest targets] 0]
ok "the real worker request is valid nested JSON with a singleton target list" {
    [dict get $parsed_worker_request action] eq "stage" &&
    [llength [dict get $parsed_worker_request operation manifest targets]] == 1 &&
    [dict get $first_worker_target node_id] eq "node-a"
}
ok "the worker receives only the reviewed targets' registry rows" {
    [dict keys [dict get $parsed_worker_request registry]] eq {alpha} &&
    ![string match *other-secret* $::fake_worker_json]
}
set saved_token $::vapi::token
set saved_registry $::vinst::instances
set ::vapi::token default-token
set ::vinst::instances [dict create \
    local [dict create url $::vapi::base host h@example.invalid unit u] \
    remote [dict create url https://remote.invalid host h@example.invalid unit u]]
set inherited [::vfleetui::_worker_registry [dict create manifest [dict create targets [list \
    [dict create instance local] [dict create instance remote]]]]]
set ::vapi::token $saved_token
set ::vinst::instances $saved_registry
ok "a same-origin row inherits the default token; a remote row never does" {
    [dict get $inherited local token] eq "default-token" &&
    [dict get $inherited remote token] eq ""
}
catch {close $::fake_worker_write}
catch {close $::fake_worker_read}

# Worker replies: stderr is merged into the pipe, so noise may precede
# the structured reply line.
set worker_reply [dict create ok 1 operation $::vfleetui::operation]
set reply_path [file join /tmp veles-fleet-ui-reply-[pid].json]
set reply_file [open $reply_path w]
puts $reply_file "a warning on stderr"
puts -nonewline $reply_file [::vfleetjson::encode [::vfleetjson::operation_value $worker_reply]]
close $reply_file
set ::vfleetui::worker [open $reply_path r]
fconfigure $::vfleetui::worker -blocking 1 -encoding utf-8
set ::vfleetui::worker_output ""
::vfleetui::_worker_finish
::vfleetui::_worker_finish
ok "worker success clears busy state and stores the operation dictionary" {
    $::vfleetui::busy == 0 &&
    [dict exists $::vfleetdeploy::operations [dict get $::vfleetui::operation id]] &&
    [dict get $::vfleetui::operation status] eq "reviewed"
}
ok "worker completion re-enables resume and exposes rollback finalization" {
    [$::vfleetui::w.body.actions.resume cget -state] eq "normal" &&
    [winfo exists $::vfleetui::w.body.actions.finish-rollback] &&
    [::vfleetui::_action_target $::vfleetui::operation finish-rollback] eq ""
}
set refusal [dict create ok 0 error {transport operation failed (diagnostic redacted)} diagnostic {HTTP 409: a signed-download review is still awaiting confirmation or cancellation}]
set reply_file [open $reply_path w]
puts -nonewline $reply_file [::vfleetjson::encode [::vfleetjson::operation_value $refusal]]
close $reply_file
set ::vfleetui::worker [open $reply_path r]
set ::vfleetui::worker_output ""
::vfleetui::_worker_finish
::vfleetui::_worker_finish
file delete -force $reply_path
ok "a refusal shows the operator its unjournaled diagnostic" {
    [string match "*redacted*Diagnostic*409*" $::status_message]
}

# Resume with two unfinished operations loads one instead of refusing.
destroy .fleetdeploy
::vfleetui::open
::vfleetui::review
set second [::vfleetdeploy::apply $::vfleetui::manifest $::vfleetui::preflight]
set unfinished_ids [lsort [lmap op [::vfleetjournal::unfinished] {dict get $op id}]]
set ::vfleetui::busy 0
ok "resume loads the oldest of several unfinished operations and names the rest" {
    [llength $unfinished_ids] >= 2 &&
    [::vfleetui::resume] == 1 &&
    $::vfleetui::op_id eq [lindex $unfinished_ids 0] &&
    [string match "*Also unfinished*" $::status_message]
}
set ::vfleetui::worker ""
set started [::vfleetui::mutate abandon]
ok "abandon is offered for the loaded operation and runs in the worker" {
    $started == 1 && [dict get [::json::json2dict $::fake_worker_json] action] eq "abandon"
}
catch {close $::fake_worker_write}
catch {close $::fake_worker_read}
set ::vfleetui::busy 0
set ::vfleetui::worker ""

set later_failure $::vfleetui::operation
dict set later_failure manifest targets [list \
    [dict create node_id node-a] [dict create node_id node-b]]
dict set later_failure targets node-a status healthy
dict set later_failure targets node-b [dict create status failed install_started 1]
set ::vfleetui::selected node-a
ok "a failed later target cannot make verify reinstall the healthy canary" {
    [::vfleetui::_action_target $later_failure verify] eq ""
}
ok "a failed later target is the unique rollback target" {
    [::vfleetui::_action_target $later_failure rollback] eq "node-b"
}
dict set later_failure targets node-a status rolled-back
dict set later_failure targets node-b status rolled-back
ok "a fully rolled-back operation cannot target rollback again" {
    [::vfleetui::_action_target $later_failure rollback] eq ""
}

# Android cannot start the worker pipe: the dialog is refused there.
destroy .fleetdeploy
namespace eval ::vandroid { proc on {} { return 1 } }
::vfleetui::open
ok "the dialog is refused on Android instead of failing after a journal write" {
    ![winfo exists .fleetdeploy] && ![::vfleetui::available]
}
rename ::vandroid::on ""

rename ::vfleetdeploy::stage ""
rename ::vfleetdeploy::stage_real_for_ui_test ::vfleetdeploy::stage
file delete -force $::env(VELES_HOME)
puts "fleetdeployuitest: $pass passed, $fail failed"
exit [expr {$fail ? 1 : 0}]
