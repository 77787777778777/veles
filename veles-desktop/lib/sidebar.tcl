# veles-desktop/lib/sidebar.tcl — sessions panel: PINNED + recents,
# filter-as-you-type, rename/delete via context actions.
#
# Pins key on the session id (M5 scope: veles ids don't rotate the way
# hermes' compression lineage does; if a pinned session gets deleted the
# pin drops with a notice). Pin state lives in the desktop state blob
# (server-durable via /api/desktop/state).

# ::http::formatQuery builds the listing's query (api.tcl loads the
# package too; the suites stub ::vapi and need it named here).
package require http

namespace eval ::vsidebar {
    variable filter ""
    variable pins {}       ;# list of session ids, display order
    variable rows {}       ;# current listbox row -> session id map
    ;# What each session's row is MADE of, so one row can be re-rendered
    ;# without refetching the list: sid -> {label count basemark}.
    variable rowmeta {}
    ;# The session currently wearing the ▸ / the accent. `mark_current`
    ;# compares against it to know which row to un-mark.
    variable marked ""
    # Widget root — .sidebar when docked, a frame inside the float
    # toplevel when undocked (rebuild model: Tk cannot reparent across
    # toplevels, so undock destroys + rebuilds the panel).
    variable R ".sidebar"
    # Archive bucket count, when collapsed (the "Archive — show N more"
    # header). Defaults to 1 = COLLAPSED (lazy load is what was asked
    # for); toggling flips it. USER: a long-lived gateway collects
    # hundreds of sessions; an uncapped list is a vertical mile of
    # scroll to reach "recent". (Must be 0/1, not "" — Tcl's `if`
    # rejects an empty string as "expected boolean value".)
    variable archive_collapsed 1
    variable archive_total {}   ;# sid-less count for the header label
}

# Rebuild the panel at a new root (undock/redock). Packs only when the
# root lives directly in a float toplevel (docked = the pane manages it).
proc ::vsidebar::rebuild {root} {
    variable R
    if {[winfo exists $R]} { destroy $R }
    set R $root
    build
    set parent [winfo parent $R]
    if {[winfo toplevel $parent] eq $parent} {
        pack $R -fill both -expand 1
    }
    refresh
}

proc ::vsidebar::load_pins {} {
    variable pins
    set pins {}
    # Unattached: no request leaves the process.
    if {[::vapi::unattached]} return
    if {[catch {::vapi::get_json /api/desktop/state} state]} return
    if {[dict exists $state pins]} {
        set pins [dict get $state pins]
        if {[catch {llength $pins}]} { set pins {} }
    }
}

# Serialize a Tcl value to JSON: nested dicts → objects, lists → arrays,
# the literal string null → JSON null (override round-trip safety),
# everything else an escaped string.
proc ::vsidebar::to_json {v} {
    if {$v eq "null"} { return "null" }
    if {![catch {dict size $v} n]} {
        set parts {}
        dict for {k val} $v {
            lappend parts "\"[::vapi::json_escape $k]\": [to_json $val]"
        }
        return "\{[join $parts ", "]\}"
    }
    if {[llength $v] > 1} {
        set parts {}
        foreach item $v { lappend parts [to_json $item] }
        return "\[[join $parts ", "]\]"
    }
    return "\"[::vapi::json_escape $v]\""
}

proc ::vsidebar::save_pins {} {
    variable pins
    # Merge with the full state blob — the server stores override:* keys
    # here too; a pins-only write would wipe them.
    if {![::vstate::put pins $pins]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {pins not saved: %s}] $::vstate::error] }
    }
}

proc ::vsidebar::toggle_pin {sid} {
    variable pins
    if {$sid eq "ARCHIVE_TOGGLE" || $sid eq ""} { return }
    set i [lsearch -exact $pins $sid]
    if {$i >= 0} {
        set pins [lreplace $pins $i $i]
    } else {
        lappend pins $sid
    }
    save_pins
    refresh
}

# "▶ " for a session with a turn in flight. Guarded: the sidebar is
# built before chat.tcl has any state, and refresh runs on startup.
proc ::vsidebar::_busy_mark {sid} {
    if {[catch { ::vchat::is_running $sid } r]} { return "" }
    return [expr {$r ? "▶ " : ""}]
}

# The label a session shows, from the last list this pane fetched.
#
# `/api/sessions` already carries every session's title and this pane
# already asks for it once a turn; nothing else needs an endpoint or a
# round trip to learn a session's name. "" when the list has never been
# fetched, or the session is not in it — callers fall back to the id.
proc ::vsidebar::label_for {sid} {
    variable labels
    if {[info exists labels] && [dict exists $labels $sid]} {
        return [dict get $labels $sid]
    }
    return ""
}

