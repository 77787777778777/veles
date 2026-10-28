#!/usr/bin/env tclsh
# Mutation harness for round P2 of PLAN-sessions-and-phone.md
# (2026-10-11): the agent's `config` tool (asked first, never credentials,
# never who may reach the agent, written through the running gateway) and
# the Phone page (what each row says, which Android screen each button
# opens, the redraw on coming back) and the All settings page (typed by
# the TOML library, typed JSON back, `raw` read by the gateway). Each mutant removes one guard and
# must turn its named test RED; the source is restored after every run.
#
# What each mutant UNLOCKS was checked first (memory: Xvfb does not
# isolate openers): phonetest stubs `borg`, so no mutant reaches Android
# or a browser; every config write lands in a scratch home (the unit
# tests pin port 9, the e2e runs its own gateway).
#
# Not a mutant here, on purpose: desktop_tools' unit-test port (9 when a
# test names none). Mutating it back to 8642 would point the unit tests'
# writes at the person's running gateway.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-config-phone.tcl [--dry] [a filter]

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set CT    crates/veles-core/src/config_tool.rs
set ACL   crates/veles-core/src/acl.rs
set PH    veles-desktop/lib/phone.tcl
set PAL   veles-desktop/lib/palette.tcl
set SF    crates/veles-core/src/settings_form.rs
set CE    crates/veles-core/src/configedit.rs
set SFT   veles-desktop/lib/settingsform.tcl
set LIB {-p veles-core --lib}
set FRONT {-p veles-frontends --lib}
set SFTEST veles-desktop/lib/settingsformtest.tcl
set E2E {-p veles-cli --test config_tool}
set PT  veles-desktop/lib/phonetest.tcl

