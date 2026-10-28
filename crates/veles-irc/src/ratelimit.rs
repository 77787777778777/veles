//! Flood and abuse control — slopdrop's lessons as reusable types.
//!
//! OUTBOUND: a token bucket per connection. slopdrop's Excess Flood
//! deaths came from bursting many IRC lines at the server; the bucket
//! (burst capacity + steady refill) makes that structurally impossible
//! while still letting short replies go out instantly.
//!
//! INBOUND: a sliding-window counter per source identity. IRC channels
//! are untrusted; a hostile user spamming the bot would otherwise
//! spam the LLM (real money). Anything over the window budget is
//! dropped with a NOTICE once, then silently.
//!
//! That last sentence was aspirational until 2026-09-11: `allow`
//! returned a bare bool and the only caller logged `warn!` and
//! returned, so the NOTICE was never sent and a throttled user could
//! not tell a rate limit from a broken bot. `Verdict::Throttle` now
//! carries the once-per-episode flag that makes the promise true.
//!
//! The key is the CALLER's choice and it matters more than it looks:
//! see `client::throttle_key`. Keyed on the nick — as it was — the
//! limiter had a one-word bypass (`/nick`) and leaked one permanent map
//! entry per nick ever seen.

use std::collections::HashMap;
use std::time::Instant;

/// Token bucket for outbound line pacing.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_per_sec: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// `burst` = max lines that may leave back-to-back; `per_sec` =
    /// steady-state line rate after the burst is spent.
    pub fn new(burst: u32, per_sec: f64) -> Self {
        Self {
            capacity: burst.max(1) as f64,
            tokens: burst.max(1) as f64,
            refill_per_sec: per_sec.max(0.1),
            last_refill: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + dt * self.refill_per_sec).min(self.capacity);
        self.last_refill = now;
    }

    /// Take one token if available. Returns the wait time when empty
    /// (0 = send now). Callers must sleep for the returned duration
    /// before sending.
    pub fn take(&mut self) -> std::time::Duration {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            std::time::Duration::ZERO
        } else {
            let deficit = 1.0 - self.tokens;
            std::time::Duration::from_secs_f64(deficit / self.refill_per_sec)
        }
    }

    /// Blocking variant: waits until a token is available, then takes it.
    pub async fn take_blocking(&mut self) {
        loop {
            let wait = self.take();
            if wait.is_zero() {
                return;
            }
            tokio::time::sleep(wait).await;
        }
    }
}

/// Outbound pacing by what the SERVER charges for a line.
///
/// UnrealIRCd (SynIRC, IRC4Fun) keeps a "fake lag" per client: every
/// command it parses adds `penalty × (1 + bytes / penalty_bytes)`, the
/// lag drains at one second per second, and while a client is more than
/// ten seconds in debt the server parses nothing more from it. The
/// unparsed lines wait in the client's receive queue, and a queue past
/// the class's `recvq` (8000 bytes by default) is the "Excess Flood"
/// quit. A line-count bucket cannot keep that: a long line costs three
/// short ones. This keeps the server's own account, so a line goes the
/// moment the server would parse it at once, never sooner. Sending
/// sooner gains nothing, because the line would only wait in that queue.
///
/// The figures are UnrealIRCd 6's for a known user (identified, or with
/// reputation), the class the bot is in on SynIRC. A network that
/// charges more is slowed with `flood_burst`/`flood_per_sec`, which
/// still apply on top.
#[derive(Debug, Clone)]
pub struct ServerLag {
    penalty_ms: f64,
    penalty_bytes: usize,
    /// How far into debt we let the server get: under its ten seconds,
    /// so its whole-second clock and our late start (a fresh
    /// connection's JOINs are not counted) cannot tip it over.
    budget_ms: f64,
    /// The server's lag as we account it, as of `at`.
    lag_ms: f64,
    at: Instant,
}

