#!/usr/bin/env tclsh
# A stub Telegram Bot API for a GATEWAY's adapter (teloxide), for the
# three-node rehearsal (scripts/g-three-nodes.sh). The gateway is pointed
# at it with TELOXIDE_API_URL — stub the wire, not the function.
# tg_stub.tcl serves the headless signer, whose client is our own and
# reads a looser shape; teloxide deserializes the real Bot API types, so
# every answer here is a complete one.
#
# Usage: tclsh tg_bot_stub.tcl <port> <owner_id> <press-first|none> <log>
#
#   getMe                 → a bot user
#   getUpdates            → one queued update, or [] after ~1s
#   sendMessage …         → a full Message; with an inline keyboard, in
#                           press-first mode the OWNER presses its first
#                           button (a callback_query on the next poll)
#   anything else         → true
#   GET /__calls          → every method seen (lower case), one per line
#   GET /__sent           → every sendMessage text, one JSON string per line

package require json

set PORT  [lindex $argv 0]
set OWNER [lindex $argv 1]
set MODE  [lindex $argv 2]
set LOGF  [lindex $argv 3]
set LOG [open $LOGF a]
fconfigure $LOG -encoding utf-8
set CALLS {}
set SENT {}
set UPDATES {}
set NEXT_UPDATE 0
set NEXT_MSG 0

proc log {m} { puts $::LOG $m; flush $::LOG }

proc jstr {s} {
    return "\"[string map [list \\ \\\\ \" \\\" \n \\n \r \\r \t \\t] $s]\""
}

proc reply {chan status ctype body} {
    set body [encoding convertto utf-8 $body]
    puts -nonewline $chan "HTTP/1.1 $status\r\nContent-Type: $ctype\r\nContent-Length: [string length $body]\r\nConnection: close\r\n\r\n$body"
    flush $chan
    close $chan
}
proc ok {chan result} { reply $chan "200 OK" application/json "{\"ok\": true, \"result\": $result}" }

proc message {chat text} {
    incr ::NEXT_MSG
    return "{\"message_id\": $::NEXT_MSG, \"date\": [clock seconds], \"chat\": {\"id\": $chat, \"type\": \"private\", \"first_name\": \"owner\"}, \"text\": [jstr $text]}"
}

proc queue_press {data} {
    incr ::NEXT_UPDATE
    lappend ::UPDATES "{\"update_id\": $::NEXT_UPDATE, \"callback_query\": {\"id\": \"q$::NEXT_UPDATE\", \"from\": {\"id\": $::OWNER, \"is_bot\": false, \"first_name\": \"owner\"}, \"chat_instance\": \"ci$::NEXT_UPDATE\", \"data\": [jstr $data]}}"
}

proc handle {chan addr port} {
    fconfigure $chan -translation binary -blocking 1
    if {[catch {gets $chan reqline}] || $reqline eq ""} { close $chan; return }
    lassign [split $reqline] method path
    set clen 0
    set ctype ""
    while {[gets $chan line] > 0} {
        set line [string trim $line]
        if {$line eq ""} break
        if {[string match -nocase "content-length:*" $line]} {
            set clen [string trim [string range $line 15 end]]
        }
        if {[string match -nocase "content-type:*" $line]} {
            set ctype [string trim [string range $line 13 end]]
        }
    }
    set body ""
    if {$clen > 0} { set body [encoding convertfrom utf-8 [read $chan $clen]] }
    if {$method eq "GET" && [string match "*/__calls*" $path]} {
        reply $chan "200 OK" text/plain [join $::CALLS "\n"]
        return
    }
    if {$method eq "GET" && [string match "*/__sent*" $path]} {
        reply $chan "200 OK" text/plain [join $::SENT "\n"]
        return
    }
    # Method names are case-insensitive in the Bot API, and teloxide
    # sends them PascalCase (GetUpdates, SendMessage).
    set m [string tolower [lindex [split [lindex [split $path ?] 0] /] end]]
    if {$m eq "getupdates"} {
        after 0 [list poll $chan 0]
        return
    }
    lappend ::CALLS $m
    log "$m $body"
    set d {}
    if {[string match "*json*" $ctype]} { catch { set d [::json::json2dict $body] } }
    switch -- $m {
        getme {
            ok $chan "{\"id\": 4242, \"is_bot\": true, \"first_name\": \"veles\", \"username\": \"veles_stub_bot\", \"can_join_groups\": false, \"can_read_all_group_messages\": false, \"supports_inline_queries\": false, \"can_connect_to_business\": false, \"has_main_web_app\": false}"
        }
        sendmessage - editmessagetext - sendphoto - senddocument - sendaudio - sendvoice {
            set chat $::OWNER
            if {[dict exists $d chat_id]} { set chat [dict get $d chat_id] }
            set text ""
            if {[dict exists $d text]} { set text [dict get $d text] }
            if {$m eq "sendmessage"} {
                lappend ::SENT [jstr $text]
                if {$::MODE eq "press-first" && [dict exists $d reply_markup inline_keyboard]} {
                    set first [lindex [lindex [dict get $d reply_markup inline_keyboard] 0] 0]
                    if {[dict exists $first callback_data]} {
                        queue_press [dict get $first callback_data]
                    }
                }
            }
            ok $chan [message $chat $text]
        }
        default { ok $chan true }
    }
}

# A short long-poll: one queued update per poll.
proc poll {chan waited} {
    if {[llength $::UPDATES]} {
        set u [lindex $::UPDATES 0]
        set ::UPDATES [lrange $::UPDATES 1 end]
        ok $chan "\[$u\]"
        return
    }
    if {$waited >= 1000} {
        ok $chan "\[\]"
        return
    }
    after 50 [list poll $chan [expr {$waited + 50}]]
}

socket -server handle -myaddr 127.0.0.1 $PORT
log "tg_bot_stub on 127.0.0.1:$PORT owner=$OWNER mode=$MODE"
vwait forever
