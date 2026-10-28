//! Triggers: reactions to channel events that run without a turn.
//!
//! The slopdrop cutover (8410bc4) left the channel without its
//! eggdrop-style reflexes: link titles, timtom, the join greetings.
//! slopdrop's engine is intact and already expresses the per-channel
//! split (`triggers disable_for irc4fun #coven …` in its own state);
//! what is missing is a feed. veles holds the IRC connection, so veles
//! forwards.
//!
//! Two halves share this one control surface:
//!
//!  - **Slopdrop** rows forward the event over the bridge and relay
//!    the `{channel, message}` pairs back. The bridge's own disable
//!    rules and channel patterns apply; our scope fields are the
//!    coarse gate in front (is this channel worth an HTTP call).
//!  - **Vision** rows match an image URL and describe it through the
//!    `[vision]` route, inside cooldown/cap brakes and the shared
//!    media budget.
//!
//! Nothing here sends on its own. Actions return `Outcome`s; the IRC
//! adapter is the only thing with a connection and it decides what a
//! reply looks like. That split is why the engine is testable from the
//! store layer without a network.
//!
//! The forward MASK (which events a channel actually wants, so we do
//! not spend a bridge call on a JOIN nothing listens to) is DERIVED
//! from slopdrop's own bindings via `GET /api/triggers`, never typed
//! twice — with a per-channel override for the operator who knows
//! better, stored in the same table as a `mask`-kind row.

use crate::config::VelesConfig;
use crate::store::Store;
use std::collections::HashMap;
use std::sync::Arc;
use veles_types::plugin::method::{TRIGGERS_FORWARD, TRIGGERS_MASKS};

/// One trigger row (store::Trigger is the persisted twin).
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    // ── scope ──
    pub platform: Option<String>,
    pub network: Option<String>,
    pub channel: Option<String>,
    pub event: String,
    // ── match ──
    pub match_kind: MatchKind,
    pub pattern: Option<String>,
    // ── action ──
    pub action: ActionKind,
    pub arg: Option<String>,
    // ── brakes ──
    pub cooldown_secs: i64,
    pub per_hour_cap: i64,
    pub min_role: Option<String>,
    // ── bookkeeping ──
    pub fire_count: i64,
    pub last_fired_at: Option<String>,
    pub last_status: Option<String>,
}

impl Trigger {
    /// Does this row apply to the (network, channel) it is being asked
    /// about? Glob, not equality: a row with no network or channel
    /// field means "any".
    pub fn in_scope(&self, platform: &str, network: &str, channel: &str) -> bool {
        if let Some(p) = &self.platform {
            if !glob_match(p, platform) {
                return false;
            }
        }
        if let Some(n) = &self.network {
            if !glob_match_ci(n, network) {
                return false;
            }
        }
        if let Some(c) = &self.channel {
            if !glob_match_ci(c, channel) {
                return false;
            }
        }
        true
    }

    /// Would the text fire this trigger? (Cooldown and caps are
    /// separate: they answer "should we", this answers "does it
    /// match".)
    pub fn matches_text(&self, text: &str) -> bool {
        match self.match_kind {
            MatchKind::Always => true,
            MatchKind::Contains => self
                .pattern
                .as_deref()
                .is_some_and(|p| text.to_lowercase().contains(&p.to_lowercase())),
            MatchKind::Regex => {
                let Some(p) = self.pattern.as_deref() else {
                    return false;
                };
                regex_lite(p, text)
            }
            MatchKind::Url | MatchKind::ImageUrl => {
                for url in extract_urls(text) {
                    if self.match_kind == MatchKind::Url {
                        return true;
                    }
                    // ImageUrl: extension-only check (media::is_image
                    // works on a URL string).
                    let img = url
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .split('?')
                        .next()
                        .unwrap_or_default();
                    if crate::media::is_image(img) {
                        return true;
                    }
                }
                false
            }
            MatchKind::Bluesky => !crate::bskylink::links_in(text).is_empty(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Always,
    Contains,
    Regex,
    Url,
    ImageUrl,
    /// A Bluesky post or profile link (bsky.app, impro.social):
    /// [`crate::bskylink::links_in`].
    Bluesky,
}

impl MatchKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "always" | "any" | "*" => MatchKind::Always,
            "contains" => MatchKind::Contains,
            "regex" => MatchKind::Regex,
            "url" => MatchKind::Url,
            "imageurl" | "image" | "image-url" => MatchKind::ImageUrl,
            "bluesky" | "bsky" | "bluesky-url" => MatchKind::Bluesky,
            _ => return None,
        })
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            MatchKind::Always => "always",
            MatchKind::Contains => "contains",
            MatchKind::Regex => "regex",
            MatchKind::Url => "url",
            MatchKind::ImageUrl => "imageurl",
            MatchKind::Bluesky => "bluesky",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Slopdrop,
    Vision,
    /// Preview Bluesky links as the bot ([`crate::bskylink`]).
    Bluesky,
    Prompt,
    Command,
}

impl ActionKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "slopdrop" => ActionKind::Slopdrop,
            "vision" => ActionKind::Vision,
            "bluesky" | "bsky" => ActionKind::Bluesky,
            "prompt" => ActionKind::Prompt,
            "command" => ActionKind::Command,
            _ => return None,
        })
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionKind::Slopdrop => "slopdrop",
            ActionKind::Vision => "vision",
            ActionKind::Bluesky => "bluesky",
            ActionKind::Prompt => "prompt",
            ActionKind::Command => "command",
        }
    }
}

/// What to do with a matched trigger — the adapter's contract.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Relay these channel messages back (slopdrop's handlers).
    Slopdrop(Vec<(String, String)>),
    /// Describe this image with the question.
    Vision { url: String, question: String },
    /// Preview these Bluesky links as the bot. The adapter runs these
    /// BEFORE any Slopdrop forward of the same event and names the links
    /// it answered to the forward, so slopdrop's resolver does not answer
    /// them a second time; a link the bot could not read stays slopdrop's.
    Bluesky(Vec<crate::bskylink::Link>),
    /// Run a turn with this prompt in the channel's session (future
    /// use; kept in the enum so the store round-trips).
    Prompt(String),
    /// Run a command line (future use).
    Command(String),
}

/// One observed event, as the hook hands it over.
#[derive(Debug, Clone)]
pub struct Event<'a> {
    pub platform: &'a str,
    pub network: &'a str,
    pub channel: &'a str,
    pub event: &'a str,
    pub nick: &'a str,
    pub mask: &'a str,
    pub text: &'a str,
    /// The sender's role, when the adapter resolved it. `None` means
    /// "unknown, assume the worst" — same convention the context
    /// block's min-role filter uses.
    pub role: Option<crate::acl::Role>,
}

/// Cooldown state per (trigger, channel), in memory: a trigger that
/// fired 30s ago does not need a row read to know it. The cap window
/// is the trailing hour, kept the same way.
#[derive(Default)]
pub struct Brakes {
    last_fire: HashMap<(i64, String), i64>,
    hour_marks: HashMap<(i64, String), Vec<i64>>,
    /// The BRIDGE budget, keyed `network\u{1}channel` — a different
    /// question from the two above. Those bound one ROW; this bounds
    /// what one CHANNEL may spend on slopdrop in a minute, whatever mix
    /// of rows produced it, because the interpreter on the far side is
    /// single-threaded and a busy room must not starve `tcl` from the
    /// same room. `[triggers] max_per_minute` is its ceiling.
    bridge_marks: HashMap<String, Vec<i64>>,
}

