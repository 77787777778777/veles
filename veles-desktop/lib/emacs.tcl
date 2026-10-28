# veles-desktop/lib/emacs.tcl — real emacs editing for text widgets.
#
# USER: "Emacs should be the first class citizen always, everything else
# can just follow."
#
# It was not. Tk's Text class ships almost nothing: of the keys the help
# screen advertised — C-a/C-e line ends, C-b/C-f char, M-b/M-f word,
# C-k kill, C-y yank, C-w kill word — only C-k and C-d actually existed.
# The rest were documentation for bindings that were never written.
#
# `C-/` was bound, but to a single whole-buffer snapshot: it restored
# everything back to the last typing-burst boundary, which is why it
# felt like undoing the turn rather than undoing an edit. Tk's Text
# widget has a real undo stack (`-undo 1`, `edit undo`); this uses it.
#
# ── what this provides ──────────────────────────────────────────────
#
#   motion    C-a C-e  C-b C-f  C-p C-n  M-b M-f  M-< M->
#   kill ring C-k kill-line · C-w kill-region · M-w copy-region
#             M-d kill-word · C-y yank · M-y yank-pop
#   mark      C-space set-mark, region is mark..point
#   goto      M-g g  (and M-g M-g), like emacs
#   undo      C-/ C-_ C-x u  → the widget's own undo stack
#
# Kills append when consecutive, as emacs does: C-k C-k C-k yanks back
# three lines, not the last one.
#
# ── why it is mode-gated ────────────────────────────────────────────
#
# These CANNOT be unconditional: normie binds Ctrl-K to the command
# palette, and C-k here is kill-line. `install`/`uninstall` are driven
# by ::vkeymap::use, so emacs and vi get the full set and normie keeps
# its Ctrl-* application shortcuts.

namespace eval ::vemacs {
    variable kill_ring {}      ;# most recent kill first
    variable last_kill_end ""  ;# widget index where the last kill stopped
    variable last_was_kill 0   ;# consecutive kills append
    variable last_yank {}      ;# {start end} of the last yank, for M-y
    variable ring_pos 0        ;# which ring entry the last yank used
    variable installed {}      ;# widgets we have bound
    variable mark_active       ;# w -> 1 while the region is live
    variable _prior_bindings   ;# w,seq -> the binding install replaced
    variable last_external ""  ;# last outside copy absorbed (not a new copy)
    variable primary_frozen "" ;# what PRIMARY serves, frozen at ownership
    variable primary_handler 0 ;# the PRIMARY handler registers once, ever
}

# ── kill ring ───────────────────────────────────────────────────────

proc ::vemacs::_push_kill {text {append_to_last 0}} {
    variable kill_ring
    variable ring_pos
    if {$text eq ""} { return }
    # Absorb FIRST — emacs' save-interprogram-paste-before-kill. An
    # outside copy joins the ring BELOW the kill about to be made, so a
    # fresh kill is never beaten by a stale clipboard (absorbing at
    # YANK time instead put the stale copy on TOP, and C-y pasted
    # something you killed an hour ago over the thing you just killed).
    absorb_external_copy
    if {$append_to_last && [llength $kill_ring]} {
        lset kill_ring 0 "[lindex $kill_ring 0]$text"
    } else {
        set kill_ring [linsert $kill_ring 0 $text]
    }
    # Emacs' default kill-ring-max is 120; no reason to grow forever.
    if {[llength $kill_ring] > 120} {
        set kill_ring [lrange $kill_ring 0 119]
    }
    set ring_pos 0
    # X selection law, followed: PRIMARY is "what you just selected or
    # killed" — middle-click anywhere pastes it — and CLIPBOARD is
    # "what you explicitly copied". Kills used to mirror into CLIPBOARD,
    # which broke the other direction: `absorb_external_copy` reads
    # CLIPBOARD first, so it could only ever see our own mirror, never
    # what the user actually copied elsewhere.
    _own_primary [_current_kill]
}

# Take PRIMARY ownership for a FROZEN snapshot of the text.
#
# RM3 #9: _serve_primary used to re-read the CURRENT kill per chunk —
# anything moving the ring between a transfer's INCR chunks (an M-y, a
# kill in another pane) served a DIFFERENT string at a stale offset,
# and a short chunk ends the transfer wherever it happens to land:
# mid-word, mid-character. A selection serves what was OWNED, full
# stop, until something else takes ownership. The handler is also
# registered ONCE, not per kill: re-registering mid-transfer resets
# Tk's byte offsets for the next chunk (tkSelect.c gives every
# `selection handle` a fresh CommandInfo).
proc ::vemacs::_own_primary {text} {
    variable primary_frozen
    variable primary_handler
    set primary_frozen $text
    if {!$primary_handler} {
        catch {
            selection handle -selection PRIMARY . ::vemacs::_serve_primary
        }
        set primary_handler 1
    }
    catch { selection own -selection PRIMARY . }
}