impl ServerLag {
    pub const UNREAL_PENALTY_MS: u32 = 500;
    pub const UNREAL_PENALTY_BYTES: u32 = 180;
    pub const BUDGET_MS: u32 = 8_000;

    pub fn new(penalty_ms: u32, penalty_bytes: u32, budget_ms: u32) -> Self {
        Self {
            penalty_ms: f64::from(penalty_ms),
            penalty_bytes: penalty_bytes.max(1) as usize,
            budget_ms: f64::from(budget_ms),
            lag_ms: 0.0,
            at: Instant::now(),
        }
    }

    /// UnrealIRCd 6's known-user charge with the default budget.
    pub fn unrealircd() -> Self {
        Self::new(
            Self::UNREAL_PENALTY_MS,
            Self::UNREAL_PENALTY_BYTES,
            Self::BUDGET_MS,
        )
    }

    /// What the server charges for a line of `bytes` (its own integer
    /// division: 179 bytes cost one penalty, 180 cost two).
    pub fn cost_ms(&self, bytes: usize) -> f64 {
        self.penalty_ms * (1 + bytes / self.penalty_bytes) as f64
    }

    fn drain(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.at).as_secs_f64() * 1000.0;
        self.lag_ms = (self.lag_ms - dt).max(0.0);
        self.at = now;
    }

    /// How long a line of `bytes` must wait at `now`; zero when the
    /// server would parse it at once. A line that alone costs more than
    /// the budget still goes when the debt is paid off, or it would
    /// never go.
    pub fn wait_at(&mut self, now: Instant, bytes: usize) -> std::time::Duration {
        self.drain(now);
        let over = self.lag_ms + self.cost_ms(bytes) - self.budget_ms;
        if over <= 0.0 || self.lag_ms == 0.0 {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_secs_f64(over.min(self.lag_ms) / 1000.0)
        }
    }

    /// Count a line that went out at `now`. Also for a line that may not
    /// wait (an approval ask): the server charges it all the same.
    pub fn charge_at(&mut self, now: Instant, bytes: usize) {
        self.drain(now);
        self.lag_ms += self.cost_ms(bytes);
    }

    pub fn charge(&mut self, bytes: usize) {
        self.charge_at(Instant::now(), bytes);
    }

    /// Wait until the server would parse a line of `bytes` at once,
    /// then count it.
    pub async fn take_blocking(&mut self, bytes: usize) {
        loop {
            let wait = self.wait_at(Instant::now(), bytes);
            if wait.is_zero() {
                break;
            }
            tokio::time::sleep(wait).await;
        }
        self.charge(bytes);
    }
}

/// What the limiter decided about one inbound message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Under budget — handle the message.
    Allow,
    /// Over budget, drop it. `notify` is true exactly ONCE per episode,
    /// which is what lets the caller send the single NOTICE this module
    /// has always promised without turning a flood into an echo of
    /// itself. It resets when the source falls out of its window.
    Throttle { notify: bool },
}

/// One source's window, plus whether we have already said so.
#[derive(Debug, Default)]
struct Window {
    hits: Vec<std::time::Instant>,
    noticed: bool,
}

/// The most distinct sources tracked at once.
///
/// The map is keyed by a string the SENDER influences, so without a
/// ceiling it grows for as long as the bot is connected. 4096 is far
/// past any real channel's roster and small enough to be free.
const MAX_TRACKED_SOURCES: usize = 4096;

/// Sliding-window rate limiter keyed by source identity.
#[derive(Debug)]
pub struct InboundLimiter {
    /// max events allowed per window
    max_per_window: u32,
    window: std::time::Duration,
    /// source → recent events + whether the NOTICE went out
    hits: HashMap<String, Window>,
    /// when the aged-out sources were last swept
    last_sweep: std::time::Instant,
}

impl InboundLimiter {
    pub fn new(max_per_window: u32, window_secs: u64) -> Self {
        Self {
            max_per_window,
            window: std::time::Duration::from_secs(window_secs.max(1)),
            hits: HashMap::new(),
            last_sweep: std::time::Instant::now(),
        }
    }

