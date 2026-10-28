# veles-desktop/lib/transcriptspectest.tcl — run lib/transcriptspec.tcl
# against the REAL chat.tcl rendering procs in a real text widget.
#
# The spec file is the source of truth; this only executes it. Two
# verdicts, same convention as mdspectest:
#
#   case  a ground rule (or a step towards one) that HOLDS. Mismatch
#         is a FAILURE.
#   gap   today's behaviour pinned where it misses the rule. Mismatch
#         is a DRIFT — reported separately, because it usually means
#         someone improved the renderer and the entry should be
#         promoted to `case`, not that anything broke.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]

set failed 0
set passed 0
set drift  0
set gaps   0

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

# ── the pieces chat.tcl leans on (same stub set as sessiontest) ────
namespace eval ::vstatus {
    proc set_msg {m} {}
    proc set_route {t {ovr 0}} {}
}
namespace eval ::vsidebar {
    proc refresh {} {}
    proc selected_sid {} { return "" }
}
namespace eval ::vapi {
    variable stopped {}
    variable ev_opens {}
    proc chat_stop {{s ""}} {}
    proc session_events {s c nc h} { return t }
    proc sse_close {t} {}
    proc fatal_code {c} { return 0 }
    proc http_reason {c {at ""}} { return x }
}
namespace eval ::vfind { proc reset_marks {} {} ; proc build {} {} }
namespace eval ::vedit { proc hist_push {t} {} }
namespace eval ::vkeymap { variable current normie ; proc label {a} { return "?" } }
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }
namespace eval ::va11y { proc name {w t} {} }
namespace eval ::vctx { proc attach {w items} {} }
namespace eval ::vask { proc show {d} {} }
namespace eval ::vplace { proc centered {args} {} ; proc near {args} {} }
# The REAL markdown renderer and theme — the transcript's assistant
# bodies go through ::vmd::render, and stubbing it would make the
# separation cases assert against a lie.
source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir markdown.tcl]
namespace eval ::vmedia {
    proc forget {w} {}
    proc insert {w p} { $w insert end "MEDIA:$p\n" tool }
}
namespace eval ::vmd [list proc forget_links {w} {}]

# The REAL module under test.
source [file join $_dir chat.tcl]

# The composer/transcript widgets the handlers poke at. The transcript
# is gated like the app gates every claimed log (_arm_pose via
# _claim_log): a pre-created base widget never passes through
# _make_log, and without the gate the case's direct `$w yview moveto`
# bypasses the pose latch entirely — the latch is part of the behavior
# under test (R6), not harness decoration.
foreach f {.mid .mid.chatwrap .mid.chatwrap.chat .mid.chatwrap.chat.bar} { frame $f }
text .mid.chatwrap.chat.log
::vchat::_arm_pose .mid.chatwrap.chat.log
text .mid.chatwrap.chat.input
button .mid.chatwrap.chat.bar.send
button .mid.chatwrap.chat.bar.stop
button .mid.chatwrap.chat.bar.detach
# The follow cases (R6) need a MAPPED widget with real geometry: yview
# fractions on an unmapped text widget are degenerate — nothing ever
# reports "at the bottom", which is exactly the lie the prestash exists
# not to believe. Map the whole ancestor chain, not just the log: a
# packed child of an unpacked frame is still unmapped.
wm geometry . 420x260
pack .mid -fill both -expand true
pack .mid.chatwrap -fill both -expand true
pack .mid.chatwrap.chat -fill both -expand true
pack .mid.chatwrap.chat.log -fill both -expand true
update idletasks

set w .mid.chatwrap.chat.log

