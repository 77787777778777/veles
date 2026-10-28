#!/usr/bin/env tclsh
# mutcheck-2026-09-29-review.tcl — mutation harness for the review of the
# B5/C/B6 batch (PLAN-pool-round.md, Part A): the storesync and migration
# receivers treat peer data as data, the log follows the config, the
# fence and the park serialize, and a lease loss is noticed on time and
# recovered from. Then Part C: replicas complete enough to take over;
# Part S, the headless signer; Part E, bot challenges put to a person.
#
# House rules (b3a/b4/c3/b6 harnesses):
#   - cargo stderr goes through `2>@1`, NEVER `2>&1`.
#   - every pattern is DRY-CHECKED first: it must occur exactly once, or
#     the run stops before mutating anything (a braced or mistyped
#     pattern that never matches would otherwise score as NOTFOUND at
#     best, and a mutation that lands twice mutates what nobody meant).
#   - a mutation that fails to COMPILE is NOCOMPILE, not a detection.
#   - green requires a NONZERO pass count ("ok. 0 passed" from a filter
#     that matches nothing is not green — the b3a vacuity lesson).
#   - QUOTING: every pattern is a DOUBLE-QUOTED word — `[`, `$`, `\`
#     escaped, real newlines as `\n`, Rust `"` as `\"`.
#   - run it with a per-run TMPDIR (the gate's own litter rule):
#       RUN=$(mktemp -d /dev/shm/veles-run.XXXXXX)
#       TMPDIR=$RUN tclsh scripts/mutcheck-2026-09-29-review.tcl </dev/null
#       command rm -rf "$RUN"

set repo [file normalize [file dirname [file dirname [info script]]]]
set targets [dict create]

proc target {name pkg file find replace test why} {
    global targets
    dict set targets $name [dict create pkg $pkg file $file find $find \
        replace $replace test $test why $why]
}

set ss crates/veles-core/src/storesync.rs
set mg crates/veles-core/src/migration.rs

# ── A1: storesync — an offer is data from a peer ─────────────────────
# Both layers of the ALTER path at once: with only the identifier check
# gone, the single-statement `execute` runs the ALTER alone, the apply's
# transaction rolls it back, and upsert_row's own check refuses the row —
# an equivalent mutant (measured: GREEN-ESC). The pair is the exploit.
target ident-check veles-core $ss \
    "        let k = wire_ident(k)?;\n        if existing.contains(k) \{" \
    "        if existing.contains(k) \{" \
    a_hostile_column_name_is_refused_and_nothing_escapes \
    "a row key closes its quote and the drift-fix ALTER runs whatever follows (ATTACH the primary)"
dict set targets ident-check extra [list \
    "        conn.execute(&ddl, \[\])\n" \
    "        conn.execute_batch(&ddl)\n"]
target pk-is-the-pk veles-core $ss \
    "        keys == want," \
    "        true," \
    a_hostile_pk_key_is_refused \
    "a pk key widens a replicated DELETE to every row"
target node-id-is-a-file-name veles-core $ss \
    "    anyhow::ensure!(\n        valid_node_id(from_node),\n        \"{from_node:?} is not a node id — no replica file is opened for it\"" \
    "    anyhow::ensure!(\n        true,\n        \"{from_node:?} is not a node id — no replica file is opened for it\"" \
    a_path_shaped_node_id_opens_nothing \
    "`../state` opens and stamps the node's own state.db"
target fold-ban veles-core $ss \
    "        marker.as_deref() == Some(offer.from_node.as_str())," \
    "        true," \
    apply_refuses_a_store_that_is_not_the_senders_replica \
    "an offer applies into the primary, or into another node's replica"
target first-use-binds veles-core $ss \
    "    meta_put(store.conn(), &key, from_node)?;\n    Ok(())" \
    "    Ok(())" \
    a_peer_speaks_for_one_node \
    "a peer that is not bound can speak for any node it likes, offer by offer"
target receiver-binds-sender veles-frontends crates/veles-frontends/src/a2a_server.rs \
    "                .and_then(|offer| bound(&offer).map(|()| offer))" \
    "                .and_then(|offer| { let _ = &bound; Ok(offer) })" \
    a_store_offer_cannot_name_a_path_or_another_peers_node \
    "the receiver takes the offer's own claim of who sent it"