    /// Drop sources whose whole window has aged out.
    ///
    /// The previous shape pruned timestamps INSIDE an entry and never
    /// removed the entry, so one permanent map slot was allocated per
    /// distinct source ever seen — and the caller keyed it on the nick,
    /// which is free to change. A long-lived connection in a busy
    /// channel leaked, and a nick-cycling flood leaked on purpose.
    ///
    /// Amortized: at most once per window, so the common path stays the
    /// single hash lookup it was.
    fn sweep(&mut self, now: std::time::Instant) {
        if now.duration_since(self.last_sweep) < self.window {
            return;
        }
        let w = self.window;
        self.hits.retain(|_, e| {
            e.hits.retain(|t| now.duration_since(*t) < w);
            !e.hits.is_empty()
        });
        self.last_sweep = now;
    }

    /// Record one event from `source`.
    pub fn allow(&mut self, source: &str) -> Verdict {
        let now = std::time::Instant::now();
        self.sweep(now);
        // The ceiling only bites when every tracked source is still
        // inside its window, i.e. a genuine flood from many identities.
        // Refusing the NEW source is the safe direction: the people
        // already talking keep their budgets, and the flood is what
        // gets dropped. No NOTICE — answering an unbounded set of
        // identities is the amplification this guards against.
        if !self.hits.contains_key(source) && self.hits.len() >= MAX_TRACKED_SOURCES {
            return Verdict::Throttle { notify: false };
        }
        let window = self.window;
        let max = self.max_per_window;
        let entry = self.hits.entry(source.to_string()).or_default();
        entry.hits.retain(|t| now.duration_since(*t) < window);
        if entry.hits.is_empty() {
            // Out of the window entirely: this is a fresh episode, so
            // the next throttle is allowed to speak again.
            entry.noticed = false;
        }
        if entry.hits.len() as u32 >= max {
            let notify = !entry.noticed;
            entry.noticed = true;
            return Verdict::Throttle { notify };
        }
        entry.hits.push(now);
        Verdict::Allow
    }

    /// How many sources are currently tracked (for status tooling and
    /// for the test that pins the ceiling).
    pub fn tracked(&self) -> usize {
        self.hits.len()
    }

