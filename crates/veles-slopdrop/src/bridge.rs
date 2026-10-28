//! The slopdrop bridge — evaluating Tcl in somebody else's interpreter.
//!
//! WHY
//!
//! veles' own `tcl` is `sandbox::execute` on a temp file: safe, and
//! stateless. Every proc a channel defines dies with the process, so the
//! channel can never build anything. slopdrop is the complement — one
//! persistent git-backed interpreter with years of accumulated procs and
//! a rollback command — and the two have historically meant two bots in
//! one channel. USER, 2026-09-11: *"Especially useful if I don't want to
//! connect two full bots to a channel."*
//!
//! So: veles keeps the channel, slopdrop runs headless, and `tcl` goes
//! over HTTP.
//!
//! # Who the call is made as
//!
//! slopdrop has its own privilege split (`tcl` vs `tclAdmin`) and veles
//! has its own ACL, and the bridge must not flatten either into the
//! other. USER chose the mapping: *mirror the caller's veles role*. A
//! caller veles resolves as OWNER is offered as an admin request; every
//! other caller — including a trusted one — is an ordinary request. The
//! decision is [`veles_core::tcleval::full_interp_allowed`], the same function
//! the local path uses to decide between the safe interp and a real
//! `tclsh`, so the two back ends answer one question.
//!
//! It is offered, not asserted: the admin token is used only when one is
//! configured, and slopdrop re-checks the request against its OWN
//! `privileged_users` regardless. Two gates, neither trusting the other,
//! which is the only arrangement that survives one of them being wrong.
//!
//! The caller's subject rides along as `user`, so slopdrop's git history
//! records the person in the channel rather than recording every commit
//! as "the bridge".

use serde_json::Value;

/// The channel context one evaluation runs in.
///
/// WHY THIS EXISTS AT ALL
///
/// slopdrop's Tcl surface is not pure: a large part of what a channel
/// has built over the years reads the CURRENT conversation out of
/// interpreter globals that slopdrop's own IRC connection set before
/// each eval — `::nick`, `::channel`, `::mask`, `::network`, and the
/// `::slopdrop_channel_members` array behind `chanlist`. Its shipped
/// `tcl/utils.tcl` wraps them as `[nick]`, `[names]`, `[name]` and
/// `[hostmask]`; `tcl/timtom.tcl` builds `channel_nicks`,
/// `nick_count` and `random_other_nick` on top of `chanlist`.
///
/// Measured against the real `#coven` state on 2026-09-11, not
/// guessed: of 512 stored procs, 18 call `[nick]`, 9 call `[name]`,
/// 5 call `[names]`, and 2 read the channel log. Those are the procs
/// that go quiet the moment slopdrop stops being the bot in the room.
///
/// Headless, the only process that still knows any of this is the one
/// holding the connection — so veles sends it, per evaluation, and
/// slopdrop sets the same globals its IRC frontend used to.
///
/// Empty fields are OMITTED from the request rather than sent blank:
/// an older slopdrop ignores what it does not know, and a blank
/// `::channel` is worse than an absent one — it makes `[names]` answer
/// confidently with nothing.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Context {
    /// The speaker's nick — `[nick]`.
    pub nick: String,
    /// `ident@host`. slopdrop builds `nick!ident@host` from this and its
    /// own `nick`, which is why it is NOT the full mask.
    pub mask: String,
    /// The channel, or the querying nick for a DM — the eggdrop
    /// convention slopdrop's `::channel` follows.
    pub channel: String,
    /// The network's config name. `chanlist` keys members as
    /// `network:#channel`, so without this the roster is filed where
    /// nothing looks for it.
    pub network: String,
    /// The channel topic. slopdrop has never had one — there is no
    /// `::topic` anywhere in its tree — so this is new state it can
    /// only get from here.
    pub topic: String,
    /// The roster, bare nicks with the `@`/`+` prefixes stripped:
    /// `chanlist` has always returned names, not modes. `None` means no
    /// real channel was identified (or a DM); `Some([])` is a channel
    /// whose roster is empty or unavailable, and clears the old chanlist.
    pub members: Option<Vec<String>>,
    /// The Bluesky post the line came from and its thread
    /// (`{post, thread}`, the atproto adapter's `SenderContext::post`):
    /// slopdrop's `$::bsky::post` and `$::bsky::thread`. `None` off
    /// Bluesky.
    pub bsky: Option<Value>,
}

impl Context {
    /// Nothing known — a REPL, a cron job, any surface with no room
    /// behind it. Sends no context fields at all, which is what an
    /// evaluation with no conversation should do.
    pub fn none() -> Self {
        Self::default()
    }

    fn is_empty(&self) -> bool {
        self.nick.is_empty()
            && self.mask.is_empty()
            && self.channel.is_empty()
            && self.network.is_empty()
            && self.topic.is_empty()
            && self.members.is_none()
            && self.bsky.is_none()
    }
}

