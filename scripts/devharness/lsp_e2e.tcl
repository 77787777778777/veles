# Boot the REAL desktop against the REAL gateway and drive the LSP path:
# hover, go-to-definition, a tab switch, a root ping-pong, and Shift-hold.
# Reports one line per check, then a tally, then LSP_E2E_DONE.
set PORT [lindex $argv 0]
set H    [lindex $argv 1]

set ::bgerrors {}
proc bgerror {msg} {
    lappend ::bgerrors $msg
    puts stderr "BGERROR: $msg"
    catch { puts stderr $::errorInfo }
}

source [file join $env(VELES_DESKTOP_DIR) main.tcl]

set ::P 0 ; set ::F 0
proc check {name ok} {
    if {[uplevel 1 [list expr $ok]]} {
        puts "  ok    $name" ; incr ::P
    } else {
        puts "  FAIL  $name" ; incr ::F
    }
}

# Spin the event loop for ms milliseconds (lets SSE frames / after
# handlers run) — a blocking sleep would freeze the client.
proc settle {ms} {
    set ::_settle 0
    after $ms { set ::_settle 1 }
    vwait ::_settle
}

# Poll expression `cond` until true or `ms` elapses. Returns 0/1.
proc waitfor {ms cond} {
    set stop [expr {[clock milliseconds] + $ms}]
    while {[clock milliseconds] < $stop} {
        if {[uplevel 1 [list expr $cond]]} { return 1 }
        settle 60
    }
    return [uplevel 1 [list expr $cond]]
}

# Cursor position (0-based, LSP style) of the first match of $needle in
# the current editor buffer, or "" if absent.
proc pos_of {needle} {
    set w [::vdev::editor_widget]
    set idx [$w search -- $needle 1.0 end]
    if {$idx eq ""} { return "" }
    lassign [split $idx .] l c
    return [list [expr {$l - 1}] $c]
}

proc _hv_cb {r e} {
    set ::_hv [expr {$e ne "" ? "ERR:$e" : [::vlsp::render_hover $r]}]
}
proc _df_cb {r e} {
    set ::_df [expr {$e ne "" ? "" : [::vlsp::definition_target $r]}]
}

proc hover_text {lc} {
    if {$lc eq ""} { puts "    (no cursor position found)" ; return "" }
    lassign $lc l c
    set ::_hv __pending__
    ::vlsp::hover_at $l $c ::_hv_cb
    waitfor 6000 { $::_hv ne "__pending__" }
    if {$::_hv eq "__pending__"} { puts "    (hover reply never arrived @ $l,$c)" ; return "" }
    return $::_hv
}

proc def_target {lc} {
    if {$lc eq ""} { return "" }
    lassign $lc l c
    set ::_df __pending__
    ::vlsp::definition_at $l $c ::_df_cb
    waitfor 6000 { $::_df ne "__pending__" }
    return [expr {$::_df eq "__pending__" ? "" : $::_df}]
}

proc open_file {path} {
    set ::vdev::current_file $path
    ::vdev::editor_open
}

# editor_open flips ::vlsp::ready as soon as `initialized` is queued, but
# didOpen rides that message's ACK — so a hover fired the instant ready
# goes 1 can beat the document to the server. Wait for the round trip.
proc await_ready {{ms 25000}} {
    if {![waitfor $ms { $::vlsp::ready }]} { return 0 }
    settle 1800
    return 1
}

# ── run ──────────────────────────────────────────────────────────────
settle 4000
catch { ::vpanes::show dev }
settle 400

set app  [file join $H proj app.tcl]
set plug [file join $H plugins reasoning-summary plugin.tcl]

puts "\n1. a plain project file"
open_file $app
check "the LSP session came up"            {[await_ready]}
check "…rooted at the project (its .git)"   {[string match "*/proj" $::vlsp::root]}
puts "    uri=$::vlsp::uri  hover=[::vlsp::supports hoverProvider] def=[::vlsp::supports definitionProvider]"
set hv [hover_text [pos_of "twice 21"]]
check "hover on ::demo::twice says something" {$hv ne ""}
check "…and it is the proc, not a builtin"    {[string match {*twice*} $hv]}
set tgt [def_target [pos_of "twice 21"]]
check "go-to-definition resolves it"          {$tgt ne ""}
check "…into lib.tcl"                          {[string match {*proj/lib.tcl} [lindex $tgt 0]]}

puts "\n2. a plugin file — a different root, resolved through the stub"
open_file $plug
check "the LSP re-rooted for the plugin" \
    {[await_ready] && [string match "*/plugins" $::vlsp::root]}
set hv [hover_text [pos_of "call_llm"]]
check "hover on ::veles::call_llm resolves"   {$hv ne ""}
check "…and carries the stub's own doc, not set/puts" \
    {[string match -nocase {*summar*} $hv] || [string match {*provider*} $hv]}
set tgt [def_target [pos_of "call_llm"]]
check "go-to-definition on ::veles::call_llm"  {$tgt ne ""}
check "…lands in veles_api.tcl" {[string match {*plugins/veles_api.tcl} [lindex $tgt 0]]}

puts "\n3. switching tabs re-points the server (C1)"
set appid ""
foreach id [::vdev::buf_list] {
    if {[::vdev::buf_path_of $id] eq $app} { set appid $id }
}
check "the project file still has a tab"       {$appid ne ""}
::vdev::buf_show $appid
check "buf_show moved doc_path back to the project file" \
    {[waitfor 12000 { [::vlsp::doc_path] eq $app && $::vlsp::ready }]}
check "…and hover there answers for the project again" \
    {[hover_text [pos_of "twice 21"]] ne ""}

puts "\n4. root ping-pong — no crash, session survives (C2)"
# Let each switch's handshake finish before forcing the next: this is a
# human alt-tabbing between two projects, not a fuzzer. (Hammering faster
# than a handshake completes just churns servers — recoverable, but not
# what we are testing here.)
for {set i 0} {$i < 3} {incr i} {
    open_file $plug
    check "  switch $i -> plugins settled" {[await_ready] && [string match "*/plugins" $::vlsp::root]}
    open_file $app
    check "  switch $i -> proj settled"    {[await_ready] && [string match "*/proj" $::vlsp::root]}
}
check "no bgerrors across the ping-pong"       {[llength $::bgerrors] == 0}
check "the session is still alive"             {$::vlsp::ready}
check "…and hover still works after all that"  {[hover_text [pos_of "twice 21"]] ne ""}

puts "\n5. Shift-hold on a real hover"
set ::vdev::lsp_hover_pos {160 110}
::vlsp::hover_at {*}[pos_of "twice 21"] ::vdev::_hover_result
check "a hover tip appeared"                   {[waitfor 6000 { [winfo exists .vsyn_tip] }]}
event generate . <KeyPress-Shift_L>
settle 150
check "Shift pinned it"                        {$::vdev::tip_pin == 1 && [winfo exists .vsyn_tip]}
event generate . <KeyRelease-Shift_L>
check "Shift release let it go"                {[waitfor 3000 { $::vdev::tip_pin == 0 }]}
catch { ::vdev::_tip_destroy }

puts "\nlsp_e2e: $::P passed, $::F failed"
puts "BGERRORS: [llength $::bgerrors]"
foreach e $::bgerrors { puts "  ! $e" }
puts "LSP_E2E_DONE"
exit [expr {($::F > 0 || [llength $::bgerrors] > 0) ? 1 : 0}]
