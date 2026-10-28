#!/usr/bin/env wish
# veles-desktop/lib/sidebartest.tcl — the session list buckets by age.
#
# USER: "the session list scroll could probably also grow too long —
# let's divide it into section: Today, Earlier this week (last 7 days)
# and Archive (collapsed list, lazy load)". Three promises:
#   1. sessions split into TODAY / EARLIER THIS WEEK / ARCHIVE by
#      their created_at against the wall clock;
#   2. ARCHIVE is collapsed by default — its rows are NOT inserted
#      until toggled (lazy load), so the list stays short;
#   3. a BUSY archive session stays visible through the collapse (a
#      live turn must never be hidden).

package require json

set _dir [file dirname [file normalize [info script]]]

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

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

source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir i18n.tcl]
::vi18n::init

# Collaborators.
namespace eval ::vstatus { proc set_msg {m} { set ::vstatus::last $m } }
namespace eval ::va11y  { proc name {w t} {} ; proc tab_order {w} {} }
namespace eval ::vcoach { proc attach {w} {} }
namespace eval ::vctx   { proc attach {w items} {} }
namespace eval ::vchat  {
    variable session ""
    # A miniature of the real reconcile (chat.tcl's sync_running): the
    # engine's verdict from the listing is what ▶ is drawn from. Here it
    # also pins the ORDER — refresh must reconcile BEFORE it draws the
    # rows, or every ▶ is one refresh behind the truth.
    variable live {}
    proc sync_running {sessions} {
        set ::vchat::live {}
        foreach s $sessions {
            catch {
                if {[dict get $s running]} { dict set ::vchat::live [dict get $s id] 1 }
            }
        }
    }
    proc is_running {sid} {
        return [expr {$sid eq "BUSY-SID" || [dict exists $::vchat::live $sid]}]
    }
    # COUNTED: the recursion guard below is asserted as a number of
    # opens, not as an absence of symptoms.
    proc open_session {s} { set ::last_opened $s ; incr ::opens }
    proc retitle_window {} {}
}
set ::opens 0
namespace eval ::vapi {
    variable sessions {}
    # COUNTED: mark_current must cost NO round trip (refresh blocks on
    # this call, and a session switch is meant to be one `pack`).
    variable gets 0
    proc unattached {} { return 0 }
    proc get_json {path} { incr ::vapi::gets ; return [dict create sessions $::vapi::sessions] }
    proc post {p b} { return {} }
    proc delete {p} { return {} }
    proc json_escape {s} { return $s }
}

source [file join $_dir jsonfield.tcl]
source [file join $_dir sidebar.tcl]

# Build the real panel — build() creates its own widgets at the
# .sidebar path, so destroy anything we pre-created at that path.
foreach w {.sidebar .sidebar.filter .sidebar.list .sidebar.btns .sidebar.btns.new .sidebar.btns.pin .sidebar.btns.rename .sidebar.btns.del .sidebar.btns.reload} {
    catch { destroy $w }
}
::vsidebar::build

proc sessions_of {now} {
    set out {}
    # Active today.
    lappend out [dict create id "TODAY-1" title "today one" message_count 1 \
        created_at [clock format [expr {$now - 40*86400}] -format "%Y-%m-%d %H:%M:%S"] \
        updated_at [clock format $now -format "%Y-%m-%d %H:%M:%S"]]
    # 3 days ago = this week.
    lappend out [dict create id "WEEK-1" title "three days ago" message_count 2 \
        created_at [clock format $now -format "%Y-%m-%d %H:%M:%S"] \
        updated_at [clock format [expr {$now - 3*86400}] -format "%Y-%m-%d %H:%M:%S"]]
    # 30 days ago = archive.
    lappend out [dict create id "OLD-1" title "month old" message_count 3 \
        created_at [clock format $now -format "%Y-%m-%d %H:%M:%S"] \
        updated_at [clock format [expr {$now - 30*86400}] -format "%Y-%m-%d %H:%M:%S"]]
    lappend out [dict create id "OLD-2" title "two months" message_count 4 \
        created_at [clock format $now -format "%Y-%m-%d %H:%M:%S"] \
        updated_at [clock format [expr {$now - 60*86400}] -format "%Y-%m-%d %H:%M:%S"]]
    return $out
}

