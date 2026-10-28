# veles-desktop/lib/rendertest.tcl — the transcript must actually show
# the assistant's answer, reasoning block or not.
#
# USER: "I still don't really see responses. Sometimes part of a
# response is visible but when I reload the session view I see only
# reasoning blocks and my prompts."
#
# Root cause: Tk SILENTLY drops `insert` and `delete` on a `-state
# disabled` text widget (rc=0, no error). _render_collapsible_reasoning
# had a nested "normal … disabled" pair inside _paint_message's own
# pair, so it flipped the transcript back to disabled on its way out and
# every insert after it — the ACTUAL ANSWER — was silently discarded.
# Reasoning is painted before the content, so the reasoning survived and
# the reply did not.
#
# These tests render real messages through the real emit_message into a
# real transcript widget, and assert the answer is present in the raw
# buffer AND visible (elide honoured). A fix that only clears the one
# call site but leaves the force-disable pattern elsewhere goes red on
# the nested assertions below.
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

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

# ── the pieces chat.tcl leans on (same shape as sessiontest.tcl) ────
namespace eval ::vstatus { proc set_msg {m} { set ::vstatus::last $m } }
set ::vstatus::last ""
namespace eval ::vsidebar {
    variable refreshes 0
    proc refresh {} { incr ::vsidebar::refreshes }
    proc selected_sid {} { return "" }
}
# get_json must return a VALID empty themes dict — theme.tcl's
# from_engine catches an error but then `dict get`s the result, which
# would throw on a bare `{}`. An empty themes list short-circuits it.
namespace eval ::vapi {

    proc get_json {args} { return [dict create themes {}] }
    proc chat_stop {{sid ""}} {}
    proc post_json {args} { return {} }
}
namespace eval ::vfind  { proc reset_marks {} {} ; proc build {} {} }
namespace eval ::vedit  { proc hist_push {t} {} }
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }
namespace eval ::va11y  { proc name {w t} {} }
namespace eval ::vctx   { proc attach {w items} {} }
namespace eval ::vask   { proc show {d} {} }
namespace eval ::vplace { proc centered {args} {} ; proc near {args} {} }
namespace eval ::vpanes { variable R .nonexistent }

# use_theme persists the choice through ::vstate::put now, and a missing
# proc is a hard error rather than something a catch eats — which is the
# point of the change, so the stub has to carry it.
namespace eval ::vstate {
    proc put {key value} { return 1 }
    proc merge {updates} { return 1 }
    proc all {} { return {} }
    proc get {key {default ""}} { return $default }
    proc repairs {} { return {} }
}
# The key hints chat.tcl renders come from the REAL registry, not a
# stub. A stub returning "M-x" for every action answers questions it
# cannot know, and the whole reason this lookup was rewritten is that
# hints had drifted from the keys that actually run the action — a
# suite that fakes the lookup would never notice that again. Both files
# are pure at load (bindings.tcl only fills a table; keymap.tcl only
# defines procs), so there is nothing to stub around.
source [file join $_dir safeexec.tcl]
source [file join $_dir bindings.tcl]
source [file join $_dir keymap.tcl]
set ::vkeymap::current emacs

source [file join $_dir vtext.tcl]
source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
# media.tcl owns the "can we draw this?" question for BOTH the
# transcript and the preview pane, so both of the modules below need it.
source [file join $_dir media.tcl]
source [file join $_dir markdown.tcl]
source [file join $_dir chat.tcl]

::vt::use_theme synthwave

# Build the transcript widget, mirroring ::vchat::build.
foreach f {.mid .mid.chatwrap .mid.chatwrap.chat .mid.chatwrap.chat.bar} { frame $f }
text .mid.chatwrap.chat.log -wrap word -state disabled \
    -background $::vt::T(bg_input) -foreground $::vt::T(fg) -font {TkDefaultFont 11}
text .mid.chatwrap.chat.input
button .mid.chatwrap.chat.bar.send
button .mid.chatwrap.chat.bar.stop
button .mid.chatwrap.chat.bar.detach
update idletasks

proc body {} { return [.mid.chatwrap.chat.log get 1.0 end-1c] }

# Walk the buffer honouring -elide, exactly as the transcript renders.
proc visible_body {} {
    set w .mid.chatwrap.chat.log
    set out ""
    set i "1.0"
    while {[$w compare $i < "end-1c"]} {
        set elided 0
        foreach t [$w tag names $i] {
            set e [$w tag cget $t -elide]
            if {$e ne "" && $e} { set elided 1 ; break }
        }
        if {!$elided} { append out [$w get $i "$i +1c"] }
        set i [$w index "$i +1c"]
    }
    return $out
}

set REASON "reasoning line 0\nreasoning line 1\nreasoning line 2"

# ── the headline: the answer must survive the reasoning block ────────
puts "answer survives reasoning"
::vchat::emit_message user "prompt one" {}
::vchat::emit_message assistant "THE ANSWER IS FORTY-TWO." $REASON
ok "the answer is in the raw buffer" \
    {[string match "*THE ANSWER IS FORTY-TWO.*" [body]]}
ok "the reasoning header is present" \
    {[string match "*reasoning (3 lines*" [body]]}
ok "the reasoning body is still in the raw buffer (elided, not gone)" \
    {[string match "*reasoning line 0*" [body]]}
ok "the answer is VISIBLE, not just present" \
    {[string match "*THE ANSWER IS FORTY-TWO.*" [visible_body]]}
ok "the collapsed reasoning body is NOT visible" \
    {![string match "*reasoning line 0*" [visible_body]]}
ok "the widget is left read-only after painting" \
    {[.mid.chatwrap.chat.log cget -state] eq "disabled"}

# ── nesting: a second reply in the same session must not vanish ──────
puts "\nrepeated replies"
::vchat::emit_message user "prompt two" {}
::vchat::emit_message assistant "SECOND ANSWER HERE." $REASON
ok "the first answer is still there after a second reply" \
    {[string match "*THE ANSWER IS FORTY-TWO.*" [body]]}
ok "the second answer survives too" \
    {[string match "*SECOND ANSWER HERE.*" [body]]}
