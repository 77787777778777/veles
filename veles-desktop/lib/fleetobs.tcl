# veles-desktop/lib/fleetobs.tcl — the Instances tab's per-node
# observation: a node's log tail, its metrics, the fleet's version and
# config drift, its health alerts, and an explicit gossip exchange
# (owner ask 287976.1, docs/command-center.md "Observing nodes").
#
# Everything goes through the ATTACHED gateway's /api/fleet/* routes
# (crates/veles-frontends/src/fleet_obs.rs). For another node the
# gateway asks it over its own [a2a_agents] link; the registry row is
# identity only and nothing here dials a node. The answers are redacted
# on the node they come from.
#
# Phone layout: the log and drift viewers are ordinary dialogs placed
# through ::vplace::centered, which fits them in dp on Android
# (lib/android.tcl); metrics, drift and alerts also land in the detail
# pane, which needs no dialog at all.
#
# SOURCING ORDER: after api.tcl and instpane.tcl.

namespace eval ::vfleetobs {
    variable metrics            ;# node id -> last /api/fleet/metrics body
    array set metrics {}
    variable metrics_at         ;# node id -> [clock seconds] it arrived
    array set metrics_at {}
    variable drift ""           ;# last /api/fleet/drift body
    variable logs_node ""       ;# node id the log viewer shows
    variable logs_name ""
    variable logs_lines 100
    variable logs_level all
    variable logs_gen 0         ;# drops a reply for a node no longer shown
    variable busy               ;# key -> 1 while a request is out
    array set busy {}
}

# ── pure formatters (the suite drives these without a gateway) ──────

proc ::vfleetobs::_bytes {n} {
    if {![string is wideinteger -strict $n]} { return "?" }
    if {$n >= 1073741824} { return [format "%.1f GiB" [expr {$n / 1073741824.0}]] }
    if {$n >= 1048576}    { return [format "%.0f MiB" [expr {$n / 1048576.0}]] }
    return "$n B"
}

proc ::vfleetobs::_num {v {fmt %.2f}} {
    if {[string is double -strict $v]} { return [format $fmt $v] }
    return "?"
}

# A /api/fleet/metrics body as detail-pane lines.
proc ::vfleetobs::metrics_lines {m} {
    set lines {}
    lappend lines [format [::vi18n::mc {Build: veles %1$s, revision %2$s (%3$s %4$s)}] \
        [::vapi::field $m version ?] [::vapi::field $m revision ?] \
        [::vapi::field $m os] [::vapi::field $m arch]]
    set cpu [::vapi::field $m cpu]
    if {[::vapi::field $cpu load1] eq "" && [::vapi::field $cpu busy_pct] ne ""} {
        # Windows keeps no load average; the node measured its CPU busy
        # share since its previous sample instead (FLEET-7).
        lappend lines [format [::vi18n::mc {CPU: %1$s cores, %2$s%% busy over the last %3$s s}] \
            [::vapi::field $cpu cores ?] [_num [::vapi::field $cpu busy_pct] %.1f] \
            [_num [::vapi::field $cpu busy_window_secs] %.0f]]
    } else {
        lappend lines [format [::vi18n::mc {CPU: %1$s cores, load %2$s / %3$s / %4$s (%5$s%% of the cores)}] \
            [::vapi::field $cpu cores ?] [_num [::vapi::field $cpu load1]] \
            [_num [::vapi::field $cpu load5]] [_num [::vapi::field $cpu load15]] \
            [::vapi::field $cpu load1_pct ?]]
    }
    set mem [::vapi::field $m memory]
    if {$mem ne ""} {
        lappend lines [format [::vi18n::mc {Memory: %1$s available of %2$s (%3$s%% used)}] \
            [_bytes [::vapi::field $mem available_bytes]] \
            [_bytes [::vapi::field $mem total_bytes]] [::vapi::field $mem used_pct ?]]
    }
    set disk [::vapi::field $m disk]
    if {$disk ne ""} {
        lappend lines [format [::vi18n::mc {Disk: %1$s free of %2$s at %3$s}] \
            [_bytes [::vapi::field $disk free_bytes]] \
            [_bytes [::vapi::field $disk total_bytes]] [::vapi::field $disk path ?]]
    }
    set io [::vapi::field $m io_pressure]
    if {$io ne ""} {
        lappend lines [format [::vi18n::mc {IO pressure: %1$s%% / %2$s%% / %3$s%% of the time some task waited (10 s / 1 min / 5 min)}] \
            [_num [::vapi::field $io some_avg10]] [_num [::vapi::field $io some_avg60]] \
            [_num [::vapi::field $io some_avg300]]]
    }
    lappend lines [format [::vi18n::mc {Uptime: host %1$s, gateway %2$s}] \
        [::vinstpane::_uptime [::vapi::field $m host_uptime_secs]] \
        [::vinstpane::_uptime [::vapi::field $m gateway_uptime_secs]]]
    set fl [::vapi::field $m in_flight]
    set turns {}
    set by [::vapi::field $fl turns]
    if {$by ne ""} {
        dict for {surface n} $by { lappend turns "$surface $n" }
    }
    set tasks {}
    set bk [::vapi::field $fl tasks]
    if {$bk ne ""} {
        dict for {kind n} $bk { lappend tasks "$kind $n" }
    }
    lappend lines [format [::vi18n::mc {In flight: %1$s turn(s) %2$s, %3$s background task(s) %4$s}] \
        [::vapi::field $fl turns_total 0] [expr {[llength $turns] ? "([join $turns {, }])" : ""}] \
        [::vapi::field $fl tasks_total 0] [expr {[llength $tasks] ? "([join $tasks {, }])" : ""}]]
    foreach u [::vapi::field $m unavailable {}] {
        lappend lines [format [::vi18n::mc {Not available: %s}] $u]
    }
    return $lines
}

