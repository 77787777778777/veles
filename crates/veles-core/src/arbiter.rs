//! The singleton arbiter (Plan B, milestone B3a, 2026-09-27).
//!
//! The one hard problem in the compute-pool plan: exactly one node may
//! ever poll telegram, hold an IRC connection, or fire cron — "two bots
//! in one channel" is the catastrophic failure mode, not a degraded
//! mode. Consensus is unavailable at two nodes (no majority under
//! partition), so the 2-node answer is a **fail-closed static arbiter**:
//! ONE configured node id (`[cluster] arbiter`), the lease rows in that
//! node's own store, and if the arbiter is unreachable NOBODY runs the
//! singletons. A degraded pool that stays silent is recoverable; two
//! pollers is not. B3b replaces the implementation behind the same
//! trait with a Raft leader lease once ≥3 always-on nodes exist — the
//! seam is the point.
//!
//! ## The lease is a lease, not a fencing token
//!
//! A fencing token only works when the protected resource checks it,
//! and Telegram, an IRC server and a cron side effect cannot. A holder
//! that was PAUSED past expiry — a laptop suspended mid-job is exactly
//! that — resumes believing it still holds the lease. So:
//!
//!   - the holder RE-VALIDATES immediately before every side effect
//!     (never "I checked when the loop started");
//!   - renewals happen well inside the TTL;
//!   - [`LeaseGuard::held`] is EXPIRY-AWARE: `held()` returning true
//!     once means nothing 30 seconds later, and callers must not cache
//!     the answer across a side effect.
//!
//! ## What "fail closed" costs, on purpose
//!
//! With the arbiter down, a healthy non-arbiter node stays silent. That
//! is the design (gap #13): at two nodes there is no tiebreaker that
//! cannot double-poll, so the silence is the safe answer. The lease's
//! TTL bounds how long an arbiter's crash silences the pool: once the
//! row expires, the FIRST node to ask the restarted arbiter re-grants.
//!
//! ## Cron's exactly-once does not move
//!
//! Cron already claims `(job_id, slot)` through the durable
//! `inbound_ledger` BEFORE firing (`claim_inbound`). The lease decides
//! whether this node runs the scheduler AT ALL; the ledger keeps
//! exactly-once per slot. Both layers stay: the lease without the
//! ledger would let a lease flap re-fire a slot, the ledger without the
//! lease would run on two nodes.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Marker lines for the arbiter wire protocol (framed by `gossip`).
pub const LEASE_ASK_MARKER: &str = "[veles lease ask]";
pub const LEASE_ANSWER_MARKER: &str = "[veles lease answer]";
pub const GOSSIP_REPLY_MARKER: &str = "[veles cluster gossip reply]";

/// How long a lease lasts before it is nobody's. Renewals land well
/// inside this; side effects re-validate against the store row.
pub const LEASE_TTL_SECS: i64 = 90;

/// The lease keys — one per singleton duty. IRC keys are per network
/// (`singleton:irc:<net>`); everything else is one duty.
pub fn lease_key_cron() -> String {
    "singleton:cron".to_string()
}
pub fn lease_key_telegram() -> String {
    "singleton:telegram".to_string()
}
pub fn lease_key_atproto() -> String {
    "singleton:atproto".to_string()
}
pub fn lease_key_irc(net: &str) -> String {
    format!("singleton:irc:{net}")
}

/// What an ask of the arbiter came back with.
#[derive(Debug, Clone, PartialEq)]
pub enum LeaseState {
    /// We hold it until this instant (RFC 3339 UTC).
    Held(String),
    /// Held by another node until then.
    HeldByOther { holder: String, until: String },
    /// Not granted: arbiter unreachable, not configured, or refused.
    /// The string says why — it reaches the operator, not just a log.
    Refused(String),
}

/// The seam (B3a → B3b): what a node asks to run a singleton.
///
/// `StaticArbiter` implements this against the local store when this
/// node IS the arbiter, and over the authenticated A2A link when it is
/// not. A `RaftArbiter` (B3b) slots in behind the same trait without a
/// config rewrite — the same deferral pattern as `node::Bootstrap`.
#[async_trait::async_trait]
pub trait Arbiter: Send + Sync {
    /// Ask for (or renew) the lease on `key` for `holder`.
    async fn try_acquire_lease(&self, key: &str, holder: &str) -> LeaseState;
    /// Read a lease without claiming it (status displays).
    async fn lease_status(&self, key: &str) -> LeaseState;
}

/// The B3a implementation: fail-closed static arbitration.
///
/// When this node is the configured arbiter, leases live in the LOCAL
/// store — no network in the path, so an arbiter node keeps its own
/// singletons alive even while its A2A link is down (which is correct:
/// the arbiter asking itself is not a partition). When it is NOT, every
/// ask is one authenticated A2A call to the arbiter node; unreachable
/// means [`LeaseState::Refused`], never a local fallback.
pub struct StaticArbiter {
    store: Arc<std::sync::Mutex<crate::store::Store>>,
    /// Kept for the B3b seam: a Raft arbiter will need cluster config
    /// (voter set, timeouts) this static one does not read today.
    #[allow(dead_code)]
    cfg: Arc<crate::config::VelesConfig>,
    /// The resolved arbiter node id (validated at boot).
    arbiter_id: String,
    /// This node's id.
    me: String,
    /// How to reach the arbiter over A2A: (url, bearer).
    arbiter_endpoint: Option<(String, Option<String>)>,
}