# One row's text: "  ▸★▶ label  ·  42", or "    ↳ ▸★▶ label  ·  42"
# for a child.
#
# `mark` is the row's own standing decoration (★ for pinned, "" for the
# rest); the ▸ and the ▶ are looked up live, so re-rendering a row is
# enough to move either of them.
#
# `depth` is how far down the fork tree this row sits and drives the
# INDENT; `child` is whether it is a child at all and drives the ↳.
# They are two arguments rather than one because an ORPHAN — a child
# whose parent is not in the listing — is a child at depth 0: it keeps
# the glyph that says "this was forked off something" while sitting at
# the left margin, because there is nothing above it to sit under.
#
# The indent is capped at four levels. Delegates cannot nest
# (VELES_CHILD_DEPTH), but `/branch` chains can, and a sidebar is not
# the place to render a twelve-deep staircase.
proc ::vsidebar::_row {id label count mark {depth 0} {child 0}} {
    set cur ""
    catch {
        if {$id eq $::vchat::session} { set cur "▸" }
    }
    if {$cur ne "" && $mark ne ""} {
        set mark "$cur[string trimright $mark]"
    } elseif {$cur ne ""} {
        set mark "$cur "
    }
    set pad [string repeat "  " [expr {1 + ($depth > 4 ? 4 : $depth)}]]
    set glyph [expr {$child ? "↳ " : ""}]
    return "$pad$glyph$mark[_busy_mark $id]$label  ·  $count"
}

# Insert one session's row and remember what it was made of, so
# `mark_current` can re-render it later without the list.
#
# `depth` and `child` go into rowmeta with everything else. They have
# to: `_redraw_row` rebuilds the row from this and nothing else, so a
# field the meta does not carry is a field the row loses the moment you
# click it — which for a child means it jumps back to the left margin
# and drops its ↳.
proc ::vsidebar::_insert_row {id label count mark {depth 0} {child 0}} {
    variable rows
    variable rowmeta
    $::vsidebar::R.list insert end [_row $id $label $count $mark $depth $child]
    lappend rows $id
    dict set rowmeta $id [list $label $count $mark $depth $child]
}

# Re-render ONE session's row in place, from what `refresh` recorded.
#
# delete + insert at the same index, so the row keeps its position and
# LOSES its per-item colour — which is exactly how the accent leaves the
# session that is no longer on screen.
proc ::vsidebar::_redraw_row {sid} {
    variable rows
    variable rowmeta
    if {$sid eq "" || ![dict exists $rowmeta $sid]} return
    set i [lsearch -exact $rows $sid]
    if {$i < 0} return
    lassign [dict get $rowmeta $sid] label count mark depth child
    catch {
        $::vsidebar::R.list delete $i
        $::vsidebar::R.list insert $i [_row $sid $label $count $mark $depth $child]
    }
}

# Put the selection and the accent on one session's row.
#
# NO RE-ENTRANCY GUARD, and that is a measured fact rather than an
# assumption: Tk 8.6's listbox generates <<ListboxSelect>> from user
# interaction and from an explicit `event generate`, NOT from the
# `selection` widget command. (Probed directly — `selection set` leaves
# a bound handler uncalled.) So this cannot re-enter `on_select` and
# reopen the session, which would otherwise happen on every switch and
# twice a turn forever. Anyone adding an `event generate
# <<ListboxSelect>>` here is re-opening that door; sidebartest pins it
# shut on both callers.
proc ::vsidebar::_select_row {sid} {
    variable rows
    if {$sid eq ""} return
    set i [lsearch -exact $rows $sid]
    if {$i < 0} return
    catch { $::vsidebar::R.list selection clear 0 end }
    catch { $::vsidebar::R.list selection set $i }
    catch { $::vsidebar::R.list see $i }
    # Colour it too: a listbox that has lost focus draws its selection
    # in a shade that reads as "nothing is selected".
    catch {
        $::vsidebar::R.list itemconfigure $i -foreground $::vt::T(accent)
    }
}

# The session on screen changed: move the ▸, the selection and the
# accent to it. WITHOUT refetching anything.
#
# USER 2026-09-17: "the highlighted session doesn't seem to be the
# actually attached one … both sessions have the icon prefix but one
# also has different colour". `refresh` was the only thing that moved
# any of the three, and `::vchat::open_session` never called it — it
# runs on TURN boundaries, so between switches all three sat on
# whichever session was open at the last one. A switch from the
# palette's "goto" (which never touches this pane) moved nothing at all.
#
# NOT a refresh: that blocks on GET /api/sessions, and a session switch
# is meant to cost one `pack` — re-fetching the whole list to move a
# glyph would put the round trip back into the one path chat.tcl spent
# a release taking out of it. Two rows are re-rendered instead: the one
# that HAD the mark and the one that should have it.
proc ::vsidebar::mark_current {} {
    variable marked
    set cur ""
    catch { set cur $::vchat::session }
    if {$cur eq $marked} return
    set was $marked
    set marked $cur
    _redraw_row $was
    _redraw_row $cur
    _select_row $cur
}

