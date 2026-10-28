#!/usr/bin/env tclsh
# Mutation harness for R9's Racket host (PLAN-plugins-installer.md): the
# engine (crates/veles-lang-racket) and its Racket-side loop. Each mutant
# removes one guard and must turn its named test RED; the source is
# restored after every run. A mutant that HANGS is killed by the per-run
# timeout, which counts as RED; one that makes the test process exit
# (the `exit` guard) reads RED through cargo's "didn't exit successfully",
# and one whose test process ends with status 0 and no verdict (Racket's
# own exit at boot) is RED too: the first run called that GREEN.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-10-r9-racket.tcl [a filter]
#   direnv exec . tclsh scripts/mutcheck-2026-10-10-r9-racket.tcl --dry   (patterns only)
#
# Matching is by `string first`, never a glob. Every field is
# BRACE-quoted or double-quoted with escaped braces.
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set ENGINE  crates/veles-lang-racket/src/engine.rs
set RUNTIME crates/veles-lang-racket/src/runtime.rkt
set VELES   crates/veles-lang-racket/src/veles.rkt
set RK {-p veles-lang-racket --lib}
set RKB {-p veles-lang-racket --test broken_runtime}
set T_BOOT {a_runtime_that_does_not_read_leaves_the_engine_dead_not_the_process}
set T_SEAM     {engine::tests::a_racket_plugin_registers_and_answers_through_the_seam}
set T_FAILLOAD {engine::tests::a_load_that_raises_or_exits_leaves_nothing_registered}
set T_DEADLINE {engine::tests::a_handler_past_the_deadline_is_killed_and_the_script_keeps_working}
set T_SIGNALS  {engine::tests::the_boot_leaves_the_gateways_signal_handlers_alone}
# name file find repl target test
set mutations [list \
    [list {a plugin may exit the gateway (the exit-handler guard is gone at load)} \
        $RUNTIME \
        {                     [exit-handler no-exit]
                     [current-directory dir])} \
        {                     [current-directory dir]) ; MUTATION: exit-handler left alone} \
        $RK $T_FAILLOAD] \
    [list {a failed load keeps its namespace (no custodian shutdown, no removal)} \
        $RUNTIME \
        {         (custodian-shutdown-all cust)
         (hash-remove! scripts name)
         (reply-err id (cdr outcome))]))} \
        {         ;; MUTATION: the failed load's state stays
         (reply-err id (cdr outcome))]))} \
        $RK $T_FAILLOAD] \
    [list {a handler past the deadline is not killed} \
        $RUNTIME \
        {          (kill-thread t)
          (reply-err id (format "racket:~a: the handler ran ~a s without answering; killed"} \
        {          (reply-err id (format "racket:~a: the handler ran ~a s without answering; killed"} \
        $RK $T_DEADLINE] \
    [list {(void) from a hook is a verdict, not no opinion} \
        $RUNTIME \
        {  (cond [(void? v) "null"]
        [(jsexpr? v) (jsexpr->string v)]} \
        {  (cond [(jsexpr? v) (jsexpr->string v)] ; MUTATION: (void) prints} \
        $RK $T_SEAM] \
    [list {an unloaded script answers a late invoke as if loaded} \
        $RUNTIME \
        {    [(not s) (reply-err id (format "racket:~a: not loaded" name))]
    [(not handler) (reply-err id (format "no callback ~a in racket:~a" cb name))]} \
        {    [(not handler) (reply-err id (format "no callback ~a in racket:~a" cb name))]} \
        $RK $T_SEAM] \
    [list {a host reply is never delivered to the waiting handler} \
        $RUNTIME \
        {     (when ch (async-channel-put ch m))]} \
        {     (void ch)] ; MUTATION: the reply is dropped} \
        $RK $T_SEAM] \
    [list {the mailbox push does not wake the loop} \
        $ENGINE \
        {        unsafe { libc::write(self.wake_w, b.as_ptr() as *const libc::c_void, 1) };} \
        {        let _ = b; // MUTATION: no wake byte} \
        $RK $T_SEAM] \
    [list {a load that failed stays in the host's script table} \
        $ENGINE \
        "                Err(AskError::Replied(_)) => \{\n                    // The load raised (or tried to exit): the Racket side\n                    // shut the script's custodian and dropped its entry\n                    // before it answered; only the host-call door closes here.\n                    self.scripts\n                        .lock()\n                        .unwrap_or_else(|p| p.into_inner())\n                        .remove(&script.name);\n                \}" \
        "                Err(AskError::Replied(_)) => \{\} // MUTATION: the failed load stays listed" \
        $RK $T_FAILLOAD] \
    [list {the boot's signal dispositions are not put back} \
        $ENGINE \
        {    restore_signal_dispositions(&before);} \
        {    let _ = &before; // MUTATION: Racket keeps the handlers} \
        $RK $T_SIGNALS] \
    [list {an error at boot exits the gateway (boot.rkt's handler is gone)} \
        $ENGINE \
        {(with-handlers ([(lambda (e) #t)
                 (lambda (e) (report (if (exn? e) (exn-message e) (format "raised: ~e" e))))])} \
        {(let () ; MUTATION: no handler at boot, Racket's own exit} \
        $RKB $T_BOOT] \
    [list {the runtime's last words are dropped (a dead engine says only that the loop returned)} \
        $ENGINE \
        {        Ok(()) => engine
            .last_words
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_else(|| "the Racket loop returned".to_string()),} \
        {        Ok(()) => "the Racket loop returned".to_string(), // MUTATION: the last words are dropped} \
        $RKB $T_BOOT] \
]
proc run_cmd {argv} {
    # `timeout` bounds a mutant that hangs (a load whose reply never
    # comes waits 90 s in the engine, then the test fails; a loop that
    # never wakes waits the same); 600 s is generous for one test. The
    # -k is for a test process that ignores SIGTERM, as the signal
    # mutant's does: without it the orphan kept this pipe open and the
    # harness waited eight hours for EOF (2026-10-10).
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
