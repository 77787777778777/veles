# veles-desktop/lib/tabtest.tcl — a tab is a saved LAYOUT.
#
# emacs `tab-bar-mode`, not browser tabs. A browser tab is one document
# filling the window; an emacs tab is a whole window configuration. The
# difference is the whole feature: picking the browser model would have
# cost watching the transcript while working in a pane beside it, which
# is what the desktop is built around.
#
# What these pin:
#
#   - switching NEVER destroys a widget. .mid forget + ::vdock::dock is
#     the entire mechanism, so a tab you left an hour ago comes back
#     exactly as it was. (::vdock::undock has to rebuild, because Tk
#     cannot reparent across toplevels — a tab switch never crosses one.)
#   - each tab owns its own sashes, its own focus, its own docked set
#     and its own per-region state, so the same .panes widget is Files
#     in Chat and the editor in Dev.
#   - listing a region does not force it open. The right-hand pane has
#     always been revealed on demand and still is.
#   - the focus ring is scoped to the active tab: C-x o is
#     `other-window`, and the other windows are the ones in front of you.
#
# ::vpanes::apply_state and capture_state here are the REAL ones, out of
# panes.tcl. Only `show` is stubbed (it hits the API). A suite that
# restates the code it is testing proves nothing — rendertest stubbed a
# key lookup to return a hardcoded "M-x" and could never have caught the
# hints drifting from the bindings.
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
    array set T {hairline #333 bg_panel #222 fg #eee bg_hover #444 accent #0ff
                 bg_input #111 fg_dim #888 select_bg #044 green #0f0 red #f00}
}
namespace eval ::vapi   { proc get_json {args} { return {} } ; proc get {args} { return "" }
                          proc post {args} { return "" } }
namespace eval ::vstatus { variable last "" ; proc set_msg {m} { variable last $m } }
namespace eval ::vi18n { proc mc {s args} { if {[llength $args]} { return [format $s {*}$args] } ; return $s } }
namespace eval ::vi18n { proc label {w key args} { if {![winfo exists $w]} { return 0 } ; catch { $w configure -text [::vi18n::mc $key {*}$args] } ; return 1 }
                    proc aname {w key args} { if {![winfo exists $w]} { return 0 } ; catch { ::va11y::name $w [::vi18n::mc $key {*}$args] } ; return 1 } }
namespace eval ::va11y  { proc name {w d} {} ; proc describe {w} { return "" } ; proc tab_order {l} {} }
namespace eval ::vcoach { variable attached {} ; proc attach {w} { variable attached ; lappend attached $w } }
namespace eval ::vctx   { proc attach {w i} {} }
namespace eval ::vstate {
    variable blob {}
    proc all {} { variable blob ; return $blob }
    proc get {k {d ""}} { variable blob
        if {[dict exists $blob $k]} { return [dict get $blob $k] } ; return $d }
    proc merge {u} { variable blob ; dict for {k v} $u { dict set blob $k $v } ; return 1 }
    proc put {k v} { return [merge [dict create $k $v]] }
}
namespace eval ::vsidebar { variable R .sidebar }

# The REAL binding registry, so the tab strip's key labels are asserted
# against the table that actually installs them rather than a fixture.
source [file join $_dir safeexec.tcl]
source [file join $_dir bindings.tcl]
namespace eval ::vkeymap { variable current emacs }
proc ::vkeymap::key_for {command} {
    return [::vbindings::keys_for $command $::vkeymap::current]
}

source [file join $_dir dock.tcl]
source [file join $_dir panes.tcl]
source [file join $_dir tabs.tcl]

# Keep the real ::vpanes::show reachable: the stub below replaces it for
# the layout tests, but the guard that keeps Alt-N inside the current tab
# lives in the real one and has to be exercised there.
rename ::vpanes::show ::vpanes::_real_show

# The one stub: the real `show` refreshes over the API and highlights
# tab buttons that do not exist here. Everything it does that MATTERS
# to a tab — which pane is current, and packing it — is kept.
proc ::vpanes::show {tab} {
    variable current $tab
    variable visible 1
    foreach t {files review preview terminal dev} { catch { pack forget .panes.$t } }
    catch { pack .panes.$tab -fill both -expand 1 }
}
proc ::vpanes::hide {} {
    variable visible 0
    catch { .mid forget .panes }
}

