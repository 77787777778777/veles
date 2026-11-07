# Review standing tool-approval permissions, not pending asks.
# Global grants are durable store rules; session grants belong to a live
# agent (desktop-hosted or a gateway chat: Telegram, IRC, …) and
# disappear with that session; `[approvals] allowlist` entries come from
# config.toml and are listed read-only. Revoke always uses an exact key.
namespace eval ::vgrants {
    variable rows {}
}

proc ::vgrants::build {w} {
    toplevel $w
    wm title $w [::vi18n::mc {Permission grants}]
    wm geometry $w 960x520
    wm minsize $w 600 300
    ::voverlay::_header $w [::vi18n::mc {Permission grants}]
    bind $w <Escape> [list destroy $w]
    ttk::frame $w.bar
    pack $w.bar -fill x -padx 8 -pady 6
    ttk::button $w.bar.refresh -text [::vi18n::mc {Refresh}] -command [list ::vgrants::load $w]
    ttk::button $w.bar.revoke -text [::vi18n::mc {Revoke selected}] -command [list ::vgrants::revoke_selected $w]
    ttk::label $w.bar.status -text "" -foreground $::vt::T(fg_dim)
    pack $w.bar.refresh $w.bar.revoke -side left -padx {0 6}
    pack $w.bar.status -side left -fill x -expand 1
    ttk::label $w.help -wraplength 900 -justify left -foreground $::vt::T(fg_dim) \
        -text [::vi18n::mc {Standing permissions allow later tool calls without another ask. Session grants are held by live sessions on every surface (desktop, Telegram, IRC, …); global grants persist; [approvals] allowlist entries from config.toml are listed read-only. Revoking one never changes the other grants.}]
    pack $w.help -fill x -padx 10 -pady {0 6}
    ttk::frame $w.body
    pack $w.body -fill both -expand 1 -padx 8 -pady 6
    ttk::treeview $w.body.t -columns {scope surface session rule created note} -show headings -selectmode browse
    foreach {col title width} {
        scope {Scope} 70 surface {Surface} 90 session {Session} 190
        rule {Exact permission rule} 300 created {Granted at} 145 note {Note} 200
    } {
        $w.body.t heading $col -text [::vi18n::mc $title]
        $w.body.t column $col -width $width -stretch [expr {$col in {rule note}}]
    }
    ttk::scrollbar $w.body.s -orient vertical -command [list $w.body.t yview]
    $w.body.t configure -yscrollcommand [list $w.body.s set]
    pack $w.body.s -side right -fill y
    pack $w.body.t -side left -fill both -expand 1
    ::va11y::name $w.body.t [::vi18n::mc {standing global and session permission grants}]
    ::va11y::tab_order [list $w.bar.refresh $w.bar.revoke $w.body.t]
    load $w
    focus $w.body.t
    return $w
}

proc ::vgrants::paint {w data} {
    variable rows
    set rows {}
    $w.body.t delete [$w.body.t children {}]
    set index 0
    foreach row [dict get $data global] {
        dict set row scope global
        lappend rows $row
        # json2dict renders a JSON null as the word "null".
        set note ""
        catch { set note [dict get $row note] }
        if {$note eq "null"} { set note "" }
        $w.body.t insert {} end -id grant-$index -values [list global "" "" \
            [dict get $row rule_key] [dict get $row created_at] $note]
        incr index
    }
    foreach row [dict get $data sessions] {
        dict set row scope session
        lappend rows $row
        set surface ""
        catch { set surface [dict get $row surface] }
        $w.body.t insert {} end -id grant-$index -values [list session $surface \
            [dict get $row session_id] [dict get $row rule_key] "" ""]
        incr index
    }
    # An older gateway sends no config list.
    set config {}
    catch { set config [dict get $data config] }
    foreach row $config {
        dict set row scope config
        lappend rows $row
        $w.body.t insert {} end -id grant-$index -values [list config config.toml "" \
            [dict get $row rule_key] "" [::vi18n::mc {read-only: [approvals] allowlist}]]
        incr index
    }
    ::vi18n::label $w.bar.status {%s standing grants} [llength $rows]
}

proc ::vgrants::load {w} {
    if {![winfo exists $w]} { return 0 }
    if {[catch {::vapi::get_json /api/approvals/grants} data]} {
        ::vi18n::label $w.bar.status {failed: %s} $data
        return 0
    }
    paint $w $data
    return 1
}

proc ::vgrants::revoke_selected {w} {
    variable rows
    set selected [$w.body.t selection]
    if {![llength $selected]} {
        ::vi18n::label $w.bar.status {select a grant first}
        return 0
    }
    set id [lindex $selected 0]
    if {![string match grant-* $id]} { return 0 }
    set row [lindex $rows [string range $id 6 end]]
    if {[dict get $row scope] eq "config"} {
        # config.toml is the source of truth; the panel never rewrites it.
        ::vi18n::label $w.bar.status {an allowlist entry is removed by editing config.toml}
        return 0
    }
    set body [dict create scope [dict get $row scope] rule_key [dict get $row rule_key]]
    if {[dict get $row scope] eq "session"} {
        dict set body session_id [dict get $row session_id]
    }
    if {[catch {::vapi::delete_json /api/approvals/grants $body} result]} {
        ::vi18n::label $w.bar.status {failed: %s} $result
        return 0
    }
    if {![dict exists $result revoked] || ![dict get $result revoked]} {
        ::vi18n::label $w.bar.status {revoke refused}
        return 0
    }
    load $w
    return 1
}
