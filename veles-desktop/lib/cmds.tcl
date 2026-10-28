# cmds.tcl — the engine's slash-command table, fetched once.
#
# WHY THIS EXISTS
#
# The desktop kept THREE hand-maintained copies of the command list:
# `chat.tcl`'s `/help` literal, `complete.tcl`'s `_slash_candidates`
# ("Mirror of chat.tcl's slash_command dispatch") and the switch in
# `chat.tcl` itself. Three copies of a list that grows in a fourth
# place — `frontends/commands.rs` — is not a list, it is a rumour, and
# they had already drifted: they advertised `/config` and `/verbose`
# while omitting `/queue`, `/context`, `/secret`, `/guards`,
# `/sessions` and `/cron`, all of which work.
#
# So: the engine serves its own table at `/api/commands/builtin`, with
# each row's required capability, and this module is the desktop's one
# reader of it. The only hand-maintained list left is
# `desktop_local` — commands that exist ONLY here (there is no
# `/keymap` in the engine) — and it is small enough to see all of.

namespace eval ::vcmds {
    # Cached engine rows: a list of dicts {usage name desc cap}.
    variable engine {}
    variable fetched 0

    # Commands that live only in the desktop. Not drift: the engine has
    # no window to redraw, no clipboard and no keymap.
    #
    # The DESCRIPTION column is a catalog key (USER report 2026-09-26:
    # "the help text in most places also seems to be untranslated").
    # ::vcmds::help_text resolves it through mc(): an engine row's
    # description arrives already localized and is not a key, so mc()
    # returns it unchanged — which is why ONE resolution covers both
    # halves of the list, and why these rows are brace groups like every
    # other key in the tree.
    variable desktop_local {
        {/help {} {this list}}
        {/theme {name} {switch or list themes}}
        {/config {} {show current configuration}}
        {/source {} {toggle markdown source view}}
        {/preview {} {toggle composer preview}}
        {/log {} {open the engine log}}
        {/keymap {normie|emacs|vi} {switch keybinding scheme}}
        ;# The desktop-local face of /locale: the engine command switches
        ;# the gateway's locale (and every text surface it serves), while
        ;# THIS row opens the picker that switches the window you are
        ;# looking at. Two different processes, two different switches —
        ;# the picker's "Save as default" is what makes them agree.
        {/language {} {language picker (the swap for THIS window)}}
        {/setup {} {open the setup wizard (re-runnable; edits config.toml in place)}}
        {/find {} {find in transcript}}
        {/copy {} {copy last reply}}
        {/paste {} {paste into the composer}}
        {/verbose {} {toggle verbose event output}}
        {/redraw {} {repaint the transcript from memory (use /refetch to reread the store)}}
        {/refetch {} {drop the transcript and reread it from the store (C-S-V too)}}
        {/quit {} {exit veles-desktop}}
    }
}

# Fetch the engine table once per process. A failure leaves the cache
# empty and is NOT retried per keystroke: the completion popup calls
# this, and a dead gateway must not make typing slow.
proc ::vcmds::load {{force 0}} {
    variable engine
    variable fetched
    if {$fetched && !$force} { return $engine }
    set fetched 1
    set engine {}
    if {[catch { ::vapi::get_json /api/commands/builtin } d]} { return $engine }
    if {[catch { dict get $d commands } rows]} { return $engine }
    foreach r $rows {
        set row [dict create usage "" name "" desc "" cap ""]
        foreach k {usage name desc cap} {
            if {[dict exists $r $k]} { dict set row $k [dict get $r $k] }
        }
        if {[dict get $row name] eq ""} { continue }
        lappend engine $row
    }
    return $engine
}

# Every command, engine + desktop-local, as {usage args desc cap} rows
# in the shape the completion wants. Engine rows first, then the
# desktop's own; a name is never listed twice.
proc ::vcmds::all {} {
    variable desktop_local
    set out {}
    set seen {}
    foreach r [load] {
        set name [dict get $r name]
        if {$name in $seen} { continue }
        lappend seen $name
        # `usage` is "/name [args]" — split the args back off, because
        # the completion renders them separately.
        set usage [dict get $r usage]
        set args [string trim [string range $usage [string length $name] end]]
        lappend out [list $name $args [dict get $r desc] [dict get $r cap]]
    }
    foreach r $desktop_local {
        set name [lindex $r 0]
        if {$name in $seen} { continue }
        lappend seen $name
        lappend out [list $name [lindex $r 1] [lindex $r 2] ""]
    }
    return $out
}

# `/help`, rendered. One column of names, one of descriptions, with the
# admin rows marked — discovery used to show every row to everyone,
# which is a lie the moment the gateway refuses one.
proc ::vcmds::help_text {} {
    set rows [all]
    set w 0
    foreach r $rows {
        set u [string trim "[lindex $r 0] [lindex $r 1]"]
        if {[string length $u] > $w} { set w [string length $u] }
    }
    set lines {}
    foreach r $rows {
        set u [string trim "[lindex $r 0] [lindex $r 1]"]
        set cap [lindex $r 3]
        set mark ""
        if {$cap eq "cmd-admin"} { set mark [::vi18n::mc {  (admin)}] }
        # The DESCRIPTION goes through the catalog: the engine's rows
        # arrive already localized (rendered in the request's language),
        # but the desktop-local half — /keymap, /theme, /log, /language —
        # is a hand-written table in cmds.tcl, and its text was English
        # in every locale (USER report 2026-09-26: "the help text in most
        # places also seems to be untranslated"). An engine row's
        # description is not a catalog key, so mc() returns it unchanged,
        # which is exactly right.
        lappend lines [format "%-*s  %s%s" $w $u [::vi18n::mc [lindex $r 2]] $mark]
    }
    if {![llength $lines]} {
        return [::vi18n::mc {command list unavailable (the gateway is not reachable)}]
    }
    return [join $lines "\n"]
}

# The engine's table carries DESCRIPTIONS rendered in the language of the
# request that fetched it (the window's, via X-Veles-Locale). After a
# language change the cached copy is in the old language, so it is marked
# stale — refetched on next use, not now: a swap must not block on the
# gateway, and a dead gateway would otherwise stall the swap.
proc ::vcmds::stale {} {
    variable fetched
    set fetched 0
}
if {[llength [info commands ::vi18n::on_locale_change]]} {
    ::vi18n::on_locale_change ::vcmds::stale
}