# Emit one bucket: its header, then its rows as a tree.
#
# COLLECT FIRST, WRITE SECOND. The header can only be written once it
# is known that the bucket has something to put under it, and with a
# tree that is no longer the same question as "is the entry list
# empty": a bucket whose every row is a child of a parent in another
# bucket emits nothing, because those rows are drawn up there instead.
#
# `headid` is the id `rows` records for the header line — "" for a
# plain heading, ARCHIVE_TOGGLE for the one that is clickable.
proc ::vsidebar::_emit_bucket {header entries mark {headid ""}} {
    variable rows
    variable tree_inline
    variable tree_seen
    set acc {}
    foreach e $entries {
        if {[dict exists $tree_inline [lindex $e 0]]} { continue }
        lassign $e id label count
        _collect_subtree $id $label $count $mark 0 acc
    }
    if {![llength $acc]} { return }
    $::vsidebar::R.list insert end $header
    lappend rows $headid
    foreach spec $acc {
        lassign $spec id label count m depth child
        _insert_row $id $label $count $m $depth $child
    }
}

# One row and everything forked off it, depth-first, in listing order,
# appended to the caller's accumulator.
#
# `tree_seen` is the cycle guard AND the once-only guard. A `parent`
# that points at its own descendant (or at itself) cannot be produced
# by the engine today — a delegate child cannot spawn children — but
# `/branch` chains can grow arbitrarily deep and a listing is untrusted
# input: without this, two rows pointing at each other recurse until Tk
# gives up, with the pane half-drawn.
proc ::vsidebar::_collect_subtree {id label count mark depth accvar} {
    upvar 1 $accvar acc
    variable tree_kids
    variable tree_seen
    variable tree_child
    variable tree_meta
    if {[dict exists $tree_seen $id]} { return }
    dict set tree_seen $id 1
    lappend acc [list $id $label $count $mark $depth \
        [expr {[dict exists $tree_child $id] ? 1 : 0}]]
    if {![dict exists $tree_kids $id]} { return }
    foreach kid [dict get $tree_kids $id] {
        if {![dict exists $tree_meta $kid]} { continue }
        lassign [dict get $tree_meta $kid] klabel kcount
        # Children never inherit the parent's ★: a pin is a property of
        # the session that was pinned.
        _collect_subtree $kid $klabel $kcount "" [expr {$depth + 1}] acc
    }
}

# The listing the pane shows: the newest page, narrowed by the filter
# on the gateway (titles, ids, first lines — every session, not just
# the newest page), plus the pinned sessions wherever they fall.
proc ::vsidebar::_list_path {} {
    variable filter
    variable pins
    # remote 1: the sessions other nodes own, read from their replicas
    # here (USER 2026-10-05: "a key audit path"). Opened read-only.
    set args [list limit 100 remote 1]
    if {[string trim $filter] ne ""} { lappend args q [string trim $filter] }
    if {[info exists pins] && [llength $pins]} { lappend args include [join $pins ,] }
    return "/api/sessions?[::http::formatQuery {*}$args]"
}

# Fetch and paint, now. For a click (⟳, a rename, a delete) and for the
# suites; anything an EVENT triggers uses refresh_soon instead.
proc ::vsidebar::refresh {} {
    $::vsidebar::R.list delete 0 end
    # Unattached: no request leaves the process; the list says so.
    if {[::vapi::unattached]} {
        $::vsidebar::R.list insert end [::vi18n::mc {not attached}]
        $::vsidebar::R.list insert end [::vi18n::mc {press Attach in the Instances tab}]
        return
    }
    if {[catch {::vapi::get_json [_list_path]} resp]} {
        ::vstatus::set_msg [::vi18n::mc {sessions: unreachable}]
        return
    }
    _paint $resp
}

# Refresh after a pause, asynchronously, at most one fetch in flight.
#
# The desktop hung for long stretches while a batch spawned sessions
# (USER 2026-10-04). Every turn start, turn end and history change
# called `refresh` from inside an event-stream handler: a synchronous
# GET, i.e. a nested `vwait`, while the gateway's store lock was
# contended by the batch's writers — and the next stream line ran
# another handler, which nested another wait inside the first. Events
# now only schedule this: bursts coalesce into one fetch, the reply is
# painted from the event loop, and nothing waits inside a handler.
proc ::vsidebar::refresh_soon {} {
    variable soon_after
    if {[info exists soon_after] && $soon_after ne ""} { return }
    set soon_after [after 400 ::vsidebar::_refresh_async]
}