proc body {} { return [.sidebar.list get 0 end] }

set ::vapi::sessions [sessions_of [clock seconds]]
set ::vsidebar::archive_collapsed 1
::vsidebar::refresh

puts "\nbucketing by LAST ACTIVITY (not creation)"
# TODAY-1 was created 40 days ago but touched now → must land under
# TODAY, proving the sort key is updated_at not created_at.
ok "a TODAY header appears" \
    {[lsearch -glob [body] "*TODAY*"] >= 0}
ok "an EARLIER THIS WEEK header appears" \
    {[lsearch -glob [body] "*EARLIER THIS WEEK*"] >= 0}
ok "an ARCHIVE header appears, collapsed" \
    {[lsearch -glob [body] "*ARCHIVE (2) ▸ show*"] >= 0}
ok "a session touched today lands under TODAY (not archive)" \
    {[lsearch -glob [body] "*today one*"] >= 0}
ok "today and week rows are inserted" \
    {[lsearch -glob [body] "*today one*"] >= 0
     && [lsearch -glob [body] "*three days ago*"] >= 0}
ok "…but archive rows are NOT (lazy)" \
    {[lsearch -glob [body] "*month old*"] < 0}

# Toggle open, rows appear.
::vsidebar::toggle_archive
ok "toggle expands the archive lazily" \
    {[lsearch -glob [body] "*month old*"] >= 0
     && [lsearch -glob [body] "*two months*"] >= 0
     && [lsearch -glob [body] "*ARCHIVE (2) ▾ hide*"] >= 0}
::vsidebar::toggle_archive
ok "toggle collapses it again" \
    {[lsearch -glob [body] "*month old*"] < 0}

# A busy archive session stays visible even when collapsed.
set ::vapi::sessions [linsert [sessions_of [clock seconds]] 3 \
    [dict create id "BUSY-SID" title "busy old" message_count 9 \
        created_at [clock format [expr {[clock seconds] - 45*86400}] -format "%Y-%m-%d %H:%M:%S"] \
        updated_at [clock format [expr {[clock seconds] - 45*86400}] -format "%Y-%m-%d %H:%M:%S"]]]
set ::vsidebar::archive_collapsed 1
::vsidebar::refresh
ok "a busy archive session survives the collapse" \
    {[lsearch -glob [body] "*busy old*"] >= 0}

# ── which session is open, after a redraw ───────────────────────────
#
# USER: "there should be a better indicator which session is active in
# the session list than highlight because highlight immediately
# disappears on redraw". `refresh` does `delete 0 end` and re-inserts
# every row, and it runs on every turn start and every turn end — so the
# selection highlight, which WAS the only marker, was gone seconds after
# you picked a row.
puts "\nthe open session stays marked through a redraw"

set ::vapi::sessions [sessions_of [clock seconds]]
set ::vsidebar::archive_collapsed 1
set ::vchat::session "TODAY-1"
::vsidebar::refresh
ok "the open session's row carries a marker" \
    {[lsearch -glob [body] "*▸*today one*"] >= 0}
# Scoped to SESSION rows: the collapsed-archive header spells its own
# expand affordance "▸ show", which a bare glob would count as a marked
# session.
ok "…and no other session's row does" \
    {[lsearch -glob [body] "*▸*three days ago*"] < 0}

# The marker is TEXT, so it survives the rebuild that ate the highlight.
::vsidebar::refresh
::vsidebar::refresh
ok "…and it is still there two redraws later" \
    {[lsearch -glob [body] "*▸*today one*"] >= 0}

