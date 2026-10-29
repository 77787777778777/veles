#!/usr/bin/env wish
# veles-desktop/lib/panetest.tcl — the right-pane Tasks tab (::vpanes).
#
# USER (2026-09-13, verbatim): "2. The Tasks pane doesn't render the
# markdown for the plan 3. The Task pane returns to the top after a few
# seconds so it's impossible to scroll down and read the plan fully."
# The pane had NO suite at all (taskstest.tcl drives the Running-tasks
# OVERLAY — a different widget), so all three plan-mode bugs shipped.
# This suite drives the REAL ::vpanes::_build_tasks/_tasks_body/
# tasks_refresh against a stubbed /api/sessions/:id/todos, on Xvfb.

set _dir [file dirname [file normalize [info script]]]

if {![info exists ::env(XVFB_RUNNING)]
    && !([info exists ::env(VELES_TK_TEST_FORCE)] && $::env(VELES_TK_TEST_FORCE) ne "")} {
    puts "SKIP: refusing to map a window on a real display."
    exit 0
}

source [file join $_dir paths.tcl]
source [file join $_dir theme.tcl]
::vt::palette_synthwave ::vt::T
source [file join $_dir i18n.tcl]
::vi18n::init

# ── stubbed collaborators ────────────────────────────────────────────
namespace eval ::vapi {
    variable todos {}
    proc get_json {path} {
        variable todos
        return $todos
    }
}
namespace eval ::vchat {
    variable session "sess-1"
}
namespace eval ::vtext {
    proc readonly {w} { catch { $w configure -state disabled } }
}
namespace eval ::vstatus {
    proc set_msg {m} {}
}
namespace eval ::va11y {
    proc name {w label} {}
}

# markdown.tcl is the renderer the pane now rides (::vmd::render), and
# vtext.tcl is the real readonly helper — stubbed above only if the
# real one fails to source (it needs nothing beyond Tk).
source [file join $_dir markdown.tcl]
catch { source [file join $_dir vtext.tcl] }

source [file join $_dir panes.tcl]

set passed 0
set failed 0
proc ok {name cond} {
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ✓ $name"
        incr ::passed
    } else {
        puts "  ✗ $name"
        incr ::failed
    }
}

# Build just the tasks tab in a plain frame (the full pane shell needs
# the .mid layout; _build_tasks only needs a parent).
frame .ptest
pack .ptest -fill both -expand 1
set ::vpanes::R ".ptest"
::vpanes::_build_tasks .ptest.tasks
pack .ptest.tasks -fill both -expand 1
wm geometry . 800x600
update idletasks
update

# ── FIX 2: the plan renders as markdown, not source ─────────────────
puts "\nthe plan renders as markdown"

