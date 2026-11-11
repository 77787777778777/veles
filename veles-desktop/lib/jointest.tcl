# Joined desktop sessions: transport, target ownership and local return
# view. Real api.tcl/instances.tcl and the real _join_command; the HTTP
# boundary and window operations are small recording stubs.

package require json
set _dir [file dirname [file normalize [info script]]]
if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}
source [file join $_dir paths.tcl]
source [file join $_dir api.tcl]
source [file join $_dir instances.tcl]
source [file join $_dir attach.tcl]

set failed 0
set passed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name" ; incr ::passed
    } else {
        puts "  ✗ $name" ; incr ::failed
    }
}
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} {return [format $s {*}$args]} ; return $s} }
namespace eval ::vstatus { variable last "" ; proc set_msg {m} {set ::vstatus::last $m} }
namespace eval ::vstatus { proc set_attachment {spec args} {} }
namespace eval ::vtabs { proc select {name args} {} }
source [file join $_dir attach.tcl]
proc ::vapp::rebuild_regions {} {}
namespace eval ::vchat {
    variable notes {}
}
source [file join $_dir chat.tcl]
proc ::vchat::emit_message {kind text args} { lappend ::vchat::notes [list $kind $text] }
proc ::vchat::open_session {sid} { set ::vchat::session $sid }
set ::vchat::session mine

set _home [file join $::env(TMPDIR) veles-jointest-[pid]]
file delete -force $_home
file mkdir $_home
set ::env(XDG_DATA_HOME) $_home
unset -nocomplain ::env(VELES_HOME)
set ::env(VELES_PROFILE) jointest
::vapi::bind_instance laptop http://127.0.0.1:8642 laptop-token
::vapi::bind_instance vps http://127.0.0.1:18642 vps-token
::vapi::use_instance laptop

namespace eval ::_http {
    variable calls {}
    variable fail_save 0
    variable chat_answer ""
    variable get_answer ""
    variable join_answer ""
    variable candidates {{"sessions": []}}
    variable command_answer {{"handled": true, "text": "engine answered"}}
}
rename ::vinst::save ::vinst::save_fixture_real
proc ::vinst::save {} {
    if {$::_http::fail_save} { set ::vinst::error "disk full (fixture)" ; return 0 }
    return [::vinst::save_fixture_real]
}
# Stub the wire BELOW post_json: chat_send posts its body directly.
rename ::vapi::post ::vapi::post_fixture_real
proc ::vapi::post {path body args} {
    lappend ::_http::calls [list [::vapi::attached] $path $body [::vapi::_url $path]]
    if {$path eq "/api/join"} {
        if {$::_http::join_answer ne ""} { return [{*}$::_http::join_answer] }
        return {{"join": {"join_id": "j-e2e", "from_instance": "laptop", "from_session": "mine", "target_instance": "", "target_session": "alice"}}}
    }
    if {$path eq "/api/join/leave"} { return {{"ok": true}} }
    if {$path eq "/api/command"} { return $::_http::command_answer }
    if {$path eq "/api/commands/render"} { error {HTTP 404: {"error":"no such command"}} }
    if {$path eq "/api/chat"} {
        if {$::_http::chat_answer ne ""} { return $::_http::chat_answer }
        return {{"started": true, "join": true}}
    }
    error "unexpected POST $path"
}
# Real post_json above the recording raw-post stub: typed JSON values
# must be encoded exactly as they are by the shipping client.

