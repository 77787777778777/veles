#!/usr/bin/env tclsh
# Mutation harness for the 2026-10-11 round: an [acl] role held BY TICKET
# (the Bluesky test account, USER: "another owner of the veles account
# that uses our ticketing to operate so that veles can't just escalate
# privileges on its own"), `/sudo`, Bluesky co-operators, and the people
# gate letting through the mentions a reply's spec may carry. Each mutant
# removes one guard and must turn its named test RED; the source is
# restored after every run (written back, so cargo sees a fresh mtime).
#
#   direnv exec . tclsh scripts/mutcheck-2026-10-11-ticketed.tcl [--dry] [a filter]
#
# --dry only checks that every pattern is found in its file — run it
# first: a pattern that never matches reports PATTERN-MISSING, not RED.
# Matching is by `string first`, never a glob.

set root [file normalize [file dirname [file dirname [info script]]]]
cd $root

set ACL   crates/veles-core/src/acl.rs
set TKD   crates/veles-core/src/ticketed.rs
set AGENT crates/veles-core/src/agent.rs
set GW    crates/veles-frontends/src/gateway.rs
set CMDS  crates/veles-frontends/src/commands.rs
set ID    crates/veles-atproto/src/identity.rs
set AD    crates/veles-atproto/src/adapter.rs
set ALT   crates/veles-atproto/src/alttext.rs
set ACFG  crates/veles-atproto/src/config.rs
set CHECK crates/veles-slopdrop/src/check.rs
set SPUB  crates/veles-slopdrop/src/public.rs
set SBSKY crates/veles-slopdrop/src/bsky.rs
set LIB  {-p veles-core --lib}
set FLIB {-p veles-frontends --lib}
set ALIB {-p veles-atproto --lib}
set SLIB {-p veles-slopdrop --lib}

set T_STAND ticketed::tests::a_ticketed_owner_stands_at_user
set T_TURN  agent::tests::a_ticketed_owner_runs_an_owner_tool_only_on_a_signed_ticket
set T_GATES agent::tests::a_ticket_raises_only_to_the_role_held_and_never_past_the_gates
set T_SUDO  gateway::tests::sudo_raises_one_command_on_a_signed_ticket

