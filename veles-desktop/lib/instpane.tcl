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
    ttk::treeview $R_list.list -columns {version uptime platforms} \
        -show {tree headings} -selectmode browse
    $R_list.list heading #0        -text [::vi18n::mc {instance}]
    $R_list.list heading version   -text [::vi18n::mc {version}]
    $R_list.list heading uptime    -text [::vi18n::mc {uptime}]
    $R_list.list heading platforms -text [::vi18n::mc {platforms}]
    $R_list.list column #0        -width 160 -stretch 1
    $R_list.list column version   -width 70  -stretch 0 -anchor w
    $R_list.list column uptime    -width 100 -stretch 0 -anchor w
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
    ::vctx::attach $R_list.list {
        {"Attach"  {::vinstpane::attach_selected}}
        {-}
        {"Add…"    {::vinstpane::add_entry}}
        {"Edit…"   {::vinstpane::edit_entry}}
        {"Remove"  {::vinstpane::remove_entry}}
    }
    catch { ::va11y::name $R_list.list [::vi18n::mc {instances}] }
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
proc ::vinstpane::refresh_list {} {
    variable R_list
    variable health
    variable meta
    variable selected
    if {![winfo exists $R_list.list]} { return }
    set keep $selected
    $R_list.list delete [$R_list.list children {}]
    set attached ""
    if {[llength [info commands ::vapi::attached]]} {
        set attached [::vapi::attached]
    }
    foreach {g names} [::vinst::groups] {
        $R_list.list insert {} end -id "g:$g" -text "▾ $g" -tags group
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
            $R_list.list insert "g:$g" end -id $n -text "$dot $n$mark" \
                -values [list $ver $up $plats] -tags $tag ;# i18n-exempt: glyph + node name
        }
    }
    if {$keep ne "" && [$R_list.list exists $keep]} {
        $R_list.list selection set $keep
        $R_list.list focus $keep
    }
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
    return $lines
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
    if {[_tab_current]} {
        foreach n [::vinst::names] { _health_one $n }
    }
    set list_after [after $poll_list_ms ::vinstpane::_poll_list_tick]
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

proc ::vinstpane::poll_detail_once {} {
    variable selected
    variable meta
    variable detail
    variable inflight
    variable health
    variable gen
    if {$selected eq ""} { return }
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
    set rc [::vapp::attach $name]
    refresh_list
    _enable_buttons
    return $rc
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
    set sep 0
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
    set labels [dict create]
    foreach {id label} {
        attach    {Attach}
        detach    {Detach}
        |         {}
        reload    {Reload plugins}
        pause     {Pause scheduler}
        resume    {Resume scheduler}
        |         {}
        start     {Start}
        stop      {Stop}
        restart   {Restart}
        |         {}
        add       {Add…}
        edit      {Edit…}
        remove    {Remove}
    } {
        if {$id eq "|"} {
            incr sep
            ttk::separator $R_ctl.s$sep -orient vertical
            pack $R_ctl.s$sep -side left -fill y -padx 3 -pady 3
            continue
        }
        dict set labels $id $label
        ttk::button $R_ctl.$id -text [::vi18n::mc $label] \
            -command [list ::vinstpane::_ctl_$id]
        ::vi18n::label $R_ctl.$id $label
        pack $R_ctl.$id -side left -padx 2 -pady 3
        catch { ::vcoach::attach $R_ctl.$id }
    }
    catch { ::va11y::name $R_ctl [::vi18n::mc {instance controls}] }
    # Eleven buttons are wider than a phone: there they wrap into rows
    # (lib/android.tcl). Add… was off the screen on the emulator.
    if {[llength [info commands ::vandroid::flow]]} { ::vandroid::flow $R_ctl }
    # Float-path pack only — see build_list. At boot this packed the
    # strip into the root AFTER .mid, which left it a zero-height
    # cavity until the first tab switch repacked it.
    set parent [winfo parent $R_ctl]
    if {$parent ne "." && [winfo toplevel $parent] eq $parent} { pack $R_ctl -side bottom -fill x }
    return $R_ctl
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
    if {$name eq ""} {
        set spec [dict create]
        set st never
    } else {
        set spec [::vinst::get $name]
        if {$spec eq ""} { set spec [dict create] }
        set st [expr {[info exists health($name)] ? $health($name) : "never"}]
    }
    set states [button_states $name $spec $st]
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
    set n [_name_or_msg]
    if {$n eq ""} { return 0 }
    return [_ssh_run $n restart]
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
    set row 0
    foreach {k label} $fields {
        ttk::label $dlg.l$k -text $label
        grid $dlg.l$k -row $row -column 0 -sticky e -padx 4 -pady 3
        ttk::entry $dlg.e$k -width 44
        $dlg.e$k insert 0 [dict get $spec $k]
        grid $dlg.e$k -row $row -column 1 -sticky we -padx 4 -pady 3
        catch { ::vi18n::aname $dlg.e$k {field: %s} $label }
        incr row
    }
    set _dlg_rc ""
    frame $dlg.btns
    ttk::button $dlg.btns.ok -text OK -command { set ::vinstpane::_dlg_rc ok }
    ttk::button $dlg.btns.cancel -text [::vi18n::mc {Cancel}] \
        -command { set ::vinstpane::_dlg_rc cancel }
    pack $dlg.btns.ok $dlg.btns.cancel -side left -padx 6 -pady 6
    grid $dlg.btns -row $row -column 0 -columnspan 2 -pady 6
    bind $dlg <Escape> { set ::vinstpane::_dlg_rc cancel }
    bind $dlg <Return> { set ::vinstpane::_dlg_rc ok }
    catch { ::vplace::centered $dlg 380 [expr {80 + 28 * [llength $fields]}] 0.2 }
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
    ::vinst::add $name $url [dict get $v token]
    foreach k {host unit group} {
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

proc ::vinstpane::add_entry {} {
    set v [_ask [::vi18n::mc {Add instance}] [dict create \
        name "" url http://127.0.0.1:8642 token "" host "" unit veles-gateway group ""]]
    if {$v eq ""} { return 0 }
    return [_save_entry [dict get $v name] $v]
}

proc ::vinstpane::edit_entry {} {
    set n [_name_or_msg]
    if {$n eq ""} { return 0 }
    set spec [::vinst::get $n]
    set defaults [dict create name $n]
    foreach k {url token host unit group} {
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
