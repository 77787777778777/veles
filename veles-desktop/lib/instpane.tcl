# veles-desktop/lib/instpane.tcl — the Instances tab: a virt-manager
# for bot instances.
#
# PLAN: PLAN-instances-pane.md (Parts 2-5). Under the app-wide-attach
# model the Instances view is the default surface when the app boots
# without an instance to connect to, stays reachable as a tab while
# attached, and its Attach button IS the switch mechanism — there is no
# second, separate "switch instance" path.
#
# Three regions, mirroring virt-manager and the IRC tab precedent:
#
#   .instlist    the tree: groups → instances, with a state dot
#   .instdetail  the selected instance's live detail
#   .instctl     the controls strip
#
# THE RULES THAT KEEP THIS HONEST
#
#   - The pane polls its own targets regardless of attach state, always
#     through ::vapi::with_instance per row. It never assumes the
#     attached instance is any particular row.
#   - Polling stops entirely when the tab is not current.
#   - Detail results carry a selection generation; a response that
#     comes back after the selection moved is dropped, not painted.
#     Health is a per-ROW fact and is never selection-gated.
#   - The dot has four states, because a wrong token is a different
#     problem from a dead host and must look different:
#     ● green reachable · ● red reached-and-failing ·
#     ● amber reachable but unauthorized (401/403) · ○ grey never
#     reached. Health is unauthenticated and can never produce amber;
#     only the detail poll's /api/status can.
#
# SOURCING ORDER: after instances.tcl, api.tcl and attach.tcl.

namespace eval ::vinstpane {
    variable R_list   ".instlist"
    variable R_detail ".instdetail"
    variable R_ctl    ".instctl"

    variable fleet_nodes {}
    variable fleet_sync {}
    # /api/fleet's alerts (recent health changes), version spread and
    # fleet-wide forgotten nodes (lib/fleetobs.tcl renders them).
    variable fleet_alerts {}
    variable fleet_versions {}
    variable fleet_forgotten {}
    # /api/fleet `consistency`: the last deployment-consistency verdict.
    variable fleet_consistency {}
    variable fleet_generation 0
    # The registry group's tree id. Saved groups are `g:<name>`, and a
    # saved group literally named `cluster-registry` collided with the
    # old fixed id and stopped the list rendering (FLEET-13); a control
    # character cannot come from the editor.
    variable registry_group "g:\x01cluster-registry"
    # The health alerts group's id, and alert row id -> node id.
    variable alerts_group "g:\x01health-alerts"
    variable alert_node
    array set alert_node {}
    variable selected ""            ;# instance name, or ""
    variable health                  ;# name -> up|down|unauth|never
    array set health {}
    variable meta                   ;# name -> /api/status dict
    array set meta {}
    variable detail                 ;# name -> dict key -> LIST OF LINES
    array set detail {}
    variable cron_stats             ;# name -> {jobs N paused M …}
    array set cron_stats {}         ;# the STRUCTURED cron facts, so
                                    ;# button_states never has to read
                                    ;# a translated display string
    variable trigger_stats          ;# name -> {triggers N …} — same
    array set trigger_stats {}      ;# principle for the /api/triggers
                                    ;# detail row
    variable inflight               ;# guard key -> 1 while out
    array set inflight {}
    variable list_after ""
    variable detail_after ""
    variable poll_list_ms   15000
    variable poll_detail_ms 5000
    variable gen 0                  ;# selection generation
    variable ssh_after ""
    variable ssh_tries 0
    variable _ssh_out
    array set _ssh_out {}
    variable _dlg_rc ""
    # The control strip's actions and its current rows, per strip root
    # (docked and floated roots are separate strips).
    variable ctl_flows [dict create]
    # Validator regexps, named so the tests can reach them. A unit name
    # and an ssh host are not free text (see the plan's argument-safety
    # section); a leading `-` in host becomes an ssh OPTION, which is
    # the same class as the IRC-nick exec bug (safeexec.tcl).
    variable unit_re {^[A-Za-z0-9@._-]+$}
    variable host_re {^[A-Za-z0-9@._:-]+$}
}

# ── selection generation ────────────────────────────────────────────
#
# ::vapi::get's synchronous path runs a nested event loop (http::wait),
# so timers, clicks and therefore SELECTION CHANGES can fire while a
# detail poll is mid-flight. Everything that paints the detail pane
# captures the generation when it starts and drops itself if it moved.
# This is the pane-local half of the attach-epoch story in attach.tcl.

proc ::vinstpane::_bump_gen {} {
    variable gen
    incr gen
    return $gen
}

proc ::vinstpane::_fresh {g} {
    variable gen
    return [expr {$g == $gen}]
}

# ── humanisers ──────────────────────────────────────────────────────

proc ::vinstpane::_uptime {s} {
    if {![string is double -strict $s]} { return "" }
    set s [expr {int($s)}]
    if {$s < 0} { return "" }
    if {$s < 60}   { return "${s}s" }
    if {$s < 3600} { return "[expr {$s / 60}]m [expr {$s % 60}]s" }
    if {$s < 86400} {
        return "[expr {$s / 3600}]h [expr {($s % 3600) / 60}]m"
    }
    return "[expr {$s / 86400}]d [expr {($s % 86400) / 3600}]h"
}

# 1234 -> "1.2k", like the status bar's _hn.
proc ::vinstpane::_hn {n} {
    if {![string is integer -strict $n]} { return $n }
    if {$n >= 1000000} { return [format %.1fM [expr {$n / 1000000.0}]] }
    if {$n >= 1000}    { return [format %.1fk [expr {$n / 1000.0}]] }
    return $n
}

# The host part of a URL — delegated to the registry's own helper
# (single implementation; ::vinst::groups needs it too).
proc ::vinstpane::_host_of {url} {
    return [::vinst::host_of $url]
}

# "in 4h 12m" from an RFC3339 UTC timestamp, or "".
proc ::vinstpane::_in {iso} {
    if {$iso eq "" || $iso eq "null"} { return "" }
    set s [string trimright $iso Z]
    if {[catch { clock scan $s -format {%Y-%m-%dT%H:%M:%S} -timezone :UTC } t]} {
        return ""
    }
    set d [expr {$t - [clock seconds]}]
    if {$d < 0} { return [::vi18n::mc {overdue}] }
    return [format [::vi18n::mc {in %s}] [_uptime $d]]
}

