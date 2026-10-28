//! The `irc` tool — the model's IRC surface, backed by IrcHandle.
//!
//! Single tool with an `action` field (like the `a2a` tool), not twenty
//! separate tools: keeps the tool schema compact and mirrors how the
//! model already uses a2a. The handle is a global singleton set by the
//! running adapter — when it's absent the tool says so (never panics).

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use veles_core::tools::Tool;

/// The model's IRC surface.
///
/// `session` is the calling session, when there is one. It is how the
/// tool answers two questions it could not answer before: WHO is
/// calling (`acl::session_principal`) and WHICH buffer is theirs
/// (`tools::session_origin`). The globally-registered singleton has
/// `None` and is replaced by a bound copy in every session registry
/// (`Tool::for_session`), so the un-bound one is only ever reachable
/// from owner paths.
#[derive(Default)]
pub struct IrcTool {
    pub session: Option<String>,
}

/// Actions grouped by what they can do, because one capability for the
/// whole tool is either too loose or useless: reading a channel the bot
/// is already in is not the same permission as opping someone.
///
/// Until 2026-09-09 there was no gate here at all — `acl::tool_cap`
/// answers `None` for adapter tools, which the tool loop read as "no
/// gate", so `kick`, `mode` and `say`-to-anywhere were reachable by any
/// principal who could start a turn.
fn action_cap(action: &str) -> Option<veles_core::acl::Cap> {
    use veles_core::acl::Cap;
    Some(match action {
        // Reading what the bot can already see in a buffer. `wait`
        // reads too — it blocks for the NEXT line rather than a past
        // one, and blocking is bounded (`MAX_WAIT_SECS`), so it is not
        // a privilege of its own.
        "history" | "tail" | "wait" | "channel_info" | "list" | "whois" => Cap::Chat,
        // Speaking as the bot. Below admin this is additionally pinned
        // to the caller's own buffer (see `gate`).
        "say" | "notice" => Cap::CmdSession,
        // Changing what the bot IS on the network: membership, modes,
        // topic, identity. Operator work.
        "join" | "part" | "topic" | "kick" | "mode" | "nick" => Cap::CmdAdmin,
        _ => return None,
    })
}

impl IrcTool {
    /// Resolve the handle for an optional network name. When the model
    /// names one, that network's bot connection; otherwise the default —
    /// the first configured bot.
    fn handle(&self, network: Option<&str>) -> Option<crate::client::IrcHandle> {
        match network {
            Some(n) if !n.is_empty() => crate::client::handle_for(n, crate::tap::PRESENCE_BOT),
            _ => crate::client::handle(),
        }
    }

    /// The buffer this session belongs to (`irc:#chan` → `#chan`), when
    /// it came from IRC at all.
    fn own_buffer(&self) -> Option<String> {
        let sid = self.session.as_deref()?;
        let origin = veles_core::tools::session_origin(sid)?;
        origin.strip_prefix("irc:").map(str::to_string)
    }

    /// The network THIS caller is on, read out of their principal's
    /// subject (`irc:<network>/<nick>!<ident>@<host>` — the mask-derived
    /// shape, which on a services-less network is everyone).
    ///
    /// It is what the history surfaces default to when the model names
    /// no network, because "whichever adapter booted first" answers the
    /// wrong network's scrollback for everybody on the second one. A
    /// caller identified by a Services account or grouped under
    /// `[[acl.users]]` has no network in their subject, and falls back
    /// to the default parked one.
    fn session_network(&self) -> Option<String> {
        let sid = self.session.as_deref()?;
        let p = veles_core::acl::session_principal(sid)?;
        let rest = p.subject.strip_prefix("irc:")?;
        let (net, tail) = rest.split_once('/')?;
        if tail.contains('!') && !net.is_empty() {
            Some(net.to_string())
        } else {
            None
        }
    }

