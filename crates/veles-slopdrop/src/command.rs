//! `/slopdrop` — the mirror command and the public `/tcl` ledger, as a
//! module's DIRECT command (PLAN-plugins-installer.md R5): registered
//! in core's command registry under this module's owner, answered by
//! every surface's dispatcher like a builtin, never rendered as a
//! prompt. The whole name is `Cap::Shell` (`save` deploys into a shared
//! interpreter, and the command names host paths); the ledger verbs
//! re-check `Cap::All`.

use std::sync::Arc;

use veles_core::acl::{Cap, Principal};
use veles_core::commands::{CommandBody, DirectCall, DirectCommand, PluginCommand};

use crate::settings::settings;
use crate::withheld::{PostKind, WithheldEntry, WithheldStore};

pub const NAME: &str = "slopdrop";

/// Register the command (the module's `start`; purged with the module).
pub fn register() -> Result<(), String> {
    veles_core::commands::register_command(PluginCommand {
        name: NAME.into(),
        description: "the slopdrop state mirror (sync|status|save) and the public /tcl ledger \
                      (withheld|posts|approve|delete|check)"
            .into(),
        body: CommandBody::Direct(DirectCommand {
            cap: Cap::Shell,
            run: Arc::new(|call| Box::pin(run(call))),
        }),
    })
}

fn caller_is(principal: Option<&Principal>, cap: Cap) -> bool {
    match principal {
        None => true,
        Some(p) => p.has(cap),
    }
}

fn denied(principal: Option<&Principal>, what: &str, cap: Cap) -> String {
    let who = principal
        .map(|p| p.label())
        .unwrap_or_else(|| "this caller".to_string());
    format!(
        "[acl] {what} requires the `{}` capability — denied for {who}.",
        cap.as_str()
    )
}

/// Truncate a string to `cap` CHARS, appending an ellipsis when cut.
fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(cap).collect::<String>())
    }
}