impl Brakes {
    /// RECORD a fire. Split from `peek` on purpose: `evaluate` runs
    /// before the forward mask and before the action, so a brake spent
    /// there is spent on a fire that may never happen — a masked-out
    /// event, or a Vision row that finds no image, used to burn the
    /// cooldown and refuse the next REAL event for it. Peek to decide,
    /// commit at the moment something is actually spent.
    pub fn commit(&mut self, t: &Trigger, channel: &str, now: i64) {
        let key = (t.id, channel.to_string());
        self.last_fire.insert(key.clone(), now);
        self.hour_marks.entry(key).or_default().push(now);
    }

    /// The BRIDGE budget: may this channel spend one more forward on
    /// slopdrop right now? Records the spend when it may — this is the
    /// commitment point, called where the forward is about to happen
    /// and not a moment earlier.
    ///
    /// `max_per_minute == 0` disables the brake, the same spelling
    /// `timers_poll_secs = 0` uses for its tick.
    pub fn bridge_claim(
        &mut self,
        network: &str,
        channel: &str,
        now: i64,
        max_per_minute: u64,
    ) -> Result<(), String> {
        if max_per_minute == 0 {
            return Ok(());
        }
        let key = mask_key(network, channel);
        let marks = self.bridge_marks.entry(key).or_default();
        let cutoff = now - 60;
        marks.retain(|&m| m > cutoff);
        if marks.len() as u64 >= max_per_minute {
            return Err(format!(
                "bridge: {} forwards from this channel in the last minute (max {})",
                marks.len(),
                max_per_minute
            ));
        }
        marks.push(now);
        Ok(())
    }

    /// The refusal reasons a candidate would meet NOW, without
    /// claiming anything. For `/triggers test`.
    pub fn peek(&self, t: &Trigger, channel: &str, now: i64) -> Vec<String> {
        let mut out = Vec::new();
        let key = (t.id, channel.to_string());
        if t.cooldown_secs > 0 {
            if let Some(&last) = self.last_fire.get(&key) {
                let elapsed = now - last;
                if elapsed < t.cooldown_secs {
                    out.push(format!(
                        "cooldown: {}s of {}s left",
                        t.cooldown_secs - elapsed,
                        t.cooldown_secs
                    ));
                }
            }
        }
        if t.per_hour_cap > 0 {
            let cutoff = now - 3600;
            let n = self
                .hour_marks
                .get(&key)
                .map(|m| m.iter().filter(|&&x| x > cutoff).count())
                .unwrap_or(0);
            if n as i64 >= t.per_hour_cap {
                out.push(format!(
                    "cap: {} fires in the last hour (max {})",
                    n, t.per_hour_cap
                ));
            }
        }
        out
    }
}

/// Evaluate one event against the enabled triggers. Returns what to
/// do, in row order. Network calls are the CALLER's side of the
/// contract: a `Slopdrop` outcome says "ask the bridge", it does not
/// ask it here (the engine stays offline-testable).
pub fn evaluate(
    store: &Store,
    brakes: &Brakes,
    ev: &Event<'_>,
    now: i64,
) -> Vec<(Trigger, Result<Outcome, String>)> {
    // A row whose enumerated fields do not parse is SKIPPED, not read
    // through `into_trigger`'s `unwrap_or` defaults — those widen
    // (`Always` matches every line, `Slopdrop` spends a bridge call).
    // The hot path stays quiet about it on purpose: one refusal row per
    // channel line would be its own flood. `listing` and `show` name
    // the bad field instead, which is where an operator asking "why
    // didn't it react" actually looks — and the four write surfaces
    // make a new invalid row impossible.
    let rows = store
        .trigger_list(true)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| validate_row(r).is_ok())
        .map(|r| r.into_trigger())
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    for t in rows {
        if !t.in_scope(ev.platform, ev.network, ev.channel) {
            continue;
        }
        if t.event != ev.event {
            continue;
        }
        // A min_role the sender is below refuses the fire, and the
        // refusal is a LOGGED outcome, not a silence.
        if let Some(min) = &t.min_role {
            let floor = crate::acl::Role::parse(min).unwrap_or(crate::acl::Role::Guest);
            match ev.role {
                Some(r) if r >= floor => {}
                Some(r) => {
                    out.push((
                        t.clone(),
                        Err(format!("min-role: {} < {}", r.as_str(), min)),
                    ));
                    continue;
                }
                None => {
                    out.push((
                        t.clone(),
                        Err(format!("min-role: sender role unknown, need {min}")),
                    ));
                    continue;
                }
            }
        }
        // A notification's title and text are its words, as a line is
        // a channel's.
        if matches!(ev.event, "TEXT" | "NOTIFICATION") && !t.matches_text(ev.text) {
            continue;
        }
        // Cooldown and cap are INSPECTED before any network call, and a
        // refusal is recorded in the fire log with its reason. They are
        // not spent here: the mask and the action both run after this,
        // so the caller commits at the point it actually spends
        // something (`Brakes::commit`).
        let braked = brakes.peek(&t, ev.channel, now);
        if !braked.is_empty() {
            out.push((t.clone(), Err(braked.join("; "))));
            continue;
        }
        let outcome = match t.action {
            ActionKind::Slopdrop => Ok(Outcome::Slopdrop(Vec::new())),
            ActionKind::Vision => {
                let url = extract_urls(ev.text)
                    .into_iter()
                    .find(|u| {
                        let stem = u
                            .rsplit('/')
                            .next()
                            .unwrap_or_default()
                            .split('?')
                            .next()
                            .unwrap_or_default();
                        crate::media::is_image(stem)
                    })
                    .unwrap_or_default();
                if url.is_empty() {
                    Err("no image url in text".into())
                } else {
                    Ok(Outcome::Vision {
                        url,
                        question: t.arg.clone().unwrap_or_else(|| {
                            "Describe this image in one or two sentences.".into()
                        }),
                    })
                }
            }
            ActionKind::Bluesky => {
                let links = crate::bskylink::links_in(ev.text);
                if links.is_empty() {
                    Err("no bluesky link in text".into())
                } else {
                    Ok(Outcome::Bluesky(links))
                }
            }
            ActionKind::Prompt => Ok(Outcome::Prompt(t.arg.clone().unwrap_or_default())),
            ActionKind::Command => Ok(Outcome::Command(t.arg.clone().unwrap_or_default())),
        };
        out.push((t.clone(), outcome));
    }
    out
}

// ── the derived forward mask ────────────────────────────────────────

/// The mask for one channel: which event types to forward.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelMask {
    pub events: Vec<String>,
    /// None = derived (auto); Some = the operator's override.
    pub override_events: Option<Vec<String>>,
}

impl ChannelMask {
    pub fn allows(&self, event: &str) -> bool {
        let list = self.override_events.as_ref().unwrap_or(&self.events);
        list.iter().any(|e| e.eq_ignore_ascii_case(event))
    }
    pub fn is_derived(&self) -> bool {
        self.override_events.is_none()
    }
}

// ── the bridge, through the service registry ─────────────────────────
//
// What a Slopdrop row DOES — forward the event to the bridge and derive
// the per-channel mask from the bridge's own bindings — is the slopdrop
// MODULE's, not core's. The engine, the rows, the brakes and the mask
// cache stay here (every surface reaches them); the two calls that need
// the bridge are the services `triggers.forward` and `triggers.masks`
// (`crate::services`), which the slopdrop module provides while
// `[slopdrop]` is enabled. Until 2026-10-10 they went through a
// `Forwarder` slot only an in-process module could fill. With no
// provider a Slopdrop row refuses with a reason (never silently), and a
// refresh says what is missing.

/// Does anything here provide `triggers.forward`? Without it a Slopdrop
/// row can only fail, so the adapter refuses it with [`NO_FORWARDER`].
pub fn forwarder_configured() -> bool {
    crate::services::available(TRIGGERS_FORWARD)
}

/// What a refresh or a forward says with no provider.
pub const NO_FORWARDER: &str = "no plugin provides `triggers.forward` on this node (the \
                                slopdrop module, with [slopdrop] enabled)";