# Pinned AND open: both facts, neither lost.
set ::vsidebar::pins {TODAY-1}
::vsidebar::refresh
ok "a pinned session that is open shows both marks" \
    {[lsearch -glob [body] "*▸★*today one*"] >= 0}
set ::vsidebar::pins {}

# ── a subagent child is a real session, marked as forked ──────────────
# USER taxonomy (2026-09-17): children are "available to attach and
# review from the session list". The row carries ↳ so a child reads as a
# child at a glance, and the WINDOW TITLE (which follows the row label
# through `labels`) keeps the CLEAN title — no glyph in the title bar.
puts "\na child session is marked ↳; the title stays clean"
set now [clock seconds]
set ::vapi::sessions [list \
    [dict create id "PARENT-1" title "parent task" message_count 5 parent "" \
        created_at [clock format $now -format "%Y-%m-%d %H:%M:%S"] \
        updated_at [clock format $now -format "%Y-%m-%d %H:%M:%S"]] \
    [dict create id "CHILD-1" title "subagent: audit" message_count 3 parent "PARENT-1" \
        created_at [clock format $now -format "%Y-%m-%d %H:%M:%S"] \
        updated_at [clock format $now -format "%Y-%m-%d %H:%M:%S"]]]
set ::vsidebar::archive_collapsed 1
set ::vchat::session "CHILD-1"
::vsidebar::refresh
ok "the child row carries ↳" \
    {[lsearch -glob [body] "*↳*subagent: audit*"] >= 0}
ok "the parent row does NOT" \
    {[lsearch -glob [body] "*↳*parent task*"] < 0}
# The window title follows the clean label: a glyph there is noise, and
# `labels` is what the title reads — assert the map holds the unmarked
# title even though the row shows the marked one.
ok "the title map keeps the clean label (no ↳)" \
    {[dict get $::vsidebar::labels CHILD-1] eq "subagent: audit"}
# Restore the shared fixture: every test below this point assumes the
# `sessions_of` list (WEEK-1 "three days ago", TODAY-1 "today one") and
# TODAY-1 as the open session.
set ::vapi::sessions [sessions_of [clock seconds]]
set ::vchat::session "TODAY-1"
::vsidebar::refresh

# RESTORING THE SELECTION MUST NOT REOPEN THE SESSION. It does not
# today, and the reason is a measured property of Tk rather than a guard
# in our code: `selection set` as a WIDGET COMMAND does not generate
# <<ListboxSelect>> (probed — a bound handler is not called; only user
# interaction and an explicit `event generate` fire it). This pin exists
# because that is exactly the kind of fact somebody "fixes" by adding an
# `event generate` — at which point refresh reopens the session twice a
# turn, forever.
set ::opens 0
::vsidebar::refresh
update
ok "a refresh that restores the selection opens NOTHING" {$::opens == 0}

# …and a real selection still does.
set ::opens 0
set i [lsearch -glob [body] "*three days ago*"]
$::vsidebar::R.list selection clear 0 end
$::vsidebar::R.list selection set $i
::vsidebar::on_select
ok "…while an actual selection still opens it" \
    {$::opens == 1 && $::last_opened eq "WEEK-1"}

# ── the session id, out of the pane ─────────────────────────────────
#
# USER: "need session id right click option to view/copy quickly".
puts "\nthe session id is reachable"

set i [lsearch -glob [body] "*today one*"]
$::vsidebar::R.list selection clear 0 end
$::vsidebar::R.list selection set $i
clipboard clear
::vsidebar::copy_sid
ok "copy puts the ID on the clipboard, not the title" \
    {[clipboard get] eq "TODAY-1"}
ok "…and SAYS what it copied" \
    {[string match "*TODAY-1*" $::vstatus::last]}

::vsidebar::show_sid
ok "the id dialog opens" {[winfo exists .dlg_sid]}
ok "…with the id in a field you can select" \
    {[.dlg_sid.e get] eq "TODAY-1"}
