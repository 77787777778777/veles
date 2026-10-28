# veles-desktop/lib/titletest.tcl — behavioural tests for the in-app
# window controls, and specifically for maximise.
#
# USER: "the maximize button functions differently than the OS maximize,
# doesn't seem to respect size of my current screen".
#
# The old toggle_max used `winfo screenwidth`, whose comment claimed it
# was "the usable screen, not the virtual desktop". On X11 with Xinerama
# it is exactly the virtual desktop — every monitor in one rectangle —
# so maximising spanned all of them.
#
# The existing checks for this module were `string match` against the
# source text, which cannot tell a working toggle_max from a broken one.
# These press the button.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]
namespace eval ::vstatus { proc set_msg {m} { set ::vstatus::last $m } }
set ::vstatus::last ""
# titlebar.tcl decorates its buttons through these.
namespace eval ::va11y { proc name {w t} {} }
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }
namespace eval ::vt { variable T ; array set T {accent #88c0d0} }
source [file join $_dir titlebar.tcl]

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

wm withdraw .

# A pretend two-monitor layout: a laptop panel with a larger screen to
# its right. Injected, so the tests do not depend on the machine they
# run on (Xvfb has exactly one screen).
proc two_monitors {} {
    set ::vtitle::monitor_cache {{0 0 1920 1200} {1920 0 2560 1440}}
}
proc no_monitors {} { set ::vtitle::monitor_cache {} }

# ── the xrandr parse ────────────────────────────────────────────────
puts "xrandr parsing"
# Exactly what `xrandr --listmonitors` prints, second monitor offset.
set raw "Monitors: 2
 0: +*eDP-1 1920/344x1200/215+0+0  eDP-1
 1: +HDMI-1 2560/597x1440/336+1920+0  HDMI-1"
set parsed {}
foreach line [split $raw \n] {
    if {[regexp {([0-9]+)/[0-9]+x([0-9]+)/[0-9]+\+([0-9]+)\+([0-9]+)} $line -> mw mh mx my]} {
        lappend parsed [list $mx $my $mw $mh]
    }
}
ok "both monitors are found" {[llength $parsed] == 2}
ok "sizes are the pixel sizes, not the millimetre ones" \
    {[lindex $parsed 0] eq {0 0 1920 1200}}
ok "the offset monitor keeps its origin" {[lindex $parsed 1] eq {1920 0 2560 1440}}

# ── which monitor is a point on ─────────────────────────────────────
puts "\nmonitor_at"
two_monitors
ok "a point on the left panel finds the left monitor" \
    {[::vtitle::monitor_at 100 100] eq {0 0 1920 1200}}
ok "a point past the seam finds the right monitor" \
    {[::vtitle::monitor_at 2000 300] eq {1920 0 2560 1440}}
ok "the seam itself belongs to the right monitor" \
    {[::vtitle::monitor_at 1920 0] eq {1920 0 2560 1440}}
ok "the last pixel of the left monitor is still the left one" \
    {[::vtitle::monitor_at 1919 1199] eq {0 0 1920 1200}}
ok "a point below everything matches nothing" {[::vtitle::monitor_at 100 5000] eq ""}
no_monitors
ok "with no layout known, nothing matches" {[::vtitle::monitor_at 100 100] eq ""}

# ── maximise, undecorated: the reported bug ─────────────────────────
puts "\nmaximise without a window manager to ask"
proc fresh_win {geo} {
    catch { destroy .w }
    toplevel .w
    wm overrideredirect .w 1
    wm geometry .w $geo
    update idletasks
    array unset ::vtitle::maximised
    return .w
}

two_monitors
# A window sitting on the SECOND monitor.
fresh_win "400x300+2100+200"
ok "the window really is on the right monitor" {[winfo rootx .w] >= 1920}
::vtitle::toggle_max .w
update idletasks
set geo [wm geometry .w]
ok "maximises to the monitor it is on, not the whole X screen" \
    {$geo eq "2560x1440+1920+0"}
ok "…and that is NOT the full virtual desktop" \
    {![string match "4480x*" $geo] && ![string match "*+0+0" $geo]}

::vtitle::toggle_max .w
update idletasks
ok "toggling again restores the previous geometry" \
    {[string match "400x300+*" [wm geometry .w]]}

# The first monitor must work too — a fix that always picks monitor 2 is
# just as wrong.
fresh_win "400x300+100+100"
::vtitle::toggle_max .w
update idletasks
ok "a window on the left monitor maximises to the left monitor" \
    {[wm geometry .w] eq "1920x1200+0+0"}

# ── degradation when the layout is unknown ──────────────────────────
puts "\nno xrandr"
no_monitors
set ::vstatus::last ""
fresh_win "400x300+100+100"
::vtitle::toggle_max .w
update idletasks
ok "still maximises to something" {[string match "*x*+0+0" [wm geometry .w]]}
ok "and SAYS the layout is unknown rather than silently spanning" \
    {[string match "*xrandr is unavailable*" $::vstatus::last]}

# ── decorated windows are the WM's job ──────────────────────────────
puts "\ndecorated windows delegate"
catch { destroy .w }
toplevel .w
wm geometry .w 400x300+100+100
update idletasks
array unset ::vtitle::maximised
ok "the window reports itself decorated" {[::vtitle::_decorated .w]}
::vtitle::toggle_max .w
update idletasks
# The WM (there is none under bare Xvfb) owns the restore geometry, so
# nothing may be recorded here — a stale entry is what made our button
# disagree with the OS one after maximising with the OS control.
ok "no restore geometry is recorded for a decorated window" \
    {![info exists ::vtitle::maximised(.w)]}
wm overrideredirect .w 1
update idletasks
ok "an overrideredirect window reports itself undecorated" \
    {![::vtitle::_decorated .w]}

# ── the cache ───────────────────────────────────────────────────────
puts "\nmonitor cache"
two_monitors
ok "cached layout is returned" {[llength [::vtitle::monitors]] == 2}
::vtitle::forget_monitors
ok "forget_monitors drops it" {![info exists ::vtitle::monitor_cache]}
# Re-querying must not error even when xrandr is missing (it is, in the
# pure dev shell) — absence has to be survivable. `[llength] == 0 ||
# [llength] > 0` was the first version of this: always true, so it would
# have passed against a monitors{} that threw.
set threw [catch { ::vtitle::monitors } again]
ok "a re-query with no xrandr does not raise" {!$threw}
ok "…and yields a well-formed (possibly empty) list of rectangles" \
    {[lsearch -not -regexp $again {^[0-9]+ [0-9]+ [0-9]+ [0-9]+$}] < 0}
ok "…and repopulates the cache" {[info exists ::vtitle::monitor_cache]}

# ── the preference is durable ───────────────────────────────────────
#
# The module header used to cite `[ui] native_titlebar` in config.toml.
# No [ui] section exists anywhere in the config — the key was documented
# and never implemented, so the toggle reset on every start.
puts "\nnative title bar preference"

# A fake state blob, so this does not need a running backend.
namespace eval ::vstate {
    variable fake
    array set fake {}
    proc get {key {default ""}} {
        variable fake
        if {[info exists fake($key)]} { return $fake($key) }
        return $default
    }
    proc put {key value} { variable fake ; set fake($key) $value }
}
array unset ::vstate::fake
array set ::vstate::fake {}

# A window that carries a strip, so set_native is willing to act on it.
catch { destroy .p }
toplevel .p
::vtitle::attach .p "test"
update idletasks
set ::vtitle::native 1

::vtitle::toggle_native
ok "toggling off records the choice" \
    {[::vstate::get native_titlebar] == 0}
ok "…and the window really lost its decorations" \
    {[wm overrideredirect .p] == 1}

::vtitle::toggle_native
ok "toggling back records that too" {[::vstate::get native_titlebar] == 1}
ok "…and the decorations came back" {[wm overrideredirect .p] == 0}

# A fresh start with "off" saved must come up undecorated.
set ::vstate::fake(native_titlebar) 0
unset -nocomplain ::vtitle::native
::vtitle::restore_native
ok "a saved 'off' is applied at startup" {$::vtitle::native == 0}
ok "…and reaches the windows" {[wm overrideredirect .p] == 1}

# The default is decorated: an unset preference must not strip the
# window (that would leave a first-time user with no way to move it).
wm overrideredirect .p 0
array unset ::vstate::fake
array set ::vstate::fake {}
unset -nocomplain ::vtitle::native
::vtitle::restore_native
ok "with nothing saved the default is native decorations" {$::vtitle::native == 1}
ok "…and no window is stripped" {[wm overrideredirect .p] == 0}
catch { destroy .p }

puts "\ntitletest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
