#!/usr/bin/env tclsh
# tcl-lsp-check.tcl — one-shot LSP diagnostics driver for our own
# tcl-lsp (stdio JSON-RPC). Usage: tcl-lsp-check.tcl <file.tcl> [timeout-s]
#
# Protocol dance: initialize → initialized → textDocument/didOpen,
# then wait for textDocument/publishDiagnostics and print each
# diagnostic as `line:col severity message`. Exits 0 when clean,
# 1 when diagnostics exist, 2 on transport/timeout errors.
#
# Why a driver instead of a --check flag: tcl-lsp is a pure stdio LSP
# server (stdout carries JSON-RPC only — writing anything else to it
# is a compile error in its source). This script is the sanctioned
# batch surface.

if {[llength $argv] < 1} {
    puts stderr "usage: tcl-lsp-check.tcl <file.tcl> \[timeout-s\]"
    exit 2
}
set target [lindex $argv 0]
set timeout_s [expr {[llength $argv] > 1 ? [lindex $argv 1] : 15}]

if {![file exists $target]} {
    puts stderr "error: no such file: $target"
    exit 2
}
set target [file normalize $target]
set root [file dirname $target]
set text [read [open $target r]]

# ── spawn ────────────────────────────────────────────────────────────
set lsp [auto_execok tcl-lsp]
if {$lsp eq ""} {
    puts stderr "error: tcl-lsp not on PATH"
    exit 2
}
# 2>/dev/null is load-bearing: tcl-lsp logs to stderr, and with the
# default merged stream those log lines arrive on the read channel and
# break the Content-Length framing.
set pipe [open "|[list $lsp] 2>/dev/null" r+]
fconfigure $pipe -translation binary -buffering none

proc send_msg {pipe obj} {
    set body [encoding convertto utf-8 $obj]
    puts -nonewline $pipe "Content-Length: [string length $body]\r\n\r\n$body"
    flush $pipe
}

# Read one LSP message (headers + body). Returns "" on EOF.
proc read_msg {pipe} {
    set headers ""
    while {1} {
        set line [gets $pipe]
        if {[eof $pipe]} { return "" }
        if {$line eq "" || $line eq "\r"} { break }
        append headers "$line\n"
    }
    set len 0
    foreach h [split $headers "\n"] {
        if {[regexp -nocase {^content-length:\s*(\d+)} $h _ n]} { set len $n }
    }
    if {$len == 0} { return "" }
    set body [read $pipe $len]
    return [encoding convertfrom utf-8 $body]
}

# Minimal JSON string escape (paths/text we control).
proc jesc {s} {
    set s [string map {"\\" "\\\\" "\"" "\\\"" "\n" "\\n" "\r" "\\r" "\t" "\\t"} $s]
    return $s
}

# Given a single diagnostic JSON object, return "line character" for its
# range.start — order-independent (the server serializes keys
# alphabetically, so "end" precedes "start" and "character" precedes
# "line"). Brace-matches the start object, then greps each number.
proc _diag_start {obj} {
    set idx [string first "\"start\"" $obj]
    if {$idx < 0} { return "0 0" }
    set brace [string first "\{" $obj $idx]
    if {$brace < 0} { return "0 0" }
    # Brace-match the start object.
    set depth 1; set j [expr {$brace + 1}]; set len [string length $obj]
    while {$j < $len && $depth > 0} {
        set c [string index $obj $j]
        if {$c eq "\{"} { incr depth }
        if {$c eq "\}"} { incr depth -1 }
        incr j
    }
    set startobj [string range $obj $brace [expr {$j - 1}]]
    set line 0; set col 0
    if {[regexp {"line":\s*(\d+)} $startobj _ l]} { set line $l }
    if {[regexp {"character":\s*(\d+)} $startobj _ c]} { set col $c }
    return "$line $col"
}

# Given a diagnostic object, return the unescaped "message" string.
proc _diag_message {obj} {
    if {[regexp {"message":\s*"((?:[^"\\]|\\.)*)"} $obj _ m]} {
        return [string map {"\\n" "\n" "\\t" "\t" "\\\"" "\"" "\\\\" "\\"} $m]
    }
    return "?"
}

# ── initialize ───────────────────────────────────────────────────────
set init "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{ \"processId\": null, \"rootUri\": \"file://[jesc $root]\", \"capabilities\": {} }}"
send_msg $pipe $init


set deadline [expr {[clock seconds] + $timeout_s}]
set initialized 0
set opened 0
set done 0
set ndiag 0

while {[clock seconds] < $deadline && !$done} {
    # Tcl channels on pipes block on gets; the deadline bounds the total
    # wait. Everything below is STRING matching on the raw JSON —
    # json::json2dict breaks on diagnostic message text containing
    # braces/spaces, so we never parse the whole message as a dict.
    set msg [read_msg $pipe]
    if {$msg eq ""} { break }

    # 1) initialize response: {"jsonrpc":"2.0","id":1,...}
    # (\} is escaped so Tcl's brace matcher ignores it inside the word)
    if {!$initialized && [regexp {"id":\s*1[,\}]} $msg]} {
        send_msg $pipe "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{}}"
        set initialized 1
        set uri "file://[jesc $target]"
        set didopen "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didOpen\",\"params\":{\"textDocument\":{\"uri\":\"[jesc $uri]\",\"languageId\":\"tcl\",\"version\":1,\"text\":\"[jesc $text]\"}}}"
        send_msg $pipe $didopen
        set opened 1
        continue
    }

    # 2) publishDiagnostics notification — extract each diagnostic
    #    object from the "diagnostics":[...] array with a brace matcher.
    if {$opened && [string match {*"method":"textDocument/publishDiagnostics"*} $msg]} {
        set arr_start_idx [string first "\"diagnostics\"" $msg]
        if {$arr_start_idx >= 0} {
            set bracket [string first "\[" $msg $arr_start_idx]
            set arr_end [string last "\]" $msg]
            if {$bracket >= 0 && $arr_end > $bracket} {
                set arr [string range $msg [expr {$bracket + 1}] [expr {$arr_end - 1}]]
                # Brace-matched top-level objects.
                set objs {}; set i 0; set len [string length $arr]
                while {$i < $len} {
                    set c [string index $arr $i]
                    if {$c eq "\{"} {
                        set depth 1; set j [expr {$i + 1}]
                        while {$j < $len && $depth > 0} {
                            set cc [string index $arr $j]
                            if {$cc eq "\{"} { incr depth }
                            if {$cc eq "\}"} { incr depth -1 }
                            incr j
                        }
                        lappend objs [string range $arr $i [expr {$j - 1}]]
                        set i $j
                    } else {
                        incr i
                    }
                }
                foreach obj $objs {
                    lassign [_diag_start $obj] line0 col
                    set line [expr {$line0 + 1}]
                    set sev ""
                    if {[regexp {"severity":\s*(\d+)} $obj _ s]} {
                        set sev [lindex {"" ERROR WARNING INFO HINT} $s]
                    }
                    set msgtxt [_diag_message $obj]
                    puts "$line:$col $sev $msgtxt"
                    incr ndiag
                }
            }
        }
        set done 1
    }
}

# The server exits when we hang up; ignore the resulting close error.
catch { close $pipe }
if {!$done} {
    puts stderr "error: timed out waiting for diagnostics (server silent ${timeout_s}s)"
    exit 2
}
if {$ndiag == 0} {
    puts "clean — no diagnostics"
    exit 0
}
exit 1
