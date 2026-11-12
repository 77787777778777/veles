# veles-desktop/lib/fleetmanagetest.tcl — the Instances tab's node
# management (lib/fleetmanage.tcl, 0.2.2): Restart over the cluster link
# and the wait for the node to come back, one node's config (read, write,
# what is refused), Probe all and Sync all with one confirmation that
# lists the targets, the audit log, node health alerts in the window and
# as rows, and the deployment-consistency verdict.
#
# WHAT IS REAL. api.tcl's transport, instpane.tcl, fleetobs.tcl and
# fleetmanage.tcl run unmodified against the loopback fake gateway in
# fleetfixtures.tcl. The management routes answer with bodies CAPTURED
# from three clustered scratch gateways of this build (the audit's smoke
# harness, 2026-11-10: the laptop's desktop API asking the phone, which
# ran under veles-gw, and the vps, which ran under nothing), never with
# shapes written from the Rust source (pitfall 101). The fake changes
# only what a restart changes: the boot.
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

set ::env(VELES_HOME) [file join /tmp veles-fleetmanage-test-[pid]]
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
source [file join $_dir fleetmanage.tcl]
source [file join $_dir fleetfixtures.tcl]
source [file join $_dir fleetmanagefixtures.tcl]

proc pump {cond {ms 8000}} {
    set deadline [expr {[clock milliseconds] + $ms}]
    while {![uplevel 1 [list expr $cond]] && [clock milliseconds] < $deadline} {
        after 20 { set ::_tick 1 }
        vwait ::_tick
    }
    return [uplevel 1 [list expr $cond]]
}
proc detail {} { return [.instdetail.body get 1.0 end] }
proc asked {path} { return [expr {[lsearch -exact $::fleetfix::gw(log) $path] >= 0}] }
proc body_of {path} {
    set out ""
    foreach {p b} $::fleetfix::mg(bodies) { if {$p eq $path} { set out $b } }
    return $out
}
proc reset_log {} {
    set ::fleetfix::gw(log) {}
    set ::fleetfix::mg(bodies) {}
    ::fleetfix::manage_reset
}
# The confirmation: records what it says, answers as told.
set ::_confirm_answer 1
proc ::vinstpane::_confirm {message detail} {
    set ::_confirm_message $message
    set ::_confirm_detail $detail
    return $::_confirm_answer
}

lassign [::fleetfix::serve] server port
::vapi::set_base "http://127.0.0.1:$port"
::vapi::set_token ""
set ::vinst::instances [dict create]

::vinstpane::build_list .instlist
::vinstpane::build_detail .instdetail
::vinstpane::build_ctl .instctl
::vinstpane::refresh_fleet
pump {[llength $::vinstpane::fleet_nodes] > 0}
set remote_id "" ; set stale_id "" ; set self_id ""
foreach n $::vinstpane::fleet_nodes {
    switch -- [dict get $n health state] {
        self  { set self_id [dict get $n id] }
        stale { set stale_id [dict get $n id] }
    }
    if {[dict get $n name] eq "b"} { set remote_id [dict get $n id] }
}

puts "\nthe controls"
::vinstpane::_select cluster:$remote_id
ok "a live registry row offers Restart and Config…" {
    ![.instctl.restart instate disabled] && ![.instctl.config instate disabled]
}
ok "Probe all…, Sync all… and Audit log… are offered with a registry" {
    ![.instctl.bulkprobe instate disabled] && ![.instctl.bulksync instate disabled]
    && ![.instctl.audit instate disabled]
}
::vinstpane::_select cluster:$stale_id
ok "a stale row offers no Restart (it would not answer), still Config…" {
    [.instctl.restart instate disabled] && ![.instctl.config instate disabled]
}