ok "both answers are visible" \
    {[string match "*THE ANSWER IS FORTY-TWO.*" [visible_body]]
     && [string match "*SECOND ANSWER HERE.*" [visible_body]]}

# ── a reply with NO reasoning must also survive ──────────────────────
puts "\nplain reply"
::vchat::emit_message assistant "NO REASONING HERE." {}
ok "a reply with no reasoning shows up" \
    {[string match "*NO REASONING HERE.*" [body]]}

# ── source view and rebuild compose with the write-helper ────────────
puts "\nrebuild"
::vchat::rebuild
ok "rebuild keeps every answer" \
    {[string match "*THE ANSWER IS FORTY-TWO.*" [body]]
     && [string match "*SECOND ANSWER HERE.*" [body]]
     && [string match "*NO REASONING HERE.*" [body]]}
ok "…and stays readable" {[.mid.chatwrap.chat.log cget -state] eq "disabled"}

# ── rebuild repaints ONE session, not every loaded one ──────────────
#
# USER 2026-09-19: /redraw on a subtask's window dumped the PARENT's
# transcript into it. `rebuild` deleted the current widget and repainted
# EVERY id in `raw` — a global array of every loaded session's sources —
# with no owner check, while its deletion twin `forget_raw` had always
# filtered by `raw_w($id)`. With two sessions loaded, /redraw on the
# child wiped the child's text and replayed parent-and-child into the
# child's window.
puts "\nrebuild is per-transcript"
set child_w [::vchat::log][incr ::vchat::logs_seq]
text $child_w -state disabled
dict set ::vchat::logs "CHILD" $child_w
set ::vchat::session "CHILD"
::vchat::emit_message user "child line one" {}
ok "the child transcript starts separate" \
    {[string match "*child line one*" [$child_w get 1.0 end-1c]]
     && ![string match "*THE ANSWER IS FORTY-TWO.*" [$child_w get 1.0 end-1c]]}
::vchat::rebuild
ok "a rebuild of the child keeps every child answer" \
    {[string match "*child line one*" [$child_w get 1.0 end-1c]]}
ok "…and none of the parent transcript bleeds in" \
    {![string match "*THE ANSWER IS FORTY-TWO.*" [$child_w get 1.0 end-1c]]}
ok "…while the parent's transcript stays in its own widget" \
    {[string match "*THE ANSWER IS FORTY-TWO.*" [.mid.chatwrap.chat.log get 1.0 end-1c]]}
set ::vchat::session ""

# ── tool rounds must be visible, not swallowed with the stream ───────
# USER: "tool invocations were also missing from the output." During a
# turn the ⚙ lines stream in, but on_done replaced the whole streaming
# region with just the reply, so the calls that produced it vanished.
puts "\ntool rounds"
::vchat::clear
::vchat::emit_message assistant "The answer." {} \
    [list [list "read_file" "" "line one\nline two"]]
ok "the tool name is shown" {[string match "*⚙ read_file*" [body]]}
ok "the tool result is behind an expand header" \
    {[regexp {result \(\d+ chars — click to expand\)} [body]]}
ok "…collapsed: the result is in the buffer but not on screen" \
    {[string match "*line one*" [body]] && ![string match "*line one*" [visible_body]]}
ok "the answer follows the tool round" {[string match "*The answer.*" [body]]}

# A role=tool history row renders as a tool round, not an assistant
# bubble with the raw result leaked as content.
::vchat::clear
::vchat::emit_message tool "raw tool result body" {} \
    [list [list "execute" "path=/tmp/x" "ran ok"]]
ok "a tool row shows its glyph, invocation and result" \
    {[string match "*⚙ execute path=/tmp/x*" [body]] && [string match "*ran ok*" [body]]}
ok "…and does not leak the raw result as a bubble" \
    {![string match "*raw tool result body*" [body]]}

# on_done actually carries the tracked rounds into the final message.
::vchat::clear
set ::vchat::session "T"
set ::vchat::stream_session "T"
dict set ::vchat::turn_anchor T 1.0
::vchat::on_tool start "shell" {}
::vchat::on_tool end "shell" [dict create result "echo hi"]
::vchat::on_done [dict create provider p model m prompt_tokens 1 \
    completion_tokens 4 text "ran it" notice null]
ok "on_done re-renders the turn with its tool round" \
    {[string match "*⚙ shell*" [body]] && [string match "*ran it*" [body]]}

# ── expanding reasoning must not eat the line break ─────────────────
#
# USER: "Reasoning blocks now expand on the same line as the collapsed
# summary and it looks weird". The header tag covers its trailing
# newline, so the toggle's delete removed the break and the re-insert
# did not put it back.
puts "\nreasoning toggle keeps its line break"

::vchat::clear
::vchat::forget_raw
::vchat::emit_message assistant "THE ANSWER" "line one\nline two\nline three"
update
set w .mid.chatwrap.chat.log
set body_tag ""
set hdr_tag ""
foreach t [$w tag names] {
    if {[string match "cbody_reason*" $t]} { set body_tag $t }
    if {[string match "chdr_reason*" $t]} { set hdr_tag $t }
}
ok "the reasoning block rendered" {$body_tag ne "" && $hdr_tag ne ""}

proc hdr_line {} {
    set w .mid.chatwrap.chat.log
    foreach line [split [$w get 1.0 end-1c] "\n"] {
        if {[string match "*reasoning (*" $line]} { return $line }
    }
    return ""
}
ok "collapsed, the header is its own line" \
    {[string match "*click to expand)" [hdr_line]]}

::vchat::_toggle_collapsible [string range $body_tag 6 end]
update
ok "expanded, the header is STILL its own line" \
    {[string match "*click to collapse)" [hdr_line]]}
ok "…the body did not weld onto it" \
    {![string match "*collapse)line one*" [hdr_line]]}
ok "…and the body is visible" {[$w tag cget $body_tag -elide] == 0}

::vchat::_toggle_collapsible [string range $body_tag 6 end]
update
ok "collapsing again keeps the line break too" \
    {[string match "*click to expand)" [hdr_line]]
     && ![string match "*expand)line one*" [hdr_line]]}
ok "…and hides the body" {[$w tag cget $body_tag -elide] == 1}

