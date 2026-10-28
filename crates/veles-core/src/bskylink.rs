//! Bluesky link previews for the trigger engine, read AS THE BOT.
//!
//! A post link pasted into a channel used to reach slopdrop's
//! linkresolver, which scrapes the page's meta tags. That page is
//! rendered for a SIGNED-OUT reader, so two kinds of link came back
//! wrong: impro.social serves one generic page for every route (every
//! link read "Title: Impro"), and bsky.app replaces the text of an
//! author who limits their posts to signed-in users with a notice.
//!
//! The bot is a signed-in user. A `bluesky` trigger row reads the link
//! through the atproto plugin's `appview.query` service — the bot's own
//! PDS, proxied to the AppView — and answers with one line per link.
//! With no plugin serving that here (another node holds the atproto
//! lease, the plugin is down, the session is not up yet), the row
//! answers nothing and the link is left to slopdrop's resolver, which
//! is what answered before this existed. Only links the bot actually
//! previewed are named to slopdrop as answered, so a link the bot could
//! not read (deleted, blocked, a timeout) still gets the old answer.
//!
//! Nothing is cut (the USER, 2026-10-10: "I want the bot never to elide
//! content"), and everything a preview names is linked, so it can all be
//! explored from IRC. What the AppView hides behind a block — the post
//! itself, its parent, a quote — is read raw through the plugin's
//! `repo.get_record` (a record read has no viewer, so no block applies)
//! and said to be blocked; a hidden post that is gone is said to be
//! deleted.
//!
//! The rendering needs no network: which links a line holds, which
//! query reads each, what the answer hides ([`hidden_refs`]), and the
//! line it all becomes ([`render_thread`]). The calls are [`preview`]
//! and [`read_hidden`].

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;
use veles_types::plugin::at_uri_parts;

/// Web clients whose links follow bsky.app's routes
/// (`/profile/<actor>[/post/<rkey>]`). impro.social is the owner's.
pub const CLIENT_HOSTS: &[&str] = &["bsky.app", "impro.social"];

/// At most this many links are previewed per line — the same cap
/// slopdrop's resolver keeps, so a pasted list is not a flood.
pub const MAX_LINKS: usize = 2;

/// How long one preview may take before the link is left to slopdrop.
pub const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Post { actor: String, rkey: String },
    Profile { actor: String },
}

/// One Bluesky link found in a line: the URL as written (what slopdrop
/// will see) and what it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub target: Target,
}

impl Link {
    /// The `appview.query` that reads this link: its method and params.
    pub fn query(&self) -> (&'static str, Vec<(String, String)>) {
        match &self.target {
            Target::Post { actor, rkey } => (
                "app.bsky.feed.getPostThread",
                vec![
                    (
                        "uri".into(),
                        format!("at://{actor}/app.bsky.feed.post/{rkey}"),
                    ),
                    ("depth".into(), "0".into()),
                    // One parent: enough to say whom a reply answers.
                    ("parentHeight".into(), "1".into()),
                ],
            ),
            Target::Profile { actor } => (
                "app.bsky.actor.getProfile",
                vec![("actor".into(), actor.clone())],
            ),
        }
    }
}

/// The Bluesky links in a line, deduplicated, at most [`MAX_LINKS`].
pub fn links_in(text: &str) -> Vec<Link> {
    let mut out: Vec<Link> = Vec::new();
    for url in crate::triggers::extract_urls(text) {
        if out.len() >= MAX_LINKS {
            break;
        }
        if out.iter().any(|l| l.url == url) {
            continue;
        }
        if let Some(target) = parse(&url) {
            out.push(Link { url, target });
        }
    }
    out
}

/// What a client URL names, if it is one of [`CLIENT_HOSTS`]' post or
/// profile routes.
pub fn parse(url: &str) -> Option<Target> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if !CLIENT_HOSTS.contains(&host) {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        ["profile", actor] if valid_actor(actor) => Some(Target::Profile {
            actor: actor.to_ascii_lowercase(),
        }),
        ["profile", actor, "post", rkey] if valid_actor(actor) && valid_rkey(rkey) => {
            Some(Target::Post {
                actor: normalize_actor(actor),
                rkey: (*rkey).to_string(),
            })
        }
        _ => None,
    }
}

