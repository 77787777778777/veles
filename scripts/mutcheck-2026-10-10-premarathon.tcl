#!/usr/bin/env tclsh
# Mutation harness for the 2026-10-10 pre-marathon round: the plan-mode
# shell splitter (item 1) and notes on asks (item 4). Each mutant removes
# one guard and must turn its named test RED; the source is restored
# after every run (written back, so cargo sees a fresh mtime). A target
# of TCL runs a desktop suite on a private Xvfb this script starts (there
# is no xvfb-run on this machine); its test field is the suite path.
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-10-premarathon.tcl [--dry] [a filter]
#
# --dry only checks that every pattern is found in its file — run it
# first: a pattern that never matches reports PATTERN-MISSING, not RED.
# Matching is by `string first`, never a glob.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set PLAN crates/veles-core/src/plan.rs
set AGENT crates/veles-core/src/agent.rs
set ASK  crates/veles-core/src/ask.rs
set TUI  crates/veles-frontends/src/tui.rs
set ASKTCL veles-desktop/lib/ask.tcl
set SBSKY crates/veles-slopdrop/src/bsky.rs
set SPUB crates/veles-slopdrop/src/public.rs
set RICH crates/veles-atproto/src/rich.rs
set ALT  crates/veles-atproto/src/alttext.rs
set NOTIFY crates/veles-atproto/src/notify.rs
set DAPI crates/veles-frontends/src/desktop_api.rs
set LIB  {-p veles-core --lib}
set FLIB {-p veles-frontends --lib}
set SLIB {-p veles-slopdrop --lib}
set ALIB {-p veles-atproto --lib}