ok "…and names the session above it" \
    {[.dlg_sid.t cget -text] eq "today one"}
destroy .dlg_sid

# ── the marker moves on a SWITCH, not at the next turn boundary ─────
#
# USER 2026-09-17: "the highlighted session doesn't seem to be the
# actually attached one … both sessions have the icon prefix but one
# also has different colour".
#
# `refresh` was the ONLY thing that ever moved the ▸, the listbox
# selection or the accent colour, and `::vchat::open_session` never
# called it. Refresh runs on turn boundaries, so between switches all
# three sat on whichever session was open at the last one — and a
# switch from the palette's "goto" (which never touches this pane)
# moved nothing at all.
#
# `mark_current` is the re-mark, and it must cost NO round trip:
# `refresh` blocks on GET /api/sessions, while a session switch is
# supposed to be one `pack` (that is the whole per-session-transcript
# design in chat.tcl).
puts "\nthe marks follow the session on screen"

set ::vapi::sessions [sessions_of [clock seconds]]
set ::vsidebar::archive_collapsed 1
set ::vsidebar::pins {}
set ::vchat::session "TODAY-1"
::vsidebar::refresh
set ti [lsearch -glob [body] "*today one*"]
set wi [lsearch -glob [body] "*three days ago*"]
ok "the open session's row is accented after a refresh" \
    {[$::vsidebar::R.list itemcget $ti -foreground] eq $::vt::T(accent)}

# THE SWITCH. No refresh — this is what open_session has to hand.
set ::vchat::session "WEEK-1"
set ::vapi::gets 0
set ::opens 0
::vsidebar::mark_current
update
ok "the ▸ moved to the session now on screen" \
    {[lsearch -glob [body] "*▸*three days ago*"] >= 0}
ok "…and left the one it was on" \
    {[lsearch -glob [body] "*▸*today one*"] < 0}
ok "…the accent moved with it" \
    {[$::vsidebar::R.list itemcget $wi -foreground] eq $::vt::T(accent)}
ok "…and is gone from the old row" \
    {[$::vsidebar::R.list itemcget $ti -foreground] ne $::vt::T(accent)}
ok "…the selection moved with it" \
    {[lindex [$::vsidebar::R.list curselection] 0] == $wi}
ok "…it cost no round trip" {$::vapi::gets == 0}
# Same pin as the refresh path: restoring a selection must never reopen
# a session, or a switch reopens what it just opened.
ok "…and it reopened nothing" {$::opens == 0}

# The rest of the row survives being re-rendered in place.
ok "the re-rendered row keeps its label and message count" \
    {[lsearch -glob [body] "*three days ago  ·  2"] >= 0}

# Idempotent: marking the session that is already marked is a no-op,
# not a second delete/insert that could drift the row indices.
::vsidebar::mark_current
ok "marking the same session twice changes nothing" \
    {[lsearch -glob [body] "*▸*three days ago*"] >= 0
     && [lindex [$::vsidebar::R.list curselection] 0] == $wi}

# A pinned row re-rendered by the re-mark keeps its ★.
set ::vsidebar::pins {TODAY-1}
set ::vchat::session "TODAY-1"
::vsidebar::refresh
set ::vchat::session "WEEK-1"
::vsidebar::mark_current
ok "a pinned row that loses the ▸ keeps its ★" \
    {[lsearch -glob [body] "*★*today one*"] >= 0
     && [lsearch -glob [body] "*▸*today one*"] < 0}
set ::vsidebar::pins {}
set ::vchat::session "TODAY-1"
::vsidebar::refresh

# ── ▶ comes from the listing, and is drawn from it the same refresh ──
#
# USER 2026-09-17: a session "is still showing as active even though
# it's actually ended a long time ago". /api/sessions carries a
# cross-process `running` per session; this pane never read it and drew
# ▶ from the window's own belief instead.
puts "\nthe busy mark is drawn from the engine's verdict"

