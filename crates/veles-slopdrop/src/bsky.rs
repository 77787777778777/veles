//! Bluesky in the bridge (USER 2026-10-10: "special tcl variables and
//! procs to manipulate posts and facets at atproto/bsky level", with
//! Bluesky event triggers and AppView reads).
//!
//! Three things, all on the PUBLIC surface's side of the bridge:
//!
//! - **The reply spec.** A `/tcl` line from a Bluesky post can record
//!   links, tags, mentions, a quote or card, images and languages
//!   (slopdrop's `tcl/bsky.tcl`). [`validate_spec`] decides what of it the
//!   post may carry: mentions only of the caller and the thread's own
//!   participants (a proc must not notify strangers from the bot's
//!   account), a few tags, one quote or card, images only on the owner's
//!   `/tcladmin` path. The result rides the reply's
//!   `meta["bsky.spec"]` to the atproto plugin.
//! - **Read grants.** `bsky::query` reads through the bot's own AppView
//!   session. Each Bluesky evaluation gets a token good for a few reads
//!   ([`grant_read`]), handed to slopdrop with the post and revoked when
//!   the answer is back; the reads arrive on this module's OPEN route
//!   (`/api/plugins/slopdrop/open/bsky-read`), which checks the token,
//!   allows only the public reads in [`READS`], and asks `appview.query`.
//!   slopdrop never holds the desktop's owner bearer, and Tcl never sees
//!   the token (slopdrop keeps it in Rust).
//! - **Events.** The atproto plugin's notifications — likes, reposts,
//!   follows, quotes, replies, mentions — arrive as the `bsky.event`
//!   service ([`event_service`]): the trigger rows scoped to `atproto`
//!   decide, slopdrop's trigger engine answers, and each answer is shaped
//!   by the same public-reply policy as `/tcl`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use veles_core::moduleroutes::RouteResponse;
use veles_types::plugin::{
    method::APPVIEW_QUERY, AppviewQueryParams, BskyEventParams, BskyEventReplies, BskyEventReply,
};

use crate::settings::SlopdropSection;

// ── read grants ─────────────────────────────────────────────────────

/// Reads one evaluation may make.
pub const READS_PER_EVAL: u32 = 5;
/// A grant outlives its evaluation only by accident (a revoke that never
/// ran); it dies by this regardless. slopdrop's eval timeout is 30 s.
const GRANT_TTL: Duration = Duration::from_secs(90);

struct Grant {
    until: Instant,
    remaining: u32,
}

fn grants() -> &'static Mutex<HashMap<String, Grant>> {
    static G: OnceLock<Mutex<HashMap<String, Grant>>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(HashMap::new()))
}

fn new_token() -> Option<String> {
    veles_core::challenge::random_hex(24)
}

/// A read grant for one Bluesky evaluation: the token, and the `query`
/// object slopdrop's `bsky::query` uses. `None` when this node does not
/// serve reads (`[slopdrop] bsky_read_url` unset): the procs then fail
/// with the reason, and nothing else changes.
pub fn grant_read(cfg: &SlopdropSection) -> Option<(String, Value)> {
    let base = cfg
        .bsky_read_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())?
        .trim_end_matches('/')
        .to_string();
    let token = new_token()?;
    if let Ok(mut g) = grants().lock() {
        let now = Instant::now();
        g.retain(|_, v| v.until > now);
        g.insert(
            token.clone(),
            Grant {
                until: now + GRANT_TTL,
                remaining: READS_PER_EVAL,
            },
        );
    }
    let url = format!("{base}/api/plugins/slopdrop/open/bsky-read");
    Some((
        token.clone(),
        json!({"url": url, "token": token, "max": READS_PER_EVAL}),
    ))
}

/// End a grant (its evaluation answered).
pub fn revoke(token: &str) {
    if let Ok(mut g) = grants().lock() {
        g.remove(token);
    }
}

/// Spend one read of a live grant.
fn spend(token: &str) -> Result<(), &'static str> {
    let mut g = grants().lock().map_err(|_| "grants poisoned")?;
    let now = Instant::now();
    match g.get_mut(token) {
        Some(grant) if grant.until > now && grant.remaining > 0 => {
            grant.remaining -= 1;
            Ok(())
        }
        Some(grant) if grant.until > now => Err("this evaluation has used all its reads"),
        _ => Err("no live read grant (reads are only for an evaluation from a Bluesky post)"),
    }
}

