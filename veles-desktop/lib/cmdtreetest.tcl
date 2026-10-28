# veles-desktop/lib/cmdtreetest.tcl — every command behind one button.
#
# USER 2026-10-01: "I just want one button to do that plus literally
# every other command so that it's all reachable from touching
# buttons", and "then which-key tells you what keybinding to use in the
# future if you're on a real keyboard".
#
# What these pin:
#   - "literally every": each palette row, each command any mode's keys
#     run (prefix chords too), each tab and each slash command has a
#     row, so the tree cannot fall behind the lists it reads;
#   - a tap opens a category, Back goes up a level and then closes, and
#     the keyboard walks it too;
#   - a row runs its command with the tree gone and the focus back, and
#     the coach then names the key once, until it is learned;
#   - a keyboard shows each row's key; a phone shows none;
#   - a slash command that needs its argument is written into the
#     composer instead of being run bare.
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

namespace eval ::vt {
    variable T
    array set T {hairline #333 bg_panel #222 fg #eee fg_dim #888 accent #0ff
                 gold #fc0 select_bg #044 bg_input #111 pink #f0f}
}
namespace eval ::vstatus { variable last "" ; proc set_msg {m} { variable last $m } }
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }
namespace eval ::vstate {
    variable blob {}
    proc get {key {default ""}} {
        variable blob
        if {[dict exists $blob $key]} { return [dict get $blob $key] }
        return $default
    }
    proc put {key value} { variable blob ; dict set blob $key $value ; return 1 }
}
# The engine's slash commands, as /api/commands/builtin serves them.
namespace eval ::vapi {
    proc get_json {path args} {
        if {$path eq "/api/commands/builtin"} {
            return {commands {
                {usage "/model <name>" name /model desc "switch the model" cap cmd}
                {usage "/queue [run]" name /queue desc "the queued turns" cap cmd}
                {usage "/agents" name /agents desc "running subagents" cap cmd}
            }}
        }
        error "no gateway in this test"
    }
}
namespace eval ::vtabs {
    variable bar .tabbar
    variable order {chat instances irc dev}
    proc title {id} { return [string totitle $id] }
    proc select_index {n} { lappend ::ran "tab $n" }
    proc select {id} { lappend ::ran "select $id" }
}

# The commands rows run, recorded instead.
namespace eval ::vsidebar {}
namespace eval ::vpanes {}
namespace eval ::vdev {}
namespace eval ::vtitle {}
namespace eval ::vchat {}

source [file join $_dir place.tcl]
source [file join $_dir a11y.tcl]
source [file join $_dir vtext.tcl]
source [file join $_dir bindings.tcl]
source [file join $_dir keymap.tcl]
source [file join $_dir coach.tcl]
source [file join $_dir cmds.tcl]
source [file join $_dir palette.tcl]
source [file join $_dir cmdtree.tcl]

set ::vkeymap::current normie
set ::vcoach::enabled 1
set ::vcoach::threshold 3

# Read a widget property without ever throwing: a test that throws
# aborts its suite, and the flake reads a missing tally as a broken run.
proc probe {script {default ""}} {
    set v $default
    catch { set v [uplevel 1 $script] }
    return $v
}

# The leaf row in category `cat` that runs `action`, or {}.
proc leaf {cat action} {
    set want [::vbindings::_norm $action]
    foreach r [::vcmdtree::rows] {
        if {$cat ne "" && [dict get $r cat] ne $cat} { continue }
        if {[::vbindings::_norm [dict get $r action]] eq $want} { return $r }
    }
    return {}
}

# The index on the screen of the row whose action (or category) matches.
proc row_index {what} {
    set i 0
    foreach r $::vcmdtree::shown {
        if {[dict exists $r to] && [dict get $r to] eq $what} { return $i }
        if {[::vbindings::_norm [dict get $r action]] eq [::vbindings::_norm $what]} { return $i }
        incr i
    }
    return -1
}

# The text of row i as drawn: "<key>\t<label>…".
proc line_of {i} {
    set l [expr {$i + 1}]
    return [probe {.cmdtree.body get $l.0 "$l.0 lineend"}]
}

ttk::frame .mid
ttk::frame .mid.chatwrap
ttk::frame .mid.chatwrap.chat
text .mid.chatwrap.chat.input -height 2
pack .mid .mid.chatwrap .mid.chatwrap.chat .mid.chatwrap.chat.input
ttk::entry .probe
pack .probe
update

# ── literally every command ──────────────────────────────────────────
puts "every command has a row"

set missing {}
foreach a [::vpalette::actions] {
    lassign $a label action cat
    if {[leaf $cat $action] eq {}} { lappend missing "palette: $label ($cat)" }
}
foreach m $missing { puts "      no row: $m" }
ok "every palette row, under its own category" {[llength $missing] == 0}

