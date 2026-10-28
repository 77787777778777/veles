//! Session mail — one veles session sends text into another, and it runs
//! there as that session's next turn (PLAN-sessions-and-phone.md, Part S).
//!
//! The shape (USER decisions D1, D2, 2026-10-11):
//!
//! - **The mailbox is the queue that already exists.** A mail is a
//!   `kind='mail'` row in the target's `session_queue`, so every surface
//!   that drains its queue at a turn boundary runs it with no new code;
//!   `session_mail` holds what the queue cannot (sender, rights, loop
//!   guard, reply). Both rows are written in one transaction.
//! - **The body names its mail.** The queued text is `[veles mail <id>]`,
//!   a newline, then the text. [`Agent::run_turn`](crate::agent::Agent::run_turn)
//!   is the one choke point every turn passes; it [`claim`]s the body
//!   there, so the rules below cannot be skipped by a surface that
//!   forgot them — including the gateway, which deletes the queue row
//!   and re-injects the text before the turn starts. A body is a mail
//!   only when the row is addressed to THIS session and its hash is the
//!   exact text; anything else is ordinary text.
//! - **A mail turn runs with the lower of the sender's and the target's
//!   rights** ([`turn_principal`]). A turn caused by someone has at most
//!   their rights, whoever's session it runs in.
//! - **Delivery is a nudge, not a second writer.** The process that holds
//!   the target session open ([`crate::presence`], here or a sibling over
//!   [`crate::runsock`]) runs it when idle; a busy one reaches it at its
//!   boundary. A session nobody holds keeps its mail queued until it is
//!   next opened — its principal is unknowable while nobody holds it, and
//!   running it as the owner (cron's rule) would be a guess.
//! - **Loops are bounded.** Mail sent from inside a mail turn continues
//!   that turn's chain one hop further; [`MAX_HOPS`] ends it. A waited
//!   send to a session that is itself blocked waiting on this chain is
//!   refused, because it would deadlock until both time out.
//!
//! One other kind rides the same rows: a line the OWNER typed into a
//! session they joined ([`send_join`], `crate::join`). It queues and is
//! claimed exactly like mail, so it waits behind a running turn and
//! cannot be forged by the session's own person, but it runs with the
//! owner's rights rather than the lower of the two (USER, 2026-10-13:
//! "with my rights").

use crate::acl::{Cap, Principal, Role};
use crate::lockutil::LockOk;
use crate::store::{MailRow, NewMail, Store};
use anyhow::{anyhow, bail, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many hops one chain may take: A mails B (hop 0), B's turn mails C
/// (hop 1), … A chain that would go further is refused at the send.
pub const MAX_HOPS: i64 = 6;

/// The longest text one mail carries.
pub const MAX_TEXT: usize = 32 * 1024;

/// Per sending session: at most this many sends per [`RATE_WINDOW`].
const RATE_MAX: usize = 30;
const RATE_WINDOW: Duration = Duration::from_secs(60);

const HEADER_OPEN: &str = "[veles mail ";

/// A `session_mail` row's kind: mail one session sent another.
pub const KIND_MAIL: &str = "mail";
/// A line the owner typed into a session they joined ([`send_join`]).
pub const KIND_JOIN: &str = "join";

/// A fresh mail id: `m` + 16 hex digits.
pub fn new_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("m{}", &u[..16])
}

