//! Failover (PLAN-pool-round.md C2): the sessions of a node that stopped
//! answering are taken over by a survivor, from the survivor's replica of
//! it (B5), through the adopt a migration uses (B6) — so the frozen
//! prompt travels here too, and the provider's cache survives the
//! failover as it survives a move.
//!
//! Who may take a session over (USER 2026-09-29, "you break the tie"):
//!
//! - **the arbiter**, once the owner's node lease (`node:<id>`, held by
//!   every gateway with `[cluster] failover`) has run out: it grants
//!   `takeover:<owner>:<session>` to the first survivor that asks, and
//!   keeps the grant ([`TAKEOVER_TTL_SECS`]) so the owner can check it
//!   when it comes back;
//! - **a ticket**, when the arbiter is the node that died: one approval,
//!   signed with any trusted key — on the survivor's own device too, which
//!   in a two-device pool is the only one left. The gateway holds no key,
//!   so the approval is a person's;
//! - **nobody, for a fork**: continuing a replica's session under a NEW
//!   id changes nothing the owner has, so it needs no permission. It is
//!   the way on when nobody is there to sign.
//!
//! When the owner comes back it finds the takeover record in its replica
//! of the survivor, checks the authorization itself, and yields: what it
//! wrote after the last message the survivor had becomes a fork session,
//! and its own copy is tombstoned to the survivor. Nothing is lost, and
//! one session id never keeps two writers.
//!
//! Takeover is LAZY (C3): a desktop turn or a chat message for a dead
//! node's session takes it over through the arbiter ([`bring_here`]), so
//! a session moves when someone wants to talk to it — a laptop that goes
//! to sleep does not lose every session to the phone.
//!
//! Not carried: checkpoint blobs (file undo — a replica holds rows only)
//! and the owner's background tasks.

use crate::migration::{self, AdoptReport, SessionRow};
use crate::store::Store;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension as _};
use serde::{Deserialize, Serialize};

/// The tool name a takeover ticket is signed for.
pub const TAKEOVER_TOOL: &str = "session_takeover";

/// How long the arbiter keeps a takeover grant: long enough for the owner
/// to come back and check it.
pub const TAKEOVER_TTL_SECS: i64 = 30 * 24 * 3600;

/// The lease a node holds while it is alive (`[cluster] failover`).
pub fn node_key(node: &str) -> String {
    format!("node:{node}")
}

/// The arbiter's grant to take `session` over from `owner`.
pub fn takeover_key(owner: &str, session: &str) -> String {
    format!("takeover:{owner}:{session}")
}

/// The call a takeover ticket authorizes, as its arguments.
pub fn takeover_args(session: &str, owner: &str) -> serde_json::Value {
    serde_json::json!({ "session": session, "from_node": owner })
}

/// The record a takeover leaves in the survivor's store. The table
/// replicates, which is how the owner learns of it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TakeoverRecord {
    /// The subtree root taken over.
    pub session_id: String,
    pub from_node: String,
    pub to_node: String,
    /// The owner's newest message id the replica held (the owner's id
    /// space: replicas keep ids). Anything the owner wrote above it, it
    /// wrote after the takeover's copy was made.
    pub last_source_msg_id: i64,
    /// `arbiter:<node id>` or `ticket:<veles-ticket:1.…>`.
    pub authorized_by: String,
    pub at: String,
}

/// What a takeover or a fork would start from.
#[derive(Debug, Clone)]
pub struct Plan {
    pub owner: String,
    pub owner_name: String,
    pub root: String,
    pub subtree: Vec<SessionRow>,
    pub last_source_msg_id: i64,
    pub messages: i64,
}

fn node_name(store: &Store, id: &str) -> String {
    store
        .node_get(id)
        .ok()
        .flatten()
        .map(|n| n.name)
        .unwrap_or_else(|| id.to_string())
}

/// Find `session` in the replicas this node holds, and say what a
/// takeover of it would carry. Refused when the session is live here
/// already, when no replica has it, and when its owner had it parked
/// mid-migration (then it may already live on a third node).
pub fn plan(store: &Store, session: &str) -> Result<Plan> {
    if let Some(stamp) = store.session_node_stamp(session)? {
        anyhow::ensure!(
            migration::is_retired(Some(&stamp)),
            "session {session} is live on this node already ({stamp})"
        );
    } else if store.session_exists(session)? {
        anyhow::bail!("session {session} is live on this node already");
    }
    let mut moved = Vec::new();
    for info in crate::storesync::list_replicas()? {
        let conn = crate::storesync::open_replica(&info.node)?;
        let stamp: Option<Option<String>> = conn
            .query_row(
                "SELECT node FROM sessions WHERE id = ?1",
                rusqlite::params![session],
                |r| r.get(0),
            )
            .optional()?;
        let Some(stamp) = stamp else { continue };
        if migration::is_retired(stamp.as_deref()) {
            moved.push(format!("{} ({})", info.node, stamp.unwrap_or_default()));
            continue;
        }
        anyhow::ensure!(
            !migration::is_parked(stamp.as_deref()),
            "session {session} was mid-migration on {} ({}) — it may live on its target \
             already; ask there first",
            info.node,
            stamp.unwrap_or_default()
        );
        let subtree = migration::session_subtree_on(&conn, session)?;
        let ids: Vec<String> = subtree.iter().map(|s| s.id.clone()).collect();
        let (mut last, mut messages) = (0i64, 0i64);
        for id in &ids {
            let (m, n): (i64, i64) = conn.query_row(
                "SELECT COALESCE(MAX(msg_id), 0), COUNT(*) FROM messages WHERE session_id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            last = last.max(m);
            messages += n;
        }
        return Ok(Plan {
            owner_name: node_name(store, &info.node),
            owner: info.node,
            root: session.to_string(),
            subtree,
            last_source_msg_id: last,
            messages,
        });
    }
    if moved.is_empty() {
        anyhow::bail!("no replica held here has session {session}");
    }
    anyhow::bail!(
        "session {session} had already moved on from {} — take it over from where it went",
        moved.join(", ")
    )
}

/// The ticket request a person signs to authorize taking `plan` over on
/// this node, when the arbiter cannot.
pub fn ticket_request(me: &str, plan: &Plan) -> Result<String> {
    crate::ticket::TicketRequest::new(
        me,
        me,
        &plan.root,
        TAKEOVER_TOOL,
        takeover_args(&plan.root, &plan.owner),
        "owner",
        crate::ticket::DEFAULT_TTL_SECS,
    )?
    .encode()
}

/// Check a takeover ticket for `plan` on this node and spend it.
pub fn spend_ticket(
    store: &Store,
    trusted: &[crate::ticket::TrustedSigner],
    me: &str,
    plan: &Plan,
    ticket_text: &str,
) -> Result<String> {
    let ticket = crate::ticket::Ticket::parse(ticket_text)?;
    let now = chrono::Utc::now().timestamp();
    let args = takeover_args(&plan.root, &plan.owner);
    let v = crate::ticket::verify(
        &ticket,
        trusted,
        &crate::ticket::CallContext {
            me,
            origin: me,
            session: &plan.root,
            tool: TAKEOVER_TOOL,
            args: &args,
            now,
        },
    )?;
    crate::ticket::spend(store, &v, now)?;
    Ok(format!("ticket:{}", ticket_text.trim()))
}

/// The injected row a taken-over session's model reads first.
fn taken_over_note(plan: &Plan, me: &str, me_name: &str) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".into());
    format!(
        "[taken over] Node {from} ({from_id}) stopped answering, and node {me_name} ({me}) \
         took this session over at {at} from its copy of {from}'s history. Anything {from} \
         wrote after that copy was made is not here. The session runs on {me_name} now: \
         tools act on this machine, and the working directory is {cwd}. The environment the \
         system prompt above describes is {from}'s. File checkpoints (undo) stayed on {from}.",
        from = plan.owner_name,
        from_id = plan.owner,
        at = crate::a2a::now_iso(),
    )
}

