# veles-desktop/lib/whichkeytest.tcl — which-key, and the coverage the
# registry is supposed to guarantee.
#
# USER: "Did which-key and reverse-which-key just silently disappear? I
# don't see any hints for keyboard shortcuts when I click, a lot of
# stuff doesn't even seem to have a keyboard shortcut because it was
# never added."
#
# Nothing disappeared. Two separate things were true:
#
#   1. ::vwhichkey::arm fires ONLY from ::vkeymap::_arm_prefix, which is
#      only bound when the active mode has a `prefix` row. Those live in
#      _emacs_prefixes, appended to emacs and vi only — normie, the
#      shipped default, has none by deliberate design (commit 267d355),
#      so the popup could never appear in it. F2 / ::vwhichkey::show_all
#      is the answer that does not undo that decision.
#   2. 23 of 43 palette actions had no key in any mode. ACL and
#      Mechanisms shipped without bindings.tcl being touched at all.
#
# And NOTHING TESTED ANY OF IT: uitest stubs ::vwhichkey::arm and hide
# to no-ops for unrelated pane tests, so a real rendering regression —
# a theme key, a ::vplace call — would have gone straight to the user.
# This suite drives the real popup.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]

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

if {![info exists ::env(XVFB_RUNNING)] && [info exists ::env(DISPLAY)]} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
# ::vi18n::label/aname are the REGISTERING shapes (a -text with format
# args must use them so a live swap can re-render it). whichkey.tcl uses
# label() for its title, so a suite that sources it without the real
# i18n.tcl needs the stub or the popup never renders.
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
namespace eval ::vstatus {
    variable last ""
    proc set_msg {m} { variable last $m }
}
source [file join $_dir place.tcl]
source [file join $_dir bindings.tcl]

