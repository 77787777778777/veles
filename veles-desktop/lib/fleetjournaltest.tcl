#!/usr/bin/env wish
# Durable, local-only F1 deployment operation journal tests.
package require json
set dir [file dirname [file normalize [info script]]]
source [file join $dir fleetjournal.tcl]
set pass 0
set fail 0
proc ok {name condition} {
    if {[uplevel 1 [list expr $condition]]} {
        incr ::pass
        puts "  ok $name"
    } else {
        incr ::fail
        puts "  FAIL $name"
    }
}
proc rejects {name command fragment} {
    if {[catch {uplevel 1 $command} error]} {
        ok $name [expr {[string first $fragment $error] >= 0}]
    } else {
        ok $name 0
        puts "    expected refusal: $fragment; got $error"
    }
}
set root [file join /tmp veles-fleet-journal-[pid]-[clock clicks]]
::vfleetjournal::configure $root
set plan [dict create plan_sig [string repeat a 64] service_model native-bundle release [dict create commit [string repeat b 40]] targets [list [dict create node_id node-a]]]
set op [::vfleetjournal::begin $plan]
set id [dict get $op id]
ok "an operation begins as reviewed, not deployed" {
    [dict get $op status] eq "reviewed" &&
    [dict get $op plan_sig] eq [dict get $plan plan_sig] &&
    [regexp {^[A-Za-z0-9_-]+$} $id]
}
ok "on unix the journal stays owner-only" {
    $::tcl_platform(platform) ne "unix" ||
    (([scan [file attributes $root -permissions] %o] & 0777) == 0700 &&
     ([scan [file attributes [file join $root "$id.json"] -permissions] %o] & 0777) == 0600)
}
::vfleetjournal::transition $id node-a staging {signed download preview accepted}
::vfleetjournal::transition $id node-a staged {signed bundle selected}
set rows [::vfleetjournal::all]
ok "target transitions persist independently" {
    [llength $rows] == 1 &&
    [dict get [lindex $rows 0] status] eq "staged" &&
    [llength [dict get [lindex $rows 0] events]] == 3
}
::vfleetjournal::transition $id node-a restarting {configured unit restart requested}
::vfleetjournal::transition $id node-a healthy {revision matched}
set recovered [::vfleetjournal::get $id]
ok "reload preserves plan and target state" {
    [dict get $recovered plan_sig] eq [dict get $plan plan_sig] &&
    [dict get [dict get $recovered targets node-a] status] eq "healthy" &&
    [llength [dict get $recovered events]] == 5
}
rejects "a healthy target cannot silently become staging" {
    ::vfleetjournal::transition $id node-a staging {invalid transition}
} "illegal operation transition"
rejects "unknown targets cannot be appended by transport output" {
    ::vfleetjournal::transition $id unknown staging {wrong node}
} "not a declared target"
rejects "journal paths cannot escape the configured state root" {
    ::vfleetjournal::get "../../outside"
} "invalid operation id"
rejects "transport errors cannot write credentials into the journal" {
    ::vfleetjournal::transition $id node-a failed {Bearer local-secret token=abc123}
} "secret-shaped"
::vfleetjournal::transition $id node-a rollback-preview {review previous release}
::vfleetjournal::transition $id node-a rollback-staged {previous release selected}
::vfleetjournal::transition $id node-a rollback-restarting {restored unit starting}
::vfleetjournal::transition $id node-a rolled-back {previous release selected}
# A direct transition is not proof: completion requires the recorded
# rollback verification (F1-06).
rejects "a rolled-back target without its rollback verification cannot close the operation" {
    ::vfleetjournal::complete $id rolled-back
} "no recorded rollback verification"
::vfleetjournal::rollback_verified [::vfleetjournal::get $id] node-a [string repeat a 40] 1
::vfleetjournal::complete $id rolled-back
ok "rollback is a distinct audited terminal outcome" {
    [dict get [::vfleetjournal::get $id] status] eq "rolled-back"
}
rejects "terminal operations are immutable" {
    ::vfleetjournal::transition $id node-a restarting {late duplicate}
} "terminal"
set resume_plan [dict create plan_sig [string repeat c 64] service_model native-bundle release [dict create commit [string repeat d 40]] targets [list [dict create node_id node-b]]]
set resume_op [::vfleetjournal::begin $resume_plan]
set unfinished [::vfleetjournal::unfinished]
ok "unfinished journal discovery finds a nonterminal operation" {
    [llength $unfinished] == 1 && [dict get [lindex $unfinished 0] id] eq [dict get $resume_op id]
}
::vfleetjournal::transition [dict get $resume_op id] node-b staging {interrupted preview}
::vfleetjournal::transition [dict get $resume_op id] node-b staged {interrupted install}
ok "staged interruption remains visible for explicit recovery review" {
    [llength [::vfleetjournal::unfinished]] == 1 &&
    [dict get [::vfleetjournal::get [dict get $resume_op id]] targets node-b status] eq "staged"
}
set json_probe [dict create status "raw injected" detail "string gamma" targets [dict create] events [dict create] ok 1]
set json_probe_encoded [::vfleetjson::encode [::vfleetjson::operation_value $json_probe]]
set json_probe_decoded [::json::json2dict $json_probe_encoded]
ok "journal serializer preserves prose and explicit containers" {
    [dict get $json_probe_decoded status] eq "raw injected" &&
    [dict get $json_probe_decoded detail] eq "string gamma" &&
    [dict size [dict get $json_probe_decoded targets]] == 0 &&
    [dict size [dict get $json_probe_decoded events]] == 0 &&
    [dict get $json_probe_decoded ok] in {1 true}
}
file delete -force $root
# FLEET-1: Windows Tcl knows no `file attributes -permissions` (only
# -archive -hidden -longname -readonly -shortname -system). The journal
# used to call it unguarded, so every deployment on the Windows desktop
# failed before its first stage. Emulate that Tcl here: the platform says
# windows and the option is refused with Windows' own message.
set win_root [file join /tmp veles-fleet-journal-win-[pid]-[clock clicks]]
set saved_platform $::tcl_platform(platform)
rename ::file ::_fleetjournaltest_file
proc ::file {args} {
    if {[lindex $args 0] eq "attributes" && "-permissions" in $args} {
        return -code error {bad option "-permissions", must be -archive, -hidden, -longname, -readonly, -shortname, or -system}
    }
    return [uplevel 1 [list ::_fleetjournaltest_file {*}$args]]
}
set ::tcl_platform(platform) windows
set win_configure [catch {::vfleetjournal::configure $win_root} win_configure_error]
set win_begin 1
set win_path ""
if {!$win_configure} {
    set win_begin [catch {::vfleetjournal::begin $plan} win_op]
    if {!$win_begin} { set win_path [file join $win_root "[dict get $win_op id].json"] }
}
set ::tcl_platform(platform) $saved_platform
rename ::file {}
rename ::_fleetjournaltest_file ::file
ok "the journal is written where file attributes has no -permissions" {
    !$win_configure && !$win_begin && $win_path ne "" && [file exists $win_path]
}
if {$win_configure} { puts "    configure: $win_configure_error" }
if {!$win_configure && $win_begin} { puts "    begin: $win_op" }
file delete -force $win_root
puts "fleetjournaltest: $pass passed, $fail failed"
exit [expr {$fail ? 1 : 0}]
