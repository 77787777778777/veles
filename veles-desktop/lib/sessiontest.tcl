# veles-desktop/lib/sessiontest.tcl — switching sessions while a turn is
# running.
#
# USER: "I can't switch between sessions mid-turn apparently".
#
# open_session used to refuse outright ("busy — stop the current turn
# first"). The turn runs on the SERVER; the only real obstacle was that
# every piece of stream state was global — one `busy` flag, one
# streaming anchor, one `event:`/token pair in ::vapi — so a second
# session had nowhere to put its own.
#
# These tests drive the dispatch logic directly: they set the same
# variables ::vapi sets before each callback, and assert on what the
# handlers do with them.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]

set failed 0
set passed 0
set paints 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

# ── the pieces chat.tcl leans on ────────────────────────────────────
namespace eval ::vstatus {
    proc set_msg {m} { set ::vstatus::last $m }
    # Same contract as the real one (statusbar.tcl): the provider half
    # is captured for _paint_quota BEFORE any widget guard — the stub
    # must not drop it either, or the plan-relevance pins below lie.
    proc set_route {t {ovr 0}} {
        set ::vstatus::route $t
        set ::vstatus::route_ovr $ovr
        if {$t eq ""} {
            set ::vstatus::route_prov ""
        } else {
            set ::vstatus::route_prov [lindex [split $t /] 0]
        }
    }
}
set ::vstatus::last ""
set ::vstatus::route ""
set ::vstatus::route_ovr 0
set ::vstatus::route_prov ""
namespace eval ::vsidebar {
    variable refreshes 0
    proc refresh {} { incr ::vsidebar::refreshes }
    # COUNTED: open_session has to move the ▸ / selection / accent onto
    # the session it just put on screen. `refresh` is NOT the answer —
    # it blocks on GET /api/sessions and runs only on turn boundaries,
    # which is why the marks sat on the previous session until the next
    # turn (USER 2026-09-17).
    variable marks 0
    proc mark_current {} { incr ::vsidebar::marks }
    proc label_for {sid} { return "" }
}
namespace eval ::vapi {
    variable stopped {}
    proc chat_stop {{sid ""}} { lappend ::vapi::stopped $sid }
    # The events stream: open_session opens one, but the tests drive
    # on_session_event directly, so opening is a no-op that records the
    # intent (session + resume cursors, msg and note) and hands back a
    # fake token.
    variable ev_opens {}
    proc session_events {sid cursor note_cursor handler} {
        variable ev_opens
        lappend ev_opens [list $sid $cursor $note_cursor]
        return "evtok-$sid"
    }
    proc sse_close {tok} {}
    proc fatal_code {code} { return [expr {$code == 401 || $code == 403 || $code == 404}] }
    proc http_reason {code {at ""}} { return "http $code at $at" }
}

