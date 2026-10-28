#!/usr/bin/env wish
# veles-desktop/lib/statustest.tcl — what the status bar says about the
# prompt cache.
#
# USER (2026-09-02): "we need the visibility of all of the backend stuff
# in all frontends". The per-call cache hit rate existed in the gateway's
# `/api/sessions/<id>/context` JSON and was rendered by none of the four
# surfaces that show a context report. The desktop showed only the
# SESSION mean — the number that made a cache running at 98% read as
# 87.9%, because a lifetime average can never shake off the cold first
# call.
#
# Three promises:
#   * the LAST CALL rate is shown, and labelled as such;
#   * the session mean is still shown beside it, labelled as its own
#     scope — one of them is operational, the other is the bill;
#   * a provider that reports NO cache field shows no percentage at all.
#     Providers differ here (OpenAI-shaped ones send
#     prompt_tokens_details, DeepSeek prompt_cache_hit_tokens, Anthropic
#     cache_read_input_tokens, some send nothing) and "not reported" must
#     never render as "0%", which would send someone hunting a prefix bug
#     that does not exist.
#
# Drives the REAL ::vstatus::poll against stubbed HTTP, on Xvfb.

set _dir [file dirname [file normalize [info script]]]

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir i18n.tcl]
::vi18n::init

# The collaborators poll() reaches for, stubbed to the surface it uses.
namespace eval ::vchat { variable session "sess-1" }
namespace eval ::vapi {
    # What the two endpoints answer this tick. Each test rewrites them.
    variable usage {}
    variable context {}
    proc unattached {} { return 0 }
    # Real callback boundary: a tick is complete only once every answer
    # has had its turn through the event loop, not merely after idle work.
    proc get_json_async {path cmd {ms 0}} {
        if {[catch {get_json $path} d]} {
            after 0 [list uplevel #0 [list {*}$cmd 0 $d]]
        } else {
            after 0 [list uplevel #0 [list {*}$cmd 1 $d]]
        }
        return tok
    }
    proc get_json {path} {
        variable usage
        variable context
        if {[string match "*/api/usage*" $path]}   { return $usage }
        if {[string match "*/context*" $path]}     { return $context }
        # /api/status — enough shape for poll() to get past it.
        return [dict create uptime_s 42 platforms {local} plan_mode false]
    }
}

source [file join $_dir statusbar.tcl]

# The widgets poll() writes into.
frame .statusbar
label .statusbar.dot   -text "•"
label .statusbar.msg   -text ""
label .statusbar.right -text ""
pack .statusbar.dot .statusbar.msg .statusbar.right -side left
pack .statusbar
update idletasks

set passed 0
set failed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name — bar reads: [.statusbar.right cget -text]"
        incr ::failed
    }
}

# One tick with a given pair of endpoint answers; returns the bar text.
proc tick {usage context} {
    set ::vapi::usage $usage
    set ::vapi::context $context
    ::vstatus::poll
    # poll() arms a 15s timer; kill it so the suite does not linger.
    catch { after cancel $::vstatus::poll_after }
    set ::vstatus::poll_after ""
    update
    return [.statusbar.right cget -text]
}

# A session running at 98% on the last call, 73% cumulative — the exact
# shape that used to display as "cache 73%" and read as a broken cache.
#
# CONTRACT NOTE: `busy` is present on EVERY /context answer — the engine
# pins that shape in a Rust test (a_turn_from_another_surface_reads_busy
# _here), which is the authority these fixtures encode. A fixture WITHOUT
# the key would restate the pre-0fb5781 API and keep passing forever
# while the real endpoint broke the status bar ("the context stats are
# gone", 2026-09-07).
puts "\nboth cache scopes are visible, and named"
set u [dict create session [dict create prompt_tokens 4500 \
                                        completion_tokens 60 \
                                        cached_tokens 3300]]
set c [dict create busy false \
                   cache_hit_rate_last_call 0.98 \
                   cache_hit_rate 0.733 \
                   history_percent_of_budget 12.0 \
                   context_window_tokens 200000 \
                   compaction_trigger_percent 85]
set bar [tick $u $c]
ok "the LAST CALL rate is shown"        {[string match "*98% last call*" $bar]}
ok "the session mean is shown beside it" {[string match "*73% session*" $bar]}
ok "…and the two are not confusable"     {[string first "last call" $bar] \
                                          < [string first "% session" $bar]}