/// The reads `bsky::query` may make: public profile, post, feed and
/// graph views and search. Not notifications, preferences or chat — the
/// bot's own private views stay the bot's.
pub const READS: &[&str] = &[
    "app.bsky.actor.getProfile",
    "app.bsky.actor.getProfiles",
    "app.bsky.actor.searchActors",
    "app.bsky.feed.getPosts",
    "app.bsky.feed.getPostThread",
    "app.bsky.feed.getAuthorFeed",
    "app.bsky.feed.searchPosts",
    "app.bsky.feed.getLikes",
    "app.bsky.feed.getRepostedBy",
    "app.bsky.feed.getQuotes",
    "app.bsky.graph.getFollowers",
    "app.bsky.graph.getFollows",
];

/// `POST /open/bsky-read {method, params: [[k, v], …]}` with the grant
/// as the bearer: one read, answered `{"result": …}` or `{"error": …}`.
pub async fn read_route(bearer: Option<&str>, body: &Value) -> RouteResponse {
    let deny = |status: u16, why: &str| RouteResponse::status(status, json!({"error": why}));
    let Some(token) = bearer else {
        return deny(401, "no read grant");
    };
    if let Err(why) = spend(token) {
        return deny(403, why);
    }
    let method = body
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !READS.contains(&method) {
        return deny(
            403,
            &format!("`{method}` is not one of the reads bsky::query may make"),
        );
    }
    let mut params = Vec::new();
    for pair in body
        .get("params")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(k), Some(v)) = (
            pair.get(0).and_then(Value::as_str),
            pair.get(1).and_then(Value::as_str),
        ) else {
            return deny(400, "params must be [key, value] pairs of strings");
        };
        if params.len() >= 25 || k.len() > 64 || v.len() > 1024 {
            return deny(400, "too many or too long parameters");
        }
        params.push((k.to_string(), v.to_string()));
    }
    let p = AppviewQueryParams {
        method: method.to_string(),
        params,
    };
    let Ok(p) = serde_json::to_value(p) else {
        return deny(500, "params");
    };
    match veles_core::services::call(APPVIEW_QUERY, p).await {
        Ok(v) => RouteResponse::ok(json!({"result": v.get("value").cloned().unwrap_or(v)})),
        Err(e) => deny(502, &e),
    }
}

// ── the reply spec ──────────────────────────────────────────────────

/// Who and what a reply may carry.
#[derive(Default)]
pub struct Allowed<'a> {
    /// The caller's handle (they may always be mentioned).
    pub caller: Option<&'a str>,
    /// The thread's registered participants.
    pub participants: &'a [String],
    /// Images only on the owner's `/tcladmin` path.
    pub images: bool,
    /// The configured owner asked (`public::asked_by_owner`): a quote may
    /// name any post.
    pub owner: bool,
    /// The caller's DID: anyone may quote their own posts.
    pub caller_did: Option<&'a str>,
    /// The at-uris anyone else may quote: the post that asked and the
    /// thread it sits in ([`quotable`]).
    pub quotable: &'a [String],
}

/// The posts a non-owner's reply may quote: the post that asked, its
/// parent and root, and every post of the thread above it — the
/// conversation the caller is already in, never a stranger's post from
/// elsewhere (a quote notifies its author, and the bot's account is the
/// one quoting). From the atproto post context (`{post, thread}`,
/// `SenderContext::post`); `#cid` fragments dropped.
pub fn quotable(ctx: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |u: Option<&str>| {
        if let Some(u) = u.map(strip_fragment).filter(|u| u.starts_with("at://")) {
            if !out.iter().any(|o| o == u) {
                out.push(u.to_string());
            }
        }
    };
    let Some(ctx) = ctx else { return out };
    if let Some(post) = ctx.get("post") {
        add(post.get("uri").and_then(Value::as_str));
        if let Some(reply) = post.get("reply") {
            add(reply.get("parent").and_then(Value::as_str));
            add(reply.get("root").and_then(Value::as_str));
        }
    }
    for p in ctx
        .get("thread")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        add(p.get("uri").and_then(Value::as_str));
    }
    out
}

