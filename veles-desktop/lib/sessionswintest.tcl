#!/usr/bin/env wish
# veles-desktop/lib/sessionswintest.tcl — the Sessions window (::vsessions).
#
# USER 2026-10-04: "/sessions doesn't show full list. Should show a
# window with a session search+explore + all the session actions".
#
# Drives the REAL window (sessions.tcl on the real manage.tcl chrome)
# against a stubbed wire: GET /api/sessions answers pages of a 250-row
# fixture honouring limit/offset, and every request is recorded, so the
# query the window SENDS is what is asserted — paging, search, the
# titled filter, the stale-reply drop, the preview, and a multi-row
# delete's one POST.

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
package require json
package require http
source [file join $_dir jsonfield.tcl]

set pass 0
set fail 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        incr ::pass
        puts "  ✓ $name"
    } else {
        incr ::fail
        puts "  ✗ $name"
    }
}

# ── the wire ────────────────────────────────────────────────────────
#
# 250 sessions, newest first. Every 3rd is untitled (its first user line
# is the preview); s-0007 is running.
set ::fixture {}
for {set i 0} {$i < 250} {incr i} {
    set id [format "s-%04d" $i]
    set title [expr {$i % 3 == 0 ? "" : "title $i"}]
    lappend ::fixture [::json::json2dict [format \
        {{"id":"%s","title":"%s","preview":"first line %d","parent":null,"created_at":"2026-10-03 10:00:00","updated_at":"2026-10-03 12:%02d:00","message_count":%d,"running":%s}} \
        $id $title $i [expr {$i % 60}] [expr {$i + 1}] [expr {$i == 7 ? "true" : "false"}]]]
}

