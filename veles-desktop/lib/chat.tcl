# veles-desktop/lib/chat.tcl — transcript + composer.
#
# Features beyond the M5 skeleton (hermes-desktop parity + more):
#  - expandable reasoning blocks: click the ⟨reasoning⟩ header to
#    collapse/expand (hermes shows/hides reasoning per turn)
#  - tool rounds: ⚙ name… with the result indented, truncated
#  - per-turn usage line (model · prompt→completion tokens)
#  - streaming deltas with live reasoning rendering
#  - Stop button wired to http::reset (cancel-on-disconnect server side)
#
# The transcript widget is .mid.chatwrap.chat.log; the composer is .mid.chatwrap.chat.input.

# Links and block headers take their clicks through ::vtext::click (a
# tap's release on a phone, the press elsewhere). Where lib/vtext.tcl is
# not loaded (suites that source this file on its own) a click is the
# press, as markdown.tcl does; vtext.tcl's definition wins wherever it is.
if {![llength [info commands ::vtext::click]]} {
    namespace eval ::vtext {}
    proc ::vtext::click {w tag script} { $w tag bind $tag <Button-1> $script }
}

package require http

namespace eval ::vchat {
    variable session ""
    variable busy 0
    ;# Sessions with a turn in flight, as sid -> 1. `busy` is now just
    ;# "does the session ON SCREEN have one", derived from this.
    ;#
    ;# It is this window's BELIEF, and `sync_running` reconciles it with
    ;# the engine's cross-process answer on every session listing —
    ;# a turn whose end this window never saw used to keep its ▶ forever.
    variable running {}
    ;# sid -> when this window came to believe it (ms), or 0 once the
    ;# engine has confirmed it. See _mark_running / sync_running.
    variable running_since
    array set running_since {}
    ;# Which session the SSE event being dispatched belongs to. Set by
    ;# ::vapi before each callback; "" outside a callback.
    variable stream_session ""
    ;# Per-session transcript anchor for the streaming region, as
    ;# sid -> index. Was one global, which is meaningless once the
    ;# widget can be showing a different session than the stream.
    variable turn_anchor {}
    variable route ""
    variable turn_prompt 0
    variable turn_completion 0
    variable live_rtag ""
    ;# Header tag for the LIVE reasoning block. The body (live_rtag) is
    ;# elided when "reasoning" is off, but a header with nothing under it
    ;# is the difference between COLLAPSING and VANISHING — without this
    ;# the live thinking disappeared with nothing left to click. Named
    ;# live_r_hdr_* so forget_raw's live_r_* reap catches it too.
    variable live_rhdr ""
    variable show_reasoning 1
    ;# Kind of the last LIVE event painted (reasoning/delta/tool/route).
    ;# A change of kind needs a line break first — the live stream
    ;# concatenates raw chunks, and without this the reasoning welds
    ;# onto the first delta and the tool line onto the last word of
    ;# it. The turn-end re-render always had its newlines, which is
    ;# why the same turn looked fine reopened.
    variable live_last_kind ""
    ;# Text of the CURRENTLY OPEN live reasoning segment, and the list
    ;# of finished-so-far segments as {reason rounds} pairs. The engine
    ;# persists ONE assistant row per provider call, so a turn with tool
    ;# rounds carries several reasoning SEGMENTS; on_done reconciles one
    ;# collapsible per segment (R7 — live groups exactly like reload).
    variable live_rtext ""
    ;# Reasoning arrived since the last paced paint, and that paint's
    ;# timer. Per chunk on_reasoning does one string append and nothing
    ;# else; the yview, the insert, the segment-accumulator copy and the
    ;# `see end` all happen once per TICK. A burst reaches the desktop as
    ;# one frame per token whatever the gateway does, so per-token widget
    ;# work is per-token event-loop time the SSE reader does not get.
    variable live_rpend ""
    variable live_r_after ""
    ;# The live accumulator — see _live_call_slot. One entry per ROW this
    ;# turn will persist, in arrival order, because that is what the
    ;# reconcile has to rebuild the turn into.
    variable live_blocks {}
    ;# Per-session start marks + count for the live tool rounds painted
    ;# this turn (the ⚙ header at `start`, the result collapsible at
    ;# `end`). The reconcile deletes each span before rebuilding it from
    ;# live_blocks, so the live round and the reloaded one do not both
    ;# end up on screen (the "exactly one round" half of R3). sid ->
    ;# {n {mark …}}; dropped with the turn.
    variable live_tool_marks {}
    ;# The OPEN call's prose, appended per token and folded into its row
    ;# at boundaries (_live_block_settle) — on_delta runs per SSE frame,
    ;# so the accumulation it does has to be O(1).
    variable live_ptext ""
    ;# Lines painted into a session's live TURN REGION by something that
    ;# is not the stream — a background child/shell/monitor reporting the
    ;# moment it finished. on_done replaces everything from turn_anchor to
    ;# end with the reconciled blocks, so a line that landed inside that
    ;# span is deleted with it and the report vanishes from the transcript
    ;# that was showing it a second ago. Kept here so the reconcile can
    ;# put them back; dropped with the turn.
    variable live_extra {}
    ;# Whether live output pulls the view along. On by default, and it
    ;# only pulls when the view is ALREADY at the bottom — scrolling up
    ;# mid-turn disarms it, and the floating "▾ new output" button is
    ;# the way back (which re-arms). USER: "Can't scroll up during
    ;# agent output because it immediately goes to the bottom again,
    ;# needs a toggle".
    variable follow 1
    ;# Was THIS session's transcript sitting at its live edge, as of the
    ;# last time it was on screen? sid -> 0/1. `follow` above is the
    ;# window's arm; this is the per-session position it arms against.
    variable at_edge
    array set at_edge {}
    ;# THE TRANSCRIPT PER SESSION. sid -> text widget path.
    ;#
    ;# USER: "switching between sessions in desktop sucks again. Maybe
    ;# they should never get unloaded, just hidden." The render cache
    ;# below removed the HTTP round trip from a switch; it did not remove
    ;# the RENDER, and that is what costs: markdown for up to
    ;# $paint_limit messages, every image decoded again (::vmedia::forget
    ;# frees them on the way out), and scroll position, expanded
    ;# reasoning blocks and the "load older" window all thrown away.
    ;#
    ;# So a session's transcript is now a widget of its own that is
    ;# HIDDEN, not wiped: switching is one `pack forget` and one `pack`.
    ;#
    ;# The base path .mid.chatwrap.chat.log is still a real widget and
    ;# still built by `build` — it is the one the first session claims,
    ;# the one that shows the welcome text before any session is open,
    ;# and the fallback `log_for` returns for an unclaimed sid. Every
    ;# test that builds a transcript by hand at that path keeps working.
    variable logs {}
    variable logs_seq 0
    ;# sids in least-recently-SHOWN order, most recent last.
    variable logs_lru {}
    ;# How many transcripts stay resident. Each is a full text widget
    ;# with its rendered markdown and its decoded images, so "never
    ;# unloaded" needs a ceiling or a long day across fifty sessions
    ;# keeps fifty of them with nothing to bound it. The thirteenth open
    ;# drops the oldest and SAYS which — an eviction nobody is told about
    ;# is indistinguishable from the bug this replaced.
    variable logs_cap 12
    ;# What the last LRU eviction dropped, waiting to be folded into the
    ;# status line of the open that caused it.
    variable last_eviction ""
    ;# Which widget each message id was painted into, so forget_raw can
    ;# reap ONE session's messages. msg_seq is global, so ids never
    ;# collide across sessions; only the reap needed teaching.
    variable raw_w
    ;# Per-session PARSED HISTORY, so switching between two sessions is
    ;# a local repaint instead of an HTTP round trip + JSON parse +
    ;# full render from zero each time. USER: "It takes long and a lot
    ;# of cpu time to re-render a session when switching between two
    ;# active". The async refetch on every open keeps it honest: paint
    ;# from cache now, verify in the background, repaint only if the
    ;# server disagrees. Keyed sid -> {raw messages}.
    variable hist_cache
    ;# Tool rounds seen this turn, sid -> list of {name result} pairs.
    ;# Accumulated during the stream and re-rendered into the final
    ;# assistant message, so the calls that produced an answer are not
    ;# invisible once `on_done` replaces the raw streaming region.
    ;# Declared-but-unset: the first `lappend tool_rounds($sid)` makes it
    ;# an array (initialising it to "" would pin it as a scalar).
    variable tool_rounds
    ;# The INVOCATION (args JSON) of the in-flight tool call, keyed by
    ;# session, captured on ToolStart so ToolEnd can pair it with the
    ;# result — "the tool calls should also show the invocation".
    variable tool_args
    ;# Index where the current turn's assistant reply begins (raw
    ;# streaming region) — re-rendered through markdown at turn end.
    ;# Live delta region: the accumulated raw DELTA text since the last
    ;# render, the MARK anchoring where it starts, and the debounce timer.
    ;# One region at a time (there is one event stream), owned by
    ;# live_delta_sid. The region renders markdown on a debounced schedule
    ;# — not every chunk — and closes (flushes) whenever a non-delta
    ;# construct arrives, so a note or tool line never welds onto or gets
    ;# deleted with the raw streaming region.
    variable live_delta_mark ""
    variable live_delta_buf ""
    variable live_delta_after ""
    ;# How long to wait before the NEXT flush, in ms. Adaptive: set from
    ;# the duration of the last one, so an expensive render buys itself
    ;# proportionally more quiet rather than re-entering on a fixed timer
    ;# and starving the SSE reader that shares this event loop.
    variable live_delta_period 200
    variable live_delta_sid ""
    ;# Whose live region the scalars above currently hold, and the
    ;# stash for everyone else. See _live_ctx.
    variable live_ctx ""
    variable live_saved {}
}

# Tk SILENTLY IGNORES insert and delete on a `-state disabled` text
# widget. No error, no exception, `catch` returns 0 — the text simply
# never appears. Nothing in the app can notice.
#
# That made a NESTED "normal … disabled" pair catastrophic and
# invisible, and _render_collapsible_reasoning had exactly one, inside
# _paint_message's own pair: it flipped the transcript back to disabled
# on its way out, so every insert after it — the ACTUAL ANSWER and its
# trailing newline — was silently discarded. Reasoning is painted before
# the content, so the reasoning survived and the reply did not.
#
# USER: "I still don't really see responses. Sometimes part of a
# response is visible but when I reload the session view I see only
# reasoning blocks and my prompts." Live deltas came in through
# log_insert (its own top-level pair, so they showed); on_done then
# repainted the message through emit_message and the answer vanished.
#
# Save and RESTORE rather than force-disable, so nesting composes.
proc ::vchat::_writable {w script} {
    set prev [$w cget -state]
    $w configure -state normal
    set rc [catch { uplevel 1 $script } res opts]
    catch { $w configure -state $prev }
    if {$rc} { return -options $opts $res }
    return $res
}

# The single run-insert chokepoint for the transcript.
#
# Every byte the chat renderer writes into the log widget goes through
# here (markdown's ::vmd::_emit and media's ::vmedia::_emit are the
# sibling chokepoints for the bodies they fill). Before this, ≈19 raw
# `$w insert end` sites in chat.tcl — plus 37 in markdown.tcl and 7 in
# media.tcl — each decided independently how a run lands; whitespace and
# tag handling were therefore spread across ~63 call sites (the phase-2
# "no insert chokepoint" complaint). Now "how a transcript run is
# written" is one proc.
#
# Pure pass-through on purpose: callers already hold the widget writable
# (via _writable), so this adds no state toggling and no per-run cost.
# tags defaults to {} so a bare separator newline needs no empty list.
proc ::vchat::_run {w text {tags {}}} {
    $w insert end $text $tags
}

# The in-place sibling: a targeted splice at an index rather than an
# append at end (the collapsible header replace). Same chokepoint idea —
# one proc knows how a run is written — for the one write that is not an
# append.
proc ::vchat::_run_at {w index text {tags {}}} {
    $w insert $index $text $tags
}

# The block emitter — the single entry point for writing ONE transcript
# block, and the only proc besides _paint_message that owns block
# separation. This is the phase-2 "one emitter taking (kind, body, tags)"
# chokepoint: a block is emitted whole, separated from what precedes it
# by exactly one blank line (R1), with its body written through _run.
#
#   kind    the block's identity (note, empty, message…). Separation is
#           tail-driven in _block_sep, so kind does not re-decide it; it
#           names the block at the call site and is the hook a future
#           per-kind policy would hang off.
#   body    the literal run text for a simple block.
#   tags    the run tags for that body.
#   render  for a multi-run block (a collapsible, a tool round), a
#           pre-substituted command LIST that fills the body under this
#           same writable+separation umbrella — so even a rich block is
#           separated once, here, and never by its own renderer.
#
# Opens the widget writable itself, so a caller writing a standalone
# block needs no _writable wrapper of its own.
proc ::vchat::_emit_block {kind body tags {render {}}} {
    set w [::vchat::log]
    if {$w eq "" || ![winfo exists $w]} { return }
    _writable $w {
        _block_sep $w
        if {$render ne ""} {
            {*}$render
        } else {
            _run $w $body $tags
        }
    }
}

proc ::vchat::log_insert {txt tag {sid ""}} {
    # POSITION BEFORE INSERT. Checking yview after the insert is wrong:
    # one big chunk (an elided reasoning block, a long tool result)
    # lands and the view is no longer "at the bottom" — so following
    # silently disarmed mid-stream and never came back (USER: "follow
    # doesn't stick — it only follows for a while"). At-bottom has to
    # mean "the user was at the live edge when this arrived".
    # `sid` names whose transcript this line belongs to; empty means the
    # session on screen. A live-region caller passes its own, because a
    # hidden session's tool lines go into ITS widget now.
    if {$sid eq ""} {
        set w [::vchat::log]
    } else {
        set w [::vchat::log_for $sid]
    }
    # No transcript for this session — it is not loaded, so there is
    # nothing on screen or off it that this line could go into. The row
    # is persisted server-side regardless; opening the session paints it.
    if {$w eq "" || ![winfo exists $w]} { return }
    _at_bottom_prestash $w
    _writable $w {
        _run $w $txt $tag
    }
    _maybe_follow $w
}

# Was the view at the live edge before the newest insert? Captured by
# log_insert (and the other live painters) UP FRONT; empty string when
# nothing is pending.
namespace eval ::vchat {
    variable follow_was_bottom ""
}

# `w` names the transcript being painted — a hidden session's own widget
# is a legitimate target now, and scroll bookkeeping is the one thing
# that genuinely belongs to the transcript ON SCREEN. A hidden widget's
# yview is whatever it was when it was packed away; reading it is
# meaningless and acting on it would fight the user's scroll position
# the moment they switch back.
proc ::vchat::_at_bottom_prestash {{w ""}} {
    variable follow_was_bottom
    variable session
    if {$w eq ""} { set w [::vchat::log] }
    # NOT `[::vchat::log]` — that follows `event_session`, so during a
    # background turn it names the HIDDEN widget and this guard would
    # wave it through. The question is only ever "is this the widget the
    # user is looking at", which is the VISIBLE session's.
    if {![winfo exists $w] || $w ne [::vchat::log_for $session]} {
        set follow_was_bottom ""
        return
    }
    # INTENT, NOT POSE. The latch is set by arrival and by jumps to the
    # end; pose is only consulted when no intent is recorded for this
    # session yet (a transcript this window has never scrolled, painted
    # by a first fill). _maybe_follow itself re-arms the latch on every
    # scroll it performs, so a user who never scrolls away stays
    # following across pane docks, chips strips, sash moves — the width
    # changes that re-wrap the transcript and silently drop a
    # bottom-pinned yview fraction to ~0.96 (measured under Xvfb; the
    # 2026-09-20 round-2 "follow STILL unsticks after tool calls"
    # report).
    variable at_edge
    if {[info exists at_edge($session)]} {
        set follow_was_bottom $at_edge($session)
    } else {
        set follow_was_bottom [expr {[lindex [$w yview] 1] >= 0.999}]
    }
}

# Follow the stream ONLY when following is armed AND the view was at
# the live edge when this content arrived — scrolling up mid-turn is a
# deliberate act and must not be yanked back. While content arrives
# from a scrolled-up position, the floating "▾ new output" button is
# the way back; the bar's "follow" checkbox disarms even the
# at-bottom rule.
proc ::vchat::_maybe_follow {{w ""}} {
    variable follow
    variable follow_was_bottom
    variable session
    if {$w eq ""} { set w [::vchat::log] }
    # Scrolling a transcript nobody is looking at, and flipping the
    # window's follow button on its behalf, are both wrong: the button
    # is window chrome and belongs to the session ON SCREEN — which is
    # `session`, not `[::vchat::log]` (that one follows the EVENT's
    # session and is the hidden widget during a background turn).
    if {![winfo exists $w] || $w ne [::vchat::log_for $session]} {
        set follow_was_bottom ""
        return
    }
    set at_bottom $follow_was_bottom
    set follow_was_bottom ""
    if {$at_bottom eq ""} {
        # No pre-captured position (a painter that did not stash —
        # emit_message goes through mark/collapsible machinery): fall
        # back to reading it now, which is right for the small,
        # non-eliding appends this path makes.
        set at_bottom [expr {[lindex [$w yview] 1] >= 0.999}]
    }
    if {!$follow} {
        _show_follow_btn
        return
    }
    if {$at_bottom} {
        _hide_follow_btn
        $w see end
        # ARRIVAL RE-ARMS THE LATCH. The scroll this proc just performed
        # puts the view at the live edge by construction; recording it
        # here is what makes the latch self-sustaining across the
        # geometry noise that invalidates a pose read (see
        # _at_bottom_prestash). Reading the fraction back here would
        # reintroduce pose, and pose is not intent.
        variable at_edge
        set at_edge($session) 1
    } else {
        _show_follow_btn
    }
}

# Scroll-follow armed but the user is not at the bottom: say there is
# new output instead of dragging them. One widget, shown/hidden, never
# created twice. It never shows AT the bottom — there is nothing new
# there, and a button would lie.
# Show or hide the floating "▾ new output" button to match where the
# visible transcript actually is. A switch back to a kept session must
# not assume either answer: it is at the bottom or it is not, and the
# button is the thing that says so.
proc ::vchat::_sync_follow_btn {} {
    set w [::vchat::log]
    if {$w eq "" || ![winfo exists $w]} { _hide_follow_btn ; return }
    if {[lindex [$w yview] 1] >= 0.999} {
        _hide_follow_btn
    } else {
        _show_follow_btn
    }
}

# ── the live edge is a property of the SESSION, not of the window ────
#
# USER 2026-09-17: "the 'following' unsticks when switching sessions and
# has to be clicked/scrolled again each time it's re-attached".
#
# `follow` is window chrome: it says whether arrival is allowed to pull
# the view at all. Where a given transcript is sitting is that
# transcript's own business, and nothing recorded it. _maybe_follow
# rightly will not scroll a widget nobody is looking at, so a hidden
# session's text grows past its viewport — and the switch back then read
# a yview that was no longer at the bottom and offered the ▾ button to a
# user who had never stopped following.
#
# So: stash it on the way OUT, off the widget that is still mapped.
# Asking an unmapped text widget where it is sitting is asking the wrong
# oracle — its yview is computed against a geometry the geometry manager
# stopped maintaining when `pack forget` ran.
#
# 2026-09-20, round 2 ("the following STILL unsticks sometimes"): the
# stash was a pose read, and POSE IS NOT INTENT. Measured under Xvfb: a
# transcript pinned at the bottom (yview[1] == 1.0) whose WIDTH changes
# — the right pane docking beside it on a preview/focus tool call, the
# chips strip packing on, a sash move — re-wraps and settles at 0.964
# without any user scroll and without any insert. The next prestash
# read that as "the user left" and disarmed follow exactly when tool
# output was arriving. at_edge is therefore a LATCH: set by arrival and
# by every jump-to-end, cleared ONLY by the user's own scrolling
# (_read_pose, armed from the log's input bindings). Geometry noise
# cannot touch it.
proc ::vchat::_stash_edge {sid} {
    variable at_edge
    if {$sid eq ""} return
    set w [log_for $sid]
    if {$w eq "" || ![winfo exists $w]} return
    # OR, not assign: once the user is at the live edge, only their own
    # scrolling (or dropping the transcript) may say otherwise.
    if {[lindex [$w yview] 1] >= 0.999} { set at_edge($sid) 1 }
}

# …and put it back on the way IN. You left at the live edge, you come
# back at the live edge, still following; you left scrolled up, you come
# back exactly where you were, with the button to say what arrived.
#
# `follow` still wins over both: off means reading beats arrival, and a
# switch is not a reason to overrule that.
proc ::vchat::_restore_edge {sid} {
    variable at_edge
    variable follow
    set w [log_for $sid]
    if {$w eq "" || ![winfo exists $w]} { _hide_follow_btn ; return }
    if {$follow && [info exists at_edge($sid)] && $at_edge($sid)} {
        $w see end
        _hide_follow_btn
        return
    }
    _sync_follow_btn
}

# Record where the ON-SCREEN transcript now sits — the pose OFF THE
# REAL widget command, never through the gate (the gate's own
# bookkeeping must not re-enter it; the reader's first shape recursed
# a thousand interpreter frames per scroll and left the latch holding
# whatever an intermediate frame read mid-adjust).
proc ::vchat::_read_pose {} {
    variable session
    variable at_edge
    if {$session eq ""} { return }
    set w [::vchat::log_for $session]
    if {$w eq "" || ![winfo exists $w]} { return }
    if {[llength [info commands ::${w}._pose]]} {
        _pose_from [lindex [$w._pose yview] 1]
    } else {
        _pose_from [lindex [$w yview] 1]
    }
}

# The latch writer, split from the reader so the GATE can feed it a
# fraction it already holds instead of querying the widget again.
proc ::vchat::_pose_from {frac} {
    variable session
    variable at_edge
    if {$session eq ""} { return }
    set at_edge($session) [expr {$frac >= 0.999}]
}

# The pose recorder, installed by renaming the widget's own command:
# `$w` becomes a thin interceptor and the real Tk widget command lives
# on as `$w._pose`. yview and see are the two subcommands that move the
# view, and EVERY scroller funnels through them — the scrollbar's
# -command, the Text class's key/mouse handling (whatever bindings.tcl
# and the keymaps install), a wheel event, a test's `moveto`. Bindings
# could never cover that surface: emacs/vi scroll through class
# bindings that call `$w yview` directly, and a programmatic
# `yview moveto` fires no event anyone can bind. The app's own `see
# end` scrolls arrive here too, by design: arrival, restore and
# first-paint all put the view AT the live edge, and recording that is
# exactly what the latch wants. Geometry noise — a pane docking, a
# re-wrap — moves the FRACTION but never calls yview, which is the
# whole point of the split: pose changes without a scroll cannot touch
# the latch.
#
# QUERIES NEVER WRITE THE LATCH. A bare `$w yview` is a READ, and the
# app reads it constantly — every prestash, every sync, even a debug
# puts. Latching on reads reintroduced the pose-lie through the gate's
# own front door: after a re-wrap dropped the fraction to ~0.96, the
# next insert's prestash queried the view and the gate obediently
# recorded "user left". Only MUTATING calls (moveto/scroll/place/see)
# record, and the fraction they record is read off the REAL command,
# after it has acted.
proc ::vchat::_arm_pose {w} {
    if {![winfo exists $w]} { return }
    # ABSOLUTE NAMES throughout: this runs inside ::vchat, and a
    # relative `proc $w` / `rename` would create the interceptor in the
    # ::vchat namespace while the widget command lives at global scope —
    # ".mid.chatwrap.chat.log" would then resolve to no command at all
    # (the startup crash this caused: "invalid command name"). Widget
    # paths begin with ".", so "::${w}" is their absolute spelling.
    # Idempotent: _claim_log can hand the same widget to a re-opened
    # session, and a second rename would layer gate on gate.
    if {[llength [info commands ::${w}._pose]]} { return }
    rename ::$w ::${w}._pose
    set body {
        set sub [lindex $args 0]
        if {$sub eq "yview" && [llength $args] > 1} {
            set r [uplevel 1 [linsert $args 0 @W@._pose]]
            catch { ::vchat::_pose_from [lindex [@W@._pose yview] 1] }
            return $r
        }
        if {$sub eq "see"} {
            set r [uplevel 1 [linsert $args 0 @W@._pose]]
            catch { ::vchat::_pose_from [lindex [@W@._pose yview] 1] }
            return $r
        }
        return [uplevel 1 [linsert $args 0 @W@._pose]]
    }
    proc ::$w {args} [string map [list @W@ $w] $body]
}

proc ::vchat::_show_follow_btn {} {
    set w [::vchat::log]
    if {[winfo exists $w] && [lindex [$w yview] 1] >= 0.999} { return }
    set b .mid.chatwrap.chat.follow
    if {![winfo exists $b]} {
        button $b -text [::vi18n::mc {▾ new output — follow}] -foreground $::vt::T(accent) \
            -background $::vt::T(bg_panel) -activeforeground $::vt::T(accent) \
            -activebackground $::vt::T(bg_panel) -relief solid -bd 1 \
            -command ::vchat::follow_now
        place $b -relx 0.5 -rely 1.0 -y -28 -anchor s
        catch { ::va11y::name $b [::vi18n::mc {jump to the latest output and keep following}] }
    }
    raise $b
}

proc ::vchat::_hide_follow_btn {} {
    catch { destroy .mid.chatwrap.chat.follow }
}

# The bar checkbox flipped follow off, or back on. Off means reading
# wins over arrival; on means the live edge pulls again (but only from
# the bottom, same as the stream-time rule).
proc ::vchat::_toggle_follow {} {
    variable follow
    if {$follow} {
        ::vstatus::set_msg [::vi18n::mc {follow: on}]
    } else {
        ::vstatus::set_msg [::vi18n::mc {follow: off — the ▾ button jumps back}]
    }
}

# The button itself, and the scrollbar drag release: both re-arm and
# jump.
proc ::vchat::follow_now {} {
    variable follow
    variable session
    variable at_edge
    set follow 1
    set w [::vchat::log]
    if {[winfo exists $w]} { $w see end }
    # The session on screen is now AT its live edge, and a switch away
    # and back must not forget that (the whole point of at_edge).
    if {$session ne ""} { set at_edge($session) 1 }
    _hide_follow_btn
}

# ── message regions: raw source kept, rendered in place ──────────────
#
# Each message owns a marked range in .mid.chatwrap.chat.log plus its original
# markdown, so it can be re-rendered as rendered⇄source without
# disturbing its neighbours. Marks (not indices) because they ride along
# with every edit elsewhere in the widget: msgstart has LEFT gravity so
# re-inserting at the start doesn't push the mark past the new text;
# msgend keeps the default right gravity so it tracks growth.

# Drop the retained sources for ONE transcript.
#
# Per widget, not global: with a transcript per session, an `array unset
# raw` would reap the messages of every session still loaded and leave
# their widgets full of text nothing could re-render or toggle to source.
# msg_seq is a process-wide counter so ids never collide; `raw_w` is the
# only thing that was missing to reap by owner.
proc ::vchat::forget_raw {{w ""}} {
    variable raw
    variable raw_kind
    variable raw_src
    variable raw_reason
    variable raw_tools
    variable raw_note
    variable raw_w
    variable live_rtag
    variable live_rhdr
    variable live_rtext
    variable live_blocks
    variable live_ptext
    if {$w eq ""} { set w [log] }
    foreach id [array names raw] {
        if {[info exists raw_w($id)] && $raw_w($id) ne $w} { continue }
        catch { $w mark unset msgstart_$id msgend_$id }
        foreach a {raw raw_kind raw_src raw_reason raw_tools raw_note raw_w raw_at raw_msgid} {
            catch { unset ::vchat::${a}($id) }
        }
    }
    catch { unset ::vchat::stamp_day($w) }
    # Collapsible state belongs to tags that just went away. This is a
    # SESSION change, not a repaint — `rebuild` deliberately does not
    # come through here, which is how an expanded block survives a theme
    # change but not switching sessions.
    variable collapse_open
    variable collapse_text
    catch { array unset collapse_open }
    catch { array unset collapse_text }
    foreach t [$w tag names] {
        if {[string match "chdr_*" $t] || [string match "cbody_*" $t]
            || [string match "reasonhdr*" $t] || [string match "live_r_*" $t]} {
            catch { $w tag delete $t }
        }
    }
    # The live reasoning tags went away with the wipe above (the live_r_*
    # glob catches the body live_r_* AND the header live_r_hdr_*). Forget
    # their names too, so a turn still streaming when you switch back
    # creates freshly-configured tags instead of writing into deleted ones
    # (which Tk would silently re-create unstyled and un-elided). on_done
    # resets these on the normal path; this is the session-change
    # catch-all. Pending reasoning goes with them: the region it was
    # queued for is gone, and painting it now would put it in whatever
    # transcript is on screen next.
    # …but ONLY when the loaded live region is the one that just went
    # away. The accumulators hold whichever session `_live_ctx` last
    # loaded, which need not be `$w`'s: evicting a transcript, or
    # clearing one for a fresh paint, while ANOTHER session is streaming
    # would otherwise blank that session's region mid-turn — the tags it
    # is writing into forgotten out from under it.
    variable live_ctx
    if {$live_ctx eq "" || [log_for $live_ctx] eq $w} {
        _reset_live_reasoning
        set live_rtag ""
        set live_rhdr ""
        set live_rtext ""
        set live_blocks {}
        set live_ptext ""
    }
}

# Render one message into the transcript and remember it. `kind` is
# user|assistant; `reason` is the message's display_reasoning (rendered
# as a collapsible block ABOVE the content, and survives rebuilds —
# reasoning is part of the message, not a side-channel). Returns the
# message id.
# ── when things happened ─────────────────────────────────────────────
#
# USER 2026-09-29: "we're also missing timestamps, in the desktop … it's
# not always obvious to me from the context when the messages were
# sent." Every block carries the moment it happened: a reloaded row the
# server's `at` (RFC 3339, UTC), a live one this clock. It paints as a
# dim HH:MM at the head of the block, and a date line wherever the day
# changes (digits only — nothing to translate). Hovering a stamp puts
# the full date and time in the status bar.
#
# `at` in: "" (unknown — painted without a stamp, never a made-up one),
# clock seconds, or RFC 3339 UTC. Out: clock seconds or "".
proc ::vchat::_stamp_secs {at} {
    if {$at eq "" || $at eq "null"} { return "" }
    if {[string is integer -strict $at]} { return $at }
    if {[catch { clock scan $at -format {%Y-%m-%dT%H:%M:%SZ} -timezone :UTC } secs]} {
        return ""
    }
    return $secs
}

# The stamp for block `id`, at the insertion point: a date line first if
# the day changed since the last stamped block in this widget.
proc ::vchat::_paint_stamp {w id} {
    variable raw_at
    variable stamp_day
    if {![info exists raw_at($id)] || $raw_at($id) eq ""} { return }
    _paint_stamp_secs $w $raw_at($id)
}

proc ::vchat::_paint_stamp_secs {w secs} {
    variable stamp_day
    set day [clock format $secs -format %Y-%m-%d]
    if {![info exists stamp_day($w)] || $stamp_day($w) ne $day} {
        set stamp_day($w) $day
        _run $w "── $day ──\n" stampday ;# i18n-exempt: a date, no words
    }
    # Seconds too (USER 2026-09-30: "don't show seconds") — a tool loop
    # does several things a minute, and HH:MM stamped them all alike.
    _run $w "[clock format $secs -format %H:%M:%S] " [list stamp stampt$secs] ;# i18n-exempt: a time, no words
}

# The stamp a LIVE provider call wears, painted once, at the head of its
# first visible output — its thinking header or its first prose.
#
# USER 2026-09-30: "the timestamps are only on tool calls". A call's
# blocks got their stamp from the reconcile at turn END, so for the
# whole of a long turn every thought and every line of prose had none
# and only the ⚙ rows did. The time comes from the call entry itself,
# which the reconcile reads too, so the finished block keeps the stamp
# the live one showed. More thinking after prose, or prose after
# thinking, is the same call and gets no second stamp; what opens a new
# call is a tool round or a note, exactly where the reconcile starts a
# new block.
proc ::vchat::_live_call_stamp {w} {
    variable live_blocks
    set fresh [expr {![llength $live_blocks] || [lindex $live_blocks end 0] ne "call"}]
    set i [_live_call_slot]
    if {$fresh} { _paint_stamp_secs $w [lindex $live_blocks $i 3] }
}

# Hovering a stamp: the full date and time, in the status bar.
proc ::vchat::_stamp_hover {w x y} { _stamp_show $w @$x,$y }

proc ::vchat::_stamp_show {w idx} {
    foreach t [$w tag names $idx] {
        if {[regexp {^stampt(\d+)$} $t -> secs]} {
            catch { ::vstatus::set_msg [clock format $secs -format {%Y-%m-%d %H:%M:%S}] }
            return
        }
    }
}

# A raw user row with a still-painted range. Marks survive rebuilds;
# a reconnect's delete collapses the range, so a stale source is not
# mistaken for a prompt which is actually on screen.
proc ::vchat::_painted_user_msgid {w mid} {
    variable raw_w
    variable raw_kind
    variable raw_msgid
    if {![array exists raw_msgid]} { return "" }
    set best ""
    foreach id [array names raw_msgid] {
        if {$raw_msgid($id) ne $mid} { continue }
        if {![info exists raw_w($id)] || $raw_w($id) ne $w} { continue }
        if {![info exists raw_kind($id)] || $raw_kind($id) ne "user"} { continue }
        if {[catch {$w compare msgstart_$id < msgend_$id} visible] || !$visible} { continue }
        if {$best eq "" || $id > $best} { set best $id }
    }
    return $best
}

# Prefer the persisted row identity to text. The text-only candidate is
# the optimistic copy from the current send; an already persisted older
# prompt with identical words must not be adopted by a later turn.
proc ::vchat::_turn_prompt_row {w prompt msgid} {
    variable raw
    variable raw_w
    variable raw_kind
    variable raw_msgid
    if {$msgid > 0} {
        set found [_painted_user_msgid $w $msgid]
        if {$found ne ""} { return $found }
    }
    set best ""
    foreach id [array names raw] {
        if {![info exists raw_w($id)] || $raw_w($id) ne $w} { continue }
        if {![info exists raw_kind($id)] || $raw_kind($id) ne "user"} { continue }
        if {[array exists raw_msgid] && [info exists raw_msgid($id)]} { continue }
        if {$raw($id) ne $prompt} { continue }
        if {[catch {$w compare msgstart_$id < msgend_$id} visible] || !$visible} { continue }
        if {$best eq "" || $id > $best} { set best $id }
    }
    return $best
}