/// The event as `triggers.forward` carries it.
pub fn event_wire(ev: &Event<'_>) -> veles_types::plugin::TriggerEvent {
    veles_types::plugin::TriggerEvent {
        platform: ev.platform.to_string(),
        network: ev.network.to_string(),
        channel: ev.channel.to_string(),
        event: ev.event.to_string(),
        nick: ev.nick.to_string(),
        mask: ev.mask.to_string(),
        text: ev.text.to_string(),
        role: ev.role.map(|r| r.as_str().to_string()),
    }
}

/// Forward one event to the bridge (a Slopdrop row's action), naming
/// the URLs another row already answered (the bot's Bluesky previews),
/// so the bridge's link resolver leaves them alone. The `(channel,
/// message)` pairs to relay back.
pub async fn forward(ev: &Event<'_>, answered: &[String]) -> Result<Vec<(String, String)>, String> {
    if !forwarder_configured() {
        return Err(NO_FORWARDER.to_string());
    }
    let params = veles_types::plugin::TriggersForwardParams {
        event: event_wire(ev),
        answered_urls: answered.to_vec(),
    };
    let v = crate::services::call(
        TRIGGERS_FORWARD,
        serde_json::to_value(params).map_err(|e| e.to_string())?,
    )
    .await?;
    let out: veles_types::plugin::TriggersForwarded = serde_json::from_value(v)
        .map_err(|e| format!("{TRIGGERS_FORWARD} answered something else: {e}"))?;
    Ok(out
        .replies
        .into_iter()
        .map(|r| (r.channel, r.message))
        .collect())
}

/// The events each `(network, channel)` pair still has a LIVE binding
/// for on the bridge (`triggers.masks`); a pair the bridge knows no
/// network for is left out.
async fn derive_masks(
    pairs: &[(String, String)],
) -> Result<HashMap<(String, String), Vec<String>>, String> {
    if !crate::services::available(TRIGGERS_MASKS) {
        return Err(NO_FORWARDER.to_string());
    }
    let params = veles_types::plugin::TriggersMasksParams {
        pairs: pairs
            .iter()
            .map(|(n, c)| veles_types::plugin::NetChannel {
                network: n.clone(),
                channel: c.clone(),
            })
            .collect(),
    };
    let v = crate::services::call(
        TRIGGERS_MASKS,
        serde_json::to_value(params).map_err(|e| e.to_string())?,
    )
    .await?;
    let out: veles_types::plugin::TriggersMasks = serde_json::from_value(v)
        .map_err(|e| format!("{TRIGGERS_MASKS} answered something else: {e}"))?;
    Ok(out
        .masks
        .into_iter()
        .map(|m| ((m.network, m.channel), m.events))
        .collect())
}

