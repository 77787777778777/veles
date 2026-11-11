#!/usr/bin/env tclsh
# Real headless worker protocol test. The first block reaches only an
# invalid HTTPS fixture host. The end-to-end block runs the real worker
# process against a loopback fake gateway that serves bodies captured from
# a real gateway (fleetfixtures.tcl), with `ssh` shimmed on PATH: no live
# gateway, SSH target or installation is touched.
set dir [file dirname [file normalize [info script]]]
source [file join $dir paths.tcl]
source [file join $dir instances.tcl]
source [file join $dir instpane.tcl]
source [file join $dir fleetdeploy.tcl]
source [file join $dir fleetfixtures.tcl]
set pass 0
set fail 0
proc ok {name condition} {
    if {[uplevel 1 [list expr $condition]]} { incr ::pass; puts "  ok $name" } else { incr ::fail; puts "  FAIL $name" }
}
proc release {} {
    return [dict create repository 77787777778777/veles tag v0.1.6 commit [string repeat a 40] workflow_sha [string repeat b 40]]
}
set root [file join /tmp veles-worker-test-[pid]-[clock clicks]]
::vfleetjournal::configure $root
set ::vfleetdeploy::nodes [::vfleetdeploy::nodes_from_inventory [::json::json2dict [::fleetfix::fleet_json {{node-a alpha 0.1.6}}]]]
set ::vfleetdeploy::instances [dict create alpha [dict create node_id node-a url https://alpha.invalid token fixture-secret host alpha@example.invalid unit veles-gateway]]
set m [::vfleetdeploy::manifest {node-a} [release] native-bundle]
set pre [::vfleetdeploy::preflight $m [dict create node-a [dict create status ok version 0.1.6 revision [string repeat c 40]]]]
set op [::vfleetdeploy::apply $m $pre]
set req [dict create operation $op action stage node_id node-a journal_root $root registry $::vfleetdeploy::instances]
set reqdir "$root.req"
file mkdir $reqdir
set input [file join $reqdir request.json]
set f [open $input w]
puts -nonewline $f [::vfleetjson::encode [::vfleetjson::operation_value $req]]
close $f
set output [exec tclsh [file join $dir fleetworker.tcl] < $input]
set parsed [::json::json2dict $output]
ok "worker returns structured JSON" {[dict exists $parsed ok] && [dict exists $parsed operation]}
# An unreachable target cannot prove which node it is (FLEET-2), so the
# stage is refused before anything is asked of it and nothing is
# journaled as failed: the operation stays reviewed and can be retried or
# abandoned. A failure AFTER the proof is durable (the end-to-end block).
ok "worker refuses the unreachable fixture target without deploying" {
    [dict get $parsed ok] in {0 false} &&
    [string match "*did not prove it is the selected node*" [dict get $parsed error]] &&
    [string match "*could not be asked which node it is*" [dict get $parsed diagnostic]]
}
ok "worker does not leak the instance credential" {![string match *fixture-secret* $output]}
set saved [::vfleetjournal::get [dict get $op id]]
ok "an identity refusal leaves the target reviewed in the journal" {[dict get $saved targets node-a status] eq "reviewed"}
# F1-11: the operator used to see only the redacted phrase. The reply now
# carries the raw cause as `diagnostic`, which the journal never stores.
ok "a worker refusal carries the operator diagnostic, and the journal does not" {
    [dict exists $parsed diagnostic] &&
    [dict get $parsed diagnostic] ne [dict get $parsed error] &&
    [string first [dict get $parsed diagnostic] [::vfleetjson::encode [::vfleetjson::operation_value $saved]]] < 0
}
set tampered $req
dict set tampered registry alpha url https://redirected.invalid
set tampered_file [file join $reqdir tampered.json]
set f [open $tampered_file w]
puts -nonewline $f [::vfleetjson::encode [::vfleetjson::operation_value $tampered]]
close $f
set tampered_output [exec tclsh [file join $dir fleetworker.tcl] < $tampered_file]
set tampered_parsed [::json::json2dict $tampered_output]
ok "worker refuses a registry URL changed after review" {
    [dict get $tampered_parsed ok] in {0 false} &&
    [string match "*binding changed*" [dict get $tampered_parsed error]]
}
ok "registry tampering does not leak the token" {![string match *fixture-secret* $tampered_output]}

# ── End to end: the real worker, the real transport, a fake gateway ──
# F1-01 (health is JSON), F1-04 (the worker's ssh restart needs
# safeexec) and F1-05 (the single pending signed plan) were invisible to
# the dict-returning fake transports. This drives them over real HTTP.
lassign [::fleetfix::serve] server port
set bin [file join $root bin]
file mkdir $bin
set marker [file join $root restarted]
set argv_log [file join $root ssh-argv]
set shim [open [file join $bin ssh] w]
puts $shim "#!/bin/sh\nprintf '%s\\n' \"\$@\" > '$argv_log'\n: > '$marker'\nexit 0"
close $shim
file attributes [file join $bin ssh] -permissions 0755
set ::env(PATH) "$bin:$::env(PATH)"
set ::fleetfix::gw(revision) [string repeat c 40]
set ::fleetfix::gw(after_revision) [dict get [release] commit]
set ::fleetfix::gw(restarted_marker) $marker
set ::fleetfix::gw(digest) [string repeat 8 64]
set ::fleetfix::gw(release) [release]
set e2e_instances [dict create alpha [dict create node_id node-a url http://127.0.0.1:$port token e2e-fixture-secret host alpha@example.invalid unit veles-gateway]]
set ::vfleetdeploy::instances $e2e_instances
::vapi::bind_instance alpha http://127.0.0.1:$port e2e-fixture-secret
set observed [::vfleetdeploy::health_observation alpha]
ok "a captured /api/health body is a parsed healthy observation" {
    [dict get $observed status] eq "ok" && [dict get $observed revision] eq [string repeat c 40]
}

proc run_worker {request} {
    set chan [open |[list tclsh [file join $::dir fleetworker.tcl] 2>@1] r+]
    fconfigure $chan -blocking 0 -encoding utf-8 -translation binary
    puts -nonewline $chan [::vfleetjson::encode [::vfleetjson::operation_value $request]]
    flush $chan
    chan close $chan write
    set ::worker_out ""
    set ::worker_done 0
    fileevent $chan readable [list worker_read $chan]
    set timer [after 90000 {set ::worker_done timeout}]
    vwait ::worker_done
    after cancel $timer
    set lines [lsearch -all -inline -not -exact [split [string trim $::worker_out] \n] ""]
    if {[catch {::json::json2dict [lindex $lines end]} reply]} {
        puts "    worker output: $::worker_out"
        return [dict create ok 0 error unparsable]
    }
    if {[dict exists $reply ok] && [dict get $reply ok] ni {1 true}} {
        puts "    worker refused: [dict get $reply error] / [expr {[dict exists $reply diagnostic] ? [dict get $reply diagnostic] : ""}]"
    }
    return $reply
}
proc worker_read {chan} {
    append ::worker_out [read $chan]
    if {[eof $chan]} {
        fileevent $chan readable {}
        catch {close $chan}
        set ::worker_done 1
    }
}

set m [::vfleetdeploy::manifest {node-a} [release] native-bundle]
set pre [::vfleetdeploy::preflight $m [dict create node-a $observed]]
set e2e [::vfleetdeploy::apply $m $pre]
# FLEET-2 over real HTTP: the gateway behind `alpha` says it is node-x.
set ::fleetfix::gw(self_id) node-x
set ::fleetfix::gw(log) {}
set wrong_reply [run_worker [dict create operation $e2e action stage node_id node-a journal_root $root registry $e2e_instances]]
ok "the worker refuses to stage on a gateway that answers as another node, naming both ids" {
    [dict get $wrong_reply ok] in {0 false} &&
    [string match "*saved instance alpha is node node-x, not node-a*" [dict get $wrong_reply diagnostic]] &&
    $::fleetfix::gw(log) eq {GET /api/fleet} && $::fleetfix::gw(pending) eq "" &&
    [dict get [::vfleetjournal::get [dict get $e2e id]] targets node-a status] eq "reviewed"
}
set ::fleetfix::gw(self_id) node-a
set ::fleetfix::gw(log) {}
set staged_reply [run_worker [dict create operation $e2e action stage node_id node-a journal_root $root registry $e2e_instances]]
set staged_journal [::vfleetjournal::get [dict get $e2e id]]
ok "the worker stages over the real transport: identity, health, installer state, signed preview" {
    [dict get $staged_reply ok] in {1 true} &&
    [dict get $staged_journal targets node-a status] eq "staged" &&
    $::fleetfix::gw(log) eq {GET /api/fleet GET /api/health GET /api/install/state POST /api/install/download}
}
ok "the staged plan is the gateway's single pending plan, under a fresh request id" {
    $::fleetfix::gw(pending) eq [dict get $staged_journal reviews node-a plan_id] &&
    [string match "[dict get $e2e id].*" $::fleetfix::gw(pending)]
}
set verify_reply [run_worker [dict create operation $e2e action verify node_id node-a journal_root $root registry $e2e_instances]]
set argv_text ""
if {[file exists $argv_log]} {
    set f [open $argv_log r]
    set argv_text [string trim [read $f]]
    close $f
}
ok "the worker's restart runs the exact ssh argv (safeexec is loaded in the worker)" {
    [split $argv_text \n] eq {-o BatchMode=yes -o ConnectTimeout=5 alpha@example.invalid systemctl --user restart veles-gateway}
}
ok "verify confirms the plan, restarts and records the reviewed revision" {
    [dict get $verify_reply ok] in {1 true} &&
    [dict get [::vfleetjournal::get [dict get $e2e id]] targets node-a status] eq "healthy" &&
    $::fleetfix::gw(pending) eq ""
}
ok "the end-to-end replies never carry the instance credential" {
    ![string match *e2e-fixture-secret* $::worker_out] &&
    ![string match *e2e-fixture-secret* [::vfleetjson::encode [::vfleetjson::operation_value [::vfleetjournal::get [dict get $e2e id]]]]]
}

# An operation whose stage previewed a plan and was then given up holds
# the gateway's only signed slot; abandon must free it (F1-05).
file delete -force $marker
set abandon_op [::vfleetdeploy::apply $m $pre]
set ::fleetfix::gw(log) {}
run_worker [dict create operation $abandon_op action stage node_id node-a journal_root $root registry $e2e_instances]
set pending_before_abandon $::fleetfix::gw(pending)
set abandoned [run_worker [dict create operation $abandon_op action abandon journal_root $root registry $e2e_instances]]
ok "abandoning an untouched staged operation cancels its signed plan and closes it" {
    $pending_before_abandon ne "" &&
    [dict get $abandoned ok] in {1 true} &&
    $::fleetfix::gw(pending) eq "" &&
    [dict get [::vfleetjournal::get [dict get $abandon_op id]] status] eq "abandoned" &&
    [lsearch -exact [lmap op [::vfleetjournal::unfinished] {dict get $op id}] [dict get $abandon_op id]] < 0
}
# A failure after the identity proof is durable: the target answers as
# node-a, then reports a running revision that is not a commit.
set failing_op [::vfleetdeploy::apply $m $pre]
set saved_revision $::fleetfix::gw(revision)
set ::fleetfix::gw(revision) not-a-commit
set failed_reply [run_worker [dict create operation $failing_op action stage node_id node-a journal_root $root registry $e2e_instances]]
set ::fleetfix::gw(revision) $saved_revision
set failed_journal [::vfleetjournal::get [dict get $failing_op id]]
ok "worker failure after the identity proof is durable in the journal, its diagnostic is not" {
    [dict get $failed_reply ok] in {0 false} &&
    [dict get $failed_journal targets node-a status] eq "failed" &&
    [dict exists $failed_reply diagnostic] &&
    [string first [dict get $failed_reply diagnostic] [::vfleetjson::encode [::vfleetjson::operation_value $failed_journal]]] < 0
}
close $server
file delete -force $root $reqdir
puts "fleetworkertest: $pass passed, $fail failed"
exit [expr {$fail ? 1 : 0}]
