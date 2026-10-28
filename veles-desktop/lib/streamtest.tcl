# veles-desktop/lib/streamtest.tcl — the live streaming region must cost
# the same per character however long the answer gets.
#
# USER, on deepseek-v4.1-flash: "I get huge chunks of thinking with huge
# delays ... this looks like some kind of polling that's not fast enough
# to catch up with the actual output". The wire turned out to be the
# cause of the delays (examples/stream_pacing.rs: nine to eleven bursts
# per turn, silences up to 35s, while qwen on the same endpoint streams
# one frame per socket read). But the bursts land on a render path that
# could not absorb them:
#
#   _flush_live_delta deleted the whole live region and re-rendered the
#   ENTIRE accumulated answer through markdown, every 200ms, on the Tk
#   event loop — the same thread that reads the socket. The debounce
#   bounds how OFTEN that happens, not what it COSTS, so a turn paid
#   O(answer x turn-length) and the reader starved while it ran.
#
# The property these tests pin is the one that was missing: **the work
# done per character must not grow with the length of the answer**. A
# fixed structure streamed twice, once four times longer, must cost
# about four times as much — not sixteen.
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

# ── the pieces chat.tcl leans on (same shape as rendertest.tcl) ──────
namespace eval ::vstatus { proc set_msg {m} { set ::vstatus::last $m } }
set ::vstatus::last ""
namespace eval ::vsidebar {
    proc refresh {} {}
    proc selected_sid {} { return "" }
}
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
namespace eval ::vstate {
    proc put {key value} { return 1 }
    proc merge {updates} { return 1 }
    proc all {} { return {} }
    proc get {key {default ""}} { return $default }
    proc repairs {} { return {} }
}

source [file join $_dir safeexec.tcl]
source [file join $_dir bindings.tcl]
source [file join $_dir keymap.tcl]
set ::vkeymap::current emacs
source [file join $_dir vtext.tcl]
source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
source [file join $_dir media.tcl]
source [file join $_dir markdown.tcl]
source [file join $_dir chat.tcl]

::vt::use_theme synthwave

foreach f {.mid .mid.chatwrap .mid.chatwrap.chat .mid.chatwrap.chat.bar} { frame $f }
text .mid.chatwrap.chat.log -wrap word -state disabled \
    -background $::vt::T(bg_input) -foreground $::vt::T(fg) -font {TkDefaultFont 11}
text .mid.chatwrap.chat.input
button .mid.chatwrap.chat.bar.send
button .mid.chatwrap.chat.bar.stop
button .mid.chatwrap.chat.bar.detach
update idletasks

proc body {} { return [.mid.chatwrap.chat.log get 1.0 end-1c] }
proc clear_log {} {
    .mid.chatwrap.chat.log configure -state normal
    .mid.chatwrap.chat.log delete 1.0 end
    .mid.chatwrap.chat.log configure -state disabled
    ::vchat::_reset_live_delta
    set ::vchat::live_last_kind ""
}

# ── the meter ────────────────────────────────────────────────────────
#
# Count the CHARACTERS handed to the markdown renderer, not wall time: a
# clock assertion in a suite that shares a machine with a cargo build is
# a flake generator, and the defect is structural anyway. The wrapper
# calls THROUGH, so every other assertion still sees real rendering.
rename ::vmd::render ::vmd::_render_real
set ::rendered_chars 0
proc ::vmd::render {w text} {
    incr ::rendered_chars [string length $text]
    ::vmd::_render_real $w $text
}

# One synthetic answer: $n paragraphs of the same shape, streamed in
# $per_para chunks each, with a flush between chunks (the 200ms debounce
# firing). Returns {chars_rendered answer_length}.
proc stream_answer {n per_para} {
    clear_log
    set ::rendered_chars 0
    set sid "s-bench"
    set ::vchat::session $sid
    set ::vchat::stream_session $sid
    set para "This paragraph is ordinary prose with some **bold** in it and a\
              little `code` span, long enough to be worth re-rendering."
    set whole ""
    for {set p 0} {$p < $n} {incr p} {
        set text "$para (paragraph $p)\n\n"
        append whole $text
        # Split into per_para roughly equal chunks, the way tokens arrive.
        set len [string length $text]
        set step [expr {($len + $per_para - 1) / $per_para}]
        for {set i 0} {$i < $len} {incr i $step} {
            ::vchat::on_delta [string range $text $i [expr {$i + $step - 1}]]
            # The debounce timer firing. Called directly: `after` needs an
            # event loop this suite deliberately does not spin.
            ::vchat::_flush_live_delta $sid
        }
    }
    ::vchat::_flush_live_delta $sid 1
    set ::vchat::stream_session ""
    return [list $::rendered_chars [string length $whole]]
}

