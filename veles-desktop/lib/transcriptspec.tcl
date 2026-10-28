# veles-desktop/lib/transcriptspec.tcl — THE TRANSCRIPT SPEC, as data.
#
# The markdown renderer got this treatment first (mdspec.tcl, at the
# USER's explicit request) and it worked; this file is the same Pattern
# B applied to the transcript: the user's ground rules for how a chat
# transcript must look, each one an executable entry, run by
# transcriptspectest.tcl against the REAL chat.tcl procs in a real
# widget. Changing a rendering means changing an entry here first.
#
# ── the ground rules ────────────────────────────────────────────────
#
# From the rescue plan (item 2), quoting the user:
#
#   R1  logical blocks are separated by a blank line
#   R2  newlines inside output remain newlines (no welded blobs)
#   R3  a tool round renders once — the same way live and on reload
#   R4  reasoning collapses; it never vanishes
#   R5  a patch is shown as diff lines
#   R6  the live edge never orphans the view — follow survives a turn
#   R7  every reasoning segment is its own block, live and reloaded alike
#
# A `case` is a rule (or a step towards one) that HOLDS today; a `gap`
# pins today's behaviour where it does not yet meet the rule — a
# promise the behaviour will not drift silently. When a gap gets
# implemented, promote its entry to `case`. (mdspec.tcl's verdicts:
# a gap mismatch is a DRIFT, reported separately, not a failure.)
#
# ── entry shape ─────────────────────────────────────────────────────
#
#   ::tspec::case {name} {rule ref} {setup script} {checks…}
#   ::tspec::gap  {name} {rule ref} {setup script} {checks…}
#
# `setup` runs in a FRESH transcript (cleared widget, forgotten raw
# state, defaults restored — the runner does that between cases).
# Each check is {description script}; the script is `expr`-evaluated
# in the same scope and must be true. Checks inside one entry run in
# order and share the transcript the setup (and earlier checks) left.
#
# Everything runs under Xvfb via scripts/tcl_tests.sh. Assertions on
# what is VISIBLE use elide/tag state, never bare `$w get` — the get
# returns elided text too, which is how rendertest once stayed green
# while a block was invisible on screen (see rendertest.tcl's note).

namespace eval ::tspec {
    variable cases {}
}

proc ::tspec::case {name rule setup checks} {
    variable cases
    lappend cases [list case $name $rule $setup $checks]
}

proc ::tspec::gap {name rule setup checks} {
    variable cases
    lappend cases [list gap $name $rule $setup $checks]
}

# Shorthand for the checks list — keeps entries readable.
proc ::tspec::checks {args} { return $args }

# The engine emits this shape from filetools' PatchTool (summary line
# first, then a hunk-accurate unified diff built from the match
# offsets). The spec's patch cases render exactly this.
set ::tspec::patch_result {patched f.txt (1 replacement)
--- f.txt
+++ f.txt
@@ -1,7 +1,7 @@
 a
 b
 c
-d
+D
 e}

# ── R2: newlines inside output remain newlines ──────────────────────

# The tool HEADER is the one place a newline is wrong — it is a single
# line by construction. The args arrive with real newlines (JSON-decoded);
# the header flattens them and truncates. Was the "welded blob": a patch
# call dumped whole file fragments into one ⚙ line while the reload path
# truncated at 200 chars — the same call, two renderings.
::tspec::case {tool header is one line whatever the args carry} {R2} {
    ::vchat::emit_message tool "" {} [list [list patch "old: a\nb\nc\nnew: x" "patched"]]
} [::tspec::checks \
    {the ⚙ header flattens embedded newlines} \
    {[string match "*⚙ patch old: a b c new: x*" [body]]} \
    {and the header is exactly one line} \
    {[llength [regexp -all -inline {\n} [lindex [split [body] "\n"] 0]]] == 0 \
        && [string first "\n" [lindex [split [body] "\n"] 0]] == -1} \
]

# The tool RESULT keeps its newlines — behind the collapsible for the
# generic shape, verbatim. A flattened result is unreadable and lies
# about what the tool printed.
::tspec::case {a tool result keeps its newlines} {R2} {
    ::vchat::emit_message tool "" {} [list [list terminal "printf" "one\ntwo\nthree"]]
} [::tspec::checks \
    {the result body is stored verbatim} \
    {[string match "*one\ntwo\nthree*" [body]]} \
]

# ── R3: a tool round renders once — live and reload the same ───────

# The live path paints a preview line ("↳ …"), then on_done re-renders
# the turn through the SAME round renderer the reload ladder uses, so
# the final live transcript and a reopened one carry the same shape:
# ⚙ invocation header + expandable result — not ⚙ twice, not ↳ leftovers.
::tspec::case {a live round ends up shaped like a reloaded one} {R3} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::tool_rounds(S) {}
    dict set ::vchat::turn_anchor "S" [.mid.chatwrap.chat.log index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_tool "start" "terminal" [dict create args "sleep 30"]
    ::vchat::on_tool "end"   "terminal" [dict create result "done\nout"]
    ::vchat::on_done [dict create text "the final answer" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "" notice ""]
} [::tspec::checks \
    {the round reads as a call: name + invocation on the ⚙ line} \
    {[string match "*⚙ terminal sleep 30*" [body]]} \
    {the live preview line is gone after the reconcile} \
    {![string match "*↳*" [body]]} \
    {the result is expandable, exactly one round on screen} \
    {[llength [lsearch -all -inline [taglist] chdr_*]] == 1} \
    {and the answer follows the round} \
    {[string match "*the final answer*" [body]]} \
]

# The reload ladder paints the identical shape from the persisted rows.
::tspec::case {a reloaded round paints the same shape} {R3} {
    ::vchat::emit_message tool "" {} [list [list terminal "sleep 30" "done\nout"]]
} [::tspec::checks \
    {same ⚙ header} \
    {[string match "*⚙ terminal sleep 30*" [body]]} \
    {same expandable result} \
    {[llength [lsearch -all -inline [taglist] chdr_*]] == 1} \
]

