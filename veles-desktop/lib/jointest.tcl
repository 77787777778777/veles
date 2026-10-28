# Joined desktop sessions: transport, target ownership and local return
# view. Real api.tcl/instances.tcl and the real _join_command; the HTTP
# boundary and window operations are small recording stubs.

package require json
set _dir [file dirname [file normalize [info script]]]
if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}
source [file join $_dir paths.tcl]
source [file join $_dir api.tcl]
source [file join $_dir instances.tcl]
source [file join $_dir attach.tcl]

set failed 0
set passed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name" ; incr ::passed
    } else {
        puts "  ✗ $name" ; incr ::failed
    }
}
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} {return [format $s {*}$args]} ; return $s} }
namespace eval ::vstatus { variable last "" ; proc set_msg {m} {set ::vstatus::last $m} }
namespace eval ::vstatus { proc set_attachment {spec args} {} }
namespace eval ::vtabs { proc select {name args} {} }
source [file join $_dir attach.tcl]
proc ::vapp::rebuild_regions {} {}
namespace eval ::vchat {
    variable notes {}
}
source [file join $_dir chat.tcl]
proc ::vchat::emit_message {kind text args} { lappend ::vchat::notes [list $kind $text] }
proc ::vchat::open_session {sid} { set ::vchat::session $sid }
set ::vchat::session mine

set _home [file join $::env(TMPDIR) veles-jointest-[pid]]
file delete -force $_home
file mkdir $_home
set ::env(XDG_DATA_HOME) $_home
unset -nocomplain ::env(VELES_HOME)
set ::env(VELES_PROFILE) jointest
::vapi::bind_instance laptop http://127.0.0.1:8642 laptop-token
::vapi::bind_instance vps http://127.0.0.1:18642 vps-token
::vapi::use_instance laptop

namespace eval ::_http {
    variable calls {}
    variable fail_save 0
    variable chat_answer ""
    variable get_answer ""
}
rename ::vinst::save ::vinst::save_fixture_real
proc ::vinst::save {} {
    if {$::_http::fail_save} { set ::vinst::error "disk full (fixture)" ; return 0 }
    return [::vinst::save_fixture_real]
}
# Stub the wire BELOW post_json: chat_send posts its body directly.
rename ::vapi::post ::vapi::post_fixture_real
proc ::vapi::post {path body args} {
    lappend ::_http::calls [list [::vapi::attached] $path $body [::vapi::_url $path]]
    if {$path eq "/api/join"} {
        return {{"join": {"join_id": "j-e2e", "from_instance": "laptop", "from_session": "mine", "target_instance": "", "target_session": "alice"}}}
    }
    if {$path eq "/api/join/leave"} { return {{"ok": true}} }
    if {$path eq "/api/chat"} {
        if {$::_http::chat_answer ne ""} { return $::_http::chat_answer }
        return {{"started": true, "join": true}}
    }
    error "unexpected POST $path"
}
# Real post_json above the recording raw-post stub: typed JSON values
# must be encoded exactly as they are by the shipping client.

# A cross-node join is made ON the target, not on the attached instance.
# The real restart helper validates the target before reopening the
# joined view. Stub GET below the production JSON/URL layers.
rename ::vapi::get ::vapi::get_fixture_real
proc ::vapi::get {path {ms 0}} {
    lappend ::_http::calls [list [::vapi::attached] $path {} [::vapi::_url $path]]
    if {[string match "/api/join/status?*" $path]} {
        if {$::_http::get_answer ne ""} { return $::_http::get_answer }
        return {{"join":{"join_id":"j-restart","target_session":"alice","from_instance":"","from_session":""}}}
    }
    error "unexpected GET $path"
}
proc ::_http::reset {} { variable calls {} }
::_http::reset
::vchat::_join_command mine /join {vps/alice}
set _calls $::_http::calls
ok "join is issued against the named target gateway" {
    [llength $_calls] == 1
    && [lindex $_calls 0 0] eq "vps"
    && [lindex $_calls 0 1] eq "/api/join"
    && [lindex $_calls 0 3] eq "http://127.0.0.1:18642/api/join"
}
set d [::json::json2dict [lindex $_calls 0 2]]
ok "the request carries only the target and local return-view labels" {
    [dict get $d session_id] eq "alice"
    && [dict get $d instance] eq "laptop"
    && [dict get $d owner_session] eq "mine"
}
ok "the desktop moved to the target and kept only its return view" {
    [::vapi::attached] eq "vps"
    && $::vchat::session eq "alice"
    && [dict get [::vinst::remote_join] join_id] eq "j-e2e"
    && [dict get [::vinst::remote_join] from_instance] eq "laptop"
    && [dict get [::vinst::remote_join] from_session] eq "mine"
}
::vchat::_join_command alice /join {}
ok "a second join is refused while the first is active" {
    [llength $::_http::calls] == 1
}

