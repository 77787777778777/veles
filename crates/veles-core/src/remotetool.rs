//! Re-homed tools (PLAN-pool-round.md D1) and what a peer may run here
//! without a ticket (D2).
//!
//! **Re-homing.** `[tools.home]` maps tool globs to nodes —
//! `"web_*" = "laptop"` sends every web call to the laptop and its
//! residential IP (USER 2026-09-29: "web fetch might work better from a
//! residential ip on the laptop") — and `/tools home <glob> <node>` does
//! the same for one session. The call passes every gate on the CALLING
//! node first (the caller's ACL; the agent's approval screen), then
//! travels as a cluster message framed like the other protocols (marker +
//! base64 + sha256), and the node that runs it answers with the tool's
//! output. The same list as remote subagents gates it, `[cluster]
//! task_peers`: a peer that may place work here may place tool calls.
//! A session that came from another node keeps that node's tool list (C1),
//! so a tool this node lacks goes home to the node that built the list
//! instead of failing as unknown.
//!
//! **What a peer may run here unasked** (USER 2026-09-29: "Everything at
//! User ACL level and below really. Like especially the sandboxed writes
//! etc should be whitelisted … The main point of this is not cross-node
//! confidentiality but rather limiting the blast radius of a node
//! compromise"):
//!
//! - what the ACL's User role may do, at the LOWER of that and the
//!   caller's own role — web, media, and a shell and file writes routed
//!   exactly as a local User's are: into a sandbox, never the host. It
//!   follows [`Role::default_caps`], so changing what users may do
//!   changes this with it;
//! - browser tools, when the caller may use them;
//! - read-only file tools on the HOST (`read_file`, `search_files`),
//!   outside the paths the F1 read jail fences, and never a search whose
//!   root holds them.
//!
//! Everything else needs a ticket (ticket.rs): one exact call, signed on
//! a device that is not the calling node. The refusal carries the ticket
//! request, so the owner can sign it and the call can be made again with
//! it. A ticket's grant decides the level the call runs at, and an owner
//! grant runs it as this node's own owner surface would.
//!
//! **Who is calling** is what this node established — the authenticated
//! peer and the node id gossip bound it to — never what the ask claims.

use crate::acl::{Cap, Principal, Role};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// The marker every remote tool call starts with.
pub const REMOTE_TOOL_MARKER: &str = "[veles remote tool]";
/// The reply marker (the tool's output rides inside).
pub const REMOTE_TOOL_REPLY_MARKER: &str = "[veles remote tool reply]";

/// How long a caller waits for a remote tool: a terminal command may run
/// for its own long cap, and the transport must outlive it.
pub const CALL_TIMEOUT_SECS: u64 = 1020;

/// The most output a reply carries. The calling agent caps what the
/// model reads anyway; this bounds the message on the wire.
const MAX_REPLY_CHARS: usize = 512 * 1024;

/// Tools that act on the calling session's own state (its todos, plan,
/// memory, subagents, schedule), or that only wrap another call. Never
/// re-homed, whatever a glob says: on another node they would act on a
/// session that is not there.
pub const STAYS_HOME: &[&str] = &[
    "todo",
    "memory",
    "clarify",
    "show_plan",
    "delegate_task",
    "cronjob",
    "session_search",
    "session_export",
    "skill_view",
    "skills_list",
    "skill_manage",
    "tool_search",
    "tool_describe",
    "tool_call",
    "a2a",
];

pub fn is_remote_tool(raw: &str) -> bool {
    raw.trim_start().starts_with(REMOTE_TOOL_MARKER)
}

pub fn is_remote_tool_reply(raw: &str) -> bool {
    raw.trim_start().starts_with(REMOTE_TOOL_REPLY_MARKER)
}

/// One tool call, as it travels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteToolAsk {
    /// The calling node's id. Checked against the node gossip bound the
    /// authenticated peer to; a mismatch is refused.
    pub from_node: String,
    /// The calling session (the ticket names it).
    pub session: String,
    /// The tool's own name — never an alias (the receiver runs exactly
    /// this tool, and a ticket names exactly this tool).
    pub tool: String,
    pub args: Value,
    /// The caller's principal, serialized as a local child's
    /// `VELES_SESSION_PRINCIPAL` carries it; `None` = an owner surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// A ticket for exactly this call, when it needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// The id the receiver registers its task under, so the caller's
    /// Stop can cancel it (remotetask.rs C3, the same scheme).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_task_id: Option<String>,
    /// The call goes the way other people's web calls go (egress.rs),
    /// even when it is the owner's: their `/egress tor`. Someone else's
    /// goes that way whatever this says.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub others: bool,
}

pub fn encode_ask(ask: &RemoteToolAsk) -> Result<String> {
    Ok(crate::remotetask::fence(
        REMOTE_TOOL_MARKER,
        "one tool call over the cluster link",
        &serde_json::to_string(ask)?,
    ))
}

pub fn decode_ask(raw: &str) -> Result<RemoteToolAsk> {
    serde_json::from_str(&crate::remotetask::unfence(raw)?)
        .context("payload is not a remote tool call")
}

pub fn encode_reply(reply: &Value) -> Result<String> {
    Ok(crate::remotetask::fence(
        REMOTE_TOOL_REPLY_MARKER,
        "",
        &serde_json::to_string(reply)?,
    ))
}

pub fn decode_reply(raw: &str) -> Result<Value> {
    serde_json::from_str(&crate::remotetask::unfence(raw)?)
        .context("payload is not a remote tool reply")
}

// ── D1: where a tool runs ──────────────────────────────────────────

/// How specific a glob is: exact names first, then more literal
/// characters.
fn specificity(glob: &str) -> (bool, usize) {
    (
        !glob.contains(['*', '?']),
        glob.chars().filter(|c| !matches!(c, '*' | '?')).count(),
    )
}

/// The most specific of `rules` that matches `tool`; a later rule wins
/// a tie (a session's newer `/tools home` over its older one).
fn pick<'a, T: 'a>(rules: impl Iterator<Item = (&'a str, &'a T)>, tool: &str) -> Option<&'a T> {
    let mut best: Option<((bool, usize), &T)> = None;
    for (glob, node) in rules {
        if !crate::triggers::glob_match(glob.trim(), tool) {
            continue;
        }
        let s = specificity(glob.trim());
        if best.is_none_or(|(b, _)| s >= b) {
            best = Some((s, node));
        }
    }
    best.map(|(_, n)| n)
}

/// Where `tool` runs for `session`, in order of preference: the
/// session's `/tools home` rules first (one node each), then
/// `[tools.home]` (a node, or an ordered list). `None` = here — no rule,
/// a first choice that is this node or `local`, or a tool that
/// [`STAYS_HOME`]. A later entry that is this node stays in the list: it
/// is "here", tried at its position.
pub fn homes_for(
    store: &crate::store::Store,
    homes: &std::collections::BTreeMap<String, crate::config::HomeSpec>,
    session: &str,
    tool: &str,
) -> Option<Vec<String>> {
    if STAYS_HOME.contains(&tool) {
        return None;
    }
    let rows = if session.is_empty() {
        Vec::new()
    } else {
        store.tool_homes(session).unwrap_or_default()
    };
    let nodes = match pick(rows.iter().map(|(g, n)| (g.as_str(), n)), tool) {
        Some(n) => vec![n.trim().to_string()],
        None => pick(homes.iter().map(|(g, n)| (g.as_str(), n)), tool)?.nodes(),
    };
    match nodes.first() {
        Some(first) if !is_here(first) => Some(nodes),
        _ => None,
    }
}

/// The first node of [`homes_for`].
pub fn home_for(
    store: &crate::store::Store,
    homes: &std::collections::BTreeMap<String, crate::config::HomeSpec>,
    session: &str,
    tool: &str,
) -> Option<String> {
    homes_for(store, homes, session, tool).and_then(|v| v.into_iter().next())
}

/// The tools whose calls act in a zone (`user_envs::zone_for`): the ones
/// a node without a sandbox sends to its `[cluster] sandbox_home`.
pub const SANDBOX_TOOLS: &[&str] = &[
    "terminal",
    "read_file",
    "write_file",
    "patch",
    "search_files",
    "execute_code",
];

/// Where a call goes on a node that names a `[cluster] sandbox_home`
/// (`home`): `Some(Ok(node))` to run it there, `Some(Err(why))` to
/// refuse it, `None` to run it here — no home, not a zone tool, a call
/// whose zone is the host (`sandboxed` false), or a home that is this
/// node. A background shell is refused: it is process-local, and the
/// home keeps no shell for another node.
pub fn sandbox_home_for(
    home: Option<&str>,
    tool: &str,
    args: &Value,
    sandboxed: bool,
) -> Option<Result<String, String>> {
    let node = home.map(str::trim).filter(|n| !n.is_empty())?;
    if !sandboxed || !SANDBOX_TOOLS.contains(&tool) || is_here(node) {
        return None;
    }
    if tool == "terminal" && args["background"].as_bool().unwrap_or(false) {
        return Some(Err(format!(
            "error: this node has no sandbox of its own, and its sandboxed calls run on node \
             {node}, which keeps no background shell for another node. Run the command in \
             the foreground."
        )));
    }
    Some(Ok(node.to_string()))
}

// ── T2: automatic homes (PLAN-phone-userland.md) ───────────────────

/// What a tool needs on the node that runs it: a tool glob and the
/// programs (`node::PROBED_TOOLS` names) of which any one will do. A
/// node that advertises none of them sends the call to a peer that does
/// ([`auto_home_for`]). `text_to_speech` needs edge-tts only when no API
/// provider is configured (`[agent] tts_provider`, see [`needs_of`]).
pub const NEEDS: &[(&str, &[&str])] = &[
    ("terminal", &["bash", "sh"]),
    ("search_files", &["rg"]),
    ("browser_*", &["agent-browser"]),
    ("text_to_speech", &["edge-tts"]),
    ("clipboard_*", &["xclip", "xsel", "wl-paste"]),
    ("execute_code", &["tclsh"]),
];