# ── the headline: cost per character must not grow with the answer ──
puts "the live region costs the same per character however long the answer"
lassign [stream_answer 3 6] small_work small_len
lassign [stream_answer 12 6] big_work big_len
set len_ratio [expr {double($big_len) / $small_len}]
set work_ratio [expr {double($big_work) / $small_work}]
puts "    3 paragraphs: [set small_len] chars in, [set small_work] chars rendered\
      ([format %.1f [expr {double($small_work)/$small_len}]]x)"
puts "   12 paragraphs: [set big_len] chars in, [set big_work] chars rendered\
      ([format %.1f [expr {double($big_work)/$big_len}]]x)"
puts "   length x[format %.1f $len_ratio], work x[format %.1f $work_ratio]"
# Four times the answer must cost about four times the work. Quadratic
# growth lands near sixteen; the bound is deliberately loose enough that
# only a genuine change of exponent trips it.
ok "four times the answer costs well under eight times the render work" \
    {$work_ratio < 8.0}
# And the absolute cost stays within a small constant of the answer: each
# character is rendered once when its block settles, plus a bounded
# number of tail re-renders while its own paragraph is still being typed.
ok "the whole turn renders under ten characters of markdown per character of answer" \
    {double($big_work) / $big_len < 10.0}

# ── incremental must equal one-shot ─────────────────────────────────
#
# R3's contract ("live body text renders like reloaded text") is what
# makes settling safe: a block rendered early must land exactly where
# rendering it late would have. Fences are the case that can break it,
# so the fixture carries one.
puts "an incrementally settled render matches a one-shot render"
set DOC "# A heading\n\nSome **bold** prose and a `code` span.\n\n\
```tcl\nset x 1\n\n# a blank line INSIDE a fence is not a boundary\nputs \$x\n```\n\n\
Closing paragraph with a \[link\](https://example.com).\n"

clear_log
set ::vchat::session "s-one"
set ::vchat::stream_session "s-one"
::vchat::on_delta $DOC
::vchat::_flush_live_delta "s-one" 1
set one_shot [body]
set ::vchat::stream_session ""

clear_log
set ::vchat::session "s-inc"
set ::vchat::stream_session "s-inc"
# Stream it a few characters at a time, flushing constantly — the worst
# case for a renderer that settles blocks early.
set len [string length $DOC]
for {set i 0} {$i < $len} {incr i 7} {
    ::vchat::on_delta [string range $DOC $i [expr {$i + 6}]]
    ::vchat::_flush_live_delta "s-inc"
}
::vchat::_flush_live_delta "s-inc" 1
set incremental [body]
set ::vchat::stream_session ""

# A bare "not identical" is not a usable failure. Name the first
# divergence, so the next person sees WHICH block settled wrong.
if {$incremental ne $one_shot} {
    set n [string length $one_shot]
    for {set i 0} {$i < $n} {incr i} {
        if {[string index $one_shot $i] ne [string index $incremental $i]} { break }
    }
    puts "    first divergence at char $i:"
    puts "      one-shot:    [list [string range $one_shot [expr {$i-40}] [expr {$i+40}]]]"
    puts "      incremental: [list [string range $incremental [expr {$i-40}] [expr {$i+40}]]]"
}
ok "the incremental body is identical to the one-shot body" \
    {$incremental eq $one_shot}

# ── a fence is never split ──────────────────────────────────────────
#
# The settle point is "the last blank line at fence depth zero". The
# blank line inside the fenced block above is NOT one; settling there
# would render half a code block as prose and leave the closer dangling.
puts "the settle point respects an open fence"
ok "no settle point inside an unclosed fence" \
    {[::vmd::settle_point "text\n\n```tcl\nset x 1\n\n"] == 6}
ok "a closed fence settles after it" \
    {[::vmd::settle_point "```\nx\n```\n\npost"] > 0}
ok "a plain paragraph break settles" \
    {[::vmd::settle_point "one\n\ntwo"] == 5}
ok "nothing settles before the first break" \
    {[::vmd::settle_point "just one unfinished line"] == 0}



