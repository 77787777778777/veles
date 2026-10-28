# veles-desktop/lib/complete.tcl — helm-grade completion for the composer.
#
# The old readline-style Tab (longest-prefix + status-bar listing) was
# "very basic and very much not on par with helm" (user audit). This is
# the replacement: a LIVE popup that filters as you type.
#
# Behavior:
#   Tab on "/"…      slash-command candidates (fuzzy-filtered) — each row
#                    carries the command's usage AND what it does; the
#                    full text of the highlighted row sits on its own
#                    line under the list (helm's doc line)
#   Tab on "/name "  ARGUMENT candidates (themes, keymaps, formats…)
#   Tab on ~,/,./…   path completion (dirs get a trailing /)
#   Tab otherwise    word-completion from recent history lines
# While the popup is open:
#   Tab / Down       next candidate
#   Up               previous candidate
#   Enter            accept the highlighted candidate
#   Esc              dismiss without inserting (vi mode: before normal)
#   C-g              dismiss (abort ladder)
#   typing           keeps filtering live
#   a second Tab with ONE candidate inserts it and closes.
#
# Scoring (helm-style): exact prefix > substring > subsequence; runs of
# contiguous matches score above scattered ones. Case-insensitive.

namespace eval ::vcomp {
    variable win ""          ;# popup toplevel path
    variable items {}        ;# current filtered {display completion category desc}
    variable sel 0
    variable prefix ""       ;# the token being completed
    variable prefix_kind ""  ;# slash | slash_arg | path | word
    variable arg_ctx ""      ;# for slash_arg: the command name
    variable hist_words {}   ;# cached history vocabulary
    variable has_desc 0      ;# any visible candidate carries a description
    variable popup_px 0      ;# the popup's width, before the list is mapped
}

# ── scoring ──────────────────────────────────────────────────────────

# Fuzzy subsequence score. Returns -1 for no match. Higher = better:
# prefix matches dominate, then early anchors, then contiguous runs.
proc ::vcomp::_score {needle hay} {
    set needle [string tolower $needle]
    set hay [string tolower $hay]
    if {$needle eq ""} { return 1 }
    if {[string match "$needle*" $hay]} { return [expr {10000 - [string length $hay]}] }
    set sub [string first $needle $hay]
    if {$sub == 0} { return [expr {10000 - [string length $hay]}] }
    if {$sub > 0} { return [expr {5000 - $sub - [string length $hay]}] }
    # Subsequence walk.
    set score 0
    set hi 0
    set run 0
    set hlen [string length $hay]
    foreach ch [split $needle ""] {
        set found -1
        for {set i $hi} {$i < $hlen} {incr i} {
            if {[string index $hay $i] eq $ch} { set found $i; break }
        }
        if {$found < 0} { return -1 }
        if {$found == $hi} { incr run } else { set run 0 }
        incr score [expr {10 + $run * 15 - $found / 10}]
        set hi [expr {$found + 1}]
    }
    return $score
}

# Rank candidates by how well `q` matches. `field` selects WHICH part
# of the row is matched: 0 (default) is the display, 1 is the
# completion. Slash rows match on their NAME — their display carries the
# usage ("/export [md|json]"), and matching that would surface `/export`
# for a query of "md", i.e. let argument hints pollute the namespace.
#
# Rows leave here uniformly FOUR fields, {display completion category
# desc}. Sources built for path/word/arg completion are three-field rows
# (they have no prose), and one of them reaches `_open` without passing
# through `_source` at all — so the padding belongs in the one function
# every candidate path goes through, not in each source.
proc ::vcomp::_filter {candidates q {field 0}} {
    set scored {}
    foreach c $candidates {
        if {[llength $c] < 4} { lappend c "" }
        set s [_score $q [lindex $c $field]]
        if {$s >= 0} { lappend scored [list $s $c] }
    }
    set scored [lsort -integer -decreasing -index 0 $scored]
    set out {}
    foreach s $scored { lappend out [lindex $s 1] }
    return $out
}

# ── candidate sources ────────────────────────────────────────────────