# ── A2: the log follows the config ──────────────────────────────────
target no-peers-no-log veles-core $ss \
    "    if storesync_peers.is_empty() {\n        return Ok(if uninstall_log(store)? {" \
    "    if false {\n        return Ok(if uninstall_log(store)? {" \
    no_storesync_peers_means_no_log \
    "a single node copies every message into store_log forever"
target owner-stamp-without-log veles-core $ss \
    "    stamp_owner(store)?;\n    if storesync_peers.is_empty() \{" \
    "    if storesync_peers.is_empty() \{" \
    the_owner_stamp_does_not_need_the_log \
    "a node with no storesync peers stops saying which node owns its sessions"
target prune-waits-for-every-peer veles-core $ss \
    "            None => return Ok(0)," \
    "            None => continue," \
    pruning_keeps_what_any_peer_has_not_applied \
    "a peer that never reported loses rows it still needs"
target acked-is-monotone veles-core $ss \
    "    if applied > cur {" \
    "    if true {" \
    pruning_keeps_what_any_peer_has_not_applied \
    "a stale reply lowers the floor — or a reordered one raises it past what was applied"

# ── A3: adopt trusts the bundle ─────────────────────────────────────
target foreign-rows-refused veles-core $mg \
    "        .find(|sid| !ids.contains(sid));" \
    "        .find(|_sid| false);" \
    adopt_refuses_rows_for_sessions_the_bundle_does_not_carry \
    "a bundle plants history and a queued turn in a live local session"
target blob-key-stays-home veles-core $mg \
    "            dir == crate::checkpoints::sanitize(&c.session_id)\n                && plain_part(dir)\n                && plain_part(name)" \
    "            { let _ = (dir, name); true }" \
    a_blob_key_cannot_leave_the_blob_root \
    "a peer-chosen blob key writes any file the user can"

# ── A4: migration correctness ───────────────────────────────────────
target tombstone-replaced veles-core $mg \
    "                conn.execute(sql, rusqlite::params!\[s.id\])?;" \
    "                let _ = sql;" \
    a_round_trip_leaves_one_copy \
    "A→B→A doubles every message"
target archived-travels veles-core $mg \
    "                    m.archived as i64," \
    "                    0i64," \
    archived_history_travels_with_its_flag \
    "compacted history arrives as live context"
target notes-keep-their-place veles-core $mg \
    "                .and_then(|m| m.range(..=n.after_source_msg_id).next_back())" \
    "                .and_then(|m| m.iter().next_back())" \
    notes_travel_and_keep_their_place \
    "every note lands at the end of the transcript"
target park-checks-the-registry veles-core $mg \
    "        if !busy.is_empty() {" \
    "        if false {" \
    a_turn_admitted_by_the_fence_blocks_park_until_it_ends \
    "a park lands on a session a turn is still writing"
target fence-registers-atomically veles-core crates/veles-core/src/store.rs \
    "            if crate::migration::is_parked(n) || crate::migration::is_retired(n) {" \
    "            if false {" \
    a_turn_admitted_by_the_fence_blocks_park_until_it_ends \
    "a turn is admitted into a parked session"

# ── A5: lease loss, on time and recovered ───────────────────────────
target loss-noticed-on-time veles-core crates/veles-core/src/arbiter.rs \
    "        while self.held() || !self.ever_held() {\n            tokio::time::sleep(std::time::Duration::from_secs(1)).await;" \
    "        while self.held() || !self.ever_held() {\n            tokio::time::sleep(std::time::Duration::from_secs(30)).await;" \
    wait_lost_fires_at_the_deadline_not_at_the_next_poll \
    "the old and the new holder run side by side for up to ~25 s"
target duty-restarts veles-core crates/veles-core/src/arbiter.rs \
    "                () = self.wait_lost() => {\n                    tracing::warn!(" \
    "                () = self.wait_lost() => {\n                    return Ok(());\n                    tracing::warn!(" \
    supervise_restarts_the_duty_when_the_lease_comes_back \
    "a duty stopped by a lease blip never starts again"

