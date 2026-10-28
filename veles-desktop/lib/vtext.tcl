# veles-desktop/lib/vtext.tcl — the settings every Text widget needs and
# almost none of them had.
#
# USER: "The keyboard controls don't work quite right in the read-only
# text boxes and in the input box it's not visible when something is
# selected or not".
#
# Both verified under Xvfb against the real theme:
#
#  1. SELECTION IS INVISIBLE WHEN THE WIDGET IS NOT FOCUSED.
#     theme.tcl does `option add *selectBackground $T(select_bg)`, which
#     covers `-selectbackground` but NOT `-inactiveselectbackground` —
#     a separate option (resource name inactiveSelectBackground). It
#     kept Tk's default #c3c3c3, so an unfocused selection rendered as
#     #f5edff text on light grey. Focus leaves constantly: clicking the
#     transcript, opening any overlay.
#
#  2. READ-ONLY BOXES CANNOT BE REACHED BY KEYBOARD.
#     `tk::FocusOK` returns 0 for a Text with `-state disabled` and the
#     default `-takefocus ""`, so Tab traversal skips every transcript,
#     diff, log and preview pane in the app. Setting `-takefocus 1`
#     makes FocusOK return 1 and tk_focusNext reach it — and once
#     focused, motion and Shift-selection already work on a disabled
#     widget (only insertion is refused, which is the point).
#
#  3. The insertion cursor defaults to #000000 — black on a near-black
#     field. Three widgets set it explicitly; the other nineteen did not.
#
# Fixed through the option database rather than at 22 call sites, so a
# text widget added later gets it too. Existing widgets are updated by
# walking the tree on theme change — `::vt::retheme_widgets` had a
# hardcoded list of three.

namespace eval ::vtext {}