ok "token counts survive"                {[string match "*in /*out*" $bar]}
ok "the context fill still renders"      {[string match "*ctx 12.0%*" $bar]}
ok "…with EXACT figures, comma-grouped (a 9% label is not enough)" \
    {[string match "*24,000/200,000 tok*" $bar]}

# The reasoning split (2026-09-13): a route that reports reasoning tokens
# turns "N out" into "A answer · T thinking" — completion is really
# answer + thinking, and naming both is what makes an overthinking model
# legible instead of expensive-looking. None/null keeps the plain "out".
puts "\nthe completion split names answer and thinking"
set ur [dict create session [dict create prompt_tokens 4500 \
                                         completion_tokens 5100 \
                                         cached_tokens 0]]
set cr [dict create busy false \
                    cache_hit_rate_last_call null \
                    history_percent_of_budget 12.0 \
                    context_window_tokens 200000 \
                    session_reasoning_tokens 2800]
set barr [tick $ur $cr]
ok "the answer is named"      {[string match "*2.3k answer*" $barr]}
ok "the thinking is named"    {[string match "*2.8k thinking*" $barr]}
ok "the plain 'out' is gone"  {![string match "*out*" $barr]}

# A provider that reports no cache field at all. cached_tokens 0 and no
# last-call rate is NOT a 0% hit — it is no reading, and the bar must not
# invent one.
puts "\na provider that reports no cache field shows no percentage"
set u0 [dict create session [dict create prompt_tokens 4500 \
                                         completion_tokens 60 \
                                         cached_tokens 0]]
set c0 [dict create busy false \
                    cache_hit_rate_last_call null \
                    history_percent_of_budget 3.0 \
                    context_window_tokens 200000]
set bar0 [tick $u0 $c0]
ok "no cache segment at all"        {![string match "*cache*" $bar0]}
ok "…but the rest of the bar is intact" {[string match "*in /*out*" $bar0]
                                         && [string match "*ctx 3.0%*" $bar0]}

# A COMPACTION in flight: the summarizer holds the agent lock, yet busy
# is false (a compaction is not a turn) and the figures come from the
# store — the bar must render them, not blank, and must not infer
# "done → reload time" from any key here (that decision rides the
# `history-changed` bus event; the poll that used to misread this shape
# is gone).
puts "\ncompaction-in-flight shape still renders (busy:false + figures)"
set cc [dict create busy false \
                    note {compaction in progress} \
                    cache_hit_rate_last_call null \
                    history_percent_of_budget 9.5 \
                    context_window_tokens 200000]
set barc [tick $u0 $cc]
ok "the fill renders while compacting" {[string match "*ctx 9.5%*" $barc]}

# Mid-turn the endpoint answers busy:true (a turn snapshot exists; this
# has meant the agent lock exactly never since the snapshot replaced it).
# Blank the segment and the number flickers away exactly while it is
# most interesting; keep the last reading and say it is mid-turn.
# (The 9.5% compaction tick above is the last GOOD reading, so that is
# the one the busy tick must retain — not the 12% from further back.)
puts "\nmid-turn busy keeps the last reading, marked"
set cb [dict create busy true note {turn in progress}]
set barb [tick $u $cb]
ok "the last fill survives a busy tick" {[string match "*ctx 9.5%*" $barb]}
ok "…and is marked as mid-turn" {[string match "*(turn running)*" $barb]}

# Mid-turn AND the figures ARE present: they are computed from the store,
# not the turn's snapshot lock, so a long multi-round turn's fill tracks
# each provider round instead of sitting on the stale retained line. This
# is the fix for "now I can't see any context" (2026-09-12): fresh beats
# next-to-nothing, and the segment provably moves during the turn.
puts "\nmid-turn busy paints FRESH figures, marked turn running"
set cg [dict create busy true \
        cache_hit_rate_last_call null \
        history_percent_of_budget 14.5 \
        context_window_tokens 200000]
set barg [tick $u $cg]
ok "fresh fill wins over the retained line" {[string match "*ctx 14.5%*" $barg]}
ok "…and the turn mark rides along" {[string match "*(turn running)*" $barg]}

