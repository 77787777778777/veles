# veles-desktop/lib/texttest.tcl — read-only text buffers: reachable by
# keyboard, navigable, and with a selection you can actually see.
#
# USER: "The keyboard controls don't work quite right in the read-only
# text boxes and in the input box it's not visible when something is
# selected or not".
#
# Both causes were measured, not guessed:
#
#   tk::FocusOK on a `-state disabled` Text with the default
#   `-takefocus ""` returns 0 — keyboard traversal skips every
#   transcript, diff and log pane in the app.
#
#   theme.tcl sets *selectBackground but not *inactiveSelectBackground,
#   so an unfocused selection kept Tk's #c3c3c3 and rendered as #f5edff
#   text on light grey.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]
namespace eval ::vstatus { proc set_msg {m} { set ::vstatus::last $m } }
set ::vstatus::last ""
namespace eval ::vpanes { variable R .nonexistent }
# use_theme persists the choice through ::vstate::put now, and a missing
# proc is a hard error rather than something a catch eats — which is the
# point of the change, so the stub has to carry it.
namespace eval ::vstate {
    proc put {key value} { return 1 }
    proc merge {updates} { return 1 }
    proc all {} { return {} }
    proc get {key {default ""}} { return $default }
    proc repairs {} { return {} }
}
source [file join $_dir vtext.tcl]
source [file join $_dir paths.tcl]
source [file join $_dir i18n.tcl]
source [file join $_dir theme.tcl]
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

::vt::use_theme synthwave

proc fresh_ro {} {
    catch { destroy .ro }
    text .ro -state disabled -background $::vt::T(bg_input) -foreground $::vt::T(fg)
    pack .ro
    .ro configure -state normal
    .ro insert end "alpha beta gamma\nsecond line here\nthird line"
    .ro configure -state disabled
    .ro mark set insert 1.0
    update
    return .ro
}

# ── colour helpers ──────────────────────────────────────────────────
puts "colour blending"
ok "blend of a colour with itself is unchanged" \
    {[::vtext::blend "#5c3a8f" "#5c3a8f" 0.5] eq "#5c3a8f"}
ok "a half blend of black and white is grey" \
    {[::vtext::blend "#ffffff" "#000000" 0.5] eq "#7f7f7f"}
ok "frac 1.0 keeps the first colour" \
    {[::vtext::blend "#123456" "#abcdef" 1.0] eq "#123456"}
ok "frac 0.0 gives the second" \
    {[::vtext::blend "#123456" "#abcdef" 0.0] eq "#abcdef"}
ok "garbage in is returned unchanged, not an error" \
    {[::vtext::blend "red" "#000000" 0.5] eq "red"}

# ── the selection must be visible in BOTH states ────────────────────
puts "\nselection visibility"
set w [fresh_ro]
set sb [$w cget -selectbackground]
set isb [$w cget -inactiveselectbackground]
set sfg [$w cget -selectforeground]
ok "the focused selection uses the theme colour" {$sb eq $::vt::T(select_bg)}
ok "the UNfocused selection is no longer Tk's #c3c3c3" {$isb ne "#c3c3c3"}
ok "…and is derived from the theme, not the default" \
    {$isb eq [::vtext::inactive_colour]}

