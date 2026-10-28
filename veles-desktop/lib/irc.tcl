# veles-desktop/lib/irc.tcl — the IRC client pane.
#
# Three regions, which is why tabs had to come first: a buffer list, a
# transcript and a roster do not fit in the right-hand pane's 40% beside
# a chat transcript. The IRC tab gives them the whole window.
#
#   .ircbufs    networks and their buffers, unread counts
#   .ircview    the transcript + the composer
#   .ircroster  who is in the channel, ops and voice first
#
# WHERE THE LINES COME FROM
#
# Never from the IRC server directly. The gateway holds the connection —
# that is the entire point of the bouncer: it stays attached while the
# desktop is closed. So this reaches IRC the same way it reaches
# everything else, through the gateway API:
#
#   GET /api/irc/state     what is connected, joined, and has history
#   GET /api/irc/backlog   one buffer, paged backwards
#   GET /api/irc/events    the live stream, resumable from an id
#
# RESUMING
#
# The event stream replays from the store before going live, so
# "closed for six hours" and "socket dropped for six seconds" are the
# same case and need no separate handling here. What this side must do
# is remember the last DURABLE id it rendered: live events have not been
# written yet and carry no id, so the cursor stops advancing once live
# and resumes from the last stored line if the socket drops.
#
# FORMATTING
#
# IRC lines are not markdown and must not go through ::vmd::. A message
# containing *stars* or _underscores_ is text with stars and underscores
# in it; interpreting it would corrupt what someone actually typed. What
# they DO carry is mIRC control codes, which this renders as tags.

namespace eval ::virc {
    variable R_bufs   .ircbufs
    variable R_view   .ircview
    variable R_roster .ircroster

    variable stream ""          ;# the open SSE token
    variable cursor 0           ;# last durable line id rendered
    variable live 0             ;# past the replay marker?
    variable current ""         ;# "network/presence/buffer" on screen
    variable buffers {}         ;# ordered list of buffer keys
    variable unread             ;# key -> count
    variable meta               ;# key -> dict(network presence buffer)
    variable rosters            ;# key -> member list
    variable topics             ;# key -> topic
    variable nets {}            ;# the last /api/irc/state payload
    variable why ""             ;# why there is nothing to show, if there is nothing
    variable retry 1000         ;# reconnect backoff, ms
    variable quoting 0          ;# C-v armed: next key goes in literally
    variable sendable {}        ;# presences the gateway will send as
    variable activity           ;# key -> level: 1 join/part/mode, 2 msg, 3 highlight
    variable own_nick           ;# "network/presence" -> the bot's own nick there
    variable highlight_words {} ;# extra words that highlight, beyond the own nick
    variable smart_filter 1     ;# hide join/part from nicks who never spoke
    variable read_marker        ;# key -> last-seen line id (drawn as a rule on reopen)
    variable seen               ;# key -> latest line id ingested, seen or not
    variable recency            ;# key -> nicks, most-recently-spoken first
    variable ignores {}         ;# glob patterns; a matching nick is not drawn
    variable aliases            ;# name -> expansion, for /alias
    variable bufopt             ;# "key/name" -> value, per-buffer overrides
    variable splits {}          ;# buffer keys pinned into extra panes
    variable split_w            ;# key -> that pane's text widget
    variable merged 0           ;# one server buffer for every network?
    variable typing             ;# key -> nicks currently typing there (ephemeral)
    variable typing_after       ;# key -> expiry timer per active nick set
    variable typing_out_after   ;# key -> our own "stop saying we type" timer
    array set unread {}
    array set aliases {}
    array set bufopt {}
    array set split_w {}
    array set meta {}
    array set typing {}
    array set rosters {}
    array set topics {}
    array set aways {}
    array set activity {}
    array set own_nick {}
    array set read_marker {}
    array set seen {}
    array set recency {}
    # Tab-completion cycling state (an array keyed by the word context),
    # NOT a scalar: the `complete` proc reads and writes it as one.
    array set complete {}
}

proc ::virc::key {network presence buffer} { return "$network/$presence/$buffer" }

# ── mIRC formatting ─────────────────────────────────────────────────
#
# \x02 bold  \x1D italic  \x1F underline  \x1E strike  \x0F reset
# \x03N[,M]  colour (foreground[,background]), 0-15 or 16-98
#
# Parsed into runs so the transcript can tag them. Returns a list of
# {text tags} pairs; unknown codes are DROPPED rather than shown,
# because a stray control byte in a transcript is worse than losing the
# styling it asked for.

# The classic 16, by Tk colour name.
proc ::virc::_mirc_basic_names {} {
    return {
        0 white 1 black 2 blue 3 green 4 red 5 brown 6 purple 7 orange
        8 yellow 9 lightgreen 10 cyan 11 lightcyan 12 lightblue
        13 pink 14 grey 15 lightgrey
    }
}

# mIRC's extended palette, 16–98. Verbatim from the published table
# rather than interpolated — it is not a gradient, it is a list, and
# generating it would get the greys at 88–98 wrong.
#
# This used to render plain, on the reasoning that "the extended palette
# is rarely used and a wrong colour is worse than none". The first half
# of that is true and the second is the right instinct; the answer is
# just to use the real values instead of guessing.
proc ::virc::_mirc_extended {} {
    return {
        16 #470000 17 #472100 18 #474700 19 #324700 20 #004700 21 #00472c
        22 #004747 23 #002747 24 #000047 25 #2e0047 26 #470047 27 #47002a
        28 #740000 29 #743a00 30 #747400 31 #517400 32 #007400 33 #007449
        34 #007474 35 #004074 36 #000074 37 #4b0074 38 #740074 39 #740045
        40 #b50000 41 #b56300 42 #b5b500 43 #7db500 44 #00b500 45 #00b571
        46 #00b5b5 47 #0063b5 48 #0000b5 49 #7500b5 50 #b500b5 51 #b5006b
        52 #ff0000 53 #ff8c00 54 #ffff00 55 #b2ff00 56 #00ff00 57 #00ffa0
        58 #00ffff 59 #008cff 60 #0000ff 61 #a500ff 62 #ff00ff 63 #ff0098
        64 #ff5959 65 #ffb459 66 #ffff71 67 #cfff60 68 #6fff6f 69 #65ffc9
        70 #6dffff 71 #59b4ff 72 #5959ff 73 #c459ff 74 #ff66ff 75 #ff59bc
        76 #ff9c9c 77 #ffd39c 78 #ffff9c 79 #e2ff9c 80 #9cff9c 81 #9cffdb
        82 #9cffff 83 #9cd3ff 84 #9c9cff 85 #dc9cff 86 #ff9cff 87 #ff94d3
        88 #000000 89 #131313 90 #282828 91 #363636 92 #4d4d4d 93 #656565
        94 #818181 95 #9f9f9f 96 #bcbcbc 97 #e2e2e2 98 #ffffff
    }
}

proc ::virc::_mirc_colour {n} {
    set basic [_mirc_basic_names]
    if {[dict exists $basic $n]} { return [dict get $basic $n] }
    set ext [_mirc_extended]
    if {[dict exists $ext $n]} { return [dict get $ext $n] }
    # 99 is "default", and anything past it does not exist. Both render
    # in the theme's own foreground rather than in a guess.
    return ""
}

proc ::virc::parse_formatting {s} {
    set runs {}
    set cur ""
    set bold 0 ; set italic 0 ; set under 0 ; set strike 0 ; set colour ""
    set n [string length $s]
    set i 0
    set flush {}
    while {$i < $n} {
        set c [string index $s $i]
        set code [scan $c %c]
        switch -- $code {
            2 - 29 - 31 - 30 - 15 - 3 {
                # A style change ends the current run.
                if {$cur ne ""} {
                    lappend runs [list $cur [_tags $bold $italic $under $strike $colour]]
                    set cur ""
                }
            }
        }
        switch -- $code {
            2  { set bold [expr {!$bold}] ; incr i ; continue }
            29 { set italic [expr {!$italic}] ; incr i ; continue }
            31 { set under [expr {!$under}] ; incr i ; continue }
            30 { set strike [expr {!$strike}] ; incr i ; continue }
            15 {
                set bold 0 ; set italic 0 ; set under 0 ; set strike 0
                set colour "" ; incr i ; continue
            }
            3 {
                incr i
                # \x03 with no digits turns colour OFF.
                set digits ""
                while {$i < $n && [string is digit [string index $s $i]]
                       && [string length $digits] < 2} {
                    append digits [string index $s $i] ; incr i
                }
                if {$digits eq ""} { set colour "" ; continue }
                set colour [_mirc_colour [scan $digits %d]]
                # A background may follow after a comma; parsed so it is
                # not printed, but not rendered — a background colour in
                # a themed transcript is how you get unreadable text.
                if {[string index $s $i] eq "," &&
                    [string is digit [string index $s [expr {$i+1}]]]} {
                    incr i
                    set bg ""
                    while {$i < $n && [string is digit [string index $s $i]]
                           && [string length $bg] < 2} {
                        append bg [string index $s $i] ; incr i
                    }
                }
                continue
            }
        }
        # Any other C0 control byte is dropped, not shown.
        if {$code < 32} { incr i ; continue }
        append cur $c
        incr i
    }
    if {$cur ne ""} {
        lappend runs [list $cur [_tags $bold $italic $under $strike $colour]]
    }
    return $runs
}

proc ::virc::_tags {bold italic under strike colour} {
    set t {}
    if {$bold}   { lappend t irc_bold }
    if {$italic} { lappend t irc_italic }
    if {$under}  { lappend t irc_under }
    if {$strike} { lappend t irc_strike }
    if {$colour ne ""} { lappend t irc_fg_$colour }
    return $t
}

# Formatting removed entirely — for the buffer list, notifications and
# anything that wants the words without the styling.
proc ::virc::strip_formatting {s} {
    set out ""
    foreach run [parse_formatting $s] { append out [lindex $run 0] }
    return $out
}

# ── rendering one line ──────────────────────────────────────────────

# A stable colour per nick, so the same person looks the same all
# session. Hashed rather than assigned in arrival order: arrival order
# differs between a live view and a replayed backlog, and a nick
# changing colour when you scroll up is disorienting.
proc ::virc::nick_colour {nick} {
    set palette {irc_nick0 irc_nick1 irc_nick2 irc_nick3 irc_nick4
                 irc_nick5 irc_nick6 irc_nick7}
    set h [zlib crc32 [string tolower $nick]]
    return [lindex $palette [expr {$h % [llength $palette]}]]
}

proc ::virc::_time {ts} {
    # The server sends RFC3339; show wall-clock only.
    if {[regexp {T(\d\d:\d\d)} $ts -> hm]} { return $hm }
    if {[regexp {^(\d\d:\d\d)} $ts -> hm]} { return $hm }
    return "  :  "
}

# Insert one line into the transcript. `line` is the API's dict.
proc ::virc::render_line {w line} {
    if {![winfo exists $w]} { return }
    set kind [dict get $line kind]
    set nick ""
    catch { set nick [dict get $line nick] }
    set text ""
    catch { set text [dict get $line text] }
    set ts ""
    catch { set ts [dict get $line ts] }
    # Smart filter hides join/part/quit/nick/mode from nicks that never
    # spoke in this buffer (weechat's smart_filter, always on unless
    # ::virc::smart_filter is cleared).
    if {[_is_filtered $line]} { return }
    # A highlight tags the line so it stands out. Own echoes and
    # join/part noise never ask for it.
    set hl [_is_highlight $line]
    set hltag [expr {$hl ? {irc_highlight} : {}}]

    # Bare display (weechat's /window bare): no timestamp, no glyphs, so
    # a selection pasted elsewhere reads as a conversation rather than
    # as a screenshot of a client. Per-buffer, because the buffer you
    # copy out of is not usually the one you live in.
    variable current
    set bare [_bufopt_get $current bare 0]

    $w configure -state normal
    if {!$bare} { $w insert end "[_time $ts] " irc_time }

    switch -- $kind {
        privmsg {
            $w insert end "<" irc_punct
            $w insert end $nick [concat [list [nick_colour $nick]] $hltag]
            $w insert end "> " irc_punct
            _insert_formatted $w $text $hltag
        }
        action {
            $w insert end "* " irc_punct
            $w insert end $nick [concat [list [nick_colour $nick]] $hltag]
            $w insert end " "
            _insert_formatted $w $text $hltag
        }
        notice {
            $w insert end "-" irc_punct
            $w insert end $nick [concat [list [nick_colour $nick]] $hltag]
            $w insert end "- " irc_punct
            _insert_formatted $w $text $hltag
        }
        join   { _event_line $w "→" "$nick joined" }
        part   { _event_line $w "←" [string trim "$nick left $text"] }
        quit   { _event_line $w "←" [string trim "$nick quit $text"] }
        nick   { _event_line $w "•" "$nick is now $text" }
        topic  { _event_line $w "•" "$nick set the topic: [strip_formatting $text]" }
        mode   { _event_line $w "•" "$nick set mode $text" }
        kick   { _event_line $w "←" "$nick kicked $text" }
        connected    { _event_line $w "●" $text }
        disconnected { _event_line $w "○" $text }
        connecting   { _event_line $w "◌" $text }
        info      {
            # Synthesized introspection answers (whois, ban list, topic
            # queries). `nick` is only set when the line is about us
            # (away confirmations); otherwise it is server talk.
            if {$nick ne ""} {
                _event_line $w "·" "$nick: $text"
            } else {
                _event_line $w "·" $text
            }
        }
        default      { _insert_formatted $w $text }
    }
    $w insert end "\n"
    $w configure -state disabled
    $w see end
}

proc ::virc::_event_line {w glyph text} {
    $w insert end "$glyph " irc_meta
    $w insert end $text irc_meta
}

proc ::virc::_insert_formatted {w text {extra {}}} {
    foreach run [parse_formatting $text] {
        lassign $run chunk tags
        if {$extra ne ""} { lappend tags {*}$extra }
        $w insert end $chunk $tags
    }
}

# ── quoted insert (C-v) ─────────────────────────────────────────────
#
# IRC formatting is control BYTES — \x02 bold, \x03 colour, \x1F
# underline, \x0F reset — and there is no way to type one on a keyboard
# that a text widget will not swallow as a shortcut first. So C-v arms,
# and the next keystroke goes in as its literal character: C-v Ctrl-C
# produces a real \x03.
#
# WHAT IT COSTS. Tk generates <<Paste>> from <Control-Key-v>, so this
# composer no longer pastes on C-v. It still pastes on C-y and on
# Shift-Insert, which Tk also generates <<Paste>> from — and C-y is the
# emacs spelling, so in emacs mode this is the binding that was already
# right. The registry documents both, because a composer that behaves
# differently from the one above it has to say so.
#
# RENDERING THE INVISIBLE. A control byte occupies no space and draws
# nothing, so a composer holding formatting would look empty. Inserted
# codes get a tag with a visible background: the byte is untouched, it
# just stops being invisible.

proc ::virc::quoted_insert {} {
    variable quoting 1
    catch { ::vstatus::set_msg [::vi18n::mc {^ — next key is inserted literally}] }
    return -code break
}

# Runs for every keypress in the composer, and does nothing at all
# unless armed — ordinary typing is untouched.
proc ::virc::quoted_key {w ch} {
    variable quoting
    if {!$quoting} { return }
    # A bare modifier has no character of its own and arrives as its own
    # event, so stay armed through it or C-v Ctrl-C could never work.
    if {$ch eq ""} { return -code break }
    set quoting 0
    if {[winfo exists $w]} {
        $w insert insert $ch
        mark_controls $w
    }
    catch { ::vstatus::set_msg [format [::vi18n::mc {inserted %s}] [describe_ctrl $ch]] }
    return -code break
}

# A readable name for a control byte. "^C" alone does not say what it
# will do to the message.
proc ::virc::describe_ctrl {ch} {
    set code [scan $ch %c]
    if {$code eq "" || $code >= 32} { return $ch }
    set names {2 "^B bold" 3 "^C colour" 15 "^O reset"
               29 "^] italic" 30 "^^ strike" 31 "^_ underline"}
    if {[dict exists $names $code]} { return [dict get $names $code] }
    return [format "^%c" [expr {$code + 64}]]
}