# The status bar must not state a FACT it has not observed. Crossing
# the compaction trigger is a condition, not an event: mid-turn
# compaction is a conditional valve (it may or may not fire this
# round), so the label says what is TRUE — the window is at or past
# the trigger — and never "(compacting)", which claims the summarizer
# is running when all that happened is a percent comparison.
# (PENDING 2026-09-12 round, tail: "compaction due" vs "(compacting)".)
puts "\ncrossing the trigger is a condition, not an announcement"
set cd [dict create busy false \
                    cache_hit_rate_last_call null \
                    history_percent_of_budget 90.0 \
                    context_window_tokens 200000 \
                    compaction_trigger_percent 85]
set bard [tick $u $cd]
ok "the crossing renders as due, not in-flight" \
    {[string match "*(compaction due)*" $bard]}
ok "…and never claims a compaction is running" \
    {![string match "*(compacting)*" $bard]}
set cn [dict create busy false \
                    cache_hit_rate_last_call null \
                    history_percent_of_budget 70.0 \
                    context_window_tokens 200000 \
                    compaction_trigger_percent 85]
set barn [tick $u $cn]
ok "the near band still says near" {[string match "*(near compaction)*" $barn]}
ok "…and the near band is not 'due'" {![string match "*(compaction due)*" $barn]}
# The busy arm paints the same segment; the honesty rule is one rule.
set ce [dict create busy true \
                    cache_hit_rate_last_call null \
                    history_percent_of_budget 90.0 \
                    context_window_tokens 200000 \
                    compaction_trigger_percent 85]
set bare [tick $u $ce]
ok "mid-turn the label stays honest too" \
    {[string match "*(compaction due)*" $bare]
     && ![string match "*(compacting)*" $bare]}

# The context endpoint being unreachable must not take the usage numbers
# down with it: the session mean is still a real reading.
puts "\nthe session mean survives a context fetch that fails"
namespace eval ::vapi {
    proc get_json {path} {
        variable usage
        if {[string match "*/api/usage*" $path]} { return $usage }
        if {[string match "*/context*" $path]}   { error "unreachable" }
        return [dict create uptime_s 42 platforms {local} plan_mode false]
    }
}
set bar1 [tick $u {}]
ok "falls back to the session mean" {[string match "*73% session*" $bar1]}
ok "…and says nothing about a last call" {![string match "*last call*" $bar1]}

# ── the plan field: old is not absent ────────────────────────────────
#
# USER 2026-09-16: "the quota/usage in the status bar seems to disable
# itself on session reopen/switch and then never reactivates by itself."
#
# It never disabled anything. `[tokenplan] cache_ttl_secs` ships at 300
# and `poll_secs` at 900, so a reading is past its TTL for 600 of every
# 900 seconds — and the engine answered `cold: true` for that whole
# stretch, which this proc renders as "plan —". Two thirds of the time,
# on the shipped defaults, and up to ten minutes before the next warm.
#
# The engine now serves the last reading with `stale: true` and its age.
# What this pins is the consumer half: a stale reading paints its
# PERCENTAGE, and "plan —" is left meaning the one thing it should —
# no reading has ever arrived.
puts "\nthe plan field shows an old reading rather than nothing"