set now [clock seconds]
set ::vapi::sessions [list \
    [dict create id "RUN-1" title "engine says working" message_count 1 \
        created_at [clock format $now -format "%Y-%m-%d %H:%M:%S"] \
        updated_at [clock format $now -format "%Y-%m-%d %H:%M:%S"] \
        running true] \
    [dict create id "IDLE-1" title "engine says idle" message_count 1 \
        created_at [clock format $now -format "%Y-%m-%d %H:%M:%S"] \
        updated_at [clock format $now -format "%Y-%m-%d %H:%M:%S"] \
        running false]]
set ::vchat::session ""
set ::vchat::live {}
::vsidebar::refresh
ok "the listing's verdict reaches the chat layer" \
    {[dict exists $::vchat::live "RUN-1"] && ![dict exists $::vchat::live "IDLE-1"]}
# The ORDER is the point: reconciled first, drawn second. A reconcile
# after the rebuild would leave every ▶ one refresh stale.
ok "…and the ▶ it implies is on the row in the SAME refresh" \
    {[lsearch -glob [body] "*▶ engine says working*"] >= 0}
ok "…while an idle session gets none" \
    {[lsearch -glob [body] "*▶ engine says idle*"] < 0}

# The engine changes its mind; one refresh is enough.
set ::vapi::sessions [lreplace $::vapi::sessions 0 0 \
    [dict create id "RUN-1" title "engine says working" message_count 1 \
        created_at [clock format $now -format "%Y-%m-%d %H:%M:%S"] \
        updated_at [clock format $now -format "%Y-%m-%d %H:%M:%S"] \
        running false]]
::vsidebar::refresh
ok "a turn the engine has let go loses its ▶ on the next refresh" \
    {[lsearch -glob [body] "*▶ engine says working*"] < 0}

# ── the child glyph marks a CHILD, and a JSON null is not a parent ───
#
# USER 2026-09-18: "All of the sessions in the session list on desktop
# are showing the child icon."
#
# `parent` is an Option<String> (desktop_api.rs), so a session with no
# parent arrives as `"parent": null` — and tcllib's ::json::json2dict
# renders JSON null as the four-character STRING "null". `$parent ne
# ""` is true for it, so all 563 sessions in the user's list wore ↳.
#
# The fixtures above could never show it: they omit the key entirely,
# which is not what the server sends. These rows are built through a
# REAL parse so the suite sees what the wire produces.
puts "\nthe child glyph marks a child, not every session"
set now [clock seconds]
set ts [clock format $now -format "%Y-%m-%d %H:%M:%S"]
set ::vapi::sessions [dict get [::json::json2dict [string map [list @TS@ $ts] {
    {"sessions": [
      {"id":"P-1","title":"the parent","message_count":1,
       "created_at":"@TS@","updated_at":"@TS@","parent":null},
      {"id":"C-1","title":"the child","message_count":1,
       "created_at":"@TS@","updated_at":"@TS@","parent":"P-1"},
      {"id":"N-1","title":null,"message_count":0,
       "created_at":"@TS@","updated_at":"@TS@","parent":null}
    ]}}]] sessions]
set ::vchat::session ""
set ::vchat::live {}
::vsidebar::refresh
ok "a session whose parent is null carries NO child glyph" \
    {[lsearch -glob [body] "*↳*the parent*"] < 0}
ok "…and a session with a real parent does" \
    {[lsearch -glob [body] "*↳*the child*"] >= 0}
ok "a null TITLE is not the word null" \
    {[lsearch -glob [body] "*null*"] < 0}