impl StaticArbiter {
    /// The resolved arbiter node id (display + fence logging).
    pub fn arbiter_id(&self) -> &str {
        &self.arbiter_id
    }

    /// Is THIS node the arbiter (leases served locally)?
    pub fn is_local(&self) -> bool {
        self.arbiter_endpoint.is_none()
    }

    /// Resolve the arbiter and this node's identity. Errors are the
    /// fail-closed refusal, said out loud, at BOOT — the
    /// catastrophic-misconfiguration case (`[cluster] arbiter` unset or
    /// unresolvable) must not wait for the 3am partition to surface.
    pub fn new(
        store: Arc<std::sync::Mutex<crate::store::Store>>,
        cfg: Arc<crate::config::VelesConfig>,
    ) -> Result<Self> {
        let arbiter_id = {
            let s = store.lock().unwrap_or_else(|p| p.into_inner());
            crate::node::validate_arbiter(&cfg.cluster, &s)?
        };
        let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?.id;
        // How to reach the arbiter when it is not us: its registry
        // endpoint, plus a bearer from [a2a_agents] if one is named.
        let arbiter_endpoint = if arbiter_id == me {
            None
        } else {
            let s = store.lock().unwrap_or_else(|p| p.into_inner());
            let row = s
                .node_get(&arbiter_id)?
                .with_context(|| format!("arbiter node {arbiter_id} vanished from the registry"))?;
            match row.endpoint.as_deref() {
                Some(e) => {
                    let bearer = Self::bearer_for_peer(&cfg, e);
                    Some((e.to_string(), bearer))
                }
                None => anyhow::bail!(
                    "arbiter node {arbiter_id} ({}) has no endpoint — a non-arbiter node \
                     cannot ask it for leases. Set [node] endpoint on it.",
                    row.name
                ),
            }
        };
        Ok(Self {
            store,
            cfg,
            arbiter_id,
            me,
            arbiter_endpoint,
        })
    }

    /// The bearer for an A2A call to `url`, from `[a2a_agents]` entries
    /// whose url matches. Named-but-unmatched is honest None.
    fn bearer_for_peer(cfg: &crate::config::VelesConfig, url: &str) -> Option<String> {
        cfg.a2a_agents
            .iter()
            .find(|(_, p)| p.url.trim_end_matches('/') == url.trim_end_matches('/'))
            .and_then(|(_, p)| {
                let t = p.auth.token.trim();
                (!t.is_empty()).then(|| t.to_string())
            })
    }
}