/// Fetch slopdrop's live bindings and compute the per-channel derived
/// mask for every (veles network, channel) the triggers table knows.
/// Returns masks keyed by `"network\u{1}channel"` (the NUL is a
/// separator a channel name cannot contain).
///
/// The store borrow is the CALLER's: `refresh_masks` itself only
/// awaits HTTP, and a `MutexGuard` held across that await makes the
/// whole gateway future non-Send. Take the list out under the lock,
/// drop the guard, then talk to the network.
pub async fn refresh_masks(
    _cfg: &VelesConfig,
    store: &Arc<std::sync::Mutex<Store>>,
) -> Result<HashMap<String, ChannelMask>, String> {
    // Everything the store has to say, said under ONE lock, before any
    // await: a guard held across the HTTP call makes the caller's
    // whole future non-Send and the gateway compiles nowhere.
    let rows = {
        let s = match store.lock() {
            Ok(s) => s,
            Err(_) => return Err("store unavailable".into()),
        };
        s.trigger_list(false).unwrap_or_default()
    };
    // Overrides again, after the rows: one lock, one pass, no await
    // between taking the data and using it.
    let mut override_map: HashMap<(String, String), String> = HashMap::new();
    {
        let s = match store.lock() {
            Ok(s) => s,
            Err(_) => return Err("store unavailable".into()),
        };
        for t in &rows {
            if let (Some(net), Some(chan)) = (&t.network, &t.channel) {
                if let Some(o) = s.trigger_mask_override(net, chan) {
                    override_map.insert((net.clone(), chan.clone()), o);
                }
            }
        }
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    for t in &rows {
        if let (Some(net), Some(chan)) = (t.network.clone(), t.channel.clone()) {
            if !pairs.iter().any(|p| p.0 == net && p.1 == chan) {
                pairs.push((net, chan));
            }
        }
    }
    let derived = derive_masks(&pairs).await?;
    let mut out = HashMap::new();
    for (net, chan) in pairs {
        let key = format!("{net}\u{1}{chan}");
        // A pair the bridge knows no network for has no mask: the hot
        // path answers false for it, the safe direction.
        let Some(events) = derived.get(&(net.clone(), chan.clone())) else {
            continue;
        };
        out.insert(
            key,
            ChannelMask {
                events: events.clone(),
                override_events: override_map.get(&(net, chan)).map(|evs| {
                    evs.split(',')
                        .map(|s| s.trim().to_ascii_uppercase())
                        .collect()
                }),
            },
        );
    }
    Ok(out)
}

// ── the mask cache: process-global, and it lives HERE ───────────────
//
// WHY veles-core and not the IRC adapter: two surfaces share one cache
// and they sit on opposite sides of the crate boundary. The IRC hot path
// READS it (`mask_allows`, per event, to decide whether a forward is
// worth a bridge call) and `/triggers refresh` in commands.rs WRITES it
// and reports what it derived. veles-core cannot call into veles-irc, so
// a cache kept in the adapter was unreachable from the refresh command.
//
// That is not hypothetical — it is the bug this closes. The first version
// kept the cache in triggers_hook.rs behind a private `runtime()`
// singleton, with an `ensure_masks` that NOTHING ever called: zero call
// sites in the whole tree. So the cache stayed empty forever,
// `mask_allows` answered false for every channel, and both Slopdrop rows
// were refused with "mask: event not wanted" on their first live fire —
// while `/triggers refresh` cheerfully printed the correct masks, because
// it derived into a local and reported that without ever installing it.
// A command that reports a refresh it did not perform is worse than no
// command: it tells the operator the wiring is fine when it is not.
// Caught 2026-09-15 by tracing the wiring rather than trusting it.
//
// The fix is one cache, in the crate both sides can reach, with a single
// writer (`install_masks`) that `force_refresh_masks` calls and the hot
// path's TTL refresher calls. `mask_allows` is the only reader.

#[derive(Default)]
struct MaskCache {
    masks: HashMap<String, ChannelMask>,
    refreshed_at: Option<std::time::Instant>,
}

fn mask_cache() -> &'static std::sync::RwLock<MaskCache> {
    static C: std::sync::OnceLock<std::sync::RwLock<MaskCache>> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// The mask key's separator: a control byte a channel name cannot
/// contain, so `"network\u{1}channel"` round-trips unambiguously. Public
/// so a caller building a key for `install_masks` cannot drift from the
/// reader's.
pub fn mask_key(network: &str, channel: &str) -> String {
    format!("{network}\u{1}{channel}")
}

/// Does this channel want this event forwarded? The hot path's verdict
/// and the ONLY reader of the cache. An unknown channel — no trigger row
/// names it, or the cache was never refreshed — answers FALSE. That is
/// the safe direction: a false negative skips one forward and logs it,
/// while a false positive spends a bridge call on a channel that asked
/// for silence.
pub fn mask_allows(network: &str, channel: &str, event: &str) -> bool {
    match mask_cache().read() {
        Ok(c) => match c.masks.get(&mask_key(network, channel)) {
            Some(m) => m.allows(event),
            None => false,
        },
        Err(_) => false,
    }
}

/// Replace the whole cache with freshly derived masks. The one writer
/// for a full refresh.
pub fn install_masks(masks: HashMap<String, ChannelMask>) {
    if let Ok(mut c) = mask_cache().write() {
        c.masks = masks;
        c.refreshed_at = Some(std::time::Instant::now());
    }
}

/// Apply ONE channel's mask override to the live cache, immediately.
///
/// `/triggers mask <net> <chan> EVENTS` writes the override to the
/// store; without this the cache would not see it until the next TTL
/// refresh (up to ten minutes), so the operator pins a mask and watches
/// nothing change. An override needs no slopdrop: `ChannelMask::allows`
/// answers from `override_events` alone, so `Some(events)` installs a
/// mask that decides from the pin. `None` (the `auto` case, back to
/// derived) drops the cached entry — the next refresh re-derives it from
/// slopdrop, and until then `mask_allows` answers false, the safe
/// direction (skip a forward, never spend one on a channel mid-revert).
///
/// This does not touch `refreshed_at`: a single-channel pin is not a
/// full re-derivation, and bumping the TTL would let one override delay
/// the next real slopdrop refresh by ten minutes.
pub fn set_override_in_cache(network: &str, channel: &str, events: Option<Vec<String>>) {
    let key = mask_key(network, channel);
    if let Ok(mut c) = mask_cache().write() {
        match events {
            Some(evs) => {
                c.masks.insert(
                    key,
                    ChannelMask {
                        events: Vec::new(),
                        override_events: Some(evs),
                    },
                );
            }
            None => {
                c.masks.remove(&key);
            }
        }
    }
}

/// How long ago the cache was refreshed, if ever.
///
/// Two spellings on purpose. Seconds is what a human reads in
/// `/triggers refresh`; the `Duration` is what a TEST can hold to
/// account. The seconds-only version made
/// `install_sets_the_refresh_clock_but_a_single_override_does_not`
/// unfalsifiable: it asserted `age < 5` after the override, which is
/// equally true when the override DID reset the clock — the mutation
/// the test names could not turn it red.
pub fn masks_age() -> Option<std::time::Duration> {
    match mask_cache().read() {
        Ok(c) => c.refreshed_at.map(|at| at.elapsed()),
        Err(_) => None,
    }
}

/// The human spelling, reported by `refresh_failure_report`.
pub fn masks_age_secs() -> Option<u64> {
    masks_age().map(|d| d.as_secs())
}

/// What `/triggers refresh` says when the derive FAILS. This is the
/// moment "is the mask stale" stops being idle curiosity: the refresh
/// is best-effort, so the masks already installed keep deciding every
/// forward, and their age is the operator's only clue whether that is
/// fine or a problem. An empty cache is the other answer, and it is
/// louder — nothing forwards at all until a refresh lands.
///
/// (`masks_age_secs` documented itself as something the refresh command
/// reported. The refresh arm never called it; it had no production
/// caller at all. This is that caller, saying the thing worth saying.)
pub fn refresh_failure_report(err: &str, age_secs: Option<u64>) -> String {
    match age_secs {
        Some(age) => format!(
            "refresh failed: {err}\n\
             the masks in force are {age}s old and keep deciding every forward"
        ),
        None => format!(
            "refresh failed: {err}\n\
             NO masks are installed — nothing forwards until a refresh succeeds"
        ),
    }
}

/// Derive from slopdrop, INSTALL, and return. This is the write path
/// `/triggers refresh` must use: deriving into a local and reporting it
/// while leaving the cache stale is the exact lie that hid this bug.
pub async fn force_refresh_masks(
    cfg: &VelesConfig,
    store: &Arc<std::sync::Mutex<Store>>,
) -> Result<HashMap<String, ChannelMask>, String> {
    let masks = refresh_masks(cfg, store).await?;
    install_masks(masks.clone());
    Ok(masks)
}

/// TTL-gated refresh for the hot path. Re-derives at most once per `ttl`
/// and is best-effort: a slopdrop that is down must not stall the IRC
/// loop, so on error the previous masks (or an empty cache) keep
/// deciding. No cache lock is held across the HTTP await — the read
/// guard for the staleness check drops before it, and the write guard
/// is taken only for the install swap.
pub async fn ensure_masks_fresh(
    cfg: &VelesConfig,
    store: &Arc<std::sync::Mutex<Store>>,
    ttl: std::time::Duration,
) {
    let stale = match mask_cache().read() {
        Ok(c) => match c.refreshed_at {
            Some(at) => at.elapsed() > ttl,
            None => true,
        },
        Err(_) => return,
    };
    if !stale {
        return;
    }
    if let Err(e) = force_refresh_masks(cfg, store).await {
        tracing::debug!("triggers: mask refresh failed (keeping old): {e}");
    }
}

// ── field validation: one answer, four write surfaces ───────────────
//
// Four surfaces reach this table — `/triggers add`, `/triggers edit`,
// `POST /api/triggers/new`, `POST /api/triggers/:name` — and every one
// of them validated a different subset. `add` checked event, match and
// action but swallowed unknown KEYS, so `netwrok=synirc` meant
// `network: None`, which is ANY network. `edit` rejected unknown keys
// but checked no VALUES, so `match=contians` reached the store. Neither
// checked `min_role` at all.
//
// What an unchecked value costs is not a stuck row, it is a WIDER one.
// `TriggerRow::into_trigger` reads back with `unwrap_or`: an
// unparseable match kind becomes `Always` (matches every line in the
// channel), an unparseable action becomes `Slopdrop` (spends a bridge
// call), and `evaluate` reads a typo'd `min_role` as `Guest` — the
// floor of everything that reaches the hook, so the restriction the
// operator asked for is no restriction. Every default fails OPEN.

/// The events a trigger row may name: the six IRC events, and the six
/// Bluesky notifications the atproto
/// plugin forwards (`bsky.event`, the slopdrop module): a row scoped
/// `platform = atproto network = atproto channel = bluesky` binds them.
pub const EVENTS: &[&str] = &[
    "TEXT",
    "JOIN",
    "PART",
    "QUIT",
    "KICK",
    "NICK",
    "LIKE",
    "REPOST",
    "FOLLOW",
    "QUOTE",
    "REPLY",
    "MENTION",
    // A phone's events (`crate::device_events`, platform=device).
    "NOTIFICATION",
    "POWER",
];

/// The `k=v` keys `/triggers add|edit` accept. `add` used to ignore
/// anything else silently.
pub const FIELDS: &[&str] = &[
    "platform", "network", "channel", "event", "match", "pattern", "action", "arg", "cooldown",
    "cap", "min-role",
];

/// The platform a new row is for when it names none: a phone's events
/// are the device's (`crate::device_events`), the rest IRC's as before.
/// A NOTIFICATION row scoped to IRC by default would never fire.
pub fn default_platform(event: &str) -> &'static str {
    if crate::device_events::EVENTS.contains(&event) {
        crate::device_events::PLATFORM
    } else {
        "irc"
    }
}

/// Canonicalise an event name, or say what it should have been.
pub fn validate_event(s: &str) -> Result<String, String> {
    let up = s.trim().to_ascii_uppercase();
    if EVENTS.contains(&up.as_str()) {
        Ok(up)
    } else {
        Err(format!(
            "unknown event '{}' ({})",
            s.trim(),
            EVENTS.join("|")
        ))
    }
}

/// Canonicalise a match kind, or say what it should have been.
pub fn validate_match(s: &str) -> Result<String, String> {
    match MatchKind::parse(s.trim()) {
        Some(k) => Ok(k.as_str().to_string()),
        None => Err(format!(
            "unknown match '{}' (always|contains|regex|url|imageurl|bluesky)",
            s.trim()
        )),
    }
}