# ── the list is a TREE: a child sits under the session it forked from ─
#
# USER 2026-09-18: "it doesn't look like they're organised in a tree".
# The ↳ glyph alone left children scattered by their own last-activity
# — three subagents of one parent could land under three different
# headings with nothing saying whose they were.
#
# The rule (settled with the user): a child FOLLOWS ITS PARENT,
# indented, wherever the parent lands, across time buckets. A child
# whose parent is not in the list (filtered out, deleted) stays in its
# own bucket at depth 0 and keeps the ↳.
proc tree_sessions {} {
    set now [clock seconds]
    set today [clock format $now -format "%Y-%m-%d %H:%M:%S"]
    set old   [clock format [expr {$now - 3*86400}] -format "%Y-%m-%d %H:%M:%S"]
    return [dict get [::json::json2dict [string map [list @NOW@ $today @OLD@ $old] {
      {"sessions": [
        {"id":"T-1","title":"plain today","message_count":1,
         "created_at":"@NOW@","updated_at":"@NOW@","parent":null},
        {"id":"PAR","title":"the fork parent","message_count":5,
         "created_at":"@OLD@","updated_at":"@OLD@","parent":null},
        {"id":"KID-A","title":"kid a","message_count":1,
         "created_at":"@NOW@","updated_at":"@NOW@","parent":"PAR"},
        {"id":"KID-B","title":"kid b","message_count":1,
         "created_at":"@NOW@","updated_at":"@NOW@","parent":"PAR"},
        {"id":"GRAND","title":"grandchild","message_count":1,
         "created_at":"@NOW@","updated_at":"@NOW@","parent":"KID-A"},
        {"id":"ORPH","title":"orphan kid","message_count":1,
         "created_at":"@NOW@","updated_at":"@NOW@","parent":"GONE"}
      ]}}]] sessions]
}
puts "\nthe session list is a tree"
set ::vapi::sessions [tree_sessions]
set ::vsidebar::pins {}
set ::vchat::session ""
set ::vchat::live {}
set ::vsidebar::archive_collapsed 1
::vsidebar::refresh
set B [body]
proc rowat {pat} {
    set i [lsearch -glob [body] $pat]
    if {$i < 0} { return "" }
    return [lindex [body] $i]
}
proc rowidx {pat} { return [lsearch -glob [body] $pat] }

ok "a child is emitted directly beneath its parent" \
    {[rowidx "*kid a*"] == [rowidx "*the fork parent*"] + 1}
ok "…and its sibling directly after the child's own subtree" \
    {[rowidx "*kid b*"] > [rowidx "*kid a*"]
     && [rowidx "*kid b*"] > [rowidx "*grandchild*"]}
ok "a child follows its parent into the PARENT's bucket" \
    {[rowidx "*the fork parent*"] > [rowidx "*EARLIER THIS WEEK*"]
     && [rowidx "*kid a*"] > [rowidx "*EARLIER THIS WEEK*"]}
ok "…so it is NOT also under its own (TODAY) heading" \
    {[rowidx "*kid a*"] > [rowidx "*plain today*"]}
ok "a child is indented one level deeper than its parent" \
    {[string length [regexp -inline {^ +} [rowat "*kid a*"]]]
     > [string length [regexp -inline {^ +} [rowat "*the fork parent*"]]]}
ok "a grandchild is indented deeper again" \
    {[string length [regexp -inline {^ +} [rowat "*grandchild*"]]]
     > [string length [regexp -inline {^ +} [rowat "*kid a*"]]]}
ok "every child carries the ↳" \
    {[string match "*↳*" [rowat "*kid a*"]]
     && [string match "*↳*" [rowat "*grandchild*"]]}
ok "a top-level session carries none" \
    {![string match "*↳*" [rowat "*plain today*"]]}
ok "an orphan (parent not in the list) stays in its own bucket, marked" \
    {[rowidx "*orphan kid*"] > [rowidx "*TODAY*"]
     && [rowidx "*orphan kid*"] < [rowidx "*EARLIER THIS WEEK*"]
     && [string match "*↳*" [rowat "*orphan kid*"]]}
ok "every session appears exactly once" \
    {[llength [lsearch -all -glob [body] "*kid a*"]] == 1
     && [llength [lsearch -all -glob [body] "*the fork parent*"]] == 1}

