//! The gateway's message bus: what platform adapters push inbound
//! messages into and replies come back out of, and the process-wide
//! handle to the running one.
//!
//! It lived in `frontends/gateway.rs` until 2026-09-28, when the
//! frontends moved into their own crate (`veles-frontends`) to cut
//! veles-core's compile (docs/build-memory.md). Cron, the ask relay and
//! the plugin bridge deliver through this bus, so it stays in core; the
//! gateway re-exports it, and `veles_frontends::gateway::GatewayBus` is
//! still the same type.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::lockutil::LockOk;

pub use veles_types::bus::{
    meta_keys, InboundMessage, MessageKind, OutboundMessage, DEFAULT_ROUTE,
};

/// Platform adapter contract. Implementations are long-running tasks.
#[async_trait]
pub trait PlatformAdapter: Send {
    fn name(&self) -> &str;
    /// Run the adapter until it exits. `tx` delivers replies back to the
    /// platform; inbound messages come back through the gateway bus.
    async fn run(&self, bus: GatewayBus) -> Result<()>;
}

/// Handle shared with adapters: push inbound messages, register the
/// adapter's outbound channel. The outbound registry is shared with the
/// gateway so replies route back to the right adapter.
#[derive(Clone)]
pub struct GatewayBus {
    pub inbound: tokio::sync::mpsc::UnboundedSender<InboundMessage>,
    /// platform → ROUTE → adapter channel.
    ///
    /// The inner map is the 2026-09-11 fix, and the bug it closes was
    /// live: one platform can have SEVERAL adapter instances — two IRC
    /// networks are two connections, two rosters and two audiences — and
    /// this was `HashMap<String, Sender>`, so the second
    /// `register_outbound("irc", …)` silently REPLACED the first. Every
    /// reply on the platform then went out on whichever network happened
    /// to start last, addressed to a channel name from the other one. The
    /// bot went mute on one network and, where a channel of the same name
    /// existed on the other, said it to the wrong room.
    ///
    /// `""` is the default route, which is what a single-instance
    /// platform (telegram, a2a) registers under and what a caller that
    /// names no route resolves to. So nothing about the single-adapter
    /// case changed.
    pub outbound: Arc<Mutex<HashMap<String, HashMap<String, OutboundTx>>>>,
    /// Last chat each platform ROUTE talked to — used for lifecycle
    /// notices ("gateway stopping") that have no turn context to reply
    /// to. Persisted so boot announcements reach the right chat even
    /// after a restart.
    ///
    /// Keyed by `last_chat_key`: the bare platform for the default
    /// route, `platform@route` otherwise — so a state file written
    /// before routes existed still resolves.
    pub last_chat: Arc<Mutex<HashMap<String, String>>>,
    pub state_path: std::path::PathBuf,
}

/// The wrapped sender `register_outbound` installs. Named because it is
/// now two levels deep in a map and the signature was unreadable.
pub type OutboundTx = tokio::sync::mpsc::UnboundedSender<OutboundMessage>;

/// Where `last_chat` files a (platform, route) pair.
///
/// The default route keeps the bare platform key so a `gateway-state.json`
/// written before routes existed is read back unchanged.
fn last_chat_key(platform: &str, route: &str) -> String {
    if route.is_empty() {
        platform.to_string()
    } else {
        format!("{platform}@{route}")
    }
}

/// A PUBLIC one-to-many feed (a Bluesky account): every line the bot
/// sends there is a public post, not a chat message. Such a platform
/// never gets an unsolicited broadcast (lifecycle "gateway up"/
/// "stopping": an unprompted line threads under whatever the account
/// last touched, a context it does not belong in), and a non-owner's
/// slash commands are limited to the few whose answers are meant to be
/// public (`gateway.rs`, `public_feed_refusal`).
pub fn is_public_feed(platform: &str) -> bool {
    matches!(platform, "atproto") || public_feeds().keys().iter().any(|k| k == platform)
}

/// Platforms a plugin DECLARED public (an ABI 4 `register_platform`
/// with `public_feed`, R8): owner-tracked, purged with the plugin.
fn public_feeds() -> &'static crate::owned::OwnedRegistry<String, ()> {
    static CELL: std::sync::OnceLock<crate::owned::OwnedRegistry<String, ()>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(crate::owned::OwnedRegistry::default)
}

/// Declare `platform` a public feed for the public-reply policy. The
/// registering thread's owner is recorded, so a purge undoes it.
pub fn declare_public_feed(platform: &str) {
    public_feeds().insert(platform.to_string(), ());
}

pub fn purge_owner_public_feeds(owner: Option<&'static str>) -> usize {
    public_feeds().purge(owner)
}