# Serve the FROZEN snapshot to PRIMARY requestors (other apps'
# middle-click). Tk calls with {offset maxBytes}; a short return ends
# the transfer — which is why the string must never change under it.
proc ::vemacs::_serve_primary {offset maxbytes} {
    variable primary_frozen
    set s $primary_frozen
    if {$s eq ""} { return "" }
    return [string range $s $offset [expr {$offset + $maxbytes - 1}]]
}

# Pull an outside copy into the ring.
#
# USER: "the mouse copy-paste seems separate from the killring - they
# should be integrated."
#
# Absorbed at KILL time (see _push_kill) and when the ring is empty at
# yank time. CLIPBOARD first (an explicit copy), then PRIMARY (an X
# selection) — and only when it is NEW: not equal to the ring's top and
# not equal to the last thing absorbed (a copy still sitting on the
# clipboard is not a second copy).
#
# TWO failure modes this shape guards against, both real:
#   - `catch { clipboard get } ext` sets ext to the ERROR MESSAGE on an
#     empty clipboard — the message then joined the ring and got pasted
#     into the buffer. Every fetch is checked for success now.
#   - `clipboard get` with no -type asks for STRING (Latin-1), which
#     GTK/Qt owners down-convert — truncating at the first
#     non-convertible character and mangling anything past Latin-1.
#     USER (live): pasted text came back with a stray "Á". UTF8_STRING
#     is requested first.
# The best text the CLIPBOARD will give: UTF8_STRING first, then the
# typeless (Latin-1) floor. `clipboard get` with no -type asks for
# STRING, which GTK/Qt owners down-convert — truncating at the first
# non-convertible character (RM3 #9: the em-dash cut). One proc, used
# by absorb AND the composer's /paste, so the two cannot drift.
proc ::vemacs::clipboard_text {} {
    if {[catch { clipboard get -type UTF8_STRING } t] == 0} { return $t }
    if {[catch { clipboard get } t] == 0} { return $t }
    return ""
}

proc ::vemacs::absorb_external_copy {} {
    variable kill_ring
    variable ring_pos
    variable last_external
    set ext [clipboard_text]
    if {$ext eq ""} {
        if {[catch { selection get -selection PRIMARY } e] == 0} {
            set ext $e
        }
    }
    if {$ext eq ""} { return 0 }
    if {[llength $kill_ring] && [lindex $kill_ring 0] eq $ext} { return 0 }
    if {[info exists last_external] && $last_external eq $ext} { return 0 }
    set kill_ring [linsert $kill_ring 0 $ext]
    if {[llength $kill_ring] > 120} { set kill_ring [lrange $kill_ring 0 119] }
    set ring_pos 0
    set last_external $ext
    return 1
}

# Copy the current selection (however it was made — mouse drag included)
# into the ring AND the clipboard, so the two stay one thing.
proc ::vemacs::copy_selection {w} {
    if {[catch { $w get sel.first sel.last } text]} { return 0 }
    if {$text eq ""} { return 0 }
    _push_kill $text 0
    _explicit_copy $text
    variable last_was_kill 0
    ::vstatus::set_msg [::vi18n::mc {copied to the kill ring}]
    return 1
}

# An EXPLICIT copy (M-w, menu Copy) takes CLIPBOARD — that is what
# CLIPBOARD is for in X law. Kills (C-k/C-w) do NOT: they own PRIMARY
# via _push_kill, and mirroring them into CLIPBOARD unconditionally is
# what hid real external copies from `absorb_external_copy`.
proc ::vemacs::_explicit_copy {text} {
    if {[catch {
        clipboard clear
        clipboard append $text
    } clip_err]} {
        puts stderr "veles-desktop: clipboard write failed: $clip_err"
    }
}

proc ::vemacs::_current_kill {} {
    variable kill_ring
    variable ring_pos
    if {![llength $kill_ring]} { return "" }
    return [lindex $kill_ring $ring_pos]
}

# ── mark and region ─────────────────────────────────────────────────

proc ::vemacs::set_mark {w} {
    variable mark_active
    catch { $w mark set vemacs_mark insert }
    catch { $w mark gravity vemacs_mark left }
    set mark_active($w) 1
    show_region $w
    ::vstatus::set_msg [::vi18n::mc {mark set}]
    return
}

# Paint mark..point with the `sel` tag.
#
# USER: "selection is only visible when done with mouse". The mark was
# real but purely internal — _region computed mark..point on demand at
# copy time and nothing ever tagged it, so there was no way to see what
# C-w or M-w was about to act on. Mouse drags looked selected only
# because Tk's own bindings set `sel` itself.
#
# Using `sel` (rather than a private tag) is what makes the region and
# the mouse selection ONE selection: X selection ownership, the context
# menu's Copy, and $w get sel.first sel.last all follow it.
proc ::vemacs::show_region {w} {
    variable mark_active
    if {![winfo exists $w]} { return }
    catch { $w tag remove sel 1.0 end }
    if {![info exists mark_active($w)] || !$mark_active($w)} { return }
    set r [_region $w]
    if {[llength $r] != 2} { return }
    lassign $r a b
    if {[$w compare $a != $b]} {
        catch { $w tag add sel $a $b }
        # Own the X selection, so other apps can paste the region.
        catch { selection own -selection PRIMARY $w }
    }
    return
}

