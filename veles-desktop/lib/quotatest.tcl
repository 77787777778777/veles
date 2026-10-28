#!/usr/bin/env wish
# veles-desktop/lib/quotatest.tcl — what the desktop says about the
# UPSTREAM plan's quota.
#
# The feature exists because the only way to see these numbers was a
# browser console. The risk it carries is the opposite of not showing
# them: showing one that is not true. Three fields the Alibaba APIs
# genuinely leave out —
#
#   * the personal 5-hour window (absent from the live payload; the
#     vendor's own CLI prints "may be unlimited"),
#   * a released seat's credit pool (`{"EquityType":"CREDITS"}` and
#     nothing else),
#   * the whole reading, before the gateway's first background warm,
#
# — must each render as a NAMED absence. A 0% on a quota display is a
# lie with a decimal point on it, and it is the kind that sends somebody
# to the console to find out why their plan is empty.
#
# 2026-09-20: THE PLANS ARE LABELLED. USER: "the existing usage is
# confusing, it conflates the personal and team alibaba tokens. Please
# clearly label the plans separately in the usage widget and the status
# line." Every plan section now opens with its own named header, and
# the status field reads "team 59%" rather than a bare "plan 59%" —
# because four providers on this box draw on four disjoint pools and a
# figure that does not name its pool answers no question.
#
# The mapping is the ENGINE's (`/api/quota?provider=…` → `active_plan`),
# never re-derived here: a second implementation of "which pool is this"
# is exactly how the desktop and the engine came to disagree.
#
# Drives the REAL ::voverlay::_quota_fill and ::vstatus::_paint_quota
# against a stubbed /api/quota, on Xvfb.

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

namespace eval ::vchat { variable session "sess-1" }