# R3's second half, and the case the first live-markdown attempt shipped
# without: BODY TEXT obeys the same rule. The live answer streams as raw
# text — literal backticks on screen — but the reconciled turn must read
# like the reloaded one: a fence rendered into a code block, not left as
# `` ``` `` marks. (That attempt only ever streamed a single plain message
# with no reasoning/tool in the way and never measured a fence, which is
# exactly how the garble slipped through.)
::tspec::case {live body text renders like reloaded text, fences included} {R3} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    dict set ::vchat::turn_anchor "S" [.mid.chatwrap.chat.log index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_delta "before\n```\ncode\n```\nafter"
    ::vchat::on_done [dict create text "before\n```\ncode\n```\nafter" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "" notice ""]
} [::tspec::checks \
    {the fence rendered, not left literal} \
    {[string match "*before*" [body]] && ![string match "*```*" [body]]} \
    {the fenced body carries the codeblock tag} \
    {[llength [lsearch -all -inline [taglist] md_codeblock]] >= 1 \
        && [string match "*code*" [tagtext md_codeblock]]} \
    {answer text around the fence survives} \
    {[string match "*after*" [body]]} \
]

# R3's third half: the streamed answer is re-rendered IN PLACE across a
# tool round, not re-accumulated. A turn that streams answer text, calls a
# tool, then answers again used to (a) re-append every earlier chunk onto
# each later render and (b) let the region's anchor drift across the tool
# and reasoning lines, so the next flush deleted those lines and re-inserted
# the buffered answer — the "answer duplicated over and over, welded to a
# column of thinking" garble. Each delta region renders once and only once,
# and stays off the reasoning/tool lines around it.
::tspec::case {a streamed answer survives a tool round without duplicating} {R3} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    ::vchat::on_reasoning "think round one"
    ::vchat::on_delta "partial answer"
    ::vchat::on_tool "start" "shell" [dict create args ls]
    ::vchat::on_tool "end" "shell" [dict create result "file.txt"]
    ::vchat::on_reasoning "think round two"
    ::vchat::on_delta " and the rest"
    ::vchat::_flush_live_delta "S" 1
} [::tspec::checks \
    {the first answer chunk appears once, not twice} \
    {[llength [regexp -all -inline {partial answer} [body]]] == 1} \
    {the second answer chunk appears once, not twice} \
    {[llength [regexp -all -inline { and the rest} [body]]] == 1} \
    {the tool line and its result survive between the two} \
    {[string match "*⚙ shell*" [body]] && [string match "*file.txt*" [body]]} \
    {the answer is not welded to the reasoning before it} \
    {[string match "*think round one\npartial answer*" [body]]} \
]

# R3's fourth half, and the one that was missing: a BACKGROUND
# COMPLETION landing mid-turn. `on_note` closes the live regions before
# it paints — "so the note neither welds onto raw deltas nor gets
# deleted by the next flush" — and the three bus arms that announce
# finished background work (shell-complete, child-complete,
# monitor-fired) did not. USER 2026-09-18: "the session window mix[es]
# reasoning blocks with tool calls again with no newline between the end
# of reasoning and start of tool call … looks like an intermittent
# issue". Intermittent because it needs a child/shell/monitor to finish
# DURING a turn.
#
# `log_insert` appends at `end`, i.e. past `live_delta_mark`, and the
# next debounce tick opens with `$w delete $live_delta_mark end`. So the
# line was destroyed ~200ms after it was drawn.
::tspec::case {a background completion mid-turn survives the next flush} {R3} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::live_ctx ""
    set ::vchat::live_saved {}
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "thinking about it"
    ::vchat::on_delta "the answer so far"
    ::vchat::on_session_event "S" child-complete \
        [dict create id 7 goal "audit the thing" ok 1 child_session "CHILD1"]
    # The debounce tick that used to eat it.
    ::vchat::_flush_live_delta "S" 0
} [::tspec::checks \
    {the completion line is still on screen after the flush} \
    {[string match "*subagent 7 finished*" [body]]} \
    {and the attach pointer with it} \
    {[string match "*/resume CHILD1*" [body]]} \
    {the streamed answer is still there too} \
    {[string match "*the answer so far*" [body]]} \
]