fn valid_id(id: &str) -> bool {
    id.len() == 17
        && id.starts_with('m')
        && id[1..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// The text the target's queue holds for a mail.
pub fn render_body(id: &str, text: &str) -> String {
    format!("{HEADER_OPEN}{id}]\n{text}")
}

/// Split a queued body into (mail id, text). `None` for anything that is
/// not exactly a rendered mail body.
pub fn parse_body(body: &str) -> Option<(&str, &str)> {
    let rest = body.strip_prefix(HEADER_OPEN)?;
    let (id, rest) = rest.split_once(']')?;
    if !valid_id(id) {
        return None;
    }
    Some((id, rest.strip_prefix('\n')?))
}

/// The hash `session_mail.body_sha` binds a row to its queued body with.
pub fn body_sha(body: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(body.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Who is sending.
#[derive(Debug, Clone, Default)]
pub struct Sender {
    /// The sending session; `None` outside any session (the CLI).
    pub session: Option<String>,
    /// Its title, for the target's provenance line.
    pub title: Option<String>,
    /// Its rights; `None` = the owner surface.
    pub principal: Option<Principal>,
}

/// Where a sent mail went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// A surface in this process holds the target and took the nudge.
    Here,
    /// A sibling process (this pid) holds it and took the nudge.
    Process(u32),
    /// Nobody holds it open: it waits in its queue for the next opener.
    Queued,
}

/// A sent mail.
#[derive(Debug, Clone)]
pub struct Sent {
    pub id: String,
    pub to: String,
    pub delivery: Delivery,
    pub chain_id: String,
    pub hop: i64,
}

/// The mail a session's current turn is running — what continues a
/// chain when that turn sends mail of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    pub mail_id: String,
    pub chain_id: String,
    pub hop: i64,
}

fn current_map() -> &'static Mutex<HashMap<String, Current>> {
    static M: std::sync::OnceLock<Mutex<HashMap<String, Current>>> = std::sync::OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Mark `session_id`'s turn as running `row` (the agent, at turn start).
pub fn enter(session_id: &str, row: &MailRow) {
    current_map().lock_ok().insert(
        session_id.to_string(),
        Current {
            mail_id: row.id.clone(),
            chain_id: row.chain_id.clone(),
            hop: row.hop,
        },
    );
}

/// The turn ended.
pub fn leave(session_id: &str) {
    current_map().lock_ok().remove(session_id);
}

/// The mail `session_id`'s turn is running, if it is a mail turn.
pub fn current(session_id: &str) -> Option<Current> {
    current_map().lock_ok().get(session_id).cloned()
}

fn rate_ok(key: &str) -> bool {
    static R: std::sync::OnceLock<Mutex<HashMap<String, VecDeque<Instant>>>> =
        std::sync::OnceLock::new();
    let mut m = R.get_or_init(|| Mutex::new(HashMap::new())).lock_ok();
    let q = m.entry(key.to_string()).or_default();
    let now = Instant::now();
    while q
        .front()
        .is_some_and(|t| now.duration_since(*t) > RATE_WINDOW)
    {
        q.pop_front();
    }
    if q.len() >= RATE_MAX {
        return false;
    }
    q.push_back(now);
    true
}

fn local_node_id() -> Option<String> {
    crate::node::NodeIdentity::load(&crate::config::config_dir())
        .ok()
        .flatten()
        .map(|i| i.id)
}

/// Send `text` from `from` into session `to` (an exact id — resolve
/// references first with [`resolve`]) and nudge whoever holds it.
pub fn send(store: &Arc<Mutex<Store>>, from: &Sender, to: &str, text: &str) -> Result<Sent> {
    send_with(store, from, to, text, deliver)
}

/// A line the owner typed while joined to session `to` (`crate::join`):
/// queued like mail and claimed at the same point, but run with the
/// OWNER's rights ([`MailTurn::join`]), with provenance that names the
/// owner as a second person in the conversation, not as a program.
///
/// Only the owner may send one, and the gateway's `/join` is the only
/// caller: the sessions tool sends [`KIND_MAIL`], and so does every send
/// relayed from another node, so neither a model nor a peer can raise a
/// turn this way.
pub fn send_join(
    store: &Arc<Mutex<Store>>,
    owner: &Principal,
    owner_name: &str,
    to: &str,
    text: &str,
) -> Result<Sent> {
    send_join_with(store, owner, owner_name, to, text, deliver)
}

fn send_join_with(
    store: &Arc<Mutex<Store>>,
    owner: &Principal,
    owner_name: &str,
    to: &str,
    text: &str,
    deliver: impl Fn(&str) -> Delivery,
) -> Result<Sent> {
    if !is_owner(owner) {
        bail!("only the owner can write into a session they joined");
    }
    let from = Sender {
        session: None,
        title: Some(owner_name.to_string()),
        principal: Some(owner.clone()),
    };
    send_inner(store, &from, None, KIND_JOIN, to, text, deliver)
}

/// Mail that arrived from another pool node (`crate::mailnet`): the peer
/// it came through, and the exchange it continues there.
#[derive(Debug, Clone)]
pub struct Relay {
    /// The A2A peer it came from — the name THIS node knows it by, which
    /// is also how a reply addresses it (`<peer>/<session>`).
    pub peer: String,
    /// The exchange it continues; `None` starts one here.
    pub chain_id: Option<String>,
    pub hop: i64,
}

/// Enqueue mail a pool peer sent (`crate::mailnet::receive`) and nudge
/// whoever holds the target. `from.principal` must already be capped at
/// what the peer holds here.
pub fn send_relayed(
    store: &Arc<Mutex<Store>>,
    from: &Sender,
    relay: &Relay,
    to: &str,
    text: &str,
) -> Result<Sent> {
    send_inner(store, from, Some(relay), KIND_MAIL, to, text, deliver)
}

/// [`send`] with the delivery step supplied — tests deliver nowhere, so
/// they never knock on a real data dir's sockets.
fn send_with(
    store: &Arc<Mutex<Store>>,
    from: &Sender,
    to: &str,
    text: &str,
    deliver: impl Fn(&str) -> Delivery,
) -> Result<Sent> {
    send_inner(store, from, None, KIND_MAIL, to, text, deliver)
}

fn send_inner(
    store: &Arc<Mutex<Store>>,
    from: &Sender,
    relay: Option<&Relay>,
    kind: &str,
    to: &str,
    text: &str,
    deliver: impl Fn(&str) -> Delivery,
) -> Result<Sent> {
    let text = text.trim();
    if text.is_empty() {
        bail!("nothing to send: the text is empty");
    }
    if text.len() > MAX_TEXT {
        bail!(
            "the text is {} bytes; one mail carries at most {MAX_TEXT}",
            text.len()
        );
    }
    if relay.is_none() && from.session.as_deref() == Some(to) {
        bail!("a session cannot mail itself");
    }
    if !store.lock_ok().session_exists(to)? {
        bail!("no session {to} in this store");
    }
    let id = new_id();
    let (chain_id, hop) = match relay {
        Some(r) => (r.chain_id.clone().unwrap_or_else(|| id.clone()), r.hop),
        None => match from.session.as_deref().and_then(current) {
            Some(c) => (c.chain_id, c.hop + 1),
            None => (id.clone(), 0),
        },
    };
    // A relayed hop is the peer's word: below zero or past the limit it
    // is refused outright, never carried on (a huge value plus one would
    // wrap in a release build and read as a fresh chain).
    if !(0..=MAX_HOPS).contains(&hop) {
        bail!(
            "this mail would be hop {hop} of one chain of messages between sessions, and a \
             chain stops at {MAX_HOPS}. Answer in your own turn instead of sending more mail."
        );
    }
    let rate_key = match relay {
        Some(r) => format!("peer:{}", r.peer),
        None if kind == KIND_JOIN => format!(
            "join:{}",
            from.principal
                .as_ref()
                .map(|p| p.subject.as_str())
                .unwrap_or("owner")
        ),
        None => from.session.clone().unwrap_or_else(|| "cli".to_string()),
    };
    if !rate_ok(&rate_key) {
        bail!("too much mail: at most {RATE_MAX} sends a minute from one session");
    }
    let principal_json = match &from.principal {
        Some(p) => Some(serde_json::to_string(p)?),
        None => None,
    };
    let body = render_body(&id, text);
    let sha = body_sha(&body);
    let node = match relay {
        Some(r) => Some(r.peer.clone()),
        None => local_node_id(),
    };
    store.lock_ok().mail_enqueue(&NewMail {
        id: &id,
        to_session: to,
        from_session: from.session.as_deref(),
        from_title: from.title.as_deref(),
        from_node: node.as_deref(),
        // Nothing in THIS machine waits on a relayed mail: the peer's
        // a2a call does, inline.
        from_pid: relay.is_none().then(|| std::process::id() as i64),
        from_principal: principal_json.as_deref(),
        via_peer: relay.map(|r| r.peer.as_str()),
        chain_id: &chain_id,
        hop,
        body: &body,
        body_sha: &sha,
        kind,
    })?;
    let delivery = deliver(to);
    Ok(Sent {
        id,
        to: to.to_string(),
        delivery,
        chain_id,
        hop,
    })
}

/// Nudge whoever holds `session_id`: a surface in this process first,
/// then each sibling process.
pub fn deliver(session_id: &str) -> Delivery {
    if crate::presence::nudge(session_id) {
        return Delivery::Here;
    }
    let req = serde_json::json!({"op": "nudge", "session": session_id});
    for peer in crate::runsock::peers() {
        match crate::runsock::request(&peer, &req) {
            Ok(v) if v.get("held").and_then(|h| h.as_bool()) == Some(true) => {
                return Delivery::Process(peer.pid);
            }
            Ok(_) => {}
            Err(e) => tracing::debug!("mail: nudge {} failed: {e:#}", peer.pid),
        }
    }
    Delivery::Queued
}

/// What [`claim`] found in a turn's text.
#[derive(Debug)]
pub enum Claim {
    /// Ordinary text.
    Plain,
    /// A mail, claimed for this turn.
    Mail(Box<MailTurn>),
    /// The exact body of a mail that was already answered (a queue row
    /// re-run after a crash between the answer and the row's release).
    /// The turn must not run again: re-running it as plain text would
    /// drop the rights clamp.
    Spent(String),
}

/// A claimed mail, as the turn that runs it needs it.
#[derive(Debug, Clone)]
pub struct MailTurn {
    pub row: MailRow,
    /// The text without its header — what the model reads.
    pub text: String,
    /// The sender's rights; `None` = the owner surface.
    pub sender: Option<Principal>,
    /// The owner's own line in a session they joined ([`KIND_JOIN`]):
    /// the turn runs with `sender`'s rights, not the lower of the two.
    /// True only for a join row sent HERE (no peer) by an owner
    /// principal; a join row that is anything else runs as plain mail.
    pub join: bool,
}

/// Decide whether `user_text` is a mail for `session_id`, and claim it
/// when it is.
pub fn claim(store: &Store, session_id: &str, user_text: &str) -> Claim {
    let Some((id, text)) = parse_body(user_text) else {
        return Claim::Plain;
    };
    let sha = body_sha(user_text);
    match store.mail_claim(id, session_id, &sha) {
        Ok(Some(row)) => {
            // A row whose recorded principal does not parse is read as
            // a GUEST, never as the owner: the fail-open the delegate
            // principal fix closed (acl.rs, PRINCIPAL_ENV) must not
            // reopen here.
            let sender = row.from_principal.as_deref().map(|j| {
                serde_json::from_str::<Principal>(j).unwrap_or_else(|_| Principal::guest())
            });
            let join = row.kind == KIND_JOIN
                && row.via_peer.is_none()
                && sender.as_ref().is_some_and(is_owner);
            if row.kind == KIND_JOIN && !join {
                tracing::warn!(
                    "mail {}: a join line that is not the owner's own runs as ordinary mail",
                    row.id
                );
            }
            Claim::Mail(Box::new(MailTurn {
                text: text.to_string(),
                sender,
                join,
                row,
            }))
        }
        Ok(None) => match store.mail_get(id) {
            Ok(Some(row)) if row.to_session == session_id && row.body_sha == sha => {
                Claim::Spent(row.id)
            }
            _ => Claim::Plain,
        },
        Err(e) => {
            tracing::warn!("mail: claim {id} failed ({e:#}); the turn runs as plain text");
            // The store could not answer. Running the text as a plain
            // turn would drop the clamp, so it does not run: the row
            // stays queued for the next drain.
            Claim::Spent(id.to_string())
        }
    }
}

/// The capability set two principals share.
fn meet(a: &Principal, b: &Principal) -> std::collections::BTreeSet<Cap> {
    if a.has(Cap::All) {
        return b.caps.clone();
    }
    if b.has(Cap::All) {
        return a.caps.clone();
    }
    a.caps.intersection(&b.caps).copied().collect()
}

fn is_owner(p: &Principal) -> bool {
    p.role == Role::Owner && p.has(Cap::All)
}

/// The principal a mail turn must run as, when that is BELOW the
/// target's own: the lower of sender and target (D2). `None` = run with
/// the target's own rights, unchanged. Both arguments use `None` for the
/// owner surface.
pub fn turn_principal(sender: Option<&Principal>, target: Option<&Principal>) -> Option<Principal> {
    let s = sender.filter(|s| !is_owner(s))?;
    let Some(t) = target.filter(|t| !is_owner(t)) else {
        // The target runs as the owner; the sender is below it.
        return Some(s.clone());
    };
    if s.role < t.role {
        return Some(s.clone());
    }
    if s.role > t.role {
        return None;
    }
    let caps = meet(s, t);
    if caps == t.caps {
        return None;
    }
    let mut p = t.clone();
    p.caps = caps;
    Some(p)
}

/// The provenance row a mail turn writes before its text — the line the
/// model reads (the `injection` column is renderer metadata only).
pub fn origin_line(row: &MailRow) -> String {
    let who = match (&row.from_session, &row.from_title) {
        (Some(sid), Some(t)) if !t.is_empty() => format!("session \u{201c}{t}\u{201d} ({sid})"),
        (Some(sid), _) => format!("session {sid}"),
        (None, _) => "the veles command line (the owner, outside any session)".to_string(),
    };
    // Only mail from another node names one: a local sender's node is
    // this one.
    let node = row
        .via_peer
        .as_deref()
        .map(|n| format!(" on node {n}"))
        .unwrap_or_default();
    let reply = match &row.from_session {
        Some(sid) => format!(
            " Your final answer this turn is its reply: the sender gets it if it is waiting, \
             and can check for it later otherwise. To send it anything more, use the sessions \
             tool with to={}.",
            reply_address(row, sid)
        ),
        None => " Your final answer this turn is its reply.".to_string(),
    };
    format!(
        "[origin] this message is mail from {who}{node}: another agent or program wrote it, \
         not the user.{reply}"
    )
}

/// The provenance row before the owner's own line in a session they
/// joined: a second PERSON in this conversation, named, with the rights
/// the turn runs with and who reads the answer. `user` is whoever the
/// session belongs to, as the conversation names them.
pub fn join_origin_line(row: &MailRow, user: Option<&str>) -> String {
    let owner = row
        .from_title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or("the owner");
    let user = user
        .filter(|u| !u.trim().is_empty())
        .unwrap_or("the person this conversation belongs to");
    format!(
        "[origin] {owner}, the owner of this agent, has joined this conversation and wrote \
         the next message; {user} did not write it. This turn runs with the owner's rights. \
         Your answer appears in this chat, where {user} reads it, and the owner sees it too."
    )
}

/// How a mail's sender is addressed from here: its session id, behind
/// `<peer>/` when it came from another node.
fn reply_address(row: &MailRow, sid: &str) -> String {
    match &row.via_peer {
        Some(peer) => format!("{peer}/{sid}"),
        None => sid.to_string(),
    }
}

/// The `injection` tag of a mail turn's text row.
pub fn injection_tag(row: &MailRow) -> String {
    match row.from_session.as_deref() {
        Some(sid) => format!("mail:{}", reply_address(row, sid)),
        None => "mail:cli".to_string(),
    }
}

/// Record how the turn a mail started ended, and wake whoever waits for
/// it — here, or in the sending process.
pub fn finish(
    store: &Arc<Mutex<Store>>,
    row: &MailRow,
    outcome: std::result::Result<&str, String>,
) {
    let (state, text) = match outcome {
        Ok(reply) => ("answered", reply.to_string()),
        Err(e) => ("failed", e),
    };
    if let Err(e) = store.lock_ok().mail_finish(&row.id, state, &text) {
        tracing::warn!("mail: recording {} as {state} failed: {e:#}", row.id);
    }
    notify_done(&row.id);
    if let Some(pid) = row.from_pid.and_then(|p| u32::try_from(p).ok()) {
        if let Some(peer) = crate::runsock::peer_for_pid(pid) {
            let id = row.id.clone();
            // Off the turn's thread: a sibling that is slow to answer
            // must not hold the turn's end (2 s budget either way).
            let _ = std::thread::Builder::new()
                .name("veles-mail-done".into())
                .spawn(move || {
                    let req = serde_json::json!({"op": "mail_done", "mail": id});
                    if let Err(e) = crate::runsock::request(&peer, &req) {
                        tracing::debug!("mail: mail_done to {pid} failed: {e:#}");
                    }
                });
        }
    }
}

fn waiters() -> &'static Mutex<HashMap<String, Arc<tokio::sync::Notify>>> {
    static W: std::sync::OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>> =
        std::sync::OnceLock::new();
    W.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A mail's turn ended (here, or a sibling said so over the socket).
pub fn notify_done(id: &str) {
    if let Some(n) = waiters().lock_ok().get(id) {
        // notify_one keeps a permit when nobody is parked yet, so a
        // turn that ends before the waiter parks is not missed.
        n.notify_one();
    }
}

/// How a wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waited {
    Answered(String),
    Failed(String),
    /// The time ran out; the mail is in this state.
    Pending(String),
}

/// Clears the waiting flag and the waiter however a wait ends — the
/// turn waiting may be cancelled, which drops its future mid-await, and
/// a flag left set would make the cycle check refuse a later send.
struct WaitGuard<'a> {
    store: &'a Arc<Mutex<Store>>,
    id: &'a str,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        let _ = self.store.lock_ok().mail_set_waiting(self.id, false);
        waiters().lock_ok().remove(self.id);
    }
}

