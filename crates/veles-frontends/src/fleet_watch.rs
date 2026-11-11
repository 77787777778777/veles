//! Fleet alerts (0.2.0, owner ask 287976.1): tell the owner once when a
//! registry node goes stale or failing, and once when it recovers.
//!
//! WATCHING. Every gateway runs [`tick`] on the gossip cadence and keeps
//! the node health the Instances tab shows (`fleet_ops::node_health`):
//! `stale` (no heartbeat within `[cluster] stale_after_secs`), `failing`
//! (heartbeating, but a sync kind is in backoff or the last probe
//! failed) or `fresh`. A node's first sighting is its baseline, not an
//! alert — a gateway that boots next to a node that has been off for a
//! week says nothing. After that, a CHANGE is reported once it has held
//! for `[notify] fleet_alert_debounce_secs` (the per-node debounce): a
//! node that fails one sync and recovers on the next tick is not news.
//! The recent changes stay in this process (the last [`KEEP`]) and are
//! listed in `/api/fleet`, sent or not.
//!
//! SENDING. Through the owner's home chat, the destination the
//! `[notify] ask_relay` path puts questions in (askrelay.rs), and only
//! with `[notify] fleet_alerts` on. `[notify]` replicates, so in a fleet
//! the switch is on everywhere at once; to say each change once, only
//! the node whose bus runs the home chat's adapter sends — the lease
//! holder of `singleton:telegram`. The others record "not sent from this
//! node". The limit that follows is stated, not hidden: a change in the
//! node that RUNS the adapter (it went stale) cannot be sent by it; it
//! is sent only by a node the `singleton:telegram` lease has moved to by
//! the time that node's own debounce reports the change (the adapter is
//! checked per change, not at boot), and otherwise by nobody. The
//! Instances tab of any gateway still shows it. The bus scrubs
//! registered secrets and credential shapes from every outbound text
//! (bus.rs, F2); the detail line is scrubbed here as well, because it is
//! also what `/api/fleet` shows.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use veles_core::config::VelesConfig;
use veles_core::lockutil::LockOk;
use veles_core::store::Store;

/// How many recent changes `/api/fleet` lists.
pub const KEEP: usize = 50;
/// The platform whose home chat the ask relay uses (gateway.rs
/// `owner_home_chat`).
const HOME_PLATFORM: &str = "telegram";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub id: String,
    pub name: String,
    pub from: String,
    pub to: String,
    pub detail: String,
}

struct Watched {
    /// The state last reported (or the baseline).
    reported: String,
    /// A different state seen, and since when.
    pending: Option<(String, Instant)>,
}

/// Per-node state machines. Pure: the caller supplies the clock.
#[derive(Default)]
pub struct Tracker {
    nodes: HashMap<String, Watched>,
}

impl Tracker {
    /// Feed one tick's view: (id, name, state, detail) per node. Returns
    /// the changes that held for `debounce`. A node absent from `seen`
    /// (forgotten) is dropped; `self` rows are never watched.
    pub fn observe(
        &mut self,
        seen: &[(String, String, String, String)],
        now: Instant,
        debounce: Duration,
    ) -> Vec<Change> {
        let mut out = Vec::new();
        self.nodes
            .retain(|id, _| seen.iter().any(|(sid, ..)| sid == id));
        for (id, name, state, detail) in seen {
            if state == "self" {
                self.nodes.remove(id);
                continue;
            }
            let w = self.nodes.entry(id.clone()).or_insert_with(|| Watched {
                reported: state.clone(),
                pending: None,
            });
            if &w.reported == state {
                w.pending = None;
                continue;
            }
            if !matches!(&w.pending, Some((s, _)) if s == state) {
                w.pending = Some((state.clone(), now));
            }
            let since = w.pending.as_ref().map_or(now, |(_, at)| *at);
            if now.saturating_duration_since(since) >= debounce {
                out.push(Change {
                    id: id.clone(),
                    name: name.clone(),
                    from: std::mem::replace(&mut w.reported, state.clone()),
                    to: state.clone(),
                    detail: detail.clone(),
                });
                w.pending = None;
            }
        }
        out
    }
}

fn tracker() -> &'static Mutex<Tracker> {
    static T: OnceLock<Mutex<Tracker>> = OnceLock::new();
    T.get_or_init(Default::default)
}

