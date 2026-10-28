//! The IRC side of the trigger engine: observe channel events, feed
//! the engine, relay what comes back.
//!
//! Where this hooks (all inside `client.rs`):
//!
//!  - **TEXT** — at the mention-gate early return in `handle_privmsg`.
//!    A line the bot was NOT addressed by is exactly the line a
//!    trigger exists for: link titles, timtom, greetings react to the
//!    room, not to the bot. Addressed lines never reach the hook; they
//!    are a turn, and the turn is the user's.
//!  - **JOIN/PART/QUIT/KICK/NICK** — in `handle_incoming`'s arms, next
//!    to the roster updates.
//!
//! Rules enforced here, each pinned by a test:
//!  - never on our own lines (the own-echo guard sits above us for
//!    TEXT; the JOIN arms check `current_nick` themselves);
//!  - never for a banned sender (`may_speak_to_the_model` is the
//!    existing verdict — reused, not re-derived);
//!  - never on a line the bare-`tcl` arm will answer
//!    (`is_ours_by_convention`) — linkresolver skips `tcl ` on its
//!    side, and doing it on ours too keeps the rule legible in one
//!    place;
//!  - only in SANCTIONED channels (the hook is below the sanctioned
//!    gate for TEXT; the event arms check it directly);
//!  - the derived per-channel mask decides whether a Slopdrop forward
//!    is spent at all.
//!
//! Everything that can refuse is recorded in `trigger_fires` with its
//! reason: the log is the answer to "why didn't it react".
#![allow(private_interfaces)] // `Conn` is deliberately private to the adapter

use crate::client::Conn;
use veles_core::acl::Role;
use veles_core::config::VelesConfig;
use veles_core::triggers::{self, ActionKind, Event, Outcome};

/// Process-wide engine state: the brakes. One per process, not per
/// network — the brakes key on (trigger, channel) and a channel name
/// collision across networks is what the network field on the row is
/// for.
///
/// The MASK cache used to live here too, and that was the bug: nothing
/// in this crate could write it from `/triggers refresh` (commands.rs,
/// in veles-core, cannot reach across into veles-irc), so the cache
/// stayed empty and every Slopdrop forward was refused with "mask: event
/// not wanted" — while the refresh command printed the correct masks
/// because it derived into a local and reported that. The cache now
/// lives in `veles_core::triggers`, where both the reader (here) and the
/// writer (the refresh command) can reach it. Only the brakes — read and
/// written solely inside `evaluate` on this side — stay local.
pub struct Runtime {
    brakes: triggers::Brakes,
}

fn runtime() -> &'static std::sync::Mutex<Runtime> {
    static R: std::sync::OnceLock<std::sync::Mutex<Runtime>> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        std::sync::Mutex::new(Runtime {
            brakes: triggers::Brakes::default(),
        })
    })
}

/// Is the whole feature on? `[triggers] enabled = true`. It also used
/// to need the slopdrop bridge loaded, which switched vision and Bluesky
/// rows off with it; since the bridge is a service (`triggers.forward`,
/// 2026-10-10) a Slopdrop row with no provider is refused by itself, with
/// the reason, and the other rows fire.
pub fn feature_on(cfg: &VelesConfig) -> bool {
    cfg.triggers.enabled
}

/// Ensure the mask cache is fresh. The cache itself lives in
/// veles_core::triggers (see `Runtime` above); the adapter asks it to
/// refresh from `run`, the single funnel both hooks share, BEFORE the
/// evaluate/brakes lock block — the refresh is an async HTTP call and
/// must never hold the store mutex across its await. It had zero call
/// sites in the first version, which is the bug the move-to-core closes.
const MASK_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// The BRIDGE budget — `[triggers] max_per_minute`, per (network,
/// channel). Called where the forward is about to be SPENT, which is
/// after the mask: the interpreter on the far side is single-threaded,
/// so what has to be bounded is forwards per channel, whatever mix of
/// rows produced them.
///
/// This knob had ZERO readers when it was written: config said 30/min,
/// nothing counted, and the live `slop-flashsupport` row (TEXT, always,
/// no cooldown, no cap) turned every unaddressed line in the channel
/// into an unthrottled POST. A documented brake nobody reads is worse
/// than none — it is a promise the operator budgets against.
///
/// A poisoned brake REFUSES: the safe direction here is the one that
/// skips a forward and logs why, never the one that spends unbounded.
fn claim_bridge(cfg: &VelesConfig, ev: &Event<'_>, now: i64) -> Result<(), String> {
    match runtime().lock() {
        Ok(mut r) => {
            r.brakes
                .bridge_claim(ev.network, ev.channel, now, cfg.triggers.max_per_minute)
        }
        Err(_) => Err("brakes unavailable".into()),
    }
}