# ── C2a: replicas complete enough to take a session over ────────────
target fresh-install-backfills veles-core $ss \
    "    if !fresh || backfill_days == 0 \{" \
    "    if true \{" \
    a_fresh_log_carries_the_recent_sessions_whole \
    "a replica holds a session's tail and never its start"
target backfill-window veles-core $ss \
    "(SELECT id FROM sessions WHERE updated_at >= datetime('now', ?1))" \
    "(SELECT id FROM sessions WHERE ?1 IS NOT NULL)" \
    a_fresh_log_carries_the_recent_sessions_whole \
    "a backfill copies the whole store, however old"
target replica-has-session-prompt veles-core $ss \
    "    CREATE TABLE IF NOT EXISTS session_prompt (\n        session_id TEXT PRIMARY KEY," \
    "    CREATE TABLE IF NOT EXISTS session_prompt_gone (\n        session_id TEXT PRIMARY KEY," \
    a_fresh_log_carries_the_recent_sessions_whole \
    "a takeover rebuilds the prompt and the provider's cache goes cold"
target fts-keyed-by-msg-id veles-core $ss \
    "        VALUES (new.msg_id, new.role, new.content, new.session_id, new.msg_id);" \
    "        VALUES (NULL, new.role, new.content, new.session_id, new.msg_id);" \
    a_message_that_arrives_twice_is_one_search_hit \
    "a message indexed twice is listed twice"
target old-index-rebuilt veles-core $ss \
    "    if meta_get(conn, REPLICA_FTS_META_KEY).as_deref() == Some(\"msg_id\") \{" \
    "    if true \{" \
    an_old_replica_index_is_rebuilt_once \
    "a replica from before keeps its doubled index and the old trigger"
target offer-carries-base veles-core $ss \
    "        base: if since_seq < floor \{ floor \} else \{ 0 \}," \
    "        base: 0," \
    a_replica_behind_the_log_start_skips_to_it_and_gets_the_backfill \
    "a peer behind the log's start refuses every offer as a gap, for ever"
target rebase-skips veles-core $ss \
    "    if offer.base > applied_before \{" \
    "    if false \{" \
    a_replica_behind_the_log_start_skips_to_it_and_gets_the_backfill \
    "a peer behind the log's start refuses every offer as a gap, for ever"
target empty-log-keeps-its-floor veles-core $ss \
    "            \"SELECT seq FROM sqlite_sequence WHERE name = 'store_log'\"," \
    "            \"SELECT 0\"," \
    a_replica_behind_the_log_start_skips_to_it_and_gets_the_backfill \
    "a fully pruned log claims to hold everything"
target backfill-once-per-floor veles-core $ss \
    "    meta_put(store.conn(), &key, &floor.to_string())?;\n    Ok(Some(r))" \
    "    Ok(Some(r))" \
    a_replica_behind_the_log_start_skips_to_it_and_gets_the_backfill \
    "a replica that keeps refusing gets the recent sessions appended every tick"
target turn-end-nudges veles-core crates/veles-core/src/agent.rs \
    "        crate::storesync::nudge(&self.1);\n" \
    "" \
    a_turn_end_nudges_the_store_push \
    "a replica lags the conversation by up to a gossip tick"

# ── C2b: takeover, and the owner's return ───────────────────────────
set tk crates/veles-core/src/takeover.rs
set ar crates/veles-core/src/arbiter.rs
target takeover-needs-dead-owner veles-core $ar \
    "            Ok(Some((_, exp))) if exp > now => \{" \
    "            Ok(Some((_, exp))) if exp > now && false => \{" \
    a_takeover_grant_waits_for_the_owners_node_lease \
    "a survivor takes over the sessions of a node that is alive"
target takeover-needs-failover-on veles-core $ar \
    "            Ok(None) => \{\n                return LeaseState::Refused(format!(\n                    \"node \{owner\} has never held a node lease" \
    "            Ok(None) if false => \{\n                return LeaseState::Refused(format!(\n                    \"node \{owner\} has never held a node lease" \
    a_takeover_grant_waits_for_the_owners_node_lease \
    "a node that never joined failover has its sessions taken"