/// Wait up to `timeout` for the turn a mail started to end. The store is
/// the truth; the notification only cuts the wait short.
pub async fn wait(store: &Arc<Mutex<Store>>, id: &str, timeout: Duration) -> Result<Waited> {
    let n = waiters()
        .lock_ok()
        .entry(id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
        .clone();
    store.lock_ok().mail_set_waiting(id, true)?;
    let _guard = WaitGuard { store, id };
    let deadline = Instant::now() + timeout;
    let out = loop {
        let row = store
            .lock_ok()
            .mail_get(id)?
            .ok_or_else(|| anyhow!("mail {id} is gone"))?;
        match row.state.as_str() {
            "answered" => break Waited::Answered(row.reply.unwrap_or_default()),
            "failed" => break Waited::Failed(row.reply.unwrap_or_default()),
            _ => {}
        }
        let now = Instant::now();
        if now >= deadline {
            break Waited::Pending(row.state);
        }
        let step = (deadline - now).min(Duration::from_secs(1));
        tokio::select! {
            _ = n.notified() => {}
            _ = tokio::time::sleep(step) => {}
        }
    };
    Ok(out)
}

/// A session some process on this machine holds open or is running a
/// turn in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    pub session_id: String,
    pub title: String,
    pub surface: String,
    /// The process holding it (or running its turn).
    pub pid: Option<u32>,
    /// A turn is running in it right now.
    pub running: bool,
    /// Some process holds it open, so mail is run when it arrives (a
    /// session only RUNNING — a one-shot `veles chat -q` — is not held:
    /// its process ends with the turn).
    pub held: bool,
}