# Slash commands with their help text (category = "cmd").
proc ::vcomp::_slash_candidates {} {
    # The ENGINE's table (`/api/commands/builtin`) plus the handful of
    # commands that exist only here. This used to be a hand-maintained
    # "mirror of chat.tcl's slash_command dispatch", and a mirror of a
    # copy of a list that grows in `frontends/commands.rs` drifts:
    # it advertised `/config` and `/verbose` while omitting `/queue`,
    # `/context`, `/secret`, `/guards`, `/sessions` and `/cron`, all of
    # which work. See cmds.tcl.
    set out {}
    foreach r [::vcmds::all] {
        lappend out [list [lindex $r 0] [lindex $r 1] [lindex $r 2]]
    }
    return $out
}

# Argument candidates for a slash command ("" = none).
proc ::vcomp::_arg_candidates {cmd} {
    switch -- $cmd {
        /model {
            # Known model names from the gateway config (providers'
            # model lists + aliases). Offline → empty, Tab then says
            # "takes no completable arguments" as before.
            set out {}
            catch {
                set cfg [::vapi::get_json /api/config]
                foreach p [dict get $cfg providers] {
                    foreach m [dict get $p models] {
                        lappend out [list $m $m model]
                    }
                }
                foreach a [dict get $cfg aliases] {
                    set n [dict get $a name]
                    lappend out [list $n $n alias]
                }
            }
            return [lsort -unique -index 0 $out]
        }
        /theme {
            set out {}
            foreach t [::vt::theme_names] {
                lappend out [list $t $t theme]
            }
            return $out
        }
        /keymap {
            return {{normie normie keymap} {emacs emacs keymap} {vi vi keymap}}
        }
        /export {
            return {{md md format} {json json format}}
        }
        /new {
            # Session-name suggestion: existing titles for reference.
            set out {}
            catch {
                set resp [::vapi::get_json /api/sessions]
                foreach s [dict get $resp sessions] {
                    set t [dict get $s title]
                    if {$t ne ""} { lappend out [list $t $t session] }
                }
            }
            return $out
        }
    }
    return {}
}

# Path candidates for a token starting with ~ / .
proc ::vcomp::_path_candidates {word} {
    set dir [file dirname $word]
    set base [file tail $word]
    if {$dir eq "." && ![string match "~*" $word] && ![string match "/*" $word]} {
        set dir ""
    }
    set expdir $dir
    if {[string match "~*" $expdir]} {
        catch { set expdir [string replace $expdir 0 0 $::env(HOME)] }
    }
    if {$expdir eq "" || ![file isdirectory $expdir]} { return {} }
    set out {}
    foreach f [lsort [glob -nocomplain -tails -directory $expdir "${base}*"]] {
        set full [file join $dir $f]
        if {[file isdirectory [file join $expdir $f]]} {
            lappend out [list "$f/" "$full/" dir]
        } else {
            lappend out [list $f $full file]
        }
    }
    return $out
}

# Vocabulary from input history (words ≥4 chars, deduped, capped).
proc ::vcomp::_hist_vocab {} {
    variable hist_words
    set seen {}
    set words {}
    foreach line $::vedit::hist {
        foreach w [regexp -all -inline {\w{4,}} $line] {
            set lw [string tolower $w]
            if {$lw ni $seen} {
                lappend seen $lw
                lappend words [list $w $w history]
            }
        }
        if {[llength $words] > 400} break
    }
    set hist_words $words
    return $words
}

# ── render ───────────────────────────────────────────────────────────

# The popup's font, in one place: the listbox draws with it and the
# column math measures with it.
proc ::vcomp::_font {} { return {monospace 10} }

# Clip `text` to `n` characters, MARKING the cut. A row that silently
# stops mid-word reads as a complete description that happens to be
# short — the same rule the transcript's elisions follow.
proc ::vcomp::_clip {text n} {
    if {$n <= 0} { return "" }
    if {[string length $text] <= $n} { return $text }
    if {$n == 1} { return "…" }
    return "[string range $text 0 [expr {$n - 2}]]…"
}

# Width of `text` in `font` pixels. Falls back to a flat 8px/char
# estimate if Tk cannot measure (a missing family, a torn-down display):
# a wrong column is a cosmetic bug, while an exception here would take
# the popup down mid-keystroke via bgerror.
proc ::vcomp::_w {font text} {
    if {![catch {font measure $font $text} m]} { return $m }
    return [expr {8 * [string length $text]}]
}