# The header must survive its own toggle. It used to INHERIT the body's
# tag: _toggle_collapsible read `[$w tag names $hs]` on the line AFTER
# deleting the header, at which point $hs is the body's first character.
# The rebuilt header therefore carried cbody_$key, and this second
# collapse elided the header along with the body — the whole block
# vanished, unclickable, until the session was reopened.
#
# `$w get` CANNOT see this: it returns elided text too, which is exactly
# why the assertions above stayed green for weeks while the block was
# invisible on screen. `count -displaychars` asks what is actually
# DISPLAYED, and is the only assertion here that fails on the bug.
#
# USER: "the reasoning blocks still just disappear instead of being
# collapsed on a live transcript, only reopening a transcript makes them
# reappear".
set ckey [string range $body_tag 6 end]
set hdr_tag "chdr_$ckey"
lassign [$w tag ranges $hdr_tag] hs he
ok "the rebuilt header does not inherit the body tag" \
    {[lsearch -exact [$w tag names $hs] $body_tag] < 0}
ok "…so a collapsed block still SHOWS its header" \
    {[$w count -displaychars $hs $he] > 0}

# Toggling repeatedly must not accumulate or lose newlines.
for {set i 0} {$i < 6} {incr i} { ::vchat::_toggle_collapsible [string range $body_tag 6 end] }
update
set blanks 0
foreach line [split [$w get 1.0 end-1c] "\n"] {
    if {[string trim $line] eq ""} { incr blanks }
}
ok "six toggles do not accumulate blank lines" {$blanks <= 4}

# ── engine notes are transcript entries, not decoration ─────────────
#
# USER: "The guards disappear after I reload the session?" The history
# endpoint returns them and open_session replays them, but they were
# written straight into the widget and never recorded next to the
# messages — so every rebuild (view-source, theme change, one message
# flipped) redrew from `raw` alone and the notes were gone.
puts "\nengine notes survive a rebuild"
set ::vchat::stream_session ""
::vchat::clear
::vchat::forget_raw
::vchat::emit_message user "the question"
set note_id [::vchat::on_note [dict create \
    kind guard \
    summary "turn halted — iteration cap reached" \
    detail "The iteration guard stops a turn that keeps calling tools." \
    toggle [dict create key agent.guards.max_iterations value 12 \
                       choices {} help "how many tool rounds"]]]
::vchat::emit_message assistant "the answer"
update

proc order_ok {} {
    set b [body]
    set a [string first "the question" $b]
    set n [string first "iteration cap reached" $b]
    set z [string first "the answer" $b]
    return [expr {$a >= 0 && $n > $a && $z > $n}]
}

ok "the note is in the transcript"     {[string match "*iteration cap reached*" [body]]}
ok "…in the order it happened"         {[order_ok]}
ok "…with its glyph"                   {[string match "*⚠ turn halted*" [body]]}
ok "…and a hint that it expands"       {[string match "*▸*" [body]]}

::vchat::rebuild
update
ok "a rebuild keeps the note"          {[string match "*iteration cap reached*" [body]]}
ok "…still in the right place"         {[order_ok]}
ok "…and still clickable"              \
    {[.mid.chatwrap.chat.log tag bind chdr_note$note_id <Button-1>] ne ""}

::vchat::toggle_source
update
ok "a view-source toggle keeps it too" {[string match "*iteration cap reached*" [body]]}
ok "…and does not turn it into source" {[.mid.chatwrap.chat.log tag cget cbody_note$note_id -elide] == 1}
::vchat::toggle_source
update

# Expanding must produce the detail AND the knob, or the "toggle it
# from where you noticed it" promise is empty.
#
# AND IT MUST EXPAND IN PLACE. USER: "Clicking on a guard seems to dump
# new text in the transcript window at the bottom instead of expanding
# like reasoning, ideally it would just do that." `_note_expand` used to
# end in `emit_message system`, which appends a whole new message at the
# end of the transcript — so the detail appeared, but detached from the
# note, after everything else, and again on every click.
set _before [body]
set _tail_before [string range [body] end-40 end]
::vchat::_toggle_collapsible note$note_id
update
ok "expanding shows the detail"        {[.mid.chatwrap.chat.log tag cget cbody_note$note_id -elide] == 0
     && [string match "*stops a turn that keeps calling tools*" [body]]}
ok "…and names the knob"               {[string match "*agent.guards.max_iterations = 12*" [body]]}
ok "…and the transcript did not grow at the END" \
    {[string range [body] end-40 end] eq $_tail_before}

# The detail sits UNDER ITS OWN NOTE, not somewhere after the last
# message. Both are in the text either way; only the position tells them
# apart.
proc line_of {needle} {
    set i 0
    foreach l [split [body] "\n"] {
        incr i
        if {[string match "*$needle*" $l]} { return $i }
    }
    return -1
}
ok "…directly below the summary it belongs to" \
    {[line_of "stops a turn that keeps calling tools"] == [line_of "turn halted"] + 1}

# Clicking again collapses. The append version could only ever grow.
::vchat::_toggle_collapsible note$note_id
update
ok "clicking again hides the detail"   {[.mid.chatwrap.chat.log tag cget cbody_note$note_id -elide] == 1}
ok "…and the summary is still there"   {[string match "*turn halted*" [body]]}

# Twice more, to catch an expand that re-inserts rather than un-elides.
::vchat::_toggle_collapsible note$note_id
::vchat::_toggle_collapsible note$note_id
::vchat::_toggle_collapsible note$note_id
update
ok "…and toggling repeatedly leaves ONE copy" \
    {[regexp -all {stops a turn that keeps calling tools} [body]] == 1}

# An expanded note survives a repaint. This is why the block is keyed by
# message id rather than by a clock reading: `rebuild` repaints from
# `raw`, and a clock-minted key is a different block every time.
ok "the note is expanded going in" {![::vchat::collapsed note$note_id]}
::vchat::rebuild
update
ok "a rebuild keeps it expanded"       {![::vchat::collapsed note$note_id]}
ok "…and the detail is still on screen" \
    {[string match "*stops a turn that keeps calling tools*" [body]]}