namespace eval ::vapi {
    variable gets {}
    variable posts {}
    variable bodies {}
    variable held {}
    variable hold 0
    proc unattached {} { return 0 }
    proc _arg {path key default} {
        if {[regexp "\[?&\]$key=(\[^&\]*)" $path -> v]} { return $v }
        return $default
    }
    proc answer {path} {
        if {[string match "/api/sessions/*/head*" $path]} {
            return [dict create messages [list \
                [dict create role user content "hello there"] \
                [dict create role assistant content "general kenobi"]]]
        }
        set off [_arg $path offset 0]
        set lim [_arg $path limit 100]
        set rows [lrange $::fixture $off [expr {$off + $lim - 1}]]
        return [dict create sessions $rows total [llength $::fixture] offset $off]
    }
    # Asynchronous, as the real one: the reply is handed over from the
    # event loop, unless the suite is holding replies to reorder them.
    proc get_json_async {path cmd {ms 0}} {
        variable gets
        variable held
        variable hold
        lappend gets $path
        if {$hold} {
            lappend held [list $cmd [answer $path]]
        } else {
            after 0 [list uplevel #0 [list {*}$cmd 1 [answer $path]]]
        }
        return tok
    }
    proc post_json {path body} {
        variable posts
        variable bodies
        lappend posts $path
        lappend bodies $body
        if {$path eq "/api/sessions/delete"} {
            return [dict create deleted 3]
        }
        return [dict create ok 1]
    }
    proc json_raw {t} { return [list ::vapi::__json__ $t] }
    proc json_strings {items} { return [json_raw "\[[join $items ,]\]"] }
    proc json_string {s} { return [json_raw "\"$s\""] }
    proc reason {e} { return $e }
}
namespace eval ::voverlay {
    proc _header {w title} {}
    proc open {which} {
        if {![winfo exists .ov_$which]} { ::vsessions::build .ov_$which }
    }
}
namespace eval ::va11y { proc name {w l} {} }
namespace eval ::vcoach { proc attach {w} {} }
namespace eval ::vctx { proc attach {w e} {} }
namespace eval ::vsidebar {
    variable pins {s-0002}
    variable soon 0
    proc refresh_soon {} { incr ::vsidebar::soon }
}
namespace eval ::vchat {
    variable cleared {}
    proc clear_if_current {sid} { lappend ::vchat::cleared $sid }
    proc open_session {sid} { set ::vchat::opened $sid }
}
proc tk_messageBox {args} { return yes }

source [file join $_dir manage.tcl]
source [file join $_dir sessions.tcl]

proc settle {} { update ; update idletasks ; update }
proc tree {} { return [::vsessions::_tree] }

puts "sessions window"
::voverlay::open sessions
settle
ok "the first page is 200 rows" {[llength [[tree] children {}]] == 200}
ok "the status line says how many of how many" \
    {[string match "*200*250*" [.ov_sessions.bar.status cget -text]]}
ok "Load more is offered while rows remain" \
    {[.ov_sessions.foot.more cget -state] eq "normal"}
ok "an untitled row shows its first line, dimmed" \
    {[lindex [[tree] item s-0003 -values] 0] eq "first line 3"
     && "dim" in [[tree] item s-0003 -tags]}
ok "a titled row shows its title" {[lindex [[tree] item s-0004 -values] 0] eq "title 4"}
ok "a pinned session carries the star" {[string match "★ *" [lindex [[tree] item s-0002 -values] 0]]}
ok "a running session says so" {[lindex [[tree] item s-0007 -values] 3] ne ""}

puts "\nparent-linked sessions form a tree"
set ::all_fixture $::fixture
namespace eval ::vapi {
    proc answer {path} {
        if {[string match "/api/sessions/*/head*" $path]} {
            return [dict create messages [list \
                [dict create role user content "hello there"] \
                [dict create role assistant content "general kenobi"]]]
        }
        if {[dict exists $::fixture $path]} { return [dict get $::fixture $path] }
        set off [_arg $path offset 0]
        set lim [_arg $path limit 100]
        set rows [lrange $::fixture $off [expr {$off + $lim - 1}]]
        return [dict create sessions $rows total [llength $::fixture] offset $off]
    }
}
set ::fixture [dict create {*}[::vsessions::_query 0] [dict create sessions [list \
    [dict create id child-first title "subagent: review" preview "" parent parent-1 \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:03:00" message_count 3 running false] \
    [dict create id parent-1 title "review workflows" preview "" parent "" \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:02:00" message_count 4 running false] \
    [dict create id orphan title "subagent: older page" preview "" parent missing-parent \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:01:00" message_count 2 running false]] total 3 offset 0]]
::vsessions::load 0
settle
ok "a child is nested even when it arrives before its parent" \
    {[[tree] parent child-first] eq "parent-1"}
ok "a child is visibly marked" \
    {[string match "↳ *" [lindex [[tree] item child-first -values] 0]]}
ok "an orphan stays visible at the root during paging or filtering" \
    {[[tree] parent orphan] eq "" && [string match "↳ *" [lindex [[tree] item orphan -values] 0]]}
set ::fixture [dict create {*}[::vsessions::_query 0] [dict create sessions [list \
    [dict create id cycle-a title "cycle a" preview "" parent cycle-b \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:01:00" message_count 1 running false] \
    [dict create id cycle-b title "cycle b" preview "" parent cycle-a \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:00:00" message_count 1 running false]] total 2 offset 0]]
::vsessions::load 0
settle
ok "an invalid parent cycle never hides either session" \
    {[llength [[tree] children {}]] == 2 && [[tree] exists cycle-a] && [[tree] exists cycle-b]}
set ::fixture [dict create {*}[::vsessions::_query 0] [dict create sessions [list \
    [dict create id parent-1 title "review workflows" preview "" parent "" \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:02:00" message_count 4 running false]] total 2 offset 0]]
::vsessions::load 0
settle
set ::fixture [dict create {*}[::vsessions::_query 1] [dict create sessions [list \
    [dict create id child-later title "subagent: later page" preview "" parent parent-1 \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:03:00" message_count 3 running false]] total 2 offset 1]]
::vsessions::load 1
settle
ok "a child arriving on a later page joins its loaded parent" \
    {[[tree] parent child-later] eq "parent-1" && [llength [[tree] children {}]] == 1}

set ::fixture [dict create {*}[::vsessions::_query 0] [dict create sessions [list \
    [dict create id child-early title "subagent: early page" preview "" parent parent-later \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:03:00" message_count 3 running false]] total 2 offset 0]]
::vsessions::load 0
settle
ok "a child whose parent is not loaded stays visible at the root" \
    {[[tree] parent child-early] eq "" && [[tree] exists child-early]}
set ::fixture [dict create {*}[::vsessions::_query 1] [dict create sessions [list \
    [dict create id parent-later title "parent on a later page" preview "" parent null \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:02:00" message_count 4 running false]] total 2 offset 1]]
::vsessions::load 1
settle
ok "a later-arriving parent adopts the already loaded orphan" \
    {[[tree] parent child-early] eq "parent-later" && [llength [[tree] children {}]] == 1}

set ::fixture [dict create {*}[::vsessions::_query 0] [dict create sessions [list \
    [dict create id paged-child title "subagent: supplied ancestor" preview "" parent paged-parent \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:03:00" message_count 3 running false]] \
    total 1 offset 0 parents [list \
    [dict create id paged-parent title "supplied ancestor" preview "" parent null \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:02:00" message_count 4 running false]]]]
::vsessions::load 0
settle
ok "the separate ancestor context places a child without changing the page total" \
    {[[tree] parent paged-child] eq "paged-parent" && $::vsessions::offset == 1}

# SI-6: the PARENT is on the page; its child ranks far past it and comes
# as downward context (`children`), which must nest under it.
set ::fixture [dict create {*}[::vsessions::_query 0] [dict create sessions [list \
    [dict create id onpage-parent title "a visible parent" preview "" parent null \
        created_at "2026-10-03 10:00:00" updated_at "2026-10-03 12:05:00" message_count 4 running false]] \
    total 900 offset 0 parents {} children [list \
    [dict create id offpage-child title "subagent: ranked 922" preview "" parent onpage-parent \
        created_at "2026-10-01 10:00:00" updated_at "2026-10-01 10:01:00" message_count 2 running false \
        context_only true]]]]
::vsessions::load 0
settle
ok "a child beyond the page arrives as context and nests under its on-page parent" \
    {[[tree] exists offpage-child] && [[tree] parent offpage-child] eq "onpage-parent"
     && $::vsessions::offset == 1}

set ::fixture $::all_fixture

::vsessions::load 0
settle
::vsessions::load 1
settle
ok "Load more appends the rest" {[llength [[tree] children {}]] == 250}
ok "…from the offset the first page ended at" {[string match "*offset=200*" [lindex $::vapi::gets end]]}
ok "…and is spent" {[.ov_sessions.foot.more cget -state] eq "disabled"}

puts "\nsearch and filter"
set ::vsessions::q "first line 12"
::vsessions::search
settle
ok "the search goes to the gateway, encoded" \
    {[string match "*q=first%20line%2012*" [lindex $::vapi::gets end]]
     || [string match "*q=first+line+12*" [lindex $::vapi::gets end]]}
set ::vsessions::fts 1
::vsessions::search
settle
ok "message-text search asks for fts" {[string match "*fts=1*" [lindex $::vapi::gets end]]}
set ::vsessions::fts 0
set ::vsessions::q ""
.ov_sessions.find.kind current 2
::vsessions::_kind_changed
settle
ok "the Untitled filter asks for untitled" {[string match "*titled=untitled*" [lindex $::vapi::gets end]]}
.ov_sessions.find.kind current 0
::vsessions::_kind_changed
settle
ok "All sessions sends no filter" {![string match "*titled=*" [lindex $::vapi::gets end]]}

puts "\na stale reply is dropped"
set ::vapi::hold 1
set ::vapi::held {}
::vsessions::load 0
::vsessions::load 0
set ::vapi::hold 0
lassign [lindex $::vapi::held 1] cmd2 resp2
lassign [lindex $::vapi::held 0] cmd1 resp1
set resp1 [dict replace $resp1 sessions [lrange [dict get $resp1 sessions] 0 4]]
uplevel #0 [list {*}$cmd2 1 $resp2]
uplevel #0 [list {*}$cmd1 1 $resp1]
ok "the older search's late reply does not repaint the list" \
    {[llength [[tree] children {}]] == 200}

puts "\npreview"
[tree] selection set s-0010
::vsessions::preview
settle
ok "selecting a row asks for its head" {[string match "/api/sessions/s-0010/head*" [lindex $::vapi::gets end]]}
ok "…and shows it" {[string match "*general kenobi*" [.ov_sessions.pw.pvf.t get 1.0 end]]}

puts "\nactions"
::vsessions::open_selected
ok "Open opens the selected session" {$::vchat::opened eq "s-0010"}
[tree] selection set {s-0011 s-0012 s-0013}
::vsessions::delete_selected
settle
ok "a multi-row delete is ONE request" \
    {[llength [lsearch -all $::vapi::posts /api/sessions/delete]] == 1}
ok "…naming every selected session" \
    {[string match "*s-0011*s-0012*s-0013*" [lindex $::vapi::bodies end]]}
ok "…each one cleared if it was on screen" {$::vchat::cleared eq {s-0011 s-0012 s-0013}}
ok "…and the sidebar is told to refresh" {$::vsidebar::soon > 0}
ok "the status line reports the count" \
    {[string match "*3*" [.ov_sessions.bar.status cget -text]]}

puts "\ntimes"
set local [clock format [clock scan "2026-10-03 12:00:00" -format "%Y-%m-%d %H:%M:%S" -gmt 1] \
    -format "%Y-%m-%d %H:%M"]
ok "a UTC stamp is shown in local time" {[::vsessions::_when "2026-10-03 12:00:00"] eq $local}

puts "\nsessionswintest: $pass passed, $fail failed"
exit [expr {$fail ? 1 : 0}]