# A cross-node join is made ON the target, not on the attached instance.
# The real restart helper validates the target before reopening the
# joined view. Stub GET below the production JSON/URL layers.
rename ::vapi::get ::vapi::get_fixture_real
proc ::vapi::get {path {ms 0}} {
    lappend ::_http::calls [list [::vapi::attached] $path {} [::vapi::_url $path]]
    if {$path eq "/api/join/candidates"} { return $::_http::candidates }
    if {[string match "/api/sessions/*/export?format=*" $path]} {
        return {{"content": "exported", "format": "x", "message_count": 0}}
    }
    if {[string match "/api/join/status?*" $path]} {
        if {$::_http::get_answer ne ""} { return $::_http::get_answer }
        return {{"join":{"join_id":"j-restart","target_session":"alice","from_instance":"","from_session":""}}}
    }
    error "unexpected GET $path"
}
proc ::_http::reset {} { variable calls {} }
::_http::reset
::vchat::_join_command mine /join {vps/alice}
set _calls $::_http::calls
ok "join is issued against the named target gateway" {
    [llength $_calls] == 1
    && [lindex $_calls 0 0] eq "vps"
    && [lindex $_calls 0 1] eq "/api/join"
    && [lindex $_calls 0 3] eq "http://127.0.0.1:18642/api/join"
}
set d [::json::json2dict [lindex $_calls 0 2]]
ok "the request carries only the target and local return-view labels" {
    [dict get $d session_id] eq "alice"
    && [dict get $d instance] eq "laptop"
    && [dict get $d owner_session] eq "mine"
}
ok "the desktop moved to the target and kept only its return view" {
    [::vapi::attached] eq "vps"
    && $::vchat::session eq "alice"
    && [dict get [::vinst::remote_join] join_id] eq "j-e2e"
    && [dict get [::vinst::remote_join] from_instance] eq "laptop"
    && [dict get [::vinst::remote_join] from_session] eq "mine"
}
::vchat::_join_command alice /join {}
ok "a second join is refused while the first is active" {
    [llength $::_http::calls] == 1
}

# chat_send consults the local return view but POSTs to its target, and
# carries the id and attachment text to the shared gateway join path.
::_http::reset
set raw [::vapi::chat_send alice {owner line} [list {/tmp/a file.txt}]]
set d [::json::json2dict [lindex $::_http::calls 0 2]]
ok "input goes to the target with the active join id" {
    [lindex $::_http::calls 0 0] eq "vps"
    && [dict get $d join_id] eq "j-e2e"
    && [dict get $d session_id] eq "alice"
    && [dict get $d text] eq "owner line"
    && [lindex [dict get $d attachments] 0] eq "/tmp/a file.txt"
}
::vapi::chat_send other {not the joined view}
ok "another view cannot borrow the active join id" {
    [llength $::_http::calls] == 2
    && [dict get [::json::json2dict [lindex $::_http::calls 1 2]] join_id] eq "null"
}

::_http::reset
::vchat::_join_command alice /leave {}
set d [::json::json2dict [lindex $::_http::calls 0 2]]
ok "leave stays on the target and identifies the exact joined session" {
    [lindex $::_http::calls 0 0] eq "vps"
    && [lindex $::_http::calls 0 3] eq "http://127.0.0.1:18642/api/join/leave"
    && [dict get $d join_id] eq "j-e2e"
    && [dict get $d session_id] eq "alice"
}
ok "leave restores the local view and clears the join sidecar" {
    [::vapi::attached] eq "laptop"
    && $::vchat::session eq "mine"
    && [dict size [::vinst::remote_join]] == 0
}

# A registry save failure after a successful join must undo the target
# binding, on the target, and restore the old view.
::vinst::set_remote_join {}
set ::_http::fail_save 1
::_http::reset
::vchat::_join_command mine /join {vps/alice}
set _paths [lmap c $::_http::calls {lindex $c 1}]
ok "a failed local save rolls back the target join" {
    $_paths eq [list /api/join /api/join/leave]
    && [lindex $::_http::calls 1 0] eq "vps"
    && [::vapi::attached] eq "laptop"
    && [dict size [::vinst::remote_join]] == 0
}