fn recent_cell() -> &'static Mutex<VecDeque<Value>> {
    static R: OnceLock<Mutex<VecDeque<Value>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// The recent changes, newest first, as `/api/fleet` lists them.
pub fn recent() -> Vec<Value> {
    recent_cell().lock_ok().iter().rev().cloned().collect()
}

fn remember(entry: Value) {
    let mut r = recent_cell().lock_ok();
    if r.len() == KEEP {
        r.pop_front();
    }
    r.push_back(entry);
}

/// The owner-facing sentence for a change.
pub fn message(c: &Change) -> String {
    match c.to.as_str() {
        "stale" => veles_core::i18n!(
            "veles fleet: node %{node} is stale — %{detail}",
            node = c.name,
            detail = c.detail
        ),
        "failing" => veles_core::i18n!(
            "veles fleet: node %{node} is failing — %{detail}",
            node = c.name,
            detail = c.detail
        ),
        _ => veles_core::i18n!(
            "veles fleet: node %{node} recovered (it was %{was})",
            node = c.name,
            was = c.from
        ),
    }
}

/// Why a node is in its state, in one line, scrubbed. `health` is a
/// `fleet_ops::node_health` answer with the node's per-kind sync status
/// under `sync` (`/fleet list` words its rows with it too).
pub(crate) fn detail(health: &Value) -> String {
    let secrets = veles_core::secrets::scrubbable();
    let clean =
        |s: &str| veles_core::a2a::redact_outbound(&veles_core::secrets::scrub(s, &secrets));
    match health["state"].as_str().unwrap_or("") {
        "stale" => match health["heartbeat_age_secs"].as_i64() {
            Some(age) => {
                veles_core::i18n!("no heartbeat for %{mins} min", mins = (age / 60).max(1))
            }
            None => veles_core::i18n!("no heartbeat within the threshold"),
        },
        "failing" => {
            let mut parts = Vec::new();
            for kind in health["failing"].as_array().into_iter().flatten() {
                let k = kind.as_str().unwrap_or("?");
                let line = health["sync"][k]["last"]["line"].as_str().unwrap_or("");
                parts.push(if line.is_empty() {
                    format!("{k} sync")
                } else {
                    format!("{k} sync: {}", clean(line))
                });
            }
            if health["unreachable"] == json!(true) {
                parts.push(format!(
                    "probe: {}",
                    clean(health["probe"]["line"].as_str().unwrap_or(""))
                ));
            }
            parts.join("; ").chars().take(300).collect()
        }
        _ => String::new(),
    }
}

/// Where an alert raised in this process goes.
enum Route<'a> {
    /// To this chat, through this process's adapter.
    Here(&'a veles_core::bus::GatewayBus, String),
    /// The home chat's adapter runs on another node, which sends its own
    /// copy of the change.
    Elsewhere,
    /// Nowhere, and why.
    No(&'static str),
}

fn route<'a>(cfg: &VelesConfig, bus: Option<&'a veles_core::bus::GatewayBus>) -> Route<'a> {
    if !cfg.notify.fleet_alerts {
        return Route::No("[notify] fleet_alerts is off");
    }
    if !cfg.notify.ask_relay {
        return Route::No("[notify] ask_relay is off");
    }
    let Some(chat) = veles_core::home::chat(HOME_PLATFORM) else {
        return Route::No("no [platforms.telegram.home_channel] chat_id");
    };
    let Some(bus) = bus else {
        return Route::No("no gateway bus in this process");
    };
    if !bus.has_adapter(HOME_PLATFORM) {
        return Route::Elsewhere;
    }
    Route::Here(bus, chat)
}

/// Where this process's alerts go, without sending one, for
/// `/api/fleet` `alerts.delivery`: `sends` is `here`, `elsewhere` (the
/// node running the home chat's adapter sends them) or `no`, with the
/// `reason`. The Instances tab used to say "sent to the owner's home
/// chat" whenever `[notify] fleet_alerts` was on (FLEET-8).
pub fn delivery(cfg: &VelesConfig, bus: Option<&veles_core::bus::GatewayBus>) -> Value {
    match route(cfg, bus) {
        Route::Here(..) => json!({"sends": "here", "reason": ""}),
        Route::Elsewhere => json!({
            "sends": "elsewhere",
            "reason": "the telegram adapter runs on another node",
        }),
        Route::No(why) => json!({"sends": "no", "reason": why}),
    }
}