target node-lease-is-its-own veles-core $ar \
    "        if node != holder \{" \
    "        if false \{" \
    a_takeover_grant_waits_for_the_owners_node_lease \
    "a peer holds another node's liveness lease and keeps it 'alive'"
target takeover-grant-outlives-the-owner veles-core $ar \
    "        ttl = crate::takeover::TAKEOVER_TTL_SECS;" \
    "" \
    a_takeover_grant_waits_for_the_owners_node_lease \
    "the grant is gone before the owner returns to check it"
target takeover-records-itself veles-core $tk \
    "                \"INSERT OR REPLACE INTO session_takeovers" \
    "                \"INSERT OR IGNORE INTO session_takeovers_x" \
    a_takeover_continues_from_the_replica_with_the_owners_prompt \
    "the owner never learns, and two nodes keep writing one session"
target replica-blobs-not-read veles-core $mg \
    "                    if !read_blobs \{" \
    "                    if false \{" \
    a_fork_from_the_replica_takes_a_new_id_and_leaves_the_original \
    "a takeover reads a same-named LOCAL file as the dead node's checkpoint"
target owner-checks-the-ticket veles-core $tk \
    "        Ok(_) => Verdict::Yield,\n        Err(e) => Verdict::Refuse(format!(\"\{e:#\}\"))," \
    "        Ok(_) => Verdict::Yield,\n        Err(_) => Verdict::Yield," \
    a_takeover_ticket_may_be_signed_on_the_survivor \
    "the owner gives its session to any node that claims a ticket"
target owner-keeps-the-tail veles-core $tk \
    "    if tail == 0 \{" \
    "    if true \{" \
    the_owner_yields_and_keeps_what_it_wrote_as_a_fork \
    "what the owner wrote while apart is tombstoned with the rest"
target settled-once veles-core $tk \
    "            if !seen \{" \
    "            if true \{" \
    the_owner_yields_and_keeps_what_it_wrote_as_a_fork \
    "the owner forks the same session again on every pass"
target unknown-asks-again veles-core $tk \
    "        Verdict::Unknown(why) => \{\n            return Ok(" \
    "        Verdict::Unknown(why) => \{\n            mark(store.conn(), \"unknown\")?;\n            return Ok(" \
    a_claim_that_does_not_hold_keeps_the_session \
    "an arbiter that was briefly unreachable settles a real takeover as refused, for ever"
target fork-takes-new-ids veles-core $tk \
    "    migration::rename_sessions(&mut bundle, &ids);\n    let root = ids\[&plan.root\].clone();" \
    "    let root = ids\[&plan.root\].clone();" \
    a_fork_from_the_replica_takes_a_new_id_and_leaves_the_original \
    "a fork overwrites the session it copies"
target fence-while-lost veles-core $tk \
    "    if !fence || g.held() \{" \
    "    if !fence \{" \
    the_fence_holds_turns_only_while_the_lease_is_lost \
    "the fence stops a node that holds its lease"

# ── C3: a session is taken over when someone talks to it ────────────
target lazy-needs-failover-on veles-core $tk \
    "    if !cfg.cluster.failover \{" \
    "    if false \{" \
    talking_to_a_dead_nodes_session_takes_it_over \
    "a node that never opted into failover takes other nodes' sessions"
target lazy-acts-on-elsewhere veles-core $tk \
    "        Whereabouts::Here | Whereabouts::Unknown => return Ok(Brought::Here)," \
    "        _ => return Ok(Brought::Here)," \
    talking_to_a_dead_nodes_session_takes_it_over \
    "a dead node's session stays dead until someone runs a command"
target whereabouts-follows-tombstone veles-core $tk \
    "            Some(node) => Whereabouts::Moved \{ node \}," \
    "            Some(_) => Whereabouts::Here," \
    whereabouts_follow_the_tombstone_and_the_replicas \
    "a migrated session reads as local and the fence refuses every turn"
