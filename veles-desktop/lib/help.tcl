# veles-desktop/lib/help.tcl — F1 help overlay, registry-driven.
#
# Every row comes from lib/bindings.tcl for the ACTIVE mode — the same
# table that installs the bindings, so the help screen can never show a
# stale key. Grouped by category (general / view / session / panes /
# navigation / composer / prefix chords), with the always-true editing
# keys documented at the bottom.
#
# On a phone, help is the touches first (touch): its keys are for a
# keyboard it may not have.

namespace eval ::vhelp {}

proc ::vhelp::open {} {
    if {[llength [info commands ::vandroid::on]] && [::vandroid::on]} {
        return [touch]
    }
    keys
}

# USER 2026-10-01: "We need another help screen for android to explain
# all the controls". What a finger does, as lib/android.tcl and the
# touch translation (android/androwish/touch.patch) make it: each line
# is one of their behaviours, and androidtest pins them.
proc ::vhelp::touch {} {
    set w .help
    if {[winfo exists $w]} { destroy $w }
    toplevel $w
    wm title $w [::vi18n::mc {veles — help}]
    wm geometry $w 640x720
    ttk::label $w.title -foreground $::vt::T(accent) -font {TkDefaultFont 12 bold}
    ::vi18n::label $w.title {veles · touch help}
    text $w.body -wrap word -background $::vt::T(bg_panel) \
        -foreground $::vt::T(fg) -font {TkDefaultFont 10} -bd 0 \
        -highlightthickness 0 -spacing1 3 -spacing3 3 -height 20 -width 40
    $w.body tag configure cat -foreground $::vt::T(gold) \
        -font {TkDefaultFont 10 bold} -spacing1 10
    $w.body tag configure act -lmargin1 8 -lmargin2 20
    # Literal mc calls, so the catalog tools see every line as used.
    foreach {heading lines} [list \
        [::vi18n::mc {moving around}] [list \
            [::vi18n::mc {☰ shows or hides the session list; picking a session closes it.}] \
            [::vi18n::mc {⋮ opens every command, a tap at a time.}] \
            [::vi18n::mc {Back closes what is on top: a menu, the session list, a window. With nothing open, veles goes to the background and keeps running.}]] \
        [::vi18n::mc {the transcript}] [list \
            [::vi18n::mc {Drag to scroll; a quick swipe goes further.}] \
            [::vi18n::mc {Tap a link to open it, or a block's header to open or close the block.}] \
            [::vi18n::mc {Touch and hold a word to select it. When you lift your finger, its menu opens, with Copy at the top.}] \
            [::vi18n::mc {Hold, then drag before you lift: the selection follows your finger a word at a time, scrolling at the top or bottom edge.}] \
            [::vi18n::mc {Hold either end of a selection, then drag, to move that end a letter at a time.}] \
            [::vi18n::mc {Tap the text to clear the selection.}]] \
        [::vi18n::mc {writing}] [list \
            [::vi18n::mc {Tap the box at the bottom to type. Send sends, Stop ends the turn, and Detach moves a running command to the background.}] \
            [::vi18n::mc {📎 attaches files from the phone. Touch and hold in the box for its editing menu.}] \
            [::vi18n::mc {follow keeps the newest output in view; reasoning shows the model's reasoning.}]] \
        [::vi18n::mc {menus}] [list \
            [::vi18n::mc {Touch and hold a session, or anything else that has a menu, to open the menu. A tap outside it, or Back, closes it.}] \
            [::vi18n::mc {How long a hold takes is Android's touch and hold delay, in its accessibility settings.}]] \
        [::vi18n::mc {notifications}] [list \
            [::vi18n::mc {While veles is in the background, an agent's question or a finished turn is a notification; tap it to open the session.}]] \
        [::vi18n::mc {a keyboard}] [list \
            [::vi18n::mc {With a keyboard attached, every key works as on a laptop.}]]] {
        $w.body insert end "$heading\n" cat
        # A no-break space keeps the bullet on its line: Tk wraps only
        # at spaces, and a line of Japanese, which has none, left it
        # alone above the text.
        foreach line $lines {
            $w.body insert end "·\u00a0$line\n" act
        }
    }
    ::vtext::readonly $w.body
    ttk::frame $w.btns
    ttk::button $w.btns.keys -text [::vi18n::mc {Keyboard help}] \
        -command { destroy .help ; ::vhelp::keys }
    ttk::button $w.btns.close -text [::vi18n::mc {Close}] -command [list destroy $w]
    pack $w.btns.close $w.btns.keys -side right -padx {6 0}
    pack $w.title -fill x -padx 10 -pady {10 4}
    pack $w.btns -side bottom -fill x -padx 10 -pady {4 10}
    pack $w.body -fill both -expand 1 -padx 10 -pady 4
    bind $w <Escape> [list destroy $w]
    return $w
}