ok "…still exactly once"               \
    {[regexp -all {stops a turn that keeps calling tools} [body]] == 1}

# A note carrying neither detail nor toggle has nothing to expand, and
# must not advertise that it does.
::vchat::clear
::vchat::forget_raw
::vchat::on_note [dict create kind memory summary "12k of context injected"]
update
ok "a bare note renders"               {[string match "*12k of context injected*" [body]]}
ok "…without an expander"              {![string match "*▸*" [body]]}

::vchat::clear
::vchat::forget_raw

# ── notes stay where they happened when the turn ends ───────────────
#
# USER 2026-09-29: "all the guards are dumped at the end of the agent
# turn - those ideally should only be shown inline when they happen."
# They were painted inline live — then on_done's reconcile deleted the
# live region and rebuilt it from the turn's calls and tool rounds only,
# and the next rebuild (a theme change, a view toggle) painted the notes
# from the ordered store, bunched together, ahead of the turn. The turn
# below is the engine's real order: a call streams, the note ABOUT it
# follows its row, a guard fires inside a tool round, the answer
# streams, and a closing note follows it.
puts "\nnotes keep their place across the turn's reconcile"
set _was_session $::vchat::session
set ::vchat::session "NT"
set ::vchat::stream_session "NT"
::vchat::emit_message user "NOTE-Q the question"
dict set ::vchat::turn_anchor NT [.mid.chatwrap.chat.log index end-1c]
::vchat::on_delta "NOTE-P1 looking. "
::vchat::on_note [dict create kind cache summary "NOTE-CACHE missed"]
::vchat::on_tool start terminal [dict create args [dict create command ls]]
::vchat::on_note [dict create kind guard summary "NOTE-GUARD repeated"]
::vchat::on_tool end terminal [dict create result "NOTE-RESULT"]
::vchat::on_delta "NOTE-ANSWER here."
::vchat::on_note [dict create kind guard summary "NOTE-VERIFY unverified"]
update

# Live, the ⚙ header is painted when the tool STARTS, so a guard that
# fires inside the round sits between the header and the result. The
# persisted turn has the round as ONE row (header and result), after the
# call that made it — the guard, anchored to that call's row, comes just
# before the round. The reconcile paints what a reload paints.
set live_order   {NOTE-Q NOTE-P1 NOTE-CACHE terminal NOTE-GUARD NOTE-RESULT
                  NOTE-ANSWER NOTE-VERIFY}
set reload_order {NOTE-Q NOTE-P1 NOTE-CACHE NOTE-GUARD terminal NOTE-RESULT
                  NOTE-ANSWER NOTE-VERIFY}
proc note_order {order} {
    set b [body]
    set prev -1
    foreach needle $order {
        set i [string first $needle $b]
        if {$i <= $prev} { return "$needle out of order" }
        set prev $i
    }
    foreach needle {NOTE-CACHE NOTE-GUARD NOTE-VERIFY NOTE-ANSWER} {
        if {[regexp -all $needle $b] != 1} { return "$needle x[regexp -all $needle $b]" }
    }
    return ok
}
ok "live: every note where it arrived"          {[note_order $live_order] eq "ok"}
# A note the HISTORY painter replays belongs to a finished turn; it must
# never become one of this turn's blocks (the reconcile would move it).
::vchat::_paint_one_row [dict create role note kind guard summary "NOTE-OLD replayed"]
ok "a replayed note is not one of the live turn's blocks" \
    {![string match "*NOTE-OLD*" $::vchat::live_blocks]}
::vchat::on_done [dict create provider p model m prompt_tokens 1 \
    completion_tokens 1 text "NOTE-ANSWER here."]
update
ok "after the turn ends: in place, once"        {[note_order $reload_order] eq "ok"}
::vchat::rebuild
update
ok "after a rebuild: still in place, once"      {[note_order $reload_order] eq "ok"}
set ::vchat::stream_session ""
set ::vchat::session $_was_session
::vchat::clear
::vchat::forget_raw

# ── when things happened ────────────────────────────────────────────
#
# USER 2026-09-29: "we're also missing timestamps, in the desktop … it's
# not always obvious to me from the context when the messages were
# sent." A reloaded row carries the server's time (RFC 3339, UTC) and
# paints it in local time; a live one is stamped as it happens; a row
# with no time gets no stamp rather than an invented one. The date gets
# a line of its own whenever it changes.
puts "\ntimestamps"
set ::vchat::stream_session ""
::vchat::clear
::vchat::forget_raw
set t1 [clock scan "2026-09-28T23:58:10Z" -format {%Y-%m-%dT%H:%M:%SZ} -timezone :UTC]
set t2 [expr {$t1 + 26 * 3600}]
set hm1 [clock format $t1 -format %H:%M:%S]
set hm2 [clock format $t2 -format %H:%M:%S]
::vchat::emit_message user "STAMP-Q1 first" "" {} "2026-09-28T23:58:10Z"
::vchat::emit_message assistant "STAMP-A1 answer" "" {} "2026-09-28T23:58:10Z"
::vchat::emit_message user "STAMP-Q2 next day" "" {} [clock format $t2 -format {%Y-%m-%dT%H:%M:%SZ} -timezone :UTC]
::vchat::emit_message user "STAMP-Q3 no time known" "" {} ""
update
ok "a reloaded row shows its time, in local time" \
    {[string match "*$hm1 you ▸ STAMP-Q1*" [body]]}
ok "…and so does the answer"                  {[string match "*$hm1 STAMP-A1*" [body]]}
ok "the day gets a line of its own"           \
    {[regexp -all "── [clock format $t1 -format %Y-%m-%d] ──" [body]] == 1}
ok "…and a new one when it changes"           \
    {[string match "*── [clock format $t2 -format %Y-%m-%d] ──*$hm2 you ▸ STAMP-Q2*" [body]]}
ok "a row with no time gets no invented one"  \
    {[string match "*\nyou ▸ STAMP-Q3*" [body]]}
::vchat::rebuild
update
ok "a rebuild paints the same stamps"         \
    {[string match "*$hm1 you ▸ STAMP-Q1*" [body]]
     && [regexp -all "── [clock format $t1 -format %Y-%m-%d] ──" [body]] == 1}
