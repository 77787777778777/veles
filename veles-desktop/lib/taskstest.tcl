#!/usr/bin/env wish
# veles-desktop/lib/taskstest.tcl — the Running-tasks overlay.
#
# USER (2026-09-02): "the implementation of shells/backgrounds, monitors
# and subagents in veles was done in hermes and possibly never verified."
# The audit found task state was tracked and displayed NOWHERE: the
# registry was written on every delegate call and read by nothing, and
# the desktop's own /background printed a string telling you to go and
# run `veles chat --resume` in a terminal — under a help line claiming it
# "moved the running turn to the background".
#
# Four promises this suite holds the desktop to:
#   * running tasks are LISTED, with their kind and age;
#   * every row offers cancel, and cancel actually POSTs;
#   * only a shell offers [output] — a subagent has none to show;
#   * a gateway that is down says so, rather than rendering an empty
#     list that looks like "nothing is running".
#
# Drives the REAL ::voverlay::_tasks_fill against stubbed HTTP, on Xvfb.

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
    variable tasks {}
    variable fail 0
    variable posts {}
    variable output {}
    proc get_json {path} {
        variable tasks
        variable fail
        variable output
        if {$fail} { error "HTTP 502: gateway down" }
        if {[string match "*/output" $path]} { return $output }
        return $tasks
    }
    proc post {path body} {
        variable posts
        lappend posts $path
        return "{}"
    }
}
namespace eval ::vstatus {
    variable msgs {}
    proc set_msg {m} { variable msgs; lappend msgs $m }
}
namespace eval ::vmanage {
    proc _reason {e} { return $e }
    # The overlay chrome, reduced to what _tasks touches.
    proc window {w title geom minsize} {
        toplevel $w
        wm withdraw $w
        ttk::frame $w.bar
        pack $w.bar -fill x
    }
    proc button {parent name label cmd {tip ""}} {
        ttk::button $parent.$name -text $label -command $cmd
        pack $parent.$name -side left
    }
}

source [file join $_dir vtext.tcl]
source [file join $_dir jsonfield.tcl]
source [file join $_dir overlays.tcl]

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

# Build the panel once; each test rewrites what the endpoint answers and
# re-fills, exactly as the 3s poll does.
proc refill {} {
    catch { destroy .ov_tasks }
    ::voverlay::_tasks .ov_tasks
    # The panel starts its own poll; cancel it so the suite is
    # deterministic and does not leak `after` handlers between tests.
    ::voverlay::_tasks_stop .ov_tasks .ov_tasks
    return [.ov_tasks.main.t get 1.0 end]
}

proc task {id kind label secs {status running} {csession ""}} {
    set d [dict create id $id kind $kind label $label \
                       elapsed_s $secs status $status session s1]
    if {$csession ne ""} { dict set d child_session $csession }
    return $d
}

# ── a mixed list renders every kind ──────────────────────────────────
puts "\nrunning tasks are listed, with kind and age"
set ::vapi::tasks [dict create tasks [list \
    [task 1 subagent "audit the loader" 42] \
    [task 2 shell "sleep 300" 7] \
    [task 3 monitor "shell: cargo test" 3]] live 3 total_tracked 3]
set body [refill]
ok "the subagent row is there"      {[string match "*audit the loader*" $body]}
ok "the shell row is there"         {[string match "*sleep 300*" $body]}
ok "the monitor row is there"       {[string match "*cargo test*" $body]}
ok "ids are shown"                  {[string first "\[1\]" $body] >= 0}
ok "kinds are shown"                {[string match "*subagent*" $body]
                                     && [string match "*monitor*" $body]}
ok "ages are shown"                 {[string match "*42s*" $body]}

# ── per-row actions ──────────────────────────────────────────────────
puts "\nevery row offers cancel; only a shell offers output"
ok "the shell row offers output"    {[llength [.ov_tasks.main.t tag ranges btn_2_output]] > 0}
ok "the subagent row does NOT"      {[llength [.ov_tasks.main.t tag ranges btn_1_output]] == 0}
ok "the monitor row does NOT"       {[llength [.ov_tasks.main.t tag ranges btn_3_output]] == 0}
foreach id {1 2 3} {
    ok "task $id offers cancel"     {[llength [.ov_tasks.main.t tag ranges btn_${id}_cancel]] > 0}
}

# ── cancel actually posts ────────────────────────────────────────────
# The failure this excludes: a button that relabels the row and stops
# nothing. a2a's tasks/cancel does exactly that, which is why the
# assertion is on the POST and not on the rendering.
puts "\ncancel posts to the API"
set ::vapi::posts {}
::voverlay::_tasks_action .ov_tasks.main.t 2 cancel
ok "posted to the cancel route"     {[lsearch -exact $::vapi::posts "/api/tasks/2/cancel"] >= 0}
ok "…and said so in the status bar" {[string match "*cancelled task 2*" [lindex $::vstatus::msgs end]]}

# ── output is fetched and shown ──────────────────────────────────────
puts "\noutput is fetched for a shell"
set ::vapi::output [dict create id 2 output "line one\nline two"]
::voverlay::_tasks_action .ov_tasks.main.t 2 output
set body [.ov_tasks.main.t get 1.0 end]
ok "the captured output is shown"   {[string match "*line two*" $body]}