proc ::vsidebar::_refresh_async {} {
    variable soon_after
    variable inflight
    variable again
    set soon_after ""
    if {![winfo exists $::vsidebar::R.list] || [::vapi::unattached]} { return }
    # A fetch whose reply never came (a re-attach drops stale replies
    # unanswered) must not block every later refresh: in flight means
    # started less than 15 s ago.
    if {[info exists inflight] && [clock seconds] - $inflight < 15} {
        set again 1
        return
    }
    set inflight [clock seconds]
    if {[catch {::vapi::get_json_async [_list_path] ::vsidebar::_refreshed} err]} {
        set inflight 0
        ::vstatus::set_msg [format [::vi18n::mc {unreachable: %s}] $err]
    }
}

proc ::vsidebar::_refreshed {ok resp} {
    variable inflight
    variable again
    set inflight 0
    if {![winfo exists $::vsidebar::R.list]} { return }
    if {$ok} {
        _paint $resp
    } else {
        ::vstatus::set_msg [::vi18n::mc {sessions: unreachable}]
    }
    if {[info exists again] && $again} {
        set again 0
        refresh_soon
    }
}

proc ::vsidebar::_paint {resp} {
    variable filter
    variable pins
    variable rows
    variable rowmeta
    variable labels
    variable archive_collapsed
    variable archive_total
    set rows {}
    set rowmeta {}
    if {![info exists labels]} { set labels {} }
    $::vsidebar::R.list delete 0 end
    set sessions [dict get $resp sessions]
    # A gateway that pages reports `total` and has already applied the
    # filter (to titles, ids AND first lines); the local filter below is
    # for an older one that sent its newest 100 whatever was asked.
    set total ""
    catch { set total [dict get $resp total] }
    # BEFORE the rows are built, so the ▶ drawn below is the engine's
    # answer and not this window's memory of one. The listing carries a
    # cross-process `running` per session (the session_runs registry);
    # until this call nothing read it, so a turn whose end this window
    # did not witness kept its mark forever and a turn running in the
    # tui or telegram showed none at all. (USER 2026-09-17: "still
    # showing as active even though it's actually ended a long time
    # ago".) Reconciling AFTER the rebuild would leave every mark one
    # refresh behind, which is why this sits here and sidebartest pins
    # the order.
    catch { ::vchat::sync_running $sessions }

    set fl [expr {$total eq "" ? [string tolower $filter] : ""}]
    # Time buckets: TODAY / THIS WEEK / ARCHIVE (collapsed). A
    # long-lived gateway accumulates hundreds of sessions and a flat
    # "recent" list is a vertical mile of scroll (USER: "the session
    # list scroll could probably also grow too long — let's divide it
    # into section: Today, Earlier this week (last 7 days) and Archive
    # (collapsed list, lazy load)").
    set now [clock seconds]
    set midnight [clock scan "00:00:00"]
    set week_start [expr {$midnight - 7 * 86400}]
    set pinned_rows {}
    set today_rows {}
    set week_rows {}
    set archive_rows {}
    # THE FORK TREE, collected while bucketing and applied after.
    #
    # `parent_of` is only filled for rows that SURVIVED the filter, so
    # "does this child have a parent to sit under" is answered about
    # what will actually be on screen — a filter that hides the parent
    # turns its children into orphans rather than into rows with no
    # visible anchor.
    set parent_of [dict create]
    set kidmeta [dict create]
    foreach s $sessions {
        set id [dict get $s id]
        set title [::vapi::field $s title]
        set count [dict get $s message_count]
        set label [expr {$title ne "" ? $title : $id}]
        # A session another node owns says where it lives: it opens
        # read-only here (lib/chat.tcl, `_sync_readonly`).
        set node ""
        catch {
            if {[string is true -strict [dict get $s remote]]} {
                set node [::vapi::field $s node_name]
            }
        }
        if {$node ne ""} { set label "$label  @$node" }
        # Remembered BEFORE the filter: the window title must follow a
        # rename even while a filter hides the row it came from. (The
        # clean label — no child glyph — is what the window title gets.)
        dict set labels $id $label
        # A subagent child is a real session in this list (USER
        # taxonomy, 2026-09-17); ↳ marks it as forked so children read
        # as children at a glance. The parent id rides in the row's
        # tooltip via rowmeta, not in the label.
        set parent [::vapi::field $s parent]
        set rowlabel [expr {$parent ne "" ? "↳ $label" : $label}]
        # Bucket by LAST ACTIVITY (updated_at), not created_at. A session
        # you resumed today belongs under Today even if you created it a
        # month ago — "recent" is a wall-clock notion about when you were
        # last in it, not when it was born (USER: "tier by last
        # activity, not by when it started").
        set active $now
        # The store's stamps are UTC; scanned as local time, every day
        # bucket was off by the zone's offset.
        catch {
            set active [clock scan [dict get $s updated_at] \
                -format "%Y-%m-%d %H:%M:%S" -gmt 1]
        }
        if {$fl ne "" && ![string match *[string map {* \\*} $fl]* [string tolower $label]] \
            && ![string match *[string map {* \\*} $fl]* [string tolower $id]]} {
            continue
        }
        # The entry carries the CLEAN label; the ↳ and the indent are
        # decided by `_row` from the row's place in the tree, which is
        # not known until every session has been seen.
        set entry [list $id $label $count]
        dict set kidmeta $id [list $label $count]
        if {$parent ne ""} { dict set parent_of $id $parent }
        if {[lsearch -exact $pins $id] >= 0} {
            lappend pinned_rows $entry
        } elseif {$active >= $midnight} {
            lappend today_rows $entry
        } elseif {$active >= $week_start} {
            lappend week_rows $entry
        } else {
            lappend archive_rows $entry
        }
    }

    # A running turn is visible from here — otherwise "it is still
    # thinking" is information the window no longer contains anywhere.
    # (Pins and Today and This Week are all expanded; Archive is the
    # one that collapses — its entries surface via the header.) A
    # BUSY archive row has to stay expanded so its ▶ is reachable;
    # simplest correct: never collapse away a busy session.
    # THE ACTIVE SESSION IS MARKED IN THE TEXT, not by the selection.
    #
    # USER: "there should be a better indicator which session is active
    # in the session list than highlight because highlight immediately
    # disappears on redraw". Exactly so — this proc's caller does
    # `delete 0 end` and re-inserts every row, and refresh runs on every
    # turn start and every turn end, so the highlight was gone seconds
    # after you clicked. A marker made of characters survives a rebuild
    # by construction, which is the whole point.
    #
    # ▸ for the session on screen, ★ for pinned, both when both.
    # (The proc itself lives below — it is called from `mark_current`
    # too, which re-renders ONE row without refetching the list.)

    # THE TREE, built once now that every surviving row is known.
    #
    # USER 2026-09-18: "it doesn't look like they're organised in a
    # tree". The ↳ alone left a fork's children scattered across three
    # headings by their own last-activity, with nothing saying whose
    # they were. A child now FOLLOWS ITS PARENT, indented, wherever the
    # parent lands — settled with the user against the alternative of
    # keeping strict per-bucket recency.
    #
    # `kids` maps a parent to its children in listing order; `inline`
    # is the set of rows that will be emitted under a parent and must
    # therefore be SKIPPED where their own bucket would have put them.
    # A child whose parent did not survive the filter is in neither: it
    # stays where it is, at depth 0, and keeps the ↳ (see `_row`).
    variable tree_kids
    variable tree_inline
    variable tree_meta
    variable tree_seen
    variable tree_child
    set tree_kids [dict create]
    set tree_inline [dict create]
    set tree_meta $kidmeta
    set tree_child [dict create]
    dict for {kid par} $parent_of {
        dict set tree_child $kid 1
        if {![dict exists $kidmeta $par]} { continue }
        dict lappend tree_kids $par $kid
        dict set tree_inline $kid 1
    }
    # REACHABILITY, before anything is drawn.
    #
    # A row is only "placed under a parent" if some top-level row
    # actually reaches it. A parent CYCLE — a↔b, which the engine
    # cannot produce today but a listing is untrusted input — makes
    # every member inline and none of them reachable from any root, so
    # without this they would be skipped in their own bucket and drawn
    # in no other: the sessions would simply vanish from the list. The
    # members of an unreachable knot are promoted back to top level.
    #
    # Keys are collected before the unset: modifying a dict while
    # `dict for` walks it is undefined in Tcl.
    set reach [dict create]
    set stack {}
    foreach id [dict keys $tree_meta] {
        if {![dict exists $tree_inline $id]} { lappend stack $id }
    }
    while {[llength $stack]} {
        set id [lindex $stack end]
        set stack [lrange $stack 0 end-1]
        if {[dict exists $reach $id]} { continue }
        dict set reach $id 1
        if {[dict exists $tree_kids $id]} {
            foreach k [dict get $tree_kids $id] { lappend stack $k }
        }
    }
    foreach id [dict keys $tree_inline] {
        if {![dict exists $reach $id]} { dict unset tree_inline $id }
    }
    # `tree_seen` spans the WHOLE refresh, not one bucket: it is what
    # makes a parent cycle cost one row each instead of hanging the
    # pane, and it is what stops the collapsed-archive busy scan below
    # from emitting a row the tree has already placed.
    set tree_seen [dict create]
    _emit_bucket "— PINNED —" $pinned_rows "★ "
    _emit_bucket "— TODAY —" $today_rows ""
    _emit_bucket "— EARLIER THIS WEEK —" $week_rows ""
    # Archive: a header row that expands/collapses. Expanded shows the
    # rows (lazy-loaded here — they were never INSERTED until now);
    # collapsed parks them behind one header so the list stays short.
    set archive_total [llength $archive_rows]
    set busy_archive {}
    set quiet_archive {}
    foreach e $archive_rows {
        lassign $e id label count
        if {[catch { ::vchat::is_running $id } r] == 0 && $r} {
            lappend busy_archive $e
        } else {
            lappend quiet_archive $e
        }
    }
    if {[info exists archive_collapsed] && $archive_collapsed} {
        # Collapsed: only busy archive rows stay individually visible
        # (they are LIVE turns, so hiding them loses information).
        #
        # FLAT, at depth 0, tree or no tree. The promise is "a live
        # turn is never hidden", and an indent under a parent that the
        # collapse did not draw would be an indent under nothing. The
        # `tree_seen` check is what stops a busy archived CHILD whose
        # parent lives in an expanded bucket from being drawn twice —
        # the tree already placed it up there.
        if {[llength $quiet_archive] || [llength $busy_archive]} {
            set n [expr {[llength $quiet_archive] + [llength $busy_archive]}]
            $::vsidebar::R.list insert end "— ARCHIVE ($n) ▸ show —"
            lappend rows "ARCHIVE_TOGGLE"
            foreach e $busy_archive {
                lassign $e id label count
                if {[dict exists $tree_seen $id]} { continue }
                dict set tree_seen $id 1
                _insert_row $id $label $count "" 0 \
                    [expr {[dict exists $tree_child $id] ? 1 : 0}]
            }
        }
    } else {
        _emit_bucket "— ARCHIVE ($archive_total) ▾ hide —" $archive_rows "" \
            "ARCHIVE_TOGGLE"
    }
    if {![$::vsidebar::R.list size]} {
        # Wrapped since 2026-09-25: a listbox insert is outside the
        # unwrapped-literal scan, so this stayed English in every locale.
        set ::vsidebar::empty_row "  [::vi18n::mc {(no sessions)}]"
        $::vsidebar::R.list insert end $::vsidebar::empty_row
        lappend rows ""
    }
    # Put the selection back on the session that is open. The glyph
    # above is what actually survives a rebuild; this is so the row still
    # reads as selected to the keyboard and to a screen reader.
    # (`_select_row` carries the no-re-entrancy note — it is the same
    # rule for the same reason, now on two callers.)
    variable marked
    set cur ""
    catch { set cur $::vchat::session }
    set marked $cur
    _select_row $cur
    # A session's title is written ASYNCHRONOUSLY by the engine — a
    # provisional from the first line of the prompt, replaced moments
    # later by a summarised one — and this pane already refreshes on
    # every turn start and end. So this is where the window learns its
    # name, rather than at open time when there often is not one yet.
    catch { ::vchat::retitle_window }
    # The real count: "100 session(s)" was the page size, whatever the
    # store held.
    if {$total ne "" && $total > [llength $sessions]} {
        ::vstatus::set_msg [format [::vi18n::mc {%1$s of %2$s sessions}] \
            [llength $sessions] $total]
    } else {
        ::vstatus::set_msg [format [::vi18n::mc {%s session(s)}] [llength $sessions]]
    }
}