# ── live reasoning pays per tick, not per token ─────────────────────
#
# The burst arrives at the desktop as one SSE frame per token whatever
# the gateway does: on loopback the kernel send buffer absorbs the whole
# response, so there is no application-level backpressure for the
# gateway's merge-on-drain to fold. Measured on the live gateway — a
# 1200-word v4.1-flash turn read at 1KB/s still delivered 732 reasoning
# frames averaging 23 bytes.
#
# So the desktop has to absorb 732 individually-framed lines, and
# on_reasoning used to spend, PER LINE, a yview read, a widget insert,
# an O(n) copy of the whole accumulated reasoning, and a `see end` that
# forces a scroll. That is the cost this pins: the widget work must
# scale with the number of TICKS, not the number of tokens.
puts "live reasoning costs per tick, not per token"
proc stream_reasoning {chunks per_tick} {
    clear_log
    set ::vchat::live_rtag ""
    set ::vchat::live_rhdr ""
    set ::vchat::live_rtext ""
    set ::vchat::live_blocks {}
    set sid "s-reason"
    set ::vchat::session $sid
    set ::vchat::stream_session $sid
    set ::runs 0
    set whole ""
    for {set i 0} {$i < $chunks} {incr i} {
        set t "tok$i "
        append whole $t
        ::vchat::on_reasoning $t
        # The paced timer firing, every $per_tick chunks.
        if {($i + 1) % $per_tick == 0} { ::vchat::_flush_live_reasoning }
    }
    ::vchat::_flush_live_reasoning
    set ::vchat::stream_session ""
    return [list $::runs $whole]
}

# Count widget writes rather than wall time, for the same reason the
# delta meter does: the defect is structural.
rename ::vchat::_run ::vchat::_run_real
proc ::vchat::_run {w text {tags {}}} {
    incr ::runs
    ::vchat::_run_real $w $text $tags
}

lassign [stream_reasoning 300 30] runs whole
puts "   300 chunks, 10 ticks -> $runs widget writes"
# 10 ticks plus the segment header. A per-token path lands at 300+.
ok "300 reasoning chunks over 10 ticks cost under 20 widget writes" \
    {$runs < 20}
ok "every character still arrives, in order" \
    {[string match "*$whole*" [body]]}
# live_rtext is what on_done reconciles the segment from, so it must
# carry the flushed text and not lag a tick behind.
ok "the accumulated reasoning matches what was painted" \
    {$::vchat::live_rtext eq $whole}

# A construct boundary must flush what is pending BEFORE the segment
# closes — otherwise the tail of a reasoning block is dropped on the
# floor when the answer starts.
puts "a pending reasoning tail survives the construct that closes it"
clear_log
set ::vchat::live_rtag ""
set ::vchat::live_rhdr ""
set ::vchat::live_rtext ""
set ::vchat::live_blocks {}
set ::vchat::session "s-edge"
set ::vchat::stream_session "s-edge"
::vchat::on_reasoning "thinking about it"
::vchat::on_reasoning " some more"
# No tick fired: everything is still pending. The answer now starts.
::vchat::on_delta "The answer."
::vchat::_flush_live_delta "s-edge" 1
set ::vchat::stream_session ""
ok "the un-ticked reasoning tail was painted, not dropped" \
    {[string match "*thinking about it some more*" [body]]}
ok "and the answer that closed it is there too" \
    {[string match "*The answer.*" [body]]}
# ── the flush pays for itself ───────────────────────────────────────
#
# A fixed 200ms timer bounds how OFTEN the region repaints and says
# nothing about what a repaint costs — and this runs on the thread that
# reads the socket, so a render slower than its own interval starves the
# reader outright. The period has to be a function of the last flush.
puts "the flush period is paced by what the last flush cost"
ok "a free render keeps the floor" \
    {[::vchat::_next_flush_period 0] == 200}
ok "a cheap render still keeps the floor" \
    {[::vchat::_next_flush_period 50] == 200}
ok "a 100ms render buys 300ms of quiet (a third of the loop)" \
    {[::vchat::_next_flush_period 100] == 300}
ok "a 400ms render buys 1200ms" \
    {[::vchat::_next_flush_period 400] == 1200}
ok "one pathological render cannot make the view catatonic" \
    {[::vchat::_next_flush_period 60000] == 2000}
# A stretched period must not outlive the region that earned it: the
# previous turn's worst paint is not evidence about this one.
set ::vchat::live_delta_period 1700
::vchat::_reset_live_delta
ok "a fresh region starts back at the floor" \
    {$::vchat::live_delta_period == 200}