proc ::vchat::emit_message {kind text {reason ""} {tools {}} {at now}} {
    variable raw
    variable raw_kind
    variable raw_src
    variable raw_reason
    variable raw_tools
    variable msg_seq
    variable source_mode
    if {![info exists msg_seq]} { set msg_seq 0 }
    incr msg_seq
    set id $msg_seq
    set raw($id) $text
    set raw_kind($id) $kind
    # `now` for a block painted live; a reloaded row passes the server's
    # time, or "" when it had none.
    variable raw_at
    set raw_at($id) [expr {$at eq "now" ? [clock seconds] : [_stamp_secs $at]}]
    variable raw_w
    set raw_w($id) [log]
    set raw_src($id) [expr {[info exists source_mode] ? $source_mode : 0}]
    if {$reason ne "" && $reason ne "null"} { set raw_reason($id) $reason }
    if {[llength $tools]} { set raw_tools($id) $tools }
    set w [::vchat::log]
    if {$w eq "" || ![winfo exists $w]} { return "" }
    # Stash the view BEFORE the paint, not after: guard notes and
    # reconcile repaints fire around tool calls, and the after-insert
    # read is the same lie that un-follows mid-stream (R6). The paint
    # below elides reasoning bodies — which MOVES the bottom — so the
    # pre-insert read is the only honest one.
    _at_bottom_prestash
    _writable $w {
        $w mark set msgstart_$id [$w index end-1c]
        $w mark gravity msgstart_$id left
        _paint_message $id
        # Left, like the start: what comes after the message is not in
        # it. With Tk's default (right) the end moved on with every
        # message after it, every message ran to the end of the
        # transcript, and "Copy message" copied whichever of them came
        # up first: an engine note (USER 2026-10-01, on the phone).
        $w mark set msgend_$id [$w index end-1c]
        $w mark gravity msgend_$id left
    }
    _maybe_follow
    return $id
}

# Exactly one blank line before a logical block — never none, never two.
#
# R1 of the transcript spec: "logical blocks are separated by a blank
# line". This is a property of the MODEL now, decided in ONE place — it
# used to be decided independently at every painter: the live send added
# a break after the user message that the reload path did not, assistant
# bodies ended in a blank nothing else got, and notes and tool rounds
# welded onto whatever came before. The spec pinned those as gaps; this
# retires them. Bodies end with a single newline; the separator owns
# every blank between blocks.
proc ::vchat::_block_sep {w} {
    if {[$w compare end-1c == 1.0]} { return }
    # Tk keeps a final newline at end-1c; look at the tail before it and
    # count the run of newlines ending the content.
    set tail [$w get end-12c end-1c]
    set nl 0
    foreach c [split $tail ""] {
        if {$c eq "\n"} { incr nl } else { set nl 0 }
    }
    if {$nl >= 2} { return }
    _writable $w {
        if {$nl == 0} {
            _run $w "\n\n" {}
        } else {
            _run $w "\n" {}
        }
    }
}

# Draw message `id` at the insertion point, honouring its source flag.
proc ::vchat::_paint_message {id} {
    variable raw
    variable raw_kind
    variable raw_src
    variable raw_reason
    variable raw_tools
    variable raw_note
    set w [::vchat::log]
    # Every message block is separated from what precedes it — the
    # model owns the blank line, not each caller (R1).
    _block_sep $w
    set text $raw($id)
    if {$raw_kind($id) eq "tool"} {
        # A tool round is rendered as ⚙ name + invocation + an expandable
        # result.
        _paint_stamp $w $id
        if {[info exists raw_tools($id)]} {
            set ti 0
            foreach tr $raw_tools($id) {
                lassign $tr tname targs tresult
                _render_tool_round "tool$id.[incr ti]" $tname $targs $tresult
            }
        }
        return
    }
    if {$raw_kind($id) eq "note"} {
        variable raw_note
        if {[info exists raw_note($id)]} { _paint_note $id $raw_note($id) }
        return
    }
    if {$raw_kind($id) eq "user"} {
        _paint_stamp $w $id
        _run $w "you ▸ " userhdr
        if {$raw_src($id)} {
            _run $w "$text\n" mdsource
        } else {
            _render_user_with_media $w $text
        }
        return
    }
    _paint_stamp $w $id
    # Reasoning block rides with the message (survives source toggles).
    if {[info exists raw_reason($id)] && !$raw_src($id)} {
        _render_collapsible_reasoning $id $raw_reason($id)
    }
    # The tool rounds that produced this reply (assistant only), shown
    # before the content so the answer reads as the result of the calls.
    if {$raw_kind($id) eq "assistant" && [info exists raw_tools($id)] && !$raw_src($id)} {
        set ti 0
        foreach tr $raw_tools($id) {
            lassign $tr tname targs tresult
            _render_tool_round "tool$id.[incr ti]" $tname $targs $tresult
        }
    }
    if {$raw_src($id)} {
        # Source view: the markdown exactly as the model wrote it. The
        # blank line after belongs to the NEXT block's separator.
        _run $w "$text\n" mdsource
    } else {
        ::vchat::_render_with_media $w $text
    }
}

# Re-render the whole transcript from the retained sources.
#
# Deliberately a full rebuild rather than an in-place splice: the
# renderer appends at `end`, so painting one message at a mark would
# mean transplanting tagged runs (and images) between widgets. A chat
# log is small and this runs on a keystroke, so simple wins.
proc ::vchat::rebuild {} {
    variable raw
    variable raw_w
    set w [::vchat::log]
    set yview [lindex [$w yview] 0]
    _writable $w {
        # Named Tk images are not garbage-collected — drop the ones this
        # transcript owns or every rebuild leaks (a pre-existing leak in
        # on_done's delete-and-re-render, multiplied by toggling).
        # The artifact cards' action tags go with them.
        catch { ::vmedia::forget $w }
        $w delete 1.0 end
        catch { ::vmd::forget_links $w }
        catch { unset ::vchat::stamp_day($w) }
        foreach id [lsort -integer [array names raw]] {
            # ONLY THIS WIDGET'S messages. `raw` holds every loaded
            # session's sources (each session keeps its transcript
            # widget), and repainting all of them into the current one
            # is precisely how one session's transcript landed in
            # another's: /redraw on a child window replayed the parent
            # into it. Its deletion twin `forget_raw` always filtered by
            # owner (`raw_w`); this loop never did.
            if {![info exists raw_w($id)] || $raw_w($id) ne $w} { continue }
            $w mark set msgstart_$id [$w index end-1c]
            $w mark gravity msgstart_$id left
            _paint_message $id
            $w mark set msgend_$id [$w index end-1c]
            $w mark gravity msgend_$id left
        }
    }
    $w yview moveto $yview
    _forget_anchors [event_session]
}

# Global rendered⇄source toggle (Ctrl-R / palette).
proc ::vchat::toggle_source {} {
    variable source_mode
    variable raw_src
    variable raw
    set source_mode [expr {[info exists source_mode] ? !$source_mode : 1}]
    foreach id [array names raw] { set raw_src($id) $source_mode }
    rebuild
    ::vstatus::set_msg [expr {$source_mode \
        ? [::vi18n::mc {markdown source (%s to render)} [::vkeymap::key_for ::vchat::toggle_source]] \
        : [::vi18n::mc {rendered markdown (%s for source)} [::vkeymap::key_for ::vchat::toggle_source]]}]
}

# The message at a point of the transcript: the one that starts last
# at or before it, of those whose span holds it, or "". Where one ends
# the next begins, and a point there is the later one's.
proc ::vchat::message_at {x y} {
    variable raw
    variable raw_w
    set w [::vchat::log]
    if {[catch { $w index @$x,$y } idx]} { return "" }
    set best ""
    foreach id [array names raw] {
        if {[info exists raw_w($id)] && $raw_w($id) ne $w} { continue }
        if {[catch {
            set in [expr {[$w compare $idx >= msgstart_$id]
                && [$w compare $idx <= msgend_$id]}]
        }] || !$in} { continue }
        if {$best eq "" || [$w compare msgstart_$id > msgstart_$best]} {
            set best $id
        }
    }
    return $best
}

# Copy one message's markdown source to the clipboard (right-click).
proc ::vchat::copy_message_at {x y} {
    variable raw
    set id [message_at $x $y]
    if {$id eq ""} { return }
    clipboard clear
    clipboard append $raw($id)
    ::vstatus::set_msg [format [::vi18n::mc {copied message (%s chars)}] [string length $raw($id)]]
}

# Toggle just the message under the pointer (right-click menu).
proc ::vchat::toggle_source_at {x y} {
    variable raw
    variable raw_src
    set id [message_at $x $y]
    if {$id eq ""} { return }
    set raw_src($id) [expr {!$raw_src($id)}]
    rebuild
    ::vstatus::set_msg [expr {$raw_src($id) \
        ? [::vi18n::mc {message %s: source} $id] : [::vi18n::mc {message %s: rendered} $id]}]
}

# The transcript's empty state. WHAT IT SAYS is a function of attach
# state, not of whichever teardown emptied it last:
#
#   unattached          → the Instances tab is where switching lives
#   attached, no session → pick or start a session
#
# The first version painted one fixed string in detach_clear, which runs
# during EVERY attach's teardown — so an ATTACHED client with no session
# selected kept telling the user to "press Attach in the Instances tab".
proc ::vchat::empty_state_text {} {
    if {[llength [info commands ::vapi::unattached]] && [::vapi::unattached]} {
        return [::vi18n::mc {Chat — not attached; press Attach in the Instances tab}]
    }
    # The key from the registry, not the sentence: it said "(M-n)" in
    # every mode, normie's Ctrl-N included, and on a phone, which has
    # no key for it at all (lib/cmdtree.tcl's ⋮ has the command).
    set t [::vi18n::mc {No session selected — pick one in the sidebar or start a new one}]
    set k ""
    catch { set k [::vkeymap::key_for ::vsidebar::new_session] }
    if {$k ne ""} { append t " ($k)" }
    return $t
}

proc ::vchat::paint_empty_state {} {
    variable session
    if {$session ne ""} { return }
    _emit_block empty "[empty_state_text]\n" empty_state
}

# The empty-state hint is written into the TRANSCRIPT text widget, which
# the language swap's re-text pass cannot reach (it walks widget -text
# options). Seen on the real desktop: after a swap to German the whole
# chrome followed and this line stayed English. So a language change
# repaints it — only while it is what the transcript shows.
proc ::vchat::_relocalize_empty {} {
    variable session
    if {$session ne ""} { return }
    catch { clear }
    catch { paint_empty_state }
}
if {[llength [info commands ::vi18n::on_locale_change]]} {
    ::vi18n::on_locale_change ::vchat::_relocalize_empty
}

# The per-turn anchors are absolute `line.char` indices, so they describe
# ONE arrangement of text. Every wholesale repaint — clear, rebuild,
# growing the paint window, dropping a transcript — invalidates them, and
# an anchor that survives one names a line in the middle of somebody
# else's message: `on_done` then runs `$w delete $anchor end` and eats
# the transcript from there down.
#
# Forgetting them is the SAFE state and always was: "no anchor" already
# means APPEND at the tail (on_done), and the next turn-started or
# turn-snapshot re-sets them against the transcript as it now stands.
proc ::vchat::_forget_anchors {sid} {
    variable turn_anchor
    variable turn_reload_anchor
    variable turn_prompt_ids
    catch { dict unset turn_anchor $sid }
    catch { dict unset turn_reload_anchor $sid }
    # Keep message IDs: a rebuild/reconnect may still replay this turn's
    # store rows into a freshly cleared widget, and those IDs are the
    # surviving identity of its optimistic prompt. Positional anchors
    # cannot make that crossing; the ID map can.
}

proc ::vchat::clear {} {
    # Photos first: `delete 1.0 end` unmaps an embedded image without
    # freeing it, and named Tk images are never garbage-collected, so
    # clearing a transcript full of attachments used to leak all of them.
    catch { ::vmedia::forget [::vchat::log] }
    _writable [::vchat::log] {
        [::vchat::log] delete 1.0 end
    }
    # Link targets and per-message raw source belong to the text that
    # just went away.
    catch { ::vmd::forget_links [::vchat::log] }
    catch { ::vchat::forget_raw }
    _forget_anchors [event_session]
}

# Write a session's transcript to a file and return its path. One proc
# for /export (the session on screen) and the Sessions window and the
# sidebar's menu (the session you picked) — the menu's Export… used to
# run /export, which exported the CURRENT session whatever row was
# right-clicked.
proc ::vchat::export_session {sid fmt} {
    set data [::vapi::get_json "/api/sessions/$sid/history"]
    set path "/tmp/veles_export_$sid.[expr {$fmt eq "json" ? "json" : "md"}]"
    set f [open $path w]
    if {$fmt eq "json"} {
        puts $f $data
    } else {
        puts $f "# Session $sid\n"
        foreach m [dict get $data messages] {
            puts $f "\n## [dict get $m role]\n\n[dict get $m content]\n"
        }
    }
    close $f
    return $path
}

# /resume <id or prefix>: the session itself, else the most recently
# active one whose id starts with it — over every session, through the
# listing's search (the desktop told people to /resume and had no
# /resume).
proc ::vchat::resume {ref} {
    if {[catch {::vapi::get_json "/api/sessions?[::http::formatQuery q $ref limit 200]"} d]} {
        emit_message system [::vi18n::mc {sessions: unreachable}]
        return
    }
    set pick ""
    foreach s [dict get $d sessions] {
        set id [dict get $s id]
        if {$id eq $ref} { set pick $id ; break }
        if {$pick eq "" && [string first $ref $id] == 0} { set pick $id }
    }
    if {$pick eq ""} {
        emit_message system [format [::vi18n::mc {no session matching %s}] $ref]
        return
    }
    ::vchat::open_session $pick
}

# The reply to a fresh view's "is this turn still live?" (see the
# session-snapshot arm). Engine-confirmed live: re-handshake so the fresh
# snapshot re-adopts the turn (this view never invents result text).
# _events_connect closes the old stream itself, so this is the SAME
# session-lagged ladder: a direct reconnect, not a disconnect/connect
# pair — the disconnect would drop this session's stream entirely.
proc ::vchat::_live_check {sid ok cx} {
    if {!$ok} { return }
    if {[catch {dict get $cx busy} busy] || !$busy} { return }
    _events_reconnect $sid
}

proc ::vchat::clear_if_current {sid} {
    variable session
    variable turn_anchor
    # A session that goes away is DELETED: cancelling its turn is part
    # of the deletion, via the explicit endpoint (the engine no longer
    # cancels on disconnect). A failure to cancel a session being deleted
    # is not worth blocking on, but it is captured, not swallowed.
    catch { ::vapi::post_json "/api/sessions/$sid/cancel" [dict create] } _cancel_err
    # Close ITS event stream — sockets only; the turn was just
    # cancelled through the endpoint above. Unconditional now: every
    # loaded session has one, not just the one on screen.
    _events_disconnect $sid
    dict unset turn_anchor $sid
    _mark_running $sid 0
    # Its cached history goes with it — a re-created session with the
    # same id would otherwise paint the deleted one's transcript.
    variable hist_cache
    if {[info exists hist_cache]} { catch { dict unset hist_cache $sid } }
    if {$sid eq $session} {
        set session ""
        clear
        catch { paint_empty_state }
        retitle_window
    }
    # And its transcript. A kept widget for a session that no longer
    # exists is the sharper version of the stale-cache bug above: the
    # next session to take that id would be SHOWN the deleted one's
    # text, not merely painted it from a cache. _drop_log wipes the base
    # widget rather than destroying it, so the empty state survives.
    _drop_log $sid
}

# ── attach teardown ─────────────────────────────────────────────────
#
# Called by ::vapp::teardown_for_attach. A session id from gateway A
# does not exist on gateway B: drop the session, stop any stream it
# owned (streams also get closed wholesale by the checklist step 1)
# and blank the transcript so nothing from the old gateway is on
# screen labelled as the new one's.
#
# NOTE this closes SOCKETS ONLY. The turns themselves keep running on
# the old gateway by design — "detach" means "stop watching", not
# "stop working" (the engine no longer cancels on disconnect, and a
# turn that was minutes from done used to die here).
proc ::vchat::detach_clear {} {
    variable session
    variable busy
    variable turn_anchor
    # Close the event stream and stop its reconnect backoff — a pending
    # retry would otherwise fire into the NEW gateway with the OLD cursor
    # (irc.tcl's disconnect cancels its timer for exactly this reason).
    catch { _events_disconnect_all }
    set session ""
    set busy 0
    set turn_anchor [dict create]
    # Rendered histories belong to the gateway they came from — an
    # attach teardown is exactly when a cached transcript would lie.
    variable hist_cache
    set hist_cache {}
    # EVERY transcript, not just the visible one: a session id from
    # gateway A does not exist on gateway B, so a kept widget is text
    # from one machine sitting under a label that now names another.
    variable logs
    foreach sid [dict keys $logs] { catch { _drop_log $sid } }
    variable logs_lru
    set logs_lru {}
    catch { clear }
    catch { retitle_window }
    # The unattached empty-state, named like IRC's. Which text it is at
    # ATTACH time is decided by paint_empty_state (teardown runs before
    # the flip, so this pass shows the unattached wording; the attach
    # itself repaints after use_instance).
    catch { paint_empty_state }
    catch { _sync_buttons }
}

# MEDIA: backport (2026-08-10) — hermes convention: a MEDIA:/path line
# attaches a file to a reply. Desktop renders images inline (Tk-native
# PNG/GIF — core Tk 8.6 has no JPEG/webp decoder), everything else is a
# path note. Mirrors veles_core::media::extract_media in Rust.
proc ::vchat::_render_with_media {w text} {
    set seg ""
    foreach line [split $text "\n"] {
        set t [string trim $line]
        if {[string match "MEDIA:*" $t]} {
            if {$seg ne ""} {
                ::vmd::render $w $seg
                set seg ""
            }
            set path [string trim [string range $t 6 end]]
            if {$path ne ""} { _render_media_item $w $path }
        } else {
            append seg "$line\n"
        }
    }
    if {$seg ne ""} {
        ::vmd::render $w $seg
    }
}

# Every attached file goes through ::vmedia, which draws the picture
# when it can (PNG/GIF natively, JPEG/TIFF/BMP/ICO/... via tkimg, scaled
# to fit) and an artifact card - name, kind, size, Open / Save as... /
# Preview / Copy path - when it cannot. USER: "attachments/artifacts in
# general. I didn't mean just model-to-user MEDIA."
# Render a FRAGMENT of a larger document: the same output _render_with_media
# gives, minus the trailing separation it adds for a document's end.
#
# Both halves of the render pipeline normalise their input to end with a
# newline and then emit one newline per element of the split, the phantom
# element after that final newline included. The consequence is an exact
# identity:
#
#     _render_with_media(T)  ==  <the lines of T>  +  "\n\n"
#
# for every T — check it on "a" ("a\n\n"), "a\n" ("a\n\n\n") and "\n"
# ("\n\n\n"). So rendering a document in k pieces produces 2(k-1) newlines
# that rendering it in one call never would, and the live region's settled
# blocks would drift from the reload exactly that far apart (R3).
#
# Taking those two characters back per fragment makes the concatenation
# exact for ANY split at a line boundary, with no arithmetic on the input
# and no special case for a chunk that happens to start or end blank.
# `end-1c` is the widget's own implicit final newline; the two characters
# before it are the pair the identity names.
proc ::vchat::_render_fragment {w text} {
    _render_with_media $w $text
    if {[$w compare "end-3c" >= 1.0]} {
        $w delete "end-3c" "end-1c"
    }
}

proc ::vchat::_render_media_item {w path} {
    ::vmedia::insert $w $path
}

# A user message carries MEDIA: markers exactly as a reply does — the
# attachment chip writes them into the body before it is sent. The
# assistant path renders them through _render_with_media; the user path
# used to insert the text verbatim, so an attached image showed as a
# literal "MEDIA:/home/…/shot.png" line instead of the picture.
proc ::vchat::_render_user_with_media {w text} {
    set seg ""
    foreach line [split $text "\n"] {
        set t [string trim $line]
        if {[string match "MEDIA:*" $t]} {
            if {$seg ne ""} { _run $w $seg user; set seg "" }
            set path [string trim [string range $t 6 end]]
            if {$path ne ""} { _render_media_item $w $path }
        } else {
            append seg "$line\n"
        }
    }
    if {$seg ne ""} { _run $w $seg user }
}

# Paint one parsed history dict into the transcript. Shared by the
# first load, the cache replay and the background verify — one painter
# means all three render identically.
#
# WINDOWED: only the newest rows render on open (PAINT_LIMIT), because
# a 3,000-row transcript is one Tk text widget with tens of thousands
# of tagged runs and painting all of it made opens take seconds at
# 100% CPU (the second repair session's own transcript was the
# exhibit). The full set stays in hist_cache; a header line offers the
# rest, and clicking it repaints with a bigger window — pure local
# work, no refetch.
namespace eval ::vchat {
    ;# Rows per window. 400 ≈ a long afternoon; renders fast, scrolls
    ;# fine.
    variable paint_limit 400
    ;# sid -> unpainted row count under the header (0 = all shown).
    variable paint_hidden
}

proc ::vchat::_paint_history {hist} {
    variable paint_limit
    variable paint_hidden
    variable session
    set msgs [dict get $hist messages]
    set total [llength $msgs]
    set hidden [expr {$total > $paint_limit ? $total - $paint_limit : 0}]
    set paint_hidden($session) $hidden
    if {$hidden > 0} {
        set w [::vchat::log]
        _writable $w {
            _run $w "⋯ $hidden older messages — click to load the previous $::vchat::paint_limit\n" paint_more_line
        }
        ::vtext::click $w paint_more_line [list ::vchat::_paint_more $session]
        $w tag bind paint_more_line <Enter> {%W configure -cursor hand2}
        $w tag bind paint_more_line <Leave> {%W configure -cursor xterm}
        catch {
            $w tag configure paint_more_line \
                -foreground $::vt::T(accent) -font {TkDefaultFont 10}
        }
    }
    _paint_history_rows $msgs $hidden $total
}

# Grow the window by one PAINT_LIMIT and repaint from the CACHE. A
# prepend-in-place would fight every mark and tag the renderer owns;
# a full repaint of the visible region is bounded, local and correct.
proc ::vchat::_paint_more {sid} {
    variable paint_hidden
    variable paint_limit
    variable session
    variable hist_cache
    if {$session ne $sid} { return }
    if {![info exists paint_hidden($sid)] || $paint_hidden($sid) == 0} { return }
    if {![info exists hist_cache] || ![dict exists $hist_cache $sid]} { return }
    set hist [dict get $hist_cache $sid]
    set msgs [dict get $hist messages]
    set total [llength $msgs]
    set new_hidden [expr {$paint_hidden($sid) - $paint_limit}]
    if {$new_hidden < 0} { set new_hidden 0 }
    set w [::vchat::log]
    # Keep the view anchored on what the user is reading (roughly a
    # third of the window down — the top of the old window) rather than
    # jumping to the very bottom.
    set anchor ""
    catch {
        set y [expr {[winfo height $w] / 3}]
        set anchor [$w index @$y,$y]
    }
    clear
    if {$new_hidden > 0} {
        set paint_hidden($sid) $new_hidden
        _writable $w {
            _run $w "⋯ $new_hidden older messages — click to load the previous $::vchat::paint_limit\n" paint_more_line
        }
        ::vtext::click $w paint_more_line [list ::vchat::_paint_more $sid]
        $w tag bind paint_more_line <Enter> {%W configure -cursor hand2}
        $w tag bind paint_more_line <Leave> {%W configure -cursor xterm}
    } else {
        set paint_hidden($sid) 0
    }
    _paint_history_rows $msgs $new_hidden $total
    if {$anchor ne ""} { catch { $w see $anchor } }
    ::vstatus::set_msg [::vi18n::mc {loaded older messages}]
    return
}

proc ::vchat::_paint_history_rows {msgs from to} {
    # PASS 1 (cheap): assistant rows carry the tool_calls JSON — the
    # ONLY persistent copy of the arguments — so index them by call id
    # BEFORE the tool rows ask for them. Tool results are separate rows
    # keyed by tool_call_id, exactly as the wire ordered them.
    set callmap {}
    foreach m [lrange $msgs $from [expr {$to - 1}]] {
        set role ""
        catch { set role [dict get $m role] }
        if {$role ne "assistant"} { continue }
        set tc {}
        catch { set tc [dict get $m tool_calls] }
        foreach c $tc {
            set cid "" ; set cname "" ; set cargs ""
            catch { set cid [dict get $c id] }
            catch { set cname [dict get $c function name] }
            catch { set cargs [dict get $c function arguments] }
            if {$cid ne ""} { dict set callmap $cid [list $cname $cargs] }
        }
    }
    foreach m [lrange $msgs $from [expr {$to - 1}]] {
        _paint_one_row $m $callmap
    }
}

# One history row → transcript. Split out of _paint_history so the
# windowed paths (open, load-more) share the exact same row renderer.
proc ::vchat::_paint_one_row {m {callmap {}}} {
    set role ""
    catch { set role [dict get $m role] }
    # ROLE FIRST, and every other key defensively.
    #
    # `set content [dict get $m content]` used to run here, before the
    # role was examined — and a `note` row has no `content`. The
    # missing key threw, the enclosing catch turned the whole loop into
    # "failed to load history", and EVERY message after the first note
    # was silently lost. The same shape as the vanishing answers: one
    # strict lookup on a row that legitimately lacks the key.
    if {$role eq "note"} {
        # An engine note persisted from an earlier turn. Render it
        # through the SAME path as a live one, so a reopened session
        # shows the mechanisms that acted rather than a conversation
        # that looks like you had it.
        ::vchat::on_note $m 0
        return
    }
    set content ""
    catch { set content [dict get $m content] }
    # When the row was written — the server's RFC 3339, or "" (no stamp).
    set at ""
    catch { set at [dict get $m at] }
    # RM3-S4/A4: a user-role row the ENGINE wrote, not you. Labelled
    # block through the SAME path as an engine note (glyph, summary,
    # expandable detail, mute knob) — never a normal speech bubble,
    # which is how the model-blaming "I never said that" transcripts
    # got made. The row's toggle arrives pre-filled from the server.
    set inj ""
    catch { set inj [dict get $m injection] }
    # Session mail (veles_core::mail): another session's words. The
    # engine's provenance row before them is for the MODEL; the mail row
    # itself says who sent it, so the pair paints once — the sender in
    # the head, the first line in view, the whole text on a click.
    if {$role eq "user" && $inj eq "origin:mail"} { return }
    if {$role eq "user" && [string match "mail:*" $inj]} {
        ::vchat::_mail_note $inj $content $at
        return
    }
    if {$role eq "user" && $inj ne ""} {
        set d [dict create kind injection summary [_injection_caption $inj]]
        if {$content ne ""} { dict set d detail $content }
        catch { dict set d toggle [dict get $m toggle] }
        if {$at ne ""} { dict set d at $at }
        ::vchat::on_note $d 0
        return
    }
    if {$role eq "tool"} {
        # A tool result — show ⚙ name + invocation + ↳ result, not a
        # bare noun. The invocation comes from the call map; live and
        # historical rounds now read alike.
        # USER: "terminal doesn't show the command ran … only in the
        # live reasoning trace, and then it disappears".
        set tname ""
        catch { set tname [dict get $m tool_name] }
        set tcid ""
        catch { set tcid [dict get $m tool_call_id] }
        set targs ""
        if {$tcid ne "" && [dict exists $callmap $tcid]} {
            lassign [dict get $callmap $tcid] _cn targs
            if {$tname eq ""} { set tname $_cn }
        }
        emit_message tool $content {} [list [list $tname $targs $content]] $at
        return
    }
    set dr ""
    catch { set dr [dict get $m display_reasoning] }
    # Every message keeps its markdown source, its reasoning, and a
    # marked region — re-renderable in place (view source).
    set painted [emit_message [expr {$role eq "user" ? "user" : "assistant"}] $content $dr {} $at]
    if {$role eq "user" && $inj eq "" && $painted ne "" && [dict exists $m msg_id]} {
        variable raw_msgid
        set raw_msgid($painted) [dict get $m msg_id]
    }
    return $painted
}

# Drop one session's cached transcript — every operation that MUTATES
# history server-side (clear, compress, undo, retry, branch) must call
# this, or the next open paints the pre-change view. The event stream
# keeps the LIVE view fresh now; the cache is only a repaint speed-up for
# the next open, so dropping it is what makes that open fetch the truth.
proc ::vchat::_drop_cache {sid} {
    variable hist_cache
    if {[info exists hist_cache]} { catch { dict unset hist_cache $sid } }
}

# The store changed UNDER a transcript that is already on screen:
# /clear, /undo, /branch, a finished compaction.
#
# USER 2026-09-17: "I did notice the compaction 'reload' inconsistency".
#
# A kept session never repaints — that is the POINT of keeping it, and
# it is what makes a switch cost one `pack`. Both call sites did
# `_drop_cache` + `open_session`, and `_drop_cache` forgets the parsed
# HISTORY CACHE only: the widget is untouched, so open_session takes the
# kept branch and returns without painting. The window went on showing
# the pre-change transcript while the status line said "reloading".
#
# Dropping the WIDGET is what makes the next open take the paint path.
# It is done for a hidden session too — the stale text is believed the
# moment you switch back — and the open below is only for the one on
# screen, which has to be right NOW rather than at the next switch.
#
# NEVER under a live turn. `_drop_log` destroys the widget the turn is
# painting into and closes the stream feeding it. Both real callers
# clear the running mark before they get here (history-changed means
# the store moved OUTSIDE a turn), so this guard is the contract for
# any future caller rather than a condition they hit; sessiontest
# asserts it by calling this proc directly on a running session.
# `force` is the USER saying "this window is wrong, refetch it".
#
# The is_running guard exists so an AUTOMATIC reload cannot yank the
# transcript out from under a live turn. A human asking for a refresh
# has already decided otherwise, and the case they ask in is exactly
# the one the guard blocks: a session running in ANOTHER PROCESS — a
# delegate or /btw child — whose events this window cannot receive at
# all, because the session bus is per-process.
proc ::vchat::reload_session {sid {force 0}} {
    variable session
    _drop_cache $sid
    if {!$force && [is_running $sid]} { return }
    _drop_log $sid
    if {$session eq $sid} { open_session $sid }
}

# Refetch the transcript on screen from the store, whatever it is
# doing. USER 2026-09-18: "we probably need a way to force-refresh a
# transcript/session window."
#
# Everything this needs already exists: `_drop_log` drops the widget,
# the anchors, the live region, the stream and the cursor, and
# `open_session`'s not-kept branch re-fetches the history, re-seeds the
# cursor and reopens the stream. The only new thing is a human being
# allowed to ask.
proc ::vchat::refetch {} {
    variable session
    if {$session eq ""} { return }
    reload_session $session 1
    catch { ::vstatus::set_msg [::vi18n::mc {transcript refetched}] }
}

# The pre-command name (C-S-V binding and older muscle memory).
proc ::vchat::force_refresh {} { ::vchat::refetch }

# Redraw approvals that were still pending while this window was closed.
#
# The engine half of this fix made an ask OUTLIVE the request that raised
# it: `DesktopAsker` parks it in the `crate::ask` registry and rides the
# session bus, so a closed window no longer auto-denies the next approval.
# This is the consumer half — `GET /api/asks` existed precisely to redraw
# open approvals after a reload and had ZERO callers. A turn that asked
# something while nobody was watching now surfaces the question when the
# session is reopened, instead of being silently lost.
#
# `shown` is the dialog on screen when the request went out
# (`_ask_on_screen`). A reply is a snapshot from then; if the dialog
# changed while it was in flight, a live `ask`/`ask-resolved` is fresher
# than it, and the reply is dropped — otherwise its "nothing parked"
# would close a question that arrived a moment after it was asked.
proc ::vchat::_redraw_asks {sid shown ok val} {
    variable session
    if {!$ok} { return }
    # Only the session still on screen: the user may have navigated away
    # before the fetch landed (the epoch check in get_json_async covers a
    # re-attach; this covers a plain session switch within one attach).
    if {$session ne $sid} { return }
    set cur [_ask_on_screen]
    if {$cur ne $shown} { return }
    set asks {}
    catch { set asks [dict get $val asks] }
    foreach a $asks {
        set asid ""
        catch { set asid [dict get $a session_id] }
        if {$asid ne $sid} { continue }
        # The agent asks serially, so the first open ask for this session
        # is THE pending one; ::vask::show keeps one dialog at a time by
        # design. Showing it is enough — answering goes over REST as usual.
        catch { ::vask::show $a }
        return
    }
    # No open ask for this session is parked any more. If a dialog FOR THIS
    # SESSION is up, its `ask-resolved` was dropped (a stream reconnect's
    # replay carries store rows, not tap-only events) — close it, or it
    # lingers forever. Scoped by current_sid: another session's dialog is
    # none of this re-fetch's business.
    set cur_sid ""
    catch { set cur_sid $::vask::current_sid }
    if {$cur ne "" && $cur_sid eq $sid} {
        catch { ::vask::resolved [dict create id $cur] }
    }
}

# The ask whose dialog is up, or "".
proc ::vchat::_ask_on_screen {} {
    set cur ""
    catch { set cur $::vask::current }
    return $cur
}

# Name the window after the session it is showing.
#
# USER: "need window title set to actual session name not session id".
# Two separate bugs were behind that one sentence:
#
#   * the title was the raw `20260915_194936_559209`, not the name the
#     sidebar shows and the engine writes for you; and
#   * it was set with `wm title`, which is the WM's own bar — and this
#     app hides that by default (::vtitle::attach draws its own strip,
#     main.tcl:214). The one title anybody could see never moved.
#
# Both get set now. The label comes from the sidebar's last listing, so
# there is no extra round trip and no second source of truth; a session
# whose title has not been written yet falls back to its id, which is
# what the sidebar shows for it too.
proc ::vchat::retitle_window {} {
    variable session
    if {$session eq ""} {
        catch { wm title . [::vi18n::mc {veles}] }
        catch { ::vtitle::set_title . "veles" }
        return
    }
    set label $session
    catch {
        set l [::vsidebar::label_for $session]
        if {$l ne ""} { set label $l }
    }
    catch { wm title . [format [::vi18n::mc {veles — %s}] $label] }
    catch { ::vtitle::set_title . "veles — $label" }
}

# Sessions this window opened from ANOTHER node's replica: sid → the
# owning node's name. Their transcript is read once, on open (and on
# Refresh); no event stream follows them and the composer is off — a
# turn would be refused anyway ("lives on node …").
namespace eval ::vchat { variable remote {} }

proc ::vchat::is_remote {sid} {
    variable remote
    return [dict exists $remote $sid]
}

# Learn from a history reply whether `sid` is another node's.
proc ::vchat::_note_remote {sid hist} {
    variable remote
    set ro 0
    catch { set ro [string is true -strict [dict get $hist read_only]] }
    if {!$ro} {
        dict unset remote $sid
        return
    }
    set node "?"
    catch { set node [dict get $hist node_name] }
    dict set remote $sid [list $node [clock format [clock seconds] -format %H:%M]]
}

# The composer and the read-only banner follow the session on screen.
proc ::vchat::_sync_readonly {} {
    variable session
    variable remote
    set f .mid.chatwrap.chat.ro
    if {![winfo exists .mid.chatwrap.chat.input]} { return }
    if {[dict exists $remote $session]} {
        lassign [dict get $remote $session] node at
        if {![winfo exists $f]} {
            ttk::frame $f
            ttk::label $f.l -foreground $::vt::T(fg_dim)
            ttk::button $f.refresh -text [::vi18n::mc {Refresh}] \
                -command ::vchat::reload_remote
            pack $f.l -side left -padx {6 4}
            pack $f.refresh -side right -padx 4
        }
        $f.l configure -text [format \
            [::vi18n::mc {read-only: this session lives on %1$s (as read at %2$s)}] $node $at]
        catch { pack $f -side bottom -fill x -padx 6 -pady {0 2} \
                    -before .mid.chatwrap.chat.bar }
        .mid.chatwrap.chat.input configure -state disabled
        catch { .mid.chatwrap.chat.bar.send configure -state disabled }
    } else {
        if {[winfo exists $f]} { pack forget $f }
        .mid.chatwrap.chat.input configure -state normal
        catch { .mid.chatwrap.chat.bar.send configure -state normal }
    }
}