# Toggle the archive section (the header row). Re-running refresh
# rebuilds the list with the other branch.
proc ::vsidebar::toggle_archive {} {
    variable archive_collapsed
    set archive_collapsed [expr {[info exists archive_collapsed] && $archive_collapsed ? 0 : 1}]
    refresh
}

proc ::vsidebar::selected_sid {} {
    variable rows
    set sel [$::vsidebar::R.list curselection]
    if {$sel eq ""} { return "" }
    set row [lindex $rows $sel]
    if {$row eq "" || $row eq {}} { return "" }
    return $row
}

proc ::vsidebar::on_select {} {
    set sid [selected_sid]
    if {$sid eq ""} return
    if {$sid eq "ARCHIVE_TOGGLE"} {
        ::vsidebar::toggle_archive
        return
    }
    ::vchat::open_session $sid
}

proc ::vsidebar::detach_clear {} {
    variable rows
    set rows {}
    catch { $::vsidebar::R.list delete 0 end }
    catch {
        $::vsidebar::R.list insert end [::vi18n::mc {not attached}]
        $::vsidebar::R.list insert end [::vi18n::mc {press Attach in the Instances tab}]
    }
}

proc ::vsidebar::new_session {} {
    if {[catch {::vapi::post /api/sessions/new ""} data]} {
        ::vstatus::set_msg [format [::vi18n::mc {new session failed: %s}] $data]
        return
    }
    set sid [string trim [dict get [::json::json2dict $data] session_id]]
    refresh
    ::vchat::open_session $sid
}