/// Take `plan` over on this node, authorized as `authorized_by` (the
/// caller has checked it). One transaction: the sessions from the
/// replica, and the takeover record the owner will find.
pub fn take_over(
    store: &Store,
    me: &str,
    me_name: &str,
    plan: &Plan,
    authorized_by: &str,
) -> Result<AdoptReport> {
    let conn = crate::storesync::open_replica(&plan.owner)?;
    let bundle =
        migration::build_bundle_on(&conn, false, &plan.owner, &plan.owner_name, me, &plan.root)?;
    let record = TakeoverRecord {
        session_id: plan.root.clone(),
        from_node: plan.owner.clone(),
        to_node: me.to_string(),
        last_source_msg_id: plan.last_source_msg_id,
        authorized_by: authorized_by.to_string(),
        at: crate::a2a::now_iso(),
    };
    migration::adopt_with(
        store,
        me,
        &bundle,
        Some(taken_over_note(plan, me, me_name)),
        &|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO session_takeovers
                     (session_id, from_node, to_node, last_source_msg_id, authorized_by, at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    record.session_id,
                    record.from_node,
                    record.to_node,
                    record.last_source_msg_id,
                    record.authorized_by,
                    record.at
                ],
            )?;
            Ok(())
        },
    )
}

/// Fresh ids for every session of a bundle (a fork is a new session).
fn fork_ids(bundle: &migration::MigrationBundle) -> std::collections::HashMap<String, String> {
    bundle
        .sessions
        .iter()
        .map(|s| (s.id.clone(), crate::store::new_session_id()))
        .collect()
}

/// Continue a replica's session HERE under a new id. Needs nobody's
/// permission: the owner's session is untouched, and the two go their own
/// ways. Returns the new root id.
pub fn fork_from_replica(
    store: &Store,
    me: &str,
    me_name: &str,
    session: &str,
) -> Result<(String, AdoptReport)> {
    let plan = plan(store, session)?;
    let conn = crate::storesync::open_replica(&plan.owner)?;
    let mut bundle =
        migration::build_bundle_on(&conn, false, &plan.owner, &plan.owner_name, me, &plan.root)?;
    let ids = fork_ids(&bundle);
    migration::rename_sessions(&mut bundle, &ids);
    let root = ids[&plan.root].clone();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".into());
    let note = format!(
        "[forked] This is a copy of session {session} from node {from} ({from_id}), as this \
         node last received its history, continued on node {me_name} ({me}) as a session of its \
         own from {at}. {from}'s session is untouched and goes its own way. Tools act on \
         {me_name} now, and the working directory is {cwd}; the environment the system prompt \
         above describes is {from}'s. File checkpoints (undo) stayed on {from}.",
        from = plan.owner_name,
        from_id = plan.owner,
        at = crate::a2a::now_iso(),
    );
    let report = migration::adopt_with(store, me, &bundle, Some(note), &|_| Ok(()))?;
    Ok((root, report))
}

// ── the owner, back: learning and yielding ─────────────────────────

/// Node-local bookkeeping: the takeover records this node has settled.
const SETTLED_SQL: &str = "CREATE TABLE IF NOT EXISTS takeovers_settled (
        session_id TEXT NOT NULL,
        to_node TEXT NOT NULL,
        at TEXT NOT NULL,
        verdict TEXT NOT NULL,
        settled_at TEXT DEFAULT (datetime('now')),
        PRIMARY KEY (session_id, to_node, at)
    )";

/// Takeover records naming THIS node as the owner that it has not
/// settled yet, from every replica it holds.
pub fn unsettled(store: &Store, me: &str) -> Result<Vec<TakeoverRecord>> {
    not_yet_settled(store, replica_takeovers(me)?)
}

