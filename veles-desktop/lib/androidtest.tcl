# veles-desktop/lib/androidtest.tcl — the same tree on a phone.
#
# lib/android.tcl changes three things under AndroWish (the size, the
# window, the first gateway) and must change nothing anywhere else: it
# runs at every desktop boot. Android itself is `borg` and `sdltk`,
# AndroWish's commands, stubbed here the way the emulator answered them
# (1080x2400 at 420 dpi, density 2.625). What the stubs cannot show,
# whether AndroWish draws it right, the emulator run shows
# (scripts/android-emulator-smoke.sh).
#
# Run: scripts/tcl_tests.sh

package require Tk

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

source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
# ::vinst::path resolves the config base through ::vpaths now.
source [file join $_dir paths.tcl]
source [file join $_dir instances.tcl]
source [file join $_dir android.tcl]
source [file join $_dir manage.tcl]
source [file join $_dir place.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
namespace eval ::vstatus {
    variable msgs {}
    proc set_msg {m} { lappend ::vstatus::msgs $m }
}
source [file join $_dir a11y.tcl]
source [file join $_dir vtext.tcl]
source [file join $_dir brand.tcl]
# What the phone's hooks call in modules this suite does not load.
namespace eval ::vi18n {
    proc mc {s args} { return $s }
}
namespace eval ::vchat {
    variable attached {}
    variable reconnects {}
    variable session s1
    variable streams {s1 s2}
    variable disconnected {}
    proc attach {p} { lappend ::vchat::attached $p ; return 1 }
    proc streaming_sessions {} { return $::vchat::streams }
    # As chat.tcl's: closing a stream forgets it, and reconnecting opens
    # it again. A stub that remembered them hid a foreground that asked
    # streaming_sessions AFTER the close, and reopened nothing.
    proc _events_disconnect_all {} {
        lappend ::vchat::disconnected $::vchat::streams
        set ::vchat::streams {}
    }
    proc _events_reconnect {sid} {
        lappend ::vchat::reconnects $sid
        if {$sid ni $::vchat::streams} { lappend ::vchat::streams $sid }
    }
}

set tmp [expr {[info exists ::env(TMPDIR)] && $::env(TMPDIR) ne "" ? $::env(TMPDIR) : "/tmp"}]
set scratch [file join $tmp androidtest-[pid]]
file delete -force $scratch
set ::env(XDG_CONFIG_HOME) $scratch
set registry [file join $scratch veles-desktop instances.json]
set laptop_scaling [tk scaling]

proc read_file {p} {
    set f [open $p r]
    set s [read $f]
    close $f
    return $s
}

# The banner's drawing, measured: whether the sun and the wordmark are
# wholly inside the canvas, and whether anything else but the grid is
# wholly outside it. The grid's outer lines overshoot the edges by
# design (tagged `grid` in brand.tcl) and the canvas clips them; every
# other item belongs on screen.
proc brand_off_canvas {} {
    set W [winfo width .brand]
    set H [winfo height .brand]
    set off 0
    set wm_ok 1
    set wm_seen 0
    foreach id [.brand find all] {
        lassign [.brand bbox $id] x0 y0 x1 y1
        set outside [expr {$x1 < 0 || $y1 < 0 || $x0 > $W || $y0 > $H}]
        if {$outside && "grid" ni [.brand gettags $id]} { incr off }
        if {[.brand type $id] eq "text"
            && [string match "*VELES*" [.brand itemcget $id -text]]} {
            set wm_seen 1
            if {$x0 < 0 || $y0 < 0 || $x1 > $W || $y1 > $H} { set wm_ok 0 }
        }
    }
    list $off $wm_ok $wm_seen
}

# ── on a laptop ───────────────────────────────────────────────────────
puts "on a laptop, nothing changes"
ok "a long press selects nothing here (a mouse has its own drag)" \
    {[::vandroid::hold_bind . .nomenu] == 0}

ok "not Android" {![::vandroid::on]}
ok "scale leaves tk scaling alone" \
    {[::vandroid::scale] == 0 && [tk scaling] == $laptop_scaling}
ok "a desktop pixel is a pixel" {[::vandroid::dp 380] == 380}
button .tipped -text tipped
pack .tipped
::va11y::name .tipped "what the button does"
update
::va11y::_show_tip .tipped 20 20
ok "a hover shows its tip as a card" {[winfo exists .va11y_tip]}
::va11y::_hide_tip
catch { destroy .va11y_tip }
toplevel .dlg
::vplace::centered .dlg 380 200
update
ok "a dialog is the size it asks for" {[string match 380x200+* [wm geometry .dlg]]}
ok "window sets nothing (main.tcl keeps its geometry)" \
    {[::vandroid::window] == 0 && ![wm attributes . -fullscreen]}
ok "no registry is seeded" \
    {[::vandroid::seed_registry] == 0 && ![file exists $registry]}
frame .lbar
foreach b {one two three} { button .lbar.$b -text $b ; pack .lbar.$b -side left }
ok "rows of buttons stay one row" \
    {[::vandroid::flow .lbar] == 0 && [llength [pack slaves .lbar]] == 3}
ok "a dialog is not fitted, the Back key not bound, nothing arranged" {
    [::vandroid::fit .lbar 100 100] eq {} && [bind all <Key-Break>] eq ""
    && [::vandroid::arrange] == 0 && [::vandroid::install] == 0
}
ok "a key is a key: the palette is not ⋮" {[::vandroid::control_for ::vpalette::open] eq ""
    && [::vandroid::control_for ::vcmdtree::open] eq ""}
ok "the tab strip gets no ☰ (its ⋮ is tabs.tcl's, everywhere)" {[::vandroid::tab_strip .lbar] == 0 && ![winfo exists .lbar.drawer]}
ok "📎 is Tk's own dialog" {[::vandroid::pick_files] == 0}
ok "the theme keeps its desktop paddings" \
    {[catch ::vt::apply_theme] == 0 && [ttk::style configure TButton -padding] eq {10 4}}
text .ltext
::vtext::click .ltext link {set ::clicked 1}
ok "a link opens on the press" {[.ltext tag bind link <Button-1>] eq {set ::clicked 1}}
destroy .lbar .ltext

# ── the banner's drawing stays on its canvas ──────────────────────────
# The density variable was named `k`, which the grid loop below it
# reuses as its row counter: the closing `scale all` then multiplied
# the whole drawing by ~3 instead of by the density, and everything but
# the sky gradient and the stars (drawn before the loop's last rows, at
# the top) landed off the bottom edge — on the laptop (scale 3) and on
# the phone (scale 3 for 2.625) alike (2026-10-01).
wm geometry . 600x900
::vbrand::build
pack .brand -side top -fill x
update
lassign [brand_off_canvas] off wm_ok wm_seen
ok "the banner draws inside its canvas on a laptop" \
    {$off == 0 && $wm_ok && $wm_seen}
destroy .brand

# The chat column as chat.tcl builds it: the composer packed into the
# button bar, beside the buttons.
panedwindow .mid -orient horizontal
frame .sidebar -width 700 -height 100
frame .mid.chatwrap
frame .mid.chatwrap.chat
frame .mid.chatwrap.chat.bar
text .mid.chatwrap.chat.input -height 3
foreach b {send stop detach} { button .mid.chatwrap.chat.bar.$b -text $b }
pack .mid.chatwrap.chat.bar.send .mid.chatwrap.chat.bar.stop .mid.chatwrap.chat.bar.detach -side right
pack .mid.chatwrap.chat.input -in .mid.chatwrap.chat.bar -side left -fill both -expand 1
pack .mid.chatwrap.chat.bar -side bottom -fill x
pack .mid.chatwrap.chat -fill both -expand 1
.mid add .sidebar .mid.chatwrap
pack .mid -fill both -expand 1
wm geometry . 600x900
update
ok "the composer stays in the button bar" {
    [::vandroid::layout] == 0
    && [dict get [pack info .mid.chatwrap.chat.input] -in] eq ".mid.chatwrap.chat.bar"
}

# ── on a phone ────────────────────────────────────────────────────────
puts "\non a phone"

set borg_calls {}
# The display in its current rotation; the window can be shorter.
set display {1080 2400}
proc ::borg {args} {
    switch -- [lindex $args 0] {
        displaymetrics {
            lassign $::display w h
            return [list density 2.625 densitydpi 420 width $w height $h xdpi 420.0 ydpi 420.0]
        }
        withdraw - activity - onintent { lappend ::borg_calls $args ; return "" }
        queryfields {
            # AttachNonce's one static field, as the real borg returns it
            # (name value); the suite sets ::attach_nonce_word.
            if {[lindex $args 1] eq "chat.janitor.veles.AttachNonce"} {
                return [list nonce $::attach_nonce_word]
            }
            error "borg stub: $args"
        }
    }
    error "borg stub: $args"
}
set sdltk_calls {}
proc ::sdltk {args} { lappend ::sdltk_calls $args }

ok "AndroWish is recognised" {[::vandroid::on]}

# 1 pt = 96/72 dp: the 8 pt default font is 28 px, not the 56 px
# AndroWish's true-dpi scaling gave it. Tk keeps the scaling as whole
# pixels per millimetre of the screen, so it reads back near 3.5, not
# at it (3.507 on Xvfb).
ok "a desktop pixel is an Android dp" \
    {[::vandroid::scale] == 1 && abs([tk scaling] - 3.5) < 0.05}
ok "…in fonts and in the sizes given in pixels" {[::vandroid::dp 380] == 998}
# A tap is an Enter with no Leave: the card sat over what was tapped.
set ::vstatus::msgs {}
::va11y::_show_tip .tipped 20 20
ok "a touch screen puts the tip in the status bar, not a card" \
    {![winfo exists .va11y_tip] && "what the button does" in $::vstatus::msgs}
# The Add instance dialog's 380 px cut its fields off on the emulator.
# The window here is 600x900; AndroWish frames a dialog with 27 px sides
# and a 78 px title at 420 dpi (sdl2tk's table).
ok "AndroWish's window frame at 420 dpi" {[::vandroid::decor] eq {27 78}}
::vplace::centered .dlg 150 100
update
ok "a dialog is sized in dp" {[string match 394x263+* [wm geometry .dlg]]}
::vplace::centered .dlg 860 520
update
ok "…never wider or taller than the window, its frame included" \
    {[wm geometry .dlg] eq "546x795+0+0"}
# The keyboard takes the bottom of the window: the dialog moves up and
# shrinks to what is left, its buttons still on the screen.
wm geometry . 600x500
update
::vandroid::_settle
update
ok "…and fitted again when the keyboard takes the bottom" \
    {[wm geometry .dlg] eq "546x395+0+0"}
wm geometry . 600x900
update
::vandroid::_settle
destroy .dlg
toplevel .pop
wm overrideredirect .pop 1
ok "a popup without a frame fills to the edges" {[::vandroid::decor .pop] eq {0 0}}
destroy .pop
# A label's -wraplength is desktop pixels like the dialog's size: dp, and
# never wider than the dialog it was fitted to (an approval's heading ran
# off a 320 px screen, 2026-10-01).
toplevel .wl
ttk::label .wl.head -text "terminal: deletes files recursively/forcibly" -wraplength 140
ttk::frame .wl.f
ttk::label .wl.f.wide -text x -wraplength 900
ttk::label .wl.f.free -text x
pack .wl.head .wl.f
pack .wl.f.wide .wl.f.free -in .wl.f
::vplace::centered .wl 400 200
update
set _ww [lindex [split [wm geometry .wl] x+] 0]
ok "a dialog's labels wrap in dp, inside the dialog" {
    [.wl.head cget -wraplength] == [::vandroid::dp 140]
    && [.wl.f.wide cget -wraplength] == $_ww - 2 * [::vandroid::dp 20]
    && [.wl.f.free cget -wraplength] == $_ww - 2 * [::vandroid::dp 20]
}
::vandroid::_settle
update
ok "…and from their own lengths again when it is fitted again" \
    {[.wl.head cget -wraplength] == [::vandroid::dp 140]}
destroy .wl
ok "…forgotten with the dialog" {[array names ::vandroid::wraps .wl.*] eq {}}

::vapi::set_token phone-token
ok "a fresh install seeds the registry" {[::vandroid::seed_registry] == 1}
ok "…with the gateway in the same app, attached" {
    [::vinst::load] == 1
    && [dict get [::vinst::get phone] url] eq [::vapi::base]
    && [::vinst::last_attached] eq "phone"
}
ok "…and no token in the file" {![string match *token* [read_file $registry]]}
ok "…which is the owner's alone" \
    {[file attributes $registry -permissions] eq "00600"}
::vinst::bind_all
ok "the row inherits the token the bootstrap read" \
    {[dict get $::vapi::instances(phone) token] eq "phone-token"}

# A phone set up to use another gateway keeps it.
::vinst::remove phone
::vinst::add laptop http://10.0.2.2:8642 laptop-token
::vinst::set_last_attached laptop
ok "an existing registry is left alone" {
    [::vandroid::seed_registry] == 0
    && [::vinst::load] == 1
    && [::vinst::get phone] eq {}
    && [::vinst::last_attached] eq "laptop"
}

# The emulator's chat column was 482 px, and the composer got 0.
ok "the composer gets its own row, above the buttons" {
    [::vandroid::layout] == 1
    && [dict get [pack info .mid.chatwrap.chat.input] -in] eq ".mid.chatwrap.chat"
    && [lsearch [pack slaves .mid.chatwrap.chat] .mid.chatwrap.chat.input]
       == [lsearch [pack slaves .mid.chatwrap.chat] .mid.chatwrap.chat.bar] + 1
}
update
ok "…as wide as the chat" \
    {[winfo width .mid.chatwrap.chat.input] > [winfo width .mid.chatwrap.chat] - 20}
# Packed beside the buttons it expanded; on its own row that split the
# column with the transcript, and a gap opened under it.
ok "…and takes no height it does not need" \
    {![dict get [pack info .mid.chatwrap.chat.input] -expand]}

# ── the screen's shape: stacked in portrait, the list a drawer ────────
proc hidden {p} { .mid panecget $p -hide }
ok "portrait stacks the regions" {[.mid cget -orient] eq "vertical"}
ok "…with the chat on the screen and the session list away" \
    {[hidden .sidebar] && ![hidden .mid.chatwrap]}
ok "☰ opens the list over the chat" {
    [::vandroid::drawer_toggle] == 1 && ![hidden .sidebar] && [hidden .mid.chatwrap]
}
# The chat takes the height left over, and it is hidden now.
ok "…and the list takes the height" {[.mid panecget .sidebar -stretch] eq "always"}
::vandroid::_picked
ok "picking a session closes it" {
    !$::vandroid::drawer && [hidden .sidebar] && ![hidden .mid.chatwrap]
    && [.mid panecget .sidebar -stretch] eq "never"
}
::vandroid::drawer_toggle
ok "Back closes the list first" {[::vandroid::back] eq "drawer" && !$::vandroid::drawer}
set display {2400 1080}
wm geometry . 900x600
update
::vandroid::_settle
ok "landscape puts them side by side, both on the screen" {
    [.mid cget -orient] eq "horizontal" && ![hidden .sidebar] && ![hidden .mid.chatwrap]
}
ok "…the list at least 200 dp wide" {[.mid panecget .sidebar -minsize] == [::vandroid::dp 200]}
ok "☰ does not hide it there" {[::vandroid::drawer_toggle] == 0 && ![hidden .sidebar]}
set display {1080 2400}
wm geometry . 600x900
update
::vandroid::_settle
ok "…and turned back, portrait again" {[.mid cget -orient] eq "vertical" && [hidden .sidebar]}
# The keyboard takes the window's height, not the display's: a window
# left wider than tall is still a phone held upright.
wm geometry . 900x500
update
::vandroid::_settle
ok "…and the keyboard does not turn it" {[.mid cget -orient] eq "vertical" && [hidden .sidebar]}
wm geometry . 600x900
update
::vandroid::_settle
# Sashes and widths are x positions: a vertical .mid has none worth
# keeping (lib/dock.tcl, lib/tabs.tcl skip them).

# ── the banner: only with height to spare ─────────────────────────────
canvas .brand -height 10
pack .brand -side top -fill x -before .mid
wm geometry . 600x500
update
ok "the banner steps aside for the keyboard" {[::vandroid::banner] == 0 && [winfo manager .brand] eq ""}
wm geometry . 600x1700
update
ok "…and comes back where it was" {
    [::vandroid::banner] == 1
    && [lsearch [pack slaves .] .brand] == [lsearch [pack slaves .] .mid] - 1
}
wm geometry . 600x900
update
destroy .brand

# …and the drawing itself, at the phone's density (2.625): built 152 px
# tall for a 58 dp banner, drawn in dp and scaled up once.
::vbrand::build
pack .brand -side top -fill x
update
lassign [brand_off_canvas] off wm_ok wm_seen
ok "the banner draws inside its canvas on a phone" \
    {$off == 0 && $wm_ok && $wm_seen && [winfo height .brand] == [::vandroid::dp 58]}
destroy .brand

# ── rows that wrap ───────────────────────────────────────────────────
frame .bar -width 300
pack .bar -side top -fill x -before .mid
foreach b {a b c d e} { ttk::button .bar.$b -text "button $b" -width 12 }
ttk::separator .bar.sep -orient vertical
pack .bar.a .bar.b -side left
pack .bar.sep -side left
pack .bar.e .bar.d .bar.c -side right
wm geometry . 300x900
update
::vandroid::flow .bar
update
proc rows {f} { dict get $::vandroid::flows $f rows }
ok "a row wider than the screen wraps" {[llength [rows .bar]] >= 2}
ok "…keeping the order it showed in, separators gone" {
    [join [rows .bar]] eq ".bar.a .bar.b .bar.c .bar.d .bar.e" && [winfo manager .bar.sep] eq ""
}
ok "…every button on the screen" {
    [winfo rootx .bar.e] + [winfo width .bar.e] <= [winfo rootx .] + [winfo width .]
}
ok "flowing it again is harmless" {[catch { ::vandroid::flow .bar }] == 0 && [join [rows .bar]] eq ".bar.a .bar.b .bar.c .bar.d .bar.e"}
destroy .bar
wm geometry . 600x900
update
frame .bar2
pack .bar2 -side top -fill x -before .mid
foreach b {a b c} { ttk::button .bar2.$b -text "button $b" -width 12 }
pack .bar2.c .bar2.b .bar2.a -side right
update
::vandroid::flow .bar2 -from end
update
ok "-from end keeps the last buttons together on the last row" \
    {[lindex [rows .bar2] end] eq {.bar2.b .bar2.c} || [lindex [rows .bar2] end] eq {.bar2.a .bar2.b .bar2.c}}
destroy .bar2
wm geometry . 600x900
update

# ── touch ────────────────────────────────────────────────────────────
ok "two fingers no longer zoom the window; Back is bound" {
    [::vandroid::install] == 1 && {touchtranslate 9} in $::sdltk_calls
    && [bind all <Key-Break>] ne ""
}
# A label a fitted dialog shows after its fit (the setup wizard builds
# each step when it is reached) is capped as it maps.
toplevel .wl2
ttk::label .wl2.head -text x -wraplength 140
pack .wl2.head
::vplace::centered .wl2 400 200
update
set _ww [lindex [split [wm geometry .wl2] x+] 0]
ttk::label .wl2.late -text x -wraplength 900
pack .wl2.late
update
ok "a dialog's label shown after its fit wraps inside it too" \
    {[.wl2.late cget -wraplength] == $_ww - 2 * [::vandroid::dp 20]}
destroy .wl2
text .t -height 5 -width 20
for {set i 0} {$i < 200} {incr i} { .t insert end "line $i\n" }
::vtext::readonly .t
pack .t -side top -before .mid
update
event generate .t <Button-1> -x 5 -y 60
event generate .t <B1-Motion> -x 5 -y 10 -state 256
update
ok "a finger drags read-only text" {[lindex [.t yview] 0] > 0}
ok "…and selects none of it, nor asks for the keyboard" {[.t tag ranges sel] eq ""}
set ::clicked 0
.t tag add link 1.0 1.4
::vtext::click .t link {incr ::clicked}
.t yview moveto 0
update
lassign [.t bbox 1.1] lx ly
set tapx [expr {$lx + 1}]
set tapy [expr {$ly + 1}]
::vandroid::_tap_arm .t $tapx $tapy
::vandroid::_tap_fire .t $tapx $tapy {incr ::clicked}
ok "a tap on a link opens it" {$::clicked == 1}
::vandroid::_tap_arm .t $tapx $tapy
::vandroid::_tap_fire .t $tapx [expr {$tapy + 200}] {incr ::clicked}
ok "…a drag that started on it does not" {$::clicked == 1}
ok "…and on a phone it waits for the release" {
    [.t tag bind link <ButtonRelease-1>] ne "" && [string match *_tap_arm* [.t tag bind link <Button-1>]]
}
destroy .t
# A frame that scrolls (::vmanage::scrollframe: the setup wizard's step):
# a wipe moves it, on a field too; a drag does on its text, and in a
# field the drag is the field's own.
toplevel .sf
wm geometry .sf 300x200
set _in [::vmanage::scrollframe .sf s]
pack .sf.s -fill both -expand 1
ttk::entry $_in.e
pack $_in.e -fill x
for {set i 0} {$i < 80} {incr i} { ttk::label $_in.l$i -text "row $i" ; pack $_in.l$i -anchor w }
::vmanage::scroll_children $_in
update
proc _top {} { lindex [.sf.s.c yview] 0 }
event generate $_in.e <Button-2> -x 5 -y 5 -rootx 100 -rooty 150
event generate $_in.e <B2-Motion> -x 5 -y 5 -rootx 100 -rooty 20 -state 512
update
ok "a wipe on a field moves a frame that scrolls" {[_top] > 0}
.sf.s.c yview moveto 0
update
event generate $_in.l1 <Button-1> -x 5 -y 5 -rootx 100 -rooty 150
event generate $_in.l1 <B1-Motion> -x 5 -y 5 -rootx 100 -rooty 20 -state 256
update
ok "…and a drag on its text" {[_top] > 0}
.sf.s.c yview moveto 0
update
event generate $_in.e <Button-1> -x 5 -y 5 -rootx 100 -rooty 150
# The press focuses the field; its FocusIn comes before the motion.
update
event generate $_in.e <B1-Motion> -x 5 -y 5 -rootx 100 -rooty 20 -state 256
update
ok "…but not a drag in a field" {[_top] == 0}
listbox $_in.list -height 3
for {set i 0} {$i < 46} {incr i} { $_in.list insert end "language $i" }
pack $_in.list -after $_in.e -fill x
::vmanage::scroll_children $_in
update
event generate $_in.list <Button-2> -x 5 -y 30 -rootx 100 -rooty 150
event generate $_in.list <B2-Motion> -x 5 -y 5 -rootx 100 -rooty 125 -state 512
update
ok "…nor a wipe on a list, which scrolls the list" {[_top] == 0 && [lindex [$_in.list yview] 0] > 0}
destroy .sf

# ── Back ─────────────────────────────────────────────────────────────
toplevel .d
set ::closed 0
wm protocol .d WM_DELETE_WINDOW {set ::closed 1 ; destroy .d}
update
ok "Back closes the window on top, the way its close box does" \
    {[::vandroid::back] eq "window" && $::closed == 1}
set ::borg_calls {}
ok "…and with nothing open sends the app to the background" \
    {[::vandroid::back] eq "withdraw" && {withdraw} in $::borg_calls}

# ── selecting text ───────────────────────────────────────────────────
# USER 2026-10-01: "select arbitrary parts of the transcript and copy".
# The touch translation makes a held finger Button-3 and its motion
# B3-Motion (android/androwish/touch.patch); hold and its kin turn that
# into a selection, and the menu opens where the finger lifts.
source [file join $_dir ctxmenu.tcl]
toplevel .sel
text .sel.t -width 30 -height 6 -font {TkFixedFont 14} -wrap word
pack .sel.t
.sel.t insert end "alpha beta gamma\ndelta epsilon zeta\neta theta iota\n"
::vtext::readonly .sel.t
set ::_copied ""
::vctx::attach .sel.t {{"Copy message" {set ::_copied message}}}
set _m .sel.t.vctxmenu
update
proc _at {idx} {
    lassign [.sel.t bbox $idx] x y w h
    list [expr {$x + $w / 2}] [expr {$y + $h / 2}]
}
proc _sel {} {
    if {![llength [.sel.t tag ranges sel]]} { return "" }
    .sel.t get sel.first sel.last
}
ok "a long press on read-only text with a menu selects first" \
    {[string match "::vandroid::hold *" [bind .sel.t <Button-3>]]}
::vandroid::hold .sel.t $_m {*}[_at 1.7] 0 0
ok "…the word under the finger" {[_sel] eq "beta"}
::vandroid::hold_drag .sel.t {*}[_at 2.8]
ok "…and held, the finger carries the far end a word at a time" \
    {[_sel] eq "beta gamma\ndelta epsilon"}
::vandroid::hold_drag .sel.t {*}[_at 1.2]
ok "…before the word it started on, too" {[_sel] eq "alpha beta"}
::vandroid::hold_end .sel.t 10 10
update
ok "lifting it opens the menu, Copy first" {
    [winfo ismapped $_m] && [$_m entrycget 0 -label] eq "Copy"
    && [$_m entrycget 2 -label] eq "Copy message"
}
clipboard clear
$_m invoke 0
ok "…which copies the selection" {[clipboard get] eq "alpha beta"}
ok "Back closes a menu that is up" {[::vandroid::back] eq "menu" && ![winfo ismapped $_m]}
ok "…and the Menu class hands it Back, which it takes from every key" \
    {[bind Menu <Key-Break>] ne ""}
# Its end, held: that end moves, a letter at a time.
lassign [.sel.t bbox 1.9] _x _y _w _h
::vandroid::hold .sel.t $_m [expr {$_x + $_w}] [expr {$_y + $_h / 2}] 0 0
::vandroid::hold_drag .sel.t {*}[_at 2.3]
ok "a long press on a selection's end moves that end, by letters" \
    {[_sel] eq "alpha beta gamma\ndel"}
set ::vandroid::held {}
# A short word: both its ends are in reach, and the finger on its end
# means the end (emulator: holding the end of "first" moved its start).
.sel.t tag remove sel 1.0 end
.sel.t tag add sel 3.0 3.3
lassign [.sel.t bbox 3.2] _x _y _w _h
::vandroid::hold .sel.t $_m [expr {$_x + $_w}] [expr {$_y + $_h / 2}] 0 0
::vandroid::hold_drag .sel.t {*}[_at 3.6]
ok "…the nearer end, when a short word puts both in reach" {[_sel] eq "eta th"}
set ::vandroid::held {}
.sel.t tag remove sel 1.0 end
::vandroid::hold .sel.t $_m {*}[_at 1.5] 0 0
ok "a fingertip between two words takes the word beside it" {[_sel] eq "alpha"}
set ::vandroid::held {}
# A tap clears it; a long press lifts the left button just before it
# presses the right, and keeps it; a drag scrolls, and keeps it.
::vandroid::_drag_mark .sel.t 10
::vandroid::_drag_end .sel.t
::vandroid::hold .sel.t $_m {*}[_at 1.2] 0 0
update
ok "a long press keeps the selection a tap would clear" {[_sel] eq "alpha"}
set ::vandroid::held {}
::vandroid::_drag_mark .sel.t 10
::vandroid::_drag .sel.t 12 1
::vandroid::_drag_end .sel.t
update
ok "…and so does a drag" {[_sel] eq "alpha"}
::vandroid::_drag_mark .sel.t 10
::vandroid::_drag_end .sel.t
update
ok "…and a tap clears it" {[_sel] eq ""}
::vctx::_rebuild $_m
ok "with nothing selected the menu has no Copy" {[$_m entrycget 0 -label] eq "Copy message"}
# Editable text: its own drag selects, and a long press is the menu.
text .sel.e -width 30 -height 2
pack .sel.e
::vctx::attach .sel.e {{"Paste" {set ::_copied paste}}}
.sel.e insert end "typed words"
update
::vandroid::hold .sel.e .sel.e.vctxmenu {*}[list 5 5] 20 20
update
ok "a long press on editable text opens its menu at once, selecting nothing" {
    [winfo ismapped .sel.e.vctxmenu] && $::vandroid::held eq ""
    && ![llength [.sel.e tag ranges sel]]
}
::vandroid::back
destroy .sel

# ── help ─────────────────────────────────────────────────────────────
# USER 2026-10-01: "We need another help screen for android to explain
# all the controls". Help on a phone is the touches; the keys are a
# button away, for a phone with a keyboard.
if {![llength [info commands ::vi18n::label]]} {
    proc ::vi18n::label {w key args} { $w configure -text [::vi18n::mc $key {*}$args] }
}
source [file join $_dir help.tcl]
proc ::vhelp::keys {} { set ::_keys_help 1 }
set _h [::vhelp::open]
update
ok "help on a phone is what a finger does" {
    $_h eq ".help" && [string match "*Touch and hold a word to select it*" [.help.body get 1.0 end]]
    && [string match "*Back closes what is on top*" [.help.body get 1.0 end]]
}
set ::_keys_help 0
.help.btns.keys invoke
ok "…and the keyboard's help is a button on it" {$::_keys_help == 1 && ![winfo exists .help]}

# ── life ─────────────────────────────────────────────────────────────
# Backgrounded, nothing keeps the radio up: the polls, the banner's
# streaks and every open stream stop (2026-10-04: the phone's battery).
namespace eval ::vpanes {
    variable stopped 0
    variable started 0
    proc poll_stop {} { incr ::vpanes::stopped }
    proc poll_start {} { incr ::vpanes::started }
}
namespace eval ::vstatus {
    variable stopped 0
    variable polled 0
    proc poll_stop {} { incr ::vstatus::stopped }
    proc poll {} { incr ::vstatus::polled }
}
# The 30 s layout save: running in the background it kept the phone's
# engine up for ever (2026-10-04).
namespace eval ::vdock {
    variable saves_stopped 0
    variable saves_started 0
    proc layout_save_stop {} { incr ::vdock::saves_stopped }
    proc layout_save_start {} { incr ::vdock::saves_started }
}
# How many streak timers are pending — the variable can name one while
# another runs untracked.
proc shoot_timers {} {
    set n 0
    foreach id [after info] {
        if {[lindex [after info $id] 0] eq "::vbrand::shoot"} { incr n }
    }
    return $n
}
::vbrand::build
update
ok "the banner runs one streak timer" {[shoot_timers] == 1}
::vandroid::background
ok "background stops the polls, the streaks and the streams" {
    $::vpanes::stopped == 1 && $::vstatus::stopped == 1
    && $::vbrand::shoot_after eq "" && [shoot_timers] == 0
    && $::vchat::disconnected eq {{s1 s2}}
}
ok "…and the periodic layout save" {
    $::vdock::saves_stopped == 1 && $::vdock::saves_started == 0
}
ok "…and going quiet reconnects nothing" {$::vchat::reconnects eq ""}
::vandroid::foreground
ok "back on the screen, everything re-arms and the open streams reconnect" {
    $::vpanes::started == 1 && $::vstatus::polled == 1
    && $::vbrand::shoot_after ne "" && [shoot_timers] == 1
    && $::vchat::reconnects eq {s1 s2} && $::vchat::streams eq {s1 s2}
    && $::vdock::saves_started == 1
}
ok "foreground without a background is a no-op" {
    $::vandroid::away_since == 0 && [::vandroid::foreground] == 0
}
ok "background again, the streams stop again" {
    [::vandroid::background] == 1 && $::vchat::disconnected eq {{s1 s2} {s1 s2}}
}

# ── what a phone has instead of keys ─────────────────────────────────
ok "every command is ⋮" {[::vandroid::control_for ::vcmdtree::open] eq "⋮"}
ok "…the palette's search its first row" {[::vandroid::control_for ::vpalette::open] eq "⋮"}
ttk::frame .tabs
foreach t {chat dev} { ttk::button .tabs.t$t -text $t -width 13 ; pack .tabs.t$t -side left }
# ::vtabs::build packs the ⋮ (lib/tabs.tcl), on every platform.
ttk::button .tabs.all -text "⋮" -width 0 -command ::vcmdtree::open
pack .tabs.all -side right
pack .tabs -side top -before .mid
ok "the tab strip gets ☰ first, and keeps ⋮ last" {
    [::vandroid::tab_strip .tabs] == 1
    && [lindex [dict get $::vandroid::flows .tabs kids] 0] eq ".tabs.drawer"
    && [lindex [dict get $::vandroid::flows .tabs kids] end] eq ".tabs.all"
    && [.tabs.tchat cget -width] == 0 && [.tabs.all cget -width] == 0
}
destroy .tabs
set ::borg_calls {}
ok "📎 opens Android's picker" {
    [::vandroid::pick_files] == 1
    && [lindex $::borg_calls 0 end] eq {chat.janitor.veles chat.janitor.veles.AttachActivity}
}
set ::attach_nonce_word "one-time-word-7f3a"
set ::vchat::attached {}
ok "an attach intent without the picker's word attaches nothing" {
    [::vandroid::intent android.intent.action.MAIN {} {} {} [list attach "/c/a.png\n/c/b.txt"]] == 0
    && $::vchat::attached eq {}
}
ok "an attach intent with the wrong word attaches nothing" {
    [::vandroid::intent android.intent.action.MAIN {} {} {} [list attach "/c/a.png" attach_nonce "forged"]] == 0
    && $::vchat::attached eq {}
}
ok "an attach intent naming the picker's word attaches" {
    [::vandroid::intent android.intent.action.MAIN {} {} {} [list attach "/c/a.png\n/c/b.txt" attach_nonce "one-time-word-7f3a"]] == 2
    && $::vchat::attached eq {/c/a.png /c/b.txt}
}
ok "the same word again attaches nothing (it is spent)" {
    [::vandroid::intent android.intent.action.MAIN {} {} {} [list attach "/c/a.png" attach_nonce "one-time-word-7f3a"]] == 0
    && $::vchat::attached eq {/c/a.png /c/b.txt}
}
set ::attach_nonce_word ""
ok "no live picker, no attach (a cleared word matches nothing)" {
    [::vandroid::intent android.intent.action.MAIN {} {} {} [list attach "/c/a.png" attach_nonce ""]] == 0
    && $::vchat::attached eq {/c/a.png /c/b.txt}
}
# A pinned app's home-screen shortcut (PinActivity.java) opens the
# desktop with veles-app://<name>: that app runs, a moment later.
ok "a pinned app's shortcut runs that app" {
    [::vandroid::intent android.intent.action.VIEW veles-app://egg-counter {} {} {}] == 1
    && [lsearch -glob [lmap a [after info] {lindex [after info $a] 0}] "*open_from_shortcut egg-counter*"] >= 0
}
ok "a shortcut that names no app is not one" {
    [::vandroid::intent android.intent.action.VIEW {veles-app://../x} {} {} {}] == 0
    && [::vandroid::intent android.intent.action.VIEW {veles-app://Big} {} {} {}] == 0
}
foreach _a [after info] {
    if {[string match "*open_from_shortcut*" [lindex [after info $_a] 0]]} { after cancel $_a }
}
# The stub above answers the way `borg queryfields` does, keyed by the
# Java FIELD's name; the desktop reads `nonce`. A field renamed on one
# side only refused every attach on the phone (968c0be read `nonce` from
# a field named `current`), and no stub can see that — the source can.
set _java [file join $_dir .. .. android java chat janitor veles AttachNonce.java]
if {[file exists $_java]} {
    set _fh [open $_java r]
    set _src [read $_fh]
    close $_fh
    ok "AttachNonce.java declares the field the desktop reads (nonce)" \
        {[regexp {public static String nonce\M} $_src]}
} else {
    puts "      (AttachNonce.java is not in this tree; the field check runs in the repo)"
}
set ::vchat::attached {}
ok "the launcher's own intent attaches nothing" \
    {[::vandroid::intent android.intent.action.MAIN {} {} {} {}] == 0}
proc ::vchat::open_session {sid} { lappend ::vchat::opened $sid }
set ::vchat::opened {}
::vandroid::drawer_toggle
ok "a notification opens its session, the list closed" {
    [::vandroid::intent android.intent.action.MAIN {} {} {} {session s9}] == 1
    && [::vandroid::show_session s9] == 1 && $::vchat::opened eq "s9"
    && !$::vandroid::drawer
}

# ── sizes for a finger ───────────────────────────────────────────────
::vt::apply_theme
ok "buttons are padded in dp" \
    {[ttk::style configure TButton -padding] eq [list [::vandroid::dp 10] [::vandroid::dp 7]]}
ok "the tree's triangle is 14 dp" \
    {[ttk::style configure Item -indicatorsize] == [::vandroid::dp 14]}
ok "a radio or check button's text wraps inside the screen" {
    [ttk::style configure TRadiobutton -wraplength] > 0
    && [ttk::style configure TCheckbutton -wraplength]
        <= min([winfo screenwidth .], [winfo screenheight .]) - [::vandroid::dp 100]
}

# What is asked of the window manager, not its answer: on X11 a mapped
# window reads back fullscreen only once a window manager confirms it,
# and this Xvfb has none. AndroWish applies it at once (emulator:
# -fullscreen 1, the window 1080x2400).
# install wrapped `wm`: a dialog given a size in pixels is fitted.
toplevel .sized
wm geometry .sized 150x100
update
ok "a dialog that sizes itself in pixels is fitted in dp" \
    {[dict exists $::vandroid::fitted .sized] && [string match 394x263+* [wm geometry .sized]]}
destroy .sized
toplevel .natural
label .natural.l -text "a dialog nobody placed" -width 100
pack .natural.l
update
update
ok "…and one nobody placed, when it shows" {
    [dict exists $::vandroid::fitted .natural]
    && [winfo width .natural] <= [winfo width .] - 54
}
destroy .natural
# Tk's own dialogs have a class of their own: tk_messageBox is a Dialog,
# whose bindtags have no Toplevel (it ran off the side of the screen).
# Its message is gridded beside its icon, as there, and wraps in the room
# right of it: wrapped at the dialog's width, its lines ran off the edge.
proc _right_edge {k} {
    expr {[winfo rootx $k] - [winfo rootx [winfo toplevel $k]] + [$k cget -wraplength]}
}
toplevel .classed -class Dialog
frame .classed.icon -width [::vandroid::dp 48] -height 20
label .classed.l -text "Close the setup wizard? Nothing has been written yet." -wraplength 5i
grid .classed.icon .classed.l -padx 2m
update
update
ok "…and one of Tk's own, with a class of its own" {
    [dict exists $::vandroid::fitted .classed]
    && [winfo width .classed] <= [winfo width .] - 54
    && [_right_edge .classed.l] <= [winfo width .classed]
}
destroy .classed
# The same beside an icon in a dialog placed before it shows (approvals).
toplevel .besides
ttk::frame .besides.icon -width [::vandroid::dp 48] -height 20
ttk::label .besides.l -text "terminal: deletes files recursively/forcibly" -wraplength 900
grid .besides.icon .besides.l
::vplace::centered .besides 400 200
update
ok "…and beside an icon in one placed before it shows" {
    [_right_edge .besides.l] <= [winfo width .besides]
}
destroy .besides
# …and beside a scrollbar: the wizard's step scrolls, and its lines ran
# under the bar (320 px emulator).
toplevel .sfw
set _in [::vmanage::scrollframe .sfw s]
pack .sfw.s -fill both -expand 1 -padx 60
ttk::label $_in.l -text [string repeat "word " 80] -wraplength 900
pack $_in.l -anchor w
::vplace::centered .sfw 400 200
update
ok "…and clear of a scrollbar beside it" {
    [_right_edge $_in.l] <= [winfo rootx .sfw.s.sb] - [winfo rootx .sfw]
}
destroy .sfw
rename ::wm ::_real_wm
set wm_calls {}
proc ::wm {args} { lappend ::wm_calls $args ; ::_real_wm {*}$args }
ok "the window is the screen" {
    [::vandroid::window] == 1
    && [lsearch -exact $wm_calls {attributes . -fullscreen 1}] >= 0
}
rename ::wm {}
rename ::_real_wm ::wm

tk scaling $laptop_scaling
file delete -force $scratch

puts "── the APK carries every package the desktop requires"
# The APK's assets are AndroWish's Tcl and Tk libraries plus tcllib's
# json (flake.nix, the android payload). Anything else a boot-time file
# `package require`s ends the desktop with "can't find package" before a
# window maps: install.tcl's `package require uuid` did that to v0.1.0
# on every phone, and no suite saw it, because they run with the
# laptop's whole tcllib. An optional package is required inside a catch,
# which this does not match.
set _shipped {Tcl Tk http msgcat json}
set _missing {}
foreach f [concat [list [file join $_dir .. main.tcl]] [glob [file join $_dir *.tcl]]] {
    if {[string match *test.tcl $f]} { continue }
    set fh [open $f r]
    set n 0
    foreach line [split [read $fh] \n] {
        incr n
        if {[regexp {^\s*\[?package require (?:-exact )?([A-Za-z0-9_:]+)} $line -> pkg]
            && $pkg ni $_shipped} {
            lappend _missing "[file tail $f]:$n $pkg"
        }
    }
    close $fh
}
ok "every package a boot-time file requires is one the APK ships" {[llength $_missing] == 0}
if {[llength $_missing]} { puts "      not in the APK: $_missing" }

puts "\nandroidtest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