/// Build the context from what the adapter observed, filling the roster
/// and topic from the IRC tap's live per-network state.
///
/// The tap is the right source rather than a second copy: it already
/// holds `NetworkState.rosters` and `.topics` for every connection, it
/// is what the context block a model reads is built from, and it is
/// keyed casefolded exactly like the lookup here.
pub fn context_from(sender: Option<&veles_core::acl::SenderContext>) -> Context {
    let Some(s) = sender else {
        return Context::none();
    };
    // The atproto room: no channel/mask on the wire, so the IRC-derived
    // fields below would all be empty and slopdrop would answer `[name]`,
    // `[names]` and `[channel]` out of stale state. The room is the
    // THREAD: nick = the caller's handle, channel = "bluesky", the roster
    // = the thread's registered participants (`[names]`; `[name]` picks
    // one at random, slopdrop-side). `mask` carries the handle as the
    // nick, matching how the adapter fills SenderContext.
    if s.platform.as_deref() == Some("atproto") {
        // The handle rides in `handle` (display only), never in `mask`.
        let nick = s.handle.clone().unwrap_or_default();
        let mut members = s
            .chat_id
            .as_deref()
            .map(veles_core::rooms::thread_room)
            .unwrap_or_default();
        if !nick.is_empty() && !members.contains(&nick) {
            members.push(nick.clone());
        }
        return Context {
            nick,
            mask: String::new(),
            channel: "bluesky".to_string(),
            network: "atproto".to_string(),
            topic: String::new(),
            members: Some(members),
            bsky: s.post.clone(),
        };
    }
    // `mask` on the wire is `nick!ident@host`; slopdrop wants the two
    // halves separately, because it reassembles them to match its own
    // `privileged_users` patterns.
    let (nick, mask) = match s.mask.as_deref() {
        Some(m) => match m.split_once('!') {
            Some((n, rest)) => (n.to_string(), rest.to_string()),
            None => (m.to_string(), String::new()),
        },
        None => (String::new(), String::new()),
    };
    // A query has no channel, and eggdrop's answer — the one slopdrop's
    // procs were written against — is the other party's nick.
    let channel = s
        .channel
        .clone()
        .or_else(|| if s.direct { Some(nick.clone()) } else { None })
        .unwrap_or_default();
    let network = s.network.clone().unwrap_or_default();
    let mut ctx = Context {
        nick,
        mask,
        channel,
        network,
        ..Context::default()
    };
    if ctx.channel.is_empty() || ctx.network.is_empty() {
        return ctx;
    }
    let want_net = ctx.network.to_lowercase();
    let want_chan = ctx.channel.to_lowercase();
    // The channel is real even while its roster is empty or has not
    // arrived. A DM is the exception: `channel` then names the other
    // person, and their own nick is not a channel roster.
    if !s.direct {
        ctx.members = Some(Vec::new());
    }
    for st in veles_core::irc_tap::states() {
        if st.network.to_lowercase() != want_net {
            continue;
        }
        if let Some((_, topic)) = st
            .topics
            .iter()
            .find(|(k, _)| k.to_lowercase() == want_chan)
        {
            ctx.topic = topic.trim().to_string();
        }
        if let Some((_, roster)) = st
            .rosters
            .iter()
            .find(|(k, _)| k.to_lowercase() == want_chan)
        {
            ctx.members = Some(roster.iter().map(|n| bare_nick(n)).collect());
        }
        break;
    }
    ctx
}
/// A roster entry reduced to the bare nick: `@bob` → `bob`, and
/// `+bob!~u@host` → `bob`.
///
/// TWO decorations, from two different sources, and the second was
/// missed. The membership prefix (`@`, `+`, …) is always there;
/// `!user@host` is there only when the server ACKed the IRCv3
/// `userhost-in-names` cap, which veles requests — so the roster looks
/// different per NETWORK. synirc ACKs it and IRC4Fun does not, which is
/// exactly how it showed up. USER, 2026-09-12:
///
/// ```text
/// ~lewrker | tcl name        (#coven, IRC4Fun)
///   jackma | Psy-Q
///  ~xenia | tcl name        (#flashsupport, synirc)
///   jackma | demon!~demon@synIRC-23349F1F.chat
/// ```
///
///   *"Why does it dump entire hostmaska on one network"*
///
/// `chanlist` has always answered with NAMES, so everything built on it
/// inherited the bug: `[name]`, `[names]`, and any stored proc doing
/// `lsearch [names] $who` — which finds nothing when the roster arrives
/// decorated, silently, on one network only.
///
/// The roster itself keeps the decorations on purpose: the desktop
/// client renders op and voice from them. Stripping belongs here, at
/// the point where it becomes somebody else's `chanlist`.
///
/// `split_once` rather than `trim_end_matches`: a nick cannot contain
/// `!`, so the FIRST one starts the mask, and `bob!a@b@c` is `bob`.
fn bare_nick(entry: &str) -> String {
    let no_prefix = entry.trim_start_matches(['@', '+', '%', '~', '&', '!']);
    match no_prefix.split_once('!') {
        Some((nick, _)) => nick.to_string(),
        None => no_prefix.to_string(),
    }
}

/// What the bridge is configured to do, resolved once per call.
struct Route {
    url: String,
    token: Option<String>,
    timeout: std::time::Duration,
}

/// Is the bridge configured at all?
pub fn enabled(cfg: &crate::settings::SlopdropSection) -> bool {
    cfg.enabled && cfg.url.as_deref().is_some_and(|u| !u.trim().is_empty())
}