# Emacs deactivates the mark after the region has been used, and on C-g.
#
# UNSETS the Tk mark, it does not merely clear the flag. Clearing the
# flag alone left `vemacs_mark` sitting in the widget, and _region below
# only ever asked whether that mark existed — so after C-g the region was
# invisible but C-w and M-w still acted on a mark set minutes earlier.
# USER: "the mark/selection sticks forever and C-g doesn't cancel it".
proc ::vemacs::deactivate_mark {w} {
    variable mark_active
    set mark_active($w) 0
    catch { $w tag remove sel 1.0 end }
    catch { $w mark unset vemacs_mark }
    return
}

# {start end} of mark..point, or {} when there is no mark.
#
# Honours mark_active as well as the mark's existence: those are two
# different questions and only checking the second is what made C-g
# cosmetic. Both are checked because either one going stale on its own
# must still read as "no region" rather than as a region nobody can see.
proc ::vemacs::_region {w} {
    variable mark_active
    if {![info exists mark_active($w)] || !$mark_active($w)} { return {} }
    if {[catch { $w index vemacs_mark } m]} { return {} }
    set p [$w index insert]
    if {[$w compare $m <= $p]} { return [list $m $p] }
    return [list $p $m]
}

# ── commands ────────────────────────────────────────────────────────

proc ::vemacs::kill_line {w} {
    variable last_was_kill
    set start [$w index insert]
    set eol [$w index "insert lineend"]
    if {[$w compare $start == $eol]} {
        # At end of line: kill the newline itself, joining the next line.
        set end [$w index "insert +1c"]
    } else {
        set end $eol
    }
    set text [$w get $start $end]
    $w delete $start $end
    _push_kill $text $last_was_kill
    set last_was_kill 1
    return
}

proc ::vemacs::kill_region {w} {
    set r [_region $w]
    if {![llength $r]} { ::vstatus::set_msg [::vi18n::mc {no mark set}]; return }
    lassign $r a b
    set text [$w get $a $b]
    $w delete $a $b
    _push_kill $text 0
    variable last_was_kill 0
    deactivate_mark $w
    return
}

proc ::vemacs::copy_region {w} {
    set r [_region $w]
    if {![llength $r]} { ::vstatus::set_msg [::vi18n::mc {no mark set}]; return }
    lassign $r a b
    set text [$w get $a $b]
    _push_kill $text 0
    _explicit_copy $text
    variable last_was_kill 0
    deactivate_mark $w
    ::vstatus::set_msg [::vi18n::mc {region copied}]
    return
}

proc ::vemacs::kill_word {w} {
    variable last_was_kill
    set start [$w index insert]
    set end [_word_forward $w $start]
    set text [$w get $start $end]
    $w delete $start $end
    _push_kill $text $last_was_kill
    set last_was_kill 1
    return
}

proc ::vemacs::yank {w} {
    variable last_yank
    variable last_was_kill 0
    # An outside copy still reaches C-y (USER: "the mouse copy-paste
    # seems separate from the killring") — but only if it is NEWER than
    # the last kill. absorb's dedup carries that: a copy already seen at
    # kill time sits BELOW the kill, and re-absorbing it is a no-op, so
    # a fresh kill is never pasted over by a stale clipboard; a copy
    # made AFTER the kill is new, joins on top, and yanks.
    absorb_external_copy
    set text [_current_kill]
    if {$text eq ""} { ::vstatus::set_msg [::vi18n::mc {kill ring is empty}]; return }
    set start [$w index insert]
    # UNDO ATOMICITY (RM3 #11): consecutive inserts MERGE on Tk's undo
    # stack — an auto-separator is only added when the edit KIND
    # changes — so typing-then-yanking read as ONE undo item and C-/
    # removed the typing with the paste ("cannot undo paste"). Bracket
    # the yank exactly like tk_textPaste brackets itself.
    catch { $w configure -autoseparators 0 }
    catch { $w edit separator }
    $w insert insert $text
    catch { $w edit separator }
    catch { $w configure -autoseparators 1 }
    set last_yank [list $start [$w index insert]]
    return
}

# M-y: replace the just-yanked text with the next ring entry. Emacs only
# allows this immediately after a yank, and so does this.
proc ::vemacs::yank_pop {w} {
    variable last_yank
    variable kill_ring
    variable ring_pos
    if {![llength $last_yank]} { ::vstatus::set_msg [::vi18n::mc {previous command was not a yank}]; return }
    if {[llength $kill_ring] < 2} { ::vstatus::set_msg [::vi18n::mc {nothing else in the kill ring}]; return }
    lassign $last_yank a b
    set ring_pos [expr {($ring_pos + 1) % [llength $kill_ring]}]
    set text [lindex $kill_ring $ring_pos]
    # Same undo bracket as yank: the delete+insert pair must be one
    # item, not welded to the surrounding edits.
    catch { $w configure -autoseparators 0 }
    catch { $w edit separator }
    $w delete $a $b
    $w mark set insert $a
    $w insert insert $text
    catch { $w edit separator }
    catch { $w configure -autoseparators 1 }
    set last_yank [list $a [$w index insert]]
    ::vstatus::set_msg [format [::vi18n::mc {yank-pop (%1$s/%2$s)}] [expr {$ring_pos + 1}] [llength $kill_ring]]
    return
}