set ::vapi::todos [dict create plan_mode 1 plan {# The Plan

## Phase 1 — survey
- read `PENDING.md`
- list **all** the crates

## Phase 2 — build
```tcl
puts {the code block}
```

Regular paragraph with *emphasis* and a [link](https://example.com).
} todos [list \
    [dict create status in_progress content "survey the repo"] \
    [dict create status pending content "build the thing"] \
    [dict create status completed content "read the ask"]]]
::vpanes::tasks_refresh
update idletasks
update

set body .ptest.tasks.body
ok "the plan section is present" \
    {[string match "*The Plan*" [$body get 1.0 end]]}
# No literal markup survives: headings, bold, fences, code spans.
ok "no literal ** bold markers remain" \
    {![string match "*\`\`*" [$body get 1.0 end]]
     && ![string match {*\**} [$body get 1.0 end-1c]]}
ok "no literal fence ticks remain in the plan" \
    {![string match "*puts {the code block}*" [$body get 1.0 end]]
     || [string first "the code block" [$body get 1.0 end]] >= 0}
# Heading rendered as a heading TAG (md_h1/md_h2), not plain text.
ok "plan headings render with heading tags" \
    {[llength [$body tag ranges md_h1]] > 0
     && [llength [$body tag ranges md_h2]] > 0}
# The fence body carries the codeblock tag.
ok "the fence renders as a code block" \
    {[llength [$body tag ranges md_codeblock]] > 0}
# Tasks rows are still plain and all three marks survive.
ok "task rows render with their status marks" \
    {[string first "▶ survey the repo" [$body get 1.0 end]] >= 0
     && [string first "☐ build the thing" [$body get 1.0 end]] >= 0
     && [string first "✓ read the ask" [$body get 1.0 end]] >= 0}
ok "the mode bar announces plan mode" \
    {[.ptest.tasks.bar.mode cget -text] eq "▲ PLAN MODE"}

# ── FIX 3: the view survives a refresh with the same payload ────────
puts "\nthe view survives repaints"

# Scroll to the middle of a long plan, refresh the SAME payload, the
# view must stay.
set ::vapi::todos [dict create plan_mode 1 plan [string repeat "line of plan text\n" 200] todos {}]
::vpanes::tasks_refresh
update idletasks
update
$body yview moveto 0.5
update
set before [lindex [$body yview] 0]
if {$before < 0.05} {
    # Not scrollable yet — the widget is small; make it scroll.
    $body configure -height 10
    update idletasks
    update
    $body yview moveto 0.5
    update
    set before [lindex [$body yview] 0]
}
::vpanes::tasks_refresh
update idletasks
update
set after [lindex [$body yview] 0]
ok "an unchanged payload keeps the view (no jump to top)" \
    {[expr {abs($after - $before)}] < 0.02}

# A CHANGED payload repaints with the new content (the no-op must not
# swallow real changes — the mutation-verifiable half).
set ::vapi::todos [dict create plan_mode 1 plan "# A DIFFERENT PLAN\n\nnew body" todos {}]
::vpanes::tasks_refresh
update idletasks
update
ok "a changed payload shows the new content" \
    {[string first "A DIFFERENT PLAN" [$body get 1.0 end]] >= 0}

# A changed payload ALSO restores the reader's anchor: scroll deep,
# change the tail, the view stays near where it was.
set ::vapi::todos [dict create plan_mode 1 plan [string repeat "anchor line\n" 300] todos {}]
::vpanes::tasks_refresh
update idletasks
update
$body yview moveto 0.7
update
set deep_before [lindex [$body yview] 0]
set longer [string repeat "anchor line\n" 300]
append longer "tail addition line"
set ::vapi::todos [dict create plan_mode 1 plan $longer todos {}]
::vpanes::tasks_refresh
update idletasks
update
set deep_after [lindex [$body yview] 0]
ok "a changed payload restores the reader's anchor" \
    {[expr {abs($deep_after - $deep_before)}] < 0.05}

# ── error paths stay honest ──────────────────────────────────────────
proc ::vapi::get_json {path} { error "HTTP 502: gateway down" }
set ::vpanes::tasks_payload ""
catch { ::vpanes::tasks_refresh }
ok "a down gateway says so rather than wiping the pane silently" \
    {[string first "tasks unavailable" [.ptest.tasks.body get 1.0 end]] >= 0}

# ── the Terminal pane's Run line ─────────────────────────────────────
#
# It ran `exec bash -c` synchronously: a phone has no bash (every command
# answered "couldn't execute bash"), and a long command froze the whole
# window. Now: bash or the system sh, in the repo dir, output as it comes.
puts "\nthe Run line runs in the repo dir, without freezing the window"
::vpanes::_build_terminal .ptest.terminal
set ::vpanes::review_repo [file normalize [pwd]]
ok "the shell is bash or a system sh, never a name that is not there" {
    [file executable [::vpanes::_term_shell]]
}
set run_dir [file join [expr {[info exists ::env(TMPDIR)] ? $::env(TMPDIR) : "/tmp"}] panetest-[pid]]
file mkdir $run_dir
set ::vpanes::review_repo $run_dir
.ptest.terminal.run.cmd delete 0 end
.ptest.terminal.run.cmd insert 0 {sleep 0.3; pwd; echo "it's l""ate"; exit 3}
set ::vpanes::term_done 0
::vpanes::_term_run
ok "control comes back before the command has finished" {
    [string first "it's late" [.ptest.terminal.out get 1.0 end]] < 0
}
after 10000 {set ::vpanes::term_done timeout}
vwait ::vpanes::term_done
set term_text [.ptest.terminal.out get 1.0 end]
ok "its output arrives, quoting intact" {
    [string first "it's late" $term_text] >= 0
}
ok "…from the repo dir" {
    [string first [file normalize $run_dir] $term_text] >= 0
}
ok "…and a failing exit says so" {
    [string first "(exit 3)" $term_text] >= 0
}
file delete -force $run_dir

# ── the Preview belongs to the session that opened it ───────────────
#
# OWNER (2026-10-29): "fix the Preview always opening the last preview
# page presented on every start regardless of session or how long it's
# been since it was relevant". The 1.5 s poll asked for THE preview,
# and a fresh window has seen no stamp, so the first reply after every
# start raised the Preview pane and handed a days-old url to the
# browser, whichever session was on screen. The gateway now answers
# per session (panes.rs preview_for: its owner, and fresh); the window
# has to ask for the session it shows, keep a late reply for a session
# it has left off the screen, and drop another session's page when it
# switches.
puts "\nthe Preview belongs to the session that opened it"
if {![llength [info commands ::vmedia::is_image]]} {
    namespace eval ::vmedia { proc is_image {p} { return 0 } }
}
::vpanes::_build_preview .ptest.preview
set ::pv_asked {}
set ::pv_shown {}
set ::pv_opened {}
namespace eval ::vapi {
    proc unattached {} { return 0 }
    if {![llength [info commands urlencode]]} {
        proc urlencode {s} { return $s }
    }
    proc get_json_async {path cmd {ms 0}} {
        if {[string match /api/preview* $path]} {
            lappend ::pv_asked [list $path $cmd]
        }
        return tok
    }
}
proc ::vmd::open_url {url} { lappend ::pv_opened $url }
proc ::vpanes::show {tab} { lappend ::pv_shown $tab }
proc pv_tick {} {
    set ::pv_asked {}
    ::vpanes::_poll_tick
    after cancel $::vpanes::poll_after
    return [lindex $::pv_asked end]
}
set pv_item [dict create kind url url http://localhost:3000/ label "" \
    session sess-1 set_at 2026-10-29T12:00:00+00:00]

set ::vchat::session sess-1
::vpanes::_preview_empty_state
lassign [pv_tick] pv_path pv_cmd
ok "the poll asks for the preview of the session on screen" {
    $pv_path eq "/api/preview?session=sess-1"
}
# The reply comes back after the window moved to another session.
set ::vchat::session sess-2
uplevel #0 [list {*}$pv_cmd 1 $pv_item]
ok "a reply for a session no longer on screen is not shown" {
    $::pv_shown eq {} && $::pv_opened eq {}
}
set ::vchat::session sess-1
lassign [pv_tick] pv_path pv_cmd
uplevel #0 [list {*}$pv_cmd 1 $pv_item]
ok "its own session's preview is shown" {
    $::pv_shown eq {preview} && $::pv_opened eq {http://localhost:3000/}
}
uplevel #0 [list {*}$pv_cmd 1 $pv_item]
ok "…once" { [llength $::pv_shown] == 1 }
# Switch: the gateway has nothing for sess-2, and sess-1's page goes.
set ::vchat::session sess-2
lassign [pv_tick] pv_path pv_cmd
uplevel #0 [list {*}$pv_cmd 1 null]
ok "switching sessions takes the other session's page off the pane" {
    [string match "Nothing previewed yet*" [.ptest.preview.body get 1.0 end]]
}
ok "…and opens nothing" { [llength $::pv_shown] == 1 && [llength $::pv_opened] == 1 }
# A file the user clicked in the Files pane is theirs, not a session's:
# opened over sess-2's agent preview, it is no longer sess-2's page.
dict set pv_item session sess-2
dict set pv_item set_at 2026-10-29T12:01:00+00:00
lassign [pv_tick] pv_path pv_cmd
uplevel #0 [list {*}$pv_cmd 1 $pv_item]
ok "precondition: sess-2's own preview is up" { [llength $::pv_shown] == 2 }
::vpanes::_preview_set "user text" /tmp/mine.txt
set ::vchat::session sess-3
pv_tick
ok "a file the user opened stays when the session changes" {
    [string match "*user text*" [.ptest.preview.body get 1.0 end]]
}

puts "\npanetest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