/// Canonicalise an action, or say what it should have been.
pub fn validate_action(s: &str) -> Result<String, String> {
    match ActionKind::parse(s.trim()) {
        Some(a) => Ok(a.as_str().to_string()),
        None => Err(format!(
            "unknown action '{}' (slopdrop|vision|bluesky|prompt|command)",
            s.trim()
        )),
    }
}

/// Canonicalise a minimum role. The one field whose typo fails OPEN:
/// `evaluate` resolves an unparseable floor to `Guest`, which every
/// sender that reaches the hook is already at or above.
pub fn validate_min_role(s: &str) -> Result<String, String> {
    match crate::acl::Role::parse(s.trim()) {
        Some(r) => Ok(r.as_str().to_string()),
        None => Err(format!(
            "unknown min-role '{}' (guest|user|trusted|admin|owner)",
            s.trim()
        )),
    }
}

/// Is this a `k=v` key the row has a home for?
pub fn validate_field(k: &str) -> Result<(), String> {
    if FIELDS.contains(&k) {
        Ok(())
    } else {
        Err(format!("unknown field '{k}' ({})", FIELDS.join(" ")))
    }
}

/// Split a `k=v …` argument line into tokens, honouring quotes.
///
/// `split_whitespace` was the whole parser, so a multi-word value was
/// untypeable from chat OR the CLI: `arg=Describe this image` became
/// `arg=Describe` plus two tokens that are not `k=v` pairs at all. Both
/// live vision rows had to be created by hand through REST JSON for
/// exactly this reason.
///
/// Rules, deliberately the shell's so nobody has to learn a second set:
/// whitespace separates; `"` and `'` quote a run and may open anywhere
/// in a token (`arg="two words"` and `"arg=two words"` are the same
/// thing); inside double quotes `\` escapes the next character; a quote
/// that never closes is an ERROR naming itself, never a silently
/// truncated value — a value cut in half is the failure this exists to
/// end, and guessing where it ended would just relocate it.
pub fn split_kv_line(line: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' && q == '"' {
                    match chars.next() {
                        Some(n) => cur.push(n),
                        None => return Err("trailing backslash in a quoted value".into()),
                    }
                } else if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    started = true;
                } else if c.is_whitespace() {
                    if started {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                } else {
                    cur.push(c);
                    started = true;
                }
            }
        }
    }
    if let Some(q) = quote {
        return Err(format!(
            "unterminated {q} quote — a value that opens a quote has to close it"
        ));
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

/// Re-join a shell's argv into one `k=v` line WITHOUT destroying the
/// boundaries the shell already established. The CLI takes `Vec<String>`
/// and joined it with spaces, so `veles … arg='two words'` — which the
/// shell had correctly delivered as ONE element — came back out as two
/// tokens, undoing the quoting a second time on the way in.
pub fn rejoin_argv(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if !a
                .chars()
                .any(|c| c.is_whitespace() || c == '"' || c == '\'')
            {
                return a.clone();
            }
            let quoted = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
            // Quote the VALUE half when it looks like a pair, so the
            // line stays readable in a transcript: `arg="two words"`
            // rather than `"arg=two words"`. Both parse identically.
            match a.split_once('=') {
                Some((k, v)) if !k.is_empty() && !k.contains(char::is_whitespace) => {
                    format!("{k}={}", quoted(v))
                }
                _ => quoted(a),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every enumerated field of a PERSISTED row, checked at read time.
/// `Ok` means `into_trigger` will not have to invent anything.
pub fn validate_row(r: &crate::store::TriggerRow) -> Result<(), String> {
    validate_event(&r.event)?;
    validate_match(&r.match_kind)?;
    validate_action(&r.action)?;
    if let Some(m) = &r.min_role {
        validate_min_role(m)?;
    }
    Ok(())
}

// ── small helpers ───────────────────────────────────────────────────

/// Case-insensitive glob (`*` and `?`), the semantics IRC channel
/// matching needs. No `[]` classes: nobody has wanted them yet.
pub fn glob_match_ci(pattern: &str, text: &str) -> bool {
    glob_match(&pattern.to_lowercase(), &text.to_lowercase())
}

pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            b'*' => {
                // collapse consecutive stars
                let rest = &p[1..];
                if rest.is_empty() {
                    return true;
                }
                for i in 0..=t.len() {
                    if inner(rest, &t[i..]) {
                        return true;
                    }
                }
                false
            }
            b'?' => !t.is_empty() && inner(&p[1..], &t[1..]),
            c => !t.is_empty() && t[0] == c && inner(&p[1..], &t[1..]),
        }
    }
    inner(pattern.as_bytes(), text.as_bytes())
}

/// Extract http(s) URLs from text, the way linkresolver does (scheme
/// + non-space run; trailing punctuation trimmed).
pub fn extract_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in text.split_whitespace() {
        let tok = tok.trim_start_matches(['(', '<', '"']);
        if !(tok.starts_with("http://") || tok.starts_with("https://")) {
            continue;
        }
        let url = tok.trim_end_matches([')', '>', '"', '.', ',', ';', '!', '?']);
        if !url.is_empty() {
            out.push(url.to_string());
        }
    }
    out
}

/// A regex-lite: `*` wildcard match over the pattern (the same glob),
/// case-SENSITIVE, for users who wrote a shell-style glob and called
/// it a regex. Real regex would pull a crate; nobody has asked.
fn regex_lite(pattern: &str, text: &str) -> bool {
    glob_match(pattern, text)
}