proc ::vsidebar::delete_selected {} {
    set sid [selected_sid]
    if {$sid eq "" || $sid eq "ARCHIVE_TOGGLE"} return
    if {[tk_messageBox -title [::vi18n::mc {Delete session}] -icon question -type yesno \
        -message [format [::vi18n::mc {Delete session %s and all its messages?}] $sid]] ne "yes"} return
    if {[catch {::vapi::delete "/api/sessions/$sid"} e]} {
        ::vstatus::set_msg [format [::vi18n::mc {delete failed: %s}] $e]
        return
    }
    ::vchat::clear_if_current $sid
    refresh
}

# The session id, on the clipboard.
#
# USER: "need session id right click option to view/copy quickly". There
# was no way to get one out of this pane at all — the rows show the
# TITLE, and an untitled session shows an id you can read but not
# select. ::vctx selects the row under the pointer first, so both of
# these act on the row you right-clicked.
proc ::vsidebar::copy_sid {} {
    set sid [selected_sid]
    if {$sid eq "" || $sid eq "ARCHIVE_TOGGLE"} return
    clipboard clear
    clipboard append $sid
    # A clipboard write that says nothing is indistinguishable from one
    # that failed (panes.tcl's copy-path idiom).
    ::vstatus::set_msg [format [::vi18n::mc {copied: %s}] $sid]
}

