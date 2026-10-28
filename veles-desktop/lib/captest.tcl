#!/usr/bin/env wish
# veles-desktop/lib/captest.tcl — headless-ish visual capture harness.
# Builds the synthwave theme + brand banner + a chat panel, prints the
# X window id, stays alive briefly so `import -window $ID` can grab it.
package require Tk
wm title . "veles-captest"
wm geometry . 1080x720

set d [file dirname [info script]]
source [file join $d theme.tcl]
source [file join $d brand.tcl]
::vt::apply_theme
::vbrand::build
pack .brand -side top -fill x

# chat panel with the token colors
ttk::frame .mid.chatwrap.chat
text .mid.chatwrap.chat.log -height 14 -wrap word -background $::vt::T(bg_input) \
    -foreground $::vt::T(fg) -font {TkDefaultFont 11} -bd 0 -highlightthickness 0
.mid.chatwrap.chat.log tag configure userhdr -foreground $::vt::T(cyan) -font {TkDefaultFont 11 bold}
.mid.chatwrap.chat.log tag configure user -foreground $::vt::T(user)
.mid.chatwrap.chat.log tag configure assistant -foreground $::vt::T(assistant)
.mid.chatwrap.chat.log tag configure reasoning -foreground $::vt::T(reasoning) -font {TkDefaultFont 10 italic}
.mid.chatwrap.chat.log tag configure tool -foreground $::vt::T(tool) -font {monospace 10}
.mid.chatwrap.chat.log tag configure status -foreground $::vt::T(fg_dim) -font {TkDefaultFont 9}
.mid.chatwrap.chat.log insert end "you ▸ " userhdr
.mid.chatwrap.chat.log insert end "hello neon world\n" user
.mid.chatwrap.chat.log insert end "veles ▸ " assistant
.mid.chatwrap.chat.log insert end "Rendered **markdown** with `code` and a bullet\n" assistant
.mid.chatwrap.chat.log insert end "⟦reasoning block⟧\n" reasoning
.mid.chatwrap.chat.log insert end "⚙ read_file…\n" tool
.mid.chatwrap.chat.log insert end "· session 123 · 12→34 tok\n" status
.mid.chatwrap.chat.log configure -state disabled
ttk::frame .mid.chatwrap.chat.bar
ttk::button .mid.chatwrap.chat.bar.send -text "Send" -style Accent.TButton
ttk::button .mid.chatwrap.chat.bar.stop
button .mid.chatwrap.chat.bar.detach -text "Detach"
pack .mid.chatwrap.chat.bar.send .mid.chatwrap.chat.bar.stop -side right -padx 2 -pady 2
pack .mid.chatwrap.chat.bar -side bottom -fill x
pack .mid.chatwrap.chat.log -side left -fill both -expand 1
pack .mid.chatwrap.chat -side left -fill both -expand 1 -padx 6 -pady 6

# sidebar stub
ttk::frame .side
listbox .side.list -height 12 -background $::vt::T(bg_panel) \
    -foreground $::vt::T(fg) -selectbackground $::vt::T(select_bg) \
    -selectforeground $::vt::T(fg) -relief flat -bd 0 -highlightthickness 0
.side.list insert end "◆ irc:#velestest · 21"
.side.list insert end "  telegram:59813… · 6"
.side.list insert end "  say hi briefly · 2"
pack .side.list -fill both -expand 1 -padx 4 -pady 4
pack .side -side left -fill y -padx 6 -pady 6

update
puts "WINDOW_ID=[winfo id .]"
after 6000 { exit }
vwait forever