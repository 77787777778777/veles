# veles-desktop/lib/demo.tcl — the real window, driven by a FAKE gateway.
#
# USER 2026-09-17: "it would be good if you figured out a way to replay
# some fake sessions in your xvfb test so that I don't have to invent
# reasons for two different sessions to run just so I can spot-test".
#
# Every cross-session bug this window has had — a marker on the wrong
# row, an approval whose lines went to the transcript you happened to be
# looking at, a ▶ for a turn that ended an hour ago, following that
# unsticks on a switch — needs TWO sessions doing real work at the same
# time to see. Manufacturing that against a live gateway means inventing
# two errands for a model and paying for both, every time, and the
# interesting moment (the approval, the switch mid-stream) lasts seconds.
#
# So: the modules are the REAL ones. main.tcl sources this instead of
# nothing when VELES_DESKTOP_DEMO=1, and it replaces the TRANSPORT only
# — ::vapi's handful of entry points — with a fixture gateway and a
# scripted timeline. Everything above the socket is the shipping code:
# the sidebar, the transcript, the live region, the ask dialog, the
# status bar, follow, the per-session streams.
#
#   scripts/desktop_demo.sh              on your display, to click at
#   scripts/desktop_demo.sh --headless   on a throwaway X, with shots
#
# The timeline is a list of {ms sid event data} played by one `after`
# chain; `::vdemo::script` is the whole scenario and is meant to be
# edited. Nothing here is a pass/fail suite — the suites are in
# lib/*test.tcl. This is the thing you LOOK at.

namespace eval ::vdemo {
    variable t0 0
    variable handler          ;# sid -> the stream handler chat.tcl gave us
    array set handler {}
    variable sessions {}      ;# ordered, freshest first (as the engine sends)
    variable history          ;# sid -> list of {role content}
    array set history {}
    variable live             ;# sid -> 0/1, what /api/sessions reports
    array set live {}
    variable open_asks {}     ;# id -> dict, for GET /api/asks
    variable cursor 0
}

# ── JSON, by hand ───────────────────────────────────────────────────
# Small enough not to want a library, and building the strings (rather
# than overriding get_json to hand back dicts) keeps ::vapi's own
# parsing in the path being exercised.
proc ::vdemo::jstr {s} { return "\"[::vapi::json_escape $s]\"" }
proc ::vdemo::jobj {pairs} {
    set out {}
    foreach {k v} $pairs { lappend out "[jstr $k]:$v" }
    return "\{[join $out ,]\}"
}
proc ::vdemo::jarr {items} { return "\[[join $items ,]\]" }
proc ::vdemo::jbool {b} { return [expr {$b ? "true" : "false"}] }

proc ::vdemo::now {} { return [clock format [clock seconds] -format "%Y-%m-%d %H:%M:%S"] }
proc ::vdemo::ago {secs} {
    return [clock format [expr {[clock seconds] - $secs}] -format "%Y-%m-%d %H:%M:%S"]
}

# ── the fixture world ───────────────────────────────────────────────

proc ::vdemo::session {id title updated count {running 0}} {
    variable sessions
    variable live
    set live($id) $running
    lappend sessions [list $id $title $updated $count]
}

proc ::vdemo::_sessions_json {} {
    variable sessions
    variable live
    set out {}
    foreach s $sessions {
        lassign $s id title updated count
        set r [expr {[info exists live($id)] && $live($id)}]
        set pairs [list id [jstr $id] title [jstr $title] \
            created_at [jstr [ago 86400]] updated_at [jstr $updated] \
            message_count $count running [jbool $r]]
        if {$r} {
            lappend pairs surface [jstr "desktop"] pid 4242 host [jstr "demo"]
        }
        lappend out [jobj $pairs]
    }
    return [jobj [list sessions [jarr $out]]]
}

proc ::vdemo::_history_json {sid} {
    variable history
    variable cursor
    set rows {}
    if {[info exists history($sid)]} {
        foreach m $history($sid) {
            lassign $m role content
            lappend rows [jobj [list role [jstr $role] content [jstr $content]]]
        }
    }
    return [jobj [list messages [jarr $rows] last_msg_id $cursor]]
}

