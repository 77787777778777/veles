#!/usr/bin/env wish
# Standing permission review: scope and exact rule identity survive the UI.
#
# The wire is stubbed, not the functions (the asktest/memfilestest
# rule): api.tcl is the real one, talking to a loopback socket that
# records each request, so the DELETE-with-JSON-body that revoke sends
# is what a gateway would actually receive (review finding approvals-4).
if {![info exists ::env(XVFB_RUNNING)] && ![info exists ::env(VELES_TK_TEST_FORCE)]} {
    puts "SKIP: requires Xvfb"
    exit 0
}
set dir [file dirname [file normalize [info script]]]
source [file join $dir i18n.tcl]
source [file join $dir theme.tcl]
::vt::palette_synthwave ::vt::T
namespace eval ::voverlay { proc _header {w title} {} }
namespace eval ::va11y { proc name {w title} {}; proc tab_order {widgets} {} }
namespace eval ::vstatus { proc set_msg {m} {} }
source [file join $dir jsonfield.tcl]
source [file join $dir api.tcl]

# ── the loopback gateway ────────────────────────────────────────────
# One thread serves and asks: a synchronous http::geturl waits in the
# event loop, where these handlers run.
set ::requests {}
set ::grants_json {{"global":[{"scope":"global","rule_key":"terminal:recursive-delete","note":null,"created_at":"now"}],
 "sessions":[{"scope":"session","session_id":"session-1","surface":"desktop","rule_key":"browser:navigate"},
             {"scope":"session","session_id":"tg-7","surface":"telegram","rule_key":"terminal:remote-code-execution"}],
 "config":[{"scope":"config","rule_key":"rm -rf ./build*","read_only":true}]}}
proc serve {ch addr port} {
    fconfigure $ch -blocking 0 -translation crlf
    set ::rq($ch) [dict create head {} body ""]
    fileevent $ch readable [list ::_head $ch]
}
proc ::_head {ch} {
    while {[gets $ch line] >= 0} {
        if {$line eq ""} {
            set len 0
            foreach h [dict get $::rq($ch) head] {
                regexp -nocase {^content-length:\s*(\d+)} $h -> len
            }
            dict set ::rq($ch) len $len
            fconfigure $ch -translation binary
            fileevent $ch readable [list ::_body $ch]
            ::_body $ch
            return
        }
        dict lappend ::rq($ch) head $line
    }
    if {[eof $ch]} { close $ch; unset ::rq($ch) }
}
proc ::_body {ch} {
    set have [dict get $::rq($ch) body]
    set need [expr {[dict get $::rq($ch) len] - [string length $have]}]
    if {$need > 0} {
        append have [read $ch $need]
        dict set ::rq($ch) body $have
    }
    if {[string length $have] < [dict get $::rq($ch) len]} {
        if {[eof $ch]} { close $ch; unset ::rq($ch) }
        return
    }
    fileevent $ch readable {}
    lassign [lindex [dict get $::rq($ch) head] 0] method path
    lappend ::requests [list $method $path [encoding convertfrom utf-8 $have]]
    if {$method eq "GET"} {
        set reply $::grants_json
    } else {
        set reply {{"revoked":true}}
    }
    set reply [encoding convertto utf-8 $reply]
    puts -nonewline $ch "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: [string length $reply]\r\n\r\n$reply"
    close $ch
    unset ::rq($ch)
}
set srv [socket -server serve -myaddr 127.0.0.1 0]
::vapi::set_base "http://127.0.0.1:[lindex [fconfigure $srv -sockname] 2]"
::vapi::set_token ""
proc ::_last_delete {} {
    foreach r [lreverse $::requests] {
        if {[lindex $r 0] eq "DELETE"} {
            return [list [lindex $r 1] [::json::json2dict [lindex $r 2]]]
        }
    }
    return {}
}
proc ::_deletes {} {
    set n 0
    foreach r $::requests { if {[lindex $r 0] eq "DELETE"} { incr n } }
    return $n
}

source [file join $dir grants.tcl]
set failed 0
set passed 0
proc ok {name condition} {
    # An error inside a check counts as red, never aborts the tally.
    if {![catch {uplevel 1 [list expr $condition]} r] && $r} {
        puts "  ok $name"; incr ::passed
    } else {
        puts "  FAIL $name"; incr ::failed
    }
}
::vgrants::build .grants
set ids [.grants.body.t children {}]
ok "global, desktop, chat-session and config grants are all listed" {[llength $ids] == 4}
proc ::_row_of {scope key} {
    foreach id [.grants.body.t children {}] {
        set v [.grants.body.t item $id -values]
        if {[lindex $v 0] eq $scope && [lindex $v 3] eq $key} { return $id }
    }
    return ""
}
set tg [::_row_of session terminal:remote-code-execution]
ok "a chat session's grant names its surface" {
    $tg ne "" && [lindex [.grants.body.t item $tg -values] 1] eq "telegram"
    && [lindex [.grants.body.t item $tg -values] 2] eq "tg-7"
}
ok "a missing note renders empty, not the word null" {
    [lindex [.grants.body.t item [::_row_of global terminal:recursive-delete] -values] 5] eq ""
}

.grants.body.t selection set [::_row_of session browser:navigate]
ok "session revoke sends DELETE with the exact key and owning session" {
    [::vgrants::revoke_selected .grants] &&
    [lindex [::_last_delete] 0] eq "/api/approvals/grants" &&
    [lsort -stride 2 [lindex [::_last_delete] 1]] eq
        [lsort -stride 2 [dict create scope session rule_key browser:navigate session_id session-1]]
}
.grants.body.t selection set [::_row_of session terminal:remote-code-execution]
::vgrants::revoke_selected .grants
ok "a chat session's grant revokes through the same route" {
    [dict get [lindex [::_last_delete] 1] session_id] eq "tg-7" &&
    [dict get [lindex [::_last_delete] 1] rule_key] eq "terminal:remote-code-execution"
}
.grants.body.t selection set [::_row_of global terminal:recursive-delete]
::vgrants::revoke_selected .grants
ok "global revoke does not invent a session scope" {
    [dict get [lindex [::_last_delete] 1] scope] eq "global" &&
    [dict get [lindex [::_last_delete] 1] rule_key] eq "terminal:recursive-delete" &&
    ![dict exists [lindex [::_last_delete] 1] session_id]
}
set before [::_deletes]
.grants.body.t selection set [::_row_of config {rm -rf ./build*}]
ok "a config allowlist entry is read-only: no request is sent" {
    ![::vgrants::revoke_selected .grants] && [::_deletes] == $before
    && [string match "*config.toml*" [.grants.bar.status cget -text]]
}
destroy .grants
close $srv
puts "granttest: $passed passed, $failed failed"
exit [expr {$failed ? 1 : 0}]
