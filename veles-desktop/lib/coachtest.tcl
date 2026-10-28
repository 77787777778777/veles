# veles-desktop/lib/coachtest.tcl — the app teaches its own keyboard.
#
# The desktop could always answer "what does this key do" (help,
# which-key, palette hints, all off bindings.tcl). It could never answer
# the other direction: click a button and the key that would have done
# the same thing stays invisible forever.
#
# The coach answers it, and the only reason it can is that the command
# string an affordance runs is already byte-identical to the registry's
# action column — so `$w cget -command` IS the lookup key and there is
# no table to keep in sync.
#
# What these pin, in rough order of how easy each is to get wrong:
#
#   - the pointer coaches, the keyboard does not. A coach that fires
#     when you already pressed the key is pure noise, and "activated a
#     focused button with Space" is keyboard use too.
#   - it retires. Hints that never stop are a permanent tax on someone
#     who has already learned the app.
#   - the tally is per COMMAND, so reaching an action three different
#     ways teaches one key and counts once per keypress.
#   - the toast is NOT the status bar, which ::vstatus::poll overwrites
#     every 15 seconds.
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

namespace eval ::vstatus {
    variable last ""
    proc set_msg {m} { variable last $m }
}
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }

source [file join $_dir place.tcl]
source [file join $_dir bindings.tcl]
source [file join $_dir keymap.tcl]
source [file join $_dir paths.tcl]
source [file join $_dir a11y.tcl]
source [file join $_dir ctxmenu.tcl]

# The state blob is the coach's memory; keep it in a variable.
namespace eval ::vstate {
    variable blob {}
    proc get {key {default ""}} {
        variable blob
        if {[dict exists $blob $key]} { return [dict get $blob $key] }
        return $default
    }
    proc put {key value} { variable blob ; dict set blob $key $value ; return 1 }
}
source [file join $_dir coach.tcl]

