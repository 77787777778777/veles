#!/usr/bin/env wish
# UI commands must return to Tk's event loop before their HTTP answers
# arrive. A nested vwait repaints, but it also admits unrelated commands
# into half-finished UI work; that is not asynchronous I/O.

set _dir [file dirname [file normalize [info script]]]
if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir i18n.tcl]
::vi18n::init
source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]

namespace eval ::vchat { variable session s-responsive }
namespace eval ::vstatus { variable route_prov "" }
namespace eval ::vtitle { proc attach {args} {} }
namespace eval ::vcoach {
    proc attach {args} {}
    proc attach_all {args} {}
}
namespace eval ::va11y { proc name {args} {} }
namespace eval ::vlang { proc refresh {args} {} }
source [file join $_dir manage.tcl]
source [file join $_dir statusbar.tcl]
source [file join $_dir overlays.tcl]

set passed 0
set failed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

# A loopback server that deliberately leaves each request unanswered for
# 200 ms. All reads and writes are event-driven; it never blocks the very
# event loop whose responsiveness is under test.
namespace eval ::wire {
    variable requests {}
    variable replies 0
    proc serve {ch addr port} {
        fconfigure $ch -blocking 0 -translation crlf
        fileevent $ch readable [list ::wire::request $ch ""]
    }
    proc request {ch first} {
        if {[gets $ch line] < 0} {
            if {[eof $ch]} { close $ch }
            return
        }
        if {$first eq ""} { set first $line }
        if {$line ne ""} {
            fileevent $ch readable [list ::wire::request $ch $first]
            return
        }
        fileevent $ch readable {}
        set path [lindex $first 1]
        lappend ::wire::requests $path
        after 200 [list ::wire::reply $ch $path]
    }
    proc reply {ch path} {
        if {[string match "/api/usage*" $path]} {
            set body {{"prompt_tokens":11,"completion_tokens":7,"calls":2,"by_plan":[],"session":null,"session_by_plan":[]}}
        } elseif {[string match "/api/quota*" $path]} {
            set body {{"enabled":true,"cold":false,"age_secs":1,"poll_secs":900,"active_plan":null,"plan_fractions":{},"team_error":null,"personal_error":null,"zai_error":null,"kimi_error":null}}
        } elseif {[string match "*/context" $path]} {
            set body {{"busy":false,"history_percent_of_budget":12.0,"context_window_tokens":200000,"cache_hit_rate_last_call":null}}
        } else {
            set body {{"uptime_s":42,"platforms":["desktop"],"plan_mode":false}}
        }
        puts $ch "HTTP/1.0 200 OK"
        puts $ch "Content-Type: application/json"
        puts $ch "Content-Length: [string length [encoding convertto utf-8 $body]]"
        puts $ch ""
        fconfigure $ch -translation binary
        puts -nonewline $ch $body
        close $ch
        incr ::wire::replies
    }
}
set ::wire::server [socket -server ::wire::serve -myaddr 127.0.0.1 0]
::vapi::set_base "http://127.0.0.1:[lindex [fconfigure $::wire::server -sockname] 2]"
::vapi::set_token ""
::vapi::bind_instance responsive [::vapi::base] ""
::vapi::use_instance responsive
set ::bgerrors {}
proc bgerror {err} {
    lappend ::bgerrors $err
    puts stderr "TEST BGERROR: $err\n$::errorInfo"
}

puts "responsivity"
puts "\nusage opening does not wait inside the click handler"
set ::usage_returned 0
set ::usage_probe {}
after 20 {
    lappend ::usage_probe [list $::usage_returned [winfo exists .ov_usage] $::wire::replies]
}
set ::usage_done 0
after 900 {set ::usage_done 1}
::voverlay::open usage
set ::usage_returned 1
vwait ::usage_done
ok "the click returns before either response arrives" \
    {[llength $::usage_probe] == 1 && [lindex $::usage_probe 0 0] == 1}
ok "the window exists immediately, before its data" \
    {[lindex $::usage_probe 0 1] == 1 && [lindex $::usage_probe 0 2] == 0}
ok "both responses still render after the click has returned" \
    {[winfo exists .ov_usage.body.h1] \
     && [.ov_usage.body.v1 cget -text] == 11 \
     && [.ov_usage.body.hq cget -text] ne ""}
