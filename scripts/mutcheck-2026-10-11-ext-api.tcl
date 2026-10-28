#!/usr/bin/env tclsh
# Mutation harness for the extension API's drift guard
# (PLAN-extension-api.md D7). Each mutant removes one guard, or makes one
# of the files the guard watches drift, and must turn its named check
# RED; the file is restored after every run. Verdicts as in
# scripts/mutcheck-2026-10-10-signal-pin.tcl: a timeout-killed run, a
# process that ends without a verdict, and a non-zero exit are all RED.
#
# Three kinds of mutant:
#   - code: a guard in the source is weakened (a rebuild per mutant);
#   - data: the spec, the changelog or the author doc drifts (the tests
#     read them at run time, so no rebuild);
#   - site: the docs site drifts, judged by the script or the site gate
#     that watches it (a command; RED is a non-zero exit).
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-ext-api.tcl [a filter]
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-ext-api.tcl --dry
set root [file normalize [file dirname [file dirname [info script]]]]
cd $root
set site [expr {[info exists ::env(VELES_DOCS_SITE)] ? $::env(VELES_DOCS_SITE) : "$::env(HOME)/veles-docs-work"}]
set EXTAPI   crates/veles-cli/src/extapi.rs
set PLUGINS  crates/veles-core/src/plugins.rs
set PRELUDE  crates/veles-lang-js/src/prelude.js
set SPEC     docs/extension-api.json
set CHANGES  docs/extension-api-changes.md
set DOC      docs/extending.md
set SITEPAGE $site/src/en/extending.md
set SITEREF  $site/ref/extension-api.json
set LIB  {-p veles-cli --lib}
set EXT  {-p veles-cli --test ext_api}
set CORE {-p veles-core --lib}
set T_PARSE   {extapi::tests::every_surface_parser_finds_its_known_items}
set T_CHECKS  {extapi::tests::each_check_refuses_its_drift}
set T_SPEC    {the_published_spec_matches_the_code}
set T_CHANGES {every_spec_change_has_a_changelog_entry}
set T_DOCS    {the_docs_match_the_spec}
set T_API     {plugins::tests::a_plugin_targeting_a_newer_api_is_noted}
set SITE_SH   {CMD bash scripts/check-ext-api-site.sh}
set SITE_GATE [list CMD sh -c "cd '$site' && tclsh build/check-sources.tcl en"]
# name file find repl target test
set mutations [list \
    [list {code: the JS parser finds no binding} \
        $EXTAPI \
        "        if !name.is_empty() \{\n            out.push(name);\n" \
        "        if false \{ // MUTATION: no JS binding is read\n            out.push(name);\n" \
        $LIB $T_PARSE] \
    [list {code: a repeated changelog version is accepted} \
        $EXTAPI \
        "            if p >= q \{" \
        "            if p > q \{ // MUTATION: a repeat passes" \
        $LIB $T_CHECKS] \
    [list {code: a doc that never names an item passes} \
        $EXTAPI \
        "let missing: Vec<&String> = names.iter().filter(|n| !mentioned.contains(*n)).collect();" \
        "let missing: Vec<&String> = names.iter().filter(|_| false).collect(); // MUTATION" \
        $LIB $T_CHECKS] \
    [list {code: a stale reference row passes} \
        $EXTAPI \
        "                .filter(|n| !names.contains(n))\n" \
        "                .filter(|_| false) // MUTATION: stale rows pass\n" \
        $LIB $T_CHECKS] \
    [list {code: a stale doc stamp passes} \
        $EXTAPI \
        "        Some(s) if s == stamp => \{\}\n" \
        "        Some(_) if true => \{\} // MUTATION: any stamp passes\n" \
        $LIB $T_CHECKS] \
    [list {code: a plugin targeting a newer API is not noted} \
        $PLUGINS \
        "    (w > h).then(||" \
        "    (w > h && false).then(||" \
        $CORE $T_API] \
    [list {code: a JS binding renamed without a new spec} \
        $PRELUDE \
        "    jsonGet: (...a) => call(\"json_get\", ...a)," \
        "    jsonGot: (...a) => call(\"json_get\", ...a), // MUTATION" \
        $EXT $T_SPEC] \
    [list {data: the spec names an op the code does not have} \
        $SPEC \
        "\"name\": \"call_llm\"" \
        "\"name\": \"call_llm_mutated\"" \
        $EXT $T_SPEC] \
    [list {data: the newest changelog entry names another hash} \
        $CHANGES \
        "\n## 0." \
        "\n## 0.1 sha256:0000000000000000 (MUTATION: a stale entry on top)\n\n## 0." \
        $EXT $T_CHANGES] \
    [list {data: the newest changelog entry names another version} \
        $CHANGES \
        "\n## 0." \
        "\n## 9" \
        $EXT $T_CHANGES] \
    [list {data: the author doc's stamp is stale} \
        $DOC \
        "<!-- extension-api: 0." \
        "<!-- extension-api: 9" \
        $EXT $T_DOCS] \
    [list {data: the author doc lost an item} \
        $DOC \
        "| `kanban_task_blocked` | reserved |\n" \
        "" \
        $EXT $T_DOCS] \
    [list {data: the author doc names an item the spec lacks} \
        $DOC \
        "| `kanban_task_blocked` | reserved |\n" \
        "| `kanban_task_blocked` | reserved |\n| `not_in_the_spec` | reserved |\n" \
        $EXT $T_DOCS] \
    [list {site: the site page's stamp is stale (the repo's gate step)} \
        $SITEPAGE \
        "extension_api: \"0." \
        "extension_api: \"9" \
        $SITE_SH {}] \
    [list {site: the site's copy of the spec differs (the repo's gate step)} \
        $SITEREF \
        "\"name\": \"call_llm\"" \
        "\"name\": \"call_llm_mutated\"" \
        $SITE_SH {}] \
    [list {site: the site page's stamp is stale (the site's own gate)} \
        $SITEPAGE \
        "extension_api: \"0." \
        "extension_api: \"9" \
        $SITE_GATE {}] \
]
proc run_cmd {argv} {
    set rc 0
    if {[catch {exec timeout -s KILL 900 sh -c "direnv exec . $argv 2>&1" </dev/null} out]} {
        if {[lindex $::errorCode 0] eq "CHILDSTATUS"} {
            set rc [lindex $::errorCode 2]
        } elseif {[lindex $::errorCode 0] eq "CHILDKILLED"} {
            set rc 137
        } else {
            set rc -1
        }
    }
    return [list $rc $out]
}
proc run_test {target filter} {
    if {[lindex $target 0] eq "CMD"} {
        # A command: a non-zero exit is the guard firing.
        set rc 0
        if {[catch {exec timeout -s KILL 300 {*}[lrange $target 1 end] </dev/null 2>@1} out]} {
            set rc 1
        }
        return [expr {$rc ? "RED" : "GREEN"}]
    }
    lassign [run_cmd "cargo test $target -- $filter"] rc out
    if {[regexp {error\[E[0-9]+\]|could not compile} $out]} { return "NOCOMPILE" }
    if {$rc == 124 || $rc == 137} { puts -nonewline "(killed by the timeout) "; return "RED" }
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
    # The first occurrence only: a data mutant means ONE drift.
    set at [string first $find $orig]
    set mutated "[string range $orig 0 [expr {$at - 1}]]$repl[string range $orig [expr {$at + [string length $find]}] end]"
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh $mutated
    close $fh
    set verdict [catch {run_test $target $filter} r]
    set fh [open $file w]
    fconfigure $fh -encoding utf-8
    puts -nonewline $fh $orig
    close $fh
    if {$verdict} { return "ERROR: $r" }
    return $r
}
# Before anything: every check must be GREEN on the tree as it is, or a
# RED mutant proves nothing.
proc baseline {} {
    foreach {target filter} [list $::LIB extapi:: $::EXT "" $::SITE_SH {} $::SITE_GATE {}] {
        set v [run_test $target $filter]
        if {$v ne "GREEN"} {
            puts "baseline is $v for [list $target $filter]: fix the tree first"
            exit 2
        }
    }
    puts "baseline: every check GREEN"
}
set filter ""
set dry 0
foreach a $argv {
    if {$a eq "--dry"} { set dry 1 } else { set filter $a }
}
if {!$dry} { baseline }
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
        puts [format "%-72s %s" $name [expr {$ok ? "ok" : "PATTERN-MISSING"}]]
        if {$ok} { incr red }
        continue
    }
    puts -nonewline [format "%-72s " $name]
    flush stdout
    set v [apply_and_run $file $find $repl $target $test]
    puts $v
    if {$v eq "RED"} { incr red }
}
puts "$red/$total [expr {$dry ? "patterns match" : "RED"}]"
exit [expr {$red == $total ? 0 : 1}]