# A theme just deep enough for the toast.
namespace eval ::vt {
    variable T
    array set T {hairline #333 bg_panel #222 fg #eee}
}

set ::vkeymap::current normie
set ::vcoach::enabled 1
set ::vcoach::threshold 3

# Read a widget property without ever throwing. A test that throws
# ABORTS its suite — no ✗, no tally — and the flake reads a missing
# tally as a broken run rather than a red one. Every assertion below
# that inspects a window the code under test may not have created goes
# through one of these.
proc probe {script {default ""}} {
    set v $default
    catch { set v [uplevel 1 $script] }
    return $v
}

proc accel {menu index} {
    return [probe [list $menu entrycget $index -accelerator]]
}

proc reset_coach {} {
    set ::vcoach::seen {}
    set ::vcoach::enabled 1
    set ::vcoach::threshold 3
    set ::vcoach::pointer_fire 0
    set ::vstate::blob {}
    catch { destroy .vcoach_toast }
}

# ── the lookup the whole thing rests on ─────────────────────────────
puts "what key runs this command"

ok "a bound command reports its key" \
    {[::vcoach::hint ::vsidebar::new_session] eq "Ctrl-N"}
ok "the key follows the keymap mode" \
    {[set ::vkeymap::current emacs
      set k [::vcoach::hint ::vsidebar::new_session]
      set ::vkeymap::current normie
      expr {$k eq "M-n"}]}
# A command that is deliberately in no registry, in any mode.
#
# This used to be ::vhotpatch::load, chosen because it happened to have
# no binding — and then it got one (F5), and four tests here started
# failing for the best possible reason. A fixture for "unbound" has to
# be something nobody would ever bind, or it measures the binding table
# rather than the coach.
namespace eval ::vprobe { proc never_bound {} {} }

ok "the fixture really is unbound in every mode" \
    {[expr {[::vbindings::keys_for ::vprobe::never_bound normie] eq ""
         && [::vbindings::keys_for ::vprobe::never_bound emacs] eq ""
         && [::vbindings::keys_for ::vprobe::never_bound vi] eq ""}]}
ok "an unbound command reports nothing, not a guess" \
    {[::vcoach::hint ::vprobe::never_bound] eq ""}
ok "the pane hotkeys resolve" \
    {[::vcoach::hint {::vpanes::show review}] eq "Alt-2"}

# ── clicking teaches, typing does not ───────────────────────────────
puts "\nthe pointer is coached, the keyboard is not"

set ::fired {}
proc ::fake_action {} { lappend ::fired ran }
frame .probe
ttk::button .probe.b -text "New" -command ::vsidebar::new_session
pack .probe .probe.b
update

reset_coach
::vcoach::attach .probe.b
ok "attach rewrote the command through the coach" \
    {[string match "::vcoach::_fire *" [.probe.b cget -command]]}
ok "attach is idempotent" \
    {[::vcoach::attach .probe.b
      llength [.probe.b cget -command]] == 3}

# A real click.
namespace eval ::vsidebar { variable calls 0 ; proc new_session {} { variable calls ; incr calls } }
reset_coach
set ::vsidebar::calls 0
event generate .probe.b <ButtonPress-1> -x 5 -y 5 -when now
event generate .probe.b <ButtonRelease-1> -x 5 -y 5 -when now
update
ok "clicking still runs the command" {$::vsidebar::calls == 1}
ok "clicking raises a toast" {[::vcoach::toast_visible]}
ok "the toast names the key" \
    {[string match "*Ctrl-N*" [::vcoach::toast_text]]}
ok "…and what the button is for" \
    {[string match "*New*" [::vcoach::toast_text]]}

# Keyboard activation of the same button: ttk invokes -command without
# any ButtonRelease, so the coach must stay quiet.
reset_coach
set ::vsidebar::calls 0
.probe.b invoke
update
ok "keyboard activation still runs the command" {$::vsidebar::calls == 1}
ok "keyboard activation raises NO toast" {![::vcoach::toast_visible]}

# Press, drag off, release elsewhere: ttk never invokes, so the flag is
# set and never consumed. It must not leak into the next activation.
reset_coach
event generate .probe.b <ButtonPress-1> -x 5 -y 5 -when now
event generate .probe.b <ButtonRelease-1> -x 5 -y 5 -when now
update
catch { destroy .vcoach_toast }
.probe.b invoke
update
ok "a stale pointer press does not leak into the next keypress" \
    {![::vcoach::toast_visible]}

# ── it retires ──────────────────────────────────────────────────────
puts "\nthe hint stops once the key is learned"

reset_coach
ok "a fresh command is not learned" {![::vcoach::learned ::vsidebar::new_session]}
::vcoach::note_key ::vsidebar::new_session
ok "one keypress counts"     {[::vcoach::times ::vsidebar::new_session] == 1}
ok "one keypress is not enough" {![::vcoach::learned ::vsidebar::new_session]}
::vcoach::note_key ::vsidebar::new_session
::vcoach::note_key ::vsidebar::new_session
ok "the threshold retires it" {[::vcoach::learned ::vsidebar::new_session]}

# …and a learned command no longer toasts on click.
event generate .probe.b <ButtonPress-1> -x 5 -y 5 -when now
event generate .probe.b <ButtonRelease-1> -x 5 -y 5 -when now
update
ok "a learned command no longer toasts" {![::vcoach::toast_visible]}

# The tally is per command, not per widget: the SAME action reached
# from a different affordance is already learned.
ok "learning is per command, not per widget" \
    {[::vcoach::learned {::vsidebar::new_session}]}
ok "a different command is unaffected" \
    {![::vcoach::learned ::vchat::toggle_source]}

# Clicking must never count as learning — otherwise the hint teaches
# itself away without the key ever being pressed.
reset_coach
foreach _i {1 2 3 4 5} {
    event generate .probe.b <ButtonPress-1> -x 5 -y 5 -when now
    event generate .probe.b <ButtonRelease-1> -x 5 -y 5 -when now
    update
}
ok "clicking five times teaches the coach nothing" \
    {[::vcoach::times ::vsidebar::new_session] == 0}
ok "so it is still coaching" {[::vcoach::toast_visible]}

# ── the off switch ──────────────────────────────────────────────────
puts "\nthe off switch"

reset_coach
set ::vcoach::enabled 0
catch { destroy .vcoach_toast }
event generate .probe.b <ButtonPress-1> -x 5 -y 5 -when now
event generate .probe.b <ButtonRelease-1> -x 5 -y 5 -when now
update
ok "disabled means silent" {![::vcoach::toast_visible]}
::vcoach::note_key ::vsidebar::new_session
ok "disabled means it stops counting too" \
    {[::vcoach::times ::vsidebar::new_session] == 0}
set ::vcoach::enabled 1

# An action with NO key has nothing to teach.
reset_coach
ttk::button .probe.n -text "n" -command ::vprobe::never_bound
pack .probe.n
update
::vcoach::attach .probe.n
event generate .probe.n <ButtonPress-1> -x 5 -y 5 -when now
event generate .probe.n <ButtonRelease-1> -x 5 -y 5 -when now
update
ok "an unbound action raises no toast" {![::vcoach::toast_visible]}

# ── scrollbars are never wrapped ─────────────────────────────────────
#
# A scrollbar's -command is invoked BY THE TOOLKIT with arguments
# (`moveto 0.5`, `scroll -1 units`). Wrapping it in the old two-arg
# _fire meant every thumb-drag died in bgerror — and coaching a drag
# is meaningless anyway. Skip at attach; and the wrap must round-trip
# args for anything else that passes them.
puts "\nscrollbars are never coached"

set ::vprobe::moved {}
proc ::vprobe::yview {args} { lappend ::vprobe::moved $args }
text .probe.t
ttk::scrollbar .probe.sb -command [list ::vprobe::yview]
::vcoach::attach .probe.sb
ok "the scrollbar is left alone" \
    {[string match "*vprobe::yview*" [.probe.sb cget -command]]
     && ![string match "*vcoach*" [.probe.sb cget -command]]}
# And the belt-and-braces half: _fire with ARGS still runs the command.
::vcoach::attach .probe.t  ;# a no-op (text has no -command) — just proving no crash
set rc [catch { ::vcoach::_fire .probe.b ::vprobe::yview moveto 0.5 } err]
ok "a wrapped command called WITH args round-trips them" \
    {$rc == 0 && [lindex $::vprobe::moved end] eq "moveto 0.5"}

# ── the toast is not the status bar ─────────────────────────────────
#
# ::vstatus::set_msg is transient: ::vstatus::poll overwrites it every
# 15 seconds with "gateway up Ns", which is why the route needed its own
# label. A coach hint put there is gone before it is read.
puts "\nthe toast is its own surface"

reset_coach
set ::vstatus::last "SENTINEL"
event generate .probe.b <ButtonPress-1> -x 5 -y 5 -when now
event generate .probe.b <ButtonRelease-1> -x 5 -y 5 -when now
update
ok "the hint went to a toast"          {[::vcoach::toast_visible]}
ok "the status line was not clobbered" {$::vstatus::last eq "SENTINEL"}
ok "the toast is a transient popup"    {[probe {wm overrideredirect .vcoach_toast} 0]}

# ── persistence ─────────────────────────────────────────────────────
puts "\nprogress survives a restart"

reset_coach
::vcoach::note_key ::vsidebar::new_session
::vcoach::note_key ::vsidebar::new_session
ok "the tally reached the state blob" \
    {[dict exists [::vstate::get coach_seen {}] ::vsidebar::new_session]}
set ::vcoach::seen {}
ok "and it is gone from memory" {[::vcoach::times ::vsidebar::new_session] == 0}
::vcoach::init
ok "init reloads it" {[::vcoach::times ::vsidebar::new_session] == 2}

# Past the threshold the number cannot change an answer, so it stops
# being written — a hot key must not mean a write per keypress.
reset_coach
foreach _i {1 2 3 4 5 6 7 8} { ::vcoach::note_key ::vsidebar::new_session }
ok "counting stops at the threshold" \
    {[dict get [::vstate::get coach_seen {}] ::vsidebar::new_session] <= 4}

# ── menus name their own key ────────────────────────────────────────
puts "\ncontext menus carry the accelerator"

frame .ctxprobe
listbox .ctxprobe.l
pack .ctxprobe .ctxprobe.l
update
::vctx::attach .ctxprobe.l {
    {"New session" {::vsidebar::new_session}}
    {-}
    {"Reload"      {::vprobe::never_bound}}
}
::vctx::_rebuild .ctxprobe.l.vctxmenu
ok "the bound entry shows its chord" \
    {[accel .ctxprobe.l.vctxmenu 0] eq "Ctrl-N"}
ok "the unbound entry shows nothing" \
    {[accel .ctxprobe.l.vctxmenu 2] eq ""}
set ::vkeymap::current emacs
::vctx::_rebuild .ctxprobe.l.vctxmenu
ok "and it follows the keymap mode" \
    {[accel .ctxprobe.l.vctxmenu 0] eq "M-n"}
set ::vkeymap::current normie

# ── hover names the key too ─────────────────────────────────────────
puts "\nhover names the key"

::va11y::name .probe.b "delete the selected session"
ok "the description survives"  {[::va11y::describe .probe.b] eq "delete the selected session"}
ok "and the key is appended"   {[::va11y::announce .probe.b] eq "delete the selected session · Ctrl-N"}
ok "an unbound widget just describes" \
    {[::va11y::name .probe.n "reload"
      expr {[::va11y::announce .probe.n] eq "reload"}]}

# ── a phone names no keys ───────────────────────────────────────────
puts "\na phone names no keys"
set _chord [::vkeymap::key_for ::vpalette::open]
namespace eval ::vandroid {
    proc on {} { return 1 }
    proc control_for {command} {
        if {$command eq "::vpalette::open"} { return "⋮" }
        return ""
    }
}
ok "the palette is the ⋮ on the screen" {[::vkeymap::key_for ::vpalette::open] eq "⋮"}
ok "anything else names no chord" {[::vkeymap::key_for ::vsidebar::new_session] eq ""}
namespace delete ::vandroid
ok "…and on a laptop the chords are back" {[::vkeymap::key_for ::vpalette::open] eq $_chord && $_chord ne ""}

puts "\ncoachtest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