    /// The capability check for one action, plus the buffer pin for
    /// sub-admin callers. `None` = allowed; `Some(denial)` is a tool
    /// result the model reads and adapts to, exactly like
    /// `acl::tool_denial`.
    fn gate(&self, action: &str, subject: Option<&str>) -> Option<String> {
        use veles_core::acl::{Cap, Role};
        // No session, or no principal on it: an owner surface. The
        // human at the keyboard is the owner by definition.
        let who = veles_core::acl::session_principal(self.session.as_deref()?)?;
        let Some(cap) = action_cap(action) else {
            // An action nobody classified is owner-only, the same
            // fail-closed posture the tool loop applies to a tool
            // nobody classified.
            if who.has(Cap::All) {
                return None;
            }
            return Some(format!(
                "error: access denied — irc action `{action}` has no declared capability, so it is owner-only and {} is not the owner.",
                who.label()
            ));
        };
        if !who.has(cap) {
            return Some(format!(
                "error: access denied — irc action `{action}` requires the `{}` capability, which {} does not have. Do not retry; tell the user they need to be granted it.",
                cap.as_str(),
                who.label()
            ));
        }
        // Below admin, the tool acts only where the caller is: the bot
        // sits in channels this caller may not even be able to see, and
        // "say" is the bot's voice, not theirs.
        if who.role < Role::Admin {
            let own = self.own_buffer();
            match (subject, own.as_deref()) {
                (Some(want), Some(mine)) if want.eq_ignore_ascii_case(mine) => {}
                (Some(want), Some(mine)) => {
                    return Some(format!(
                        "error: access denied — {} may use irc `{action}` only in {mine}, not {want}. Do not retry elsewhere.",
                        who.label()
                    ));
                }
                (Some(want), None) => {
                    return Some(format!(
                        "error: access denied — this session has no IRC buffer of its own, so {} cannot use irc `{action}` on {want}.",
                        who.label()
                    ));
                }
                // An action with no buffer subject (list) is fine.
                (None, _) => {}
            }
        }
        None
    }
}

#[async_trait]
impl Tool for IrcTool {
    fn name(&self) -> &str {
        "irc"
    }

