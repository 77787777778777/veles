//! The IRC wire types, shared between core and the IRC process plugin
//! (PLAN-plugins-installer.md D2, Phase 4).
//!
//! Core keeps the tap's data model — the broadcast bus, the rings, the
//! per-network state, the durable store writes — in `veles_core::irc_tap`.
//! What moves here is only the *serialisable shape* of an event, a send
//! and an op, because the plugin (a separate process) has to produce and
//! consume those exact bytes over the JSON-RPC protocol
//! ([`crate::plugin`]): it converts a wire line into an [`IrcEvent`] and
//! reports it on `irc_tap.publish`, and it answers `irc.send`/`irc.op`
//! carrying a [`SendRequest`]/[`IrcOp`]. Core re-exports all of it, so
//! every existing path still compiles.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// What kind of thing happened. Serialized to the durable scrollback by
/// name (`from_wire` is the inverse), so a new kind needs both arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Privmsg,
    /// CTCP ACTION — `/me`. Its own kind because a client renders it as
    /// "* nick does a thing", not as speech.
    Action,
    Notice,
    Join,
    Part,
    Quit,
    Nick,
    Topic,
    Mode,
    Kick,
    /// IRCv3 `+typing` TAGMSG (typing cap). Ephemeral by design: it is
    /// published to live subscribers but the store writer skips it, so
    /// a backlog never replays stale "is typing" state.
    Typing,
    Connected,
    Disconnected,
    /// Announced when a dial starts and again before each reconnect
    /// backoff, so a client can say "connecting" / "reconnecting in Ns"
    /// instead of showing nothing between a Disconnected line and the
    /// next Connected one. Text carries the destination or the delay.
    Connecting,
    /// Synthesized informational output that answers an introspection
    /// request — a whois answer, a ban list, a topic query answer, an
    /// away confirmation. No wire line carries these (they are numerics
    /// on the wire), so the client publishes them itself; without a
    /// kind of their own they would exist only as an op reply string
    /// and never reach a buffer or the scrollback store.
    Info,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Privmsg => "privmsg",
            EventKind::Action => "action",
            EventKind::Notice => "notice",
            EventKind::Join => "join",
            EventKind::Part => "part",
            EventKind::Quit => "quit",
            EventKind::Nick => "nick",
            EventKind::Topic => "topic",
            EventKind::Mode => "mode",
            EventKind::Kick => "kick",
            EventKind::Typing => "typing",
            EventKind::Connected => "connected",
            EventKind::Disconnected => "disconnected",
            EventKind::Connecting => "connecting",
            EventKind::Info => "info",
        }
    }

    /// The inverse of [`as_str`](Self::as_str) — what the durable
    /// scrollback stores. `None` for a name no version of this enum
    /// ever wrote, so a row from the future is skipped rather than
    /// mis-rendered.
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "privmsg" => EventKind::Privmsg,
            "action" => EventKind::Action,
            "notice" => EventKind::Notice,
            "join" => EventKind::Join,
            "part" => EventKind::Part,
            "quit" => EventKind::Quit,
            "nick" => EventKind::Nick,
            "topic" => EventKind::Topic,
            "mode" => EventKind::Mode,
            "kick" => EventKind::Kick,
            "typing" => EventKind::Typing,
            "connected" => EventKind::Connected,
            "disconnected" => EventKind::Disconnected,
            "connecting" => EventKind::Connecting,
            "info" => EventKind::Info,
            _ => return None,
        })
    }
}

/// One thing that happened on a connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrcEvent {
    /// Monotonic within a process run. The desktop resumes from it, and
    /// the store keeps its own durable id — this one only has to order
    /// live events for subscribers. Stamped by core on re-publish.
    pub seq: u64,
    pub network: String,
    pub presence: String,
    /// Channel, the other nick for a query, or `*` for the server.
    pub buffer: String,
    /// RFC3339. From the server's `server-time` tag when it sends one,
    /// so a replayed backlog carries the time the line was SAID rather
    /// than the time we happened to read it.
    pub ts: String,
    pub kind: EventKind,
    pub nick: String,
    /// The verified services account, when the server tells us.
    pub account: Option<String>,
    pub text: String,
    /// We said this. Set by echo-message or by the local-echo fallback.
    pub own: bool,
    /// The adapter's verdict: this SPEAKER may not reach the agent —
    /// banned by the ACL, or outside the legacy allowlist when that is
    /// the trust boundary in force. The event is still published and
    /// still stored, but the rolling context never feeds it to a model.
    #[serde(default)]
    pub blocked: bool,
}

/// A connection's current shape, for a client that attaches late and
/// needs to draw something before any backlog arrives.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkState {
    pub network: String,
    pub presence: String,
    pub nick: String,
    pub connected: bool,
    pub channels: Vec<String>,
    /// channel -> topic
    pub topics: HashMap<String, String>,
    /// channel -> member nicks, prefix chars included (@, +, …)
    pub rosters: HashMap<String, Vec<String>>,
    /// channel -> the subset of its roster who are away (from WHO/H
    /// polling), same entry format as `rosters`.
    #[serde(default)]
    pub aways: HashMap<String, Vec<String>>,
}