/// Send `text` to the owner's home chat if this node may: (sent, note).
pub fn deliver(
    cfg: &VelesConfig,
    bus: Option<&veles_core::bus::GatewayBus>,
    text: &str,
) -> (bool, String) {
    let (bus, chat) = match route(cfg, bus) {
        Route::Here(bus, chat) => (bus, chat),
        Route::Elsewhere => return (
            false,
            "not sent from this node: the telegram adapter runs on another node, which sends it"
                .into(),
        ),
        Route::No(why) => return (false, format!("not sent: {why}")),
    };
    let sent = bus.send_outbound(
        HOME_PLATFORM,
        veles_core::bus::OutboundMessage::plain(
            &chat,
            text.to_string(),
            veles_core::bus::MessageKind::Reply,
        ),
    );
    if sent {
        (true, format!("sent to {HOME_PLATFORM}:{chat}"))
    } else {
        (
            false,
            "not sent: the telegram adapter did not take it".into(),
        )
    }
}

/// One watch pass: read the registry, feed the tracker, record and send
/// what changed. Returns the changes.
pub fn tick(
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    bus: Option<&veles_core::bus::GatewayBus>,
    now: Instant,
) -> Vec<Change> {
    tick_with(tracker(), store, cfg, bus, now)
}

/// [`tick`] with the state machines passed in. The process has one
/// tracker; a test that shared it raced every other test that ticks,
/// because a pass forgets the nodes its own registry does not list.
fn tick_with(
    tracker: &Mutex<Tracker>,
    store: &Arc<Mutex<Store>>,
    cfg: &VelesConfig,
    bus: Option<&veles_core::bus::GatewayBus>,
    now: Instant,
) -> Vec<Change> {
    let nodes = match veles_core::store::lock_timed(store, "fleet_watch::tick").nodes_list() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("fleet alerts: registry read failed: {e:#}");
            return Vec::new();
        }
    };
    let me = veles_core::node::NodeIdentity::load(&veles_core::config::config_dir())
        .ok()
        .flatten()
        .map(|i| i.id);
    let seen: Vec<(String, String, String, String)> = nodes
        .iter()
        .map(|n| {
            let h = crate::fleet_ops::node_health_light(
                cfg,
                n,
                me.as_deref() == Some(n.id.as_str()),
                now,
            );
            (
                n.id.clone(),
                n.name.clone(),
                h["state"].as_str().unwrap_or("fresh").to_string(),
                detail(&h),
            )
        })
        .collect();
    let debounce = Duration::from_secs(cfg.notify.fleet_alert_debounce_secs);
    let changes = tracker.lock_ok().observe(&seen, now, debounce);
    for c in &changes {
        let text = message(c);
        let (sent, note) = deliver(cfg, bus, &text);
        if sent {
            tracing::info!("fleet alert sent: {text}");
        } else {
            tracing::info!("fleet alert ({note}): {text}");
        }
        remember(json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "node": c.id,
            "name": c.name,
            "from": c.from,
            "to": c.to,
            "detail": c.detail,
            "text": text,
            "sent": sent,
            "note": note,
        }));
    }
    changes
}