    fn for_session(&self, session_id: &str) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(IrcTool {
            session: Some(session_id.to_string()),
        }))
    }

    fn description(&self) -> &str {
        "Interact with IRC: send messages, manage channels, read and search a buffer's stored scrollback, and wait for someone to answer. Actions: say, notice, join, part, topic, kick, mode, nick, whois, channel_info, list, history, tail, wait. Formatting is applied automatically (markdown → IRC codes); code is sent byte-exact. When more than one network is connected, set `network` to choose which one, otherwise the first is used."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["say", "notice", "join", "part", "topic", "kick", "mode", "nick", "whois", "channel_info", "list", "history", "tail", "wait"],
                    "description": "What to do. `history` searches stored scrollback, `tail` reads the last lines of a buffer with no search at all, `wait` blocks for the next matching line (how you get another bot's or person's answer to something you just said)."
                },
                "network": {
                    "type": "string",
                    "description": "Which configured network to act on (its `name`); optional when there is only one"
                },
                "target": {
                    "type": "string",
                    "description": "Channel or nick (say/notice/topic/mode)"
                },
                "text": {
                    "type": "string",
                    "description": "Message text (say/notice) or new topic (topic)"
                },
                "channel": {
                    "type": "string",
                    "description": "Channel (join/part/kick), or the buffer to read (history/tail/wait). Omit on history to search every buffer on the network."
                },
                "key": {
                    "type": "string",
                    "description": "Channel key (join, optional)"
                },
                "reason": {
                    "type": "string",
                    "description": "Part/kick reason (optional)"
                },
                "nick": {
                    "type": "string",
                    "description": "Nick (kick/whois/nick), or the only nick to accept lines from (history/tail/wait)"
                },
                "modes": {
                    "type": "string",
                    "description": "Mode string like '+o nick' (mode)"
                },
                "query": {
                    "type": "string",
                    "description": "Case-insensitive substring the line must contain (history; optional on wait)"
                },
                "since": {
                    "type": "string",
                    "description": "Only lines at or after this time (history/tail). RFC3339, or any prefix of one like '2026-09-09' or '2026-09-09T14'."
                },
                "until": {
                    "type": "string",
                    "description": "Only lines before this time (history/tail), same format as `since`"
                },
                "limit": {
                    "type": "integer",
                    "description": "How many lines to return (history/tail/wait). Default 25, maximum 200."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "How long `wait` may block. Default 30, maximum 120. It returns as soon as it has a match."
                }
            },
            "required": ["action"]
        })
    }

    async fn call(&self, args: Value) -> String {
        let network = args
            .get("network")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let Some(h) = self.handle(network.as_deref()) else {
            if let Some(n) = &network {
                return format!("irc: no network named {n:?} is connected");
            }
            return "irc adapter is not connected".into();
        };
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let get = |k: &str| -> Option<String> {
            args.get(k).and_then(|v| v.as_str()).map(str::to_string)
        };

        // ── ACL: per action, from the SESSION's principal ──
        // The subject is the buffer the action names, so a below-admin
        // caller is pinned to their own. Resolved here, before any
        // handle call, and never from the model's own claim about who
        // it serves.
        let subject = match action.as_str() {
            "say" | "notice" | "topic" | "mode" => get("target"),
            "join" | "part" | "kick" | "channel_info" | "history" | "tail" | "wait" => {
                get("channel")
            }
            _ => None,
        };
        if let Some(denial) = self.gate(&action, subject.as_deref()) {
            return denial;
        }

        match action.as_str() {
            "say" => {
                let (Some(target), Some(text)) = (get("target"), get("text")) else {
                    return "say needs target + text".into();
                };
                h.say(&target, &text, None)
            }
            "notice" => {
                let (Some(target), Some(text)) = (get("target"), get("text")) else {
                    return "notice needs target + text".into();
                };
                h.notice(&target, &text).await
            }
            "join" => {
                let Some(channel) = get("channel") else {
                    return "join needs channel".into();
                };
                h.join(&channel, get("key")).await
            }
            "part" => {
                let Some(channel) = get("channel") else {
                    return "part needs channel".into();
                };
                h.part(&channel, get("reason")).await
            }
            "topic" => {
                let Some(target) = get("target") else {
                    return "topic needs target".into();
                };
                h.topic(&target, get("text")).await
            }
            "kick" => {
                let (Some(channel), Some(nick)) = (get("channel"), get("nick")) else {
                    return "kick needs channel + nick".into();
                };
                h.kick(&channel, &nick, get("reason")).await
            }
            "mode" => {
                let (Some(target), Some(modes)) = (get("target"), get("modes")) else {
                    return "mode needs target + modes".into();
                };
                h.mode(&target, &modes).await
            }
            "nick" => {
                let Some(nick) = get("nick") else {
                    return "nick needs nick".into();
                };
                h.nick(&nick).await
            }
            "whois" => {
                let Some(nick) = get("nick") else {
                    return "whois needs nick".into();
                };
                h.whois(&nick).await
            }
            "channel_info" => {
                let Some(channel) = get("channel") else {
                    return "channel_info needs channel".into();
                };
                h.channel_info(&channel).await
            }
            "list" => {
                let chans = h.list_channels().await;
                if chans.is_empty() {
                    "no channels joined".into()
                } else {
                    chans.join(", ")
                }
            }
            "history" | "tail" => {
                let channel = get("channel").unwrap_or_default();
                let query = get("query").filter(|q| !q.trim().is_empty());
                if action == "history" && query.is_none() && get("nick").is_none() {
                    return "history needs query (text to search for) or nick (whose lines to read) — for the last lines of a buffer regardless of content, use tail".into();
                }
                if action == "tail" && channel.is_empty() {
                    return "tail needs channel — the buffer to read".into();
                }
                self.history_reply(
                    network.as_deref(),
                    &veles_core::store::IrcQuery {
                        network: String::new(), // filled in by history_reply
                        buffer: Some(channel).filter(|c| !c.is_empty()),
                        needle: query,
                        nick: get("nick"),
                        since: get("since"),
                        until: get("until"),
                        limit: number(&args, "limit", DEFAULT_HISTORY_LIMIT, MAX_HISTORY_LIMIT),
                    },
                )
                .await
            }
            "wait" => {
                let channel = get("channel").unwrap_or_default();
                if channel.is_empty() {
                    return "wait needs channel — the buffer to watch".into();
                }
                self.wait_for_line(
                    &h,
                    &channel,
                    get("nick").as_deref(),
                    get("query").as_deref(),
                    number(&args, "limit", 1, MAX_HISTORY_LIMIT) as usize,
                    number(&args, "timeout_secs", DEFAULT_WAIT_SECS, MAX_WAIT_SECS),
                )
                .await
            }
            other => format!("unknown irc action: {other}"),
        }
    }
}

/// Default and ceiling for how many scrollback lines an answer carries.
/// It was a hard-coded 25 in two places and absent from the schema, so
/// the model could neither ask for more nor know it could not.
const DEFAULT_HISTORY_LIMIT: u32 = 25;
const MAX_HISTORY_LIMIT: u32 = 200;
/// `wait` blocks a turn. Bounded so a stuck turn is a slow turn and
/// never a hung one.
const DEFAULT_WAIT_SECS: u32 = 30;
const MAX_WAIT_SECS: u32 = 120;