# ── a layout to switch around ───────────────────────────────────────
frame .sidebar
listbox .sidebar.list
pack .sidebar.list -fill both -expand 1

::vdock::build
frame .mid.chatwrap
frame .mid.chatwrap.chat
text .mid.chatwrap.chat.log
text .mid.chatwrap.chat.input
pack .mid.chatwrap.chat -fill both -expand 1
pack .mid.chatwrap.chat.log .mid.chatwrap.chat.input -fill both -expand 1

frame .panes
foreach t {files review preview terminal dev} { frame .panes.$t }
# The paths ::vpanes::content_widget actually looks for. Building a
# flat .panes.$t instead made it fall through to _descendant_content
# and hand back a pane that was not on screen.
text  .panes.files.tree
text  .panes.review.files
text  .panes.review.diff
text  .panes.preview.body
text  .panes.terminal.out
frame .panes.dev.edit
text  .panes.dev.edit.body
foreach w {.panes.files.tree .panes.review.files .panes.preview.body
           .panes.terminal.out .panes.dev.edit} {
    pack $w -fill both -expand 1
}
pack .panes.dev.edit.body -fill both -expand 1
pack .panes.files -fill both -expand 1

::vdock::dock .sidebar
::vdock::dock .mid.chatwrap
wm geometry . 1000x600
update

proc docked {w} { return [expr {[lsearch -exact [.mid panes] $w] >= 0}] }

# ── boot ────────────────────────────────────────────────────────────
puts "the default tabs"

::vtabs::boot
update
ok "there are tabs"              {[llength [::vtabs::ids]] >= 2}
ok "chat is first"               {[lindex [::vtabs::ids] 0] eq "chat"}
ok "chat is what you land on"    {[::vtabs::current] eq "chat"}
ok "the sidebar is docked"       {[docked .sidebar]}
ok "the transcript is docked"    {[docked .mid.chatwrap]}

# Listing a region in a tab is not the same as forcing it open. The
# right-hand pane has always been revealed on demand, and a tab strip
# is no reason to change what the app looks like on first start.
ok "…and the right pane is NOT forced open" {![docked .panes]}

# ── switching keeps everything ──────────────────────────────────────
puts "\nswitching is not rebuilding"

.mid.chatwrap.chat.log insert end "a line the user was reading"
::vtabs::select dev
update
ok "dev is a full-window layout"   {[.mid panes] eq {.panes}}
ok "dev shows the editor"          {$::vpanes::current eq "dev"}
ok "the transcript widget survives" {[winfo exists .mid.chatwrap.chat.log]}
::vtabs::select chat
update
ok "and comes back with its content" \
    {[string match "*a line the user was reading*" [.mid.chatwrap.chat.log get 1.0 end]]}

# ── each tab owns its pane ──────────────────────────────────────────
puts "\nthe same widget, different tabs"

::vtabs::select chat
::vpanes::show review
::vdock::dock .panes
update
ok "chat's right pane is Review"  {$::vpanes::current eq "review"}
::vtabs::select dev
update
ok "dev's is the editor"          {$::vpanes::current eq "dev"}
::vtabs::select chat
update
ok "chat still remembers Review"  {$::vpanes::current eq "review"}
ok "…and that it was open"        {[docked .panes]}
ok "one .panes widget, not two"   {[llength [info commands .panes]] == 1}

# ── sashes are per tab ──────────────────────────────────────────────
puts "\nsashes are per tab"

::vtabs::select chat
update
.mid sash place 0 210 1
update
set chat_sash [lindex [.mid sash coord 0] 0]
::vtabs::select dev
update
::vtabs::select chat
update
ok "a sash survives a round trip" {[lindex [.mid sash coord 0] 0] == $::chat_sash}

# ── the focus ring is scoped ────────────────────────────────────────
puts "\nC-x o walks the tab you are in"