# Tag every control byte in the widget so it is visible. Cheap — the
# composer holds one message, not a transcript — and it catches pasted
# codes too, not only the ones C-v put there.
proc ::virc::mark_controls {w} {
    if {![winfo exists $w]} { return }
    catch { $w tag remove irc_ctrl 1.0 end }
    set text ""
    catch { set text [$w get 1.0 end-1c] }
    set n [string length $text]
    for {set i 0} {$i < $n} {incr i} {
        set code [scan [string index $text $i] %c]
        if {$code ne "" && $code < 32 && $code != 10} {
            catch { $w tag add irc_ctrl "1.0 + $i chars" "1.0 + [expr {$i + 1}] chars" }
        }
    }
}

# ── the event stream ────────────────────────────────────────────────

proc ::virc::connect {} {
    variable stream
    variable cursor
    variable live 0
    # While torn down for an attach switch, no reconnects: a pending
    # backoff would fire into the NEW gateway with the OLD cursor.
    if {[info exists ::virc::disconnected] && $::virc::disconnected} {
        return ""
    }
    # Closing a stream that is already dead is the normal case here —
    # this runs on every reconnect, and the usual reason to reconnect is
    # that the socket already went away. The error is bound and dropped
    # rather than left unbound, so "we chose to ignore this" is written
    # down instead of inferred.
    if {$stream ne ""} {
        catch { ::vapi::sse_close $stream } _already_closed
        set stream ""
    }
    set path /api/irc/events
    # Resume from the last DURABLE line rendered. Zero means "the store
    # from the beginning" would be wrong for a first run — the state
    # call gives us latest_id for that — so 0 here means "live only".
    if {$cursor > 0} { append path "?since=$cursor" }
    if {[catch { ::vapi::sse_open $path ::virc::on_event } tok]} {
        # Say why. A stream that cannot be opened retries quietly
        # forever otherwise, and "IRC shows nothing" is indistinguishable
        # from "nobody is talking".
        catch { ::vstatus::set_msg [format [::vi18n::mc {irc: event stream unavailable: %s}] $tok] }
        _schedule_retry
        return ""
    }
    set stream $tok
    return $tok
}

proc ::virc::_schedule_retry {} {
    variable retry
    variable retry_after
    # RECORD THE TIMER ID. `disconnect` cancels it on an attach switch,
    # and for a while it could not: this was a bare `after $retry
    # ::virc::connect` whose id went nowhere, so `disconnect`'s
    # `after cancel $retry_after` tested a variable nothing ever set and
    # silently did nothing. The `disconnected` flag caught the fallout
    # (connect no-ops while torn down), but the timer still survived the
    # switch and fired one spurious reconnect into the NEW gateway the
    # moment `reconnect` cleared the flag.
    if {[info exists retry_after] && $retry_after ne ""} {
        catch { after cancel $retry_after }
    }
    set retry_after [after $retry ::virc::connect]
    # Back off to a minute. A gateway that is down stays down for a
    # while, and a tight retry loop against a dead socket is how a
    # desktop client burns a core.
    set retry [expr {min($retry * 2, 60000)}]
}

# ── attach teardown/rebuild helpers ─────────────────────────────────
#
# Called by ::vapp::teardown_for_attach (lib/attach.tcl). IRC holds
# state that is meaningless against a different gateway: the event
# stream (captured url+token at open time) and the cursor (line ids
# are per-STORE, so the old cursor silently skips or re-renders lines).

proc ::virc::disconnect {} {
    variable stream
    variable retry_after
    if {$stream ne ""} {
        catch { ::vapi::sse_close $stream } _already_closed
        set stream ""
    }
    # Stop the reconnect backoff too, or a pending `after` reconnects
    # straight into the NEW gateway's url with the OLD cursor.
    # _schedule_retry records its id for exactly this cancel.
    if {[info exists retry_after] && $retry_after ne ""} {
        catch { after cancel $retry_after }
        set retry_after ""
    }
    # Belt and braces: the cancel above closes the window, and the flag
    # closes the one case it cannot — a timer that already fired and is
    # sitting in the event queue. `connect` no-ops while disconnected,
    # and `reconnect` re-arms on attach.
    variable disconnected 1
}

proc ::virc::reset_cursor {} {
    variable cursor
    set cursor 0
}

# Re-arm after an attach: fresh snapshot, fresh stream, cursor reset.
proc ::virc::reconnect {} {
    variable disconnected
    variable why
    set disconnected 0
    set why ""
    refresh_state
    connect
}

proc ::virc::on_event {ev d} {
    variable live
    variable cursor
    variable retry
    switch -- $ev {
        irc {
            set retry 1000
            _ingest $d
        }
        irc-live {
            # The replay is done; everything from here is happening now.
            set live 1
            set retry 1000
        }
        irc-lagged {
            # The server dropped events for us rather than pretending.
            # The store still has them, so reconnecting from the cursor
            # refetches exactly what was missed.
            catch { ::vstatus::set_msg [::vi18n::mc {irc: stream fell behind, refetching}] }
            connect
        }
        closed {
            variable stream
            # A stream WE reset is not a drop — the api layer silences
            # reset-ends for exactly this reason (http::reset runs the
            # -command from inside our own teardown, which used to arm a
            # phantom reconnect storm); same rule here, consumer side.
            set st ""
            catch { set st [dict get $d status] }
            if {$st eq "reset"} { return }
            # Only forget the CURRENT stream — a stale token's late
            # close must not clobber the one that is live now.
            set tok ""
            catch { set tok [dict get $d token] }
            if {$tok eq "" || $stream eq $tok} { set stream "" }
            # A rejected token is not a transient outage. Retrying it on
            # a backoff produces exactly the failure this was written to
            # end: an IRC pane that shows nothing, forever, silently.
            set code 0
            catch { set code [dict get $d code] }
            if {[::vapi::fatal_code $code]} {
                set why "irc: [::vapi::http_reason $code]"
                if {[catch { ::vstatus::set_msg $why } report_err]} {
                    catch { puts stderr "$why ($report_err)" }
                }
                return
            }
            _schedule_retry
        }
    }
}

# One line, from either half of the stream.
proc ::virc::_ingest {d} {
    variable cursor
    variable current
    variable unread
    variable activity
    variable seen
    variable lasttouch
    variable read_marker
    variable meta
    if {![dict exists $d buffer]} { return }
    # Typing indicators are EPHEMERAL: they never touch the cursor, the
    # unread count, the hotlist or the transcript. They update the
    # indicator state and stop — everything below is for scrollback.
    set kind "" ; catch { set kind [dict get $d kind] }
    if {$kind eq "typing"} {
        typing_on_event $d
        return
    }
    set network "" ; catch { set network [dict get $d network] }
    set presence "" ; catch { set presence [dict get $d presence] }
    set buffer [dict get $d buffer]
    set k [key $network $presence $buffer]

    # Connection lifecycle drives the status row under the topic, no
    # matter which buffer of this network is on screen at the time.
    if {$kind in {connected disconnected connecting}} {
        variable netstate
        set ltext ""
        catch { set ltext [dict get $d text] }
        set netstate($network/$presence) [dict create kind $kind text $ltext]
        _refresh_status
    }

    # Only replayed lines carry a durable id; live ones have not been
    # written yet. So the cursor tracks the last STORED line — which is
    # exactly what a resume needs, and why it must not be advanced from
    # a live event that has no id to resume from.
    if {[dict exists $d id]} {
        set id [dict get $d id]
        if {[string is integer -strict $id] && $id > $cursor} { set cursor $id }
        set seen($k) $id
    }

    _note_buffer $k $network $presence $buffer

    # Raise the buffer's activity to the level of this line (a highlight
    # escalates a message, a message escalates join/part noise; nothing
    # ever demotes).
    set lvl [_activity_level $k $d]
    if {$lvl > [_activity_of $k]} { set activity($k) $lvl }

    # Per-buffer nick recency, for Tab completion.
    set nick ""
    catch { set nick [dict get $d nick] }
    if {$nick ne ""} { _bump_recency $k $nick }

    # Keep the roster current. Nothing on the event stream announces a
    # roster change — there is no state event — so the pane applies the
    # delta itself and repaints only if the buffer on screen moved.
    # Called ONCE and its result kept: `_roster_apply` mutates, so
    # calling it again to ask what it did would apply the delta twice.
    if {$current in [_roster_apply $d]} { _refresh_roster $current }

    # A server line ALSO goes to the merged buffer when merging is on.
    # Fanned out here rather than rewritten at the source, so `*` per
    # network keeps working for anyone who prefers it — and so turning
    # merging off does not need a refetch.
    variable merged
    variable linebuf
    if {$merged && [_is_server_line $d]} {
        set mk [merged_key]
        _note_buffer $mk "*" "*" "*"
        if {![info exists linebuf($mk)]} { set linebuf($mk) {} }
        lappend linebuf($mk) $d
        foreach w [_targets_for $mk] { render_line $w $d }
        if {$mk ne $current} {
            if {![info exists unread($mk)]} { set unread($mk) 0 }
            incr unread($mk)
            if {$lvl > [_activity_of $mk]} { set activity($mk) $lvl }
        }
    }

    # Every pane showing this buffer: the primary when it is current,
    # plus any split pinned to it. A split keeps drawing whether or not
    # the buffer is the one on the composer, which is the whole point.
    foreach w [_targets_for $k] {
        if {$w eq [transcript]} { continue }
        render_line $w $d
        catch { $w see end }
    }

    if {$k eq $current} {
        render_line [transcript] $d
        # On screen, so read immediately: the count and level reset in
        # memory. The read marker persists on the NEXT buffer switch, not
        # once per line.
        set unread($k) 0
        set activity($k) 0
        if {[info exists seen($k)]} { set read_marker($k) $seen($k) }
    } else {
        # The sort tiebreak: when two buffers share a level, the one with
        # fresher unread activity floats up.
        if {$lvl > 0} { set lasttouch($k) [clock milliseconds] }
        if {![info exists unread($k)]} { set unread($k) 0 }
        incr unread($k)
        # Notebook the reader if they are not looking: highlight or PM.
        if {$lvl > 0} { _maybe_notify $d $lvl }
        _refresh_buffers
    }
}

# The activity level this line deserves. 0 = our own echo, 1 = channel
# noise (join/part/quit/nick/mode), 2 = a message, 3 = a highlight.
proc ::virc::_activity_level {k d} {
    set kind ""
    catch { set kind [dict get $d kind] }
    switch -exact -- $kind {
        privmsg - action - notice {
            if {[dict exists $d own] && [dict get $d own]} { return 0 }
            if {[_is_highlight $d]} { return 3 }
            return 2
        }
        join - part - quit - nick - mode - topic - kick { return 1 }
        info {
            if {[dict exists $d own] && [dict get $d own]} { return 0 }
            return 1
        }
        connected - disconnected - connecting { return 0 }
        default { return 2 }
    }
}

proc ::virc::_activity_of {k} {
    variable activity
    if {[info exists activity($k)]} { return $activity($k) }
    return 0
}

# Does this line highlight the reader? Own nick (per network/presence)
# or any configured extra word, matched on token boundaries so "jackma"
# does not fire on "jackman".
proc ::virc::_is_highlight {d} {
    variable own_nick
    variable highlight_words
    set network "" ; catch { set network [dict get $d network] }
    set presence "" ; catch { set presence [dict get $d presence] }
    set text ""
    catch { set text [dict get $d text] }
    if {$text eq ""} { return 0 }
    set nick ""
    if {[info exists own_nick($network/$presence)]} {
        set nick $own_nick($network/$presence)
    }
    if {$nick ne "" && [_mentions $text $nick]} { return 1 }
    foreach w $highlight_words {
        if {$w ne "" && [_mentions $text $w]} { return 1 }
    }
    return 0
}

# WORD appears in TEXT as IRC would highlight it: its own token, with
# punctuation allowed on either side but no alphanumeric adjoining it.
proc ::virc::_mentions {text word} {
    if {$word eq ""} { return 0 }
    set low [string tolower [strip_formatting $text]]
    set w [string tolower $word]
    set pos [string first $w $low]
    while {$pos >= 0} {
        set before [expr {$pos > 0 ? [string index $low [expr {$pos - 1}]] : " "}]
        set after [string index $low [expr {$pos + [string length $w]}]]
        if {![string match {[A-Za-z0-9]} $before]
            && ![string match {[A-Za-z0-9]} $after]} {
            return 1
        }
        set pos [string first $w $low [expr {$pos + 1}]]
    }
    return 0
}

proc ::virc::_bump_recency {k nick} {
    variable recency
    if {![info exists recency($k)]} { set recency($k) {} }
    # Drop any existing (case-insensitive) occurrence, then put this nick
    # on top so it is the first thing Tab offers.
    set rest [lsearch -all -not -inline -exact -nocase $recency($k) $nick]
    set recency($k) [lrange [linsert $rest 0 $nick] 0 49]
}

# Would the smart filter hide this line? join/part/quit/nick/mode from a
# nick that has never spoken (this session) is noise on a big channel.
# ── /ignore ─────────────────────────────────────────────────────────
#
# A glob against the SENDER, matched case-insensitively. Patterns rather
# than plain nicks because the thing people actually want to silence is
# a botnet spelling itself `spam-4471`, and typing eight /ignore lines
# for that is how you decide not to bother.
#
# Matched against both the bare nick and `nick!user@host` so a hostmask
# pattern works whether or not userhost-in-names is negotiated. The
# roster holds masks when it is; the event stream carries bare nicks
# either way, so a pattern that only matched one of them would silence
# the roster and not the transcript.
proc ::virc::_is_ignored {line} {
    variable ignores
    if {![llength $ignores]} { return 0 }
    set nick "" ; catch { set nick [dict get $line nick] }
    if {$nick eq ""} { return 0 }
    set host "" ; catch { set host [dict get $line host] }
    foreach pat $ignores {
        if {[string match -nocase $pat $nick]} { return 1 }
        if {$host ne "" && [string match -nocase $pat "$nick!$host"]} { return 1 }
    }
    return 0
}

proc ::virc::_is_filtered {line} {
    variable smart_filter
    variable recency
    # An ignore outranks everything, including the smart filter's own
    # "but they spoke recently" exemption — that exemption exists to
    # keep people you care about visible, which is the opposite of this.
    if {[_is_ignored $line]} { return 1 }
    set kind ""
    catch { set kind [dict get $line kind] }
    if {$kind ni {join part quit nick mode}} { return 0 }
    set network "" ; catch { set network [dict get $line network] }
    set presence "" ; catch { set presence [dict get $line presence] }
    set buffer "" ; catch { set buffer [dict get $line buffer] }
    set k [key $network $presence $buffer]
    # Per-buffer override, then the global default. A filter you want on
    # in a channel of four hundred and off in a channel of four is the
    # whole reason weechat has per-buffer settings.
    if {![_bufopt_get $k filter $smart_filter]} { return 0 }
    # A nick change names the new nick in `text`, but the speaker is the
    # old one — both count as "this nick".
    set name ""
    catch { set name [dict get $line nick] }
    if {$name eq ""} { return 0 }
    if {[info exists recency($k)]
        && [lsearch -exact -nocase $recency($k) $name] >= 0} {
        return 0
    }
    return 1
}

proc ::virc::_note_buffer {k network presence buffer} {
    variable buffers
    variable meta
    if {$k in $buffers} { return }
    lappend buffers $k
    set meta($k) [dict create network $network presence $presence buffer $buffer]
    _refresh_buffers
}

# ── sending ─────────────────────────────────────────────────────────
#
# Through the gateway, never straight to IRC: the gateway holds the
# connection, which is the whole point of the bouncer.
#
# The text goes out VERBATIM, control bytes and all. Rendering mIRC
# codes as markup on the way IN is right; doing the reverse to what a
# human typed would mangle any message that merely contains an asterisk.

proc ::virc::send_current {} {
    variable current
    variable meta
    variable R_view
    if {![winfo exists $R_view.input]} { return 0 }
    set text [string trimright [$R_view.input get 1.0 end] "\n"]
    if {$text eq ""} { return 0 }
    if {$current eq "" || ![info exists meta($current)]} {
        catch { ::vstatus::set_msg [::vi18n::mc {no IRC buffer selected}] }
        return 0
    }
    set m $meta($current)

    # Slash commands. `//` sends a literal leading slash; anything else
    # starting with `/` is dispatched. `_slash` acts or refuses; the
    # composer clears either way, because a refused unknown command must
    # never leak into the channel as speech.
    if {[string match "/*" $text]} {
        if {[string match "//*" $text]} {
            set text [string range $text 1 end]
        } else {
            _slash [string range $text 1 end]
            $R_view.input delete 1.0 end
            return 1
        }
    }

    # Paste guard: a multi-line send (a pasted stack trace) confirms before
    # it goes out. One accidental wall-paste is one too many.
    if {[string first "\n" $text] >= 0 && ![::virc::_confirm_multiline $text]} {
        return 0
    }

    if {![send [dict get $m network] [dict get $m presence] \
              [dict get $m buffer] $text privmsg]} {
        return 0
    }
    # The message went out; stop announcing that we type. Cancelling the
    # idle timer AND sending done covers both orders — a timer that was
    # about to fire would otherwise re-announce after the message.
    variable typing_out_after
    catch { after cancel $typing_out_after($current) }
    _send_typing [dict get $m network] [dict get $m presence] \
        [dict get $m buffer] done
    $R_view.input delete 1.0 end
    return 1
}

