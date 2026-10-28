#!/usr/bin/env tclsh
# Mutation harness for the device bridge (PLAN-sessions-and-phone.md,
# round P1, 2026-10-11): a window announces what it can do, the agent's
# `device` tool calls it through the gateway, the loopback tools carry the
# API's token, links and the Terminal pane's Run line work on a phone.
# Each mutant removes one guard and must turn its named test RED; the
# source is restored after every run.
#
# Not a mutant here, on purpose: the stream's detach on close
# (desktop_api.rs DeviceAttach). A gone window is caught twice — the
# detach drops its sender, and every read checks `is_closed()` — so
# removing either one alone changes nothing observable (measured: the
# detach mutant stayed GREEN against device_bridge.rs). The pair is what
# device_bridge.rs pins.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-devices.tcl [--dry] [a filter]

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set DEV   crates/veles-core/src/devices.rs
set DTOOL crates/veles-core/src/desktop_tools.rs
set DAPI  crates/veles-frontends/src/desktop_api.rs
set ACL   crates/veles-core/src/acl.rs
set CLI   veles-desktop/lib/device.tcl
set AND   veles-desktop/lib/device_android.tcl
set MD    veles-desktop/lib/markdown.tcl
set PANES veles-desktop/lib/panes.tcl
set LIB {-p veles-core --lib}
set E2E {-p veles-cli --test device_bridge}

