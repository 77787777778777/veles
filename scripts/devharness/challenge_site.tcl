#!/usr/bin/env tclsh
# A site behind a fake "verify you are human" gate, for the challenge
# relay's end-to-end check (scripts/e-challenge.sh). The gate page is what
# a generic interstitial looks like to challenge::in_page — a gate title,
# next to no text — and it opens for a click on its one button, which sits
# at a fixed place so the check knows where a person would click: the
# button covers (100,100)–(300,180), so (200,140) is its middle.
#
# Usage: tclsh challenge_site.tcl <port>

set PORT [lindex $argv 0]

set GATE {<!doctype html><html><head><title>Just a moment...</title></head>
<body style="margin:0">
<p style="position:absolute;left:100px;top:40px;margin:0">Verify you are human by completing the action below.</p>
<button id="v" style="position:absolute;left:100px;top:100px;width:200px;height:80px"
 onclick="document.cookie='passed=1; path=/'; location.reload()">Verify</button>
</body></html>}

set SHOP {<!doctype html><html><head><title>The shop</title></head>
<body><h1>the secret content</h1><p>Past the gate, a page like any other.</p></body></html>}

proc handle {chan addr port} {
    fconfigure $chan -translation binary -blocking 1
    if {[catch {gets $chan reqline}] || $reqline eq ""} { close $chan; return }
    set cookie ""
    while {[gets $chan line] > 0} {
        set line [string trim $line]
        if {$line eq ""} break
        if {[string match -nocase "cookie:*" $line]} { set cookie [string range $line 7 end] }
    }
    set page [expr {[string match "*passed=1*" $cookie] ? $::SHOP : $::GATE}]
    set b [encoding convertto utf-8 $page]
    puts -nonewline $chan "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: [string length $b]\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n$b"
    flush $chan
    close $chan
}

socket -server handle -myaddr 127.0.0.1 $PORT
vwait forever
