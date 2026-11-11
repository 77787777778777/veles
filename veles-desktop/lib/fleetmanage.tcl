# veles-desktop/lib/fleetmanage.tcl — the Instances tab's node
# management (0.2.2, owner ask 287976: "deploy/redeploy/manage the nodes
# themselves"): Restart a registry node's gateway over the cluster link,
# read and write one node's config key, Probe all / Sync all with one
# confirmation that lists the targets, the audit trail of every
# command-center action, and node health alerts in the window.
#
# Every request goes to the attached gateway (/api/fleet/*), which asks
# the node over its configured [a2a_agents] link (crates/veles-frontends
# fleet_manage.rs). The node decides: it restarts only under a
# supervisor it knows, refuses in a lockdown, and takes config only under
# config sync's rules. What it refuses comes back as the reason, shown
# as it is.
#
# SOURCING ORDER: after api.tcl, place.tcl and instpane.tcl.

namespace eval ::vfleetmanage {
    variable busy                 ;# key -> 1 while a request is out
    array set busy {}
    variable restart_wait ""      ;# after id of the come-back poll
    variable config_node ""       ;# node id the config dialog edits
    variable config_name ""
    variable config_key ""
    variable config_value ""
    variable config_hash ""       ;# what the last read answered
    variable bulk_kind store
    variable alerts_seen ""       ;# newest alert `at` already shown
    variable alerts_primed 0      ;# the first answer is the baseline
    variable alerts_after ""
    variable alerts_error ""     ;# why the last alerts poll could not be sent
    variable toast_after ""
    variable last_toast ""
}

# ── restart ──────────────────────────────────────────────────────────

# Restart a registry node: read its status (supervisor, running work),
# ask once (twice when turns run: the second is the force), send it, and
# watch for the node to come back with a new boot.
proc ::vfleetmanage::restart {node_id name} {
    variable busy
    if {[info exists busy(restart)]} { return 0 }
    set busy(restart) 1
    if {[catch {
        ::vapi::post_json_async /api/fleet/status [dict create node $node_id] \
            [list ::vfleetmanage::_restart_failed $name] \
            [list ::vfleetmanage::_restart_status $node_id $name]
    } err]} {
        _restart_failed $name $err
        return 0
    }
    ::vstatus::set_msg [format [::vi18n::mc {asking %s how it is supervised…}] $name]
    return 1
}

proc ::vfleetmanage::_restart_failed {name why} {
    variable busy
    unset -nocomplain busy(restart)
    ::vstatus::set_msg [format [::vi18n::mc {Restart of %1$s failed: %2$s}] $name $why]
}

# What the confirmation says for a node's status; pure, for the tests.
proc ::vfleetmanage::restart_question {name status} {
    set sup [::vapi::field $status supervisor]
    if {[::vapi::field $sup can_restart 0] ni {true 1}} {
        return [list refused [format [::vi18n::mc {%1$s cannot restart itself: %2$s}] \
            $name [::vapi::field $sup detail ?]]]
    }
    set turns [::vapi::field [::vapi::field $status in_flight] turns_total 0]
    if {$turns eq "null" || $turns eq ""} { set turns 0 }
    set detail [format [::vi18n::mc {Its gateway stops and %s.}] [::vapi::field $sup detail ?]]
    if {$turns > 0} {
        return [list force [format [::vi18n::mc {%1$s is running %2$s turn(s). Restart it anyway?}] $name $turns] \
            "$detail [::vi18n::mc {Desktop turns continue from their saved history after the restart; connector, A2A, mail, cron and subagent turns do not.}]"]
    }
    return [list ask [format [::vi18n::mc {Restart %s?}] $name] $detail]
}

proc ::vfleetmanage::_restart_status {node_id name status} {
    variable busy
    lassign [restart_question $name $status] verdict message detail
    if {$verdict eq "refused"} {
        unset -nocomplain busy(restart)
        ::vstatus::set_msg $message
        return 0
    }
    if {![::vinstpane::_confirm $message $detail]} {
        unset -nocomplain busy(restart)
        ::vstatus::set_msg [::vi18n::mc {Restart cancelled.}]
        return 0
    }
    set boot [::vapi::field $status boot ""]
    set body [dict create node $node_id confirm [::vapi::json_bool 1] \
        force [::vapi::json_bool [expr {$verdict eq "force"}]]]
    if {[catch {
        ::vapi::post_json_async /api/fleet/restart $body \
            [list ::vfleetmanage::_restart_failed $name] \
            [list ::vfleetmanage::_restart_sent $node_id $name $boot]
    } err]} {
        _restart_failed $name $err
        return 0
    }
    return 1
}