/// The container runtimes a sandbox-zone call needs ([`SANDBOX_TOOLS`]
/// whose zone is a sandbox): a node with none of them has no sandbox of
/// its own, and [`auto_sandbox_home`] finds one. `userland` is the
/// phone's (PLAN-phone-userland.md T3 design item 4), advertised only
/// while its userland makes this node's boxes (`node::probe_tools`).
pub const SANDBOX_RUNTIMES: &[&str] = &[
    "podman",
    "systemd-nspawn",
    "firecracker",
    "jail",
    "userland",
];

/// The programs `tool` needs, when [`NEEDS`] names it and the config
/// does not already satisfy it (an API TTS provider needs no edge-tts).
pub fn needs_of(cfg: &crate::config::VelesConfig, tool: &str) -> Option<&'static [&'static str]> {
    if tool == "text_to_speech"
        && cfg
            .agent
            .tts_provider
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty())
    {
        return None;
    }
    NEEDS
        .iter()
        .find(|(glob, _)| crate::triggers::glob_match(glob, tool))
        .map(|(_, names)| *names)
}

/// Whether a session or `[tools.home]` rule names `tool` at all —
/// `local` included, which is how an operator keeps a tool here whatever
/// this node lacks. [`homes_for`] answers None for both "no rule" and
/// "a rule that says here"; the automatic home must tell them apart.
fn pinned(
    store: &crate::store::Store,
    homes: &std::collections::BTreeMap<String, crate::config::HomeSpec>,
    session: &str,
    tool: &str,
) -> bool {
    let rows = if session.is_empty() {
        Vec::new()
    } else {
        store.tool_homes(session).unwrap_or_default()
    };
    pick(rows.iter().map(|(g, n)| (g.as_str(), n)), tool).is_some()
        || pick(homes.iter().map(|(g, n)| (g.as_str(), n)), tool).is_some()
}

/// The `[a2a_agents]` name a registry row is reached through: the agent
/// whose url is the node's endpoint, else the one named like the node
/// (`delegate::remote_target_for`'s match, over a config in hand).
fn agent_name_for<'a>(
    cfg: &'a crate::config::VelesConfig,
    node: &crate::store::NodeRow,
) -> Option<&'a str> {
    let by_endpoint = node.endpoint.as_deref().and_then(|ep| {
        cfg.a2a_agents
            .iter()
            .find(|(_, a)| a.url.trim_end_matches('/') == ep.trim_end_matches('/'))
    });
    by_endpoint
        .or_else(|| cfg.a2a_agents.iter().find(|(name, _)| **name == node.name))
        .map(|(name, _)| name.as_str())
}

/// The live peers that may take work from here (`[cluster]
/// task_peers`, reached through `[a2a_agents]`) and advertise one of
/// `needs`, least loaded first (`placement::load_order`). Never this
/// node; never a stale one; never a node whose row advertises nothing.
pub fn capable_peers(
    store: &crate::store::Store,
    cfg: &crate::config::VelesConfig,
    me: &str,
    needs: &[&str],
) -> Vec<crate::store::NodeRow> {
    let stale = cfg.cluster.stale_after_secs as i64;
    let mut peers: Vec<crate::store::NodeRow> = store
        .nodes_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.id != me)
        .filter(|n| !crate::node::is_stale(&n.heartbeat_at, stale))
        .filter(|n| n.caps_has_any(needs))
        .filter(|n| {
            agent_name_for(cfg, n).is_some_and(|a| cfg.cluster.task_peers.iter().any(|p| p == a))
        })
        .collect();
    peers.sort_by(crate::placement::load_order);
    peers
}

/// This node's advertised programs, from its own registry row (what
/// the gateway probed at start and registered). `None` = unknown: no
/// identity, no row, or a row from before T2 — and unknown runs here.
fn my_tools(store: &crate::store::Store, me: Option<&str>) -> Option<Vec<String>> {
    store.node_get(me?).ok()??.caps_tools()
}

/// Where a call to `tool` goes when this node lacks what it needs and
/// no rule says where (the automatic home). `None` = here: the tool
/// stays home ([`STAYS_HOME`]), it needs nothing [`NEEDS`] names, this
/// node has one of the programs (or cannot say what it has),
/// `[tools] auto_home = false`, a session or `[tools.home]` rule names
/// the tool (`local` is the opt-out), or no live task peer advertises
/// one. Otherwise the capable peers' ids, least loaded first: an
/// ordered home, so an unreachable one passes to the next.
pub fn auto_home_for(
    store: &crate::store::Store,
    cfg: &crate::config::VelesConfig,
    me: Option<&str>,
    session: &str,
    tool: &str,
) -> Option<Vec<String>> {
    if STAYS_HOME.contains(&tool) || !cfg.tools.auto_home {
        return None;
    }
    let needs = needs_of(cfg, tool)?;
    if pinned(store, &cfg.tools.home, session, tool) {
        return None;
    }
    let mine = my_tools(store, me)?;
    if needs.iter().any(|n| mine.iter().any(|t| t == n)) {
        return None;
    }
    let peers = capable_peers(store, cfg, me?, needs);
    (!peers.is_empty()).then(|| peers.into_iter().map(|n| n.id).collect())
}

/// The `[cluster] sandbox_home` this node would have, when it names
/// none: on a node with no container runtime ([`SANDBOX_RUNTIMES`]), the
/// least loaded live task peer that advertises one. `None` when a rule
/// names the tool, `auto_home` is off, this node has a runtime (or
/// cannot say), or no peer has one.
pub fn auto_sandbox_home(
    store: &crate::store::Store,
    cfg: &crate::config::VelesConfig,
    me: Option<&str>,
    session: &str,
    tool: &str,
) -> Option<String> {
    if !cfg.tools.auto_home
        || !SANDBOX_TOOLS.contains(&tool)
        || pinned(store, &cfg.tools.home, session, tool)
    {
        return None;
    }
    let mine = my_tools(store, me)?;
    if SANDBOX_RUNTIMES.iter().any(|r| mine.iter().any(|t| t == r)) {
        return None;
    }
    capable_peers(store, cfg, me?, SANDBOX_RUNTIMES)
        .into_iter()
        .next()
        .map(|n| n.id)
}

/// `local`, `here`, or this node's own id or name.
pub fn is_here(node: &str) -> bool {
    let n = node.trim();
    if n.is_empty() || n.eq_ignore_ascii_case("local") || n.eq_ignore_ascii_case("here") {
        return true;
    }
    crate::node::NodeIdentity::load(&crate::config::config_dir())
        .ok()
        .flatten()
        .is_some_and(|me| me.id == n || (!me.name.is_empty() && me.name == n))
}

// ── D1: the calling side ───────────────────────────────────────────

/// What a remote call came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The rule resolved to this node after all (an id prefix of our
    /// own): run it here.
    Here,
    /// The node ran it; its output.
    Ran { node: String, output: String },
    /// The node runs it only with a ticket. `request` is what a signer
    /// signs (`veles ticket sign`).
    NeedsTicket {
        node: String,
        why: String,
        request: String,
    },
    /// Nothing ran; why, naming the node.
    Failed(String),
    /// Nothing ran because the node could not be reached at all: its
    /// heartbeat is stale, or the connection never opened. The one
    /// failure a tool's next home may be tried after — nothing can have
    /// run there.
    Unreachable(String),
}

impl Answer {
    /// The tool result the model reads.
    pub fn text(&self, tool: &str) -> String {
        match self {
            Answer::Here => String::new(),
            Answer::Ran { node, output } => format!("[{tool} ran on node {node}]\n{output}"),
            Answer::NeedsTicket { node, why, request } => format!(
                "error: {tool} is homed on node {node}, which runs it only with a ticket: \
                 {why}\nThe owner can sign this request on a device that is not this node \
                 (`veles ticket sign '<request>'`) and paste the ticket back:\n{request}"
            ),
            Answer::Failed(why) | Answer::Unreachable(why) => format!("error: {why}"),
        }
    }
}

/// A large file's parts, from the node whose reply announced it
/// (downloads.rs): one call each, so no message carries more than
/// [`crate::downloads::PART_BYTES`] of it.
async fn pull_parts(
    client: &crate::a2a::A2AClient,
    url: &str,
    bearer: Option<&str>,
    session: &str,
    principal: Option<&Principal>,
    a: &crate::downloads::Announced,
) -> Result<Vec<String>> {
    let mut parts = Vec::with_capacity(a.parts);
    for index in 0..a.parts {
        let ask = RemoteToolAsk {
            from_node: my_id().unwrap_or_default(),
            session: session.to_string(),
            tool: crate::downloads::PART_OP.to_string(),
            args: json!({ "token": a.token, "index": index }),
            principal: principal.and_then(|p| serde_json::to_string(p).ok()),
            ticket: None,
            wire_task_id: Some(crate::a2a::new_task_id()),
            others: false,
        };
        let result = client
            .call(
                url,
                bearer,
                &encode_ask(&ask)?,
                &crate::a2a::new_context_id(),
            )
            .await?;
        let v = decode_reply(&crate::a2a::extract_text(&result))?;
        anyhow::ensure!(
            v["status"] == "ok",
            "part {} of {}: {}",
            index + 1,
            a.parts,
            v["error"].as_str().unwrap_or("refused")
        );
        parts.push(
            v["part"]
                .as_str()
                .context("a part without its bytes")?
                .to_string(),
        );
    }
    Ok(parts)
}

/// The node a re-homed call goes to, and how to dial it when it is not
/// here: its A2A url and, when configured, the bearer token.
type Dial = (crate::store::NodeRow, Option<(String, Option<String>)>);

/// Resolve the node `spec` names to a row and how to dial it.
fn resolve_target(store: &crate::store::Store, spec: &str) -> Result<Dial> {
    let stale = crate::config::load()
        .map(|c| c.cluster.stale_after_secs)
        .unwrap_or(600) as i64;
    let me = my_id();
    let routable = |n: &crate::store::NodeRow| {
        me.as_deref() == Some(n.id.as_str()) || crate::delegate::remote_target_for(n).is_some()
    };
    let p = crate::placement::resolve(
        store,
        &crate::placement::BackendSpec::Node(spec.to_string()),
        me.as_deref(),
        stale,
        &routable,
    )?;
    if me.as_deref() == Some(p.node.id.as_str()) {
        return Ok((p.node, None));
    }
    let dial = crate::delegate::remote_target_for(&p.node);
    Ok((p.node, Some(dial.unwrap_or_default())))
}