# A theme deep enough for the popup's four colours.
namespace eval ::vt {
    variable T
    array set T {accent #f0f gold #fc0 fg #eee fg_dim #888 bg_panel #222 hairline #333}
}
namespace eval ::vkeymap { variable current normie }
source [file join $_dir whichkey.tcl]

# ── the sheet renders, for real ─────────────────────────────────────
puts "the sheet renders"

set w [::vwhichkey::show_all normie]
update
ok "show_all maps a window"        {[winfo exists .whichkey]}
ok "…and it is the popup"          {$w eq ".whichkey"}
ok "…named for the mode"           {[string match "normie*" [.whichkey.f.title cget -text]]}

# Every row of the registry has to be ON it. A sheet that silently drops
# the last column is worse than no sheet: it teaches that a key does not
# exist.
proc labels_on_sheet {} {
    set out {}
    foreach c [winfo children .whichkey.f] {
        catch { lappend out [$c cget -text] }
    }
    return $out
}
set shown [labels_on_sheet]
set missing {}
foreach entry [::vbindings::describe normie] {
    lassign $entry keys help category
    if {[lsearch -exact $shown $keys] < 0} { lappend missing $keys }
}
foreach m $missing { puts "      not on the sheet: $m" }
ok "every normie chord is on the sheet" {[llength $missing] == 0}

set rows [llength [::vbindings::describe normie]]
ok "the sheet is not one long column" \
    {[expr {[winfo reqheight .whichkey] < 40 * $rows}]}

# ── it gives `.` its binding back ───────────────────────────────────
#
# The sheet borrows `bind . <Key>` to dismiss itself. `all <KeyPress>`
# is keymap.tcl's prefix-dispatch slot and `<Key>` is the same event, so
# binding there would silently break C-x and M-o; borrowing `.` instead
# is only safe if it is handed back.
puts "\nthe dismiss binding is borrowed, not kept"

::vwhichkey::hide
update
ok "hide destroys the popup"   {![winfo exists .whichkey]}
ok "…and clears its binding"   {[bind . <Key>] eq ""}

bind . <Key> {set ::probe_ran 1}
::vwhichkey::show_all normie
ok "an existing binding is saved" {[string match "*::vwhichkey*" [bind . <Key>]]}
::vwhichkey::hide
ok "…and restored on hide"        {[bind . <Key>] eq "set ::probe_ran 1"}
bind . <Key> {}

# Showing twice must not stack handlers or leak a window.
::vwhichkey::show_all normie
::vwhichkey::show_all normie
update
ok "showing twice leaves one window" {[llength [winfo children .]] >= 1 && [winfo exists .whichkey]}
::vwhichkey::hide
ok "…and one hide clears it"        {![winfo exists .whichkey]}

# ── the armed popup still works ─────────────────────────────────────
puts "\nthe armed popup is unchanged"

set ::vkeymap::current emacs
::vwhichkey::arm "C-x"
update
ok "arming a prefix shows its continuations" {[winfo exists .whichkey]}
set conts [::vbindings::prefix_continuations "C-x"]
set shown [labels_on_sheet]
set missing {}
foreach row $conts {
    if {[lsearch -exact $shown [lindex $row 0]] < 0} { lappend missing [lindex $row 0] }
}
ok "every continuation is listed" {[llength $missing] == 0}
::vwhichkey::hide
ok "arming an unknown prefix shows nothing" \
    {[::vwhichkey::arm "C-zzz"] eq "" && ![winfo exists .whichkey]}
set ::vkeymap::current normie

# ── coverage: the gap the user reported ─────────────────────────────
puts "\nevery palette action has a key"

source [file join $_dir palette.tcl]

# Actions that are deliberately keyless, with the reason for each. This
# is the escape hatch that would otherwise let the coverage gap grow
# back, so it stays short and every line justifies itself.
set exempt {
    {::vtitle::restore_all}    ;# recovery for a window lost off-screen
    {::vtitle::toggle_native}  ;# a one-off WM preference, set and forgotten
    {::vfg::edit_script}       ;# F8 opens the blueprint; the hook script
                                ;# behind it is a second-level surface
    {exit}                     ;# a key that quits the app is a trap
}
set exempt [lmap e $exempt {string trim $e}]
foreach mode {normie emacs vi} {
    set gaps {}
    foreach item [::vpalette::actions] {
        lassign $item label action
        if {[lsearch -exact $exempt [string trim $action]] >= 0} { continue }
        # A compound action ("show dev ; buf_new") inherits no key on
        # purpose — advertising one would promise the whole thing.
        if {[string first ";" $action] >= 0} { continue }
        if {[::vbindings::keys_for $action $mode] eq ""} { lappend gaps $label }
    }
    foreach g $gaps { puts "      $mode has no key for: $g" }
    ok "$mode: every palette action resolves to a key" {[llength $gaps] == 0}
}

# The flat manager chords must fill GAPS, never shadow a mode's own
# idiom: emacs reaches the overlays through M-o and its fingers know it.
ok "emacs keeps M-o for the overlays it already had" \
    {[::vbindings::keys_for {::voverlay::open models} emacs] eq "M-o m"}
ok "normie gets a flat chord for the same thing" \
    {[::vbindings::keys_for {::voverlay::open models} normie] eq "C-S-M"}
ok "a surface M-o never had is flat in both" \
    {[::vbindings::keys_for {::voverlay::open hooks} emacs] eq "C-S-H"
     && [::vbindings::keys_for {::voverlay::open hooks} normie] eq "C-S-H"}

# ── no chord is documented only in a widget label ───────────────────
#
# dev.tcl binds ~12 C-c chords directly on its widgets and used to write
# the chord into each button's -text by hand. bindings.tcl is the ONE
# registry every hint surface reads, so a chord that lives only in a
# label is invisible to help, the F2 sheet, the palette column and the
# coach — the exact drift the registry was created to end.
puts "\nwidget-local chords are still in the registry"

proc slurp {path} {
    set fh [open $path r]; set d [read $fh]; close $fh; return $d
}
set dev [slurp [file join $_dir dev.tcl]]
set literal {}
foreach {- chord} [regexp -all -inline {\(C-c C-([a-z])\)} $dev] {
    lappend literal "C-c C-$chord"
}
set undeclared {}
foreach chord [lsort -unique $literal] {
    set found 0
    foreach row [::vbindings::table normie] {
        if {[lindex $row 0] eq $chord} { set found 1 ; break }
    }
    if {!$found} { lappend undeclared $chord }
}
foreach u $undeclared { puts "      chord in a label but not the registry: $u" }
ok "every C-c chord dev.tcl advertises is declared" {[llength $undeclared] == 0}

# The rows stay the Dev pane's: dev.tcl binds them on its widgets
# (uitest.tcl checks the keymap binds none of them globally).
set bad {}
foreach row [::vbindings::_dev_pane] {
    lassign $row keys target action help category
    if {$target ni {dev repl}} { lappend bad $keys }
}
ok "…as the Dev pane's rows" {[llength $bad] == 0}

# And each says what its chord DOES. Three did not (2026-10-01): the
# help screen said C-c C-s saves (it is signature help; C-x C-s saves),
# C-c C-o opens a file (the outline), C-c C-g evaluates in the gateway
# (it sets the REPL's target). Read dev.tcl's own binds and require a
# row with that chord running that command; the command tree runs the
# row's action, so a wrong one is a wrong command now, not a wrong
# word.
proc _dev_chord {seq} {
    set out {}
    foreach tok [regexp -all -inline {<[^>]+>|[^<]} $seq] {
        set tok [string trim $tok "<>"]
        if {[string match Control-* $tok]} { set tok "C-[string range $tok 8 end]" }
        lappend out $tok
    }
    return [join $out " "]
}
set dev_binds {}
foreach {- seq call} [regexp -all -inline -line \
        {bind \$w(?:\.repl\.in)? ((?:<Control-[cx]>)(?:<[^>]+>|[0-9a-z]))\s+\{ (::vdev::[^;]+?) ; break \}} $dev] {
    lappend dev_binds [_dev_chord $seq] [string trim $call]
}
set wrong {}
foreach {chord call} $dev_binds {
    set found 0
    foreach row [::vbindings::_dev_pane] {
        lassign $row keys target action
        if {$keys eq $chord && [string match "*; $call" [::vbindings::_norm $action]]} { set found 1 }
    }
    if {!$found} { lappend wrong "$chord runs $call" }
}
foreach w $wrong { puts "      dev.tcl binds, the registry does not say: $w" }
ok "dev.tcl's chords were read"                   {[llength $dev_binds] / 2 >= 15}
ok "every Dev chord's row runs what dev.tcl binds" {[llength $wrong] == 0}

# ── the coach sweeps, at the three choke points ─────────────────────
puts "\nthe coach reaches every affordance"

set sweeps 0
foreach {file needle} [list \
    [file join $_dir .. main.tcl]   "vcoach::attach_all ." \
    [file join $_dir overlays.tcl]  "vcoach::attach_all \$w" \
    [file join $_dir panes.tcl]     "vcoach::attach_all \$::vpanes::R"] {
    if {[string match "*$needle*" [slurp $file]]} { incr sweeps }
}
ok "boot, overlays and panes all sweep" {$sweeps == 3}

puts "\nwhichkeytest: $passed passed, $failed failed"
# ALWAYS exit. `if {$failed > 0} { exit 1 }` left wish sitting in the
# event loop on a green run, and tcl_tests.sh runs the suites in
# sequence — so a PASSING suite hung every suite after it.
exit [expr {$failed > 0 ? 1 : 0}]