# Hovering a stamp says the whole moment.
set w .mid.chatwrap.chat.log
set ::vstatus::last ""
::vchat::_stamp_show $w [lindex [$w tag ranges stamp] 0]
ok "hovering a stamp shows the full date and time" \
    {$::vstatus::last eq [clock format $t1 -format {%Y-%m-%d %H:%M:%S}]}
# A note from history keeps ITS time, and a live one is stamped now.
::vchat::_paint_one_row [dict create role note kind guard summary "STAMP-NOTE old" \
    at "2026-09-28T23:58:10Z"]
set before [clock seconds]
::vchat::on_note [dict create kind guard summary "STAMP-NOTE live"]
set after [clock seconds]
update
ok "a replayed note keeps the server's time"  {[string match "*$hm1 ⚠ STAMP-NOTE old*" [body]]}
ok "a live note is stamped as it happens"     \
    {[string match "*[clock format $before -format %H:%M:%S] ⚠ STAMP-NOTE live*" [body]]
     || [string match "*[clock format $after -format %H:%M:%S] ⚠ STAMP-NOTE live*" [body]]}
ok "a stamp says the second, not only the minute" \
    {[regexp "(^|\n)$hm1 you ▸ STAMP-Q1" [body]] && [regexp {\d\d:\d\d:\d\d } [body]]}
::vchat::clear
::vchat::forget_raw

# A LIVE call is stamped at its first visible output, not at turn end.
#
# USER 2026-09-30: "the timestamps are only on tool calls". The prose and
# thinking blocks got their stamp from the reconcile when the turn
# ENDED, so for the whole of a long turn only the ⚙ rows had one —
# reproduced on the real desktop against a real gateway. Thinking then
# prose is ONE call (one stamp); a tool round opens the next call.
set ::vchat::session "S"
set ::vchat::stream_session ""
set ::vchat::live_ctx ""
set ::vchat::live_saved {}
set ::vchat::tool_rounds(S) {}
dict set ::vchat::turn_anchor "S" [$w index end-1c]
dict set ::vchat::turn_reload_anchor "S" [$w index end-1c]
::vchat::_mark_running "S" 1
::vchat::on_reasoning "LIVE-THINK one"
::vchat::on_delta "LIVE-PROSE one"
::vchat::on_tool start read_file [dict create args "{}"]
::vchat::on_tool end read_file [dict create result "x"]
::vchat::on_delta "LIVE-PROSE two"
update
set live_stamps [regexp -all -inline {\d\d:\d\d:\d\d} [body]]
ok "a live call's thinking is stamped as it starts" \
    {[regexp {\d\d:\d\d:\d\d [^\n]*\n[^\n]*LIVE-THINK one} [body]]}
ok "…its prose, the same call, gets no second stamp" \
    {![regexp {\d\d:\d\d:\d\d LIVE-PROSE one} [body]]}
ok "the call after a tool round is stamped too" \
    {[regexp {\d\d:\d\d:\d\d LIVE-PROSE two} [body]]}
ok "one stamp per call and per round, mid-turn" {[llength $live_stamps] == 3}
::vchat::on_done [dict create text "LIVE-PROSE two" provider p model m \
    prompt_tokens 1 completion_tokens 2 display_reasoning "" notice ""]
update
ok "the turn's end keeps the stamps the live view showed" \
    {[regexp -all -inline {\d\d:\d\d:\d\d} [body]] eq $live_stamps}
::vchat::_mark_running "S" 0
::vchat::clear
::vchat::forget_raw

# ── the preview pane renders what it can ────────────────────────────
#
# USER: "preview pane doesn't render markdown". It never did — every
# file went in as raw text in a fixed font. That is also why the agent's
# workaround for the table bug (write a .md, open it in Preview) came
# out as pipes and asterisks: the one surface that was supposed to
# render a document was the one that could not.
puts "\nthe preview pane"
source [file join $_dir panes.tcl]
frame .previewroot
set ::vpanes::R .previewroot
::vpanes::_build_preview $::vpanes::R.preview
update idletasks

proc pv {} { return [$::vpanes::R.preview.body get 1.0 end-1c] }
proc pvtag {tag} {
    set out ""
    foreach {a b} [$::vpanes::R.preview.body tag ranges $tag] {
        append out [$::vpanes::R.preview.body get $a $b]
    }
    return $out
}

ok "a .md path is markdown"        {[::vpanes::preview_kind_for "/tmp/a.md"] eq "markdown"}
ok "…case-insensitively"           {[::vpanes::preview_kind_for "/tmp/A.MARKDOWN"] eq "markdown"}
ok "a .rs path is not"             {[::vpanes::preview_kind_for "/tmp/a.rs"] eq "plain"}
ok "an extensionless path is not"  {[::vpanes::preview_kind_for "/tmp/Makefile"] eq "plain"}

# The pane used to carry its own two-extension list (.png .gif), so a
# JPEG went to the fs text reader and came back "binary — not
# previewable". There is one list now, in ::vmedia, and this is the
# assertion that keeps a second one from growing back.
ok "a .jpg is an image to the pane"  {[::vpanes::preview_kind_for "/tmp/a.jpg"] eq "image"}
ok "…and so is a .webp"              {[::vpanes::preview_kind_for "/tmp/a.webp"] eq "image"}
ok "…while a .zip is still plain"    {[::vpanes::preview_kind_for "/tmp/a.zip"] eq "plain"}

::vpanes::_preview_set "# Title\n\nsome **bold** and a \[link\](https://e.example/x)\n" "/tmp/doc.md"
update
ok "markdown is rendered, not printed" {![string match "*# Title*" [pv]]}
ok "…the heading keeps its text"       {[string match "*Title*" [pv]]}
ok "…bold is a tag, not asterisks"     {[pvtag md_bold] eq "bold"}
ok "…and the link is a link"           {[pvtag md_link] eq "link"}
ok "…with a Source button offered"     {[winfo manager $::vpanes::R.preview.bar.src] ne ""}

::vpanes::toggle_preview_source
update
ok "Source shows the markdown as written" {[string match "*# Title*" [pv]]}
ok "…and the button offers the way back" \
    {[$::vpanes::R.preview.bar.src cget -text] eq "Rendered"}