proc ::vemacs::undo {w} {
    if {[catch { $w edit undo }]} {
        ::vstatus::set_msg [::vi18n::mc {nothing to undo}]
    }
    return
}

proc ::vemacs::redo {w} {
    if {[catch { $w edit redo }]} {
        ::vstatus::set_msg [::vi18n::mc {nothing to redo}]
    }
    return
}

# M-g g — goto line, prompting in the status bar.
proc ::vemacs::goto_line {w} {
    set n [::vemacs::_prompt "goto line:"]
    if {$n eq "" || ![string is integer -strict $n]} { return }
    catch { $w mark set insert "$n.0" ; $w see insert }
    return
}

# A one-line modal prompt. Deliberately tiny — a full minibuffer lives
# in editline.tcl; this only needs a number.
proc ::vemacs::_prompt {label} {
    set t .vemacs_prompt
    catch { destroy $t }
    toplevel $t
    wm overrideredirect $t 1
    ttk::frame $t.f -padding 6
    ttk::label $t.f.l -text $label
    ttk::entry $t.f.e -width 12
    pack $t.f.l -side left -padx {0 6}
    pack $t.f.e -side left
    pack $t.f
    ::vplace::near_measured $t [focus -displayof .]
    set ::vemacs::_prompt_result ""
    bind $t.f.e <Return> { set ::vemacs::_prompt_result [%W get] }
    bind $t.f.e <Escape> { set ::vemacs::_prompt_result "" }
    focus -force $t.f.e
    grab set $t
    vwait ::vemacs::_prompt_result
    catch { grab release $t }
    catch { destroy $t }
    return $::vemacs::_prompt_result
}

# ── motion helpers ──────────────────────────────────────────────────

proc ::vemacs::_word_forward {w idx} {
    # Skip separators, then the word. Matches emacs' forward-word.
    set i $idx
    while {[$w compare $i < end-1c] && ![regexp {\w} [$w get $i]]} {
        set i [$w index "$i +1c"]
    }
    while {[$w compare $i < end-1c] && [regexp {\w} [$w get $i]]} {
        set i [$w index "$i +1c"]
    }
    return $i
}

proc ::vemacs::_word_back {w idx} {
    set i $idx
    while {[$w compare $i > 1.0] && ![regexp {\w} [$w get "$i -1c"]]} {
        set i [$w index "$i -1c"]
    }
    while {[$w compare $i > 1.0] && [regexp {\w} [$w get "$i -1c"]]} {
        set i [$w index "$i -1c"]
    }
    return $i
}

proc ::vemacs::forward_word {w} {
    $w mark set insert [_word_forward $w [$w index insert]]
    $w see insert
    return
}

proc ::vemacs::back_word {w} {
    $w mark set insert [_word_back $w [$w index insert]]
    $w see insert
    return
}

# ── install / uninstall ─────────────────────────────────────────────

# Every binding runs through this so `last_was_kill` is cleared by any
# command that is not a kill — the rule that makes consecutive C-k
# append and a single C-k not.
# Central dispatch, so the "was the last command a kill?" state has ONE
# owner. Kills leave it set (kill_line/kill_word do that themselves);
# everything else clears it, which is what makes C-k C-k append into one
# ring entry but C-k C-f C-k produce two.
proc ::vemacs::_run {w cmd {is_kill 0}} {
    variable last_was_kill
    if {!$is_kill} { set last_was_kill 0 }
    if {$cmd ne "yank" && $cmd ne "yank_pop"} { variable last_yank {} }
    ::vemacs::$cmd $w
    return
}

# Raw motion bindings do not go through _run, so they clear the state
# themselves. Without this a C-f between two C-k kept them appending.
proc ::vemacs::_moved {} {
    variable last_was_kill 0
    variable last_yank {}
    return
}

# Called after every motion binding: the region has to track the point,
# or it is a selection that only updates when you copy it.
proc ::vemacs::_after_motion {w} {
    catch { show_region $w }
    catch { $w see insert }
    return
}

# ── the region tracks the point, whatever moved it ───────────────────
#
# USER: "I can't modify the selection after doing C-space and then C-a
# and trying to shrink it."
#
# C-a and C-f were never the problem — they are our bindings and end in
# _after_motion. The ARROW keys are Tk's, and tk::TextSetCursor clears
# the `sel` tag unconditionally, so every plain Left/Right/Home/End
# wiped the region while leaving vemacs_mark behind: still live
# internally, completely invisible on screen. Shift-arrows had the
# mirror-image bug — Tk extended `sel` from its own anchor without
# touching the mark, so the next C-f collapsed the selection.
#
# Bound on the `all` bindtag, which runs LAST (widget → class →
# toplevel → all): Tk moves the cursor, then we repaint from the mark.
# The Text class is the wrong place — one script per tag+sequence means
# binding there would REPLACE Tk's motion rather than follow it — and
# per-widget is both too early and blind to the text widgets that
# overlays and panes create lazily.
proc ::vemacs::_track_motion {w} {
    if {![winfo exists $w] || [winfo class $w] ne "Text"} { return }
    _moved
    _after_motion $w
    return
}