/// The text of the post `uri` names, when it is the post that asked or
/// one of its thread (the atproto post context, `{post, thread}`).
pub fn thread_text(ctx: Option<&Value>, uri: &str) -> Option<String> {
    let want = strip_fragment(uri);
    let ctx = ctx?;
    std::iter::once(ctx.get("post"))
        .flatten()
        .chain(
            ctx.get("thread")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
        .find(|p| p.get("uri").and_then(Value::as_str).map(strip_fragment) == Some(want))
        .and_then(|p| p.get("text").and_then(Value::as_str))
        .map(str::to_string)
}

fn strip_fragment(u: &str) -> &str {
    u.split('#').next().unwrap_or(u)
}

/// The repo (a DID, or a handle in a hand-written at-uri) a post's at-uri
/// names.
pub fn uri_repo(uri: &str) -> Option<&str> {
    uri.strip_prefix("at://")?
        .split('/')
        .next()
        .filter(|r| !r.is_empty())
}

/// May this caller's reply quote `uri`? The owner: any post, by at-uri or
/// bsky.app link. Anyone else: an at-uri of their own or one of the
/// thread's posts ([`quotable`]); a bsky.app link names a post by a
/// handle that is resolved later, so it is the owner's only.
fn may_quote(uri: &str, allowed: &Allowed<'_>) -> bool {
    if allowed.owner {
        return uri.starts_with("at://") || uri.starts_with("https://bsky.app/");
    }
    let uri = strip_fragment(uri);
    if !uri.starts_with("at://") || !uri.contains("/app.bsky.feed.post/") {
        return false;
    }
    allowed.quotable.iter().any(|q| q == uri)
        || allowed
            .caller_did
            .is_some_and(|did| did.starts_with("did:") && uri_repo(uri) == Some(did))
}

const MAX_LINKS: usize = 20;
const MAX_TAGS: usize = 8;
const MAX_MENTIONS: usize = 10;
const MAX_IMAGES: usize = 4;
const MAX_LANGS: usize = 3;

fn is_http(u: &str) -> bool {
    (u.starts_with("https://") || u.starts_with("http://"))
        && u.len() <= 2000
        && !u.chars().any(char::is_whitespace)
}

fn is_lang(code: &str) -> bool {
    let mut parts = code.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    (2..=3).contains(&first.len())
        && first.chars().all(|c| c.is_ascii_alphabetic())
        && parts.all(|p| (2..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// What of a recorded spec the post may carry, and the notes for what it
/// may not (said in the reply, so a proc author sees why). Records come
/// back as flat string objects in the order they were recorded.
/// The `@handle`s a reply's spec may carry, as the output check reads
/// them: each mention [`validate_spec`] keeps, by its HANDLE (never the
/// text a proc recorded beside it, which could be anything).
pub fn allowed_mentions(raw: &Value, allowed: &Allowed<'_>) -> Vec<String> {
    let (spec, _) = validate_spec(raw, allowed);
    spec.as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("mention"))
        .filter_map(|r| r.get("handle").and_then(Value::as_str))
        .map(|h| format!("@{h}"))
        .collect()
}

pub fn validate_spec(raw: &Value, allowed: &Allowed<'_>) -> (Option<Value>, Vec<String>) {
    let lc = |s: &str| s.trim().trim_start_matches('@').to_ascii_lowercase();
    let may_mention = |h: &str| {
        let h = lc(h);
        allowed.caller.map(lc).is_some_and(|c| c == h)
            || allowed.participants.iter().any(|p| lc(p) == h)
    };
    let mut out = Vec::new();
    let mut notes = Vec::new();
    let (mut links, mut tags, mut mentions, mut images, mut langs) = (0, 0, 0, 0, 0);
    let mut embed_taken = false;
    let mut images_refused = false;
    let mut quote_refused = false;
    for r in raw.as_array().into_iter().flatten() {
        let f = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        match f("kind").as_str() {
            "link" => {
                let (text, uri) = (f("text"), f("uri"));
                if links < MAX_LINKS && !text.is_empty() && text.len() <= 300 && is_http(&uri) {
                    links += 1;
                    out.push(json!({"kind": "link", "text": text, "uri": uri}));
                }
            }
            "tag" => {
                let tag = f("tag").trim_start_matches('#').to_string();
                if tags < MAX_TAGS
                    && (1..=64).contains(&tag.chars().count())
                    && !tag.chars().any(char::is_whitespace)
                {
                    tags += 1;
                    out.push(json!({"kind": "tag", "text": format!("#{tag}"), "tag": tag}));
                }
            }
            "mention" => {
                let handle = lc(&f("handle"));
                if mentions < MAX_MENTIONS && !handle.is_empty() && may_mention(&handle) {
                    mentions += 1;
                    out.push(json!({"kind": "mention", "text": f("text"), "handle": handle}));
                }
            }
            "quote" if !embed_taken => {
                let uri = f("uri");
                if may_quote(&uri, allowed) {
                    embed_taken = true;
                    out.push(json!({"kind": "quote", "uri": strip_fragment(&uri)}));
                } else if !uri.is_empty() {
                    quote_refused = true;
                }
            }
            "card" if !embed_taken => {
                let uri = f("uri");
                if is_http(&uri) {
                    embed_taken = true;
                    let title: String = f("title").chars().take(300).collect();
                    let desc: String = f("description").chars().take(1000).collect();
                    out.push(
                        json!({"kind": "card", "uri": uri, "title": title, "description": desc}),
                    );
                }
            }
            "image" => {
                if !allowed.images {
                    images_refused = true;
                } else {
                    let uri = f("uri");
                    if images < MAX_IMAGES && uri.starts_with("https://") && is_http(&uri) {
                        images += 1;
                        let alt: String = f("alt").chars().take(1000).collect();
                        out.push(json!({"kind": "image", "uri": uri, "alt": alt}));
                    }
                }
            }
            "lang" => {
                let code = f("code");
                if langs < MAX_LANGS && is_lang(&code) {
                    langs += 1;
                    out.push(json!({"kind": "lang", "code": code}));
                }
            }
            _ => {}
        }
    }
    if images_refused {
        notes.push("(images attach only from the owner's /tcladmin)".to_string());
    }
    if quote_refused {
        notes.push("(a quote may name only a post of this thread, or your own)".to_string());
    }
    ((!out.is_empty()).then_some(Value::Array(out)), notes)
}

// ── events: the atproto plugin's notifications ──────────────────────

/// The gateway's store, parked by `start` (trigger rows live there).
fn parked() -> &'static Mutex<Option<Arc<Mutex<veles_core::store::Store>>>> {
    static S: OnceLock<Mutex<Option<Arc<Mutex<veles_core::store::Store>>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

pub fn park_store(store: Option<Arc<Mutex<veles_core::store::Store>>>) {
    if let Ok(mut s) = parked().lock() {
        *s = store;
    }
}

/// The gateway's bus, parked by `start`: a trigger's answer is posted
/// through it, so the plugin's delivery receipt reaches the ledger
/// exactly as a `/tcl` reply's does (the owner can delete it by link).
fn bus() -> &'static Mutex<Option<veles_core::bus::GatewayBus>> {
    static B: OnceLock<Mutex<Option<veles_core::bus::GatewayBus>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(None))
}

pub fn park_bus(b: veles_core::bus::GatewayBus) {
    if let Ok(mut s) = bus().lock() {
        *s = Some(b);
    }
}

fn brakes() -> &'static Mutex<veles_core::triggers::Brakes> {
    static B: OnceLock<Mutex<veles_core::triggers::Brakes>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(veles_core::triggers::Brakes::default()))
}