::vtabs::select chat
update
set chat_ring [::vdock::buffer_ring]
::vtabs::select dev
update
set dev_ring [::vdock::buffer_ring]

ok "chat's ring has the transcript" \
    {[lsearch -exact $::chat_ring .mid.chatwrap.chat.log] >= 0}
ok "chat's ring has the sessions list" \
    {[lsearch -exact $::chat_ring .sidebar.list] >= 0}
ok "dev's ring does NOT reach into chat" \
    {[lsearch -exact $::dev_ring .mid.chatwrap.chat.log] < 0}
# The bug this caught: the geometry pass lived inside _restore_sashes,
# which returns early for a tab with no remembered sashes — so the
# FIRST visit to a tab left its regions unmapped, buffer_ring (which
# filters on ismapped) came back empty, and C-x o did nothing at all.
ok "dev's ring is not empty"      {[llength $::dev_ring] > 0}
ok "…and it is dev's own buffer"  {[string match ".panes.*" [lindex $::dev_ring 0]]}

# The case that actually needs the scoping.
#
# For a region simply not docked, `ismapped` already keeps it out of
# the ring — so a global ring and a per-tab ring agree, and the
# difference only shows for a region TORN OFF into its own window: it
# stays mapped, so a global ring would cycle into it from any tab.
#
# It should not, and the reason is emacs': `other-window` does not
# cross frames. A floated sidebar is the Chat tab's sidebar showing
# Chat's session list, and reaching it from the Dev tab would move
# focus into a layout you are not in.
# Tearing off a region REBUILDS it at the float toplevel — Tk cannot
# reparent across toplevels — so $::vsidebar::R moves with it, which is
# why the buffers registry holds scripts rather than fixed paths.
::vtabs::select chat
update
dict set ::vdock::floats .sidebar .float_sidebar
catch { .mid forget .sidebar }
toplevel .float_sidebar
frame .float_sidebar.body
listbox .float_sidebar.body.list
pack .float_sidebar.body -fill both -expand 1
pack .float_sidebar.body.list -fill both -expand 1
set ::vsidebar::R .float_sidebar.body
update
set floated .float_sidebar.body.list
ok "a floated region is still mapped" {[winfo ismapped $::floated]}
::vtabs::select dev
update
ok "…but dev does not cycle into it" \
    {[lsearch -exact [::vdock::buffer_ring] $::floated] < 0}
::vtabs::select chat
update
ok "…while chat, which owns it, does" \
    {[lsearch -exact [::vdock::buffer_ring] $::floated] >= 0}
set ::vdock::floats [dict remove $::vdock::floats .sidebar]
catch { destroy .float_sidebar }
set ::vsidebar::R .sidebar
::vdock::dock .sidebar never
update

# ── index and step ──────────────────────────────────────────────────
puts "\nselecting by number and by step"

ok "C-1 selects the first tab"  {[::vtabs::select_index 1] && [::vtabs::current] eq "chat"}
ok "C-2 selects the second"     {[::vtabs::select_index 2] && [::vtabs::current] eq "dev"}
# Pressing C-7 with two tabs open should do nothing, not throw.
ok "an out-of-range index is a no-op" {[::vtabs::select_index 7] == 0}
ok "…and leaves the tab alone"        {[::vtabs::current] eq "dev"}
::vtabs::next
ok "next wraps"      {[::vtabs::current] eq "chat"}
::vtabs::previous
ok "previous wraps"  {[::vtabs::current] eq "dev"}
ok "an unknown tab is refused" {[::vtabs::select nope] == 0}

# ── a floated region stays floated ──────────────────────────────────
#
# Tearing a region off is a deliberate act. A tab switch that mentions
# that region must not yank it back into the window.
puts "\na torn-off region is left alone"

::vtabs::select chat
update
dict set ::vdock::floats .sidebar .float_sidebar
catch { .mid forget .sidebar }
::vtabs::select dev
update
::vtabs::select chat
update
ok "the floated region was not re-docked" {![docked .sidebar]}
set ::vdock::floats [dict remove $::vdock::floats .sidebar]
::vtabs::select dev
::vtabs::select chat
::vdock::dock .sidebar never
update

