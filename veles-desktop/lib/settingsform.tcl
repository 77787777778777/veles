# veles-desktop/lib/settingsform.tcl — every setting as a form
# (PLAN-sessions-and-phone.md A2, round P2).
#
# Typing TOML on a phone is not setup. The gateway describes every option
# it reads (GET /api/config/form, built from the structs that read
# config.toml): its kind, its default, the value config.toml holds now,
# and when a change takes effect. This draws one control per option and
# writes it back through /api/config/set, which keeps the file's comments,
# backs it up and refuses a result that would not parse.
#
# One section at a time, picked from a list, and a search across all of
# them: two hundred options built at once is slow on a cheap phone, and
# nobody reads them all.
#
# Values go back as typed JSON — a switch as true/false, a number as a
# number, a list as an array. An option whose kind nothing says (unset by
# default and unset here) goes back as `raw`, and the gateway reads it as
# a TOML literal with the TOML library; this file parses no TOML.

namespace eval ::vsettings {
    variable form {}
    variable section ""
    # path -> the field dict the form gave
    variable fields [dict create]
    variable filter_after ""
    # Searches show at most this many matches.
    variable max_matches 40
}

proc ::vsettings::build {w {payload ""}} {
    variable section
    toplevel $w
    wm title $w [::vi18n::mc {All settings}]
    wm geometry $w 760x640
    wm minsize $w 320 360
    ::voverlay::_header $w {All settings}
    bind $w <Escape> [list destroy $w]

    ttk::frame $w.bar
    ttk::combobox $w.bar.section -state readonly -width 28 \
        -textvariable ::vsettings::section
    ::va11y::name $w.bar.section [::vi18n::mc {Section}]
    ttk::entry $w.bar.q -width 18
    ::va11y::name $w.bar.q [::vi18n::mc {Search settings}]
    pack $w.bar.section -side left
    pack $w.bar.q -side left -fill x -expand 1 -padx {8 0}
    pack $w.bar -fill x -padx 8 -pady 6
    bind $w.bar.section <<ComboboxSelected>> [list ::vsettings::render $w]
    bind $w.bar.q <KeyRelease> [list ::vsettings::_filter_soon $w]

    ttk::label $w.status -justify left -wraplength 700
    pack $w.status -side bottom -fill x -padx 8 -pady 4
    set ::vsettings::body [::vmanage::scrollframe $w body]
    pack $w.body -fill both -expand 1 -padx 8

    load $w $payload
    return $w
}

# `payload` stands in for GET /api/config/form in tests.
proc ::vsettings::load {w {payload ""}} {
    variable form
    variable section
    variable fields
    if {$payload eq ""} {
        if {[catch { ::vapi::get_json /api/config/form } payload]} {
            ::vi18n::label $w.status {unavailable: %s} [::vapi::reason $payload]
            return 0
        }
    }
    set form $payload
    set fields [dict create]
    set names {}
    foreach s [dict get $form sections] {
        lappend names [_section_label [dict get $s name]]
        foreach k [dict get $s knobs] { dict set fields [dict get $k path] $k }
    }
    $w.bar.section configure -values $names
    if {$section eq "" || $section ni $names} { set section [lindex $names 0] }
    render $w
    return 1
}

# The top level has no section name; config.toml is where its keys live.
proc ::vsettings::_section_label {name} {
    expr {$name eq "" ? "config.toml" : $name}
}

proc ::vsettings::_filter_soon {w} {
    variable filter_after
    after cancel $filter_after
    set filter_after [after 250 [list ::vsettings::render $w]]
}

# The rows of the picked section, or every match of the search.
proc ::vsettings::rows {w} {
    variable form
    variable section
    variable max_matches
    set q [string tolower [string trim [$w.bar.q get]]]
    set out {}
    foreach s [dict get $form sections] {
        set label [_section_label [dict get $s name]]
        foreach k [dict get $s knobs] {
            if {$q ne ""} {
                if {[string first $q [string tolower [dict get $k path]]] < 0} { continue }
                if {[llength $out] >= $max_matches} { return $out }
            } elseif {$label ne $section} {
                continue
            }
            lappend out $k
        }
    }
    return $out
}

proc ::vsettings::render {w} {
    variable body
    if {![winfo exists $w]} { return }
    foreach c [winfo children $body] { destroy $c }
    set searching [expr {[string trim [$w.bar.q get]] ne ""}]
    set i 0
    foreach k [rows $w] {
        set r $body.r[incr i]
        _row $r $k $searching
        pack $r -fill x -pady 4
    }
}

proc ::vsettings::_value {k} {
    if {[dict get $k set] eq "true"} { return [dict get $k value] }
    return [dict get $k default]
}