/// Every live session sharing this data dir: the ones this process and
/// its siblings hold open, plus any running a turn (`session_runs`).
pub fn live(store: &Arc<Mutex<Store>>) -> Vec<LiveSession> {
    let me = std::process::id();
    let mut out: Vec<LiveSession> = Vec::new();
    let mut add = |sid: String, surface: String, pid: Option<u32>, held: bool| {
        if let Some(l) = out.iter_mut().find(|l| l.session_id == sid) {
            l.held |= held;
            return;
        }
        out.push(LiveSession {
            session_id: sid,
            title: String::new(),
            surface,
            pid,
            running: false,
            held,
        });
    };
    for s in crate::presence::list() {
        add(s.session_id, s.surface, Some(me), true);
    }
    let req = serde_json::json!({"op": "open_sessions"});
    for peer in crate::runsock::peers() {
        let Ok(v) = crate::runsock::request(&peer, &req) else {
            continue;
        };
        for s in v["sessions"].as_array().into_iter().flatten() {
            let (Some(sid), Some(surface)) = (s["session_id"].as_str(), s["surface"].as_str())
            else {
                continue;
            };
            add(sid.to_string(), surface.to_string(), Some(peer.pid), true);
        }
    }
    let runs = store.lock_ok().runs_live(30).unwrap_or_default();
    for r in &runs {
        add(
            r.session_id.clone(),
            r.surface.clone(),
            u32::try_from(r.pid).ok(),
            false,
        );
    }
    for l in out.iter_mut() {
        l.running = runs.iter().any(|r| r.session_id == l.session_id);
    }
    let ids: Vec<String> = out.iter().map(|l| l.session_id.clone()).collect();
    if let Ok(rows) = store.lock_ok().session_rows(&ids) {
        for r in rows {
            if let Some(l) = out.iter_mut().find(|l| l.session_id == r.id) {
                l.title = r.title;
            }
        }
    }
    out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    out
}

/// What a reference to a session resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    One(String),
    None,
    /// Several sessions match; their ids.
    Many(Vec<String>),
}