/// Is a persistent interpreter configured at all?
///
/// The config-loading twin of `enabled`, for the callers that have to
/// decide whether a line is theirs BEFORE they have a config in hand —
/// chiefly the IRC adapter's bare-`tcl` arm.
///
/// Cheap: the settings snapshot, re-read only when config.toml's mtime
/// moved.
pub fn configured() -> bool {
    crate::settings::settings().is_enabled()
}

fn route(cfg: &crate::settings::SlopdropSection, admin: bool) -> Result<Route, String> {
    let url = cfg
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or("[slopdrop] url is not set")?
        .trim_end_matches('/')
        .to_string();
    // An admin call with no admin token falls back to the ordinary one
    // rather than failing: the caller asked to evaluate, and evaluating
    // with less privilege is a better answer than refusing. The reply
    // says so, because a silent demotion is the thing the transcript
    // rules exist to prevent.
    let key = if admin {
        cfg.admin_token_env.as_deref().or(cfg.token_env.as_deref())
    } else {
        cfg.token_env.as_deref()
    };
    let token = key
        .map(veles_core::authn::env_or_dotenv)
        .filter(|t| !t.is_empty());
    Ok(Route {
        url,
        token,
        timeout: std::time::Duration::from_millis(cfg.timeout_ms.unwrap_or(45_000)),
    })
}

/// Would an admin request actually be made as one? False when no admin
/// token is configured, so the caller can be told.
pub fn admin_token_available(cfg: &crate::settings::SlopdropSection) -> bool {
    cfg.admin_token_env
        .as_deref()
        .map(veles_core::authn::env_or_dotenv)
        .is_some_and(|t| !t.is_empty())
}

/// One evaluation. `caller` is the veles subject, recorded by slopdrop
/// as the git author; `admin` asks for its privileged path; `ctx` is the
/// channel the line came from, which slopdrop turns back into the
/// globals its own IRC connection used to set (see [`Context`]).
pub async fn eval(
    cfg: &crate::settings::SlopdropSection,
    code: &str,
    caller: &str,
    admin: bool,
    owner: bool,
    ctx: &Context,
) -> Result<EvalReply, String> {
    let r = route(cfg, admin)?;
    let client = reqwest::Client::builder()
        .timeout(r.timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let mut body = serde_json::json!({"code": code, "user": caller});
    // The owner's own line: its `http` requests leave from slopdrop's
    // host directly, everyone else's through slopdrop's `[tcl]
    // http_proxy` (Tor on the VPS). Sent only when true, so an older
    // slopdrop is sent what it always was.
    if owner {
        body["owner"] = Value::Bool(true);
    }
    // Only what we actually know. A blank `::channel` would make
    // `[names]` answer "nothing is here" with confidence, where an
    // absent one leaves slopdrop's last value alone.
    if !ctx.is_empty() {
        let obj = body.as_object_mut().expect("json! built an object");
        for (k, v) in [
            ("nick", &ctx.nick),
            ("mask", &ctx.mask),
            ("channel", &ctx.channel),
            ("network", &ctx.network),
            ("topic", &ctx.topic),
        ] {
            if !v.is_empty() {
                obj.insert(k.to_string(), Value::String(v.clone()));
            }
        }
        // A real room with no roster is DIFFERENT from no room at all.
        // slopdrop retains chanlist between calls: omitting an empty
        // roster leaves the previous channel's members there. `None`
        // (no room, or a DM) keeps the compatibility omission; `Some`
        // always carries the roster, including an empty one.
        if let Some(members) = &ctx.members {
            obj.insert(
                "members".to_string(),
                Value::Array(members.iter().cloned().map(Value::String).collect()),
            );
        }
    }
    // The Bluesky post, and for it a read grant (`bsky::query`) that
    // lives only as long as this evaluation: revoked the moment the
    // answer is back, whatever it was.
    let mut grant = None;
    if let Some(b) = &ctx.bsky {
        let mut b = b.clone();
        if let Some((token, q)) = crate::bsky::grant_read(cfg) {
            b["query"] = q;
            grant = Some(token);
        }
        body["bsky"] = b;
    }
    let out = send_and_parse(client.post(format!("{}/api/eval", r.url)).json(&body), &r).await;
    if let Some(t) = grant {
        crate::bsky::revoke(&t);
    }
    out
}

/// `tcl more` — the next page of the LAST answer, from the cache
/// slopdrop already holds.
///
/// A separate function because it is a separate endpoint, and that is
/// the whole bug it fixes. slopdrop's own IRC frontend intercepted the
/// exact word `more` before Tcl ever saw it (`tcl_plugin.rs`); over the
/// bridge nothing did, so `tcl more` was evaluated — and `#coven` has a
/// stored proc of that name taking an argument, so the channel got
/// `wrong # args: should be "more block"` rather than its own output.
/// The remainder was sitting in slopdrop's cache the whole time.
///
/// The ROOM has to ride along: the cache is keyed `"{channel}:{user}"`,
/// so asking without the channel the eval named looks in a slot nobody
/// filled. (slopdrop `f811ae5` is what gave `/api/more` a channel
/// parameter to be told.)
pub async fn more(
    cfg: &crate::settings::SlopdropSection,
    caller: &str,
    ctx: &Context,
) -> Result<EvalReply, String> {
    // Never the admin route: `more` reads back what an ordinary eval
    // already produced, and asking for it is not a privileged act.
    let r = route(cfg, false)?;
    let client = reqwest::Client::builder()
        .timeout(r.timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let mut query: Vec<(&str, String)> = vec![("user", caller.to_string())];
    if !ctx.channel.is_empty() {
        query.push(("channel", ctx.channel.clone()));
    }
    send_and_parse(client.get(format!("{}/api/more", r.url)).query(&query), &r).await
}

/// Send a built request, and turn what comes back into an `EvalReply`.
///
/// Shared by `eval` and `more` so the token header, the 401 wording and
/// the response shape have ONE implementation: two copies would let the
/// pagination path drift into reporting errors differently from the
/// path everyone reads.
async fn send_and_parse(req: reqwest::RequestBuilder, r: &Route) -> Result<EvalReply, String> {
    let mut req = req;
    if let Some(t) = &r.token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("slopdrop unreachable at {}: {e}", r.url))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        // 401 is the one worth naming: it means the token is missing or
        // wrong, which looks exactly like "the bot is broken" from a
        // channel and exactly like a typo from the operator's side.
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(
                "slopdrop refused the bridge token (HTTP 401) — check [slopdrop].token_env \
                        against slopdrop's [[web.tokens]]"
                    .to_string(),
            );
        }
        return Err(format!("slopdrop: HTTP {status}: {body}"));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("slopdrop sent something that is not json: {e}"))?;
    Ok(EvalReply {
        output: json["output"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        is_error: json["is_error"].as_bool().unwrap_or(false),
        more_available: json["more_available"].as_bool().unwrap_or(false),
        commit: json["commit_info"]["changes_summary"]
            .as_str()
            .map(str::to_string),
        invoked_hash: json["invoked_hash"].as_str().map(str::to_string),
        bsky: json.get("bsky").filter(|b| b.is_array()).cloned(),
    })
}