::vpanes::toggle_preview_source
update
ok "toggling back renders again" {![string match "*# Title*" [pv]]}

# A source file must NOT go through the markdown renderer: `*` and `_`
# are operators there, and rendering them as emphasis eats characters.
::vpanes::_preview_set "let x = a * b * c;\nlet _y = 1;\n" "/tmp/main.rs"
update
ok "a source file is shown verbatim" {[string trim [pv]] eq "let x = a * b * c;\nlet _y = 1;"}
ok "…with no emphasis applied"       {[pvtag md_italic] eq ""}
ok "…and no Source button"           {[winfo manager $::vpanes::R.preview.bar.src] eq ""}

# Opening a second file must not leave the first one's Source state on.
::vpanes::_preview_set "# A\n" "/tmp/a.md"
::vpanes::toggle_preview_source
::vpanes::_preview_set "# B\n" "/tmp/b.md"
update
ok "a new file starts rendered" {![string match "*# B*" [pv]]}

# ── attachments in the transcript ───────────────────────────────────
#
# USER: "attachments/artifacts in general. I didn't mean just
# model-to-user MEDIA." A reply carrying a MEDIA: marker used to inline
# PNG/GIF and, for anything else, write one dead line with no size and
# no actions. The renderer lives in ::vmedia now; these pin that the
# transcript actually goes through it.
puts "\nattachments in the transcript"
set _mtmp [file join [expr {[info exists ::env(TMPDIR)] ? $::env(TMPDIR) : "/tmp"}] \
    "veles-rendertest-[pid]"]
file mkdir $_mtmp
set _png [file join $_mtmp pic.png]
set _g [image create photo -width 20 -height 20]
$_g put "#ff00aa" -to 0 0 20 20
$_g write $_png -format png
set _odd [file join $_mtmp report.pdf]
set _fh [open $_odd w] ; puts -nonewline $_fh "%PDF-1.4 not really" ; close $_fh

::vchat::clear
::vchat::forget_raw
::vchat::emit_message assistant "here you go\nMEDIA:$_png"
update
ok "an attached PNG is a picture, not a path" \
    {[llength [.mid.chatwrap.chat.log image names]] == 1}
ok "…captioned with its file name" {[string match "*pic.png*" [body]]}

::vchat::clear
::vchat::forget_raw
::vchat::emit_message assistant "the report:\nMEDIA:$_odd"
update
ok "a file we cannot draw gets a card" {[string match "*report.pdf*" [body]]}
ok "…that says what it is"            {[string match "*PDF document*" [body]]}
ok "…and offers to open it"           {[string match "*Open*" [body]]}
ok "…and to save it"                  {[string match "*Save as*" [body]]}
set _acts 0
foreach t [.mid.chatwrap.chat.log tag names] {
    if {[string match "vmedia_a*" $t]} { incr _acts }
}
ok "…with each action separately clickable" {$_acts >= 3}

# A missing attachment must not look like a working one.
::vchat::clear
::vchat::forget_raw
::vchat::emit_message assistant "MEDIA:[file join $_mtmp gone.png]"
update
ok "a missing attachment says not found" {[string match "*not found*" [body]]}
ok "…and offers nothing to click"        {![string match "*Save as*" [body]]}

# ── RM3-S4/A4: injection rows are labelled blocks, never speech ─────
#
# A user-role row the ENGINE wrote arrives from history with an
# `injection` class. Rendering it as a plain bubble frames the engine's
# report as words the human said; it must paint as a labelled note
# block with the class's knob, while steer (the user's own words) and
# compaction (structural) get the label but no control.
puts "\ninjection rows paint as labelled blocks"
::vchat::clear
::vchat::forget_raw
::vchat::_paint_one_row [dict create \
    role user \
    content {[BACKGROUND SHELL COMPLETE] shell 7 finished (command: log). Output: none} \
    injection background \
    toggle [dict create key agent.injections.background value true \
                   choices {} help "background completions enter the model's context"]]
::vchat::_paint_one_row [dict create \
    role user \
    content {[STEER] use python instead} \
    injection steer]
update

ok "the injected row is labelled, not a speech bubble" \
    {[string match "*engine told: background task completed*" [body]]}
ok "…with the injection glyph" \
    {[string match "*⇒ engine told: background task completed*" [body]]}
ok "…and the raw banner text is not shown as speech until expanded" \
    {![string match {*[[]BACKGROUND SHELL COMPLETE[]]*} [$w get 1.0 end-1c]]}
ok "steer renders as a label too" \
    {[string match "*steered: your redirect mid-turn*" [body]]}
::vchat::_paint_one_row [dict create \
    role user \
    content {[origin] this message arrived via the desktop surface} \
    injection origin:desktop]
update
ok "an origin row renders as 'said on <surface>'" \
    {[string match "*⇒ said on desktop*" [body]]}
ok {…and never as its raw [origin] banner} \
    {![string match {*[[]origin[]]*} [$w get 1.0 end-1c]]}
catch {
    foreach tag [.mid.chatwrap.chat.log tag names] {
        if {[string match "chdr_note*" $tag]} {
            set inj_id [string range $tag 5 end]
        }
    }
}
::vchat::_toggle_collapsible $inj_id
update
ok "expanding an injection block shows the suppressed detail" \
    {[string match "*shell 7 finished*" [body]]}
ok "…and names its mute knob" \
    {[string match "*agent.injections.background = true*" [body]]}

# Session mail (veles_core::mail): another session's words. The engine's
# provenance row before them is for the MODEL and paints nothing; the
# mail row paints once, as mail — its sender in the head, its first line
# in view — and never as a "you" bubble or with its raw header.
puts "\nsession mail paints as mail, once"
::vchat::clear
::vchat::forget_raw
::vchat::_paint_one_row [dict create \
    role user \
    content {[origin] this message is mail from session “worker” (20261011_101010_abcdef): another agent or program wrote it, not the user.} \
    injection origin:mail]
::vchat::_paint_one_row [dict create \
    role user \
    content "what is on the phone's screen?\nand the battery too" \
    injection mail:20261011_101010_abcdef]