# ── slash commands ──────────────────────────────────────────────────
#
# A dispatch table, not a chain of `string match`. Two rules that matter
# more than the list: an unknown /foo REFUSES and says so (it must never
# fall through to PRIVMSG, which is how `/join #foo` used to land in the
# channel as speech), and `//text` sends a literal leading slash.

proc ::virc::_slash {line} {
    # Aliases resolve BEFORE dispatch and exactly once — see
    # `_expand_alias` for why a fixpoint loop is the wrong shape here.
    # A built-in name cannot be aliased, so this can never redirect
    # /part into something else.
    set line [_expand_alias $line]
    set words [split $line " "]
    set cmd [string tolower [lindex $words 0]]
    set rest [string trimleft [string range $line [string length $cmd] end]]
    switch -exact -- $cmd {
        help   { _slash_help }
        ignore   { _slash_ignore $rest }
        unignore { _slash_unignore $rest }
        ignores  { _slash_ignores $rest }
        alias    { _slash_alias $rest }
        set      { _slash_set $rest }
        bare     { _slash_bare $rest }
        split    { _slash_split $rest }
        unsplit  { _slash_unsplit $rest }
        merge    { _slash_merge $rest }
        bans     { _slash_bans $rest }
        join   { _slash_join $rest }
        part   { _slash_part $rest }
        query  { _slash_query $rest }
        msg    { _slash_msg $rest }
        nick   { _slash_nick $rest }
        topic  { _slash_topic $rest }
        whois  { _slash_whois $rest }
        who    { _slash_who $rest }
        names  { _slash_names $rest }
        away   { _slash_away $rest }
        me     { _slash_me $rest }
        notice { _slash_notice $rest }
        raw    { _slash_raw $rest }
        quote  { _slash_raw $rest }
        close  { _slash_close }
        lastlog { _slash_lastlog $rest }
        urls   { _slash_urls $rest }
        url    { _slash_url $rest }
        acl    { _slash_acl }
        default {
            if {$cmd ne ""} {
                catch { ::vstatus::set_msg [format [::vi18n::mc {unknown command /%s — /help lists them}] $cmd] }
            }
        }
    }
}

proc ::virc::_slash_help {} {
    set w [transcript]
    if {![winfo exists $w]} { return }
    $w configure -state normal
    foreach line [_irc_help] {
        $w insert end "$line\n" irc_meta
    }
    $w configure -state disabled
    $w see end
}

proc ::virc::_irc_help {} {
    return [list \
        [::vi18n::mc {commands:}] \
        {  /join #chan [key]   /part [reason]   /query nick   /msg nick text} \
        {  /nick new  /topic [text]  /whois nick  /away [msg]  /names [chan]  /who [mask]} \
        {  /me action  /notice target text  /raw line (confirms)  /quote line} \
        {  /close  /help  /lastlog text  /urls  /url N} \
        {  /ignore pattern  /unignore pattern  /ignores} \
        {  /alias name expansion ($1..$9, $*)  /alias name (removes)} \
        {  /set [name on|off]  /bare} \
        {  /split [buffer]  /unsplit [buffer]  /merge [on|off]} \
        {  /bans [channel] — view the ban list} \
        {  /acl — manage identities and access from this tab} \
        "  //text sends a literal leading slash"]
}

proc ::virc::_slash_join {rest} {
    set chan [string trim [lindex [split $rest " "] 0]]
    if {$chan eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {/join needs a channel}] }
        return
    }
    set key [string trim [lindex [split $rest " "] 1]]
    set args [list action join channel $chan]
    if {$key ne ""} { lappend args key $key }
    op_here {*}$args
}

proc ::virc::_slash_part {rest} {
    set reason [string trim $rest]
    set chan [current_channel]
    if {![string match "#*" $chan] && ![string match "&*" $chan]} {
        catch { ::vstatus::set_msg [::vi18n::mc {/part needs a channel}] }
        return
    }
    set args [list action part channel $chan]
    if {$reason ne ""} { lappend args reason $reason }
    op_here {*}$args
}

proc ::virc::_slash_query {rest} {
    variable current
    variable meta
    set nick [string trim [lindex [split $rest " "] 0]]
    if {$nick eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {/query needs a nick}] }
        return
    }
    if {$current eq "" || ![info exists meta($current)]} { return }
    set m $meta($current)
    set k [key [dict get $m network] [dict get $m presence] $nick]
    _note_buffer $k [dict get $m network] [dict get $m presence] $nick
    show $k
}

proc ::virc::_slash_msg {rest} {
    variable current
    variable meta
    set nick [string trim [lindex [split $rest " "] 0]]
    set text [string trimleft [string range $rest [string length $nick] end]]
    if {$nick eq "" || $text eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {/msg needs a nick and text}] }
        return
    }
    if {$current eq "" || ![info exists meta($current)]} { return }
    set m $meta($current)
    send [dict get $m network] [dict get $m presence] $nick $text privmsg
}

proc ::virc::_slash_nick {rest} {
    set nick [string trim [lindex [split $rest " "] 0]]
    if {$nick eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {/nick needs a nick}] }
        return
    }
    op_here action nick nick $nick
}

# The buffer a channel-scoped introspection answer should land in: the
# current channel when we are ON one, "" (the server buffer) otherwise.
# The merged buffer is a synthetic VIEW over all networks — it is never
# a valid scope, because no single connection owns it.
proc ::virc::info_scope {} {
    set c [current_channel]
    if {![string match "#*" $c] && ![string match "&*" $c]} { return "" }
    return $c
}

proc ::virc::_slash_topic {rest} {
    set chan [current_channel]
    if {![string match "#*" $chan] && ![string match "&*" $chan]} {
        catch { ::vstatus::set_msg [::vi18n::mc {/topic needs a channel}] }
        return
    }
    set args [list action topic channel $chan]
    set topic [string trim $rest]
    if {$topic ne ""} {
        lappend args topic $topic
    } else {
        # A QUERY: its answer comes back as an Info event published into
        # this buffer, not as a status whisper.
        lappend args scope $chan
    }
    op_here {*}$args
}

proc ::virc::_slash_whois {rest} {
    set nick [string trim [lindex [split $rest " "] 0]]
    if {$nick eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {/whois needs a nick}] }
        return
    }
    set args [list action whois nick $nick]
    set sc [info_scope]
    if {$sc ne ""} { lappend args scope $sc }
    op_here {*}$args
}

# /names [channel] — the roster from the pane's own member tracking, no
# wire query. /names * lists every tracked channel with a count.
proc ::virc::_slash_names {{rest ""}} {
    variable rosters
    variable meta
    set chan [string trim $rest]
    if {$chan eq ""} { set chan [current_channel] }
    if {$chan ne "" && $chan ne "*"} {
        set chosen ""
        foreach k [array names rosters] {
            set parts [split $k "/"]
            if {[lindex $parts 2] ne "" && [string equal -nocase [lindex $parts 2] $chan]} {
                set chosen $k
                break
            }
        }
        if {$chosen eq ""} {
            set txt "$chan: roster not synced (or no members)"
        } else {
            set roster $rosters($chosen)
            if {[llength $roster]} {
                set txt "$chan: [llength $roster] member(s)\n[join $roster " "]"
            } else {
                set txt "$chan: no members"
            }
        }
    } else {
        set lines {}
        foreach k [lsort [array names rosters]] {
            set parts [split $k "/"]
            set nm [lindex $parts 2]
            if {$nm eq ""} { set nm $k }
            lappend lines "$nm: [llength $rosters($k)]"
        }
        if {![llength $lines]} { set lines {{no rosters tracked}} }
        set txt "[join $lines "\n"]"
    }
    _local_info $txt
}

# /who [mask] — WHO query on the wire; rows land in this buffer as an
# Info event when the server's 315 closes the reply.
proc ::virc::_slash_who {{rest ""}} {
    set target [string trim $rest]
    if {$target eq ""} { set target [current_channel] }
    if {$target eq ""} { set target "*" }
    set args [list action who target $target]
    set sc [info_scope]
    if {$sc ne ""} { lappend args scope $sc }
    op_here {*}$args
}

# Insert a pane-local Info-style line into the CURRENT buffer's
# transcripts (roster dumps etc.), styled like the engine's own info
# events so the two are indistinguishable in scrollback.
proc ::virc::_local_info {txt} {
    variable current
    variable meta
    if {$current eq ""} { return }
    set network ""; set presence ""; set buffer $current
    if {[dict exists $meta $current]} {
        set d [dict get $meta $current]
        catch { set network [dict get $d network] }
        catch { set presence [dict get $d presence] }
        catch { set buffer [dict get $d buffer] }
    }
    set ev [dict create kind info network $network presence $presence \
        buffer $buffer nick "" text $txt ts [clock seconds] own 1]
    foreach w [_targets_for $current] { render_line $w $ev }
}

proc ::virc::_slash_away {rest} {
    set msg [string trim $rest]
    set args [list action away]
    if {$msg ne ""} { lappend args message $msg }
    op_here {*}$args
}

proc ::virc::_slash_me {rest} {
    variable current
    variable meta
    if {$rest eq ""} { return }
    if {$current eq "" || ![info exists meta($current)]} { return }
    set m $meta($current)
    send [dict get $m network] [dict get $m presence] [dict get $m buffer] $rest action
}

proc ::virc::_slash_notice {rest} {
    variable current
    variable meta
    set target [string trim [lindex [split $rest " "] 0]]
    set text [string trimleft [string range $rest [string length $target] end]]
    if {$target eq "" || $text eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {/notice needs a target and text}] }
        return
    }
    if {$current eq "" || ![info exists meta($current)]} { return }
    set m $meta($current)
    send [dict get $m network] [dict get $m presence] $target $text notice
}

proc ::virc::_slash_raw {rest} {
    set line [string trim $rest]
    if {$line eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {/raw needs a line}] }
        return
    }
    confirm_op [format [::vi18n::mc {Send raw line to the server? (%s)}] $line] action raw line $line
}

proc ::virc::_slash_close {} {
    variable buffers
    variable current
    variable R_view
    if {$current eq ""} { return }
    set i [lsearch -exact $buffers $current]
    if {$i < 0} { return }
    # close is a hide, not a part: the channel/query stays connected, it
    # is just removed from the list until the next line reopens it.
    set buffers [lreplace $buffers $i $i]
    catch { unset meta($current) }
    _refresh_buffers
    if {[llength $buffers]} {
        set idx [expr {$i >= [llength $buffers] ? [llength $buffers] - 1 : $i}]
        show [lindex $buffers $idx]
    } else {
        set current ""
        if {[winfo exists $R_view.body]} {
            $R_view.body configure -state normal
            $R_view.body delete 1.0 end
            $R_view.body configure -state disabled
        }
    }
}

# ── Tier 2: paste guard, notifications, URL grabbing, /lastlog ──────

# Ask before a multi-line send. A pasted stack trace into a channel is a
# thing people do exactly once, and this is the guard that stops them.
proc ::virc::_confirm_multiline {text} {
    set n [llength [split [string trimright $text "\n"] "\n"]]
    return [expr {[tk_messageBox -type yesno -icon warning \
        -title [::vi18n::mc {IRC}] \
        -message [format [::vi18n::mc {Send a %d-line message to the channel?}] $n]] eq "yes"}]
}

# Desktop notification, fire-and-forget, and only when a notification
# daemon is actually present (the way media.tcl guards its converters).
proc ::virc::_notify {title body} {
    variable notify_path
    if {![info exists notify_path]} {
        set notify_path [lindex [auto_execok notify-send] 0]
        if {$notify_path eq ""} { set notify_path "" }
    }
    if {$notify_path eq ""} { return }
    # `$title` is a NICK and `$body` is MESSAGE TEXT — both straight off
    # the wire, and a private message notifies without needing any
    # channel in common. Handed to a bare `exec` they stopped being
    # arguments and became grammar: a body of ">/path" truncated that
    # file, and a nick of "|" (legal under RFC 2812) turned the body
    # into a command that ran. safeexec.tcl carries the demonstration
    # and the XChat CVE-2010-2192 precedent.
    ::vsafe::spawn $notify_path $title $body
}

# Who deserves a desktop notification for this line, when it lands in a
# buffer the reader is NOT looking at: a highlight, or a private message.
proc ::virc::_maybe_notify {d lvl} {
    set kind ""
    catch { set kind [dict get $d kind] }
    set buffer ""
    catch { set buffer [dict get $d buffer] }
    set text ""
    catch { set text [dict get $d text] }
    set nick ""
    catch { set nick [dict get $d nick] }
    if {$kind ni {privmsg action notice}} { return }
    set is_pm [expr {![string match "#*" $buffer] && ![string match "&*" $buffer]}]
    if {$lvl == 3 || ($is_pm && $kind eq "privmsg")} {
        _notify [strip_formatting $nick] [strip_formatting $text]
    }
}

# Every http(s) URL in the rendered transcript, de-duplicated.
proc ::virc::_urls_in_buffer {} {
    set w [transcript]
    if {![winfo exists $w]} { return {} }
    set text [$w get 1.0 end-1c]
    return [lsort -unique [regexp -all -inline {https?://[^ \t\n\"<>]+} $text]]
}

proc ::virc::_open_url {url} {
    if {$::tcl_platform(platform) eq "windows"} {
        # The markdown opener already knows `start` and the URL checks.
        return [::vmd::open_url $url]
    }
    set opener [lindex [auto_execok xdg-open] 0]
    if {$opener eq ""} { set opener [lindex [auto_execok open] 0] }
    if {$opener eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {no program to open URLs}] }
        return 0
    }
    # `_urls_in_buffer` already anchors on http(s)://, so the scheme
    # re-check is belt and braces — but that regex is exactly the kind
    # that gets widened later to catch bare www. links, and this is what
    # stops that from becoming a scheme injection.
    set safe [::vsafe::url $url]
    if {$safe eq ""} { return 0 }
    return [::vsafe::spawn $opener $safe]
}

proc ::virc::_slash_urls {rest} {
    set urls [_urls_in_buffer]
    if {![llength $urls]} {
        catch { ::vstatus::set_msg [::vi18n::mc {no URLs in this buffer}] }
        return
    }
    set w [transcript]
    if {[winfo exists $w]} {
        $w configure -state normal
        set i 0
        foreach u $urls {
            $w insert end [format "%2d. %s\n" $i $u] irc_meta
            incr i
        }
        $w configure -state disabled
        $w see end
    }
}

proc ::virc::_slash_url {rest} {
    set n [string trim $rest]
    if {![string is integer -strict $n]} {
        catch { ::vstatus::set_msg [::vi18n::mc {/url needs a number from /urls}] }
        return
    }
    set urls [_urls_in_buffer]
    if {$n < 0 || $n >= [llength $urls]} {
        catch { ::vstatus::set_msg [::vi18n::mc {no such URL — /urls lists them}] }
        return
    }
    _open_url [lindex $urls $n]
}

# Search the rendered transcript and jump to the first hit. This is the
# rendered slice, not the store — scroll further back first for the rest.
proc ::virc::_slash_lastlog {rest} {
    set q [string trim $rest]
    if {$q eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {/lastlog needs text to search for}] }
        return
    }
    set w [transcript]
    if {![winfo exists $w]} { return }
    set matches [::vfind::match_all $w $q]
    if {![llength $matches]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {no match for %s}] $q] }
        return
    }
    ::vfind::highlight $w $matches
}


# ── Tier 3: ignore lists, aliases, per-buffer settings, bare display ─