/// Generic GET of one slopdrop JSON endpoint (the mirror's reads).
pub async fn get_json(
    cfg: &crate::settings::SlopdropSection,
    path: &str,
) -> Result<serde_json::Value, String> {
    let r = route(cfg, false)?;
    let client = reqwest::Client::builder()
        .timeout(r.timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.get(format!("{}/{}", r.url, path.trim_start_matches('/')));
    if let Some(t) = &r.token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("slopdrop unreachable at {}: {e}", r.url))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err("slopdrop refused the bridge token (HTTP 401)".into());
        }
        return Err(format!("slopdrop: HTTP {status}: {body}"));
    }
    resp.json()
        .await
        .map_err(|e| format!("slopdrop sent something that is not json: {e}"))
}

/// Generic POST of one JSON body to a slopdrop endpoint (the mirror's
/// write).
pub async fn post_json(
    cfg: &crate::settings::SlopdropSection,
    path: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let r = route(cfg, false)?;
    let client = reqwest::Client::builder()
        .timeout(r.timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client
        .post(format!("{}/{}", r.url, path.trim_start_matches('/')))
        .json(&body);
    if let Some(t) = &r.token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("slopdrop unreachable at {}: {e}", r.url))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err("slopdrop refused the bridge token (HTTP 401)".into());
        }
        return Err(format!("slopdrop: HTTP {status}: {body}"));
    }
    resp.json()
        .await
        .map_err(|e| format!("slopdrop sent something that is not json: {e}"))
}

/// What came back.
#[derive(Debug)]
pub struct EvalReply {
    pub output: String,
    pub is_error: bool,
    /// slopdrop paginates; there is more behind `/api/more`.
    pub more_available: bool,
    /// What the evaluation changed in the git-backed state, when it
    /// changed something. Shown because a `tcl` line that silently
    /// rewrote a shared proc is exactly the kind of invisible mechanism
    /// the transcript rules are about.
    pub commit: Option<String>,
    /// The content hash of the stored proc the line named, when slopdrop
    /// reports one (slopdrop ≥ this change). The public-surface safety
    /// layer classifies by this hash; `None` (ad-hoc code, an old
    /// slopdrop) is unknown and fails CLOSED.
    pub invoked_hash: Option<String>,
    /// What a Bluesky evaluation recorded for its reply (`bsky::link`,
    /// `bsky::tag`, …): a JSON array of flat records, unvalidated —
    /// `crate::bsky::validate_spec` decides what of it the post carries.
    pub bsky: Option<Value>,
}

// ── the trigger/timer bridge (plan item 1, slopdrop 89422ac) ────────

/// The `/api/event` body. `answered_urls` — the links in the line the
/// bot already previewed (a Bluesky trigger row) — is sent only when
/// there are some: slopdrop's linkresolver skips them, and an older
/// slopdrop that does not know the field is sent exactly what it was
/// always sent.
fn event_body(
    network: &str,
    event: &str,
    args: &[String],
    answered: &[String],
    log: bool,
    bsky: Option<&serde_json::Value>,
    owner: bool,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "event": event,
        "network": network,
        "args": args,
        "log": log,
    });
    // The owner's own event (a link the owner pasted): its handlers'
    // `http` requests leave directly, as for `eval`; sent only when true.
    if owner {
        body["owner"] = serde_json::Value::Bool(true);
    }
    if !answered.is_empty() {
        body["answered_urls"] = serde_json::json!(answered);
    }
    // A Bluesky event's detail, `$::bsky::event` for its handlers.
    if let Some(b) = bsky {
        body["bsky"] = b.clone();
    }
    body
}

