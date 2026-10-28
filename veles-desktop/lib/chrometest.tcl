# veles-desktop/lib/chrometest.tcl — the lsp-ui chrome: sideline
# diagnostics and peek.
#
# Two things the Dev pane could not do, both from the plan's Phase C:
#
#   SIDELINE. The server sends a diagnostic's line, column, severity AND
#   message. ::vsyn::diagnostics used the first three to draw a squiggle
#   and threw the message away — so the editor could tell you something
#   on line 40 was wrong and never what. The message now renders at the
#   end of its line, as an EMBEDDED WINDOW: a Text widget's `get` skips
#   embedded windows, so C-x C-s and C-c C-c still see the file the user
#   wrote. Inserted text would have silently saved the error messages
#   into the source, which is the assertion below that matters most.
#
#   PEEK. The references popup was a listbox of "chat.tcl:412" and
#   nothing else: to find out whether a hit was the one you meant you
#   had to jump to it, and jumping meant leaving the buffer you were
#   reading. Peek shows the line at each location and the lines around
#   the selected one, so skimming never leaves the buffer.
#
# It also pins a bug found while writing it: _references_jump called
# `open_file`, a proc that exists nowhere in the app, and parked the
# target position in a shape _apply_jump could not read. Every
# cross-file reference jump threw into bgerror.
#
# Run: scripts/tcl_tests.sh

set _dir [file dirname [file normalize [info script]]]

set failed 0
set passed 0
# A sideline that stops being drawn must report, not abort: a suite that
# dies before its tally reads to CI as a broken run, not as the bug.
proc winidx {w} {
    set names [$w window names]
    if {![llength $names]} { return "" }
    return [$w index [lindex $names 0]]
}
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

source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
namespace eval ::vstatus {
    variable last ""
    proc set_msg {m} { set ::vstatus::last $m }
}
source [file join $_dir place.tcl]
source [file join $_dir syntax.tcl]

wm geometry . 1000x700+0+0

# ── sideline ────────────────────────────────────────────────────────
puts "the diagnostic's message, at the end of its line"

text .ed -wrap none -font {monospace 10}
pack .ed -fill both -expand 1
.ed insert end "proc broken {} {\n    set x \$undefined\n    puts \$x\n}\n"
update idletasks
# The text BEFORE any annotation — the thing the sideline must not
# change. Comparing against a literal here would only test my ability to
# count the trailing newline Tk keeps.
set pristine [.ed get 1.0 end-1c]

::vsyn::diagnostics .ed {
    {2 12 ERROR "undefined variable: undefined"}
}
update
set texts [::vsyn::sideline_texts .ed]
ok "the message is on screen"        {[llength $texts] == 1}
ok "…and it is the message"          {[lindex $texts 0] eq "undefined variable: undefined"}
ok "…placed on the offending line" {[lindex [split [winidx .ed] .] 0] == 2}
ok "…at its end, not in the middle of the code" \
    {[winidx .ed] ne "" && [.ed get 2.0 [winidx .ed]] eq "    set x \$undefined"}
ok "…and the token is still underlined" \
    {[llength [.ed tag ranges syn_diag]] > 0}

# THE assertion. An annotation that ends up in the buffer is an
# annotation that gets saved to the user's file.
set body [.ed get 1.0 end-1c]
ok "the buffer text does NOT contain the message" \
    {![string match "*undefined variable*" $::body]}
ok "…and is byte-for-byte what it was before" {$::body eq $::pristine}

puts "\nseveral diagnostics, one line"
::vsyn::diagnostics .ed {
    {2 12 ERROR "first problem"}
    {2 4  WARNING "second problem"}
}
update
set texts [::vsyn::sideline_texts .ed]
ok "one label, not two stacked"      {[llength $texts] == 1}
ok "…carrying both messages"         {[string match "*first problem*" [lindex $::texts 0]]
                                      && [string match "*second problem*" [lindex $::texts 0]]}

puts "\nseverity is visible, not just recorded"
::vsyn::diagnostics .ed {{2 4 WARNING "just a warning"}}
update
proc labelfg {w} {
    set names [$w window names]
    if {![llength $names]} { return "<none>" }
    return [[lindex $names 0] cget -foreground]
}
ok "a warning is gold"               {[labelfg .ed] eq $::vt::T(gold)}
::vsyn::diagnostics .ed {{2 4 ERROR "a real error"}}
update
ok "an error is red"                 {[labelfg .ed] eq $::vt::T(red)}