impl GatewayBus {
    fn note_last_chat(&self, platform: &str, route: &str, chat_id: &str) {
        if chat_id.is_empty() {
            return;
        }
        let key = last_chat_key(platform, route);
        let changed = {
            let mut m = self.last_chat.lock_ok();
            let prev = m.get(&key).cloned();
            if prev.as_deref() != Some(chat_id) {
                m.insert(key, chat_id.to_string());
                true
            } else {
                false
            }
        };
        if changed {
            // Tiny file, written only on change — cheap enough per
            // message, and it's what makes notices survive restarts.
            let map = self.last_chat.lock_ok().clone();
            let _ = std::fs::write(
                &self.state_path,
                serde_json::to_string(&map).unwrap_or_default(),
            );
        }
    }

    pub fn send_inbound(&self, msg: InboundMessage) {
        self.note_last_chat(&msg.platform, msg.route(), &msg.chat_id);
        let _ = self.inbound.send(msg);
    }

    /// Adapter registers its outbound channel for its platform key.
    ///
    /// F2: THE outbound choke point — and it has to be HERE, not in
    /// [`send_outbound`](Self::send_outbound), because turn replies and
    /// mid-turn steps send through a CLONE of the registered channel
    /// (the progress/step paths), bypassing every method on the bus.
    /// Wrapping the channel itself catches all of them. Every message
    /// passes two layers before any adapter sees it: every value in
    /// the secrets map (`secrets.env` layered over `.env`, ≥4 chars)
    /// becomes `NAME=<REDACTED>`, then credential-SHAPED strings
    /// (sk-/ghp_/xox/AKIA/JWT/Bearer/email) are redacted even when the
    /// value was never registered — the last-ditch net for a key the
    /// model dug out of a file. Applies to ALL deliveries, owner
    /// included: the owner who really wants a secret cats the file
    /// themselves; no chat transcript is a safe place for one.
    /// (Consequence, accepted: credential-shaped emails redact in
    /// owner chats too.)
    pub fn register_outbound(
        &self,
        platform: &str,
        tx: tokio::sync::mpsc::UnboundedSender<OutboundMessage>,
    ) {
        self.register_outbound_route(platform, DEFAULT_ROUTE, tx);
    }

