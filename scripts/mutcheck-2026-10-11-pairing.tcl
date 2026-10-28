#!/usr/bin/env tclsh
# Mutation harness for QR pool pairing (PLAN-sessions-and-phone.md P2d,
# USER decisions D8-D10, 2026-10-11): only the arbiter invites, an invite
# is single use and brief, its bearer opens nothing else, only the
# arbiter introduces members, the hub's root helper takes no address a
# peer holds, and the phone asks before it joins. Each mutant removes one
# guard and must turn its named test RED; the source is restored after
# every run.
#
# What each mutant unlocks was checked first: every config write lands
# in a scratch home (unit tests, the three-gateway e2e), the WireGuard
# helper runs against a fake `wg`, and the phone page's camera and
# gateway are stubbed.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-pairing.tcl [--dry] [a filter]

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set POOL  crates/veles-core/src/pool.rs
set A2A   crates/veles-frontends/src/a2a_server.rs
set PH    veles-desktop/lib/phone.tcl
set WG    deploy/vps/wg/veles-wg-peers.sh
set LIB {-p veles-core --lib}
set E2E {-p veles-cli --test pool_join}
set PT  veles-desktop/lib/phonetest.tcl

set mutations [list \
    [list "invite: any node invites" \
        $POOL \
        "        Some(a) if a == me.id || me.id.starts_with(a) => \{\}" \
        "        Some(_) => \{\} // MUTATION" \
        $LIB pool::tests::an_invite_is_the_arbiters_once_and_briefly] \
    [list "invite: a used or expired bearer still opens the join" \
        $POOL \
        "        .find(|p| p.to_arbiter_sha == sha && p.redeemed_at.is_none() && p.expires > t)" \
        "        .find(|p| p.to_arbiter_sha == sha) // MUTATION" \
        $LIB pool::tests::an_invite_is_the_arbiters_once_and_briefly] \
    [list "redeem: used twice" \
        $POOL \
        "        if row.redeemed_at.is_some() \{" \
        "        if false \{ // MUTATION" \
        $LIB pool::tests::a_redeem_checks_the_invite_and_claims_it_once] \
    [list "redeem: expired is fine" \
        $POOL \
        "        if row.expires <= t \{" \
        "        if false \{ // MUTATION" \
        $LIB pool::tests::a_redeem_checks_the_invite_and_claims_it_once] \
    [list "redeem: any secret, any bearer" \
        $POOL \
        "        if row.secret_sha != sha_hex(&ask.secret) || row.to_arbiter_sha != sha_hex(bearer) \{" \
        "        if false \{ // MUTATION" \
        $LIB pool::tests::a_redeem_checks_the_invite_and_claims_it_once] \
    [list "member: anyone introduces" \
        $POOL \
        "    if link.name != peer \{" \
        "    if false \{ // MUTATION" \
        $LIB pool::tests::a_member_takes_introductions_only_from_its_arbiter] \
    [list "member: any arbiter id" \
        $POOL \
        "    if ask.arbiter != arbiter && !ask.arbiter.starts_with(arbiter) \{" \
        "    if false \{ // MUTATION" \
        $LIB pool::tests::a_member_takes_introductions_only_from_its_arbiter] \
    [list "tokens: a link's old token stays beside the new" \
        $POOL \
        "        .filter(|(n, _)| !links.iter().any(|l| &l.name == n))" \
        "        .filter(|_| true) // MUTATION" \
        $LIB pool::tests::peer_tokens_merge_by_name] \
    [list "wireguard: a taken address is handed out" \
        $POOL \
        "        if !taken.contains(&ip) \{" \
        "        if true \{ // MUTATION" \
        $LIB pool::tests::wireguard_keys_and_addresses_are_what_wg_would_make] \
    [list "wireguard: the ledger's leases are forgotten" \
        $POOL \
        "        if let Some(Ok(a)) = r.wg.as_ref().map(|w| w.address.parse()) \{" \
        "        if let Some(Ok(a)) = None::<Option<Result<Ipv4Addr, std::net::AddrParseError>>>.flatten() \{ // MUTATION" \
        $LIB pool::tests::the_hub_mints_a_tunnel_with_the_invite] \
    [list "a2a: introductions are never handled" \
        $A2A \
        "        if veles_core::pool::is_member(&raw_text) \{" \
        "        if false \{ // MUTATION" \
        $E2E a_fresh_node_joins_the_whole_pool_from_one_invite] \
    [list "a2a: an invite's bearer is never let through" \
        $A2A \
        "        if let Some(invited) = veles_core::pool::invite_bearer(auth.as_deref()) \{" \
        "        if let Some(invited) = None::<String> \{ // MUTATION" \
        $E2E a_fresh_node_joins_the_whole_pool_from_one_invite] \
    [list "helper: a held address is taken" \
        $WG \
        "    if held | grep -qx \"\$addr\"; then" \
        "    if false; then" \
        SH scripts/test-wg-peers.sh] \
    [list "helper: any key" \
        $WG \
        "    if ! valid_key \"\$key\"; then echo \"skip \$name: not a WireGuard public key\" >&2; continue; fi" \
        "    : # MUTATION" \
        SH scripts/test-wg-peers.sh] \
    [list "phone: joins without the yes" \
        $PH \
        "    pack \$w.confirm -fill x -padx 10 -after \$w.status" \
        "    join_now \$w" \
        TCL $PT] \
    [list "phone: an expired code is offered" \
        $PH \
        "    if \{\[dict get \$r expires_in\] <= 0\} \{" \
        "    if \{0\} \{" \
        TCL $PT] \
    [list "phone: the camera opens unasked" \
        $PH \
        "    if \{1 ni \[borg checkpermission android.permission.CAMERA 0\]\} \{" \
        "    if \{0\} \{" \
        TCL $PT] \
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
puts "mutcheck pool pairing 2026-10-11: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