# ── nothing is left dead ────────────────────────────────────────────
#
# .mid.chatwrap is the only region the old dock code would stretch. A
# tab without it — Dev — would have had every region -stretch never,
# leaving one 140px column and the rest of the window blank.
puts "\nno tab leaves dead space"

# Assert the POLICY, not just the pixels: a lone pane fills the window
# whatever its -stretch is, so measuring width alone cannot tell the
# difference and the check was vacuous — removing the policy entirely
# left it green.
::vtabs::select dev
update
ok "dev's only region is the stretcher" \
    {[.mid panecget .panes -stretch] eq "always"}

::vtabs::select chat
::vdock::dock .panes never
update
ok "chat stretches the transcript" \
    {[.mid panecget .mid.chatwrap -stretch] eq "always"}
ok "…and not the sidebar" \
    {[.mid panecget .sidebar -stretch] eq "never"}
ok "…nor the right pane"  \
    {[.mid panecget .panes -stretch] eq "never"}

# And the pixels follow: widen the window and the stretcher takes it.
set before_chat [winfo width .mid.chatwrap]
set before_side [winfo width .sidebar]
wm geometry . 1300x600
update
ok "widening grows the stretcher"  {[winfo width .mid.chatwrap] > $::before_chat}
ok "…and leaves the others alone"  {[winfo width .sidebar] == $::before_side}
wm geometry . 1000x600
update

# ── persistence ─────────────────────────────────────────────────────
puts "\nlayouts survive a restart"

::vtabs::select chat
update
# Ask for 260 but remember what Tk actually SETTLED on. A panedwindow
# clamps a sash against the minsize of the panes either side, so the
# number that comes back is not always the number you asked for — and
# the invariant being tested is "the layout you left comes back", not
# "Tk stores integers".
.mid sash place 0 260 1
update
set want_sash [lindex [.mid sash coord 0] 0]
::vtabs::select dev
update
ok "the active tab was written"  {[::vstate::get tab_current] eq "dev"}
set payload [::vstate::get tab_layout {}]
ok "…and so was each tab"        {[dict exists $payload chat] && [dict exists $payload dev]}
ok "chat's sashes are in there"  {[dict exists [dict get $payload chat] sashes]}
ok "so is its docked set"        {[dict exists [dict get $payload chat] docked]}

# Reload from the blob the way a fresh start would.
array unset ::vtabs::saved
array set ::vtabs::saved {}
array unset ::vtabs::spec
array set ::vtabs::spec {}
set ::vtabs::order {}
set ::vtabs::current ""
set want [::vtabs::boot]
update
ok "boot returns to the tab you left" {$::want eq "dev"}
ok "and the pane it was showing"      {$::vpanes::current eq "dev"}
::vtabs::select chat
update
ok "the other tab's sash came back"   {[lindex [.mid sash coord 0] 0] == $::want_sash}
ok "…and its regions did too"         {[docked .sidebar] && [docked .mid.chatwrap]}

# A payload for a tab that no longer exists is dropped, not resurrected.
::vstate::put tab_layout [dict create ghost {sashes 100} chat {sashes 180}]
array unset ::vtabs::saved ; array set ::vtabs::saved {}
::vtabs::load
ok "a stale tab in the blob is ignored" {![info exists ::vtabs::saved(ghost)]}

# ── the strip ───────────────────────────────────────────────────────
puts "\nthe tab strip"

ok "the strip exists"          {[winfo exists .tabbar]}
ok "one button per tab, and ⋮" {[llength [lsearch -all -glob [winfo children .tabbar] .tabbar.t*]] == [llength [::vtabs::ids]]
    && [llength [winfo children .tabbar]] == [llength [::vtabs::ids]] + 1}
ok "the strip sits above .mid" {[winfo exists .tabbar.tchat]}
# Every tab has a key, and the strip is exactly where someone who does
# not know that will click.
ok "the strip is coached"      {[llength $::vcoach::attached] >= 2}