# Pixel-accurate clip, for the doc line — its font is proportional, so
# counting characters would overflow the label on a wide glyph run.
proc ::vcomp::_fit {text px {font ""}} {
    if {$font eq ""} { set font [_font] }
    if {$px <= 0} { return "" }
    if {[_w $font $text] <= $px} { return $text }
    set out ""
    foreach ch [split $text ""] {
        if {[_w $font "$out$ch…"] > $px} { break }
        append out $ch
    }
    return "$out…"
}

# Does any candidate carry a description? That is what decides whether
# the popup spends a line on the selected row's full text.
proc ::vcomp::_any_desc {items} {
    foreach c $items {
        if {[lindex $c 3] ne ""} { return 1 }
    }
    return 0
}

# ─ popup ────────────────────────────────────────────────────────────

proc ::vcomp::_open {q cands} {
    variable win
    variable items
    variable sel 0
    variable prefix_kind
    variable has_desc
    variable popup_px
    if {$win ne "" && [winfo exists $win]} { destroy $win }
    set win .vcomp
    toplevel $win
    wm overrideredirect $win 1
    # Anchor above the composer, spanning its width.
    set cw .mid.chatwrap.chat.input
    set x [winfo rootx $cw]
    set y [expr {[winfo rooty $cw] - 8}]
    set width [winfo width $cw]
    if {$width < 420} { set width 420 }
    # Score against the NAME for slash rows (see `_filter`'s `field`
    # parameter) — the display carries usage hints that must not match.
    set score_field [expr {$prefix_kind eq "slash" ? 1 : 0}]
    set score_q [expr {$prefix_kind eq "path" ? [file tail $q] : $q}]
    set items [_filter $cands $score_q $score_field]
    if {![llength $items]} {
        destroy $win
        set win ""
        return
    }
    set popup_px $width
    set has_desc [_any_desc $items]
    set n [llength $items]
    if {$n > 12} { set n 12 }
    # The doc line (the selected row, unclipped) gets a row of its own
    # only when there is a description to read.
    set height [expr {$n * 20 + 26 + ($has_desc ? 18 : 0)}]
    wm geometry $win "${width}x${height}+${x}+[expr {$y - $height}]"
    ttk::label $win.hint -text [::vi18n::mc {↑↓/Tab select · Enter accept · Esc dismiss}] \
        -foreground $::vt::T(fg_dim) -font {TkDefaultFont 8}
    listbox $win.list -background $::vt::T(bg_panel) -foreground $::vt::T(fg) \
        -selectbackground $::vt::T(select_bg) -selectforeground $::vt::T(fg) \
        -highlightthickness 0 -relief flat -bd 0 -activestyle none \
        -font [_font] -exportselection 0
    if {$has_desc} {
        # Packed before the list so it sits between the rows and the
        # key hint, and exists before _refill runs (which fills it).
        ttk::label $win.doc -text "" -anchor w \
            -foreground $::vt::T(fg) -font {TkDefaultFont 8}
    }
    pack $win.hint -side bottom -fill x -padx 4 -pady {0 2}
    if {$has_desc} { pack $win.doc -side bottom -fill x -padx 8 -pady {0 1} }
    pack $win.list -fill both -expand 1 -padx 2 -pady 2
    _refill
    raise $win
    # Click selects; the popup never steals keyboard focus from the
    # composer (typing keeps filtering).
    bind $win.list <ButtonRelease-1> {
        if {[.vcomp.list curselection] ne ""} {
            set ::vcomp::sel [lindex [.vcomp.list curselection] 0]
            ::vcomp::_accept
        }
    }
}

