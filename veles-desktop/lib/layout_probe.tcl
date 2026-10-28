# Runs INSIDE the booted app (sourced by main.tcl when VELES_LAYOUT_PROBE=1).
# Asserts the pane layout + composer distinction, prints PASS/FAIL lines.
proc ::probe {} {
    set fails 0
    # 1. panedwindow has sidebar + chatwrap docked.
    set panes [.mid panes]
    if {[lsearch -exact $panes .sidebar] < 0 || [lsearch -exact $panes .mid.chatwrap] < 0} {
        puts "FAIL: docked panes = $panes"; incr fails
    } else { puts "ok: sidebar + chatwrap docked" }
    # 2. sidebar has a real width (>100px).
    set sw [winfo width .sidebar]
    if {$sw < 100} { puts "FAIL: sidebar width $sw"; incr fails } else { puts "ok: sidebar width $sw" }
    # 3. chatwrap fills the rest.
    set cw [winfo width .mid.chatwrap]
    set tot [winfo width .mid]
    if {$cw < [expr {$tot / 2}]} { puts "FAIL: chatwrap $cw of $tot"; incr fails } else { puts "ok: chatwrap $cw / $tot" }
    # 4. composer bg differs from transcript bg.
    update
    set in_bg [.mid.chatwrap.chat.input cget -background]
    set log_bg [.mid.chatwrap.chat.log cget -background]
    if {$in_bg eq $log_bg} { puts "FAIL: composer bg $in_bg == transcript $log_bg"; incr fails } else { puts "ok: composer $in_bg vs transcript $log_bg" }
    # 5. undock/redock cycle of the panes shell.
    ::vpanes::show files
    update
    ::vdock::undock .panes
    update
    if {![winfo exists .float_panes]} { puts "FAIL: no float toplevel"; incr fails } else { puts "ok: undocked to .float_panes" }
    if {[lsearch -exact [.mid panes] .panes] >= 0} { puts "FAIL: still docked"; incr fails } else { puts "ok: removed from pane" }
    ::vdock::redock .panes
    update
    if {[lsearch -exact [.mid panes] .panes] < 0} { puts "FAIL: redock failed"; incr fails } else { puts "ok: redocked" }
    # 5. Dev pane: tab exists, REPL/editor/inspector widgets built.
    ::vpanes::show dev
    update
    set R $::vpanes::R
    if {![winfo exists $R.tabs.dev]} { puts "FAIL: dev tab missing"; incr fails } else { puts "ok: dev tab exists" }
    foreach w "$R.dev.repl.in $R.dev.repl.out $R.dev.edit.body $R.dev.dbg.out" {
        if {![winfo exists $w]} { puts "FAIL: dev widget $w"; incr fails } else { puts "ok: dev widget $w" }
    }
    # REPL default target is the APP interpreter (the guts, not the
    # plugin sandbox): define a proc in the live window, call it back.
    ::vdev::repl_eval "proc ::veles_probe_greet {} { return GUTS_OK }"
    update
    ::vdev::repl_eval "::veles_probe_greet"
    update
    after 300
    set log [$R.dev.repl.out get 1.0 end]
    if {[string match "*GUTS_OK*" $log]} {
        puts "ok: app-target REPL self-modified the live window"
    } else {
        puts "FAIL: app-target REPL got: $log"; incr fails
    }
    # And gw target still round-trips (plugin sandbox remains reachable).
    ::vdev::set_target gw
    ::vdev::repl_eval "expr {6*7}"
    update
    after 500
    set log2 [$R.dev.repl.out get 1.0 end]
    if {[string match "*42*" $log2]} { puts "ok: gw-target REPL 6*7 -> 42" } else { puts "FAIL: gw REPL: $log2"; incr fails }
    ::vdev::set_target app

    if {$fails == 0} { puts "LAYOUT_PROBE_ALL_PASS" } else { puts "LAYOUT_PROBE_FAILS=$fails" }
    exit $fails
}
after 2500 ::probe