/// The watcher's loop, on the gossip cadence (at least every 5 s). The
/// config is read live each pass, so `[notify] fleet_alerts` and the
/// debounce take effect without a restart.
pub async fn run(store: Arc<Mutex<Store>>, bus: veles_core::bus::GatewayBus, interval: u64) {
    let mut every = tokio::time::interval(Duration::from_secs(interval.max(5)));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        let Ok(cfg) = veles_core::config::load() else {
            continue;
        };
        let (s, b) = (store.clone(), bus.clone());
        let dirs = veles_core::config::dirs();
        let _ = veles_core::rt::spawn_blocking(move || {
            let _dirs = veles_core::config::set_dirs(dirs);
            tick(&s, &cfg, Some(&b), Instant::now())
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(state: &str) -> Vec<(String, String, String, String)> {
        vec![(
            "id-vps".into(),
            "vps".into(),
            state.into(),
            "no heartbeat for 9 min".into(),
        )]
    }

    /// The state machine: the first sighting is a baseline; a change is
    /// reported once, after it held for the debounce; a flap inside the
    /// debounce is never reported; recovery is its own change; a
    /// forgotten node and this node's own row are not watched.
    #[test]
    fn a_change_is_reported_once_after_it_holds() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let d = Duration::from_secs(120);
        assert!(t.observe(&seen("fresh"), t0, d).is_empty(), "baseline");
        assert!(t
            .observe(&seen("failing"), t0 + Duration::from_secs(30), d)
            .is_empty());
        assert!(
            t.observe(&seen("fresh"), t0 + Duration::from_secs(60), d)
                .is_empty(),
            "a flap inside the debounce"
        );
        assert!(t
            .observe(&seen("stale"), t0 + Duration::from_secs(90), d)
            .is_empty());
        let c = t.observe(&seen("stale"), t0 + Duration::from_secs(211), d);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!((c[0].from.as_str(), c[0].to.as_str()), ("fresh", "stale"));
        assert!(
            t.observe(&seen("stale"), t0 + Duration::from_secs(400), d)
                .is_empty(),
            "once per change"
        );
        assert!(t
            .observe(&seen("fresh"), t0 + Duration::from_secs(410), d)
            .is_empty());
        let r = t.observe(&seen("fresh"), t0 + Duration::from_secs(530), d);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].from.as_str(), r[0].to.as_str()), ("stale", "fresh"));
        // Zero debounce: reported on the tick it is seen.
        let z = t.observe(
            &seen("failing"),
            t0 + Duration::from_secs(600),
            Duration::ZERO,
        );
        assert_eq!(z.len(), 1);
        // Forgotten: dropped, and a new sighting is a baseline again.
        assert!(t.observe(&[], t0 + Duration::from_secs(700), d).is_empty());
        assert!(t
            .observe(&seen("stale"), t0 + Duration::from_secs(710), d)
            .is_empty());
        let mut me = seen("self");
        me[0].0 = "id-me".into();
        assert!(t
            .observe(&me, t0 + Duration::from_secs(720), Duration::ZERO)
            .is_empty());
    }

    fn bus(
        home: &std::path::Path,
    ) -> (
        veles_core::bus::GatewayBus,
        tokio::sync::mpsc::UnboundedReceiver<veles_core::bus::OutboundMessage>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (itx, _irx) = tokio::sync::mpsc::unbounded_channel();
        let bus = veles_core::bus::GatewayBus {
            inbound: itx,
            outbound: Default::default(),
            last_chat: Default::default(),
            state_path: home.join("gateway-state.json"),
        };
        bus.register_outbound(HOME_PLATFORM, tx);
        (bus, rx)
    }

    /// Who sends: only with the switch and the relay on, a home chat
    /// named, and the home chat's adapter running in THIS process.
    #[tokio::test]
    async fn an_alert_goes_to_the_home_chat_only_from_the_adapters_node() {
        let (home, _env) = veles_core::testenv::scratch_home();
        let mut cfg = VelesConfig::default();
        let (with_adapter, mut rx) = bus(home.path());
        let (none_tx, _none_rx) = tokio::sync::mpsc::unbounded_channel();
        let without = veles_core::bus::GatewayBus {
            inbound: none_tx,
            outbound: Default::default(),
            last_chat: Default::default(),
            state_path: home.path().join("other-state.json"),
        };
        let say = |c: &VelesConfig, b: &veles_core::bus::GatewayBus| deliver(c, Some(b), "x");
        // FLEET-8: `/api/fleet` says the same without sending.
        let goes = |c: &VelesConfig, b: Option<&veles_core::bus::GatewayBus>| {
            let d = delivery(c, b);
            (
                d["sends"].as_str().unwrap_or("").to_string(),
                d["reason"].as_str().unwrap_or("").to_string(),
            )
        };
        assert!(say(&cfg, &with_adapter).1.contains("fleet_alerts is off"));
        assert_eq!(
            goes(&cfg, Some(&with_adapter)),
            ("no".into(), "[notify] fleet_alerts is off".into())
        );
        cfg.notify.fleet_alerts = true;
        assert!(say(&cfg, &with_adapter).1.contains("ask_relay is off"));
        assert_eq!(
            goes(&cfg, Some(&with_adapter)),
            ("no".into(), "[notify] ask_relay is off".into())
        );
        cfg.notify.ask_relay = true;
        veles_core::home::clear_all();
        assert!(say(&cfg, &with_adapter).1.contains("home_channel"));
        assert!(goes(&cfg, Some(&with_adapter)).1.contains("home_channel"));
        veles_core::home::set(HOME_PLATFORM, "-5000");
        assert_eq!(goes(&cfg, None).0, "no", "no bus in this process");
        assert_eq!(goes(&cfg, Some(&without)).0, "elsewhere");
        assert_eq!(goes(&cfg, Some(&with_adapter)).0, "here");
        let (sent, note) = say(&cfg, &without);
        assert!(!sent && note.contains("another node"), "{note}");
        let (sent, note) = say(&cfg, &with_adapter);
        assert!(sent, "{note}");
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.chat_id, "-5000");
        assert_eq!(got.text, "x");
        veles_core::home::clear_all();
    }

    /// End to end through the registry: a node that stops heartbeating
    /// is reported stale once, through the adapter, and listed.
    #[tokio::test]
    async fn a_node_going_stale_is_sent_once_and_listed() {
        let (home, _env) = veles_core::testenv::scratch_home();
        let store = Store::open(&veles_core::config::veles_home().join("state.db")).unwrap();
        store
            .node_register(
                "id-watch-vps",
                "watched-vps",
                "h",
                "0.2.0",
                "always-on",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        let mut cfg = VelesConfig::default();
        cfg.notify.fleet_alerts = true;
        cfg.notify.ask_relay = true;
        cfg.notify.fleet_alert_debounce_secs = 0;
        veles_core::home::set(HOME_PLATFORM, "-77");
        let (bus, mut rx) = bus(home.path());
        let t0 = Instant::now();
        assert!(tick(&store, &cfg, Some(&bus), t0).is_empty(), "baseline");
        store
            .lock_ok()
            .conn()
            .execute(
                "UPDATE nodes SET heartbeat_at = '2000-01-01 00:00:00' WHERE id = 'id-watch-vps'",
                [],
            )
            .unwrap();
        let c = tick(&store, &cfg, Some(&bus), t0 + Duration::from_secs(1));
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].to, "stale");
        assert!(tick(&store, &cfg, Some(&bus), t0 + Duration::from_secs(2)).is_empty());
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            got.text.contains("watched-vps") && got.text.contains("stale"),
            "{}",
            got.text
        );
        let listed = recent();
        assert!(
            listed
                .iter()
                .any(|a| a["node"] == "id-watch-vps" && a["to"] == "stale" && a["sent"] == true),
            "{listed:?}"
        );
        veles_core::home::clear_all();
    }

    /// FLEET-3: a failed manual probe is reported as failing, and the
    /// next good gossip exchange with the node is reported as its
    /// recovery. The probe verdict used to stand until the next probe, so
    /// the "failing" alert was never followed by a "recovered" one.
    #[test]
    fn a_failed_probe_is_followed_by_a_recovery_alert() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let store = Store::open(&veles_core::config::veles_home().join("state.db")).unwrap();
        store
            .node_register(
                "id-watch-probed",
                "watched-probed",
                "h",
                "0.2.0",
                "always-on",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        let mut cfg = VelesConfig::default();
        cfg.cluster.gossip_peers = vec!["watched-probed".into()];
        cfg.notify.fleet_alert_debounce_secs = 0;
        let own = Mutex::new(Tracker::default());
        let tick = |now: Instant| tick_with(&own, &store, &cfg, None, now);
        let t0 = Instant::now();
        let mine = |c: Vec<Change>| -> Vec<(String, String)> {
            c.into_iter()
                .filter(|c| c.id == "id-watch-probed")
                .map(|c| (c.from, c.to))
                .collect()
        };
        assert!(mine(tick(t0)).is_empty(), "baseline");
        crate::fleet_ops::record("probe", "id-watch-probed", Err("connection refused"));
        assert_eq!(
            mine(tick(t0 + Duration::from_secs(1))),
            vec![("fresh".to_string(), "failing".to_string())]
        );
        std::thread::sleep(Duration::from_millis(5));
        crate::fleet_ops::record("gossip", "watched-probed", Ok("merged 2 row(s)"));
        assert_eq!(
            mine(tick(t0 + Duration::from_secs(2))),
            vec![("failing".to_string(), "fresh".to_string())],
            "the next good exchange is the recovery"
        );
        assert!(recent()
            .iter()
            .any(|a| a["node"] == "id-watch-probed" && a["to"] == "fresh"));
    }
}