/// Every replica's takeover records naming `me` as the owner, settled
/// or not. It reads only the replica files, so it needs no lock on this
/// node's store. Opening and reading them with that lock held kept
/// it for up to 18.6 s on a busy disk (`takeover::unsettled`).
pub fn replica_takeovers(me: &str) -> Result<Vec<TakeoverRecord>> {
    let mut out = Vec::new();
    for info in crate::storesync::list_replicas()? {
        let conn = crate::storesync::open_replica(&info.node)?;
        let mut stmt = conn.prepare(
            "SELECT session_id, from_node, to_node, last_source_msg_id, authorized_by, at
             FROM session_takeovers WHERE from_node = ?1 AND to_node = ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![me, info.node], |r| {
                Ok(TakeoverRecord {
                    session_id: r.get(0)?,
                    from_node: r.get(1)?,
                    to_node: r.get(2)?,
                    last_source_msg_id: r.get(3)?,
                    authorized_by: r.get(4)?,
                    at: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        out.extend(rows);
    }
    Ok(out)
}

/// The records in `recs` that this node has not settled yet.
pub fn not_yet_settled(store: &Store, recs: Vec<TakeoverRecord>) -> Result<Vec<TakeoverRecord>> {
    store.conn().execute_batch(SETTLED_SQL)?;
    let mut out = Vec::new();
    for rec in recs {
        let seen: bool = store.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM takeovers_settled
             WHERE session_id = ?1 AND to_node = ?2 AND at = ?3)",
            rusqlite::params![rec.session_id, rec.to_node, rec.at],
            |r| r.get(0),
        )?;
        if !seen {
            out.push(rec);
        }
    }
    Ok(out)
}

/// What the owner makes of a takeover record.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// The authorization holds: give the session up.
    Yield,
    /// It does not: keep the session, and say why.
    Refuse(String),
    /// Cannot tell yet (the arbiter is unreachable): ask again later.
    Unknown(String),
}

/// Check a ticket-authorized record as the OWNER: the signature, against
/// this node's own trusted signers. The ticket's expiry is not judged
/// here — it bounded when the survivor could use it, which the owner
/// cannot see.
pub fn check_ticket(rec: &TakeoverRecord, trusted: &[crate::ticket::TrustedSigner]) -> Verdict {
    let Some(text) = rec.authorized_by.strip_prefix("ticket:") else {
        return Verdict::Refuse(format!("not a ticket: {}", rec.authorized_by));
    };
    let ticket = match crate::ticket::Ticket::parse(text) {
        Ok(t) => t,
        Err(e) => return Verdict::Refuse(format!("{e:#}")),
    };
    let args = takeover_args(&rec.session_id, &rec.from_node);
    match crate::ticket::verify(
        &ticket,
        trusted,
        &crate::ticket::CallContext {
            me: &rec.to_node,
            origin: &rec.to_node,
            session: &rec.session_id,
            tool: TAKEOVER_TOOL,
            args: &args,
            now: ticket.exp,
        },
    ) {
        Ok(_) => Verdict::Yield,
        Err(e) => Verdict::Refuse(format!("{e:#}")),
    }
}

/// Check an arbiter-authorized record as the OWNER: the pool's arbiter
/// must be the one named, and must still hold the grant for the survivor.
pub async fn check_arbiter(
    rec: &TakeoverRecord,
    arbiter: Option<&crate::arbiter::StaticArbiter>,
) -> Verdict {
    use crate::arbiter::{Arbiter as _, LeaseState};
    let Some(named) = rec.authorized_by.strip_prefix("arbiter:") else {
        return Verdict::Refuse(format!("not an arbiter grant: {}", rec.authorized_by));
    };
    let Some(arb) = arbiter else {
        return Verdict::Unknown("no arbiter is configured on this node to confirm it".into());
    };
    if arb.arbiter_id() != named {
        return Verdict::Refuse(format!(
            "granted by {named}, which is not this pool's arbiter ({})",
            arb.arbiter_id()
        ));
    }
    match arb
        .lease_status(&takeover_key(&rec.from_node, &rec.session_id))
        .await
    {
        LeaseState::HeldByOther { holder, .. } if holder == rec.to_node => Verdict::Yield,
        LeaseState::HeldByOther { holder, .. } => Verdict::Refuse(format!(
            "the arbiter says {holder} holds that grant, not {}",
            rec.to_node
        )),
        LeaseState::Held(_) => Verdict::Refuse("the arbiter says THIS node holds it".into()),
        LeaseState::Refused(r) if r == "no lease" => {
            Verdict::Refuse("the arbiter has no such grant".into())
        }
        LeaseState::Refused(r) => Verdict::Unknown(r),
    }
}

/// Act on a verdict. Yield: tombstone the local subtree to the survivor;
/// what this node wrote after the survivor's copy continues as a fork
/// (full history plus that tail). Refuse: keep the session, record why.
/// Unknown: nothing recorded — asked again next time. Returns what
/// happened, for the log and the CLI.
pub fn settle(
    store: &Store,
    me: &str,
    me_name: &str,
    rec: &TakeoverRecord,
    verdict: &Verdict,
) -> Result<String> {
    store.conn().execute_batch(SETTLED_SQL)?;
    let mark = |conn: &Connection, v: &str| -> Result<()> {
        conn.execute(
            "INSERT OR IGNORE INTO takeovers_settled (session_id, to_node, at, verdict)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![rec.session_id, rec.to_node, rec.at, v],
        )?;
        Ok(())
    };
    let why = match verdict {
        Verdict::Unknown(why) => {
            return Ok(format!(
                "{}: taken over by {}? cannot confirm yet ({why})",
                rec.session_id, rec.to_node
            ))
        }
        Verdict::Refuse(why) => why.clone(),
        Verdict::Yield => String::new(),
    };
    if !why.is_empty() {
        mark(store.conn(), &format!("refused: {why}"))?;
        return Ok(format!(
            "{}: {} claims to have taken it over, but the claim does not hold ({why}) — it \
             stays here",
            rec.session_id, rec.to_node
        ));
    }
    let subtree = migration::session_subtree(store, &rec.session_id)?;
    let live: Vec<SessionRow> = subtree
        .into_iter()
        .filter(|s| !migration::is_retired(s.node.as_deref()))
        .collect();
    if live.is_empty() {
        mark(store.conn(), "yielded: nothing live here")?;
        return Ok(format!("{}: nothing live here to give up", rec.session_id));
    }
    let tail: i64 = {
        let mut n = 0i64;
        for s in &live {
            n += store.conn().query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND msg_id > ?2",
                rusqlite::params![s.id, rec.last_source_msg_id],
                |r| r.get::<_, i64>(0),
            )?;
        }
        n
    };
    if tail == 0 {
        let conn = store.conn();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let r = (|| -> Result<()> {
            migration::retire_on(conn, &live, &rec.to_node)?;
            mark(conn, "yielded")
        })();
        match r {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
        return Ok(format!(
            "{}: yielded to {} — nothing was written here after its copy",
            rec.session_id, rec.to_node
        ));
    }
    // Written here after the survivor's copy: that tail must not vanish.
    // The whole local history continues as a fork, and the original is
    // tombstoned in the same transaction.
    let mut bundle = migration::build_bundle(store, me, me_name, me, &rec.session_id)?;
    let ids = fork_ids(&bundle);
    migration::rename_sessions(&mut bundle, &ids);
    let fork = ids[&rec.session_id].clone();
    let survivor = node_name(store, &rec.to_node);
    let note = format!(
        "[forked] While node {me_name} ({me}) was out of contact, node {survivor} ({to}) took \
         session {sid} over. What was written here meanwhile continues in this copy, {fork}, \
         with the history before it; session {sid} itself goes on on {survivor}.",
        to = rec.to_node,
        sid = rec.session_id,
    );
    migration::adopt_with(store, me, &bundle, Some(note), &|tx| {
        migration::retire_on(tx, &live, &rec.to_node)?;
        mark(
            tx,
            &format!("yielded; {tail} row(s) written here continue in {fork}"),
        )
    })?;
    Ok(format!(
        "{}: yielded to {survivor} — {tail} message(s) written here after its copy continue \
         in the fork {fork}",
        rec.session_id
    ))
}

/// The owner's whole pass: find unsettled records, check each, settle.
/// Store work runs on the blocking pool; the arbiter ask is awaited.
/// Each line comes with whether it SETTLED the record — an unconfirmable
/// one is asked about again next pass, and a caller that runs every tick
/// need not say so every tick.
pub async fn learn(
    store: std::sync::Arc<std::sync::Mutex<Store>>,
    arbiter: Option<std::sync::Arc<crate::arbiter::StaticArbiter>>,
) -> Result<Vec<(bool, String)>> {
    let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?;
    let (s, id) = (store.clone(), me.id.clone());
    let records = crate::rt::spawn_blocking(move || {
        let recs = replica_takeovers(&id)?;
        if recs.is_empty() {
            return Ok(Vec::new());
        }
        not_yet_settled(&crate::store::lock_timed(&s, "takeover::unsettled"), recs)
    })
    .await??;
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let trusted = crate::config::load()
        .map(|c| crate::ticket::trusted_signers(&c))
        .unwrap_or_default();
    let mut out = Vec::new();
    for rec in records {
        let verdict = if rec.authorized_by.starts_with("ticket:") {
            check_ticket(&rec, &trusted)
        } else {
            check_arbiter(&rec, arbiter.as_deref()).await
        };
        let settled = !matches!(verdict, Verdict::Unknown(_));
        let (s, me2) = (store.clone(), me.clone());
        let line = crate::rt::spawn_blocking(move || {
            let s = crate::store::lock_timed(&s, "takeover::settle");
            settle(&s, &me2.id, &me2.name, &rec, &verdict)
        })
        .await??;
        out.push((settled, line));
    }
    Ok(out)
}

// ── lazy failover: a session is taken over when someone talks to it ──

/// Where a session lives, as far as this node can tell.
#[derive(Debug, Clone, PartialEq)]
pub enum Whereabouts {
    /// Live in this store (parked mid-migration counts: the fence says so).
    Here,
    /// Tombstoned here: it went to `node`.
    Moved { node: String },
    /// Not in this store; live in this node's replica of `node`.
    Replica { node: String },
    /// Nowhere this node knows of.
    Unknown,
}

/// The node a `migrated_to:<node>@<epoch>` tombstone names.
pub fn retired_to(stamp: &str) -> Option<String> {
    stamp
        .strip_prefix(migration::RETIRED_PREFIX)
        .and_then(|rest| rest.split('@').next())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

pub fn whereabouts(store: &Store, session: &str) -> Result<Whereabouts> {
    if store.session_exists(session)? {
        return Ok(
            match store
                .session_node_stamp(session)?
                .as_deref()
                .and_then(retired_to)
            {
                Some(node) => Whereabouts::Moved { node },
                None => Whereabouts::Here,
            },
        );
    }
    for info in crate::storesync::list_replicas()? {
        let conn = crate::storesync::open_replica(&info.node)?;
        let stamp: Option<Option<String>> = conn
            .query_row(
                "SELECT node FROM sessions WHERE id = ?1",
                rusqlite::params![session],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(stamp) = stamp {
            if !migration::is_retired(stamp.as_deref()) {
                return Ok(Whereabouts::Replica { node: info.node });
            }
        }
    }
    Ok(Whereabouts::Unknown)
}

/// What [`bring_here`] found or did.
#[derive(Debug, Clone, PartialEq)]
pub enum Brought {
    /// Runnable here (or unknown: the caller's own not-found path runs).
    Here,
    /// Its owner had stopped answering; this node took it over just now.
    TookOver(String),
    /// It lives on another node, which is alive (or cannot be judged).
    Elsewhere {
        node: String,
        name: String,
        why: String,
    },
}

/// Make `session` runnable here when it is this node's to run: live here
/// already, or its owner has stopped answering and the arbiter lets this
/// node take it over. Takeover is LAZY (USER 2026-09-29: automatic while
/// the arbiter is alive): a session moves when someone wants to talk to
/// it, never wholesale. Otherwise the answer says where it lives.
///
/// Needs `[cluster] failover` here; without it a session elsewhere is
/// only ever reported, never taken.
pub async fn bring_here(
    store: std::sync::Arc<std::sync::Mutex<Store>>,
    cfg: std::sync::Arc<crate::config::VelesConfig>,
    session: &str,
) -> Result<Brought> {
    use crate::arbiter::{Arbiter as _, LeaseState};
    let (s, sid) = (store.clone(), session.to_string());
    let (where_, name_of) = crate::rt::spawn_blocking(move || -> Result<_> {
        let st = s.lock().unwrap_or_else(|p| p.into_inner());
        let w = whereabouts(&st, &sid)?;
        let node = match &w {
            Whereabouts::Moved { node } | Whereabouts::Replica { node } => node.clone(),
            _ => String::new(),
        };
        Ok((w, node_name(&st, &node)))
    })
    .await??;
    let node = match where_ {
        Whereabouts::Here | Whereabouts::Unknown => return Ok(Brought::Here),
        Whereabouts::Moved { node } | Whereabouts::Replica { node } => node,
    };
    let elsewhere = |why: String| Brought::Elsewhere {
        node: node.clone(),
        name: name_of.clone(),
        why,
    };
    if !cfg.cluster.failover {
        return Ok(elsewhere("failover is off on this node".into()));
    }
    let (s, sid) = (store.clone(), session.to_string());
    let plan = match crate::rt::spawn_blocking(move || {
        plan(&s.lock().unwrap_or_else(|p| p.into_inner()), &sid)
    })
    .await?
    {
        Ok(p) => p,
        Err(e) => return Ok(elsewhere(format!("{e:#}"))),
    };
    let arb = match crate::arbiter::StaticArbiter::new(store.clone(), cfg.clone()) {
        Ok(a) => a,
        Err(e) => return Ok(elsewhere(format!("no arbiter to ask: {e:#}"))),
    };
    let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?;
    match arb
        .try_acquire_lease(&takeover_key(&plan.owner, &plan.root), &me.id)
        .await
    {
        LeaseState::Held(_) => {
            let by = format!("arbiter:{}", arb.arbiter_id());
            let s = store.clone();
            let r = crate::rt::spawn_blocking(move || {
                take_over(
                    &s.lock().unwrap_or_else(|p| p.into_inner()),
                    &me.id,
                    &me.name,
                    &plan,
                    &by,
                )
                .map(|r| (r, plan))
            })
            .await??;
            let (r, plan) = r;
            Ok(Brought::TookOver(format!(
                "{} stopped answering; this node took session {} over from its replica \
                 ({} message(s); {} checkpoint blob(s) stayed there)",
                plan.owner_name,
                plan.root,
                r.messages,
                r.missing_blobs.len()
            )))
        }
        LeaseState::HeldByOther { holder, .. } => {
            let name = node_name(&store.lock().unwrap_or_else(|p| p.into_inner()), &holder);
            Ok(Brought::Elsewhere {
                node: holder,
                name,
                why: "that node took it over".into(),
            })
        }
        LeaseState::Refused(why) => Ok(elsewhere(why)),
    }
}

// ── the opt-in self-fence ──────────────────────────────────────────

/// The node lease this gateway holds (`[cluster] failover`), and whether
/// `[cluster] failover_fence` is on.
static NODE_LEASE: std::sync::OnceLock<(std::sync::Arc<crate::arbiter::LeaseGuard>, bool)> =
    std::sync::OnceLock::new();

pub fn set_node_lease(guard: std::sync::Arc<crate::arbiter::LeaseGuard>, fence: bool) {
    let _ = NODE_LEASE.set((guard, fence));
}

pub fn node_lease() -> Option<std::sync::Arc<crate::arbiter::LeaseGuard>> {
    NODE_LEASE.get().map(|(g, _)| g.clone())
}

/// Why no turn may run on this node right now: the fence is on and the
/// node lease is lost, so a survivor may be taking these sessions over.
/// None = run. Without the fence a partition leaves two writers for a
/// while, and the owner's side becomes a fork when they meet — kept, not
/// lost; with it, one side goes quiet instead (a laptop offline stops).
pub fn fence_verdict(guard: Option<&crate::arbiter::LeaseGuard>, fence: bool) -> Option<String> {
    let g = guard?;
    if !fence || g.held() {
        return None;
    }
    Some(format!(
        "this node has lost its node lease ({}) — with [cluster] failover_fence on it runs no \
         turns until the lease is back, because another node may be taking its sessions over",
        g.lost_reason().unwrap_or_else(|| "not granted".into())
    ))
}

/// [`fence_verdict`] for this process.
pub fn fenced() -> Option<String> {
    NODE_LEASE
        .get()
        .and_then(|(g, fence)| fence_verdict(Some(g), *fence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbiter::{grant_local, LeaseState};

    /// Two nodes' stores in one scratch home: `a` owns the sessions,
    /// `b` survives. Both logs run; `ship` folds one's log into the
    /// other's replica of it (the shared `replicas/` is where each looks).
    struct Pool {
        _dir: Option<tempfile::TempDir>,
        _g: Option<crate::config::DirGuard>,
        a: Store,
        b: Store,
    }

    /// For sync tests: a thread-local directory override. A test that
    /// goes through `spawn_blocking` needs [`pool_at`] under
    /// `testenv::scratch_home` instead — the override does not follow a
    /// blocking-pool thread, which would read the LIVE home.
    fn pool() -> Pool {
        let dir = tempfile::tempdir().unwrap();
        let g = crate::config::set_dirs(Some(crate::config::DirOverride::forced(
            dir.path().to_path_buf(),
        )));
        let mut p = pool_at(dir.path());
        p._dir = Some(dir);
        p._g = Some(g);
        p
    }

    fn pool_at(root: &std::path::Path) -> Pool {
        let a = Store::open(&root.join("a.db")).unwrap();
        let b = Store::open(&root.join("b.db")).unwrap();
        crate::storesync::install_triggers(&a).unwrap();
        crate::storesync::install_triggers(&b).unwrap();
        // A checkpoint whose blob path names a file that exists HERE: a
        // replica's path is the owner's, so a takeover must never read it.
        let blob = root.join("owner-blob");
        std::fs::write(&blob, "not the owner's bytes").unwrap();
        a.conn()
            .execute(
                "INSERT INTO checkpoints(session_id, path, blob, bytes, tool)
                 VALUES('s1', '/x/file.txt', ?1, 21, 'write_file')",
                [blob.display().to_string()],
            )
            .unwrap();
        a.conn()
            .execute_batch(
                "INSERT INTO sessions(id, title) VALUES('s1', 'the talk');
                 INSERT INTO sessions(id, title, parent) VALUES('s1c', 'a child', 's1');
                 INSERT INTO messages(session_id, role, content) VALUES('s1', 'user', 'hello');
                 INSERT INTO messages(session_id, role, content) VALUES('s1', 'assistant', 'hi');
                 INSERT INTO messages(session_id, role, content) VALUES('s1c', 'user', 'sub');",
            )
            .unwrap();
        a.session_prompt_put(
            "s1",
            &crate::store::SessionPrompt {
                system_prompt: "a's frozen prompt".into(),
                tools_json: "[\"t\"]".into(),
                format: 1,
                node: Some("node-a".into()),
            },
        )
        .unwrap();
        Pool {
            _dir: None,
            _g: None,
            a,
            b,
        }
    }

    fn ship(from: &Store, node: &str) {
        let conn = crate::storesync::open_replica(node).unwrap();
        let since = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'veles.applied_seq'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let offer = crate::storesync::build_offer(from, node, node, since).unwrap();
        crate::storesync::apply_offer(&conn, &offer).unwrap();
    }

    fn count(s: &Store, sql: &str) -> i64 {
        s.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn stamp(s: &Store, id: &str) -> Option<String> {
        s.session_node_stamp(id).unwrap()
    }

    /// The survivor continues the session from its replica: the whole
    /// subtree, the owner's frozen prompt (the provider's cache is keyed
    /// on it), a note to the model, and the record the owner will find.
    #[test]
    fn a_takeover_continues_from_the_replica_with_the_owners_prompt() {
        let p = pool();
        ship(&p.a, "node-a");
        let pl = plan(&p.b, "s1").unwrap();
        assert_eq!(pl.owner, "node-a");
        assert_eq!(pl.subtree.len(), 2);
        let last: i64 = count(&p.a, "SELECT MAX(msg_id) FROM messages");
        assert_eq!(pl.last_source_msg_id, last);

        let r = take_over(&p.b, "node-b", "b", &pl, "arbiter:arb").unwrap();
        assert_eq!(r.sessions, 2);
        assert_eq!(
            count(
                &p.b,
                "SELECT COUNT(*) FROM messages WHERE session_id = 's1' AND injection IS NULL"
            ),
            2
        );
        let note: String = p
            .b
            .conn()
            .query_row(
                "SELECT content FROM messages WHERE session_id = 's1' AND injection = 'migration'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(note.starts_with("[taken over]"), "{note}");
        let prompt = p.b.session_prompt_get("s1").unwrap().unwrap();
        assert_eq!(prompt.system_prompt, "a's frozen prompt");
        assert_eq!(
            prompt.node.as_deref(),
            Some("node-a"),
            "still the owner's bytes"
        );
        let by: String =
            p.b.conn()
                .query_row(
                    "SELECT authorized_by FROM session_takeovers WHERE session_id = 's1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
        assert_eq!(by, "arbiter:arb");
        assert!(
            plan(&p.b, "s1").is_err(),
            "live here now: no second takeover"
        );
    }

    /// Back again, the owner finds the record, and what it wrote while
    /// the survivor held the session continues as a fork — whole history
    /// plus that tail — while its own copy is tombstoned to the survivor.
    #[test]
    fn the_owner_yields_and_keeps_what_it_wrote_as_a_fork() {
        let p = pool();
        ship(&p.a, "node-a");
        let pl = plan(&p.b, "s1").unwrap();
        take_over(&p.b, "node-b", "b", &pl, "arbiter:arb").unwrap();
        p.a.conn()
            .execute(
                "INSERT INTO messages(session_id, role, content)
                 VALUES('s1', 'user', 'written while apart')",
                [],
            )
            .unwrap();
        ship(&p.b, "node-b");
        let recs = unsettled(&p.a, "node-a").unwrap();
        assert_eq!(recs.len(), 1, "{recs:?}");
        assert_eq!(recs[0].to_node, "node-b");

        let out = settle(&p.a, "node-a", "a", &recs[0], &Verdict::Yield).unwrap();
        assert!(
            stamp(&p.a, "s1").is_some_and(|s| s.starts_with("migrated_to:node-b@")),
            "{:?}",
            stamp(&p.a, "s1")
        );
        assert!(
            stamp(&p.a, "s1c").is_some_and(|s| s.starts_with("migrated_to:node-b@")),
            "the subtree goes with it"
        );
        let fork: String =
            p.a.conn()
                .query_row(
                    "SELECT id FROM sessions WHERE title = 'the talk (fork)'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
        assert!(out.contains(&fork), "{out}");
        let tail: i64 =
            p.a.conn()
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND injection IS NULL",
                    [&fork],
                    |r| r.get(0),
                )
                .unwrap();
        assert_eq!(tail, 3, "the history and what was written while apart");
        assert!(
            unsettled(&p.a, "node-a").unwrap().is_empty(),
            "settled once"
        );
    }

    /// Nothing written after the survivor's copy: no fork, just the
    /// hand-over.
    #[test]
    fn an_owner_with_nothing_new_just_hands_over() {
        let p = pool();
        ship(&p.a, "node-a");
        let pl = plan(&p.b, "s1").unwrap();
        take_over(&p.b, "node-b", "b", &pl, "arbiter:arb").unwrap();
        ship(&p.b, "node-b");
        let rec = unsettled(&p.a, "node-a").unwrap().remove(0);
        settle(&p.a, "node-a", "a", &rec, &Verdict::Yield).unwrap();
        assert!(stamp(&p.a, "s1").is_some_and(|s| s.starts_with("migrated_to:node-b@")));
        assert_eq!(
            count(
                &p.a,
                "SELECT COUNT(*) FROM sessions WHERE title LIKE '%(fork)'"
            ),
            0
        );
    }

    /// A claim that does not hold leaves the session where it is, and is
    /// settled (said once, not every tick); one that cannot be checked yet
    /// is asked about again.
    #[test]
    fn a_claim_that_does_not_hold_keeps_the_session() {
        let p = pool();
        ship(&p.a, "node-a");
        let pl = plan(&p.b, "s1").unwrap();
        take_over(&p.b, "node-b", "b", &pl, "arbiter:arb").unwrap();
        ship(&p.b, "node-b");
        let rec = unsettled(&p.a, "node-a").unwrap().remove(0);
        settle(
            &p.a,
            "node-a",
            "a",
            &rec,
            &Verdict::Unknown("unreachable".into()),
        )
        .unwrap();
        assert_eq!(
            unsettled(&p.a, "node-a").unwrap().len(),
            1,
            "asked again later"
        );
        let out = settle(
            &p.a,
            "node-a",
            "a",
            &rec,
            &Verdict::Refuse("no grant".into()),
        )
        .unwrap();
        assert!(out.contains("stays here"), "{out}");
        assert_eq!(stamp(&p.a, "s1"), None, "still this node's");
        assert!(unsettled(&p.a, "node-a").unwrap().is_empty());
    }

    /// A fork of a replica's session needs nobody: a new id here, the
    /// owner's session untouched (and still takeable).
    #[test]
    fn a_fork_from_the_replica_takes_a_new_id_and_leaves_the_original() {
        let p = pool();
        ship(&p.a, "node-a");
        let (fork, r) = fork_from_replica(&p.b, "node-b", "b", "s1").unwrap();
        assert_ne!(fork, "s1");
        assert_eq!(r.sessions, 2);
        assert_eq!(
            r.missing_blobs.len(),
            1,
            "the owner's checkpoint blob stayed there"
        );
        let blob: Option<String> =
            p.b.conn()
                .query_row(
                    "SELECT blob FROM checkpoints WHERE session_id = ?1",
                    [&fork],
                    |r| r.get(0),
                )
                .unwrap();
        assert_eq!(
            blob, None,
            "a same-named local file is not the owner's blob"
        );
        assert!(
            !p.b.session_exists("s1").unwrap(),
            "the original is not here"
        );
        let prompt = p.b.session_prompt_get(&fork).unwrap().unwrap();
        assert_eq!(
            prompt.system_prompt, "a's frozen prompt",
            "the cache's prefix too"
        );
        let child_parent: String =
            p.b.conn()
                .query_row(
                    "SELECT parent FROM sessions WHERE title = 'a child (fork)'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
        assert_eq!(child_parent, fork, "the subtree follows the new root");
        assert!(
            plan(&p.b, "s1").is_ok(),
            "the original can still be taken over"
        );
    }

    /// The arbiter grants a takeover only once the owner's node lease has
    /// run out — and never for a node that never held one.
    #[test]
    fn a_takeover_grant_waits_for_the_owners_node_lease() {
        let p = pool();
        let now = chrono::Utc::now();
        let key = takeover_key("node-a", "s1");
        let refused = |st: LeaseState, what: &str| match st {
            LeaseState::Refused(r) => assert!(r.contains(what), "{r}"),
            other => panic!("granted: {other:?}"),
        };
        refused(
            grant_local(&p.a, &key, "node-b", now),
            "never held a node lease",
        );
        assert!(matches!(
            grant_local(&p.a, &node_key("node-a"), "node-a", now),
            LeaseState::Held(_)
        ));
        refused(grant_local(&p.a, &key, "node-b", now), "is alive");
        refused(
            grant_local(&p.a, &node_key("node-a"), "node-b", now),
            "may not hold",
        );
        let later = now + chrono::Duration::seconds(crate::arbiter::LEASE_TTL_SECS + 1);
        refused(grant_local(&p.a, &key, "node-a", later), "its own session");
        match grant_local(&p.a, &key, "node-b", later) {
            LeaseState::Held(until) => {
                let until = chrono::DateTime::parse_from_rfc3339(&until).unwrap();
                assert!(
                    until > later + chrono::Duration::days(29),
                    "kept for the owner"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            grant_local(&p.a, &key, "node-c", later),
            LeaseState::HeldByOther { holder, .. } if holder == "node-b"
        ));
    }

    /// A survivor that is the arbiter, with the owner's replica shipped:
    /// the owner (`node-a`) is registered, this node (`node-b`) is the
    /// arbiter and runs failover.
    #[allow(clippy::type_complexity)]
    fn arbiter_pool() -> (
        tempfile::TempDir,
        crate::testenv::EnvGuard,
        Pool,
        std::sync::Arc<std::sync::Mutex<Store>>,
        std::sync::Arc<crate::config::VelesConfig>,
    ) {
        let (home, env) = crate::testenv::scratch_home();
        let p = pool_at(home.path());
        ship(&p.a, "node-a");
        std::fs::write(
            crate::config::config_dir().join("node.toml"),
            "id = \"node-b\"\nname = \"bee\"\n",
        )
        .unwrap();
        let b = Store::open(&home.path().join("b.db")).unwrap();
        for (id, name, ep) in [
            ("node-a", "ay", Some("http://127.0.0.1:0")),
            ("node-b", "bee", None),
        ] {
            b.node_register(id, name, "h", "0", "always-on", None, ep, None, None)
                .unwrap();
        }
        let mut cfg = crate::config::VelesConfig::default();
        cfg.cluster.arbiter = Some("node-b".into());
        cfg.cluster.failover = true;
        (
            home,
            env,
            p,
            std::sync::Arc::new(std::sync::Mutex::new(b)),
            std::sync::Arc::new(cfg),
        )
    }

    /// Where a session lives: here, moved (a tombstone names the node),
    /// in a replica only, or nowhere known.
    #[test]
    fn whereabouts_follow_the_tombstone_and_the_replicas() {
        let p = pool();
        ship(&p.a, "node-a");
        assert_eq!(whereabouts(&p.a, "s1").unwrap(), Whereabouts::Here);
        assert_eq!(
            whereabouts(&p.b, "s1").unwrap(),
            Whereabouts::Replica {
                node: "node-a".into()
            }
        );
        assert_eq!(whereabouts(&p.b, "nope").unwrap(), Whereabouts::Unknown);
        let sub = migration::session_subtree(&p.a, "s1").unwrap();
        migration::retire(&p.a, &sub, "node-b").unwrap();
        assert_eq!(
            whereabouts(&p.a, "s1").unwrap(),
            Whereabouts::Moved {
                node: "node-b".into()
            }
        );
    }

    /// Lazy failover: a session whose owner stopped answering is taken
    /// over the moment someone talks to it here — and not before its
    /// owner's node lease has run out, and never with failover off.
    #[tokio::test]
    async fn talking_to_a_dead_nodes_session_takes_it_over() {
        let (_home, _env, _p, b, cfg) = arbiter_pool();
        let r = bring_here(b.clone(), cfg.clone(), "s1").await.unwrap();
        assert!(
            matches!(&r, Brought::Elsewhere { why, .. } if why.contains("never held a node lease")),
            "{r:?}"
        );
        let past = chrono::Utc::now() - chrono::Duration::seconds(300);
        b.lock()
            .unwrap()
            .lease_grant(
                &node_key("node-a"),
                "node-a",
                past,
                crate::arbiter::LEASE_TTL_SECS,
            )
            .unwrap();
        let mut off = (*cfg).clone();
        off.cluster.failover = false;
        let r = bring_here(b.clone(), std::sync::Arc::new(off), "s1")
            .await
            .unwrap();
        assert!(
            matches!(r, Brought::Elsewhere { .. }),
            "failover off: {r:?}"
        );
        let r = bring_here(b.clone(), cfg.clone(), "s1").await.unwrap();
        assert!(matches!(&r, Brought::TookOver(_)), "{r:?}");
        assert!(b.lock().unwrap().session_exists("s1").unwrap());
        assert_eq!(
            bring_here(b.clone(), cfg, "s1").await.unwrap(),
            Brought::Here,
            "ours now"
        );
    }

    /// An owner that is alive keeps its session: the answer names it.
    #[tokio::test]
    async fn a_live_owners_session_is_only_pointed_at() {
        let (_home, _env, _p, b, cfg) = arbiter_pool();
        b.lock()
            .unwrap()
            .lease_grant(
                &node_key("node-a"),
                "node-a",
                chrono::Utc::now(),
                crate::arbiter::LEASE_TTL_SECS,
            )
            .unwrap();
        match bring_here(b.clone(), cfg, "s1").await.unwrap() {
            Brought::Elsewhere { node, name, why } => {
                assert_eq!((node.as_str(), name.as_str()), ("node-a", "ay"));
                assert!(why.contains("is alive"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!b.lock().unwrap().session_exists("s1").unwrap());
    }

    /// The opt-in fence stops turns only while the node lease is lost.
    #[test]
    fn the_fence_holds_turns_only_while_the_lease_is_lost() {
        let lost = crate::arbiter::LeaseGuard::standby("node:me", "me");
        assert!(fence_verdict(Some(&lost), true).is_some(), "lost, fenced");
        assert!(
            fence_verdict(Some(&lost), false).is_none(),
            "the fence is opt-in"
        );
        assert!(fence_verdict(None, true).is_none(), "no failover, no fence");
        let held = crate::arbiter::LeaseGuard::standby("node:me", "me");
        held.hold_for_test();
        assert!(
            fence_verdict(Some(&held), true).is_none(),
            "held: turns run"
        );
    }

    /// With two devices and the arbiter gone, the survivor's own key signs
    /// its takeover; the owner accepts it on its return, a ticket is good
    /// once, and a key the owner does not trust authorizes nothing.
    #[test]
    fn a_takeover_ticket_may_be_signed_on_the_survivor() {
        let p = pool();
        ship(&p.a, "node-a");
        let pl = plan(&p.b, "s1").unwrap();
        let (key, _) = crate::ticket::SignerKey::generate().unwrap();
        let trust =
            || vec![crate::ticket::TrustedSigner::from_config("k", &key.public_config()).unwrap()];
        let req =
            crate::ticket::TicketRequest::decode(&ticket_request("node-b", &pl).unwrap()).unwrap();
        let ticket = key.sign(&req).unwrap();

        let by = spend_ticket(&p.b, &trust(), "node-b", &pl, &ticket).unwrap();
        assert!(
            spend_ticket(&p.b, &trust(), "node-b", &pl, &ticket).is_err(),
            "one use"
        );

        let rec = TakeoverRecord {
            session_id: "s1".into(),
            from_node: "node-a".into(),
            to_node: "node-b".into(),
            last_source_msg_id: pl.last_source_msg_id,
            authorized_by: by,
            at: "t".into(),
        };
        assert_eq!(check_ticket(&rec, &trust()), Verdict::Yield);
        assert!(
            matches!(check_ticket(&rec, &[]), Verdict::Refuse(_)),
            "a key the owner does not trust"
        );
        let other = TakeoverRecord {
            session_id: "s1c".into(),
            ..rec
        };
        assert!(
            matches!(check_ticket(&other, &trust()), Verdict::Refuse(_)),
            "a ticket names one session"
        );
    }
}