# The keys, from the registry.
proc ::vhelp::keys {} {
    set w .help
    if {[winfo exists $w]} { destroy $w }
    toplevel $w
    wm title $w [::vi18n::mc {veles — help}]
    wm geometry $w "640x560+[expr {[winfo rootx .]+150}]+[expr {[winfo rooty .]+40}]"
    ttk::label $w.title -foreground $::vt::T(accent) -font {TkDefaultFont 12 bold}
    ::vi18n::label $w.title {veles · keyboard help (bindings: %s)} $::vkeymap::current
    text $w.body -height 26 -wrap word -background $::vt::T(bg_panel) \
        -foreground $::vt::T(fg) -font {monospace 9} -bd 0 -highlightthickness 0
    $w.body tag configure key -foreground $::vt::T(cyan) -font {monospace 9 bold}
    $w.body tag configure act -foreground $::vt::T(fg)
    $w.body tag configure dim -foreground $::vt::T(fg_dim)
    $w.body tag configure cat -foreground $::vt::T(gold) -font {TkDefaultFont 10 bold}

    set last_cat ""
    foreach entry [::vbindings::describe $::vkeymap::current] {
        lassign $entry keys help category
        if {$category ne $last_cat} {
            # The CATEGORY is a group header on this screen, so it is
            # user-visible text — but the registry's value is also the
            # grouping IDENTIFIER (which-key buckets rows by it), so the
            # translation belongs here at the display site rather than in
            # the table. Translating it in ::vbindings::table would make
            # two modes' buckets collide if a locale rendered two
            # categories the same way.
            $w.body insert end "\n── [::vi18n::mc $category] ──\n" cat
            set last_cat $category
        }
        $w.body insert end [format "  %-14s" $keys] key
        $w.body insert end "$help\n" act
    }

    set k $::vkeymap::current
    # These three pairs are the help screen's own prose — the whole
    # "composer" section USER reported as untranslated (2026-09-26).
    # They are computed text (mode-dependent), so they go through mc()
    # here rather than being registered on a widget.
    if {$k eq "emacs"} {
        set edit [::vi18n::mc {C-a/C-e line ends · C-b/C-f char · M-b/M-f word · C-k kill to EOL · C-y yank · M-w copy · C-w kill region · C-space set mark · C-/ undo}]
        set extra [::vi18n::mc {C-g aborts everything · C-j newline · M-p/M-n history · Tab complete}]
    } elseif {$k eq "vi"} {
        set edit [::vi18n::mc {emacs keys in insert mode · Esc → normal (j/k sessions, i insert)}]
        set extra [::vi18n::mc {C-g aborts everything · C-j newline · M-p/M-n history · Tab complete}]
    } else {
        set edit [::vi18n::mc {Text class editing (arrows, Home/End, Ctrl-U, Ctrl-W) · Shift-Enter newline}]
        set extra [::vi18n::mc {M-p/M-n history · Tab complete}]
    }
    $w.body insert end "\n── [::vi18n::mc {composer}] ($k) ──\n" cat
    $w.body insert end "  $edit\n" act
    $w.body insert end "  $extra\n" act
    $w.body insert end "\n[::vi18n::mc {Tips:}]\n" cat
    # Read the registry: these used to claim "M-x in every mode" and
    # "prefix chords C-x/M-o", both false once normie became pure Ctrl.
    $w.body insert end "  · [format [::vi18n::mc {%s opens the command palette in %s.}] \
        [::vkeymap::key_for ::vpalette::open] $k]\n" dim
    ;# keymap-literal: both branches are already mode-conditional, and
    ;# the prefix NAMES are fixed rows in the registry for those modes.
    if {$k eq "normie"} {
        $w.body insert end "  · [::vi18n::mc {normie is pure Ctrl — no emacs prefix chords.}]\n" dim
    } else {
        $w.body insert end "  · [::vi18n::mc {Prefix chords (C-x, M-o) show a which-key popup the moment they arm.}]\n" dim
    }
    $w.body insert end "  · [::vi18n::mc {/keymap <normie|emacs|vi> or palette → Keybindings switches schemes.}]\n" dim
    $w.body insert end "  · [::vi18n::mc {The Dev pane's REPL evaluates in the LIVE app — the UI is its own lisp.}]\n" dim
    ::vtext::readonly $w.body
    ttk::button $w.close -text [::vi18n::mc {Close (Esc)}] -command [list destroy $w]
    pack $w.title -fill x -padx 10 -pady {10 4}
    pack $w.body -fill both -expand 1 -padx 10 -pady 4
    pack $w.close -side right -padx 10 -pady {0 10}
    bind $w <Escape> [list destroy $w]
    bind $w <F1> [list destroy $w]
    focus $w.close
}