# The second eraser: `on_done` replaces everything from `turn_anchor` to
# `end` with the reconciled blocks. A completion line painted inside
# that span is deleted a second time, so a turn that finished after a
# child did showed no sign the child had ever reported.
::tspec::case {a background completion mid-turn survives the reconcile} {R3} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::live_ctx ""
    set ::vchat::live_saved {}
    set ::vchat::tool_rounds(S) {}
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "think first"
    ::vchat::on_tool "start" "terminal" [dict create args "sleep 1"]
    ::vchat::on_tool "end"   "terminal" [dict create result "ok"]
    ::vchat::on_session_event "S" shell-complete \
        [dict create id 4 command "make -j8" exit_code 0]
    ::vchat::on_done [dict create text "the final answer" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "" notice ""]
} [::tspec::checks \
    {the shell's completion line outlives the turn that carried it} \
    {[string match "*shell 4 finished*" [body]]} \
    {and the turn still reconciled normally around it} \
    {[string match "*⚙ terminal sleep 1*" [body]] \
        && [string match "*the final answer*" [body]]} \
]

# ── R3, the half the segment model could not hold ───────────────────
#
# THE MODEL'S OWN PROSE. A turn that narrates between tool calls ("Now
# the remaining sites in shells.rs") is telling you what it is doing,
# and you can read it while it streams — then `on_done` deletes the
# region and rebuilds the turn WITHOUT it, because `live_segs` entries
# were {reason rounds} with nowhere to put the text. A reload brings it
# back, so the finished live transcript was missing text the model
# produced. USER 2026-09-18: "Intermediate prose missing is a failure
# mode, not an acceptable state."
::tspec::case {a round's answer text survives the reconcile} {R3} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::live_ctx ""
    set ::vchat::live_saved {}
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "thinking about the parser"
    ::vchat::on_delta "Now the remaining sites in shells.rs."
    ::vchat::on_tool "start" "patch" [dict create args "shells.rs"]
    ::vchat::on_tool "end"   "patch" [dict create result "patched"]
    ::vchat::on_reasoning "now the tests"
    ::vchat::on_delta "That is every site."
    ::vchat::on_done [dict create text "That is every site." provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "now the tests" notice ""]
} [::tspec::checks \
    {the prose the round narrated is still there afterwards} \
    {[string match "*Now the remaining sites in shells.rs.*" [body]]} \
    {…exactly once} \
    {[llength [regexp -all -inline {Now the remaining sites} [body]]] == 1} \
    {…below the reasoning that led to it} \
    {[string first "thinking about the parser" [body]] \
        < [string first "Now the remaining sites" [body]]} \
    {…and above the call it introduced} \
    {[string first "Now the remaining sites" [body]] < [string first "⚙ patch" [body]]} \
    {the answer still closes the turn, once} \
    {[string first "⚙ patch" [body]] < [string first "That is every site." [body]] \
        && [llength [regexp -all -inline {That is every site} [body]]] == 1} \
]

# The shape a third slot on the segment could not express. Segments are
# delimited by REASONING, so a round that thinks nothing opens none —
# its prose and its call would fold into the previous segment and come
# out as `r1 · p1p2 · t1 · t2` when the turn went `r1 · p1 · t1 · p2 ·
# t2`. Rounds with no reasoning at all are the single commonest shape in
# the live store (10,618 assistant rows carry neither reasoning nor
# content; another 15,234 carry both), so this is the normal case, not a
# corner.
::tspec::case {prose from a round with no reasoning keeps its own place} {R3} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::live_ctx ""
    set ::vchat::live_saved {}
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "only the first round thinks"
    ::vchat::on_delta "PROSE-ONE"
    ::vchat::on_tool "start" "terminal" [dict create args "ls"]
    ::vchat::on_tool "end"   "terminal" [dict create result "a.txt"]
    ::vchat::on_delta "PROSE-TWO"
    ::vchat::on_tool "start" "read_file" [dict create args "a.txt"]
    ::vchat::on_tool "end"   "read_file" [dict create result "contents"]
    ::vchat::on_delta "PROSE-THREE"
    ::vchat::on_done [dict create text "PROSE-THREE" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "" notice ""]
} [::tspec::checks \
    {all three pieces of prose survive} \
    {[string match "*PROSE-ONE*" [body]] && [string match "*PROSE-TWO*" [body]] \
        && [string match "*PROSE-THREE*" [body]]} \
    {and each one sits between the calls it actually sat between} \
    {[string first "PROSE-ONE" [body]] < [string first "⚙ terminal" [body]] \
        && [string first "⚙ terminal" [body]] < [string first "PROSE-TWO" [body]] \
        && [string first "PROSE-TWO" [body]] < [string first "⚙ read_file" [body]] \
        && [string first "⚙ read_file" [body]] < [string first "PROSE-THREE" [body]]} \
]

# ── R7: a reasoning SEGMENT is a provider CALL ──────────────────────
#
# A provider that emits thinking, then content, then more thinking emits
# ONE call, and the engine persists it as ONE row with ONE reasoning
# field — so a reopen shows one collapsible. The live path minted a
# fresh region per burst (correctly — that is how it streams) and the
# reconcile KEPT both, so the finished live transcript claimed the turn
# had two reasoning blocks where the stored one has a single one.
::tspec::case {two reasoning bursts in one call reconcile to one collapsible} {R7} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::live_ctx ""
    set ::vchat::live_saved {}
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "first burst"
    ::vchat::on_delta "the answer"
    ::vchat::on_reasoning "second burst"
    ::vchat::on_done [dict create text "the answer" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "second burst" notice ""]
    set nreason 0
    foreach t [lsearch -all -inline [taglist] chdr_*] {
        if {[string match "*reasoning*" [tagtext $t]]} { incr nreason }
    }
} [::tspec::checks \
    {one collapsible, matching the single row the call becomes} \
    {$nreason == 1} \
    {and neither burst is lost} \
    {[string match "*first burst*" [body]] && [string match "*second burst*" [body]]} \
]

# ── R3, made structural ─────────────────────────────────────────────
#
# Every entry above asserts something ABOUT live-vs-reload. This one
# asserts the thing itself: drive a turn through the live handlers,
# then drive the ROWS that same turn persists through `_paint_one_row`
# — the real reload ladder, not a hand-written stand-in — and require
# the two transcripts to be the same characters.
#
# The stand-in is how this drifted. `the reload ladder paints the same
# one-per-segment shape` below used hand-rolled emit_message calls that
# happened to omit the per-round prose, so it agreed with a reconcile
# that dropped it and the divergence lived behind a green suite.
# USER 2026-09-18: "Intermediate prose missing is a failure mode".
#
# The one live-only line is on_done's `· provider/model · N→N tok`
# footer, which is turn chrome and not part of the transcript; it is
# trimmed before the comparison. The time stamps are not compared either
# (the suite's `body` drops them: a live block is stamped by this clock,
# a reloaded row by the server's). Nothing else is normalised.
::tspec::case {a live turn and a reload of its rows paint the same transcript} {R3} {
    proc _turnbody {} {
        set b [body]
        set cut [string first "· p/m ·" $b]
        if {$cut >= 0} { set b [string range $b 0 [expr {$cut - 1}]] }
        return [string trimright $b]
    }

    # ── the live path ───────────────────────────────────────────────
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::live_ctx ""
    set ::vchat::live_saved {}
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "weigh the options"
    ::vchat::on_delta "First I will look at the parser."
    ::vchat::on_tool "start" "read_file" [dict create args "parser.rs"]
    ::vchat::on_tool "end"   "read_file" [dict create result "fn parse() {}"]
    ::vchat::on_delta "Now the store."
    ::vchat::on_tool "start" "terminal" [dict create args "grep -n store"]
    ::vchat::on_tool "end"   "terminal" [dict create result "12: store"]
    ::vchat::on_reasoning "that is everything"
    ::vchat::on_delta "Both sites are covered."
    ::vchat::on_done [dict create text "Both sites are covered." provider p model m \
        prompt_tokens 1 completion_tokens 2 \
        display_reasoning "that is everything" notice ""]
    set live [_turnbody]

    # ── the same turn, as the rows it persists ──────────────────────
    fresh
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set rows [list \
        [dict create role assistant content "First I will look at the parser." \
             display_reasoning "weigh the options" \
             tool_calls [list [dict create id c1 \
                 function [dict create name read_file arguments "parser.rs"]]]] \
        [dict create role tool tool_name read_file tool_call_id c1 \
             content "fn parse() {}"] \
        [dict create role assistant content "Now the store." \
             display_reasoning "" \
             tool_calls [list [dict create id c2 \
                 function [dict create name terminal arguments "grep -n store"]]]] \
        [dict create role tool tool_name terminal tool_call_id c2 \
             content "12: store"] \
        [dict create role assistant content "Both sites are covered." \
             display_reasoning "that is everything"]]
    ::vchat::_paint_history_rows $rows 0 [llength $rows]
    set reload [_turnbody]
} [::tspec::checks \
    {the live transcript is not empty (a vacuous pass is not a pass)} \
    {[string length $live] > 80} \
    {every piece of prose the turn produced is in the live one} \
    {[string match "*First I will look at the parser.*" $live] \
        && [string match "*Now the store.*" $live] \
        && [string match "*Both sites are covered.*" $live]} \
    {and the live turn is character-for-character the reloaded one} \
    {$live eq $reload} \
]