# The one HTTP door, stubbed. `asked` records the PATH so the
# cache-only contract is assertable: opening the overlay must not carry
# ?refresh=1, and the Refresh button must.
namespace eval ::vapi {
    variable quota {}
    # Callback fixture defaults to a valid, enabled quota payload; tests
    # for cold/disabled/error states replace it through the sync helper.
    variable async_quota [dict create enabled true cold false age_secs 1 poll_secs 900 \
        team_error null personal_error null zai_error null kimi_error null]
    variable asked {}
    variable fail 0
    proc unattached {} { return 0 }
    proc urlencode {s} { return $s }
    # Mirror the production window's asynchronous boundary, while the
    # _quota_fill tests below keep their standalone quota-view seam.
    proc get_json_async {path cmd {ms 0}} {
        variable quota
        variable async_quota
        variable fail
        if {$fail} {
            after 0 [list uplevel #0 [list {*}$cmd 0 "HTTP 502: gateway down"]]
            return tok
        }
        if {[string match "*/api/quota*" $path] && [lindex $cmd 0] eq "::vstatus::_quota_answer"} {
            set d $quota
        } elseif {[string match "*/api/quota*" $path]} {
            set d $async_quota
        } else {
            set d $::vmanage::usage_payload
        }
        after 0 [list uplevel #0 [list {*}$cmd 1 $d]]
        return tok
    }
    proc get_json {path} {
        variable quota
        variable asked
        variable fail
        lappend asked $path
        if {$fail} { error "HTTP 502: gateway down" }
        if {[string match "*/api/usage*" $path]} {
            if {[info exists ::vmanage::usage_payload]} {
                return $::vmanage::usage_payload
            }
        }
        if {[string match "*/api/quota*" $path]} {
            return $quota
        }
        if {[string match "*/api/usage*" $path]} {
            return [dict create prompt_tokens 1 completion_tokens 2 calls 3]
        }
        return [dict create uptime_s 42 platforms {local} plan_mode false]
    }
}

# ::vmanage supplies the window/status surfaces the quota half touches.
namespace eval ::vmanage {
    variable said {}
    variable saidkind {}
    variable gets {}
    # What /api/usage answers for the Usage-widget half of this suite.
    # Shaped exactly as the engine serves it, `by_plan` rows included
    # (2026-09-20) — a fixture that omitted the key would test the
    # fixture rather than the widget.
    variable usage_payload [dict create]
    # `said` is the LAST line, which is what most assertions here want.
    # `saidall` is every line in order — needed because one fill says
    # more than once (a declined re-entrant click, then the age note on
    # the way out), and a last-one-wins variable cannot see the first.
    variable saidall {}
    proc say {w msg {kind info}} {
        variable said
        variable saidkind
        variable saidall
        set said $msg
        set saidkind $kind
        lappend saidall $msg
    }
    proc _reason {err} { return $err }
    # `window` and `button`: stubbed to the chrome a manager gets, so a
    # test can drive the REAL ::voverlay::_usage entry point (the proc
    # ::voverlay::open calls) rather than its parts. The scroll round
    # needs that reach: a mutation that drops the scroll wrapper from
    # `_usage` must redden a test, and a test that builds the wrapper
    # itself can never see it.
    proc window {w title geometry {minsize {520 340}}} {
        toplevel $w
        wm geometry $w $geometry
        ttk::frame $w.bar
        pack $w.bar -fill x
        ttk::label $w.bar.status -text "" -foreground $::vt::T(fg_dim)
        pack $w.bar.status -side right -fill x -expand 1
        return $w.bar
    }
    proc button {bar name label cmd {help ""}} {
        ttk::button $bar.$name -text $label -command $cmd
        pack $bar.$name -side left
        return $bar.$name
    }
    proc get {w path} {
        variable gets
        variable usage_payload
        lappend gets $path
        if {[string match "*/api/usage*" $path]} { return $usage_payload }
        return {}
    }
    # Async callbacks are stubbed in ::vapi, above: production fetches
    # through ::vapi::get_json_async, not through the manager wrapper.
}

source [file join $_dir statusbar.tcl]
# overlays.tcl reads nullable fields through ::vapi::field (jsonfield.tcl)
# — production sources it from main.tcl; a suite that drives the overlay
# procs directly has to source it too.
source [file join $_dir jsonfield.tcl]
source [file join $_dir overlays.tcl]

set passed 0
set failed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

# ── fixtures, shaped exactly as /api/quota serves them ───────────────

# The live answer of 2026-09-15 was one assigned seat with credits, a
# personal WEEKLY window and a 5-hour window the API did not report.
# 2026-09-23: the personal plan reports a MONTH window now and nothing
# else (see `tokenplan::WINDOW_FAMILIES`), so the fixture carries what
# the wire carries today — a month at the reading taken live (0.5%,
# resetting 2026-10-03 16:00 UTC) — and the kimi section still supplies
# the "not reported" absence the display rules need (`month · code`).
#
# `plan_fractions` and `active_plan*` are the ENGINE's own resolution,
# served on every answer (2026-09-20); the overlay reads them rather
# than re-deriving the mapping.
proc live_payload {{prov ""}} {
    set d [dict create \
        enabled true cold false age_secs 3 ttl_secs 300 poll_secs 900 \
        tightest_used_fraction 0.55339872180948 \
        team_used_fraction 0.55339872180948 \
        personal_used_fraction 0.004967843176105556 \
        zai_used_fraction 0.60 \
        kimi_used_fraction 1.0 \
        plan_fractions [dict create team 0.55339872180948 \
            personal 0.004967843176105556 zai 0.60 kimi 1.0] \
        team [dict create \
            released_seats 1 members 1 seated 1 unseated 0 \
            seats [list [dict create \
                seat_id seat_e5cfa7b0 subscription subs-42217ce2 \
                spec_type max status NORMAL assigned ASSIGNED \
                credits_total 250000 credits_remaining 111650.31954763 \
                credits_unit Credit used_fraction 0.55339872180948 \
                cycle_start 2026-09-03T09:00:00+00:00 \
                cycle_end 2026-10-03T09:00:00+00:00]] \
            spend [dict create bill_month 2026-09 currency USD \
                total 253.93 pretax 206.45 tax 47.48]] \
        team_error null \
        personal [dict create free_tier {} windows [list \
            [dict create label month \
                used_fraction 0.004967843176105556 \
                resets_at 2026-10-03T16:00:00+00:00]]] \
        personal_error null \
        zai [dict create level pro \
            window_5h [dict create label 5-hour used_fraction 0.01 \
                resets_at 2026-09-15T15:48:10+00:00] \
            weekly [dict create label weekly used_fraction 0.60 \
                resets_at 2026-09-20T20:28:33+00:00] \
            tools [dict create limit 1000 used 69 remaining 931 \
                used_fraction 0.069 \
                resets_at 2026-10-01T12:58:51+00:00 \
                details [list \
                    [dict create model search-prime usage 54] \
                    [dict create model web-reader usage 15] \
                    [dict create model zread usage 0]]]] \
        zai_error null \
        kimi [dict create windows [list \
            [dict create label 5-hour used_fraction 1.0 \
                resets_at 2026-09-20T15:22:56+00:00] \
            [dict create label month used_fraction 0.0546 \
                resets_at 2026-10-20T00:00:00+00:00] \
            [dict create label {month · code} used_fraction null \
                resets_at 2026-10-20T00:00:00+00:00]]] \
        kimi_error null]
    if {$prov ne ""} {
        set table [dict create \
            ali-token-plan-team team \
            ali-token-plan-personal personal \
            ali-responses team \
            {custom:zai-coding-plan} zai \
            {custom:kimi-coding-plan} kimi \
            openrouter null]
        if {[dict exists $table $prov]} {
            set plan [dict get $table $prov]
            dict set d active_plan $plan
            if {$plan ne "" && $plan ne "null"} {
                # The engine serves the fraction BESIDE the name; the
                # surface reads both off the wire.
                dict set d active_plan_fraction \
                    [dict get $d plan_fractions $plan]
            }
        }
    }
    return $d
}

# A fresh overlay shell, built the way ::vmanage::window would leave it.
proc fresh_overlay {} {
    catch { destroy .ov }
    toplevel .ov
    ttk::frame .ov.body
    pack .ov.body
    return .ov
}

# What /api/usage answers, shaped as the engine serves it — including
# `by_plan` and `session_by_plan`, the split added 2026-09-20. The rows
# carry the plan name, the providers that fed it, and the totals.
proc usage_payload {} {
    return [dict create \
        prompt_tokens 1870018956 completion_tokens 14174355 \
        cached_tokens 1825253522 cache_write_tokens 0 calls 13540 \
        scope {all sessions} \
        by_plan [list \
            [dict create plan team providers {ali-responses ali-token-plan-team} \
                 calls 3506 prompt_tokens 472971728 completion_tokens 4943286 \
                 cached_tokens 0 total_tokens 477915014] \
            [dict create plan personal providers {ali-token-plan-personal} \
                 calls 6964 prompt_tokens 1007183171 completion_tokens 7703844 \
                 cached_tokens 0 total_tokens 1014887015] \
            [dict create plan zai providers {zai-coding-plan} \
                 calls 2753 prompt_tokens 361717710 completion_tokens 1430622 \
                 cached_tokens 0 total_tokens 363148332] \
            [dict create plan null providers {openrouter} \
                 calls 20 prompt_tokens 1485632 completion_tokens 4806 \
                 cached_tokens 0 total_tokens 1490438]] \
        session [dict create prompt_tokens 4500 completion_tokens 60 \
                 cached_tokens 3300 calls 12] \
        session_by_plan [list \
            [dict create plan team providers {ali-token-plan-team} \
                 calls 12 prompt_tokens 4500 completion_tokens 60 \
                 cached_tokens 3300 total_tokens 4560]]]
}

proc overlay_text {w} {
    set out {}
    foreach c [winfo children $w.body] {
        catch { lappend out [$c cget -text] }
    }
    return [join $out "\n"]
}

proc fill {payload {force 0}} {
    set ::vapi::quota $payload
    set ::vapi::asked {}
    set w [fresh_overlay]
    ::voverlay::_quota_fill $w 0 $force
    update idletasks
    return [overlay_text $w]
}

puts "quotatest"
proc bgerror {err} {
    puts stderr "TEST BGERROR: $err\n$::errorInfo"
    exit 1
}

# ── the Usage widget's plan split (2026-09-20) ───────────────────────
#
# USER: "the existing usage is confusing, it conflates the personal and
# team alibaba tokens." The widget's answer was ONE flat total across
# every provider; it now leads with the ledger split by plan, each row
# naming its providers, so "1.9G tokens" becomes four figures that can
# each be acted on.
puts "\nthe Usage widget splits the local ledger by plan"
# The quota half uses its own standalone payload seam. The Usage window's
# local ledger is exercised through the real asynchronous pair above.
set ::vmanage::usage_payload [usage_payload]
set w [fresh_overlay]
::voverlay::_usage_fill $w 0
update
set ut [overlay_text $w]
ok "…and names each plan it spent on" \
    {[string match "*team*" $ut] && [string match "*personal*" $ut] \
     && [string match "*zai*" $ut]}
ok "…with the split heading, so the rows have an owner on screen" \
    {[string match "*by plan*" $ut]}
ok "a plan's token total is abbreviated, not printed to ten digits" \
    {[string match "*1.0G tok*" $ut] || [string match "*1.0G*" $ut]}
ok "…and the four pools are separate figures, not one sum" \
    {[string match "*477.9M*" $ut] || [string match "*478.0M*" $ut]}
# A provider no plan claims keeps its own name rather than being folded
# into somebody's pool — and it is named, not "other".
ok "an unmapped provider is named in the split, not bucketed" \
    {[string match "*openrouter*" $ut]}
# The entries under a plan are listed: `ali-responses` riding the team
# pool is exactly the kind of claim a reader should be able to check.
ok "the providers under a plan are listed, so the grouping is checkable" \
    {[string match "*ali-responses, ali-token-plan-team*" $ut]}
# The flat totals the status bar abbreviates are still there.
ok "the flat totals survive below the split" \
    {[string match "*prompt tokens*" $ut] || [string match "*API calls*" $ut]}

# The fixtures below drive the REAL _usage_fill too (the re-entrancy
# tests), so leave a valid payload in place.
set ::vmanage::usage_payload [usage_payload]

# ── the overlay ───────────────────────────────────────────────────────

set t [fill [live_payload]]

ok "the credit pool is shown against the pool it is a fraction of" \
    {[string match "*111,650 / 250,000*" $t]}
ok "…with the percentage USED, not remaining" \
    {[string match "*55.3% used*" $t]}
ok "the cycle end is a readable UTC stamp" \
    {[string match "*2026-10-03 09:00 UTC*" $t]}
ok "the month's spend is shown" \
    {[string match "*2026-09 USD 253.93*" $t]}
ok "the personal month window is shown with its reset" \
    {[string match "*0.5%*" $t] && [string match "*2026-10-03 16:00 UTC*" $t]}

# THE CENTRAL PROMISE. (The 0.0 glob must not match "1.0%" or "60.0%"
# — anchor it to the start of the percent figure.)
ok "a window the API did not report reads 'not reported', never 0%" \
    {[string match "*not reported*" $t] && ![regexp {[^0-9]0\.0%|^0\.0%} $t]}

# ─ the plans are LABELLED (USER 2026-09-20) ──────────────────────────
#
# "the existing usage is confusing, it conflates the personal and team
# alibaba tokens. Please clearly label the plans separately."
#
# Every pool opens with its own header. Two plans both have a "weekly"
# and two both have a "5-hour", so a column of bare window names cannot
# be told apart — which is exactly the conflation complained of.
puts "\nthe plans are named, one header each"

ok "the team section has its own named header" \
    {[string match "*team*" $t]}
ok "…and the personal one" \
    {[string match "*personal*" $t]}
ok "…and z.ai" \
    {[string match "*z.ai*" $t]}
ok "…and kimi" \
    {[string match "*kimi*" $t]}
ok "each header carries that plan's own fraction, from the engine" \
    {[regexp {team\s+·\s+55\.3% used} $t]}
ok "…and the plans' percentages are not interchangeable" \
    {[regexp {team\s+·\s+55\.3% used} $t] \
     && [regexp {personal\s+·\s+0\.5% used} $t] \
     && [regexp {z\.ai\s+·\s+60\.0% used} $t] \
     && [regexp {kimi\s+·\s+100\.0% used} $t]}
# The engine's fraction for a plan with no reading is an absence, said
# as one — never a 0% (which is a lie with a decimal point on it).
set nodata [dict replace [live_payload] \
    plan_fractions [dict create team 0.5534 personal 0.005 zai 0.6 kimi null]]
set tn [fill $nodata]
ok "a plan with no fraction says 'not reported', not 0%" \
    {[regexp {kimi\s+·\s+not reported} $tn]}

# The z.ai section kept its tier and tool quota, now under a header.
ok "the z.ai tier is shown under its own header" \
    {[string match "*pro*" $t]}
ok "the z.ai 5-hour window shows its percentage" \
    {[string match "*1.0%*" $t]}
ok "the z.ai weekly window shows its percentage and reset" \
    {[string match "*60.0%*" $t] && [string match "*2026-09-20 20:28 UTC*" $t]}
ok "the z.ai tool quota is grounded against its pool" \
    {[string match "*69 / 1,000*" $t]}
ok "the per-tool breakdown is listed" \
    {[string match "*search-prime 54*" $t] && [string match "*web-reader 15*" $t]}

# ── kimi, the fourth pool (2026-09-20) ───────────────────────────────
puts "\nthe kimi plan renders like its siblings"
ok "the kimi 5-hour window carries the absolute figure" \
    {[string match "*5-hour*100.0%*" $t]}
ok "…and the monthly ratio, with its own label" \
    {[string match "*5.5%*" $t]}
ok "…and the month·code window is named rather than merged" \
    {[string match "*month · code*" $t]}
ok "…and its reset date" \
    {[string match "*2026-10-20 00:00 UTC*" $t]}

# ── this session's plan, said at the top ──────────────────────────────
#
# The sections are labelled, but the ANSWER to "what am I spending" is
# one line; burying it inside four blocks is how it got missed.
puts "\nthe overlay says which plan this session is on"
set t_resp [fill [live_payload ali-token-plan-team]]
ok "a team session says so" \
    {[string match "*team*plan*" $t_resp]}
ok "…and the header marks the session's own pool, once" \
    {[regexp -all {← active} $t_resp] == 1}
set t_resp [fill [live_payload {custom:kimi-coding-plan}]]
ok "a kimi session marks kimi, not the default route" \
    {[string match "*kimi*← active*" $t_resp]}

# A provider on NO plan: the row says so rather than showing somebody
# else's pool as if it were this session's.
::vstatus::set_route "openrouter/some-model"
set t_resp [fill [live_payload openrouter]]
ok "a session on no plan says so, and names the provider" \
    {[string match "*on no plan*openrouter*" $t_resp]}
ok "…and nothing is marked active" \
    {![string match "*← active*" $t_resp]}
::vstatus::set_route ""

# The age wording matches the Rust renderer's `ago()` exactly, so the
# desktop and the text surfaces cannot disagree about what "fresh" means:
# under five seconds is "just now", and only past that does it count.
ok "a few-seconds-old reading reads as just now" \
    {[string match "*just now*" $::vmanage::said]}
ok "…and how often they refresh themselves" \
    {[string match "*15m*" $::vmanage::said]}

fill [dict replace [live_payload] age_secs 240]
ok "a four-minute-old reading says four minutes, not just now" \
    {[string match "*4m ago*" $::vmanage::said]
     && ![string match "*just now*" $::vmanage::said]}

fill [dict replace [live_payload] poll_secs 0]
ok "with the background warm off, the note says so" \
    {[string match "*no background refresh*" $::vmanage::said]}

# ── the cache-only contract ───────────────────────────────────────────

ok "opening the overlay does NOT force a remote read" \
    {![string match "*refresh=1*" $::vapi::asked]}

set t [fill [live_payload] 1]
ok "the Refresh button DOES force one" \
    {[string match "*refresh=1*" $::vapi::asked]}

# The provider rides the request so the ENGINE resolves the plan. A
# surface that re-derived the mapping itself is how the desktop and the
# engine came to disagree about `ali-responses` (a team credential with
# neither "team" nor "personal" in its name).
::vstatus::set_route "ali-responses/qwen3.8-max"
set ::vapi::asked {}
fill [live_payload ali-responses]
ok "the session's provider is sent up with the quota request" \
    {[string match "*provider=ali-responses*" $::vapi::asked]}
::vstatus::set_route ""

# ── the window scrolls (USER 2026-09-23) ──────────────────────────────
#
# "I need to be able to scroll the window because there's so much stuff
# in there now it barely fits." The fill grids the ledger split, four
# pools and the reading's age into one frame; past the window's height
# Tk clipped the bottom with no way to reach it, the Mechanisms overlay's
# exact history one window over. These pins drive the REAL
# _usage_scroll — the wrapper the overlay builds — because a fixture
# that hand-built its own canvas would test the fixture.
puts "\nthe Usage window scrolls"

# A real overlay shell, built by the REAL ::voverlay::_usage — the proc
# ::voverlay::open calls. Driving the entry point rather than
# hand-building its parts lets a mutation that drops the scroll wrapper
# redden something.
catch { destroy .ov_scroll }
::voverlay::_usage .ov_scroll
wm geometry .ov_scroll 560x300
set ::vmanage::usage_payload [usage_payload]
set ::vapi::async_quota [live_payload]
set ::vapi::quota [live_payload]
::voverlay::_usage_fill .ov_scroll 0
update idletasks
update

set c .ov_scroll.scroll.c
# Every probe below goes through `ytop`/`ybot`, which answer "" when the
# canvas does not exist. A mutation that drops the scroll wrapper makes
# `.ov_scroll.scroll.c` absent, and an unguarded `$c yview` then THROWS
# straight out of the suite — the harness reported "suite did not
# report" instead of RED, and a crash is not a failing assertion.
proc ytop {} {
    if {![winfo exists .ov_scroll.scroll.c]} { return "" }
    return [lindex [.ov_scroll.scroll.c yview] 0]
}
proc ybot {} {
    if {![winfo exists .ov_scroll.scroll.c]} { return "" }
    return [lindex [.ov_scroll.scroll.c yview] 1]
}
proc safe {script} { catch { uplevel #0 $script } }

set region ""
catch { set region [$c cget -scrollregion] }
ok "the overlay builds the content scrollable, not as a clipped frame" \
    {[winfo exists $c] && $region ne ""}
ok "…with a scrollbar beside it" \
    {[winfo exists .ov_scroll.scroll.sb]}
# The keys below follow the FOCUS, and a canvas is not in the default
# Tab traversal — so this is what makes them reachable at all. Without
# it `focus -force $c` below lands nowhere and every key assertion here
# goes back to being a green-from-nothing (the first cut's failure).
set next ""
catch { set next [tk_focusNext .ov_scroll.bar.refresh] }
ok "…and it is keyboard-reachable (in the Tab chain)" \
    {$next eq $c}
# The whole point: the content is TALLER than the viewport, which is
# what used to make the bottom unreachable.
set content_h [lindex $region 3]
set view_h 0
catch { set view_h [winfo height $c] }
ok "the painted content is taller than the window (the reported state)" \
    {$content_h > $view_h && $view_h > 1}

# The wheel scrolls it. X11 sends Button-4/5 with no %D; the binding
# lives on the toplevel so it covers every row the fill re-creates.
set top0 [ytop]
safe { event generate .ov_scroll <Button-5> }
update
set top1 [ytop]
ok "the wheel scrolls the window" {$top1 ne "" && $top1 > $top0}
safe { event generate .ov_scroll <Button-4> }
update
ok "…and scrolls back" {[ytop] eq $top0}

# Keyboard, the same surface: a manager window that only answers the
# mouse is what this app's keymap doctrine exists to avoid. Key events
# (unlike the wheel) follow the FOCUS, so the test focuses the window's
# scrollable child first — exactly what a user clicking into the window
# does — and the assertions then have to be real ones.
catch { focus -force $c }
update
set top0 [ytop]
safe { event generate $c <Down> }
update
ok "the Down key scrolls it" {[ytop] ne "" && [ytop] > $top0}
set top1 [ytop]
safe { event generate $c <Up> }
update
ok "…and the Up key scrolls back" {[ytop] ne "" && [ytop] < $top1}
set top1 [ytop]
safe { event generate $c <Next> }
update
ok "PageDown jumps a page" {[ytop] ne "" && [ytop] - $top1 > 0.1}
safe { event generate $c <End> }
update
ok "End reaches the bottom — the row that used to be clipped" \
    {[ybot] ne "" && [ybot] >= 0.999}
safe { event generate $c <Prior> }
update
ok "PageUp comes back off the bottom" {[ybot] ne "" && [ybot] < 1.0}
safe { event generate $c <Home> }
update
ok "Home returns to the top" {[ytop] ne "" && [ytop] <= 0.001}

# A Refresh rebuilds every row. Tk recomputes yview against the new
# scrollregion, so the reader keeps their place — this pins the
# OBSERVED behaviour (an earlier remember/restore pair meant to
# guarantee it was dead code; see _uscr_settle's header), so a future
# fill that resets the view would redden here.
safe { $c yview moveto 0.55 }
update
set before [ytop]
set ::vmanage::usage_payload [usage_payload]
set ::vapi::async_quota [live_payload]
set ::vapi::quota [live_payload]
::voverlay::_usage_fill .ov_scroll 1
update idletasks
update
set after [ytop]
ok "a Refresh keeps the reader's place, not back to the top" \
    {$after ne "" && $after > 0.3 && abs($after - $before) < 0.15}
ok "…and the content is still fully painted after it" \
    {[llength [winfo children .ov_scroll.body]] > 5}

# The bare-body case keeps working: every other test in this file (and
# any embedder) builds `$w.body` by hand with no canvas, and the fill's
# scroll helpers must no-op rather than throw.
set w [fresh_overlay]
::voverlay::_usage_fill $w 1
update
ok "a fill against a bare body (no scroll wrapper) still works" \
    {[winfo exists $w.body.h1] && $::voverlay::usage_fill == 0}
ok "…and the scroll helpers no-op there rather than throwing" \
    {[::voverlay::_uscr_settle $w] eq "" \
     && [::voverlay::_uscr_key $w top] eq ""}

# ── re-entrancy (2026-09-15) ──────────────────────────────────
#
# Asynchronous fills cannot nest an HTTP wait inside the click. A second
# click before the answers still has to be harmless: the generation guard
# rejects it, and the outstanding pair paints exactly one complete fill.
set ::vapi::quota [live_payload]
set ::vmanage::said ""
set ::vmanage::saidall {}
set ::vapi::asked {}
set w [fresh_overlay]
set rc [catch { ::voverlay::_usage_fill $w 1 } err]
::voverlay::_usage_fill $w 1
update
ok "a fill re-entered before its answers does not crash" \
    {$rc == 0}
ok "…and the outer fill still painted both halves" \
    {[winfo exists $w.body.h1] && [winfo exists $w.body.hq]}
ok "…and the declined click is SAID, not swallowed" \
    {[string match "*already refreshing*" $::vmanage::saidall]}
ok "…and the guard is released on the way out" \
    {$::voverlay::usage_fill == 0}

# Closing the real window invalidates its generation and releases the
# guard. A late answer cannot paint into a reopened window at that path.
destroy .ov_closing
::voverlay::_usage .ov_closing
destroy .ov_closing
update
ok "a destroyed window releases its fill and rejects its old answer" \
    {$::voverlay::usage_fill == 0 && ![winfo exists .ov_closing]}

# A REQUEST has a session scope too. Generation scoping rejects a closed
# window, but does not by itself reject a reply for a session the user
# has since left while the same window remains open. Hold A's pair,
# switch to B. A second click coalesces a B refresh after A settles;
# neither a duplicate fill nor a discarded A answer may strand B.
puts "\nthe Usage window belongs to the session that requested it"
update
rename ::vapi::get_json_async ::vapi::_async_for_usage_stale_test
set ::vapi::usage_held {}
proc ::vapi::get_json_async {path cmd {ms 0}} {
    lappend ::vapi::usage_held [list $path $cmd]
    return tok
}
proc answer_usage_pair {which value} {
    set callback [lindex $::vapi::usage_held $which 1]
    uplevel #0 [list {*}$callback 1 $value]
}
proc fail_usage_pair {which} {
    set callback [lindex $::vapi::usage_held $which 1]
    uplevel #0 [list {*}$callback 0 {HTTP 502: gateway down}]
}
set ::vchat::session session-A
::voverlay::_usage .ov_switch
set ::vchat::session session-B
::voverlay::_usage_fill .ov_switch
answer_usage_pair 0 [dict create prompt_tokens 100 completion_tokens 10 calls 1 \
    session [dict create prompt_tokens 111 completion_tokens 11 calls 1]]
answer_usage_pair 1 [dict create enabled false reason {no quota for the fixture}]
ok "A's stale pair never paints into B while its replacement is outstanding" \
    {![string match "*111*" [overlay_text .ov_switch]]}
ok "the session switch queued exactly one replacement request pair" \
    {[llength $::vapi::usage_held] == 4}
answer_usage_pair 2 [dict create prompt_tokens 200 completion_tokens 20 calls 2 \
    session [dict create prompt_tokens 222 completion_tokens 22 calls 2]]
answer_usage_pair 3 [dict create enabled false reason {no quota for the fixture}]
ok "the current session's answer paints in the open window" \
    {[string match "*222*" [overlay_text .ov_switch]]}
set usage_b [overlay_text .ov_switch]
answer_usage_pair 0 [dict create prompt_tokens 300 completion_tokens 30 calls 3 \
    session [dict create prompt_tokens 333 completion_tokens 33 calls 3]]
ok "a late answer from the previous session cannot replace its figures" \
    {[overlay_text .ov_switch] eq $usage_b && ![string match "*333*" [overlay_text .ov_switch]]}
::voverlay::_usage_fill .ov_switch
::voverlay::_usage_fill .ov_switch
fail_usage_pair 4
fail_usage_pair 5
ok "a failed refresh retains the last good usage figures" \
    {[overlay_text .ov_switch] eq $usage_b}
ok "…reports the failure and settles the failed request" \
    {[string match "*502*" $::vmanage::said] && ![info exists ::voverlay::usage_pending(6)]}
ok "…and does not lose a refresh queued during the failed request" \
    {[llength $::vapi::usage_held] == 8 && $::voverlay::usage_fill == 1}
answer_usage_pair 6 [dict create prompt_tokens 500 completion_tokens 50 calls 5]
answer_usage_pair 7 [dict create enabled false reason {no quota for the fixture}]
ok "the queued recovery request paints and settles" \
    {[string match "*500*" [overlay_text .ov_switch]] && $::voverlay::usage_fill == 0}
destroy .ov_switch
::voverlay::_usage .ov_switch
set closing_callbacks [lrange $::vapi::usage_held 8 9]
destroy .ov_switch
::voverlay::_usage .ov_switch
set reopen_count [llength $::vapi::usage_held]
foreach request $closing_callbacks {
    uplevel #0 [list {*}[lindex $request 1] 1 \
        [dict create prompt_tokens 999 completion_tokens 999 calls 999]]
}
ok "a closed window's late callbacks cannot paint into its replacement" \
    {![string match "*999*" [overlay_text .ov_switch]] && $::voverlay::usage_fill == 1}
answer_usage_pair [expr {$reopen_count - 2}] [dict create \
    prompt_tokens 400 completion_tokens 40 calls 4]
answer_usage_pair [expr {$reopen_count - 1}] [dict create enabled false reason {no quota for the fixture}]
ok "the reopened window completes its own request after the old replies" \
    {[string match "*400*" [overlay_text .ov_switch]] && $::voverlay::usage_fill == 0}
rename ::vapi::get_json_async {}
rename ::vapi::_async_for_usage_stale_test ::vapi::get_json_async
destroy .ov_switch
set ::vchat::session sess-1
set ::vmanage::usage_payload [usage_payload]

# ─ the absences, each named ──────────────────────────────────────────

set cold [dict create enabled true cold true ttl_secs 300 poll_secs 900]
set t [fill $cold]
ok "before the first warm the overlay says there is no reading yet" \
    {[string match "*no reading yet*" $t]}
ok "…and shows no percentage at all" {![string match "*%*" $t]}

set off [dict create enabled false reason {[tokenplan] enabled = false in config.toml}]
set t [fill $off]
ok "a disabled tokenplan says so, and names the config key" \
    {[string match "*enabled = false*" $t]}

set nopool [dict create enabled true cold false age_secs 1 ttl_secs 300 poll_secs 900 \
    team [dict create released_seats 1 seats [list [dict create \
        seat_id seat_dead status RELEASE assigned UNASSIGNED \
        credits_total null credits_remaining null used_fraction null]]] \
    team_error null personal_error null]
set t [fill $nopool]
ok "a seat whose pool the API omitted reads 'not reported', not 0 / 0" \
    {[string match "*not reported*" $t] && ![string match "*0 / 0*" $t]}

set broken [dict create enabled true cold false age_secs 1 ttl_secs 300 poll_secs 900 \
    team_error {AccessKey rejected} personal_error {console session expired}]
set t [fill $broken]
ok "a failing team path is named, under the team header" \
    {[string match "*team*AccessKey rejected*" $t]}
ok "…and so is a failing personal path, independently" \
    {[string match "*personal*console session expired*" $t]}
ok "…and a plan with no reading says so rather than resting on the last one" \
    {[string match "*kimi*not reported*" $t]}

# The HTTP layer itself falling over is SAID, the panetest way.
set ::vapi::fail 1
set w [fresh_overlay]
::voverlay::_quota_fill $w 0 0
update idletasks
ok "an unreachable gateway is reported on the status line, not swallowed" \
    {[string match "*502*" $::vmanage::said]}
set ::vapi::fail 0

# ── the status bar field ──────────────────────────────────────────────
#
# USER 2026-09-20: "…and the status line". A bare "plan 59%" named
# neither the pool nor the bill; it now reads "<plan> <pct>%".
puts "\nthe status field names the plan it is showing"

frame .statusbar
label .statusbar.quota -text ""
pack .statusbar.quota
update idletasks

proc paint {payload} {
    set ::vapi::quota $payload
    ::vstatus::_paint_quota
    update
    return [.statusbar.quota cget -text]
}

proc paint_on_route {route payload} {
    ::vstatus::set_route $route
    set ::vapi::quota $payload
    ::vstatus::_paint_quota
    update
    return [.statusbar.quota cget -text]
}

# ── the relevance contract (2026-09-16), now with names (2026-09-20) ──
#
# USER: the plans are DISJOINT — showing team's remainder on a personal
# session (or either Alibaba figure on a z.ai session) answers the wrong
# question. The field reads the plan the ROUTE says it draws from, and
# since 2026-09-20 it SAYS WHICH.
ok "a z.ai session reads the z.ai pool, and says so" \
    {[paint_on_route {custom:zai-coding-plan/glm-5.2} [live_payload {custom:zai-coding-plan}]] eq "zai 60%"}
ok "a team session reads the team pool, and says so" \
    {[paint_on_route {ali-token-plan-team/deepseek-v4.1-flash} [live_payload ali-token-plan-team]] eq "team 55%"}
ok "a personal session reads the personal pool, not the team's" \
    {[paint_on_route {ali-token-plan-personal/qwen3.8-flash} [live_payload ali-token-plan-personal]] eq "personal 0%"}
ok "a kimi session reads the kimi pool" \
    {[paint_on_route {custom:kimi-coding-plan/k3} [live_payload {custom:kimi-coding-plan}]] eq "kimi 100%"}
# THE CASE THE OLD MAPPING GOT WRONG: `ali-responses` draws on the TEAM
# key, and its name contains neither "team" nor "personal".
ok "an ali-responses session reads the TEAM pool" \
    {[paint_on_route {ali-responses/qwen3.8-max} [live_payload ali-responses]] eq "team 55%"}
ok "an effort suffix on the route does not confuse the provider" \
    {[paint_on_route {zai-coding-plan/glm-5.2 (effort high)} [live_payload {custom:zai-coding-plan}]] eq "zai 60%"}

# The engine resolves; the surface renders. A payload that names a plan
# this desktop has never heard of still renders THAT plan's label,
# because the label comes off the wire rather than a local table.
set exotic [dict replace [live_payload] active_plan moonshot active_plan_fraction 0.25]
ok "the plan label comes from the engine, not a local table" \
    {[paint_on_route {something-new/model-x} $exotic] eq "moonshot 25%"}

# A provider on NO plan: the field must not show somebody else's pool.
# It shows the label with no number — the fact is "no upstream reading
# for this route", not a percentage off a different bill.
set t_np [paint_on_route {openrouter/some-model} [live_payload openrouter]]
ok "a session on no plan shows the fact, not another plan's number" \
    {[string match "*no plan*" $t_np]}
ok "…and never another plan's name or figure beside it" \
    {![string match "*personal*" $t_np] && ![string match "*55%*" $t_np]}

# Async reads are generation-scoped: a slow reply for the route just
# left must not replace the new route's figure after that later answer.
set ::vapi::held {}
rename ::vapi::get_json_async ::vapi::_async_for_stale_test
proc ::vapi::get_json_async {path cmd {ms 0}} {
    lappend ::vapi::held $cmd
    return $path
}
::vstatus::set_route {ali-token-plan-team/deepseek}
::vstatus::_paint_quota
::vstatus::set_route {custom:zai-coding-plan/glm}
::vstatus::_paint_quota
lassign $::vapi::held old_callback new_callback
uplevel #0 [list {*}$new_callback 1 \
    [dict create enabled true cold false active_plan zai active_plan_fraction 0.60 age_secs 3 poll_secs 900]]
uplevel #0 [list {*}$old_callback 1 \
    [dict create enabled true cold false active_plan team active_plan_fraction 0.55 age_secs 3 poll_secs 900]]
set stale_quota_text [.statusbar.quota cget -text]
ok "a stale quota reply cannot overwrite the new route's plan" \
    {[string match "*zai*60*" $stale_quota_text] \
     && ![string match "*team*55*" $stale_quota_text]}
rename ::vapi::get_json_async {}
rename ::vapi::_async_for_stale_test ::vapi::get_json_async
set ::vapi::held {}

# A z.ai session whose plan has no reading yet: the dash, never another
# plan's number presented as this session's.
set nozai [dict replace [live_payload {custom:zai-coding-plan}] \
    active_plan_fraction null]
ok "a plan with no reading shows its name and a dash" \
    {[paint_on_route {zai-coding-plan/glm-5.2} $nozai] eq "zai —"}

::vstatus::set_route {}
set hot [dict replace [live_payload] active_plan team active_plan_fraction 0.93]
ok "a pool past ninety percent paints red, name and all" \
    {[paint $hot] eq "team 93%"
     && [.statusbar.quota cget -foreground] eq $::vt::T(red)}

set warm [dict replace [live_payload] active_plan team active_plan_fraction 0.80]
ok "past seventy-five it paints gold" \
    {[paint $warm] eq "team 80%"
     && [.statusbar.quota cget -foreground] eq $::vt::T(gold)}

ok "under half it stays dim" \
    {[paint [dict replace [live_payload] active_plan team \
        active_plan_fraction 0.10]] eq "team 10%"
     && [.statusbar.quota cget -foreground] eq $::vt::T(fg_dim)}

ok "a disabled tokenplan shows nothing rather than a zero" \
    {[paint $off] eq ""}
ok "a cold cache shows a dash, not a number it does not have" \
    {[paint $cold] eq "plan —"}

set ::vapi::fail 1
ok "an unreachable gateway leaves the field empty rather than stale" \
    {[paint [live_payload]] eq ""}
set ::vapi::fail 0

puts "\nquotatest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]