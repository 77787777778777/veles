#!/usr/bin/env tclsh
# Mutation harness for IRC's outbound pacing (2026-10-10: a 9 KB answer
# left the socket in one write and SynIRC quit the bot for Excess Flood).
# Each mutant removes one guard and must turn its named test RED; the
# source is restored after every run. Same verdict rules as
# scripts/mutcheck-2026-10-10-signal-pin.tcl: a timeout-killed run, a
# process that ends without a verdict, and a non-zero exit are all RED.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-10-irc-flood.tcl [a filter]
#   direnv exec . tclsh scripts/mutcheck-2026-10-10-irc-flood.tcl --dry
#
# Not covered here, on purpose: `WriterTask`'s abort and the select arm
# for a failed writer. Both are for a writer stuck on, or failed against,
# a socket whose read side still looks alive, which a stub cannot
# produce; under every test the writer ends by itself when the
# connection's sender drops.
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set CLIENT crates/veles-irc/src/client.rs
set RATE   crates/veles-irc/src/ratelimit.rs
set IRC {-p veles-irc --lib}
set T_WIRE  {client::stub_server::a_long_reply_leaves_the_socket_at_the_buckets_pace}
set T_LAG   {client::stub_server::long_lines_are_paced_by_what_the_server_charges_for_them}
set T_ASK   {client::stub_server::a_long_approval_ask_is_paced_by_the_servers_charge_too}
set T_MODEL {ratelimit::tests::a_reply_paced_by_the_servers_charge_never_queues_there}
set T_DEAR  {ratelimit::tests::a_line_dearer_than_the_budget_still_goes}
# name file find repl target test
set mutations [list \
    [list {the writer rides the read stream again (one burst per handler)} \
        $CLIENT \
        "    let outgoing = conn\n        .client\n        .outgoing()\n        .ok_or_else(|| anyhow::anyhow!(\"irc: the connection's writer was already taken\"))?;\n    let mut writer = WriterTask(tokio::spawn(outgoing));\n" \
        "    let mut writer = WriterTask(tokio::spawn(std::future::pending::<irc::error::Result<()>>())); // MUTATION: Outgoing stays in stream()\n" \
        $IRC $T_WIRE] \
    [list {a line costs a flat penalty, whatever its size (model)} \
        $RATE \
        "        self.penalty_ms * (1 + bytes / self.penalty_bytes) as f64\n" \
        "        \{\n            let _ = bytes;\n            self.penalty_ms // MUTATION: flat charge\n        \}\n" \
        $IRC $T_MODEL] \
    [list {a line costs a flat penalty, whatever its size (wire)} \
        $RATE \
        "        self.penalty_ms * (1 + bytes / self.penalty_bytes) as f64\n" \
        "        \{\n            let _ = bytes;\n            self.penalty_ms // MUTATION: flat charge\n        \}\n" \
        $IRC $T_LAG] \
    [list {the budget runs past the server's ten seconds} \
        $RATE \
        "    pub const BUDGET_MS: u32 = 8_000;\n" \
        "    pub const BUDGET_MS: u32 = 12_000; // MUTATION\n" \
        $IRC $T_MODEL] \
    [list {send_line never consults the server's charge} \
        $CLIENT \
        "        self.server_lag.take_blocking(bytes).await;\n        self.client.send(cmd)?;\n" \
        "        let _ = bytes; // MUTATION: no server charge\n        self.client.send(cmd)?;\n" \
        $IRC $T_LAG] \
    [list {an approval ask skips the server's charge with the bucket} \
        $CLIENT \
        "        if paced \{\n            self.outbound_bucket.take_blocking().await;\n        \}\n        self.server_lag.take_blocking(bytes).await;\n" \
        "        if paced \{\n            self.outbound_bucket.take_blocking().await;\n            self.server_lag.take_blocking(bytes).await; // MUTATION: asks unpaced\n        \}\n" \
        $IRC $T_ASK] \
    [list {a line dearer than the budget waits past the debt} \
        $RATE \
        "from_secs_f64(over.min(self.lag_ms) / 1000.0)" \
        "from_secs_f64(over / 1000.0) /* MUTATION */" \
        $IRC $T_DEAR] \
]
proc run_cmd {argv} {
    set rc 0
    if {[catch {exec timeout -s KILL 600 sh -c "direnv exec . $argv 2>&1" </dev/null} out]} {
        if {[lindex $::errorCode 0] eq "CHILDSTATUS"} {
            set rc [lindex $::errorCode 2]
        } elseif {[lindex $::errorCode 0] eq "CHILDKILLED"} {
            # `timeout -s KILL` kills its whole group, itself included:
            # Tcl reports a child killed by a signal, not an exit status.
            set rc 137
        } else {
            set rc -1
        }
    }
    return [list $rc $out]
}
proc run_test {target filter} {
    lassign [run_cmd "cargo test $target -- $filter"] rc out
    if {[regexp {error\[E[0-9]+\]|could not compile} $out]} { return "NOCOMPILE" }
    if {$rc == 124 || $rc == 137} { puts -nonewline "(killed by the 600 s timeout) "; return "RED" }
    if {[regexp {process didn't exit successfully|\(signal: 6|SIGABRT|\(exit status: [1-9]} $out]} { return "RED" }
    if {![regexp {running ([1-9][0-9]*) tests?} $out]} { return "NO-SUCH-TEST-OR-KILLED" }
    if {[string match "*test result: FAILED*" $out]} { return "RED" }
    if {![string match "*test result: ok*" $out]} {
        puts -nonewline "(the test process ended without a verdict) "
        return "RED"
    }
    return "GREEN"
}
proc apply_and_run {file find repl target filter} {
    set fh [open $file r]
    fconfigure $fh -encoding utf-8
    set orig [read $fh]
    close $fh
    if {[string first $find $orig] < 0} { return PATTERN-MISSING }
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh [string map [list $find $repl] $orig]
    close $fh
    set verdict [catch {run_test $target $filter} r]
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh $orig
    close $fh
    if {$verdict} { return "ERROR: $r" }
    return $r
}
set filter ""
set dry 0
foreach a $argv {
    if {$a eq "--dry"} { set dry 1 } else { set filter $a }
}
set red 0
set total 0
foreach m $mutations {
    lassign $m name file find repl target test
    if {$filter ne "" && [string first $filter $name] < 0} { continue }
    incr total
    if {$dry} {
        set fh [open $file r]
        fconfigure $fh -encoding utf-8
        set src [read $fh]
        close $fh
        set ok [expr {[string first $find $src] >= 0}]
        puts [format "%-78s %s" $name [expr {$ok ? "ok" : "PATTERN-MISSING"}]]
        if {$ok} { incr red }
        continue
    }
    puts -nonewline [format "%-78s " $name]
    flush stdout
    set v [apply_and_run $file $find $repl $target $test]
    puts $v
    if {$v eq "RED"} { incr red }
}
puts "$red/$total [expr {$dry ? "patterns match" : "RED"}]"
exit [expr {$red == $total ? 0 : 1}]