proc ::vfleetmanage::_restart_sent {node_id name boot resp} {
    ::vstatus::set_msg [format [::vi18n::mc {%s is restarting; waiting for it to come back…}] $name]
    _restart_poll $node_id $name $boot [clock seconds]
}

# Poll the node's status until it answers with another boot (it is
# back) or two minutes pass. While it restarts the asks fail; that is
# the wait, not an error.
proc ::vfleetmanage::_restart_poll {node_id name boot started} {
    variable restart_wait
    variable busy
    if {[clock seconds] - $started > 120} {
        unset -nocomplain busy(restart)
        ::vstatus::set_msg [format [::vi18n::mc {%s did not come back within 120 s: look at its log where it runs.}] $name]
        return
    }
    set restart_wait [after 2000 [list ::vfleetmanage::_restart_ask $node_id $name $boot $started]]
}

proc ::vfleetmanage::_restart_ask {node_id name boot started} {
    if {[catch {
        ::vapi::post_json_async /api/fleet/status [dict create node $node_id] \
            [list ::vfleetmanage::_restart_down $node_id $name $boot $started] \
            [list ::vfleetmanage::_restart_back $node_id $name $boot $started]
    } why]} {
        _restart_poll $node_id $name $boot $started
    }
}

# An ask that failed: the node is still down (or the link is). Wait on.
proc ::vfleetmanage::_restart_down {node_id name boot started why} {
    _restart_poll $node_id $name $boot $started
}

proc ::vfleetmanage::_restart_back {node_id name boot started status} {
    variable busy
    if {[::vapi::field $status boot ""] eq $boot} {
        _restart_poll $node_id $name $boot $started
        return
    }
    unset -nocomplain busy(restart)
    ::vstatus::set_msg [format [::vi18n::mc {%1$s is back after %2$s s (veles %3$s).}] \
        $name [expr {[clock seconds] - $started}] [::vapi::field $status version ?]]
    catch { ::vinstpane::refresh_fleet }
}

# ── one node's config ────────────────────────────────────────────────