puts "\nre-diagnosing replaces, and clearing clears"
::vsyn::diagnostics .ed {{2 4 ERROR "a"} {3 4 ERROR "b"}}
update
ok "two lines, two labels"           {[llength [::vsyn::sideline_texts .ed]] == 2}
::vsyn::diagnostics .ed {{2 4 ERROR "a"}}
update
ok "a shorter run does not leave the old ones behind" \
    {[llength [::vsyn::sideline_texts .ed]] == 1}
ok "…and the widget has no orphan windows" {[llength [.ed window names]] == 1}
::vsyn::clear_diag .ed
update
ok "clear_diag removes the labels"   {[llength [::vsyn::sideline_texts .ed]] == 0}
ok "…and the underlines"             {[llength [.ed tag ranges syn_diag]] == 0}

puts "\nthe sideline is survivable"
# A server that reports past the end of the buffer (a stale didChange,
# a file truncated under it) must not take the editor down with it.
set threw [catch { ::vsyn::diagnostics .ed {{99 4 ERROR "off the end"}} }]
ok "a diagnostic past the end does not throw" {!$::threw}
ok "…and places nothing"                     {[llength [::vsyn::sideline_texts .ed]] == 0}

::vsyn::toggle_sideline
::vsyn::diagnostics .ed {{2 4 ERROR "hidden"}}
update
ok "toggled off, no message is drawn" {[llength [::vsyn::sideline_texts .ed]] == 0}
ok "…but the underline stays"         {[llength [.ed tag ranges syn_diag]] > 0}
::vsyn::toggle_sideline
::vsyn::diagnostics .ed {{2 4 ERROR "back"}}
update
ok "toggled on, it comes back"        {[llength [::vsyn::sideline_texts .ed]] == 1}
::vsyn::clear_diag .ed
destroy .ed

# ── peek ────────────────────────────────────────────────────────────
puts "\npeek shows the line, not just the number"