/// Is the node `spec` names in the registry with a heartbeat too old to
/// trust? (`placement::resolve` refuses such a node; this tells that
/// refusal apart from a node that does not exist.)
fn node_is_stale(store: &crate::store::Store, spec: &str) -> bool {
    let stale = crate::config::load()
        .map(|c| c.cluster.stale_after_secs)
        .unwrap_or(600) as i64;
    let row = match store.node_resolve(spec) {
        Ok(crate::store::NodeMatch::One(row)) => Some(*row),
        _ => store
            .nodes_list()
            .ok()
            .and_then(|l| l.into_iter().find(|n| n.name == spec)),
    };
    row.is_some_and(|n| crate::node::is_stale(&n.heartbeat_at, stale))
}

/// Did an A2A call fail before its connection opened (refused, no
/// route, the connect timeout)? Then nothing was sent.
fn never_connected(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<reqwest::Error>()
            .is_some_and(|r| r.is_connect())
    })
}

pub(crate) fn my_id() -> Option<String> {
    crate::node::NodeIdentity::load(&crate::config::config_dir())
        .ok()
        .flatten()
        .map(|i| i.id)
}

/// One tool call as it leaves this node. Made inside the call's scope
/// (egress.rs): a call that goes the way other people's web calls go
/// says so, the owner's `/egress tor` included, so the node that runs it
/// does not take it for the owner's own.
fn tool_ask(
    session: &str,
    tool: &str,
    args: Value,
    principal: Option<&Principal>,
    ticket: Option<String>,
) -> RemoteToolAsk {
    RemoteToolAsk {
        from_node: my_id().unwrap_or_default(),
        session: session.to_string(),
        tool: tool.to_string(),
        args,
        principal: principal.and_then(|p| serde_json::to_string(p).ok()),
        ticket,
        wire_task_id: Some(crate::a2a::new_task_id()),
        others: crate::egress::others().is_some(),
    }
}

/// Run `tool(args)` on the node `spec` names, for `session`, under the
/// caller's principal (`None` = an owner surface here). Never a quiet
/// local run: a call homed elsewhere for a reason (an IP, a browser, a
/// GPU) that ran here instead would be the wrong answer looking right —
/// every failure names the node.
pub async fn call_on(
    store: &Arc<Mutex<crate::store::Store>>,
    spec: &str,
    session: &str,
    tool: &str,
    args: Value,
    principal: Option<&Principal>,
    ticket: Option<String>,
) -> Answer {
    let resolved = {
        let st = store.lock().unwrap_or_else(|p| p.into_inner());
        resolve_target(&st, spec)
    };
    let (node, dial) = match resolved {
        Ok(r) => r,
        Err(e) => {
            let why = format!("{tool} is homed on node {spec}: {e:#}");
            let st = store.lock().unwrap_or_else(|p| p.into_inner());
            return if node_is_stale(&st, spec) {
                Answer::Unreachable(why)
            } else {
                Answer::Failed(why)
            };
        }
    };
    let Some((url, bearer)) = dial else {
        return Answer::Here;
    };
    if url.is_empty() {
        return Answer::Failed(format!(
            "{tool} is homed on node {} ({}), which has no [a2a_agents] entry here — add one \
             with its URL and token",
            node.name, node.id
        ));
    }
    let ask = tool_ask(session, tool, args, principal, ticket);
    let text = match encode_ask(&ask) {
        Ok(t) => t,
        Err(e) => return Answer::Failed(format!("{tool}: cannot encode the call: {e:#}")),
    };
    let client =
        crate::a2a::A2AClient::with_timeout(std::time::Duration::from_secs(CALL_TIMEOUT_SECS));
    let ctx = crate::a2a::new_context_id();
    let watch = crate::remotetask::CancelWatch::turn(session);
    let result = tokio::select! {
        r = client.call(&url, bearer.as_deref(), &text, &ctx) => r,
        () = watch.wait() => {
            if let Some(id) = ask.wire_task_id.as_deref() {
                let _ = crate::a2a::A2AClient::new()
                    .cancel_task(&url, bearer.as_deref(), id)
                    .await;
            }
            return Answer::Failed(format!("{tool} on node {}: cancelled", node.name));
        }
    };
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            let why = format!(
                "{tool} is homed on node {}, which did not answer: {e:#}",
                node.name
            );
            // Only a connection that never opened is "unreachable": a
            // call that was sent may have run there.
            return if never_connected(&e) {
                Answer::Unreachable(why)
            } else {
                Answer::Failed(why)
            };
        }
    };
    let reply_text = crate::a2a::extract_text(&result);
    if !is_remote_tool_reply(&reply_text) {
        return Answer::Failed(format!(
            "node {} did not run {tool}: {reply_text}",
            node.name
        ));
    }
    let v = match decode_reply(&reply_text) {
        Ok(v) => v,
        Err(e) => return Answer::Failed(format!("node {}'s answer: {e:#}", node.name)),
    };
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    match v["status"].as_str() {
        Some("ok") => {
            // Files the tool downloaded there, staged here for the
            // session's node to deliver (downloads.rs).
            for a in crate::downloads::receive(&v["files"]) {
                let parts = pull_parts(&client, &url, bearer.as_deref(), session, principal, &a)
                    .await
                    .with_context(|| format!("from node {}", node.name));
                crate::downloads::accept(&a, parts);
            }
            Answer::Ran {
                node: node.name,
                output: s("output"),
            }
        }
        Some("needs_ticket") => Answer::NeedsTicket {
            node: node.name,
            why: s("error"),
            request: s("request"),
        },
        _ => Answer::Failed(format!("node {} refused {tool}: {}", node.name, s("error"))),
    }
}

/// D3: ask whoever is attached to `session` for a ticket for a call
/// that needs one. The prompt names the call; the detail describes it
/// in full (the arguments too — the person approving reads what will
/// run, not a summary) and ends with the request token a signer signs.
/// The answer is the ticket: `None` when declined, timed out (the
/// request's own expiry), or when nobody interactive is attached.
pub async fn ask_for_ticket(
    session: &str,
    tool: &str,
    node: &str,
    why: &str,
    request: &str,
) -> Option<String> {
    use crate::ask::{AskChoice, AskKind, AskRequest, CHOICE_TICKET_DECLINE};
    let described = crate::ticket::TicketRequest::decode(request)
        .map(|r| r.describe(4000))
        .unwrap_or_default();
    let req = AskRequest::new(
        session,
        AskKind::Ticket,
        &format!("{tool} on node {node} needs a ticket — {why}"),
        vec![AskChoice::new(CHOICE_TICKET_DECLINE, "decline")],
    )
    .with_detail(format!(
        "{described}\n\nSign it on a device that is not this node, then paste the \
         ticket here:\n{request}"
    ))
    .with_timeout(crate::ticket::DEFAULT_TTL_SECS as u64);
    let ask_id = req.id.clone();
    let asked = crate::ask::ask_session(session, req);
    let cfg = crate::config::live();
    let urls = cfg.cluster.signers.clone();
    drop(cfg);
    if urls.is_empty() {
        return ticket_in(asked.await?);
    }
    // Part S: the headless signers are asked ALONGSIDE the session. A
    // chat surface can show the request and take a pasted ticket, but it
    // cannot sign; a headless signer can, once the owner says
    // yes on its own bot or nick. Whichever answers first decides.
    let token = signer_token();
    let signed = ask_signers(&urls, &token, request);
    tokio::pin!(asked);
    tokio::pin!(signed);
    let (mut ask_open, mut signer_open) = (true, true);
    loop {
        tokio::select! {
            a = &mut asked, if ask_open => {
                ask_open = false;
                match a {
                    // A ticket pasted here, or a decline here.
                    Some(answer) => return ticket_in(answer),
                    // Nobody to ask here, or nobody answered: the
                    // signer may still.
                    None if signer_open => {}
                    None => return None,
                }
            }
            s = &mut signed, if signer_open => {
                signer_open = false;
                match s {
                    SignerAnswer::Ticket(t) => {
                        // Closes the prompt on every surface, saying why.
                        crate::ask::resolve(&ask_id, crate::ask::AskAnswer::text(&t));
                        return Some(t);
                    }
                    SignerAnswer::Declined(by) => {
                        tracing::info!("ticket for {tool} on {node}: declined on the signer by {by}");
                        crate::ask::resolve(
                            &ask_id,
                            crate::ask::AskAnswer::choice(CHOICE_TICKET_DECLINE),
                        );
                        return None;
                    }
                    SignerAnswer::Unavailable(why) => {
                        tracing::warn!("ticket for {tool} on {node}: no signer could ask — {why}");
                        if !ask_open {
                            return None;
                        }
                    }
                }
            }
        }
    }
}

/// The ticket in an answer typed or pasted on a surface.
fn ticket_in(answer: crate::ask::AskAnswer) -> Option<String> {
    let text = answer.free_text?;
    crate::ticket::Ticket::is_in(&text).then(|| text.trim().to_string())
}

/// The token this gateway presents to the headless signers.
///
/// Read where every other gateway secret is (the environment, the
/// vault, the CONFIG dir's `.env`). It used to be read from the data
/// dir, which is the same directory only under a forced `VELES_HOME`
/// (the container, the phone): on an XDG install like the laptop the
/// token was never found, and every signer answered 401.
fn signer_token() -> String {
    crate::secrets::resolve("VELES_SIGNER_TOKEN").unwrap_or_default()
}

/// What the headless signers said about one request.
#[derive(Debug, Clone, PartialEq)]
pub enum SignerAnswer {
    /// Signed: the ticket.
    Ticket(String),
    /// The owner said no — on the signer's bot or nick.
    Declined(String),
    /// None of them could put it to the owner (down, refused, timed out).
    Unavailable(String),
}