proc ::vemacs::_track_shift_motion {w} {
    variable mark_active
    if {![winfo exists $w] || [winfo class $w] ne "Text"} { return }
    _moved
    if {[info exists mark_active($w)] && $mark_active($w)} {
        # We already own a region: recompute from the mark, so Tk's
        # anchor and our mark cannot disagree about where it starts.
        show_region $w
        return
    }
    # Shift-select with no mark — emacs' shift-select-mode makes one.
    adopt_selection $w
    return
}

proc ::vemacs::_track_mouse {w} {
    if {![winfo exists $w] || [winfo class $w] ne "Text"} { return }
    # A click moves the point: the last yank's index pair is stale the
    # moment the caret is elsewhere, and M-y must say so rather than
    # delete a range that no longer holds the yanked text. USER: kill
    # ring "pastes part, then all of it with a duplicated prefix" — one
    # of the four causes.
    variable last_yank {}
    # A drag that selected nothing is a plain click: it moves the point
    # and should drop the region, not adopt an empty one.
    set r {}
    catch { set r [$w tag ranges sel] }
    if {[llength $r] == 2} { adopt_selection $w } else { deactivate_mark $w }
    return
}

# Turn an existing `sel` range into a real region: whichever end is not
# the point becomes the mark.
#
# This is what makes "the mouse selection" and "the region" one object
# rather than two — after a drag or a shift-select you can go on to
# adjust it with C-f/C-b/arrows, and M-w copies what you can see.
proc ::vemacs::adopt_selection {w} {
    variable mark_active
    set r {}
    catch { set r [$w tag ranges sel] }
    if {[llength $r] != 2} { return 0 }
    lassign $r a b
    set p [$w index insert]
    set m [expr {[$w compare $p == $a] ? $b : $a}]
    catch { $w mark set vemacs_mark $m }
    catch { $w mark gravity vemacs_mark left }
    set mark_active($w) 1
    return 1
}

# Keysyms whose motion is Tk's, not ours. Kept as one list so the
# install and uninstall halves cannot drift.
proc ::vemacs::_motion_keys {} {
    return {<Left> <Right> <Up> <Down> <Home> <End> <Prior> <Next>
            <Control-Left> <Control-Right> <Control-Home> <Control-End>}
}

proc ::vemacs::_shift_motion_keys {} {
    return {<Shift-Left> <Shift-Right> <Shift-Up> <Shift-Down>
            <Shift-Home> <Shift-End> <Shift-Prior> <Shift-Next>
            <Shift-Control-Left> <Shift-Control-Right>
            <Shift-Control-Home> <Shift-Control-End>}
}

proc ::vemacs::install_motion_tracking {} {
    foreach seq [_motion_keys] {
        bind all $seq {+::vemacs::_track_motion %W}
    }
    foreach seq [_shift_motion_keys] {
        bind all $seq {+::vemacs::_track_shift_motion %W}
    }
    bind all <ButtonRelease-1> {+::vemacs::_track_mouse %W}
    return
}

proc ::vemacs::uninstall_motion_tracking {} {
    foreach seq [concat [_motion_keys] [_shift_motion_keys] [list <ButtonRelease-1>]] {
        catch { bind all $seq {} }
    }
    return
}

# Typing invalidates the yank: M-y is only legal immediately after a
# yank, and after a self-inserting command (or any edit) the remembered
# index pair points at text that is no longer what was yanked — M-y then
# deleted a stale range and reinserted elsewhere, which read as "pastes
# part, then all of it with a duplicated prefix".
proc ::vemacs::_typing_clears_yank {keysym} {
    variable last_yank
    if {[string length $keysym] == 1
        || $keysym in {space BackSpace Delete Return}} {
        set last_yank {}
    }
    return
}