/// The command body. `call.principal` is the resolved caller (`None` on
/// a local owner surface), exactly as the shared dispatcher passed it.
pub async fn run(call: DirectCall) -> String {
    let principal = call.principal.as_ref();
    let rest = call.args.as_str();
    let cfg = settings();
    if !caller_is(principal, Cap::Shell) {
        return denied(principal, "/slopdrop", Cap::Shell);
    }
    let mut parts = rest.split_whitespace();
    let verb = parts.next().unwrap_or("status");
    match verb {
        "sync" => match crate::mirror::sync(&cfg).await {
            Ok(r) => format!(
                "mirror synced: {} procs, {} vars ({} dropped), {} — {}",
                r.procs,
                r.vars,
                r.dropped,
                r.instance,
                r.root.display()
            ),
            Err(e) => format!("slopdrop sync failed: {e}"),
        },
        "save" => {
            let arg = parts.next().unwrap_or_default();
            if arg.is_empty() {
                return "usage: /slopdrop save <path>  — a file under the mirror's procs/ tree"
                    .into();
            }
            let path = match crate::mirror::resolve_save_path(&cfg, arg) {
                Ok(p) => p,
                Err(e) => return format!("save: {e}"),
            };
            let caller = principal.map_or_else(|| "local:owner".to_string(), |p| p.subject.clone());
            match crate::mirror::save(&cfg, &path, &caller).await {
                Ok(r) => match r.commit {
                    Some(c) => format!("deployed '{}' — committed on slopdrop ({c})", r.name),
                    None => format!("deployed '{}' — slopdrop reported no commit info", r.name),
                },
                Err(e) => format!("save refused: {e}"),
            }
        }
        // The public `/tcl` ledger. `withheld`: the replies the local output
        // check (or the bad list) held back, awaiting the owner. `posts`:
        // every public reply of the last 7 days, inline ones included, so
        // any of them can be deleted. Owner-only — approving reposts the
        // body inline on the operator's public account and allowlists it;
        // deleting takes the post/link/blobs down and blocks it for good.
        "withheld" | "posts" => {
            if !caller_is(principal, Cap::All) {
                return denied(principal, &format!("/slopdrop {verb}"), Cap::All);
            }
            let mut list = WithheldStore::load().list();
            if verb == "withheld" {
                list.retain(|e| e.kind == PostKind::Withheld);
            }
            if list.is_empty() {
                let what = if verb == "withheld" {
                    "nothing withheld"
                } else {
                    "no public /tcl replies in the last 7 days"
                };
                return if crate::publish::mover_available() {
                    what.into()
                } else {
                    // Only the node running the atproto plugin records
                    // (and can act on) public replies.
                    format!(
                        "{what} on this node — public /tcl replies live on the node running \
                         the atproto plugin; ask there (Telegram reaches it)"
                    )
                };
            }
            ledger_listing(verb, &list)
        }
        "approve" | "delete" => {
            if !caller_is(principal, Cap::All) {
                return denied(principal, &format!("/slopdrop {verb}"), Cap::All);
            }
            let target = parts.next().unwrap_or_default();
            if target.is_empty() {
                return format!(
                    "usage: /slopdrop {verb} <id | post link | /t/ link | rkey> | /slopdrop approve proc <name>"
                );
            }
            // `/slopdrop approve proc <name>` lifts a proc off the bad list
            // without touching a specific withheld entry.
            if verb == "approve" && target == "proc" {
                let name = parts.next().unwrap_or_default();
                if name.is_empty() {
                    return "usage: /slopdrop approve proc <name>".into();
                }
                let Some(idx) = crate::safety::proc_index(&cfg).await else {
                    return "slopdrop did not answer /api/procs".into();
                };
                let Some(hash) = idx.hash_for_name(name).map(str::to_string) else {
                    return format!("no stored proc is named {name}");
                };
                return match crate::safety::record(&hash, crate::safety::Verdict::Approved) {
                    Ok(()) => format!(
                        "{name} approved — off the bad list; its outputs still go through the check"
                    ),
                    Err(e) => format!("approve failed: {e}"),
                };
            }
            match withheld_act(verb, target).await {
                Ok(msg) => msg,
                Err(e) => format!("{verb} {target}: {e}"),
            }
        }
        "check" => {
            if !caller_is(principal, Cap::All) {
                return denied(principal, "/slopdrop check", Cap::All);
            }
            // `rest` is "<verb> <text…>"; the text is everything past the verb.
            let text = rest.split_once(' ').map(|x| x.1).unwrap_or_default().trim();
            if text.is_empty() {
                return "usage: /slopdrop check <text>".into();
            }
            let hash = crate::check::text_hash(text);
            let allowlisted = crate::safety::allowlisted(&hash);
            let endpoints = cfg.output_classifier.clone();
            let tox = cfg.output_classifier_tox.unwrap_or(0.30);
            let idt = cfg.output_classifier_id.unwrap_or(0.25);
            // Judged as an OUTPUT would be: the text a post would carry.
            let pt = crate::safety::public_text(text);
            let det = crate::check::deterministic(&pt.body);
            let c = crate::check::run_detailed(&endpoints, &pt.body, allowlisted, tox, idt, &[]).await;
            format!(
                "deterministic: {}\nwith the scorer: {}{}",
                if det.pass() {
                    "pass".to_string()
                } else {
                    format!("withheld ({})", det.reason().unwrap_or("?"))
                },
                if c.outcome.pass() {
                    "pass".to_string()
                } else {
                    format!("withheld ({})", c.outcome.reason().unwrap_or("?"))
                },
                match (&c.top, c.ms) {
                    (Some((h, p)), Some(ms)) => format!(" — top {h}={p:.3}, {ms} ms"),
                    _ => String::new(),
                },
            )
        }
        "status" => {
            let st = crate::mirror::status(&cfg);
            if !st.tsv {
                return format!(
                    "no mirror yet — /slopdrop sync builds it under {}",
                    st.root.display()
                );
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut out = format!(
                "slopdrop mirror: {} procs, {} vars — {}\n",
                st.procs,
                st.vars,
                synced_phrase(st.synced_at, now)
            );
            out.push_str(&format!("  {} ({})\n", st.instance, st.root.display()));
            out.push_str(
                "  a VIEW, not slopdrop's repo: /api/rollback on slopdrop makes it stale — \
                 /slopdrop sync regenerates it. Saves here deploy through the interpreter.",
            );
            out
        }
        _ => "usage: /slopdrop sync | status | save <path> | withheld | posts | approve <post> | \
             delete <post> | approve proc <name> | check <text>\n  sync   — rebuild the mirror \
             from slopdrop's live state\n  status — the mirror on this disk (no HTTP)\n  save   — \
             deploy an edited proc file through the interpreter (conflict-checked)\n  withheld — \
             public /tcl replies held back for the owner; posts — every public /tcl reply (7 days)\n  \
             approve|delete <post> — by id, post link, /t/ link or rkey (owner)\n  check  — what \
             the public output check says of a text (owner)"
            .into(),
    }
}

/// `/slopdrop withheld|posts`: one block per entry — the id, what it is
/// and why, the request, the post link the owner can open (and paste back
/// into `delete`), and the head of the output.
pub fn ledger_listing(verb: &str, list: &[WithheldEntry]) -> String {
    let mut out = format!(
        "{} {}:\n",
        list.len(),
        if verb == "withheld" {
            "withheld"
        } else {
            "public /tcl replies"
        }
    );
    for e in list.iter().take(20) {
        let proc = e.proc_name.as_deref().unwrap_or("-");
        let silent = if e.no_ask { "  [silent]" } else { "" };
        let post = e
            .post_uris
            .first()
            .and_then(|u| crate::withheld::post_link(u))
            .unwrap_or_else(|| "(not posted yet)".to_string());
        out.push_str(&format!(
            "  {}  [{}] {}  proc={}  {}{}\n    {}\n    {}\n",
            e.id,
            e.kind.label(),
            e.reason,
            proc,
            e.requester,
            silent,
            post,
            truncate_chars(&e.output.replace('\n', " "), 80),
        ));
    }
    if list.len() > 20 {
        out.push_str(&format!("  … and {} older\n", list.len() - 20));
    }
    out.push_str("delete one with /slopdrop delete <id or post link>");
    out
}

/// `/slopdrop approve|delete <target>`: hand the recorded reply to the
/// publisher (the marks here, the atproto plugin's generic services move
/// the bytes). `target` is the entry id or anything that names its post
/// (`WithheldStore::find`). The owner reached this through a Cap::All
/// surface already.
pub async fn withheld_act(verb: &str, target: &str) -> Result<String, String> {
    let action = match verb {
        "approve" => crate::publish::WithheldAction::Approve,
        "delete" => crate::publish::WithheldAction::Delete,
        other => return Err(format!("unknown verb {other}")),
    };
    let out = crate::publish::act(target, action).await;
    // The Telegram button logs its "sync decision"; the typed command left
    // no line at all — the owner's delete of 6d434548 (10-07) showed only
    // in the files' mtimes.
    match &out {
        Ok(report) => veles_core::module::log(
            tracing::Level::INFO,
            &format!("tcl: /slopdrop {verb} {target}: {report}"),
        ),
        Err(e) => veles_core::module::log(
            tracing::Level::WARN,
            &format!("tcl: /slopdrop {verb} {target} failed: {e}"),
        ),
    }
    out
}

/// When the mirror last saw slopdrop, as a whole phrase.
///
/// The three arms used to yield a bare `age` with `" ago"` appended
/// unconditionally, so a mirror synced seconds ago reported "synced now
/// ago" and one with an unreadable timestamp reported "synced unknown
/// ago". The arms are only correct together, which is why they are
/// built together.
pub fn synced_phrase(synced_at: Option<u64>, now: u64) -> String {
    match synced_at {
        Some(t) if now > t => format!("synced {} ago", humantime_secs(now - t)),
        // Same second, or a clock that went backwards: "just now" is
        // true either way and "-3s ago" would not be.
        Some(_) => "synced just now".to_string(),
        None => "sync time unknown".to_string(),
    }
}

/// Seconds → "3h 2m", "45s", with sub-minute precision ("12s") — the
/// rough age line `/slopdrop status` wants, not a general formatter.
fn humantime_secs(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ledger listing shows each reply's kind, why, and a link to its
    /// post the owner can open — and paste back into `/slopdrop delete`.
    #[test]
    fn the_ledger_listing_shows_each_posts_link() {
        let e = WithheldEntry {
            id: "6d434548".into(),
            kind: PostKind::Inline,
            reason: "passed the output check".into(),
            requester: "alice.example (atproto:did:plc:a)".into(),
            output: "42".into(),
            post_uris: vec!["at://did:plc:bot/app.bsky.feed.post/3mxc3gudjowwr".into()],
            ..Default::default()
        };
        let out = ledger_listing("posts", &[e]);
        assert!(
            out.contains("6d434548  [inline] passed the output check"),
            "{out}"
        );
        assert!(
            out.contains("https://impro.social/profile/did:plc:bot/post/3mxc3gudjowwr"),
            "{out}"
        );
    }

    #[test]
    fn the_mirror_status_phrase_reads_as_english_in_every_arm() {
        assert_eq!(synced_phrase(Some(1000), 1240), "synced 4m 0s ago");
        assert_eq!(synced_phrase(Some(1000), 1000), "synced just now");
        assert_eq!(synced_phrase(Some(1000), 900), "synced just now");
        assert_eq!(synced_phrase(None, 1000), "sync time unknown");
        for arm in [synced_phrase(Some(1000), 1000), synced_phrase(None, 1000)] {
            assert!(!arm.ends_with(" ago"), "{arm}");
        }
    }

    /// The command is a DIRECT one: it answers, with its own cap floor,
    /// and a below-floor caller is refused before any verb runs.
    #[tokio::test]
    async fn the_command_registers_as_direct_and_gates_on_shell() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        register().unwrap();
        let d = veles_core::commands::direct_command(NAME).expect("registered direct");
        assert_eq!(d.cap, Cap::Shell);
        let guest = Principal::known("irc:nobody", veles_core::acl::Role::Guest);
        let out = veles_core::commands::run_direct(
            NAME,
            &d,
            DirectCall {
                args: "status".into(),
                principal: Some(guest),
                ..Default::default()
            },
        )
        .await;
        assert!(out.starts_with("[acl]"), "{out}");
        // A local owner surface (no principal) reaches the mirror status.
        let out = run(DirectCall {
            args: "status".into(),
            ..Default::default()
        })
        .await;
        assert!(
            out.contains("no mirror yet") || out.contains("slopdrop mirror"),
            "{out}"
        );
    }
}