set missing {}
foreach mode {normie emacs vi} {
    foreach r [::vbindings::table $mode] {
        lassign $r keys target action help category
        if {$category eq "prefix"} {
            foreach p [::vbindings::prefix_continuations $keys] {
                lassign $p k pa ph
                if {$pa ne "" && [leaf "" $pa] eq {}} { lappend missing "$mode $keys $k: $pa" }
            }
            continue
        }
        if {$action eq "" || $category eq "tabs" || $action eq "::vpalette::open"} { continue }
        if {[leaf "" $action] eq {}} { lappend missing "$mode $keys: $action" }
    }
}
foreach m $missing { puts "      no row: $m" }
ok "every command any mode's keys run, prefix chords too" {[llength $missing] == 0}
ok "one only emacs has a key for is there for normie too (C-x s)" \
    {[leaf panes {::vdock::toggle .sidebar}] ne {}}
ok "one only a key reached has a row: delete the session" \
    {[leaf sessions ::vsidebar::delete_selected] ne {}}
ok "the Dev editor's chords: go to definition" \
    {[leaf dev {::vpanes::show dev ; ::vdev::_goto_def}] ne {}}
ok "each tab by name" {
    [dict get [leaf panes {::vtabs::select_index 3}] label] eq "Switch to the Irc tab"
    && [leaf panes {::vtabs::select_index 4}] ne {}
}
set slash {}
foreach r [::vcmdtree::rows] {
    if {[dict get $r cat] eq "slash"} { lappend slash [dict get $r word] }
}
set lost {}
foreach r [::vcmds::all] {
    if {[lindex $r 0] ni $slash} { lappend lost [lindex $r 0] }
}
ok "every slash command, the engine's and the desktop's" \
    {[llength $lost] == 0 && "/model" in $slash && "/keymap" in $slash}
ok "F1's help and F2's every key, under Help & keys" \
    {[leaf help ::vhelp::open] ne {} && [leaf help ::vwhichkey::show_all] ne {}}
