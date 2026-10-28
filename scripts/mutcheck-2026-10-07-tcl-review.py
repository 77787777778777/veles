#!/usr/bin/env python3
# mutcheck — the /tcl public-output mutants. Each must turn its named test
# RED; the source is restored after every run (written fresh, so cargo sees
# a new mtime — never a cp -p restore).
#
# 1-7: the 2026-10-06 plan's mutants (anchors moved with the 10-07 review).
# 8-24: the 2026-10-07 review's: the bad list through reach, the scorer's
# incomplete/truncated answers, the fold, the posted-text check, the id
# fill, the store races, the stream cut, the atproto mask.
# 25-34: the same day's delete round: every public reply recorded, found
# by its link, only withheld ones asked, an approved repost kept
# deletable, the handle a Jetstream event lacks.
#
# Run: direnv exec . python3 scripts/mutcheck-2026-10-07-tcl-review.py [name…]
# with a per-run TMPDIR; prints one line per mutant and a total. Names
# given = only those mutants.
import os
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CORE = f"{ROOT}/crates/veles-core/src"
CK = f"{CORE}/slopdrop_check.rs"
SF = f"{CORE}/slopdrop_safety.rs"
WH = f"{CORE}/slopdrop_withheld.rs"
PB = f"{CORE}/slopdrop_publish.rs"
PR = f"{CORE}/providers.rs"
AG = f"{CORE}/agent.rs"
AD = f"{ROOT}/crates/veles-atproto/src/adapter.rs"
ID = f"{ROOT}/crates/veles-atproto/src/identity.rs"
GW = f"{ROOT}/crates/veles-frontends/src/gateway.rs"
SA = f"{CORE}/syncask.rs"
RS = f"{CORE}/responses.rs"


def run(name, path, old, new, crate, test):
    src = open(path, encoding="utf-8").read()
    if src.count(old) != 1:
        print(f"MUTANT {name}: ANCHOR {'MISSING' if old not in src else 'AMBIGUOUS'}", flush=True)
        return False
    open(path, "w", encoding="utf-8").write(src.replace(old, new, 1))
    try:
        time.sleep(1.2)
        p = subprocess.run(
            ["cargo", "test", "-p", crate, "--lib", test],
            cwd=ROOT, capture_output=True, text=True, timeout=1500,
            stdin=subprocess.DEVNULL,
        )
        out = p.stdout + p.stderr
        ran = "running 1 test" in out or "running 2 tests" in out
        red = p.returncode != 0 and ("test result: FAILED" in out or "error[" in out)
        how = "RED (correct)" if red else "STILL GREEN (BAD)"
        if red and "error[" in out and "test result: FAILED" not in out:
            how = "RED by compile error (check the anchor)"
            red = False
        if not ran and not red:
            how = "NO TEST RAN (BAD filter)"
        print(f"MUTANT {name}: {how}", flush=True)
        return red
    finally:
        open(path, "w", encoding="utf-8").write(src)
        time.sleep(1.2)


