# sandbox.tcl — the named sandboxes overlay.
#
# WHY: `[sandbox]` was config-only, and the desktop had no `user_envs`
# reference at all — the one surface with room for a table of
# containers and who lands in which could not see them. USER
# (2026-09-09): "ALL of this should be manageable not only through the
# config.toml file, but with full first-class support in all
# frontends."
#
# Three tables, because there are three separate facts: what is
# DECLARED (name, image, memory, network), what is ASSIGNED (which
# callers, in which place, land in which box) and what podman actually
# has RUNNING. A box exists implicitly on first use, so "declared" is
# not the same list as "live", and conflating them is how you end up
# purging a container nobody declared and thinking nothing happened.

namespace eval ::vsandbox {
    variable defs {}
    variable binds {}
}

proc ::vsandbox::build {w} {
    variable defs
    variable binds
    toplevel $w
    wm title $w [::vi18n::mc {Sandboxes}]
    wm geometry $w 900x640
    wm minsize $w 520 360
    ::voverlay::_header $w [::vi18n::mc {Sandboxes}]
    bind $w <Escape> [list destroy $w]

    ttk::frame $w.bar
    ttk::label $w.bar.title -text [::vi18n::mc {Sandboxes}]
    ttk::label $w.bar.status -text ""
    ttk::button $w.bar.reload -text [::vi18n::mc {Reload}] \
        -command [list ::vsandbox::load $w]
    pack $w.bar.title -side left -padx 6
    pack $w.bar.reload -side right -padx 4
    pack $w.bar.status -side right -padx 6
    pack $w.bar -fill x -pady {4 2}

    ttk::frame $w.split
    # Declared boxes.
    ttk::treeview $w.split.d -columns {name image memory network note} \
        -show headings -height 6
    foreach {c t} {name Name image Image memory Memory network Network note Note} {
        $w.split.d heading $c -text [::vi18n::mc $t]
    }
    # Assignments.
    ttk::treeview $w.split.b -columns {selectors sandbox} -show headings -height 6
    $w.split.b heading selectors -text [::vi18n::mc {Applies to}]
    $w.split.b heading sandbox -text [::vi18n::mc {Sandbox}]
    # Live containers.
    ttk::treeview $w.split.l -columns {name state workspace} -show headings -height 6
    foreach {c t} {name Name state State workspace Workspace} {
        $w.split.l heading $c -text [::vi18n::mc $t]
    }
    pack $w.split.d -fill both -expand 1 -padx 6 -pady 2
    pack $w.split.b -fill both -expand 1 -padx 6 -pady 2
    pack $w.split.l -fill both -expand 1 -padx 6 -pady 2
    pack $w.split -fill both -expand 1

    ttk::label $w.zone -text "" -justify left
    pack $w.zone -fill x -padx 6 -pady {2 6}

    ::vctx::attach $w.split.d {
        {"Purge container"        {::vsandbox::purge_selected 0}}
        {"Purge + delete files"   {::vsandbox::purge_selected 1}}
        {-}
        {"Forget declaration"     {::vsandbox::drop_selected}}
    }
    ::vctx::attach $w.split.b {
        {"Unassign" {::vsandbox::unassign_selected}}
    }
    load $w
    return $w
}

