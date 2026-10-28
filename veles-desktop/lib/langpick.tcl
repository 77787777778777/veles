# veles-desktop/lib/langpick.tcl — the language picker (USER ask
# 2026-09-20: "a dedicated button to swap between them").
#
# Three entry points, one implementation:
#
#   * Palette row "Language…" / `/language`  -> ::vlang::picker
#   * Status-bar chip: click -> ::vlang::swap_click, right-click -> picker
#   * `/locale` in the composer                -> the ENGINE's command,
#     which sets this SESSION's language for engine text on every
#     surface (it does not re-text this window; the picker does)
#
# The picker lists every catalog the build ships (from the msgs/
# directory, so a new .msg shows up with no second edit), previews
# which strings will change, and applies live — msgcat re-resolves on
# every ::vi18n::mc call, and ::vi18n::use re-texts the window.
#
# EVERYTHING the swap needs is settable HERE. The first version could
# only set the swap target indirectly (as a side effect of "Save as
# default"), and the chip answered a click with "set
# tui.locale_secondary" — an instruction to go and edit config.toml by
# hand (USER report, 2026-09-25). Now:
#
#   Use now             this window only, nothing written
#   Make default        tui.locale (the language every start uses)
#   Set as swap target  tui.locale_secondary (what the chip toggles to)
#
# PERSISTENCE is still an explicit act, one button per key, through
# /api/config/set (the engine's in-place editor: comments kept, backup
# written, a broken file refused), and the answer names what it wrote.
# A picker that silently rewrote config.toml is exactly the implicit
# mechanism the transcript rules exist to prevent.

namespace eval ::vlang {
    variable picker_win ""
    variable why ""   ;# why the picker was opened, shown under the state line
}

# Human name for a catalog id: the endonym from ::vi18n::locale_name,
# which the catalogs themselves carry so a speaker recognizes their
# language in a list rendered in a language they may not read.
proc ::vlang::label_for {id} {
    set name [::vi18n::locale_name $id]
    if {$name eq $id} { return $id }
    return "$name  ·  $id"
}

# One line per catalog, active and secondary marked. Built fresh on
# every open so a swap (or a config change) cannot leave a stale mark.
proc ::vlang::rows {} {
    set out {}
    foreach id [::vi18n::available] {
        set marks ""
        if {$id eq [::vi18n::current]} { append marks "◆ " }
        if {$id eq $::vi18n::primary} { append marks "★ " }
        if {$id eq $::vi18n::secondary} { append marks "⇄ " }
        if {$marks eq ""} { set marks "   " }
        lappend out [list $marks [::vlang::label_for $id]]
    }
    return $out
}

proc ::vlang::picker {{why ""}} {
    set w .lang_picker
    if {[winfo exists $w]} { destroy $w }
    toplevel $w
    wm title $w [::vi18n::mc {Language}]
    # Wide enough for all five buttons in the longer locales (the first
    # cut clipped "Close" to "Clo" even in English).
    wm geometry $w "660x470+[expr {[winfo rootx .]+160}]+[expr {[winfo rooty .]+60}]"

    ttk::label $w.hint -text [::vi18n::mc {pick a language (↑↓ Enter · Esc) · ◆ active · ★ default · ⇄ swap target}] \
        -foreground $::vt::T(fg_dim)
    # What is configured right now, in words — and, when the picker was
    # opened BECAUSE something is missing (the chip with no swap target),
    # why. Recomputed by _refresh_rows after every button.
    ttk::label $w.state -text "" -wraplength 630 -justify left
    listbox $w.list -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(select_bg) -selectforeground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -bd 0 -activestyle none
    foreach row [::vlang::rows] {
        lassign $row marks name
        $w.list insert end "$marks$name"
    }
    # Restore the cursor to the active language, so the list opens where
    # the user already is rather than at the top.
    set active [::vi18n::current]
    set i 0
    foreach row [::vlang::rows] {
        lassign $row marks name
        if {[string first "◆" $marks] >= 0} { $w.list selection set $i ; $w.list see $i ; break }
        incr i
    }

    text $w.preview -height 9 -wrap word -background $::vt::T(bg_input) \
        -foreground $::vt::T(fg) -font {TkDefaultFont 9} -bd 0 \
        -highlightthickness 0 -state disabled

    ttk::frame $w.btns
    # One button per act, deliberately: a temporary look at another
    # language must not rewrite the config, a config write must not look
    # like a mere preview, and the default and the swap target are two
    # different keys.
    ttk::button $w.btns.use -text [::vi18n::mc {Use now}] \
        -command [list ::vlang::_use $w]
    ttk::button $w.btns.default -text [::vi18n::mc {Make default}] \
        -command [list ::vlang::_make_default $w]
    ttk::button $w.btns.target -text [::vi18n::mc {Set as swap target}] \
        -command [list ::vlang::_set_target $w]
    ttk::button $w.btns.swap -text [::vi18n::mc {Swap ⇄}] \
        -command [list ::vlang::swap_click]
    ttk::button $w.btns.cancel -text [::vi18n::mc {Close}] \
        -command [list destroy $w]

    pack $w.hint -fill x -padx 8 -pady {8 2}
    pack $w.state -fill x -padx 8 -pady {0 4}
    pack $w.list -fill both -expand 1 -padx 8 -pady 2
    pack $w.preview -fill x -padx 8 -pady {2 4}
    pack $w.btns -fill x -padx 8 -pady {0 8}
    pack $w.btns.use $w.btns.default $w.btns.target $w.btns.swap -side left -padx 2
    pack $w.btns.cancel -side right -padx 2

    ::va11y::tab_order [list $w.list $w.btns.use $w.btns.default $w.btns.target \
        $w.btns.swap $w.btns.cancel]
    catch { ::va11y::name $w.list [::vi18n::mc {language list}] }

    bind $w.list <<ListboxSelect>> [list ::vlang::_preview $w]
    bind $w.list <Return> [list ::vlang::_use $w]
    bind $w <Escape> [list destroy $w]
    set ::vlang::why $why
    ::vlang::_refresh_state $w
    ::vlang::_preview $w
    focus $w.list
}