proc ::vsettings::_row {r k searching} {
    set path [dict get $k path]
    set kind [dict get $k kind]
    ttk::frame $r
    # The full path while searching: matches come from every section.
    ttk::label $r.name -font TkHeadingFont \
        -text [expr {$searching ? $path : [dict get $k key]}]
    grid $r.name -row 0 -column 0 -sticky w
    if {$kind eq "bool"} {
        ttk::checkbutton $r.c -variable ::vsettings::bool($path) \
            -command [list ::vsettings::_save_bool $r $path]
    } else {
        ttk::entry $r.c
    }
    _fill $r $k
    ::va11y::name $r.c $path ;# i18n-exempt: the option's config key, the same in every language
    grid $r.c -row 0 -column 1 -sticky ew -padx {8 0}
    if {$kind ne "bool"} {
        bind $r.c <Return> [list ::vsettings::_save_entry $r $path]
        bind $r.c <FocusOut> [list ::vsettings::_save_entry $r $path]
    }
    ttk::button $r.reset -text "↺" -width 3 \
        -command [list ::vsettings::_reset $r $path]
    ::va11y::name $r.reset [::vi18n::mc {Reset to default}]
    grid $r.reset -row 0 -column 2 -padx {4 0}
    set d [dict get $k default]
    # An empty default (an empty list, an empty string) says nothing.
    if {$d ne "null" && [string trim $d] ne ""} {
        ttk::label $r.d -foreground $::vt::T(fg_dim)
        if {$kind eq "list"} {
            ::vi18n::label $r.d {default: %s} [join $d ", "]
        } else {
            ::vi18n::label $r.d {default: %s} $d
        }
        grid $r.d -row 1 -column 0 -columnspan 3 -sticky w
    }
    if {$kind eq "list"} {
        ttk::label $r.hint -foreground $::vt::T(fg_dim)
        ::vi18n::label $r.hint {comma-separated}
        grid $r.hint -row 2 -column 0 -columnspan 3 -sticky w
    }
    grid columnconfigure $r 1 -weight 1
}

# Put the field's value now (or its default) into the row's control.
proc ::vsettings::_fill {r k} {
    set v [_value $k]
    set path [dict get $k path]
    switch -- [dict get $k kind] {
        bool { set ::vsettings::bool($path) [expr {$v eq "true"}] }
        list {
            $r.c delete 0 end
            if {$v ne "null"} { $r.c insert 0 [join $v ", "] }
        }
        default {
            $r.c delete 0 end
            if {$v ne "null"} { $r.c insert 0 $v }
        }
    }
}

# ── writing ──────────────────────────────────────────────────────────

proc ::vsettings::_save_bool {r path} {
    set on [set ::vsettings::bool($path)]
    _send $r $path [expr {$on ? "true" : "false"}] value
}

proc ::vsettings::_save_entry {r path} {
    variable fields
    if {![winfo exists $r.c]} { return }
    set k [dict get $fields $path]
    set text [string trim [$r.c get]]
    # Nothing changed, nothing to write: a FocusOut is not an edit.
    set was [_value $k]
    if {[dict get $k kind] eq "list"} { set was [join $was ", "] }
    if {$text eq $was || ($text eq "" && $was eq "null")} { return }
    switch -- [dict get $k kind] {
        int {
            if {![regexp {^-?[0-9]+$} $text]} { return [_say_bad $r] }
            _send $r $path $text value
        }
        float {
            if {![regexp {^-?[0-9]+(\.[0-9]+)?([eE][-+]?[0-9]+)?$} $text]} { return [_say_bad $r] }
            _send $r $path $text value
        }
        list {
            set items {}
            foreach item [split $text ,] {
                set item [string trim $item]
                if {$item ne ""} { lappend items "\"[::vapi::json_escape $item]\"" }
            }
            _send $r $path "\[[join $items ,]\]" value
        }
        string {
            _send $r $path "\"[::vapi::json_escape $text]\"" value
        }
        default {
            # Nothing says what this option holds: the gateway reads the
            # text as a TOML literal (a number stays a number), else as
            # a string.
            _send $r $path "\"[::vapi::json_escape $text]\"" raw
        }
    }
}

proc ::vsettings::_reset {r path} {
    _send $r $path null value
}

proc ::vsettings::_say_bad {r} {
    set w [winfo toplevel $r]
    ::vi18n::label $w.status {not a number}
}

# POST one change. `json` is the value already as JSON; `field` is
# `value`, or `raw` for text the gateway reads as a TOML literal.
proc ::vsettings::_send {r path json field} {
    variable fields
    set w [winfo toplevel $r]
    set body "{\"path\":\"[::vapi::json_escape $path]\",\"$field\":$json}"
    if {[catch { ::json::json2dict [::vapi::post /api/config/set $body] } resp]} {
        ::vi18n::label $w.status {refused: %s} [::vapi::reason $resp]
        return 0
    }
    # The form's copy follows the file, so the next FocusOut compares
    # against what is written now.
    if {[dict exists $fields $path]} {
        set k [dict get $fields $path]
        if {$json eq "null"} {
            dict set k set false
        } else {
            dict set k set true
            if {[catch { ::vapi::get_json "/api/config/get?path=$path" } got]} {
                # The write landed; only reading it back failed. Keep
                # what was sent, as the form's copy.
                catch { dict set k value [dict get [::json::json2dict "{\"v\":$json}"] v] }
            } else {
                catch { dict set k value [dict get $got value] }
            }
        }
        dict set fields $path $k
        if {[winfo exists $r.c]} { _fill $r $k }
    }
    set eff ""
    catch { set eff [dict get $resp effect] }
    switch -- $eff {
        live            { ::vi18n::label $w.status {%s: in effect now} $path }
        new_sessions    { ::vi18n::label $w.status {%s: applies to new sessions; open ones keep the old value} $path }
        restart_gateway { ::vi18n::label $w.status {%s: needs a gateway restart} $path }
        default         { ::vi18n::label $w.status {saved} }
    }
    return 1
}