/// One integer argument, clamped. Accepts a JSON number or a numeric
/// string, because models send both.
fn number(args: &Value, key: &str, default: u32, max: u32) -> u32 {
    let raw = args.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
    });
    match raw {
        Some(0) | None => default,
        Some(n) => (n.min(max as u64)) as u32,
    }
}

/// One scrollback row, the way both the tool and `.agent history`
/// render it.
pub fn render_line(l: &veles_core::store::IrcLine, with_buffer: bool) -> String {
    let hhmm = l.ts.get(11..16).unwrap_or(&l.ts);
    if with_buffer {
        format!("[{hhmm}] {} <{}> {}", l.buffer, l.nick, l.text)
    } else {
        format!("[{hhmm}] <{}> {}", l.nick, l.text)
    }
}

impl IrcTool {
    /// Read durable scrollback along whichever axes the caller named.
    ///
    /// `q.network` is ignored on the way in — the network comes from
    /// the tool's own `network` argument, or the default parked one,
    /// because a store parked under one name must never be searched
    /// under another (which is what the old `OnceLock` pair did on a
    /// multi-network gateway).
    async fn history_reply(
        &self,
        network: Option<&str>,
        q: &veles_core::store::IrcQuery,
    ) -> String {
        let net = match network
            .map(str::to_string)
            .or_else(|| self.session_network())
            .or_else(crate::client::parked_network)
        {
            Some(n) => n,
            None => return "irc history unavailable (no network parked)".into(),
        };
        let Some(store) = crate::client::parked_store_for(Some(&net)) else {
            let known = crate::client::parked_networks();
            if known.is_empty() {
                return "irc history unavailable (the gateway store is not parked)".into();
            }
            return format!(
                "irc history unavailable — no store for network {net:?}; parked: {}",
                known.join(", ")
            );
        };
        let q = veles_core::store::IrcQuery {
            network: net,
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
            // Say which filter came up empty, so the model narrows or
            // widens the RIGHT axis instead of guessing.
            let mut what = Vec::new();
            if let Some(n) = q.needle.as_deref().filter(|n| !n.is_empty()) {
                what.push(format!("mentioning {n:?}"));
            }
            if let Some(n) = q.nick.as_deref().filter(|n| !n.is_empty()) {
                what.push(format!("from {n}"));
            }
            if let Some(s) = q.since.as_deref().filter(|s| !s.is_empty()) {
                what.push(format!("since {s}"));
            }
            if let Some(u) = q.until.as_deref().filter(|u| !u.is_empty()) {
                what.push(format!("before {u}"));
            }
            if what.is_empty() {
                return format!("no lines in {scope}");
            }
            return format!("no lines {} in {scope}", what.join(" "));
        }
        let show_buffer = q.buffer.is_none();
        let mut out = format!("{scope} — {} line(s):", rows.len());
        for l in &rows {
            out.push('\n');
            out.push_str(&render_line(l, show_buffer));
        }
        out
    }