puts "\nrestart"
reset_log
set ::fleetfix::mg(status) $::fleetfix::manage(status_none)
::vfleetmanage::restart $remote_id b
ok "a node that knows no supervisor: the reason is shown, nothing is sent" {
    [pump {[string match "*cannot restart itself*" $::vstatus::last]}]
    && ![asked /api/fleet/restart]
}
ok "…in the node's own words" {
    [string first [dict get [::json::json2dict $::fleetfix::manage(status_none)] supervisor detail] $::vstatus::last] >= 0
}
reset_log
set ::fleetfix::mg(status) $::fleetfix::manage(status_gw)
set ::_confirm_message ""
set ::_confirm_answer 0
::vfleetmanage::restart $remote_id b
ok "a supervised node is restarted only after a confirmation that names its supervisor" {
    [pump {[string match "*cancelled*" $::vstatus::last]}]
    && [string match "Restart b?" $::_confirm_message]
    && [string match "*veles-gw*" $::_confirm_detail]
    && ![asked /api/fleet/restart]
}
reset_log
set ::_confirm_answer 1
::vfleetmanage::restart $remote_id b
ok "confirmed, the restart is sent with confirm and without force" {
    [pump {[asked /api/fleet/restart]}]
    && [dict get [body_of /api/fleet/restart] confirm] in {true 1}
    && [dict get [body_of /api/fleet/restart] force] in {false 0}
}
ok "…and the tab waits for the node to come back with a new boot" {
    [pump {[string match "*is back after*" $::vstatus::last]} 20000]
}
ok "…saying which version came back" {
    [string match "*(veles 0.2.1)*" $::vstatus::last]
}
reset_log
set ::fleetfix::mg(status) [string map {{"turns_total":0} {"turns_total":2}} $::fleetfix::manage(status_gw)]
::vfleetmanage::restart $remote_id b
ok "running turns: the confirmation says how many, and what resumes" {
    [pump {[asked /api/fleet/restart]}]
    && [string match "*running 2 turn(s)*" $::_confirm_message]
    && [string match "*Desktop turns continue*" $::_confirm_detail]
}
ok "…and a yes is sent as force" {
    [dict get [body_of /api/fleet/restart] force] in {true 1}
}
pump {![info exists ::vfleetmanage::busy(restart)]} 20000

puts "\none node's config"
reset_log
::vfleetmanage::open_config $remote_id b
set ::vfleetmanage::config_key model.default
set ::vfleetmanage::config_value x
::vfleetmanage::config_write 0
ok "a write before a read is refused here: it must name the value it replaces" {
    [string match "Read the key first*" [.fleetconfig.result cget -text]]
    && ![asked /api/fleet/config]
}
::vfleetmanage::config_read
ok "Read shows the key as the node has it" {
    [pump {[string match "model.default = mock-model on phone*" [.fleetconfig.result cget -text]]}]
    && $::vfleetmanage::config_value eq "mock-model"
}
ok "…and keeps the hash a write names" { [string length $::vfleetmanage::config_hash] == 16 }
set ::vfleetmanage::config_value fast-model
::vfleetmanage::config_write 0
ok "Write asks first, naming the node, the key and the value" {
    [pump {[asked /api/fleet/config] && [dict exists [body_of /api/fleet/config] raw]}]
    && [string match "Set model.default on b to fast-model?" $::_confirm_message]
}
set b [body_of /api/fleet/config]
ok "…and sends the text, the hash it read and confirm" {
    [dict get $b raw] eq "fast-model" && [dict get $b expect] eq $::fleetfix::mg(read_hash)
    && [dict get $b confirm] in {true 1} && [dict get $b key] eq "model.default"
}
ok "…and says what the node did with it" {
    [pump {[string match "b: *written in its config.toml*" [.fleetconfig.result cget -text]]}]
}
set held [::json::json2dict $::fleetfix::manage(config_held)]
ok "a held section reads with what it means: written where it is edited, applied by its owner" {
    [string match "*held section*" [join [::vfleetmanage::config_lines $held] \n]]
}
set withheld [dict replace $held withheld true set true value null key platforms.telegram.token]
ok "a credential is never shown" {
    [string match "*holds a credential*" [join [::vfleetmanage::config_lines $withheld] \n]]
}
reset_log
set ::vfleetmanage::config_key acl.default
set ::vfleetmanage::config_value all
set ::vfleetmanage::config_hash $::fleetfix::mg(read_hash)
::vfleetmanage::config_write 0
ok "the node's refusal of a held section reaches the dialog in its words" {
    [pump {[string match "*held section*" [.fleetconfig.result cget -text]]}]
}
destroy .fleetconfig

