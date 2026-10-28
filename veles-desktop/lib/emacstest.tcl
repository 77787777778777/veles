# veles-desktop/lib/emacstest.tcl — behavioural tests for lib/emacs.tcl.
#
# These drive a real Text widget with real key events, because the whole
# point of the module is that the help screen used to ADVERTISE emacs
# bindings that were never written. Only pressing the keys proves they
# exist.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]
# ::vstatus and ::vplace are used for messages and the goto prompt.
namespace eval ::vstatus { proc set_msg {m} {} }
# emacs.tcl routes status lines through ::vi18n (C3). Without a stub the
# kill-ring tests throw inside bindings and the suite wedges under Xvfb.
if {![llength [info commands ::vi18n::mc]]} {
    namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
}
namespace eval ::vi18n {
    proc label {w key args} {
        if {![winfo exists $w]} { return 0 }
        catch { $w configure -text [::vi18n::mc $key {*}$args] }
        return 1
    }
    proc aname {w key args} {
        if {![winfo exists $w]} { return 0 }
        catch { ::va11y::name $w [::vi18n::mc $key {*}$args] }
        return 1
    }
}
source [file join $_dir place.tcl]
source [file join $_dir emacs.tcl]

set failed 0
set passed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

wm geometry . 600x400+0+0
catch { wm attributes . -topmost 0 }

proc fresh {content} {
    catch { destroy .t }
    text .t
    pack .t
    ::vemacs::install .t
    .t delete 1.0 end
    .t insert end $content
    .t mark set insert 1.0
    # A clean kill ring per test.
    set ::vemacs::kill_ring {}
    set ::vemacs::last_was_kill 0
    set ::vemacs::last_yank {}
    set ::vemacs::ring_pos 0
    set ::vemacs::primary_frozen ""
    catch { selection clear -selection PRIMARY }
    focus -force .t
    update
}

proc key {seq} {
    event generate .t $seq
    update
}
proc body {} { return [.t get 1.0 end-1c] }

# A region that is not there must FAIL the assertion, not throw: a
# missing `vemacs_mark` used to abort the script, and a suite that dies
# prints no tally — which the flake check reads as a broken run rather
# than as the specific thing that regressed.
proc markidx {} { if {[catch {.t index vemacs_mark} i]} { return "<none>" } ; return $i }
proc marked {} {
    return [expr {[info exists ::vemacs::mark_active(.t)] && $::vemacs::mark_active(.t)}]
}

# Seed the ring the way a real kill would: the ring AND the X clipboard
# together. C-y now pulls an outside copy in before yanking (emacs'
# save-interprogram-paste-before-kill), so a ring set without the
# clipboard is an incoherent fixture — the stale clipboard would
# legitimately win.
proc set_ring {args} {
    set ::vemacs::kill_ring $args
    set ::vemacs::ring_pos 0
    set ::vemacs::last_external ""
    catch { clipboard clear }
    if {[llength $args]} { catch { clipboard append [lindex $args 0] } }
    # PRIMARY is the other half of absorb_external_copy's lookup.
    catch { selection clear -selection PRIMARY }
    update
}

# ── the keys the help screen promised ───────────────────────────────
puts "bindings actually exist"
fresh "hello world"
foreach seq {<Control-a> <Control-e> <Control-b> <Control-f> <Control-p>
             <Control-n> <Alt-b> <Alt-f> <Control-k> <Control-w> <Alt-w>
             <Alt-d> <Control-y> <Alt-y> <Control-space> <Control-slash>
             <Alt-g> <Alt-less> <Alt-greater>} {
    ok "$seq is bound" {[bind .t $seq] ne ""}
}

# ── motion ──────────────────────────────────────────────────────────
puts "\nmotion"
fresh "hello world"
.t mark set insert 1.5
key <Control-a>
ok "C-a goes to line start" {[.t index insert] eq "1.0"}
key <Control-e>
ok "C-e goes to line end" {[.t index insert] eq "1.11"}
key <Control-b>
ok "C-b moves back one char" {[.t index insert] eq "1.10"}
key <Control-f>
ok "C-f moves forward one char" {[.t index insert] eq "1.11"}