set mutations [list \
    [list "config: who may reach the agent is writable" \
        $CT \
        "        if let Some(why) = guarded(path) \{" \
        "        if let Some(why) = guarded(path).filter(|_| false) \{ // MUTATION" \
        $LIB config_tool::tests::credentials_and_absent_owners_change_nothing] \
    [list "config: a credential's key goes through a turn" \
        $CT \
        "        if names_a_secret(path)
            || value" \
        "        if false && names_a_secret(path) // MUTATION
            || value" \
        $LIB config_tool::tests::credentials_and_absent_owners_change_nothing] \
    [list "config: any answer is a yes" \
        $CT \
        "        if answer.first() != Some(crate::ask::CHOICE_ONCE) \{" \
        "        if false \{ // MUTATION" \
        $LIB config_tool::tests::a_change_is_asked_first_and_only_a_yes_writes_it] \
    [list "config: silence reads as a refusal, not as no answer" \
        $CT \
        "        if answer.timed_out \{" \
        "        if false \{ // MUTATION" \
        $LIB config_tool::tests::no_answer_no_change_and_credentials_stay_hidden] \
    [list "config: an unchanged value is asked anyway" \
        $CT \
        "        if now == value \{" \
        "        if false \{ // MUTATION" \
        $LIB config_tool::tests::no_answer_no_change_and_credentials_stay_hidden] \
    [list "config: get shows a credential" \
        $CT \
        "        _ if names_a_secret(key) || crate::a2a::secret_shaped(&v.to_string()) => \{" \
        "        _ if false => \{ // MUTATION" \
        $LIB config_tool::tests::no_answer_no_change_and_credentials_stay_hidden] \
    [list "config: the write skips the running gateway" \
        $CT \
        "            \"/api/config/set\"," \
        "            \"/api/no-such-route\", // MUTATION" \
        $E2E the_owner_is_asked_and_only_a_yes_changes_the_live_config] \
    [list "acl: the config tool reaches a session ungated" \
        $ACL \
        "        \"config\" => All," \
        "        // MUTATION: unclassified" \
        $LIB tools::tests::every_registered_tool_is_acl_classified] \
    [list "phone: notifications off read as on" \
        $PH \
        "        0 \{
            ::vi18n::label \$n.state \{off\}" \
        "        0 \{
            ::vi18n::label \$n.state \{on\}" \
        TCL $PT] \
    [list "phone: the notification screen is not this app's" \
        $PH \
        "                \[list android.provider.extra.APP_PACKAGE \$pkg\]" \
        "                \{\}" \
        TCL $PT] \
    [list "phone: no fallback when the battery dialog is missing" \
        $PH \
        "            if \{\[catch \{_intent android.settings.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS package:\$pkg \{\}\}\]\} \{" \
        "            catch \{_intent android.settings.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS package:\$pkg \{\}\} ; if \{0\} \{" \
        TCL $PT] \
    [list "phone: precise location asks again" \
        $PH \
        "            if \{\[_get \[status\] location\] eq \"precise\"" \
        "            if \{0 && \[_get \[status\] location\] eq \"precise\"" \
        TCL $PT] \
    [list "phone: back from Android's settings, nothing redraws" \
        $PH \
        "    if \{\[_mtime\] != \$drawn_mtime\} \{ render \$w \}" \
        "    if \{0\} \{ render \$w \}" \
        TCL $PT] \
    [list "phone: closing leaves the watch running" \
        $PH \
        "    after cancel \$poll" \
        "    # MUTATION after cancel \$poll" \
        TCL $PT] \
    [list "phone: every maker gets the extra step" \
        $PH \
        "    if \{\$maker in \$strict_makers\} \{" \
        "    if \{1\} \{" \
        TCL $PT] \
    [list "phone: a status value is cut at its own =" \
        $PH \
        "        set i \[string first = \$line\]" \
        "        set i \[string last = \$line\]" \
        TCL $PT] \
    [list "form: a record shows as a field" \
        $SF \
        "        if k.shape == Shape::TableArray || crate::config_tool::names_a_secret(&k.path) \{" \
        "        if crate::config_tool::names_a_secret(&k.path) \{ // MUTATION" \
        $LIB settings_form::tests::every_option_is_a_typed_field_with_its_value_now] \
    [list "form: a credential shows as a field" \
        $SF \
        "        if k.shape == Shape::TableArray || crate::config_tool::names_a_secret(&k.path) \{" \
        "        if k.shape == Shape::TableArray \{ // MUTATION" \
        $LIB settings_form::tests::every_option_is_a_typed_field_with_its_value_now] \
    [list "form: typed by what the file holds" \
        $SF \
        "        let kind = default
            .as_ref()
            .or(value.as_ref())" \
        "        let kind = value // MUTATION
            .as_ref()
            .or(default.as_ref())" \
        $LIB settings_form::tests::every_option_is_a_typed_field_with_its_value_now] \
    [list "form: a section split around its sub-tables" \
        $SF \
        "    knobs.sort_by(|a, b| (a.section(), a.key()).cmp(&(b.section(), b.key())));" \
        "    // MUTATION: no section sort" \
        $LIB settings_form::tests::every_option_is_a_typed_field_with_its_value_now] \
    [list "form: raw text is always a string" \
        $CE \
        "    text.trim()
        .parse::<Value>()
        .unwrap_or_else(|_| Value::from(text.trim()))" \
        "    Value::from(text.trim()) // MUTATION" \
        $FRONT desktop_api::tests::config_set_reads_raw_text_as_a_toml_literal_and_the_form_shows_it] \
    [list "form page: a number that is not one is sent" \
        $SFT \
        "            if \{!\[regexp \{^-?\[0-9\]+\$\} \$text\]\} \{ return \[_say_bad \$r\] \}" \
        "            if \{0\} \{ return \[_say_bad \$r\] \}" \
        TCL $SFTEST] \
    [list "form page: a list keeps its blanks" \
        $SFT \
        "                if \{\$item ne \"\"\} \{ lappend items" \
        "                if \{1\} \{ lappend items" \
        TCL $SFTEST] \
    [list "form page: untyped text goes as a string" \
        $SFT \
        "            _send \$r \$path \"\\\"\[::vapi::json_escape \$text\]\\\"\" raw" \
        "            _send \$r \$path \"\\\"\[::vapi::json_escape \$text\]\\\"\" value" \
        TCL $SFTEST] \
    [list "form page: a reset leaves the old text" \
        $SFT \
        "        if \{\[winfo exists \$r.c\]\} \{ _fill \$r \$k \}" \
        "        if \{0\} \{ _fill \$r \$k \}" \
        TCL $SFTEST] \
    [list "form page: a search sees only the open section" \
        $SFT \
        "            if \{\$q ne \"\"\} \{" \
        "            if \{0\} \{" \
        TCL $SFTEST] \
    [list "form page: leaving a field writes it again" \
        $SFT \
        "    if \{\$text eq \$was || (\$text eq \"\" && \$was eq \"null\")\} \{ return \}" \
        "    if \{0\} \{ return \}" \
        TCL $SFTEST] \
    [list "phone: intent extras in the component's place" \
        $PH \
        "    borg activity \$action \$uri \{\} \{\} \{\} \$extras" \
        "    borg activity \$action \$uri \{\} \{\} \$extras" \
        TCL $PT] \
    [list "share: intent extras in the component's place" \
        veles-desktop/lib/device_android.tcl \
        "    borg activity android.intent.action.SEND \{\} text/plain \{\} \{\} \\" \
        "    borg activity android.intent.action.SEND \{\} text/plain \{\} \\" \
        TCL veles-desktop/lib/devicetest.tcl] \
    [list "form page: an empty default reads \"default:\"" \
        $SFT \
        "    if \{\$d ne \"null\" && \[string trim \$d\] ne \"\"\} \{" \
        "    if \{\$d ne \"null\"\} \{" \
        TCL $SFTEST] \
    [list "theme: a readonly picker is clam's grey" \
        veles-desktop/lib/theme.tcl \
        "    ttk::style map TCombobox \\" \
        "    list \\" \
        TCL $SFTEST] \
    [list "palette: the Phone row shows off a phone" \
        $PAL \
        "    if \{!(\[llength \[info commands ::vandroid::on\]\] && \[::vandroid::on\])\} \{" \
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
puts "mutcheck config + phone 2026-10-11: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