# ── R5: a patch is shown as diff lines ─────────────────────────────

# The engine builds the diff from the exact match offsets and carries
# it in the result; the per-tool renderer paints it AS a diff — adds,
# dels and hunk headers tagged, file labels not mistaken for content —
# in the round itself, not behind a click.
::tspec::case {a patch round renders as diff lines} {R5} {
    ::vchat::emit_message tool "" {} [list [list patch \
        {{"path":"f.txt"}} $::tspec::patch_result]]
} [::tspec::checks \
    {the summary line leads} \
    {[string match "*patched f.txt (1 replacement)*" [body]]} \
    {removed lines are tagged as dels} \
    {[tagtext diffdel] eq "  -d\n"} \
    {added lines are tagged as adds} \
    {[tagtext diffadd] eq "  +D\n"} \
    {hunk headers are tagged as headers} \
    {[tagtext diffhdr] eq "  @@ -1,7 +1,7 @@\n"} \
    {file labels are not diff content} \
    {![string match "*--- f.txt*" [tagtext diffdel]] \
        && ![string match "*+++ f.txt*" [tagtext diffadd]]} \
]

# Live and reload agree for the patch shape too — same renderer, one
# entry point per round kind.
::tspec::case {a live patch round reconciles to the same diff} {R5} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::tool_rounds(S) {}
    dict set ::vchat::turn_anchor "S" [.mid.chatwrap.chat.log index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_tool "start" "patch" [dict create args {{"path":"f.txt"}}]
    ::vchat::on_tool "end"   "patch" [dict create result $::tspec::patch_result]
    ::vchat::on_done [dict create text "patched it" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "" notice ""]
} [::tspec::checks \
    {the reconciled round carries the diff lines} \
    {[string match "*-d*" [body]] && [string match "*+D*" [body]]} \
    {colored like the reloaded shape} \
    {[tagtext diffadd] eq "  +D\n" && [tagtext diffdel] eq "  -d\n"} \
]

# ── R4: reasoning collapses, never vanishes ─────────────────────────

# Collapsed by default with a clickable header; the body text stays in
# the buffer (elide, not delete). Toggle open→closed→open and the block
# must still be there — the "blocks just disappear, only reopening the
# transcript brings them back" bug is pinned by rendertest's
# display-chars assertions; this entry pins the transcript-level shape.
::tspec::case {reasoning collapses and comes back} {R4} {
    ::vchat::emit_message assistant "the answer" "because\nof reasons"
    set key [lindex [lsearch -all -inline [taglist] cbody_*] 0]
    set rkey [string range $key 6 end]
} [::tspec::checks \
    {collapsed by default with a header} \
    {[llength [lsearch -all -inline [taglist] chdr_*]] >= 1 \
        && [$w tag cget cbody_$rkey -elide] == 1} \
    {expanding shows the body} \
    {[::vchat::_toggle_collapsible $rkey] == 1 \
        && [$w tag cget cbody_$rkey -elide] == 0} \
    {collapsing again hides but keeps it} \
    {[::vchat::_toggle_collapsible $rkey] == 0 \
        && [$w tag cget cbody_$rkey -elide] == 1 \
        && [string match "*because*" [body]]} \
    {the header never inherits the body tag} \
    {[lsearch -exact [$w tag names [lindex [$w tag ranges chdr_$rkey] 0]] cbody_$rkey] < 0} \
]

# The live reasoning tag is reaped when the turn reconciles (R4 hygiene).
# on_reasoning streams each turn into a FRESH live_r_* tag; on_done
# deletes the live region — the only text carrying it — and re-renders
# the reasoning as a message-keyed collapsible. Deleting the region
# removes the tag's RANGES but not its entry in the widget's tag table,
# so without an explicit `tag delete` one dead live_r_* config leaked per
# turn for the life of the widget. The reasoning is NOT lost (it rides
# back as a collapsible — R4, never vanishes); only the dead live tag is
# dropped. forget_raw is the session-change catch-all for the paths that
# leave live text on screen (turn_error, a reasoning-only turn).
::tspec::case {the live reasoning tag is reaped when the turn reconciles} {R4} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    dict set ::vchat::turn_anchor "S" [.mid.chatwrap.chat.log index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "thinking hard\n"
    ::vchat::on_done [dict create text "the answer" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "thinking hard" notice ""]
} [::tspec::checks \
    {no dead live_r_* tag config survives the turn} \
    {[lsearch -glob [taglist] live_r_*] == -1} \
    {and the reasoning still rode back as a collapsible (R4 — never vanishes)} \
    {[llength [lsearch -all -inline [taglist] cbody_*]] >= 1 \
        && [string match "*the answer*" [body]]} \
]

# Hiding the LIVE reasoning COLLAPSES it; it does not vanish (R4). The
# "reasoning" checkbox (show_reasoning) elides the live body — without a
# header that left nothing on screen and nothing to click back open.
# USER: "when unchecking the verbose thinking it just disappears instead
# of collapsing". The header tag (live_r_hdr_*) stays visible and labelled,
# the body (live_r_*) folds away, exactly like a completed collapsible.
::tspec::case {hiding live reasoning collapses to a header, not nothing} {R4} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::show_reasoning 1
    dict set ::vchat::turn_anchor "S" [.mid.chatwrap.chat.log index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "thinking hard\n"
    # Verbose: the body is on screen under an open header.
    set verbose_body_shown [expr {[$w tag cget $::vchat::live_rtag -elide] == 0}]
    # Uncheck "reasoning" mid-stream: the body must fold, the header stay.
    set ::vchat::show_reasoning 0
    ::vchat::_toggle_live_reasoning
} [::tspec::checks \
    {verbose thinking shows the live body} \
    {$verbose_body_shown} \
    {unchecking elides the body…} \
    {[$w tag cget $::vchat::live_rtag -elide] == 1} \
    {…but the header survives — visible, labelled, clickable (collapse, not vanish)} \
    {$::vchat::live_rhdr ne "" \
        && [$w tag cget $::vchat::live_rhdr -elide] == 0 \
        && [string match "*reasoning*" [tagtext $::vchat::live_rhdr]]} \
]