# chat_send consults the local return view but POSTs to its target, and
# carries the id and attachment text to the shared gateway join path.
::_http::reset
set raw [::vapi::chat_send alice {owner line} [list {/tmp/a file.txt}]]
set d [::json::json2dict [lindex $::_http::calls 0 2]]
ok "input goes to the target with the active join id" {
    [lindex $::_http::calls 0 0] eq "vps"
    && [dict get $d join_id] eq "j-e2e"
    && [dict get $d session_id] eq "alice"
    && [dict get $d text] eq "owner line"
    && [lindex [dict get $d attachments] 0] eq "/tmp/a file.txt"
}
::vapi::chat_send other {not the joined view}
ok "another view cannot borrow the active join id" {
    [llength $::_http::calls] == 2
    && [dict get [::json::json2dict [lindex $::_http::calls 1 2]] join_id] eq "null"
}

::_http::reset
::vchat::_join_command alice /leave {}
set d [::json::json2dict [lindex $::_http::calls 0 2]]
ok "leave stays on the target and identifies the exact joined session" {
    [lindex $::_http::calls 0 0] eq "vps"
    && [lindex $::_http::calls 0 3] eq "http://127.0.0.1:18642/api/join/leave"
    && [dict get $d join_id] eq "j-e2e"
    && [dict get $d session_id] eq "alice"
}
ok "leave restores the local view and clears the join sidecar" {
    [::vapi::attached] eq "laptop"
    && $::vchat::session eq "mine"
    && [dict size [::vinst::remote_join]] == 0
}

# A registry save failure after a successful join must undo the target
# binding, on the target, and restore the old view.
::vinst::set_remote_join {}
set ::_http::fail_save 1
::_http::reset
::vchat::_join_command mine /join {vps/alice}
set _paths [lmap c $::_http::calls {lindex $c 1}]
ok "a failed local save rolls back the target join" {
    $_paths eq [list /api/join /api/join/leave]
    && [lindex $::_http::calls 1 0] eq "vps"
    && [::vapi::attached] eq "laptop"
    && [dict size [::vinst::remote_join]] == 0
}

# The real registry persists the sidecar, but not as a fake instance.
set ::_http::fail_save 0
set spec [dict create join_id j-restart instance vps target_session alice from_instance laptop from_session mine]
::vinst::set_remote_join $spec
set path [::vinst::path]
set ::vinst::remote_join {}
::vinst::load
ok "return-view state survives a real registry reload" {
    [file exists $path]
    && [::vinst::remote_join] eq $spec
    && [lsearch -exact [::vinst::names] __remote_join__] < 0
}
# Restart boot uses the real boot_decide and the real attach path.
::vinst::set_last_attached laptop
::vapi::use_instance ""
unset -nocomplain ::env(VELES_INSTANCE)
set boot [::vapp::boot_decide]
ok "restart reattaches to the saved joined target, not the origin" {
    $boot eq "vps"
    && [::vapi::attached] eq "vps"
}
::vapi::use_instance ""
set ::env(VELES_INSTANCE) laptop
set explicit [::vapp::boot_decide]
unset ::env(VELES_INSTANCE)
ok "an explicit instance still wins over saved join restoration" {
    $explicit eq "laptop"
    && [::vapi::attached] eq "laptop"
}

# Restart restoration is more than sidecar parsing. The target must
# still hold this exact join and admit the participant's current role.
::_http::reset
::vapi::use_instance vps
set restored [::vinst::restore_remote_join]
ok "a valid saved join revalidates on its target before reopening" {
    $restored == 1
    && [llength $::_http::calls] == 1
    && [lindex $::_http::calls 0 0] eq "vps"
    && [string match "/api/join/status?*" [lindex $::_http::calls 0 1]]
    && [::vapi::attached] eq "vps"
    && $::vchat::session eq "alice"
}
set ::_http::get_answer {{"error":"join j-restart is no longer valid under the current ACL"}}
set ::vchat::notes {}
::_http::reset
set restored [::vinst::restore_remote_join]
ok "a revoked saved join returns to the origin and clears its sidecar" {
    $restored == -1
    && [::vapi::attached] eq "laptop"
    && $::vchat::session eq "mine"
    && [dict size [::vinst::remote_join]] == 0
    && [string match "*current ACL*" [lindex $::vchat::notes end 1]]
}