target desktop-names-the-owner veles-frontends crates/veles-frontends/src/desktop_api.rs \
    "                StatusCode::CONFLICT,\n                Json(json!(\{\n                    \"error\": format!(\n                        \"this session lives on node" \
    "                StatusCode::ACCEPTED,\n                Json(json!(\{\n                    \"error\": format!(\n                        \"this session lives on node" \
    chat_for_a_session_that_lives_elsewhere_names_its_node \
    "the desktop starts a turn the fence refuses"
target chat-leaves-a-moved-session veles-frontends crates/veles-frontends/src/gateway.rs \
    "            self.session_cfg(),\n            &agent.session_id,\n        )\n        .await" \
    "            self.session_cfg(),\n            \"no-such-session\",\n        )\n        .await" \
    a_chat_whose_session_moved_away_continues_in_a_new_one \
    "a chat whose session moved is refused on every message"

# ── D1/D2: re-homed tools, and what a peer may run unasked ───────────
set rt crates/veles-core/src/remotetool.rs
target d-cap-role veles-core $rt \
    "        role: carried.role.min(Role::User)," \
    "        role: carried.role," \
    a_callers_level_is_capped_at_user \
    "a peer's owner runs here as this node's owner"
target d-cap-caps veles-core $rt \
    "    let mut allowed = Role::User.default_caps();" \
    "    let mut allowed = Role::Owner.default_caps();" \
    a_callers_level_is_capped_at_user \
    "a capped caller keeps capabilities above the user level"
target d-host-read-jail veles-core $rt \
    "        return match host_read_denial(&ask.tool, &ask.args) \{" \
    "        return match None::<&str> \{" \
    host_reads_stop_at_the_secrets \
    "a peer reads this node's keys and config unasked"
target d-search-root veles-core crates/veles-core/src/approval.rs \
    "    if home.starts_with(&canon) \{" \
    "    if false \{" \
    host_reads_stop_at_the_secrets \
    "a peer's search sweeps the whole home directory"
target d-gate-unasked veles-core $rt \
    "    if let Some(denial) = crate::acl::gate_denial(&ask.tool, cfg, &p) \{" \
    "    if let Some(denial) = None::<String> \{" \
    the_user_level_runs_unasked_and_more_needs_a_ticket \
    "a peer runs admin tools without a ticket"
target d-origin-bound veles-core $rt \
    "        if !ask.from_node.is_empty() && ask.from_node != bound \{" \
    "        if false \{" \
    a_spoofed_origin_is_refused \
    "a peer speaks for another node"
target d-ticket-spent veles-core $rt \
    "    crate::ticket::spend(store, &v, now)?;" \
    "    let _ = (store, &v);" \
    a_ticket_raises_one_call_once \
    "one ticket runs a call again and again"
target d-delegate-capped veles-core $rt \
    "        return Ok(Some(capped(&carried, caller)));" \
    "        return Ok(None);" \
    a_remote_subagent_is_capped_unless_ticketed \
    "a remote subagent runs as this node's owner"
target d-execute-principal veles-core $rt \
    "            let sid = scoped(p.role);\n            crate::acl::set_session_principal(&sid, p.clone());" \
    "            let sid = scoped(p.role);" \
    execute_runs_the_decided_tool_as_decided \
    "an unasked call runs with no principal — as the owner"
target d-stays-home veles-core $rt \
    "    if STAYS_HOME.contains(&tool) \{\n        return None;\n    \}\n    let rows" \
    "    let rows" \
    a_tool_goes_where_the_most_specific_rule_says \
    "a session's todos and memory are sent to another node"
target d-rehome-dispatch veles-core crates/veles-core/src/tools.rs \
    "            if let Some(node) = self.home_of(&inner) \{" \
    "            if let Some(node) = None::<String> \{" \
    a_homed_tool_goes_to_its_node_and_never_quietly_runs_here \
    "a tool homed on the laptop quietly runs here"
target d-rehome-envelope veles-core crates/veles-core/src/tools.rs \
    "        let envelope = target == \"tool_call\";" \
    "        let envelope = false;" \
    a_homed_tool_goes_to_its_node_and_never_quietly_runs_here \
    "a deferred tool (web, browser) reached through tool_call is never re-homed"