fresh "alpha beta gamma"
.t mark set insert 1.0
key <Alt-f>
ok "M-f moves over a word" {[.t index insert] eq "1.5"}
key <Alt-f>
ok "M-f again reaches the next word end" {[.t index insert] eq "1.10"}
key <Alt-b>
ok "M-b moves back a word" {[.t index insert] eq "1.6"}

# ── kill and yank ───────────────────────────────────────────────────
puts "\nkill ring"
fresh "first line\nsecond line"
.t mark set insert 1.0
key <Control-k>
ok "C-k kills to end of line" {[body] eq "\nsecond line"}
key <Control-y>
ok "C-y yanks it back" {[body] eq "first line\nsecond line"}

# Consecutive kills APPEND, as in emacs.
fresh "aaa\nbbb\nccc"
.t mark set insert 1.0
key <Control-k>
key <Control-k>
key <Control-k>
key <Control-k>
ok "four C-k empty the buffer" {[body] eq "ccc"}
set killed [lindex $::vemacs::kill_ring 0]
ok "consecutive kills appended into one entry" {[string match "aaa*bbb*" $killed]}
ok "…and the ring holds one entry, not four" {[llength $::vemacs::kill_ring] == 1}

# A non-kill between kills starts a new entry.
fresh "one\ntwo"
.t mark set insert 1.0
key <Control-k>
key <Control-f>
key <Control-k>
ok "a motion between kills starts a new ring entry" {[llength $::vemacs::kill_ring] == 2}

# ── mark and region ─────────────────────────────────────────────────
puts "\nmark and region"
fresh "abcdefghij"
.t mark set insert 1.2
key <Control-space>
.t mark set insert 1.6
key <Alt-w>
ok "M-w copies the region without deleting" {[body] eq "abcdefghij"}
ok "…and the region is what was copied" {[lindex $::vemacs::kill_ring 0] eq "cdef"}

fresh "abcdefghij"
.t mark set insert 1.2
key <Control-space>
.t mark set insert 1.6
key <Control-w>
ok "C-w kills the region" {[body] eq "abghij"}
key <Control-y>
ok "C-y restores it" {[body] eq "abcdefghij"}

fresh "no mark here"
set_ring
key <Control-w>
ok "C-w without a mark is a no-op, not a crash" {[body] eq "no mark here"}
# C-g must cancel the region as a REGION, not merely hide it.
#
# deactivate_mark used to clear mark_active and remove the `sel` tag but
# leave the Tk mark itself in place, while _region asked ONLY whether
# that mark existed. So C-g took the highlight off the screen and C-w
# went on killing the same region minutes later — the cancel key was
# purely cosmetic. USER: "the mark/selection sticks forever and C-g
# doesn't cancel it".
#
# Driven through the real <Control-g> binding, not by calling
# deactivate_mark directly: the binding is half of what was broken.
fresh "abcdefghij"
.t mark set insert 1.2
key <Control-space>
.t mark set insert 1.6
key <Control-g>
ok "C-g clears the active flag" {![marked]}
ok "…and unsets the mark itself" {[markidx] eq "<none>"}
ok "…so no region survives it" {[::vemacs::_region .t] eq ""}
key <Control-w>
ok "…and C-w after C-g kills nothing" {[body] eq "abcdefghij"}

# The flag alone must also be enough: a stale mark with the region
# deactivated is not a region, whichever of the two went stale first.
fresh "abcdefghij"
.t mark set insert 1.2
key <Control-space>
.t mark set insert 1.6
set ::vemacs::mark_active(.t) 0
ok "an inactive mark yields no region even when the mark exists" \
    {[::vemacs::_region .t] eq ""}


# ── yank-pop ────────────────────────────────────────────────────────
puts "\nyank-pop"
fresh "xxx"
set_ring "newest" "older" "oldest"
.t delete 1.0 end
key <Control-y>
ok "C-y yanks the newest" {[body] eq "newest"}
key <Alt-y>
ok "M-y replaces it with the next" {[body] eq "older"}
key <Alt-y>
ok "M-y again walks further back" {[body] eq "oldest"}
key <Alt-y>
ok "M-y wraps around the ring" {[body] eq "newest"}

fresh "abc"
set_ring "q" "r"
set ::vemacs::last_yank {}
key <Alt-y>
ok "M-y without a preceding yank does nothing" {[body] eq "abc"}