    /// Register one ADAPTER INSTANCE of a platform.
    ///
    /// A platform with several instances — one IRC adapter per network —
    /// registers each under its own route, and replies name the route
    /// they came in on. Without this the second registration replaced
    /// the first and half the connections went mute; see the `outbound`
    /// field for the live failure.
    ///
    /// A route is never overwritten silently: re-registering the same
    /// one (a reconnect installing a fresh channel) is normal and
    /// replaces it, but that is the same key, not a different adapter
    /// claiming an occupied one.
    pub fn register_outbound_route(
        &self,
        platform: &str,
        route: &str,
        tx: tokio::sync::mpsc::UnboundedSender<OutboundMessage>,
    ) {
        let (wrap_tx, mut wrap_rx) = tokio::sync::mpsc::unbounded_channel::<OutboundMessage>();
        crate::rt::spawn(async move {
            while let Some(mut msg) = wrap_rx.recv().await {
                // `scrubbable`, not `load`: the injection map holds every
                // .env key, so scrubbing with it redacted the server
                // hostname and the channel name out of ordinary replies.
                let secrets = crate::secrets::scrubbable();
                msg.text = crate::a2a::redact_outbound(&crate::secrets::scrub(&msg.text, &secrets));
                if let Some(r) = msg.reasoning.as_mut() {
                    *r = crate::a2a::redact_outbound(&crate::secrets::scrub(r, &secrets));
                }
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        self.outbound
            .lock_ok()
            .entry(platform.to_string())
            .or_default()
            .insert(route.to_string(), wrap_tx);
    }

    /// Take one adapter instance's route off the bus: it stopped
    /// answering for the platform on this node (a process plugin whose
    /// lease went elsewhere). An emptied platform goes too, so
    /// [`Self::has_adapter`] says "not here".
    pub fn unregister_outbound_route(&self, platform: &str, route: &str) {
        let mut map = self.outbound.lock_ok();
        if let Some(routes) = map.get_mut(platform) {
            routes.remove(route);
            if routes.is_empty() {
                map.remove(platform);
            }
        }
    }

    /// Push an outbound message to a platform's default adapter.
    ///
    /// Kept for callers with no route in hand — cron, the plugin bridge,
    /// anything replying to a platform that has only one adapter.
    pub fn send_outbound(&self, platform: &str, msg: OutboundMessage) -> bool {
        self.send_outbound_to(platform, None, msg)
    }

    /// Push an outbound message to ONE adapter instance of a platform.
    ///
    /// Resolution, in order, and the fallbacks are what keep every
    /// route-unaware caller working:
    ///
    ///  1. the named route, when that adapter is registered;
    ///  2. the default route (`""`), which is where a single-adapter
    ///     platform lives;
    ///  3. the only registered route, when there is exactly one — a
    ///     single-network IRC setup registers under its network name,
    ///     and a caller that names no route still reaches it.
    ///
    /// With SEVERAL routes and no match there is no safe guess, so it
    /// returns false rather than picking one: delivering a reply to the
    /// wrong network is worse than not delivering it, because the wrong
    /// network is a different audience.
    pub fn send_outbound_to(
        &self,
        platform: &str,
        route: Option<&str>,
        msg: OutboundMessage,
    ) -> bool {
        let Some((resolved, tx)) = self.resolve_outbound(platform, route) else {
            return false;
        };
        self.note_last_chat(platform, &resolved, &msg.chat_id);
        tx.send(msg).is_ok()
    }

    /// Does an adapter for `platform` run in this process right now? An
    /// adapter registers when its lease is granted and goes with it.
    pub fn has_adapter(&self, platform: &str) -> bool {
        self.outbound
            .lock_ok()
            .get(platform)
            .is_some_and(|routes| !routes.is_empty())
    }

    /// The adapter channel a (platform, route) resolves to, and the
    /// route it actually landed on.
    ///
    /// ONE resolution rule, in one place, because the turn loop needs
    /// the raw channel too: mid-turn progress and step messages are sent
    /// through a CLONE of it rather than through `send_outbound`, and a
    /// second copy of "which adapter is this" would be a second copy
    /// that drifts.
    fn resolve_outbound(
        &self,
        platform: &str,
        route: Option<&str>,
    ) -> Option<(String, OutboundTx)> {
        let want = route.unwrap_or(DEFAULT_ROUTE);
        let map = self.outbound.lock_ok();
        let routes = map.get(platform)?;
        let pick = routes
            .get_key_value(want)
            .or_else(|| routes.get_key_value(DEFAULT_ROUTE))
            .or_else(|| {
                if routes.len() == 1 {
                    routes.iter().next()
                } else {
                    None
                }
            });
        match pick {
            Some((k, tx)) => Some((k.clone(), tx.clone())),
            None => {
                tracing::warn!(
                    "gateway: no adapter for {platform} route {want:?} \
                     ({} registered) — dropping rather than guessing a network",
                    routes.len()
                );
                None
            }
        }
    }

    /// The adapter channel for one instance, for callers that send
    /// through it directly (the turn loop's progress and step messages).
    pub fn outbound_channel(&self, platform: &str, route: Option<&str>) -> Option<OutboundTx> {
        self.resolve_outbound(platform, route).map(|(_, tx)| tx)
    }

    /// Platforms with a live outbound channel (cron `deliver: all`).
    pub fn outbound_platforms(&self) -> Vec<String> {
        self.outbound.lock_ok().keys().cloned().collect()
    }

    /// Every live adapter instance, as (platform, route).
    pub fn outbound_routes(&self) -> Vec<(String, String)> {
        self.outbound
            .lock_ok()
            .iter()
            .flat_map(|(pl, routes)| routes.keys().map(|r| (pl.clone(), r.clone())))
            .collect()
    }

    /// Send a lifecycle notice (no turn context) to the last chat each
    /// adapter INSTANCE talked to. Returns how many got it.
    ///
    /// Per route, not per platform: with two IRC networks the boot
    /// announcement used to go out once, on one connection, addressed to
    /// the last chat the whole platform saw — which live meant the
    /// IRC4Fun connection saying "gateway up" to `#flashsupport`, a
    /// synirc channel it is not even in.
    ///
    /// PUBLIC social surfaces are excluded. A lifecycle notice is
    /// operational chatter for a chat operator, not content — and on a
    /// public Bluesky account it lands as a PUBLIC POST threaded under
    /// whatever the bot last replied to (USER 2026-10-06: a "▶ veles
    /// gateway up" line appeared in a stranger's thread). Broadcast is
    /// for surfaces where an unsolicited line is normal (IRC, Telegram
    /// chats the bot is in); a one-to-many public feed is not that.
    pub fn broadcast_notice(&self, text: &str) -> usize {
        let mut sent = 0usize;
        for (pl, route) in self.outbound_routes() {
            if is_public_feed(&pl) {
                continue;
            }
            let chat = self
                .last_chat
                .lock_ok()
                .get(&last_chat_key(&pl, &route))
                .cloned()
                .unwrap_or_default();
            if chat.is_empty() {
                continue;
            }
            if self.send_outbound_to(
                &pl,
                Some(&route),
                OutboundMessage::plain(&chat, text.to_string(), MessageKind::Reply),
            ) {
                sent += 1;
            }
        }
        sent
    }
}

/// The running gateway's bus, parked process-wide. `Gateway::new` sets
/// it; the plugin bridge's `veles::send_outbound` reads it. None while
/// no gateway process is up — plugin outbound then fails with a clean
/// "gateway not running" error instead of vanishing.
static GLOBAL_BUS: std::sync::OnceLock<std::sync::RwLock<Option<GatewayBus>>> =
    std::sync::OnceLock::new();

fn global_bus_cell() -> &'static std::sync::RwLock<Option<GatewayBus>> {
    GLOBAL_BUS.get_or_init(|| std::sync::RwLock::new(None))
}

pub fn set_global_bus(bus: GatewayBus) {
    if let Ok(mut w) = global_bus_cell().write() {
        *w = Some(bus);
    }
}

pub fn global_bus() -> Option<GatewayBus> {
    global_bus_cell().read().ok()?.clone()
}

#[cfg(test)]
mod tests {
    /// A public one-to-many feed never gets an unsolicited lifecycle
    /// blast; a chat surface does. (The "/t" short-link and broadcast
    /// share nothing else — this guards the atproto exclusion.)
    #[test]
    fn broadcast_skips_public_feeds_but_not_chat() {
        assert!(super::is_public_feed("atproto"));
        assert!(!super::is_public_feed("irc"));
        assert!(!super::is_public_feed("telegram"));
        assert!(!super::is_public_feed("desktop"));
    }
}

// ── delivery observers (PLAN-plugins-installer.md R5) ──────────────────
//
// A platform's plugin answers a delivery with a receipt: the ids it
// posted, and `meta` it wants recorded (a hosted copy's link and blob
// ids). What is recorded WHERE is not the host's business: a module that
// stamped `meta` on the outbound message (the slopdrop ledger's entry
// id) registers an observer and files the receipt itself. The process
// host calls `notify_delivered` after every delivery, with the outbound
// message's `meta`, the receipt's ids and the receipt's `meta`.
// OWNER-TRACKED (M4): purged with the module on reload.

/// Sees every delivery receipt.
pub trait DeliveryObserver: Send + Sync {
    fn delivered(
        &self,
        outbound_meta: &std::collections::BTreeMap<String, String>,
        ids: &[String],
        receipt_meta: &std::collections::BTreeMap<String, String>,
    );
}

fn delivery_observers() -> &'static crate::owned::OwnedRegistry<String, Arc<dyn DeliveryObserver>> {
    static R: std::sync::OnceLock<crate::owned::OwnedRegistry<String, Arc<dyn DeliveryObserver>>> =
        std::sync::OnceLock::new();
    R.get_or_init(crate::owned::OwnedRegistry::default)
}

/// Register `observer` under `name` (one per concern; a re-register by
/// the same owner replaces). The owner is read from the calling thread.
pub fn register_delivery_observer(name: &str, observer: Arc<dyn DeliveryObserver>) {
    delivery_observers().insert(name.to_string(), observer);
}

/// Remove every observer `owner` registered (R3 teardown). `None`
/// (core) removes nothing. Returns the count removed.
pub fn purge_owner_delivery_observers(owner: Option<&'static str>) -> usize {
    delivery_observers().purge(owner)
}

/// Tell every observer about one delivery.
pub fn notify_delivered(
    outbound_meta: &std::collections::BTreeMap<String, String>,
    ids: &[String],
    receipt_meta: &std::collections::BTreeMap<String, String>,
) {
    let all: Vec<Arc<dyn DeliveryObserver>> = delivery_observers().values_cloned();
    for o in all {
        o.delivered(outbound_meta, ids, receipt_meta);
    }
}

#[cfg(test)]
mod observer_tests {
    use super::*;