# Drive the real composer with real text widgets under Xvfb. Stubs only
# replace the transcript's rendering and the wire; send_current itself,
# its busy branch and its joined slash-command boundary are shipping code.
proc ::vchat::log {} { return .joinlog }
proc ::vchat::_mark_running {sid on args} { set ::vchat::busy $on }
proc ::vchat::_refresh_chips {} {}
proc ::vchat::turn_error {msg} {
    # The production turn_error ends by clearing the running flag. Keep
    # that observable contract while omitting its unrelated transcript
    # reconciliation in this small fixture.
    set ::vchat::turn_error $msg
    ::vchat::_mark_running $::vchat::session 0
}
proc ::vchat::_sync_buttons {} {}
namespace eval ::vfind { proc reset_marks {} {} }
namespace eval ::vsidebar { proc refresh {} {} ; proc refresh_soon {} {} }
namespace eval ::vedit { proc hist_push {text} {} }
text .joinlog
frame .mid
frame .mid.chatwrap
frame .mid.chatwrap.chat
text .mid.chatwrap.chat.input
set ::vchat::session alice
set ::vchat::busy 1
::vinst::set_remote_join [dict create join_id j-e2e instance vps target_session alice from_instance laptop from_session mine]
::vapi::use_instance vps
::_http::reset
.mid.chatwrap.chat.input insert 1.0 {owner while busy}
::vchat::send_current
ok "busy joined input uses /api/chat with join_id, never /steer" {
    [llength $::_http::calls] == 1
    && [lindex $::_http::calls 0 1] eq "/api/chat"
    && [dict get [::json::json2dict [lindex $::_http::calls 0 2]] join_id] eq "j-e2e"
    && [dict get [::json::json2dict [lindex $::_http::calls 0 2]] text] eq "owner while busy"
}
::_http::reset
set ::vchat::busy 0
.mid.chatwrap.chat.input insert 1.0 {/secret set KEY never-send-this}
::vchat::send_current
ok "a joined slash command cannot run the ordinary local command path" {
    [llength $::_http::calls] == 0
    && [string match "*commands stay on your own session*" [lindex $::vchat::notes end 1]]
    && [string match "*never-send-this*" [lindex $::vchat::notes end 1]] == 0
}
set ::_http::chat_answer {{"error":"commands stay on your own session; /leave returns from the joined session"}}
::_http::reset
.mid.chatwrap.chat.input insert 1.0 {/steer must not steer}
::vchat::send_current
ok "the target rejects joined /steer without an ordinary turn" {
    [llength $::_http::calls] == 0
    && [string match "*commands stay on your own session*" [lindex $::vchat::notes end 1]]
}

# A palette/keyboard action calls slash_command without send_current.
# It must meet the same joined-session boundary.
namespace eval ::voverlay { variable opened {} ; proc open {what args} { set ::voverlay::opened $what } }
set ::_http::calls {}
set ::voverlay::opened {}
::vchat::slash_command /model
ok "a direct palette command cannot bypass the joined-session boundary" {
    $::voverlay::opened eq {}
    && [llength $::_http::calls] == 0
    && [string match "*commands stay on your own session*" [lindex $::vchat::notes end 1]]
}

# A rejected joined POST must not leave an optimistic running turn.
set ::_http::chat_answer {{"error":"join j-e2e is not active on this gateway"}}
::_http::reset
.mid.chatwrap.chat.input insert 1.0 {this send is refused}
::vchat::send_current
ok "a rejected joined send does not remain a running turn" {
    [llength $::_http::calls] == 1
    && [lindex $::_http::calls 0 1] eq "/api/chat"
    && $::vchat::busy == 0
    && [string match "*not active*" [lindex $::vchat::notes end 1]]
}

# /leave remains available while the joined session is busy. The target
# returns a handled response, not an ordinary streaming turn.
set ::_http::chat_answer {{"handled":true,"join":true,"text":"left alice; what you write here is your own session's again"}}
::_http::reset
set ::vchat::busy 1
.mid.chatwrap.chat.input insert 1.0 {/leave}
::vchat::send_current
ok "busy joined /leave uses the target and restores the origin" {
    [llength $::_http::calls] == 1
    && [lindex $::_http::calls 0 1] eq "/api/chat"
    && [dict get [::json::json2dict [lindex $::_http::calls 0 2]] text] eq "/leave"
    && [::vapi::attached] eq "laptop"
    && $::vchat::session eq "mine"
    && $::vchat::busy == 0
    && [dict size [::vinst::remote_join]] == 0
}

file delete -force $_home
puts "jointest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