# `$payload` stands in for the /api/sandboxes body so the renderer is
# testable without HTTP — the same seam ::vacl::load uses.
proc ::vsandbox::load {w {payload ""}} {
    variable defs
    variable binds
    if {$payload eq ""} {
        if {[catch { ::vapi::get_json /api/sandboxes } payload]} {
            catch { ::vi18n::label $w.bar.status {unavailable: %s} $payload }
            return 0
        }
    }
    set defs {}
    set binds {}
    catch { $w.split.d delete [$w.split.d children {}] }
    catch { $w.split.b delete [$w.split.b children {}] }
    catch { $w.split.l delete [$w.split.l children {}] }

    set d_rows {}
    catch { set d_rows [dict get $payload defs] }
    foreach r $d_rows {
        set name [_get $r name]
        dict set defs $name $r
        $w.split.d insert {} end -values [list $name \
            [_get $r image] [_get $r memory_mb] \
            [_bool [_get $r network]] [_get $r note]]
    }
    set b_rows {}
    catch { set b_rows [dict get $payload binds] }
    foreach r $b_rows {
        set sel {}
        foreach k {identity platform network channel role} {
            set v [_get $r $k]
            if {$v ne ""} { lappend sel "$k $v" }
        }
        if {![llength $sel]} { set sel [list [::vi18n::mc {everyone}]] }
        set key [_get $r key]
        dict set binds $key $r
        $w.split.b insert {} end -id $key \
            -values [list [join $sel " + "] [_get $r sandbox]]
    }
    set l_rows {}
    catch { set l_rows [dict get $payload live] }
    foreach r $l_rows {
        set name [_get $r name]
        if {$name eq ""} { set name [_get $r key] }
        set running 0
        catch { set running [dict get $r running] }
        $w.split.l insert {} end -id [_get $r key] -values [list $name \
            [expr {[string is true -strict $running] || $running eq "1"
                   ? [::vi18n::mc {running}] : [::vi18n::mc {stopped}]}] \
            [_get $r workspace]]
    }
    # The posture, which is the thing an admin most needs to see before
    # opting out of it.
    set zone [_get $payload admin_default_zone]
    set local [_get $payload local_default_zone]
    if {$local eq ""} { set local "host" }
    # The exec bound matters here for the same reason the zone does: a
    # shared box is a shared resource, and this is the number that
    # refuses somebody's call when the room is busy.
    #
    # "exec-slots", hyphenated: uitest's `exec`-with-substituted-args
    # audit is a TEXTUAL scan for `exec` as a command word, and
    # `exec slots per box: %3$s` reads to it as an exec of a
    # substituted argument. The hyphen keeps the audit honest instead
    # of adding a message string to its allow-list.
    set slots [_get $payload max_concurrent_exec]
    if {$slots eq "" || $slots eq "0"} { set slots [::vi18n::mc {unbounded}] }
    catch { ::vi18n::label $w.zone {admin default zone: %1$s  ·  local (this window): %2$s  ·  exec-slots per box: %3$s  ·  below admin, the sandbox is not optional} \
        $zone $local $slots }
    return 1
}

proc ::vsandbox::_get {d k} {
    if {[catch { dict get $d $k } v]} { return "" }
    if {$v eq "null"} { return "" }
    return $v
}

proc ::vsandbox::_bool {v} {
    if {$v eq ""} { return "" }
    if {[string is true -strict $v] || $v eq "1"} { return "on" }
    return "off"
}

proc ::vsandbox::_selected_def {w} {
    set sel [lindex [$w.split.d selection] 0]
    if {$sel eq ""} { return "" }
    return [lindex [$w.split.d item $sel -values] 0]
}

proc ::vsandbox::purge_selected {delete_work {w .ov_sandbox}} {
    set name [_selected_def $w]
    if {$name eq ""} {
        catch { $w.bar.status configure -text [::vi18n::mc {select a sandbox first}] }
        return 0
    }
    # Deleting the WORKSPACE deletes people's files. Confirm, the same
    # way the app confirms reverting a file or deleting a session.
    if {$delete_work} {
        set ans [tk_messageBox -type yesno -icon warning \
            -title [::vi18n::mc {Delete workspace?}] \
            -message [format [::vi18n::mc \
                {Delete %s's workspace? Everything anyone put in that box is lost.}] $name]]
        if {$ans ne "yes"} { return 0 }
    }
    _post $w /api/sandboxes/purge \
        [dict create name $name delete_work [::vapi::json_bool $delete_work]]
}

proc ::vsandbox::drop_selected {{w .ov_sandbox}} {
    set name [_selected_def $w]
    if {$name eq ""} {
        catch { $w.bar.status configure -text [::vi18n::mc {select a sandbox first}] }
        return 0
    }
    _post $w /api/sandboxes/drop \
        [dict create name $name delete_work [::vapi::json_bool 0]]
}

proc ::vsandbox::unassign_selected {{w .ov_sandbox}} {
    variable binds
    set key [lindex [$w.split.b selection] 0]
    if {$key eq "" || ![dict exists $binds $key]} {
        catch { $w.bar.status configure -text [::vi18n::mc {select an assignment first}] }
        return 0
    }
    set r [dict get $binds $key]
    set body [dict create sandbox [_get $r sandbox]]
    foreach k {identity network channel platform role} {
        set v [_get $r $k]
        if {$v ne ""} { dict set body $k [::vapi::json_string $v] }
    }
    _post $w /api/sandboxes/unassign $body
}

proc ::vsandbox::_post {w path body} {
    if {[catch { ::vapi::post_json $path $body } resp]} {
        catch { ::vi18n::label $w.bar.status {failed: %s} $resp }
        return 0
    }
    set err ""
    catch { set err [dict get $resp error] }
    if {$err ne ""} {
        catch { ::vi18n::label $w.bar.status {refused: %s} $err }
        return 0
    }
    catch { $w.bar.status configure -text [::vi18n::mc {done}] }
    load $w
    return 1
}
