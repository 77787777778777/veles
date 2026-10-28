#!/usr/bin/env tclsh
# A local Telegram Bot API fixture for live-join-rehearsal.sh. It serves
# the real Telegram module, never Telegram's servers, and admits one
# synthetic participant on demand through POST /__message.
package require json

set PORT [lindex $argv 0]
set OWNER [lindex $argv 1]
set PARTICIPANT [lindex $argv 2]
set LOGF [lindex $argv 3]
set LOG [open $LOGF a]
fconfigure $LOG -encoding utf-8
set UPDATES {}
set SENT {}
set NEXT_UPDATE 0
set NEXT_MESSAGE 0

proc log {m} { puts $::LOG $m; flush $::LOG }
proc jstr {s} {
    return "\"[string map [list \\ \\\\ \" \\\" \n \\n \r \\r \t \\t] $s]\""
}
proc reply {chan body} {
    set body [encoding convertto utf-8 $body]
    puts -nonewline $chan "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: [string length $body]\r\nConnection: close\r\n\r\n$body"
    flush $chan
    close $chan
}
proc ok {chan result} { reply $chan "{\"ok\":true,\"result\":$result}" }
proc text_reply {chan text} {
    set text [encoding convertto utf-8 $text]
    puts -nonewline $chan "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: [string length $text]\r\nConnection: close\r\n\r\n$text"
    flush $chan
    close $chan
}
proc message {chat text} {
    incr ::NEXT_MESSAGE
    return "{\"message_id\":$::NEXT_MESSAGE,\"date\":[clock seconds],\"chat\":{\"id\":$chat,\"type\":\"private\",\"first_name\":\"fixture\"},\"text\":[jstr $text]}"
}
proc queue_participant {} {
    incr ::NEXT_UPDATE
    lappend ::UPDATES "{\"update_id\":$::NEXT_UPDATE,\"message\":{\"message_id\":$::NEXT_UPDATE,\"date\":[clock seconds],\"from\":{\"id\":$::PARTICIPANT,\"is_bot\":false,\"first_name\":\"FixtureParticipant\"},\"chat\":{\"id\":$::PARTICIPANT,\"type\":\"private\",\"first_name\":\"FixtureParticipant\"},\"text\":\"hello from the synthetic participant\"}}"
}

proc handle {chan addr port} {
    fconfigure $chan -translation binary -blocking 1
    if {[catch {gets $chan req}] || $req eq ""} { close $chan; return }
    lassign [split $req] method path
    set length 0
    set ctype ""
    while {[gets $chan line] > 0} {
        set line [string trim $line]
        if {$line eq ""} break
        if {[string match -nocase content-length:* $line]} {
            set length [string trim [string range $line 15 end]]
        }
        if {[string match -nocase content-type:* $line]} {
            set ctype [string trim [string range $line 13 end]]
        }
    }
    set body ""
    if {$length > 0} { set body [encoding convertfrom utf-8 [read $chan $length]] }
    if {$method eq "GET" && $path eq "/__sent"} { text_reply $chan [join $::SENT "\n"]; return }
    if {$method eq "POST" && $path eq "/__message"} {
        queue_participant
        ok $chan true
        return
    }
    set m [string tolower [lindex [split [lindex [split $path ?] 0] /] end]]
    if {$m eq "getupdates"} { after 100 [list poll $chan]; return }
    log "$m $body"
    set d {}
    if {[string match *json* $ctype]} { catch {set d [::json::json2dict $body]} }
    switch -- $m {
        getme {
            ok $chan "{\"id\":4242,\"is_bot\":true,\"first_name\":\"fixture\",\"username\":\"veles_join_fixture\",\"can_join_groups\":false,\"can_read_all_group_messages\":false,\"supports_inline_queries\":false,\"can_connect_to_business\":false,\"has_main_web_app\":false}"
        }
        getwebhookinfo {
            ok $chan "{\"url\":\"\",\"has_custom_certificate\":false,\"pending_update_count\":[llength $::UPDATES],\"allowed_updates\":\[\]}"
        }
        sendmessage - editmessagetext - sendphoto - senddocument - sendaudio - sendvoice {
            set chat $::OWNER
            if {[dict exists $d chat_id]} { set chat [dict get $d chat_id] }
            set text ""
            if {[dict exists $d text]} { set text [dict get $d text] }
            if {$m eq "sendmessage"} { lappend ::SENT [jstr $text] }
            ok $chan [message $chat $text]
        }
        default { ok $chan true }
    }
}
proc poll {chan} {
    if {[llength $::UPDATES]} {
        set u [lindex $::UPDATES 0]
        set ::UPDATES [lrange $::UPDATES 1 end]
        ok $chan "\[$u\]"
    } else {
        ok $chan "\[\]"
    }
}
socket -server handle -myaddr 127.0.0.1 $PORT
log "telegram join fixture on 127.0.0.1:$PORT"
vwait forever