# Fresh transcript state between cases: cleared widget, forgotten raw
# state, default collapse/tool/stream variables. Whatever a previous
# case left behind must not shape the next one.
proc fresh {} {
    ::vchat::clear
    ::vchat::forget_raw
    set ::vchat::session ""
    set ::vchat::busy 0
    set ::vchat::running {}
    set ::vchat::stream_session ""
    set ::vchat::turn_anchor [dict create]
    catch { array unset ::vchat::tool_rounds }
    catch { array unset ::vchat::tool_args }
    set ::vchat::live_rtag ""
    set ::vchat::live_rhdr ""
    set ::vchat::live_rtext ""
    set ::vchat::live_blocks {}
    set ::vchat::live_tool_marks {}
    set ::vchat::show_reasoning 1
    set ::vchat::live_last_kind ""
    set ::vchat::follow 1
    # The at-edge latch too: in the app a user scroll passes the pose
    # gate and updates it; this harness scrolls the widget DIRECTLY, so
    # a latch left set by the previous case would make arrival follow a
    # "user" who scrolled up in the next one (the R6 scrolled-up case).
    catch { array unset ::vchat::at_edge }
    ::vchat::_reset_live_delta
    catch { destroy .mid.chatwrap.chat.follow }
}

# The transcript as the layout rules read it: WITHOUT the time stamps.
# A stamp (HH:MM, the date line) sits inside a block's head, and when it
# happened is not what R1-R7 are about — a live block is stamped by this
# clock, a reloaded row by the server's (rendertest pins the stamps).
# Dropping the text tagged `stamp`/`stampday` restores the exact shape
# every rule below was written against.
proc body {} {
    set w .mid.chatwrap.chat.log
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

proc taglist {} { return [.mid.chatwrap.chat.log tag names] }

# All text covered by $tag, concatenated in document order.
proc tagtext {tag} {
    set w .mid.chatwrap.chat.log
    set out ""
    set r {}
    catch { set r [$w tag ranges $tag] }
    foreach {a b} $r { append out [$w get $a $b] }
    return $out
}

# on_tool / on_done route through for_display/event_session, which
# consult ::vchat::stream_session / session — the case setups set
# those; make the predicates honest rather than overriding them.
if {[llength [info procs ::vchat::for_display]] == 0} {
    proc ::vchat::for_display {} { return [expr {$::vchat::stream_session eq "" \
        || $::vchat::stream_session eq $::vchat::session}] }
    proc ::vchat::event_session {} { return $::vchat::stream_session }
}

proc esc {s} { return [string map {"\n" "\\n"} $s] }

proc run_case {kind name rule setup checks} {
    fresh
    if {[catch {uplevel #0 $setup} err]} {
        puts "  ✗ $name \[$rule\] — setup threw: $err"
        incr ::failed
        return
    }
    set bad 0
    foreach {desc cond} $checks {
        if {[catch {uplevel #0 [list expr $cond]} got] || !$got} {
            if {$bad == 0} {
                if {$kind eq "gap"} {
                    puts "  ~ DRIFT  $name  \[$rule\]"
                } else {
                    puts "  ✗ $name  \[$rule\]"
                }
            }
            if {[info exists got] && ![string is boolean -strict $got]} {
                puts "    $desc — threw: $got"
            } else {
                puts "    $desc"
                puts "      body: '[esc [body]]'"
            }
            set bad 1
        }
    }
    if {!$bad} {
        if {$kind eq "gap"} { incr ::gaps } else { incr ::passed }
    } elseif {$kind eq "gap"} {
        incr ::drift
    } else {
        incr ::failed
    }
}

source [file join $_dir transcriptspec.tcl]

puts "transcript spec — [llength $::tspec::cases] entries from lib/transcriptspec.tcl"
foreach c $::tspec::cases {
    lassign $c kind name rule setup checks
    run_case $kind $name $rule $setup $checks
}

puts ""
# Same tally-line contract as every other suite: flake.nix greps
#   ^<suite>: <n> passed, 0 failed
puts "transcriptspectest: $passed passed, $failed failed"
puts "  ($gaps documented gaps, $drift drifted)"
if {$drift} {
    puts "NOTE: $drift gap(s) no longer match — if that is an improvement,"
    puts "      promote the entries from `gap` to `case` in lib/transcriptspec.tcl."
}
exit [expr {$failed > 0 ? 1 : 0}]
