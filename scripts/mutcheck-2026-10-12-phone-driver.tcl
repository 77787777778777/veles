#!/usr/bin/env tclsh
# Mutation harness for the agent's hands on the phone
# (PLAN-sessions-and-phone.md A3, round P5; USER decision D4): on a phone
# only the owner drives, the gateway's token goes only to a loopback
# driver, the loopback CONNECT proxy for git answers only its token and
# only CONNECT, on loopback only, and the Phone page shows the driver's
# row only where the build has the driver and opens Android's switch
# for it. Each mutant removes one guard and must turn its named test RED;
# the source is restored after every run.
#
# Not here: the Java driver (AccessibilityDriver: its token check, Stop,
# the capture and the actions) and the store bundle's manifest cut
# (flake.nix velesAab). The emulator smoke drives those
# (scripts/android-emulator-smoke.sh, the P5 section); the bundle's
# build fails by itself on a declared driver.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-12-phone-driver.tcl [--dry] [a filter]

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set CU    crates/veles-core/src/computeruse.rs
set CP    crates/veles-core/src/connect_proxy.rs
set PT    veles-desktop/lib/phone.tcl
set LIB {-p veles-core --lib}
set PTEST veles-desktop/lib/phonetest.tcl

set mutations [list \
    [list "anyone drives a phone" \
        $CU \
        "        Some(p) => p.role == crate::acl::Role::Owner," \
        "        Some(_) => true, // MUTATION" \
        $LIB computeruse::tests::only_the_owner_drives_a_phone] \
    [list "the token goes to any host" \
        $CU \
        "    matches!(host, \"127.0.0.1\" | \"localhost\" | \"::1\")" \
        "    !host.is_empty() // MUTATION" \
        $LIB computeruse::tests::the_token_goes_only_to_loopback] \
    [list "a long capture is cut inside a character" \
        $CU \
        "    format!(\"\{\}\\n\[...truncated\]\", &text\[..text.floor_char_boundary(max)\])" \
        "    format!(\"\{\}\\n\[...truncated\]\", &text\[..max\]) // MUTATION" \
        $LIB computeruse::tests::a_long_capture_is_cut_on_a_character] \
    [list "the proxy answers without the token" \
        $CP \
        "    if !authorized \{" \
        "    if false \{ // MUTATION" \
        $LIB connect_proxy::tests::only_an_authorized_connect_is_answered] \
    [list "the proxy forwards a plain GET" \
        $CP \
        "    if method != \"CONNECT\" \{" \
        "    if false \{ // MUTATION" \
        $LIB connect_proxy::tests::only_an_authorized_connect_is_answered] \
    [list "the proxy listens beyond loopback" \
        $CP \
        "    if !addr.ip().is_loopback() || token.len() < 16 \{" \
        "    if token.len() < 16 \{ // MUTATION" \
        $LIB connect_proxy::tests::the_spec_is_loopback_with_a_token] \
    [list "the proxy tunnels nothing" \
        $CP \
        "    tokio::io::copy_bidirectional(&mut sock, &mut up).await?;" \
        "    let _ = (&mut sock, &mut up); // MUTATION" \
        $LIB connect_proxy::tests::a_tunnel_carries_bytes_both_ways] \
    [list "Phone page: a driver row where the build has none" \
        $PT \
        "        pack forget \$dr" \
        "        # MUTATION" \
        TCL $PTEST] \
    [list "Phone page: the driver button opens the wrong switch" \
        $PT \
        "            set driver \$pkg/\$pkg.AccessibilityDriver" \
        "            set driver \$pkg" \
        TCL $PTEST] \
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
    if {$target eq "SH"} {
        set rc [catch { exec env TMPDIR=/dev/shm bash $filter </dev/null >& /dev/null }]
        return [expr {$rc ? "RED" : "GREEN"}]
    }
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
puts "mutcheck phone driver 2026-10-12: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
