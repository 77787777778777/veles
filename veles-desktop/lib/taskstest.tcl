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
source [file join $_dir a11y.tcl]

# ── stubbed collaborators ────────────────────────────────────────────
namespace eval ::vapi {
    variable tasks {}
    variable ok_errors {}
    variable fail 0
    variable posts {}
    variable output {}
    proc urlencode {s} { return $s }
    proc get_json {path} {
        variable tasks
        variable fail
        variable output
        if {$fail} { error "HTTP 502: gateway down" }
        if {[string match "*/output" $path]} { return $output }
        return $tasks
    }
    proc get_json_async {path cmd {ms 0}} {
        variable async_calls
        variable tasks
        variable fail
        variable output
        lappend async_calls [list $path $cmd]
        if {$fail} {
            after idle [list ::vapi::_deliver $cmd 0 "HTTP 502: gateway down"]
        } elseif {[string match "*/output*" $path]} {
            after idle [list ::vapi::_deliver $cmd 1 $output]
        } else {
            after idle [list ::vapi::_deliver $cmd 1 $tasks]
        }
        return async-[llength $async_calls]
    }
    proc _deliver {cmd ok payload} {
        {*}$cmd $ok $payload
    }
    variable async_calls {}
    proc post {path body} {
        variable posts
        lappend posts $path
        return "{}"
    }
    proc post_async {path body {on_err ""} {on_ok ""}} {
        variable posts
        lappend posts $path
        # Exactly as api.tcl:_post_async_done calls it: `{*}$on_ok` with NO
        # response argument, and a callback error caught and reported, not
        # raised. This stub used to append a "{}" response the real one
        # never passes, so a three-argument callback passed here and threw
        # "wrong # args" in the real desktop (SMOKE-14).
        if {$on_ok ne ""} {
            after idle [list apply {{on_ok} {
                if {[catch { uplevel #0 [list {*}$on_ok] } e]} {
                    lappend ::vapi::ok_errors $e
                }
            }} $on_ok]
        }
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
source [file join $_dir taskpanel.tcl]

namespace eval ::vmanage {
    proc confirm {w title detail} { return 1 }
}
namespace eval ::vchat { variable session s1 }

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
    if {![winfo exists .ov_tasks]} {
        ::voverlay::_tasks .ov_tasks
    } else {
        ::voverlay::_tasks_fill .ov_tasks
    }
    # The panel starts its own poll; cancel it so the suite is
    # deterministic and does not leak `after` handlers between tests.
    after cancel $::voverlay::tasks_after
    set ::voverlay::tasks_after ""
    update
    return "[.ov_tasks.list.rows get 0 end]\n[.ov_tasks.detail.t get 1.0 end]"
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
ok "kinds are shown"                {[string match "*Subagent*" $body]
                                     && [string match "*Monitor*" $body]}
ok "ages are shown"                 {[string match "*42s*" $body]}

# ── selection through the LISTBOX, not by id ─────────────────────────
# The list interleaves section headers ("── Shells ──") with task rows.
# Selection used to map the raw listbox index onto the task list, so a
# click on "[2] sleep 300" selected task 3 and Cancel then targeted it
# (review TSG-1). Every check above selects by id and could not see it;
# these go through <<ListboxSelect>>, the event a click, an arrow key
# or a tap on Android produces.
puts "\nclicking, arrowing or tapping a row selects THAT row's task"
set _lb .ov_tasks.list.rows
proc row_of {needle} {
    set i 0
    foreach r [.ov_tasks.list.rows get 0 end] {
        if {[string match "*$needle*" $r]} { return $i }
        incr i
    }
    return -1
}
proc highlighted {} {
    set cur [.ov_tasks.list.rows curselection]
    if {![llength $cur]} { return "" }
    return [.ov_tasks.list.rows get [lindex $cur 0]]
}
proc click_row {index} {
    .ov_tasks.list.rows selection clear 0 end
    .ov_tasks.list.rows selection set $index
    .ov_tasks.list.rows activate $index
    event generate .ov_tasks.list.rows <<ListboxSelect>>
    update
}
ok "after a fill the highlight is on the selected task's row, not a header" {
    [string match "\\\[$::voverlay::tasks_selected\\\]*" [highlighted]]
}
click_row [row_of "sleep 300"]
ok "clicking the shell row selects the shell" {$::voverlay::tasks_selected == 2}
ok "…and the detail pane describes it" {
    [string match "task 2 — shell*" [.ov_tasks.detail.t get 1.0 end]]
}
click_row [row_of "cargo test"]
ok "clicking the monitor row selects the monitor" {$::voverlay::tasks_selected == 3}
click_row [row_of "audit the loader"]
ok "clicking the subagent row selects the subagent" {$::voverlay::tasks_selected == 1}
ok "…and that row stays highlighted" {[string match "*audit the loader*" [highlighted]]}
set ::vapi::posts {}
::voverlay::_tasks_selected_cancel .ov_tasks
update
ok "Cancel targets the row that was clicked" {
    [lsearch -exact $::vapi::posts "/api/tasks/1/cancel?session=s1"] >= 0
    && [llength $::vapi::posts] == 1
}
# A header is not a task: landing on one (a click, or an arrow key
# stepping over a section boundary) moves on to the nearest task row in
# the direction of travel, so the keyboard never gets stuck on it.
click_row [row_of "sleep 300"]
click_row [row_of "── Monitors"]
ok "stepping down onto a header lands on the next section's task" {
    $::voverlay::tasks_selected == 3 && [string match "*cargo test*" [highlighted]]
}
click_row [row_of "── Monitors"]
ok "stepping up onto a header lands on the previous section's task" {
    $::voverlay::tasks_selected == 2 && [string match "*sleep 300*" [highlighted]]
}
::voverlay::_tasks_select .ov_tasks 3
ok "selecting by id highlights that task's own row" {[string match "*cargo test*" [highlighted]]}
::voverlay::_tasks_fill .ov_tasks
update
ok "a refresh keeps the highlight on the selected task" {
    $::voverlay::tasks_selected == 3 && [string match "*cargo test*" [highlighted]]
}
ok "…and the keyboard cursor with it" {
    [string match "*cargo test*" [$_lb get active]]
}
# The real arrow-key binding, end to end. Keys reach a mapped window
# only, so map it for this block and withdraw it again after.
wm deiconify .ov_tasks
update
focus -force $_lb
update
::voverlay::_tasks_select .ov_tasks 2
event generate $_lb <Down>
update
ok "the Down key walks over a section header to the next task" {
    $::voverlay::tasks_selected == 3 && [string match "*cargo test*" [highlighted]]
}
event generate $_lb <Up>
update
ok "…and Up walks back over it" {
    $::voverlay::tasks_selected == 2 && [string match "*sleep 300*" [highlighted]]
}
wm withdraw .ov_tasks
update

# A 200 whose body has no `tasks` key is no list at all: the previous
# selection must not stay armed behind "tasks unavailable" (TSG-8).
puts "\na malformed list answer disarms the per-task buttons"
::voverlay::_tasks_select .ov_tasks 2
set _keep $::vapi::tasks
set ::vapi::tasks [dict create error "registry exploded"]
set body [refill]
ok "a body without tasks says unavailable" {[string match "*unavailable*" $body]}
ok "…and disables output and cancel" {
    [.ov_tasks.bar.output instate disabled] && [.ov_tasks.bar.cancel instate disabled]
}
set ::vapi::posts {}
::voverlay::_tasks_selected_cancel .ov_tasks
update
ok "…so Cancel can no longer post for the vanished selection" {[llength $::vapi::posts] == 0}
set ::vapi::tasks $_keep
refill
ok "the next good list re-selects the task that was selected" {
    $::voverlay::tasks_selected == 2 && [string match "*sleep 300*" [highlighted]]
}

# ── per-row actions ──────────────────────────────────────────────────
puts "\nevery row offers cancel; only a shell offers output"
::voverlay::_tasks_select .ov_tasks 2
ok "the shell row offers output"    {[.ov_tasks.bar.output instate !disabled]}
::voverlay::_tasks_select .ov_tasks 1
ok "the subagent row does NOT"      {[.ov_tasks.bar.output instate disabled]}
::voverlay::_tasks_select .ov_tasks 3
ok "the monitor row does NOT"       {[.ov_tasks.bar.output instate disabled]}
foreach id {1 2 3} {
    ::voverlay::_tasks_select .ov_tasks $id
    ok "task $id offers cancel"     {[.ov_tasks.bar.cancel instate !disabled]}
}

# ── cancel actually posts ────────────────────────────────────────────
# The failure this excludes: a button that relabels the row and stops
# nothing. a2a's tasks/cancel does exactly that, which is why the
# assertion is on the POST and not on the rendering.
puts "\ncancel posts to the API"
set ::vapi::posts {}
::voverlay::_tasks_action .ov_tasks.main 2 cancel
update
ok "posted to the cancel route"     {[lsearch -exact $::vapi::posts "/api/tasks/2/cancel?session=s1"] >= 0}
ok "…and said so in the status bar" {[string match "*cancelled task 2*" [lindex $::vstatus::msgs end]]}
ok "…and its completion callback ran cleanly" {[llength $::vapi::ok_errors] == 0}

# ── output is fetched and shown ──────────────────────────────────────
puts "\noutput is fetched for a shell"
set ::vapi::output [dict create id 2 output "line one\nline two"]
::voverlay::_tasks_select .ov_tasks 2
::voverlay::_tasks_action .ov_tasks.main 2 output
update
set body [.ov_tasks.detail.t get 1.0 end]
ok "the captured output is shown"   {[string match "*line two*" $body]}

proc attach_offered {id} {
    ::voverlay::_tasks_select .ov_tasks $id
    return [.ov_tasks.bar.attach instate !disabled]
}

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
::voverlay::_tasks_select .ov_tasks 1
ok "the subagent row offers attach" \
    {[.ov_tasks.bar.attach instate !disabled]}
ok "…labelled \[attach\], not the raw verb:arg" \
    {[string match "*\[attach\]*" $body]}
foreach id {2 3} {
    ok "task $id (no session) offers no attach" {![attach_offered $id]}
}
# A subagent whose row has NO child_session (an older engine, or a spawn
# where provisioning fell back) must not offer a dead link.
set ::vapi::tasks [dict create tasks [list \
    [task 4 subagent "no session yet" 5]] live 1 total_tracked 1]
set body [refill]
ok "a session-less subagent offers no attach" {![attach_offered 4]}

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
::voverlay::_tasks_action .ov_tasks.main 1 attach:20260917_120000_abc123
ok "opened the child session" \
    {[lsearch -exact $::opened_sessions 20260917_120000_abc123] >= 0}
ok "…and said so in the status bar" \
    {[string match "*attached to child session*" [lindex $::vstatus::msgs end]]}

# ── foreground detach: different from cancelling a listed background task ──
puts "\nthe panel can move the session's foreground command to the background"
namespace eval ::vchat {
    variable detach_calls 0
    variable turn_running 0
    proc detach_current {} { variable detach_calls; incr detach_calls }
    proc is_running {sid} { variable turn_running; return $turn_running }
}
# Attaching above closes this panel. Detach is a separate window/action.
refill
ok "the panel offers foreground detach" {[winfo exists .ov_tasks.bar.detach]}
ok "the action is named distinctly from Attach" \
    {[.ov_tasks.bar.detach cget -text] eq "Detach foreground"}
# It follows the composer's Detach: armed while this session's turn
# runs (whether that turn is inside a foreground command is engine
# state, and the endpoint answers honestly), disarmed otherwise.
ok "with no turn running, Detach is disabled" {[.ov_tasks.bar.detach instate disabled]}
set ::vchat::turn_running 1
refill
ok "with a turn running, Detach is enabled" {[.ov_tasks.bar.detach instate !disabled]}
set ::vapi::async_calls {}
.ov_tasks.bar.detach invoke
ok "invokes the same session detach action as the composer" \
    {$::vchat::detach_calls == 1}
after 1000 {set ::_detach_waited 1}
vwait ::_detach_waited
ok "…and re-reads the list, where the adopted shell appears" {
    [lsearch -glob [lmap c $::vapi::async_calls {lindex $c 0}] "/api/tasks?session=s1"] >= 0
}
set ::vchat::turn_running 0

# ── empty and unreachable are DIFFERENT ──────────────────────────────
# The distinction that matters operationally: "nothing is running" and
# "I cannot tell you what is running" must never look the same.
puts "\nempty and unreachable read differently"
set ::vapi::tasks [dict create tasks {} live 0 total_tracked 0]
set body [refill]
ok "an empty list says so"          {[string match "*no shells or monitors*" $body]}
ok "…and offers no rows"            {[llength $::voverlay::tasks_rows] == 0}

set ::vapi::fail 1
set body [refill]
ok "a dead gateway says unavailable" {[string match "*unavailable*" $body]}
set ::vapi::fail 0

# ── finished tasks are not shown as running ──────────────────────────
puts "\nfinished work remains inspectable and selection survives refresh"
set ::vapi::tasks [dict create tasks [list \
    [task 1 subagent "done thing" 9 done] \
    [task 2 shell "live thing" 4]] live 1 total_tracked 2]
set body [refill]
ok "the finished one remains inspectable" {[string match "*done thing*" $body]}
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
    ok "task $id (child_session:null) offers no attach" {![attach_offered $id]}
}
ok "…and a real child_session still offers one" {[attach_offered 7]}

puts "\nfinished work remains inspectable and selection survives refresh"
set ::vapi::tasks [dict create tasks [list \
    [task 1 subagent "live audit" 42] \
    [task 2 shell "printf done" 7 done] \
    [task 3 monitor "file: /tmp/build.done" 3 done]] live 1 total_tracked 3]
set body [refill]
ok "a finished shell remains listed" {[string match "*printf done*" $body]}
ok "a finished monitor remains listed" {[string match "*build.done*" $body]}
::voverlay::_tasks_select .ov_tasks 2
ok "a finished shell cannot be cancelled" {[.ov_tasks.bar.cancel instate disabled]}
::voverlay::_tasks_select .ov_tasks 2
::voverlay::_tasks_fill .ov_tasks
ok "selection survives a refresh" {$::voverlay::tasks_selected == 2}
::voverlay::_tasks_select .ov_tasks 3
ok "monitor condition is visible in details" {[string match "*file: /tmp/build.done*" [.ov_tasks.detail.t get 1.0 end]]}

puts "\noutput and cancellation are asynchronous and confirmed"
set ::vapi::async_calls {}
set ::vapi::posts {}
set ::confirm_answer 0
namespace eval ::vmanage {
    proc confirm {w title detail} { return $::confirm_answer }
}
::voverlay::_tasks_select .ov_tasks 2
::voverlay::_tasks_action .ov_tasks.main 2 cancel
ok "declining cancellation sends nothing" {[llength $::vapi::posts] == 0}
set ::confirm_answer 1
::voverlay::_tasks_action .ov_tasks.main 2 cancel
ok "confirming cancellation uses the async cancel route" {
    [lsearch -exact $::vapi::posts "/api/tasks/2/cancel?session=s1"] >= 0
}
::voverlay::_tasks_action .ov_tasks.main 2 output
ok "output uses an async GET" {[llength $::vapi::async_calls] > 0}
ok "output request is scoped to the session" {
    [string match "*/api/tasks/2/output?session=s1" [lindex $::vapi::async_calls end 0]]
}
lassign [lindex $::vapi::async_calls end] path callback
{*}$callback 1 [dict create output "async output line"]
ok "async output lands in the detail pane" {
    [string match "*async output line*" [.ov_tasks.detail.t get 1.0 end]]
}
set before [llength $::vapi::async_calls]
::voverlay::_tasks_fill .ov_tasks
update
{*}$callback 1 [dict create output "stale output"]
ok "a stale output reply cannot replace newer content" {
    [llength $::vapi::async_calls] > $before &&
    ![string match "*stale output*" [.ov_tasks.detail.t get 1.0 end]]
}

puts "\nlong output remains keyboard readable across refresh"
# Scroll fractions are viewport-sized. An unmapped one-pixel detail view
# rounds a 25% scroll to the same fraction as a refresh on some Tk builds.
# Map the real window, not a synthetic scrollbar, for this geometry test.
wm deiconify .ov_tasks
wm geometry .ov_tasks 980x640+0+0
update
set ::vapi::output [dict create id 2 output [string repeat "captured line\n" 240]]
::voverlay::_tasks_select .ov_tasks 2
update
.ov_tasks.detail.t yview moveto 0.25
set before_scroll [lindex [.ov_tasks.detail.t yview] 0]
::voverlay::_tasks_fill .ov_tasks
update
ok "detail scroll survives a refresh" {
    $before_scroll > 0 &&
    abs([lindex [.ov_tasks.detail.t yview] 0] - $before_scroll) < 0.001
}
ok "detail is read-only but focusable" {
    [::vtext::is_readonly .ov_tasks.detail.t] &&
    [.ov_tasks.detail.t cget -state] eq "normal" &&
    [.ov_tasks.detail.t cget -takefocus] == 1
}

puts "\ntaskstest: $passed passed, $failed failed"
exit [expr {$failed > 0 ? 1 : 0}]