# ── R1: blocks are separated by a blank line ────────────────────────
#
# Separation is a property of the model now: every message-shaped block
# (user, assistant, tool, note) goes through one painter that ensures
# exactly one blank line before it. Before that, each painter decided
# for itself — the live send added a break the reload path did not,
# assistant bodies ended in a blank nothing else got, and user messages
# and tool rounds welded onto whatever preceded them.

::tspec::case {consecutive blocks are blank-separated exactly once} {R1} {
    ::vchat::emit_message assistant "one"
    ::vchat::emit_message assistant "two"
    ::vchat::emit_message assistant "three"
} [::tspec::checks \
    {one blank between the first two — never none} \
    {[string match "*one*\n\ntwo*" [body]]} \
    {never two} \
    {![string match "*one*\n\n\ntwo*" [body]]} \
    {and the same further down} \
    {[string match "*two*\n\nthree*" [body]] && ![string match "*two*\n\n\nthree*" [body]]} \
]

::tspec::case {a user message is separated from the tool round that follows} {R1} {
    ::vchat::emit_message user "the question"
    ::vchat::emit_message assistant "" "" [list [list read_file "{}" "x"]]
} [::tspec::checks \
    {exactly one blank between question and round} \
    {[string match "*the question*\n\n⚙ read_file*" [body]] \
        && ![string match "*the question*\n\n\n⚙ read_file*" [body]]} \
]

# Live and reload agree: the live path used to add its own break after
# the optimistic user message (send_current's `log_insert "\n"`), so the
# transcript you watched and the one you reopened were laid out
# differently. The separator owns the blank now; this case pins the
# LIVE shape (the optimistic user message painted by send's helper, then
# the reconciled reply) against the reload shape above.
::tspec::case {the live turn region matches the reload layout} {R1} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    dict set ::vchat::turn_reload_anchor "S" [.mid.chatwrap.chat.log index end-1c]
    ::vchat::emit_message user "live question"
    set ::vchat::tool_rounds(S) {}
    dict set ::vchat::turn_anchor "S" [.mid.chatwrap.chat.log index end-1c]
    ::vchat::on_done [dict create text "live answer" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "" notice ""]
} [::tspec::checks \
    {the answer is separated from the question exactly like a reload} \
    {[string match "*live question*\n\nlive answer*" [body]] \
        && ![string match "*live question*\n\n\nlive answer*" [body]]} \
]

::tspec::case {a note is a block, separated like the rest} {R1} {
    ::vchat::emit_message assistant "body"
    ::vchat::emit_note [dict create kind guard summary "a guard fired"]
} [::tspec::checks \
    {exactly one blank before the note line} \
    {[string match "*body*\n\n*⚠ a guard fired*" [body]] \
        && ![string match "*body*\n\n\n*⚠ a guard fired*" [body]]} \
]

# A background completion is a block too, and the kind-break has to know
# it happened. The three bus arms painted with `log_insert` and never
# called `_live_break`, so `live_last_kind` still said "reasoning" when
# the next tool round asked for its separator — and a transition that
# compares equal returns no break at all. USER 2026-09-18: "no newline
# between the end of reasoning and start of tool call".
::tspec::case {a background completion does not weld the next block onto the last} {R1} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::live_ctx ""
    set ::vchat::live_saved {}
    set ::vchat::tool_rounds(S) {}
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "thinking hard"
    ::vchat::on_session_event "S" monitor-fired \
        [dict create id 2 condition "ci is green" status fired]
    ::vchat::on_tool "start" "read_file" [dict create args "{}"]
} [::tspec::checks \
    {the monitor line starts on its own line, not welded to the thinking} \
    {[string match "*thinking hard\n*⏰ monitor 2 fired*" [body]]} \
    {and the tool round that follows starts on its own line, not welded} \
    {[string match "*⏰ monitor 2 fired: ci is green\n⚙ read_file*" [body]]} \
]

# ── R6: the live edge never orphans the view ────────────────────────
#
# Follow is armed by default and pulls only when the view was already
# at the live edge when content arrived. The failure this rule pins:
# ONE unbracketed insert — the kind-transition newline, inserted between
# its own prestash and the caller's — made the caller's after-the-fact
# yview read say "not at bottom", which disarmed follow for the REST of
# the turn: the stream arrived below a view that never came down.
# USER: "follow stops on a tool call when /reasoning is on, but not
# always — hard to tell why it unsticks. Maybe two implementations are
# fighting for control?" (There is one implementation; the fight was an
# insert escaping its bracket.)

::tspec::case {follow stays pinned through a whole live turn} {R6} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::follow 1
    ::vchat::_mark_running "S" 1
    # Enough content that the view genuinely scrolls, parked at the live
    # edge like a watching user.
    for {set i 0} {$i < 60} {incr i} {
        ::vchat::log_insert "filler line $i to make the view scroll\n" status
    }
    $w see end
    update idletasks
    set was_bottom [expr {[lindex [$w yview] 1] >= 0.999}]
    # Chunks that do NOT end in newlines: every kind transition wants
    # its break inserted — the exact shape that used to kill follow.
    ::vchat::on_reasoning "thinking about it"
    update idletasks
    set y_after_reasoning [lindex [$w yview] 1]
    ::vchat::on_tool "start" "terminal" [dict create args "sleep 30"]
    update idletasks
    set y_after_tool_start [lindex [$w yview] 1]
    ::vchat::on_tool "end" "terminal" [dict create result "done output"]
    update idletasks
    set y_after_tool_end [lindex [$w yview] 1]
    ::vchat::on_reasoning "thinking again"
    update idletasks
    set y_after_reasoning2 [lindex [$w yview] 1]
    ::vchat::on_delta "the answer"
    update idletasks
    set y_after_delta [lindex [$w yview] 1]
} [::tspec::checks \
    {precondition: the view was at the live edge before the turn} \
    {$was_bottom} \
    {still pinned after the reasoning chunk} \
    {$y_after_reasoning >= 0.999} \
    {still pinned after the reasoning→tool transition (the old killer)} \
    {$y_after_tool_start >= 0.999} \
    {still pinned after the tool result} \
    {$y_after_tool_end >= 0.999} \
    {still pinned after the tool→reasoning transition} \
    {$y_after_reasoning2 >= 0.999} \
    {still pinned after the answer} \
    {$y_after_delta >= 0.999} \
]