# ── Alt-N stays inside the tab you are in ───────────────────────────
#
# USER (spot test, 2026-08-25): "pressing M-\d combinations opens the
# corresponding side pane even in irc mode".
#
# Alt-1..5 run ::vpanes::show, and ::vpanes::show docks .panes whenever
# it is hidden. So a chord documented as "pane within the current tab"
# reached into ANOTHER tab's layout and forced a region into this one.
puts "\nAlt-N respects the tab"

# A tab shaped like IRC's: three regions, none of them .panes. (The real
# irc tab is only defined when ::virc::boot exists, which it does not
# here — and `dev` is no good for this, its one region IS .panes.)
foreach r {.ta .tb .tc} {
    frame $r
    text $r.body
    pack $r.body -fill both -expand 1
    # The second argument is a SCRIPT evaluated at global scope, not a
    # literal list — that is how the real regions register.
    ::vdock::register $r [list list $r.body]
}
::vtabs::define three "Three" {.ta .tb .tc}
update idletasks

# The real ::vpanes::show highlights the pane strip's buttons, which the
# stub layout above never built. Build them, so the chat-tab assertion
# exercises the whole proc rather than dying halfway.
frame .panes.tabs
foreach t {files review preview terminal dev} {
    ttk::button .panes.tabs.$t -text $t
}

::vtabs::select chat
ok "the chat layout has a right-hand pane" {[::vtabs::has_region .panes]}
::vtabs::select three
ok "a three-region layout does not" {![::vtabs::has_region .panes]}

# The real proc, not the stub — the guard is what is under test.
#
# CAUGHT, deliberately. Without the guard this falls through into the
# lazy-fill, which wants a real treeview the fixture has no reason to
# build — and a suite that dies prints NO TALLY, which precommit.sh
# scores as a broken run rather than a red one. Catching it means
# removing the guard produces a ✗ that can be counted.
proc try_show {tab} {
    catch { ::vpanes::_real_show $tab } r
    return $r
}
try_show files
ok "Alt-1 there does NOT dock .panes" {[lsearch -exact [.mid panes] .panes] < 0}
ok "…and does not leave it floating either" {![::vdock::floating .panes]}

# It focuses that tab's Nth region instead, which is what "pane within
# the current tab" has to mean where there is no right-hand pane.
ok "focus_nth 1 lands in the first region"  {[::vtabs::focus_nth 1] eq ".ta.body"}
ok "focus_nth 3 lands in the third"         {[::vtabs::focus_nth 3] eq ".tc.body"}
ok "focus_nth past the end does nothing"    {[::vtabs::focus_nth 9] eq ""}
ok "focus_nth 0 does nothing"               {[::vtabs::focus_nth 0] eq ""}
# Alt-1 IS focus_nth 1 here — the guard routes it there by ordinal.
ok "Alt-1 and focus_nth 1 agree"            {[try_show files] eq ".ta.body"}
ok "Alt-3 reaches the third region"         {[try_show preview] eq ".tc.body"}

# And in a tab that DOES have .panes, nothing changed: the guard lets it
# through and .panes is docked. (The lazy-fill past that point wants a
# real treeview, which this fixture has no reason to build.)
::vtabs::select chat
catch { ::vpanes::_real_show files }
ok "Alt-1 in the chat tab still reaches the pane" {$::vpanes::current eq "files"}
ok "…and docks .panes, as it always did" \
    {[lsearch -exact [.mid panes] .panes] >= 0}

::vtabs::select chat

# ── bottom strips ───────────────────────────────────────────────────
#
# The Instances controls strip (`.instctl`) is the reason this exists:
# defined as an ordinary region it docked as a fourth panedwindow
# column, its separators ran the full window height, and its buttons
# floated mid-cavity in dead space (USER, 2026-08-28: "Why the vertical
# bars/spacers? Is something supposed to be in those panes?"). A strip
# region docks PACKED UNDER .mid, full width — never a column.
puts "\nbottom strips dock under .mid"

