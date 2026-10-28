#!/usr/bin/env tclsh
# A mock model for the challenge relay's end-to-end check
# (scripts/e-challenge.sh): round 1 opens <url> in the browser, round 2
# (once a tool result is in the conversation) answers. Every request body
# is appended to <log>, so the check can read what the model was handed
# as the page.
#
# Usage: tclsh challenge_mock.tcl <port> <log> <url>

package require json

set PORT [lindex $argv 0]
set LOGF [lindex $argv 1]
set URL  [lindex $argv 2]

proc sse {chan obj} { puts -nonewline $chan "data: $obj\n\n"; flush $chan }

proc handle {chan addr port} {
    fconfigure $chan -translation binary -blocking 1
    if {[catch {gets $chan reqline}] || $reqline eq ""} { close $chan; return }
    lassign [split $reqline] method path
    set clen 0
    while {[gets $chan line] > 0} {
        set line [string trim $line]
        if {$line eq ""} break
        if {[string match -nocase "content-length:*" $line]} {
            set clen [string trim [string range $line 15 end]]
        }
    }
    if {$method eq "GET"} {
        set b "{\"data\": \[{\"id\": \"mock-model\"}\]}"
        puts -nonewline $chan "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: [string length $b]\r\nConnection: close\r\n\r\n$b"
        flush $chan; close $chan; return
    }
    set body ""
    if {$clen > 0} { set body [encoding convertfrom utf-8 [read $chan $clen]] }
    set fh [open $::LOGF a]; puts $fh "=== REQUEST\n$body"; close $fh
    set tools 0; set results 0
    if {![catch {::json::json2dict $body} req]} {
        if {[dict exists $req tools]} { set tools [llength [dict get $req tools]] }
        if {[dict exists $req messages]} {
            foreach m [dict get $req messages] {
                if {[dict exists $m role] && [dict get $m role] eq "tool"} { incr results }
            }
        }
    }
    puts -nonewline $chan "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
    flush $chan
    if {$tools == 0} {
        # An aux call (the auto-titler): a title, nothing else.
        sse $chan "{\"choices\":\[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"a shop page\"},\"finish_reason\":null}\]}"
        sse $chan "{\"choices\":\[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}\]}"
    } elseif {$results == 0} {
        set args [string map {\" \\\"} "{\"url\": \"$::URL\"}"]
        sse $chan "{\"choices\":\[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}\]}"
        sse $chan "{\"choices\":\[{\"index\":0,\"delta\":{\"tool_calls\":\[{\"index\":0,\"id\":\"call_nav\",\"function\":{\"name\":\"browser_navigate\",\"arguments\":\"$args\"}}\]},\"finish_reason\":null}\]}"
        sse $chan "{\"choices\":\[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}\]}"
    } else {
        sse $chan "{\"choices\":\[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"I have the page.\"},\"finish_reason\":null}\]}"
        sse $chan "{\"choices\":\[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}\]}"
    }
    sse $chan "{\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":5}}"
    puts -nonewline $chan "data: \[DONE\]\n\n"
    flush $chan
    close $chan
}

socket -server handle -myaddr 127.0.0.1 $PORT
vwait forever