/// Record the row's own cooldown/cap spend. `evaluate` only INSPECTS
/// the brakes (it runs before the mask and before the action), so the
/// clock advances here, at the point something is actually spent — a
/// masked-out event or a budget refusal must not burn a slot for a fire
/// that never happened and refuse the next real one for it.
fn commit_brake(t: &triggers::Trigger, channel: &str, now: i64) {
    if let Ok(mut r) = runtime().lock() {
        r.brakes.commit(t, channel, now);
    }
}

fn mask_allows(network: &str, channel: &str, event: &str) -> bool {
    // Delegates to the core cache — the single source of truth that
    // `/triggers refresh` also writes, so the hot path and the command
    // can no longer disagree about what the mask holds.
    triggers::mask_allows(network, channel, event)
}

/// The TEXT hook. Returns replies to send (already deduped, ordered);
/// the caller sends them with the same pass-through formatting the
/// bare-tcl arm uses.
pub async fn observe_text(
    conn: &Conn,
    cfg: &VelesConfig,
    network: &str,
    channel: &str,
    nick: &str,
    mask: &str,
    text: &str,
) -> Vec<String> {
    if !feature_on(cfg) {
        return Vec::new();
    }
    let Some(store) = crate::client::parked_store_for(Some(network)) else {
        return Vec::new();
    };
    // The banned verdict — same one the model path uses.
    let sender_ctx = veles_core::acl::SenderContext {
        network: Some(network.to_string()),
        platform: Some("irc".into()),
        channel: Some(channel.to_string()),
        chat_id: Some(channel.to_string()),
        direct: false,
        handle: None,
        post: None,
        mask: Some(mask.to_string()),
    };
    if !conn.may_speak_to_the_model(None, &sender_ctx) {
        return Vec::new();
    }
    // A line the bare-tcl arm will answer is not channel chatter.
    if crate::client::is_ours_by_convention_public(text) {
        return Vec::new();
    }
    let role = resolve_role(conn, &sender_ctx);
    let ev = Event {
        platform: "irc",
        network,
        channel,
        event: "TEXT",
        nick,
        mask,
        text,
        role,
    };
    run(cfg, &store, &ev).await
}

/// The event hook (JOIN/PART/QUIT/KICK/NICK). Channel is "* " for
/// QUIT/NICK, which have none — same convention the trigger engine's
/// channel glob uses.
#[allow(clippy::too_many_arguments)]
pub async fn observe_event(
    conn: &Conn,
    cfg: &VelesConfig,
    network: &str,
    event: &str,
    nick: &str,
    mask: &str,
    channel: &str,
    text: &str,
) -> Vec<String> {
    if !feature_on(cfg) {
        return Vec::new();
    }
    // QUIT/NICK are network-wide: they arrive with channel "*", which
    // the trigger engine's own glob reads as "any channel". The value
    // passes through unchanged — an earlier `if channel == "*" { "*" }
    // else { channel }` here was an identity dressed as a decision.
    let chan = channel;
    let Some(store) = crate::client::parked_store_for(Some(network)) else {
        return Vec::new();
    };
    let sender_ctx = veles_core::acl::SenderContext {
        network: Some(network.to_string()),
        platform: Some("irc".into()),
        channel: if chan == "*" {
            None
        } else {
            Some(chan.to_string())
        },
        chat_id: if chan == "*" {
            None
        } else {
            Some(chan.to_string())
        },
        direct: false,
        handle: None,
        post: None,
        mask: Some(mask.to_string()),
    };
    if !conn.may_speak_to_the_model(None, &sender_ctx) {
        return Vec::new();
    }
    let role = resolve_role(conn, &sender_ctx);
    let ev = Event {
        platform: "irc",
        network,
        channel: chan,
        event,
        nick,
        mask,
        text,
        role,
    };
    run(cfg, &store, &ev).await
}