# Read a remote session's transcript again (its node keeps writing it;
# the replica here follows within a sync tick).
proc ::vchat::reload_remote {} {
    variable session
    variable hist_cache
    set sid $session
    if {![is_remote $sid]} { return }
    catch { dict unset hist_cache $sid }
    if {[catch { set hist [::vapi::get_json "/api/sessions/$sid/history"] } e]} {
        log_insert "[format [::vi18n::mc {failed to load history: %s}] $e]\n" system
        return
    }
    clear
    if {[catch { _paint_history $hist } pe]} {
        log_insert "[format [::vi18n::mc {failed to load history: %s}] $pe]\n" system
    }
    if {![info exists hist_cache]} { set hist_cache {} }
    dict set hist_cache $sid $hist
    _note_remote $sid $hist
    _sync_readonly
    set w [::vchat::log]
    if {[winfo exists $w]} { $w see end }
}

proc ::vchat::open_session {sid} {
    variable session
    variable busy
    variable turn_anchor
    variable logs
    # Switching away from a running turn used to be REFUSED ("busy —
    # stop the current turn first"). The turn runs on the server; there
    # was never a reason it had to hold the whole window hostage.
    #
    # SWITCHING NO LONGER UNLOADS ANYTHING. A session that has been
    # opened before keeps its transcript widget; the switch hides one
    # and shows the other, and every rendered block, expanded reasoning
    # header, decoded image and scroll position is exactly where it was
    # left. Only a session this window has never shown — or one the LRU
    # evicted — pays for a paint.
    # WHERE THE SESSION WE ARE LEAVING WAS SITTING. Read now, off the
    # widget that is still mapped — a hidden text widget's yview is
    # computed against a geometry its geometry manager no longer
    # maintains, so asking it on the way back is asking the wrong
    # oracle. See _stash_edge / _restore_edge.
    _stash_edge $session
    set kept [dict exists $logs $sid]
    set session $sid
    # THE SESSION LIST LEARNS THIS HERE, not at the next turn boundary.
    #
    # USER 2026-09-17: "the highlighted session doesn't seem to be the
    # actually attached one … both sessions have the icon prefix but one
    # also has different colour". `::vsidebar::refresh` was the only
    # thing that ever moved the ▸, the listbox selection or the accent
    # colour, and it runs on turn start/end — so between switches all
    # three sat on whichever session was open at the LAST turn, and a
    # switch from the palette's "goto" moved nothing at all.
    #
    # `mark_current`, not `refresh`: refresh blocks on GET /api/sessions,
    # and this proc's whole reason for existing is that a switch costs
    # one `pack`.
    catch { ::vsidebar::mark_current }
    _claim_log $sid
    _show_log $sid
    _touch_log $sid
    set busy [is_running $sid]
    _sync_buttons
    retitle_window
    variable ev_cursor
    if {$kept} {
        # Nothing to paint, and nothing to scroll. A kept transcript
        # keeps its SCROLL POSITION too — that was in the promise
        # ("scroll position, expanded blocks and decoded images exactly
        # where they were left") and a `see end` here quietly broke it,
        # yanking you to the bottom of a session you had switched away
        # from mid-read. The follow button decides the live edge, the
        # same as it does while you sit in one session — except for the
        # one thing the button cannot know on its own: whether you left
        # this session AT its live edge. See _restore_edge.
        _restore_edge $sid
        variable ev_stream
        # A RUNNING SESSION IS NOT RE-HANDSHAKED ANY MORE.
        #
        # It used to be: a hidden session dropped its deltas, so coming
        # back meant asking the engine for a turn-snapshot to rebuild
        # what was missed. `_events_reconnect` is the DROPPED-SOCKET
        # path — it sets ev_reconnect_pending, and `_flush_replay` then
        # deletes from turn_reload_anchor to the end of the transcript
        # before repainting. On a plain view switch that is a whole live
        # region wiped and rebuilt for nothing, which is the visible
        # "the transcript reloads when I switch back" (USER 2026-09-16).
        #
        # A hidden session now paints its own stream into its own widget
        # (see _live_ctx), so there is nothing missed and nothing stale:
        # the stream stayed up, the text is already there, and the only
        # correct action is none.
        if {![info exists ev_stream($sid)] || $ev_stream($sid) eq ""} {
            # Quiet, and its stream is gone (a drop still backing off, or
            # a gateway that was down when it was opened). Otherwise the
            # stream has been up the whole time this session was hidden
            # and there is nothing to do: reconnecting a healthy stream
            # on every switch would drop and refetch for no reason, which
            # is most of what the old one-stream design cost.
            if {![is_remote $sid]} { _open_session_stream $sid }
        }
        _sync_readonly
        _open_session_tail $sid kept
        return
    }
    # The live-markdown region points into text that `clear` below is about
    # to delete — abandon it, and cancel its debounce, before repainting.
    _reset_live_delta
    variable live_last_kind
    set live_last_kind ""
    clear
    # THE RENDER CACHE. A session being painted for the FIRST time (or
    # after an eviction) still avoids the HTTP round trip when its parsed
    # history is cached; a not-yet-cached session fetches once. Either
    # way the transcript's last_msg_id becomes the event stream's resume
    # cursor, so the stream carries the live tail (and any gap since this
    # paint) and never re-sends the whole history.
    variable hist_cache
    if {![info exists hist_cache]} { set hist_cache {} }
    set hist {}
    if {[dict exists $hist_cache $sid]} {
        set hist [dict get $hist_cache $sid]
        if {[catch { _paint_history $hist } e]} {
            log_insert "[format [::vi18n::mc {failed to paint cached history: %s}] $e]\n" system
        }
    } elseif {![catch {
        set hist [::vapi::get_json "/api/sessions/$sid/history"]
    } e]} {
        if {[catch { _paint_history $hist } pe]} {
            log_insert "[format [::vi18n::mc {failed to load history: %s}] $pe]\n" system
        }
        dict set hist_cache $sid $hist
    } else {
        log_insert "[format [::vi18n::mc {failed to load history: %s}] $e]\n" system
    }
    _note_remote $sid $hist
    # The resume cursor is the msg_id this paint reached. The history
    # endpoint returns it (phase 1a) precisely so a client never derives
    # its own — a stale cache yields a stale cursor, and the stream then
    # replays exactly the gap, which is the correct catch-up.
    set ev_cursor($sid) 0
    catch { set ev_cursor($sid) [dict get $hist last_msg_id] }
    # Same discipline for the note cursor: the endpoint hands over the
    # newest engine_notes.id it showed, not one the client derives from
    # the message list (a note's anchor is not its creation order).
    variable ev_note_cursor
    set ev_note_cursor($sid) 0
    catch { set ev_note_cursor($sid) [dict get $hist last_note_id] }
    # A freshly loaded view lands on the live edge — the replay is not
    # streaming output, so the follow button's "new output" would be a
    # lie; go there directly.
    set w [::vchat::log]
    if {[winfo exists $w]} { $w see end }
    _hide_follow_btn
    variable at_edge
    set at_edge($sid) 1
    if {![is_remote $sid]} { _open_session_stream $sid }
    _sync_readonly
    _open_session_tail $sid loaded
}

# Open the persistent event stream, resuming from this session's cursor.
#
# It replays persisted rows after the cursor, hands over any in-flight
# turn (turn-snapshot → running + the partial), marks the live boundary,
# then streams — and reconnects itself on a drop. This ONE stream
# replaces the old per-turn SSE stream AND the polling watcher:
# liveness, reattach catch-up, and a turn started by another window or
# surface all arrive through it, so there is no separate context poll
# and no 200-tick give-up that froze a long turn.
#
# A fresh open is NOT a reconnect: clear any stale per-turn anchor and
# the reconnect flag so the stream's snapshot (if a turn is live) sets
# them anew and the first replay does not clear the region just built.
proc ::vchat::_open_session_stream {sid} {
    variable turn_reload_anchor
    variable ev_reconnect_pending
    catch { dict unset turn_reload_anchor $sid }
    set ev_reconnect_pending($sid) 0
    _events_connect $sid
}

# The welcome's "⋮ All commands", a button in the transcript's text. It
# is the transcript's child, so it goes with the text that holds it.
proc ::vchat::_welcome_button {} {
    set w [::vchat::log]
    if {$w eq "" || ![winfo exists $w]} { return "" }
    set b $w.welcome_all
    catch { destroy $b }
    ttk::button $b -text "⋮ [::vi18n::mc {All commands}]" -command ::vcmdtree::open
    _writable $w {
        $w window create end -window $b -padx 6 -align center
    }
    return $b
}

# Everything an open owes the rest of the window, kept or freshly
# painted: the approvals that were open while nobody was watching, the
# welcome text an empty transcript needs, and the status bar's figures.
# `how` is what the transcript on screen COST: `kept` (the widget was
# never torn down, nothing repainted), `loaded` (first sight of this
# session in this window, or the LRU had evicted it), or "" for a caller
# that has no opinion.
proc ::vchat::_open_session_tail {sid {how ""}} {
    # Pick up any approval that was still open while this window was
    # closed. It now outlives the request that raised it (the engine parks
    # it in the ask registry and rides the bus), and GET /api/asks — which
    # existed for exactly this and had zero callers — is how the client
    # recovers it. Async, so opening a session pays no extra round trip
    # and a stale reply is dropped if the user has since moved on.
    catch {
        ::vapi::get_json_async "/api/asks" \
            [list ::vchat::_redraw_asks $sid [_ask_on_screen]]
    } _asks_async_err
    # Empty session: a small welcome so the chat pane is never a void.
    # It used to name keys ("Ctrl-K for the command palette … F1 for
    # help"), which a phone has not got. Now it holds the ⋮ button
    # itself: every command, and on a keyboard each row names its key
    # (lib/cmdtree.tcl).
    if {[[::vchat::log] index end-1c] <= 1} {
        log_insert "[::vi18n::mc {▲ VELES — neon impressionism}]\n" system
        log_insert [::vi18n::mc {  type a message below to begin. Every command is here:}] status
        _welcome_button
        log_insert "\n" status
    }
    variable last_eviction
    # THE LABEL, NOT THE ID. The id is in the title strip and one
    # right-click away; this line is the one thing the user reads on a
    # switch, and `20260916_160524_4a676a` tells them nothing they were
    # asking.
    set label $sid
    catch {
        set l [::vsidebar::label_for $sid]
        if {$l ne ""} { set label $l }
    }
    set line [format [::vi18n::mc {session %s}] $label]
    # AND WHAT IT COST. Switching away and back repaints nothing — the
    # widget is kept, scroll position, expanded blocks, decoded images
    # and all — while a first open (or a return after eviction) pays a
    # full paint. Those look identical from the outside, so the one
    # question the design answers ("did that just rebuild my
    # transcript?") had no answer on screen. USER 2026-09-16: "the whole
    # transcript seems to reload/refresh on reopen too, not sure if
    # that's intended, might be intended." It is intended, in one of the
    # two cases; saying which is what makes it legible.
    # Spelled out rather than `mc $how`: a catalogue key built from a
    # variable is invisible to the scraper that checks every `mc` key
    # has an entry (and every entry a key), and invisible to whoever
    # translates the file.
    if {$how eq "kept"} {
        append line " · [::vi18n::mc {kept}]"
    } elseif {$how eq "loaded"} {
        append line " · [::vi18n::mc {loaded}]"
    }
    if {$last_eviction ne ""} {
        append line " · $last_eviction"
        set last_eviction ""
    }
    ::vstatus::set_msg $line
    catch { refresh_route $sid }
    # The status bar's context/usage numbers belong to the session ON
    # SCREEN — a switch without a refetch leaves the previous session's
    # "9% of 1M" sitting there for up to 15s. USER: "seems to be stuck
    # even when I jump between sessions".
    catch { ::vstatus::poll_now }
    focus .mid.chatwrap.chat.input
}

# ── one collapsible, three callers ───────────────────────────────────
#
# Reasoning, tool results and engine notes are all "a clickable header
# with an elided body under it". That was written twice and was about to
# be written a third time for notes; instead it is written once here.
#
# What the copies had to learn separately, and what this now carries in
# one place:
#
#  - `tag cget -elide` returns "" when unset, so `!$elide` crashes with
#    "can't use empty string as operand". The state lives in an array
#    keyed by the block, never read back off the widget.
#  - THE HEADER TAG COVERS ITS TRAILING NEWLINE. Deleting the range to
#    rewrite the header deletes the line break with it, and re-inserting
#    without it welds the body onto the header's line — USER: "Reasoning
#    blocks now expand on the same line as the collapsed summary and it
#    looks weird". Put back exactly what was there.
#  - the toggle is a plain proc taking the key as an argument, never a
#    closure over a loop variable.
#
# KEY. The block's identity, and it must be derived from the MESSAGE ID
# for anything that can be repainted — a source toggle, a theme change
# and a per-message flip all call `rebuild`, which repaints from `raw`.
# A key minted from the clock is a new block every repaint, so an open
# block closes itself and its stored state leaks. Notes and tool rounds
# pass ids; reasoning now does too, where it used to use `clock clicks`.
#
# OPTS is a dict:
#   closed   header text when collapsed        (required)
#   open     header text when expanded         (required)
#   body     the text to hide                  (required)
#   htags    extra tags for the header line    (default {})
#   btags    extra tags for the body           (default {})
#   bconf    `tag configure` options for the body tag
#   start    1 to paint it already open        (default: remembered, else 0)
proc ::vchat::_collapsible {w key opts} {
    variable collapse_open
    variable collapse_text
    if {![info exists collapse_open]} { array set collapse_open {} }
    set closed [dict get $opts closed]
    set open_t [dict get $opts open]
    set body   [dict get $opts body]
    set htags  [expr {[dict exists $opts htags] ? [dict get $opts htags] : {}}]
    set btags  [expr {[dict exists $opts btags] ? [dict get $opts btags] : {}}]
    set bconf  [expr {[dict exists $opts bconf] ? [dict get $opts bconf] : {}}]

    set hdr_tag "chdr_$key"
    set body_tag "cbody_$key"
    # Remembered state wins, so a repaint restores what was on screen.
    set is_open 0
    if {[info exists collapse_open($key)]} {
        set is_open $collapse_open($key)
    } elseif {[dict exists $opts start]} {
        set is_open [dict get $opts start]
    }
    set collapse_open($key) $is_open
    set collapse_text($key) [list $closed $open_t]

    _writable $w {
        $w tag configure $body_tag -elide [expr {!$is_open}] {*}$bconf
        _run $w "[expr {$is_open ? $open_t : $closed}]\n" \
            [concat $htags [list $hdr_tag]]
        _run $w "$body\n" [concat $btags [list $body_tag]]
    }
    ::vtext::click $w $hdr_tag [list ::vchat::_toggle_collapsible $key]
    $w tag bind $hdr_tag <Enter> {%W configure -cursor hand2}
    $w tag bind $hdr_tag <Leave> {%W configure -cursor xterm}
    return $body_tag
}

proc ::vchat::_toggle_collapsible {key} {
    variable collapse_open
    variable collapse_text
    set w [::vchat::log]
    set hdr_tag "chdr_$key"
    set body_tag "cbody_$key"
    if {[catch { $w tag ranges $body_tag } ranges] || $ranges eq ""} { return 0 }
    set is_open 0
    if {[info exists collapse_open($key)]} { set is_open $collapse_open($key) }
    set is_open [expr {!$is_open}]
    set collapse_open($key) $is_open
    $w tag configure $body_tag -elide [expr {!$is_open}]

    lassign [$w tag ranges $hdr_tag] hs he
    if {$hs eq "" || ![info exists collapse_text($key)]} { return $is_open }
    lassign $collapse_text($key) closed open_t
    set new [expr {$is_open ? $open_t : $closed}]
    # See the header-newline note above: the range includes it.
    if {[string match "*\n" [$w get $hs $he]]} { append new "\n" }
    # Read the header's tags BEFORE deleting it.
    #
    # This used to be `[$w tag names $hs]` on the line AFTER the delete.
    # _collapsible lays the header down as "header\n" immediately followed
    # by the body, so once the header is gone, index $hs IS the body's
    # first character and `tag names` returns the BODY's tags — including
    # cbody_$key. The rebuilt header inherited it, so the next collapse
    # set -elide 1 on a tag that now covered the header too: header and
    # body both vanished, and elided text cannot be clicked, so the block
    # was unrecoverable without a full repaint.
    #
    # That is why reopening the session "fixed" it — clear/forget_raw
    # drops collapse_open and the chdr_*/cbody_* tags, so the block was
    # painted fresh and correct. USER: "the reasoning blocks still just
    # disappear instead of being collapsed on a live transcript, only
    # reopening a transcript makes them reappear".
    set hdr_tags [$w tag names $hs]
    _writable $w {
        $w delete $hs $he
        _run_at $w $hs $new [concat $hdr_tags [list $hdr_tag]]
    }
    return $is_open
}

# Whether a block is currently expanded (for tests and for callers that
# want to paint a summary differently).
proc ::vchat::collapsed {key} {
    variable collapse_open
    if {![info exists collapse_open($key)]} { return 1 }
    return [expr {!$collapse_open($key)}]
}

# Collapsible reasoning: a clickable header + elided body.
# Collapsed by DEFAULT — long chains of thought must not drown the
# transcript.
proc ::vchat::_render_collapsible_reasoning {id text} {
    set w [::vchat::log]
    set nlines [llength [split [string trimright $text] "\n"]]
    _collapsible $w "reason$id" [dict create \
        closed "▶ reasoning ($nlines lines, click to expand)" \
        open   "▼ reasoning ($nlines lines, click to collapse)" \
        body   $text \
        htags  [list reasonhdr$id] \
        bconf  [list -foreground $::vt::T(reasoning) \
                     -font {TkDefaultFont 10} -lmargin1 12 -lmargin2 12]]
    _writable $w {
        $w tag configure reasonhdr$id -foreground $::vt::T(reasoning) \
            -font {TkDefaultFont 10 italic}
    }
}

# One rendering of a tool invocation, shared by the live stream and the
# reload path.
#
# It exists because those two disagreed, visibly. `on_tool` interpolated
# the args straight into its header line — and `payload` has already been
# through ::json::json2dict by then, so every `\n` escape in the arguments
# is a REAL newline. A `patch` call therefore dumped whole file fragments,
# unescaped and untruncated, into what is nominally one "⚙" line, while
# the reload path truncated the same args at 200 chars. The same call
# rendered two different ways depending on whether you watched it.
# USER: "actual newlines inside of the output should remain newlines
# instead of garbling everything together".
#
# A HEADER is the one place a newline is wrong — it is a single line by
# construction. Real output keeps its newlines; the full result keeps
# them too, behind the collapsible. Only this one line is flattened.
proc ::vchat::_tool_inv {name args_text} {
    if {$args_text eq "" || $args_text eq "{}"} { return $name }
    regsub -all {\s+} $args_text " " flat
    set flat [string trim $flat]
    if {$flat eq ""} { return $name }
    if {[string length $flat] > 200} { set flat "[string range $flat 0 200]…" }
    return "$name $flat"
}

# One line of tool RESULT preview for the live stream — same flattening,
# same reason. The reload path puts the whole result behind a collapsible
# with its newlines intact; this is the one-line teaser above it.
proc ::vchat::_tool_preview {text} {
    regsub -all {\s+} $text " " flat
    set flat [string trim $flat]
    if {[string length $flat] > 200} { set flat "[string range $flat 0 200]…" }
    return $flat
}

# A tool call: "⚙ name + invocation" with the result behind a
# click-to-expand header. The result can be a full file dump; eliding it
# (collapsed by default, one click to reveal the whole thing) beats
# hard-truncating at 200 chars with no way to see the rest. The args are
# shown on the header line so a round reads as a CALL (name + what it was
# given), not a bare noun — USER: "show the invocation, not just the result".
proc ::vchat::_render_tool_round {key tname targs tresult} {
    set w [::vchat::log]
    set inv [_tool_inv $tname $targs]
    _run $w "⚙ $inv" tool
    if {$tresult eq ""} {
        _run $w "\n" tool
        return
    }
    _run $w "\n" tool
    # Per-tool renderers, dispatched on the tool name: a result is not
    # opaque text, it is what a specific tool did. Adding a shape means
    # adding an arm here FIRST (the transcriptspec suite pins each one).
    if {$tname eq "patch"} {
        _render_patch_result $w $tresult
        return
    }
    set nchars [string length $tresult]
    # `key` is the WHOLE collapsible key, not a suffix — callers pass a
    # name that is already unique ("tool<id>.<n>" on the reload ladder,
    # "tool<sid>.<n>" live). Tagging the collapsible "tool$key" here
    # double-prefixed it, and the live path needs the live and rebuilt
    # round to share ONE key so a round expanded live stays expanded
    # across the reconcile.
    _collapsible $w $key [dict create \
        closed "  ▸ result ($nchars chars — click to expand)" \
        open   "  ▾ result ($nchars chars — click to collapse)" \
        body   $tresult \
        htags  [list tool] \
        btags  [list tool] \
        bconf  [list -lmargin1 12 -lmargin2 12]]
}

# A `patch` result is a summary line plus the unified diff the engine
# built from the exact match offsets. The diff IS the display — shown as
# diff lines in the round itself (adds green, dels red, hunks cyan, the
# review pane's palette), not as opaque text behind yet another click.
# USER ground rule: "patch should be shown as diff lines".
#
# Rendered by the ONE tool-round renderer, so live and reload get it
# identically — the live preview line is replaced by this at on_done.
proc ::vchat::_render_patch_result {w tresult} {
    set lines [split $tresult "\n"]
    set summary [lindex $lines 0]
    if {$summary eq ""} { set summary "patch" }
    _run $w "  $summary\n" tool
    foreach line [lrange $lines 1 end] {
        if {$line eq ""} { continue }
        set tag tool
        # `string match "+++"` would only exclude the BARE header; the
        # real header is "+++ path", so the guard must be a prefix —
        # otherwise the file labels render as diff content.
        if {[string match "+*" $line] && ![string match "+++*" $line]} { set tag diffadd }
        if {[string match "-*" $line] && ![string match "---*" $line]} { set tag diffdel }
        if {[string match "@@*" $line]} { set tag diffhdr }
        _run $w "  $line\n" $tag
    }
}

# Is THIS attached view the target of the saved join? Matching only a
# session id would lend the join id to a same-named session elsewhere.
proc ::vchat::_joined_session {sid} {
    if {![llength [info commands ::vinst::remote_join]]} { return 0 }
    set spec [::vinst::remote_join]
    if {![dict size $spec]} { return 0 }
    foreach k {instance target_session join_id} {
        if {![dict exists $spec $k] || [dict get $spec $k] eq ""} { return 0 }
    }
    return [expr {[dict get $spec instance] eq [::vapi::attached]
        && [dict get $spec target_session] eq $sid}]
}

proc ::vchat::send_current {} {
    variable session
    variable busy
    variable turn_anchor
    variable turn_reload_anchor
    variable tool_rounds
    if {$session eq ""} {
        ::vsidebar::new_session
        set session [::vsidebar::selected_sid]
        if {$session eq ""} return
    }
    # Another node's session is read here, not continued.
    if {[is_remote $session]} {
        variable remote
        lassign [dict get $remote $session] node at
        ::vstatus::set_msg [format \
            [::vi18n::mc {read-only: this session lives on %1$s (as read at %2$s)}] $node $at]
        return
    }
    variable attachments
    if {![info exists attachments]} { set attachments {} }
    set txt [string trim [.mid.chatwrap.chat.input get 1.0 end]]
    # An attachment on its own IS a message — "what is this?" with a
    # screenshot and no words is the commonest way to send one.
    if {$txt eq "" && ![llength $attachments]} return
    # `/secret set NAME <value>` keeps only its command part in the
    # input history (which outlives the session): the value is dropped
    # by the /secret arm and must not survive here either.
    if {[regexp {^(/secret\s+set\s+\S+)\s+\S} $txt -> safe]} {
        ::vedit::hist_push $safe
    } elseif {$txt ne ""} { ::vedit::hist_push $txt }

    # Slash commands (desktop-local — never sent to the model). They
    # work MID-TURN too: they touch the UI or the session state, not
    # the stream, so the old busy gate only kept them out by accident.
    # USER: "can't steer from desktop during agent turns … also applies
    # to other slash commands apparently".
    # A joined view has its own command boundary. Only /leave is
    # accepted; /steer, /secret and every other slash command must not
    # run as an ordinary local desktop command. chat_send routes /leave
    # through the target's joined-command endpoint as well.
    if {[string index $txt 0] eq "/" && [_joined_session $session]} {
        .mid.chatwrap.chat.input delete 1.0 end
        if {[string equal -nocase $txt "/leave"]} {
            # The target owns exact-binding leave; chat_send returns its
            # handled response, which restores the saved local view.
        } else {
            emit_message system [::vi18n::mc {commands stay on your own session; /leave returns from the joined session}]
            return
        }
    } elseif {[string index $txt 0] eq "/"} {
        .mid.chatwrap.chat.input delete 1.0 end
        ::vchat::slash_command $txt
        return
    }

    # A turn is running in THIS session: typing becomes steering. The
    # engine queues it and injects it after the current tool round
    # (agent.pending_steer), so the model reads it before deciding its
    # next call. Same endpoint /steer uses — one code path.
    # json_string, NOT a bare value: the value is free text and the
    # dict heuristic mangles multi-word strings (see its comment).
    # A joined line is always the shared join queue's next owner line,
    # never the ordinary desktop /steer endpoint. The target handles
    # ordering with its running turn.
    if {$busy && ![_joined_session $session]} {
        .mid.chatwrap.chat.input delete 1.0 end
        if {[catch {::vapi::post_json \
            "/api/sessions/$session/steer" \
            [dict create message [::vapi::json_string $txt]]} e]} {
            emit_message system [::vi18n::mc {steer failed: %s} $e]
        } else {
            # state: "live" reached the running turn (injects after
            # this tool round); "parked" docks until the next turn —
            # say WHICH, because a steer parked behind what looks like
            # a turn (a compaction, a queued-job drain) that never
            # injects reads exactly like an eaten steer.
            set st [expr {[dict exists $e state] ? [dict get $e state] : "queued"}]
            if {$st eq "parked"} {
                log_insert "[format [::vi18n::mc {↳ steer parked: %s}] $txt]\n[::vi18n::mc {  (no live turn — it opens the next one)}]\n" system
                ::vstatus::set_msg [format [::vi18n::mc {steer parked: %s}] $txt]
            } else {
                log_insert "[format [::vi18n::mc {↳ steer queued: %s}] $txt]\n" system
                ::vstatus::set_msg [format [::vi18n::mc {steer queued: %s}] $txt]
            }
        }
        return
    }

    .mid.chatwrap.chat.input delete 1.0 end
    # The MESSAGE carries the markers, so the transcript shows the
    # attachment inline straight away and a reopened session shows it
    # again — the engine stores what we send it.
    set body $txt
    foreach a $attachments {
        if {$body ne ""} { append body "\n" }
        append body "MEDIA:$a"
    }
    # Remember where the transcript stood BEFORE this turn's optimistic
    # user message. If the stream drops mid-turn and reconnects, the
    # replay re-sends the persisted user message (the resume cursor is
    # pre-turn), so the reconnect clears from here first — one copy, from
    # the store, not the optimistic one plus the replay. turn_anchor (set
    # below, AFTER the user message) is the narrower one on_done uses to
    # replace just the assistant's live region.
    dict set turn_reload_anchor $session [[::vchat::log] index end-1c]
    emit_message user $body
    # No ad-hoc "\n" after the user message: the block separator owns
    # the blank line, so the live transcript and a reopened one are
    # byte-identical (R1 — this used to be the one break only the live
    # path added).
    set sent $attachments
    set attachments {}
    _refresh_chips
    set route ""
    catch { unset tool_rounds($session) }
    dict set turn_anchor $session [[::vchat::log] index end-1c]
    _mark_running $session 1
    ::vstatus::set_msg [::vi18n::mc {thinking…}]
    ::vfind::reset_marks
    if {[catch {
        set response [::json::json2dict [::vapi::chat_send $session $txt $sent]]
        if {[dict exists $response error]} {
            error [dict get $response error]
        }
        if {[_joined_session $session]
            && !([dict exists $response join] && [dict get $response join])} {
            error "joined target did not accept the join; refusing to send ordinary chat"
        }
        if {[dict exists $response join] && [dict get $response join]
            && [dict exists $response handled] && [dict get $response handled]} {
            # A joined-session command runs on the target gateway; it
            # does not open a model turn here. /leave returns the view.
            _mark_running $session 0
            set command_text ""
            if {[dict exists $response text]} {
                set command_text [dict get $response text]
            }
            if {$command_text ne ""} {
                emit_message system $command_text
            }
            if {$command_text ne ""} {
                set spec [::vinst::remote_join]
                if {[dict size $spec] && [dict get $spec target_session] eq $session} {
                    set return_inst [dict get $spec from_instance]
                    set return_sid [dict get $spec from_session]
                    if {$return_inst ne [::vapi::attached]} {
                        if {![::vapp::attach $return_inst]} {
                            ::vinst::set_remote_join {}
                            ::vchat::open_session $session
                            return
                        }
                    }
                    ::vinst::set_remote_join {}
                    ::vchat::open_session $return_sid
                    catch {::vsidebar::refresh}
                }
            }
        }
    } e]} {
        # LOUDLY, in the transcript: a status-bar-only error is how an
        # entire evening of version skew went undiagnosed — every send was
        # failing and the conversation said nothing.
        emit_message system [::vi18n::mc {send failed: %s} $e]
        turn_error "request failed: $e"
    }
}

# ── attachments, user → model ───────────────────────────────────────
#
# USER: "Is image/file support implemented at all? I swear qwen and
# other slopshit models always reassured me this has long been done,
# but image preview just opens my browser, no option to even attach or
# see it in chat or otherwise."
#
# Half of it was real: a REPLY carrying `MEDIA:/path` rendered inline.
# The other direction did not exist — `/image` printed "image noted (use
# vision_analyze tool in your next prompt)" and attached nothing, while
# `/help` said it attached, and `image_url` appeared nowhere in the
# provider client. The model never saw a pixel.
#
# Attachments ride the SAME `MEDIA:` marker as a reply, so they persist
# with the message, render in the transcript, and need no second
# mechanism anywhere.

proc ::vchat::attach {path} {
    variable attachments
    if {![info exists attachments]} { set attachments {} }
    set path [string trim $path]
    if {$path eq ""} { return 0 }
    # A path with no directory part is almost always a typo — and a
    # relative path resolves against the GATEWAY's cwd, not yours,
    # which would attach a different file than the one you named.
    if {![string match "http://*" $path] && ![string match "https://*" $path]} {
        set path [file normalize $path]
        if {![file isfile $path]} {
            ::vstatus::set_msg [format [::vi18n::mc {no such file: %s}] $path]
            return 0
        }
    }
    if {[lsearch -exact $attachments $path] >= 0} {
        ::vstatus::set_msg [format [::vi18n::mc {already attached: %s}] [file tail $path]]
        return 0
    }
    lappend attachments $path
    _refresh_chips
    ::vstatus::set_msg [format [::vi18n::mc {attached %1$s (%2$s pending)}] [file tail $path] [llength $attachments]]
    return 1
}

proc ::vchat::detach {path} {
    variable attachments
    if {![info exists attachments]} { return }
    set attachments [lsearch -all -inline -not -exact $attachments $path]
    _refresh_chips
    return
}

proc ::vchat::attach_dialog {} {
    # A phone's picker is Android's (lib/android.tcl).
    if {[llength [info commands ::vandroid::pick_files]] && [::vandroid::pick_files]} {
        return
    }
    set types {
        {"Images"    {.png .jpg .jpeg .gif .webp .bmp}}
        {"Text"      {.txt .md .rs .tcl .py .json .toml .yaml .yml .sh}}
        {"All files" *}
    }
    set paths [tk_getOpenFile -multiple 1 -title [::vi18n::mc {Attach to the next message}] \
        -filetypes $types]
    foreach p $paths { attach $p }
    return
}

proc ::vchat::_refresh_chips {} {
    variable attachments
    if {![info exists attachments]} { set attachments {} }
    set f .mid.chatwrap.chat.chips
    if {![winfo exists $f]} { return }
    foreach c [winfo children $f] { destroy $c }
    if {![llength $attachments]} {
        catch { pack forget $f }
        return
    }
    set i 0
    foreach a $attachments {
        incr i
        ttk::frame $f.c$i
        ttk::label $f.c$i.n -foreground $::vt::T(fg_dim) \
            -font {TkDefaultFont 9}
        ::vi18n::label $f.c$i.n {📎 %s} [file tail $a]
        # The × is the whole reason the chips exist: attaching the wrong
        # file must not mean retyping the message.
        ttk::button $f.c$i.x -text "×" -width 2 -command [list ::vchat::detach $a]
        pack $f.c$i.n -side left
        pack $f.c$i.x -side left -padx {2 8}
        pack $f.c$i -side left
    }
    catch { pack $f -side bottom -fill x -padx 6 -pady {0 2} \
                -before .mid.chatwrap.chat.bar }
    return
}


# One shared command, run by the ENGINE. Returns 1 when it answered.
#
# The desktop implements a handful of commands natively because they
# open panes (/model's picker, /log, /find). Everything else the engine
# already knows — /queue, /context, /limits, /sandbox, /secret, /cron,
# /guards, /hooks — used to be either reimplemented here or advertised
# and then refused. This is the one call that ends that.
proc ::vchat::_try_shared_command {sid cmd} {
    set verb [string tolower [lindex $cmd 0]]
    if {$verb eq "/join" || $verb eq "/leave"} {
        return [_join_command $sid $verb [string trim [string range $cmd [string length $verb] end]]]
    }
    if {[catch {
        set r [::vapi::post_json /api/command \
            [dict create session_id [::vapi::json_string $sid] \
                          instance [::vapi::json_string [::vapi::attached]] \
                          command [::vapi::json_string $cmd]]]
    } e]} {
        # A transport failure is not "no such command": say so rather
        # than sending the user to look for a typo.
        emit_message system [format [::vi18n::mc {%s failed: %s}] $cmd $e]
        return 1
    }
    set handled 0
    catch { set handled [dict get $r handled] }
    if {!([string is true -strict $handled] || $handled eq "1")} { return 0 }
    set text ""
    catch { set text [dict get $r text] }
    if {$text ne ""} { emit_message system $text }
    set reload 0
    catch { set reload [dict get $r reload] }
    # A transcript-editing command (compact, undo, clear) says so, and
    # the view has a local copy — repaint it, the same one call /clear
    # makes. (It was `_drop_cache` + `open_session`, which forgets the
    # parsed history and then takes open_session's KEPT branch, which
    # paints nothing.)
    if {[string is true -strict $reload] || $reload eq "1"} {
        catch { ::vchat::reload_session $sid }
    }
    # A CHAT ALIAS (`/qwen-flash <prompt>`) minted a session and pointed
    # it at its own route. The engine hands back the id because the
    # switch is the caller's to make — without this the reply names a
    # session the pane never opens, and the next thing typed goes to the
    # OLD one on the OLD route.
    #
    # `prompt` is the rest of the alias line. The engine deliberately
    # does not run it: /api/command answers commands, and a turn belongs
    # on the chat path that streams. So it is sent from here, after the
    # switch, exactly as if it had been typed into the new session.
    set new_sid ""
    catch { set new_sid [string trim [dict get $r session_id]] }
    if {$new_sid ne "" && $new_sid ne $sid} {
        catch { ::vsidebar::refresh }
        catch { ::vchat::open_session $new_sid }
        set prompt ""
        catch { set prompt [dict get $r prompt] }
        if {[string trim $prompt] ne ""} {
            # Literally as if typed: fill the input and press send. There
            # is no send-this-string entry point — `send_current` reads
            # the widget — and inventing one would be a second path
            # through the attachment, history and steering logic.
            catch {
                .mid.chatwrap.chat.input delete 1.0 end
                .mid.chatwrap.chat.input insert 1.0 $prompt
                ::vchat::send_current
            }
        }
    }
    return 1
}