# A real stub rebuilder (undock REBUILDS into the float, and its proc
# must exist or undock refuses): same shape as ::vsidebar::rebuild.
proc ::vdock::_stub_rebuild {root} {
    variable _stub_R
    set _stub_R $root
    if {[winfo exists $root]} { destroy $root }
    ttk::frame $root
    ttk::button $root.b1 -text Attach
    ttk::button $root.b2 -text Remove
    pack $root.b1 $root.b2 -side left
    set parent [winfo parent $root]
    if {[winfo toplevel $parent] eq $parent} { pack $root -fill both -expand 1 }
}
proc ::vdock::_pi_side {w} {
    # "side" of the strip's packing, or "" when not managed. Kept out of
    # the ok-expressions: pack info THROWS on an unmanaged window, and
    # a throw inside `ok`'s expr kills the whole suite.
    if {![winfo exists $w]} { return "" }
    if {[catch { pack info $w } pi] || $pi eq ""} { return "" }
    return [lindex $pi [expr {[lsearch -exact $pi -side] + 1}]]
}

frame .ctlbar
ttk::button .ctlbar.b1 -text Attach
ttk::button .ctlbar.b2 -text Remove
pack .ctlbar.b1 .ctlbar.b2 -side left
# The real regions register exactly this way (instpane::register_regions).
::vdock::register .ctlbar {list} ::vdock::_stub_rebuild
::vdock::register_strip .ctlbar
::vdock::register .il  {list} ""
::vdock::register .id  {list} ""
frame .il ; text .il.body ; pack .il.body -fill both -expand 1
frame .id ; text .id.body ; pack .id.body -fill both -expand 1
::vtabs::define inst "Inst" {.il .id .ctlbar}
update idletasks

::vtabs::select inst
update
ok "the strip is NOT a panedwindow column" {[lsearch -exact [.mid panes] .ctlbar] < 0}
ok "…it is packed below .mid" {[::vdock::_pi_side .ctlbar] eq "bottom"}
ok "…full width"   {[winfo width .ctlbar] >= [winfo width .mid] - 2}
ok "…the columns still dock beside it" {
    [lsearch -exact [.mid panes] .il] >= 0 && [lsearch -exact [.mid panes] .id] >= 0
}
ok "…the detail column is the stretcher, the strip never" {
    [.mid panecget .il -stretch] eq "never" && [.mid panecget .id -stretch] eq "always"
}
ok "…the strip's height is its natural one" {
    [winfo reqheight .ctlbar] > 0 && [winfo height .ctlbar] <= [winfo reqheight .ctlbar] + 20
}
# vdock::docked must agree, or _capture_docked would drop the strip
# from the remembered set and a reload would come back stripless.
ok "::vdock::docked reports the strip docked" {[::vdock::docked .ctlbar]}

# Leaving the tab must take the strip with it — it is not a .mid pane,
# so the forget-everything loop over [.mid panes] never sees it.
::vtabs::select dev
update
ok "leaving the tab removes the strip" {
    [lsearch -exact [.mid panes] .ctlbar] < 0 && [::vdock::_pi_side .ctlbar] eq ""
}
::vtabs::select inst
update
ok "…and coming back restores it below .mid" {[::vdock::_pi_side .ctlbar] eq "bottom"}

# A torn-off strip is a float like any other region.
::vdock::undock .ctlbar
update
ok "a strip undocks into a float" {
    [winfo exists .float_ctlbar] && [::vdock::_pi_side .ctlbar] eq ""
}
ok "…the float was REBUILT, content and all" {
    [winfo exists .float_ctlbar.body] && [winfo exists .float_ctlbar.body.b1]
}
ok "…and vdock::docked says NOT docked" {![::vdock::docked .ctlbar]}
::vdock::redock .ctlbar
update
ok "redocking puts it back below .mid" {
    [::vdock::_pi_side .ctlbar] eq "bottom" && [::vdock::docked .ctlbar]
}

# Sash capture walks [.mid panes]; the strip must not enter it.
::vtabs::select inst
update
ok "sash capture sees only the two columns" {
    [llength [dict get [::vtabs::_encode inst] docked]] == 3
    && [llength [lsearch -all -exact [.mid panes] .ctlbar]] == 0
}
# Leave the fixture the way we found it: docked strip destroyed, chat
# back on top, the strip unregistered so later sections are unaffected.
::vdock::undock .ctlbar
update
catch { destroy .float_ctlbar }
::vtabs::select chat
update