# ── undo is the real stack, not one snapshot ────────────────────────
puts "\nundo"
fresh ""
.t insert insert "aaa"
.t edit separator
.t insert insert "bbb"
ok "buffer has both edits" {[body] eq "aaabbb"}
key <Control-slash>
ok "C-/ undoes ONE edit, not the whole buffer" {[body] eq "aaa"}
key <Control-slash>
ok "C-/ again undoes the previous edit" {[body] eq ""}

# ── kill ring reaches the clipboard ─────────────────────────────────
puts "\nclipboard interop"
fresh "copy me please"
.t mark set insert 1.0
key <Control-space>
.t mark set insert 1.7
key <Alt-w>
set clip ""
catch { set clip [clipboard get] }
ok "M-w puts the region on the clipboard" {$clip eq "copy me"}

# ── mode gating ─────────────────────────────────────────────────────
puts "\nmode gating"
fresh "text"
::vemacs::uninstall .t
ok "uninstall removes C-k" {[bind .t <Control-k>] eq ""}
ok "uninstall removes C-y" {[bind .t <Control-y>] eq ""}
ok "uninstall removes C-space" {[bind .t <Control-space>] eq ""}
::vemacs::install .t
ok "install puts them back" {[bind .t <Control-k>] ne ""}

# ── the region is VISIBLE, not just internal ────────────────────────
#
# USER: "selection is only visible when done with mouse". C-space set a
# private `vemacs_mark` and nothing ever tagged the text, so the region
# existed but could not be seen. It now uses `sel` — the same tag Tk's
# mouse bindings use — which is also what makes the two ONE selection.
puts "\nthe region is visible"
fresh "abcdefghij"
.t mark set insert 1.2
key <Control-space>
ok "setting the mark alone selects nothing" {[.t tag ranges sel] eq ""}
key <Control-f>
ok "moving the point extends the visible region" {[.t tag ranges sel] eq {1.2 1.3}}
key <Control-f>
key <Control-f>
ok "…and keeps following it" {[.t tag ranges sel] eq {1.2 1.5}}
ok "…so the region and the mouse selection are the same tag" \
    {[.t get sel.first sel.last] eq "cde"}
key <Control-g>
ok "C-g drops the region" {[.t tag ranges sel] eq ""}

# ── the region survives keys that are TK'S, not ours ────────────────
#
# USER: "I can't modify the selection after doing C-space and then C-a
# and trying to shrink it." C-a/C-f were fine; the arrows were not.
# tk::TextSetCursor clears `sel` on every plain motion, so Left/Right/
# Home/End wiped the region and left the mark behind — live internally,
# invisible on screen. install_motion_tracking repaints from the mark
# on the `all` bindtag, i.e. after Tk has moved the cursor.
puts "\nthe region survives Tk's own motion keys"
::vemacs::install_motion_tracking
fresh "abcdefghij"
.t mark set insert 1.6
key <Control-space>
key <Control-a>
ok "C-a after C-space marks the line so far" {[.t tag ranges sel] eq {1.0 1.6}}
key <Right>
ok "Right SHRINKS the region instead of wiping it" {[.t tag ranges sel] eq {1.1 1.6}}
key <Right>
ok "…and keeps shrinking it" {[.t tag ranges sel] eq {1.2 1.6}}
key <Left>
ok "Left grows it back" {[.t tag ranges sel] eq {1.1 1.6}}
key <Home>
ok "Home extends to line start" {[.t tag ranges sel] eq {1.0 1.6}}
key <End>
ok "End swaps the region to the other side of the mark" \
    {[.t tag ranges sel] eq {1.6 1.10}}
ok "…and the mark did not move" {[markidx] eq "1.6"}

# Shift-arrow with no mark: emacs' shift-select-mode starts a region.
# Tk was extending `sel` from its own anchor while vemacs_mark stayed
# wherever it last was, so the very next C-f collapsed the selection.
fresh "abcdefghij"
.t mark set insert 1.4
::vemacs::deactivate_mark .t
key <Shift-Right>
ok "Shift-Right with no mark selects" {[.t tag ranges sel] eq {1.4 1.5}}
ok "…and adopts it as a real region" {[marked]}
ok "…anchored where the selection started" {[markidx] eq "1.4"}
key <Control-f>
ok "…so C-f goes on extending it" {[.t tag ranges sel] eq {1.4 1.6}}
key <Alt-w>
ok "…and M-w copies exactly that" {[lindex $::vemacs::kill_ring 0] eq "ef"}