proc ::virc::_slash_ignore {rest} {
    variable ignores
    set pat [string trim $rest]
    if {$pat eq ""} { return [_slash_ignores ""] }
    if {[lsearch -exact $ignores $pat] >= 0} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {already ignoring %s}] $pat] }
        return
    }
    lappend ignores $pat
    _save_prefs
    catch { ::vstatus::set_msg [format [::vi18n::mc {ignoring %s}] $pat] }
    _repaint_current
}

proc ::virc::_slash_unignore {rest} {
    variable ignores
    set pat [string trim $rest]
    set i [lsearch -exact $ignores $pat]
    if {$i < 0} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {not ignoring %s}] $pat] }
        return
    }
    set ignores [lreplace $ignores $i $i]
    _save_prefs
    catch { ::vstatus::set_msg [format [::vi18n::mc {no longer ignoring %s}] $pat] }
    _repaint_current
}

proc ::virc::_slash_ignores {rest} {
    variable ignores
    if {![llength $ignores]} {
        catch { ::vstatus::set_msg [::vi18n::mc {nothing is ignored}] }
        return
    }
    _meta_lines [linsert $ignores 0 [::vi18n::mc {ignored:}]]
}

# ── aliases ─────────────────────────────────────────────────────────
#
# `/alias j /join` makes `/j #chan` work. The expansion replaces the
# COMMAND WORD only and the rest of the line is appended, which is what
# people expect from `/alias wii /msg NickServ` — the arguments are
# theirs, not the alias's.
#
# $1..$9 and $* are supported because an alias that cannot reorder its
# arguments is not much of an alias. When any of them appears, the
# arguments are consumed by the substitution instead of being appended,
# or `/alias op /mode $1 +o $2` would send the args twice.

proc ::virc::_slash_alias {rest} {
    variable aliases
    set rest [string trim $rest]
    if {$rest eq ""} {
        set out [list [::vi18n::mc {aliases:}]]
        foreach n [lsort [array names aliases]] { lappend out "  /$n → $aliases($n)" }
        if {[llength $out] == 1} { lappend out [::vi18n::mc {  (none)}] }
        _meta_lines $out
        return
    }
    set name [string tolower [lindex [split $rest " "] 0]]
    set name [string trimleft $name "/"]
    set body [string trim [string range $rest [string length [lindex [split $rest " "] 0]] end]]
    if {$name eq ""} { return }
    if {$body eq ""} {
        # `/alias name` with no body removes it — one command for both
        # directions, the way weechat's /alias -del is a second one.
        if {[info exists aliases($name)]} {
            unset aliases($name)
            _save_prefs
            catch { ::vstatus::set_msg [format [::vi18n::mc {alias /%s removed}] $name] }
        } else {
            catch { ::vstatus::set_msg [format [::vi18n::mc {no alias /%s}] $name] }
        }
        return
    }
    # An alias may not shadow a built-in. Letting `/alias join /part`
    # exist would make the most destructive commands in the pane mean
    # whatever a stray keystroke last set them to.
    if {$name in [_builtin_slash_names]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {/%s is built in — pick another name}] $name] }
        return
    }
    set aliases($name) $body
    _save_prefs
    catch { ::vstatus::set_msg [format [::vi18n::mc {alias /%s → %s}] $name $body] }
}

# Expand an alias, once. Returns the line to run.
#
# ONCE, deliberately: `/alias a /b` plus `/alias b /a` is two keystrokes
# away and a fixpoint loop would hang the UI with no way out. One level
# is enough for every real use and cannot spin.
proc ::virc::_expand_alias {line} {
    variable aliases
    set words [split $line " "]
    set cmd [string tolower [lindex $words 0]]
    if {![info exists aliases($cmd)]} { return $line }
    set body $aliases($cmd)
    set args [lrange $words 1 end]
    set used 0
    set out ""
    set n [string length $body]
    for {set i 0} {$i < $n} {incr i} {
        set c [string index $body $i]
        if {$c ne "\$"} { append out $c ; continue }
        set nx [string index $body [expr {$i + 1}]]
        if {$nx eq "*"} {
            append out [join $args " "] ; set used 1 ; incr i
        } elseif {[string is digit -strict $nx] && $nx ne "0"} {
            append out [lindex $args [expr {$nx - 1}]] ; set used 1 ; incr i
        } else {
            append out $c
        }
    }
    if {!$used && [llength $args]} { append out " " [join $args " "] }
    return [string trimleft $out "/"]
}

proc ::virc::_builtin_slash_names {} {
    return {help join part query msg nick topic whois away me notice raw
            quote close lastlog urls url ignore unignore ignores alias
            set bare split unsplit merge bans}
}

# ── per-buffer settings ─────────────────────────────────────────────
#
# `/set` with no argument lists what is set here; `/set name value`
# scopes to the current buffer; `/set name` clears it. The only settings
# that make sense per buffer are the display ones — a smart filter you
# want off in a channel of forty and on in a channel of four hundred is
# the whole reason weechat has per-buffer settings at all.

proc ::virc::_bufopt_get {k name default} {
    variable bufopt
    if {[info exists bufopt($k/$name)]} { return $bufopt($k/$name) }
    return $default
}

proc ::virc::_slash_set {rest} {
    variable bufopt
    variable current
    set rest [string trim $rest]
    if {$current eq ""} { return }
    if {$rest eq ""} {
        set out [list [format [::vi18n::mc {settings for %s:}] [_label $current]]]
        foreach n [lsort [array names bufopt "$current/*"]] {
            lappend out "  [string range $n [expr {[string length $current] + 1}] end] = $bufopt($n)"
        }
        if {[llength $out] == 1} { lappend out [::vi18n::mc {  (none — using the defaults)}] }
        lappend out [::vi18n::mc {  settable: filter, bare}]
        _meta_lines $out
        return
    }
    set name [string tolower [lindex [split $rest " "] 0]]
    set val [string trim [string range $rest [string length $name] end]]
    if {$name ni {filter bare}} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {no setting %s — try filter or bare}] $name] }
        return
    }
    if {$val eq ""} {
        unset -nocomplain bufopt($current/$name)
        catch { ::vstatus::set_msg [format [::vi18n::mc {%s cleared for this buffer}] $name] }
    } else {
        if {$val ni {0 1 on off true false yes no}} {
            catch { ::vstatus::set_msg [format [::vi18n::mc {%s takes on or off}] $name] }
            return
        }
        set bufopt($current/$name) [expr {$val in {1 on true yes} ? 1 : 0}]
        catch { ::vstatus::set_msg [format [::vi18n::mc {%1$s = %2$s here}] $name $val] }
    }
    _save_prefs
    _repaint_current
}

# ── bare display ────────────────────────────────────────────────────
#
# No timestamp, no arrow glyphs — just `<nick> text`, so a selection
# pasted elsewhere reads as a conversation instead of as a screenshot of
# a client. weechat calls this /window bare and binds it to Alt-l.

proc ::virc::_slash_bare {rest} {
    variable current
    variable bufopt
    set now [_bufopt_get $current bare 0]
    set bufopt($current/bare) [expr {!$now}]
    _save_prefs
    _repaint_current
    catch { ::vstatus::set_msg [expr {$bufopt($current/bare)
        ? [::vi18n::mc {bare display on}] : [::vi18n::mc {bare display off}]}] }
}

# ── shared plumbing ─────────────────────────────────────────────────

# Print lines into the transcript as metadata. Four commands wanted
# this and each had its own copy.
proc ::virc::_meta_lines {lines} {
    set w [transcript]
    if {![winfo exists $w]} { return }
    $w configure -state normal
    foreach l $lines { $w insert end "$l\n" irc_meta }
    $w configure -state disabled
    $w see end
}

# Redraw the buffer on screen from the lines already loaded. An ignore,
# or a display setting, that only affected the NEXT line would look
# broken — the whole point of /ignore is the wall of noise already on
# screen.
#
# `_render_transcript`, not `show`: `show` refetches a page from the
# gateway, and a round trip to redraw text we already hold is both slow
# and wrong when the gateway is momentarily unreachable.
#
# And NO `catch`. The first version of this called a proc that does not
# exist — `select_buffer`, when the proc is `show` — inside a bare
# catch, so /ignore, /set and /bare silently failed to repaint and the
# suite stayed green because the tests redrew by hand. Swallowing the
# error is the only reason that survived review.
proc ::virc::_repaint_current {} {
    variable current
    if {$current eq ""} { return }
    _render_transcript
}

proc ::virc::_save_prefs {} {
    variable ignores
    variable aliases
    variable bufopt
    foreach {key val} [list \
        irc_ignores $ignores \
        irc_aliases [array get aliases] \
        irc_bufopt [array get bufopt]] {
        if {[catch { ::vstate::put $key $val } put_err]} {
            puts stderr "veles-desktop: irc prefs save $key failed: $put_err"
        }
    }
    variable merged
    if {[catch { ::vstate::put irc_merged $merged } put_err]} {
        puts stderr "veles-desktop: irc prefs save irc_merged failed: $put_err"
    }
}

proc ::virc::_load_prefs {} {
    variable ignores
    variable aliases
    variable bufopt
    set v {} ; catch { set v [::vstate::get irc_ignores {}] }
    if {[string is list -strict $v]} { set ignores $v }
    set v {} ; catch { set v [::vstate::get irc_aliases {}] }
    # A blob written by an older build, or a hand-edited one, must not
    # take the pane down on start. `array set` on an odd-length list
    # throws, so the length is checked rather than caught after the
    # fact — half an alias table applied is worse than none.
    if {[string is list -strict $v] && [llength $v] % 2 == 0} {
        array unset aliases ; array set aliases $v
    }
    variable merged
    set v 0 ; catch { set v [::vstate::get irc_merged 0] }
    if {[string is boolean -strict $v]} { set merged [expr {$v ? 1 : 0}] }
    set v {} ; catch { set v [::vstate::get irc_bufopt {}] }
    if {[string is list -strict $v] && [llength $v] % 2 == 0} {
        array unset bufopt ; array set bufopt $v
    }
}


# ── split windows ───────────────────────────────────────────────────
#
# weechat's /window splith. More than one buffer visible at once, which
# on a busy day is the difference between reading a channel and
# refreshing it.
#
# THE MODEL. There is one PRIMARY transcript — `$R_view.body`, the one
# that has always been there — and it follows `current`, so switching
# buffers works exactly as before. A split is an ADDITIONAL pane pinned
# to one buffer key, stacked above the primary, which does not move when
# you switch. That asymmetry is deliberate: weechat's every-pane-equal
# model means you can lose track of which pane your composer is talking
# to, and here the composer always talks to the primary.
#
# WHY PACK AND NOT A PANEDWINDOW. Tk's paned windows only manage their
# own children, so adopting the existing `$R_view.body` into one means
# renaming it — 48 references, most of them in the suites. The trade is
# that splits share space evenly and cannot be dragged. Recorded in
# PENDING as the follow-up rather than pretended away.

# Every widget that should receive a line for buffer `k`: the primary
# when it is current, plus any split pinned to it. A buffer visible in
# both gets it in both, which is what "visible in both" means.
proc ::virc::_targets_for {k} {
    variable current
    variable split_w
    set out {}
    if {$k eq $current} { lappend out [transcript] }
    if {[info exists split_w($k)] && [winfo exists $split_w($k)]} {
        lappend out $split_w($k)
    }
    return $out
}

proc ::virc::_slash_split {rest} {
    variable splits
    variable split_w
    variable current
    variable meta
    variable R_view
    set name [string trim $rest]
    set k $current
    if {$name ne ""} {
        set k [_resolve_buffer $name]
        if {$k eq ""} {
            catch { ::vstatus::set_msg [format [::vi18n::mc {no buffer %s}] $name] }
            return
        }
    }
    if {$k eq ""} { return }
    if {[info exists split_w($k)]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {%s is already split}] [_label $k]] }
        return
    }
    if {[llength $splits] >= 4} {
        # Four is not a technical limit, it is the point past which each
        # pane is too short to read. Refusing beats silently producing
        # five two-line windows.
        catch { ::vstatus::set_msg [::vi18n::mc {four splits is the maximum}] }
        return
    }
    if {![winfo exists $R_view]} { return }
    set n [llength $splits]
    set f $R_view.sp$n
    if {[winfo exists $f]} { destroy $f }
    ttk::frame $f
    ttk::label $f.hdr -text [_label $k] -anchor w -foreground $::vt::T(fg_dim)
    text $f.body -wrap word -relief flat -highlightthickness 0 -height 8 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) -padx 8 -pady 4
    ttk::scrollbar $f.sb -orient vertical -command [list $f.body yview]
    $f.body configure -yscrollcommand [list $f.sb set]
    _style_tags $f.body
    ::vtext::readonly $f.body
    pack $f.hdr -side top -fill x -padx 8
    pack $f.sb -side right -fill y
    pack $f.body -fill both -expand 1 -padx 2
    # Before the scrollbar, so the split takes a full-width band off the
    # top of the cavity and the primary's scrollbar still hugs what is
    # left. Packing it after would put the band inside the primary's
    # column.
    pack $f -before $R_view.sb -side top -fill both -expand 1
    catch { ::vi18n::aname $f.body {IRC split: %s} [_label $k] }
    lappend splits $k
    set split_w($k) $f.body
    _paint_split $k
    catch { ::vstatus::set_msg [format [::vi18n::mc {split: %s}] [_label $k]] }
}

proc ::virc::_slash_unsplit {rest} {
    variable splits
    variable split_w
    variable current
    set name [string trim $rest]
    if {$name eq ""} {
        # No argument closes the LAST one opened, which is the one the
        # eye is on and the one an accidental /split just made.
        if {![llength $splits]} {
            catch { ::vstatus::set_msg [::vi18n::mc {nothing is split}] }
            return
        }
        set k [lindex $splits end]
    } else {
        set k [_resolve_buffer $name]
    }
    if {$k eq "" || ![info exists split_w($k)]} {
        catch { ::vstatus::set_msg [::vi18n::mc {that buffer is not split}] }
        return
    }
    set i [lsearch -exact $splits $k]
    if {$i >= 0} { set splits [lreplace $splits $i $i] }
    catch { destroy [winfo parent $split_w($k)] }
    unset split_w($k)
    # Repack the survivors so the frame names stay dense (sp0, sp1, …)
    # and a later /split does not collide with a destroyed sibling's
    # name.
    _rebuild_splits
}

# Tear down and rebuild every split pane. Called after a removal so the
# `spN` names stay contiguous, and after a theme change so the new
# palette reaches panes that `retheme` does not know about.
proc ::virc::_rebuild_splits {} {
    variable splits
    variable split_w
    set keys $splits
    foreach k $keys {
        if {[info exists split_w($k)]} {
            catch { destroy [winfo parent $split_w($k)] }
            unset split_w($k)
        }
    }
    set splits {}
    foreach k $keys { _slash_split [dict get $::virc::meta($k) buffer] }
}

# Draw a split's pinned buffer from whatever is already loaded.
proc ::virc::_paint_split {k} {
    variable split_w
    variable linebuf
    if {![info exists split_w($k)] || ![winfo exists $split_w($k)]} { return }
    set w $split_w($k)
    $w configure -state normal
    $w delete 1.0 end
    if {[info exists linebuf($k)]} {
        foreach line $linebuf($k) { render_line $w $line }
    } else {
        # Nothing loaded for a buffer that is not current — fetch one
        # page so the split is not an empty box.
        set linebuf($k) [_fetch_backlog $k ""]
        foreach line $linebuf($k) { render_line $w $line }
    }
    ::vtext::readonly $w
    $w see end
}

# Turn "#chan" or a full key into a buffer key, preferring the current
# network so `/split #other` means the network you are looking at.
proc ::virc::_resolve_buffer {name} {
    variable buffers
    variable meta
    variable current
    if {$name in $buffers} { return $name }
    set net "" ; set pres ""
    if {$current ne "" && [info exists meta($current)]} {
        set net [dict get $meta($current) network]
        set pres [dict get $meta($current) presence]
    }
    set k [key $net $pres $name]
    if {$k in $buffers} { return $k }
    foreach b $buffers {
        if {[dict get $meta($b) buffer] eq $name} { return $b }
    }
    return ""
}

# ── merged server buffer ────────────────────────────────────────────
#
# weechat puts every network's server messages in one buffer instead of
# one `*` per network. With three networks that is three buffers you
# check separately for the same class of thing — connection notices,
# MOTDs, mode changes on you — and the reason it is worth merging is
# that server traffic is the traffic you read chronologically rather
# than by channel.
#
# The merged view is SYNTHETIC: no gateway ever sends a line for it. It
# is `_ingest` fanning a server line into a second buffer key as well as
# its own, so `*` per network keeps working for anyone who prefers it.

