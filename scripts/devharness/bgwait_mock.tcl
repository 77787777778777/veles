#!/usr/bin/env tclsh
# bgwait_mock.tcl — scripted OpenAI-compatible provider for
# bgwait_e2e.sh: drives the background-waiting paths of a REAL gateway
# (2026-09-28 review) and logs what the model would have been told.
#
# Tcl, not python: python3 is not in this project's devshell, this repo
# is Tcl-first (USER policy: no new python tooling), and cache_mock.tcl
# right beside this is the same instrument in the same language.
#
# The script is keyed on the LAST message of each request:
#   user "START"                 -> terminal{background:true} (sleep 3)
#   tool result "started"        -> a plain reply (the turn ENDS)
#   user/injection "[BACKGROUND SHELL COMPLETE]" -> reply WAKE_OK
#   user "SLEEP"                 -> terminal{command:"sleep 60; echo x"}
#   user "WAIT"                  -> terminal{background:true} (sleep 2)
#   tool result "started" after WAIT -> shell{action:"wait", id}
#   user "LEFTOVER"              -> terminal{command:"sleep 40 & echo ..."}
#   any other tool result        -> a plain reply
# Every request's last message is appended to the log as one JSON line.
#
#   tclsh bgwait_mock.tcl PORT LOGFILE

package require json

set PORT [lindex $argv 0]
set LOGF [lindex $argv 1]

set LOG [open $LOGF a]
set MODE ""

proc log_line {role txt} {
    global LOG
    if {[string length $txt] > 4000} { set txt [string range $txt 0 3999] }
    # One JSON line per request, escaped exactly like json.dumps did in
    # the python original — bgwait_e2e.sh greps for the escaped shapes
    # (e.g. \"status\": \"done\") inside the text value.
    set m [list "\\" "\\\\" "\"" "\\\"" "\n" "\\n" "\r" "\\r" "\t" "\\t"]
    set role [string map $m $role]
    set txt  [string map $m $txt]
    puts $LOG "\{\"role\": \"$role\", \"text\": \"$txt\"\}"
    flush $LOG
}

# The python original's text_of: content is either a string or a list
# of {type:text,text:...} parts. json2dict turns a JSON array into a
# Tcl list and an object into a dict, so try reading parts and fall
# back to the raw string.
proc text_of {m} {
    if {![dict exists $m content]} { return "" }
    set c [dict get $m content]
    if {![catch {llength $c} n] && $n > 0} {
        set parts {}
        set isparts 1
        foreach p $c {
            if {[catch {dict exists $p text} ok] || !$ok} { set isparts 0; break }
            lappend parts [dict get $p text]
        }
        if {$isparts} { return [join $parts " "] }
    }
    return $c
}

proc sse {chan obj} {
    puts -nonewline $chan "data: $obj\n\n"
    flush $chan
}

# The three-chunk shapes the python original sent; the gateway's stream
# parser accepted them on the real e2e run. Fixed payloads are braced
# literals; the variable bits are spliced with string map so the JSON
# braces never meet Tcl's quoting rules.
proc say {chan words} {
    sse $chan {{"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}}
    set tmpl {{"choices":[{"index":0,"delta":{"content":"__W__"},"finish_reason":null}]}}
    sse $chan [string map [list __W__ $words] $tmpl]
    sse $chan {{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}}
}

proc call {chan name args_json} {
    sse $chan {{"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}}
    # arguments travel as a JSON string INSIDE the delta JSON: escape
    # the quotes of the inner object.
    set inner [string map {"\"" "\\\""} $args_json]
    set tmpl {{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"__N__","arguments":"__A__"}}]},"finish_reason":null}]}}
    sse $chan [string map [list __N__ $name __A__ $inner] $tmpl]
    sse $chan {{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}}
}

proc send_json {chan body} {
    puts -nonewline $chan "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: [string length $body]\r\nConnection: close\r\n\r\n$body"
    flush $chan
}

proc handle {chan addr port} {
    global MODE
    fconfigure $chan -translation binary -blocking 1
    if {[catch {gets $chan reqline} e] || $reqline eq ""} { close $chan; return }
    set method [lindex [split $reqline] 0]

    set clen 0
    while {[gets $chan line] > 0} {
        set line [string trim $line]
        if {$line eq ""} { break }
        if {[string match -nocase "content-length:*" $line]} {
            set clen [string trim [string range $line 15 end]]
        }
    }

    if {$method eq "GET"} {
        send_json $chan {{"data":[{"id":"mock-model"}]}}
        close $chan
        return
    }

    set body ""
    if {$clen > 0} { set body [read $chan $clen] }
    if {[catch {::json::json2dict $body} req]} { set req [dict create] }

    set role ""; set txt ""
    if {[dict exists $req messages]} {
        set msgs [dict get $req messages]
        if {[llength $msgs] > 0} {
            set last [lindex $msgs end]
            if {[dict exists $last role]} { set role [dict get $last role] }
            set txt [text_of $last]
        }
    }
    log_line $role $txt

    puts -nonewline $chan "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
    flush $chan

    set trimmed [string trim $txt]
    if {$role eq "user" && [string match "*BACKGROUND SHELL COMPLETE*" $txt]} {
        say $chan "WAKE_OK"
    } elseif {$role eq "user" && [string match "*START" $trimmed]} {
        set MODE "start"
        call $chan terminal {{"command":"sleep 3; echo BG_DONE_MARKER","background":true}}
    } elseif {$role eq "user" && [string match "*SLEEP" $trimmed]} {
        set MODE "sleep"
        call $chan terminal {{"command":"sleep 60; echo x"}}
    } elseif {$role eq "user" && [string match "*LEFTOVER" $trimmed]} {
        set MODE "leftover"
        call $chan terminal {{"command":"sleep 40 & echo LEFTOVER_MARKER","timeout":120}}
    } elseif {$role eq "user" && [string match "*WAIT" $trimmed]} {
        set MODE "wait"
        call $chan terminal {{"command":"sleep 2; echo WAITED_MARKER","background":true}}
    } elseif {$role eq "tool" && $MODE eq "wait" && [string match {*"started"*} $txt]} {
        set tid 0
        regexp {"task_id"\s*:\s*(\d+)} $txt -> tid
        set MODE "waiting"
        call $chan shell "\{\"action\":\"wait\",\"id\":$tid,\"timeout\":20\}"
    } else {
        say $chan "TURN_END"
    }
    puts -nonewline $chan "data: \[DONE\]\n\n"
    flush $chan
    close $chan
}

socket -server handle -myaddr 127.0.0.1 $PORT
vwait forever