proc ::vemacs::install {w} {
    variable installed
    variable _prior_bindings
    if {![winfo exists $w]} { return }
    # A real undo stack, instead of one whole-buffer snapshot.
    if {[catch { $w configure -undo 1 -autoseparators 1 -maxundo 200 } undo_err]} {
        puts stderr "veles-desktop: undo stack not enabled on $w: $undo_err"
    }

    # Remember what C-/, C-_, C-z and the widget's KeyPress script were
    # bound to before we overwrite them, so `uninstall` RESTORES them
    # instead of blanking — the uninstall_select_all pattern. Blanking
    # left a normie→emacs→normie round trip with dead C-/ and C-_, and
    # wiping <Key> silently took editline's completion-refilter and
    # undo-snap hooks with it. FIRST capture wins: apply_mode can
    # install twice without an uninstall between, and the second capture
    # would otherwise snapshot our own appends as "prior".
    foreach seq {<Control-slash> <Control-underscore> <Control-z> <Key>} {
        if {![info exists _prior_bindings($w,$seq)]} {
            set _prior_bindings($w,$seq) [bind $w $seq]
        }
    }

    # `break` on every one: these must beat the Text class bindings
    # (Tk's own C-k is a different kill) and must not fall through to
    # the app-level chords.
    foreach {seq body} [list \
        <Control-a>          "$w mark set insert {insert linestart}; $w see insert" \
        <Control-e>          "$w mark set insert {insert lineend}; $w see insert" \
        <Control-b>          "$w mark set insert {insert -1c}; $w see insert" \
        <Control-f>          "$w mark set insert {insert +1c}; $w see insert" \
        <Control-p>          "$w mark set insert {insert -1l}; $w see insert" \
        <Control-n>          "$w mark set insert {insert +1l}; $w see insert" \
        <Alt-less>           "$w mark set insert 1.0; $w see insert" \
        <Alt-greater>        "$w mark set insert end-1c; $w see insert" \
        <Alt-b>              [list ::vemacs::_run $w back_word] \
        <Alt-f>              [list ::vemacs::_run $w forward_word] \
        <Control-k>          [list ::vemacs::_run $w kill_line 1] \
        <Control-w>          [list ::vemacs::_run $w kill_region] \
        <Alt-w>              [list ::vemacs::_run $w copy_region] \
        <Alt-d>              [list ::vemacs::_run $w kill_word 1] \
        <Control-y>          [list ::vemacs::_run $w yank] \
        <Alt-y>              [list ::vemacs::_run $w yank_pop] \
        <Control-space>      [list ::vemacs::_run $w set_mark] \
        <Control-slash>      [list ::vemacs::_run $w undo] \
        <Control-underscore> [list ::vemacs::_run $w undo] \
        <Control-z>          [list ::vemacs::_run $w undo] \
    ] {
        # Only the raw motion scripts get the reset prologue. Commands
        # dispatched through _run must NOT be pre-cleared — _run needs
        # to see the previous value to decide whether a kill appends.
        if {[string match "::vemacs::_run*" $body]} {
            bind $w $seq "$body; ::vemacs::show_region $w; break"
        } else {
            # _moved clears kill/yank state BEFORE the motion;
            # _after_motion repaints the region AFTER it. The region has
            # to follow the point, or it only updates when you copy.
            bind $w $seq "::vemacs::_moved; $body; ::vemacs::_after_motion $w; break"
        }
    }

    # M-g g and M-g M-g, the two spellings emacs accepts.
    bind $w <Alt-g> [list ::vemacs::_arm_goto $w]
    # Copy through the kill ring, so the mouse selection and C-y are one
    # thing rather than two parallel clipboards.
    bind $w <<Copy>> [list ::vemacs::copy_selection $w]
    bind $w <Control-s> { ::vemacs::isearch 1 ; break }
    bind $w <Control-r> { ::vemacs::isearch -1 ; break }
    # Typing invalidates the yank (see _typing_clears_yank). Appended to
    # the widget's KeyPress so it runs after editline's, not instead of
    # it — and never breaks, so the character still types. Guarded: a
    # double install must not stack a second append.
    if {[string first "::vemacs::_typing_clears_yank" [bind $w <KeyPress>]] < 0} {
        bind $w <KeyPress> {+ ::vemacs::_typing_clears_yank %K}
    }
    # `break` is load-bearing: without it Tk's Text class binding also
    # fires and TYPES the "h", and tk::TextInsert deletes the selection
    # it lands in — so the mark was made and immediately wiped.
    #
    # The `catch { ::vkeymap::_disarm }` is equally load-bearing: C-x
    # armed the keymap's prefix (the arm binding lives on `all` and ran
    # when the widget had no C-x of its own), and this widget-level
    # completion `break`s BEFORE the all-tag handlers can see the "h" —
    # so without disarming here, the prefix stays armed and the next
    # plain "s" fires C-x s (toggle sidebar) instead of typing.
    bind $w <Control-x><h> "::vemacs::mark_whole_buffer $w; catch { ::vkeymap::_disarm }; break"
    # C-g: emacs' quit — drop the region.
    bind $w <Control-g> [list ::vemacs::deactivate_mark $w]
    lappend installed $w
    return
}

# M-g is a prefix: the next g (or M-g) runs goto-line, anything else
# cancels. Bound on the widget only while armed.
proc ::vemacs::_arm_goto {w} {
    bind $w <Key> [list ::vemacs::_goto_key $w %K]
    ::vstatus::set_msg [::vi18n::mc {M-g- (g: goto line)}]
    return -code break
}

proc ::vemacs::_goto_key {w keysym} {
    bind $w <Key> {}
    if {$keysym eq "g"} {
        after idle [list ::vemacs::_run $w goto_line]
        return -code break
    }
    ::vstatus::set_msg ""
    return
}

