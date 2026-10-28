#!/usr/bin/env wish
# scripts/shot-quota.tcl — the inner half of shot-quota.sh.
#
# Sources the SHIPPING modules (theme, i18n, overlays, statusbar) and
# stubs only ::vapi — the wire — so the two readers under test are the
# real procs. Fills, shoots, exits.

set dir [file join [file dirname [file normalize [info script]]] .. veles-desktop lib]

source [file join $dir paths.tcl]
source [file join $dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $dir i18n.tcl]
::vi18n::init
package require json
source [file join $dir jsonfield.tcl]

# ─ the wire, stubbed ────────────────────────────────────────────────
namespace eval ::vapi {
    variable quota {}
    variable usage {}
    proc urlencode {s} { return $s }
    proc get_json {path} {
        variable quota
        variable usage
        if {[string match "/api/usage*" $path]} { return $usage }
        # The engine resolves the plan from the provider the surface
        # sends (`?provider=`), so this stub must too — otherwise every
        # row of the shot would show whichever payload was canned.
        set prov ""
        if {[regexp {[?&]provider=([^&]*)} $path -> prov]} {
            set table [dict create \
                ali-token-plan-team team \
                ali-token-plan-personal personal \
                ali-responses team \
                zai-coding-plan zai \
                kimi-coding-plan kimi \
                openrouter ""]
            if {[dict exists $table $prov]} {
                set plan [dict get $table $prov]
                set d $quota
                dict set d active_plan [expr {$plan eq "" ? "null" : $plan}]
                if {$plan ne ""} {
                    dict set d active_plan_fraction \
                        [dict get $d ${plan}_used_fraction]
                } else {
                    dict set d active_plan_fraction "null"
                }
                return $d
            }
        }
        return $quota
    }
}

# ── the fixture: what the engine answers on this box ─────────────────
# Figures are the real ones read off 127.0.0.1:8642 during the round.
set q_json {{
  "enabled": true, "cold": false, "stale": false,
  "age_secs": 42, "ttl_secs": 300, "poll_secs": 900,
  "active_plan": "personal", "active_plan_fraction": 0.005,
  "team_used_fraction": 0.5964,
  "personal_used_fraction": 0.005,
  "zai_used_fraction": 0.99,
  "kimi_used_fraction": 0.0546,
  "tightest_used_fraction": 1.0,
  "team": {
    "seats": [{
      "seat_id": "seat_e5cfa7b0", "subscription": "subs-42217ce2",
      "spec_type": "max", "status": "NORMAL", "assigned": "ASSIGNED",
      "credits_total": 250000, "credits_remaining": 111650.31954763,
      "credits_unit": "Credit", "used_fraction": 0.55339872180948,
      "cycle_start": "2026-09-03T09:00:00+00:00",
      "cycle_end": "2026-10-03T09:00:00+00:00"
    }],
    "spend": {"bill_month": "2026-09", "currency": "USD",
              "total": 253.93, "pretax": 206.45, "tax": 47.48}
  },
  "personal": {"windows": [
    {"label": "month", "used_fraction": 0.005,
     "resets_at": "2026-10-03T16:00:00+00:00"}
  ]},
  "zai": {"level": "pro",
    "window_5h": {"label": "5-hour", "used_fraction": 0.01,
       "resets_at": "2026-09-15T15:48:10+00:00"},
    "weekly": {"label": "weekly", "used_fraction": 0.60,
       "resets_at": "2026-09-20T20:28:33+00:00"},
    "tools": {"limit": 1000, "used": 69, "remaining": 931,
       "used_fraction": 0.069, "resets_at": "2026-10-01T12:58:51+00:00",
       "details": [{"model": "search-prime", "usage": 54},
                   {"model": "web-reader", "usage": 15},
                   {"model": "zread", "usage": 0}]}
  },
  "kimi": {"windows": [
    {"label": "5-hour", "used_fraction": 0.0,
     "resets_at": "2026-09-20T20:22:56+00:00"},
    {"label": "month \u00b7 code", "used_fraction": 0.0,
     "resets_at": "2026-10-20T00:00:00+00:00"},
    {"label": "month", "used_fraction": 0.0546,
     "resets_at": "2026-10-20T00:00:00+00:00"}
  ]}
}}