::tspec::case {a view scrolled up mid-turn is never yanked back} {R6} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::follow 1
    ::vchat::_mark_running "S" 1
    for {set i 0} {$i < 60} {incr i} {
        ::vchat::log_insert "filler line $i to make the view scroll\n" status
    }
    # The user scrolls UP while the turn streams: reading wins over
    # arrival, and the ▾ button is the way back.
    $w yview moveto 0.0
    update idletasks
    ::vchat::on_reasoning "streamed thinking"
    ::vchat::on_tool "start" "terminal" [dict create args "sleep 30"]
    ::vchat::on_tool "end" "terminal" [dict create result "done output"]
    update idletasks
    set top [lindex [$w yview] 0]
    set btn [winfo exists .mid.chatwrap.chat.follow]
} [::tspec::checks \
    {the view is still near the top — no yank} \
    {$top < 0.05} \
    {and the ▾ new-output button says what arrived instead} \
    {$btn} \
]

# ── R7: every reasoning segment is its own block ─────────────────────
#
# The engine persists ONE assistant row per provider call, so a turn
# with tool rounds carries several reasoning SEGMENTS — and a reopen
# renders each as its own collapsible. The live view used to mint ONE
# live_r_* region per turn and pour every segment into it with the ⚙/↳
# lines interleaved: one block, mixed with tool calls, one toggle.
# USER: "the /reasoning toggle only collapses all the live blocks at
# once. When I reload the transcript they are correctly grouped into
# blocks; on the live view they are all in one block mixed with tool
# calls." Live must group exactly like reload: a fresh region per
# segment, headers between the tool rounds, and a reconcile that keeps
# every segment.

::tspec::case {a turn's reasoning streams as one block per segment} {R7} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::show_reasoning 1
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "first thoughts"
    ::vchat::on_tool "start" "terminal" [dict create args "sleep 30"]
    ::vchat::on_tool "end" "terminal" [dict create result "done output"]
    ::vchat::on_reasoning "second thoughts"
    ::vchat::on_tool "start" "read_file" [dict create args "{}"]
    ::vchat::on_tool "end" "read_file" [dict create result "file contents"]
    ::vchat::on_reasoning "third thoughts"
    set hdrs [lsearch -all -inline [taglist] live_r_hdr_*]
    set bodies {}
    foreach t [lsearch -all -inline [taglist] live_r_*] {
        if {![string match live_r_hdr_* $t]} { lappend bodies $t }
    }
    set first_body [tagtext [lindex $bodies 0]]
    set second_body [tagtext [lindex $bodies 1]]
} [::tspec::checks \
    {three segments mint three live headers} \
    {[llength $hdrs] == 3} \
    {and three live body regions} \
    {[llength $bodies] == 3} \
    {each body carries only its own segment's text} \
    {[string match "*first thoughts*" $first_body] \
        && ![string match "*second*" $first_body] \
        && [string match "*second thoughts*" $second_body]} \
]

::tspec::case {hiding live reasoning folds every segment, headers stay} {R7} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::show_reasoning 1
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "first thoughts"
    ::vchat::on_tool "start" "terminal" [dict create args "sleep 30"]
    ::vchat::on_tool "end" "terminal" [dict create result "done output"]
    ::vchat::on_reasoning "second thoughts"
    ::vchat::on_tool "start" "read_file" [dict create args "{}"]
    ::vchat::on_tool "end" "read_file" [dict create result "contents"]
    ::vchat::on_reasoning "third thoughts"
    set ::vchat::show_reasoning 0
    ::vchat::_toggle_live_reasoning
    set elides {}
    foreach t [lsearch -all -inline [taglist] live_r_*] {
        if {![string match live_r_hdr_* $t]} {
            lappend elides [$w tag cget $t -elide]
        }
    }
    set hdr_vis {}
    foreach t [lsearch -all -inline [taglist] live_r_hdr_*] {
        lappend hdr_vis [$w tag cget $t -elide]
    }
    set hdr_text ""
    foreach t [lsearch -all -inline [taglist] live_r_hdr_*] {
        append hdr_text [tagtext $t]
    }
} [::tspec::checks \
    {every live body folds — not just the newest segment} \
    {$elides eq {1 1 1}} \
    {every header stays visible} \
    {$hdr_vis eq {0 0 0}} \
    {and says folded, click to show} \
    {[string match "*▶*" $hdr_text]} \
]

::tspec::case {a multi-segment turn reconciles one collapsible per segment} {R7} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "first thoughts"
    ::vchat::on_tool "start" "terminal" [dict create args "sleep 30"]
    ::vchat::on_tool "end" "terminal" [dict create result "done output"]
    ::vchat::on_reasoning "second thoughts"
    ::vchat::on_tool "start" "read_file" [dict create args "{}"]
    ::vchat::on_tool "end" "read_file" [dict create result "contents"]
    ::vchat::on_reasoning "third thoughts"
    ::vchat::on_delta "the final answer"
    ::vchat::on_done [dict create text "the final answer" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "third thoughts" notice ""]
    set nreason 0
    foreach t [lsearch -all -inline [taglist] chdr_*] {
        if {[string match "*reasoning*" [tagtext $t]]} { incr nreason }
    }
} [::tspec::checks \
    {three reasoning collapsibles — one per segment} \
    {$nreason == 3} \
    {EARLY segments survive the reconcile, not just the last} \
    {[string match "*first thoughts*" [body]] \
        && [string match "*second thoughts*" [body]]} \
    {the answer is there} \
    {[string match "*the final answer*" [body]]} \
    {and no live tags remain} \
    {[lsearch -glob [taglist] live_r_*] == -1} \
]