# The key of the merged buffer. Not a real network/presence/buffer
# triple, and deliberately unroutable: `can_send` must refuse it,
# because "which network would this go to" has no answer.
proc ::virc::merged_key {} { return "*/*/*" }

proc ::virc::merged_enabled {} {
    variable merged
    return [expr {$merged ? 1 : 0}]
}

proc ::virc::_slash_merge {rest} {
    variable merged
    variable buffers
    set arg [string tolower [string trim $rest]]
    if {$arg eq ""} {
        set merged [expr {!$merged}]
    } elseif {$arg in {on 1 true yes}} {
        set merged 1
    } elseif {$arg in {off 0 false no}} {
        set merged 0
    } else {
        catch { ::vstatus::set_msg [::vi18n::mc {/merge takes on or off}] }
        return
    }
    if {[catch { ::vstate::put irc_merged $merged } put_err]} {
        puts stderr "veles-desktop: irc /merge prefs save failed: $put_err"
    }
    if {$merged} {
        _note_buffer [merged_key] "*" "*" "*"
        catch { ::vstatus::set_msg [::vi18n::mc {server buffers merged}] }
    } else {
        # Drop the synthetic buffer rather than leaving a stale one in
        # the list that nothing will ever write to again.
        set i [lsearch -exact $buffers [merged_key]]
        if {$i >= 0} { set buffers [lreplace $buffers $i $i] }
        variable meta ; unset -nocomplain meta([merged_key])
        variable linebuf ; unset -nocomplain linebuf([merged_key])
        variable current
        if {$current eq [merged_key]} { set current "" }
        _refresh_buffers
        catch { ::vstatus::set_msg [::vi18n::mc {server buffers separate}] }
    }
}

# Is this line one the merged buffer wants? Server traffic only — a
# channel message must not appear there or the merged buffer becomes a
# second copy of everything.
proc ::virc::_is_server_line {d} {
    set buffer "" ; catch { set buffer [dict get $d buffer] }
    return [expr {$buffer eq "*"}]
}

# ── ban lists ────────────────────────────────────────────────────────
#
# /bans asks the engine for MODE +b and shows what came back in the
# transcript as meta lines. Viewing is read-only on purpose: SETTING a
# ban is a mode change (+b mask) and already works through /raw or the
# roster's mode action, both of which confirm first. A dedicated setter
# here would bypass that confirmation for the one op most likely to be
# aimed at the wrong nick.

proc ::virc::_slash_bans {rest} {
    set chan [string trim $rest]
    if {$chan eq ""} { set chan [current_channel] }
    if {$chan eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {/bans needs a channel}] }
        return
    }
    set args [list action bans channel $chan]
    if {$rest eq ""} { lappend args scope $chan }
    op_here {*}$args
}

# ── ACL management from this tab ─────────────────────────────────────
#
# USER (2026-08-30): "there was also supposed to be direct management
# over the IRC tab". The people an ACL is about are the people in the
# roster, so the tab gets both surfaces: /acl opens the overlay
# prefilled for this network, and the roster's right-click menu grants
# the selected member's MASK a role — or bans it — scoped to the
# network they are on.

proc ::virc::_slash_acl {} {
    ::vacl::open_for "" [_acl_current_network]
}

# The network of the buffer on screen ("" when none/unattached) — the
# scope a roster grant belongs to.
proc ::virc::_acl_current_network {} {
    variable current
    variable meta
    if {$current eq "" || ![info exists meta($current)]} { return "" }
    return [dict get $meta($current) network]
}

# The ACL identity for the selected roster member.
#
# The FULL mask when NAMES carried one (userhost-in-names); otherwise
# ASK — a WHOIS answers with `hostmask nick!user@host`, and that is the
# identity a rule should name. `nick!*@*` is only the fallback for when
# the ask fails, and it is a bad one: a nick is the most spoofable
# thing on IRC, so a grant keyed on `nick!*@*` hands the role to
# whoever takes the nick next (PENDING.md: "roster grants default to
# spoofable nick!*@*"). The caller is told which one it got.
#
# `$whois` is the injection seam for the tests: a one-argument command
# prefix taking the nick and returning the WHOIS reply text.
proc ::virc::_roster_acl_identity {{whois ::virc::_roster_whois}} {
    variable R_roster
    if {![winfo exists $R_roster.list]} { return "" }
    set i [lindex [$R_roster.list curselection] 0]
    if {$i eq ""} { return "" }
    set m [string trimleft [$R_roster.list get $i] "@%+~&"]
    if {$m eq ""} { return "" }
    if {[string first "!" $m] >= 0} { return $m }
    set reply ""
    catch { set reply [{*}$whois $m] }
    if {[regexp {hostmask\s+(\S+!\S+@\S+?);} $reply -> mask]} { return $mask }
    return "$m!*@*"
}

# One WHOIS, synchronously, through the same op endpoint the roster's
# Whois menu item uses. Returns the reply text ("" on failure).
proc ::virc::_roster_whois {nick} {
    variable current
    variable meta
    if {$current eq "" || ![info exists meta($current)]} { return "" }
    set d $meta($current)
    set body [dict create network [dict get $d network] \
                  presence [dict get $d presence] action whois nick $nick]
    if {[catch { ::vapi::post_json /api/irc/op $body } resp]} { return "" }
    set out ""
    catch { set out [dict get $resp result] }
    return $out
}

proc ::virc::_roster_acl_grant {role} {
    set identity [_roster_acl_identity]
    if {$identity eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {select a member first}] }
        return 0
    }
    set network [_acl_current_network]
    set body [dict create identity $identity role $role]
    if {$network ne ""} { dict set body network $network }
    # Say when the identity is the spoofable fallback: a grant keyed on
    # a bare nick belongs to whoever holds that nick, and the operator
    # should know that is what they just made.
    if {[string match {*!\*@\**} $identity]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc \
            {acl: no hostmask for %s — granting the NICK, which anyone can take}] \
            $identity] }
    }
    if {[catch { ::vapi::post_json /api/acl/grant $body } resp]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {acl: grant failed: %s}] $resp] }
        return 0
    }
    set why ""
    catch { set why [dict get $resp error] }
    if {$why ne ""} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {acl: refused: %s}] $why] }
        return 0
    }
    catch { ::vstatus::set_msg [format [::vi18n::mc {acl: %s → %s%s}] \
        $identity $role [expr {$network eq "" ? "" : " on $network"}]] }
    return 1
}

# Revoke whatever runtime grant matches this member's identity string.
# Config pins never come through here; the overlay explains those.
proc ::virc::_roster_acl_revoke {} {
    set identity [_roster_acl_identity]
    if {$identity eq ""} {
        catch { ::vstatus::set_msg [::vi18n::mc {select a member first}] }
        return 0
    }
    if {[catch { ::vapi::post_json /api/acl/revoke [dict create identity $identity] } resp]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {acl: revoke failed: %s}] $resp] }
        return 0
    }
    set ok 0
    catch { set ok [dict get $resp ok] }
    if {$ok} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {acl: revoked %s}] $identity] }
    } else {
        catch { ::vstatus::set_msg [format [::vi18n::mc {acl: no runtime grant for %s}] $identity] }
    }
    return 1
}

# ── typing indicators ────────────────────────────────────────────────
#
# BOTH directions, off one piece of state per buffer:
#
#   INBOUND — a `typing` tap event updates ::virc::typing($k) to a list
#   of nicks currently typing and schedules its own expiry; rendering
#   happens in the topic line, which is always visible and never scrolls
#   away. The events are EPHEMERAL (the gateway store skips them), so
#   nothing stale can come back with a replay.
#
#   OUTBOUND — keys in the composer arm a +typing=active TAGMSG (only if
#   the connection negotiated the cap) and send +typing=done when the
#   message actually goes out or after five idle seconds. weechat's
#   shape: indicators are for "somebody is at their keyboard", not a
#   keystroke stream.

proc ::virc::typing_on_event {d} {
    variable typing
    variable typing_after
    set k [key [dict get $d network] [dict get $d presence] [dict get $d buffer]]
    set nick [dict get $d nick]
    set active [expr {[dict get $d text] eq "active"}]
    set cur {}
    if {[info exists typing($k)]} { set cur $typing($k) }
    set has [expr {[lsearch -exact $cur $nick] >= 0}]
    if {$active && !$has} {
        lappend cur $nick
    } elseif {!$active && $has} {
        set i [lsearch -exact $cur $nick]
        set cur [lreplace $cur $i $i]
    }
    if {![llength $cur]} {
        array unset typing $k
    } else {
        set typing($k) $cur
    }
    # Each indicator expires itself: servers are not obliged to send
    # +typing=done (a crash mid-sentence must not pin an eternal
    # "typing"), so six seconds after the last ACTIVE event the nick
    # drops out on our side regardless.
    if {$active} {
        catch { after cancel $typing_after($k) }
        set typing_after($k) [after 6000 [list ::virc::_typing_expire $k $nick]]
    }
    _refresh_typing $k
}

proc ::virc::_typing_expire {k nick} {
    variable typing
    if {![info exists typing($k)]} { return }
    set i [lsearch -exact $typing($k) $nick]
    if {$i >= 0} { set typing($k) [lreplace $typing($k) $i $i] }
    if {![llength $typing($k)]} { array unset typing $k }
    _refresh_typing $k
}

# The indicator lives in the topic label — visible in every state, no
# scrolling, and no interference with the transcript's history.
proc ::virc::_refresh_typing {{k ""}} {
    variable current
    if {$k eq ""} { set k $current }
    if {$k ne $current} { return }
    catch { _refresh_topic $k }
}

# Outbound: called from the composer on each edit.
proc ::virc::composer_typed {} {
    variable current
    variable meta
    variable typing_out_after
    if {$current eq "" || ![info exists meta($current)]} { return }
    set m $meta($current)
    set net [dict get $m network]
    set pres [dict get $m presence]
    set target [dict get $m buffer]
    # Channels and queries carry indicators; the server buffer ("*") has
    # nobody to show them to. `_send_typing` also refuses server-side
    # when the connection never negotiated the cap.
    if {$target eq "" || $target eq "*"} { return }
    _send_typing $net $pres $target active
    catch { after cancel $typing_out_after($current) }
    set typing_out_after($current) \
        [after 5000 [list ::virc::_send_typing $net $pres $target done]]
}

proc ::virc::_send_typing {network presence target state} {
    set body [dict create network $network presence $presence \
                       target $target state $state]
    # Deliberate fire-and-forget: a refused indicator must never toast
    # over the composer, because the message it is attached to is
    # unaffected. But "deliberate" has to be VISIBLE, so the error is
    # bound and dropped rather than left unbound — same discipline as
    # the reconnect path in `connect`.
    if {[catch { ::vapi::post_json /api/irc/typing $body } err]} {
        set ::virc::typing_send_error $err
    }
    return
}

# ── Tab completion ─────────────────────────────────────────────────
#
# Nicks (most-recently-spoken first), command names after `/`, and
# channel names after `/join `. Repeated Tab cycles.

proc ::virc::complete {} {
    variable R_view
    variable complete
    if {![winfo exists $R_view.input]} { return }
    set w $R_view.input

    # A REPEAT Tab cycles; a fresh one starts over.
    #
    # The cycle state cannot be keyed on the word under the cursor, which
    # is what it was: after the first Tab that word IS the completion
    # ("bob: "), so the second Tab looked up a different key, found no
    # candidates for the prefix "bob:", and did nothing at all. One of
    # two matching nicks was unreachable from the keyboard.
    #
    # Key it on what we last INSERTED instead, and confirm the text
    # immediately before the cursor is still exactly that. Typing
    # anything after a completion breaks the match and the next Tab is a
    # fresh one, which is what it should be.
    set repeat 0
    if {[info exists complete(inserted)] && $complete(inserted) ne ""} {
        set n [string length $complete(inserted)]
        if {[$w get "insert - $n chars" insert] eq $complete(inserted)} { set repeat 1 }
    }

    if {$repeat} {
        set cands $complete(cands)
        set fmt   $complete(fmt)
        set idx   [expr {($complete(idx) + 1) % [llength $cands]}]
        set drop  [string length $complete(inserted)]
    } else {
        set txt [$w get 1.0 insert]
        set lastspace [string last " " $txt]
        set word [string range $txt [expr {$lastspace + 1}] end]
        set fmt {%s }
        if {[string match "/*" $word] && $lastspace < 0} {
            # A slash command, only at the very start of the line.
            set cands [_completion_candidates [string trimleft $word "/"] command]
            set fmt {/%s }
        } elseif {[lindex [split [string trimleft $txt] " "] 0] eq "/join" && $lastspace >= 0} {
            set cands [_completion_candidates $word channel]
        } else {
            set cands [_completion_candidates $word nick]
            # At the START of a line a nick gets "name: " — weechat's
            # behaviour, and what makes addressing someone one keystroke.
            set fmt [expr {$lastspace < 0 ? "%s: " : "%s "}]
        }
        set idx 0
        set drop [string length $word]
    }

    # No candidate must leave the composer exactly as it was: not eat the
    # word, not insert an empty completion.
    if {![llength $cands]} {
        array unset complete
        return
    }

    set replacement [format $fmt [lindex $cands $idx]]
    $w delete "insert - $drop chars" insert
    $w insert insert $replacement
    set complete(cands)    $cands
    set complete(fmt)      $fmt
    set complete(idx)      $idx
    set complete(inserted) $replacement
    return -code break
}

# The candidates for one completion context, filtered by `prefix`.
proc ::virc::_completion_candidates {prefix context} {
    switch -exact -- $context {
        command {
            # From the one list, not a fourth hand-maintained copy. A
            # command you can type but cannot complete is the kind of
            # drift nobody notices until they need the command.
            return [lsearch -all -inline -glob [lsort [_builtin_slash_names]] "${prefix}*"]
        }
        channel {
            return [lsort -unique [lsearch -all -inline -glob [_channels_of_current] "${prefix}*"]]
        }
        nick {
            return [lsearch -all -inline -glob [_nicks_of_current] "${prefix}*"]
        }
    }
    return {}
}

# Every channel we know on the connection the current buffer belongs to.
proc ::virc::_channels_of_current {} {
    variable buffers
    variable meta
    variable current
    if {$current eq "" || ![info exists meta($current)]} { return {} }
    set m $meta($current)
    set out {}
    foreach k $buffers {
        if {![info exists meta($k)]} { continue }
        set mk $meta($k)
        if {[dict get $mk network] ne [dict get $m network]} { continue }
        if {[dict get $mk presence] ne [dict get $m presence]} { continue }
        set b [dict get $mk buffer]
        if {[string match "#*" $b] || [string match "&*" $b]} { lappend out $b }
    }
    return $out
}

# Nicks for the current buffer: most-recently-spoken first, then the rest
# of the roster, in case order.
proc ::virc::_nicks_of_current {} {
    variable current
    variable recency
    variable rosters
    if {$current eq ""} { return {} }
    set out {}
    if {[info exists recency($current)]} { set out $recency($current) }
    if {[info exists rosters($current)]} {
        foreach m $rosters($current) {
            set n [nick_of $m]
            if {$n ni $out} { lappend out $n }
        }
    }
    return $out
}

proc ::virc::send {network presence target text {kind privmsg}} {
    # `text` is FREE FORM and multi-word — typed as a JSON string or the
    # _json_value shape inference serializes it as an array (odd word
    # count) or an object (even, identifier-headed), and the gateway's
    # serde wants a string: HTTP 422 (RM3 #10).
    set body [dict create network $network presence $presence \
                          target $target text [::vapi::json_string $text] kind $kind]
    if {[catch { ::vapi::post_json /api/irc/send $body } resp]} {
        # Never silent. A message that goes nowhere without saying so is
        # the worst outcome available here — it looks sent.
        catch { ::vstatus::set_msg [format [::vi18n::mc {irc: not sent — %s}] $resp] }
        return 0
    }
    if {[catch { dict get $resp error } err] == 0 && $err ne ""} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {irc: not sent — %s}] $err] }
        return 0
    }
    return 1
}