# The real registry persists the sidecar, but not as a fake instance.
set ::_http::fail_save 0
set spec [dict create join_id j-restart instance vps target_session alice from_instance laptop from_session mine]
::vinst::set_remote_join $spec
set path [::vinst::path]
set ::vinst::remote_join {}
::vinst::load
ok "return-view state survives a real registry reload" {
    [file exists $path]
    && [::vinst::remote_join] eq $spec
    && [lsearch -exact [::vinst::names] __remote_join__] < 0
}
# Restart boot uses the real boot_decide and the real attach path.
::vinst::set_last_attached laptop
::vapi::use_instance ""
unset -nocomplain ::env(VELES_INSTANCE)
set boot [::vapp::boot_decide]
ok "restart reattaches to the saved joined target, not the origin" {
    $boot eq "vps"
    && [::vapi::attached] eq "vps"
}
::vapi::use_instance ""
set ::env(VELES_INSTANCE) laptop
set explicit [::vapp::boot_decide]
unset ::env(VELES_INSTANCE)
ok "an explicit instance still wins over saved join restoration" {
    $explicit eq "laptop"
    && [::vapi::attached] eq "laptop"
}

# Restart restoration is more than sidecar parsing. The target must
# still hold this exact join and admit the participant's current role.
::_http::reset
::vapi::use_instance vps
set restored [::vinst::restore_remote_join]
ok "a valid saved join revalidates on its target before reopening" {
    $restored == 1
    && [llength $::_http::calls] == 1
    && [lindex $::_http::calls 0 0] eq "vps"
    && [string match "/api/join/status?*" [lindex $::_http::calls 0 1]]
    && [::vapi::attached] eq "vps"
    && $::vchat::session eq "alice"
}
set ::_http::get_answer {{"error":"join j-restart is no longer valid under the current ACL"}}
set ::vchat::notes {}
::_http::reset
set restored [::vinst::restore_remote_join]
ok "a revoked saved join returns to the origin and clears its sidecar" {
    $restored == -1
    && [::vapi::attached] eq "laptop"
    && $::vchat::session eq "mine"
    && [dict size [::vinst::remote_join]] == 0
    && [string match "*current ACL*" [lindex $::vchat::notes end 1]]
}

# Drive the real composer with real text widgets under Xvfb. Stubs only
# replace the transcript's rendering and the wire; send_current itself,
# its busy branch and its joined slash-command boundary are shipping code.
proc ::vchat::log {} { return .joinlog }
proc ::vchat::_mark_running {sid on args} { set ::vchat::busy $on }
proc ::vchat::_refresh_chips {} {}
proc ::vchat::turn_error {msg} {
    # The production turn_error ends by clearing the running flag. Keep
    # that observable contract while omitting its unrelated transcript
    # reconciliation in this small fixture.
    set ::vchat::turn_error $msg
    ::vchat::_mark_running $::vchat::session 0
}
proc ::vchat::_sync_buttons {} {}
namespace eval ::vfind { proc reset_marks {} {} }
namespace eval ::vsidebar { proc refresh {} {} ; proc refresh_soon {} {} }
namespace eval ::vedit { proc hist_push {text} {} }
text .joinlog
frame .mid
frame .mid.chatwrap
frame .mid.chatwrap.chat
text .mid.chatwrap.chat.input
set ::vchat::session alice
set ::vchat::busy 1
::vinst::set_remote_join [dict create join_id j-e2e instance vps target_session alice from_instance laptop from_session mine]
::vapi::use_instance vps
::_http::reset
.mid.chatwrap.chat.input insert 1.0 {owner while busy}
::vchat::send_current
ok "busy joined input uses /api/chat with join_id, never /steer" {
    [llength $::_http::calls] == 1
    && [lindex $::_http::calls 0 1] eq "/api/chat"
    && [dict get [::json::json2dict [lindex $::_http::calls 0 2]] join_id] eq "j-e2e"
    && [dict get [::json::json2dict [lindex $::_http::calls 0 2]] text] eq "owner while busy"
}
::_http::reset
set ::vchat::busy 0
.mid.chatwrap.chat.input insert 1.0 {/secret set KEY never-send-this}
::vchat::send_current
ok "a joined slash command cannot run the ordinary local command path" {
    [llength $::_http::calls] == 0
    && [string match "*commands stay on your own session*" [lindex $::vchat::notes end 1]]
    && [string match "*never-send-this*" [lindex $::vchat::notes end 1]] == 0
}
set ::_http::chat_answer {{"error":"commands stay on your own session; /leave returns from the joined session"}}
::_http::reset
.mid.chatwrap.chat.input insert 1.0 {/steer must not steer}
::vchat::send_current
ok "the target rejects joined /steer without an ordinary turn" {
    [llength $::_http::calls] == 0
    && [string match "*commands stay on your own session*" [lindex $::vchat::notes end 1]]
}

