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

# A late lsp-live marker from a reused session is not a fresh session.
# Sending initialize again makes tcl-lsp answer null and leaves the
# client believing it supports no capabilities.
set ::vlsp::ready 0
set ::vlsp::session_ready 1
set ::vlsp::pending [dict create 1 ::vlsp::_on_init]
set ::_drv::sent {}
::vlsp::_on_stream lsp-live {}
ok "a late reused-session marker cannot initialize twice" \
    {[llength $::_drv::sent] == 0 && !$::vlsp::ready}

# ── reused-process initialization replay ──────────────────────────────
# These namespace values must survive ::vlsp::start returning. A proc-local
# shadow looks correct to a caller that manually seeds state, but loses the
# replay reservation and sends a second initialize on lsp-live.
set ::vlsp::ch ""
set ::vlsp::root ""
set ::vlsp::session_ready 0
set ::vlsp::pending [dict create 9 ignored-callback]
set ::vlsp::next_id 77
set ::_drv::next_root "/projC"
set ::_drv::sent {}
proc ::vapi::post {path body} {
    if {[string match *lsp/start* $path]} {
        return {{"ok":true,"reused":true,"root":"/projC","path":"/projC/reused.tcl"}}
    }
    return {{}}
}
::vlsp::start /projC/reused.tcl "proc reused {} {}" ""
ok "a reused server reserves its replayed initialize response" {
    $::vlsp::session_ready == 1 && [dict get $::vlsp::pending 1] eq "::vlsp::_on_init"
}
ok "the replay reservation replaces the stale pending id" {
    [dict size $::vlsp::pending] == 1
}
ok "the next request id is reset for the new client handshake" {
    $::vlsp::next_id == 2
}
::vlsp::_on_stream lsp-live {}
ok "the reused handshake marker does not send a duplicate initialize" {
    [llength $::_drv::sent] == 0
}

# ── re-root against a gateway-faithful mock (dev-ext-6) ───────────────
# The real lsp_proxy::start spawns a fresh server on the re-root PROBE,
# and the fresh-start POST that follows then finds that same live
# process and answers reused=true. A process nobody has initialized must
# still get this client's initialize, or the client waits on id 1 forever
# (no didOpen, no hover, no tokens).
namespace eval ::_gw {
    variable live_root ""
    variable initialized 0
}
proc ::vapi::post {path body} {
    if {[string match *lsp/start* $path]} {
        set p [dict get [::json::json2dict $body] path]
        set r [file dirname $p]
        if {$::_gw::live_root eq $r} {
            set reused true
        } else {
            set ::_gw::live_root $r
            set ::_gw::initialized 0
            set reused false
        }
        return "{\"ok\":true,\"reused\":$reused,\"root\":\"$r\",\"path\":\"$p\"}"
    }
    return {{}}
}
set ::_gw::live_root "/projD"
set ::_gw::initialized 1
set ::vlsp::ch "tok-D"; set ::vlsp::root "/projD"; set ::vlsp::uri "file:///projD/a.tcl"
set ::vlsp::ready 1; set ::vlsp::session_ready 1; set ::vlsp::doc_path "/projD/a.tcl"
set ::vlsp::pending {}
set ::_drv::sent {}
::vlsp::start /projE/plugin.tcl "proc e {} {}" ""
::vlsp::_on_stream lsp-live {}
set ::init_frames 0
foreach f $::_drv::sent {
    if {[string match {*"method":"initialize"*} $f]} { incr ::init_frames }
}
ok "a re-root whose probe spawned the server initializes it (reused-after-probe is fresh)" {
    $::vlsp::root eq "/projE" && $::init_frames == 1
}

