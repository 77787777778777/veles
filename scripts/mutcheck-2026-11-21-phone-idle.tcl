#!/usr/bin/env tclsh
# Regression calibration for the phone's idle lifecycle and the /new
# rebind's live-session boundary. Each mutant must turn the named REAL
# test red, then be restored. Java's last-unbind path is driven on the
# phone (docs/issues/open-items/phone-userland.md), not faked here.
#
#   direnv exec . tclsh scripts/mutcheck-2026-11-21-phone-idle.tcl [--dry]

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set DE crates/veles-core/src/device_events.rs
set GW crates/veles-frontends/src/gateway.rs

set mutations [list \
    [list "a sleeping pool member wakes after one minute" \
        $DE "        Some(now + 15 * 60)" "        Some(now + 60) // MUTATION" \
        "-p veles-core --lib" \
        device_events::tests::a_pool_member_wake_plan_carries_a_battery_respecting_heartbeat] \
    [list "a resume takes another sender's live session" \
        $GW {        if self
            .session_ids
            .lock_ok()
            .iter()
            .any(|(other_key, sid)| sid == new_sid && other_key != &key)
        } "\n        if false \{ // MUTATION" \
        "-p veles-frontends --lib" \
        desktop_join::tests::a_resume_cannot_readdress_another_senders_live_session] \
    [list "an unattended fleet-alert poll is passive on the real request" \
        veles-desktop/lib/fleetmanage.tcl "    set ::vapi::passive 1\n    if {\[catch { ::vapi::get_json_async /api/fleet/alerts" \
        "    # MUTATION: no passive marker\n    if {\[catch { ::vapi::get_json_async /api/fleet/alerts" \
        "-tcl fleetmanagetest" \
        ""] \
]

proc read_file {path} {
    set f [open $path r]
    fconfigure $f -encoding utf-8 -translation binary
    set s [read $f]
    close $f
    return [encoding convertfrom utf-8 $s]
}
proc write_file {path s} {
    set f [open $path w]
    fconfigure $f -translation binary
    puts -nonewline $f [encoding convertto utf-8 $s]
    close $f
}
set dry [expr {[lindex $argv 0] eq "--dry"}]
set verified 0
set failed 0
foreach mutation $mutations {
    lassign $mutation name path find repl target test
    set before [read_file $path]
    set at [string first $find $before]
    if {$at < 0 || [string first $find $before [expr {$at + 1}]] >= 0} {
        puts "$name: PATTERN-MISSING"
        incr failed
        continue
    }
    puts -nonewline "$name: "
    flush stdout
    if {$dry} {
        puts FOUND
        incr verified
        continue
    }
    write_file $path [string map [list $find $repl] $before]
    set out ""
    set rc 0
    try {
        if {$target eq "-tcl fleetmanagetest"} {
            set command [list env TMPDIR=/dev/shm direnv exec . bash scripts/one-tcl-test.sh fleetmanagetest]
            set f [open "| $command < /dev/null 2>@1" r]
        } else {
            set f [open "| env TMPDIR=/dev/shm direnv exec . cargo test $target $test -- --exact < /dev/null 2>@1" r]
        }
        set out [read $f]
        set rc [catch {close $f}]
    } finally {
        write_file $path $before
    }
    if {$target eq "-tcl fleetmanagetest"} {
        set red [expr {$rc && [regexp {✗ an unattended fleet-alert poll is passive on the real request} $out]
            && [regexp {fleetmanagetest: [0-9]+ passed, [1-9][0-9]* failed} $out]}]
    } else {
        set red [expr {$rc && [regexp {test result: FAILED} $out]
            && [regexp "test [regsub -all {[^a-zA-Z0-9_]} $test .] \\.\\.\\. FAILED" $out]}]
    }
    puts [expr {$red ? "RED" : "NOT RED"}]
    if {!$red} {
        puts $out
        incr failed
    } else {
        incr verified
    }
}
puts "mutcheck phone idle 2026-11-21: $verified verified red, $failed not"
exit [expr {$failed > 0}]