# Owner joins: the target gateway owns execution and persistence; the
# client keeps only its return view in the local instance registry.
proc ::vchat::_join_command {sid verb rest} {
    set here [::vapi::attached]
    if {$verb eq "/join" && [dict size [::vinst::remote_join]]} {
        emit_message system [::vi18n::mc {/leave the current joined session before opening another}] ; return 1
    }
    if {$verb eq "/join"} {
        if {$rest eq ""} { emit_message system [::vi18n::mc {usage: /join <session> or /join <instance>/<session>}] ; return 1 }
        set slash [string first / $rest]
        if {$slash > 0} {
            set inst [string range $rest 0 [expr {$slash - 1}]]
            set target [string range $rest [expr {$slash + 1}] end]
        } else { set inst $here ; set target $rest }
        if {$inst eq $here && $target eq $sid} { emit_message system [::vi18n::mc {already in that session}] ; return 1 }
        set body [dict create \
            session_id [::vapi::json_string $target] \
            instance [::vapi::json_string $here] \
            owner_session [::vapi::json_string $sid]]
        if {[catch {::vapi::with_instance $inst {::vapi::post_json /api/join $body}} response]} {
            emit_message system [format [::vi18n::mc {join failed on %s: %s}] $inst $response] ; return 1
        }
        set join [dict get $response join]
        set spec [dict create join_id [dict get $join join_id] instance $inst \
            target_session $target from_instance $here from_session $sid]
        if {$inst ne $here && ![::vapp::attach $inst]} {
            set rollback ""
            catch {::vapi::with_instance $inst {::vapi::post_json /api/join/leave \
                [dict create join_id [::vapi::json_string [dict get $join join_id]] \
                    session_id [::vapi::json_string $target]]}} rollback
            if {$rollback ne ""} {
                emit_message system [format [::vi18n::mc {join rollback on %s failed: %s}] $inst $rollback]
            }
            if {[::vapi::attached] ne $here} { ::vapp::attach $here }
            emit_message system [format [::vi18n::mc {join: attach to %s failed; previous view kept}] $inst] ; return 1
        }
        if {![::vinst::set_remote_join $spec]} {
            set why [::vinst::error_text]
            set rollback ""
            catch {::vapi::with_instance $inst {::vapi::post_json /api/join/leave \
                [dict create join_id [::vapi::json_string [dict get $join join_id]] \
                    session_id [::vapi::json_string $target]]}} rollback
            if {$rollback ne ""} {
                emit_message system [format [::vi18n::mc {join rollback on %s failed: %s}] $inst $rollback]
            }
            ::vinst::set_remote_join {}
            if {[::vapi::attached] ne $here} { ::vapp::attach $here }
            emit_message system [format [::vi18n::mc {join cancelled; could not save its return state: %s}] $why] ; return 1
        }
        ::vchat::open_session $target
        catch {::vsidebar::refresh}
        emit_message system [format [::vi18n::mc {joined %s on %s with owner rights; /leave returns to %s/%s}] $target $inst $here $sid]
        return 1
    }
    set spec [::vinst::remote_join]
    if {![dict size $spec]} { emit_message system [::vi18n::mc {no joined desktop session}] ; return 1 }
    set inst [dict get $spec instance]
    if {$inst ne $here || [dict get $spec target_session] ne $sid} {
        emit_message system [format [::vi18n::mc {open the joined session before /leave (on %s)}] $inst] ; return 1
    }
    if {[catch {::vapi::with_instance $inst {::vapi::post_json /api/join/leave \
        [dict create join_id [::vapi::json_string [dict get $spec join_id]] \
            session_id [::vapi::json_string $sid]]}} response]} {
        emit_message system [format [::vi18n::mc {leave failed; join retained: %s}] $response] ; return 1
    }
    if {[dict exists $response error]} { emit_message system [format [::vi18n::mc {leave failed; join retained: %s}] [dict get $response error]] ; return 1 }
    set return_inst [dict get $spec from_instance]
    set return_sid [dict get $spec from_session]
    set setbefore $::vchat::session
    if {$return_inst ne $here && ![::vapp::attach $return_inst]} {
        # The target leave already succeeded. Stay on the working target
        # rather than pointing at an instance that just refused to attach.
        if {[::vapi::attached] ne $inst} { ::vapp::attach $inst }
        ::vchat::open_session $setbefore
        emit_message system [format [::vi18n::mc {left the joined session, but restoring %s failed: %s}] $return_inst $::vstatus::last] ; return 1
    }
    if {![::vinst::set_remote_join {}]} {
        # The target is already left. Keep the local credential in memory
        # so /leave can finish cleanup, and stay on a working attachment.
        if {$return_inst ne $here && [::vapi::attached] ne $return_inst} {
            ::vapp::attach $return_inst
        }
        ::vchat::open_session $setbefore
        emit_message system [::vi18n::mc {left on the target, but saving the return view failed; retry /leave}] ; return 1
    }
    if {$return_inst ne $here && [::vapi::attached] ne $return_inst} {
        emit_message system [format [::vi18n::mc {left the joined session, but %s is not attached}] $return_inst] ; return 1
    }
    ::vchat::open_session $return_sid
    catch {::vsidebar::refresh}
    emit_message system [format [::vi18n::mc {left the joined session; restored %s/%s}] $return_inst $return_sid]
    return 1
}

# ── Slash commands (desktop-local, same set as the CLI/TUI) ──────────
# POST a per-session setting and say what came back — including, when
# it failed, WHAT failed.
#
# The three arms this replaces each reported every error as
# "(gateway offline?)". The gateway was not offline: the endpoints
# answered 404 "session not active" for any session whose agent this
# gateway process had not built yet — which is every session after a
# config.toml save, because the file watcher's reload empties the agent
# cache. So `/plan on` printed a wrong cause and set nothing, and the
# user reasonably believed the mode was on. USER 2026-09-16: "I DID
# call /plan on that session."
#
# The engine half is fixed (the handlers build the agent now). This is
# the half that made it unreportable: a message that names the wrong
# cause is worse than no message, because it sends you to look in the
# wrong place.
proc ::vchat::_session_post {what path body} {
    if {$::vchat::session eq ""} {
        emit_message system [format [::vi18n::mc {%s: no session open}] $what]
        return
    }
    if {[catch {::vapi::post_json $path $body} r]} {
        emit_message system [format [::vi18n::mc {%s failed: %s}] $what $r]
        return
    }
    set msg ""
    catch { set msg [dict get $r message] }
    if {$msg eq ""} { catch { set msg [dict get $r error] } }
    if {$msg eq ""} { set msg "$what: no answer from the gateway" }
    emit_message system $msg
}

proc ::vchat::slash_command {cmd} {
    variable session
    set parts [split $cmd " "]
    set name [string range [lindex $parts 0] 1 end]
    set rest [join [lrange $parts 1 end] " "]

    # The composer is not the only command entry point: palettes and
    # keyboard actions call here directly. The joined-session boundary
    # therefore belongs here too, not only in send_current.
    if {[_joined_session $session] && [string tolower $name] ne "leave"} {
        emit_message system [::vi18n::mc {commands stay on your own session; /leave returns from the joined session}]
        return
    }

    # No private "background"/"bg"/"tasks"/"subagents"/"monitors"/"btw"
    # arms here. The taxonomy split (USER, 2026-09-17) made
    # `/background <cmd>` start a background SHELL and `/tasks` the TODO
    # list — both engine-side SHARED_COMMANDS now — and a desktop copy
    # of the old semantics is exactly the drift `_try_shared_command`
    # exists to end. Every one of these falls through to the default
    # arm, which POSTs /api/command and renders the engine's own answer,
    # so there is one implementation, not two that can disagree.
    switch -- $name {
        "help" - "h" - "?" {
            # The ENGINE's table, served, plus the desktop-local rows.
            # This was a 50-line literal — one of three hand-maintained
            # copies of a list that grows in `frontends/commands.rs` —
            # and it had drifted: it advertised `/config` and
            # `/verbose` while omitting `/queue`, `/context`,
            # `/secret`, `/guards`, `/sessions` and `/cron`, all of
            # which work. See cmds.tcl.
            emit_message system [::vcmds::help_text]
        }
        "new" - "reset" {
            ::vsidebar::new_session
            if {$rest ne ""} {
                if {[catch {::vapi::post_json \
                    "/api/sessions/[::vsidebar::selected_sid]/title" \
                    [dict create title [::vapi::json_string $rest]]} e]} {
                    emit_message system [::vi18n::mc {rename failed: %s} $e]
                }
            }
        }
        "sessions" {
            # The window, not a dump of the newest 100 ids (USER
            # 2026-10-04: "/sessions doesn't show full list"). Words
            # after it are the search.
            ::voverlay::open sessions
            if {$rest ne ""} {
                set ::vsessions::q $rest
                ::vsessions::search
            }
        }
        "resume" {
            if {$rest eq ""} {
                emit_message system [::vi18n::mc {usage: /resume <id or prefix>}]
            } else {
                ::vchat::resume $rest
            }
        }
        "theme" {
            if {$rest eq ""} {
                ::vpalette::open_themes
            } else {
                if {[catch {::vt::use_theme $rest} e]} {
                    emit_message system [format [::vi18n::mc {theme '%s' failed: %s}] $rest $e]
                } else {
                    ::vstatus::set_msg [format [::vi18n::mc {theme: %s}] $rest]
                }
            }
        }
        "model" {
            # No arguments: open the Models & settings overlay (same as
            # M-o m) — it shows the current default AND lets you apply a
            # per-session override. No more "use the cli" dead end.
            if {$rest eq ""} {
                ::voverlay::open models
            } else {
                # /model <name>: set it as the session override directly.
                if {$::vchat::session eq ""} {
                    ::vstatus::set_msg [::vi18n::mc {no session open}]
                    return
                }
                if {[catch {
                    ::vapi::post_json "/api/sessions/$::vchat::session/override" \
                        [dict create model [::vapi::json_string $rest]]
                } e]} {
                    emit_message system [::vi18n::mc {override failed: %s} $e]
                } else {
                    emit_message system [format [::vi18n::mc {session model override → %s}] $rest]
                }
            }
        }
        "config" {
            set data [::vapi::get_json "/api/status"]
            set lines {}
            foreach k [dict keys $data] {
                lappend lines "$k: [dict get $data $k]"
            }
            emit_message system [join $lines "\n"]
        }
        "status" {
            set data [::vapi::get_json /api/status]
            set lines {}
            lappend lines "status: [dict get $data status]"
            lappend lines "uptime: [dict get $data uptime_s]s"
            lappend lines "sessions: [dict get $data session_count]"
            lappend lines "platforms: [join [dict get $data platforms] ", "]"
            lappend lines "version: [dict get $data version]"
            if {![catch {::vapi::get_json /api/config} cfg]} {
                lappend lines "model: [dict get $cfg default_model] @ [dict get $cfg default_provider]"
            }
            lappend lines "current session: $::vchat::session"
            emit_message system [join $lines "\n"]
        }
        "export" {
            set fmt [expr {$rest eq "json" ? "json" : "markdown"}]
            if {[catch {::vchat::export_session $::vchat::session $fmt} e]} {
                emit_message system [::vi18n::mc {export failed: %s} $e]
            } else {
                emit_message system [format [::vi18n::mc {exported to %s}] $e]
            }
        }
        "source" { ::vchat::toggle_source }
        "preview" { ::vchat::toggle_preview }
        "log" { ::voverlay::open log }
        "reasoning" {
            variable show_reasoning
            set show_reasoning [expr {!$show_reasoning}]
            # ONE proc owns the fold. The command used to flip the
            # variable and stop there, so /reasoning mid-stream changed
            # neither the live elide nor the header glyph — the checkbox
            # worked, the command did nothing (R7).
            # …and it owns the ANNOUNCEMENT too, when it moved blocks
            # that were already on screen. Restating the plain line here
            # would throw away the count it just reported — the same
            # "two places say one thing" that made the command and the
            # checkbox disagree in the first place.
            if {[::vchat::_toggle_live_reasoning] == 0} {
                ::vstatus::set_msg [format [::vi18n::mc {reasoning display: %s}] [expr {$show_reasoning ? {on} : {off}}]]
            }
        }
        "keymap" - "bindings" {
            if {$rest ne ""} {
                if {$rest in {normie emacs vi}} {
                    ::vkeymap::use $rest
                    ::vstatus::set_msg [format [::vi18n::mc {keymap: %s}] $rest]
                } else {
                    ::vstatus::set_msg [format [::vi18n::mc {unknown keymap: %s (normie/emacs/vi)}] $rest]
                }
            } else {
                ::vkeymap::picker
            }
        }
        "language" {
            # The DESKTOP-LOCAL face of the language switch. The engine's
            # own /locale is a SHARED command (the TUI, REPL, IRC and
            # Telegram all get it) and it switches the GATEWAY's locale,
            # which is the wrong process for the window in front of you:
            # this re-texts the running app through ::vi18n::use.
            #
            # ONLY `language` is claimed here. `locale` and `lang` are
            # the engine's own spellings and deliberately keep falling
            # through to _try_shared_command, so a desktop user can
            # still reach the gateway-side switch — shadowing all three
            # would have removed the only way to change what IRC and
            # Telegram render.
            if {$rest ne ""} {
                if {[catch {::vi18n::use $rest} now]} {
                    ::vstatus::set_msg [format \
                        [::vi18n::mc {unknown language: %s}] $rest]
                } else {
                    ::vstatus::set_msg [format \
                        [::vi18n::mc {language: %s}] $now]
                }
            } else {
                ::vlang::picker
            }
        }
        "find" { ::vfind::show }
        "secret" {
            # `/secret set NAME` opens the masked field: a value typed
            # into this line would sit in the transcript and the command
            # history. Anything typed after the name is dropped here and
            # sent nowhere. list/unset are the engine's.
            if {[lindex $parts 1] eq "set"} {
                set nm [string trimleft [lindex $parts 2] {$}]
                if {$nm eq ""} {
                    emit_message system [::vi18n::mc {usage: /secret set NAME — the value goes into a masked field, never this line}]
                } else {
                    if {[llength $parts] > 3} {
                        emit_message system [::vi18n::mc {the text after the name was NOT sent anywhere — type the value into the masked field}]
                    }
                    ::vask::secret_dialog $nm ""
                }
                return
            }
            set sid [::vsidebar::selected_sid]
            if {$sid ne ""} { _try_shared_command $sid $cmd }
        }
        "setup" {
            # The desktop-local face of the setup wizard: `veles setup`
            # is the CLI's, this opens the Tk one (PLAN-setup-wizard-tk).
            # Re-runnable from here — palette row "Setup wizard…" too.
            ::vsetup::open
        }
        "steer" {
            if {$rest ne ""} {
                # Queue via API — the gateway picks it up on the next tool
                # round. Report failure AND the state: parked vs live is
                # the difference between "redirecting the agent now" and
                # "will open the next turn", and the old line said
                # "queued" for both.
                if {[catch {::vapi::post_json \
                    "/api/sessions/$::vchat::session/steer" \
                    [dict create message [::vapi::json_string $rest]]} e]} {
                    emit_message system [::vi18n::mc {steer failed: %s} $e]
                } else {
                    set st [expr {[dict exists $e state] ? [dict get $e state] : "queued"}]
                    ::vstatus::set_msg [format \
                        [::vi18n::mc [expr {$st eq "parked"
                            ? {steer parked: %s}
                            : {steer queued: %s}}]] $rest]
                }
            }
        }
        "branch" - "fork" {
            if {[catch {
                set data [::vapi::post_json \
                    "/api/sessions/$::vchat::session/branch" [dict create]]
                set new_id [dict get $data new_session_id]
                emit_message system [format [::vi18n::mc {branched → %s (select from sidebar)}] $new_id]
            } e]} {
                emit_message system [::vi18n::mc {branch failed: %s} $e]
            }
        }
        "undo" {
            # The transcript it drops rides back over the bus: the engine
            # publishes `history-changed`, and on_session_event reloads
            # the view (and every other watcher's). Status-bar text,
            # not a transcript line — the reload would repaint over it.
            set n [expr {$rest eq "" ? 1 : $rest}]
            if {[catch {
                set data [::vapi::post_json \
                    "/api/sessions/$::vchat::session/undo" [dict create n $n]]
                set dropped [dict get $data dropped]
                ::vstatus::set_msg [format [::vi18n::mc {undone: %1$s message(s) dropped}] $dropped]
            } e]} {
                emit_message system [::vi18n::mc {undo failed: %s} $e]
            }
        }
        "retry" {
            if {[catch {::vapi::post_json \
                "/api/sessions/$::vchat::session/retry" [dict create]} e]} {
                emit_message system [::vi18n::mc {retry failed: %s} $e]
            } else {
                ::vstatus::set_msg [::vi18n::mc {retrying last prompt…}]
            }
        }
        "clear" {
            # Real context clearing, same as the REPL and TUI: the
            # engine archives the session's live history (rows stay
            # searchable), and the transcript reloads empty. This
            # used to only reload the view and call that clearing.
            if {$::vchat::session ne ""} {
                if {[catch {::vapi::post "/api/sessions/$::vchat::session/clear" "{}"} err]} {
                    emit_message system [::vi18n::mc {clear failed: %s} $err]
                } else {
                    # reload_session, not _drop_cache + open_session:
                    # the transcript on screen is a KEPT widget, and
                    # open_session's kept branch paints nothing, so
                    # "cleared" used to leave every cleared message
                    # exactly where it was.
                    ::vchat::reload_session $::vchat::session
                    ::vstatus::set_msg [::vi18n::mc {context cleared (same session)}]
                }
            } else {
                ::vstatus::set_msg [::vi18n::mc {no session open}]
            }
        }
        "diff" {
            # git itself, not `bash -c`: the same line on every platform.
            if {[catch { exec git diff --stat HEAD 2>@1 } out]} {
                set out "(not a git repo)"
            }
            emit_message system $out
        }
        "tools" {
            if {[catch {::vapi::get_json /api/tools} data]} {
                emit_message system [::vi18n::mc {tools unavailable: %s} $data]
            } else {
                set b [dict get $data builtin]
                set d [dict get $data deferred]
                emit_message system "[format [::vi18n::mc {builtin (%s):}] [dict get $data builtin_count]]\n  [join $b "\n  "]\n\n[format [::vi18n::mc {deferred — tiered builtins + MCP (%s):}] [dict get $data deferred_count]]\n  [join $d "\n  "]"
            }
        }
        "skills" {
            # Ask the engine, which knows BOTH skill roots (~/.hermes for
            # migration compat and the config dir's skills/ for veles' own).
            # This used to glob ~/.hermes/skills directly, so it missed
            # every veles skill and reported a bare category count.
            if {[catch {::vapi::get_json /api/skills} data]} {
                emit_message system [::vi18n::mc {skills unavailable: %s} $data]
            } else {
                set skills [dict get $data skills]
                set count [dict get $data count]
                if {$count == 0} {
                    emit_message system [::vi18n::mc {no skills found}]
                } else {
                    set lines {}
                    foreach sk $skills {
                        set nm [dict get $sk name]
                        set desc [dict get $sk description]
                        set root [dict get $sk root]
                        lappend lines [format "  %-28s %s" "$nm ($root)" $desc]
                    }
                    emit_message system "[format [::vi18n::mc {%s skill(s):}] $count]\n[join $lines "\n"]"
                }
            }
        }
        "memory" {
            # /api/memory reports entries, char budgets and current usage.
            # This used to be a bare "use the memory tool" dead end.
            if {[catch {::vapi::get_json /api/memory} data]} {
                emit_message system [::vi18n::mc {memory unavailable: %s} $data]
            } else {
                set mem_used [dict get $data usage memory]
                set usr_used [dict get $data usage user]
                set mem_max [dict get $data limits memory]
                set usr_max [dict get $data limits user]
                set mem_n [llength [dict get $data memory]]
                set usr_n [llength [dict get $data user]]
                emit_message system [join [list \
                    "MEMORY  $mem_n entries  $mem_used / $mem_max chars" \
                    "USER    $usr_n entries  $usr_used / $usr_max chars"] "\n"]
            }
        }
        "plugins" {
            ::voverlay::open plugins
        }
        "acl" {
            ::voverlay::open acl
        }
        "usage" {
            ::voverlay::open usage
        }
        "profile" {
            # Was a hardcoded "default". Now asks the engine, which
            # knows the VELES_PROFILE env + hostname match.
            if {[catch {::vapi::get_json /api/profile} data]} {
                emit_message system [::vi18n::mc {profile unavailable: %s} $data]
            } else {
                set active [dict get $data active]
                set names [list]
                foreach p [dict get $data profiles] {
                    lappend names [dict get $p name]
                }
                emit_message system "[format [::vi18n::mc {active profile: %s}] $active]\n[::vi18n::mc {profiles:}] [join $names {, }]"
            }
        }
        "compress" - "compact" {
            # Runs the REAL summarizer now (one LLM call, same as the
            # automatic path). The engine returns "started" immediately
            # and compacts in the background — a summarizer can outlast
            # the HTTP timeout, and the old inline wait reported
            # "transport: timeout" while still completing server-side.
            if {$::vchat::session ne ""} {
                if {[catch {::vapi::post "/api/sessions/$::vchat::session/compress" "{}"} raw]} {
                    emit_message system [::vi18n::mc {compress failed: %s} $raw]
                } else {
                    set data [::json::json2dict $raw]
                    if {[dict exists $data started]} {
                        set before [dict get $data before]
                        emit_message system [format [::vi18n::mc {compacting %s messages in the background — the transcript reloads when it finishes}] $before]
                        # The background task holds the agent lock while
                        # it summarizes. Compaction is NOT a turn, so no
                        # turn event announces it; the engine publishes a
                        # `history-changed` bus event when the summarizer
                        # returns, and on_session_event unmarks running,
                        # reloads the transcript and repaints the context
                        # segment. No polling anywhere: busy in /context
                        # means "a turn is live", and a compaction is
                        # never a turn, so that key could not (and did
                        # not) carry the finish — see the 2026-09-07
                        # "context stats gone" incident in
                        # docs/issues/reference/PENDING.md.
                        catch { ::vchat::_mark_running $::vchat::session 1 }
                    } else {
                        emit_message system [format [::vi18n::mc {compressed: %1$s → %2$s messages, %3$s archived}] [dict get $data before] [dict get $data after] [dict get $data archived]]
                    }
                }
            } else {
                ::vstatus::set_msg [::vi18n::mc {no session open}]
            }
        }
        "image" - "attach" {
            if {$rest ne ""} {
                ::vchat::attach $rest
            } else {
                ::vchat::attach_dialog
            }
        }
        "copy" {
            # Copy the LAST assistant message's markdown source.
            variable raw
            variable raw_kind
            set last ""
            foreach id [lsort -integer [array names raw]] {
                if {$raw_kind($id) eq "assistant"} { set last $raw($id) }
            }
            if {$last eq ""} {
                ::vstatus::set_msg [::vi18n::mc {nothing to copy (no assistant reply yet)}]
            } else {
                clipboard clear
                clipboard append $last
                ::vstatus::set_msg [format [::vi18n::mc {last reply copied (%s chars)}] [string length $last]]
            }
        }
        "paste" {
            # UTF8_STRING FIRST (RM3 #9): a typeless `clipboard get`
            # asks for STRING, which GTK/Qt owners down-convert to
            # Latin-1 — truncating at the first non-convertible
            # character (the em-dash cut). Same read order absorb uses,
            # factored into one proc so they cannot drift.
            catch {
                set clip [::vemacs::clipboard_text]
                if {$clip ne ""} {
                    .mid.chatwrap.chat.input insert end $clip
                    ::vstatus::set_msg [::vi18n::mc {pasted from clipboard}]
                }
            }
        }
        "redraw" {
            rebuild
            ::vstatus::set_msg [::vi18n::mc {redrawn}]
        }
        "refetch" {
            # The store reread, distinct from /redraw on purpose:
            # /redraw repaints what is already in memory; /refetch
            # drops the transcript and re-fetches it from the store.
            # (Same path as C-S-V.)
            return [::vchat::refetch]
        }
        "verbose" {
            ::vstatus::set_msg [::vi18n::mc {verbose is always on}]
        }
        "reload" {
            ::vhotpatch::load
            ::vstatus::set_msg [::vi18n::mc {UI patches reloaded}]
        }
        "quit" - "exit" { exit }
        "agents" {
            # S7: defined subagents — read-only listing via the gateway.
            if {[catch {::vapi::get_json "/api/agents"} r]} {
                emit_message system [::vi18n::mc {agents unavailable (gateway offline?)}]
            } else {
                emit_message system [dict get $r listing]
            }
        }
        "mechanisms" - "mech" {
            # What the engine may do on its own, and how to stop it.
            # The engine notes in the transcript point here.
            if {$::vchat::session eq ""} {
                emit_message system [::vi18n::mc {no session open}]
                return
            }
            set body [dict create name [::vapi::json_string ""] value [::vapi::json_string ""]]
            if {$rest ne ""} {
                set parts [split [string trim $rest]]
                dict set body name [::vapi::json_string [lindex $parts 0]]
                if {[llength $parts] > 1} { dict set body value [::vapi::json_string [lindex $parts 1]] }
            }
            if {[catch {::vapi::post_json \
                "/api/sessions/$::vchat::session/mechanism" $body} e]} {
                emit_message system [::vi18n::mc {mechanisms failed: %s} $e]
            } elseif {[dict exists $e message]} {
                emit_message system [dict get $e message]
            } else {
                set overrides {}
                catch { set overrides [dict get $e overrides] }
                set lines {"mechanisms (what the engine may do on its own):"}
                foreach m [dict get $e mechanisms] {
                    set k [dict get $m key]
                    set state "config default"
                    foreach o $overrides {
                        if {[dict get $o key] eq $k} {
                            set state "[dict get $o value] (session override)"
                        }
                    }
                    lappend lines [format "  %s %-15s %-24s %s" \
                        [dict get $m glyph] $k $state [dict get $m help]]
                }
                lappend lines ""
                lappend lines "  /mechanisms <name> off|on   override for this session"
                lappend lines "  /mechanisms <name> default  drop the override"
                emit_message system [join $lines "\n"]
            }
        }
        "failover" {
            # S16: session failover mode — via the gateway API.
            _session_post failover "/api/sessions/$::vchat::session/failover" \
                [dict create mode $rest]
        }
        "plan" {
            # Plan mode — the desktop's /api/chat runs turns directly
            # (never through the gateway builtin dispatcher), so the
            # composer needs its own route. Backport 2026-08-29 review.
            _session_post "plan mode" "/api/sessions/$::vchat::session/plan" \
                [dict create mode $rest]
        }
        "body" {
            # Session extra_body override (PENDING 2026-08-29).
            _session_post extra_body "/api/sessions/$::vchat::session/body" \
                [dict create body $rest]
        }
        default {
            set sid [::vsidebar::selected_sid]
            # FIRST: the engine's own shared command set. The desktop
            # was the only surface that never called
            # `commands::dispatch`, so everything it supported it
            # reimplemented here — which is why its /help could
            # advertise /queue, /context and /limits and then answer
            # "unknown command: /queue". POST /api/command runs the
            # real one; `handled` false means the engine does not know
            # the name either, and we fall through to custom commands.
            if {$sid ne "" && [_try_shared_command $sid $cmd]} { return }
            # S6: custom commands — ask the gateway API to render the
            # template (args/shell/file inlines live in Rust), then send
            # the result as a normal turn.
            set render_err ""
            if {$sid eq ""} {
                set render_ok 0
            } else {
                set render_ok [expr {![catch {
                    set r [::vapi::post_json "/api/commands/render" \
                        [dict create name [::vapi::json_string $name] \
                                       args [::vapi::json_string $rest]]]
                    set prompt [dict get $r prompt]
                } render_err]}]
            }
            if {$render_ok} {
                if {$prompt ne ""} {
                    # Same anchoring send_current does: the optimistic user
                    # message goes in with its reload anchor set BEFORE it, so
                    # the turn-started event the gateway publishes does not
                    # re-emit the prompt on top of the copy already on screen
                    # (the "initial prompt printed twice" report — this path
                    # used to skip the anchor, and turn-started's guard keyed
                    # on exactly it).
                    dict set ::vchat::turn_reload_anchor $sid \
                        [[::vchat::log] index end-1c]
                    emit_message user $prompt
                    dict set ::vchat::turn_anchor $sid \
                        [[::vchat::log] index end-1c]
                    ::vchat::_mark_running $sid 1
                    ::vstatus::set_msg [format [::vi18n::mc {custom command /%s…}] $name]
                    if {[catch {::vapi::chat_send $sid $prompt} e]} {
                        emit_message system [::vi18n::mc {send failed: %s} $e]
                        turn_error "request failed: $e"
                    }
                } else {
                    emit_message system "[format [::vi18n::mc {unknown command: /%s}] $name]\n[::vi18n::mc {try /help}]"
                }
            } elseif {$render_err ne "" && ![string match "*404*" $render_err]} {
                # A transport/render failure is NOT "no such command" —
                # saying so sent people looking in the wrong place.
                emit_message system [format [::vi18n::mc {/%s failed: %s}] $name $render_err]
            } else {
                emit_message system "[format [::vi18n::mc {unknown command: /%s}] $name]\n[::vi18n::mc {try /help}]"
            }
        }
    }
}

proc ::vchat::stop_current {} {
    variable session
    # Stop the turn for the session ON SCREEN — via the EXPLICIT cancel
    # endpoint, which flips the token in the process-wide LIVE_TURNS the
    # turn registered itself in. NEVER by killing the event stream: the
    # socket dying must mean "the view went away", not "stop working"
    # (USER: "sessions die when desktop is closed/detached, instead of
    # continuing in the background"). The interrupted result then arrives
    # on the stream like any other turn's.
    if {[catch {::vapi::post_json \
        "/api/sessions/$session/cancel" [dict create]} r]} {
        if {[string match {*404*} $r]} {
            # No turn in flight per the engine — the engine is the truth,
            # so stop claiming busy locally instead of arguing with it.
            _mark_running $session 0
            log_insert "\n[::vi18n::mc {· nothing to stop (no turn in flight)}]\n" system
        } else {
            log_insert "\n[format [::vi18n::mc {✖ stop failed: %s}] $r]\n" system
        }
        ::vstatus::set_msg [format [::vi18n::mc {stop: %s}] $r]
        return
    }
    if {[dict exists $r error]} {
        ::vstatus::set_msg [format [::vi18n::mc {stop: %s}] [dict get $r error]]
        return
    }
    # In the TRANSCRIPT, not only the status bar: a Stop press that looks
    # like nothing happened is how "it won't stop" gets reported for what
    # is really "it said nothing".
    log_insert "\n[::vi18n::mc {⏹ stop requested}]\n" system
    ::vstatus::set_msg [::vi18n::mc {stop requested…}]
}

proc ::vchat::detach_current {} {
    variable session
    # Move the running turn's foreground command into the background
    # shells — /cancel's gentle sibling. Explicit endpoint, same reason
    # Stop has one: the turn holding the agent lock is the very turn
    # blocking on the command, so this must NOT ride the chat path.
    if {$session eq ""} return
    if {[catch {::vapi::post_json \
        "/api/sessions/$session/detach" [dict create]} r]} {
        if {[string match {*409*} $r]} {
            log_insert "\n[::vi18n::mc {· nothing to detach (the turn is not inside a foreground command)}]\n" system
        } elseif {[string match {*404*} $r]} {
            _mark_running $session 0
            log_insert "\n[::vi18n::mc {· nothing to detach (no turn in flight)}]\n" system
        } else {
            log_insert "\n[format [::vi18n::mc {✖ detach failed: %s}] $r]\n" system
        }
        ::vstatus::set_msg [format [::vi18n::mc {detach: %s}] $r]
        return
    }
    if {[dict exists $r error]} {
        ::vstatus::set_msg [format [::vi18n::mc {detach: %s}] [dict get $r error]]
        return
    }
    # In the TRANSCRIPT, same rule as Stop: a press that looks like
    # nothing happened is how a working feature gets reported broken.
    log_insert "\n[::vi18n::mc {⤵ detach requested — the command keeps running as a background shell}]\n" system
    ::vstatus::set_msg [::vi18n::mc {detach requested…}]
}

# Stop every running turn — used on quit. This DOES cancel server-side:
# quitting the app is an explicit act, not a disconnect. (The window
# going away without this call is precisely the case that must NOT
# cancel.)
proc ::vchat::stop_all {} {
    variable running
    # Cancel every turn the client knows is running, keyed off the
    # client-side `running` dict — NOT the open streams, because there is
    # now ONE event stream (the session on screen) while a turn can be in
    # flight in any session this window started or watched.
    foreach sid [dict keys $running] {
        if {[catch {::vapi::post_json \
            "/api/sessions/$sid/cancel" [dict create]} e]} {
            # Say it: a turn that could not be told to stop is still
            # running, and quitting must not look like it stopped it.
            puts stderr "veles-desktop: cancel $sid failed: $e"
        }
    }
    _events_disconnect_all
    ::vstatus::set_msg [::vi18n::mc {stopping all turns…}]
}

# ── whose event is this? ─────────────────────────────────────────────
#
# A turn keeps running when you switch away from its session — that is
# the point of switching. So every callback has to answer two separate
# questions:
#
#   which session does this belong to      → event_session
#   is that the transcript on screen       → for_display
#
# Bookkeeping (busy flags, button states) follows the first. Rendering
# follows the second. Conflating them is what made switching mid-turn
# unsafe, and the old code answered it by refusing to switch at all.

proc ::vchat::event_session {} {
    variable stream_session
    variable session
    if {$stream_session ne ""} { return $stream_session }
    return $session
}

