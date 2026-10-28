//! Sync decisions held for the owner, asked where the owner is.
//!
//! USER 2026-10-06: "can you tack on configsync approvals forwarded to
//! telegram like ask approvals? because it's going to be really annoying
//! to have to do that from different frontends and I can always review
//! everything from telegram" — and "if there are other sync approvals
//! that i haven't noticed/haven't even been doing then those too".
//!
//! Until now each of these waited, with a WARN in a log, for a step
//! typed on the node that held it — three nodes, three terminals:
//!
//! | held here                                   | the step it waited for              |
//! |---------------------------------------------|-------------------------------------|
//! | a config offer (security rows, a conflict)  | `/configsync apply` · `drop`        |
//! | a memory offer with a conflicting path      | `veles memory apply --inbox`, by hand |
//! | a node asking to join the vault             | `veles vault admit --fingerprint`   |
//! | a vault offer to a node with no vault yet   | `veles vault apply --inbox --trust` |
//! | queued messages that came with a session    | `/queue run`                        |
//!
//! Now the sync loop asks the owner about each, one at a time, through
//! the owner's leg the gateway installed for asks (`[notify] ask_relay`:
//! the home chat — here, or on whichever node holds the Telegram lease,
//! `askrelay`), and acts on the answer exactly as that step would.
//! Without an owner leg nothing is asked and the steps still work.
//!
//! ONE AT A TIME: a reply typed in the home chat answers the oldest
//! question open there (`ask::resolve_for_chat`), so two open sync
//! questions would make "1" ambiguous. The next is asked when the
//! current one is answered or gives up.
//!
//! CHOICE QUESTIONS ARE APPROVALS: an approval takes only a choice
//! (its number, key or label, or a plain yes/no) and lets any other
//! text through as an ordinary message. A clarify would take the
//! owner's next Telegram message, whatever it was, as its answer.
//!
//! The vault keeps its out-of-band check: a join is answered with the
//! FINGERPRINT read on the joining device (`veles vault whoami` there),
//! typed back — a tap could not prove the request came from the device
//! in your hand — and an adoption with the pool root's fingerprint.

use crate::ask::{AskAnswer, AskChoice, AskKind, AskRequest, Asker};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// How long one question stays open.
const ASK_TIMEOUT_SECS: u64 = 6 * 3600;
/// Unanswered: asked again this long after it was asked.
const REASK_UNANSWERED_SECS: u64 = 6 * 3600;
/// Answered "later".
const REASK_LATER_SECS: u64 = 24 * 3600;
/// Not delivered (the relay was down, a gateway restarted under it),
/// or the answer could not be acted on (a mistyped fingerprint).
const RETRY_SECS: u64 = 600;

/// One decision held for the owner.
#[derive(Debug, Clone, PartialEq)]
pub enum Pending {
    /// A config offer left in the inbox: rows for a security section,
    /// a conflict, or (without `configsync_auto`) every offer.
    Config {
        file: PathBuf,
        from: String,
        /// (key, value); value "" is a removal.
        rows: Vec<(String, String)>,
        /// The drain applied the rest and kept these for the owner.
        security_only: bool,
    },
    /// A memory offer `memsync_auto` left in the inbox (a conflict).
    MemoryConflict {
        file: PathBuf,
        from: String,
        paths: Vec<String>,
    },
    /// A node asking to be admitted to the vault.
    VaultJoin {
        file: PathBuf,
        /// The signed request the question was about, kept so a late
        /// answer can still be checked after the drain archives `file`.
        request: crate::vault::JoinRequest,
    },
    /// A vault offer to this node, which has no vault to merge it into.
    VaultAdopt { file: PathBuf, from: String },
    /// Queued messages that arrived HELD with a migrated session.
    QueueHeld { session: String, count: usize },
    /// A decision a registered [`PendingSource`] holds (R5: the slopdrop
    /// module's withheld public `/tcl` replies). `source` is the name
    /// it registered under; `id` is its own.
    External { source: String, id: String },
}

// ── pending-decision sources (PLAN-plugins-installer.md R5) ────────────
//
// A module that holds decisions for the owner (the slopdrop ledger's
// withheld replies) registers a source: what it has open, how to ask
// about each, what to do with the answer. The sync loop asks them like
// its own, one at a time, through the same owner leg. OWNER-TRACKED
// (M4): a module's source is purged with it on reload; its open
// decisions simply stop being asked until it is back.

/// The question a source wants asked about one of its decisions.
#[derive(Debug, Clone)]
pub struct SourceQuestion {
    pub kind: AskKind,
    pub prompt: String,
    pub detail: String,
    pub choices: Vec<AskChoice>,
    /// A picture the decision is about, base64 PNG (`AskRequest::image`):
    /// the slopdrop module's held drawing, which the owner judges by
    /// looking at it. Surfaces that can show it do.
    pub image: Option<String>,
}

/// What a source did with the owner's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceOutcome {
    /// Decided; the decision is gone.
    Decided(String),
    /// Kept; ask again in a day.
    Later(String),
}

/// A holder of decisions the owner is asked about.
pub trait PendingSource: Send + Sync {
    /// The ids of every decision open right now.
    fn scan(&self) -> Vec<String>;
    /// How to ask about `id`. `node_name` is this node's, for the prompt.
    fn question(&self, id: &str, node_name: &str) -> SourceQuestion;
    /// Act on the owner's answer: `pick` is the choice as the ask layer
    /// delivered it (a choice key, or `ask::CHOICE_ONCE` / `CHOICE_DENY`
    /// for a plain yes / no), `free_text` any text that came with it.
    /// `Err` means the answer could not be acted on; it is asked again
    /// after the retry delay.
    fn decide(&self, id: &str, pick: &str, free_text: &str) -> Result<SourceOutcome>;
}

fn sources_cell() -> &'static crate::owned::OwnedRegistry<String, Arc<dyn PendingSource>> {
    static S: std::sync::OnceLock<crate::owned::OwnedRegistry<String, Arc<dyn PendingSource>>> =
        std::sync::OnceLock::new();
    S.get_or_init(crate::owned::OwnedRegistry::default)
}

/// Register `source` under `name` (the decision ids become
/// `<name>:<id>`). The owner is read from the calling thread (M4).
pub fn register_source(name: &str, source: Arc<dyn PendingSource>) {
    sources_cell().insert(name.to_string(), source);
}

/// Remove every source `owner` registered (R3 teardown). `None` (core)
/// removes nothing. Returns the count removed.
pub fn purge_owner_sources(owner: Option<&'static str>) -> usize {
    sources_cell().purge(owner)
}

/// Remove the source registered under `name`, whoever registered it.
pub fn unregister_source(name: &str) {
    sources_cell().remove(&name.to_string());
}

fn sources() -> Vec<(String, Arc<dyn PendingSource>)> {
    sources_cell()
        .keys()
        .into_iter()
        .filter_map(|k| sources_cell().with(&k, Arc::clone).map(|s| (k, s)))
        .collect()
}