/// A handle (a domain name) or a DID. Anything else — `/profile/x`
/// with no dot, a path that only looks like a route — is not a link we
/// can read.
fn valid_actor(a: &str) -> bool {
    if let Some(rest) = a.strip_prefix("did:") {
        let mut it = rest.splitn(2, ':');
        let (Some(method), Some(id)) = (it.next(), it.next()) else {
            return false;
        };
        return !method.is_empty()
            && method.chars().all(|c| c.is_ascii_lowercase())
            && !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:%-".contains(c));
    }
    let labels: Vec<&str> = a.split('.').collect();
    a.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// A handle is case-insensitive and travels lowercased; a DID is
/// case-sensitive and travels as written.
fn normalize_actor(a: &str) -> String {
    if a.starts_with("did:") {
        a.to_string()
    } else {
        a.to_ascii_lowercase()
    }
}

/// A record key: 1–512 of `A-Za-z0-9._:~-`, not `.` or `..`.
fn valid_rkey(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 512
        && k != "."
        && k != ".."
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:~-".contains(c))
}

// ── rendering ─────────────────────────────────────────────────────────
//
// Nothing is cut. A post is at most 300 graphemes, and the IRC side
// splits a long line into as many PRIVMSGs as it needs (veles-irc's
// `privmsg`, at word boundaries), so a preview says everything the post
// says: the whole text, the full URL behind every shortened link, every
// image and video by its URL and alt text, and a link to every post it
// names (its parent, a quote), so each can be pasted back to read on.
// What the AppView hides behind a block is read raw (`repo.get_record`)
// and said to be blocked, and a hidden post that is gone is said to be
// deleted. Links are written `<url>`, so no client folds the `:` or `]`
// that follows into the URL.

/// Labels that mean "not safe for work" on the post or its author.
const NSFW_LABELS: &[&str] = &["porn", "sexual", "nudity", "graphic-media"];

const THREAD_VIEW_POST: &str = "app.bsky.feed.defs#threadViewPost";
const BLOCKED_POST: &str = "app.bsky.feed.defs#blockedPost";
const NOT_FOUND_POST: &str = "app.bsky.feed.defs#notFoundPost";
const VIEW_RECORD: &str = "app.bsky.embed.record#viewRecord";
const VIEW_BLOCKED: &str = "app.bsky.embed.record#viewBlocked";
const VIEW_NOT_FOUND: &str = "app.bsky.embed.record#viewNotFound";
const VIEW_DETACHED: &str = "app.bsky.embed.record#viewDetached";

/// How deep quotes inside quotes are followed (the AppView itself stops
/// hydrating below the second level).
const QUOTE_DEPTH: usize = 3;

/// One line of display text from someone else's string: whitespace runs
/// (newlines included) become one space, and every other control
/// character is dropped — IRC formatting and CTCP codes in a post must
/// not reach the channel as codes.
pub fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if c.is_control() || ('\u{200b}'..='\u{200f}').contains(&c) || c == '\u{feff}' {
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    out
}

/// A post's text with the full URL of every link facet where the text
/// shows a shortened one (clients write `example.com/some/pa...` and keep
/// the URL in the facet). A link whose text is not the URL keeps its text
/// and gets the URL after it.
pub fn expand_links(text: &str, facets: &Value) -> String {
    let mut spans: Vec<(usize, usize, &str)> = Vec::new();
    for f in facets.as_array().into_iter().flatten() {
        let (Some(start), Some(end)) = (
            f["index"]["byteStart"].as_u64(),
            f["index"]["byteEnd"].as_u64(),
        ) else {
            continue;
        };
        let (start, end) = (start as usize, end as usize);
        let uri = f["features"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|x| x["$type"].as_str() == Some("app.bsky.richtext.facet#link"))
            .and_then(|x| x["uri"].as_str());
        let Some(uri) = uri else { continue };
        if start < end
            && end <= text.len()
            && text.is_char_boundary(start)
            && text.is_char_boundary(end)
        {
            spans.push((start, end, uri));
        }
    }
    spans.sort_by_key(|s| s.0);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end, uri) in spans {
        if start < at {
            continue; // overlaps the one before: a malformed facet
        }
        out.push_str(&text[at..start]);
        let shown = &text[start..end];
        if shows(shown, uri) {
            out.push_str(uri);
        } else {
            out.push_str(shown);
            out.push_str(" <");
            out.push_str(uri);
            out.push('>');
        }
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// Is `shown` how a client writes `uri`: the URL, or it without its
/// scheme, perhaps cut short with `...`?
fn shows(shown: &str, uri: &str) -> bool {
    fn bare(s: &str) -> &str {
        s.strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
            .unwrap_or(s)
    }
    let s = shown.trim();
    let s = s
        .strip_suffix("...")
        .or_else(|| s.strip_suffix('…'))
        .unwrap_or(s);
    let s = bare(s);
    !s.is_empty() && bare(uri).starts_with(s)
}

/// A post record's text, links expanded, as one line.
fn post_text(record: &Value) -> String {
    one_line(&expand_links(
        record["text"].as_str().unwrap_or_default(),
        &record["facets"],
    ))
}

/// 1234 → "1.2K", 2_500_000 → "2.5M".
fn count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => short(n as f64 / 1_000.0, "K"),
        _ => short(n as f64 / 1_000_000.0, "M"),
    }
}

fn short(v: f64, unit: &str) -> String {
    if v >= 100.0 {
        format!("{v:.0}{unit}")
    } else {
        let s = format!("{v:.1}");
        format!("{}{unit}", s.trim_end_matches(".0"))
    }
}

/// An author's handle, or their DID when the view has no handle.
fn handle_of(author: &Value) -> String {
    let handle = one_line(author["handle"].as_str().unwrap_or_default());
    if handle.is_empty() {
        one_line(author["did"].as_str().unwrap_or("?"))
    } else {
        handle
    }
}

/// `Name (@handle)`, or `@handle` when the name is empty.
fn who(author: &Value) -> String {
    let handle = handle_of(author);
    let name = one_line(author["displayName"].as_str().unwrap_or_default());
    if name.is_empty() {
        format!("@{handle}")
    } else {
        format!("{name} (@{handle})")
    }
}

fn has_nsfw_label(v: &Value) -> bool {
    v["labels"].as_array().is_some_and(|ls| {
        ls.iter().any(|l| {
            l["val"]
                .as_str()
                .is_some_and(|val| NSFW_LABELS.contains(&val))
                && !l["neg"].as_bool().unwrap_or(false)
        })
    })
}

/// A raw record's own labels (`com.atproto.label.defs#selfLabels`).
fn has_nsfw_self_label(record: &Value) -> bool {
    record["labels"]["values"].as_array().is_some_and(|ls| {
        ls.iter().any(|l| {
            l["val"]
                .as_str()
                .is_some_and(|val| NSFW_LABELS.contains(&val))
        })
    })
}

/// A blob's CID in a raw record: `ref.$link`, or a legacy blob's `cid`.
fn blob_cid(blob: &Value) -> Option<&str> {
    blob["ref"]["$link"]
        .as_str()
        .or_else(|| blob["cid"].as_str())
        .filter(|c| !c.is_empty() && c.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// ` — alt: …` when there is alt text.
fn alt(v: &Value) -> String {
    let a = one_line(v["alt"].as_str().unwrap_or_default());
    if a.is_empty() {
        String::new()
    } else {
        format!(" — alt: {a}")
    }
}

/// `link <uri> — title: description`, as much as the card carries.
fn external_part(ext: &Value) -> String {
    let uri = one_line(ext["uri"].as_str().unwrap_or_default());
    let title = one_line(ext["title"].as_str().unwrap_or_default());
    let desc = one_line(ext["description"].as_str().unwrap_or_default());
    let mut s = String::from("link");
    if !uri.is_empty() {
        s.push_str(&format!(" <{uri}>"));
    }
    match (title.is_empty(), desc.is_empty()) {
        (false, false) => s.push_str(&format!(" — {title}: {desc}")),
        (false, true) => s.push_str(&format!(" — {title}")),
        (true, false) => s.push_str(&format!(" — {desc}")),
        (true, true) => {}
    }
    s
}

/// What the raw reads found for the posts and authors a thread view
/// hides ([`hidden_refs`] names them; [`preview`] reads them).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hidden {
    /// AT URI → the record as its author's repo holds it, `None` when it
    /// is not there any more (deleted). Absent: it could not be read.
    pub records: HashMap<String, Option<Value>>,
    /// DID → that author's profile view.
    pub profiles: HashMap<String, Value>,
}

/// What a thread answer hides that a raw read can show: records by AT
/// URI, and the authors (by DID) a hidden view names only by DID.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wants {
    pub records: BTreeSet<String>,
    pub profiles: BTreeSet<String>,
}

/// Which records and authors the thread answer hides: a blocked post,
/// parent or quote (its record, to show it, or to know it was deleted),
/// and the authors of deleted and detached ones (to say whose they were).
pub fn hidden_refs(answer: &Value) -> Wants {
    let mut w = Wants::default();
    let thread = &answer["thread"];
    match thread["$type"].as_str() {
        Some(BLOCKED_POST) => w.blocked(thread),
        Some(THREAD_VIEW_POST) => {
            let parent = &thread["parent"];
            match parent["$type"].as_str() {
                Some(BLOCKED_POST) => w.blocked(parent),
                Some(NOT_FOUND_POST) => w.author_of(parent["uri"].as_str()),
                _ => {}
            }
            w.embed(&thread["post"]["embed"], 0);
        }
        _ => {}
    }
    w
}