# Can we send as this buffer's presence at all? A composer offered for
# a connection the gateway cannot send through is a composer that
# loses messages.
proc ::virc::can_send {{k ""}} {
    variable current
    variable meta
    variable sendable
    if {$k eq ""} { set k $current }
    if {$k eq "" || ![info exists meta($k)]} { return 0 }
    # The merged buffer is a VIEW over several networks, so "which
    # network does this go to" has no answer. Refusing here is what
    # keeps the composer from picking one arbitrarily — which, on a
    # buffer whose whole purpose is that it spans networks, would send
    # to the wrong one sooner or later.
    if {$k eq [merged_key]} { return 0 }
    return [expr {[dict get $meta($k) presence] in $sendable}]
}

# ── ops: driving a connection ───────────────────────────────────────
#
# Join, part, topic, kick, mode. The reply is whatever the server said,
# shown verbatim — an op that silently did nothing looks exactly like
# one the server rejected.
#
# Destructive ops confirm FIRST, in the desktop. The gateway does not do
# it: the approvals layer exists for the agent to ask a human, keyed on
# a session and delivered through a turn's stream, and a kick you
# clicked has neither. This is the same tk_messageBox the app already
# uses before reverting a file or deleting a session.

proc ::virc::op {network presence args} {
    set body [dict create network $network presence $presence {*}$args]
    if {[catch { ::vapi::post_json /api/irc/op $body } resp]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {irc: %s failed — %s}] [dict get $body action] $resp] }
        return 0
    }
    set err ""
    catch { set err [dict get $resp error] }
    if {$err ne ""} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {irc: %s}] $err] }
        return 0
    }
    set result ""
    catch { set result [dict get $resp result] }
    # Say what happened even on success — for ACTIONS. Introspection
    # answers (whois, ban list, channel info, topic queries, away
    # confirmations) arrive as `info` tap events in the buffer they
    # were asked from; repeating them in the status bar is exactly the
    # transient-message duplication the user asked to end. Wire actions
    # (join/kick/mode/…) keep the status line — most have no event of
    # their own and "did my kick work" must not stay unanswered.
    set quiet 0
    set action ""
    catch { set action [dict get $body action] }
    if {$action in {whois bans away channel_info}} { set quiet 1 }
    if {$action eq "topic" && ![dict exists $body topic]} { set quiet 1 }
    if {!$quiet} {
        catch { ::vstatus::set_msg [string trim [format [::vi18n::mc {irc: %1$s — %2$s}] $action $result]] }
    }
    return 1
}

# The op targets the CURRENT buffer's presence and network, so an
# action always acts as whoever owns the buffer you are looking at.
proc ::virc::op_here {args} {
    variable current
    variable meta
    if {$current eq "" || ![info exists meta($current)]} {
        catch { ::vstatus::set_msg [::vi18n::mc {no IRC buffer selected}] }
        return 0
    }
    set m $meta($current)
    return [op [dict get $m network] [dict get $m presence] {*}$args]
}

# Destructive ops name the presence in the prompt. Acting as the bot
# when you meant to act as yourself is the failure mode here, and a
# confirm that does not say who it will act as does not prevent it.
proc ::virc::confirm_op {question args} {
    variable current
    variable meta
    if {$current eq "" || ![info exists meta($current)]} { return 0 }
    set who [dict get $meta($current) presence]
    set buf [dict get $meta($current) buffer]
    set msg "$question\n\nActing as: $who in $buf"
    if {[tk_messageBox -type yesno -icon warning -title [::vi18n::mc {IRC}] \
            -message $msg] ne "yes"} {
        return 0
    }
    return [op_here {*}$args]
}

proc ::virc::current_channel {} {
    variable current
    variable meta
    if {$current eq "" || ![info exists meta($current)]} { return "" }
    return [dict get $meta($current) buffer]
}

proc ::virc::selected_nick {} {
    variable R_roster
    if {![winfo exists $R_roster.list]} { return "" }
    set i [lindex [$R_roster.list curselection] 0]
    if {$i eq ""} { return "" }
    return [nick_of [$R_roster.list get $i]]
}

# ── roster actions ──────────────────────────────────────────────────

proc ::virc::whois_selected {} {
    set n [selected_nick]
    if {$n eq ""} { return 0 }
    set args [list action whois nick $n]
    set sc [info_scope]
    if {$sc ne ""} { lappend args scope $sc }
    return [op_here {*}$args]
}

proc ::virc::mode_selected {flag} {
    set n [selected_nick]
    set c [current_channel]
    if {$n eq "" || $c eq ""} { return 0 }
    return [confirm_op [format [::vi18n::mc {Set mode %1$s on %2$s?}] $flag $n] \
        action mode target $c modes "$flag $n"]
}

proc ::virc::kick_selected {} {
    set n [selected_nick]
    set c [current_channel]
    if {$n eq "" || $c eq ""} { return 0 }
    return [confirm_op [format [::vi18n::mc {Kick %1$s from %2$s?}] $n $c] \
        action kick channel $c nick $n]
}

# ── channel actions ─────────────────────────────────────────────────

proc ::virc::part_current {} {
    set c [current_channel]
    if {![string match "#*" $c] && ![string match "&*" $c]} {
        catch { ::vstatus::set_msg [::vi18n::mc {not a channel}] }
        return 0
    }
    return [confirm_op [format [::vi18n::mc {Leave %s?}] $c] action part channel $c]
}

proc ::virc::join_prompt {} {
    set c [_ask_line [::vi18n::mc {Join which channel?}] "#"]
    if {$c eq ""} { return 0 }
    return [op_here action join channel $c]
}

proc ::virc::topic_prompt {} {
    variable topics
    variable current
    set c [current_channel]
    if {$c eq ""} { return 0 }
    set now ""
    if {[info exists topics($current)]} { set now [strip_formatting $topics($current)] }
    set t [_ask_line [format [::vi18n::mc {Topic for %s}] $c] $now]
    if {$t eq ""} { return 0 }
    return [op_here action topic channel $c topic $t]
}

# A one-line prompt. Deliberately modal and tiny — these are rare
# actions and a whole dialog module for them would be worse.
proc ::virc::_ask_line {prompt {initial ""}} {
    set w .virc_ask
    catch { destroy $w }
    toplevel $w
    wm title $w [::vi18n::mc {IRC}]
    wm transient $w .
    ttk::label $w.l -text $prompt
    ttk::entry $w.e -width 40
    $w.e insert 0 $initial
    variable _ask_result ""
    ttk::frame $w.b
    ttk::button $w.b.ok -text [::vi18n::mc {OK}] \
        -command { set ::virc::_ask_result [.virc_ask.e get] ; destroy .virc_ask }
    ttk::button $w.b.no -text [::vi18n::mc {Cancel}] \
        -command { set ::virc::_ask_result "" ; destroy .virc_ask }
    pack $w.l -padx 10 -pady {10 4}
    pack $w.e -padx 10 -fill x
    pack $w.b.ok $w.b.no -side left -padx 2
    pack $w.b -pady 8
    bind $w.e <Return> { set ::virc::_ask_result [.virc_ask.e get] ; destroy .virc_ask }
    bind $w.e <Escape> { set ::virc::_ask_result "" ; destroy .virc_ask }
    catch { ::vplace::centered $w 380 130 0.3 }
    focus $w.e
    catch { grab set $w }
    tkwait window $w
    variable _ask_result
    return $_ask_result
}

# ── the three regions ───────────────────────────────────────────────

proc ::virc::transcript {} { variable R_view ; return $R_view.body }

proc ::virc::build_bufs {{root ""}} {
    variable R_bufs
    if {$root ne ""} { set R_bufs $root }
    if {[winfo exists $R_bufs]} { destroy $R_bufs }
    ttk::frame $R_bufs
    # A treeview, not a listbox: the hotlist colour-codes rows by activity
    # level, and a plain listbox has a single foreground for the whole
    # widget. The item id is the buffer key, so `_pick` reads the
    # selection straight off.
    ttk::treeview $R_bufs.list -columns {label} -show {} -selectmode browse
    $R_bufs.list heading #0 -text ""
    $R_bufs.list column #0 -width 0 -minwidth 0 -stretch 0
    $R_bufs.list tag configure hl    -foreground $::vt::T(red)
    $R_bufs.list tag configure msg   -foreground $::vt::T(fg)
    $R_bufs.list tag configure noise -foreground $::vt::T(fg_dim)
    ttk::scrollbar $R_bufs.sb -orient vertical -command [list $R_bufs.list yview]
    $R_bufs.list configure -yscrollcommand [list $R_bufs.sb set]
    pack $R_bufs.sb -side right -fill y
    pack $R_bufs.list -fill both -expand 1
    bind $R_bufs.list <<TreeviewSelect>> { ::virc::_pick %W }
    ::vctx::attach $R_bufs.list {
        {"Open"          {::virc::_pick_selected}}
        {"Mark read"     {::virc::mark_read}}
        {-}
        {"Join…"         {::virc::join_prompt}}
        {"Set topic…"    {::virc::topic_prompt}}
        {"Leave…"        {::virc::part_current}}
        {-}
        {"Copy name"     {::virc::_copy_buffer_name}}
    }
    catch { ::va11y::name $R_bufs.list [::vi18n::mc {IRC buffers}] }
    _refresh_buffers
    # Pack only on the FLOAT path, where the parent is the float's own
    # toplevel. At boot the parent is the main window, and this guard
    # used to fire there too — every region came up packed into the
    # root BESIDE the active tab's layout (the "fourth pane in the chat
    # tab" startup bug) until a tab switch happened to absorb them.
    set parent [winfo parent $R_bufs]
    if {$parent ne "." && [winfo toplevel $parent] eq $parent} { pack $R_bufs -fill both -expand 1 }
    return $R_bufs
}

proc ::virc::build_view {{root ""}} {
    variable R_view
    if {$root ne ""} { set R_view $root }
    if {[winfo exists $R_view]} { destroy $R_view }
    ttk::frame $R_view
    ttk::label $R_view.topic -text "" -anchor w -foreground $::vt::T(fg_dim)
    # The connection state of the current buffer's network. WeeChat
    # keeps this visible at all times (server buffer + status-bar lag);
    # here it used to exist only as `●`/`○` lines inside `*`, which you
    # saw only if that buffer happened to be open. Fed by the tap's
    # connected/disconnected/connecting events and /api/irc/state.
    ttk::label $R_view.status -text "" -anchor w -foreground $::vt::T(fg_dim)
    text $R_view.body -wrap word -relief flat -highlightthickness 0 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
        -padx 8 -pady 6
    ttk::scrollbar $R_view.sb -orient vertical -command [list $R_view.body yview]
    $R_view.body configure -yscrollcommand [list $R_view.sb set]
    text $R_view.input -height 2 -wrap word -relief flat -highlightthickness 0 \
        -background $::vt::T(bg_panel) -foreground $::vt::T(fg) -padx 8 -pady 4
    $R_view.input tag configure irc_ctrl \
        -background $::vt::T(accent) -foreground $::vt::T(bg_panel)
    # Bound on the WIDGET, so it beats the Text class binding that would
    # otherwise paste. Paste stays on C-y and Shift-Insert.
    # Enter sends, Shift-Enter is a newline — the composer idiom the
    # chat pane already uses, so one habit covers both.
    bind $R_view.input <Return>       { ::virc::send_current ; break }
    bind $R_view.input <Shift-Return> { %W insert insert "\n" ; break }
    bind $R_view.input <Control-Key-v> { ::virc::quoted_insert }
    bind $R_view.input <KeyPress>      { ::virc::quoted_key %W %A }
    bind $R_view.input <KeyRelease>    { ::virc::mark_controls %W ; ::virc::composer_typed }
    bind $R_view.input <Tab>           { ::virc::complete }
    # Scrolling to the top of the transcript pages backwards.
    bind $R_view.body <MouseWheel>     { ::virc::_maybe_load_older %W }
    bind $R_view.body <Button-4>       { ::virc::_maybe_load_older %W }
    bind $R_view.body <Prior>          { ::virc::_maybe_load_older %W }
    _style_tags $R_view.body
    ::vtext::readonly $R_view.body
    pack $R_view.topic -side top -fill x -padx 8 -pady {4 2}
    pack $R_view.status -side top -fill x -padx 8
    pack $R_view.input -side bottom -fill x -padx 2 -pady {2 4}
    pack $R_view.sb -side right -fill y
    pack $R_view.body -fill both -expand 1 -padx 2
    ::vctx::attach $R_view.body {
        {"Copy"            {::virc::_copy_selection}}
        {-}
        {"Scroll to end"   {::virc::_see_end}}
    }
    catch { ::va11y::name $R_view.body [::vi18n::mc {IRC transcript}] }
    catch { ::va11y::name $R_view.input [::vi18n::mc {IRC composer}] }
    # The same seam as the chat pane: this transcript is tall and the input
    # is packed -side bottom under it, so a pointer near the seam used to
    # get "IRC transcript" written across what you are typing. Split views
    # build several of these, and ::va11y::keepout is idempotent and inert
    # for dead paths, so a rebuild just adds its own.
    ::va11y::keepout $R_view.input
    # Float-path pack only — see build_bufs for the startup bug this guard
    # used to cause when it fired at boot as well.
    set parent [winfo parent $R_view]
    if {$parent ne "." && [winfo toplevel $parent] eq $parent} { pack $R_view -fill both -expand 1 }
    return $R_view
}

proc ::virc::build_roster {{root ""}} {
    variable R_roster
    if {$root ne ""} { set R_roster $root }
    if {[winfo exists $R_roster]} { destroy $R_roster }
    ttk::frame $R_roster
    listbox $R_roster.list -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(select_bg) -selectforeground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -bd 0 -selectmode single -exportselection 0
    ttk::scrollbar $R_roster.sb -orient vertical -command [list $R_roster.list yview]
    $R_roster.list configure -yscrollcommand [list $R_roster.sb set]
    pack $R_roster.sb -side right -fill y
    pack $R_roster.list -fill both -expand 1
    ::vctx::attach $R_roster.list {
        {"Whois"       {::virc::whois_selected}}
        {-}
        {"ACL: grant admin"   {::virc::_roster_acl_grant admin}}
        {"ACL: grant trusted" {::virc::_roster_acl_grant trusted}}
        {"ACL: grant user"    {::virc::_roster_acl_grant user}}
        {"ACL: grant guest"   {::virc::_roster_acl_grant guest}}
        {"ACL: ban mask"      {::virc::_roster_acl_grant banned}}
        {"ACL: revoke"        {::virc::_roster_acl_revoke}}
        {-}
        {"Op"          {::virc::mode_selected +o}}
        {"Deop"        {::virc::mode_selected -o}}
        {"Voice"       {::virc::mode_selected +v}}
        {"Devoice"     {::virc::mode_selected -v}}
        {-}
        {"Kick…"       {::virc::kick_selected}}
        {-}
        {"Copy nick"   {::virc::_copy_nick}}
    }
    catch { ::va11y::name $R_roster.list [::vi18n::mc {channel members}] }
    # Float-path pack only — see build_bufs.
    set parent [winfo parent $R_roster]
    if {$parent ne "." && [winfo toplevel $parent] eq $parent} { pack $R_roster -fill both -expand 1 }
    return $R_roster
}

# A named font derived from the widget's own, with one attribute
# changed. Named so it is created once and reconfigured on a theme
# change rather than leaking a font per rebuild.
proc ::virc::_derive_font {w suffix args} {
    set name virc_$suffix
    if {[lsearch -exact [font names] $name] < 0} {
        if {[catch { font create $name }]} { return [$w cget -font] }
    }
    catch { font configure $name {*}[font actual [$w cget -font]] }
    catch { font configure $name {*}$args }
    return $name
}