proc ::vinstpane::_is_loopback_url {url} {
    if {![regexp {^https?://([^/:]+)} $url -> h]} { return 0 }
    if {$h eq "localhost"} { return 1 }
    return [string match {127.*} $h]
}

# ── the tree ────────────────────────────────────────────────────────

proc ::vinstpane::build_list {{root ""}} {
    variable R_list
    if {$root ne ""} { set R_list $root }
    if {[winfo exists $R_list]} { destroy $R_list }
    ttk::frame $R_list
    # A treeview, not ::vlist: the rows are GROUPED (a listbox has no
    # non-selectable heading rows) and colour-coded per row, which a
    # plain listbox foreground cannot do. Keyboard handling stays
    # Tk-native (Up/Down walk rows, including group headings, Enter
    # attaches) — consistent with the IRC buffer tree this mirrors.
    ttk::treeview $R_list.list -columns {version uptime load platforms} \
        -show {tree headings} -selectmode browse
    $R_list.list heading #0        -text [::vi18n::mc {instance}]
    $R_list.list heading version   -text [::vi18n::mc {version}]
    $R_list.list heading uptime    -text [::vi18n::mc {uptime}]
    $R_list.list heading load      -text [::vi18n::mc {load}]
    $R_list.list heading platforms -text [::vi18n::mc {platforms}]
    $R_list.list column #0        -width 160 -stretch 1
    $R_list.list column version   -width 70  -stretch 0 -anchor w
    $R_list.list column uptime    -width 100 -stretch 0 -anchor w
    $R_list.list column load      -width 50  -stretch 0 -anchor w
    $R_list.list column platforms -width 160 -stretch 0 -anchor w
    $R_list.list tag configure up     -foreground $::vt::T(green)
    $R_list.list tag configure down   -foreground $::vt::T(red)
    $R_list.list tag configure unauth -foreground $::vt::T(gold)
    $R_list.list tag configure never  -foreground $::vt::T(fg_dim)
    $R_list.list tag configure group  -foreground $::vt::T(fg_dim)
    ttk::scrollbar $R_list.sb -orient vertical -command [list $R_list.list yview]
    $R_list.list configure -yscrollcommand [list $R_list.sb set]
    pack $R_list.sb -side right -fill y
    pack $R_list.list -fill both -expand 1
    bind $R_list.list <<TreeviewSelect>> { ::vinstpane::_pick %W }
    bind $R_list.list <Configure> { ::vinstpane::_fit_columns %W %w }
    ::vctx::attach $R_list.list {
        {"Attach"  {::vinstpane::attach_selected}}
        {-}
        {"Add…"    {::vinstpane::add_entry}}
        {"Edit…"   {::vinstpane::edit_entry}}
        {"Remove"  {::vinstpane::remove_entry}}
    }
    catch { ::va11y::name $R_list.list [::vi18n::mc {instances}] }
    # Entering the tab maps the list (so does floating it into its own
    # window): what it shows is asked again at once. The polls run only
    # while the tab is current, so the list was empty for up to 15 s on
    # a first visit, and on a return it showed the snapshot from when
    # the tab was left: a node down for minutes, green and "fresh"
    # (SMOKE-6).
    bind $R_list <Map> { ::vinstpane::_shown }
    # Pack only on the FLOAT path (parent = the float toplevel). At boot
    # the parent is the main window, and this guard used to fire there
    # too — the instances regions came up packed into the root beside
    # the chat tab's layout until a tab switch absorbed them.
    set parent [winfo parent $R_list]
    if {$parent ne "." && [winfo toplevel $parent] eq $parent} { pack $R_list -fill both -expand 1 }
    return $R_list
}

# One row per instance under a per-host group heading. The attached
# instance is marked — a client talking to a gateway with no visible
# indication of WHICH is how you act on the wrong machine.
#
# The rows are brought to what they should be IN PLACE (_sync_rows), not
# deleted and inserted again. The rebuild re-inserted every group closed
# on every 15 s refresh: the cluster's nodes never showed by themselves,
# and a group someone opened closed again under them (SMOKE-5). A row
# that stays keeps what the person did with it — open or closed,
# selected, where the list was scrolled — and a new group arrives open,
# so every node is on screen without a click.
proc ::vinstpane::refresh_list {} {
    variable R_list
    variable health
    variable meta
    variable selected
    if {![winfo exists $R_list.list]} { return }
    set attached ""
    if {[llength [info commands ::vapi::attached]]} {
        set attached [::vapi::attached]
    }
    set want {}
    foreach {g names} [::vinst::groups] {
        # The tree draws the open/closed indicator itself; a ▾ in the
        # label stayed "open" on a closed group.
        lappend want [list {} "g:$g" $g {} group]
        foreach n $names {
            set dot "○"
            set tag never
            if {[info exists health($n)]} {
                switch -- $health($n) {
                    up     { set dot "●" ; set tag up }
                    down   { set dot "●" ; set tag down }
                    unauth { set dot "●" ; set tag unauth }
                    never  { set dot "○" ; set tag never }
                }
            }
            set ver "" ; set up "" ; set plats ""
            if {[info exists meta($n)]} {
                catch { set ver   [dict get $meta($n) version] }
                catch { set up    [_uptime [dict get $meta($n) uptime_s]] }
                catch { set plats [join [dict get $meta($n) platforms] { }] }
            }
            set mark [expr {$n eq $attached ? " ⚡" : ""}]
            lappend want [list "g:$g" $n "$dot $n$mark" [list $ver $up "" $plats] $tag] ;# i18n-exempt: glyph + node name
        }
    }
    # The gateway's cluster registry is automatic; manually configured
    # SSH instances remain above it. `cluster:` rows have no SSH target.
    variable fleet_nodes
    variable fleet_alerts
    variable registry_group
    if {[llength $fleet_nodes]} {
        set group $registry_group
        lappend want [list {} $group [regsub {^▾\s*} [::vi18n::mc {▾ Cluster registry}] {}] {} group]
        foreach node $fleet_nodes {
            set id "cluster:[dict get $node id]"
            # Health, not just heartbeat age: a node whose sync pushes
            # or last probe fail is red, a stale one grey (FLEET-6).
            set state [_node_state $node]
            switch -- $state {
                stale   { set dot "○" ; set tag never }
                failing { set dot "●" ; set tag down }
                default { set dot "●" ; set tag up }
            }
            set name [dict get $node name]
            # Load has its own column: it sat under "uptime" (SMOKE-21).
            set load [dict get $node load1]
            if {$load eq "null" || $load eq ""} { set load "?" }
            # A node bound to a saved instance is the SAME machine as that
            # row: say so, rather than showing it twice as unrelated.
            set gateway [_cluster_gateway $node]
            set how [expr {$gateway eq "" ? [::vi18n::mc {registered}]
                : [format [::vi18n::mc {managed via %s}] $gateway]}]
            lappend want [list $group $id "$dot $name" \
                [list [dict get $node version] "" $load $how] $tag] ;# i18n-exempt: registry name, no UI prose
        }
    }
    # The gateway's node health alerts, newest first, one row each (0.2.2:
    # they reached only the home chat). Selecting one selects its node,
    # whose detail lists them; lib/fleetmanage.tcl also shows a new one
    # in the window on any tab.
    variable alerts_group
    variable alert_node
    array unset alert_node
    set recent [::vapi::field $fleet_alerts recent {}]
    if {[llength $recent]} {
        lappend want [list {} $alerts_group [::vi18n::mc {Health alerts}] {} group]
        foreach a [lrange $recent 0 9] {
            set id "alert:[::vapi::field $a at ?]:[::vapi::field $a node ?]"
            set alert_node($id) [::vapi::field $a node ""]
            lappend want [list $alerts_group $id \
                "⚠ [::vapi::field $a name ?]: [::vapi::field $a from ?] → [::vapi::field $a to ?]" \
                [list "" "" "" [::vapi::field $a at ?]] [expr {[::vapi::field $a to ""] eq "fresh" ? "up" : "down"}]] ;# i18n-exempt: node name and states
        }
    }
    _sync_rows $R_list.list $want
    # The selection survives in place; it is re-asserted only when it is
    # not already the widget's (each `selection set` queues a
    # <<TreeviewSelect>>, see _select).
    if {$selected ne "" && [$R_list.list exists $selected]
        && [$R_list.list selection] ne [list $selected]} {
        $R_list.list selection set $selected
        $R_list.list focus $selected
    }
}

# Make a two-level treeview hold exactly `want` — {parent id text values
# tags} rows, parents before their children — moving, updating, adding
# and deleting rows, never rebuilding. Open state, selection, focus and
# the scroll position belong to rows that stay, so they stay too.
proc ::vinstpane::_sync_rows {t want} {
    set keep [dict create]
    set next [dict create]
    foreach r $want {
        lassign $r parent id text values tags
        dict set keep $id 1
        set i [expr {[dict exists $next $parent] ? [dict get $next $parent] : 0}]
        if {[$t exists $id]} {
            if {[$t parent $id] ne $parent || [$t index $id] != $i} {
                $t move $id $parent $i
            }
            $t item $id -text $text -values $values -tags $tags
        } else {
            $t insert $parent $i -id $id -text $text -values $values \
                -tags $tags -open 1
        }
        dict set next $parent [incr i]
    }
    foreach top [$t children {}] {
        foreach c [$t children $top] {
            if {![dict exists $keep $c]} { $t delete $c }
        }
        if {![dict exists $keep $top]} { $t delete $top }
    }
}

# The columns a list this wide can show beside a readable name: from the
# right, platforms, load and uptime give way first. On a 360 dp phone the
# fixed columns took all the width and the name column showed two
# letters of each node (the detail pane has all of it).
proc ::vinstpane::_fit_columns {t width} {
    if {![winfo exists $t]} { return }
    set used 160
    set show {}
    foreach c {version uptime load platforms} {
        incr used [$t column $c -width]
        if {$used > $width} { break }
        lappend show $c
    }
    if {[llength $show] == 4} { set show #all }
    if {[$t cget -displaycolumns] ne $show} { $t configure -displaycolumns $show }
}

proc ::vinstpane::_pick {w} {
    variable selected
    set sel [$w selection]
    if {![llength $sel]} { return }
    set id [lindex $sel 0]
    # Group headings are not selectable; a stray selection of one is a
    # click that missed.
    if {[string match {g:*} $id]} {
        catch { $w selection remove $id }
        return
    }
    # An alert row stands for its node: select the node, whose detail
    # lists its alerts.
    if {[string match {alert:*} $id]} {
        variable alert_node
        set node [expr {[info exists alert_node($id)] ? $alert_node($id) : ""}]
        if {$node ne "" && [$w exists "cluster:$node"]} {
            _select "cluster:$node"
        } else {
            catch { $w selection remove $id }
        }
        return
    }
    # The same re-entrancy guard ::virc::_pick carries, for the same
    # reason: refresh_list re-asserts the selection, ttk fires
    # <<TreeviewSelect>> from `selection set` regardless, and a poll's
    # nested event loop dispatches it back into _select. The
    # inflight(d:$n) guard bounds that recursion here rather than
    # breaking it — and bounding is not breaking.
    if {$id eq $selected} { return }
    _select $id
}

proc ::vinstpane::_select {name} {
    variable selected
    variable R_list
    # The pane is CLEARED BEFORE the first poll returns. Rendering a
    # stale value from the previously selected instance is the
    # "previous gateway's data labelled as the new one's" failure in
    # miniature.
    _bump_gen
    set selected $name
    # AND THE WIDGET IS TOLD, FIRST. `selected` and the treeview's own
    # selection are two copies of one fact, and a programmatic _select
    # used to update only the variable. Then:
    #
    #   refresh_list re-asserts `selection set $selected` -> ttk queues
    #   <<TreeviewSelect>> -> the next synchronous poll's nested event
    #   loop dispatches it -> _pick reads the WIDGET, which still holds
    #   the PREVIOUS row -> _select(previous) re-enters, repaints the
    #   previous instance, and the outer poll then bails on a stale
    #   generation.
    #
    # Result, reproduced live against two real gateways: selecting
    # `wrongtoken` painted `demo`'s name, status, usage, route and logs.
    # That is precisely the failure this whole plan was written to
    # prevent — the previous gateway's data on screen labelled as the
    # new one's — and no stub-driven test could see it, because it needs
    # a real widget AND a real nested event loop. Syncing the widget
    # here makes _pick's `$id eq $selected` guard swallow the stale
    # event instead of acting on it.
    if {[winfo exists $R_list.list] && [$R_list.list exists $name]} {
        catch { $R_list.list selection set $name }
        catch { $R_list.list see $name }
    }
    _clear_detail
    _paint_empty_detail
    _enable_buttons
    poll_detail_once
}

proc ::vinstpane::selected {} {
    variable selected
    return $selected
}

# ── the detail pane ─────────────────────────────────────────────────

proc ::vinstpane::build_detail {{root ""}} {
    variable R_detail
    if {$root ne ""} { set R_detail $root }
    if {[winfo exists $R_detail]} { destroy $R_detail }
    ttk::frame $R_detail
    text $R_detail.body -wrap word -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -font {monospace 10} \
        -yscrollcommand [list $R_detail.sb set] -state disabled
    ttk::scrollbar $R_detail.sb -orient vertical -command [list $R_detail.body yview]
    pack $R_detail.sb -side right -fill y
    pack $R_detail.body -fill both -expand 1
    _detail_tags $R_detail.body
    catch { ::va11y::name $R_detail.body [::vi18n::mc {instance detail}] }
    # Float-path pack only — see build_list.
    set parent [winfo parent $R_detail]
    if {$parent ne "." && [winfo toplevel $parent] eq $parent} { pack $R_detail -fill both -expand 1 }
    return $R_detail
}

proc ::vinstpane::_detail_tags {w} {
    catch {
        $w tag configure head -foreground $::vt::T(accent) \
            -font {TkDefaultFont 12 bold}
        $w tag configure dim  -foreground $::vt::T(fg_dim)
        $w tag configure ok   -foreground $::vt::T(green)
        $w tag configure warn -foreground $::vt::T(gold)
        $w tag configure bad  -foreground $::vt::T(red)
        $w tag configure hr   -foreground $::vt::T(hairline)
    }
}

proc ::vinstpane::_clear_detail {} {
    variable R_detail
    if {![winfo exists $R_detail.body]} { return }
    $R_detail.body configure -state normal
    $R_detail.body delete 1.0 end
    $R_detail.body configure -state disabled
}

# Where the reader is in the detail, kept across a repaint of the SAME
# row: the polls repaint it every 5 s, and each repaint put the view
# back at the top under someone reading the sync lines further down. A
# new selection starts at the top (_select clears the pane first).
proc ::vinstpane::_detail_top {} {
    variable R_detail
    if {![winfo exists $R_detail.body]} { return 0 }
    return [lindex [$R_detail.body yview] 0]
}
proc ::vinstpane::_detail_moveto {top} {
    variable R_detail
    if {[winfo exists $R_detail.body]} { $R_detail.body yview moveto $top }
}

proc ::vinstpane::_detail_write {text {tag ""}} {
    variable R_detail
    if {![winfo exists $R_detail.body]} { return }
    $R_detail.body configure -state normal
    set at [$R_detail.body index end-1c]
    $R_detail.body insert end "$text\n"
    if {$tag ne ""} { catch { $R_detail.body tag add $tag $at end-1c } }
    $R_detail.body configure -state disabled
}

# What the pane shows between the selection change and the first poll.
proc ::vinstpane::_paint_empty_detail {} {
    variable selected
    if {$selected eq ""} { return }
    set url ""
    catch { set url [dict get [::vinst::get $selected] url] }
    _detail_write ""
    _detail_write $selected head
    if {$url ne ""} { _detail_write $url dim }
    _detail_write "────────────────────────────────" hr
    _detail_write [::vi18n::mc {waiting for first poll…}] dim
}

# Render the detail from cached per-instance data. Each field degrades
# independently: a gateway that answers /api/status but 404s /api/cron
# shows the status and prints "unavailable" for cron rather than
# blanking the pane.
proc ::vinstpane::paint_detail {} {
    variable selected
    variable health
    variable meta
    variable detail
    if {$selected eq ""} { return }
    set n $selected
    set top [_detail_top]
    _clear_detail
    set url ""
    catch { set url [dict get [::vinst::get $n] url] }
    _detail_write ""
    _detail_write $n head
    if {$url ne ""} { _detail_write $url dim }
    _detail_write "────────────────────────────────" hr

    set st [expr {[info exists health($n)] ? $health($n) : "never"}]
    switch -- $st {
        up {
            _detail_write [::vi18n::mc {state       reachable}] ok
        }
        down {
            _detail_write [::vi18n::mc {state       unreachable}] bad
        }
        unauth {
            _detail_write [::vi18n::mc {state       reachable but unauthorized (401/403) — check the token}] warn
        }
        default {
            _detail_write [::vi18n::mc {state       never reached}] dim
        }
    }

    if {[info exists meta($n)]} {
        set m $meta($n)
        set ver "" ; set up "" ; set plats "" ; set sess ""
        catch { set ver   [dict get $m version] }
        catch { set up    [_uptime [dict get $m uptime_s]] }
        catch { set plats [join [dict get $m platforms] {, }] }
        catch { set sess  [dict get $m session_count] }
        if {$ver ne "" || $up ne ""} {
            _detail_write [format [::vi18n::mc {gateway     v%1$s · up %2$s}] $ver $up]
        }
        if {$plats ne ""} {
            _detail_write [format [::vi18n::mc {platforms   %s}] $plats]
        }
        if {$sess ne ""} {
            _detail_write [format [::vi18n::mc {sessions    %s}] $sess]
        }
    }

    # THE RENDERER CONTRACT: every _fmt_* returns a LIST OF LINES, and
    # this loop prints the label once, then indents the rest under it.
    #
    # Written down because the first version treated the value as a
    # list unconditionally while four of the five renderers returned a
    # STRING — so `usage 1.2M in / 340k out / 8431 calls` came out as
    # eight rows reading "usage 1.2M", "usage in", "usage /". A field
    # that renders one line must return a ONE-ELEMENT LIST, not a bare
    # string; llength on a bare string is a word count.
    if {[info exists detail($n)]} {
        set pad [string repeat " " 12]
        dict for {k lines} $detail($n) {
            set label $k
            if {$k eq "logs"} { set label [::vi18n::mc {log (last 20)}] }
            if {![llength $lines]} {
                _detail_write [format {%-11s %s} $label [::vi18n::mc {unavailable}]]
                continue
            }
            _detail_write [format {%-11s %s} $label [lindex $lines 0]]
            foreach line [lrange $lines 1 end] {
                _detail_write "$pad$line"
            }
        }
    }

    # TLS honesty: the supported remote transport is a tunnel; the pane
    # must not imply http:// to a real host is fine.
    if {$url ne "" && [string match {http://*} $url] && ![_is_loopback_url $url]} {
        _detail_write ""
        _detail_write [::vi18n::mc {plaintext http to a non-loopback host — use a tunnel (HANDOFF §16)}] warn
    }
    _detail_moveto $top
}

# ── field renderers: endpoint -> LIST OF LINES ──────────────────────
#
# EVERY renderer returns a list, even when it is one line long, and the
# empty list means "the gateway could not answer this" — paint_detail
# turns that into `unavailable` and keeps going. A renderer that
# returns a bare string gets word-split by the caller, which is how the
# whole detail pane once rendered as one row per word.

proc ::vinstpane::_fmt_usage {u} {
    set pt 0 ; set ct 0 ; set calls 0
    catch { set pt    [dict get $u prompt_tokens] }
    catch { set ct    [dict get $u completion_tokens] }
    catch { set calls [dict get $u calls] }
    return [list [format [::vi18n::mc {%1$s in / %2$s out / %3$s calls}] \
        [_hn $pt] [_hn $ct] $calls]]
}

proc ::vinstpane::_fmt_route {c} {
    set p "" ; set m ""
    catch { set p [dict get $c default_provider] }
    catch { set m [dict get $c default_model] }
    set out ""
    if {$p ne "" || $m ne ""} { set out "$p:$m" }
    catch {
        set fbs [dict get $c fallback_providers]
        if {[llength $fbs]} {
            set f [lindex $fbs 0]
            set fp "" ; set fm ""
            catch { set fp [dict get $f provider] }
            catch { set fm [dict get $f model] }
            append out [format [::vi18n::mc {  (fallback: %1$s:%2$s)}] $fp $fm]
        }
    }
    if {$out eq ""} { return [list [::vi18n::mc {not configured}]] }
    return [list $out]
}

proc ::vinstpane::_fmt_plugins {p} {
    set rows ""
    catch { set rows [dict get $p plugins] }
    # An EMPTY list is an answer, not a missing one. These renderers
    # used to return {} for both, and paint_detail's {} means "the
    # gateway could not answer" — so a perfectly healthy gateway with
    # no plugins and no cron jobs read as `plugins unavailable · cron
    # unavailable`, i.e. broken. Seen on the live demo gateway.
    if {![llength $rows]} { return [list [::vi18n::mc {none loaded}]] }
    set on 0
    foreach r $rows {
        set e 0
        catch { set e [dict get $r enabled] }
        if {$e} { incr on }
    }
    return [list [format [::vi18n::mc {%1$d loaded (%2$d enabled)}] \
        [llength $rows] $on]]
}

# The STRUCTURED facts about a gateway's cron list: how many jobs, how
# many paused, and which enabled one runs next.
#
# Separate from _fmt_cron on purpose. button_states needs the counts to
# decide whether Pause/Resume are enabled, and the first version got
# them by REGEXPING THE RENDERED LINE — which is a ::vi18n::mc string,
# so under any translation the regexp missed and both buttons were
# permanently disabled with the reason "no cron jobs" next to a list of
# cron jobs. Never derive state from display text.
#
# Jobs come back newest-created first; the SOONEST next run is the one
# an operator cares about, so scan all of them and compare raw ISO
# stamps (string compare on RFC3339 UTC == chronological compare).
proc ::vinstpane::cron_counts {j} {
    set jobs ""
    catch { set jobs [dict get $j jobs] }
    set paused 0
    set next_name "" ; set next_at ""
    foreach job $jobs {
        set en 1
        catch { set en [dict get $job enabled] }
        if {!$en} { incr paused }
        set nr ""
        catch { set nr [dict get $job next_run_at] }
        if {$en && $nr ne "" && $nr ne "null"
            && ($next_at eq "" || $nr < $next_at)} {
            set next_at $nr
            set next_name ""
            catch { set next_name [dict get $job name] }
        }
    }
    return [dict create jobs [llength $jobs] paused $paused \
        next_name $next_name next_at $next_at]
}

proc ::vinstpane::_fmt_cron {j} {
    set c [cron_counts $j]
    if {[dict get $c jobs] == 0} { return [list [::vi18n::mc {no jobs}]] }
    set line [format [::vi18n::mc {%1$d jobs (%2$d paused)}] \
        [dict get $c jobs] [dict get $c paused]]
    if {[dict get $c next_name] ne ""} {
        append line [format [::vi18n::mc { · next: %1$s %2$s}] \
            [dict get $c next_name] [_in [dict get $c next_at]]]
    }
    return [list $line]
}

# The structured trigger facts for the detail row + button states.
# Separate from _fmt_triggers for the same reason cron_counts is
# separate from _fmt_cron: never derive state from display text.
proc ::vinstpane::trigger_counts {t} {
    set rows ""
    catch { set rows [dict get $t triggers] }
    set enabled 0 ; set disabled 0
    foreach tr $rows {
        set en 1
        catch { set en [dict get $tr enabled] }
        if {$en} { incr enabled } else { incr disabled }
    }
    return [dict create triggers [llength $rows] \
        enabled $enabled disabled $disabled]
}

proc ::vinstpane::_fmt_triggers {t} {
    set c [trigger_counts $t]
    if {[dict get $c triggers] == 0} { return [list [::vi18n::mc {no triggers}]] }
    return [list [format [::vi18n::mc {%1$d triggers (%2$d disabled)}] \
        [dict get $c triggers] [dict get $c disabled]]]
}

# Already a list of lines — the one renderer the old contract fitted.
proc ::vinstpane::_fmt_logs {l} {
    set lines ""
    catch { set lines [dict get $l logs] }
    if {![llength $lines]} { return [list [::vi18n::mc {(empty)}]] }
    # /api/logs answers records: one line each, as the Logs… viewer
    # prints them. The pane printed the raw dict (SMOKE-21).
    set out {}
    foreach e $lines {
        if {[catch { dict get $e msg } msg]} { lappend out $e ; continue }
        set head {}
        foreach k {time level target} {
            if {![catch { dict get $e $k } v] && $v ne "" && $v ne "null"} { lappend head $v }
        }
        lappend out [expr {[llength $head] ? "[join $head { }]: $msg" : $msg}]
    }
    return $out
}

# The attached gateway's registry, not the selected instance's URL:
# cluster-only rows have no manually configured target to route through.
#
# `http::geturl` raises SYNCHRONOUSLY when the attached gateway's host
# name does not resolve (VPN or tailnet down): "couldn't open socket".
# Uncaught, that unwound _poll_list_tick before it re-armed its timer and
# the whole Instances poll froze until restart (FLEET-2). A failure here
# is an empty registry, reported like any other failed fetch.
proc ::vinstpane::refresh_fleet {} {
    variable fleet_generation
    set generation [incr fleet_generation]
    if {[catch {
        ::vapi::get_json_async /api/fleet \
            [list ::vinstpane::_fleet_loaded $generation]
    } err]} {
        _fleet_loaded $generation 0 $err
    }
}

proc ::vinstpane::_fleet_loaded {generation ok response} {
    variable fleet_generation
    variable fleet_nodes
    variable fleet_sync
    variable fleet_alerts
    variable fleet_versions
    variable fleet_forgotten
    variable fleet_consistency
    if {$generation != $fleet_generation} { return }
    if {!$ok || [catch {dict get $response nodes} nodes]} {
        set fleet_nodes {}
        set fleet_sync {}
        set fleet_alerts {}
        set fleet_versions {}
        set fleet_forgotten {}
        set fleet_consistency {}
    } else {
        set fleet_nodes $nodes
        set fleet_sync [::vapi::field $response sync]
        set fleet_alerts [::vapi::field $response alerts]
        set fleet_versions [::vapi::field $response versions]
        set fleet_forgotten [::vapi::field $response forgotten {}]
        set fleet_consistency [::vapi::field $response consistency {}]
    }
    refresh_list
    # A registry row's controls follow its node (stale → Forget, a new
    # sync kind → Sync now), so they are recomputed with the data.
    # Its detail is painted from this same answer: repainted now, it
    # never says "fresh" beside a row that just turned stale.
    if {[_is_cluster [selected]]} {
        _enable_buttons
        _paint_cluster_detail [selected]
    }
}

# ── polling ─────────────────────────────────────────────────────────
#
# List poll: GET /api/health for every row, every 15s — unauthenticated
# and cheap, drives the dot only. Detail poll: six endpoints for the
# SELECTED row, every 5s. Both short-circuit when the tab is not
# current; both keep an inflight guard so a black-holed host cannot
# stack one pending request per tick forever. Explicit short timeouts
# (2s list, 5s detail) so a dead host does not wedge the timer.

proc ::vinstpane::poll_start {} {
    variable list_after
    variable detail_after
    # Idempotent: boot calls this once, but a hotpatch reload re-runs
    # boot, and two armed timers would poll twice per tick forever.
    if {$list_after ne ""}   { catch { after cancel $list_after } }
    if {$detail_after ne ""} { catch { after cancel $detail_after } }
    _poll_list_tick
    _poll_detail_tick
}

proc ::vinstpane::_tab_current {} {
    if {![llength [info commands ::vtabs::current]]} { return 1 }
    return [expr {[::vtabs::current] eq "instances"}]
}

proc ::vinstpane::_poll_list_tick {} {
    variable list_after
    variable poll_list_ms
    # Re-arm FIRST: nothing a tick does may be able to stop the next one.
    set list_after [after $poll_list_ms ::vinstpane::_poll_list_tick]
    if {[_tab_current]} {
        refresh_fleet
        foreach n [::vinst::names] { _health_one $n }
    }
}

# The list was mapped: one refresh now, outside the poll timers (their
# cadence and their single chain stay as they are). From a timer, not
# from inside the <Map>: the tab switch that maps the list lays out and
# paints first, and a burst of Map events asks once.
proc ::vinstpane::_shown {} {
    after cancel ::vinstpane::_refresh_now
    after 0 ::vinstpane::_refresh_now
}

proc ::vinstpane::_refresh_now {} {
    if {![_tab_current]} { return }
    refresh_fleet
    foreach n [::vinst::names] { _health_one $n }
    poll_detail_once
}

proc ::vinstpane::_poll_detail_tick {} {
    variable detail_after
    variable poll_detail_ms
    if {[_tab_current]} {
        poll_detail_once
    }
    set detail_after [after $poll_detail_ms ::vinstpane::_poll_detail_tick]
}

proc ::vinstpane::_health_one {n} {
    variable inflight
    variable health
    if {[info exists inflight(h:$n)]} { return }
    set spec [::vinst::get $n]
    if {$spec eq ""} { return }
    set url ""
    catch { set url [dict get $spec url] }
    if {$url eq ""} { return }
    set inflight(h:$n) 1
    set was [expr {[info exists health($n)] ? $health($n) : "never"}]
    if {![info exists health($n)]} { set health($n) never }
    if {[catch {
        # Health is the one unauthenticated route; a `get` here says
        # nothing about the token, only about the process being up.
        ::vapi::with_instance $n { ::vapi::get /api/health 2000 }
    } err]} {
        # A FAILED health poll means unreachable — health is
        # unauthenticated, so no token problem can cause this. `unauth`
        # degrades to `down` here: amber means REACHABLE-but-unauthorized
        # (found by driving a real gateway and killing it; the first
        # version preserved amber forever and a dead host stayed amber).
        set health($n) down
    } else {
        # A SUCCESSFUL health poll must not clobber `unauth`: the host
        # is alive and the token is wrong, which is the more
        # informative state; only a successful DETAIL poll clears it.
        if {$was ne "unauth"} { set health($n) up }
    }
    unset -nocomplain inflight(h:$n)
    refresh_list
    if {$n eq [selected]} { _enable_buttons }
}

proc ::vinstpane::_is_cluster {name} {
    return [string match {cluster:*} $name]
}

proc ::vinstpane::_cluster_node {name} {
    variable fleet_nodes
    set id [string range $name 8 end]
    foreach node $fleet_nodes {
        if {[dict get $node id] eq $id} { return $node }
    }
    return ""
}

proc ::vinstpane::_cluster_gateway {node} {
    set id [dict get $node id]
    foreach instance [::vinst::names] {
        set spec [::vinst::get $instance]
        if {[dict exists $spec node_id] && [dict get $spec node_id] eq $id} {
            return $instance
        }
    }
    return ""
}

# ── the command center's registry-row verbs ────────────────────────
#
# Every sync kind the gateway runs — store, memory, config, vault —
# has a status in the detail pane and an explicit push/apply here, all
# through /api/fleet/sync (crates/veles-frontends/src/fleet_ops.rs). The
# registry row supplies the node ID only; the gateway dials its own
# [a2a_agents] entry. Operations that change data (a config or vault
# offer either way, a memory merge here) are confirmed first, and the
# gateway refuses them unconfirmed anyway (FLEET-5).

# The node's health from /api/fleet (`health.state`: fresh|stale|
# failing|self), falling back to the heartbeat flag for a gateway that
# predates it.
proc ::vinstpane::_node_state {node} {
    set state ""
    catch { set state [dict get $node health state] }
    if {$state eq "" || $state eq "null"} {
        set state [expr {[::vapi::field $node stale 0] ? "stale" : "fresh"}]
    }
    return $state
}

# The sync kinds this node is a configured peer for, in display order.
proc ::vinstpane::_node_sync_kinds {node} {
    set kinds [::vapi::field $node sync_kinds {}]
    if {![llength $kinds] && [::vapi::field $node sync_peer] ne ""} {
        # A gateway before per-kind status: store sync only.
        set kinds store
    }
    set out {}
    foreach k {store memory config vault} {
        if {$k in $kinds} { lappend out $k }
    }
    return $out
}

proc ::vinstpane::_kind_label {kind} {
    switch -- $kind {
        store  { return [::vi18n::mc {Store}] }
        memory { return [::vi18n::mc {Memory}] }
        config { return [::vi18n::mc {Config}] }
        vault  { return [::vi18n::mc {Vault}] }
    }
    return $kind
}

# Mirrors fleet_ops::needs_confirmation. A store push lands in the
# peer's inbox and a store apply writes replicas only; a memory push is
# merged by the peer's own rules. Everything else changes data.
proc ::vinstpane::_needs_confirm {kind operation} {
    if {$kind eq "store"} { return 0 }
    if {$kind eq "memory" && $operation eq "push"} { return 0 }
    return 1
}

# The one confirmation prompt, so the suite can answer it.
proc ::vinstpane::_confirm {message detail} {
    return [expr {[tk_messageBox -type yesno -icon warning \
        -title [::vi18n::mc {Instances}] -message $message -detail $detail] eq "yes"}]
}

# One POST per kind. `confirmed` is 1 only after _confirm said yes, and
# only the kinds that need it carry `confirm: true`.
proc ::vinstpane::_sync_request {operation kinds node_id confirmed} {
    variable selected
    set sent 0
    foreach kind $kinds {
        set body [dict create kind $kind operation $operation]
        if {$operation eq "push"} { dict set body node $node_id }
        if {$confirmed && [_needs_confirm $kind $operation]} {
            dict set body confirm [::vapi::json_bool 1]
        }
        # post_json_async raises synchronously on a name that does not
        # resolve, exactly like the GET in refresh_fleet.
        if {[catch {
            ::vapi::post_json_async /api/fleet/sync $body \
                [list ::vinstpane::_sync_failed $selected $kind] \
                [list ::vinstpane::_sync_done $selected $kind]
        } err]} {
            _sync_failed $selected $kind $err
            continue
        }
        incr sent
    }
    return $sent
}

# Sync now / Apply received: pick the kinds. Not modal — OK runs
# _sync_dialog_ok — so a slow gateway never holds the event loop.
proc ::vinstpane::_sync_dialog {operation} {
    variable selected
    variable _sync_pick
    variable _sync_op
    variable fleet_sync
    set node [_cluster_node $selected]
    if {$node eq ""} { return "" }
    if {$operation eq "push"} {
        set kinds [_node_sync_kinds $node]
    } else {
        set kinds {store memory config vault}
    }
    if {![llength $kinds]} {
        ::vstatus::set_msg [::vi18n::mc {This node is not a configured sync peer for any kind.}]
        return ""
    }
    set inbox {}
    catch { set inbox [dict get $fleet_sync inbox] }
    set dlg .fleetsync
    if {[winfo exists $dlg]} { destroy $dlg }
    toplevel $dlg
    catch { wm transient $dlg . }
    if {$operation eq "push"} {
        wm title $dlg [::vi18n::mc {Sync now}]
        set head [format [::vi18n::mc {Push to %s:}] [dict get $node name]]
    } else {
        wm title $dlg [::vi18n::mc {Apply received}]
        set head [::vi18n::mc {Apply what this node received:}]
    }
    ttk::label $dlg.head -text $head
    pack $dlg.head -anchor w -padx 8 -pady 6
    array unset _sync_pick
    foreach kind $kinds {
        set _sync_pick($kind) [expr {![_needs_confirm $kind $operation]}]
        set text [_kind_label $kind]
        if {$operation eq "apply"} {
            set waiting [::vapi::field $inbox $kind 0]
            # Nothing waiting is nothing to preselect (the store drain
            # was checked with 0 waiting, SMOKE-21). A kind that changes
            # data stays opt-in however much waits.
            if {![string is integer -strict $waiting] || $waiting <= 0} { set _sync_pick($kind) 0 }
            set text [format [::vi18n::mc {%1$s (%2$s waiting)}] $text $waiting]
        }
        if {[_needs_confirm $kind $operation]} {
            set text [format [::vi18n::mc {%s — changes data, asks to confirm}] $text]
        }
        ttk::checkbutton $dlg.k$kind -text $text -variable ::vinstpane::_sync_pick($kind)
        pack $dlg.k$kind -anchor w -padx 16 -pady 2
    }
    ttk::frame $dlg.btns
    ttk::button $dlg.btns.ok -text [::vi18n::mc {OK}] -command ::vinstpane::_sync_dialog_ok
    ttk::button $dlg.btns.cancel -text [::vi18n::mc {Cancel}] -command [list destroy $dlg]
    pack $dlg.btns.ok $dlg.btns.cancel -side left -padx 6
    pack $dlg.btns -pady 8
    bind $dlg <Escape> [list destroy $dlg]
    set _sync_op [list $operation [dict get $node id]]
    return $dlg
}

proc ::vinstpane::_sync_dialog_ok {} {
    variable _sync_pick
    variable _sync_op
    lassign $_sync_op operation node_id
    set kinds {}
    foreach k {store memory config vault} {
        if {[info exists _sync_pick($k)] && $_sync_pick($k)} { lappend kinds $k }
    }
    catch { destroy .fleetsync }
    if {![llength $kinds]} {
        ::vstatus::set_msg [::vi18n::mc {Nothing selected.}]
        return 0
    }
    set risky {}
    foreach k $kinds {
        if {[_needs_confirm $k $operation]} { lappend risky [_kind_label $k] }
    }
    set confirmed 0
    if {[llength $risky]} {
        if {$operation eq "push"} {
            set message [format [::vi18n::mc {Send %s to the peer?}] [join $risky ", "]]
        } else {
            set message [format [::vi18n::mc {Merge the received %s into this node?}] [join $risky ", "]]
        }
        set detail [::vi18n::mc {A config offer can change settings and a vault offer merges secrets; changes to a security section still wait for the owner.}]
        if {![_confirm $message $detail]} {
            ::vstatus::set_msg [::vi18n::mc {Sync cancelled.}]
            return 0
        }
        set confirmed 1
    }
    set labels {}
    foreach k $kinds { lappend labels [_kind_label $k] }
    ::vstatus::set_msg [format [::vi18n::mc {sync requested: %s}] [join $labels ", "]]
    return [_sync_request $operation $kinds $node_id $confirmed]
}

proc ::vinstpane::_cluster_sync {} { return [_sync_dialog push] }
proc ::vinstpane::_cluster_apply {} { return [_sync_dialog apply] }

# `why` is post_json_async's "HTTP 409: {json}" for a refusal: show the
# engine's words, not its JSON (SMOKE-20).
proc ::vinstpane::_sync_failed {selection kind why} {
    if {$selection ne $::vinstpane::selected} { return }
    ::vstatus::set_msg [format [::vi18n::mc {%1$s sync failed: %2$s}] [_kind_label $kind] [::vapi::reason $why]]
}

proc ::vinstpane::_sync_done {selection kind response} {
    if {$selection ne $::vinstpane::selected} { return }
    set message [::vi18n::mc {Sync complete.}]
    catch { set message [dict get $response result] }
    ::vstatus::set_msg [format [::vi18n::mc {%1$s sync: %2$s}] [_kind_label $kind] $message]
    refresh_fleet
}

proc ::vinstpane::_ctl_sync {} { _cluster_sync }
proc ::vinstpane::_ctl_apply {} { _cluster_apply }

# Forget a STALE registry row (the gateway refuses this node and a node
# still heartbeating). Destructive: confirmed here and by the body.
proc ::vinstpane::_ctl_forget {} {
    variable selected
    set node [_cluster_node $selected]
    if {$node eq ""} { return 0 }
    set name [dict get $node name]
    if {![_confirm [format [::vi18n::mc {Forget node %s?}] $name] \
            [::vi18n::mc {It is removed from the cluster registry fleet-wide: the next gossip exchange tells every 0.2.0 peer to drop it too, and none of them merges it back. A peer older than 0.2.0 keeps listing it. If the machine comes back and heartbeats, it is registered again. Sync peers and saved instances are untouched.}]]} {
        return 0
    }
    set body [dict create node [dict get $node id] confirm [::vapi::json_bool 1]]
    if {[catch {
        ::vapi::post_json_async /api/fleet/forget $body \
            [list ::vinstpane::_forget_failed $name] \
            [list ::vinstpane::_forget_done $selected $name]
    } err]} {
        _forget_failed $name $err
        return 0
    }
    return 1
}

proc ::vinstpane::_forget_failed {name why} {
    ::vstatus::set_msg [format [::vi18n::mc {Forgetting %1$s failed: %2$s}] $name $why]
}

proc ::vinstpane::_forget_done {row name response} {
    variable selected
    ::vstatus::set_msg [format [::vi18n::mc {Forgot node %s.}] $name]
    if {$selected eq $row} {
        set selected ""
        _clear_detail
        _enable_buttons
    }
    refresh_fleet
}

# Is the node reachable on its configured A2A card? The gateway dials
# its own [a2a_agents] entry, never the registry endpoint.
proc ::vinstpane::_ctl_probe {} {
    variable selected
    set node [_cluster_node $selected]
    if {$node eq ""} { return 0 }
    set name [dict get $node name]
    if {[catch {
        ::vapi::post_json_async /api/fleet/probe [dict create node [dict get $node id]] \
            [list ::vinstpane::_probe_failed $name] \
            [list ::vinstpane::_probe_done $name]
    } err]} {
        _probe_failed $name $err
        return 0
    }
    ::vstatus::set_msg [format [::vi18n::mc {probing %s…}] $name]
    return 1
}

# Observation verbs (lib/fleetobs.tcl): the selected registry node's log
# tail and metrics, the fleet's drift, and a gossip exchange now.
proc ::vinstpane::_ctl_logs {} {
    variable selected
    set node [_cluster_node $selected]
    if {$node eq ""} { return 0 }
    return [expr {[::vfleetobs::open_logs [dict get $node id] [dict get $node name]] ne ""}]
}

proc ::vinstpane::_ctl_metrics {} {
    variable selected
    set node [_cluster_node $selected]
    if {$node eq ""} { return 0 }
    return [::vfleetobs::fetch_metrics [dict get $node id] [dict get $node name]]
}

proc ::vinstpane::_ctl_drift {} { return [::vfleetobs::check_drift] }
proc ::vinstpane::_ctl_gossip {} { return [::vfleetobs::gossip_now] }

proc ::vinstpane::_probe_failed {name why} {
    ::vstatus::set_msg [format [::vi18n::mc {Probe of %1$s failed: %2$s}] $name $why]
}

proc ::vinstpane::_probe_done {name response} {
    if {[::vapi::field $response reachable 0] eq "true" || [::vapi::field $response reachable 0] == 1} {
        ::vstatus::set_msg [format [::vi18n::mc {%1$s is reachable (%2$s ms).}] $name \
            [::vapi::field $response latency_ms ?]]
    } else {
        ::vstatus::set_msg [format [::vi18n::mc {%1$s is unreachable: %2$s}] $name \
            [::vapi::field $response error ?]]
    }
    refresh_fleet
}

# The node's health and per-kind sync state, as detail-pane lines.
proc ::vinstpane::_health_lines {node} {
    set lines {}
    set state [_node_state $node]
    switch -- $state {
        fresh   { set label [::vi18n::mc {fresh — heartbeating, sync passing}] }
        stale   { set label [::vi18n::mc {stale — no heartbeat within the threshold}] }
        failing { set label [::vi18n::mc {failing — heartbeating, but a sync push or the last probe failed}] }
        self    { set label [::vi18n::mc {this gateway}] }
        default { set label $state }
    }
    lappend lines "[::vi18n::mc {Health}]: $label"
    set age ""
    catch { set age [dict get $node health heartbeat_age_secs] }
    if {$age ne "" && $age ne "null"} {
        lappend lines [format [::vi18n::mc {Heartbeat age: %s s}] $age]
    }
    set probe ""
    catch { set probe [dict get $node health probe] }
    if {$probe ne "" && $probe ne "null"} {
        lappend lines [format [::vi18n::mc {Last probe: %1$s — %2$s}] \
            [::vapi::field $probe at ?] [::vapi::field $probe line]]
    }
    return $lines
}

proc ::vinstpane::_sync_lines {node} {
    variable fleet_sync
    set lines {}
    set status {}
    catch { set status [dict get $node sync_status] }
    if {$status eq "null"} { set status {} }
    foreach kind [_node_sync_kinds $node] {
        if {![dict exists $status $kind]} { continue }
        set s [dict get $status $kind]
        set mode [expr {[::vapi::field $s auto 0] eq "true" || [::vapi::field $s auto 0] == 1
            ? [::vi18n::mc {automatic}] : [::vi18n::mc {manual only}]}]
        lappend lines [format [::vi18n::mc {%1$s sync with %2$s (%3$s)}] \
            [_kind_label $kind] [::vapi::field $s peer ?] $mode]
        if {[::vapi::field $s dialable 1] in {false 0}} {
            lappend lines "  [::vi18n::mc {no [a2a_agents] entry — this peer cannot be dialed}]"
        }
        if {$kind eq "vault"} {
            if {[::vapi::field $s has_vault 0] in {false 0}} {
                lappend lines "  [::vi18n::mc {this node has no vault}]"
            } elseif {[::vapi::field $s current 0] in {true 1}} {
                lappend lines "  [::vi18n::mc {the peer holds this vault}]"
            } else {
                lappend lines "  [::vi18n::mc {the peer has not been sent this vault yet}]"
            }
        } else {
            lappend lines "  [format [::vi18n::mc {cursor %1$s of %2$s (%3$s behind)}] \
                [::vapi::field $s cursor 0] [::vapi::field $s local ?] [::vapi::field $s behind ?]]"
        }
        set last [::vapi::field $s last]
        if {$last ne ""} {
            set verdict [expr {[::vapi::field $last ok 0] in {true 1}
                ? [::vi18n::mc {ok}] : [::vi18n::mc {failed}]}]
            lappend lines "  [format [::vi18n::mc {last pass %1$s at %2$s: %3$s}] \
                $verdict [::vapi::field $last at ?] [::vapi::field $last line]]"
        }
        set backoff [::vapi::field $s backoff]
        if {$backoff ne ""} {
            lappend lines "  [format [::vi18n::mc {backing off: %1$s failure(s), next automatic try in %2$s s}] \
                [::vapi::field $backoff failures ?] [::vapi::field $backoff retry_in_secs ?]]"
        }
    }
    if {![llength $lines]} {
        lappend lines [::vi18n::mc {No configured sync peer for this node.}]
    }
    if {[::vapi::field $node self 0] in {true 1}} {
        set inbox {}
        catch { set inbox [dict get $fleet_sync inbox] }
        if {$inbox ne "" && $inbox ne "null"} {
            lappend lines [format [::vi18n::mc {Received, waiting to apply: store %1$s · memory %2$s · config %3$s · vault %4$s}] \
                [::vapi::field $inbox store 0] [::vapi::field $inbox memory 0] \
                [::vapi::field $inbox config 0] [::vapi::field $inbox vault 0]]
        }
    }
    return $lines
}

# Where this gateway's health alerts go, from /api/fleet `alerts`: off,
# or `delivery.sends` here / elsewhere / no (with its reason). A change is
# sent only with ask_relay on, a home chat named, and by the node that
# runs that chat's Telegram adapter; the header used to say "sent to the
# owner's home chat" whenever the switch was on (FLEET-8). A gateway
# before 0.2.2 reports no delivery, and nothing is claimed for it.
proc ::vinstpane::_alerts_delivery {alerts} {
    if {[::vapi::field $alerts enabled 0] ni {true 1}} {
        return [::vi18n::mc {listed here only; [notify] fleet_alerts is off}]
    }
    set delivery [::vapi::field $alerts delivery]
    switch -- [::vapi::field $delivery sends] {
        here { return [::vi18n::mc {sent to the owner's home chat from this gateway}] }
        elsewhere { return [::vi18n::mc {sent to the owner's home chat by the gateway that runs its Telegram adapter, not this one}] }
        no {
            return [format [::vi18n::mc {not sent: %s}] [::vapi::field $delivery reason ?]]
        }
    }
    return [::vi18n::mc {on; this gateway does not say whether they reach the home chat}]
}

proc ::vinstpane::_paint_cluster_detail {name} {
    variable R_detail
    set node [_cluster_node $name]
    if {$node eq "" || ![winfo exists $R_detail.body]} { return }
    set lines [list [::vi18n::mc {Automatically discovered from the attached gateway's cluster registry.}]]
    foreach {key label} {
        id {Node id} name {Name} host {Host} version {Version}
        endpoint {Endpoint} static_ip {Static IP} heartbeat_at {Heartbeat}
        uptime_class {Uptime class} sessions_served {Sessions served}
    } {
        # A field the registry holds no value for is left out: "Static
        # IP: null" told the reader nothing (SMOKE-21).
        set value [::vapi::field $node $key]
        if {$value eq "" || $value eq "null"} { continue }
        lappend lines "[::vi18n::mc $label]: $value"
    }
    set load [dict get $node load1]
    if {$load eq "null"} { set load "unknown" }
    lappend lines "[::vi18n::mc {Load}]: $load"
    lappend lines "[::vi18n::mc {Stale}]: [dict get $node stale]"
    lappend lines {*}[_health_lines $node]
    if {[dict get $node capabilities] ne "null"} {
        lappend lines "[::vi18n::mc {Capabilities}]: [dict get $node capabilities]"
    }
    if {[dict get $node replica] ne "null"} {
        lappend lines "[::vi18n::mc {Store replica}]: [dict get $node replica]"
    }
    lappend lines "" {*}[_sync_lines $node] ""
    # Observation (lib/fleetobs.tcl): metrics and drift as last asked,
    # this node's recent health alerts, the registry's version spread,
    # gossip and the nodes forgotten fleet-wide.
    if {[llength [info commands ::vfleetobs::detail_lines]]} {
        variable fleet_alerts
        variable fleet_versions
        variable fleet_forgotten
        variable fleet_sync
        lappend lines {*}[::vfleetobs::detail_lines [dict get $node id]]
        set vline [::vfleetobs::version_line $fleet_versions]
        if {$vline ne ""} { lappend lines $vline }
        # Deployment consistency as the last Drift check found it
        # (lib/fleetmanage.tcl): the fleet's verdict and what differs here.
        variable fleet_consistency
        if {[llength [info commands ::vfleetmanage::consistency_lines]]} {
            lappend lines {*}[::vfleetmanage::consistency_lines $fleet_consistency [dict get $node id]]
        }
        set is_self [expr {[::vapi::field $node self 0] in {true 1}}]
        set alerts [::vfleetobs::alert_lines $fleet_alerts [expr {$is_self ? "" : [dict get $node id]}]]
        lappend lines [format [::vi18n::mc {Health alerts (%s):}] [_alerts_delivery $fleet_alerts]]
        if {[llength $alerts]} {
            foreach a $alerts { lappend lines "  $a" }
        } else {
            lappend lines "  [::vi18n::mc {none since this gateway started}]"
        }
        if {$is_self} {
            foreach g [::vapi::field $fleet_sync gossip {}] {
                set last [::vapi::field $g last]
                set said [expr {$last eq "" ? [::vi18n::mc {no exchange yet}]
                    : "[::vapi::field $last at ?] [::vapi::field $last line]"}]
                lappend lines [format [::vi18n::mc {Gossip with %1$s: %2$s}] [::vapi::field $g peer ?] $said]
            }
            if {[llength $fleet_forgotten]} {
                set ids {}
                foreach f $fleet_forgotten { lappend ids [::vapi::field $f id ?] }
                lappend lines [format [::vi18n::mc {Forgotten fleet-wide (refused until they heartbeat again): %s}] [join $ids {, }]]
            }
        }
        lappend lines ""
    }
    set gateway [_cluster_gateway $node]
    if {$gateway ne ""} {
        lappend lines "[::vi18n::mc {Configured desktop gateway}]: $gateway"
    } else {
        lappend lines [::vi18n::mc {No explicitly configured desktop gateway is bound to this node.}]
    }
    # The ssh target is the bound saved instance's host and unit, the
    # pair Fleet deploy restarts through (FLEET-8: this line used to deny
    # one on every row, including rows that offered Fleet deploy).
    set host ""
    set unit ""
    if {$gateway ne ""} {
        set spec [::vinst::get $gateway]
        catch { set host [dict get $spec host] }
        catch { set unit [dict get $spec unit] }
    }
    if {$host ne "" && $unit ne ""} {
        lappend lines "" [format [::vi18n::mc {SSH lifecycle: through saved instance %1$s (%2$s, unit %3$s).}] \
            $gateway $host $unit]
    } else {
        lappend lines "" [::vi18n::mc {No SSH lifecycle target is configured for this node: Restart asks the node to restart itself through its supervisor; Fleet deploy needs a saved instance with an ssh target.}]
    }
    set top [_detail_top]
    _clear_detail
    $R_detail.body configure -state normal
    $R_detail.body insert end [join $lines \n]
    $R_detail.body configure -state disabled
    _detail_moveto $top
}

proc ::vinstpane::poll_detail_once {} {
    variable selected
    variable meta
    variable detail
    variable inflight
    variable health
    variable gen
    if {$selected eq ""} { return }
    if {[_is_cluster $selected]} {
        _paint_cluster_detail $selected
        return
    }
    # The not-current-tab short circuit lives HERE as well as in the
    # timer, because _select calls this directly — and _select runs at
    # boot, from ::vinstpane::boot, BEFORE ::vtabs::boot and
    # ::vapp::boot. Six synchronous 5s requests fired there froze the
    # splash for half a minute whenever the first registry row was a
    # black-holed remote, and issued them on an UNATTACHED boot, which
    # is supposed to leave the process silent.
    if {![_tab_current]} { return }
    set n $selected
    if {[info exists inflight(d:$n)]} { return }
    set inflight(d:$n) 1
    set g $gen
    set d [dict create]

    # /api/status first — it is the one that distinguishes a wrong
    # token from a dead host.
    set got_status 0
    if {[catch {
        ::vapi::with_instance $n { ::vapi::get_json /api/status 5000 }
    } m]} {
        if {[regexp {HTTP (401|403)} $m]} {
            set health($n) unauth
        }
    } else {
        set meta($n) $m
        set got_status 1
        # A successful AUTHENTICATED status poll is the strongest
        # signal there is: the host is up AND the token works. It
        # clears both `unauth` and a stale `down` (e.g. the host came
        # back between health polls).
        set health($n) up
    }

    variable cron_stats
    variable trigger_stats
    foreach {key path fmt} {
        usage   /api/usage         ::vinstpane::_fmt_usage
        route   /api/config        ::vinstpane::_fmt_route
        plugins /api/plugins       ::vinstpane::_fmt_plugins
        cron    /api/cron          ::vinstpane::_fmt_cron
        triggers /api/triggers     ::vinstpane::_fmt_triggers
        logs    /api/logs?limit=20 ::vinstpane::_fmt_logs
    } {
        if {[catch {
            ::vapi::with_instance $n [list ::vapi::get_json $path 5000]
        } raw]} {
            dict set d $key {}
            # A cron/trigger list we could not fetch is not "zero rows"
            # — drop the stale counts so the row says unreachable rather
            # than confidently reporting an empty table.
            if {$key eq "cron"}     { unset -nocomplain cron_stats($n) }
            if {$key eq "triggers"} { unset -nocomplain trigger_stats($n) }
        } else {
            dict set d $key [$fmt $raw]
            # The structured counts, alongside the rendered line. This
            # is what button_states reads; the line is for the eye only.
            if {$key eq "cron"}     { set cron_stats($n)     [cron_counts $raw] }
            if {$key eq "triggers"} { set trigger_stats($n)  [trigger_counts $raw] }
        }
        # A slow field must not hold a moved selection hostage: if the
        # selection changed while we were fetching, stop fetching —
        # nothing we paint now is wanted.
        if {![_fresh $g]} {
            unset -nocomplain inflight(d:$n)
            return
        }
    }
    set detail($n) $d
    unset -nocomplain inflight(d:$n)
    refresh_list
    if {[_fresh $g] && $n eq [selected]} {
        paint_detail
        _enable_buttons
    }
}

# ── attach / detach — the pane's verbs ──────────────────────────────
#
# The console button. There is exactly ONE mechanism: switching IS
# attaching (PLAN Part 6). Both delegate to ::vapp, which owns the
# teardown checklist; detach then selects the Instances tab, which is
# the management view the app returns to.

proc ::vinstpane::attach {name} {
    if {[_is_cluster $name]} {
        set node [_cluster_node $name]
        set gateway [expr {$node eq "" ? "" : [_cluster_gateway $node]}]
        if {$gateway eq ""} {
            ::vstatus::set_msg [::vi18n::mc {No explicitly configured gateway is bound to this node. Add the gateway first; its registry endpoint is not used as a desktop URL.}]
            return 0
        }
        set why [verify_node_binding $gateway [dict get $node id]]
        if {$why ne ""} {
            ::vstatus::set_msg $why
            return 0
        }
        set name $gateway
    }
    set rc [::vapp::attach $name]
    refresh_list
    _enable_buttons
    return $rc
}

# Is the gateway behind saved instance `instance` really registry node
# `node_id`? The association is operator-typed text, and string
# equality with it proved nothing about the machine at that URL
# (FLEET-4): a typo or a stale binding attached — and deployed — to the
# wrong host. The gateway's own /api/fleet names its `self` row; that
# must match. "" = verified, else the reason, ready for the status bar.
# The deployment executor (fleetdeploy.tcl verify_identity) asks the same
# question through its own transport, in the worker process.
proc ::vinstpane::verify_node_binding {instance node_id} {
    return [lindex [node_binding_verdict $instance $node_id] 1]
}

# The same answer with its kind: {ok ""}, {mismatch why} (the gateway is
# another node), {anonymous why} (it answers with no self row) or
# {unreachable why} (it could not be asked). A lifecycle verb needs the
# kind: a stopped gateway cannot answer, and that is not a mismatch.
proc ::vinstpane::node_binding_verdict {instance node_id} {
    if {[catch {
        ::vapi::with_instance $instance { ::vapi::get_json /api/fleet 5000 }
    } resp]} {
        return [list unreachable [format [::vi18n::mc {Cannot verify that %1$s is node %2$s: %3$s}] \
            $instance $node_id $resp]]
    }
    set self_id ""
    foreach n [::vapi::field $resp nodes {}] {
        if {[::vapi::field $n self 0] in {true 1}} {
            set self_id [::vapi::field $n id]
            break
        }
    }
    if {$self_id eq ""} {
        return [list anonymous [format [::vi18n::mc {%1$s reports no cluster node identity, so it cannot be verified as %2$s.}] \
            $instance $node_id]]
    }
    if {$self_id ne $node_id} {
        return [list mismatch [format [::vi18n::mc {%1$s is node %2$s, not %3$s. Fix its node id in Edit….}] \
            $instance $self_id $node_id]]
    }
    return [list ok ""]
}

proc ::vinstpane::detach {} {
    set rc [::vapp::detach]
    catch { ::vtabs::select instances }
    refresh_list
    _enable_buttons
    return $rc
}

# ── the controls strip ──────────────────────────────────────────────
#
# Buttons are disabled WITH A REASON in the tooltip (via ::va11y::name,
# which is this app's tooltip mechanism) rather than hidden. A greyed
# button that says "no host configured for this instance" teaches the
# operator what to add; a missing one just looks broken.

proc ::vinstpane::build_ctl {{root ""}} {
    variable R_ctl
    variable labels
    if {$root ne ""} { set R_ctl $root }
    if {[winfo exists $R_ctl]} { destroy $R_ctl }
    ttk::frame $R_ctl
    # The label is a CATALOG KEY and the button is REGISTERED through
    # ::vi18n::label, not ::vi18n::mc. Two separate bugs lived here and
    # both were invisible to every static check:
    #
    #   1. `-text $label` never went through the catalog at all, so the
    #      strip was English in all 46 locales - while `Reload plugins`
    #      and `Pause scheduler` looked translated, because those words
    #      reach a catalog key by another route. A live probe of the real
    #      window is what surfaced it: 11 buttons sitting in an English
    #      strip inside a German app.
    #   2. `mc` resolves ONCE, at creation. `label` registers the widget,
    #      so the strip re-texts on a locale swap like everything else.
    #
    # The single-word labels are why the extractor never saw them either:
    # flat_prose_cells requires whitespace, so `Attach`/`Stop`/`Add…`
    # failed its prose test and only the multi-word cells became keys.
    #
    # THE TABLE IS THE STRIP. One line per action, `|` between groups;
    # `_ctl_<id>` is its dispatcher. A new action is a line here and its
    # proc, nothing else: the strip wraps into as many rows as the window
    # needs (_flow_ctl), on every platform. It used to be one packed row
    # that needed 2227 px, so at the default 1080x720 ten of the twenty
    # actions (Start, Stop, Restart, Fleet deploy…, Add…, Edit…, Remove
    # among them) were never on screen, and seven still were not at
    # 1440 px (SMOKE-4); only Android wrapped it.
    set labels [dict create]
    set items {}
    foreach {id label} {
        attach    {Attach}
        detach    {Detach}
        |         {}
        sync      {Sync now}
        apply     {Apply received}
        gossip    {Gossip now}
        |         {}
        probe     {Probe}
        logs      {Logs…}
        metrics   {Metrics}
        drift     {Drift…}
        config    {Config…}
        forget    {Forget node…}
        |         {}
        bulkprobe {Probe all…}
        bulksync  {Sync all…}
        audit     {Audit log…}
        |         {}
        reload    {Reload plugins}
        pause     {Pause scheduler}
        resume    {Resume scheduler}
        |         {}
        start     {Start}
        stop      {Stop}
        restart   {Restart}
        deploy    {Fleet deploy…}
        |         {}
        add       {Add…}
        edit      {Edit…}
        remove    {Remove}
    } {
        if {$id eq "|"} {
            lappend items |
            continue
        }
        dict set labels $id $label
        # -width 0: the label's own width. The theme's minimum of eleven
        # characters made "Probe" as wide as "Apply received" and cost
        # the strip a row.
        ttk::button $R_ctl.$id -text [::vi18n::mc $label] -width 0 \
            -command [list ::vinstpane::_ctl_$id]
        ::vi18n::label $R_ctl.$id $label
        catch { ::vcoach::attach $R_ctl.$id }
        # A label that grows (a locale swap) may no longer fit its row.
        bindtags $R_ctl.$id [linsert [bindtags $R_ctl.$id] 0 VInstCtlKid]
        lappend items $R_ctl.$id
    }
    variable ctl_flows
    dict set ctl_flows $R_ctl [dict create items $items rows {}]
    bindtags $R_ctl [linsert [bindtags $R_ctl] 0 VInstCtl]
    bind VInstCtl <Configure> { ::vinstpane::_flow_ctl %W }
    bind VInstCtl <Destroy> { catch { dict unset ::vinstpane::ctl_flows %W } }
    bind VInstCtlKid <Configure> { ::vinstpane::_flow_ctl_soon [winfo parent %W] }
    _flow_ctl $R_ctl
    catch { ::va11y::name $R_ctl [::vi18n::mc {instance controls}] }
    # Float-path pack only — see build_list. At boot this packed the
    # strip into the root AFTER .mid, which left it a zero-height
    # cavity until the first tab switch repacked it.
    set parent [winfo parent $R_ctl]
    if {$parent ne "." && [winfo toplevel $parent] eq $parent} { pack $R_ctl -side bottom -fill x }
    return $R_ctl
}

# The strip in rows: its actions in table order, a new row whenever the
# next one would run past the strip's width, a separator where two
# groups meet inside a row (a group boundary at a row's start needs
# none). Rows are frames LOWERED beneath the buttons they hold, so the
# buttons stay visible and keep their creation order for Tab. Runs on
# every <Configure> of the strip and of its buttons; an unchanged layout
# is left alone, which is what ends the Configure it causes.
proc ::vinstpane::_flow_ctl {f} {
    variable ctl_flows
    if {![winfo exists $f] || ![dict exists $ctl_flows $f]} { return 0 }
    set width [winfo width $f]
    # Not mapped yet (the tab is not current): its window's width is
    # where it will be packed, full width.
    if {$width <= 1} { set width [winfo width [winfo toplevel $f]] }
    if {$width <= 1} { set width [winfo screenwidth $f] }
    set pad [_dp 2]
    set sepw [expr {[_dp 2] + 2 * [_dp 3]}]
    set rows {}
    set row {}
    set used 0
    set boundary 0
    foreach item [dict get $ctl_flows $f items] {
        if {$item eq "|"} { set boundary 1 ; continue }
        if {![winfo exists $item]} { continue }
        set w [expr {[winfo reqwidth $item] + 2 * $pad}]
        set gap [expr {[llength $row] && $boundary ? $sepw : 0}]
        if {[llength $row] && $used + $gap + $w > $width} {
            lappend rows $row
            set row {}
            set used 0
            set gap 0
        }
        if {$gap} { lappend row | }
        lappend row $item
        incr used [expr {$gap + $w}]
        set boundary 0
    }
    if {[llength $row]} { lappend rows $row }
    if {$rows eq [dict get $ctl_flows $f rows]} { return [llength $rows] }
    dict set ctl_flows $f rows $rows
    foreach c [winfo children $f] {
        if {[string match "$f.row*" $c] || [string match "$f.sep*" $c]} { destroy $c }
    }
    set n 0
    set s 0
    foreach r $rows {
        set rf $f.row$n
        ttk::frame $rf
        lower $rf
        pack $rf -side top -anchor w
        foreach item $r {
            if {$item eq "|"} {
                ttk::separator $f.sep[incr s] -orient vertical
                pack $f.sep$s -in $rf -side left -fill y -padx [_dp 3] -pady [_dp 3]
                continue
            }
            pack $item -in $rf -side left -padx $pad -pady $pad
        }
        incr n
    }
    return [llength $rows]
}

proc ::vinstpane::_flow_ctl_soon {f} {
    after cancel [list ::vinstpane::_flow_ctl $f]
    after idle [list ::vinstpane::_flow_ctl $f]
}

# A size in desktop pixels, in this screen's (a dp on a phone).
proc ::vinstpane::_dp {n} {
    if {[llength [info commands ::vandroid::dp]]} { return [::vandroid::dp $n] }
    return $n
}

# state -> {id reason}: "" reason = enabled. `name` is the selected
# instance ("" = none), `spec` its registry entry, `st` its health.
# Kept pure so the tests can drive it without widgets.
proc ::vinstpane::button_states {name spec st} {
    variable detail
    set reachable [expr {$st eq "up" || $st eq "unauth"}]
    set has_host 0
    catch { set has_host [expr {[dict get $spec host] ne ""}] }
    set attached 0 ; set any_attached 0
    if {[llength [info commands ::vapi::attached]]} {
        set attached    [expr {[::vapi::attached] eq $name && $name ne ""}]
        set any_attached [expr {[::vapi::attached] ne ""}]
    }
    # has jobs / has paused jobs, from the last detail fetch's STRUCTURED
    # cron counts. Emphatically not from the rendered cron line: that
    # goes through ::vi18n::mc, so a regexp over it matches in English
    # and nowhere else.
    variable cron_stats
    set has_jobs 0 ; set has_paused 0
    if {$name ne "" && [info exists cron_stats($name)]} {
        catch { set has_jobs   [expr {[dict get $cron_stats($name) jobs]   > 0}] }
        catch { set has_paused [expr {[dict get $cron_stats($name) paused] > 0}] }
    }
    set unreach [::vi18n::mc {instance unreachable}]
    set out [dict create]
    dict set out attach [expr {
        $name eq "" ? [::vi18n::mc {no instance selected}] :
        (!$reachable ? $unreach :
         ($attached ? [::vi18n::mc {already attached}] : ""))}]
    dict set out detach [expr {
        $any_attached ? "" : [::vi18n::mc {not attached}]}]
    dict set out reload [expr { !$reachable || $name eq "" ? $unreach : "" }]
    dict set out pause [expr {
        $name eq "" || !$reachable ? $unreach :
        ($has_jobs ? "" : [::vi18n::mc {no cron jobs}])}]
    dict set out resume [expr {
        $name eq "" || !$reachable ? $unreach :
        ($has_paused ? "" : [::vi18n::mc {no paused jobs}])}]
    set nohost [::vi18n::mc {no host configured for this instance}]
    dict set out start [expr {
        $name eq "" ? [::vi18n::mc {no instance selected}] :
        (!$has_host ? $nohost :
         ($st eq "up" ? [::vi18n::mc {already up}] : ""))}]
    dict set out stop [expr {
        $name eq "" ? [::vi18n::mc {no instance selected}] :
        (!$has_host ? $nohost :
         ($reachable ? "" : [::vi18n::mc {not running}]))}]
    dict set out restart [expr {
        $name eq "" ? [::vi18n::mc {no instance selected}] :
        (!$has_host ? $nohost : "")}]
    dict set out deploy [expr {$name eq "" ? [::vi18n::mc {no instance selected}] : ""}]
    dict set out add    ""
    set nosel [::vi18n::mc {no instance selected}]
    dict set out edit   [expr {$name eq "" ? $nosel : ""}]
    dict set out remove [expr {$name eq "" ? $nosel : ""}]
    return $out
}

proc ::vinstpane::_enable_buttons {} {
    variable R_ctl
    variable labels
    variable selected
    variable health
    if {![winfo exists $R_ctl]} { return }
    set name $selected
    # Fleet-wide management (lib/fleetmanage.tcl): Probe all and Sync
    # all reach every registry node the attached gateway can dial, the
    # audit log is that gateway's; neither needs a row selected.
    variable fleet_nodes
    set fleet_state [expr {[llength $fleet_nodes] ? "normal" : "disabled"}]
    foreach id {bulkprobe bulksync audit} {
        catch { $R_ctl.$id configure -state $fleet_state }
    }
    catch { $R_ctl.config configure -state disabled }
    if {[_is_cluster $name]} {
        foreach id {attach detach reload sync apply pause resume start stop restart edit remove} {
            catch { $R_ctl.$id configure -state disabled }
        }
        foreach id {forget probe logs metrics drift gossip} { catch { $R_ctl.$id configure -state disabled } }
        set node [_cluster_node $name]
        if {$node ne ""} {
            # A registry node's own gateway restarts itself through its
            # supervisor, and its config is read and written over the
            # same link (the node refuses what it cannot or may not do,
            # and says why). A stale node does not answer.
            catch { $R_ctl.config configure -state normal }
            if {[_node_state $node] ne "stale"} {
                catch { $R_ctl.restart configure -state normal }
            }
            # Any registry node can be asked for its log and metrics (the
            # gateway answers for itself and asks another node over its
            # [a2a_agents] link); drift compares the whole registry, and
            # gossip runs with the configured gossip peers.
            catch { $R_ctl.logs configure -state normal }
            catch { $R_ctl.metrics configure -state normal }
            catch { $R_ctl.drift configure -state normal }
            variable fleet_sync
            if {[llength [::vapi::field $fleet_sync gossip_peers {}]]} {
                catch { $R_ctl.gossip configure -state normal }
            }
            if {[_cluster_gateway $node] ne ""} { catch { $R_ctl.attach configure -state normal } }
            if {[llength [_node_sync_kinds $node]]} { catch { $R_ctl.sync configure -state normal } }
            if {[dict get $node self]} { catch { $R_ctl.apply configure -state normal } }
            # Forget only a stale row that is not this gateway's own (the
            # gateway refuses the rest too); probe any other node.
            set is_self [expr {[::vapi::field $node self 0] in {true 1}}]
            if {!$is_self && [_node_state $node] eq "stale"} {
                catch { $R_ctl.forget configure -state normal }
            }
            if {!$is_self} { catch { $R_ctl.probe configure -state normal } }
        }
        # Fleet deploy needs a registry node bound to a saved instance;
        # it used to keep whatever state the previous row left (FLEET-3).
        set deployable [expr {$node ne "" && [_cluster_gateway $node] ne "" &&
            (![llength [info commands ::vfleetui::available]] || [::vfleetui::available])}]
        catch { $R_ctl.deploy configure -state [expr {$deployable ? "normal" : "disabled"}] }
        catch { $R_ctl.add configure -state normal }
        return
    }
    if {$name eq ""} {
        set spec [dict create]
        set st never
    } else {
        set spec [::vinst::get $name]
        if {$spec eq ""} { set spec [dict create] }
        set st [expr {[info exists health($name)] ? $health($name) : "never"}]
    }
    set states [button_states $name $spec $st]
    foreach id {sync apply} {
        if {[winfo exists $R_ctl.$id]} { $R_ctl.$id configure -state disabled }
    }
    foreach id {forget probe logs metrics drift gossip} {
        if {[winfo exists $R_ctl.$id]} { $R_ctl.$id configure -state disabled }
    }
    dict for {id reason} $states {
        if {![winfo exists $R_ctl.$id]} { continue }
        if {$reason eq ""} {
            $R_ctl.$id configure -state normal
        } else {
            $R_ctl.$id configure -state disabled
        }
        # The disabled-reason tooltip: ::va11y::name shows on hover and
        # on keyboard focus, exactly what a greyed button needs.
        #
        # The first argument is the button's LABEL, not its id. It used to
        # pass $id — the identifier from the table above — so every tooltip
        # read "attach — no instance selected": an English verb welded to a
        # translated reason, in every locale. The id is a lookup key for
        # dispatch (`_ctl_$id`); it was never a word for a human to read.
        set lab $id
        if {[info exists labels] && [dict exists $labels $id]} {
            set lab [::vi18n::mc [dict get $labels $id]]
        }
        catch { ::vi18n::aname $R_ctl.$id {%1$s — %2$s} $lab $reason }
    }
}

# The button dispatchers. Each names the instance it acts on and
# reports through the status bar; none is a bare no-op on failure.

proc ::vinstpane::_name_or_msg {} {
    variable selected
    if {$selected ne ""} { return $selected }
    ::vstatus::set_msg [::vi18n::mc {no instance selected}]
    return ""
}

proc ::vinstpane::attach_selected {} {
    set n [_name_or_msg]
    if {$n eq ""} { return 0 }
    return [attach $n]
}

# The strip's dispatchers. attach/detach/add/edit/remove route to the
# public verbs; the API verbs live below.
proc ::vinstpane::_ctl_attach {} { return [attach_selected] }
proc ::vinstpane::_ctl_detach {} { return [detach] }
proc ::vinstpane::_ctl_add    {} { return [add_entry] }
proc ::vinstpane::_ctl_edit   {} { return [edit_entry] }
proc ::vinstpane::_ctl_remove {} { return [remove_entry] }

proc ::vinstpane::_ctl_reload {} {
    set n [_name_or_msg]
    if {$n eq ""} { return 0 }
    if {[tk_messageBox -type yesno -icon warning -title [::vi18n::mc {Instances}] \
            -message [format [::vi18n::mc {Reload the Tcl plugin layer on %s?}] $n] \
            -detail [::vi18n::mc {A failing plugin may drop hooks mid-turn.}]] ne "yes"} {
        return 0
    }
    if {[catch {
        ::vapi::with_instance $n { ::vapi::post /api/tcl/reload {{}} 5000 }
    } r]} {
        ::vstatus::set_msg [format [::vi18n::mc {reload on %1$s failed: %2$s}] $n $r]
        return 0
    }
    # A reload that failed must not say "reloaded": the body carries
    # ok:false when a plugin script raised.
    set ok 0 ; set why ""
    if {[catch { set rj [::json::json2dict $r] } json_err]} {
        puts stderr "veles-desktop: reload response is not JSON: $json_err"
    }
    if {[info exists rj]} {
        catch { set ok   [dict get $rj ok] }
        catch { set why  [dict get $rj result] }
    }
    if {$ok} {
        ::vstatus::set_msg [format [::vi18n::mc {plugins reloaded on %s}] $n]
    } else {
        ::vstatus::set_msg [format [::vi18n::mc {reload on %1$s failed: %2$s}] $n $why]
    }
    poll_detail_once
    return $ok
}

# No global pause endpoint exists; "pause the scheduler" is "pause
# every job", iterating /api/cron and posting :id/pause. Reports
# n ok, m failed. Switch to a global endpoint if one appears.
proc ::vinstpane::_pause_resume {verb} {
    set n [_name_or_msg]
    if {$n eq ""} { return 0 }
    if {[tk_messageBox -type yesno -icon warning -title [::vi18n::mc {Instances}] \
            -message [format [::vi18n::mc {%1$s every cron job on %2$s?}] $verb $n]] ne "yes"} {
        return 0
    }
    if {[catch {
        ::vapi::with_instance $n { ::vapi::get_json /api/cron 5000 }
    } j]} {
        ::vstatus::set_msg [format [::vi18n::mc {cron list on %1$s failed: %2$s}] $n $j]
        return 0
    }
    set jobs ""
    catch { set jobs [dict get $j jobs] }
    set done 0 ; set failed 0
    foreach job $jobs {
        set id "" ; set en 1
        catch { set id [dict get $job id] }
        catch { set en [dict get $job enabled] }
        if {$id eq ""} { continue }
        if {$verb eq "Pause" && !$en} { continue }
        if {$verb eq "Resume" && $en} { continue }
        set path [expr {$verb eq "Pause"
            ? "/api/cron/[::vsafe::urlenc $id]/pause"
            : "/api/cron/[::vsafe::urlenc $id]/resume"}]
        if {[catch {
            ::vapi::with_instance $n [list ::vapi::post $path {{}} 5000]
        } err]} {
            puts stderr "veles-desktop: cron $verb $id on $n failed: $err"
            incr failed
        } else {
            incr done
        }
    }
    ::vstatus::set_msg [format [::vi18n::mc {%1$s on %2$s: %3$d ok, %4$d failed}] \
        $verb $n $done $failed]
    poll_detail_once
    return [expr {$failed == 0}]
}

proc ::vinstpane::_ctl_pause {}  { return [_pause_resume Pause] }
proc ::vinstpane::_ctl_resume {} { return [_pause_resume Resume] }

# ── Part 5: start / stop / restart over SSH ─────────────────────────
#
# `ssh <host> systemctl --user <verb> <unit>`, BatchMode so a host
# needing a passphrase cannot hang the desktop on a prompt that does
# not exist. host/unit come from a file the operator edits by hand —
# the same shape as the IRC-nick exec bug (safeexec.tcl) — so every
# data word goes through ::vsafe::arg AND both fields are validated
# against a whitelist regexp first. The composed argv is a LIST, never
# a string; the tests assert it as a list so a regression that builds
# a string is visible.

proc ::vinstpane::valid_unit {u} {
    variable unit_re
    return [expr {[regexp -- $unit_re $u] && ![string match {-*} $u]}]
}

proc ::vinstpane::valid_host {h} {
    variable host_re
    return [expr {[regexp -- $host_re $h] && ![string match {-*} $h]}]
}

# Pure: the exact ssh argv for a lifecycle verb. Exported for the
# tests, which assert it AS A LIST.
proc ::vinstpane::ssh_argv {host unit verb} {
    return [list ssh \
        -o BatchMode=yes \
        -o ConnectTimeout=5 \
        [::vsafe::arg $host] \
        systemctl --user $verb [::vsafe::arg $unit]]
}

# A saved instance bound to a cluster node (its node_id) must still BE
# that node before ssh starts, stops or restarts its unit (FLEET-2): the
# same check as attach and Fleet deploy, asked of the instance's own
# gateway. A gateway that answers as another node, or with no identity,
# is refused with the reason in the status bar. One that cannot be asked
# is the normal state for `start` (a stopped gateway does not answer),
# which proceeds and is checked once it is up; a stop or a restart of a
# gateway that cannot be asked (a hung one is why one restarts) asks the
# operator first. No node_id, nothing to prove.
proc ::vinstpane::_lifecycle_binding_ok {n spec verb} {
    set node_id ""
    catch { set node_id [dict get $spec node_id] }
    if {$node_id eq ""} { return 1 }
    lassign [node_binding_verdict $n $node_id] verdict why
    switch -- $verdict {
        ok { return 1 }
        unreachable {
            if {$verb eq "start"} { return 1 }
            if {[_confirm [format [::vi18n::mc {%1$s %2$s over ssh anyway?}] $verb $n] \
                    [format [::vi18n::mc {%s The command goes over ssh to the configured host, which this check could not tie to the node.}] $why]]} {
                return 1
            }
            ::vstatus::set_msg $why
            return 0
        }
        default {
            ::vstatus::set_msg $why
            return 0
        }
    }
}

proc ::vinstpane::_ssh_run {n verb} {
    # `variable _ssh_out` is not decoration: without it the `set
    # _ssh_out($f) ""` below creates a LOCAL array that dies with this
    # proc, and _ssh_readable's accumulator starts from nothing.
    variable _ssh_out
    set spec [::vinst::get $n]
    set host "" ; set unit ""
    catch { set host [dict get $spec host] }
    catch { set unit [dict get $spec unit] }
    if {$unit eq ""} { set unit "veles-gateway" }
    if {![valid_host $host]} {
        ::vstatus::set_msg [format [::vi18n::mc {bad host for %1$s: %2$s}] $n $host]
        return 0
    }
    if {![valid_unit $unit]} {
        ::vstatus::set_msg [format [::vi18n::mc {bad unit for %1$s: %2$s}] $n $unit]
        return 0
    }
    if {![_lifecycle_binding_ok $n $spec $verb]} { return 0 }
    set argv [ssh_argv $host $unit $verb]
    ::vstatus::set_msg [format [::vi18n::mc {%1$s %2$s over ssh…}] $verb $n]
    # `open |argv r` with a LIST: no shell, no exec redirection grammar,
    # one argument per element. Asynchronous via fileevent so the Tk
    # event loop keeps running for the connect timeout.
    if {[catch { open |$argv r } f]} {
        ::vstatus::set_msg [format [::vi18n::mc {ssh to %1$s failed: %2$s}] $host $f]
        return 0
    }
    fconfigure $f -blocking 0 -encoding utf-8 -buffering line
    set ep [::vapp::epoch]
    set _ssh_out($f) ""
    fileevent $f readable [list ::vinstpane::_ssh_readable $f $n $verb $ep]
    return 1
}

proc ::vinstpane::_ssh_readable {f n verb ep} {
    variable _ssh_out
    if {![::vapp::fresh $ep]} {
        # The world moved (attach/detach) while ssh was out; nobody is
        # watching this result on behalf of the old epoch.
        catch { close $f }
        unset -nocomplain _ssh_out($f)
        return
    }
    if {[catch { read $f } chunk]} {
        # A read error that is not EOF re-fires the readable event in a
        # hot loop. Disarm and treat it as EOF.
        puts stderr "veles-desktop: ssh read error on $verb $n: $chunk"
        catch { fileevent $f readable {} }
        set chunk ""
    }
    if {$chunk ne ""} { append _ssh_out($f) $chunk }
    if {[eof $f]} {
        set out ""
        if {[info exists _ssh_out($f)]} { set out $_ssh_out($f) }
        unset -nocomplain _ssh_out($f)
        # close raises on a nonzero child exit; its message is the
        # reason (ssh's stderr rides the same pipe).
        if {[catch { close $f } why]} {
            ::vstatus::set_msg [format [::vi18n::mc {%1$s %2$s failed: %3$s}] $verb $n $why]
            return
        }
        if {$verb eq "start"} {
            ::vstatus::set_msg [format \
                [::vi18n::mc {unit starting on %s — waiting for health…}] $n]
            _wait_healthy $n
        } else {
            ::vstatus::set_msg [format [::vi18n::mc {%1$s %2$s done}] $verb $n]
            _health_one $n
        }
    }
}

# A start that succeeds does not mean a gateway that is up: systemd
# returns while the unit is still activating. Poll /api/health every
# second for up to 30s and only then let the dot go green. Giving up
# is REPORTED — "started" while the port is still closed is the failure
# mode this pane exists to prevent.
proc ::vinstpane::_wait_healthy {n} {
    variable ssh_after
    variable ssh_tries
    if {[catch {
        ::vapi::with_instance $n { ::vapi::get /api/health 2000 }
    } err]} {
        incr ssh_tries
        if {$ssh_tries >= 30} {
            set ssh_tries 0
            ::vstatus::set_msg [format \
                [::vi18n::mc {%s did not become healthy within 30s}] $n]
            return 0
        }
        set ssh_after [after 1000 [list ::vinstpane::_wait_healthy $n]]
        return 0
    }
    set ssh_tries 0
    ::vstatus::set_msg [format [::vi18n::mc {%s is up}] $n]
    # A start could not be checked while the gateway was down; it can now.
    set node_id ""
    catch { set node_id [dict get [::vinst::get $n] node_id] }
    if {$node_id ne ""} {
        set why [verify_node_binding $n $node_id]
        if {$why ne ""} {
            ::vstatus::set_msg [format [::vi18n::mc {%1$s is up, but its node binding is wrong: %2$s}] $n $why]
        }
    }
    _health_one $n
    poll_detail_once
    return 1
}

proc ::vinstpane::_ctl_start   {} {
    set n [_name_or_msg]
    if {$n eq ""} { return 0 }
    return [_ssh_run $n start]
}
proc ::vinstpane::_ctl_stop    {} {
    set n [_name_or_msg]
    if {$n eq ""} { return 0 }
    return [_ssh_run $n stop]
}
proc ::vinstpane::_ctl_restart {} {
    variable selected
    # A registry row: the node restarts its own gateway through its
    # supervisor, asked over the cluster link (lib/fleetmanage.tcl). A
    # saved instance: ssh + systemd --user, as before.
    if {[_is_cluster $selected]} {
        set node [_cluster_node $selected]
        if {$node eq ""} { return 0 }
        return [::vfleetmanage::restart [dict get $node id] [dict get $node name]]
    }
    set n [_name_or_msg]
    if {$n eq ""} { return 0 }
    return [_ssh_run $n restart]
}
# Node management (lib/fleetmanage.tcl): one node's config, Probe all,
# Sync all of a kind, and the command center's audit log.
proc ::vinstpane::_ctl_config {} {
    variable selected
    set node [_cluster_node $selected]
    if {$node eq ""} { return 0 }
    return [expr {[::vfleetmanage::open_config [dict get $node id] [dict get $node name]] ne ""}]
}
proc ::vinstpane::_ctl_bulkprobe {} { return [::vfleetmanage::bulk probe] }
proc ::vinstpane::_ctl_bulksync {} { return [expr {[::vfleetmanage::open_bulk_sync] ne ""}] }
proc ::vinstpane::_ctl_audit {} { return [expr {[::vfleetmanage::open_audit] ne ""}] }
proc ::vinstpane::_ctl_deploy {} {
    return [::vfleetui::open]
}

# ── registry editing ────────────────────────────────────────────────

proc ::vinstpane::_fields {} {
    return {
        name  {name}
        url   {url}
        token {token}
        host  {host — ssh target, optional}
        unit  {systemd --user unit}
        group {group}
        node_id {cluster node id — explicit attach association, optional}
    }
}

# Modal entry dialog. Returns a dict field->string, or "" on cancel.
# The fields are read BEFORE the dialog is destroyed.
proc ::vinstpane::_ask {title spec} {
    variable _dlg_rc
    set fields [_fields]
    set dlg .instentry
    if {[winfo exists $dlg]} { destroy $dlg }
    toplevel $dlg
    wm title $dlg $title
    catch { wm transient $dlg . }
    # Each label above its field, one column: the long labels ("cluster
    # node id — explicit attach association, optional") no longer push
    # the fields off a narrow window, and the column is as wide as the
    # widest of them or a 44-character field.
    set row 0
    foreach {k label} $fields {
        ttk::label $dlg.l$k -text $label
        grid $dlg.l$k -row $row -column 0 -sticky w -padx 8 -pady {6 0}
        incr row
        ttk::entry $dlg.e$k -width 44
        $dlg.e$k insert 0 [dict get $spec $k]
        grid $dlg.e$k -row $row -column 0 -sticky we -padx 8 -pady {1 2}
        catch { ::vi18n::aname $dlg.e$k {field: %s} $label }
        incr row
    }
    grid columnconfigure $dlg 0 -weight 1
    set _dlg_rc ""
    frame $dlg.btns
    ttk::button $dlg.btns.ok -text OK -command { set ::vinstpane::_dlg_rc ok }
    ttk::button $dlg.btns.cancel -text [::vi18n::mc {Cancel}] \
        -command { set ::vinstpane::_dlg_rc cancel }
    pack $dlg.btns.ok $dlg.btns.cancel -side left -padx 6 -pady 6
    grid $dlg.btns -row $row -column 0 -pady 6
    bind $dlg <Escape> { set ::vinstpane::_dlg_rc cancel }
    bind $dlg <Return> { set ::vinstpane::_dlg_rc ok }
    # As large as it asks to be. A guessed 380 px cut a 640 px dialog:
    # the url showed eight characters and Cancel read "Can" (SMOKE-16).
    # Twice: on a phone the first fit wraps the long labels to the
    # screen, and the wrapped dialog is taller than it asked before.
    foreach pass {1 2} {
        update idletasks
        catch { ::vplace::centered $dlg [winfo reqwidth $dlg] [winfo reqheight $dlg] 0.2 }
    }
    catch { focus $dlg.ename }
    catch { grab set $dlg }
    tkwait variable ::vinstpane::_dlg_rc
    catch { grab release $dlg }
    set out ""
    if {$::vinstpane::_dlg_rc eq "ok"} {
        set out [dict create]
        foreach {k label} $fields {
            set v ""
            catch { set v [string trim [$dlg.e$k get]] }
            dict set out $k $v
        }
    }
    destroy $dlg
    return $out
}

proc ::vinstpane::_save_entry {name v} {
    # Validate before saving — the registry is hand-editable, so a bad
    # value here is a bad value on every boot afterwards.
    if {$name eq ""} {
        ::vstatus::set_msg [::vi18n::mc {instance needs a name}]
        return 0
    }
    set url [dict get $v url]
    if {![::vinst::validate $url]} {
        ::vstatus::set_msg [format [::vi18n::mc {not a valid http(s) url: %s}] $url]
        return 0
    }
    set host [dict get $v host]
    if {$host ne "" && ![valid_host $host]} {
        ::vstatus::set_msg [format [::vi18n::mc {not a valid ssh host: %s}] $host]
        return 0
    }
    set unit [dict get $v unit]
    if {$unit ne "" && ![valid_unit $unit]} {
        ::vstatus::set_msg [format [::vi18n::mc {not a valid unit name: %s}] $unit]
        return 0
    }
    # Tree ids: a saved row's id is its name and a group's is `g:<name>`,
    # and `cluster:` marks a registry row. A saved `cluster:x` lost its
    # controls as a registry row would (FLEET-13).
    if {[string match {cluster:*} $name] || [string match {g:*} $name]} {
        ::vstatus::set_msg [format [::vi18n::mc {instance names may not start with "cluster:" or "g:": %s}] $name]
        return 0
    }
    # node_id decides which machine a registry row attaches and deploys
    # to, so an id the attached registry does not know is refused. With
    # no registry loaded it cannot be checked here; attach verifies it
    # against the gateway itself either way.
    # An UNCHANGED id is not re-judged: editing a token must not fail
    # because the node has since been forgotten.
    variable fleet_nodes
    set node_id [dict get $v node_id]
    set previous_id ""
    catch { set previous_id [dict get [::vinst::get $name] node_id] }
    if {$node_id ne "" && $node_id ne $previous_id && [llength $fleet_nodes]} {
        set known 0
        foreach node $fleet_nodes {
            if {[dict get $node id] eq $node_id} { set known 1 ; break }
        }
        if {!$known} {
            ::vstatus::set_msg [format [::vi18n::mc {not a node in the attached gateway's cluster registry: %s}] $node_id]
            return 0
        }
    }
    # The dialog shows every field, so what it returns is the whole
    # entry: an emptied node id is a cleared association, and re-adding
    # a name does not inherit the old one's binding (FLEET-4). The old
    # carry-over of `previous` node_id made both impossible.
    ::vinst::add $name $url [dict get $v token]
    foreach k {host unit group node_id} {
        if {[dict get $v $k] ne ""} {
            ::vinst::update $name $k [dict get $v $k]
        }
    }
    if {![::vinst::save]} {
        ::vstatus::set_msg [format [::vi18n::mc {registry not saved: %s}] \
            [::vinst::error_text]]
        return 0
    }
    ::vapi::bind_instance $name $url [dict get $v token]
    refresh_list
    ::vstatus::set_msg [format [::vi18n::mc {saved instance %s}] $name]
    return 1
}

# The Add… dialog's starting values. With a registry row selected, the
# new instance is that node ADOPTED: its name and node id are filled in.
# Not its URL or SSH host — the registry endpoint is the A2A address,
# not the desktop API, and a guessed SSH target is how one restarts the
# wrong machine.
proc ::vinstpane::add_defaults {} {
    variable selected
    set d [dict create name "" url http://127.0.0.1:8642 token "" host "" \
        unit veles-gateway group "" node_id ""]
    if {[_is_cluster $selected]} {
        set node [_cluster_node $selected]
        if {$node ne ""} {
            dict set d name [dict get $node name]
            dict set d node_id [dict get $node id]
        }
    }
    return $d
}

proc ::vinstpane::add_entry {} {
    set v [_ask [::vi18n::mc {Add instance}] [add_defaults]]
    if {$v eq ""} { return 0 }
    return [_save_entry [dict get $v name] $v]
}

proc ::vinstpane::edit_entry {} {
    set n [_name_or_msg]
    if {$n eq ""} { return 0 }
    set spec [::vinst::get $n]
    set defaults [dict create name $n]
    foreach k {url token host unit group node_id} {
        set val ""
        catch { set val [dict get $spec $k] }
        dict set defaults $k $val
    }
    if {[dict get $defaults unit] eq ""} { dict set defaults unit veles-gateway }
    set v [_ask [format [::vi18n::mc {Edit %s}] $n] $defaults]
    if {$v eq ""} { return 0 }
    set newname [dict get $v name]
    if {$newname ne $n} {
        # A rename is remove + re-add; detach first if it was attached.
        if {[llength [info commands ::vapi::attached]] && [::vapi::attached] eq $n} {
            detach
        }
        ::vinst::remove $n
    }
    return [_save_entry $newname $v]
}

proc ::vinstpane::remove_entry {} {
    variable selected
    set n [_name_or_msg]
    if {$n eq ""} { return 0 }
    if {[tk_messageBox -type yesno -icon warning -title [::vi18n::mc {Instances}] \
            -message [format [::vi18n::mc {Remove %1$s from the registry?}] $n] \
            -detail [::vi18n::mc {The gateway itself is untouched.}]] ne "yes"} {
        return 0
    }
    if {[llength [info commands ::vapi::attached]] && [::vapi::attached] eq $n} {
        detach
    }
    ::vinst::remove $n
    if {![::vinst::save]} {
        ::vstatus::set_msg [format [::vi18n::mc {registry not saved: %s}] \
            [::vinst::error_text]]
        return 0
    }
    if {$selected eq $n} {
        set selected ""
        _clear_detail
        _enable_buttons
    }
    refresh_list
    ::vstatus::set_msg [format [::vi18n::mc {removed instance %s}] $n]
    return 1
}

# ── boot ────────────────────────────────────────────────────────────

# Region registration, split out of boot so the layout contract is
# assertable in suites that cannot boot the whole app (tabtest fixtures
# drive ::vdock/::vtabs directly).
#
# .instctl is a BOTTOM STRIP (::vdock::register_strip), not a column:
# the plan names it "the controls strip", and docked as a column its
# separators stretched the full window height with the buttons floating
# mid-cavity (2026-08-28 user report: "is something supposed to be in
# those panes?"). Strips stay tear-off-able like every other region.
proc ::vinstpane::register_regions {} {
    ::vdock::register .instlist   {list $::vinstpane::R_list.list}   ::vinstpane::build_list
    ::vdock::register .instdetail {list $::vinstpane::R_detail.body} ::vinstpane::build_detail
    ::vdock::register .instctl    {list}                             ::vinstpane::build_ctl
    ::vdock::register_strip .instctl
}

proc ::vinstpane::boot {} {
    variable R_list
    # Regions register with ::vdock so tabs, undocking and the focus
    # ring all know about them without dock.tcl naming this pane.
    register_regions
    build_list .instlist
    build_detail .instdetail
    build_ctl .instctl
    refresh_list
    # Select the first instance so the tab is never a dead tree. The
    # poll machinery handles unreachable rows fine; that is its job.
    set names [::vinst::names]
    if {[llength $names]} {
        set first [lindex $names 0]
        catch { $R_list.list selection set $first }
        _select $first
    } else {
        _enable_buttons
    }
    poll_start
    return .instlist
}