/// Resolve a reference to a session in this store: an exact id, a
/// unique id prefix, or an exact title. Among several title matches the
/// ones some process holds open win; still several is ambiguous.
pub fn resolve(store: &Store, reference: &str, open: &[String]) -> Result<Resolved> {
    let r = reference.trim();
    if r.is_empty() {
        return Ok(Resolved::None);
    }
    if store.session_exists(r)? {
        return Ok(Resolved::One(r.to_string()));
    }
    let by_prefix = store.sessions_with_prefix(r, 5)?;
    match by_prefix.len() {
        1 => return Ok(Resolved::One(by_prefix[0].clone())),
        0 => {}
        _ => return Ok(Resolved::Many(by_prefix)),
    }
    let titled = store.sessions_titled(r, 20)?;
    let live: Vec<String> = titled
        .iter()
        .filter(|s| open.contains(s))
        .cloned()
        .collect();
    let pick = if live.is_empty() { titled } else { live };
    Ok(match pick.len() {
        0 => Resolved::None,
        1 => Resolved::One(pick[0].clone()),
        _ => Resolved::Many(pick),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store in its own temp dir. The dir rides along so it lives as
    /// long as the test holds the pair.
    fn store() -> (Arc<Mutex<Store>>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let st = Store::open(&dir.path().join("state.db")).unwrap();
        (Arc::new(Mutex::new(st)), dir)
    }

    /// Send, delivering nowhere: a test must never knock on the sockets
    /// of whatever data dir the process happens to point at.
    fn send(st: &Arc<Mutex<Store>>, from: &Sender, to: &str, text: &str) -> Result<Sent> {
        send_with(st, from, to, text, |_| Delivery::Queued)
    }

    #[test]
    fn a_body_round_trips_and_near_misses_are_plain_text() {
        let id = new_id();
        assert!(valid_id(&id), "{id}");
        let body = render_body(&id, "hello\nthere");
        assert_eq!(parse_body(&body), Some((id.as_str(), "hello\nthere")));
        // Not a header: wrong prefix, bad id, missing newline.
        assert_eq!(parse_body("hello"), None);
        assert_eq!(parse_body("[veles mail mXYZ]\nhi"), None);
        assert_eq!(parse_body(&format!("[veles mail {id}] hi")), None);
        assert_eq!(parse_body(&format!(" [veles mail {id}]\nhi")), None);
    }

    #[test]
    fn a_sent_mail_is_queued_as_one_row_pair_and_claimed_once() {
        let (st, _dir) = store();
        {
            let s = st.lock_ok();
            s.create_session("mail-from").unwrap();
            s.create_session("mail-to").unwrap();
        }
        let from = Sender {
            session: Some("mail-from".into()),
            title: Some("worker".into()),
            principal: None,
        };
        let sent = send(&st, &from, "mail-to", "  do the thing  ").unwrap();
        assert_eq!(sent.delivery, Delivery::Queued, "nobody holds mail-to");
        assert_eq!((sent.chain_id.as_str(), sent.hop), (sent.id.as_str(), 0));
        let item = st.lock_ok().queue_take_next("mail-to").unwrap().unwrap();
        assert_eq!(item.kind.as_deref(), Some("mail"));
        assert_eq!(parse_body(&item.body).unwrap().1, "do the thing");

        // The wrong session cannot claim it; the right one can.
        assert!(matches!(
            claim(&st.lock_ok(), "mail-from", &item.body),
            Claim::Plain
        ));
        let Claim::Mail(m) = claim(&st.lock_ok(), "mail-to", &item.body) else {
            panic!("the addressee claims its mail");
        };
        assert_eq!(m.text, "do the thing");
        assert_eq!(m.row.from_session.as_deref(), Some("mail-from"));
        assert!(m.sender.is_none(), "an owner sender records no principal");

        // A tampered body is not that mail.
        let tampered = item.body.replace("thing", "other thing");
        assert!(matches!(
            claim(&st.lock_ok(), "mail-to", &tampered),
            Claim::Plain
        ));

        finish(&st, &m.row, Ok("done"));
        let row = st.lock_ok().mail_get(&m.row.id).unwrap().unwrap();
        assert_eq!(row.state, "answered");
        assert_eq!(row.reply.as_deref(), Some("done"));
        // Re-running the exact body after the answer: spent, not plain.
        assert!(matches!(
            claim(&st.lock_ok(), "mail-to", &item.body),
            Claim::Spent(_)
        ));
    }

    #[test]
    fn a_crash_rerun_of_a_running_mail_is_claimed_again() {
        let (st, _dir) = store();
        {
            let s = st.lock_ok();
            s.create_session("crash-to").unwrap();
        }
        let sent = send(&st, &Sender::default(), "crash-to", "x").unwrap();
        let body = st.lock_ok().queue_items("crash-to").unwrap()[0]
            .body
            .clone();
        assert!(matches!(
            claim(&st.lock_ok(), "crash-to", &body),
            Claim::Mail(_)
        ));
        // The process died mid-turn: the row is still `running`.
        assert!(
            matches!(claim(&st.lock_ok(), "crash-to", &body), Claim::Mail(_)),
            "the re-run is clamped again, not run as plain text"
        );
        let _ = sent;
    }

    #[test]
    fn sending_refuses_self_unknown_empty_and_oversized() {
        let (st, _dir) = store();
        st.lock_ok().create_session("self-a").unwrap();
        let me = Sender {
            session: Some("self-a".into()),
            ..Default::default()
        };
        assert!(send(&st, &me, "self-a", "hi").is_err());
        assert!(send(&st, &me, "nobody-here", "hi").is_err());
        st.lock_ok().create_session("self-b").unwrap();
        assert!(send(&st, &me, "self-b", "   ").is_err());
        assert!(send(&st, &me, "self-b", &"x".repeat(MAX_TEXT + 1)).is_err());
    }

    #[test]
    fn a_chain_continues_from_a_mail_turn_and_stops_at_the_cap() {
        let (st, _dir) = store();
        {
            let s = st.lock_ok();
            s.create_session("hop-a").unwrap();
            s.create_session("hop-b").unwrap();
        }
        let a = Sender {
            session: Some("hop-a".into()),
            ..Default::default()
        };
        let first = send(&st, &a, "hop-b", "one").unwrap();
        let row = st.lock_ok().mail_get(&first.id).unwrap().unwrap();
        // hop-b's turn runs that mail and sends on: same chain, hop 1.
        enter("hop-b", &row);
        let b = Sender {
            session: Some("hop-b".into()),
            ..Default::default()
        };
        let second = send(&st, &b, "hop-a", "two").unwrap();
        assert_eq!(second.chain_id, first.chain_id);
        assert_eq!(second.hop, 1);
        leave("hop-b");
        // A turn deep in a chain cannot send past the cap.
        let mut deep = row.clone();
        deep.hop = MAX_HOPS;
        enter("hop-b", &deep);
        let e = send(&st, &b, "hop-a", "too far").unwrap_err();
        assert!(format!("{e:#}").contains("chain stops"), "{e:#}");
        leave("hop-b");
        // Outside a mail turn, a new chain starts.
        let fresh = send(&st, &b, "hop-a", "new").unwrap();
        assert_eq!((fresh.chain_id.as_str(), fresh.hop), (fresh.id.as_str(), 0));
    }

    #[test]
    fn the_turn_runs_with_the_lower_of_sender_and_target() {
        let owner = Principal::owner();
        let user = Principal::known("telegram:1", Role::User);
        let admin = Principal::known("telegram:2", Role::Admin);
        // Owner sender: the target's own rights, whatever they are.
        assert_eq!(turn_principal(None, None), None);
        assert_eq!(turn_principal(None, Some(&user)), None);
        assert_eq!(turn_principal(Some(&owner), Some(&user)), None);
        // A lower sender clamps an owner or higher target down to it.
        assert_eq!(turn_principal(Some(&user), None), Some(user.clone()));
        assert_eq!(
            turn_principal(Some(&user), Some(&admin)),
            Some(user.clone())
        );
        // A higher sender never raises a lower target.
        assert_eq!(turn_principal(Some(&admin), Some(&user)), None);
        // Same role: the capabilities both hold.
        let user_no_web = Principal::known("telegram:3", Role::User).without(Cap::Web);
        let clamped = turn_principal(Some(&user_no_web), Some(&user)).unwrap();
        assert!(!clamped.has(Cap::Web));
        assert_eq!(
            clamped.subject, user.subject,
            "it stays the target's session subject"
        );
        assert_eq!(turn_principal(Some(&user), Some(&user_no_web)), None);
    }

    #[test]
    fn an_unparseable_sender_is_a_guest_never_the_owner() {
        let (st, _dir) = store();
        st.lock_ok().create_session("bad-p").unwrap();
        let id = new_id();
        let body = render_body(&id, "hi");
        st.lock_ok()
            .mail_enqueue(&NewMail {
                id: &id,
                to_session: "bad-p",
                from_session: Some("x"),
                from_title: None,
                from_node: None,
                from_pid: None,
                from_principal: Some("{not json"),
                via_peer: None,
                chain_id: &id,
                hop: 0,
                body: &body,
                body_sha: &body_sha(&body),
                kind: "mail",
            })
            .unwrap();
        let Claim::Mail(m) = claim(&st.lock_ok(), "bad-p", &body) else {
            panic!("claimable");
        };
        assert_eq!(m.sender.as_ref().map(|p| p.role), Some(Role::Guest));
        assert_eq!(
            turn_principal(m.sender.as_ref(), None).map(|p| p.role),
            Some(Role::Guest)
        );
    }

    #[test]
    fn references_resolve_by_id_prefix_and_title_preferring_open_sessions() {
        let (st, _dir) = store();
        let s = st.lock_ok();
        s.create_session("20261011_100000_aaaaaa").unwrap();
        s.create_session("20261011_100000_bbbbbb").unwrap();
        s.create_session("20261011_200000_cccccc").unwrap();
        s.set_title("20261011_100000_aaaaaa", "phone").unwrap();
        s.set_title("20261011_200000_cccccc", "phone").unwrap();
        assert_eq!(
            resolve(&s, "20261011_100000_aaaaaa", &[]).unwrap(),
            Resolved::One("20261011_100000_aaaaaa".into())
        );
        assert_eq!(
            resolve(&s, "20261011_2", &[]).unwrap(),
            Resolved::One("20261011_200000_cccccc".into())
        );
        assert!(matches!(
            resolve(&s, "20261011_1", &[]).unwrap(),
            Resolved::Many(_)
        ));
        assert!(matches!(
            resolve(&s, "phone", &[]).unwrap(),
            Resolved::Many(_)
        ));
        assert_eq!(
            resolve(&s, "phone", &["20261011_200000_cccccc".to_string()]).unwrap(),
            Resolved::One("20261011_200000_cccccc".into())
        );
        assert_eq!(resolve(&s, "nope", &[]).unwrap(), Resolved::None);
    }

    #[tokio::test]
    async fn a_wait_returns_the_reply_when_the_turn_ends() {
        let (st, _dir) = store();
        st.lock_ok().create_session("wait-to").unwrap();
        let sent = send(&st, &Sender::default(), "wait-to", "q").unwrap();
        let body = st.lock_ok().queue_items("wait-to").unwrap()[0].body.clone();
        let Claim::Mail(m) = claim(&st.lock_ok(), "wait-to", &body) else {
            panic!("claimable");
        };
        let st2 = st.clone();
        let row = m.row.clone();
        crate::rt::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            finish(&st2, &row, Ok("the answer"));
        });
        let w = wait(&st, &sent.id, Duration::from_secs(10)).await.unwrap();
        assert_eq!(w, Waited::Answered("the answer".into()));
        let row = st.lock_ok().mail_get(&sent.id).unwrap().unwrap();
        assert!(!row.waiting, "the waiting flag is cleared after the wait");
        // And a wait that runs out reports the state it saw.
        let other = send(&st, &Sender::default(), "wait-to", "r").unwrap();
        let w = wait(&st, &other.id, Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(w, Waited::Pending("queued".into()));
    }

    /// A provider that answers every chat call with `reply` and keeps
    /// each request body, so a test can read what the model was sent.
    async fn capture_server(reply: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let s = seen.clone();
        crate::rt::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut raw = Vec::new();
                let mut chunk = [0u8; 16384];
                let mut need: Option<usize> = None;
                loop {
                    let n = match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&chunk[..n]);
                    if need.is_none() {
                        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                            let len = head
                                .split("content-length:")
                                .nth(1)
                                .and_then(|t| t.split(['\r', '\n']).next())
                                .and_then(|t| t.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            need = Some(end + 4 + len);
                        }
                    }
                    if matches!(need, Some(t) if raw.len() >= t) {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&raw).to_string();
                let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                s.lock_ok().push(body);
                let sse = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"role\":\"assistant\",\"content\":\"{reply}\"}}}}]}}\n\ndata: [DONE]\n\n"
                );
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    sse.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(sse.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{addr}/v1"), seen)
    }

    /// The whole turn, through `Agent::run_turn` against a real wire: an
    /// ADMIN session gets mail from a USER-level sender, so the turn
    /// must run as the user (the prompt the provider is sent says so),
    /// write mail provenance instead of surface provenance, record the
    /// answer as the reply, put the admin back afterwards — and refuse
    /// to run the same body a second time.
    #[tokio::test]
    async fn a_mail_turn_runs_clamped_records_its_reply_and_never_runs_twice() {
        // The prompt build reads memory and skills under VELES_HOME.
        let _home = crate::testenv::scratch_home();
        let (url, seen) = capture_server("pong").await;
        let target = "mail-turn-target";
        let agent = crate::testenv::mock_agent(&url, target);
        let st = agent.store.clone();
        {
            let s = st.lock_ok();
            let _ = s.create_session(target);
            let _ = s.create_session("mail-turn-sender");
        }
        let admin = Principal::known("telegram:7001", Role::Admin);
        agent.set_principal(admin.clone());
        let from = Sender {
            session: Some("mail-turn-sender".into()),
            title: Some("the worker".into()),
            principal: Some(Principal::known("telegram:7002", Role::User)),
        };
        let sent = send(&st, &from, target, "what time is it").unwrap();
        let item = st.lock_ok().queue_take_next(target).unwrap().unwrap();
        let mut history = Vec::new();
        let res = agent
            .run_turn(&mut history, &item.body, None, |_| {})
            .await
            .unwrap();
        assert_eq!(res.reply, "pong");

        // What the provider was sent: the user's rights, the mail's
        // provenance, the text without its header.
        let bodies = seen.lock_ok().clone();
        assert_eq!(bodies.len(), 1, "one provider call");
        let req = &bodies[0];
        assert!(
            req.contains("the host is off-limits to them"),
            "the turn ran with the USER's rights (below the admin target)"
        );
        assert!(
            req.contains("this message is mail from session"),
            "provenance"
        );
        assert!(req.contains("the worker"), "the sender is named");
        assert!(req.contains("what time is it"));
        assert!(
            !req.contains("[veles mail "),
            "the header never reaches the model"
        );

        // The admin is back, and its prompt with it.
        assert_eq!(agent.principal().as_deref(), Some(&admin));
        assert!(!agent
            .system_prompt
            .read()
            .unwrap()
            .contains("the host is off-limits to them"));
        assert!(current(target).is_none(), "the chain marker is gone");

        // The reply is recorded; the transcript labels the rows.
        let row = st.lock_ok().mail_get(&sent.id).unwrap().unwrap();
        assert_eq!(
            (row.state.as_str(), row.reply.as_deref()),
            ("answered", Some("pong"))
        );
        let hist = st.lock_ok().load_history(target).unwrap();
        let origin = hist
            .iter()
            .find(|m| m.injection.as_deref() == Some("origin:mail"))
            .expect("a mail origin row");
        assert!(origin.content.starts_with("[origin] this message is mail"));
        let text_row = hist
            .iter()
            .find(|m| m.content == "what time is it")
            .expect("the text row");
        assert_eq!(text_row.injection.as_deref(), Some("mail:mail-turn-sender"));
        assert!(
            !hist
                .iter()
                .any(|m| m.injection.as_deref() == Some("origin:other")),
            "no surface provenance for a mail turn"
        );

        // The same body again (a crash between answer and release): no
        // second turn, no second provider call.
        let again = agent
            .run_turn(&mut history, &item.body, None, |_| {})
            .await
            .unwrap();
        assert!(again.reply.is_empty());
        assert_eq!(seen.lock_ok().len(), 1, "no second provider call");
    }

    /// A capped mail turn keeps its cap whatever happens to the session's
    /// own principal meanwhile. The gateway re-resolves the session's
    /// person for every message that arrives during a turn (it parks
    /// them). A clamp written over the session's own principal made that
    /// re-resolve put the higher rights back mid-turn, so the rest of
    /// the mail turn ran with the session's rights instead of the
    /// sender's (found 2026-10-13). An ACL refresh to the same rights
    /// does not lift the cap either, a revocation below the cap still
    /// lowers the turn, and the prompt stays the one the cap allows.
    #[tokio::test]
    async fn a_capped_mail_turn_keeps_its_cap_when_the_session_is_re_resolved() {
        let _home = crate::testenv::scratch_home();
        let (url, seen) = capture_server("pong").await;
        let target = "mail-cap-target";
        let agent = crate::testenv::mock_agent(&url, target);
        let st = agent.store.clone();
        {
            let s = st.lock_ok();
            let _ = s.create_session(target);
            let _ = s.create_session("mail-cap-sender");
        }
        let admin = Principal::known("telegram:7101", Role::Admin);
        agent.set_principal(admin.clone());
        let from = Sender {
            session: Some("mail-cap-sender".into()),
            title: Some("the worker".into()),
            principal: Some(Principal::known("telegram:7102", Role::User)),
        };
        send(&st, &from, target, "what time is it").unwrap();
        let item = st.lock_ok().queue_take_next(target).unwrap().unwrap();
        let roles = |a: &crate::agent::Agent| {
            (
                a.principal().map(|p| p.role),
                crate::acl::session_principal(target).map(|p| p.role),
            )
        };
        let mut seen_roles = Vec::new();
        let mut once = false;
        let mut history = Vec::new();
        agent
            .run_turn(&mut history, &item.body, None, |_| {
                if std::mem::replace(&mut once, true) {
                    return;
                }
                // The gateway's re-resolve (`agent_for_sender`), as it
                // runs for a message that parks behind this turn.
                if agent.standing_principal().as_deref() != Some(&admin) {
                    agent.set_principal(admin.clone());
                }
                seen_roles.push(roles(&agent));
                // An ACL refresh to the same rights.
                agent.set_principal(admin.clone());
                seen_roles.push(roles(&agent));
                // A revocation below the cap, then the rights back.
                agent.set_principal(Principal::known("telegram:7101", Role::Guest));
                seen_roles.push(roles(&agent));
                agent.set_principal(admin.clone());
            })
            .await
            .unwrap();
        let user = (Some(Role::User), Some(Role::User));
        let guest = (Some(Role::Guest), Some(Role::Guest));
        assert_eq!(seen_roles, vec![user, user, guest], "during the turn");
        let bodies = seen.lock_ok().clone();
        assert_eq!(bodies.len(), 1, "one provider call");
        assert!(
            bodies[0].contains("the host is off-limits to them"),
            "the request after the re-resolves still carries the capped prompt"
        );
        assert_eq!(roles(&agent), (Some(Role::Admin), Some(Role::Admin)));
        assert!(!agent
            .system_prompt
            .read()
            .unwrap()
            .contains("the host is off-limits to them"));
    }

    /// A relayed hop count is only taken within 0..=MAX_HOPS: a peer that
    /// sends a negative or huge one gets a refusal, not a chain that
    /// never ends.
    #[test]
    fn a_relayed_hop_outside_the_range_is_refused() {
        let (st, _dir) = store();
        st.lock_ok().create_session("relay-to").unwrap();
        let from = Sender {
            session: Some("20261011_101010_abcdef".into()),
            title: Some("laptop worker".into()),
            principal: Some(Principal::known("a2a:laptop", Role::User)),
        };
        for hop in [-1, MAX_HOPS + 1, i64::MAX, i64::MIN] {
            let relay = Relay {
                peer: "laptop".into(),
                chain_id: Some("mabcdefabcdef0123".into()),
                hop,
            };
            let e = send_inner(
                &st,
                &from,
                Some(&relay),
                KIND_MAIL,
                "relay-to",
                "hi",
                |_| Delivery::Queued,
            )
            .unwrap_err();
            assert!(e.to_string().contains("chain stops at"), "hop {hop}: {e}");
        }
        let relay = Relay {
            peer: "laptop".into(),
            chain_id: Some("mabcdefabcdef0123".into()),
            hop: MAX_HOPS,
        };
        assert!(
            send_inner(
                &st,
                &from,
                Some(&relay),
                KIND_MAIL,
                "relay-to",
                "hi",
                |_| { Delivery::Queued }
            )
            .is_ok(),
            "the last hop within the limit still lands"
        );
    }

    /// Mail from another node says which node, and how to answer it there:
    /// `<peer>/<session>` — the address `sessions send` takes. Local mail
    /// names no node (this one is implied) and answers to the bare id.
    #[test]
    fn relayed_mail_names_its_node_and_its_reply_address() {
        let (st, _dir) = store();
        st.lock_ok().create_session("relay-to").unwrap();
        let from = Sender {
            session: Some("20261011_101010_abcdef".into()),
            title: Some("laptop worker".into()),
            principal: Some(Principal::known("a2a:laptop", Role::User)),
        };
        let relay = Relay {
            peer: "laptop".into(),
            chain_id: Some("mabcdefabcdef0123".into()),
            hop: 2,
        };
        let sent = send_inner(
            &st,
            &from,
            Some(&relay),
            KIND_MAIL,
            "relay-to",
            "hi",
            |_| Delivery::Queued,
        )
        .unwrap();
        assert_eq!((sent.chain_id.as_str(), sent.hop), ("mabcdefabcdef0123", 2));
        let row = st.lock_ok().mail_get(&sent.id).unwrap().unwrap();
        assert_eq!(row.via_peer.as_deref(), Some("laptop"));
        assert!(row.from_pid.is_none());
        let line = origin_line(&row);
        assert!(line.contains("on node laptop"), "{line}");
        assert!(line.contains("to=laptop/20261011_101010_abcdef"), "{line}");
        assert_eq!(injection_tag(&row), "mail:laptop/20261011_101010_abcdef");

        let local = send(&st, &from, "relay-to", "hi").unwrap();
        let row = st.lock_ok().mail_get(&local.id).unwrap().unwrap();
        assert!(row.via_peer.is_none());
        let line = origin_line(&row);
        assert!(!line.contains("on node"), "{line}");
        assert!(line.contains("to=20261011_101010_abcdef."), "{line}");
    }

    // ── the owner's lines in a session they joined (crate::join) ──

    fn owner() -> Principal {
        let mut p = Principal::known("irc:wrath", Role::Owner);
        p.caps = [Cap::All].into_iter().collect();
        p
    }

    fn join_send(st: &Arc<Mutex<Store>>, who: &Principal, to: &str, text: &str) -> Result<Sent> {
        send_join_with(st, who, "wrath", to, text, |_| Delivery::Queued)
    }

    #[test]
    fn a_join_line_is_the_owners_and_is_claimed_as_one() {
        let (st, _dir) = store();
        st.lock_ok().create_session("join-a").unwrap();
        let sent = join_send(&st, &owner(), "join-a", "hello there").unwrap();
        let row = st.lock_ok().mail_get(&sent.id).unwrap().unwrap();
        assert_eq!(row.kind, KIND_JOIN);
        assert_eq!(row.from_title.as_deref(), Some("wrath"));
        assert!(row.from_session.is_none() && row.via_peer.is_none());
        let item = st.lock_ok().queue_take_next("join-a").unwrap().unwrap();
        let Claim::Mail(m) = claim(&st.lock_ok(), "join-a", &item.body) else {
            panic!("a join line is claimed like mail");
        };
        assert!(m.join, "the owner's own line");
        assert_eq!(m.text, "hello there");
        assert_eq!(m.sender.as_ref().map(|p| p.role), Some(Role::Owner));
        // Ordinary mail is never a join line.
        let from = Sender {
            session: Some("x".into()),
            title: None,
            principal: Some(owner()),
        };
        st.lock_ok().create_session("join-b").unwrap();
        let sent = send(&st, &from, "join-b", "mail").unwrap();
        let row = st.lock_ok().mail_get(&sent.id).unwrap().unwrap();
        assert_eq!(row.kind, KIND_MAIL);
        let item = st.lock_ok().queue_take_next("join-b").unwrap().unwrap();
        let Claim::Mail(m) = claim(&st.lock_ok(), "join-b", &item.body) else {
            panic!("mail");
        };
        assert!(!m.join, "mail, even the owner's, is not a joined line");
    }

    #[test]
    fn only_the_owner_writes_a_join_line() {
        let (st, _dir) = store();
        st.lock_ok().create_session("join-c").unwrap();
        for p in [
            Principal::known("irc:adm", Role::Admin),
            Principal::known("irc:u", Role::User),
            // The owner's role without the owner's caps is not the owner.
            Principal::known("irc:o", Role::Owner),
        ] {
            let mut p = p;
            p.caps.remove(&Cap::All);
            let e = join_send(&st, &p, "join-c", "hi").unwrap_err();
            assert!(e.to_string().contains("only the owner"), "{e}");
        }
        assert!(st.lock_ok().queue_take_next("join-c").unwrap().is_none());
    }

    /// The raise is the row's, and only a row the owner wrote HERE earns
    /// it: one naming a lesser sender, or one that came from another node,
    /// runs as ordinary mail (the lower of the two rights).
    #[test]
    fn a_join_row_that_is_not_the_owners_own_runs_as_mail() {
        let (st, _dir) = store();
        let cases = [
            ("join-d", Principal::known("irc:u", Role::User), None),
            ("join-e", owner(), Some("laptop")),
        ];
        for (sid, who, peer) in cases {
            st.lock_ok().create_session(sid).unwrap();
            let id = new_id();
            let body = render_body(&id, "raise me");
            let pj = serde_json::to_string(&who).unwrap();
            st.lock_ok()
                .mail_enqueue(&NewMail {
                    id: &id,
                    to_session: sid,
                    from_session: None,
                    from_title: Some("wrath"),
                    from_node: None,
                    from_pid: None,
                    from_principal: Some(&pj),
                    via_peer: peer,
                    chain_id: &id,
                    hop: 0,
                    body: &body,
                    body_sha: &body_sha(&body),
                    kind: KIND_JOIN,
                })
                .unwrap();
            let Claim::Mail(m) = claim(&st.lock_ok(), sid, &body) else {
                panic!("claimed");
            };
            assert!(!m.join, "{sid}: not the owner's own line");
        }
    }

    #[test]
    fn the_join_origin_names_the_owner_and_the_sessions_person() {
        let (st, _dir) = store();
        st.lock_ok().create_session("join-f").unwrap();
        let sent = join_send(&st, &owner(), "join-f", "x").unwrap();
        let row = st.lock_ok().mail_get(&sent.id).unwrap().unwrap();
        let l = join_origin_line(&row, Some("alice"));
        assert!(
            l.starts_with("[origin] wrath, the owner of this agent, has joined"),
            "{l}"
        );
        assert!(l.contains("alice did not write it"), "{l}");
        assert!(l.contains("runs with the owner's rights"), "{l}");
        let l = join_origin_line(&row, None);
        assert!(
            l.contains("the person this conversation belongs to did not"),
            "{l}"
        );
    }

    struct Recorder;
    #[async_trait::async_trait]
    impl crate::ask::Asker for Recorder {
        async fn ask(&self, _req: crate::ask::AskRequest) -> crate::ask::AskAnswer {
            crate::ask::AskAnswer::choice("deny")
        }
    }

    /// The whole joined line, through `Agent::run_turn` against a real
    /// wire: a USER session, the owner's line. During the turn the
    /// session's rights and its asks are the owner's (a re-resolve of the
    /// session's person mid-turn, which the gateway does for every
    /// message it parks, does not take them back); the prompt stays the
    /// user's, so none of the owner's memory reaches a model the user
    /// reads; afterwards everything is the user's again.
    #[tokio::test]
    async fn a_joined_line_runs_with_the_owners_rights_and_asks_the_owner() {
        let _home = crate::testenv::scratch_home();
        let (url, seen) = capture_server("hi wrath").await;
        let target = "join-turn-target";
        let agent = crate::testenv::mock_agent(&url, target);
        let st = agent.store.clone();
        let _ = st.lock_ok().create_session(target);
        let user = Principal::known("irc:alice", Role::User);
        agent.set_principal(user.clone());
        crate::acl::set_session_sender(
            target,
            crate::acl::SenderContext {
                mask: Some("alice!~a@host".into()),
                ..Default::default()
            },
        );
        let prompt_before = agent.system_prompt.read().unwrap().clone();
        let rec: Arc<dyn crate::ask::Asker> = Arc::new(Recorder);
        crate::join::set(
            target,
            crate::join::Joined {
                owner_name: "wrath".into(),
                asker: rec.clone(),
            },
        );
        join_send(&st, &owner(), target, "alice, try it again").unwrap();
        let item = st.lock_ok().queue_take_next(target).unwrap().unwrap();
        let mut during = Vec::new();
        let mut history = Vec::new();
        let res = agent
            .run_turn(&mut history, &item.body, None, |_| {
                // Whatever the gateway re-resolves meanwhile, and a
                // memory write's prompt rebuild: neither may take the
                // owner's rights back, or put the owner's memory in
                // alice's prompt.
                agent.set_principal(user.clone());
                agent.rebuild_system_prompt();
                let role = crate::acl::session_principal(target).map(|p| p.role);
                let asks_owner = crate::ask::turn_asker(target)
                    .is_some_and(|a| std::ptr::addr_eq(Arc::as_ptr(&a), Arc::as_ptr(&rec)));
                let agent_asks_owner = agent
                    .asker()
                    .is_some_and(|a| std::ptr::addr_eq(Arc::as_ptr(&a), Arc::as_ptr(&rec)));
                during.push((
                    role,
                    agent.principal().map(|p| p.role),
                    asks_owner,
                    agent_asks_owner,
                ));
            })
            .await
            .unwrap();
        assert_eq!(res.reply, "hi wrath");
        assert!(!during.is_empty());
        for d in &during {
            assert_eq!(
                *d,
                (Some(Role::Owner), Some(Role::Owner), true, true),
                "the owner's rights and asks for the whole turn"
            );
        }
        // The provider saw the user's prompt and the joined line.
        let bodies = seen.lock_ok().clone();
        assert_eq!(bodies.len(), 1);
        let req = &bodies[0];
        assert!(
            req.contains("You are NOT talking to the owner"),
            "the prompt is still the user's"
        );
        assert!(req.contains("wrath, the owner of this agent, has joined"));
        assert!(req.contains("alice did not write it"));
        assert!(req.contains("alice, try it again"));
        assert!(!req.contains("[veles mail "));
        // Afterwards: the user's again, the prompt untouched.
        assert_eq!(agent.principal().as_deref(), Some(&user));
        assert_eq!(
            crate::acl::session_principal(target).map(|p| p.role),
            Some(Role::User)
        );
        assert!(crate::ask::turn_asker(target).is_none());
        assert!(!agent
            .asker()
            .is_some_and(|a| std::ptr::addr_eq(Arc::as_ptr(&a), Arc::as_ptr(&rec))));
        assert_eq!(*agent.system_prompt.read().unwrap(), prompt_before);
        // The transcript: the owner's line is a plain user row after an
        // origin row that names both people.
        let hist = st.lock_ok().load_history(target).unwrap();
        let origin = hist
            .iter()
            .find(|m| m.injection.as_deref() == Some("origin:join"))
            .expect("a join origin row");
        assert!(origin.content.contains("wrath"));
        let line = hist
            .iter()
            .find(|m| m.content == "alice, try it again")
            .expect("the line");
        assert!(
            line.injection.is_none(),
            "the owner's surfaces show it as theirs"
        );

        // The user's own next turn names its speaker while wrath is here.
        let _ = agent
            .run_turn(&mut history, "thanks", None, |_| {
                assert_eq!(
                    crate::acl::session_principal(target).map(|p| p.role),
                    Some(Role::User),
                    "the user's own turn has the user's rights"
                );
            })
            .await
            .unwrap();
        let hist = st.lock_ok().load_history(target).unwrap();
        let o = hist
            .iter()
            .rev()
            .find(|m| m.content.starts_with("[origin] this message arrived via"))
            .expect("the user's origin row");
        assert!(o.content.contains(", from alice."), "{}", o.content);
        assert!(o
            .content
            .contains("wrath, the owner of this agent, has joined"));
        crate::join::clear(target);
    }

    /// With the owner gone from the session before their line ran, its
    /// questions have nobody to go to: an approval is refused by policy,
    /// never asked of the session's own person.
    #[tokio::test]
    async fn a_joined_line_with_the_owner_gone_asks_nobody() {
        let _home = crate::testenv::scratch_home();
        let (url, _seen) = capture_server("ok").await;
        let target = "join-turn-gone";
        let agent = crate::testenv::mock_agent(&url, target);
        let st = agent.store.clone();
        let _ = st.lock_ok().create_session(target);
        agent.set_principal(Principal::known("irc:bob", Role::User));
        // The session's own person can be asked: they must not be.
        let _bob = agent.set_asker(Arc::new(Recorder));
        crate::join::clear(target);
        join_send(&st, &owner(), target, "still there?").unwrap();
        let item = st.lock_ok().queue_take_next(target).unwrap().unwrap();
        let mut interactive = Vec::new();
        let mut history = Vec::new();
        agent
            .run_turn(&mut history, &item.body, None, |_| {
                interactive.push(agent.asker().map(|a| a.interactive()));
            })
            .await
            .unwrap();
        assert!(!interactive.is_empty());
        assert!(
            interactive.iter().all(|i| *i == Some(false)),
            "{interactive:?}"
        );
        assert_eq!(
            agent.asker().map(|a| a.interactive()),
            Some(true),
            "after the turn, the session's own person is asked again"
        );
    }

    #[test]
    fn a_waited_send_back_into_a_waiting_chain_is_detectable() {
        let (st, _dir) = store();
        {
            let s = st.lock_ok();
            s.create_session("cyc-a").unwrap();
            s.create_session("cyc-b").unwrap();
        }
        let a = Sender {
            session: Some("cyc-a".into()),
            ..Default::default()
        };
        let m = send(&st, &a, "cyc-b", "ask").unwrap();
        st.lock_ok().mail_set_waiting(&m.id, true).unwrap();
        assert!(st
            .lock_ok()
            .mail_waiting_in_chain("cyc-a", &m.chain_id)
            .unwrap());
        assert!(!st
            .lock_ok()
            .mail_waiting_in_chain("cyc-b", &m.chain_id)
            .unwrap());
        st.lock_ok().mail_set_waiting(&m.id, false).unwrap();
        assert!(!st
            .lock_ok()
            .mail_waiting_in_chain("cyc-a", &m.chain_id)
            .unwrap());
    }
}