# ── prose accumulation costs per BOUNDARY, not per token ────────────
#
# `on_delta` runs once per SSE frame — measured at 732 frames averaging
# 23 bytes for a single turn — so anything it does that is O(answer) is
# the same quadratic the meters above exist to kill. The first version
# of the row accumulator (1ab7445) rebuilt the open call's whole prose
# string on EVERY chunk, and nothing here would have caught it: these
# meters count WIDGET WRITES, and folding a string costs none.
#
# So the property is counted where it lives. The prose is appended per
# token and folded into its row at the boundaries that can close or read
# a call — a tool round, and the reconcile — which is a handful of times
# per turn however long the answer gets.
puts "\nprose accumulation costs per boundary, not per token"
set ::settles 0
rename ::vchat::_live_block_settle ::vchat::_live_block_settle_real
proc ::vchat::_live_block_settle {} {
    incr ::settles
    ::vchat::_live_block_settle_real
}
clear_log
set ::vchat::live_blocks {}
set ::vchat::live_ptext ""
set ::vchat::live_last_kind ""
set ::vchat::live_rtag ""
::vchat::_reset_live_delta
set ::vchat::session "s-prose"
set ::vchat::stream_session "s-prose"
set whole ""
for {set i 0} {$i < 300} {incr i} {
    set t "w$i "
    append whole $t
    ::vchat::on_delta $t
    # A tool round every hundred chunks: three real call boundaries.
    if {($i + 1) % 100 == 0} {
        ::vchat::on_tool "start" "terminal" [dict create args "x"]
        ::vchat::on_tool "end"   "terminal" [dict create result "y"]
    }
}
::vchat::_live_block_settle
set ::vchat::stream_session ""
puts "   300 chunks, 3 boundaries -> $::settles folds"
ok "300 chunks over 3 boundaries fold a handful of times, not 300" \
    {$::settles <= 8}
# The cheap path must still be the COMPLETE one: every character the
# model streamed has to be in the rows the reconcile will read.
set got ""
foreach b $::vchat::live_blocks {
    if {[lindex $b 0] eq "call"} { append got [lindex $b 2] }
}
ok "…and every character reached the rows, in order" {$got eq $whole}

# ── turn end does not wait for the status HTTP pair ─────────────────
#
# USER 2026-10-25: "The end of a long term sometimes hangs extremely
# long, possibly compounded by the usage window bug." The final text
# reconcile must finish without waiting for /api/usage or /context. An
# earlier _paint_right nested a vwait for one GET inside the other's,
# letting turn-final callbacks run amid half-completed status work.
# Exercise on_done itself with a populated transcript, not a bare status
# helper. Only the network boundary is replaced; all transcript work is
# the production renderer against real widgets.
puts "\nthe real on_done returns before its delayed status replies"
namespace eval ::turnwire {
    variable calls {}
    proc get_json_async {path cmd {ms 0}} {
        variable calls
        variable pending
        lappend calls $path
        # Held replies, not wall-clock waits: none can fire until this
        # test explicitly drives them, so the old nested wait would hang
        # here instead of becoming a timing-sensitive performance check.
        dict set ::turnwire::pending $path [list $cmd 0 {deliberate held reply}]
        return $path
    }
}
namespace eval ::vsidebar {
    proc refresh_soon {} {}
    proc mark_current {} {}
}
namespace eval ::vfind { proc reset_marks {} {} }
namespace eval ::vapi {
    proc unattached {} { return 0 }
    proc urlencode {s} { return $s }
    proc get_json_async {path cmd {ms 0}} {
        ::turnwire::get_json_async $path $cmd $ms
    }
}
# The real status scheduler and painter, not a stubbed refresh_context.
source [file join $_dir statusbar.tcl]
frame .statusbar
label .statusbar.msg
label .statusbar.right
pack .statusbar.msg .statusbar.right
clear_log
set ::vchat::session s-turn-end
set ::vchat::stream_session s-turn-end
set ::vchat::logs [dict create s-turn-end .mid.chatwrap.chat.log]
set ::vchat::live_ctx ""
set ::vchat::live_saved {}
set ::vchat::live_extra {}
set ::vchat::live_tool_marks {}
set ::vchat::turn_anchor [dict create s-turn-end 1.0]
set ::vchat::live_blocks {}
set ::vchat::live_ptext ""
set ::vchat::running [dict create s-turn-end 1]
::vchat::on_delta "a streamed answer that must remain visible"
::vchat::_flush_live_delta s-turn-end 1
set ::vchat::turn_prompt 120
set ::vchat::turn_completion 12
set ::turn_done_returned 0
::vchat::on_done [dict create provider test model slow-status \
    prompt_tokens 120 completion_tokens 12 \
    text "the completed answer is visible"]
set ::turn_done_returned 1
ok "on_done returns before the scheduled status fetch even starts" \
    {$::turn_done_returned == 1 && [llength $::turnwire::calls] == 0}
ok "the reconciled answer is in the real transcript immediately" \
    {[string match "*the completed answer is visible*" [body]]}
ok "the completed turn is no longer marked busy" \
    {![dict exists $::vchat::running s-turn-end]}
# Fire the callback that refresh_context scheduled. Both requests are
# sent, but their callbacks remain in the future; the UI work has ended.
::vstatus::_paint_right
ok "the scheduled status work is asynchronous after on_done too" \
    {[llength $::turnwire::calls] == 2 && $::vstatus::painting}
foreach _cmd [after info] { after cancel $_cmd }

puts ""
puts "streamtest: $passed passed, $failed failed"
exit [expr {$failed ? 1 : 0}]