proc ::vdemo::_asks_json {} {
    variable open_asks
    set out {}
    foreach {id d} $open_asks {
        set choices {}
        foreach c [dict get $d choices] {
            lappend choices [jobj [list key [jstr [dict get $c key]] \
                                        label [jstr [dict get $c label]]]]
        }
        lappend out [jobj [list id [jstr $id] \
            session_id [jstr [dict get $d session_id]] \
            kind [jstr [dict get $d kind]] \
            prompt [jstr [dict get $d prompt]] \
            detail [jstr [dict get $d detail]] \
            choices [jarr $choices]]]
    }
    return [jobj [list asks [jarr $out]]]
}

# ── the router ──────────────────────────────────────────────────────
#
# Anything not named here answers "{}". That is deliberate: every module
# in this window already guards its reads (`catch`, `dict exists`), so an
# unmodelled endpoint degrades to "the gateway told me nothing" rather
# than to a stack trace — and the scenario stays readable instead of
# growing a mock for all 60 routes.
proc ::vdemo::_get {path} {
    variable live
    if {$path eq "/api/sessions"} { return [_sessions_json] }
    if {$path eq "/api/asks"}     { return [_asks_json] }
    if {$path eq "/api/health"}   { return [jobj [list status [jstr ok] version [jstr demo]]] }
    # EMPTY, not absent: ::vt::from_engine catches a failed request but
    # reads `themes` out of the reply unguarded, so "{}" kills startup
    # while an empty list falls back to the built-in palette — which is
    # what a demo wants anyway.
    if {$path eq "/api/themes"}   { return [jobj [list themes [jarr {}]]] }
    if {[regexp {^/api/sessions/([^/]+)/history} $path -> sid]} {
        return [_history_json $sid]
    }
    if {[regexp {^/api/sessions/([^/]+)/context} $path -> sid]} {
        set busy [expr {[info exists live($sid)] && $live($sid)}]
        return [jobj [list busy [jbool $busy] \
            history_percent_of_budget 37.5 context_window_tokens 200000 \
            compaction_trigger_percent 85 last_msg_id $::vdemo::cursor]]
    }
    if {[string match "/api/status*" $path]} {
        return [jobj [list provider [jstr "demo"] model [jstr "fixture-1"] \
            uptime_s 1234 platforms [jarr [list [jstr "demo"]]] \
            plan_mode null]]
    }
    return "{}"
}

proc ::vdemo::_post {path body} {
    if {[regexp {^/api/approvals/([^/]+)/decide} $path -> id]} {
        decided $id $body
        return "{}"
    }
    if {[regexp {^/api/sessions/([^/]+)/cancel} $path -> sid]} {
        stop_turn $sid
        return "{}"
    }
    return "{}"
}