    /// Block until the next matching line lands in `buffer`, or the
    /// timeout runs out.
    ///
    /// This is what makes working WITH somebody possible rather than
    /// just talking at them: a turn can `say` something to another bot
    /// and read the reply in the same turn. Spontaneous-reply mode
    /// stays a non-goal — the model asks for the lines it wants, it is
    /// not handed every line in the channel.
    ///
    /// Honours turn cancellation, so `/cancel` ends the wait instead of
    /// leaving the turn parked for two minutes.
    async fn wait_for_line(
        &self,
        h: &crate::client::IrcHandle,
        buffer: &str,
        nick: Option<&str>,
        needle: Option<&str>,
        want: usize,
        timeout_secs: u32,
    ) -> String {
        // Subscribe BEFORE anything else, so a line that arrives while
        // this is being set up is not missed.
        let mut rx = crate::tap::subscribe();
        let network = h.network().to_string();
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs as u64);
        let session = self.session.clone();
        let mut hits: Vec<String> = Vec::new();
        loop {
            if let Some(sid) = session.as_deref() {
                if veles_core::agent::turn_cancelled(sid) {
                    return "wait cancelled".into();
                }
            }
            // Wake regularly even with no traffic, so cancellation is
            // noticed and the deadline is honoured.
            let tick = tokio::time::Duration::from_millis(200);
            let next = tokio::time::timeout(tick, rx.recv()).await;
            match next {
                Ok(Ok(ev)) => {
                    let ours = ev.network.eq_ignore_ascii_case(&network)
                        && ev.buffer.eq_ignore_ascii_case(buffer)
                        && !ev.own
                        && matches!(
                            ev.kind,
                            crate::tap::EventKind::Privmsg
                                | crate::tap::EventKind::Action
                                | crate::tap::EventKind::Notice
                        );
                    let from_ok = nick
                        .map(|n| ev.nick.eq_ignore_ascii_case(n))
                        .unwrap_or(true);
                    let text_ok = needle
                        .filter(|n| !n.is_empty())
                        .map(|n| ev.text.to_lowercase().contains(&n.to_lowercase()))
                        .unwrap_or(true);
                    if ours && from_ok && text_ok {
                        let hhmm = ev.ts.get(11..16).unwrap_or("");
                        hits.push(format!("[{hhmm}] <{}> {}", ev.nick, ev.text));
                        if hits.len() >= want.max(1) {
                            break;
                        }
                    }
                }
                // Lagged: the broadcast ring overran while we waited.
                // Say so rather than pretending nothing was missed.
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    hits.push(format!("(missed {n} line(s) — the event stream lagged)"));
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    return "wait ended: the event stream closed".into();
                }
                Err(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
        if hits.is_empty() {
            let mut what = format!("nothing new in {buffer}");
            if let Some(n) = nick {
                what.push_str(&format!(" from {n}"));
            }
            if let Some(n) = needle.filter(|n| !n.is_empty()) {
                what.push_str(&format!(" mentioning {n:?}"));
            }
            return format!("{what} within {timeout_secs}s");
        }
        format!("{buffer}:\n{}", hits.join("\n"))
    }
}

/// Register the irc tool into a registry (gateway sessions only).
pub fn register(reg: &mut veles_core::tools::ToolRegistry) {
    reg.register(Arc::new(IrcTool::default()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `history` action reads the parked gateway store — the seam
    /// `veles-cli` wires at boot. The real turn test is the store's;
    /// this pins the tool side of the seam.
    #[tokio::test]
    async fn history_search_reads_the_parked_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = veles_core::store::Store::open(&dir.path().join("state.db")).expect("store");
        for (nick, text) in [
            ("wrath", "the build broke again"),
            ("erasmus", "the BUILD passed on ci"),
        ] {
            store
                .irc_append(
                    "n1",
                    "bot",
                    "#chan",
                    "2026-08-25T10:00:00Z",
                    "privmsg",
                    nick,
                    None,
                    text,
                    false,
                    0,
                    false,
                )
                .expect("append");
        }
        crate::client::park_store(std::sync::Arc::new(std::sync::Mutex::new(store)), "n1");

        let tool = IrcTool::default();
        let q = |buffer: Option<&str>, needle: Option<&str>, nick: Option<&str>| {
            veles_core::store::IrcQuery {
                network: String::new(),
                buffer: buffer.map(str::to_string),
                needle: needle.map(str::to_string),
                nick: nick.map(str::to_string),
                limit: 25,
                ..veles_core::store::IrcQuery::default()
            }
        };
        let got = tool
            .history_reply(Some("n1"), &q(Some("#chan"), Some("build"), None))
            .await;
        assert!(
            got.contains("the build broke again") && got.contains("the BUILD passed on ci"),
            "the named network is searched case-insensitively: {got}"
        );
        assert!(
            got.contains("[10:00]"),
            "each answer line carries its HH:MM: {got}"
        );

        // A nick filter is one of the axes the interactive case needs.
        let mine = tool
            .history_reply(Some("n1"), &q(Some("#chan"), None, Some("ERASMUS")))
            .await;
        assert!(
            mine.contains("the BUILD passed on ci") && !mine.contains("broke again"),
            "a nick filter reads one person's lines, case-insensitively: {mine}"
        );

        // No buffer searches every buffer on the network, and says
        // which buffer each line came from.
        let all = tool
            .history_reply(Some("n1"), &q(None, Some("build"), None))
            .await;
        assert!(
            all.contains("all buffers") && all.contains("#chan <wrath>"),
            "an all-buffers answer names the buffer per line: {all}"
        );

        // A network nobody parked says so, and says what IS parked —
        // it does not silently answer out of another network's data,
        // which is what the OnceLock pair used to do.
        let nope = tool
            .history_reply(Some("nowhere"), &q(Some("#chan"), Some("build"), None))
            .await;
        assert!(
            nope.contains("no store for network") && nope.contains("n1"),
            "{nope}"
        );

        // An empty result names the filter that came up empty, so the
        // model widens the right axis.
        let empty = tool
            .history_reply(Some("n1"), &q(Some("#chan"), Some("nothing"), None))
            .await;
        assert!(
            empty.contains("mentioning \"nothing\"") && empty.contains("#chan"),
            "{empty}"
        );
    }

    #[test]
    fn a_limit_argument_is_read_clamped_and_defaulted() {
        // It was a hard-coded 25 in two places and absent from the
        // schema: the model could neither ask for more nor learn that
        // it could not.
        let args = json!({"limit": 5});
        assert_eq!(number(&args, "limit", 25, 200), 5);
        assert_eq!(
            number(&json!({"limit": "7"}), "limit", 25, 200),
            7,
            "models send numbers as strings"
        );
        assert_eq!(
            number(&json!({}), "limit", 25, 200),
            25,
            "absent is the default"
        );
        assert_eq!(
            number(&json!({"limit": 0}), "limit", 25, 200),
            25,
            "zero is not `no lines`"
        );
        assert_eq!(
            number(&json!({"limit": 100000}), "limit", 25, 200),
            200,
            "clamped, so one call cannot pull the whole table"
        );
        assert_eq!(
            number(&json!({"limit": "nonsense"}), "limit", 25, 200),
            25,
            "garbage is the default, not an error the model has to parse"
        );
    }

    #[tokio::test]
    async fn wait_returns_the_next_matching_line_and_times_out_cleanly() {
        // The piece that makes working WITH another bot possible: say
        // something, then read the answer in the same turn. Without it
        // the only way to see a reply was spontaneous-reply mode, which
        // is a non-goal ("do not feed the model every chat message").
        let (tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(cmd_rx);
        let h = crate::client::IrcHandle::for_test(tx, "waitnet");
        let tool = IrcTool::default();

        // A line from somebody else, published while the wait is open.
        let feeder = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            crate::tap::publish(crate::tap::event(
                "waitnet",
                crate::tap::PRESENCE_BOT,
                "#wait",
                crate::tap::EventKind::Privmsg,
                "erasmus",
                "pong, and the build is green",
            ));
        });
        let got = tool
            .wait_for_line(&h, "#wait", Some("erasmus"), Some("build"), 1, 5)
            .await;
        feeder.await.expect("feeder");
        assert!(
            got.contains("<erasmus> pong, and the build is green"),
            "the awaited line is the answer: {got}"
        );

        // A wait nobody answers says so, and says what it was waiting
        // for, rather than returning an empty string the model has to
        // guess about.
        let quiet = tool
            .wait_for_line(&h, "#wait", Some("nobody"), None, 1, 1)
            .await;
        assert!(
            quiet.contains("nothing new in #wait")
                && quiet.contains("from nobody")
                && quiet.contains("within 1s"),
            "{quiet}"
        );
    }

    #[tokio::test]
    async fn wait_ignores_other_buffers_networks_and_our_own_echo() {
        let (tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(cmd_rx);
        let h = crate::client::IrcHandle::for_test(tx, "waitnet2");
        let tool = IrcTool::default();
        let feeder = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            // Right network, wrong buffer.
            crate::tap::publish(crate::tap::event(
                "waitnet2",
                crate::tap::PRESENCE_BOT,
                "#elsewhere",
                crate::tap::EventKind::Privmsg,
                "erasmus",
                "over here",
            ));
            // Right buffer, wrong network.
            crate::tap::publish(crate::tap::event(
                "othernet",
                crate::tap::PRESENCE_BOT,
                "#wait2",
                crate::tap::EventKind::Privmsg,
                "erasmus",
                "different network",
            ));
            // Our own line coming back through echo-message: waiting
            // for an answer must not be satisfied by the question.
            let mut own = crate::tap::event(
                "waitnet2",
                crate::tap::PRESENCE_BOT,
                "#wait2",
                crate::tap::EventKind::Privmsg,
                "velesbot",
                "anyone there?",
            );
            own.own = true;
            crate::tap::publish(own);
        });
        let got = tool.wait_for_line(&h, "#wait2", None, None, 1, 1).await;
        feeder.await.expect("feeder");
        assert!(
            got.contains("nothing new in #wait2"),
            "none of those three is an answer in this buffer: {got}"
        );
    }