# The id in a selectable field, for when the status line is not enough —
# it scrolls away, and you often want to paste the id somewhere the
# clipboard has already been used for something else.
proc ::vsidebar::show_sid {} {
    set sid [selected_sid]
    if {$sid eq "" || $sid eq "ARCHIVE_TOGGLE"} return
    set w .dlg_sid
    if {[winfo exists $w]} { destroy $w }
    toplevel $w
    wm title $w [::vi18n::mc {Session id}]
    set label [label_for $sid]
    if {$label eq "" || $label eq $sid} { set label [::vi18n::mc {(untitled)}] }
    ttk::label $w.t -text $label
    # A read-only ENTRY, not a label: the point is to select and copy it.
    ttk::entry $w.e -width 34
    $w.e insert 0 $sid
    $w.e selection range 0 end
    ttk::button $w.copy -text [::vi18n::mc {Copy}] \
        -command [list ::vsidebar::_copy_text $sid]
    ttk::button $w.close -text [::vi18n::mc {Close}] -command [list destroy $w]
    pack $w.t -fill x -padx 8 -pady {8 0}
    pack $w.e -fill x -padx 8 -pady 4
    pack $w.close $w.copy -side right -padx 8 -pady {4 8}
    bind $w <Escape> [list destroy $w]
    focus $w.e
}

proc ::vsidebar::_copy_text {t} {
    clipboard clear
    clipboard append $t
    ::vstatus::set_msg [format [::vi18n::mc {copied: %s}] $t]
}

# Export the row you right-clicked — the menu used to run /export, which
# exports the session on screen.
proc ::vsidebar::export_selected {} {
    set sid [selected_sid]
    if {$sid eq "" || $sid eq "ARCHIVE_TOGGLE"} return
    if {[catch {::vchat::export_session $sid markdown} r]} {
        ::vstatus::set_msg [::vi18n::mc {export failed: %s} $r]
    } else {
        ::vstatus::set_msg [format [::vi18n::mc {exported to %s}] $r]
    }
}

proc ::vsidebar::rename_selected {} {
    set sid [selected_sid]
    if {$sid eq "" || $sid eq "ARCHIVE_TOGGLE"} return
    set w .dlg_rename
    if {[winfo exists $w]} { destroy $w }
    toplevel $w
    wm title $w [::vi18n::mc {Rename session}]
    ttk::label $w.l
    ::vi18n::label $w.l {New title for %s:} $sid
    ttk::entry $w.e
    proc ::vsidebar::_do_rename {w sid} {
        set t [$w.e get]
        set body "\{\"title\": \"[::vapi::json_escape $t]\"\}"
        if {[catch {::vapi::post "/api/sessions/$sid/title" $body} e]} {
            ::vstatus::set_msg [format [::vi18n::mc {rename failed: %s}] $e]
        }
        destroy $w
        ::vsidebar::refresh
    }
    ttk::button $w.ok -text [::vi18n::mc {Rename}] -command [list ::vsidebar::_do_rename $w $sid]
    ttk::button $w.cancel -text [::vi18n::mc {Cancel}] -command [list destroy $w]
    pack $w.l -fill x -padx 8 -pady {8 0}
    pack $w.e -fill x -padx 8 -pady 4
    pack $w.ok $w.cancel -side right -padx 8 -pady {4 8}
    bind $w.e <Return> [list $w.ok invoke]
    focus $w.e
}