# The drift report's lines for one node (or a single line when it was
# not compared), from a /api/fleet/drift body.
proc ::vfleetobs::drift_node_lines {d node_id} {
    foreach n [::vapi::field $d nodes {}] {
        if {[::vapi::field $n id] ne $node_id} { continue }
        set err [::vapi::field $n error]
        if {$err ne ""} {
            return [list [format [::vi18n::mc {Drift: not compared — %s}] $err]]
        }
        set lines {}
        if {[::vapi::field $n version_differs 0] in {true 1}} {
            lappend lines [format [::vi18n::mc {Drift: runs version %1$s, this gateway %2$s}] \
                [::vapi::field $n version ?] [::vapi::field [::vapi::field $d self] version ?]]
        }
        if {[::vapi::field $n revision_differs 0] in {true 1}} {
            lappend lines [format [::vi18n::mc {Drift: built from revision %1$s, this gateway from %2$s}] \
                [::vapi::field $n revision ?] [::vapi::field [::vapi::field $d self] revision ?]]
        }
        set cfg [::vapi::field $n config]
        set keys [::vapi::field $cfg keys]
        # One literal catalog call per sentence, formatted where it is
        # made: the key extractor takes no table cell with a `%` or a `:`
        # in it, and i18ntest wants every placeholder key formatted.
        set differ [::vapi::field $keys differ {}]
        set only_here [::vapi::field $keys only_here {}]
        set only_there [::vapi::field $keys only_there {}]
        if {[llength $differ]} {
            lappend lines [format [::vi18n::mc {Config differs: %s}] [join $differ {, }]]
        }
        if {[llength $only_here]} {
            lappend lines [format [::vi18n::mc {Config only on this gateway: %s}] [join $only_here {, }]]
        }
        if {[llength $only_there]} {
            lappend lines [format [::vi18n::mc {Config only on that node: %s}] [join $only_there {, }]]
        }
        set log [::vapi::field $cfg log]
        foreach a [::vapi::field $log here_ahead {}] {
            lappend lines [format \
                [::vi18n::mc {Config log: this gateway has %1$s's changes up to %2$s, that node up to %3$s}] \
                [::vapi::field $a author_name [::vapi::field $a author ?]] \
                [::vapi::field $a here ?] [::vapi::field $a there ?]]
        }
        foreach a [::vapi::field $log there_ahead {}] {
            lappend lines [format \
                [::vi18n::mc {Config log: that node has %1$s's changes up to %3$s, this gateway up to %2$s}] \
                [::vapi::field $a author_name [::vapi::field $a author ?]] \
                [::vapi::field $a here ?] [::vapi::field $a there ?]]
        }
        if {![llength $lines]} {
            lappend lines [::vi18n::mc {Drift: none — same version, revision and replicated config}]
        }
        if {[::vapi::field $n syncs_config 0] ni {true 1}} {
            lappend lines [::vi18n::mc {(not a memsync peer of this gateway: config is not synced with it)}]
        }
        return $lines
    }
    return {}
}

# The whole report, for the drift viewer.
proc ::vfleetobs::drift_lines {d} {
    set lines {}
    # Deployment consistency first (lib/fleetmanage.tcl, 0.2.2): whether
    # every node runs the same point release at the same commit.
    if {[llength [info commands ::vfleetmanage::consistency_lines]]} {
        lappend lines {*}[::vfleetmanage::consistency_lines [::vapi::field $d consistency]] ""
    }
    foreach g [::vapi::field [::vapi::field $d versions] groups {}] {
        lappend lines [format [::vi18n::mc {Version %1$s: %2$s}] [::vapi::field $g version ?] \
            [join [_group_names $g] {, }]]
    }
    lappend lines ""
    foreach n [::vapi::field $d nodes {}] {
        lappend lines "[::vapi::field $n name ?]:"
        foreach l [drift_node_lines $d [::vapi::field $n id]] { lappend lines "  $l" }
    }
    if {![llength [::vapi::field $d nodes {}]]} {
        lappend lines [::vi18n::mc {No other registered node to compare with.}]
    }
    return $lines
}

# A version group's node names, a stale node marked (its version does
# not count as drift: the gateway compares live nodes only).
proc ::vfleetobs::_group_names {g} {
    set names {}
    foreach n [::vapi::field $g nodes {}] {
        set name [::vapi::field $n name ?]
        if {[::vapi::field $n stale 0] in {true 1}} {
            set name [format [::vi18n::mc {%s (stale)}] $name]
        }
        lappend names $name
    }
    return $names
}

# The registry's version line, from /api/fleet `versions`.
proc ::vfleetobs::version_line {versions} {
    if {$versions eq "" || [::vapi::field $versions drift 0] ni {true 1}} { return "" }
    set parts {}
    foreach g [::vapi::field $versions groups {}] {
        lappend parts "[::vapi::field $g version ?] ([join [_group_names $g] {, }])"
    }
    return [format [::vi18n::mc {Version drift in the registry: %s}] [join $parts {; }]]
}

# The alert lines for one node (or every node, node_id ""), newest
# first, from /api/fleet `alerts`.
proc ::vfleetobs::alert_lines {alerts node_id {limit 5}} {
    set lines {}
    foreach a [::vapi::field $alerts recent {}] {
        if {$node_id ne "" && [::vapi::field $a node] ne $node_id} { continue }
        set sent [expr {[::vapi::field $a sent 0] in {true 1}
            ? [::vi18n::mc {sent}] : [::vapi::field $a note]}]
        lappend lines [format [::vi18n::mc {%1$s: %2$s → %3$s %4$s (%5$s)}] \
            [::vapi::field $a at ?] [::vapi::field $a from ?] [::vapi::field $a to ?] \
            [expr {$node_id eq "" ? [::vapi::field $a name] : ""}] $sent]
        if {[llength $lines] >= $limit} { break }
    }
    return $lines
}

# A log answer as the viewer's text: one line per entry.
proc ::vfleetobs::log_text {resp} {
    set out {}
    foreach l [::vapi::field $resp lines {}] {
        lappend out "[::vapi::field $l time] [::vapi::field $l level] [::vapi::field $l target]: [::vapi::field $l msg]"
    }
    return [join $out \n]
}

# ── metrics: into the detail pane ───────────────────────────────────

proc ::vfleetobs::fetch_metrics {node_id name} {
    variable busy
    if {[info exists busy(m:$node_id)]} { return 0 }
    set busy(m:$node_id) 1
    if {[catch {
        ::vapi::post_json_async /api/fleet/metrics [dict create node $node_id] \
            [list ::vfleetobs::_metrics_failed $node_id $name] \
            [list ::vfleetobs::_metrics_done $node_id $name]
    } err]} {
        _metrics_failed $node_id $name $err
        return 0
    }
    ::vstatus::set_msg [format [::vi18n::mc {asking %s for its metrics…}] $name]
    return 1
}

proc ::vfleetobs::_metrics_failed {node_id name why} {
    variable busy
    unset -nocomplain busy(m:$node_id)
    ::vstatus::set_msg [format [::vi18n::mc {Metrics of %1$s failed: %2$s}] $name $why]
}

proc ::vfleetobs::_metrics_done {node_id name resp} {
    variable busy
    variable metrics
    variable metrics_at
    unset -nocomplain busy(m:$node_id)
    set metrics($node_id) $resp
    set metrics_at($node_id) [clock seconds]
    ::vstatus::set_msg [format [::vi18n::mc {Metrics of %s received.}] $name]
    if {[::vinstpane::selected] eq "cluster:$node_id"} {
        ::vinstpane::_paint_cluster_detail "cluster:$node_id"
    }
}

# The metrics block of the detail pane for a node, if fetched.
proc ::vfleetobs::detail_lines {node_id} {
    variable metrics
    variable metrics_at
    variable drift
    set lines {}
    if {[info exists metrics($node_id)]} {
        lappend lines [format [::vi18n::mc {Metrics (asked %s):}] \
            [clock format $metrics_at($node_id) -format %H:%M:%S]]
        foreach l [metrics_lines $metrics($node_id)] { lappend lines "  $l" }
    } else {
        lappend lines [::vi18n::mc {Metrics: not asked yet — use Metrics.}]
    }
    if {$drift ne ""} {
        foreach l [drift_node_lines $drift $node_id] { lappend lines $l }
    }
    return $lines
}

# ── the log viewer ──────────────────────────────────────────────────

proc ::vfleetobs::open_logs {node_id name} {
    variable logs_node
    variable logs_name
    set logs_node $node_id
    set logs_name $name
    set dlg .fleetlogs
    if {![winfo exists $dlg]} {
        toplevel $dlg
        catch { wm transient $dlg . }
        ttk::frame $dlg.bar
        ttk::label $dlg.bar.lines_l -text [::vi18n::mc {Lines}]
        ttk::combobox $dlg.bar.lines -width 5 -state readonly \
            -values {50 100 200 500} -textvariable ::vfleetobs::logs_lines
        ttk::label $dlg.bar.level_l -text [::vi18n::mc {Level}]
        ttk::combobox $dlg.bar.level -width 6 -state readonly \
            -values {all info warn error} -textvariable ::vfleetobs::logs_level
        ttk::button $dlg.bar.refresh -text [::vi18n::mc {Refresh}] \
            -command ::vfleetobs::refresh_logs
        ttk::button $dlg.bar.close -text [::vi18n::mc {Close}] -command [list destroy $dlg]
        pack $dlg.bar.lines_l $dlg.bar.lines $dlg.bar.level_l $dlg.bar.level \
            $dlg.bar.refresh $dlg.bar.close -side left -padx 3 -pady 4
        bind $dlg.bar.lines <<ComboboxSelected>> ::vfleetobs::refresh_logs
        bind $dlg.bar.level <<ComboboxSelected>> ::vfleetobs::refresh_logs
        ttk::label $dlg.head -anchor w
        text $dlg.body -wrap none -font {monospace 9} -state disabled \
            -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
            -yscrollcommand [list $dlg.sy set] -xscrollcommand [list $dlg.sx set]
        ttk::scrollbar $dlg.sy -orient vertical -command [list $dlg.body yview]
        ttk::scrollbar $dlg.sx -orient horizontal -command [list $dlg.body xview]
        pack $dlg.bar -side top -fill x
        pack $dlg.head -side top -fill x -padx 6
        pack $dlg.sx -side bottom -fill x
        pack $dlg.sy -side right -fill y
        pack $dlg.body -side left -fill both -expand 1
        bind $dlg <Escape> [list destroy $dlg]
        catch { ::va11y::name $dlg.body [::vi18n::mc {node log}] }
        ::vplace::centered $dlg 760 480
    }
    wm title $dlg [format [::vi18n::mc {Log of %s}] $name]
    refresh_logs
    return $dlg
}

proc ::vfleetobs::refresh_logs {} {
    variable logs_node
    variable logs_name
    variable logs_lines
    variable logs_level
    variable logs_gen
    if {$logs_node eq "" || ![winfo exists .fleetlogs]} { return 0 }
    set g [incr logs_gen]
    # ::vi18n::label, so a locale swap re-texts the heading too.
    ::vi18n::label .fleetlogs.head {Asking %s…} $logs_name
    set body [dict create node $logs_node lines $logs_lines level $logs_level]
    if {[catch {
        ::vapi::post_json_async /api/fleet/logs $body \
            [list ::vfleetobs::_logs_failed $g] [list ::vfleetobs::_logs_done $g]
    } err]} {
        _logs_failed $g $err
        return 0
    }
    return 1
}

proc ::vfleetobs::_logs_failed {g why} {
    variable logs_gen
    if {$g != $logs_gen || ![winfo exists .fleetlogs]} { return }
    ::vi18n::label .fleetlogs.head {The log could not be read: %s} $why
}

proc ::vfleetobs::_logs_done {g resp} {
    variable logs_gen
    variable logs_name
    if {$g != $logs_gen || ![winfo exists .fleetlogs]} { return }
    set shown [llength [::vapi::field $resp lines {}]]
    # The key on the call's own line: the key extractor reads it there.
    set held [::vapi::field $resp held ?]
    ::vi18n::label .fleetlogs.head {%1$s: the last %2$s of %3$s lines its gateway keeps in memory, secrets redacted on that node.} $logs_name $shown $held
    .fleetlogs.body configure -state normal
    .fleetlogs.body delete 1.0 end
    .fleetlogs.body insert end [log_text $resp]
    .fleetlogs.body configure -state disabled
    # The newest lines, from the start of each. `see end` on this -wrap
    # none text also scrolled sideways to the end of the last line, and a
    # long last line opened the viewer a third of the way across (SMOKE-15).
    .fleetlogs.body yview moveto 1
    .fleetlogs.body xview moveto 0
}

# ── drift ───────────────────────────────────────────────────────────

proc ::vfleetobs::check_drift {} {
    variable busy
    if {[info exists busy(drift)]} { return 0 }
    set busy(drift) 1
    if {[catch {
        ::vapi::post_json_async /api/fleet/drift [::vapi::json_raw "{}"] \
            ::vfleetobs::_drift_failed ::vfleetobs::_drift_done
    } err]} {
        _drift_failed $err
        return 0
    }
    ::vstatus::set_msg [::vi18n::mc {comparing the nodes' versions and config…}]
    return 1
}

proc ::vfleetobs::_drift_failed {why} {
    variable busy
    unset -nocomplain busy(drift)
    ::vstatus::set_msg [format [::vi18n::mc {Drift check failed: %s}] $why]
}

proc ::vfleetobs::_drift_done {resp} {
    variable busy
    variable drift
    unset -nocomplain busy(drift)
    set drift $resp
    set n [::vapi::field $resp differing 0]
    ::vstatus::set_msg [format [::vi18n::mc {Drift check: %s node(s) differ from this gateway.}] $n]
    show_drift
    set sel [::vinstpane::selected]
    if {[::vinstpane::_is_cluster $sel]} { ::vinstpane::_paint_cluster_detail $sel }
}

proc ::vfleetobs::show_drift {} {
    variable drift
    set dlg .fleetdrift
    if {![winfo exists $dlg]} {
        toplevel $dlg
        catch { wm transient $dlg . }
        wm title $dlg [::vi18n::mc {Fleet drift}]
        text $dlg.body -wrap word -state disabled -font {monospace 9} \
            -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
            -yscrollcommand [list $dlg.sy set]
        ttk::scrollbar $dlg.sy -orient vertical -command [list $dlg.body yview]
        ttk::frame $dlg.btns
        ttk::button $dlg.btns.again -text [::vi18n::mc {Check again}] -command ::vfleetobs::check_drift
        ttk::button $dlg.btns.close -text [::vi18n::mc {Close}] -command [list destroy $dlg]
        pack $dlg.btns.again $dlg.btns.close -side left -padx 6
        pack $dlg.btns -side bottom -pady 6
        pack $dlg.sy -side right -fill y
        pack $dlg.body -side left -fill both -expand 1
        bind $dlg <Escape> [list destroy $dlg]
        catch { ::va11y::name $dlg.body [::vi18n::mc {fleet drift}] }
        ::vplace::centered $dlg 640 420
    }
    $dlg.body configure -state normal
    $dlg.body delete 1.0 end
    $dlg.body insert end [join [drift_lines $drift] \n]
    $dlg.body configure -state disabled
    return $dlg
}

# ── gossip now ──────────────────────────────────────────────────────

proc ::vfleetobs::gossip_now {} {
    variable busy
    if {[info exists busy(gossip)]} { return 0 }
    set busy(gossip) 1
    if {[catch {
        ::vapi::post_json_async /api/fleet/gossip [::vapi::json_raw "{}"] \
            ::vfleetobs::_gossip_failed ::vfleetobs::_gossip_done
    } err]} {
        _gossip_failed $err
        return 0
    }
    ::vstatus::set_msg [::vi18n::mc {exchanging registry gossip…}]
    return 1
}

proc ::vfleetobs::_gossip_failed {why} {
    variable busy
    unset -nocomplain busy(gossip)
    ::vstatus::set_msg [format [::vi18n::mc {Gossip failed: %s}] $why]
}

proc ::vfleetobs::_gossip_done {resp} {
    variable busy
    unset -nocomplain busy(gossip)
    set parts {}
    foreach r [::vapi::field $resp results {}] {
        set mark [expr {[::vapi::field $r ok 0] in {true 1} ? "✓" : "✗"}]
        lappend parts "$mark [::vapi::field $r peer ?]"
    }
    ::vstatus::set_msg [format [::vi18n::mc {Gossip: %1$s of %2$s peer(s) answered (%3$s).}] \
        [::vapi::field $resp ok 0] [llength [::vapi::field $resp results {}]] [join $parts {, }]]
    ::vinstpane::refresh_fleet
}