/// The six Bluesky events slopdrop binds.
pub const EVENTS: &[&str] = &["LIKE", "REPOST", "FOLLOW", "QUOTE", "REPLY", "MENTION"];

/// The room every Bluesky event happens in, as slopdrop's bindings and
/// the trigger rows name it.
pub const NETWORK: &str = "atproto";
pub const CHANNEL: &str = "bluesky";

/// The handler arguments slopdrop's `triggers bind` documents for each
/// Bluesky event (`tcl/triggers.tcl`).
pub fn event_args(p: &BskyEventParams) -> Vec<String> {
    let who = if p.handle.is_empty() {
        p.did.clone()
    } else {
        p.handle.clone()
    };
    match p.event.as_str() {
        "LIKE" | "REPOST" => vec![who, p.did.clone(), CHANNEL.into(), p.subject.clone()],
        "FOLLOW" => vec![who, p.did.clone(), CHANNEL.into()],
        _ => vec![
            who,
            p.did.clone(),
            CHANNEL.into(),
            p.uri.clone(),
            p.text.clone(),
        ],
    }
}

/// Where an answer to this event is posted: under the bot's post a like
/// or repost was about, under the post that quoted, replied or mentioned.
/// A follow has no post to answer under.
pub fn reply_target(p: &BskyEventParams) -> Option<String> {
    match p.event.as_str() {
        "LIKE" | "REPOST" => Some(p.subject.clone()).filter(|s| !s.is_empty()),
        "FOLLOW" => None,
        _ => Some(p.uri.clone()).filter(|s| !s.is_empty()),
    }
}