/// How a send reaches the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendKind {
    /// Ordinary speech.
    Privmsg,
    /// CTCP ACTION — `/me`.
    Action,
    Notice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendRequest {
    pub network: String,
    pub presence: String,
    /// Channel or nick.
    pub target: String,
    /// VERBATIM. Whatever the composer holds, including the control
    /// bytes C-v put there. The desktop deliberately does not convert
    /// markdown to IRC formatting — that is the bot's job on its own
    /// output, and doing it to a human's typing would corrupt a message
    /// that happens to contain an asterisk.
    pub text: String,
    pub kind: SendKind,
}

/// An operator command on a connection. Serialized to the plugin over
/// `irc.op`; the `action` tag names the variant on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum IrcOp {
    Join {
        channel: String,
        #[serde(default)]
        key: Option<String>,
    },
    Part {
        channel: String,
        #[serde(default)]
        reason: Option<String>,
    },
    Topic {
        channel: String,
        #[serde(default)]
        topic: Option<String>,
        /// Where the answer belongs when this is a QUERY (topic: None).
        #[serde(default)]
        scope: Option<String>,
    },
    Kick {
        channel: String,
        nick: String,
        #[serde(default)]
        reason: Option<String>,
    },
    Mode {
        target: String,
        modes: String,
    },
    Nick {
        nick: String,
    },
    Whois {
        nick: String,
        /// Buffer the synthesized answer is published to (Info event).
        #[serde(default)]
        scope: Option<String>,
    },
    /// Set or clear away status (`/away [message]`).
    Away {
        #[serde(default)]
        message: Option<String>,
        /// Buffer the confirmation is published to. Default: server.
        #[serde(default)]
        scope: Option<String>,
    },
    /// Send a raw IRC line (`/raw LINE`). Dangerous by design.
    Raw {
        line: String,
    },
    ChannelInfo {
        channel: String,
        #[serde(default)]
        scope: Option<String>,
    },
    /// View a channel's ban list (MODE +b query → 367/368). Read-only.
    Bans {
        channel: String,
        #[serde(default)]
        scope: Option<String>,
    },
    /// The channel roster (`/names`). Answered from the client's live
    /// member list, no wire query at all.
    Names {
        channel: String,
        #[serde(default)]
        scope: Option<String>,
    },
    /// WHO query (`/who [mask]`) — answers at 315/352.
    Who {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        scope: Option<String>,
    },
    /// Outbound typing indicator (typing cap). Best-effort on purpose.
    Typing {
        target: String,
        state: String,
    },
    List,
}

impl IrcOp {
    /// Ops that change what other people can do, or remove them — plus
    /// raw, which can do literally anything the connection can.
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            IrcOp::Kick { .. } | IrcOp::Mode { .. } | IrcOp::Raw { .. }
        )
    }
}

/// One observed IRC line, as the durable scrollback reads it back.
/// Lives here (not in core's store) so the plugin can render the answer
/// to an `irc.history` call without linking core.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrcLine {
    /// Durable, monotonic, and what a client resumes from.
    pub id: i64,
    pub network: String,
    pub presence: String,
    pub buffer: String,
    pub ts: String,
    pub kind: String,
    pub nick: String,
    pub account: String,
    pub text: String,
    pub own: bool,
    /// The speaker failed the authorization check when the line was
    /// recorded. Rendered by clients, never fed to a turn.
    pub blocked: bool,
}

/// One scrollback question, along every axis core's store answers.
/// `Default` is "the newest `limit` lines on this network, unfiltered".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IrcQuery {
    pub network: String,
    /// One buffer, or every buffer on the network when None/empty.
    #[serde(default)]
    pub buffer: Option<String>,
    /// Case-insensitive substring of the line text. None/empty is a
    /// `tail`: the newest lines, whatever they say.
    #[serde(default)]
    pub needle: Option<String>,
    /// Case-insensitive exact nick.
    #[serde(default)]
    pub nick: Option<String>,
    /// RFC3339, or any prefix of one. Inclusive.
    #[serde(default)]
    pub since: Option<String>,
    /// RFC3339, or any prefix of one. Exclusive.
    #[serde(default)]
    pub until: Option<String>,
    /// Newest N matches. Clamped to at least 1.
    #[serde(default)]
    pub limit: u32,
}

// ── Phase 4 protocol params/results (crate::plugin::method) ──────────

/// `irc.send` (gateway → plugin): speak as the bot on the instance's
/// network. Just the [`SendRequest`]; the instance names the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrcSendParams {
    #[serde(flatten)]
    pub req: SendRequest,
}

/// `irc.op` (gateway → plugin): run one operator command on the
/// instance's network; the string result is the reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrcOpParams {
    #[serde(default)]
    pub presence: String,
    pub op: IrcOp,
}

/// `irc.typing` (gateway → plugin notification): the agent is composing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IrcTypingParams {
    pub target: String,
    pub state: String,
}

/// `irc_tap.publish` (plugin → gateway notification): one line off the
/// wire, shaped for the tap. Core re-publishes it onto the tap bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrcEventParams {
    pub event: IrcEvent,
}

/// `irc.history` (plugin → gateway): read the durable scrollback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrcHistoryParams {
    #[serde(flatten)]
    pub query: IrcQuery,
}

/// What `irc.history` returns.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IrcHistoryValue {
    #[serde(default)]
    pub lines: Vec<IrcLine>,
}

/// `session.principal` (plugin → gateway): resolve the principal for a
/// session id, so the `irc` tool can gate itself on core's verdict.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionPrincipalParams {
    pub session_id: String,
}