# ── the transport, replaced ─────────────────────────────────────────
#
# ATTACHED, to an instance named "demo". Both halves matter: every
# request guard tests `unattached`, and ::vapp::boot tests `attached`
# and drops you on the Instances tab when it is empty — which is not
# what a demo of the chat window wants to open on.
proc ::vapi::unattached {} { return 0 }
proc ::vapi::attached {} { return "demo" }
proc ::vapi::get {path {ms 0}} { return [::vdemo::_get $path] }
proc ::vapi::post {path body {ms 0}} { return [::vdemo::_post $path $body] }
proc ::vapi::delete {path} { return "{}" }
proc ::vapi::get_json_async {path cmd {ms 0}} {
    set raw [::vdemo::_get $path]
    set d {}
    catch { set d [::json::json2dict $raw] }
    after idle [list uplevel #0 [list {*}$cmd 1 $d]]
    return "demo-async"
}
# One "stream" per session, exactly as the real one: the handler is
# bound to its sid, and ::vdemo::emit is what the gateway would have
# written down the socket.
proc ::vapi::session_events {session_id cursor note_cursor handler} {
    set ::vdemo::handler($session_id) $handler
    # A SESSION THAT IS ALREADY WORKING hands over a snapshot before the
    # live boundary, exactly as the engine does — that is how a client
    # attaching mid-turn learns what it missed, and it is worth having
    # in the demo because it is a different painting path from the
    # stream (lib/chat.tcl's _paint_snapshot).
    if {[info exists ::vdemo::live($session_id)] && $::vdemo::live($session_id)} {
        after idle [list ::vdemo::emit $session_id turn-snapshot \
            [dict create session_id $session_id surface demo base_msg_id 0 \
                rounds [list [dict create \
                    reasoning "Picking this up from a snapshot — the stream was not open." \
                    content "Working.\n\n" name grep args "{}" result "ok"]] \
                reasoning "…and this is the tail since that round closed." \
                content ""]]
    }
    after idle [list ::vdemo::emit $session_id session-live \
        [dict create last_msg_id $::vdemo::cursor]]
    return "demo-ev-$session_id"
}
proc ::vapi::sse_open {path handler} { return "demo-sse" }
proc ::vapi::sse_close {tok} {}
proc ::vapi::chat_send {session_id text {attachments {}}} {
    ::vdemo::typed $session_id $text
    return "{}"
}
proc ::vapi::chat_stop {{sid ""}} { ::vdemo::stop_turn $sid ; return "{}" }
proc ::vapi::ask_decide {id choice {text ""}} {
    ::vdemo::decided $id [list $choice $text]
    return "{}"
}

# ── playing the tape ────────────────────────────────────────────────

# One event, into one session's stream, exactly where the real one goes.
proc ::vdemo::emit {sid ev d} {
    variable handler
    if {![info exists handler($sid)]} { return }
    if {[catch { uplevel #0 [list {*}$handler($sid) $ev $d] } e]} {
        puts stderr "demo: $sid $ev → $e"
    }
}

# A turn's worth of reasoning/deltas, paced so a human can switch
# sessions in the middle of it — which is the whole point.
proc ::vdemo::stream {sid at reasoning content} {
    set out {}
    foreach chunk $reasoning {
        lappend out [list $at $sid reasoning [dict create text $chunk]]
        incr at 700
    }
    foreach chunk $content {
        lappend out [list $at $sid delta [dict create text $chunk]]
        incr at 400
    }
    return $out
}

proc ::vdemo::play {script} {
    foreach step $script {
        lassign $step ms sid ev d
        after $ms [list ::vdemo::_step $sid $ev $d]
    }
}

proc ::vdemo::_step {sid ev d} {
    variable live
    switch -- $ev {
        _live_on  { set live($sid) 1 ; catch { ::vsidebar::refresh } ; return }
        _live_off { set live($sid) 0 ; catch { ::vsidebar::refresh } ; return }
        _say      { puts "demo: $d" ; return }
        _open     { catch { ::vchat::open_session $sid } ; return }
        _answer   { catch { ::vask::_answer "ask-demo-1" $d "" } ; return }
    }
    # An ask OUTLIVES the event that raised it — the engine parks it in
    # its registry and GET /api/asks redraws it for a window that was
    # closed, or for a session you switch back to. File it here so the
    # demo has that property too.
    if {$ev eq "ask"} {
        variable open_asks
        catch { dict set open_asks [dict get $d id] $d }
    }
    if {$ev eq "turn-started"} { set live($sid) 1 ; catch { ::vsidebar::refresh } }
    if {$ev eq "turn-result" || $ev eq "turn-error"} {
        set live($sid) 0
        catch { ::vsidebar::refresh }
    }
    emit $sid $ev $d
}

# ── what the window does back ───────────────────────────────────────

# The composer sent something. Echo a turn so typing into the demo is
# not a dead end.
proc ::vdemo::typed {sid text} {
    variable live
    set live($sid) 1
    emit $sid turn-started [dict create surface desktop prompt $text \
        started_at [now] base_msg_id $::vdemo::cursor]
    play [concat \
        [stream $sid 300 [list "They typed: $text" "Nothing here talks to a model — this is a fixture."] \
                         [list "You sent **" $text "**.\n\nThe demo gateway has no model behind it; " \
                               "this reply is a scripted echo so the composer is not a dead end.\n"]] \
        [list [list 3000 $sid turn-result [dict create text "echoed" \
            provider demo model fixture-1 prompt_tokens 12 completion_tokens 34]]]]
}

proc ::vdemo::stop_turn {sid} {
    variable live
    if {$sid eq ""} { return }
    set live($sid) 0
    emit $sid cancelled {}
    emit $sid turn-result [dict create text "(interrupted)" \
        provider demo model fixture-1 prompt_tokens 0 completion_tokens 0]
}

# An ask was answered HERE. Resolve it and let the turn carry on, the
# way the engine would.
proc ::vdemo::decided {id answer} {
    variable open_asks
    if {![dict exists $open_asks $id]} { return }
    set d [dict get $open_asks $id]
    set sid [dict get $d session_id]
    dict unset open_asks $id
    emit $sid note [dict create kind approval \
        summary "approval: [lindex $answer 0] — [dict get $d prompt]"]
    play [concat \
        [stream $sid 400 [list "Approved. Carrying on."] \
                         [list "Done — the fixture turn continues after your answer.\n"]] \
        [list [list 2500 $sid turn-result [dict create text "finished" \
            provider demo model fixture-1 prompt_tokens 900 completion_tokens 120]]]]
}

# ── the scenario ────────────────────────────────────────────────────

# THE FIXTURES ARE BUILT AT SOURCE TIME, not on a timer: main.tcl calls
# ::vsidebar::refresh and opens the freshest session during boot, and a
# world that arrives a second later shows "(no sessions)" and an empty
# transcript. Only the TIMELINE waits.
proc ::vdemo::fixtures {} {
    variable history
    variable open_asks

    set A "20260917_091500_a1f4c0"
    set B "20260917_090200_b7e219"
    set C "20260916_2210_c0ffee"
    set D "20260917_070000_d0d0d0"

    session $A "fixing the unbalanced paren" [now] 14
    session $B "sweeping the log directory"  [now] 8
    # D is running ACCORDING TO THE ENGINE and this window never saw it
    # start — a turn in the tui, telegram or cron. Its ▶ has to appear
    # anyway, and has to go away when the engine lets go of it.
    session $D "started in the tui"          [ago 120] 3 1
    session $C "yesterday's notes"           [ago 90000] 40

    set history($A) {
        {user "walk the AST and find the unbalanced paren"}
        {assistant "I'll parse it and count the depth as I go."}
        {user "the file is lib/chat.tcl"}
    }
    set history($B) {
        {user "sweep the log directory, anything older than a week"}
        {assistant "Listing first — I will not delete anything without showing you."}
    }
    set history($C) {
        {user "what did we decide about the cache?"}
        {assistant "Drop it on every write; the verify pass heals a stale one."}
    }
    set history($D) {
        {user "started from the tui, still going"}
    }

    # NOTHING OPEN AT BOOT. `open_asks` is what GET /api/asks answers,
    # and _open_session_tail asks on every open — so seeding it here
    # would pop the approval dialog the instant the demo opened its
    # sessions, a second in, instead of at the scripted moment. The ask
    # step below files it when it is raised, which is also what makes
    # the redraw path real: switch away and back mid-approval and the
    # question is still there.
    set open_asks {}

}

proc ::vdemo::start {} {
    set A "20260917_091500_a1f4c0"
    set B "20260917_090200_b7e219"
    set D "20260917_070000_d0d0d0"
    puts "── veles desktop demo ─────────────────────────────────────────"
    puts "   A  $A   fixing the unbalanced paren"
    puts "   B  $B   sweeping the log directory"
    puts "   D  $D   'started in the tui' — running per the ENGINE only"
    puts ""
    puts "   what to look at:"
    puts "    · D wears ▶ from the first refresh, though this window"
    puts "      never saw its turn start. At ~25s the engine lets go and"
    puts "      the ▶ clears with no event of its own."
    puts "    · A and B both stream from ~3s. Switch between them: the ▸"
    puts "      and the accent move with you, at once, with no turn."
    puts "    · Sit at the BOTTOM of A, switch to B, come back: still"
    puts "      following. Scroll A UP first and it stays where you left"
    puts "      it, with the ▾ button."
    puts "    · At ~12s B asks for an approval. If you are in A, the"
    puts "      dialog says B is asking and offers to go there — and"
    puts "      every ⟶ line lands in B, none in A."
    puts "    · Leave it unanswered: ~35s it is relayed (a line in B),"
    puts "      ~70s somebody else answers it and the dialog closes."
    puts "───────────────────────────────────────────────────────────────"

    # BOTH SESSIONS OPEN, view left on A. A stream exists only for a
    # session this window has opened — that is the real rule, and it is
    # also what a user spot-testing a background approval has: they were
    # working in both. B first, A second, so A is the one on screen.
    play [concat \
        [list [list 100 $B _open ""]] \
        [list [list 200 $A _open ""]] \
        [list [list 1200 $A turn-started [dict create surface desktop \
            prompt "walk the AST and find the unbalanced paren" \
            started_at [now] base_msg_id 0]]] \
        [stream $A 1600 \
            [list "The file is 4600 lines; counting depth in one pass." \
                  "Braces inside comments and inside strings do not count, so the scanner needs both states." \
                  "Tcl has no block comment, which helps."] \
            [list "Reading `lib/chat.tcl` and tracking depth.\n\n" \
                  "So far the depth returns to zero at every top-level `proc`, " \
                  "which means the imbalance is *inside* one of them.\n\n"]] \
        [list [list 6000 $A tool [dict create state start name grep \
            args "{\"pattern\":\"^proc \",\"path\":\"lib/chat.tcl\"}"]]] \
        [list [list 7200 $A tool [dict create state end name grep \
            result "142 procs"]]] \
        [stream $A 7600 \
            [list "142 procs. Bisecting by depth-at-proc-start." \
                  "The first proc whose entry depth is non-zero is the one before the break."] \
            [list "Bisecting.\n\n" \
                  "The imbalance starts inside `_paint_history_rows` — " \
                  "its `foreach` opens a brace the `if` below never closes.\n\n" \
                  "Here is the fix:\n\n```tcl\nforeach m \$msgs {\n    _paint_one_row \$m \$callmap\n}\n```\n\n" \
                  "That is the whole change. Everything after it was a symptom.\n"]] \
        [list [list 2800 $B turn-started [dict create surface desktop \
            prompt "sweep the log directory, anything older than a week" \
            started_at [now] base_msg_id 0]]] \
        [stream $B 3200 \
            [list "Listing before deleting; the user asked for a sweep, not a surprise."] \
            [list "Listing `/var/log/veles` first.\n\n"]] \
        [list [list 5200 $B tool [dict create state start name terminal \
            args "{\"command\":\"find /var/log/veles -mtime +7 | wc -l\"}"]]] \
        [list [list 6400 $B tool [dict create state end name terminal \
            result "412"]]] \
        [stream $B 6800 \
            [list "412 files. That is a lot to remove without asking."] \
            [list "412 files match. Asking before I touch any of them.\n\n"]] \
        [list [list 12000 $B ask [dict create id "ask-demo-1" \
            session_id $B kind approval \
            prompt "Delete 412 files from /var/log/veles?" \
            detail "rm -rf /var/log/veles/2026-08*\n\nThe oldest is 41 days; the newest matching is 8 days.\nNothing in the list is currently open by another process." \
            choices {{key once label "allow once"} {key session label "allow for this session"}
                     {key always label "always allow"} {key deny label "deny"}}]]] \
        [list [list 8000 $D _live_off ""]] \
        [list [list 8200 $D _say "the engine let go of D — its ▶ clears on this refresh, with no event of its own"]] \
        [list [list 22000 $B ask-relayed [dict create id "ask-demo-1" \
            to "telegram:owner" after_secs 10]]] \
        [list [list 60000 $B ask-resolved [dict create id "ask-demo-1" \
            by "answer" answer "deny"]]] \
        [list [list 60200 $B note [dict create kind approval \
            summary "approval: deny (answered on telegram) — Delete 412 files from /var/log/veles?"]]] \
        [list [list 61000 $B turn-result [dict create text "left them alone" \
            provider demo model fixture-1 prompt_tokens 880 completion_tokens 96]]] \
        [list [list 40000 $A turn-result [dict create text "found it" \
            provider demo model fixture-1 prompt_tokens 1420 completion_tokens 260]]]]

    # HEADLESS plays the whole thing with nobody at the keyboard, so it
    # answers its own question — otherwise every shot after the approval
    # is a picture of the dialog. Interactively the question is yours,
    # and if you leave it the relay (22s) and the foreign answer (60s)
    # happen to you instead.
    if {[info exists ::env(VELES_DEMO_AUTO)] && $::env(VELES_DEMO_AUTO) eq "1"} {
        play [list [list 28000 $B _answer once]]
    }
}

::vdemo::fixtures
after 900 ::vdemo::start