# Text tags for mIRC formatting and for the chrome around a line.
proc ::virc::_style_tags {w} {
    $w tag configure irc_time   -foreground $::vt::T(fg_dim)
    $w tag configure irc_punct  -foreground $::vt::T(fg_dim)
    $w tag configure irc_meta   -foreground $::vt::T(fg_dim)
    $w tag configure irc_highlight -background $::vt::T(select_bg)
    $w tag configure irc_readrule  -foreground $::vt::T(accent) \
        -font [_derive_font $w readrule -weight bold]
    # NOT [list {*}[$w cget -font] bold]. A text widget's -font is
    # usually a NAMED font ("TkFixedFont"), so that builds the list
    # {TkFixedFont bold}, which Tk reads as family + SIZE and rejects
    # with "expected integer but got bold" — taking the whole pane down
    # at build time. Derive a real font from the base's actual
    # attributes instead.
    $w tag configure irc_bold   -font [_derive_font $w bold   -weight bold]
    $w tag configure irc_italic -font [_derive_font $w italic -slant italic]
    $w tag configure irc_under  -underline 1
    $w tag configure irc_strike -overstrike 1
    # Nick colours ride the theme's accents rather than fixed RGB, so
    # they stay legible in high-contrast and large-print themes.
    set accents [list $::vt::T(accent) $::vt::T(green) $::vt::T(red) \
                      $::vt::T(fg) $::vt::T(accent) $::vt::T(green) \
                      $::vt::T(red) $::vt::T(fg)]
    for {set i 0} {$i < 8} {incr i} {
        $w tag configure irc_nick$i -foreground [lindex $accents $i]
    }
    # Every colour a line can name — the classic sixteen by Tk name, and
    # the extended 16–98 by hex. Configured from the same tables
    # `_mirc_colour` reads, so the two cannot drift: a colour it can
    # return but nothing configured would render as an unstyled run with
    # no error anywhere.
    foreach {n c} [concat [_mirc_basic_names] [_mirc_extended]] {
        if {[catch { $w tag configure irc_fg_$c -foreground $c } tag_err]} {
            puts stderr "veles-desktop: irc colour tag irc_fg_$c failed: $tag_err"
        }
    }
}

# ── buffer list ─────────────────────────────────────────────────────

# The label names its PRESENCE whenever more than one is connected.
# Acting as the bot when you meant to act as yourself is the failure
# mode of this whole pane, and a buffer list that shows "#veles" twice
# with no way to tell them apart is how that happens.
proc ::virc::_label {k} {
    variable meta
    variable unread
    set name $k
    set who ""
    # The merged buffer is synthetic and its meta is all "*", which as a
    # label reads as one network's server buffer — the one thing it is
    # not.
    if {$k eq [merged_key]} {
        set n 0
        if {[info exists unread($k)]} { set n $unread($k) }
        set name [::vi18n::mc {all servers}]
        if {$n > 0} { return [format "%s  (%d)" $name $n] }
        return $name
    }
    if {[info exists meta($k)]} {
        set name [dict get $meta($k) buffer]
        set who [dict get $meta($k) presence]
    }
    if {[llength [presences]] > 1 && $who ne ""} { set name "$name ($who)" }
    # The count is the unread tally; the treeview tag colours the row by
    # the activity LEVEL (highlight red, message plain, noise dim).
    set n 0
    if {[info exists unread($k)]} { set n $unread($k) }
    if {$n > 0} { return [format "%s  (%d)" $name $n] }
    return $name
}

# Every presence that has a buffer here.
proc ::virc::presences {} {
    variable buffers
    variable meta
    set out {}
    foreach k $buffers {
        if {![info exists meta($k)]} { continue }
        set p [dict get $meta($k) presence]
        if {$p ni $out} { lappend out $p }
    }
    return [lsort $out]
}

proc ::virc::_refresh_buffers {} {
    variable R_bufs
    variable buffers
    variable current
    if {![winfo exists $R_bufs.list]} { return }
    # Hotlist order: highest activity level first, then freshest. Ties on
    # level fall through to lasttouch; insert order breaks the rest.
    set ordered [lsort -command _buffer_sort_compare $buffers]
    set kids [$R_bufs.list children {}]
    if {[llength $kids]} { $R_bufs.list delete $kids }
    foreach k $ordered {
        set tag {}
        switch -- [_activity_of $k] {
            3 { set tag hl }
            2 { set tag msg }
            1 { set tag noise }
        }
        $R_bufs.list insert {} end -id $k -values [list [_label $k]] -tags $tag
    }
    if {$current ne "" && [lsearch -exact $ordered $current] >= 0} {
        # `selection set`, two words. `selection_set` is a Tk 8.4-era
        # listbox spelling that ttk::treeview has never had, so this
        # threw every time a buffer was selected with the list on
        # screen — which is to say every buffer switch in the real pane.
        # It went unnoticed because no test built the buffer LIST and
        # then called `show`; the suites built one or the other.
        $R_bufs.list selection set [list $current]
        $R_bufs.list see $current
    }
}

proc ::virc::_buffer_sort_compare {a b} {
    variable activity
    variable lasttouch
    set la [expr {[info exists activity($a)] ? $activity($a) : 0}]
    set lb [expr {[info exists activity($b)] ? $activity($b) : 0}]
    if {$la != $lb} { return [expr {$lb - $la}] }
    set ta [expr {[info exists lasttouch($a)] ? $lasttouch($a) : 0}]
    set tb [expr {[info exists lasttouch($b)] ? $lasttouch($b) : 0}]
    # The tiebreak must be a COMPARISON, not a difference. Timestamps
    # come from `clock milliseconds`, so the subtraction yields a 64-bit
    # wide integer — and lsort's -compare contract takes a plain int, so
    # a wide return throws "compare command returned non-integer result"
    # mid-sort. Two fresh timestamps stay under 2^31 only while the
    # epoch is young; today they never are, which is why this survived
    # until the first test let two buffers carry equal activity and
    # unequal freshness into the same sort.
    if {$tb < $ta} { return -1 }
    if {$tb > $ta} { return 1 }
    return 0
}

# The <<TreeviewSelect>> handler — GUARDED against re-showing the buffer
# that is already current. The guard is load-bearing, not an
# optimisation:
#
#     show -> _refresh_buffers -> `$R_bufs.list selection set $current`
#
# ttk::treeview fires <<TreeviewSelect>> from `selection set`
# unconditionally, even when the selection did not change. The event is
# VIRTUAL, so it is QUEUED rather than delivered inline — and the very
# next thing `show` does is _fetch_backlog, whose ::vapi::get_json runs
# a NESTED EVENT LOOP (http::wait). That loop dispatches the queued
# event, which calls _pick, which calls show, which queues another one.
# The desktop wedges in that cycle the moment a live gateway returns a
# non-empty buffer list.
#
# It never showed up in the suites because no test built the buffer LIST
# and then drove the event loop — the same blind spot that hid the
# `selection_set` spelling bug documented in _refresh_buffers.
#
# Pre-existing: the `selection set` line arrived in 2038979, well before
# the Instances pane. Found while trying to drive that pane live.
#
# An EXPLICIT open (the buffer list's context menu) goes through
# _pick_selected, which calls `show` directly — re-opening the current
# buffer still re-renders. The guard is on the EVENT path only.
proc ::virc::_pick {w} {
    variable buffers
    variable current
    set sel [$w selection]
    if {![llength $sel]} { return }
    # The treeview item id IS the buffer key.
    set k [lindex $sel 0]
    if {$k eq $current} { return }
    if {$k in $buffers} { show $k }
}

proc ::virc::_pick_selected {} {
    variable R_bufs
    variable buffers
    if {![winfo exists $R_bufs.list]} { return }
    set sel [$R_bufs.list selection]
    if {![llength $sel]} { return }
    set k [lindex $sel 0]
    if {$k in $buffers} { show $k }
}

proc ::virc::mark_read {{k ""}} {
    variable current
    variable unread
    variable activity
    variable seen
    variable read_marker
    if {$k eq ""} { set k $current }
    if {$k eq ""} { return }
    set unread($k) 0
    set activity($k) 0
    if {[info exists seen($k)]} {
        set read_marker($k) $seen($k)
        _save_read_marker
    }
    _refresh_buffers
}

# Persist the read markers into the desktop state blob (buffer key →
# line id), so a marker survives the app closing.
proc ::virc::_save_read_marker {} {
    variable read_marker
    if {![array exists read_marker]} { return }
    ::vstate::put irc_read_marker [array get read_marker]
}

proc ::virc::_load_read_marker {} {
    variable read_marker
    set stored [::vstate::get irc_read_marker {}]
    if {[catch { dict size $stored }]} { return }
    dict for {k id} $stored {
        if {[string is integer -strict $id]} { set read_marker($k) $id }
    }
}

# ── showing a buffer ────────────────────────────────────────────────

proc ::virc::show {k} {
    variable current
    variable meta
    variable topics
    variable linebuf
    variable earliest
    if {![info exists meta($k)]} { return 0 }
    # Persist the buffer we are leaving before its marker goes stale.
    if {$current ne "" && $current ne $k} { _save_read_marker }
    set current $k
    mark_read $k
    # Load the newest page of history and retain it for paging. The
    # transcript re-renders from this list, so reading older lines later
    # does not lose the live ones already drawn.
    set linebuf($k) [_fetch_backlog $k ""]
    set earliest($k) [_first_id $linebuf($k)]
    _render_transcript
    _refresh_roster $k
    _refresh_topic $k
    _refresh_status
    _refresh_buffers
    # A split pinned to the buffer we just switched TO would otherwise
    # show a stale copy beside a fresh one, which is the single most
    # confusing thing a split view can do.
    variable split_w
    if {[info exists split_w($k)]} { _paint_split $k }
    return 1
}

# Fetch one page of a buffer's backlog, oldest-first. `before` empty →
# the newest page. Returns the lines as dicts, and stamps `seen` with the
# newest id so the read marker advances against what was loaded.
proc ::virc::_fetch_backlog {k before} {
    variable meta
    variable seen
    set m $meta($k)
    set q [format "network=%s&presence=%s&buffer=%s&limit=200" \
        [_urlenc [dict get $m network]] \
        [_urlenc [dict get $m presence]] \
        [_urlenc [dict get $m buffer]]]
    if {$before ne ""} { append q "&before=$before" }
    if {[catch { ::vapi::get_json "/api/irc/backlog?$q" } resp]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {irc: backlog unavailable: %s}] $resp] }
        return {}
    }
    if {![dict exists $resp lines]} { return {} }
    set lines [dict get $resp lines]
    if {[llength $lines]} {
        set last [lindex $lines end]
        set id ""
        catch { set id [dict get $last id] }
        # `seen` tracks the newest line we know; paging only loads older
        # ids, so never let it move backwards.
        if {[string is integer -strict $id]
            && (![info exists seen($k)] || $id > $seen($k))} {
            set seen($k) $id
        }
    }
    return $lines
}

proc ::virc::_first_id {lines} {
    if {![llength $lines]} { return 0 }
    set id ""
    catch { set id [dict get [lindex $lines 0] id] }
    if {[string is integer -strict $id]} { return $id }
    return 0
}

# Re-render the current buffer from `linebuf`, drawing the read marker as
# a rule between what was seen and what arrived later.
proc ::virc::_render_transcript {} {
    variable current
    variable linebuf
    variable read_marker
    set w [transcript]
    if {![winfo exists $w]} { return }
    # `_repaint_current` can reach here before any buffer has been
    # loaded — /ignore on a fresh pane. A bare `linebuf($current)` read
    # throws, and this proc is called from a slash command, so the throw
    # would surface as an unexplained error in the composer.
    if {![info exists linebuf($current)]} { return }
    $w configure -state normal
    $w delete 1.0 end
    set mark ""
    if {[info exists read_marker($current)]} { set mark $read_marker($current) }
    set n [llength $linebuf($current)]
    set i 0
    foreach line $linebuf($current) {
        render_line $w $line
        set id ""
        catch { set id [dict get $line id] }
        if {$mark ne "" && [string is integer -strict $mark]
            && [string is integer -strict $id] && $id == $mark && $i < $n - 1} {
            _read_rule $w
        }
        incr i
    }
    $w configure -state disabled
    $w see end
}

proc ::virc::_read_rule {w} {
    $w configure -state normal
    $w insert end "── read up to here ──\n" irc_readrule
    $w configure -state disabled
}

# Scrollback paging: fetch the page before the earliest loaded line and
# prepend it, keeping the view anchored on what it was showing.
proc ::virc::_load_older {k} {
    variable linebuf
    variable earliest
    if {![info exists earliest($k)]} { return }
    set before $earliest($k)
    if {$before <= 1} { return }
    set older [_fetch_backlog $k $before]
    if {![llength $older]} { return }
    set added [llength $older]
    set linebuf($k) [concat $older $linebuf($k)]
    set earliest($k) [_first_id $linebuf($k)]
    _render_transcript
    # The line that used to be first is now at index `added + 1`; hold it
    # at the top of the viewport so the reader keeps their place.
    [transcript] yview [expr {$added + 1}].0
}

proc ::virc::_maybe_load_older {{w ""}} {
    variable current
    variable meta
    variable earliest
    if {$w eq ""} { set w [transcript] }
    set k $current
    if {$k eq "" || ![info exists meta($k)]} { return }
    if {![info exists earliest($k)]} { return }
    if {[lindex [$w yview] 0] > 0.0} { return }
    _load_older $k
}

# One implementation, in ::vsafe, because the Instances pane needs the
# same encoder for cron job ids and must not depend on the IRC module.
proc ::virc::_urlenc {s} {
    return [::vsafe::urlenc $s]
}

# ── roster and topic ────────────────────────────────────────────────

# Ops first, then voiced, then everyone else, each group alphabetical —
# the order every IRC client has used for thirty years, and the reason
# is that the people who can act on your request are the ones you are
# scanning the list for.
# The bare nick out of a roster entry.
#
# THE CAP THAT MADE THIS NECESSARY: client.rs requests
# `userhost-in-names`, so NAMES replies carry `nick!user@host`, not
# `nick`. The roster therefore held — and DISPLAYED — entries like
# `@jackma_veles!~u@example.com`, and `selected_nick` stripped only the
# `@%+` prefix chars. So every roster action sent the whole mask where a
# nick belongs: whois, op, deop, voice, devoice and kick were all
# addressed to a target no server will match.
#
# The mask is kept in `rosters()` — it is what a ban wants — and stripped
# here, at the two places that need a nick: what is drawn, and what is
# acted on.
proc ::virc::nick_of {member} {
    set m [string trimleft $member "@%+~&"]
    set i [string first "!" $m]
    if {$i >= 0} { set m [string range $m 0 [expr {$i - 1}]] }
    return $m
}

# The prefix chars a member carries ("@", "+", "@+" with multi-prefix),
# which are what says who is opped and have to survive into the display.
proc ::virc::prefix_of {member} {
    set out ""
    foreach c [split $member ""] {
        if {$c ni {@ % + ~ &}} break
        append out $c
    }
    return $out
}

# What the roster list shows: the prefix, then the nick, and no hostmask.
proc ::virc::roster_label {member} {
    return "[prefix_of $member][nick_of $member]"
}

proc ::virc::sort_roster {members} {
    set rank {@ 0 % 1 + 2}
    set keyed {}
    foreach m $members {
        set p [string index $m 0]
        set r 3
        if {[dict exists $rank $p]} { set r [dict get $rank $p] }
        # Sort on the NICK. With userhost-in-names the entry is
        # `nick!user@host`, and sorting the whole mask puts a nick's
        # ordering at the mercy of its ident.
        lappend keyed [list $r [string tolower [nick_of $m]] $m]
    }
    # lsort is stable, so sorting by NAME first and by rank second
    # leaves names ordered within each rank. Doing it the other way
    # round sorts by rank and then destroys that ordering.
    set out {}
    foreach e [lsort -index 0 -integer [lsort -index 1 $keyed]] {
        lappend out [lindex $e 2]
    }
    return $out
}

# ── keeping the roster current between snapshots ─────────────────────
#
# PENDING recorded this as "rosters lag": the snapshot IS republished on
# join/part/mode, but the pane only re-read it in `refresh_state`, so a
# roster stayed stale until the buffer was reopened — and the entry
# guessed the pane was "ignoring what the tap already carries".
#
# Checked rather than believed, and the entry was wrong about the cause.
# The SSE stream carries `irc`, `irc-live` and `irc-lagged` and nothing
# else; there is no state event on it at all. The snapshot is HTTP-only.
# Nothing was ever going to tell the pane the roster had moved.
#
# Re-polling /api/irc/state per join would be a round trip per line on a
# busy channel. The answer every IRC client has used for thirty years is
# to apply the delta locally — and tap.rs already expects exactly that:
# "a quit is not scoped to one channel — the client fans it out to
# whichever of its buffers had that nick in the roster".
#
# `refresh_state` stays the reconciling source of truth. These deltas
# only have to be right until the next snapshot, not forever.