impl Wants {
    fn blocked(&mut self, v: &Value) {
        if let Some(uri) = v["uri"].as_str().filter(|u| at_uri_parts(u).is_some()) {
            self.records.insert(uri.to_string());
        }
        self.author_of(v["uri"].as_str());
    }

    fn author_of(&mut self, uri: Option<&str>) {
        if let Some((repo, _, _)) = uri.and_then(at_uri_parts) {
            self.profiles.insert(repo.to_string());
        }
    }

    fn embed(&mut self, view: &Value, depth: usize) {
        match view["$type"].as_str().unwrap_or_default() {
            "app.bsky.embed.record#view" => self.quote(&view["record"], depth),
            "app.bsky.embed.recordWithMedia#view" => self.quote(&view["record"]["record"], depth),
            _ => {}
        }
    }

    fn quote(&mut self, rec: &Value, depth: usize) {
        match rec["$type"].as_str().unwrap_or_default() {
            VIEW_BLOCKED => self.blocked(rec),
            VIEW_NOT_FOUND | VIEW_DETACHED => self.author_of(rec["uri"].as_str()),
            VIEW_RECORD if depth + 1 < QUOTE_DEPTH => {
                for e in rec["embeds"].as_array().into_iter().flatten() {
                    self.embed(e, depth + 1);
                }
            }
            _ => {}
        }
    }
}

/// What rendering needs besides the answer: the client the pasted link
/// came from (the links a preview writes point there, so impro.social
/// stays impro.social) and what the raw reads found.
#[derive(Debug, Clone, Copy)]
pub struct Cx<'a> {
    pub site: &'a str,
    pub hidden: &'a Hidden,
}

impl Cx<'_> {
    /// `Name (@handle)` for an author a view names only by DID.
    fn who_did(&self, did: &str) -> String {
        match self.hidden.profiles.get(did) {
            Some(p) => who(p),
            None => format!("@{}", one_line(did)),
        }
    }

    /// What to write in a link to this author: the handle the profile
    /// read found, else the DID (which every client takes as well).
    fn actor_for(&self, did: &str) -> String {
        self.hidden
            .profiles
            .get(did)
            .and_then(|p| p["handle"].as_str())
            .filter(|h| !h.is_empty() && *h != "handle.invalid")
            .map(one_line)
            .unwrap_or_else(|| one_line(did))
    }

    /// The client link to the record at `uri` (a post, feed, list or
    /// starter pack), written with `actor`.
    fn url(&self, actor: &str, uri: &str) -> Option<String> {
        let (repo, coll, rkey) = at_uri_parts(uri)?;
        let actor = if actor.is_empty() { repo } else { actor };
        let site = self.site;
        Some(match coll {
            "app.bsky.feed.post" => format!("https://{site}/profile/{actor}/post/{rkey}"),
            "app.bsky.feed.generator" => format!("https://{site}/profile/{actor}/feed/{rkey}"),
            "app.bsky.graph.list" => format!("https://{site}/profile/{actor}/lists/{rkey}"),
            "app.bsky.graph.starterpack" => format!("https://{site}/starter-pack/{actor}/{rkey}"),
            _ => return None,
        })
    }

    /// The link to `uri`, its author known only by the DID in it.
    fn url_by_did(&self, uri: &str) -> Option<String> {
        let (repo, _, _) = at_uri_parts(uri)?;
        self.url(&self.actor_for(repo), uri)
    }
}

/// ` <url>`, or nothing.
fn link(url: Option<String>) -> String {
    url.map(|u| format!(" <{u}>")).unwrap_or_default()
}

fn push_parts(line: &mut String, parts: Vec<String>) {
    for part in parts {
        line.push_str(" [");
        line.push_str(&part);
        line.push(']');
    }
}

fn push_text(line: &mut String, text: &str) {
    if !text.is_empty() {
        line.push(' ');
        line.push_str(text);
    }
}

/// A `getPostThread` answer as one line, or `None` when there is nothing
/// to show: a post that is gone (`#notFoundPost`), or one hidden from the
/// bot whose raw read failed.
pub fn render_thread(answer: &Value, cx: &Cx) -> Option<String> {
    let thread = &answer["thread"];
    match thread["$type"].as_str() {
        Some(THREAD_VIEW_POST) => {}
        Some(BLOCKED_POST) => return render_blocked_post(thread, cx),
        _ => return None,
    }
    let post = &thread["post"];
    if !post.is_object() {
        return None;
    }
    let mut line = String::from("🦋 ");
    if has_nsfw_label(post) || has_nsfw_label(&post["author"]) {
        line.push_str("[nsfw] ");
    }
    line.push_str(&who(&post["author"]));
    line.push_str(&reply_note(&thread["parent"], &post["record"], cx));
    line.push(':');
    push_text(&mut line, &post_text(&post["record"]));
    push_parts(&mut line, embed_parts(&post["embed"], cx, 0));
    Some(line)
}

/// The thread's own post when the AppView hides it from the bot (a block
/// either way): read raw, and said to be blocked; deleted if it is gone.
fn render_blocked_post(thread: &Value, cx: &Cx) -> Option<String> {
    let uri = thread["uri"].as_str()?;
    let (did, _, _) = at_uri_parts(uri)?;
    let record = cx.hidden.records.get(uri)?;
    let mut line = String::from("🦋 ");
    let Some(record) = record else {
        line.push_str(&cx.who_did(did));
        line.push_str(" (blocked): this post was deleted");
        return Some(line);
    };
    if has_nsfw_self_label(record) {
        line.push_str("[nsfw] ");
    }
    line.push_str(&cx.who_did(did));
    line.push_str(" (blocked)");
    if let Some(parent) = record["reply"]["parent"]["uri"].as_str() {
        line.push_str(&format!(", replying to{}", link(cx.url_by_did(parent))));
    }
    line.push(':');
    push_text(&mut line, &post_text(record));
    push_parts(&mut line, raw_embed_parts(&record["embed"], did, cx));
    Some(line)
}