# Blend two #rrggbb colours. Used for the inactive selection: still
# clearly a selection, visibly not the focused one.
proc ::vtext::blend {a b frac} {
    if {![regexp {^#([0-9a-fA-F]{6})$} $a] || ![regexp {^#([0-9a-fA-F]{6})$} $b]} {
        return $a
    }
    set out "#"
    foreach i {1 3 5} {
        scan [string range $a $i [expr {$i + 1}]] %x ca
        scan [string range $b $i [expr {$i + 1}]] %x cb
        append out [format %02x [expr {int($ca * $frac + $cb * (1.0 - $frac))}]]
    }
    return $out
}

# Relative luminance, 0..1. Cheap Rec.601 — enough to answer "can you
# see this against that".
proc ::vtext::luminance {hex} {
    if {![regexp {^#([0-9a-fA-F]{6})$} $hex]} { return -1 }
    scan [string range $hex 1 2] %x r
    scan [string range $hex 3 4] %x g
    scan [string range $hex 5 6] %x b
    return [expr {(0.299 * $r + 0.587 * $g + 0.114 * $b) / 255.0}]
}

# A selection background you can actually SEE against the text.
#
# USER: "the selection is still only visible when selected with the
# mouse" — and then, still: invisible for the keyboard region too.
#
# The cause was not the tag. `sel` was applied correctly; the COLOUR was
# unusable. `::vt::from_engine` maps an engine theme's `panel_hi` onto
# `select_bg`, and panel_hi is by definition a SUBTLE panel highlight —
# a couple of shades off the background. As a panel tint that is right;
# as a selection it is invisible.
#
# So do not trust the palette blindly: if select_bg does not separate
# from the text background, walk it toward the accent until it does.
# The result still belongs to the theme, and it can always be seen.
proc ::vtext::selection_colour {} {
    set sel $::vt::T(select_bg)
    set bg $::vt::T(bg_input)
    if {[luminance $sel] < 0 || [luminance $bg] < 0} { return $sel }
    set tries 0
    while {abs([luminance $sel] - [luminance $bg]) < 0.18 && $tries < 10} {
        set sel [blend $::vt::T(accent) $sel 0.35]
        incr tries
    }
    return $sel
}

proc ::vtext::inactive_colour {} {
    set sel [selection_colour]
    set bg  $::vt::T(bg_input)
    return [blend $sel $bg 0.55]
}

# Option-database defaults for every Text created from here on. Called
# by ::vt::apply_theme, which runs before the layout is built.
proc ::vtext::defaults {} {
    option add *Text.selectBackground [selection_colour]
    option add *Text.selectForeground $::vt::T(fg)
    option add *Text.inactiveSelectBackground [inactive_colour]
    option add *Text.insertBackground $::vt::T(pink)
    # Read-only panes are still buffers you navigate, copy from and Tab
    # into — Emacs has no notion of a region you cannot even reach.
    option add *Text.takeFocus 1
}

# Every Text widget under $root, including $root.
proc ::vtext::all {{root .}} {
    set out {}
    if {[winfo class $root] eq "Text"} { lappend out $root }
    foreach c [winfo children $root] {
        foreach w [all $c] { lappend out $w }
    }
    return $out
}

# Update widgets that already exist (theme switch, or anything built
# before the options were installed).
proc ::vtext::restyle_all {{root .}} {
    set inactive [inactive_colour]
    foreach w [all $root] {
        catch {
            $w configure \
                -selectbackground [selection_colour] \
                -selectforeground $::vt::T(fg) \
                -inactiveselectbackground $inactive \
                -takefocus 1
        }
        # Only widgets that were left with Tk's black default — never
        # stomp a deliberate choice.
        catch {
            if {[$w cget -insertbackground] in {#000000 black}} {
                $w configure -insertbackground $::vt::T(pink)
            }
        }
    }
    return
}

# ── read-only buffers that are still real buffers ───────────────────
#
# USER: "The yanking/killring still don't show a cursor in the read-only
# areas".
#
# `-state disabled` was how read-only was done, and it takes away three
# separate things:
#
#   * Tk draws NO insertion cursor on a disabled widget, so emacs point
#     motion (C-n, C-p, M-f …) moves an invisible point. You cannot see
#     where the mark will be set from, or where a yank would land.
#   * insert/delete are SILENTLY dropped — rc 0, no error. That is how a
#     whole session of answers went missing (see ::vchat::_writable).
#   * tk::FocusOK returns 0, so keyboard traversal skips the widget.
#
# So: keep the widget `-state normal` and refuse the MODIFYING events
# instead, through a bindtag placed just before the Text class tag.
# Bindtag order is {$w VReadonly Text . all}: per-widget bindings still
# win, and our emacs class bindings (which sit on Text) still run,
# because a command chord is not an edit and falls through.

# Does this key event try to change the text?
proc ::vtext::_is_edit_key {A st K} {
    # Control, Alt or Meta held → a command chord, not typing.
    #
    # 0x4 Control, 0x8 Mod1. The last two matter: Tk also reports Alt as
    # 0x20000 and Meta as 0x40000 when the key is not mapped to Mod1 —
    # which is the case on any keyboard layout that does not put Alt on
    # Mod1, and on a bare X server with no modifier map. Checking Mod1
    # alone would let M-f through as a printable "f" and swallow every
    # M- chord in every read-only pane.
    if {($st & 0x4) || ($st & 0x8) || ($st & 0x20000) || ($st & 0x40000)} {
        return 0
    }
    if {$K in {BackSpace Delete Return KP_Enter}} { return 1 }
    if {$A eq ""} { return 0 }
    scan $A %c code
    # Control characters (Tab, Escape, …) are navigation, not text.
    if {$code < 32 || $code == 127} { return 0 }
    return 1
}

proc ::vtext::_install_readonly_class {} {
    variable readonly_class_done
    if {[info exists readonly_class_done]} { return }
    set readonly_class_done 1
    bind VReadonly <Key> { if {[::vtext::_is_edit_key %A %s %K]} { break } }
    # The paste/cut family, including X11 middle-click paste.
    foreach ev {<<Paste>> <<Cut>> <<Clear>> <<PasteSelection>> <<Undo>> <<Redo>>} {
        bind VReadonly $ev { break }
    }
    bind VReadonly <Button-2> { break }
    return
}

# Make $w read-only WITHOUT disabling it.
proc ::vtext::readonly {w} {
    if {![winfo exists $w]} { return }
    _install_readonly_class
    $w configure -state normal
    # A caret you can actually see, and one that blinks.
    catch { $w configure -insertbackground $::vt::T(pink) -insertwidth 2 -insertofftime 300 }
    set tags [bindtags $w]
    if {[lsearch -exact $tags VReadonly] < 0} {
        # After the widget tag, before Text.
        bindtags $w [linsert $tags 1 VReadonly]
    }
    return
}

proc ::vtext::is_readonly {w} {
    if {![winfo exists $w]} { return 0 }
    return [expr {[lsearch -exact [bindtags $w] VReadonly] >= 0}]
}

# A click on a tagged range (a link, a block's header). On a laptop it
# runs on the press, as `tag bind <Button-1>` always did. On a phone a
# finger's drag scrolls read-only text (lib/android.tcl), and a drag
# that happened to start on a link opened it: there it runs on the
# release of a tap, one that did not move and was not held.
proc ::vtext::click {w tag script} {
    if {[llength [info commands ::vandroid::on]] && [::vandroid::on]} {
        $w tag bind $tag <Button-1> {::vandroid::_tap_arm %W %x %y}
        $w tag bind $tag <ButtonRelease-1> [list ::vandroid::_tap_fire %W %x %y $script]
    } else {
        $w tag bind $tag <Button-1> $script
    }
}


# Replace a text widget's whole content.
#
# Every read-only pane in the app open-codes this, and the ones that
# used `-state disabled` had to remember to flip it back and forth —
# keymap.tcl's binding preview once did `configure -text` on a text
# widget instead, threw on every selection change, and therefore never
# rendered at all. A widget made read-only by ::vtext::readonly stays
# in `normal` state (that is the point of the bindtags approach), so
# filling it needs no state dance; a `-state disabled` one still does,
# and this handles both.
proc ::vtext::set_text {w text} {
    if {![winfo exists $w]} { return }
    set was ""
    catch { set was [$w cget -state] }
    if {$was eq "disabled"} { $w configure -state normal }
    $w delete 1.0 end
    if {$text ne ""} { $w insert end $text }
    if {$was eq "disabled"} { $w configure -state disabled }
    return
}