# A mouse drag is the same object as a keyboard region: adopt it, so it
# can be adjusted from the keyboard afterwards.
fresh "abcdefghij"
::vemacs::deactivate_mark .t
.t tag add sel 1.2 1.5
.t mark set insert 1.5
::vemacs::_track_mouse .t
ok "a drag becomes a real region" {[marked]}
ok "…anchored at the far end" {[markidx] eq "1.2"}
key <Control-f>
ok "…so the keyboard can extend a mouse selection" {[.t tag ranges sel] eq {1.2 1.6}}

fresh "abcdefghij"
.t mark set insert 1.2
key <Control-space>
key <Control-f>
.t tag remove sel 1.0 end
.t mark set insert 1.7
::vemacs::_track_mouse .t
ok "a plain click drops the region" {![marked]}

::vemacs::uninstall_motion_tracking
ok "uninstall unbinds the tracking" {[bind all <Right>] eq ""}
::vemacs::install_motion_tracking

fresh "abcdefghij"
.t mark set insert 1.2
key <Control-space>
.t mark set insert 1.6
::vemacs::show_region .t
key <Alt-w>
ok "M-w deactivates the mark, as emacs does" {[.t tag ranges sel] eq ""}

# ── kill ring and clipboard are one thing ───────────────────────────
#
# USER: "the mouse copy-paste seems separate from the killring - they
# should be integrated."
puts "\nkill ring and clipboard are integrated"
fresh ""
set_ring "internal"
# Something outside the ring — a mouse copy, another app, the context
# menu — lands on the clipboard.
catch { clipboard clear }
catch { clipboard append "from outside" }
update
key <Control-y>
ok "C-y yanks what was copied outside the ring" {[body] eq "from outside"}
ok "…and it joined the ring" {[lindex $::vemacs::kill_ring 0] eq "from outside"}
ok "…above what was already there" {[lindex $::vemacs::kill_ring 1] eq "internal"}
key <Alt-y>
ok "M-y still walks back to the internal entry" {[body] eq "internal"}

fresh ""
set_ring "same"
key <Control-y>
ok "an unchanged clipboard is not pushed again" \
    {[llength $::vemacs::kill_ring] == 1}

# A selection made ANY way copies into the ring through <<Copy>>.
fresh "hello world"
set_ring
.t tag add sel 1.0 1.5
event generate .t <<Copy>>
update
ok "<<Copy>> on a mouse selection fills the kill ring" \
    {[lindex $::vemacs::kill_ring 0] eq "hello"}
set clip ""
catch { set clip [clipboard get] }
ok "…and the clipboard agrees" {$clip eq "hello"}

# ── select-all and incremental search ───────────────────────────────
#
# USER: "Is the emacs select-all shortcut missing" (it was) and "C-s
# should call the different kinds of emacs-style search and cycle
# through results" (C-s was unbound).
puts "\nmark-whole-buffer and isearch"

fresh "alpha beta\nsecond line\nthird"
ok "C-x h is bound" {[bind .t <Control-x><h>] ne ""}
.t mark set insert 2.3
key <Control-x>
key <h>
ok "C-x h marks the whole buffer" {[.t tag ranges sel] eq {1.0 3.5}}
ok "…and the region is the whole text" \
    {[.t get sel.first sel.last] eq "alpha beta\nsecond line\nthird"}
ok "…so M-w copies all of it" \
    {[::vemacs::copy_selection .t] == 1
     && [lindex $::vemacs::kill_ring 0] eq "alpha beta\nsecond line\nthird"}

ok "C-s is bound" {[bind .t <Control-s>] ne ""}
ok "C-r is bound" {[bind .t <Control-r>] ne ""}

# With no findbar on this surface, isearch must SAY so rather than
# erroring — the dev pane and any bare Text are in that position.
#
# This suite stubs set_msg as a no-op, so record it here for the
# assertion rather than asserting on something nothing maintains.
set ::vstatus::last ""
proc ::vstatus::set_msg {m} { set ::vstatus::last $m }
::vemacs::isearch 1
ok "isearch without a findbar says so instead of erroring" \
    {[string match "*no search bar*" $::vstatus::last]}