fn resolve_role(conn: &Conn, ctx: &veles_core::acl::SenderContext) -> Option<Role> {
    let acl = veles_core::acl::live_config();
    if acl.is_configured() {
        Some(veles_core::acl::Principal::resolve_ctx(None, ctx, &acl).role)
    } else if conn.cfg.allowed_users.is_empty() {
        Some(Role::Owner) // open mode
    } else {
        None
    }
}

async fn run(
    cfg: &VelesConfig,
    store: &std::sync::Arc<std::sync::Mutex<veles_core::store::Store>>,
    ev: &Event<'_>,
) -> Vec<String> {
    // It took `conn` and `_text` only to discard them: both hooks have
    // already judged the sender (`may_speak_to_the_model`, the role is
    // on `ev`) and `ev.text` is the text. A parameter whose body is
    // `let _ = it` is a claim that it matters here. It does not.
    // Refresh the derived mask BEFORE the lock block below. This is an
    // async HTTP call to slopdrop, so it must run with no store guard
    // held: a MutexGuard alive across an await both risks a deadlock
    // (the one that froze the gateway on 2026-09-14) and makes the
    // caller's future non-Send. TTL-gated inside, so this is one HTTP
    // call per ten minutes, not per line.
    //
    // It is CALLED here, which is the whole fix: `ensure_masks` existed
    // in the first version with zero call sites, so the mask cache was
    // never populated and every Slopdrop row was refused as
    // "event not wanted by this channel".
    veles_core::triggers::ensure_masks_fresh(cfg, store, MASK_TTL).await;
    let now = chrono::Utc::now().timestamp();
    let decisions = {
        let s = match store.lock() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let b = match runtime().lock() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        // Two locks held at once (store + runtime) — both are local
        // mutexes taken for microseconds, and the engine needs the
        // rows AND the brakes together to decide anything. `evaluate`
        // only READS the brakes; the spend is committed below, per
        // outcome, where it is actually made.
        triggers::evaluate(&s, &b.brakes, ev, now)
    };
    // Bluesky rows run FIRST, whatever their row order: the links the
    // bot previews are named to every Slopdrop forward of this event, so
    // slopdrop's linkresolver leaves them alone instead of answering a
    // second time. A link the bot could not read is not named, and stays
    // slopdrop's — the answer it got before the bot could read links.
    let (bluesky, others): (Vec<_>, Vec<_>) = decisions
        .into_iter()
        .partition(|(_, o)| matches!(o, Ok(Outcome::Bluesky(_))));
    let mut answered: Vec<String> = Vec::new();
    let mut replies = Vec::new();
    for (t, outcome) in bluesky.into_iter().chain(others) {
        let (status, detail) = match &outcome {
            Ok(o) => match o {
                Outcome::Bluesky(links) => {
                    // No bot on this node: nothing is spent, and the
                    // link is slopdrop's as it always was.
                    if !veles_core::bskylink::bot_available() {
                        (
                            "fallback".to_string(),
                            Some(veles_core::bskylink::NO_BOT.to_string()),
                        )
                    } else {
                        // Not the bridge, so the bridge budget does not
                        // apply; the row's own cooldown and cap do.
                        commit_brake(&t, ev.channel, now);
                        match veles_core::bskylink::preview(links).await {
                            Ok((previews, missed)) => {
                                let fired = !previews.is_empty();
                                for p in previews {
                                    answered.push(p.url);
                                    replies.push(p.line);
                                }
                                let detail = (!missed.is_empty()).then(|| missed.join("; "));
                                if fired {
                                    ("fired".to_string(), detail)
                                } else {
                                    ("fallback".to_string(), detail)
                                }
                            }
                            Err(e) => ("fallback".to_string(), Some(e)),
                        }
                    }
                }
                Outcome::Slopdrop(_) => {
                    // Two gates in front of the one HTTP call, in the
                    // order they cost: the derived MASK (does this
                    // channel want this event at all) and then the
                    // per-channel bridge BUDGET. Both checked HERE,
                    // after the match, so each refusal is logged with
                    // its own reason rather than silently not
                    // forwarding.
                    if !veles_core::triggers::forwarder_configured() {
                        (
                            "refused".to_string(),
                            Some(veles_core::triggers::NO_FORWARDER.to_string()),
                        )
                    } else if !mask_allows(ev.network, ev.channel, ev.event) {
                        (
                            "refused".to_string(),
                            Some("mask: event not wanted by this channel".into()),
                        )
                    } else if let Err(why) = claim_bridge(cfg, ev, now) {
                        ("refused".to_string(), Some(why))
                    } else {
                        // Past both gates: this forward is being spent,
                        // so the row's own cooldown/cap clock advances
                        // now and not before.
                        commit_brake(&t, ev.channel, now);
                        match forward_slopdrop(ev, &answered).await {
                            Ok(pairs) => {
                                for (chan, msg) in pairs {
                                    // The bridge's channel is slopdrop's
                                    // own spelling of ours; reply to
                                    // where the line landed.
                                    let _ = chan;
                                    replies.push(msg);
                                }
                                ("fired".to_string(), None)
                            }
                            Err(e) => ("error".to_string(), Some(e)),
                        }
                    }
                }
                Outcome::Vision { url, question } => {
                    // The vision route is not the bridge, so the bridge
                    // budget does not apply — but the row's cooldown and
                    // hourly cap do, and this is where they are spent.
                    commit_brake(&t, ev.channel, now);
                    match vision_describe(cfg, ev, url, question).await {
                        Ok(desc) => {
                            replies.push(desc);
                            ("fired".to_string(), None)
                        }
                        Err(e) => ("error".to_string(), Some(e)),
                    }
                }
                Outcome::Prompt(_) | Outcome::Command(_) => {
                    // Future work: these actions need a session, which
                    // is the gateway's, not the adapter's. Recorded so
                    // the log says why nothing happened.
                    (
                        "unsupported".to_string(),
                        Some("prompt/command actions are not wired yet".into()),
                    )
                }
            },
            Err(reason) => ("refused".to_string(), Some(reason.clone())),
        };
        if let Ok(s) = store.lock() {
            let _ = s.trigger_record_fire(t.id, ev.channel, &status, detail.as_deref());
        }
    }
    replies
}

