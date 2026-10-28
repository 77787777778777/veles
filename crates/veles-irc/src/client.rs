//! IRC client + gateway adapter on top of the `irc` crate.
//!
//! We drive the IRCv3 negotiation ourselves (the crate only provides
//! send-side primitives — no auto CAP/SASL/PONG): CAP LS → REQ → ACK →
//! SASL PLAIN or EXTERNAL (CertFP) → CAP END → registration, then the
//! event loop. slopdrop's run-loop shape (single select! over stream +
//! command channels) is the proven pattern, and its abuse lessons are
//! built in: ISUPPORT-driven line limits, outbound token bucket,
//! inbound per-source sliding-window rate limits, CRLF/NUL stripping.

use crate::format;
use crate::ratelimit::{InboundLimiter, ServerLag, TokenBucket};
use crate::{FormatMode, IrcConfig};
use anyhow::Result;
use base64::Engine;
use futures::StreamExt;
use irc::client::prelude::*;
use irc::proto::command::CapSubCommand;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn, Instrument as _};
use veles_frontends::gateway::{GatewayBus, InboundMessage, MessageKind, OutboundMessage};

type ParkedStore = std::sync::Arc<std::sync::Mutex<veles_core::store::Store>>;

/// The gateway's store and this network's name, parked process-wide by
/// `veles-cli` before the adapter runs. The IRC scrollback SEARCH (the
/// `irc history` tool action and `.agent history`) reads through this:
/// the adapter must not import the gateway (the dependency runs the
/// other way), and a parked Arc is the same seam
/// `acl::set_runtime_pairing_store` uses.
/// One entry per configured network, plus whichever was parked first
/// as the default.
///
/// A MAP, not a `OnceLock`. `veles-cli` parks in a per-network loop,
/// and `OnceLock::set` silently succeeds once and silently does
/// nothing afterwards — so on a two-network gateway the second
/// network's store was dropped on the floor and every history search
/// there answered out of the FIRST network's name. The default is
/// still the first one parked, but it is now only the answer when the
/// caller genuinely has no network of its own.
#[derive(Default)]
struct Parked {
    by_network: HashMap<String, ParkedStore>,
    default_network: Option<String>,
}

impl Parked {
    fn park(&mut self, store: ParkedStore, network: &str) {
        self.by_network.insert(network.to_lowercase(), store);
        if self.default_network.is_none() {
            self.default_network = Some(network.to_string());
        }
    }

    fn get(&self, network: Option<&str>) -> Option<ParkedStore> {
        let key = match network {
            Some(n) => n.to_lowercase(),
            None => self.default_network.as_ref()?.to_lowercase(),
        };
        self.by_network.get(&key).cloned()
    }

    /// Take one network's store out (R4 module teardown: a reloaded
    /// instance re-parks its own; a stopped one leaves nothing behind).
    /// The default moves to another parked network if its store went.
    fn unpark(&mut self, network: &str) -> Option<ParkedStore> {
        let out = self.by_network.remove(&network.to_lowercase());
        if self.default_network.as_deref() == Some(network) && !self.by_network.is_empty() {
            // The FIRST remaining, alphabetically (the map has no
            // registration order; "first configured" was a load order
            // the parked map never tracked).
            let next = self.by_network.keys().min().cloned();
            self.default_network = next;
        }
        if self.by_network.is_empty() {
            self.default_network = None;
        }
        out
    }

    fn networks(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_network.keys().cloned().collect();
        v.sort();
        v
    }
}

fn parked() -> &'static std::sync::Mutex<Parked> {
    static P: std::sync::OnceLock<std::sync::Mutex<Parked>> = std::sync::OnceLock::new();
    P.get_or_init(|| std::sync::Mutex::new(Parked::default()))
}

/// Park the gateway store under the network name it belongs to, for
/// the history search. Called per configured network — by `veles-cli`
/// per network when linked (pre-R4), by the module's own `start` now.
pub fn park_store(store: ParkedStore, network: &str) {
    if let Ok(mut p) = parked().lock() {
        p.park(store, network);
    }
}

/// Un-park one network's store (R4): the module's `stop` calls it so a
/// torn-down instance leaves no store entry behind (a reloaded instance
/// re-parks its own in `start`).
pub fn unpark_store(network: &str) {
    if let Ok(mut p) = parked().lock() {
        p.unpark(network);
    }
}

/// The store for one network — or the default when the caller names
/// none. None before `park_store`, and None for a network that was
/// never parked (which is an answer, not a silent wrong answer).
pub fn parked_store_for(network: Option<&str>) -> Option<ParkedStore> {
    parked().lock().ok()?.get(network)
}

/// The parked store, for history searches. None before `park_store`.
pub fn parked_store() -> Option<ParkedStore> {
    parked_store_for(None)
}

/// The parked default network name.
pub fn parked_network() -> Option<String> {
    parked().lock().ok()?.default_network.clone()
}

/// Every network that has a parked store, sorted. What the history
/// surfaces name when the caller asks for a network nobody parked.
pub fn parked_networks() -> Vec<String> {
    match parked().lock() {
        Ok(p) => p.networks(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod parked_tests {
    use super::*;

    #[test]
    fn every_parked_network_keeps_its_own_store() {
        // `OnceLock::set` succeeds once and then silently no-ops, and
        // veles-cli parks in a per-network loop: on a two-network
        // gateway the second network's store was dropped and every
        // history search there answered out of the FIRST network's
        // name. Tested on a local `Parked` rather than the process-wide
        // one — the global is shared with every other test in this
        // binary, and a test that races for it proves nothing.
        let dir = tempfile::tempdir().expect("tempdir");
        let mk = |n: &str| {
            std::sync::Arc::new(std::sync::Mutex::new(
                veles_core::store::Store::open(&dir.path().join(format!("{n}.db"))).expect("store"),
            ))
        };
        let mut p = Parked::default();
        p.park(mk("first"), "FirstNet");
        p.park(mk("second"), "secondnet");
        assert!(
            p.get(Some("firstnet")).is_some(),
            "the first network is reachable, case-insensitively"
        );
        assert!(
            p.get(Some("SecondNet")).is_some(),
            "and so is the second — the bug was that it was not"
        );
        assert!(
            p.get(Some("neverparked")).is_none(),
            "an unknown network is an answer, not the default's data"
        );
        assert_eq!(
            p.default_network.as_deref(),
            Some("FirstNet"),
            "the default is the first one parked"
        );
        assert!(p.get(None).is_some(), "and it resolves to a store");
        assert_eq!(
            p.networks(),
            vec!["firstnet".to_string(), "secondnet".to_string()]
        );
        assert!(
            Parked::default().get(None).is_none(),
            "nothing parked is None, not a panic"
        );
    }
}

/// Capabilities we request when offered (all exist in irc-proto's enum;
/// message-tags is implicit with CAP LS 302 and parsed by the crate).
/// Capability isn't Clone, so we rebuild from names at REQ time.
const WANTED_CAP_NAMES: &[&str] = &[
    "multi-prefix",
    "sasl",
    "account-tag",
    "account-notify",
    "extended-join",
    "server-time",
    "userhost-in-names",
    "echo-message",
    "typing", // +typing TAGMSG notifications (in and out)
    // chathistory needs batch + server-time to be useful: the replayed
    // lines arrive inside a BATCH and carry their original timestamps.
    "batch",
    "chathistory",
];

fn cap_by_name(name: &str) -> Option<Capability> {
    match name {
        "multi-prefix" => Some(Capability::MultiPrefix),
        "sasl" => Some(Capability::Sasl),
        "account-tag" => Some(Capability::AccountTag),
        "account-notify" => Some(Capability::AccountNotify),
        "extended-join" => Some(Capability::ExtendedJoin),
        "server-time" => Some(Capability::ServerTime),
        "userhost-in-names" => Some(Capability::UserhostInNames),
        "echo-message" => Some(Capability::EchoMessage),
        "typing" => Some(Capability::Custom("typing")),
        "batch" => Some(Capability::Batch),
        "chathistory" => Some(Capability::Custom("chathistory")),
        _ => None,
    }
}

// ── command channel (tools + internal callers → adapter loop) ────────

/// Commands the `irc` tool layer sends to the running adapter loop.
#[derive(Debug)]
pub enum IrcCommand {
    Say {
        target: String,
        text: String,
        mode: Option<FormatMode>,
        reply: oneshot::Sender<String>,
    },
    Notice {
        target: String,
        text: String,
        reply: oneshot::Sender<String>,
    },
    Join {
        channel: String,
        key: Option<String>,
        reply: oneshot::Sender<String>,
    },
    Part {
        channel: String,
        reason: Option<String>,
        reply: oneshot::Sender<String>,
    },
    Topic {
        channel: String,
        topic: Option<String>,
        /// Buffer the QUERY answer publishes to (default: the channel).
        scope: Option<String>,
        reply: oneshot::Sender<String>,
    },
    Kick {
        channel: String,
        nick: String,
        reason: Option<String>,
        reply: oneshot::Sender<String>,
    },
    /// Channel introspection: roster + topic + modes (ircx parity).
    ChannelInfo {
        channel: String,
        /// Buffer the answer publishes to (default: the channel).
        scope: Option<String>,
        reply: oneshot::Sender<String>,
    },
    /// Ban list query: sends MODE <channel> +b, answer arrives as
    /// 367/368 and is delivered to the reply at ENDOFBANLIST.
    Bans {
        channel: String,
        /// Buffer the answer publishes to (default: the channel).
        scope: Option<String>,
        reply: oneshot::Sender<String>,
    },
    /// Roster dump (`/names`): answered from the live RPL_NAMREPLY
    /// member list, no wire query.
    Names {
        channel: String,
        /// Buffer the answer publishes to (default: the channel).
        scope: Option<String>,
        reply: oneshot::Sender<String>,
    },
    /// WHO query (`/who [mask]`): rows arrive at RPL_WHOREPLY (352),
    /// the answer resolves at RPL_ENDOFWHO (315).
    Who {
        target: Option<String>,
        /// Buffer the answer publishes to (default: the server buffer).
        scope: Option<String>,
        reply: oneshot::Sender<String>,
    },
    /// Outbound typing indicator (typing cap). `active` while the human
    /// is composing; `done` on send or after the idle timeout.
    Typing {
        target: String,
        state: String,
    },
    Mode {
        target: String,
        modes: String,
        reply: oneshot::Sender<String>,
    },
    Nick {
        nick: String,
        reply: oneshot::Sender<String>,
    },
    List {
        reply: oneshot::Sender<Vec<String>>,
    },
    Whois {
        nick: String,
        /// Buffer the synthesized answer publishes to (default: server).
        scope: Option<String>,
        reply: oneshot::Sender<String>,
    },
    /// The human's `/away` — set (or unset) away status. `message` is
    /// the away text; `None` means come back.
    Away {
        message: Option<String>,
        /// Buffer the confirmation publishes to (default: server).
        scope: Option<String>,
        reply: oneshot::Sender<String>,
    },
    Raw {
        line: String,
        reply: oneshot::Sender<String>,
    },
    /// End the connection with a QUIT (R4): the module's `stop` sends
    /// it, the connection's command arm writes `QUIT :<reason>` and
    /// returns so the reconnect loop sees the close. The reply is sent
    /// AFTER the write so `stop` can wait for the parting line to be
    /// out before teardown drops the connection task — a Quit that is
    /// only queued in the channel is lost when the task is dropped.
    Quit {
        reason: String,
        /// Answered once the line is written (or the write failed).
        done: oneshot::Sender<()>,
    },
}

/// Who a WHOIS says someone IS: the numerics between the 311 handshake
/// and the 318 terminator. Every field optional — servers send subsets —
/// and composed once at 318 into the answer.
#[derive(Default, Clone)]
struct WhoExtra {
    realname: Option<String>,
    oper: bool,
    idle: Option<String>,
    signon: Option<String>,
    away: Option<String>,
    channels: Option<String>,
}

/// Handle shared with the tools layer: cheap clone, command channel to
/// the adapter loop.
#[derive(Clone)]
pub struct IrcHandle {
    tx: mpsc::UnboundedSender<IrcCommand>,
    /// Which network this handle speaks to. The tool layer needs it by
    /// name — `irc wait` watches the tap, which is keyed on
    /// network/buffer — and "whichever adapter booted first" is the
    /// wrong answer on a multi-network gateway.
    network: String,
}

impl IrcHandle {
    /// The network this handle belongs to.
    pub fn network(&self) -> &str {
        &self.network
    }

    /// A handle for the tool tests: no adapter behind it, so anything
    /// that actually sends fails honestly, while the parts that only
    /// need the network name (`irc wait`) can be tested without
    /// standing up a connection.
    #[cfg(test)]
    pub fn for_test(tx: mpsc::UnboundedSender<IrcCommand>, network: &str) -> Self {
        IrcHandle {
            tx,
            network: network.to_string(),
        }
    }

    fn send(&self, cmd: IrcCommand) -> Result<(), String> {
        self.tx
            .send(cmd)
            .map_err(|_| "irc adapter not running".to_string())
    }

    /// Send QUIT and end the connection (R4): the module's `stop` calls
    /// it so a reload parts cleanly instead of timing out. Async: it
    /// waits (bounded) for the connection loop to pick the command up
    /// and write the line — teardown drops the task right after `stop`,
    /// so a Quit that is only QUEUED would never reach the wire.
    ///
    /// `true` once the connection has written the line (and the server
    /// closed, or the flush's bound passed); `false` when there is no
    /// adapter, or no connection picked the command up in time (between
    /// connections, or wedged).
    pub async fn quit(&self, reason: &str) -> bool {
        let (done, rx) = oneshot::channel();
        if self
            .send(IrcCommand::Quit {
                reason: reason.to_string(),
                done,
            })
            .is_err()
        {
            return false;
        }
        // The write is quick or never: do not hold teardown for a wedged
        // connection.
        matches!(
            tokio::time::timeout(Duration::from_secs(2), rx).await,
            Ok(Ok(()))
        )
    }

    /// Fire-and-forget say (gateway delivery path).
    pub fn say(&self, target: &str, text: &str, mode: Option<FormatMode>) -> String {
        let (tx, _rx) = oneshot::channel(); // rx dropped → reply ignored
        match self.send(IrcCommand::Say {
            target: target.into(),
            text: text.into(),
            mode,
            reply: tx,
        }) {
            Ok(()) => "queued".into(),
            Err(e) => e,
        }
    }

    /// Request/response helper for tools. ASYNC — the handle is called
    /// from the agent's tokio runtime, where blocking_recv panics
    /// ("Cannot block the current thread from within a runtime"; hit live
    /// 2026-08-10 on the first tool call). Timeout so a dead adapter
    /// can't wedge the agent turn forever.
    async fn request(&self, make: impl FnOnce(oneshot::Sender<String>) -> IrcCommand) -> String {
        let (tx, rx) = oneshot::channel();
        if let Err(e) = self.send(make(tx)) {
            return e;
        }
        match tokio::time::timeout(Duration::from_secs(20), rx).await {
            Ok(Ok(s)) => s,
            Ok(Err(_)) => "irc adapter dropped the request".into(),
            Err(_) => "irc request timed out".into(),
        }
    }

    pub async fn notice(&self, target: &str, text: &str) -> String {
        let t = target.to_string();
        let x = text.to_string();
        self.request(|reply| IrcCommand::Notice {
            target: t,
            text: x,
            reply,
        })
        .await
    }

    pub async fn join(&self, channel: &str, key: Option<String>) -> String {
        let c = channel.to_string();
        self.request(|reply| IrcCommand::Join {
            channel: c,
            key,
            reply,
        })
        .await
    }

    pub async fn part(&self, channel: &str, reason: Option<String>) -> String {
        let c = channel.to_string();
        self.request(|reply| IrcCommand::Part {
            channel: c,
            reason,
            reply,
        })
        .await
    }

    pub async fn topic(&self, channel: &str, topic: Option<String>) -> String {
        let c = channel.to_string();
        self.request(|reply| IrcCommand::Topic {
            channel: c,
            topic,
            scope: None,
            reply,
        })
        .await
    }

    pub async fn kick(&self, channel: &str, nick: &str, reason: Option<String>) -> String {
        let (c, n) = (channel.to_string(), nick.to_string());
        self.request(|reply| IrcCommand::Kick {
            channel: c,
            nick: n,
            reason,
            reply,
        })
        .await
    }

    pub async fn mode(&self, target: &str, modes: &str) -> String {
        let (t, m) = (target.to_string(), modes.to_string());
        self.request(|reply| IrcCommand::Mode {
            target: t,
            modes: m,
            reply,
        })
        .await
    }

    pub async fn nick(&self, nick: &str) -> String {
        let n = nick.to_string();
        self.request(|reply| IrcCommand::Nick { nick: n, reply })
            .await
    }

    pub async fn whois(&self, nick: &str) -> String {
        let n = nick.to_string();
        self.request(|reply| IrcCommand::Whois {
            nick: n,
            scope: None,
            reply,
        })
        .await
    }

    pub async fn channel_info(&self, channel: &str) -> String {
        let c = channel.to_string();
        self.request(|reply| IrcCommand::ChannelInfo {
            channel: c,
            scope: None,
            reply,
        })
        .await
    }

    pub async fn raw(&self, line: &str) -> String {
        let l = line.to_string();
        self.request(|reply| IrcCommand::Raw { line: l, reply })
            .await
    }

    pub async fn list_channels(&self) -> Vec<String> {
        let (tx, rx) = oneshot::channel();
        if self.send(IrcCommand::List { reply: tx }).is_err() {
            return Vec::new();
        }
        match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(v)) => v,
            _ => Vec::new(),
        }
    }
}

/// Handles keyed by `(network, presence)`. Was keyed by presence alone —
/// one connection per name — which a second network could not use at
/// all: two networks each have their own "bot" connection.
static HANDLES: std::sync::OnceLock<std::sync::Mutex<HashMap<(String, String), IrcHandle>>> =
    std::sync::OnceLock::new();

/// Registration order, so `handle()` can return the FIRST bot. A
/// `HashMap` has no order; "first in configured order" is the order the
/// adapters registered in, which follows the config file.
static HANDLE_ORDER: std::sync::OnceLock<std::sync::Mutex<Vec<(String, String)>>> =
    std::sync::OnceLock::new();

fn handles() -> &'static std::sync::Mutex<HashMap<(String, String), IrcHandle>> {
    HANDLES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn handle_order() -> &'static std::sync::Mutex<Vec<(String, String)>> {
    HANDLE_ORDER.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

pub fn register_handle(network: &str, presence: &str, h: IrcHandle) {
    let key = (network.to_string(), presence.to_string());
    if let Ok(mut m) = handles().lock() {
        m.insert(key.clone(), h);
    }
    if let Ok(mut o) = handle_order().lock() {
        if !o.contains(&key) {
            o.push(key);
        }
    }
}

/// Take one (network, presence)'s handle off the registry (R4 module
/// teardown): a stopped instance's handle must not answer after it —
/// the module's `stop` calls this, and `purge_owner_handles` covers a
/// teardown where `stop` did not run (the deadline passed).
pub fn unregister_handle(network: &str, presence: &str) {
    let key = (network.to_string(), presence.to_string());
    if let Ok(mut m) = handles().lock() {
        m.remove(&key);
    }
    if let Ok(mut o) = handle_order().lock() {
        o.retain(|k| k != &key);
    }
}

/// The presence name this module's bot runs as. Both `stop` and the
/// purge key on it (`client::run_presence_loop` registers under it).
pub fn bot_presence() -> &'static str {
    crate::tap::PRESENCE_BOT
}

/// QUIT one (network, presence)'s connection, with `reason` as the
/// parting line: the module's `stop` sends it so the network sees a
/// clean close rather than a ping timeout at the next reload. Async:
/// the quit waits (bounded) for its line to be written, because
/// teardown drops the connection task right after `stop` returns — a
/// Quit only queued in the channel would never reach the wire. Scoped
/// to one network: `stop` quits only its own, never the whole module.
/// `IrcHandle::quit` says what the `bool` means.
pub async fn quit_network(network: &str, presence: &str, reason: &str) -> bool {
    match handle_for(network, presence) {
        Some(h) => h.quit(reason).await,
        None => false,
    }
}

pub fn handle_for(network: &str, presence: &str) -> Option<IrcHandle> {
    handles()
        .lock()
        .ok()?
        .get(&(network.to_string(), presence.to_string()))
        .cloned()
}

/// The tool layer's access point: always the BOT's connection, and when
/// there is more than one network, the FIRST bot in configured order.
/// The model can pick a specific network by naming it in the tool call.
///
/// The model drives the bot, never the human's session — an agent that
/// could speak as the admin would be able to impersonate them to
/// everyone in the channel, and `irc` is a tool the model calls on its
/// own initiative.
pub fn handle() -> Option<IrcHandle> {
    let order = handle_order().lock().ok()?;
    for (network, presence) in order.iter() {
        if presence == crate::tap::PRESENCE_BOT {
            if let Some(h) = handle_for(network, presence) {
                return Some(h);
            }
        }
    }
    None
}

// ── connection state ─────────────────────────────────────────────────

/// ISUPPORT-driven server limits (slopdrop's ServerLimits, ported).
#[derive(Debug, Clone, Default)]
struct ServerLimits {
    msglen: Option<usize>,
    nicklen: Option<usize>,
}

impl ServerLimits {
    fn parse_isupport(&mut self, args: &[String]) {
        for arg in args.iter().skip(1) {
            if arg.contains("are supported") {
                break;
            }
            if let Some((key, value)) = arg.split_once('=') {
                match key.to_ascii_uppercase().as_str() {
                    "MSGLEN" => {
                        self.msglen = value.parse().ok();
                    }
                    "NICKLEN" | "MAXNICKLEN" => {
                        self.nicklen = value.parse().ok();
                    }
                    _ => {}
                }
            }
        }
    }

    /// Max trailing bytes for a PRIVMSG to `target` (slopdrop's
    /// calculation: MSGLEN if advertised, else 512 − prefix overhead,
    /// clamped to 100..480).
    fn max_message_len(&self, target: &str, hostmask: Option<&str>, nicklen: usize) -> usize {
        if let Some(msglen) = self.msglen {
            return msglen;
        }
        const IRC_PROTOCOL_MAX: usize = 512;
        let prefix_len = match hostmask {
            Some(h) => 1 + h.len(),
            None => 1 + nicklen + 1 + 10 + 1 + 63, // worst case nick!user@host
        };
        let overhead = prefix_len + 9 /* " PRIVMSG " */ + target.len() + 1 + 3; // ":\r\n"
        IRC_PROTOCOL_MAX.saturating_sub(overhead).clamp(100, 480)
    }
}

// ── ceilings on everything a SERVER can make us accumulate ───────────
//
// Class: WeeChat WSA-2026-3, "missing limit on unterminated IRC
// messages causes memory exhaustion" — present in every version from
// 0.3.3 to 4.9.1, which is to say for thirteen years. The general shape
// is that a client keeps per-nick and per-channel state, the server
// decides how many nicks and channels there are, and nobody wrote down
// a number.
//
// veles had five of these: `rosters` grew on every RPL_NAMREPLY without
// ever deduplicating (so a bouncer replaying NAMES on netjoin doubled
// it), and `accounts`, `topics`, `away` and `whois_cache` each gained an
// entry for any nick or channel the server chose to mention, whether we
// had ever joined it or not.
//
// The numbers are far above real use — the largest channels on the
// largest networks run a few thousand — so hitting one means something
// is wrong, not that somebody is popular.
//
/// Members tracked per channel.
const MAX_ROSTER_MEMBERS: usize = 5_000;
/// Channels we hold a topic or roster for.
const MAX_TRACKED_CHANNELS: usize = 1_024;
/// Nicks we hold an account, away flag or WHOIS answer for.
const MAX_TRACKED_NICKS: usize = 20_000;

/// Insert into a server-driven map, refusing once it is full.
///
/// Refusing rather than evicting is deliberate. Against a hostile
/// server an eviction policy just lets the attacker choose what we
/// forget — and what we would forget here includes which nick holds
/// which verified services account, which is exactly what
/// `is_authorized` consults. Refusing leaves the state incomplete but
/// bounded, and incomplete fails CLOSED: an unknown account is an
/// unauthorised one.
///
/// Updating a key already present is always allowed, so a full map
/// still tracks nick changes for everyone it already knows.
fn bounded_insert<V>(
    map: &mut std::collections::HashMap<String, V>,
    cap: usize,
    key: String,
    value: V,
) -> bool {
    if map.len() >= cap && !map.contains_key(&key) {
        warn!("irc: refusing to track more than {cap} entries — server is flooding state");
        return false;
    }
    map.insert(key, value);
    true
}

/// How many force-joins we will answer with a PART before we stop.
///
/// The same reasoning as `bounded_insert`: a hostile server (or an oper
/// with SAJOIN and a grudge) can put us in rooms faster than we can leave
/// them, and an unbounded record of "channels we tried to leave" is a
/// memory leak keyed by somebody else's choices. Stopping is the safe
/// direction — the sanction gate still silences every one of them.
const MAX_UNSANCTIONED_PARTS: usize = 256;

/// The PART reason a force-joined channel is given.
///
/// It says who decided, because a channel op watching a bot leave
/// deserves to know it was configuration and not a netsplit.
const UNSANCTIONED_PART_REASON: &str = "not a configured channel";

/// Leave a channel the network put us in.
///
/// The `sanctioned` set answers "may this room start a turn"; this
/// answers the question after it — "should we be sitting in it at all".
/// They are separate because the first has to fail closed the instant a
/// force-join lands, while the second is a courtesy we may not be
/// permitted to perform: a network can re-JOIN us as fast as we part, and
/// some run a lobby you cannot leave.
///
/// So: exactly ONE PART per channel per connection. If we are put back,
/// that is the server saying no, and a PART/JOIN loop against it is a
/// flood we would be blamed for — it is how a bot earns a K-line. We log
/// it and stay, silent, which is what the gate already guaranteed.
async fn leave_unsanctioned(conn: &mut Conn, channel: &str) {
    if !conn.cfg.part_unsanctioned_channels {
        info!(
            "irc: {channel} is not a configured channel — staying \
             (part_unsanctioned_channels = false); it cannot start a turn"
        );
        return;
    }
    let key = conn.casefold(channel);
    if conn.parted_unsanctioned.contains(&key) {
        warn!(
            "irc: put back into {channel} after parting it — leaving it \
             alone rather than fighting the server; it still cannot start \
             a turn"
        );
        return;
    }
    if conn.parted_unsanctioned.len() >= MAX_UNSANCTIONED_PARTS {
        warn!(
            "irc: refusing to part more than {MAX_UNSANCTIONED_PARTS} \
             unconfigured channels — the server is putting us in rooms \
             faster than we can leave them; the rest stay silent"
        );
        return;
    }
    conn.parted_unsanctioned.insert(key);
    match conn
        .send_line(
            Command::PART(
                channel.to_string(),
                Some(UNSANCTIONED_PART_REASON.to_string()),
            ),
            true,
        )
        .await
    {
        Ok(()) => info!("irc: parting {channel} — nobody configured or joined it"),
        Err(e) => warn!("irc: could not part {channel}: {e:#}"),
    }
}

/// Merge one RPL_NAMREPLY's worth of names into a channel roster.
///
/// Returns false if the cap stopped it short.
///
/// Two things this fixes, one security and one plainly visible. The
/// unbounded `extend` it replaces let a server grow the Vec forever by
/// sending 353 and never 366 (WSA-2026-3's shape); and it also meant a
/// bouncer replaying NAMES on netjoin — which is ordinary, correct
/// server behaviour — duplicated every member in the roster the pane
/// draws. The second one was a live bug against a well-behaved server,
/// which is worth saying because it is the reason a "hardening" change
/// is also a bug fix.
///
/// The dedupe is linear per name, so filling a roster from empty is
/// quadratic — about 12M comparisons at the 5000 cap, once, on a
/// background thread at join time. Once AT the cap it is O(1) per line,
/// which is the case that matters: a server flooding 353 gets rejected
/// on the first name of every line rather than paying to scan.
fn merge_roster(entry: &mut Vec<String>, names: &str) -> bool {
    for name in names.split_whitespace() {
        if entry.len() >= MAX_ROSTER_MEMBERS {
            return false;
        }
        if !entry.iter().any(|m| m == name) {
            entry.push(name.to_string());
        }
    }
    true
}

/// Keep the rosters honest between NAMES cycles.
///
/// `rosters` is filled from RPL_NAMREPLY and, until now, was never
/// touched again: a nick that quit, parted, was kicked or renamed
/// stayed listed until the next explicit NAMES. That was invisible
/// while the roster only fed a UI pane, and it stops being invisible
/// the moment a turn reads "who is here" out of it — the model would
/// address somebody who left an hour ago.
///
/// Called for OTHER people's movements (our own joins and parts are
/// handled where the channel list is), and deliberately edits only
/// rosters that already exist: an unknown channel is one we are not
/// in, not one to start tracking from a stray line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RosterMove<'a> {
    /// Add to one channel (someone else's JOIN).
    Joined(&'a str),
    /// Drop from one channel (someone else's PART, or a KICK).
    Left(&'a str),
    /// Drop from every channel (a QUIT).
    Gone,
    /// Rename in every channel, keeping the prefix chars.
    Renamed(&'a str),
}

fn roster_update(rosters: &mut HashMap<String, Vec<String>>, nick: &str, mv: RosterMove<'_>) {
    if nick.is_empty() {
        return;
    }
    let same = |entry: &String| bare_nick(entry).eq_ignore_ascii_case(nick);
    match mv {
        RosterMove::Joined(chan) => {
            if let Some(entry) = rosters.get_mut(chan) {
                if !entry.iter().any(same) && entry.len() < MAX_ROSTER_MEMBERS {
                    entry.push(nick.to_string());
                }
            }
        }
        RosterMove::Left(chan) => {
            if let Some(entry) = rosters.get_mut(chan) {
                entry.retain(|e| !same(e));
            }
        }
        RosterMove::Gone => {
            for entry in rosters.values_mut() {
                entry.retain(|e| !same(e));
            }
        }
        RosterMove::Renamed(new) => {
            for entry in rosters.values_mut() {
                for e in entry.iter_mut() {
                    if same(e) {
                        // The prefix chars are the channel's opinion of
                        // the person, not part of their nick: `@wrath`
                        // renaming to `wrath_afk` is still an op.
                        let prefix: String =
                            e.chars().take_while(|c| "@%+~&".contains(*c)).collect();
                        *e = format!("{prefix}{new}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod roster_tests {
    use super::*;

    fn r() -> HashMap<String, Vec<String>> {
        HashMap::from([
            ("#a".to_string(), vec!["@wrath".into(), "erasmus".into()]),
            ("#b".to_string(), vec!["+erasmus".into()]),
        ])
    }

    #[test]
    fn a_quit_leaves_every_channel() {
        let mut m = r();
        roster_update(&mut m, "erasmus", RosterMove::Gone);
        assert_eq!(m["#a"], vec!["@wrath".to_string()]);
        assert!(m["#b"].is_empty());
    }

    #[test]
    fn a_part_leaves_only_that_channel() {
        let mut m = r();
        roster_update(&mut m, "ERASMUS", RosterMove::Left("#a"));
        assert_eq!(m["#a"], vec!["@wrath".to_string()]);
        assert_eq!(m["#b"], vec!["+erasmus".to_string()], "#b is untouched");
    }

    #[test]
    fn a_rename_keeps_the_channel_prefix() {
        let mut m = r();
        roster_update(&mut m, "wrath", RosterMove::Renamed("wrath_afk"));
        assert_eq!(
            m["#a"],
            vec!["@wrath_afk".to_string(), "erasmus".to_string()],
            "an op who renames is still an op"
        );
    }

    #[test]
    fn a_join_adds_once_and_only_to_a_channel_we_track() {
        let mut m = r();
        roster_update(&mut m, "newcomer", RosterMove::Joined("#a"));
        roster_update(&mut m, "NEWCOMER", RosterMove::Joined("#a"));
        assert_eq!(
            m["#a"],
            vec![
                "@wrath".to_string(),
                "erasmus".to_string(),
                "newcomer".to_string()
            ],
            "added once, case-insensitively"
        );
        roster_update(&mut m, "stranger", RosterMove::Joined("#never"));
        assert!(
            !m.contains_key("#never"),
            "a stray line does not start tracking a channel we are not in"
        );
    }
}

/// Record one RPL_BANLIST mask, bounded on both axes.
///
/// Returns false when the mask was REFUSED (either axis full) so the
/// caller can say so once rather than silently dropping. A duplicate
/// mask is not a refusal — it returns true, having changed nothing.
///
/// Fails CLOSED on the channel axis like `bounded_insert`: a full map
/// refuses a NEW channel rather than evicting one, because the evicted
/// entry would be a ban list someone is waiting on at 368.
fn collect_ban(scratch: &mut HashMap<String, Vec<String>>, key: String, mask: &str) -> bool {
    if scratch.len() >= MAX_TRACKED_CHANNELS && !scratch.contains_key(&key) {
        return false;
    }
    let entry = scratch.entry(key).or_default();
    if entry.iter().any(|m| m == mask) {
        return true;
    }
    if entry.len() >= MAX_ROSTER_MEMBERS {
        return false;
    }
    entry.push(mask.to_string());
    true
}

/// Mutable per-connection state.
pub(crate) struct Conn {
    pub(crate) cfg: IrcConfig,
    /// The gateway bus — `None` for a presence that must never reach
    /// the agent.
    ///
    /// THIS IS THE SECURITY BOUNDARY, and it is a type rather than a
    /// flag on purpose. `bus.send_inbound` is the single call that
    /// turns an IRC line into an LLM turn; there is exactly one of them
    /// in this file. With an Option, the admin's connection cannot make
    /// that call because it has no bus to make it with — not by policy,
    /// not by a boolean someone might forget to check, but because the
    /// code does not compile otherwise.
    ///
    /// Without it, every line the human types in their own IRC session
    /// would be sent to the model.
    bus: Option<GatewayBus>,
    /// How this connection identifies itself on the event tap. The
    /// network is what a client groups buffers under; the presence
    /// says whether these lines are the bot's or the admin's.
    network: String,
    presence: String,
    client: Client,
    /// negotiated caps (ACKed)
    caps: HashSet<String>,
    /// caps offered by the server (from CAP LS)
    offered: HashSet<String>,
    sasl_sent: bool,
    sasl_done: bool,
    registered: bool,
    /// Services says we are logged in (SASL 903, or a NickServ
    /// IDENTIFY that was accepted). Gates the RELEASE in `regain_nick`
    /// and is reported on the boot line, because "is the bot actually
    /// identified" was previously only answerable by squinting at a
    /// WHOIS from another client.
    identified: bool,
    /// SASL failed but a password exists — send NickServ IDENTIFY as
    /// soon as registration completes (a PRIVMSG before RPL_WELCOME is
    /// not deliverable).
    identify_pending: bool,
    current_nick: String,
    desired_nick: String,
    nick_attempt: u32,
    hostmask: Option<String>,
    limits: ServerLimits,
    /// Channels the SERVER says we are in. Includes force-joins.
    channels: HashSet<String>,
    /// Channels somebody ASKED for: the config list, plus anything
    /// joined at runtime through an op or the tool.
    ///
    /// Deliberately not the same set as `channels`. A network can put us
    /// in a room on its own initiative — IRC4Fun force-joins `#lobby`,
    /// services and opers have SAJOIN — and "the server put me here" is
    /// not consent to spend a turn on whoever is in it. See the gate in
    /// `handle_privmsg`.
    sanctioned: HashSet<String>,
    /// Channels we have already sent one PART for, because nobody
    /// sanctioned them. One attempt per channel per connection — see
    /// `leave_unsanctioned` for why we do not fight a server that puts
    /// us back.
    parted_unsanctioned: HashSet<String>,
    /// nick → verified account (extended-join / account-notify / WHOIS)
    accounts: HashMap<String, String>,
    /// channel → topic text (RPL_TOPIC)
    topics: HashMap<String, String>,
    /// channel → roster nicks with prefix chars (RPL_NAMREPLY)
    rosters: HashMap<String, Vec<String>>,
    /// casefolded nick → away? from WHO replies (RPL_WHOREPLY/H).
    away: HashMap<String, bool>,
    /// nick → (user@host, server) from WHOIS 311/312
    whois_cache: HashMap<String, (String, String)>,
    /// nick → WHOIS extras (301/311-realname/313/317/319) for the 318 answer.
    whois_extra: HashMap<String, WhoExtra>,
    /// Pending WHO requesters: (target, answer buffer, reply) awaiting RPL_ENDOFWHO (315)
    who_pending: Vec<(String, Option<String>, oneshot::Sender<String>)>,
    /// Rows collected for a pending WHO; drained at 315.
    who_rows: Vec<(String, String, String, String, String, String)>,
    /// pending WHOIS requesters: (nick, answer buffer, reply) awaiting ENDOFWHOIS
    whois_pending: Vec<(String, Option<String>, oneshot::Sender<String>)>,
    /// pending ChannelInfo requesters: (channel, answer buffer, reply)
    /// awaiting ENDOFNAMES (roster complete)
    info_pending: Vec<(String, Option<String>, oneshot::Sender<String>)>,
    /// Pending ban-list requesters: (channel, answer buffer, reply)
    /// awaiting RPL_ENDOFBANLIST (368). Same shape as info_pending
    /// because the answer arrives as a stream of numerics with an
    /// explicit end.
    bans_pending: Vec<(String, Option<String>, oneshot::Sender<String>)>,
    /// Pending topic QUERIES: (channel, answer buffer, reply) awaiting
    /// RPL_TOPIC (332) / RPL_NOTOPIC (331). Setting a topic answers
    /// immediately — the server announces the change in-band.
    topic_pending: Vec<(String, Option<String>, oneshot::Sender<String>)>,
    /// channel → ban masks being collected for a pending request.
    /// Cleared at 368; bounded on BOTH axes by `collect_ban` —
    /// MAX_TRACKED_CHANNELS slots, MAX_ROSTER_MEMBERS masks each — so a
    /// server that streams 367 forever cannot grow this without end.
    bans_scratch: HashMap<String, Vec<String>>,
    /// Channels we have already sent CHATHISTORY for, so a reconnect
    /// does not re-fetch history the store already holds.
    history_done: HashSet<String>,
    outbound_bucket: TokenBucket,
    /// The server's own charge for what we send (`ServerLag`): the
    /// bucket above counts lines, this counts what they cost.
    server_lag: ServerLag,
    inbound_limiter: InboundLimiter,
}

impl Conn {
    fn casefold(&self, s: &str) -> String {
        s.to_lowercase()
    }

    /// `.agent history` — search this buffer's durable scrollback and
    /// shape the answer. Failed searches say so; they never guess.
    fn history_reply(&self, store: &ParkedStore, q: &veles_core::store::IrcQuery) -> String {
        let q = veles_core::store::IrcQuery {
            network: self.network.clone(),
            ..q.clone()
        };
        let rows = match store.lock() {
            Ok(s) => match s.irc_query(&q) {
                Ok(r) => r,
                Err(e) => return format!("history search failed: {e:#}"),
            },
            Err(_) => return "store busy — retry".into(),
        };
        let scope = q.buffer.clone().unwrap_or_else(|| "all buffers".into());
        if rows.is_empty() {
            return format!("no matching lines in {scope}");
        }
        let show_buffer = q.buffer.is_none();
        let mut out = format!("{scope} — {} line(s):", rows.len());
        for l in &rows {
            out.push('\n');
            out.push_str(&crate::tools::render_line(l, show_buffer));
        }
        out
    }

    /// Send one sanitized outbound line through the flood bucket and
    /// the server's charge (`ServerLag`).
    ///
    /// `paced=false` skips the bucket — reserved for security prompts
    /// (approval asks): a question that gates a destructive action must
    /// reach the human promptly, not wait its turn behind chat echoes and
    /// tool-progress markers. It does NOT skip the server's charge. The
    /// server parses an ask's lines in order, behind whatever it already
    /// holds, so holding them here until it would parse them makes them
    /// no later. And a plan-mode approval carries up to 30 lines of
    /// detail (`ask::render_prompt`): unpaced, that alone is an Excess
    /// Flood.
    ///
    /// The wait here is the wait on the wire only because the writer
    /// runs on its own task (`run_connection_inner`); `client.send`
    /// writes nothing itself.
    async fn send_line(&mut self, cmd: Command, paced: bool) -> Result<()> {
        // What the server counts: the line as sent, CRLF included.
        let bytes = String::from(&cmd).len() + 2;
        if paced {
            self.outbound_bucket.take_blocking().await;
        }
        self.server_lag.take_blocking(bytes).await;
        self.client.send(cmd)?;
        Ok(())
    }

    /// PRIVMSG with formatting + ISUPPORT-aware splitting (slopdrop
    /// style: visible-length-aware chunks, codes reopened across cuts).
    async fn privmsg(&mut self, target: &str, text: &str, mode: FormatMode) -> Result<()> {
        self.privmsg_paced(target, text, mode, true).await
    }

    async fn privmsg_paced(
        &mut self,
        target: &str,
        text: &str,
        mode: FormatMode,
        paced: bool,
    ) -> Result<()> {
        let converted = match mode {
            FormatMode::Irc => format::markdown_to_irc(text),
            FormatMode::Markdown => text.to_string(),
            FormatMode::Plain => format::strip_markdown(text),
        };
        // CR/NUL out, NEWLINES KEPT — the split below turns them into
        // separate IRC lines. (Sanitizing whole-text first collapsed
        // every multi-line reply into one run-on line.)
        let converted = crate::sanitize_preserving_newlines(&converted);
        let max = self.limits.max_message_len(
            target,
            self.hostmask.as_deref(),
            self.limits.nicklen.unwrap_or(30),
        );
        for chunk in format::split_formatted(&converted, max) {
            // A chunk never spans a newline; the final per-line sanitize
            // is the wire-time injection guard. Empty chunks are the
            // blank separator lines — IRC renders nothing for an empty
            // PRIVMSG, and some servers count it as spam: skip, don't
            // spend a flood token on it.
            if chunk.trim().is_empty() {
                continue;
            }
            let chunk = crate::sanitize_outbound(&chunk);
            // Echo per CHUNK, not per call: the client should show the
            // same lines the network saw, split where they were split.
            let (kind, shown) = match crate::tap::as_action(&chunk) {
                Some(inner) => (crate::tap::EventKind::Action, inner.to_string()),
                None => (crate::tap::EventKind::Privmsg, chunk.clone()),
            };
            self.send_line(Command::PRIVMSG(target.to_string(), chunk), paced)
                .await?;
            tap_local_echo(self, target, &shown, kind);
        }
        Ok(())
    }

    async fn notice(&mut self, target: &str, text: &str) -> Result<()> {
        let text = crate::sanitize_preserving_newlines(text);
        let max = self.limits.max_message_len(
            target,
            self.hostmask.as_deref(),
            self.limits.nicklen.unwrap_or(30),
        );
        for chunk in format::split_formatted(&text, max) {
            if chunk.trim().is_empty() {
                continue;
            }
            let chunk = crate::sanitize_outbound(&chunk);
            let shown = chunk.clone();
            self.send_line(Command::NOTICE(target.to_string(), chunk), true)
                .await?;
            tap_local_echo(self, target, &shown, crate::tap::EventKind::Notice);
        }
        Ok(())
    }

    /// Resolve the verified account for a sender: account tag first
    /// (spoof-proof), then extended-join cache. Never the bare nick
    /// unless dangerously opted in.
    fn resolve_account(&self, msg: &Message, sender_nick: &str) -> Option<String> {
        if let Some(tags) = &msg.tags {
            for tag in tags {
                if tag.0 == "account" {
                    if let Some(v) = &tag.1 {
                        return Some(v.clone());
                    }
                }
            }
        }
        self.accounts.get(&self.casefold(sender_nick)).cloned()
    }

    /// May this sender reach the agent at all?
    ///
    /// USER, 2026-09-09: *"The bot should NOT have NickServ/services
    /// gating of any sort, the ACL host masks are authoritative."* So
    /// the ACL decides whenever there IS an ACL: a banned principal is
    /// dropped here (the gateway drops them too — this is the cheaper
    /// door), and everyone else goes through to be judged by their
    /// capabilities. `allowed_users` is not consulted at all in that
    /// mode; it is announced as ignored at startup.
    ///
    /// With NO `[acl]` section the ACL is dormant and resolves every
    /// sender as OWNER, so the legacy allowlist has to stay the trust
    /// boundary — removing both gates at once would hand the bot to
    /// whoever spoke first. Entries there may now be masks as well as
    /// account names, so a services-less network can use the legacy
    /// path too.
    /// May this speaker's words be shown to a MODEL?
    ///
    /// `is_authorized` answers a narrower question — may they reach the
    /// agent's door at all — and returns true for everyone the ACL has
    /// not banned. That was the test the tap's `blocked` verdict used,
    /// and it let a GUEST's channel speech into somebody else's
    /// addressed turn as `irc_context`, even though the gateway refuses
    /// that same guest a turn of their own. A speaker the authorization
    /// gate would itself refuse is exactly the speaker whose words must
    /// not steer a turn.
    ///
    /// So: banned is still banned, and above that the test is the `chat`
    /// capability — the one the gateway's own gate applies. Dormant
    /// installs are untouched (`is_authorized` short-circuits on the
    /// legacy allowlist there, and dormant resolves everyone as owner
    /// anyway), and `context_requires_chat_cap = false` restores the old
    /// behaviour for an operator who would rather the bot follow the
    /// whole room.
    pub(crate) fn may_speak_to_the_model(
        &self,
        account: Option<&str>,
        ctx: &veles_core::acl::SenderContext,
    ) -> bool {
        context_verdict(
            self.is_authorized(account, ctx),
            &self.cfg.context_min_role,
            &veles_core::acl::live_config(),
            account
                .map(|a| format!("irc:{}", self.casefold(a)))
                .as_deref(),
            ctx,
        )
    }

    fn is_authorized(&self, account: Option<&str>, ctx: &veles_core::acl::SenderContext) -> bool {
        let acl = veles_core::acl::live_config();
        if acl.is_configured() {
            let id = account.map(|a| format!("irc:{}", self.casefold(a)));
            let p = veles_core::acl::Principal::resolve_ctx(id.as_deref(), ctx, &acl);
            return p.role != veles_core::acl::Role::Banned;
        }
        if self.cfg.allowed_users.is_empty() {
            return true; // open mode (warned at startup)
        }
        if let Some(acc) = account {
            if self.cfg.allowed_users.contains(&self.casefold(acc)) {
                return true;
            }
        }
        // A mask entry in the legacy allowlist, for a network with no
        // services at all.
        let Some(mask) = ctx.mask.as_deref() else {
            return false;
        };
        self.cfg.allowed_users.iter().any(|entry| {
            veles_core::ircmask::looks_like_mask(entry)
                && veles_core::ircmask::MaskPattern::parse(entry)
                    .map(|p| p.matches(mask))
                    .unwrap_or(false)
        })
    }

    /// Alternate nick when the desired one is taken (slopdrop strategy:
    /// append underscores, then numbers).
    fn alternative_nick(&self) -> String {
        match self.nick_attempt {
            0 => self.desired_nick.clone(),
            1..=3 => format!(
                "{}{}",
                self.desired_nick,
                "_".repeat(self.nick_attempt as usize)
            ),
            n => format!("{}_{}", self.desired_nick, n - 3),
        }
    }

    fn channel_mode(&self, target: &str) -> FormatMode {
        self.cfg
            .channels
            .iter()
            .find(|c| self.casefold(&c.name) == self.casefold(target))
            .map(|c| c.format)
            .unwrap_or(self.cfg.default_format)
    }

    fn channel_requires_mention(&self, target: &str) -> bool {
        self.cfg
            .channels
            .iter()
            .find(|c| self.casefold(&c.name) == self.casefold(target))
            .and_then(|c| c.require_mention)
            .unwrap_or(self.cfg.require_mention)
    }

    /// Did anyone ASK to be in this channel?
    ///
    /// The config list plus deliberate runtime joins — never
    /// `conn.channels`, which is where the server's own idea of our
    /// membership lives and therefore where a force-join lands.
    fn is_sanctioned_channel(&self, target: &str) -> bool {
        let want = self.casefold(target);
        self.sanctioned.iter().any(|c| self.casefold(c) == want)
    }

    /// Record a channel somebody deliberately joined, so lines from it
    /// can start a turn. The ONE way into `sanctioned` after boot.
    fn sanction_channel(&mut self, channel: &str) {
        if !self.is_sanctioned_channel(channel) {
            self.sanctioned.insert(channel.to_string());
        }
    }
}

// ── the adapter entry point ──────────────────────────────────────────

/// How loudly the boot line about the trust boundary should be said.
///
/// A variant rather than a `tracing::Level` so the two call sites stay
/// static callsites, and so a test can assert on the SEVERITY without
/// capturing a log.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TrustBoot {
    /// The boundary is in force; naming it is bookkeeping.
    Info(String),
    /// Something is wider than an operator would assume.
    Warn(String),
}

/// The one line the adapter prints about who may reach it.
///
/// It used to warn that an empty `IRC_ALLOWED_USERS` left the bot "OPEN
/// to every account". That is true only while `[acl]` is absent: with an
/// ACL configured the allowlist is not consulted at all and the ACL
/// decides (`is_authorized`), so the live box printed a lie at every
/// start. A warning that is wrong trains operators to skip the line that
/// matters — the one saying a dormant ACL resolves every sender as OWNER.
///
/// So this names the boundary actually in force. `allowlist_entries` is
/// the count of `IRC_ALLOWED_USERS` entries, and `acl_configured` is
/// `[acl].is_configured()` — the same pair `privileged_refusal` judges
/// by, so the two can never disagree about what the posture is.
pub(crate) fn boot_trust_line(acl_configured: bool, allowlist_entries: usize) -> TrustBoot {
    match (acl_configured, allowlist_entries) {
        (true, 0) => TrustBoot::Info(
            "irc: the trust boundary is [acl] — IRC_ALLOWED_USERS is empty and is not \
             consulted while an ACL is configured: a banned principal is dropped at the \
             door, everyone else is judged by their role"
                .to_string(),
        ),
        (true, n) => TrustBoot::Info(format!(
            "irc: the trust boundary is [acl] — the {n} IRC_ALLOWED_USERS entries are not \
             consulted while an ACL is configured: a banned principal is dropped at the \
             door, everyone else is judged by their role"
        )),
        (false, 0) => TrustBoot::Warn(
            "irc: no [acl] section and an empty IRC_ALLOWED_USERS — the bot is OPEN: the \
             ACL is dormant, so every sender resolves as OWNER with full capabilities \
             (slopdrop would refuse this)"
                .to_string(),
        ),
        // An allowlist with no ACL is the documented legacy posture, and
        // still worth a warning: everyone it admits is owner-equivalent,
        // which is not what "allowlist" usually implies.
        (false, n) => TrustBoot::Warn(format!(
            "irc: no [acl] section — the {n} IRC_ALLOWED_USERS entries ARE the trust \
             boundary, and every sender they admit resolves as OWNER (the ACL is dormant; \
             declare [acl] owner to narrow that)"
        )),
    }
}

/// Run the IRC adapter, reconnecting with backoff forever (the gateway
/// owns process lifetime).
pub async fn run_adapter(cfg: &IrcConfig, bus: GatewayBus) -> Result<()> {
    // Model-facing `irc` tool: registered once, picked up by every
    // session registry built from now on.
    static TOOL_REGISTERED: std::sync::Once = std::sync::Once::new();
    TOOL_REGISTERED.call_once(|| {
        veles_core::tools::register_global_tool(std::sync::Arc::new(
            crate::tools::IrcTool::default(),
        ));
        // The tool checks its OWN capabilities, per action: reading a
        // buffer and opping a nick are not one permission. Declaring
        // that here is what keeps the engine's fail-closed gate for
        // unclassified tools from refusing it for everyone but the
        // owner.
        veles_core::acl::declare_self_gated("irc");
        info!("irc: registered the `irc` tool (self-gated per action) for gateway sessions");
    });

    // The trust boundary, stated once at boot — and stated as the one
    // actually in force, because the old "OPEN to every account" line was
    // false on any box with an `[acl]` section.
    match boot_trust_line(
        veles_core::acl::live_config().is_configured(),
        cfg.allowed_users.len(),
    ) {
        TrustBoot::Info(line) => info!("{line}"),
        TrustBoot::Warn(line) => warn!("{line}"),
    }
    if cfg.use_tls && !cfg.tls_verify {
        warn!("irc: TLS certificate verification DISABLED — assuming a self-signed test server");
    }
    if cfg.sasl_mechanism.as_deref() == Some("EXTERNAL") && cfg.tls_client_cert.is_none() {
        anyhow::bail!("irc: SASL EXTERNAL requires a client certificate (IRC_TLS_CLIENT_CERT)");
    }

    // Outbound channel: the gateway routes replies here.
    //
    // Registered under the NETWORK as its route, because one gateway can
    // run several of these and they are not interchangeable. The plain
    // `register_outbound` would file every network under the bare
    // platform key, and the map's second insert silently replaced the
    // first — live on 2026-09-11 that made the IRC4Fun connection the
    // owner of every `irc` reply, including the ones for #flashsupport on
    // synirc, which it is not even in.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<OutboundMessage>();
    bus.register_outbound_route("irc", &cfg.name, out_tx);
    run_presence_loop(cfg, Some(bus), crate::tap::PRESENCE_BOT, out_rx).await
}

/// The connect/reconnect loop. The `presence` it runs as is the bot's
/// (`PRESENCE_BOT`): the owner's own second connection (the bouncer) was
/// removed on 2026-10-07 — the owner speaks through the bot. The tap and
/// the store keep the field, so stored history still says who said what.
async fn run_presence_loop(
    cfg: &IrcConfig,
    bus: Option<GatewayBus>,
    presence: &str,
    out_rx: mpsc::UnboundedReceiver<OutboundMessage>,
) -> Result<()> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<IrcCommand>();
    register_handle(
        &cfg.name,
        presence,
        IrcHandle {
            tx: cmd_tx.clone(),
            network: cfg.name.clone(),
        },
    );
    info!("irc/{presence}: handle registered on network {}", cfg.name);

    // A directory nothing serves, and a URL with nothing behind it, are both
    // a config that LOOKS like media delivery and is not. Said once here
    // rather than on the first undelivered file: whoever set one of the two
    // meant to set both. The configured case is logged too, so "where do
    // replies put files" has an answer in the journal.
    match (&cfg.media_public_dir, cfg.media_public_url.is_empty()) {
        (Some(dir), true) => warn!(
            "irc/{presence}: media_public_dir {} is set but media_public_url is not — \
             media will not be delivered, because there is no link to give",
            dir.display()
        ),
        (None, false) => warn!(
            "irc/{presence}: media_public_url {} is set but media_public_dir is not — \
             media will not be delivered, because there is nowhere to put it",
            cfg.media_public_url
        ),
        (Some(dir), false) => info!(
            "irc/{presence}: media publishes into {} and links from {}",
            dir.display(),
            cfg.media_public_url
        ),
        (None, true) => {}
    }
    // ── the knobs that decide behaviour, at their EFFECTIVE values ──
    //
    // Every one of these can come from three places — a default, the
    // `[platforms.irc]` block, an `IRC_*` var — and two of those are
    // silently unavailable once `[[platforms.irc.networks]]` exists
    // (`all_from_config` never calls `apply_env`). Printing what the
    // connection actually holds is the only way an operator can tell a
    // key that was read from a key that was ignored, and it is cheap:
    // one line, once per connect.
    //
    // The types are the trap this guards. `apply_mapping` reads
    // `media_public_max_age_days` and `channels` with `as_str`, so a
    // TOML integer or array parses as ABSENT and falls back — expiry
    // silently becomes "never", channels silently become none. A number
    // in this line that disagrees with config.toml is that bug.
    info!(
        "irc/{presence}: effective — require_mention={} show_route={} \
         context_min_role={} context<={}L/{}C inject_context={} \
         media_expiry={}d inbound={}/{}s prefix={:?}",
        cfg.require_mention,
        cfg.show_route,
        cfg.context_min_role,
        cfg.context_max_lines,
        cfg.context_max_chars,
        cfg.inject_channel_context,
        cfg.media_public_max_age_days,
        cfg.inbound_max_per_window,
        cfg.inbound_window_secs,
        cfg.command_prefix,
    );
    // Expiry runs at connect and then hourly (the interval arm in the
    // select loop): a link's lifetime should not depend on whether the
    // gateway happened to stay up.
    if let Some(removed) = sweep_published_media(cfg) {
        if removed > 0 {
            info!(
                "irc/{presence}: media expiry removed {removed} published file(s) older than {} days",
                cfg.media_public_max_age_days
            );
        }
    }

    // Bridge the desktop's send bus into this connection's command
    // channel, registered under (network, presence): what the desktop
    // pane types goes out as the bot.
    {
        let (send_tx, mut send_rx) = mpsc::unbounded_channel::<veles_core::irc_tap::SendRequest>();
        veles_core::irc_tap::register_sender(&cfg.name, presence, send_tx);
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            while let Some(req) = send_rx.recv().await {
                let (reply, _rx) = oneshot::channel();
                // VERBATIM. What the composer holds goes out as typed,
                // control bytes and all — converting a human's markdown
                // to IRC formatting would corrupt a message that merely
                // contains an asterisk. FormatMode::Markdown is the
                // pass-through arm despite the name.
                let cmd = match req.kind {
                    veles_core::irc_tap::SendKind::Notice => IrcCommand::Notice {
                        target: req.target,
                        text: req.text,
                        reply,
                    },
                    veles_core::irc_tap::SendKind::Action => IrcCommand::Say {
                        target: req.target,
                        text: format!("\u{1}ACTION {}\u{1}", req.text),
                        mode: Some(FormatMode::Markdown),
                        reply,
                    },
                    veles_core::irc_tap::SendKind::Privmsg => IrcCommand::Say {
                        target: req.target,
                        text: req.text,
                        mode: Some(FormatMode::Markdown),
                        reply,
                    },
                };
                if cmd_tx.send(cmd).is_err() {
                    break;
                }
            }
        });
    }

    // Ops (join/part/topic/kick/mode/…) from the desktop. Separate
    // from sends because they carry a reply: an op that silently did
    // nothing is indistinguishable from one the server rejected.
    {
        let (op_tx, mut op_rx) = mpsc::unbounded_channel::<veles_core::irc_tap::OpRequest>();
        veles_core::irc_tap::register_ops(&cfg.name, presence, op_tx);
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            while let Some(req) = op_rx.recv().await {
                use veles_core::irc_tap::IrcOp;
                let reply = req.reply;
                let cmd = match req.op {
                    IrcOp::Join { channel, key } => IrcCommand::Join {
                        channel,
                        key,
                        reply,
                    },
                    IrcOp::Part { channel, reason } => IrcCommand::Part {
                        channel,
                        reason,
                        reply,
                    },
                    IrcOp::Topic {
                        channel,
                        topic,
                        scope,
                    } => IrcCommand::Topic {
                        channel,
                        topic,
                        scope,
                        reply,
                    },
                    IrcOp::Kick {
                        channel,
                        nick,
                        reason,
                    } => IrcCommand::Kick {
                        channel,
                        nick,
                        reason,
                        reply,
                    },
                    IrcOp::Mode { target, modes } => IrcCommand::Mode {
                        target,
                        modes,
                        reply,
                    },
                    IrcOp::Nick { nick } => IrcCommand::Nick { nick, reply },
                    IrcOp::Whois { nick, scope } => IrcCommand::Whois { nick, scope, reply },
                    IrcOp::Away { message, scope } => IrcCommand::Away {
                        message,
                        scope,
                        reply,
                    },
                    IrcOp::Raw { line } => IrcCommand::Raw { line, reply },
                    IrcOp::ChannelInfo { channel, scope } => IrcCommand::ChannelInfo {
                        channel,
                        scope,
                        reply,
                    },
                    IrcOp::Bans { channel, scope } => IrcCommand::Bans {
                        channel,
                        scope,
                        reply,
                    },
                    IrcOp::Names { channel, scope } => IrcCommand::Names {
                        channel,
                        scope,
                        reply,
                    },
                    IrcOp::Who { target, scope } => IrcCommand::Who {
                        target,
                        scope,
                        reply,
                    },
                    IrcOp::Typing { target, state } => {
                        // Fire-and-forget: answer the op immediately so
                        // the HTTP layer never waits on the connection.
                        let _ = reply.send(String::new());
                        IrcCommand::Typing { target, state }
                    }
                    IrcOp::List => {
                        // List answers with a Vec, not a String, so it
                        // needs its own channel and its own join.
                        let (ltx, lrx) = oneshot::channel();
                        if cmd_tx.send(IrcCommand::List { reply: ltx }).is_err() {
                            break;
                        }
                        let names = lrx.await.unwrap_or_default();
                        let _ = reply.send(names.join(" "));
                        continue;
                    }
                };
                if cmd_tx.send(cmd).is_err() {
                    break;
                }
            }
        });
    }

    let mut out_rx = out_rx;
    let mut cmd_rx = cmd_rx;
    // One queue per presence, not one per TCP connection. A message
    // accepted before registration stays here if that attempt dies.
    let mut pending_out = std::collections::VecDeque::new();
    let mut pending_cmd = std::collections::VecDeque::new();
    let mut delay = Duration::from_secs(2);
    let mut last_connect: Option<std::time::Instant> = None;
    let mut first_attempt = true;

    // One span for the connection's life: every line the loop and the
    // connection below it emit inherits network + presence, so a
    // two-network gateway's log lines name their own network instead of
    // being indistinguishable (RingLayer renders the enclosing span's
    // fields; the desktop's log pane reads the ring).
    //
    // The loop is INSTRUMENTED, never entered: an `enter()` guard held
    // across an await (the connection runs for hours, the backoff sleeps)
    // leaves the span current on the worker thread while this task is
    // parked, so other tasks' lines there wore `network=…`, and the exit
    // could run on another thread than the enter (tracing's documented
    // async pitfall; clippy.toml now refuses the guard across an await).
    let conn_span = tracing::info_span!("irc", network = %cfg.name, presence);

    async move {
        loop {
            if first_attempt {
                first_attempt = false;
                tap_lifecycle(
                    cfg,
                    presence,
                    crate::tap::EventKind::Connecting,
                    &format!("connecting to {}:{}", cfg.server, cfg.port),
                );
            }
            info!("irc/{presence}: connecting to {}:{}", cfg.server, cfg.port);
            match run_connection(
                cfg,
                bus.as_ref(),
                presence,
                out_rx,
                cmd_rx,
                pending_out,
                pending_cmd,
            )
            .await
            {
                Ok((_, _, _, _, _, ConnEnd::Quit)) => {
                    // A QUIT we were asked for (the module's `stop`): the
                    // instance is going away, so this is the end of the
                    // loop, not a reason to reconnect.
                    tap_lifecycle(cfg, presence, crate::tap::EventKind::Disconnected, "quit");
                    return Ok(());
                }
                Ok((o, c, p, q, lived, ConnEnd::Closed)) => {
                    out_rx = o;
                    cmd_rx = c;
                    pending_out = p;
                    pending_cmd = q;
                    if lived > Duration::from_secs(120) {
                        delay = Duration::from_secs(2); // healthy session → fast retry
                    }
                    info!("irc: connection ended cleanly; reconnecting in {delay:?}");
                    tap_lifecycle(
                        cfg,
                        presence,
                        crate::tap::EventKind::Disconnected,
                        "connection closed",
                    );
                }
                Err((e, o, c, p, q)) => {
                    out_rx = o;
                    cmd_rx = c;
                    pending_out = p;
                    pending_cmd = q;
                    error!("irc: connection error: {e:#}; reconnecting in {delay:?}");
                    tap_lifecycle(
                        cfg,
                        presence,
                        crate::tap::EventKind::Disconnected,
                        &format!("{e}"),
                    );
                }
            }
            let _ = last_connect.insert(std::time::Instant::now());
            // Say the reconnect BEFORE the backoff, not only after it: the
            // pane's status line has to be able to show "reconnecting in Ns"
            // during the sleep, which is the whole time the network is down.
            tap_lifecycle(
                cfg,
                presence,
                crate::tap::EventKind::Connecting,
                &format!("reconnecting in {}s", delay.as_secs()),
            );
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    }
    .instrument(conn_span)
    .await
}

/// One connection lifetime. Returns the receivers (they must survive
/// reconnects) plus how long the connection lived.
/// Announce a connection coming up or going down on the tap. A client
/// needs to grey out a network it can no longer reach, and needs to
/// know the moment it can talk again.
fn tap_lifecycle(cfg: &IrcConfig, presence: &str, kind: crate::tap::EventKind, text: &str) {
    crate::tap::publish(crate::tap::event(
        &cfg.name,
        presence,
        crate::tap::SERVER_BUFFER,
        kind,
        "",
        text,
    ));
    if matches!(kind, crate::tap::EventKind::Disconnected) {
        crate::tap::mark_disconnected(&cfg.name, presence);
    }
}

async fn run_connection(
    cfg: &IrcConfig,
    bus: Option<&GatewayBus>,
    presence: &str,
    mut out_rx: mpsc::UnboundedReceiver<OutboundMessage>,
    mut cmd_rx: mpsc::UnboundedReceiver<IrcCommand>,
    mut pending_out: std::collections::VecDeque<OutboundMessage>,
    mut pending_cmd: std::collections::VecDeque<IrcCommand>,
) -> std::result::Result<
    (
        mpsc::UnboundedReceiver<OutboundMessage>,
        mpsc::UnboundedReceiver<IrcCommand>,
        std::collections::VecDeque<OutboundMessage>,
        std::collections::VecDeque<IrcCommand>,
        Duration,
        ConnEnd,
    ),
    (
        anyhow::Error,
        mpsc::UnboundedReceiver<OutboundMessage>,
        mpsc::UnboundedReceiver<IrcCommand>,
        std::collections::VecDeque<OutboundMessage>,
        std::collections::VecDeque<IrcCommand>,
    ),
> {
    let started = std::time::Instant::now();
    match run_connection_inner(
        cfg,
        bus,
        presence,
        &mut out_rx,
        &mut cmd_rx,
        &mut pending_out,
        &mut pending_cmd,
    )
    .await
    {
        Ok(end) => Ok((
            out_rx,
            cmd_rx,
            pending_out,
            pending_cmd,
            started.elapsed(),
            end,
        )),
        Err(e) => Err((e, out_rx, cmd_rx, pending_out, pending_cmd)),
    }
}

/// How a connection ended without an error, which decides whether the
/// loop reconnects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnEnd {
    /// The server closed it: reconnect.
    Closed,
    /// We sent QUIT because we were asked to (`IrcCommand::Quit`, the
    /// module's `stop`): the loop ends.
    Quit,
}

/// The connection's writer (`Outgoing`) on its task, aborted when the
/// connection ends: it holds the socket's write half, which would
/// otherwise outlive the connection.
struct WriterTask(tokio::task::JoinHandle<irc::error::Result<()>>);

impl Drop for WriterTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// After a QUIT is queued, read the connection's stream until the
/// server closes it (its answer to QUIT is ERROR, then EOF) or `within`
/// passes. The writer task sends the QUIT; this waits for the answer.
///
/// It stops at the stream's END or first error. An ended stream answers
/// every later poll at once with `None` — over TLS without touching the
/// socket, so without spending tokio's coop budget — and a loop that
/// polled past the end never yielded: the bound could not fire, and the
/// task held its worker thread at 100% CPU for good, starving the
/// instance runtime's timers and I/O. That was every IRC reload on
/// 2026-10-08: `stop` ran into its 10 s deadline, teardown waited out
/// its 10 s shutdown, and each reload leaked one spinning thread.
///
/// Returns the server's ERROR text, its closing line. That is the
/// server's own account of the quit: UnrealIRCd puts the reason it
/// actually showed in it (`Closing Link: jackma[host] (Quit: reload)`),
/// or the nick when its anti-spam-quit-message-time swallowed the
/// reason, which a client's display alone cannot tell from an empty QUIT.
async fn flush_quit<S, E>(stream: &mut S, within: Duration) -> Option<String>
where
    S: futures::Stream<Item = std::result::Result<Message, E>> + Unpin,
{
    let mut closing = None;
    let _ = tokio::time::timeout(within, async {
        while let Some(Ok(msg)) = stream.next().await {
            if let Command::ERROR(text) = msg.command {
                closing = Some(text);
            }
        }
    })
    .await;
    closing
}

#[cfg(test)]
mod flush_quit_tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    type Line = std::result::Result<Message, &'static str>;

    /// A connection's stream around the server's close, the way the TLS
    /// read side behaves then: it yields `lines`, then `last` at every
    /// later poll, at once, never `Pending`.
    struct Ended {
        lines: std::collections::VecDeque<Line>,
        last: Option<Line>,
    }

    impl Ended {
        fn new(lines: Vec<Line>, last: Option<Line>) -> Self {
            Ended {
                lines: lines.into(),
                last,
            }
        }
    }

    impl futures::Stream for Ended {
        type Item = Line;
        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Line>> {
            Poll::Ready(self.lines.pop_front().or_else(|| self.last.clone()))
        }
    }

    /// `flush_quit` over `stream` on a thread of its own: how long it
    /// took and the closing line it returned, or `None` when it never
    /// returned (it spins — the thread is left behind and the test fails
    /// instead of hanging the suite).
    fn flush(mut stream: Ended) -> Option<(Duration, Option<String>)> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let t0 = std::time::Instant::now();
            let closing = rt.block_on(flush_quit(&mut stream, Duration::from_millis(300)));
            let _ = tx.send((t0.elapsed(), closing));
        });
        rx.recv_timeout(Duration::from_secs(5)).ok()
    }

    #[test]
    fn the_quit_flush_ends_when_the_server_closes() {
        let (took, _) =
            flush(Ended::new(vec![], None)).expect("flush_quit spun on a closed stream");
        assert!(
            took < Duration::from_millis(200),
            "it waited out its bound instead of stopping at the close: {took:?}"
        );
    }

    #[test]
    fn the_quit_flush_ends_at_a_stream_error() {
        let (took, _) = flush(Ended::new(vec![], Some(Err("broken pipe"))))
            .expect("flush_quit spun on a failed stream");
        assert!(
            took < Duration::from_millis(200),
            "it waited out its bound instead of stopping at the error: {took:?}"
        );
    }

    #[test]
    fn the_quit_flush_returns_the_servers_closing_line() {
        let line = |raw: &str| Ok(raw.parse::<Message>().expect("a valid line"));
        let (_, closing) = flush(Ended::new(
            vec![
                line(":jackma!u@h PRIVMSG #c :late"),
                line("ERROR :Closing Link: jackma[192.0.2.1] (Quit: reload)"),
            ],
            None,
        ))
        .expect("flush_quit spun");
        assert_eq!(
            closing.as_deref(),
            Some("Closing Link: jackma[192.0.2.1] (Quit: reload)")
        );
    }
}

async fn run_connection_inner(
    cfg: &IrcConfig,
    bus: Option<&GatewayBus>,
    presence: &str,
    out_rx: &mut mpsc::UnboundedReceiver<OutboundMessage>,
    cmd_rx: &mut mpsc::UnboundedReceiver<IrcCommand>,
    pending_out: &mut std::collections::VecDeque<OutboundMessage>,
    pending_cmd: &mut std::collections::VecDeque<IrcCommand>,
) -> Result<ConnEnd> {
    // rustls 0.23 refuses to guess the crypto provider when both ring
    // and aws-lc-rs features are in the graph (feature unification) —
    // pick ring explicitly. Idempotent (Err when already installed).
    if cfg.use_tls {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    // Build the irc-crate config. QUIRK (documented): the crate's
    // rustls path reads the cert chain from client_cert_path (PEM) and
    // the PRIVATE KEY from client_cert_pass — a string field, so we
    // load the key file contents into it. The native-tls path instead
    // wants one PKCS12 DER archive in client_cert_path.
    let client_cert_pass = match (&cfg.tls_client_key, &cfg.tls_client_cert) {
        (Some(key), _) => Some(std::fs::read_to_string(key)?),
        (None, Some(_)) => None, // cert-only: error at TLS layer if a key is needed
        (None, None) => None,
    };

    let irc_cfg = Config {
        nickname: Some(cfg.nickname.clone()),
        alt_nicks: vec![],
        username: Some(cfg.username.clone()),
        realname: Some(cfg.realname.clone()),
        server: Some(cfg.server.clone()),
        port: Some(cfg.port),
        password: cfg.server_password.clone(),
        use_tls: Some(cfg.use_tls),
        // Trusted CA for self-signed servers (the local ergochat test
        // CA) — preferred over disabling verification.
        cert_path: cfg
            .tls_ca_cert
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        dangerously_accept_invalid_certs: Some(!cfg.tls_verify),
        client_cert_path: cfg
            .tls_client_cert
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        client_cert_pass,
        channels: vec![], // we join after registration ourselves
        ..Default::default()
    };

    let client = Client::from_config(irc_cfg)
        .await
        .map_err(|e| anyhow::anyhow!("irc connect: {e}"))?;

    let mut conn = Conn {
        cfg: cfg.clone(),
        bus: bus.cloned(),
        // The network is what a client groups buffers under. The
        // config's `name` is its stable identity; when none is set it is
        // the server host.
        network: cfg.name.clone(),
        presence: presence.to_string(),
        client,
        caps: HashSet::new(),
        offered: HashSet::new(),
        sasl_sent: false,
        sasl_done: false,
        registered: false,
        identified: false,
        identify_pending: false,
        current_nick: cfg.nickname.clone(),
        desired_nick: cfg.nickname.clone(),
        nick_attempt: 0,
        hostmask: None,
        limits: ServerLimits::default(),
        channels: HashSet::new(),
        // Seeded from the config, then added to by deliberate joins.
        // Casefolded on read, so the raw spellings are fine here.
        sanctioned: cfg.channels.iter().map(|c| c.name.clone()).collect(),
        parted_unsanctioned: HashSet::new(),
        accounts: HashMap::new(),
        topics: HashMap::new(),
        rosters: HashMap::new(),
        whois_extra: HashMap::new(),
        who_pending: Vec::new(),
        who_rows: Vec::new(),
        away: HashMap::new(),
        whois_cache: HashMap::new(),
        whois_pending: Vec::new(),
        info_pending: Vec::new(),
        bans_pending: Vec::new(),
        topic_pending: Vec::new(),
        bans_scratch: HashMap::new(),
        history_done: HashSet::new(),
        outbound_bucket: TokenBucket::new(cfg.flood_burst, cfg.flood_per_sec),
        server_lag: ServerLag::unrealircd(),
        inbound_limiter: InboundLimiter::new(cfg.inbound_max_per_window, cfg.inbound_window_secs),
    };

    // IRCv3 registration order — SASL BEFORE NICK. This matters on
    // ergo: the bot's nick is reserved by its (always-on) account, and
    // a pre-SASL NICK for a reserved nick gets 433. Sequence:
    //   CAP LS → REQ → ACK → AUTHENTICATE → 903 → CAP END → NICK/USER
    // NICK/USER are held back until finish_cap().
    conn.client.send_cap_ls(NegotiationVersion::V302)?;

    // The WRITER runs on a task of its own. Left to `client.stream()`,
    // it rides inside the read stream and writes only while this loop
    // polls `stream.next()`, which the loop cannot do while it is inside
    // a handler. A reply's lines were paced into the queue there and
    // then written in one burst when the handler returned: the
    // 2026-10-10 Excess Flood. Taken first, so `stream()` only reads.
    let outgoing = conn
        .client
        .outgoing()
        .ok_or_else(|| anyhow::anyhow!("irc: the connection's writer was already taken"))?;
    let mut writer = WriterTask(tokio::spawn(outgoing));
    let mut stream = conn.client.stream()?;
    let mut ping_interval = tokio::time::interval(Duration::from_secs(90));
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut nick_reclaim = tokio::time::interval(Duration::from_secs(300));
    nick_reclaim.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut media_sweep_interval = tokio::time::interval(Duration::from_secs(3600));
    media_sweep_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // RPL_WELCOME is processed by the stream arm. Return to this
        // point only after it has set `registered`, so deferred output
        // cannot outrun the registration handshake.
        while conn.registered {
            let Some(out) = pending_out.pop_front() else {
                break;
            };
            handle_outbound(&mut conn, out).await;
        }
        while conn.registered {
            let Some(cmd) = pending_cmd.pop_front() else {
                break;
            };
            handle_tool_command(&mut conn, cmd).await;
        }
        tokio::select! {
            // The writer ends only on a write error: its queue cannot
            // close while `conn.client` holds the sender.
            ended = &mut writer.0 => {
                return Err(match ended {
                    Ok(Ok(())) => anyhow::anyhow!("irc: the writer stopped"),
                    Ok(Err(e)) => anyhow::Error::new(e).context("irc write error"),
                    Err(e) => anyhow::anyhow!("irc: the writer task ended: {e}"),
                });
            }
            next = stream.next() => {
                match next {
                    Some(Ok(msg)) => {
                        // THE TAP. Every inbound line, unfiltered, with
                        // its tags intact, before handle_incoming gets
                        // to decide whether the agent cares. This is
                        // the only point all inbound traffic passes
                        // through, which is why it is the only place
                        // this has to go.
                        tap_observe(&conn, &msg);
                        let changes_state = tap_is_stateful(&msg);
                        if let Err(e) = handle_incoming(&mut conn, msg).await {
                            error!("irc: handler error: {e:#}");
                        }
                        // Snapshot AFTER handling, so the roster and
                        // topics a late-attaching client reads are the
                        // ones the line just produced.
                        if changes_state {
                            tap_publish_state(&conn);
                        }
                    }
                    Some(Err(e)) => {
                        // `.context`, not `anyhow!("...{e}")`: irc-proto's
                        // ProtocolError::Io Displays as the fixed string
                        // "an io error occurred" and hangs the real reason
                        // off itself as a #[source]. Interpolating it threw
                        // that reason away, so every transport failure —
                        // including the over-long-line refusal the vendored
                        // codec now raises — reported nothing at all.
                        return Err(anyhow::Error::new(e).context("irc stream error"));
                    }
                    None => {
                        info!("irc: stream closed by server");
                        return Ok(ConnEnd::Closed);
                    }
                }
            }
            Some(out) = out_rx.recv() => {
                // RPL_WELCOME, not queue arrival, makes a message
                // deliverable. Do not block the select loop on a ready
                // notification: CAP, PING and QUIT must still progress.
                if conn.registered {
                    handle_outbound(&mut conn, out).await;
                } else {
                    pending_out.push_back(out);
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                // A `Quit` ends this connection cleanly (the module's
                // `stop`). It is handled HERE, not in `handle_tool_command`,
                // because it ends the loop: after queueing the QUIT we read
                // the stream (`flush_quit`) until the server closes, THEN
                // answer `done` and return, which aborts the writer task.
                // Answering first is how a queued Quit was dropped with the
                // connection task and never hit the wire (back when the
                // writer rode the read stream and wrote only while this
                // loop polled it). Inbound lines that arrive meanwhile are
                // dropped: the connection is ending.
                if let IrcCommand::Quit { reason, done } = cmd {
                    conn.outbound_bucket.take_blocking().await;
                    let _ = conn
                        .client
                        .send(Command::QUIT(Some(crate::sanitize_outbound(&reason))));
                    let closing = flush_quit(&mut stream, Duration::from_millis(300)).await;
                    let _ = done.send(());
                    match closing {
                        Some(line) => info!("irc: quit ({reason}); the server closed the link: {line}"),
                        None => info!("irc: quit ({reason}); ending the connection"),
                    }
                    // A deliberate stop is not a reconnect: do not let
                    // unregistered work replay into a future lifecycle.
                    pending_out.clear();
                    pending_cmd.clear();
                    return Ok(ConnEnd::Quit);
                }
                if conn.registered {
                    handle_tool_command(&mut conn, cmd).await;
                } else {
                    pending_cmd.push_back(cmd);
                }
            }
            _ = ping_interval.tick(), if conn.registered => {
                let _ = conn.client.send(Command::PING(conn.cfg.server.clone(), None));
            }
            _ = nick_reclaim.tick(), if conn.registered => {
                if conn.current_nick != conn.desired_nick {
                    info!("irc: reclaiming nickname {}", conn.desired_nick);
                    regain_nick(&mut conn).await;
                }
            }
            _ = media_sweep_interval.tick(), if conn.registered => {
                // How long a link lives is a config answer, not a cron
                // somebody remembers. `None` = unconfigured, and cheap
                // to poll hourly.
                if let Some(removed) = sweep_published_media(&conn.cfg) {
                    if removed > 0 {
                        info!(
                            "irc/{}: media expiry removed {removed} published file(s) \
                             older than {} days",
                            conn.presence, conn.cfg.media_public_max_age_days
                        );
                    }
                }
            }
            else => break,
        }
    }
    Ok(ConnEnd::Closed)
}

// ── the event tap ────────────────────────────────────────────────────
//
// These live here rather than in tap.rs because they read `Conn`, which
// is deliberately private: the tap publishes a VIEW of the connection,
// and letting anything outside this file reach into its innards would
// make the two drift.

/// Does this message change something a client draws from state rather
/// than from scrollback — the channel list, a roster, a topic, our own
/// nick? Cheap to ask, and it keeps the snapshot off the hot path for
/// ordinary chatter.
fn tap_is_stateful(msg: &Message) -> bool {
    match &msg.command {
        Command::JOIN(..)
        | Command::PART(..)
        | Command::QUIT(..)
        | Command::NICK(..)
        | Command::TOPIC(..)
        | Command::KICK(..)
        | Command::ChannelMODE(..)
        | Command::UserMODE(..) => true,
        Command::Response(resp, _) => matches!(
            resp,
            Response::RPL_NAMREPLY
                | Response::RPL_ENDOFNAMES
                | Response::RPL_TOPIC
                | Response::RPL_NOTOPIC
                | Response::RPL_WELCOME
                // WHO answers are what `aways` in the snapshot is built
                // from, so a reply that changes an away flag has to
                // republish or the roster keeps the old marks until
                // some unrelated join happens to trigger one.
                | Response::RPL_WHOREPLY
        ),
        _ => false,
    }
}

fn tap_publish_state(conn: &Conn) {
    let mut channels: Vec<String> = conn.channels.iter().cloned().collect();
    channels.sort();
    // Away nicks per channel, so the client can mark them in the roster.
    let mut aways: HashMap<String, Vec<String>> = HashMap::new();
    for (chan, roster) in &conn.rosters {
        let mut aw = Vec::new();
        for entry in roster {
            if conn
                .away
                .get(&conn.casefold(&bare_nick(entry)))
                .copied()
                .unwrap_or(false)
            {
                aw.push(entry.clone());
            }
        }
        if !aw.is_empty() {
            aways.insert(chan.clone(), aw);
        }
    }
    crate::tap::put_state(crate::tap::NetworkState {
        network: conn.network.clone(),
        presence: conn.presence.clone(),
        nick: conn.current_nick.clone(),
        connected: conn.registered,
        channels,
        topics: conn.topics.clone(),
        rosters: conn.rosters.clone(),
        aways,
    });
}

/// The bare nick out of a NAMES entry like `@+nick!user@host`.
fn bare_nick(entry: &str) -> String {
    let no_prefix = entry.trim_start_matches(['@', '%', '+', '~', '&']);
    match no_prefix.split_once('!') {
        Some((nick, _)) => nick.to_string(),
        None => no_prefix.to_string(),
    }
}

/// `server-time` when the server sends it, so a line replayed out of
/// the backlog carries the time it was SAID rather than the time we
/// read it. Falls back to now.
fn tap_ts(msg: &Message) -> String {
    if let Some(tags) = &msg.tags {
        for t in tags {
            if t.0 == "time" {
                if let Some(v) = &t.1 {
                    if !v.is_empty() {
                        return v.clone();
                    }
                }
            }
        }
    }
    crate::tap::now_ts()
}

fn tap_nick(msg: &Message) -> String {
    crate::tap::sender_nick(msg)
}

/// Turn one inbound line into an event, if it is one a client renders.
/// The classification itself lives in tap.rs, where it can be tested
/// without standing up a whole connection.
/// Which buffers one classified line belongs in.
///
/// Almost every line belongs to exactly the buffer `classify` named.
/// A QUIT, a NICK and a user MODE do not: they arrive with no channel
/// on the wire, so `classify` can only file them under the server
/// buffer `*`. That made them invisible in every channel — the tap's
/// own `context_kind` admits `Quit`/`Nick`/`Mode`, and the rendering
/// tests passed because they published a `#channel` event by hand
/// rather than going through this path. In a real channel "erasmus
/// quit" and "erasmus is now known as erasmus_afk" simply never
/// appeared, which is exactly the kind of thing a turn needs to know
/// before it addresses somebody who is gone.
///
/// So: fan those out to every channel whose roster held the nick, from
/// the roster the connection already keeps. Called BEFORE
/// `handle_incoming`, so the roster still has the nick that is
/// leaving or renaming. No channel held them ⇒ the server buffer,
/// unchanged.
fn fanout_buffers(
    kind: crate::tap::EventKind,
    buffer: &str,
    nick: &str,
    rosters: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    use crate::tap::EventKind as K;
    let network_wide = matches!(kind, K::Quit | K::Nick | K::Mode);
    if !network_wide || buffer != crate::tap::SERVER_BUFFER || nick.is_empty() {
        return vec![buffer.to_string()];
    }
    let mut out: Vec<String> = rosters
        .iter()
        .filter(|(_, names)| {
            names
                .iter()
                .any(|e| bare_nick(e).eq_ignore_ascii_case(nick))
        })
        .map(|(chan, _)| chan.clone())
        .collect();
    out.sort();
    if out.is_empty() {
        out.push(buffer.to_string());
    }
    out
}

fn tap_observe(conn: &Conn, msg: &Message) {
    let Some((buffer, kind, text)) = crate::tap::classify(msg, &conn.current_nick) else {
        return;
    };
    let nick = tap_nick(msg);
    // With echo-message the server sends our own lines back; they
    // arrive here like anyone else's and are marked as ours.
    let own = !nick.is_empty() && nick.eq_ignore_ascii_case(&conn.current_nick);
    // The authorization verdict rides the event for SPEECH. The tap
    // itself stays a recorder of everything (that is its contract with
    // the desktop client), but a line from a speaker who could not
    // reach the agent must not come back as `irc_context` on the next
    // addressed turn — context poisoning that cannot act while
    // `require_mention` is true and can the moment it is not. Judged
    // HERE, with the same question the turn gate asks — may this
    // speaker reach the agent — so the two doors cannot disagree; state events (join/part/topic/…)
    // stay unjudged because who is IN the room is legitimate context
    // even when one of them is banned.
    let blocked =
        !own && matches!(
            kind,
            crate::tap::EventKind::Privmsg | crate::tap::EventKind::Action
        ) && {
            let account = conn.resolve_account(msg, &nick);
            let is_channel = crate::is_channel(&buffer);
            let ctx = sender_context(conn, msg, is_channel, &buffer, &nick);
            !conn.may_speak_to_the_model(account.as_deref(), &ctx)
        };
    for buf in fanout_buffers(kind, &buffer, &nick, &conn.rosters) {
        crate::tap::publish(crate::tap::IrcEvent {
            seq: 0,
            network: conn.network.clone(),
            presence: conn.presence.clone(),
            buffer: buf,
            ts: tap_ts(msg),
            kind,
            nick: nick.clone(),
            account: None,
            text: text.clone(),
            own,
            blocked,
        });
    }
}

#[cfg(test)]
mod fanout_tests {
    use super::*;
    use crate::tap::EventKind as K;

    fn rosters() -> HashMap<String, Vec<String>> {
        HashMap::from([
            (
                "#veles".to_string(),
                vec!["@wrath".into(), "erasmus".into()],
            ),
            ("#other".to_string(), vec!["+erasmus".into()]),
            ("#none".to_string(), vec!["someoneelse".into()]),
        ])
    }

    #[test]
    fn a_quit_reaches_every_channel_the_nick_was_in() {
        assert_eq!(
            fanout_buffers(K::Quit, crate::tap::SERVER_BUFFER, "erasmus", &rosters()),
            vec!["#other".to_string(), "#veles".to_string()],
            "a quit belongs in both shared channels and in neither #none nor `*`"
        );
    }

    #[test]
    fn a_nick_change_follows_the_old_nick_prefix_and_case_insensitively() {
        assert_eq!(
            fanout_buffers(K::Nick, crate::tap::SERVER_BUFFER, "WRATH", &rosters()),
            vec!["#veles".to_string()],
            "the roster entry is `@wrath`; the prefix and the case are not identity"
        );
    }

    #[test]
    fn a_stranger_leaving_stays_in_the_server_buffer() {
        assert_eq!(
            fanout_buffers(K::Quit, crate::tap::SERVER_BUFFER, "nobody", &rosters()),
            vec![crate::tap::SERVER_BUFFER.to_string()],
            "no channel held them, so the line belongs where it landed"
        );
    }

    #[test]
    fn a_line_that_already_names_its_channel_is_left_alone() {
        // The whole point is that this must not multiply ordinary
        // traffic: a channel PRIVMSG, a JOIN, a channel MODE all
        // arrive with their buffer already decided.
        for kind in [K::Privmsg, K::Join, K::Part, K::Kick, K::Topic, K::Mode] {
            assert_eq!(
                fanout_buffers(kind, "#veles", "erasmus", &rosters()),
                vec!["#veles".to_string()],
                "{kind:?} named its own buffer"
            );
        }
    }
}

/// Publish a line WE sent, for servers that do not support
/// `echo-message`.
///
/// The condition is the whole correctness of this: with the cap, the
/// server sends our messages back and this must stay silent or every
/// line appears twice; without it, nothing comes back and this is the
/// only way our own words reach our own client. It is checked at send
/// time because the cap is only known once CAP negotiation ends.
///
/// It also has to redact, and for a reason worth writing down: this path
/// does NOT go through `classify`, so the services-credential redaction
/// added for the echo-message case missed it entirely. The two paths are
/// mutually exclusive by construction — exactly one of them records any
/// given line — so a defence on one of them is a defence on half the
/// networks. This is the half WITHOUT `echo-message`, which is the older
/// and less capable half, i.e. the likelier one to still be taking a
/// plaintext `IDENTIFY`.
fn tap_local_echo(conn: &Conn, target: &str, text: &str, kind: crate::tap::EventKind) {
    if !crate::tap::needs_local_echo(&conn.caps) {
        return;
    }
    let shown =
        crate::tap::redact_service_command(target, text).unwrap_or_else(|| text.to_string());
    let mut ev = crate::tap::event(
        &conn.network,
        &conn.presence,
        &crate::tap::buffer_for(target, &conn.current_nick, &conn.current_nick),
        kind,
        &conn.current_nick,
        &shown,
    );
    ev.own = true;
    crate::tap::publish(ev);
}

// ── incoming message handling ────────────────────────────────────────

async fn handle_incoming(conn: &mut Conn, msg: Message) -> Result<()> {
    match &msg.command {
        // ── capability negotiation ──
        Command::Response(resp, args) => {
            debug!("irc numeric {:?}: {:?}", resp, args);
            handle_numeric(conn, *resp, args).await?;
        }
        Command::CAP(_, sub, marker, params) => {
            // CAP's fields: (nick, subcommand, '*' continuation marker,
            // cap list). The marker decides multi-line LS, not the list.
            let list: Vec<String> = params.iter().cloned().collect();
            let more = marker.as_deref() == Some("*");
            handle_cap(conn, sub, marker.as_deref(), more, &list).await?;
        }
        Command::AUTHENTICATE(payload) => {
            if payload == "+" {
                // Server ready for our SASL payload.
                send_sasl_payload(conn).await?;
            }
        }
        // ── keepalive ──
        Command::PING(token, _) => {
            conn.client.send(Command::PONG(token.clone(), None))?;
        }
        // ── our own nick confirmed / changed ──
        Command::NICK(new_nick) => {
            if let Some(Prefix::Nickname(old, user, host)) = &msg.prefix {
                let was_us = conn.casefold(old) == conn.casefold(&conn.current_nick);
                if was_us {
                    conn.current_nick = new_nick.clone();
                    conn.hostmask = Some(format!("{new_nick}!{user}@{host}"));
                }
                // Did somebody ELSE just move us off our own nick?
                //
                // Services enforcement arrives as a NICK for us that we
                // did not ask for — on 2026-09-11 that was
                // `jackma -> synGuest27554`, and the only recovery was a
                // 300s tick that then failed forever (see `regain_nick`).
                // Try immediately: five minutes under the wrong name is
                // five minutes of not answering to a mention, because
                // `addressed_to` matches on `current_nick`.
                //
                // `desired_nick` is what we WANT, and it is updated by
                // the deliberate-rename paths (`.irc nick`, the tool),
                // so this cannot fight a nick change the operator asked
                // for — that one moves the goalpost with it.
                if was_us
                    && !conn
                        .casefold(new_nick)
                        .eq(&conn.casefold(&conn.desired_nick))
                {
                    warn!(
                        "irc: our nick was changed to {new_nick} by someone else \
                         (services enforcement?) — trying to regain {}",
                        conn.desired_nick
                    );
                    regain_nick(conn).await;
                }
                // Ours or anyone's: the rosters name people, and a
                // rename that is not followed through leaves a nick
                // listed as present under a name nobody answers to.
                roster_update(&mut conn.rosters, old, RosterMove::Renamed(new_nick));
                if let Some(acc) = conn.accounts.remove(&conn.casefold(old)) {
                    let key = conn.casefold(new_nick);
                    bounded_insert(&mut conn.accounts, MAX_TRACKED_NICKS, key, acc);
                }
                // The trigger engine's NICK event — never for our own
                // rename (a trigger must not fire on ourselves).
                if !was_us {
                    if let Some((user, host)) = user_host_of(&msg) {
                        fire_triggers_event(
                            conn,
                            "NICK",
                            old,
                            &format!("{old}!{user}@{host}"),
                            "*",
                            new_nick,
                        )
                        .await;
                    }
                }
            }
        }
        // ── chat traffic ──
        Command::PRIVMSG(target, text) => {
            // A server may send PRIVMSG before 001. No chat, trigger or
            // command is acted on during capability negotiation: this is
            // the same registration boundary as the outbound queues.
            // Ordinary connection bookkeeping uses the other command arms.
            if !conn.registered {
                return Ok(());
            }
            handle_privmsg(conn, &msg, target, text).await?;
        }
        Command::NOTICE(target, text) => {
            debug!("irc NOTICE <{target}>: {text}");
        }
        // ── account tracking (extended-join) ──
        Command::JOIN(channel, account, _) => {
            if let Some(Prefix::Nickname(nick, _, _)) = &msg.prefix {
                if let Some(acc) = account {
                    if acc != "*" {
                        let key = conn.casefold(nick);
                        bounded_insert(&mut conn.accounts, MAX_TRACKED_NICKS, key, acc.clone());
                    }
                }
                if conn.casefold(nick) == conn.casefold(&conn.current_nick) {
                    conn.channels.insert(conn.casefold(channel));
                    // A fresh join starts a new NAMES cycle — drop any
                    // stale roster so members don't accumulate duplicates
                    // across re-joins/refreshes (hit live 2026-08-10:
                    // roster listed every member twice).
                    conn.rosters.remove(&conn.casefold(channel));
                    info!("irc: joined {channel}");
                    // A JOIN we did not ask for. The gate below already
                    // makes the room mute; this is the other half —
                    // leaving it, if the network lets us.
                    if !conn.is_sanctioned_channel(channel) {
                        leave_unsanctioned(conn, channel).await;
                    }
                } else {
                    let key = conn.casefold(channel);
                    roster_update(&mut conn.rosters, nick, RosterMove::Joined(&key));
                    // The trigger engine's JOIN event — other people
                    // joining, in a sanctioned channel.
                    if conn.is_sanctioned_channel(channel) {
                        if let Some((user, host)) = user_host_of(&msg) {
                            fire_triggers_event(
                                conn,
                                "JOIN",
                                nick,
                                &format!("{nick}!{user}@{host}"),
                                channel,
                                "",
                            )
                            .await;
                        }
                    }
                }
            }
        }
        Command::PART(channel, reason) => {
            if let Some(Prefix::Nickname(nick, _, _)) = &msg.prefix {
                if conn.casefold(nick) == conn.casefold(&conn.current_nick) {
                    conn.channels.remove(&conn.casefold(channel));
                    conn.rosters.remove(&conn.casefold(channel));
                } else {
                    conn.accounts.remove(&conn.casefold(nick));
                    let key = conn.casefold(channel);
                    roster_update(&mut conn.rosters, nick, RosterMove::Left(&key));
                    if conn.is_sanctioned_channel(channel) {
                        if let Some((user, host)) = user_host_of(&msg) {
                            fire_triggers_event(
                                conn,
                                "PART",
                                nick,
                                &format!("{nick}!{user}@{host}"),
                                channel,
                                reason.as_deref().unwrap_or_default(),
                            )
                            .await;
                        }
                    }
                }
            }
        }
        Command::QUIT(reason) => {
            if let Some(Prefix::Nickname(nick, _, _)) = &msg.prefix {
                let is_us = conn.casefold(nick) == conn.casefold(&conn.current_nick);
                conn.accounts.remove(&conn.casefold(nick));
                roster_update(&mut conn.rosters, nick, RosterMove::Gone);
                if !is_us {
                    if let Some((user, host)) = user_host_of(&msg) {
                        fire_triggers_event(
                            conn,
                            "QUIT",
                            nick,
                            &format!("{nick}!{user}@{host}"),
                            "*",
                            reason.as_deref().unwrap_or_default(),
                        )
                        .await;
                    }
                }
            }
        }
        // A kick is a part somebody else decided. There is no arm for
        // it in the agent's own dispatch — nothing downstream cared —
        // but the roster does: the kicked nick is no longer present.
        Command::KICK(channel, who, reason) => {
            if conn.casefold(who) == conn.casefold(&conn.current_nick) {
                conn.channels.remove(&conn.casefold(channel));
                conn.rosters.remove(&conn.casefold(channel));
            } else {
                conn.accounts.remove(&conn.casefold(who));
                let key = conn.casefold(channel);
                roster_update(&mut conn.rosters, who, RosterMove::Left(&key));
                if conn.is_sanctioned_channel(channel) {
                    // The args are [kicked, kicker, channel, reason];
                    // the hook builds them from what this arm has.
                    if let Some(Prefix::Nickname(kicker, ku, kh)) = &msg.prefix {
                        fire_triggers_event(
                            conn,
                            "KICK",
                            who,
                            &format!("{kicker}!{ku}@{kh}"),
                            channel,
                            reason.as_deref().unwrap_or_default(),
                        )
                        .await;
                    }
                }
            }
        }
        // ── account-notify ──
        Command::ACCOUNT(account) => {
            if let Some(Prefix::Nickname(nick, _, _)) = &msg.prefix {
                if account == "*" {
                    conn.accounts.remove(&conn.casefold(nick));
                } else {
                    let key = conn.casefold(nick);
                    bounded_insert(&mut conn.accounts, MAX_TRACKED_NICKS, key, account.clone());
                }
            }
        }
        // ── chathistory replay ──
        // BATCH +ref chathistory <chan> … BATCH -ref. The inner lines are
        // ALSO dispatched individually by the codec (BATCH is metadata,
        // not an envelope the crate unwraps), so there is nothing to do
        // here for their CONTENT — tap_observe has already classified each
        // PRIVMSG/JOIN/… with its server-time tag intact. The only reason
        // to look at BATCH at all is bookkeeping: log that a replay ran,
        // so a silent gap is distinguishable from "nothing happened".
        Command::BATCH(ref_tag, Some(sub), _) => {
            let _ = ref_tag;
            debug!("irc: batch {} ({})", ref_tag, sub.to_str());
        }
        // WHOIS account reply (RPL_WHOISACCOUNT, 330) — not in this
        // proto version's Response enum, arrives as Command::Raw.
        Command::Raw(code, args) => {
            if code == "330" {
                if let (Some(nick), Some(acc)) = (args.get(1), args.get(2)) {
                    let key = conn.casefold(nick);
                    bounded_insert(&mut conn.accounts, MAX_TRACKED_NICKS, key, acc.clone());
                }
            // NOTE: there is no `code == "352"` arm here, and there must
            // not be. 352 is RPL_WHOREPLY, which IS in irc-proto's
            // Response enum, so it arrives as Command::Response and
            // never as Raw — an arm here is dead code. There was one,
            // and away tracking therefore never worked at all: the WHO
            // poll fired on every ENDOFNAMES and its answer went in the
            // bin, so no nick was ever marked away. 330 below is
            // different and genuinely does need Raw — RPL_WHOISACCOUNT
            // is NOT in this proto version's enum. The real handler is
            // in `handle_numeric`.
            } else if code.eq_ignore_ascii_case("TAGMSG") {
                // +typing indicator (IRCv3). tags carry +typing=active|done.
                //
                // This was `debug!`-logged only, which is why the pane
                // never showed an indicator: the wire line was parsed
                // and thrown away. Now it becomes a Typing tap event —
                // EPHEMERAL (the store writer skips that kind), so a
                // reconnect never replays stale typing state. Only
                // +typing-tagged TAGMSG to a target we know matters;
                // anything else stays invisible.
                let Some(state) = msg
                    .tags
                    .as_ref()
                    .and_then(|tags| tags.iter().find(|t| t.0 == "+typing"))
                    .and_then(|t| t.1.clone())
                else {
                    return Ok(());
                };
                let Some(Prefix::Nickname(nick, _, _)) = &msg.prefix else {
                    return Ok(());
                };
                if conn.casefold(nick) == conn.casefold(&conn.current_nick) {
                    return Ok(()); // our own echo-message
                }
                let target = match &msg.command {
                    Command::Raw(_, a) => a.first().cloned().unwrap_or_default(),
                    _ => String::new(),
                };
                let buffer = crate::tap::buffer_for(&target, nick, &conn.current_nick);
                let text = match state.as_str() {
                    "active" => "active".to_string(),
                    "done" | "" => "done".to_string(),
                    other => other.to_string(),
                };
                crate::tap::publish(crate::tap::IrcEvent {
                    seq: 0,
                    network: conn.network.clone(),
                    presence: conn.presence.clone(),
                    buffer,
                    ts: tap_ts(&msg),
                    kind: crate::tap::EventKind::Typing,
                    nick: nick.clone(),
                    account: None,
                    text,
                    own: false,
                    blocked: false,
                });
            }
        }
        _ => {
            debug!("irc unhandled: {:?}", msg.command);
        }
    }
    Ok(())
}

async fn handle_numeric(conn: &mut Conn, resp: Response, args: &[String]) -> Result<()> {
    use irc::proto::Response::*;
    match resp {
        RPL_WELCOME => {
            conn.registered = true;
            info!("irc: registered as {}", conn.current_nick);
            // Registration, not TCP connect, is when a client can act.
            let mut ev = crate::tap::event(
                &conn.network,
                &conn.presence,
                crate::tap::SERVER_BUFFER,
                crate::tap::EventKind::Connected,
                &conn.current_nick,
                &format!("registered as {}", conn.current_nick),
            );
            ev.own = true;
            crate::tap::publish(ev);
            // The deferred IDENTIFY (see ERR_SASLFAIL). Now is the
            // first moment a PRIVMSG is deliverable.
            if conn.identify_pending {
                conn.identify_pending = false;
                if let Some(pw) = conn.cfg.sasl_password.clone() {
                    let account = conn
                        .cfg
                        .sasl_username
                        .clone()
                        .unwrap_or_else(|| conn.desired_nick.clone());
                    info!("irc: SASL failed earlier — identifying to NickServ as {account}");
                    conn.send_line(
                        Command::PRIVMSG(
                            "NickServ".to_string(),
                            format!("IDENTIFY {account} {pw}"),
                        ),
                        true,
                    )
                    .await?;
                }
            }
            // Say whether the bot is actually identified.
            //
            // Not cosmetic: an unidentified bot on a network with
            // services is one enforcement timer away from losing its
            // nick, and when that happened on 2026-09-11 the only
            // symptom was that the bot stopped answering — because
            // mention gating keys on `current_nick`. The boot line is
            // where that becomes visible before it costs anything.
            if conn.identified {
                info!("irc/{}: identified to services", conn.presence);
            } else if conn.cfg.sasl_password.is_some() {
                info!(
                    "irc/{}: not identified yet — an IDENTIFY is in flight",
                    conn.presence
                );
            } else {
                warn!(
                    "irc/{}: NOT identified and no sasl_password is configured. On a network \
                     with services an enforced nick will be taken away, and the bot will keep \
                     sitting in its channels under a name nobody addresses.",
                    conn.presence
                );
            }
            // Awaiting the pacer needs a mutable connection, so do not
            // retain the immutable channel-list borrow across the await.
            let joins: Vec<_> = conn
                .cfg
                .channels
                .iter()
                .map(|c| (c.name.clone(), c.key.clone()))
                .collect();
            // Welcome is also the reconnect path for configured channels.
            // These JOINs must use the same wire budget as live commands;
            // writing them directly made the reconnect backlog one burst.
            for (channel, key) in joins {
                conn.send_line(Command::JOIN(channel, None, key), true)
                    .await?;
            }
        }
        RPL_ISUPPORT => {
            conn.limits.parse_isupport(args);
            debug!("irc: ISUPPORT limits: {:?}", conn.limits);
        }
        RPL_SASLSUCCESS => {
            conn.sasl_done = true;
            conn.identified = true;
            info!("irc: SASL authentication successful");
            finish_cap(conn).await?;
        }
        ERR_SASLFAIL | ERR_NICKLOCKED => {
            // NOT the end of it. SASL is refused for reasons that a
            // NickServ IDENTIFY is not — synIRC declines SASL for an
            // account whose email is unconfirmed, and the same password
            // then identifies fine over PRIVMSG. Falling back matters
            // because the alternative is what happened live on
            // 2026-09-11: unidentified, services enforced the nick, and
            // the bot sat in the channel as `synGuest27554`.
            //
            // Deferred, not sent here: we are still mid-CAP and cannot
            // PRIVMSG before registration completes. `identify_pending`
            // is drained at RPL_WELCOME.
            if conn.cfg.sasl_password.is_some() {
                warn!(
                    "irc: SASL authentication FAILED — will try NickServ IDENTIFY \
                     once registered"
                );
                conn.identify_pending = true;
            } else {
                warn!("irc: SASL authentication FAILED — continuing unauthenticated");
            }
            finish_cap(conn).await?;
        }
        RPL_LOGGEDIN => {
            // 900 is the server telling us we hold an account, and it
            // arrives for BOTH routes in: SASL at connect, and a
            // NickServ IDENTIFY afterwards. Setting the flag here rather
            // than only in RPL_SASLSUCCESS is what makes the IDENTIFY
            // fallback count as identified — otherwise `regain_nick`
            // would skip the RELEASE for exactly the connection that
            // most needs it.
            conn.identified = true;
            if let Some(acc) = args.get(2) {
                info!("irc: logged in as account {acc}");
            }
        }
        // ── channel introspection state ──
        RPL_TOPIC => {
            // 332 <nick> <channel> :<topic>
            if let (Some(chan), Some(topic)) = (args.get(1), args.get(2)) {
                let key = conn.casefold(chan);
                bounded_insert(&mut conn.topics, MAX_TRACKED_CHANNELS, key, topic.clone());
            }
            let Some(chan) = args.get(1) else {
                return Ok(());
            };
            let key = conn.casefold(chan);
            // Answer pending topic QUERIES: this is their 332. The
            // answer goes to the reply AND into the requested buffer as
            // an Info tap event — a topic query typed in a channel
            // reads its answer in that channel, not in a status line.
            let pending: Vec<_> = conn
                .topic_pending
                .extract_if(
                    ..,
                    |(c, _, _): &mut (String, Option<String>, oneshot::Sender<String>)| *c == key,
                )
                .collect();
            let current = conn.topics.get(&key).cloned();
            for (_, buffer, reply) in pending {
                let text = match &current {
                    Some(t) => format!("{chan}: {t}"),
                    None => format!("{chan}: no topic set"),
                };
                let _ = reply.send(text.clone());
                tap_info(conn, &buffer.unwrap_or_default(), "", &text);
            }
        }
        RPL_NOTOPIC => {
            // 331 <nick> <channel> :No topic is set — the QUERY'S "no"
            // answer. Nothing published this to the tap, so a topic
            // query on a topicless channel answered via timeout and the
            // status bar instead of the buffer.
            let Some(chan) = args.get(1) else {
                return Ok(());
            };
            let key = conn.casefold(chan);
            let pending: Vec<_> = conn
                .topic_pending
                .extract_if(
                    ..,
                    |(c, _, _): &mut (String, Option<String>, oneshot::Sender<String>)| *c == key,
                )
                .collect();
            for (_, buffer, reply) in pending {
                let text = format!("{chan}: no topic is set");
                let _ = reply.send(text.clone());
                tap_info(conn, &buffer.unwrap_or_default(), "", &text);
            }
        }
        RPL_WHOREPLY => {
            // 352 <me> <chan> <user> <host> <server> <nick> <flags>
            // <hops> :<real>. The flags field combines H (here) / G
            // (gone, i.e. away) with * for an oper and channel prefix
            // characters.
            //
            // This lived under `Command::Raw("352", ..)` and was
            // therefore DEAD: 352 is in irc-proto's Response enum, so
            // it never arrives as Raw. Away marking in the roster has
            // consequently never worked — the WHO poll that ENDOFNAMES
            // fires had nothing listening for its answer. Found by
            // mutation: making the old handler index out of bounds
            // changed nothing, which is only possible if it never runs.
            if let (Some(nick), Some(flags)) = (args.get(5), args.get(6)) {
                let key = conn.casefold(nick);
                bounded_insert(&mut conn.away, MAX_TRACKED_NICKS, key, flags.contains('G'));
            }
            // Collect rows for a pending /who. 352 <me> <chan> <user>
            // <host> <server> <nick> <flags> ... — row tuples mirror that
            // order (channel, user, host, server, nick, flags); capped so
            // a server that streams 352 forever cannot grow without end.
            if !conn.who_pending.is_empty() {
                let row = (
                    args.get(1).cloned().unwrap_or_default(),
                    args.get(2).cloned().unwrap_or_default(),
                    args.get(3).cloned().unwrap_or_default(),
                    args.get(4).cloned().unwrap_or_default(),
                    args.get(5).cloned().unwrap_or_default(),
                    args.get(6).cloned().unwrap_or_default(),
                );
                if conn.who_rows.len() < MAX_TRACKED_NICKS {
                    conn.who_rows.push(row);
                }
            }
        }
        RPL_NAMREPLY => {
            // 353 <nick> = <channel> :nick1 @op2 +voice3
            //
            // This appended unconditionally, which is two problems at
            // once. A server can send 353 forever and never send 366,
            // so the Vec grew without bound (WSA-2026-3's shape); and a
            // bouncer that replays NAMES on netjoin duplicated every
            // member, so the roster in the pane read wrong even against
            // an entirely well-behaved server.
            if let (Some(chan), Some(names)) = (args.get(2), args.get(3)) {
                let key = conn.casefold(chan);
                if conn.rosters.len() >= MAX_TRACKED_CHANNELS && !conn.rosters.contains_key(&key) {
                    warn!(
                        "irc: refusing to track a roster for more than \
                         {MAX_TRACKED_CHANNELS} channels"
                    );
                } else {
                    let entry = conn.rosters.entry(key).or_default();
                    if !merge_roster(entry, names) {
                        warn!("irc: roster for {chan} is full at {MAX_ROSTER_MEMBERS}");
                    }
                }
            }
        }
        RPL_ENDOFNAMES => {
            // 366 <nick> <channel> :End of /NAMES list
            if let Some(chan) = args.get(1) {
                // Refresh away state for this channel's roster. WHO is
                // also the periodic poll: it re-fires on every NAMES
                // completion, which is how a strayed-away nick stays
                // marked even after we missed the switch.
                let _ = conn
                    .send_line(Command::Raw(format!("WHO {chan}"), Vec::new()), true)
                    .await;
                let key = conn.casefold(chan);
                // chathistory backfill: ask the server for what we
                // missed while offline. Once per channel per connection —
                // asking twice would replay history the store already
                // holds. (Across a RECONNECT it is asked again, and that
                // is fine: the msgid/server-time dedup key below catches
                // the redelivery. This set only stops us asking twice on
                // one connection.)
                //
                // This fires on ENDOFNAMES, which is NOT the same as "our
                // own join" — a server may send an unsolicited NAMES cycle
                // for any channel name it invents, and each novel one
                // would mint a set entry AND make us emit a CHATHISTORY
                // line. So it is capped like every other channel-keyed
                // map. Requires the server to have ACKed chathistory;
                // servers that do not offer it simply never get asked.
                if !conn.history_done.contains(&key)
                    && conn.caps.contains("chathistory")
                    && conn.history_done.len() < MAX_TRACKED_CHANNELS
                {
                    conn.history_done.insert(key.clone());
                    conn.send_line(
                        Command::Raw(format!("CHATHISTORY LATEST {chan} * 50"), Vec::new()),
                        true,
                    )
                    .await?;
                }
                let pending: Vec<_> = conn
                    .info_pending
                    .extract_if(
                        ..,
                        |(c, _, _): &mut (String, Option<String>, oneshot::Sender<String>)| {
                            *c == key
                        },
                    )
                    .collect();
                for (_, buffer, reply) in pending {
                    let roster = conn.rosters.get(&key).cloned().unwrap_or_default();
                    let topic = conn.topics.get(&key).cloned();
                    let mut out = format!("{chan}: {} member(s)", roster.len());
                    if let Some(t) = topic {
                        out.push_str(&format!(" | topic: {t}"));
                    }
                    if !roster.is_empty() {
                        let shown: Vec<&String> = roster.iter().take(30).collect();
                        out.push_str(&format!(
                            "\nmembers: {}",
                            shown
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(" ")
                        ));
                        if roster.len() > 30 {
                            out.push_str(&format!(" … (+{} more)", roster.len() - 30));
                        }
                    }
                    let _ = reply.send(out.clone());
                    tap_info(conn, &buffer.unwrap_or_default(), "", &out);
                }
            }
        }
        // ── WHOIS enrichment (ircx irc_whois parity) ──
        RPL_WHOISUSER => {
            // 311 <me> <nick> <user> <host> * :<realname>
            if let (Some(nick), Some(user), Some(host)) = (args.get(1), args.get(2), args.get(3)) {
                let key = conn.casefold(nick);
                let server = conn
                    .whois_cache
                    .get(&key)
                    .map(|(_, s)| s.clone())
                    .unwrap_or_default();
                bounded_insert(
                    &mut conn.whois_cache,
                    MAX_TRACKED_NICKS,
                    key.clone(),
                    (format!("{user}@{host}"), server),
                );
                if let Some(real) = args.get(5) {
                    if real != "*"
                        && (conn.whois_extra.len() < MAX_TRACKED_NICKS
                            || conn.whois_extra.contains_key(&key))
                    {
                        conn.whois_extra.entry(key.clone()).or_default().realname =
                            Some(real.clone());
                    }
                }
            }
        }
        RPL_WHOISSERVER => {
            // 312 <me> <nick> <server> :<server info>
            if let (Some(nick), Some(server)) = (args.get(1), args.get(2)) {
                let key = conn.casefold(nick);
                let uh = conn
                    .whois_cache
                    .get(&key)
                    .map(|(u, _)| u.clone())
                    .unwrap_or_default();
                bounded_insert(
                    &mut conn.whois_cache,
                    MAX_TRACKED_NICKS,
                    key,
                    (uh, server.clone()),
                );
            }
        }
        RPL_AWAY => {
            // 301 <me> <nick> :<away text> — part of a WHOIS reply.
            if let (Some(nick), Some(reason)) = (args.get(1), args.get(2)) {
                let key = conn.casefold(nick);
                if conn.whois_extra.len() < MAX_TRACKED_NICKS || conn.whois_extra.contains_key(&key)
                {
                    conn.whois_extra.entry(key).or_default().away = Some(reason.clone());
                }
            }
        }
        RPL_WHOISOPERATOR => {
            // 313 <me> <nick> :is an IRC operator
            if let Some(nick) = args.get(1) {
                let key = conn.casefold(nick);
                if conn.whois_extra.len() < MAX_TRACKED_NICKS || conn.whois_extra.contains_key(&key)
                {
                    conn.whois_extra.entry(key).or_default().oper = true;
                }
            }
        }
        RPL_WHOISIDLE => {
            // 317 <me> <nick> <idle-seconds> <signon-time>
            if let (Some(nick), Some(idle)) = (args.get(1), args.get(2)) {
                let key = conn.casefold(nick);
                if conn.whois_extra.len() < MAX_TRACKED_NICKS || conn.whois_extra.contains_key(&key)
                {
                    let e = conn.whois_extra.entry(key).or_default();
                    e.idle = Some(idle.clone());
                    e.signon = args.get(3).cloned();
                }
            }
        }
        RPL_WHOISCHANNELS => {
            // 319 <me> <nick> :channels...
            if let Some(nick) = args.get(1) {
                let key = conn.casefold(nick);
                let chans = args[2..].join(" ");
                if conn.whois_extra.len() < MAX_TRACKED_NICKS || conn.whois_extra.contains_key(&key)
                {
                    conn.whois_extra.entry(key).or_default().channels = Some(chans);
                }
            }
        }
        RPL_ENDOFWHO => {
            // 315 — answer pending /who requests with the rows collected
            // since the query went out (RPL_WHOREPLY below).
            let pending: Vec<_> = std::mem::take(&mut conn.who_pending);
            let rows: Vec<_> = std::mem::take(&mut conn.who_rows);
            for (target, buffer, reply) in pending {
                let total = rows.len();
                let long = total > 30;
                let mut out = format!("who {target}: {total} row(s)");
                for (chan, user, host, server, nick, flags) in &rows {
                    if long {
                        out.push_str(&format!("\n{nick} ({user}@{host}) {flags}"));
                    } else {
                        out.push_str(&format!(
                            "\n{nick} ({user}@{host}) on {chan} via {server} {flags}"
                        ));
                    }
                }
                if long {
                    out.push_str(&format!("\n… (+{} more)", total - 30));
                }
                let _ = reply.send(out.clone());
                tap_info(conn, &buffer.unwrap_or_default(), "who", &out);
            }
        }
        RPL_ENDOFWHOIS => {
            // 318 <me> <nick> :End of /WHOIS — answer pending requests.
            if let Some(nick) = args.get(1) {
                let key = conn.casefold(nick);
                let pending: Vec<_> = conn
                    .whois_pending
                    .extract_if(
                        ..,
                        |(n, _, _): &mut (String, Option<String>, oneshot::Sender<String>)| {
                            *n == key
                        },
                    )
                    .collect();
                for (_, buffer, reply) in pending {
                    let acc = conn.accounts.get(&key).cloned();
                    let host = conn.whois_cache.get(&key).map(|(h, _)| h.clone());
                    let server = conn.whois_cache.get(&key).map(|(_, s)| s.clone());
                    let mut out = format!("{nick}:");
                    if let Some(a) = acc {
                        out.push_str(&format!(" verified account {a};"));
                    }
                    if let Some(h) = host {
                        out.push_str(&format!(" hostmask {nick}!{h};"));
                    }
                    if let Some(s) = server {
                        out.push_str(&format!(" server {s};"));
                    }
                    if let Some(e) = conn.whois_extra.get(&key) {
                        if let Some(r) = &e.realname {
                            out.push_str(&format!(" realname \"{r}\";"));
                        }
                        if e.oper {
                            out.push_str(" is an IRC operator;");
                        }
                        if let Some(a) = &e.away {
                            out.push_str(&format!(" away: {a};"));
                        }
                        if let Some(i) = &e.idle {
                            out.push_str(&format!(" idle {i}s;"));
                        }
                        if let Some(s) = &e.signon {
                            out.push_str(&format!(" signon {s};"));
                        }
                        if let Some(c) = &e.channels {
                            out.push_str(&format!(" channels {c};"));
                        }
                    }
                    if out == format!("{nick}:") {
                        out.push_str(" no information available");
                    }
                    let _ = reply.send(out.clone());
                    // The answer is also a buffer line: publish it to
                    // the buffer the op asked for, so a whois typed in
                    // a channel reads there (and survives in scrollback)
                    // instead of dying as a status-bar whisper.
                    tap_info(conn, &buffer.unwrap_or_default(), "", &out);
                }
            }
        }
        ERR_NICKNAMEINUSE => {
            conn.nick_attempt += 1;
            let alt = conn.alternative_nick();
            warn!(
                "irc: 433 nick {} in use (attempt {alt}) — note: the crate ALSO \
                 sees this and kills the stream, so this alt-nick path rarely fires",
                conn.cfg.nickname
            );
            conn.client.send(Command::NICK(alt))?;
        }
        // ── ban list (Tier 3) ──
        // 367 <me> <channel> <banmask> [<setter> <ts>]. Collected into a
        // scratch Vec per channel; the reply fires at 368.
        //
        // TWO axes have to be bounded, and the first version only bounded
        // one. Masks per channel is the obvious one. The other is CHANNEL
        // COUNT: `entry(key).or_default()` mints a map slot for whatever
        // channel name the numeric carries, and 367 does not require us to
        // have asked, or even to be in the channel. A server streaming
        // `367 <me> #1 x`, `367 <me> #2 x`, … grows the map for as long as
        // it cares to type — WSA-2026-3's shape at a map instead of a
        // buffer. `collect_ban` bounds both.
        RPL_BANLIST => {
            if let (Some(chan), Some(mask)) = (args.get(1), args.get(2)) {
                let key = conn.casefold(chan);
                if !collect_ban(&mut conn.bans_scratch, key, mask) {
                    warn!(
                        "irc: refusing to collect more ban masks — \
                         {MAX_TRACKED_CHANNELS} channels / \
                         {MAX_ROSTER_MEMBERS} masks is the ceiling"
                    );
                }
            }
        }
        RPL_ENDOFBANLIST => {
            // 368 <me> <channel> :End of channel ban list
            let Some(chan) = args.get(1) else {
                return Ok(());
            };
            let key = conn.casefold(chan);
            let bans = conn.bans_scratch.remove(&key).unwrap_or_default();
            let pending: Vec<_> = conn
                .bans_pending
                .extract_if(
                    ..,
                    |(c, _, _): &mut (String, Option<String>, oneshot::Sender<String>)| *c == key,
                )
                .collect();
            for (_, buffer, reply) in pending {
                let out = if bans.is_empty() {
                    format!("{chan}: no bans")
                } else {
                    format!("{chan}: {} ban(s)\n{}", bans.len(), bans.join("\n"))
                };
                let _ = reply.send(out.clone());
                tap_info(conn, &buffer.unwrap_or_default(), "", &out);
            }
        }
        // ── server buffer: MOTD and connection statistics ──
        //
        // WeeChat renders these in the server buffer; here they fell
        // into the catch-all and vanished — the user's first report
        // after making this pane the daily driver was "I don't see
        // server status or motd". They are Info events on `*`: numerics
        // with no wire line of their own, which is exactly the case
        // EventKind::Info was made for. 376 (RPL_ENDOFMOTD) gets no
        // line — weechat does not draw it either; the MOTD block is
        // self-delimiting.
        RPL_MOTDSTART => {
            // 375 <me> :- <server> Message of the day -
            if let Some(text) = args.get(1) {
                tap_server(conn, &format!("MOTD — {}", motd_body(text)));
            }
        }
        RPL_MOTD => {
            // 372 <me> :- <line>
            if let Some(text) = args.get(1) {
                tap_server(conn, motd_body(text));
            }
        }
        ERR_NOMOTD => {
            tap_server(conn, "MOTD — none");
        }
        RPL_LUSERCLIENT => {
            // 251 <me> :There are N users on M servers
            if let Some(text) = args.get(1) {
                tap_server(conn, text);
            }
        }
        RPL_LUSEROP | RPL_LUSERUNKNOWN | RPL_LUSERCHANNELS => {
            // 252/253/254 <me> <count> :<what>
            if let Some(n) = args.get(1) {
                let what = args.get(2).map(String::as_str).unwrap_or_default();
                tap_server(conn, &format!("{n} {what}"));
            }
        }
        RPL_LUSERME => {
            // 255 <me> :I have N clients and M servers
            if let Some(text) = args.get(1) {
                tap_server(conn, text);
            }
        }
        _ => {}
    }
    Ok(())
}

async fn handle_cap(
    conn: &mut Conn,
    sub: &CapSubCommand,
    marker: Option<&str>,
    more_following: bool,
    params: &[String],
) -> Result<()> {
    match sub {
        CapSubCommand::LS => {
            // The irc crate puts the cap list into the 3rd field on the
            // FINAL LS line (`CAP * LS :caps` → marker=Some("caps")),
            // and into the 4th on continuation lines (`CAP * LS * :caps`).
            // Parse both so no chunk is ever lost.
            let mut collect = |list: &str| {
                for cap in list.split_whitespace() {
                    conn.offered
                        .insert(cap.split('=').next().unwrap_or(cap).to_string());
                }
            };
            if let Some(m) = marker {
                if m != "*" {
                    collect(m);
                }
            }
            if let Some(list) = params.last() {
                collect(list);
            }
            // Multi-line LS carries a '*' marker; only negotiate once
            // the list is complete.
            if !more_following {
                let wanted: Vec<Capability> = WANTED_CAP_NAMES
                    .iter()
                    .filter(|name| conn.offered.contains(**name))
                    .filter_map(|name| cap_by_name(name))
                    .collect();
                if wanted.is_empty() {
                    finish_cap(conn).await?;
                } else {
                    info!(
                        "irc: requesting caps: {:?}",
                        wanted.iter().map(|c| c.as_ref()).collect::<Vec<_>>()
                    );
                    conn.client.send_cap_req(&wanted)?;
                }
            }
        }
        CapSubCommand::ACK => {
            // Same 3rd-field quirk as LS: `CAP * ACK :caps` lands the
            // cap list in the marker slot — read both fields.
            let mut collect = |list: &str| {
                for cap in list.split_whitespace() {
                    conn.caps.insert(cap.trim_start_matches('-').to_string());
                }
            };
            if let Some(m) = marker {
                if m != "*" {
                    collect(m);
                }
            }
            if let Some(list) = params.last() {
                collect(list);
            }
            // All requested caps acked → SASL or done.
            if conn.caps.contains("sasl") && !conn.sasl_sent {
                conn.sasl_sent = true;
                match conn.cfg.sasl_mechanism.as_deref() {
                    Some("EXTERNAL") => {
                        info!("irc: starting SASL EXTERNAL (CertFP)");
                        conn.client.send_sasl_external()?;
                    }
                    Some("PLAIN") => {
                        info!("irc: starting SASL PLAIN");
                        conn.client.send_sasl_plain()?;
                    }
                    _ => {
                        finish_cap(conn).await?;
                    }
                }
            } else if !conn.caps.contains("sasl") || conn.sasl_done {
                finish_cap(conn).await?;
            }
        }
        CapSubCommand::NAK => {
            warn!("irc: CAP NAK — proceeding without refused caps");
            finish_cap(conn).await?;
        }
        _ => {}
    }
    Ok(())
}

/// CAP END closes negotiation, THEN we register (NICK/USER). SASL-first
/// ordering (IRCv3 SASL 3.1 spec sequence): the nick is claimed only
/// after the account is proven, so ergo's reserved-nick rule
/// (always-on accounts) never fires 433.
async fn finish_cap(conn: &mut Conn) -> Result<()> {
    if let Some(pass) = &conn.cfg.server_password {
        conn.client
            .send(Command::PASS(crate::sanitize_outbound(pass)))?;
    }
    // CAP END before NICK/USER — the spec sequence for SASL.
    conn.client
        .send(Command::CAP(None, CapSubCommand::END, None, None))?;
    conn.client.send(Command::NICK(conn.cfg.nickname.clone()))?;
    conn.client.send(Command::USER(
        conn.cfg.username.clone(),
        "0".to_string(),
        conn.cfg.realname.clone(),
    ))?;
    Ok(())
}

/// Send the SASL payload after AUTHENTICATE +.
async fn send_sasl_payload(conn: &mut Conn) -> Result<()> {
    // The payload is ALREADY the wire form (base64 for PLAIN, "+" for
    // EXTERNAL) — chunked at 400 chars per the SASL spec, never
    // re-encoded.
    let payload = match conn.cfg.sasl_mechanism.as_deref() {
        Some("EXTERNAL") => "+".to_string(), // empty payload
        Some("PLAIN") => {
            let user = conn
                .cfg
                .sasl_username
                .clone()
                .unwrap_or_else(|| conn.cfg.nickname.clone());
            let pass = conn.cfg.sasl_password.clone().unwrap_or_default();
            let raw = format!("{user}\0{user}\0{pass}");
            base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
        }
        _ => {
            conn.client.send_sasl_abort()?;
            return Ok(());
        }
    };
    for chunk in payload.as_bytes().chunks(400) {
        conn.client.send(Command::AUTHENTICATE(
            String::from_utf8_lossy(chunk).into_owned(),
        ))?;
    }
    if !payload.is_empty() && payload.len() % 400 == 0 {
        // Exact multiple → spec terminator.
        conn.client.send(Command::AUTHENTICATE("+".to_string()))?;
    }
    Ok(())
}

// ── PRIVMSG → gateway bus (with mention gating + abuse control) ──────

/// The identity of the chat a PRIVMSG belongs to. A channel message
/// belongs to its channel. A query (DM) PRIVMSG's target is OUR OWN
/// NICK — the chat is the sender. Keying sessions, asks and replies on
/// `target` therefore made every DM reply to ourselves and collapsed
/// all DMs into one session named after the bot.
fn chat_identity<'a>(is_channel: bool, target: &'a str, sender: &'a str) -> &'a str {
    if is_channel {
        target
    } else {
        sender
    }
}

#[cfg(test)]
mod chat_identity_tests {
    use super::*;

    #[test]
    fn a_channel_message_belongs_to_its_channel() {
        assert_eq!(chat_identity(true, "#veles", "wrath"), "#veles");
        assert_eq!(chat_identity(true, "&local", "wrath"), "&local");
    }

    #[test]
    fn a_query_belongs_to_the_sender_not_our_own_nick() {
        // A DM PRIVMSG targets the bot's OWN nick; keying sessions,
        // asks and replies on it made every DM answer itself and
        // collapsed all DMs into one bot-named session.
        assert_eq!(chat_identity(false, "velestest", "wrath"), "wrath");
    }
}

/// Is a nick character — the set a nick may legally contain, so that
/// `velesbot` is not read as `veles` with something after it.
///
/// RFC 2812 allows letters, digits and `[]\`_^{|}-`; servers differ at
/// the margins, so the test is deliberately generous: anything that
/// could be part of somebody's nick blocks a match.
fn is_nick_char(c: char) -> bool {
    c.is_alphanumeric() || "-[]\\`_^{|}".contains(c)
}

/// Does `nick` sit at byte offset `at` in `text`, as a whole nick?
///
/// ASCII-case-insensitive (IRC's own casemapping is ASCII-ish and the
/// nick is the bot's own), and the character AFTER the match must not
/// be a nick character — that is what tells `veles` apart from
/// `velesbot`.
fn nick_at(text: &str, nick: &str, at: usize) -> bool {
    let end = at + nick.len();
    if !text.is_char_boundary(at) || end > text.len() || !text.is_char_boundary(end) {
        return false;
    }
    if !text[at..end].eq_ignore_ascii_case(nick) {
        return false;
    }
    !matches!(text[end..].chars().next(), Some(c) if is_nick_char(c))
}

/// Ask services to let go of our nick, then take it back.
///
/// WHY A BARE `NICK` IS NOT ENOUGH (found live, 2026-09-11): the bot
/// registered `jackma`, could not identify (the account's email was
/// unconfirmed, and synIRC refuses SASL for an unconfirmed account), so
/// services enforced the nick and renamed it to `synGuest27554`. The
/// 300s reclaim tick then sent `NICK jackma` every five minutes,
/// forever, and every one of them was refused — because an ENFORCED
/// nick is not free, it is HELD by services' own enforcer. Nothing in
/// the log said so, and the bot sat in the channel under a name nobody
/// addresses, which also silently breaks mention gating: `addressed_to`
/// matches on `current_nick`, so "jackma: hi" stopped being a mention.
///
/// `RELEASE` drops that hold, and it needs no password when we are
/// already identified (which SASL does at connect). Sending it first
/// costs one line to a service and turns a permanently-failing reclaim
/// into one that works on the first try.
///
/// Best-effort by construction: a network with no services, or one that
/// spells the command differently, simply ignores the RELEASE and the
/// NICK behaves exactly as it did before.
async fn regain_nick(conn: &mut Conn) {
    let want = conn.desired_nick.clone();
    if conn.cfg.sasl_password.is_some() || conn.identified {
        // Only meaningful for a nick we own; harmless otherwise.
        let _ = conn
            .send_line(
                Command::PRIVMSG("NickServ".to_string(), format!("RELEASE {want}")),
                true,
            )
            .await;
    }
    let _ = conn.send_line(Command::NICK(want), true).await;
}

/// The key the inbound flood limiter counts against.
///
/// NOT the nick, which is what it was until 2026-09-11. A `/nick` is
/// free and instant on IRC, so a limiter keyed on it is a limiter with
/// a one-word bypass — and since the map is keyed by whatever this
/// returns, every fresh nick also leaked a permanent entry.
///
/// Preference order, hardest-to-choose first:
///
///  1. the verified services account, when the network gave us one. A
///     client cannot choose this at all.
///  2. `ident@host` off the wire. The host is assigned by the server
///     (a cloak, a rDNS, an IP hash) and the ident is at worst
///     half-chosen — `~` marks one the server could not verify. This is
///     the same reasoning `ircmask::MaskSpecificity` encodes when it
///     ranks host above ident above nick.
///  3. the nick, only when the prefix carried no user/host at all,
///     which is the shape a service or a malformed line has.
///
/// Scoped by network, so the same person on two networks has two
/// budgets — they are two connections and two rosters.
///
/// DELIBERATELY NOT the principal subject. The subject is an IDENTITY:
/// it keys sessions, sandboxes (`user_envs::zone_for` builds
/// `dm:{subject}`) and ask binding. Coarsening THAT to `ident@host`
/// would put two strangers behind one bouncer in a single chat session,
/// which is a privacy leak far worse than the budget bypass it fixes. A
/// throttle key may safely be coarser than an identity; an identity may
/// not safely be coarser than itself.
fn throttle_key(network: &str, account: Option<&str>, msg: &Message, sender: &str) -> String {
    if let Some(a) = account {
        let a = a.trim();
        if !a.is_empty() {
            return format!("{network}/account:{}", a.to_lowercase());
        }
    }
    if let Some(Prefix::Nickname(_, user, host)) = &msg.prefix {
        if !host.trim().is_empty() {
            return format!(
                "{network}/{}@{}",
                user.trim().to_lowercase(),
                host.trim().to_lowercase()
            );
        }
    }
    format!("{network}/nick:{}", sender.to_lowercase())
}

/// Is this slopdrop's spelling: a bare `tcl <code>` line, with no sigil
/// and no nick.
///
/// Requires something after the word, so a lone "tcl" stays ordinary
/// channel speech; a sentence that merely begins with it ("tcl is a
/// strange little language") is still evaluated, which is the documented
/// cost of a convention people have a decade of muscle memory for.
fn is_bare_tcl_line(text: &str) -> bool {
    // `split_whitespace` already skips leading whitespace.
    let mut words = text.split_whitespace();
    // Both of slopdrop's words, because somebody who types one types
    // the other. `tclAdmin` still has to clear the role check in the
    // arm; recognising the spelling is not granting it.
    words
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("tcl") || w.eq_ignore_ascii_case("tcladmin"))
        && words.next().is_some()
}

/// Is this line ours by CONVENTION rather than by address?
///
/// Two questions that must be answered together, which is why they are
/// one function called from both places rather than two conditions
/// written twice. The mention gate asks it to decide whether to let an
/// unaddressed line through, and the arm asks it to decide whether to
/// answer one. If they ever disagreed in the direction gate-yes /
/// arm-no, an unaddressed `tcl …` would skip the gate, find no arm to
/// answer it, and fall through to `send_inbound` — a BILLED TURN for a
/// line nobody addressed to us.
///
/// The second half — a persistent interpreter is configured — is what
/// makes the spelling ours at all. `tcl` is slopdrop's word, and the
/// only reason veles answers to it is that veles is holding slopdrop's
/// interpreter. Without one there is nothing of slopdrop's to answer
/// out of, and a local throwaway sandbox answering in its place is not
/// a degraded answer but a wrong one: it holds none of the channel's
/// stored procs, so `tcl [someproc]` comes back "invalid command name"
/// — which reads as *your proc is gone*. USER, 2026-09-12.
///
/// `configured()` parses the config file, so the cheap string test
/// short-circuits it and ordinary chatter pays nothing.
fn is_ours_by_convention(text: &str) -> bool {
    is_bare_tcl_line(text) && veles_core::tcleval::backend_configured()
}

/// The same verdict, for the trigger hook (outside this module's
/// privacy): a line the bare-tcl arm will answer is not channel
/// chatter, and the trigger engine must not double-handle it.
pub(crate) fn is_ours_by_convention_public(text: &str) -> bool {
    is_ours_by_convention(text)
}

/// Where a bare `tcl` line's answer goes: the channel, not the caller.
///
/// Every other command on this adapter answers with a NOTICE to the
/// person who typed it, and for them that is right — `.acl list` is
/// administration, addressed to one human, and putting it in the
/// channel is noise everyone else has to scroll past.
///
/// The bare `tcl` spelling is the opposite case, and for the same
/// reason it exists at all. `#coven` has a decade of muscle memory for
/// slopdrop, and slopdrop answers `tcl expr {6 * 7}` **in the
/// channel**: the answer is shared, the next person builds on it, and
/// the whole convention is a room computing together. Answering the
/// asker privately keeps the letter of the spelling and throws away
/// the point of it. USER, 2026-09-12: *"jackma … responds with /notice
/// instead of responding in the channel"*.
///
/// In a query `target` is our OWN nick, so the reply has to go to the
/// sender or it goes to us.
///
/// `FormatMode::Markdown` is the pass-through arm despite the name
/// (see the desktop send bridge). Interpreter output is program
/// output: markdown-to-IRC conversion would corrupt any result that
/// happens to contain an asterisk or an underscore, which for a
/// language whose idiom is `string repeat * 5` is not hypothetical.
async fn reply_bare_tcl(
    conn: &mut Conn,
    is_channel: bool,
    target: &str,
    sender: &str,
    text: &str,
) -> Result<()> {
    let dest = if is_channel { target } else { sender };
    conn.privmsg(dest, text, FormatMode::Markdown).await
}

/// Is this line addressed to `nick`, and what is left of it once the
/// address is taken off?
///
/// `Some(rest)` means yes; `rest` is the line with the address removed
/// and trimmed, which is what the model should see (`veles: hi` is a
/// turn that says `hi`, not one that says `veles: hi`). `None` means
/// the line was not addressed to us.
///
/// WHY a helper and not two `contains` calls: the previous version
/// tested only `nick:` and `@nick`, anywhere in the line. That misses
/// the two commonest ways people actually type — `veles, hi` and bare
/// `veles hi` — so on a `require_mention` channel a perfectly normal
/// address got no turn at all, while `wrath@velestest` (a hostmask in
/// somebody else's paste) got one.
///
/// The shapes accepted:
///
/// * leading `nick` followed by `,` `:` `;` `>` or whitespace, or the
///   bare nick alone (`veles: hi`, `veles, hi`, `veles hi`, `veles`)
/// * `@nick` anywhere, as long as the `@` itself starts a word
///   (`hey @veles look`) — so an `@` inside a mask or an address does
///   not count
/// * a trailing `, nick` (`does that work, veles`)
fn addressed_to(nick: &str, text: &str) -> Option<String> {
    if nick.is_empty() {
        return None;
    }
    let t = text.trim();

    // Leading address: the nick, then a separator or whitespace, or
    // nothing at all.
    if nick_at(t, nick, 0) {
        let rest = &t[nick.len()..];
        let after_sep = match rest.chars().next() {
            None => Some(rest),
            Some(c) if matches!(c, ':' | ',' | ';' | '>') => Some(&rest[c.len_utf8()..]),
            Some(c) if c.is_whitespace() => Some(rest),
            Some(_) => None,
        };
        if let Some(body) = after_sep {
            return Some(body.trim().to_string());
        }
    }

    // `@nick` anywhere, with the `@` at a word start.
    let mut from = 0usize;
    while let Some(off) = t[from..].find('@') {
        let at_sign = from + off;
        let before_ok = !matches!(t[..at_sign].chars().next_back(), Some(c) if is_nick_char(c));
        let name = at_sign + 1;
        if before_ok && nick_at(t, nick, name) {
            // Lifting `@veles` out of the middle must not leave the
            // double space (or the orphaned comma) behind: rejoin the
            // two halves the way a person would have typed them.
            let head = t[..at_sign].trim_end();
            let tail = t[name + nick.len()..].trim_start();
            let mut out = String::with_capacity(t.len());
            out.push_str(head);
            let joins = !head.is_empty()
                && !matches!(
                    tail.chars().next(),
                    None | Some(',' | ':' | ';' | '.' | '!' | '?')
                );
            if joins {
                out.push(' ');
            }
            out.push_str(tail);
            return Some(out.trim().to_string());
        }
        from = at_sign + 1;
    }

    // Trailing `, nick`.
    if let Some(start) = t.len().checked_sub(nick.len()) {
        if nick_at(t, nick, start) {
            if let Some(head) = t[..start].trim_end().strip_suffix(',') {
                return Some(head.trim().to_string());
            }
        }
    }

    None
}

#[cfg(test)]
mod addressed_tests {
    use super::*;

    #[test]
    fn the_shapes_people_actually_type_are_addresses() {
        for line in [
            "veles: hello",
            "veles, hello",
            "veles hello",
            "veles;hello",
            "veles>hello",
            "@veles hello",
            "VeLeS: hello",
        ] {
            assert_eq!(
                addressed_to("veles", line).as_deref(),
                Some("hello"),
                "{line:?} addresses us and leaves `hello`"
            );
        }
        assert_eq!(
            addressed_to("veles", "does that work, veles").as_deref(),
            Some("does that work"),
            "a trailing address is an address too"
        );
        assert_eq!(
            addressed_to("veles", "hey @veles look at this").as_deref(),
            Some("hey look at this"),
            "an inline @nick is lifted out of the middle"
        );
        assert_eq!(
            addressed_to("veles", "  veles  ").as_deref(),
            Some(""),
            "the bare nick is an address with an empty body"
        );
    }

    #[test]
    fn a_nick_that_merely_starts_ours_is_not_us() {
        // The regression the old `contains` test could not see: any
        // line mentioning a longer nick that begins with ours counted
        // as addressed, so two bots answered each other's traffic.
        for line in [
            "velesbot: hello",
            "velesbot hello",
            "@velesbot hello",
            "hello, velesbot",
        ] {
            assert_eq!(
                addressed_to("veles", line),
                None,
                "{line:?} addresses velesbot, not veles"
            );
        }
    }

    #[test]
    fn our_nick_inside_a_word_is_not_an_address() {
        for line in [
            "not-veles hello",
            "unveles: hello",
            "wrath@veles said hi",
            "see http://veles.example/ for docs",
            "i asked veles about it",
        ] {
            assert_eq!(
                addressed_to("veles", line),
                None,
                "{line:?} is not an address"
            );
        }
    }

    #[test]
    fn an_empty_nick_never_matches() {
        // Pre-registration the connection has no nick yet; matching the
        // empty string at offset 0 would make every line an address.
        assert_eq!(addressed_to("", "hello"), None);
    }

    fn privmsg_from(nick: &str, user: &str, host: &str) -> Message {
        Message {
            tags: None,
            prefix: Some(Prefix::Nickname(
                nick.to_string(),
                user.to_string(),
                host.to_string(),
            )),
            command: Command::PRIVMSG("#c".to_string(), "hi".to_string()),
        }
    }

    /// The bypass this key exists to close: a `/nick` used to buy a
    /// fresh flood budget, because the limiter counted nicks.
    ///
    /// Mutation check: return the nick from `throttle_key` and the
    /// first assertion goes red.
    #[test]
    fn a_nick_change_does_not_buy_a_fresh_budget() {
        let a = throttle_key(
            "synirc",
            None,
            &privmsg_from("eve", "~e", "cloak.example"),
            "eve",
        );
        let b = throttle_key(
            "synirc",
            None,
            &privmsg_from("eve2", "~e", "cloak.example"),
            "eve2",
        );
        assert_eq!(a, b, "same ident@host, two nicks — one budget");

        // …but two people who merely share a HOST stay apart. On a
        // bouncer the ident is the account, which is exactly the case
        // #coven has: ~Demotion and ~cromega behind one shared bouncer.
        let c = throttle_key(
            "irc4fun",
            None,
            &privmsg_from("cromega", "~cromega", "shared-bouncer.example.net"),
            "cromega",
        );
        let d = throttle_key(
            "irc4fun",
            None,
            &privmsg_from("Demotion", "~Demotion", "shared-bouncer.example.net"),
            "Demotion",
        );
        assert_ne!(c, d, "one host, two idents — two budgets");
    }

    #[test]
    fn a_verified_account_outranks_the_mask_and_the_network_scopes_it() {
        // An account holder who changes BOTH nick and host (a reconnect
        // from somewhere else) is still one budget.
        let home = throttle_key(
            "synirc",
            Some("Wrath"),
            &privmsg_from("xenia", "~hastur", "home.example"),
            "xenia",
        );
        let away = throttle_key(
            "synirc",
            Some("wrath"),
            &privmsg_from("xenia_", "~h", "other.example"),
            "xenia_",
        );
        assert_eq!(home, away, "the account is the key, case-folded");

        // Two networks are two connections, so two budgets.
        let elsewhere = throttle_key(
            "irc4fun",
            Some("wrath"),
            &privmsg_from("xenia", "~hastur", "home.example"),
            "xenia",
        );
        assert_ne!(home, elsewhere, "scoped by network");
    }

    /// A service or a malformed line can arrive with no user/host. It
    /// must still get A key rather than collapsing everyone into one.
    #[test]
    fn a_prefix_without_a_host_falls_back_to_the_nick() {
        let m = Message {
            tags: None,
            prefix: Some(Prefix::ServerName("services.example".to_string())),
            command: Command::PRIVMSG("#c".to_string(), "hi".to_string()),
        };
        assert_eq!(
            throttle_key("synirc", None, &m, "NickServ"),
            "synirc/nick:nickserv"
        );
        // An empty host is the same case, not an `@`-suffixed key.
        let empty = privmsg_from("weird", "~w", "");
        assert_eq!(
            throttle_key("synirc", None, &empty, "weird"),
            "synirc/nick:weird"
        );
    }
}

/// What the ACL sees about a sender: the wire mask, the network, and
/// where the line landed.
///
/// The mask is the IDENTITY on a network without Services, which is
/// the posture this bot is built for — not a weaker fallback to an
/// account. `platform`, `chat_id` and `direct` are filled for every
/// connector, so a rule or a sandbox assignment can scope on them
/// without knowing IRC's spelling of "a room".
/// Whether one speaker's words may enter the rolling context block a
/// model reads.
///
/// A free function taking the ACL rather than a method reading the
/// global, so a test can hold the decision without a live config file
/// and without racing every other test in this binary for `VELES_HOME`
/// — the same reason `gen_denied` and `video_gate` were extracted from
/// their command.
///
/// `authorized` is the transport's own answer (`Conn::is_authorized`:
/// not banned, or on the legacy allowlist). Two things sit above it.
///
/// A speaker with `chat` explicitly denied contributes nothing: they
/// cannot take a turn, so their words have no business steering one.
/// (`Role::Guest` DOES hold `chat` by deliberate design — a guest can
/// talk — so this catches an explicit `deny_caps`, not a whole rung.)
///
/// And `min_role` is the operator's own line. It defaults to `guest`,
/// which changes nothing, because the honest default is the one that
/// was already there. What it buys is the ability to raise it: every
/// unaddressed line in a channel reaches the model as `irc_context` on
/// the next ADDRESSED turn, so in a room of strangers "whose words may
/// steer a turn" is a different question from "who may start one", and
/// `require_mention` only answers the second.
fn context_verdict(
    authorized: bool,
    min_role: &str,
    acl: &veles_core::acl::AclConfig,
    id: Option<&str>,
    ctx: &veles_core::acl::SenderContext,
) -> bool {
    if !authorized {
        return false;
    }
    if !acl.is_configured() {
        // Dormant: the adapter allowlist IS the boundary, and
        // `resolve_ctx` would hand every stranger `Role::Owner` anyway.
        return true;
    }
    let p = veles_core::acl::Principal::resolve_ctx(id, ctx, acl);
    if !p.has(veles_core::acl::Cap::Chat) {
        return false;
    }
    // An unparseable role name must not silently open the gate: fall
    // back to `guest`, which is the documented default, rather than to
    // "allow everything" or to a refusal nobody asked for.
    let floor = veles_core::acl::Role::parse(min_role).unwrap_or(veles_core::acl::Role::Guest);
    p.role >= floor
}

/// The raw `user@host` of a message's sender, for the trigger hook's
/// mask (same fields `sender_context` puts in `SenderContext.mask`).
fn user_host_of(msg: &Message) -> Option<(String, String)> {
    match &msg.prefix {
        Some(Prefix::Nickname(_, user, host)) => Some((user.clone(), host.clone())),
        _ => None,
    }
}

/// Feed one non-TEXT event to the trigger engine and relay the replies
/// to the channel. QUIT/NICK are network-wide (`channel = "*"`): the
/// engine's rows decide per-channel via the glob, and replies for a
/// channel-less event have nowhere to go — so for those the replies are
/// recorded in the fire log only.
async fn fire_triggers_event(
    conn: &mut Conn,
    event: &str,
    nick: &str,
    mask: &str,
    channel: &str,
    text: &str,
) {
    let cfg = veles_core::config::load().unwrap_or_default();
    let replies = crate::triggers_hook::observe_event(
        conn,
        &cfg,
        &conn.network,
        event,
        nick,
        mask,
        channel,
        text,
    )
    .await;
    if channel != "*" {
        for reply in replies {
            let _ = conn.privmsg(channel, &reply, FormatMode::Markdown).await;
        }
    }
}

fn sender_context(
    conn: &Conn,
    msg: &Message,
    is_channel: bool,
    target: &str,
    sender: &str,
) -> veles_core::acl::SenderContext {
    let mut ctx = veles_core::acl::SenderContext {
        network: Some(conn.cfg.name.clone()),
        platform: Some("irc".into()),
        direct: !is_channel,
        ..Default::default()
    };
    if is_channel {
        ctx.channel = Some(target.to_string());
        ctx.chat_id = Some(target.to_string());
    } else {
        // A query's conversation is the SENDER, not our own nick (the
        // 2026-08 regression that collapsed every DM into one session).
        ctx.chat_id = Some(sender.to_string());
    }
    if let Some(irc::proto::Prefix::Nickname(_, user, host)) = &msg.prefix {
        ctx.mask = Some(format!("{sender}!{user}@{host}"));
    }
    ctx
}

async fn handle_privmsg(
    conn: &mut Conn,
    msg: &Message,
    target: &str,
    raw_text: &str,
) -> Result<()> {
    let Some(Prefix::Nickname(sender, _user, _host)) = &msg.prefix else {
        return Ok(());
    };
    if conn.casefold(sender) == conn.casefold(&conn.current_nick) {
        return Ok(()); // our own echo-message
    }

    let is_channel = crate::is_channel(target);

    // A channel the operator never asked for cannot start a turn.
    //
    // FOUND LIVE (2026-09-11, first connect to IRC4Fun): the network
    // force-joins `#lobby`, so with `channels = ""` the bot was sitting
    // in a public room nothing in the config mentions. `cfg.channels`
    // was read in exactly ONE place — the join loop at registration —
    // and nothing downstream ever asked whether an inbound line came
    // from a channel we chose. Every stranger in that room could
    // address the bot and spend a billed LLM turn.
    //
    // The gate is on the CONFIG list, not on `conn.channels`: the latter
    // is "where the server says I am", which is precisely what a
    // force-join, an SAJOIN or a services-driven join writes into. A
    // channel joined deliberately at runtime (`.irc join`, the tool, the
    // desktop) is the operator asking, and `sanctioned_channels` is
    // where that lands so it stays one list.
    //
    // Channel messages only. A DM is addressed to the bot BY someone,
    // which is a different question and one the ACL already answers.
    if is_channel && !conn.is_sanctioned_channel(target) {
        debug!(
            "irc: ignoring {target} — not a configured channel (we are in it, \
             but nobody asked for it)"
        );
        return Ok(());
    }

    let chat = chat_identity(is_channel, target, sender);

    // The inbound rate limit USED TO BE HERE, and both halves of that
    // were wrong (2026-09-11):
    //
    //  - it ran BEFORE the mention gate, so ordinary channel chatter
    //    spent the budget. Ten lines in ten seconds is a normal person
    //    talking, and the eleventh line — the one actually addressed to
    //    the bot — was the one dropped. In a busy channel that reads as
    //    "the bot ignores me";
    //  - it was keyed on the NICK, which is free to change. The brake
    //    between a stranger and the operator's provider bill was one
    //    `/nick` deep, and every new nick leaked a permanent map entry.
    //
    // It now runs after the mention gate and after the identity is
    // known, keyed by `throttle_key`. Nothing is lost by moving it: the
    // rolling context ring is fed by `tap_observe` from the main
    // message loop (well before this function), so the limiter never
    // gated what the model reads — only what reaches the agent.

    // IRC formatting off inbound text (display text for the model).
    let text = format::strip_irc_codes(raw_text);

    // Mention gating for channels (hermes require_mention parity).
    // The CHECK runs always, the GATE only when the channel asks for
    // it — the `.agent` prefix below needs the answer even on open
    // channels (the multi-bot rule: a second bot in the channel must
    // not act on a command addressed to a third nick).
    let mut text = text;
    let addressed = addressed_to(&conn.current_nick, &text);
    let mentioned = !is_channel || addressed.is_some();
    // A bare `tcl <code>` line is addressed to the bot by CONVENTION
    // rather than by nick, so the mention gate must not swallow it —
    // USER, 2026-09-11: "without mention-gating this one". Everything
    // else on this path (authorization, lockdown, the `Cap::Shell` tier,
    // the eval budget) still applies; this exempts the line from one
    // gate, not from the rest. `is_ours_by_convention` is shared with
    // the arm that answers, deliberately: see there for why the two
    // must never disagree.
    let bare_tcl = is_ours_by_convention(&text);
    // Whether the line goes on, and what the bot reads of it: core's
    // verdict, unless a plugin overrides `irc.addressed`
    // (PLAN-extension-api.md E4). `mentioned` stays the real mention:
    // the command rule and the context injection below read it.
    let (address, read) = if is_channel {
        crate::points::decide(crate::points::Line {
            network: &conn.network,
            channel: target,
            sender,
            nick: &conn.current_nick,
            text: &text,
            mentioned,
            require_mention: conn.channel_requires_mention(target),
            by_convention: bare_tcl,
            body: addressed.as_deref(),
        })
        .await
    } else {
        (true, text.clone())
    };
    if is_channel && !address {
        // The trigger hook: the UNADDRESSED line is exactly what
        // channel triggers exist for (link titles, timtom, image
        // descriptions react to the room, not to the bot). Everything
        // the hook needs to refuse is checked inside: the feature
        // switch, the sanctioned gate (passed above), the banned
        // verdict, the bare-tcl exemption, the derived mask. Replies
        // go out with the same pass-through formatting the bare-tcl
        // arm uses — a link title is not markdown.
        let cfg = veles_core::config::load().unwrap_or_default();
        if let Some((user, host)) = user_host_of(msg) {
            let mask = format!("{sender}!{user}@{host}");
            for reply in crate::triggers_hook::observe_text(
                conn,
                &cfg,
                &conn.network,
                target,
                sender,
                &mask,
                &text,
            )
            .await
            {
                let _ = conn.privmsg(target, &reply, FormatMode::Markdown).await;
            }
        }
        return Ok(());
    }
    if is_channel {
        // The address itself is not part of what was said: `read` is
        // the line without it (or what an `irc.addressed` override
        // answered).
        text = read;
    }

    // ── who is this? ──
    //
    // The wire mask, the network and where the line landed — built
    // HERE, before anything can return, because every gate below needs
    // it. It used to be assembled just before `send_inbound`, which is
    // after the ask-resolution and `.agent` branches had already
    // answered without it: those two paths could not see mask rules at
    // all, so a mask-recognised owner could not administer the bot.
    let sender_ctx = sender_context(conn, msg, is_channel, target, sender);
    // A Services account, when the network HAS services and this
    // sender used them. Not required for anything any more (USER,
    // 2026-09-09: "The bot should NOT have NickServ/services gating of
    // any sort, the ACL host masks are authoritative") — it is one
    // more identity an `[acl]` rule may name, and the stronger one
    // when it exists.
    let account = conn.resolve_account(msg, sender);
    if !conn.is_authorized(account.as_deref(), &sender_ctx) {
        debug!("irc: ignoring unauthorized message from {sender}");
        return Ok(());
    }

    // ── inbound rate limit (cost-spam protection — slopdrop lesson) ──
    //
    // HERE, not at the top of the function: past the mention gate, so
    // ordinary conversation does not spend it, and past authorization,
    // so a banned sender never occupies a slot in the limiter's map.
    // Keyed by `throttle_key` rather than the nick — see there.
    let tkey = throttle_key(&conn.network, account.as_deref(), msg, sender);
    match conn.inbound_limiter.allow(&tkey) {
        crate::ratelimit::Verdict::Allow => {}
        crate::ratelimit::Verdict::Throttle { notify } => {
            warn!("irc: rate-limited inbound from {sender} (key {tkey})");
            // Say it ONCE. A throttled user who is told nothing cannot
            // tell a rate limit from a broken bot; a user told every
            // time turns their own flood into ours.
            if notify {
                conn.notice(
                    sender,
                    "you are sending messages faster than I answer them — \
                     the extra ones are being dropped. Try again in a moment.",
                )
                .await?;
            }
            return Ok(());
        }
    }

    // ── lockdown ──
    //
    // The gateway has the same gate, but two things are answered HERE,
    // before the bus: an ask reply ("1") and the command sigil. Without
    // this a locked-down bot would still approve tool calls and still
    // run `/grant` for anyone who could reach the channel. USER,
    // 2026-09-09: *"only local (tui, desktop, repl) frontends are
    // allowed to do anything."*
    //
    // `/lockdown` and its sigil form are let through to the bus, which
    // answers them — a sender who was refused must be able to ask what
    // refused them, and the reply explains why `off` is not theirs to
    // give.
    {
        let body = text.trim_start();
        let sigil = conn.cfg.command_prefix.clone();
        let asks_about_lockdown = body.starts_with("/lockdown")
            || body
                .strip_prefix(sigil.as_str())
                .is_some_and(|r| r.trim_start().starts_with("lockdown"));
        if !asks_about_lockdown {
            // This network's OWN store, with no "whichever booted
            // first" fallback: every network parks the same gateway
            // state.db, so the lookup always lands in production, and
            // a security decision must not depend on boot order.
            let store = crate::client::parked_store_for(Some(&conn.network));
            if let Some(l) = veles_core::lockdown::current(store.as_ref()) {
                let subject = veles_core::acl::Principal::resolve_ctx(
                    None,
                    &sender_ctx,
                    &veles_core::acl::live_config(),
                )
                .subject;
                debug!("irc: lockdown refused {subject}");
                if veles_core::lockdown::notice_once(&subject) {
                    conn.notice(sender, &veles_core::lockdown::refusal(&l))
                        .await?;
                }
                return Ok(());
            }
        }
    }

    // Is this the answer to a question the agent is parked on? Resolve
    // it HERE and swallow the message: sending it through the inbound
    // bus would queue it behind the very turn that is waiting for it
    // (the gateway serializes per chat).
    let chat_key = format!("irc:{chat}");
    // The responder is the SUBJECT — who they are for keying, which on
    // a services-less network is their wire mask rather than nothing at
    // all. An ask is bound to the subject that opened it; passing an
    // account here meant every ask in every channel was opened by
    // "nobody" and answerable by anybody who typed "1".
    let responder = veles_core::acl::Principal::resolve_ctx(
        account
            .as_deref()
            .map(|a| format!("irc:{}", conn.casefold(a)))
            .as_deref(),
        &sender_ctx,
        &veles_core::acl::live_config(),
    )
    .subject;
    if let Some(id) = veles_core::ask::resolve_for_chat(&chat_key, &text, Some(responder.as_str()))
    {
        debug!("irc: reply from {sender} answered ask {id}");
        conn.notice(sender, &format!("decision recorded ({id})"))
            .await?;
        return Ok(());
    }

    // ── slopdrop's own spelling: a BARE `tcl <code>` line ──
    //
    // USER, 2026-09-11: *"add bare `tcl` as another alias to .tcl and
    // /tcl so that everyone who's used to just calling it like that in
    // slopdrop isn't confused/annoyed"* — and, explicitly, *"without
    // mention-gating this one"*.
    //
    // That is the one deliberate exception to the rule the sigil and
    // the literal `/name` both follow (`!is_channel || mentioned`,
    // which keeps two veles instances in one channel independently
    // addressable). It has to be: the whole point of the spelling is
    // that a room full of people with a decade of `tcl expr {1+1}` in
    // their fingers do not have to learn a new one, and "say the bot's
    // name first" is the new one.
    //
    // The cost is real and bounded: a line that HAPPENS to start with
    // "tcl " is evaluated, so "tcl is a strange language" becomes an
    // error in the channel. Slopdrop has always had exactly that
    // behaviour and the convention survived it. If two bots that both
    // answer bare `tcl` share a channel they will both reply — which is
    // an argument for the headless slopdrop topology, not for a gate.
    //
    // ONE condition was added on 2026-09-12: the bare spelling is only
    // ours when a PERSISTENT interpreter is configured. USER: *"jackma
    // tries to exec `tcl` in bash"* — what actually happened is that
    // `[slopdrop]` was unset, so `eval_line` fell back to the local
    // throwaway sandbox and answered `#coven` out of an interpreter
    // holding none of the channel's 512 stored procs. That answer is not
    // merely unhelpful, it is CONFIDENTLY WRONG: `tcl [someproc]` comes
    // back "invalid command name", which reads as "your proc is gone"
    // rather than "you are talking to the wrong interpreter". And while
    // slopdrop is still in the channel it is a second, worse answer to
    // a question already answered correctly.
    //
    // So the bare spelling reaches slopdrop's interpreter or nobody's.
    // The SIGIL spellings (`.tcl`, `/tcl`) are veles' own command and
    // keep the local sandbox unconditionally — that is the "do not burn
    // a turn on `expr {1+1}`" feature, and it is not what this arm is
    // for.
    //
    // Everything downstream is unchanged: it goes through
    // `handle_irc_command`, so the `Cap::Shell` tier, the
    // declare-a-boundary refusal and the per-caller eval budget all
    // apply exactly as they do to `.tcl`.
    {
        let body = text.trim_start().to_string();
        if is_ours_by_convention(&body) {
            let word = body
                .split_whitespace()
                .next()
                .unwrap_or("tcl")
                .to_ascii_lowercase();
            // The boundary refusal stays a NOTICE to the caller, and
            // that is not an inconsistency with the reply below: it
            // says this bot is not declared for this channel, so
            // announcing it IN the channel would be the very thing it
            // is refusing to do.
            if let Some(refusal) = command_boundary_refusal(conn, &word) {
                conn.notice(sender, &refusal).await?;
                return Ok(());
            }
            if let Some(reply) =
                handle_irc_command(conn, chat, &body, account.as_deref(), &sender_ctx).await
            {
                reply_bare_tcl(conn, is_channel, target, sender, &reply).await?;
                return Ok(());
            }
        }
    }

    // ── the command sigil ──
    //
    // `.acl list` is `/acl list`, and so is every other name: same
    // commands, same tiers, one table. USER, 2026-09-09: *"Can we fold
    // the `.agent` commands so that they just work like all the other
    // slash commands… Uniform user interface is good."* The sigil
    // exists at all because IRC clients eat a leading `/` — it is a
    // transport quirk, not a second command language.
    //
    // Multi-bot safe: in a channel a sigil line only reaches us when
    // OUR nick was mentioned (the ircx collision fix). That holds even
    // when the channel's chat gate is off — two veles instances
    // sharing a channel stay independently addressable, and a command
    // aimed at another bot's nick is not ours to act on.
    let prefix = conn.cfg.command_prefix.clone();
    if is_command_line(&prefix, &text) && (!is_channel || mentioned) {
        let body = text[prefix.len()..].trim().to_string();
        let name = body
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match handle_irc_command(conn, chat, &body, account.as_deref(), &sender_ctx).await {
            // Ours: answered as a NOTICE, outside any turn.
            Some(reply) => {
                conn.notice(sender, &reply).await?;
                return Ok(());
            }
            // Not ours, but a real command: hand it to the one shared
            // dispatcher by spelling it the way every other surface
            // does. The gateway answers it as a normal reply — after
            // the same boundary check the immediate path applies,
            // because the gateway's dormant mode would resolve a
            // stranger on an undeclared bot as the owner.
            None if known_command(&name) => {
                if let Some(refusal) = command_boundary_refusal(conn, &name) {
                    conn.notice(sender, &refusal).await?;
                    return Ok(());
                }
                text = format!("/{body}");
            }
            // Not a command at all. Saying so beats forwarding
            // `/frobnicate` to the model as a prompt, which is what
            // the gateway would otherwise do with it.
            None => {
                conn.notice(
                    sender,
                    &format!(
                        "unknown command: {name} — `{prefix}help` lists them. \
                         (`{prefix}agent <verb>` is gone: the sigil IS the command now, \
                         so it is `{prefix}acl list`, not `{prefix}agent acl list`.)"
                    ),
                )
                .await?;
                return Ok(());
            }
        }
    }

    // A literal `/name` is the same command line with the other
    // spelling, so it takes the same boundary check. Some clients let
    // an unknown `/word` through to the server, and before the sigil
    // fold that path skipped the check the `.agent` path applied.
    if let Some(name) = text
        .strip_prefix('/')
        .map(|r| {
            r.split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase()
        })
        .filter(|n| known_command(n))
    {
        if let Some(refusal) = command_boundary_refusal(conn, &name) {
            conn.notice(sender, &refusal).await?;
            return Ok(());
        }
        // …and a name this adapter answers ITSELF is answered here in
        // both spellings, not just the sigil. Routing `/tcl` through the
        // bus instead would queue it behind the per-chat turn lock and,
        // worse, hand a different `full` decision to a different code
        // path. `IRC_IMMEDIATE` is the list of things that cannot wait;
        // which sigil the client ate is not a reason to make them.
        if IRC_IMMEDIATE.contains(&name.as_str()) {
            let body = text.trim_start().trim_start_matches('/').to_string();
            if let Some(reply) =
                handle_irc_command(conn, chat, &body, account.as_deref(), &sender_ctx).await
            {
                // A NOTICE, outside any turn — the same answer the
                // sigil spelling gets, from the same call.
                conn.notice(sender, &reply).await?;
                return Ok(());
            }
        }
    }

    // Dedup key: msgid tag when the server provides one (ergo does for
    // chathistory), else server-time + content. WHY time, not content
    // alone: redeliveries share the original server-time, so the key is
    // stable across replays; but two legitimately different messages
    // with identical text at different moments get different keys.
    // Content-only hashing would eat genuine repeated messages.
    let msg_id = msg
        .tags
        .as_ref()
        .and_then(|tags| {
            tags.iter()
                .find(|t| t.0 == "msgid")
                .and_then(|t| t.1.clone())
        })
        .unwrap_or_else(|| {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let time_tag = msg
                .tags
                .as_ref()
                .and_then(|tags| {
                    tags.iter()
                        .find(|t| t.0 == "time")
                        .and_then(|t| t.1.clone())
                })
                .unwrap_or_default();
            let mut h = DefaultHasher::new();
            sender.hash(&mut h);
            target.hash(&mut h);
            time_tag.hash(&mut h);
            text.hash(&mut h);
            format!("h{:x}", h.finish())
        });

    // No bus means this connection does not feed the agent. The admin's
    // presence takes this path for every line it sees, including the
    // human's own.
    let Some(bus) = conn.bus.clone() else {
        return Ok(());
    };
    bus.send_inbound(InboundMessage {
        platform: "irc".into(),
        // WHICH network this came in on, so the reply goes back out the
        // same connection. `platform` stays "irc" — that is what the ACL
        // scopes on and what `[display.platforms.irc]` is keyed by, and
        // neither should fork per network. Before this, two networks
        // registered one outbound channel under one key and the second
        // adapter to start silently took every reply.
        route: Some(conn.network.clone()),
        chat_id: chat.to_string(),
        msg_id: Some(msg_id),
        dedup_id: None,
        sender: match &account {
            Some(acc) => format!("{sender} ({acc})"),
            None => sender.clone(),
        },
        // The Services account, when the network has services and this
        // sender used them — one more identity a rule may name, and
        // NOT a requirement. What identifies a sender on a
        // services-less network is `sender_ctx.mask`, and what tells
        // two of them apart downstream is the principal's `subject`.
        sender_id: account
            .as_deref()
            .map(|acc| format!("irc:{}", conn.casefold(acc))),
        sender_ctx,
        text,
        // What the turn is walking into (platforms.irc.
        // inject_channel_context, ON by default — `IrcConfig::
        // default`): where it is, who it is, what the room is about,
        // who is in it, and what has been said since it last looked.
        // It arrives in the transcript as the ⇒ labelled `irc_context`
        // injection, not as the user's own words.
        //
        // Only for a line that ADDRESSED us. On a channel with
        // `require_mention = false` every line starts a turn, and
        // attaching the window to all of them fed the model a
        // multi-kilobyte block per line of chatter — the single largest
        // consumer of a capped caller's budget.
        context: if mentioned && conn.cfg.inject_channel_context {
            channel_context(
                &conn.network,
                &conn.presence,
                chat,
                sender,
                conn.cfg.context_max_lines,
                conn.cfg.context_max_chars,
            )
        } else {
            None
        },
    });
    Ok(())
}

/// The context block for one addressed line, and the bookkeeping that
/// keeps it from being paid for twice.
///
/// `chat` is the buffer the conversation lives in — the channel, or
/// the other party for a query. Three things happen here that did not
/// before:
///
/// * a query gets a block at all (it was gated on `is_channel`, so a
///   DM turn could not tell which network it was answering on);
/// * only lines NEWER than the ones this conversation was already
///   shown are attached, so a six-turn exchange stops paying six times
///   for the same window;
/// * a cold ring (the first turn after a gateway restart) falls back to
///   the durable scrollback instead of returning nothing.
fn channel_context(
    network: &str,
    presence: &str,
    chat: &str,
    sender: &str,
    max_lines: usize,
    max_chars: usize,
) -> Option<String> {
    let key = crate::tap::context_mark_key(network, chat, sender);
    let after = crate::tap::context_mark(&key);
    // Only a COLD conversation can use the store, and only then is the
    // query worth making.
    let backlog = if after == 0 {
        crate::client::parked_store_for(Some(network))
            .and_then(|s| {
                let g = s.lock().ok()?;
                g.irc_backlog(
                    network,
                    presence,
                    chat,
                    None,
                    max_lines.min(u32::MAX as usize) as u32,
                )
                .ok()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let block = crate::tap::context_block(network, chat, max_lines, max_chars, after, &backlog)?;
    // A cold block's lines came from the store, whose ids are not ring
    // seqs — leaving the mark alone means the next turn reads the ring
    // from the beginning, which is right: it has not been shown those.
    if !block.cold && block.newest > after {
        crate::tap::set_context_mark(&key, block.newest);
    }
    Some(block.text)
}

#[cfg(test)]
mod channel_context_tests {
    use super::*;

    #[test]
    fn a_conversation_pays_for_each_line_once() {
        // Re-sending the whole window every turn meant a six-turn
        // exchange paid for six near-identical multi-kB blocks, mostly
        // duplicate text. The bookkeeping is HERE, in the adapter, so
        // it is pinned here and not only in the tap.
        let net = "ctxnet";
        let buf = "#ctx";
        let pub_line = |nick: &str, text: &str| {
            crate::tap::publish(crate::tap::event(
                net,
                crate::tap::PRESENCE_BOT,
                buf,
                crate::tap::EventKind::Privmsg,
                nick,
                text,
            ));
        };
        crate::tap::forget_context_mark(&crate::tap::context_mark_key(net, buf, "wrath"));
        pub_line("wrath", "first thing");
        let first = channel_context(net, crate::tap::PRESENCE_BOT, buf, "wrath", 50, 64 * 1024)
            .expect("a cold turn gets a block");
        assert!(first.contains("first thing"), "{first}");

        pub_line("erasmus", "second thing");
        let second = channel_context(net, crate::tap::PRESENCE_BOT, buf, "wrath", 50, 64 * 1024)
            .expect("a second turn still gets a block (the header)");
        assert!(
            second.contains("second thing"),
            "the new line is attached: {second}"
        );
        assert!(
            !second.contains("first thing"),
            "the line already shown is not re-sent: {second}"
        );

        // A DIFFERENT person in the same buffer has their own mark, so
        // their first turn still sees the whole window. In a shared
        // channel that is the difference between context and silence.
        crate::tap::forget_context_mark(&crate::tap::context_mark_key(net, buf, "erasmus"));
        let theirs = channel_context(net, crate::tap::PRESENCE_BOT, buf, "erasmus", 50, 64 * 1024)
            .expect("their first turn");
        assert!(
            theirs.contains("first thing") && theirs.contains("second thing"),
            "a second caller's cold turn is not short-changed by the first caller's mark: {theirs}"
        );
    }
}

/// `<sigil>help [topic]` — the IRC rendering of the one command table.
///
/// Paged, not longer. The flat listing already outgrew one IRC line
/// and split into two NOTICEs; with the sandbox verbs, the history
/// filters and the ask family it would be four or five, which is a
/// flood the channel did not ask for. So the bare form names the
/// topics, each topic is one line, and `all` prints the whole engine
/// table for a caller who explicitly asked for it.
///
/// `every_help_topic_names_real_commands` keeps these lines honest
/// against `builtin_cap`, which is the inventory: a topic page that
/// advertises a command nobody implements is the drift this fold
/// existed to remove, in a new place.
/// `principal` filters the `all` page to what this caller can run —
/// `None` prints the whole table, which is what the unit tests want
/// and what a bot with no declared boundary has to say anyway.
fn irc_help(p: &str, topic: &str, principal: Option<&veles_core::acl::Principal>) -> String {
    match topic.trim_start_matches("--").to_lowercase().as_str() {
        "" => format!(
            "`{p}<cmd>` runs any command — same names, same tiers, as `/<cmd>` here or on \
             any other surface. {p}help <topic> — topics: session, history, acl, sandbox, \
             chat. {p}help all — every command."
        ),
        "session" => format!(
            "{p}irc — connection state | {p}irc nick <n> — change nick | \
             {p}asks — open questions | {p}approve <id> | {p}deny <id> | \
             {p}answer <id> <choice> | {p}todos — the current plan | \
             {p}checkpoints — undoable file edits | {p}restore <id> | \
             {p}tcl <code> — evaluate Tcl (owner)"
        ),
        "history" => format!(
            "{p}irc history <text> [--nick n] [--since t] [--until t] [--limit n] [--all] \
             [--tail] — search this buffer's stored scrollback. `--all` searches every \
             buffer on this network; `--tail` needs no text. Times are RFC3339 or any \
             prefix (2026-09-09, 2026-09-09T14)."
        ),
        "acl" => format!(
            "{p}acl — your own role, caps and ceiling | {p}caps — the same, shorter | \
             {p}acl list — the rules (admin) | \
             {p}acl explain <id|mask> [net] [chan] — why somebody has the role they have | \
             {p}acl export — the live rules as TOML | \
             {p}acl promote — the store's grants become config lines (owner) | \
             {p}grant <id|mask> <role> | \
             {p}revoke <id|mask> | {p}acl user save <name> <role> <id|mask>… (owner) | \
             {p}limits [set <role> <n>] | {p}lockdown [on|off] — refuse every connector"
        ),
        "sandbox" => format!(
            "{p}sandbox where — which box you land in | {p}sandbox use <name> — pick one \
             of yours | {p}sandboxes — list them (admin) | {p}sandbox create <name> \
             [--image i] | {p}sandbox assign <name> --role guest --channel #c | \
             {p}sandbox unassign … | {p}sandbox purge <name> [--work] (admin)"
        ),
        "chat" => format!(
            "{p}model, {p}status, {p}usage, {p}context, {p}compact, {p}new, {p}queue, \
             {p}cancel — the session commands, identical on every surface. {p}help all \
             lists them; a leading `/` works too, for clients that let one through."
        ),
        "all" | "commands" | "everything" => veles_frontends::commands::help_text_for(
            veles_frontends::commands::Surface::Gateway,
            principal,
        )
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | "),
        other => {
            format!("no help topic {other:?} — try: session, history, acl, sandbox, chat, all")
        }
    }
}

#[cfg(test)]
mod irc_help_tests {
    use super::*;

    #[test]
    fn the_bare_listing_stays_one_irc_line() {
        // The flat listing had already split into two NOTICEs, and the
        // sandbox verbs plus the history filters plus the ask family
        // would have made it four or five. A help flood is still a
        // flood.
        let base = irc_help(".", "", None);
        assert!(
            base.len() < 400,
            "the bare listing must fit one IRC line ({} chars): {base}",
            base.len()
        );
        for t in ["session", "history", "acl", "sandbox", "chat", "all"] {
            assert!(base.contains(t), "the topic list must name {t}: {base}");
        }
        assert!(
            base.contains("same names, same tiers"),
            "the fold's whole promise belongs in the first line: {base}"
        );
    }

    #[test]
    fn every_named_topic_answers_and_carries_the_sigil() {
        for t in ["session", "history", "acl", "sandbox", "chat"] {
            let out = irc_help("!", t, None);
            assert!(
                out.contains('!'),
                "{t} must spell commands with the configured sigil: {out}"
            );
            assert!(out.len() < 520, "{t} must fit one line: {out}");
        }
    }

    /// The topic pages are prose, and prose drifts. Every command name
    /// they advertise must be one the engine actually has — the same
    /// rule `cmdstest.tcl` applies to the desktop's table.
    #[test]
    fn every_help_topic_names_real_commands() {
        for t in ["session", "history", "acl", "sandbox", "chat"] {
            let page = irc_help(".", t, None);
            for word in page.split_whitespace() {
                let Some(name) = word.strip_prefix('.') else {
                    continue;
                };
                let name = name.trim_end_matches([',', '|', '.', ':', ';']);
                // `.agent history` is spelled `.irc history`; the
                // second word is a SUBcommand, not a command.
                if name.is_empty() || name.contains('<') {
                    continue;
                }
                assert!(
                    veles_core::acl::builtin_cap(name).is_some() || IRC_COMMANDS.contains(&name),
                    "help topic {t} advertises `.{name}`, which is not a command: {page}"
                );
            }
        }
    }

    #[test]
    fn an_unknown_topic_lists_the_real_ones() {
        let out = irc_help(".", "frobnicate", None);
        assert!(
            out.contains("no help topic") && out.contains("sandbox"),
            "{out}"
        );
        // `--history` and `history` are the same ask: people type both.
        assert_eq!(
            irc_help(".", "--history", None),
            irc_help(".", "HISTORY", None)
        );
    }

    /// …and `all` is the engine's own table, not a copy of it.
    #[test]
    fn the_all_page_is_the_engines_table() {
        let all = irc_help(".", "all", None);
        for c in ["/model", "/queue", "/sandbox", "/lockdown"] {
            assert!(
                all.contains(c),
                "`{c}` missing from the full listing: {all}"
            );
        }
    }
}

/// Parse `<sigil>irc history` arguments.
///
/// `<text>` searches for text; `--nick n` reads one person's lines;
/// `--since`/`--until` bound the time; `--limit n` sets how many;
/// `--all` widens from this buffer to every buffer on the network;
/// `--tail` drops the text requirement entirely.
///
/// Returns the usage line when the arguments would search for nothing
/// at all — a bare `irc history` with no filter is a request for the
/// whole table, not a question.
fn parse_history_args(buffer: &str, args: &str) -> Result<veles_core::store::IrcQuery, String> {
    let mut q = veles_core::store::IrcQuery {
        buffer: Some(buffer.to_string()),
        limit: 25,
        ..veles_core::store::IrcQuery::default()
    };
    let mut needle: Vec<&str> = Vec::new();
    let mut tail = false;
    let mut it = args.split_whitespace().peekable();
    while let Some(tok) = it.next() {
        match tok {
            "--nick" | "--from" => q.nick = it.next().map(str::to_string),
            "--since" | "--after" => q.since = it.next().map(str::to_string),
            "--until" | "--before" => q.until = it.next().map(str::to_string),
            "--limit" | "-n" => {
                // 0 is not "no lines" — it is somebody who meant the
                // default. Garbage is the same.
                if let Some(n) = it.next().and_then(|v| v.parse::<u32>().ok()) {
                    if n > 0 {
                        q.limit = n.min(200);
                    }
                }
            }
            "--all" => q.buffer = None,
            "--tail" => tail = true,
            other => needle.push(other),
        }
    }
    if !needle.is_empty() {
        q.needle = Some(needle.join(" "));
    }
    let has_filter =
        q.needle.is_some() || q.nick.is_some() || q.since.is_some() || q.until.is_some();
    if !has_filter && !tail {
        return Err(
            "usage: history <text> [--nick n] [--since t] [--until t] [--limit n] [--all] \
             — searches this buffer's stored scrollback (case-insensitive). \
             `--all` searches every buffer on the network; `--tail` reads the last \
             lines with no text filter. Times are RFC3339 or any prefix (2026-09-09)."
                .to_string(),
        );
    }
    Ok(q)
}

#[cfg(test)]
mod history_args_tests {
    use super::*;

    #[test]
    fn a_bare_needle_searches_this_buffer() {
        let q = parse_history_args("#veles", "the build broke").expect("a needle is a question");
        assert_eq!(q.needle.as_deref(), Some("the build broke"));
        assert_eq!(q.buffer.as_deref(), Some("#veles"));
        assert_eq!(q.limit, 25, "the default is stated, not hard-coded twice");
        assert!(q.nick.is_none() && q.since.is_none());
    }

    #[test]
    fn every_axis_parses_and_the_needle_survives_around_it() {
        let q = parse_history_args(
            "#veles",
            "--nick erasmus deploy --since 2026-09-09 --limit 5 froze --all",
        )
        .expect("parses");
        assert_eq!(q.nick.as_deref(), Some("erasmus"));
        assert_eq!(q.since.as_deref(), Some("2026-09-09"));
        assert_eq!(q.limit, 5);
        assert_eq!(q.buffer, None, "--all widens to the whole network");
        assert_eq!(
            q.needle.as_deref(),
            Some("deploy froze"),
            "the words that are not flags are the needle, in order"
        );
    }

    #[test]
    fn a_limit_is_clamped_rather_than_trusted() {
        assert_eq!(parse_history_args("#c", "x --limit 0").unwrap().limit, 25);
        assert_eq!(
            parse_history_args("#c", "x --limit 9999").unwrap().limit,
            200
        );
        assert_eq!(
            parse_history_args("#c", "x --limit nonsense")
                .unwrap()
                .limit,
            25,
            "garbage leaves the default rather than erroring at the user"
        );
    }

    #[test]
    fn a_filterless_ask_gets_the_usage_line_not_the_whole_table() {
        let err = parse_history_args("#veles", "").expect_err("no filter is not a question");
        assert!(err.starts_with("usage: history"), "{err}");
        // …unless it explicitly asks for the tail.
        let tail = parse_history_args("#veles", "--tail").expect("tail needs no needle");
        assert!(tail.needle.is_none() && tail.buffer.as_deref() == Some("#veles"));
        // A nick alone is a question too: "what has erasmus said here".
        let who = parse_history_args("#veles", "--nick erasmus").expect("a nick is a filter");
        assert_eq!(who.nick.as_deref(), Some("erasmus"));
    }
}

/// Is this line a command, or somebody talking?
///
/// The sigil is one character by default, so the question is real:
/// `"...anyway"`, `".oO(hmm)"` and `".42"` all start with `.` and none
/// of them is a command. The rule is that the first token has to LOOK
/// like a command name — a letter, then letters, digits, `-` or `_`
/// — which is enough to tell those three apart and cheap enough to be
/// obviously right. A multi-character sigil (`!bot `, or the old
/// `.agent`) needs no such care, but the rule is harmless there.
fn is_command_line(prefix: &str, text: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    let Some(rest) = text.strip_prefix(prefix) else {
        return false;
    };
    let Some(word) = rest.split_whitespace().next() else {
        return false;
    };
    word.starts_with(|c: char| c.is_ascii_alphabetic())
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Does this name belong to a command at all?
///
/// The inventory, in the one place it is already written down:
/// `builtin_cap` (every shared and gateway command — a name without a
/// tier there is a test failure), this adapter's own list, the user's
/// custom commands on disk, and the config-declared aliases. Asked so an
/// unknown name is ANSWERED rather than forwarded to the model as a
/// prompt.
///
/// The alias check loads config, so it goes LAST — the three cheap
/// lookups in front of it answer every builtin without touching the
/// disk, and only a word none of them claims pays for the read.
fn known_command(name: &str) -> bool {
    !name.is_empty()
        && (veles_core::acl::builtin_cap(name).is_some()
            || IRC_COMMANDS.contains(&name)
            || veles_core::commands::find_any(name).is_some()
            || veles_core::config::load()
                .map(|c| veles_core::aliases::is_alias(&c, name))
                .unwrap_or(false))
}

/// Commands the IRC adapter answers ITSELF, before the bus.
///
/// Not a preference — a requirement. The gateway serialises a chat's
/// messages behind a per-chat lock that a running turn holds, so
/// anything routed through it waits for that turn to finish. Three
/// families cannot wait:
///
/// - the ASK verbs, because the turn that is running is the one
///   waiting for the answer (routing them would deadlock);
/// - the ACL/sandbox/limits/lockdown verbs, because they are what you
///   reach for WHILE something is going wrong;
/// - the connection verbs (`irc`, `tcl`), which need the live `Conn`
///   and have no meaning off this adapter.
///
/// Everything else is forwarded as `/<cmd>` and answered by the one
/// shared dispatcher, which is the point of the fold. `/cancel` and
/// `/stop` are absent on purpose: the gateway answers those BEFORE its
/// own lock already.
const IRC_IMMEDIATE: &[&str] = &[
    "help",
    "caps",
    "asks",
    "answer",
    "approve",
    "deny",
    "acl",
    "access",
    "grant",
    "revoke",
    "limits",
    "sandbox",
    "sandboxes",
    "lockdown",
    "irc",
    "tcl",
    "eval",
    "tcladmin",
];

/// Commands that exist ONLY on this surface, because they need the
/// live connection: `irc` (state, scrollback, nick) and `tcl` (the
/// channel's evaluator). Declared so `irc_help`'s pages can be checked
/// against the engine's inventory plus this list, and so a name added
/// here without a tier is a test failure.
const IRC_COMMANDS: &[&str] = &["irc", "tcl", "eval", "tcladmin"];

/// Reporting that stays open even on an UNDECLARED bot: what the bot
/// is (`irc`), what commands exist (`help`), what questions are
/// pending (`asks`), and the caller's own level (`caps`).
///
/// Everything else is refused until a boundary exists, because with no
/// `[acl]` section dormant mode resolves every stranger as owner —
/// see `privileged_refusal`. `caps` is in here and `acl` is not, which
/// looks arbitrary until you read what each ANSWERS: `caps` reports
/// the caller's own level (including "dormant, so nobody is
/// privileged"), while `acl` reads and writes the rule table.
const OPEN_ON_AN_UNDECLARED_BOT: &[&str] = &["help", "caps", "asks", "irc"];

/// The "declare a boundary first" refusal, for one command name.
///
/// Applied to BOTH spellings of a command line — the sigil and a
/// literal `/name` — because after the sigil fold they are the same
/// thing. That is a fix, not a side effect: the old `.agent` path
/// checked this and the plain `/name` path did not, so on an
/// undeclared bot with an open transport a stranger could send
/// `/restore 1` and dormant mode resolved them as the owner.
fn command_boundary_refusal(conn: &Conn, name: &str) -> Option<String> {
    if OPEN_ON_AN_UNDECLARED_BOT.contains(&name) {
        return None;
    }
    privileged_refusal(conn).map(|why| format!("{name}: {why}"))
}

/// The capability each IRC-answered command needs at the transport.
///
/// Reporting stays open (that is what open mode is FOR); acting needs
/// the capability that matches the damage. This table replaced a
/// blanket "needs a registered account" refusal on 2026-09-09: the
/// account requirement was services gating, and the ACL host masks are
/// authoritative now — but the eggdrop lesson underneath it
/// (CVE-2004-0274: a command prefix whose authorization was coarser
/// than what its subcommands could do) is exactly as true, so every
/// acting command names its own capability instead.
fn irc_command_cap(cmd: &str) -> Option<veles_core::acl::Cap> {
    use veles_core::acl::Cap;
    // The shared names answer to the SAME tier they answer to on every
    // other surface — `builtin_cap` is the one table, and a second
    // opinion here is how a surface becomes the soft way in.
    if let Some(cap) = veles_core::acl::builtin_cap(cmd) {
        return Some(cap);
    }
    Some(match cmd {
        // Connection state and stored scrollback: a report. `irc nick`
        // changes the bot's identity on the network and is checked at
        // the arm, which is where the subcommand is known.
        "irc" => Cap::CmdStatus,
        // Code execution. The safe interp for anyone with `shell`; the
        // full tclsh is owner-only and gated separately at the arm.
        "tcl" | "eval" => Cap::Shell,
        _ => return None,
    })
}

/// Resolve the IRC caller's ACL principal exactly as the gateway does
/// when IT enforces: identity = `irc:<casefolded account>`, resolved
/// against the global [acl] config. When config cannot be read, fall
/// back to the legacy mode the gateway itself applies — a dormant ACL
/// resolves every allowlisted sender as owner, which is today's
/// pre-config behavior preserved.
fn irc_principal(
    account: Option<&str>,
    conn: &Conn,
    ctx: &veles_core::acl::SenderContext,
) -> veles_core::acl::Principal {
    let id = account.map(|a| format!("irc:{}", conn.casefold(a)));
    veles_core::acl::Principal::resolve_ctx(id.as_deref(), ctx, &veles_core::acl::live_config())
}

/// Can anyone be privileged here at all?
///
/// With no `[acl]` section the ACL is dormant and resolves EVERY
/// sender as owner. That is the documented legacy posture for a
/// single-user bot behind an allowlist — but on an open transport it
/// would hand `.agent grant` to whoever spoke first, which is the
/// eggdrop mistake with the serial numbers filed off. The `tcl` arm
/// has always refused the full interpreter for this reason; every
/// acting subcommand now does.
///
/// The answer is not "identify to services" (there may be none) — it
/// is "declare a boundary", and the refusal says which two.
fn privileged_refusal(conn: &Conn) -> Option<String> {
    let acl = veles_core::acl::live_config();
    if acl.is_configured() || !conn.cfg.allowed_users.is_empty() {
        return None;
    }
    Some(
        "this bot has no [acl] section and an open transport, so nobody is privileged — \
         the owner must declare themselves in config.toml ([acl] owner = [\"nick!ident@host\"]) \
         or set an allowlist"
            .to_string(),
    )
}

/// The commands this adapter answers itself. `None` = not mine —
/// forward it to the shared dispatcher as `/<cmd>`.
///
/// This used to be `handle_agent_command`, reached by a two-word
/// `.agent <verb>` prefix with its own verb table, its own tiers and
/// its own copies of `/acl`, `/todos`, `/checkpoints` and `/restore`.
/// USER, 2026-09-09: *"Can we fold the `.agent` commands so that they
/// just work like all the other slash commands. I don't think it's
/// very useful to have them separated, unless you can see a good
/// reason. Uniform user interface is good."* There was no good reason,
/// so the prefix is a plain command sigil now and this function is
/// only the part that CANNOT go through the bus (see `IRC_IMMEDIATE`).
async fn handle_irc_command(
    conn: &mut Conn,
    target: &str,
    body: &str,
    account: Option<&str>,
    ctx: &veles_core::acl::SenderContext,
) -> Option<String> {
    let mut parts = body.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("").to_lowercase();
    if !IRC_IMMEDIATE.contains(&cmd.as_str()) {
        return None;
    }
    // ── authorization: capabilities, not accounts ──
    if let Some(cap) = irc_command_cap(&cmd) {
        // Including the cmd-status self-reports that answer a question
        // about PRIVILEGE. It is tempting to let a bare `acl` through
        // as harmless, but on an undeclared bot dormant mode resolves
        // every stranger as owner — so the honest answer to "what may
        // I do here" is not "you are the owner with all caps", it is
        // "no boundary is declared, so nobody is privileged", which is
        // what this returns.
        if let Some(refusal) = command_boundary_refusal(conn, &cmd) {
            return Some(refusal);
        }
        let principal = irc_principal(account, conn, ctx);
        if !principal.has(cap) {
            return Some(format!(
                "[acl] `{cmd}` requires the `{}` capability — you are {}. \
                 `{p}caps` shows your own level.",
                cap.as_str(),
                principal.label(),
                p = conn.cfg.command_prefix
            ));
        }
    }
    // Tcl is answered HERE, ahead of the bus, but it has no IRC-owned
    // match arm. The shared no-session dispatcher owns the command and
    // receives the caller's real principal and sender context. In
    // particular this must not construct a throwaway owner Agent or wait
    // for the gateway's per-chat turn lock.
    if ["tcl", "eval", "tcladmin"].contains(&cmd.as_str()) {
        let principal = irc_principal(account, conn, ctx);
        let store = match irc_store(conn) {
            Ok(s) => s,
            Err(e) => return Some(e),
        };
        let fallback = veles_core::acl::live_config();
        let actx = veles_frontends::commands::AclCtx {
            store: &store,
            fallback: &fallback,
            session_id: "",
            principal: Some(&principal),
            sender: Some(ctx),
        };
        return veles_frontends::commands::dispatch_async_as(
            &actx,
            &format!("/{cmd}"),
            parts.next().unwrap_or(""),
        )
        .await
        .map(|handled| handled.text);
    }
    Some(match cmd.as_str() {
        // `help` carries no capability — asking what a bot does is
        // not privileged — but the ANSWER is tiered: the `all` page is
        // filtered to this caller, so a guest is not handed a menu of
        // refusals. Resolved here rather than above because the cap
        // branch never ran for an open command.
        "help" => {
            let principal = irc_principal(account, conn, ctx);
            irc_help(
                &conn.cfg.command_prefix,
                parts.next().unwrap_or("").trim(),
                Some(&principal),
            )
        }
        // The connection itself: state, its stored scrollback, and the
        // bot's nick. `/status` is the SESSION's status on every
        // surface, so this could not keep that name — and the three
        // things here genuinely need the live `Conn`.
        "irc" => {
            let rest = parts.next().unwrap_or("").trim().to_string();
            let (sub, args) = match rest.split_once(char::is_whitespace) {
                Some((s, a)) => (s.to_ascii_lowercase(), a.trim().to_string()),
                None => (rest.to_ascii_lowercase(), String::new()),
            };
            match sub.as_str() {
                "" | "status" | "state" => format!(
                    "veles-irc: network={}, nick={}, channels={}, caps={:?}, sasl={}, limits={:?}",
                    conn.network,
                    conn.current_nick,
                    conn.channels.len(),
                    conn.caps,
                    conn.sasl_done,
                    conn.limits
                ),
                "history" | "search" => {
                    // Scope defaults to the buffer the command arrived
                    // in — a channel searches itself, a PM searches
                    // that query window — and `--all` widens it to the
                    // whole network.
                    match parse_history_args(target, &args) {
                        Err(usage) => usage,
                        Ok(q) => match crate::client::parked_store_for(Some(&conn.network))
                            .or_else(crate::client::parked_store)
                        {
                            None => "history search unavailable (the gateway store is not parked)"
                                .to_string(),
                            Some(store) => conn.history_reply(&store, &q),
                        },
                    }
                }
                "nick" => {
                    // The bot's identity on the network: cmd-admin,
                    // checked here because the tier at the NAME is the
                    // report's — and `irc` is in
                    // OPEN_ON_AN_UNDECLARED_BOT for the report's sake,
                    // so the boundary check belongs here too.
                    if let Some(refusal) = privileged_refusal(conn) {
                        return Some(format!("irc nick: {refusal}"));
                    }
                    let principal = irc_principal(account, conn, ctx);
                    if !principal.has(veles_core::acl::Cap::CmdAdmin) {
                        format!(
                            "[acl] `irc nick` requires the `cmd-admin` capability — you are {}.",
                            principal.label()
                        )
                    } else if args.is_empty() {
                        format!("usage: {}irc nick <newnick>", conn.cfg.command_prefix)
                    } else {
                        let n = args.split_whitespace().next().unwrap_or("").to_string();
                        conn.desired_nick = n.clone();
                        match conn.send_line(Command::NICK(n.clone()), true).await {
                            Ok(()) => format!("nick change requested: {n}"),
                            Err(e) => format!("error: {e:#}"),
                        }
                    }
                }
                other => format!(
                    "usage: {p}irc [status | history <text> … | nick <n>]  (got `{other}`)",
                    p = conn.cfg.command_prefix
                ),
            }
        }
        //
        // `caps` is open to any caller (knowing your own tier is not a
        // privilege). `acl` lists grants (admin floor). `grant` and
        // `revoke` mutate them and are OWNER-ONLY: the same Cap::All
        // gate /pair's management subcommands use in the gateway, so
        // IRC and telegram cannot drift apart on who may change roles.
        "caps" => {
            let principal = irc_principal(account, conn, ctx);
            format!(
                "you are {} · caps: {}",
                principal.label(),
                if principal.caps.is_empty() {
                    "(none)".into()
                } else {
                    principal
                        .caps
                        .iter()
                        .map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                }
            )
        }
        // ── the ACL, the sandboxes and the ceilings ──
        //
        // ONE implementation, reached from here. These four verbs used
        // to be hand-rolled copies of what `frontends::commands` does
        // — a grant listing that read the store directly, a sandbox
        // lister that knew about container keys but not about sandbox
        // NAMES, no `explain` at all, no `limits` at all — and the
        // copies had already drifted from every other surface. The
        // tiering (open self-report, cmd-admin to read the rules,
        // owner to change them) lives in the shared code too, so IRC
        // cannot become the loose way in.
        "acl" | "access" | "grant" | "revoke" | "limits" | "sandbox" | "sandboxes" | "lockdown" => {
            let principal = irc_principal(account, conn, ctx);
            let rest = parts.next().unwrap_or("").trim().to_string();
            let store = match irc_store(conn) {
                Ok(s) => s,
                Err(e) => return Some(e),
            };
            // `acl::live_config()`, not `config::load()`: the full load
            // parses every plugin and provider on every call, which on
            // this path is a per-command tax for one section.
            let fallback = veles_core::acl::live_config();
            let actx = veles_frontends::commands::AclCtx {
                store: &store,
                fallback: &fallback,
                // No session: these answer outside any turn, so
                // `sandbox where` resolves from the caller and the
                // channel instead (see `AclCtx::sender`).
                session_id: "",
                principal: Some(&principal),
                sender: Some(ctx),
            };
            match cmd.as_str() {
                "grant" | "revoke" => {
                    veles_frontends::commands::acl_command(&actx, &format!("{cmd} {rest}"))
                }
                "limits" => veles_frontends::commands::limits_command(&actx, &rest),
                "sandbox" | "sandboxes" => {
                    let rest = if cmd == "sandboxes" && rest.is_empty() {
                        "list".to_string()
                    } else {
                        rest
                    };
                    veles_frontends::commands::sandbox_command(&actx, &rest).await
                }
                // `Surface::Gateway`: a connector may ENGAGE the brake
                // and may not lift it, whatever their role. The
                // command says so; this arm does not get to decide it.
                "lockdown" => veles_frontends::commands::lockdown_command(
                    &actx,
                    veles_frontends::commands::Surface::Gateway,
                    &rest,
                ),
                _ => veles_frontends::commands::acl_command(&actx, &rest),
            }
        }
        // ── the pending questions ──
        //
        // Answered HERE and not through the bus because the turn that
        // is running is the one waiting for the answer: routing these
        // through the gateway's per-chat lock would queue the answer
        // behind the turn it unblocks. The colloquial "1" is resolved
        // even earlier, in `handle_privmsg`, for the same reason.
        //
        // Scope is this buffer, and `Decider::Sender` is the caller's
        // SUBJECT — so the capability gets them as far as the verb and
        // the ask's own binding decides whether this one is theirs.
        "asks" | "answer" | "approve" | "deny" => {
            let principal = irc_principal(account, conn, ctx);
            let rest = parts.next().unwrap_or("").trim().to_string();
            veles_frontends::commands::ask_command(
                Some(&principal),
                Some(&format!("irc:{target}")),
                veles_core::ask::Decider::principal(&principal),
                &cmd,
                &rest,
            )
        }
        // Unreachable: `IRC_IMMEDIATE` is the guard at the top, and
        // `irc_immediate_and_the_arms_agree` pins that the two lists
        // are the same set. Kept as a loud answer rather than a silent
        // fallthrough in case they ever diverge.
        other => format!(
            "`{other}` is listed as an immediate IRC command but has no arm — this is a bug"
        ),
    })
}

/// The gateway's store for the ACL-family commands, or a fresh handle
/// on the same file when this adapter never parked one (the standalone
/// `veles irc` path).
fn irc_store(conn: &Conn) -> Result<ParkedStore, String> {
    if let Some(s) =
        crate::client::parked_store_for(Some(&conn.network)).or_else(crate::client::parked_store)
    {
        return Ok(s);
    }
    match veles_core::store::Store::open(&veles_core::config::veles_home().join("state.db")) {
        Ok(s) => Ok(std::sync::Arc::new(std::sync::Mutex::new(s))),
        Err(e) => Err(format!("store unavailable: {e}")),
    }
}

// ── outbound: gateway replies → IRC ──────────────────────────────────

/// Assemble the IRC body for a reply: optional reasoning blockquote
/// (`show_reasoning` — OFF by default on IRC: channels are not the
/// place for a thinking dump, and every dump is a multi-kilobyte send
/// that eats the flood budget; telegram's knob lives separately in
/// `display.platforms.telegram`), then the answer, then the route
/// footer (`show_route` — ON by default, and the only thing that
/// suppresses it; a quiet channel says so with that key or with
/// `[mechanisms.platforms.irc] verbosity = "quiet"`). Media markers in the
/// answer are delivered on the way through (`media_body`): a link when
/// `media_public_dir` and `media_public_url` are both set, a note when they
/// are not. Split from `handle_outbound` so the gating is unit-tested
/// without a wire.
fn outbound_body(cfg: &IrcConfig, out: &OutboundMessage) -> String {
    let mut body = String::new();
    if let Some(r) = &out.reasoning {
        if cfg.show_reasoning && !r.is_empty() {
            body.push_str("> ");
            body.push_str(&r.replace('\n', "\n> "));
            body.push_str("\n\n");
        }
    }
    body.push_str(&media_body(cfg, &out.text));
    if cfg.show_route && !out.provider.is_empty() {
        body.push_str(&format!("\n— {} · {}", out.provider, out.model));
    }
    body
}

/// The largest file IRC will publish. The copy lands in a directory
/// something serves, so this is a web server's disk: 64 MiB is past any
/// chart, clip or waveform a model produces, and short of "the marker was
/// pointed at a disk image".
const MAX_PUBLISH_BYTES: u64 = 64 * 1024 * 1024;

/// Delete published files older than the configured age. `None` means
/// expiry is not configured (no directory, or `0` = never) and nothing
/// was even looked at; `Some(n)` is how many files were removed.
///
/// Deliberately shallow: `publish` writes flat files, so the sweep
/// touches only regular files directly under the published directory —
/// no recursion into anything a foreign process might have planted
/// there, and symlinks are skipped by `symlink_metadata` rather than
/// followed (a symlink's own mtime says nothing about its target's,
/// and deleting "through" one is how a sweeper learns what rm -rf
/// feels like). Age is mtime, which `publish` sets to copy time — that
/// IS when the link went public.
fn sweep_published_media(cfg: &IrcConfig) -> Option<u64> {
    let dir = cfg.media_public_dir.as_ref()?;
    if cfg.media_public_max_age_days == 0 {
        return None;
    }
    // saturating: an enormous age must mean "delete everything", not
    // panic on (or underflow past) the subtraction.
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            cfg.media_public_max_age_days.saturating_mul(86_400),
        ))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut removed = 0u64;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // An unpublished directory is not an error worth a log line;
        // publish() creates it the first time it is needed.
        Err(_) => return Some(0),
    };
    for entry in entries.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        if mtime < cutoff {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) => warn!(
                    "irc: media expiry could not remove {}: {e}",
                    entry.path().display()
                ),
            }
        }
    }
    Some(removed)
}

/// The name a file is published under: sanitized, then held to an
/// extension allowlist.
///
/// The served directory is a WEB ROOT, so the extension is an
/// instruction to a browser rather than decoration. `media::deliverable`
/// lets a below-owner sender name their own sandbox workspace and
/// `Role::User` holds `FileWrite` there, so `MEDIA:/work/evil.html` was
/// a marker the gate correctly allowed and the publisher then served as
/// `text/html` under the operator's own domain. `nosniff` is no defence
/// against a file that really is HTML.
///
/// Renaming rather than refusing is deliberate: the file still reaches
/// the channel, it just arrives as a download instead of as a page. The
/// vhost `scripts/media-publish.sh` writes is the second half of this —
/// belt and braces, because the two are deployed independently and an
/// installation running an older vhost still gets the rename.
///
/// Both this and the list MOVED to `veles_core::publish`, so that a
/// second frontend publishing a long answer as a file cannot end up
/// holding a divergent idea of which extensions may survive into a web
/// root. Two allowlists would be one allowlist and one bug.
fn safe_published_name(raw: &str) -> String {
    veles_core::publish::safe_published_name(raw)
}

/// Copy one file into the served directory and return the URL for it.
///
/// `None` covers every way it could fail — unconfigured, missing, not a
/// file, oversized, unwritable, 1000 name collisions — and each is logged,
/// because "the channel got a note instead of a picture" is only debuggable
/// from this side of the wire.
fn publish(cfg: &IrcConfig, path: &str) -> Option<String> {
    let dir = match &cfg.media_public_dir {
        Some(d) => d.clone(),
        None => {
            warn!("irc: no media_public_dir, so {path} was not delivered");
            return None;
        }
    };
    if cfg.media_public_url.is_empty() {
        warn!("irc: media_public_dir is set but media_public_url is not — {path} was copied nowhere and delivered as nothing");
        return None;
    }
    let src = std::path::Path::new(path);
    let meta = match std::fs::metadata(src) {
        Ok(m) if m.is_file() => m,
        Ok(_) => {
            warn!("irc: media path is not a file: {path}");
            return None;
        }
        Err(e) => {
            warn!("irc: media not readable: {path}: {e}");
            return None;
        }
    };
    if meta.len() > MAX_PUBLISH_BYTES {
        warn!(
            "irc: media too large to publish: {path} ({} > {MAX_PUBLISH_BYTES} bytes)",
            meta.len()
        );
        return None;
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("irc: cannot create media_public_dir {}: {e}", dir.display());
        return None;
    }

    let stem = safe_published_name(
        &src.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "media".to_string()),
    );
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = match (0..1000u32)
        .map(|n| {
            if n == 0 {
                format!("{stamp}-{stem}")
            } else {
                format!("{stamp}-{n}-{stem}")
            }
        })
        .find(|candidate| !dir.join(candidate).exists())
    {
        Some(n) => n,
        None => {
            warn!("irc: gave up finding a free name in {}", dir.display());
            return None;
        }
    };
    let target = dir.join(&name);
    if let Err(e) = std::fs::copy(src, &target) {
        warn!("irc: cannot publish {path} into {}: {e}", dir.display());
        return None;
    }
    // 0644 explicitly: the umask of a systemd service is not the operator's
    // umask, and a file the web server cannot read is a 403 with no
    // explanation anywhere the operator would look.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)) {
            warn!(
                "irc: published {} but could not make it readable: {e}",
                target.display()
            );
        }
    }
    Some(format!(
        "{}/{}",
        cfg.media_public_url.trim_end_matches('/'),
        name
    ))
}

/// IRC's delivery method for a reply's media: publish each file into the
/// served directory and put the LINK where the marker was. Telegram uploads;
/// IRC has no upload, so it hands out a URL — and with no directory
/// configured it hands out a note, because the alternative is printing the
/// marker, which puts a host path in a public channel.
///
/// This is delivery, not policy: which paths may be delivered at all was
/// decided at the gateway seam by [`veles_core::media::enforce_outbound`],
/// the one place that knows the principal. A connector that re-decided would
/// be a second ACL to keep in step with the first.
fn media_body(cfg: &IrcConfig, text: &str) -> String {
    let (paths, rest) = veles_core::media::extract_media(text);
    if paths.is_empty() {
        return text.to_string();
    }
    let mut out = rest;
    for p in paths {
        let line = publish(cfg, &p).unwrap_or_else(|| {
            "📎 media not delivered on IRC (no public directory configured)".to_string()
        });
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// What a `Progress` message should look like on IRC, if anything.
///
/// Typing indicators (empty text) and streaming partials (accumulating
/// text that re-sends the whole buffer) are dropped — IRC has no
/// editable status line and partials would be churn. A tool-round marker
/// ("⚙ …") is a real step worth showing: forwarded when `tool_progress`
/// narrates to THIS destination — resolved per `dest` because the default
/// posture is DMs-yes / channels-no (see [`ToolProgress`]).
fn progress_body(
    out: &OutboundMessage,
    tool_progress: crate::ToolProgress,
    dest: &str,
) -> Option<String> {
    if out.text.is_empty() {
        return None;
    }
    if tool_progress.narrates(dest) && out.text.starts_with('⚙') {
        Some(out.text.clone())
    } else {
        None
    }
}

async fn handle_outbound(conn: &mut Conn, out: OutboundMessage) {
    // Progress messages (typing/tool rounds/streaming partials) are a
    // Telegram-style concept; the only one with an IRC analogue is the
    // tool-round marker, forwarded as a normal message when enabled.
    if out.kind == MessageKind::Progress {
        let body = progress_body(&out, conn.cfg.tool_progress, &out.chat_id);
        if let Some(body) = body {
            let mode = conn.channel_mode(&out.chat_id);
            if let Err(e) = conn.privmsg(&out.chat_id, &body, mode).await {
                error!("irc: outbound failed: {e:#}");
            }
        }
        return;
    }

    // Questions: IRC has no buttons, so render numbered options and let
    // `parse_reply` accept "1"/"yes"/"deny" (or `.agent answer <id> …`
    // when several are in flight).
    if out.kind == MessageKind::Ask {
        let Some(req) = out.ask.as_ref() else { return };
        let body = veles_core::ask::render_prompt(req);
        let mode = conn.channel_mode(&out.chat_id);
        // Security prompts must not queue behind flood pacing — see
        // `send_line`/`privmsg_paced` for the rationale.
        let res = conn.privmsg_paced(&out.chat_id, &body, mode, false).await;
        if let Err(e) = res {
            error!("irc: ask send failed: {e:#}");
            veles_core::ask::cancel(&req.id);
        }
        return;
    }

    let body = outbound_body(&conn.cfg, &out);
    let mode = conn.channel_mode(&out.chat_id);
    if let Err(e) = conn.privmsg(&out.chat_id, &body, mode).await {
        error!("irc: outbound failed: {e:#}");
    }
}

// ── tool commands (model-driven irc_* surface) ───────────────────────

/// Publish synthesized informational output — a whois answer, a ban
/// list, a topic query answer, an away confirmation — into the tap so
/// it reaches live buffers AND the durable scrollback. None of these
/// have a wire line of their own (they arrive as numerics), which is
/// why the older code delivered them only as an op reply string.
fn tap_info(conn: &Conn, buffer: &str, nick: &str, text: &str) {
    let mut ev = crate::tap::event(
        &conn.network,
        &conn.presence,
        buffer,
        crate::tap::EventKind::Info,
        nick,
        text,
    );
    ev.own = true;
    crate::tap::publish(ev);
}

/// Server-wide information — a MOTD line, a user-count numeric — into
/// the server buffer. Same shape as `tap_info`, but it is SERVER talk
/// (nobody's reply to us), so it is not marked own: the pane ranks it
/// as noise rather than as our output.
fn tap_server(conn: &Conn, text: &str) {
    crate::tap::publish(crate::tap::event(
        &conn.network,
        &conn.presence,
        crate::tap::SERVER_BUFFER,
        crate::tap::EventKind::Info,
        "",
        text,
    ));
}

/// MOTD lines arrive with a leading "- " that exists only for the wire;
/// clients strip it before display (weechat included), so a raw dump
/// reads as "- - text".
fn motd_body(raw: &str) -> &str {
    let t = raw.strip_prefix("- ").unwrap_or(raw);
    match t.strip_prefix('-') {
        Some(rest) => rest.trim_start(),
        None => t,
    }
}

/// Where an introspection answer lands: the buffer its op named, or
/// the per-op default.
fn info_scope(scope: &Option<String>, default: &str) -> String {
    match scope {
        Some(s) if !s.is_empty() => s.clone(),
        _ => default.to_string(),
    }
}

/// Handle one tool/desktop command against the live connection. A
/// `Quit` never reaches here — the connection's select loop handles it
/// (it ends the connection), so this arm only answers a test that drove
/// the handler directly.
async fn handle_tool_command(conn: &mut Conn, cmd: IrcCommand) {
    // Every arm that carries a reply sender answers it. The early
    // version discarded the sender with `..` on half the arms, so those
    // ops' callers (the pane's /api/irc/op AND the model's `irc` tool)
    // sat on their 10s timeout and reported "the connection did not
    // answer in time" even though the wire action succeeded. A reply
    // oneshot is a promise; answer it or do not take it.
    match cmd {
        IrcCommand::Say {
            target,
            text,
            mode,
            reply,
        } => {
            let mode = mode.unwrap_or(conn.channel_mode(&target));
            let result = match conn.privmsg(&target, &text, mode).await {
                Ok(()) => format!("sent to {target}"),
                Err(e) => format!("error: {e:#}"),
            };
            let _ = reply.send(result);
        }
        IrcCommand::Notice {
            target,
            text,
            reply,
        } => {
            let result = match conn.notice(&target, &text).await {
                Ok(()) => format!("notice sent to {target}"),
                Err(e) => format!("error: {e:#}"),
            };
            let _ = reply.send(result);
        }
        IrcCommand::Join {
            channel,
            key,
            reply,
        } => {
            // Registration makes deferred commands deliverable, not
            // immediately sendable. They owe the same wire budget as a
            // live command; a direct client.send made this a JOIN burst.
            let result = match conn
                .send_line(Command::JOIN(channel.clone(), None, key), true)
                .await
            {
                Ok(()) => {
                    // A join that came through here is somebody asking:
                    // the desktop, `.irc join`, or the model's tool —
                    // all of which are gated above this point. That is
                    // what separates it from the server putting us in a
                    // room, so it is the one place that may widen the
                    // sanctioned set.
                    conn.sanction_channel(&channel);
                    format!("joining {channel}")
                }
                Err(e) => format!("error: {e:#}"),
            };
            let _ = reply.send(result);
        }
        IrcCommand::Part {
            channel,
            reason,
            reply,
        } => {
            let result = match conn
                .send_line(Command::PART(channel.clone(), reason), true)
                .await
            {
                Ok(()) => format!("parted {channel}"),
                Err(e) => format!("error: {e:#}"),
            };
            let _ = reply.send(result);
        }
        IrcCommand::Topic {
            channel,
            topic,
            scope,
            reply,
        } => match topic {
            Some(t) => {
                // A SET announces itself in-band (the server's TOPIC
                // line becomes a Topic tap event in the channel), so
                // the reply confirms the send and nothing more.
                let result = match conn
                    .send_line(Command::TOPIC(channel.clone(), Some(t)), true)
                    .await
                {
                    Ok(()) => format!("topic set on {channel}"),
                    Err(e) => format!("error: {e:#}"),
                };
                let _ = reply.send(result);
            }
            None => {
                // A QUERY answers with 332/331. Queue the reply; the
                // numeric handler delivers AND publishes the answer to
                // the requested buffer.
                let key = conn.casefold(&channel);
                conn.topic_pending
                    .push((key.clone(), Some(info_scope(&scope, &channel)), reply));
                match conn
                    .send_line(Command::TOPIC(channel.clone(), None), true)
                    .await
                {
                    Ok(()) => {}
                    Err(e) => {
                        // The reply would otherwise wait for a 332 that
                        // will never come.
                        let pending: Vec<_> = conn
                            .topic_pending
                            .extract_if(
                                ..,
                                |(c, _, _): &mut (
                                    String,
                                    Option<String>,
                                    oneshot::Sender<String>,
                                )| { *c == key },
                            )
                            .collect();
                        for (_, _, r) in pending {
                            let _ = r.send(format!("error: {e:#}"));
                        }
                    }
                }
            }
        },
        IrcCommand::Kick {
            channel,
            nick,
            reason,
            reply,
        } => {
            let result = match conn
                .send_line(Command::KICK(channel.clone(), nick.clone(), reason), true)
                .await
            {
                Ok(()) => format!("kicked {nick} from {channel}"),
                Err(e) => format!("error: {e:#}"),
            };
            let _ = reply.send(result);
        }
        IrcCommand::Bans {
            channel,
            scope,
            reply,
        } => {
            let key = conn.casefold(&channel);
            if !conn.channels.contains(&key) {
                let _ = reply.send(format!("not in {channel}"));
            } else {
                // The answer is a stream of 367s ended by 368; the scratch
                // and the pending list are both keyed by channel, so two
                // overlapping requests for one channel cannot interleave
                // (the second reply fires from the same completed set).
                conn.bans_scratch.remove(&key);
                conn.bans_pending
                    .push((key, Some(info_scope(&scope, &channel)), reply));
                let _ = conn
                    .send_line(Command::Raw(format!("MODE {channel} +b"), Vec::new()), true)
                    .await;
            }
        }
        IrcCommand::Names {
            channel,
            scope,
            reply,
        } => {
            let key = conn.casefold(&channel);
            let roster = conn.rosters.get(&key).cloned().unwrap_or_default();
            let mut out = format!("{channel}: {} member(s)", roster.len());
            if roster.is_empty() {
                out.push_str(" (roster not synced yet)");
            } else {
                out.push_str(&format!("\n{}", roster.join(" ")));
            }
            let _ = reply.send(out.clone());
            tap_info(conn, &info_scope(&scope, &channel), "names", &out);
        }
        IrcCommand::Who {
            target,
            scope,
            reply,
        } => {
            let Some(target) = target.filter(|t| !t.trim().is_empty()) else {
                let _ = reply.send("who: give a mask or a channel".to_string());
                return;
            };
            if !conn.who_pending.is_empty() {
                let _ = reply.send("who: a query is already in flight".to_string());
                return;
            }
            conn.who_rows.clear();
            let buffer = info_scope(&scope, &target);
            conn.who_pending.push((target.clone(), Some(buffer), reply));
            let _ = conn
                .send_line(Command::Raw(format!("WHO {target}"), Vec::new()), true)
                .await;
        }
        IrcCommand::Typing { target, state } => {
            // TAGMSG with a +typing tag. Only when the cap was ACKed —
            // sending it to a server that never offered it would be a
            // protocol error, and the pane only calls this when the
            // connection advertises typing anyway. No bucket: indicators
            // are high-frequency by design, and the spec's own clients
            // rate-limit at the UI layer.
            if !conn.caps.contains("typing") {
                return;
            }
            match Message::with_tags(
                Some(vec![irc::proto::message::Tag(
                    "+typing".to_string(),
                    Some(state.clone()),
                )]),
                None,
                "TAGMSG",
                vec![target.as_str()],
            ) {
                Ok(msg) => {
                    let _ = conn.client.send(msg);
                }
                Err(e) => error!("irc: typing indicator: {e}"),
            }
        }
        IrcCommand::Mode {
            target,
            modes,
            reply,
        } => {
            let line = format!("MODE {target} {modes}");
            let result = match conn.send_line(Command::Raw(line, Vec::new()), true).await {
                Ok(()) => "mode sent".to_string(),
                Err(e) => format!("error: {e:#}"),
            };
            let _ = reply.send(result);
        }
        IrcCommand::Nick { nick, reply } => {
            conn.desired_nick = nick.clone();
            let result = match conn.send_line(Command::NICK(nick.clone()), true).await {
                Ok(()) => format!("nick change to {nick} requested"),
                Err(e) => format!("error: {e:#}"),
            };
            let _ = reply.send(result);
        }
        IrcCommand::List { reply, .. } => {
            let mut v: Vec<String> = conn.channels.iter().cloned().collect();
            v.sort();
            let _ = reply.send(v);
        }
        IrcCommand::Whois { nick, scope, reply } => {
            let key = conn.casefold(&nick);
            let buffer = info_scope(&scope, crate::tap::SERVER_BUFFER);
            // Queue the reply: the full answer arrives with 318
            // ENDOFWHOIS (account + hostmask + server). Fast path when
            // we already have everything cached and no WHOIS is needed.
            if let Some(acc) = conn.accounts.get(&key) {
                if conn.whois_cache.contains_key(&key) {
                    let (host, server) = conn.whois_cache.get(&key).unwrap();
                    let mut out = format!("{nick}: verified account {acc};");
                    if !host.is_empty() {
                        out.push_str(&format!(" hostmask {nick}!{host};"));
                    }
                    if !server.is_empty() {
                        out.push_str(&format!(" server {server};"));
                    }
                    let _ = reply.send(out.clone());
                    tap_info(conn, &buffer, "", &out);
                    return;
                }
            }
            conn.whois_pending.push((key, Some(buffer), reply));
            let _ = conn
                .send_line(Command::WHOIS(None, nick.clone()), true)
                .await;
        }
        IrcCommand::ChannelInfo {
            channel,
            scope,
            reply,
        } => {
            let key = conn.casefold(&channel);
            if !conn.channels.contains(&key) {
                let _ = reply.send(format!("not in {channel}"));
                return;
            }
            // Roster refresh: NAMES re-issues 353/366; TOPIC re-issues
            // 332. The reply is delivered on ENDOFNAMES. Clear the old
            // roster first or members accumulate across refreshes.
            conn.rosters.remove(&key);
            conn.info_pending
                .push((key, Some(info_scope(&scope, &channel)), reply));
            let _ = conn
                .send_line(Command::Raw(format!("TOPIC {channel}"), Vec::new()), true)
                .await;
            let _ = conn
                .send_line(Command::Raw(format!("NAMES {channel}"), Vec::new()), true)
                .await;
        }
        IrcCommand::Away {
            message,
            scope,
            reply,
        } => {
            let leaving = message.is_some();
            let result = match conn.send_line(Command::AWAY(message), true).await {
                Ok(()) => {
                    if leaving {
                        "away set".to_string()
                    } else {
                        "you are back".to_string()
                    }
                }
                Err(e) => format!("error: {e:#}"),
            };
            let buffer = info_scope(&scope, crate::tap::SERVER_BUFFER);
            let _ = reply.send(result.clone());
            tap_info(conn, &buffer, &conn.current_nick, &result);
        }
        IrcCommand::Raw { line, reply } => {
            let result = if !conn.cfg.allow_raw_tool {
                "raw tool disabled (IRC_ALLOW_RAW_TOOL)".to_string()
            } else {
                // Through the flood bucket like every other outbound:
                // a raw line is one more line the server counts.
                match conn
                    .send_line(
                        Command::Raw(crate::sanitize_outbound(&line), Vec::new()),
                        true,
                    )
                    .await
                {
                    Ok(()) => "sent".to_string(),
                    Err(e) => format!("error: {e:#}"),
                }
            };
            let _ = reply.send(result);
        }
        // A `Quit` is handled by the connection's select loop (it ends
        // the connection), never here; this arm answers a test that
        // drove the handler directly.
        IrcCommand::Quit { .. } => {}
    }
}

#[cfg(test)]
mod tap_tests {
    use super::*;
    use crate::tap::{as_action, EventKind};

    fn msg(prefix: Option<&str>, cmd: Command) -> Message {
        Message {
            tags: None,
            prefix: prefix.map(Prefix::new_from_str),
            command: cmd,
        }
    }

    // The tap is NOT a mirror of handle_incoming. That one asks "does
    // the agent care about this"; this one asks "would a person see it
    // in their client". The answers differ for exactly the traffic that
    // was being thrown away.
    #[test]
    fn traffic_the_agent_gate_drops_still_changes_what_a_client_draws() {
        for c in [
            Command::PART("#veles".to_string(), Some("bye".to_string())),
            Command::QUIT(Some("ping timeout".to_string())),
            Command::TOPIC("#veles".to_string(), Some("new topic".to_string())),
            Command::JOIN("#veles".to_string(), None, None),
            Command::KICK("#veles".to_string(), "nyx".to_string(), None),
        ] {
            assert!(
                tap_is_stateful(&msg(Some("wrath!u@h"), c.clone())),
                "{c:?} changes the channel list, a roster or a topic"
            );
        }
    }

    #[test]
    fn ordinary_chatter_and_protocol_are_not_state() {
        // A notice is client traffic but changes no state — it must not
        // cost a snapshot on every line.
        assert!(!tap_is_stateful(&msg(
            Some("NickServ!s@services."),
            Command::NOTICE("jackma".to_string(), "identified".to_string())
        )));
        assert!(!tap_is_stateful(&msg(
            None,
            Command::PING("server".to_string(), None)
        )));
        assert!(!tap_is_stateful(&msg(
            Some("wrath!u@h"),
            Command::PRIVMSG("#veles".to_string(), "hello".to_string())
        )));
    }

    #[test]
    fn server_time_beats_arrival_time() {
        // A line replayed out of the backlog must carry the time it was
        // SAID, not the time we happened to read it.
        let mut m = msg(
            Some("wrath!u@h"),
            Command::PRIVMSG("#veles".to_string(), "hi".to_string()),
        );
        m.tags = Some(vec![irc::proto::message::Tag(
            "time".to_string(),
            Some("2026-08-25T01:02:03.000Z".to_string()),
        )]);
        assert_eq!(tap_ts(&m), "2026-08-25T01:02:03.000Z");
    }

    #[test]
    fn a_missing_or_empty_time_tag_falls_back_to_now() {
        let m = msg(
            Some("wrath!u@h"),
            Command::PRIVMSG("#veles".to_string(), "hi".to_string()),
        );
        assert!(!tap_ts(&m).is_empty(), "no tag at all");

        let mut m2 = m.clone();
        m2.tags = Some(vec![irc::proto::message::Tag(
            "time".to_string(),
            Some(String::new()),
        )]);
        assert!(!tap_ts(&m2).is_empty(), "empty tag is not a timestamp");

        let mut m3 = m.clone();
        m3.tags = Some(vec![irc::proto::message::Tag(
            "account".to_string(),
            Some("wrath".to_string()),
        )]);
        assert!(!tap_ts(&m3).is_empty(), "some other tag");
    }

    #[test]
    fn the_sender_comes_off_the_prefix() {
        let m = msg(
            Some("wrath!user@host"),
            Command::PRIVMSG("#veles".to_string(), "hi".to_string()),
        );
        assert_eq!(tap_nick(&m), "wrath");
        // Straight from the server there is no nick, and that is normal.
        assert_eq!(
            tap_nick(&msg(None, Command::PING("s".to_string(), None))),
            ""
        );
    }

    #[test]
    fn the_bare_nick_comes_out_of_a_names_entry() {
        // 353 with userhost-in-names gives `prefix nick!user@host`; the
        // away lookup needs just the nick.
        assert_eq!(bare_nick("bob"), "bob");
        assert_eq!(bare_nick("@bob"), "bob");
        assert_eq!(bare_nick("+bob!~u@h"), "bob");
        assert_eq!(bare_nick("@+bob!~u@example.com"), "bob");
        assert_eq!(bare_nick("bob!a@b@c"), "bob");
    }

    #[test]
    fn an_action_is_its_own_kind() {
        // A client renders "* nick waves", not "<nick> \u{1}ACTION…".
        assert_eq!(as_action("\u{1}ACTION waves\u{1}"), Some("waves"));
        let kind = match as_action("\u{1}ACTION waves\u{1}") {
            Some(_) => EventKind::Action,
            None => EventKind::Privmsg,
        };
        assert_eq!(kind, EventKind::Action);
        assert_eq!(as_action("just talking"), None);
    }
}

#[cfg(test)]
mod boundary_tests {
    //! The one thing in this crate that must not be got wrong.
    //!
    //! `bus.send_inbound` is the single call that turns an IRC line
    //! into an agent turn. The admin's presence must never make it, or
    //! every line the human types in their own session — and every line
    //! anyone says to them — is sent to the model.
    //!
    //! The guarantee is a TYPE, not a policy: `Conn::bus` is
    //! `Option<GatewayBus>` and the presence is built with `None`, so
    //! the call is not reachable rather than merely not made. These
    //! tests pin the shape that makes that true, because a later change
    //! to `Option<GatewayBus>` -> `GatewayBus` plus a boolean would
    //! compile fine and quietly undo it.

    /// The trust line and its severity, for the posture tests below.
    use super::{boot_trust_line, TrustBoot};

    /// The module's own source, EXCLUDING these tests.
    ///
    /// include_str! pulls in this module too, so counting call sites
    /// over the whole file counts the assertions themselves — which
    /// made "exactly one send_inbound" read three and fail against its
    /// own text.
    fn source() -> &'static str {
        let full = include_str!("client.rs");
        match full.find("mod boundary_tests {") {
            Some(i) => &full[..i],
            None => full,
        }
    }

    #[test]
    fn there_is_exactly_one_call_that_creates_an_agent_turn() {
        // If this count ever rises, the new one needs the same guard —
        // and whoever added it needs to have thought about the admin.
        let n = source().matches("send_inbound(").count();
        assert_eq!(
            n, 1,
            "expected one send_inbound call site, found {n}; every one \
             of them must be behind the Option<GatewayBus> guard"
        );
        // A call on `conn.bus` directly would bypass the Option — the
        // only legal spelling is through a guarded local.
        assert!(
            !source().contains("conn.bus.send_inbound"),
            "no call site may reach the bus without the Option guard"
        );
    }

    #[test]
    fn the_bus_is_optional_so_a_presence_cannot_hold_one() {
        let src = source();
        assert!(
            src.contains("bus: Option<GatewayBus>"),
            "Conn::bus must stay an Option — a bool flag beside a real \
             bus would compile with the guard removed"
        );
        // And the sole call site must be gated on having one.
        assert!(
            src.contains("let Some(bus) = conn.bus.clone() else {"),
            "the send_inbound site must be gated on the Option"
        );
    }

    /// The bus-less owner presence (the bouncer) was removed on
    /// 2026-10-07; the one connection left is the bot's, and it carries
    /// the bus.
    #[test]
    fn the_bot_is_started_with_the_bus() {
        let src = source();
        assert_eq!(
            src.matches("run_presence_loop(").count(),
            2,
            "one definition and one call: nothing else starts a connection"
        );
        assert!(
            src.contains("run_presence_loop(cfg, Some(bus), crate::tap::PRESENCE_BOT, out_rx)"),
            "only the bot's adapter passes a bus"
        );
    }

    #[test]
    fn a_presence_registers_no_outbound_channel() {
        // register_outbound* is how the gateway routes an agent reply to
        // a platform. A presence that registered one could have a turn's
        // output delivered as if the human had said it.
        //
        // Matched on the shared prefix so BOTH spellings count: this
        // test read `register_outbound(` literally, and when the call
        // became `register_outbound_route(` in 2026-09 it went to zero
        // matches and failed loudly. That is the right failure — a
        // source-scanning guard that silently stops finding its subject
        // is a green test that has stopped looking — but the assertion
        // has to be on what it MEANS, not on one spelling of it.
        let src = source();
        let n = src.matches("bus.register_outbound").count();
        assert_eq!(n, 1, "only the bot's adapter may register outbound");
        let idx = src.find("bus.register_outbound").unwrap();
        let before = &src[..idx];
        assert!(
            before.contains("pub async fn run_adapter"),
            "the one registration belongs to run_adapter"
        );
        assert!(
            !before.contains("pub async fn run_presence"),
            "it must come before run_presence, not inside it"
        );
    }

    /// The registration has to name the NETWORK as its route.
    ///
    /// Registering under the bare platform key is the 2026-09-11 bug
    /// itself: the bus filed every IRC adapter under `"irc"`, a map
    /// insert, so with two networks the second to start replaced the
    /// first and owned every reply on the platform — including replies
    /// for a channel on the OTHER network, which it would then say in
    /// whatever room of that name it could reach.
    ///
    /// Cheap to get wrong again (`register_outbound` still exists and is
    /// the obvious call to reach for), invisible until a second network
    /// is configured, and silent when it happens. So it is pinned here.
    #[test]
    fn the_outbound_registration_is_scoped_to_the_network() {
        let src = source();
        assert!(
            src.contains(r#"bus.register_outbound_route("irc", &cfg.name, out_tx)"#),
            "the adapter must register under its network as the route, \
             or two networks share one channel and one of them wins"
        );
    }

    #[test]
    fn the_model_facing_handle_is_always_the_bots() {
        // An agent able to speak as the admin could impersonate them to
        // everyone in the channel, and `irc` is a tool the model calls
        // on its own initiative.
        let src = source();
        assert!(
            src.contains("pub fn handle() -> Option<IrcHandle> {")
                && src.contains("presence == crate::tap::PRESENCE_BOT"),
            "handle() must resolve to the bot's presence"
        );
    }

    /// The boot line names the boundary that is actually in force.
    ///
    /// It used to say an empty `IRC_ALLOWED_USERS` left the bot "OPEN to
    /// every account" — printed at every start on a box where `[acl]` was
    /// configured and the allowlist was not consulted at all. A warning
    /// that is false is worse than no warning: it is how operators learn
    /// to skim past the security lines. Mutation: swapping the severity of
    /// the configured arms puts the scare word back where it does not
    /// belong, and the "no [acl]" arms are what must stay loud.
    #[test]
    fn the_boot_line_names_the_boundary_in_force() {
        // ACL configured, allowlist empty: bookkeeping, and no claim that
        // the bot is open — that is the lie being fixed.
        let line = match boot_trust_line(true, 0) {
            TrustBoot::Info(l) => l,
            TrustBoot::Warn(l) => panic!("a configured [acl] is not an emergency: {l}"),
        };
        assert!(line.contains("the trust boundary is [acl]"), "{line}");
        assert!(
            !line.to_lowercase().contains("open"),
            "it must not claim the bot is open while the ACL decides: {line}"
        );
        assert!(
            line.contains("is not consulted"),
            "and it must say the allowlist is not consulted: {line}"
        );

        // ACL configured with legacy entries present: still bookkeeping,
        // and it says how many are being ignored.
        let line = match boot_trust_line(true, 3) {
            TrustBoot::Info(l) => l,
            TrustBoot::Warn(l) => panic!("a configured [acl] is not an emergency: {l}"),
        };
        assert!(line.contains("3 IRC_ALLOWED_USERS"), "{line}");

        // No ACL and no allowlist: THIS is the open bot, and it stays a
        // warning naming what open means.
        let line = match boot_trust_line(false, 0) {
            TrustBoot::Warn(l) => l,
            TrustBoot::Info(l) => panic!("a dormant ACL with no allowlist must warn: {l}"),
        };
        assert!(line.to_lowercase().contains("open"), "{line}");
        assert!(line.contains("resolves as OWNER"), "{line}");

        // No ACL with an allowlist: the legacy posture, loud because every
        // admitted sender is owner-equivalent.
        let line = match boot_trust_line(false, 2) {
            TrustBoot::Warn(l) => l,
            TrustBoot::Info(l) => panic!("a dormant ACL must warn even with an allowlist: {l}"),
        };
        assert!(
            line.contains("2 IRC_ALLOWED_USERS entries ARE the trust boundary"),
            "{line}"
        );
        assert!(line.contains("resolves as OWNER"), "{line}");
    }

    /// The boot line and the refusal a stranger gets are two descriptions
    /// of one posture, so they must be built from the same two facts. A
    /// pane that announced "the ACL decides" while `privileged_refusal`
    /// told the owner there was no boundary would be the same lie again.
    #[test]
    fn the_boot_line_and_the_refusal_agree_about_the_posture() {
        let src = source();
        // Search INSIDE run_adapter: `boot_trust_line(` also appears in its
        // own definition, which would satisfy a naive find while the call
        // site passed hardcoded values.
        let adapter = src
            .find("pub async fn run_adapter")
            .expect("the adapter entry point");
        let body = &src[adapter..];
        let at = body
            .find("boot_trust_line(")
            .expect("run_adapter prints the trust line");
        let call = &body[at..at + 220.min(body.len() - at)];
        assert!(
            call.contains("live_config().is_configured()"),
            "it must ask live_config, the same source privileged_refusal asks: {call}"
        );
        assert!(
            call.contains("allowed_users.len()"),
            "and the same allowlist privileged_refusal judges by: {call}"
        );
        assert!(
            src.contains("if acl.is_configured() || !conn.cfg.allowed_users.is_empty() {"),
            "privileged_refusal must keep judging by the same pair"
        );
    }
}

/// Bounds on what a SERVER can make us accumulate.
///
/// Class: WeeChat WSA-2026-3 — "missing limit on unterminated IRC
/// messages causes memory exhaustion", live from 0.3.3 (2010) to 4.9.1
/// (2026). The shape generalises past the line buffer they fixed: a
/// client keeps per-nick and per-channel state, the SERVER decides how
/// many nicks and channels exist, and nobody writes down a number.
#[cfg(test)]
mod hostile_server_bounds {
    use super::*;

    #[test]
    fn a_roster_stops_growing_at_the_cap() {
        let mut roster: Vec<String> = Vec::new();
        // Feed it far past the cap in 353-sized chunks (a 512-byte line
        // carries roughly thirty names).
        let mut refused = false;
        for batch in 0..400 {
            let names: Vec<String> = (0..30).map(|i| format!("nick{}_{}", batch, i)).collect();
            if !merge_roster(&mut roster, &names.join(" ")) {
                refused = true;
            }
        }
        assert!(refused, "the cap must eventually refuse");
        assert_eq!(
            roster.len(),
            MAX_ROSTER_MEMBERS,
            "the roster settles exactly at the cap, not past it"
        );
    }

    #[test]
    fn a_replayed_names_burst_does_not_duplicate_members() {
        // A bouncer re-issuing NAMES on netjoin is ordinary, correct
        // behaviour. The previous `extend` doubled the roster every
        // time it happened, which the pane then drew.
        let mut roster: Vec<String> = Vec::new();
        for _ in 0..5 {
            assert!(merge_roster(&mut roster, "@alice +bob carol"));
        }
        assert_eq!(roster, vec!["@alice", "+bob", "carol"]);
    }

    #[test]
    fn merging_preserves_prefixes_and_order() {
        let mut roster: Vec<String> = Vec::new();
        merge_roster(&mut roster, "@op +voice plain");
        merge_roster(&mut roster, "~owner @op");
        assert_eq!(roster, vec!["@op", "+voice", "plain", "~owner"]);
    }

    #[test]
    fn a_full_map_still_updates_keys_it_already_holds() {
        // Refusing rather than evicting is the whole design: with a
        // hostile server an eviction policy lets the attacker choose
        // what we forget, and what we would forget includes which nick
        // holds which verified account. But a full map must still track
        // changes for everyone it already knows, or a nick that changes
        // account gets frozen at the old one.
        let mut m: HashMap<String, String> = HashMap::new();
        for i in 0..MAX_TRACKED_NICKS {
            m.insert(format!("nick{i}"), "acct".into());
        }
        assert!(
            !bounded_insert(&mut m, MAX_TRACKED_NICKS, "newcomer".into(), "acct".into()),
            "a new key is refused when full"
        );
        assert_eq!(m.len(), MAX_TRACKED_NICKS);
        assert!(
            bounded_insert(&mut m, MAX_TRACKED_NICKS, "nick0".into(), "changed".into()),
            "an existing key still updates when full"
        );
        assert_eq!(m.get("nick0").map(String::as_str), Some("changed"));
    }

    // The consequence of refusing rather than evicting — a nick whose
    // account we could not record is not authorised — is deliberately
    // NOT asserted here. `is_authorized` is a method on `Conn`, `Conn`
    // owns a live `Client`, and a `Client` needs a socket; testing it
    // honestly means driving a real connection. A grep over this file
    // dressed up as a test would pass while proving nothing, so that
    // case lives with the stub server instead.
}

/// Malformed input from a hostile or broken server, against the REAL
/// client.
///
/// WHY A STUB SERVER AND NOT A MOCK
///
/// The bug class this defends against is a handler that indexes an
/// argument the server did not send. In C that is WeeChat's WSA-2020-1
/// (buffer overflow on a malformed 324 channel mode), WSA-2020-2 (crash
/// on a malformed 352 WHO) and WSA-2020-3 (buffer overflow on a 005
/// with new nick prefixes), and irssi's NULL dereferences on an empty
/// nick, on a DCC message with no source, and on an invalid timestamp.
/// In Rust it is a panic — and a panic on the adapter thread takes that
/// network's connection down, so it is a remote denial of service, not
/// a memory-safety hole.
///
/// Calling `handle_numeric` directly would test the handlers while
/// skipping the codec, the parser and the argument splitting — which is
/// where a malformed line becomes a short argument list in the first
/// place. So this speaks IRC over a real socket to the real
/// `run_connection_inner`, and asserts the task does not panic.
///
/// WeeChat's tests/unit/plugins/irc/test-irc-protocol.cpp is the model:
/// one case per class, named for what it defends against.
#[cfg(test)]
mod stub_server {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    /// Run the real client against a scripted server and return every
    /// line the client sent. Panics in the client surface as a failed
    /// join, which is the assertion that matters.
    /// Drive the client against a stub that advertises NOTHING in CAP LS.
    async fn drive(cfg_tweak: impl FnOnce(&mut IrcConfig), script: Vec<String>) -> Vec<String> {
        drive_caps(cfg_tweak, "", script).await
    }

    /// Drive it against a stub that advertises `advertise` (a
    /// space-separated CAP LS list) and ACKs whatever the client then
    /// REQs.
    ///
    /// This exists because the no-cap stub SILENTLY skips every
    /// cap-gated path. The chathistory test was written against the
    /// no-cap stub and asserted `asks <= 1`; the real number was 0,
    /// because the client had nothing to REQ and `conn.caps` was empty.
    /// It passed for a year's worth of the wrong reason in one
    /// afternoon. Anything gated on `conn.caps.contains(..)` MUST use
    /// this and MUST assert a positive count.
    async fn drive_caps(
        cfg_tweak: impl FnOnce(&mut IrcConfig),
        advertise: &str,
        script: Vec<String>,
    ) -> Vec<String> {
        drive_full(cfg_tweak, advertise, script, Vec::new()).await
    }

    /// Services enforcement, end to end on the stub wire.
    ///
    /// The live failure (2026-09-11): synIRC renamed the bot to
    /// `synGuest27554` because it could not identify, and the 300s
    /// reclaim then sent a bare `NICK jackma` forever — which an
    /// ENFORCED nick always refuses, because services is holding it.
    /// The bot sat in the channel under a name nobody addresses, and
    /// since `addressed_to` matches on `current_nick`, it silently
    /// stopped answering mentions.
    ///
    /// Two things are asserted, and the ORDER is the fix: RELEASE must
    /// come before NICK, or the NICK is refused exactly as before.
    ///
    /// Mutation check: drop the RELEASE from `regain_nick` and the
    /// ordering assertion goes red; remove the `regain_nick` call from
    /// the NICK arm and both do.
    #[tokio::test]
    async fn automatic_parts_and_nick_recovery_use_the_wire_budget() {
        let sent = drive_wire_timed(
            |c| {
                c.nickname = "jackma".into();
                c.sasl_password = Some("hunter2".into());
                c.sasl_username = Some("jackma".into());
                c.flood_burst = 1;
                c.flood_per_sec = 10.0;
            },
            "",
            vec![
                ":jackma!~jackma@cloak NICK :synGuest27554".into(),
                ":synGuest27554!~jackma@cloak JOIN #lobby0".into(),
                ":synGuest27554!~jackma@cloak JOIN #lobby1".into(),
            ],
            Vec::new(),
            Vec::new(),
            Duration::from_millis(0),
            Duration::from_millis(400),
        )
        .await;
        let recovery_start = sent
            .iter()
            .position(|(_, line)| line.starts_with("PRIVMSG NickServ :"))
            .expect("the enforced nick must trigger recovery");
        let recovery: Vec<_> = sent[recovery_start..]
            .iter()
            .filter(|(_, line)| {
                line.starts_with("PRIVMSG NickServ :")
                    || line == "NICK jackma"
                    || line.starts_with("PART #lobby")
            })
            .collect();
        assert_eq!(
            recovery.len(),
            4,
            "two recovery lines and both PARTs, after the initial nick: {sent:?}"
        );
        assert_eq!(recovery[0].1, "PRIVMSG NickServ :RELEASE jackma");
        assert_eq!(recovery[1].1, "NICK jackma");
        for (previous, next) in recovery.iter().zip(recovery.iter().skip(1)) {
            assert!(
                next.0.saturating_sub(previous.0) >= Duration::from_millis(60),
                "automatic traffic bypassed the line budget: {sent:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_forced_rename_is_answered_by_releasing_and_retaking_the_nick() {
        let sent = drive_full_with_delay(
            |c| {
                c.nickname = "jackma".into();
                c.sasl_password = Some("hunter2".into());
                c.sasl_username = Some("jackma".into());
            },
            "",
            vec![":jackma!~jackma@cloak NICK :synGuest27554".to_string()],
            vec![],
            Duration::from_millis(250),
        )
        .await;
        let release = sent
            .iter()
            .position(|l| l.contains("NickServ") && l.contains("RELEASE jackma"));
        let retake = sent
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim() == "NICK jackma")
            .map(|(i, _)| i)
            .next_back();
        assert!(
            release.is_some(),
            "an enforced nick is HELD — without a RELEASE the retake is refused: {sent:?}"
        );
        assert!(
            retake.is_some(),
            "the bot must ask for its nick back: {sent:?}"
        );
        assert!(
            release < retake,
            "RELEASE has to precede NICK or it changes nothing: {sent:?}"
        );
    }

    /// A rename that lands ON the nick we want must not be fought.
    ///
    /// `.irc nick` and the `irc` tool both move `desired_nick`, so the
    /// regain path compares against that rather than against the
    /// configured nickname — otherwise a deliberate rename would start
    /// a tug of war with the bot's own reclaim.
    #[tokio::test]
    async fn a_rename_to_the_nick_we_asked_for_is_not_fought() {
        let sent = drive_full_with_delay(
            |c| {
                c.nickname = "jackma".into();
                c.sasl_password = Some("hunter2".into());
            },
            "",
            vec![":jackma!~jackma@cloak NICK :jackma".to_string()],
            vec![],
            Duration::from_millis(250),
        )
        .await;
        assert!(
            !sent.iter().any(|l| l.contains("RELEASE")),
            "no enforcement happened, so nothing should be released: {sent:?}"
        );
    }

    /// …and `ops` are `IrcCommand`s pushed at the client from the tool
    /// side once registration has settled, so the tests can exercise
    /// what the client SENDS on its own initiative rather than only how
    /// it answers the wire. The default push-delay keeps op→wire
    /// ordering deterministic against this stub; tests that need the
    /// script processed FIRST (roster priming) use `_with_delay`.
    async fn drive_full(
        cfg_tweak: impl FnOnce(&mut IrcConfig),
        advertise: &str,
        script: Vec<String>,
        ops: Vec<IrcCommand>,
    ) -> Vec<String> {
        drive_full_with_delay(
            cfg_tweak,
            advertise,
            script,
            ops,
            Duration::from_millis(150),
        )
        .await
    }

    #[tokio::test]
    async fn a_dm_agent_asks_sees_asks_keyed_on_the_sender() {
        // The discriminating observable for the DM-chat-id fix: an ask
        // registered the way the gateway does — `irc:{chat_id}`, with
        // chat_id now the SENDER — must be LISTED by `.agent asks`
        // arriving as a DM. Before the fix the lookup ran on
        // `irc:{our own nick}` and every DM reported "no open
        // questions".
        // Approval, not Clarify: Clarify parses ANY text as a free-text
        // answer, which would swallow the `.agent asks` line before the
        // command prefix is ever seen. Approval parses only yes/no/
        // amend, so the command survives to the handler under test.
        let req = veles_core::ask::AskRequest::new(
            "s",
            veles_core::ask::AskKind::Approval,
            "approve rm?",
            vec![veles_core::ask::AskChoice::new("once", "once")],
        );
        let id = req.id.clone();
        let _rx = veles_core::ask::register(&req, Some("irc:wrath"), None);

        let sent = drive(|_| {}, vec![":wrath!u@h PRIVMSG velestest :.asks".into()]).await;
        veles_core::ask::cancel(&id);
        assert!(
            sent.iter()
                .any(|l| l.starts_with("NOTICE wrath") && l.contains("approve rm?")),
            "a DM must resolve asks keyed on the sender, got: {sent:?}"
        );
    }

    /// One scratch VELES_HOME for the whole stub-server binary.
    ///
    /// The authorization paths read the LIVE `[acl]` section, so
    /// without this the corpus judged senders against whatever config
    /// the developer happens to have — the failure mode this repo has
    /// already paid for once ("passed on a configured machine and
    /// failed anywhere else"). Set once, to the same value, so no test
    /// races another for the variable; an empty home means the ACL is
    /// dormant, which is the posture these cases are about.
    fn isolate_home() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let dir = veles_core::scratch::kept_dir();
            std::env::set_var("VELES_HOME", &dir);
            veles_core::acl::invalidate_live_config();
        });
    }

    async fn drive_full_with_delay(
        cfg_tweak: impl FnOnce(&mut IrcConfig),
        advertise: &str,
        script: Vec<String>,
        ops: Vec<IrcCommand>,
        op_delay: Duration,
    ) -> Vec<String> {
        drive_wire(cfg_tweak, advertise, script, ops, Vec::new(), op_delay).await
    }

    /// …and `outbounds` are gateway REPLIES pushed into the client's outbound
    /// channel once registration has settled, so a test can assert what the
    /// WIRE carries rather than what the body assembler returned.
    async fn drive_wire(
        cfg_tweak: impl FnOnce(&mut IrcConfig),
        advertise: &str,
        script: Vec<String>,
        ops: Vec<IrcCommand>,
        outbounds: Vec<OutboundMessage>,
        op_delay: Duration,
    ) -> Vec<String> {
        drive_wire_timed(
            cfg_tweak,
            advertise,
            script,
            ops,
            outbounds,
            op_delay,
            Duration::from_millis(400),
        )
        .await
        .into_iter()
        .map(|(_, line)| line)
        .collect()
    }

    /// …and WHEN each line reached the stub's socket, with the stub
    /// reading until the client has been quiet for `quiet`. Pacing is a
    /// property of the wire: the 2026-10-10 Excess Flood passed every
    /// test that counted lines, because the lines were all there — they
    /// had just left in one burst.
    async fn drive_wire_after_caps(
        cfg_tweak: impl FnOnce(&mut IrcConfig),
        after_caps: Vec<String>,
        script: Vec<String>,
    ) -> Vec<(Duration, String)> {
        drive_wire_after_caps_impl(cfg_tweak, after_caps, script).await
    }

    async fn drive_wire_after_caps_impl(
        cfg_tweak: impl FnOnce(&mut IrcConfig),
        after_caps: Vec<String>,
        script: Vec<String>,
    ) -> Vec<(Duration, String)> {
        isolate_home();
        let t0 = std::time::Instant::now();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let mut cfg = IrcConfig {
            server: "127.0.0.1".into(),
            port,
            use_tls: false,
            nickname: "velestest".into(),
            username: "velestest".into(),
            realname: "velestest".into(),
            name: "stub".into(),
            ..IrcConfig::default()
        };
        cfg_tweak(&mut cfg);
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.expect("accept");
            let (rd, mut wr) = sock.into_split();
            let mut lines = BufReader::new(rd).lines();
            let mut got = Vec::new();
            let mut acknowledged = false;
            loop {
                match tokio::time::timeout(Duration::from_secs(3), lines.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        got.push((t0.elapsed(), line.clone()));
                        if line.starts_with("CAP LS") {
                            wr.write_all(b":stub CAP * LS :sasl\r\n").await.unwrap();
                        } else if line.starts_with("CAP REQ") {
                            acknowledged = true;
                            wr.write_all(b":stub CAP * ACK :sasl\r\n").await.unwrap();
                        } else if line == "AUTHENTICATE PLAIN" {
                            wr.write_all(b"AUTHENTICATE +\r\n").await.unwrap();
                        } else if line.starts_with("AUTHENTICATE ") {
                            wr.write_all(b":stub 904 velestest :SASL failed\r\n")
                                .await
                                .unwrap();
                        } else if line.starts_with("CAP END") {
                            wr.write_all(b":stub 001 velestest :Welcome\r\n")
                                .await
                                .unwrap();
                            break;
                        } else if line.starts_with("USER ") {
                            // NICK/USER completes the client's part of
                            // registration; the server may now send the
                            // scripted numerics before its own welcome.
                            for scripted in &after_caps {
                                wr.write_all(scripted.as_bytes()).await.unwrap();
                                wr.write_all(b"\r\n").await.unwrap();
                            }
                            wr.write_all(b":stub 001 velestest :Welcome\r\n")
                                .await
                                .unwrap();
                            break;
                        }
                    }
                    _ => panic!("registration stopped early: {got:?}"),
                }
            }
            assert!(acknowledged, "the fixture must negotiate SASL: {got:?}");
            for line in &script {
                wr.write_all(line.as_bytes()).await.unwrap();
                wr.write_all(b"\r\n").await.unwrap();
            }
            wr.flush().await.unwrap();
            while let Ok(Ok(Some(line))) =
                tokio::time::timeout(Duration::from_millis(600), lines.next_line()).await
            {
                got.push((t0.elapsed(), line));
            }
            got
        });
        let (_out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut pending = std::collections::VecDeque::new();
        let mut commands = std::collections::VecDeque::new();
        let client = tokio::spawn(async move {
            let _ = run_connection_inner(
                &cfg,
                None,
                "bot",
                &mut out_rx,
                &mut cmd_rx,
                &mut pending,
                &mut commands,
            )
            .await;
        });
        let sent = server.await.expect("stub server");
        client.await.expect("client must not panic");
        sent
    }

    #[derive(Default)]
    struct WireEvents {
        early: Vec<String>,
        script: Vec<String>,
        ops: Vec<IrcCommand>,
        outbounds: Vec<OutboundMessage>,
    }

    #[derive(Clone, Copy)]
    struct WireTiming {
        op_delay: Duration,
        quiet: Duration,
    }

    async fn drive_wire_timed(
        cfg_tweak: impl FnOnce(&mut IrcConfig),
        advertise: &str,
        script: Vec<String>,
        ops: Vec<IrcCommand>,
        outbounds: Vec<OutboundMessage>,
        op_delay: Duration,
        quiet: Duration,
    ) -> Vec<(Duration, String)> {
        drive_wire_timed_early(
            cfg_tweak,
            advertise,
            WireEvents {
                script,
                ops,
                outbounds,
                early: Vec::new(),
            },
            WireTiming { op_delay, quiet },
        )
        .await
    }

    async fn drive_wire_timed_early(
        cfg_tweak: impl FnOnce(&mut IrcConfig),
        advertise: &str,
        events: WireEvents,
        timing: WireTiming,
    ) -> Vec<(Duration, String)> {
        let WireEvents {
            early: early_script,
            script,
            ops,
            outbounds,
        } = events;
        let WireTiming { op_delay, quiet } = timing;
        isolate_home();
        let t0 = std::time::Instant::now();
        let advertise = advertise.to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let mut cfg = IrcConfig {
            server: "127.0.0.1".into(),
            port,
            use_tls: false,
            nickname: "velestest".into(),
            username: "velestest".into(),
            realname: "velestest".into(),
            name: "stub".into(),
            ..IrcConfig::default()
        };
        cfg_tweak(&mut cfg);

        // The server side: accept, read whatever the client sends until
        // it goes quiet, answer with the script, then close. Closing is
        // what makes `run_connection_inner` return rather than hang.
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.expect("accept");
            let (rd, mut wr) = sock.into_split();
            let mut lines = BufReader::new(rd).lines();

            // Registration has to actually COMPLETE, or the corpus runs
            // against a client that is still negotiating and most of it
            // is never reached. (It did: the first version of this stub
            // only ever saw "CAP LS 302", because nothing answered the
            // capability probe and the client sat waiting for it. The
            // positive control in the authorization test is what caught
            // that — without one, five green tests were testing an
            // unregistered connection.)
            let mut got: Vec<(Duration, String)> = Vec::new();
            let mut sent_welcome = false;
            loop {
                let next =
                    tokio::time::timeout(Duration::from_millis(300), lines.next_line()).await;
                match next {
                    Ok(Ok(Some(line))) => {
                        got.push((t0.elapsed(), line.clone()));
                        if line.starts_with("CAP LS") {
                            // Empty by default: the client has nothing to
                            // REQ and goes straight to CAP END.
                            let _ = wr
                                .write_all(format!(":stub CAP * LS :{advertise}\r\n").as_bytes())
                                .await;
                        } else if line.starts_with("CAP REQ") {
                            // The trailing colon is OPTIONAL: irc-proto
                            // emits `CAP REQ :a b` for several caps and
                            // `CAP REQ one` for exactly one. Splitting on
                            // ':' therefore ACKed an EMPTY list whenever a
                            // single cap was requested — the connection
                            // registered, `conn.caps` stayed empty, and
                            // every cap-gated assertion measured the
                            // ungated path while looking like it had
                            // negotiated. Take everything after the verb.
                            let want = line
                                .trim_start_matches("CAP REQ")
                                .trim_start()
                                .trim_start_matches(':')
                                .trim();
                            let _ = wr
                                .write_all(format!(":stub CAP * ACK :{want}\r\n").as_bytes())
                                .await;
                        } else if !sent_welcome
                            && (line.starts_with("CAP END") || line.starts_with("USER "))
                        {
                            sent_welcome = true;
                            // Hostile-but-legal sequencing: the server
                            // sends lines before the welcome. The client
                            // must still be negotiating while it reads them.
                            for line in &early_script {
                                let _ = wr.write_all(line.as_bytes()).await;
                                let _ = wr.write_all(b"\r\n").await;
                            }
                            let _ = wr.flush().await;
                            // Give the client's reader a chance to process
                            // an early line before registration completes.
                            if !early_script.is_empty() {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                            let _ = wr.write_all(b":stub 001 velestest :Welcome\r\n").await;
                            let _ = wr.flush().await;
                        }
                    }
                    // Quiet or closed: the client has said its piece.
                    _ => break,
                }
            }
            assert!(
                sent_welcome,
                "the client never registered — the corpus would run against a \
                 half-negotiated connection. Sent: {got:?}"
            );

            for line in &script {
                let _ = wr.write_all(line.as_bytes()).await;
                let _ = wr.write_all(b"\r\n").await;
            }
            let _ = wr.flush().await;

            // And KEEP READING. What the client says in ANSWER to the
            // script is the whole point — a NOTICE it sends back, or a
            // CTCP reply it must not send. Stopping at the drain above
            // meant `got` held only the registration handshake, so
            // every assertion about the client's replies was looking at
            // an empty set and passing for that reason.
            // A local Tcl evaluation may legitimately take up to 30s.
            // Each expected reply therefore gets a bounded processing
            // wait, not an idle-socket assumption of 400ms. Once all
            // arrive the ordinary short drain still applies. The stub
            // otherwise closed the socket while eval_line was running,
            // turning a correct late reply into a flaky empty result.
            // Explicit eval spellings each finish with one NOTICE,
            // including a refusal. Bare `tcl` is deliberately excluded:
            // without a configured bridge it has no reply to wait for.
            let mut pending_evals = script
                .iter()
                .filter(|line| {
                    line.split_once(" :").is_some_and(|(_, text)| {
                        text.split_ascii_whitespace().any(|word| {
                            matches!(
                                word,
                                ".tcl" | "/tcl" | ".eval" | "/eval" | ".tcladmin" | "/tcladmin"
                            )
                        })
                    })
                })
                .count();
            loop {
                let wait = if pending_evals > 0 {
                    Duration::from_secs(35)
                } else {
                    quiet
                };
                let next = tokio::time::timeout(wait, lines.next_line()).await;
                match next {
                    Ok(Ok(Some(line))) => {
                        if pending_evals > 0
                            && (line.starts_with("NOTICE ") || line.starts_with("PRIVMSG "))
                        {
                            pending_evals -= 1;
                        }
                        got.push((t0.elapsed(), line));
                    }
                    _ => break,
                }
            }
            drop(wr);
            got
        });

        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        // AFTER registration, or the cap-gated ops are evaluated against
        // an empty `conn.caps` and every one of them is dropped — which is
        // indistinguishable from the gate working.
        if !ops.is_empty() || !outbounds.is_empty() {
            let reply_tx = out_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(op_delay).await;
                for op in ops {
                    let _ = cmd_tx.send(op);
                }
                for out in outbounds {
                    let _ = reply_tx.send(out);
                }
            });
        }
        let client = tokio::spawn(async move {
            // Ok or Err are both fine; a PANIC is not, and that is what
            // the join below catches.
            let mut pending = std::collections::VecDeque::new();
            let mut commands = std::collections::VecDeque::new();
            let _ = run_connection_inner(
                &cfg,
                None,
                "bot",
                &mut out_rx,
                &mut cmd_rx,
                &mut pending,
                &mut commands,
            )
            .await;
        });

        let sent = server.await.expect("the stub server must not panic");
        client
            .await
            .expect("the CLIENT must not panic on anything a server sends");
        // The outbound sender outlives the client on purpose: dropping it
        // first would close `out_rx` under a connection that is still
        // running, which reads as a shutdown rather than an idle channel.
        drop(out_tx);
        sent
    }

    /// Every numeric the client handles, in every shape a malformed
    /// server can produce: missing arguments, empty arguments, and one
    /// absurdly long one.
    fn malformed_corpus() -> Vec<String> {
        let long = "x".repeat(4096);
        let mut out = Vec::new();
        for code in [
            "001", "005", "324", "330", "332", "333", "352", "353", "366", "311", "312", "318",
            "330", "433", "900", "903", "904",
        ] {
            // No arguments at all.
            out.push(format!(":stub {code}"));
            // One argument — the handlers all want at least two.
            out.push(format!(":stub {code} velestest"));
            // Empty trailing.
            out.push(format!(":stub {code} velestest :"));
            // Empty middle arguments.
            out.push(format!(":stub {code}   :"));
            // A very long one.
            out.push(format!(":stub {code} velestest {long}"));
            // No prefix at all.
            out.push(format!("{code} velestest #chan :x"));
        }
        out
    }

    #[tokio::test]
    async fn malformed_numerics_do_not_panic_the_adapter() {
        // The WSA-2020-1/2/3 class. Nothing here should be survivable
        // by luck: `args.get()` is why it is survivable by design, and
        // this is what keeps somebody from writing `args[1]`.
        drive(|_| {}, malformed_corpus()).await;
    }

    #[tokio::test]
    async fn empty_and_degenerate_nicks_do_not_panic() {
        // irssi CVE-2017-9468 and its siblings: a NULL dereference for
        // an "empty" nick, and for a message with no source nick/host.
        drive(
            |_| {},
            vec![
                ":stub 353 velestest = #chan :".into(),
                ":stub 353 velestest = #chan :@ + ~ %".into(),
                ": PRIVMSG #chan :no nick in the prefix".into(),
                ":! PRIVMSG #chan :degenerate prefix".into(),
                ":@ JOIN #chan".into(),
                ":!@ MODE #chan +o".into(),
                ":stub 352 velestest #chan u h s  G :0 r".into(),
                ":stub 352 velestest".into(),
                "PRIVMSG #chan :no prefix at all".into(),
                ":stub 324 velestest".into(),
                ":stub 324 velestest #chan".into(),
                ":stub 005 velestest".into(),
                ":stub 005 velestest PREFIX= :are supported".into(),
                ":stub 005 velestest PREFIX=(qaohv)~&@%+ NICKLEN= MSGLEN= :x".into(),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn a_353_flood_does_not_grow_without_bound() {
        // WSA-2026-3's shape end to end: 353 forever, 366 never. The
        // unit test on `merge_roster` proves the cap; this proves the
        // cap is actually on the path a server can reach.
        let mut script = Vec::new();
        for i in 0..2000 {
            script.push(format!(
                ":stub 353 velestest = #flood :n{i}a n{i}b n{i}c n{i}d n{i}e"
            ));
        }
        drive(|_| {}, script).await;
    }

    #[tokio::test]
    async fn a_nick_matching_an_allowed_account_is_still_refused() {
        // The one the unit tests could not reach: `is_authorized` must
        // consult the verified services ACCOUNT and never the nick.
        // slopdrop authorises on hostmask, which is spoofable; this is
        // the reason veles does not.
        //
        // "trusted" is an allowed ACCOUNT name. A stranger simply
        // NAMING themselves trusted, with no account behind it, must
        // not be authorised.
        //
        // The observable is `.agent status`: past the authorization
        // check it answers with a NOTICE built purely from `conn`, so
        // it needs no bus, no store and no model. The first draft of
        // this test asserted "no PRIVMSG is sent", which was VACUOUS —
        // no reply is sent either way without an LLM configured, so
        // granting authorization changed nothing observable and the
        // mutation that broke `is_authorized` stayed green.
        let msg = ":trusted!u@h PRIVMSG velestest :.irc";

        // The negative: named, not verified, refused.
        let refused = drive(
            |cfg| {
                cfg.allowed_users = ["trusted".to_string()].into_iter().collect();
            },
            vec![msg.into()],
        )
        .await;
        assert!(
            !refused.iter().any(|l| l.starts_with("NOTICE")),
            "a sender with no verified account must be refused, got: {refused:?}"
        );

        // The positive control, without which the above proves nothing:
        // the SAME message, on a connection with no allowlist, does
        // produce the NOTICE. So the test can see the difference.
        let allowed = drive(|cfg| cfg.allowed_users.clear(), vec![msg.into()]).await;
        assert!(
            allowed.iter().any(|l| l.starts_with("NOTICE")),
            "open mode must answer, or this test cannot tell refusal from \
             a client that never replies at all — got: {allowed:?}"
        );
    }

    #[tokio::test]
    async fn a_ctcp_other_than_action_gets_no_automatic_reply() {
        // CTCP auto-replies are the classic amplification primitive:
        // send VERSION to a channel, every client answers the target of
        // your choosing. veles handles only ACTION, and this is what
        // keeps a well-meaning VERSION handler from being added without
        // somebody thinking about it first.
        let sent = drive(
            |_| {},
            vec![
                ":a!u@h PRIVMSG velestest :\u{1}VERSION\u{1}".into(),
                ":a!u@h PRIVMSG velestest :\u{1}PING 12345\u{1}".into(),
                ":a!u@h PRIVMSG velestest :\u{1}TIME\u{1}".into(),
                ":a!u@h PRIVMSG velestest :\u{1}CLIENTINFO\u{1}".into(),
                ":a!u@h PRIVMSG velestest :\u{1}DCC SEND x 0 0 0\u{1}".into(),
                ":a!u@h PRIVMSG #chan :\u{1}VERSION\u{1}".into(),
            ],
        )
        .await;
        assert!(
            !sent.iter().any(|l| l.contains('\u{1}')),
            "no CTCP reply may be generated, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn an_over_long_line_drops_the_connection_instead_of_buffering() {
        // The vendored irc-proto cap, reached the way a server would
        // reach it. 64 KiB with no terminator anywhere.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let cfg = IrcConfig {
            server: "127.0.0.1".into(),
            port,
            use_tls: false,
            nickname: "velestest".into(),
            username: "velestest".into(),
            realname: "velestest".into(),
            name: "stub".into(),
            ..IrcConfig::default()
        };
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.expect("accept");
            let (_rd, mut wr) = sock.into_split();
            let _ = wr.write_all(b":stub 001 velestest :Welcome\r\n").await;
            // No newline, ever.
            let _ = wr.write_all(&vec![b'x'; 64 * 1024]).await;
            let _ = wr.flush().await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        let (_out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            run_connection_inner(
                &cfg,
                None,
                "bot",
                &mut out_rx,
                &mut cmd_rx,
                &mut std::collections::VecDeque::new(),
                &mut std::collections::VecDeque::new(),
            ),
        )
        .await
        .expect("the connection must END, not sit there buffering");
        let err = res.expect_err("an unterminated line must be an error");
        assert!(
            format!("{err:#}").contains("no terminator"),
            "the reason must survive to the log, got: {err:#}"
        );
    }

    /// eggdrop's `.chanset` bug, in our command prefix.
    ///
    /// USER: "I guess eggdrop and eggdrop derivations was also tcl and
    /// probably had a history of tests and CVEs". Before 1.3.25 eggdrop
    /// let ordinary users drive `.chanset` and "make the bot perform
    /// virtually any function"; CVE-2004-0274 was unauthorised
    /// promotion to a sharebot. Both are one mistake: a command prefix
    /// whose authorization is coarser than what its subcommands do.
    ///
    /// veles had it in open mode, which is the DEFAULT. `.agent
    /// restore` rolls back the agent's file edits and `.agent approve`
    /// grants a pending approval — the gate in front of tool use.
    ///
    /// The refusal used to be "needs a registered account", which is
    /// services gating and had to go (USER, 2026-09-09: the ACL host
    /// masks are authoritative). What replaced it is not weaker: with
    /// NO `[acl]` and an open transport the ACL is dormant and resolves
    /// every sender as OWNER, so there is nobody to be privileged
    /// ABOVE — and the refusal now says which two lines of config fix
    /// that, instead of telling someone to identify to a service their
    /// network may not run.
    #[tokio::test]
    async fn privileged_agent_commands_refuse_when_no_boundary_is_declared() {
        // Batches of at most four. `flood_burst` is 4 at 1/sec, so a
        // fifth reply is held about a second by the outbound token
        // bucket and lands after the stub stops reading — which looks
        // exactly like the gate silently dropping it. The bucket is
        // right; the first draft of this test was wrong.
        for batch in [
            vec![
                ":stranger!u@h PRIVMSG velestest :.restore 1",
                ":stranger!u@h PRIVMSG velestest :.approve abc",
                ":stranger!u@h PRIVMSG velestest :.deny abc",
                ":stranger!u@h PRIVMSG velestest :.answer abc yes",
            ],
            vec![":stranger!u@h PRIVMSG velestest :.irc nick pwned"],
            // The ACL additions ride the same list: listing grants and
            // mutating roles are acting, not reporting.
            vec![
                ":stranger!u@h PRIVMSG velestest :.acl",
                ":stranger!u@h PRIVMSG velestest :.grant irc:x trusted",
                ":stranger!u@h PRIVMSG velestest :.revoke irc:x",
            ],
        ] {
            let want = batch.len();
            let sent = drive(
                |cfg| cfg.allowed_users.clear(), // open mode
                batch.into_iter().map(String::from).collect(),
            )
            .await;
            let notices: Vec<&String> = sent.iter().filter(|l| l.starts_with("NOTICE")).collect();
            assert_eq!(
                notices.len(),
                want,
                "each must be ANSWERED, not silently dropped — a stranger who \
                 gets no reply cannot tell refusal from a broken bot: {sent:?}"
            );
            for notice in &notices {
                assert!(
                    notice.contains("no [acl] section and an open transport"),
                    "every acting subcommand must refuse when no boundary exists, got: {notice}"
                );
                assert!(
                    !notice.contains("registered account"),
                    "…and it must not send anyone to NickServ: {notice}"
                );
            }
            // The bot's nick is unchanged: `.agent nick` never fired.
            assert!(
                !sent.iter().any(|l| l.starts_with("NICK pwned")),
                "a stranger must not be able to rename the bot: {sent:?}"
            );
        }
    }

    /// Every ACTING `.agent` subcommand names a capability, and every
    /// REPORTING one stays open — that is what open mode is for. The
    /// table is the authorization: a subcommand that grows here without
    /// an entry is reachable by anyone who can see the bot, which is
    /// the eggdrop `.chanset` mistake.
    /// The ACL verbs must ROUTE to the shared implementation, not
    /// reimplement it.
    ///
    /// They used to be hand-rolled: a grant listing that read the
    /// store directly, a sandbox lister that knew about container keys
    /// but not about sandbox NAMES, no `explain` and no `limits` at
    /// all — and the copies had already drifted from every other
    /// surface. Textual, like `every_shared_command_reaches_every_
    /// surface`: the BEHAVIOUR is pinned once in veles-core
    /// (`acl_subcommands_are_tiered_inside_the_command`,
    /// `sandbox_management_is_tiered_and_the_names_are_checked`,
    /// `limits_read_open_and_set_admin_and_the_set_is_live`), and what
    /// this defends is that IRC still goes through it.
    #[test]
    fn the_acl_verbs_route_to_the_shared_implementation() {
        let src = include_str!("client.rs");
        for f in [
            "commands::acl_command",
            "commands::limits_command",
            "commands::sandbox_command",
        ] {
            assert!(
                src.contains(f),
                "IRC must reach `{f}` rather than growing its own copy"
            );
        }
        // And every verb has to be covered, or one of them answers
        // "listed as immediate but has no arm". `irc_immediate_and_the_
        // arms_agree` checks the whole table; this names the family.
        for verb in [
            "acl",
            "access",
            "grant",
            "revoke",
            "limits",
            "sandbox",
            "sandboxes",
            "lockdown",
        ] {
            assert!(
                IRC_IMMEDIATE.contains(&verb),
                "`{verb}` must be answered here, not queued behind a running turn"
            );
        }
        assert!(
            src.contains("commands::lockdown_command"),
            "the brake reaches the shared implementation too"
        );
        // The old hand-rolled bodies are gone, not merely bypassed.
        // Split literals: this file is its own haystack, so spelling
        // them out here would match the test rather than the code.
        for gone in [
            concat!("listing grants needs ", "admin access"),
            concat!("sandbox management needs ", "admin access"),
            concat!("unknown sandboxes ", "subcommand"),
        ] {
            assert!(
                !src.contains(gone),
                "a hand-rolled copy is still here: {gone:?}"
            );
        }
    }

    #[test]
    fn every_immediate_irc_command_names_a_capability() {
        use veles_core::acl::Cap;
        for (cmd, want) in [
            ("answer", Cap::Approve),
            ("approve", Cap::Approve),
            ("deny", Cap::Approve),
            // `acl`, `sandbox`/`sandboxes`, `limits` and `lockdown`
            // answer a question ABOUT THE CALLER in their bare form and
            // manage the rules in their subcommands, so the transport
            // gate is the bare form's tier and the subcommands are
            // tiered in the shared implementation. The pin that matters
            // for them is the SUBCOMMAND one — `acl_subcommands_are_
            // tiered_inside_the_command` in veles-core — not this
            // table. `irc` is the same shape: the report is open,
            // `irc nick` is checked at the arm.
            ("acl", Cap::CmdStatus),
            ("sandboxes", Cap::CmdStatus),
            ("sandbox", Cap::CmdStatus),
            ("limits", Cap::CmdStatus),
            ("lockdown", Cap::CmdStatus),
            ("irc", Cap::CmdStatus),
            ("grant", Cap::All),
            ("revoke", Cap::All),
            ("tcl", Cap::Shell),
            ("eval", Cap::Shell),
        ] {
            assert_eq!(
                irc_command_cap(cmd),
                Some(want),
                "`{cmd}` must require {want:?}"
            );
        }
        // Reporting is open — a stranger may ask what the bot is and
        // what they themselves may do.
        for cmd in ["help", "caps", "asks"] {
            assert_eq!(
                irc_command_cap(cmd),
                Some(Cap::CmdStatus),
                "`{cmd}` must stay open at the reporting tier"
            );
        }
        // …and a name this adapter does not answer has no opinion here
        // at all: the shared dispatcher tiers it.
        assert_eq!(irc_command_cap("frobnicate"), None);
    }

    /// The guard at the top of `handle_irc_command` and the arms below
    /// it must be the same set. A name in the list with no arm answers
    /// "this is a bug"; an arm not in the list is dead code, and the
    /// command silently goes to the gateway instead — where it would
    /// wait behind the very turn it was meant to interrupt.
    #[test]
    fn irc_immediate_and_the_arms_agree() {
        let src = include_str!("client.rs");
        let body = src
            .split("async fn handle_irc_command(")
            .nth(1)
            .expect("the immediate handler");
        for cmd in IRC_IMMEDIATE {
            let arm = format!("\"{cmd}\"");
            assert!(
                body.contains(&arm),
                "`{cmd}` is declared immediate but no arm matches it"
            );
        }
        // Every IRC-only command is immediate by definition — it needs
        // the live connection, so the bus cannot serve it.
        for cmd in IRC_COMMANDS {
            assert!(
                IRC_IMMEDIATE.contains(cmd),
                "`{cmd}` exists only here but is not answered here"
            );
        }
        // And every immediate name is a real command, so `known_command`
        // can never disagree with the table.
        for cmd in IRC_IMMEDIATE {
            assert!(
                known_command(cmd),
                "`{cmd}` is answered but is not in any inventory"
            );
        }
    }

    /// The far end of the chain `every_dispatcher_arm_is_declared_and_
    /// tiered` starts: a shared command must be ANSWERED here, not
    /// forwarded to the model as a prompt.
    ///
    /// `known_command` derives its inventory from `builtin_cap` rather
    /// than keeping a list, which is why this holds for free today — and
    /// exactly why it is worth pinning. The day that derivation is
    /// replaced by a hand-written list, every shared command that list
    /// forgets becomes a billed turn asking an LLM to interpret
    /// `/checkpoints`.
    #[test]
    fn every_shared_command_is_answered_here_not_prompted() {
        for c in veles_frontends::commands::SHARED_COMMANDS {
            let bare = c.trim_start_matches('/');
            assert!(
                known_command(bare),
                "`{c}` is a shared command but this adapter does not recognise it, \
                 so `{c}` reaches the model as a prompt and costs a turn"
            );
        }
    }

    /// The sigil is one character, so "is this a command" is a real
    /// question — and getting it wrong turns ordinary typing into
    /// "unknown command".
    #[test]
    fn a_sigil_needs_a_letter_after_it() {
        assert!(is_command_line(".", ".acl list"));
        assert!(is_command_line(".", ". acl list"), "a space is forgivable");
        assert!(!is_command_line(".", "...anyway"), "ellipsis is not a verb");
        assert!(!is_command_line(".", ".oO(hmm)"), "( is not a letter");
        assert!(!is_command_line(".", ".42"), "nor is a digit");
        assert!(!is_command_line(".", "hello"));
        assert!(!is_command_line(".", "."));
        // A longer sigil still works, and still needs the letter.
        assert!(is_command_line("!bot ", "!bot acl"));
        assert!(!is_command_line("!bot ", "!bot ..."));
        assert!(!is_command_line("", "acl"), "no sigil, no commands");
        // The migration path documented in docs/acl.md: an operator who
        // wants the old two-word form back sets the old prefix, and it
        // works verbatim because the prefix is stripped before the name
        // is read.
        assert!(is_command_line(".agent ", ".agent acl list"));
        assert!(!is_command_line(".agent ", ".acl list"));
    }

    /// `known_command` is the difference between answering "unknown
    /// command" and handing `/frobnicate` to the model as a prompt.
    #[test]
    fn the_inventory_knows_shared_commands_and_not_nonsense() {
        for name in ["acl", "queue", "compact", "model", "lockdown", "sandbox"] {
            assert!(known_command(name), "`{name}` is a real command");
        }
        for name in ["irc", "tcl"] {
            assert!(known_command(name), "`{name}` lives on this surface");
        }
        // The one the fold removed, which is exactly what someone with
        // muscle memory will type.
        assert!(!known_command("agent"), "`.agent` is gone");
        assert!(!known_command("frobnicate"));
        assert!(!known_command(""));
    }

    /// While we are here: the bot does not flood in reply to a flood.
    /// eggdrop's history is full of this, and it is the property that
    /// made the test above look broken — worth pinning rather than
    /// leaving as something you rediscover by writing a failing test.
    #[tokio::test]
    async fn a_burst_of_commands_does_not_produce_a_burst_of_replies() {
        let sent = drive(
            |cfg| cfg.allowed_users.clear(),
            (0..12)
                .map(|i| format!(":stranger!u@h PRIVMSG velestest :.irc {i}"))
                .collect(),
        )
        .await;
        let notices = sent.iter().filter(|l| l.starts_with("NOTICE")).count();
        assert!(
            notices <= 5,
            "the outbound bucket must pace replies (burst 4 at 1/sec), got {notices}: {sent:?}"
        );
        assert!(
            notices > 0,
            "…but it must not stop answering altogether: {sent:?}"
        );
    }

    /// The 2026-10-10 Excess Flood (SynIRC, #flashsupport): a 9 KB
    /// answer, 42 IRC lines, left the socket in ONE write, and the
    /// server quit the bot mid-sentence. The bucket WAS consulted —
    /// `send_line` slept before every `client.send` — but `client.send`
    /// only queues. The irc crate's writer (`Outgoing`) lived inside the
    /// read stream and ran only when the select loop polled it, which
    /// the loop could not do while it sat in the handler doing the
    /// sleeping. Every line waited for the last one, and they went
    /// together.
    ///
    /// The count tests above could not see it: the lines were all
    /// there. This one times them at the socket.
    ///
    /// Mutation check: hand `Outgoing` back to `client.stream()` and
    /// this goes red with a spread of about a millisecond.
    #[tokio::test]
    async fn a_long_reply_leaves_the_socket_at_the_buckets_pace() {
        let text = (1..=8)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let sent = drive_wire_timed(
            |c| {
                c.flood_burst = 2;
                c.flood_per_sec = 10.0;
            },
            "",
            vec![],
            vec![],
            vec![OutboundMessage::plain("#chan", &text, MessageKind::Reply)],
            Duration::from_millis(150),
            // Longer than the whole reply takes, so a burst at the END
            // is still read and measured rather than missed.
            Duration::from_millis(1500),
        )
        .await;
        let at: Vec<Duration> = sent
            .iter()
            .filter(|(_, l)| l.starts_with("PRIVMSG #chan :line"))
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(at.len(), 8, "every line must arrive: {sent:?}");
        // Two go at once (the burst), the other six 100 ms apart, so the
        // first is on the wire about 600 ms before the last.
        let spread = at[7] - at[0];
        assert!(
            spread >= Duration::from_millis(400),
            "the reply left in one burst, {spread:?} from first line to last: {at:?}"
        );
    }

    /// An inbound command is not a free pass around registration: the
    /// adapter must not act on it before 001, even if a hostile server
    /// writes it during CAP negotiation. A later, identical command is
    /// the positive control that proves the post-registration path works.
    #[tokio::test]
    async fn inbound_commands_wait_for_registration() {
        let sent = drive_wire_timed_early(
            |c| {
                c.channels = crate::parse_channels("#veles", c.default_format);
                c.allowed_users = ["op!u@h"].into_iter().map(String::from).collect();
            },
            "",
            WireEvents {
                early: vec![":op!u@h PRIVMSG velestest :.asks".into()],
                script: vec![":op!u@h PRIVMSG velestest :.asks".into()],
                ..WireEvents::default()
            },
            WireTiming {
                op_delay: Duration::ZERO,
                quiet: Duration::from_millis(400),
            },
        )
        .await;
        let notices: Vec<_> = sent
            .iter()
            .filter(|(_, line)| line.starts_with("NOTICE op "))
            .collect();
        assert_eq!(
            notices.len(),
            1,
            "the pre-001 command must not execute; the post-001 control must execute once: {sent:?}"
        );
        let join = sent
            .iter()
            .find(|(_, line)| line.starts_with("JOIN "))
            .expect("the welcome must complete the configured join");
        assert!(
            notices[0].0 >= join.0,
            "an inbound command answered before registration completed: {sent:?}"
        );
    }

    /// Welcome sends the configured JOINs on every new connection, then
    /// drains commands retained by an attempt that never registered.
    /// Both sequences owe the connection's line and server-lag budgets.
    /// A one-token burst and 10 tokens/s make every adjacent pair prove
    /// the wait; this is not a fixed expectation from the first attempt.
    #[tokio::test]
    async fn reconnect_join_backlogs_are_paced_after_registration() {
        isolate_home();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let cfg = IrcConfig {
            server: "127.0.0.1".into(),
            port,
            use_tls: false,
            nickname: "velestest".into(),
            username: "velestest".into(),
            realname: "velestest".into(),
            name: "stub".into(),
            channels: (0..3)
                .map(|i| crate::ChannelSpec {
                    name: format!("#configured{i}"),
                    key: None,
                    require_mention: None,
                    format: crate::FormatMode::Irc,
                })
                .collect(),
            flood_burst: 1,
            flood_per_sec: 10.0,
            ..IrcConfig::default()
        };
        let (_out_tx, out_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        for i in 0..3 {
            let (reply, _rx) = oneshot::channel();
            cmd_tx
                .send(IrcCommand::Join {
                    channel: format!("#deferred{i}"),
                    key: None,
                    reply,
                })
                .unwrap();
        }
        let (reply, _rx) = oneshot::channel();
        cmd_tx
            .send(IrcCommand::Part {
                channel: "#configured0".into(),
                reason: None,
                reply,
            })
            .unwrap();
        let client = tokio::spawn(async move {
            let first = run_connection(
                &cfg,
                None,
                "bot",
                out_rx,
                cmd_rx,
                std::collections::VecDeque::new(),
                std::collections::VecDeque::new(),
            )
            .await;
            let (out_rx, cmd_rx, pending, commands) = match first {
                Ok((o, c, p, q, _, _)) => (o, c, p, q),
                Err((_, o, c, p, q)) => (o, c, p, q),
            };
            assert_eq!(
                commands.len(),
                4,
                "the unregistered attempt must retain every command"
            );
            let _ = run_connection(&cfg, None, "bot", out_rx, cmd_rx, pending, commands).await;
        });
        let start = std::time::Instant::now();
        let expected = [
            "JOIN #configured0",
            "JOIN #configured1",
            "JOIN #configured2",
            "JOIN #deferred0",
            "JOIN #deferred1",
            "JOIN #deferred2",
            "PART #configured0",
        ];
        let mut sent = Vec::new();
        for attempt in 0..2 {
            let (sock, _) = listener.accept().await.expect("accept");
            let (rd, mut wr) = sock.into_split();
            let mut lines = BufReader::new(rd).lines();
            let mut cap_ended = false;
            let mut nick_seen = false;
            while let Ok(Some(line)) =
                tokio::time::timeout(Duration::from_secs(3), lines.next_line())
                    .await
                    .unwrap()
            {
                if line.starts_with("CAP LS") {
                    wr.write_all(b":stub CAP * LS :\r\n").await.unwrap();
                } else if line.starts_with("CAP END") {
                    cap_ended = true;
                    wr.write_all(b":stub CAP * ACK :\r\n").await.unwrap();
                    if attempt == 0 {
                        drop(wr);
                        break;
                    }
                } else if line.starts_with("NICK ") {
                    nick_seen = true;
                } else if line.starts_with("USER ") && cap_ended && nick_seen {
                    wr.write_all(b":stub 001 velestest :Welcome\r\n")
                        .await
                        .unwrap();
                } else if line.starts_with("JOIN ") || line.starts_with("PART ") {
                    assert_eq!(attempt, 1, "an unregistered connection sent {line}");
                    assert_eq!(line, expected[sent.len()], "FIFO order");
                    sent.push(start.elapsed());
                    if sent.len() == expected.len() {
                        drop(wr);
                        break;
                    }
                }
            }
        }
        tokio::time::timeout(Duration::from_secs(5), client)
            .await
            .expect("both connection attempts must finish")
            .unwrap();
        assert_eq!(sent.len(), expected.len());
        for (previous, next) in sent.iter().zip(sent.iter().skip(1)) {
            assert!(
                next.saturating_sub(*previous) >= Duration::from_millis(60),
                "the reconnect command backlog bypassed the line budget: {sent:?}"
            );
        }
    }

    /// The deferred services fallback is ordinary traffic too: after
    /// SASL fails, IDENTIFY precedes the configured joins and shares
    /// their line budget rather than going out in one welcome burst.
    #[tokio::test]
    async fn deferred_identify_and_welcome_joins_share_the_line_budget() {
        let sent = drive_wire_after_caps(
            |c| {
                c.sasl_mechanism = Some("PLAIN".into());
                c.sasl_username = Some("velestest".into());
                c.sasl_password = Some("hunter2".into());
                c.channels = crate::parse_channels("#one,#two,#three", c.default_format);
                c.flood_burst = 1;
                c.flood_per_sec = 10.0;
            },
            vec![":stub 904 velestest :SASL failed".into()],
            Vec::new(),
        )
        .await;
        let traffic: Vec<_> = sent
            .iter()
            .filter(|(_, line)| {
                line.starts_with("PRIVMSG NickServ :IDENTIFY ") || line.starts_with("JOIN ")
            })
            .collect();
        assert_eq!(traffic.len(), 4, "one IDENTIFY and three joins: {sent:?}");
        assert!(traffic[0].1.starts_with("PRIVMSG NickServ :IDENTIFY "));
        for (previous, next) in traffic.iter().zip(traffic.iter().skip(1)) {
            assert!(
                next.0.saturating_sub(previous.0) >= Duration::from_millis(60),
                "the deferred IDENTIFY bypassed the join budget: {sent:?}"
            );
        }
    }

    /// An output produced during connection setup waits for RPL_WELCOME,
    /// survives an attempt that never registers, and is delivered once on
    /// the next connection. This is the same ordering rule ergo's retained
    /// sessions exercise on reconnect: the IRC session may survive, but
    /// this connection has not negotiated the right to send yet.
    #[tokio::test]
    async fn unregistered_output_is_deferred_then_sent_after_reconnect_registration() {
        isolate_home();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let cfg = IrcConfig {
            server: "127.0.0.1".into(),
            port,
            use_tls: false,
            nickname: "velestest".into(),
            username: "velestest".into(),
            realname: "velestest".into(),
            name: "stub".into(),
            ..IrcConfig::default()
        };
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        out_tx
            .send(OutboundMessage::plain(
                "#chan",
                "queued while offline",
                MessageKind::Reply,
            ))
            .unwrap();
        let client = tokio::spawn(async move {
            let first = run_connection(
                &cfg,
                None,
                "bot",
                out_rx,
                cmd_rx,
                std::collections::VecDeque::new(),
                std::collections::VecDeque::new(),
            )
            .await;
            let (out_rx, cmd_rx, pending, commands) = match first {
                Ok((o, c, p, q, _, _)) => (o, c, p, q),
                Err((_, o, c, p, q)) => (o, c, p, q),
            };
            assert_eq!(
                pending.len(),
                1,
                "the first attempt must not consume the output"
            );
            let second = run_connection(&cfg, None, "bot", out_rx, cmd_rx, pending, commands).await;
            let pending = match second {
                Ok((_, _, p, _, _, _)) => p,
                Err((_, _, _, p, _)) => p,
            };
            assert!(
                pending.is_empty(),
                "a registered connection delivered the queue"
            );
        });
        for attempt in 0..2 {
            let (sock, _) = listener.accept().await.expect("accept");
            let (rd, mut wr) = sock.into_split();
            let mut lines = BufReader::new(rd).lines();
            let mut cap_ended = false;
            let mut nick_seen = false;
            let mut registered = false;
            while let Ok(Some(line)) =
                tokio::time::timeout(Duration::from_secs(3), lines.next_line())
                    .await
                    .unwrap()
            {
                if line.starts_with("CAP LS") {
                    wr.write_all(b":stub CAP * LS :\r\n").await.unwrap();
                } else if line.starts_with("CAP END") {
                    cap_ended = true;
                    wr.write_all(b":stub CAP * ACK :\r\n").await.unwrap();
                    if attempt == 0 {
                        // This attempt dies before the server's welcome.
                        drop(wr);
                        break;
                    }
                } else if line.starts_with("NICK ") {
                    nick_seen = true;
                } else if line.starts_with("USER ") && cap_ended && nick_seen {
                    registered = true;
                    wr.write_all(b":stub 001 velestest :Welcome\r\n")
                        .await
                        .unwrap();
                } else if line.starts_with("PRIVMSG") {
                    assert_eq!(attempt, 1, "an unregistered connection sent: {line}");
                    assert!(registered);
                    assert_eq!(line, "PRIVMSG #chan :queued while offline");
                    drop(wr);
                    break;
                }
            }
        }
        tokio::time::timeout(Duration::from_secs(5), client)
            .await
            .expect("both connection attempts must finish")
            .unwrap();
    }

    /// An approval ask skips the line bucket but not the server's
    /// charge: a plan-mode approval carries up to 30 lines of detail
    /// (`ask::render_prompt`), and thirty long lines at once are an
    /// Excess Flood by themselves. The server parses them in order
    /// whenever they are sent, so holding them until it would costs the
    /// human nothing.
    ///
    /// Mutation check: skip `server_lag` when `paced` is false and the
    /// sixth detail line goes at once: red.
    #[tokio::test]
    async fn a_long_approval_ask_is_paced_by_the_servers_charge_too() {
        let mut req = veles_core::ask::AskRequest::new(
            "s",
            veles_core::ask::AskKind::Approval,
            "run the plan?",
            vec![veles_core::ask::AskChoice::new("once", "once")],
        );
        req.detail = Some(
            (1..=6)
                .map(|i| format!("{i} {}", "lorem ".repeat(60).trim_end()))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let mut out = OutboundMessage::plain("#chan", "", MessageKind::Ask);
        out.ask = Some(req);
        let sent = drive_wire_timed(
            |_| {},
            "",
            vec![],
            vec![],
            vec![out],
            Duration::from_millis(150),
            Duration::from_millis(2000),
        )
        .await;
        let at: Vec<Duration> = sent
            .iter()
            .filter(|(_, l)| l.starts_with("PRIVMSG #chan :") && l.contains("lorem"))
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(at.len(), 6, "one IRC line per detail line: {sent:?}");
        // The header and five detail lines make 8 s of server lag, the
        // budget: they go at once, past the default bucket's burst of 4.
        assert!(
            at[4] - at[0] < Duration::from_millis(300),
            "an ask is not held by the line bucket: {at:?}"
        );
        assert!(
            at[5] - at[0] >= Duration::from_millis(700),
            "the sixth detail line would wait in the server's receive queue: {at:?}"
        );
    }

    /// …and the pace has to be the SERVER's. UnrealIRCd charges a line
    /// by its size (500 ms, plus 500 ms per 180 bytes, for a known user)
    /// and parses nothing more while a client is ten seconds in debt;
    /// what it has not parsed waits in the receive queue, and a queue
    /// past 8000 bytes is the Excess Flood quit. A per-line bucket
    /// cannot keep that: the VPS's 10 at 5/s (2026-09-12) sends 400-byte
    /// lines at five times the rate SynIRC parses them.
    ///
    /// Six 380-byte lines with the line bucket wide open: 1500 ms of
    /// server lag each, so five fit the 8-second budget and go at once —
    /// a short answer must not get slower — and the sixth waits about a
    /// second, which is when the server would have parsed it anyway.
    ///
    /// Mutation check: charge every line the flat 500 ms (drop the byte
    /// term) and the sixth goes at once: red.
    #[tokio::test]
    async fn long_lines_are_paced_by_what_the_server_charges_for_them() {
        let line = |i: usize| format!("{i} {}", "lorem ".repeat(60).trim_end());
        let text = (1..=6).map(line).collect::<Vec<_>>().join("\n");
        let sent = drive_wire_timed(
            |c| {
                c.flood_burst = 100;
                c.flood_per_sec = 100.0;
            },
            "",
            vec![],
            vec![],
            vec![OutboundMessage::plain("#chan", &text, MessageKind::Reply)],
            Duration::from_millis(150),
            Duration::from_millis(2000),
        )
        .await;
        let at: Vec<Duration> = sent
            .iter()
            .filter(|(_, l)| l.starts_with("PRIVMSG #chan :") && l.contains("lorem"))
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(at.len(), 6, "one IRC line per text line: {sent:?}");
        assert!(
            at[4] - at[0] < Duration::from_millis(300),
            "five lines fit the server's budget and must go at once: {at:?}"
        );
        assert!(
            at[5] - at[0] >= Duration::from_millis(700),
            "the sixth would wait in the server's receive queue; it must wait here: {at:?}"
        );
    }

    /// The regression the user hit on 2026-09-04: a multi-line reply
    /// arrived on IRC as ONE run-on line — paragraphs glued, no breaks.
    /// `privmsg` sanitized the whole text (stripping every `\n`) before
    /// the split that turns lines into IRC lines. Now the split runs
    /// first, so three lines become three PRIVMSG lines.
    #[tokio::test]
    async fn a_multiline_reply_reaches_the_wire_as_separate_lines() {
        let (tx, _rx) = oneshot::channel();
        let sent = drive_full(
            |_| {},
            "",
            vec![],
            vec![IrcCommand::Say {
                target: "#chan".into(),
                text: "line one\n\nline three".into(),
                mode: Some(FormatMode::Plain),
                reply: tx,
            }],
        )
        .await;
        let privmsgs: Vec<&String> = sent
            .iter()
            .filter(|l| l.starts_with("PRIVMSG #chan"))
            .collect();
        assert_eq!(
            privmsgs.len(),
            2,
            "blank separator lines are skipped (empty PRIVMSGs are spam), \
             content lines each get their own: {privmsgs:?}"
        );
        assert!(privmsgs[0].contains("line one"), "{privmsgs:?}");
        assert!(privmsgs[1].contains("line three"), "{privmsgs:?}");
        assert!(
            !privmsgs
                .iter()
                .any(|l| l.contains("\nline") || l.contains("one line three")),
            "the lines must NOT be glued: {privmsgs:?}"
        );
    }

    /// Reasoning gating — extracted into `outbound_body` so the default
    /// is testable without a wire. Off by default (channels are not a
    /// place for thinking dumps), on by explicit config.
    #[test]
    fn outbound_body_gates_the_reasoning_prefix() {
        let mut out = OutboundMessage::plain("#chan", "the answer", MessageKind::Reply);
        out.reasoning = Some("I thought hard about this".into());
        out.provider = "custom:ali".into();
        out.model = "deepseek".into();

        let hidden = outbound_body(&IrcConfig::default(), &out);
        assert!(
            !hidden.contains("thought"),
            "show_reasoning=false must drop the dump: {hidden:?}"
        );
        assert!(hidden.starts_with("the answer"), "{hidden:?}");

        let loud = IrcConfig {
            show_reasoning: true,
            ..IrcConfig::default()
        };
        let shown = outbound_body(&loud, &out);
        assert!(
            shown.starts_with("> I thought hard about this"),
            "show_reasoning=true keeps the blockquote: {shown:?}"
        );
        assert!(shown.contains("the answer"), "{shown:?}");
        assert!(shown.ends_with("custom:ali · deepseek"), "{shown:?}");
    }

    #[test]
    fn outbound_body_empty_reasoning_adds_nothing_even_when_enabled() {
        let mut out = OutboundMessage::plain("#chan", "answer", MessageKind::Reply);
        out.reasoning = Some(String::new());
        let cfg = IrcConfig {
            show_reasoning: true,
            ..IrcConfig::default()
        };
        assert_eq!(outbound_body(&cfg, &out), "answer");
    }

    /// The route footer — `— provider · model` — was the one part of a
    /// reply no knob could turn off: it went out whenever the gateway named
    /// a provider, which is every reply. `show_route` is that knob. On by
    /// default, so nothing that worked changes shape; off means the answer
    /// arrives with no trailing line and no blank line where the line was.
    /// This is display only — the provider still travels on the payload for
    /// anything that reads it structurally.
    ///
    /// Mutation: dropping `cfg.show_route &&` from the footer arm fails the
    /// half that turns it off; flipping the default fails the half that
    /// keeps it on.
    #[test]
    fn show_route_gates_the_provider_model_footer() {
        let mut out = OutboundMessage::plain("#chan", "the answer", MessageKind::Reply);
        out.provider = "custom:ali".into();
        out.model = "qwen3.8-flash".into();

        assert!(
            IrcConfig::default().show_route,
            "the footer is on unless a channel says otherwise"
        );
        let shown = outbound_body(&IrcConfig::default(), &out);
        assert!(
            shown.ends_with("\n— custom:ali · qwen3.8-flash"),
            "on keeps the route on the wire: {shown:?}"
        );

        let quiet = IrcConfig {
            show_route: false,
            ..IrcConfig::default()
        };
        let hidden = outbound_body(&quiet, &out);
        assert_eq!(
            hidden, "the answer",
            "off must leave the answer and nothing else: {hidden:?}"
        );
        assert!(
            !hidden.contains('—') && !hidden.ends_with('\n'),
            "no em-dash and no trailing newline where the footer was: {hidden:?}"
        );
    }

    /// A reply that cannot name its route prints no footer at all — a bare
    /// em-dash would be a line about nothing.
    #[test]
    fn no_route_no_footer_even_when_show_route_is_on() {
        let out = OutboundMessage::plain("#chan", "the answer", MessageKind::Reply);
        assert_eq!(
            outbound_body(&IrcConfig::default(), &out),
            "the answer",
            "the provider is empty, so the footer has nothing to say"
        );
    }

    /// The same knob, end to end: config → connection → wire.
    ///
    /// `outbound_body` is the assembly point and the tests above pin it, but
    /// nothing pinned that `handle_outbound` hands it the CONNECTION's config.
    /// A default-constructed one there compiles, passes every unit test in
    /// this file, and prints the route in every channel no matter what the
    /// operator wrote — which is precisely the complaint `show_route` exists
    /// to answer.
    ///
    /// Mutation: `outbound_body(&IrcConfig::default(), &out)` in
    /// `handle_outbound` fails the off half of this test and nothing else.
    #[tokio::test]
    async fn show_route_off_reaches_the_wire() {
        let reply = || {
            let mut out = OutboundMessage::plain("#chan", "the answer", MessageKind::Reply);
            out.provider = "custom:ali".into();
            out.model = "qwen3.8-flash".into();
            out
        };

        let on = drive_wire(
            |_| {},
            "",
            vec![],
            vec![],
            vec![reply()],
            Duration::from_millis(150),
        )
        .await;
        assert!(
            on.iter()
                .any(|l| l.starts_with("PRIVMSG #chan") && l.contains("custom:ali")),
            "on by default, the route must reach the channel: {on:?}"
        );

        let off = drive_wire(
            |c| c.show_route = false,
            "",
            vec![],
            vec![],
            vec![reply()],
            Duration::from_millis(150),
        )
        .await;
        assert!(
            off.iter()
                .any(|l| l.starts_with("PRIVMSG #chan") && l.contains("the answer")),
            "the answer itself is not optional: {off:?}"
        );
        assert!(
            !off.iter()
                .any(|l| l.contains("custom:ali") || l.contains('—')),
            "show_route=false must leave the route off the wire: {off:?}"
        );
    }

    /// IRC has no upload, so its delivery method is a copy into a directory
    /// something else serves, and the channel gets the LINK. What it must
    /// never get is the marker: that was a host path in a public channel.
    #[test]
    fn media_becomes_a_link_in_the_served_directory() {
        let dir = tempfile::tempdir().unwrap();
        let served = dir.path().join("www");
        let src = dir.path().join("my_chart.png");
        std::fs::write(&src, b"png-bytes").unwrap();
        let cfg = IrcConfig {
            media_public_dir: Some(served.clone()),
            // A trailing slash is the spelling an operator copies out of an
            // nginx config; the join must not produce `//`.
            media_public_url: "https://media.example.org/".into(),
            ..IrcConfig::default()
        };

        let body = media_body(&cfg, &format!("here you go\nMEDIA:{}\nbye", src.display()));

        let url = body
            .lines()
            .find(|l| l.starts_with("https://"))
            .unwrap_or_else(|| panic!("the link is the delivery: {body:?}"));
        assert_eq!(
            url.matches('/').count(),
            3,
            "one slash between base and name: {url}"
        );
        assert!(
            url.ends_with("my-chart.png"),
            "the name is sanitized: {url}"
        );
        assert!(
            body.contains("here you go") && body.contains("bye"),
            "{body:?}"
        );
        assert!(!body.contains("MEDIA:"), "no marker survives: {body:?}");
        assert!(
            !body.contains(&src.display().to_string()),
            "the channel never sees where the file came from: {body:?}"
        );

        let published: Vec<_> = std::fs::read_dir(&served)
            .expect("the served directory was created")
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(published.len(), 1, "{published:?}");
        assert_eq!(std::fs::read(&published[0]).unwrap(), b"png-bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&published[0])
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o644,
                "a web server that is not us must be able to read it"
            );
        }
    }

    /// The default is NO delivery: nothing is configured, so nothing is
    /// copied and nothing is linked — and the note that replaces it carries
    /// no path, because the reason a file was not delivered is not a reason
    /// to publish where it lives.
    #[test]
    fn no_public_dir_means_a_note_and_never_a_path() {
        let body = media_body(&IrcConfig::default(), "look:\nMEDIA:/etc/passwd");
        assert!(body.contains("not delivered"), "{body:?}");
        assert!(!body.contains("MEDIA:"), "{body:?}");
        assert!(
            !body.contains("/etc/passwd"),
            "no host path on the wire: {body:?}"
        );
    }

    /// Half a configuration is not a configuration: a directory with no URL
    /// would fill a disk nobody can reach, so it delivers nothing and says so
    /// in the log (warned on connect, and again per file).
    #[test]
    fn a_half_configured_directory_delivers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("chart.png");
        std::fs::write(&src, b"png").unwrap();
        let cfg = IrcConfig {
            media_public_dir: Some(dir.path().join("www")),
            ..IrcConfig::default()
        };
        let body = media_body(&cfg, &format!("MEDIA:{}", src.display()));
        assert!(body.contains("not delivered"), "{body:?}");
        assert!(
            !dir.path().join("www").exists(),
            "nothing is copied where nothing serves it"
        );
    }

    /// The cap is about the copy, not the read: a sparse file costs nothing to
    /// create and still has to be refused on its SIZE, before it is copied
    /// onto a web server's disk.
    #[test]
    fn an_oversized_file_is_not_published() {
        let dir = tempfile::tempdir().unwrap();
        let served = dir.path().join("www");
        let big = dir.path().join("disk.img");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&big).unwrap();
            f.write_all(b"hdr").unwrap();
            f.set_len(MAX_PUBLISH_BYTES + 1).unwrap();
        }
        let cfg = IrcConfig {
            media_public_dir: Some(served.clone()),
            media_public_url: "https://media.example.org".into(),
            ..IrcConfig::default()
        };
        let body = media_body(&cfg, &format!("MEDIA:{}", big.display()));
        assert!(body.contains("not delivered"), "{body:?}");
        assert!(
            !served.exists() || std::fs::read_dir(&served).unwrap().count() == 0,
            "the oversized file was refused before it was copied"
        );
    }

    /// The published name comes from a path in a model's reply, so it is
    /// reduced before it is joined onto a served directory: no separators, no
    /// climb, no leading `~`, and no `_` (italic to the markdown converter on
    /// the way to the wire, which is how a working link becomes a 404).
    #[test]
    fn a_published_name_cannot_climb_or_italicize() {
        assert_eq!(
            veles_core::publish::sanitize_name("my_chart.png"),
            "my-chart.png"
        );
        for raw in [
            "../../etc/passwd",
            "/etc/passwd",
            "~/x.png",
            "..",
            "",
            "....",
        ] {
            let name = veles_core::publish::sanitize_name(raw);
            assert!(!name.is_empty(), "{raw:?} still has to name something");
            assert!(!name.contains('/'), "{raw:?} -> {name:?}");
            assert!(!name.contains(".."), "{raw:?} -> {name:?}");
            assert!(!name.starts_with('~'), "{raw:?} -> {name:?}");
            assert!(!name.contains('_'), "{raw:?} -> {name:?}");
        }
        assert_eq!(
            veles_core::publish::sanitize_name(&"x".repeat(300)).len(),
            80
        );
    }

    /// Who may write the text a turn reads.
    ///
    /// Every unaddressed line in the channel reaches the model as
    /// `irc_context` on the next ADDRESSED turn, so "whose words may
    /// steer a turn" is a different question from "who may start one",
    /// and `require_mention` only answers the second. The default is
    /// `guest` — everyone not banned, which is what the tap already did
    /// — and the value of the knob is that it can be raised for a room
    /// of strangers.
    ///
    /// Held as a pure decision so it needs no config file and cannot
    /// race the other tests in this binary for `VELES_HOME`.
    #[test]
    fn the_context_block_has_a_role_floor_and_an_explicit_chat_denial_is_under_it() {
        use veles_core::acl::{AclConfig, SenderContext};
        let ctx = |nick: &str| SenderContext {
            mask: Some(format!("{nick}!u@h")),
            network: Some("net".into()),
            channel: Some("#chan".into()),
            platform: Some("irc".into()),
            chat_id: Some("#chan".into()),
            direct: false,
            handle: None,
            post: None,
        };
        // A CONFIGURED acl: unlisted senders are guests (fail-closed),
        // one identity is a user, one is a guest with `chat` denied.
        let mut acl = AclConfig {
            default_role: Some("guest".into()),
            ..Default::default()
        };
        acl.roles.insert("irc:friend".into(), "user".into());
        acl.deny_caps
            .insert("irc:muted".into(), vec!["chat".into()]);
        assert!(acl.is_configured());

        // The default floor changes nothing: a guest still contributes,
        // exactly as before this knob existed.
        assert!(
            context_verdict(true, "guest", &acl, Some("irc:stranger"), &ctx("stranger")),
            "the default must not silently blind the bot to the channel"
        );
        // Raised to `user`, the stranger stops contributing and the
        // named friend keeps doing so.
        assert!(!context_verdict(
            true,
            "user",
            &acl,
            Some("irc:stranger"),
            &ctx("stranger")
        ));
        assert!(context_verdict(
            true,
            "user",
            &acl,
            Some("irc:friend"),
            &ctx("friend")
        ));
        // Raised again, the friend goes under the line too.
        assert!(!context_verdict(
            true,
            "trusted",
            &acl,
            Some("irc:friend"),
            &ctx("friend")
        ));
        // A speaker who cannot take a turn never contributes, whatever
        // the floor says — they are refused the turn their words would
        // otherwise steer.
        assert!(!context_verdict(
            true,
            "guest",
            &acl,
            Some("irc:muted"),
            &ctx("muted")
        ));
        // An unparseable floor falls back to the documented default
        // rather than opening or closing the gate by accident.
        assert!(context_verdict(
            true,
            "archduke",
            &acl,
            Some("irc:stranger"),
            &ctx("stranger")
        ));
        // Unauthorized at the transport (banned, or off the legacy
        // allowlist) is still the first and hardest no.
        assert!(!context_verdict(
            false,
            "guest",
            &acl,
            Some("irc:x"),
            &ctx("x")
        ));
        // And a DORMANT acl is untouched: there the adapter allowlist is
        // the boundary, and resolve_ctx would call every stranger owner.
        let dormant = AclConfig::default();
        assert!(!dormant.is_configured());
        assert!(context_verdict(
            true,
            "trusted",
            &dormant,
            Some("irc:stranger"),
            &ctx("stranger")
        ));
    }

    /// A published file is served from a web root, so its EXTENSION is
    /// an instruction to a browser.
    ///
    /// The marker gate (`media::deliverable`) lets a below-owner sender
    /// name their own sandbox workspace, and `Role::User` holds
    /// `FileWrite` there — so `MEDIA:/work/evil.html` was a legitimate
    /// marker, copied into the served directory with its extension
    /// intact and handed out as a link. That is arbitrary
    /// attacker-authored content hosted under the operator's own
    /// domain: `nosniff` does not help, because the file really is
    /// `text/html`. `.svg` is the same story with script in it.
    ///
    /// So the name is the second gate: media keeps its extension,
    /// everything else is published as `.bin` and served as a download.
    /// Delivery is not refused — the file still arrives, it just cannot
    /// pretend to be a page.
    #[test]
    fn only_media_extensions_survive_publication() {
        // The shapes a channel would actually see.
        assert_eq!(safe_published_name("chart.png"), "chart.png");
        assert_eq!(safe_published_name("clip.MP4"), "clip.MP4");
        assert_eq!(safe_published_name("say.opus"), "say.opus");
        assert_eq!(safe_published_name("paper.pdf"), "paper.pdf");
        // …and the ones that must not be believed.
        for hostile in [
            "evil.html",
            "evil.htm",
            "evil.svg",
            "evil.xhtml",
            "payload.js",
            "sheet.xml",
            "page.HTML",
        ] {
            let out = safe_published_name(hostile);
            assert!(
                out.ends_with(".bin"),
                "{hostile} must not keep an extension a browser executes: {out}"
            );
        }
        // An extensionless name is not media either, and the sanitizer's
        // own rules still apply first (no path separators, no leading
        // dots, `_` folded for the markdown converter).
        assert_eq!(safe_published_name("README"), "README.bin");
        assert!(!safe_published_name("../../etc/passwd").contains('/'));
        assert_eq!(safe_published_name("my_shot.png"), "my-shot.png");
        // A double extension resolves on the LAST one, which is the one
        // a web server reads.
        assert_eq!(safe_published_name("evil.png.html"), "evil.png.html.bin");
    }

    /// Expiry is a config answer: an unset age deletes nothing, a set
    /// age deletes only what is older than it, and nothing else in the
    /// directory — not subdirectories, not symlinks, not fresh files —
    /// is ever touched. A sweeper that recursed or followed links would
    /// eventually meet a planted entry; `publish` writes flat, so flat
    /// is all it cleans.
    #[test]
    fn the_expiry_sweep_deletes_only_stale_flat_files() {
        let dir = std::env::temp_dir().join(format!(
            "veles-media-sweep-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fresh = dir.join("fresh.png");
        std::fs::write(&fresh, b"new").unwrap();
        let old = dir.join("old.png");
        std::fs::write(&old, b"ancient").unwrap();
        // mtime far in the past: older than any plausible cutoff.
        let ancient = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400 * 365);
        #[cfg(not(unix))]
        let _ = ancient;
        #[cfg(unix)]
        {
            let f = std::fs::OpenOptions::new().write(true).open(&old).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(ancient))
                .unwrap();
        }
        let subdir = dir.join("nested");
        std::fs::create_dir_all(&subdir).unwrap();
        let stale_inside = subdir.join("stale.png");
        std::fs::write(&stale_inside, b"planted or nested").unwrap();
        #[cfg(unix)]
        {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(&stale_inside)
                .unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(ancient))
                .unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&old, dir.join("link.png")).unwrap();

        let mut cfg = IrcConfig {
            media_public_dir: Some(dir.clone()),
            ..Default::default()
        };
        assert_eq!(sweep_published_media(&cfg), None, "0 means never");
        assert!(old.exists(), "0 means nothing was touched");

        cfg.media_public_max_age_days = 7;
        let removed = sweep_published_media(&cfg).expect("age is set");
        assert_eq!(removed, 1, "exactly the one stale flat file");
        assert!(!old.exists(), "the stale file is gone");
        assert!(fresh.exists(), "the fresh file stays");
        assert!(
            stale_inside.exists(),
            "the sweep is flat: nothing inside a subdirectory is touched"
        );
        assert!(
            dir.join("link.png").symlink_metadata().is_ok(),
            "symlinks are skipped, not followed or deleted"
        );

        // And an absurd age neither panics on the subtraction nor
        // wraps into deleting everything: checked_sub saturates to the
        // epoch, nothing predates the epoch, nothing is touched.
        std::fs::write(&old, b"again").unwrap();
        cfg.media_public_max_age_days = u64::MAX / 8;
        let removed2 = sweep_published_media(&cfg).expect("age is set");
        assert_eq!(
            removed2, 0,
            "an absurd age is 'nothing is that old', not 'delete everything'"
        );
        assert!(old.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End to end, because the assembler is not the wire: a reply carrying a
    /// marker goes out as one PRIVMSG with the URL on it, and the URL survives
    /// `markdown_to_irc` and the outbound sanitizer intact — a link with eaten
    /// characters is a link that 404s, and only this test would notice.
    ///
    /// Mutation: drop `media_body` from `outbound_body` and this fails with
    /// the raw marker on the wire, while every unit test above still passes.
    #[tokio::test]
    async fn a_media_reply_reaches_the_wire_as_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let served = dir.path().join("www");
        let src = dir.path().join("chart.png");
        std::fs::write(&src, b"png").unwrap();
        let src = src.display().to_string();

        let src_for_reply = src.clone();
        let reply = move || {
            OutboundMessage::plain(
                "#chan",
                format!("here is the chart\nMEDIA:{src_for_reply}"),
                MessageKind::Reply,
            )
        };

        let served_for_cfg = served.clone();
        let on = drive_wire(
            move |c| {
                c.media_public_dir = Some(served_for_cfg.clone());
                c.media_public_url = "https://media.example.org".into();
            },
            "",
            vec![],
            vec![],
            vec![reply()],
            Duration::from_millis(200),
        )
        .await;

        let link = on
            .iter()
            .find(|l| l.starts_with("PRIVMSG #chan") && l.contains("https://media.example.org/"))
            .unwrap_or_else(|| panic!("the link must reach the channel: {on:?}"));
        assert!(
            !on.iter().any(|l| l.contains("MEDIA:")),
            "no marker on the wire: {on:?}"
        );
        assert!(
            !on.iter().any(|l| l.contains(&src)),
            "no host path on the wire: {on:?}"
        );
        let name = link.rsplit('/').next().unwrap();
        assert!(
            served.join(name).is_file(),
            "the URL names the file that was actually published: {link:?}"
        );
    }

    /// Mid-turn `Progress` traffic: tool markers are forwarded when the
    /// posture narrates to THIS destination, while typing indicators and
    /// re-sent streaming partials stay dropped — they have no IRC
    /// analogue and would be churn.
    #[test]
    fn progress_markers_are_forwarded_but_partials_are_not() {
        use crate::ToolProgress;
        // A channel marker: only an explicit On forwards it. The shipped
        // Posture stays quiet in a shared channel (the #coven flood fix).
        let tool = OutboundMessage::plain("#chan", "⚙ terminal…", MessageKind::Progress);
        assert_eq!(
            progress_body(&tool, ToolProgress::On, "#chan").as_deref(),
            Some("⚙ terminal…"),
            "an explicit On forwards markers even to a channel"
        );
        assert_eq!(
            progress_body(&tool, ToolProgress::Posture, "#chan"),
            None,
            "the default posture does NOT narrate into a shared channel"
        );
        assert_eq!(
            progress_body(&tool, ToolProgress::Off, "#chan"),
            None,
            "tool_progress=off drops markers"
        );
        // A DM: the posture narrates — one reader who asked.
        let dm = OutboundMessage::plain("someuser", "⚙ terminal…", MessageKind::Progress);
        assert_eq!(
            progress_body(&dm, ToolProgress::Posture, "someuser").as_deref(),
            Some("⚙ terminal…"),
            "the default posture narrates in a DM"
        );

        let typing = OutboundMessage::plain("#chan", "", MessageKind::Progress);
        assert_eq!(
            progress_body(&typing, ToolProgress::On, "#chan"),
            None,
            "typing has no text"
        );

        let partial = OutboundMessage::plain("#chan", "partial delta text", MessageKind::Progress);
        assert_eq!(
            progress_body(&partial, ToolProgress::On, "#chan"),
            None,
            "streaming partials re-send the whole buffer and are churn"
        );
    }

    /// Under lockdown the adapter answers nothing it would otherwise
    /// answer BEFORE the bus — an ask reply and the command sigil —
    /// and says why exactly once.
    ///
    /// The gateway has its own gate, but these two paths never reach
    /// it: a locked-down bot that still approved tool calls for
    /// whoever typed "1" would be locked down in name only.
    #[tokio::test]
    async fn lockdown_silences_the_adapters_own_answers() {
        // A pending approval, so "1" has something to resolve.
        let req = veles_core::ask::AskRequest::new(
            "s",
            veles_core::ask::AskKind::Approval,
            "approve rm?",
            vec![veles_core::ask::AskChoice::new("once", "once")],
        );
        let id = req.id.clone();
        let _rx = veles_core::ask::register(&req, Some("irc:#lk"), None);

        // Park a store for THIS network only: the registry is
        // process-global and every other test in this binary must keep
        // seeing no lockdown at all.
        isolate_home();
        let net = "lockdown-test-net";
        let dir = veles_core::scratch::kept_dir();
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            veles_core::store::Store::open(&dir.join("lk.db")).expect("store"),
        ));
        veles_core::lockdown::engage(&store, "tui:owner", "an incident").unwrap();
        crate::client::park_store(store.clone(), net);

        let sent = drive(
            |cfg| {
                cfg.allowed_users.clear();
                cfg.name = net.to_string();
            },
            vec![
                ":stranger!u@h PRIVMSG velestest :1".into(),
                ":stranger!u@h PRIVMSG velestest :.irc".into(),
                ":stranger!u@h PRIVMSG velestest :hello?".into(),
            ],
        )
        .await;
        let notices: Vec<&String> = sent.iter().filter(|l| l.starts_with("NOTICE")).collect();
        assert_eq!(
            notices.len(),
            1,
            "one notice for the whole lockdown, not one per line: {sent:?}"
        );
        assert!(
            notices[0].contains("locked down"),
            "and it says what happened: {notices:?}"
        );
        assert!(
            !sent.iter().any(|l| l.contains("veles-irc: nick=")),
            "the command must not have run: {sent:?}"
        );
        assert!(
            !sent.iter().any(|l| l.contains("decision recorded")),
            "the ask must not have been answered: {sent:?}"
        );
        assert!(
            !veles_core::ask::open_for_chat("irc:#lk").is_empty(),
            "the approval is still pending, not resolved"
        );
        veles_core::ask::cancel(&id);
        veles_core::lockdown::release(&store).unwrap();
    }

    /// The other half, without which the above proves only that the bot
    /// is broken: REPORTING stays open, because that is what open mode
    /// is for. Splitting by damage, not by mode.
    #[tokio::test]
    async fn reporting_agent_commands_still_work_in_open_mode() {
        let sent = drive(
            |cfg| cfg.allowed_users.clear(),
            vec![
                ":stranger!u@h PRIVMSG velestest :.irc".into(),
                ":stranger!u@h PRIVMSG velestest :.help".into(),
            ],
        )
        .await;
        let notices: Vec<&String> = sent.iter().filter(|l| l.starts_with("NOTICE")).collect();
        // One notice each. The help listing used to outgrow an IRC
        // line and split into two sends; it is PAGED now (`.help
        // <topic>`), because the alternative — carrying the history
        // filters, the sandbox verbs and the ask family in one flat
        // listing — is a four-line flood the channel did not ask for.
        assert_eq!(notices.len(), 2, "both must be answered: {sent:?}");
        assert!(
            notices.iter().any(|n| n.contains("help <topic>")),
            "the bare listing must offer the topics: {notices:?}"
        );
        assert!(
            notices.iter().any(|n| n.contains("veles-irc: network=")),
            "the connection report must actually report: {notices:?}"
        );
        assert!(
            !notices
                .iter()
                .any(|n| n.contains("needs a registered account")),
            "reporting must NOT be gated: {notices:?}"
        );
    }

    /// The fold, end to end on the real path: the sigil answers what
    /// it owns, forwards what the shared dispatcher owns, and says so
    /// for a name that is neither — including `agent`, which is what
    /// muscle memory will type.
    #[tokio::test]
    async fn the_sigil_answers_forwards_or_says_unknown() {
        let sent = drive(
            |cfg| {
                cfg.allowed_users.clear();
                // A boundary, so the commands are not all refused for
                // being on an undeclared bot — this test is about
                // ROUTING.
                cfg.allowed_users.insert("stranger!*@*".into());
            },
            vec![
                // Ours: answered here, as a NOTICE.
                ":stranger!u@h PRIVMSG velestest :.help".into(),
                // The dispatcher's: forwarded to the bus, so nothing
                // goes out on the wire from this adapter.
                ":stranger!u@h PRIVMSG velestest :.queue".into(),
                // Neither.
                ":stranger!u@h PRIVMSG velestest :.frobnicate".into(),
                // The spelling the fold removed.
                ":stranger!u@h PRIVMSG velestest :.agent acl".into(),
            ],
        )
        .await;
        let notices: Vec<&String> = sent.iter().filter(|l| l.starts_with("NOTICE")).collect();
        assert_eq!(
            notices.len(),
            3,
            "help + two unknowns are answered here; /queue is not: {sent:?}"
        );
        assert!(
            notices[0].contains("runs any command"),
            "the sigil's own help: {notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|n| n.contains("unknown command: frobnicate")),
            "a name nobody implements is answered, not fed to the model: {notices:?}"
        );
        let agent = notices
            .iter()
            .find(|n| n.contains("unknown command: agent"))
            .expect("`.agent` is gone and says so");
        assert!(
            agent.contains(".acl list") && agent.contains(".agent acl list"),
            "…and the answer shows the new spelling next to the old: {agent}"
        );
        assert!(
            !sent.iter().any(|l| l.contains("queue")),
            "a shared command must go to the dispatcher, not be answered here: {sent:?}"
        );
    }

    /// Ordinary typing that happens to start with the sigil is not a
    /// command. One character of prefix makes this a real risk, and
    /// "unknown command: oO(hmm)" in a channel is worse than silence.
    #[tokio::test]
    async fn ellipses_and_emoticons_are_not_commands() {
        let sent = drive(
            |cfg| cfg.allowed_users.clear(),
            vec![
                ":stranger!u@h PRIVMSG velestest :...anyway, what about it".into(),
                ":stranger!u@h PRIVMSG velestest :.oO(thinking)".into(),
                ":stranger!u@h PRIVMSG velestest :.42 is the answer".into(),
            ],
        )
        .await;
        assert!(
            !sent.iter().any(|l| l.starts_with("NOTICE")),
            "none of these is a command: {sent:?}"
        );
    }

    /// A long field from the server, which is eggdrop's CVE-2007-2807
    /// (overly long PRIVMSG from a malicious server → overflow), and
    /// the empty one that is CVE-2008-4450 — the DoS introduced BY the
    /// incorrect fix for the first. Both are already in the numeric
    /// corpus; this is the PRIVMSG path specifically, which is the one
    /// eggdrop got wrong twice.
    #[tokio::test]
    async fn absurd_and_empty_privmsg_bodies_are_survivable() {
        drive(
            |_| {},
            vec![
                format!(":a!u@h PRIVMSG velestest :{}", "x".repeat(8000)),
                ":a!u@h PRIVMSG velestest :".into(),
                ":a!u@h PRIVMSG velestest".into(),
                ":a!u@h PRIVMSG :".into(),
                ":a!u@h PRIVMSG".into(),
                format!(":a!u@h PRIVMSG #{} :hi", "c".repeat(4000)),
                format!(":{}!u@h PRIVMSG velestest :hi", "n".repeat(4000)),
                ":a!u@h NOTICE velestest :".into(),
                ":a!u@h PRIVMSG velestest :\u{1}".into(),
                ":a!u@h PRIVMSG velestest :\u{1}ACTION".into(),
                ":a!u@h PRIVMSG velestest :\u{1}\u{1}".into(),
            ],
        )
        .await;
    }

    /// ── Tier 3: ban lists, typing indicators, chathistory ──
    ///
    /// The stub answers CAP REQ with ACK for whatever the client asks,
    /// so these run with typing/chathistory negotiated — the shape a
    /// real modern server presents.

    #[tokio::test]
    async fn a_ban_list_stream_with_no_requester_is_survivable() {
        // /bans drives the MODE +b query through the op channel, which
        // the drive helper cannot issue — so from the wire's side this
        // is what an UNSOLICITED 367/368 stream looks like (a bouncer
        // replaying one, or another client's query being echoed). It
        // must be collected and dropped cleanly, never panicked on.
        drive(
            |_| {},
            vec![
                ":stub 367 velestest #chan bob!*@host1".into(),
                ":stub 367 velestest #chan spam*!*@*".into(),
                ":stub 368 velestest :End of channel ban list".into(),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn a_367_flood_cannot_grow_the_ban_scratch_without_bound() {
        // WSA-2026-3's shape, at the newest map: 367 forever, 368 never.
        // Over the wire this can only assert "no panic" — the scratch is
        // inside the connection task. The BOUND itself is asserted below
        // against `collect_ban`, which is the code this path calls.
        let mut script = Vec::new();
        for i in 0..2000 {
            script.push(format!(":stub 367 velestest #flood b{i}!*@*"));
        }
        let _ = drive(|_| {}, script).await;
    }

    #[test]
    fn collect_ban_bounds_masks_and_channels_both() {
        // The first version of this bounded ONE axis and its docstring
        // claimed both. The channel axis was open: 367 names a channel we
        // never asked about and need not be in, so `entry(key)` minted a
        // slot per name the server invented.
        let mut s: HashMap<String, Vec<String>> = HashMap::new();

        // Mask axis: fills to the cap, then refuses.
        for i in 0..MAX_ROSTER_MEMBERS {
            assert!(collect_ban(&mut s, "#c".into(), &format!("m{i}")), "{i}");
        }
        assert_eq!(s["#c"].len(), MAX_ROSTER_MEMBERS);
        assert!(
            !collect_ban(&mut s, "#c".into(), "one-too-many"),
            "a full mask list must refuse, not grow"
        );
        assert_eq!(s["#c"].len(), MAX_ROSTER_MEMBERS);

        // A duplicate is not a refusal, and does not double up.
        assert!(collect_ban(&mut s, "#c".into(), "m0"));
        assert_eq!(s["#c"].len(), MAX_ROSTER_MEMBERS);

        // Channel axis: fills to the cap, then refuses a NEW channel.
        let mut t: HashMap<String, Vec<String>> = HashMap::new();
        for i in 0..MAX_TRACKED_CHANNELS {
            assert!(collect_ban(&mut t, format!("#c{i}"), "m"), "{i}");
        }
        assert_eq!(t.len(), MAX_TRACKED_CHANNELS);
        assert!(
            !collect_ban(&mut t, "#newcomer".into(), "m"),
            "a full map must refuse a new channel, not evict one"
        );
        assert_eq!(t.len(), MAX_TRACKED_CHANNELS, "and must not have grown");
        // …while an EXISTING channel still collects: refusing everything
        // once full would break the ban list someone is waiting on.
        assert!(collect_ban(&mut t, "#c0".into(), "m2"));
        assert_eq!(t["#c0"].len(), 2);
    }

    #[tokio::test]
    async fn malformed_banlist_numerics_do_not_panic() {
        drive(
            |_| {},
            vec![
                ":stub 367".into(),
                ":stub 367 velestest".into(),
                ":stub 367 velestest :".into(),
                ":stub 368".into(),
                ":stub 368 velestest".into(),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn an_inbound_typing_tagmsg_produces_a_typing_event_not_a_line() {
        // The observable: nothing is SENT in reply (indicators are not
        // answered), and nothing panics. The tap-side assertion lives in
        // the classify/store tests; here we pin that the handler runs
        // clean against both states and a missing tag.
        let sent = drive(
            |_| {},
            vec![
                "@+typing=active :wrath!u@h TAGMSG #veles".into(),
                "@+typing=done :wrath!u@h TAGMSG #veles".into(),
                "@+typing=active :wrath!u@h TAGMSG velestest".into(),
                ":wrath!u@h TAGMSG #veles".into(), // no tag at all
                "@+typing=active TAGMSG #veles".into(), // no prefix
                "@+typing=active :velestest!u@h TAGMSG #veles".into(), // our own echo
            ],
        )
        .await;
        assert!(
            !sent.iter().any(|l| l.contains("TAGMSG")),
            "a typing indicator must never be replied to, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn chathistory_is_requested_once_per_channel() {
        // Two NAMES cycles for the same channel must produce EXACTLY one
        // CHATHISTORY line: at least one (or the feature does nothing)
        // and no more (or a refresh replays history the store holds).
        //
        // `drive_caps`, not `drive`. Written against the no-cap stub this
        // asserted `asks <= 1` and the real number was 0 — the client had
        // nothing to REQ, `conn.caps` was empty, and the whole feature was
        // untested behind a green tick.
        let sent = drive_wire_timed(
            |c| {
                c.flood_burst = 1;
                c.flood_per_sec = 10.0;
            },
            "chathistory batch server-time",
            vec![
                ":stub 353 velestest = #hist @op +voice plain".into(),
                ":stub 366 velestest #hist :End of /NAMES list".into(),
                ":stub 353 velestest = #hist @op +voice plain2".into(),
                ":stub 366 velestest #hist :End of /NAMES list".into(),
            ],
            Vec::new(),
            Vec::new(),
            Duration::from_millis(0),
            Duration::from_millis(400),
        )
        .await;
        let follow_ups: Vec<_> = sent
            .iter()
            .filter(|(_, line)| line.starts_with("WHO ") || line.starts_with("CHATHISTORY "))
            .collect();
        assert_eq!(
            follow_ups.len(),
            3,
            "two WHO refreshes and one history request: {sent:?}"
        );
        for (previous, next) in follow_ups.iter().zip(follow_ups.iter().skip(1)) {
            assert!(
                next.0.saturating_sub(previous.0) >= Duration::from_millis(60),
                "automatic queries bypassed the line budget: {sent:?}"
            );
        }
        let sent: Vec<_> = sent.into_iter().map(|(_, line)| line).collect();
        let asks: Vec<&String> = sent
            .iter()
            .filter(|l| l.starts_with("CHATHISTORY"))
            .collect();
        assert_eq!(
            asks.len(),
            1,
            "CHATHISTORY must fire exactly once per channel per \
             connection, got {}: {sent:?}",
            asks.len()
        );
        assert!(
            asks[0].contains("#hist"),
            "and for the channel that completed NAMES: {:?}",
            asks[0]
        );
    }

    #[tokio::test]
    async fn chathistory_is_not_requested_when_the_cap_was_not_acked() {
        // The negative control for the test above: same script, no cap.
        // Asking a server that never offered chathistory is a protocol
        // error, and this is the case the old `<= 1` assertion was
        // silently measuring.
        let sent = drive(
            |_| {},
            vec![
                ":stub 353 velestest = #hist @op plain".into(),
                ":stub 366 velestest #hist :End of /NAMES list".into(),
            ],
        )
        .await;
        assert!(
            !sent.iter().any(|l| l.starts_with("CHATHISTORY")),
            "no cap ACK means no CHATHISTORY: {sent:?}"
        );
    }

    #[tokio::test]
    async fn the_client_requests_only_what_the_server_advertised() {
        // The precondition every cap-gated test below rests on. Asserted
        // once, here, so the others can assume it.
        let with_cap = drive_caps(
            |_| {},
            "typing message-tags",
            vec![":stub NOTICE velestest :hello".into()],
        )
        .await;
        assert!(
            with_cap.iter().any(|l| l.contains("CAP REQ")),
            "the client must REQ what the stub advertised: {with_cap:?}"
        );
        assert!(
            with_cap.iter().any(|l| l.contains("typing")),
            "…and the REQ must include typing: {with_cap:?}"
        );

        let without = drive(|_| {}, vec![":stub NOTICE velestest :hello".into()]).await;
        assert!(
            !without.iter().any(|l| l.starts_with("CAP REQ")),
            "nothing advertised means nothing requested: {without:?}"
        );
    }

    #[tokio::test]
    async fn an_outbound_typing_indicator_is_sent_only_with_the_cap() {
        // `IrcCommand::Typing` returns early unless the cap was ACKed.
        // Asserting that needs BOTH halves and an op actually pushed at
        // the client — the first version of this test only inspected the
        // CAP REQ line, so deleting the gate outright left it green.
        let op = || IrcCommand::Typing {
            target: "#veles".into(),
            state: "active".into(),
        };

        let with_cap = drive_full(
            |_| {},
            "typing message-tags",
            vec![":stub NOTICE velestest :hello".into()],
            vec![op()],
        )
        .await;
        let tagmsgs: Vec<&String> = with_cap.iter().filter(|l| l.contains("TAGMSG")).collect();
        assert_eq!(
            tagmsgs.len(),
            1,
            "the cap was ACKed, so the indicator must go out: {with_cap:?}"
        );
        assert!(
            tagmsgs[0].contains("+typing=active") && tagmsgs[0].contains("#veles"),
            "…tagged and addressed: {:?}",
            tagmsgs[0]
        );

        let without = drive_full(
            |_| {},
            "",
            vec![":stub NOTICE velestest :hello".into()],
            vec![op()],
        )
        .await;
        assert!(
            !without.iter().any(|l| l.contains("TAGMSG")),
            "no cap ACK means no TAGMSG — sending one is a protocol \
             error: {without:?}"
        );
    }

    #[tokio::test]
    async fn agent_commands_need_our_mention_in_channels_even_open() {
        // The multi-instance question: two veles bots (or a veles and an
        // ircx bot) in one channel. A line ADDRESSED TO THE OTHER BOT
        // must not fire OUR `.agent` handler — even when the channel's
        // chat gate is off (require_mention: false), because the chat
        // gate is about turns, and .agent is about acting. Otherwise
        // every bot in the channel executes every command aimed at any
        // of them.
        let sent = drive(
            |c| {
                c.channels = crate::parse_channels("#veles", c.default_format);
                c.require_mention = false; // open channel: turns need no mention
                c.allowed_users = std::collections::HashSet::from(["op".to_string()]);
            },
            vec![
                // op is a VERIFIED account (330 → account map), so the
                // authorized `.agent` path is reachable at all.
                ":stub 330 velestest op op :is logged in".into(),
                ":op!u@h PRIVMSG #veles :otherbot: .acl".into(),
                ":op!u@h PRIVMSG #veles :velestest: .acl".into(),
            ],
        )
        .await;
        let notices: Vec<&String> = sent.iter().filter(|l| l.starts_with("NOTICE")).collect();
        assert_eq!(
            notices.len(),
            1,
            "only the line addressing OUR nick may act; both answer = both bots \
             execute everything: {sent:?}"
        );
        // The single answer is SOME refusal/reply for the addressed
        // command — its exact wording depends on the host's [acl] state
        // (this crate's tests must not read the developer's config), so
        // the property pinned here is the ONE-ANSWER rule, not the text.
        assert!(
            !notices[0].is_empty(),
            "the addressed command gets an answer: {notices:?}"
        );
    }

    #[tokio::test]
    async fn a_quit_reaches_the_channels_rolling_context_through_the_real_path() {
        // The fan-out hole, end to end. `context_kind` has always
        // admitted Quit and Nick, and the rendering tests in
        // veles-core publish a `#channel` event by hand — so they
        // passed while the only real producer, `classify`, filed both
        // under the server buffer `*`. In a live channel the block a
        // turn read never said anyone had left or renamed.
        let sent = drive(
            |c| {
                c.name = "fanoutnet".into();
            },
            vec![
                ":stub 353 velestest = #fanout :@wrath erasmus".into(),
                ":stub 366 velestest #fanout :End of /NAMES".into(),
                ":erasmus!u@h NICK :erasmus_afk".into(),
                ":erasmus_afk!u@h QUIT :bbl".into(),
            ],
        )
        .await;
        assert!(
            sent.iter()
                .any(|l| l.starts_with("CAP END") || l.starts_with("USER ")),
            "the connection has to have registered for the script to be read: {sent:?}"
        );
        let ctx = crate::tap::rolling_context("fanoutnet", "#fanout", 50, 64 * 1024)
            .expect("the channel buffer must have context");
        assert!(
            ctx.contains("erasmus is now known as erasmus_afk"),
            "the nick change belongs in the channel it happened in: {ctx}"
        );
        assert!(
            ctx.contains("erasmus_afk quit (bbl)"),
            "and so does the quit: {ctx}"
        );
    }

    #[tokio::test]
    async fn a_refused_speakers_line_is_tapped_blocked_and_not_context() {
        // The adapter half of the `blocked` verdict: the same
        // `is_authorized` the turn gate uses decides at the tap, with
        // the legacy allowlist as the boundary (the stub home keeps the
        // ACL dormant). The banned line must still be PUBLISHED — the
        // desktop renders what the channel saw — and must not survive
        // into the rolling context a turn would read. If the verdict
        // stops being computed in `tap_observe`, this goes red with a
        // poison pill in the context block.
        let mut rx = crate::tap::subscribe();
        let _ = drive(
            |c| {
                c.name = "fanoutnet".into();
                // Only op!u@h may reach the agent; everyone else is a
                // stranger to the allowlist.
                c.allowed_users = ["op!u@h"].into_iter().map(String::from).collect();
            },
            vec![
                ":stranger!u@h PRIVMSG #fanout :poison pill".into(),
                ":op!u@h PRIVMSG #fanout :the real line".into(),
            ],
        )
        .await;

        let mut saw_blocked = false;
        let mut saw_clear = false;
        while let Ok(e) = rx.try_recv() {
            if e.buffer == "#fanout" && e.text.contains("poison pill") {
                assert!(e.blocked, "the verdict must travel with the event");
                saw_blocked = true;
            }
            if e.buffer == "#fanout" && e.text.contains("the real line") {
                assert!(!e.blocked, "an allowlisted speaker is not blocked");
                saw_clear = true;
            }
        }
        assert!(saw_blocked, "the tap records the refused line: {rx:?}");
        assert!(saw_clear, "and the allowlisted one: {rx:?}");

        let ctx = crate::tap::rolling_context("fanoutnet", "#fanout", 50, 64 * 1024)
            .expect("the channel buffer must have context");
        assert!(
            !ctx.contains("poison pill"),
            "a refused speaker's words are not context: {ctx}"
        );
        assert!(
            ctx.contains("the real line"),
            "an allowlisted speaker's words are: {ctx}"
        );
    }

    /// The persistent interpreter is a MODULE's (R5): the tests install
    /// a stub in core's backend slot instead of configuring `[slopdrop]`.
    /// It answers every line with a reply that names the bridge, so a
    /// local-sandbox answer (`42`) and a bridged one are told apart.
    struct StubBridge;
    impl veles_core::tcleval::TclBackend for StubBridge {
        fn configured(&self) -> bool {
            true
        }
        fn eval<'a>(
            &'a self,
            code: &'a str,
            _full: bool,
            _caller: &'a str,
            _sender: Option<&'a veles_core::acl::SenderContext>,
        ) -> veles_core::tcleval::EvalFuture<'a> {
            Box::pin(async move { (format!("slopdrop: stub bridge took `{code}`"), None).into() })
        }
    }

    /// The slot is process-global: the tests that set and clear it run
    /// one at a time (an async lock: it is held across the stub drives).
    static BRIDGE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct BridgeGuard {
        _lock: tokio::sync::MutexGuard<'static, ()>,
    }
    impl Drop for BridgeGuard {
        fn drop(&mut self) {
            veles_core::tcleval::clear_backend();
        }
    }

    /// Install the stub for the test's duration (cleared on drop).
    async fn stub_bridge() -> BridgeGuard {
        let lock = BRIDGE_LOCK.lock().await;
        veles_core::tcleval::set_backend(std::sync::Arc::new(StubBridge));
        BridgeGuard { _lock: lock }
    }

    /// The sync spelling, for a `#[test]` outside any runtime.
    fn stub_bridge_blocking() -> BridgeGuard {
        let lock = BRIDGE_LOCK.blocking_lock();
        veles_core::tcleval::set_backend(std::sync::Arc::new(StubBridge));
        BridgeGuard { _lock: lock }
    }

    /// A legal evaluation may be slower than the test's idle grace. The
    /// stub must not close the socket and call that an empty answer.
    /// This is real Tcl through the native safe interpreter, not a stub
    /// backend or a sleep replacing the production evaluation.
    #[tokio::test]
    async fn a_slow_local_tcl_reply_is_not_lost_to_the_stub_idle_deadline() {
        isolate_home();
        let _h = crate::scratch_home();
        let home = std::path::PathBuf::from(std::env::var_os("VELES_HOME").unwrap());
        std::fs::write(
            home.join("config.toml"),
            "[sandbox]\nbackend = \"native\"\n",
        )
        .unwrap();
        let mut body = "set n 0;".to_string();
        for _ in 0..24 {
            body.push_str("for {set i 0} {$i < 250000} {incr i} {incr n};");
        }
        body.push_str("puts slow-tcl-42");
        let sent = drive_wire_timed_early(
            |c| {
                c.allowed_users = ["op!u@h"].into_iter().map(String::from).collect();
            },
            "",
            WireEvents {
                script: vec![format!(":op!u@h PRIVMSG velestest :.tcl {body}")],
                ..WireEvents::default()
            },
            WireTiming {
                op_delay: Duration::ZERO,
                quiet: Duration::from_millis(400),
            },
        )
        .await;
        let sent: Vec<_> = sent.into_iter().map(|(_, line)| line).collect();
        assert!(
            sent.iter()
                .any(|line| line.starts_with("NOTICE op ") && line.contains("slow-tcl-42")),
            "the real slow evaluation must reach the wire: {sent:?}"
        );
    }

    #[tokio::test]
    async fn the_sigil_spellings_evaluate_locally_and_the_bare_one_is_not_ours_without_a_bridge() {
        // A scratch home, because this asserts on a CONFIG-derived
        // decision and the developer's own `~/.config/veles` must not
        // be the thing that decides it. (The previous version of this
        // test read the real config and was green only because that
        // machine had no `[slopdrop]`.)
        // ORDER MATTERS, and not obviously. `drive_wire` calls
        // `isolate_home()`, which sets VELES_HOME from a `Once` — so
        // whichever test in this binary drives a stub FIRST points the
        // config resolver at the shared stub home. If that happened
        // after the scratch home below, it would silently throw away
        // the config this test is about, and whether it did would
        // depend on test order. Consuming the `Once` here makes the
        // later call a no-op.
        isolate_home();
        let _h = crate::scratch_home();
        let sent = drive(
            |c| {
                c.require_mention = true;
                c.channels = crate::parse_channels("#veles", c.default_format);
                // The stub bot is otherwise undeclared, and an undeclared
                // bot on an open transport refuses every command
                // ("declare a boundary"). An allowlist is a boundary.
                c.allowed_users = ["op!u@h"].into_iter().map(String::from).collect();
            },
            vec![
                ":op!u@h PRIVMSG #veles :velestest .tcl expr {6 * 7}".into(),
                ":op!u@h PRIVMSG #veles :velestest /tcl expr {6 * 7}".into(),
                // No nick, no sigil — slopdrop's spelling exactly.
                ":op!u@h PRIVMSG #veles :tcl expr {6 * 7}".into(),
                ":op!u@h PRIVMSG #veles :tcl is a strange little language".into(),
                ":op!u@h PRIVMSG #veles :tcl".into(),
                // ADDRESSED, so the mention gate passes it and the arm
                // itself has to decline. Without this line the gate
                // does all the work and the arm's own guard is
                // untested — which is how the first draft of this test
                // survived removing it.
                ":op!u@h PRIVMSG #veles :velestest tcl expr {6 * 7}".into(),
            ],
        )
        .await;
        let notices: Vec<&String> = sent.iter().filter(|l| l.starts_with("NOTICE")).collect();
        // The sigil spellings are veles' OWN command and keep the local
        // throwaway sandbox: that is the "do not burn a turn on
        // `expr {1+1}`" feature and it is unaffected by any of this.
        assert_eq!(
            notices.iter().filter(|l| l.contains("42")).count(),
            2,
            "`.tcl` and `/tcl` still answer the caller from the local sandbox: {notices:?}"
        );
        // The bare spelling is slopdrop's, and with no persistent
        // interpreter configured there is nothing of slopdrop's to
        // answer OUT OF. Answering anyway is what put an empty
        // interpreter's "invalid command name" into #coven.
        assert!(
            !sent.iter().any(|l| l.starts_with("PRIVMSG #veles")),
            "an unbridged bare `tcl` is not ours to answer: {sent:?}"
        );
        assert_eq!(
            notices.len(),
            2,
            "and it is not answered privately either: {notices:?}"
        );
    }

    #[tokio::test]
    async fn a_configured_bridge_makes_the_bare_spelling_ours_and_it_answers_the_channel() {
        // The other side of the same switch. The URL is port 9 (discard)
        // on loopback, so the bridge call FAILS — and that is the point:
        // a failure that says "slopdrop" proves the line went to the
        // bridge, where a local-sandbox answer would have said "42".
        // One assertion therefore pins two things at once: the arm fired,
        // and it fired at the right interpreter.
        // ORDER MATTERS, and not obviously. `drive_wire` calls
        // `isolate_home()`, which sets VELES_HOME from a `Once` — so
        // whichever test in this binary drives a stub FIRST points the
        // config resolver at the shared stub home. If that happened
        // after the scratch home below, it would silently throw away
        // the config this test is about, and whether it did would
        // depend on test order. Consuming the `Once` here makes the
        // later call a no-op.
        isolate_home();
        let _h = crate::scratch_home();
        let _b = stub_bridge().await;
        let sent = drive(
            |c| {
                c.require_mention = true;
                c.channels = crate::parse_channels("#veles", c.default_format);
                c.allowed_users = ["op!u@h"].into_iter().map(String::from).collect();
            },
            // Unaddressed, on a require_mention channel: the convention
            // exemption has to hold here, or #coven's decade of muscle
            // memory needs a nick prefix it has never typed.
            vec![":op!u@h PRIVMSG #veles :tcl expr {6 * 7}".into()],
        )
        .await;
        let channel: Vec<&String> = sent
            .iter()
            .filter(|l| l.starts_with("PRIVMSG #veles"))
            .collect();
        assert_eq!(
            channel.len(),
            1,
            "the bare spelling answers, in the channel, unaddressed: {sent:?}"
        );
        assert!(
            channel[0].contains("slopdrop"),
            "…out of the BRIDGE, not the local sandbox — a sandbox answer \
             would read `42`: {channel:?}"
        );
    }

    /// The predicate both call sites share, pinned on its own.
    ///
    /// The stub-server cases above exercise it through the wire, which
    /// proves the wiring but leaves one mutation alive: dropping the
    /// `configured()` half from the MENTION GATE only is invisible
    /// there, because a line that skips the gate and finds no arm goes
    /// to `send_inbound` — and the stub presence is built with
    /// `bus: None`, so that call is a no-op nothing can observe. The
    /// two halves are one function so that mutation cannot be written;
    /// this is the test that the function keeps both of them.
    #[test]
    fn the_bare_spelling_is_ours_only_when_an_interpreter_is_configured() {
        isolate_home();
        {
            let _h = crate::scratch_home();
            let _g = BRIDGE_LOCK.blocking_lock();
            veles_core::tcleval::clear_backend();
            assert!(
                !is_ours_by_convention("tcl expr {1 + 1}"),
                "with no persistent interpreter installed there is nothing of slopdrop's to answer out of"
            );
        }
        {
            let _h = crate::scratch_home();
            let _b = stub_bridge_blocking();
            assert!(
                is_ours_by_convention("tcl expr {1 + 1}"),
                "configured, the spelling is ours"
            );
            assert!(
                is_ours_by_convention("tclAdmin info commands"),
                "both of slopdrop's words — the role check is the arm's job, not this one's"
            );
            assert!(
                !is_ours_by_convention("tcl"),
                "a lone `tcl` is channel speech, configured or not"
            );
            assert!(
                !is_ours_by_convention("hello there"),
                "and an ordinary line is never ours by convention"
            );
        }
    }

    #[tokio::test]
    async fn a_bare_tcl_in_a_query_answers_the_asker_not_ourselves() {
        // The channel case sends to `target`, which in a QUERY is our
        // own nick — so the naive "answer where it was asked" would
        // PRIVMSG ourselves and the asker would see nothing at all.
        // This is the arm that keeps `target` from being the answer.
        // ORDER MATTERS, and not obviously. `drive_wire` calls
        // `isolate_home()`, which sets VELES_HOME from a `Once` — so
        // whichever test in this binary drives a stub FIRST points the
        // config resolver at the shared stub home. If that happened
        // after the scratch home below, it would silently throw away
        // the config this test is about, and whether it did would
        // depend on test order. Consuming the `Once` here makes the
        // later call a no-op.
        isolate_home();
        let _h = crate::scratch_home();
        let _b = stub_bridge().await;
        let sent = drive(
            |c| {
                c.require_mention = true;
                c.allowed_users = ["op!u@h"].into_iter().map(String::from).collect();
            },
            vec![":op!u@h PRIVMSG velestest :tcl expr {6 * 7}".into()],
        )
        .await;
        assert!(
            sent.iter()
                .any(|l| l.starts_with("PRIVMSG op ") && l.contains("slopdrop")),
            "the asker gets the answer: {sent:?}"
        );
        assert!(
            !sent.iter().any(|l| l.starts_with("PRIVMSG velestest")),
            "and we do not talk to ourselves: {sent:?}"
        );
    }

    #[tokio::test]
    async fn a_comma_or_a_bare_nick_addresses_us_on_the_real_path() {
        // `addressed_to`'s unit tests prove the shapes; this proves the
        // shapes reach the gate. The old detector was `nick:` and
        // `@nick` matched with `contains`, so `velestest, …` and bare
        // `velestest …` — the two commonest ways people type — were
        // NOT addressed: on a require_mention channel they got no turn
        // and no `.agent`, which is how the bot looked deaf to half
        // the channel.
        let sent = drive(
            |c| {
                c.channels = crate::parse_channels("#veles", c.default_format);
                c.require_mention = true; // the default: only addressed lines act
            },
            vec![
                ":op!u@h PRIVMSG #veles :velestest, .asks".into(),
                ":op!u@h PRIVMSG #veles :velestest .asks".into(),
                ":op!u@h PRIVMSG #veles :velestestbot .asks".into(),
                ":op!u@h PRIVMSG #veles :i asked velestest about it .asks".into(),
            ],
        )
        .await;
        let notices: Vec<&String> = sent.iter().filter(|l| l.starts_with("NOTICE")).collect();
        assert_eq!(
            notices.len(),
            2,
            "the comma and the bare-nick forms each act once, and neither \
             `velestestbot` nor a mid-sentence mention acts at all: {sent:?}"
        );
    }

    #[tokio::test]
    async fn a_topic_query_is_answered_by_the_332() {
        // THE /topic bug: the Topic arm discarded its reply sender, so a
        // query answered via HTTP timeout while the wire action worked
        // fine. The reply must carry the server's topic text.
        let (tx, mut rx) = oneshot::channel();
        let sent = drive_full(
            |_| {},
            "",
            vec![
                ":stub 332 velestest #chan :hello topic".into(),
                ":stub 333 velestest #chan setter 1700000000".into(),
            ],
            vec![IrcCommand::Topic {
                channel: "#chan".into(),
                topic: None,
                scope: None,
                reply: tx,
            }],
        )
        .await;
        assert!(
            sent.iter().any(|l| l.starts_with("TOPIC #chan")),
            "the query must go out on the wire: {sent:?}"
        );
        let out = tokio::time::timeout(Duration::from_secs(2), &mut rx)
            .await
            .expect("the 332 never answered the query")
            .expect("the topic reply was dropped");
        assert!(
            out.contains("hello topic"),
            "the answer carries the topic text: {out}"
        );
    }

    #[tokio::test]
    async fn a_topic_query_on_an_empty_channel_is_answered_by_the_331() {
        let (tx, mut rx) = oneshot::channel();
        drive_full(
            |_| {},
            "",
            vec![":stub 331 velestest #chan :No topic is set".into()],
            vec![IrcCommand::Topic {
                channel: "#chan".into(),
                topic: None,
                scope: None,
                reply: tx,
            }],
        )
        .await;
        let out = tokio::time::timeout(Duration::from_secs(2), &mut rx)
            .await
            .expect("the 331 never answered the query")
            .expect("the topic reply was dropped");
        assert!(out.contains("no topic is set"), "the 331 answer: {out}");
    }

    #[tokio::test]
    async fn names_answers_from_the_tracked_roster() {
        // /names is a local answer from the member list tracked off
        // 353/366. It answers for whatever the roster holds at dispatch,
        // so the 353 must land first: the delayed-op drive does exactly
        // that (script written ~300ms in, ops fired at 600ms).
        let (tx, mut rx) = oneshot::channel();
        let sent = drive_full_with_delay(
            |_| {},
            "",
            vec![
                ":velestest!u@h JOIN :#chan".into(),
                ":stub 353 velestest = #chan :@op +voice nyx".into(),
                ":stub 366 velestest #chan :End of /NAMES list".into(),
            ],
            vec![IrcCommand::Names {
                channel: "#chan".into(),
                scope: Some("#chan".into()),
                reply: tx,
            }],
            Duration::from_millis(600),
        )
        .await;
        let out = tokio::time::timeout(Duration::from_secs(2), &mut rx)
            .await
            .expect("names never answered")
            .expect("names reply dropped");
        assert!(
            !sent.iter().any(|l| l.starts_with("NAMES")),
            "names must not query the wire: {sent:?}"
        );
        assert!(out.contains("3 member(s)"), "{out}");
        assert!(out.contains("nyx"), "{out}");
    }

    #[tokio::test]
    async fn whois_extra_numerics_reach_the_318_answer() {
        // The weechat-parity whois: realname, idle/signon, channels and
        // away must all surface in the composed 318 answer, not just the
        // 311 handshake fields.
        let (tx, mut rx) = oneshot::channel();
        drive_full(
            |_| {},
            "",
            vec![
                ":stub 311 velestest nyx ~nyx host * :Nyx the Ghost".into(),
                ":stub 312 velestest nyx irc.stub :stub server".into(),
                ":stub 319 velestest nyx :@#chan +#else".into(),
                ":stub 317 velestest nyx 42 1700000000 :seconds idle".into(),
                ":stub 301 velestest nyx :gone fishing".into(),
                ":stub 318 velestest nyx :End of /WHOIS".into(),
            ],
            vec![IrcCommand::Whois {
                nick: "nyx".into(),
                scope: Some("#chan".into()),
                reply: tx,
            }],
        )
        .await;
        let out = tokio::time::timeout(Duration::from_secs(2), &mut rx)
            .await
            .expect("the 318 never answered the whois")
            .expect("the whois reply was dropped");
        assert!(out.contains("Nyx the Ghost"), "realname: {out}");
        assert!(out.contains("idle 42s"), "idle: {out}");
        assert!(out.contains("signon 1700000000"), "signon: {out}");
        assert!(out.contains("@#chan +#else"), "channels: {out}");
        assert!(out.contains("gone fishing"), "away: {out}");
    }

    #[tokio::test]
    async fn a_who_query_answers_at_315_with_rows() {
        let (tx, mut rx) = oneshot::channel();
        let sent = drive_full(
            |_| {},
            "",
            vec![
                ":stub 352 velestest #chan ~u host irc.stub nyx HG :3 Nix".into(),
                ":stub 315 velestest #chan :End of /WHO".into(),
            ],
            vec![IrcCommand::Who {
                target: Some("#chan".into()),
                scope: Some("#chan".into()),
                reply: tx,
            }],
        )
        .await;
        assert!(
            sent.iter().any(|l| l.starts_with("WHO #chan")),
            "the query must go out: {sent:?}"
        );
        let out = tokio::time::timeout(Duration::from_secs(2), &mut rx)
            .await
            .expect("the 315 never answered the who")
            .expect("the who reply was dropped");
        assert!(out.contains("nyx (~u@host)"), "{out}");
        assert!(out.contains("on #chan via irc.stub"), "{out}");
    }

    #[tokio::test]
    async fn a_whois_answer_is_published_into_the_requested_buffer() {
        // The whois answer is not only the op reply: it must reach the
        // named buffer (and scrollback) as an Info event, because the
        // pane renders those as buffer lines.
        let mut tap_rx = crate::tap::subscribe();
        let (tx, mut rx) = oneshot::channel();
        drive_full(
            |_| {},
            "",
            vec![
                ":stub 311 velestest nyx ~nyx host * :Nyxl".into(),
                ":stub 312 velestest nyx irc.stub :stub server".into(),
                ":stub 318 velestest nyx :End of /WHOIS".into(),
            ],
            vec![IrcCommand::Whois {
                nick: "nyx".into(),
                scope: Some("#chan".into()),
                reply: tx,
            }],
        )
        .await;
        let out = tokio::time::timeout(Duration::from_secs(2), &mut rx)
            .await
            .expect("the 318 never answered the whois")
            .expect("the whois reply was dropped");
        assert!(out.contains("nyx"), "the answer names the nick: {out}");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut events = Vec::new();
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), tap_rx.recv()).await {
                Ok(Ok(ev)) => events.push(ev),
                _ => break,
            }
        }
        assert!(
            events.iter().any(|ev| {
                ev.kind == crate::tap::EventKind::Info
                    && ev.buffer == "#chan"
                    && ev.text.contains("nyx")
            }),
            "the whois answer must appear as an Info event in the buffer \
             it was asked from: {events:?}"
        );
    }

    #[tokio::test]
    async fn wire_actions_answer_their_replies_even_without_a_server_echo() {
        // The class the strangled replies belonged to: Join/Part/Kick/
        // Mode/Nick/Away/Raw took the reply sender and never used it,
        // so every pane op hit the 10s request_op timeout though the
        // wire action succeeded. A send confirmation must arrive
        // promptly and without any numeric from the server.
        for cmd in [
            IrcCommand::Join {
                channel: "#chan".into(),
                key: None,
                reply: oneshot::channel().0,
            },
            IrcCommand::Mode {
                target: "#chan".into(),
                modes: "+b x".into(),
                reply: oneshot::channel().0,
            },
            IrcCommand::Away {
                message: Some("gone".into()),
                scope: None,
                reply: oneshot::channel().0,
            },
        ] {
            let (tx, mut rx) = oneshot::channel();
            let mut c = cmd;
            let reply: &mut oneshot::Sender<String> = match &mut c {
                IrcCommand::Join { reply, .. } => reply,
                IrcCommand::Mode { reply, .. } => reply,
                IrcCommand::Away { reply, .. } => reply,
                _ => unreachable!(),
            };
            *reply = tx;
            // The borrow of `c` ends at the last use of `reply` (NLL),
            // so the move into drive_full below is sound — an earlier
            // version said `drop(reply)` here, which drops a REFERENCE
            // and is a no-op, earning the dropping_references warning.
            drive_full(|_| {}, "", vec![], vec![c]).await;
            let out = tokio::time::timeout(Duration::from_secs(3), &mut rx)
                .await
                .expect("an action op must answer without any server echo")
                .expect("the action reply was dropped");
            assert!(!out.starts_with("error:"), "the action succeeded: {out}");
        }
    }

    // The MOTD and the connection statistics are what a person sees
    // when a client joins a server — weechat renders them in the
    // server buffer. The catch-all arm used to eat them, so the first
    // report after this pane became the daily driver was "I don't see
    // server status or motd".
    #[tokio::test]
    async fn motd_and_connection_stats_land_in_the_server_buffer() {
        let mut tap_rx = crate::tap::subscribe();
        drive_full(
            |_| {},
            "",
            vec![
                ":stub 375 velestest :- stub Message of the day -".into(),
                ":stub 372 velestest :- be excellent to each other".into(),
                ":stub 376 velestest :End of /MOTD command".into(),
                ":stub 251 velestest :There are 120 users on 12 servers".into(),
                ":stub 252 velestest 7 :operators online".into(),
                ":stub 254 velestest 431 :channels formed".into(),
                ":stub 255 velestest :I have 9 clients and 1 server".into(),
            ],
            vec![],
        )
        .await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut events = Vec::new();
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(250), tap_rx.recv()).await {
                Ok(Ok(ev)) => events.push(ev),
                _ => break,
            }
        }
        let server: Vec<_> = events
            .iter()
            .filter(|ev| ev.buffer == crate::tap::SERVER_BUFFER)
            .collect();
        assert!(
            server
                .iter()
                .any(|ev| ev.kind == crate::tap::EventKind::Info
                    && ev.text.contains("Message of the day")),
            "the MOTD header reaches the server buffer: {events:?}"
        );
        assert!(
            server
                .iter()
                .any(|ev| ev.text.contains("be excellent to each other")),
            "each MOTD line does too: {events:?}"
        );
        assert!(
            server.iter().any(|ev| ev.text.contains("120 users")),
            "251 (user count) does too: {events:?}"
        );
        assert!(
            server
                .iter()
                .any(|ev| ev.text.contains("7 operators online")),
            "252 keeps its count AND its text: {events:?}"
        );
        assert!(
            server
                .iter()
                .any(|ev| ev.text.contains("431 channels formed")),
            "254 the same: {events:?}"
        );
        assert!(
            !server.iter().any(|ev| ev.text.contains("End of /MOTD")),
            "376 is a terminator, not a line — weechat does not draw it either: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_motd_says_so_instead_of_nothing() {
        let mut tap_rx = crate::tap::subscribe();
        drive_full(
            |_| {},
            "",
            vec![":stub 422 velestest :MOTD File is missing".into()],
            vec![],
        )
        .await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut events = Vec::new();
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(250), tap_rx.recv()).await {
                Ok(Ok(ev)) => events.push(ev),
                _ => break,
            }
        }
        assert!(
            events
                .iter()
                .any(|ev| { ev.buffer == crate::tap::SERVER_BUFFER && ev.text.contains("MOTD") }),
            "no MOTD is still news in the server buffer: {events:?}"
        );
    }

    /// A room the server put us in is not a room anyone asked for.
    ///
    /// FOUND LIVE (2026-09-11): the first connect to IRC4Fun with
    /// `channels = ""` came back with the bot sitting in `#lobby` —
    /// the network force-joins it. `cfg.channels` was read in exactly
    /// ONE place, the join loop at registration, and nothing downstream
    /// ever asked whether an inbound line came from a channel we chose.
    /// So a public room nobody configured was a live inbound surface:
    /// anyone in it could address the bot and spend a billed LLM turn.
    ///
    /// This is the case `require_mention` does NOT cover — it decides
    /// whether a line in a channel is aimed at us, not whether we should
    /// be listening to that channel at all. Same for the ACL: a stranger
    /// in `#lobby` resolves to `guest`, and a guest may chat.
    ///
    /// The observable is a command's NOTICE, because the stub harness
    /// runs without a gateway bus: an addressed `.acl` in a sanctioned
    /// channel answers, and the identical line from the force-joined one
    /// produces nothing at all. (The answer goes to the SENDER, so the
    /// two are told apart by counting, with the configuration as the
    /// only variable between the two runs.)
    #[tokio::test]
    async fn a_force_joined_channel_cannot_start_a_turn() {
        // The network puts us somewhere on its own initiative — exactly
        // what IRC4Fun's #lobby JOIN looks like on the wire, from our
        // own prefix — and the same command is then said in both rooms.
        let script = || {
            vec![
                ":velestest!~velestest@h JOIN #lobby".into(),
                ":op!u@h PRIVMSG #lobby :velestest: .acl".into(),
                ":op!u@h PRIVMSG #veles :velestest: .acl".into(),
            ]
        };
        let count_notices =
            |sent: &[String]| sent.iter().filter(|l| l.starts_with("NOTICE")).count();

        let gated = drive(
            |c| {
                c.require_mention = true;
                c.channels = crate::parse_channels("#veles", c.default_format);
                c.allowed_users = ["op!u@h"].into_iter().map(String::from).collect();
            },
            script(),
        )
        .await;
        assert_eq!(
            count_notices(&gated),
            1,
            "only the configured channel may answer — the force-joined one \
             is an unbudgeted room nobody asked for: {gated:?}"
        );

        // The control, and the whole point: CONFIGURING #lobby is what
        // makes the difference. Same script, same bot, one config line —
        // so the silence above is the gate and not some other property
        // of that line.
        let allowed = drive(
            |c| {
                c.require_mention = true;
                c.channels = crate::parse_channels("#veles,#lobby", c.default_format);
                c.allowed_users = ["op!u@h"].into_iter().map(String::from).collect();
            },
            script(),
        )
        .await;
        assert_eq!(
            count_notices(&allowed),
            2,
            "a channel the operator DID ask for is heard: {allowed:?}"
        );
    }

    /// …but a join somebody ASKED for is sanctioned by the asking.
    ///
    /// The gate must not break the ordinary runtime join: `.irc join`,
    /// the desktop's op and the model's tool all land on
    /// `IrcCommand::Join`, and all three are gated above that point. If
    /// the gate were keyed on the config list alone, a channel joined
    /// this way would be permanently deaf.
    #[tokio::test]
    async fn a_channel_we_deliberately_joined_is_heard() {
        let (tx, _rx) = oneshot::channel();
        let sent = drive_full_with_delay(
            |c| {
                c.require_mention = true;
                c.channels = Vec::new(); // nothing configured at all
                c.allowed_users = ["op!u@h"].into_iter().map(String::from).collect();
            },
            "",
            vec![":op!u@h PRIVMSG #asked :velestest: .acl".into()],
            vec![IrcCommand::Join {
                channel: "#asked".into(),
                key: None,
                reply: tx,
            }],
            // The op has to be processed BEFORE the scripted line, or
            // the join has not sanctioned anything yet.
            Duration::from_millis(0),
        )
        .await;

        assert!(
            sent.iter().any(|l| l.starts_with("JOIN #asked")),
            "the join went out: {sent:?}"
        );
        assert!(
            sent.iter().any(|l| l.starts_with("NOTICE")),
            "a channel we asked to be in must be heard, even with an \
             empty config list: {sent:?}"
        );
    }

    /// USER, 2026-09-11: *"make the bot leave force-joined unsanctioned
    /// channels if it's allowed to"*.
    ///
    /// The sanction gate made such a room mute, which answers "may this
    /// start a turn" and not "should we be sitting in it". Sitting in it
    /// is not free: every line said there is read into the tap ring and
    /// `state.db`, so the bot is a silent recorder in a room nobody
    /// invited it to — and to everyone else in the channel it is a bot
    /// that joined and then ignored them.
    ///
    /// The control is the same script with the channel CONFIGURED, so
    /// what is asserted is the sanction test and not "this build parts
    /// everything".
    #[tokio::test]
    async fn a_room_the_network_put_us_in_is_left() {
        let parts = |sent: &[String]| sent.iter().filter(|l| l.starts_with("PART #lobby")).count();

        let forced = drive(
            |c| c.channels = crate::parse_channels("#veles", c.default_format),
            vec![":velestest!~velestest@h JOIN #lobby".into()],
        )
        .await;
        assert_eq!(
            parts(&forced),
            1,
            "a channel nobody configured is parted: {forced:?}"
        );
        assert!(
            forced
                .iter()
                .any(|l| l.starts_with("PART #lobby") && l.contains("not a configured channel")),
            "the reason says who decided, so an op watching does not read \
             it as a netsplit: {forced:?}"
        );

        let configured = drive(
            |c| c.channels = crate::parse_channels("#veles,#lobby", c.default_format),
            vec![":velestest!~velestest@h JOIN #lobby".into()],
        )
        .await;
        assert_eq!(
            parts(&configured),
            0,
            "the operator asked for this room — one config line is the \
             only difference between the two runs: {configured:?}"
        );
    }

    /// A network that puts us straight back is one we do not fight.
    ///
    /// This is the half that keeps the courtesy from becoming an attack
    /// on the server: PART, re-JOINed, PART, re-JOINed is a flood we
    /// would be blamed for and is how a bot earns a K-line. One attempt
    /// per channel per connection, then we stay — silent, which is what
    /// the gate already guaranteed.
    #[tokio::test]
    async fn a_network_that_puts_us_back_is_not_fought() {
        let sent = drive(
            |c| c.channels = crate::parse_channels("#veles", c.default_format),
            vec![
                ":velestest!~velestest@h JOIN #lobby".into(),
                ":velestest!~velestest@h PART #lobby :not a configured channel".into(),
                // The lobby is compulsory here: the server puts us back.
                ":velestest!~velestest@h JOIN #lobby".into(),
            ],
        )
        .await;
        assert_eq!(
            sent.iter().filter(|l| l.starts_with("PART #lobby")).count(),
            1,
            "exactly one PART — a second one starts a loop with the \
             server: {sent:?}"
        );
    }

    /// Some networks require presence in a lobby, so the courtesy has an
    /// off switch. It is not what makes the room safe — the sanction
    /// gate is — so turning it off costs nothing but a silent guest.
    #[tokio::test]
    async fn the_knob_can_keep_us_in_a_lobby() {
        let sent = drive(
            |c| {
                c.channels = crate::parse_channels("#veles", c.default_format);
                c.part_unsanctioned_channels = false;
            },
            vec![":velestest!~velestest@h JOIN #lobby".into()],
        )
        .await;
        assert!(
            !sent.iter().any(|l| l.starts_with("PART")),
            "part_unsanctioned_channels = false stays put: {sent:?}"
        );
    }

    /// The regression this invites: a channel joined through a gated
    /// command is sanctioned by the asking, and parting it would make
    /// `.irc join` a no-op that reports success.
    #[tokio::test]
    async fn a_channel_we_asked_to_join_is_never_parted() {
        let (tx, _rx) = oneshot::channel();
        let sent = drive_full_with_delay(
            |c| c.channels = Vec::new(),
            "",
            vec![":velestest!~velestest@h JOIN #asked".into()],
            vec![IrcCommand::Join {
                channel: "#asked".into(),
                key: None,
                reply: tx,
            }],
            Duration::from_millis(0),
        )
        .await;
        assert!(
            sent.iter().any(|l| l.starts_with("JOIN #asked")),
            "the join went out: {sent:?}"
        );
        assert!(
            !sent.iter().any(|l| l.starts_with("PART")),
            "the join sanctioned the channel, so nothing may part it \
             back out from under the caller: {sent:?}"
        );
    }

    /// The OTHER half of the credential redaction, and the half that was
    /// missing.
    ///
    /// `redact_service_command` was wired into `tap::classify`, which
    /// only ever sees lines that came off the WIRE — so it covered the
    /// `echo-message` networks and nothing else. Without that cap the
    /// server sends nothing back and `tap_local_echo` is the only thing
    /// that records our own words; it builds its event by hand and
    /// bypassed the redaction completely. The two paths are mutually
    /// exclusive, so half the networks were uncovered, and it is the
    /// older half — the likelier one to still want a plaintext IDENTIFY.
    ///
    /// This stub advertises no caps at all, which is exactly the
    /// condition `needs_local_echo` tests for.
    #[tokio::test]
    async fn our_own_password_is_redacted_on_a_server_without_echo_message() {
        let mut tap_rx = crate::tap::subscribe();
        let (tx, _rx) = oneshot::channel();
        drive_full(
            |_| {},
            "", // no caps advertised → needs_local_echo() is true
            vec![],
            vec![IrcCommand::Say {
                target: "NickServ".into(),
                text: "IDENTIFY jackma hunter2".into(),
                mode: None,
                reply: tx,
            }],
        )
        .await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut events = Vec::new();
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(250), tap_rx.recv()).await {
                Ok(Ok(ev)) => events.push(ev),
                _ => break,
            }
        }

        let ours: Vec<_> = events.iter().filter(|ev| ev.buffer == "NickServ").collect();
        assert!(
            !ours.is_empty(),
            "the local echo has to RECORD the line — redaction narrows it, \
             it does not delete it: {events:?}"
        );
        assert!(
            !ours.iter().any(|ev| ev.text.contains("hunter2")),
            "our own password reached the tap: {ours:?}"
        );
        assert!(
            ours.iter().any(|ev| ev.text == "IDENTIFY <redacted>"),
            "the verb must survive — an operator needs to see that an \
             IDENTIFY happened: {ours:?}"
        );
    }

    /// A QUIT we were asked for (the module's `stop`) ENDS the reconnect
    /// loop. It used to count as a server close: the loop reconnected
    /// after its back-off, so a stopping instance came back online until
    /// teardown dropped it, and only the 10 s stall on every stop hid it.
    #[tokio::test]
    async fn a_requested_quit_ends_the_reconnect_loop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        isolate_home();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accepted = std::sync::Arc::new(AtomicUsize::new(0));
        let stub_count = accepted.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                stub_count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let (rd, mut wr) = sock.into_split();
                    let mut lines = BufReader::new(rd).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if line.starts_with("CAP LS") {
                            let _ = wr.write_all(b":stub CAP * LS :\r\n").await;
                        } else if line.starts_with("CAP END") || line.starts_with("USER ") {
                            let _ = wr.write_all(b":stub 001 velestest :Welcome\r\n").await;
                        } else if line.starts_with("QUIT") {
                            // A server's answer to QUIT: ERROR, then close.
                            let _ = wr.write_all(b"ERROR :Closing link\r\n").await;
                            break;
                        }
                    }
                });
            }
        });

        let cfg = IrcConfig {
            server: "127.0.0.1".into(),
            port,
            use_tls: false,
            nickname: "velestest".into(),
            username: "velestest".into(),
            realname: "velestest".into(),
            name: "quitnet".into(),
            ..IrcConfig::default()
        };
        let presence = crate::tap::PRESENCE_BOT;
        let (_out_tx, out_rx) = mpsc::unbounded_channel();
        let looped =
            tokio::spawn(async move { run_presence_loop(&cfg, None, presence, out_rx).await });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while handle_for("quitnet", presence).is_none() || accepted.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "never connected");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            quit_network("quitnet", presence, "stop").await,
            "the connection did not take the QUIT"
        );
        // The back-off before a reconnect is 2 s: a loop still running
        // after 4 s reconnected instead of ending.
        let ended = tokio::time::timeout(Duration::from_secs(4), looped).await;
        unregister_handle("quitnet", presence);
        assert!(ended.is_ok(), "the loop reconnected after a requested QUIT");
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "a second connection after the QUIT"
        );
    }
}