puts "\nbulk"
reset_log
set ::_confirm_answer 1
::vfleetmanage::bulk probe
ok "Probe all lists its targets in one confirmation before anything runs" {
    [pump {[string match "*Bulk probe*" $::vstatus::last]} 10000]
    && [string match "Probe 2 node(s)?" $::_confirm_message]
    && [string match "*phone*" $::_confirm_detail] && [string match "*vps*" $::_confirm_detail]
}
ok "…then runs once, confirmed, and says per node how it went" {
    [llength [lsearch -all $::fleetfix::gw(log) /api/fleet/bulk]] == 2
    && [dict get [body_of /api/fleet/bulk] confirm] in {true 1}
    && [string match "*✓ phone*" $::vstatus::last]
}
reset_log
set ::_confirm_answer 0
::vfleetmanage::bulk sync config
ok "Sync all of a kind names the kind, and a no runs nothing" {
    [pump {[string match "*cancelled*" $::vstatus::last]}]
    && [string match "Sync config to 2 node(s)?" $::_confirm_message]
    && [llength [lsearch -all $::fleetfix::gw(log) /api/fleet/bulk]] == 1
}
set ::_confirm_answer 1

puts "\nthe audit log"
::vfleetmanage::open_audit
ok "Audit log… lists who did what to which node, and how it went" {
    [pump {[string match "*restart*" [.fleetaudit.body get 1.0 end]]}]
    && [string match "*desktop (owner)*" [.fleetaudit.body get 1.0 end]]
}
destroy .fleetaudit

puts "\nalerts reach the window"
. configure -width 800 -height 600
update
set ::fleetfix::mg(alerts) $::fleetfix::manage(alerts)
::vfleetmanage::_alerts_answer 1 [::json::json2dict $::fleetfix::manage(alerts)]
update
ok "the first answer is the baseline: nothing replayed on start" {
    $::vfleetmanage::last_toast eq ""
    && (![winfo exists .fleettoast] || ![winfo ismapped .fleettoast])
}
set newer [string map [list $::fleetfix::mg(alert_at) 2099-01-01T00:00:00+00:00] $::fleetfix::manage(alerts)]
::vfleetmanage::_alerts_answer 1 [::json::json2dict $newer]
update
ok "a new alert is shown in the window, on any tab" {
    [winfo ismapped .fleettoast]
    && [string match "⚠ veles fleet: node vps *" [.fleettoast cget -text]]
}
ok "…and in the status bar" { [string match "veles fleet: node *" $::vstatus::last] }
ok "…once" {
    [::vfleetmanage::_alerts_answer 1 [::json::json2dict $newer]] eq ""
    && [llength [::vfleetmanage::new_alerts [::vapi::field [::json::json2dict $newer] recent] $::vfleetmanage::alerts_seen]] == 0
}
::vinstpane::refresh_fleet
pump {[llength [::vapi::field $::vinstpane::fleet_alerts recent {}]] > 0}
::vinstpane::refresh_list
set rows [.instlist.list children $::vinstpane::alerts_group]
ok "the Instances list has a Health alerts row per alert" { [llength $rows] >= 1 }
set row [lindex $rows 0]
set ::vinstpane::selected ""
.instlist.list selection set $row
update
ok "…and choosing one selects its node, whose detail lists it" {
    [string match "cluster:*" $::vinstpane::selected]
    && [string match "*Health alerts*" [detail]]
}