/// Forward one event to whoever provides `triggers.forward` (the
/// slopdrop module): the network mapping, the argument shapes and the
/// HTTP call are the provider's; the adapter only hands the event over.
async fn forward_slopdrop(
    ev: &Event<'_>,
    answered: &[String],
) -> Result<Vec<(String, String)>, String> {
    veles_core::triggers::forward(ev, answered).await
}

/// The image a link names, described. Someone other than the owner
/// pasted it: fetched the way their own web calls go (egress.rs), over
/// the node's proxy under a name of the sender's own, never from the
/// node's address.
async fn vision_describe(
    cfg: &VelesConfig,
    ev: &Event<'_>,
    url: &str,
    question: &str,
) -> Result<String, String> {
    let theirs = ev.role != Some(Role::Owner);
    let who = format!("trigger:{}:{}:{}", ev.platform, ev.network, ev.nick);
    veles_core::egress::scope(
        &who,
        theirs,
        veles_core::vision::describe_url(cfg, url, question),
    )
    .await
}

// Keep ActionKind referenced for the match above even under future
// refactors; the compiler would flag a genuinely unused import, this
// just documents the pairing.
const _: fn(ActionKind) -> ActionKind = |k| k;

#[cfg(test)]
mod tests {
    //! Source-scrape tests, the house defence against a function that
    //! COMPILES but is never CALLED — which is exactly how the empty
    //! mask cache shipped. `ensure_masks` existed in the first version
    //! with zero call sites; nothing in the type system objects to an
    //! unused `pub async fn`, so the cache stayed empty and every
    //! Slopdrop forward was refused while the code looked wired. A
    //! scraper over the real sources is what makes that loud.
    //!
    //! These check the WIRING, not the behaviour (the behavioural
    //! round-trip lives in veles-core's triggers.rs tests). They are
    //! textual on purpose: the contract is "this call appears in that
    //! function's body", which is precisely what a compiler will not
    //! enforce for a `pub` item.
    //!
    //! No `use super::*`: this module is purely textual — it needs
    //! nothing from the parent, and an unused-import glob would fail the
    //! workspace's `-D warnings` clippy.