target d-shape-gate veles-core crates/veles-core/src/tools.rs \
    "                if let Some(denial) = self.gate(&target) \{\n                    return denial;\n                \}\n                if let Some(t) = self.tools.get(&target).cloned() \{" \
    "                if let Some(t) = self.tools.get(&target).cloned() \{" \
    a_shape_alias_is_gated_on_the_tool_it_runs \
    "a guest writes files through str_replace_editor"
target d-queue-held veles-core $mg \
    "                 VALUES (?1, ?2, ?3, 'held', ?4)\"," \
    "                 VALUES (?1, ?2, ?3, 'pending', ?4)\"," \
    adopt_lands_everything_stamps_the_node_and_maps_msg_ids \
    "a shipped session's queued message runs here as a turn by itself"
target d-a2a-task-peers veles-frontends crates/veles-frontends/src/a2a_server.rs \
    "                if !cfg.cluster.task_peers.iter().any(|p| p == peer) \{\n                    json!(\{" \
    "                if false \{\n                    json!(\{" \
    a_remote_tool_call_runs_at_the_user_level_or_asks_for_a_ticket \
    "any authenticated peer runs tools here"
target d-a2a-delegate-cap veles-frontends crates/veles-frontends/src/a2a_server.rs \
    "                        let principal = match self.delegate_principal(peer, &ask) \{\n                            Ok(p) => p," \
    "                        let principal = match self.delegate_principal(peer, &ask) \{\n                            Ok(_) => ask.principal.clone()," \
    a_remote_child_runs_under_the_callers_principal \
    "a peer's delegation runs here as this node's owner"

# ── 2026-09-30: what the user saw after the round's build ────────────
target notes-bus-filter veles-frontends crates/veles-frontends/src/desktop_api.rs \
    "is_some_and(|n| bus_notes.contains(&n))" \
    "is_some_and(|n| bus_notes.contains(&n) && false)" \
    a_note_the_bus_delivered_is_not_sent_again_when_the_stream_goes_quiet \
    "every note of a turn is sent again when the stream goes quiet, and painted twice"
target notes-after-result veles-core crates/veles-core/src/agent.rs \
    "                                on_event(StreamEvent::ToolEnd(name.to_string(), result.clone()));\n                                let tool_msg = ChatMessage::tool_result(&id, name, result);\n                                self.push_persist(history, tool_msg);\n                                self.emit_parked_notes(&mut on_event);" \
    "                                self.emit_parked_notes(&mut on_event);\n                                on_event(StreamEvent::ToolEnd(name.to_string(), result.clone()));\n                                let tool_msg = ChatMessage::tool_result(&id, name, result);\n                                self.push_persist(history, tool_msg);" \
    a_note_raised_during_a_call_follows_its_result_row \
    "a note about a call lands between its ⚙ line and its result, and ahead of the round on a reload"
target alias-through-gates veles-core crates/veles-core/src/agent.rs \
    "                    target = redirected;\n                    shaped = Some((converted.to_string(), notes));" \
    "                    let _ = (&mut target, &mut shaped);\n                    let mut out = self.tools.dispatch(&redirected, converted).await;\n                    if !notes.is_empty() \{\n                        out = format!(\"\{\}\\n\{out\}\", notes.join(\"; \"));\n                    \}\n                    return CallOutcome::Ran(self.apply_output_cap(name, out));" \
    plan_mode_refuses_an_editor_alias_create \
    "an editor-alias create writes the file past plan mode, hooks and the approval prefilter"
target memory-write-turn veles-core crates/veles-core/src/memory.rs \
    "        let turn = WRITE_TURN.lock().unwrap_or_else(|p| p.into_inner());\n        Ok((turn, self.db()?))" \
    "        static ELSEWHERE: std::sync::Mutex<()> = std::sync::Mutex::new(());\n        let turn = ELSEWHERE.lock().unwrap_or_else(|p| p.into_inner());\n        Ok((turn, self.db()?))" \
    a_write_waits_for_this_process_s_turn \
    "memory writers in one process race SQLite's polling busy handler and starve"