fn source(name: &str) -> Option<Arc<dyn PendingSource>> {
    sources_cell().with(&name.to_string(), Arc::clone)
}

impl Pending {
    /// A stable id per decision, for the book of when to ask.
    pub fn id(&self) -> String {
        let f = |p: &Path| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        match self {
            Pending::Config { file, .. } => format!("config:{}", f(file)),
            Pending::MemoryConflict { file, .. } => format!("memory:{}", f(file)),
            Pending::VaultJoin { file, .. } => format!("vault-join:{}", f(file)),
            Pending::VaultAdopt { file, .. } => format!("vault-adopt:{}", f(file)),
            Pending::QueueHeld { session, .. } => format!("queue:{session}"),
            Pending::External { source, id } => format!("{source}:{id}"),
        }
    }

    /// The question, as the owner reads it on the phone. `note` is what
    /// went wrong with the last answer, said again on the re-ask.
    pub fn request(&self, node_name: &str, note: Option<&str>) -> AskRequest {
        let mut image = None;
        let three = |a: (&str, &str), b: (&str, &str)| {
            vec![
                AskChoice::new(a.0, a.1),
                AskChoice::new(b.0, b.1),
                AskChoice::new("later", "later"),
            ]
        };
        let (kind, prompt, detail, choices) = match self {
            Pending::Config {
                from,
                rows,
                security_only,
                ..
            } => (
                AskKind::Approval,
                if *security_only {
                    format!(
                        "config from {from} changes {} security setting(s) — apply it on \
                         {node_name}?",
                        rows.len()
                    )
                } else {
                    format!(
                        "config from {from} waits on {node_name} ({} change(s); a conflict, or \
                         configsync_auto is off) — apply it (the newer write wins) or drop it?",
                        rows.len()
                    )
                },
                rows.iter()
                    .map(|(k, v)| {
                        let tag = if crate::configsync::is_held_key(k) {
                            " [security]"
                        } else {
                            ""
                        };
                        if v.is_empty() {
                            format!("{k}{tag} (removed)")
                        } else {
                            format!("{k}{tag} = {}", cut(v, 300))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                three(("apply", "apply"), ("drop", "drop it")),
            ),
            Pending::MemoryConflict { from, paths, .. } => (
                AskKind::Approval,
                format!(
                    "memory from {from}: {} file(s) changed on both nodes — keep whose \
                     version on {node_name}?",
                    paths.len()
                ),
                paths.join("\n"),
                three(
                    ("theirs", &format!("take {from}'s")),
                    ("mine", &format!("keep {node_name}'s")),
                ),
            ),
            Pending::VaultJoin { request, .. } => (
                AskKind::Clarify,
                format!(
                    "{} asks to join the vault on {node_name}. Run `veles vault whoami` ON \
                     {} and reply with the fingerprint it shows — or decline.",
                    request.name, request.name
                ),
                format!("node {}", request.node),
                vec![AskChoice::new("decline", "decline")],
            ),
            Pending::VaultAdopt { from, .. } => (
                AskKind::Clarify,
                format!(
                    "{from} offers its vault to {node_name}, which has none yet. Reply with the \
                     pool root's fingerprint (`veles vault status` on a member) to adopt it — \
                     or decline."
                ),
                String::new(),
                vec![AskChoice::new("decline", "decline")],
            ),
            Pending::QueueHeld { session, count } => (
                AskKind::Approval,
                format!(
                    "{count} queued message(s) came to {node_name} with session {session} from \
                     another node — release them to run here?"
                ),
                String::new(),
                vec![
                    AskChoice::new("run", "release them"),
                    AskChoice::new("leave", "leave them held"),
                ],
            ),
            Pending::External { source, id } => match self::source(source) {
                Some(src) => {
                    let q = src.question(id, node_name);
                    image = q.image;
                    (q.kind, q.prompt, q.detail, q.choices)
                }
                None => (
                    AskKind::Approval,
                    format!(
                        "{source} holds a decision ({id}) on {node_name}, but the module that \
                         owns it is not loaded — keep it for later?"
                    ),
                    String::new(),
                    vec![AskChoice::new("later", "later")],
                ),
            },
        };
        let detail = match note {
            Some(n) if detail.is_empty() => n.to_string(),
            Some(n) => format!("{n}\n\n{detail}"),
            None => detail,
        };
        let mut req = AskRequest::new("", kind, &prompt, choices).with_timeout(ASK_TIMEOUT_SECS);
        if !detail.is_empty() {
            req = req.with_detail(detail);
        }
        req.image = image;
        req
    }
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// Apply the enabled automatic inboxes before scanning or acting on an
/// owner decision. A receive and this loop can cross, so presence in an
/// inbox is not itself proof of a conflict. The ordinary apply routines
/// remain authoritative: they archive clean offers, preserve conflicts
/// and security holds, and retire only exact already-admitted joins.
/// Errors are logged and the question scan still sees the pending file;
/// they are never converted into approval or data loss.
pub fn drain_automatic(store: &crate::store::Store, cfg: &crate::config::VelesConfig) {
    if cfg.cluster.memsync_auto {
        let mem = crate::memory::MemoryStore::new();
        if let Err(e) = crate::memsync::apply_inbox(&mem, &cfg.cluster.local_memory_globs) {
            tracing::warn!("memsync: the pre-decision drain failed: {e:#}");
        }
    }
    if !cfg.cluster.configsync_auto {
        return;
    }
    match crate::node::NodeIdentity::load_or_create(&crate::config::config_dir()) {
        Ok(me) => {
            if let Err(e) = crate::configsync::apply_inbox_auto(store, &me.id) {
                tracing::warn!("configsync: the pre-decision drain failed: {e:#}");
            }
        }
        Err(e) => tracing::warn!("configsync: no local identity for the pre-decision drain: {e:#}"),
    }
    if let Err(e) = crate::vault::apply_inbox_auto() {
        tracing::warn!("vault: the pre-decision drain failed: {e:#}");
    }
}

/// Every decision held on this node now, oldest kind first.
pub fn scan(store: &crate::store::Store, cfg: &crate::config::VelesConfig) -> Vec<Pending> {
    let mut out = Vec::new();
    // Config: whatever is still in the inbox after the drain. With
    // `configsync_auto` that is a security hold (the offer was cut down
    // to its held rows) or a conflict (kept whole); without it, every
    // offer waits for a person.
    for p in crate::configsync::inbox_pending(&crate::configsync::inbox_dir()).unwrap_or_default() {
        let Ok(e) = crate::configsync::inbox_read(&p) else {
            continue;
        };
        if e.offer.rows.is_empty() {
            continue;
        }
        let from = if e.offer.from_name.is_empty() {
            e.offer.from_node.clone()
        } else {
            e.offer.from_name.clone()
        };
        let security_only = e
            .offer
            .rows
            .iter()
            .all(|r| crate::configsync::is_held_key(&r.key));
        out.push(Pending::Config {
            file: p,
            from,
            rows: e
                .offer
                .rows
                .iter()
                .map(|r| (r.key.clone(), r.value.clone()))
                .collect(),
            security_only,
        });
    }
    // Memory: with memsync_auto on, an offer still in the inbox is one
    // with a conflict (a clean one moved to applied/ on arrival). Off,
    // the inbox is the manual flow's and has no conflict to decide yet.
    if cfg.cluster.memsync_auto {
        for p in crate::memsync::inbox_pending(&crate::memsync::inbox_dir()).unwrap_or_default() {
            let Ok(e) = crate::memsync::inbox_read(&p) else {
                continue;
            };
            let from = if e.offer.from_name.is_empty() {
                e.peer.clone()
            } else {
                e.offer.from_name.clone()
            };
            let mut paths: Vec<String> = e.offer.revisions.iter().map(|r| r.path.clone()).collect();
            paths.sort();
            paths.dedup();
            out.push(Pending::MemoryConflict {
                file: p,
                from,
                paths,
            });
        }
    }
    // Vault: join requests, and offers this node cannot merge because it
    // has no vault (`AutoOutcome::NoVault`). An offer refused by a vault
    // it does have is a fault for the log, not a decision.
    let has_vault = crate::vault::Vault::open_local_if_any()
        .ok()
        .flatten()
        .is_some_and(|v| v.is_initialized());
    for p in crate::vault::inbox_pending(&crate::vault::inbox_dir()).unwrap_or_default() {
        let Ok(e) = crate::vault::inbox_read(&p) else {
            continue;
        };
        match e.message {
            crate::vault::Message::Join(request) => {
                out.push(Pending::VaultJoin { file: p, request })
            }
            crate::vault::Message::Offer { from_name, .. } if !has_vault => {
                out.push(Pending::VaultAdopt {
                    file: p,
                    from: if from_name.is_empty() {
                        e.peer
                    } else {
                        from_name
                    },
                })
            }
            crate::vault::Message::Offer { .. } => {}
        }
    }
    for (session, count) in store.queue_held_counts().unwrap_or_default() {
        out.push(Pending::QueueHeld { session, count });
    }
    // Withheld public `/tcl` replies awaiting approve/delete. The store is
    // read live; an entry the owner already decided is gone from it (or,
    // approved, is inline now), and one they marked "keep + stop asking"
    // (`no_ask`) is left but never asked. Inline replies and blocked notes
    // are recorded only so they can be deleted: nothing to ask.
    for (name, src) in sources() {
        for id in src.scan() {
            out.push(Pending::External {
                source: name.clone(),
                id,
            });
        }
    }
    out
}

/// When each decision may be asked next (`syncask.json` beside the
/// store), and what went wrong with its last answer.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub next_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The round of the question out for it ([`AskRequest::relay_key`]),
    /// kept across this node's restarts: a phone stops five minutes
    /// after its last use, long before the owner answers, and its next
    /// start asks with the same round to take the answer the owner gave
    /// meanwhile. Ends when an answer is used or the question runs out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<String>,
}

type Book = BTreeMap<String, Entry>;

fn book_path() -> PathBuf {
    crate::config::data_dir().join("syncask.json")
}

fn book_load() -> Book {
    std::fs::read_to_string(book_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn book_save(b: &Book) {
    let p = book_path();
    let tmp = p.with_extension("json.tmp");
    if let Ok(body) = serde_json::to_vec_pretty(b) {
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// When to ask next; the round, if one is out, stays.
fn book_set(id: &str, next_at: u64, note: Option<String>) {
    let mut b = book_load();
    let round = b.get(id).and_then(|e| e.round.clone());
    b.insert(
        id.to_string(),
        Entry {
            next_at,
            note,
            round,
        },
    );
    book_save(&b);
}

/// The round is over (its answer was used, or its question ran out): the
/// next ask is a new question.
fn book_end_round(id: &str) {
    let mut b = book_load();
    if let Some(e) = b.get_mut(id) {
        if e.round.take().is_some() {
            book_save(&b);
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The first decision due at `now`: never asked, or its time has come.
/// Forgets entries for decisions that are gone (decided, or decided
/// with the CLI meanwhile).
pub fn next_due(pending: &[Pending], book: &mut Book, now: u64) -> Option<Pending> {
    book.retain(|id, _| pending.iter().any(|p| p.id() == *id));
    pending
        .iter()
        .find(|p| book.get(&p.id()).is_none_or(|e| e.next_at <= now))
        .cloned()
}

/// What an answer did, and when (if ever) to ask again.
#[derive(Debug, PartialEq)]
pub struct Acted {
    pub line: String,
    /// `None`: decided, the decision is gone. `Some(secs)`: ask again
    /// after that long.
    pub again_in: Option<u64>,
}

fn decided(line: String) -> Result<Acted> {
    Ok(Acted {
        line,
        again_in: None,
    })
}

fn later(line: String) -> Result<Acted> {
    Ok(Acted {
        line,
        again_in: Some(REASK_LATER_SECS),
    })
}

fn move_to(file: &Path, sub: &str) -> Result<()> {
    let dir = file.parent().context("inbox file has no dir")?.join(sub);
    std::fs::create_dir_all(&dir)?;
    std::fs::rename(file, dir.join(file.file_name().unwrap_or_default()))
        .with_context(|| format!("move {} to {sub}/", file.display()))?;
    Ok(())
}

/// The automatic drain or an operator on another surface may have
/// settled the offer after its question was delivered. A missing inbox
/// file is that success case, not a reason to re-ask for ever. Failures
/// to read a file that still exists keep their original error.
fn already_settled(p: &Pending, answer: &AskAnswer) -> Option<Result<Acted>> {
    let (file, subject) = match p {
        Pending::Config { file, from, .. } => (file, format!("config from {from}")),
        Pending::MemoryConflict { file, from, .. } => (file, format!("memory from {from}")),
        Pending::VaultJoin { file, request, .. } => {
            (file, format!("{}'s vault join", request.name))
        }
        Pending::VaultAdopt { file, from } => {
            if !file.exists() && matches!(answer.first().unwrap_or(""), "apply" | "decline") {
                return Some(decided(format!(
                    "{from}'s vault adoption was already settled while its answer was on the way"
                )));
            }
            return None;
        }
        _ => return None,
    };
    if file.exists() {
        return None;
    }
    match answer.first().unwrap_or("") {
        crate::ask::CHOICE_ONCE
        | crate::ask::CHOICE_DENY
        | "apply"
        | "drop"
        | "mine"
        | "theirs"
        | "decline" => Some(decided(format!(
            "{subject} was already settled while its answer was on the way"
        ))),
        _ => None,
    }
}

/// Act on the owner's answer, as the CLI step would.
pub fn act(
    store: &Mutex<crate::store::Store>,
    cfg: &crate::config::VelesConfig,
    p: &Pending,
    answer: &AskAnswer,
) -> Result<Acted> {
    // A clean offer may have arrived between the first scan and this
    // answer. Re-apply the enabled inboxes before trusting the stale
    // question's view of the world. A genuine conflict/security hold
    // survives this drain and is still the owner's to decide.
    drain_automatic(&store.lock().unwrap_or_else(|p| p.into_inner()), cfg);
    if let Some(done) = already_settled(p, answer) {
        return done;
    }
    // An approval's plain "yes" arrives as `once` and "no" as `deny`
    // (`ask::parse_reply`): yes is the first choice; no is the one that
    // changes nothing here — never "take theirs".
    let pick = match answer.first().unwrap_or("") {
        crate::ask::CHOICE_ONCE => match p {
            Pending::MemoryConflict { .. } => "later",
            Pending::QueueHeld { .. } => "run",
            _ => "apply",
        },
        crate::ask::CHOICE_DENY => match p {
            Pending::QueueHeld { .. } => "leave",
            Pending::MemoryConflict { .. } => "later",
            _ => "drop",
        },
        other => other,
    }
    .to_string();
    let text = answer.free_text.clone().unwrap_or_default();
    // A source reads the answer as the ask layer delivered it: the
    // yes/no mapping above is core's own decisions' and means nothing
    // to a module's choices.
    let raw = answer.first().unwrap_or("").to_string();
    match p {
        Pending::Config { file, from, .. } => match pick.as_str() {
            "apply" => {
                let me =
                    crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?.id;
                let done = {
                    let s = store.lock().unwrap_or_else(|p| p.into_inner());
                    crate::configsync::apply_inbox_file(&s, &me, file)?
                };
                decided(format!(
                    "config from {from} applied: {}",
                    done.report.summarize()
                ))
            }
            "drop" => {
                crate::configsync::drop_inbox_file(file)?;
                decided(format!("config from {from} dropped here"))
            }
            _ => later(format!("config from {from} left for later")),
        },
        Pending::MemoryConflict { file, from, .. } => match pick.as_str() {
            "theirs" => {
                // Applying again names the conflicts again (the clean
                // paths are in, node-local ones stay out), each with
                // the incoming content — which becomes the newest
                // revision here, so the next offer from there is
                // "unchanged".
                let entry = crate::memsync::inbox_read(file)?;
                let mem = crate::memory::MemoryStore::new();
                let report = crate::memsync::apply_offer_scoped(
                    &mem,
                    &entry.offer,
                    &cfg.cluster.local_memory_globs,
                )?;
                for c in &report.conflicts {
                    mem.write_file(&c.path, &c.incoming_content)?;
                }
                move_to(file, "applied")?;
                decided(format!(
                    "memory from {from}: took their version of {} file(s)",
                    report.conflicts.len()
                ))
            }
            "mine" => {
                move_to(file, "applied")?;
                decided(format!("memory from {from}: kept this node's version"))
            }
            _ => later(format!("memory from {from} left for later")),
        },
        Pending::VaultJoin { file, request } => {
            let name = &request.name;
            if pick == "decline" {
                if !file.exists() {
                    return decided(format!(
                        "{name}'s vault join was already settled while its answer was on the way"
                    ));
                }
                move_to(file, "dropped")?;
                return decided(format!("{name}'s vault join declined"));
            }
            let fp = text.trim();
            // `admit` checks the signature and the typed fingerprint on
            // THIS preserved request, even if the pending file was
            // already retired by the receiving drain.
            let mut v = crate::vault::Vault::open_local()?;
            let out = v.update(|v| v.admit(request, fp))?;
            if file.exists() {
                move_to(file, "applied")?;
            }
            let replacing = match &out.replaced {
                Some((old, new)) => format!("replacing key for {name} ({old} → {new}); "),
                None => String::new(),
            };
            decided(format!(
                "{replacing}{name} admitted to the vault ({} value(s) re-encrypted for it)",
                out.rekeyed.len()
            ))
        }
        Pending::VaultAdopt { file, from } => {
            if pick == "decline" {
                move_to(file, "dropped")?;
                return decided(format!("{from}'s vault declined here"));
            }
            let fp = text.trim();
            let crate::vault::Message::Offer { file: incoming, .. } =
                crate::vault::inbox_read(file)?.message
            else {
                anyhow::bail!("{} is not a vault offer", file.display());
            };
            // `apply` with --trust's fingerprint: refused unless the
            // offer's root is that one.
            let mut v = crate::vault::Vault::open_local()?;
            let report = v.update(|v| v.apply(&incoming, Some(fp)))?;
            move_to(file, "applied")?;
            // Offers that waited behind this one merge now.
            let _ = crate::vault::apply_inbox_auto();
            decided(format!("{from}'s vault adopted: {}", report.summarize()))
        }
        Pending::QueueHeld { session, .. } => match pick.as_str() {
            "run" => {
                let n = store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .queue_run_held(session, None)?;
                decided(format!(
                    "{n} held message(s) of {session} released — they run when its next turn \
                     ends"
                ))
            }
            _ => later(format!("held messages of {session} left held")),
        },
        Pending::External { source, id } => {
            let Some(src) = self::source(source) else {
                anyhow::bail!("{source} is not loaded here; {id} waits for it");
            };
            match src.decide(id, &raw, &text)? {
                SourceOutcome::Decided(line) => decided(line),
                SourceOutcome::Later(line) => later(line),
            }
        }
    }
}

/// The questions this node has open, by decision id. A decision is never
/// asked twice at once, but DIFFERENT decisions ask in parallel — a pile
/// of withheld /tcl replies must not serialize behind the first one the
/// owner is slow to answer (USER 2026-10-07: "approvals arrive serially
/// after the previous one is decided? extremely inefficient"). Bounded so
/// a flood of withheld replies does not bury the owner.
fn asking_set() -> &'static Mutex<std::collections::HashSet<String>> {
    static M: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// The most asks open at once.
const MAX_OPEN_ASKS: usize = 4;

/// Decisions this process has asked about: a round out when it started
/// is asked again once, at once, and after that on the book's schedule.
fn rejoined_set() -> &'static Mutex<std::collections::HashSet<String>> {
    static M: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

struct Asking(String);

impl Drop for Asking {
    fn drop(&mut self) {
        asking_set()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

/// The sync loop's share, every tick: open asks for decisions that are due,
/// through the owner's leg (`ask::owner_leg`), and act on the answers.
/// Returns at once; each ask runs on its own task (it can wait for hours),
/// up to [`MAX_OPEN_ASKS`] in flight.
pub fn ask_due(store: Arc<Mutex<crate::store::Store>>, cfg: Arc<crate::config::VelesConfig>) {
    if let Some(leg) = crate::ask::owner_leg() {
        ask_due_with(store, cfg, leg);
    }
}

/// [`ask_due`] through a given leg. The last spawned task's handle.
pub fn ask_due_with(
    store: Arc<Mutex<crate::store::Store>>,
    cfg: Arc<crate::config::VelesConfig>,
    leg: Arc<dyn Asker>,
) -> Option<tokio::task::JoinHandle<()>> {
    // Do this even when no question is due: an offer may have arrived
    // between the gateway's tick and this call. The scan below must be
    // of the post-drain state, not the state at the tick's beginning.
    drain_automatic(&store.lock().unwrap_or_else(|p| p.into_inner()), &cfg);
    let pending = {
        let s = store.lock().unwrap_or_else(|p| p.into_inner());
        scan(&s, &cfg)
    };
    let now = now_secs();
    let mut book = book_load();
    let node_name = cfg.node.name.clone().unwrap_or_else(|| {
        crate::node::NodeIdentity::load(&crate::config::config_dir())
            .ok()
            .flatten()
            .map(|i| i.name)
            .unwrap_or_else(|| "this node".into())
    });
    // Open asks for every due decision up to the cap. Each holds its slot
    // until its answer is acted on (the Asking guard drops on task end).
    let mut last = None;
    loop {
        let open = asking_set().lock().unwrap_or_else(|p| p.into_inner()).len();
        if open >= MAX_OPEN_ASKS {
            break;
        }
        // A round that was out when this process started first (its
        // answer may be waiting on the node that asked the owner), then
        // the first due decision not already open.
        let due = {
            let asking = asking_set().lock().unwrap_or_else(|p| p.into_inner());
            let rejoined = rejoined_set().lock().unwrap_or_else(|p| p.into_inner());
            let rejoin = pending
                .iter()
                .find(|p| {
                    let id = p.id();
                    !asking.contains(&id)
                        && !rejoined.contains(&id)
                        && book.get(&id).is_some_and(|e| e.round.is_some())
                })
                .cloned();
            let mut book2 = book.clone();
            rejoin.or_else(|| {
                next_due(&pending, &mut book2, now).filter(|p| !asking.contains(&p.id()))
            })
        };
        let Some(p) = due else { break };
        let id = p.id();
        // Claim the slot before spawning so a second tick can't double-ask.
        if !asking_set()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.clone())
        {
            break;
        }
        rejoined_set()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.clone());
        let note = book.get(&id).and_then(|e| e.note.clone());
        // A new round's name: unique to this process (the counter) and
        // to this start of it (the clock).
        static ROUNDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let round = book
            .get(&id)
            .and_then(|e| e.round.clone())
            .unwrap_or_else(|| {
                let n = ROUNDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                format!("{id}@{now}.{n}")
            });
        book.insert(
            id.clone(),
            Entry {
                next_at: now + REASK_UNANSWERED_SECS,
                note: None,
                round: Some(round.clone()),
            },
        );
        book_save(&book);
        let node_name = node_name.clone();
        let leg = leg.clone();
        let store = store.clone();
        let cfg = cfg.clone();
        last = Some(crate::rt::spawn(async move {
            let _asking = Asking(id.clone());
            let mut req = p.request(&node_name, note.as_deref());
            req.relay_key = Some(round);
            tracing::info!("sync decision {id}: asking the owner ({})", req.id);
            let answer = leg.ask(req).await;
            if answer.timed_out {
                // Gone well before its time: never delivered (no relay, a
                // restart under it) — try again soon, in the same round. A
                // real timeout keeps the six hours stamped above, and the
                // next question is a new one.
                if now_secs().saturating_sub(now) + 60 < ASK_TIMEOUT_SECS {
                    book_set(&id, now_secs() + RETRY_SECS, None);
                    tracing::warn!("sync decision {id}: the question did not reach the owner");
                } else {
                    book_end_round(&id);
                }
                return;
            }
            // The answer is used now, whatever comes of it: a decision
            // asked again is a new question.
            book_end_round(&id);
            let (s, c, p2) = (store.clone(), cfg.clone(), p.clone());
            let done = crate::rt::spawn_blocking(move || act(&s, &c, &p2, &answer)).await;
            match done {
                Ok(Ok(a)) => {
                    tracing::info!("sync decision: {}", a.line);
                    if let Some(secs) = a.again_in {
                        book_set(&id, now_secs() + secs, None);
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!("sync decision {id}: not done: {e:#}");
                    book_set(
                        &id,
                        now_secs() + RETRY_SECS,
                        Some(format!("Your last answer was not used: {e:#}")),
                    );
                }
                Err(e) => {
                    tracing::warn!("sync decision {id}: the act task failed: {e:#}");
                    book_set(&id, now_secs() + RETRY_SECS, None);
                }
            }
        }));
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// The owner's leg, scripted: records each question, answers from
    /// the list (a timeout when it runs out), and can be held open.
    struct Owner {
        answers: Mutex<VecDeque<AskAnswer>>,
        seen: Mutex<Vec<AskRequest>>,
        hold: Option<Arc<tokio::sync::Notify>>,
    }

    impl Owner {
        fn answering(a: Vec<AskAnswer>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(a.into()),
                seen: Mutex::new(Vec::new()),
                hold: None,
            })
        }
        fn seen(&self) -> Vec<AskRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Asker for Owner {
        async fn ask(&self, req: AskRequest) -> AskAnswer {
            self.seen.lock().unwrap().push(req);
            if let Some(h) = &self.hold {
                h.notified().await;
            }
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(AskAnswer::timeout)
        }
    }

    fn cfg_named(name: &str) -> Arc<crate::config::VelesConfig> {
        let mut c = crate::config::VelesConfig::default();
        c.node.name = Some(name.into());
        Arc::new(c)
    }

    fn scratch_store() -> (tempfile::TempDir, Arc<Mutex<crate::store::Store>>) {
        let d = tempfile::tempdir().unwrap();
        let s = crate::store::Store::open(&d.path().join("state.db")).unwrap();
        (d, Arc::new(Mutex::new(s)))
    }

    /// A config.toml older than any incoming row (last writer wins),
    /// and an offer from the laptop that changes one ACL key.
    fn held_acl_offer() -> PathBuf {
        let path = crate::configedit::config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[model]\ndefault = \"m\"\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
            .unwrap();
        let offer = crate::configsync::ConfigSyncOffer {
            from_node: "peer-node".into(),
            from_name: "laptop".into(),
            since_seq: 0,
            rows: vec![crate::configsync::ConfigLogRow {
                node: "peer-node".into(),
                node_seq: 1,
                at: crate::configsync::now_stamp(),
                key: "acl.default_role".into(),
                value: "\"user\"".into(),
            }],
        };
        crate::configsync::inbox_store(&crate::configsync::inbox_dir(), "laptop", "t1", offer)
            .unwrap()
    }

    fn config_text() -> String {
        std::fs::read_to_string(crate::configedit::config_path()).unwrap()
    }

    /// A security hold is asked ONCE, as an approval naming what it
    /// changes and where, and "apply" lands it as `/configsync apply`
    /// would; the next tick has nothing to ask.
    #[tokio::test]
    async fn a_held_config_change_is_asked_once_and_applied_on_the_answer() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        let file = held_acl_offer();
        let owner = Owner::answering(vec![AskAnswer::choice("apply")]);
        ask_due_with(store.clone(), cfg_named("vps"), owner.clone())
            .expect("asked")
            .await
            .unwrap();
        let seen = owner.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, AskKind::Approval);
        assert!(
            seen[0]
                .prompt
                .contains("config from laptop changes 1 security setting")
                && seen[0].prompt.contains("apply it on vps"),
            "{}",
            seen[0].prompt
        );
        let detail = seen[0].detail.clone().unwrap_or_default();
        assert!(
            detail.contains("acl.default_role [security] = \"user\""),
            "{detail}"
        );
        assert!(
            config_text().contains("default_role = \"user\""),
            "{}",
            config_text()
        );
        assert!(!file.exists(), "the offer left the inbox");
        assert!(file
            .parent()
            .unwrap()
            .join("applied")
            .join(file.file_name().unwrap())
            .exists());

        assert!(ask_due_with(store, cfg_named("vps"), owner.clone()).is_none());
        assert_eq!(owner.seen().len(), 1, "nothing left to ask");
    }

    /// "later" keeps the offer and is not asked again for a day; once
    /// due, a plain "no" drops it (the change is never made here).
    #[tokio::test]
    async fn later_waits_a_day_and_no_drops_the_offer() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        let file = held_acl_offer();
        let req = Pending::Config {
            file: file.clone(),
            from: "laptop".into(),
            rows: vec![],
            security_only: true,
        }
        .request("vps", None);
        let no = crate::ask::parse_reply(&req, "no").expect("no is an answer");
        let owner = Owner::answering(vec![AskAnswer::choice("later"), no]);
        ask_due_with(store.clone(), cfg_named("vps"), owner.clone())
            .unwrap()
            .await
            .unwrap();
        assert!(file.exists(), "later leaves it held");
        let id = format!("config:{}", file.file_name().unwrap().to_string_lossy());
        let next = book_load()[&id].next_at;
        assert!(
            next >= now_secs() + REASK_LATER_SECS - 5,
            "a day out: {next}"
        );
        assert!(ask_due_with(store.clone(), cfg_named("vps"), owner.clone()).is_none());
        assert_eq!(owner.seen().len(), 1);

        book_set(&id, 0, None);
        ask_due_with(store, cfg_named("vps"), owner.clone())
            .unwrap()
            .await
            .unwrap();
        assert_eq!(owner.seen().len(), 2);
        let (r1, r2) = (
            owner.seen()[0].relay_key.clone(),
            owner.seen()[1].relay_key.clone(),
        );
        assert!(
            r1.is_some() && r1 != r2,
            "after a used answer, a new round: {r1:?} {r2:?}"
        );
        assert!(!file.exists());
        assert!(file
            .parent()
            .unwrap()
            .join("dropped")
            .join(file.file_name().unwrap())
            .exists());
        assert!(
            !config_text().contains("default_role"),
            "dropped is not applied"
        );
    }

    /// Due decisions are asked in PARALLEL (up to MAX_OPEN_ASKS), each as
    /// its own message, and the same decision is never asked twice at once
    /// — a held /tcl withheld ask must not serialize the ones behind it.
    #[tokio::test]
    async fn due_questions_are_asked_in_parallel() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        held_acl_offer();
        {
            let s = store.lock().unwrap();
            s.conn()
                .execute_batch(
                    "INSERT INTO session_queue (session_id, seq, body, state, kind)
                     VALUES ('moved-1', 1, 'hi', 'held', 'message');",
                )
                .unwrap();
        }
        // Auto-answering owner: each ask resolves immediately, so one tick
        // opens both due decisions and none serialize.
        let owner = Owner::answering(vec![AskAnswer::choice("apply"), AskAnswer::choice("run")]);
        ask_due_with(store.clone(), cfg_named("vps"), owner.clone())
            .expect("asked")
            .await
            .unwrap();
        // Give the parallel tasks a moment to finish acting.
        for _ in 0..50 {
            if owner.seen().len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let seen = owner.seen();
        assert_eq!(seen.len(), 2, "the config and the queue asks both went out");
        // Nothing left pending: a follow-up tick asks nothing.
        let owner2 = Owner::answering(vec![]);
        assert!(ask_due_with(store.clone(), cfg_named("vps"), owner2.clone()).is_none());
        assert!(owner2.seen().is_empty());
    }

    /// The vault keeps its out-of-band check: a fingerprint that is not
    /// the joining node's admits nothing, the request stays, and the
    /// re-ask says why; the right one admits it.
    #[tokio::test]
    async fn a_wrong_fingerprint_admits_nothing_and_the_reask_says_why() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        let mut mine = crate::vault::Vault::open_or_create_local().unwrap();
        mine.update(|v| v.init()).unwrap();
        let t = tempfile::tempdir().unwrap();
        let phone = crate::vault::Vault::open_or_create(t.path(), "phone-node", "phone").unwrap();
        let join = phone.join_request();
        let fp = join.fingerprint();
        let file = crate::vault::inbox_store(
            &crate::vault::inbox_dir(),
            "phone",
            "t1",
            crate::vault::Message::Join(join),
        )
        .unwrap();
        let owner = Owner::answering(vec![
            AskAnswer::text("dead-beef-dead-beef"),
            AskAnswer::text(&format!("  {fp} ")),
        ]);
        ask_due_with(store.clone(), cfg_named("laptop"), owner.clone())
            .unwrap()
            .await
            .unwrap();
        assert_eq!(
            owner.seen()[0].kind,
            AskKind::Clarify,
            "a fingerprint is typed"
        );
        assert!(file.exists(), "the request stays for a right answer");
        assert!(!crate::vault::Vault::open_local()
            .unwrap()
            .membership()
            .is_member("phone-node"));
        let id = format!("vault-join:{}", file.file_name().unwrap().to_string_lossy());
        let e = book_load()[&id].clone();
        assert!(e.next_at <= now_secs() + RETRY_SECS);
        book_set(&id, 0, e.note);
        ask_due_with(store, cfg_named("laptop"), owner.clone())
            .unwrap()
            .await
            .unwrap();
        let again = owner.seen()[1].detail.clone().unwrap_or_default();
        assert!(again.contains("fingerprint mismatch"), "{again}");
        assert!(crate::vault::Vault::open_local()
            .unwrap()
            .membership()
            .is_member("phone-node"));
        assert!(!file.exists());
    }

    /// A node with no vault is offered the pool's: adopted only with the
    /// pool root's fingerprint, as `vault apply --inbox --trust` is.
    #[tokio::test]
    async fn a_vault_is_adopted_only_with_the_roots_fingerprint() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        // This node has keys (it sent a join) but no vault.
        let here = crate::vault::Vault::open_or_create_local().unwrap();
        let t = tempfile::tempdir().unwrap();
        let mut pool = crate::vault::Vault::open_or_create(t.path(), "vps-node", "vps").unwrap();
        pool.init().unwrap();
        let join = here.join_request();
        let fp = join.fingerprint();
        pool.admit(&join, &fp).unwrap();
        let root = pool.membership().root.unwrap();
        let root_fp = crate::vault::fingerprint(&root.recipient, &root.sign_pk);
        let file = crate::vault::inbox_store(
            &crate::vault::inbox_dir(),
            "vps",
            "t1",
            crate::vault::Message::Offer {
                from_node: "vps-node".into(),
                from_name: "vps".into(),
                file: pool.file.clone(),
            },
        )
        .unwrap();
        let owner = Owner::answering(vec![
            AskAnswer::text("dead-beef-dead-beef"),
            AskAnswer::text(&root_fp),
        ]);
        ask_due_with(store.clone(), cfg_named("phone"), owner.clone())
            .unwrap()
            .await
            .unwrap();
        assert!(owner.seen()[0]
            .prompt
            .contains("vps offers its vault to phone"));
        assert!(file.exists(), "a wrong fingerprint adopts nothing");
        assert!(!crate::vault::Vault::open_local().unwrap().is_initialized());
        let id = format!(
            "vault-adopt:{}",
            file.file_name().unwrap().to_string_lossy()
        );
        book_set(&id, 0, None);
        ask_due_with(store, cfg_named("phone"), owner.clone())
            .unwrap()
            .await
            .unwrap();
        assert!(!file.exists());
        let v = crate::vault::Vault::open_local().unwrap();
        assert!(v.is_initialized() && v.membership().is_member(&here.join_request().node));
    }

    /// "take theirs" makes the incoming content the newest revision
    /// here; "keep mine" leaves the local one. Both settle the offer.
    #[tokio::test]
    async fn a_memory_conflict_is_settled_either_way() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        let mut cfg = crate::config::VelesConfig::default();
        cfg.cluster.memsync_auto = true;
        cfg.node.name = Some("laptop".into());
        let cfg = Arc::new(cfg);
        let here = crate::memory::MemoryStore::new();
        let st = tempfile::tempdir().unwrap();
        let there = crate::memory::MemoryStore::at(st.path().to_path_buf());
        there.write_file("system/core.md", "base").unwrap();
        crate::memsync::apply_offer(
            &here,
            &crate::memsync::build_offer(&there, "vps-node", "vps", 0).unwrap(),
        )
        .unwrap();
        there.write_file("system/core.md", "base +vps").unwrap();
        here.write_file("system/core.md", "base +laptop").unwrap();
        let store_conflict = || {
            crate::memsync::inbox_store(
                &crate::memsync::inbox_dir(),
                "vps",
                "t",
                crate::memsync::build_offer(&there, "vps-node", "vps", 0).unwrap(),
            )
            .unwrap()
        };

        let f1 = store_conflict();
        let owner = Owner::answering(vec![AskAnswer::choice("mine"), AskAnswer::choice("theirs")]);
        ask_due_with(store.clone(), cfg.clone(), owner.clone())
            .unwrap()
            .await
            .unwrap();
        assert!(owner.seen()[0]
            .prompt
            .contains("memory from vps: 1 file(s)"));
        assert!(!f1.exists());
        assert_eq!(
            here.latest_content("system/core.md").unwrap(),
            "base +laptop"
        );

        let f2 = store_conflict();
        ask_due_with(store, cfg, owner.clone())
            .unwrap()
            .await
            .unwrap();
        assert!(!f2.exists());
        assert_eq!(here.latest_content("system/core.md").unwrap(), "base +vps");
    }

    /// The 2026-10-06 live race: the offer's tip is already local, so
    /// the normal drain archives it cleanly. The ask scanner must see no
    /// memory conflict after that drain, not mistake inbox presence for
    /// proof that the two sides diverged.
    #[tokio::test]
    async fn a_clean_memory_echo_is_not_an_owner_decision() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        let mut cfg = crate::config::VelesConfig::default();
        cfg.cluster.memsync_auto = true;
        let here = crate::memory::MemoryStore::new();
        let st = tempfile::tempdir().unwrap();
        let there = crate::memory::MemoryStore::at(st.path().to_path_buf());
        there
            .write_file("system/core.md", "the shared tip")
            .unwrap();
        crate::memsync::apply_offer(
            &here,
            &crate::memsync::build_offer(&there, "vps-node", "vps", 0).unwrap(),
        )
        .unwrap();
        let file = crate::memsync::inbox_store(
            &crate::memsync::inbox_dir(),
            "vps",
            "echo",
            crate::memsync::build_offer(&there, "vps-node", "vps", 0).unwrap(),
        )
        .unwrap();

        assert!(file.exists(), "the offer starts stored, not classified");
        let owner = Owner::answering(vec![]);
        // ask_due_with must drain BEFORE scanning: no manual apply here,
        // because that would hide the receive/tick race this test pins.
        assert!(ask_due_with(store, Arc::new(cfg), owner.clone()).is_none());
        assert!(!file.exists(), "the normal drain archived the clean echo");
        assert!(owner.seen().is_empty(), "no false conflict was asked");
    }

    /// A Telegram answer can arrive just after the drain or an operator
    /// on another surface has archived the offer. That answer completed
    /// the outstanding question; ENOENT is not a reason to ask it again.
    #[test]
    fn an_answer_to_an_already_settled_config_offer_is_success() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        let file = held_acl_offer();
        let p = Pending::Config {
            file: file.clone(),
            from: "laptop".into(),
            rows: vec![],
            security_only: true,
        };
        move_to(&file, "applied").unwrap();
        let a = act(
            &store,
            &crate::config::VelesConfig::default(),
            &p,
            &AskAnswer::choice("apply"),
        )
        .unwrap();
        assert_eq!(a.again_in, None, "{a:?}");
        assert!(a.line.contains("already settled"), "{}", a.line);
        let owner = Owner::answering(vec![]);
        assert!(ask_due_with(store, cfg_named("vps"), owner.clone()).is_none());
        assert!(owner.seen().is_empty());
    }

    /// A late typed vault fingerprint is still checked against the join
    /// that was asked about. Duplicate admission is a no-op only after
    /// that check, never a bypass of the out-of-band proof.
    #[tokio::test]
    async fn a_late_duplicate_vault_answer_still_needs_its_fingerprint() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        let mut mine = crate::vault::Vault::open_or_create_local().unwrap();
        mine.update(|v| v.init()).unwrap();
        let t = tempfile::tempdir().unwrap();
        let phone = crate::vault::Vault::open_or_create(t.path(), "phone-node", "phone").unwrap();
        let join = phone.join_request();
        let fp = join.fingerprint();
        let file = crate::vault::inbox_store(
            &crate::vault::inbox_dir(),
            "phone",
            "duplicate-join",
            crate::vault::Message::Join(join.clone()),
        )
        .unwrap();
        let p = Pending::VaultJoin {
            file: file.clone(),
            request: join.clone(),
        };
        mine.update(|v| v.admit(&join, &fp)).unwrap();
        crate::vault::apply_inbox_auto().unwrap();
        assert!(!file.exists(), "the drain retired the duplicate join");

        let bad = act(
            &store,
            &crate::config::VelesConfig::default(),
            &p,
            &AskAnswer::text("wrong"),
        );
        assert!(bad.is_err(), "a late answer still cannot guess the keys");
        let good = act(
            &store,
            &crate::config::VelesConfig::default(),
            &p,
            &AskAnswer::text(&fp),
        )
        .unwrap();
        assert_eq!(good.again_in, None);
        let owner = Owner::answering(vec![]);
        assert!(ask_due_with(store, cfg_named("vps"), owner.clone()).is_none());
        assert!(owner.seen().is_empty());
    }

    /// A question that never reached the owner is asked again in
    /// minutes, not hours.
    #[tokio::test]
    async fn an_undelivered_question_is_retried_soon() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        let file = held_acl_offer();
        let owner = Owner::answering(vec![]); // times out at once
        ask_due_with(store, cfg_named("vps"), owner)
            .unwrap()
            .await
            .unwrap();
        let id = format!("config:{}", file.file_name().unwrap().to_string_lossy());
        assert!(book_load()[&id].next_at <= now_secs() + RETRY_SECS);
        assert!(
            book_load()[&id].round.is_some(),
            "the retry is the same round: the question may be open where it went"
        );
    }

    /// A phone asks, then stops for idleness long before the owner
    /// answers (2026-10-13: its config approval, answered on Telegram,
    /// found nothing waiting). Restarted, it asks again at once — not
    /// after the six hours the book says — in the SAME round, so the node
    /// that put the question to the owner hands over the answer given
    /// meanwhile; once used, the round is over.
    #[tokio::test]
    async fn a_round_out_at_a_restart_is_asked_again_at_once_in_the_same_round() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        let file = held_acl_offer();
        let id = format!("config:{}", file.file_name().unwrap().to_string_lossy());
        // The first run: asked, and stopped before anyone answered.
        let first = Arc::new(Owner {
            answers: Mutex::new(VecDeque::new()),
            seen: Mutex::new(Vec::new()),
            hold: Some(Arc::new(tokio::sync::Notify::new())),
        });
        let task = ask_due_with(store.clone(), cfg_named("phone"), first.clone()).expect("asked");
        for _ in 0..200 {
            if !first.seen().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let round = first.seen()[0]
            .relay_key
            .clone()
            .expect("a sync question names its round");
        task.abort();
        let _ = task.await;
        let e = book_load()[&id].clone();
        assert!(e.next_at > now_secs() + 3600, "the book says hours");
        assert_eq!(e.round.as_deref(), Some(round.as_str()));
        assert!(
            ask_due_with(store.clone(), cfg_named("phone"), first.clone()).is_none(),
            "the same run does not ask again before its time"
        );

        // The next run (a new process: nothing asked in it yet).
        rejoined_set().lock().unwrap().remove(&id);
        let second = Owner::answering(vec![AskAnswer::choice("apply")]);
        ask_due_with(store.clone(), cfg_named("phone"), second.clone())
            .expect("asked again at once")
            .await
            .unwrap();
        assert_eq!(second.seen()[0].relay_key.as_deref(), Some(round.as_str()));
        assert!(
            config_text().contains("default_role = \"user\""),
            "the answer was used: {}",
            config_text()
        );
        assert!(!file.exists());
    }

    /// Choice questions are approvals: text that is not a choice flows
    /// on as an ordinary message instead of answering (a clarify would
    /// take the owner's next Telegram message, whatever it said).
    #[test]
    fn chat_in_the_home_channel_does_not_answer_a_sync_question() {
        let req = Pending::QueueHeld {
            session: "s".into(),
            count: 2,
        }
        .request("vps", None);
        assert!(crate::ask::parse_reply(&req, "how's the weather on the vps").is_none());
        assert_eq!(
            crate::ask::parse_reply(&req, "1").and_then(|a| a.first().map(String::from)),
            Some("run".into())
        );
        assert_eq!(
            crate::ask::parse_reply(&req, "leave").and_then(|a| a.first().map(String::from)),
            Some("leave".into())
        );
    }

    /// A registered source's decisions are scanned, asked with its own
    /// question, and decided by it with the answer as delivered; an
    /// unloaded source's decision is kept, never dropped.
    struct StubSource;
    impl PendingSource for StubSource {
        fn scan(&self) -> Vec<String> {
            vec!["one".into()]
        }
        fn question(&self, id: &str, node_name: &str) -> SourceQuestion {
            SourceQuestion {
                kind: AskKind::Approval,
                prompt: format!("stub {id} on {node_name}?"),
                detail: "d".into(),
                choices: vec![AskChoice::new("go", "go"), AskChoice::new("keep", "keep")],
                image: Some("iVBORw0KGgo=".into()),
            }
        }
        fn decide(&self, id: &str, pick: &str, _free_text: &str) -> Result<SourceOutcome> {
            Ok(match pick {
                "go" => SourceOutcome::Decided(format!("{id} went")),
                _ => SourceOutcome::Later(format!("{id} kept")),
            })
        }
    }

    /// Takes the stub source back out when the test ends, however it ends:
    /// the registry is process-global and the other tests count the scan.
    struct Unregister(&'static str);
    impl Drop for Unregister {
        fn drop(&mut self) {
            unregister_source(self.0);
        }
    }

    #[test]
    fn a_registered_source_is_scanned_asked_and_decided_as_its_own() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d, store) = scratch_store();
        register_source("stubsrc", Arc::new(StubSource));
        let _cleanup = Unregister("stubsrc");
        let p = {
            let s = store.lock().unwrap();
            scan(&s, &crate::config::VelesConfig::default())
                .into_iter()
                .find(|p| p.id() == "stubsrc:one")
                .expect("scanned")
        };
        let req = p.request("laptop", None);
        assert!(req.prompt.contains("stub one on laptop?"), "{}", req.prompt);
        assert_eq!(
            req.image.as_deref(),
            Some("iVBORw0KGgo="),
            "the source's picture rides the ask"
        );
        let a = act(
            &store,
            &crate::config::VelesConfig::default(),
            &p,
            &AskAnswer::choice("go"),
        )
        .unwrap();
        assert_eq!(a.again_in, None, "decided");
        assert_eq!(a.line, "one went");
        let a = act(
            &store,
            &crate::config::VelesConfig::default(),
            &p,
            &AskAnswer::choice("keep"),
        )
        .unwrap();
        assert!(a.again_in.is_some(), "kept: asked again later");
        // The source gone (purged with its module): its decision is not
        // dropped, the answer is retried later.
        assert_eq!(
            purge_owner_sources(None),
            0,
            "core's install is nobody's to purge"
        );
        let gone = Pending::External {
            source: "nobody".into(),
            id: "x".into(),
        };
        assert!(gone.request("laptop", None).prompt.contains("not loaded"));
        assert!(act(
            &store,
            &crate::config::VelesConfig::default(),
            &gone,
            &AskAnswer::choice("go")
        )
        .is_err());
    }
}