set mutations [list \
    [list "devices: several devices answer an unnamed call" \
        $DEV \
        "        if names.len() > 1 \{" \
        "        if false && names.len() > 1 \{ // MUTATION" \
        $LIB devices::tests::ambiguity_absence_and_silence_are_named] \
    [list "devices: a device not listening is called anyway" \
        $DEV \
        "        let Some(tx) = c.tx.clone().filter(|t| !t.is_closed()) else \{" \
        "        let Some(tx) = c.tx.clone() else \{ // MUTATION" \
        $LIB devices::tests::ambiguity_absence_and_silence_are_named] \
    [list "acl: the device tool reaches a session ungated" \
        $ACL \
        "        \"device\" => All," \
        "        // MUTATION: unclassified" \
        $LIB tools::tests::every_registered_tool_is_acl_classified] \
    [list "desktop tools: no token on the loopback again" \
        $DTOOL \
        "        req.bearer_auth(token.trim())" \
        "        \{ let _ = token; req \} // MUTATION" \
        $E2E the_agent_lists_and_calls_what_a_window_offers] \
    [list "window: a provider gets its args wrapped in a list" \
        $CLI \
        "    if \{\[catch \{ \{*\}\$proc \$a \} result\]\} \{" \
        "    if \{\[catch \{ \{*\}\$proc \[list \$a\] \} result\]\} \{" \
        TCL veles-desktop/lib/devicetest.tcl] \
    [list "window: the stream is not passive" \
        $CLI \
        "    set ::vapi::passive 1
    set failed \[catch \{ ::vapi::sse_open \$path ::vdevice::_event \} t\]" \
        "    set ::vapi::passive 0
    set failed \[catch \{ ::vapi::sse_open \$path ::vdevice::_event \} t\]" \
        TCL veles-desktop/lib/devicetest.tcl] \
    [list "phone: open fires any scheme" \
        $AND \
        "    if \{!\[regexp \{^(https?|geo|tel|mailto|sms|market):\} \$uri\]\} \{" \
        "    if \{0 && !\[regexp \{^(https?|geo|tel|mailto|sms|market):\} \$uri\]\} \{" \
        TCL veles-desktop/lib/devicetest.tcl] \
    [list "links: a phone tries xdg-open again" \
        $MD \
        "    if \{\[info commands ::borg\] ne \"\"\} \{" \
        "    if \{0 && \[info commands ::borg\] ne \"\"\} \{" \
        TCL veles-desktop/lib/mdtest.tcl] \
    [list "terminal: the Run line names a shell that is not there" \
        $PANES \
        "    foreach sh \{bash sh\} \{" \
        "    foreach sh \{\} \{" \
        TCL veles-desktop/lib/panetest.tcl] \
]

proc run_cmd {argv} {
    set f [open "| direnv exec . $argv </dev/null 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    catch { close $f }
    return $out
}

# A private Xvfb for the TCL target, started on first use.
set ::xvfb ""
proc xvfb_display {} {
    if {$::xvfb ne ""} { return $::xvfb }
    for {set n 171} {$n < 199} {incr n} {
        if {![file exists /tmp/.X$n-lock]} { break }
    }
    set ::xvfb_pid [exec Xvfb :$n -screen 0 1024x768x24 >& /dev/null &]
    for {set i 0} {$i < 100 && ![file exists /tmp/.X$n-lock]} {incr i} { after 100 }
    set ::xvfb :$n
    return $::xvfb
}

proc run_tcl_suite {suite} {
    set f [open "| env DISPLAY=[xvfb_display] XVFB_RUNNING=1 direnv exec . wish $suite </dev/null 2>@1" r]
    fconfigure $f -encoding utf-8
    set out ""
    catch { set out [read $f] }
    set rc [catch { close $f }]
    if {![regexp {([0-9]+) passed, ([0-9]+) failed} $out -> p fl]} {
        # No summary: the suite died before its last line. That is RED
        # only when it died of a Tcl error the mutant can explain (a
        # dialog the mutated code destroyed early, a missing window) —
        # never for a suite that could not start at all.
        set log [file join [expr {[info exists ::env(TMPDIR)] ? $::env(TMPDIR) : "/tmp"}] mutcheck-tcl-last.log]
        catch { set fh [open $log w]; puts $fh $out; close $fh }
        if {$rc && [regexp {bad window path name|invalid command name|can't read|while executing} $out]} {
            return "RED"
        }
        return "NO-SUCH-TEST (output in $log)"
    }
    return [expr {($fl > 0 || $rc) ? "RED" : "GREEN"}]
}

proc run_test {target filter} {
    if {$target eq "TCL"} { return [run_tcl_suite $filter] }
    set out [run_cmd "cargo test $target $filter"]
    if {[regexp {error\[E[0-9]+\]|could not compile} $out]} { return "NOCOMPILE" }
    if {[regexp {process didn't exit successfully|\(signal: 6|SIGABRT} $out]} { return "RED" }
    if {![regexp {running ([1-9][0-9]*) tests?} $out]} { return "NO-SUCH-TEST" }
    return [expr {[string match "*test result: FAILED*" $out] ? "RED" : "GREEN"}]
}

proc read_file {file} {
    set fh [open $file r]
    fconfigure $fh -encoding utf-8 -translation binary
    set s [read $fh]
    close $fh
    return [encoding convertfrom utf-8 $s]
}

proc write_file {file s} {
    set fh [open $file w]
    fconfigure $fh -translation binary
    puts -nonewline $fh [encoding convertto utf-8 $s]
    close $fh
}

proc apply_and_run {file find repl target filter} {
    set orig [read_file $file]
    if {[string first $find $orig] < 0} { return PATTERN-MISSING }
    if {$::dry} { return FOUND }
    write_file $file [string map [list $find $repl] $orig]
    set verdict [catch {run_test $target $filter} r]
    write_file $file $orig
    if {$verdict} { return "ERROR: $r" }
    return $r
}

set dry 0
if {[lindex $argv 0] eq "--dry"} { set dry 1; set argv [lrange $argv 1 end] }
set filter [lindex $argv 0]
set verified 0
set failures 0
foreach m $mutations {
    lassign $m name file find repl target test
    if {$filter ne "" && [string first $filter $name] < 0} { continue }
    puts -nonewline "[format %-72s $name] "
    flush stdout
    set r [apply_and_run $file $find $repl $target $test]
    puts $r
    if {$r eq "RED" || ($dry && $r eq "FOUND")} { incr verified } else { incr failures }
}
if {[info exists ::xvfb_pid]} { catch { exec kill $::xvfb_pid } }
puts ""
puts "mutcheck device bridge 2026-10-11: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