/// Ask every signer in `urls` for a ticket for `request`. The first
/// ticket wins; an owner's decline on any of them is a decline.
pub async fn ask_signers(urls: &[String], token: &str, request: &str) -> SignerAnswer {
    use futures::StreamExt as _;
    // The whole ask may take the ticket's TTL (a person decides on the
    // signer's side), but DIALING a signer may not: a signer that is
    // down, or a host whose closed ports answer nothing (Windows with
    // its firewall's stealth rules black-holed a loopback SYN and this
    // waited on the IO driver for the full TTL, 2026-10-13), must become
    // `Unavailable` in seconds, as `A2AClient` already does for peers.
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            crate::ticket::DEFAULT_TTL_SECS as u64 + 30,
        ))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return SignerAnswer::Unavailable(format!("{e}")),
    };
    let mut asks: futures::stream::FuturesUnordered<_> = urls
        .iter()
        .map(|u| ask_one_signer(&client, u, token, request))
        .collect();
    let mut why = Vec::new();
    while let Some(a) = asks.next().await {
        match a {
            SignerAnswer::Unavailable(w) => why.push(w),
            decided => return decided,
        }
    }
    SignerAnswer::Unavailable(why.join("; "))
}

async fn ask_one_signer(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    request: &str,
) -> SignerAnswer {
    let r = client
        .post(format!("{}/v1/sign", url.trim_end_matches('/')))
        .bearer_auth(token)
        .json(&serde_json::json!({ "request": request }))
        .send()
        .await;
    let r = match r {
        Ok(r) => r,
        Err(e) => return SignerAnswer::Unavailable(format!("{url}: {e}")),
    };
    let status = r.status().as_u16();
    let body: Value = r.json().await.unwrap_or(Value::Null);
    match (status, body["ticket"].as_str(), body["declined"].as_str()) {
        (200, Some(t), _) if crate::ticket::Ticket::is_in(t) => SignerAnswer::Ticket(t.to_string()),
        (403, _, Some(by)) => SignerAnswer::Declined(by.to_string()),
        _ => SignerAnswer::Unavailable(format!(
            "{url}: {status} {}",
            body["refused"]
                .as_str()
                .or(body["error"].as_str())
                .unwrap_or("no answer")
        )),
    }
}

// ── D2: the receiving side ─────────────────────────────────────────

/// Who is calling, as this node established it.
pub struct Caller<'a> {
    /// The node id gossip bound the authenticated peer to, when known.
    pub node: Option<&'a str>,
    /// Its name (the peer's label when unbound).
    pub name: &'a str,
}

/// How an allowed call runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Grant {
    /// Within what a peer may do unasked, as this capped principal.
    Unasked(Principal),
    /// A read-only file tool on the host.
    HostRead,
    /// A ticket raised it to `role`.
    Ticket {
        role: Role,
        signer: String,
        id: String,
    },
}

impl Grant {
    /// For the audit log and the answer.
    pub fn label(&self) -> String {
        match self {
            Grant::Unasked(p) => format!("{} (unasked)", p.role.as_str()),
            Grant::HostRead => "a host read (unasked)".into(),
            Grant::Ticket { role, signer, id } => {
                format!("{} (ticket {id}, signed by {signer})", role.as_str())
            }
        }
    }
}

/// What this node does with a call.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Run(Grant),
    /// Not without a ticket: why, and the request a signer signs.
    NeedsTicket {
        why: String,
        request: String,
    },
    Refused(String),
}

/// The caller's principal as it arrived.
pub fn carried_principal(raw: Option<&str>) -> Result<Principal> {
    match raw {
        None => Ok(Principal::owner()),
        Some(s) => serde_json::from_str(s).context("the caller's principal does not parse"),
    }
}

/// The caller as it runs here unasked: the lower of its own role and
/// User, with the User role's capabilities (as that role is defined on
/// THIS node) plus browser tools — each only where the caller had it.
/// Its subject names the calling node, so a peer's sandbox is its own.
pub fn capped(carried: &Principal, caller: &Caller) -> Principal {
    let mut allowed = Role::User.default_caps();
    allowed.insert(Cap::Browser);
    let caps = allowed.into_iter().filter(|c| carried.has(*c)).collect();
    let subject = format!("peer:{}/{}", caller.name, carried.subject);
    Principal {
        id: Some(subject.clone()),
        subject,
        role: carried.role.min(Role::User),
        caps,
        rule_context_limit: None,
        mask: None,
        via: Some(format!(
            "a call from node {} — capped at the user level without a ticket",
            caller.name
        )),
        ticket_role: None,
    }
}

/// Why a host read is not one a peer may make unasked.
fn host_read_denial(tool: &str, args: &Value) -> Option<&'static str> {
    let path = args["path"].as_str().unwrap_or(".");
    if tool == "search_files" {
        crate::approval::search_root_holds_secrets(path)
    } else {
        crate::approval::sensitive_read_path(path)
    }
}

/// The ticket request for a call that needs one: the caller's own
/// level, at least User (nothing below needs a ticket).
fn request_for(me: &str, origin: &str, ask: &RemoteToolAsk, carried: &Principal) -> String {
    let grant = carried.role.max(Role::User).as_str();
    crate::ticket::TicketRequest::new(
        origin,
        me,
        &ask.session,
        &ask.tool,
        ask.args.clone(),
        grant,
        crate::ticket::DEFAULT_TTL_SECS,
    )
    .and_then(|r| r.encode())
    .unwrap_or_default()
}

/// Decide how (and whether) a call from a peer runs here. Spends the
/// ticket when one authorizes it — a ticket is one call.
pub fn decide(
    store: &crate::store::Store,
    cfg: &crate::config::VelesConfig,
    me: &str,
    caller: &Caller,
    ask: &RemoteToolAsk,
    now: i64,
) -> Decision {
    let carried = match carried_principal(ask.principal.as_deref()) {
        Ok(p) => p,
        Err(e) => return Decision::Refused(format!("{e:#}")),
    };
    if carried.role == Role::Banned {
        return Decision::Refused("the caller is banned on its own node".into());
    }
    if let Some(bound) = caller.node {
        if !ask.from_node.is_empty() && ask.from_node != bound {
            return Decision::Refused(format!(
                "the call says it comes from node {}, but this peer is node {bound}",
                ask.from_node
            ));
        }
    }
    if let Some(text) = ask.ticket.as_deref() {
        return match authorize(
            store,
            cfg,
            me,
            caller,
            &ask.session,
            &ask.tool,
            &ask.args,
            text,
            now,
        ) {
            Ok(g) => Decision::Run(g),
            Err(e) => Decision::Refused(format!("{e:#}")),
        };
    }
    let needs = |why: String| Decision::NeedsTicket {
        request: caller
            .node
            .map(|origin| request_for(me, origin, ask, &carried))
            .unwrap_or_default(),
        why: if caller.node.is_some() {
            why
        } else {
            format!(
                "{why} (and this node has not learned which node {} is yet, so no ticket \
                 can name it — let gossip run once)",
                caller.name
            )
        },
    };
    if matches!(ask.tool.as_str(), "read_file" | "search_files") {
        if !carried.has(Cap::FileRead) {
            return needs("the caller may not read files".into());
        }
        return match host_read_denial(&ask.tool, &ask.args) {
            None => Decision::Run(Grant::HostRead),
            Some(why) => needs(format!(
                "{why} — a peer reads this node's files unasked only outside its secrets"
            )),
        };
    }
    let p = capped(&carried, caller);
    // A large download's parts (downloads.rs): the file one of this
    // node's replies announced, named by its token. It takes what the
    // tool that downloads takes, and no tool of its own.
    let gated_as = if ask.tool == crate::downloads::PART_OP {
        "web_extract"
    } else {
        ask.tool.as_str()
    };
    if let Some(denial) = crate::acl::gate_denial(gated_as, cfg, &p) {
        return needs(denial);
    }
    if let crate::user_envs::ZoneArg::Named(want) = crate::user_envs::ZoneArg::from_args(&ask.args)
    {
        if !crate::sandboxes::may_use_session(&p, &want) {
            return Decision::Refused(crate::user_envs::deny_named_sandbox(&want, &p));
        }
    }
    // The media-write jail (agent.rs F1's write side): TTS writes the
    // host filesystem whatever the zone, so its output path must be one
    // this principal could have delivered.
    if ask.tool == "text_to_speech" {
        if let Some(path) = ask.args["output_path"].as_str().filter(|s| !s.is_empty()) {
            if crate::media::deliverable(path, Some(&p)).is_some() {
                return needs("output_path is outside the media directory".into());
            }
        }
    }
    Decision::Run(Grant::Unasked(p))
}

/// Verify and spend a ticket for exactly this call.
#[allow(clippy::too_many_arguments)]
fn authorize(
    store: &crate::store::Store,
    cfg: &crate::config::VelesConfig,
    me: &str,
    caller: &Caller,
    session: &str,
    tool: &str,
    args: &Value,
    text: &str,
    now: i64,
) -> Result<Grant> {
    let origin = caller.node.with_context(|| {
        format!(
            "a ticket names the calling node, and this node has not learned which node {} \
             is yet (no gossip binding for the peer)",
            caller.name
        )
    })?;
    let t = crate::ticket::Ticket::parse(text)?;
    let v = crate::ticket::verify(
        &t,
        &crate::ticket::trusted_signers(cfg),
        &crate::ticket::CallContext {
            me,
            origin,
            session,
            tool,
            args,
            now,
        },
    )?;
    crate::ticket::spend(store, &v, now)?;
    Ok(Grant::Ticket {
        role: Role::parse(&v.grant).context("a ticket grant that is not a role")?,
        signer: v.signer,
        id: v.id,
    })
}