    struct Count(std::sync::Mutex<Vec<String>>);
    impl DeliveryObserver for Count {
        fn delivered(
            &self,
            outbound_meta: &std::collections::BTreeMap<String, String>,
            ids: &[String],
            receipt_meta: &std::collections::BTreeMap<String, String>,
        ) {
            self.0.lock().unwrap().push(format!(
                "{}:{}:{}",
                outbound_meta
                    .get("ledger.entry")
                    .cloned()
                    .unwrap_or_default(),
                ids.join(","),
                receipt_meta.get("hosted.link").cloned().unwrap_or_default()
            ));
        }
    }

    #[test]
    fn every_observer_sees_the_receipt_with_both_metas() {
        let c = Arc::new(Count(std::sync::Mutex::new(Vec::new())));
        register_delivery_observer("count-test", c.clone());
        let mut om = std::collections::BTreeMap::new();
        om.insert("ledger.entry".to_string(), "e1".to_string());
        let mut rm = std::collections::BTreeMap::new();
        rm.insert("hosted.link".to_string(), "https://x/t/1".to_string());
        notify_delivered(&om, &["at://p/1".to_string()], &rm);
        assert_eq!(
            c.0.lock().unwrap().as_slice(),
            ["e1:at://p/1:https://x/t/1"]
        );
    }
}