namespace eval ::va11y { proc name {w text} { set ::last_tip $text } }
namespace eval ::voverlay {
    proc _qage {secs} {
        if {![string is integer -strict $secs]} { return "just now" }
        if {$secs < 60} { return "${secs}s ago" }
        return "[expr {$secs / 60}]m ago"
    }
}
label .statusbar.quota -text ""
pack .statusbar.quota -side left
namespace eval ::vapi { variable quota {} }
namespace eval ::vapi {
    # `urlencode` is part of the surface _paint_quota now uses: the
    # provider rides the quota request so the ENGINE resolves which plan
    # it draws from (2026-09-20). In production api.tcl provides it; a
    # suite that stubs ::vapi must stub the whole surface the proc under
    # test reaches for, or the test dies on "invalid command name"
    # instead of exercising the behavior.
    proc urlencode {s} { return $s }
    proc get_json_async {path cmd {ms 0}} {
        variable quota
        if {[catch {get_json $path} d]} {
            after 0 [list uplevel #0 [list {*}$cmd 0 $d]]
        } else {
            after 0 [list uplevel #0 [list {*}$cmd 1 $d]]
        }
        return tok
    }
    proc get_json {path} {
        variable usage
        variable context
        variable quota
        if {[string match "*/api/quota*" $path]} { return $quota }
        if {[string match "*/api/usage*" $path]} { return $usage }
        if {[string match "*/context*" $path]}   { return $context }
        return [dict create uptime_s 42 platforms {local} plan_mode false]
    }
}
# The route decides which pool the field reads; personal, as on the box
# the report came from. Since 2026-09-20 the figure NAMES its plan, so
# the fixture carries the engine's resolution (`active_plan`) — the
# surface never re-derives the mapping (see _paint_quota).
::vstatus::set_route "ali-token-plan-personal/qwen3.8-max"

proc quota_tick {q} {
    set ::vapi::quota $q
    set ::last_tip ""
    ::vstatus::_paint_quota
    update
    return [.statusbar.quota cget -text]
}

set fresh [dict create enabled true cold false stale false \
    age_secs 60 ttl_secs 300 poll_secs 900 \
    active_plan personal active_plan_fraction 0.58 \
    personal_used_fraction 0.58 tightest_used_fraction 0.93]
ok "a fresh reading paints its percentage, with its plan named" \
    {[string match "personal 58%" [quota_tick $fresh]]}
# THE TWO FIELDS MUST NOT CARRY THE SAME NUMBER HERE. They did until
# 2026-09-20, and the mutation harness is what said so: with
# active_plan_fraction and tightest_used_fraction both 0.58, reading the
# WRONG one painted the same "personal 58%" and the mutation stayed
# green — a fixture that cannot produce the wire's distinction tests the
# fixture (the cfaa851 lesson, third time). On the real box they differ
# precisely when it matters: zai at 99% while the session's own pool sits
# at 58%, which is the state this fixture now describes.
ok "…and it is THIS session's pool, not the tightest of all of them" \
    {![string match "*93%*" [quota_tick $fresh]]}
ok "…and does not call itself stale" \
    {![string match "*past its ttl*" $::last_tip]}

# The ten-minute stretch. Same figure, older stamp.
set stale [dict create enabled true cold false stale true \
    age_secs 660 ttl_secs 300 poll_secs 900 \
    active_plan personal active_plan_fraction 0.58 \
    personal_used_fraction 0.58 tightest_used_fraction 0.93]
set bars [quota_tick $stale]
ok "a STALE reading still paints its percentage" {[string match "personal 58%" $bars]}
ok "…and is not the em dash that means 'nothing to show'" \
    {![string match "*—*" $bars]}
ok "…with the age said out loud in the tooltip" \
    {[string match "*read 11m ago*" $::last_tip]}
ok "…and the staleness named, not left implicit" \
    {[string match "*past its ttl, a refresh is due*" $::last_tip]}

# Cold keeps its narrow meaning, and it is the ONLY thing that paints
# the dash.
set cold [dict create enabled true cold true ttl_secs 300 poll_secs 900]
ok "a genuinely cold cache is the one case that says 'plan —'" \
    {[string match "plan —" [quota_tick $cold]]}

# The name is not decoration: a figure whose pool is changed changes its
# label too, so a reader can never take "58%" for a different bill.
set teamsame [dict replace $fresh active_plan team]
ok "the same percentage under another plan says that plan's name" \
    {[string match "team 58%" [quota_tick $teamsame]]}

# ── config changes that wait for the owner ───────────────────────────
#
# configsync_auto applies a peer's config on receipt, except a security
# section or a conflict: those wait, and /api/status counts them. The
# status line says so, with the command that shows them.
puts "\nconfig changes waiting are named on the status line"
namespace eval ::vapi {
    variable waiting 3
    proc get_json {path} {
        variable usage
        variable waiting
        if {[string match "*/api/usage*" $path]} { return $usage }
        if {[string match "*/context*" $path]}   { return {} }
        return [dict create uptime_s 42 platforms {local} plan_mode false \
            configsync_waiting $waiting]
    }
}
catch { after cancel $::vstatus::poll_after }
::vstatus::poll
update
ok "a waiting count is shown with /configsync" \
    {[string match "*3 config change(s) wait for you*/configsync*" [.statusbar.msg cget -text]]}
set ::vapi::waiting 0
catch { after cancel $::vstatus::poll_after }
::vstatus::poll
update
ok "…and nothing is said when none wait" \
    {![string match "*configsync*" [.statusbar.msg cget -text]]}
catch { after cancel $::vstatus::poll_after }

puts "\nstatustest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