# name file find repl target test
set mutations [list \
    [list "acl: the ticket role is dropped" \
        $ACL \
        "        let ticket_role = w.ticket_role.filter(|t| *t > w.role);" \
        "        let ticket_role: Option<Role> = None; // MUTATION" \
        $LIB $T_STAND] \
    [list "acl: a ticket role at or below the standing role still counts" \
        $ACL \
        "w.ticket_role.filter(|t| *t > w.role)" \
        "w.ticket_role.filter(|_t| true)" \
        $LIB ticketed::tests::a_ticketed_user_has_nothing_to_raise] \
    [list "acl: a users rule's ticket role is dropped" \
        $ACL \
        "                    u.ticket_role.as_deref().and_then(Role::parse)," \
        "                    None, // MUTATION" \
        $LIB $T_STAND] \
    [list "acl: an entries rule's ticket role is dropped" \
        $ACL \
        "                e.ticket_role.as_deref().and_then(Role::parse)," \
        "                None, // MUTATION" \
        $LIB ticketed::tests::a_ticketed_entry_stands_at_user] \
    [list "acl: a raised principal keeps its ticket role" \
        $ACL \
        "            via: Some(how.to_string()),
            ticket_role: None," \
        "            via: Some(how.to_string()),
            ticket_role: self.ticket_role, // MUTATION" \
        $LIB ticketed::tests::raised_keeps_the_subject_and_drops_the_ceiling] \
    [list "acl: a ticket would admit anyone" \
        $ACL \
        "            Some(role) => gate_denial(tool, cfg, &self.raised(role, \"\")).is_none(),
            None => false," \
        "            Some(role) => gate_denial(tool, cfg, &self.raised(role, \"\")).is_none(),
            None => true, // MUTATION" \
        $LIB ticketed::tests::a_ticket_would_admit_what_the_ceiling_may_run] \
    [list "ticketed: a ticket raises past the rule's role" \
        $TKD \
        "        .min(role);" \
        "        .max(Role::Guest); // MUTATION" \
        $LIB $T_GATES] \
    [list "ticketed: the ticket is not spent" \
        $TKD \
        "    crate::ticket::spend(&store.lock().unwrap_or_else(|e| e.into_inner()), &v, now)?;" \
        "    let _ = &store; // MUTATION: not spent" \
        $LIB $T_TURN] \
    [list "ticketed: the arguments verified are not the call's" \
        $TKD \
        "            tool,
            args,
            now," \
        "            tool,
            args: &serde_json::Value::Null, // MUTATION
            now," \
        $LIB $T_TURN] \
    [list "ticketed: a prompt passes as a /sudo command" \
        $TKD \
        "    if head.eq_ignore_ascii_case(\"sudo\") || !crate::aliases::is_builtin(head) \{" \
        "    if head.eq_ignore_ascii_case(\"sudo\") \{ // MUTATION" \
        $LIB ticketed::tests::sudo_parses_one_builtin_command] \
    [list "ticketed: /sudoku reads as /sudo" \
        $TKD \
        "    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) \{" \
        "    if false \{ // MUTATION" \
        $LIB ticketed::tests::sudo_parses_one_builtin_command] \
    [list "ticketed: the /sudo hint goes to everyone" \
        $TKD \
        "        _ => String::new()," \
        "        _ => \" You hold owner by ticket: `/sudo`\".to_string(), // MUTATION" \
        $LIB ticketed::tests::the_sudo_hint_is_for_a_ticketed_caller] \
    [list "agent: a ticketed caller is refused without being asked" \
        $AGENT \
        "                if who.ticket_would_admit(&gate_target, &self.cfg) \{" \
        "                if false \{ // MUTATION" \
        $LIB $T_TURN] \
    [list "agent: plan mode no longer stops a ticketed call" \
        $AGENT \
        "        if let Some(refused) = self.plan_refusal(target, args) \{" \
        "        if let Some(refused) = None::<String> \{ // MUTATION" \
        $LIB $T_GATES] \
    [list "agent: a ticket for less than the tool needs still runs it" \
        $AGENT \
        "        if let Some(d) = crate::acl::gate_denial(target, &self.cfg, &raised) \{" \
        "        if let Some(d) = None::<String> \{ // MUTATION" \
        $LIB $T_GATES] \
    [list "gateway: /sudo from a caller with no ticketed role is asked about" \
        $GW \
        "        if principal.ticket_role.is_none() \{
            return Err(" \
        "        if false \{
            return Err(" \
        $FLIB $T_SUDO] \
    [list "gateway: the raised command keeps the /sudo message id" \
        $GW \
        "inner_msg.msg_id = msg.msg_id.as_ref().map(|id| format!(\"\{id\}#sudo\"));" \
        "inner_msg.msg_id = msg.msg_id.clone(); // MUTATION" \
        $FLIB $T_SUDO] \
    [list "gateway: the public feed refuses /sudo" \
        $GW \
        "    if head == \"sudo\" && principal.ticket_role.is_some() \{" \
        "    if false \{ // MUTATION" \
        $FLIB gateway::tests::sudo_passes_the_public_feed_filter_only_for_a_ticketed_caller] \
    [list "gateway: the command gate's refusal says nothing of /sudo" \
        $GW \
        "                                veles_core::ticketed::sudo_hint(&principal, cap)" \
        "                                String::new() // MUTATION" \
        $FLIB $T_SUDO] \
    [list "commands: a command's own refusal says nothing of /sudo" \
        $CMDS \
        "        .map(|p| veles_core::ticketed::sudo_hint(p, cap))" \
        "        .map(|_p| String::new()) // MUTATION" \
        $FLIB $T_SUDO] \
    [list "atproto: a co-operator is resolved through the graph" \
        $ID \
        "        if self.co_operators.iter().any(|d| d == did) \{" \
        "        if false \{ // MUTATION" \
        $ALIB identity::tests::a_co_operator_resolves_without_the_network] \
    [list "atproto: the door refuses a co-operator" \
        $AD \
        "            Relation::Operator | Relation::CoOperator | Relation::Mutual" \
        "            Relation::Operator | Relation::Mutual" \
        $ALIB adapter::tests::a_co_operator_is_admitted] \
    [list "atproto: /alt on from a co-operator gets the stranger's answer" \
        $AD \
        "        (Relation::Mutual | Relation::CoOperator, Command::On) => \{" \
        "        (Relation::Mutual, Command::On) => \{" \
        $ALIB adapter::tests::a_co_operator_opts_in_to_alt_text_like_a_mutual] \
    [list "atproto: the reply re-check drops co-operators" \
        $ALT \
        "        crate::identity::Relation::Mutual | crate::identity::Relation::CoOperator
    )
\}" \
        "        crate::identity::Relation::Mutual
    )
\}" \
        $ALIB alttext::tests::the_reply_recheck_admits_mutuals_and_co_operators] \
    [list "atproto config: the operator may be its own co-operator" \
        $ACFG \
        ".filter(|d| d.starts_with(\"did:\") && *d != operator_did)" \
        ".filter(|d| d.starts_with(\"did:\"))" \
        $ALIB config::co_operator_tests::co_operators_parse_dids_only] \
    [list "check: an allowed mention is still read as a stranger" \
        $CHECK \
        "            if mentions.iter().any(|m| m.eq_ignore_ascii_case(bare)) \{
                continue;
            \}" \
        "            // MUTATION: no allowlist" \
        $SLIB check::tests::an_allowed_mention_passes_the_people_gate_and_nothing_else_does] \
    [list "check: any @handle passes" \
        $CHECK \
        "            if mentions.iter().any(|m| m.eq_ignore_ascii_case(bare)) \{" \
        "            if !mentions.is_empty() || bare.is_empty() \{ // MUTATION" \
        $SLIB check::tests::an_allowed_mention_passes_the_people_gate_and_nothing_else_does] \
    [list "public: the reply's mentions never reach the check" \
        $SPUB \
        "                &mentions,
            )
            .await;" \
        "                &\[\],
            )
            .await;" \
        $SLIB public::tests::a_reply_mentioning_its_caller_goes_out_inline] \
    [list "bsky: a recorded mention text is trusted over its handle" \
        $SBSKY \
        "        .filter_map(|r| r.get(\"handle\").and_then(Value::as_str))
        .map(|h| format!(\"@\{h\}\"))" \
        "        .filter_map(|r| r.get(\"text\").and_then(Value::as_str))
        .map(|h| h.to_string())" \
        $SLIB bsky::tests::allowed_mentions_are_handles_never_recorded_text] \
    [list "atproto: the thread is read from the answer wrapper again" \
        $AD \
        "    if let Some(node) = resp.get(\"thread\") \{
        flatten_thread(node, bot_did, &mut posts);
    \}" \
        "    flatten_thread(resp, bot_did, &mut posts); // MUTATION" \
        $ALIB adapter::tests::the_posts_come_out_of_a_get_post_thread_answer] \
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
puts "mutcheck ticketed owner 2026-10-11: $verified [expr {$dry ? {patterns found} : {verified red}}], $failures not"
exit [expr {$failures > 0}]