update
ok "the provenance row paints nothing — not even as a labelled note" \
    {![string match {*[[]origin[]]*} [$w get 1.0 end-1c]]
     && ![string match "*said on mail*" [body]]
     && [llength [lsearch -all -glob [.mid.chatwrap.chat.log tag names] chdr_note*]] == 1}
ok "the mail row names its sender with the mail glyph" \
    {[string match "*✉ message from session 20261011_101010_abcdef*" [body]]}
ok "…with its first line in view" \
    {[string match "*what is on the phone's screen?*" [body]]}
ok "…and is not painted as something you typed" \
    {![string match "*you*what is on the phone*" [body]]}

# ── a reopened row keeps its reasoning ───────────────────────────────
#
# USER 2026-09-16: "reopened transcript seems to lose many historical
# thinking blocks."
#
# The payload is not the suspect: `history_rows` falls back to the raw
# `reasoning` column when no plugin wrote a `display_reasoning`, and the
# session the report came from returns one on 298 of 298 assistant rows.
# What had never been asserted is the CONSUMER — `_paint_one_row` is
# driven above for injection rows and nowhere else, and every
# `display_reasoning` fixture in the other suites is a done-EVENT shape,
# not a history row. So the one path the report is about was unpinned.
#
# It is pinned now, in both directions: a row that carries reasoning
# gets a header, and a row that does not must not sprout one — a paint
# that invents an empty block would read as "the model thought nothing"
# rather than "nothing was recorded".
puts "\na history row paints the reasoning it carries"
::vchat::clear
::vchat::forget_raw
::vchat::_paint_one_row [dict create \
    role assistant \
    content "The identity check is dead code." \
    display_reasoning "first I read the two arms, then the caller"]
update
ok "the reasoning arrives as a collapsible header" \
    {[string match "*▶ reasoning (1 lines, click to expand)*" [body]]}
ok "…the answer is painted beside it" \
    {[string match "*The identity check is dead code.*" [body]]}
ok "…and the body is in the buffer, elided until clicked" \
    {[string match "*first I read the two arms, then the caller*" [body]]
     && ![string match "*first I read the two arms*" [visible_body]]}

# The engine sends JSON null for a row with no reasoning, and tcllib
# hands that through as the four-letter string. A paint that took it at
# face value would open a block whose body reads "null".
puts "\na row with no reasoning grows no block"
::vchat::clear
::vchat::forget_raw
::vchat::_paint_one_row [dict create \
    role assistant \
    content "no thinking was recorded for this one" \
    display_reasoning null]
update
ok "a null reasoning field is not a reasoning block" \
    {![string match "*▶ reasoning*" [body]]}
ok "…and never paints the word null" {![string match "*null*" [body]]}
ok "…while the answer is still there" \
    {[string match "*no thinking was recorded*" [body]]}

# The round that produced a tool call: empty answer, real reasoning.
# This is most of a long turn's history — 298 assistant rows in the
# reported session, the majority with an empty content field — so a
# paint that needs non-empty content to render the block would lose
# nearly all of them.
puts "\nthe reasoning of a tool-call round survives an empty answer"
::vchat::clear
::vchat::forget_raw
::vchat::_paint_one_row [dict create \
    role assistant \
    content "" \
    display_reasoning "let me batch these reads" \
    tool_calls [list [dict create id call_1 \
        function [dict create name terminal arguments {{"cmd":"ls"}}]]]]
update
ok "an empty-answer round still shows what it was thinking" \
    {[string match "*▶ reasoning*" [body]]
     && [string match "*let me batch these reads*" [body]]}

# ── the reasoning knob reaches the whole transcript ──────────────────
#
# USER 2026-09-16: "reopened transcript seems to lose many historical
# thinking blocks."
#
# Nothing was lost. Driving the real 634-row payload through the real
# `_paint_history` paints 298 reasoning headers for the 298 assistant
# rows that carry reasoning — every one of them present, and every one
# of them FOLDED, which is the documented decision ("long chains of
# thought must not drown the transcript") and stays the default here.
#
# What was actually broken is the way out. `_toggle_live_reasoning` —
# the checkbox, and `/reasoning` — globbed `live_r_*` and nothing else,
# so on a reopened session the control labelled "reasoning" moved
# nothing at all and the only way to read back a turn's thinking was to
# click its header. A control that does nothing is indistinguishable
# from one that is broken, which is most of why folded read as lost.
puts "\nthe reasoning knob reaches historical blocks, not just the live region"
::vchat::clear
::vchat::forget_raw
array unset ::vchat::collapse_open
::vchat::emit_message assistant "first answer" "thought one"
::vchat::emit_message assistant "second answer" "thought two"
update
ok "both blocks start folded — the default is unchanged" \
    {![string match "*thought one*" [visible_body]]
     && ![string match "*thought two*" [visible_body]]}

set ::vchat::show_reasoning 1
set n [::vchat::_toggle_live_reasoning]
update
ok "turning it on unfolds EVERY block, not the newest one" \
    {[string match "*thought one*" [visible_body]]
     && [string match "*thought two*" [visible_body]]}
ok "…and says how many it moved, so the gesture is not silent" \
    {$n == 2 && [string match "*2 blocks*" $::vstatus::last]}

set ::vchat::show_reasoning 0
::vchat::_toggle_live_reasoning
update
ok "turning it off folds them again" \
    {![string match "*thought one*" [visible_body]]
     && ![string match "*thought two*" [visible_body]]}
ok "…and the bodies are still in the buffer — folded, never deleted" \
    {[string match "*thought one*" [body]] && [string match "*thought two*" [body]]}
ok "…with a header left to click back open" \
    {[string match "*▶ reasoning*" [visible_body]]}

# Pressing it when nothing disagrees must not announce a no-op: the
# status line is for things that happened.
set ::vstatus::last ""
set again [::vchat::_toggle_live_reasoning]
ok "a knob already in that position moves nothing and says nothing" \
    {$again == 0 && $::vstatus::last eq ""}
set ::vchat::show_reasoning 1