proc ::vemacs::uninstall {w} {
    variable installed
    variable _prior_bindings
    if {![winfo exists $w]} { return }
    foreach seq {<Control-a> <Control-e> <Control-b> <Control-f> <Control-p>
                 <Control-n> <Alt-less> <Alt-greater> <Alt-b> <Alt-f>
                 <Control-k> <Control-w> <Alt-w> <Alt-d> <Control-y>
                 <Alt-y> <Control-space> <Control-slash>
                 <Control-underscore> <Control-z> <Alt-g> <Control-g> <<Copy>>
                 <Control-s> <Control-r> <Key>} {
        # RESTORE what was there before we bound it (editline's C-/ and
        # C-_, whatever owned C-z), not {} — the uninstall_select_all
        # pattern. Blanking here is what left a normie→emacs→normie
        # round trip with dead C-/ and C-_.
        if {[info exists _prior_bindings($w,$seq)]} {
            catch { bind $w $seq $_prior_bindings($w,$seq) }
            unset _prior_bindings($w,$seq)
        } else {
            catch { bind $w $seq {} }
        }
    }
    set installed [lsearch -all -inline -not -exact $installed $w]
    return
}

# Emacs motion and region for READ-ONLY buffers.
#
# USER: "The keyboard controls don't work quite right in the read-only
# text boxes". They had no emacs bindings at all — `targets` listed the
# three editable widgets only — so C-space, M-w, M-< and M-> did nothing
# in the transcript, the diff, the log or any preview.
#
# In emacs a read-only buffer still has a point you move and a region
# you copy; only modification is refused. So this binds exactly the
# non-destructive half. C-k, C-w, C-y, M-y, M-d and undo are left out
# deliberately: on a `-state disabled` widget they would raise inside
# the binding and land in bgerror.
#
# Bound on the TEXT CLASS, not per widget. There are 22 Text widgets and
# most are created lazily when an overlay or pane opens, so any list
# here would be stale the moment one is added — which is exactly how the
# three that had support came to be the only three. Bindtag order is
# widget → class → toplevel → all, so the editable widgets' own
# bindings still win.
proc ::vemacs::install_readonly {} {
    foreach {seq body} [list \
        <Control-a>     {%W mark set insert {insert linestart}; %W see insert} \
        <Control-e>     {%W mark set insert {insert lineend}; %W see insert} \
        <Control-b>     {%W mark set insert {insert -1c}; %W see insert} \
        <Control-f>     {%W mark set insert {insert +1c}; %W see insert} \
        <Control-p>     {%W mark set insert {insert -1l}; %W see insert} \
        <Control-n>     {%W mark set insert {insert +1l}; %W see insert} \
        <Control-v>     {%W yview scroll 1 pages} \
        <Alt-v>         {%W yview scroll -1 pages} \
        <Alt-less>      {%W mark set insert 1.0; %W see insert} \
        <Alt-greater>   {%W mark set insert end-1c; %W see insert} \
        <Alt-b>         {::vemacs::_run %W back_word} \
        <Alt-f>         {::vemacs::_run %W forward_word} \
        <Alt-w>         {::vemacs::_run %W copy_region} \
        <Control-space> {::vemacs::_run %W set_mark} \
    ] {
        if {[string match "::vemacs::_run*" $body]} {
            bind Text $seq "$body; ::vemacs::show_region %W; break"
        } else {
            bind Text $seq "::vemacs::_moved; $body; ::vemacs::_after_motion %W; break"
        }
    }
    bind Text <<Copy>> { ::vemacs::copy_selection %W }
    bind Text <Control-s> { ::vemacs::isearch 1 ; break }
    bind Text <Control-r> { ::vemacs::isearch -1 ; break }
    # Same disarm as the editable C-x h: the `break` keeps the keymap's
    # all-tag handlers from seeing the "h", so the prefix would stay
    # armed and swallow the next plain key.
    bind Text <Control-x><h> { ::vemacs::mark_whole_buffer %W; catch { ::vkeymap::_disarm }; break }
    bind Text <Control-g> { ::vemacs::deactivate_mark %W }
    return
}

proc ::vemacs::uninstall_readonly {} {
    # Restore Tk's own Text class bindings rather than blanking them —
    # blanking would leave normie mode with no C-a/C-e at all, which is
    # worse than what it had.
    foreach seq {<Control-a> <Control-e> <Control-b> <Control-f>
                 <Control-p> <Control-n> <Control-v> <Alt-v>
                 <Alt-less> <Alt-greater> <Alt-b> <Alt-f> <Alt-w>
                 <Control-space> <Control-g> <<Copy>>
                 <Control-s> <Control-r> <Control-x><h>} {
        catch { bind Text $seq [set ::vemacs::_tk_text($seq)] }
    }
    return
}

# Tk's defaults, captured once before we overwrite any of them.
proc ::vemacs::_capture_tk_text {} {
    variable _tk_text
    if {[info exists _tk_text]} { return }
    foreach seq {<Control-a> <Control-e> <Control-b> <Control-f>
                 <Control-p> <Control-n> <Control-v> <Alt-v>
                 <Alt-less> <Alt-greater> <Alt-b> <Alt-f> <Alt-w>
                 <Control-space> <Control-g> <<Copy>>
                 <Control-s> <Control-r> <Control-x><h>} {
        set _tk_text($seq) [bind Text $seq]
    }
    return
}