set ::wire::requests {}
set ::wire::replies 0
::voverlay::_usage_fill .ov_usage 1
set ::usage_detach_done 0
after 20 { ::voverlay::detach_async }
after 450 { set ::usage_detach_done 1 }
vwait ::usage_detach_done
ok "attach teardown drops the usage window and its outstanding pair" \
    {![winfo exists .ov_usage] && !$::voverlay::usage_fill \
     && [array size ::voverlay::usage_pending] == 0}
ok "late usage replies after teardown are harmless" \
    {$::wire::replies == 2 && [llength $::bgerrors] == 0}

puts "\nturn-end status refresh is asynchronous too"
frame .statusbar
label .statusbar.right -text "unchanged"
pack .statusbar.right
set ::wire::requests {}
set ::wire::replies 0
set ::refresh_returned 0
set ::refresh_probe {}
after 320 {
    lappend ::refresh_probe [list $::refresh_returned $::wire::replies]
}
set ::refresh_done 0
after 1100 {set ::refresh_done 1}
::vstatus::refresh_context s-responsive
set ::refresh_returned 1
vwait ::refresh_done
ok "the scheduled turn-end refresh does not wait for its responses" \
    {[llength $::refresh_probe] == 1 && [lindex $::refresh_probe 0 0] == 1}
ok "the refresh paints after the event handler has returned" \
    {[string match "*ctx 12.0%*" [.statusbar.right cget -text]]}

puts "\nrepeated turn-end refreshes coalesce without nested HTTP waits"
set ::wire::requests {}
set ::wire::replies 0
::vstatus::_paint_right
::vstatus::_paint_right
::vstatus::_paint_right
# A reply admitted by Tk's event loop before the second click is stale;
# it must not swallow the one coalesced follow-up pair.
update
::vstatus::_paint_right
set ::coalesced_done 0
after 1100 {set ::coalesced_done 1}
vwait ::coalesced_done
set ::coalesced_requests {}
foreach path $::wire::requests {
    if {[string match "/api/usage*" $path] || [string match "*/context" $path]} {
        lappend ::coalesced_requests $path
    }
}
ok "two refreshes while one is out produce one follow-up pair" \
    {[llength $::coalesced_requests] == 4 && $::wire::replies == 4}
ok "the follow-up pair completes and leaves the painter idle" \
    {!$::vstatus::painting && [array size ::vstatus::paint_pending] == 0}

::vstatus::_paint_right
set ::status_painting_before_detach $::vstatus::painting
::vstatus::detach_async
ok "status teardown cancels outstanding painting and its timer" \
    {$::status_painting_before_detach && !$::vstatus::painting \
     && $::vstatus::paint_after eq "" && [array size ::vstatus::paint_pending] == 0}
after 300
update
ok "a discarded status reply cannot wedge or resurrect the painter" \
    {!$::vstatus::painting && $::vstatus::paint_after eq ""}

puts "\nthe context popup opens before its uncached response"
destroy .ov_usage
set ::vstatus::last_ctx {}
set ::vstatus::last_ctx_sid ""
source [file join $_dir vtext.tcl]
set ::context_returned 0
set ::context_reopened 0
set ::context_old_generation 0
set ::context_probe {}
after 20 {
    lappend ::context_probe [list $::context_returned [winfo exists .ov_context]]
    # Close while the old request is out, then reopen at the SAME Tk
    # path. Only the new generation may paint into the replacement.
    set ::context_old_generation $::vstatus::context_generation
    destroy .ov_context
    ::vstatus::show_context
    set ::context_reopened 1
}
set ::context_done 0
after 1350 {set ::context_done 1}
::vstatus::show_context
set ::context_returned 1
vwait ::context_done
set ::context_body [.ov_context.body get 1.0 end-1c]
ok "the context click returns before the delayed report" \
    {[llength $::context_probe] == 1 && [lindex $::context_probe 0 0] == 1}
ok "the context window exists before the report arrives" \
    {[lindex $::context_probe 0 1] == 1}
ok "closing a context popup invalidates its outstanding read" \
    {$::context_reopened && $::vstatus::context_generation > $::context_old_generation \
     && [winfo exists .ov_context]}
destroy .ov_context
ok "the delayed context report replaces the loading text" \
    {[string match "*history_percent_of_budget*" $::context_body]}
ok "all responsive paths complete without a background error" \
    {[llength $::bgerrors] == 0}

close $::wire::server
puts "\nresponsivitytest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