# ── one gateway session per language family (0.2.0) ──────────────────
# The start reply names the session, the language and the URIs; the
# client keeps them, attaches its event stream to THAT session, and a
# switch to another language family at the same root is a new session,
# not a reuse of the Tcl one.
namespace eval ::_lang {
    variable opened {}
}
proc ::vapi::sse_open {path handler} {
    lappend ::_lang::opened $path
    return "tok-[incr ::_drv::n]"
}
proc ::vapi::post {path body} {
    set p [dict get [::json::json2dict $body] path]
    switch -- [file extension $p] {
        .ts {
            return "{\"ok\":true,\"reused\":false,\"root\":\"/ext\",\"path\":\"$p\",\"session\":\"typescript\",\"language\":\"typescript\",\"cmd\":\"typescript-language-server\",\"uri\":\"file:///ext/plugin.ts\",\"root_uri\":\"file:///ext\",\"platform\":\"linux\",\"note\":\"no tsconfig.json\"}"
        }
        .rkt {
            return "{\"ok\":false,\"error\":\"no Racket language server is installed on the gateway (tried racket); install Racket and racket-langserver\",\"language\":\"racket\"}"
        }
    }
    return "{\"ok\":true,\"reused\":false,\"root\":\"/ext\",\"path\":\"$p\",\"session\":\"tcl\",\"language\":\"tcl\",\"uri\":\"file://$p\"}"
}
set ::vlsp::ch ""
set ::vlsp::session ""
set ::vlsp::root ""
::vlsp::start /ext/plugin.tcl "proc a {} {}" ""
ok "the Tcl session's stream is that session's" \
    {[lindex $::_lang::opened end] eq "/api/lsp/events?session=tcl" && $::vlsp::session eq "tcl"}
set ::_drv::sent {}
set ::before [llength $::_lang::opened]
::vlsp::start /ext/plugin.ts "await veles.hook('x', () => undefined);" ""
ok "a TypeScript file at the same root is another session, not a reuse" \
    {[llength $::_lang::opened] == $::before + 1 && [lindex $::_lang::opened end] eq "/api/lsp/events?session=typescript"}
ok "…keeping the gateway's URI, language, server and note" {
    $::vlsp::uri eq "file:///ext/plugin.ts" && [::vlsp::language] eq "typescript" &&
    [::vlsp::server_label] eq "typescript-language-server" && [::vlsp::note] eq "no tsconfig.json"
}
ok "…and the old session is left to the gateway, not stopped" {$::_drv::stops == $::stops_before}
# The gateway reports the program it FOUND (a full path since 0.2.2). The
# status line shows its name, the same for every gateway: a Windows
# gateway's path has backslashes, which `file tail` on a Unix desktop
# does not split on, and an `.exe` the user never typed.
set ::_saved_server $::vlsp::server
set ::_labels {}
foreach p {{C:\Program Files\LLVM\bin\clangd.exe} /nix/store/abc-clang/bin/clangd {C:\Users\u\AppData\Roaming\npm\typescript-language-server.CMD} tcl-lsp} {
    set ::vlsp::server $p
    lappend ::_labels [::vlsp::server_label]
}
set ::vlsp::server $::_saved_server
ok "the server's name is its program name on a Windows gateway as on a Unix one ($::_labels)" {
    $::_labels eq {clangd clangd typescript-language-server tcl-lsp}
}
::vlsp::_on_stream lsp-live {}
set ::init ""
foreach f $::_drv::sent { if {[string match {*"method":"initialize"*} $f]} { set ::init $f } }
ok "the new session is initialized with its own capabilities" \
    {[string match {*"utf-32","utf-16"*} $::init] && [string match {*"rootUri":"file:///ext"*} $::init]}
set ::vlsp::ch ""
::vlsp::start /ext/plugin.rkt "#lang racket/base" ""
ok "a language the gateway has no server for is not running" {![::vlsp::running]}
ok "…and the gateway's reason is kept for the editor" \
    {[string match "*racket-langserver*" [::vlsp::last_error]] && [string match "*on the gateway*" $::vstatus::last]}

puts "\nlspstarttest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