/// `, replying to @handle <url>` for the post's parent, with what hides
/// it when something does; nothing for a post that is not a reply.
fn reply_note(parent: &Value, record: &Value, cx: &Cx) -> String {
    let uri = parent["uri"]
        .as_str()
        .or_else(|| record["reply"]["parent"]["uri"].as_str());
    match parent["$type"].as_str() {
        Some(THREAD_VIEW_POST) => {
            let author = &parent["post"]["author"];
            let h = handle_of(author);
            let url = parent["post"]["uri"].as_str().and_then(|u| cx.url(&h, u));
            format!(", replying to @{h}{}", link(url))
        }
        Some(BLOCKED_POST) => {
            let Some((did, _, _)) = uri.and_then(at_uri_parts) else {
                return ", replying to a blocked post".into();
            };
            let h = cx.actor_for(did);
            match uri.and_then(|u| cx.hidden.records.get(u)) {
                Some(None) => format!(", replying to @{h} (blocked; that post was deleted)"),
                _ => format!(
                    ", replying to @{h} (blocked){}",
                    link(uri.and_then(|u| cx.url(&h, u)))
                ),
            }
        }
        Some(NOT_FOUND_POST) => match uri.and_then(at_uri_parts) {
            Some((did, _, _)) => {
                format!(", replying to a deleted post by @{}", cx.actor_for(did))
            }
            None => ", replying to a deleted post".into(),
        },
        // No parent view (an AppView that sent none): the record still
        // says what it answers.
        _ => match record["reply"]["parent"]["uri"].as_str() {
            Some(u) => format!(", replying to{}", link(cx.url_by_did(u))),
            None => String::new(),
        },
    }
}

/// What a post's embed view carries, each as one bracketed part.
fn embed_parts(embed: &Value, cx: &Cx, depth: usize) -> Vec<String> {
    let mut out = Vec::new();
    match embed["$type"].as_str().unwrap_or_default() {
        "app.bsky.embed.images#view" => {
            for img in embed["images"].as_array().into_iter().flatten() {
                let url = one_line(
                    img["fullsize"]
                        .as_str()
                        .or_else(|| img["thumb"].as_str())
                        .unwrap_or_default(),
                );
                out.push(format!(
                    "image{}{}",
                    link((!url.is_empty()).then_some(url)),
                    alt(img)
                ));
            }
        }
        "app.bsky.embed.video#view" => {
            let url = one_line(embed["playlist"].as_str().unwrap_or_default());
            out.push(format!(
                "video{}{}",
                link((!url.is_empty()).then_some(url)),
                alt(embed)
            ));
        }
        "app.bsky.embed.external#view" => out.push(external_part(&embed["external"])),
        "app.bsky.embed.record#view" => out.push(quoted(&embed["record"], cx, depth)),
        "app.bsky.embed.recordWithMedia#view" => {
            out.extend(embed_parts(&embed["media"], cx, depth));
            out.push(quoted(&embed["record"]["record"], cx, depth));
        }
        _ => {}
    }
    out
}

/// A quoted record (`app.bsky.embed.record#view`'s `record`).
fn quoted(rec: &Value, cx: &Cx, depth: usize) -> String {
    let uri = rec["uri"].as_str().unwrap_or_default();
    let did = at_uri_parts(uri).map(|(d, _, _)| d).unwrap_or_default();
    match rec["$type"].as_str().unwrap_or_default() {
        VIEW_RECORD => {
            let author = &rec["author"];
            let mut s = format!(
                "quoting {}{}:",
                who(author),
                link(cx.url(&handle_of(author), uri))
            );
            push_text(&mut s, &post_text(&rec["value"]));
            if depth + 1 < QUOTE_DEPTH {
                for e in rec["embeds"].as_array().into_iter().flatten() {
                    push_parts(&mut s, embed_parts(e, cx, depth + 1));
                }
            }
            s
        }
        VIEW_BLOCKED => match cx.hidden.records.get(uri) {
            Some(Some(record)) => {
                let mut s = format!(
                    "quoting {} (blocked){}:",
                    cx.who_did(did),
                    link(cx.url_by_did(uri))
                );
                push_text(&mut s, &post_text(record));
                push_parts(&mut s, raw_embed_parts(&record["embed"], did, cx));
                s
            }
            Some(None) => format!(
                "quoting {} (blocked; that post was deleted)",
                cx.who_did(did)
            ),
            None => format!(
                "quoting {} (blocked; it could not be read){}",
                cx.who_did(did),
                link(cx.url_by_did(uri))
            ),
        },
        VIEW_NOT_FOUND => {
            if did.is_empty() {
                "quoting a deleted post".into()
            } else {
                format!("quoting a deleted post by {}", cx.who_did(did))
            }
        }
        VIEW_DETACHED => format!(
            "quoting {} (detached by its author){}",
            cx.who_did(did),
            link(cx.url_by_did(uri))
        ),
        "app.bsky.feed.defs#generatorView" => format!(
            "feed: {}{}",
            one_line(rec["displayName"].as_str().unwrap_or_default()),
            link(cx.url(&handle_of(&rec["creator"]), uri))
        ),
        "app.bsky.graph.defs#listView" => format!(
            "list: {}{}",
            one_line(rec["name"].as_str().unwrap_or_default()),
            link(cx.url(&handle_of(&rec["creator"]), uri))
        ),
        "app.bsky.graph.defs#starterPackViewBasic" => format!(
            "starter pack: {}{}",
            one_line(rec["record"]["name"].as_str().unwrap_or_default()),
            link(cx.url(&handle_of(&rec["creator"]), uri))
        ),
        _ => format!("quoting a record{}", link(cx.url_by_did(uri))),
    }
}

/// What a RAW record's embed carries (the record a block hid, read from
/// the repo): the same parts as a view's, its blobs addressed the way the
/// AppView addresses them for that author.
fn raw_embed_parts(embed: &Value, did: &str, cx: &Cx) -> Vec<String> {
    let mut out = Vec::new();
    match embed["$type"].as_str().unwrap_or_default() {
        "app.bsky.embed.images" => {
            for img in embed["images"].as_array().into_iter().flatten() {
                let url = blob_cid(&img["image"]).map(|cid| {
                    format!("https://cdn.bsky.app/img/feed_fullsize/plain/{did}/{cid}@jpeg")
                });
                out.push(format!("image{}{}", link(url), alt(img)));
            }
        }
        "app.bsky.embed.video" => {
            let url = blob_cid(&embed["video"]).map(|cid| {
                format!(
                    "https://video.bsky.app/watch/{}/{cid}/playlist.m3u8",
                    did.replace(':', "%3A")
                )
            });
            out.push(format!("video{}{}", link(url), alt(embed)));
        }
        "app.bsky.embed.external" => out.push(external_part(&embed["external"])),
        "app.bsky.embed.record" => out.push(format!(
            "quoting a post{}",
            link(
                embed["record"]["uri"]
                    .as_str()
                    .and_then(|u| cx.url_by_did(u))
            )
        )),
        "app.bsky.embed.recordWithMedia" => {
            out.extend(raw_embed_parts(&embed["media"], did, cx));
            out.push(format!(
                "quoting a post{}",
                link(
                    embed["record"]["record"]["uri"]
                        .as_str()
                        .and_then(|u| cx.url_by_did(u))
                )
            ));
        }
        _ => {}
    }
    out
}