/// The arbiter's side of an acquire, against its own store — the local
/// path and the A2A server both answer through this. Two kinds of key
/// carry rules of their own (PLAN-pool-round.md C2):
///
/// - `node:<id>`, a node's liveness lease, is only ever `<id>`'s;
/// - `takeover:<owner>:<session>` is granted only once the owner's node
///   lease has run out — never to a node that has not held one (it does
///   not take part in failover), never while it is alive — and it lasts
///   [`crate::takeover::TAKEOVER_TTL_SECS`], so the owner can check it on
///   its return. The first survivor to ask gets it; the next is told who.
pub fn grant_local(
    store: &crate::store::Store,
    key: &str,
    holder: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> LeaseState {
    let mut ttl = LEASE_TTL_SECS;
    if let Some(node) = key.strip_prefix("node:") {
        if node != holder {
            return LeaseState::Refused(format!("{holder} may not hold {node}'s node lease"));
        }
    }
    if let Some(rest) = key.strip_prefix("takeover:") {
        let Some((owner, _session)) = rest.split_once(':') else {
            return LeaseState::Refused(format!("{key}: not takeover:<node>:<session>"));
        };
        if owner == holder {
            return LeaseState::Refused("a node does not take over its own session".into());
        }
        match store.lease_row(&crate::takeover::node_key(owner)) {
            Err(e) => return LeaseState::Refused(format!("arbiter store error: {e:#}")),
            Ok(None) => {
                return LeaseState::Refused(format!(
                    "node {owner} has never held a node lease — failover is off there. Move \
                     the session from it with `veles session migrate`, or continue a copy with \
                     `veles session fork-replica`"
                ))
            }
            Ok(Some((_, exp))) if exp > now => {
                return LeaseState::Refused(format!(
                    "node {owner} is alive — its node lease runs until {}",
                    exp.to_rfc3339()
                ))
            }
            Ok(Some(_)) => {}
        }
        ttl = crate::takeover::TAKEOVER_TTL_SECS;
    }
    match store.lease_grant(key, holder, now, ttl) {
        Ok(Some((_, exp))) => LeaseState::Held(exp),
        Ok(None) => store
            .lease_get(key, now)
            .ok()
            .flatten()
            .map(|(h, _, u)| LeaseState::HeldByOther {
                holder: h,
                until: u,
            })
            .unwrap_or_else(|| LeaseState::Refused("lease grant raced".to_string())),
        Err(e) => LeaseState::Refused(format!("arbiter store error: {e:#}")),
    }
}

#[async_trait::async_trait]
impl Arbiter for StaticArbiter {
    async fn try_acquire_lease(&self, key: &str, holder: &str) -> LeaseState {
        if self.is_local() {
            // We are the arbiter: the lease is local, and asking
            // ourselves is not a partition.
            let s = self.store.lock().unwrap_or_else(|p| p.into_inner());
            grant_local(&s, key, holder, chrono::Utc::now())
        } else {
            self.remote_ask(key, holder, true).await
        }
    }

    async fn lease_status(&self, key: &str) -> LeaseState {
        if self.me == self.arbiter_id {
            let s = self.store.lock().unwrap_or_else(|p| p.into_inner());
            return match s.lease_get(key, chrono::Utc::now()) {
                Ok(Some((holder, _, exp))) => {
                    if holder == self.me {
                        LeaseState::Held(exp)
                    } else {
                        LeaseState::HeldByOther { holder, until: exp }
                    }
                }
                Ok(None) => LeaseState::Refused("no lease".into()),
                Err(e) => LeaseState::Refused(format!("store error: {e:#}")),
            };
        }
        self.remote_ask(key, &self.me, false).await
    }
}

impl StaticArbiter {
    /// One ask over the A2A link. The wire protocol rides marker-framed
    /// message/send (the memsync/configsync transport shape), answered
    /// inline — no session, no turn, no inbox (a lease ask is a
    /// question, not data to apply later).
    async fn remote_ask(&self, key: &str, holder: &str, acquire: bool) -> LeaseState {
        let Some((url, bearer)) = &self.arbiter_endpoint else {
            return LeaseState::Refused(
                "no route to the arbiter (this node is not the arbiter and has no endpoint for it)"
                    .to_string(),
            );
        };
        let msg = crate::gossip::encode_lease_ask(key, holder, acquire);
        let client = crate::a2a::A2AClient::shared();
        let ctx = crate::a2a::new_context_id();
        let result = client.call(url, bearer.as_deref(), &msg, &ctx).await;
        match result {
            Ok(v) => crate::gossip::decode_lease_answer(&crate::a2a::extract_text(&v)),
            Err(e) => LeaseState::Refused(format!("arbiter unreachable: {e:#}")),
        }
    }
}

/// How much earlier than the arbiter's expiry a holder stops acting —
/// room for the ask's own round trip and for a tick that started just
/// before the deadline.
pub const LEASE_MARGIN_SECS: u64 = 5;

/// A clock that keeps counting through system SUSPEND, read locally.
///
/// The lease deadline is judged on THIS, never on the arbiter's
/// timestamp: B3a compared the arbiter's absolute expiry with the
/// holder's wall clock, so a holder whose clock ran behind acted past
/// the arbiter's expiry — two holders. `Instant` would fix the skew but
/// stops while a laptop sleeps (CLOCK_MONOTONIC), which is exactly the
/// suspended holder the fence exists for. CLOCK_BOOTTIME does both;
/// elsewhere the local wall clock is the fallback (still local, so still
/// free of cross-node skew).
fn boot_clock() -> std::time::Duration {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime writes one timespec we own.
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } == 0 {
            return std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32);
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
}

/// A singleton lease as seen by the node that wants it — the handle
/// singleton loops hold. The RENEW LOOP lives here so no caller
/// re-implements "renew well inside the TTL" (and gets it wrong by
/// renewing AT the expiry).
///
/// **The rule:** a holder may act until `sent + TTL − margin`, where
/// `sent` is when the ask that was last answered `Held` LEFT this node
/// (on [`boot_clock`]). The arbiter granted no earlier than that, so
/// the arbiter's own expiry is later — and it grants nobody else before
/// its expiry. Acting inside that window is therefore safe whatever the
/// next answer is, which is why a failed renewal needs no special case:
/// the window simply runs out.
///
/// **It never gives up.** B3a's loop returned on the first non-Held
/// answer, and a node refused at boot never asked again — one network
/// blip on the holder stopped cron across the pool until restarts. Now
/// the loop keeps asking every TTL/3: a holder that lost a renewal
/// regains the lease on the next one, and a STANDBY (refused because
/// another node held it) takes over once that node's lease expires.
///
/// The guard itself is passive: cron re-validates [`LeaseGuard::held`]
/// before every tick, and the long-lived duties (telegram's poller, each
/// IRC connection, each process plugin instance) run under
/// [`LeaseGuard::supervise`],
/// which starts them while held, stops them at the local deadline, and
/// starts them again when the lease comes back.
pub struct LeaseGuard {
    key: String,
    holder: String,
    /// Boot-clock deadline from the last Held answer; None = never held.
    deadline: std::sync::Mutex<Option<std::time::Duration>>,
    /// The arbiter's own expiry string from that answer — display only.
    until: std::sync::Mutex<String>,
    /// The last non-Held answer since the last Held one, for status
    /// displays and log lines.
    lost_why: std::sync::Mutex<Option<String>>,
}

impl LeaseGuard {
    /// A guard that does not hold the lease (yet).
    /// Tests: this guard holds its lease for a full window from now.
    #[cfg(test)]
    pub(crate) fn hold_for_test(&self) {
        self.apply(boot_clock(), LeaseState::Held("test".into()));
    }