/// `bsky.event` — see the module header. Only the atproto plugin may call
/// it: a script or another plugin pushing events into the interpreter
/// would be a way to make the bot post publicly.
pub async fn event_service(p: Value, cx: veles_core::services::CallCx) -> Result<Value, String> {
    use veles_core::triggers::{Event, Outcome};
    let name = veles_types::plugin::method::BSKY_EVENT;
    if cx.caller != "process:atproto" {
        return Err(format!(
            "{name}: only the atproto plugin forwards Bluesky events (caller {})",
            cx.caller
        ));
    }
    let p: BskyEventParams = serde_json::from_value(p).map_err(|e| format!("{name}: {e}"))?;
    if !EVENTS.contains(&p.event.as_str()) {
        return Err(format!("{name}: unknown event {}", p.event));
    }
    let cfg = veles_core::config::live();
    let empty = || serde_json::to_value(BskyEventReplies::default()).map_err(|e| e.to_string());
    if !cfg.triggers.enabled {
        return empty();
    }
    let s = crate::settings::settings();
    if !s.is_enabled() {
        return empty();
    }
    let Some(store) = parked().lock().ok().and_then(|g| g.clone()) else {
        return Err(format!("{name}: no store on this node"));
    };
    let now = chrono::Utc::now().timestamp();
    let ev = Event {
        platform: NETWORK,
        network: NETWORK,
        channel: CHANNEL,
        event: &p.event,
        nick: if p.handle.is_empty() {
            &p.did
        } else {
            &p.handle
        },
        mask: &p.did,
        text: &p.text,
        role: None,
    };
    let decisions = {
        let st = store.lock().map_err(|_| "store poisoned")?;
        let br = brakes().lock().map_err(|_| "brakes poisoned")?;
        veles_core::triggers::evaluate(&st, &br, &ev, now)
    };
    let detail = json!({
        "kind": p.event, "did": p.did, "handle": p.handle, "uri": p.uri,
        "cid": p.cid, "subject": p.subject, "text": p.text,
    });
    let mut replies = Vec::new();
    for (t, outcome) in decisions {
        let (status, why) = match outcome {
            Ok(Outcome::Slopdrop(_)) => {
                let claimed = brakes()
                    .lock()
                    .map_err(|_| "brakes poisoned".to_string())
                    .and_then(|mut b| {
                        b.bridge_claim(NETWORK, CHANNEL, now, cfg.triggers.max_per_minute)
                    });
                if let Err(why) = claimed {
                    ("refused".to_string(), Some(why))
                } else {
                    if let Ok(mut b) = brakes().lock() {
                        b.commit(&t, CHANNEL, now);
                    }
                    match crate::bridge::event(
                        &s,
                        NETWORK,
                        &p.event,
                        &event_args(&p),
                        &[],
                        false,
                        Some(&detail),
                        false,
                    )
                    .await
                    {
                        Ok(answers) => {
                            let n = answers.len();
                            for a in answers {
                                if let Some(r) = shape_answer(&cfg, &p, &a).await {
                                    post(&p, &r);
                                    replies.push(r);
                                }
                            }
                            ("fired".to_string(), Some(format!("{n} answer(s)")))
                        }
                        Err(e) => ("error".to_string(), Some(e)),
                    }
                }
            }
            Ok(_) => (
                "unsupported".to_string(),
                Some("only slopdrop rows act on Bluesky events".into()),
            ),
            Err(why) => ("refused".to_string(), Some(why)),
        };
        if let Ok(st) = store.lock() {
            let _ = st.trigger_record_fire(t.id, CHANNEL, &status, why.as_deref());
        }
    }
    serde_json::to_value(BskyEventReplies { replies }).map_err(|e| e.to_string())
}

/// Post a reply on Bluesky through the gateway, threaded under
/// `reply_to` in `chat`, carrying `meta` (its ledger entry, its spec): the
/// plugin's delivery receipt reaches the ledger as a `/tcl` reply's does.
/// A trigger's answer and an approved picture go out this way.
pub fn send_reply(
    chat: &str,
    reply_to: &str,
    text: String,
    meta: std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    let b = bus()
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .ok_or("no bus is parked to post through")?;
    let mut o =
        veles_types::bus::OutboundMessage::plain(chat, text, veles_types::bus::MessageKind::Reply);
    o.reply_to = Some(reply_to.to_string());
    o.meta = meta;
    if b.send_outbound_to(NETWORK, None, o) {
        Ok(())
    } else {
        Err("no Bluesky adapter runs in this process".to_string())
    }
}

