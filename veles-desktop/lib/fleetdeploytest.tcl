#!/usr/bin/env wish
# F1.4: reviewed fleet deployment manifests and target preflight.
# No real network, SSH or installer calls: the production transport seams
# are exercised by insttest and desktop_install's authenticated API tests.
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
        puts "    expected refusal containing: $fragment; got $error"
    }
}
# The node map is built the way the dialog builds it: from a /api/fleet
# body in the gateway's own shape (`nodes` is a JSON ARRAY).
proc fixture_inventory {} {
    return [::json::json2dict [::fleetfix::fleet_json {
        {node-a alpha 0.1.6} {node-b beta 0.1.5} {node-old stale 0.1.5 true}}]]
}
proc fixture_nodes {} {
    return [::vfleetdeploy::nodes_from_inventory [fixture_inventory]]
}
ok "the captured /api/fleet body carries its nodes as a list, not an id map" {
    [llength [dict get [fixture_inventory] nodes]] == 3 &&
    ![dict exists [dict get [fixture_inventory] nodes] node-a]
}
ok "the inventory conversion keys every node by its id" {
    [lsort [dict keys [fixture_nodes]]] eq {node-a node-b node-old} &&
    [dict get [fixture_nodes] node-a name] eq "alpha"
}
rejects "an inventory node without an id is refused, not keyed as empty" {
    ::vfleetdeploy::nodes_from_inventory [::json::json2dict {{"nodes":[{"id":null,"name":"x"}]}}]
} "without an id"
proc fixture_instances {} {
    return [dict create \
        alpha [dict create node_id node-a url https://alpha.invalid token local-secret host alpha@example.invalid unit veles-gateway] \
        beta [dict create node_id node-b url https://beta.invalid token another-secret host beta@example.invalid unit veles-gateway]]
}
proc release {} {
    return [dict create \
        repository 77787777778777/veles tag v0.1.6 \
        commit 98cc833012d884334604f20abc7f7b90182737e3 \
        workflow_sha 1234567890abcdef1234567890abcdef12345678]
}

namespace eval ::vfleetdeploy {
    variable nodes [fixture_nodes]
    variable instances [fixture_instances]
}

# A manifest is a typed target selection, not credentials or remote shell.
set m [::vfleetdeploy::manifest [list node-a node-b] [release] native-bundle]
ok "manifest binds the two selected nodes" {
    [dict get $m version] == 1 &&
    [llength [dict get $m targets]] == 2 &&
    [dict get [lindex [dict get $m targets] 0] instance] eq "alpha" &&
    [dict get [lindex [dict get $m targets] 1] instance] eq "beta" &&
    [dict get $m service_model] eq "native-bundle"
}
ok "manifest has a stable review signature for identical ordered targets" {
    [string length [dict get $m plan_sig]] == 64 &&
    [dict get $m plan_sig] eq [dict get [::vfleetdeploy::manifest [list node-a node-b] [release] native-bundle] plan_sig]
}
ok "reversing the canary order changes the reviewed plan" {
    [dict get $m plan_sig] ne [dict get [::vfleetdeploy::manifest [list node-b node-a] [release] native-bundle] plan_sig] &&
    [dict get [lindex [dict get [::vfleetdeploy::manifest [list node-b node-a] [release] native-bundle] targets] 0] stage] eq "canary"
}
set review_text [::json::dict2json $m]
ok "review never contains saved tokens or guessed SSH targets" {
    ![string match *local-secret* $review_text] &&
    ![string match *another-secret* $review_text] &&
    [dict get [lindex [dict get $m targets] 0] unit] eq "veles-gateway"
}
set diff_rows [::vfleetdeploy::diff $m]
ok "target diff names observed and desired revisions" {
    [dict get [lindex $diff_rows 0] current_version] eq "0.1.6" &&
    [dict get [lindex $diff_rows 0] desired_commit] eq [dict get [release] commit] &&
    [dict get [lindex $diff_rows 0] stage] eq "canary" &&
    [dict get [lindex $diff_rows 1] stage] eq "remaining"
}
rejects "unknown node cannot select a saved gateway" {
    ::vfleetdeploy::manifest [list missing] [release] native-bundle
} "not a registered node"
rejects "a registry name is not silently an SSH target" {
    namespace eval ::vfleetdeploy { variable instances [dict create orphan [dict create url https://orphan.invalid host root@elsewhere.invalid]] }
    ::vfleetdeploy::manifest [list node-a] [release] native-bundle
} "no explicitly associated saved instance"
namespace eval ::vfleetdeploy { variable instances [fixture_instances] }
rejects "duplicate node selection is refused" {
    ::vfleetdeploy::manifest [list node-a node-a] [release] native-bundle
} "selected more than once"
rejects "instance name is not a second target alias" {
    ::vfleetdeploy::manifest [list alpha] [release] native-bundle
} "not a registered node"

# The manifest can be constructed from the last registry snapshot, but an
# apply may not start until every target has fresh, matching health.
set before [::vfleetdeploy::preflight $m [dict create node-a [dict create status ok version 0.1.6 revision [dict get [release] commit]] node-b [dict create status ok version 0.1.5 revision 1234567890abcdef1234567890abcdef12345678]]]
ok "preflight binds a fresh target health observation" {
    [dict get $before ready] == 1 &&
    [dict get [lindex [dict get $before observations] 0] revision] eq [dict get [release] commit]
}
rejects "preflight refuses a missing target observation" {
    ::vfleetdeploy::preflight $m [dict create]
} "no current health observation"
rejects "preflight refuses an incomplete target observation" {
    ::vfleetdeploy::preflight $m [dict create node-a [dict create status ok version 0.1.6] node-b [dict create status ok version 0.1.5]]
} "revision"
set dev [::vfleetdeploy::preflight $m [dict create node-a [dict create status ok version 0.1.6 revision unreported-development-build] node-b [dict create status ok version 0.1.5 revision 1234567890abcdef1234567890abcdef12345678]]]
ok "preflight records an explicitly unreported development revision" {
    [dict get [lindex [dict get $dev observations] 0] revision] eq "unreported-development-build"
}
rejects "preflight rejects an unknown nonstandard revision" {
    ::vfleetdeploy::preflight $m [dict create node-a [dict create status ok version 0.1.6 revision not-a-source-id] node-b [dict create status ok version 0.1.5 revision 1234567890abcdef1234567890abcdef12345678]]
} "invalid running source revision"
rejects "preflight refuses a stale target" {
    ::vfleetdeploy::preflight $m [dict create node-a [dict create status ok stale 1] node-b [dict create status ok]]
} "stale"
rejects "preflight refuses a changed registry version" {
    ::vfleetdeploy::preflight $m [dict create node-a [dict create status ok version 9.9.9] node-b [dict create status ok version 0.1.5]]
} "changed since"
rejects "apply cannot bypass a preflight" {
    ::vfleetdeploy::apply $m no-such-preflight
} "no reviewed preflight"
set journal_root [file join /tmp veles-fleet-plan-[pid]]
::vfleetjournal::configure $journal_root
set reviewed [::vfleetdeploy::apply $m $before]
ok "valid reviewed apply creates a journal without changing a target" {
    [dict get $reviewed status] eq "reviewed" &&
    [dict get $reviewed targets node-a status] eq "reviewed" &&
    [dict get $reviewed targets node-b status] eq "reviewed"
}
file delete -force $journal_root
rejects "stale nodes must be re-observed" {
    ::vfleetdeploy::manifest [list node-old] [release] native-bundle
} "no explicitly associated saved instance"
rejects "short commit ids cannot be deployed" {
    ::vfleetdeploy::manifest [list node-a] [dict merge [release] [dict create commit 98cc833]] native-bundle
} "full lowercase"
rejects "container deployments cannot use the native bundle installer" {
    ::vfleetdeploy::manifest [list node-a] [release] container-image
} "unsupported fleet service model"
rejects "an empty rollout has nothing to review" {
    ::vfleetdeploy::manifest {} [release] native-bundle
} "at least one"

puts "fleetdeploytest: $pass passed, $fail failed"
exit [expr {$fail ? 1 : 0}]
