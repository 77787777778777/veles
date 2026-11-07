# veles-desktop/lib/fleetobstest.tcl — the Instances tab's per-node
# observation (lib/fleetobs.tcl): logs, metrics, drift, alerts, gossip
# now and the fleet-wide forget wording, on the desktop and the phone
# layout.
#
# WHAT IS REAL. api.tcl's transport (http::geturl over a socket, the JSON
# encoder for the request bodies, ::json::json2dict for the answers),
# instpane.tcl, place.tcl and fleetobs.tcl run unmodified against the
# loopback fake gateway in fleetfixtures.tcl, which answers with bodies
# captured from a real gateway pair (pitfall 101: a fixture that cannot
# produce the wire's shape tests the fixture). Only the status bar,
# i18n and a11y are stubbed, as in insttest.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]
set failed 0
set passed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

if {![info exists ::env(XVFB_RUNNING)] && [info exists ::env(DISPLAY)]} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

set ::env(VELES_HOME) [file join /tmp veles-fleetobs-test-[pid]]
file mkdir $::env(VELES_HOME)

namespace eval ::vstatus { variable last "" ; proc set_msg {m} { set ::vstatus::last $m } }
namespace eval ::vi18n {
    proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s }
    proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
    proc aname {w key args} { return 1 }
}
namespace eval ::va11y  { proc name {w d} {} }
namespace eval ::vcoach { proc attach {w} {} }
namespace eval ::vctx   { proc attach {w i} {} }
namespace eval ::vt     {
    variable T
    array set T {hairline #333 bg_panel #222 fg #eee bg_hover #444 accent #0ff
                 bg_input #111 fg_dim #888 select_bg #044 green #0f0 red #f00
                 gold #ff0}
}

package require json
source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
source [file join $_dir paths.tcl]
source [file join $_dir instances.tcl]
source [file join $_dir place.tcl]
source [file join $_dir instpane.tcl]
source [file join $_dir fleetobs.tcl]
source [file join $_dir fleetfixtures.tcl]

# Wait in the REAL event loop (the transport is asynchronous).
proc pump {cond {ms 8000}} {
    set deadline [expr {[clock milliseconds] + $ms}]
    while {![uplevel 1 [list expr $cond]] && [clock milliseconds] < $deadline} {
        after 20 { set ::_tick 1 }
        vwait ::_tick
    }
    return [uplevel 1 [list expr $cond]]
}
proc detail {} { return [.instdetail.body get 1.0 end] }
proc last_body {} { return [lindex $::fleetfix::gw(bodies) end] }

lassign [::fleetfix::serve] server port
::vapi::set_base "http://127.0.0.1:$port"
::vapi::set_token ""
set ::vinst::instances [dict create]

set fleet [::json::json2dict [::fleetfix::obs_fleet_json]]
set remote_id ""
set stale_id ""
set self_id ""
foreach n [dict get $fleet nodes] {
    set state [dict get $n health state]
    if {$state eq "self"} { set self_id [dict get $n id] }
    if {$state eq "stale"} { set stale_id [dict get $n id] }
    # b: the node the captured metrics, logs and drift answers are about.
    if {[dict get $n name] eq "b"} { set remote_id [dict get $n id] }
}

puts "\nthe registry poll keeps the observation fields"
::vinstpane::build_list .instlist
::vinstpane::build_detail .instdetail
::vinstpane::build_ctl .instctl
::vinstpane::refresh_fleet
ok "the captured /api/fleet carries a remote, a stale and a self row" {
    $remote_id ne "" && $stale_id ne "" && $self_id ne ""
}
ok "a registry poll through the real transport fills the rows" {
    [pump {[llength $::vinstpane::fleet_nodes] == [llength [dict get $::fleet nodes]]}]
}
ok "…and keeps the alerts, the version spread and the forgotten nodes" {
    [llength [::vapi::field $::vinstpane::fleet_alerts recent {}]] > 0
    && [::vapi::field $::vinstpane::fleet_versions drift 0] in {true 1}
    && [llength $::vinstpane::fleet_forgotten] > 0
}

puts "\nthe detail pane"
::vinstpane::_select cluster:$remote_id
set d [detail]
ok "metrics are offered, not invented" { [string match "*Metrics: not asked yet*" $d] }
ok "the registry's version drift is named, with the nodes" {
    [string match "*Version drift in the registry: 0.2.0 (ghost (stale), b, a); 0.1.6 (phone)*" $d]
}
ok "a node with no health change says so, and that alerts are off" {
    [string match "*Health alerts (listed here only;*fleet_alerts is off):*" $d]
    && [string match "*none since this gateway started*" $d]
}
::vinstpane::_select cluster:$stale_id
set d [detail]
ok "the stale node's change is listed, with what happened to it" {
    [string match "*fresh → stale*not sent:*fleet_alerts is off*" $d]
}
::vinstpane::_select cluster:$self_id
set d [detail]
ok "this gateway's row lists every node's changes, by name" {
    [string match "*fresh → stale ghost*" $d]
}
ok "this gateway's row lists the gossip peers' last exchange" {
    [string match "*Gossip with *" $d]
}
ok "…and the nodes forgotten fleet-wide" {
    [string match "*Forgotten fleet-wide (refused until they heartbeat again): *" $d]
}

puts "\nthe controls"
::vinstpane::_select cluster:$remote_id
ok "a registry row offers Logs…, Metrics and Drift…" {
    ![.instctl.logs instate disabled] && ![.instctl.metrics instate disabled]
    && ![.instctl.drift instate disabled]
}
ok "Gossip now is offered while gossip peers are configured" {
    ![.instctl.gossip instate disabled]
}
# Repainted from that enabled state, so a control the row leaves on from
# before is caught too.
set _nogossip $::vinstpane::fleet_sync
dict set ::vinstpane::fleet_sync gossip_peers {}
::vinstpane::_select cluster:$remote_id
ok "…and without gossip peers there is nothing to gossip with" {
    [.instctl.gossip instate disabled] && ![.instctl.logs instate disabled]
}
set ::vinstpane::fleet_sync $_nogossip
::vinstpane::_select cluster:$remote_id
set ::vinst::instances [dict create alpha [dict create url http://127.0.0.1:1 token t]]
::vapi::bind_instance alpha http://127.0.0.1:1 t
# The row must exist in the tree, or the selection the widget still
# holds is re-asserted from inside the poll's nested event loop.
::vinstpane::refresh_list
::vinstpane::_select alpha
ok "a saved-instance row offers none of them (they are registry verbs)" {
    [.instctl.logs instate disabled] && [.instctl.metrics instate disabled]
    && [.instctl.drift instate disabled] && [.instctl.gossip instate disabled]
}
set ::vinst::instances [dict create]
::vinstpane::refresh_list
::vinstpane::_select cluster:$remote_id

puts "\nmetrics"
set ::fleetfix::gw(bodies) {}
ok "Metrics asks the gateway for the selected node, by id" {
    [::vinstpane::_ctl_metrics] == 1
    && [pump {[info exists ::vfleetobs::metrics($::remote_id)]}]
    && [dict get [last_body] node] eq $remote_id
}
set d [detail]
ok "the answer lands in the detail pane: cores, memory, disk, IO pressure" {
    [string match "*CPU: * cores, load *" $d] && [string match "*Memory: *" $d]
    && [string match "*Disk: * free of *" $d] && [string match "*IO pressure: *" $d]
}
ok "…uptime, the build and its revision, and the work in flight" {
    [string match "*Uptime: host *" $d] && [string match "*Build: veles *, revision *" $d]
    && [string match "*In flight: *" $d]
}

puts "\nthe log viewer"
set ::fleetfix::gw(bodies) {}
::vinstpane::_ctl_logs
ok "Logs… opens the viewer and asks for the default 100 lines, all levels" {
    [winfo exists .fleetlogs]
    && [pump {[llength $::fleetfix::gw(bodies)] > 0}]
    && [dict get [last_body] node] eq $remote_id
    && [dict get [last_body] lines] == 100 && [dict get [last_body] level] eq "all"
}
ok "the node's lines are shown, as redacted on that node" {
    [pump {[string match "*<REDACTED>*" [.fleetlogs.body get 1.0 end]]}]
}
ok "the heading says how many lines the node keeps" {
    [string match "*lines its gateway keeps in memory*" [.fleetlogs.head cget -text]]
}
set ::vfleetobs::logs_level warn
set ::vfleetobs::logs_lines 500
set ::fleetfix::gw(bodies) {}
::vfleetobs::refresh_logs
ok "the level and the line count are sent as chosen" {
    [pump {[llength $::fleetfix::gw(bodies)] > 0}]
    && [dict get [last_body] level] eq "warn" && [dict get [last_body] lines] == 500
}
pump {[string match "*<REDACTED>*" [.fleetlogs.body get 1.0 end]]}
set _before [.fleetlogs.body get 1.0 end]
::vfleetobs::_logs_done [expr {$::vfleetobs::logs_gen - 1}] [dict create lines [list [dict create msg stale]] held 1]
ok "a reply to an earlier request is dropped, not painted" {
    [.fleetlogs.body get 1.0 end] eq $_before
}
destroy .fleetlogs

puts "\ndrift"
set ::fleetfix::gw(bodies) {}
ok "Drift… compares the fleet and opens the report" {
    [::vinstpane::_ctl_drift] == 1
    && [pump {$::vfleetobs::drift ne "" && [winfo exists .fleetdrift]}]
}
set report [.fleetdrift.body get 1.0 end]
ok "the report names each version and who runs it" {
    [string match "*Version *: *" $report]
}
ok "…and per node what differs, by key name" {
    [string match "*Config *: *" $report] || [string match "*Drift: none*" $report]
}
ok "…and a node it could not ask, with the reason" {
    [string match "*phone:*Drift: not compared — no \\\[a2a_agents\\\] entry names phone*" $report]
    && [string match "*ghost:*Drift: not compared — stale: not asked*" $report]
}
ok "the selected node's drift is in its detail pane too" {
    [string match "*Drift: none — same version, revision and replicated config*" [detail]]
}
destroy .fleetdrift

# What differs, said by name: the captured answer for b with its values
# changed (the shape is the gateway's own; only the verdicts differ).
set _dd [::json::json2dict [::fleetfix::obs_drift_json]]
set _nodes {}
foreach n [dict get $_dd nodes] {
    if {[dict get $n id] eq $remote_id} {
        dict set n version_differs true
        dict set n version 0.1.9
        dict set n revision_differs true
        dict set n revision abc1234
        dict set n syncs_config false
        dict set n config keys differ {model.default}
        dict set n config keys only_here {agent.max_turns}
        dict set n config keys only_there {web.backends}
        dict set n config log there_ahead [list [dict create author laptop-id author_name laptop here 3 there 5]]
        dict set n config log here_ahead [list [dict create author vps-id here 9 there 7]]
        dict set n config same false
    }
    lappend _nodes $n
}
dict set _dd nodes $_nodes
set _dl [join [::vfleetobs::drift_node_lines $_dd $remote_id] \n]
ok "a differing node names its version and revision against this gateway's" {
    [string match "*Drift: runs version 0.1.9, this gateway 0.2.0*" $_dl]
    && [string match "*Drift: built from revision abc1234, this gateway from unreported-dirty-build*" $_dl]
}
ok "…the config keys that differ or exist on one side only" {
    [string match "*Config differs: model.default*" $_dl]
    && [string match "*Config only on this gateway: agent.max_turns*" $_dl]
    && [string match "*Config only on that node: web.backends*" $_dl]
}
ok "…whose config-log rows each side has more of, by name where known" {
    [string match "*that node has laptop's changes up to 5, this gateway up to 3*" $_dl]
    && [string match "*this gateway has vps-id's changes up to 9, that node up to 7*" $_dl]
}
ok "…and that it is not a memsync peer, so its config is not synced" {
    [string match "*not a memsync peer of this gateway*" $_dl]
    && ![string match "*Drift: none*" $_dl]
}

puts "\ngossip now"
set ::fleetfix::gw(bodies) {}
set ::fleetfix::gw(log) {}
ok "Gossip now asks the gateway to exchange with its gossip peers" {
    [::vinstpane::_ctl_gossip] == 1
    && [pump {[string match "Gossip: *" $::vstatus::last]}]
}
ok "…says who answered, and refreshes the registry" {
    [string match "*peer(s) answered*" $::vstatus::last]
    && [pump {[lsearch -exact $::fleetfix::gw(log) /api/fleet] >= 0}]
}

puts "\nforget is fleet-wide, and says so"
set ::_confirm_detail ""
proc ::vinstpane::_confirm {message detail} { set ::_confirm_detail $detail ; return 0 }
::vinstpane::_select cluster:$stale_id
::vinstpane::_ctl_forget
ok "the confirmation says the forget travels by gossip and what an old peer does" {
    [string match "*fleet-wide*" $::_confirm_detail]
    && [string match "*older than 0.2.0*" $::_confirm_detail]
}

puts "\nthe phone layout"
namespace eval ::vandroid {
    variable fits {}
    variable flows {}
    proc on {} { return 1 }
    proc fit {w width height {frac 0.15}} { lappend ::vandroid::fits [list $w $width $height] ; return {} }
    proc flow {f args} { lappend ::vandroid::flows $f }
}
destroy .instctl
::vinstpane::build_ctl .instctl
ok "the controls strip wraps into rows on a phone, the observation buttons in it" {
    ".instctl" in $::vandroid::flows
    && [winfo exists .instctl.logs] && [winfo exists .instctl.metrics]
    && [winfo exists .instctl.drift] && [winfo exists .instctl.gossip]
}
::vinstpane::_select cluster:$remote_id
::vinstpane::_ctl_logs
ok "the log viewer is fitted to the phone in dp, not placed in desktop pixels" {
    [lsearch -index 0 $::vandroid::fits .fleetlogs] >= 0
}
ok "…and still shows the node's log there" {
    [pump {[string match "*<REDACTED>*" [.fleetlogs.body get 1.0 end]]}]
}
destroy .fleetlogs
::vfleetobs::show_drift
ok "the drift report is fitted the same way" {
    [lsearch -index 0 $::vandroid::fits .fleetdrift] >= 0
}
destroy .fleetdrift
set ::vandroid::fits {}
unset -nocomplain ::vfleetobs::metrics($remote_id)
::vinstpane::_ctl_metrics
ok "metrics need no dialog on a phone: they land in the detail pane" {
    [pump {[info exists ::vfleetobs::metrics($::remote_id)]}]
    && [string match "*CPU: *" [detail]] && ![llength $::vandroid::fits]
}

close $server
file delete -force $::env(VELES_HOME)
puts "\nfleetobstest: $passed passed, $failed failed"
exit [expr {$failed > 0}]