# WHAT for_display GATES, now that every session owns a widget AND its
# own live region.
#
# It used to mean "may this event paint at all", because there was one
# transcript and an off-screen session's output had nowhere to go. It
# means only "is this the session whose WINDOW CHROME this event may
# touch" — the status line, the Stop button, the follow button, the
# route label. Two call sites, both chrome.
#
# IT NO LONGER GATES RENDERING AT ALL. It used to gate the live region
# as well, on the argument that nobody watches a hidden transcript
# render token by token and that the accumulators are one set. The
# argument rested on two claims and both were false: the finished
# message did NOT arrive complete at turn end (`on_done` read
# the accumulator only when on screen, so a hidden session rebuilt from one
# display_reasoning and lost every earlier round), and the re-handshake
# on switching back did NOT recover what was missed (the engine's
# snapshot cleared its reasoning on every ToolEnd). What the user got
# was: "switch away mid-turn, switch back, most of the newest thinking
# blocks are missing" — 2026-09-16, with that repro.
#
# The accumulators are still one set; they are SWAPPED per session by
# `_live_ctx` instead of being reserved for whoever is visible. So a
# hidden session streams into its own widget, its segments are intact
# when its turn ends, and switching back is a `pack` — no handshake, no
# snapshot, nothing deleted and repainted.
#
# Everything PERSISTED — notes, errors, the finished reply, shell
# completions, replayed history — has always painted into its own
# session's transcript whether or not you are looking at it.
proc ::vchat::for_display {} {
    variable stream_session
    variable session
    if {$stream_session eq ""} { return 1 }
    return [expr {$stream_session eq $session}]
}

# Mark a session as having a turn in flight, and reflect it in the UI if
# that session is the one on screen.
#
# `repaint` is 0 for the ONE caller that is already inside the pane's
# rebuild (`sync_running`, below): refreshing the sidebar from there
# would re-enter the refresh that called it, mid-listbox.
proc ::vchat::_mark_running {sid on {repaint 1}} {
    variable running
    variable busy
    variable session
    variable running_since
    if {$on} {
        dict set running $sid 1
        # WHEN this window came to believe it, in ms. A mark made here
        # is this window's own — usually optimistic, since send_current
        # marks before the POST goes out — and `sync_running` gives it a
        # grace window before the engine's silence is allowed to
        # contradict it. An engine-CONFIRMED mark stamps 0, i.e. no
        # grace left to spend.
        set running_since($sid) [clock milliseconds]
    } else {
        dict unset running $sid
        catch { unset running_since($sid) }
    }
    if {$sid eq $session} {
        set busy [expr {$on ? 1 : 0}]
        _sync_buttons
    }
    # Scheduled, never fetched here: this runs inside event-stream
    # handlers (see ::vsidebar::refresh_soon for the hang it caused).
    if {$repaint} { catch { ::vsidebar::refresh_soon } }
}

# How long this window's own "running" survives an engine that does not
# (yet) agree. One round trip is plenty; the engine's own reaper waits
# 30s, so nothing here can outlast it.
namespace eval ::vchat { variable running_grace_ms 10000 }

# Reconcile which sessions this window thinks are working with the
# engine's answer, from the session listing the sidebar just fetched.
#
# USER 2026-09-17: "the session that was attached … is still showing as
# active even though it's actually ended a long time ago".
#
# `running` was set and cleared purely by stream events, so a turn whose
# end this window did not witness — stream never opened for it, evicted,
# dropped, or outlived by a gateway restart — kept its ▶ forever with no
# path back. The one self-healing arm (`session-live`) only runs when a
# stream is (re)opened, which a view switch deliberately no longer does.
#
# /api/sessions has carried the answer all along: `running`, from the
# cross-process session_runs registry, heartbeated by the process that
# owns the turn and reaped by any reader after 30s of silence. So it is
# authoritative in BOTH directions — it also adopts a turn started in
# the tui, telegram or cron, which this window had no way to hear about
# and showed as idle.
#
# Two things it refuses to do:
#   * a listing with no `running` key at all (an older gateway) says
#     NOTHING — absence is not "stopped", the same rule the
#     history-changed arm states for liveness generally;
#   * a mark younger than `running_grace_ms` is left alone. send_current
#     marks running BEFORE the POST and _mark_running refreshes the
#     pane, so without this the very refresh a send triggers would clear
#     the flag it just set and flicker Send/Steer on every message.
proc ::vchat::sync_running {sessions} {
    variable running
    variable running_since
    variable running_grace_ms
    set now [clock milliseconds]
    foreach s $sessions {
        if {[catch { dict get $s id } sid] || $sid eq ""} continue
        if {[catch { dict get $s running } live]} continue
        if {![string is boolean -strict $live]} continue
        if {$live} {
            if {![dict exists $running $sid]} { _mark_running $sid 1 0 }
            # Confirmed by the engine: no grace left to spend, so the
            # next `false` is obeyed immediately.
            set running_since($sid) 0
            continue
        }
        if {![dict exists $running $sid]} continue
        set since 0
        if {[info exists running_since($sid)]} { set since $running_since($sid) }
        if {$since != 0 && $now - $since < $running_grace_ms} { continue }
        _mark_running $sid 0 0
    }
}

proc ::vchat::_sync_buttons {} {
    variable busy
    # Send NEVER disables: while a turn runs it becomes STEER (the
    # composer's send path queues the text into the running turn). A
    # greyed-out composer mid-turn is exactly what "can't steer from
    # the desktop" was.
    #
    # THE CAPTION IS REGISTERED PER BRANCH, not computed by a ternary
    # (USER report 2026-09-26: "the steer button is untranslated"). A
    # quoted string inside [expr] is invisible to the key extractor —
    # there is no brace group at an ::vi18n::mc call, and no call at
    # all — so "Steer" sat in no catalog and the button said Steer in
    # every language. `Send` was already in the catalog (its create site
    # wraps it), which is why only half the pair showed the bug:
    # whichever word the extractor could not see stayed English.
    #
    # Written as two branches rather than `[expr {$busy ? {Steer} :
    # {Send}}]` so the key is a literal brace group at the call — the
    # shape every scan in this tree can see. Registration also needs a
    # single key per call, which a ternary cannot express.
    # Each operation in its own catch: a label-write failure must not
    # stop the button-state writes (a dead Stop during a live turn is
    # worse than an untranslated label).
    if {$busy} {
        catch { ::vi18n::label .mid.chatwrap.chat.bar.send {Steer} }
    } else {
        catch { ::vi18n::label .mid.chatwrap.chat.bar.send {Send} }
    }
    catch {
        .mid.chatwrap.chat.bar.stop configure -state [expr {$busy ? "normal" : "disabled"}]
    }
    # Detach arms whenever Stop does: whether the turn is INSIDE a
    # foreground terminal call right now is engine state the client
    # cannot see, and the endpoint answers honestly when it is not
    # (409 → "not inside a foreground command"). A dead-looking
    # button during the exact window it exists for is worse than a
    # polite refusal a few hundred ms later.
    catch {
        .mid.chatwrap.chat.bar.detach configure -state [expr {$busy ? "normal" : "disabled"}]
    }
}

proc ::vchat::is_running {sid} {
    variable running
    return [dict exists $running $sid]
}

# ── SSE callbacks (called by ::vapi) ─────────────────────────────────
proc ::vchat::on_route {provider {model ""} {effort ""}} {
    variable route
    if {![for_display]} { return }
    set route [expr {$model ne "" ? "$provider/$model" : $provider}]
    # Effort level rides next to provider/model: "p/m (effort high)". The
    # reasoning_effort field is what the route will actually send — the
    # config default / provider pinned / session override already merged.
    if {$effort ne "" && $effort ne "null"} {
        append route " (effort $effort)"
    }
    # The whole chain, in the label that survives the status poll.
    ::vstatus::set_route $route
    ::vstatus::set_msg [format [::vi18n::mc {thinking… (%s)}] $route]
}

# What this session will actually send, shown BEFORE the first prompt.
#
# USER: "it's hard to see what the model selected is before first
# prompt". GET /api/sessions/:id/override reports the effective route
# (and whether it is a session override), so there is no need to wait
# for a turn to find out.
proc ::vchat::refresh_route {{sid ""}} {
    variable session
    if {$sid eq ""} { set sid $session }
    if {$sid eq ""} { ::vstatus::set_route "" ; return }
    if {[catch { ::vapi::get_json "/api/sessions/$sid/override" } ov]} {
        ::vstatus::set_route ""
        return
    }
    if {[dict exists $ov live] && [dict get $ov live]} {
        set prov ""
        set mod ""
        catch { set prov [dict get $ov provider] }
        catch { set mod  [dict get $ov model] }
        set ovr 0
        catch { set ovr [dict get $ov overridden] }
        if {$prov ne "" || $mod ne ""} {
            ::vstatus::set_route "$prov/$mod" $ovr
            return
        }
    }
    # Not started yet: report the parked choice if there is one, so a
    # pending override is visible rather than silently waiting. BOTH
    # halves are painted — the pending row carries the provider as well
    # as the model, and a label of "glm-5.2 (pending)" makes the quota
    # field read the WRONG plan's pool (route_prov splits at the first
    # "/", and no slash means no provider — the session sits on z.ai
    # while the bar shows personal's number). Found live, 2026-09-15.
    if {[dict exists $ov pending]} {
        set pend [dict get $ov pending]
        if {$pend ne "" && $pend ne "null" && [catch {dict size $pend}] == 0} {
            set pm ""
            catch { set pm [dict get $pend model] }
            if {$pm ne "" && $pm ne "null"} {
                set pp ""
                catch { set pp [dict get $pend provider] }
                if {$pp ne "" && $pp ne "null"} {
                    ::vstatus::set_route "$pp/$pm (pending)" 1
                } else {
                    ::vstatus::set_route "$pm (pending)" 1
                }
                return
            }
        }
    }
    # Nothing session-specific: fall back to the CONFIGURED DEFAULT, so
    # a brand-new session still says what it is about to use. Showing
    # nothing until the first turn is exactly the complaint.
    if {![catch { ::vapi::get_json /api/config } cfg]} {
        set dp ""
        set dm ""
        catch { set dp [dict get $cfg default_provider] }
        catch { set dm [dict get $cfg default_model] }
        if {$dp ne "" || $dm ne ""} {
            ::vstatus::set_route "$dp/$dm" 0
            return
        }
    }
    ::vstatus::set_route ""
    return
}

# A live stream arrives as raw chunks with NO separators between
# kinds: reasoning runs straight into the first delta, and a tool
# line welds onto whatever came before. The turn-end re-render always
# had proper line breaks, which is exactly why the live view looked
# worse than a reopened one. Compute the break for the kind
# transition — the CALLER inserts it inside its own
# prestash→insert→follow bracket.
#
# This proc is PURE about the widget's content (it may READ the tail,
# never write): it used to prestash and insert its own "\n", and the
# caller's prestash then OVERWROTE that good at-bottom stash with a
# post-insert read — one unbracketed newline disarmed follow for the
# rest of the turn, the stream arrived below a view that never came
# back down (R6; USER: "follow stops on a tool call when /reasoning
# is on"). uitest pins this mechanically: no insert, no prestash, no
# follow may appear in this body.
proc ::vchat::_live_break {kind {w ""}} {
    variable live_last_kind
    variable live_rtag
    variable live_rhdr
    variable live_ctx
    if {$w eq ""} { set w [::vchat::log_for $live_ctx] }
    if {$w eq "" || ![winfo exists $w]} { return "" }
    set prev $live_last_kind
    set live_last_kind $kind
    if {$prev eq "" || $prev eq $kind} { return "" }
    # Leaving reasoning CLOSES the live region: each segment of a
    # multi-round turn mints its own region + header on the way back
    # in, so live reasoning groups one-block-per-segment exactly like
    # a reopen (R7). The closed region's tags stay configured on
    # screen; on_done's glob and forget_raw reap them.
    #
    # A FOREIGN kind (status/note — a background completion, an
    # engine note) does NOT close it (2026-09-20: "the reasoning gets
    # split into two blocks when it is clearly part of the same
    # stream"). The wire sends one contiguous reasoning run per call;
    # a construct painted between two SSE frames of that run is
    # foreign to it, and clearing the tags here reopened the thought
    # under a SECOND "reasoning (streaming)" header when its next
    # frame arrived. The STREAM's own transitions (delta, tool) still
    # close it — those really are the end of the row's thought.
    if {$prev eq "reasoning" && $kind ni {reasoning status note}} {
        set live_rtag ""
        set live_rhdr ""
    }
    if {[$w compare end-1c == 1.0]} { return "" }
    set tail [$w get end-2c end-1c]
    if {$tail eq "\n"} { return "" }
    return "\n"
}

# Close whatever live region `sid` has open, and return the kind-break
# for the block about to be painted in its place.
#
# A construct that is NOT part of the stream — an engine note, a
# finished background child/shell/monitor — has three things to do
# before it inserts, and shipping two of them is how this went wrong
# twice:
#
#   * load the EVENT's session context (_live_ctx): the accumulators
#     are per-session, and a completion for a hidden session must not
#     read the visible one's;
#   * flush and CLOSE both live regions. `log_insert` appends at `end`,
#     which is past `live_delta_mark`, and the next debounce tick opens
#     with `$w delete $live_delta_mark end` — so the line was erased
#     ~200ms after it was drawn, and again by on_done's
#     `delete $anchor end`. A pending REASONING tail is worse than
#     erased: it is painted AFTERWARDS and lands below the foreign line,
#     so the line teleports above the thinking it actually arrived
#     after;
#   * record the kind transition. Without it `live_last_kind` still
#     names the stream's last kind, and the NEXT block asks for a
#     separator from a comparison that comes out equal and gets none —
#     the welded "no newline between the end of reasoning and start of
#     tool call".
#
# USER 2026-09-18 reported all three symptoms as one intermittent
# garble; intermittent because it takes a background unit finishing
# DURING a turn. `on_note` had the first two and not the third;
# shell-complete, child-complete and monitor-fired (c0c12c6, "same
# shape as shell-complete" — which it was, defect included) had none.
# ONE proc, so a fourth caller cannot arrive with a subset.
proc ::vchat::_close_live_for {sid {kind status}} {
    _live_ctx $sid
    _flush_live_reasoning $sid
    if {[catch { _flush_live_delta $sid 1 } flush_err]} {
        puts stderr "veles-desktop: _close_live_for flush failed for $sid: $flush_err"
    }
    return [_live_break $kind [::vchat::log_for $sid]]
}

# One background unit's completion line. Closes the live region (see
# above), paints, and — when a turn is in flight — REMEMBERS the line,
# because on_done is about to delete the span it just landed in.
#
# The reconcile puts it back after the finished turn, which is also
# where a reload shows it: the engine's [BACKGROUND … COMPLETE] row is
# injected at the turn BOUNDARY, not inside the turn. So live and
# reload agree about position, and the live paint keeps its point — you
# see it the moment it happens, and it is still there afterwards.
proc ::vchat::_log_completion {sid text} {
    variable turn_anchor
    variable live_extra
    set brk [_close_live_for $sid]
    log_insert "${brk}${text}" status $sid ;# i18n-exempt: separator glyph + row text, no words
    if {[dict exists $turn_anchor $sid]} {
        dict lappend live_extra $sid $text
    }
}

# ── the live region belongs to a SESSION, not to the window ──────────
#
# September 15 made the STREAMS per-session; the region they paint into
# stayed single, and `for_display` dropped every delta and every
# reasoning chunk belonging to a session that was not on screen. The
# design leaned on two claims to make that safe, and neither held:
#
#   * "the finished message lands in the hidden session's own widget at
#     turn end, complete with its reasoning" — `on_done` takes
#     the accumulator only when on screen, so a session that was hidden has
#     NO segments and falls back to the done event's single
#     display_reasoning: the last round's. Every earlier round's
#     thinking was gone from the view.
#   * "switching TO a busy session re-handshakes so the engine's
#     snapshot paints whatever streamed while you were away" — the
#     engine's snapshot cleared its reasoning on every ToolEnd, so it
#     could only ever hand back the last round.
#
# USER 2026-09-16, with the repro: "open session -> switch to different
# session while turn is running -> switch back = most of the newest
# thinking blocks are missing."
#
# So the accumulators become per-session. Rather than rewrite the ~300
# lines that read them, the ONE set is SWAPPED: an event for session X
# stashes whoever held the scalars and loads X's. Same invariant the
# code already relies on (one open region at a time), now scoped to the
# session the events belong to instead of to whichever transcript
# happens to be visible.
#
# Every entry point that touches the region calls `_live_ctx` with the
# event's session first — including the debounce timers, which fire
# long after the context that armed them may have moved on.
proc ::vchat::_live_ctx {sid} {
    variable live_ctx
    variable live_saved
    if {$sid eq "" || $sid eq $live_ctx} { return }
    if {$live_ctx ne ""} { dict set live_saved $live_ctx [_live_snapshot] }
    if {[dict exists $live_saved $sid]} {
        _live_restore [dict get $live_saved $sid]
    } else {
        _live_restore [_live_blank]
    }
    set live_ctx $sid
}

# The scalars that make up one session's live region. Listed ONCE:
# snapshot, restore and blank all read this, so a variable added to the
# region cannot be added to two of the three and forgotten in the last.
proc ::vchat::_live_vars {} {
    return {live_rtag live_rhdr live_rtext live_rpend live_r_after
            live_last_kind live_blocks live_ptext live_delta_mark live_delta_buf
            live_delta_after live_delta_period live_delta_sid}
}

proc ::vchat::_live_snapshot {} {
    set d {}
    foreach v [_live_vars] {
        dict set d $v [set ::vchat::$v]
    }
    return $d
}

proc ::vchat::_live_restore {d} {
    foreach v [_live_vars] {
        set ::vchat::$v [expr {[dict exists $d $v] ? [dict get $d $v] : ""}]
    }
    # Two of them are not strings-that-default-to-empty.
    if {![dict exists $d live_blocks]}       { set ::vchat::live_blocks {} }
    if {![dict exists $d live_delta_period]} { set ::vchat::live_delta_period 200 }
}

proc ::vchat::_live_blank {} {
    return [dict create live_blocks {} live_delta_period 200]
}

# Drop a session's stashed live region — detach, eviction, forget_raw.
proc ::vchat::_live_forget {sid} {
    variable live_ctx
    variable live_saved
    variable live_extra
    variable live_tool_marks
    catch { dict unset live_saved $sid }
    catch { dict unset live_extra $sid }
    # A session's live tool-round marks die with its region — the widget
    # they point into is going away (or the session is), and a mark left
    # in the dict would be read by the next reconcile as the CURRENT
    # session's and delete a span it never painted.
    catch { dict unset live_tool_marks $sid }
    if {$live_ctx eq $sid} {
        _live_restore [_live_blank]
        set live_ctx ""
    }
}


# ── the live accumulator: the ROW SEQUENCE this turn will persist ────
#
# `on_done` deletes the streamed region and rebuilds the turn, so what
# it rebuilds FROM decides what the finished transcript contains. The
# old accumulator was a list of {reason rounds} SEGMENTS delimited by
# reasoning, and that had two consequences:
#
#   * no slot for the model's own prose, so a turn that narrated
#     between tool calls lost every word of it at turn end while a
#     reload brought it back — the finished live transcript was missing
#     text the model produced (USER 2026-09-18: "Intermediate prose
#     missing is a failure mode, not an acceptable state");
#   * the segment was the wrong UNIT, so a third slot could not have
#     fixed it. Segments open on REASONING, so a round that thinks
#     nothing opens none and its prose and its call fold into the
#     previous segment: `r1 · p1p2 · t1 · t2` for a turn that actually
#     went `r1 · p1 · t1 · p2 · t2`. The container had nowhere to put
#     the interleaving. Rounds with no reasoning are not a corner case
#     — they are the commonest row shape in the store.
#
# So the unit is the ROW. The engine persists one assistant row per
# provider call and one row per tool result, and `_paint_one_row` — the
# reload ladder this must equal (R3) — is exactly two emit shapes. This
# list is that sequence, in arrival order:
#
#   {call <reason> <prose> <at>}          one provider call's assistant row
#   {tool <name> <args> <result> <at>}    one tool result row
#   {note <dict> <raw id>}                an engine note, where it arrived
#
# <at> is when the call or the round STARTED (clock seconds), so the
# reconcile paints the same stamp the live view showed.
#
# A tool round is what ENDS a call, which is what ends an assistant row
# on the wire. "Is a call open" is read off the list itself, so there is
# no second piece of state for _live_restore to forget.

# The call entry reasoning and prose attach to, minting one when the
# last entry is a tool (or there is nothing yet). Returns its index.
proc ::vchat::_live_call_slot {} {
    variable live_blocks
    if {![llength $live_blocks] || [lindex $live_blocks end 0] ne "call"} {
        lappend live_blocks [list call "" "" [clock seconds]]
    }
    return [expr {[llength $live_blocks] - 1}]
}

# APPEND, both of them. One provider call carries ONE `reasoning` field
# and ONE `content` field in the row it becomes, however many bursts of
# each arrived on the wire — a model that thinks, speaks, then thinks
# again inside one call is still one row, and a reopen shows one
# collapsible. The live view still streams a fresh region per burst;
# that is the streaming view, and this is what it reconciles to.
proc ::vchat::_live_block_reason {chunk} {
    variable live_blocks
    set i [_live_call_slot]
    lset live_blocks $i 1 "[lindex $live_blocks $i 1]$chunk"
}

# PER TOKEN, so it must stay O(1). on_delta runs once per SSE frame —
# measured at 732 frames averaging 23 bytes for a single turn — and the
# first cut of this rebuilt the call's whole prose string on every one
# of them, which is the same quadratic streamtest was written to kill
# ("an O(n) copy of the whole accumulated reasoning into live_segs …
# per token"). Those meters count WIDGET WRITES and this adds none, so
# they would never have seen it. A plain append; the fold happens at
# the boundaries below.
proc ::vchat::_live_block_prose {chunk} {
    variable live_ptext
    append live_ptext $chunk
}

# Fold the accumulated prose into the open call. Called exactly where a
# call can CLOSE or be READ — a tool round, and the reconcile — so the
# O(n) copy is paid once per round instead of once per token.
proc ::vchat::_live_block_settle {} {
    variable live_blocks
    variable live_ptext
    if {$live_ptext eq ""} { return }
    set i [_live_call_slot]
    lset live_blocks $i 2 "[lindex $live_blocks $i 2]$live_ptext"
    set live_ptext ""
}

# One tool RESULT, one entry — the reload ladder emits one block per
# row, and the old accumulator collected a segment's rounds into one
# list that went out as a single block. `args_`, not `args`: a proc
# parameter called `args` is variadic in Tcl.
proc ::vchat::_live_block_round {name args_ result {at ""}} {
    variable live_blocks
    _live_block_settle
    lappend live_blocks [list tool $name $args_ $result $at]
}

# One engine note (a guard, a compaction, a cache miss…), at the point it
# arrived: the prose before it is folded into its call first, so the note
# sits between what came before and what came after.
#
# USER 2026-09-29: "all the guards are dumped at the end of the agent
# turn - those ideally should only be shown inline when they happen."
# They WERE painted inline — and the reconcile at turn end deleted the
# whole live region and rebuilt it from this list, which held calls and
# tool rounds only. The notes went with the delete; the next rebuild
# (a theme change, a view toggle) painted them from `raw`, bunched
# together, ahead of the turn they belonged to.
proc ::vchat::_live_block_note {d id} {
    variable live_blocks
    _live_block_settle
    lappend live_blocks [list note $d $id]
}

# Drop one painted entry from the ordered store — the note the reconcile
# is about to paint again in its place, so a rebuild does not show it
# twice. forget_raw's per-id cleanup, for one id.
proc ::vchat::_forget_raw_entry {id {w ""}} {
    if {$w eq ""} { set w [::vchat::log] }
    catch { $w mark unset msgstart_$id msgend_$id }
    foreach a {raw raw_kind raw_src raw_reason raw_tools raw_note raw_w raw_at raw_msgid} {
        catch { unset ::vchat::${a}($id) }
    }
}

proc ::vchat::on_reasoning {text} {
    # Live reasoning streams into a dedicated tag so it can collapse.
    variable live_rtag
    variable live_rhdr
    variable live_rtext
    variable show_reasoning
    variable live_rpend
    variable live_r_after
    variable live_delta_period
    # AN EMPTY FRAME IS NOT A BREAK. Some providers send a zero-length
    # reasoning frame as a heartbeat or an explicit "end of segment"
    # marker rather than omitting the event entirely; treating it as a
    # construct closed the region and the next non-empty frame opened a
    # SECOND header (2026-09-20 round 2, same family as the foreign-kind
    # fix in _live_break: one contiguous thought must stay one block).
    if {$text eq ""} { return }
    # A session's reasoning lands in ITS OWN transcript, on screen or
    # not. The old gate here was `if {![for_display]} { return }` — a
    # hidden session's thinking was dropped on the floor, which is the
    # whole of the switch-away/switch-back report.
    set sid [event_session]
    _live_ctx $sid
    set w [::vchat::log_for $sid]
    if {$w eq "" || ![winfo exists $w]} { return }
    # A non-delta construct CLOSES the open delta region: render it through
    # markdown now, so reasoning never welds onto raw deltas and the next
    # flush does not delete it (the anchor defect that garbled the first
    # attempt).
    if {[catch { ::vchat::_flush_live_delta [::vchat::event_session] 1 } flush_err]} {
        puts stderr "veles-desktop: on_reasoning flush failed: $flush_err"
    }
    set brk [_live_break reasoning]
    if {![info exists live_rtag] || $live_rtag eq ""} {
        # Pre-stash BEFORE the header insert. It changes the layout under
        # the view (a new line), and the after-insert yview read is
        # exactly the lie that un-follows mid-turn (see log_insert). The
        # kind-break must ride in the SAME bracket — that is the whole
        # point of _live_break being pure now.
        _at_bottom_prestash $w
        set clk [clock clicks]
        set live_rtag "live_r_$clk"
        set live_rhdr "live_r_hdr_$clk"
        # A new VISUAL region opens here (the previous one closed when
        # the kind changed away from reasoning — see _live_break). The
        # accumulator is no longer keyed off it: a burst of thinking
        # after a delta is still the same provider CALL, so the row it
        # becomes has one reasoning field and _live_block_reason appends
        # into it. What opens a new call entry is a TOOL ROUND.
        set live_rtext ""
        # A header that OUTLIVES the body's elide. "reasoning" off folds
        # the body away but leaves this ▶ line to click back open — exactly
        # like a completed reasoning collapsible. Without it, hiding the
        # live thinking made it VANISH with nothing left to click (USER:
        # "unchecking the verbose thinking it just disappears instead of
        # collapsing"). Rendered before the body so it sits above it.
        _writable $w {
            _run $w $brk $live_rhdr
            _live_call_stamp $w
            _run $w "[_live_reason_hdr]\n" $live_rhdr
        }
        $w tag configure $live_rhdr -foreground $::vt::T(reasoning) \
            -font {TkDefaultFont 10 italic} -elide 0
        ::vtext::click $w $live_rhdr {::vchat::_click_live_reasoning}
        $w tag bind $live_rhdr <Enter> {%W configure -cursor hand2}
        $w tag bind $live_rhdr <Leave> {%W configure -cursor xterm}
        $w tag configure $live_rtag -foreground $::vt::T(reasoning) \
            -font {TkDefaultFont 10 italic} -elide [expr {!$show_reasoning}]
        _maybe_follow $w
    }
    # Per CHUNK this is now one string append and nothing else. It used
    # to be a yview read, a widget insert, an O(n) copy of the whole
    # accumulated reasoning into the block list, and a `see end` that forces a
    # scroll — per token, with no pacing of any kind.
    #
    # That was affordable while tokens trickled. It is not affordable
    # under a burst, and the burst reaches the desktop as one frame per
    # token whatever the gateway does: on loopback the kernel send buffer
    # swallows the whole response, so there is no application-level
    # backpressure for merge-on-drain to fold. Measured on the live
    # gateway — a 1200-word v4.1-flash turn read at 1KB/s still arrived
    # as 732 reasoning frames averaging 23 bytes.
    append live_rpend $text
    if {$live_r_after eq ""} {
        set live_r_after [after $live_delta_period [list ::vchat::_flush_live_reasoning $sid]]
    }
}

# Paint the reasoning accumulated since the last tick.
#
# The insert is append-only, so this is about the things AROUND it: one
# yview, one `see end` and one segment-accumulator copy per tick instead
# of per token. Called on the paced timer and, crucially, by every
# construct that closes the reasoning segment — a pending tail would
# otherwise be dropped when _live_break clears live_rtag on the way out.
proc ::vchat::_flush_live_reasoning {{sid ""}} {
    variable live_rpend
    variable live_r_after
    variable live_rtext
    variable live_rtag
    variable live_delta_period
    # The timer that armed this may have been armed under a different
    # session's context; load the one it belongs to before touching a
    # single accumulator. Called with no sid only by callers that have
    # just set the context themselves.
    if {$sid ne ""} { _live_ctx $sid }
    if {$live_r_after ne ""} {
        catch { after cancel $live_r_after }
        set live_r_after ""
    }
    if {$live_rpend eq ""} { return }
    # No open segment to paint into: the region was reset under us (a
    # session switch, a cleared transcript). Drop the text rather than
    # painting it into somebody else's transcript with a dead tag.
    if {![info exists live_rtag] || $live_rtag eq ""} {
        set live_rpend ""
        return
    }
    variable live_ctx
    set w [::vchat::log_for $live_ctx]
    if {$w eq "" || ![winfo exists $w]} { return }
    set t0 [clock milliseconds]
    set chunk $live_rpend
    set live_rpend ""
    _at_bottom_prestash $w
    _writable $w {
        _run $w $chunk $live_rtag
    }
    append live_rtext $chunk
    # The CHUNK, not the region's running total: live_rtext resets with
    # every new region, and the call this belongs to may span several.
    _live_block_reason $chunk
    _maybe_follow $w
    set live_delta_period [_next_flush_period [expr {[clock milliseconds] - $t0}]]
}

# Drop any pending reasoning and its timer. For the paths that abandon
# the live region wholesale (session switch, transcript clear), where
# painting the tail would put it in the wrong transcript.
proc ::vchat::_reset_live_reasoning {{sid ""}} {
    variable live_rpend
    variable live_r_after
    if {$sid ne ""} { _live_ctx $sid }
    if {$live_r_after ne ""} { catch { after cancel $live_r_after } }
    set live_r_after ""
    set live_rpend ""
}

# CLOSE the reasoning region without dropping a single character: flush
# the pending tail, then clear the tags so the NEXT burst (this row is
# over — a tool call took it) opens a fresh region with its own header,
# rather than welding onto a region whose identity is already spent.
# The flush half runs on every kind transition anyway; this is the one
# spelling of "the thought is over", for callers that know it is.
proc ::vchat::_close_live_reasoning {{sid ""}} {
    variable live_rtag
    variable live_rhdr
    variable live_last_kind
    _flush_live_reasoning $sid
    set live_rtag ""
    set live_rhdr ""
    # A region closed this way is followed by a non-reasoning construct
    # (the caller paints it); the kind recorder must agree, or the next
    # reasoning burst asks _live_break for a separator against a stale
    # "reasoning" and gets one welded the wrong side.
    set live_last_kind "tool"
}

# The live reasoning header line, glyph + hint reflecting show_reasoning.
# One shape, used both when the block is first painted and when the toggle
# rewrites it, so the two can never disagree.
proc ::vchat::_live_reason_hdr {} {
    variable show_reasoning
    return [expr {$show_reasoning
        ? "▼ reasoning (streaming — click to hide)"
        : "▶ reasoning (streaming — click to show)"}]
}

# Clicking the live header toggles the SAME show_reasoning the checkbox
# drives (and the checkbox -variable tracks it back), so the header, the
# body elide, and the toolbar control are one state.
proc ::vchat::_click_live_reasoning {} {
    variable show_reasoning
    set show_reasoning [expr {!$show_reasoning}]
    _toggle_live_reasoning
}

proc ::vchat::on_delta {text} {
    variable live_delta_mark
    variable live_delta_buf
    variable live_delta_after
    variable live_delta_period
    variable live_delta_sid
    # AN EMPTY FRAME IS NOT A BREAK (the on_reasoning note, same day:
    # providers that emit zero-length frames as heartbeats must not
    # close regions or burn a kind transition). Nothing to paint, and
    # nothing to break.
    if {$text eq ""} { return }
    # ITS OWN transcript, on screen or not (see _live_ctx). The gate
    # that used to stand here dropped a hidden session's answer text
    # exactly as it dropped its reasoning.
    set sid [event_session]
    _live_ctx $sid
    set w [::vchat::log_for $sid]
    if {$w eq "" || ![winfo exists $w]} { return }
    # Paint any pending reasoning FIRST. _live_break is about to clear
    # live_rtag, and a pending tail with no tag to land in is a tail
    # dropped on the floor.
    _flush_live_reasoning $sid
    set brk [_live_break delta]
    # A NEW stream (or the first chunk of this one) opens the region: the
    # mark sits where the raw text lands, AFTER the break. The break is
    # inserted and the mark set in ONE bracket — one insert, one
    # prestash, one follow (R6).
    set first [expr {$live_delta_sid ne $sid || $live_delta_mark eq ""}]
    if {$first} {
        # A region left over from a previous turn (or a session switch)
        # is stale: cancel its debounce and clear it before starting fresh.
        _reset_live_delta
        set live_delta_sid $sid
    }
    _at_bottom_prestash $w
    _writable $w {
        if {$first} {
            _run $w "$brk" assistant
            # Before the mark: the stamp sits outside the region the
            # debounced re-render replaces.
            _live_call_stamp $w
            set live_delta_mark "live_delta_[clock clicks]"
            # Anchor at the character the delta text will land BEHIND
            # (end-1c), not at `end`: `end` is the phantom position past
            # the widget's implicit trailing newline, so a left-gravity
            # mark set there ends up AFTER the delta text, and
            # `delete $mark end` deletes an empty range — the raw run
            # stayed and every re-render piled a fresh copy on top (the
            # duplicated-assistant garble).
            $w mark set $live_delta_mark end-1c
            $w mark gravity $live_delta_mark left
        }
        _run $w $text assistant
    }
    append live_delta_buf $text
    # WHAT THE MODEL SAID, kept for the reconcile. live_delta_buf cannot
    # serve: it is the unrendered TAIL and _flush_live_delta trims every
    # settled block off it. Without this the turn's prose existed only
    # as pixels, and on_done's delete was the end of it.
    _live_block_prose $text
    _maybe_follow $w
    # Debounce: re-render the UNSETTLED tail of the region through
    # markdown, not every chunk and not the whole answer. Rendering the
    # entire growing region per token was the O(n²) freeze (32k chars ran
    # past two minutes); rendering it every 200ms instead bounded how
    # OFTEN that happened and not what it COST, which is the same freeze
    # with a smaller constant. _flush_live_delta now settles completed
    # blocks permanently, so each character is rendered once plus however
    # many ticks its own paragraph is still open for.
    #
    # The period is adaptive (live_delta_period), set from the last
    # flush's duration — the reader of this socket shares this event
    # loop, and a fixed timer has no way to notice it is starving it.
    if {$live_delta_after eq ""} {
        set live_delta_after \
            [after $live_delta_period [list ::vchat::_flush_live_delta $sid]]
    }
}