namespace eval ::vsyn { proc diagnostics {w rows} {} }
# The REAL client, so the wire-shape normalizers under test are the
# ones the app runs; only the transport is replaced.
source [file join $_dir lspclient.tcl]
namespace eval ::vlsp {
    variable up 0
    variable def_result ""
    proc running {} { return $::vlsp::up }
    proc start {path text cb} { return 0 }
    proc sync {text} {}
    proc supports {n} { return 1 }
    proc definition_at {l c cb} { uplevel #0 [list $cb $::vlsp::def_result ""] }
    variable fold_result ""
    proc folding_ranges {cb} { uplevel #0 [list $cb $::vlsp::fold_result ""] }
    variable action_result ""
    variable executed {}
    proc code_action_at {sl sc el ec cb {d {}}} {
        set ::vlsp::last_range [list $sl $sc $el $ec]
        uplevel #0 [list $cb $::vlsp::action_result ""]
    }
    proc execute_command {command cb} { lappend ::vlsp::executed $command }
    variable last_range {}
}
namespace eval ::vemacs { proc install {w} {} }
namespace eval ::va11y { proc name {w t} {} }
namespace eval ::vapi {
    variable files
    proc get_json {url} {
        if {[regexp {path=([^&]*)} $url -> path]} {
            if {[info exists ::vapi::files($path)]} {
                return [dict create text $::vapi::files($path)]
            }
            return [dict create text ""]
        }
        return [dict create text ""]
    }
    proc post {args} { return "{}" }
    proc json_escape {s} { return $s }
}
namespace eval ::json { proc json2dict {s} { return [dict create] } }
namespace eval ::vpanes { variable R .root }
# i18n first: dev.tcl labels its live-patching control through
# ::vi18n::mc, and an unresolved `mc` is a hard error at
# widget-build time, not a fallback. init resolves the identity
# catalog, which is what a suite wants.
source [file join $_dir i18n.tcl]
::vi18n::init
source [file join $_dir dev.tcl]

frame .root
pack .root -fill both -expand 1
::vpanes::_build_dev .root.dev
pack .root.dev -fill both -expand 1
update

set ::vapi::files(/tmp/other.tcl) "line one\nline two\nTHE TARGET LINE\nline four\nline five\n"
set ::vapi::files(/tmp/here.tcl)  "aaa\nbbb\nccc\n"

set ::vdev::current_file /tmp/here.tcl
::vdev::editor_open
update

::vdev::peek_open "references" {
    {/tmp/here.tcl 2 0}
    {/tmp/other.tcl 3 4}
}
update
ok "the peek window is up"           {[winfo exists .vdev_peek]}
set rows {}
foreach i {0 1} { lappend rows [.vdev_peek.f.pw.l.list get $i] }
ok "each row names its file"         {[string match "here.tcl*" [lindex $::rows 0]]}
ok "…and its line number"            {[string match "*2*" [lindex $::rows 0]]}
ok "…and the SOURCE at that line"    {[string match "*bbb*" [lindex $::rows 0]]}
ok "…for a file that is not open too" {[string match "*THE TARGET LINE*" [lindex $::rows 1]]}

ok "the preview shows the target"    {[string match "*bbb*" [.vdev_peek.f.pw.r.body get 1.0 end]]}
ok "…with context around it"         {[string match "*aaa*" [.vdev_peek.f.pw.r.body get 1.0 end]]
                                      && [string match "*ccc*" [.vdev_peek.f.pw.r.body get 1.0 end]]}
ok "…and the hit marked"             {[llength [.vdev_peek.f.pw.r.body tag ranges peekhit]] > 0}

.vdev_peek.f.pw.l.list selection clear 0 end
.vdev_peek.f.pw.l.list selection set 1
::vdev::_peek_preview
update
ok "selecting another hit re-previews" \
    {[string match "*THE TARGET LINE*" [.vdev_peek.f.pw.r.body get 1.0 end]]}
ok "…and names its file"             {[string match "*other.tcl*" [.vdev_peek.f.pw.r.body get 1.0 end]]}

puts "\nan unsaved buffer peeks at what you are editing"
# Reading the file from disk here would show the last save, which for a
# buffer you are mid-edit in is the wrong file.
[::vdev::editor_widget] insert 2.0 "EDITED-NOT-SAVED "
::vdev::peek_close
::vdev::peek_open "references" {{/tmp/here.tcl 2 0}}
update
ok "the peek reads the live buffer"  {[string match "*EDITED-NOT-SAVED*" [.vdev_peek.f.pw.l.list get 0]]}
::vdev::peek_close

# …and the same for a buffer open in ANOTHER tab. That is the case the
# buffer scan exists for: the current-editor shortcut cannot see it, so
# without the scan you would peek at the last save of a file you have
# unsaved edits in — sitting in a tab two along.
set ::vdev::current_file /tmp/other.tcl
::vdev::editor_open
update
[::vdev::editor_widget] insert 1.0 "EDITED-IN-ANOTHER-TAB "
::vdev::buf_show [lindex [::vdev::buf_list] 0]
update
ok "the other tab is not the one showing" \
    {[::vdev::buf_path_of [::vdev::buf_current]] eq "/tmp/here.tcl"}
::vdev::peek_open "references" {{/tmp/other.tcl 1 0}}
update
ok "…and peeking it still reads its live buffer" \
    {[string match "*EDITED-IN-ANOTHER-TAB*" [.vdev_peek.f.pw.l.list get 0]]}
::vdev::peek_close
# Back to one tab so the jump tests below start clean.
foreach id [lrange [::vdev::buf_list] 1 end] {
    ::vdev::buf_close $id
    ::vdev::buf_close $id
}
::vdev::peek_open "references" {{/tmp/here.tcl 2 0}}
update

puts "\njumping"
# Same file: move the cursor, do not reopen anything.
set before [::vdev::buf_current]
.vdev_peek.f.pw.l.list selection set 0
::vdev::_peek_jump
update
ok "a same-file jump stays in the buffer" {[::vdev::buf_current] eq $::before}
ok "…and moves the cursor there"          {[string match "2.*" [[::vdev::editor_widget] index insert]]}
ok "…and closes the peek"                 {![winfo exists .vdev_peek]}

# Cross-file. This is the path that used to call `open_file` — a proc
# that does not exist — and park the position as a two-element list.
::vdev::peek_open "references" {{/tmp/other.tcl 3 4}}
update
.vdev_peek.f.pw.l.list selection set 0
set threw [catch { ::vdev::_peek_jump } jerr]
update
ok "a cross-file jump does not throw"     {!$::threw}
ok "…it opens the other file"             {[::vdev::buf_path_of [::vdev::buf_current]] eq "/tmp/other.tcl"}
ok "…in its own tab"                      {[llength [::vdev::buf_list]] == 2}
ok "…and lands on the line"               {[string match "3.*" [[::vdev::editor_widget] index insert]]}
ok "…with the buffer holding that file"   {[string match "*THE TARGET LINE*" [[::vdev::editor_widget] get 1.0 end]]}

puts "\npeek closes"
::vdev::peek_open "references" {{/tmp/here.tcl 1 0}}
update
::vdev::peek_close
ok "Escape's handler closes the window"   {![winfo exists .vdev_peek]}

puts "\npeek definition"
set ::vlsp::up 1
set ::vlsp::def_result [dict create uri "file:///tmp/other.tcl" \
    range [dict create start [dict create line 2 character 4]]]
::vdev::_peek_def
update
ok "C-c C-p peeks the definition"         {[winfo exists .vdev_peek]}
ok "…at the definition's line"            {[string match "*THE TARGET LINE*" [.vdev_peek.f.pw.r.body get 1.0 end]]}
::vdev::peek_close

puts "\nreferences arrive as LSP Locations and become peek items"
::vdev::_references_result [list \
    [dict create uri "file:///tmp/other.tcl" \
        range [dict create start [dict create line 2 character 4]]] \
    [dict create uri "file:///tmp/here.tcl" \
        range [dict create start [dict create line 0 character 0]]]] ""
update
ok "both locations became rows"           {[.vdev_peek.f.pw.l.list size] == 2}
ok "…with 0-based wire lines shown 1-based" \
    {[string match "*    3*" [.vdev_peek.f.pw.l.list get 0]]}
::vdev::peek_close

# An empty result must say so rather than opening an empty window.
::vdev::_references_result "null" ""
ok "no references opens nothing"          {![winfo exists .vdev_peek]}
# editor_status writes into the editor's OWN status label, not the app
# status bar — that is the whole point of the attach plumbing.
ok "…and says so where the editor is" \
    {[string match "*no references*" [.root.dev.edit.status cget -text]]}

# ── folding ────────────────────────────────────────────────────────
puts "\nfolding hides lines without touching the file"

::vdev::unfold_all
set ::vdev::current_file /tmp/fold.tcl
set ::vapi::files(/tmp/fold.tcl) "proc outer {} {\n    set a 1\n    set b 2\n    set c 3\n}\nproc after_it {} {}\n"
::vdev::editor_open
update
set ed [::vdev::editor_widget]
set pristine [$ed get 1.0 end-1c]

# The server answers 0-based, start..end inclusive: proc outer spans
# wire lines 0..4, which is editor lines 1..5.
set ::vlsp::fold_result [list \
    [dict create startLine 0 endLine 4 kind "region"] \
    [dict create startLine 1 endLine 3 kind "region"]]
$ed mark set insert 3.0
::vdev::toggle_fold
update
ok "a fold was applied"          {[llength [::vdev::folded_ranges $::ed]] == 1}
# The INNERMOST range wins: folding the whole proc when the cursor is on
# an inner block would hide the thing you were looking at.
ok "…the innermost one"          {[::vdev::folded_ranges $::ed] eq {{2 4}}}
ok "…and it elides"              {[llength [$::ed tag ranges vdevfold_2]] > 0}
ok "…with a marker saying how much" \
    {[string match "*2 lines*" [lindex [::vsyn::annotation_texts $::ed fold2] 0]]}

# THE claim: elision is a display property. Fold a region and the file
# you save is unchanged.
ok "the buffer text is untouched" {[$::ed get 1.0 end-1c] eq $::pristine}

$ed mark set insert 3.0
::vdev::toggle_fold
update
ok "toggling again unfolds"      {[llength [::vdev::folded_ranges $::ed]] == 0}
ok "…and drops the elide tag"    {[lsearch -exact [$::ed tag names] vdevfold_2] < 0}
ok "…and the marker"             {[llength [::vsyn::annotation_texts $::ed fold2]] == 0}

# The outer range when the cursor is outside the inner one.
$ed mark set insert 5.0
::vdev::toggle_fold
update
ok "the cursor picks which fold" {[::vdev::folded_ranges $::ed] eq {{1 5}}}
::vdev::unfold_all
ok "unfold_all clears them"      {[llength [::vdev::folded_ranges $::ed]] == 0}

# Unfolding must work with no server: you must always be able to get
# your text back, even if the thing that folded it has died.
::vdev::fold_apply $ed 1 5
set ::vlsp::up 0
$ed mark set insert 3.0
::vdev::toggle_fold
ok "unfolding works with the lsp gone" {[llength [::vdev::folded_ranges $::ed]] == 0}
set ::vlsp::up 1

# A range past the end of the buffer must not throw or hide everything.
set threw [catch { ::vdev::fold_apply $ed 400 500 } r]
ok "a fold past the end is refused" {!$::threw && !$::r}
ok "…and nothing got hidden"        {[llength [::vdev::folded_ranges $::ed]] == 0}

set ::vlsp::fold_result "null"
$ed mark set insert 2.0
::vdev::toggle_fold
ok "a server with no ranges says so" \
    {[string match "*no foldable*" [.root.dev.edit.status cget -text]]}

# ── code actions ───────────────────────────────────────────────────
puts "\ncode actions apply the fix the server already has"

set ::vdev::current_file /tmp/act.tcl
set ::vapi::files(/tmp/act.tcl) "set x 1\nset y 2\n"
::vdev::editor_open
update
set ed [::vdev::editor_widget]

proc edit_for {uri sl sc el ec text} {
    return [dict create changes [dict create $uri \
        [list [dict create range [dict create \
            start [dict create line $sl character $sc] \
            end [dict create line $el character $ec]] newText $text]]]]
}

set ::vlsp::action_result [list \
    [dict create title "Rename x to xx" kind "quickfix" \
        edit [edit_for "file:///tmp/act.tcl" 0 4 0 5 "xx"]] \
    [dict create title "Run the server's own fixer" \
        command [dict create title "fix" command "tcl.fixit" arguments [list "a"]]]]

$ed mark set insert 1.4
::vdev::code_action
update
ok "the action list is up"       {[winfo exists .vdev_actions]}
ok "…listing both"               {[.vdev_actions.f.list size] == 2}
ok "…with the kind shown"        {[string match "*quickfix*" [.vdev_actions.f.list get 0]]}
ok "…and the cursor's line asked about, 0-based" \
    {[lindex $::vlsp::last_range 0] == 0}

::vdev::apply_code_action 0
# Read the status BEFORE the event loop turns: Tk queues <<Modified>>,
# and its handler rewrites the status line to "act.tcl • (modified)" the
# moment we let it run. What is asserted here is what the user sees at
# the instant the action lands.
set said [.root.dev.edit.status cget -text]
update
ok "applying the edit changes the buffer" {[string match "set xx 1*" [$::ed get 1.0 end]]}
ok "…and closes the list"                 {![winfo exists .vdev_actions]}
ok "…and says what it did"                {[string match "*1 edit*" $::said]}
ok "…naming the action"                   {[string match "Rename x to xx*" $::said]}

# A bare Command cannot be applied locally — it runs on the server. The
# status has to say that, not claim the fix landed.
set ::vlsp::executed {}
::vdev::code_action
update
::vdev::apply_code_action 1
set said [.root.dev.edit.status cget -text]
ok "a command action is sent to the server" {[llength $::vlsp::executed] == 1}
proc sent_name {} {
    if {![llength $::vlsp::executed]} { return "<nothing sent>" }
    if {[catch { dict get [lindex $::vlsp::executed 0] command } n]} { return "<no name>" }
    return $n
}
ok "…by name"                               {[sent_name] eq "tcl.fixit"}
ok "…and says so rather than claiming a fix" {[string match "*sent to the server*" $::said]}

set ::vlsp::action_result "null"
::vdev::code_action
ok "no actions opens no window" {![winfo exists .vdev_actions]}
ok "…and says so"               {[string match "*no code actions*" [.root.dev.edit.status cget -text]]}

# A selection, not just a point: the range sent must cover it.
set ::vlsp::action_result [list [dict create title "x" kind "quickfix"]]
$ed tag add sel 1.0 2.3
::vdev::code_action
update
ok "a region asks about the whole region" \
    {[lindex $::vlsp::last_range 0] == 0 && [lindex $::vlsp::last_range 2] == 1
     && [lindex $::vlsp::last_range 3] == 3}
catch { destroy .vdev_actions }

puts "\nchrometest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