# The state line: what is default, what the chip swaps to — and why the
# picker was opened, when something asked for it.
proc ::vlang::_refresh_state {w} {
    if {![winfo exists $w.state]} { return }
    set def [expr {$::vi18n::primary eq "" ? [::vi18n::current] : $::vi18n::primary}]
    set tgt [expr {$::vi18n::secondary eq ""
        ? [::vi18n::mc {none yet}] : [::vlang::label_for $::vi18n::secondary]}]
    set text [format [::vi18n::mc {default: %1$s · swap target: %2$s}] \
        [::vlang::label_for $def] $tgt]
    if {$::vlang::why ne ""} { append text "\n" $::vlang::why }
    $w.state configure -text $text
}

# The id under the cursor, or "" when nothing is selected.
proc ::vlang::_selected {w} {
    set sel [lindex [$w.list curselection] 0]
    if {$sel eq ""} { return "" }
    set row [$w.list get $sel]
    # Rows end with "id"; strip the marks and take the last token after
    # the separator, which is the canonical id and never a display name.
    set idx [string last "  ·  " $row]
    if {$idx < 0} { return "" }
    return [string trim [string range $row [expr {$idx + 5}] end]]
}

# Preview: a few strings the user is looking at RIGHT NOW, rendered in
# the highlighted language. Concrete beats a percentage — "27% of keys
# translated" tells nobody whether the UI will read correctly.
proc ::vlang::_preview {w} {
    set id [::vlang::_selected $w]
    if {![winfo exists $w.preview]} { return }
    if {$id eq ""} {
        $w.preview configure -state normal
        $w.preview delete 1.0 end
        $w.preview configure -state disabled
        return
    }
    # Sample through the ACTIVE catalog machinery by asking msgcat for
    # this specific locale: mcset values live per locale, so a lookup
    # with an explicit locale needs a temporary switch. Switching and
    # switching back is cheap and keeps ONE resolution path (no second
    # implementation that can drift).
    set was [::vi18n::current]
    set code [catch { ::msgcat::mclocale $id ; set ok 1 }]
    set lines {}
    if {!$code} {
        foreach probe {{Send} {Cancel} {Rename} {Close} {Close (Esc)}} {
            set got [::msgcat::mc $probe]
            lappend lines "$got    ←  $probe"
        }
        ::msgcat::mclocale $was
    }
    $w.preview configure -state normal
    $w.preview delete 1.0 end
    if {[llength $lines]} {
        $w.preview insert end [join $lines "\n"]
    } else {
        $w.preview insert end [::vi18n::mc {no sample available}]
    }
    $w.preview configure -state disabled
}

# Apply the highlighted language to the running window. ANNOUNCES the
# switch in the status bar, naming what changed and what did not — the
# nothing-implicit rule.
proc ::vlang::_use {w} {
    set id [::vlang::_selected $w]
    if {$id eq ""} { return }
    set was [::vi18n::current]
    if {$id eq $was} {
        ::vstatus::set_msg [format [::vi18n::mc {already using %s}] \
            [::vlang::label_for $id]]
        return
    }
    if {[catch { ::vi18n::use $id } err]} {
        ::vstatus::set_msg $err
        return
    }
    # Rebuild the list marks in place (no reload): the cursor stays put.
    ::vlang::_refresh_rows $w
    ::vstatus::set_msg [format [::vi18n::mc {language → %1$s (was %2$s) · running window only — Save as default writes config.toml}] \
        [::vlang::label_for [::vi18n::current]] [::vlang::label_for $was]]
    # The title/help/which-key chrome is not a registered label widget
    # everywhere; a redraw of the pieces that rebuild themselves is
    # enough, and the pieces that do not are listed here on purpose.
    catch { ::vtitle::refresh }
}

