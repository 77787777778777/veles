# veles-desktop/lib/sessions.tcl — the Sessions window.
#
# USER (2026-10-04): "Sessions list has overflown due to so many
# translation sessions for our website, `/sessions` doesn't show full
# list. Should show a window with a session search+explore + all the
# session actions available."
#
# The sidebar shows one page, the newest; a batch run of 1,770 untitled
# `veles chat -q` sessions pushed everything else off it. This window
# reaches every session: a search over titles, ids and first lines (or
# the message text), a titled/untitled filter, paging, a read-only look
# at the selected session's opening, and the session actions — on the
# selected row, and delete on many at once.
#
# Every fetch is asynchronous and generation-checked: typing a search
# while a reply is in flight never paints the stale reply, and nothing
# here waits on the gateway inside the event loop.

package require http

namespace eval ::vsessions {
    variable q ""
    variable fts 0
    variable kind all
    variable offset 0
    variable total 0
    variable page 200
    variable gen 0
    variable pgen 0
    variable search_after ""
    variable preview_after ""
    # What the last action did, kept for the reload that follows it —
    # which would otherwise overwrite it with the page count at once.
    variable note ""
}

proc ::vsessions::_win {} { return .ov_sessions }
proc ::vsessions::_tree {} { return [_win].pw.list.t }

proc ::vsessions::build {w} {
    set bar [::vmanage::window $w [::vi18n::mc {Sessions}] 980x640 {560 360}]
    ::vmanage::button $bar open [::vi18n::mc {Open}] ::vsessions::open_selected
    ::vmanage::button $bar rename [::vi18n::mc {Rename…}] ::vsessions::rename_selected
    ::vmanage::button $bar pin [::vi18n::mc {Pin / unpin}] ::vsessions::pin_selected
    ::vmanage::button $bar fork [::vi18n::mc {Fork}] ::vsessions::fork_selected
    ::vmanage::button $bar export [::vi18n::mc {Export…}] ::vsessions::export_selected
    ::vmanage::button $bar copy [::vi18n::mc {Copy session id}] ::vsessions::copy_selected
    ::vmanage::button $bar stop [::vi18n::mc {Stop}] ::vsessions::stop_selected
    ::vmanage::separator $bar s1
    ::vmanage::button $bar del [::vi18n::mc {Delete}] ::vsessions::delete_selected

    ttk::frame $w.find
    ttk::entry $w.find.q -textvariable ::vsessions::q
    catch { ::va11y::name $w.find.q [::vi18n::mc {search titles, ids and first lines}] }
    ttk::checkbutton $w.find.fts -text [::vi18n::mc {Search message text}] \
        -variable ::vsessions::fts -command ::vsessions::search
    ttk::combobox $w.find.kind -state readonly -width 16 -values [list \
        [::vi18n::mc {All sessions}] [::vi18n::mc {Titled}] [::vi18n::mc {Untitled}]]
    $w.find.kind current 0
    bind $w.find.kind <<ComboboxSelected>> ::vsessions::_kind_changed
    ttk::button $w.find.go -text [::vi18n::mc {Refresh}] -command ::vsessions::search
    pack $w.find.q -side left -fill x -expand 1 -padx {0 6}
    pack $w.find.fts $w.find.kind $w.find.go -side left -padx {0 6}
    pack $w.find -fill x -padx 8 -pady {6 4}
    bind $w.find.q <KeyRelease> ::vsessions::search_soon
    bind $w.find.q <Return> ::vsessions::search

    ttk::panedwindow $w.pw -orient vertical
    set tf [::vmanage::tree $w.pw list {title updated messages state} \
        [list [::vi18n::mc {Title}] [::vi18n::mc {Last active}] \
            [::vi18n::mc {Messages}] [::vi18n::mc {running}]] \
        {520 150 80 80}]
    $tf.t configure -selectmode extended
    $tf.t tag configure dim -foreground $::vt::T(fg_dim)
    catch { ::va11y::name $tf.t [::vi18n::mc {Sessions}] }
    ttk::frame $w.pw.pvf
    text $w.pw.pvf.t -wrap word -height 8 -background $::vt::T(bg_panel) \
        -foreground $::vt::T(fg) -relief flat -borderwidth 0 -highlightthickness 0
    $w.pw.pvf.t tag configure role -foreground $::vt::T(accent)
    $w.pw.pvf.t configure -state disabled
    catch { ::va11y::name $w.pw.pvf.t [::vi18n::mc {Preview}] }
    ttk::scrollbar $w.pw.pvf.sb -orient vertical -command [list $w.pw.pvf.t yview]
    $w.pw.pvf.t configure -yscrollcommand [list $w.pw.pvf.sb set]
    pack $w.pw.pvf.sb -side right -fill y
    pack $w.pw.pvf.t -side left -fill both -expand 1
    $w.pw add $tf -weight 3
    $w.pw add $w.pw.pvf -weight 1
    pack $w.pw -fill both -expand 1 -padx 8 -pady 4

    ttk::frame $w.foot
    ttk::button $w.foot.more -text [::vi18n::mc {Load more}] \
        -command [list ::vsessions::load 1]
    pack $w.foot.more -side left
    pack $w.foot -fill x -padx 8 -pady {0 8}

    bind $tf.t <<TreeviewSelect>> ::vsessions::preview_soon
    bind $tf.t <Double-1> ::vsessions::open_selected
    bind $tf.t <Return> ::vsessions::open_selected
    bind $tf.t <Delete> ::vsessions::delete_selected
    ::vctx::attach $tf.t {
        {"Open"            {::vsessions::open_selected}}
        {-}
        {"Copy session id" {::vsessions::copy_selected}}
        {"Rename…"         {::vsessions::rename_selected}}
        {"Pin / unpin"     {::vsessions::pin_selected}}
        {"Fork"            {::vsessions::fork_selected}}
        {"Export…"         {::vsessions::export_selected}}
        {"Stop"            {::vsessions::stop_selected}}
        {-}
        {"Delete"          {::vsessions::delete_selected}}
    }
    focus $w.find.q
    load 0
}