/// The principal a remote SUBAGENT runs under here (B4's delegate, D2):
/// the caller's own, capped like a tool call unless a ticket for exactly
/// this delegation (`delegate_task` with `{goal, context}`) raises it.
/// `Ok(None)` = the owner surface (an owner ticket).
#[allow(clippy::too_many_arguments)]
pub fn delegate_principal(
    store: &crate::store::Store,
    cfg: &crate::config::VelesConfig,
    me: &str,
    caller: &Caller,
    carried: Option<&str>,
    ticket: Option<&str>,
    goal: &str,
    context: &str,
    now: i64,
) -> Result<Option<Principal>> {
    let carried = carried_principal(carried)?;
    anyhow::ensure!(
        carried.role != Role::Banned,
        "the caller is banned on its own node"
    );
    let Some(text) = ticket else {
        return Ok(Some(capped(&carried, caller)));
    };
    let args = json!({"goal": goal, "context": context});
    // The session a delegation names is its origin's; the ask carries
    // none, so the ticket names the empty one.
    match authorize(
        store,
        cfg,
        me,
        caller,
        "",
        "delegate_task",
        &args,
        text,
        now,
    )? {
        Grant::Ticket {
            role: Role::Owner, ..
        } => Ok(None),
        Grant::Ticket { role, .. } => {
            let mut p =
                Principal::known(&format!("peer:{}/{}", caller.name, carried.subject), role);
            p.via = Some(format!("a delegation from node {}, by ticket", caller.name));
            Ok(Some(p))
        }
        _ => unreachable!("authorize only grants by ticket"),
    }
}

/// A short, path-safe form of a node label for session ids.
fn short(label: &str) -> String {
    label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(12)
        .collect()
}

/// Run a decided call and build the reply. The call runs under a
/// session of the CALLER's (namespaced by node and level), so a peer's
/// unasked calls get a sandbox of their own and two calls at different
/// levels never share a principal slot. The tool is run as itself —
/// never through `dispatch`, whose aliases and shape conversions would
/// run a different tool than the one decided on (and ticketed).
pub async fn execute(
    store: Arc<Mutex<crate::store::Store>>,
    caller: &Caller<'_>,
    ask: &RemoteToolAsk,
    grant: &Grant,
    cancel: crate::providers::CancelToken,
) -> Value {
    if ask.tool == crate::downloads::PART_OP {
        let mut v = crate::downloads::serve_part(&ask.args);
        v["ran_as"] = json!(grant.label());
        return v;
    }
    let mut args = ask.args.clone();
    let scoped = |role: Role| {
        format!(
            "peer-{}-{}-{}",
            short(caller.node.unwrap_or(caller.name)),
            role.as_str(),
            ask.session
        )
    };
    let session = match grant {
        Grant::HostRead => {
            if let Some(o) = args.as_object_mut() {
                o.remove("sandbox");
                o.insert("host".into(), json!(true));
            }
            String::new()
        }
        Grant::Unasked(p) => {
            crate::user_envs::strip_host_request(&mut args);
            let sid = scoped(p.role);
            crate::acl::set_session_principal(&sid, p.clone());
            sid
        }
        Grant::Ticket {
            role: Role::Owner, ..
        } => String::new(),
        Grant::Ticket { role, .. } => {
            let carried = carried_principal(ask.principal.as_deref())
                .map(|p| p.subject)
                .unwrap_or_default();
            let mut p = Principal::known(&format!("peer:{}/{carried}", caller.name), *role);
            p.via = Some(format!("a call from node {}, by ticket", caller.name));
            let sid = scoped(*role);
            crate::acl::set_session_principal(&sid, p);
            sid
        }
    };
    let registry = crate::tools::base_registry(store, &session);
    // An internal operation (the guest browser) is in no registry: the
    // gate above judged it by its own name, and it runs as itself.
    let internal = crate::browsertools::is_internal(&ask.tool);
    let tool = registry.get(&ask.tool);
    if tool.is_none() && !internal {
        return json!({
            "status": "error",
            "error": format!("there is no tool {} on this node", ask.tool),
        });
    }
    // As the caller's session, so a web call for someone other than the
    // owner leaves the way `[web] others` says (egress.rs). Whose call it
    // is comes from the principal it CARRIES, not the level it runs at
    // here: an unasked call runs capped at user level, the owner's
    // included, and read as someone else's the owner's own searches from
    // IRC were refused on the laptop (D5, live 2026-10-13 01:16–). The
    // owner's call sent that way on purpose (`/egress tor`) runs under a
    // session of the caller's: its own Tor circuit, the owner's box here.
    let owners = carried_principal(ask.principal.as_deref()).is_ok_and(|p| p.role == Role::Owner);
    let scope_as = match (ask.others, owners) {
        (true, _) if session.is_empty() => scoped(Role::Owner),
        (true, _) | (false, false) => session.clone(),
        (false, true) => String::new(),
    };
    let run = crate::egress::scope(&scope_as, ask.others, async {
        match tool {
            Some(t) => t.call(args).await,
            None => crate::browsertools::internal(&ask.tool, args).await,
        }
    });
    let out = tokio::select! {
        out = run => out,
        () = async {
            while !cancel.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        } => return json!({"status": "cancelled", "error": "cancelled by the caller"}),
    };
    // A file the tool downloaded travels beside its text, never in it
    // (downloads.rs).
    let files = crate::downloads::attach(&out);
    let mut output: String = out.chars().take(MAX_REPLY_CHARS).collect();
    if output.len() < out.len() {
        output.push_str("\n… (cut at the cluster reply limit)");
    }
    let mut reply = json!({"status": "ok", "output": output, "ran_as": grant.label()});
    if !files.is_empty() {
        reply["files"] = Value::Array(files);
    }
    reply
}

#[cfg(test)]
mod tests {
    // ── Part S: the headless signer, from the gateway's side ─────────

    /// A5: a list is an order. A first choice that is this node is here;
    /// a later one stays in the list as "here, at that position".
    #[test]
    fn an_ordered_home_is_tried_in_order() {
        let (_d, _env, store) = scratch_store();
        let order =
            |v: &[&str]| crate::config::HomeSpec::Order(v.iter().map(|s| s.to_string()).collect());
        let mut homes = BTreeMap::new();
        homes.insert("web_*".to_string(), order(&["laptop", "vps"]));
        homes.insert("browser_*".to_string(), "laptop".into());
        homes.insert("vision_*".to_string(), order(&["local", "laptop"]));
        homes.insert("image_*".to_string(), order(&["laptop", " ", "local"]));
        assert_eq!(
            homes_for(&store, &homes, "s1", "web_extract"),
            Some(vec!["laptop".to_string(), "vps".to_string()])
        );
        assert_eq!(
            homes_for(&store, &homes, "s1", "browser_navigate"),
            Some(vec!["laptop".to_string()])
        );
        assert_eq!(homes_for(&store, &homes, "s1", "vision_analyze"), None);
        assert_eq!(
            homes_for(&store, &homes, "s1", "image_generate"),
            Some(vec!["laptop".to_string(), "local".to_string()]),
            "blanks dropped, a later `local` kept"
        );
        // A session's own rule is one node, and wins.
        store.tool_home_set("s1", "web_*", "phone").unwrap();
        assert_eq!(
            homes_for(&store, &homes, "s1", "web_extract"),
            Some(vec!["phone".to_string()])
        );
        assert_eq!(
            home_for(&store, &homes, "s2", "web_extract").as_deref(),
            Some("laptop")
        );
    }