# ── isearch and mark-whole-buffer ───────────────────────────────────
#
# USER: "Is the emacs select-all shortcut missing or did I just forget
# what it was" — it was missing; C-x h is emacs' mark-whole-buffer.
#
# USER: "C-s should call the different kinds of emacs-style search and
# cycle through results" — C-s was unbound. The findbar already knows how
# to search and step matches, so this drives it rather than growing a
# second search.
#
#   C-s   isearch-forward   — open the bar, or step to the NEXT match
#   C-r   isearch-backward  — open the bar, or step to the PREVIOUS one
#   C-x h mark-whole-buffer

proc ::vemacs::isearch {dir} {
    # No findbar (a plain Text under test, or the dev pane): degrade to
    # saying so rather than erroring.
    if {![winfo exists .mid.chatwrap.findbar.entry]} {
        catch { ::vstatus::set_msg [::vi18n::mc {no search bar on this surface}] }
        return
    }
    set active 0
    catch { set active $::vfind::active }
    set q ""
    catch { set q [.mid.chatwrap.findbar.entry get] }
    if {!$active || $q eq ""} {
        # Opening IS the search — emacs starts incremental search empty
        # and narrows as you type.
        ::vfind::show
        catch { ::vstatus::set_msg [expr {$dir > 0 \
            ? [::vi18n::mc {I-search: type to search, C-s for next}] \
            : [::vi18n::mc {I-search backward: type to search, C-r for previous}]}] }
        return
    }
    if {$dir > 0} { ::vfind::next } else { ::vfind::prev }
}

# C-x h — the whole buffer becomes the region, visibly.
proc ::vemacs::mark_whole_buffer {w} {
    variable mark_active
    if {![winfo exists $w]} { return }
    catch { $w mark set vemacs_mark 1.0 }
    catch { $w mark gravity vemacs_mark left }
    catch { $w mark set insert end-1c }
    set mark_active($w) 1
    show_region $w
    catch { ::vstatus::set_msg [::vi18n::mc {whole buffer marked}] }
    return
}

# The widgets that should get emacs editing when the mode calls for it.
proc ::vemacs::targets {} {
    set out {}
    foreach w [list .mid.chatwrap.chat.input \
                    $::vpanes::R.dev.edit.body \
                    $::vpanes::R.dev.repl.in] {
        if {[winfo exists $w]} { lappend out $w }
    }
    return $out
}

proc ::vemacs::apply_mode {mode} {
    _capture_tk_text
    foreach w [targets] {
        if {$mode eq "normie"} {
            uninstall $w
        } else {
            install $w
        }
    }
    # X11 maps a BARE <Control-Key-x> onto <<Cut>> (tk.tcl's virtual
    # table) — so pressing the C-x of any C-x chord with a live region
    # DELETED the region before the chord's key arrived. In emacs/vi
    # C-x is a PREFIX, never a cut: remove the physical mapping for
    # those modes. Event mappings are process-global, which is why
    # this lives at the mode switch — normie restores C-x=cut below,
    # where it IS the convention.
    if {$mode eq "normie"} {
        catch { event add <<Cut>> <Control-Key-x> }
    } else {
        catch { event delete <<Cut>> <Control-Key-x> }
    }
    if {$mode eq "normie"} {
        uninstall_readonly
        uninstall_motion_tracking
        # normie = GUI law: Ctrl-A selects ALL (in emacs mode the same
        # chord means beginning-of-line, and C-x h marks the whole
        # buffer — both still true there). USER: "there is no select
        # all shortcut?"
        install_select_all
    } else {
        # ORDER MATTERS: uninstall_select_all restores Tk's default
        # Text C-a, which would clobber the emacs line-start binding —
        # so it runs BEFORE install_readonly writes the emacs one.
        uninstall_select_all
        install_readonly
        install_motion_tracking
    }
    return
}

# Ctrl-A → select everything. Bound on the widget CLASSES, not per
# widget: the transcript, the composer and every overlay text are Text
# widgets, and the filter entries are Entries — a per-widget list would
# be stale the moment a lazy overlay opens one (the exact drift that
# made readonly bindings class-wide for the same reason).
proc ::vemacs::install_select_all {} {
    bind Text <Control-a> {
        catch { %W tag add sel 1.0 end }
        %W mark set insert 1.0
        break
    }
    bind Entry <Control-a> {
        %W selection range 0 end
        %W icursor 0
        break
    }
    # ttk entries are their own class.
    bind TEntry <Control-a> {
        %W selection range 0 end
        %W icursor 0
        break
    }
}

proc ::vemacs::uninstall_select_all {} {
    # Restore whatever Tk had before, so normie→emacs→normie round
    # trips never accumulate or lose bindings.
    if {[info exists ::vemacs::_tk_text(<Control-a>)]} {
        catch { bind Text <Control-a> $::vemacs::_tk_text(<Control-a>) }
    }
    catch { bind Entry <Control-a> {} }
    catch { bind TEntry <Control-a> {} }
}