/// A `getProfile` answer as one line.
pub fn render_profile(answer: &Value) -> Option<String> {
    if !answer["handle"].is_string() && !answer["did"].is_string() {
        return None;
    }
    let mut line = String::from("🦋 ");
    if has_nsfw_label(answer) {
        line.push_str("[nsfw] ");
    }
    line.push_str(&who(answer));
    let bio = one_line(answer["description"].as_str().unwrap_or_default());
    if !bio.is_empty() {
        line.push_str(": ");
        line.push_str(&bio);
    }
    if let Some(n) = answer["followersCount"].as_u64() {
        line.push_str(&format!(
            " · {} follower{}",
            count(n),
            if n == 1 { "" } else { "s" }
        ));
    }
    Some(line)
}

// ── the call ──────────────────────────────────────────────────────────

/// One link's preview, as the bot read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub url: String,
    pub line: String,
}

/// Is there a bot here: does anything provide `appview.query` (the
/// atproto plugin, while it runs on this node)?
pub fn bot_available() -> bool {
    crate::services::available(veles_types::plugin::method::APPVIEW_QUERY)
}

/// What a node with no bot says. The link is slopdrop's, as before.
pub const NO_BOT: &str = "no bot here: nothing provides appview.query on this node \
                          (the atproto plugin does, where it runs); slopdrop's resolver answers";

/// The client host a link was written for, so the links a preview
/// writes point at the same client.
fn site_of(url: &str) -> &'static str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    CLIENT_HOSTS
        .iter()
        .copied()
        .find(|h| *h == host)
        .unwrap_or(CLIENT_HOSTS[0])
}

/// One `appview.query`, its answer's `value`.
async fn appview(method: &str, params: Vec<(String, String)>) -> Result<Value, String> {
    let p = veles_types::plugin::AppviewQueryParams {
        method: method.to_string(),
        params,
    };
    let pv = serde_json::to_value(p).map_err(|e| e.to_string())?;
    let call = crate::services::call(veles_types::plugin::method::APPVIEW_QUERY, pv);
    match tokio::time::timeout(QUERY_TIMEOUT, call).await {
        Ok(Ok(v)) => Ok(v.get("value").cloned().unwrap_or(Value::Null)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!("no answer in {}s", QUERY_TIMEOUT.as_secs())),
    }
}

/// One raw record (`repo.get_record`): `Ok(None)` when it was deleted.
async fn raw_record(uri: &str) -> Result<Option<Value>, String> {
    use veles_types::plugin::method::REPO_GET_RECORD;
    if !crate::services::available(REPO_GET_RECORD) {
        return Err(format!("nothing provides {REPO_GET_RECORD} here"));
    }
    let pv = serde_json::to_value(veles_types::plugin::GetRecordParams {
        uri: uri.to_string(),
    })
    .map_err(|e| e.to_string())?;
    let call = crate::services::call(REPO_GET_RECORD, pv);
    let v = match tokio::time::timeout(QUERY_TIMEOUT, call).await {
        Ok(r) => r?,
        Err(_) => return Err(format!("no answer in {}s", QUERY_TIMEOUT.as_secs())),
    };
    let got: veles_types::plugin::GetRecordValue =
        serde_json::from_value(v).map_err(|e| e.to_string())?;
    Ok(got.found.then_some(got.value))
}

/// Read what a thread view hid, all at once: each record raw and each
/// author's profile. A read that fails is left out, and the preview says
/// that part could not be read.
pub async fn read_hidden(w: &Wants) -> Hidden {
    let records = futures::future::join_all(
        w.records
            .iter()
            .map(|uri| async move { (uri.clone(), raw_record(uri).await) }),
    );
    let profiles = futures::future::join_all(w.profiles.iter().map(|did| async move {
        let got = appview(
            "app.bsky.actor.getProfile",
            vec![("actor".to_string(), did.clone())],
        )
        .await;
        (did.clone(), got)
    }));
    let (records, profiles) = tokio::join!(records, profiles);
    let mut hidden = Hidden::default();
    for (uri, r) in records {
        match r {
            Ok(v) => {
                hidden.records.insert(uri, v);
            }
            Err(e) => tracing::debug!("bskylink: raw read of {uri} failed: {e}"),
        }
    }
    for (did, r) in profiles {
        match r {
            Ok(v) if v.is_object() => {
                hidden.profiles.insert(did, v);
            }
            Ok(_) => {}
            Err(e) => tracing::debug!("bskylink: profile of {did} failed: {e}"),
        }
    }
    hidden
}