# A palette/keyboard action calls slash_command without send_current.
# It must meet the same joined-session boundary.
namespace eval ::voverlay { variable opened {} ; proc open {what args} { set ::voverlay::opened $what } }
set ::_http::calls {}
set ::voverlay::opened {}
::vchat::slash_command /model
ok "a direct palette command cannot bypass the joined-session boundary" {
    $::voverlay::opened eq {}
    && [llength $::_http::calls] == 0
    && [string match "*commands stay on your own session*" [lindex $::vchat::notes end 1]]
}

# A rejected joined POST must not leave an optimistic running turn.
set ::_http::chat_answer {{"error":"join j-e2e is not active on this gateway"}}
::_http::reset
.mid.chatwrap.chat.input insert 1.0 {this send is refused}
::vchat::send_current
ok "a rejected joined send does not remain a running turn" {
    [llength $::_http::calls] == 1
    && [lindex $::_http::calls 0 1] eq "/api/chat"
    && $::vchat::busy == 0
    && [string match "*not active*" [lindex $::vchat::notes end 1]]
}

# /leave remains available while the joined session is busy. The target
# returns a handled response, not an ordinary streaming turn.
set ::_http::chat_answer {{"handled":true,"join":true,"text":"left alice; what you write here is your own session's again"}}
::_http::reset
set ::vchat::busy 1
.mid.chatwrap.chat.input insert 1.0 {/leave}
::vchat::send_current
ok "busy joined /leave uses the target and restores the origin" {
    [llength $::_http::calls] == 1
    && [lindex $::_http::calls 0 1] eq "/api/chat"
    && [dict get [::json::json2dict [lindex $::_http::calls 0 2]] text] eq "/leave"
    && [::vapi::attached] eq "laptop"
    && $::vchat::session eq "mine"
    && $::vchat::busy == 0
    && [dict size [::vinst::remote_join]] == 0
}

# ── /shells and /monitors: bare opens the panel, arguments stay shared ─
# The desktop claims only the BARE command (it opens the task panel).
# `/shells kill 7`, `/shells output 7` and `/monitors cancel 2` are the
# engine's shared grammar and must reach POST /api/command with the full
# text. They silently did nothing once: the panel's switch arm ended
# without a fall-through to `default`, the only arm that posts (TSG-2).
# Here, back on our own session, through the real slash_command and the
# real post_json above the recording POST stub.
puts "\n/shells and /monitors keep their shared arguments"
namespace eval ::vsidebar { proc selected_sid {} { return $::vchat::session } }
foreach {line} {{/shells} {/monitors}} {
    ::_http::reset
    set ::voverlay::opened {}
    ::vchat::slash_command $line
    ok "bare $line opens the task panel and posts nothing" {
        $::voverlay::opened eq "tasks" && [llength $::_http::calls] == 0
    }
}
foreach {line} {{/shells kill 7} {/shells output 7} {/monitors cancel 2}} {
    ::_http::reset
    set ::voverlay::opened {}
    set ::vchat::notes {}
    ::vchat::slash_command $line
    set _posted [lsearch -inline -index 1 $::_http::calls /api/command]
    ok "$line reaches the engine's shared command with its full text" {
        [llength $_posted] > 0
        && [dict get [::json::json2dict [lindex $_posted 2]] command] eq $line
        && [dict get [::json::json2dict [lindex $_posted 2]] session_id] eq "mine"
        && $::voverlay::opened eq {}
    }
    ok "…and the engine's answer is shown" {
        [string match "*engine answered*" [lindex $::vchat::notes end 1]]
    }
}
# With no session open there is nothing to run them in: say so, never
# nothing at all (the silence was the bug).
rename ::vsidebar::selected_sid ::vsidebar::_selected_sid_real
proc ::vsidebar::selected_sid {} { return "" }
::_http::reset
set ::vchat::notes {}
::vchat::slash_command {/shells kill 7}
ok "/shells kill 7 with no session open says so and posts nothing" {
    [llength $::_http::calls] == 0
    && [string match "*no session open*" [lindex $::vchat::notes end 1]]
}
rename ::vsidebar::selected_sid {}
rename ::vsidebar::_selected_sid_real ::vsidebar::selected_sid
# An engine that does not handle them (an older gateway) is not silence
# either.
set ::_http::command_answer {{"handled": false}}
set ::vchat::notes {}
::vchat::slash_command {/monitors cancel 2}
ok "an unhandled /monitors cancel 2 says the command is unknown" {
    [string match "*unknown command: /monitors*" [lindex $::vchat::notes end 1]]
}
set ::_http::command_answer {{"handled": true, "text": "engine answered"}}