# The real defect was contrast: near-white text on light grey. Assert
# the unfocused selection is dark enough to sit under light text.
proc luminance {hex} {
    if {![regexp {^#([0-9a-fA-F]{6})$} $hex]} { return -1 }
    scan [string range $hex 1 2] %x r
    scan [string range $hex 3 4] %x g
    scan [string range $hex 5 6] %x b
    return [expr {(0.299 * $r + 0.587 * $g + 0.114 * $b) / 255.0}]
}
ok "selected-text colour is light" {[luminance $sfg] > 0.6}
ok "…so the unfocused selection background must be dark" \
    {[luminance $isb] < 0.5}
ok "…which Tk's default was NOT (this is the bug)" {[luminance "#c3c3c3"] > 0.5}

# ── read-only panes must be reachable by keyboard ───────────────────
puts "\nkeyboard reach"
set w [fresh_ro]
ok "a read-only pane accepts focus" {[tk::FocusOK $w]}
entry .e
pack .e
update
focus -force .e
update
ok "…and Tab traversal actually lands on it" {[tk_focusNext .e] eq $w}
catch { destroy .e }

# Without -takefocus it does not — the state this was in.
catch { destroy .plain }
text .plain -state disabled
pack .plain
.plain configure -takefocus ""
update
ok "a Text left at Tk's default is skipped (the original behaviour)" \
    {![tk::FocusOK .plain]}
catch { destroy .plain }

# ── emacs motion in a read-only buffer ──────────────────────────────
puts "\nemacs bindings on read-only text"
::vemacs::apply_mode emacs
set w [fresh_ro]
focus -force $w
update

proc key {seq} { event generate .ro $seq ; update }

$w mark set insert 1.6
key <Control-a>
ok "C-a goes to line start" {[$w index insert] eq "1.0"}
key <Control-e>
ok "C-e goes to line end" {[$w index insert] eq "1.16"}
key <Control-n>
ok "C-n moves down a line" {[string match "2.*" [$w index insert]]}
key <Control-p>
ok "C-p moves back up" {[string match "1.*" [$w index insert]]}
key <Alt-greater>
ok "M-> goes to the end of the buffer" {[string match "3.*" [$w index insert]]}
key <Alt-less>
ok "M-< goes to the top" {[$w index insert] eq "1.0"}
key <Alt-f>
ok "M-f moves over a word" {[$w index insert] eq "1.5"}
key <Alt-b>
ok "M-b moves back" {[$w index insert] eq "1.0"}

# mark + copy, the part that read-only buffers genuinely need
$w mark set insert 1.0
key <Control-space>
$w mark set insert 1.5
set ::vemacs::kill_ring {}
key <Alt-w>
ok "C-space then M-w copies the region" \
    {[lindex $::vemacs::kill_ring 0] eq "alpha"}
ok "…without modifying the buffer" \
    {[string match "alpha beta gamma*" [$w get 1.0 end-1c]]}
ok "…and the widget is still read-only" {[$w cget -state] eq "disabled"}

# The destructive half must not be OURS at class level — on a disabled
# widget ::vemacs::_run would raise inside the binding and land in
# bgerror. (Tk has its own C-k/C-w/C-y class bindings; those are
# no-ops on a disabled widget, which is why they are left alone.)
foreach seq {<Control-k> <Control-w> <Control-y> <Alt-y> <Alt-d>} {
    ok "$seq carries no ::vemacs handler on the Text class" \
        {![string match "*::vemacs::*" [bind Text $seq]]}
}

# Pressing them must be harmless rather than an error.
set ::bgerr ""
proc bgerror {m} { set ::bgerr $m }
foreach seq {<Control-k> <Control-w> <Control-y> <Alt-d>} { key $seq }
ok "…and pressing them raises nothing" {$::bgerr eq ""}
ok "…and changes nothing" {[string match "alpha beta gamma*" [$w get 1.0 end-1c]]}

# ── normie mode gives Tk's own bindings back ────────────────────────
puts "\nmode gating"
::vemacs::apply_mode normie
ok "C-space is restored to exactly what Tk had" \
    {[bind Text <Control-space>] eq $::vemacs::_tk_text(<Control-space>)}
# C-a is the ONE key normie deliberately re-assigns: GUI law says
# Ctrl-A selects ALL (the emacs line-start meaning stays with the
# emacs/vi modes, C-x h there). So the binding is NOT what Tk shipped.
ok "normie re-binds C-a to select-all" \
    {[string match "*sel 1.0 end*" [bind Text <Control-a>]]}
# Behaviour: the selection really covers the buffer.
set w2 [fresh_ro]
focus -force $w2
update
$w2 mark set insert 1.6
event generate .ro <Control-a>
update
ok "…and C-a in normie selects the whole buffer" \
    {[catch {lassign [$w2 tag ranges sel] _s _e}]==0 && $_s eq "1.0"
     && [$w2 compare $_e >= end-1c]}
::vemacs::apply_mode emacs
ok "switching back reinstalls them" \
    {[string match "*set_mark*" [bind Text <Control-space>]]}
ok "…and hands C-a back to emacs line-start" \
    {[string match "*linestart*" [bind Text <Control-a>]]}

# ── restyle reaches widgets that already exist ──────────────────────
puts "\ntheme switching"
catch { destroy .old }
text .old
pack .old
.old configure -inactiveselectbackground "#c3c3c3" -takefocus "" \
    -insertbackground "#000000"
update
::vtext::restyle_all
ok "restyle fixes an existing widget's inactive selection" \
    {[.old cget -inactiveselectbackground] ne "#c3c3c3"}
ok "…gives it keyboard focus" {[.old cget -takefocus] == 1}
ok "…and a visible caret" {[.old cget -insertbackground] eq $::vt::T(pink)}

# A deliberate caret colour must survive. NOT $T(accent) — in every
# bundled palette accent and pink are the same colour, so that version
# of this check passed even when restyle_all overwrote it.
.old configure -insertbackground "#00ff00"
::vtext::restyle_all
ok "a caret colour someone chose on purpose is left alone" \
    {[.old cget -insertbackground] eq "#00ff00"}
ok "(and the guard colour is genuinely not the theme's)" \
    {$::vt::T(pink) ne "#00ff00"}

ok "::vtext::all finds nested widgets" {[lsearch [::vtext::all .] .old] >= 0}
catch { destroy .old }

# ── read-only WITHOUT -state disabled ───────────────────────────────
#
# USER: "The yanking/killring still don't show a cursor in the read-only
# areas".
#
# `-state disabled` hides the insertion cursor outright, so emacs point
# motion moved an invisible point. ::vtext::readonly keeps the widget
# `normal` — caret visible, point real, inserts not silently eaten — and
# refuses the modifying events through a bindtag instead.
puts "\nread-only without disabling"

catch { destroy .ro2 }
text .ro2
pack .ro2
.ro2 insert end "alpha beta\nsecond line"
::vtext::readonly .ro2
update
focus -force .ro2
update

ok "the widget stays normal, not disabled" {[.ro2 cget -state] eq "normal"}
ok "it reports itself read-only" {[::vtext::is_readonly .ro2]}
ok "the caret is visible, not Tk's black default" \
    {[.ro2 cget -insertbackground] ni {#000000 black}}
ok "…and has width" {[.ro2 cget -insertwidth] > 0}
ok "…and blinks (an off-time means a blinking caret)" {[.ro2 cget -insertofftime] > 0}

set before [.ro2 get 1.0 end-1c]
proc rokey {seq args} { eval [list event generate .ro2 $seq] $args ; update }

rokey <KeyPress-x> -state 0
ok "typing a letter does not change the text" {[.ro2 get 1.0 end-1c] eq $before}
rokey <KeyPress-space> -state 0
ok "…nor does space" {[.ro2 get 1.0 end-1c] eq $before}
.ro2 mark set insert 1.3
rokey <BackSpace> -state 0
ok "…nor BackSpace" {[.ro2 get 1.0 end-1c] eq $before}
rokey <Delete> -state 0
ok "…nor Delete" {[.ro2 get 1.0 end-1c] eq $before}
rokey <Return> -state 0
ok "…nor Return" {[.ro2 get 1.0 end-1c] eq $before}
event generate .ro2 <<Paste>> ; update
ok "…nor a paste" {[.ro2 get 1.0 end-1c] eq $before}

# But the POINT must still move — that is the entire reason to keep the
# widget enabled.
.ro2 mark set insert 1.0
rokey <Right>
ok "the point still moves with the arrow keys" {[.ro2 index insert] eq "1.1"}
rokey <Down>
ok "…and vertically" {[string match "2.*" [.ro2 index insert]]}

# The app itself must still be able to write to it: a read-only pane
# that its own renderer cannot fill is the -state disabled trap again.
.ro2 insert end "\nappended by the app"
ok "the app can still insert programmatically" \
    {[string match "*appended by the app*" [.ro2 get 1.0 end-1c]]}

# Control chords must fall THROUGH the read-only tag to the emacs
# bindings on the Text class — the bindtag sits before Text, so a
# too-eager block here would kill every emacs key in every pane.
::vemacs::apply_mode emacs
.ro2 mark set insert 2.4
event generate .ro2 <Control-a> ; update
ok "C-a still reaches the emacs binding through the read-only tag" \
    {[.ro2 index insert] eq "2.0"}
event generate .ro2 <Control-space> ; update
event generate .ro2 <Control-f> ; update
event generate .ro2 <Control-f> ; update
ok "…and so does the mark, so the region works in a read-only pane" \
    {[.ro2 tag ranges sel] eq {2.0 2.2}}
# ALT chords are the ones the modifier exemption actually protects: M-f
# arrives with %A = "f", a printable character. Without the Control/Alt
# check, _is_edit_key would call that typing and swallow every M- key in
# every read-only pane. (Control chords survive either way — %A is a
# control character, which the code check already rejects.)
event generate .ro2 <Control-g> ; update
# `event generate <Alt-f>` on a bare X server carries Tk's virtual Alt
# bit (0x20000), NOT Mod1, and does not match `bind <Alt-f>`. A real
# keyboard sends Mod1, so send Mod1 — otherwise this tests the test
# harness rather than the binding.
set MOD1 8
set before2 [.ro2 get 2.0 "2.0 lineend"]
.ro2 mark set insert 2.0
event generate .ro2 <Key-f> -state $MOD1 ; update
ok "M-f reaches the emacs binding (the Alt exemption)" \
    {[.ro2 index insert] ne "2.0"}
ok "…and M-f did not type an 'f'" {[.ro2 get 2.0 "2.0 lineend"] eq $before2}
event generate .ro2 <Key-b> -state $MOD1 ; update
ok "M-b too" {[.ro2 index insert] eq "2.0"}
# The predicate itself, directly. Through the widget, a virtual-Alt "f"
# is indistinguishable from a blocked one — `bind <Alt-f>` does not
# match that state either way, so the buffer is unchanged whichever
# branch ran. The rule is only observable at the function.
ok "_is_edit_key: a plain letter IS typing"        {[::vtext::_is_edit_key f 0 f] == 1}
ok "_is_edit_key: Control+letter is not"           {[::vtext::_is_edit_key f 4 f] == 0}
ok "_is_edit_key: Mod1+letter is not"              {[::vtext::_is_edit_key f 8 f] == 0}
ok "_is_edit_key: virtual Alt (0x20000) is not"    {[::vtext::_is_edit_key f 131072 f] == 0}
ok "_is_edit_key: virtual Meta (0x40000) is not"   {[::vtext::_is_edit_key f 262144 f] == 0}
ok "_is_edit_key: a bare arrow key is not typing"  {[::vtext::_is_edit_key "" 0 Left] == 0}
ok "_is_edit_key: Return IS an edit"               {[::vtext::_is_edit_key "\r" 0 Return] == 1}
ok "_is_edit_key: BackSpace IS an edit"            {[::vtext::_is_edit_key "\b" 0 BackSpace] == 1}
ok "_is_edit_key: Tab is navigation, not text"     {[::vtext::_is_edit_key "\t" 0 Tab] == 0}
ok "_is_edit_key: Escape is not text"              {[::vtext::_is_edit_key "\x1b" 0 Escape] == 0}
::vemacs::apply_mode normie
catch { destroy .ro2 }

# ── the selection must be VISIBLE, whatever the theme says ──────────
#
# USER, twice: "the selection is still only visible when selected with
# the mouse". The tag was right; the COLOUR was not. ::vt::from_engine
# maps an engine theme's `panel_hi` onto select_bg, and panel_hi is a
# subtle panel tint — a couple of shades off the background.
puts "\nselection contrast"

ok "luminance reads a colour" {[::vtext::luminance "#ffffff"] > 0.9}
ok "…and the dark end" {[::vtext::luminance "#000000"] < 0.1}
ok "garbage luminance is -1, not an error" {[::vtext::luminance "nope"] == -1}

# The real failing case: select_bg two shades off the background.
set ::vt::T(bg_input) "#1a1024"
set ::vt::T(select_bg) "#2e1e4a"
set ::vt::T(accent) "#ff3d8a"
set c [::vtext::selection_colour]
ok "a too-subtle select_bg is pushed away from the background" \
    {abs([::vtext::luminance $c] - [::vtext::luminance "#1a1024"]) >= 0.18}
ok "…and it is not the theme's unusable value" {$c ne "#2e1e4a"}

# A palette that ALREADY contrasts must be left alone.
set ::vt::T(select_bg) "#5c3a8f"
set good [::vtext::selection_colour]
ok "a usable select_bg is used as-is" {$good eq "#5c3a8f"}

# Even a select_bg identical to the background resolves to something
# visible rather than looping forever.
set ::vt::T(select_bg) "#1a1024"
set same [::vtext::selection_colour]
ok "select_bg == background still yields a visible colour" \
    {abs([::vtext::luminance $same] - [::vtext::luminance "#1a1024"]) >= 0.18}

# The inactive colour is derived from the CORRECTED selection, so it
# cannot inherit the unusable one.
set ::vt::T(select_bg) "#2e1e4a"
ok "the inactive colour derives from the corrected selection" \
    {[::vtext::inactive_colour] ne [::vtext::blend "#2e1e4a" "#1a1024" 0.55]}

# ── Chinese and Japanese line breaks (nix/tk-kinsoku.patch) ──────────
#
# Tk wraps words only at ASCII spaces, and Chinese and Japanese put none
# between words: a reply in either wrapped wherever it ran out of room,
# and a line could open with "。" (2026-10-01). The desktop's Tk breaks
# them where those languages do. -wrap char is the control: it breaks
# anywhere, so the same text at the same widths puts "。" first, which
# shows the widths reach the case at all.
set kin_ja "これは長い日本語の文です。句読点が行頭に来てはいけません、「括弧」も同じです。ツールの実行が終わりました。次の質問をどうぞ。"
set kin_en "The quick brown fox jumps over the lazy dog and then over the lazy dog again"
set kin_nostart [split "。、」』）ーっゃゅょ" ""]
set kin_noend [split "「『（" ""]
toplevel .kin
pack [text .kin.t -font {TkDefaultFont 12} -borderwidth 0 \
    -highlightthickness 0 -padx 0 -width 1 -height 1] -fill both -expand 1
proc kin_starts {mode text} {
    .kin.t configure -wrap $mode
    .kin.t delete 1.0 end
    .kin.t insert end $text
    set starts {}
    for {set px 80} {$px <= 400} {incr px 9} {
        wm geometry .kin ${px}x300
        update
        set i 1.0
        while 1 {
            set next [.kin.t index "$i +1 display lines display linestart"]
            if {[.kin.t compare $next <= $i]
                    || [.kin.t compare $next >= "end -1c"]} break
            lappend starts [.kin.t get "$next -1c"] [.kin.t get $next]
            set i $next
        }
    }
    return $starts
}
proc kin_bad {starts} {
    set bad 0
    foreach {before first} $starts {
        if {$first in $::kin_nostart || $before in $::kin_noend} { incr bad }
    }
    return $bad
}
ok "-wrap char puts \"。\" first somewhere (the control)" \
    {[kin_bad [kin_starts char $kin_ja]] > 0}
set kin_word [kin_starts word $kin_ja]
ok "-wrap word wraps Japanese with no spaces in it" {[llength $kin_word] > 0}
ok "…and no line opens with 。、」 or ends with 「" {[kin_bad $kin_word] == 0}
set kin_en_starts [kin_starts word $kin_en]
set kin_mid 0
foreach {before first} $kin_en_starts { if {$before ne " "} { incr kin_mid } }
ok "English still wraps only at its spaces" \
    {[llength $kin_en_starts] > 0 && $kin_mid == 0}

# Labels, canvas text and message boxes lay out through
# Tk_ComputeTextLayout, which the patch covers too.
pack forget .kin.t
pack [canvas .kin.c -width 500 -height 400 -highlightthickness 0]
wm geometry .kin 500x400
set kin_id [.kin.c create text 0 0 -anchor nw -font {TkDefaultFont 12} \
    -text $kin_ja]
set kin_ls [font metrics {TkDefaultFont 12} -linespace]
set kin_cbad 0
set kin_clines 0
for {set px 80} {$px <= 400} {incr px 9} {
    .kin.c itemconfigure $kin_id -width $px
    update
    set y1 [lindex [.kin.c bbox $kin_id] 3]
    for {set y [expr {$kin_ls + $kin_ls / 2}]} {$y < $y1} {incr y $kin_ls} {
        set i [.kin.c index $kin_id @0,$y]
        incr kin_clines
        if {[string index $kin_ja $i] in $kin_nostart
                || [string index $kin_ja [expr {$i - 1}]] in $kin_noend} {
            incr kin_cbad
        }
    }
}
ok "canvas text wraps Japanese, and never before 。 or after 「" \
    {$kin_clines > 0 && $kin_cbad == 0}
destroy .kin

puts "\ntexttest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