    /// The config form: a string or a list, and config sync carries the
    /// list as one replicated key whose value is the whole array.
    #[test]
    fn an_ordered_home_parses_and_replicates_per_key() {
        let raw = "[tools.home]\n\"web_*\" = [\"laptop\", \"vps\"]\n\"browser_*\" = \"laptop\"\n";
        let cfg: crate::config::VelesConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.tools.home["web_*"].nodes(), vec!["laptop", "vps"]);
        assert_eq!(cfg.tools.home["browser_*"], "laptop".into());
        assert_eq!(cfg.tools.home["web_*"].to_string(), "laptop, then vps");
        let snap = crate::configsync::scan_config_toml(raw).unwrap();
        let (key, value) = snap
            .iter()
            .find(|(k, _)| k.contains("web_*"))
            .expect("the web_* rule replicates");
        assert!(key.starts_with("tools.home."), "{key}");
        let back: toml::Value = toml::from_str(&format!("v = {value}")).unwrap();
        assert_eq!(back["v"].as_array().map(|a| a.len()), Some(2), "{value}");
    }

    /// Only a node that could not be reached at all may be passed over:
    /// a stale heartbeat, or a connection that never opened. A call that
    /// was sent may have run there.
    #[tokio::test]
    async fn only_an_unreachable_node_is_passed_over() {
        let refused = reqwest::Client::new()
            .get("http://127.0.0.1:0/")
            .send()
            .await
            .expect_err("nothing listens on port 1");
        assert!(never_connected(
            &anyhow::Error::from(refused).context("a2a call")
        ));
        assert!(!never_connected(&anyhow::anyhow!(
            "node refused web_extract"
        )));

        let (_d, _env, store) = scratch_store();
        store
            .node_register(
                "aaaa-1",
                "vps",
                "h",
                "v",
                "always-on",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert!(!node_is_stale(&store, "vps"), "a fresh heartbeat");
        store
            .conn()
            .execute(
                "UPDATE nodes SET heartbeat_at = '2001-01-01 00:00:00' WHERE id = 'aaaa-1'",
                [],
            )
            .unwrap();
        assert!(node_is_stale(&store, "vps"), "by name");
        assert!(node_is_stale(&store, "aaaa"), "by id prefix");
        assert!(
            !node_is_stale(&store, "nowhere"),
            "unknown is not stale: it is a mistake"
        );
    }

    /// A1: what a node with a `[cluster] sandbox_home` sends there — a
    /// zone tool whose zone is a sandbox, and nothing else.
    #[test]
    fn a_sandboxed_call_goes_to_the_sandbox_home() {
        use super::sandbox_home_for as f;
        // `is_here` reads this node's identity from the config dir, and the
        // laptop that runs the gate is named `laptop`: without a scratch home
        // this test passes only while a neighbour holds one (pitfall 86).
        let (_home, _env) = crate::testenv::scratch_home();
        let none = serde_json::json!({});
        // No home: here.
        assert_eq!(f(None, "execute_code", &none, true), None);
        assert_eq!(f(Some("  "), "execute_code", &none, true), None);
        // A home: sandboxed zone tools go there.
        for tool in super::SANDBOX_TOOLS {
            assert_eq!(
                f(Some("laptop"), tool, &none, true),
                Some(Ok("laptop".to_string())),
                "{tool}"
            );
        }
        // …but not a host-zone call, and not a tool that has no zone.
        assert_eq!(f(Some("laptop"), "execute_code", &none, false), None);
        assert_eq!(f(Some("laptop"), "web_extract", &none, true), None);
        assert_eq!(f(Some("laptop"), "memory", &none, true), None);
        // A home that is this node is no home.
        assert_eq!(f(Some("local"), "terminal", &none, true), None);
        // A background shell is refused by name, not sent.
        let bg = serde_json::json!({"command": "sleep 100", "background": true});
        match f(Some("laptop"), "terminal", &bg, true) {
            Some(Err(why)) => assert!(why.contains("foreground"), "{why}"),
            other => panic!("a background shell must be refused, got {other:?}"),
        }
    }

    /// The signer token is read from the CONFIG dir's `.env`, like every
    /// other gateway secret — on an XDG install that is not the data dir.
    #[test]
    fn the_signer_token_is_read_from_the_config_dir() {
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        std::env::remove_var("VELES_SIGNER_TOKEN");
        let cfg = crate::config::config_dir();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(crate::config::data_dir()).unwrap();
        assert_ne!(cfg, crate::config::data_dir(), "the dirs must be apart");
        std::fs::write(cfg.join(".env"), "VELES_SIGNER_TOKEN=tok-from-config\n").unwrap();
        assert_eq!(super::signer_token(), "tok-from-config");
    }

    /// A stand-in for `veles signer`'s HTTP endpoint: answers every POST
    /// with `status` and `body`, and hands the test what it was sent.
    async fn stub_signer(
        status: u16,
        body: serde_json::Value,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        crate::rt::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut got: Vec<u8> = Vec::new();
                let mut buf = vec![0u8; 16384];
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&got).to_string();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text[..h]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if got.len() >= h + 4 + len {
                            break;
                        }
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&got).to_string());
                let b = body.to_string();
                let _ = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{b}",
                            b.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        (url, rx)
    }

    /// Point this scratch node at `url` as its signer, with a token.
    fn use_signer(url: &str) {
        // The resolver prefers the REAL process env over the scratch
        // `.env` below — and this developer's shell has carried
        // VELES_SIGNER_TOKEN since the VPS signer round. Scrub it (the
        // same env-sensitivity class the sibling config-dir test
        // pinned): without this, an ambient token silently wins and the
        // assertion sees the wrong bearer.
        std::env::remove_var("VELES_SIGNER_TOKEN");
        let home = crate::config::config_dir();
        std::fs::write(
            home.join("config.toml"),
            format!("[cluster]\nsigners = [\"{url}\"]\n"),
        )
        .unwrap();
        std::fs::write(home.join(".env"), "VELES_SIGNER_TOKEN=gw-secret\n").unwrap();
        crate::config::invalidate_live();
    }

    fn a_request() -> String {
        crate::ticket::TicketRequest::new(
            "aaaaaaaa-0000-4000-8000-00000000000a",
            "bbbbbbbb-0000-4000-8000-00000000000b",
            "signer-sess",
            "cronjob",
            serde_json::json!({ "action": "create" }),
            "admin",
            600,
        )
        .unwrap()
        .encode()
        .unwrap()
    }

    /// Nobody is attached to the session — a cron turn, `veles tool` —
    /// and still a ticket comes back: the signer on the other device put
    /// the call to the owner on its own bot. That is the whole point of
    /// Part S: IRC and Telegram can show a request but cannot sign it.
    #[tokio::test]
    async fn a_signer_answers_a_ticket_ask_nobody_here_can() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (url, mut seen) = stub_signer(
            200,
            serde_json::json!({ "ticket": "veles-ticket:1.KID.PAYLOAD.SIG", "approved_by": "telegram:1" }),
        )
        .await;
        use_signer(&url);
        let request = a_request();
        let t = ask_for_ticket(
            "signer-sess",
            "cronjob",
            "laptop",
            "above the User level",
            &request,
        )
        .await;
        assert_eq!(t.as_deref(), Some("veles-ticket:1.KID.PAYLOAD.SIG"));
        let sent = seen.recv().await.expect("the signer was asked");
        assert!(sent.contains("POST /v1/sign"), "{sent}");
        assert!(
            sent.to_ascii_lowercase()
                .contains("authorization: bearer gw-secret"),
            "with this node's token: {sent}"
        );
        assert!(
            sent.contains(&request),
            "for exactly the request the target minted"
        );
    }

    /// The owner's no on the signer is a no here.
    #[tokio::test]
    async fn an_owners_no_on_the_signer_declines() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (url, _seen) = stub_signer(403, serde_json::json!({ "declined": "telegram:1" })).await;
        use_signer(&url);
        let t = ask_for_ticket("signer-sess", "cronjob", "laptop", "why", &a_request()).await;
        assert_eq!(t, None);
    }

    /// A signer that is down or refuses is not a ticket, and with nobody
    /// here to paste one the call is refused rather than waited on.
    #[tokio::test]
    async fn an_unreachable_signer_is_no_ticket() {
        let (_home, _env) = crate::testenv::scratch_home();
        use_signer("http://127.0.0.1:0");
        let t = ask_for_ticket("signer-sess", "cronjob", "laptop", "why", &a_request()).await;
        assert_eq!(t, None);
    }

    use super::*;
    use std::collections::BTreeMap;

    fn ask(tool: &str, args: Value) -> RemoteToolAsk {
        RemoteToolAsk {
            from_node: "node-a".into(),
            session: "s1".into(),
            tool: tool.into(),
            args,
            principal: None,
            ticket: None,
            wire_task_id: None,
            others: false,
        }
    }

    /// A store in a scratch home: `is_here` and the read jail read the
    /// config directory, which must never be the live one.
    fn scratch_store() -> (
        tempfile::TempDir,
        crate::testenv::EnvGuard,
        crate::store::Store,
    ) {
        let (home, env) = crate::testenv::scratch_home();
        let store = crate::store::Store::open(&home.path().join("t.db")).unwrap();
        (home, env, store)
    }

    // ── T2: automatic homes (PLAN-phone-userland.md) ─────────────────

    /// A pool of four, as the registry sees it from the phone: the
    /// phone itself, a laptop, a vps and a desktop, each with the tools
    /// it advertises and its load. The laptop and vps are task peers
    /// (reachable through `[a2a_agents]`); the desktop is NOT.
    fn pool(store: &crate::store::Store, phone_tools: &[&str]) -> crate::config::VelesConfig {
        let reg = |id: &str, name: &str, tools: &[&str], load: u8| {
            let caps = serde_json::json!({ "os": "linux", "tools": tools }).to_string();
            store
                .node_register(
                    id,
                    name,
                    "h",
                    "v",
                    "ephemeral",
                    None,
                    Some(&format!("http://{name}:9900")),
                    None,
                    Some(&caps),
                )
                .unwrap();
            store.node_heartbeat(id, Some(load)).unwrap();
        };
        reg("phone-1", "phone", phone_tools, 10);
        reg(
            "laptop-1",
            "laptop",
            &["bash", "rg", "podman", "agent-browser"],
            60,
        );
        reg("vps-1", "vps", &["bash", "rg"], 20);
        reg("desk-1", "desk", &["bash", "rg", "podman"], 0);
        toml::from_str(
            "[cluster]\ntask_peers = [\"laptop\", \"vps\"]\n\
             [a2a_agents.laptop]\nurl = \"http://laptop:9900\"\n\
             [a2a_agents.vps]\nurl = \"http://vps:9900\"\n\
             [a2a_agents.desk]\nurl = \"http://desk:9900\"\n",
        )
        .unwrap()
    }

    /// The phone without rg sends search_files to the task peers that
    /// have it, least loaded first; the desktop has rg and the lowest
    /// load but is not a task peer, so it is never a home.
    #[test]
    fn phone_without_rg_homes_search_to_peer_that_has_it() {
        let (_d, _env, store) = scratch_store();
        let cfg = pool(&store, &["bash", "sh", "tclsh"]);
        assert_eq!(
            auto_home_for(&store, &cfg, Some("phone-1"), "s1", "search_files"),
            Some(vec!["vps-1".to_string(), "laptop-1".to_string()])
        );
        // The browser: only the laptop has agent-browser.
        assert_eq!(
            auto_home_for(&store, &cfg, Some("phone-1"), "s1", "browser_navigate"),
            Some(vec!["laptop-1".to_string()])
        );
        // What the phone has runs here; what stays home stays.
        assert_eq!(
            auto_home_for(&store, &cfg, Some("phone-1"), "s1", "terminal"),
            None
        );
        assert_eq!(
            auto_home_for(&store, &cfg, Some("phone-1"), "s1", "todo"),
            None
        );
        // An API TTS provider needs no edge-tts.
        let mut tts = cfg.clone();
        assert!(auto_home_for(&store, &tts, Some("phone-1"), "s1", "text_to_speech").is_none());
        tts.agent.tts_provider = Some("openai".into());
        assert_eq!(needs_of(&tts, "text_to_speech"), None);
    }

    /// A rule for the tool wins over the automatic home: `local` keeps
    /// it here (the opt-out), a session's `/tools home` too, and
    /// `[tools] auto_home = false` turns the whole thing off.
    #[test]
    fn pin_to_local_beats_auto_home() {
        let (_d, _env, store) = scratch_store();
        let mut cfg = pool(&store, &["bash"]);
        cfg.tools
            .home
            .insert("search_*".to_string(), "local".into());
        assert_eq!(
            auto_home_for(&store, &cfg, Some("phone-1"), "s1", "search_files"),
            None
        );
        cfg.tools.home.clear();
        store.tool_home_set("s1", "search_files", "local").unwrap();
        assert_eq!(
            auto_home_for(&store, &cfg, Some("phone-1"), "s1", "search_files"),
            None
        );
        assert!(
            auto_home_for(&store, &cfg, Some("phone-1"), "s2", "search_files").is_some(),
            "another session has no rule"
        );
        cfg.tools.auto_home = false;
        assert_eq!(
            auto_home_for(&store, &cfg, Some("phone-1"), "s2", "search_files"),
            None
        );
        let off: crate::config::VelesConfig =
            toml::from_str("[tools]\nauto_home = false\n").unwrap();
        assert!(!off.tools.auto_home);
        assert!(crate::config::VelesConfig::default().tools.auto_home);
    }

    /// Nobody advertises it (or this node cannot say what it has, or a
    /// peer advertises nothing at all): the call runs here and fails or
    /// succeeds on its own terms.
    #[test]
    fn no_peer_has_it_runs_here() {
        let (_d, _env, store) = scratch_store();
        let cfg = pool(&store, &["bash"]);
        assert_eq!(
            auto_home_for(&store, &cfg, Some("phone-1"), "s1", "clipboard_read"),
            None,
            "no node has a clipboard tool"
        );
        assert_eq!(
            auto_home_for(&store, &cfg, None, "s1", "search_files"),
            None,
            "no identity: unknown runs here"
        );
        // A peer on an older build advertises nothing: never a home.
        store
            .node_register(
                "old-1",
                "old",
                "h",
                "v",
                "ephemeral",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let mut only_old = cfg.clone();
        only_old.cluster.task_peers = vec!["old".into()];
        only_old.a2a_agents = toml::from_str::<crate::config::VelesConfig>(
            "[a2a_agents.old]\nurl = \"http://old:9900\"\n",
        )
        .unwrap()
        .a2a_agents;
        assert_eq!(
            auto_home_for(&store, &only_old, Some("phone-1"), "s1", "search_files"),
            None
        );
    }

    /// Least loaded first, and an unknown load ranks after every known
    /// one (unknown is not idle).
    #[test]
    fn peer_order_is_least_loaded() {
        let (_d, _env, store) = scratch_store();
        let cfg = pool(&store, &["bash"]);
        // vps 20, laptop 60: the vps first, against id order.
        assert_eq!(
            auto_home_for(&store, &cfg, Some("phone-1"), "s1", "search_files"),
            Some(vec!["vps-1".to_string(), "laptop-1".to_string()])
        );
        // The vps gets busy: the order follows the load.
        store.node_heartbeat("vps-1", Some(90)).unwrap();
        let peers = capable_peers(&store, &cfg, "phone-1", &["rg"]);
        assert_eq!(
            peers.iter().map(|n| n.load1).collect::<Vec<_>>(),
            vec![Some(60), Some(90)]
        );
    }

    /// A node with no container runtime derives a sandbox home: the
    /// least loaded task peer WITH one (the vps is less loaded and has
    /// none, so the laptop). A node with a runtime, or a configured
    /// rule, keeps its calls.
    #[test]
    fn sandbox_zone_auto_homes_to_runtime_peer() {
        let (_d, _env, store) = scratch_store();
        let cfg = pool(&store, &["bash", "rg"]);
        assert_eq!(
            auto_sandbox_home(&store, &cfg, Some("phone-1"), "s1", "terminal").as_deref(),
            Some("laptop-1")
        );
        assert_eq!(
            auto_sandbox_home(&store, &cfg, Some("phone-1"), "s1", "web_search"),
            None,
            "not a zone tool"
        );
        assert_eq!(
            auto_sandbox_home(&store, &cfg, Some("laptop-1"), "s1", "terminal"),
            None,
            "the laptop has podman"
        );
        let mut pinned = cfg.clone();
        pinned.tools.home.insert("terminal".into(), "local".into());
        assert_eq!(
            auto_sandbox_home(&store, &pinned, Some("phone-1"), "s1", "terminal"),
            None
        );
    }

    /// A phone whose userland makes its boxes (`[sandbox] backend =
    /// "userland"`, which is when it advertises `userland`) keeps its
    /// sandbox-zone calls (PLAN-phone-userland.md T3 design item 4).
    #[test]
    fn a_phone_whose_userland_makes_boxes_keeps_its_sandbox_zone_calls() {
        let (_d, _env, store) = scratch_store();
        let cfg = pool(&store, &["bash", "rg", "apk", "userland"]);
        assert_eq!(
            auto_sandbox_home(&store, &cfg, Some("phone-1"), "s1", "terminal"),
            None,
            "the userland is this node's runtime"
        );
        assert!(SANDBOX_RUNTIMES.contains(&"userland"));
    }

    fn caller() -> Caller<'static> {
        Caller {
            node: Some("node-a"),
            name: "laptop",
        }
    }

    /// Wire round trip, and a payload changed in transit is refused.
    #[test]
    fn an_ask_round_trips_and_tampering_is_refused() {
        let a = ask("web_extract", json!({"urls": ["https://example.org"]}));
        let enc = encode_ask(&a).unwrap();
        assert!(is_remote_tool(&enc));
        assert!(!is_remote_tool_reply(&enc));
        assert!(!crate::remotetask::is_remote_task(&enc));
        let back = decode_ask(&enc).unwrap();
        assert_eq!(back.tool, "web_extract");
        assert_eq!(back.args, a.args);
        let forged = enc.replacen("sha256:", "sha256:0", 1);
        assert!(decode_ask(&forged)
            .unwrap_err()
            .to_string()
            .contains("checksum"));
        let r = encode_reply(&json!({"status": "ok", "output": "hi"})).unwrap();
        assert!(is_remote_tool_reply(&r) && !is_remote_tool(&r));
        assert_eq!(decode_reply(&r).unwrap()["output"], "hi");
    }

    /// The session's rules win over the config's, the most specific glob
    /// wins among either, `local` keeps a tool here, and session-state
    /// tools never leave.
    #[test]
    fn a_tool_goes_where_the_most_specific_rule_says() {
        let (_d, _env, store) = scratch_store();
        let mut homes = BTreeMap::new();
        homes.insert("web_*".to_string(), "laptop".into());
        homes.insert("web_search".to_string(), "phone".into());
        homes.insert("*".to_string(), "vps".into());
        assert_eq!(
            home_for(&store, &homes, "s1", "web_extract").as_deref(),
            Some("laptop")
        );
        assert_eq!(
            home_for(&store, &homes, "s1", "web_search").as_deref(),
            Some("phone")
        );
        assert_eq!(
            home_for(&store, &homes, "s1", "terminal").as_deref(),
            Some("vps")
        );
        assert_eq!(
            home_for(&store, &homes, "s1", "todo"),
            None,
            "session state stays"
        );
        store.tool_home_set("s1", "web_*", "local").unwrap();
        assert_eq!(home_for(&store, &homes, "s1", "web_extract"), None);
        assert_eq!(
            home_for(&store, &homes, "s2", "web_extract").as_deref(),
            Some("laptop"),
            "a session's rule is its own"
        );
        store.tool_home_set("s1", "web_*", "phone").unwrap();
        assert_eq!(
            home_for(&store, &homes, "s1", "web_extract").as_deref(),
            Some("phone")
        );
        assert!(store.tool_home_clear("s1", "web_*").unwrap());
        assert_eq!(
            home_for(&store, &homes, "s1", "web_extract").as_deref(),
            Some("laptop")
        );
        assert_eq!(
            home_for(&store, &BTreeMap::new(), "s1", "web_extract"),
            None
        );
    }

    /// The owner of the calling node runs at the user level here, with
    /// the user role's capabilities plus the browser; a guest stays a
    /// guest.
    #[test]
    fn a_callers_level_is_capped_at_user() {
        let p = capped(&Principal::owner(), &caller());
        assert_eq!(p.role, Role::User);
        let mut want = Role::User.default_caps();
        want.insert(Cap::Browser);
        assert_eq!(p.caps, want);
        assert!(!p.has(Cap::All));
        assert!(p.subject.starts_with("peer:laptop/"), "{}", p.subject);
        let g = capped(&Principal::known("irc:x", Role::Guest), &caller());
        assert_eq!(g.role, Role::Guest);
        assert!(!g.has(Cap::Shell) && !g.has(Cap::Browser) && g.has(Cap::FileRead));
    }

    fn decide_now(store: &crate::store::Store, a: &RemoteToolAsk) -> Decision {
        let cfg = crate::config::VelesConfig::default();
        decide(
            store,
            &cfg,
            "node-b",
            &caller(),
            a,
            chrono::Utc::now().timestamp(),
        )
    }

    /// Everything at the user level runs unasked — a sandboxed shell and
    /// writes included — and so do browser tools; what only an admin may
    /// do asks for a ticket, with a request that names this exact call.
    #[test]
    fn the_user_level_runs_unasked_and_more_needs_a_ticket() {
        let (_d, _env, store) = scratch_store();
        for tool in ["web_extract", "terminal", "write_file", "browser_navigate"] {
            match decide_now(&store, &ask(tool, json!({}))) {
                Decision::Run(Grant::Unasked(p)) => assert_eq!(p.role, Role::User, "{tool}"),
                d => panic!("{tool}: {d:?}"),
            }
        }
        match decide_now(&store, &ask("cronjob", json!({"action": "list"}))) {
            Decision::NeedsTicket { request, .. } => {
                let r = crate::ticket::TicketRequest::decode(&request).unwrap();
                assert_eq!((r.origin.as_str(), r.target.as_str()), ("node-a", "node-b"));
                assert_eq!((r.tool.as_str(), r.grant.as_str()), ("cronjob", "owner"));
            }
            d => panic!("cronjob: {d:?}"),
        }
    }

    /// A call made in a scope that goes the way other people's go says
    /// so on the wire, and the field stays off the wire otherwise (an
    /// older node is sent exactly what it was always sent).
    #[tokio::test]
    async fn a_call_says_when_it_goes_the_way_others_calls_go() {
        let owner = tool_ask("s-owner", "web_search", json!({}), None, None);
        assert!(!owner.others);
        assert!(!encode_ask(&owner).unwrap().contains("\"others\""));
        let chosen = crate::egress::scope("s-owner", true, async {
            tool_ask("s-owner", "web_search", json!({}), None, None)
        })
        .await;
        assert!(chosen.others, "the owner's /egress tor");
        let back = decode_ask(&encode_ask(&chosen).unwrap()).unwrap();
        assert!(back.others);
    }

    /// The guest browser (PLAN-challenge-relay.md) arrives from the
    /// session's node for a user's challenge: it runs unasked for a
    /// caller with `web` — a user, who since D3 has `browser` too, run
    /// in their own box — and not for a guest, who has neither.
    #[test]
    fn the_guest_browser_runs_for_a_user_and_not_for_a_guest() {
        let (_d, _env, store) = scratch_store();
        let mut a = ask(crate::browsertools::GUEST_OP, json!({"action": "look"}));
        a.principal = serde_json::to_string(&Principal::known("irc:a", Role::User)).ok();
        match decide_now(&store, &a) {
            Decision::Run(Grant::Unasked(p)) => {
                assert!(p.has(Cap::Web) && p.has(Cap::Browser), "{p:?}")
            }
            d => panic!("a user's guest browser: {d:?}"),
        }
        a.principal = serde_json::to_string(&Principal::known("irc:x", Role::Guest)).ok();
        assert!(
            !matches!(decide_now(&store, &a), Decision::Run(_)),
            "a guest has no web"
        );
    }

    /// Host reads run unasked outside the secrets; a secret path, or a
    /// search whose root holds veles' own files or the home directory,
    /// needs a ticket.
    #[test]
    fn host_reads_stop_at_the_secrets() {
        let (home, _env) = crate::testenv::scratch_home();
        let store = crate::store::Store::open(&home.path().join("t.db")).unwrap();
        let plain = home.path().join("notes.txt");
        std::fs::write(&plain, "hello").unwrap();
        let plain = plain.display().to_string();
        assert_eq!(
            decide_now(&store, &ask("read_file", json!({"path": plain}))),
            Decision::Run(Grant::HostRead)
        );
        for (tool, path) in [
            ("read_file", "~/.ssh/id_ed25519".to_string()),
            (
                "read_file",
                crate::config::config_dir()
                    .join("config.toml")
                    .display()
                    .to_string(),
            ),
            (
                "search_files",
                crate::config::config_dir().display().to_string(),
            ),
            ("search_files", "~".to_string()),
        ] {
            assert!(
                matches!(
                    decide_now(&store, &ask(tool, json!({"path": path}))),
                    Decision::NeedsTicket { .. }
                ),
                "{tool} {path}"
            );
        }
    }

    /// A call that claims another node than the peer it came from is
    /// refused; so is a banned caller.
    #[test]
    fn a_spoofed_origin_is_refused() {
        let (_d, _env, store) = scratch_store();
        let mut a = ask("web_extract", json!({}));
        a.from_node = "node-c".into();
        assert!(matches!(decide_now(&store, &a), Decision::Refused(why) if why.contains("node-c")));
        let mut a = ask("web_extract", json!({}));
        a.principal =
            Some(serde_json::to_string(&Principal::known("irc:x", Role::Banned)).unwrap());
        assert!(matches!(decide_now(&store, &a), Decision::Refused(_)));
    }

    /// A trusted signer and its key.
    fn signer_cfg() -> (crate::ticket::SignerKey, crate::config::VelesConfig) {
        let (key, _) = crate::ticket::SignerKey::generate().unwrap();
        let mut cfg = crate::config::VelesConfig::default();
        cfg.cluster.ticket_signers.insert(
            "phone".into(),
            crate::config::TicketSignerEntry {
                key: key.public_config(),
            },
        );
        (key, cfg)
    }

    fn signed(key: &crate::ticket::SignerKey, a: &RemoteToolAsk, grant: &str) -> String {
        let req = crate::ticket::TicketRequest::new(
            "node-a",
            "node-b",
            &a.session,
            &a.tool,
            a.args.clone(),
            grant,
            600,
        )
        .unwrap();
        key.sign(&req).unwrap()
    }

    /// A ticket runs the call at its grant, once.
    #[test]
    fn a_ticket_raises_one_call_once() {
        let (_d, _env, store) = scratch_store();
        let now = chrono::Utc::now().timestamp();
        let (key, cfg) = signer_cfg();
        let mut a = ask("cronjob", json!({"action": "list"}));
        a.ticket = Some(signed(&key, &a, "owner"));
        match decide(&store, &cfg, "node-b", &caller(), &a, now) {
            Decision::Run(Grant::Ticket { role, .. }) => assert_eq!(role, Role::Owner),
            d => panic!("{d:?}"),
        }
        assert!(matches!(
            decide(&store, &cfg, "node-b", &caller(), &a, now),
            Decision::Refused(why) if why.contains("already used")
        ));
    }

    /// A remote subagent runs at the user level unless a ticket for that
    /// exact delegation raises it.
    #[test]
    fn a_remote_subagent_is_capped_unless_ticketed() {
        let (_d, _env, store) = scratch_store();
        let now = chrono::Utc::now().timestamp();
        let (key, cfg) = signer_cfg();
        let p = delegate_principal(&store, &cfg, "node-b", &caller(), None, None, "g", "c", now)
            .unwrap()
            .expect("a principal");
        assert_eq!(p.role, Role::User);
        let req = crate::ticket::TicketRequest::new(
            "node-a",
            "node-b",
            "",
            "delegate_task",
            json!({"goal": "g", "context": "c"}),
            "owner",
            600,
        )
        .unwrap();
        let t = key.sign(&req).unwrap();
        assert_eq!(
            delegate_principal(
                &store,
                &cfg,
                "node-b",
                &caller(),
                None,
                Some(&t),
                "g",
                "c",
                now
            )
            .unwrap(),
            None,
            "an owner ticket runs it as the owner surface"
        );
        let t = key.sign(&req).unwrap();
        assert!(
            delegate_principal(
                &store,
                &cfg,
                "node-b",
                &caller(),
                None,
                Some(&t),
                "other",
                "c",
                now
            )
            .is_err(),
            "a ticket for another goal"
        );
    }

    /// An unasked call runs under a session of the caller's with the
    /// capped principal; a host read reads the host.
    #[tokio::test]
    async fn execute_runs_the_decided_tool_as_decided() {
        let (home, _env) = crate::testenv::scratch_home();
        let store = Arc::new(Mutex::new(
            crate::store::Store::open(&home.path().join("t.db")).unwrap(),
        ));
        let f = home.path().join("hello.txt");
        std::fs::write(&f, "remote hello").unwrap();
        let a = ask(
            "read_file",
            json!({"path": f.display().to_string(), "sandbox": true}),
        );
        let v = execute(
            store.clone(),
            &caller(),
            &a,
            &Grant::HostRead,
            crate::providers::CancelToken::new(),
        )
        .await;
        assert_eq!(v["status"], "ok", "{v}");
        assert!(
            v["output"].as_str().unwrap().contains("remote hello"),
            "{v}"
        );

        let p = capped(&Principal::owner(), &caller());
        let a = ask("todo", json!({"action": "list"}));
        let v = execute(
            store.clone(),
            &caller(),
            &a,
            &Grant::Unasked(p.clone()),
            crate::providers::CancelToken::new(),
        )
        .await;
        assert_eq!(v["status"], "ok", "{v}");
        let sid = "peer-node-a-user-s1";
        assert_eq!(
            crate::acl::session_principal(sid).as_deref(),
            Some(&p),
            "the call ran under the caller's own capped session"
        );
        let v = execute(
            store,
            &caller(),
            &ask("no_such_tool", json!({})),
            &Grant::HostRead,
            crate::providers::CancelToken::new(),
        )
        .await;
        assert_eq!(v["status"], "error");
    }

    /// A file a tool downloaded travels beside the reply's text — a
    /// small one inside the reply — and a large one's parts are served
    /// to a caller that may use what web_extract uses, and to no other.
    #[tokio::test]
    async fn downloads_ride_the_reply_and_parts_are_served() {
        use base64::Engine as _;
        let (home, _env) = crate::testenv::scratch_home();
        let store = Arc::new(Mutex::new(
            crate::store::Store::open(&home.path().join("t.db")).unwrap(),
        ));
        let meta = crate::downloads::Meta {
            name: "a.pdf".into(),
            mime: "application/pdf".into(),
            url: "https://example.org/a.pdf".into(),
        };
        let small = crate::downloads::stage(b"%PDF-1.4 small", &meta).unwrap();
        let owner = Grant::Ticket {
            role: Role::Owner,
            signer: "s".into(),
            id: "t".into(),
        };
        let echo = ask(
            "terminal",
            json!({"command": format!("echo '{}'", crate::downloads::line(&small)), "host": true}),
        );
        let v = execute(
            store.clone(),
            &caller(),
            &echo,
            &owner,
            crate::providers::CancelToken::new(),
        )
        .await;
        assert_eq!(v["status"], "ok", "{v}");
        assert_eq!(v["files"][0]["token"], small.as_str(), "{v}");
        assert!(
            v["files"][0]["b64"].is_string(),
            "a small file rides inline: {v}"
        );

        let big =
            crate::downloads::stage(&vec![7u8; crate::downloads::PART_BYTES + 1], &meta).unwrap();
        let part = |i: usize| ask(crate::downloads::PART_OP, json!({"token": big, "index": i}));
        let user = capped(&Principal::owner(), &caller());
        for (i, len) in [(0, crate::downloads::PART_BYTES), (1, 1)] {
            let v = execute(
                store.clone(),
                &caller(),
                &part(i),
                &Grant::Unasked(user.clone()),
                crate::providers::CancelToken::new(),
            )
            .await;
            assert_eq!(v["status"], "ok", "{v}");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(v["part"].as_str().unwrap())
                .unwrap();
            assert_eq!(bytes.len(), len);
        }

        let cfg = crate::config::VelesConfig::default();
        let now = chrono::Utc::now().timestamp();
        let st = store.lock().unwrap();
        assert!(
            matches!(
                decide(&st, &cfg, "node-b", &caller(), &part(0), now),
                Decision::Run(_)
            ),
            "a caller with the web capability pulls"
        );
        let mut no_web = Principal::owner();
        no_web.role = Role::User;
        no_web.caps = Default::default();
        let mut refused = part(0);
        refused.principal = Some(serde_json::to_string(&no_web).unwrap());
        assert!(
            !matches!(
                decide(&st, &cfg, "node-b", &caller(), &refused, now),
                Decision::Run(_)
            ),
            "a caller without it does not"
        );
    }
}