proc ::vsidebar::build {} {
    ttk::frame $::vsidebar::R
    ttk::entry $::vsidebar::R.filter -textvariable ::vsidebar::filter
    listbox $::vsidebar::R.list -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(select_bg) -selectforeground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -bd 0 -selectmode single -exportselection 0
    ttk::frame $::vsidebar::R.btns
    ttk::button $::vsidebar::R.btns.new -text [::vi18n::mc {＋ New}] -command ::vsidebar::new_session
    ttk::button $::vsidebar::R.btns.pin -text "★" -width 3 -command {::vsidebar::toggle_pin [::vsidebar::selected_sid]}
    ::va11y::name $::vsidebar::R.btns.pin [::vi18n::mc {pin or unpin the selected session}]
    ttk::button $::vsidebar::R.btns.rename -text "✎" -width 3 -command ::vsidebar::rename_selected
    ::va11y::name $::vsidebar::R.btns.rename [::vi18n::mc {rename the selected session}]
    ttk::button $::vsidebar::R.btns.del -text "✕" -width 3 -command ::vsidebar::delete_selected
    ::va11y::name $::vsidebar::R.btns.del [::vi18n::mc {delete the selected session}]
    # Right-click a session row. ::vctx selects the row under the
    # pointer first, so these act on what you clicked rather than on
    # whatever happened to be selected — which is how "delete" deletes
    # the wrong session.
    ::vctx::attach $::vsidebar::R.list {
        {"Open"        {::vchat::open_session [::vsidebar::selected_sid]}}
        {-}
        {"Copy session id" {::vsidebar::copy_sid}}
        {"Session id…"     {::vsidebar::show_sid}}
        {-}
        {"Rename…"     {::vsidebar::rename_selected}}
        {"Pin / unpin" {::vsidebar::toggle_pin [::vsidebar::selected_sid]}}
        {"Export…"     {::vsidebar::export_selected}}
        {-}
        {"Delete"      {::vsidebar::delete_selected}}
        {-}
        {"New session" {::vsidebar::new_session}}
    }
    ttk::button $::vsidebar::R.btns.reload -text "⟳" -width 3 -command ::vsidebar::refresh
    ::va11y::name $::vsidebar::R.btns.reload [::vi18n::mc {refresh the session list}]
    # Clicking any of these teaches the key that does the same thing.
    # ::vcoach::attach reads each widget's own -command, so nothing here
    # names a chord — the registry already knows which of these have one
    # (new = Ctrl-N/M-n, delete = Ctrl-D/M-d) and which do not.
    foreach _b {new pin rename del reload} {
        catch { ::vcoach::attach $::vsidebar::R.btns.$_b }
    }
    # Explicit tab cycle for the sidebar toolbar (a11y).
    ::va11y::tab_order [list \
        $::vsidebar::R.filter \
        $::vsidebar::R.btns.new \
        $::vsidebar::R.btns.pin \
        $::vsidebar::R.btns.rename \
        $::vsidebar::R.btns.del \
        $::vsidebar::R.btns.reload]
    pack $::vsidebar::R.btns.new -side left -padx {2 4}
    pack $::vsidebar::R.btns.pin $::vsidebar::R.btns.rename $::vsidebar::R.btns.del $::vsidebar::R.btns.reload -side left -padx 1

    pack $::vsidebar::R.filter -fill x -padx 4 -pady {4 2}
    pack $::vsidebar::R.list -fill both -expand 1 -padx 4 -pady 2
    pack $::vsidebar::R.btns -fill x -padx 4 -pady {2 4}
    # $::vsidebar::R itself is managed by the .mid panedwindow (::vdock::dock),
    # NOT packed here — the pane owns the geometry (resizable + undockable).

    # The filter now asks the gateway (every session, not the newest
    # page), so a keystroke schedules a fetch rather than making one.
    bind $::vsidebar::R.filter <KeyRelease> {::vsidebar::refresh_soon}
    bind $::vsidebar::R.list <<ListboxSelect>> ::vsidebar::on_select
    bind $::vsidebar::R.list <Double-Button-1> ::vsidebar::on_select
    # helm flow: typing filters the session list; ↓ enters it; Enter on
    # the filter opens the first match; Esc clears the filter.
    bind $::vsidebar::R.filter <Down> [list focus $::vsidebar::R.list]
    bind $::vsidebar::R.filter <Return> {
        if {[$::vsidebar::R.list size] > 0} {
            $::vsidebar::R.list selection clear 0 end
            $::vsidebar::R.list selection set 0
            ::vsidebar::on_select
        }
    }
    bind $::vsidebar::R.filter <Escape> {
        $::vsidebar::R.filter delete 0 end
        ::vsidebar::refresh
    }
    bind $::vsidebar::R.list <Return> ::vsidebar::on_select
    bind $::vsidebar::R.list <Up> {
        if {[lindex [%W curselection] 0] == 0} { focus [winfo parent %W].filter }
    }
}

# The empty row is listbox CONTENT, which the language swap's re-text pass
# cannot see; re-render it on a language change while it is the only row.
namespace eval ::vsidebar { variable empty_row "" }
proc ::vsidebar::_relocalize_empty {} {
    variable empty_row
    set l $::vsidebar::R.list
    if {![winfo exists $l] || [$l size] != 1 || [$l get 0] ne $empty_row} { return }
    set empty_row "  [::vi18n::mc {(no sessions)}]"
    $l delete 0
    $l insert end $empty_row
}
if {[llength [info commands ::vi18n::on_locale_change]]} {
    ::vi18n::on_locale_change ::vsidebar::_relocalize_empty
}