# Re-rendering ONE row must not flatten it. `mark_current` deletes and
# re-inserts a single row from `rowmeta`, so a depth the meta does not
# carry is a depth the row loses the moment you click it.
set ::vchat::session "KID-A"
::vsidebar::mark_current
ok "re-rendering one row keeps its indent and its ↳" \
    {[string match "*↳*" [rowat "*kid a*"]]
     && [string length [regexp -inline {^ +} [rowat "*kid a*"]]]
        > [string length [regexp -inline {^ +} [rowat "*the fork parent*"]]]}
ok "…and still shows it as the active session" \
    {[string match "*▸*" [rowat "*kid a*"]]}
set ::vchat::session ""

# A cycle in the data must cost one row each, not a hung pane. The
# engine cannot produce one today (a child cannot spawn children), but
# /branch chains can, and a listing is untrusted input.
puts "\na parent cycle is not a hang"
set now [clock seconds]
set ts [clock format $now -format "%Y-%m-%d %H:%M:%S"]
set ::vapi::sessions [dict get [::json::json2dict [string map [list @TS@ $ts] {
  {"sessions": [
    {"id":"A","title":"loop a","message_count":1,
     "created_at":"@TS@","updated_at":"@TS@","parent":"B"},
    {"id":"B","title":"loop b","message_count":1,
     "created_at":"@TS@","updated_at":"@TS@","parent":"A"}
  ]}}]] sessions]
set rc [catch { ::vsidebar::refresh } cyc]
ok "refresh survived a cycle" {$rc == 0}
ok "…and emitted each session once" \
    {[llength [lsearch -all -glob [body] "*loop a*"]] == 1
     && [llength [lsearch -all -glob [body] "*loop b*"]] == 1}

# ── events refresh asynchronously, once per burst ─────────────────
#
# The 2026-10-04 hang: every turn event fetched the list synchronously
# from inside a stream handler. Events now call refresh_soon — a burst
# is one fetch, a burst during a fetch is one more after its reply, and
# nothing in the handler waits.
puts "\nrefresh_soon"
set ::vapi::async 0
set ::vapi::async_cmd ""
proc ::vapi::get_json_async {path cmd {ms 0}} {
    incr ::vapi::async
    set ::vapi::async_cmd $cmd
    set ::vapi::async_path $path
    return tok
}
proc wait_ms {ms} { after $ms {set ::waited 1} ; vwait ::waited }
for {set i 0} {$i < 5} {incr i} { ::vsidebar::refresh_soon }
ok "refresh_soon returns before any fetch" {$::vapi::async == 0}
wait_ms 500
ok "five events, one fetch" {$::vapi::async == 1}
ok "…the paged listing" {[string match "/api/sessions?limit=100*" $::vapi::async_path]}
::vsidebar::refresh_soon
wait_ms 500
ok "a refresh asked for during a fetch does not start a second" {$::vapi::async == 1}
uplevel #0 [list {*}$::vapi::async_cmd 1 [dict create sessions $::vapi::sessions total 999]]
ok "the reply paints the list" {[$::vsidebar::R.list size] > 0}
wait_ms 500
ok "…and the waiting refresh runs once it is in" {$::vapi::async == 2}

# Sessions other nodes own are asked for, and say where they live.
puts "\nanother node's sessions"
ok "the listing asks for them" {[string match "*remote=1*" [::vsidebar::_list_path]]}
set ::vapi::sessions [list \
    [dict create id VPS-1 title "#flashsupport" message_count 3 \
        updated_at [clock format [clock seconds] -format "%Y-%m-%d %H:%M:%S" -gmt 1] \
        remote true node vps-id node_name vps]]
::vsidebar::refresh
set _rows [$::vsidebar::R.list get 0 end]
ok "a remote row names its node" {[lsearch -glob $_rows "*#flashsupport*@vps*"] >= 0}

puts "\nsidebartest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