/// Post one shaped answer through the gateway, threaded under its target.
fn post(p: &BskyEventParams, r: &BskyEventReply) {
    let chat = if p.thread.is_empty() {
        r.reply_to.clone()
    } else {
        p.thread.clone()
    };
    if let Err(e) = send_reply(&chat, &r.reply_to, r.text.clone(), r.meta.clone()) {
        veles_core::module::log(
            tracing::Level::WARN,
            &format!("bsky: a trigger answered, but it was not posted: {e}"),
        );
    }
}

/// One handler answer, through the public-reply policy (the same ledger,
/// output check and per-proc verdicts as `/tcl`). A FOLLOW's answer is
/// never posted: there is no post to put it under, and a top-level post
/// for every new follower is spam from the bot's account.
async fn shape_answer(
    cfg: &veles_core::config::VelesConfig,
    p: &BskyEventParams,
    a: &crate::bridge::EventReply,
) -> Option<BskyEventReply> {
    use veles_core::tcleval::PublicReplyRequest;
    let Some(target) = reply_target(p) else {
        veles_core::module::log(
            tracing::Level::INFO,
            &format!(
                "bsky: a {} trigger answered ({} chars) and was not posted: no post to answer under",
                p.event,
                a.message.len()
            ),
        );
        return None;
    };
    if a.message.trim().is_empty() {
        return None;
    }
    let msg = veles_types::bus::InboundMessage {
        platform: NETWORK.into(),
        route: None,
        chat_id: if p.thread.is_empty() {
            target.clone()
        } else {
            p.thread.clone()
        },
        msg_id: Some(target.clone()),
        dedup_id: None,
        sender: if p.handle.is_empty() {
            p.did.clone()
        } else {
            p.handle.clone()
        },
        sender_id: Some(format!("atproto:{}", p.did)),
        sender_ctx: Default::default(),
        text: p.text.clone(),
        context: None,
    };
    let code = format!(
        "[{} trigger] {}",
        p.event,
        a.proc_name.as_deref().unwrap_or("(unknown proc)")
    );
    // Never as the owner: the event's author did not write the proc
    // that answered, even when the owner's like fired it.
    let shaped = crate::public::Policy
        .shape_with(
            PublicReplyRequest {
                cfg,
                msg: &msg,
                code: &code,
                output: &a.message,
                invoked_hash: a.invoked_hash.as_deref(),
                spec: None,
            },
            false,
        )
        .await;
    Some(BskyEventReply {
        text: shaped.text,
        meta: shaped.meta,
        reply_to: target,
    })
}

#[cfg(test)]
mod tests {
    /// The people gate is handed each allowed mention by its HANDLE: a
    /// proc that records a mention of the caller with any other visible
    /// text gets no word through the gate but `@<handle>`, and a mention of
    /// a stranger gets none.
    #[test]
    fn allowed_mentions_are_handles_never_recorded_text() {
        let raw = serde_json::json!([
            {"kind": "mention", "text": "evil.example", "handle": "op.example"},
            {"kind": "mention", "text": "@stranger.example", "handle": "stranger.example"},
        ]);
        let allowed = Allowed {
            caller: Some("op.example"),
            participants: &[],
            images: false,
            ..Default::default()
        };
        assert_eq!(
            allowed_mentions(&raw, &allowed),
            vec!["@op.example".to_string()]
        );
    }

    use super::*;