puts "\ndeployment consistency"
set drift [::json::json2dict $::fleetfix::manage(drift)]
set lines [join [::vfleetobs::drift_lines $drift] \n]
ok "Drift… leads with the consistency verdict the gateway gave" {
    [string match "*Deployment consistency unknown*" $lines]
    || [string match "*The fleet is*" $lines]
}
set bad [dict create checked true consistent false compared 3 \
    flagged [list [dict create id $remote_id name b differs {{revision r0} {user plugin anthropic shadows the bundled one (user bbbb, bundled aaaa)}}]] \
    unknown {} shadows {}]
set ::vinstpane::fleet_consistency $bad
::vinstpane::_select cluster:$remote_id
ok "a node that differs says what differs in its detail" {
    [string match "*The fleet is NOT consistent: 1 node(s) differ.*" [detail]]
    && [string match "*b differs: revision r0; user plugin anthropic shadows*" [detail]]
}
::vinstpane::_select cluster:$self_id
ok "…and the others' details carry the verdict only" {
    [string match "*NOT consistent*" [detail]] && ![string match "*b differs*" [detail]]
}
dict set bad consistent true
dict set bad flagged {}
dict set bad unknown [list [dict create id $stale_id name ghost why {stale: not asked}]]
dict set bad consistent false
ok "an unasked node makes it unknown, never consistent" {
    [string match "*unknown: 1 node(s)*" [join [::vfleetmanage::consistency_lines $bad] \n]]
}
ok "not checked yet is said as such" {
    [string match "*not checked yet*" [join [::vfleetmanage::consistency_lines {}] \n]]
}
# A node that lacks an item the others have (fleet_obs::consistency):
# the gateway names the item with the nodes that lack it, and the fleet
# is consistent all the same — the verdict is the point release and the
# commit, not what a platform loads (the owner, 2026-11-12; 0.2.2 said
# "consistent with differences", and no real fleet could read
# "consistent"). The Instances detail of the node that lacks it names
# the item; another node's detail carries the verdict only.
set diff [dict create checked true consistent true verdict consistent \
    compared 3 flagged {} unknown {} shadows {} \
    missing [list [dict create item module:racket on {laptop vps} \
        lacking [list [dict create id $remote_id name winbox]]]]]
set out [join [::vfleetmanage::consistency_lines $diff] \n]
ok "a node lacking a module leaves the fleet consistent" {
    [string match "*The fleet is consistent: 3 node(s) run the same release and commit.*" $out]
    && ![string match "*with differences*" $out]
    && ![string match "*unknown*" $out]
    && ![string match "*NOT consistent*" $out]
}
# The same fleet as a 0.2.2 gateway answers it (consistent false, its
# own verdict word): the desktop reads it by today's rule.
set old [dict replace $diff consistent false verdict consistent_with_differences]
set old_out [join [::vfleetmanage::consistency_lines $old] \n]
ok "…also when a 0.2.2 gateway says it its way" {
    [string match "*The fleet is consistent: 3 node(s) run the same release and commit.*" $old_out]
    && [string match "*module:racket is not on: winbox*" $old_out]
    && ![string match "*unknown*" $old_out]
}
# No verdict word, nothing flagged, nothing unknown, not consistent: the
# desktop does not make "consistent" up.
ok "…and never without the gateway's word" {
    [string match "*unknown*" [join [::vfleetmanage::consistency_lines \
        [dict create checked true consistent false compared 3 flagged {} unknown {} shadows {} \
            missing [dict get $diff missing]]] \n]]
}
ok "…and the item is named with the node that lacks it" {
    [string match "*module:racket is not on: winbox*" $out]
}
set ::vinstpane::fleet_consistency $diff
::vinstpane::_select cluster:$remote_id
ok "…in that node's detail" {
    [string match "*module:racket is not on: winbox*" [detail]]
}
::vinstpane::_select cluster:$self_id
ok "…and not in another node's" {
    [string match "*The fleet is consistent: 3 node(s)*" [detail]]
    && ![string match "*module:racket is not on*" [detail]]
}

close $server
file delete -force $::env(VELES_HOME)
puts "\nfleetmanagetest: $passed passed, $failed failed"
exit [expr {$failed > 0}]
