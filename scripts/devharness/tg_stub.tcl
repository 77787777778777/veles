#!/usr/bin/env tclsh
# A stub Telegram Bot API for the headless signer's end-to-end check
# (scripts/s-signer.sh). It stands where api.telegram.org does and the
# real signer talks to it over HTTP — stub the wire, not the function.
#
# Usage: tclsh tg_stub.tcl <port> <owner_id> <approve|decline|none> <log>
#
#   sendMessage with a Sign button   → recorded; in approve/decline mode
#                                      the OWNER presses Sign/Decline
#                                      (a callback_query on the next poll)
#   getUpdates                       → what is queued, or [] after ~1s
#   answerCallbackQuery, editMessageText → recorded
#   GET /__calls                     → every method seen, one per line
#
# In approve mode a STRANGER presses Sign first, so the check can assert
# that a press from anyone but the owner signs nothing.

package require json

set PORT  [lindex $argv 0]
set OWNER [lindex $argv 1]
set MODE  [lindex $argv 2]
set LOGF  [lindex $argv 3]
set LOG [open $LOGF a]
set CALLS {}
set UPDATES {}
set NEXT_UPDATE 0
set NEXT_MSG 0

proc log {m} { puts $::LOG $m; flush $::LOG }

proc send {chan status body} {
    puts -nonewline $chan "HTTP/1.1 $status\r\nContent-Type: application/json\r\nContent-Length: [string length $body]\r\nConnection: close\r\n\r\n$body"
    flush $chan
    close $chan
}

proc queue_press {from code verb} {
    incr ::NEXT_UPDATE
    lappend ::UPDATES "{\"update_id\": $::NEXT_UPDATE, \"callback_query\": {\"id\": \"q$::NEXT_UPDATE\", \"from\": {\"id\": $from}, \"data\": \"$verb:$code\"}}"
}

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
    set body ""
    if {$clen > 0} { set body [encoding convertfrom utf-8 [read $chan $clen]] }
    if {$method eq "GET" && [string match "*/__calls*" $path]} {
        set b [join $::CALLS "\n"]
        puts -nonewline $chan "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: [string length $b]\r\nConnection: close\r\n\r\n$b"
        flush $chan; close $chan; return
    }
    set m [lindex [split $path /] end]
    if {$m eq "getUpdates"} {
        after 0 [list poll $chan 0]
        return
    }
    lappend ::CALLS $m
    log "$m $body"
    if {$m eq "sendMessage"} {
        if {![catch { ::json::json2dict $body } d] && [dict exists $d reply_markup]} {
            set cb [dict get [lindex [lindex [dict get $d reply_markup inline_keyboard] 0] 0] callback_data]
            set code [lindex [split $cb :] 1]
            switch -- $::MODE {
                approve { queue_press 7 $code sign ; queue_press $::OWNER $code sign }
                decline { queue_press $::OWNER $code decline }
            }
        }
    }
    incr ::NEXT_MSG
    send $chan "200 OK" "{\"ok\": true, \"result\": {\"message_id\": $::NEXT_MSG}}"
}

# A short long-poll: hand out ONE queued update per poll, so the stranger's
# press is processed before the owner's.
proc poll {chan waited} {
    if {[llength $::UPDATES]} {
        set u [lindex $::UPDATES 0]
        set ::UPDATES [lrange $::UPDATES 1 end]
        send $chan "200 OK" "{\"ok\": true, \"result\": \[$u\]}"
        return
    }
    if {$waited >= 1000} {
        send $chan "200 OK" "{\"ok\": true, \"result\": \[\]}"
        return
    }
    after 50 [list poll $chan [expr {$waited + 50}]]
}

socket -server handle -myaddr 127.0.0.1 $PORT
log "tg_stub on 127.0.0.1:$PORT owner=$OWNER mode=$MODE"
vwait forever