proc ::vsessions::open {} { ::voverlay::open sessions }

proc ::vsessions::_kind_changed {} {
    variable kind
    set kind [lindex {all titled untitled} [[_win].find.kind current]]
    search
}

# Typing searches after a pause, not per key.
proc ::vsessions::search_soon {} {
    variable search_after
    after cancel $search_after
    set search_after [after 300 ::vsessions::search]
}

proc ::vsessions::search {} { load 0 }

# The query string for one page.
proc ::vsessions::_query {off} {
    variable q
    variable fts
    variable kind
    variable page
    set args [list limit $page offset $off]
    if {[string trim $q] ne ""} { lappend args q [string trim $q] }
    if {$fts} { lappend args fts 1 }
    if {$kind ne "all"} { lappend args titled $kind }
    return "/api/sessions?[::http::formatQuery {*}$args]"
}

# Fetch a page. `more` appends the next page; otherwise the list starts
# over. Replies from an older search are dropped by generation.
proc ::vsessions::load {more} {
    variable gen
    variable offset
    if {![winfo exists [_win]]} { return }
    if {[::vapi::unattached]} { return }
    set off [expr {$more ? $offset : 0}]
    set g [incr gen]
    ::vapi::get_json_async [_query $off] [list ::vsessions::_loaded $g $more]
}

proc ::vsessions::_loaded {g more ok resp} {
    variable gen
    variable offset
    variable total
    if {$g != $gen || ![winfo exists [_win]]} { return }
    set w [_win]
    if {!$ok} {
        ::vmanage::say $w [format [::vi18n::mc {unreachable: %s}] [::vmanage::_reason $resp]] warn
        return
    }
    set t [_tree]
    if {!$more} {
        $t delete [$t children {}]
        set offset 0
    }
    set rows [dict get $resp sessions]
    catch { set total [dict get $resp total] }
    foreach s $rows {
        set id [dict get $s id]
        if {[$t exists $id]} { continue }
        set title [::vapi::field $s title]
        set tags {}
        if {$title eq ""} {
            set title [string map {"\n" " "} [::vapi::field $s preview]]
            if {$title eq ""} { set title $id }
            set tags dim
        }
        if {[::vsessions::_pinned $id]} { set title "★ $title" }
        set state [expr {[::vapi::field $s running] in {1 true} ? [::vi18n::mc {running}] : ""}]
        $t insert {} end -id $id -tags $tags -values [list $title \
            [_when [::vapi::field $s updated_at]] [::vapi::field $s message_count] $state]
    }
    incr offset [llength $rows]
    [_win].foot.more configure -state [expr {$offset < $total ? "normal" : "disabled"}]
    variable note
    set msg [format [::vi18n::mc {%1$s of %2$s sessions}] $offset $total]
    if {$note ne ""} {
        set msg "$note · $msg"
        set note ""
        ::vmanage::say $w $msg ok
    } else {
        ::vmanage::say $w $msg
    }
}

# The store keeps UTC; the window shows local time.
proc ::vsessions::_when {utc} {
    if {[catch {clock scan $utc -format "%Y-%m-%d %H:%M:%S" -gmt 1} s]} { return $utc }
    return [clock format $s -format "%Y-%m-%d %H:%M"]
}

proc ::vsessions::_pinned {sid} {
    if {![info exists ::vsidebar::pins]} { return 0 }
    return [expr {[lsearch -exact $::vsidebar::pins $sid] >= 0}]
}