# The other half of the live markdown: re-render the accumulated raw
# region in place, on the debounced schedule or when a non-delta
# construct forces the region to close. The mark anchors the region, so a
# note or tool line inserted elsewhere in the transcript never pushes it.
#
# `close` distinguishes the two callers. The debounced timer keeps the
# region OPEN: more deltas may follow, so the mark and buffer stay so the
# next chunk appends and the next flush replaces the whole region in one
# piece. A non-delta construct (reasoning/tool/note) CLOSES the region:
# once its markdown is rendered, the mark and buffer are dropped — a
# later delta (the next round's answer, streaming after a tool call) must
# open a FRESH region, not append to raw text that already got rendered
# and whose anchor now points across reasoning and tool lines. Leaving
# them set is what duplicated the answer and welded it to a column of
# thinking.
proc ::vchat::_flush_live_delta {sid {close 0}} {
    variable live_delta_mark
    variable live_delta_buf
    variable live_delta_after
    variable live_delta_period
    # The debounce timer fires long after it was armed; load the region
    # it belongs to before touching one accumulator.
    _live_ctx $sid
    if {$live_delta_after ne ""} {
        catch { after cancel $live_delta_after }
        set live_delta_after ""
    }
    if {$live_delta_mark eq ""} { return }
    if {$live_delta_buf eq ""} {
        # Everything streamed so far has already SETTLED — it is painted
        # permanently and there is nothing left to repaint. A close still
        # has to run, though: leaving the mark set would let the next
        # delta append to a region whose anchor now points across the
        # construct that closed it, which is the duplicated-assistant
        # garble _live_break exists to prevent.
        if {$close} { _reset_live_delta }
        return
    }
    set w [::vchat::log_for $sid]
    if {$w eq "" || ![winfo exists $w]} { return }
    set t0 [clock milliseconds]
    # How much of the buffer can never change again. On close the answer
    # is "all of it" — no more deltas are coming for this region.
    set cut [expr {$close ? [string length $live_delta_buf]
                          : [::vmd::settle_point $live_delta_buf]}]
    set settled [string range $live_delta_buf 0 [expr {$cut - 1}]]
    set tail [string range $live_delta_buf $cut end]
    # An unclosed fence has no settle point, so the tail can grow without
    # bound — and re-rendering it every tick would be the very quadratic
    # this proc was rewritten to remove. Past the cap the tail is painted
    # as the RAW text on_delta already streamed: one insert instead of a
    # markdown parse, and the moment the fence closes the settled render
    # replaces it properly.
    set raw_tail [expr {[string length $tail] > 4096}]
    _at_bottom_prestash $w
    _writable $w {
        # Deleting to `end` eats exactly ONE newline before the mark
        # (verified: "PRIOR\n" + mark + "TAIL" deletes back to "PRIOR").
        # The restore below puts exactly one back, so the balance holds
        # whether the mark sits after a break or after settled text.
        $w delete $live_delta_mark end
        if {![$w compare end-1c == 1.0]} {
            _run $w "\n" {}
        }
        if {$settled ne ""} {
            # A mid-stream chunk is a FRAGMENT of the answer; the close
            # flush is the answer's end and renders as it always did.
            if {$close} {
                _render_with_media $w $settled
            } else {
                _render_fragment $w $settled
            }
            set live_delta_buf $tail
        }
        # Everything written above this point is PERMANENT, and the
        # anchor has to sit past all of it — the restored separator
        # included.
        #
        # Anchoring only after settled text was not enough, and the way
        # it failed is worth keeping: `delete $mark end` clamps a mark
        # that now points past the end back TO the end, and the restored
        # "\n" is then inserted at a left-gravity mark, which puts it
        # AFTER the mark rather than before. So the region swallowed one
        # newline per flush and the blank line between two settled blocks
        # disappeared a flush or two after it was drawn. Re-anchoring
        # here, unconditionally, makes the mark mean one thing: the tail
        # starts here and nothing before it is ever touched again.
        $w mark set $live_delta_mark end-1c
        $w mark gravity $live_delta_mark left
        if {$tail ne ""} {
            if {$raw_tail} {
                _run $w $tail assistant
            } else {
                _render_with_media $w $tail
            }
        }
    }
    _maybe_follow $w
    set live_delta_period [_next_flush_period [expr {[clock milliseconds] - $t0}]]
    if {$close} {
        set live_delta_mark ""
        set live_delta_buf ""
        set live_delta_sid ""
    }
}

# How long to wait before the next live-region flush, given how long the
# last one took.
#
# The old rule was a flat 200ms, which bounds how OFTEN the region is
# repainted and says nothing about what a repaint COSTS. That is the
# wrong half to bound: this runs on the Tk event loop, which is also the
# thread that reads the SSE socket, so a flush that takes longer than the
# interval simply starves the reader and the deltas it was waiting for
# then arrive in a burst — the "huge chunks with huge delays" shape.
#
# Holding the next flush to three times the last one's duration caps the
# renderer at roughly a third of the event loop BY CONSTRUCTION, whatever
# the content turns out to cost. The floor keeps a cheap render feeling
# live; the ceiling stops one pathological paint from making the view
# catatonic for the rest of the turn.
proc ::vchat::_next_flush_period {elapsed_ms} {
    return [expr {min(2000, max(200, 3 * $elapsed_ms))}]
}

# Clear the live-markdown region state: cancel the pending debounce and
# drop the mark/buffer. Called when the turn's whole raw region is about
# to be replaced (on_done) or when a stale region is abandoned for a
# fresh stream (on_delta's first chunk, session switch).
proc ::vchat::_reset_live_delta {{sid ""}} {
    variable live_delta_mark
    variable live_delta_buf
    variable live_delta_after
    variable live_delta_period
    variable live_delta_sid
    if {$sid ne ""} { _live_ctx $sid }
    if {$live_delta_after ne ""} { catch { after cancel $live_delta_after } }
    set live_delta_after ""
    set live_delta_mark ""
    set live_delta_buf ""
    set live_delta_sid ""
    # A fresh region starts from the floor: the last turn's worst render
    # is not evidence about this one, and carrying a stretched period
    # across a reset would make one bad paint slow every turn after it.
    set live_delta_period 200
}

proc ::vchat::on_tool {state name payload} {
    variable tool_rounds
    variable tool_args
    set sid [event_session]
    if {$state eq "start"} {
        variable live_tool_marks
        # Capture the INVOCATION (the args) — shown with the result so a
        # tool round reads as "called X with Y, got Z", not just a name.
        set args ""
        catch { set args [dict get $payload args] }
        set tool_args($sid) $args
        _live_ctx $sid
        if {1} {
            # Number this round and mark where its ⚙ header lands, so the
            # reconcile can delete exactly this span before rebuilding the
            # round from live_blocks — the live paint and the reconcile
            # paint are the same round, and only one may survive (R3).
            # The KEY is derived from the count (see the end arm), so the
            # live collapsible and the reloaded one share one identity and
            # a round expanded live stays expanded across the reconcile.
            set ent [expr {[dict exists $live_tool_marks $sid]
                           ? [dict get $live_tool_marks $sid] : {0 {}}}]
            set n [expr {[lindex $ent 0] + 1}]
            set marks [lindex $ent 1]
            dict set live_tool_marks $sid [list $n $marks]
            # A tool round CLOSES the open regions first: render them
            # now so the ⚙ line never welds onto raw deltas/thinking
            # and the next flush cannot delete them. Only for the
            # DISPLAYED session — the live-delta state is that
            # session's, and flushing on an off-screen tool event
            # would stomp the visible transcript.
            #
            # THE REASONING REGION IS CLOSED, NOT JUST FLUSHED, and
            # ONLY on `start` (2026-09-20: "the reasoning gets split
            # into two blocks when it is clearly part of the same
            # stream"). A flush alone painted the pending tail but left
            # the region OPEN, so `_live_break tool` cleared the tag on
            # its way out and the thought that was still streaming
            # reopened as a SECOND "reasoning (streaming)" block with
            # its own header — a provider that emits its thought as
            # bursts around the call rendered as two. `start` is the
            # boundary that means it: the call is what ended the row's
            # thought, exactly as ToolStart is what moves the engine's
            # snapshot into a new round. `end` does not gate — thinking
            # can stream between a round's start and its end.
            ::vchat::_flush_live_reasoning $sid
            ::vchat::_close_live_reasoning $sid
            if {[catch { ::vchat::_flush_live_delta $sid 1 } flush_err]} {
                puts stderr "veles-desktop: on_tool flush failed for $sid: $flush_err"
            }
            # Paint the ⚙ header NOW — the call is what is happening, and
            # a slow tool must show its invocation while it runs, not only
            # when the result lands. The result collapsible is added at
            # `end` under the SAME round key (tool<sid>.<n>). Mark where
            # the header lands so the reconcile can delete exactly this
            # span (header + result) before rebuilding the round.
            # THE SEPARATOR IS ASKED OF THE WIDGET, NOT OF _live_break.
            # _close_live_reasoning above just recorded live_last_kind
            # "tool" — it must, so the next reasoning burst opens a
            # fresh region — so _live_break asked now compares tool
            # against tool and answers "". That ordering is the
            # 2026-09-20 round-2 report verbatim: "tool calls now start
            # right after the reasoning AGAIN even though we've just
            # added something today". The three flushes above also
            # appended content the kind recorder never saw; the tail
            # actually sitting in the widget is the only honest oracle
            # — the same spell the `end` arm's weld guard uses.
            set w [::vchat::log_for $sid]
            set brk ""
            if {$w ne "" && [winfo exists $w]
                && [$w compare end-1c != 1.0]
                && [$w get end-2c end-1c] ne "\n"} {
                set brk "\n"
            }
            # The round's mark is planted BEFORE the insert with left
            # gravity — the emit_message msgstart pattern — so it names
            # where the round BEGINS, break included. The previous
            # placement, `end-1c linestart` read AFTER the insert,
            # lands on the implicit line Tk keeps past the final
            # newline (verified under Xvfb: content "Xhello\n" puts it
            # at 2.0, one line BELOW the header), which made the
            # reconcile's delete-from-mark miss the header and made the
            # weld guard that hung off the mark read the header's own
            # trailing newline instead of the tail before it — dead
            # code, both halves.
            if {$w ne "" && [winfo exists $w]} {
                $w mark set toolround_${sid}_$n end-1c
                $w mark gravity toolround_${sid}_$n left
            }
            variable tool_at
            set tool_at($sid) [clock seconds]
            if {$w ne "" && [winfo exists $w]} {
                _writable $w {
                    _run $w $brk {}
                    _paint_stamp_secs $w $tool_at($sid)
                }
            }
            log_insert "[format [::vi18n::mc {⚙ %s}] [_tool_inv $name $args]]\n" tool $sid ;# i18n-exempt: the glyph is a marker; the invocation is tool syntax
            if {$w ne "" && [winfo exists $w]} {
                dict set live_tool_marks $sid \
                    [list $n [lappend marks toolround_${sid}_$n]]
            }
        }
    } else {
        set args ""
        catch { set args $tool_args($sid) }
        catch { unset tool_args($sid) }
        set res ""
        catch { set res [dict get $payload result] }
        lappend tool_rounds($sid) [list $name $args $res]
        _live_ctx $sid
        if {1} {
            # One entry per RESULT, and it closes the provider call
            # that made it — exactly what the wire did.
            variable tool_at
            _live_block_round $name $args $res \
                [expr {[info exists tool_at($sid)] ? $tool_at($sid) : [clock seconds]}]
            # The RESULT, live — the same shape the reload ladder paints,
            # not the one-line `_tool_preview` stub this used to be (the
            # "tool output blocks only start showing the true output/format
            # when turn ends" report). The ⚙ header went down at `start`;
            # header + this together are `_render_tool_round` verbatim, in
            # the same two-run order that proc lays them down.
            #
            # The KEY is the round's own ("tool<sid>.<n>"), and the mark
            # the start arm recorded spans it, so the reconcile's delete
            # takes exactly this round back off the screen before
            # rebuilding it from live_blocks — one round on screen, live
            # and reload alike (R3).
            variable live_tool_marks
            set n [llength $tool_rounds($sid)]
            set w [::vchat::log_for $sid]
            if {$w ne "" && [winfo exists $w]} {
                # THE WELD GUARD (the "tool calls mixed into the reasoning
                # without a newline" report), and this is the arm that
                # needs it. `_live_break` answers about the START of a
                # round, against the kind transition — but reasoning can
                # arrive BETWEEN a round's start and its end (the model
                # thinks, calls, then thinks again inside one call), and
                # that burst leaves raw text whose tail need not end in a
                # newline. Nothing consults _live_break here, so the
                # result line used to weld onto the thinking. Ask the
                # widget: if the tail is not at a line boundary, put one
                # in — inside the prestash bracket, so follow survives
                # the extra insert (R6).
                _at_bottom_prestash $w
                _writable $w {
                    if {[$w compare end-1c != 1.0] && [$w get end-2c end-1c] ne "\n"} {
                        _run $w "\n" tool
                    }
                    if {$name eq "patch"} {
                        _render_patch_result $w $res
                    } elseif {$res ne ""} {
                        set nchars [string length $res]
                        _collapsible $w "tool${sid}.$n" [dict create \
                            closed "  ▸ result ($nchars chars — click to expand)" \
                            open   "  ▾ result ($nchars chars — click to collapse)" \
                            body   $res \
                            htags  [list tool] \
                            btags  [list tool] \
                            bconf  [list -lmargin1 12 -lmargin2 12]]
                    }
                    # No result to show: the ⚙ header the start arm
                    # painted is already newline-terminated, so the round
                    # stands alone exactly as _render_tool_round leaves it
                    # for an empty result. Nothing to add.
                }
                _maybe_follow $w
            }
        }
        # The Tasks pane shows what these two tools just changed — a
        # `todo` write or a presented plan must land there the moment it
        # happens, not on the next 5s poll of a pane you may not even
        # have open.
        if {$name in {todo present_plan}} {
            catch { ::vpanes::tasks_refresh }
        }
    }
}

# An implicit engine action — a guard fired, history was compacted, a
# fallback route answered. Rendered inline AT THE POINT IT HAPPENED,
# because "my context changed and I do not know when" is the complaint
# this exists to answer.
#
# One dim line by default. Click it for the detail and, when the
# mechanism has a knob, the control to change it — so the setting is
# reachable from where you noticed you wanted it.
# `live` is 1 for a note arriving on the event stream (api.tcl) and 0 for
# one the history painter replays (_paint_one_row): only a LIVE note is
# one of the turn now streaming — a replayed one belongs to a finished
# turn and must never be pulled into this one's reconcile.
proc ::vchat::on_note {d {live 1}} {
    # A note arriving mid-delta is a non-delta construct: close the open
    # region first (render it, drop the anchor) so the note neither welds
    # onto raw deltas nor gets deleted by the next flush. Only for the
    # session ON SCREEN — the live region is that one's, and flushing it
    # from a background session's note would stomp the visible
    # transcript.
    set sid [::vchat::event_session]
    # The shared prologue (see _close_live_for). The break it returns is
    # discarded here and only here: emit_note goes through the block
    # painter, which owns its own separation (R1). What on_note needs
    # from it is the CLOSE and the kind-transition record — the latter
    # was missing, so a note between two delta chunks left live_last_kind
    # saying "delta" and the next chunk asked for a separator it could
    # not get.
    _close_live_for $sid note
    # A live note happened now; a replayed one carries the server's time.
    if {$live && ![dict exists $d at]} { dict set d at [clock seconds] }
    set id [emit_note $d]
    # Inside a turn this window is streaming, the note is also one of the
    # turn's entries — the reconcile rebuilds the turn from them, and it
    # must put the note back where it happened.
    variable turn_anchor
    if {$live && $id ne "" && [dict exists $turn_anchor $sid]} {
        _live_block_note $d $id
    }
    return $id
}

# A note is a transcript entry, not decoration.
#
# USER: "The guards disappear after I reload the session?" The history
# endpoint returns them and open_session replays them — but they were
# written straight into the text widget and never recorded alongside
# the messages, so `rebuild` (a view-source toggle, a theme change, a
# per-message flip) redrew the transcript from `raw` alone and every
# note vanished. Notes now take a slot in the SAME ordered store as
# messages, which is the only way they can keep their position in it.
# Paint a stamp, then whatever the block's own render is — the note
# painters go through _emit_block's render arm, so the stamp rides inside
# the same separation.
proc ::vchat::_stamped_render {w id render} {
    _paint_stamp $w $id
    {*}$render
}

proc ::vchat::emit_note {d} {
    variable raw
    variable raw_kind
    variable raw_src
    variable raw_note
    variable msg_seq
    if {![info exists msg_seq]} { set msg_seq 0 }
    incr msg_seq
    set id $msg_seq
    set summary ""
    catch { set summary [dict get $d summary] }
    set w [::vchat::log]
    if {$w eq "" || ![winfo exists $w]} { return "" }
    set raw($id) $summary
    set raw_kind($id) note
    set raw_src($id) 0
    set raw_note($id) $d
    variable raw_at
    set at ""
    catch { set at [dict get $d at] }
    set raw_at($id) [_stamp_secs $at]
    variable raw_w
    set raw_w($id) $w
    # Same bracket discipline as emit_message (R6): notes with
    # collapsible bodies elide — the bottom MOVES under the view.
    _at_bottom_prestash
    _writable $w {
        $w mark set msgstart_$id [$w index end-1c]
        $w mark gravity msgstart_$id left
        _paint_note $id $d
        $w mark set msgend_$id [$w index end-1c]
        $w mark gravity msgend_$id left
    }
    _maybe_follow
    return $id
}

# One dim line at the point it happened, expandable for the detail and,
# when the mechanism has a knob, the control to change it — so the
# setting is reachable from where you noticed you wanted it.
#
# The tag is keyed by MESSAGE ID, not by a running counter: a rebuild
# repaints the same note and must land on the same tag, or the click
# binding and the stashed payload drift apart from the text.
proc ::vchat::_paint_note {id d} {
    set kind ""
    set summary ""
    catch { set kind [dict get $d kind] }
    catch { set summary [dict get $d summary] }
    set detail ""
    catch { set detail [dict get $d detail] }
    set toggle {}
    catch { set toggle [dict get $d toggle] }

    set glyph [_note_glyph $kind]
    set w [::vchat::log]
    set head "$glyph $summary"

    # Nothing to expand: a plain line, and NOT clickable. A header that
    # invites a click and then does nothing is worse than no header.
    # A note is a logical block: _emit_block separates it like every
    # other one (R1) — separation is no longer hand-rolled here.
    if {$detail eq "" && ![llength $toggle]} {
        _emit_block note "" {} [list _stamped_render $w $id [list _run $w "$head\n" note]]
        return
    }

    # The body, built once and hidden — not assembled on click.
    #
    # THE BUG THIS FIXES. `_note_expand` used to end in `emit_message
    # system`, which appends a whole new message at the END of the
    # transcript. USER: "Clicking on a guard seems to dump new text in
    # the transcript window at the bottom instead of expanding like
    # reasoning, ideally it would just do that."
    #
    # Painting the body HERE rather than on click is also what makes it
    # survive `rebuild` — the same reason reasoning is painted this way.
    set lines {}
    if {$detail ne ""} {
        foreach l [split $detail "\n"] { lappend lines "  [string trim $l]" }
    }
    if {[llength $toggle]} {
        set tkey ""  ; catch { set tkey [dict get $toggle key] }
        set tval ""  ; catch { set tval [dict get $toggle value] }
        set thelp "" ; catch { set thelp [dict get $toggle help] }
        if {[llength $lines]} { lappend lines "" }
        lappend lines "  $tkey = $tval"
        if {$thelp ne ""} { lappend lines "  $thelp" }
        lappend lines "  → /mechanisms to change this for the session"
    }

    # Keyed by MESSAGE ID, so a repaint lands on the same block and an
    # expanded note comes back expanded. The collapsible is a multi-run
    # body, so it rides through _emit_block's render arm: separated once,
    # here, and filled by _collapsible under the same umbrella.
    set opts [dict create \
        closed "$head  ▸" \
        open   "$head  ▾" \
        body   [join $lines "\n"] \
        htags  [list note] \
        btags  [list note] \
        bconf  [list -lmargin1 12 -lmargin2 12]]
    _emit_block note "" {} [list _stamped_render $w $id [list _collapsible $w "note$id" $opts]]
    return
}

proc ::vchat::_note_glyph {kind} {
    switch -- $kind {
        guard          { return "⚠" }
        compaction     { return "⇲" }
        failover       { return "⇄" }
        approval       { return "⚖" }
        route          { return "⇢" }
        aux_route      { return "↗" }
        memory         { return "🧠" }
        plugin_rewrite { return "✎" }
        plugin         { return "⚙" }
        schedule       { return "⏱" }
        cache          { return "≡" }
        injection      { return "⇒" }
        reasoning      { return "💭" }
        mail           { return "✉" }
    }
    return "•"
}

# Session mail (veles_core::mail) as one labelled block: the sender in
# the head, the first line in view, the whole text on a click. The ONE
# painter for it — the history row and a live turn-started both call it,
# so live and reload cannot disagree about what mail looks like.
proc ::vchat::_mail_note {inj content {at ""}} {
    set from [string range $inj 5 end]
    set first [lindex [split [string trim $content] "\n"] 0]
    if {[string length $first] > 120} {
        set first "[string range $first 0 118]…"
    }
    set d [dict create kind mail \
        summary "[::vi18n::mc {message from session %s} $from] — $first"]
    if {$content ne ""} { dict set d detail $content }
    if {$at ne ""} { dict set d at $at }
    ::vchat::on_note $d 0
}

# RM3-S4/A4: what the labelled block SAYS for each injection class.
proc ::vchat::_injection_caption {inj} {
    switch -- $inj {
        steer        { return "steered: your redirect mid-turn" }
        background   { return "engine told: background task completed" }
        compaction   { return "context compacted: summary of the compressed history" }
        todo         { return "engine told: open TODO ITEMS re-injected after compaction" }
        plan         { return "engine told: the plan you presented, re-injected after compaction" }
        hook         { return "engine told: compact_start hook context" }
        verification { return "engine told: verification nudge from the guard ring" }
        irc_context   { return "engine told: IRC channel context (the lines this turn saw)" }
        migration    { return "engine told: this session moved to another node" }
    }
    # The surface-provenance family (PENDING, 2026-09-16): origin:desktop,
    # origin:tui, origin:telegram, … — each renders as "said on X".
    if {[string match "origin:*" $inj]} {
        return "said on [string range $inj 7 end]"
    }
    return "engine told: $inj"
}

# Replace a turn's raw live region with its reconciled blocks.
#
# ONE ladder, shared by the turn that finished and the turn that died.
# R3 says the live transcript must equal what a reload paints, and a
# FAILED turn is still a turn: the store holds its rows plus
# `[veles] all routes failed` as an ordinary assistant row. While
# `turn_error` painted no reconcile, live and reload disagreed for
# every failed turn, and the live copy was raw unrendered markdown in
# a region whose tags the next turn's reap used to strip (43fb99b).
#
# Reads this session's accumulators, so `_live_ctx $sid` must already
# have run. `reply` REPLACES the last call's streamed prose (see the
# note inside) — a failed turn passes "" and keeps what it streamed.
proc ::vchat::_reconcile_turn {sid reply {fallback_reasoning ""}} {
    variable live_blocks
    variable live_extra
    variable turn_anchor
    variable tool_rounds
    set w [::vchat::log_for $sid]
    if {[winfo exists $w]} {
        # live_blocks is THIS session's, loaded by the _live_ctx above —
        # every row it streamed, whether or not anyone was watching. The
        # fallback below (rebuild from the done event's single
        # display_reasoning) is only for a turn this window never saw
        # start at all.
        set blocks $live_blocks
        # No anchor — a turn this window never saw start, or one whose
        # transcript the LRU dropped mid-flight — means APPEND: the
        # delete below spans nothing and the emit lands at the tail.
        # Skipping the paint (the old behaviour) would lose the answer
        # outright now that reopening does not reload.
        #
        # `end`, not `end-1c`, which claimed to be a no-op and was not:
        # end-1c is the position OF the transcript's final newline, so
        # the "no-op" ate it and welded the answer onto the last line.
        set anchor [expr {[dict exists $turn_anchor $sid]
                          ? [dict get $turn_anchor $sid]
                          : [$w index end]}]
        set tools {}
        if {[info exists tool_rounds($sid)]} { set tools $tool_rounds($sid) }
        catch { unset tool_rounds($sid) }
        if {$anchor ne "" && ($reply ne "" || [llength $tools] || [llength $blocks])} {
            _writable $w {
                $w delete $anchor end
            }
            # The live tool rounds were painted OUTSIDE the region the
            # delete above spans in one case: a background completion
            # (live_extra) lands between the last prose and a tool round,
            # so the round can sit BELOW the anchor and survive the wipe.
            # The rebuild from live_blocks paints every round again, so
            # any live round that escaped the delete is a duplicate —
            # the "exactly one round" half of R3. Each round's start mark
            # was recorded when it streamed; delete from there to the
            # tail, then unset the mark. Oldest first: marks are left
            # gravity and stable, so removing an earlier span does not
            # move a later round's mark above its target.
            variable live_tool_marks
            if {[dict exists $live_tool_marks $sid]} {
                lassign [dict get $live_tool_marks $sid] cnt marks
                foreach m $marks {
                    if {[catch { $w index $m } mi] || $mi eq ""} { continue }
                    $w delete $m end
                    catch { $w mark unset $m }
                }
                # The delete removed the live rounds' TEXT but not their
                # collapsible tags or state — a tag with no ranges still
                # answers `tag names`, so the next reload/rebuild counting
                # chdr_* would see the dead round's tag beside the live
                # one. The rebuilt round re-paints under the RELOAD
                # ladder's own key (tool<msgid>.<n>, not the live
                # tool<sid>.<n>), so the live keys must be dropped whole —
                # tag and collapse state — or they leak one entry per
                # round for the life of the widget.
                variable collapse_open
                variable collapse_text
                for {set i 1} {$i <= $cnt} {incr i} {
                    catch { $w tag delete chdr_tool${sid}.$i }
                    catch { $w tag delete cbody_tool${sid}.$i }
                    catch { unset collapse_open(tool${sid}.$i) }
                    catch { unset collapse_text(tool${sid}.$i) }
                }
                dict unset live_tool_marks $sid
            }
            # The live region was the only text carrying live_r_*, so the
            # delete above removed the tags' ranges — but NOT their entries
            # in the widget's tag table. Drop the configs too, or one dead
            # live_r_* tag accumulates per turn for the life of the widget
            # (the phase-2 note: "live_r_* tags are never deleted — one
            # leaked tag per turn"). One region PER BURST (R7), so the
            # names cannot be enumerated from the accumulators.
            #
            # WHAT THE DELETE ACTUALLY REMOVED, then — not every live_r_*
            # in the widget. `tag names` is the whole TRANSCRIPT, and a
            # turn that died leaves its live region on screen on purpose
            # (turn_error paints no reconcile over it), so the next turn's
            # reap reached back and deleted a region that was still there.
            # forget_raw already says, one proc away, what Tk does then:
            # the text is "silently re-created unstyled and un-elided" —
            # the dead turn's thinking kept its words and lost its colour,
            # its italic and its fold, and read as the model's own output.
            # USER 2026-09-18: "it's showing the reasoning blocks before
            # it as if it was model output?"
            #
            # A tag with no ranges left is a leaked tag; a tag that still
            # has ranges is a region still on screen. That distinction is
            # the whole rule, and it needs no bookkeeping to ask.
            # forget_raw still reaps wholesale on a session change, which
            # is right — there the text goes too.
            foreach t [$w tag names] {
                if {[string match "live_r_*" $t]
                    && ![llength [$w tag ranges $t]]} {
                    catch { $w tag delete $t }
                }
            }
            # THE RELOAD LADDER, on the live path. `_paint_one_row`
            # renders a persisted turn with exactly two emit shapes —
            # one per assistant row, one per tool row — and R3 says the
            # live transcript must equal it. So the reconcile IS those
            # two shapes, walking the rows this turn produced, and the
            # equality is structural rather than something two
            # hand-written ladders are asserted to agree about.
            if {![llength $blocks]} {
                # NO row record of this turn — this window never saw it
                # start. All we have is the rounds, in order, and ONE
                # reasoning text: wrap_for_bus (agent.rs) keeps the LAST
                # provider call's, i.e. the thinking that produced the
                # ANSWER, after the last tool round.
                #
                # This used to stack every round of the turn under that
                # one block — a collapsible claiming to be the thinking
                # behind calls it never saw. USER 2026-09-18: "these
                # reasoning blocks seem to also all get merged into one
                # single reasoning even though they were actually
                # interspersed with tool calls". And with no reasoning at
                # all it emitted only the reply, so the turn's tool
                # rounds vanished off the screen the delete had just
                # cleared.
                #
                # Rounds first, then the answer carrying the one reasoning
                # that led to it: the shape a RELOAD of the same turn
                # paints, and the most that can honestly be claimed from
                # what the done event carries.
                set dr ""
                set dr $fallback_reasoning
                foreach t $tools { lappend blocks [linsert $t 0 tool] }
                if {$dr ne ""} { lappend blocks [list call $dr ""] }
            }
            # THE ANSWER IS THE LAST CALL'S CONTENT. TurnResult.reply is
            # `message.content.clone()` (agent.rs) — the last provider
            # call's content, not an accumulation — so it REPLACES that
            # call's streamed prose rather than being appended after it,
            # and nothing doubles. A turn that ended on a tool round and
            # still produced text (the reconnect shapes) gets it as a
            # trailing call, because that is the row it would be.
            #
            # Notes do not end a call: a note that arrived after the last
            # prose (a cache miss, the verification hint) still leaves
            # that call the last one, so look past trailing notes.
            if {$reply ne ""} {
                set last -1
                for {set i [expr {[llength $blocks] - 1}]} {$i >= 0} {incr i -1} {
                    if {[lindex $blocks $i 0] ne "note"} { set last $i ; break }
                }
                if {$last >= 0 && [lindex $blocks $last 0] eq "call"} {
                    lset blocks $last 2 $reply
                } else {
                    lappend blocks [list call "" $reply]
                }
            }
            foreach b $blocks {
                lassign $b bkind bx by bz bw
                if {$bkind eq "call"} {
                    # A call that neither thought nor spoke has nothing
                    # to show. The reload ladder does emit the empty
                    # block for such a row; _block_sep collapses runs of
                    # blanks, so both paths render the same characters.
                    if {$bx eq "" && $by eq ""} { continue }
                    emit_message assistant $by $bx {} [expr {$bz eq "" ? "now" : $bz}]
                } elseif {$bkind eq "note"} {
                    # Painted again in its place; its live copy went with
                    # the delete above, and its old entry in the ordered
                    # store goes too, or a rebuild would show it twice.
                    _forget_raw_entry $by
                    emit_note $bx
                } else {
                    emit_message tool $bz {} [list [list $bx $by $bz]] \
                        [expr {$bw eq "" ? "now" : $bw}]
                }
            }
            # Foreign lines painted INTO this region while the turn ran —
            # a background child/shell/monitor that finished — went with
            # the delete above. Put them back after the reconciled turn,
            # which is where a reload shows them too: the engine injects
            # its [BACKGROUND … COMPLETE] row at the turn BOUNDARY, not
            # inside the turn.
            if {[dict exists $live_extra $sid]} {
                foreach line [dict get $live_extra $sid] {
                    log_insert $line status $sid
                }
            }
        }
    }
}

proc ::vchat::on_done {d} {
    variable busy
    variable turn_prompt
    variable turn_completion
    variable live_rtag
    variable live_rhdr
    variable live_rtext
    variable live_blocks
    variable live_ptext
    variable live_extra
    variable turn_anchor
    variable tool_rounds
    variable turn_prompt_ids
    # Whose turn finished — not necessarily the one on screen, and it no
    # longer matters: this session's live region is its own.
    set sid [event_session]
    _live_ctx $sid
    # Any reasoning still pending has to be painted and accumulated
    # BEFORE the reconcile reads it — the collapsible this turn leaves
    # behind is built from live_blocks, and a tail that never got
    # flushed would be missing from the reloaded transcript too (R4:
    # reasoning never vanishes).
    #
    # This used to run only `if {$on_screen}`, and the reconcile below
    # then read the accumulator only when on screen — so a turn finishing
    # while you were looking elsewhere rebuilt its transcript from the
    # done event's single display_reasoning and dropped every earlier
    # round's thinking. That is the second half of the switch-away
    # report; the first half was never painting it at all.
    _flush_live_reasoning $sid
    # …and the prose the last call streamed, for the same reason: it is
    # accumulated cheaply and folded into its row at boundaries, and the
    # final round has no boundary after it but this one.
    _live_block_settle
    # The turn's raw region (including the live-markdown region inside
    # it) is about to be replaced whole by the reconciled markdown
    # render — cancel any pending debounce and clear the region state
    # first, or the timer would re-render into text the reconcile just
    # deleted.
    ::vchat::_reset_live_delta $sid
    # ONE key per catch. These were a single catch block with `text`
    # fetched last, so any missing key above it — and the interrupted
    # path sends `done` with nothing but `text` — silently skipped the
    # reply as well.
    set provider ""
    set model ""
    set reply ""
    set notice ""
    catch { set provider        [dict get $d provider] }
    catch { set model           [dict get $d model] }
    catch { set turn_prompt     [dict get $d prompt_tokens] }
    catch { set turn_completion [dict get $d completion_tokens] }
    catch { set reply           [dict get $d text] }
    catch { set notice          [dict get $d notice] }
    if {$notice eq "null"} { set notice "" }
    ;# Replace the raw-streamed region with the markdown-rendered reply
    ;# (hermes-desktop parity). The final display_reasoning rides on the
    ;# done event — re-render it as a collapsible block (the raw
    ;# streamed reasoning went away with the region).
    ;#
    ;# INTO THIS SESSION'S OWN TRANSCRIPT, on screen or not. It used to
    ;# be skipped for a background session on the grounds that "the text
    ;# is on the server either way, and reopening the session loads it"
    ;# — which stopped being true the moment a reopen stopped reloading.
    ;# A finished background turn that painted nowhere would simply be
    ;# missing until the app restarted.
    # The reconcile, shared with the failed-turn path — see
    # `_reconcile_turn`. The last provider call's display_reasoning is
    # the fallback for a turn this window never saw start; the
    # accumulator path (live_blocks) usually makes it redundant.
    set display_reasoning ""
    catch { set display_reasoning [dict get $d display_reasoning] }
    if {$display_reasoning eq "null"} { set display_reasoning "" }
    _reconcile_turn $sid $reply $display_reasoning
    set w [::vchat::log_for $sid]
    if {[winfo exists $w]} {
        # A turn can finish having produced reasoning and no answer.
        # Saying nothing left the reasoning looking like the reply —
        # USER: "I only got reasoning output to my first prompt? model
        # claims it delivered the response."
        if {$notice ne ""} {
            log_insert "\n[format [::vi18n::mc {⚠ %s}] $notice]\n" system
        }
        log_insert "· $provider/$model · ${turn_prompt}→${turn_completion} tok\n" status ;# i18n-exempt: provider/model and token counts, no words
    }
    # This session's live region is spent: the reconcile above replaced
    # it with the finished blocks. Reset it whether or not anyone was
    # watching — it is the SESSION's region now, and leaving a hidden
    # one armed would weld the next turn onto the last one's tags.
    set live_rtag ""
    set live_rhdr ""
    set live_rtext ""
    set live_blocks {}
    set live_ptext ""
    catch { dict unset live_extra $sid }
    # The marks are spent: the reconcile above deleted their spans and
    # the rebuilt rounds carry their own keys. Leaving them would make
    # the NEXT turn's reconcile delete text this turn's marks point at
    # (they ride with edits, so they would land inside the new turn).
    variable live_tool_marks
    catch { dict unset live_tool_marks $sid }
    variable live_last_kind
    set live_last_kind ""
    # The STATUS LINE is window chrome and still belongs to the session
    # on screen — a background turn finishing says so and no more.
    if {[for_display]} {
        ::vstatus::set_msg [expr {$notice ne "" \
            ? [::vi18n::mc {done, but the model returned no text}] \
            : [::vi18n::mc {done · %1$s/%2$s} $provider $model]}]
    } else {
        ::vstatus::set_msg [::vi18n::mc {a turn finished in another session}]
    }
    dict unset turn_anchor $sid
    # A later turn may legitimately reuse this session's replay cursor,
    # but never this completed turn's prompt-ID mappings.
    catch { dict unset turn_prompt_ids $sid }
    _mark_running $sid 0
    # Context window just changed — the turn appended rows. Refresh the
    # context segment NOW rather than waiting up to 15s for the status
    # poll (USER: "context should update every turn, not just when
    # control is returned"). The report is computed from the store
    # without the agent lock, so the figures are real the moment the
    # guard drops.
    catch { ::vstatus::refresh_context $sid }
    # The cache for THIS session is now stale — the turn persisted at
    # least one new row. The next open's background verify would heal
    # it, but dropping it now makes the very next switch paint AND
    # verify the truth, not yesterday's.
    variable hist_cache
    if {[info exists hist_cache]} { catch { dict unset hist_cache $sid } }
}