M = [
    # ── 1-7: the plan's ──
    ("drop-art-gate", CK,
     "        .any(|c| !c.is_ascii() || (c.is_control() && c != '\\n' && c != '\\t'))",
     "        .any(|c| c.is_ascii() && (c.is_control() && c != '\\n' && c != '\\t') && false)",
     "veles-core", "slopdrop_check::tests::art_and_shape_gate_trips"),
    ("drop-fold", CK, "'1' => 'i',", "'1' => '1',",
     "veles-core", "slopdrop_check::tests::the_fold_catches_leet_spaced_and_acrostic"),
    ("drop-url-gate", CK,
     "fn link_or_contact(text: &str) -> bool {\n    let lower = text.to_ascii_lowercase();",
     "fn link_or_contact(text: &str) -> bool {\n    let _ = text; return false;\n    #[allow(unreachable_code)]\n    let lower = text.to_ascii_lowercase();",
     "veles-core", "slopdrop_check::tests::link_and_contact_gate_trips"),
    ("timeout-as-pass", CK,
     "            Err(_) => {\n                mark_failed(ep);\n                continue;\n            }\n        };\n        let status",
     "            Err(_) => {\n                return Checked { outcome: Outcome::Pass, top: None, ms: None };\n            }\n        };\n        let status",
     "veles-core", "slopdrop_check::tests::a_dead_endpoint_means_withheld_not_pass"),
    ("approval-skips-compare", CK,
     "    if allowlisted {\n        return plain(Outcome::Pass);\n    }",
     "    let _ = allowlisted;\n    if true {\n        return plain(Outcome::Pass);\n    }",
     "veles-core", "slopdrop_check::tests::a_non_allowlisted_art_text_is_not_passed_by_run"),
    ("host-blocked-output", SF,
     "            artifacts: None,\n            kind: PostKind::Blocked,",
     "            artifacts: Some(build_artifacts(code, &hosted)),\n            kind: PostKind::Blocked,",
     "veles-core", "slopdrop_safety::tests::a_blocked_output_gets_no_hosted_copy"),
    ("skip-bad-list", SF,
     "verdict.filter(|v| v.bad_list())", "verdict.filter(|v| false && v.bad_list())",
     "veles-core", "slopdrop_safety::tests::a_bad_list_proc_stays_withheld_even_when_its_output_passes"),
    # ── 8-22: the review's ──
    ("drop-bad-reach", SF,
     "Some(v) if v.bad_list() => return Some(name.to_string()),",
     "Some(v) if false && v.bad_list() => return Some(name.to_string()),",
     "veles-core", "slopdrop_check::tests::the_public_decision_holds_a_line_that_wraps_a_bad_list_proc"),
    ("decision-skips-reach", CK,
     "        Some(i) => i\n            .bad_reach(code, &safety::verdicts())",
     "        Some(i) => i\n            .bad_reach(\"\", &safety::verdicts())",
     "veles-core", "slopdrop_check::tests::the_public_decision_holds_a_line_that_wraps_a_bad_list_proc"),
    ("judge-reads-a-missing-head-as-zero", CK,
     "            _ => return incomplete,", "            _ => heads.push((h, 0.0)),",
     "veles-core", "slopdrop_check::tests::an_incomplete_or_truncated_answer_is_withheld"),
    ("judge-ignores-truncated", CK,
     "    if body.get(\"truncated\").and_then(|v| v.as_bool()) != Some(false) {",
     "    if false {",
     "veles-core", "slopdrop_check::tests::an_incomplete_or_truncated_answer_is_withheld"),
    ("fold-drops-at", CK, "            '@' => 'a',", "            '@' => '@',",
     "veles-core", "slopdrop_check::tests::the_fold_catches_leet_spaced_and_acrostic"),
    ("check-the-raw-output", CK,
     "                &pt.body,\n                false,", "                output,\n                false,",
     "veles-core", "slopdrop_check::tests::the_public_decision_checks_the_posted_text"),
    ("strip-nothing", SF,
     "pub fn strip_irc_codes(input: &str) -> String {\n",
     "pub fn strip_irc_codes(input: &str) -> String {\n    if true { return input.to_string(); }\n",
     "veles-core", "slopdrop_safety::tests::public_text_is_the_posted_text"),
    ("fill-guesses-the-entry", AD,
     "    let Some(id) = entry_id else {\n        return;\n    };",
     "    let guessed = store.list().into_iter().find(|e| e.post_uris.is_empty()).map(|e| e.id);\n"
     "    let Some(id) = entry_id.map(str::to_string).or(guessed) else {\n        return;\n    };\n"
     "    let id = id.as_str();",
     "veles-atproto", "adapter::tests::a_posts_ids_land_on_the_named_entry_only"),
    ("act-saves-a-stale-snapshot", PB,
     "    let acted = s.act(&entry, action).await?;\n",
     "    let before = store.list();\n    let acted = s.act(&entry, action).await?;\n"
     "    for b in before.into_iter().filter(|b| b.id != id) {\n"
     "        let bid = b.id.clone();\n        let _ = store.update(&bid, |x| *x = b);\n    }\n",
     "veles-core", "slopdrop_publish::tests::two_approvals_in_flight_both_stand"),
    ("dedupe-drops-the-old-ids", WH,
     "                let mut posts = std::mem::take(&mut old.post_uris);",
     "                let mut posts: Vec<String> = Vec::new();\n                old.post_uris.clear();",
     "veles-core", "slopdrop_withheld::tests::the_same_output_twice_is_one_entry_that_remembers_both_posts"),
    ("a-cut-stream-is-an-answer", PR,
     "            if content.trim().is_empty() && tool_calls.is_empty() {",
     "            if false {",
     "veles-core", "providers::tests::a_stream_cut_with_only_reasoning_is_truncated"),
    ("no-retry-of-a-cut", AG,
     "            if let Err(ProviderError::Truncated(why)) = &result {",
     "            if let (false, Err(ProviderError::Truncated(why))) = (true, &result) {",
     "veles-core", "agent::tests::a_stream_cut_mid_thought_is_retried_not_taken_as_an_empty_answer"),
    ("atproto-handle-in-mask", AD,
     "            mask: None,\n            network: Some(\"atproto\".to_string()),",
     "            mask: ev.author_handle.clone(),\n            network: Some(\"atproto\".to_string()),",
     "veles-atproto", "adapter::tests::a_bluesky_sender_is_not_matched_by_irc_mask_rules"),
    ("no-sink-marks-anyway", PB,
     "    let Some(s) = sink() else {",
     "    let _ = delete_marks(&entry);\n    let Some(s) = sink() else {",
     "veles-core", "slopdrop_publish::tests::with_no_sink_nothing_is_marked_or_dropped"),
    ("approve-keeps-the-block", PB,
     "    safety::block_remove(&h)?;\n", "",
     "veles-core", "slopdrop_publish::tests::approve_takes_the_output_off_the_blocked_list"),
    ("responses-cut-is-an-answer", RS,
     "            if content.trim().is_empty() && tool_calls.is_empty() {",
     "            if false {",
     "veles-core", "responses::tests::a_responses_stream_cut_with_only_reasoning_is_truncated"),
    ("a-blocked-output-is-asked-again", SF,
     "            artifacts: None,\n            kind: PostKind::Blocked,",
     "            artifacts: None,\n            kind: PostKind::Withheld,",
     "veles-core", "slopdrop_safety::tests::a_blocked_output_gets_no_hosted_copy"),
    # ── 25-34: the delete round ──
    ("an-inline-reply-is-not-recorded", GW,
     "                            entry_id = Some(id);",
     "                            if s.kind == veles_core::slopdrop_withheld::PostKind::Withheld {\n"
     "                                entry_id = Some(id);\n                            }",
     "veles-frontends", "gateway::tests::every_public_tcl_reply_is_recorded_for_a_delete"),
    ("find-by-id-only", WH,
     "        let t = Target::parse(target)?;\n",
     "        if true {\n            return self.get(target);\n        }\n        let t = Target::parse(target)?;\n",
     "veles-core", "slopdrop_withheld::tests::an_entry_is_found_by_anything_the_owner_can_see"),
    ("find-ignores-the-request", WH,
     "                .or_else(|| hit(&|e| uri_rkey(&e.request_msg_id) == rkey)),",
     "                .or_else(|| None),",
     "veles-core", "slopdrop_withheld::tests::an_entry_is_found_by_anything_the_owner_can_see"),
    ("kinds-merge", WH,
     "                .find(|x| x.kind == e.kind && output_hash(&x.output) == h)",
     "                .find(|x| output_hash(&x.output) == h)",
     "veles-core", "slopdrop_withheld::tests::the_same_output_of_two_kinds_is_two_entries"),
    ("an-inline-reply-is-asked", SA,
     "        if e.no_ask || e.kind != crate::slopdrop_withheld::PostKind::Withheld {",
     "        if e.no_ask {",
     "veles-core", "syncask::tests::only_a_withheld_reply_is_asked_about"),
    ("approve-drops-the-entry", PB,
     "            let posted = acted.posted.clone();\n",
     "            let posted = acted.posted.clone();\n            let _ = store.remove(&id);\n",
     "veles-core", "slopdrop_publish::tests::an_approved_repost_stays_deletable"),
    ("approve-twice", PB,
     "    if action == WithheldAction::Approve && entry.kind == PostKind::Inline {",
     "    if false {",
     "veles-core", "slopdrop_publish::tests::an_approved_repost_stays_deletable"),
    ("a-profile-of-another-did", ID,
     "    if v.get(\"did\").and_then(|d| d.as_str()) != Some(did) {",
     "    if false {",
     "veles-atproto", "identity::tests::a_profile_answer_yields_only_a_vouched_handle_of_that_did"),
    ("an-invalid-handle-shown", ID,
     "        .filter(|h| !h.is_empty() && *h != \"handle.invalid\")",
     "        .filter(|h| !h.is_empty())",
     "veles-atproto", "identity::tests::a_profile_answer_yields_only_a_vouched_handle_of_that_did"),
    ("listing-without-the-link", f"{ROOT}/crates/veles-frontends/src/commands.rs",
     "            post,\n            truncate_chars(",
     "            \"\",\n            truncate_chars(",
     "veles-frontends", "commands::tests::the_ledger_listing_shows_each_posts_link"),
]

only = set(sys.argv[1:])
results = [run(*m) for m in M if not only or m[0] in only]
ok = sum(results)
print(f"== mutants: {ok}/{len(results)} red-correct, {len(results) - ok} problems ==", flush=True)
sys.exit(0 if ok == len(results) else 1)