set u_json {{
  "prompt_tokens": 1870018956, "completion_tokens": 14174355,
  "cached_tokens": 1825253522, "cache_write_tokens": 0, "calls": 13540,
  "scope": "all sessions",
  "by_plan": [
    {"plan": "team", "providers": ["ali-responses", "ali-token-plan-team"],
     "calls": 3506, "prompt_tokens": 472971728, "completion_tokens": 4943286,
     "cached_tokens": 0, "total_tokens": 477915014},
    {"plan": "personal", "providers": ["ali-token-plan-personal"],
     "calls": 6964, "prompt_tokens": 1007183171, "completion_tokens": 7703844,
     "cached_tokens": 0, "total_tokens": 1014887015},
    {"plan": "zai", "providers": ["zai-coding-plan"],
     "calls": 2753, "prompt_tokens": 361717710, "completion_tokens": 1430622,
     "cached_tokens": 0, "total_tokens": 363148332},
    {"plan": "kimi", "providers": ["kimi-coding-plan"],
     "calls": 282, "prompt_tokens": 25679710, "completion_tokens": 60002,
     "cached_tokens": 0, "total_tokens": 25739712},
    {"plan": null, "providers": ["openrouter"],
     "calls": 20, "prompt_tokens": 1485632, "completion_tokens": 4806,
     "cached_tokens": 0, "total_tokens": 1490438}
  ],
  "session": {"prompt_tokens": 4500, "completion_tokens": 60,
              "cached_tokens": 3300, "calls": 12},
  "session_by_plan": [
    {"plan": "team", "providers": ["ali-token-plan-team"],
     "calls": 12, "prompt_tokens": 4500, "completion_tokens": 60,
     "cached_tokens": 3300, "total_tokens": 4560}
  ]
}}

set ::vapi::quota [::json::json2dict $q_json]
set ::vapi::usage [::json::json2dict $u_json]

# The overlay asks through ::vmanage::get (the pane's fetch layer), the
# status field through ::vapi::get_json. Both stubs, so the procs under
# this shot are the real ones.
namespace eval ::vmanage {
    variable gets {}
    proc get {w path} {
        variable gets
        lappend gets $path
        if {[string match "*/api/usage*" $path]} { return $::vapi::usage }
        return {}
    }
    # The pane's status line — the fill talks to it on the way out.
    variable said {}
    proc say {w msg {kind info}} {
        variable said
        set said $msg
    }
}

# statusbar.tcl is sourced by main.tcl normally; pull what _paint_quota
# needs without booting the whole window.
source [file join $dir overlays.tcl]
source [file join $dir statusbar.tcl]

# The overlay scopes its usage question to the on-screen session.
namespace eval ::vchat { variable session "" }

wm title . "veles — quota paint (fixture)"
wm geometry . 1100x900

# Two frames side by side: the overlay, and a status strip.
ttk::frame .left
ttk::frame .right
pack .left -side left -fill both -expand 1 -padx 8 -pady 8
pack .right -side right -fill y -padx 8 -pady 8

# The REAL scroll wrapper (2026-09-23) — the shot must show the window
# the user gets, scrollbar and all, and the content is taller than this
# 900px frame anyway.
::voverlay::_usage_scroll .left
::voverlay::_usage_fill .left 0

# The status field, painted by the real proc. The widget NAME matters:
# _paint_quota refuses to touch anything but .statusbar.quota.
ttk::frame .statusbar
pack .statusbar -side top -anchor w
label .statusbar.quota -text "" -font {TkDefaultFont 11}
pack .statusbar.quota -side left
namespace eval ::vstatus { variable route_prov "ali-token-plan-personal" }
::vstatus::_paint_quota

ttk::label .right.t -text "the same reading,\nunder each route —\nthe field names the pool:" \
    -justify left -font {TkDefaultFont 10}
pack .right.t -side top -anchor w -pady {0 8}

foreach prov {ali-token-plan-team ali-token-plan-personal zai-coding-plan kimi-coding-plan openrouter} {
    label .right.$prov -text "" -font {TkDefaultFont 11}
    pack .right.$prov -side top -anchor w
    set ::vstatus::route_prov $prov
    ::vstatus::_paint_quota
    catch { .right.$prov configure -text [format "%-26s %s" $prov [.statusbar.quota cget -text]] }
}

update idletasks
after 200

proc shoot {out name} {
    update idletasks
    catch { exec import -window root [file join $out "$name.png"] } e
    if {[info exists e] && $e ne ""} { puts "shot failed: $e" }
}

set out $::env(OUT_DIR)
shoot $out 01-usage-split-and-plans
wm geometry . 1100x900+0+0
update idletasks
puts "shot written"
after 100
exit 0