/// One trigger handler's answer, and the stored proc that wrote it (when
/// slopdrop names one: the public surface classifies by its hash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventReply {
    pub channel: String,
    pub message: String,
    pub proc_name: Option<String>,
    pub invoked_hash: Option<String>,
}

/// Forward one IRC event. `network` is SLOPDROP's spelling (synirc,
/// irc4fun) — the `disabled` array and the timer keys are keyed on it,
/// and sending ours silently matches nothing.
#[allow(clippy::too_many_arguments)]
pub async fn event(
    cfg: &crate::settings::SlopdropSection,
    network: &str,
    event: &str,
    args: &[String],
    answered: &[String],
    log: bool,
    bsky: Option<&serde_json::Value>,
    owner: bool,
) -> Result<Vec<EventReply>, String> {
    let r = route(cfg, false)?;
    let client = reqwest::Client::builder()
        .timeout(r.timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let body = event_body(network, event, args, answered, log, bsky, owner);
    let mut req = client.post(format!("{}/api/event", r.url)).json(&body);
    if let Some(t) = &r.token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("slopdrop unreachable at {}: {e}", r.url))?;
    let status = resp.status();
    if !status.is_success() {
        let b = resp.text().await.unwrap_or_default();
        return Err(format!("slopdrop: HTTP {status}: {b}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("slopdrop sent something that is not json: {e}"))?;
    if json["is_error"].as_bool().unwrap_or(false) {
        return Err("slopdrop event dispatch failed".into());
    }
    let mut out = Vec::new();
    if let Some(arr) = json["responses"].as_array() {
        for item in arr {
            if let (Some(c), Some(m)) = (item["channel"].as_str(), item["message"].as_str()) {
                out.push(EventReply {
                    channel: c.to_string(),
                    message: m.to_string(),
                    proc_name: item["proc"].as_str().map(str::to_string),
                    invoked_hash: item["invoked_hash"].as_str().map(str::to_string),
                });
            }
        }
    }
    Ok(out)
}

/// Fire due timers on the other side, getting the `{channel, message}`
/// pairs back. The channel may carry a `network:` prefix in slopdrop's
/// spelling; mapping it to a connection is ours.
pub async fn timers_check(
    cfg: &crate::settings::SlopdropSection,
) -> Result<Vec<(String, String)>, String> {
    let r = route(cfg, false)?;
    let client = reqwest::Client::builder()
        .timeout(r.timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.post(format!("{}/api/timers/check", r.url));
    if let Some(t) = &r.token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("slopdrop unreachable at {}: {e}", r.url))?;
    let status = resp.status();
    if !status.is_success() {
        let b = resp.text().await.unwrap_or_default();
        return Err(format!("slopdrop: HTTP {status}: {b}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("slopdrop sent something that is not json: {e}"))?;
    let mut out = Vec::new();
    if let Some(arr) = json["fired"].as_array() {
        for item in arr {
            if let (Some(c), Some(m)) = (item["channel"].as_str(), item["message"].as_str()) {
                out.push((c.to_string(), m.to_string()));
            }
        }
    }
    Ok(out)
}

/// The live trigger bindings and disable rules — the truth the forward
/// mask is DERIVED from, never restated.
pub async fn triggers_state(
    cfg: &crate::settings::SlopdropSection,
) -> Result<crate::triggers::SlopdropTriggers, String> {
    let r = route(cfg, false)?;
    let client = reqwest::Client::builder()
        .timeout(r.timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.get(format!("{}/api/triggers", r.url));
    if let Some(t) = &r.token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("slopdrop unreachable at {}: {e}", r.url))?;
    let status = resp.status();
    if !status.is_success() {
        let b = resp.text().await.unwrap_or_default();
        return Err(format!("slopdrop: HTTP {status}: {b}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("slopdrop sent something that is not json: {e}"))?;
    let mut bindings = Vec::new();
    if let Some(arr) = json["bindings"].as_array() {
        for b in arr {
            if let (Some(e), Some(p), Some(pr)) = (
                b["event"].as_str(),
                b["pattern"].as_str(),
                b["proc"].as_str(),
            ) {
                bindings.push((e.to_string(), p.to_string(), pr.to_string()));
            }
        }
    }
    let mut disabled = Vec::new();
    if let Some(arr) = json["disabled"].as_array() {
        for d in arr {
            let key = d["key"].as_str().unwrap_or_default().to_string();
            let procs = d["procs"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if !key.is_empty() {
                disabled.push((key, procs));
            }
        }
    }
    Ok(crate::triggers::SlopdropTriggers { bindings, disabled })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The links the bot answered ride along only when there are some:
    /// the body an older slopdrop gets for every other line is unchanged.
    #[test]
    fn answered_urls_are_sent_only_when_there_are_some() {
        let args = vec!["n".to_string(), "m".into(), "#c".into(), "t".into()];
        let plain = event_body("synirc", "TEXT", &args, &[], true, None, false);
        assert!(plain.get("answered_urls").is_none(), "{plain}");
        assert_eq!(plain["args"], serde_json::json!(args));
        let url = "https://impro.social/profile/a.b/post/1".to_string();
        let with = event_body(
            "synirc",
            "TEXT",
            &args,
            std::slice::from_ref(&url),
            true,
            None,
            false,
        );
        assert_eq!(with["answered_urls"], serde_json::json!([url]));
        assert_eq!(with["network"], "synirc");
        assert!(plain.get("owner").is_none() && with.get("owner").is_none());
    }

    /// The owner's own event (a link the owner pasted) says so, so
    /// slopdrop's link resolver fetches it directly; anyone else's says
    /// nothing, and goes through slopdrop's proxy.
    #[test]
    fn the_owners_event_says_so() {
        let args = vec!["n".to_string(), "m".into(), "#c".into(), "t".into()];
        let owners = event_body("synirc", "TEXT", &args, &[], true, None, true);
        assert_eq!(owners["owner"], serde_json::json!(true));
    }

    /// A roster entry is decorated by TWO different things, and only
    /// one of them is the same on every network.
    ///
    /// The membership prefix is always there. `!user@host` appears only
    /// where the server ACKed `userhost-in-names` — which veles
    /// requests, so synirc has it and IRC4Fun does not, and `tcl name`
    /// answered `Psy-Q` in one channel and
    /// `demon!~demon@synIRC-23349F1F.chat` in the other.
    #[test]
    fn a_roster_entry_reduces_to_the_bare_nick_on_either_network() {
        // IRC4Fun: no userhost-in-names.
        assert_eq!(bare_nick("Psy-Q"), "Psy-Q");
        assert_eq!(bare_nick("@ChanServ"), "ChanServ");
        assert_eq!(bare_nick("+gid"), "gid");
        // synirc: the same roster, with the cap.
        assert_eq!(
            bare_nick("demon!~demon@synIRC-23349F1F.chat"),
            "demon",
            "a proc doing `lsearch [names] $who` finds nothing otherwise"
        );
        assert_eq!(bare_nick("@mdl!~mdl@synIRC-F9CEF472.org"), "mdl");
        // A `@` inside the HOST must not be mistaken for the start of
        // one: the first `!` ends the nick, and nothing after it counts.
        assert_eq!(bare_nick("bob!a@b@c"), "bob");
        // Several prefixes at once is a real roster shape (op + voice).
        assert_eq!(bare_nick("@+bob!~u@example.com"), "bob");
    }
    use crate::settings::SlopdropSection;

    fn cfg_for(url: &str) -> SlopdropSection {
        SlopdropSection {
            enabled: true,
            url: Some(url.to_string()),
            token_env: Some("TEST_SLOPDROP_TOKEN".into()),
            admin_token_env: None,
            timeout_ms: Some(5_000),
            output_classifier: Vec::new(),
            output_classifier_tox: None,
            output_classifier_id: None,
            bsky_read_url: None,
        }
    }

    #[tokio::test]
    async fn an_eval_carries_the_caller_and_the_token_and_returns_the_output() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        std::env::set_var("TEST_SLOPDROP_TOKEN", "s3cret");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = veles_core::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let ok =
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            sock.write_all(ok.as_bytes()).await.unwrap();
            sock.write_all(
                br#"{"output":["42","and more"],"is_error":false,"more_available":true,
                     "commit_info":{"changes_summary":"+proc: answer"}}"#,
            )
            .await
            .unwrap();
            req
        });
        let c = cfg_for(&format!("http://127.0.0.1:{port}"));
        let reply = eval(
            &c,
            "expr {6 * 7}",
            "irc:someone",
            false,
            false,
            &Context::none(),
        )
        .await
        .expect("the bridge must answer");
        assert_eq!(reply.output, "42\nand more");
        assert!(reply.more_available);
        assert_eq!(reply.commit.as_deref(), Some("+proc: answer"));

        let req = srv.await.unwrap();
        assert!(req.starts_with("POST /api/eval"), "{req}");
        assert!(
            req.contains("Bearer s3cret"),
            "the bridge token must ride: {req}"
        );
        assert!(
            req.contains("\"user\":\"irc:someone\""),
            "the CALLER is the git author, not the bridge: {req}"
        );
        std::env::remove_var("TEST_SLOPDROP_TOKEN");
    }

    /// `more` is a GET to a DIFFERENT endpoint, and it carries the room.
    ///
    /// Both halves matter and each was a bug. Sending it to `/api/eval`
    /// evaluates it — `#coven` has a stored proc named `more` that takes
    /// an argument, so the channel got `wrong # args` instead of its own
    /// output. And slopdrop keys the pagination cache `"{channel}:{user}"`,
    /// so asking without the channel the eval named reads a slot nobody
    /// filled and answers "No cached output" with total confidence.
    #[tokio::test]
    async fn more_is_a_get_to_the_pagination_endpoint_and_names_the_room() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        std::env::set_var("TEST_SLOPDROP_TOKEN", "s3cret");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = veles_core::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let ok =
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            sock.write_all(ok.as_bytes()).await.unwrap();
            sock.write_all(
                br#"{"output":["line11","line12"],"is_error":false,"more_available":false}"#,
            )
            .await
            .unwrap();
            req
        });
        let c = cfg_for(&format!("http://127.0.0.1:{port}"));
        let ctx = Context {
            channel: "#coven".to_string(),
            ..Context::default()
        };
        let reply = more(&c, "user:wrath", &ctx)
            .await
            .expect("the bridge answers");
        assert_eq!(reply.output, "line11\nline12");

        let req = srv.await.unwrap();
        assert!(
            req.starts_with("GET /api/more"),
            "`more` is its own endpoint, not code: {req}"
        );
        assert!(
            req.contains("channel=%23coven"),
            "without the room the cache key cannot match: {req}"
        );
        assert!(
            req.contains("user=user%3Awrath") || req.contains("user=user:wrath"),
            "the cache is keyed on the caller too: {req}"
        );
        assert!(req.contains("Bearer s3cret"), "still authenticated: {req}");
        std::env::remove_var("TEST_SLOPDROP_TOKEN");
    }

    /// A 401 is the failure an operator will actually hit, and "HTTP
    /// 401" alone sends them reading the wrong logs.
    #[tokio::test]
    async fn a_refused_token_says_which_knob_is_wrong() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = veles_core::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            sock.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        });
        let c = cfg_for(&format!("http://127.0.0.1:{port}"));
        let e = eval(&c, "x", "irc:someone", false, false, &Context::none())
            .await
            .unwrap_err();
        assert!(e.contains("token_env"), "names the knob to fix: {e}");
        assert!(e.contains("web.tokens"), "and the other end of it: {e}");
        srv.await.unwrap();
    }

    /// USER, 2026-09-11: *"make sure we handle the user list and topic
    /// and all of that for slopdrop's internal variables — they're used
    /// in many procs"*.
    ///
    /// slopdrop's own IRC connection set `::nick`, `::channel`, `::mask`,
    /// `::network` and the `chanlist` array before every eval, and its
    /// shipped `tcl/utils.tcl` publishes them as `[nick]`, `[names]`,
    /// `[name]` and `[hostmask]`. Headless it has no connection, so
    /// whatever this request does not carry is state that no longer
    /// exists anywhere.
    ///
    /// The roster and topic come from the tap rather than from a second
    /// copy kept here — the same `NetworkState` the context block a model
    /// reads is built from.
    #[tokio::test]
    async fn the_bridge_sends_the_room_the_line_was_typed_in() {
        let mut topics = std::collections::HashMap::new();
        topics.insert("#coven".to_string(), "the topic".to_string());
        let mut rosters = std::collections::HashMap::new();
        rosters.insert(
            "#coven".to_string(),
            vec![
                "@Psy-Q".to_string(),
                "+gid".to_string(),
                "cromega".to_string(),
            ],
        );
        veles_core::irc_tap::put_state(veles_core::irc_tap::NetworkState {
            network: "slopctxnet".to_string(),
            presence: "bot".to_string(),
            nick: "jackma".to_string(),
            connected: true,
            channels: vec!["#coven".to_string()],
            topics,
            rosters,
            aways: std::collections::HashMap::new(),
        });
        let sender = veles_core::acl::SenderContext {
            mask: Some("Demotion!~Demotion@shared-bouncer.example.net".to_string()),
            network: Some("slopctxnet".to_string()),
            channel: Some("#coven".to_string()),
            platform: Some("irc".to_string()),
            chat_id: Some("#coven".to_string()),
            direct: false,
            handle: None,
            post: None,
        };
        let ctx = context_from(Some(&sender));

        assert_eq!(ctx.nick, "Demotion", "[nick] is the speaker, not the mask");
        assert_eq!(
            ctx.mask, "~Demotion@shared-bouncer.example.net",
            "slopdrop rebuilds `nick!ident@host` itself, so the halves go \
             separately or its privileged_users patterns never match"
        );
        assert_eq!(ctx.channel, "#coven");
        assert_eq!(ctx.network, "slopctxnet");
        assert_eq!(ctx.topic, "the topic");
        assert_eq!(
            ctx.members,
            Some(vec!["Psy-Q".into(), "gid".into(), "cromega".into()]),
            "`chanlist` has always answered with NAMES: a proc doing \
             `lsearch [names] $who` finds nothing if the ops arrive \
             decorated with @ and +"
        );

        // …and all of it reaches the wire, which is the half a struct
        // test cannot see.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = veles_core::rt::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let ok =
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            sock.write_all(ok.as_bytes()).await.unwrap();
            sock.write_all(br#"{"output":["ok"],"is_error":false}"#)
                .await
                .unwrap();
            req
        });
        let c = cfg_for(&format!("http://127.0.0.1:{port}"));
        eval(&c, "names", "irc:demotion", false, false, &ctx)
            .await
            .expect("the bridge must answer");
        let req = srv.await.unwrap();
        for needle in [
            "\"nick\":\"Demotion\"",
            "\"mask\":\"~Demotion@shared-bouncer.example.net\"",
            "\"channel\":\"#coven\"",
            "\"network\":\"slopctxnet\"",
            "\"topic\":\"the topic\"",
            "\"members\":[\"Psy-Q\",\"gid\",\"cromega\"]",
        ] {
            assert!(req.contains(needle), "the body carries {needle}: {req}");
        }
        assert!(
            !req.contains("\"owner\""),
            "someone else's line goes through slopdrop's proxy: {req}"
        );
    }

    /// A surface with no room behind it — the REPL, a cron job — sends
    /// no room, rather than sending an empty one.
    ///
    /// The distinction is not pedantry: slopdrop keeps its globals
    /// between evaluations, so an absent `channel` leaves the last real
    /// one alone, while a blank one makes `[names]` answer "nobody is
    /// here" with total confidence.
    #[tokio::test]
    async fn an_empty_channel_roster_clears_slopdrops_previous_chanlist() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = veles_core::rt::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            sock.write_all(br#"{"output":["ok"],"is_error":false}"#)
                .await
                .unwrap();
            req
        });
        let c = cfg_for(&format!("http://127.0.0.1:{port}"));
        let sender = veles_core::acl::SenderContext {
            mask: Some("someone!~u@example.net".into()),
            network: Some("slopctx-empty-room".into()),
            channel: Some("#empty".into()),
            platform: Some("irc".into()),
            chat_id: Some("#empty".into()),
            direct: false,
            handle: None,
            post: None,
        };
        let ctx = context_from(Some(&sender));
        assert_eq!(ctx.members, Some(Vec::new()));
        eval(&c, "names", "irc:someone", false, false, &ctx)
            .await
            .expect("empty roster evaluation answers");
        let req = srv.await.unwrap();
        assert!(
            req.contains("\"members\":[]"),
            "empty roster must clear: {req}"
        );
    }

    #[tokio::test]
    async fn an_evaluation_with_no_room_sends_no_room() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = veles_core::rt::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            sock.write_all(br#"{"output":["ok"],"is_error":false}"#)
                .await
                .unwrap();
            req
        });
        let c = cfg_for(&format!("http://127.0.0.1:{port}"));
        eval(&c, "expr 1", "local:owner", false, true, &Context::none())
            .await
            .expect("the bridge must answer");
        let req = srv.await.unwrap();
        for absent in ["\"channel\"", "\"nick\"", "\"members\"", "\"topic\""] {
            assert!(
                !req.contains(absent),
                "nothing known means nothing sent — {absent} is in: {req}"
            );
        }
        assert!(req.contains("\"user\":\"local:owner\""), "{req}");
        // The owner's own line: slopdrop's http leaves directly for it.
        assert!(req.contains("\"owner\":true"), "{req}");
    }

    /// A query has no channel, and the answer slopdrop's procs were
    /// written against is eggdrop's: the other party's nick IS the
    /// channel. Without this a DM makes `[channel]` empty and every
    /// proc that keys on it silently works on nothing.
    #[test]
    fn a_query_is_its_own_channel() {
        let sender = veles_core::acl::SenderContext {
            mask: Some("wrath!~w@example.org".to_string()),
            network: Some("slopctxnet2".to_string()),
            channel: None,
            platform: Some("irc".to_string()),
            chat_id: Some("wrath".to_string()),
            direct: true,
            handle: None,
            post: None,
        };
        let ctx = context_from(Some(&sender));
        assert_eq!(ctx.channel, "wrath");
        assert_eq!(ctx.nick, "wrath");
    }

    #[test]
    fn an_atproto_sender_gets_the_thread_as_its_room() {
        let root = "at://did:plc:x/app.bsky.feed.post/root1";
        veles_core::rooms::register_thread_room(
            root,
            &["alice.example".to_string(), "bob.example".to_string()],
        );
        let sender = veles_core::acl::SenderContext {
            mask: None,
            network: Some("atproto".to_string()),
            channel: None,
            platform: Some("atproto".to_string()),
            chat_id: Some(root.to_string()),
            direct: false,
            handle: Some("carol.example".to_string()),
            post: None,
        };
        let ctx = context_from(Some(&sender));
        assert_eq!(ctx.nick, "carol.example", "the caller is [nick]");
        assert_eq!(ctx.channel, "bluesky", "[channel] is the platform");
        assert_eq!(ctx.network, "atproto");
        let members = ctx.members.as_deref().unwrap_or_default();
        assert!(
            members.contains(&"alice.example".to_string())
                && members.contains(&"bob.example".to_string())
                && members.contains(&"carol.example".to_string()),
            "[names] = thread participants + the caller: {:?}",
            ctx.members
        );
    }

    #[test]
    fn the_thread_room_dedupes_and_a_first_mention_still_has_the_caller() {
        let root = "at://did:plc:x/app.bsky.feed.post/root2";
        veles_core::rooms::register_thread_room(root, &["alice.example".to_string()]);
        veles_core::rooms::register_thread_room(
            root,
            &["alice.example".to_string(), "d.example".to_string()],
        );
        let room = veles_core::rooms::thread_room(root);
        assert_eq!(
            room.iter().filter(|h| *h == "alice.example").count(),
            1,
            "no duplicates"
        );
        assert!(room.contains(&"d.example".to_string()));
        // A first mention (nothing registered) still resolves to the caller.
        let sender = veles_core::acl::SenderContext {
            mask: None,
            network: Some("atproto".to_string()),
            channel: None,
            platform: Some("atproto".to_string()),
            chat_id: Some("at://did:plc:x/app.bsky.feed.post/never-registered".to_string()),
            direct: false,
            handle: Some("newbie.example".to_string()),
            post: None,
        };
        let ctx = context_from(Some(&sender));
        assert_eq!(ctx.members, Some(vec!["newbie.example".to_string()]));
    }
}
