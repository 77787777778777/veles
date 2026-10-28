# veles-desktop/lib/cmdstest.tcl — the desktop reads the ENGINE's
# command table instead of keeping copies of it.
#
# WHY THIS SUITE EXISTS
#
# There were three hand-maintained copies of the slash-command list
# (`chat.tcl`'s `/help` literal, `complete.tcl`'s "mirror of
# chat.tcl", and the dispatch switch) of a list that grows in a fourth
# place, `frontends/commands.rs`. They had drifted: they advertised
# `/config` and `/verbose` while omitting `/queue`, `/context`,
# `/secret`, `/guards`, `/sessions` and `/cron`, all of which work.
#
# The engine serves the table at `/api/commands/builtin` (pinned in
# Rust by `the_command_table_is_served_not_copied`); this pins that the
# desktop RENDERS that rather than a copy.
#
# Run: scripts/tcl_tests.sh

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

source [file join $_dir paths.tcl]
source [file join $_dir i18n.tcl]

# ── stubs ────────────────────────────────────────────────────────────
namespace eval ::vapi {
    variable gets 0
    variable fail 0
    variable payload [dict create commands [list \
        [dict create usage {/model [name]} name /model desc "show or switch model" cap cmd-session] \
        [dict create usage /status name /status desc "session, model, lifecycle" cap cmd-status] \
        [dict create usage {/secret …} name /secret desc "secrets, redacted from history (admin)" cap cmd-admin] \
        [dict create usage /queue name /queue desc "messages parked mid-turn" cap cmd-session] \
        [dict create usage /context name /context desc "prompt breakdown" cap cmd-status]]]
    proc get_json {path args} {
        variable gets
        variable fail
        variable payload
        incr gets
        if {$fail} { error "connection refused" }
        if {[string match */api/commands/builtin $path]} { return $payload }
        error "unexpected GET $path"
    }
}

source [file join $_dir cmds.tcl]

# ── the engine's rows arrive ─────────────────────────────────────────
set rows [::vcmds::all]
proc names {rows} {
    set out {}
    foreach r $rows { lappend out [lindex $r 0] }
    return $out
}
set n [names $rows]
ok "the engine's commands are listed" {
    "/model" in $n && "/status" in $n && "/secret" in $n
}
# The six the hand-copies dropped. `/queue` and `/context` stand in
# here — the Rust side asserts all six against the real table.
ok "…including the ones the hand-copies omitted" {"/queue" in $n && "/context" in $n}
ok "the desktop-only commands are still there" {
    "/keymap" in $n && "/theme" in $n && "/quit" in $n
}
ok "no command is listed twice" {[llength $n] == [llength [lsort -unique $n]]}

# The usage args are split back off the served usage string, because
# the completion popup renders name and args in separate columns.
set model ""
foreach r $rows { if {[lindex $r 0] eq "/model"} { set model $r } }
ok "the args are split off the usage string" {[lindex $model 1] eq {[name]}}
ok "…and the description survives" {[lindex $model 2] eq "show or switch model"}
ok "…and the capability rides along" {[lindex $model 3] eq "cmd-session"}

# ── fetched ONCE ─────────────────────────────────────────────────────
# The completion popup calls this per keystroke. A GET per keystroke
# would make typing as slow as the gateway.
set before $::vapi::gets
::vcmds::all
::vcmds::all
ok "the table is fetched once, not per keystroke" {$::vapi::gets == $before}

# ── /help renders it ─────────────────────────────────────────────────
set h [::vcmds::help_text]
ok "/help renders the served table" {
    [string first "/model \[name\]" $h] >= 0
    && [string first "session, model, lifecycle" $h] >= 0
}
# Discovery used to show every row to everyone, which is a lie the
# moment the gateway refuses one.
ok "…marking the admin rows" {[regexp -line {/secret.*\(admin\)} $h]}
# `-line`, because Tcl.s `.` matches a newline: without it "/status"
# on one line and "(admin)" on the /secret line five rows down count
# as a match, and the test passes for the wrong reason.
ok "…and not marking the open ones" {![regexp -line {/status.*\(admin\)} $h]}
ok "the desktop-only rows render too" {[string first "/keymap" $h] >= 0}

# ── a dead gateway ───────────────────────────────────────────────────
# Not retried per keystroke, and it says so rather than showing an
# empty list that looks like "there are no commands".
namespace eval ::vcmds { variable fetched 0; variable engine {} }
set ::vapi::fail 1
set rows [::vcmds::all]
set n [names $rows]
ok "a dead gateway still lists the desktop's own commands" {"/quit" in $n}
ok "…and does not list the engine's" {"/model" ni $n}
set before $::vapi::gets
::vcmds::all
ok "…and is not retried per keystroke" {$::vapi::gets == $before}

# ── the completion source is the same list ───────────────────────────
namespace eval ::vcmds { variable fetched 0; variable engine {} }
set ::vapi::fail 0
set _c [open [file join $_dir complete.tcl] r]
set _csrc [read $_c]; close $_c
ok "complete.tcl reads the shared table" {
    [string first {::vcmds::all} $_csrc] >= 0
}
ok "…and keeps no copy of its own" {
    [string first {Mirror of chat.tcl} $_csrc] < 0
}
set _ch [open [file join $_dir chat.tcl] r]
set _chsrc [read $_ch]; close $_ch
ok "chat.tcl's /help reads the shared table" {
    [string first {::vcmds::help_text} $_chsrc] >= 0
}
ok "…and keeps no literal of its own" {
    [string first {"/help           show this help"} $_chsrc] < 0
}

puts "\ncmdstest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