proc ::vchat::on_error {msg} {
    # Into the failing session's OWN transcript, on screen or not. A
    # background turn that died silently and showed nothing when you
    # came back to it is the failure this whole change exists to end.
    log_insert "\n[format [::vi18n::mc {✖ %s}] $msg]\n" system
    turn_error $msg
}

# ── the session event stream: one resumable view onto every turn ─────
#
# POST /api/chat is fire-and-forget; a turn's events ride the session bus
# and reach this client over ONE persistent stream per open session
# (GET /api/sessions/:id/events). This is irc.tcl's reconnect supervisor
# applied to chat, deliberately and near line-for-line, because IRC already
# solved "a dropped event stream resumes from a durable cursor": replay
# since the cursor, a live-boundary marker, lagged reported rather than
# silence, and a backed-off reconnect on drop.
#
# It replaces BOTH the per-turn SSE stream AND the polling turn watcher
# (_watch_turn/_watch_tick, deleted). A view that reconnects itself needs
# no poller, so the 200-tick give-up that froze any turn over ~10 minutes
# is gone by construction, and a second window — or this window after a
# reconnect — watches the same turn the starter does.
namespace eval ::vchat {
    ;# ONE STREAM PER LOADED SESSION, all open at once.
    ;#
    ;# This used to be one connection for the session on screen, and
    ;# switching closed it — so a session you were not looking at went
    ;# BLIND, and the only reason that was survivable is that the switch
    ;# back reloaded the transcript from scratch. Sessions keep their
    ;# transcripts now, so there is nothing to reload into; the stream
    ;# has to stay up instead.
    ;#
    ;# Tk carries this fine. `::vapi::session_events` is already
    ;# `http::geturl -handler … -command … -timeout 0` — fileevent-driven,
    ;# no `vwait`, no blocking — and `::vapi::_sse` is already an array
    ;# keyed by token with a `streaming_sessions` (plural) accessor. N
    ;# connections interleave on the one event loop. Threads would not
    ;# help and are not used: a Tk widget may only be touched by the
    ;# thread that owns it, so painting stays here either way.
    ;#
    ;# Every scalar below is therefore an ARRAY keyed by session id.
    ;# Token of each open stream.
    variable ev_stream
    array set ev_stream {}
    ;# Last durable msg_id rendered, per session — the resume cursor. Set
    ;# from the history fetch on open, advanced by session-live and
    ;# turn-result, so a reconnect refetches exactly the gap and never
    ;# replays what is already on screen.
    variable ev_cursor
    array set ev_cursor {}
    ;# The NOTE-side resume cursor, twin of ev_cursor: the highest
    ;# engine_notes.id rendered, advanced by notes wherever they arrive
    ;# (live `note` frames, replayed note rows, session-live /
    ;# history-appended). A note's anchor is where it RENDERS, so the
    ;# message cursor cannot say whether this client has seen it —
    ;# without this a note anchored at/before the cursor but persisted
    ;# after it was never replayed on reattach (PENDING 2026-09-19).
    variable ev_note_cursor
    array set ev_note_cursor {}
    ;# Reconnect backoff (ms) and its timer id, per session, mirroring
    ;# ::virc. Per session because one gateway hiccup on one stream must
    ;# not back off the other eleven.
    variable ev_retry
    array set ev_retry {}
    variable ev_retry_after
    array set ev_retry_after {}
    variable ev_disconnected
    array set ev_disconnected {}
    ;# Buffered `history` rows, painted as one batch at the live boundary
    ;# so tool-call correlation (the callmap pass) works exactly as a full
    ;# open does — a row-at-a-time paint would lose the args.
    variable ev_replay
    array set ev_replay {}
    ;# sid -> transcript index BEFORE the turn's optimistic user message.
    ;# A mid-turn reconnect replays the persisted user row (the resume
    ;# cursor is pre-turn), so the reconnect clears from here first and the
    ;# replay repaints one copy from the store instead of doubling it.
    variable turn_reload_anchor {}
    ;# sid -> persisted prompt msg_id -> raw row id. Unlike a text anchor,
    ;# this survives a transcript rebuild, which is exactly when a delayed
    ;# replay used to append the optimistic prompt a second time.
    variable turn_prompt_ids {}
    ;# Set when a reconnect is reopening after a DROP (not a first open),
    ;# so the next replay knows to clear the stale live region first.
    variable ev_reconnect_pending
    array set ev_reconnect_pending {}
    ;# Set when this connection has been handed the in-flight turn (a
    ;# turn-snapshot arrived before session-live). If a turn is marked
    ;# running but no snapshot came, the turn DIED during the drop — the
    ;# gateway restarted and the new process never knew about it.
    variable ev_snap_seen
    array set ev_snap_seen {}
    ;# Separately remembers HOW this connection came to be: 1 after a
    ;# drop-reconnect, 0 on a plain view switch. `ev_reconnect_pending`
    ;# is consumed by the FIRST replay that arrives, so it cannot serve
    ;# as the session-live discriminator — a dead turn reconnects with
    ;# an EMPTY replay and the flag is spent before the guard reads it.
    variable ev_reconnected
    array set ev_reconnected {}
}

# Every session with a stream open or opening.
proc ::vchat::streaming_sessions {} {
    variable ev_stream
    return [array names ev_stream]
}

# Open (or reopen) the events stream for $sid, resuming from its cursor.
proc ::vchat::_events_connect {sid} {
    variable ev_stream
    variable ev_cursor
    variable ev_note_cursor
    variable ev_disconnected
    variable ev_snap_seen
    variable ev_reconnected
    set ev_disconnected($sid) 0
    # Each connection re-derives whether a turn is live from its own
    # snapshot, so the "did we see one this time" flag starts clear.
    set ev_snap_seen($sid) 0
    # Normalize to "fresh open" at connect: _events_reconnect stamps the
    # reconnect fact right after this returns, and any other caller is
    # by definition a fresh (view-switch) open.
    set ev_reconnected($sid) 0
    # Close only THIS session's previous stream. It used to close
    # whatever was open, which is how switching sessions blinded the one
    # you left; the other sessions' connections are none of this one's
    # business.
    if {[info exists ev_stream($sid)] && $ev_stream($sid) ne ""} {
        catch { ::vapi::sse_close $ev_stream($sid) } _close_err
    }
    set ev_stream($sid) ""
    if {![info exists ev_cursor($sid)]} { set ev_cursor($sid) 0 }
    if {![info exists ev_note_cursor($sid)]} { set ev_note_cursor($sid) 0 }
    if {[catch {
        ::vapi::session_events $sid $ev_cursor($sid) $ev_note_cursor($sid) \
            [list ::vchat::on_session_event $sid]
    } tok]} {
        # Say why — a stream that cannot open retries quietly otherwise,
        # and "chat shows nothing" reads as "nobody said anything".
        catch { ::vstatus::set_msg [format [::vi18n::mc {session stream unavailable: %s}] $tok] }
        _events_schedule_retry $sid
        return ""
    }
    set ev_stream($sid) $tok
    return $tok
}

proc ::vchat::_events_schedule_retry {sid} {
    variable ev_retry
    variable ev_retry_after
    if {![info exists ev_retry($sid)]} { set ev_retry($sid) 1000 }
    # RECORD THE TIMER ID so _events_disconnect can cancel it on a switch
    # or detach — irc.tcl's bug was a bare `after` whose id went nowhere,
    # so a pending reconnect fired into the NEW gateway with the OLD cursor.
    if {[info exists ev_retry_after($sid)] && $ev_retry_after($sid) ne ""} {
        catch { after cancel $ev_retry_after($sid) }
    }
    set ev_retry_after($sid) \
        [after $ev_retry($sid) [list ::vchat::_events_reconnect $sid]]
    # Back off to a minute: a gateway that is down stays down, and a tight
    # loop against a dead socket burns a core.
    set ev_retry($sid) [expr {min($ev_retry($sid) * 2, 60000)}]
}

# Reconnect from the cursor — the backoff timer's target, the response to
# a session-lagged event (the engine dropped rather than pretended; the
# store still has everything, so resuming refetches exactly the gap), and
# how a switch TO a busy session gets its in-flight partial.
proc ::vchat::_events_reconnect {sid} {
    variable ev_retry_after
    variable ev_reconnect_pending
    variable ev_reconnected
    set ev_retry_after($sid) ""
    if {$sid eq ""} { return }
    # This is a reopen after a DROP, so the replay that follows must clear
    # the stale live turn region before repainting (see _flush_replay).
    set ev_reconnect_pending($sid) 1
    _events_connect $sid
    # _events_connect normalized the fact to "fresh"; this connection is
    # not. Set it AFTER the call so the fact survives to session-live.
    set ev_reconnected($sid) 1
    # The reconnect's replay carries store rows, not tap-only events, so an
    # `ask-resolved` that fired during the drop never arrives. Re-fetch the
    # parked asks: _redraw_asks shows one still open AND closes a dialog
    # whose question was answered while the socket was down.
    if {[catch {
        ::vapi::get_json_async "/api/asks" \
            [list ::vchat::_redraw_asks $sid [_ask_on_screen]]
    } reconnect_asks_err]} {
        puts stderr "veles-desktop: reconnect ask refetch failed for $sid: $reconnect_asks_err"
    }
}

# Close ONE session's stream and stop its backoff — on delete, on an LRU
# eviction, and (via _events_disconnect_all) on detach and quit. SOCKETS
# ONLY: the turn keeps running on the gateway, which is the whole point
# ("the session runs on the gateway; my window is a view onto it").
proc ::vchat::_events_disconnect {sid} {
    variable ev_stream
    variable ev_retry_after
    variable ev_disconnected
    if {[info exists ev_stream($sid)] && $ev_stream($sid) ne ""} {
        catch { ::vapi::sse_close $ev_stream($sid) } _close_err
    }
    catch { unset ev_stream($sid) }
    if {[info exists ev_retry_after($sid)] && $ev_retry_after($sid) ne ""} {
        catch { after cancel $ev_retry_after($sid) }
    }
    catch { unset ev_retry_after($sid) }
    set ev_disconnected($sid) 1
}

# Every stream, for the two events that mean "this window is done
# watching this gateway": quit and attach teardown.
proc ::vchat::_events_disconnect_all {} {
    foreach sid [streaming_sessions] { _events_disconnect $sid }
}

# The stream's event router. Bound to $sid at open, so an event is
# attributed to the right transcript even after `current` moves on — the
# same reason ::vapi captures the connection per token.
proc ::vchat::on_session_event {sid ev d} {
    variable ev_cursor
    variable ev_note_cursor
    variable ev_retry
    variable ev_snap_seen
    variable ev_replay
    variable ev_stream
    variable turn_reload_anchor
    variable turn_prompt_ids
    variable turn_anchor
    variable session
    variable stream_session
    switch -- $ev {
        history {
            # A persisted row replayed after the cursor. Buffer it; the
            # boundary event paints the batch through the reload ladder.
            # A note row carries its engine_notes id — advance the note
            # cursor from it so the note is never replayed again.
            set role ""
            catch { set role [dict get $d role] }
            if {$role eq "note"} {
                set nid 0
                catch { set nid [dict get $d note_id] }
                if {$nid > 0} {
                    if {![info exists ev_note_cursor($sid)] || $nid > $ev_note_cursor($sid)} {
                        set ev_note_cursor($sid) $nid
                    }
                }
            }
            lappend ::vchat::ev_replay($sid) $d
        }
        turn-snapshot {
            # Attaching MID-TURN: the engine's picture of the turn already
            # in flight. Paint the replay that precedes it, adopt the turn
            # as running, then render the partial from the snapshot.
            _flush_replay $sid
            set ev_snap_seen($sid) 1
            _mark_running $sid 1
            # The ANCHORS are set for every session, on screen or not —
            # they are where on_done splices the answer into this
            # session's own transcript — and so is the PARTIAL: the live
            # handlers paint into the event's own widget now, so a
            # snapshot for a hidden session rebuilds that session's
            # region rather than being skipped. (It used to say "the
            # live region is singular by design"; it is per-session.)
            set w [::vchat::log_for $sid]
            if {[winfo exists $w]} {
                set stream_session $sid
                if {![dict exists $turn_anchor $sid]} {
                    dict set turn_anchor $sid [$w index end-1c]
                }
                # Anchor the partial so a LATER reconnect clears it before
                # the snapshot repaints — otherwise the stale partial and
                # the fresh one stack up.
                if {![dict exists $turn_reload_anchor $sid]} {
                    dict set turn_reload_anchor $sid [$w index end-1c]
                }
                _paint_snapshot $d
                set stream_session ""
            }
        }
        history-appended {
            # Rows written by a turn in ANOTHER PROCESS — a delegate or
            # /btw child, which runs as its own `veles chat -q` — and
            # therefore published to a session bus this gateway is not
            # subscribed to. `session_tap` is process-wide by
            # construction, there is no events table in the store and
            # no relay, so for a child session the live half of this
            # stream has nothing to carry: it replayed once at attach
            # and then sat still for the whole turn (USER 2026-09-18:
            # "it stopped streaming and looked hung even though there
            # were new messages coming in").
            #
            # The engine polls the store for exactly that case and
            # sends the gap as ordinary `history` frames; this is only
            # the boundary that paints them and moves the cursor.
            #
            # NOT `session-live`: that arm runs the
            # was-I-handed-a-snapshot ladder, which for a session
            # `is_running` in another process asks /context and
            # re-handshakes — once per poll tick, forever. NOT
            # `history-changed`: that repaints the whole transcript.
            _flush_replay $sid
            catch { set ev_cursor($sid) [dict get $d last_msg_id] }
            catch { set nid [dict get $d last_note_id]
                if {![info exists ev_note_cursor($sid)] || $nid > $ev_note_cursor($sid)} {
                    set ev_note_cursor($sid) $nid
                } }
        }
        session-live {
            # The past is replayed; everything after this is live. Paint
            # any buffered rows and take the cursor the engine hands over.
            # Both of them: last_note_id is the note-side resume point.
            _flush_replay $sid
            set ev_retry($sid) 1000
            catch { set ev_cursor($sid) [dict get $d last_msg_id] }
            catch { set nid [dict get $d last_note_id]
                if {![info exists ev_note_cursor($sid)] || $nid > $ev_note_cursor($sid)} {
                    set ev_note_cursor($sid) $nid
                } }
            # A turn marked running that this connection was NOT handed
            # (no snapshot before the live boundary) is one of TWO very
            # different stories, and the note-printing must pick the
            # right one:
            #
            #  RECONNECT (drop-reopen): the running mark was painted by
            #  THIS view, the turn's own events never reached it, and the
            #  replay it just opened from the pre-turn cursor shows no
            #  finished row. That is the crash fingerprint — the gateway
            #  restarted, the new process never knew the turn. Say so
            #  (the note is true ONLY here), clear the mark and drop the
            #  cache, because the turn persisted rows no result event
            #  will ever announce.
            #
            #  FRESH open (view switch back): the mark can predate this
            #  view entirely, or hold over from its own switch-away —
            #  and the correct states that produce "empty replay + stale
            #  running" include the turn COMPLETING while this view sat
            #  elsewhere. There is no inference to make; ask the engine.
            #  It has a single-source answer: the turn registry that the
            #  guard clears on every exit, panic included, reported by
            #  GET /context `busy` ("nothing about liveness is inferred
            #  from key presence anywhere" — see the history-changed
            #  arm). A live turn gets re-adopted via the fresh snapshot
            #  (the engine becomes the only one deciding what this view
            #  marks running), WITHOUT any invented result text —
            #  "Nothing implicit" is a transcript rule. A finished turn
            #  is exactly the normal case here: clear silently and leave
            #  the survivor events (turn-result / history-changed) to
            #  reconcile the transcript. NO NOTE either way — the
            #  transcript is correct, and the banked lie ("turn did not
            #  survive") was the only thing wrong.
            if {![info exists ev_snap_seen($sid)] || !$ev_snap_seen($sid)} {
              if {[is_running $sid]} {
                variable ev_reconnected
                if {[info exists ev_reconnected($sid)] && $ev_reconnected($sid)} {
                    if {$session eq $sid} {
                        log_insert "\n[::vi18n::mc {⟳ the turn did not survive the gateway — showing what it persisted}]\n" system
                    }
                    dict unset turn_anchor $sid
                    dict unset turn_reload_anchor $sid
                    _drop_cache $sid
                } else {
                    # Asked asynchronously: this is a stream handler, and
                    # a synchronous GET here is a nested wait that every
                    # later stream line nests inside (the 2026-10-04
                    # hang). The mark is cleared now, as before; a reply
                    # saying the turn is live re-adopts it.
                    if {[catch {
                        ::vapi::get_json_async "/api/sessions/$sid/context" \
                            [list ::vchat::_live_check $sid]
                    } err]} {
                        ::vstatus::set_msg [format [::vi18n::mc {unreachable: %s}] $err]
                    }
                }
                _mark_running $sid 0
              }
            }
        }
        turn-started {
            set ev_retry($sid) 1000
            _flush_replay $sid
            _mark_running $sid 1
            # A desktop send already anchored + painted the user message
            # optimistically, so turn_reload_anchor is set and this is a
            # no-op. A window that did NOT start the turn — a second
            # desktop, or one watching an IRC/cron turn — never painted the
            # question, and the persisted copy sits at/below the cursor so
            # no replay re-sends it. turn-started carries the prompt so the
            # watcher shows the question above the reply about to stream.
            #
            # NO LONGER GATED on this being the session you are looking
            # at. Every loaded session has its own transcript, and the
            # question has to go in it: the anchors below are where
            # on_done splices the answer, so a background turn that
            # never anchored would land its reply in the wrong place —
            # or, before the append fallback, nowhere at all.
            set w [::vchat::log_for $sid]
            if {[winfo exists $w]} {
                set stream_session $sid
                set prompt_id 0
                catch { set prompt_id [dict get $d prompt_msg_id] }
                if {![dict exists $turn_reload_anchor $sid]} {
                    # Anchor BEFORE the prompt: a reconnect clears from here
                    # and the replay (cursor is pre-turn) repaints the copy
                    # the store now has.
                    dict set turn_reload_anchor $sid [$w index end-1c]
                    set prompt ""
                    catch { set prompt [dict get $d prompt] }
                    # Mail (veles_core::mail) is another session's words:
                    # painted as mail, never as something you typed.
                    set pinj ""
                    catch { set pinj [dict get $d injection] }
                    # Idempotency: the window that SENT this turn painted the
                    # prompt optimistically and set the anchor, so this is a
                    # no-op for it — the guard above. But the anchor can be
                    # missing on a fresh open, a second window, or a send path
                    # that forgot to set it (the custom-command renderer at
                    # chat.tcl:~2507 used to be one). Emitting the prompt
                    # unconditionally then DOUBLES it against the copy the
                    # starter already showed. So dedupe on content: if the
                    # last user block in THIS transcript is already this exact
                    # prompt, skip the emit and only take the anchor. A
                    # genuinely new question is never its own duplicate, and
                    # the reload path's replay is keyed by cursor, not by this
                    # check, so nothing real is ever suppressed.
                    if {$prompt ne ""} {
                        # With a durable id, text alone cannot identify
                        # the current turn: the user may ask the same thing
                        # twice. Only the unlabelled optimistic row (or the
                        # row with THIS id) is a duplicate then. Keep the
                        # old tail check for an older gateway.
                        set dup [expr {$prompt_id > 0
                            ? [_turn_prompt_row $w $prompt $prompt_id] ne "" : 0}]
                        if {!$dup} {
                            # A renderer without raw rows cannot use the
                            # durable-id lookup. Keep the old literal-tail
                            # fallback for that lightweight/older-client
                            # shape; with real rows, an older persisted
                            # identical prompt is NOT this turn's copy.
                            set has_rows 0
                            if {[array exists ::vchat::raw_w]} {
                                foreach _row [array names ::vchat::raw_w] {
                                    if {$::vchat::raw_w($_row) ne $w} { continue }
                                    if {![catch {$w compare msgstart_$_row < msgend_$_row} _visible]
                                        && $_visible} {
                                        set has_rows 1
                                        break
                                    }
                                }
                            }
                            if {!$has_rows || $prompt_id == 0} {
                                catch {
                                    # The tail of the transcript, wide enough to
                                    # hold the painted user block (body + the
                                    # "you  " header + its blank separators).
                                    # Clamped, because a transcript shorter than
                                    # the window would index before 1.0.
                                    set span [expr {[string length $prompt] + 80}]
                                    set from [$w index "end-1c -${span}c"]
                                    if {[$w compare $from < 1.0]} { set from 1.0 }
                                    set tail [$w get $from end-1c]
                                    # Literal text, not a glob pattern: the
                                    # prompt may contain *, [, or ?.
                                    if {[string first $prompt $tail] >= 0} { set dup 1 }
                                }
                            }
                        }
                        if {!$dup} {
                            if {[string match "mail:*" $pinj]} {
                                ::vchat::_mail_note $pinj $prompt
                            } else {
                                emit_message user $prompt
                            }
                        }
                    }
                }
                if {![dict exists $turn_anchor $sid]} {
                    dict set turn_anchor $sid [$w index end-1c]
                }
                # Record the prompt's durable row id even when the
                # existing anchored/text path above avoided a second
                # emit. A later rebuild drops the anchors but keeps the
                # raw row, so replay must use identity rather than text.
                if {$prompt_id > 0 && $prompt ne ""} {
                    variable raw_msgid
                    set painted [_turn_prompt_row $w $prompt $prompt_id]
                    if {$painted ne ""} {
                        set raw_msgid($painted) $prompt_id
                        dict set turn_prompt_ids $sid $prompt_id $painted
                    }
                }
            }
        }
        turn-ended {
            # The turn STOPPED. The verdict — turn-result or turn-error —
            # is published by run_turn itself a beat AFTER its guard
            # drops, for EVERY surface, and THAT clears running,
            # reconciles and heals the cache. Doing any of it here would
            # race a keystroke starting the next turn in the gap between
            # the two events (the pending result would delete the new
            # turn's region). If neither verdict ever arrives — a task
            # that died without unwinding — the next Stop press clears
            # the flag: cancel of a dead turn 404s, and "nothing to
            # stop" unsets the local busy mark.
        }
        turn-result {
            # The finished message, in the EXACT shape on_done already
            # consumed as the SSE `done` event — so the renderer is
            # unchanged and live matches reload.
            set stream_session $sid
            on_done $d
            set stream_session ""
            catch { set ev_cursor($sid) [dict get $d last_msg_id] }
            dict unset turn_reload_anchor $sid
        }
        turn-error {
            set stream_session $sid
            set msg "turn failed"
            catch { set msg [dict get $d error] }
            on_error $msg
            set stream_session ""
            dict unset turn_reload_anchor $sid
            # Whatever the failed turn persisted, no result event will
            # announce it — heal the cache so the next open shows it.
            _drop_cache $sid
        }
        delta {
            set stream_session $sid
            set t ""
            catch { set t [dict get $d text] }
            on_delta $t
            set stream_session ""
        }
        reasoning {
            set stream_session $sid
            set t ""
            catch { set t [dict get $d text] }
            on_reasoning $t
            set stream_session ""
        }
        route {
            set stream_session $sid
            set p "" ; set m "" ; set r ""
            catch { set p [dict get $d provider] }
            catch { set m [dict get $d model] }
            catch { set r [dict get $d reasoning_effort] }
            if {$r eq "null"} { set r "" }
            on_route $p $m $r
            set stream_session ""
            # A new provider call means the engine just landed a usage row;
            # refresh the context segment so a long turn's fill tracks
            # each round rather than waiting for turn end.
            catch { ::vstatus::step_refresh $sid }
        }
        tool {
            set stream_session $sid
            set st "" ; set nm ""
            catch { set st [dict get $d state] }
            catch { set nm [dict get $d name] }
            on_tool $st $nm $d
            set stream_session ""
            # A tool round completing is a provider round boundary: its usage
            # row is in. Refresh the context fill so the bar moves DURING a
            # multi-step turn, not only at its boundary.
            if {$st eq "end"} {
                catch { ::vstatus::step_refresh $sid }
            }
        }
        note {
            set stream_session $sid
            on_note $d
            set stream_session ""
        }
        shell-complete {
            # A background shell finished the MOMENT it happened — the
            # engine publishes this bus event at exit, so the owner
            # learns here instead of at the session's next turn
            # boundary (the model still gets its boundary-injected
            # [BACKGROUND SHELL COMPLETE] row; this is the display
            # half, like an engine note delivering as its own message).
            # Only the on-screen session's stream carries events at
            # all — the sidebar's session state is the off-screen
            # signal by design.
            set stream_session $sid
            set shellid ""
            catch { set shellid [dict get $d id] }
            set shcmd ""
            catch { set shcmd [dict get $d command] }
            set shcode "?"
            catch { set shcode [dict get $d exit_code] }
            if {$session eq $sid} {
                if {$shcode eq 0} {
                    _log_completion $sid "✓ shell $shellid finished: $shcmd\n"
                } else {
                    _log_completion $sid "⚠ shell $shellid finished (exit $shcode): $shcmd\n"
                }
                catch { ::vstatus::set_msg [format [::vi18n::mc {shell %1$s finished}] $shellid] }
            }
            set stream_session ""
        }
        child-complete {
            # A subagent child finished the MOMENT it happened. Same
            # shape as shell-complete: the model still gets its
            # boundary-injected [BACKGROUND CHILD COMPLETE] row, this is
            # the DISPLAY half. A child is a real session (USER
            # taxonomy, 2026-09-17), so the row names it and offers to
            # attach — and because the child session just gained its
            # final messages, the sidebar is refreshed so its row and
            # count are current.
            set stream_session $sid
            set cid ""
            catch { set cid [dict get $d id] }
            set cgoal ""
            catch { set cgoal [dict get $d goal] }
            set cok ""
            catch { set cok [dict get $d ok] }
            set csession ""
            catch { set csession [dict get $d child_session] }
            if {$session eq $sid} {
                if {$cok} {
                    _log_completion $sid "✓ subagent $cid finished: $cgoal\n"
                } else {
                    _log_completion $sid "⚠ subagent $cid ended (not ok): $cgoal\n"
                }
                if {$csession ne ""} {
                    # Name the child session so "attach and review" has a
                    # target: /resume <id> from the composer, or the
                    # tasks overlay's [attach] link. log_insert renders
                    # plain text (no inline click tags), so this is the
                    # id to copy, not a button.
                    _log_completion $sid "  ↳ review it: /resume $csession\n"
                }
                catch { ::vstatus::set_msg [format [::vi18n::mc {subagent %1$s finished}] $cid] }
            }
            catch { ::vsidebar::refresh_soon }
            set stream_session ""
        }
        monitor-fired {
            # A monitor's condition was met (or it gave up at its
            # deadline). The model gets its [MONITOR FIRED] injection at
            # the boundary; this says so on screen. No attach link — a
            # monitor has no session of its own.
            set stream_session $sid
            set mid ""
            catch { set mid [dict get $d id] }
            set mcond ""
            catch { set mcond [dict get $d condition] }
            set mstatus ""
            catch { set mstatus [dict get $d status] }
            if {$session eq $sid} {
                if {$mstatus eq "fired"} {
                    _log_completion $sid "⏰ monitor $mid fired: $mcond\n"
                } else {
                    _log_completion $sid "⏰ monitor $mid ended ($mstatus): $mcond\n"
                }
                catch { ::vstatus::set_msg [format [::vi18n::mc {monitor %1$s fired}] $mid] }
            }
            set stream_session ""
        }
        ask {
            # The stream's own session, so an ask payload that does not
            # name one (an older gateway) still resolves to the session
            # whose stream carried it rather than to whatever is on
            # screen.
            set stream_session $sid
            ::vask::show $d
            set stream_session ""
        }
        ask-resolved {
            # Answered somewhere else — another window, telegram, REST —
            # or withdrawn with the turn. The dialog closes WITHOUT
            # posting an answer of its own.
            #
            # This branch did not exist. api.tcl dispatches these two on
            # its handler-less SSE arm, and the chat stream never takes
            # it (session_events always registers a handler), so both
            # events were dropped: a question answered on the phone left
            # this window's dialog up, and a second answer would have
            # POSTed a decide on a spent id.
            set stream_session $sid
            if {[catch { ::vask::resolved $d } resolved_err]} {
                puts stderr "veles-desktop: ask-resolved handler failed: $resolved_err"
                # The dialog is stale either way — force-close it so
                # the user cannot answer a spent id.
                catch { grab release .ask }
                catch { destroy .ask }
            }
            set stream_session ""
        }
        ask-relayed {
            # The question has ALSO been put to the owner's other
            # surface because nobody answered it here. Said out loud, on
            # the surface that asked — a question quietly forwarded to
            # somebody's phone is exactly the silent automation this
            # codebase refuses.
            set stream_session $sid
            catch { ::vask::relayed $d }
            set stream_session ""
        }
        cancelled {
            # The turn was interrupted. turn-result (interrupted) carries
            # the reconcile; this is just the status, so it does not race
            # the transcript.
            catch { ::vstatus::set_msg [::vi18n::mc {turn interrupted}] }
        }
        history-changed {
            # The transcript changed OUTSIDE a turn: compaction finished,
            # /clear, /undo, /branch. The store is the truth; the event
            # is the finish signal.
            #
            # This REPLACES `_compact_poll`, which watched /context's
            # `busy` key and was structurally blind: busy means "a turn
            # is live" (a session_tap snapshot), and a compaction holds
            # the agent lock without being a turn — so the key said
            # false from the summarizer's first tick, and "absence of
            # busy" could never have meant done anyway. Nothing about
            # liveness is inferred from key presence anywhere.
            catch { set ev_cursor($sid) [dict get $d last_msg_id] }
            _mark_running $sid 0
            # The store head moved (or rows were archived at a head the
            # cursor does not describe): repaint from it. `_drop_cache`
            # alone only forgot the parsed HISTORY — the widget kept the
            # pre-change text, and open_session's kept branch returns
            # without painting, so this arm said "reloading" while
            # nothing did. reload_session drops both, for a hidden
            # session as well as the one on screen.
            reload_session $sid
            if {$session eq $sid} {
                # AFTER the repaint, in past tense, because now it is
                # true: said before it, the repaint ate the line.
                set reason ""
                catch { set reason [dict get $d reason] }
                if {$reason eq "compaction"} {
                    log_insert "\n[::vi18n::mc {· compaction finished — transcript reloaded}]\n" status
                }
                # /context answers with real figures (it never needed the
                # lock, only its own snapshot) — repaint the fill now.
                catch { ::vstatus::refresh_context $sid }
            }
            catch { ::vsidebar::refresh_soon }
        }
        session-lagged {
            # The engine dropped events rather than pretend. The store has
            # them; reconnect from the cursor refetches exactly the gap.
            catch { ::vstatus::set_msg [::vi18n::mc {session stream fell behind, refetching}] }
            _events_reconnect $sid
        }
        closed {
            # Defense in depth: a stream WE reset must not read as a
            # drop. The api layer already silences reset-ends (the
            # teardown's own http::reset runs its -command synchronously
            # and used to arm a phantom reconnect from inside the close —
            # the once-a-second reconnect storm that flickered the
            # transcript and sidebar through every live turn); this is
            # the same rule on the consumer side.
            set st ""
            catch { set st [dict get $d status] }
            if {$st eq "reset"} { return }
            # Only forget the CURRENT stream: a stale token's late close
            # (already replaced by a switch or a reconnect) must not
            # clobber the stream that is live now.
            set tok ""
            catch { set tok [dict get $d token] }
            if {$tok eq "" || ([info exists ev_stream($sid)] && $ev_stream($sid) eq $tok)} {
                set ev_stream($sid) ""
            }
            # A rejected token is not a transient outage; retrying it on a
            # backoff is a slower way of failing (irc.tcl learned this the
            # hard way — a blank pane that said nothing).
            set code 0
            catch { set code [dict get $d code] }
            if {[::vapi::fatal_code $code]} {
                set why [::vapi::http_reason $code]
                # In the TRANSCRIPT, not only the status bar. A 404 here
                # means the gateway predates the events endpoint — exactly
                # the version-skew failure that once stayed invisible
                # through a whole debugging session while every send
                # silently timed out against an engine that spoke SSE.
                if {$session eq $sid} {
                    log_insert "\n[format [::vi18n::mc {✖ session stream: %s}] $why]\n" system
                }
                catch { ::vstatus::set_msg [format [::vi18n::mc {session: %s}] $why] }
                return
            }
            _events_schedule_retry $sid
        }
    }
}