# ── boot must not leak regions into the root window ────────────────
#
# USER (2026-08-30): "when I first start the desktop instead of the
# instances pane being shown alone, it is rendered as a fourth pane in
# the chat tab, or the irc pane is in the chat window. when I select a
# tab it eventually settles".
#
# The IRC and Instances builders end with a pack guard written for the
# FLOAT path (undock rebuilds a region inside its float toplevel, and
# the rebuilt root has to pack itself there). The guard tested "is my
# parent a toplevel" — which is ALSO true at boot, where the parent is
# the main window itself. So every region came up pack-managed in the
# root, beside the active tab's layout, and only `.mid add` (which
# implicitly unpacks) on a visit to its own tab absorbed it.
puts "\nboot keeps regions out of the root window"

namespace eval ::leak {
    variable R_a .leak_a
    variable R_b .leak_b
}
# The builder shape of irc.tcl/instpane.tcl, including the guard under
# test: pack only when rebuilt into a FLOAT toplevel, never at boot.
proc ::leak::build_a {{root ""}} {
    variable R_a
    if {$root ne ""} { set R_a $root }
    if {[winfo exists $R_a]} { destroy $R_a }
    ttk::frame $R_a
    text $R_a.body
    pack $R_a.body -fill both -expand 1
    set parent [winfo parent $R_a]
    if {$parent ne "." && [winfo toplevel $parent] eq $parent} { pack $R_a -fill both -expand 1 }
    return $R_a
}
proc ::leak::build_b {{root ""}} {
    variable R_b
    if {$root ne ""} { set R_b $root }
    if {[winfo exists $R_b]} { destroy $R_b }
    ttk::frame $R_b
    text $R_b.body
    pack $R_b.body -fill both -expand 1
    set parent [winfo parent $R_b]
    if {$parent ne "." && [winfo toplevel $parent] eq $parent} { pack $R_b -fill both -expand 1 }
    return $R_b
}

# The boot path, exactly like ::virc::boot: canonical roots, whose
# parent is the main window.
::vdock::register .leak_a {list $::leak::R_a.body} ::leak::build_a
::vdock::register .leak_b {list $::leak::R_b.body} ::leak::build_b
::leak::build_a .leak_a
::leak::build_b .leak_b
::vtabs::define leak "Leak" {.leak_a .leak_b}
update idletasks

ok "a region built at boot is NOT packed into the root" {
    [lsearch -exact [pack slaves .] .leak_a] < 0
    && [lsearch -exact [pack slaves .] .leak_b] < 0
}
ok "…and is not on screen" {![winfo ismapped .leak_a] && ![winfo ismapped .leak_b]}

# The root's pack list is chrome only: the panedwindow and the tab
# strip. Any region frame in here is the leak.
set _strays {}
foreach w [pack slaves .] {
    if {$w ni {.mid .tabbar}} { lappend _strays $w }
}
ok "the root window packs chrome only" {$_strays eq {}}

::vtabs::select leak
update
ok "selecting the tab docks the regions in .mid" \
    {[.mid panes] eq {.leak_a .leak_b}}

# The float path the guard was WRITTEN for still works: undock rebuilds
# the region inside the float toplevel, and there it must pack.
::vdock::undock .leak_a
update
ok "a torn-off region is rebuilt inside its float" \
    {[winfo exists .float_leak_a.body]}
ok "…and packs there" {[winfo ismapped .float_leak_a.body]}
::vdock::redock .leak_a
update
::vtabs::select chat
update
ok "leaving the tab takes the regions with it" \
    {[lsearch -exact [.mid panes] .leak_a] < 0
     && [lsearch -exact [.mid panes] .leak_b] < 0
     && [lsearch -exact [pack slaves .] .leak_a] < 0}

# ── the strip says which key ────────────────────────────────────────
#
# USER: "The tabs don't say which keyboard combination to use when you
# switch them by clicking". The coach says so AFTER a click, which is
# one click too late for someone reading the strip.
puts "\nthe strip advertises its keys"