    pub fn standby(key: &str, holder: &str) -> Self {
        Self {
            key: key.to_string(),
            holder: holder.to_string(),
            deadline: std::sync::Mutex::new(None),
            until: std::sync::Mutex::new(String::new()),
            lost_why: std::sync::Mutex::new(None),
        }
    }

    /// Ask once, now, and return the guard either way: holding on
    /// `Held`, a standby otherwise. Pair with [`Self::spawn_renewal`].
    pub async fn acquire(arbiter: &dyn Arbiter, key: &str, holder: &str) -> Arc<Self> {
        let guard = Arc::new(Self::standby(key, holder));
        guard.ask_now(arbiter).await;
        guard
    }

    /// Ask once, now, and fold the answer in.
    pub async fn ask_now(&self, arbiter: &dyn Arbiter) {
        let sent = boot_clock();
        let answer = arbiter.try_acquire_lease(&self.key, &self.holder).await;
        self.apply(sent, answer);
    }

    /// May this holder act NOW? Re-read before every side effect; the
    /// answer is worth nothing 30 seconds later.
    pub fn held(&self) -> bool {
        let d = *self.deadline.lock().unwrap_or_else(|p| p.into_inner());
        d.is_some_and(|d| boot_clock() < d)
    }

    /// Why the last ask was not granted, while that is still the news.
    pub fn lost_reason(&self) -> Option<String> {
        self.lost_why
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Wait until a lease this process HELD is no longer held: the
    /// held→lost transition (the arbiter granted it elsewhere) or the
    /// local deadline expiring without a renewal. Resolves immediately
    /// when already lost; NEVER resolves for a guard that never held
    /// (a standby's teardown signal was its non-start). The teardown
    /// half of the B3a tail: the adapter runtime that awaits this
    /// selects it against its own run() and exits, so a lost singleton
    /// stops instead of doubling with the new holder.
    ///
    /// Checked every second against the boot clock. It polled at the
    /// renewal cadence (30 s) until 2026-09-29, so a holder noticed its
    /// deadline up to 30 s late — past the arbiter's expiry (deadline +
    /// margin), overlapping the next holder by up to ~25 s. A sleep
    /// until the deadline would not do either: tokio's clock stops while
    /// the machine is suspended and the boot clock does not, so a laptop
    /// waking long after its lease ran out would keep polling for
    /// whatever that sleep had left.
    pub async fn wait_lost(&self) {
        while self.held() || !self.ever_held() {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    /// Wait until this node holds the lease — at once if it does now.
    /// The renewal loop ([`Self::spawn_renewal`]) is what turns a
    /// standby into a holder; this only notices.
    pub async fn wait_held(&self) {
        while !self.held() {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    /// Run a singleton duty whenever this node holds its lease: start it
    /// once held, stop it (drop its future) the moment the lease is lost,
    /// and start it again when the lease comes back — so a standby takes
    /// over when the holder goes away, and a holder that lost a renewal
    /// to a blip resumes instead of staying silent until a restart (the
    /// C1/C2 teardown stopped duties and nothing ever started them
    /// again). `start` builds a fresh run each time.
    ///
    /// Returns only when the duty ends on its own (its future completes):
    /// an adapter that exits or fails is not restarted here — that is its
    /// own policy, as it is without a lease.
    pub async fn supervise<F, Fut>(&self, what: &str, mut start: F) -> anyhow::Result<()>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<()>>,
    {
        loop {
            if !self.held() {
                tracing::info!(
                    "{what}: standing by until this node holds lease {}",
                    self.key
                );
            }
            self.wait_held().await;
            tracing::info!("{what}: lease {} held — starting", self.key);
            tokio::select! {
                r = start() => return r,
                () = self.wait_lost() => {
                    tracing::warn!(
                        "{what}: lease {} lost — stopped (another node may hold it now)",
                        self.key
                    );
                }
            }
        }
    }

    /// Did this guard ever hold (a boot-time refusal = no)?
    fn ever_held(&self) -> bool {
        self.deadline
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// The arbiter's expiry from the last grant (display only).
    pub fn until(&self) -> String {
        self.until.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Fold one answer in. `sent` is when that ask left this node.
    fn apply(&self, sent: std::time::Duration, answer: LeaseState) {
        let was = self.held();
        match answer {
            LeaseState::Held(until) => {
                let ttl = std::time::Duration::from_secs(LEASE_TTL_SECS as u64);
                let margin = std::time::Duration::from_secs(LEASE_MARGIN_SECS);
                *self.deadline.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(sent + ttl.saturating_sub(margin));
                *self.until.lock().unwrap_or_else(|p| p.into_inner()) = until;
                *self.lost_why.lock().unwrap_or_else(|p| p.into_inner()) = None;
                if !was {
                    tracing::info!("lease {} held by {}", self.key, self.holder);
                }
            }
            other => {
                // The deadline stays: it is inside the arbiter's expiry
                // of the last grant, so it runs out on its own.
                let why = format!("{other:?}");
                let first = self
                    .lost_why
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .replace(why.clone())
                    .is_none();
                if first {
                    tracing::warn!(
                        "lease {} not granted to {}: {why} — asking again every {}s",
                        self.key,
                        self.holder,
                        (LEASE_TTL_SECS / 3).max(5)
                    );
                }
            }
        }
    }

    /// Ask again every TTL/3, forever: renew while held, take over when
    /// free. The first ask of this loop is one interval out — call
    /// [`Self::acquire`] first for the immediate one.
    pub fn spawn_renewal(
        self: &Arc<Self>,
        arbiter: Arc<dyn Arbiter>,
    ) -> tokio::task::JoinHandle<()> {
        let guard = self.clone();
        crate::rt::spawn(async move {
            let every = std::time::Duration::from_secs((LEASE_TTL_SECS / 3).max(5) as u64);
            loop {
                tokio::time::sleep(every).await;
                let sent = boot_clock();
                let answer = arbiter.try_acquire_lease(&guard.key, &guard.holder).await;
                guard.apply(sent, answer);
            }
        })
    }
}

/// The wire ask/answer (rides gossip's marker framing).
#[derive(Serialize, Deserialize, Debug)]
pub struct LeaseAskWire {
    pub key: String,
    pub holder: String,
    pub acquire: bool,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct LeaseAnswerWire {
    /// held | held_by_other | refused
    pub state: String,
    #[serde(default)]
    pub holder: String,
    #[serde(default)]
    pub until: String,
    #[serde(default)]
    pub why: String,
}

impl From<LeaseState> for LeaseAnswerWire {
    fn from(s: LeaseState) -> Self {
        match s {
            LeaseState::Held(u) => LeaseAnswerWire {
                state: "held".into(),
                holder: String::new(),
                until: u,
                why: String::new(),
            },
            LeaseState::HeldByOther { holder, until } => LeaseAnswerWire {
                state: "held_by_other".into(),
                holder,
                until,
                why: String::new(),
            },
            LeaseState::Refused(why) => LeaseAnswerWire {
                state: "refused".into(),
                holder: String::new(),
                until: String::new(),
                why,
            },
        }
    }
}

impl From<LeaseAnswerWire> for LeaseState {
    fn from(w: LeaseAnswerWire) -> Self {
        match w.state.as_str() {
            "held" => LeaseState::Held(w.until),
            "held_by_other" => LeaseState::HeldByOther {
                holder: w.holder,
                until: w.until,
            },
            _ => LeaseState::Refused(w.why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (
        tempfile::TempDir,
        Arc<std::sync::Mutex<crate::store::Store>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        (dir, store)
    }

    fn cfg_with_arbiter(id: &str) -> Arc<crate::config::VelesConfig> {
        let mut cfg = crate::config::VelesConfig::default();
        cfg.cluster.arbiter = Some(id.to_string());
        Arc::new(cfg)
    }

    /// The core property: two nodes ask, exactly one wins. The loser's
    /// answer names the winner and the expiry.
    #[test]
    fn a_second_holder_is_refused_while_the_first_is_unexpired() {
        let (_d, store) = scratch();
        let now = chrono::Utc::now();
        assert!(store
            .lock()
            .unwrap()
            .lease_grant("singleton:cron", "node-a", now, 90)
            .unwrap()
            .is_some());
        let r = store
            .lock()
            .unwrap()
            .lease_grant("singleton:cron", "node-b", now, 90)
            .unwrap();
        assert!(
            r.is_none(),
            "fail closed: node-b must not run the singleton"
        );
    }

    /// The same holder renews: a poller asking every TTL/3 must never
    /// be refused by its own lease.
    #[test]
    fn the_same_holder_renews_freely() {
        let (_d, store) = scratch();
        let now = chrono::Utc::now();
        store
            .lock()
            .unwrap()
            .lease_grant("singleton:telegram", "node-a", now, 90)
            .unwrap();
        let later = now + chrono::Duration::seconds(30);
        assert!(store
            .lock()
            .unwrap()
            .lease_grant("singleton:telegram", "node-a", later, 90)
            .unwrap()
            .is_some());
    }

    /// An expired row is NOBODY's lease — the arbiter's death silences
    /// the pool only until the TTL runs out, then the first ask re-grants.
    #[test]
    fn an_expired_lease_is_nobodys() {
        let (_d, store) = scratch();
        let t0 = chrono::Utc::now();
        store
            .lock()
            .unwrap()
            .lease_grant("singleton:irc:libera", "node-a", t0, 90)
            .unwrap();
        let after = t0 + chrono::Duration::seconds(91);
        // Reads as unheld…
        assert!(store
            .lock()
            .unwrap()
            .lease_get("singleton:irc:libera", after)
            .unwrap()
            .is_none());
        // …and re-grants to the first asker, including a different node.
        assert!(store
            .lock()
            .unwrap()
            .lease_grant("singleton:irc:libera", "node-b", after, 90)
            .unwrap()
            .is_some());
    }

    /// The suspended-holder case the plan calls out: the process was
    /// paused past expiry and resumes believing it still holds. The
    /// GUARD must refuse — its own expiry field says no — without any
    /// network round trip.
    #[test]
    fn a_suspended_holder_resumes_refused() {
        // Granted by an ask that left TTL+1 s ago on the boot clock —
        // which keeps counting through suspend.
        let guard = LeaseGuard::standby("singleton:cron", "node-a");
        let long_ago =
            boot_clock().saturating_sub(std::time::Duration::from_secs(LEASE_TTL_SECS as u64 + 1));
        guard.apply(
            long_ago,
            LeaseState::Held("2099-01-01T00:00:00+00:00".into()),
        );
        assert!(!guard.held(), "an expired guard must not act");
    }

    /// The deadline is judged on THIS node's clock from when the ask
    /// left, never on the arbiter's timestamp: B3a compared the
    /// arbiter's absolute expiry with the holder's wall clock, so a
    /// clock running behind kept acting past the arbiter's expiry.
    #[test]
    fn the_deadline_is_local_whatever_the_arbiters_clock_says() {
        // Arbiter's clock an hour AHEAD, grant old: not held.
        let g = LeaseGuard::standby("singleton:cron", "n");
        let stale =
            boot_clock().saturating_sub(std::time::Duration::from_secs(LEASE_TTL_SECS as u64));
        let an_hour_ahead = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        g.apply(stale, LeaseState::Held(an_hour_ahead));
        assert!(
            !g.held(),
            "the arbiter's timestamp must not extend our window"
        );
        // Arbiter's clock an hour BEHIND, grant fresh: held.
        let g = LeaseGuard::standby("singleton:cron", "n");
        let an_hour_behind = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        g.apply(boot_clock(), LeaseState::Held(an_hour_behind));
        assert!(g.held(), "nor its lag cut a fresh grant short");
    }

    /// A release by anyone but the holder must not remove the winner's
    /// lease (a late race loser releasing what it never won).
    #[test]
    fn only_the_holder_releases() {
        let (_d, store) = scratch();
        let now = chrono::Utc::now();
        store
            .lock()
            .unwrap()
            .lease_grant("singleton:cron", "node-a", now, 90)
            .unwrap();
        store
            .lock()
            .unwrap()
            .lease_release("singleton:cron", "node-b")
            .unwrap();
        assert!(
            store
                .lock()
                .unwrap()
                .lease_get("singleton:cron", now)
                .unwrap()
                .is_some(),
            "node-b's release must not touch node-a's lease"
        );
    }

    /// The wire round trip keeps the three states distinguishable.
    #[test]
    fn lease_wire_round_trip() {
        for state in [
            LeaseState::Held("t".into()),
            LeaseState::HeldByOther {
                holder: "n2".into(),
                until: "t2".into(),
            },
            LeaseState::Refused("down".into()),
        ] {
            let wire: LeaseAnswerWire = state.clone().into();
            let back: LeaseState = wire.into();
            assert_eq!(back, state);
        }
    }

    /// Fail closed at CONSTRUCTION: an unresolvable arbiter id is a
    /// boot-time refusal, not a runtime surprise.
    #[test]
    fn an_unresolvable_arbiter_refuses_at_construction() {
        let (_d, store) = scratch();
        let cfg = cfg_with_arbiter("zzzz");
        assert!(StaticArbiter::new(store, cfg).is_err());
    }

    /// When this node IS the arbiter, asks stay LOCAL: the endpoint
    /// lookup is skipped entirely (an arbiter asking itself is not a
    /// partition, and dialing itself would be a self-call loop).
    #[test]
    fn the_arbiter_node_asks_itself_locally() {
        let (_dir, _g) = crate::testenv::scratch_home();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&crate::config::data_dir().join("state.db")).unwrap(),
        ));
        let arbiter_id = "aaaa-1111";
        {
            let s = store.lock().unwrap();
            s.node_register(
                arbiter_id,
                "vps",
                "h",
                "0",
                "always-on",
                None,
                Some("http://127.0.0.1:9900"),
                None,
                None,
            )
            .unwrap();
            s.node_register(
                "bbbb-2222",
                "laptop",
                "h2",
                "0",
                "ephemeral",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        }
        std::fs::write(
            crate::config::config_dir().join("node.toml"),
            format!("id = \"{arbiter_id}\"\nname = \"vps\"\n"),
        )
        .unwrap();
        let cfg = cfg_with_arbiter(arbiter_id);
        let arb = StaticArbiter::new(store, cfg).unwrap();
        assert!(arb.arbiter_endpoint.is_none(), "self-arbitration is local");
        // And a lease ask answers locally.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let state = rt.block_on(arb.try_acquire_lease(&lease_key_cron(), arbiter_id));
        assert!(matches!(state, LeaseState::Held(_)), "got {state:?}");
    }

    /// A non-arbiter with no endpoint for the arbiter is a
    /// construction-time refusal — it could never ask.
    #[test]
    fn a_non_arbiter_without_a_route_refuses_at_construction() {
        let (_dir, _g) = crate::testenv::scratch_home();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&crate::config::data_dir().join("state.db")).unwrap(),
        ));
        let arbiter_id = "aaaa-1111";
        {
            let s = store.lock().unwrap();
            s.node_register(
                arbiter_id,
                "vps",
                "h",
                "0",
                "always-on",
                None,
                None,
                None,
                None,
            )
            .unwrap();
            s.node_register(
                "bbbb-2222",
                "laptop",
                "h2",
                "0",
                "ephemeral",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        }
        std::fs::write(
            crate::config::config_dir().join("node.toml"),
            "id = \"bbbb-2222\"\nname = \"laptop\"\n",
        )
        .unwrap();
        let cfg = cfg_with_arbiter(arbiter_id);
        let err = match StaticArbiter::new(store, cfg) {
            Ok(_) => panic!("a non-arbiter with no route must refuse at construction"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("endpoint"),
            "refusal names the missing piece: {err}"
        );
    }
}

#[cfg(test)]
mod b3a_pins {
    use super::*;
    use std::sync::Arc;

    /// An arbiter whose answers the test scripts, one per ask.
    struct Scripted(std::sync::Mutex<std::collections::VecDeque<LeaseState>>);
    #[async_trait::async_trait]
    impl Arbiter for Scripted {
        async fn try_acquire_lease(&self, _k: &str, _h: &str) -> LeaseState {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(LeaseState::Refused("script ran out".into()))
        }
        async fn lease_status(&self, _k: &str) -> LeaseState {
            LeaseState::Refused("unused".into())
        }
    }
    fn held() -> LeaseState {
        LeaseState::Held("t".into())
    }
    fn other() -> LeaseState {
        LeaseState::HeldByOther {
            holder: "other".into(),
            until: "t".into(),
        }
    }

    /// A lost renewal is not the end: B3a's loop RETURNED on the first
    /// non-Held answer, so one blip on the holder left cron dead across
    /// the pool until a restart. The loop keeps asking and a later grant
    /// counts. Paused tokio time: the loop's sleeps are virtual.
    #[tokio::test(start_paused = true)]
    async fn a_holder_that_misses_a_renewal_regains_the_lease() {
        let arb = Arc::new(Scripted(std::sync::Mutex::new(
            vec![held(), LeaseState::Refused("unreachable".into()), held()].into(),
        )));
        let guard = LeaseGuard::acquire(arb.as_ref(), "singleton:cron", "n").await;
        assert!(guard.held(), "the first grant holds");
        let _h = guard.spawn_renewal(arb.clone());
        // Sleeping (not `advance` + one yield) lets paused time run every
        // earlier timer first, so the renewal's ask has landed by now.
        tokio::time::sleep(std::time::Duration::from_secs(31)).await;
        assert!(guard.lost_reason().is_some(), "the refusal is visible");
        // Sleeping (not `advance` + one yield) lets paused time run every
        // earlier timer first, so the renewal's ask has landed by now.
        tokio::time::sleep(std::time::Duration::from_secs(31)).await;
        assert!(guard.held(), "and the next grant is held again");
        assert!(
            guard.lost_reason().is_none(),
            "the loss is cleared by the grant"
        );
    }

    /// A node refused at boot (another holds the lease) is a STANDBY:
    /// it keeps asking and takes over once the lease frees. B3a asked
    /// once and gave up, so the singleton never moved.
    #[tokio::test(start_paused = true)]
    async fn a_standby_takes_over_when_the_lease_frees() {
        let arb = Arc::new(Scripted(std::sync::Mutex::new(
            vec![other(), other(), held()].into(),
        )));
        let guard = LeaseGuard::acquire(arb.as_ref(), "singleton:cron", "n").await;
        assert!(!guard.held(), "refused at boot: standby");
        let _h = guard.spawn_renewal(arb.clone());
        // Sleeping (not `advance` + one yield) lets paused time run every
        // earlier timer first, so the renewal's ask has landed by now.
        tokio::time::sleep(std::time::Duration::from_secs(31)).await;
        assert!(!guard.held(), "still held by the other node");
        // Sleeping (not `advance` + one yield) lets paused time run every
        // earlier timer first, so the renewal's ask has landed by now.
        tokio::time::sleep(std::time::Duration::from_secs(31)).await;
        assert!(guard.held(), "the lease freed and the standby took it");
    }

    /// A refused renewal never EXTENDS the window, and a HeldByOther
    /// answer never makes a standby act.
    #[test]
    fn only_a_held_answer_opens_the_window() {
        let g = LeaseGuard::standby("singleton:cron", "n");
        g.apply(boot_clock(), other());
        assert!(!g.held());
        g.apply(boot_clock(), LeaseState::Refused("down".into()));
        assert!(!g.held());
        g.apply(boot_clock(), held());
        assert!(g.held());
    }
}

#[cfg(test)]
mod b3a_tail_pins {
    use super::*;
    use std::collections::VecDeque;

    /// An arbiter whose answers the test scripts, one per ask.
    struct Scripted(std::sync::Mutex<VecDeque<LeaseState>>);
    #[async_trait::async_trait]
    impl Arbiter for Scripted {
        async fn try_acquire_lease(&self, _k: &str, _h: &str) -> LeaseState {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(LeaseState::Refused("script ran out".into()))
        }
        async fn lease_status(&self, _k: &str) -> LeaseState {
            LeaseState::Refused("unused".into())
        }
    }
    fn held() -> LeaseState {
        LeaseState::Held("t".into())
    }
    fn other() -> LeaseState {
        LeaseState::HeldByOther {
            holder: "someone-else".into(),
            until: "t".into(),
        }
    }
    /// A grant sent far enough in the past that its local deadline
    /// (sent + TTL − margin) is already expired.
    fn stale_grace() -> std::time::Duration {
        std::time::Duration::from_secs((LEASE_TTL_SECS + LEASE_MARGIN_SECS as i64 + 10) as u64)
    }

    /// The B3a tail teardown signal: a guard that HELD stops being
    /// held — by the held→lost transition (arbiter granted elsewhere)
    /// or the local deadline expiring. A guard that never held never
    /// resolves it (a standby's teardown was its non-start).
    ///
    /// Both loss scenarios fold an ALREADY-EXPIRED grant (sent far in
    /// the past) so the deadline is over without burning the real TTL
    /// of wall clock — boot_clock reads CLOCK_BOOTTIME, which tokio's
    /// paused clock cannot advance. The two causes stay distinguished:
    /// the transition records lost_reason(), pure expiry does not.
    #[tokio::test(start_paused = true)]
    async fn wait_lost_resolves_on_loss_and_deadline_but_not_standby() {
        // Cause 1 — the held→lost transition: a grant whose deadline
        // is past, then an explicit HeldByOther answer. wait_lost
        // resolves immediately (the lease is truly over) and the
        // transition is visible in lost_reason.
        let arb = Scripted(std::sync::Mutex::new(VecDeque::from(vec![other()])));
        let g = LeaseGuard::standby("k", "me");
        g.apply(boot_clock() - stale_grace(), held());
        assert!(g.ever_held(), "it held once");
        g.ask_now(&arb).await; // the loss arrives
        assert!(g.lost_reason().is_some(), "the transition is recorded");
        assert!(!g.held());
        g.wait_lost().await; // resolves, not hangs

        // Cause 2 — pure deadline expiry (no further answer, the
        // renewal never came): same resolution, no lost_reason.
        let g2 = LeaseGuard::standby("k", "me");
        g2.apply(boot_clock() - stale_grace(), held());
        assert!(!g2.held(), "the deadline is already expired");
        assert!(g2.ever_held());
        g2.wait_lost().await;
        assert!(g2.lost_reason().is_none(), "no answer arrived to record");

        // A standby (never granted): wait_lost must NOT resolve — its
        // teardown signal is its non-start, not a runtime event.
        let arb3 = Scripted(std::sync::Mutex::new(VecDeque::from(vec![other()])));
        let g3 = LeaseGuard::acquire(&arb3, "k", "me").await;
        assert!(!g3.held());
        tokio::time::timeout(std::time::Duration::from_secs(600), g3.wait_lost())
            .await
            .expect_err("a standby's wait_lost never resolves");
    }

    /// A grant whose local deadline is `left` from now.
    fn held_for(g: &LeaseGuard, left: std::time::Duration) {
        let window = std::time::Duration::from_secs(LEASE_TTL_SECS as u64 - LEASE_MARGIN_SECS);
        g.apply(boot_clock() + left - window, held());
    }

    /// The loss is noticed within a second of the local deadline — not at
    /// the next 30 s poll, which fell past the arbiter's own expiry and
    /// let the next holder's adapter and this one run side by side.
    /// Real time: the deadline is on the boot clock, which tokio's paused
    /// clock does not move.
    #[tokio::test]
    async fn wait_lost_fires_at_the_deadline_not_at_the_next_poll() {
        let g = LeaseGuard::standby("k", "me");
        held_for(&g, std::time::Duration::from_millis(1500));
        assert!(g.held());
        tokio::time::timeout(std::time::Duration::from_secs(4), g.wait_lost())
            .await
            .expect("noticed within ~1 s of the deadline");
    }

    /// Under the supervisor a duty runs while held, stops when the lease
    /// is lost, and starts again — exactly once — when it comes back. The
    /// C1/C2 teardown stopped it and nothing ever started it again: one
    /// blip on the arbiter silenced telegram on every node until restart.
    #[tokio::test]
    async fn supervise_restarts_the_duty_when_the_lease_comes_back() {
        use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
        let g = Arc::new(LeaseGuard::standby("k", "me"));
        held_for(&g, std::time::Duration::from_secs(60));
        let starts = Arc::new(AtomicU32::new(0));
        let (g2, s2) = (g.clone(), starts.clone());
        let task = crate::rt::spawn(async move {
            g2.supervise("duty", || {
                let s = s2.clone();
                async move {
                    s.fetch_add(1, SeqCst);
                    std::future::pending::<anyhow::Result<()>>().await
                }
            })
            .await
        });
        let until = |n: u32| {
            let starts = starts.clone();
            async move {
                while starts.load(SeqCst) < n {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), until(1))
            .await
            .expect("started while held");
        g.apply(boot_clock() - stale_grace(), held()); // lost: deadline over
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert_eq!(starts.load(SeqCst), 1, "not restarted while lost");
        held_for(&g, std::time::Duration::from_secs(60)); // regained
        tokio::time::timeout(std::time::Duration::from_secs(3), until(2))
            .await
            .expect("restarted once the lease came back");
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert_eq!(starts.load(SeqCst), 2, "exactly once");
        task.abort();
    }
}