# Paint the buffered replay rows as one batch through the SAME windowed
# row renderer a full open uses, so tool-call args correlate and a resumed
# stream matches a fresh load. Rows go into the session's OWN transcript,
# on screen or not — nothing else would ever re-send them.
proc ::vchat::_flush_replay {sid} {
    variable ev_replay
    variable ev_reconnect_pending
    variable turn_reload_anchor
    variable turn_prompt_ids
    variable turn_anchor
    variable stream_session
    # INTO THIS SESSION'S OWN TRANSCRIPT, on screen or not. This used to
    # read `::vchat::log` and give up when the session was not the one in
    # front of you — the last of the one-transcript rule that for_display
    # retired everywhere else. The rows were dropped on the floor while
    # session-live advanced that session's cursor anyway, so nothing was
    # ever going to re-send them, and coming back took open_session's
    # KEPT branch and repainted nothing. A background session's replay
    # belongs in its own widget, exactly like its deltas.
    set w [::vchat::log_for $sid]
    set live [expr {$w ne "" && [winfo exists $w]}]
    # Reopening after a DROP: clear the stale live turn region (the
    # optimistic user message plus whatever streamed before the socket
    # died) BEFORE repainting, so the replay from the pre-turn cursor
    # restores ONE copy from the store instead of doubling it. This runs
    # only once the replay actually arrives — during the outage nothing is
    # cleared, so the view is never blanked while the gateway is down.
    set cleared 0
    set reconnecting 0
    if {[info exists ev_reconnect_pending($sid)] && $ev_reconnect_pending($sid)} {
        set reconnecting 1
        set ev_reconnect_pending($sid) 0
        if {$live && [dict exists $turn_reload_anchor $sid]} {
            _writable $w {
                $w delete [dict get $turn_reload_anchor $sid] end
            }
            # The narrow on_done anchor pointed INTO the region just
            # deleted; drop it so the snapshot/turn-started that follows
            # re-sets it against the repainted transcript.
            #
            # `variable turn_anchor` is what makes that comment true.
            # Without the declaration `turn_anchor` was a LOCAL here, the
            # unset threw, the catch ate it, and the anchor survived every
            # reconnect still pointing at text that no longer existed — so
            # on_done cut the repainted transcript at a boundary measured
            # before the drop.
            catch { dict unset turn_anchor $sid }
            set cleared 1
        }
    }
    set had_replay [expr {[info exists ev_replay($sid)] && [llength $ev_replay($sid)] > 0}]
    if {$had_replay} {
        set rows $ev_replay($sid)
        set ev_replay($sid) {}
        # If a rebuild lost the positional anchor, the raw prompt's id is
        # still a durable boundary. Remove the optimistic prompt and the
        # transient tail after it, then paint the whole replay in STORE
        # ORDER. Skipping just the prompt would move it below provenance
        # rows and leave the next reconnect boundary before later context.
        #
        # With a valid anchor the delete above already removed the live
        # tail, so this case does not arise. The store rows are always the
        # copy of record; nothing here dedupes by message text.
        if {$reconnecting && ![dict exists $turn_reload_anchor $sid]
            && [dict exists $turn_prompt_ids $sid]} {
            variable raw
            variable raw_w
            set reset_from ""
            foreach row $rows {
                set role ""
                set mid ""
                catch { set role [dict get $row role] }
                catch { set mid [dict get $row msg_id] }
                if {$role ne "user" || $mid eq ""
                    || ![dict exists $turn_prompt_ids $sid $mid]} { continue }
                set reset_from [_painted_user_msgid $w $mid]
                if {$reset_from ne ""} { break }
            }
            if {$reset_from ne ""} {
                set ids [lsort -integer [array names raw]]
                set at [lsearch -exact -integer $ids $reset_from]
                _writable $w { $w delete msgstart_$reset_from end }
                if {$at >= 0} {
                    foreach id [lrange $ids $at end] {
                        if {[info exists raw_w($id)] && $raw_w($id) eq $w} {
                            _forget_raw_entry $id $w
                        }
                    }
                }
                # Keep the ID map: the history painter below gives its
                # replacement raw row the same persisted message id.
                catch { dict unset turn_anchor $sid }
            }
        }
        if {$live} {
            # The paint chokepoints resolve their widget through
            # event_session, like every other arm of on_session_event;
            # say which session this batch belongs to.
            set _prev $stream_session
            set stream_session $sid
            if {[catch { _paint_history_rows $rows 0 [llength $rows] } e]} {
                log_insert "[format [::vi18n::mc {failed to paint replayed history: %s}] $e]\n" system
            }
            # A replayed ordinary user row has its persisted id attached
            # by the painter. Keep the turn's map pointed at the NEW raw
            # row after a reset, so a later replay can find that row too.
            foreach row $rows {
                set role ""
                set mid ""
                catch { set role [dict get $row role] }
                catch { set mid [dict get $row msg_id] }
                if {$role ne "user" || $mid eq ""} { continue }
                set painted [_painted_user_msgid $w $mid]
                if {$painted ne "" && [dict exists $turn_prompt_ids $sid]} {
                    dict set turn_prompt_ids $sid $mid $painted
                }
            }
            set stream_session $_prev
        }
    }
    # THE RELOAD ANCHOR IS WHERE THE PERSISTED TRANSCRIPT ENDS, and the
    # replay just moved that. It used to be set ONCE, before the
    # optimistic user message (send_current), and never again — while
    # every reconnect's session-live advances ev_cursor to the store
    # HEAD. So the second drop in one turn deleted from before the user's
    # question and replayed from after it, and nothing in the transcript
    # ever put it back. USER 2026-09-18: "my own second prompt got
    # swallowed when I told session … to continue after a network error".
    #
    # Re-anchored here the invariant holds again: everything after the
    # anchor is either a row the next replay will re-send or live text
    # the next snapshot will rebuild.
    if {$live && ($cleared || $had_replay)} {
        # After the persisted replay, but BEFORE the snapshot paints its
        # live partial. An EMPTY replay on a later drop must preserve the
        # existing boundary, not move it past the old partial.
        dict set turn_reload_anchor $sid [$w index end-1c]
    }
}

# Reconstruct the live region from the engine's snapshot of the turn in
# flight — the text streamed before this window attached. Feeds the SAME
# live handlers a real-time stream does, so it renders identically and
# turn-result's reconcile replaces it exactly as it would theirs. Called
# with stream_session already set to $sid.
proc ::vchat::_paint_snapshot {d} {
    # IN ORDER. This used to paint the reasoning, then the content, then
    # every tool round — so a turn that actually went
    # think→call→think→call→answer was rebuilt as one lump of thinking,
    # the answer, and then the calls. Wrong even for what the snapshot
    # carried, and the snapshot carried only the last round anyway (the
    # engine cleared the rest at each ToolEnd; both halves fixed
    # 2026-09-16).
    set rounds {}
    catch { set rounds [dict get $d rounds] }
    foreach r $rounds {
        # Each round is a dict: the reasoning and text that led to the
        # call, then the call itself.
        set rr "" ; catch { set rr [dict get $r reasoning] }
        set rc "" ; catch { set rc [dict get $r content] }
        set nm "" ; catch { set nm [dict get $r name] }
        set ra "" ; catch { set ra [dict get $r args] }
        set rs "" ; catch { set rs [dict get $r result] }
        if {$rr ne ""} { on_reasoning $rr }
        if {$rc ne ""} { on_delta $rc }
        if {$nm eq ""} { continue }
        on_tool "start" $nm [dict create name $nm args $ra]
        if {$rs ne ""} {
            on_tool "end" $nm [dict create name $nm result $rs]
        }
    }
    # …then the live tail: everything since the last round closed.
    set reasoning "" ; catch { set reasoning [dict get $d reasoning] }
    set content ""   ; catch { set content   [dict get $d content] }
    if {$reasoning ne ""} { on_reasoning $reasoning }
    if {$content ne ""}   { on_delta $content }
}

proc ::vchat::turn_error {msg} {
    variable live_rtag
    variable live_rhdr
    variable live_rtext
    variable live_blocks
    variable live_ptext
    variable live_extra
    variable turn_anchor
    variable turn_prompt_ids
    set sid [event_session]
    _live_ctx $sid
    if {1} {
        # A FAILED TURN IS STILL A TURN (R3). The store holds its rows —
        # everything the turn streamed before the last provider call
        # died, plus `[veles] all routes failed` as an ordinary
        # assistant row (agent.rs pushes it before returning Err). The
        # live region holds the same streamed content raw: live_blocks
        # this turn accumulated, NOT {} — wiping them painted the
        # store's copy over the raw one. While live ≠ reload for every
        # failed turn, C-S-V looked like it "corrupted" the transcript:
        # the reasoning survived as prose but lost its live_r_* colour
        # and fold (43fb99b's reap), and the error row differed too.
        # So run the SAME ladder on_done runs: flush what the turn
        # streamed, settle the prose, and let _reconcile_turn replace
        # the raw region with the rows the store kept. A failed turn
        # passes reply "" — its streamed prose is the copy of record,
        # nothing replaces it — and no display_reasoning fallback: the
        # done path has one because the event carries it, the error
        # path does not, and the accumulator already holds every burst
        # the turn showed.
        _flush_live_reasoning $sid
        _live_block_settle
        ::vchat::_reset_live_delta $sid
        _reconcile_turn $sid "" ""
        # …and the region is spent either way: the reconcile replaced
        # it with the reconciled blocks.
        set live_rtag ""
        set live_rhdr ""
        set live_rtext ""
        set live_blocks {}
        set live_ptext ""
        catch { dict unset live_extra $sid }
        variable live_tool_marks
        catch { dict unset live_tool_marks $sid }
        # THE ERROR ITSELF, as a transcript row — not a status-bar line
        # only, and not the store's `[veles] all routes failed`
        # assistant row. Until now the only place the error text lived
        # was the status bar, which is where the "errors are missing /
        # merged into model output" report came from: a failure that
        # streamed nothing left an empty reconcile and one status line,
        # and a failure after streaming left the engine's error row
        # reading as the model's own words. `system`, not `status`:
        # status is a low-contrast chrome line for notes and
        # completions, and a turn that failed is not chrome. Put it in
        # the transcript so it survives a reload and reads as an error,
        # exactly where the turn would have left its answer.
        emit_message system [::vi18n::mc {✖ turn failed: %s} $msg]
        ::vstatus::set_msg [format [::vi18n::mc {error: %s}] $msg]
    }
    dict unset turn_anchor $sid
    catch { dict unset turn_prompt_ids $sid }
    _mark_running $sid 0
}

# ── widget construction ──────────────────────────────────────────────
# ── one transcript per session ───────────────────────────────────────
#
# Which widget does a paint go into? `event_session` already answered
# the harder half of that question — "which session does this event
# belong to", as opposed to "is that the session on screen". Every paint
# site now asks `log`, which asks `event_session`, so a stream callback
# writes into ITS session's transcript whether or not that transcript is
# the one you are looking at, and a window operation writes into the one
# you are.

# The transcript a paint belongs in.
proc ::vchat::log {} {
    return [log_for [event_session]]
}

# One session's transcript, or "" when it has none.
#
# THE FALLBACK IS NARROW ON PURPOSE. The base widget is what `build`
# makes: the welcome text before any session is open, and the widget
# every test that hand-builds a transcript at the historical path is
# talking to. It is also, at any moment, the CURRENT view's — so handing
# it to a session that has no transcript of its own is precisely how one
# session's output lands in another's, which is the bug this whole file
# spent a release closing. A background session with no widget gets "",
# and the paint chokepoints below treat that as "nothing to paint into".
proc ::vchat::log_for {sid} {
    variable logs
    variable session
    if {$sid ne "" && [dict exists $logs $sid]} { return [dict get $logs $sid] }
    if {$sid eq "" || $sid eq $session} { return .mid.chatwrap.chat.log }
    return ""
}

# Every live transcript — what a theme switch has to re-colour, not just
# the one on screen.
proc ::vchat::logs_all {} {
    variable logs
    set out {}
    foreach {sid w} $logs {
        if {[winfo exists $w]} { lappend out $w }
    }
    if {[winfo exists .mid.chatwrap.chat.log]
        && [lsearch -exact $out .mid.chatwrap.chat.log] < 0} {
        lappend out .mid.chatwrap.chat.log
    }
    return $out
}

# Give $sid a transcript. The base widget goes to whoever asks first —
# it already exists, it is empty, and handing it out means a
# single-session window creates no extra widgets at all.
proc ::vchat::_claim_log {sid} {
    variable logs
    variable logs_seq
    if {[dict exists $logs $sid]} { return [dict get $logs $sid] }
    set taken {}
    foreach {_s _w} $logs { lappend taken $_w }
    if {[lsearch -exact $taken .mid.chatwrap.chat.log] < 0} {
        set w [::vchat::log]
        if {![winfo exists $w]} { _make_log $w }
    } else {
        set w [::vchat::log][incr logs_seq]
        _make_log $w
    }
    # Armed HERE rather than only in _make_log: the base widget may have
    # been created by someone else (a test harness's bare `text` call,
    # an embedder) and _make_log then never runs for it — a transcript
    # whose scroller never touches the latch. Every claimed transcript
    # passes through here whoever built it; _arm_pose is idempotent.
    if {[catch { _arm_pose $w } pose_err]} {
        puts stderr "veles-desktop: _claim_log: _arm_pose failed: $pose_err"
    }
    dict set logs $sid $w
    return $w
}

# Put $sid's transcript on screen and take the previous one off.
#
# PITFALL 18: a second `pack` with different flags silently drops the
# ones it omits, so the flags live here once and nowhere else. The
# scrollbar is re-pointed both ways — a scrollbar still driving a widget
# nobody can see scrolls nothing and reports the wrong thumb.
proc ::vchat::_show_log {sid} {
    variable logs
    set w [log_for $sid]
    if {![winfo exists $w]} { return }
    foreach other [logs_all] {
        if {$other ne $w} { catch { pack forget $other } }
    }
    if {[winfo exists .mid.chatwrap.chat.sb]} {
        catch { .mid.chatwrap.chat.sb configure -command [list $w yview] }
        catch { $w configure -yscrollcommand [list .mid.chatwrap.chat.sb set] }
        # A scrollbar drag is a user scroll this widget's own bindings
        # never see. The release is where the latch is read — reading
        # during motion would race the drag itself.
        catch {
            bind .mid.chatwrap.chat.sb <ButtonRelease-1> \
                {+after idle {catch {::vchat::_read_pose}}}
        }
    }
    if {[catch {
        pack $w -side left -fill both -expand 1 -padx {4 0} -pady {4 0}
    } pack_err]} {
        puts stderr "veles-desktop: _show_log: pack failed for $sid: $pack_err"
    }
    return $w
}

# LRU touch, then evict past the cap.
#
# Never the session on screen, and never one with a turn in flight:
# dropping a busy session's transcript would silently stop watching a
# running turn, which is the opposite of what keeping them loaded is for.
proc ::vchat::_touch_log {sid} {
    variable logs_lru
    variable logs_cap
    variable session
    set logs_lru [lsearch -all -inline -not -exact $logs_lru $sid]
    lappend logs_lru $sid
    variable logs
    while {[dict size $logs] > $logs_cap} {
        set victim ""
        foreach cand $logs_lru {
            if {$cand eq $session} { continue }
            if {![dict exists $logs $cand]} { continue }
            if {[is_running $cand]} { continue }
            set victim $cand
            break
        }
        if {$victim eq ""} { break }
        set label $victim
        catch {
            set l [::vsidebar::label_for $victim]
            if {$l ne ""} { set label $l }
        }
        _drop_log $victim
        # RECORDED, not said. `_touch_log` runs on the way INTO an open,
        # and the open's own status line ("session <id>") lands after
        # it — so saying it here would post a message the user is
        # guaranteed never to see. The tail folds it into the one line
        # the switch does write.
        variable last_eviction
        set last_eviction [format \
            [::vi18n::mc {unloaded %1$s (%2$s kept)}] $label $logs_cap]
    }
}

# Destroy one session's transcript and everything that pointed into it.
proc ::vchat::_drop_log {sid} {
    variable logs
    variable logs_lru
    if {![dict exists $logs $sid]} { return }
    _forget_anchors $sid
    set w [dict get $logs $sid]
    # Named Tk images are never garbage-collected: destroying the widget
    # unmaps them and frees nothing (clear's comment, one layer up).
    catch { ::vmedia::forget $w }
    catch { ::vmd::forget_links $w }
    catch { forget_raw $w }
    dict unset logs $sid
    set logs_lru [lsearch -all -inline -not -exact $logs_lru $sid]
    variable paint_hidden
    catch { unset paint_hidden($sid) }
    # Where it was sitting dies with the widget: a remembered live edge
    # for a transcript that has been wiped would jump a freshly repainted
    # one to the bottom of somebody else's scroll position.
    variable at_edge
    catch { unset at_edge($sid) }
    # Its live region goes with the widget it painted into — a stashed
    # region for a transcript that no longer exists would be restored
    # the next time that sid streams and hand it marks and tags from a
    # destroyed widget.
    catch { _live_forget $sid }
    # The stream too. A connection whose transcript no longer exists
    # paints into nothing and holds a socket for the privilege; the
    # session reconnects from its cursor when it is next opened.
    catch { _events_disconnect $sid }
    variable ev_cursor
    catch { unset ev_cursor($sid) }
    # The BASE widget is never destroyed — it is the fallback log_for
    # hands out, and the app would lose its empty state with it. Wipe it
    # instead; a later session claims it again.
    if {$w eq ".mid.chatwrap.chat.log"} {
        catch { _writable $w { $w delete 1.0 end } }
    } else {
        catch { destroy $w }
    }
    # The pose interceptor's forwarding proc goes with the widget: the
    # real command died with `destroy` above (or was wiped for the base
    # widget), and an orphan that forwards into a dead command would
    # throw on the next call instead of simply not existing. ::-qualified
    # (see _arm_pose). The base widget keeps its interceptor: the widget
    # survives (only wiped), _claim_log hands it to the next session,
    # and _arm_pose is idempotent.
    if {$w ne ".mid.chatwrap.chat.log"} {
        catch { rename ::$w "" }
        catch { rename ::${w}._pose "" }
    }
}

# Build ONE transcript widget at $w: the text, its read-only wrapper,
# its twelve tags and its context menu.
#
# Extracted from `build` verbatim so that a second, third and twelfth
# session get a transcript configured EXACTLY like the first. A widget
# built any other way is a transcript that renders differently depending
# on which session you opened first, which is the kind of divergence
# nobody notices until a screenshot.
proc ::vchat::_make_log {w} {
    text $w -wrap word -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -font {TkDefaultFont 11} -spacing1 2 -spacing3 2 \
        -insertbackground $::vt::T(pink) -highlightthickness 0 -relief flat -bd 0
    catch { ::va11y::name $w [::vi18n::mc {conversation transcript}] }
    # Read-only, but still a real buffer: visible caret, working point
    # motion, selectable, focusable. `-state disabled` gave none of
    # those and silently ate inserts on top.
    ::vtext::readonly $w
    # USER SCROLL UPDATES THE LATCH, and nothing else may. Every way a
    # human moves this transcript — scrollbar drag and release, mouse
    # wheel, keyboard scroll keys, selection drag — funnels through one
    # helper bound at this widget's construction. Geometry noise (a
    # pane docking beside it, a sash move) re-wraps lines and moves the
    # view without any of these firing, which is precisely why the
    # latch exists: pose read after a re-wrap is a lie, and the user's
    # last expressed intent is the truth.
    ::vchat::_arm_pose $w

    $w tag configure user -foreground $::vt::T(user) -font {TkDefaultFont 11 bold}
    $w tag configure userhdr -foreground $::vt::T(cyan) -font {TkDefaultFont 11 bold}
    $w tag configure assistant -foreground $::vt::T(assistant)
    $w tag configure reasoning -foreground $::vt::T(reasoning) -font {TkDefaultFont 10 italic}
    $w tag configure tool -foreground $::vt::T(tool) -font {monospace 10}
    # Diff lines inside a tool round (a `patch` result). Same colours as
    # the review pane's diff (panes.tcl), so a patch reads the same in
    # both places.
    $w tag configure diffadd -foreground #50fa7b -font {monospace 10}
    $w tag configure diffdel -foreground #ff5555 -font {monospace 10}
    $w tag configure diffhdr -foreground $::vt::T(cyan) \
        -font {monospace 10 bold}
    $w tag configure system -foreground $::vt::T(system) -font {TkDefaultFont 10 italic}
    $w tag configure status -foreground $::vt::T(fg_dim) -font {TkDefaultFont 9}
    # Engine notes: present but quiet. Always visible, never shouting —
    # they must not read as content, and must not be missable either.
    $w tag configure note -foreground $::vt::T(fg_dim) \
        -font {TkDefaultFont 9} -lmargin1 2 -lmargin2 4 -spacing1 2
    $w tag configure note_more -foreground $::vt::T(accent) \
        -font {TkDefaultFont 9}
    # When a block happened: quiet, like a note, and never mistaken for
    # content. The full date and time is one hover away.
    $w tag configure stamp -foreground $::vt::T(fg_dim) -font {TkDefaultFont 9}
    $w tag configure stampday -foreground $::vt::T(fg_dim) -font {TkDefaultFont 9} \
        -justify center -spacing1 4 -spacing3 2
    $w tag bind stamp <Enter> [list ::vchat::_stamp_hover $w %x %y]
    # Markdown source view: monospace, muted, so it reads as "this is
    # the text behind the rendering" rather than as content.
    $w tag configure mdsource -foreground $::vt::T(fg_dim) \
        -font {monospace 10} -lmargin1 4 -lmargin2 4
    # Right-click a message to flip just that one. ::vctx captures the
    # click position (and offers a Menu-key equivalent) so this no
    # longer keeps its own _ctx_x/_ctx_y globals.
    ::vctx::attach $w {
        {"View source / rendered" {::vchat::toggle_source_at $::vctx::x $::vctx::y}}
        {"Copy message"           {::vchat::copy_message_at $::vctx::x $::vctx::y}}
        {-}
        {"Find in transcript…"    {::vfind::show}}
        {"Jump to composer"       {focus .mid.chatwrap.chat.input}}
    }
    return $w
}

proc ::vchat::build {} {
    variable show_reasoning

    ttk::frame .mid.chatwrap.chat
    _make_log .mid.chatwrap.chat.log
    ttk::scrollbar .mid.chatwrap.chat.sb -orient vertical -command [list .mid.chatwrap.chat.log yview]
    .mid.chatwrap.chat.log configure -yscrollcommand [list .mid.chatwrap.chat.sb set]

    ttk::frame .mid.chatwrap.chat.bar
    # The composer deliberately differs from the transcript: panel-shade
    # background (vs the transcript's input-shade) + a hairline border
    # that lights up accent-coloured on focus, so you always see where
    # the box starts.
    text .mid.chatwrap.chat.input -height 3 -wrap word -font {TkDefaultFont 11} \
        -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -insertbackground $::vt::T(pink) \
        -highlightthickness 1 -highlightbackground $::vt::T(hairline) \
        -highlightcolor $::vt::T(accent) -relief flat -bd 2 \
        -undo 1 -autoseparators 1 -maxundo 200
    catch { ::va11y::name .mid.chatwrap.chat.input [::vi18n::mc {message composer}] }
    catch { ::va11y::name .mid.chatwrap.chat.preview [::vi18n::mc {markdown preview of the composer}] }
    ::vctx::attach .mid.chatwrap.chat.input {
        {"Cut"    {event generate .mid.chatwrap.chat.input <<Cut>>}}
        {"Copy"   {event generate .mid.chatwrap.chat.input <<Copy>>}}
        {"Paste"  {event generate .mid.chatwrap.chat.input <<Paste>>}}
        {-}
        {"Select all" {.mid.chatwrap.chat.input tag add sel 1.0 end}}
        {"Clear composer" {.mid.chatwrap.chat.input delete 1.0 end}}
        {-}
        {"Toggle preview" {::vchat::toggle_preview}}
    }

    ttk::button .mid.chatwrap.chat.bar.send -text [::vi18n::mc {Send}] -style Accent.TButton \
        -command ::vchat::send_current
    ttk::button .mid.chatwrap.chat.bar.stop -text [::vi18n::mc {Stop}] -state disabled -command ::vchat::stop_current
    # Detach: /cancel's gentle sibling (USER taxonomy, 2026-09-17 —
    # "there should also be a way for the user to move a running
    # foreground shell into the background"). Arms the engine-side
    # flag; the terminal call inside the running turn adopts its live
    # process as a background shell and says so in the transcript.
    ttk::button .mid.chatwrap.chat.bar.detach -text [::vi18n::mc {Detach}] -state disabled \
        -command ::vchat::detach_current
    # USER: "Is image/file support implemented at all? … no option to
    # even attach". There was none: `/image` printed "image noted" and
    # attached nothing. This is the affordance that was missing.
    ttk::button .mid.chatwrap.chat.bar.attach -text "📎" -width 3 \
        -command ::vchat::attach_dialog
    ttk::checkbutton .mid.chatwrap.chat.bar.reason -text [::vi18n::mc {reasoning}] -variable ::vchat::show_reasoning \
        -command ::vchat::_toggle_live_reasoning
    # Follow toggle: armed = live output pulls the view along (and only
    # while you are already at the bottom); off = reading wins and the
    # floating "▾ new output" button is the way back.
    ttk::checkbutton .mid.chatwrap.chat.bar.follow -text [::vi18n::mc {follow}] -variable ::vchat::follow \
        -command ::vchat::_toggle_follow
    pack .mid.chatwrap.chat.bar.send .mid.chatwrap.chat.bar.stop .mid.chatwrap.chat.bar.detach -side right -padx 2 -pady 2
    pack .mid.chatwrap.chat.bar.attach -side right -padx 2
    pack .mid.chatwrap.chat.bar.reason .mid.chatwrap.chat.bar.follow -side right -padx 8
    catch { ::va11y::name .mid.chatwrap.chat.bar.attach [::vi18n::mc {attach a file}] }
    catch { ::va11y::name .mid.chatwrap.chat.bar.reason \
        [::vi18n::mc {show the model's reasoning blocks (same as /reasoning)}] }
    catch { ::va11y::name .mid.chatwrap.chat.bar.follow \
        [::vi18n::mc {keep scrolling to the newest output as it arrives}] }
    catch { ::va11y::name .mid.chatwrap.chat.bar.send \
        [::vi18n::mc {send the message; while a turn runs, queue it as steering}] }
    catch { ::va11y::name .mid.chatwrap.chat.bar.stop \
        [::vi18n::mc {cancel the running turn}] }
    catch { ::va11y::name .mid.chatwrap.chat.bar.detach \
        [::vi18n::mc {move the running foreground command to the background (keeps running)}] }

    # The chips: what is going out with the next message. Packed only
    # when there is something to show — an always-present empty strip
    # above the composer is just lost height.
    ttk::frame .mid.chatwrap.chat.chips

    # Composer resize grip: drag vertically to grow/shrink the input
    # box (1..25 lines). Persisted via ::vdock::save_layout.
    ttk::frame .mid.chatwrap.chat.grip -cursor sb_v_double_arrow
    pack .mid.chatwrap.chat.grip -side bottom -fill x -ipady 1
    bind .mid.chatwrap.chat.grip <Button-1> { ::vchat::_grip_start %Y }
    bind .mid.chatwrap.chat.grip <B1-Motion> { ::vchat::_grip_drag %Y }
    bind .mid.chatwrap.chat.grip <ButtonRelease-1> { ::vchat::_grip_done }

    # Live markdown preview of what you're typing (Ctrl-P). Read-only,
    # rendered by the same ::vmd renderer as the transcript, so what you
    # see here is exactly what the reply pane would show.
    text .mid.chatwrap.chat.preview -height 5 -wrap word \
        -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -font {TkDefaultFont 11} -highlightthickness 0 -relief flat -bd 0

    pack .mid.chatwrap.chat.input -in .mid.chatwrap.chat.bar -side left -fill both -expand 1 -padx {2 4} -pady 2
    pack .mid.chatwrap.chat.bar -side bottom -fill x -padx 4 -pady {0 4}
    # The composer band is off-limits to floating tooltips: the bar holds
    # the input AND its buttons, chips sits above it, preview above that,
    # grip below. Hovering the transcript used to drop its accessible name
    # on the composer's first line (USER 2026-09-06). Zone geometry is
    # re-read per tip, so this survives resizes, preview toggles and a
    # composer dragged to 25 lines.
    #
    # Two halves of one rule, and the list has to be exact for both to
    # hold. A registered widget gets NO tip of its own (a "message
    # composer" card on the composer is this bug in a different label),
    # while an UNregistered control merely inside a registered rectangle
    # keeps its hint and gets placed clear of the band - which is why
    # `.input` is listed alongside `.bar`: the bar is the rectangle, the
    # input is the surface. Register the transcript's `.log` here instead
    # and the pane loses its tooltip outright.
    foreach _k {.mid.chatwrap.chat.input .mid.chatwrap.chat.bar
                .mid.chatwrap.chat.chips .mid.chatwrap.chat.preview
                .mid.chatwrap.chat.grip} {
        ::va11y::keepout $_k
    }
    pack .mid.chatwrap.chat.sb -side right -fill y -padx {0 4} -pady {4 0}
    _show_log ""
    # .mid.chatwrap.chat itself: last child of root, fills what .statusbar (bottom)
    # and .sidebar (left) left over — one pack call (pitfall 18).
    pack .mid.chatwrap.chat -side left -fill both -expand 1 -padx {0 6} -pady 6

    bind .mid.chatwrap.chat.input <Return> {
        # Completion popup open → accept the highlight, don't send.
        if {[::vcomp::open]} { ::vcomp::_accept; break }
        ::vchat::send_current
        break
    }
    bind .mid.chatwrap.chat.input <Shift-Return> {
        .mid.chatwrap.chat.input insert insert "\n"
        break
    }
    bind .mid.chatwrap.chat.input <Escape> {
        if {[::vcomp::open]} { ::vcomp::_close; break }
    }
    bind .mid.chatwrap.chat.input <KeyRelease> {::vchat::_preview_schedule}
}

# ── composer preview ─────────────────────────────────────────────────

# Pack/unpack with IDENTICAL flags every time (pitfall 18: a second pack
# call with different flags silently drops the missing ones).
proc ::vchat::_preview_pack {} {
    pack .mid.chatwrap.chat.preview -side bottom -fill x -padx 4 -pady {0 2}
}

proc ::vchat::toggle_preview {} {
    variable preview_on
    set preview_on [expr {[info exists preview_on] ? !$preview_on : 1}]
    if {$preview_on} {
        _preview_pack
        _preview_refresh
        ::vstatus::set_msg [format [::vi18n::mc {composer preview on (%s to hide)}] [::vkeymap::key_for ::vchat::toggle_preview]]
    } else {
        catch { pack forget .mid.chatwrap.chat.preview }
        ::vstatus::set_msg [::vi18n::mc {composer preview off}]
    }
}

proc ::vchat::_preview_schedule {} {
    variable preview_on
    variable preview_after
    if {![info exists preview_on] || !$preview_on} return
    if {[info exists preview_after]} { catch { after cancel $preview_after } }
    # Debounced: re-rendering markdown on every keystroke of a long
    # paragraph is wasted work.
    set preview_after [after 150 ::vchat::_preview_refresh]
}

proc ::vchat::_preview_refresh {} {
    variable preview_on
    if {![info exists preview_on] || !$preview_on} return
    if {![winfo exists .mid.chatwrap.chat.preview]} return
    set txt [string trim [.mid.chatwrap.chat.input get 1.0 end]]
    _writable .mid.chatwrap.chat.preview {
        .mid.chatwrap.chat.preview delete 1.0 end
        if {$txt eq ""} {
            .mid.chatwrap.chat.preview insert end "preview — type markdown above" status
        } else {
            ::vmd::render .mid.chatwrap.chat.preview $txt
        }
    }
}

# The reasoning fold, applied to EVERY block in the transcript.
#
# Named for the live region because that is all it used to reach. The
# checkbox says "reasoning", not "reasoning of the turn currently
# streaming" — and a transcript where the top half obeys the knob and
# the bottom half ignores it is two transcripts. Historical blocks are
# `_collapsible`s, so they fold through the toggle that already knows
# how to rewrite a header without eating its newline.
proc ::vchat::_toggle_live_reasoning {} {
    variable show_reasoning
    variable collapse_open
    set w [::vchat::log]
    # The completed blocks first: each one whose state disagrees with
    # the knob is flipped through `_toggle_collapsible`, which owns the
    # header rewrite (and the tags-before-delete trap documented there).
    # Only blocks in the VISIBLE widget move — _toggle_collapsible reads
    # the ranges off it and returns early when the key is not there, so
    # a hidden session's blocks keep whatever state they were left in.
    set folded 0
    if {[info exists collapse_open]} {
        foreach key [array names collapse_open reason*] {
            if {$collapse_open($key) == $show_reasoning} { continue }
            if {[catch { _toggle_collapsible $key } fold_err]} {
                puts stderr "veles-desktop: fold-all: _toggle_collapsible $key failed: $fold_err"
                continue
            }
            if {$collapse_open($key) == $show_reasoning} { incr folded }
        }
    }
    # EVERY live segment folds, not just the newest — a turn with tool
    # rounds carries several live_r_* regions (R7) and the checkbox
    # means "hide the thinking", all of it. The glob catches body and
    # header tags alike; bodies elide, headers do not.
    foreach t [$w tag names] {
        if {[string match "live_r_*" $t] && ![string match "live_r_hdr_*" $t]} {
            $w tag configure $t -elide [expr {!$show_reasoning}]
        }
    }
    # Rewrite the header glyphs too. The body eliding on its own is the
    # "disappear" these headers exist to prevent; the ▶/▼ line is what
    # tells the user the thinking is still there, folded — click to
    # bring it back. Read each header's tags BEFORE the delete: after it
    # the index points at the body, the same trap _toggle_collapsible
    # documents.
    foreach t [$w tag names] {
        if {![string match "live_r_hdr_*" $t]} { continue }
        set r {}
        catch { set r [$w tag ranges $t] }
        if {$r eq ""} { continue }
        lassign $r hs he
        set htags [$w tag names $hs]
        _writable $w {
            $w delete $hs $he
            _run_at $w $hs "[_live_reason_hdr]\n" $htags
        }
    }
    # SAY WHAT THE GESTURE REACHED. A transcript reopened after a long
    # session carries hundreds of folded blocks, and a control that
    # moved none of them looks broken rather than satisfied — which is
    # half of why the folded thinking read as lost thinking.
    if {$folded > 0} {
        ::vstatus::set_msg [format \
            [::vi18n::mc {reasoning display: %1$s · %2$s blocks}] \
            [expr {$show_reasoning ? {on} : {off}}] $folded]
    }
    return $folded
}

# ── composer resize grip ─────────────────────────────────────────────
# Drag vertically; 1..25 lines. The height persists in the desktop
# state blob (via ::vdock::save_layout).

namespace eval ::vchat {
    variable grip_start_y 0
    variable grip_start_h 0
}

proc ::vchat::_grip_start {rootY} {
    variable grip_start_y $rootY
    variable grip_start_h [.mid.chatwrap.chat.input cget -height]
}

proc ::vchat::_grip_drag {rootY} {
    variable grip_start_y
    variable grip_start_h
    set lh [font metrics TkDefaultFont -linespace]
    if {$lh < 1} { set lh 18 }
    set delta_lines [expr {int(round(($grip_start_y - $rootY) / double($lh)))}]
    set new [expr {$grip_start_h + $delta_lines}]
    if {$new < 1} { set new 1 }
    if {$new > 25} { set new 25 }
    .mid.chatwrap.chat.input configure -height $new
}

proc ::vchat::_grip_done {} {
    ::vdock::save_layout
}

proc ::vchat::set_composer_height {h} {
    if {$h < 1} { set h 1 }
    if {$h > 25} { set h 25 }
    catch { .mid.chatwrap.chat.input configure -height $h }
}