# name file find repl target test
set mutations [list \
    [list {plan: `$(` inside double quotes is no longer refused} \
        $PLAN \
        {                            if c.get(i + 1) == Some(&'(') {
                                return Err("`$(…)` runs a command inside the command".into());
                            }
                            cur.push('$');} \
        {                            cur.push('$'); // MUTATION: $( in quotes passes} \
        $LIB {plan::tests::shapes_that_cannot_be_judged_from_their_words_are_refused}] \
    [list {plan: a backtick inside double quotes is no longer refused} \
        $PLAN \
        {                        Some('`') => {
                            return Err("a backtick runs a command inside the command".into())
                        }} \
        {                        Some('`') => { cur.push('`'); i += 1; } // MUTATION} \
        $LIB {plan::tests::shapes_that_cannot_be_judged_from_their_words_are_refused}] \
    [list {plan: a lone & is read as part of a word} \
        $PLAN \
        {            '&' => return Err("a lone `&` sends part of the command to the background".into()),} \
        {            '&' => { in_word = true; cur.push('&'); i += 1; } // MUTATION} \
        $LIB {plan::tests::shapes_that_cannot_be_judged_from_their_words_are_refused}] \
    [list {plan: the heredoc reason is gone (a vaguer rule catches it)} \
        $PLAN \
        {        ('<', Some('<')) => {
            return Err("a heredoc or here-string cannot be judged from its words".into())
        }} \
        {        // MUTATION: no heredoc arm} \
        $LIB {plan::tests::shapes_that_cannot_be_judged_from_their_words_are_refused}] \
    [list {plan: the process-substitution reason is gone} \
        $PLAN \
        {        (_, Some('(')) => {
            return Err("process substitution runs a command inside the command".into())
        }} \
        {        // MUTATION: no process-substitution arm} \
        $LIB {plan::tests::shapes_that_cannot_be_judged_from_their_words_are_refused}] \
    [list {plan: any redirect target passes} \
        $PLAN \
        {    if target == "/dev/null" {
        return Ok(i);
    }} \
        {    if !target.is_empty() { // MUTATION: any target
        return Ok(i);
    }} \
        $LIB {plan::tests::redirects_that_write_or_read_files_are_refused}] \
    [list {plan: `>& file` passes as a descriptor dup} \
        $PLAN \
        {if i > start || at(i) == Some('-')} \
        {if true /* MUTATION: any >& is a dup */} \
        $LIB {plan::tests::redirects_that_write_or_read_files_are_refused}] \
    [list {plan: a pipe is read as a word break, not a separator} \
        $PLAN \
        {            '|' => {
                flush(&mut segs, &mut cur, &mut in_word);
                segs.push(Vec::new());
                i += if matches!(next, Some('|') | Some('&')) {
                    2
                } else {
                    1
                };
            }} \
        {            '|' => {
                flush(&mut segs, &mut cur, &mut in_word); // MUTATION: no new segment
                i += if matches!(next, Some('|') | Some('&')) {
                    2
                } else {
                    1
                };
            }} \
        $LIB {plan::tests::quotes_keep_separators_inside_one_word}] \
    [list {plan: `;` does not separate} \
        $PLAN \
        {            '\n' | ';' => {
                flush(&mut segs, &mut cur, &mut in_word);
                segs.push(Vec::new());
                i += 1;
            }} \
        {            '\n' | ';' => {
                flush(&mut segs, &mut cur, &mut in_word); // MUTATION: no new segment
                i += 1;
            }} \
        $LIB {plan::tests::quotes_keep_separators_inside_one_word}] \
    [list {plan: `&&` does not separate} \
        $PLAN \
        {            '&' if next == Some('&') => {
                flush(&mut segs, &mut cur, &mut in_word);
                segs.push(Vec::new());
                i += 2;
            }} \
        {            '&' if next == Some('&') => {
                flush(&mut segs, &mut cur, &mut in_word); // MUTATION: no new segment
                i += 2;
            }} \
        $LIB {plan::tests::chaining_defeats_a_read_only_head}] \
    [list {plan: only the first segment is judged} \
        $PLAN \
        {    for seg in &segs {
        judge_segment(seg)?;
    }} \
        {    judge_segment(&segs[0])?; // MUTATION: the rest pass unread} \
        $LIB {plan::tests::chaining_defeats_a_read_only_head}] \
    [list {plan: `--flag=value` no longer hits its flag} \
        $PLAN \
        {.is_some_and(|tail| tail.starts_with('='));} \
        {.is_some_and(|tail| tail.is_empty()); // MUTATION} \
        $LIB {plan::tests::the_write_paths_inside_read_commands_are_refused}] \
    [list {plan: a short flag inside a cluster is missed} \
        $PLAN \
        {return cluster.chars().all(|c| c.is_ascii_alphabetic()) && cluster.contains(letter);} \
        {return false && cluster.contains(letter); // MUTATION} \
        $LIB {plan::tests::the_write_paths_inside_read_commands_are_refused}] \
    [list {plan: `git -c` is skipped like any flag} \
        $PLAN \
        {w == "-c" || w.starts_with("--config-env") || (w.starts_with("-c") && w.len() > 2)} \
        {false /* MUTATION: -c passes */} \
        $LIB {plan::tests::the_write_paths_inside_read_commands_are_refused}] \
    [list {plan: `git --exec-path=` is skipped like any flag} \
        $PLAN \
        {if w.starts_with("--exec-path=")} \
        {if false /* MUTATION */} \
        $LIB {plan::tests::the_write_paths_inside_read_commands_are_refused}] \
    [list {plan: `git -C` no longer skips its value} \
        $PLAN \
        {matches!(w, "-C" | "--git-dir" | "--work-tree" | "--namespace")} \
        {false /* MUTATION */} \
        $LIB {plan::tests::the_write_paths_inside_read_commands_are_refused}] \
    [list {plan: sed -i in a cluster passes} \
        $PLAN \
        {cluster.contains('i') || cluster.contains('f')} \
        {cluster.contains('f') /* MUTATION: -i passes */} \
        $LIB {plan::tests::sed_runs_only_as_a_printer}] \
    [list {plan: sed without -n passes} \
        $PLAN \
        {    quiet
        && scripts.iter()} \
        {    true // MUTATION: -n not required
        && scripts.iter()} \
        $LIB {plan::tests::sed_runs_only_as_a_printer}] \
    [list {plan: any sed script passes} \
        $PLAN \
        {.all(|p| re.is_match(p))} \
        {.all(|p| { let _ = (p, re); true }) /* MUTATION */} \
        $LIB {plan::tests::sed_runs_only_as_a_printer}] \
    [list {plan: `command X` passes without judging X} \
        $PLAN \
        {return judge_segment(&words[k..]);} \
        {let _ = k; return Ok(()); // MUTATION} \
        $LIB {plan::tests::the_write_paths_inside_read_commands_are_refused}] \
    [list {plan: `uniq in out` passes} \
        $PLAN \
        {head == "uniq" && positionals.len() > 1} \
        {head == "uniq" && positionals.len() > 9 /* MUTATION */} \
        $LIB {plan::tests::the_write_paths_inside_read_commands_are_refused}] \
    [list {plan: rg --pre is not a takeback} \
        $PLAN \
        {("rg", &["--pre"]),} \
        {("rg", &[]), // MUTATION} \
        $LIB {plan::tests::the_write_paths_inside_read_commands_are_refused}] \
    [list {plan: the refusal no longer says reads run} \
        $PLAN \
        {Read-only shell DOES run in plan mode} \
        {MUTATION: no hint} \
        $LIB {plan::tests::the_terminal_refusal_names_the_part_and_says_reads_run}] \
    [list {notes: a keep-planning note is dropped} \
        $PLAN \
        {{n}\n\nRevise the plan to address that} \
        {MUTATION\n\nRevise the plan to address that} \
        $LIB {plan::tests::a_note_reaches_the_model_with_either_verdict_and_alone_never_approves}] \
    [list {notes: a plain yes (once) no longer approves a plan} \
        $PLAN \
        {            Some(CHOICE_APPROVE) | Some(crate::ask::CHOICE_ONCE)} \
        {            Some(CHOICE_APPROVE) /* MUTATION: once is not approve */} \
        $LIB {plan::tests::a_typed_yes_approves_and_a_typed_no_keeps_planning}] \
    [list {notes: an allowed call's note is not handed back} \
        $AGENT \
        {(extras, &note)} \
        {(None::<&ApprovalExtras>, &note) /* MUTATION */} \
        $LIB {agent::tests::an_approval_note_reaches_the_model_and_a_bare_note_declines}] \
    [list {notes: a declined call's note is dropped} \
        $AGENT \
        {"{}\nThe user declined it and wrote:\n\n{n}",} \
        {"{}", /* MUTATION */} \
        $LIB {agent::tests::an_approval_note_reaches_the_model_and_a_bare_note_declines}] \
    [list {notes: a typed choice + note is not an answer} \
        $ASK \
        {(key, note.is_empty())} \
        {(key, true) /* MUTATION */} \
        $LIB {ask::tests::a_choice_with_a_note_answers_both_and_prose_still_answers_nothing}] \
    [list {notes: the TUI key answer drops the note} \
        $TUI \
        {        a.free_text = Some(note.to_string());} \
        {        let _ = note; // MUTATION} \
        $FLIB {tui::tui_tests::an_approval_key_carries_the_typed_note_and_a_clarify_is_untouched}] \
    [list {notes: a clicked desktop row sends no note} \
        $ASKTCL \
        {                    bind $sub <Button-1> [list ::vask::_answer_key_entry $id $key $w.btns.e]} \
        {                    bind $sub <Button-1> [list ::vask::_answer $id $key ""] ;# MUTATION} \
        TCL veles-desktop/lib/asktest.tcl] \
    [list {notes: a digit typed into the note answers} \
        $ASKTCL \
        {    if {$cls in {TEntry Entry Text}} {
        return
    }} \
        {    # MUTATION: digits answer even from the note box} \
        TCL veles-desktop/lib/asktest.tcl] \
    [list {notes: Enter sends choice 1 with the note again} \
        $ASKTCL \
        "        bell\n        return\n    \}\n    ::vask::_answer \$id \"\" \$txt" \
        "        bell\n        return\n    \}\n    ::vask::_answer \$id once \$txt ;# MUTATION" \
        TCL veles-desktop/lib/asktest.tcl] \
    [list {bsky: a stranger may be mentioned} \
        $SBSKY \
        {        allowed.caller.map(lc).is_some_and(|c| c == h)} \
        {        true || allowed.caller.map(lc).is_some_and(|c| c == h) // MUTATION} \
        $SLIB {bsky::tests::a_spec_keeps_only_what_a_public_reply_may_carry}] \
    [list {bsky: a public reply may carry images} \
        $SBSKY \
        {if !allowed.images} \
        {if false /* MUTATION */} \
        $SLIB {bsky::tests::a_spec_keeps_only_what_a_public_reply_may_carry}] \
    [list {bsky: a read grant is never spent} \
        $SBSKY \
        {            grant.remaining -= 1;} \
        {            // MUTATION: not spent} \
        $SLIB {bsky::tests::a_read_grant_is_spent_and_revoked}] \
    [list {bsky: any XRPC method may be read} \
        $SBSKY \
        {if !READS.contains(&method)} \
        {if false /* MUTATION */} \
        $SLIB {bsky::tests::the_read_route_refuses_without_a_grant_and_outside_the_allowlist}] \
    [list {bsky: anyone may push Bluesky events} \
        $SBSKY \
        {if cx.caller != "process:atproto"} \
        {if false /* MUTATION */} \
        $SLIB {bsky::tests::only_the_atproto_plugin_may_push_events}] \
    [list {bsky: a follow's answer has a post to go under} \
        $SBSKY \
        {"FOLLOW" => None,} \
        {"FOLLOW" => Some(p.uri.clone()), // MUTATION} \
        $SLIB {bsky::tests::each_event_gets_slopdrops_argument_shape_and_its_reply_target}] \
    [list {bsky: a withheld reply carries the spec} \
        $SPUB \
        {if s.kind == PostKind::Inline} \
        {if true /* MUTATION */} \
        $SLIB {public::tests::an_inline_reply_carries_its_validated_spec_and_its_post_breaks}] \
    [list {rich: an unresolved mention still gets a facet} \
        $RICH \
        {                None => {
                    placed[i] = true;
                    from = end;
                    continue;
                }} \
        {                None => json!({"$type": "app.bsky.richtext.facet#mention", "did": h}), // MUTATION} \
        $ALIB {rich::tests::spans_land_on_the_right_bytes_across_posts_and_in_order}] \
    [list {rich: an auto link facet may overlap a spec one} \
        $RICH \
        {if taken.iter().all(|&(x, y)| b <= x || a >= y)} \
        {if true /* MUTATION */} \
        $ALIB {rich::tests::a_spec_link_over_a_bare_url_wins_and_the_rest_stay}] \
    [list {alt: a mutual who did not opt in is a candidate} \
        $ALT \
        {        let Some(since) = self.since(&ev.author_did) else {
            return false;
        };} \
        {        let since = self.since(&ev.author_did).unwrap_or(0); // MUTATION} \
        $ALIB {alttext::tests::the_operator_and_opted_in_mutuals_are_candidates_and_nobody_else}] \
    [list {alt: two workers may take one post} \
        $ALT \
        {.map(|mut s| s.insert(uri.to_string()))} \
        {.map(|mut s| { s.insert(uri.to_string()); true }) /* MUTATION */} \
        $ALIB {alttext::tests::one_worker_per_post}] \
    [list {notify: the bot's own doing is a notice} \
        $NOTIFY \
        {if did.is_empty() || did == bot_did} \
        {if did.is_empty() /* MUTATION */} \
        $ALIB {notify::tests::every_bound_reason_becomes_a_notice_with_its_subject_and_thread}] \
    [list {desktop: a deeper path under open/ skips the gate} \
        $DAPI \
        {["api", "plugins", name, "open", route]} \
        {["api", "plugins", name, "open", route, ..] /* MUTATION */} \
        $FLIB {desktop_api::tests::a_modules_open_route_skips_the_bearer_gate_and_nothing_else_does}] \
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
puts "mutcheck pre-marathon 2026-10-10: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
