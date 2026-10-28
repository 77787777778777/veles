# veles-desktop/lib/devicetest.tcl — the window's half of the device
# bridge (PLAN-sessions-and-phone.md A0/A1): what it announces, how a
# call reaches its provider, and what goes back.
#
# Run:  wish veles-desktop/lib/devicetest.tcl   (under Xvfb: scripts/tcl_tests.sh)

package require Tk
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
if {![info exists ::env(XVFB_RUNNING)] && ![info exists ::env(VELES_TK_TEST_FORCE)]} {
    puts "SKIP: refusing to map a window on a real display (run under Xvfb)."
    exit 0
}

source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
source [file join $_dir safeexec.tcl]
source [file join $_dir device.tcl]
source [file join $_dir device_android.tcl]
source [file join $_dir device_desktop.tcl]
namespace eval ::vstatus { proc set_msg {m} {} }
namespace eval ::vi18n { proc mc {s args} { return [format $s {*}$args] } }

# The gateway, recorded: every POST, rendered as the JSON it would send.
set ::posts {}
proc ::_passive {} {
    expr {[info exists ::vapi::passive] && $::vapi::passive}
}
proc ::vapi::post_json {path body} {
    lappend ::posts [list $path [::vapi::_json_value $body] [::_passive]]
    return {}
}
set ::opened_streams {}
proc ::vapi::sse_open {path handler} {
    lappend ::opened_streams [list $path $handler [::_passive]]
    return tok1
}
proc ::vapi::sse_close {tok} {}
proc ::vapi::unattached {} { return 0 }

puts "\nthe window announces what it can do"
::vdevice::provide echo "Say it back." \
    {{"type":"object","properties":{"text":{"type":"string"}}}} \
    [list apply {{a} { return "echo: [dict get $a text]" }}]
::vdevice::provide boom "Always fails." {} [list apply {{a} { error "it broke" }}]
::vdevice::start
ok "the request stream is opened for this client" {
    [string match "/api/device/events?client_id=*" [lindex $::opened_streams 0 0]]
}
::vdevice::_event device-live {}
set hello [lindex $::posts end]
ok "the stream's first event sends the catalog" {
    [lindex $hello 0] eq "/api/device/hello"
}
# Keeping its own line open is not a person using the gateway: a phone's
# engine must still idle out with veles open and untouched.
ok "the stream and the catalog are passive requests" {
    [lindex $::opened_streams 0 2] && [lindex $hello 2] && ![::_passive]
}
set h [::json::json2dict [lindex $hello 1]]
ok "…naming this device and platform" {
    [dict get $h name] eq [::vdevice::device_name]
    && [dict get $h platform] eq [::vdevice::platform]
}
set names {}
foreach c [dict get $h capabilities] { lappend names [dict get $c name] }
ok "…with every provider, schemas intact" {
    "echo" in $names && "boom" in $names
    && [dict get [lindex [dict get $h capabilities] [lsearch $names echo]] schema properties text type] eq "string"
}

puts "\na call reaches its provider and the answer goes back"
set ::posts {}
::vdevice::_event request [dict create id r1 action echo args [dict create text hello]]
update
set res [lindex $::posts end]
set r [::json::json2dict [lindex $res 1]]
ok "the answer is posted to /api/device/result under the call's id" {
    [lindex $res 0] eq "/api/device/result" && [dict get $r id] eq "r1"
}
ok "…ok, with the provider's text" {
    [dict get $r ok] && [dict get $r result] eq "echo: hello"
}
set ::posts {}
::vdevice::_event request [dict create id r2 action boom args {}]
update
set r [::json::json2dict [lindex [lindex $::posts end] 1]]
ok "a provider's error goes back as a refusal the agent reads" {
    ![dict get $r ok] && [dict get $r result] eq "it broke"
}
set ::posts {}
::vdevice::_event request [dict create id r3 action nothing args {}]
update
set r [::json::json2dict [lindex [lindex $::posts end] 1]]
ok "an action this window does not have is refused by name" {
    ![dict get $r ok] && [string match "*no action nothing*" [dict get $r result]]
}

puts "\non a phone the providers are borg's"
set ::borg_calls {}
proc ::borg {args} { lappend ::borg_calls $args ; return "" }
::vdevice::android::register
ok "a phone offers notify, speak, open, share and location" {
    [dict exists $::vdevice::caps notify] && [dict exists $::vdevice::caps speak]
    && [dict exists $::vdevice::caps open] && [dict exists $::vdevice::caps share]
    && [dict exists $::vdevice::caps location]
}
set ::posts {}
::vdevice::_event request [dict create id a1 action open args [dict create uri https://example.org]]
update
ok "open fires a VIEW intent" {
    [lindex $::borg_calls end] eq {activity android.intent.action.VIEW https://example.org {}}
}
set ::borg_calls {}
::vdevice::_event request [dict create id a1s action share args [dict create text "hello there"]]
update
set c [lindex $::borg_calls end]
ok "share's text is an extra, not a component" {
    [lindex $c 1] eq "android.intent.action.SEND" && [lindex $c 3] eq "text/plain"
    && [lindex $c 5] eq "" && [lindex $c 6] eq {android.intent.extra.TEXT {hello there}}
}
set ::borg_calls [list [lindex $::borg_calls 0]]
::vdevice::_event request [dict create id a2 action open args [dict create uri javascript:alert(1)]]
update
set r [::json::json2dict [lindex [lindex $::posts end] 1]]
ok "…and refuses a scheme it was not built to open" {
    ![dict get $r ok] && [llength $::borg_calls] == 1
}
::vdevice::_event request [dict create id a3 action speak args [dict create text hi lang de]]
update
ok "speak passes its language through" {
    [lindex $::borg_calls end] eq {speak hi de}
}

# Location: asked for the first time the agent wants it, not before.
set ::granted 0
proc ::borg {args} {
    lappend ::borg_calls $args
    switch -- [lindex $args 0] {
        checkpermission { return [list $::granted $::granted] }
        location {
            if {[lindex $args 1] eq "get"} { return {gps {latitude 1.5 longitude 2.5}} }
        }
    }
    return ""
}
set ::borg_calls {}
set ::posts {}
::vdevice::_event request [dict create id l1 action location args {}]
update
set r [::json::json2dict [lindex [lindex $::posts end] 1]]
ok "without permission, location asks the person and says so" {
    ![dict get $r ok] && [string match "*asking the person*" [dict get $r result]]
    && [lindex $::borg_calls end end] == 1
    && ![string match "*location start*" $::borg_calls]
}
set ::granted 1
set ::borg_calls {}
::vdevice::_event request [dict create id l2 action location args [dict create wait 2]]
update
set r [::json::json2dict [lindex [lindex $::posts end] 1]]
ok "with it, the fix comes back" {
    [dict get $r ok] && [string match "*latitude 1.5*" [dict get $r result]]
}
rename ::borg {}

puts "\ndevicetest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