set named {}
foreach a [::vpalette::actions] {
    if {[regexp {\((F[0-9]+|C-|M-|Ctrl-|Alt-)} [lindex $a 0]]} { lappend named [lindex $a 0] }
}
ok "no palette label writes in a key (the column does)" {[llength $named] == 0}

# ── the top, down and back up ────────────────────────────────────────
puts "\ndown and back up"

focus -force .probe
update
::vcmdtree::open
update
set top $::vcmdtree::shown
set cats {}
foreach r $top { if {[dict get $r kind] eq "cat"} { lappend cats [dict get $r to] } }
ok "the top: search first, the categories, quit last" {
    [dict get [lindex $top 0] action] eq "::vpalette::open"
    && $cats eq [::vcmdtree::categories]
    && [dict get [lindex $top end] action] eq "exit"
}
set bad {}
foreach r $top {
    if {[dict get $r kind] ne "cat"} { continue }
    if {[dict get $r count] != [llength [::vcmdtree::level_rows [dict get $r to]]]} { lappend bad [dict get $r to] }
}
ok "a category's count is its rows" {[llength $bad] == 0}
ok "the window's close box and the phone's Back go up first" \
    {[wm protocol .cmdtree WM_DELETE_WINDOW] eq "::vcmdtree::back"}
ok "no ‹ at the top" {[winfo manager .cmdtree.head.up] eq ""}

::vcmdtree::run [row_index agent]
update
ok "a tap on a category opens it" {
    $::vcmdtree::level eq "agent" && [winfo manager .cmdtree.head.up] eq "pack"
    && [string match "*› Agent & engine" [.cmdtree.head.title cget -text]]
}
ok "…with its commands" {[row_index {::voverlay::open models}] >= 0}
ok "‹ goes up" {[::vcmdtree::back] eq "up" && $::vcmdtree::level eq ""}
ok "at the top, Back closes" {[::vcmdtree::back] eq "closed" && ![winfo exists .cmdtree]}
# [focus] is empty on a display with no window manager once a toplevel
# goes; what the app hands back is the main window's own focus.
ok "…and the focus is where it was" {[focus -lastfor .] eq ".probe"}

puts "\nby keyboard"
focus -force .probe
update
::vcmdtree::open
update
set t .cmdtree.body
# Key events go to the focus, whatever window they are generated on.
focus -force $t
update
event generate $t <Down>
ok "↓ moves the row" {$::vcmdtree::cur == 1}
event generate $t <Up>
event generate $t <Up>
ok "↑ from the top wraps to the end" {$::vcmdtree::cur == [llength $::vcmdtree::shown] - 1}
event generate $t <Right>
ok "→ on a command runs nothing" {[winfo exists .cmdtree] && $::vcmdtree::level eq ""}
::vcmdtree::move_to [row_index sessions]
event generate $t <Return>
ok "Enter on a category opens it" {$::vcmdtree::level eq "sessions"}
event generate $t <Left>
ok "← goes up" {$::vcmdtree::level eq ""}
event generate $t <BackSpace>
ok "…and at the top does not close" {[winfo exists .cmdtree]}
event generate $t <Escape>
update
ok "Esc closes, the focus back" {![winfo exists .cmdtree] && [focus -lastfor .] eq ".probe"}

# ── running a row, and the key for next time ─────────────────────────
puts "\nrunning a row"

proc ::vsidebar::new_session {} { lappend ::ran new }
set ::ran {}
::vcmdtree::open sessions
update
ok "a keyboard shows the row's key, which-key style" \
    {[string match "Ctrl-N\t*New session" [line_of [row_index ::vsidebar::new_session]]]}
::vcmdtree::run [row_index ::vsidebar::new_session]
update
ok "a row runs its command, the tree gone" {$::ran eq {new} && ![winfo exists .cmdtree]}
ok "…and the coach names its key for next time" \
    {[string match "*New session*Ctrl-N*" [::vcoach::toast_text]]}
catch { destroy .vcoach_toast }
dict set ::vcoach::seen ::vsidebar::new_session 3
::vcmdtree::open sessions
update
::vcmdtree::run [row_index ::vsidebar::new_session]
update
ok "a key already learned is not taught again" {[::vcoach::toast_text] eq ""}

proc ::vpanes::show {args} { lappend ::ran "show $args" }
proc ::vdev::buf_new {} { lappend ::ran buf_new }
set ::ran {}
::vcmdtree::open dev
update
::vcmdtree::run [row_index {::vpanes::show dev ; ::vdev::buf_new}]
update
ok "a command with no key runs, and teaches nothing" \
    {$::ran eq {{show dev} buf_new} && [::vcoach::toast_text] eq ""}
::vcmdtree::open dev
update
ok "a Dev editor chord is named on its row" \
    {[string match "C-c C-p\t*" [line_of [row_index {::vpanes::show dev ; ::vdev::_peek_def}]]]}
::vcmdtree::close

proc ::vtitle::restore_all {} { error "nothing hidden" }
::vcmdtree::open view
update
::vcmdtree::run [row_index ::vtitle::restore_all]
update
ok "a command that fails says so on the status line" {$::vstatus::last eq "nothing hidden"}

# The wrap: a row's continuation lines start under its label.
::vcmdtree::open slash
update
set col [lindex [.cmdtree.body cget -tabs] 0]
ok "a wrapped row carries on under its label" \
    {$col > 0 && [.cmdtree.body tag cget row0 -lmargin2] == $col}

# ── slash commands ───────────────────────────────────────────────────
puts "\nslash commands"

proc ::vchat::slash_command {c} { lappend ::ran "slash $c" }
set ::ran {}
ok "a slash row shows its command in the key column" \
    {[string match "/model\t<name> — switch the model" [line_of [row_index {::vcmdtree::compose {/model }}]]]}
::vcmdtree::run [row_index {::vcmdtree::compose {/model }}]
update
ok "one that needs its argument is written into the composer" {
    [.mid.chatwrap.chat.input get 1.0 end-1c] eq "/model " && [focus -lastfor .] eq ".mid.chatwrap.chat.input"
    && "select chat" in $::ran
}
::vcmdtree::open slash
update
::vcmdtree::run [row_index {::vchat::slash_command "/queue"}]
ok "one whose argument is optional runs" {"slash /queue" in $::ran}
ok "a slash row is the command its chord runs (/agents is C-S-G)" \
    {[::vbindings::keys_for [dict get [leaf slash {::vchat::slash_command "/agents"}] action] normie] eq "C-S-G"}

# ── a phone ──────────────────────────────────────────────────────────
puts "\na phone"

namespace eval ::vandroid {
    proc on {} { return 1 }
    proc dp {n} { return $n }
    proc fit {w width height {frac 0.15}} { wm geometry $w ${width}x$height }
    proc control_for {command} {
        if {$command in {::vcmdtree::open ::vpalette::open}} { return "⋮" }
        return ""
    }
    proc drawer_toggle {} { lappend ::ran drawer }
    proc _tap_arm {w x y} {}
    proc _tap_fire {w x y script} {}
}
::vcmdtree::open sessions
update
ok "no chords on a phone" \
    {[string match "\tNew session" [line_of [row_index ::vsidebar::new_session]]]}
ok "the session list's ☰ has a row" {[row_index ::vandroid::drawer_toggle] >= 0}
ok "no keyboard hint at the bottom" {[winfo manager .cmdtree.hint] eq ""}
::vcmdtree::close
::vcmdtree::open slash
update
ok "a slash command still shows its name" \
    {[string match "/agents\t*" [line_of [row_index {::vchat::slash_command "/agents"}]]]}
# Written into the composer by a tap on a row, not on the composer: the
# keyboard is raised for the argument.
set ::sdltk_calls {}
proc ::sdltk {args} { lappend ::sdltk_calls $args }
::vcmdtree::run [row_index {::vcmdtree::compose {/model }}]
ok "a phone's keyboard comes up for the argument" {{textinput 1} in $::sdltk_calls}
rename ::sdltk {}
namespace delete ::vandroid

puts "\ncmdtreetest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