# ── a line that is text although it opens with / (SMOKE-19 follow-up) ─
# The composer handed every `/` line to its commands, and `//usr is
# full` or `/home/x/file fails` ended as "unknown command" with no way
# to send it. The gateway's one rule (commands::slash_line, also the
# TUI's and the REPL's) answers `send`; the composer sends that as a
# message, through send_current, exactly as if typed. An unknown
# command says how to send it as text.
puts "\na line the gateway judges text is sent as a message"
foreach {typed answer} {
    {//usr is full} {/usr is full}
    {/home/x/file fails} {/home/x/file fails}
} {
    set ::_http::command_answer [format {{"handled": false, "text": "", "reload": false, "send": "%s"}} $answer]
    set ::_http::chat_answer {{"started": true}}
    ::_http::reset
    set ::vchat::busy 0
    set ::vchat::notes {}
    .mid.chatwrap.chat.input delete 1.0 end
    .mid.chatwrap.chat.input insert 1.0 $typed
    ::vchat::send_current
    set _cmd [lsearch -inline -index 1 $::_http::calls /api/command]
    set _chat [lsearch -inline -index 1 $::_http::calls /api/chat]
    ok "$typed: the gateway is asked with the line as typed" {
        [llength $_cmd] > 0
        && [dict get [::json::json2dict [lindex $_cmd 2]] command] eq $typed
    }
    ok "$typed: sent as the message `$answer`" {
        [llength $_chat] > 0
        && [dict get [::json::json2dict [lindex $_chat 2]] text] eq $answer
    }
}
set ::_http::command_answer {{"handled": false, "text": "", "reload": false}}
::_http::reset
set ::vchat::notes {}
.mid.chatwrap.chat.input delete 1.0 end
.mid.chatwrap.chat.input insert 1.0 {/usr is full}
::vchat::send_current
ok "an unknown command is not sent, and says how to send it as text" {
    [lsearch -index 1 $::_http::calls /api/chat] < 0
    && [string match "*unknown command: /usr*" [lindex $::vchat::notes end 1]]
    && [string match "*start the line with // to send it as text*" [lindex $::vchat::notes end 1]]
}
set ::_http::command_answer {{"handled": true, "text": "engine answered"}}
set ::_http::chat_answer ""


# ── /join by name, and a bare /join that lists (ASK-1) ───────────────
# Help and README promise `/join [name|id]` on the desktop; the desktop
# sent the word verbatim and the gateway looked it up as an exact id,
# so `/join alice` answered "no live chat-backed session alice" and a
# bare /join printed usage. The gateway resolves (one resolver for every
# surface); the desktop must follow the id the gateway resolved, list
# what can be joined for a bare /join, and show a refusal as words.
puts "\n/join by name and bare /join"
::vinst::set_remote_join {}
set _here [::vapi::attached]
set ::vchat::session mine
::_http::reset
set ::vchat::notes {}
set ::_http::candidates {{"sessions": [{"session_id": "20261110_120000_aaaa11", "label": "[alice in #chan]"}, {"session_id": "20261110_110000_bbbb22", "label": "[bob on irc]"}]}}
::vchat::slash_command /join
ok "a bare /join asks the attached gateway who can be joined" {
    [lsearch -index 1 $::_http::calls /api/join/candidates] >= 0
    && [lindex $::_http::calls 0 0] eq $_here
}
ok "…and lists them with their ids, never a usage line" {
    [string match "*20261110_120000_aaaa11*alice in #chan*20261110_110000_bbbb22*bob on irc*" [lindex $::vchat::notes end 1]]
    && ![string match "*usage*" [lindex $::vchat::notes end 1]]
}
set ::_http::candidates {{"sessions": []}}
set ::vchat::notes {}
::vchat::slash_command /join
ok "with nobody to join it says so" {
    [string match "*nobody*" [lindex $::vchat::notes end 1]]
}
::_http::reset
set ::_http::join_answer {return {{"join": {"join_id": "j-name", "from_instance": "", "from_session": "", "target_instance": "", "target_session": "20261110_120000_aaaa11"}}}}
::vchat::slash_command {/join alice}
set _join_body [::json::json2dict [lindex [lsearch -inline -index 1 $::_http::calls /api/join] 2]]
ok "/join alice sends the name for the gateway to resolve" {
    [dict get $_join_body session_id] eq "alice"
}
ok "…and the desktop follows the session the gateway resolved" {
    $::vchat::session eq "20261110_120000_aaaa11"
    && [dict get [::vinst::remote_join] target_session] eq "20261110_120000_aaaa11"
}
::vchat::slash_command /leave
ok "/leave from it posts the resolved id, not the name" {
    [dict get [::json::json2dict [lindex [lsearch -inline -index 1 $::_http::calls /api/join/leave] 2]] session_id] eq "20261110_120000_aaaa11"
    && ![dict size [::vinst::remote_join]]
}
set ::_http::join_answer {error {HTTP 409: {"error":"no session of \"carol\" is open here; /join lists the ones you can join"}}}
set ::vchat::notes {}
set ::vchat::session mine
::vchat::slash_command {/join carol}
ok "a refused join is shown as the gateway's words, not its JSON (SMOKE-20)" {
    [lindex $::vchat::notes end 1] eq [format {join failed on %s: no session of "carol" is open here; /join lists the ones you can join} $_here]
}
set ::_http::join_answer ""

# ── /export takes the engine's formats (ASK-6, SMOKE-24) ─────────────
# `/export JSON` wrote Markdown and `/export xml` wrote Markdown: only a
# lower-case `json` was recognised and everything else fell to the
# default. The engine (export.rs Format::parse) takes md, markdown and
# json in any case and refuses anything else.
puts "\n/export formats"
foreach {line want} {{/export JSON} json {/export md} markdown {/export} markdown {/export Markdown} markdown} {
    ::_http::reset
    set ::vchat::notes {}
    ::vchat::slash_command $line
    ok "$line asks for $want" {
        [lsearch -index 1 $::_http::calls "/api/sessions/mine/export?format=$want"] >= 0
        && [string match "exported to *" [lindex $::vchat::notes end 1]]
    }
}
::_http::reset
set ::vchat::notes {}
::vchat::slash_command {/export xml}
ok "/export xml is refused with the usage and exports nothing" {
    [llength $::_http::calls] == 0
    && [string match "*usage: /export \\\[md|json\\\]*" [lindex $::vchat::notes end 1]]
}

file delete -force $_home
puts "jointest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
