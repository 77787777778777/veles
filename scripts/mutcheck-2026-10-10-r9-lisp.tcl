#!/usr/bin/env tclsh
# Mutation harness for R9's Common Lisp host (PLAN-plugins-installer.md):
# the engine (crates/veles-lang-lisp) and its Lisp-side loop. Each mutant
# removes one guard and must turn its named test RED; the source is
# restored after every run. Same verdict rules as
# scripts/mutcheck-2026-10-10-r9-racket.tcl: a timeout-killed run (300 s
# here: an armed atexit hangs the test process at its exit), a process
# that ends without a verdict, and a non-zero exit are all RED. A Lisp
# mutant must keep the brackets balanced: a runtime that does not read
# turns its test red for the wrong reason, and is reported BROKEN (the
# first run's unload mutant closed one bracket too many and read RED).
# The timeout is a SIGKILL to the whole group: the tests pin tokio's
# SIGTERM handler, so a hung test process outlives a SIGTERM, keeps the
# pipe open, and `timeout -k` never fires once its direct child is gone.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-10-r9-lisp.tcl [a filter]
#   direnv exec . tclsh scripts/mutcheck-2026-10-10-r9-lisp.tcl --dry
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set ENGINE  crates/veles-lang-lisp/src/engine.rs
set RUNTIME crates/veles-lang-lisp/src/runtime.lisp
set LP  {-p veles-lang-lisp --lib}
set LPB {-p veles-lang-lisp --test broken_runtime}
set T_SEAM    {engine::tests::a_lisp_plugin_registers_and_answers_through_the_seam}
set T_FAIL    {engine::tests::a_load_that_raises_or_exits_leaves_nothing_registered}
set T_DEAD    {engine::tests::a_handler_past_the_deadline_is_killed_and_the_script_keeps_working}
set T_SIGNALS {engine::tests::the_boot_leaves_the_gateways_signal_handlers_alone}
set T_WILD    {engine::tests::a_runaway_script_is_a_lisp_error_never_a_dead_gateway}
set T_UNLOAD  {engine::tests::an_unload_kills_the_scripts_threads_and_deletes_its_package}
set T_BOOT    {a_runtime_that_does_not_load_leaves_the_engine_dead_not_the_process}
# name file find repl target test
set mutations [list \
    [list {ECL keeps its SIGSEGV handler (a fault on a Rust thread would end it silently)} \
        $ENGINE \
        {    (ffi::opt::TRAP_SIGSEGV, 0),} \
        {    (ffi::opt::TRAP_SIGSEGV, 1), // MUTATION: ECL traps SIGSEGV} \
        $LP $T_SIGNALS] \
    [list {ECL's threads keep glibc's default stack (its overflow check misses)} \
        $ENGINE \
        {        let rc = ffi::pthread_setattr_default_np(&attr);} \
        {        let rc = { let _ = &attr; 0 }; // MUTATION: default stacks} \
        $LP $T_WILD] \
    [list {ECL's atexit cl_shutdown stays armed (the process never exits)} \
        $ENGINE \
        {    unsafe { libc::atexit(disarm_ecl_shutdown) };} \
        {    let _ = disarm_ecl_shutdown; // MUTATION: armed} \
        $LP $T_SEAM] \
    [list {an unhandled error in a script's thread reaches ECL's debugger} \
        $RUNTIME \
        {  (setf *debugger-hook* #'unhandled)
  (setf ext:*invoke-debugger-hook* #'unhandled)} \
        {  ;; MUTATION: no debugger hooks} \
        $LP $T_WILD] \
    [list {a plugin may exit the gateway (ext:exit and ext:quit left alone)} \
        $RUNTIME \
        {  (setf (fdefinition 'ext:exit) #'no-exit)
  (setf (fdefinition 'ext:quit) #'no-exit)} \
        {  ;; MUTATION: exit and quit left alone} \
        $LP $T_FAIL] \
    [list {the Rust side puts raw UTF-8 on the wire} \
        $ENGINE \
        "    if text.is_ascii() \{" \
        "    if true \{ // MUTATION: raw UTF-8" \
        $LP $T_SEAM] \
    [list {the Lisp side writes raw characters (no escapes past 127)} \
        $RUNTIME \
        {                 ((< code 128) (write-char c out))} \
        {                 ((< code #x110000) (write-char c out)) ; MUTATION: raw} \
        $LP $T_SEAM] \
    [list {an unload leaves the script's threads running} \
        $RUNTIME \
        {        (ignore-errors (mp:process-kill p))))} \
        {        (identity p))) ; MUTATION: no kill} \
        $LP $T_UNLOAD] \
    [list {a handler past the deadline is not killed} \
        $RUNTIME \
        {                 (ignore-errors (mp:process-kill p))
                 (reply-err id (format nil "lisp:~a: the handler ran} \
        {                 (identity p) ; MUTATION: no kill
                 (reply-err id (format nil "lisp:~a: the handler ran} \
        $LP $T_DEAD] \
    [list {a failed load stays in the Lisp side's table} \
        $RUNTIME \
        {                 (drop-script s)
                 (remhash name *scripts*)
                 (reply-err id (cdr outcome))} \
        {                 ;; MUTATION: the failed load stays
                 (reply-err id (cdr outcome))} \
        $LP $T_FAIL] \
    [list {a failed load stays in the host's script table} \
        $ENGINE \
        {                Err(AskError::Replied(_)) => {
                    // The load raised (or tried to exit): the Lisp side
                    // killed the script's threads and deleted its packages
                    // before it answered; only the host-call door closes here.
                    self.scripts
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&script.name);
                }} \
        {                Err(AskError::Replied(_)) => {} // MUTATION: the failed load stays listed} \
        $LP $T_FAIL] \
    [list {the boot's last words are dropped (a dead engine says only that the loop returned)} \
        $ENGINE \
        {             (veles-sys:%dead} \
        {             (identity ; MUTATION: no last words
} \
        $LPB $T_BOOT] \
]
proc run_cmd {argv} {
    set rc 0
    if {[catch {exec timeout -s KILL 300 sh -c "direnv exec . $argv 2>&1" </dev/null} out]} {
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
    if {[string first "Reader error in file" $out] >= 0} { return "BROKEN (the runtime does not read)" }
    if {$rc == 137} { puts -nonewline "(killed by the 300 s timeout) "; return "RED" }
    if {$rc == 124} { puts -nonewline "(killed by the 300 s timeout) "; return "RED" }
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