::vtabs::build
set _chat [.tabbar.tchat cget -text]
ok "the first tab carries a chord" {[string match "C-1 *" $_chat]}
ok "…and still its title"          {[string match "*Chat*" $_chat]}
ok "the second tab carries C-2"    {[string match "C-2 *" [.tabbar.t[lindex [::vtabs::ids] 1] cget -text]]}

# The chord comes from the REGISTRY, through the command the button
# runs — so it cannot advertise a key it does not answer to. Prove it by
# moving the binding: the label has to move with it.
set _saved [::vbindings::table emacs]
proc ::vbindings::table {mode} { return {{C-8 all {::vtabs::select_index 1} "tab 1" tabs}} }
::vtabs::build
ok "moving the binding moves the label" {[string match "C-8 *" [.tabbar.tchat cget -text]]}
rename ::vbindings::table {}
proc ::vbindings::table {mode} {
    variable mode_tables
    if {![dict exists $mode_tables $mode]} { return {} }
    return [dict get $mode_tables $mode]
}
::vtabs::build

# USER 2026-10-01: "I just want one button to do that plus literally
# every other command so that it's all reachable from touching
# buttons". The strip ends in it, on every platform.
ok "the strip ends in ⋮, every command" {
    [winfo exists .tabbar.all] && [.tabbar.all cget -command] eq "::vcmdtree::open"
    && [dict get [pack info .tabbar.all] -side] eq "right"
}

# ── the strip follows a language swap ───────────────────────────────
#
# The tab titles were raw English in every locale. title() now reads the
# catalog (mc), and relabel() re-texts the buttons on a swap — a stored
# -text reads no catalog on its own.
puts "\nthe strip follows a language swap"
rename ::vi18n::mc ::vi18n::mc_orig
proc ::vi18n::mc {s args} {
    if {$s eq "Chat"} { return "Klatsch" }
    if {[llength $args]} { return [format $s {*}$args] }
    return $s
}
::vtabs::build
ok "a translated title renders in the strip" \
    {[string match "*Klatsch*" [.tabbar.tchat cget -text]]}
# A swap re-texts WITHOUT a rebuild: relabel() reads the catalog again.
proc ::vi18n::mc {s args} {
    if {$s eq "Chat"} { return "Bavardage" }
    if {[llength $args]} { return [format $s {*}$args] }
    return $s
}
::vtabs::relabel
ok "relabel re-texts the strip on a swap" \
    {[string match "*Bavardage*" [.tabbar.tchat cget -text]]}
rename ::vi18n::mc {}
rename ::vi18n::mc_orig ::vi18n::mc
::vtabs::build

# ── the periodic layout save ────────────────────────────────────────
#
# A timer, so its save goes out passive (never use on the phone's idle
# clock), it keeps exactly one timer, and a phone's `background` can stop
# it (lib/android.tcl): running there it kept the engine up for ever.
puts "\nthe periodic layout save"
proc layout_timers {} {
    set n 0
    foreach id [after info] {
        if {[lindex [after info $id] 0] eq "::vdock::_layout_save_loop"} { incr n }
    }
    return $n
}
rename ::vdock::save_layout ::vdock::save_layout_real
proc ::vdock::save_layout {} { lappend ::layout_saves $::vapi::passive }
set ::layout_saves {}
set ::vapi::passive 0
::vdock::_layout_save_loop
ok "a periodic save goes out passive, and the flag is down after it" \
    {$::layout_saves eq {1} && $::vapi::passive == 0}
ok "…and re-arms one timer" {[layout_timers] == 1}
::vdock::layout_save_stop
ok "stop leaves no timer" {[layout_timers] == 0}
::vdock::layout_save_start
::vdock::layout_save_start
ok "start (twice) re-arms exactly one, without saving now" \
    {[layout_timers] == 1 && $::layout_saves eq {1}}
::vdock::layout_save_stop
rename ::vdock::save_layout {}
rename ::vdock::save_layout_real ::vdock::save_layout

puts "\ntabtest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