/// Read each link as the bot. `Err` only when there is no bot at all;
/// a link the bot could not read is left out of the answer (and logged),
/// so the caller leaves it to slopdrop.
pub async fn preview(links: &[Link]) -> Result<(Vec<Preview>, Vec<String>), String> {
    if !bot_available() {
        return Err(NO_BOT.to_string());
    }
    let mut out = Vec::new();
    let mut missed = Vec::new();
    for link in links {
        let (method, params) = link.query();
        let value = match appview(method, params).await {
            Ok(v) => v,
            Err(e) => {
                missed.push(format!("{}: {e}", link.url));
                continue;
            }
        };
        let line = match link.target {
            Target::Profile { .. } => render_profile(&value),
            Target::Post { .. } => {
                let hidden = read_hidden(&hidden_refs(&value)).await;
                render_thread(
                    &value,
                    &Cx {
                        site: site_of(&link.url),
                        hidden: &hidden,
                    },
                )
            }
        };
        match line {
            Some(line) => out.push(Preview {
                url: link.url.clone(),
                line,
            }),
            None => missed.push(format!(
                "{}: nothing to show (deleted, or hidden and unreadable)",
                link.url
            )),
        }
    }
    Ok((out, missed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn post_and_profile_links_on_both_clients_parse() {
        assert_eq!(
            parse("https://impro.social/profile/Alice.Example.com/post/3mxhdhodv222n"),
            Some(Target::Post {
                actor: "alice.example.com".into(),
                rkey: "3mxhdhodv222n".into()
            })
        );
        assert_eq!(
            parse("https://bsky.app/profile/did:plc:z72i7hdynmk6r22z27h6tvur/post/3abc?ref=x#y"),
            Some(Target::Post {
                actor: "did:plc:z72i7hdynmk6r22z27h6tvur".into(),
                rkey: "3abc".into()
            })
        );
        assert_eq!(
            parse("https://www.bsky.app/profile/bsky.app/"),
            Some(Target::Profile {
                actor: "bsky.app".into()
            })
        );
        assert_eq!(
            parse("http://IMPRO.social/profile/a.b.c"),
            Some(Target::Profile {
                actor: "a.b.c".into()
            })
        );
    }

    #[test]
    fn other_routes_and_hosts_are_not_ours() {
        for url in [
            "https://example.com/profile/a.b/post/3abc",
            "https://bsky.app.evil.example/profile/a.b/post/3abc",
            "https://bsky.app/profile/a.b/feed/whats-hot",
            "https://bsky.app/profile/a.b/lists/3abc",
            "https://bsky.app/profile/nodot/post/3abc",
            "https://bsky.app/profile/a.b/post/..",
            "https://bsky.app/profile/a.b/post/x%2Fy",
            "https://bsky.app/search?q=x",
            "https://impro.social/",
            "https://impro.social",
        ] {
            assert_eq!(parse(url), None, "{url}");
        }
    }

    #[test]
    fn a_line_yields_at_most_two_distinct_links() {
        let line = "see https://bsky.app/profile/a.b/post/1 and \
                    (https://bsky.app/profile/a.b/post/1) also https://example.com \
                    https://impro.social/profile/c.d https://bsky.app/profile/e.f/post/2";
        let links = links_in(line);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].url, "https://bsky.app/profile/a.b/post/1");
        assert_eq!(links[1].url, "https://impro.social/profile/c.d");
    }

    #[test]
    fn a_post_link_queries_the_thread_by_its_at_uri() {
        let l = &links_in("https://impro.social/profile/a.b/post/3xyz")[0];
        let (m, p) = l.query();
        assert_eq!(m, "app.bsky.feed.getPostThread");
        assert!(veles_types::plugin::is_appview_read(m));
        assert_eq!(
            p[0],
            ("uri".into(), "at://a.b/app.bsky.feed.post/3xyz".into())
        );
        let l = &links_in("https://bsky.app/profile/a.b")[0];
        let (m, p) = l.query();
        assert_eq!(m, "app.bsky.actor.getProfile");
        assert!(veles_types::plugin::is_appview_read(m));
        assert_eq!(p, vec![("actor".into(), "a.b".into())]);
    }

    fn thread(post: Value, parent: Option<Value>) -> Value {
        let mut t = json!({"$type": THREAD_VIEW_POST, "post": post});
        if let Some(p) = parent {
            t["parent"] = p;
        }
        json!({ "thread": t })
    }

    fn render(v: &Value, hidden: &Hidden) -> Option<String> {
        render_thread(
            v,
            &Cx {
                site: "bsky.app",
                hidden,
            },
        )
    }

    #[test]
    fn a_post_renders_whole_with_its_parent_images_and_quote_linked() {
        let v = thread(
            json!({
                "author": {"handle": "alice.example.com", "displayName": "Alice\nA."},
                "record": {"text": "line one\n\nline\ttwo \u{3}04red\u{3} \u{2}bold"},
                "embed": {
                    "$type": "app.bsky.embed.recordWithMedia#view",
                    "media": {"$type": "app.bsky.embed.images#view", "images": [
                        {"fullsize": "https://cdn.example/1@jpeg", "thumb": "t1", "alt": "a cat\non a mat"},
                        {"fullsize": "https://cdn.example/2@jpeg", "alt": ""}
                    ]},
                    "record": {"record": {
                        "$type": VIEW_RECORD,
                        "uri": "at://did:plc:bob/app.bsky.feed.post/q1",
                        "author": {"did": "did:plc:bob", "handle": "bob.example.com"},
                        "value": {"text": "quoted\ntext"},
                        "embeds": [{"$type": "app.bsky.embed.video#view",
                                    "playlist": "https://video.example/p.m3u8", "alt": "a clip"}]
                    }}
                }
            }),
            Some(json!({
                "$type": THREAD_VIEW_POST,
                "post": {"uri": "at://did:plc:carol/app.bsky.feed.post/p1",
                         "author": {"handle": "carol.example.com"}}
            })),
        );
        assert_eq!(
            render(&v, &Hidden::default()).unwrap(),
            "🦋 Alice A. (@alice.example.com), replying to @carol.example.com \
             <https://bsky.app/profile/carol.example.com/post/p1>: line one line two 04red bold \
             [image <https://cdn.example/1@jpeg> — alt: a cat on a mat] \
             [image <https://cdn.example/2@jpeg>] \
             [quoting @bob.example.com <https://bsky.app/profile/bob.example.com/post/q1>: \
             quoted text [video <https://video.example/p.m3u8> — alt: a clip]]"
        );
    }

    #[test]
    fn a_link_card_says_its_url_title_and_description() {
        let v = thread(
            json!({
                "author": {"handle": "a.b", "displayName": ""},
                "record": {"text": ""},
                "embed": {"$type": "app.bsky.embed.external#view",
                          "external": {"uri": "https://x.example/a", "title": "A title",
                                       "description": "What it says"}}
            }),
            None,
        );
        assert_eq!(
            render(&v, &Hidden::default()).unwrap(),
            "🦋 @a.b: [link <https://x.example/a> — A title: What it says]"
        );
        let v = thread(
            json!({"author": {"handle": "a.b"}, "record": {"text": ""},
                   "embed": {"$type": "app.bsky.embed.external#view",
                             "external": {"uri": "https://x.example", "title": ""}}}),
            None,
        );
        assert_eq!(
            render(&v, &Hidden::default()).unwrap(),
            "🦋 @a.b: [link <https://x.example>]"
        );
    }

    /// The USER, 2026-10-10: "I want the bot never to ellide content".
    #[test]
    fn nothing_is_cut_however_long() {
        let long = "word ".repeat(100);
        let name = "N".repeat(150);
        let v = thread(
            json!({
                "author": {"handle": "a.b", "displayName": name},
                "record": {"text": long},
                "labels": [{"val": "porn"}],
                "embed": {"$type": "app.bsky.embed.video#view", "playlist": "https://v.example/p.m3u8"}
            }),
            None,
        );
        let line = render(&v, &Hidden::default()).unwrap();
        assert!(
            line.starts_with(&format!("🦋 [nsfw] {name} (@a.b): word word")),
            "{line}"
        );
        assert_eq!(line.matches("word").count(), 100, "{line}");
        assert!(!line.contains('…'), "{line}");
        assert!(
            line.ends_with("word [video <https://v.example/p.m3u8>]"),
            "{line}"
        );
        let bio = "b".repeat(400);
        let p = render_profile(&json!({"handle": "a.b", "description": bio})).unwrap();
        assert!(p.ends_with(&bio), "{p}");
        // A negated label is not a label.
        let v = thread(
            json!({"author": {"handle": "a.b"}, "record": {"text": "x"},
                   "labels": [{"val": "porn", "neg": true}]}),
            None,
        );
        assert_eq!(render(&v, &Hidden::default()).unwrap(), "🦋 @a.b: x");
    }

    #[test]
    fn a_shortened_link_shows_its_full_url() {
        let text = "read example.com/some/pa... and this — here ok";
        let s = text.find("example").unwrap();
        let e = s + "example.com/some/pa...".len();
        let h = text.find("here").unwrap();
        let facets = json!([
            {"index": {"byteStart": s, "byteEnd": e},
             "features": [{"$type": "app.bsky.richtext.facet#link",
                           "uri": "https://example.com/some/path/to/page?x=1"}]},
            {"index": {"byteStart": h, "byteEnd": h + 4},
             "features": [{"$type": "app.bsky.richtext.facet#link", "uri": "https://y.example/"}]},
            // Overlapping, out of range, inside a character: ignored.
            {"index": {"byteStart": s + 2, "byteEnd": e},
             "features": [{"$type": "app.bsky.richtext.facet#link", "uri": "https://evil.example"}]},
            {"index": {"byteStart": 0, "byteEnd": 9999},
             "features": [{"$type": "app.bsky.richtext.facet#link", "uri": "https://evil.example"}]},
            {"index": {"byteStart": text.find('—').unwrap() + 1, "byteEnd": h},
             "features": [{"$type": "app.bsky.richtext.facet#link", "uri": "https://evil.example"}]},
            // A mention is not a link.
            {"index": {"byteStart": 0, "byteEnd": 4},
             "features": [{"$type": "app.bsky.richtext.facet#mention", "did": "did:plc:x"}]}
        ]);
        assert_eq!(
            expand_links(text, &facets),
            "read https://example.com/some/path/to/page?x=1 and this — here <https://y.example/> ok"
        );
        assert_eq!(expand_links("plain", &Value::Null), "plain");
    }

    fn quoting_blocked(uri: &str) -> Value {
        thread(
            json!({
                "author": {"handle": "k.example", "displayName": "K"},
                "record": {"text": "look at this", "embed": {"$type": "app.bsky.embed.record",
                           "record": {"uri": uri, "cid": "bafyq"}}},
                "embed": {"$type": "app.bsky.embed.record#view", "record": {
                    "$type": VIEW_BLOCKED, "uri": uri, "blocked": true,
                    "author": {"did": "did:plc:ed", "viewer": {}}
                }}
            }),
            None,
        )
    }

    fn ed_profile() -> HashMap<String, Value> {
        HashMap::from([(
            "did:plc:ed".to_string(),
            json!({"did": "did:plc:ed", "handle": "ed.example", "displayName": "Ed"}),
        )])
    }

    /// The USER: "I want to know if it was deleted, and if it wasn't
    /// deleted I want to know what it was".
    #[test]
    fn a_blocked_quote_is_read_raw_or_said_to_be_deleted() {
        let uri = "at://did:plc:ed/app.bsky.feed.post/3q";
        let v = quoting_blocked(uri);
        let want = hidden_refs(&v);
        assert_eq!(want.records, BTreeSet::from([uri.to_string()]));
        assert_eq!(want.profiles, BTreeSet::from(["did:plc:ed".to_string()]));

        let found = Hidden {
            records: HashMap::from([(
                uri.to_string(),
                Some(json!({"text": "the hidden\npost", "embed": {
                    "$type": "app.bsky.embed.images",
                    "images": [{"image": {"$type": "blob", "ref": {"$link": "bafkimg"},
                                          "mimeType": "image/jpeg"}, "alt": "a chart"}]
                }})),
            )]),
            profiles: ed_profile(),
        };
        assert_eq!(
            render(&v, &found).unwrap(),
            "🦋 K (@k.example): look at this [quoting Ed (@ed.example) (blocked) \
             <https://bsky.app/profile/ed.example/post/3q>: the hidden post \
             [image <https://cdn.bsky.app/img/feed_fullsize/plain/did:plc:ed/bafkimg@jpeg> — alt: a chart]]"
        );

        let deleted = Hidden {
            records: HashMap::from([(uri.to_string(), None)]),
            profiles: ed_profile(),
        };
        assert_eq!(
            render(&v, &deleted).unwrap(),
            "🦋 K (@k.example): look at this [quoting Ed (@ed.example) (blocked; that post was deleted)]"
        );

        // Neither read came back: still whose and where, by DID.
        assert_eq!(
            render(&v, &Hidden::default()).unwrap(),
            "🦋 K (@k.example): look at this [quoting @did:plc:ed (blocked; it could not be read) \
             <https://bsky.app/profile/did:plc:ed/post/3q>]"
        );
    }

    #[test]
    fn hidden_and_gone_parents_say_so_and_link() {
        let post = json!({"author": {"handle": "k.example"}, "record": {"text": "reply"}});
        let blocked = thread(
            post.clone(),
            Some(
                json!({"$type": BLOCKED_POST, "uri": "at://did:plc:ed/app.bsky.feed.post/p",
                        "blocked": true, "author": {"did": "did:plc:ed"}}),
            ),
        );
        let w = hidden_refs(&blocked);
        assert!(w.records.contains("at://did:plc:ed/app.bsky.feed.post/p"));
        let hidden = Hidden {
            records: HashMap::from([(
                "at://did:plc:ed/app.bsky.feed.post/p".to_string(),
                Some(json!({"text": "p"})),
            )]),
            profiles: ed_profile(),
        };
        assert_eq!(
            render(&blocked, &hidden).unwrap(),
            "🦋 @k.example, replying to @ed.example (blocked) \
             <https://bsky.app/profile/ed.example/post/p>: reply"
        );
        let mut gone = hidden.clone();
        gone.records
            .insert("at://did:plc:ed/app.bsky.feed.post/p".to_string(), None);
        assert_eq!(
            render(&blocked, &gone).unwrap(),
            "🦋 @k.example, replying to @ed.example (blocked; that post was deleted): reply"
        );
        let deleted = thread(
            post,
            Some(
                json!({"$type": NOT_FOUND_POST, "uri": "at://did:plc:ed/app.bsky.feed.post/p",
                        "notFound": true}),
            ),
        );
        assert_eq!(
            hidden_refs(&deleted).profiles,
            BTreeSet::from(["did:plc:ed".to_string()])
        );
        assert_eq!(
            render(&deleted, &hidden).unwrap(),
            "🦋 @k.example, replying to a deleted post by @ed.example: reply"
        );
    }

    #[test]
    fn a_post_hidden_from_the_bot_is_read_raw() {
        let uri = "at://did:plc:ed/app.bsky.feed.post/3m";
        let v = json!({"thread": {"$type": BLOCKED_POST, "uri": uri, "blocked": true,
                                  "author": {"did": "did:plc:ed"}}});
        assert_eq!(hidden_refs(&v).records, BTreeSet::from([uri.to_string()]));
        let hidden = Hidden {
            records: HashMap::from([(
                uri.to_string(),
                Some(
                    json!({"text": "mine", "labels": {"values": [{"val": "nudity"}]},
                            "reply": {"parent": {"uri": "at://did:plc:x/app.bsky.feed.post/r"}}}),
                ),
            )]),
            profiles: ed_profile(),
        };
        assert_eq!(
            render(&v, &hidden).unwrap(),
            "🦋 [nsfw] Ed (@ed.example) (blocked), replying to \
             <https://bsky.app/profile/did:plc:x/post/r>: mine"
        );
        let gone = Hidden {
            records: HashMap::from([(uri.to_string(), None)]),
            profiles: ed_profile(),
        };
        assert_eq!(
            render(&v, &gone).unwrap(),
            "🦋 Ed (@ed.example) (blocked): this post was deleted"
        );
        // Unread: nothing to say, so slopdrop's resolver answers.
        assert_eq!(render(&v, &Hidden::default()), None);
    }

    #[test]
    fn deleted_and_detached_quotes_say_whose() {
        for (kind, said) in [
            (
                VIEW_NOT_FOUND,
                "[quoting a deleted post by Ed (@ed.example)]",
            ),
            (
                VIEW_DETACHED,
                "[quoting Ed (@ed.example) (detached by its author) \
                 <https://impro.social/profile/ed.example/post/3q>]",
            ),
        ] {
            let v = thread(
                json!({"author": {"handle": "k.example"}, "record": {"text": "x"},
                       "embed": {"$type": "app.bsky.embed.record#view", "record": {
                           "$type": kind, "uri": "at://did:plc:ed/app.bsky.feed.post/3q"}}}),
                None,
            );
            assert_eq!(
                hidden_refs(&v).profiles,
                BTreeSet::from(["did:plc:ed".to_string()])
            );
            let hidden = Hidden {
                records: HashMap::new(),
                profiles: ed_profile(),
            };
            let line = render_thread(
                &v,
                &Cx {
                    site: "impro.social",
                    hidden: &hidden,
                },
            )
            .unwrap();
            assert_eq!(line, format!("🦋 @k.example: x {said}"));
        }
    }

    #[test]
    fn gone_posts_render_nothing() {
        for t in [
            json!({"thread": {"$type": NOT_FOUND_POST, "notFound": true}}),
            json!({"thread": {"$type": BLOCKED_POST, "blocked": true}}),
            json!({}),
            Value::Null,
        ] {
            assert_eq!(render(&t, &Hidden::default()), None, "{t}");
        }
    }

    #[test]
    fn a_profile_renders_with_its_bio_and_followers() {
        let v = json!({
            "handle": "bsky.app", "displayName": "Bluesky",
            "description": "official\nBluesky account", "followersCount": 32_512_345u64
        });
        assert_eq!(
            render_profile(&v).unwrap(),
            "🦋 Bluesky (@bsky.app): official Bluesky account · 32.5M followers"
        );
        let v = json!({"handle": "a.b", "followersCount": 1});
        assert_eq!(render_profile(&v).unwrap(), "🦋 @a.b · 1 follower");
        assert_eq!(render_profile(&json!({"error": "x"})), None);
    }

    #[test]
    fn counts_shorten() {
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1K");
        assert_eq!(count(1_234), "1.2K");
        assert_eq!(count(123_456), "123K");
        assert_eq!(count(2_500_000), "2.5M");
    }

    #[test]
    fn links_point_at_the_client_they_came_from() {
        assert_eq!(
            site_of("https://impro.social/profile/a.b/post/1"),
            "impro.social"
        );
        assert_eq!(site_of("https://WWW.bsky.app/profile/a.b"), "bsky.app");
        assert_eq!(site_of("https://elsewhere.example/x"), "bsky.app");
    }

    /// No provider: the row says so rather than answer, so the link
    /// stays slopdrop's. With one, the query reaches it as the contract
    /// says and its answer becomes the line; a post it cannot show is
    /// left out (and said why), not answered; a blocked quote is read
    /// through `repo.get_record`.
    #[test]
    fn previews_go_through_the_appview_query_service() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let links = links_in(
            "https://impro.social/profile/a.b/post/1 https://bsky.app/profile/a.b/post/gone",
        );
        assert!(!bot_available());
        assert_eq!(rt.block_on(preview(&links)).unwrap_err(), NO_BOT);

        let owner = crate::lifecycle::owner_tag("process:bskylink-test");
        crate::services::register_for(
            owner,
            crate::services::ServiceDecl {
                name: veles_types::plugin::method::APPVIEW_QUERY.into(),
                cap: Some("any".into()),
                ..Default::default()
            },
            crate::services::handler(|p, _cx| async move {
                let q: veles_types::plugin::AppviewQueryParams =
                    serde_json::from_value(p).map_err(|e| e.to_string())?;
                if q.method == "app.bsky.actor.getProfile" {
                    return Ok(json!({"value": {"did": "did:plc:ed", "handle": "ed.example"}}));
                }
                assert_eq!(q.method, "app.bsky.feed.getPostThread");
                let uri = &q.params[0].1;
                let thread = if uri.ends_with("/gone") {
                    json!({"$type": NOT_FOUND_POST})
                } else {
                    json!({"$type": THREAD_VIEW_POST,
                           "post": {"author": {"handle": "a.b"}, "record": {"text": "hi"},
                                    "embed": {"$type": "app.bsky.embed.record#view", "record": {
                                        "$type": VIEW_BLOCKED,
                                        "uri": "at://did:plc:ed/app.bsky.feed.post/3q"}}}})
                };
                Ok(json!({"value": {"thread": thread}}))
            }),
        )
        .unwrap();
        crate::services::register_for(
            owner,
            crate::services::ServiceDecl {
                name: veles_types::plugin::method::REPO_GET_RECORD.into(),
                cap: Some("any".into()),
                ..Default::default()
            },
            crate::services::handler(|p, _cx| async move {
                let q: veles_types::plugin::GetRecordParams =
                    serde_json::from_value(p).map_err(|e| e.to_string())?;
                assert_eq!(q.uri, "at://did:plc:ed/app.bsky.feed.post/3q");
                Ok(json!({"found": true, "value": {"text": "behind the block"}}))
            }),
        )
        .unwrap();
        assert!(bot_available());
        let (previews, missed) = rt.block_on(preview(&links)).unwrap();
        crate::services::purge_owner(Some(owner));
        assert_eq!(
            previews,
            vec![Preview {
                url: "https://impro.social/profile/a.b/post/1".into(),
                line: "🦋 @a.b: hi [quoting @ed.example (blocked) \
                       <https://impro.social/profile/ed.example/post/3q>: behind the block]"
                    .into()
            }]
        );
        assert_eq!(missed.len(), 1);
        assert!(missed[0].contains("/gone: nothing to show"), "{missed:?}");
    }
}