# The reopen shape the reconcile above must equal: one collapsible per
# persisted ROW, tool rounds interleaved between them — driven through
# the REAL ladder (`_paint_history_rows` → `_paint_one_row`) against the
# rows the turn above persists.
#
# This used to hand-roll the ladder out of emit_message calls, and the
# hand-rolled version happened to omit the per-round prose — so it
# agreed with a reconcile that dropped it, and the divergence lived
# behind a green suite until the user found it in a session. A stand-in
# for the path under test proves what the stand-in does.
::tspec::case {the reload ladder paints the same one-per-row shape} {R7} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set rows [list \
        [dict create role assistant content "" display_reasoning "first thoughts" \
             tool_calls [list [dict create id c1 \
                 function [dict create name terminal arguments "sleep 30"]]]] \
        [dict create role tool tool_name terminal tool_call_id c1 \
             content "done output"] \
        [dict create role assistant content "" display_reasoning "second thoughts" \
             tool_calls [list [dict create id c2 \
                 function [dict create name read_file arguments "{}"]]]] \
        [dict create role tool tool_name read_file tool_call_id c2 \
             content "contents"] \
        [dict create role assistant content "the final answer" \
             display_reasoning "third thoughts"]]
    ::vchat::_paint_history_rows $rows 0 [llength $rows]
    set nreason 0
    foreach t [lsearch -all -inline [taglist] chdr_*] {
        if {[string match "*reasoning*" [tagtext $t]]} { incr nreason }
    }
} [::tspec::checks \
    {same three reasoning collapsibles} \
    {$nreason == 3} \
    {each round's invocation comes back from the assistant row's tool_calls} \
    {[string match "*⚙ terminal sleep 30*" [body]] \
        && [string match "*⚙ read_file*" [body]]} \
    {and the answer closes it} \
    {[string first "⚙ read_file" [body]] < [string first "the final answer" [body]]} \
]

# The slash command and the checkbox drive ONE state through ONE proc;
# the command used to flip the variable and nothing else, so
# /reasoning mid-stream changed neither the elide nor the header.
::tspec::case {the /reasoning command folds the live region too} {R7} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::show_reasoning 1
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "live thinking"
    ::vchat::slash_command "/reasoning"
} [::tspec::checks \
    {the command (not just the checkbox) elides the live body} \
    {[$w tag cget $::vchat::live_rtag -elide] == 1} \
    {and leaves the header to click back open} \
    {$::vchat::live_rhdr ne "" \
        && [$w tag cget $::vchat::live_rhdr -elide] == 0} \
]

# The fallback nobody had a case for. When `live_blocks` is empty at turn
# end — a turn this window never saw start — `on_done` rebuilds it from
# the done event's SINGLE `display_reasoning`, which `wrap_for_bus`
# (agent.rs) fills with the LAST provider call's reasoning only. The old
# fallback stacked every tool round of the turn under that one block:
# one collapsible claiming to be the thinking behind calls it never saw,
# and every earlier round's thinking simply absent. USER 2026-09-18:
# "these reasoning blocks seem to also all get merged into one single
# reasoning even though they were actually interspersed with tool calls".
#
# What a RELOAD of the same turn paints is the shape to match: the
# rounds in order, then the answer carrying the reasoning that led to
# it. With no per-round record there is nothing better to claim, and
# claiming less is the honest half.
::tspec::case {a turn with no live segments does not stack every round under one reasoning} {R7} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::live_ctx ""
    set ::vchat::live_saved {}
    set ::vchat::live_blocks {}
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    set ::vchat::tool_rounds(S) [list \
        [list terminal  "ls"   "a b"] \
        [list read_file "{}"   "contents"] \
        [list patch     "p"    "patched"]]
    ::vchat::on_done [dict create text "the answer" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "final thoughts" notice ""]
} [::tspec::checks \
    {every round the turn made is still on screen} \
    {[string match "*⚙ terminal ls*" [body]] \
        && [string match "*⚙ read_file*" [body]] \
        && [string match "*⚙ patch p*" [body]]} \
    {the rounds come FIRST — the one known reasoning is not claimed as theirs} \
    {[string first "⚙ terminal ls" [body]] < [string first "final thoughts" [body]]} \
    {and it rides with the answer it actually led to} \
    {[string first "final thoughts" [body]] < [string first "the answer" [body]]} \
    {exactly one reasoning collapsible, not one per round} \
    {[llength [lsearch -all -inline [taglist] reasonhdr*]] == 1} \
]

# The same fallback with NO reasoning at all: `segments` stayed empty,
# `n == 0`, and the branch that runs then emits the reply and nothing
# else — while the delete above had already wiped the live ⚙ lines. The
# turn's tool rounds disappeared off the screen entirely.
::tspec::case {a segment-less turn with no reasoning still shows its rounds} {R7} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::live_ctx ""
    set ::vchat::live_saved {}
    set ::vchat::live_blocks {}
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    set ::vchat::tool_rounds(S) [list [list terminal "make" "built"]]
    ::vchat::on_done [dict create text "done building" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "" notice ""]
} [::tspec::checks \
    {the round survives a turn that carried no reasoning} \
    {[string match "*⚙ terminal make*" [body]]} \
    {and the answer follows it} \
    {[string first "⚙ terminal make" [body]] < [string first "done building" [body]]} \
]