# With a findbar, C-s opens it and C-s again steps matches.
namespace eval ::vfind {
    variable active 0
    variable stepped ""
    proc show {} { set ::vfind::active 1 ; set ::vfind::stepped "shown" }
    proc next {} { set ::vfind::stepped "next" }
    proc prev {} { set ::vfind::stepped "prev" }
}
frame .mid ; frame .mid.chatwrap ; frame .mid.chatwrap.findbar
entry .mid.chatwrap.findbar.entry
set ::vfind::active 0
::vemacs::isearch 1
ok "C-s with no bar open opens it" {$::vfind::stepped eq "shown"}
.mid.chatwrap.findbar.entry insert 0 "query"
::vemacs::isearch 1
ok "C-s again steps to the next match" {$::vfind::stepped eq "next"}
::vemacs::isearch -1
ok "C-r steps to the previous one" {$::vfind::stepped eq "prev"}
set ::vfind::active 0
::vemacs::isearch -1
ok "C-r with no bar open opens it too" {$::vfind::stepped eq "shown"}
catch { destroy .mid }

# ── the four reported input bugs, as assertions (rescue round, item 7)
puts "\nthe reported behaviours"

# 3. Kill ring "pastes part, then all of it with a duplicated prefix" —
# three of the four causes, now pinned:
#
#   a. typing after a yank invalidates the yank (M-y must refuse, not
#      delete a stale index range and reinsert elsewhere);
#   b. a mouse click moves the point — same refusal;
#   c. a kill no longer mirrors into CLIPBOARD, so an external copy is
#      what C-y absorbs — asked for as UTF8_STRING, because a typeless
#      `clipboard get` reads Latin-1 STRING and truncates at the first
#      non-convertible character.
fresh ""
set_ring "newest" "older"
key <Control-y>
ok "C-y yanks" {[body] eq "newest"}
key <Key-a>
ok "typing invalidates the yank" {$::vemacs::last_yank eq {}}
key <Alt-y>
ok "M-y after typing refuses rather than re-splicing" {[body] eq "newesta"}

fresh "xyz"
set_ring "newest" "older"
key <Control-y>
event generate .t <Button-1> -x 2 -y 2
event generate .t <ButtonRelease-1> -x 2 -y 2
update
ok "a mouse click invalidates the yank too" {$::vemacs::last_yank eq {}}

set_ring
clipboard clear
clipboard append "héllo wörld"
set ::vemacs::kill_ring {}
::vemacs::absorb_external_copy
ok "an external copy survives past Latin-1 (UTF8_STRING first)" \
    {[lindex $::vemacs::kill_ring 0] eq "héllo wörld"}

# An EXPLICIT copy still takes CLIPBOARD — X law: PRIMARY is what you
# selected/killed, CLIPBOARD is what you copied.
fresh "copy me"
set_ring
.t tag add sel 1.0 1.6
::vemacs::copy_selection .t
set clip ""
catch { set clip [clipboard get] }
ok "an explicit copy takes CLIPBOARD" {$clip eq "copy m"}

# A kill takes PRIMARY (middle-click elsewhere pastes it) and leaves
# CLIPBOARD alone.
fresh "kill me"
clipboard clear
set_ring
.t mark set insert 1.0
key <Control-space>
.t mark set insert 1.6
key <Control-w>
set clip "<none>"
catch { set clip [clipboard get] }
ok "a kill does NOT clobber CLIPBOARD" {$clip eq "<none>"}
set prim "<none>"
catch { set prim [selection get -selection PRIMARY] }
ok "…but it does own PRIMARY" {$prim eq "kill m"}

# 4. "C-/ dies, C-z works by accident" — a normie→emacs→normie round
# trip must RESTORE editline's bindings, not blank them; C-z is bound
# explicitly in both modes.
fresh "text"
set ::vemacs::_prior_bindings(.t,<Control-slash>) { ::vedit::undo; break }
::vemacs::uninstall .t
ok "uninstall restores C-/ rather than blanking it" \
    {[string match "*vedit::undo*" [bind .t <Control-slash>]]}
ok "…and C-z goes back to whatever owned it" \
    {[bind .t <Control-z>] eq ""}
::vemacs::install .t
ok "install binds C-z" {[bind .t <Control-z>] ne ""}
fresh "before"
.t edit reset
.t insert end "later"
key <Control-z>
ok "C-z undoes through the real stack" {[body] eq "before"}