# ── attach: a subagent IS a session (USER taxonomy, 2026-09-17) ─────
# The taxonomy promised children are "available to attach and review
# from the session list". The tasks overlay is where a RUNNING child is
# attachable, so a subagent whose registry row carries a child_session
# must offer [attach], and nothing else may: a shell and a monitor have
# no session to switch to.
puts "\na subagent with a session offers attach; nothing else does"
set ::vapi::tasks [dict create tasks [list \
    [task 1 subagent "audit the loader" 42 running 20260917_120000_abc123] \
    [task 2 shell "sleep 300" 7] \
    [task 3 monitor "shell: cargo test" 3]] live 3 total_tracked 3]
set body [refill]
ok "the subagent row offers attach" \
    {[llength [.ov_tasks.main.t tag ranges btn_1_attach:20260917_120000_abc123]] > 0}
ok "…labelled \[attach\], not the raw verb:arg" \
    {[string match "*\[attach\]*" $body]}
foreach id {2 3} {
    ok "task $id (no session) offers no attach" \
        {[lsearch -glob [.ov_tasks.main.t tag names] "btn_${id}_attach:*"] < 0}
}
# A subagent whose row has NO child_session (an older engine, or a spawn
# where provisioning fell back) must not offer a dead link.
set ::vapi::tasks [dict create tasks [list \
    [task 4 subagent "no session yet" 5]] live 1 total_tracked 1]
set body [refill]
ok "a session-less subagent offers no attach" \
    {[string first "attach" $body] < 0}

# Clicking attach navigates: it must ask the chat to open the CHILD
# session, not the parent it was listed under. The failure this
# excludes: a link that closes the overlay and switches you nowhere.
puts "\nattach switches the chat to the child session"
set ::opened_sessions {}
namespace eval ::vchat {
    proc open_session {sid} { lappend ::opened_sessions $sid }
}
set ::vapi::tasks [dict create tasks [list \
    [task 1 subagent "audit the loader" 42 running 20260917_120000_abc123]] \
    live 1 total_tracked 1]
refill
::voverlay::_tasks_action .ov_tasks.main.t 1 attach:20260917_120000_abc123
ok "opened the child session" \
    {[lsearch -exact $::opened_sessions 20260917_120000_abc123] >= 0}
ok "…and said so in the status bar" \
    {[string match "*attached to child session*" [lindex $::vstatus::msgs end]]}

# ── empty and unreachable are DIFFERENT ──────────────────────────────
# The distinction that matters operationally: "nothing is running" and
# "I cannot tell you what is running" must never look the same.
puts "\nempty and unreachable read differently"
set ::vapi::tasks [dict create tasks {} live 0 total_tracked 0]
set body [refill]
ok "an empty list says so"          {[string match "*no running tasks*" $body]}
ok "…and offers no rows"            {[string first "cancel" $body] < 0}

set ::vapi::fail 1
set body [refill]
ok "a dead gateway says unavailable" {[string match "*unavailable*" $body]}
ok "…and does NOT claim nothing runs" {![string match "*no running tasks*" $body]}
set ::vapi::fail 0

# ── finished tasks are not shown as running ──────────────────────────
puts "\nfinished tasks drop out of the list"
set ::vapi::tasks [dict create tasks [list \
    [task 1 subagent "done thing" 9 done] \
    [task 2 shell "live thing" 4]] live 1 total_tracked 2]
set body [refill]
ok "the finished one is gone"       {![string match "*done thing*" $body]}
ok "the running one remains"        {[string match "*live thing*" $body]}

# ── the poll stops with the window ───────────────────────────────────
# A panel that keeps polling a gateway after it is closed is a leak the
# instances pane already learned about the hard way.
puts "\nclosing the panel stops the poll"
::voverlay::_tasks .ov_tasks2
ok "polling while open"             {$::voverlay::tasks_after ne ""}
destroy .ov_tasks2
ok "…and stopped on destroy"        {$::voverlay::tasks_after eq ""}

# …and the shape the engine ACTUALLY sends.
#
# `child_session` is an Option<String> (tasks.rs:257), so a shell or a
# monitor row goes out as `"child_session": null` — and tcllib's
# ::json::json2dict renders JSON null as the four-character STRING
# "null". `$csession ne ""` is true for it, so every shell and monitor
# row in this overlay grew an [attach] link pointing at a session
# called "null", and clicking it asked the chat to open one.
#
# The fixture above could not show it: `task` only sets the key when it
# is non-empty, which is not what the server does. THAT is the defect
# this block exists for — a fixture that cannot produce the wire's
# shape tests the fixture.
puts "\na null child_session is not a session"
set ::vapi::tasks [dict create tasks [list \
    [task 5 shell "sleep 300" 7 running null] \
    [task 6 monitor "shell: cargo test" 3 running null] \
    [task 7 subagent "real child" 9 running 20260917_120000_abc123]] \
    live 3 total_tracked 3]
set body [refill]
foreach id {5 6} {
    ok "task $id (child_session:null) offers no attach" \
        {[lsearch -glob [.ov_tasks.main.t tag names] "btn_${id}_attach:*"] < 0}
}
ok "…and a real child_session still offers one" \
    {[llength [.ov_tasks.main.t tag ranges btn_7_attach:20260917_120000_abc123]] > 0}

puts "\ntaskstest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