/// The listing `/triggers` renders.
pub fn listing(store: &Arc<std::sync::Mutex<Store>>) -> String {
    let rows = match store.lock() {
        Ok(s) => s.trigger_list(false).unwrap_or_default(),
        Err(_) => return "triggers: store unavailable".to_string(),
    };
    if rows.is_empty() {
        return "no triggers (/triggers add <name> network=… channel=… event=TEXT \
                action=slopdrop)"
            .to_string();
    }
    let mut out = String::new();
    for t in &rows {
        // A row the engine cannot read is marked here, with the field
        // that broke it. `evaluate` skips it silently (fail closed);
        // this is where that silence gets its explanation.
        if let Err(why) = validate_row(t) {
            out.push_str(&format!("⚠ {:<22} INVALID — {why}\n", t.name));
            continue;
        }
        out.push_str(&format!(
            "{} {:<22} {:<12} {:<18} {:<9} {}{}\n",
            if t.enabled { "✓" } else { "·" },
            t.name,
            t.event,
            format!(
                "{}/{}",
                t.network.as_deref().unwrap_or("*"),
                t.channel.as_deref().unwrap_or("*")
            ),
            t.match_kind.as_str(),
            t.action.as_str(),
            if t.cooldown_secs > 0 {
                format!(" cooldown={}s", t.cooldown_secs)
            } else {
                String::new()
            },
        ));
        if let Some(s) = &t.last_status {
            out.push_str(&format!("    last: {s} (fires: {})\n", t.fire_count));
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trig(name: &str, event: &str, mk: MatchKind, ak: ActionKind) -> Trigger {
        Trigger {
            id: 1,
            name: name.into(),
            enabled: true,
            platform: Some("irc".into()),
            network: Some("irc.synirc.net".into()),
            channel: Some("#flashsupport".into()),
            event: event.into(),
            match_kind: mk,
            pattern: None,
            action: ak,
            arg: None,
            cooldown_secs: 0,
            per_hour_cap: 0,
            min_role: None,
            fire_count: 0,
            last_fired_at: None,
            last_status: None,
        }
    }

    fn ev<'a>(text: &'a str) -> Event<'a> {
        Event {
            platform: "irc",
            network: "irc.synirc.net",
            channel: "#flashsupport",
            event: "TEXT",
            nick: "wrath",
            mask: "wrath@test",
            text,
            role: Some(crate::acl::Role::Owner),
        }
    }

    fn scratch() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("t.db")).expect("store");
        (dir, store)
    }

    /// A3 — the enumerated fields have ONE answer, and it is not
    /// "whatever the reader defaults to". `into_trigger` turned an
    /// unparseable `match_kind` into `Always` (matches every line) and
    /// an unparseable `action` into `Slopdrop` (spends a bridge call),
    /// and a typo'd `min_role` fell to `Guest`, which is the floor of
    /// everything that reaches the hook. All three widen.
    #[test]
    fn the_enumerated_fields_are_validated_in_one_place() {
        assert_eq!(validate_event("text").unwrap(), "TEXT", "canonicalises");
        assert_eq!(validate_event("JOIN").unwrap(), "JOIN");
        assert_eq!(validate_event("like").unwrap(), "LIKE", "a Bluesky event");
        let e = validate_event("MSG").unwrap_err();
        assert!(e.contains("MSG") && e.contains("TEXT"), "{e}");

        assert_eq!(validate_match("ImageURL").unwrap(), "imageurl");
        let e = validate_match("contians").unwrap_err();
        assert!(e.contains("contians") && e.contains("contains"), "{e}");

        assert_eq!(validate_action("Vision").unwrap(), "vision");
        let e = validate_action("visoin").unwrap_err();
        assert!(e.contains("visoin"), "{e}");

        // The one that fails OPEN when it is wrong: a floor nobody can
        // be below is not a floor.
        assert_eq!(validate_min_role("Trusted").unwrap(), "trusted");
        let e = validate_min_role("ownr").unwrap_err();
        assert!(e.contains("ownr") && e.contains("owner"), "{e}");

        // Unknown k=v keys. `add` used to swallow them, so
        // `network=` misspelled meant network: None — ANY network.
        assert!(validate_field("network").is_ok());
        assert!(validate_field("min-role").is_ok());
        let e = validate_field("netwrok").unwrap_err();
        assert!(e.contains("netwrok"), "{e}");
    }

    /// C1 — a multi-word value was untypeable from every surface,
    /// because `split_whitespace` was the whole parser. Both live vision
    /// rows carry a sentence in `arg=` and both had to be created by
    /// hand through REST JSON.
    #[test]
    fn a_quoted_value_survives_the_tokenizer_and_the_argv_rejoin() {
        let toks = split_kv_line(r#"coven-vision channel=#coven arg="Describe this image.""#)
            .expect("parses");
        assert_eq!(
            toks,
            vec![
                "coven-vision".to_string(),
                "channel=#coven".to_string(),
                "arg=Describe this image.".to_string(),
            ]
        );
        // Single quotes too, and the quote may wrap the whole pair.
        assert_eq!(
            split_kv_line("a arg='two words' 'b=three more words'").unwrap(),
            vec![
                "a".to_string(),
                "arg=two words".to_string(),
                "b=three more words".to_string()
            ]
        );
        // A quote inside double quotes, escaped.
        assert_eq!(
            split_kv_line(r#"arg="say \"hi\" to them""#).unwrap(),
            vec![r#"arg=say "hi" to them"#.to_string()]
        );
        // An apostrophe is ordinary inside double quotes — an English
        // sentence is the whole point of this field.
        assert_eq!(
            split_kv_line(r#"arg="what's in this image?""#).unwrap(),
            vec!["arg=what's in this image?".to_string()]
        );
        // An empty value is a value.
        assert_eq!(
            split_kv_line(r#"pattern="""#).unwrap(),
            vec!["pattern=".to_string()]
        );
        // Unterminated is an ERROR, never a silently halved value.
        let e = split_kv_line(r#"arg="never closed"#).unwrap_err();
        assert!(e.contains("unterminated"), "{e}");

        // The CLI half: the shell already split these correctly, and
        // joining with a bare space undid it.
        let argv = vec![
            "coven-vision".to_string(),
            "arg=Describe this image.".to_string(),
            "channel=#coven".to_string(),
        ];
        let line = rejoin_argv(&argv);
        assert_eq!(
            split_kv_line(&line).unwrap(),
            argv,
            "argv → line → tokens must round-trip: {line}"
        );
        // Readable in a transcript: the VALUE is quoted, not the pair.
        assert!(line.contains(r#"arg="Describe this image.""#), "{line}");
        // A value containing a quote survives the round trip too.
        let tricky = vec![r#"arg=say "hi" now"#.to_string()];
        assert_eq!(split_kv_line(&rejoin_argv(&tricky)).unwrap(), tricky);
    }

    /// A row already in the store with a bad enumerated field must fail
    /// CLOSED on the hot path — never widen to "match everything, forward
    /// it". The write surfaces make new ones impossible; this is the
    /// belt for rows that predate them.
    #[test]
    fn a_row_with_an_unparseable_field_never_fires() {
        let (dir, store) = scratch();
        let mut row = row_of(&trig(
            "bad",
            "TEXT",
            MatchKind::ImageUrl,
            ActionKind::Vision,
        ));
        row.match_kind = "contians".into();
        store.trigger_upsert(&row).unwrap();
        let brakes = Brakes::default();
        let out = evaluate(&store, &brakes, &ev("anything at all"), 1000);
        assert!(
            out.is_empty(),
            "an invalid row must not widen to Always/Slopdrop: {out:?}"
        );

        // And the human-facing surfaces SAY so — the hot path is quiet
        // (a per-line refusal row would be its own flood), so the answer
        // to "why didn't it react" lives where an operator looks.
        let arc = std::sync::Arc::new(std::sync::Mutex::new(store));
        let text = listing(&arc);
        assert!(
            text.contains("contians"),
            "the listing names the bad field: {text}"
        );
        drop(dir);
    }

    #[test]
    fn scope_and_match_decide_the_fire() {
        let mut t = trig("t", "TEXT", MatchKind::ImageUrl, ActionKind::Vision);
        t.platform = None;
        // in scope, image url present
        assert!(t.in_scope("irc", "irc.synirc.net", "#flashsupport"));
        assert!(t.matches_text("look https://x.example/pic.png)"));
        assert!(!t.matches_text("look https://x.example/page.html"));
        assert!(!t.matches_text("no url here"));
        // channel glob
        t.channel = Some("#flash*".into());
        assert!(t.in_scope("irc", "irc.synirc.net", "#flashsupport"));
        assert!(!t.in_scope("irc", "irc.synirc.net", "#coven"));
        // network spelled exactly (case-insensitive)
        t.channel = None;
        t.network = Some("irc.SYNIRC.net".into());
        assert!(t.in_scope("irc", "irc.synirc.net", "#any"));
    }

    #[test]
    fn cooldown_and_cap_refuse_with_reasons() {
        let mut t = trig("t", "TEXT", MatchKind::Always, ActionKind::Slopdrop);
        t.cooldown_secs = 60;
        let mut b = Brakes::default();
        assert!(b.peek(&t, "#c", 1000).is_empty());
        b.commit(&t, "#c", 1000);
        let err = b.peek(&t, "#c", 1030).join("; ");
        assert!(err.contains("cooldown"), "{err}");
        assert!(b.peek(&t, "#c", 1061).is_empty());
        b.commit(&t, "#c", 1061);
        // cap
        t.cooldown_secs = 0;
        t.per_hour_cap = 2;
        let mut b2 = Brakes::default();
        assert!(b2.peek(&t, "#c", 100).is_empty());
        b2.commit(&t, "#c", 100);
        assert!(b2.peek(&t, "#c", 200).is_empty());
        b2.commit(&t, "#c", 200);
        let err = b2.peek(&t, "#c", 300).join("; ");
        assert!(err.contains("cap"), "{err}");
        // window slides
        assert!(b2.peek(&t, "#c", 3600 + 101).is_empty());
    }

    #[test]
    fn a_min_role_below_the_sender_refuses_with_a_reason() {
        let mut t = trig("t", "TEXT", MatchKind::Always, ActionKind::Slopdrop);
        t.min_role = Some("trusted".into());
        let mut e = ev("hi");
        e.role = Some(crate::acl::Role::Guest);
        let (dir, store) = scratch();
        store.trigger_upsert(&row_of(&t)).unwrap();
        let b = Brakes::default();
        let out = evaluate(&store, &b, &e, 1000);
        assert_eq!(out.len(), 1);
        let (_, r) = &out[0];
        assert!(r.as_ref().unwrap_err().contains("min-role"));
        drop(dir);
    }

    #[test]
    fn evaluate_fires_a_matching_row_and_skips_the_rest() {
        let (dir, store) = scratch();
        let mut a = trig("a", "TEXT", MatchKind::Always, ActionKind::Slopdrop);
        a.name = "a".into();
        let mut b = trig("b", "TEXT", MatchKind::Contains, ActionKind::Slopdrop);
        b.name = "b".into();
        b.pattern = Some("zoom".into());
        store.trigger_upsert(&row_of(&a)).unwrap();
        store.trigger_upsert(&row_of(&b)).unwrap();
        let rows = store.trigger_list(true).unwrap();
        assert_eq!(rows.len(), 2);
        let brakes = Brakes::default();
        let out = evaluate(&store, &brakes, &ev("hello"), 1000);
        assert_eq!(out.len(), 1, "only the always-row matches 'hello'");
        assert_eq!(out[0].0.name, "a");
        let out = evaluate(&store, &brakes, &ev("zoom call"), 2000);
        assert_eq!(out.len(), 2);
        drop(dir);
    }

    /// The store's persisted shape of an engine Trigger.
    fn row_of(t: &Trigger) -> crate::store::TriggerRow {
        crate::store::TriggerRow {
            id: t.id,
            name: t.name.clone(),
            enabled: t.enabled,
            platform: t.platform.clone(),
            network: t.network.clone(),
            channel: t.channel.clone(),
            event: t.event.clone(),
            match_kind: t.match_kind.as_str().to_string(),
            pattern: t.pattern.clone(),
            action: t.action.as_str().to_string(),
            arg: t.arg.clone(),
            cooldown_secs: t.cooldown_secs,
            per_hour_cap: t.per_hour_cap,
            min_role: t.min_role.clone(),
            fire_count: t.fire_count,
            last_fired_at: t.last_fired_at.clone(),
            last_status: t.last_status.clone(),
        }
    }

    /// A1 — the bridge budget. `[triggers] max_per_minute` is a
    /// per-(network, channel) brake in FRONT of slopdrop, not a per-row
    /// one: the interpreter on the far side is single-threaded, so what
    /// has to be bounded is forwards per CHANNEL, whatever mix of rows
    /// produced them. The knob had zero readers when this was written —
    /// config said 30/min and nothing counted.
    #[test]
    fn the_bridge_budget_bounds_forwards_per_channel_per_minute() {
        let mut b = Brakes::default();
        let (net, chan) = ("irc.synirc.net", "#flashsupport");
        // Three allowed, the fourth refused, all inside one minute.
        for i in 0..3 {
            assert!(
                b.bridge_claim(net, chan, 1000 + i, 3).is_ok(),
                "forward {i} is inside the budget"
            );
        }
        let err = b.bridge_claim(net, chan, 1003, 3).unwrap_err();
        assert!(err.contains("bridge"), "{err}");
        assert!(err.contains("max 3"), "the refusal names the budget: {err}");

        // A DIFFERENT channel has its own budget — the brake is per
        // channel, not global, or one busy room silences every other.
        assert!(
            b.bridge_claim(net, "#coven", 1003, 3).is_ok(),
            "another channel is not spent by this one"
        );

        // The window slides: a minute later the budget is whole again.
        assert!(
            b.bridge_claim(net, chan, 1000 + 61, 3).is_ok(),
            "the per-minute window slides"
        );

        // 0 disables the brake, the same spelling `timers_poll_secs`
        // uses — an operator who wants no ceiling says so with 0.
        let mut b2 = Brakes::default();
        for i in 0..50 {
            assert!(
                b2.bridge_claim(net, chan, 2000 + i, 0).is_ok(),
                "max_per_minute = 0 disables the brake"
            );
        }
    }

    /// A2 — `evaluate` must not BURN a brake it only inspected. The
    /// forward mask and the action both run after it, so a row whose
    /// event is masked out (or whose Vision arm finds no image) used to
    /// consume the cooldown slot for a fire that never happened — and
    /// the next real event inside that window was refused for it.
    #[test]
    fn evaluate_inspects_the_brakes_without_spending_them() {
        let (dir, store) = scratch();
        let mut t = trig("cool", "TEXT", MatchKind::Always, ActionKind::Slopdrop);
        t.cooldown_secs = 60;
        store.trigger_upsert(&row_of(&t)).unwrap();
        let mut brakes = Brakes::default();

        // Evaluate twice inside the cooldown. NEITHER call spends
        // anything, so the second must still be a fire, not a refusal.
        let out = evaluate(&store, &brakes, &ev("hello"), 1000);
        assert_eq!(out.len(), 1);
        assert!(out[0].1.is_ok(), "first evaluate matches: {:?}", out[0].1);

        let out = evaluate(&store, &brakes, &ev("hello"), 1010);
        assert!(
            out[0].1.is_ok(),
            "evaluate must not have burned the cooldown: {:?}",
            out[0].1
        );

        // The CALLER commits, at the moment it actually spends
        // something — and only then does the cooldown bite.
        brakes.commit(&out[0].0, "#flashsupport", 1010);
        let out = evaluate(&store, &brakes, &ev("hello"), 1020);
        let err = out[0].1.as_ref().unwrap_err();
        assert!(err.contains("cooldown"), "after a commit it bites: {err}");

        // And past the window it opens again.
        let out = evaluate(&store, &brakes, &ev("hello"), 1010 + 61);
        assert!(out[0].1.is_ok(), "{:?}", out[0].1);
        drop(dir);
    }

    /// With no `triggers.forward` provider a forward is refused with the
    /// reason (never silently empty), a refresh says so, and the feature
    /// reads as unconfigured. With one, the event and the answered links
    /// reach it as the contract types say, and its replies come back.
    /// The registry is process-global; this is the one test here that
    /// provides these names.
    #[tokio::test]
    async fn without_a_bridge_provider_a_forward_is_refused_with_the_reason() {
        assert!(!forwarder_configured());
        let cfg = VelesConfig::default();
        let ev = Event {
            platform: "irc",
            network: "irc.synirc.net",
            channel: "#x",
            event: "TEXT",
            nick: "a",
            mask: "a@b",
            text: "hi",
            role: Some(crate::acl::Role::User),
        };
        assert_eq!(forward(&ev, &[]).await.unwrap_err(), NO_FORWARDER);
        let (_d, store) = {
            let d = tempfile::tempdir().unwrap();
            let s = Arc::new(std::sync::Mutex::new(
                Store::open(&d.path().join("state.db")).unwrap(),
            ));
            (d, s)
        };
        assert_eq!(refresh_masks(&cfg, &store).await.unwrap_err(), NO_FORWARDER);

        let owner = crate::lifecycle::owner_tag("module:triggers-test-bridge");
        let decl = |name: &str| crate::services::ServiceDecl {
            name: name.into(),
            ..Default::default()
        };
        crate::services::register_for(
            owner,
            decl(TRIGGERS_FORWARD),
            crate::services::handler(|p, _cx| async move {
                let p: veles_types::plugin::TriggersForwardParams =
                    serde_json::from_value(p).map_err(|e| e.to_string())?;
                let message = format!(
                    "relayed {} from {} ({}), {} answered",
                    p.event.text,
                    p.event.nick,
                    p.event.role.unwrap_or_default(),
                    p.answered_urls.len()
                );
                Ok(serde_json::json!({"replies": [{"channel": p.event.channel, "message": message}]}))
            }),
        )
        .unwrap();
        crate::services::register_for(
            owner,
            decl(TRIGGERS_MASKS),
            crate::services::handler(|_p, _cx| async move { Ok(serde_json::json!({"masks": []})) }),
        )
        .unwrap();
        assert!(forwarder_configured());
        assert_eq!(
            forward(&ev, &[]).await.unwrap(),
            vec![(
                "#x".to_string(),
                "relayed hi from a (user), 0 answered".to_string()
            )]
        );
        // The answered links reach the bridge.
        let answered = vec!["https://bsky.app/profile/a.b/post/1".to_string()];
        assert_eq!(
            forward(&ev, &answered).await.unwrap(),
            vec![(
                "#x".to_string(),
                "relayed hi from a (user), 1 answered".to_string()
            )]
        );
        assert!(refresh_masks(&cfg, &store).await.unwrap().is_empty());
        crate::services::purge_owner(Some(owner));
        assert!(!forwarder_configured());
    }

    #[test]
    fn the_mask_override_wins_over_the_derivation() {
        let mut m = ChannelMask {
            events: vec!["TEXT".into()],
            override_events: None,
        };
        assert!(m.allows("TEXT"));
        assert!(!m.allows("JOIN"));
        m.override_events = Some(vec!["JOIN".into()]);
        assert!(!m.allows("TEXT"), "the override replaced, not extended");
        assert!(m.allows("JOIN"));
        assert!(!m.is_derived());
    }

    #[test]
    fn urls_are_extracted_with_punctuation_trimmed() {
        let urls = extract_urls("see https://example.com/a.png, and <http://b.io/x.jpg>");
        assert_eq!(
            urls,
            vec![
                "https://example.com/a.png".to_string(),
                "http://b.io/x.jpg".to_string()
            ]
        );
    }

    // ── the mask cache: the wiring the empty-cache bug was about ────
    //
    // The cache is a process-global singleton and `install_masks`
    // REPLACES the whole map, so these tests would clobber each other
    // under the default parallel test harness. A shared lock serialises
    // them; poison-tolerant so one panic does not wedge the rest.
    static CACHE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn cache_guard() -> std::sync::MutexGuard<'static, ()> {
        CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The regression that froze two of the three trigger rows: the
    /// hot path reads `mask_allows`, and NOTHING ever populated the
    /// cache it read, so every Slopdrop forward was refused with "mask:
    /// event not wanted". This pins the round-trip the fix relies on —
    /// `install_masks` writes what `mask_allows` then reads, through the
    /// shared core cache. Revert by making `mask_allows` answer from a
    /// different store than `install_masks` writes, or by no-op'ing the
    /// install, and this goes red.
    #[test]
    fn mask_allows_reads_what_install_masks_wrote() {
        let _g = cache_guard();
        let (net, chan) = ("net.roundtrip", "#install-read");
        install_masks(HashMap::from([(
            mask_key(net, chan),
            ChannelMask {
                events: vec!["TEXT".into(), "JOIN".into()],
                override_events: None,
            },
        )]));

        assert!(
            mask_allows(net, chan, "TEXT"),
            "an installed derived mask must be readable by the hot path"
        );
        assert!(mask_allows(net, chan, "JOIN"), "JOIN is in the mask");
        assert!(
            !mask_allows(net, chan, "QUIT"),
            "QUIT is not in the mask: a forward for it must be refused"
        );
        // The empty-cache safety direction: a channel nobody derived a
        // mask for answers false (skip the forward), never true.
        assert!(
            !mask_allows(net, "#never-populated", "TEXT"),
            "an unknown channel forwards nothing"
        );
        // Case-insensitive: IRC events arrive upper, but the contract
        // should not hinge on that.
        assert!(mask_allows(net, chan, "text"), "event match ignores case");
    }

    /// An operator who pins a mask with `/triggers mask` must see it
    /// take effect immediately, not after a ten-minute TTL. The override
    /// is locally determined — `ChannelMask::allows` answers from
    /// `override_events` alone — so installing needs no slopdrop. And
    /// `auto` (None) removes it, reverting to the safe empty answer
    /// until the next derive.
    #[test]
    fn a_mask_override_takes_effect_and_reverts_without_a_refresh() {
        let _g = cache_guard();
        let (net, chan) = ("net.override", "#pin-now");

        // Start empty: nothing forwards.
        set_override_in_cache(net, chan, None);
        assert!(
            !mask_allows(net, chan, "TEXT"),
            "a reverted channel forwards nothing until re-derived"
        );

        // Pin to JOIN only. TEXT must stop, JOIN must start — from the
        // cache, with no slopdrop call in between.
        set_override_in_cache(net, chan, Some(vec!["JOIN".into()]));
        assert!(
            mask_allows(net, chan, "JOIN"),
            "the pinned override allows the event it names"
        );
        assert!(
            !mask_allows(net, chan, "TEXT"),
            "an override REPLACES the derived mask, it does not extend it"
        );

        // `auto`: back to derived, which (cache-wise) is the empty
        // answer until the next refresh.
        set_override_in_cache(net, chan, None);
        assert!(
            !mask_allows(net, chan, "JOIN"),
            "reverting to auto clears the override from the live cache"
        );
    }

    /// A failed refresh is not a no-op: the masks already installed
    /// keep deciding every forward, so the report has to say how old
    /// they are — or that there are none, which is the louder answer.
    #[test]
    fn a_failed_refresh_reports_what_is_still_deciding() {
        let with = refresh_failure_report("slopdrop unreachable", Some(742));
        assert!(with.contains("slopdrop unreachable"), "{with}");
        assert!(with.contains("742s old"), "names the age: {with}");
        assert!(with.contains("keep deciding"), "{with}");

        let without = refresh_failure_report("slopdrop unreachable", None);
        assert!(
            without.contains("NO masks are installed"),
            "an empty cache is the louder answer: {without}"
        );
        assert!(
            !without.contains("old"),
            "no age to report when nothing was ever installed: {without}"
        );
    }

    /// A full install records a refresh time; a single-channel override
    /// must NOT, because bumping the TTL on one pin would delay the next
    /// real slopdrop derive for every other channel by ten minutes.
    ///
    /// This test used to assert `masks_age_secs() < 5` after the
    /// override — which is ALSO true when the override resets the clock,
    /// so the mutation it names could never turn it red. An audit that
    /// cannot fail proves nothing. It measures at Duration resolution
    /// now: let a gap open after the install, then require the clock to
    /// be at least that old still.
    #[test]
    fn install_sets_the_refresh_clock_but_a_single_override_does_not() {
        let _g = cache_guard();
        // A full install stamps the clock.
        install_masks(HashMap::from([(
            mask_key("net.clock", "#a"),
            ChannelMask::default(),
        )]));
        // Let a measurable gap open, so "did not reset" is a claim with
        // evidence rather than a bound both answers satisfy.
        let gap = std::time::Duration::from_millis(60);
        std::thread::sleep(gap);
        let before = masks_age().expect("a full install stamps the refresh clock");
        assert!(
            before >= gap,
            "the clock should be at least the gap old: {before:?}"
        );

        // A single-channel override must not move it.
        set_override_in_cache("net.clock", "#b", Some(vec!["TEXT".into()]));
        let after = masks_age().expect("the override left the clock intact");
        assert!(
            after >= before,
            "the override RESET the refresh clock ({before:?} → {after:?}); \
             one pin would then delay the next real derive for every other \
             channel by the whole TTL"
        );
    }
}
