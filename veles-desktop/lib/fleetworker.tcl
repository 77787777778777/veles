# Headless fleet deployment worker. Invoked by fleetdeployui.tcl with a
# private stdin pipe. Never put instance credentials in argv or the journal.
package require json
set lib [file dirname [file normalize [info script]]]
# safeexec: instpane's ssh_argv passes host and unit through ::vsafe::arg.
# The desktop sources it in main.tcl; this separate process must too, or
# every restart fails AFTER the signed install switched `current` (F1-04).
source [file join $lib safeexec.tcl]
source [file join $lib jsonfield.tcl]
source [file join $lib paths.tcl]
source [file join $lib instances.tcl]
source [file join $lib instpane.tcl]
source [file join $lib fleetdeploy.tcl]
proc fail {message} {
    set safe $message
    if {[llength [info commands ::vfleetdeploy::_safe_error]]} {
        set safe [::vfleetdeploy::_safe_error $message]
    }
    puts [::vfleetjson::encode [::vfleetjson::operation_value [dict create ok 0 error $safe]]]
    exit 0
}
if {[catch {set request [::json::json2dict [read stdin]]} why]} { fail "worker request is not valid JSON" }
if {[catch {dict size $request} size] || $size == 0} { fail "worker request is not an object" }
foreach key {operation action journal_root registry} {
    if {![dict exists $request $key]} { fail "worker request lacks $key" }
}
set action [dict get $request action]
if {$action ni {stage verify continue rollback finish-rollback abandon}} { fail "unknown worker action" }
if {[catch {::vfleetjournal::configure [dict get $request journal_root]} why]} { fail $why }
set ::vfleetdeploy::instances [dict get $request registry]
set secrets {}
dict for {name spec} [dict get $request registry] {
    if {[dict exists $spec token]} { lappend secrets [dict get $spec token] }
}
set id [dict get $request operation id]
if {![regexp {^[A-Za-z0-9_-]{1,80}$} $id]} { fail "invalid operation id" }
if {[catch {::vfleetdeploy::load $id} op]} { fail $op }
if {[dict get $op plan_sig] ne [dict get $request operation plan_sig]} {
    fail "request operation identity does not match its durable journal"
}
foreach target [dict get $op manifest targets] {
    set instance [dict get $target instance]
    if {![dict exists $request registry $instance]} {
        fail "reviewed instance $instance is missing from the registry"
    }
    set spec [dict get $request registry $instance]
    foreach key {node_id url host unit} {
        set manifest_key $key
        if {$key eq "url"} { set manifest_key gateway_url }
        if {[dict get $spec $key] ne [dict get $target $manifest_key]} {
            fail "registry binding for $instance changed since the manifest was reviewed"
        }
    }
    ::vapi::bind_instance $instance [dict get $spec url] [dict get $spec token]
    ::vinst::add $instance [dict get $spec url] [dict get $spec token]
}
set node ""
if {[dict exists $request node_id]} { set node [dict get $request node_id] }
if {$action ni {continue finish-rollback abandon} && $node eq ""} { fail "worker action requires a node id" }
if {[catch {
    switch -- $action {
        stage { set result [::vfleetdeploy::stage $op $node] }
        verify { set result [::vfleetdeploy::verify $op $node] }
        continue { set result [::vfleetdeploy::continue $op] }
        rollback { set result [::vfleetdeploy::rollback $op $node] }
        finish-rollback { set result [::vfleetdeploy::finish_rollback $op] }
        abandon { set result [::vfleetdeploy::abandon $op] }
    }
} why]} {
    if {[catch {::vfleetjournal::get $id} saved]} { set saved [dict get $request operation] }
    # `diagnostic` is the operator's copy (scrubbed, capped, never
    # journaled); `error` is the bounded phrase the journal also keeps.
    puts [::vfleetjson::encode [::vfleetjson::operation_value [dict create ok 0 error [::vfleetdeploy::_safe_error $why] diagnostic [::vfleetdeploy::diagnostic $why $secrets] operation $saved]]]
    exit 0
}
puts [::vfleetjson::encode [::vfleetjson::operation_value [dict create ok 1 operation $result]]]
exit 0