# Apply one event to the rosters it affects. Returns the buffer keys
# that changed, so the caller repaints only those.
proc ::virc::_roster_apply {d} {
    variable rosters
    set kind "" ; catch { set kind [dict get $d kind] }
    set nick "" ; catch { set nick [dict get $d nick] }
    if {$nick eq "" || $kind eq ""} { return {} }

    set network "" ; catch { set network [dict get $d network] }
    set presence "" ; catch { set presence [dict get $d presence] }
    set buffer "" ; catch { set buffer [dict get $d buffer] }
    set k [key $network $presence $buffer]

    switch -exact -- $kind {
        join {
            if {![info exists rosters($k)]} { return {} }
            # A joining user carries no prefix — no mode yet. Match on
            # the NICK, not the whole entry: with userhost-in-names the
            # snapshot holds `nick!user@host`, so comparing entries
            # would let a rejoin appear twice under two spellings.
            foreach m $rosters($k) {
                if {[nick_of $m] eq $nick} { return {} }
            }
            lappend rosters($k) $nick
            return [list $k]
        }
        part - kick {
            if {![info exists rosters($k)]} { return {} }
            # A kick names its VICTIM in `target`; `nick` is whoever did
            # the kicking. Removing the kicker instead would be wrong
            # and quite something to watch.
            set who $nick
            if {$kind eq "kick"} {
                set who "" ; catch { set who [dict get $d target] }
                if {$who eq ""} { return {} }
            }
            return [_roster_drop $k $who]
        }
        quit {
            # Not scoped to a channel. Drop the nick from every buffer
            # on the SAME connection — not from every buffer everywhere,
            # or a quit on one network would empty rosters on another.
            set touched {}
            foreach kk [array names rosters] {
                if {![string match "$network/$presence/*" $kk]} { continue }
                foreach t [_roster_drop $kk $nick] { lappend touched $t }
            }
            return $touched
        }
        nick {
            # `text` carries the new nick (tap.rs, EventKind::Nick).
            set new "" ; catch { set new [dict get $d text] }
            if {$new eq ""} { return {} }
            set touched {}
            foreach kk [array names rosters] {
                if {![string match "$network/$presence/*" $kk]} { continue }
                set out {} ; set hit 0
                foreach m $rosters($kk) {
                    if {[nick_of $m] eq $nick} {
                        lappend out "[prefix_of $m]$new" ; set hit 1
                    } else {
                        lappend out $m
                    }
                }
                if {$hit} { set rosters($kk) $out ; lappend touched $kk }
            }
            return $touched
        }
        mode {
            # A mode change rewrites who is opped, and deriving that
            # from the mode string means reimplementing PREFIX and
            # ISUPPORT parsing in Tcl when the engine has already done
            # it. Ask for the snapshot instead: mode changes are rare
            # enough that one round trip is the right trade, which is
            # exactly what is NOT true of join and part.
            _resync_rosters
            return {}
        }
    }
    return {}
}

# Remove a nick from one buffer's roster. Returns the key if it changed.
proc ::virc::_roster_drop {k nick} {
    variable rosters
    if {![info exists rosters($k)]} { return {} }
    set out {}
    set hit 0
    foreach m $rosters($k) {
        if {[nick_of $m] eq $nick} { set hit 1 ; continue }
        lappend out $m
    }
    if {!$hit} { return {} }
    set rosters($k) $out
    return [list $k]
}

# Re-read the snapshot, coalesced: a mode burst (+ooo when a netjoin
# reopers everybody) must not become one HTTP request per mode.
proc ::virc::_resync_rosters {} {
    variable resync_pending
    if {[info exists resync_pending] && $resync_pending} { return }
    set resync_pending 1
    after 400 ::virc::_resync_now
}

proc ::virc::_resync_now {} {
    variable resync_pending 0
    variable current
    if {[catch { refresh_state } resync_err]} {
        puts stderr "veles-desktop: irc resync failed: $resync_err"
        return
    }
    catch { _refresh_roster $current }
}

proc ::virc::_refresh_roster {k} {
    variable R_roster
    variable rosters
    variable aways
    if {![winfo exists $R_roster.list]} { return }
    $R_roster.list delete 0 end
    if {![info exists rosters($k)]} { return }
    set gone {}
    if {[info exists aways($k)]} { set gone $aways($k) }
    foreach m [sort_roster $rosters($k)] {
        set label [roster_label $m]
        if {[lsearch -exact $gone $m] >= 0} { append label " (away)" }
        $R_roster.list insert end $label
    }
}

proc ::virc::_refresh_topic {k} {
    variable R_view
    variable topics
    variable meta
    if {![winfo exists $R_view.topic]} { return }
    set t ""
    if {[info exists topics($k)]} { set t [strip_formatting $topics($k)] }
    # Prefix with who you are here. The composer sends as this presence
    # and the ops act as it, so it belongs where you look before typing
    # rather than somewhere you have to go and check.
    if {[info exists meta($k)]} {
        set m $meta($k)
        set head "[dict get $m buffer] · as [dict get $m presence]"
        set t [expr {$t eq "" ? $head : "$head — $t"}]
    }
    # Typing indicator rides the same label. The topic is always visible
    # and never scrolls away; a transcript line would be buried by the
    # very chatter that made it interesting. Ephemeral state, ephemeral
    # place — it vanishes from view the moment the list empties.
    variable typing
    if {[info exists typing($k)] && [llength $typing($k)]} {
        append t "  ·  ✍ [join [lsort -dictionary $typing($k)] ", "]"
    }
    $R_view.topic configure -text $t
}

# The connection state of the current buffer's network, under the
# topic. WeeChat keeps the server's state permanently visible (server
# buffer, status bar); here it used to exist only as `●`/`○` lines in
# `*` that scroll away, so between a drop and the next registration the
# pane showed nothing at all. State comes from two places: the tap's
# connected/disconnected/connecting events (live) and the `connected`
# flag of /api/irc/state (attach-time seed) — `refresh_state` and
# `_ingest` both write `netstate`, this paints it.
proc ::virc::_refresh_status {} {
    variable R_view
    variable current
    variable meta
    variable netstate
    variable own_nick
    if {![winfo exists $R_view.status]} { return }
    set text ""
    set fg $::vt::T(fg_dim)
    if {$current eq [merged_key]} {
        # The merged view belongs to no single network: summarize them
        # all instead of pretending to be one.
        set total 0
        set up 0
        if {[array exists netstate]} {
            foreach nk [array names netstate] {
                incr total
                if {[dict get $netstate($nk) kind] eq "connected"} { incr up }
            }
        }
        set text "merged server buffers — $up/$total connected"
        if {$total == 0} {
            set fg $::vt::T(fg_dim)
        } elseif {$up == $total} {
            set fg $::vt::T(green)
        } elseif {$up > 0} {
            set fg $::vt::T(gold)
        } else {
            set fg $::vt::T(red)
        }
    } elseif {$current ne "" && [info exists meta($current)]} {
        set m $meta($current)
        set network [dict get $m network]
        set presence [dict get $m presence]
        set nk "$network/$presence"
        set nick ""
        if {[info exists own_nick($nk)]} { set nick $own_nick($nk) }
        set head [expr {$nick eq "" ? $nk : "$nk · $nick"}]
        if {[info exists netstate($nk)]} {
            set st $netstate($nk)
            switch -- [dict get $st kind] {
                connected {
                    set text "$head — ● connected"
                    set fg $::vt::T(green)
                }
                connecting {
                    set text "$head — ◌ [dict get $st text]"
                    set fg $::vt::T(gold)
                }
                disconnected {
                    set text "$head — ○ disconnected"
                    set fg $::vt::T(red)
                }
            }
        } else {
            # No lifecycle event seen yet on this stream — say so
            # instead of implying the network is up.
            set text "$head — ○ no connection seen"
        }
    }
    $R_view.status configure -text $text -foreground $fg
}

# ── state ───────────────────────────────────────────────────────────

proc ::virc::refresh_state {} {
    variable nets
    variable rosters
    variable topics
    variable cursor
    variable own_nick
    variable why ""
    # Unattached: no request leaves the process. The pane explains
    # itself instead of erroring against no gateway.
    if {[::vapi::unattached]} {
        set why "not-attached"
        return 0
    }
    if {[catch { ::vapi::get_json /api/irc/state } resp]} {
        # CLASSIFY IT. "gateway unreachable" was the only thing this ever
        # said, and it was usually false: the gateway answers fine, it
        # just has no /api/irc route because it was built before the IRC
        # pane existed. USER (spot test): "the IRC tab is 3 empty panes".
        if {[string match "*HTTP 404*" $resp]} {
            set why "gateway-too-old"
        } elseif {[string match "*HTTP 401*" $resp] || [string match "*HTTP 403*" $resp]} {
            set why "unauthorized"
        } else {
            set why "unreachable"
        }
        catch { ::vstatus::set_msg [format [::vi18n::mc {irc: %s}] [_why_short]] }
        return 0
    }
    set nets $resp
    if {![dict exists $resp networks] || ![llength [dict get $resp networks]]} {
        set why "not-configured"
        return 0
    }
    foreach n [dict get $resp networks] {
        set network [dict get $n network]
        set presence [dict get $n presence]
        # The reader's own nick on THIS connection, for highlights.
        set nick ""
        catch { set nick [dict get $n nick] }
        if {$nick ne ""} { set own_nick($network/$presence) $nick }
        # Seed the connection state the status line shows before any
        # live lifecycle event arrives. JSON carries true/false; the
        # test stubs carry 1/0 — accept both spellings.
        variable netstate
        set up 0
        catch {
            set c [dict get $n connected]
            set up [expr {$c eq "true" || $c eq "1"}]
        }
        set netstate($network/$presence) [dict create \
            kind [expr {$up ? "connected" : "disconnected"}] text ""]
        set known {}
        catch { set known [dict get $n buffers] }
        foreach b $known { _note_buffer [key $network $presence $b] $network $presence $b }
        catch {
            dict for {chan members} [dict get $n rosters] {
                set rosters([key $network $presence $chan]) $members
            }
        }
        catch {
            dict for {chan t} [dict get $n topics] {
                set topics([key $network $presence $chan]) $t
            }
        }
        catch {
            dict for {chan gone} [dict get $n aways] {
                set aways([key $network $presence $chan]) $gone
            }
        }
    }
    variable sendable {}
    catch { set sendable [dict get $resp sendable] }
    _load_read_marker
    _load_prefs
    # The status row is seeded from the snapshot above; paint it.
    _refresh_status
    # A first run starts live rather than replaying the whole store.
    if {$cursor == 0 && [dict exists $resp latest_id]} {
        set cursor [dict get $resp latest_id]
    }
    return 1
}

# ── saying why there is nothing here ────────────────────────────────
#
# Three blank regions are the worst possible answer: "no IRC configured",
# "your gateway predates this pane" and "the token was refused" all look
# identical, and none of them looks like a question with an answer.
#
# USER (spot test, 2026-08-25): "the IRC tab is 3 empty panes". On that
# machine BOTH of the first two were true — the running gateway was 19
# hours old so /api/irc/state 404'd, and the config has no
# [platforms.irc] at all (its IRC ran through an ircx plugin that is no
# longer installed).

proc ::virc::_why_short {} {
    variable why
    switch -- $why {
        gateway-too-old { return "this gateway has no /api/irc — rebuild it" }
        unauthorized    { return "the gateway refused the token" }
        unreachable     { return "gateway unreachable" }
        not-configured  { return "no IRC connection on this gateway" }
        not-attached    { return "not attached — press Attach in the Instances tab" }
    }
    return ""
}

# The long form, written into the transcript where there is room for it.
#
# Built from BRACED line lists rather than quoted strings: the text is
# full of quotes and brackets (it is mostly TOML), and every one of them
# would need escaping inside "..." — which is how the first draft of this
# ended the string early and took the whole suite down with it.
proc ::virc::_why_long {} {
    variable why
    switch -- $why {
        gateway-too-old {
            return [join {
                {This gateway has no /api/irc endpoints.}
                {}
                {It was built before the IRC pane existed. Rebuild and restart it:}
                {}
                {    nix develop -c cargo build --release -p veles-cli}
                {    systemctl --user restart veles-gateway}
                {}
                {(scripts/veles-desktop.sh refuses to start against an engine older}
                {than the source. Launching `veles desktop` directly skips that.)}
            } "\n"]
        }
        unauthorized {
            return [join {
                {The gateway refused this desktop's token.}
                {}
                {Check VELES_TOKEN against DESKTOP_BEARER_TOKEN / DESKTOP_PEER_TOKENS}
                {on the gateway.}
            } "\n"]
        }
        unreachable {
            return [join {
                {The gateway is not answering.}
                {}
                {Start it, or check VELES_API if you meant to reach a remote one.}
            } "\n"]
        }
        not-configured {
            set txt [join {
                {No IRC connection on this gateway.}
                {}
                {This pane shows what the GATEWAY's own IRC connection sees, so it}
                {needs one. Add to @CONFIG@:}
                {}
                {    [platforms.irc]}
                {    enabled  = true}
                {    server   = "irc.example.net"}
                {    port     = 6697}
                {    use_tls  = true}
                {    nickname = "yourbot"}
                {    channels = "#chan"}
                {}
                {To connect several networks at once, list each one in a}
                {[[platforms.irc.networks]] entry. Every entry inherits the top-level}
                {values it does not override, and `name` is what labels its buffers.}
                {This is TOML-only — the IRC_* env vars describe one network and}
                {cannot describe several:}
                {}
                {    [[platforms.irc.networks]]}
                {    name     = "libera"}
                {    server   = "irc.libera.chat"}
                {    nickname = "yourbot"}
                {    channels = "#chan"}
                {}
                {    [[platforms.irc.networks]]}
                {    name     = "oftc"}
                {    server   = "irc.oftc.net"}
                {    nickname = "yourbot"}
                {}
                {Then restart the gateway. What you type in this pane goes out as the}
                {bot.}
                {}
                {NOTE: the `ircx-platform` plugin is a DIFFERENT thing and this pane}
                {does not see it — the tap lives in the built-in [platforms.irc]}
                {adapter.}
            } "\n"]
            # Two corrections, both the same mistake in different clothes.
            # The file is substituted, not written into the text: this pane
            # tells the operator which config.toml to edit, and ~/.veles is
            # not where the gateway has read it since the 2026-09-04 XDG
            # split. And `tls` is not a key `apply_mapping` reads — the one
            # it reads is `use_tls` — so the snippet as printed configured
            # a knob that did nothing, which is the bug the setup wizard had
            # (60786dd): a snippet IS a config surface.
            return [string map [list @CONFIG@ [::vpaths::config_file]] $txt]
        }
    }
    return ""
}

# Put the explanation where someone looking at an empty pane will read
# it: in the transcript, which is the big one.
proc ::virc::_explain_empty {} {
    variable R_view
    variable why
    if {$why eq ""} { return 0 }
    if {![winfo exists $R_view.body]} { return 0 }
    $R_view.body delete 1.0 end
    $R_view.body insert end "IRC: [_why_short]\n\n" irc_meta
    $R_view.body insert end [_why_long]
    $R_view.body insert end "\n"
    catch { $R_view.topic configure -text [::vi18n::mc {IRC — nothing to show}] }
    return 1
}

# ── clipboard helpers ───────────────────────────────────────────────

proc ::virc::_copy_selection {} {
    variable R_view
    if {[catch { $R_view.body get sel.first sel.last } t]} { return }
    clipboard clear ; clipboard append $t
}
proc ::virc::_copy_buffer_name {} {
    variable current
    variable meta
    if {![info exists meta($current)]} { return }
    clipboard clear ; clipboard append [dict get $meta($current) buffer]
}
proc ::virc::_copy_nick {} {
    variable R_roster
    set i [lindex [$R_roster.list curselection] 0]
    if {$i eq ""} { return }
    clipboard clear ; clipboard append [nick_of [$R_roster.list get $i]]
}
proc ::virc::_see_end {} { catch { [transcript] see end } }

# ── boot ────────────────────────────────────────────────────────────

proc ::virc::boot {} {
    variable buffers
    # Regions register with ::vdock so tabs, undocking and the focus
    # ring all know about them without dock.tcl naming IRC anywhere.
    ::vdock::register .ircbufs   {list $::virc::R_bufs.list}   ::virc::build_bufs
    ::vdock::register .ircview   {list $::virc::R_view.body $::virc::R_view.input} ::virc::build_view
    ::vdock::register .ircroster {list $::virc::R_roster.list} ::virc::build_roster
    build_bufs .ircbufs
    build_view .ircview
    build_roster .ircroster
    refresh_state
    connect
    if {[llength $buffers]} {
        show [lindex $buffers 0]
    } else {
        # Nothing to show: say which of the several reasons it is. An
        # empty pane that explains itself is the difference between a bug
        # report and a config change.
        variable why
        if {$why eq ""} { set why "not-configured" }
        _explain_empty
    }
}