# The selected session ids, or "" with a nudge.
proc ::vsessions::_selected {{many 0}} {
    set sel [[_tree] selection]
    if {![llength $sel]} {
        ::vmanage::say [_win] [format [::vi18n::mc {select %s first}] [::vi18n::mc {Sessions}]]
        return ""
    }
    return [expr {$many ? $sel : [lindex $sel 0]}]
}

# ── the preview ─────────────────────────────────────────────────────

proc ::vsessions::preview_soon {} {
    variable preview_after
    after cancel $preview_after
    set preview_after [after 150 ::vsessions::preview]
}

proc ::vsessions::preview {} {
    variable pgen
    set sel [[_tree] selection]
    if {[llength $sel] != 1} { return }
    set g [incr pgen]
    ::vapi::get_json_async "/api/sessions/[lindex $sel 0]/head?n=16" \
        [list ::vsessions::_previewed $g]
}

proc ::vsessions::_previewed {g ok resp} {
    variable pgen
    if {$g != $pgen || ![winfo exists [_win]]} { return }
    set pv [_win].pw.pvf.t
    $pv configure -state normal
    $pv delete 1.0 end
    if {!$ok} {
        $pv insert end [::vmanage::_reason $resp]
    } else {
        foreach m [dict get $resp messages] {
            $pv insert end "[dict get $m role]: " role
            $pv insert end "[string trim [dict get $m content]]\n\n"
        }
    }
    $pv configure -state disabled
}

# ── actions ─────────────────────────────────────────────────────────

proc ::vsessions::open_selected {} {
    set sid [_selected]
    if {$sid eq ""} { return }
    ::vchat::open_session $sid
}

proc ::vsessions::copy_selected {} {
    set sid [_selected]
    if {$sid eq ""} { return }
    clipboard clear
    clipboard append $sid
    ::vmanage::say [_win] [format [::vi18n::mc {copied: %s}] $sid] ok
}

proc ::vsessions::rename_selected {} {
    set sid [_selected]
    if {$sid eq ""} { return }
    set w [_win]
    set t [::vmanage::ask $w [::vi18n::mc {Rename session}] \
        [format [::vi18n::mc {New title for %s:}] $sid]]
    if {$t eq ""} { return }
    if {[::vmanage::post $w "/api/sessions/$sid/title" \
            [dict create title [::vapi::json_string $t]]] ne ""} {
        load 0
        catch { ::vsidebar::refresh_soon }
    }
}

proc ::vsessions::pin_selected {} {
    set sid [_selected]
    if {$sid eq ""} { return }
    catch { ::vsidebar::toggle_pin $sid }
    load 0
}

proc ::vsessions::fork_selected {} {
    set sid [_selected]
    if {$sid eq ""} { return }
    set resp [::vmanage::post [_win] "/api/sessions/$sid/branch" [dict create]]
    if {$resp eq ""} { return }
    ::vmanage::say [_win] [format [::vi18n::mc {branched → %s (select from sidebar)}] \
        [dict get $resp new_session_id]] ok
    load 0
    catch { ::vsidebar::refresh_soon }
}

proc ::vsessions::export_selected {} {
    set sid [_selected]
    if {$sid eq ""} { return }
    if {[catch {::vchat::export_session $sid markdown} r]} {
        ::vmanage::say [_win] [::vi18n::mc {export failed: %s} $r] warn
        return
    }
    ::vmanage::say [_win] [format [::vi18n::mc {exported to %s}] $r] ok
}

proc ::vsessions::stop_selected {} {
    set sid [_selected]
    if {$sid eq ""} { return }
    ::vmanage::post [_win] "/api/sessions/$sid/cancel" [dict create]
    load 0
}

proc ::vsessions::delete_selected {} {
    set ids [_selected 1]
    if {$ids eq ""} { return }
    set w [_win]
    if {[llength $ids] == 1} {
        set q [format [::vi18n::mc {Delete session %s and all its messages?}] [lindex $ids 0]]
    } else {
        set q [format [::vi18n::mc {Delete %s sessions and all their messages?}] [llength $ids]]
    }
    if {![::vmanage::confirm $w [::vi18n::mc {Delete session}] $q]} { return }
    set resp [::vmanage::post $w /api/sessions/delete \
        [dict create ids [::vapi::json_strings $ids]]]
    if {$resp eq ""} { return }
    foreach sid $ids { catch { ::vchat::clear_if_current $sid } }
    variable note
    set note [format [::vi18n::mc {deleted %s session(s)}] [dict get $resp deleted]]
    ::vmanage::say $w $note ok
    load 0
    catch { ::vsidebar::refresh_soon }
}