# The REAL module is sourced — a restated copy of the logic would drift
# from it and prove nothing. chat.tcl only touches widgets inside proc
# bodies, so loading it needs nothing but its collaborators present.
namespace eval ::vsidebar { proc selected_sid {} { return "" } }
# The live-delta flush re-renders the accumulated raw delta through
# ::vmd::render; a bare `forget_links` stub used to be enough because the
# old flush deleted nothing (its mark sat at the phantom `end`), so the
# render error was never observed. The flush now actually deletes and
# re-renders, so `render` (and the media it may route through) must exist.
namespace eval ::vmd    { proc forget_links {w} {} ; proc render {w text} { $w insert end $text {} } }
namespace eval ::vmedia { proc forget {w} {} ; proc insert {w p} { $w insert end "MEDIA:$p\n" {} } }
namespace eval ::vfind  { proc reset_marks {} {} ; proc build {} {} }
namespace eval ::vedit  { proc hist_push {t} {} }
namespace eval ::vt     { variable T ; array set T {bg_input #101018 bg_panel #181820 fg #e0e0e0 fg_dim #9a9aa8 pink #ff79c6 reasoning #8888aa accent #88c0d0 green #50fa7b red #ff5555 gold #f1fa8c user #ff79c6 cyan #8be9fd assistant #e0e0e0 tool #f1fa8c system #6272a4} }
namespace eval ::vkeymap { variable current normie ; proc label {a} { return "?" } ;
                          proc key_for {cmd} { return "C-k" } }
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }
namespace eval ::va11y  { proc name {w t} {} }
namespace eval ::vctx   { proc attach {w items} {} }
# ::vchat::_make_log builds a transcript per session and wraps each one
# in the read-only Text helper. The first session claims the widget this
# file hand-builds below; the SECOND gets a real `_make_log` call, which
# is when a missing stub stops the suite dead.
namespace eval ::vtext  { proc readonly {w} { $w configure -state disabled } ;
                          proc restyle_all {} {} }
namespace eval ::vask   { proc show {d} {} ; proc secret_dialog {nm id} {} }
namespace eval ::vplace { proc centered {args} {} ; proc near {args} {} }
source [file join $_dir chat.tcl]

# The composer/transcript widgets the handlers poke at.
foreach f {.mid .mid.chatwrap .mid.chatwrap.chat .mid.chatwrap.chat.bar} { frame $f }
text .mid.chatwrap.chat.log -state disabled
text .mid.chatwrap.chat.input
button .mid.chatwrap.chat.bar.send
button .mid.chatwrap.chat.bar.stop
button .mid.chatwrap.chat.bar.detach
# The follow cases below need MAPPED widgets with real geometry: yview
# fractions on an unmapped text widget are degenerate — nothing ever
# reports "at the bottom", and "is this transcript at its live edge" is
# the whole question. Map the ancestor CHAIN (a packed child of an
# unpacked frame is still unmapped); the per-session log widgets are
# packed by ::vchat::_show_log itself, with its own flags.
wm geometry . 420x260
pack .mid -fill both -expand true
pack .mid.chatwrap -fill both -expand true
pack .mid.chatwrap.chat -fill both -expand true
update idletasks

proc reset {} {
    set ::vchat::session ""
    set ::vchat::busy 0
    set ::vchat::running {}
    set ::vchat::stream_session ""
    set ::vchat::turn_anchor {}
    set ::vapi::stopped {}
    set ::vapi::ev_opens {}
    # Event-stream state: clear it and cancel any pending reconnect so a
    # backoff timer from one test cannot fire into the next. ONE STREAM
    # PER SESSION now, so every one of these is an array keyed by sid —
    # wiping them wholesale is exactly what a fresh window looks like.
    foreach _t [array names ::vchat::ev_retry_after] {
        catch { after cancel $::vchat::ev_retry_after($_t) }
    }
    foreach _a {ev_stream ev_cursor ev_retry ev_retry_after ev_replay
                ev_snap_seen ev_reconnect_pending ev_reconnected
                ev_disconnected} {
        catch { array unset ::vchat::$_a }
    }
    set ::vchat::turn_reload_anchor {}
    # The live-region state too: sections below open fresh turns, and a
    # region left open by the previous section's last on_reasoning (its
    # turn never got an on_done) would suppress the next section's
    # reasoning HEADER — the same open-region suppression the app
    # itself clears at turn end, cleared here for the same reason.
    set ::vchat::live_rtag ""
    set ::vchat::live_rhdr ""
    set ::vchat::live_rtext ""
    set ::vchat::live_rpend ""
    set ::vchat::live_blocks {}
    set ::vchat::live_ptext ""
    set ::vchat::live_last_kind ""
    catch { array unset ::vchat::live_tool_marks }
    catch { array unset ::vchat::tool_rounds }
    catch { array unset ::vchat::at_edge }
    # Per-session transcript widgets: a widget claimed by one test's
    # session id would otherwise be handed straight back to the next
    # test that happens to reuse the id, carrying its text with it.
    foreach _sid [dict keys $::vchat::logs] { catch { ::vchat::_drop_log $_sid } }
    set ::vchat::logs {}
    set ::vchat::logs_lru {}
    .mid.chatwrap.chat.log configure -state normal
    .mid.chatwrap.chat.log delete 1.0 end
    .mid.chatwrap.chat.log configure -state disabled
}

# The transcript ON SCREEN — the VISIBLE session's, deliberately, not
# `::vchat::log`'s. Inside a stream callback `log` follows the event's
# session, which is the whole point of it; a helper that did the same
# would answer "what did B's stream paint" when the assertion means
# "what is in front of the user".
#
# WITHOUT the time stamps (the HH:MM heads and the date lines): this suite
# is about which session an event paints into and in what shape, and the
# stamps are the clock's — rendertest pins them.
proc _unstamped {w} {
    set b [$w get 1.0 end-1c]
    set cuts {}
    foreach t {stamp stampday} {
        foreach {a z} [$w tag ranges $t] {
            lappend cuts [list [$w count -chars 1.0 $a] [$w count -chars 1.0 $z]]
        }
    }
    foreach c [lsort -decreasing -integer -index 0 $cuts] {
        lassign $c a z
        set b [string replace $b $a [expr {$z - 1}]]
    }
    return $b
}
proc body {} {
    set w [::vchat::log_for $::vchat::session]
    if {$w eq "" || ![winfo exists $w]} { return "" }
    return [_unstamped $w]
}
# One named session's transcript, on screen or not.
proc body_of {sid} {
    set w [::vchat::log_for $sid]
    if {$w eq "" || ![winfo exists $w]} { return "" }
    return [_unstamped $w]
}

# ── whose event is it ───────────────────────────────────────────────
puts "event routing"
reset
set ::vchat::session "A"
set ::vchat::stream_session "A"
ok "an event for the visible session is displayed" {[::vchat::for_display]}
ok "…and is attributed to it" {[::vchat::event_session] eq "A"}
set ::vchat::stream_session "B"
ok "an event for another session is NOT displayed" {![::vchat::for_display]}
ok "…but is still attributed to that session" {[::vchat::event_session] eq "B"}
set ::vchat::stream_session ""
ok "outside a callback, events belong to the visible session" \
    {[::vchat::event_session] eq "A" && [::vchat::for_display]}

# ── the reported bug: output must not bleed across sessions ─────────
puts "\nstream output stays in its own transcript"
reset
set ::vchat::session "A"
set ::vchat::stream_session "A"
::vchat::on_delta "hello from A"
ok "A's delta lands in A's transcript" {[body] eq "hello from A"}

set ::vchat::stream_session "B"
::vchat::on_delta "hello from B"
ok "B's delta does NOT land in A's transcript" {[body] eq "hello from A"}
::vchat::on_reasoning "B is thinking"
ok "…nor does B's reasoning" {[body] eq "hello from A"}
::vchat::on_tool "start" "shell" {}
ok "…nor B's tool calls" {[body] eq "hello from A"}
::vchat::on_note [dict create kind Compaction summary "squashed"]
ok "…nor B's engine notes" {[body] eq "hello from A"}

# ── per-session busy state ──────────────────────────────────────────
puts "\nbusy is per session"
reset
set ::vchat::session "A"
::vchat::_mark_running "A" 1
ok "starting A's turn marks A running" {[::vchat::is_running "A"]}
ok "…and the visible session shows busy" {$::vchat::busy}
ok "…and the send button becomes Steer (the composer stays live)" \
    {[.mid.chatwrap.chat.bar.send cget -text] eq "Steer"
     && [.mid.chatwrap.chat.bar.send cget -state] eq "normal"}
ok "…and stop is enabled" {[.mid.chatwrap.chat.bar.stop cget -state] eq "normal"}
# Detach arms with Stop (Stage 4): whether the turn is INSIDE a
# foreground command is engine state the client cannot see, so the
# button arms whenever Stop does and the endpoint refuses honestly
# (409) when it was not detachable.
ok "…and detach is enabled" {[.mid.chatwrap.chat.bar.detach cget -state] eq "normal"}

::vchat::_mark_running "B" 1
ok "B can run at the same time" {[::vchat::is_running "B"]}
ok "…without changing A's on-screen state" {$::vchat::busy}

# The other direction is the one that matters: B's turn ENDING must not
# free A's composer. A version of _mark_running that ignores which
# session it was handed passes every "still busy" check above and fails
# only here.
::vchat::_mark_running "B" 0
ok "B finishing leaves A steering" {$::vchat::busy}
ok "…and the button still reads Steer" \
    {[.mid.chatwrap.chat.bar.send cget -text] eq "Steer"}
ok "…while B is no longer marked running" {![::vchat::is_running "B"]}
ok "…and A still is" {[::vchat::is_running "A"]}

# Switching to a session with no turn must free the composer.
set ::vchat::session "C"
::vchat::_mark_running "C" 0
ok "an idle session is not busy" {!$::vchat::busy}
ok "…and its composer reads Send again" \
    {[.mid.chatwrap.chat.bar.send cget -text] eq "Send"
     && [.mid.chatwrap.chat.bar.send cget -state] eq "normal"}

# ── a turn finishing while you are elsewhere ────────────────────────
puts "\nturns that end off screen"
reset
set ::vchat::session "A"
::vchat::_mark_running "B" 1
dict set ::vchat::turn_anchor B 1.0
# Now B finishes while A is on screen.
set ::vchat::stream_session "B"
::vchat::on_done [dict create provider p model m prompt_tokens 1 \
                      completion_tokens 2 text "B's answer"]
ok "B stops being marked as running" {![::vchat::is_running "B"]}
ok "…B's answer is NOT painted into A" {![string match "*B's answer*" [body]]}
ok "…A's composer is untouched" {!$::vchat::busy}
ok "…and the status line says a turn finished elsewhere" \
    {[string match "*another session*" $::vstatus::last]}

# A DROPPED EVENT STREAM. The contract is the same as (E) — the stream
# dying no longer means the turn died — but the mechanism changed: there
# is no socket-loss watcher to arm any more. The stream reconnects ITSELF
# (irc.tcl's supervisor), resuming from the cursor, so the session stays
# marked running and a reconnect is scheduled. Wedged-forever would still
# be the bug, in the other direction: unmarking on a socket event the
# server never confirmed.
reset
set ::vchat::session "A"
::vchat::_mark_running "B" 1
::vchat::on_session_event "B" closed [dict create status eof code 0 url http://x]
ok "a dropped stream leaves the session RUNNING (the turn continues server-side)" \
    {[::vchat::is_running "B"]}
ok "…and schedules a reconnect (no poller, no 200-tick give-up)" \
    {[info exists ::vchat::ev_retry_after(B)] && $::vchat::ev_retry_after(B) ne ""}
ok "…and says nothing in A's transcript" {[body] eq ""}
# B's backoff is B's: a hiccup on one of the twelve streams must not
# stand the other eleven down.
ok "…scoped to the stream that dropped, not to every stream" \
    {![info exists ::vchat::ev_retry_after(A)]}
catch { after cancel $::vchat::ev_retry_after(B) }
set ::vchat::ev_retry_after(B) ""
set ::vchat::ev_retry(B) 1000
::vchat::_mark_running "B" 0

# A rejected token (401/403/404) is NOT transient: the supervisor must
# stop rather than retry a failure forever (irc.tcl's blank-pane lesson).
reset
set ::vchat::session "A"
set ::vchat::ev_retry_after(A) ""
::vchat::on_session_event "A" closed [dict create status error code 401 url http://x]
ok "a rejected stream does not schedule a retry" {$::vchat::ev_retry_after(A) eq ""}

# A RESET end is one WE started: sse_close/http::reset runs the token's
# -command from inside our own teardown (a switch or a reconnect closing
# the previous stream). Treating it as a drop armed a phantom reconnect
# each time, and each reconnect closed the LIVE stream whose reset armed
# the next — a once-a-second reconnect storm that repainted the in-flight
# snapshot and rebuilt the sidebar every cycle (USER: "the session list
# and the output flicker while a turn runs; a cursor rides top to
# bottom"). The api layer silences reset-ends before they reach here;
# this is the same rule on the consumer side.
reset
set ::vchat::session "A"
set ::vchat::ev_stream(A) "tok-live"
set ::vchat::ev_retry_after(A) ""
::vchat::on_session_event "A" closed \
    [dict create status reset code 200 url http://x token tok-stale]
ok "a stream WE reset does not schedule a retry" {$::vchat::ev_retry_after(A) eq ""}
ok "…and does not clobber the token that is live now" {$::vchat::ev_stream(A) eq "tok-live"}

# An error for another session likewise.
reset
set ::vchat::session "A"
::vchat::_mark_running "B" 1
set ::vchat::stream_session "B"
::vchat::on_error "boom"
ok "an off-screen error clears that session" {![::vchat::is_running "B"]}
ok "…and does not write into A" {[body] eq ""}

# ── stop only stops what you are looking at ─────────────────────────
puts "\nstop is scoped (and explicit, never socket-side)"
reset
namespace eval ::vapi {
    variable cancels {}
    proc post_json {path body} {
        variable cancels
        if {[string match "*/cancel" $path]} {
            lappend cancels [lindex [split $path /] end-1]
            return [dict create cancelling 1]
        }
        return [dict create]
    }
    proc streaming_sessions {} { return {A B} }
}
set ::vchat::session "A"
::vchat::_mark_running "A" 1
::vchat::_mark_running "B" 1
::vchat::stop_current
ok "stop_current cancels A via the endpoint" {$::vapi::cancels eq {A}}
ok "…and never touches the socket" {$::vapi::stopped eq {}}

# Detach is scoped the same way: it POSTs /detach for the session ON
# SCREEN only — B keeps running untouched (the engine's request_detach
# is per session, and the client must not guess another one).
namespace eval ::vapi {
    variable detaches {}
    proc post_json {path body} {
        variable cancels
        variable detaches
        if {[string match "*/detach" $path]} {
            lappend detaches [lindex [split $path /] end-1]
            return [dict create detaching 1]
        }
        if {[string match "*/cancel" $path]} {
            lappend cancels [lindex [split $path /] end-1]
            return [dict create cancelling 1]
        }
        return [dict create]
    }
    proc streaming_sessions {} { return {A B} }
}
set ::vapi::cancels {}
::vchat::detach_current
ok "detach_current detaches A via its own endpoint" {$::vapi::detaches eq {A}}
ok "…and never touches B" {$::vapi::detaches ne {A B}}
ok "…and is not a cancel in disguise" {$::vapi::cancels eq {}}

set ::vapi::cancels {}
set ::vchat::ev_stream(A) "tok-a"
set ::vchat::ev_stream(B) "tok-b"
::vchat::stop_all
ok "stop_all cancels every running turn (from the running set, not the streams)" \
    {$::vapi::cancels eq {A B}}
ok "…then disconnects EVERY event stream, not just one" \
    {[llength [::vchat::streaming_sessions]] == 0}

# ── deleting a session cleans up after it ───────────────────────────
puts "\ndeleted sessions"
reset
set ::vchat::session "A"
::vchat::_mark_running "B" 1
dict set ::vchat::turn_anchor B 1.0
set ::vchat::ev_stream(A) "tok-a"
set ::vchat::ev_stream(B) "tok-b"
::vchat::clear_if_current "B"
ok "a deleted session's turn is cancelled" {[lsearch $::vapi::cancels B] >= 0}
ok "…and its event stream is disconnected" \
    {[lsearch -exact [::vchat::streaming_sessions] B] < 0}
ok "…while the session you are looking at keeps its own" \
    {[lsearch -exact [::vchat::streaming_sessions] A] >= 0}
ok "…and it stops being marked running" {![::vchat::is_running "B"]}
ok "…and its anchor is dropped" {![dict exists $::vchat::turn_anchor B]}
ok "…and the visible session is untouched" {$::vchat::session eq "A"}

# ── the switch itself ───────────────────────────────────────────────
#
# The headline: open_session must actually change sessions while a turn
# is running. Checking the source no longer contains the refusal string
# would be satisfied by this very comment mentioning it — so press the
# button instead.
puts "\nswitching while a turn runs"

# A history endpoint, so open_session has something to load.
proc ::vapi::get_json {path} {
    if {[string match "*/history" $path]} {
        return [dict create messages [list [dict create role user content "old turn"]]]
    }
    return [dict create]
}
proc ::vchat::emit_message {kind text {reason ""} {tools {}} {at now}} {
    # `[::vchat::log]`, not one hardcoded path: sessions keep their own
    # transcript, so a stub naming the historic widget would paint every
    # session's history into whichever one happened to be claimed first
    # — and the assertions below would then be reading the wrong buffer.
    set w [::vchat::log]
    # Counted, so "nothing was repainted" can be asserted as a FACT
    # rather than inferred from the text looking the same — a repaint
    # that reproduces identical output is exactly the case a text
    # comparison cannot see.
    incr ::paints
    $w configure -state normal
    $w insert end "$kind: $text\n"
    # Faithful about `reason`: on_done now reconciles reasoning-only
    # turns through emit_message too (live ≡ reload, R7), and a stub
    # that drops the argument eats the reasoning entirely.
    if {$reason ne ""} {
        $w insert end "reason: $reason\n"
    }
    $w configure -state disabled
}

reset
::vchat::open_session "A"
::vchat::_mark_running "A" 1
dict set ::vchat::turn_anchor A 1.0
::vchat::open_session "B"
ok "the session really changed while A was running" {$::vchat::session eq "B"}
ok "…A is still running (the turn was not killed)" {[::vchat::is_running "A"]}
ok "…nothing was sent to stop it" {$::vapi::stopped eq {}}
ok "…B's composer is usable" {!$::vchat::busy}
# INVERTED, deliberately (2026-09-15). A's anchor used to be dropped on
# the way out because A's transcript was about to be DELETED — the
# switch wiped the one widget and repainted it for B, so an index into
# it meant nothing. Sessions keep their own widget now, so A's anchor
# still points at A's own text, which is precisely what lets A's turn
# keep landing in the right place while B is the one on screen.
ok "…and A's transcript anchor SURVIVES, because A's transcript does" \
    {[dict exists $::vchat::turn_anchor A]}
ok "…A's transcript widget was not destroyed" \
    {[winfo exists [::vchat::log_for A]]}
ok "…and B is looking at a different widget than A" \
    {[::vchat::log_for A] ne [::vchat::log_for B]}
ok "…B's history was loaded" {[string match "*old turn*" [body]]}

# Coming back to a session whose turn is still going: the stream hands
# the in-flight turn over as a SNAPSHOT and paints what streamed before
# this window attached. (open_session no longer arms this itself — the
# bus is the one source of "a turn is live and here is its partial".)
::vchat::open_session "A"
ok "switching back finds A still busy" {$::vchat::busy}
ok "…the stop button is live again" \
    {[.mid.chatwrap.chat.bar.stop cget -state] eq "normal"}
::vchat::on_session_event "A" turn-snapshot \
    [dict create surface desktop content "partial answer" reasoning "" tools {} base_msg_id 0]
ok "…the snapshot finds the streaming region still anchored" \
    {[dict exists $::vchat::turn_anchor A]}
# A KEPT transcript is not reloaded, so there is nothing to re-anchor
# AGAINST: the live region still begins exactly where the turn opened
# it, in text that never went away. Moving it would orphan whatever the
# turn had already streamed.
ok "…and leaves a kept transcript's anchor exactly where it was" \
    {[dict get $::vchat::turn_anchor A] eq "1.0"}
ok "…and paints the partial answer streamed before we attached" \
    {[string match "*partial answer*" [body]]}

# THE RELOAD PATH IS STILL THERE, for a session this window has never
# shown or whose transcript the LRU evicted. That one HAS no anchor, and
# the snapshot must plant one at the END of the reloaded history —
# anchoring at the top would make on_done delete somebody else's text.
::vchat::_drop_log "A"
dict unset ::vchat::turn_anchor A
::vchat::open_session "A"
::vchat::on_session_event "A" turn-snapshot \
    [dict create surface desktop content "partial answer" reasoning "" tools {} base_msg_id 0]
ok "a RELOADED transcript anchors the region at its end, not the top" \
    {[dict exists $::vchat::turn_anchor A]
     && [dict get $::vchat::turn_anchor A] ne "1.0"}

# And the stream that was running all along now paints into A again.
set ::vchat::stream_session "A"
::vchat::on_delta "resumed"
ok "A's stream paints into A once more" {[string match "*resumed*" [body]]}

# ── a turn that produced no answer ──────────────────────────────────
#
# USER: "I only got reasoning output to my first prompt? model claims it
# delivered the response."
puts "\nempty replies"
reset
set ::vchat::session "A"
set ::vchat::stream_session "A"
dict set ::vchat::turn_anchor A 1.0
::vchat::_mark_running "A" 1
::vchat::on_reasoning "thinking very hard"
::vchat::on_done [dict create provider p model m prompt_tokens 1 \
    completion_tokens 40 text "" \
    notice "the model returned no message text (p/m, 0 tool round(s), 40 completion tokens)"]
ok "the transcript says the reply was empty" \
    {[string match "*no message text*" [body]]}
ok "…and the reasoning is still there to look at" \
    {[string match "*thinking very hard*" [body]]}
ok "…the status line does not claim success" \
    {[string match "*no text*" $::vstatus::last]}
ok "…and the turn is finished" {![::vchat::is_running "A"]}

# A normal reply must NOT get the warning.
reset
set ::vchat::session "A"
set ::vchat::stream_session "A"
dict set ::vchat::turn_anchor A 1.0
::vchat::_mark_running "A" 1
::vchat::on_done [dict create provider p model m prompt_tokens 1 \
    completion_tokens 40 text "here is the answer" notice null]
ok "a real reply is shown" {[string match "*here is the answer*" [body]]}
ok "…with no empty-reply warning" {![string match "*no message text*" [body]]}
ok "…and a JSON null notice is not printed literally" \
    {![string match "*null*" [body]]}

# The all-or-nothing catch: `text` used to be fetched last, so a done
# event missing an earlier key dropped the reply too.
reset
set ::vchat::session "A"
set ::vchat::stream_session "A"
dict set ::vchat::turn_anchor A 1.0
::vchat::_mark_running "A" 1
::vchat::on_done [dict create text "reply with no metadata"]
ok "a done event missing provider/model still shows the reply" \
    {[string match "*reply with no metadata*" [body]]}

# ── the route the status bar shows ──────────────────────────────────
#
# USER: "during the turn the status bar only seems to show provider,
# should be full provider->model chain" and "it's hard to see what the
# model selected is before first prompt".
puts "\nthe status bar route"
reset
set ::vchat::session "A"
set ::vchat::stream_session "A"
set ::vstatus::route ""
::vchat::on_route "openrouter" "anthropic/claude"
ok "the route event shows provider AND model" \
    {$::vstatus::route eq "openrouter/anthropic/claude"}
ok "…and the transient message says so too" \
    {[string match "*openrouter/anthropic/claude*" $::vstatus::last]}

# A provider-only event (an older gateway) must not print a bare slash.
set ::vstatus::route ""
::vchat::on_route "solo"
ok "a model-less route degrades to just the provider" {$::vstatus::route eq "solo"}

# Another session's route must not repaint this session's label.
set ::vstatus::route "keep/me"
set ::vchat::stream_session "B"
::vchat::on_route "other" "other-model"
ok "another session's route does not touch the label" {$::vstatus::route eq "keep/me"}
set ::vchat::stream_session "A"

# refresh_route: live agent → its effective route, flagged when overridden.
proc ::vapi::get_json {path} {
    if {[string match "*/override" $path]} {
        return [dict create live 1 provider "prov" model "mod" overridden 1]
    }
    return [dict create]
}
set ::vstatus::route ""
::vchat::refresh_route "A"
ok "refresh_route reports the live route" {$::vstatus::route eq "prov/mod"}
ok "…and flags it as a session override" {$::vstatus::route_ovr == 1}

# Not started yet, nothing parked → the CONFIGURED DEFAULT, so a new
# session still says what it will use.
proc ::vapi::get_json {path} {
    if {[string match "*/override" $path]} { return [dict create live 0 pending null] }
    if {$path eq "/api/config"} {
        return [dict create default_provider "dp" default_model "dm"]
    }
    return [dict create]
}
set ::vstatus::route ""
::vchat::refresh_route "A"
ok "a fresh session falls back to the configured default" {$::vstatus::route eq "dp/dm"}
ok "…and is not flagged as an override" {$::vstatus::route_ovr == 0}

# A parked (deferred) override is shown as pending, not silently hidden.
# BOTH halves: the pending row carries the provider, and painting only
# the model leaves route_prov empty — the quota field then reads the
# CROSS-PLAN figure for a session that is parked on z.ai (found live,
# 2026-09-15).
proc ::vapi::get_json {path} {
    if {[string match "*/override" $path]} {
        return [dict create live 0 pending [dict create provider "custom:zai-coding-plan" model "parked"]]
    }
    return [dict create]
}
set ::vstatus::route ""
::vchat::refresh_route "A"
ok "a parked override shows as pending" {[string match "*parked*" $::vstatus::route]}
ok "…and is marked" {$::vstatus::route_ovr == 1}
ok "…and carries the PROVIDER, so the quota field reads the right pool" \
    {$::vstatus::route eq "custom:zai-coding-plan/parked (pending)"}
ok "…route_prov resolves to the plan" {$::vstatus::route_prov eq "custom:zai-coding-plan"}

# A pending row with NO provider (older store rows) still paints the
# model, just without inventing a slash.
proc ::vapi::get_json {path} {
    if {[string match "*/override" $path]} {
        return [dict create live 0 pending [dict create model "parked2"]]
    }
    return [dict create]
}
set ::vstatus::route ""
::vchat::refresh_route "A"
ok "a bare-model pending still paints" {$::vstatus::route eq "parked2 (pending)"}

# ── history rows that lack a key must not kill the load ─────────────
#
# A persisted engine note has no `content`. open_session used to read
# `[dict get $m content]` BEFORE looking at the role, so the missing key
# threw, the enclosing catch reported "failed to load history", and
# EVERY message after the first note was silently lost. Same shape as
# the vanishing answers: one strict lookup on a row that legitimately
# lacks the key.
puts "\nhistory rows with missing keys"

set ::note_calls 0
proc ::vchat::on_note {d {live 1}} { incr ::note_calls }
proc ::vapi::get_json {path} {
    if {[string match "*/history" $path]} {
        return [dict create messages [list \
            [dict create role user content "first"] \
            [dict create role note kind memory summary "12k injected"] \
            [dict create role assistant content "AFTER THE NOTE"] \
            [dict create role note kind schedule summary "ran on a schedule"] \
            [dict create role user content "LAST"]]]
    }
    return [dict create]
}
reset
::vchat::open_session "H"
ok "a note row does not abort the load" \
    {![string match "*failed to load history*" [body]]}
ok "…messages BEFORE the note survive" {[string match "*first*" [body]]}
ok "…and messages AFTER it do too" {[string match "*AFTER THE NOTE*" [body]]}
ok "…including the last one" {[string match "*LAST*" [body]]}
ok "…and every note was rendered" {$::note_calls == 2}

# A row missing `content` entirely (not a note) must also not abort.
set ::note_calls 0
proc ::vapi::get_json {path} {
    if {[string match "*/history" $path]} {
        return [dict create messages [list \
            [dict create role assistant] \
            [dict create role user content "STILL HERE"]]]
    }
    return [dict create]
}
reset
::vchat::open_session "H2"
ok "a content-less message does not abort the load either" \
    {[string match "*STILL HERE*" [body]]}

# ── the composer stays live mid-turn: typing STEERS ─────────────────
#
# USER: "can't steer from desktop during agent turns because Send is
# inactive and enter doesn't work. Also applies to other slash
# commands apparently." Both claims, pinned: plain text posts to the
# steer endpoint instead of starting a turn, and slash commands run.
puts "\nsteering from a live composer"

namespace eval ::vapi {
    variable steer_posts {}
    variable chat_calls {}
    # json_string is the real api.tcl's escape hatch for free-text
    # values; the test stubs it faithfully (tagged pre-rendered JSON).
    proc json_string {s} { return [list ::vapi::__json__ "\"[::vapi::json_escape $s]\""] }
    proc json_escape {s} { return [string map [list \\ \\\\ \" \\\" \n \\n] $s] }
    proc post_json {path body} {
        variable steer_posts
        # A cancel posted here would explode the earlier stop test's
        # counting; cancels are not this block's concern.
        if {[string match "*/cancel" $path]} { return [dict create] }
        lappend steer_posts [list $path $body]
        return {}
    }
    proc chat_sse {sid txt {att {}}} {
        variable chat_calls
        lappend chat_calls $sid
        return fake_tok
    }
}
reset
set ::vchat::session "S"
::vchat::_mark_running "S" 1
.mid.chatwrap.chat.input insert end "use Tcl instead"
::vchat::send_current
ok "busy typing posts to the steer endpoint" \
    {[llength $::vapi::steer_posts] == 1
     && [string match "*/steer" [lindex $::vapi::steer_posts 0 0]]}
ok "…with the text as the message" \
    {[string match "*use Tcl instead*" [lindex $::vapi::steer_posts 0 1]]}
ok "…and starts NO new turn" {$::vapi::chat_calls eq {}}
ok "…and the transcript says it was queued" \
    {[string match "*steer queued*" [body]]}
.mid.chatwrap.chat.input delete 1.0 end

# Slash commands keep working mid-turn — they never touched the stream.
::vchat::_mark_running "S" 1
.mid.chatwrap.chat.input insert end "/status"
proc ::vapi::get_json {path} {
    if {[string match "*/history" $path]} {
        return [dict create messages {}]
    }
    return [dict create status ok uptime_s 1 session_count 0 \
        platforms {local} version test default_provider p default_model m]
}
::vchat::send_current
ok "a slash command runs mid-turn" {[string match "*status: ok*" [body]]}
.mid.chatwrap.chat.input delete 1.0 end
::vchat::_mark_running "S" 0

# `/secret set NAME <value>` drops the VALUE from the input history (which
# outlives the session): only the command part may survive there.
puts "\n/secret set keeps only its command part in the input history"
namespace eval ::vedit {
    variable hist {}
    proc hist_push {t} { lappend ::vedit::hist $t }
}
reset
set ::vchat::session "S"
set ::vedit::hist {}
.mid.chatwrap.chat.input insert end "/secret set ALI_KEY abc123secretvalue"
::vchat::send_current
ok "the value is not pushed to the input history" \
    {[llength $::vedit::hist] == 1
     && [lindex $::vedit::hist 0] eq "/secret set ALI_KEY"
     && ![string match "*abc123secretvalue*" [lindex $::vedit::hist 0]]}
.mid.chatwrap.chat.input delete 1.0 end
set ::vedit::hist {}
.mid.chatwrap.chat.input insert end "an ordinary message"
proc ::vapi::chat {sid} { return fake_tok }
::vchat::send_current
ok "an ordinary message IS pushed whole" \
    {[llength $::vedit::hist] == 1
     && [lindex $::vedit::hist 0] eq "an ordinary message"}
.mid.chatwrap.chat.input delete 1.0 end
namespace eval ::vedit { proc hist_push {t} {} }

# ── historical tool rounds keep their invocation ────────────────────
#
# USER: "terminal doesn't show the command ran? … not available in
# historical calls". Live shows "⚙ name args"; the replayed session
# must too — the assistant row's tool_calls are the source.
puts "\nhistorical tool calls show the invocation"

set ::tool_rounds_seen {}
proc ::vchat::emit_message {kind text {reason ""} {tools {}} {at now}} {
    if {$kind eq "tool"} { lappend ::tool_rounds_seen $tools }
    # `[::vchat::log]` for the same reason as the stub above, and this
    # is the one in force for the rest of the file.
    set w [::vchat::log]
    incr ::paints
    $w configure -state normal
    $w insert end "$kind: $text\n"
    # THE REASON, like the stub above — which warns in so many words
    # that "a stub that drops the argument eats the reasoning entirely",
    # and then this one dropped it. A turn reconciles one message per
    # reasoning SEGMENT (R7), so a suite that cannot see the reason
    # cannot tell a turn that kept every round from one that kept the
    # last, which is the exact defect this file now pins.
    if {$reason ne ""} {
        $w insert end "reason: $reason\n"
    }
    $w configure -state disabled
}
proc ::vapi::get_json {path} {
    if {[string match "*/history" $path]} {
        return [dict create messages [list \
            [dict create role user content "run it"] \
            [dict create role assistant content "" tool_calls [list \
                [dict create id c1 function [dict create name terminal \
                    arguments "{{\"command\":\"id\"}}"]]]] \
            [dict create role tool tool_name terminal tool_call_id c1 \
                content "uid=1000"]]]
    }
    return [dict create]
}
reset
::vchat::open_session "TOOL"
ok "a historical tool round keeps its name" \
    {[string match "*terminal*" $::tool_rounds_seen]}
ok "…and its invocation args" \
    {[string match "*command*id*" $::tool_rounds_seen]}

# ── the live stream breaks lines between kinds ──────────────────────
#
# USER: "Missing newlines between end of reasoning block, tool call or
# output when watching live, appear when watching older transcript".
puts "\nlive kind transitions insert line breaks"

reset
set ::vchat::session "L"
set ::vchat::stream_session "L"
::vchat::on_reasoning "thinking hard"
::vchat::on_delta "the answer"
# The live reasoning now carries a "▼ reasoning (streaming…)" header above
# its body (so hiding it collapses to a clickable ▶ instead of vanishing),
# so match the TAIL: the point is the newline between the reasoning body
# and the delta, not the absence of a header.
ok "reasoning → delta gets a line break" \
    {[string match "*thinking hard\nthe answer" [body]]}
::vchat::on_tool start terminal {{args} {{command} ls}}
ok "delta → tool gets a line break too" \
    {[string match "*the answer\n*terminal*" [body]]}

# ── the batch envelope: one round, header AND result ─────────────────
#
# USER 2026-09-20: "'Batch' tool calls no longer seem visible in the
# transcript/window on desktop, only results are shown" — and, on a
# settled turn, "ONLY the literal 'batch' tool name, not the
# arguments". The engine emits one ToolStart (envelope, args = the
# model's invocations JSON) and one ToolEnd per batch; the live paint
# must render BOTH halves of the round exactly as a reloaded one does,
# not a bare result collapsible with no ⚙ line above it.
puts "\nthe batch envelope paints as a whole round"
reset
set ::vchat::session "A"
::vchat::_mark_running "A" 1
dict set ::vchat::turn_anchor A [.mid.chatwrap.chat.log index end-1c]
set ::vchat::stream_session "A"
set ::vchat::tool_args(A) ""
::vchat::on_tool start batch [dict create name batch \
    args {{\"invocations\":[{\"name\":\"read_file\",\"arguments\":{\"path\":\"one.txt\"}}]}}]
ok "the ⚙ header with the invocation is painted at start" \
    {[string match "*⚙ batch*invocations*read_file*" [body]]}
ok "…and no result is shown yet" {![string match "*result (*" [body]]}
::vchat::on_tool end batch [dict create name batch result "batch: 1/1 invocations ran"]
set ::vchat::stream_session ""
ok "the result collapsible lands under it" \
    {[string match "*⚙ batch*invocations*\n*result (*" [body]]}
::vchat::_mark_running "A" 0
catch { unset ::vchat::tool_args(A) }

# ── one thought, one block, across a foreign line ───────────────────
#
# USER 2026-09-20: "the reasoning gets split into two blocks when it is
# clearly part of the same stream". A construct painted between two
# bursts of ONE thought used to clear the region tags, so the thought
# reopened under a second "reasoning (streaming)" header.
puts "\none thought stays one block across a foreign line"
reset
set ::vchat::session "A"
::vchat::_mark_running "A" 1
dict set ::vchat::turn_anchor A [.mid.chatwrap.chat.log index end-1c]
set ::vchat::stream_session "A"
::vchat::on_reasoning "first half of the thought"
# A foreign line: a background completion arriving mid-thought.
::vchat::_log_completion "A" "\[BACKGROUND SHELL COMPLETE\] tail -f done"
::vchat::on_reasoning "second half of the same thought"
::vchat::_flush_live_reasoning "A"
set ::vchat::stream_session ""
update
ok "the thought has ONE region header" \
    {[regexp -all {reasoning \(streaming} [body]] == 1}
ok "…and both halves landed" \
    {[string match "*first half of the thought*" [body]]
     && [string match "*second half of the same thought*" [body]]}
ok "…with the foreign line between them, not welded" \
    {[string match "*first half of the thought*BACKGROUND SHELL COMPLETE*second half*" [body]]}
::vchat::_mark_running "A" 0

# ── follow survives consecutive tool rounds ─────────────────────────
#
# USER 2026-09-20: "the follow sometimes unsticks again, most likely
# after tool calls". The start arm's weld-guard splice was the one
# transcript-growing insert outside a prestash/follow bracket: it moved
# the view one line, the next painter's prestash read "not at the
# bottom", and follow disarmed exactly after a tool round.
puts "\nfollow stays armed across tool rounds"
reset
set ::vchat::session "A"
set ::vchat::follow 1
::vchat::_mark_running "A" 1
dict set ::vchat::turn_anchor A [.mid.chatwrap.chat.log index end-1c]
set ::vchat::stream_session "A"
# Enough content that the transcript actually overflows the 260px
# window — yview fractions on a fully-visible transcript are degenerate.
for {set i 0} {$i < 40} {incr i} {
    ::vchat::on_delta "filler line $i of many to overflow the window\n"
}
::vchat::_flush_live_delta "A" 1
update
set edge1 [expr {[lindex [.mid.chatwrap.chat.log yview] 1] >= 0.999}]
ok "the view is at the live edge before the rounds" {$edge1}
foreach r {one two three} {
    ::vchat::on_reasoning "thinking about round $r"
    ::vchat::on_tool start terminal [dict create args "{{\"cmd\":\"$r\"}}"]
    ::vchat::on_tool end terminal [dict create result "round $r done"]
}
update
ok "…and still at the live edge after three rounds" \
    {[lindex [.mid.chatwrap.chat.log yview] 1] >= 0.999}
ok "no follow button appeared" {![winfo exists .mid.chatwrap.chat.follow]}
::vchat::_mark_running "A" 0


# ── the render cache: paint local, verify later ─────────────────────
#
# USER: "It takes long and a lot of cpu time to re-render a session
# when switching". A cached session must not re-fetch on the second
# open.
puts "\nthe history render from cache on the second open"

set ::fetches 0
proc ::vapi::get_json {path} {
    if {[string match "*/history" $path]} { incr ::fetches }
    if {[string match "*/history" $path]} {
        return [dict create messages [list [dict create role user content "cached turn"]]]
    }
    return [dict create]
}
proc ::vapi::get_json_async {path cmd {ms 0}} { return fake_async }
reset
::vchat::open_session "CACHE1"
ok "the first open fetched once" {$::fetches == 1}
::vchat::open_session "OTHER"
set before_revisit $::fetches
::vchat::open_session "CACHE1"
ok "the second open painted from cache (no new fetch)" \
    {$::fetches == $before_revisit}
ok "…and still shows the session" {[string match "*cached turn*" [body]]}

# ── the switch says what it cost ────────────────────────────────────
#
# USER 2026-09-16: "the whole transcript seems to reload/refresh on
# reopen too, not sure if that's intended, might be intended."
#
# It is intended — in ONE of the two cases. A switch back to a session
# this window has shown before repaints nothing at all: the widget is
# kept, with its scroll position, its expanded blocks and its decoded
# images. A first open, or a return after the LRU evicted it, pays a
# full paint. From the outside those are indistinguishable, so the one
# question the whole design answers had no answer on screen — and a
# design nobody can see is, reasonably, mistaken for a bug.
puts "\nthe status line says whether the transcript was kept or rebuilt"
proc ::vapi::get_json {path} {
    if {[string match "*/history" $path]} {
        return [dict create messages [list [dict create role user content "hello there"]]]
    }
    return [dict create]
}
reset
::vchat::open_session "K1"
ok "a first open says it loaded" {[string match "*loaded*" $::vstatus::last]}
ok "…and not that it kept anything" {![string match "*kept*" $::vstatus::last]}

::vchat::open_session "K2"
::vchat::open_session "K1"
ok "coming back to a live widget says kept" \
    {[string match "*kept*" $::vstatus::last]}
ok "…and never claims to have loaded it again" \
    {![string match "*loaded*" $::vstatus::last]}
ok "…while the transcript is in fact still there" \
    {[string match "*hello there*" [body]]}

# ── windowed painting: giant sessions open fast ─────────────────────
#
# USER: "Long transcripts render at 100% CPU". Only the newest
# PAINT_LIMIT rows paint on open; the rest wait behind a header line
# that grows the window from the CACHE (no refetch).
puts "\nwindowed painting"

set big {}
for {set i 1} {$i <= 1000} {incr i} {
    lappend big [dict create role user content "row $i"]
}
proc ::vapi::get_json {path} {
    global big
    if {[string match "*/history" $path]} {
        return [dict create messages $big]
    }
    return [dict create]
}
set ::vchat::paint_limit 50
reset
set t0 [clock milliseconds]
::vchat::open_session "BIG"
set ms [expr {[clock milliseconds] - $t0}]
ok "the header line offers the hidden rows" \
    {[string match "*950 older messages*" [body]]}
# Anchored patterns — "row 100" is a PREFIX of "row 1000" and a bare
# glob matches both. (The stubbed emit_message renders "user: row N".)
ok "the newest row is painted" {[string match "*user: row 1000\n*" [body]]}
ok "the oldest window row is painted" {[string match "*user: row 951\n*" [body]]}
ok "…and anything older is NOT" {![string match "*user: row 500\n*" [body]]}
::vchat::_paint_more "BIG"
# Window grows by ONE limit: 901..1000 now, header 900.
ok "load-more paints the previous window from cache" \
    {[string match "*user: row 901\n*" [body]]
     && ![string match "*user: row 500\n*" [body]]
     && [string match "*900 older*" [body]]}

# GROWING THE WINDOW MID-TURN. Both anchors are absolute line.char
# indices, and _paint_more clears the transcript and repaints a bigger
# window under them — after which turn_anchor names a line in the middle
# of somebody else's message and on_done's `delete $anchor end` eats the
# transcript from there down. An anchor that no longer describes this
# transcript must go: "no anchor" already means APPEND, which is the
# safe reading.
::vchat::_mark_running "BIG" 1
dict set ::vchat::turn_anchor BIG [.mid.chatwrap.chat.log index end-1c]
dict set ::vchat::turn_reload_anchor BIG [.mid.chatwrap.chat.log index end-1c]
::vchat::_paint_more "BIG"
ok "growing the window drops the reconcile anchor it invalidated" \
    {![dict exists $::vchat::turn_anchor BIG]
     && ![dict exists $::vchat::turn_reload_anchor BIG]}
set ::vchat::session "BIG"
set ::vchat::stream_session "BIG"
::vchat::on_done [dict create text "the answer" provider p model m \
    prompt_tokens 1 completion_tokens 1 display_reasoning "" notice ""]
set ::vchat::stream_session ""
ok "…so the reconcile appends instead of eating the rows above it" \
    {[string match "*the answer*" [body]] && [string match "*user: row 1000\n*" [body]]}
set ::vchat::paint_limit 400

# ── the session event stream: ONE path for live and resume ──────────
#
# These drive on_session_event — the router the persistent stream feeds —
# directly, the way irctest drives ::virc::on_event. They pin the cutover:
# a replayed row batches through the SAME reload ladder as a full open,
# the cursor advances, a desktop turn's result reconciles through the very
# on_done the SSE path used (so live == reload), and a turn started by
# another surface is adopted without a result of its own.
puts "\nsession event stream"

reset
set ::vchat::session "A"
# A replayed row is buffered, then painted as ONE batch at the live
# boundary through the reload ladder (so tool-call args correlate exactly
# as they do on a full open — a row-at-a-time paint would lose them).
::vchat::on_session_event "A" history [dict create role user content "replayed question"]
ok "a replayed history row is buffered, not painted mid-stream" \
    {![string match "*replayed*" [body]]}
::vchat::on_session_event "A" session-live [dict create last_msg_id 42]
ok "…and painted at the live boundary" {[string match "*replayed question*" [body]]}
ok "the live boundary adopts the engine's resume cursor" {$::vchat::ev_cursor(A) == 42}

# turn-started adopts the turn; live deltas route to the transcript.
::vchat::on_session_event "A" turn-started [dict create surface desktop started_at x base_msg_id 42]
ok "turn-started marks the session running" {[::vchat::is_running "A"]}
::vchat::on_session_event "A" delta [dict create text "live answer"]
ok "a live delta paints into the session on screen" {[string match "*live answer*" [body]]}

# turn-ended fires BEFORE the verdict (run_turn's guard drops before it
# publishes turn-result), so it must NOT clear running or reconcile —
# that is turn-result's job, via the SAME on_done the SSE path used.
::vchat::on_session_event "A" turn-ended {}
ok "turn-ended leaves the reconcile to the verdict that follows" \
    {[::vchat::is_running "A"]}
::vchat::on_session_event "A" turn-result [dict create text "final answer" provider p model m \
    prompt_tokens 1 completion_tokens 2 display_reasoning "" notice "" last_msg_id 43]
ok "turn-result reconciles and clears running" {![::vchat::is_running "A"]}
ok "…replacing the raw stream with the finished reply" \
    {[string match "*final answer*" [body]] && ![string match "*live answer*" [body]]}
ok "…and advances the cursor to the store head" {$::vchat::ev_cursor(A) == 43}

# The verdict comes from run_turn itself now, for EVERY surface — a
# desktop window watching an IRC/cron/telegram turn reconciles it the
# same way, instead of seeing the deltas and then nothing.
reset
set ::vchat::session "A"
set ::vchat::hist_cache [dict create A {messages {} last_msg_id 3}]
::vchat::on_session_event "A" turn-started [dict create surface irc started_at x base_msg_id 3]
::vchat::on_session_event "A" delta [dict create text "irc is typing"]
::vchat::on_session_event "A" turn-ended {}
ok "turn-ended does not wedge a non-desktop turn either (the verdict is next)" \
    {[::vchat::is_running "A"]}
::vchat::on_session_event "A" turn-result [dict create text "the irc answer" provider p model m \
    prompt_tokens 1 completion_tokens 1 display_reasoning "" notice "" last_msg_id 4]
ok "an irc-started turn reconciles on the watcher's screen" \
    {[string match "*the irc answer*" [body]] && ![string match "*irc is typing*" [body]]}
ok "…clears running and heals the cache like a desktop turn" \
    {![::vchat::is_running "A"] && ![dict exists $::vchat::hist_cache A]}

# A SECOND window (or one watching an IRC/cron turn) did not paint the
# user's question — the window that sent it did, optimistically, and the
# persisted copy sits at/below the cursor so no replay re-sends it.
# turn-started carries the prompt so the watcher shows the question above
# the reply that is about to stream in. The starter already anchored its
# optimistic copy, so it must NOT paint the prompt a second time.
reset
set ::vchat::session "A"
::vchat::on_session_event "A" turn-started \
    [dict create surface irc started_at x base_msg_id 7 prompt "watched question"]
ok "a watching window paints the prompt turn-started carries" \
    {[string match "*watched question*" [body]]}
reset
set ::vchat::session "A"
dict set ::vchat::turn_reload_anchor A [.mid.chatwrap.chat.log index end-1c]
::vchat::emit_message user "starter question"
::vchat::on_session_event "A" turn-started \
    [dict create surface desktop started_at x base_msg_id 7 prompt "starter question"]
ok "the starting window does not paint the prompt twice" \
    {[regexp -all {starter question} [body]] == 1}

# …and the anchor is not always there to tell us. A send path that never
# set `turn_reload_anchor` (the custom-command renderer used to be one:
# it emitted the user row and posted, skipping the anchor), or a window
# opened onto a session mid-send, arrives at turn-started with the
# optimistic copy already on screen and no anchor to prove it. Keying
# the emit on the anchor alone doubled the prompt in exactly that case,
# so the arm ALSO checks whether this exact text is already the tail of
# the transcript.
reset
set ::vchat::session "A"
::vchat::emit_message user "already on screen"
::vchat::on_session_event "A" turn-started \
    [dict create surface desktop started_at x base_msg_id 9 prompt "already on screen"]
ok "an unanchored turn-started does not double a prompt already on screen" \
    {[regexp -all {already on screen} [body]] == 1}
# …while a genuinely new question still paints: the dedupe is about
# THIS text being present, not about some turn having started.
reset
set ::vchat::session "A"
::vchat::emit_message user "the previous question"
::vchat::on_session_event "A" turn-started \
    [dict create surface desktop started_at x base_msg_id 10 prompt "a brand new question"]
ok "…and a different prompt is still painted" \
    {[string match "*a brand new question*" [body]] \
        && [regexp -all {the previous question} [body]] == 1}
# The glob-character case: `string match "*$prompt*"` reads the prompt's
# own metacharacters as PATTERN syntax, so a prompt containing `*` would
# match text that is NOT the prompt — suppressing a real question. Here
# the screen holds "a x b" and the incoming prompt is literally "a * b":
# the pattern `*a * b*` matches the screen text, so a `string match`
# dedupe would wrongly call this a duplicate and paint nothing.
reset
set ::vchat::session "A"
::vchat::emit_message user "a x b"
::vchat::on_session_event "A" turn-started \
    [dict create surface desktop started_at x base_msg_id 11 prompt "a * b"]
ok "a prompt's glob characters are not treated as a pattern" \
    {[string first "a * b" [body]] >= 0 \
        && [string first "a x b" [body]] >= 0}

# A lagged stream refetches from the cursor immediately rather than going
# quiet (the engine dropped events; the store still has them).
reset
set ::vchat::session "A"
set ::vchat::ev_cursor(A) 42
set ::vapi::ev_opens {}
::vchat::on_session_event "A" session-lagged [dict create missed 7]
ok "session-lagged reconnects from the cursor (refetch, never silence)" \
    {[llength $::vapi::ev_opens] == 1 && [lindex $::vapi::ev_opens 0] eq {A 42 0}}

# A mid-turn reconnect must NOT double the transcript. The resume cursor is
# pre-turn, so the replay re-sends the persisted user row; the reconnect
# clears the stale live region (optimistic user + streamed partial) first
# and the replay + snapshot restore ONE copy from the store. This is the
# "gateway blipped mid-turn" case.
reset
set ::vchat::session "A"
set _w .mid.chatwrap.chat.log
dict set ::vchat::turn_reload_anchor A [$_w index end-1c]
::vchat::emit_message user "the question"
dict set ::vchat::turn_anchor A [$_w index end-1c]
::vchat::on_session_event "A" delta [dict create text "partial ans"]
::vchat::_mark_running "A" 1
ok "before the drop the optimistic user and the partial are on screen" \
    {[string match "*the question*" [body]] && [string match "*partial ans*" [body]]}
set ::vchat::ev_reconnect_pending(A) 1
::vchat::on_session_event "A" history [dict create role user content "the question"]
::vchat::on_session_event "A" turn-snapshot \
    [dict create surface desktop content "partial ans" reasoning "" tools {} base_msg_id 0]
ok "a mid-turn reconnect repaints ONE user message, not two" \
    {[regexp -all {the question} [body]] == 1}
ok "…and ONE partial, restored from the snapshot" \
    {[regexp -all {partial ans} [body]] == 1}

# A SECOND drop in the SAME turn. The first reconnect's session-live
# advanced ev_cursor to the store HEAD, so the replay no longer carries
# the user's question — but turn_reload_anchor still points at where the
# transcript stood BEFORE it, so the delete takes the question and
# nothing brings it back. The delete boundary and the replay boundary
# stopped describing the same place.
#
# USER 2026-09-18: "my own second prompt got swallowed when I told
# session … to continue after a network error … the vanished prompt does
# not seem to appear even in a settled/not-live transcript".
::vchat::on_session_event "A" session-live [dict create last_msg_id 9]
set ::vchat::ev_reconnect_pending(A) 1
::vchat::on_session_event "A" turn-snapshot \
    [dict create surface desktop content "partial two" reasoning "" tools {} base_msg_id 9]
ok "a second mid-turn reconnect still keeps the user's question" \
    {[regexp -all {the question} [body]] == 1}

# The same, with a persisted ROUND between the question and the drop:
# everything at or below the cursor has to survive, because nothing is
# going to re-send it.
reset
set ::vchat::session "A"
set _w .mid.chatwrap.chat.log
dict set ::vchat::turn_reload_anchor A [$_w index end-1c]
::vchat::emit_message user "the question"
dict set ::vchat::turn_anchor A [$_w index end-1c]
::vchat::_mark_running "A" 1
set ::vchat::ev_reconnect_pending(A) 1
::vchat::on_session_event "A" history [dict create role user content "the question"]
::vchat::on_session_event "A" history \
    [dict create role tool tool_name ripgrep tool_call_id c1 content "ripgrep found 3 matches"]
::vchat::on_session_event "A" session-live [dict create last_msg_id 9]
ok "the first reconnect restores the question and the finished round" \
    {[regexp -all {the question} [body]] == 1 && [regexp -all {ripgrep} [body]] == 1}
set ::vchat::ev_reconnect_pending(A) 1
::vchat::on_session_event "A" turn-snapshot \
    [dict create surface desktop content "partial two" reasoning "" tools {} base_msg_id 9]
ok "a second drop keeps both — the replay starts after them now" \
    {[regexp -all {the question} [body]] == 1 && [regexp -all {ripgrep} [body]] == 1}


# …and the turn's OWN reconcile must not eat what the replay restored.
# `_flush_replay` said in a comment that it dropped the narrow on_done
# anchor after the delete, and did not: with no `variable turn_anchor` in
# the proc the name was a LOCAL, the unset threw, and the catch swallowed
# it. The anchor survived every reconnect still measured against the
# transcript from BEFORE the drop, so turn-result cut the repainted one
# at the wrong line and the replay's rows went down with the live region.
reset
set ::vchat::session "A"
set _w .mid.chatwrap.chat.log
dict set ::vchat::turn_reload_anchor A [$_w index end-1c]
::vchat::emit_message user "the question"
dict set ::vchat::turn_anchor A [$_w index end-1c]
::vchat::on_session_event "A" delta [dict create text "partial ans"]
::vchat::_mark_running "A" 1
set ::vchat::ev_reconnect_pending(A) 1
::vchat::on_session_event "A" history [dict create role user content "the question"]
::vchat::on_session_event "A" history \
    [dict create role tool tool_name ripgrep tool_call_id c1 content "ripgrep found 3 matches"]
::vchat::on_session_event "A" turn-snapshot \
    [dict create surface desktop content "partial ans" reasoning "" tools {} base_msg_id 0]
::vchat::on_session_event "A" turn-result [dict create text "final answer" \
    provider p model m prompt_tokens 1 completion_tokens 1 \
    display_reasoning "" notice "" last_msg_id 9]
ok "the reconcile replaces the live region a reconnect rebuilt" \
    {[string match "*final answer*" [body]] && ![string match "*partial ans*" [body]]}
ok "…and not the rows the replay had just restored above it" \
    {[regexp -all {the question} [body]] == 1 && [regexp -all {ripgrep} [body]] == 1}

# A reconnect for a session you are NOT looking at. _flush_replay dropped
# the rows outright (`if {$session ne $sid} { return }`) while
# session-live advanced that session's cursor anyway — so they were
# painted nowhere and nothing would ever re-send them. Every loaded
# session owns a transcript; the rows belong in ITS widget, which is the
# rule for_display retired everywhere else in this file.
reset
set ::vchat::session "B"
::vchat::_claim_log "A"
::vchat::_claim_log "B"
::vchat::on_session_event "A" history [dict create role user content "asked while away"]
::vchat::on_session_event "A" session-live [dict create last_msg_id 12]
ok "a replay for an off-screen session lands in ITS transcript" \
    {[string match "*asked while away*" [body_of A]]}
ok "…and not in the one you are looking at" \
    {![string match "*asked while away*" [body_of B]]}

# A turn that DIED during a drop (the gateway restarted; the new process
# never knew about it) must not leave the client wedged on "running". The
# reconnect hands back NO snapshot, so at the live boundary the client
# reports the turn did not survive and clears the flag — the plan's
# "restart the gateway mid-turn" verification, asserted headlessly.
# A reconnect is the ONLY path allowed to print that note: the fresh-open
# path (below) has the engine answer instead, so the correct
# completed-while-away state can no longer be labelled a crash.
reset
set ::vchat::session "A"
::vchat::_mark_running "A" 1
dict set ::vchat::turn_reload_anchor A [.mid.chatwrap.chat.log index end-1c]
set ::vchat::ev_reconnect_pending(A) 1
set ::vchat::ev_reconnected(A) 1
::vchat::on_session_event "A" session-live [dict create last_msg_id 5]
ok "a turn that died during the drop stops being marked running" \
    {![::vchat::is_running "A"]}
ok "…and the client says so instead of sitting on a stale 'thinking…'" \
    {[string match "*did not survive*" [body]]}

# The died-turn path must also heal the render cache: no turn-result will
# ever announce what the dead turn persisted, and a stale cache is exactly
# how "reopened and the transcript was empty/yesterday's" happens.
reset
set ::vchat::session "A"
set ::vchat::hist_cache [dict create A {messages {} last_msg_id 3}]
::vchat::_mark_running "A" 1
set ::vchat::ev_reconnected(A) 1
::vchat::on_session_event "A" session-live [dict create last_msg_id 5]
ok "a turn that died drops the stale cache (the next open fetches the truth)" \
    {![dict exists $::vchat::hist_cache A]}

# FRESH opens must never print the died-during-drop note. The mark can be
# stale from a switch-away while the turn COMPLETED on the engine (the
# user's "the gateway is still up and my turn succeeded" report): the
# engine's own answer is the arbiter.
reset
set ::vchat::session "A"
::vchat::_mark_running "A" 1
proc ::vapi::get_json {path} {
    if {[string match "*/context" $path]} { return [dict create busy 0] }
    return [dict create]
}
::vchat::on_session_event "A" session-live [dict create last_msg_id 5]
ok "a fresh open after a completed turn prints NO survival note" \
    {![string match "*did not survive*" [body]]}
ok "…and clears the stale running mark" \
    {![::vchat::is_running "A"]}

# Completion is the operant fact for a fresh open, not pickiness in the
# fetch — a context call that fails must not reinstate the note.
reset
set ::vchat::session "A"
::vchat::_mark_running "A" 1
proc ::vapi::get_json {path} { error "unreachable" }
::vchat::on_session_event "A" session-live [dict create last_msg_id 5]
ok "a failed fresh-open fetch prints NO survival note" \
    {![string match "*did not survive*" [body]]}
ok "…and clears the stale running mark" \
    {![::vchat::is_running "A"]}

# Engine-confirmed live turn: the fresh open must re-handshake so a
# snapshot re-adopts the turn instead of transplanting running state the
# caller's heart never reasserted.
#
# In the real app the stream IS open for this sid when the switch-back
# happens (open_session connected it), so ev_sid is set here too. The
# re-handshake is a direct _events_reconnect: _events_connect closes the
# old stream itself, so the open is re-recorded by the fake api, and the
# reconnect stamp survives to the next session-live.
reset
set ::vchat::session "A"
::vchat::_mark_running "A" 1
proc ::vapi::get_json {path} {
    if {[string match "*/context" $path]} { return [dict create busy 1] }
    return [dict create]
}
set ::vapi::ev_opens {}
# The liveness question is asynchronous (a synchronous GET inside a
# stream handler was the 2026-10-04 hang): record it, then deliver the
# reply the way the event loop would, after the handler has returned.
set ::async_calls {}
proc ::vapi::get_json_async {path cmd {ms 0}} {
    lappend ::async_calls [list $path $cmd]
    return fake_async
}
::vchat::on_session_event "A" session-live [dict create last_msg_id 2]
ok "the handler asks the engine and does not wait for the answer" \
    {[llength $::async_calls] == 1 && [llength $::vapi::ev_opens] == 0}
foreach c $::async_calls {
    lassign $c p cmd
    uplevel #0 [list {*}$cmd 1 [::vapi::get_json $p]]
}
proc ::vapi::get_json_async {path cmd {ms 0}} { return fake_async }
ok "a fresh open facing a LIVE turn re-handshakes the session stream" \
    {[llength $::vapi::ev_opens] == 1 && [lindex $::vapi::ev_opens 0] eq {A 2 0}}
ok "…and that reopen is stamped a reconnect" {$::vchat::ev_reconnected(A) == 1}
ok "…without printing any survival note" \
    {![string match "*did not survive*" [body]]}

# A FATAL stream failure (404 = an engine that predates the events
# endpoint) must self-diagnose IN THE TRANSCRIPT. The version-skew evening
# this comes from: every send silently timed out against an SSE engine and
# the only complaint was one status-bar message nobody saw.
reset
set ::vchat::session "A"
::vchat::on_session_event "A" closed [dict create status error code 404 url http://x]
ok "a fatal stream failure says so in the transcript, not just the status bar" \
    {[string match "*404*" [body]]}

# ── recovering approvals after a reload ─────────────────────────────
#
# An ask now OUTLIVES the request that raised it: the engine parks it in
# the registry and rides the session bus, so a closed window no longer
# auto-denies it. GET /api/asks existed for exactly this recovery and had
# ZERO callers; _redraw_asks is the callback open_session now arms. These
# drive it directly, the way the suite drives every other handler.
puts "\nredraw open approvals on open"

namespace eval ::vask {
    variable shown {}
    proc show {d} { lappend ::vask::shown $d }
}
reset
set ::vchat::session "A"
set ::vask::shown {}
# Two open asks, one belonging to another session: only A's must surface.
::vchat::_redraw_asks "A" "" 1 [dict create asks [list \
    [dict create id askB session_id B kind approval prompt "B's question"] \
    [dict create id askA session_id A kind approval prompt "A's question"]]]
ok "the pending ask for THIS session is redrawn" \
    {[llength $::vask::shown] == 1
     && [dict get [lindex $::vask::shown 0] id] eq "askA"}

set ::vask::shown {}
::vchat::_redraw_asks "A" "" 1 [dict create asks [list \
    [dict create id askB session_id B kind approval prompt "B only"]]]
ok "an ask for ANOTHER session is not shown" {[llength $::vask::shown] == 0}

set ::vask::shown {}
::vchat::_redraw_asks "A" "" 0 "connection refused"
ok "a failed fetch redraws nothing" {[llength $::vask::shown] == 0}

# The user navigated away before the reply landed: the session on screen
# wins and the stale reply is dropped. (The epoch check in get_json_async
# covers a re-attach; this covers a plain switch within one attach.)
set ::vask::shown {}
set ::vchat::session "Z"
::vchat::_redraw_asks "A" "" 1 [dict create asks [list \
    [dict create id askA session_id A kind approval prompt "A's question"]]]
ok "a reply for a session no longer on screen is dropped" \
    {[llength $::vask::shown] == 0}

# Restore the no-op stub for anything that follows.
namespace eval ::vask { proc show {d} {} }

# A dialog whose ask-resolved was dropped (the reconnect's replay carries
# store rows, not tap-only events) is closed by the SAME re-fetch: the ask
# is gone from /api/asks, so _redraw_asks resolves the one on screen.
puts "\na dialog whose ask was answered during the drop closes on the re-fetch"
namespace eval ::vask {
    variable resolved_calls {}
    proc resolved {d} { lappend ::vask::resolved_calls $d }
}
reset
set ::vchat::session "A"
set ::vask::current "ask-gone"
set ::vask::current_sid "A"
set ::vask::resolved_calls {}
::vchat::_redraw_asks "A" "ask-gone" 1 [dict create asks [list]]
ok "no parked ask → the open dialog is resolved away" \
    {[llength $::vask::resolved_calls] == 1
     && [dict get [lindex $::vask::resolved_calls 0] id] eq "ask-gone"}

set ::vask::resolved_calls {}
::vchat::_redraw_asks "A" "ask-gone" 1 [dict create asks [list \
    [dict create id ask-gone session_id A kind approval prompt "still open"]]]
ok "an ask still parked keeps its dialog (no spurious resolve)" \
    {[llength $::vask::resolved_calls] == 0}

set ::vask::resolved_calls {}
set ::vask::current "ask-other-session"
set ::vask::current_sid "B"
::vchat::_redraw_asks "A" "ask-other-session" 1 [dict create asks [list]]
ok "a dialog for ANOTHER session is left alone here" \
    {[llength $::vask::resolved_calls] == 0}

# The reply is a snapshot from when the request went out. A question
# that arrived live after that (its dialog up now, none then) is not in
# it — and must not be closed by its "nothing parked".
set ::vask::resolved_calls {}
set ::vask::current "ask-new"
set ::vask::current_sid "A"
::vchat::_redraw_asks "A" "" 1 [dict create asks [list]]
ok "a reply older than the dialog on screen is dropped" \
    {[llength $::vask::resolved_calls] == 0}
namespace eval ::vask { proc resolved {d} {} }
set ::vask::current ""
set ::vask::current_sid ""

# ── background shells: completion arrives the moment it happens ─────
#
# The engine publishes `shell-complete` on the session bus at exit, so
# the desktop shows it immediately — not at the next turn boundary.
reset
set ::vchat::session "A"
::vchat::on_session_event "A" shell-complete \
    [dict create id 7 command "make build" exit_code 0]
ok "a finished shell is announced in the on-screen session" \
    {[string match "*✓ shell 7 finished: make build*" [body]]}
reset
set ::vchat::session "A"
::vchat::on_session_event "A" shell-complete \
    [dict create id 9 command "make test" exit_code 2]
ok "a failed shell is announced with its exit code" \
    {[string match "*⚠ shell 9 finished (exit 2): make test*" [body]]}
reset
set ::vchat::session "B"
::vchat::on_session_event "A" shell-complete \
    [dict create id 7 command "make build" exit_code 0]
ok "a shell for a session NOT on screen does not paint into the current view" \
    {![string match "*shell 7*" [body]]}

# ── switching keeps the transcript, it does not rebuild it ───────────
#
# USER: "switching between sessions in desktop sucks again. Maybe they
# should never get unloaded, just hidden."
#
# The render cache that preceded this removed the HTTP round trip and
# left the RENDER, which is the expensive half — and the switch still
# threw away scroll position, expanded reasoning blocks and every
# decoded image along with the text. Nothing survived a switch because
# there was one widget and the switch wiped it.
puts "\nswitching hides, it does not unload"

# A history with real rows in it, so the paint counter below means
# something: against the empty-dict stub that precedes this block, "no
# messages were repainted" is true of every implementation, including
# the one that wipes the widget.
proc ::vapi::get_json {path} {
    if {[string match "*/history" $path]} {
        return [dict create last_msg_id 4 messages [list \
            [dict create role user content "first question"] \
            [dict create role assistant content "first answer"]]]
    }
    if {[string match "*/context" $path]} { return [dict create busy 0] }
    return [dict create]
}

reset
::vchat::open_session "A"
ok "a first open paints the session's history" {$::paints > 0}
set a_w [::vchat::log]
set a_text [body]
::vchat::open_session "B"
ok "B is a different transcript than A" {[::vchat::log] ne $a_w}
# `winfo ismapped` is useless here — this suite never maps its
# toplevel, so everything reports unmapped and the pin would pass
# against any implementation at all. Ask the geometry manager instead:
# `pack info` errors for a widget pack does not manage.
proc packed {w} { return [expr {![catch { pack info $w }]}] }
ok "…and A's is still alive, just not packed" \
    {[winfo exists $a_w] && ![packed $a_w]}
ok "…and B's is the one packed" {[packed [::vchat::log]]}
set ::paints 0
::vchat::open_session "A"
ok "coming back shows A's ORIGINAL widget, not a rebuilt one" \
    {[::vchat::log] eq $a_w}
ok "…with not one message repainted" {$::paints == 0}
ok "…and its text exactly as it was left" {[body] eq $a_text}

# ── every loaded session keeps its own stream ────────────────────────
#
# USER, on keeping hidden sessions live: "can this be done multithreaded
# and/or async? I guess I mean concurrently. Seems like something that
# should be possible but who knows with tk."
#
# It is, and Tk was never the obstacle: ::vapi::session_events is
# `http::geturl -handler … -command … -timeout 0`, fileevent-driven with
# no `vwait`, and ::vapi::_sse was already an array keyed by token. The
# single-stream assumption lived entirely in ::vchat, in ten scalars.
puts "\nevery loaded session streams, concurrently"

reset
::vchat::open_session "A"
::vchat::open_session "B"
ok "both sessions have a stream open" \
    {[lsort [::vchat::streaming_sessions]] eq {A B}}
ok "…and switching did NOT close the one you left" \
    {[lsearch -exact [::vchat::streaming_sessions] A] >= 0}
ok "…each resuming from its OWN cursor" \
    {[info exists ::vchat::ev_cursor(A)] && [info exists ::vchat::ev_cursor(B)]}

# The payoff: a turn in the session you are NOT looking at lands in its
# own transcript. Before this it landed nowhere — the stream was closed,
# and even when it was not, the paint was gated on being on screen.
reset
::vchat::open_session "A"
::vchat::open_session "B"
::vchat::_mark_running "A" 1
::vchat::on_session_event "A" turn-started [dict create prompt "ask A"]
::vchat::on_session_event "A" turn-result \
    [dict create text "answer for A" provider p model m prompt_tokens 1 \
        completion_tokens 2 last_msg_id 7]
ok "a background turn's question lands in ITS transcript" \
    {[string match "*ask A*" [body_of A]]}
ok "…and so does its answer" {[string match "*answer for A*" [body_of A]]}
ok "…and neither reaches the session on screen" \
    {![string match "*ask A*" [body]] && ![string match "*answer for A*" [body]]}
ok "…and the status line says a turn finished elsewhere, not 'done'" \
    {[string match "*another session*" $::vstatus::last]}

# A background error is the sharper case: it used to be dropped
# outright, so a turn that died while you were elsewhere showed nothing
# at all when you came back to it.
reset
::vchat::open_session "A"
::vchat::open_session "B"
::vchat::_mark_running "A" 1
set ::vchat::stream_session "A"
::vchat::on_error "boom in A"
set ::vchat::stream_session ""
ok "a background turn's error is written where it happened" \
    {[string match "*boom in A*" [body_of A]]}
ok "…and not into the transcript you are reading" \
    {![string match "*boom*" [body]]}

# Switching TO a busy session touches NOTHING.
#
# It used to re-handshake, because a hidden session's deltas were
# dropped and the engine's turn-snapshot was the only way to recover
# them. `_events_reconnect` is the dropped-socket path: it stamps
# ev_reconnect_pending, and `_flush_replay` then deletes from
# turn_reload_anchor to the end of the transcript before repainting.
# On a plain view switch that wiped and rebuilt a whole live region for
# nothing — USER 2026-09-16: "the whole transcript seems to
# reload/refresh on reopen", and no, not on first open.
#
# A hidden session paints its own stream into its own widget now, so
# the stream stayed up, the text is already there, and the correct
# action on returning is none at all.
reset
::vchat::open_session "A"
::vchat::open_session "B"
::vchat::_mark_running "A" 1
set ::vapi::ev_opens {}
::vchat::open_session "A"
ok "switching back to a BUSY session opens no new stream" \
    {[llength $::vapi::ev_opens] == 0}
ok "…and does not stamp it a reconnect" \
    {![info exists ::vchat::ev_reconnected(A)] || $::vchat::ev_reconnected(A) == 0}
ok "…so nothing arms the delete-and-repaint" \
    {![info exists ::vchat::ev_reconnect_pending(A)]
     || $::vchat::ev_reconnect_pending(A) == 0}
::vchat::_mark_running "A" 0
set ::vapi::ev_opens {}
::vchat::open_session "B"
::vchat::open_session "A"
ok "switching to a QUIET session does not" \
    {[llength $::vapi::ev_opens] == 0}

# ── the LRU ceiling ─────────────────────────────────────────────────
#
# "Never unloaded" still needs a ceiling, or a long day across fifty
# sessions keeps fifty text widgets and their decoded images with
# nothing to bound it. Past the cap the OLDEST goes — and is named,
# because an eviction nobody is told about is indistinguishable from
# the bug this replaced.
reset
set ::vchat::logs_cap 3
set ::vstatus::last ""
foreach sid {s1 s2 s3 s4} { ::vchat::open_session $sid }
ok "past the cap, the oldest transcript is dropped" \
    {![dict exists $::vchat::logs s1]}
ok "…and the ones after it are kept" \
    {[dict exists $::vchat::logs s2] && [dict exists $::vchat::logs s4]}
# The eviction is folded into the status line of the open that caused
# it, not said before it — a message posted on the way IN would be
# overwritten by the open's own line and never reach the user.
ok "…and the eviction SAYS which session it unloaded" \
    {[string match "*unloaded*s1*" $::vstatus::last]}
ok "…in the same line that names the session being opened" \
    {[string match "*session s4*" $::vstatus::last]}

# The session on screen and any session with a turn in flight are never
# the victim: evicting a busy session would silently stop watching a
# running turn, which is the opposite of what keeping them is for.
reset
set ::vchat::logs_cap 2
::vchat::open_session "busy1"
::vchat::_mark_running "busy1" 1
::vchat::open_session "keep2"
::vchat::open_session "onscreen3"
ok "a session with a live turn is never evicted" \
    {[dict exists $::vchat::logs busy1]}
ok "…nor is the session on screen" {[dict exists $::vchat::logs onscreen3]}
ok "…the evictable one went instead" {![dict exists $::vchat::logs keep2]}
set ::vchat::logs_cap 12

# ── THE REPRO: switch away mid-turn, switch back ────────────────────
#
# USER 2026-09-16, verbatim: "open session -> switch to different
# session while turn is running -> switch back = most of the newest
# thinking blocks are missing. I guess they come back when the agent
# turn ends, maybe?"
#
# They did not come back. A hidden session's reasoning and deltas were
# dropped outright (`for_display` returned early), and the two things
# that were supposed to make that safe both failed: `on_done` read
# the accumulator only when on screen, so the reconcile rebuilt from ONE
# display_reasoning; and switching back asked the engine for a
# turn-snapshot whose reasoning had been cleared on every ToolEnd.
#
# Each session owns its live region now. Three rounds arrive while A is
# hidden; all three are in A's transcript when you return, and none of
# them is in B's.
puts "\nthinking that arrives while a session is hidden is kept"
reset
::vchat::open_session "A"
::vchat::open_session "B"
::vchat::_mark_running "A" 1
dict set ::vchat::turn_anchor A [[::vchat::log_for A] index end-1c]

set ::vchat::stream_session "A"
::vchat::on_reasoning "round one thinking"
::vchat::on_tool "start" "terminal" [dict create args {{"cmd":"ls"}}]
::vchat::on_tool "end"   "terminal" [dict create result "a.txt"]
::vchat::on_reasoning "round two thinking"
::vchat::on_tool "start" "read" [dict create args {{"path":"a.txt"}}]
::vchat::on_tool "end"   "read" [dict create result "contents"]
::vchat::on_reasoning "round three thinking"
set ::vchat::stream_session ""
::vchat::_flush_live_reasoning "A"
update

ok "every round's thinking landed in the hidden session's transcript" \
    {[string match "*round one thinking*" [body_of A]]
     && [string match "*round two thinking*" [body_of A]]
     && [string match "*round three thinking*" [body_of A]]}
ok "…and its tool rounds with them" \
    {[string match "*terminal*" [body_of A]] && [string match "*read*" [body_of A]]}
ok "…and NONE of it reached the session on screen" \
    {![string match "*round one thinking*" [body]]
     && ![string match "*round two thinking*" [body]]}

# Switching back must not rebuild what is already there. A mark set
# before the switch is the witness: the delete-and-repaint that
# `_events_reconnect` armed would take it with the region.
[::vchat::log_for A] mark set switchwitness "end-1c"
::vchat::open_session "A"
update
ok "switching back deletes nothing" \
    {[lsearch -exact [[::vchat::log_for A] mark names] switchwitness] >= 0}
ok "…and the thinking is still all there" \
    {[string match "*round one thinking*" [body]]
     && [string match "*round three thinking*" [body]]}

# And at turn end the reconcile reads THIS session's segments — one
# collapsible per round, the reload shape (R7) — instead of rebuilding
# a single block from the done event.
set ::vchat::stream_session "A"
::vchat::on_done [dict create provider p model m prompt_tokens 1 \
                      completion_tokens 2 text "the answer" \
                      display_reasoning "round three thinking"]
set ::vchat::stream_session ""
update
ok "the finished turn keeps every round's reasoning, not just the last" \
    {[string match "*round one thinking*" [body_of A]]
     && [string match "*round two thinking*" [body_of A]]
     && [string match "*round three thinking*" [body_of A]]}
ok "…and the answer is there too" {[string match "*the answer*" [body_of A]]}

# The same, for a turn that finishes while you are looking elsewhere —
# the half that made `on_done`'s `$on_screen ?` gate wrong.
puts "\na turn that ENDS while hidden keeps its rounds too"
reset
::vchat::open_session "A"
::vchat::open_session "B"
::vchat::_mark_running "A" 1
dict set ::vchat::turn_anchor A [[::vchat::log_for A] index end-1c]
set ::vchat::stream_session "A"
::vchat::on_reasoning "hidden round one"
::vchat::on_tool "start" "terminal" [dict create args {}]
::vchat::on_tool "end"   "terminal" [dict create result "ok"]
::vchat::on_reasoning "hidden round two"
::vchat::on_done [dict create provider p model m prompt_tokens 1 \
                      completion_tokens 2 text "hidden answer" \
                      display_reasoning "hidden round two"]
set ::vchat::stream_session ""
update
ok "both rounds survive a turn that ended off-screen" \
    {[string match "*hidden round one*" [body_of A]]
     && [string match "*hidden round two*" [body_of A]]}
ok "…and B is untouched" {![string match "*hidden round*" [body]]}

# ── a snapshot rebuilds the turn, not a lump of it ──────────────────
#
# The snapshot is what a client attaching MID-TURN gets — a second
# window, a reload, another surface. It had two faults at once: the
# engine cleared its reasoning on every ToolEnd, so it carried only the
# last round; and this side painted "all the reasoning, then all the
# content, then all the tools", which is not the order anything
# happened in.
#
# Both are fixed, so the snapshot now replays the turn's real shape:
# each round's thinking, then its call, then the live tail.
puts "\na mid-turn snapshot replays every round, in order"
reset
::vchat::open_session "S"
::vchat::_mark_running "S" 1
set ::vchat::stream_session "S"
::vchat::_paint_snapshot [dict create \
    rounds [list \
        [dict create reasoning "first thoughts" content "" \
                     name terminal args {{"cmd":"ls"}} result "a.txt"] \
        [dict create reasoning "second thoughts" content "" \
                     name read args {{"path":"a.txt"}} result "contents"]] \
    reasoning "third thoughts" content "the partial answer"]
set ::vchat::stream_session ""
::vchat::_flush_live_reasoning "S"
update
set b [body_of S]
ok "the first round's thinking is there — the one the engine used to drop" \
    {[string match "*first thoughts*" $b]}
ok "…and the second" {[string match "*second thoughts*" $b]}
ok "…and the live tail since the last round" {[string match "*third thoughts*" $b]}
ok "…and the partial answer" {[string match "*the partial answer*" $b]}

# ORDER is the other half: a round's thinking comes before its call,
# and the tail comes after both. A lump in the wrong order reads as a
# different turn than the one that ran.
proc _at {hay needle} { return [string first $needle $hay] }
ok "each round's thinking precedes its own call" \
    {[_at $b "first thoughts"] < [_at $b "terminal"]
     && [_at $b "second thoughts"] < [_at $b "read"]}
ok "…the rounds are in the order they happened" \
    {[_at $b "first thoughts"] < [_at $b "second thoughts"]}
ok "…and the live tail lands after the last round" \
    {[_at $b "read"] < [_at $b "third thoughts"]}

# Clearing or evicting ONE transcript must not blank another session's
# live region. The accumulators are a single swapped set (_live_ctx), so
# `forget_raw` — which runs on a session change and on eviction —
# resets them only when the loaded region is the one being wiped.
puts "\nwiping one transcript leaves another session's live turn alone"
reset
::vchat::open_session "STREAM"
::vchat::_mark_running "STREAM" 1
set ::vchat::stream_session "STREAM"
::vchat::on_reasoning "mid-turn thinking"
set ::vchat::stream_session ""
::vchat::_flush_live_reasoning "STREAM"
ok "the streaming session has an open live region" \
    {$::vchat::live_ctx eq "STREAM" && $::vchat::live_rtag ne ""
     && [llength $::vchat::live_blocks] == 1}

# Open a DIFFERENT session for the first time: that path clears and
# repaints its own widget, and used to reset whatever region happened
# to be loaded.
::vchat::open_session "OTHER"
ok "…and opening another session did not blank it" \
    {[dict exists $::vchat::live_saved STREAM]
     || ($::vchat::live_ctx eq "STREAM" && $::vchat::live_rtag ne "")}
ok "…so its thinking is still in its own transcript" \
    {[string match "*mid-turn thinking*" [body_of STREAM]]}
::vchat::_mark_running "STREAM" 0

# ── the session list is told which session is on screen ─────────────
#
# USER 2026-09-17: "the highlighted session doesn't seem to be the
# actually attached one". open_session is the ONE funnel every switch
# goes through — the sidebar click, the palette's "goto", the Dev tab,
# /clear's reopen — so it is the one place that can keep the pane
# honest. The pane's own `refresh` cannot: it runs on turn boundaries
# and costs a blocking round trip.
puts "\nopening a session re-marks the session list"

set ::vsidebar::marks 0
::vchat::open_session "STREAM"
ok "a switch tells the sidebar which session is on screen" \
    {$::vsidebar::marks >= 1}
set ::vsidebar::marks 0
::vchat::open_session "OTHER"
ok "…and so does the next one" {$::vsidebar::marks >= 1}

# ── ▶ means the ENGINE says running, not "this window once thought so" ──
#
# USER 2026-09-17: "the session that was attached … is still showing as
# active even though it's actually ended a long time ago".
#
# `running` was a client-side dict, set and cleared purely by stream
# events, and nothing ever reconciled it. A turn whose end this window
# did not witness — its stream never opened, or was evicted, dropped, or
# outlived by a gateway restart — left the mark on forever, with no path
# back. /api/sessions has carried an authoritative cross-process
# `running` per session all along (the session_runs registry, reaped by
# heartbeat), and nothing read it.
puts "\nthe running set is reconciled with the engine"

# A turn on ANOTHER SURFACE (tui, telegram, cron) — this window never
# saw a turn-started for it and could not have.
::vchat::sync_running [list [dict create id "TUI-SID" running true]]
ok "a turn on another surface is adopted" {[::vchat::is_running "TUI-SID"]}
::vchat::sync_running [list [dict create id "TUI-SID" running false]]
ok "…and let go when the engine says it stopped" \
    {![::vchat::is_running "TUI-SID"]}

# THE STALE MARK: this window believes, the engine does not, and the
# belief is old. That is the report.
::vchat::_mark_running "STALE" 1
set ::vchat::running_since(STALE) 0
::vchat::sync_running [list [dict create id "STALE" running false]]
ok "a mark the engine does not share is cleared" {![::vchat::is_running "STALE"]}

# …but NOT the one this window made a moment ago. send_current marks
# running BEFORE the POST, and _mark_running refreshes the pane, so
# without the grace the very refresh a send triggers would clear the
# flag it just set and flicker Send/Steer on every message.
::vchat::_mark_running "FRESH" 1
::vchat::sync_running [list [dict create id "FRESH" running false]]
ok "a turn sent a moment ago survives its own refresh" \
    {[::vchat::is_running "FRESH"]}
# Once the engine CONFIRMS it, the grace is spent — a later stop is
# obeyed at once rather than waiting the window out again.
::vchat::sync_running [list [dict create id "FRESH" running true]]
::vchat::sync_running [list [dict create id "FRESH" running false]]
ok "…and is dropped the moment the engine has confirmed then stopped it" \
    {![::vchat::is_running "FRESH"]}

# An older gateway answers with no verdict at all. Silence is not
# "false": nothing about liveness is inferred from key absence here
# either.
::vchat::_mark_running "OLDGW" 1
set ::vchat::running_since(OLDGW) 0
::vchat::sync_running [list [dict create id "OLDGW"]]
ok "a listing that carries no verdict changes nothing" \
    {[::vchat::is_running "OLDGW"]}
::vchat::_mark_running "OLDGW" 0

# NO RE-ENTRANCY. This runs from inside ::vsidebar::refresh, which is
# mid-rebuild of its own listbox; a _mark_running that repainted the
# pane from here would re-enter the rebuild that called it.
set ::vsidebar::refreshes 0
::vchat::sync_running [list [dict create id "REENT" running true]]
ok "reconciling does not re-enter the pane's refresh" \
    {$::vsidebar::refreshes == 0 && [::vchat::is_running "REENT"]}
::vchat::_mark_running "REENT" 0

# ── following survives a switch ─────────────────────────────────────
#
# USER 2026-09-17: "the 'following' unsticks when switching sessions and
# has to be clicked/scrolled again each time it's re-attached".
#
# _maybe_follow rightly refuses to scroll a widget nobody is looking at,
# so a hidden session's transcript grows past its viewport. `follow` is
# a WINDOW-wide arm; nothing remembered that THIS session was sitting at
# its live edge, so coming back read a yview that was no longer at the
# bottom and offered the ▾ button instead of following.
puts "\nfollowing survives a switch away and back"

reset
set ::vchat::follow 1
::vchat::open_session "FA"
for {set i 0} {$i < 80} {incr i} {
    ::vchat::log_insert "filler line $i to make the view scroll\n" status "FA"
}
set wa [::vchat::log_for "FA"]
$wa see end
update idletasks
set pre_edge [expr {[lindex [$wa yview] 1] >= 0.999}]

# Away, and a turn keeps painting into the hidden transcript.
::vchat::open_session "FB"
for {set i 0} {$i < 80} {incr i} {
    ::vchat::log_insert "arrived while hidden $i\n" status "FA"
}
update idletasks
set hidden_edge [expr {[lindex [$wa yview] 1] >= 0.999}]

# …and back.
::vchat::open_session "FA"
update idletasks
set back_edge [expr {[lindex [$wa yview] 1] >= 0.999}]
set btn [winfo exists .mid.chatwrap.chat.follow]

ok "precondition: the view was at the live edge before the switch" {$pre_edge}
ok "precondition: nothing scrolled the hidden transcript" {!$hidden_edge}
ok "coming back lands on the live edge again" {$back_edge}
ok "…and does not ask to be clicked back into following" {!$btn}

# The other direction, which this must not break: a session left
# SCROLLED UP comes back exactly where it was.
reset
set ::vchat::follow 1
::vchat::open_session "FC"
for {set i 0} {$i < 80} {incr i} {
    ::vchat::log_insert "filler line $i to make the view scroll\n" status "FC"
}
set wc [::vchat::log_for "FC"]
$wc yview moveto 0.0
update idletasks
::vchat::open_session "FD"
for {set i 0} {$i < 20} {incr i} {
    ::vchat::log_insert "arrived while hidden $i\n" status "FC"
}
::vchat::open_session "FC"
update idletasks
ok "a session left scrolled up is not yanked to the end" \
    {[lindex [$wc yview] 0] < 0.05}
ok "…and the ▾ button says what arrived instead" \
    {[winfo exists .mid.chatwrap.chat.follow]}

# follow OFF means reading wins over arrival, switch or no switch.
reset
set ::vchat::follow 0
::vchat::open_session "FE"
for {set i 0} {$i < 80} {incr i} {
    ::vchat::log_insert "filler line $i to make the view scroll\n" status "FE"
}
set we [::vchat::log_for "FE"]
$we see end
update idletasks
::vchat::open_session "FF"
for {set i 0} {$i < 80} {incr i} {
    ::vchat::log_insert "arrived while hidden $i\n" status "FE"
}
::vchat::open_session "FE"
update idletasks
ok "with follow off, coming back does not jump to the end" \
    {[lindex [$we yview] 1] < 0.999}
set ::vchat::follow 1
catch { destroy .mid.chatwrap.chat.follow }

# ── the ask events the session stream carries ───────────────────────
#
# `ask-resolved` and `ask-relayed` are published on the SESSION tap —
# the stream this window actually listens to — and on_session_event's
# switch had no branch for either, and no default. They were dispatched
# only in api.tcl's handler-less arm, which the chat stream never takes
# (session_events always registers a handler). So a question answered on
# telegram left this window's dialog up, and the relay notice was
# dropped on the floor: asktest exercises ::vask::resolved thoroughly
# and nothing called it.
puts "\nask events reach the dialog through the session stream"

namespace eval ::vask {
    variable seen {}
    proc show     {d} { lappend ::vask::seen [list show     $d $::vchat::stream_session] }
    proc resolved {d} { lappend ::vask::seen [list resolved $d $::vchat::stream_session] }
    proc relayed  {d} { lappend ::vask::seen [list relayed  $d $::vchat::stream_session] }
}

reset
set ::vask::seen {}
::vchat::open_session "ASKER"
::vchat::on_session_event "ASKER" ask \
    [dict create id "a1" session_id "ASKER" kind approval prompt "run it?"]
ok "an ask reaches the dialog" \
    {[lindex [lindex $::vask::seen 0] 0] eq "show"}
# The stream's own session is in hand while the dialog is built, so an
# older payload with no session_id still resolves to the right one.
ok "…with the stream's session in hand" \
    {[lindex [lindex $::vask::seen 0] 2] eq "ASKER"}

::vchat::on_session_event "ASKER" ask-resolved \
    [dict create id "a1" by "answer" answer "once"]
ok "ask-resolved reaches the dialog's resolver" \
    {[lindex [lindex $::vask::seen 1] 0] eq "resolved"}

::vchat::on_session_event "ASKER" ask-relayed \
    [dict create id "a1" to "telegram:owner" after_secs 30]
ok "ask-relayed reaches the relay notice" \
    {[lindex [lindex $::vask::seen 2] 0] eq "relayed"}

ok "…and none of them leaves the stream session set" \
    {$::vchat::stream_session eq ""}

namespace eval ::vask { proc show {d} {} ; proc resolved {d} {} ; proc relayed {d} {} }

# ── the store changed under a kept transcript ───────────────────────
#
# USER 2026-09-17: "I did notice the compaction 'reload' inconsistency".
#
# A kept session never repaints — that is the POINT of keeping it, and
# it is what makes a switch cost one `pack`. But `/clear`, `/undo`,
# `/branch` and a finished compaction all change the STORE under a
# transcript that is already on screen, and both call sites did
# `_drop_cache` + `open_session`. `_drop_cache` forgets the parsed
# HISTORY CACHE; the widget is untouched, so open_session takes the kept
# branch and returns without painting. The window went on showing the
# pre-change transcript while the status line said "reloading".
puts "\nthe transcript reloads when the store changes under it"

proc ::vapi::get_json {path} {
    if {[string match "*history*" $path]} {
        return [dict create messages \
            [list [dict create role user content $::HISTTEXT]] last_msg_id 7]
    }
    return {}
}
proc ::vapi::get_json_async {path cmd {ms 0}} { return fake_async }

reset
set ::HISTTEXT "before the compaction"
::vchat::open_session "CMP"
update idletasks
ok "the transcript painted its history" \
    {[string match "*before the compaction*" [body_of CMP]]}

# The engine compacted: the store's rows are different now, and the
# stream says so.
set ::HISTTEXT "after the compaction"
::vchat::on_session_event "CMP" history-changed \
    [dict create reason compaction last_msg_id 9]
update idletasks
ok "…and repaints from the store when the store changes" \
    {[string match "*after the compaction*" [body_of CMP]]}
ok "…leaving none of the text the store no longer has" \
    {![string match "*before the compaction*" [body_of CMP]]}

# A HIDDEN session has the same stale widget, and switching back is
# exactly when it would be believed.
reset
set ::HISTTEXT "hidden, before"
::vchat::open_session "HID"
::vchat::open_session "VIS"
set ::HISTTEXT "hidden, after"
::vchat::on_session_event "HID" history-changed \
    [dict create reason clear last_msg_id 11]
::vchat::open_session "HID"
update idletasks
ok "a hidden session repaints on the switch back" \
    {[string match "*hidden, after*" [body_of HID]]}
ok "…and does not keep what the store dropped" \
    {![string match "*hidden, before*" [body_of HID]]}

# NEVER under a live turn: dropping the widget would throw away the
# region the turn is painting into and close the stream feeding it.
# (Both real callers clear the running mark first — history-changed
# means the store moved OUTSIDE a turn — so this is the contract for
# any future caller, asserted directly.)
reset
set ::HISTTEXT "mid-turn"
::vchat::open_session "LIVE"
::vchat::_mark_running "LIVE" 1
set ::vchat::stream_session "LIVE"
::vchat::on_reasoning "thinking in the open"
set ::vchat::stream_session ""
::vchat::_flush_live_reasoning "LIVE"
update idletasks
set ::HISTTEXT "yanked"
::vchat::reload_session "LIVE"
update idletasks
ok "a session with a turn in flight is not wiped under it" \
    {[string match "*thinking in the open*" [body_of LIVE]]}
ok "…and keeps its stream" \
    {[info exists ::vchat::ev_stream(LIVE)] && $::vchat::ev_stream(LIVE) ne ""}
::vchat::_mark_running "LIVE" 0

# ── a session written by ANOTHER PROCESS ─────────────────────────────
#
# session_tap is a process-wide broadcast channel and a delegate or
# /btw child is a separate `veles chat -q`, so a child session's live
# events never reach this gateway at all. The engine polls the store
# for such a session and sends the gap as ordinary `history` frames,
# closed by `history-appended`. This arm is the boundary that paints
# them.
#
# USER 2026-09-18: "when I attached the child in the desktop I could
# see its progress but then at some point it stopped streaming and
# looked hung even though there were new messages coming in."
reset
set ::vchat::session "A"
::vchat::on_session_event "A" session-live [dict create last_msg_id 4]
set _opens_before [llength $::vapi::ev_opens]
::vchat::on_session_event "A" history \
    [dict create role assistant content "the child spoke"]
ok "a history row after the live boundary is buffered, not painted yet" \
    {![string match "*the child spoke*" [body]]}
::vchat::on_session_event "A" history-appended [dict create last_msg_id 9]
ok "history-appended paints the rows buffered since the boundary" \
    {[string match "*the child spoke*" [body]]}
ok "…and advances the cursor to the id it carries" \
    {$::vchat::ev_cursor(A) == 9}
# NOT session-live: that arm runs the was-I-handed-a-snapshot ladder
# and would re-handshake the stream once per poll tick, forever.
ok "…and does NOT reconnect the stream" \
    {[llength $::vapi::ev_opens] == $_opens_before}
# …and it appends. A follow tick that deleted back to an anchor would
# eat the turn it is trying to extend.
::vchat::on_session_event "A" history \
    [dict create role assistant content "and again"]
::vchat::on_session_event "A" history-appended [dict create last_msg_id 11]
ok "a second batch appends below the first" \
    {[string first "the child spoke" [body]] < [string first "and again" [body]]}

# ── force-refresh: the window the user says is wrong ─────────────────
#
# reload_session refuses to touch a RUNNING session, which is correct
# for the automatic callers (an event must not yank the transcript out
# from under a live turn) and exactly backwards for a human asking,
# because the case they ask in IS a running session — one running in
# another process, whose events this window cannot receive.
#
# USER 2026-09-18: "we probably need a way to force-refresh a
# transcript/session window".
reset
set ::vchat::session "A"
set ::refetched {}
proc ::vapi::get_json {path} {
    lappend ::refetched $path
    return [dict create messages [list [dict create role assistant content "fresh from the store"]] last_msg_id 12]
}
::vchat::emit_message user "stale text"
dict set ::vchat::hist_cache A [dict create messages {} last_msg_id 3]
::vchat::_mark_running "A" 1
::vchat::reload_session "A"
ok "an automatic reload leaves a running session alone" \
    {[string match "*stale text*" [body]] && [llength $::refetched] == 0}
::vchat::force_refresh
ok "a forced one refetches it anyway" \
    {![string match "*stale text*" [body]]}
ok "…from the STORE, not from the parsed-history cache" \
    {[lsearch -glob $::refetched "/api/sessions/A/history"] >= 0}
ok "…and what it paints is what the store now says" \
    {[string match "*fresh from the store*" [body]]}
reset
set ::vchat::session ""
set _rc [catch { ::vchat::force_refresh } _e]
ok "force_refresh with no session on screen is a no-op, not an error" \
    {$_rc == 0}

# ── /refetch, the command spelling of the store reread ───────────────
#
# USER 2026-09-19: /redraw on a subtask's window was expected to reread
# the store; it only repaints memory. /redraw keeps that job (the
# FIRST defect — it repainted EVERY loaded session's messages into the
# visible widget — is pinned in rendertest); the reread has its own
# command now.
reset
set ::vchat::session "A"
set ::refetched {}
proc ::vapi::get_json {path} {
    lappend ::refetched $path
    return [dict create messages [list [dict create role assistant content "reread now"]] last_msg_id 3]
}
::vchat::emit_message user "stale text" {}
::vchat::slash_command "/refetch"
ok "/refetch rereads the STORE, not memory" \
    {[lsearch -glob $::refetched "/api/sessions/A/history"] >= 0}
ok "…and repaints from it" \
    {![string match "*stale text*" [body]] && [string match "*reread now*" [body]]}
reset
set _rc [catch { ::vchat::slash_command "/refetch" } _e]
ok "/refetch with no session on screen is a no-op, not an error" \
    {$_rc == 0}
# The two commands are NOT the same thing: /refetch must hit the store
# even when it cannot tell whether it needs to.
set ::vchat::session "A"
set ::hits 0
set ::vstatus::last ""
proc ::vapi::get_json {path} { incr ::hits ; return [dict create messages {} last_msg_id 0] }
::vchat::slash_command "/redraw"
set _hits_after_redraw $::hits
set _msg_after_redraw $::vstatus::last
::vchat::slash_command "/refetch"
ok "/redraw repaints memory, /refetch rereads the store" \
    {$_hits_after_redraw == 0 && $::hits > 0}
ok "…and each says which on the status line" \
    {$_msg_after_redraw eq "redrawn" && $::vstatus::last eq "transcript refetched"}

# ── round 2, 2026-09-20: the weld returned, and follow still unsticks ─
#
# USER: "The tool calls now start right after the reasoning AGAIN even
# though we've just added something today so that was not supposed to
# happen because a newline would be inserted unconditionally after
# reasoning blocks. The 'following' still unsticks sometimes."
#
# Part 1 — the weld. The unconditional separator was computed and then
# eaten: on_tool's start arm calls _close_live_reasoning (which must
# set live_last_kind "tool" so the next burst opens a fresh region)
# BEFORE asking _live_break for the break, and _live_break correctly
# answers "" for a tool→tool transition. The mark-based weld guard that
# was supposed to backstop it read its newline test through a mark that
# `end-1c linestart` had planted on Tk's implicit trailing line — one
# line below the header — so it always saw "\n" and never fired.
# The separator is asked of the WIDGET TAIL now.
puts "\nthe reasoning→tool separator survives its own close helper"
reset
set ::vchat::session "A"
set ::vchat::stream_session "A"
dict set ::vchat::turn_anchor A 1.0
::vchat::_mark_running "A" 1
::vchat::on_reasoning "thinking about the file"
::vchat::_flush_live_reasoning "A"
::vchat::on_tool start terminal [dict create \
    command "cargo test" timeout 60]
set _b [body]
ok "the ⚙ header lands on its own line" \
    {[string match "*thinking about the file\n⚙ terminal*" $_b]}
ok "…the welded shape (reasoning run straight into ⚙) is absent" \
    {![string match "*file⚙*" $_b]}

# The end arm's result never welds onto mid-round reasoning either (the
# original 2026-09-18 shape, pinned again now that the guards changed).
::vchat::on_reasoning "and thinking again mid-round"
::vchat::_flush_live_reasoning "A"
::vchat::on_tool end terminal [dict create result "done in 3s"]
set _b [body]
ok "a result after mid-round reasoning starts on a fresh line" \
    {[string match "*mid-round\n*result*" $_b]}
ok "mid-round reasoning survives the round" \
    {[string match "*thinking again mid-round*" $_b]}

# Part 2 — follow latching. POSE IS NOT INTENT: measured under Xvfb, a
# transcript pinned at the bottom whose width changes (right pane
# docking on a preview/focus tool call, chips packing on, a sash move)
# re-wraps and settles at yview[1] ≈ 0.96 with no scroll and no insert.
# The prestash then read "the user left" and disarmed follow exactly
# when tool output was arriving. at_edge is a LATCH: set by arrival and
# jumps, cleared only by user scrolling (the _read_pose bindings).
puts "\nfollow is an intent latch, immune to geometry noise"
reset
set ::vchat::follow 1
::vchat::open_session "FA"
for {set i 0} {$i < 80} {incr i} {
    ::vchat::log_insert "filler line $i to make the view scroll\n" status "FA"
}
set wa [::vchat::log_for "FA"]
$wa see end
update idletasks
ok "precondition: at the live edge" \
    {[expr {[lindex [$wa yview] 1] >= 0.999}]}
# THE SIMULATED PANE DOCK: shrink the width so the content re-wraps —
# the pose fraction drops below the threshold with no user scroll.
wm geometry . 200x260
update idletasks
set _pose [lindex [$wa yview] 1]
ok "precondition: the re-wrap moved the pose off the exact bottom" \
    {$_pose < 0.999}
# Insert more content: arrival must STILL follow, because the latch —
# not the pose — answers the at-bottom question.
::vchat::log_insert "arrived after the re-wrap\n" status "FA"
ok "follow survives a width change with no user scroll" \
    {[lindex [$wa yview] 1] >= 0.999}
# And the counter-case: the user scrolling away KEEPS the button honest.
$wa yview moveto 0.0
::vchat::_read_pose
::vchat::log_insert "more output the user will not be dragged to\n" status "FA"
ok "a real user scroll away still disarms arrival" \
    {[lindex [$wa yview] 0] < 0.05}
ok "…and the ▾ button offers the way back" \
    {[winfo exists .mid.chatwrap.chat.follow]}
wm geometry . 420x260
update idletasks
catch { destroy .mid.chatwrap.chat.follow }
set ::vchat::follow 1

# ── empty frames do not break regions (2026-09-20 round 2) ───────────
#
# Providers can emit zero-length delta/reasoning frames between bursts
# of one thought; the kind transition they caused closed the reasoning
# region and the next burst reopened it under a second header.
puts "\nempty frames are not breaks"
reset
set ::vchat::session "A"
set ::vchat::stream_session "A"
dict set ::vchat::turn_anchor A 1.0
::vchat::_mark_running "A" 1
::vchat::on_reasoning "one continuous thought"
::vchat::on_reasoning ""
::vchat::on_reasoning " continues"
::vchat::_flush_live_reasoning "A"
::vchat::on_delta ""
::vchat::on_reasoning " and more"
::vchat::_flush_live_reasoning "A"
set _b [body]
ok "one thought across empty frames stays one reasoning block" \
    {[llength [regexp -all -inline {reasoning \(streaming} $_b]] == 1}

# The empty transcript's hint names the key the keymap gives "new
# session", or none: it said "(M-n)" in every mode, and on a phone.
puts "\nthe empty state names the real key"
rename ::vkeymap::key_for _kf_saved
proc ::vkeymap::key_for {cmd} { expr {$cmd eq "::vsidebar::new_session" ? "Ctrl-N" : ""} }
ok "a keyboard's key, from the registry" {[string match "*start a new one (Ctrl-N)" [::vchat::empty_state_text]]}
proc ::vkeymap::key_for {cmd} { return "" }
ok "no key, no parentheses (a phone)" {[string match "*start a new one" [::vchat::empty_state_text]]}
rename ::vkeymap::key_for {}
rename _kf_saved ::vkeymap::key_for

# ── another node's session: read-only (USER 2026-10-05, "a key audit
# path") ──────────────────────────────────────────────────────────────
puts "\nanother node's session opens read-only"
proc ::vapi::get_json {path} {
    if {$path eq "/api/sessions/VPS-1/history"} {
        return [dict create messages [list [dict create role user content "irc line"]] \
            read_only true node_name vps last_msg_id 7]
    }
    if {[string match "*/history" $path]} {
        return [dict create messages [list [dict create role user content "local turn"]]]
    }
    return [dict create]
}
reset
set ::vapi::ev_opens {}
::vchat::open_session "VPS-1"
ok "its transcript is painted" {[string match "*irc line*" [body]]}
ok "no event stream follows it" {![llength $::vapi::ev_opens]}
ok "the composer is off" {[.mid.chatwrap.chat.input cget -state] eq "disabled"}
ok "the banner names the node" \
    {[winfo ismapped .mid.chatwrap.chat.ro] || [string match "*vps*" [.mid.chatwrap.chat.ro.l cget -text]]}
ok "…and says where it lives" {[string match "*lives on vps*" [.mid.chatwrap.chat.ro.l cget -text]]}
set ::vstatus::last ""
::vchat::send_current
ok "sending is refused with the reason" {[string match "*lives on vps*" $::vstatus::last]}
::vchat::open_session "LOCAL-1"
ok "a local session gets its composer back" {[.mid.chatwrap.chat.input cget -state] eq "normal"}
ok "…and its stream" {[llength $::vapi::ev_opens] == 1}
ok "…and no banner" {[lsearch [pack slaves .mid.chatwrap.chat] .mid.chatwrap.chat.ro] < 0}
# Coming back to the kept transcript stays read-only and streamless.
::vchat::open_session "VPS-1"
ok "back on the remote one: composer off again" {[.mid.chatwrap.chat.input cget -state] eq "disabled"}
ok "…still no stream" {[llength $::vapi::ev_opens] == 1}
::vchat::open_session "LOCAL-1"

puts "\nsessiontest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