    /// Sources currently over budget (for status tooling).
    pub fn throttled_sources(&mut self) -> Vec<String> {
        let now = std::time::Instant::now();
        let w = self.window;
        let max = self.max_per_window;
        let mut out = Vec::new();
        self.hits.retain(|src, e| {
            e.hits.retain(|t| now.duration_since(*t) < w);
            if e.hits.len() as u32 >= max {
                out.push(src.clone());
            }
            !e.hits.is_empty()
        });
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_allows_burst_then_throttles() {
        let mut b = TokenBucket::new(3, 1.0);
        assert_eq!(b.take(), std::time::Duration::ZERO);
        assert_eq!(b.take(), std::time::Duration::ZERO);
        assert_eq!(b.take(), std::time::Duration::ZERO);
        // burst spent → next take needs ~1s
        let wait = b.take();
        assert!(
            wait > std::time::Duration::from_millis(500),
            "wait: {wait:?}"
        );
    }

    #[test]
    fn bucket_refills_over_time() {
        let mut b = TokenBucket::new(2, 100.0); // fast refill for the test
        b.take();
        b.take();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(b.take(), std::time::Duration::ZERO);
    }

    #[test]
    fn bucket_min_capacity_one() {
        let mut b = TokenBucket::new(0, 1.0);
        assert_eq!(b.take(), std::time::Duration::ZERO); // capacity clamped to 1
    }

    /// UnrealIRCd's side of the exchange. Each read lands in the
    /// client's receive queue and is checked against `recvq` BEFORE
    /// anything is parsed (over 8000 bytes is the Excess Flood quit);
    /// then lines are parsed while the client's lag is at most ten
    /// seconds ahead, each adding `500 × (1 + bytes / 180)` ms. Fed the
    /// times lines arrive (lines arriving together are one read), it
    /// answers the largest queue a read ever made, and the most bytes
    /// fake lag ever left waiting after a parse.
    fn unreal_queue(arrivals: &[(f64, usize)]) -> (usize, usize) {
        let (mut lag, mut t) = (0.0f64, 0.0f64);
        let (mut peak, mut held) = (0usize, 0usize);
        let mut queue: std::collections::VecDeque<usize> = Default::default();
        let mut next = 0;
        while next < arrivals.len() || !queue.is_empty() {
            // The next moment anything changes: a read, or the lag
            // draining back to the threshold.
            let mut until = arrivals.get(next).map(|a| a.0);
            if !queue.is_empty() {
                let ready = t + (lag - 10_000.0).max(0.0) / 1000.0;
                until = Some(until.map_or(ready, |u: f64| u.min(ready)));
            }
            let until = until.unwrap_or(t);
            lag = (lag - (until - t) * 1000.0).max(0.0);
            t = until;
            while next < arrivals.len() && arrivals[next].0 <= t + 1e-9 {
                queue.push_back(arrivals[next].1);
                next += 1;
            }
            peak = peak.max(queue.iter().sum());
            while lag <= 10_000.0 {
                let Some(bytes) = queue.pop_front() else {
                    break;
                };
                lag += 500.0 * (1 + bytes / 180) as f64;
            }
            held = held.max(queue.iter().sum());
        }
        (peak, held)
    }

    /// When `pacer` would let each line out, on a clock of its own.
    fn paced(pacer: &mut ServerLag, lines: &[usize]) -> Vec<(f64, usize)> {
        let start = pacer.at;
        let mut now = start;
        lines
            .iter()
            .map(|&bytes| {
                now += pacer.wait_at(now, bytes);
                pacer.charge_at(now, bytes);
                ((now - start).as_secs_f64(), bytes)
            })
            .collect()
    }

    /// The reference model is right about the failure it exists for:
    /// 42 lines and 10 KB in one write overflows the receive queue.
    #[test]
    fn the_server_model_floods_on_the_reply_that_flooded() {
        let burst: Vec<(f64, usize)> = (0..42).map(|_| (0.0, 240)).collect();
        let (peak, _) = unreal_queue(&burst);
        assert!(peak > 8000, "{peak}");
    }

    /// Paced by its charge, nothing ever waits in the server's queue,
    /// however long the reply and however long its lines.
    ///
    /// Mutation checks: charge a flat penalty (drop `bytes /
    /// penalty_bytes`) or lift the budget past ten seconds, and the
    /// backlog grows: red.
    #[test]
    fn a_reply_paced_by_the_servers_charge_never_queues_there() {
        for lines in [vec![476usize; 120], vec![105; 200], vec![240; 42]] {
            let at = paced(&mut ServerLag::unrealircd(), &lines);
            let (peak, held) = unreal_queue(&at);
            assert_eq!(
                held,
                0,
                "{} lines of {} bytes left lines waiting in the server's queue",
                lines.len(),
                lines[0]
            );
            assert!(peak < 8000, "one read queued {peak} bytes");
        }
    }

    /// The budget is spent at once, so a short answer (a 15-line
    /// slopdrop proc, the 2026-09-12 latency complaint) goes in one go.
    #[test]
    fn a_short_answer_goes_at_once() {
        let at = paced(&mut ServerLag::unrealircd(), &[105; 15]);
        assert!(at.iter().all(|&(t, _)| t == 0.0), "{at:?}");
    }

    /// A line that costs more than the whole budget still goes, once the
    /// debt is paid off, instead of waiting forever.
    #[test]
    fn a_line_dearer_than_the_budget_still_goes() {
        let mut lag = ServerLag::new(500, 180, 1000);
        let at = paced(&mut lag, &[476, 476]);
        assert_eq!(at[0].0, 0.0);
        assert!(
            (at[1].0 - 1.5).abs() < 1e-6,
            "the second waits out the first's 1.5 s: {at:?}"
        );
    }

    fn allowed(l: &mut InboundLimiter, src: &str) -> bool {
        matches!(l.allow(src), Verdict::Allow)
    }

    #[test]
    fn inbound_window_enforced() {
        let mut l = InboundLimiter::new(3, 60);
        assert!(allowed(&mut l, "alice"));
        assert!(allowed(&mut l, "alice"));
        assert!(allowed(&mut l, "alice"));
        assert!(!allowed(&mut l, "alice")); // 4th in window → drop
        assert!(allowed(&mut l, "bob")); // different source unaffected
    }

    #[test]
    fn inbound_window_expires() {
        let mut l = InboundLimiter::new(2, 1); // 1s window
        assert!(allowed(&mut l, "x"));
        assert!(allowed(&mut l, "x"));
        assert!(!allowed(&mut l, "x"));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(allowed(&mut l, "x")); // window expired
    }

    #[test]
    fn throttled_sources_reported() {
        let mut l = InboundLimiter::new(1, 60);
        l.allow("spam");
        l.allow("spam");
        assert_eq!(l.throttled_sources(), vec!["spam".to_string()]);
    }

    /// The NOTICE the module doc has always promised, which no caller
    /// could send because `allow` returned a bare bool.
    ///
    /// Mutation check: make `notify` unconditionally true and the
    /// "only once" assertion goes red; make it unconditionally false
    /// and the first one does.
    #[test]
    fn a_throttled_source_is_told_exactly_once() {
        let mut l = InboundLimiter::new(1, 60);
        assert_eq!(l.allow("eve"), Verdict::Allow);
        assert_eq!(
            l.allow("eve"),
            Verdict::Throttle { notify: true },
            "the first refusal is the one worth saying out loud"
        );
        for _ in 0..10 {
            assert_eq!(
                l.allow("eve"),
                Verdict::Throttle { notify: false },
                "a flood must not become an echo of itself"
            );
        }
    }

    /// A fresh episode may speak again — otherwise someone throttled
    /// once at 09:00 is silently dropped forever after.
    #[test]
    fn the_notice_rearms_after_the_window_passes() {
        let mut l = InboundLimiter::new(1, 1);
        l.allow("eve");
        assert_eq!(l.allow("eve"), Verdict::Throttle { notify: true });
        assert_eq!(l.allow("eve"), Verdict::Throttle { notify: false });
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(l.allow("eve"), Verdict::Allow, "the window aged out");
        assert_eq!(
            l.allow("eve"),
            Verdict::Throttle { notify: true },
            "a NEW episode is allowed to say so"
        );
    }

    /// The map is keyed by a string the sender influences, so it needs
    /// a ceiling AND it needs to forget people who stopped talking.
    ///
    /// Mutation check: delete the `sweep` body and the first assertion
    /// goes red; delete the MAX_TRACKED_SOURCES guard and the second.
    #[test]
    fn the_source_map_is_swept_and_capped() {
        // Aged-out sources are forgotten, not accumulated forever.
        let mut l = InboundLimiter::new(5, 1);
        for i in 0..50 {
            l.allow(&format!("nick{i}"));
        }
        assert_eq!(l.tracked(), 50);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        l.allow("someone-else"); // any call triggers the sweep
        assert_eq!(
            l.tracked(),
            1,
            "50 nicks that stopped talking must not stay in the map"
        );

        // And a flood of distinct identities inside one window is capped.
        let mut l = InboundLimiter::new(5, 3600);
        for i in 0..(MAX_TRACKED_SOURCES + 500) {
            l.allow(&format!("flood{i}"));
        }
        assert_eq!(
            l.tracked(),
            MAX_TRACKED_SOURCES,
            "the map must not grow without bound"
        );
    }
}
