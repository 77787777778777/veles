#!/usr/bin/env wish
# Session export must produce real files from the gateway's ONE renderer
# (GET /api/sessions/:id/export), written verbatim, new and owner-only,
# outside the media root. The old fixture fed flat {role content
# reasoning} dicts the real API never sends, so it passed while every
# real session (engine notes have no `content`) failed to export.
package require json
set dir [file dirname [file normalize [info script]]]
set tmproot [expr {[info exists ::env(TMPDIR)] && $::env(TMPDIR) ne "" ? $::env(TMPDIR) : "/tmp"}]
set ::env(VELES_HOME) [file join $tmproot veles-export-test-[pid]-[clock microseconds]]
file mkdir $::env(VELES_HOME)
set passed 0
set failed 0
proc ok {name condition} {
    if {[uplevel 1 [list expr $condition]]} { puts "  ok $name"; incr ::passed } else { puts "  FAIL $name"; incr ::failed }
}
proc slurp {path} {
    set fh [open $path r]
    fconfigure $fh -encoding utf-8
    set text [read $fh]
    close $fh
    return $text
}

# The endpoint's real response bodies, as the gateway serialises them
# (serde_json): the content is the shared exporter's output, so the JSON
# variant carries nested tool calls, nulls and notes INSIDE a string, and
# both carry characters Tcl and JSON quote differently.
set ::fixture(json) {{"session_id":"20261106_010203_abc123","format":"json","ext":"json","file_stem":"20261106_010203_abc123","message_count":3,"content":"{\n  \"session_id\": \"20261106_010203_abc123\",\n  \"title\": \"files [and] {braces}\",\n  \"message_count\": 3,\n  \"messages\": [\n    {\"role\": \"user\", \"content\": \"list \\\"the\\\" files\", \"reasoning\": null, \"msg_id\": 1},\n    {\"role\": \"assistant\", \"content\": \"\", \"reasoning\": null, \"display_reasoning\": \"why\", \"tool_calls\": [{\"id\": \"c1\", \"type\": \"function\", \"function\": {\"name\": \"terminal\", \"arguments\": \"{\\\"cmd\\\":\\\"ls\\\"}\"}}], \"msg_id\": 2},\n    {\"role\": \"tool\", \"content\": \"файл $x [y]\", \"reasoning\": null, \"tool_call_id\": \"c1\", \"tool_name\": \"terminal\", \"msg_id\": 3}\n  ],\n  \"notes\": [{\"after_msg_id\": 1, \"kind\": \"cron\", \"summary\": \"a cron job ran\", \"detail\": \"\", \"at\": \"\"}]\n}"}}
set ::fixture(markdown) {{"session_id":"20261106_010203_abc123","format":"markdown","ext":"md","file_stem":"20261106_010203_abc123","message_count":3,"content":"# files [and] {braces}\n\n## User\n\nlist \"the\" files\n\n> *Note (cron):* a cron job ran\n\n## Assistant\n\n<details><summary>Reasoning</summary>\n\nwhy\n\n</details>\n\n<details><summary>Tool call: terminal</summary>\n\n```json\n{\n  \"cmd\": \"ls\"\n}\n```\n\n</details>\n\n<details><summary>Tool result: terminal</summary>\n\n```\nфайл $x [y]\n```\n\n</details>\n\n"}}

try {
    source [file join $dir paths.tcl]
    # chat.tcl's many desktop dependencies are irrelevant to this proc.
    # Extract the production proc, then invoke it against the fixtures.
    set source [slurp [file join $dir chat.tcl]]
    set start [string first {proc ::vchat::export_session } $source]
    if {$start < 0} { error "export proc missing" }
    set end [string first "\n# /resume <id or prefix>" $source $start]
    if {$end < 0} { error "export proc end missing" }
    namespace eval ::vchat {}
    set production_proc [string range $source $start [expr {$end - 1}]]
    if {![info complete $production_proc]} { error "export proc extraction is incomplete" }
    eval $production_proc
    namespace eval ::vapi {
        variable asked {}
        proc get_json {path} {
            variable asked
            lappend asked $path
            if {![regexp {^/api/sessions/([^/?]+)/export\?format=(markdown|json)$} $path -> sid fmt]} {
                error "wrong export path: $path"
            }
            if {$sid eq "missing-one"} { error {HTTP 404: {"error":"no session missing-one on this node"}} }
            return [::json::json2dict $::fixture($fmt)]
        }
    }

    set json_path [::vchat::export_session 20261106_010203_abc123 json]
    set body [dict get [::json::json2dict $::fixture(json)] content]
    ok "the JSON file is the gateway's bytes, unchanged" {[slurp $json_path] eq $body}
    set parsed [::json::json2dict [slurp $json_path]]
    set call [lindex [dict get $parsed messages] 1]
    ok "nested tool calls stay JSON objects, not Tcl list strings" {
        [dict get [lindex [dict get $call tool_calls] 0] function name] eq "terminal"
    }
    ok "the JSON export asked the export endpoint, not /history" {
        [lindex $::vapi::asked end] eq "/api/sessions/20261106_010203_abc123/export?format=json"
    }
    ok "export is under <data>/exports, outside the media root" {
        [string first [file join [::vpaths::data_dir] exports] $json_path] == 0 &&
        [string first [file join [::vpaths::data_dir] media] $json_path] < 0
    }
    if {$::tcl_platform(platform) eq "unix"} {
        ok "export is owner-only from creation" {[file attributes $json_path -permissions] == 0600}
    }

    set md_path [::vchat::export_session 20261106_010203_abc123 markdown]
    set md [slurp $md_path]
    ok "Markdown export carries the note, the reasoning and the tool round" {
        [string match "*a cron job ran*" $md] &&
        [string match "*Reasoning*why*" $md] &&
        [string match "*Tool call: terminal*" $md] &&
        [string first {файл $x [y]} $md] >= 0
    }
    set again [::vchat::export_session 20261106_010203_abc123 markdown]
    ok "a second export is a new file, the first is untouched" {
        $again ne $md_path && [slurp $md_path] eq $md
    }
    set colon [::vchat::export_session irc:net:chan json]
    ok "a ':' in the id never reaches the file name (an NTFS stream)" {
        [string first : [file tail $colon]] < 0 && [string match "veles-irc_net_chan-*" [file tail $colon]]
    }
    ok "unsupported formats are refused" {[catch {::vchat::export_session session-1 xml} why] && [string match "*format*" $why]}
    ok "unsafe session ids cannot choose a path" {[catch {::vchat::export_session ../escape json} why] && [string match "*invalid session id*" $why]}
    set ::vapi::asked {}
    ok "a gateway error is reported, and no file is left behind" {
        [catch {::vchat::export_session missing-one json} why] &&
        [string match "*HTTP 404*" $why] &&
        [llength [glob -nocomplain -directory [file join [::vpaths::data_dir] exports] veles-missing-one-*]] == 0
    }
} on error {msg opts} {
    puts "  FAIL setup: $msg"
    incr failed
} finally {
    file delete -force $::env(VELES_HOME)
}
puts "exporttest: $passed passed, $failed failed"
exit [expr {$failed ? 1 : 0}]