proc ::vfleetmanage::open_config {node_id name} {
    variable config_node
    variable config_name
    variable config_hash
    if {$config_node ne $node_id} { set config_hash "" }
    set config_node $node_id
    set config_name $name
    set dlg .fleetconfig
    if {![winfo exists $dlg]} {
        toplevel $dlg
        catch { wm transient $dlg . }
        ttk::label $dlg.why -anchor w -justify left -wraplength 520 -text \
            [::vi18n::mc {The keys config sync carries, under its rules: node-local keys and credentials stay on their node, and a held section ([acl], approvals, sandbox, hooks, plugins, MCP servers) is changed where it is edited and pushed, then waits for that node's owner.}]
        ttk::label $dlg.key_l -text [::vi18n::mc {Key (for example model.default)}]
        ttk::entry $dlg.key -textvariable ::vfleetmanage::config_key
        ttk::label $dlg.value_l -text [::vi18n::mc {Value (TOML: "text", 5, true, [1, 2])}]
        ttk::entry $dlg.value -textvariable ::vfleetmanage::config_value
        ttk::frame $dlg.btns
        ttk::button $dlg.btns.read -text [::vi18n::mc {Read}] -command ::vfleetmanage::config_read
        ttk::button $dlg.btns.write -text [::vi18n::mc {Write…}] -command [list ::vfleetmanage::config_write 0]
        ttk::button $dlg.btns.unset -text [::vi18n::mc {Unset…}] -command [list ::vfleetmanage::config_write 1]
        ttk::button $dlg.btns.close -text [::vi18n::mc {Close}] -command [list destroy $dlg]
        # Two by two: four touch-sized buttons in one row are wider than
        # a 360 dp phone (instwindowtest measures it).
        grid $dlg.btns.read $dlg.btns.write -padx 3 -pady 3 -sticky ew
        grid $dlg.btns.unset $dlg.btns.close -padx 3 -pady 3 -sticky ew
        ttk::label $dlg.result -anchor w -justify left -wraplength 520
        # The buttons and the result are packed before anything that
        # could grow, so they stay on screen in a short window.
        pack $dlg.why -side top -fill x -padx 8 -pady {8 4}
        pack $dlg.key_l -side top -anchor w -padx 8
        pack $dlg.key -side top -fill x -padx 8
        pack $dlg.value_l -side top -anchor w -padx 8 -pady {6 0}
        pack $dlg.value -side top -fill x -padx 8
        pack $dlg.btns -side top -anchor w -padx 5 -pady 4
        pack $dlg.result -side top -fill x -padx 8 -pady {0 8}
        bind $dlg.key <Return> ::vfleetmanage::config_read
        bind $dlg <Escape> [list destroy $dlg]
        bind $dlg <Configure> [list ::vfleetmanage::_rewrap $dlg %W]
        catch { ::va11y::name $dlg.result [::vi18n::mc {config result}] }
        # Placed, then sized again from its request: on a phone the first
        # fit wraps the labels to the screen, which makes it taller.
        ::vplace::centered $dlg 560 300
        update
        catch { ::vplace::centered $dlg [winfo width $dlg] [winfo reqheight $dlg] }
    }
    wm title $dlg [format [::vi18n::mc {Config of %s}] $name]
    _config_result ""
    return $dlg
}

# Wrapped labels follow the dialog's width (the phone is narrower than
# the 520 px they start with).
proc ::vfleetmanage::_rewrap {dlg w} {
    if {$w ne $dlg} { return }
    set width [expr {max(160, [winfo width $dlg] - 24)}]
    foreach l [list $dlg.why $dlg.result] {
        catch { $l configure -wraplength $width }
    }
}

proc ::vfleetmanage::_config_result {text} {
    if {[winfo exists .fleetconfig.result]} { .fleetconfig.result configure -text $text }
    if {$text ne ""} { ::vstatus::set_msg $text }
}

# The value as the Value field shows it: TOML-ish text a write sends
# back as `raw` (a string without quotes reads back as a string).
proc ::vfleetmanage::value_text {resp} {
    if {[::vapi::field $resp set 0] ni {true 1}} { return "" }
    set v [::vapi::field $resp value ""]
    return $v
}

# What a read says; pure, for the tests.
proc ::vfleetmanage::config_lines {resp} {
    set key [::vapi::field $resp key ?]
    set lines {}
    if {[::vapi::field $resp withheld 0] in {true 1}} {
        lappend lines [format [::vi18n::mc {%s holds a credential: it stays on its node and is not shown.}] $key]
    } elseif {[::vapi::field $resp set 0] in {true 1}} {
        lappend lines [format [::vi18n::mc {%1$s = %2$s on %3$s}] $key [::vapi::field $resp value ""] [::vapi::field $resp name ?]]
    } else {
        lappend lines [format [::vi18n::mc {%1$s is not set on %2$s: the default applies.}] $key [::vapi::field $resp name ?]]
    }
    if {[::vapi::field $resp held 0] in {true 1}} {
        lappend lines [::vi18n::mc {A held section: it is not written from here; edit it on a node and push config, and its owner applies it there.}]
    }
    set note [::vapi::field $resp note ""]
    if {$note ne "" && $note ne "null"} { lappend lines $note }
    return $lines
}

proc ::vfleetmanage::config_read {} {
    variable config_node
    variable config_name
    variable config_key
    variable busy
    set key [string trim $config_key]
    if {$key eq "" || [info exists busy(config)]} { return 0 }
    set busy(config) 1
    if {[catch {
        ::vapi::post_json_async /api/fleet/config [dict create node $config_node key [::vapi::json_string $key]] \
            ::vfleetmanage::_config_failed ::vfleetmanage::_config_read_done
    } err]} {
        _config_failed $err
        return 0
    }
    _config_result [format [::vi18n::mc {reading %1$s on %2$s…}] $key $config_name]
    return 1
}

proc ::vfleetmanage::_config_failed {why} {
    variable busy
    unset -nocomplain busy(config)
    _config_result [format [::vi18n::mc {Config: %s}] $why]
}

proc ::vfleetmanage::_config_read_done {resp} {
    variable busy
    variable config_hash
    variable config_value
    unset -nocomplain busy(config)
    set config_hash [::vapi::field $resp hash ""]
    if {$config_hash eq "null"} { set config_hash "" }
    set config_value [value_text $resp]
    _config_result [join [config_lines $resp] \n]
}

# Write (or unset) the key. Needs a read first: the write names the
# value it replaces, and the node refuses it if that changed meanwhile.
proc ::vfleetmanage::config_write {unset} {
    variable config_node
    variable config_name
    variable config_key
    variable config_value
    variable config_hash
    variable busy
    set key [string trim $config_key]
    if {$key eq "" || [info exists busy(config)]} { return 0 }
    if {$config_hash eq ""} {
        _config_result [::vi18n::mc {Read the key first: a write names the value it replaces.}]
        return 0
    }
    set message [expr {$unset
        ? [format [::vi18n::mc {Unset %1$s on %2$s?}] $key $config_name]
        : [format [::vi18n::mc {Set %1$s on %2$s to %3$s?}] $key $config_name $config_value]}]
    if {![::vinstpane::_confirm $message [::vi18n::mc {It is written into that node's config.toml as an edit made there; config sync then carries it to the nodes that sync config with it.}]]} {
        _config_result [::vi18n::mc {Write cancelled.}]
        return 0
    }
    set body [dict create node $config_node key [::vapi::json_string $key] \
        expect [::vapi::json_string $config_hash] confirm [::vapi::json_bool 1]]
    if {$unset} {
        dict set body unset [::vapi::json_bool 1]
    } else {
        dict set body raw [::vapi::json_string $config_value]
    }
    set busy(config) 1
    if {[catch {
        ::vapi::post_json_async /api/fleet/config $body \
            ::vfleetmanage::_config_failed ::vfleetmanage::_config_write_done
    } err]} {
        _config_failed $err
        return 0
    }
    return 1
}

proc ::vfleetmanage::_config_write_done {resp} {
    variable busy
    variable config_hash
    unset -nocomplain busy(config)
    set config_hash [::vapi::field $resp hash ""]
    variable config_name
    set lines [list [format [::vi18n::mc {%1$s: %2$s}] $config_name [::vapi::field $resp result ""]]]
    set note [::vapi::field $resp note ""]
    if {$note ne "" && $note ne "null"} { lappend lines $note }
    _config_result [join $lines \n]
}

# ── bulk ─────────────────────────────────────────────────────────────

# Probe all / Sync all of a kind: the gateway lists the targets, one
# confirmation names them, and only then does it run.
proc ::vfleetmanage::bulk {action {kind store}} {
    variable busy
    if {[info exists busy(bulk)]} { return 0 }
    set busy(bulk) 1
    set body [dict create action $action kind $kind]
    if {[catch {
        ::vapi::post_json_async /api/fleet/bulk $body \
            [list ::vfleetmanage::_bulk_failed $action] \
            [list ::vfleetmanage::_bulk_planned $action $kind]
    } err]} {
        _bulk_failed $action $err
        return 0
    }
    return 1
}

proc ::vfleetmanage::_bulk_failed {action why} {
    variable busy
    unset -nocomplain busy(bulk)
    ::vstatus::set_msg [format [::vi18n::mc {Bulk %1$s failed: %2$s}] $action $why]
}

# The one confirmation's text; pure, for the tests.
proc ::vfleetmanage::bulk_question {action kind targets} {
    set names {}
    foreach t $targets { lappend names [::vapi::field $t name ?] }
    set message [expr {$action eq "sync"
        ? [format [::vi18n::mc {Sync %1$s to %2$s node(s)?}] $kind [llength $names]]
        : [format [::vi18n::mc {Probe %s node(s)?}] [llength $names]]}]
    return [list $message [join $names ", "]]
}

proc ::vfleetmanage::_bulk_planned {action kind resp} {
    variable busy
    lassign [bulk_question $action $kind [::vapi::field $resp targets {}]] message detail
    if {![::vinstpane::_confirm $message $detail]} {
        unset -nocomplain busy(bulk)
        ::vstatus::set_msg [::vi18n::mc {Bulk action cancelled.}]
        return 0
    }
    set body [dict create action $action kind $kind confirm [::vapi::json_bool 1]]
    if {[catch {
        ::vapi::post_json_async /api/fleet/bulk $body \
            [list ::vfleetmanage::_bulk_failed $action] ::vfleetmanage::_bulk_done
    } err]} {
        _bulk_failed $action $err
        return 0
    }
    ::vstatus::set_msg [format [::vi18n::mc {running %s on the listed nodes…}] $action]
    return 1
}

proc ::vfleetmanage::bulk_line {resp} {
    set parts {}
    foreach r [::vapi::field $resp results {}] {
        set mark [expr {[::vapi::field $r ok 0] in {true 1} ? "✓" : "✗"}]
        lappend parts "$mark [::vapi::field $r name ?]"
    }
    return [format [::vi18n::mc {Bulk %1$s: %2$s (%3$s).}] [::vapi::field $resp action ?] \
        [::vapi::field $resp result ""] [join $parts {, }]]
}

proc ::vfleetmanage::_bulk_done {resp} {
    variable busy
    unset -nocomplain busy(bulk)
    ::vstatus::set_msg [bulk_line $resp]
    catch { ::vinstpane::refresh_fleet }
}

# "Sync all…" asks which kind first.
proc ::vfleetmanage::open_bulk_sync {} {
    set dlg .fleetbulksync
    if {![winfo exists $dlg]} {
        toplevel $dlg
        catch { wm transient $dlg . }
        wm title $dlg [::vi18n::mc {Sync all}]
        ttk::label $dlg.l -text [::vi18n::mc {Push this kind to every node that is a configured peer for it:}] \
            -wraplength 360 -justify left
        pack $dlg.l -side top -anchor w -padx 8 -pady {8 4}
        foreach kind {store memory config vault} {
            ttk::radiobutton $dlg.k$kind -text [::vinstpane::_kind_label $kind] \
                -value $kind -variable ::vfleetmanage::bulk_kind
            pack $dlg.k$kind -side top -anchor w -padx 16
        }
        ttk::frame $dlg.btns
        ttk::button $dlg.btns.go -text [::vi18n::mc {List targets…}] -command {
            destroy .fleetbulksync
            ::vfleetmanage::bulk sync $::vfleetmanage::bulk_kind
        }
        ttk::button $dlg.btns.close -text [::vi18n::mc {Close}] -command [list destroy $dlg]
        pack $dlg.btns.go $dlg.btns.close -side left -padx 4
        pack $dlg.btns -side top -anchor w -padx 6 -pady 8
        bind $dlg <Escape> [list destroy $dlg]
        ::vplace::centered $dlg 400 240
    }
    return $dlg
}

# ── the audit trail ─────────────────────────────────────────────────

proc ::vfleetmanage::audit_lines {resp} {
    set lines {}
    foreach r [::vapi::field $resp records {}] {
        set node [::vapi::field $r node ""]
        if {$node eq "" || $node eq "null"} { set node "-" }
        set mark [expr {[::vapi::field $r ok 0] in {true 1} ? "✓" : "✗"}]
        lappend lines "[::vapi::field $r at ?]  $mark [::vapi::field $r who ?] · [::vapi::field $r action ?] · $node · [::vapi::field $r result {}]"
    }
    if {![llength $lines]} {
        lappend lines [::vi18n::mc {No command-center action is recorded on this gateway yet.}]
    }
    return $lines
}

proc ::vfleetmanage::open_audit {} {
    set dlg .fleetaudit
    if {![winfo exists $dlg]} {
        toplevel $dlg
        catch { wm transient $dlg . }
        wm title $dlg [::vi18n::mc {Command-center audit log}]
        ttk::frame $dlg.btns
        ttk::button $dlg.btns.refresh -text [::vi18n::mc {Refresh}] -command ::vfleetmanage::refresh_audit
        ttk::button $dlg.btns.close -text [::vi18n::mc {Close}] -command [list destroy $dlg]
        pack $dlg.btns.refresh $dlg.btns.close -side left -padx 4
        # A small requested size: the dialog's size decides, so a phone
        # can fit it (the text's default 80 columns asked 659 px).
        text $dlg.body -wrap word -state disabled -font {monospace 9} -width 30 -height 8 \
            -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
            -yscrollcommand [list $dlg.sy set]
        ttk::scrollbar $dlg.sy -orient vertical -command [list $dlg.body yview]
        pack $dlg.btns -side top -anchor w -padx 6 -pady 6
        pack $dlg.sy -side right -fill y
        pack $dlg.body -side left -fill both -expand 1
        bind $dlg <Escape> [list destroy $dlg]
        catch { ::va11y::name $dlg.body [::vi18n::mc {command-center audit log}] }
        ::vplace::centered $dlg 760 420
    }
    refresh_audit
    return $dlg
}

proc ::vfleetmanage::refresh_audit {} {
    if {[catch {
        ::vapi::get_json_async /api/fleet/audit?limit=200 ::vfleetmanage::_audit_done
    } err]} {
        _audit_done 0 $err
    }
}

proc ::vfleetmanage::_audit_done {ok resp} {
    set w .fleetaudit.body
    if {![winfo exists $w]} { return }
    $w configure -state normal
    $w delete 1.0 end
    if {$ok} {
        $w insert end [join [audit_lines $resp] \n]
    } else {
        $w insert end [format [::vi18n::mc {Audit log unavailable: %s}] $resp]
    }
    $w configure -state disabled
}

# ── alerts in the window ─────────────────────────────────────────────

# Poll the attached gateway's health alerts every 30 s on every tab: a
# change since the last answer is shown in the window (a notice at the
# top right and the status bar), and the Instances tab lists it as a row.
# The first answer is the baseline: opening the desktop does not replay
# the alerts the gateway already had.
proc ::vfleetmanage::alerts_start {} {
    variable alerts_after
    if {$alerts_after ne ""} { catch { after cancel $alerts_after } }
    alerts_poll
}

proc ::vfleetmanage::alerts_poll {} {
    variable alerts_after
    variable alerts_error
    set alerts_after [after 30000 ::vfleetmanage::alerts_poll]
    if {[llength [info commands ::vapi::unattached]] && [::vapi::unattached]} { return }
    if {[catch { ::vapi::get_json_async /api/fleet/alerts ::vfleetmanage::_alerts_answer 5000 } why]} {
        # Not sent at all; the next poll asks again.
        set alerts_error $why
    } else {
        set alerts_error ""
    }
}

# The alerts newer than `seen` (RFC 3339 times compare as text), oldest
# first; pure, for the tests.
proc ::vfleetmanage::new_alerts {recent seen} {
    set out {}
    foreach a $recent {
        if {[string compare [::vapi::field $a at ""] $seen] > 0} { lappend out $a }
    }
    return [lreverse $out]
}

proc ::vfleetmanage::_alerts_answer {ok resp} {
    variable alerts_seen
    variable alerts_primed
    if {!$ok} { return }
    set recent [::vapi::field $resp recent {}]
    set fresh [new_alerts $recent $alerts_seen]
    foreach a $recent {
        set at [::vapi::field $a at ""]
        if {[string compare $at $alerts_seen] > 0} { set alerts_seen $at }
    }
    if {!$alerts_primed} {
        set alerts_primed 1
        return
    }
    foreach a $fresh { notify [::vapi::field $a text ""] }
    if {[llength $fresh]} { catch { ::vinstpane::refresh_fleet } }
}

# A notice in the window: the status bar, and a label over the top right
# corner of the main window for ten seconds (wrapped to the window on a
# phone).
proc ::vfleetmanage::notify {text} {
    variable toast_after
    variable last_toast
    if {$text eq ""} { return }
    set last_toast $text
    ::vstatus::set_msg $text
    set w .fleettoast
    if {![winfo exists $w]} {
        label $w -justify left -anchor w -padx 10 -pady 6 -relief solid -borderwidth 1 \
            -background $::vt::T(bg_panel) -foreground $::vt::T(fg)
        bind $w <Button-1> [list place forget $w]
    }
    set width [winfo width .]
    if {$width <= 1} { set width 480 }
    $w configure -text "⚠ $text" -wraplength [expr {max(160, min(420, $width - 40))}] ;# i18n-exempt: a warning sign and the gateway's own alert text
    # Below the tab bar: above it sit the window's own controls (the
    # desktop draws its title bar), which a notice must not cover.
    set y 12
    if {[winfo exists .tabbar] && [winfo ismapped .tabbar]} {
        set y [expr {[winfo rooty .tabbar] - [winfo rooty .] + [winfo height .tabbar] + 8}]
    }
    place $w -in . -relx 1.0 -x -12 -y $y -anchor ne
    raise $w
    if {$toast_after ne ""} { catch { after cancel $toast_after } }
    set toast_after [after 10000 [list catch [list place forget $w]]]
}

# ── deployment consistency ───────────────────────────────────────────

# The deployment-consistency verdict (/api/fleet and /api/fleet/drift
# `consistency`): version, revision, modules, plugins, drivers and user
# plugins shadowing bundled ones, compared across every node. With a
# node id, only what concerns that node follows the verdict. "The fleet
# is consistent" is said only when the gateway said so: every node was
# compared, none differs and every item is on every node. An item some
# node lacks is named with the nodes that lack it, and the verdict is
# then "consistent with differences".
proc ::vfleetmanage::consistency_lines {c {node_id ""}} {
    if {$c eq "" || [::vapi::field $c checked 0] ni {true 1}} {
        return [list [::vi18n::mc {Deployment consistency: not checked yet (Drift… compares every node).}]]
    }
    set flagged [::vapi::field $c flagged {}]
    set unknown [::vapi::field $c unknown {}]
    set missing [::vapi::field $c missing {}]
    if {[::vapi::field $c consistent 0] in {true 1}} {
        set lines [list [format [::vi18n::mc {The fleet is consistent: %s node(s) run the same build, modules and plugins.}] \
            [::vapi::field $c compared 0]]]
    } elseif {[llength $flagged]} {
        set lines [list [format [::vi18n::mc {The fleet is NOT consistent: %s node(s) differ.}] [llength $flagged]]]
    } elseif {[llength $unknown] || ![llength $missing]} {
        set lines [list [format [::vi18n::mc {Deployment consistency unknown: %s node(s) could not be compared.}] [llength $unknown]]]
    } else {
        set lines [list [format [::vi18n::mc {The fleet is consistent with differences: %1$s node(s) run the same build and agree on everything they share; %2$s item(s) are not on every node.}] \
            [::vapi::field $c compared 0] [llength $missing]]]
    }
    foreach f $flagged {
        if {$node_id ne "" && [::vapi::field $f id ""] ne $node_id} { continue }
        lappend lines "  [format [::vi18n::mc {%1$s differs: %2$s}] [::vapi::field $f name ?] [join [::vapi::field $f differs {}] {; }]]"
    }
    foreach u $unknown {
        if {$node_id ne "" && [::vapi::field $u id ""] ne $node_id} { continue }
        lappend lines "  [format [::vi18n::mc {%1$s not compared: %2$s}] [::vapi::field $u name ?] [::vapi::field $u why ?]]"
    }
    # Each item not on every node, with the nodes that lack it; in a
    # node's detail, only the items that node lacks.
    foreach m $missing {
        set ids {}
        set names {}
        foreach n [::vapi::field $m lacking {}] {
            lappend ids [::vapi::field $n id ""]
            lappend names [::vapi::field $n name ?]
        }
        if {$node_id ne "" && $node_id ni $ids} { continue }
        lappend lines "  [format [::vi18n::mc {%1$s is not on: %2$s}] [::vapi::field $m item ?] [join $names {, }]]"
    }
    foreach s [::vapi::field $c shadows {}] {
        if {$node_id ne "" && [::vapi::field $s id ""] ne $node_id} { continue }
        if {[::vapi::field $s same_content 0] ni {true 1}} { continue }
        lappend lines "  [format [::vi18n::mc {%1$s: user plugin %2$s shadows the bundled one with the same content (%3$s)}] \
            [::vapi::field $s name ?] [::vapi::field $s plugin ?] [::vapi::field $s user_sha256 ?]]"
    }
    return $lines
}