# /reasoning and the checkbox are one gesture and must report alike —
# the command used to restate the plain line and throw away the count
# the fold had just computed.
puts "\nthe /reasoning command reports what the checkbox reports"
::vchat::clear
::vchat::forget_raw
array unset ::vchat::collapse_open
set ::vchat::show_reasoning 0
::vchat::emit_message assistant "an answer" "a thought"
update
set ::vstatus::last ""
::vchat::slash_command "/reasoning"
update
ok "the command unfolds the block" {[string match "*a thought*" [visible_body]]}
ok "…and its status line carries the count, not the bare on/off" \
    {[string match "*1 blocks*" $::vstatus::last]}
set ::vstatus::last ""
::vchat::slash_command "/reasoning"
ok "…and with nothing on screen to move it still says which way it went" \
    {[string match "*reasoning display: off*" $::vstatus::last]}
set ::vchat::show_reasoning 1

# ── the message under a point is that message ───────────────────────
# USER 2026-10-01, on the phone: "I do 'copy message' but nothing is in
# the clipboard". Every message's end mark had Tk's default gravity, so
# it moved on with every message painted after it: each message ran to
# the end of the transcript, a point in any of them was in all the ones
# before it too, and "Copy message" copied whichever the array gave
# first (on the emulator, an engine note). "View source / rendered"
# flipped the wrong message the same way.
puts "\nthe message under a point is that message"
::vchat::clear
::vchat::forget_raw
pack .mid -fill both -expand 1
pack .mid.chatwrap -fill both -expand 1
pack .mid.chatwrap.chat -fill both -expand 1
pack .mid.chatwrap.chat.log -fill both -expand 1
wm geometry . 700x600
set first [::vchat::emit_message user "the FIRST message" {}]
::vchat::emit_note [dict create kind context summary "449 of context injected"]
set last [::vchat::emit_message assistant "the LAST message" {}]
update
set _w .mid.chatwrap.chat.log
proc _point_of {needle} {
    set i [$::_w search -exact $needle 1.0]
    $::_w see $i
    update
    lassign [$::_w bbox "$i +2c"] x y
    return [list $x $y]
}
lassign [_point_of FIRST] _x _y
ok "a point in the first message is the first message" \
    {[::vchat::message_at $_x $_y] eq $first}
clipboard clear
::vchat::copy_message_at $_x $_y
ok "…and Copy message copies it, not a later one" \
    {[clipboard get] eq "the FIRST message"}
# A point in the last message was in every message.
lassign [_point_of LAST] _x _y
ok "a point in the last message is the last message" \
    {[::vchat::message_at $_x $_y] eq $last}
clipboard clear
::vchat::copy_message_at $_x $_y
ok "…and Copy message there copies the last message" \
    {[clipboard get] eq "the LAST message"}
::vchat::toggle_source_at $_x $_y
ok "View source flips the message under the point and no other" \
    {$::vchat::raw_src($last) == 1 && $::vchat::raw_src($first) == 0}
set ::vchat::raw_src($last) 0

puts "\nprompt replay keeps one row"
::vchat::clear
set ::vchat::session "A"
set ::vchat::stream_session ""
set ::vchat::running {}
set ::vchat::busy 0
set ::vchat::turn_anchor {}
set ::vchat::turn_reload_anchor {}
catch { unset ::vchat::turn_prompt_ids }
catch { array unset ::vchat::ev_replay }
catch { array unset ::vchat::ev_reconnect_pending }
catch { array unset ::vchat::ev_snap_seen }
::vchat::on_session_event "A" turn-started \
    [dict create surface desktop started_at x base_msg_id 6 prompt_msg_id 8 prompt "one question only"]
::vchat::rebuild
set ::vchat::ev_reconnect_pending(A) 1
::vchat::on_session_event "A" history \
    [dict create role user content {[origin] test provenance} injection "origin:desktop" msg_id 7]
::vchat::on_session_event "A" history \
    [dict create role user content "one question only" msg_id 8]
::vchat::on_session_event "A" history \
    [dict create role user content "background context survives" injection background msg_id 9]
::vchat::on_session_event "A" turn-snapshot \
    [dict create surface desktop content "first partial" reasoning "" rounds {} base_msg_id 9]
ok "a rebuilt optimistic prompt is not appended again by its replay" \
    {[regexp -all {one question only} [body]] == 1}
ok "the replay keeps its provenance and background rows" \
    {[regexp -all {said on desktop} [body]] == 1 && \
     [regexp -all {background task completed} [body]] == 1}
ok "a transcript repaint preserves the turn's message identity" \
    {[dict exists $::vchat::turn_prompt_ids A 8]}
::vchat::on_session_event "A" session-live [dict create last_msg_id 9 last_note_id 0]
set ::vchat::ev_reconnect_pending(A) 1
::vchat::on_session_event "A" turn-snapshot \
    [dict create surface desktop content "second partial" reasoning "" rounds {} base_msg_id 9]
ok "a second reconnect keeps the prompt and its persisted context" \
    {[regexp -all {one question only} [body]] == 1 && \
     [regexp -all {said on desktop} [body]] == 1 && \
     [regexp -all {background task completed} [body]] == 1}
ok "the second snapshot replaces only the live region" \
    {[string match "*second partial*" [body]] && ![string match "*first partial*" [body]]}
::vchat::on_session_event "A" delta [dict create text "one answer"]
::vchat::on_session_event "A" turn-result [dict create text "one answer" \
    provider p model m prompt_tokens 1 completion_tokens 1 display_reasoning "" \
    notice "" last_msg_id 9]
ok "completion leaves one prompt followed by its answer" \
    {[regexp -all {one question only} [body]] == 1 && \
     [string first "one question only" [body]] < [string first "one answer" [body]]}
ok "a settled turn releases its prompt id map" \
    {![dict exists $::vchat::turn_prompt_ids A]}

# The same words in a LATER turn are a different store row, not a duplicate.
::vchat::on_session_event "A" turn-started \
    [dict create surface desktop started_at y base_msg_id 9 prompt_msg_id 10 prompt "one question only"]
ok "an identical prompt in a later turn is still painted" \
    {[regexp -all {one question only} [body]] == 2}

catch { image delete $_g }
catch { file delete -force $_mtmp }

puts "\nrendertest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
