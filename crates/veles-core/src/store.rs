//! SQLite store: sessions, messages, FTS5 search, memory, cron.
//!
//! Parity targets (hermes):
//! - state.db schema: sessions, messages (FTS5-indexed), used by
//!   session_search + conversation resume.
//! - memory tables with per-store char budgets (memory / user).
//! - cron jobs table.

use crate::providers::{ChatMessage, ToolMeta};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// A fresh session id: local time plus six hex digits, the shape every
/// surface has minted since the start (`20260928_130531_d544b5`).
pub fn new_session_id() -> String {
    use chrono::Local;
    format!(
        "{}_{}",
        Local::now().format("%Y%m%d_%H%M%S"),
        &uuid::Uuid::new_v4().to_string()[..6]
    )
}

/// One message as `import_session_batch` takes it:
/// `(role, content, reasoning, tool_calls, tool_call_id, tool_name, created_at)`.
pub type ImportMessageRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);

/// What a cached prompt token bills at, as a fraction of list price.
///
/// Routes charge a cache READ at anywhere from ≈1/3 to ≈1/10 of the
/// uncached rate; rather than keep a per-route table we bill a flat
/// 1/6 and accept being wrong in both directions by a factor of two.
/// The budget's job is to bound a bill, not to reproduce an invoice.
pub const CACHE_READ_DIVISOR: u64 = 6;

/// One row of `usage_by_provider_scoped`: a provider and what it spent.
///
/// A named struct for the same reason `ProviderUsageRow` is one — the
/// positional tuple it could have been is how the usage field-order bug
/// shipped once already.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLedgerRow {
    pub provider: String,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

/// One row of `usage_by_provider`.
///
/// A named struct rather than the 5-tuple it replaced: positional
/// tuples are how the usage field-order bug shipped once already (see
/// `UsageTotals`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderUsageRow {
    pub provider: String,
    pub model: String,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Prompt tokens served from the provider's prefix cache.
    pub cached_tokens: u64,
    /// Tokens written into the cache, where the provider reports it.
    pub cache_write_tokens: u64,
    /// The same two counts over the MOST RECENT SESSION that used this
    /// provider/model — how the cache is working now, as opposed to
    /// what the lifetime figure was billed at.
    ///
    /// A session, not a fixed number of calls: "last N calls" has to
    /// pick an N, and any N is fitted to whatever history happens to be
    /// in the table. The first attempt here used 20 and the real row it
    /// was written for had 19 calls, so the window covered the very
    /// history it existed to exclude. A session is a real boundary the
    /// data already carries.
    pub last_session_prompt_tokens: u64,
    pub last_session_cached_tokens: u64,
    /// How many calls that session made on this provider/model, so a
    /// caller can say how much the reading rests on.
    pub last_session_calls: u64,
}

impl ProviderUsageRow {
    /// The most recent session's cache share, or `None` when it carries
    /// no prompt tokens to divide by. None is "no reading", never 0%.
    pub fn last_session_cache_rate(&self) -> Option<f64> {
        (self.last_session_prompt_tokens > 0).then(|| {
            self.last_session_cached_tokens as f64 / self.last_session_prompt_tokens as f64
        })
    }
}

/// Usage totals, optionally filtered to one session.
///
/// A named struct rather than the `(u64, u64, u64)` tuple it replaced:
/// a field-order bug in that shape shipped once and made every token
/// count wrong (the call count was read as the prompt total — see the
/// comment at `agent.rs::context_report`). Five u64s in a row would
/// invite the exact same mistake.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Prompt tokens served from the provider's prefix cache.
    pub cached_tokens: u64,
    /// Tokens written into the cache, where the provider reports it.
    pub cache_write_tokens: u64,
    /// Reasoning tokens, summed over the calls that reported them.
    /// `None` = NO call reported the field (folded thinking into
    /// completion), never "zero reasoning". Same contract as
    /// `CallTokens::reasoning_tokens`, summed.
    pub reasoning_tokens: Option<u64>,
}

/// One subject's spend components over a window — the breakdown behind
/// `tokens_spent_since`, for the surfaces that show where a budget went.
/// The discounted bill is `uncached + cached/CACHE_READ_DIVISOR +
/// completion`, so this and that ceiling can never tell two stories.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpendParts {
    /// Prompt tokens at list price (this INCLUDES the cached half).
    pub prompt_tokens: u64,
    /// …of which were cache reads, billed at 1/CACHE_READ_DIVISOR.
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    /// Provider calls in the window.
    pub calls: u64,
}

impl SpendParts {
    /// The discounted bill, the same arithmetic `tokens_spent_since` sums.
    pub fn billed(&self) -> u64 {
        self.prompt_tokens.saturating_sub(self.cached_tokens)
            + self.cached_tokens / CACHE_READ_DIVISOR
            + self.completion_tokens
    }
}

/// The token counts for ONE recorded call.
///
/// A named struct for the same reason `UsageTotals` is one, and it is
/// the write side of those very numbers: four bare `u64` parameters in
/// a row is the exact shape that shipped the field-order bug, and
/// adding the two cache counts to `record_usage` is what pushed it
/// there. Named fields make a transposition a compile error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallTokens {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Prompt tokens served from the provider's prefix cache.
    pub cached_tokens: u64,
    /// Tokens written into the cache, where the provider reports it.
    pub cache_write_tokens: u64,
    /// Did the provider report a cache field AT ALL on this call?
    ///
    /// `cached_tokens: 0` answers three different questions at once —
    /// the provider said zero, the provider said nothing, or the code
    /// of the day never parsed it — and only this field separates them.
    /// Every share computed anywhere skips calls where it is false, so
    /// an endpoint that reports no cache reads as "no reading" instead
    /// of as a 0% hit rate someone then goes hunting for.
    pub cache_reported: bool,
    /// Reasoning tokens the provider split out of the completion.
    /// `None` = it reported nothing (folded thinking into completion);
    /// `Some(n)` = it split them, possibly to zero. Same "unknown ≠ 0"
    /// doctrine as `cache_reported`.
    pub reasoning_tokens: Option<u64>,
}

pub struct Store {
    conn: Connection,
}

/// One persisted engine note, as it comes back out of the store.
///
/// A named struct rather than a 5-tuple: `(i64, String, String, String,
/// String)` is four indistinguishable strings at every call site, and
// `IrcLine` and `IrcQuery` moved to `veles_types::irc` in Phase 4 (the
// IRC plugin renders an `irc.history` answer without linking core). The
// store keeps the writer (`irc_append`) and the reader (`irc_query`);
// these are re-exports so `store::IrcLine`/`store::IrcQuery` still name
// them everywhere they did.
pub use veles_types::irc::{IrcLine, IrcQuery};

/// A live turn, as the registry reads it back.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionRun {
    pub session_id: String,
    pub pid: i64,
    pub host: String,
    pub surface: String,
    pub started_at: String,
    pub heartbeat_at: String,
}

/// How an id-or-prefix resolved against the node registry.
#[derive(Debug)]
pub enum NodeMatch {
    One(Box<NodeRow>),
    None,
    /// The prefix matched this many nodes.
    Ambiguous(usize),
}

/// A session's stored override (R4.5) — the one durable home for the
/// model/provider/reasoning a session was switched to, whatever
/// surface did the switching.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SessionOverride {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
}

/// One queued message (R4.6) — text that arrived while the session's
/// turn was running, parked here until the turn boundary drains it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueueItem {
    pub session_id: String,
    pub seq: i64,
    pub body: String,
    pub state: String,
    /// 'message' (a queued turn), 'mail' (a queued turn another session
    /// sent — `crate::mail`; the drainer runs it like a message, and
    /// `Agent::run_turn` recognises its body) or 'steer' (a parked steer
    /// — injected into the NEXT turn's context, never run as a turn of
    /// its own).
    pub kind: Option<String>,
    /// The platform-native id of the message that parked this row, so a
    /// threaded platform answers under the message that asked. `None`
    /// for a mail or cron row, which parked behind no platform message,
    /// and for a row that parked before the column existed — the reply
    /// then threads under the session's own chat.
    pub reply_to: Option<String>,
    /// The adapter instance that carried the message which parked this
    /// row: two IRC networks are two audiences, and the answer must go
    /// back out the connection the question came in on.
    pub route: Option<String>,
    pub queued_at: String,
}

/// A mail to enqueue (`Store::mail_enqueue`). `body` is the queued text
/// exactly as the drainer will hand it to `run_turn`; `body_sha` is its
/// hash (`crate::mail::body_sha`).
#[derive(Debug, Clone)]
pub struct NewMail<'a> {
    pub id: &'a str,
    pub to_session: &'a str,
    pub from_session: Option<&'a str>,
    pub from_title: Option<&'a str>,
    pub from_node: Option<&'a str>,
    pub from_pid: Option<i64>,
    /// The sender's principal as JSON; `None` = the owner surface.
    pub from_principal: Option<&'a str>,
    /// The pool peer it came through (`crate::mailnet`); `None` = sent here.
    pub via_peer: Option<&'a str>,
    pub chain_id: &'a str,
    pub hop: i64,
    pub body: &'a str,
    pub body_sha: &'a str,
    /// `mail`, or `join`: a line the owner typed into a session they
    /// joined (`crate::mail::send_join`).
    pub kind: &'a str,
}

/// One `session_mail` row.
#[derive(Debug, Clone, PartialEq)]
pub struct MailRow {
    pub id: String,
    pub to_session: String,
    pub from_session: Option<String>,
    pub from_title: Option<String>,
    pub from_node: Option<String>,
    pub from_pid: Option<i64>,
    pub from_principal: Option<String>,
    pub via_peer: Option<String>,
    pub chain_id: String,
    pub hop: i64,
    pub body_sha: String,
    /// queued | running | answered | failed
    pub state: String,
    pub waiting: bool,
    pub reply: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// `mail` | `join` (see `NewMail::kind`).
    pub kind: String,
}

const MAIL_COLS: &str = "id, to_session, from_session, from_title, from_node, from_pid, \
     from_principal, chain_id, hop, body_sha, state, waiting, reply, created_at, updated_at, \
     via_peer, kind";

fn mail_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MailRow> {
    Ok(MailRow {
        id: r.get(0)?,
        to_session: r.get(1)?,
        from_session: r.get(2)?,
        from_title: r.get(3)?,
        from_node: r.get(4)?,
        from_pid: r.get(5)?,
        from_principal: r.get(6)?,
        chain_id: r.get(7)?,
        hop: r.get(8)?,
        body_sha: r.get(9)?,
        state: r.get(10)?,
        waiting: r.get::<_, i64>(11)? != 0,
        reply: r.get(12)?,
        created_at: r.get(13)?,
        updated_at: r.get(14)?,
        via_peer: r.get(15)?,
        kind: r.get(16)?,
    })
}

/// A session's frozen prompt — see the `session_prompt` table.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPrompt {
    pub system_prompt: String,
    /// The tool list as its requests carry it (a JSON array); empty until
    /// the first build that recorded one.
    pub tools_json: String,
    pub format: i64,
    /// The node that built it.
    pub node: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StoredNote {
    /// The engine_notes PK — a monotonic cursor a client can resume
    /// from. `after_msg_id` is an ANCHOR (where it renders), not a
    /// creation order: a note persisted after a client synced but
    /// anchored at/before the client's message cursor can ride the
    /// anchor gate unnoticed forever. The id closes that window.
    pub id: i64,
    /// The message this note followed, so it can be rendered back in
    /// the right place. 0 = before any message.
    pub after_msg_id: i64,
    pub kind: String,
    pub summary: String,
    /// Empty when the summary was all there was.
    pub detail: String,
    /// Serialized NoteToggle, or empty.
    pub toggle: String,
    /// When the engine recorded it (SQLite `datetime('now')`, UTC).
    pub created_at: String,
}

/// One row of the cluster node registry (`nodes` table).
///
/// Named struct rather than a tuple for the same reason every other row
/// in this file is one: positional fields are how the usage field-order
/// bug shipped. `gpu`/`caps` are raw JSON text as stored — the registry
/// is transport for the probe `node.rs` produces, and re-parsing here
/// would couple the store to a probe's schema it has no opinion about.
#[derive(Debug, Clone)]
pub struct NodeRow {
    pub id: String,
    pub name: String,
    pub host: String,
    pub version: String,
    pub uptime_class: String,
    pub gpu: Option<String>,
    pub endpoint: Option<String>,
    pub static_ip: Option<String>,
    /// Where `static_ip` came from (`node::StaticIpSource`): `asserted`
    /// — the operator's `[node] static_ip`, a promise that the WAN
    /// address is stable — is the only source today; a peer-observed
    /// one is B3a's. NULL with no address, and on a pre-source row.
    pub static_ip_source: Option<String>,
    pub caps: Option<String>,
    /// Latest peer-observed reflexive address (B3a): the address an
    /// authenticated peer's call arrived FROM, as WE saw it. Never set
    /// by the node itself and never gossiped — it is a local fact about
    /// how that peer reaches this one. `observed_at` is RFC 3339 UTC.
    pub observed_addr: Option<String>,
    pub observed_at: Option<String>,
    pub observed_by: Option<String>,
    /// Normalized 1-minute load 0-100 (B4), from the node's own
    /// heartbeat; NULL = unknown, which placement must never read as
    /// idle. Gossiped like every registry column.
    pub load1: Option<i64>,
    pub load_at: Option<String>,
    /// Lifetime count of sessions this node has owned (B4). Monotone by
    /// construction: only the owning node's writes move it.
    pub sessions_served: i64,
    pub registered_at: String,
    pub heartbeat_at: String,
}

/// A registry row as it travels between nodes (B3a gossip).
///
/// Deliberately NOT `NodeRow`: the peer-observed columns have no place
/// on the wire — an observation is a local fact about how a peer reaches
/// THIS node, and gossiping it would let one node's view overwrite
/// another's evidence. The wire carries exactly what a node may assert
/// about ITSELF (plus its heartbeat, which the merge treats as a
/// freshness hint, not truth).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GossipNodeRow {
    pub id: String,
    pub name: String,
    pub host: String,
    pub version: String,
    pub uptime_class: String,
    pub gpu: Option<String>,
    pub endpoint: Option<String>,
    pub static_ip: Option<String>,
    pub static_ip_source: Option<String>,
    pub caps: Option<String>,
    /// Self-asserted placement metrics (B4). Defaults for peers on
    /// older builds: unknown load, zero served.
    #[serde(default)]
    pub load1: Option<i64>,
    #[serde(default)]
    pub load_at: Option<String>,
    #[serde(default)]
    pub sessions_served: i64,
    pub registered_at: String,
    pub heartbeat_at: String,
}

impl GossipNodeRow {
    /// The wire form of a local registry row (observations dropped).
    pub fn from_row(r: &NodeRow) -> Self {
        Self {
            id: r.id.clone(),
            name: r.name.clone(),
            host: r.host.clone(),
            version: r.version.clone(),
            uptime_class: r.uptime_class.clone(),
            gpu: r.gpu.clone(),
            endpoint: r.endpoint.clone(),
            static_ip: r.static_ip.clone(),
            static_ip_source: r.static_ip_source.clone(),
            caps: r.caps.clone(),
            load1: r.load1,
            load_at: r.load_at.clone(),
            sessions_served: r.sessions_served,
            registered_at: r.registered_at.clone(),
            heartbeat_at: r.heartbeat_at.clone(),
        }
    }
}

/// This node's registry id, from `<config_dir>/node.toml` (B1.5).
///
/// `Option` because the answer is genuinely "not yet": a scratch test
/// home, a tool run before any gateway boot. NULL in the sessions.node
/// column means exactly that era, and a None here must not invent an
/// owner — an unattributed row is honest, a wrong one is corruption.
///
/// No `load_or_create`: reading must not mint an identity as a side
/// effect (the B1 contract `NodeIdentity::load` already upholds).
fn local_node_id() -> Option<String> {
    crate::node::NodeIdentity::load(&crate::config::config_dir())
        .ok()
        .flatten()
        .map(|i| i.id)
}

/// Add a column to a table, once, race-safe.
///
/// The idempotent migrations were check-then-ALTER with no transaction:
/// two `Store::open`s of a fresh `state.db` could both pass the
/// `pragma_table_info` check and both issue the ALTER, and the second
/// failed `duplicate column name` (seen once). Checking first is kept —
/// it is the common single-opener fast path and reads better than a
/// bare tolerate-errors ALTER — but a `duplicate column name` from a
/// racer who won between our check and our ALTER is now a no-op, not a
/// fatal open error. Any OTHER error still propagates.
///
/// Returns true when THIS call added the column (the caller's backfill
/// runs), false when it was already there (ours or a racer's). The
/// backfill ordering is preserved: a racer that lost the ALTER race
/// returns false and does not re-run the backfill.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<bool> {
    let has: bool = conn
        .prepare(&format!(
            "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"
        ))?
        .query_row(params![column], |r| r.get::<_, i64>(0))?
        > 0;
    if has {
        return Ok(false);
    }
    match conn.execute_batch(ddl) {
        Ok(()) => Ok(true),
        Err(e) => {
            // Lost the race to another opener: the column exists now.
            if e.to_string().contains("duplicate column name") {
                Ok(false)
            } else {
                Err(e.into())
            }
        }
    }
}

/// How long a store-lock wait or hold must last before it is logged.
const LOCK_WARN: std::time::Duration = std::time::Duration::from_millis(100);

/// The most recent timed holder to RELEASE the store lock: its name, how
/// long it held it, and when it let go. A waiter that waited reads this
/// to name what it waited behind.
static LAST_HOLDER: std::sync::Mutex<
    Option<(&'static str, std::time::Duration, std::time::Instant)>,
> = std::sync::Mutex::new(None);

/// A store guard that remembers who holds it and for how long.
pub struct TimedGuard<'a> {
    guard: std::sync::MutexGuard<'a, Store>,
    who: &'static str,
    since: std::time::Instant,
}

impl std::ops::Deref for TimedGuard<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.guard
    }
}

impl std::ops::DerefMut for TimedGuard<'_> {
    fn deref_mut(&mut self) -> &mut Store {
        &mut self.guard
    }
}

impl Drop for TimedGuard<'_> {
    fn drop(&mut self) {
        let held = self.since.elapsed();
        if held > LOCK_WARN {
            tracing::warn!("store lock: {} held it {}ms", self.who, held.as_millis());
        }
        *LAST_HOLDER.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((self.who, held, std::time::Instant::now()));
    }
}

/// Lock the shared store as `who`. The store is one process-global
/// `Arc<Mutex<Store>>` shared by every frontend, every turn and the sync
/// paths, so one long hold stalls every other surface (USER 2026-10-05:
/// the desktop freezes during sync events). The first version of this
/// only timed the WAIT, so a long wait named the waiter and never the
/// lock's holder. Now both sides speak:
///
/// - a timed holder that keeps the lock over 100 ms says so on release,
///   with its name;
/// - a caller that WAITED over 100 ms names the last timed holder to
///   release just before it got the lock — or says the holder was not a
///   timed site (a plain `lock_ok()`), which is itself the next clue.
pub fn lock_timed<'a>(m: &'a std::sync::Mutex<Store>, who: &'static str) -> TimedGuard<'a> {
    let wait_start = std::time::Instant::now();
    let guard = m.lock().unwrap_or_else(|p| p.into_inner());
    let waited = wait_start.elapsed();
    if waited > LOCK_WARN {
        let last = *LAST_HOLDER.lock().unwrap_or_else(|p| p.into_inner());
        match last {
            // Released while we waited: that is (most likely) who we
            // waited behind.
            Some((holder, held, at)) if at >= wait_start => tracing::warn!(
                "store lock: {who} waited {}ms — behind {holder}, which held it {}ms",
                waited.as_millis(),
                held.as_millis()
            ),
            _ => tracing::warn!(
                "store lock: {who} waited {}ms — the holder was not a timed site",
                waited.as_millis()
            ),
        }
    }
    TimedGuard {
        guard,
        who,
        since: std::time::Instant::now(),
    }
}

/// Read-only connections to the shared store's file, for a reader that
/// must neither wait for the shared `Arc<Mutex<Store>>` nor hold it up.
///
/// On 2026-10-13 the desktop's session list held that lock for 30–79 s
/// while a build had the disk, and every other desktop request queued
/// behind it. The API looked dead, with 79 connections waiting in its
/// listen backlog. In WAL mode SQLite runs readers beside the writer,
/// so a reader needs its own connection, not the lock. These are
/// SQLite's own opens of the same file, never `std::fs`, which would
/// drop the shared connection's POSIX locks. They are taken from an
/// idle list and put back after use.
pub struct ReadPool {
    path: Option<std::path::PathBuf>,
    idle: std::sync::Mutex<Vec<Store>>,
}

impl ReadPool {
    /// Idle connections kept for the next reader; any beyond close.
    const KEEP: usize = 4;

    /// A pool on `store`'s file. An in-memory store has no file, and
    /// its readers take the shared lock.
    pub fn new(store: &Store) -> Self {
        Self {
            path: store
                .conn
                .path()
                .filter(|p| !p.is_empty())
                .map(std::path::PathBuf::from),
            idle: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Run `f` on a read-only connection. It falls back to `shared`,
    /// locked as `who`, when there is no file or the open fails.
    pub fn read<T>(
        &self,
        shared: &std::sync::Mutex<Store>,
        who: &'static str,
        f: impl FnOnce(&Store) -> T,
    ) -> T {
        let idle = self.idle.lock().unwrap_or_else(|p| p.into_inner()).pop();
        let conn = idle.or_else(|| {
            let path = self.path.as_ref()?;
            Store::open_reader(path)
                .map_err(|e| tracing::warn!("store: {who} reads under the lock: {e:#}"))
                .ok()
        });
        let Some(conn) = conn else {
            return f(&lock_timed(shared, who));
        };
        let out = f(&conn);
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        if idle.len() < Self::KEEP {
            idle.push(conn);
        }
        out
    }
}

/// The last timed holder to release the lock, for tests.
#[cfg(test)]
fn last_holder() -> Option<(&'static str, std::time::Duration)> {
    LAST_HOLDER
        .lock()
        .unwrap()
        .map(|(who, held, _)| (who, held))
}

impl Store {
    /// Open another node's replica (`replicas/<node>.db`) READ-ONLY: the
    /// desktop's cross-node read surface (USER 2026-10-05: sessions that
    /// live on the VPS "present in the list and lazily-read when opened
    /// in read-only mode on another node … a key audit path"). No
    /// migrations, no marker, no writes — the applier owns the file —
    /// and SQLite's own open, never `std::fs` on a database this process
    /// also holds (that drops the other connection's POSIX locks).
    pub fn open_replica_ro(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| anyhow::anyhow!("open replica {} read-only: {e}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // A replica gains a column only when a row image carries it, so a
        // young one can lack `archived`, which every history read filters
        // on. A TEMP view (the temp schema stays writable on a read-only
        // connection, and shadows `main` for unqualified names) supplies
        // it as 0 — nothing is written to the replica.
        let has_archived = conn
            .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'archived'")?
            .exists([])?;
        if !has_archived {
            conn.execute_batch(
                "CREATE TEMP VIEW messages AS SELECT *, 0 AS archived FROM main.messages;",
            )?;
        }
        Ok(Self { conn })
    }

    /// A read-only connection to a store this process already has open
    /// ([`ReadPool`]). It runs no migrations, because the writer's open
    /// ran them.
    pub fn open_reader(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| anyhow::anyhow!("open {} read-only: {e}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self { conn })
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A fresh DB opened by two processes at once: one's CREATE+ALTER
        // block holds the WAL write lock past the other's 5s busy window,
        // and the loser fails `database is locked`. That is transient by
        // construction (the winner finishes and the schema is then whole),
        // so open retries on a busy error before giving up. An EXISTING
        // store's contention is already waited out by the 5s busy_timeout;
        // this covers the cold-start race the timeout cannot out-wait.
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match Self::open_once(path) {
                Ok(s) => return Ok(s),
                Err(e) => {
                    let locked = e.to_string().contains("database is locked");
                    if locked && attempt < 12 {
                        std::thread::sleep(std::time::Duration::from_millis(150 * attempt as u64));
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    fn open_once(path: &Path) -> Result<Self> {
        let is_fresh = !path.exists();
        let conn = Connection::open(path)?;
        // A file that exists but has no `messages` table is not another
        // program's session store (hermes' always has one — that is the
        // DB the ownership marker below exists to refuse). The one way it
        // happens is veles's OWN MemoryStore creating state.db
        // (memory_revisions only) before any Store::open: `veles memory
        // log` or `veles memory apply` as the first command on a new
        // node. Treating that as foreign withheld every table the marker
        // gates (nodes, cron_jobs, checkpoints, …) for the life of the
        // install. So it is adopted exactly like a fresh file.
        let had_messages: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(1)
            > 0;
        let adoptable = is_fresh || !had_messages;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;

        // veles-ownership marker: present only in DBs veles created.
        // Without this, opening a foreign DB (hermes' ~/.hermes/state.db
        // — see the 2026-08-09 poisoning incident) would go on to create
        // the messages_ai trigger, whose columns don't exist in Hermes'
        // external-content messages_fts and break every INSERT.
        //
        // Written BEFORE `messages` exists, so a veles file that has a
        // `messages` table always has its marker. Written after it (as it
        // was), an open that failed in between — the fresh-file race's
        // "database is locked", a crash — left `messages` and no marker,
        // and every later open, the retry in `open` included, read that
        // as a foreign store and withheld the marker-gated tables for the
        // life of the install.
        let meta_value: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key='veles_schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let owns_db = match meta_value {
            Some(v) => v == "1",
            None => {
                if adoptable {
                    conn.execute(
                        "INSERT OR IGNORE INTO meta(key, value)
                         VALUES ('veles_schema_version', '1')",
                        [],
                    )?;
                    true
                } else {
                    false
                }
            }
        };

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT,
                -- The session this one was FORKED from: a subagent child
                -- records its parent here (USER taxonomy, 2026-09-17:
                -- children are "available to attach and review from the
                -- session list", and /btw "forked off the parent
                -- session"). NULL for a session a human or a cron job
                -- started. Read by the session list to mark children and
                -- by /btw to find the history to copy.
                parent TEXT,
                -- The node that OWNS this session (B1.5): the one whose
                -- state.db is the sole writer of its rows. AUTOINCREMENT
                -- ids are unique only WITHIN one database, so a future
                -- 3-node consolidation needs to know which node a
                -- session's msg_id = 47 came from — and that fact is
                -- per-SESSION (~600 rows), not per-message (~91k): every
                -- message belongs to exactly one session. NULL = written
                -- before ownership was stamped (single-node era).
                node TEXT,
                created_at TEXT DEFAULT (datetime('now')),
                updated_at TEXT DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS messages (
                msg_id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                reasoning TEXT,
                tool_calls TEXT,
                tool_call_id TEXT,
                tool_name TEXT,
                injection TEXT,
                created_at TEXT DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session
                ON messages(session_id, msg_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_updated
                ON sessions(updated_at);
            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                role, content, session_id UNINDEXED, msg_id UNINDEXED
            );
            CREATE TABLE IF NOT EXISTS memories (
                store TEXT NOT NULL,           -- 'memory' | 'user'
                idx INTEGER,
                content TEXT NOT NULL,
                PRIMARY KEY (store, idx)
            );
            CREATE TABLE IF NOT EXISTS usage (
                usage_id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                -- Who this call was spent ON BEHALF OF (acl subject).
                -- NULL for an owner surface, which is not on a budget.
                subject TEXT,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                prompt_tokens INTEGER NOT NULL DEFAULT 0,
                completion_tokens INTEGER NOT NULL DEFAULT 0,
                cached_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                -- 1 = the provider reported a cache field on this call,
                -- 0 = it reported usage but no cache field, NULL = this
                -- row predates the question. A 0 in `cached_tokens` is
                -- three different facts without this column, and the
                -- one that matters is "nobody asked".
                cache_reported INTEGER,
                -- Reasoning tokens the provider billed, split out of the
                -- completion (usage.completion_tokens_details). NULL =
                -- the route reported nothing (it folds thinking into
                -- completion_tokens); a value, even 0, means it split
                -- them — the same "unknown ≠ 0" contract as
                -- cache_reported.
                reasoning_tokens INTEGER,
                created_at TEXT DEFAULT (datetime('now'))
            );
            "#,
        )?;

        // Safety net on top of the marker: only create the trigger when
        // the messages table is actually veles-shaped (has msg_id).
        let uses_veles_schema: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('messages')
                 WHERE name='msg_id'",
            )?
            .query_row([], |r| r.get::<_, i64>(0))?
            > 0;

        if owns_db && uses_veles_schema {
            // Migration: the first M2 schema shipped a minimal cron_jobs
            // (no run metadata). No jobs were ever created against it, so
            // an old-shape EMPTY table is rebuilt in place; a table with
            // rows is left alone and flagged.
            let has_old_cron: bool = conn
                .prepare(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type='table' AND name='cron_jobs'",
                )?
                .query_row([], |r| r.get::<_, i64>(0))?
                > 0;
            if has_old_cron {
                let has_run_count: bool = conn
                    .prepare(
                        "SELECT COUNT(*) FROM pragma_table_info('cron_jobs')
                         WHERE name='run_count'",
                    )?
                    .query_row([], |r| r.get::<_, i64>(0))?
                    > 0;
                if !has_run_count {
                    let rows: i64 = conn
                        .prepare("SELECT COUNT(*) FROM cron_jobs")?
                        .query_row([], |r| r.get(0))?;
                    anyhow::ensure!(
                        rows == 0,
                        "cron_jobs has {rows} row(s) in the old schema shape — \
                         migrate by hand, veles will not drop them"
                    );
                    conn.execute_batch("DROP TABLE cron_jobs;")?;
                }
            }
            conn.execute_batch(
                r#"
                CREATE TRIGGER IF NOT EXISTS messages_ai
                AFTER INSERT ON messages BEGIN
                    INSERT INTO messages_fts(role, content, session_id, msg_id)
                    VALUES (new.role, new.content, new.session_id, new.msg_id);
                END;
                CREATE TABLE IF NOT EXISTS inbound_ledger (
                    dedup_key TEXT PRIMARY KEY,
                    seen_at TEXT DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS cron_jobs (
                    id TEXT PRIMARY KEY,
                    name TEXT,
                    schedule TEXT NOT NULL,   -- normalized: once:<rfc3339> | every:<secs> | cron:<spec>
                    prompt TEXT NOT NULL,
                    deliver TEXT NOT NULL DEFAULT 'origin',
                    enabled INTEGER DEFAULT 1,
                    script TEXT,
                    skills TEXT,              -- JSON array of skill names
                    no_agent INTEGER DEFAULT 0,
                    sandbox INTEGER DEFAULT 0,  -- safe-interp script exec
                    model TEXT,
                    provider TEXT,
                    reasoning TEXT,
                    origin TEXT,              -- 'platform:chat_id' captured at creation
                    session TEXT,             -- named session to resume (Stage 5 wakeups)
                    repeat_max INTEGER,       -- NULL = forever
                    run_count INTEGER DEFAULT 0,
                    created_at TEXT DEFAULT (datetime('now')),
                    updated_at TEXT DEFAULT (datetime('now')),
                    last_run_at TEXT,
                    last_status TEXT,
                    next_run_at TEXT
                );
                CREATE TABLE IF NOT EXISTS cron_runs (
                    run_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    job_id TEXT NOT NULL,
                    started_at TEXT DEFAULT (datetime('now')),
                    finished_at TEXT,
                    status TEXT,              -- ok | error | silent | skipped
                    session_id TEXT,
                    detail TEXT               -- error text or delivery note
                );
                CREATE INDEX IF NOT EXISTS idx_cron_runs_job
                    ON cron_runs(job_id, run_id);
                -- M11 approvals: permanent "always allow" rules + an
                -- audit trail of every gated call (including the ones the
                -- aux screen waved through, so the policy can be reviewed).
                CREATE TABLE IF NOT EXISTS approval_rules (
                    rule_key TEXT PRIMARY KEY,
                    note TEXT,
                    created_at TEXT DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS approvals (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    tool TEXT NOT NULL,
                    rule_key TEXT,
                    detail TEXT,
                    verdict TEXT NOT NULL,     -- allow | deny
                    decided_by TEXT NOT NULL,  -- off|rule|session|aux|human|policy|hook|timeout
                    reason TEXT,
                    created_at TEXT DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_approvals_session
                    ON approvals(session_id, id);
                -- M12 todo: hermes keeps the list in memory; we persist it
                -- so --resume and gateway sessions don't lose the plan.
                -- Engine notes: every implicit mechanism that acted, so
                -- reopening a session shows what happened rather than a
                -- conversation that looks like you had it. `after_msg_id`
                -- pins the note between two messages, which is the whole
                -- point — "my history changed" is only answerable if you
                -- can see WHEN.
                CREATE TABLE IF NOT EXISTS engine_notes (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    after_msg_id INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    summary TEXT NOT NULL,
                    detail TEXT,
                    toggle TEXT,
                    created_at TEXT DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_engine_notes_session
                    ON engine_notes(session_id, after_msg_id, id);
                -- Which skills actually get used. Written on every
                -- skill_view; read by the prompt's `recent` index mode
                -- so the resident index reflects real use instead of
                -- listing all 136 alphabetically. Worth having on its
                -- own: it is the first time veles can answer "which
                -- skills do I actually reach for".
                CREATE TABLE IF NOT EXISTS skill_uses (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    at TEXT DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_skill_uses_name
                    ON skill_uses(name, id DESC);
                CREATE TABLE IF NOT EXISTS todos (
                    session_id TEXT NOT NULL,
                    item_id TEXT NOT NULL,
                    content TEXT NOT NULL,
                    status TEXT NOT NULL,      -- pending|in_progress|completed|cancelled
                    position INTEGER NOT NULL,
                    updated_at TEXT DEFAULT (datetime('now')),
                    PRIMARY KEY (session_id, item_id)
                );
                -- M13 checkpoints: pre-mutation copies of files the agent
                -- edits. Blobs live under VELES_HOME/checkpoints/.
                CREATE TABLE IF NOT EXISTS checkpoints (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    path TEXT NOT NULL,
                    blob TEXT,                 -- NULL when the file did not exist
                    sha256 TEXT,
                    bytes INTEGER NOT NULL DEFAULT 0,
                    tool TEXT NOT NULL,
                    note TEXT,
                    created_at TEXT DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_checkpoints_session
                    ON checkpoints(session_id, id);
                -- IRC scrollback. The bot's agent SESSIONS already
                -- persist, but only the mention-gated lines that reached
                -- it plus its replies — never the channel. This is the
                -- channel: every line the connection saw, which is what
                -- a client has to redraw and what makes re-attaching to
                -- a bouncer session mean anything.
                --
                -- `seq` is the tap's in-process counter and resets on
                -- restart, so it is NOT the resume key; `id` is. It is
                -- kept only to correlate a stored row with a live event
                -- while both are in flight.
                CREATE TABLE IF NOT EXISTS irc_lines (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    network TEXT NOT NULL,
                    presence TEXT NOT NULL,    -- bot|admin
                    buffer TEXT NOT NULL,      -- #channel, a nick, or *
                    ts TEXT NOT NULL,          -- server-time when offered
                    kind TEXT NOT NULL,
                    nick TEXT NOT NULL DEFAULT '',
                    account TEXT,
                    text TEXT NOT NULL DEFAULT '',
                    own INTEGER NOT NULL DEFAULT 0,
                    seq INTEGER NOT NULL DEFAULT 0,
                    -- The adapter's authorization verdict for the
                    -- SPEAKER. The row is stored either way (a client
                    -- renders what the channel saw); this flag is what
                    -- keeps a banned voice out of a cold-start context
                    -- block, which would otherwise re-inject it after
                    -- a restart through the scrollback path.
                    blocked INTEGER NOT NULL DEFAULT 0
                );
                -- Reading is always "this buffer, before this id" —
                -- LIVE-TURN REGISTRY (R4.5). One row per session with a
                -- turn in flight, written by whichever PROCESS is
                -- running it (gateway, TUI, desktop, subagent worker),
                -- heartbeated while it streams and cleared on end. A
                -- reader reaps rows whose heartbeat went stale, so a
                -- crashed process cannot leave a session looking busy
                -- forever. This is the session_activity the harness
                -- audit lists as absent — liveness stops being
                -- process-local.
                CREATE TABLE IF NOT EXISTS session_runs (
                    session_id TEXT PRIMARY KEY,
                    pid INTEGER NOT NULL,
                    host TEXT NOT NULL,
                    surface TEXT NOT NULL,
                    started_at TEXT NOT NULL,
                    heartbeat_at TEXT NOT NULL
                );
                -- PER-SESSION OVERRIDES (R4.5). The desktop used to keep
                -- these as `override:<sid>` keys in desktop_state.json —
                -- invisible to every other surface, forgotten by any
                -- process that was not the desktop. One row here is one
                -- fact: /model set from the TUI, IRC, Telegram or the
                -- desktop is applied by whichever agent next opens the
                -- session.
                CREATE TABLE IF NOT EXISTS session_overrides (
                    session_id TEXT PRIMARY KEY,
                    provider TEXT,
                    model TEXT,
                    reasoning TEXT,
                    updated_at TEXT DEFAULT (datetime('now'))
                );
                -- PER-SESSION UI LANGUAGE (2026-09-25). `/locale <id>` on
                -- a gateway-served surface sets THIS session's language,
                -- not the gateway process's (which switched every user
                -- of every surface at once). Its own table, not a column
                -- on session_overrides: that row is FULL-REPLACED by the
                -- model/provider writer, and "Clear override" must not
                -- also forget the language.
                CREATE TABLE IF NOT EXISTS session_locale (
                    session_id TEXT PRIMARY KEY,
                    locale TEXT NOT NULL,
                    updated_at TEXT DEFAULT (datetime('now'))
                );
                -- A session's FROZEN PROMPT: the system prompt and the
                -- tool list exactly as its requests carry them — the
                -- byte prefix a provider's prompt cache is keyed on.
                -- Kept here (not only in the agent's memory) so a
                -- restart and a migration to another node send the same
                -- bytes, and the cache stays warm. `node` is the node
                -- that built it; `format` retires stored prompts when
                -- the builder's shape changes.
                -- A FAILOVER TAKEOVER (PLAN-pool-round.md C2): this
                -- node took `session_id` (a subtree root) over from
                -- `from_node`, which had stopped answering. Replicated,
                -- which is how the owner learns it on its return;
                -- `last_source_msg_id` is the owner's newest message the
                -- takeover's copy held (the owner's id space), so what the
                -- owner wrote after it is exactly its msg_ids above it.
                CREATE TABLE IF NOT EXISTS session_takeovers (
                    session_id TEXT PRIMARY KEY,
                    from_node TEXT NOT NULL,
                    to_node TEXT NOT NULL,
                    last_source_msg_id INTEGER NOT NULL,
                    authorized_by TEXT NOT NULL,
                    at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS session_prompt (
                    session_id TEXT PRIMARY KEY,
                    system_prompt TEXT NOT NULL,
                    tools_json TEXT NOT NULL DEFAULT '',
                    format INTEGER NOT NULL,
                    node TEXT,
                    updated_at TEXT DEFAULT (datetime('now'))
                );
                -- A session's own re-homed tools (`/tools home`,
                -- PLAN-pool-round.md D1): a tool glob -> the node that runs
                -- it. They win over `[tools.home]`.
                CREATE TABLE IF NOT EXISTS session_tool_homes (
                    session_id TEXT NOT NULL,
                    glob TEXT NOT NULL,
                    node TEXT NOT NULL,
                    set_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (session_id, glob)
                );
                -- THE MESSAGE QUEUE (R4.6). Text that arrives while the
                -- session's turn runs — from ANY surface — sits here
                -- until the turn boundary drains it. seq is a per-session
                -- counter (the handle editors and cancel use); state is
                -- 'pending'|'running'|'held', and 'running' rows are
                -- demoted to 'pending' by the drainer when no live turn
                -- owns them. 'held' rows came with a session from another
                -- node and wait for `/queue run` (the drainer never
                -- takes them).
                CREATE TABLE IF NOT EXISTS session_queue (
                    session_id TEXT NOT NULL,
                    seq INTEGER NOT NULL,
                    body TEXT NOT NULL,
                    state TEXT NOT NULL DEFAULT 'pending',
                    kind TEXT NOT NULL DEFAULT 'message',
                    -- WHERE this row's answer belongs: the platform-native
                    -- id of the message that parked it (an atproto post's
                    -- at-uri, a telegram message id) and the adapter
                    -- instance that carried it. A queued turn is a real
                    -- turn on a real message, and on a THREADED platform
                    -- the answer has to land under the message that asked
                    -- — not under whatever message happened to start the
                    -- turn whose boundary drained this row. NULL for a
                    -- mail or cron row, which parked behind no platform
                    -- message: then the reply threads under the session's
                    -- own chat.
                    reply_to TEXT,
                    route TEXT,
                    queued_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (session_id, seq)
                );
                CREATE INDEX IF NOT EXISTS idx_session_queue_state
                    ON session_queue(session_id, state, seq);
                -- SESSION MAIL (PLAN-sessions-and-phone.md S1): one row
                -- per message one session sent another. The TEXT rides
                -- the target's `session_queue` as a kind='mail' row (so
                -- every drainer runs it at its turn boundary); this row
                -- is the bookkeeping the queue cannot hold — who sent it,
                -- with which rights (`from_principal`, JSON; NULL = the
                -- owner surface), the loop guard (`chain_id`, `hop`), and
                -- the reply once the turn it started has ended.
                -- `body_sha` binds this row to the exact queued body, so
                -- a turn is a mail turn only when its text IS that body.
                -- Node-local, like approvals: never in REPLICATED_TABLES.
                CREATE TABLE IF NOT EXISTS session_mail (
                    id TEXT PRIMARY KEY,
                    to_session TEXT NOT NULL,
                    from_session TEXT,
                    from_title TEXT,
                    from_node TEXT,
                    from_pid INTEGER,
                    from_principal TEXT,
                    -- The pool peer it came through (crate::mailnet), as
                    -- this node names it; NULL for mail sent here.
                    via_peer TEXT,
                    chain_id TEXT NOT NULL,
                    hop INTEGER NOT NULL DEFAULT 0,
                    body_sha TEXT NOT NULL,
                    state TEXT NOT NULL DEFAULT 'queued',
                    waiting INTEGER NOT NULL DEFAULT 0,
                    reply TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                    -- 'mail', or 'join': a line the owner typed into a
                    -- session they joined, run with the owner's rights.
                    kind TEXT NOT NULL DEFAULT 'mail'
                );
                CREATE INDEX IF NOT EXISTS idx_session_mail_to
                    ON session_mail(to_session, state);
                CREATE INDEX IF NOT EXISTS idx_session_mail_chain
                    ON session_mail(chain_id, state);
                -- scrolling back through one channel — so the index
                -- leads with the buffer and ends on the id it pages by.
                CREATE INDEX IF NOT EXISTS idx_irc_lines_buffer
                    ON irc_lines(network, presence, buffer, id);
                -- …and "everything since id N", which is how the
                -- desktop resumes a dropped event stream.
                CREATE INDEX IF NOT EXISTS idx_irc_lines_id
                    ON irc_lines(id);
                -- Triggers (plan item 1, 2026-09-14): reactions to
                -- channel events that run without a turn. Modelled on
                -- cron_jobs, but the fire history is per (trigger,
                -- channel) and there is no scheduler — the IRC adapter
                -- feeds events, the gateway's timer tick feeds timers.
                CREATE TABLE IF NOT EXISTS triggers (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT UNIQUE NOT NULL,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    platform TEXT,               -- glob, usually 'irc'
                    network TEXT,                -- glob, veles' spelling
                    channel TEXT,                -- glob
                    event TEXT NOT NULL,         -- TEXT|JOIN|PART|QUIT|KICK|NICK
                    match_kind TEXT NOT NULL DEFAULT 'always',
                    pattern TEXT,
                    action TEXT NOT NULL,        -- slopdrop|vision|prompt|command
                    arg TEXT,                    -- vision question / prompt template / command line
                    cooldown_secs INTEGER NOT NULL DEFAULT 0,
                    per_hour_cap INTEGER NOT NULL DEFAULT 0,
                    min_role TEXT,
                    fire_count INTEGER NOT NULL DEFAULT 0,
                    last_fired_at TEXT,
                    last_status TEXT,
                    created_at TEXT DEFAULT (datetime('now')),
                    updated_at TEXT DEFAULT (datetime('now'))
                );
                CREATE TABLE IF NOT EXISTS trigger_fires (
                    fire_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    trigger_id INTEGER NOT NULL,
                    channel TEXT NOT NULL,
                    at TEXT DEFAULT (datetime('now')),
                    status TEXT NOT NULL,        -- fired|<refusal reason>
                    detail TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_trigger_fires
                    ON trigger_fires(trigger_id, fire_id);
                -- CLUSTER NODE REGISTRY (Plan B milestone B1, 2026-09-20).
                -- One row per compute node in the pool, keyed by the
                -- node's stable identity (node.toml `id`). A gateway
                -- upserts its own row at boot and heartbeats it; every
                -- surface reads the set back with `nodes_list`. The
                -- registry is deliberately just a table here, not a
                -- separate service: at pool size 2–5 the store the
                -- gateways already share (or will replicate, B5) is the
                -- natural home, and it means `veles nodes` works on a
                -- single-node install too.
                CREATE TABLE IF NOT EXISTS nodes (
                    id TEXT PRIMARY KEY,        -- stable node identity (node.toml)
                    name TEXT NOT NULL,          -- display name (defaults to hostname)
                    host TEXT NOT NULL,          -- gethostname at registration
                    version TEXT NOT NULL DEFAULT '',
                    uptime_class TEXT NOT NULL DEFAULT 'ephemeral', -- always-on|ephemeral
                    gpu TEXT,                    -- JSON probe result, NULL = none/not probed
                    endpoint TEXT,               -- how to reach this node (URL), if any
                    static_ip TEXT,              -- static WAN address (B1.5); see node.rs
                    static_ip_source TEXT,       -- 'asserted' ([node] static_ip); NULL = none
                    caps TEXT,                   -- JSON object of extra capabilities
                    observed_addr TEXT,          -- latest peer-observed address (B3a)
                    observed_at TEXT,            -- when it was seen, RFC 3339 UTC
                    observed_by TEXT,            -- which peer saw it
                    load1 INTEGER,               -- normalized 1-min load 0-100 (B4); NULL = unknown
                    load_at TEXT,                -- when load1 was sampled, RFC 3339 UTC
                    sessions_served INTEGER NOT NULL DEFAULT 0, -- lifetime sessions owned (B4)
                    registered_at TEXT DEFAULT (datetime('now')),
                    heartbeat_at TEXT DEFAULT (datetime('now'))
                );
                -- SINGLETON LEASES (B3a). The fail-closed arbiter's whole
                -- state: one row per singleton duty (cron scheduler,
                -- telegram poller, each IRC network, the admin presence).
                -- The lease lives in the ARBITER node's own store — there
                -- is no shared store — and every other node asks over the
                -- authenticated A2A link. `expires_at` is RFC 3339 UTC:
                -- an expired row is NOBODY's lease, so an arbiter that
                -- dies takes the pool to silence (fail-closed) rather
                -- than to two pollers.
                CREATE TABLE IF NOT EXISTS singleton_leases (
                    key TEXT PRIMARY KEY,        -- e.g. 'singleton:cron'
                    holder TEXT NOT NULL,        -- node id
                    granted_at TEXT NOT NULL,    -- RFC 3339 UTC
                    expires_at TEXT NOT NULL     -- RFC 3339 UTC
                );
                -- PEER-OBSERVED ADDRESSES (B3a). Every authenticated
                -- node-to-node call records the reflexive address the
                -- caller arrived from. History is append-only (the
                -- "stable across days" evidence for whether an uplink is
                -- static is a property of the SEQUENCE, not of the latest
                -- value); the denormalized nodes.observed_* columns carry
                -- the latest for display. Observations are LOCAL facts
                -- about how a peer reaches US — they are never gossiped.
                CREATE TABLE IF NOT EXISTS node_observations (
                    node TEXT NOT NULL,          -- the observed node's id
                    addr TEXT NOT NULL,          -- reflexive address (ip or ip:port)
                    by TEXT NOT NULL,            -- which peer's call we observed it on
                    at TEXT NOT NULL,            -- RFC 3339 UTC
                    PRIMARY KEY (node, at, addr)
                );
                -- CONFIG REPLICATION (B2). The append-only log of config
                -- writes: one row per replicated-key change, stamped by
                -- the node that made it. Every node appends its OWN rows
                -- and merges the union of everyone's — no master, the
                -- merge is deterministic (see configsync::merge_rows).
                -- `value = ''` is a TOMBSTONE (the key was removed), so
                -- a deletion outranks silence and deleted keys cannot
                -- resurrect from an old replica. Logical identity is
                -- (node, node_seq): importing the same offer twice
                -- inserts once.
                CREATE TABLE IF NOT EXISTS config_log (
                    seq INTEGER PRIMARY KEY AUTOINCREMENT, -- local arrival order
                    node TEXT NOT NULL,
                    node_seq INTEGER NOT NULL,
                    at TEXT NOT NULL,             -- RFC 3339 UTC, fixed-width
                    key TEXT NOT NULL,            -- dotted replicated key
                    value TEXT NOT NULL,          -- TOML value text; '' = tombstone
                    UNIQUE(node, node_seq)
                );
                CREATE INDEX IF NOT EXISTS idx_config_log_key
                    ON config_log(key, seq);
                -- The scanner's echo guard (B2): the replicated-key
                -- snapshot of config.toml as the scanner last saw it.
                -- Scans diff file-against-snapshot, and apply updates
                -- the snapshot to what it wrote — so a value imported
                -- from a peer scans as "unchanged" instead of bouncing
                -- back at the peer as a local write.
                CREATE TABLE IF NOT EXISTS config_scan (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                "#,
            )?;
            // Ledger TTL: `id:` keys (platform message ids — never repeat
            // within a chat) kept 7 days; `h:` content-hash keys kept only
            // 120s. Content-hash dedup exists to absorb reconnect-burst
            // redelivery on id-less platforms; any longer and it would
            // suppress a legitimately repeated identical message.
            // `cron:` slot keys kept 30 days (scheduler exactly-once).
            conn.execute_batch(
                r#"
                DELETE FROM inbound_ledger
                 WHERE (dedup_key LIKE 'id:%'
                        AND seen_at < datetime('now', '-7 days'))
                    OR (dedup_key LIKE 'h:%'
                        AND seen_at < datetime('now', '-120 seconds'))
                    OR (dedup_key LIKE 'cron:%'
                        AND seen_at < datetime('now', '-30 days'));
                "#,
            )?;
            // The trigger fire log had NO TTL while every other ledger
            // in this block had one. It takes a row per DECISION —
            // refusals included — so an always-match row on a live
            // channel writes one row per line, forever. 30 days is well
            // past "why didn't it react last week" and still bounded.
            conn.execute_batch(
                r#"
                DELETE FROM trigger_fires
                 WHERE at IS NOT NULL AND at < datetime('now', '-30 days');
                "#,
            )?;
            // M10 context compression: archive flag on messages. Archived
            // rows are NEVER deleted — they stay FTS-searchable — but
            // load_history skips them. Race-safe idempotent column add.
            add_column_if_missing(
                &conn,
                "messages",
                "archived",
                "ALTER TABLE messages ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;",
            )?;
            // Peer-observed address columns (B3a, same idempotent shape).
            // Fresh CREATE TABLEs carry them; pre-B3a DBs append them.
            // NULL = never observed, which reads correctly as "no
            // evidence" everywhere an observation is displayed. One call
            // per column: a racer's duplicate skips only THAT column, not
            // the rest of a shared batch.
            for (col, ddl) in [
                (
                    "observed_addr",
                    "ALTER TABLE nodes ADD COLUMN observed_addr TEXT;",
                ),
                (
                    "observed_at",
                    "ALTER TABLE nodes ADD COLUMN observed_at TEXT;",
                ),
                (
                    "observed_by",
                    "ALTER TABLE nodes ADD COLUMN observed_by TEXT;",
                ),
            ] {
                add_column_if_missing(&conn, "nodes", col, ddl)?;
            }
            // Sandbox flag on cron jobs (same idempotent shape). Fresh
            // CREATE TABLEs already carry it; pre-sandbox DBs need the
            // one column appended.
            add_column_if_missing(
                &conn,
                "cron_jobs",
                "sandbox",
                "ALTER TABLE cron_jobs ADD COLUMN sandbox INTEGER NOT NULL DEFAULT 0;",
            )?;
            // Session-wakeup jobs (Stage 5): an agent job can resume a
            // NAMED session instead of minting a fresh one. Same
            // idempotent ALTER shape as sandbox.
            add_column_if_missing(
                &conn,
                "cron_jobs",
                "session",
                "ALTER TABLE cron_jobs ADD COLUMN session TEXT;",
            )?;
            // Node-pinned jobs (2026-10-22, operator: the singleton lease
            // must stop DUPLICATES, not force every job onto whichever
            // node holds it). NULL = legacy floating job (fires on the
            // lease holder); a node id = that node fires it whenever it
            // is up, lease or no lease. The shared claim key
            // `cron:<id>:<slot>` still dedupes a slot across nodes.
            add_column_if_missing(
                &conn,
                "cron_jobs",
                "node",
                "ALTER TABLE cron_jobs ADD COLUMN node TEXT;",
            )?;
            // Kind on queued rows ('message' | 'steer'): parked steers
            // became queue rows (PENDING 2026-09-14 t5) so they survive
            // restarts and ride the session delete. Legacy rows are
            // messages — that is all the table held before.
            add_column_if_missing(
                &conn,
                "session_queue",
                "kind",
                "ALTER TABLE session_queue ADD COLUMN kind TEXT NOT NULL DEFAULT 'message';",
            )?;
            // Where a parked row's answer belongs (2026-10-16). Both
            // columns are NULLable: a row that parked before this
            // migration has no recorded target, and a mail or cron row
            // parked behind no platform message at all. Both fall back
            // to the session's own chat, which is what they did before.
            //
            // Without them the drained reply carried the turn-STARTING
            // message's id instead of its own, so on a threaded platform
            // a queued answer posted under an unrelated earlier post
            // (observed live: a Bluesky answer landed 20 minutes and two
            // posts up-thread from the question it answered), and one
            // re-injected through `parked_inbound` carried a fabricated
            // `queued:<nanos>-<seq>` id that is not an at-uri at all, so
            // atproto refused to post it (adapter.rs `deliver_reply`:
            // "a reply that cannot thread would land top-level on the
            // public feed") and the answer was silently dropped.
            add_column_if_missing(
                &conn,
                "session_queue",
                "reply_to",
                "ALTER TABLE session_queue ADD COLUMN reply_to TEXT;",
            )?;
            add_column_if_missing(
                &conn,
                "session_queue",
                "route",
                "ALTER TABLE session_queue ADD COLUMN route TEXT;",
            )?;
            // Authorization verdict on stored IRC speech (same
            // idempotent shape). Fresh CREATE TABLEs carry it; existing
            // scrollback reads as 0 = authorized, which is correct:
            // those lines were recorded before the verdict existed and
            // re-judging them by nothing would be a lie either way.
            add_column_if_missing(
                &conn,
                "irc_lines",
                "blocked",
                "ALTER TABLE irc_lines ADD COLUMN blocked INTEGER NOT NULL DEFAULT 0;",
            )?;
            // Session mail's peer column (S-b): a mail table made by
            // the S-a build has none.
            add_column_if_missing(
                &conn,
                "session_mail",
                "via_peer",
                "ALTER TABLE session_mail ADD COLUMN via_peer TEXT;",
            )?;
            // Its kind (the owner joining a session, 2026-10-13): every
            // older row was ordinary mail.
            add_column_if_missing(
                &conn,
                "session_mail",
                "kind",
                "ALTER TABLE session_mail ADD COLUMN kind TEXT NOT NULL DEFAULT 'mail';",
            )?;
            // Prompt-cache accounting columns (same idempotent shape).
            // Pre-cache DBs keep their history; the new columns read 0,
            // which means "none reported" — the totals only ever grow
            // from here.
            add_column_if_missing(
                &conn,
                "usage",
                "cached_tokens",
                "ALTER TABLE usage ADD COLUMN cached_tokens INTEGER NOT NULL DEFAULT 0;",
            )?;
            add_column_if_missing(
                &conn,
                "usage",
                "cache_write_tokens",
                "ALTER TABLE usage ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;",
            )?;
            // Nullable ON PURPOSE, and never backfilled: every row that
            // already exists was written by code that could not answer
            // the question, so NULL is the true value for all of them.
            // Defaulting them to 0 would assert "this provider reported
            // no cache", which is exactly the false 0% this column
            // exists to stop.
            add_column_if_missing(
                &conn,
                "usage",
                "cache_reported",
                "ALTER TABLE usage ADD COLUMN cache_reported INTEGER;",
            )?;
            // Reasoning tokens, split out of the completion. Nullable and
            // never backfilled: NULL is the true value for every row
            // written before the field was parsed, and 0 must keep
            // meaning "it split them, and there were none".
            add_column_if_missing(
                &conn,
                "usage",
                "reasoning_tokens",
                "ALTER TABLE usage ADD COLUMN reasoning_tokens INTEGER;",
            )?;
            // Injection class on messages (RM3-S4/A4): steer /
            // background / compaction / todo / hook / verification
            // markers, so the transcript can render them as labelled
            // blocks instead of as the user's own words. Nullable and
            // never backfilled — NULL is the true value for every row
            // written before the field existed.
            add_column_if_missing(
                &conn,
                "messages",
                "injection",
                "ALTER TABLE messages ADD COLUMN injection TEXT;",
            )?;
            // Spend accounting per CALLER, not per session (same
            // idempotent shape). A subject can hold many sessions —
            // one per chat — so "what has this person cost today"
            // could not be asked of a session-keyed table at all.
            // Nullable and never backfilled: NULL is the true value
            // for every call recorded before anyone was identified,
            // and a budget must not be primed with someone else's
            // history.
            add_column_if_missing(
                &conn,
                "usage",
                "subject",
                "ALTER TABLE usage ADD COLUMN subject TEXT;",
            )?;
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_usage_subject_time
                   ON usage(subject, created_at);",
            )?;
            // Which session this one was forked from (same idempotent
            // shape). A child session records its parent so the session
            // list can mark it and `/btw` can find the history to copy.
            // NULL for a session a human or a cron job started, which is
            // the true value for every row written before children were
            // first-class sessions.
            add_column_if_missing(
                &conn,
                "sessions",
                "parent",
                "ALTER TABLE sessions ADD COLUMN parent TEXT;",
            )?;
            // Session ownership (B1.5). Same idempotent shape: backfill
            // ONCE, when the column first appears. The backfill stamps
            // every pre-ownership row with the local node id — the
            // single-node-era rows were all written by the machine that
            // has this database, and at boot this IS that machine. A
            // node that skipped versions jumps the same fence once.
            // `added` is true only when THIS call added the column, so a
            // racer that lost the ALTER race does not re-run the backfill.
            let added = add_column_if_missing(
                &conn,
                "sessions",
                "node",
                "ALTER TABLE sessions ADD COLUMN node TEXT;",
            )?;
            if added {
                if let Some(owner) = local_node_id() {
                    conn.execute(
                        "UPDATE sessions SET node = ?1 WHERE node IS NULL",
                        params![owner],
                    )?;
                }
            }
            // The registry's egress address (B1.5) and where it came from.
            // B1.5 first added `static_ip` to the CREATE only — and
            // `CREATE TABLE IF NOT EXISTS` never alters a table B1 already
            // made, so every upgraded install failed registration and
            // `veles nodes list` on "no such column: static_ip".
            for (col, ddl) in [
                ("static_ip", "ALTER TABLE nodes ADD COLUMN static_ip TEXT;"),
                (
                    "static_ip_source",
                    "ALTER TABLE nodes ADD COLUMN static_ip_source TEXT;",
                ),
                // B4: placement metrics on the same migration pattern —
                // load (NULL = unknown, never mistaken for idle) and the
                // lifetime session counter.
                ("load1", "ALTER TABLE nodes ADD COLUMN load1 INTEGER;"),
                ("load_at", "ALTER TABLE nodes ADD COLUMN load_at TEXT;"),
                (
                    "sessions_served",
                    "ALTER TABLE nodes ADD COLUMN sessions_served INTEGER NOT NULL DEFAULT 0;",
                ),
            ] {
                add_column_if_missing(&conn, "nodes", col, ddl)?;
            }
        }

        Ok(Self { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }
    // ── cluster node registry (B1) ──────────────────────────────────

    /// Register or refresh this node's row at boot. `heartbeat_at` moves
    /// to now; `registered_at` is preserved from the first registration
    /// (INSERT OR IGNORE semantics via the UPSERT's excluded handling —
    /// a re-registering node keeps its birthday).
    #[allow(clippy::too_many_arguments)]
    pub fn node_register(
        &self,
        id: &str,
        name: &str,
        host: &str,
        version: &str,
        uptime_class: &str,
        gpu: Option<&str>,
        endpoint: Option<&str>,
        static_ip: Option<(&str, &str)>,
        caps: Option<&str>,
    ) -> Result<()> {
        // (address, source): the source travels with the address so a
        // reader can never see one without the other.
        let (static_ip, static_ip_source) = match static_ip {
            Some((addr, source)) => (Some(addr), Some(source)),
            None => (None, None),
        };
        self.conn.execute(
            "INSERT INTO nodes
                 (id, name, host, version, uptime_class, gpu, endpoint,
                  static_ip, static_ip_source, caps, registered_at, heartbeat_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                     datetime('now'), datetime('now'))
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 host = excluded.host,
                 version = excluded.version,
                 uptime_class = excluded.uptime_class,
                 gpu = excluded.gpu,
                 endpoint = excluded.endpoint,
                 static_ip = excluded.static_ip,
                 static_ip_source = excluded.static_ip_source,
                 caps = excluded.caps,
                 heartbeat_at = datetime('now')",
            params![
                id,
                name,
                host,
                version,
                uptime_class,
                gpu,
                endpoint,
                static_ip,
                static_ip_source,
                caps
            ],
        )?;
        Ok(())
    }

    /// Move only the heartbeat — the cheap liveness write a gateway
    /// makes on its tick, as opposed to the full re-register at boot.
    ///
    /// Returns the rows touched. 0 means the row is GONE — a `veles nodes
    /// forget` on this very node — and the caller must re-register: an
    /// UPDATE that matches nothing is not an error, so the first version
    /// heartbeated into the void until the next gateway restart.
    ///
    /// B4: the heartbeat also refreshes `load1` (the node's own load,
    /// normalized by CPU count; see `node::probe_load_one`). None keeps
    /// the previous sample — an unreadable load must not zero it, which
    /// would make `any` placement read the node as idle.
    pub fn node_heartbeat(&self, id: &str, load1: Option<u8>) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE nodes SET heartbeat_at = datetime('now'),
                    load1 = COALESCE(?2, load1),
                    load_at = CASE WHEN ?2 IS NULL THEN load_at
                                   ELSE datetime('now') END
             WHERE id = ?1",
            params![id, load1],
        )?)
    }

    /// One more session owned by this node (B4). Called on the session
    /// path that mints ownership; monotone, lifetime, never reset by a
    /// forget/re-register cycle (the row's history is the node's, not
    /// the registry's).
    pub fn node_served_bump(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE nodes SET sessions_served = sessions_served + 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// How many sessions a node owns, straight from the sessions table
    /// (`sessions.node`, B1.5) — the recomputable truth that
    /// `sessions_served` is the monotone shadow of. Used by placement
    /// as a tiebreak and by tests as the oracle.
    pub fn node_session_count(&self, id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE node = ?1",
            params![id],
            |r| r.get(0),
        )?)
    }

    /// Remove a node's row entirely (explicit decommission, not staleness).
    pub fn node_forget(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM nodes WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── singleton leases (B3a) ─────────────────────────────────────

    /// Grant or renew a singleton lease. Fails CLOSED when the key is
    /// held by someone else and unexpired: the caller gets `None` and
    /// must not run the singleton. A renewal by the SAME holder always
    /// succeeds; an expired row is nobody's lease and may be re-granted
    /// by anyone's next ask. Times are RFC 3339 UTC strings; `now` is
    /// passed in so tests can move the clock without sleeping.
    pub fn lease_grant(
        &self,
        key: &str,
        holder: &str,
        now: chrono::DateTime<chrono::Utc>,
        ttl_secs: i64,
    ) -> Result<Option<(String, String)>> {
        let existing = self.conn.query_row(
            "SELECT holder, expires_at FROM singleton_leases WHERE key = ?1",
            params![key],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        );
        let now_s = now.to_rfc3339();
        let expires = (now + chrono::Duration::seconds(ttl_secs)).to_rfc3339();
        match existing {
            Ok((held_by, exp_s)) => {
                let exp = chrono::DateTime::parse_from_rfc3339(&exp_s)
                    .map(|t| t.with_timezone(&chrono::Utc))
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
                if held_by != holder && exp > now {
                    // Someone else's unexpired lease: fail closed.
                    return Ok(None);
                }
                self.conn.execute(
                    "UPDATE singleton_leases SET holder = ?2, granted_at = ?3, expires_at = ?4 WHERE key = ?1",
                    params![key, holder, now_s, expires],
                )?;
                Ok(Some((now_s, expires)))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                self.conn.execute(
                    "INSERT INTO singleton_leases (key, holder, granted_at, expires_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![key, holder, now_s, expires],
                )?;
                Ok(Some((now_s, expires)))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Read a lease without touching it. `None` = not held (or expired:
    /// an expired row is nobody's lease and reads as unheld, the same
    /// rule `lease_grant` applies).
    pub fn lease_get(
        &self,
        key: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<(String, String, String)>> {
        let row = self.conn.query_row(
            "SELECT holder, granted_at, expires_at FROM singleton_leases WHERE key = ?1",
            params![key],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        );
        match row {
            Ok((holder, granted, exp_s)) => {
                let exp = chrono::DateTime::parse_from_rfc3339(&exp_s)
                    .map(|t| t.with_timezone(&chrono::Utc))
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
                if exp > now {
                    Ok(Some((holder, granted, exp_s)))
                } else {
                    Ok(None)
                }
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// A lease row whether or not it has expired: (holder, expiry).
    /// None = the key was never granted (or was released).
    pub fn lease_row(&self, key: &str) -> Result<Option<(String, chrono::DateTime<chrono::Utc>)>> {
        let row = self
            .conn
            .query_row(
                "SELECT holder, expires_at FROM singleton_leases WHERE key = ?1",
                params![key],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(row.map(|(h, exp)| {
            let exp = chrono::DateTime::parse_from_rfc3339(&exp)
                .map(|t| t.with_timezone(&chrono::Utc))
                .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
            (h, exp)
        }))
    }

    /// A session's `/tools home` rules, oldest first: (glob, node).
    pub fn tool_homes(&self, session_id: &str) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT glob, node FROM session_tool_homes WHERE session_id = ?1
             ORDER BY set_at, rowid",
        )?;
        let rows = st
            .query_map(params![session_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
        Ok(rows)
    }

    /// Set (or replace) one `/tools home` rule for a session.
    pub fn tool_home_set(&self, session_id: &str, glob: &str, node: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO session_tool_homes (session_id, glob, node)
             VALUES (?1, ?2, ?3)",
            params![session_id, glob, node],
        )?;
        Ok(())
    }

    /// Drop one `/tools home` rule. Whether there was one.
    pub fn tool_home_clear(&self, session_id: &str, glob: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM session_tool_homes WHERE session_id = ?1 AND glob = ?2",
            params![session_id, glob],
        )? > 0)
    }

    /// Release a lease, but only if this holder holds it — a late
    /// renewal-race loser must not release the winner's lease.
    pub fn lease_release(&self, key: &str, holder: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM singleton_leases WHERE key = ?1 AND holder = ?2",
            params![key, holder],
        )?;
        Ok(())
    }

    // ── peer-observed addresses (B3a) ──────────────────────────────

    /// Record that `node`'s call arrived from `addr` (the reflexive
    /// address this node saw). Appends to the history table and
    /// denormalizes the latest into `nodes.observed_*`. The caller
    /// authenticates the peer; the store just records the fact.
    pub fn node_observe(&self, node: &str, addr: &str, by: &str) -> Result<()> {
        let at = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR IGNORE INTO node_observations (node, addr, by, at)
             VALUES (?1, ?2, ?3, ?4)",
            params![node, addr, by, at],
        )?;
        self.conn.execute(
            "UPDATE nodes SET observed_addr = ?2, observed_at = ?3, observed_by = ?4
             WHERE id = ?1",
            params![node, addr, at, by],
        )?;
        Ok(())
    }

    /// Which node an authenticated peer speaks for: the node its latest
    /// gossip observation (`by = peer`) was ABOUT — never `exclude`,
    /// this node. The ECHO of our own gossip records OUR row as observed
    /// by that peer (where it saw us), which says nothing about who the
    /// peer is; B3a took the latest row observed by the peer, so with
    /// gossip running both ways the binding flipped between the peer's
    /// row and the arbiter's own, and half the peer's lease asks were
    /// refused as "bound to another node".
    pub fn node_bound_to_peer(&self, peer: &str, exclude: &str) -> Result<Option<String>> {
        match self.conn.query_row(
            "SELECT node FROM node_observations WHERE by = ?1 AND node != ?2
             ORDER BY at DESC LIMIT 1",
            params![peer, exclude],
            |r| r.get::<_, String>(0),
        ) {
            Ok(n) => Ok(Some(n)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// A node's observation history, oldest first — the evidence for
    /// "is this uplink static" (a sequence, not a snapshot).
    pub fn node_observation_history(
        &self,
        node: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT at, addr FROM node_observations WHERE node = ?1
             ORDER BY at DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![node, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().rev().collect())
    }

    // ── gossip merge (B3a) ─────────────────────────────────────────

    /// Fold a peer's registry rows into ours. The merge preserves what
    /// only WE can know: our own row's identity fields are never
    /// overwritten by gossip (a peer's stale copy of us must not rename
    /// us), `registered_at` keeps its birthday, and our LOCAL
    /// observation columns are never touched (observations are local
    /// facts). A peer's `heartbeat_at` for a node we also see is kept
    /// only when fresher (string compare on RFC 3339/SQLite text — both
    /// sides write fixed-width stamps for exactly this).
    pub fn node_merge_from_gossip(&self, row: &GossipNodeRow) -> Result<bool> {
        let local_id = local_node_id();
        if local_id.as_deref() == Some(row.id.as_str()) {
            // Our own row: only the heartbeat column may move, and only
            // forward. Everything else about us is authored here.
            self.conn.execute(
                "UPDATE nodes SET heartbeat_at = MAX(heartbeat_at, ?2) WHERE id = ?1",
                params![row.id, row.heartbeat_at],
            )?;
            return Ok(false);
        }
        let existing = self.conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE id = ?1",
            params![row.id],
            |r| r.get::<_, i64>(0),
        );
        match existing {
            Ok(0) => {
                // First time we meet this node: take its own birthday
                // claim (the same claim it registers with directly).
                self.conn.execute(
                    "INSERT INTO nodes (id, name, host, version, uptime_class, gpu,
                                        endpoint, static_ip, static_ip_source, caps,
                                        load1, load_at, sessions_served,
                                        registered_at, heartbeat_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                             ?11, ?12, ?13, ?14, ?15)",
                    params![
                        row.id,
                        row.name,
                        row.host,
                        row.version,
                        row.uptime_class,
                        row.gpu,
                        row.endpoint,
                        row.static_ip,
                        row.static_ip_source,
                        row.caps,
                        row.load1,
                        row.load_at,
                        row.sessions_served,
                        row.registered_at,
                        row.heartbeat_at
                    ],
                )?;
                Ok(true)
            }
            Ok(_) => {
                // Known node: refresh what it asserts about itself, keep
                // the birthday, never touch local observation columns —
                // and only from a copy at least as FRESH as ours. Rows
                // reach us relayed through third nodes, and B3a/B4
                // applied any copy: a peer's stale view reverted a
                // node's endpoint and load, and walked the "monotone"
                // sessions_served backwards. Last writer wins by the
                // node's own heartbeat, the one clock it authors.
                self.conn.execute(
                    "UPDATE nodes SET
                        name = ?2, host = ?3, version = ?4, uptime_class = ?5,
                        gpu = ?6, endpoint = ?7, static_ip = ?8,
                        static_ip_source = ?9, caps = ?10,
                        load1 = ?11, load_at = ?12, sessions_served = ?13,
                        heartbeat_at = ?14
                     WHERE id = ?1 AND heartbeat_at <= ?14",
                    params![
                        row.id,
                        row.name,
                        row.host,
                        row.version,
                        row.uptime_class,
                        row.gpu,
                        row.endpoint,
                        row.static_ip,
                        row.static_ip_source,
                        row.caps,
                        row.load1,
                        row.load_at,
                        row.sessions_served,
                        row.heartbeat_at
                    ],
                )?;
                Ok(true)
            }
            Err(e) => Err(e.into()),
        }
    }

    // ── config replication (B2) ──────────────────────────────────

    /// Append one config-log row. `false` = the row was already
    /// present — import is idempotent on `(node, node_seq)`, so the
    /// same offer applied twice (or an offer re-sent after a partial
    /// push) inserts once.
    pub fn config_log_append(&self, row: &crate::configsync::ConfigLogRow) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO config_log (node, node_seq, at, key, value)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row.node, row.node_seq, row.at, row.key, row.value],
        )?;
        Ok(n > 0)
    }

    /// Every log row in arrival order (the merge input is the union —
    /// order does not matter, `merge_rows` sorts).
    pub fn config_log_rows(&self) -> Result<Vec<crate::configsync::ConfigLogRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT node, node_seq, at, key, value FROM config_log ORDER BY seq")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::configsync::ConfigLogRow {
                    node: r.get(0)?,
                    node_seq: r.get(1)?,
                    at: r.get(2)?,
                    key: r.get(3)?,
                    value: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One node's rows past a cursor, in that node's own seq order —
    /// the offer payload for `configsync push`.
    pub fn config_log_rows_for(
        &self,
        node: &str,
        since_seq: i64,
    ) -> Result<Vec<crate::configsync::ConfigLogRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT node, node_seq, at, key, value FROM config_log
             WHERE node = ?1 AND seq > ?2 ORDER BY node_seq",
        )?;
        let rows = stmt
            .query_map(params![node, since_seq], |r| {
                Ok(crate::configsync::ConfigLogRow {
                    node: r.get(0)?,
                    node_seq: r.get(1)?,
                    at: r.get(2)?,
                    key: r.get(3)?,
                    value: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// A node's highest own-seq — where `scan_and_log` continues
    /// numbering from.
    pub fn config_log_max_node_seq(&self, node: &str) -> Result<i64> {
        let max: Option<i64> = self.conn.query_row(
            "SELECT MAX(node_seq) FROM config_log WHERE node = ?1",
            params![node],
            |r| r.get(0),
        )?;
        Ok(max.unwrap_or(0))
    }

    /// The highest local rowid — the natural push cursor, and what
    /// `veles configsync status` reports.
    pub fn config_log_max_seq(&self) -> Result<i64> {
        let max: Option<i64> = self
            .conn
            .query_row("SELECT MAX(seq) FROM config_log", [], |r| r.get(0))?;
        Ok(max.unwrap_or(0))
    }

    /// The scanner's snapshot of config.toml's replicated keys.
    pub fn config_scan_get(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM config_scan ORDER BY key")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Replace the whole snapshot (scan-and-log and apply both write
    /// what the file now says).
    pub fn config_scan_replace(
        &self,
        snap: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM config_scan", [])?;
        {
            let mut stmt = tx.prepare("INSERT INTO config_scan (key, value) VALUES (?1, ?2)")?;
            for (k, v) in snap {
                stmt.execute(params![k, v])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn node_row(r: &rusqlite::Row) -> rusqlite::Result<NodeRow> {
        Ok(NodeRow {
            id: r.get(0)?,
            name: r.get(1)?,
            host: r.get(2)?,
            version: r.get(3)?,
            uptime_class: r.get(4)?,
            gpu: r.get(5)?,
            endpoint: r.get(6)?,
            static_ip: r.get(7)?,
            static_ip_source: r.get(8)?,
            caps: r.get(9)?,
            observed_addr: r.get(10)?,
            observed_at: r.get(11)?,
            observed_by: r.get(12)?,
            load1: r.get(13)?,
            load_at: r.get(14)?,
            sessions_served: r.get(15)?,
            registered_at: r.get(16)?,
            heartbeat_at: r.get(17)?,
        })
    }

    const NODE_COLS: &'static str = "id, name, host, version, uptime_class, gpu, endpoint, static_ip, static_ip_source, caps, observed_addr, observed_at, observed_by, load1, load_at, sessions_served, registered_at, heartbeat_at";

    /// Every registered node, self first isn't knowable here — ordered
    /// by registration time (the pool's seniority order).
    pub fn nodes_list(&self) -> Result<Vec<NodeRow>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {} FROM nodes ORDER BY registered_at, id",
            Self::NODE_COLS
        ))?;
        let rows = st
            .query_map([], Self::node_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// One node by id (or unique id prefix — the same shorthand the
    /// session list accepts, for `veles nodes show <prefix>`). `None` for
    /// both "no such node" and "ambiguous"; [`Store::node_resolve`] tells
    /// them apart for a caller that wants to say which.
    pub fn node_get(&self, id_or_prefix: &str) -> Result<Option<NodeRow>> {
        Ok(match self.node_resolve(id_or_prefix)? {
            NodeMatch::One(row) => Some(*row),
            NodeMatch::None | NodeMatch::Ambiguous(_) => None,
        })
    }

    /// Resolve an id or unique prefix. The prefix is matched LITERALLY:
    /// `%` and `_` are LIKE wildcards, and the first version passed them
    /// through, so `forget %` — or `forget ""`, an empty prefix that
    /// matches everything — deleted the only node in a one-node registry
    /// without the caller naming it. An empty prefix now matches nothing.
    pub fn node_resolve(&self, id_or_prefix: &str) -> Result<NodeMatch> {
        let want = id_or_prefix.trim();
        if want.is_empty() {
            return Ok(NodeMatch::None);
        }
        if let Some(row) = self
            .conn
            .query_row(
                &format!("SELECT {} FROM nodes WHERE id = ?1", Self::NODE_COLS),
                params![want],
                Self::node_row,
            )
            .optional()?
        {
            return Ok(NodeMatch::One(Box::new(row)));
        }
        let escaped = want
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let like = format!("{escaped}%");
        let rows: Vec<NodeRow> = self
            .conn
            .prepare(&format!(
                "SELECT {} FROM nodes WHERE id LIKE ?1 ESCAPE '\\'",
                Self::NODE_COLS
            ))?
            .query_map(params![like], Self::node_row)?
            .collect::<Result<_, _>>()?;
        Ok(match rows.len() {
            0 => NodeMatch::None,
            1 => NodeMatch::One(Box::new(rows.into_iter().next().expect("one"))),
            n => NodeMatch::Ambiguous(n),
        })
    }

    // ── inbound exactly-once ledger ─────────────────────────────────

    /// Claim a dedup key atomically. Returns true when the key is fresh
    /// (caller processes the message), false when already claimed
    /// (duplicate — caller drops it).
    ///
    /// This is the consumer-side idempotency layer that any queue system
    /// (RabbitMQ, NATS, Redis Streams) still requires on top of its
    /// at-least-once delivery — implemented durably on the store we
    /// already have open. Unlike an in-memory window it survives gateway
    /// restarts, which is precisely when Telegram redelivers pending
    /// updates (crash/reconnect before the getUpdates offset advanced).
    pub fn claim_inbound(&self, dedup_key: &str) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO inbound_ledger (dedup_key) VALUES (?1)",
            params![dedup_key],
        )?;
        Ok(n > 0)
    }

    // ── sessions ────────────────────────────────────────────────────

    pub fn create_session(&self, id: &str) -> Result<()> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO sessions (id, node) VALUES (?1, ?2)",
            params![id, local_node_id()],
        )?;
        // B4: a row that actually landed is one more session this node
        // owns. The IGNORE half must not bump — `create_session` is
        // called for existing ids on every resume/boot-into, and the
        // counter is lifetime-owned, not lifetime-called.
        if n > 0 {
            if let Some(owner) = local_node_id() {
                let _ = self.node_served_bump(&owner);
            }
        }
        Ok(())
    }

    pub fn latest_session(&self) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id FROM sessions s
             WHERE EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.id)
             ORDER BY s.updated_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }

    pub fn list_sessions(&self, limit: u32) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, COALESCE(title, '') FROM sessions
             ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn touch_session(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET updated_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Which session this one was forked from, if any. A child session
    /// records its parent; a session a human or cron started has none.
    pub fn session_parent(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT parent FROM sessions WHERE id = ?1",
                params![session_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten())
    }

    /// The session's `node` stamp as stored: the owner's id, a B6
    /// migration state (`migrating_to:<target>` / `migrated_to:<t>@<epoch>`),
    /// or None (pre-ownership era, still local by construction). The
    /// migration fence reads this, not `session_exists`.
    pub fn session_node_stamp(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT node FROM sessions WHERE id = ?1",
                params![session_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten())
    }

    /// Create a session row that is a CHILD of `parent`: the fork link
    /// plus an optional title, in one INSERT. Idempotent on the id like
    /// [`create_session`]. Used by the subagent spawn paths (`/btw`,
    /// `delegate_task`) so a child is a real, attachable session from
    /// the moment it is minted rather than an id that only exists in
    /// the child process.
    pub fn create_child_session(&self, id: &str, parent: &str, title: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO sessions (id, title, parent, node) VALUES (?1, ?2, ?3, ?4)",
            params![id, title, parent, local_node_id()],
        )?;
        Ok(())
    }

    /// Copy every message of `from` into `to`, oldest first, and return
    /// how many rows landed. THIS is the fork primitive — `/branch`,
    /// `/fork` and the `/btw` child all mean it, and they must not each
    /// carry a copy of the loop (three hand-rolled copies of a
    /// `load_history` + `append_message` pair is how they drift).
    pub fn copy_history(&self, from: &str, to: &str) -> Result<usize> {
        let msgs = self.load_history(from)?;
        let n = msgs.len();
        for msg in &msgs {
            self.append_message(to, msg)?;
        }
        Ok(n)
    }

    // ── messages ────────────────────────────────────────────────────

    /// Append a message, returning the `msg_id` the row got.
    ///
    /// The id matters beyond bookkeeping: compaction's tail pruning
    /// REWRITES persisted rows (shrinking an old tool result to a stub),
    /// and it addresses them by id so it can only ever touch the row it
    /// actually read. A caller that keeps the message in memory should
    /// stamp this id onto it (`Agent::persist_stamped`) — otherwise a
    /// message appended mid-turn is unprunable until the next reload.
    pub fn append_message(&self, session_id: &str, msg: &ChatMessage) -> Result<i64> {
        let tool_calls = msg.tool_calls.as_ref().map(|v| v.to_string());
        self.conn.execute(
            "INSERT INTO messages
             (session_id, role, content, reasoning, tool_calls, tool_call_id, tool_name, injection)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id,
                msg.role,
                msg.content,
                msg.reasoning,
                tool_calls,
                msg.tool_meta.as_ref().map(|m| m.tool_call_id.clone()),
                msg.tool_meta.as_ref().map(|m| m.name.clone()),
                msg.injection,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.touch_session(session_id)?;
        Ok(id)
    }

    /// Import path: does this session already exist (idempotence guard)?
    pub fn session_message_count(&self, session_id: &str) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Import path: one whole session (row + messages) in ONE
    /// transaction — hermes has 30k+ messages and per-row autocommit
    /// would fsync each one. Idempotence is the caller's job
    /// (session_exists check before this call).
    pub fn import_session_batch(
        &self,
        id: &str,
        title: Option<&str>,
        created_at: &str,
        updated_at: &str,
        msgs: &[ImportMessageRow],
    ) -> Result<usize> {
        // (role, content, reasoning, tool_calls, tool_call_id, tool_name, created_at)
        let tx = self.conn.unchecked_transaction()?;
        // Ownership: an IMPORT runs on this machine, into this store —
        // the rows are written by the local node even when they were
        // authored elsewhere (hermes). Their ORIGIN is preserved in the
        // timestamps and message content, not in the ownership column,
        // which answers "whose database may write this session" — and
        // that is whoever's database they are IN.
        tx.execute(
            "INSERT OR IGNORE INTO sessions (id, title, node, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, title, local_node_id(), created_at, updated_at],
        )?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO messages
                 (session_id, role, content, reasoning, tool_calls, tool_call_id, tool_name, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (role, content, reasoning, tool_calls, tcid, tname, cat) in msgs {
                ins.execute(params![
                    id, role, content, reasoning, tool_calls, tcid, tname, cat
                ])?;
            }
        }
        tx.commit()?;
        Ok(msgs.len())
    }

    /// Import path: mark the hermes-archived rows (active=0, compaction
    /// lineage) archived=1 AFTER the batch landed. Flags are positional,
    /// matching the insert order (timestamp, id). Archived rows stay
    /// FTS-searchable but load_history skips them — hermes' durable
    /// lineage semantics, veles side.
    pub fn mark_imported_archived(&self, session_id: &str, flags: &[bool]) -> Result<usize> {
        // Grab the msg_ids we just inserted, in the same order.
        let mut stmt = self
            .conn
            .prepare("SELECT msg_id FROM messages WHERE session_id = ?1 ORDER BY msg_id")?;
        let ids: Vec<i64> = stmt
            .query_map(params![session_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            ids.len() == flags.len(),
            "imported row count {} != flag count {} for session {session_id}",
            ids.len(),
            flags.len()
        );
        let mut n = 0usize;
        let tx = self.conn.unchecked_transaction()?;
        for (mid, arch) in ids.iter().zip(flags.iter()) {
            if *arch {
                tx.execute(
                    "UPDATE messages SET archived = 1 WHERE msg_id = ?1",
                    params![mid],
                )?;
                n += 1;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// `/clear`: archive EVERY live row of a session. Returns how many.
    ///
    /// Not `archive_head(load_history().len())`: `load_history` repairs
    /// what it loads and hides orphaned tool results, so that count came
    /// up short by exactly the hidden rows, and the prefix it archived
    /// began with them — the NEWEST rows survived the clear. A session
    /// carrying that damage is precisely the one a user clears.
    pub fn archive_all_live(&self, session_id: &str) -> Result<u32> {
        let n = self.conn.execute(
            "UPDATE messages SET archived = 1
             WHERE session_id = ?1 AND COALESCE(archived, 0) = 0",
            params![session_id],
        )?;
        Ok(n as u32)
    }

    /// Compaction's archive: the head it summarized, by row id, plus
    /// every live row OLDER than the kept tail (`msg_id < below`).
    ///
    /// Why not `archive_head(cut)`: `cut` indexes the IN-MEMORY history,
    /// which is not the store's live set in row order. The load-time
    /// repair hides orphaned results, a muted injection is persisted but
    /// never enters memory, and a compaction marker is persisted AFTER
    /// the tail it precedes in memory. A count of store rows then
    /// archives the wrong prefix — it left a summarized head row live
    /// beside the summary that covers it.
    ///
    /// The id set says exactly what was summarized; the `below` bound
    /// sweeps what memory never held from the same era. `below` is the
    /// oldest id in the kept tail (or one past the newest head id when
    /// nothing is kept), so no kept row can fall inside it. One
    /// transaction: half an archive is a malformed request.
    pub fn archive_compacted_head(
        &self,
        session_id: &str,
        head_ids: &[i64],
        below: i64,
    ) -> Result<u32> {
        let tx = self.conn.unchecked_transaction()?;
        let mut n = tx.execute(
            "UPDATE messages SET archived = 1
             WHERE session_id = ?1 AND COALESCE(archived, 0) = 0 AND msg_id < ?2",
            params![session_id, below],
        )?;
        {
            let mut stmt = tx.prepare(
                "UPDATE messages SET archived = 1
                 WHERE session_id = ?1 AND msg_id = ?2 AND COALESCE(archived, 0) = 0",
            )?;
            for id in head_ids {
                n += stmt.execute(params![session_id, id])?;
            }
        }
        tx.commit()?;
        Ok(n as u32)
    }

    /// M10 context compression: mark the oldest `count` unarchived
    /// messages of a session as archived. Archived rows remain in the
    /// DB and stay FTS-searchable (hermes-style durable lineage); only
    /// load_history skips them. Returns how many rows were archived.
    ///
    /// A COUNT of store rows: only right when the caller's rows are the
    /// store's live rows in order. Compaction and `/clear` no longer use
    /// it for that reason (`archive_compacted_head`, `archive_all_live`).
    pub fn archive_head(&self, session_id: &str, count: usize) -> Result<u32> {
        let n = self.conn.execute(
            "UPDATE messages SET archived = 1
             WHERE msg_id IN (
                 SELECT msg_id FROM messages
                 WHERE session_id = ?1 AND COALESCE(archived, 0) = 0
                 ORDER BY msg_id LIMIT ?2
             )",
            params![session_id, count as i64],
        )?;
        Ok(n as u32)
    }

    /// M10 tail pruning: REPLACE the content of one persisted message
    /// with a shorter stub, addressed by its `msg_id`.
    ///
    /// The full text is NOT lost. `messages_fts` is a standalone FTS5
    /// table populated by an AFTER INSERT trigger only — there is no
    /// UPDATE trigger — so it keeps the ORIGINAL text while
    /// `messages.content` (what the prompt is built from) gets the stub.
    /// That is the difference between compaction that destroys context
    /// and compaction that moves it out of the window: session_search
    /// still finds every pruned output verbatim.
    ///
    /// Scoped to the session AND the row id: a stale id from another
    /// session must not be able to rewrite anything here. Returns the
    /// number of rows touched, which the caller checks — 0 means the
    /// row was already archived or deleted, and the in-memory prune
    /// must not be reported as persisted.
    pub fn prune_message_content(
        &self,
        session_id: &str,
        msg_id: i64,
        new_content: &str,
    ) -> Result<u32> {
        let n = self.conn.execute(
            "UPDATE messages SET content = ?3
             WHERE session_id = ?1 AND msg_id = ?2",
            params![session_id, msg_id, new_content],
        )?;
        Ok(n as u32)
    }

    /// Archive an explicit set of rows, by id.
    ///
    /// `archive_head` covers the oldest-N case; compaction's round
    /// dropping archives rows from the MIDDLE of the live history (whole
    /// assistant/tool rounds the summary now carries), which no prefix
    /// count can express. Same guarantee either way: `archived = 1` hides
    /// the row from `load_history` while leaving it in the store and in
    /// `messages_fts`, so nothing is deleted and session_search still
    /// finds it.
    ///
    /// Scoped to the session, so an id from elsewhere matches nothing.
    /// One transaction: a half-applied round would leave an assistant
    /// row declaring a tool call whose result was archived, which is
    /// exactly the malformed request this path exists to avoid.
    ///
    /// Returns the number of rows archived. The caller compares it
    /// against the request: a mismatch means some row was already
    /// archived or never existed, and the in-memory history must not
    /// claim a drop the store did not record.
    pub fn archive_messages_by_id(&self, session_id: &str, msg_ids: &[i64]) -> Result<u32> {
        if msg_ids.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0usize;
        {
            let mut stmt = tx.prepare(
                "UPDATE messages SET archived = 1
                 WHERE session_id = ?1 AND msg_id = ?2",
            )?;
            for id in msg_ids {
                n += stmt.execute(params![session_id, id])?;
            }
        }
        tx.commit()?;
        Ok(n as u32)
    }

    /// Load a session's history for resume, in message order — the set
    /// a TURN sends, so it is repaired (`repair_tool_round_pairing`):
    /// orphaned tool results are hidden and unanswered calls stripped.
    ///
    /// Each message carries its persisted `msg_id`, which is how
    /// compaction's tail pruning rewrites the SAME row it read (never a
    /// positional neighbour). Because the repair can hide rows, the
    /// result is NOT the store's live set row-for-row: never zip it
    /// against `message_ids`/`message_times`, and never turn its length
    /// into an `archive_head` count. Display paths read
    /// `load_history_since(id, 0)`, which is raw.
    pub fn load_history(&self, session_id: &str) -> Result<Vec<ChatMessage>> {
        let mut out = self.load_history_since(session_id, 0)?;
        repair_tool_round_pairing(&mut out);
        // Compaction persists its marker AFTER the plan/todo/hook rows it
        // precedes in memory (agent.rs persists marker → plan → todo →
        // hook in that order), so by msg_id the marker is last and a
        // reload showed the summary after the rows it introduces. Move
        // the compaction family (marker first, then the injections that
        // persisted after it) ahead of the tail it summarizes: stable
        // partition, index order inside each group, so the only change
        // is marker-family to the front and nothing else moves.
        let family = |m: &ChatMessage| -> bool {
            matches!(
                m.injection.as_deref(),
                Some("compaction") | Some("plan") | Some("todo") | Some("hook")
            )
        };
        if out.iter().any(&family) {
            let (front, tail): (Vec<_>, Vec<_>) =
                out.iter().enumerate().partition(|(_, m)| family(m));
            let mut front: Vec<ChatMessage> = front.into_iter().map(|(_, m)| m.clone()).collect();
            // The marker must open the run, or the plan/todo rows precede
            // the summary that introduces them — the exact reload shape
            // this fixes. Index order among the non-marker rows is kept
            // (it is the persist order).
            front.sort_by_key(|m| m.injection.as_deref() != Some("compaction"));
            out = front
                .into_iter()
                .chain(tail.into_iter().map(|(_, m)| m.clone()))
                .collect();
        }
        Ok(out)
    }

    /// `load_history`, resuming: only the rows a client has not seen.
    ///
    /// The desktop's session stream reopens with `?since=<last_msg_id>`
    /// on every reconnect, and a reconnect is exactly what a client that
    /// has fallen behind a fast stream does. `history_rows` used to serve
    /// that by loading the WHOLE session, rendering every message to
    /// JSON, and only THEN dropping everything at or below the cursor —
    /// a full session's work per reconnect, under the same store mutex
    /// the running turn needs. A lag storm paid for itself in gateway
    /// CPU, which is the half of "it melts the gateway or the frontend"
    /// that lives on this side.
    ///
    /// `since = 0` is every row (msg_id is AUTOINCREMENT, so it starts at
    /// 1), which is why `load_history` is this call plus the repair — one
    /// query and one row mapping, so the two can never drift.
    ///
    /// RAW, never repaired. Pairing can only be judged on the whole live
    /// set, and a window cuts rounds in half by design: repaired, a
    /// result whose call sat at or below the cursor looked orphaned and
    /// was dropped — and the follow tick then moved its cursor past it
    /// for good. The display consumers also zip this row-for-row against
    /// `message_ids_since`.
    pub fn load_history_since(&self, session_id: &str, since: i64) -> Result<Vec<ChatMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT msg_id, role, content, reasoning, tool_calls, tool_call_id, tool_name, injection
             FROM messages
             WHERE session_id = ?1 AND COALESCE(archived, 0) = 0 AND msg_id > ?2
             ORDER BY msg_id",
        )?;
        let rows = stmt.query_map(params![session_id, since], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (msg_id, role, content, reasoning, tool_calls, tool_call_id, tool_name, injection) =
                row?;
            let mut msg = ChatMessage {
                role,
                content,
                reasoning,
                display_reasoning: None,
                tool_calls: None,
                tool_meta: None,
                attachments: Vec::new(),
                injection: None,
                msg_id: Some(msg_id),
            };
            if let Some(tc) = tool_calls {
                msg.tool_calls = serde_json::from_str(&tc).ok();
            }
            msg.injection = injection;
            if let (Some(id), Some(name)) = (tool_call_id, tool_name) {
                msg.tool_meta = Some(ToolMeta {
                    tool_call_id: id,
                    name,
                });
            }
            out.push(msg);
        }
        Ok(out)
    }

    /// FTS5 keyword search across sessions (session_search parity seed).
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<(String, String, String)>> {
        // Wrap as a quoted phrase so punctuation (hyphens etc.) isn't
        // parsed as FTS5 operators — matches hermes session_search's
        // forgiving behavior for literal strings.
        let quoted = format!("\"{}\"", query.replace('"', "\"\""));
        let mut stmt = self.conn.prepare(
            "SELECT session_id, role, snippet(messages_fts, 1, '>>>', '<<<', '…', 12)
             FROM messages_fts WHERE messages_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![quoted, limit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ── tool usage (the `veles toolstats` regression watch) ─────────

    /// Total calls per tool name, most-used first, over the last
    /// `since_days` days (None = all time). Counts persisted tool rows
    /// only (role='tool' with a non-empty tool_name), which is what a
    /// model actually invoked — not assistant rows that merely mention a
    /// name. This is the facility that caught delegate_task at 8 calls
    /// and shell/monitor at 0 while terminal ran 25k times: a tool the
    /// model never reaches for shows up here as a flat zero, and a
    /// promotion like the awareness fix has to MOVE this number or it did
    /// nothing.
    pub fn tool_usage(&self, since_days: Option<i64>) -> Result<Vec<(String, i64)>> {
        let map = |r: &rusqlite::Row<'_>| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?));
        match since_days {
            Some(d) => {
                let mut stmt = self.conn.prepare(
                    "SELECT tool_name, COUNT(*) FROM messages
                     WHERE role='tool' AND tool_name IS NOT NULL AND tool_name!=''
                       AND created_at >= datetime('now', ?1)
                     GROUP BY tool_name ORDER BY COUNT(*) DESC, tool_name",
                )?;
                let rows = stmt.query_map(params![format!("-{d} days")], map)?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT tool_name, COUNT(*) FROM messages
                     WHERE role='tool' AND tool_name IS NOT NULL AND tool_name!=''
                     GROUP BY tool_name ORDER BY COUNT(*) DESC, tool_name",
                )?;
                let rows = stmt.query_map([], map)?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            }
        }
    }

    /// Per-day call counts for ONE tool, oldest first, over the last
    /// `since_days` (None = all time). Days with zero calls are ABSENT —
    /// that absence is the signal: a tool that stops appearing has gone
    /// dark. Returned as (YYYY-MM-DD, count).
    pub fn tool_usage_by_day(
        &self,
        tool: &str,
        since_days: Option<i64>,
    ) -> Result<Vec<(String, i64)>> {
        let map = |r: &rusqlite::Row<'_>| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?));
        match since_days {
            Some(d) => {
                let mut stmt = self.conn.prepare(
                    "SELECT date(created_at) d, COUNT(*) FROM messages
                     WHERE role='tool' AND tool_name = ?1 AND created_at >= datetime('now', ?2)
                     GROUP BY d ORDER BY d",
                )?;
                let rows = stmt.query_map(params![tool, format!("-{d} days")], map)?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT date(created_at) d, COUNT(*) FROM messages
                     WHERE role='tool' AND tool_name = ?1
                     GROUP BY d ORDER BY d",
                )?;
                let rows = stmt.query_map(params![tool], map)?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            }
        }
    }

    // ── titles ──────────────────────────────────────────────────────

    pub fn set_title(&self, session_id: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = ?2 WHERE id = ?1",
            params![session_id, title],
        )?;
        Ok(())
    }

    // ── meta kv (S16 failover overrides etc.) ────────────────────────

    pub fn meta_get(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .ok()
    }

    /// `value` from the one meta row whose key is `a ++ b`.
    ///
    /// Exists because meta keys are `prefix ++ name` everywhere, and a
    /// caller holding the two parts separately must be able to read the
    /// row without the parts meeting in a string it then has to own —
    /// see `plan::plan_for`.
    pub fn meta_get_pair(&self, a: &str, b: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1 || ?2",
                params![a, b],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Drop a meta row. The bool is "a row was actually there".
    ///
    /// It used to return `Result<()>`, and `DELETE` on a missing key is
    /// not an error, so every caller that wanted to report a removal had
    /// nothing to ask. Three of them answered with `.is_ok()` — which is
    /// true for a key that was never set — and so `/sandbox unassign`,
    /// `/sandbox undefine` and `/revoke` each printed a success line for
    /// a no-op. `/defaults unbind` was about to be the fourth.
    ///
    /// The rows-affected count is already in hand from `execute`; the
    /// only fix that reaches all four is to stop discarding it.
    pub fn meta_delete(&self, key: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM meta WHERE key = ?1", params![key])?
            > 0)
    }

    /// All keys with the given prefix, sorted (S9 pairing grants etc.).
    pub fn meta_scan_prefix(&self, prefix: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(mut stmt) = self
            .conn
            .prepare("SELECT key FROM meta WHERE key LIKE ?1 || '%' ORDER BY key")
        {
            if let Ok(rows) = stmt.query_map(params![prefix], |r| r.get::<_, String>(0)) {
                for r in rows.flatten() {
                    out.push(r);
                }
            }
        }
        out
    }

    pub fn title(&self, session_id: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT title FROM sessions WHERE id = ?1")?;
        let mut rows = stmt.query(params![session_id])?;
        Ok(match rows.next()? {
            Some(r) => r.get::<_, Option<String>>(0)?,
            None => None,
        })
    }

    /// True when the session row exists (title may still be NULL —
    /// untitled sessions are real). The desktop API uses this to reject
    /// chats against invented session ids instead of silently creating
    /// transcripts under them.
    pub fn session_exists(&self, session_id: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// First user message of a session, truncated — used as the /sessions
    /// preview when no explicit title is set.
    pub fn preview(&self, session_id: &str) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "SELECT substr(content, 1, 60) FROM messages
             WHERE session_id = ?1 AND role = 'user' ORDER BY msg_id LIMIT 1",
        )?;
        let mut rows = stmt.query(params![session_id])?;
        Ok(match rows.next()? {
            Some(r) => r.get(0)?,
            None => String::new(),
        })
    }

    // ── undo / rollback ─────────────────────────────────────────────

    /// Count user turns in a session (for /undo bounds).
    pub fn user_turn_count(&self, session_id: &str) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND role = 'user'",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// Return the msg_id of the N-th user message (1-based) — everything
    /// at and after that id gets dropped by truncate_after.
    fn msg_id_of_user_turn(&self, session_id: &str, turn: u32) -> Result<Option<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT msg_id FROM messages
             WHERE session_id = ?1 AND role = 'user' ORDER BY msg_id LIMIT 1 OFFSET ?2",
        )?;
        let mut rows = stmt.query(params![session_id, (turn - 1) as i64])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }

    /// Delete messages from a msg_id onward (undo = back up N user turns).
    /// Also drops the matching FTS rows so search stays consistent.
    pub fn truncate_from_msg_id(&self, session_id: &str, from_msg_id: i64) -> Result<u32> {
        self.conn.execute(
            "DELETE FROM messages_fts WHERE session_id = ?1 AND msg_id >= ?2",
            params![session_id, from_msg_id],
        )?;
        let n = self.conn.execute(
            "DELETE FROM messages WHERE session_id = ?1 AND msg_id >= ?2",
            params![session_id, from_msg_id],
        )?;
        Ok(n as u32)
    }

    /// Undo the last `n` user turns: returns how many messages were dropped.
    pub fn undo_turns(&self, session_id: &str, n: u32) -> Result<u32> {
        let total = self.user_turn_count(session_id)?;
        if total == 0 || n == 0 {
            return Ok(0);
        }
        let turn = total.saturating_sub(n - 1).max(1);
        let Some(cut) = self.msg_id_of_user_turn(session_id, turn)? else {
            return Ok(0);
        };
        self.truncate_from_msg_id(session_id, cut)
    }

    // ── usage tracking ──────────────────────────────────────────────

    /// Has this (session, provider, model) already been accounted for?
    ///
    /// The hermes usage importer needs it to be idempotent: re-running a
    /// partial migration must not double every token total, and there is
    /// no unique constraint on `usage` to lean on — veles appends a row
    /// per call by design.
    pub fn usage_exists(&self, session_id: &str, provider: &str, model: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM usage WHERE session_id = ?1 AND provider = ?2 AND model = ?3",
            params![session_id, provider, model],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Record that a skill was loaded. One row per `skill_view` call —
    /// deliberately not deduplicated, so "how often" stays answerable.
    pub fn record_skill_use(&self, name: &str, session_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO skill_uses (name, session_id) VALUES (?1, ?2)",
            params![name, session_id],
        )?;
        Ok(())
    }

    /// The most recent recorded call for a session.
    ///
    /// The session TOTAL averages the cold first call in forever: a
    /// session's lifetime hit rate can only ever climb slowly toward
    /// the truth, and a fresh session reports a rate that says more
    /// about its age than about whether caching works. Prompt caches
    /// are per-call, so the honest operational number — and the one
    /// that shows a regression the round it happens — is the last
    /// call's.
    pub fn last_call_tokens(&self, session_id: &str) -> Result<Option<CallTokens>> {
        let mut st = self.conn.prepare(
            "SELECT prompt_tokens, completion_tokens, cached_tokens, cache_write_tokens,
                    COALESCE(cache_reported, 0), reasoning_tokens
             FROM usage WHERE session_id = ?1 ORDER BY usage_id DESC LIMIT 1",
        )?;
        let mut rows = st.query(params![session_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(CallTokens {
                prompt_tokens: r.get::<_, i64>(0)? as u64,
                completion_tokens: r.get::<_, i64>(1)? as u64,
                cached_tokens: r.get::<_, i64>(2)? as u64,
                cache_write_tokens: r.get::<_, i64>(3)? as u64,
                cache_reported: r.get::<_, i64>(4)? != 0,
                reasoning_tokens: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
            })),
            None => Ok(None),
        }
    }

    /// (provider, model) of the most recent recorded call. The cache
    /// "last call" reading is a property of THAT route — switch the
    /// session to a different model and the old call's cache share is
    /// still the newest row until the new model makes its first call.
    /// Surfaces that show the last-call rate need this so they can say
    /// "that was the PREVIOUS model" instead of implying the new model
    /// just posted those numbers (USER: "cache stats don't follow model
    /// changes").
    pub fn last_call_route(&self, session_id: &str) -> Result<Option<(String, String)>> {
        let mut st = self.conn.prepare(
            "SELECT provider, model FROM usage
             WHERE session_id = ?1 ORDER BY usage_id DESC LIMIT 1",
        )?;
        let mut rows = st.query(params![session_id])?;
        match rows.next()? {
            Some(r) => Ok(Some((r.get::<_, String>(0)?, r.get::<_, String>(1)?))),
            None => Ok(None),
        }
    }

    /// Skills used in the most recent `sessions` DISTINCT sessions that
    /// touched any skill, most-recently-used first.
    ///
    /// Counting distinct *sessions* rather than a wall-clock window is
    /// what makes this stable for bursty use: a fortnight away from the
    /// machine should not empty the resident index.
    pub fn recent_skill_names(&self, sessions: u32) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT name FROM skill_uses
             WHERE session_id IN (
                 SELECT session_id FROM skill_uses
                 GROUP BY session_id ORDER BY MAX(id) DESC LIMIT ?1
             )
             GROUP BY name
             ORDER BY MAX(id) DESC",
        )?;
        let rows = st.query_map(params![sessions], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn record_usage(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        t: CallTokens,
    ) -> Result<()> {
        self.record_usage_for(session_id, None, provider, model, t)
    }

    /// `record_usage` plus the ACL subject the call was spent for.
    ///
    /// Separate entry point rather than a changed signature: most
    /// callers (migration, tests, owner surfaces) have no subject and
    /// should not have to say `None` to say so.
    pub fn record_usage_for(
        &self,
        session_id: &str,
        subject: Option<&str>,
        provider: &str,
        model: &str,
        t: CallTokens,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO usage (session_id, subject, provider, model, prompt_tokens,
                                completion_tokens, cached_tokens, cache_write_tokens,
                                cache_reported, reasoning_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id,
                subject,
                provider,
                model,
                t.prompt_tokens as i64,
                t.completion_tokens as i64,
                t.cached_tokens as i64,
                t.cache_write_tokens as i64,
                i64::from(t.cache_reported),
                t.reasoning_tokens.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    /// `meta` key prefix for a subject's turn-rate ledger.
    pub const TURN_RATE_PREFIX: &'static str = "turnrate:";

    /// …for a subject's MEDIA-generation ledger. A separate bucket from
    /// turns because `/gen` is answered in the gateway's command region,
    /// above both turn brakes — so a generation never consumed a turn
    /// and, until 2026-09-11, was bounded by nothing at all.
    pub const MEDIA_RATE_PREFIX: &'static str = "mediarate:";

    /// …for VIDEO specifically. Its own bucket because a clip is an
    /// order of magnitude dearer than a picture (~1200 credits) and
    /// `video-gen` starts at trusted, which the media ceiling leaves
    /// uncapped — one dial should not have to answer for both.
    pub const VIDEO_RATE_PREFIX: &'static str = "videorate:";

    /// …for `/tcl`. Same reasoning as media: answered above the brakes,
    /// and after the slopdrop bridge it is an HTTP call into an
    /// interpreter with a 30-second timeout.
    pub const EVAL_RATE_PREFIX: &'static str = "evalrate:";

    /// Claim one turn against a subject's rolling rate limit.
    ///
    /// `Ok(None)` = allowed and recorded. `Ok(Some(retry_after_secs))`
    /// = refused, with how long until the oldest claim ages out.
    ///
    /// Deliberately NOT a row in `usage`: a turn refused before any
    /// provider call produces no usage row, and that refusal is the
    /// whole point — the flood has to cost nothing.
    pub fn claim_turn(&self, subject: &str, limit: u64, window_secs: u64) -> Result<Option<u64>> {
        self.claim_rate(Self::TURN_RATE_PREFIX, subject, limit, window_secs)
    }

    /// Claim one unit against a subject's rolling rate limit in the
    /// ledger named by `prefix`.
    ///
    /// A TRUE rolling window, not an hourly bucket: the ledger holds
    /// the timestamps of the claims still inside the window and nothing
    /// else, so it is self-pruning, bounded by the limit itself, and
    /// cannot be gamed by waiting for a bucket boundary (which would
    /// admit 2x the limit in a moment).
    ///
    /// Parameterised on the prefix so turns, generations, clips and
    /// evaluations are four independent budgets sharing one
    /// implementation — three copies of a rolling window is three
    /// chances for one of them to be subtly wrong about the retry hint.
    pub fn claim_rate(
        &self,
        prefix: &str,
        subject: &str,
        limit: u64,
        window_secs: u64,
    ) -> Result<Option<u64>> {
        if limit == 0 {
            return Ok(None); // unlimited
        }
        let key = format!("{prefix}{subject}");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff = now.saturating_sub(window_secs);
        let mut stamps: Vec<u64> = self
            .meta_get(&key)
            .and_then(|raw| serde_json::from_str::<Vec<u64>>(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|t| *t > cutoff)
            .collect();
        if stamps.len() as u64 >= limit {
            stamps.sort_unstable();
            // The oldest claim inside the window decides when a slot
            // frees up; +1 so "retry in 0s" is never the answer.
            let oldest = stamps.first().copied().unwrap_or(now);
            let retry = (oldest + window_secs).saturating_sub(now).max(1);
            // Persist the pruned list even on refusal: the stale
            // entries are gone either way, and not writing them back
            // would re-scan them on every refused attempt.
            self.meta_set(&key, &serde_json::to_string(&stamps).unwrap_or_default())?;
            return Ok(Some(retry));
        }
        stamps.push(now);
        self.meta_set(&key, &serde_json::to_string(&stamps).unwrap_or_default())?;
        Ok(None)
    }

    /// How many turns a subject has claimed inside `window_secs`, for
    /// the surfaces that show a budget without consuming one.
    pub fn turns_claimed(&self, subject: &str, window_secs: u64) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff = now.saturating_sub(window_secs);
        self.meta_get(&format!("{}{subject}", Self::TURN_RATE_PREFIX))
            .and_then(|raw| serde_json::from_str::<Vec<u64>>(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|t| *t > cutoff)
            .count() as u64
    }

    /// Tokens one subject has spent in the last `window`.
    ///
    /// The BILL, not the wire count. `prompt_tokens` counts a cached
    /// prefix at full price, but the provider charges a cache READ at a
    /// fraction of list (≈1/3 to ≈1/10 depending on the route). Rather
    /// than track each route's exact rate we bill a cached token at a
    /// flat 1/6 (`CACHE_READ_DIVISOR`) and do not pretend to be exact —
    /// the point is that a warm prefix is cheap, not free, so a budget
    /// that ignored it would be gameable by anyone who kept one long
    /// prefix warm. Uncached prompt (`prompt - cached`, floored at 0 in
    /// case a route double-counts) and completion bill at full price.
    /// `created_at` is SQLite's `datetime('now')`, i.e. UTC, so the
    /// comparison is done in SQL rather than by formatting a timestamp
    /// here.
    pub fn tokens_spent_since(&self, subject: &str, window_secs: u64) -> Result<u64> {
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(
                        MAX(prompt_tokens - cached_tokens, 0)
                        + cached_tokens / ?3
                        + completion_tokens), 0)
               FROM usage
              WHERE subject = ?1
                AND created_at > datetime('now', ?2)",
            params![
                subject,
                format!("-{window_secs} seconds"),
                CACHE_READ_DIVISOR as i64
            ],
            |r| r.get(0),
        )?;
        Ok(total.max(0) as u64)
    }

    /// The raw spend components behind `tokens_spent_since`, for the
    /// `/usage` surfaces that show a caller WHERE the budget went rather
    /// than one discounted number. The discounted total is derivable
    /// from these (`uncached + cached/DIVISOR + completion`), so the two
    /// can never disagree.
    pub fn spend_parts_since(&self, subject: &str, window_secs: u64) -> Result<SpendParts> {
        let (prompt, cached, completion, calls): (i64, i64, i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(prompt_tokens),0),
                    COALESCE(SUM(cached_tokens),0),
                    COALESCE(SUM(completion_tokens),0),
                    COUNT(*)
               FROM usage
              WHERE subject = ?1
                AND created_at > datetime('now', ?2)",
            params![subject, format!("-{window_secs} seconds")],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        Ok(SpendParts {
            prompt_tokens: prompt as u64,
            cached_tokens: cached as u64,
            completion_tokens: completion as u64,
            calls: calls as u64,
        })
    }

    /// How many claims a subject has inside `window_secs` in the ledger
    /// named by `prefix`, WITHOUT consuming one — the read side of
    /// `claim_rate`, for the `/usage` surfaces. `turns_claimed` is this
    /// pinned to the turn ledger; this is the same read for the media,
    /// video and eval ledgers, which had no read side at all.
    pub fn rate_claimed(&self, prefix: &str, subject: &str, window_secs: u64) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff = now.saturating_sub(window_secs);
        self.meta_get(&format!("{prefix}{subject}"))
            .and_then(|raw| serde_json::from_str::<Vec<u64>>(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|t| *t > cutoff)
            .count() as u64
    }

    /// The most recent reported prompt-token count for a session —
    /// GROUND TRUTH for how full the context window actually was on the
    /// last request. Compaction consults this before falling back to the
    /// char estimate: a provider that reports usage makes the estimate
    /// unnecessary, and one that lies about window sizes is caught out.
    pub fn last_prompt_tokens(&self, session_id: &str) -> Result<Option<u64>> {
        let mut stmt = self.conn.prepare(
            "SELECT prompt_tokens FROM usage
             WHERE session_id = ?1 AND prompt_tokens > 0
             ORDER BY rowid DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![session_id])?;
        Ok(rows.next()?.map(|r| r.get::<_, i64>(0).unwrap_or(0) as u64))
    }

    /// Is the reported prompt-token count FRESH, i.e. measured against a
    /// prompt that is still the shape of the current one?
    ///
    /// `last_prompt_tokens` is ground truth for how full the window was
    /// on the LAST request — but a compaction rewrites history between
    /// requests, and nothing writes a usage row for it (the summarizer
    /// is a side call that deliberately records no usage). So right after
    /// a compaction the newest usage row still describes the PRE-compaction
    /// prompt: ~450k tokens when the history is now 66k.
    ///
    /// Trusting that stale number is what made compaction stack markers:
    /// every turn-start check saw "450k ≥ bar" and compacted again, each
    /// time doing almost nothing. Session 20260913_142929_3dfd8c carried
    /// FOUR markers for that reason.
    ///
    /// The fix is an ordering question, not a new number: if the newest
    /// compaction marker for this session is NEWER than the newest usage
    /// row, the anchor is stale and the caller must fall back to the char
    /// estimate until the next real turn refreshes it.
    pub fn prompt_tokens_anchor_is_fresh(&self, session_id: &str) -> Result<bool> {
        // The newest compaction marker, if any. `injection='compaction'`
        // is set by compact_impl when it writes the marker.
        let marker: Option<String> = self
            .conn
            .query_row(
                "SELECT created_at FROM messages
                 WHERE session_id = ?1 AND injection = 'compaction'
                 ORDER BY msg_id DESC LIMIT 1",
                params![session_id],
                |r| r.get::<_, Option<String>>(0),
            )
            // `created_at` has a DEFAULT so it is non-null in practice;
            // the flatten is belt-and-braces for a hand-edited row, and
            // a NULL timestamp is treated as "no marker" rather than as
            // fresh.
            .optional()?
            .flatten();
        let Some(marker_at) = marker else {
            // No compaction has ever run here: the anchor cannot be stale.
            return Ok(true);
        };
        let usage_at: Option<String> = self
            .conn
            .query_row(
                "SELECT created_at FROM usage
                 WHERE session_id = ?1 AND prompt_tokens > 0
                 ORDER BY rowid DESC LIMIT 1",
                params![session_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(usage_at) = usage_at else {
            // A marker exists but no usage was ever recorded — there is
            // no reported number to trust either way.
            return Ok(false);
        };
        // Both are SQLite `datetime('now')` strings in the same format,
        // so lexicographic order IS chronological order. Equal timestamps
        // (same second) count as STALE: the marker may have been written
        // after the usage row within that second, and guessing fresh
        // would reproduce the exact bug this guard exists to kill.
        Ok(usage_at > marker_at)
    }

    /// Totals, optionally filtered to one session.
    pub fn usage_totals(&self, session_id: Option<&str>) -> Result<UsageTotals> {
        let (calls, prompt, completion, cached, cache_write, reasoning): (
            i64,
            i64,
            i64,
            i64,
            i64,
            Option<i64>,
        ) = match session_id {
            Some(sid) => self.conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(prompt_tokens),0),
                        COALESCE(SUM(completion_tokens),0),
                        COALESCE(SUM(cached_tokens),0),
                        COALESCE(SUM(cache_write_tokens),0),
                        SUM(reasoning_tokens)
                 FROM usage WHERE session_id = ?1",
                params![sid],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )?,
            None => self.conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(prompt_tokens),0),
                        COALESCE(SUM(completion_tokens),0),
                        COALESCE(SUM(cached_tokens),0),
                        COALESCE(SUM(cache_write_tokens),0),
                        SUM(reasoning_tokens)
                 FROM usage",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )?,
        };
        Ok(UsageTotals {
            calls: calls as u64,
            prompt_tokens: prompt as u64,
            completion_tokens: completion as u64,
            cached_tokens: cached as u64,
            cache_write_tokens: cache_write as u64,
            // SUM() over a nullable column is NULL only when every row
            // is NULL — i.e. nothing reported the field. Any reported
            // value (even 0) makes the sum a number, so None keeps
            // meaning "no route split reasoning".
            reasoning_tokens: reasoning.map(|v| v as u64),
        })
    }

    /// Per-provider breakdown.
    /// Per provider/model: the lifetime totals, plus the same counts
    /// over the most recent SESSION that used it.
    ///
    /// The second window exists because a lifetime cache share is not a
    /// reading of anything. This table spans code changes: calls
    /// recorded before cache accounting existed carry `cached_tokens =
    /// 0` because nothing parsed the field — indistinguishable in the
    /// table from a real miss, and dragging the lifetime share down
    /// forever. A row here read 57% while its most recent session ran
    /// at 86.5%, which is the same "an average over the wrong window
    /// looks like a defect" mistake as the session-cumulative hit rate,
    /// one level further out.
    ///
    /// Lifetime is what you were billed. Last session is how it works.
    pub fn usage_by_provider(&self) -> Result<Vec<ProviderUsageRow>> {
        let mut stmt = self.conn.prepare(
            "WITH latest AS (
                 SELECT provider, model, session_id FROM (
                     SELECT provider, model, session_id,
                            ROW_NUMBER() OVER (
                                PARTITION BY provider, model ORDER BY usage_id DESC
                            ) AS rn
                     FROM usage
                 ) WHERE rn = 1
             ),
             recent AS (
                 -- Only calls where the provider actually reported a
                 -- cache field. A call it never answered for must not
                 -- land in the denominator as a miss.
                 SELECT u.provider AS p, u.model AS m,
                        COALESCE(SUM(u.prompt_tokens),0) AS rp,
                        COALESCE(SUM(u.cached_tokens),0) AS rc,
                        COUNT(*) AS rn2
                 FROM usage u JOIN latest l
                   ON u.provider = l.provider AND u.model = l.model
                  AND u.session_id = l.session_id
                 WHERE COALESCE(u.cache_reported, 0) = 1
                 GROUP BY u.provider, u.model
             )
             SELECT u.provider, u.model, COUNT(*),
                    COALESCE(SUM(u.prompt_tokens),0), COALESCE(SUM(u.completion_tokens),0),
                    COALESCE(SUM(u.cached_tokens),0), COALESCE(SUM(u.cache_write_tokens),0),
                    COALESCE(MAX(r.rp),0), COALESCE(MAX(r.rc),0), COALESCE(MAX(r.rn2),0)
             FROM usage u LEFT JOIN recent r
               ON u.provider = r.p AND u.model = r.m
             GROUP BY u.provider, u.model ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ProviderUsageRow {
                provider: r.get::<_, String>(0)?,
                model: r.get::<_, String>(1)?,
                calls: r.get::<_, i64>(2)? as u64,
                prompt_tokens: r.get::<_, i64>(3)? as u64,
                completion_tokens: r.get::<_, i64>(4)? as u64,
                cached_tokens: r.get::<_, i64>(5)? as u64,
                cache_write_tokens: r.get::<_, i64>(6)? as u64,
                last_session_prompt_tokens: r.get::<_, i64>(7)? as u64,
                last_session_cached_tokens: r.get::<_, i64>(8)? as u64,
                last_session_calls: r.get::<_, i64>(9)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Per-provider token totals — the raw material for the per-plan
    /// split.
    ///
    /// Deliberately NOT `usage_by_provider`, which answers a different
    /// question ("how is the prefix cache working", with last-session
    /// rates and `cache_reported` filtering) and is global-only. This is
    /// a plain SUM, optionally scoped to one session, because the
    /// question it serves is "which plan did these tokens come out of".
    ///
    /// The plan mapping itself lives in `crate::tokenplan` — this layer
    /// only knows providers.
    pub fn usage_by_provider_scoped(
        &self,
        session_id: Option<&str>,
    ) -> Result<Vec<ProviderLedgerRow>> {
        let sql = |where_clause: &str| {
            format!(
                "SELECT provider, COUNT(*),
                        COALESCE(SUM(prompt_tokens),0),
                        COALESCE(SUM(completion_tokens),0),
                        COALESCE(SUM(cached_tokens),0)
                 FROM usage {where_clause}
                 GROUP BY provider ORDER BY COUNT(*) DESC"
            )
        };
        let read = |r: &rusqlite::Row<'_>| {
            Ok(ProviderLedgerRow {
                provider: r.get::<_, String>(0)?,
                calls: r.get::<_, i64>(1)? as u64,
                prompt_tokens: r.get::<_, i64>(2)? as u64,
                completion_tokens: r.get::<_, i64>(3)? as u64,
                cached_tokens: r.get::<_, i64>(4)? as u64,
            })
        };
        let rows = match session_id {
            Some(sid) => {
                let mut stmt = self.conn.prepare(&sql("WHERE session_id = ?1"))?;
                let it = stmt.query_map(params![sid], read)?;
                it.collect::<std::result::Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = self.conn.prepare(&sql(""))?;
                let it = stmt.query_map([], read)?;
                it.collect::<std::result::Result<Vec<_>, _>>()?
            }
        };
        Ok(rows)
    }

    /// Drop a session and all its data (/quit --delete parity).
    /// Delete a session and everything scoped to it.
    ///
    /// There are no foreign keys in this schema, so nothing cascades —
    /// every session-scoped table has to be listed here by hand. This
    /// used to clean only messages/messages_fts/usage, stranding todos
    /// and checkpoints under a session id that no longer existed.
    /// Orphaned checkpoints are the expensive ones: each references a
    /// blob file on disk, and `checkpoints::prune` only ever revisits
    /// them by age, so the files accumulated indefinitely.
    ///
    /// `approvals` is deliberately NOT deleted: it is an audit trail,
    /// and the point of an audit trail is that it outlives the thing it
    /// describes.
    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        self.delete_sessions(&[session_id.to_string()]).map(|_| ())
    }

    /// Delete sessions and everything that is theirs, in one
    /// transaction. Returns how many session rows went.
    ///
    /// `messages_fts.session_id` is UNINDEXED, so each delete of it is a
    /// scan of the whole index — about a second on a 1.5 GB store, under
    /// the gateway's store lock. One `IN` over the batch is one scan,
    /// however many sessions (a batch run left 1,770 to clear,
    /// 2026-10-04). The per-session rows that only mean something with
    /// their session go too; usage, skill statistics, cron runs and the
    /// pool's takeover history are records and stay.
    pub fn delete_sessions(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        // Blob files first — once the rows are gone the paths are
        // unrecoverable. Mirrors checkpoints::drop_one.
        for id in ids {
            for cp in self.checkpoint_list(Some(id), u32::MAX)? {
                if let Some(blob) = &cp.blob {
                    let _ = std::fs::remove_file(blob);
                }
            }
        }
        let set = serde_json::to_string(ids)?;
        let tx = self.conn.unchecked_transaction()?;
        for table in [
            "messages_fts",
            "messages",
            "todos",
            "session_queue",
            "checkpoints",
            "session_overrides",
            "session_locale",
            "session_prompt",
            "session_tool_homes",
            "engine_notes",
        ] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE session_id IN (SELECT value FROM json_each(?1))"
                ),
                params![set],
            )?;
        }
        let n = tx.execute(
            "DELETE FROM sessions WHERE id IN (SELECT value FROM json_each(?1))",
            params![set],
        )?;
        tx.commit()?;
        Ok(n)
    }

    // ── cron jobs ───────────────────────────────────────────────────

    /// Insert a new job. Fails if the id already exists.
    /// Explicit `created_at` / `updated_at` / `last_run_at` on the struct
    /// are honored when non-empty (used by tests/migrations); otherwise
    /// the sqlite defaults (now) apply.
    pub fn cron_upsert(&self, job: &CronJob) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cron_jobs
             (id, name, schedule, prompt, deliver, enabled, script, skills,
              no_agent, sandbox, model, provider, reasoning, origin, session, repeat_max,
              created_at, updated_at, last_run_at, node)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     COALESCE(NULLIF(?17, ''), datetime('now')),
                     COALESCE(NULLIF(?18, ''), datetime('now')),
                     ?19, ?20)",
            params![
                job.id,
                job.name,
                job.schedule,
                job.prompt,
                job.deliver,
                job.enabled,
                job.script,
                job.skills,
                job.no_agent,
                job.sandbox,
                job.model,
                job.provider,
                job.reasoning,
                job.origin,
                job.session,
                job.repeat_max,
                job.created_at,
                job.updated_at,
                job.last_run_at,
                job.node,
            ],
        )?;
        Ok(())
    }

    pub fn cron_get(&self, id: &str) -> Result<Option<CronJob>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{} WHERE id = ?1", cron_select()))?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(r) => Ok(Some(cron_job_from_row(r)?)),
            None => Ok(None),
        }
    }

    pub fn cron_list(&self, enabled_only: bool) -> Result<Vec<CronJob>> {
        let sql = if enabled_only {
            format!("{} WHERE enabled = 1 ORDER BY created_at", cron_select())
        } else {
            format!("{} ORDER BY created_at", cron_select())
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], cron_job_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Partial update — every field Option-wrapped; None keeps the old
    /// value. Returns the updated job, or None when the id is unknown.
    #[allow(clippy::too_many_arguments)]
    pub fn cron_update(
        &self,
        id: &str,
        name: Option<&str>,
        schedule: Option<&str>,
        prompt: Option<&str>,
        deliver: Option<&str>,
        script: Option<Option<&str>>,
        skills: Option<Option<&str>>,
        model: Option<Option<&str>>,
        provider: Option<Option<&str>>,
        reasoning: Option<Option<&str>>,
        session: Option<Option<&str>>,
        repeat_max: Option<Option<i64>>,
        node: Option<Option<&str>>,
    ) -> Result<Option<CronJob>> {
        let Some(mut job) = self.cron_get(id)? else {
            return Ok(None);
        };
        if let Some(v) = name {
            job.name = Some(v.to_string());
        }
        if let Some(v) = schedule {
            job.schedule = v.to_string();
        }
        if let Some(v) = prompt {
            job.prompt = v.to_string();
        }
        if let Some(v) = deliver {
            job.deliver = v.to_string();
        }
        if let Some(v) = script {
            job.script = v.map(String::from);
        }
        if let Some(v) = skills {
            job.skills = v.map(String::from);
        }
        if let Some(v) = model {
            job.model = v.map(String::from);
        }
        if let Some(v) = provider {
            job.provider = v.map(String::from);
        }
        if let Some(v) = reasoning {
            job.reasoning = v.map(String::from);
        }
        if let Some(v) = session {
            job.session = v.map(String::from);
        }
        if let Some(v) = repeat_max {
            job.repeat_max = v;
        }
        if let Some(v) = node {
            job.node = v.map(String::from);
        }
        self.conn.execute(
            "UPDATE cron_jobs SET name=?2, schedule=?3, prompt=?4, deliver=?5,
             script=?6, skills=?7, model=?8, provider=?9, reasoning=?10,
             session=?11, repeat_max=?12, node=?13, updated_at=datetime('now') WHERE id=?1",
            params![
                job.id,
                job.name,
                job.schedule,
                job.prompt,
                job.deliver,
                job.script,
                job.skills,
                job.model,
                job.provider,
                job.reasoning,
                job.session,
                job.repeat_max,
                job.node,
            ],
        )?;
        Ok(Some(job))
    }

    pub fn cron_set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE cron_jobs SET enabled=?2, updated_at=datetime('now') WHERE id=?1",
            params![id, enabled],
        )?;
        Ok(n > 0)
    }

    pub fn cron_delete(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM cron_jobs WHERE id=?1", params![id])?;
        self.conn
            .execute("DELETE FROM cron_runs WHERE job_id=?1", params![id])?;
        Ok(n > 0)
    }

    /// Bump run bookkeeping after a tick. `next_run_at` is RFC3339 UTC or
    /// None when the job is finished (one-shot consumed / repeat cap).
    pub fn cron_record_run(
        &self,
        id: &str,
        status: &str,
        session_id: Option<&str>,
        detail: Option<&str>,
        next_run_at: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE cron_jobs SET run_count = run_count + 1,
             last_run_at = datetime('now'), last_status = ?2,
             next_run_at = ?5, updated_at = datetime('now') WHERE id = ?1",
            params![id, status, session_id, detail, next_run_at],
        )?;
        self.conn.execute(
            "INSERT INTO cron_runs (job_id, finished_at, status, session_id, detail)
             VALUES (?1, datetime('now'), ?2, ?3, ?4)",
            params![id, status, session_id, detail],
        )?;
        Ok(())
    }

    /// Update only the cached next-run column (cheap, every tick).
    pub fn cron_set_next(&self, id: &str, next_run_at: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE cron_jobs SET next_run_at = ?2 WHERE id = ?1",
            params![id, next_run_at],
        )?;
        Ok(())
    }

    /// Last N runs of one job (newest first).
    pub fn cron_run_history(&self, job_id: &str, limit: u32) -> Result<Vec<CronRun>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, started_at, COALESCE(finished_at,''), COALESCE(status,''),
                    session_id, detail
             FROM cron_runs WHERE job_id = ?1 ORDER BY run_id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![job_id, limit], |r| {
            Ok(CronRun {
                run_id: r.get(0)?,
                started_at: r.get(1)?,
                finished_at: r.get(2)?,
                status: r.get(3)?,
                session_id: r.get(4)?,
                detail: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ── triggers (plan item 1) ──────────────────────────────────────

    /// Insert or replace by NAME (the human-typed key, unlike cron's
    /// generated ids). Returns the row's id.
    pub fn trigger_upsert(&self, t: &TriggerRow) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO triggers
             (name, enabled, platform, network, channel, event, match_kind, pattern,
              action, arg, cooldown_secs, per_hour_cap, min_role,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     datetime('now'), datetime('now'))
             ON CONFLICT(name) DO UPDATE SET
              enabled=excluded.enabled, platform=excluded.platform,
              network=excluded.network, channel=excluded.channel,
              event=excluded.event, match_kind=excluded.match_kind,
              pattern=excluded.pattern, action=excluded.action, arg=excluded.arg,
              cooldown_secs=excluded.cooldown_secs,
              per_hour_cap=excluded.per_hour_cap, min_role=excluded.min_role,
              updated_at=datetime('now')",
            params![
                t.name,
                t.enabled,
                t.platform,
                t.network,
                t.channel,
                t.event,
                t.match_kind,
                t.pattern,
                t.action,
                t.arg,
                t.cooldown_secs,
                t.per_hour_cap,
                t.min_role,
            ],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM triggers WHERE name = ?1",
            params![t.name],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn trigger_list(&self, enabled_only: bool) -> Result<Vec<TriggerRow>> {
        let sql = if enabled_only {
            "SELECT id, name, enabled, platform, network, channel, event, match_kind,
                    pattern, action, arg, cooldown_secs, per_hour_cap, min_role,
                    fire_count, last_fired_at, last_status
             FROM triggers WHERE enabled = 1 ORDER BY id"
        } else {
            "SELECT id, name, enabled, platform, network, channel, event, match_kind,
                    pattern, action, arg, cooldown_secs, per_hour_cap, min_role,
                    fire_count, last_fired_at, last_status
             FROM triggers ORDER BY id"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], trigger_row_from)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn trigger_get(&self, name: &str) -> Result<Option<TriggerRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, enabled, platform, network, channel, event, match_kind,
                    pattern, action, arg, cooldown_secs, per_hour_cap, min_role,
                    fire_count, last_fired_at, last_status
             FROM triggers WHERE name = ?1",
        )?;
        let mut rows = stmt.query(params![name])?;
        match rows.next()? {
            Some(r) => Ok(Some(trigger_row_from(r)?)),
            None => Ok(None),
        }
    }

    /// Partial edit by name. None keeps the old value.
    #[allow(clippy::too_many_arguments)]
    pub fn trigger_update(
        &self,
        name: &str,
        network: Option<&str>,
        channel: Option<&str>,
        event: Option<&str>,
        match_kind: Option<&str>,
        pattern: Option<Option<&str>>,
        action: Option<&str>,
        arg: Option<Option<&str>>,
        cooldown_secs: Option<i64>,
        per_hour_cap: Option<i64>,
        min_role: Option<Option<&str>>,
    ) -> Result<Option<TriggerRow>> {
        let Some(mut row) = self.trigger_get(name)? else {
            return Ok(None);
        };
        if let Some(v) = network {
            row.network = Some(v.to_string());
        }
        if let Some(v) = channel {
            row.channel = Some(v.to_string());
        }
        if let Some(v) = event {
            row.event = v.to_string();
        }
        if let Some(v) = match_kind {
            row.match_kind = v.to_string();
        }
        if let Some(v) = pattern {
            row.pattern = v.map(String::from);
        }
        if let Some(v) = action {
            row.action = v.to_string();
        }
        if let Some(v) = arg {
            row.arg = v.map(String::from);
        }
        if let Some(v) = cooldown_secs {
            row.cooldown_secs = v;
        }
        if let Some(v) = per_hour_cap {
            row.per_hour_cap = v;
        }
        if let Some(v) = min_role {
            row.min_role = v.map(String::from);
        }
        self.conn.execute(
            "UPDATE triggers SET network=?2, channel=?3, event=?4, match_kind=?5,
             pattern=?6, action=?7, arg=?8, cooldown_secs=?9, per_hour_cap=?10,
             min_role=?11, updated_at=datetime('now') WHERE id=?1",
            params![
                row.id,
                row.network,
                row.channel,
                row.event,
                row.match_kind,
                row.pattern,
                row.action,
                row.arg,
                row.cooldown_secs,
                row.per_hour_cap,
                row.min_role,
            ],
        )?;
        Ok(Some(row))
    }

    pub fn trigger_set_enabled(&self, name: &str, enabled: bool) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE triggers SET enabled=?2, updated_at=datetime('now') WHERE name=?1",
            params![name, enabled],
        )?;
        Ok(n > 0)
    }

    pub fn trigger_delete(&self, name: &str) -> Result<bool> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM triggers WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            return Ok(false);
        };
        self.conn
            .execute("DELETE FROM triggers WHERE id=?1", params![id])?;
        self.conn
            .execute("DELETE FROM trigger_fires WHERE trigger_id=?1", params![id])?;
        Ok(true)
    }

    /// Bookkeeping after a fire (or refusal — status carries the
    /// reason). Mirrors `cron_record_run`.
    pub fn trigger_record_fire(
        &self,
        id: i64,
        channel: &str,
        status: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        // `fire_count` counted DECISIONS: every refusal bumped it
        // alongside the real fires, so a live row read `fires: 28` for
        // 19 fires and 9 refusals, and `last_fired_at` moved for a fire
        // that never happened. The ledger below keeps every decision —
        // that is what it is for; the counters on the row say what they
        // are named. `last_status` stays the last DECISION, which is
        // how an operator sees the most recent refusal.
        self.conn.execute(
            "UPDATE triggers SET
                 fire_count = fire_count + (CASE WHEN ?2 = 'fired' THEN 1 ELSE 0 END),
                 last_fired_at = CASE WHEN ?2 = 'fired'
                                      THEN datetime('now') ELSE last_fired_at END,
                 last_status = ?2,
                 updated_at = datetime('now')
               WHERE id = ?1",
            params![id, status],
        )?;
        self.conn.execute(
            "INSERT INTO trigger_fires (trigger_id, channel, status, detail)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, channel, status, detail],
        )?;
        Ok(())
    }

    /// Recent fires (or refusals), newest first, optionally for one
    /// trigger name.
    pub fn trigger_fire_history(&self, name: Option<&str>, limit: u32) -> Result<Vec<TriggerFire>> {
        let mut stmt = if let Some(n) = name {
            let mut s = self.conn.prepare(
                "SELECT f.fire_id, t.name, f.channel, COALESCE(f.at,''), COALESCE(f.status,''),
                        f.detail
                 FROM trigger_fires f JOIN triggers t ON t.id = f.trigger_id
                 WHERE t.name = ?1 ORDER BY f.fire_id DESC LIMIT ?2",
            )?;
            let rows = s.query_map(params![n, limit], trigger_fire_from)?;
            return Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?);
        } else {
            self.conn.prepare(
                "SELECT f.fire_id, t.name, f.channel, COALESCE(f.at,''), COALESCE(f.status,''),
                        f.detail
                 FROM trigger_fires f JOIN triggers t ON t.id = f.trigger_id
                 ORDER BY f.fire_id DESC LIMIT ?1",
            )?
        };
        let rows = stmt.query_map(params![limit], trigger_fire_from)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// A per-channel event-mask override (`/triggers mask net #chan
    /// TEXT,JOIN|auto`). Stored in the meta table under one prefix —
    /// one store, one listing, no second table for three rows.
    pub fn trigger_set_mask_override(
        &self,
        network: &str,
        channel: &str,
        events: Option<&str>, // None = back to auto
    ) -> Result<()> {
        let key = format!("trigmask:{network}:{channel}");
        match events {
            Some(e) => self.meta_set(&key, e)?,
            None => {
                self.meta_delete(&key)?;
            }
        }
        Ok(())
    }

    pub fn trigger_mask_override(&self, network: &str, channel: &str) -> Option<String> {
        self.meta_get(&format!("trigmask:{network}:{channel}"))
    }

    // ── session_search support ──────────────────────────────────────

    /// FTS5 search with a RAW query string (AND/OR/quotes/wildcards pass
    /// through). Errors on FTS5 syntax problems — callers decide whether
    /// to fall back to `search_full`'s quoted-phrase behavior.
    pub fn search_raw(&self, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.msg_id, m.session_id, m.role, m.content,
                    snippet(messages_fts, 1, '>>>', '<<<', '…', 16), m.created_at
             FROM messages_fts
             JOIN messages m ON m.msg_id = messages_fts.msg_id
             WHERE messages_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit], |r| {
            Ok(SearchHit {
                msg_id: r.get(0)?,
                session_id: r.get(1)?,
                role: r.get(2)?,
                content: r.get(3)?,
                snippet: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// FTS discovery: one row per matching message, carrying its msg_id so
    /// the caller can build ±windows and bookends. Ordered by relevance.
    pub fn search_full(&self, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let quoted = format!("\"{}\"", query.replace('"', "\"\""));
        let mut stmt = self.conn.prepare(
            "SELECT m.msg_id, m.session_id, m.role, m.content,
                    snippet(messages_fts, 1, '>>>', '<<<', '…', 16), m.created_at
             FROM messages_fts
             JOIN messages m ON m.msg_id = messages_fts.msg_id
             WHERE messages_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![quoted, limit], |r| {
            Ok(SearchHit {
                msg_id: r.get(0)?,
                session_id: r.get(1)?,
                role: r.get(2)?,
                content: r.get(3)?,
                snippet: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Messages in [anchor-window, anchor+window] of a session, ordered by
    /// msg_id. The anchor itself is included. Used by SCROLL + discovery.
    pub fn messages_around(
        &self,
        session_id: &str,
        anchor_msg_id: i64,
        window: u32,
    ) -> Result<(Vec<StoredMessage>, i64, i64)> {
        // Returns (messages, messages_before, messages_after).
        let before: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id=?1 AND msg_id < ?2",
            params![session_id, anchor_msg_id],
            |r| r.get(0),
        )?;
        let after: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id=?1 AND msg_id > ?2",
            params![session_id, anchor_msg_id],
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare(
            "SELECT msg_id, role, content, reasoning, tool_name, created_at
             FROM messages WHERE session_id=?1
               AND msg_id BETWEEN ?2 AND ?3 ORDER BY msg_id",
        )?;
        let rows = stmt.query_map(
            params![
                session_id,
                anchor_msg_id - window as i64,
                anchor_msg_id + window as i64
            ],
            |r| {
                Ok(StoredMessage {
                    msg_id: r.get(0)?,
                    role: r.get(1)?,
                    content: r.get(2)?,
                    reasoning: r.get(3)?,
                    tool_name: r.get(4)?,
                    created_at: r.get(5)?,
                })
            },
        )?;
        let msgs = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((msgs, before, after))
    }

    /// Bookends for a session: first N and last N user+assistant messages
    /// (tool rows filtered — they're noise for goal/resolution framing).
    pub fn session_bookends(
        &self,
        session_id: &str,
        n: u32,
    ) -> Result<(Vec<StoredMessage>, Vec<StoredMessage>)> {
        let fetch = |order: &str| -> Result<Vec<StoredMessage>> {
            let sql = format!(
                "SELECT msg_id, role, content, reasoning, tool_name, created_at
                 FROM messages WHERE session_id=?1 AND role IN ('user','assistant')
                 ORDER BY msg_id {order} LIMIT ?2"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![session_id, n], |r| {
                Ok(StoredMessage {
                    msg_id: r.get(0)?,
                    role: r.get(1)?,
                    content: r.get(2)?,
                    reasoning: r.get(3)?,
                    tool_name: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?;
            Ok(rows
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .rev()
                .collect())
        };
        let mut head = fetch("ASC")?;
        let tail = fetch("DESC")?;
        head.sort_by_key(|m| m.msg_id);
        Ok((head, tail))
    }

    /// Whole-session dump for READ shape. Returns (all_or_window, total).
    /// When the session is large, returns first 20 + last 10 (hermes parity).
    pub fn session_messages(
        &self,
        session_id: &str,
        role_filter: Option<&str>,
    ) -> Result<(Vec<StoredMessage>, u64)> {
        let total: i64 = match role_filter {
            Some(roles) => {
                let in_list = roles
                    .split(',')
                    .map(|r| format!("'{}'", r.trim().replace('\'', "''")))
                    .collect::<Vec<_>>()
                    .join(",");
                self.conn.query_row(
                    &format!(
                        "SELECT COUNT(*) FROM messages WHERE session_id=?1 AND role IN ({in_list})"
                    ),
                    params![session_id],
                    |r| r.get(0),
                )?
            }
            None => self.conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id=?1",
                params![session_id],
                |r| r.get(0),
            )?,
        };
        let role_clause = match role_filter {
            Some(roles) => format!(
                "AND role IN ({})",
                roles
                    .split(',')
                    .map(|r| format!("'{}'", r.trim().replace('\'', "''")))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            None => String::new(),
        };
        let msgs = if total > 40 {
            // first 20 + last 10
            let mut head_stmt = self.conn.prepare(&format!(
                "SELECT msg_id, role, content, reasoning, tool_name, created_at
                 FROM messages WHERE session_id=?1 {role_clause} ORDER BY msg_id LIMIT 20"
            ))?;
            let head = head_stmt.query_map(params![session_id], stored_from_row)?;
            let mut tail_stmt = self.conn.prepare(&format!(
                "SELECT msg_id, role, content, reasoning, tool_name, created_at
                 FROM messages WHERE session_id=?1 {role_clause} ORDER BY msg_id DESC LIMIT 10"
            ))?;
            let mut tail: Vec<StoredMessage> = tail_stmt
                .query_map(params![session_id], stored_from_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            tail.reverse();
            let mut all: Vec<StoredMessage> = head.collect::<std::result::Result<Vec<_>, _>>()?;
            all.extend(tail);
            all
        } else {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT msg_id, role, content, reasoning, tool_name, created_at
                 FROM messages WHERE session_id=?1 {role_clause} ORDER BY msg_id"
            ))?;
            let rows = stmt.query_map(params![session_id], stored_from_row)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok((msgs, total as u64))
    }

    /// The LAST `limit` messages of a session in chronological order,
    /// plus the session's total message count. Unlike
    /// `session_messages` (which windows head+tail for prompt context),
    /// this is a faithful tail read — what the plugin bridge's
    /// `veles::history` wants.
    pub fn recent_messages(
        &self,
        session_id: &str,
        limit: u32,
    ) -> Result<(Vec<StoredMessage>, u64)> {
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id=?1",
            params![session_id],
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare(
            "SELECT msg_id, role, content, reasoning, tool_name, created_at
             FROM messages WHERE session_id=?1
             ORDER BY msg_id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit], |r| {
            Ok(StoredMessage {
                msg_id: r.get(0)?,
                role: r.get(1)?,
                content: r.get(2)?,
                reasoning: r.get(3)?,
                tool_name: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        let mut msgs = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        msgs.reverse(); // back to chronological
        Ok((msgs, total.max(0) as u64))
    }

    /// Session metadata for BROWSE: id, title, created_at, message count.
    pub fn session_meta(&self, limit: u32) -> Result<Vec<SessionMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, COALESCE(s.title,''), COALESCE(s.created_at,''),
                    COALESCE(s.updated_at,''),
                    (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id),
                    s.parent
             FROM sessions s ORDER BY s.updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(SessionMeta {
                id: r.get(0)?,
                title: r.get(1)?,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
                message_count: r.get(4)?,
                parent: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// One page of the session browser (the desktop's Sessions window,
    /// the sidebar), newest activity first, and how many sessions match
    /// in all. `q` matches the title, the id or the first user line; with
    /// `fts` it matches message text instead (one FTS query, grouped by
    /// session).
    ///
    /// The page is picked from `sessions` alone, on `idx_sessions_updated`,
    /// and only its rows get a preview and a message count. Until
    /// 2026-10-13 one query worked both out for EVERY session and then
    /// sorted them all. For 100 rows it ran 3,791 first-line lookups and
    /// walked all 182K entries of `idx_messages_session`. Warm, that took
    /// 20 ms. With the page cache evicted by a build, it held the store
    /// lock for 30–79 s and froze the desktop. A `q` that searches first
    /// lines still reads every session's, because that is the search.
    pub fn session_page(&self, q: &SessionQuery) -> Result<(Vec<SessionRow>, u64)> {
        let mut filters: Vec<String> = Vec::new();
        let mut binds: Vec<String> = Vec::new();
        match q.titled {
            Some(true) => filters.push("COALESCE(s.title,'') <> ''".into()),
            Some(false) => filters.push("COALESCE(s.title,'') = ''".into()),
            None => {}
        }
        let text = q.q.as_deref().map(str::trim).unwrap_or_default();
        if !text.is_empty() {
            binds.push(if q.fts {
                format!("\"{}\"", text.replace('"', "\"\""))
            } else {
                text.to_lowercase()
            });
            filters.push(if q.fts {
                "s.id IN (SELECT session_id FROM messages_fts WHERE messages_fts MATCH ?1)".into()
            } else {
                format!(
                    "(instr(lower(COALESCE(s.title,'')), ?1) > 0 OR instr(lower(s.id), ?1) > 0 \
                      OR instr(lower({SESSION_PREVIEW}), ?1) > 0)"
                )
            });
        }
        let where_ = if filters.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", filters.join(" AND "))
        };
        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM sessions s {where_}"),
            rusqlite::params_from_iter(binds.iter()),
            |r| r.get(0),
        )?;
        let n = binds.len();
        // The rowid breaks ties within one second, so the order is total
        // and OFFSET paging neither repeats nor skips a session.
        let sql = format!(
            "SELECT s.id FROM sessions s {where_}
             ORDER BY s.updated_at DESC, s.rowid DESC LIMIT ?{} OFFSET ?{}",
            n + 1,
            n + 2
        );
        let mut all: Vec<rusqlite::types::Value> = binds
            .into_iter()
            .map(rusqlite::types::Value::Text)
            .collect();
        all.push(rusqlite::types::Value::Integer(i64::from(q.limit)));
        all.push(rusqlite::types::Value::Integer(i64::from(q.offset)));
        let ids = {
            let mut stmt = self.conn.prepare(&sql)?;
            let ids = stmt.query_map(rusqlite::params_from_iter(all.iter()), |r| r.get(0))?;
            ids.collect::<std::result::Result<Vec<String>, _>>()?
        };
        let at: std::collections::HashMap<&str, usize> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        let mut rows = self.session_rows(&ids)?;
        rows.sort_by_key(|r| at.get(r.id.as_str()).copied().unwrap_or(usize::MAX));
        Ok((rows, total.max(0) as u64))
    }

    /// The browser rows of these sessions, whatever page they fall on —
    /// the sidebar's pinned sessions are listed even when old.
    pub fn session_rows(&self, ids: &[String]) -> Result<Vec<SessionRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT s.id, COALESCE(s.title,''), COALESCE(s.created_at,''),
                    COALESCE(s.updated_at,''), s.parent, {SESSION_PREVIEW},
                    (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id)
             FROM sessions s WHERE s.id IN (SELECT value FROM json_each(?1))"
        ))?;
        let rows = stmt.query_map(params![serde_json::to_string(ids)?], session_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// How many sessions there are.
    pub fn session_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    /// The session a reference names: the id itself, else the most
    /// recently active session whose id starts with it. Every session —
    /// the old lookups searched the newest 1,000 and could not reach
    /// the rest.
    pub fn find_session(&self, reference: &str) -> Result<Option<String>> {
        Ok(self.sessions_with_prefix(reference, 1)?.into_iter().next())
    }

    /// Sessions whose id is `prefix` or starts with it, the exact id
    /// first, then newest activity first.
    pub fn sessions_with_prefix(&self, prefix: &str, limit: u32) -> Result<Vec<String>> {
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT id FROM sessions WHERE substr(id, 1, length(?1)) = ?1
             ORDER BY id = ?1 DESC, updated_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![prefix, limit], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Sessions whose title is exactly `title`, newest activity first.
    pub fn sessions_titled(&self, title: &str, limit: u32) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM sessions WHERE title = ?1 ORDER BY updated_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![title, limit], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// One session's title ("" when it has none), `None` when there is no
    /// such session.
    pub fn session_title(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT COALESCE(title, '') FROM sessions WHERE id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The opening of a session, for a preview: its first `n` user and
    /// assistant messages, each cut to 600 characters.
    pub fn session_head(&self, session_id: &str, n: u32) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT role, substr(content, 1, 600) FROM messages
             WHERE session_id = ?1 AND role IN ('user', 'assistant') AND content <> ''
             ORDER BY msg_id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, n], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ── approvals (M11) ─────────────────────────────────────────────

    /// Permanent "always allow" rule check.
    pub fn approval_rule_exists(&self, rule_key: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM approval_rules WHERE rule_key = ?1",
            params![rule_key],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn approval_rule_add(&self, rule_key: &str, note: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO approval_rules (rule_key, note) VALUES (?1, ?2)",
            params![rule_key, note],
        )?;
        Ok(())
    }

    pub fn approval_rule_remove(&self, rule_key: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM approval_rules WHERE rule_key = ?1",
            params![rule_key],
        )?)
    }

    pub fn approval_rules(&self) -> Result<Vec<(String, Option<String>, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT rule_key, note, created_at FROM approval_rules ORDER BY rule_key")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Audit row for every gated tool call — including the ones the aux
    /// screen allowed, so the policy itself can be reviewed later.
    #[allow(clippy::too_many_arguments)]
    pub fn record_approval(
        &self,
        session_id: &str,
        tool: &str,
        rule_key: Option<&str>,
        detail: Option<&str>,
        verdict: &str,
        decided_by: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO approvals (session_id, tool, rule_key, detail, verdict, decided_by, reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![session_id, tool, rule_key, detail, verdict, decided_by, reason],
        )?;
        Ok(())
    }

    /// The msg_ids of a session's messages, in the same order
    /// `load_history` returns them. Needed to place engine notes back
    /// between the right two messages — `load_history` returns
    /// ChatMessages, which carry no id.
    /// The highest `msg_id` this session has persisted, or 0 when it has
    /// none.
    ///
    /// The durable cursor for everything that resumes a session: an
    /// attaching client replays `msg_id > N`, and a starting turn records
    /// it so a watcher knows where the persisted transcript ends and the
    /// in-flight snapshot begins. Same role `irc_lines`' MAX(id) plays for
    /// the IRC stream.
    ///
    /// Counts ARCHIVED rows too, on purpose: this is a position in the
    /// log, not a count of what is live. Skipping archived rows would
    /// make the cursor move backwards after a compaction and replay
    /// history the client already has.
    pub fn last_msg_id(&self, session_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(msg_id), 0) FROM messages WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    /// `created_at` for each live row, parallel to `message_ids` and to
    /// `load_history`.
    ///
    /// Same `archived = 0` filter, for the same reason: a transcript
    /// zips these against the loaded messages, and a filter mismatch
    /// shifts every timestamp by however many rows a compaction took.
    pub fn message_times(&self, session_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT created_at FROM messages
             WHERE session_id = ?1 AND COALESCE(archived, 0) = 0 ORDER BY msg_id",
        )?;
        let rows = stmt.query_map(params![session_id], |r| r.get(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// `message_times`, resuming — the twin of `message_ids_since`, same
    /// filter, so the history endpoint can zip all three vectors. The
    /// desktop showed no times at all (USER 2026-09-29: "it's not always
    /// obvious to me from the context when the messages were sent").
    pub fn message_times_since(&self, session_id: &str, since: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(created_at, '') FROM messages
             WHERE session_id = ?1 AND COALESCE(archived, 0) = 0 AND msg_id > ?2
             ORDER BY msg_id",
        )?;
        let rows = stmt.query_map(params![session_id, since], |r| r.get(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn message_ids(&self, session_id: &str) -> Result<Vec<i64>> {
        self.message_ids_since(session_id, 0)
    }

    /// `message_ids`, resuming. MUST filter identically to
    /// `load_history_since` — `history_rows` zips the two vectors, and a
    /// pair that disagrees pins every note to the wrong anchor.
    pub fn message_ids_since(&self, session_id: &str, since: i64) -> Result<Vec<i64>> {
        // MUST mirror load_history's `archived = 0` filter. The history
        // endpoint zips these ids against the rendered (live) messages;
        // if this returned archived rows too, the two vectors drifted
        // apart after a compaction and notes were pinned to the wrong
        // anchor — every note then fell past the last live message and
        // rendered at the BOTTOM instead of inline (USER: "context
        // injections and approvals at the bottom, not inline").
        let mut stmt = self.conn.prepare(
            "SELECT msg_id FROM messages
             WHERE session_id = ?1 AND COALESCE(archived, 0) = 0 AND msg_id > ?2
             ORDER BY msg_id",
        )?;
        let rows = stmt.query_map(params![session_id, since], |r| r.get(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Persist an engine note, pinned after the newest message in the
    /// session so it can be rendered back in the right place.
    pub fn record_note(
        &self,
        session_id: &str,
        kind: &str,
        summary: &str,
        detail: Option<&str>,
        toggle_json: Option<&str>,
    ) -> Result<i64> {
        let after = self.last_msg_id(session_id).unwrap_or(0);
        self.conn.execute(
            "INSERT INTO engine_notes (session_id, after_msg_id, kind, summary, detail, toggle)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, after, kind, summary, detail, toggle_json],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The newest engine_note id for a session — the note-side resume
    /// cursor, twin of `last_msg_id`.
    pub fn last_note_id(&self, session_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM engine_notes WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    /// The session's frozen prompt, if one was stored.
    pub fn session_prompt_get(&self, session_id: &str) -> Result<Option<SessionPrompt>> {
        Ok(self
            .conn
            .query_row(
                "SELECT system_prompt, tools_json, format, node FROM session_prompt
                 WHERE session_id = ?1",
                params![session_id],
                |r| {
                    Ok(SessionPrompt {
                        system_prompt: r.get(0)?,
                        tools_json: r.get(1)?,
                        format: r.get(2)?,
                        node: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Store (replace) the session's frozen prompt.
    pub fn session_prompt_put(&self, session_id: &str, p: &SessionPrompt) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO session_prompt
                 (session_id, system_prompt, tools_json, format, node, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            params![session_id, p.system_prompt, p.tools_json, p.format, p.node],
        )?;
        Ok(())
    }

    /// Register (or refresh) a live turn. `INSERT OR REPLACE` because a
    /// session has one turn at a time — the row IS the statement "this
    /// session is busy", not an append-only log.
    pub fn run_start(&self, session_id: &str, pid: u32, host: &str, surface: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO session_runs
             (session_id, pid, host, surface, started_at, heartbeat_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'), datetime('now'))",
            params![session_id, pid, host, surface],
        )?;
        Ok(())
    }

    /// B6: admit a turn — or refuse it — in ONE write transaction with
    /// its live-turn registration. Returns the session's node stamp when
    /// that stamp forbids a turn (parked mid-migration, or retired),
    /// registering nothing; otherwise registers the run and returns None.
    ///
    /// `migration::park` reads this registry and stamps in one IMMEDIATE
    /// transaction too, so the two serialize across processes: a park
    /// either sees this turn and refuses, or this sees the park. The fence
    /// used to read the stamp and register only later, with the user's
    /// message persisted in between — a park in that gap handed off a
    /// session a turn was still writing.
    pub fn begin_turn_fenced(
        &self,
        session_id: &str,
        pid: u32,
        host: &str,
        surface: &str,
    ) -> Result<Option<String>> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let r = (|| -> Result<Option<String>> {
            let stamp = self.session_node_stamp(session_id)?;
            let n = stamp.as_deref();
            if crate::migration::is_parked(n) || crate::migration::is_retired(n) {
                return Ok(stamp);
            }
            self.run_start(session_id, pid, host, surface)?;
            Ok(None)
        })();
        match &r {
            Ok(_) => self.conn.execute_batch("COMMIT")?,
            Err(_) => {
                let _ = self.conn.execute_batch("ROLLBACK");
            }
        }
        r
    }

    /// Touch a live turn's heartbeat. Throttled by the caller (the tap
    /// beats on stream events, a few seconds apart at most) — the row's
    /// `heartbeat_at` is the only thing that says "still alive".
    pub fn run_heartbeat(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE session_runs SET heartbeat_at = datetime('now')
             WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Clear a live turn — `TurnGuard::drop` reaches here, so a turn
    /// that errored, was cancelled or panicked still deregisters.
    pub fn run_end(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM session_runs WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// The live runs WITHOUT reaping: what a read-only listing uses, so
    /// a GET is never a write competing for SQLite's write lock (a batch
    /// of `chat -q` writers made `GET /api/sessions` wait on it).
    pub fn runs_current(&self, max_stale_secs: i64) -> Result<Vec<SessionRun>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, pid, host, surface, started_at, heartbeat_at
             FROM session_runs WHERE heartbeat_at >= datetime('now', ?1)
             ORDER BY started_at",
        )?;
        let rows = stmt.query_map(params![format!("-{max_stale_secs} seconds")], |r| {
            Ok(SessionRun {
                session_id: r.get(0)?,
                pid: r.get(1)?,
                host: r.get(2)?,
                surface: r.get(3)?,
                started_at: r.get(4)?,
                heartbeat_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Every genuinely-live turn, reaping stale rows on the way: a
    /// process that died between heartbeats leaves its row behind, and
    /// 30s of silence is the difference between "running" and "was
    /// running". The reap is in the reader (not a background job) so
    /// every reader converges on the same answer without a sweeper
    /// process to own.
    pub fn runs_live(&self, max_stale_secs: i64) -> Result<Vec<SessionRun>> {
        self.conn.execute(
            "DELETE FROM session_runs
             WHERE heartbeat_at < datetime('now', ?1)",
            params![format!("-{max_stale_secs} seconds")],
        )?;
        let mut stmt = self.conn.prepare(
            "SELECT session_id, pid, host, surface, started_at, heartbeat_at
             FROM session_runs ORDER BY started_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SessionRun {
                session_id: r.get(0)?,
                pid: r.get(1)?,
                host: r.get(2)?,
                surface: r.get(3)?,
                started_at: r.get(4)?,
                heartbeat_at: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Persist a session's model/provider/reasoning override — FULL
    /// REPLACE: the writer snapshots the in-memory state (itself seeded
    /// from this row at birth), so a NULL column means cleared, and a
    /// `/reasoning` set after a `/model` set cannot resurrect a model
    /// the writer never had.
    pub fn override_replace(
        &self,
        session_id: &str,
        provider: Option<&str>,
        model: Option<&str>,
        reasoning: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_overrides (session_id, provider, model, reasoning, updated_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))
             ON CONFLICT(session_id) DO UPDATE SET
                provider = excluded.provider,
                model = excluded.model,
                reasoning = excluded.reasoning,
                updated_at = datetime('now')",
            params![session_id, provider, model, reasoning],
        )?;
        Ok(())
    }

    /// This session's own UI language, if one was set with `/locale`.
    pub fn session_locale_get(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT locale FROM session_locale WHERE session_id = ?1",
                params![session_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Set this session's UI language (a canonical id).
    pub fn session_locale_set(&self, session_id: &str, locale: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_locale (session_id, locale, updated_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(session_id) DO UPDATE SET
                locale = excluded.locale, updated_at = datetime('now')",
            params![session_id, locale],
        )?;
        Ok(())
    }

    /// Forget this session's UI language; it follows the surface/process
    /// default again. Returns whether one was set.
    pub fn session_locale_clear(&self, session_id: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM session_locale WHERE session_id = ?1",
            params![session_id],
        )? > 0)
    }

    /// The session's stored override, if any — what the next agent to
    /// open this session should be born with.
    pub fn override_get(&self, session_id: &str) -> Result<Option<SessionOverride>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, model, reasoning FROM session_overrides
             WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![session_id], |r| {
            Ok(SessionOverride {
                provider: r.get(0)?,
                model: r.get(1)?,
                reasoning: r.get(2)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    // ── the message queue (R4.6) ─────────────────────────────────────

    /// A transaction that takes SQLite's write lock at BEGIN (IMMEDIATE),
    /// for every read-then-write the queue does.
    ///
    /// `unchecked_transaction()` is DEFERRED: it starts as a READER and
    /// becomes a writer at its first write. In WAL mode a reader whose
    /// snapshot went stale — another connection committed in between —
    /// cannot become a writer at all: SQLite answers "cannot promote read
    /// transaction to write transaction" (SQLITE_BUSY_SNAPSHOT, 517) at
    /// once, and no busy handler waits it out. The drainers read that as
    /// "nothing queued", so a parked row stayed parked until some later
    /// boundary — measured 2026-10-11 as session mail stuck "queued" for
    /// 60 s in 4 runs of 5, whenever the sender's own write (its waiting
    /// flag) landed between a drainer's read and its write. Taking the
    /// write lock first makes a competing writer something to WAIT for.
    pub(crate) fn write_tx(&self) -> Result<rusqlite::Transaction<'_>> {
        Ok(rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?)
    }

    /// Append a message to a session's queue (state `pending`) while a
    /// turn runs. Returns the item's seq — the handle the editor and
    /// cancel endpoints use. This is the one table a message lives in
    /// BEFORE it becomes a turn anywhere, so every surface sees the
    /// same list: what the TUI queued while the desktop's turn ran is
    /// editable and cancellable from IRC.
    pub fn queue_push(&self, session_id: &str, body: &str) -> Result<i64> {
        self.queue_push_kind(session_id, body, "message")
    }

    /// Push with an explicit kind ('message' | 'steer'). The seq is
    /// shared, so a steer docked between two messages drains in the
    /// order everything was parked.
    pub fn queue_push_kind(&self, session_id: &str, body: &str, kind: &str) -> Result<i64> {
        self.queue_push_routed(session_id, body, kind, None, None)
    }

    /// Push with the answer's destination: the platform-native id of the
    /// message that parked it and the adapter instance that carried it.
    ///
    /// A parked row is a real turn on a real message, so on a THREADED
    /// platform its answer has to go under the message that asked. atproto
    /// is the only one today (telegram and IRC are flat and ignore
    /// `reply_to`), but it is a property of the row, not of the adapter
    /// that happens to read it. The row that parks behind a running turn
    /// is drained at that turn's boundary, where the `msg` in scope is the
    /// turn-STARTING message — answering with that message's id put the
    /// reply under an unrelated earlier post. Both are NULL for a mail or
    /// cron row, which parked behind no platform message, and then the
    /// reply threads under the session's own chat.
    pub fn queue_push_routed(
        &self,
        session_id: &str,
        body: &str,
        kind: &str,
        reply_to: Option<&str>,
        route: Option<&str>,
    ) -> Result<i64> {
        let tx = self.write_tx()?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM session_queue WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO session_queue (session_id, seq, body, state, kind, reply_to, route)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6)",
            params![session_id, seq, body, kind, reply_to, route],
        )?;
        tx.commit()?;
        Ok(seq)
    }

    /// Take every parked steer row, oldest first, and DELETE them (the
    /// turn start injects them; the rows' job is done). Called from
    /// turn start, not from the drain boundary, so a steer parked
    /// behind a finished turn is not raced away by the next drain.
    pub fn take_steers(&self, session_id: &str) -> Result<Vec<String>> {
        let tx = self.write_tx()?;
        let mut stmt = tx.prepare(
            "SELECT body FROM session_queue
             WHERE session_id = ?1 AND kind = 'steer' AND state = 'pending'
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![session_id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        drop(stmt);
        tx.execute(
            "DELETE FROM session_queue
             WHERE session_id = ?1 AND kind = 'steer' AND state = 'pending'",
            params![session_id],
        )?;
        tx.commit()?;
        Ok(out)
    }

    /// Discard one shell's durable completion after `shell wait` already
    /// returned its output. Match the decoded task id, not raw JSON text:
    /// queue readers and writers must agree on what is the same event.
    pub fn discard_bgevent(&self, session_id: &str, child_id: u32, kind: &str) -> Result<bool> {
        let tx = self.write_tx()?;
        let mut stmt = tx.prepare(
            "SELECT seq, body FROM session_queue
             WHERE session_id = ?1 AND kind = 'bgevent' AND state = 'pending'
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut ids = Vec::new();
        for row in rows {
            let (seq, body) = row?;
            let matches = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .is_some_and(|v| {
                    v["child_id"].as_u64() == Some(u64::from(child_id))
                        && v["kind"].as_str() == Some(kind)
                });
            if matches {
                ids.push(seq);
            }
        }
        drop(stmt);
        let found = !ids.is_empty();
        for seq in ids {
            tx.execute(
                "DELETE FROM session_queue
                 WHERE session_id = ?1 AND seq = ?2 AND kind = 'bgevent' AND state = 'pending'",
                params![session_id, seq],
            )?;
        }
        tx.commit()?;
        Ok(found)
    }

    /// Take every parked background-event row, oldest first, and DELETE
    /// them. Same shape as `take_steers` — a completed background task's
    /// result is injected at the next turn boundary, then the row's job
    /// is done.
    pub fn take_bgevents(&self, session_id: &str) -> Result<Vec<String>> {
        let tx = self.write_tx()?;
        let mut stmt = tx.prepare(
            "SELECT body FROM session_queue
             WHERE session_id = ?1 AND kind = 'bgevent' AND state = 'pending'
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![session_id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        drop(stmt);
        tx.execute(
            "DELETE FROM session_queue
             WHERE session_id = ?1 AND kind = 'bgevent' AND state = 'pending'",
            params![session_id],
        )?;
        tx.commit()?;
        Ok(out)
    }

    /// The whole queue for a session, oldest first — pending and
    /// running alike. Readers render all of it; the drainer only takes
    /// `pending` rows (`queue_take_next`).
    pub fn queue_items(&self, session_id: &str) -> Result<Vec<QueueItem>> {
        // Named columns, never `*`: a fresh CREATE TABLE puts reply_to and
        // route BEFORE queued_at, while an existing database gets them
        // appended AFTER it by `ALTER TABLE ADD COLUMN`. The two orders
        // differ, and a positional read would swap reply_to into queued_at
        // on whichever of the two shapes it was not written against.
        let mut stmt = self.conn.prepare(
            "SELECT session_id, seq, body, state, kind, reply_to, route, queued_at
             FROM session_queue WHERE session_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(QueueItem {
                session_id: r.get(0)?,
                seq: r.get(1)?,
                body: r.get(2)?,
                state: r.get(3)?,
                kind: r.get(4)?,
                reply_to: r.get(5)?,
                route: r.get(6)?,
                queued_at: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Rewrite a queued message's body. PENDING (or held) rows only — a
    /// running item is already a turn in flight, and editing THAT is what
    /// `/steer` is for. Returns false when the row was not pending (or
    /// gone), true when the rewrite landed.
    pub fn queue_patch(&self, session_id: &str, seq: i64, body: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE session_queue SET body = ?3
             WHERE session_id = ?1 AND seq = ?2 AND state IN ('pending', 'held')",
            params![session_id, seq, body],
        )?;
        Ok(n == 1)
    }

    /// Cancel a queued message — DELETE, not a state: a cancelled item
    /// must never run, and nothing audits it. PENDING rows only; the
    /// running turn is cancelled with `/cancel`, which is a different
    /// fact (a turn in flight) than a message never sent.
    pub fn queue_delete(&self, session_id: &str, seq: i64) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM session_queue
             WHERE session_id = ?1 AND seq = ?2 AND state IN ('pending', 'held')",
            params![session_id, seq],
        )?;
        Ok(n == 1)
    }

    /// Release HELD rows to the drainer: one (`Some(seq)`) or all of the
    /// session's. A row is held when it arrived with a session from
    /// another node (a migration or a takeover, PLAN-pool-round.md D2):
    /// a queued message runs as a turn, and a node must not be able to
    /// plant a turn here by shipping a session — so it waits for someone
    /// here to say so. How many were released.
    pub fn queue_run_held(&self, session_id: &str, seq: Option<i64>) -> Result<usize> {
        Ok(match seq {
            Some(seq) => self.conn.execute(
                "UPDATE session_queue SET state = 'pending'
                 WHERE session_id = ?1 AND seq = ?2 AND state = 'held'",
                params![session_id, seq],
            )?,
            None => self.conn.execute(
                "UPDATE session_queue SET state = 'pending'
                 WHERE session_id = ?1 AND state = 'held'",
                params![session_id],
            )?,
        })
    }

    /// Sessions with queued messages HELD here (they came with a
    /// session from another node), and how many each — what the owner
    /// is asked to release (`syncask`).
    pub fn queue_held_counts(&self) -> Result<Vec<(String, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, COUNT(*) FROM session_queue
             WHERE state = 'held' GROUP BY session_id ORDER BY session_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Release a row whose turn FINISHED: the drainer calls this right
    /// after `run_turn` returns, before taking the next item. RUNNING
    /// rows only — a pending row is cancelled with `queue_delete`, and
    /// a row this call cannot see is already gone.
    ///
    /// Without this the queue is not just at-least-once, it is
    /// delivered-in-a-loop: turn end clears the session's `session_runs`
    /// row, so the next `queue_take_next` reads the completed-but-still
    /// `running` row as a crashed taker's corpse, demotes it to pending,
    /// and re-runs the SAME message forever. Live in production as
    /// ~100 duplicate turns from one keystroke (2026-09-08). The
    /// demote-recovery path stays — it is correct for turns that died —
    /// but a completed row must be gone before the next take asks.
    pub fn queue_release(&self, session_id: &str, seq: i64) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM session_queue
             WHERE session_id = ?1 AND seq = ?2 AND state = 'running'",
            params![session_id, seq],
        )?;
        Ok(n == 1)
    }

    /// The drainer's take: the oldest pending item, promoted to
    /// `running`. Also RECOVERS rows stuck at `running` whose turn
    /// never got a live `session_runs` row — or died without one being
    /// reaped — by demoting them to `pending` first, so a crashed
    /// drainer cannot eat a session's queue forever.
    ///
    /// `unchecked_transaction` on `&self` because the lone store mutex
    /// already serializes writers within a process, and SQLite's own
    /// write lock covers the cross-process race: whichever process
    /// wins the BEGIN IMMEDIATE takes the row, and a loser re-sees it
    /// as `running`, not `pending`.
    pub fn queue_take_next(&self, session_id: &str) -> Result<Option<QueueItem>> {
        let tx = self.write_tx()?;
        // A `running` row without a live turn row means whoever took it
        // is gone. The live check is "any session_runs row exists":
        // turn_ended clears it on every path the tap wraps, and a
        // crashed process leaves a row the reader reaps after 30s of
        // silence — either way, no row means no live turn.
        let live_turn: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_runs WHERE session_id = ?1)",
            params![session_id],
            |r| r.get(0),
        )?;
        if !live_turn {
            let _ = tx.execute(
                "UPDATE session_queue SET state = 'pending'
                 WHERE session_id = ?1 AND state = 'running'",
                params![session_id],
            )?;
        }
        let next: Option<QueueItem> = tx
            .query_row(
                // Named columns: the column ORDER differs between a fresh
                // CREATE TABLE (reply_to, route before queued_at) and a
                // migrated database, where ALTER TABLE appended them after
                // it. A positional read would swap fields on one of the two.
                "SELECT session_id, seq, body, state, kind, reply_to, route, queued_at
                 FROM session_queue
                 WHERE session_id = ?1 AND state = 'pending'
                   AND kind IN ('message', 'mail')
                 ORDER BY seq LIMIT 1",
                params![session_id],
                |r| {
                    Ok(QueueItem {
                        session_id: r.get(0)?,
                        seq: r.get(1)?,
                        body: r.get(2)?,
                        state: r.get(3)?,
                        kind: r.get(4)?,
                        reply_to: r.get(5)?,
                        route: r.get(6)?,
                        queued_at: r.get(7)?,
                    })
                },
            )
            .optional()?;
        if let Some(item) = &next {
            tx.execute(
                "UPDATE session_queue SET state = 'running'
                 WHERE session_id = ?1 AND seq = ?2",
                params![session_id, item.seq],
            )?;
        }
        tx.commit()?;
        Ok(next)
    }

    // ── session mail (PLAN-sessions-and-phone.md S1) ─────────────────

    /// Insert a mail's bookkeeping row AND its queue row in one
    /// transaction, so a crash can never leave a queued body with no row
    /// to clamp it by (or a row nothing will ever run). Returns the
    /// queue seq.
    pub fn mail_enqueue(&self, m: &NewMail<'_>) -> Result<i64> {
        let tx = self.write_tx()?;
        tx.execute(
            "INSERT INTO session_mail (id, to_session, from_session, from_title,
                 from_node, from_pid, from_principal, chain_id, hop, body_sha, via_peer,
                 kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                m.id,
                m.to_session,
                m.from_session,
                m.from_title,
                m.from_node,
                m.from_pid,
                m.from_principal,
                m.chain_id,
                m.hop,
                m.body_sha,
                m.via_peer,
                m.kind
            ],
        )?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM session_queue WHERE session_id = ?1",
            params![m.to_session],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO session_queue (session_id, seq, body, state, kind)
             VALUES (?1, ?2, ?3, 'pending', 'mail')",
            params![m.to_session, seq, m.body],
        )?;
        tx.commit()?;
        Ok(seq)
    }

    /// One mail row by id.
    pub fn mail_get(&self, id: &str) -> Result<Option<MailRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {MAIL_COLS} FROM session_mail WHERE id = ?1"),
                params![id],
                mail_row,
            )
            .optional()?)
    }

    /// Claim a mail for the turn about to run it: `queued` (or `running`
    /// — a turn that died mid-way is re-run by the queue's crash
    /// recovery, and must be clamped again) becomes `running`, but only
    /// when the row is addressed to THIS session and its body hash is
    /// the text the turn was handed. `None` = not claimable.
    pub fn mail_claim(
        &self,
        id: &str,
        to_session: &str,
        body_sha: &str,
    ) -> Result<Option<MailRow>> {
        let n = self.conn.execute(
            "UPDATE session_mail SET state = 'running', updated_at = datetime('now')
             WHERE id = ?1 AND to_session = ?2 AND body_sha = ?3
               AND state IN ('queued', 'running')",
            params![id, to_session, body_sha],
        )?;
        if n == 1 {
            self.mail_get(id)
        } else {
            Ok(None)
        }
    }

    /// Record how the turn a mail started ended: `answered` with the
    /// reply, or `failed` with the error. Only a `running` row moves —
    /// a mail is answered once.
    pub fn mail_finish(&self, id: &str, state: &str, reply: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE session_mail SET state = ?2, reply = ?3, updated_at = datetime('now')
             WHERE id = ?1 AND state = 'running'",
            params![id, state, reply],
        )?;
        Ok(n == 1)
    }

    /// The sender is (or stopped being) blocked waiting for this mail's
    /// reply — what the cycle check reads.
    pub fn mail_set_waiting(&self, id: &str, waiting: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE session_mail SET waiting = ?2 WHERE id = ?1",
            params![id, waiting as i64],
        )?;
        Ok(())
    }

    /// Is `session` blocked, right now, waiting for an answer to a mail
    /// it sent in `chain`? A waited-for send from inside that chain back
    /// to it would deadlock: it is mid-turn, so the new mail queues
    /// behind the very turn that is waiting for the sender to finish.
    pub fn mail_waiting_in_chain(&self, session: &str, chain_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_mail
             WHERE from_session = ?1 AND chain_id = ?2 AND waiting = 1
               AND state IN ('queued', 'running'))",
            params![session, chain_id],
            |r| r.get(0),
        )?)
    }

    /// Mail addressed to or sent by a session, newest first.
    pub fn mail_for_session(&self, session: &str, limit: usize) -> Result<Vec<MailRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {MAIL_COLS} FROM session_mail
             WHERE to_session = ?1 OR from_session = ?1
             ORDER BY created_at DESC, rowid DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![session, limit as i64], mail_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Forget finished mail older than `days` (answered or failed). The
    /// transcript keeps the text; this table is only bookkeeping.
    pub fn mail_prune(&self, days: u32) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM session_mail
             WHERE state IN ('answered', 'failed')
               AND updated_at < datetime('now', ?1)",
            params![format!("-{days} days")],
        )?)
    }

    /// Every note for a session, oldest first.
    pub fn notes_for(&self, session_id: &str) -> Result<Vec<StoredNote>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, after_msg_id, kind, summary, COALESCE(detail, ''), COALESCE(toggle, ''),
                    COALESCE(created_at, '')
             FROM engine_notes WHERE session_id = ?1 ORDER BY after_msg_id, id",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(StoredNote {
                id: r.get(0)?,
                after_msg_id: r.get(1)?,
                kind: r.get(2)?,
                summary: r.get(3)?,
                detail: r.get(4)?,
                toggle: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ── IRC scrollback ───────────────────────────────────────────────

    /// Append one observed line. Returns its durable id, which is what
    /// a client resumes from — the tap's `seq` restarts with the
    /// process and cannot be.
    #[allow(clippy::too_many_arguments)]
    pub fn irc_append(
        &self,
        network: &str,
        presence: &str,
        buffer: &str,
        ts: &str,
        kind: &str,
        nick: &str,
        account: Option<&str>,
        text: &str,
        own: bool,
        seq: i64,
        blocked: bool,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO irc_lines
                (network, presence, buffer, ts, kind, nick, account, text, own, seq, blocked)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                network,
                presence,
                buffer,
                ts,
                kind,
                nick,
                account,
                text,
                if own { 1 } else { 0 },
                seq,
                if blocked { 1 } else { 0 }
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// One buffer's scrollback, oldest first.
    ///
    /// `before` pages BACKWARDS — the newest `limit` lines older than
    /// that id — because that is the only direction scrollback is ever
    /// read in. Selecting the oldest N and calling it a page would hand
    /// back the start of the channel's history when the user asked for
    /// the screenful above where they are.
    pub fn irc_backlog(
        &self,
        network: &str,
        presence: &str,
        buffer: &str,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<IrcLine>> {
        let before = before.unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT id, network, presence, buffer, ts, kind, nick,
                    COALESCE(account, ''), text, own, COALESCE(blocked, 0)
             FROM irc_lines
             WHERE network = ?1 AND presence = ?2 AND buffer = ?3 AND id < ?4
             ORDER BY id DESC LIMIT ?5",
        )?;
        let rows = stmt.query_map(
            params![network, presence, buffer, before, limit],
            Self::map_irc_line,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        // Selected newest-first to get the right page; handed back
        // oldest-first because that is the order it is rendered in.
        out.reverse();
        Ok(out)
    }

    /// Everything after `since`, across every buffer, oldest first.
    /// This is what a reconnecting event stream replays: "the desktop
    /// was closed for six hours" and "the socket dropped" are the same
    /// question.
    pub fn irc_since(&self, since: i64, limit: u32) -> Result<Vec<IrcLine>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, network, presence, buffer, ts, kind, nick,
                    COALESCE(account, ''), text, own, COALESCE(blocked, 0)
             FROM irc_lines WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since, limit], Self::map_irc_line)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Search one buffer's scrollback for `needle` (case-insensitive
    /// substring), newest first, capped at `limit`. Both presences
    /// match — one conversation, one search. `instr`, not LIKE, so
    /// `%`/`_` in the needle are literal characters.
    pub fn irc_search(
        &self,
        network: &str,
        buffer: &str,
        needle: &str,
        limit: u32,
    ) -> Result<Vec<IrcLine>> {
        self.irc_query(&IrcQuery {
            network: network.to_string(),
            buffer: Some(buffer.to_string()),
            needle: Some(needle.to_string()),
            limit,
            ..IrcQuery::default()
        })
    }

    /// Scrollback search along every axis the interactive case needs:
    /// a text needle, a nick, a time range, one buffer or all of them.
    ///
    /// WHY the axes: "one buffer, substring, cap" cannot answer the
    /// questions that actually come up when working alongside other
    /// people and other bots in a channel — *what did erasmus say
    /// about the deploy*, *what did I miss since 09:00*, *what were
    /// the last twenty lines here*. Each of those was a different
    /// manual scroll through a capped, needle-only result.
    ///
    /// Every filter is optional. No needle is a `tail` (the newest
    /// `limit` lines, unfiltered); no buffer searches every buffer on
    /// the network. Newest `limit` matches, handed back oldest-first
    /// because that is the order they render in.
    ///
    /// `since`/`until` are compared as strings against the stored
    /// RFC3339 timestamp — which is what makes a prefix (`2026-09-09`,
    /// `2026-09-09T14`) a legal bound as well as a full timestamp.
    /// `since` is inclusive, `until` exclusive.
    pub fn irc_query(&self, q: &IrcQuery) -> Result<Vec<IrcLine>> {
        let mut sql = String::from(
            "SELECT id, network, presence, buffer, ts, kind, nick,
                    COALESCE(account, ''), text, own, COALESCE(blocked, 0)
             FROM irc_lines WHERE network = ?1",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(q.network.clone())];
        let push = |sql: &mut String,
                    args: &mut Vec<Box<dyn rusqlite::ToSql>>,
                    clause: &str,
                    val: String| {
            args.push(Box::new(val));
            sql.push_str(&clause.replace('?', &format!("?{}", args.len())));
        };
        if let Some(b) = q.buffer.as_ref().filter(|b| !b.is_empty()) {
            push(&mut sql, &mut args, " AND buffer = ?", b.clone());
        }
        if let Some(n) = q.needle.as_ref().filter(|n| !n.is_empty()) {
            push(
                &mut sql,
                &mut args,
                " AND instr(lower(text), lower(?)) > 0",
                n.clone(),
            );
        }
        if let Some(n) = q.nick.as_ref().filter(|n| !n.is_empty()) {
            push(
                &mut sql,
                &mut args,
                " AND lower(nick) = lower(?)",
                n.clone(),
            );
        }
        if let Some(s) = q.since.as_ref().filter(|s| !s.is_empty()) {
            push(&mut sql, &mut args, " AND ts >= ?", s.clone());
        }
        if let Some(u) = q.until.as_ref().filter(|u| !u.is_empty()) {
            push(&mut sql, &mut args, " AND ts < ?", u.clone());
        }
        args.push(Box::new(q.limit.max(1)));
        sql.push_str(&format!(" ORDER BY id DESC LIMIT ?{}", args.len()));

        let mut stmt = self.conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), Self::map_irc_line)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        // Selected newest-first for the cap; handed back oldest-first
        // because that is the order the answers render in.
        out.reverse();
        Ok(out)
    }

    /// The newest stored id, so a fresh client knows where "now" is.
    pub fn irc_latest_id(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM irc_lines", [], |r| {
                r.get(0)
            })
            .unwrap_or(0))
    }

    /// Buffers with any scrollback, so a client can list what it can
    /// reopen even for channels the connection is not currently in.
    pub fn irc_buffers(&self, network: &str, presence: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT buffer FROM irc_lines
             WHERE network = ?1 AND presence = ?2 ORDER BY buffer",
        )?;
        let rows = stmt.query_map(params![network, presence], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    fn map_irc_line(r: &rusqlite::Row<'_>) -> rusqlite::Result<IrcLine> {
        Ok(IrcLine {
            id: r.get(0)?,
            network: r.get(1)?,
            presence: r.get(2)?,
            buffer: r.get(3)?,
            ts: r.get(4)?,
            kind: r.get(5)?,
            nick: r.get(6)?,
            account: r.get(7)?,
            text: r.get(8)?,
            own: r.get::<_, i64>(9)? != 0,
            blocked: r.get::<_, i64>(10)? != 0,
        })
    }

    /// Trim scrollback: keep at most `per_buffer` lines in each buffer,
    /// and drop anything older than `days` regardless.
    ///
    /// Per BUFFER rather than a global cap, because a global one lets a
    /// busy channel evict a quiet one entirely — the quiet channel is
    /// usually the one whose history you wanted.
    pub fn irc_prune(&self, per_buffer: u32, days: u32) -> Result<usize> {
        let mut removed = 0usize;
        if days > 0 {
            removed += self.conn.execute(
                "DELETE FROM irc_lines
                  WHERE ts < datetime('now', ?1)",
                params![format!("-{days} days")],
            )?;
        }
        if per_buffer > 0 {
            removed += self.conn.execute(
                "DELETE FROM irc_lines WHERE id IN (
                     SELECT id FROM (
                         SELECT id, ROW_NUMBER() OVER (
                             PARTITION BY network, presence, buffer
                             ORDER BY id DESC
                         ) AS rn
                         FROM irc_lines
                     ) WHERE rn > ?1
                 )",
                params![per_buffer],
            )?;
        }
        Ok(removed)
    }

    pub fn approval_history(&self, limit: u32) -> Result<Vec<ApprovalRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, tool, rule_key, detail, verdict, decided_by, reason, created_at
             FROM approvals ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(ApprovalRow {
                id: r.get(0)?,
                session_id: r.get(1)?,
                tool: r.get(2)?,
                rule_key: r.get(3)?,
                detail: r.get(4)?,
                verdict: r.get(5)?,
                decided_by: r.get(6)?,
                reason: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ── todos (M12) ─────────────────────────────────────────────────

    pub fn todos_list(&self, session_id: &str) -> Result<Vec<TodoItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT item_id, content, status FROM todos
             WHERE session_id = ?1 ORDER BY position, item_id",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(TodoItem {
                id: r.get(0)?,
                content: r.get(1)?,
                status: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Full replace (hermes default) — the model sends the whole list.
    pub fn todos_replace(&self, session_id: &str, items: &[TodoItem]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM todos WHERE session_id = ?1",
            params![session_id],
        )?;
        for (i, it) in items.iter().enumerate() {
            tx.execute(
                "INSERT INTO todos (session_id, item_id, content, status, position, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
                params![session_id, it.id, it.content, it.status, i as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Merge by item id, keeping items the model didn't mention.
    pub fn todos_merge(&self, session_id: &str, items: &[TodoItem]) -> Result<()> {
        let existing = self.todos_list(session_id)?;
        let tx = self.conn.unchecked_transaction()?;
        let mut next_pos = existing.len() as i64;
        for it in items {
            let known = existing.iter().any(|e| e.id == it.id);
            if known {
                tx.execute(
                    "UPDATE todos SET content = ?3, status = ?4, updated_at = datetime('now')
                     WHERE session_id = ?1 AND item_id = ?2",
                    params![session_id, it.id, it.content, it.status],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO todos (session_id, item_id, content, status, position, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
                    params![session_id, it.id, it.content, it.status, next_pos],
                )?;
                next_pos += 1;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ── checkpoints (M13) ───────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn checkpoint_add(
        &self,
        session_id: &str,
        path: &str,
        blob: Option<&str>,
        sha256: Option<&str>,
        bytes: i64,
        tool: &str,
        note: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO checkpoints (session_id, path, blob, sha256, bytes, tool, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![session_id, path, blob, sha256, bytes, tool, note],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn checkpoint_get(&self, id: i64) -> Result<Option<Checkpoint>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, path, blob, sha256, bytes, tool, note, created_at
             FROM checkpoints WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], checkpoint_row)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Newest first. `session_id` None = all sessions.
    pub fn checkpoint_list(&self, session_id: Option<&str>, limit: u32) -> Result<Vec<Checkpoint>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, path, blob, sha256, bytes, tool, note, created_at
             FROM checkpoints
             WHERE (?1 IS NULL OR session_id = ?1)
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit], checkpoint_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn checkpoint_delete(&self, id: i64) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM checkpoints WHERE id = ?1", params![id])?)
    }

    /// (count, total bytes) — `veles checkpoints status` and pruning.
    pub fn checkpoint_stats(&self) -> Result<(i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(bytes),0) FROM checkpoints",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Prune candidates: everything older than `days`, oldest first.
    pub fn checkpoints_older_than(&self, days: u32) -> Result<Vec<Checkpoint>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, path, blob, sha256, bytes, tool, note, created_at
             FROM checkpoints
             WHERE created_at < datetime('now', ?1)
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![format!("-{days} days")], checkpoint_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Oldest checkpoints first — used to trim down to a size budget.
    pub fn checkpoints_oldest(&self, limit: u32) -> Result<Vec<Checkpoint>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, path, blob, sha256, bytes, tool, note, created_at
             FROM checkpoints ORDER BY id LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], checkpoint_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

fn checkpoint_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Checkpoint> {
    Ok(Checkpoint {
        id: r.get(0)?,
        session_id: r.get(1)?,
        path: r.get(2)?,
        blob: r.get(3)?,
        sha256: r.get(4)?,
        bytes: r.get(5)?,
        tool: r.get(6)?,
        note: r.get(7)?,
        created_at: r.get(8)?,
    })
}

/// One row of the approvals audit trail.
#[derive(Debug, Clone)]
pub struct ApprovalRow {
    pub id: i64,
    pub session_id: String,
    pub tool: String,
    pub rule_key: Option<String>,
    pub detail: Option<String>,
    pub verdict: String,
    pub decided_by: String,
    pub reason: Option<String>,
    pub created_at: String,
}

/// One todo item (M12).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
}

/// One pre-mutation file snapshot (M13).
#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub id: i64,
    pub session_id: String,
    pub path: String,
    /// Blob file under VELES_HOME/checkpoints; None = file didn't exist
    /// before the mutation (restore means deleting it).
    pub blob: Option<String>,
    pub sha256: Option<String>,
    pub bytes: i64,
    pub tool: String,
    pub note: Option<String>,
    pub created_at: String,
}

/// One scheduled job (row of cron_jobs).
#[derive(Debug, Clone, Default)]
pub struct CronJob {
    pub id: String,
    pub name: Option<String>,
    /// Normalized schedule: `once:<rfc3339>` | `every:<secs>` | `cron:<spec>`.
    pub schedule: String,
    pub prompt: String,
    /// 'origin' (creation chat), 'local', 'all', or 'platform:chat_id'.
    pub deliver: String,
    pub enabled: bool,
    pub script: Option<String>,
    /// JSON array of skill names.
    pub skills: Option<String>,
    pub no_agent: bool,
    /// Sandboxed script execution (slopdrop-style safe interp). Tcl
    /// scripts only — a sandboxed bash script cannot be gated at the
    /// interp level, so it is refused rather than claiming safety it
    /// does not have.
    pub sandbox: bool,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub reasoning: Option<String>,
    pub origin: Option<String>,
    /// Session-wakeup jobs (USER taxonomy, 2026-09-17: crons are
    /// "either standalone scheduled scripts OR wakeups for specific
    /// session(s)"). When set, an agent job RESUMES this session —
    /// loads its history and runs the prompt as a turn in it — instead
    /// of minting a fresh `cron_*` session. Works one-off and
    /// periodical alike. The id is resolved at creation (unique-prefix
    /// match like /resume) so the stored value is always a full id.
    pub session: Option<String>,
    pub repeat_max: Option<i64>,
    /// Node this job is pinned to (a node id from `nodes`). NULL = a
    /// legacy floating job that fires on whichever node holds
    /// `singleton:cron`. Set = that node fires it whenever it is up
    /// (lease not required); the shared slot claim still dedupes.
    pub node: Option<String>,
    pub run_count: i64,
    pub created_at: String,
    pub updated_at: String,
    pub last_run_at: Option<String>,
    pub last_status: Option<String>,
    pub next_run_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CronRun {
    pub run_id: i64,
    pub started_at: String,
    pub finished_at: String,
    pub status: String,
    pub session_id: Option<String>,
    pub detail: Option<String>,
}

/// One trigger row (the persisted form of `triggers::Trigger`).
#[derive(Debug, Clone)]
pub struct TriggerRow {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub platform: Option<String>,
    pub network: Option<String>,
    pub channel: Option<String>,
    pub event: String,
    pub match_kind: String,
    pub pattern: Option<String>,
    pub action: String,
    pub arg: Option<String>,
    pub cooldown_secs: i64,
    pub per_hour_cap: i64,
    pub min_role: Option<String>,
    pub fire_count: i64,
    pub last_fired_at: Option<String>,
    pub last_status: Option<String>,
}

impl TriggerRow {
    /// Convert into the engine's Trigger, parsing the enum fields.
    pub fn into_trigger(self) -> crate::triggers::Trigger {
        crate::triggers::Trigger {
            id: self.id,
            name: self.name,
            enabled: self.enabled,
            platform: self.platform,
            network: self.network,
            channel: self.channel,
            event: self.event,
            match_kind: crate::triggers::MatchKind::parse(&self.match_kind)
                .unwrap_or(crate::triggers::MatchKind::Always),
            pattern: self.pattern,
            action: crate::triggers::ActionKind::parse(&self.action)
                .unwrap_or(crate::triggers::ActionKind::Slopdrop),
            arg: self.arg,
            cooldown_secs: self.cooldown_secs,
            per_hour_cap: self.per_hour_cap,
            min_role: self.min_role,
            fire_count: self.fire_count,
            last_fired_at: self.last_fired_at,
            last_status: self.last_status,
        }
    }
}

/// One fire (or refusal) of a trigger.
#[derive(Debug, Clone)]
pub struct TriggerFire {
    pub fire_id: i64,
    pub name: String,
    pub channel: String,
    pub at: String,
    pub status: String,
    pub detail: Option<String>,
}

fn trigger_row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<TriggerRow> {
    Ok(TriggerRow {
        id: r.get(0)?,
        name: r.get(1)?,
        enabled: r.get(2)?,
        platform: r.get(3)?,
        network: r.get(4)?,
        channel: r.get(5)?,
        event: r.get(6)?,
        match_kind: r.get(7)?,
        pattern: r.get(8)?,
        action: r.get(9)?,
        arg: r.get(10)?,
        cooldown_secs: r.get(11)?,
        per_hour_cap: r.get(12)?,
        min_role: r.get(13)?,
        fire_count: r.get(14)?,
        last_fired_at: r.get(15)?,
        last_status: r.get(16)?,
    })
}

fn trigger_fire_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<TriggerFire> {
    Ok(TriggerFire {
        fire_id: r.get(0)?,
        name: r.get(1)?,
        channel: r.get(2)?,
        at: r.get(3)?,
        status: r.get(4)?,
        detail: r.get(5)?,
    })
}

/// One FTS hit with enough context to build discovery windows.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub msg_id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub snippet: String,
    pub created_at: String,
}

/// A message row as read back for session_search output.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub msg_id: i64,
    pub role: String,
    pub content: String,
    pub reasoning: Option<String>,
    pub tool_name: Option<String>,
    pub created_at: String,
}

/// What the session browser asks for (see [`Store::session_page`]).
#[derive(Debug, Clone, Default)]
pub struct SessionQuery {
    pub limit: u32,
    pub offset: u32,
    pub q: Option<String>,
    /// Match message text (FTS) instead of title/id/first line.
    pub fts: bool,
    /// Some(true): titled only; Some(false): untitled only.
    pub titled: Option<bool>,
}

/// A browser row's preview, for the session aliased `s`: its first user
/// line, cut to 200 characters (`SessionRow::preview`, and what a
/// session-list `q` searches).
const SESSION_PREVIEW: &str = "COALESCE((SELECT substr(m.content, 1, 200) FROM messages m \
     WHERE m.session_id = s.id AND m.role = 'user' ORDER BY m.msg_id LIMIT 1), '')";

/// One row of the session browser.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub parent: Option<String>,
    /// The first user line, cut to 200 characters.
    pub preview: String,
    pub message_count: i64,
}

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: r.get(0)?,
        title: r.get(1)?,
        created_at: r.get(2)?,
        updated_at: r.get(3)?,
        parent: r.get(4)?,
        preview: r.get(5)?,
        message_count: r.get(6)?,
    })
}

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    /// The session this one was forked from, if any (a subagent child).
    /// The session list marks a child by this and lets you jump to the
    /// parent; NULL means a session a human or cron started.
    pub parent: Option<String>,
    pub created_at: String,
    /// Last activity (any message append or title change bumps this).
    /// The operative clock for "is this session recent" — created_at is
    /// just when it was first made.
    pub updated_at: String,
    pub message_count: i64,
}

fn stored_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    Ok(StoredMessage {
        msg_id: r.get(0)?,
        role: r.get(1)?,
        content: r.get(2)?,
        reasoning: r.get(3)?,
        tool_name: r.get(4)?,
        created_at: r.get(5)?,
    })
}

/// Repair tool-call/result pairing on a just-loaded live history.
///
/// The provider wire format is strict: an assistant row that declares
/// tool_calls must be followed by a tool row for every declared id, and
/// every tool row must answer a call a live assistant row declared. Break
/// either direction and the provider rejects the WHOLE request
/// (`400 tool_call_id is not found` on the OpenAI-compatible route),
/// which bricks the session — every later turn resends the same broken
/// history. Session 20260926_020336_18bd3e died exactly this way: a
/// forced `/compact` cut a giant single turn at a raw index and
/// `archive_head` archived the assistant while its tool result stayed
/// live; from then on the live history OPENED with an orphaned result.
///
/// The store is the last line of defence because corruption has more
/// than one door (that cut; a crash between `push_persist` of the
/// assistant row and its result row; `session_clear` racing a turn).
/// Repairing in `load_history`, the single place every TURN loads its
/// history from, fixes all of them plus every session already bricked on
/// disk, and the in-memory row set is what the next request sends — so
/// the repair has to be in-memory rather than another UPDATE.
///
/// Only on the WHOLE live set: `load_history_since` stays raw, because a
/// window cannot judge pairing (its call may sit below the cursor) and
/// its display consumers zip it against `message_ids_since`.
///
/// Two moves, both minimal:
///   - an ORPHANED TOOL RESULT (its call id is declared by no live
///     assistant row) is dropped from the loaded set. The row stays in
///     the DB, FTS-searchable; it just never reaches the wire again.
///   - an UNANSWERED CALL (a live assistant declares an id no live tool
///     row answers — the tail of a turn interrupted mid-round) has that
///     id stripped from its `tool_calls`. If that empties the list the
///     assistant row stays as plain text; stripping the call is what the
///     provider's own validators accept, where fabricating a result row
///     would put words in the tool's mouth.
fn repair_tool_round_pairing(history: &mut Vec<ChatMessage>) {
    use std::collections::HashSet;
    // Pass 1: collect every call id a live assistant row declares.
    let mut declared: HashSet<String> = HashSet::new();
    for m in history.iter() {
        if m.role == "assistant" {
            if let Some(serde_json::Value::Array(calls)) = &m.tool_calls {
                for c in calls {
                    if let Some(id) = c.get("id").and_then(serde_json::Value::as_str) {
                        declared.insert(id.to_string());
                    }
                }
            }
        }
    }
    // Pass 2: drop tool rows whose call no live assistant declares. Only
    // rows that CARRY an id are judged: a tool row with no tool_meta at
    // all (old imports, hand-built fixtures) cannot be proven orphaned,
    // and dropping unverifiable rows would eat real content — the repair
    // is a scalpel, not a filter.
    let before = history.len();
    history.retain(|m| {
        m.role != "tool"
            || match &m.tool_meta {
                Some(meta) => declared.contains(&meta.tool_call_id),
                None => true,
            }
    });
    // Pass 3: every id a surviving tool row answers; strip the rest.
    let mut answered: HashSet<String> = HashSet::new();
    for m in history.iter() {
        if m.role == "tool" {
            if let Some(meta) = &m.tool_meta {
                answered.insert(meta.tool_call_id.clone());
            }
        }
    }
    let mut stripped = 0usize;
    let mut stripped_at: Vec<usize> = Vec::new();
    for (i, m) in history.iter_mut().enumerate() {
        if m.role != "assistant" {
            continue;
        }
        if let Some(serde_json::Value::Array(calls)) = &m.tool_calls {
            let kept: Vec<serde_json::Value> = calls
                .iter()
                .filter(|c| {
                    c.get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(|id| answered.contains(id))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            if kept.len() != calls.len() {
                stripped += calls.len() - kept.len();
                stripped_at.push(i);
                m.tool_calls = if kept.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Array(kept))
                };
            }
        }
    }
    let dropped = before - history.len();
    // A turn still running has persisted its assistant row and is
    // waiting on the results: the calls stripped from the LAST assistant
    // row, with nothing but tool rows after it, are that turn in flight
    // — a reload mid-turn (the desktop does it on every event), not a
    // broken history. 542 such warnings in 30 h of a batch run.
    let last_assistant = history.iter().rposition(|m| m.role == "assistant");
    let in_flight = dropped == 0
        && last_assistant.is_some_and(|last| {
            stripped_at.iter().all(|&i| i == last)
                && history[last + 1..].iter().all(|m| m.role == "tool")
        });
    if in_flight && stripped > 0 {
        tracing::debug!(
            "load_history: {stripped} call(s) of the turn in flight are not answered yet"
        );
    } else if dropped > 0 || stripped > 0 {
        tracing::warn!(
            "load_history repair: dropped {dropped} orphaned tool result(s), stripped \
             {stripped} unanswered tool call(s) — the stored live history was not a legal \
             request (see compaction::snap_cut_off_tool_round)"
        );
    }
}

fn cron_select() -> String {
    "SELECT id, name, schedule, prompt, deliver, enabled, script, skills,
            no_agent, sandbox, model, provider, reasoning, origin, session, repeat_max,
            run_count, COALESCE(created_at,''), COALESCE(updated_at,''),
            last_run_at, last_status, next_run_at, node
     FROM cron_jobs"
        .to_string()
}

fn cron_job_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CronJob> {
    Ok(CronJob {
        id: r.get(0)?,
        name: r.get(1)?,
        schedule: r.get(2)?,
        prompt: r.get(3)?,
        deliver: r.get(4)?,
        enabled: r.get(5)?,
        script: r.get(6)?,
        skills: r.get(7)?,
        no_agent: r.get(8)?,
        sandbox: r.get(9)?,
        model: r.get(10)?,
        provider: r.get(11)?,
        reasoning: r.get(12)?,
        origin: r.get(13)?,
        session: r.get(14)?,
        repeat_max: r.get(15)?,
        run_count: r.get(16)?,
        created_at: r.get(17)?,
        updated_at: r.get(18)?,
        last_run_at: r.get(19)?,
        last_status: r.get(20)?,
        next_run_at: r.get(21)?,
        node: r.get(22)?,
    })
}

#[cfg(test)]
mod tests {

    /// A long hold names its holder on release, and a waiter behind it
    /// names that holder too — the first `lock_timed` named only waiters.
    #[test]
    fn a_long_hold_is_named_by_its_holder() {
        let dir = tempfile::tempdir().unwrap();
        let m = std::sync::Arc::new(std::sync::Mutex::new(
            Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        let m2 = m.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _g = lock_timed(&m2, "test::slow_holder");
            tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(150));
        });
        rx.recv().unwrap();
        let g = lock_timed(&m, "test::waiter");
        holder.join().unwrap();
        let (who, held) = last_holder().expect("the holder recorded itself");
        assert_eq!(who, "test::slow_holder");
        assert!(held >= std::time::Duration::from_millis(150), "{held:?}");
        drop(g);
    }
    use super::*;
    use crate::providers::ChatMessage;
    use std::thread::JoinHandle;
    use std::time::Duration;

    fn scratch() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("state.db")).expect("open store");
        (dir, store)
    }

    /// The cluster registry round-trips: a node registers, lists,
    /// heartbeats, and is forgotten. `registered_at` must survive a
    /// re-register (a node's birthday is not its latest boot), while
    /// `heartbeat_at` moves on every register/heartbeat — the two
    /// timestamps answer different questions ("how long in the pool" vs
    /// "is it alive now") and conflating them is how a restarted node
    /// would look brand-new.
    #[test]
    fn nodes_register_list_heartbeat_forget() {
        let (_d, store) = scratch();
        assert!(store.nodes_list().unwrap().is_empty());

        store
            .node_register(
                "id-laptop",
                "laptop",
                "laptop",
                "0.1.0",
                "ephemeral",
                Some(r#"{"name":"RTX 4090"}"#),
                Some("http://10.0.20.1:9900"),
                None,
                None,
            )
            .unwrap();
        store
            .node_register(
                "id-vps",
                "vps",
                "vps",
                "0.1.0",
                "always-on",
                None,
                Some("http://vps:9900"),
                None,
                None,
            )
            .unwrap();

        let nodes = store.nodes_list().unwrap();
        assert_eq!(nodes.len(), 2);
        // Seniority order: the first-registered node leads.
        assert_eq!(nodes[0].id, "id-laptop");
        assert_eq!(nodes[1].uptime_class, "always-on");

        // Re-register (a reboot): name/version update, registered_at is
        // preserved, heartbeat moves.
        let first_birthday = store.node_get("id-laptop").unwrap().unwrap().registered_at;
        store
            .node_register(
                "id-laptop",
                "laptop",
                "laptop",
                "0.2.0",
                "ephemeral",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let after = store.node_get("id-laptop").unwrap().unwrap();
        assert_eq!(after.version, "0.2.0");
        assert_eq!(
            after.registered_at, first_birthday,
            "birthday must survive re-register"
        );

        // Heartbeat only moves the liveness timestamp.
        store.node_heartbeat("id-laptop", None).unwrap();

        // Prefix lookup (the `nodes show <prefix>` shorthand).
        let got = store.node_get("id-vp").unwrap();
        assert!(got.is_some(), "unique prefix resolves");

        // Forget removes the row.
        assert!(store.node_forget("id-vps").unwrap());
        assert!(
            !store.node_forget("id-vps").unwrap(),
            "second forget is a no-op"
        );
        assert_eq!(store.nodes_list().unwrap().len(), 1);
    }

    /// A state.db that MemoryStore created first (memory_revisions only)
    /// is veles's own and must get the full schema. Found live while
    /// verifying memory replication: on a new node whose first command
    /// was `veles memory log` / `veles memory apply`, Store::open saw an
    /// existing file with no ownership marker, treated it as a FOREIGN
    /// store, and never created nodes / cron_jobs / checkpoints — the
    /// gateway then logged "no such table" for each, for good.
    #[test]
    fn a_db_that_memory_created_first_is_adopted() {
        let dir = tempfile::tempdir().unwrap();
        crate::memory::MemoryStore::at(dir.path().to_path_buf())
            .write_file("system/core.md", "first ever write")
            .unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        store
            .node_register(
                "n1",
                "laptop",
                "host",
                "0",
                "ephemeral",
                None,
                None,
                None,
                None,
            )
            .expect("the nodes table must exist");
        let cron: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='cron_jobs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cron, 1, "cron_jobs must exist too");
    }

    /// …while a FOREIGN store — one that already has its own `messages`
    /// table and no veles marker, which is what hermes' state.db looks
    /// like (the 2026-08-09 poisoning incident) — is still refused: no
    /// marker written, no messages_ai trigger created. Unpinned until the
    /// adoption rule above widened, which is exactly when it needed one.
    #[test]
    fn a_foreign_store_is_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                // hermes' session store shares veles's ancestry, so its
                // messages table is msg_id-shaped too; the MARKER is what
                // tells the two apart, not the columns.
                "CREATE TABLE messages (msg_id INTEGER PRIMARY KEY AUTOINCREMENT,
                                        session_id TEXT NOT NULL, role TEXT NOT NULL,
                                        content TEXT NOT NULL);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let marker: Option<String> = store
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key='veles_schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(
            marker, None,
            "a foreign store must not be marked veles-owned"
        );
        let trig: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='messages_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(trig, 0, "and must not get the messages_ai trigger");
    }

    /// An ambiguous id prefix resolves to None rather than guessing —
    /// `nodes show` must never answer about the wrong node.
    #[test]
    fn node_get_ambiguous_prefix_is_none() {
        let (_d, store) = scratch();
        store
            .node_register("aaa-1", "a1", "h", "0", "ephemeral", None, None, None, None)
            .unwrap();
        store
            .node_register("aaa-2", "a2", "h", "0", "ephemeral", None, None, None, None)
            .unwrap();
        assert!(
            store.node_get("aaa").unwrap().is_none(),
            "two matches = say nothing"
        );
        assert!(store.node_get("aaa-1").unwrap().is_some());
        assert!(matches!(
            store.node_resolve("aaa").unwrap(),
            NodeMatch::Ambiguous(2)
        ));
    }

    /// The prefix is LITERAL. `%`, `_` and the empty string used to be
    /// live wildcards, so in a one-node registry `nodes forget ""` (or
    /// `forget %`) resolved to that node and deleted it.
    #[test]
    fn a_node_prefix_is_matched_literally() {
        let (_d, store) = scratch();
        store
            .node_register(
                "abc-1",
                "only",
                "h",
                "0",
                "ephemeral",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        for wild in ["", "  ", "%", "_", "a%", "ab_"] {
            assert!(
                store.node_get(wild).unwrap().is_none(),
                "{wild:?} must not resolve to a node"
            );
        }
        assert!(
            store.node_get("abc").unwrap().is_some(),
            "a real prefix still works"
        );
    }

    /// A heartbeat for a row that is gone says so (0 rows) — the gateway
    /// re-registers instead of heartbeating into the void.
    #[test]
    fn a_heartbeat_reports_a_missing_row() {
        let (_d, store) = scratch();
        store
            .node_register("n-1", "n", "h", "0", "ephemeral", None, None, None, None)
            .unwrap();
        assert_eq!(store.node_heartbeat("n-1", None).unwrap(), 1);
        store.node_forget("n-1").unwrap();
        assert_eq!(store.node_heartbeat("n-1", None).unwrap(), 0);
    }

    /// B4: the heartbeat refreshes the load, and an unreadable load
    /// (None) PRESERVES the last sample — a momentary /proc hiccup
    /// must not read as "idle" to placement.
    #[test]
    fn a_heartbeat_carries_load_and_none_keeps_the_last_sample() {
        let (_d, store) = scratch();
        store
            .node_register("n-1", "n", "h", "0", "ephemeral", None, None, None, None)
            .unwrap();
        store.node_heartbeat("n-1", Some(42)).unwrap();
        let row = store.node_get("n-1").unwrap().unwrap();
        assert_eq!(row.load1, Some(42));
        assert!(row.load_at.is_some(), "the sample is timestamped");
        // None = unknown, not zero.
        store.node_heartbeat("n-1", None).unwrap();
        let row = store.node_get("n-1").unwrap().unwrap();
        assert_eq!(row.load1, Some(42), "unknown keeps the previous sample");
    }

    /// B4: `sessions_served` is lifetime-owned, monotone — and the
    /// bump rides only rows that actually LAND (create_session on an
    /// existing id is a resume, not a new session).
    ///
    /// The bump keys off `local_node_id()` (config dir's node.toml),
    /// so this needs a scratch HOME with the identity pinned — each
    /// test's store is its own tempdir DB, so the env override cannot
    /// contaminate a parallel test's counts.
    #[test]
    fn served_counts_only_new_sessions_and_never_resets() {
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join("node.toml"),
            "id = \"n-1\"\nname = \"n\"\n",
        )
        .unwrap();
        let store = Store::open(&home.path().join("state.db")).unwrap();
        store
            .node_register("n-1", "n", "h", "0", "ephemeral", None, None, None, None)
            .unwrap();
        assert_eq!(store.node_get("n-1").unwrap().unwrap().sessions_served, 0);
        store.create_session("s1").unwrap();
        store.create_session("s2").unwrap();
        // A resume of s1 is NOT a third session.
        store.create_session("s1").unwrap();
        assert_eq!(store.node_get("n-1").unwrap().unwrap().sessions_served, 2);
        // And the recomputable truth agrees: with an identity pinned,
        // sessions.node stamps every new row "n-1".
        assert_eq!(store.node_session_count("n-1").unwrap(), 2);
        // Re-registering (a version bump, a rename) never resets it.
        store
            .node_register(
                "n-1",
                "renamed",
                "h",
                "9",
                "always-on",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            store.node_get("n-1").unwrap().unwrap().sessions_served,
            2,
            "the counter outlives re-registration"
        );
    }

    /// `tool_usage` counts persisted tool rows by name and orders them
    /// most-used first; `tool_usage_by_day` breaks one tool out by date.
    /// This is the regression watch behind the awareness fix — the query
    /// that showed delegate_task at 8 calls and shell/monitor at 0. If the
    /// counts are wrong, the watch is worse than none: it would green-light
    /// a promotion that did nothing.
    #[test]
    fn tool_usage_counts_persisted_tool_rows() {
        let (_d, store) = scratch();
        store.create_session("tu").unwrap();
        for name in ["terminal", "terminal", "terminal", "delegate_task"] {
            let call_id = format!("c-{name}");
            store
                .append_message("tu", &ChatMessage::tool_result(&call_id, name, "ok".into()))
                .unwrap();
        }
        // An assistant row that merely MENTIONS a name is not a call.
        store
            .append_message("tu", &ChatMessage::assistant("run terminal now", None))
            .unwrap();

        let usage = store.tool_usage(None).unwrap();
        let got: std::collections::HashMap<_, _> = usage.iter().cloned().collect();
        assert_eq!(got.get("terminal").copied(), Some(3), "{usage:?}");
        assert_eq!(got.get("delegate_task").copied(), Some(1), "{usage:?}");
        // Most-used first.
        assert_eq!(usage[0].0, "terminal", "{usage:?}");

        // Both landed today; the by-day breakdown shows one row for it.
        // UTC, NOT local: `created_at` is written by sqlite's
        // `datetime('now')`, which is UTC, so `date(created_at)` buckets
        // by the UTC day. Comparing against `Local::now()` made this
        // fail every night between local-midnight and UTC-midnight
        // (00:00–02:00 CEST) — the row said the 17th while `today` had
        // already rolled to the 18th. Match the store's own clock.
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let by_day = store.tool_usage_by_day("terminal", None).unwrap();
        assert_eq!(by_day.len(), 1, "{by_day:?}");
        assert_eq!(by_day[0].0, today, "{by_day:?}");
        assert_eq!(by_day[0].1, 3, "{by_day:?}");
        // A tool nobody called has no rows — the absence IS the signal.
        assert!(store.tool_usage_by_day("monitor", None).unwrap().is_empty());
        // since_days is a window, not a filter that loses today.
        assert_eq!(
            store.tool_usage_by_day("terminal", Some(1)).unwrap().len(),
            1,
            "today is within the last 1 day"
        );
    }

    /// Resuming must return exactly the tail — and, more importantly, the
    /// SAME tail the old shape produced by loading everything and then
    /// dropping what the client had. The endpoint zips messages against
    /// `message_ids`, so the two filters have to agree row for row or
    /// every engine note pins to the wrong anchor.
    #[test]
    fn resuming_a_history_returns_exactly_what_loading_it_all_and_dropping_the_head_would() {
        let (_d, store) = scratch();
        let mut ids = Vec::new();
        for i in 0..12 {
            let body = format!("message {i}");
            let msg = if i % 2 == 0 {
                ChatMessage::user(&body)
            } else {
                ChatMessage::assistant(&body, None)
            };
            ids.push(store.append_message("s1", &msg).expect("append"));
        }
        // A second session's rows must never leak into either answer.
        store
            .append_message("s2", &ChatMessage::user("not mine"))
            .expect("append");

        let all = store.load_history("s1").expect("all");
        let all_ids = store.message_ids("s1").expect("all ids");
        assert_eq!(all.len(), 12);
        assert_eq!(all_ids, ids);

        for cut in [0usize, 1, 5, 11, 12] {
            let since = if cut == 0 { 0 } else { ids[cut - 1] };
            let tail = store.load_history_since("s1", since).expect("tail");
            let tail_ids = store.message_ids_since("s1", since).expect("tail ids");
            let expect: Vec<String> = all[cut..].iter().map(|m| m.content.clone()).collect();
            let got: Vec<String> = tail.iter().map(|m| m.content.clone()).collect();
            assert_eq!(got, expect, "since={since}");
            assert_eq!(tail_ids, ids[cut..].to_vec(), "since={since}");
            // The zip the endpoint depends on.
            assert_eq!(tail.len(), tail_ids.len(), "since={since}");
        }
    }

    /// Archived rows are invisible to `load_history`; resuming must not
    /// quietly bring them back. The two filters drifting apart is how
    /// notes ended up rendering at the bottom instead of inline once
    /// before.
    #[test]
    fn resuming_honours_the_archived_filter_the_full_load_uses() {
        let (_d, store) = scratch();
        let first = store
            .append_message("s1", &ChatMessage::user("old one"))
            .expect("append");
        store
            .append_message("s1", &ChatMessage::user("old two"))
            .expect("append");
        store
            .append_message("s1", &ChatMessage::user("new"))
            .expect("append");
        store
            .conn
            .execute(
                "UPDATE messages SET archived = 1 WHERE content = 'old two'",
                [],
            )
            .expect("archive");
        assert_eq!(store.load_history("s1").expect("all").len(), 2);
        let tail = store.load_history_since("s1", first).expect("tail");
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].content, "new");
        assert_eq!(
            store.message_ids_since("s1", first).expect("ids").len(),
            tail.len()
        );
    }

    // ── load-time tool-round repair ──────────────────────────────────

    /// An assistant row carrying one tool call, the way run_turn writes it.
    fn call_row(id: &str) -> ChatMessage {
        let mut m = ChatMessage::assistant("", None);
        m.tool_calls = Some(serde_json::json!([{
            "id": id,
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        }]));
        m
    }

    fn result_row(id: &str, content: &str) -> ChatMessage {
        let mut m = ChatMessage {
            role: "tool".into(),
            content: content.into(),
            reasoning: None,
            display_reasoning: None,
            tool_calls: None,
            tool_meta: None,
            attachments: Vec::new(),
            injection: None,
            msg_id: None,
        };
        m.tool_meta = Some(crate::providers::ToolMeta {
            tool_call_id: id.into(),
            name: "read_file".into(),
        });
        m
    }

    /// THE BUG that bricked session 20260926_020336_18bd3e: a forced
    /// /compact cut a giant single turn at a raw index and archive_head
    /// archived the assistant row while its tool result stayed live. From
    /// then on EVERY request opened with an orphaned tool result and the
    /// provider answered `400 tool_call_id is not found` — permanently.
    /// load_history must never hand that shape to a turn again.
    #[test]
    fn load_history_drops_an_orphaned_tool_result() {
        let (_d, store) = scratch();
        store
            .append_message("s1", &ChatMessage::user("do the thing"))
            .expect("user");
        // A healthy round, then the corruption: the assistant that
        // declared call-B gets archived, its result stays live.
        store
            .append_message("s1", &call_row("call_A"))
            .expect("call A");
        store
            .append_message("s1", &result_row("call_A", "a-output"))
            .expect("result A");
        let orphan_declarer = store
            .append_message("s1", &call_row("call_B"))
            .expect("call B");
        store
            .append_message("s1", &result_row("call_B", "b-output"))
            .expect("result B");
        store
            .append_message("s1", &call_row("call_C"))
            .expect("call C");
        store
            .append_message("s1", &result_row("call_C", "c-output"))
            .expect("result C");
        store
            .archive_messages_by_id("s1", &[orphan_declarer])
            .expect("archive the declaring assistant");

        let h = store.load_history("s1").expect("load");
        // The orphaned result row is gone from the LIVE set…
        assert!(
            !h.iter().any(|m| m.content == "b-output"),
            "orphaned tool result must not reach the wire: {h:?}"
        );
        // …and everything still paired: every tool row answers a call a
        // live assistant declares, every declared call has its result.
        let mut declared = std::collections::HashSet::new();
        let mut answered = std::collections::HashSet::new();
        for m in &h {
            if let Some(serde_json::Value::Array(calls)) = &m.tool_calls {
                for c in calls {
                    declared.insert(c.get("id").unwrap().as_str().unwrap().to_string());
                }
            }
            if let Some(meta) = &m.tool_meta {
                answered.insert(meta.tool_call_id.clone());
            }
        }
        assert_eq!(declared, answered, "call/result sets must match");
        // The row itself is NOT deleted — lineage, FTS-searchable.
        let total: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id='s1'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(total, 7, "repair must not delete rows, only hide them");
    }

    /// The mirror image: a turn interrupted after the assistant row was
    /// persisted but before its tool result landed (crash, cancel). The
    /// live assistant declares a call no live row answers — also a 400.
    /// The repair strips the dangling call and keeps the assistant's text.
    #[test]
    fn load_history_strips_an_unanswered_tool_call() {
        let (_d, store) = scratch();
        store
            .append_message("s1", &ChatMessage::user("run it"))
            .expect("user");
        let mut last_words = call_row("call_D");
        last_words.content = "let me check that".into();
        store.append_message("s1", &last_words).expect("assistant");
        // No result row: the process died between the two persists.

        let h = store.load_history("s1").expect("load");
        assert_eq!(h.len(), 2, "user + repaired assistant");
        assert_eq!(h[1].role, "assistant");
        assert!(
            h[1].tool_calls.is_none(),
            "the unanswered call must be stripped: {:?}",
            h[1].tool_calls
        );
        assert_eq!(h[1].content, "let me check that");
    }

    /// A healthy history passes through untouched — the repair is not a
    /// rewrite, it is a no-op on anything already legal.
    #[test]
    fn load_history_leaves_a_paired_history_alone() {
        let (_d, store) = scratch();
        store
            .append_message("s1", &ChatMessage::user("go"))
            .expect("user");
        store
            .append_message("s1", &call_row("call_E"))
            .expect("call");
        store
            .append_message("s1", &result_row("call_E", "fine"))
            .expect("result");
        let h = store.load_history("s1").expect("load");
        assert_eq!(h.len(), 3);
        assert!(h[1].tool_calls.is_some(), "answered call stays declared");
        assert_eq!(h[2].content, "fine");
    }

    /// The repair judges pairing, and pairing can only be judged on the
    /// WHOLE live set. A `since` window cuts rounds in half by design —
    /// the desktop reconnects with `?since=<last seen>`, and the
    /// child-session follow tick reads exactly the gap past its cursor —
    /// so a result whose call sits at or below the cursor looked
    /// "orphaned" to a windowed repair and was dropped. The tick then
    /// moved its cursor past it, and the client never saw that result.
    #[test]
    fn a_since_window_never_eats_a_result_whose_call_is_behind_the_cursor() {
        let (_d, store) = scratch();
        store
            .append_message("s1", &ChatMessage::user("go"))
            .expect("user");
        let call = store
            .append_message("s1", &call_row("call_W"))
            .expect("call");
        store
            .append_message("s1", &result_row("call_W", "the result"))
            .expect("result");
        let tail = store.load_history_since("s1", call).expect("tail");
        assert_eq!(tail.len(), 1, "the row past the cursor is served: {tail:?}");
        assert_eq!(tail[0].content, "the result");
        // And a call whose result has not landed YET (a turn in flight)
        // keeps its tool_calls in the window — they carry the arguments
        // the client renders.
        let (_d2, store2) = scratch();
        let user = store2
            .append_message("s2", &ChatMessage::user("go"))
            .expect("user");
        store2
            .append_message("s2", &call_row("call_X"))
            .expect("call in flight");
        let tail = store2.load_history_since("s2", user).expect("tail");
        assert!(
            tail[0].tool_calls.is_some(),
            "an in-flight call is not stripped from a window"
        );
    }

    /// `history_rows` zips `load_history_since` against
    /// `message_ids_since`, and the TUI zips the full load against
    /// `message_ids`/`message_times`. A row the repair hides from the
    /// WIRE must not vanish from those display vectors, or every note
    /// and timestamp after it shifts by one.
    #[test]
    fn the_display_loader_stays_aligned_with_the_id_and_time_vectors() {
        let (_d, store) = scratch();
        store
            .append_message("s1", &ChatMessage::user("go"))
            .expect("user");
        // An orphan: a result no live assistant declared.
        store
            .append_message("s1", &result_row("call_gone", "orphan"))
            .expect("orphan");
        store
            .append_message("s1", &ChatMessage::assistant("answer", None))
            .expect("answer");
        let raw = store.load_history_since("s1", 0).expect("raw");
        let ids = store.message_ids("s1").expect("ids");
        let times = store.message_times("s1").expect("times");
        assert_eq!(raw.len(), ids.len(), "display rows zip with ids");
        assert_eq!(raw.len(), times.len(), "display rows zip with times");
        for (m, id) in raw.iter().zip(&ids) {
            assert_eq!(m.msg_id, Some(*id), "row i IS id i");
        }
        // The wire view is still repaired.
        let wire = store.load_history("s1").expect("wire");
        assert!(!wire.iter().any(|m| m.content == "orphan"));
    }

    /// Compaction's archive is exact under the shape a SECOND compaction
    /// sees: memory holds [marker, tail…] while the store has the marker
    /// as its NEWEST row. A head of [marker, oldest tail row] must
    /// archive those two plus anything older that memory never held —
    /// and nothing the tail kept.
    #[test]
    fn compacted_head_archive_is_by_id_and_below_the_kept_tail() {
        let (_d, store) = scratch();
        let ids: Vec<i64> = ["hidden", "t1", "t2", "t3", "t4", "marker"]
            .iter()
            .map(|c| {
                store
                    .append_message("s1", &ChatMessage::user(c))
                    .expect("append")
            })
            .collect();
        // Memory: [marker, t1 | t2, t3, t4]; "hidden" is a row the
        // repair kept out of memory. Head = marker + t1, tail from t2.
        let n = store
            .archive_compacted_head("s1", &[ids[5], ids[1]], ids[2])
            .expect("archive");
        assert_eq!(n, 3, "marker, t1, and the hidden row below the tail");
        let live: Vec<String> = store
            .load_history_since("s1", 0)
            .expect("live")
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(live, vec!["t2", "t3", "t4"]);
    }

    // ── IRC scrollback ───────────────────────────────────────────────

    fn line(store: &Store, buffer: &str, nick: &str, text: &str) -> i64 {
        store
            .irc_append(
                "irc.example",
                "bot",
                buffer,
                "2026-08-25T00:00:00Z",
                "privmsg",
                nick,
                None,
                text,
                false,
                0,
                false,
            )
            .expect("append")
    }

    #[test]
    fn scrollback_reads_back_oldest_first() {
        let (_d, store) = scratch();
        for i in 0..5 {
            line(&store, "#veles", "wrath", &format!("line {i}"));
        }
        let got = store
            .irc_backlog("irc.example", "bot", "#veles", None, 50)
            .unwrap();
        let texts: Vec<&str> = got.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["line 0", "line 1", "line 2", "line 3", "line 4"]);
    }

    #[test]
    fn irc_search_finds_lines_case_insensitively_and_oldest_first() {
        let (_d, store) = scratch();
        line(&store, "#veles", "wrath", "the BUILD broke");
        line(&store, "#veles", "erasmus", "it built fine locally");
        // The admin presence is one conversation, not a stranger.
        store
            .irc_append(
                "irc.example",
                "admin",
                "#veles",
                "2026-08-25T00:00:00Z",
                "privmsg",
                "wrath",
                None,
                "ANOTHER build on staging",
                false,
                0,
                false,
            )
            .unwrap();
        let got = store
            .irc_search("irc.example", "#veles", "build", 50)
            .unwrap();
        let texts: Vec<&str> = got.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            ["the BUILD broke", "ANOTHER build on staging"],
            "case-insensitive, both presences, oldest first"
        );
        assert_eq!(got[0].nick, "wrath");
        assert_eq!(got[1].presence, "admin");
        assert_eq!(got[0].ts, "2026-08-25T00:00:00Z");
    }

    #[test]
    fn irc_search_stays_in_its_buffer_and_respects_the_cap() {
        let (_d, store) = scratch();
        for i in 0..5 {
            line(&store, "#a", "wrath", &format!("needle {i}"));
            line(&store, "#b", "erasmus", &format!("needle {i}"));
        }
        let got = store.irc_search("irc.example", "#a", "needle", 2).unwrap();
        assert_eq!(got.len(), 2, "the cap is the match count");
        // Newest two (4, 3), handed back oldest first.
        assert_eq!(
            got.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["needle 3", "needle 4"]
        );
        let other = store.irc_search("irc.example", "#b", "needle", 50).unwrap();
        assert_eq!(other.len(), 5, "#b's lines are found in #b, not in #a");
    }

    #[test]
    fn irc_query_filters_on_nick_time_and_scope() {
        // "One buffer, substring, cap" could not answer the questions
        // that come up working alongside people and bots in a channel:
        // what did erasmus say, what did I miss since 09:00, what were
        // the last N lines here.
        let (_d, store) = scratch();
        let at = |buffer: &str, ts: &str, nick: &str, text: &str| {
            store
                .irc_append(
                    "irc.example",
                    "bot",
                    buffer,
                    ts,
                    "privmsg",
                    nick,
                    None,
                    text,
                    false,
                    0,
                    false,
                )
                .expect("append");
        };
        at("#a", "2026-09-09T08:00:00Z", "wrath", "morning deploy");
        at("#a", "2026-09-09T10:00:00Z", "erasmus", "deploy is frozen");
        at("#a", "2026-09-09T12:00:00Z", "erasmus", "unfrozen now");
        at("#b", "2026-09-09T11:00:00Z", "erasmus", "deploy elsewhere");

        let texts = |q: &IrcQuery| -> Vec<String> {
            store
                .irc_query(q)
                .expect("query")
                .into_iter()
                .map(|l| l.text)
                .collect()
        };
        let base = IrcQuery {
            network: "irc.example".into(),
            limit: 50,
            ..IrcQuery::default()
        };

        assert_eq!(
            texts(&IrcQuery {
                buffer: Some("#a".into()),
                nick: Some("ERASMUS".into()),
                ..base.clone()
            }),
            vec!["deploy is frozen".to_string(), "unfrozen now".to_string()],
            "a nick filter, case-insensitively, inside one buffer"
        );
        assert_eq!(
            texts(&IrcQuery {
                buffer: Some("#a".into()),
                since: Some("2026-09-09T09".into()),
                until: Some("2026-09-09T12:00:00Z".into()),
                ..base.clone()
            }),
            vec!["deploy is frozen".to_string()],
            "since is inclusive, until is exclusive, and a prefix is a legal bound"
        );
        assert_eq!(
            texts(&IrcQuery {
                needle: Some("deploy".into()),
                ..base.clone()
            })
            .len(),
            3,
            "no buffer searches every buffer on the network"
        );
        assert_eq!(
            texts(&IrcQuery {
                buffer: Some("#a".into()),
                limit: 2,
                ..base.clone()
            }),
            vec!["deploy is frozen".to_string(), "unfrozen now".to_string()],
            "no needle is a tail: the newest `limit` lines, whatever they say"
        );
        assert!(
            texts(&IrcQuery {
                buffer: Some("#a".into()),
                nick: Some("nobody".into()),
                ..base.clone()
            })
            .is_empty(),
            "no match is an empty answer, not an error"
        );
        assert_eq!(
            store
                .irc_query(&IrcQuery {
                    buffer: Some("#a".into()),
                    limit: 0,
                    ..base
                })
                .expect("query")
                .len(),
            1,
            "a zero limit is clamped to one rather than becoming `no rows`"
        );
    }

    #[test]
    fn irc_search_treats_like_metacharacters_literally() {
        let (_d, store) = scratch();
        line(&store, "#veles", "wrath", "100% done");
        line(&store, "#veles", "erasmus", "snake_case_here");
        assert_eq!(
            store
                .irc_search("irc.example", "#veles", "100%", 50)
                .unwrap()
                .len(),
            1,
            "a percent in the needle is a literal, not a wildcard"
        );
        assert_eq!(
            store
                .irc_search("irc.example", "#veles", "case_here", 50)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .irc_search("irc.example", "#veles", "nothing", 50)
                .unwrap()
                .len(),
            0,
            "no match is an empty answer, not an error"
        );
    }

    #[test]
    fn a_page_is_the_lines_above_where_you_are() {
        // Scrollback is only ever read backwards. Handing back the
        // OLDEST n would give the start of the channel's history when
        // what was asked for is the screenful above the current view.
        let (_d, store) = scratch();
        let mut ids = Vec::new();
        for i in 0..10 {
            ids.push(line(&store, "#veles", "wrath", &format!("line {i}")));
        }
        let newest = store
            .irc_backlog("irc.example", "bot", "#veles", None, 3)
            .unwrap();
        assert_eq!(
            newest.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["line 7", "line 8", "line 9"],
            "no cursor means the newest page"
        );

        // Scroll up from line 7.
        let older = store
            .irc_backlog("irc.example", "bot", "#veles", Some(ids[7]), 3)
            .unwrap();
        assert_eq!(
            older.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["line 4", "line 5", "line 6"],
            "a page is the lines immediately above the cursor"
        );
    }

    #[test]
    fn buffers_do_not_bleed_into_each_other() {
        let (_d, store) = scratch();
        line(&store, "#veles", "wrath", "in veles");
        line(&store, "#dev", "nyx", "in dev");
        line(&store, "wrath", "wrath", "a private word");

        let veles = store
            .irc_backlog("irc.example", "bot", "#veles", None, 50)
            .unwrap();
        assert_eq!(veles.len(), 1);
        assert_eq!(veles[0].text, "in veles");

        let mut names = store.irc_buffers("irc.example", "bot").unwrap();
        names.sort();
        assert_eq!(names, vec!["#dev", "#veles", "wrath"]);

        // A different presence on the same network is a different set
        // of buffers — the admin's #veles is not the bot's #veles.
        assert!(store
            .irc_buffers("irc.example", "admin")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn since_replays_across_every_buffer_in_order() {
        // This is the reconnect path: "everything I missed", not
        // "everything in this one channel".
        let (_d, store) = scratch();
        let a = line(&store, "#veles", "wrath", "one");
        line(&store, "#dev", "nyx", "two");
        line(&store, "wrath", "wrath", "three");

        let missed = store.irc_since(a, 50).unwrap();
        assert_eq!(
            missed.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["two", "three"],
            "strictly after the cursor, across buffers"
        );
        assert_eq!(store.irc_since(0, 50).unwrap().len(), 3, "from the start");
        assert!(store.irc_since(i64::MAX, 50).unwrap().is_empty());
    }

    #[test]
    fn latest_id_is_where_a_fresh_client_starts() {
        let (_d, store) = scratch();
        assert_eq!(store.irc_latest_id().unwrap(), 0, "empty means zero");
        let id = line(&store, "#veles", "wrath", "hi");
        assert_eq!(store.irc_latest_id().unwrap(), id);
    }

    #[test]
    fn fields_survive_the_round_trip() {
        let (_d, store) = scratch();
        store
            .irc_append(
                "irc.example",
                "admin",
                "#veles",
                "2026-08-25T01:02:03Z",
                "action",
                "wrath",
                Some("wrath_acct"),
                "waves",
                true,
                42,
                false,
            )
            .unwrap();
        let got = store
            .irc_backlog("irc.example", "admin", "#veles", None, 1)
            .unwrap();
        let l = &got[0];
        assert_eq!(l.presence, "admin");
        assert_eq!(l.ts, "2026-08-25T01:02:03Z", "server-time is kept verbatim");
        assert_eq!(l.kind, "action");
        assert_eq!(l.account, "wrath_acct");
        assert!(l.own, "a line we sent stays marked as ours");
    }

    #[test]
    fn pruning_is_per_buffer_so_a_busy_channel_cannot_evict_a_quiet_one() {
        // A global cap would let one loud channel push every other
        // channel's history out — and the quiet one is usually the
        // history you wanted.
        let (_d, store) = scratch();
        for i in 0..50 {
            line(&store, "#busy", "loud", &format!("noise {i}"));
        }
        line(&store, "#quiet", "wrath", "the one line that mattered");

        store.irc_prune(5, 0).unwrap();

        let busy = store
            .irc_backlog("irc.example", "bot", "#busy", None, 100)
            .unwrap();
        assert_eq!(busy.len(), 5, "the busy channel is trimmed");
        assert_eq!(
            busy.last().unwrap().text,
            "noise 49",
            "and it keeps the NEWEST lines"
        );

        let quiet = store
            .irc_backlog("irc.example", "bot", "#quiet", None, 100)
            .unwrap();
        assert_eq!(quiet.len(), 1, "the quiet channel is untouched");
    }

    #[test]
    fn pruning_by_age_drops_old_lines_and_keeps_recent_ones() {
        let (_d, store) = scratch();
        store
            .irc_append(
                "irc.example",
                "bot",
                "#veles",
                "2020-01-01T00:00:00Z",
                "privmsg",
                "wrath",
                None,
                "ancient",
                false,
                0,
                false,
            )
            .unwrap();
        store
            .irc_append(
                "irc.example",
                "bot",
                "#veles",
                "2099-01-01T00:00:00Z",
                "privmsg",
                "wrath",
                None,
                "fresh",
                false,
                0,
                false,
            )
            .unwrap();

        store.irc_prune(0, 30).unwrap();
        let left = store
            .irc_backlog("irc.example", "bot", "#veles", None, 50)
            .unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].text, "fresh");
    }

    #[test]
    fn a_zero_limit_prunes_nothing() {
        // Both knobs off must mean "keep everything", not "delete
        // everything" — a config that forgets to set them cannot be
        // allowed to wipe the scrollback.
        let (_d, store) = scratch();
        for i in 0..5 {
            line(&store, "#veles", "wrath", &format!("line {i}"));
        }
        store.irc_prune(0, 0).unwrap();
        assert_eq!(
            store
                .irc_backlog("irc.example", "bot", "#veles", None, 50)
                .unwrap()
                .len(),
            5
        );
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: content.into(),
            reasoning: None,
            display_reasoning: None,
            tool_calls: None,
            tool_meta: None,
            attachments: Vec::new(),
            injection: None,
            msg_id: None,
        }
    }

    fn table_names(store: &Store) -> Vec<String> {
        let mut stmt = store
            .conn()
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    // ── schema ──────────────────────────────────────────────────────

    /// The liveness registry round trip (R4.5): a run row appears with
    /// the process facts, survives a heartbeat, and is gone on end —
    /// and a row that went SILENT is reaped by the reader, which is
    /// the half that keeps a crashed process from freezing a session
    /// as "running" forever.
    #[test]
    fn run_registry_round_trips_and_reaps_silent_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();

        store.run_start("live", 42, "box", "tui").unwrap();
        store.run_start("crashed", 7, "box", "gateway").unwrap();
        // The crashed one died 2 minutes ago: backdate its heartbeat
        // the way elapsed time would have.
        store
            .conn
            .execute(
                "UPDATE session_runs SET heartbeat_at = datetime('now', '-120 seconds')
                 WHERE session_id = 'crashed'",
                [],
            )
            .unwrap();
        // A fresh beat for the live one (the heartbeat thread's work).
        store.run_heartbeat("live").unwrap();

        let live = store.runs_live(30).unwrap();
        assert!(
            live.iter().any(|r| r.session_id == "live"),
            "the live turn must be reported: {live:?}"
        );
        assert!(
            !live.iter().any(|r| r.session_id == "crashed"),
            "a silent row must be reaped, not reported: {live:?}"
        );
        let row = live.iter().find(|r| r.session_id == "live").unwrap();
        assert_eq!(row.pid, 42);
        assert_eq!(row.surface, "tui");

        // End clears it — the TurnGuard path.
        store.run_end("live").unwrap();
        assert!(
            store.runs_live(30).unwrap().is_empty(),
            "an ended turn must deregister"
        );
    }

    /// The override row is a full-replace snapshot: a second write with
    /// NULLs CLEARS what the first wrote, because the writer's
    /// in-memory state is the truth. A COALESCE merge here is how a
    /// cleared model gets resurrected by the next surface to write.
    #[test]
    fn override_row_replaces_fully_and_clears_on_null() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();

        store
            .override_replace("s1", Some("p"), Some("m"), Some("high"))
            .unwrap();
        let row = store.override_get("s1").unwrap().unwrap();
        assert_eq!(
            row,
            SessionOverride {
                provider: Some("p".into()),
                model: Some("m".into()),
                reasoning: Some("high".into())
            }
        );

        store.override_replace("s1", None, None, None).unwrap();
        let row = store.override_get("s1").unwrap().unwrap();
        assert_eq!(
            row,
            SessionOverride::default(),
            "NULL must clear, not keep the old value"
        );
    }

    /// A take while ANOTHER connection is mid-write must wait for it and
    /// then take — not fail. The take used to open a DEFERRED transaction:
    /// it read under a snapshot taken while the other writer held the
    /// lock, then could not promote that stale reader to a writer
    /// ("cannot promote read transaction", 517) and failed at once; every
    /// drainer read the error as "nothing queued". Live as session mail
    /// stuck `queued` (2026-10-11): the sender's waiting-flag write landed
    /// between the target's read and its write. `write_tx` takes the lock
    /// at BEGIN, so the busy handler waits the other writer out.
    #[test]
    fn a_take_waits_out_a_concurrent_writer_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.db");
        let a = Store::open(&path).unwrap();
        a.create_session("race").unwrap();
        a.queue_push("race", "parked").unwrap();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let p2 = path.clone();
        let writer = std::thread::spawn(move || {
            let b = Store::open(&p2).unwrap();
            b.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
            b.conn()
                .execute(
                    "INSERT OR REPLACE INTO meta(key, value) VALUES ('race', '1')",
                    [],
                )
                .unwrap();
            held_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            b.conn().execute_batch("COMMIT").unwrap();
        });
        held_rx.recv().unwrap();
        let item = a
            .queue_take_next("race")
            .expect("the take waits for the other writer instead of failing");
        assert_eq!(item.map(|i| i.body).as_deref(), Some("parked"));
        writer.join().unwrap();
        // The same for a push behind a writer.
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let p3 = path.clone();
        let writer = std::thread::spawn(move || {
            let b = Store::open(&p3).unwrap();
            b.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
            b.conn()
                .execute(
                    "INSERT OR REPLACE INTO meta(key, value) VALUES ('race', '2')",
                    [],
                )
                .unwrap();
            held_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            b.conn().execute_batch("COMMIT").unwrap();
        });
        held_rx.recv().unwrap();
        a.queue_push("race", "second")
            .expect("a push waits for the other writer instead of failing");
        writer.join().unwrap();
    }

    /// The queue round trip (R4.6): push lands `pending` with a
    /// per-session seq; patch and delete touch ONLY pending rows; take
    /// promotes to `running` in FIFO order; and a `running` row whose
    /// turn never got a live run row is demoted and re-offered — the
    /// recovery a queue depends on when its drainer crashes between
    /// take and run_start.
    #[test]
    fn queue_round_trip_and_stalled_running_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("q.db")).unwrap();

        assert_eq!(store.queue_push("s1", "first").unwrap(), 1);
        assert_eq!(store.queue_push("s1", "second").unwrap(), 2);
        assert_eq!(
            store.queue_push("s2", "other session").unwrap(),
            1,
            "seq is per-session, not global"
        );

        let items = store.queue_items("s1").unwrap();
        assert_eq!(items.len(), 2);
        assert!(
            items.iter().all(|i| i.state == "pending"),
            "a push is pending, never self-promoted: {items:?}"
        );

        // Patch is pending-only and rewrites the body.
        assert!(store.queue_patch("s1", 1, "first edited").unwrap());
        assert_eq!(store.queue_items("s1").unwrap()[0].body, "first edited");

        // Take: FIFO, and the row becomes `running` so no second
        // drainer takes the same message.
        let t = store.queue_take_next("s1").unwrap().expect("first take");
        assert_eq!(t.seq, 1, "FIFO: the oldest pending row goes first");
        assert_eq!(
            store.queue_items("s1").unwrap()[0].state,
            "running",
            "a taken row is running, not pending"
        );

        // A running row cannot be silently edited away — its turn IS it.
        assert!(!store.queue_patch("s1", 1, "nope").unwrap());
        assert!(!store.queue_delete("s1", 1).unwrap());

        // RECOVERY: no live run row (the taker crashed, or never wrote
        // one) → the running row is demoted and RE-OFFERED in order,
        // not skipped into oblivion.
        let t = store
            .queue_take_next("s1")
            .unwrap()
            .expect("recovered take");
        assert_eq!(t.seq, 1, "recovery demotes; it must not skip");

        // With a LIVE run row the running item belongs to the turn in
        // flight and the drainer takes the next pending one instead.
        store.run_start("s1", 11, "box", "tui").unwrap();
        store.run_heartbeat("s1").unwrap();
        let t = store.queue_take_next("s1").unwrap().expect("second take");
        assert_eq!(t.seq, 2, "a live turn's running row must not be re-taken");
        let both = store.queue_items("s1").unwrap();
        assert!(
            both[0].state == "running" && both[1].state == "running",
            "both taken rows are running while the turn is live"
        );

        // Cancelling a pending row deletes it outright.
        assert!(store.queue_delete("s2", 1).unwrap());
        assert!(store.queue_items("s2").unwrap().is_empty());
    }

    /// THE FLOOD TEST (2026-09-08): a queue row whose turn COMPLETED must
    /// be released, or the next drain reads it as a crashed taker and
    /// re-runs the same message forever. One keystroke produced ~100
    /// duplicate turns in production. The demote-recovery above stays
    /// correct ONLY because completed rows are deleted before the take
    /// that would misread them.
    #[test]
    fn released_row_is_not_re_offered_but_abandoned_one_is() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();

        store.queue_push("flood", "make it 6am").unwrap();

        // A full turn lifecycle, exactly as the drainers run it: take →
        // run row appears → turn ends (run row cleared) → release.
        let t = store.queue_take_next("flood").unwrap().expect("take");
        store.run_start("flood", 7, "box", "desktop").unwrap();
        store.run_heartbeat("flood").unwrap();
        store.run_end("flood").unwrap(); // what turn_ended does
        assert!(
            store.queue_release("flood", t.seq).unwrap(),
            "release eats the completed row"
        );

        // The drain boundary's next take — this returned Some(seq 1)
        // before the fix and drove the flood.
        assert!(
            store.queue_take_next("flood").unwrap().is_none(),
            "a released row must never come back"
        );

        // Release is running-only and idempotent-ish: a second call, and
        // one against a pending row, both report nothing done.
        assert!(!store.queue_release("flood", t.seq).unwrap());
        let seq = store.queue_push("flood", "pending never releases").unwrap();
        assert!(!store.queue_release("flood", seq).unwrap());

        // An ABANDONED row (crashed taker: taken, run row reaped, no
        // release ever called) is still recovered — the fix must not
        // turn the flood back into a stranded queue.
        let t2 = store
            .queue_take_next("flood")
            .unwrap()
            .expect("take the pending row");
        store.run_start("flood", 7, "box", "desktop").unwrap();
        store.run_end("flood").unwrap();
        // NOTE: deliberately NO queue_release here — this is a crash.
        let recovered = store
            .queue_take_next("flood")
            .unwrap()
            .expect("crashed row is re-offered");
        assert_eq!(
            recovered.seq, t2.seq,
            "recovery still demotes abandoned rows"
        );
    }

    /// DURABLE STEERS (PENDING 2026-09-14, t5): a steer parked while no
    /// turn was live used to sit in a process-global Vec — gone on
    /// gateway restart, invisible to every surface's queue view. Now a
    /// parked steer IS a `session_queue` row with kind='steer': it
    /// survives reopen, rides the existing session delete, and the
    /// drainer NEVER mistakes it for a turn to run (a steer injected as
    /// a fresh user message would be a duplicate turn — the exact
    /// double-delivery seen live when one steer landed twice).
    #[test]
    fn steer_rows_are_durable_and_never_drained_as_turns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let store = Store::open(&path).unwrap();
            // A steer docks while idle, a message queues behind it.
            assert_eq!(store.queue_push_kind("s1", "go left", "steer").unwrap(), 1);
            assert_eq!(store.queue_push("s1", "a real message").unwrap(), 2);

            // The queue LIST shows both — one queue, every surface.
            let items = store.queue_items("s1").unwrap();
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].kind.as_deref(), Some("steer"));
            assert_eq!(items[1].kind.as_deref(), Some("message"));

            // The drainer takes the MESSAGE only; the steer row is not
            // a turn and must not be offered as one.
            let t = store.queue_take_next("s1").unwrap().expect("message taken");
            assert_eq!(t.body, "a real message");
            assert_eq!(t.kind.as_deref(), Some("message"));
            let left = store.queue_items("s1").unwrap();
            assert!(
                left.iter().any(|i| i.kind.as_deref() == Some("steer")),
                "the steer row survives the drain untouched: {left:?}"
            );

            // Dock-take: the steer rows leave in FIFO order, by kind.
            assert_eq!(store.take_steers("s1").unwrap(), vec!["go left"]);
            assert!(store.take_steers("s1").unwrap().is_empty());
        }
        // REOPEN: the store is the only state — a parked steer written
        // before a restart must still be there. (The in-memory dock
        // died with the process; the row must not.)
        {
            let store = Store::open(&path).unwrap();
            store
                .queue_push_kind("s1", "docked pre-crash", "steer")
                .unwrap();
            assert_eq!(
                store.take_steers("s1").unwrap(),
                vec!["docked pre-crash"],
                "a steer parked before a restart is still parked after it"
            );
        }

        // Migration: a pre-kind DB (column absent) upgrades in place
        // and its existing rows read as messages. The veles-ownership
        // marker is what gates the migration block — a foreign DB
        // (no marker) is read-only by design — so build the legacy
        // shape the way a veles-owned DB would have looked.
        {
            let dir2 = tempfile::tempdir().unwrap();
            let p2 = dir2.path().join("old.db");
            let raw = rusqlite::Connection::open(&p2).unwrap();
            raw.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE session_queue (
                    session_id TEXT NOT NULL, seq INTEGER NOT NULL,
                    body TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'pending',
                    queued_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (session_id, seq));",
            )
            .unwrap();
            raw.execute(
                "INSERT INTO meta (key, value) VALUES ('veles_schema_version', '1')",
                [],
            )
            .unwrap();
            raw.execute(
                "INSERT INTO session_queue (session_id, seq, body) VALUES ('old', 1, 'legacy')",
                [],
            )
            .unwrap();
            drop(raw);
            let store = Store::open(&p2).unwrap();
            let items = store.queue_items("old").unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(
                items[0].kind.as_deref(),
                Some("message"),
                "a legacy row upgrades to kind=message"
            );
            // The same ALTER path adds the threading target, and a row
            // that parked before the column existed reads as NULL: its
            // reply then falls back to the session's own chat, which is
            // what such a row did before. Pinning it here is the point of
            // building the legacy shape by hand — a positional read, or a
            // migration that forgot these two columns, fails on a real
            // upgraded database rather than only on a fresh one.
            assert_eq!(
                items[0].reply_to, None,
                "a legacy row carries no threading target"
            );
            assert_eq!(items[0].route, None, "and no adapter instance");
            assert!(
                !items[0].queued_at.is_empty(),
                "the timestamp column was not shifted into reply_to by a positional read"
            );
        }
    }

    #[test]
    fn open_creates_every_table_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        let store = Store::open(&path).expect("first open");
        let first = table_names(&store);
        for expected in [
            "sessions",
            "messages",
            "memories",
            "usage",
            "meta",
            "inbound_ledger",
            "cron_jobs",
            "cron_runs",
            "approval_rules",
            "approvals",
            "todos",
            "checkpoints",
            "session_queue",
        ] {
            assert!(
                first.iter().any(|t| t == expected),
                "missing table {expected}; got {first:?}"
            );
        }
        drop(store);

        // Re-opening an existing DB must not fail or change the schema:
        // every CREATE is IF NOT EXISTS, and migrations must be re-runnable.
        let store = Store::open(&path).expect("second open");
        assert_eq!(first, table_names(&store), "reopen changed the schema");
    }

    /// The migration's check-then-ALTER was a race: two opens of the same
    /// fresh DB both passed the `pragma_table_info` check and both ALTERed,
    /// and the second failed `duplicate column name` (seen once on a fresh
    /// state.db). Now a duplicate from a racer is a no-op. Two threads open
    /// one fresh DB at the same moment: before the fix the loser errored,
    /// after it both succeed and the schema is complete. (A bigger hammer
    /// trips the 5s busy_timeout on WAL writers — a separate, already-pinned
    /// property — not the column race this is about.)
    #[test]
    fn concurrent_opens_do_not_race_the_column_migrations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut handles: Vec<JoinHandle<Result<(), String>>> = Vec::new();
        for _ in 0..2 {
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                Store::open(&p).map_err(|e| format!("{e:#}"))?;
                Ok(())
            }));
        }
        for h in handles {
            h.join()
                .expect("open thread panicked")
                .expect("an open raced the column migrations");
        }
        // The schema is whole: one more open confirms a usable store.
        let store = Store::open(&path).expect("post-race open");
        assert!(table_names(&store).iter().any(|t| t == "session_queue"));
    }

    /// An open that dies after creating `messages` must not leave a file
    /// every later open reads as FOREIGN. The ownership marker used to be
    /// written after the schema batch, so a failure in between (the
    /// fresh-file race's "database is locked", a crash) left `messages`
    /// with no marker: the next open, `open`'s own retry included, saw a
    /// store it did not create and withheld every marker-gated table for
    /// the life of the install. The obstacle here fails the batch on the
    /// statement right after `CREATE TABLE messages`.
    #[test]
    fn an_open_that_dies_after_messages_still_leaves_a_store_veles_owns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE idx_messages_session (x);")
                .unwrap();
        }
        assert!(
            Store::open(&path).is_err(),
            "the obstacle fails the first open"
        );
        {
            let c = Connection::open(&path).unwrap();
            let messages: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(messages, 1, "the failed open got as far as messages");
            c.execute_batch("DROP TABLE idx_messages_session;").unwrap();
        }
        let store = Store::open(&path).expect("the second open");
        assert!(
            table_names(&store).iter().any(|t| t == "nodes"),
            "the second open owns the store (the marker-gated tables exist): {:?}",
            table_names(&store)
        );
    }

    /// The race helper directly: a duplicate column name from a racer is a
    /// no-op returning false (the caller's backfill must not re-run), while
    /// the column genuinely absent is added and reports true.
    #[test]
    fn add_column_if_missing_tolerates_a_racers_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = Store::open(&path).unwrap();
        let conn = store.conn();
        // First add: the column is absent, added, reports true.
        assert!(
            add_column_if_missing(
                conn,
                "sessions",
                "race_probe",
                "ALTER TABLE sessions ADD COLUMN race_probe TEXT;",
            )
            .unwrap(),
            "a genuinely new column is added"
        );
        // Second add (the racer): duplicate column name → no-op, false.
        assert!(
            !add_column_if_missing(
                conn,
                "sessions",
                "race_probe",
                "ALTER TABLE sessions ADD COLUMN race_probe TEXT;",
            )
            .unwrap(),
            "a column already there is a no-op"
        );
        // The racer proper: our check saw the column absent, and by our
        // ALTER another opener had added it. The check here asks about a
        // name that is absent while the DDL adds one that exists — the
        // interleaving the race produces, which the pre-check above can
        // never reach.
        assert!(
            !add_column_if_missing(
                conn,
                "sessions",
                "race_probe_unseen",
                "ALTER TABLE sessions ADD COLUMN race_probe TEXT;",
            )
            .unwrap(),
            "a racer's duplicate is a no-op, not an error"
        );
        // A real error (no such table) still propagates, not swallowed.
        assert!(
            add_column_if_missing(
                conn,
                "no_such_table",
                "x",
                "ALTER TABLE no_such_table ADD COLUMN x TEXT;",
            )
            .is_err(),
            "a non-duplicate error is not swallowed"
        );
    }

    // ── inbound exactly-once ledger ─────────────────────────────────

    /// The headline durability claim: telegram redelivers pending updates
    /// after a crash, and the ledger is what stops the agent answering
    /// the same message twice.
    #[test]
    fn claim_inbound_is_exactly_once() {
        let (_d, store) = scratch();
        assert!(
            store.claim_inbound("tg:123:456").unwrap(),
            "first claim wins"
        );
        assert!(
            !store.claim_inbound("tg:123:456").unwrap(),
            "second claim of the same key must lose"
        );
        // A different key is unaffected.
        assert!(store.claim_inbound("tg:123:457").unwrap());
    }

    #[test]
    fn claim_inbound_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let store = Store::open(&path).unwrap();
            assert!(store.claim_inbound("tg:redelivered").unwrap());
        }
        // Gateway restart — precisely when Telegram redelivers.
        let store = Store::open(&path).unwrap();
        assert!(
            !store.claim_inbound("tg:redelivered").unwrap(),
            "the ledger must be durable across restarts, not an in-memory window"
        );
    }

    // ── sessions & messages ─────────────────────────────────────────

    #[test]
    fn history_round_trips_roles_and_reasoning() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();

        store.append_message("s1", &msg("user", "hello")).unwrap();
        let mut assistant = msg("assistant", "hi there");
        assistant.reasoning = Some("thinking hard".into());
        assistant.tool_calls = Some(serde_json::json!([{"id": "c1", "type": "function"}]));
        store.append_message("s1", &assistant).unwrap();
        // The result row for c1: an assistant declaring a call no live
        // row answers is a malformed request on the wire, and
        // load_history's repair strips such calls — so the round-trip
        // fixture answers it.
        let mut result = msg("tool", "tool said hi");
        result.tool_meta = Some(crate::providers::ToolMeta {
            tool_call_id: "c1".into(),
            name: "greet".into(),
        });
        store.append_message("s1", &result).unwrap();

        let got = store.load_history("s1").unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].role, "user");
        assert_eq!(got[0].content, "hello");
        assert_eq!(got[1].role, "assistant");
        assert_eq!(
            got[1].reasoning.as_deref(),
            Some("thinking hard"),
            "reasoning must survive for API replay"
        );
        assert!(
            got[1].tool_calls.is_some(),
            "tool_calls must survive replay"
        );
    }

    #[test]
    fn create_session_is_idempotent_and_exists_reports_truthfully() {
        let (_d, store) = scratch();
        assert!(!store.session_exists("s1").unwrap());
        store.create_session("s1").unwrap();
        store.create_session("s1").unwrap(); // INSERT OR IGNORE
        assert!(store.session_exists("s1").unwrap());
        assert_eq!(store.session_message_count("s1").unwrap(), 0);
    }

    /// B1.5 added `nodes.static_ip` to the CREATE only, and `CREATE TABLE
    /// IF NOT EXISTS` never alters a table that already exists. Every
    /// install that booted B1 (2026-09-20) has a `nodes` table without
    /// the column, so registration, the heartbeat's re-register and
    /// `veles nodes list|show` all died on "no such column: static_ip"
    /// — reproduced against a snapshot of the live state.db.
    #[test]
    fn a_b1_nodes_table_gains_the_static_ip_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            // The B1 registry, verbatim, in a veles-owned DB.
            let store = Store::open(&path).unwrap();
            drop(store);
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                "DROP TABLE nodes;
                 CREATE TABLE nodes (
                     id TEXT PRIMARY KEY, name TEXT NOT NULL, host TEXT NOT NULL,
                     version TEXT NOT NULL DEFAULT '',
                     uptime_class TEXT NOT NULL DEFAULT 'ephemeral',
                     gpu TEXT, endpoint TEXT, caps TEXT,
                     registered_at TEXT DEFAULT (datetime('now')),
                     heartbeat_at TEXT DEFAULT (datetime('now')));
                 INSERT INTO nodes (id, name, host) VALUES ('b1-node', 'laptop', 'h');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let listed = store.nodes_list().expect("the B1 row lists");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].static_ip, None, "a B1 row has no address yet");
        store
            .node_register(
                "b1-node",
                "laptop",
                "h",
                "0",
                "ephemeral",
                None,
                None,
                None,
                None,
            )
            .expect("re-registration works on the migrated table");
    }

    /// B3a: leases fail closed, renew freely, and expire to nobody.
    /// The store-level pins behind arbiter.rs's behavioural ones.
    #[test]
    fn leases_grant_renew_refuse_and_expire() {
        let (_d, store) = scratch();
        let t0 = chrono::Utc::now();

        // Fresh grant.
        let g = store.lease_grant("singleton:cron", "a", t0, 90).unwrap();
        assert!(g.is_some(), "first ask wins");

        // Second node refused while unexpired.
        assert!(store
            .lease_grant("singleton:cron", "b", t0, 90)
            .unwrap()
            .is_none());

        // Same holder renews.
        assert!(store
            .lease_grant(
                "singleton:cron",
                "a",
                t0 + chrono::Duration::seconds(30),
                90
            )
            .unwrap()
            .is_some());

        // After expiry the row is nobody's: reads as unheld…
        let late = t0 + chrono::Duration::seconds(121);
        assert!(store.lease_get("singleton:cron", late).unwrap().is_none());
        // …and the first asker after that re-grants.
        assert!(store
            .lease_grant("singleton:cron", "b", late, 90)
            .unwrap()
            .is_some());

        // Only the holder releases.
        store.lease_release("singleton:cron", "a").unwrap();
        assert!(store.lease_get("singleton:cron", t0).unwrap().is_some());
        store.lease_release("singleton:cron", "b").unwrap();
        assert!(store.lease_get("singleton:cron", t0).unwrap().is_none());
    }

    /// B3a: a node's peer-observed address is recorded with a history —
    /// the SEQUENCE is the stability evidence, and the latest is
    /// denormalized onto the row for display.
    #[test]
    fn observations_record_history_and_latest() {
        let (_d, store) = scratch();
        store
            .node_register(
                "n1",
                "laptop",
                "h",
                "0",
                "ephemeral",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store
            .node_observe("n1", "203.0.113.9:40000", "vps")
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        store
            .node_observe("n1", "203.0.113.9:40001", "vps")
            .unwrap();

        let row = store.node_get("n1").unwrap().unwrap();
        assert!(
            row.observed_addr
                .as_deref()
                .is_some_and(|a| a.ends_with(":40001")),
            "latest wins the denormalized column: {:?}",
            row.observed_addr
        );
        assert_eq!(row.observed_by.as_deref(), Some("vps"));

        let hist = store.node_observation_history("n1", 10).unwrap();
        assert_eq!(hist.len(), 2, "both sightings survive: {hist:?}");
        assert!(hist[0].1.ends_with(":40000"), "oldest first: {hist:?}");
    }

    /// B3a: gossip merges preserve what only the RECEIVER can know —
    /// the local row's identity, the birthday, and the observation
    /// columns. A peer's stale copy of us must not rename us, and its
    /// claims about a third node land as a fresh row.
    #[test]
    fn gossip_merge_preserves_local_truths() {
        let (_d, store) = scratch();
        store
            .node_register(
                "local-1",
                "laptop",
                "h",
                "9",
                "ephemeral",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store.node_observe("local-1", "10.0.0.9:1", "vps").unwrap();

        // A peer's view of a node we know: refresh its claims, keep the
        // birthday, keep our observation.
        let before = store.nodes_list().unwrap();
        let bday = before
            .iter()
            .find(|n| n.id == "local-1")
            .unwrap()
            .registered_at
            .clone();
        let incoming = GossipNodeRow {
            id: "local-1".into(),
            name: "RENAMED-by-peer".into(),
            host: "peerhost".into(),
            version: "0".into(),
            uptime_class: "always-on".into(),
            gpu: None,
            endpoint: None,
            static_ip: None,
            static_ip_source: None,
            caps: None,
            load1: None,
            load_at: None,
            sessions_served: 0,
            registered_at: "1999-01-01 00:00:00".into(),
            // A FRESHER copy than the row registered above (merges are
            // last-writer-wins on the node's heartbeat; an older copy
            // is a stale relay and is ignored — pinned further down).
            heartbeat_at: (chrono::Utc::now() + chrono::Duration::minutes(1))
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
        };
        // NOTE: this is not our own row here (local_node_id is None in
        // the scratch env), so the merge takes the peer's claims — the
        // own-row protection is exercised by the env-scoped test below.
        store.node_merge_from_gossip(&incoming).unwrap();
        let after = store.nodes_list().unwrap();
        let row = after.iter().find(|n| n.id == "local-1").unwrap();
        assert_eq!(row.name, "RENAMED-by-peer");
        assert_eq!(row.registered_at, bday, "the birthday survives");
        assert!(row.observed_addr.is_some(), "the observation survives");

        // A node we have never seen: fresh row with its own birthday,
        // carrying its B4 placement metrics through the merge (a new
        // column that gossip silently dropped would make `any`
        // placement blind to every remote node's load).
        let third = GossipNodeRow {
            id: "third-3".into(),
            name: "vps".into(),
            host: "vh".into(),
            version: "1".into(),
            uptime_class: "always-on".into(),
            gpu: None,
            endpoint: Some("http://10.0.30.1:9900".into()),
            static_ip: None,
            static_ip_source: None,
            caps: None,
            load1: Some(37),
            load_at: Some("2026-09-28 12:00:00".into()),
            sessions_served: 9,
            registered_at: "2026-09-01 00:00:00".into(),
            heartbeat_at: "2026-09-27 12:00:00".into(),
        };
        store.node_merge_from_gossip(&third).unwrap();
        let row = store.node_get("third-3").unwrap().unwrap();
        assert_eq!(row.registered_at, "2026-09-01 00:00:00");
        assert_eq!(row.endpoint.as_deref(), Some("http://10.0.30.1:9900"));
        assert_eq!(row.load1, Some(37), "load propagates through gossip");
        assert_eq!(
            row.sessions_served, 9,
            "the served counter propagates through gossip"
        );

        // The same row again, with MOVED metrics: the UPDATE branch
        // must refresh them too. Gossip is a 30 s cadence, so almost
        // every exchange an `any` placement reads is an update of a row
        // already in the registry — a merge that only carried load on
        // first sight would leave every node's load frozen at its
        // first heartbeat, and `least-loaded` would pick by stale data
        // forever. (The mutation harness caught exactly this: pinning
        // only the INSERT branch let a frozen-UPDATE mutation survive.)
        let refreshed = GossipNodeRow {
            load1: Some(81),
            load_at: Some("2026-09-28 12:05:00".into()),
            sessions_served: 14,
            heartbeat_at: "2026-09-27 12:05:00".into(),
            ..third.clone()
        };
        store.node_merge_from_gossip(&refreshed).unwrap();
        let row = store.node_get("third-3").unwrap().unwrap();
        assert_eq!(row.load1, Some(81), "an update refreshes the load");
        assert_eq!(
            row.sessions_served, 14,
            "and the served counter, on every exchange"
        );
        assert_eq!(
            row.registered_at, "2026-09-01 00:00:00",
            "still its birthday"
        );

        // A STALE copy relayed by another peer (older heartbeat) must
        // not revert what the fresh one said.
        let stale_relay = GossipNodeRow {
            load1: Some(37),
            sessions_served: 9,
            endpoint: Some("http://old.example:9900".into()),
            heartbeat_at: "2026-09-27 12:01:00".into(),
            ..refreshed.clone()
        };
        store.node_merge_from_gossip(&stale_relay).unwrap();
        let row = store.node_get("third-3").unwrap().unwrap();
        assert_eq!(
            row.load1,
            Some(81),
            "a stale relay must not revert the load"
        );
        assert_eq!(row.sessions_served, 14, "nor walk the served counter back");
        assert_eq!(row.heartbeat_at, "2026-09-27 12:05:00");
    }

    /// B3a: a pre-B3a database (B1's nodes table, no observation
    /// columns) is migrated in place — the idempotent ALTER shape every
    /// column addition in this file uses.
    #[test]
    fn a_pre_b3a_nodes_table_gains_the_observation_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let store = Store::open(&path).unwrap();
            drop(store);
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                "DROP TABLE nodes;
                 CREATE TABLE nodes (
                     id TEXT PRIMARY KEY, name TEXT NOT NULL, host TEXT NOT NULL,
                     version TEXT NOT NULL DEFAULT '',
                     uptime_class TEXT NOT NULL DEFAULT 'ephemeral',
                     gpu TEXT, endpoint TEXT, static_ip TEXT, static_ip_source TEXT,
                     caps TEXT,
                     registered_at TEXT DEFAULT (datetime('now')),
                     heartbeat_at TEXT DEFAULT (datetime('now')));
                 INSERT INTO nodes (id, name, host) VALUES ('old', 'laptop', 'h');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let rows = store.nodes_list().expect("the pre-B3a row lists");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].observed_addr, None);
        // And the new writers work on the migrated table.
        store.node_observe("old", "10.1.1.1:5", "vps").unwrap();
        assert_eq!(
            store
                .node_get("old")
                .unwrap()
                .unwrap()
                .observed_addr
                .as_deref(),
            Some("10.1.1.1:5")
        );
        // B4: the same fixture is also a pre-B4 database — load1,
        // load_at and sessions_served must arrive through the same
        // idempotent ALTER loop, with the heartbeat/bump writers
        // working afterwards (the live-install break the static_ip
        // migration miss caused in the B1.5 review).
        let row = store.node_get("old").unwrap().unwrap();
        assert_eq!(row.load1, None);
        assert_eq!(row.sessions_served, 0);
        store.node_heartbeat("old", Some(11)).unwrap();
        let row = store.node_get("old").unwrap().unwrap();
        assert_eq!(row.load1, Some(11));
        store.node_served_bump("old").unwrap();
        assert_eq!(store.node_get("old").unwrap().unwrap().sessions_served, 1);
    }

    /// B1.5: every session row records its owner. A node with an
    /// identity stamps it at INSERT; a node without one (scratch home,
    /// pre-boot tool run) leaves NULL — honest, never a guessed owner —
    /// and nothing mints an identity as a side effect. A pre-B1.5
    /// database opened on a node WITH an identity is backfilled once.
    #[test]
    fn sessions_record_their_owning_node() {
        // scratch_xdg's guard takes the env lock itself — taking it
        // here TOO would deadlock (a Mutex is not reentrant).
        let (_cfg, _data, _guard) = crate::testenv::scratch_xdg();
        assert!(
            !crate::config::config_dir().join("node.toml").exists(),
            "fixture sanity: no identity in the scratch config dir"
        );
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(&dir.path().join("state.db")).unwrap();
            store.create_session("s1").unwrap();
        }
        let owner: Option<String> = rusqlite::Connection::open(dir.path().join("state.db"))
            .unwrap()
            .query_row("SELECT node FROM sessions WHERE id='s1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            owner, None,
            "no identity ⇒ NULL owner (honest, not guessed)"
        );
        assert!(
            !crate::config::config_dir().join("node.toml").exists(),
            "a session insert must not mint an identity as a side effect"
        );

        // The upgrade path: a pre-B1.5 veles database (marker present,
        // no node column, one session row) opened where an identity
        // exists backfills every row exactly once.
        let id = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir()).unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        {
            let c = rusqlite::Connection::open(dir2.path().join("state.db")).unwrap();
            c.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES ('veles_schema_version','1');
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY, title TEXT, parent TEXT,
                     created_at TEXT DEFAULT (datetime('now')),
                     updated_at TEXT DEFAULT (datetime('now')));
                 INSERT INTO sessions (id) VALUES ('old-1');
                 CREATE TABLE messages (
                     msg_id INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id TEXT NOT NULL, role TEXT NOT NULL,
                     content TEXT NOT NULL, reasoning TEXT, tool_calls TEXT,
                     tool_call_id TEXT, tool_name TEXT, injection TEXT,
                     created_at TEXT DEFAULT (datetime('now')));",
            )
            .unwrap();
        }
        {
            let store = Store::open(&dir2.path().join("state.db")).unwrap();
            store.create_session("new-1").unwrap();
        }
        let conn = rusqlite::Connection::open(dir2.path().join("state.db")).unwrap();
        let backfilled: String = conn
            .query_row("SELECT node FROM sessions WHERE id='old-1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(backfilled, id.id, "the backfill claims pre-ownership rows");
        let stamped: String = conn
            .query_row("SELECT node FROM sessions WHERE id='new-1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stamped, id.id, "insert-time stamping matches the backfill");
    }

    #[test]
    fn latest_session_ignores_empty_sessions() {
        let (_d, store) = scratch();
        store.create_session("empty").unwrap();
        assert_eq!(
            store.latest_session().unwrap(),
            None,
            "a session with no messages is not resumable"
        );
        store.create_session("used").unwrap();
        store.append_message("used", &msg("user", "hi")).unwrap();
        assert_eq!(store.latest_session().unwrap(), Some("used".into()));
    }

    #[test]
    fn title_set_and_read() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        assert_eq!(store.title("s1").unwrap(), None);
        store.set_title("s1", "Why the sky is blue").unwrap();
        assert_eq!(
            store.title("s1").unwrap().as_deref(),
            Some("Why the sky is blue")
        );
    }

    /// The fork link (Stage 3): a child session records its parent, a
    /// human session has none, and the link is readable both by id and
    /// through the session list — which is what makes children
    /// "attachable from the session list" rather than anonymous rows.
    #[test]
    fn child_session_records_its_parent() {
        let (_d, store) = scratch();
        store.create_session("parent").unwrap();
        store
            .create_child_session("child", "parent", Some("subagent: do the thing"))
            .unwrap();
        store.create_session("human").unwrap();

        assert_eq!(
            store.session_parent("child").unwrap().as_deref(),
            Some("parent")
        );
        assert_eq!(store.session_parent("parent").unwrap(), None);
        assert_eq!(
            store.session_parent("human").unwrap(),
            None,
            "a session nobody forked has no parent"
        );
        // The list carries the link too, or the sidebar cannot mark it.
        let metas = store.session_meta(10).unwrap();
        let child = metas.iter().find(|m| m.id == "child").unwrap();
        assert_eq!(child.parent.as_deref(), Some("parent"));
        assert_eq!(child.title, "subagent: do the thing");
    }

    /// `copy_history` is the ONE fork primitive — `/branch` and `/btw`
    /// both copy through it, so a drift here shows up in both. Oldest
    /// first, every row, and the SOURCE is untouched (a fork is not a
    /// move).
    #[test]
    fn copy_history_forks_every_row_and_leaves_the_source() {
        let (_d, store) = scratch();
        store.create_session("src").unwrap();
        store.create_session("dst").unwrap();
        for text in ["one", "two", "three"] {
            store
                .append_message("src", &ChatMessage::user(text))
                .unwrap();
        }
        let n = store.copy_history("src", "dst").unwrap();
        assert_eq!(n, 3, "every row forks");
        let dst = store.load_history("dst").unwrap();
        let texts: Vec<&str> = dst.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(texts, ["one", "two", "three"], "oldest first, in order");
        assert_eq!(
            store.session_message_count("src").unwrap(),
            3,
            "the source kept its rows — a fork copies, it does not move"
        );
    }

    // ── truncation / undo ───────────────────────────────────────────

    #[test]
    fn undo_turns_drops_the_last_user_turn_and_its_replies() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        for (role, text) in [
            ("user", "first"),
            ("assistant", "first reply"),
            ("user", "second"),
            ("assistant", "second reply"),
        ] {
            store.append_message("s1", &msg(role, text)).unwrap();
        }
        assert_eq!(store.user_turn_count("s1").unwrap(), 2);

        let dropped = store.undo_turns("s1", 1).unwrap();
        assert_eq!(dropped, 2, "the user turn AND its reply go");

        let left = store.load_history("s1").unwrap();
        assert_eq!(left.len(), 2);
        assert_eq!(left[0].content, "first");
        assert_eq!(left[1].content, "first reply");
        assert_eq!(store.user_turn_count("s1").unwrap(), 1);
    }

    #[test]
    fn undo_turns_on_empty_session_is_a_noop() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        assert_eq!(store.undo_turns("s1", 3).unwrap(), 0);
        assert_eq!(store.undo_turns("s1", 0).unwrap(), 0);
    }

    // ── tail pruning persistence ────────────────────────────────────

    /// The claim this whole feature rests on: pruning shrinks what the
    /// PROMPT reads while leaving the ORIGINAL text searchable. If FTS
    /// were external-content (reading messages.content on demand) this
    /// test would go red and pruning would be destroying context rather
    /// than relocating it.
    ///
    /// CALIBRATION MUTATION: add an AFTER UPDATE trigger on messages that
    /// syncs messages_fts, and the "still searchable" assertion fails —
    /// which is the proof that the property is about FTS holding its own
    /// copy, not about the prune being careful.
    #[test]
    fn pruning_shrinks_the_prompt_but_the_original_stays_searchable() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        // A realistic tool output, not a one-liner: the stub is ~200
        // chars, so pruning a 50-char result would make it LARGER.
        // `plan_tail_prunes` refuses to do that (it skips anything under
        // MIN_PRUNABLE_CHARS, or whose stub is not smaller), and this
        // fixture has to be a case the planner would actually pick.
        let big = format!(
            "the capybara is a large rodent found in wetlands\n{}",
            "x".repeat(5_000)
        );
        let id = store.append_message("s1", &msg("tool", &big)).unwrap();
        assert!(id > 0, "append_message returns the row id");

        // Before: the prompt and the index agree.
        let hist = store.load_history("s1").unwrap();
        assert_eq!(hist.len(), 1);
        assert!(hist[0].content.contains("capybara"));
        assert_eq!(hist[0].msg_id, Some(id), "load_history carries the id");

        let stub =
            crate::compaction::prune_stub(Some("read_file"), hist[0].content.chars().count());
        let n = store.prune_message_content("s1", id, &stub).unwrap();
        assert_eq!(n, 1, "exactly one row rewritten");

        // The prompt now reads the stub — shorter, no original text.
        let after = store.load_history("s1").unwrap();
        assert_eq!(after.len(), 1, "the row is still there");
        assert_eq!(after[0].content, stub);
        assert!(
            !after[0].content.contains("wetlands"),
            "prompt no longer carries it"
        );
        assert!(
            after[0].content.chars().count() < hist[0].content.chars().count(),
            "and it is actually smaller"
        );

        // …but session_search still finds the ORIGINAL, verbatim.
        let hits = store.search("capybara", 10).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the original text is still indexed: {hits:?}"
        );
        assert!(
            hits[0].2.contains("wetlands") || store.search("wetlands", 10).unwrap().len() == 1,
            "the full original survives in FTS"
        );
    }

    /// Pruning addresses rows by ID and is scoped to the session: a stale
    /// id from another session must not rewrite anything. This is why
    /// `msg_id` exists on ChatMessage at all — a positional index would
    /// happily hit the wrong row.
    #[test]
    fn pruning_by_id_cannot_touch_another_session() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        store.create_session("s2").unwrap();
        store
            .append_message("s1", &msg("tool", "secret-a"))
            .unwrap();
        let other_id = store
            .append_message("s2", &msg("tool", "secret-b"))
            .unwrap();

        // Use s2's id against s1: no rows match.
        let n = store
            .prune_message_content("s1", other_id, "STUBBED")
            .unwrap();
        assert_eq!(n, 0, "an id from another session matches nothing");
        assert_eq!(
            store.load_history("s1").unwrap()[0].content,
            "secret-a",
            "s1 untouched"
        );
        assert_eq!(
            store.load_history("s2").unwrap()[0].content,
            "secret-b",
            "s2 untouched"
        );
    }

    /// `load_history` must carry the row id, or nothing downstream can
    /// address a row for pruning after a reload.
    #[test]
    fn load_history_carries_the_row_ids() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        for t in ["one", "two", "three"] {
            store.append_message("s1", &msg("user", t)).unwrap();
        }
        let hist = store.load_history("s1").unwrap();
        let ids: Vec<i64> = hist.iter().map(|m| m.msg_id.unwrap()).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "ascending: {ids:?}");
    }

    /// Compaction persists its marker AFTER the plan/todo re-injections
    /// it precedes in memory (agent.rs's persist order), so msg_id last.
    /// A reload (the desktop reloads every turn) must show the marker
    /// FIRST — the summary introducing the rows it covers — while the
    /// marker AND its sibling injections stay a contiguous head over the
    /// surviving tail.
    #[test]
    fn a_reload_lists_the_compaction_marker_first() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        // The real store shape: the surviving tail predates the
        // compaction (smaller msg_ids), then the marker, then the
        // plan/todo re-injections persisted right after it — so by
        // msg_id the marker is LAST IN ITS FAMILY and a raw reload
        // showed it after the tail it summarizes.
        store
            .append_message("s1", &msg("user", "old head"))
            .unwrap();
        store
            .append_message("s1", &msg("user", "a kept tail row"))
            .expect("tail");
        store
            .append_message("s1", &compacted_marker())
            .expect("marker");
        store
            .append_message("s1", &msg("user", "PLAN…").with_injection("plan"))
            .expect("plan");
        store
            .append_message("s1", &msg("user", "TODO…").with_injection("todo"))
            .expect("todo");

        let h = store.load_history("s1").unwrap();
        assert_eq!(h.len(), 5);
        assert_eq!(
            h[0].injection.as_deref(),
            Some("compaction"),
            "the marker opens a reload"
        );
        assert_eq!(
            h[1].content, "PLAN…",
            "the plan re-injection follows the marker it was persisted after"
        );
        assert_eq!(h[2].content, "TODO…");
        assert_eq!(h[3].content, "old head");
        assert_eq!(h[4].content, "a kept tail row");
    }

    /// No member of the family → byte-for-byte identity: the repair is
    /// the load's only transform for ordinary histories (the reorder
    /// must not touch a session that never compacted).
    #[test]
    fn a_history_without_the_compaction_family_loads_unchanged() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        for t in ["a", "b", "c"] {
            store.append_message("s1", &msg("user", t)).unwrap();
        }
        let h = store.load_history("s1").unwrap();
        let contents: Vec<&str> = h.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, ["a", "b", "c"]);
    }

    // ── the session browser (2026-10-04) ────────────────────────────

    /// Three sessions: a titled one, an untitled one whose first user
    /// line is the batch prompt, and one whose text is only findable by
    /// full-text search.
    fn browser_store() -> (tempfile::TempDir, Store) {
        let (d, store) = scratch();
        for (id, title, first, later) in [
            ("s-mine", "Ali plan defaults", "switch the defaults", "done"),
            (
                "s-batch",
                "",
                "You are translating ONE page",
                "Tagalog output",
            ),
            ("s-deep", "", "hello", "the zanzibar needle"),
        ] {
            store.create_session(id).unwrap();
            if !title.is_empty() {
                store.set_title(id, title).unwrap();
            }
            store.append_message(id, &msg("user", first)).unwrap();
            store.append_message(id, &msg("assistant", later)).unwrap();
            // Usage rows are records — they must survive a delete.
            store
                .conn
                .execute(
                    "INSERT INTO usage (session_id, provider, model, prompt_tokens, completion_tokens)
                     VALUES (?1, 'test-provider', 'test-model', 100, 50)",
                    params![id],
                )
                .unwrap();
        }
        (d, store)
    }

    fn page(store: &Store, q: SessionQuery) -> (Vec<String>, u64) {
        let (rows, total) = store.session_page(&q).unwrap();
        (rows.into_iter().map(|r| r.id).collect(), total)
    }

    /// A page is picked from `sessions` and then filled in. Activity
    /// comes newest first, and ties within one second go newest-created
    /// first. The pages of one listing join up with no session twice
    /// and none missing, and each row carries its own preview and count.
    #[test]
    fn session_pages_join_up_newest_first_each_with_its_own_preview() {
        let (_d, store) = scratch();
        for i in 0..7 {
            let id = format!("s{i}");
            store.create_session(&id).unwrap();
            store
                .append_message(&id, &msg("user", &format!("first {i}")))
                .unwrap();
            for _ in 0..i {
                store
                    .append_message(&id, &msg("assistant", "more"))
                    .unwrap();
            }
        }
        // Every row above shares one second. A 2099 stamp puts s2 ahead
        // whatever second this test runs in.
        store
            .conn
            .execute(
                "UPDATE sessions SET updated_at = '2099-01-01 00:00:00' WHERE id = 's2'",
                [],
            )
            .unwrap();
        let mut seen = Vec::new();
        for offset in [0, 3, 6] {
            let (rows, total) = store
                .session_page(&SessionQuery {
                    limit: 3,
                    offset,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(total, 7);
            for r in rows {
                let i: i64 = r.id[1..].parse().unwrap();
                assert_eq!(r.preview, format!("first {i}"), "{}'s own first line", r.id);
                assert_eq!(r.message_count, i + 1, "{}'s own count", r.id);
                seen.push(r.id);
            }
        }
        assert_eq!(seen, ["s2", "s6", "s5", "s4", "s3", "s1", "s0"]);
    }

    #[test]
    fn the_session_page_searches_titles_first_lines_and_text_and_counts_all() {
        let (_d, store) = browser_store();
        let all = SessionQuery {
            limit: 2,
            ..Default::default()
        };
        let (ids, total) = page(&store, all.clone());
        assert_eq!((ids.len(), total), (2, 3), "a page of 2, out of 3");
        let (rest, _) = page(
            &store,
            SessionQuery {
                offset: 2,
                ..all.clone()
            },
        );
        assert_eq!(rest.len(), 1, "the next page holds the third");

        let find = |q: &str, fts: bool| {
            page(
                &store,
                SessionQuery {
                    limit: 50,
                    q: Some(q.into()),
                    fts,
                    ..Default::default()
                },
            )
        };
        assert_eq!(
            find("ali PLAN", false),
            (vec!["s-mine".into()], 1),
            "title, any case"
        );
        assert_eq!(
            find("translating one", false).0,
            ["s-batch"],
            "first user line"
        );
        assert_eq!(find("s-de", false).0, ["s-deep"], "id");
        assert!(find("zanzibar", false).0.is_empty(), "later text needs fts");
        assert_eq!(find("zanzibar", true).0, ["s-deep"], "full text");

        let (untitled, n) = page(
            &store,
            SessionQuery {
                limit: 50,
                titled: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(n, 2);
        assert!(!untitled.contains(&"s-mine".to_string()));
        let rows = store.session_rows(&["s-batch".into()]).unwrap();
        assert_eq!(rows[0].preview, "You are translating ONE page");
        assert_eq!(rows[0].message_count, 2);
    }

    /// One transaction for many, and only theirs: the third session and
    /// its search entries survive.
    #[test]
    fn deleting_sessions_takes_their_rows_and_nothing_else() {
        let (_d, store) = browser_store();
        store
            .override_replace("s-batch", Some("p"), Some("m"), None)
            .unwrap();
        let n = store
            .delete_sessions(&["s-batch".into(), "s-deep".into(), "absent".into()])
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            page(
                &store,
                SessionQuery {
                    limit: 9,
                    ..Default::default()
                }
            )
            .1,
            1
        );
        let count = |sql: &str| -> i64 { store.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT COUNT(*) FROM messages"), 2, "s-mine's two");
        assert_eq!(
            count("SELECT COUNT(*) FROM messages_fts WHERE session_id <> 's-mine'"),
            0
        );
        assert_eq!(count("SELECT COUNT(*) FROM session_overrides"), 0);
        // Usage rows are records — they stay.
        assert_eq!(count("SELECT COUNT(*) FROM usage"), 3, "usage is a record");
        let (hits, _) = page(
            &store,
            SessionQuery {
                limit: 9,
                q: Some("zanzibar".into()),
                fts: true,
                ..Default::default()
            },
        );
        assert!(hits.is_empty(), "the deleted text is not searchable");
    }

    /// The reference lookups reach every session, not the newest 1,000,
    /// and an exact id wins over a longer id it prefixes.
    #[test]
    fn a_session_reference_reaches_past_the_newest_thousand() {
        let (_d, store) = scratch();
        store.create_session("old-one").unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET updated_at = '2000-01-01 00:00:00' WHERE id = 'old-one'",
                [],
            )
            .unwrap();
        for i in 0..1005 {
            store.create_session(&format!("new-{i:04}")).unwrap();
        }
        assert_eq!(
            store.find_session("old").unwrap().as_deref(),
            Some("old-one")
        );
        assert_eq!(store.find_session("nope").unwrap(), None);
        store.create_session("old-one-two").unwrap();
        assert_eq!(
            store.find_session("old-one").unwrap().as_deref(),
            Some("old-one"),
            "the exact id first"
        );
        assert_eq!(store.sessions_with_prefix("old", 2).unwrap().len(), 2);
        assert_eq!(store.session_count().unwrap(), 1007);
    }

    /// A listing reads live runs without reaping: a GET is not a write.
    #[test]
    fn reading_current_runs_reaps_nothing() {
        let (_d, store) = scratch();
        store.run_start("s1", 1, "h", "desktop").unwrap();
        store
            .conn
            .execute(
                "UPDATE session_runs SET heartbeat_at = datetime('now', '-120 seconds')",
                [],
            )
            .unwrap();
        assert!(
            store.runs_current(30).unwrap().is_empty(),
            "stale is not current"
        );
        let left: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM session_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 1, "and the row is still there for the reaper");
    }

    // ── compaction anchor freshness ─────────────────────────────────

    /// Set the two timestamps explicitly, because `datetime('now')` is
    /// SECOND-granular and a test that appends both rows in one tick
    /// would be measuring the tie-break instead of the ordering.
    fn set_marker_at(store: &Store, session: &str, at: &str) {
        store
            .conn()
            .execute(
                "UPDATE messages SET created_at = ?2 WHERE session_id = ?1 AND injection='compaction'",
                params![session, at],
            )
            .unwrap();
    }
    fn set_usage_at(store: &Store, session: &str, at: &str) {
        store
            .conn()
            .execute(
                "UPDATE usage SET created_at = ?2 WHERE session_id = ?1",
                params![session, at],
            )
            .unwrap();
    }
    fn record_tokens(store: &Store, session: &str, prompt: u64) {
        store
            .record_usage_for(
                session,
                None,
                "p",
                "m",
                CallTokens {
                    prompt_tokens: prompt,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    fn compacted_marker() -> ChatMessage {
        msg("user", crate::compaction::COMPACTION_MARKER).with_injection("compaction")
    }

    /// No compaction has ever run: the reported number describes the
    /// current prompt, so it is fresh.
    #[test]
    fn anchor_is_fresh_when_no_compaction_has_run() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        store.append_message("s1", &msg("user", "hi")).unwrap();
        record_tokens(&store, "s1", 450_000);
        assert!(store.prompt_tokens_anchor_is_fresh("s1").unwrap());
    }

    /// THE BUG THIS FIXES: a compaction just rewrote history, but the
    /// newest usage row still describes the PRE-rewrite prompt. Trusting
    /// it re-fires compaction every turn — session
    /// 20260913_142929_3dfd8c collected four markers that way.
    ///
    /// CALIBRATION MUTATION: make this return `true` unconditionally and
    /// the stacked-marker agent test goes red.
    #[test]
    fn anchor_is_stale_right_after_a_compaction() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        record_tokens(&store, "s1", 450_000);
        store.append_message("s1", &compacted_marker()).unwrap();

        // Usage first, marker after → the reading predates the rewrite.
        set_usage_at(&store, "s1", "2026-09-13 17:00:00");
        set_marker_at(&store, "s1", "2026-09-13 17:05:00");
        assert!(
            !store.prompt_tokens_anchor_is_fresh("s1").unwrap(),
            "a reading from before the compaction is stale"
        );
        assert_eq!(
            store.last_prompt_tokens("s1").unwrap(),
            Some(450_000),
            "the raw number is still there — the caller must not trust it"
        );
    }

    /// Once a real turn runs after the compaction, the new usage row is
    /// measured against the shortened prompt and is fresh again.
    #[test]
    fn anchor_becomes_fresh_again_after_a_post_compaction_turn() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        record_tokens(&store, "s1", 450_000);
        store.append_message("s1", &compacted_marker()).unwrap();
        set_usage_at(&store, "s1", "2026-09-13 17:00:00");
        set_marker_at(&store, "s1", "2026-09-13 17:05:00");
        assert!(!store.prompt_tokens_anchor_is_fresh("s1").unwrap());

        // The next real turn reports against the compacted prompt.
        record_tokens(&store, "s1", 66_000);
        set_usage_at(&store, "s1", "2026-09-13 17:10:00");
        assert!(
            store.prompt_tokens_anchor_is_fresh("s1").unwrap(),
            "ground truth is back, and it is the small number"
        );
        assert_eq!(store.last_prompt_tokens("s1").unwrap(), Some(66_000));
    }

    /// The tie-break: `datetime('now')` is second-granular, so the marker
    /// and the usage row can land in the SAME second. Equal must count as
    /// STALE — the marker may well have been written after the reading
    /// within that second, and guessing "fresh" would reproduce the exact
    /// bug the guard exists to kill.
    #[test]
    fn a_same_second_marker_counts_as_stale() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        record_tokens(&store, "s1", 450_000);
        store.append_message("s1", &compacted_marker()).unwrap();
        let same = "2026-09-13 17:00:00";
        set_usage_at(&store, "s1", same);
        set_marker_at(&store, "s1", same);
        assert!(
            !store.prompt_tokens_anchor_is_fresh("s1").unwrap(),
            "equal timestamps are stale, not fresh"
        );
    }

    /// A marker with no usage at all: there is no reported number to
    /// trust, so the caller falls back to the estimate.
    #[test]
    fn a_marker_with_no_usage_is_not_fresh() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        store.append_message("s1", &compacted_marker()).unwrap();
        assert!(!store.prompt_tokens_anchor_is_fresh("s1").unwrap());
        assert_eq!(store.last_prompt_tokens("s1").unwrap(), None);
    }

    // ── FTS5 search ─────────────────────────────────────────────────

    #[test]
    fn search_finds_messages_and_scopes_to_the_right_session() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        store.create_session("s2").unwrap();
        store
            .append_message("s1", &msg("user", "the capybara is a large rodent"))
            .unwrap();
        store
            .append_message("s2", &msg("user", "unrelated content about rust"))
            .unwrap();

        let hits = store.search("capybara", 10).unwrap();
        assert_eq!(hits.len(), 1, "one match, got {hits:?}");
        assert_eq!(hits[0].0, "s1");

        assert!(
            store.search("nonexistentword", 10).unwrap().is_empty(),
            "a miss returns nothing rather than everything"
        );
    }

    #[test]
    fn search_survives_punctuation_that_is_not_valid_fts_syntax() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        store
            .append_message("s1", &msg("user", "error: cannot find symbol foo()"))
            .unwrap();
        // Raw FTS would reject this as a malformed query; the store is
        // expected to fall back to a phrase search rather than error.
        let hits = store.search("cannot find symbol", 10).unwrap();
        assert!(!hits.is_empty(), "punctuation-laden query must still match");
    }

    #[test]
    fn deleting_a_session_removes_it_from_search() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        store
            .append_message("s1", &msg("user", "distinctivetoken here"))
            .unwrap();
        assert_eq!(store.search("distinctivetoken", 10).unwrap().len(), 1);

        store.delete_session("s1").unwrap();
        assert!(
            store.search("distinctivetoken", 10).unwrap().is_empty(),
            "FTS rows must go with the session, or search returns dead hits"
        );
        assert!(!store.session_exists("s1").unwrap());
        assert_eq!(store.load_history("s1").unwrap().len(), 0);
    }

    /// USER, shown a provider row reading 57%: "57% is extremely bad".
    /// It was not a reading of anything. That row's first four calls
    /// predated cache accounting, so `cached_tokens` was 0 because
    /// nothing PARSED the field — indistinguishable in the table from a
    /// real miss, and dragging the lifetime share down forever while
    /// the recent calls ran at 86%.
    ///
    /// The last-session reading is the fix, and this is the exact shape.
    #[test]
    fn the_last_session_reading_survives_history_the_lifetime_share_cannot() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        let call = |p: u64, c: u64| CallTokens {
            prompt_tokens: p,
            completion_tokens: 0,
            cached_tokens: c,
            cache_write_tokens: 0,
            cache_reported: true,
            reasoning_tokens: None,
        };
        // Four pre-instrumentation calls: real tokens, no cache field.
        for _ in 0..4 {
            store
                .record_usage("old", "prov", "m", call(25_000, 0))
                .unwrap();
        }
        // Then a run with the cache working.
        for _ in 0..4 {
            store
                .record_usage("new", "prov", "m", call(10_000, 8_600))
                .unwrap();
        }

        let row = &store.usage_by_provider().unwrap()[0];
        let lifetime = row.cached_tokens as f64 / row.prompt_tokens as f64;
        assert!(
            lifetime < 0.25,
            "the lifetime share is dominated by history: {lifetime}"
        );
        let recent = row.last_session_cache_rate().expect("a reading");
        assert!(
            (recent - 0.86).abs() < 0.01,
            "the last session reports how it is working now: {recent}"
        );
        assert_eq!(row.last_session_calls, 4, "only the most recent session");

        // A row with no prompt tokens at all has NO reading — never 0%.
        store.record_usage("s", "silent", "m", call(0, 0)).unwrap();
        let silent = store
            .usage_by_provider()
            .unwrap()
            .into_iter()
            .find(|r| r.provider == "silent")
            .unwrap();
        assert!(
            silent.last_session_cache_rate().is_none(),
            "no prompt tokens is no reading, not a zero"
        );
    }

    /// A provider that reports NO cache field must not read as a 0% hit
    /// rate. `cached_tokens: 0` answers three different questions —
    /// "the provider said zero", "the provider said nothing", "the code
    /// of the day never parsed it" — and the last two are not misses.
    /// Every share skips calls where the provider did not answer.
    #[test]
    fn a_provider_that_reports_no_cache_field_has_no_rate_not_a_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        let call = |p: u64, c: u64, reported: bool| CallTokens {
            prompt_tokens: p,
            completion_tokens: 0,
            cached_tokens: c,
            cache_write_tokens: 0,
            cache_reported: reported,
            reasoning_tokens: None,
        };
        // "silent" never reports; "loud" reports and genuinely misses.
        store
            .record_usage("s", "silent", "m", call(9_000, 0, false))
            .unwrap();
        store
            .record_usage("s", "loud", "m", call(9_000, 0, true))
            .unwrap();

        let by = store.usage_by_provider().unwrap();
        let row = |name: &str| by.iter().find(|r| r.provider == name).expect("row").clone();
        assert!(
            row("silent").last_session_cache_rate().is_none(),
            "silence is not a zero"
        );
        assert_eq!(
            row("loud").last_session_cache_rate(),
            Some(0.0),
            "a REPORTED zero is a real 0% and must still show as one"
        );
        // The lifetime tokens are untouched either way — this changes
        // what is divided, never what was billed.
        assert_eq!(row("silent").prompt_tokens, 9_000);

        // And a reported reading round-trips through last_call_tokens.
        assert!(
            store.last_call_tokens("s").unwrap().unwrap().cache_reported,
            "the flag survives the read side too"
        );
    }
    // ── usage ───────────────────────────────────────────────────────

    /// Spend is asked per CALLER, and a caller holds one session per
    /// chat — so the question cannot be answered by a session-keyed
    /// sum, which is what existed.
    #[test]
    fn spend_is_summed_across_a_subjects_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        let call = |p: u64, c: u64| CallTokens {
            prompt_tokens: p,
            completion_tokens: c,
            cached_tokens: 0,
            cache_write_tokens: 0,
            cache_reported: true,
            reasoning_tokens: None,
        };
        store.create_session("a").unwrap();
        store.create_session("b").unwrap();
        store
            .record_usage_for("a", Some("irc:net/nick"), "p", "m", call(1_000, 200))
            .unwrap();
        store
            .record_usage_for("b", Some("irc:net/nick"), "p", "m", call(3_000, 800))
            .unwrap();
        // Someone else's spend, and an owner-surface call with no
        // subject at all, must not land on this budget.
        store
            .record_usage_for("a", Some("irc:net/other"), "p", "m", call(9_000, 9_000))
            .unwrap();
        store
            .record_usage("a", "p", "m", call(9_000, 9_000))
            .unwrap();

        assert_eq!(
            store.tokens_spent_since("irc:net/nick", 86_400).unwrap(),
            5_000,
            "prompt + completion across every session the subject holds"
        );
        assert_eq!(
            store.tokens_spent_since("irc:net/other", 86_400).unwrap(),
            18_000
        );
        assert_eq!(
            store.tokens_spent_since("irc:net/nobody", 86_400).unwrap(),
            0
        );
    }

    /// A cached prefix bills at 1/6, not list. The budget is a bill,
    /// not a wire count: 6 000 cached + 1 000 uncached prompt + 600
    /// completion = 1 000 + 1 000 + 600 = 2 600, not 7 600.
    #[test]
    fn spend_bills_cached_tokens_at_a_sixth() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        store.create_session("a").unwrap();
        store
            .record_usage_for(
                "a",
                Some("irc:net/nick"),
                "p",
                "m",
                CallTokens {
                    prompt_tokens: 7_000,
                    completion_tokens: 600,
                    cached_tokens: 6_000,
                    cache_write_tokens: 0,
                    cache_reported: true,
                    reasoning_tokens: None,
                },
            )
            .unwrap();
        assert_eq!(
            store.tokens_spent_since("irc:net/nick", 86_400).unwrap(),
            // uncached 1 000 + cached 6 000/6 = 1 000 + completion 600
            1_000 + 1_000 + 600
        );
    }

    /// The spend breakdown agrees with the discounted total: billed() on
    /// spend_parts_since equals tokens_spent_since for the same window,
    /// and exposes the cache split the total hides.
    #[test]
    fn spend_parts_agree_with_the_discounted_total() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        store.create_session("a").unwrap();
        store
            .record_usage_for(
                "a",
                Some("irc:net/nick"),
                "p",
                "m",
                CallTokens {
                    prompt_tokens: 7_000,
                    completion_tokens: 600,
                    cached_tokens: 6_000,
                    cache_write_tokens: 0,
                    cache_reported: true,
                    reasoning_tokens: None,
                },
            )
            .unwrap();
        let parts = store.spend_parts_since("irc:net/nick", 86_400).unwrap();
        assert_eq!(parts.prompt_tokens, 7_000);
        assert_eq!(parts.cached_tokens, 6_000);
        assert_eq!(parts.completion_tokens, 600);
        assert_eq!(parts.calls, 1);
        assert_eq!(
            parts.billed(),
            store.tokens_spent_since("irc:net/nick", 86_400).unwrap(),
            "the breakdown and the ceiling tell one story"
        );
    }

    /// rate_claimed reads a ledger without consuming: a claim against a
    /// prefix shows up, a different prefix does not, and the read itself
    /// does not add a claim.
    #[test]
    fn rate_claimed_reads_a_ledger_without_consuming() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        assert_eq!(store.rate_claimed(Store::MEDIA_RATE_PREFIX, "s", 3600), 0);
        // Claim two media against a high limit so neither is refused.
        assert!(store
            .claim_rate(Store::MEDIA_RATE_PREFIX, "s", 10, 3600)
            .unwrap()
            .is_none());
        assert!(store
            .claim_rate(Store::MEDIA_RATE_PREFIX, "s", 10, 3600)
            .unwrap()
            .is_none());
        assert_eq!(store.rate_claimed(Store::MEDIA_RATE_PREFIX, "s", 3600), 2);
        // A read adds nothing.
        assert_eq!(store.rate_claimed(Store::MEDIA_RATE_PREFIX, "s", 3600), 2);
        // … and the video ledger is independent.
        assert_eq!(store.rate_claimed(Store::VIDEO_RATE_PREFIX, "s", 3600), 0);
    }

    /// A route that reports `cached > prompt` (double-counted cache)
    /// must not push the uncached half negative and refund the budget:
    /// the floor is 0 uncached, so the row bills cached/6 + completion.
    #[test]
    fn spend_floors_uncached_at_zero_on_a_double_counted_row() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        store.create_session("a").unwrap();
        store
            .record_usage_for(
                "a",
                Some("irc:net/nick"),
                "p",
                "m",
                CallTokens {
                    prompt_tokens: 1_000,
                    completion_tokens: 120,
                    cached_tokens: 6_000,
                    cache_write_tokens: 0,
                    cache_reported: true,
                    reasoning_tokens: None,
                },
            )
            .unwrap();
        assert_eq!(
            store.tokens_spent_since("irc:net/nick", 86_400).unwrap(),
            1_000 + 120,
            "cached 6 000/6 = 1 000, uncached floored to 0, completion 120"
        );
    }

    /// The rate ledger is a rolling window. The case that matters is
    /// the one an hourly bucket gets wrong: claims made just before a
    /// boundary must still count against claims made just after it.
    #[test]
    fn the_turn_ledger_is_a_rolling_window_not_a_bucket() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        let who = "irc:net/floody";
        // Three allowed, the fourth refused, and the refusal says when.
        for i in 0..3 {
            assert_eq!(
                store.claim_turn(who, 3, 3_600).unwrap(),
                None,
                "claim {i} should be allowed"
            );
        }
        let retry = store
            .claim_turn(who, 3, 3_600)
            .unwrap()
            .expect("the fourth is refused");
        assert!(
            retry > 0 && retry <= 3_600,
            "retry_after must be inside the window: {retry}"
        );
        assert_eq!(store.turns_claimed(who, 3_600), 3);

        // A window short enough to have already passed frees every
        // slot — same ledger, no bucket boundary involved.
        assert_eq!(store.claim_turn(who, 3, 0).unwrap(), None);

        // 0 = unlimited, and it must not even record.
        let quiet = "irc:net/admin";
        for _ in 0..50 {
            assert_eq!(store.claim_turn(quiet, 0, 3_600).unwrap(), None);
        }
        assert_eq!(store.turns_claimed(quiet, 3_600), 0);
    }

    #[test]
    fn usage_totals_and_per_provider_breakdown() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        store
            .record_usage(
                "s1",
                "prov-a",
                "model-x",
                crate::store::CallTokens {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    cached_tokens: 9,
                    cache_write_tokens: 1,
                    cache_reported: true,
                    reasoning_tokens: None,
                },
            )
            .unwrap();
        store
            .record_usage(
                "s1",
                "prov-a",
                "model-x",
                crate::store::CallTokens {
                    prompt_tokens: 20,
                    completion_tokens: 7,
                    cached_tokens: 18,
                    cache_write_tokens: 0,
                    cache_reported: true,
                    reasoning_tokens: None,
                },
            )
            .unwrap();
        store
            .record_usage(
                "s1",
                "prov-b",
                "model-y",
                crate::store::CallTokens {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                    cache_reported: true,
                    reasoning_tokens: None,
                },
            )
            .unwrap();

        let u = store.usage_totals(Some("s1")).unwrap();
        assert_eq!(u.calls, 3);
        assert_eq!(u.prompt_tokens, 31);
        assert_eq!(u.completion_tokens, 13);
        assert_eq!(u.cached_tokens, 27, "the cache columns total too");
        assert_eq!(u.cache_write_tokens, 1);

        let mut by = store.usage_by_provider().unwrap();
        by.sort_by(|a, b| a.provider.cmp(&b.provider));
        assert_eq!(by.len(), 2);
        assert_eq!(
            by[0],
            ProviderUsageRow {
                provider: "prov-a".into(),
                model: "model-x".into(),
                calls: 2,
                prompt_tokens: 30,
                completion_tokens: 12,
                cached_tokens: 27,
                cache_write_tokens: 1,
                // Both calls fall inside the recent window.
                last_session_prompt_tokens: 30,
                last_session_cached_tokens: 27,
                last_session_calls: 2,
            }
        );
        assert_eq!(
            by[1],
            ProviderUsageRow {
                provider: "prov-b".into(),
                model: "model-y".into(),
                calls: 1,
                prompt_tokens: 1,
                completion_tokens: 1,
                cached_tokens: 0,
                cache_write_tokens: 0,
                last_session_prompt_tokens: 1,
                last_session_cached_tokens: 0,
                last_session_calls: 1,
            }
        );
    }

    /// Reasoning tokens ride the same "unknown ≠ 0" line as cache: a
    /// call that reports them sums into the totals and the last-call
    /// reading; a call that never reports leaves the reading None, never
    /// a misleading 0.
    #[test]
    fn reasoning_tokens_persist_as_none_when_unreported() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        store.create_session("s1").unwrap();
        let mk = |rt: Option<u64>| CallTokens {
            prompt_tokens: 100,
            completion_tokens: 200,
            cached_tokens: 0,
            cache_write_tokens: 0,
            cache_reported: true,
            reasoning_tokens: rt,
        };
        // Unreported first: totals must be None, not Some(0).
        store.record_usage("s1", "p", "m", mk(None)).unwrap();
        assert_eq!(
            store.usage_totals(Some("s1")).unwrap().reasoning_tokens,
            None
        );
        assert_eq!(
            store
                .last_call_tokens("s1")
                .unwrap()
                .unwrap()
                .reasoning_tokens,
            None
        );

        // Reported (even zero) is a different fact.
        store.record_usage("s1", "p", "m", mk(Some(150))).unwrap();
        let t = store.usage_totals(Some("s1")).unwrap();
        assert_eq!(t.reasoning_tokens, Some(150));
        assert_eq!(
            store
                .last_call_tokens("s1")
                .unwrap()
                .unwrap()
                .reasoning_tokens,
            Some(150)
        );
    }

    /// A DB created BEFORE the cache columns existed must open clean and
    /// gain both columns — existing sessions' history is the whole point
    /// of keeping the store, so the migration must not lose a row.
    #[test]
    fn an_old_db_gains_the_cache_columns_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        // Build the pre-cache shape: usage WITHOUT the two columns.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta(key, value) VALUES ('veles_schema_version', '1');
                 CREATE TABLE usage (
                     usage_id INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id TEXT NOT NULL,
                     provider TEXT NOT NULL,
                     model TEXT NOT NULL,
                     prompt_tokens INTEGER NOT NULL DEFAULT 0,
                     completion_tokens INTEGER NOT NULL DEFAULT 0,
                     created_at TEXT DEFAULT (datetime('now'))
                 );
                 INSERT INTO usage (session_id, provider, model, prompt_tokens, completion_tokens)
                     VALUES ('s1', 'p', 'm', 100, 10);",
            )
            .unwrap();
        }
        // Reopen through Store::open — the migration runs here.
        let store = Store::open(&path).expect("old db opens clean");
        let cols: Vec<String> = {
            let mut stmt = store
                .conn()
                .prepare("SELECT name FROM pragma_table_info('usage')")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(cols.iter().any(|c| c == "cached_tokens"), "{cols:?}");
        assert!(cols.iter().any(|c| c == "cache_write_tokens"), "{cols:?}");
        // …and the row survived.
        let u = store.usage_totals(Some("s1")).unwrap();
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 10);
        assert_eq!(u.cached_tokens, 0, "old rows read as nothing-cached");

        // The spend budget's column and index arrive on the same open.
        // This is the upgrade path for a real deployment: without it
        // the new ceiling would refer to a column that is not there,
        // and `record_usage_for` would fail on every call.
        assert!(cols.iter().any(|c| c == "subject"), "{cols:?}");
        let idx: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type='index' AND name='idx_usage_subject_time'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1, "the spend query's index must exist too");
        // A pre-upgrade row has no subject, so it must not be charged
        // to anybody: priming someone's budget with calls made before
        // anyone was identified would refuse them for a debt that is
        // not theirs.
        assert_eq!(store.tokens_spent_since("irc:anyone", 86_400).unwrap(), 0);
        // And the column is writable through the new path.
        store
            .record_usage_for(
                "s1",
                Some("irc:someone"),
                "p",
                "m",
                CallTokens {
                    prompt_tokens: 7,
                    completion_tokens: 3,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                    cache_reported: false,
                    reasoning_tokens: None,
                },
            )
            .expect("the migrated column accepts writes");
        assert_eq!(store.tokens_spent_since("irc:someone", 86_400).unwrap(), 10);
    }

    // ── meta ────────────────────────────────────────────────────────

    #[test]
    fn meta_get_set_delete_and_prefix_scan() {
        let (_d, store) = scratch();
        assert_eq!(store.meta_get("missing"), None);
        store.meta_set("failover:s1", "ask").unwrap();
        store.meta_set("failover:s2", "auto").unwrap();
        store.meta_set("other", "x").unwrap();
        assert_eq!(store.meta_get("failover:s1").as_deref(), Some("ask"));

        // set on an existing key overwrites rather than duplicating
        store.meta_set("failover:s1", "stop").unwrap();
        assert_eq!(store.meta_get("failover:s1").as_deref(), Some("stop"));

        let mut keys = store.meta_scan_prefix("failover:");
        keys.sort();
        assert_eq!(
            keys.len(),
            2,
            "prefix scan must not match 'other': {keys:?}"
        );

        store.meta_delete("failover:s1").unwrap();
        assert_eq!(store.meta_get("failover:s1"), None);
    }

    // ── todos ───────────────────────────────────────────────────────

    #[test]
    fn todos_replace_swaps_the_whole_list() {
        let (_d, store) = scratch();
        let items = |v: &[(&str, &str)]| -> Vec<TodoItem> {
            v.iter()
                .map(|(id, status)| TodoItem {
                    id: (*id).into(),
                    content: format!("task {id}"),
                    status: (*status).into(),
                })
                .collect()
        };
        store
            .todos_replace("s1", &items(&[("1", "pending")]))
            .unwrap();
        assert_eq!(store.todos_list("s1").unwrap().len(), 1);

        store
            .todos_replace("s1", &items(&[("2", "pending"), ("3", "done")]))
            .unwrap();
        let got = store.todos_list("s1").unwrap();
        assert_eq!(got.len(), 2, "replace must not append: {got:?}");
        assert!(got.iter().all(|t| t.id != "1"), "old items must be gone");
    }

    // ── checkpoints ─────────────────────────────────────────────────

    #[test]
    fn checkpoints_round_trip_and_delete() {
        let (_d, store) = scratch();
        let id = store
            .checkpoint_add(
                "s1",
                "/tmp/f.txt",
                Some("blob1"),
                Some("deadbeef"),
                12,
                "write_file",
                Some("note"),
            )
            .unwrap();

        let got = store
            .checkpoint_get(id)
            .unwrap()
            .expect("checkpoint exists");
        assert_eq!(got.session_id, "s1");
        assert_eq!(got.path, "/tmp/f.txt");
        assert_eq!(got.blob.as_deref(), Some("blob1"));
        assert_eq!(got.bytes, 12);
        assert_eq!(got.tool, "write_file");

        assert_eq!(store.checkpoint_list(Some("s1"), 10).unwrap().len(), 1);
        assert_eq!(store.checkpoint_delete(id).unwrap(), 1);
        assert!(store.checkpoint_get(id).unwrap().is_none());
    }

    /// A checkpoint whose blob is None means "the file did not exist
    /// before the mutation" — restoring it deletes the file. That is a
    /// destructive path, so the None must survive the round trip and not
    /// silently become an empty string.
    #[test]
    fn checkpoint_absent_blob_stays_none() {
        let (_d, store) = scratch();
        let id = store
            .checkpoint_add("s1", "/tmp/new.txt", None, None, 0, "write_file", None)
            .unwrap();
        let got = store.checkpoint_get(id).unwrap().unwrap();
        assert_eq!(got.blob, None, "None must not round-trip as Some(\"\")");
        assert_eq!(got.sha256, None);
    }

    /// Deleting a session must not strand its session-scoped rows.
    ///
    /// checkpoints in particular reference blob files on disk, so an
    /// orphaned row is an unbounded leak that `checkpoints::prune` will
    /// never reach (it prunes by age, but the restore path looks up by
    /// session).
    #[test]
    fn delete_session_leaves_no_orphans() {
        let (_d, store) = scratch();
        store.create_session("s1").unwrap();
        store.append_message("s1", &msg("user", "hi")).unwrap();
        store
            .record_usage(
                "s1",
                "p",
                "m",
                crate::store::CallTokens {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                    cache_reported: true,
                    reasoning_tokens: None,
                },
            )
            .unwrap();
        store
            .todos_replace(
                "s1",
                &[TodoItem {
                    id: "1".into(),
                    content: "task".into(),
                    status: "pending".into(),
                }],
            )
            .unwrap();
        // A real blob file on disk, so the fs cleanup is exercised too.
        let blob = _d.path().join("blob-1");
        std::fs::write(&blob, b"snapshot").unwrap();
        store
            .checkpoint_add(
                "s1",
                "/tmp/f",
                Some(blob.to_str().unwrap()),
                None,
                8,
                "write_file",
                None,
            )
            .unwrap();

        store.delete_session("s1").unwrap();

        assert!(
            !blob.exists(),
            "the checkpoint blob must be unlinked, not left on disk"
        );

        assert_eq!(store.load_history("s1").unwrap().len(), 0, "messages");
        assert_eq!(
            store.usage_totals(Some("s1")).unwrap().calls,
            1,
            "usage rows are records and stay"
        );
        assert_eq!(store.todos_list("s1").unwrap().len(), 0, "todos orphaned");
        assert_eq!(
            store.checkpoint_list(Some("s1"), 10).unwrap().len(),
            0,
            "checkpoints orphaned"
        );
    }

    // ── skill usage (Step 2.3) ──────────────────────────────────────

    /// The `recent` index mode is only as honest as this query. The
    /// window is the last N DISTINCT SESSIONS that touched a skill —
    /// not the last N rows and not a wall-clock window — so a burst of
    /// uses in one session cannot crowd out every other session, and a
    /// fortnight away from the machine cannot empty the index.
    #[test]
    fn recent_skill_names_windows_by_session_not_by_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).expect("open store");

        // Oldest session, one skill.
        store.record_skill_use("ancient", "s1").unwrap();
        // Then a chatty session that used one skill many times.
        for _ in 0..20 {
            store.record_skill_use("chatty", "s2").unwrap();
        }
        // Then two more sessions.
        store.record_skill_use("middle", "s3").unwrap();
        store.record_skill_use("newest", "s4").unwrap();

        // A 2-session window sees s4 and s3 only.
        let two = store.recent_skill_names(2).unwrap();
        assert_eq!(
            two,
            vec!["newest".to_string(), "middle".to_string()],
            "most-recently-used first, windowed to 2 sessions"
        );

        // A 3-session window reaches the chatty one — and reports it
        // ONCE, however many rows it wrote.
        let three = store.recent_skill_names(3).unwrap();
        assert_eq!(
            three,
            vec![
                "newest".to_string(),
                "middle".to_string(),
                "chatty".to_string()
            ],
            "20 rows in one session is still one session: {three:?}"
        );
        assert!(
            !three.contains(&"ancient".to_string()),
            "the window really excludes older sessions"
        );

        // Nothing recorded at all → empty, not an error. This is the
        // fresh-install path that must degrade to the pointer line.
        let fresh = tempfile::tempdir().unwrap();
        let empty = Store::open(&fresh.path().join("state.db")).unwrap();
        assert!(empty.recent_skill_names(20).unwrap().is_empty());
    }

    /// The lifetime average and the last call answer DIFFERENT
    /// questions, and reporting only the first is how a working cache
    /// looks broken.
    ///
    /// A session opens cold (nothing cached), then runs warm. The
    /// lifetime mean drags that first miss along forever; the last call
    /// says what caching is doing NOW — which is also the number that
    /// moves the round a prefix regression lands.
    #[test]
    fn the_last_call_rate_is_not_the_lifetime_rate() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).expect("open store");
        let call = |p: u64, c: u64| CallTokens {
            prompt_tokens: p,
            completion_tokens: 10,
            cached_tokens: c,
            cache_write_tokens: 0,
            cache_reported: true,
            reasoning_tokens: None,
        };
        // Cold open, then three warm calls — the shape every session has.
        store.record_usage("s1", "p", "m", call(3000, 0)).unwrap();
        store
            .record_usage("s1", "p", "m", call(3400, 2700))
            .unwrap();
        store
            .record_usage("s1", "p", "m", call(3800, 3150))
            .unwrap();
        store
            .record_usage("s1", "p", "m", call(4200, 3600))
            .unwrap();

        let last = store.last_call_tokens("s1").unwrap().expect("a last call");
        assert_eq!(last.prompt_tokens, 4200, "the LAST row, not the first");
        let last_rate = last.cached_tokens as f64 / last.prompt_tokens as f64;

        let tot = store.usage_totals(Some("s1")).unwrap();
        let life_rate = tot.cached_tokens as f64 / tot.prompt_tokens as f64;

        assert!(last_rate > 0.85, "the warm rate is good: {last_rate}");
        assert!(
            life_rate < last_rate - 0.05,
            "and the lifetime mean understates it ({life_rate} vs {last_rate}) — \
             which is exactly why both are reported"
        );

        // A session with no calls has no last call, and must not be
        // reported as a 0% hit rate.
        assert!(store.last_call_tokens("never-ran").unwrap().is_none());
    }

    // ── triggers (plan item 1) ─────────────────────────────────────

    fn trow(name: &str) -> crate::store::TriggerRow {
        crate::store::TriggerRow {
            id: 0,
            name: name.into(),
            enabled: true,
            platform: Some("irc".into()),
            network: Some("irc.synirc.net".into()),
            channel: Some("#flashsupport".into()),
            event: "TEXT".into(),
            match_kind: "imageurl".into(),
            pattern: None,
            action: "vision".into(),
            arg: Some("describe it".into()),
            cooldown_secs: 30,
            per_hour_cap: 4,
            min_role: Some("guest".into()),
            fire_count: 0,
            last_fired_at: None,
            last_status: None,
        }
    }

    #[test]
    fn trigger_rows_round_trip_and_upsert_replaces_by_name() {
        let (_d, store) = scratch();
        let id1 = store.trigger_upsert(&trow("coven-vision")).unwrap();
        let mut edited = trow("coven-vision");
        edited.action = "slopdrop".into();
        edited.cooldown_secs = 99;
        let id2 = store.trigger_upsert(&edited).unwrap();
        assert_eq!(id1, id2, "upsert replaces by name, not a second row");
        let rows = store.trigger_list(false).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "slopdrop");
        assert_eq!(rows[0].cooldown_secs, 99);
        // enabled_only filters
        store.trigger_set_enabled("coven-vision", false).unwrap();
        assert_eq!(store.trigger_list(true).unwrap().len(), 0);
        assert_eq!(store.trigger_list(false).unwrap().len(), 1);
    }

    /// B1 — `fire_count` counted DECISIONS, not fires. Every refusal
    /// bumped it alongside the real ones, so `/triggers` reported
    /// `fires: 28` for a row that had fired 19 times and been refused 9,
    /// and `last_fired_at` moved on a fire that never happened. The
    /// ledger keeps every decision — that is what it is for — but the
    /// counter on the row has to mean what it says.
    #[test]
    fn a_refusal_is_logged_but_is_not_counted_as_a_fire() {
        let (_d, store) = scratch();
        let id = store.trigger_upsert(&trow("t")).unwrap();
        store
            .trigger_record_fire(id, "#coven", "fired", None)
            .unwrap();
        let after_fire = store.trigger_get("t").unwrap().unwrap();
        assert_eq!(after_fire.fire_count, 1);
        let fired_at = after_fire.last_fired_at.clone();
        assert!(fired_at.is_some());

        store
            .trigger_record_fire(id, "#coven", "refused", Some("mask: event not wanted"))
            .unwrap();
        store
            .trigger_record_fire(id, "#coven", "error", Some("slopdrop unreachable"))
            .unwrap();
        let row = store.trigger_get("t").unwrap().unwrap();
        assert_eq!(row.fire_count, 1, "only the fire counted");
        assert_eq!(
            row.last_fired_at, fired_at,
            "a refusal must not move last_fired_at — nothing fired"
        );
        // `last_status` is the last DECISION, which is honest for any of
        // them, and is how an operator sees the latest refusal.
        assert_eq!(row.last_status.as_deref(), Some("error"));
        // …and all three are still in the ledger, which is the record.
        assert_eq!(store.trigger_fire_history(None, 10).unwrap().len(), 3);
    }

    /// The fire log had no TTL while every other ledger in the same
    /// migration block had one. One row per DECISION, on an unthrottled
    /// always-match row in a live channel, is one row per line forever.
    #[test]
    fn the_fire_log_is_pruned_to_its_ttl_on_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        let id = {
            let store = Store::open(&path).expect("open");
            let id = store.trigger_upsert(&trow("t")).unwrap();
            store
                .trigger_record_fire(id, "#coven", "fired", None)
                .unwrap();
            store
                .trigger_record_fire(id, "#coven", "fired", None)
                .unwrap();
            // Backdate ONE of them past the window.
            store
                .conn
                .execute(
                    "UPDATE trigger_fires SET at = datetime('now','-90 days')
                     WHERE fire_id = (SELECT MIN(fire_id) FROM trigger_fires)",
                    [],
                )
                .unwrap();
            assert_eq!(store.trigger_fire_history(None, 10).unwrap().len(), 2);
            id
        };
        let _ = id;
        // Re-open: the prune runs where every other ledger's does.
        let store = Store::open(&path).expect("reopen");
        let left = store.trigger_fire_history(None, 10).unwrap();
        assert_eq!(left.len(), 1, "the aged row is gone, the recent one stays");
    }

    #[test]
    fn a_fire_is_recorded_with_its_reason_and_the_history_reads_back() {
        let (_d, store) = scratch();
        let id = store.trigger_upsert(&trow("t")).unwrap();
        store
            .trigger_record_fire(id, "#coven", "refused", Some("cooldown: 25s of 30s left"))
            .unwrap();
        store
            .trigger_record_fire(id, "#coven", "fired", None)
            .unwrap();
        let all = store.trigger_fire_history(None, 10).unwrap();
        assert_eq!(all.len(), 2);
        // newest first
        assert_eq!(all[0].status, "fired");
        assert_eq!(all[1].status, "refused");
        assert!(all[1].detail.as_deref().unwrap().contains("cooldown"));
        let row = store.trigger_get("t").unwrap().unwrap();
        // ONE fire, though two decisions were recorded: the refusal is
        // in the ledger below, not in the counter.
        assert_eq!(row.fire_count, 1);
        assert_eq!(row.last_status.as_deref(), Some("fired"));
        // by name
        let one = store.trigger_fire_history(Some("t"), 1).unwrap();
        assert_eq!(one.len(), 1);
        // delete clears both tables
        assert!(store.trigger_delete("t").unwrap());
        assert!(store.trigger_fire_history(None, 10).unwrap().is_empty());
    }

    #[test]
    fn mask_overrides_live_in_meta_and_auto_clears() {
        let (_d, store) = scratch();
        store
            .trigger_set_mask_override("irc.synirc.net", "#flashsupport", Some("TEXT,JOIN"))
            .unwrap();
        assert_eq!(
            store.trigger_mask_override("irc.synirc.net", "#flashsupport"),
            Some("TEXT,JOIN".into())
        );
        store
            .trigger_set_mask_override("irc.synirc.net", "#flashsupport", None)
            .unwrap();
        assert_eq!(
            store.trigger_mask_override("irc.synirc.net", "#flashsupport"),
            None
        );
    }

    /// The store must wait on lock contention, and wait as long as the
    /// OTHER connections to this file do.
    ///
    /// This is a pin on an implicit dependency, not on code we wrote: the
    /// 5s comes from rusqlite, which calls `sqlite3_busy_timeout(db, 5000)`
    /// unconditionally in its own open path (inner_connection.rs). store.rs
    /// never sets it — so nothing in this crate would notice if a rusqlite
    /// bump dropped the default, or if a future `open_with_flags` path
    /// bypassed it. SQLite's own default is 0, and at 0 the first racing
    /// writer gets SQLITE_BUSY immediately. memory.rs opens this SAME
    /// state.db on its own connection and sets 5s explicitly; the two must
    /// agree, or the patient one waits while the other fails on the same
    /// lock. Gateway, desktop API, CLI, IRC and every delegate_task child
    /// all open state.db, so contention is normal, not exotic.
    #[test]
    fn store_waits_on_lock_contention_as_long_as_memory_does() {
        let (_d, store) = scratch();
        let ms: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            ms, 5000,
            "Store must match memory.rs's busy_timeout; SQLite's own \
             default is 0 and rusqlite's is the only thing setting this"
        );
    }

    /// The behavioural half: opening a store whose file another connection
    /// currently holds a write lock on must SUCCEED by waiting.
    ///
    /// The calibration block is what gives this teeth — it proves the
    /// scenario actually contends, by showing the same race DOES fail at a
    /// zero timeout. Without it this test would pass even if the blocker
    /// never took the lock at all.
    #[test]
    fn open_waits_out_a_held_write_lock_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("state.db");
        // Seed the file so the blocker is not creating it.
        drop(Store::open(&db).expect("seed store"));

        /// Hold the write lock on `db` for `hold`, then commit.
        fn hold_write_lock(db: &Path, hold: Duration) -> JoinHandle<()> {
            let db = db.to_path_buf();
            std::thread::spawn(move || {
                let conn = Connection::open(&db).expect("blocker open");
                // IMMEDIATE takes the write lock up front; a plain BEGIN
                // would not, and then there would be nothing to contend
                // over.
                conn.execute_batch("BEGIN IMMEDIATE")
                    .expect("begin immediate");
                std::thread::sleep(hold);
                conn.execute_batch("COMMIT").expect("commit");
            })
        }

        // CALIBRATION: the same race with a zero timeout MUST fail. This is
        // the mutation-equivalent — if this assert ever stops failing, the
        // blocker is no longer contending and the test below is vacuous.
        let calib = hold_write_lock(&db, Duration::from_millis(400));
        std::thread::sleep(Duration::from_millis(100));
        let impatient = Connection::open(&db).expect("open");
        impatient.busy_timeout(Duration::ZERO).unwrap();
        let err = impatient
            .execute_batch("CREATE TABLE IF NOT EXISTS probe (x)")
            .expect_err("zero timeout must lose the race");
        assert!(
            matches!(err, rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::DatabaseBusy),
            "expected SQLITE_BUSY, got {err:?}"
        );
        calib.join().expect("calib blocker");

        // THE PROPERTY: the store waits the same lock out instead.
        let blocker = hold_write_lock(&db, Duration::from_millis(300));
        std::thread::sleep(Duration::from_millis(100));
        let store = Store::open(&db).expect("open must wait out the lock");
        blocker.join().expect("blocker");
        // And it is a working store, not a half-migrated one.
        store.create_session("s1").unwrap();
        assert_eq!(store.list_sessions(10).unwrap().len(), 1);
    }
}

#[cfg(test)]
mod b3a_store_pins {
    use super::*;

    /// B3a: OUR OWN row is never rewritten by gossip — only the
    /// heartbeat may move, and only forward. A peer's stale copy of us
    /// (old name, old caps) must not describe us to ourselves.
    #[test]
    fn own_row_is_never_rewritten_by_gossip() {
        let (_cfg, _data, _guard) = crate::testenv::scratch_xdg();
        // Mint an identity for this test process.
        let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        store
            .node_register(
                &me.id,
                "true-name",
                "h",
                "9",
                "ephemeral",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let incoming = GossipNodeRow {
            id: me.id.clone(),
            name: "renamed-by-peer".into(),
            host: "peerhost".into(),
            version: "0".into(),
            uptime_class: "always-on".into(),
            gpu: None,
            endpoint: None,
            static_ip: None,
            static_ip_source: None,
            caps: None,
            load1: None,
            load_at: None,
            sessions_served: 0,
            registered_at: "1999-01-01 00:00:00".into(),
            heartbeat_at: "2026-09-27 12:00:00".into(),
        };
        let touched = store.node_merge_from_gossip(&incoming).unwrap();
        assert!(!touched, "our own row is a no-op merge");
        let row = store.node_get(&me.id).unwrap().unwrap();
        assert_eq!(row.name, "true-name", "a peer cannot rename us");
    }

    /// The node pin (2026-10-22): survives upsert/get/list, defaults to
    /// NULL (a legacy floating job), and cron_update can re-pin or clear
    /// it. The gate that reads it lives in cron.rs's tick.
    #[test]
    fn cron_job_node_pin_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        let mut job = CronJob {
            id: "pinjob".into(),
            schedule: "every:3600".into(),
            prompt: "p".into(),
            deliver: "local".into(),
            enabled: true,
            node: Some("node-abc".into()),
            ..Default::default()
        };
        store.cron_upsert(&job).unwrap();
        let got = store.cron_get("pinjob").unwrap().unwrap();
        assert_eq!(got.node.as_deref(), Some("node-abc"));
        assert_eq!(
            store.cron_list(true).unwrap()[0].node.as_deref(),
            Some("node-abc"),
            "the list row carries the pin too"
        );
        // Re-pin via update.
        let upd = store
            .cron_update(
                "pinjob",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(Some("node-def")),
            )
            .unwrap()
            .unwrap();
        assert_eq!(upd.node.as_deref(), Some("node-def"));
        // Clear back to floating.
        let upd = store
            .cron_update(
                "pinjob",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(None),
            )
            .unwrap()
            .unwrap();
        assert!(upd.node.is_none(), "cleared = floating on the lease");
        // An unrelated update leaves the pin alone.
        job.node = Some("node-xyz".into());
        store.cron_upsert(&job).ok(); // id exists; use update instead
        let upd = store
            .cron_update(
                "pinjob",
                Some("renamed"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(upd.name.as_deref(), Some("renamed"));
        assert!(upd.node.is_none(), "no node arg = keep previous (NULL)");
    }

    /// The migration itself: a pre-node table gains the column as NULL,
    /// so every legacy job reads as floating, never as a bogus pin.
    #[test]
    fn cron_jobs_node_migration_defaults_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE cron_jobs (
                    id TEXT PRIMARY KEY, name TEXT, schedule TEXT NOT NULL,
                    prompt TEXT NOT NULL, deliver TEXT NOT NULL DEFAULT 'origin',
                    enabled INTEGER DEFAULT 1, script TEXT, skills TEXT,
                    no_agent INTEGER DEFAULT 0, sandbox INTEGER DEFAULT 0,
                    model TEXT, provider TEXT, reasoning TEXT, origin TEXT,
                    session TEXT, repeat_max INTEGER, run_count INTEGER DEFAULT 0,
                    created_at TEXT DEFAULT (datetime('now')),
                    updated_at TEXT DEFAULT (datetime('now')),
                    last_run_at TEXT, last_status TEXT, next_run_at TEXT
                );
                INSERT INTO cron_jobs (id, schedule, prompt) VALUES ('legacy', 'every:60', 'x');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let job = store.cron_get("legacy").unwrap().unwrap();
        assert!(job.node.is_none(), "a migrated row floats on the lease");
        // And the select with the new column did not misalign the rest.
        assert_eq!(job.schedule, "every:60");
        assert_eq!(job.prompt, "x");
    }
}
