#!/usr/bin/env tclsh
# Mutation harness for the person's apps (PLAN-sessions-and-phone.md A4,
# round P3; USER decisions D5, D6): an app's files stay inside it, a wider
# declaration asks again, unknown capabilities are refused, each app's
# store is its own, the gateway refuses what an app never declared, an app
# asks as itself, and in the window the person is asked, the interpreter
# is safe, and every veles:: call is checked. Each mutant removes one
# guard and must turn its named test RED; the source is restored after
# every run.
#
# Not a mutant here, on purpose: removing the runaway-loop time limit.
# Without it appstest's loop never returns, and this harness has no
# per-suite timeout, so the run would hang instead of going red.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-12-apps.tcl [--dry] [a filter]

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set APPS  crates/veles-core/src/apps.rs
set DAPI  crates/veles-frontends/src/desktop_api.rs
set ACL   crates/veles-core/src/acl.rs
set AT    veles-desktop/lib/apps.tcl
set AND   veles-desktop/lib/android.tcl
set LIB {-p veles-core --lib}
set FRONT {-p veles-frontends --lib}
set E2E {-p veles-cli --test app_ask}
set ATEST veles-desktop/lib/appstest.tcl

set mutations [list \
    [list "apps: a relative path climbs out" \
        $APPS \
        "            .any(|c| !matches!(c, std::path::Component::Normal(_)))" \
        "            .any(|_| false) // MUTATION" \
        $LIB apps::tests::names_and_capabilities] \
    [list "apps: a symlink is read" \
        $APPS \
        "            if !ft.is_file() \{
                continue;
            \}" \
        "            if false \{ // MUTATION
                continue;
            \}" \
        $LIB apps::tests::an_app_is_made_listed_and_read] \
    [list "apps: the grant key ignores the declaration" \
        $APPS \
        "        grant_key: sha_hex(&format!(\"\{name\}\\n\{\}\", caps.join(\"\\n\"))),"  \
        "        grant_key: sha_hex(name), // MUTATION" \
        $LIB apps::tests::an_app_is_made_listed_and_read] \
    [list "apps: an unknown capability is kept" \
        $APPS \
        "        if known_capability(&c) \{" \
        "        if true \{ // MUTATION" \
        $LIB apps::tests::an_app_is_made_listed_and_read] \
    [list "apps: one store for every app" \
        $APPS \
        "        .join(format!(\"\{name\}.json\"))" \
        "        .join(\"shared.json\") // MUTATION" \
        $LIB apps::tests::an_apps_store_is_its_own] \
    [list "gateway: an undeclared capability is served" \
        $DAPI \
        "    if !info.capabilities.iter().any(|c| c == cap) \{" \
        "    if false \{ // MUTATION" \
        $FRONT desktop_api::tests::app_routes_list_read_and_keep_only_what_was_declared] \
    [list "gateway: an app asks as the owner" \
        $DAPI \
        "    match run_turn_as(&srv, &agent, &sid, app_principal(&name), \"app\", &text).await \{" \
        "    match run_turn_as(&srv, &agent, &sid, veles_core::acl::Principal::resolve(None, &veles_core::acl::AclConfig::default()), \"app\", &text).await \{ // MUTATION" \
        $E2E an_app_asks_as_itself_and_only_if_it_declared_ask] \
    [list "apps: the tool writes through a symlink" \
        $APPS \
        "        if !m.file_type().is_file() \{
            bail!(\"\{\} is not a plain file\", path.display());" \
        "        if false \{ // MUTATION
            bail!(\"\{\} is not a plain file\", path.display());" \
        $LIB apps::tests::the_tool_writes_and_edits_only_inside_the_app] \
    [list "apps: an app.toml lands unchecked" \
        $APPS \
        "    if rel == \"app.toml\" \{" \
        "    if false \{ // MUTATION" \
        $LIB apps::tests::the_tool_writes_and_edits_only_inside_the_app] \
    [list "acl: the app tool reaches a session ungated" \
        $ACL \
        "        \"app\" => All," \
        "        // MUTATION: unclassified" \
        $LIB tools::tests::every_registered_tool_is_acl_classified] \
    [list "window: no question before running" \
        $AT \
        "    if \{!\[ask_grant \$info \$answer\]\} \{" \
        "    if \{0\} \{" \
        TCL $ATEST] \
    [list "window: an unsafe interpreter" \
        $AT \
        "    set s \[::safe::interpCreate\]" \
        "    set s \[interp create\]" \
        TCL $ATEST] \
    [list "window: veles:: calls unchecked" \
        $AT \
        "    if \{!\[_allowed \$name \$cap\]\} \{" \
        "    if \{0\} \{" \
        TCL $ATEST] \
    [list "window: the asked grant forgotten" \
        $AT \
        "    expr \{\[dict exists \$g \$name\] && \[dict get \$g \$name\] eq \[dict get \$info grant_key\]\}" \
        "    expr \{\[dict exists \$g \$name\]\}" \
        TCL $ATEST] \
    [list "shortcut: any text names an app" \
        $AND \
        "    if \{\[regexp \{^veles-app://(\[a-z0-9\]\[a-z0-9-\]\{0,31\})/?\$\} \$uri -> app\]\} \{" \
        "    if \{\[regexp \{^veles-app://(.*)\$\} \$uri -> app\]\} \{" \
        TCL veles-desktop/lib/androidtest.tcl] \
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
puts "mutcheck apps 2026-10-12: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