proc ::vcomp::_refill {} {
    variable win
    variable items
    variable sel
    variable popup_px
    set list $win.list
    $list delete 0 end
    # Character budget for one row. The list is monospace, so characters
    # are the unit the clipping math wants; the listbox's real width is
    # only known after the first map, so the asked-for width stands in.
    set px [winfo width $list]
    if {$px <= 1} { set px $popup_px }
    set charw [_w [_font] "0"]
    if {$charw <= 0} { set charw 8 }
    set budget [expr {($px - 14) / $charw}]
    if {$budget < 24} { set budget 24 }

    # Column geometry from the VISIBLE rows. The category column is
    # dropped when every row shares one category: a slash popup is all
    # "cmd", and repeating that on every line buys the description
    # nothing.
    set w_cat 0
    set w_disp 0
    set uniform 1
    set first_cat ""
    foreach c $items {
        lassign $c display completion category desc
        if {[string length $category] > $w_cat} { set w_cat [string length $category] }
        if {[string length $display] > $w_disp} { set w_disp [string length $display] }
        if {$first_cat eq ""} {
            set first_cat $category
        } elseif {$category ne $first_cat} {
            set uniform 0
        }
    }
    set prefix 0
    if {!$uniform} { set prefix [expr {$w_cat + 1}] }
    # The usage column takes at most half the row; the description is
    # the part that has to say something, so a long usage is clipped
    # here and carried in full on the doc line.
    set max_disp [expr {$budget / 2}]
    if {$max_disp < 16} { set max_disp 16 }
    if {$w_disp > $max_disp} { set w_disp $max_disp }
    set room [expr {$budget - $prefix - $w_disp - 2}]

    foreach c $items {
        lassign $c display completion category desc
        set row [_clip $display $w_disp]
        if {!$uniform} {
            set row [format "%-*s %s" $w_cat $category $row]
        }
        if {$desc ne ""} {
            # Pad the name/usage half so the descriptions line up into
            # their own column — the reason `budget` is in characters.
            set row "[format "%-*s" [expr {$prefix + $w_disp + 2}] $row][_clip $desc $room]"
        }
        $list insert end $row
    }
    set n [$list size]
    if {$sel >= $n} { set sel [expr {$n - 1}] }
    if {$sel < 0} { set sel 0 }
    if {$n} {
        $list selection clear 0 end
        $list selection set $sel
        $list see $sel
    }
    _update_doc
}

# The doc line: the selected candidate's usage and description in FULL,
# which is what the clipped row had to cut. Truncation here still marks
# itself (a popup narrower than the text cannot show all of it, and
# pretending otherwise is the silent-elision bug).
proc ::vcomp::_update_doc {} {
    variable win
    variable items
    variable sel
    variable popup_px
    if {![winfo exists $win.doc]} { return }
    set text ""
    set item [lindex $items $sel]
    if {$item ne ""} {
        lassign $item display completion category desc
        if {$desc ne ""} {
            set text [expr {$display ne "" ? "$display — $desc" : $desc}]
        }
    }
    set px [winfo width $win.doc]
    if {$px <= 1} { set px [expr {$popup_px - 16}] }
    $win.doc configure -text [_fit $text [expr {$px - 8}] {TkDefaultFont 8}]
}

proc ::vcomp::_close {} {
    variable win
    if {$win ne "" && [winfo exists $win]} { destroy $win }
    set win ""
}

proc ::vcomp::open {} {
    variable win
    return [expr {$win ne "" && [winfo exists $win]}]
}

proc ::vcomp::move {delta} {
    variable items
    variable sel
    set n [llength $items]
    if {$n == 0} return
    set sel [expr {($sel + $delta + $n) % $n}]
    _refill
}

proc ::vcomp::_accept {} {
    variable items
    variable sel
    variable prefix
    variable prefix_kind
    if {![llength $items]} { _close; return }
    set item [lindex $items $sel]
    lassign $item display completion category
    set cw .mid.chatwrap.chat.input
    # Replace the completed token.
    set start [$cw index "insert - [string length $prefix] chars"]
    $cw delete $start insert
    $cw insert insert $completion
    if {$prefix_kind eq "path" && [string match "*/" $completion]} {
        # Completed a directory: close, then drill into the next level.
        _close
        trigger
        return
    }
    if {$prefix_kind eq "path"} {
        $cw insert insert " "
    }
    _close
}