# ── R4: a FAILED turn's reasoning is still reasoning ─────────────────
#
# The history this guard carries: `turn_error` once left the dead
# turn's live region on screen raw, and the next turn's `on_done` reaped
# the live tags by GLOB over the whole widget:
#
#     foreach t [$w tag names] {
#         if {[string match "live_r_*" $t]} { $w tag delete $t }
#     }
#
# `tag names` is every tag in the TRANSCRIPT, not the ones this turn
# painted, so the dead turn's region lost its tag — and `forget_raw`
# already says what Tk does then, one proc away: the text is "silently
# re-created unstyled and un-elided". The earlier thinking kept its
# words and lost its colour, its italic and (with the box unchecked) its
# fold, and read as the model's own output.
#
# USER 2026-09-18: "it may have corrupted the transcript a little bit,
# it's showing the reasoning blocks before it as if it was model
# output?" — after a turn that died with "[veles] all routes failed",
# which is an Err return (agent.rs) and therefore turn-error.
#
# The fix went further than the tag reap: a failed turn is still a turn
# (R3), so `turn_error` now runs the SAME reconcile `on_done` runs —
# the raw live region is replaced by the blocks a reload paints. The
# dead turn's thinking survives AS that render: a proper collapsible
# with the reasoning colour and its own fold, not a raw tag whose only
# protection is the reap above staying careful.
::tspec::case {a failed turn's reasoning survives the NEXT turn's reconcile} {R4} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::show_reasoning 1
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "the thinking the network killed"
    ::vchat::_flush_live_reasoning "S"
    ::vchat::turn_error "all routes failed: network"
    # …and now an ordinary, successful turn in the same transcript.
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_reasoning "thinking that got through"
    ::vchat::_flush_live_reasoning "S"
    ::vchat::on_done [dict create text "the answer" provider p model m \
        prompt_tokens 1 completion_tokens 2 display_reasoning "" notice ""]
    # Which collapsible the dead turn's thinking became: reason keys are
    # msg_seq-derived and shared between both turns' blocks, so take the
    # one whose BODY carries the dead turn's text.
    set deadkey ""
    foreach key [array names ::vchat::collapse_open reason*] {
        if {[string match "*the thinking the network killed*" [body]]} { set deadkey $key }
    }
    set body_tag "cbody_$deadkey"
    set hdr_tag  "chdr_$deadkey"
    set deadtags {}
    foreach t [lsearch -all -inline [taglist] live_r_*] { lappend deadtags $t }
} [::tspec::checks \
    {the dead turn's thinking is still on screen} \
    {[string match "*the thinking the network killed*" [body]]} \
    {as the reconciled collapsible — header line present} \
    {[string match "*reasoning (1 lines, click to expand)*" [body]]} \
    {carrying the reasoning colour, not the transcript's default} \
    {[$w tag cget $body_tag -foreground] eq $::vt::T(reasoning)} \
    {…and foldable: the elide the checkbox drives is still its own} \
    {[$w tag cget $body_tag -elide] == 1} \
    {while the finished turn's own live tags ARE reaped (no leak per turn)} \
    {[llength $deadtags] == 0} \
]

# ── a failed turn says so, in the transcript ────────────────────────
#
# USER: "errors are missing or merged into the transcript as model
# output". Two failure shapes: a turn that streamed nothing (the error
# lived ONLY in the status bar → "missing"), and a turn that died after
# streaming (the engine's `[veles] all routes failed` row reconciled in
# as ordinary prose → "merged into model output"). `turn_error` puts the
# reason in the transcript as a `system` row — a block, where the answer
# would have been — so a reload shows it too.
::tspec::case {a failed turn states the failure in the transcript} {R3} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    # Nothing streamed before the failure — the shape that used to leave
    # only a status line.
    ::vchat::turn_error "all routes failed: network"
} [::tspec::checks \
    {the transcript carries the failure} \
    {[string match "*✖ turn failed: all routes failed: network*" [body]]} \
    {…and the turn is no longer marked running} \
    {[::vchat::is_running "S"] == 0} \
]

# The second shape: prose streamed, then the turn died. The streamed
# prose is the copy of record (reply "" — nothing replaces it), and the
# failure line lands AFTER it, so the transcript reads "here is what it
# got through, then it failed" rather than ending on a wall of text with
# no explanation.
::tspec::case {a failure after streaming follows the streamed prose} {R3} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_delta "partial answer before the net died"
    ::vchat::turn_error "all routes failed: network"
} [::tspec::checks \
    {the streamed prose survives} \
    {[string match "*partial answer before the net died*" [body]]} \
    {and the failure is stated after it} \
    {[string first "partial answer" [body]] < [string first " turn failed" [body]]} \
]

# ─ the live round's result is the round's own ──────────────────────
#
# The "tool output blocks only show the true output/format at turn end"
# report: the live paint was `_tool_preview` (a flattened one-liner) and
# the expandable block only appeared when on_done reconciled. Now the
# result half is painted when it ARRIVES, in the reload ladder's own
# shape — so a long tool result is expandable while the turn still runs.
::tspec::case {a tool result is expandable while the turn still runs} {R3} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::tool_rounds(S) {}
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    ::vchat::on_tool "start" "terminal" [dict create args "sort big.txt"]
    # …and the result lands. No on_done yet — the turn is still open.
    ::vchat::on_tool "end" "terminal" [dict create result "aaa\nbbb\nccc\nddd"]
} [::tspec::checks \
    {the round is on screen before turn end} \
    {[string match "* terminal sort big.txt*" [body]]} \
    {with its result already expandable, not a flattened stub} \
    {[llength [lsearch -all -inline [taglist] chdr_*]] == 1} \
    {…and the result body carries every line verbatim} \
    {[string match "*aaa*bbb*ccc*ddd*" [tagtext [lsearch -inline [taglist] cbody_*]]]} \
]

# ── the weld guard, and the round it protects ───────────────────────
#
# The report: "tool calls get mixed into the reasoning without a newline
# ending the reasoning block". The break is decided against the widget's
# ACTUAL tail right before the header goes down — not from
# `live_last_kind`, which the flush can leave already saying "tool".
::tspec::case {reasoning between a round's start and its result never welds} {R3} {
    set ::vchat::session "S"
    set ::vchat::stream_session ""
    set ::vchat::show_reasoning 1
    set ::vchat::tool_rounds(S) {}
    dict set ::vchat::turn_anchor "S" [$w index end-1c]
    ::vchat::_mark_running "S" 1
    # The call goes out…
    ::vchat::on_tool "start" "terminal" [dict create args "sleep 30"]
    # …the model thinks again while it runs (and that burst leaves no
    # closing newline), then the result lands. Nothing consults
    # `_live_break` on this path — it answers "" whenever live_last_kind
    # is already "tool" — so the tail test in the end arm is the only
    # thing standing between these two blocks.
    ::vchat::on_reasoning "waiting on the sleep to return"
    ::vchat::_flush_live_reasoning "S"
    ::vchat::on_tool "end" "terminal" [dict create result "done"]
} [::tspec::checks \
    {the mid-round thinking keeps its words} \
    {[string match "*waiting on the sleep to return*" [body]]} \
    {and the result starts its own line, not welded to the thinking} \
    {[string match "*waiting on the sleep to return\n*▸ result (4 chars*" [body]]} \
]