    fn bind(session: &str, role: veles_core::acl::Role, buffer: Option<&str>) -> IrcTool {
        veles_core::acl::set_session_principal(
            session,
            veles_core::acl::Principal::known(&format!("irc:{session}"), role),
        );
        if let Some(b) = buffer {
            veles_core::tools::set_session_origin(session, &format!("irc:{b}"));
        }
        IrcTool {
            session: Some(session.to_string()),
        }
    }

    /// Every action is gated by the CALLER's capabilities, per action.
    /// One capability for the whole tool would be either too loose
    /// (a guest could op someone) or useless (nobody could read a
    /// buffer), and until 2026-09-09 there was no gate at all: the
    /// engine's `tool_cap` answers None for an adapter's tool and the
    /// tool loop read that as "no gate".
    #[test]
    fn actions_are_gated_per_capability_and_pinned_to_the_callers_buffer() {
        use veles_core::acl::Role;

        // A verified user may read the buffer they are in…
        let user = bind("gate-user", Role::User, Some("#chan"));
        assert!(
            user.gate("history", Some("#chan")).is_none(),
            "a user may search the buffer they are talking in"
        );
        // …and so may a GUEST, whose whole floor is reading (USER:
        // "Guests should have read-only access"): the buffer the bot
        // is already sitting in is not a privilege.
        let guest = bind("gate-guest", Role::Guest, Some("#chan"));
        assert!(
            guest.gate("history", Some("#chan")).is_none(),
            "a guest reads the buffer they are talking in"
        );
        assert!(
            guest.gate("say", Some("#chan")).is_some(),
            "…but does not get to speak as the bot"
        );
        // …and neither may speak as the bot, nor op anyone.
        let denial = user.gate("say", Some("#chan")).expect("say is gated");
        assert!(denial.contains("cmd-session"), "{denial}");
        let denial = user.gate("kick", Some("#chan")).expect("kick is gated");
        assert!(denial.contains("cmd-admin"), "{denial}");

        // A trusted user may speak — but only where they are.
        let trusted = bind("gate-trusted", Role::Trusted, Some("#chan"));
        assert!(trusted.gate("say", Some("#chan")).is_none());
        let elsewhere = trusted
            .gate("say", Some("#other"))
            .expect("another channel is denied");
        assert!(
            elsewhere.contains("#chan") && elsewhere.contains("#other"),
            "the denial must name both buffers: {elsewhere}"
        );
        // Reading another buffer is the same leak: the bot sits in
        // channels this caller may not be able to see.
        assert!(trusted.gate("history", Some("#other")).is_some());
        // Trusted still cannot change what the bot IS on the network.
        assert!(trusted.gate("mode", Some("#chan")).is_some());

        // An admin acts anywhere.
        let admin = bind("gate-admin", Role::Admin, Some("#chan"));
        assert!(admin.gate("say", Some("#other")).is_none());
        assert!(admin.gate("kick", Some("#other")).is_none());
        assert!(admin.gate("history", Some("#anywhere")).is_none());

        // An unknown action is owner-only, not open.
        assert!(admin.gate("frobnicate", None).is_some());
        let owner = bind("gate-owner", Role::Owner, Some("#chan"));
        assert!(owner.gate("frobnicate", None).is_none());

        // An owner SURFACE (no session, no principal) is ungated — the
        // human at the keyboard is the owner by definition.
        let bare = IrcTool::default();
        assert!(bare.gate("kick", Some("#anywhere")).is_none());
    }

    /// A below-admin session with no IRC buffer of its own (a cron job,
    /// an A2A peer) cannot use the bot's voice at all.
    #[test]
    fn a_session_with_no_buffer_cannot_act_on_one() {
        let t = bind("gate-nobuffer", veles_core::acl::Role::Trusted, None);
        let denial = t.gate("say", Some("#chan")).expect("denied");
        assert!(denial.contains("no IRC buffer of its own"), "{denial}");
    }

    /// The globally-registered singleton has no session; a session
    /// registry gets a BOUND copy, which is what makes the gate above
    /// reachable at all.
    #[test]
    fn for_session_binds_a_copy() {
        let tool = IrcTool::default();
        assert!(tool.session.is_none());
        let bound = tool.for_session("sess-bound").expect("bound copy");
        assert_eq!(bound.name(), "irc");
    }
}