# The C-x h prefix must DISARM: its widget-level `break` keeps the
# keymap's all-tag handlers from seeing the "h", so without disarming,
# the next plain "s" fires the still-armed C-x s instead of typing.
# The stub reproduces the trap: an armed continuation key BREAKS and
# never types.
namespace eval ::vkeymap {
    variable armed ""
    variable trap_fired 0
    proc _disarm {} {
        variable armed ""
        catch { bind all <Key-s> {} }
    }
    proc _arm_prefix {p} {
        variable armed $p
        bind all <Key-s> { set ::vkeymap::trap_fired 1 ; break }
    }
}
fresh "plain"
::vkeymap::_arm_prefix "C-x"
key <Control-x>
key <h>
ok "C-x h disarms the armed prefix" {$::vkeymap::armed eq ""}
ok "…so the region is marked, not trapped" {[.t tag ranges sel] eq {1.0 1.5}}
# Typing with the region live REPLACES it (emacs law) — the assertion
# is that the s is a TYPED s, not the stale prefix's C-x s firing.
key <Key-s>
ok "after C-x h, a plain s types instead of firing the stale prefix" \
    {[body] eq "s" && !$::vkeymap::trap_fired}
::vkeymap::_disarm

# ── RM3 #11: select-all that doesn't destroy, undo that undoes ──────
puts "\nC-x h does not cut, and pastes undo alone (RM3)"

# X11 Tk maps a BARE <Control-Key-x> onto <<Cut>>, so pressing the C-x
# of C-x h with a live region DELETED the region before the "h" ever
# arrived — select-all that "doesn't seem to work at all" while quietly
# eating text. In emacs/vi, C-x is a prefix, never a cut. The mapping
# is managed at the MODE switch, so drive the mode switch.
namespace eval ::vpanes { variable R ".rm3nopanes" }
::vemacs::apply_mode emacs
fresh "alpha beta\ngamma delta"
.t tag add sel 1.0 1.5
key <Control-x>
ok "pressing C-x alone does not cut the region" \
    {[body] eq "alpha beta\ngamma delta"}
key <h>
ok "…and C-x h still marks the whole buffer" \
    {[.t get sel.first sel.last] eq "alpha beta\ngamma delta"}
::vemacs::apply_mode normie

# Undo of a C-y paste must remove ONLY the paste: consecutive inserts
# MERGE on Tk's undo stack (an auto-separator is added only when the
# edit KIND changes), so typing-then-yanking was one undo item and C-/
# took the typing with it. USER: "C-/ … doesn't seem to be able to undo
# pasting".
fresh ""
.t edit reset
set_ring "pasted line one\npasted line two"
.t insert end "typed"
key <Control-y>
key <Control-slash>
ok "undo removes the yank, not the typing" {[body] eq "typed"}
key <Control-slash>
ok "…and the next undo takes the typing" {[body] eq ""}

# A real <<Paste>> (C-v / menu) is atomic — pin it so it stays that way.
fresh ""
.t edit reset
clipboard clear
clipboard append "clip line one\nclip line two"
.t insert end "typed"
key <<Paste>>
key <Control-slash>
ok "undo removes a clipboard paste, not the typing" {[body] eq "typed"}

# ── RM3 #9: PRIMARY serving is FROZEN at ownership ──────────────────
puts "\nPRIMARY is frozen at ownership (RM3)"

# _serve_primary re-read the CURRENT KILL per chunk: anything moving
# the ring between INCR chunks (an M-y, another kill in another pane)
# served a different string at a stale offset — the mid-word
# truncation. PRIMARY must serve what was OWNED, until something else
# takes ownership.
fresh ""
set_ring "older entry"
::vemacs::_push_kill "the owned kill"
# M-y moves the ring's "current" without re-owning PRIMARY…
set ::vemacs::ring_pos 1
ok "PRIMARY keeps serving what was owned when the ring moves under it" \
    {[selection get -selection PRIMARY] eq "the owned kill"}

# A >4000-byte multibyte payload crosses INCR chunk boundaries — the
# boundary must not land mid-character or mid-word.
fresh ""
set big ""
for {set i 0} {$i < 20} {incr i} {
    append big [string repeat "a—b" 300]\n
}
::vemacs::_push_kill $big
ok "a >16KB multibyte selection round-trips whole" \
    {[selection get -selection PRIMARY] eq $big}

