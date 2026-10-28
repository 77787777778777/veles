#!/usr/bin/env tclsh
# veles-desktop/lib/lsptest additions: the root-switch rule in ::vlsp::start.
# Runs standalone like the rest of the suites; follows lsptest.tcl's stubs.
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

namespace eval ::vstatus { variable last ""; proc set_msg {m} { set ::vstatus::last $m } }
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }
namespace eval ::vsyn {}
proc ::vsyn::highlight {w} {}
proc ::vsyn::diagnostics {w r} {}
source [file join $_dir api.tcl]
source [file join $_dir lspclient.tcl]

namespace eval ::_drv {
    variable next_root "/projA"; variable n 0
    variable restarts 0
    variable bodies {}
    variable sent {}        ;# every /api/lsp/send frame
    variable stops 0        ;# POST /api/lsp/stop count
}
proc ::vapi::post {path body} {
    if {[string match *lsp/start* $path]} {
        incr ::_drv::restarts
        lappend ::_drv::bodies $body
    }
    return "{\"ok\":true,\"root\":\"$::_drv::next_root\",\"path\":\"$::_drv::next_root/x.tcl\"}"
}
proc ::vapi::post_async {path body {on_err ""} {on_ok ""}} {
    if {[string match *lsp/send* $path]} { lappend ::_drv::sent $body }
    if {[string match *lsp/stop* $path]} { incr ::_drv::stops }
}
proc ::vapi::sse_open {path handler} { return "tok-[incr ::_drv::n]" }
proc ::vapi::sse_close {tok} {}

# Session already up, rooted at /projA, document a.tcl open.
set ::vlsp::ch "tok1"; set ::vlsp::root "/projA"; set ::vlsp::uri "file:///projA/a.tcl"
set ::vlsp::ready 1; set ::vlsp::doc_path "/projA/a.tcl"

set ::before $::_drv::restarts
::vlsp::start /projA/b.tcl "text" ""
ok "same-root open reuses the session (stream kept)" {$::vlsp::ch eq "tok1"}
ok "same-root open didOpen the new doc" {$::vlsp::uri eq "file:///projA/b.tcl"}
ok "same-root open posts /lsp/start exactly once (the root probe)" \
    {[expr {$::_drv::restarts - $::before} == 1]}
ok "the probe body is clean JSON the gateway can parse" \
    {![catch {dict get [::json::json2dict [lindex $::_drv::bodies end]] path} v] && $v eq "/projA/b.tcl"}

# The didClose for the file we left must be a real JSON-RPC frame. An
# unevaluated [_jstr ...] sitting where the uri string belongs (the
# 2026-08-31 regression) made the gateway drop the frame, and the old
# tcl-lsp transport died on it — so every buffer switch killed the
# server and hovers went silent.
set didclose ""
foreach f $::_drv::sent {
    if {[string match {*textDocument/didClose*} $f]} { set didclose $f }
}
ok "same-root open sent a didClose for the previous document" {$didclose ne ""}
ok "the didClose frame carries no unevaluated \[_jstr ...\]" \
    {![string match {*\[_jstr*} $didclose]}
ok "the didClose frame is valid JSON-RPC naming the old uri" \
    {![catch {::json::json2dict $didclose} d] \
     && [dict get $d params textDocument uri] eq "file:///projA/a.tcl"}

# Now a file whose derived root differs: the session must be torn down
# and re-established against /projB — never answered from /projA's index.
set ::_drv::next_root "/projB"
set ::before $::_drv::restarts
set ::stops_before $::_drv::stops
::vlsp::start /projB/c.tcl "text" ""
ok "different-root open re-roots the session" {$::vlsp::root eq "/projB"}
ok "different-root open restarts (probe + fresh start = 2 posts)" \
    {[expr {$::_drv::restarts - $::before} == 2]}
ok "the new document is the open one" {$::vlsp::uri eq "file:///projB/x.tcl"}
ok "the re-root path does NOT POST /api/lsp/stop (it would race the fresh start)" \
    {[expr {$::_drv::stops - $::stops_before} == 0]}

puts "\nlspstarttest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