# Re-filter the open popup after the composer text changed.
proc ::vcomp::refilter {{keysym ""}} {
    variable win
    variable items
    variable prefix
    variable prefix_kind
    variable arg_ctx
    if {![open]} return
    # Navigation never refilters: ↑/↓ fire KeyPress (which moves the
    # cursor) and then KeyRelease, so refilling here used to rebuild the
    # list and reset the selection to 0 on every keystroke of walking.
    if {$keysym ne "" && [::vlist::is_nav_keysym $keysym]} { return }
    set cw .mid.chatwrap.chat.input
    set line [$cw get "insert linestart" insert]
    set q ""
    switch -- $prefix_kind {
        slash {
            set q [string trim $line]
            if {$q eq "" || [string index $q 0] ne "/"} { _close; return }
            # A space after the command → switch to arg completion.
            if {[string match "* *" $q]} {
                _close
                trigger
                return
            }
        }
        slash_arg {
            if {![string match "$arg_ctx *" $line]} { _close; return }
            set q [string range $line [expr {[string length $arg_ctx] + 1}] end]
        }
        path {
            if {![regexp {([~/][^ ]*)$} $line -> w]} { _close; return }
            set q $w
        }
        word {
            if {![regexp {(\w+)$} $line -> w]} { _close; return }
            set q $w
        }
    }
    variable last_q
    set changed [expr {![info exists last_q] || $q ne $last_q}]
    set last_q $q
    set prefix $q
    set score_field [expr {$prefix_kind eq "slash" ? 1 : 0}]
    set score_q [expr {$prefix_kind eq "path" ? [file tail $q] : $q}]
    variable items [::vcomp::_filter [::vcomp::_source] $score_q $score_field]
    # Same query, new contents (async source) → keep the cursor where the
    # user put it; _refill clamps it to the new length.
    if {$changed} { variable sel 0 }
    if {![llength $items]} { _close; return }
    _refill
}

# Current candidate source for the active kind/ctx.
proc ::vcomp::_source {} {
    variable prefix_kind
    variable arg_ctx
    switch -- $prefix_kind {
        slash {
            set out {}
            foreach row [_slash_candidates] {
                lassign $row name args help
                # Rows are {display completion category desc}. The
                # DISPLAY is the usage ("/quota [team|personal|…]"),
                # the COMPLETION is just the head — accepting a command
                # must not paste its arguments — and the description
                # rides along for the popup's right-hand column. This
                # used to flatten a row of four fields to `[list $name
                # $name cmd]` and throw the description away, which is
                # why the popup could only ever say WHAT the commands
                # are, never what they do.
                set display [string trim "$name $args"]
                lappend out [list $display $name cmd $help]
            }
            return $out
        }
        slash_arg { return [_arg_candidates $arg_ctx] }
        path { return [_path_candidates $::vcomp::prefix] }
        word { return [_hist_vocab] }
    }
    return {}
}

# ── entry point (bound to Tab) ───────────────────────────────────────

proc ::vcomp::trigger {} {
    variable prefix
    variable prefix_kind
    variable arg_ctx
    set cw .mid.chatwrap.chat.input
    if {[open]} {
        # Tab while open: accept if single match, else move down.
        variable items
        if {[llength $items] == 1} { _accept } else { move 1 }
        return
    }
    set line [$cw get "insert linestart" insert]
    set trimmed [string trim $line]

    # Slash context.
    if {[string index $trimmed 0] eq "/"} {
        if {[string match "* *" $trimmed]} {
            # Argument position.
            set arg_ctx [lindex [split $trimmed " "] 0]
            set cands [_arg_candidates $arg_ctx]
            if {![llength $cands]} {
                ::vstatus::set_msg [format [::vi18n::mc {%s takes no completable arguments}] $arg_ctx]
                return
            }
            set prefix [lindex [split $trimmed " "] end]
            if {$prefix eq $arg_ctx} { set prefix "" }
            set prefix_kind slash_arg
            _open $prefix $cands
            return
        }
        set prefix $trimmed
        set prefix_kind slash
        _open $prefix [_source]
        return
    }

    # Path context.
    if {[regexp {([~/][^ ]*)$} $line -> w]} {
        set prefix $w
        set prefix_kind path
        set cands [_path_candidates $w]
        if {![llength $cands]} {
            ::vstatus::set_msg [::vi18n::mc {no path match}]
            return
        }
        _open $w $cands
        return
    }

    # Word context (history vocabulary).
    if {[regexp {(\w+)$} $line -> w] && [string length $w] >= 2} {
        set prefix $w
        set prefix_kind word
        set cands [_filter [_hist_vocab] $w]
        if {![llength $cands]} {
            ::vstatus::set_msg [::vi18n::mc {no word match}]
            return
        }
        _open $w $cands
        return
    }

    ::vstatus::set_msg [::vi18n::mc {nothing to complete}]
}

# Keyboard while the popup is open lives at the binding sites
# (editline.tcl: Tab/Up/Down; chat.tcl: Return/Escape) so the popup
# never steals focus from the composer.