    fn rec(kind: &str, kv: &[(&str, &str)]) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("kind".into(), json!(kind));
        for (k, v) in kv {
            m.insert((*k).into(), json!(v));
        }
        Value::Object(m)
    }

    /// The quotable posts are the conversation the caller is in: the
    /// post that asked, its parent and root (even when the fetched slice
    /// of the thread left them out) and the thread's posts, fragments
    /// dropped, each once.
    #[test]
    fn the_quotable_posts_are_the_thread_the_caller_is_in() {
        let ctx = json!({
            "post": {
                "uri": "at://did:plc:c/app.bsky.feed.post/ask#cid1",
                "reply": {
                    "parent": "at://did:plc:p/app.bsky.feed.post/par",
                    "root": "at://did:plc:r/app.bsky.feed.post/root",
                },
            },
            "thread": [
                {"uri": "at://did:plc:r/app.bsky.feed.post/root"},
                {"uri": "at://did:plc:x/app.bsky.feed.post/mid#c"},
                {"uri": "https://bsky.app/profile/x/post/y"},
            ],
        });
        assert_eq!(
            quotable(Some(&ctx)),
            vec![
                "at://did:plc:c/app.bsky.feed.post/ask",
                "at://did:plc:p/app.bsky.feed.post/par",
                "at://did:plc:r/app.bsky.feed.post/root",
                "at://did:plc:x/app.bsky.feed.post/mid",
            ]
        );
        assert!(quotable(None).is_empty());
    }

    /// Anyone but the owner quotes only inside the conversation: a post
    /// of the thread, or one of their own. A stranger's post elsewhere,
    /// a bsky.app link (a handle resolved later) or a record that is not
    /// a post is refused, and the reply says why. The owner quotes
    /// anything.
    #[test]
    fn a_quote_names_a_post_of_the_thread_unless_the_owner_asked() {
        let thread = vec!["at://did:plc:p/app.bsky.feed.post/par".to_string()];
        let not_owner = Allowed {
            caller: Some("c.test"),
            caller_did: Some("did:plc:c"),
            quotable: &thread,
            ..Default::default()
        };
        let one = |uri: &str, allowed: &Allowed<'_>| {
            validate_spec(&Value::Array(vec![rec("quote", &[("uri", uri)])]), allowed)
        };
        for ok in [
            "at://did:plc:p/app.bsky.feed.post/par",
            "at://did:plc:p/app.bsky.feed.post/par#somecid",
            "at://did:plc:c/app.bsky.feed.post/mine",
        ] {
            let (spec, notes) = one(ok, &not_owner);
            assert_eq!(spec.expect(ok)[0]["kind"], "quote", "{ok}");
            assert!(notes.is_empty(), "{ok}: {notes:?}");
        }
        for refused in [
            "at://did:plc:stranger/app.bsky.feed.post/elsewhere",
            "https://bsky.app/profile/p.test/post/par",
            "at://did:plc:c/app.bsky.actor.profile/self",
            "at://did:plc:cc/app.bsky.feed.post/prefix-of-mine",
        ] {
            let (spec, notes) = one(refused, &not_owner);
            assert!(spec.is_none(), "{refused} is refused");
            assert_eq!(notes.len(), 1, "{refused}: the reply says why");
        }
        let owner = Allowed {
            owner: true,
            ..Default::default()
        };
        for ok in [
            "at://did:plc:stranger/app.bsky.feed.post/elsewhere",
            "https://bsky.app/profile/p.test/post/par",
        ] {
            assert!(one(ok, &owner).0.is_some(), "the owner quotes {ok}");
        }
    }

    #[test]
    fn a_spec_keeps_only_what_a_public_reply_may_carry() {
        let raw = Value::Array(vec![
            rec("link", &[("text", "docs"), ("uri", "https://d.test")]),
            rec("link", &[("text", "bad"), ("uri", "javascript:alert(1)")]),
            rec("tag", &[("text", "#tcl"), ("tag", "#tcl")]),
            rec("tag", &[("text", "#a b"), ("tag", "a b")]),
            rec(
                "mention",
                &[("text", "@Caller.Test"), ("handle", "Caller.Test")],
            ),
            rec(
                "mention",
                &[("text", "@friend.test"), ("handle", "@friend.test")],
            ),
            rec(
                "mention",
                &[("text", "@stranger.test"), ("handle", "stranger.test")],
            ),
            rec("quote", &[("uri", "at://did:plc:q/app.bsky.feed.post/1")]),
            rec("card", &[("uri", "https://c.test")]),
            rec("image", &[("uri", "https://i.test/a.png"), ("alt", "")]),
            rec("lang", &[("code", "pt-BR")]),
            rec("lang", &[("code", "not a lang")]),
            rec("nonsense", &[]),
        ]);
        let participants = vec!["friend.test".to_string()];
        let (spec, notes) = validate_spec(
            &raw,
            &Allowed {
                caller: Some("caller.test"),
                participants: &participants,
                images: false,
                owner: true,
                ..Default::default()
            },
        );
        let spec = spec.expect("something survives");
        let kinds: Vec<&str> = spec
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["link", "tag", "mention", "mention", "quote", "lang"],
            "a bad URL, a spaced tag, a stranger, a second embed, a public image and a bad lang are dropped: {spec}"
        );
        assert_eq!(spec[1]["tag"], "tcl");
        assert_eq!(spec[2]["handle"], "caller.test");
        assert_eq!(notes.len(), 1, "the refused image is said: {notes:?}");

        let (spec, notes) = validate_spec(
            &Value::Array(vec![rec(
                "image",
                &[("uri", "https://i.test/a.png"), ("alt", "a cat")],
            )]),
            &Allowed {
                caller: None,
                participants: &[],
                images: true,
                ..Default::default()
            },
        );
        assert_eq!(spec.unwrap()[0]["alt"], "a cat", "the owner's image stays");
        assert!(notes.is_empty());
    }

    #[test]
    fn a_read_grant_is_spent_and_revoked() {
        let cfg = SlopdropSection {
            bsky_read_url: Some("http://127.0.0.1:8642/".into()),
            ..Default::default()
        };
        let (token, q) = grant_read(&cfg).expect("a grant");
        assert_eq!(
            q["url"], "http://127.0.0.1:8642/api/plugins/slopdrop/open/bsky-read",
            "{q}"
        );
        assert_eq!(q["max"], READS_PER_EVAL);
        for _ in 0..READS_PER_EVAL {
            assert!(spend(&token).is_ok());
        }
        assert!(spend(&token).unwrap_err().contains("used all"));
        revoke(&token);
        assert!(spend(&token).unwrap_err().contains("no live read grant"));
        assert!(
            grant_read(&SlopdropSection::default()).is_none(),
            "no read url, no grant"
        );
    }

    #[tokio::test]
    async fn the_read_route_refuses_without_a_grant_and_outside_the_allowlist() {
        let r = read_route(None, &json!({"method": "app.bsky.actor.getProfile"})).await;
        assert_eq!(r.status, 401);
        let r = read_route(
            Some("nope"),
            &json!({"method": "app.bsky.actor.getProfile"}),
        )
        .await;
        assert_eq!(r.status, 403);
        let cfg = SlopdropSection {
            bsky_read_url: Some("http://x".into()),
            ..Default::default()
        };
        let (token, _) = grant_read(&cfg).unwrap();
        for private in [
            "app.bsky.notification.listNotifications",
            "app.bsky.notification.getUnreadCount",
            "app.bsky.actor.getPreferences",
            "chat.bsky.convo.listConvos",
        ] {
            let r = read_route(Some(&token), &json!({"method": private})).await;
            assert_eq!(r.status, 403, "{private}: {}", r.body);
        }
        revoke(&token);
    }

    #[test]
    fn each_event_gets_slopdrops_argument_shape_and_its_reply_target() {
        let p = |event: &str| BskyEventParams {
            event: event.into(),
            did: "did:plc:a".into(),
            handle: "a.test".into(),
            uri: "at://did:plc:a/app.bsky.feed.post/q".into(),
            subject: "at://bot/app.bsky.feed.post/s".into(),
            text: "hi".into(),
            ..Default::default()
        };
        assert_eq!(
            event_args(&p("LIKE")),
            [
                "a.test",
                "did:plc:a",
                "bluesky",
                "at://bot/app.bsky.feed.post/s"
            ]
        );
        assert_eq!(event_args(&p("FOLLOW")), ["a.test", "did:plc:a", "bluesky"]);
        assert_eq!(
            event_args(&p("QUOTE")),
            [
                "a.test",
                "did:plc:a",
                "bluesky",
                "at://did:plc:a/app.bsky.feed.post/q",
                "hi"
            ]
        );
        assert_eq!(
            reply_target(&p("LIKE")).as_deref(),
            Some("at://bot/app.bsky.feed.post/s")
        );
        assert_eq!(
            reply_target(&p("REPLY")).as_deref(),
            Some("at://did:plc:a/app.bsky.feed.post/q")
        );
        assert_eq!(
            reply_target(&p("FOLLOW")),
            None,
            "a follow is never answered with a post"
        );
    }

    #[tokio::test]
    async fn only_the_atproto_plugin_may_push_events() {
        let cx = veles_core::services::CallCx {
            caller: "tcl:someone".into(),
            session_id: None,
            next: None,
        };
        let err = event_service(json!({"event": "LIKE", "did": "did:plc:a"}), cx)
            .await
            .unwrap_err();
        assert!(err.contains("only the atproto plugin"), "{err}");
    }
}