target reload-stamps veles-core crates/veles-core/src/transcript.rs \
    "    if time.is_empty() \{\n        return;\n    \}\n    for b in t.blocks.iter_mut().skip(from) \{" \
    "    if time.is_empty() || from < usize::MAX \{\n        return;\n    \}\n    for b in t.blocks.iter_mut().skip(from) \{" \
    a_reloaded_row_keeps_its_own_time \
    "a reopened session stamps every call with the moment of the reload"

# ── S: the headless signer ───────────────────────────────────────────
set sg crates/veles-signer/src
target s-owner-button veles-signer $sg/telegram.rs \
    "                let reply = if q\[\"from\"\]\[\"id\"\].as_i64() != Some(owner_id) \{" \
    "                let reply = if q\[\"from\"\]\[\"id\"\].as_i64().is_none() \{" \
    the_owner_signs_with_the_button_and_nobody_else_can \
    "anyone who can press the bot's button signs a ticket"
target s-owner-account veles-signer $sg/irc_leg.rs \
    "                            if account.as_deref() != Some(want.as_str()) \{" \
    "                            if account.is_none() && want.is_empty() \{" \
    on_irc_only_the_owners_account_signs \
    "whoever takes the owner's nick signs a ticket"
target s-gateway-token veles-signer $sg/lib.rs \
    "        .find(|(_, t)| veles_core::authn::ct_eq(t, presented))" \
    "        .find(|(_, t)| !t.is_empty() || veles_core::authn::ct_eq(t, presented))" \
    an_unknown_gateway_token_is_refused \
    "any process that can reach the signer puts questions to the owner"
target s-gateway-asks-signers veles-core crates/veles-core/src/remotetool.rs \
    "    let urls = cfg.cluster.signers.clone();" \
    "    let urls: Vec<String> = Vec::new();\n    let _ = &cfg.cluster.signers;" \
    a_signer_answers_a_ticket_ask_nobody_here_can \
    "a session nobody is attached to (cron, veles tool) can never get a ticket"

# ── E: bot challenges put to a person ────────────────────────────────
set ch crates/veles-core/src/challenge.rs
set da crates/veles-frontends/src/desktop_api.rs
target e-relay-hook veles-core crates/veles-core/src/tools.rs \
    "            && crate::challenge::marked(&out)" \
    "            && crate::challenge::marked(&out)\n            && false" \
    a_challenge_goes_to_the_person_and_their_click_gets_the_page \
    "a page behind a challenge goes back to the model as the challenge; nobody is asked"
target e-marker-head veles-core $ch \
    "        Some(l) if l.starts_with(MARKER) => true," \
    "        Some(_) if result.lines().any(|l| l.starts_with(MARKER)) => true," \
    only_the_head_of_a_result_is_a_marker \
    "a page that prints the marker in its own text starts a relay"
target e-marker-second-line veles-core $ch \
    "        Some(l) if l.starts_with('\[') && l.contains(\" ran on node \") && l.ends_with('\]') => {" \
    "        Some(_) => {" \
    only_the_head_of_a_result_is_a_marker \
    "a page's title on the second line of a result starts a relay"
target e-credit-user veles-core $ch \
    "            return if answered {" \
    "            return if true || answered {" \
    a_gate_that_clears_by_itself_is_not_credited_to_the_user \
    "a challenge that cleared on its own is reported to the model as solved by the user"
target e-page-exemption veles-frontends $da \
    "req.uri().path().starts_with(\"/challenge/\")" \
    "req.uri().path().starts_with(\"/\")" \
    the_challenge_page_answers_its_ask_and_nothing_else \
    "the relay page's exemption from the bearer token covers the whole API"
target e-page-answer veles-frontends $da \
    "        (None, Some(t)) if veles_core::challenge::Action::parse(t).is_some() => AskAnswer::text(t)," \
    "        (None, Some(t)) => AskAnswer::text(t)," \
    the_challenge_page_answers_its_ask_and_nothing_else \
    "whoever holds the link answers the ask with any text, not only an action on the page"
target e-extract-refusal veles-core crates/veles-core/src/webtools.rs \
    "    if let Some(c) = crate::challenge::in_response(status, headers, &body) \{" \
    "    if let Some(c) = crate::challenge::in_response(status, headers, &body).filter(|_| false) \{" \
    a_challenge_page_is_refused_with_the_way_through \
    "web_extract hands the model the challenge page as if it were the page"