proc ::vlang::_refresh_rows {w} {
    if {![winfo exists $w.list]} { return }
    set sel [lindex [$w.list curselection] 0]
    $w.list delete 0 end
    foreach row [::vlang::rows] {
        lassign $row marks name
        $w.list insert end "$marks$name"
    }
    if {$sel ne ""} { $w.list selection set $sel }
    ::vlang::_refresh_state $w
}

# Write one [tui] key through the engine's config editor. Returns the
# line the status bar shows for it — the key and value, or why not.
proc ::vlang::_persist {key val} {
    # post_json takes a Tcl VALUE and encodes it itself — the same call
    # shape overlays.tcl uses for model.provider. This used to hand it a
    # JSON STRING built by hand, which post_json then re-encoded word by
    # word, so /api/config/set received no `path` at all and answered
    # 400 "no path supplied": the picker's "Save as default" had never
    # persisted anything. Found by running the real desktop against a
    # real gateway; the suite's stub accepted any argument.
    set body [dict create path $key value [::vapi::json_string $val]]
    if {[catch { ::vapi::post_json /api/config/set $body } resp]} {
        return [format [::vi18n::mc {%1$s NOT saved: %2$s}] $key $resp]
    }
    return "$key = $val"
}

# Make the highlighted language the DEFAULT (tui.locale) and use it now.
# The swap target is KEPT — the first version overwrote it with whatever
# was on screen, and could set target = default. Only when the target is
# missing or equal to the new default does the old default take its place
# (so the chip still has somewhere to go), and the answer says so.
proc ::vlang::_make_default {w} {
    set id [::vlang::_selected $w]
    if {$id eq ""} { return }
    set old $::vi18n::primary
    if {$old eq ""} { set old [::vi18n::current] }
    if {[catch { ::vi18n::use $id } err]} { ::vstatus::set_msg $err ; return }
    set out [list [::vlang::_persist tui.locale $id]]
    set ::vi18n::primary $id
    if {$::vi18n::secondary eq "" || $::vi18n::secondary eq $id} {
        if {$old ne $id} {
            set ::vi18n::secondary $old
            lappend out [::vlang::_persist tui.locale_secondary $old]
        } else {
            set ::vi18n::secondary ""
        }
    }
    ::vi18n::run_locale_hooks
    set ::vlang::why ""
    ::vlang::_refresh_rows $w
    ::vstatus::set_msg [format [::vi18n::mc {language saved: %s}] [join $out {; }]]
}

# Make the highlighted language what the chip swaps to
# (tui.locale_secondary), live and persisted.
proc ::vlang::_set_target {w} {
    set id [::vlang::_selected $w]
    if {$id eq ""} { return }
    # Said HERE, not by ::vi18n::set_secondary's own refusal: i18n.tcl is
    # outside the key extractor (it spells literal braces in prose), so a
    # message it renders is never translated.
    if {$id eq [::vi18n::current]} {
        ::vstatus::set_msg [format [::vi18n::mc {%s is the language in use — pick another one to swap with}] \
            [::vlang::label_for $id]]
        return
    }
    if {[catch { ::vi18n::set_secondary $id } err]} {
        ::vstatus::set_msg $err
        return
    }
    set saved [::vlang::_persist tui.locale_secondary $id]
    set ::vlang::why ""
    ::vlang::_refresh_rows $w
    ::vstatus::set_msg [format [::vi18n::mc {swap target: %1$s — click the language chip to swap · %2$s}] \
        [::vlang::label_for $id] $saved]
}

# The status-bar chip's click handler: toggle primary <-> secondary.
# Reports either outcome, including "there is nothing to swap to".
proc ::vlang::swap_click {} {
    # Nothing to swap to: open the place where that is SET, and say why —
    # never an error that sends the user to config.toml.
    if {$::vi18n::secondary eq ""} {
        ::vlang::picker [::vi18n::mc {No swap target yet: pick a language and press “Set as swap target”.}]
        return
    }
    set was [::vi18n::current]
    # The catch var MUST be the one the error is read from. This read
    # `$err` while catching into `now` — copied from the sibling procs
    # above, which do catch into `err` — so every swap FAILURE raised
    # "can't read \"err\": no such variable" instead of reporting why the
    # swap did not happen. The unswappable case (no secondary configured)
    # is the common path, so the bug fired on the ordinary refusal, not an
    # exotic one.
    if {[catch { ::vi18n::swap } err]} {
        ::vstatus::set_msg $err
        return
    }
    set now [::vi18n::current]
    ::vstatus::set_msg [format [::vi18n::mc {language swapped: %1$s ⇄ %2$s}] \
        [::vlang::label_for $was] [::vlang::label_for $now]]
}

# The status-bar text: the active language's own code, e.g. "en" or
# "zh_hans". Kept short — the bar has other residents — and it is a
# BUTTON (see statusbar.tcl), which is the "dedicated button to swap".
proc ::vlang::chip_text {} {
    return [::vi18n::current]
}