    /// This module's source, EXCLUDING the tests below. `include_str!`
    /// pulls in the test module too, so a `!src.contains("…")` guard
    /// would match its OWN forbidden string and fail against its own
    /// text. The client.rs boundary tests hit exactly that and solved it
    /// the same way; truncating at the `mod tests {` header is the
    /// precedent. If the header is ever renamed this returns the whole
    /// file and a guard could self-match again, which is why the header
    /// name is repeated verbatim in the assertion below the cut.
    fn source() -> &'static str {
        let full = include_str!("triggers_hook.rs");
        match full.find("mod tests {") {
            Some(i) => &full[..i],
            None => full,
        }
    }

    /// `run` — the single funnel both IRC hooks (`observe_text`,
    /// `observe_event`) reach — must call `ensure_masks_fresh` BEFORE
    /// it reads the mask. If it does not, the cache `mask_allows` reads
    /// is never populated from the hot path and the bug returns.
    #[test]
    fn the_hot_path_populates_the_mask_cache_it_reads() {
        let src = source();
        let run_body = fn_body(src, "run");
        assert!(
            run_body.contains("ensure_masks_fresh"),
            "run() must call triggers::ensure_masks_fresh, or the mask \
             cache the Slopdrop arm gates on is never populated. This is \
             the regression that made every forward refuse with \
             \"event not wanted\"."
        );
        // And it must come before the mask is consulted, i.e. before
        // the `evaluate`/`mask_allows` work. A refresh AFTER the gate
        // helps nothing on the first event of a channel's life.
        let ensure_at = run_body.find("ensure_masks_fresh").unwrap();
        let mask_at = run_body.find("mask_allows").unwrap();
        assert!(
            ensure_at < mask_at,
            "ensure_masks_fresh must run before mask_allows reads it: \
             ensure@{ensure_at} mask@{mask_at}"
        );
    }

    /// The bridge budget must be READ from config and spent in `run`,
    /// after the mask. `[triggers] max_per_minute` shipped with zero
    /// readers anywhere in the tree — config said 30/min, nothing
    /// counted, and the live TEXT row forwarded every unaddressed line
    /// unthrottled into a single-threaded interpreter. Nothing in the
    /// type system objects to an unread config field, which is exactly
    /// what a scraper is for.
    ///
    /// Assertions are on CALL SHAPES (`name(`), never bare words: the
    /// arm carries prose that names these functions, and a
    /// `contains("claim_bridge")` check would be satisfied by the
    /// comment while the call itself was gone.
    #[test]
    fn the_bridge_budget_is_read_from_config_and_spent_after_the_mask() {
        let src = source();
        let run_body = fn_body(src, "run");
        assert_eq!(
            run_body.matches("claim_bridge(").count(),
            1,
            "run() must claim the per-channel bridge budget exactly once, \
             or `[triggers] max_per_minute` is a knob nobody reads"
        );
        // The knob itself, not just a function that could ignore it.
        let claim_body = fn_body(src, "claim_bridge");
        assert!(
            claim_body.contains("cfg.triggers.max_per_minute"),
            "claim_bridge must read the configured ceiling, not a constant"
        );
        // Cheapest gate first: the mask is a HashMap read, the budget a
        // vector scan. And a channel that wants nothing must not spend
        // budget deciding so.
        let mask_at = run_body.find("mask_allows").unwrap();
        let claim_at = run_body.find("claim_bridge(").unwrap();
        assert!(
            mask_at < claim_at,
            "the mask is consulted before the budget: mask@{mask_at} claim@{claim_at}"
        );
    }

    /// The row's own cooldown/cap clock advances where something is
    /// SPENT, never in `evaluate` — which runs before both the mask and
    /// the action, so a brake spent there is spent on a fire that may
    /// never happen. Three spending arms, three commits, each downstream
    /// of its gates.
    #[test]
    fn the_rows_brake_is_committed_only_where_a_spend_happens() {
        let src = source();
        let run_body = fn_body(src, "run");
        assert_eq!(
            run_body.matches("commit_brake(").count(),
            3,
            "one commit per spending arm (bluesky preview, slopdrop \
             forward, vision describe) — and none for a refusal or an \
             unsupported action"
        );
        // The Bluesky arm spends only once a bot is known to be here:
        // a node with no bot leaves the link to slopdrop and spends
        // nothing.
        let bot_at = run_body.find("bot_available()").unwrap();
        let bsky_commit = run_body.find("commit_brake(").unwrap();
        assert!(
            bot_at < bsky_commit,
            "the bluesky brake is committed after the bot check: \
             bot@{bot_at} commit@{bsky_commit}"
        );
        let claim_at = run_body.find("claim_bridge(").unwrap();
        let forward_commit = run_body[claim_at..]
            .find("commit_brake(")
            .map(|i| i + claim_at);
        assert!(
            forward_commit.is_some(),
            "the forward's brake is committed after its gates pass, not \
             before: claim@{claim_at}"
        );
        // `evaluate` must take the brakes by SHARED reference: a `&mut`
        // there is how the spend crept upstream of the gates.
        assert!(
            run_body.contains("evaluate(&s, &b.brakes"),
            "evaluate inspects the brakes, it does not spend them"
        );
    }

    /// A link the bot previewed must not be answered again by slopdrop's
    /// linkresolver: the Bluesky rows run before every forward of the
    /// same event (whatever the row order), and the forward is handed
    /// the links they answered.
    #[test]
    fn bluesky_rows_run_first_and_name_their_links_to_the_forward() {
        let src = source();
        let run_body = fn_body(src, "run");
        let split_at = run_body
            .find("matches!(o, Ok(Outcome::Bluesky(_)))")
            .expect("run() must split the Bluesky outcomes out first");
        let loop_at = run_body.find("bluesky.into_iter().chain(others)").unwrap();
        assert!(split_at < loop_at, "split@{split_at} loop@{loop_at}");
        assert!(
            run_body.contains("forward_slopdrop(ev, &answered)"),
            "the forward must carry the answered links"
        );
        assert!(
            fn_body(src, "forward_slopdrop").contains("triggers::forward(ev, answered)"),
            "and pass them on to the triggers.forward service"
        );
    }

    /// `/triggers refresh` (commands.rs) must INSTALL the masks it
    /// derives, not derive into a local and report it. The first version
    /// called the bare `refresh_masks`, printed the correct masks, and
    /// left the cache stale — a command that lies about having done its
    /// job. The fix is `force_refresh_masks`, and the scraper pins that
    /// the refresh arm calls the installing variant.
    ///
    /// Both assertions are about CALL SHAPES (`name(`), never bare
    /// words: the arm carries a long comment explaining the force/bare
    /// distinction, and a `contains("force_refresh_masks")` check was
    /// satisfied by that prose while the actual call had been reverted
    /// — a mutation that survived, caught only because the mutation was
    /// run. Counting `(`-terminated names is immune to the comment.
    #[test]
    fn the_refresh_command_installs_the_masks_it_reports() {
        let src = include_str!("../../veles-frontends/src/commands.rs");
        // The arm is the `"refresh" =>` block of triggers_command_async.
        let after = match src.split_once("\"refresh\" =>") {
            Some((_, after)) => after,
            None => panic!("could not find the /triggers refresh arm in commands.rs"),
        };
        // Take up to the next top-level arm (`"mask" =>`) so the
        // assertion is about THIS arm, not the whole file.
        let arm = match after.find("\"mask\" =>") {
            Some(n) => &after[..n],
            None => after,
        };
        assert_eq!(
            arm.matches("force_refresh_masks(").count(),
            1,
            "the /triggers refresh arm must call force_refresh_masks() \
             exactly once — it installs the derived masks into the \
             shared cache. Zero means the refresh reports masks it never \
             wrote, which is worse than no refresh command at all."
        );
        assert_eq!(
            arm.matches("::refresh_masks(").count(),
            0,
            "the bare refresh_masks() only derives; it must not be the \
             call this arm makes. (force_refresh_masks() does not match \
             this pattern — what precedes `refresh_masks` there is \
             `force_`, not `::`.)"
        );
    }

    /// The cache lives in veles-core, not here — because commands.rs
    /// (core) has to WRITE the same cache this adapter READS, and core
    /// cannot call into veles-irc. Pin that the adapter delegates rather
    /// than keeping a second private copy, which is how the two got out
    /// of sync in the first place.
    #[test]
    fn the_adapter_reads_the_core_cache_not_a_private_copy() {
        let src = source();
        let mask_body = fn_body(src, "mask_allows");
        assert!(
            mask_body.contains("triggers::mask_allows"),
            "mask_allows must delegate to veles_core::triggers::mask_allows \
             so the hot path and /triggers refresh share ONE cache"
        );
        // The old private copy kept masks on the adapter's Runtime
        // struct. If that field ever comes back, the two caches can
        // diverge again — fail loudly.
        assert!(
            !src.contains("masks: std::collections::HashMap<String, triggers::ChannelMask>"),
            "the Runtime struct must not grow a private mask cache again; \
             the single cache lives in veles_core::triggers"
        );
    }

    /// Extract the body of `async fn NAME(` / `fn NAME(` up to its
    /// matching close brace. Crude brace-counting, adequate for these
    /// sources (no braces inside the relevant bodies' string literals in
    /// a way that would unbalance the count). Returns "" if not found,
    /// so a rename of `run` fails the assertion rather than silently
    /// passing on an empty body.
    fn fn_body(src: &str, name: &str) -> String {
        // Match the fn header, allowing an `async ` and generic/arg noise.
        let needle = format!("fn {name}(");
        let start = match src.find(&needle) {
            Some(i) => i,
            None => return String::new(),
        };
        let rest = &src[start..];
        let mut depth = 0i32;
        let mut started = false;
        let mut out = String::new();
        for ch in rest.chars() {
            out.push(ch);
            match ch {
                '{' => {
                    depth += 1;
                    started = true;
                }
                '}' => {
                    depth -= 1;
                    if started && depth <= 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod vision_egress_tests {
    use super::*;

    fn ev(role: Option<Role>) -> Event<'static> {
        Event {
            platform: "irc",
            network: "net",
            channel: "#room",
            event: "TEXT",
            nick: "someone",
            mask: "someone!u@h",
            text: "look http://10.255.255.1/cat.png",
            role,
        }
    }

    /// An image link someone else pasted is fetched the way their own
    /// web calls go (egress.rs): on a node that carries none of their
    /// traffic, not at all. An unknown sender counts as someone else.
    /// The owner's is fetched as the owner's, and judged as a fetch.
    #[tokio::test]
    async fn someone_elses_image_link_is_fetched_like_their_web_calls() {
        let _h = crate::scratch_home();
        let home = std::env::var("VELES_HOME").expect("scratch home");
        std::fs::write(
            format!("{home}/config.toml"),
            "[web]\nothers = \"refuse\"\n\n[vision]\nprovider = \"custom:x\"\nmodel = \"m\"\n",
        )
        .expect("config");
        veles_core::config::invalidate_live();
        let cfg = veles_core::config::live();
        let url = "http://10.255.255.1/cat.png";
        for role in [None, Some(Role::User), Some(Role::Admin)] {
            let e = vision_describe(&cfg, &ev(role), url, "?")
                .await
                .expect_err("refused");
            assert!(e.contains("others = \"refuse\""), "{role:?}: {e}");
        }
        let e = vision_describe(&cfg, &ev(Some(Role::Owner)), url, "?")
            .await
            .expect_err("a private address");
        assert!(
            e.contains("points at") && !e.contains("others = \"refuse\""),
            "the owner's: {e}"
        );
    }
}