target e-reply-number veles-core crates/veles-core/src/ask.rs \
    "            .and_then(|i| req.choices.get(i))" \
    "            .and_then(|i| req.choices.get(i + 100))" \
    a_challenge_is_answered_in_words_or_by_its_numbers \
    "a chat user's \"1\" beside \"1) solved\" goes to the model as a new message"
target e-typed-verdict veles-core $ch \
    "                .and_then(|t| crate::ask::parse_reply(&asked, t))" \
    "                .and_then(|t| crate::ask::parse_reply(&asked, t).filter(|_| false))" \
    a_typed_verdict_is_the_verdict \
    "\"give up\" typed in the TUI's text row is \"not something to do on the page\" and asked again"

# ── harness ──────────────────────────────────────────────────────────

proc green {pkg t} {
    set out [open "|cargo test -q -p $pkg --lib $t 2>@1" r]
    set text [read $out]
    if {[catch {close $out}]} {
        return [list 0 $text]
    }
    set ran [regexp {([1-9][0-9]*) passed} $text]
    return [list [expr {[string match "*test result: ok*" $text] && $ran}] $text]
}

proc slurp {path} {
    set fh [open $path r]; set s [read $fh]; close $fh; return $s
}

puts "== review mutation harness (2026-09-29) =="
cd $repo

# `tclsh mutcheck-2026-09-29-review.tcl name …` runs only those targets.
if {$argc > 0} {
    set keep [dict create]
    foreach n $argv { dict set keep $n [dict get $targets $n] }
    set targets $keep
}

# Dry check: every pattern occurs exactly once.
set bad 0
foreach name [dict keys $targets] {
    set m [dict get $targets $name]
    set text [slurp [file join $repo [dict get $m file]]]
    set finds [list [dict get $m find]]
    if {[dict exists $m extra]} { lappend finds [lindex [dict get $m extra] 0] }
    foreach find $finds {
        set n 0; set at 0
        while {[set i [string first $find $text $at]] >= 0} { incr n; set at [expr {$i + 1}] }
        if {$n != 1} {
            puts "DRYCHECK   $name — pattern occurs $n times (want 1)"
            incr bad
        }
    }
}
if {$bad} { puts "$bad pattern(s) fail the dry check — fix the harness first."; exit 1 }
puts "dry check: every pattern occurs exactly once."

set pre 0
foreach name [dict keys $targets] {
    set m [dict get $targets $name]
    lassign [green [dict get $m pkg] [dict get $m test]] ok text
    if {!$ok} { puts "PREFLIGHT-RED $name ([dict get $m test])"; incr pre }
}
if {$pre} { puts "$pre target test(s) red BEFORE any mutation — fix first."; exit 1 }
puts "preflight: all target tests green."

set red 0; set esc 0; set nocompile 0
foreach name [dict keys $targets] {
    set m [dict get $targets $name]
    set path [file join $repo [dict get $m file]]
    set orig [slurp $path]
    set map [list [dict get $m find] [dict get $m replace]]
    if {[dict exists $m extra]} { lappend map {*}[dict get $m extra] }
    set fh [open $path w]
    puts -nonewline $fh [string map $map $orig]
    close $fh
    lassign [green [dict get $m pkg] [dict get $m test]] ok text
    if {[string match "*error\[*" $text] && ![string match "*test result:*" $text]} {
        puts "NOCOMPILE  $name"
        incr nocompile
    } elseif {!$ok} {
        puts "RED        $name — [dict get $m why]"
        incr red
    } else {
        puts "GREEN-ESC  $name — [dict get $m why]  << UNDETECTED"
        incr esc
    }
    set fh [open $path w]; puts -nonewline $fh $orig; close $fh
}

set total [dict size $targets]
puts ""
puts "targets: $total  red: $red  green-escapes: $esc  nocompile: $nocompile"
if {$esc || $nocompile || $red != $total} { puts "INCOMPLETE"; exit 1 }
puts "every mutation detected."
