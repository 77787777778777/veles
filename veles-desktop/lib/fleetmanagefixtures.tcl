# veles-desktop/lib/fleetmanagefixtures.tcl — the node-management routes
# of the loopback fake gateway (fleetfixtures.tcl), for fleetmanagetest.
#
# The bodies in ::fleetfix::manage are CAPTURED, byte for byte, from
# three clustered scratch gateways of the 0.2.2 tree (the audit's smoke
# harness on 2026-11-10: "laptop" asked over its desktop API, "phone"
# running under veles-gw, "vps" under nothing) — pitfall 101. The fake
# changes only what a restart changes: the boot, after one failed poll
# (the node is down while it restarts).

namespace eval ::fleetfix {
    variable mg
    array set mg {status "" restarted 0 polls_down 0 bodies {} read_hash "" alert_at "" boot_after ""}
    variable manage
}

source [file join [file dirname [info script]] fleetmanagecaptured.tcl]

# The read's hash and the alert's time, from the captured bodies.
set ::fleetfix::mg(read_hash) [dict get [::json::json2dict $::fleetfix::manage(config_get)] hash]
set ::fleetfix::mg(alert_at) [dict get [lindex [dict get [::json::json2dict $::fleetfix::manage(alerts)] recent] 0] at]

proc ::fleetfix::manage_route {method path request} {
    variable mg
    variable manage
    switch -- "$method $path" {
        "POST /api/fleet/status" {
            lappend mg(bodies) $path $request
            set body $mg(status)
            if {$mg(restarted)} {
                # Down for one poll, then up with another boot.
                if {$mg(polls_down) < 1} {
                    incr mg(polls_down)
                    return [list 502 {{"error":"b: error sending request: connection refused"}}]
                }
                set old [dict get [::json::json2dict $body] boot]
                set body [string map [list $old [string repeat 7 32]] $body]
            }
            return [list 200 $body]
        }
        "POST /api/fleet/restart" {
            lappend mg(bodies) $path $request
            set mg(restarted) 1
            set mg(polls_down) 0
            return [list 200 $manage(restart)]
        }
        "POST /api/fleet/config" {
            lappend mg(bodies) $path $request
            if {[dict exists $request raw] || [dict exists $request unset]} {
                if {[string match acl.* [dict get $request key]]} {
                    return [list 400 $manage(config_held_refused)]
                }
                return [list 200 $manage(config_set)]
            }
            return [list 200 $manage(config_get)]
        }
        "POST /api/fleet/bulk" {
            lappend mg(bodies) $path $request
            if {[dict exists $request confirm] && [dict get $request confirm] in {true 1}} {
                return [list 200 $manage(bulk_run)]
            }
            if {[dict get $request action] eq "sync"} { return [list 200 $manage(bulk_plan_sync)] }
            return [list 200 $manage(bulk_plan)]
        }
        "GET /api/fleet/audit?limit=200" { return [list 200 $manage(audit)] }
        "GET /api/fleet/alerts" { return [list 200 $manage(alerts)] }
    }
    return ""
}

# A new restart starts from a node that has not restarted.
proc ::fleetfix::manage_reset {} {
    variable mg
    set mg(restarted) 0
    set mg(polls_down) 0
}