# ── RM3 #9: /paste and absorb read UTF8_STRING before the Latin-1 floor
puts "\nclipboard reads ask UTF8_STRING first (RM3)"

# A GTK/Qt-style owner down-converts a typeless STRING request to
# Latin-1 — truncating at the first non-convertible character (the
# em-dash cut). ::vemacs::clipboard_text must ask for UTF8_STRING
# before falling back.
set ::rm3_full "start — middle — end ✓ tail"
namespace eval ::rm3 {}
proc ::rm3::serve_utf8 {off max} {
    return [string range $::rm3_full $off [expr {$off + $max - 1}]]
}
proc ::rm3::serve_latin1 {off max} {
    # Down-convert: keep only Latin-1, drop the rest — the hostile
    # foreign owner this models.
    set s [string map {— "" ✓ ""} $::rm3_full]
    return [string range $s $off [expr {$off + $max - 1}]]
}
catch { selection own -selection CLIPBOARD . }
selection handle -selection CLIPBOARD -type UTF8_STRING . ::rm3::serve_utf8
selection handle -selection CLIPBOARD -type STRING . ::rm3::serve_latin1
update
ok "clipboard_text prefers UTF8_STRING and survives a down-converting owner" \
    {[::vemacs::clipboard_text] eq $::rm3_full}
catch { selection clear -selection CLIPBOARD }

# ── RM3 #11, normie mode: a paste must land an undo snapshot ─────────
puts "\nnormie: paste lands an undo snapshot (RM3)"

# ::vedit::undo restores one whole-buffer SNAPSHOT, and snapshots were
# taken on KeyPress only — a menu/C-v paste types no key, so C-/ after
# a paste resurrected a pre-typing state or said "nothing to undo".
source [file join $_dir editline.tcl]
catch { destroy .mid.chatwrap.chat.input }
foreach f {.mid .mid.chatwrap .mid.chatwrap.chat} { catch { frame $f } }
# TAKE THE SPACE BACK FIRST. `.t` — this suite's own scratch text — is
# packed in the same toplevel and asks for all of it. With a big enough
# default font there is nothing left over, `.mid` is never mapped, and
# `focus -force` on an UNVIEWABLE widget silently does not take: the
# Control-slash below then goes to whatever still has the focus (`.t`)
# and this case passes or fails on something it is not about.
#
# Measured, not guessed: under the font set `nix flake check` runs with
# (Noto only) `.mid` came out 1x1 and unmapped and the focus stayed on
# `.t`; on a host with a full font set the same code gave it 600x34 and
# the focus moved. That is why this case was the one thing still red in
# the sandbox after the derivation learned to run at all.
pack forget .t
update
pack .mid -fill both -expand true
pack .mid.chatwrap -fill both -expand true
pack .mid.chatwrap.chat -fill both -expand true
text .mid.chatwrap.chat.input
pack .mid.chatwrap.chat.input -fill both -expand true
::vedit::install
set ::vedit::undo_snap ""
set ::vedit::undo_last 0
.mid.chatwrap.chat.input delete 1.0 end
.mid.chatwrap.chat.input insert end "typed"
# Age the burst clock so the paste's snapshot is not suppressed.
set ::vedit::undo_last 0
clipboard clear
clipboard append "pasted by menu\nsecond line"
event generate .mid.chatwrap.chat.input <<Paste>>
update
ok "the paste landed" \
    {[.mid.chatwrap.chat.input get 1.0 end-1c] eq "typedpasted by menu\nsecond line"}
# Key events only fire bindings on the FOCUS window — the whole point
# of `key` in this suite.
focus -force .mid.chatwrap.chat.input
update
# ASSERTED, so the layout fragility above can never be silent again. Tk
# redirects a generated key event to the FOCUS window, so without this
# the next two lines would quietly exercise a different widget.
ok "…and the composer actually has the focus" \
    {[focus] eq ".mid.chatwrap.chat.input"}
event generate .mid.chatwrap.chat.input <Control-slash>
update
ok "normie C-/ undoes the paste back to the typed text" \
    {[.mid.chatwrap.chat.input get 1.0 end-1c] eq "typed"}
catch { destroy .mid.chatwrap.chat.input }

puts "\nemacstest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
