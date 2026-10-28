//! The ledger of public `/tcl` replies: what the owner can approve or delete.
//!
//! EVERY public `/tcl` reply is recorded here, by what it is ([`PostKind`]):
//! a WITHHELD note the output check (or the bad list) held back, which the
//! owner is asked about and can APPROVE (repost inline, update the
//! allowlist/bad list) or DELETE (take the post/link/blobs down); an INLINE
//! output, which the owner can still delete; and the note posted for an
//! output already BLOCKED. Before inline replies were recorded, a reply
//! that passed the check could not be taken down by the bot at all.
//! `<state>/withheld.json` (data.rs), 7-day expiry, deduped by output
//! hash within a kind — the same output withheld twice is ONE entry that
//! remembers both posts, so a delete takes both down.
//!
//! The owner names an entry by its id or by anything they can see of it:
//! the post's link in any client, its at-uri or rkey, the `/t/` short link
//! or its id, the viewer link, or the request the reply answered
//! ([`WithheldStore::find`]).
//!
//! Every operation re-reads the file under the lock and writes it back
//! before letting go: nothing holds a copy across an await. The first
//! version handed out a snapshot, and `slopdrop_publish::act` saved that
//! snapshot back after seconds of reposting — with parallel asks, one
//! approval's save resurrected an entry another had just removed (asked
//! again, reposted twice), and an entry recorded in between was lost.
//!
//! This module is platform-neutral: it knows the output and the reason,
//! never how to repost or take down. The byte-moving half is the atproto
//! adapter's, through the `slopdrop_publish` sink.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Deserializer, Serialize};

use crate::check::text_hash;

/// What a recorded `/tcl` post is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PostKind {
    /// A note + link in place of the output: the owner is asked about it
    /// and may approve or delete it. Entries from before the kind existed
    /// are all this.
    #[default]
    Withheld,
    /// The output itself (it passed the check, the owner allowlisted it,
    /// or an approve reposted it). Never asked about; deletable.
    Inline,
    /// The note posted for an output the owner already deleted. No hosted
    /// copy; never asked about. Approving it lifts the block and reposts.
    Blocked,
}

impl PostKind {
    pub fn label(self) -> &'static str {
        match self {
            PostKind::Withheld => "withheld",
            PostKind::Inline => "inline",
            PostKind::Blocked => "blocked note",
        }
    }
}

/// One recorded `/tcl` reply.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WithheldEntry {
    /// Short id (8 lowercase hex chars), what the owner answers by.
    pub id: String,
    /// What the post is. Only a [`PostKind::Withheld`] entry is asked about.
    #[serde(default)]
    pub kind: PostKind,
    /// Unix seconds when it was (last) withheld.
    pub at: u64,
    /// Who asked: the handle the reader knows, and the verified id.
    pub requester: String,
    /// Platform/route chat id the request came from.
    pub chat_id: String,
    /// The inbound message id (the at-uri the repost threads under).
    pub request_msg_id: String,
    /// The /tcl line.
    pub code: String,
    /// The full output.
    pub output: String,
    /// The stored proc's name, when ONE stored proc produced it.
    pub proc_name: Option<String>,
    pub proc_hash: Option<String>,
    /// Why it was withheld (or, for a post that was not, what let it out).
    pub reason: String,
    // ── filled by the adapter after posting (every post of this output) ──
    /// The bot's post at-uris.
    #[serde(default)]
    pub post_uris: Vec<String>,
    /// The rendered .html blob CID (the newest post's).
    #[serde(default)]
    pub blob_html: Option<String>,
    /// The raw .md blob CID (the newest post's).
    #[serde(default)]
    pub blob_md: Option<String>,
    /// The link the newest post carried (the short link, or the full
    /// viewer URL when no short link could be written).
    #[serde(default)]
    pub link: Option<String>,
    /// The `/t/<id>` short-link ids.
    #[serde(default, alias = "short_id", deserialize_with = "one_or_many")]
    pub short_ids: Vec<String>,
    /// The owner-pointer record rkeys (`online.7778777.tcl.output`).
    #[serde(default, alias = "record_rkey", deserialize_with = "one_or_many")]
    pub record_rkeys: Vec<String>,
    /// The owner said "keep withheld and stop asking": syncask leaves the
    /// entry (still listed, still deletable) but never re-asks about it.
    /// It expires silently at the 7-day mark like any other.
    #[serde(default)]
    pub no_ask: bool,
    // ── a reply with pictures (PLAN-slopdrop-art.md P4) ──
    /// The drawings the reply carries or would carry, kept as PNGs in
    /// `<state>/art/`: the owner's question shows them, an approve
    /// reposts them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pictures: Vec<PictureRef>,
    /// The self-labels the reply carries (the code's and the image
    /// check's).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// What else the code recorded, validated (a quote, links, …), as a
    /// JSON array: an approve reposts it with the pictures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    /// The text an approve posts beside the pictures (veles' notes when
    /// the output itself was the drawing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_text: Option<String>,
    /// The withheld post(s) an approve replaced: taken down when the
    /// repost's delivery receipt arrives, so the reply is never gone
    /// while the repost is on its way.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub superseded: Vec<String>,
}

/// One drawing a reply carries, by its kept PNG.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PictureRef {
    /// sha256 of the PNG: `<state>/art/<sha>.png`.
    pub sha: String,
    pub alt: String,
    pub width: u32,
    pub height: u32,
    /// sha256 of the drawing's TEXT: an approve allowlists it, a delete
    /// blocks it, as for a text output.
    pub text_hash: String,
}

/// The first version stored one `short_id`/`record_rkey` per entry; a list
/// replaced each. Read both shapes: null, one string, or a list.
fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => Vec::new(),
        Some(OneOrMany::One(s)) => vec![s],
        Some(OneOrMany::Many(v)) => v,
    })
}

/// sha256 of an output, the dedup key.
pub fn output_hash(output: &str) -> String {
    text_hash(output)
}

const EXPIRY_SECS: u64 = 7 * 24 * 3600;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn push_unique(into: &mut Vec<String>, from: &[String]) {
    for x in from {
        if !into.contains(x) {
            into.push(x.clone());
        }
    }
}

/// What one post of a withheld reply left behind, for [`WithheldStore::add_post`].
#[derive(Debug, Clone, Default)]
pub struct PostIds {
    pub post_uris: Vec<String>,
    /// `(html_cid, md_cid)`.
    pub blobs: Option<(String, String)>,
    pub link: Option<String>,
    pub short_id: Option<String>,
    pub record_rkey: Option<String>,
}

/// A handle on the withheld store's file. Cheap; holds no entries.
pub struct WithheldStore {
    path: PathBuf,
}

impl WithheldStore {
    /// The store at `<state>/withheld.json` (data.rs).
    pub fn load() -> Self {
        Self::at(crate::data::dir().join("withheld.json"))
    }

    /// The store at a specific path (tests use a tempdir).
    pub fn at(path: PathBuf) -> Self {
        WithheldStore { path }
    }

    /// The live entries, read now. Call with the lock held.
    fn read(&self) -> Vec<WithheldEntry> {
        let n = now();
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|b| serde_json::from_str::<Vec<WithheldEntry>>(&b).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|e| n.saturating_sub(e.at) < EXPIRY_SECS)
            .collect()
    }

    /// Write the entries. Call with the lock held.
    fn write(&self, entries: &[WithheldEntry]) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("withheld dir: {e}"))?;
        }
        let body = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|e| format!("withheld write: {e}"))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("withheld rename: {e}"))
    }

    /// Read, change, write — all under the lock.
    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut Vec<WithheldEntry>) -> Result<T, String>,
    ) -> Result<T, String> {
        let _g = lock();
        let mut entries = self.read();
        let out = f(&mut entries)?;
        self.write(&entries)?;
        Ok(out)
    }

    /// Live (non-expired) entries, newest first.
    pub fn list(&self) -> Vec<WithheldEntry> {
        let _g = lock();
        let mut v = self.read();
        v.sort_by_key(|e| std::cmp::Reverse(e.at));
        v
    }

    /// One live entry by short id.
    pub fn get(&self, id: &str) -> Option<WithheldEntry> {
        let _g = lock();
        self.read().into_iter().find(|e| e.id == id)
    }

    /// Record a reply and return its id. The same output already recorded
    /// as the same kind is the SAME entry: it keeps its id (and any open
    /// question about it, and a "stop asking"), takes the newest request's
    /// details, and keeps every post/record/link id it already had — so one
    /// delete takes every copy down. A new output gets a fresh id.
    pub fn insert(&self, mut e: WithheldEntry) -> Result<String, String> {
        self.mutate(|entries| {
            let h = output_hash(&e.output);
            if let Some(old) = entries
                .iter_mut()
                .find(|x| x.kind == e.kind && output_hash(&x.output) == h)
            {
                let mut posts = std::mem::take(&mut old.post_uris);
                push_unique(&mut posts, &e.post_uris);
                let mut shorts = std::mem::take(&mut old.short_ids);
                push_unique(&mut shorts, &e.short_ids);
                let mut rkeys = std::mem::take(&mut old.record_rkeys);
                push_unique(&mut rkeys, &e.record_rkeys);
                let (id, no_ask) = (old.id.clone(), old.no_ask);
                *old = WithheldEntry {
                    id: id.clone(),
                    post_uris: posts,
                    short_ids: shorts,
                    record_rkeys: rkeys,
                    blob_html: e.blob_html.take().or(old.blob_html.take()),
                    blob_md: e.blob_md.take().or(old.blob_md.take()),
                    link: e.link.take().or(old.link.take()),
                    no_ask,
                    ..e
                };
                return Ok(id);
            }
            if e.id.is_empty() || entries.iter().any(|x| x.id == e.id) {
                e.id = fresh_id(entries);
            }
            let id = e.id.clone();
            entries.push(e);
            Ok(id)
        })
    }

    /// The entry the owner named: its id, or anything they can see of it
    /// (see [`Target`]). A post rkey is looked for among the bot's posts
    /// first, then among the requests — the reply to a request is what
    /// "delete this" means when the request is what they pasted.
    pub fn find(&self, target: &str) -> Option<WithheldEntry> {
        let t = Target::parse(target)?;
        let entries = self.list();
        let hit = |f: &dyn Fn(&WithheldEntry) -> bool| entries.iter().find(|e| f(e)).cloned();
        match &t {
            Target::Token(k) => hit(&|e| {
                e.id == *k
                    || e.post_uris.iter().any(|u| uri_rkey(u) == k)
                    || e.short_ids.iter().any(|x| x == k)
            })
            .or_else(|| hit(&|e| uri_rkey(&e.request_msg_id) == k)),
            Target::Post(rkey) => hit(&|e| e.post_uris.iter().any(|u| uri_rkey(u) == rkey))
                .or_else(|| hit(&|e| uri_rkey(&e.request_msg_id) == rkey)),
            Target::Short(id) => hit(&|e| e.short_ids.iter().any(|x| x == id)),
            Target::Html(cid) => hit(&|e| e.blob_html.as_deref() == Some(cid)),
        }
    }

    /// Mutate one entry in place and save.
    pub fn update<F: FnOnce(&mut WithheldEntry)>(&self, id: &str, f: F) -> Result<(), String> {
        self.mutate(|entries| {
            let e = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| format!("no withheld entry {id}"))?;
            f(e);
            Ok(())
        })
    }

    /// Add the ids of one post that carried this entry (the adapter, right
    /// after posting it).
    pub fn add_post(&self, id: &str, ids: PostIds) -> Result<(), String> {
        self.update(id, |e| {
            push_unique(&mut e.post_uris, &ids.post_uris);
            if let Some((html, md)) = ids.blobs {
                e.blob_html = Some(html);
                e.blob_md = Some(md);
            }
            if ids.link.is_some() {
                e.link = ids.link;
            }
            push_unique(
                &mut e.short_ids,
                &ids.short_id.into_iter().collect::<Vec<_>>(),
            );
            push_unique(
                &mut e.record_rkeys,
                &ids.record_rkey.into_iter().collect::<Vec<_>>(),
            );
        })
    }

    /// Mark one entry "keep withheld and stop asking" (or clear the flag).
    /// The entry stays listed and deletable; syncask just never re-asks.
    pub fn set_no_ask(&self, id: &str, no_ask: bool) -> Result<(), String> {
        self.update(id, |e| e.no_ask = no_ask)
    }

    /// Remove one entry, returning it. Saves.
    pub fn remove(&self, id: &str) -> Result<WithheldEntry, String> {
        self.mutate(|entries| {
            let i = entries
                .iter()
                .position(|e| e.id == id)
                .ok_or_else(|| format!("no withheld entry {id}"))?;
            Ok(entries.remove(i))
        })
    }
}

/// Where [`post_link`] points: the owner's Bluesky client. Any client's
/// `/profile/<did>/post/<rkey>` link names the same post, and
/// [`WithheldStore::find`] reads them all.
pub const POST_LINK_BASE: &str = "https://impro.social";

/// A web link to one of the bot's posts, from its at-uri. `None` for
/// anything that is not `at://<did>/app.bsky.feed.post/<rkey>`.
pub fn post_link(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("at://")?;
    let mut parts = rest.split('/');
    let (did, coll, rkey) = (parts.next()?, parts.next()?, parts.next()?);
    if coll != "app.bsky.feed.post" || did.is_empty() || rkey.is_empty() {
        return None;
    }
    Some(format!(
        "{POST_LINK_BASE}/profile/{did}/post/{}",
        uri_rkey(uri)
    ))
}

/// The rkey an at-uri ends in, without the `#<cid>` a request id carries.
fn uri_rkey(uri: &str) -> &str {
    let u = uri.split('#').next().unwrap_or(uri);
    u.rsplit('/').next().unwrap_or(u)
}

/// What the owner named an entry by.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// A bare word: an entry id, a post rkey, a short-link id, or the
    /// request's rkey.
    Token(String),
    /// A post: `https://<any client>/profile/<actor>/post/<rkey>` or
    /// `at://<did>/app.bsky.feed.post/<rkey>`.
    Post(String),
    /// A short link: `https://<site>/t/<id>`.
    Short(String),
    /// The viewer link: `https://<site>/tcl/?html=<cid>&md=<cid>`.
    Html(String),
}

impl Target {
    fn parse(s: &str) -> Option<Target> {
        let s = s.trim().trim_matches(|c| c == '<' || c == '>');
        if s.is_empty() {
            return None;
        }
        if let Some(rest) = s.strip_prefix("at://") {
            let path = rest.split('#').next().unwrap_or(rest);
            let mut parts = path.split('/');
            return match (parts.next(), parts.next(), parts.next()) {
                (Some(_), Some("app.bsky.feed.post"), Some(rkey)) if !rkey.is_empty() => {
                    Some(Target::Post(rkey.to_string()))
                }
                _ => None,
            };
        }
        let Some(rest) = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
        else {
            return Some(Target::Token(s.to_string()));
        };
        let (path, query) = match rest.split_once('?') {
            Some((p, q)) => (p, q.split('#').next().unwrap_or(q)),
            None => (rest.split('#').next().unwrap_or(rest), ""),
        };
        if let Some(cid) = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("html="))
            .filter(|c| !c.is_empty())
        {
            return Some(Target::Html(cid.to_string()));
        }
        let segs: Vec<&str> = path.split('/').skip(1).filter(|x| !x.is_empty()).collect();
        match segs.as_slice() {
            ["profile", _actor, "post", rkey] => Some(Target::Post(rkey.to_string())),
            ["t", id] => Some(Target::Short(id.to_string())),
            _ => None,
        }
    }
}

/// A fresh 8-hex-char id, unique among `entries`.
fn fresh_id(entries: &[WithheldEntry]) -> String {
    use sha2::Digest as _;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut n = 0u64;
    loop {
        let mut h = sha2::Sha256::new();
        h.update(format!("{nanos}:{}:{n}", entries.len()).as_bytes());
        let id: String = format!("{:x}", h.finalize()).chars().take(8).collect();
        if !entries.iter().any(|e| e.id == id) {
            return id;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store in its own tempdir, removed when the guard drops. The first
    /// version keyed a dir by (pid, counter) under the temp root and never
    /// removed it: 28 `ww-*` dirs piled up in /dev/shm in one evening.
    fn tmp() -> (tempfile::TempDir, WithheldStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = WithheldStore::at(dir.path().join("withheld.json"));
        (dir, store)
    }

    fn entry(out: &str, at: u64) -> WithheldEntry {
        WithheldEntry {
            at,
            requester: "someone.example (atproto:did:plc:x)".into(),
            chat_id: "at://x".into(),
            request_msg_id: "at://x/post/1".into(),
            code: "draw".into(),
            output: out.to_string(),
            proc_name: Some("draw".into()),
            proc_hash: Some("h".into()),
            reason: "art or non-ascii".into(),
            ..Default::default()
        }
    }

    #[test]
    fn insert_list_get_round_trip() {
        let (_d, s) = tmp();
        let id = s.insert(entry("out one", now())).unwrap();
        assert_eq!(id.len(), 8);
        assert_eq!(s.list().len(), 1);
        let e = s.get(&id).unwrap();
        assert_eq!(e.output, "out one");
        assert_eq!(e.proc_hash.as_deref(), Some("h"));
        assert_eq!(e.reason, "art or non-ascii");
    }

    /// The same output withheld twice is ONE entry: its id stays (an open
    /// question about it still answers), the newest request's details win,
    /// and the first post's ids are KEPT — a delete must find both posts.
    #[test]
    fn the_same_output_twice_is_one_entry_that_remembers_both_posts() {
        let (_d, s) = tmp();
        let first = s.insert(entry("same output", now() - 10)).unwrap();
        s.add_post(
            &first,
            PostIds {
                post_uris: vec!["at://bot/post/1".into()],
                blobs: Some(("cidh1".into(), "cidm1".into())),
                link: Some("https://site/t/short1".into()),
                short_id: Some("short1".into()),
                record_rkey: Some("rkey1".into()),
            },
        )
        .unwrap();
        s.set_no_ask(&first, true).unwrap();
        let mut again = entry("same output", now());
        again.request_msg_id = "at://x/post/2".into();
        let second = s.insert(again).unwrap();
        assert_eq!(second, first, "one entry, one id");
        s.add_post(
            &second,
            PostIds {
                post_uris: vec!["at://bot/post/2".into()],
                short_id: Some("short2".into()),
                record_rkey: Some("rkey2".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let e = s.get(&first).unwrap();
        assert_eq!(s.list().len(), 1);
        assert_eq!(e.request_msg_id, "at://x/post/2", "the newest request");
        assert_eq!(e.post_uris, vec!["at://bot/post/1", "at://bot/post/2"]);
        assert_eq!(e.short_ids, vec!["short1", "short2"]);
        assert_eq!(e.record_rkeys, vec!["rkey1", "rkey2"]);
        assert!(e.no_ask, "a 'stop asking' survives a repeat");
    }

    #[test]
    fn an_entry_older_than_seven_days_is_dropped() {
        let (_d, s) = tmp();
        s.insert(entry("old", now() - 8 * 24 * 3600)).unwrap();
        let new = s.insert(entry("new", now())).unwrap();
        let l = s.list();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].id, new);
    }

    /// Two handles on one file see each other at once: no handle keeps a
    /// copy. The old snapshot store let `act` save an entry back after
    /// another approval removed it (asked again, reposted twice).
    #[test]
    fn a_removal_is_never_undone_by_another_writer() {
        let (_d, a) = tmp();
        let b = WithheldStore::at(a.path.clone());
        let one = a.insert(entry("one", now())).unwrap();
        let two = a.insert(entry("two", now())).unwrap();
        a.remove(&one).unwrap();
        b.remove(&two).unwrap();
        assert!(a.list().is_empty(), "both removals stand: {:?}", a.list());
        let three = b.insert(entry("three", now())).unwrap();
        a.update(&three, |e| e.reason = "x".into()).unwrap();
        assert_eq!(b.get(&three).unwrap().reason, "x");
    }

    /// Entries written by the first version (one `short_id`, one
    /// `record_rkey`, possibly null) still load.
    #[test]
    fn the_first_versions_entries_still_load() {
        let (_d, s) = tmp();
        let old = format!(
            r#"[{{"id":"51b510eb","at":{},"requester":"r","chat_id":"c","request_msg_id":"m",
                "code":"x","output":"o","proc_name":null,"proc_hash":null,"reason":"why",
                "post_uris":["at://bot/post/9"],"blob_html":"h","blob_md":"m",
                "short_id":"abc123","record_rkey":"rk1","no_ask":true}},
               {{"id":"1bcd224b","at":{},"requester":"r","chat_id":"c","request_msg_id":"m",
                "code":"x","output":"p","proc_name":null,"proc_hash":null,"reason":"why",
                "post_uris":[],"blob_html":null,"blob_md":null,
                "short_id":null,"record_rkey":null,"no_ask":false}}]"#,
            now(),
            now()
        );
        std::fs::write(&s.path, old).unwrap();
        let e = s.get("51b510eb").expect("loads");
        assert_eq!(
            e.kind,
            PostKind::Withheld,
            "every old entry was a withheld one"
        );
        assert_eq!(e.short_ids, vec!["abc123"]);
        assert_eq!(e.record_rkeys, vec!["rk1"]);
        assert!(e.no_ask);
        let e = s.get("1bcd224b").expect("loads");
        assert!(e.short_ids.is_empty() && e.record_rkeys.is_empty());
    }

    /// The owner names a post by whatever they have in front of them —
    /// the link their client shows, the short link, the bare rkey — and
    /// the entry is found. The first version took only the 8-char id,
    /// which no post shows.
    #[test]
    fn an_entry_is_found_by_anything_the_owner_can_see() {
        let (_d, s) = tmp();
        let mut e = entry("found output", now());
        e.request_msg_id = "at://did:plc:req/app.bsky.feed.post/3reqrkeyaaaa#bafyreq".into();
        let id = s.insert(e).unwrap();
        s.insert(entry("another output", now())).unwrap();
        s.add_post(
            &id,
            PostIds {
                post_uris: vec!["at://did:plc:bot/app.bsky.feed.post/3mxc3gudjowwr".into()],
                blobs: Some(("bafyhtml".into(), "bafymd".into())),
                link: Some("https://7778777.online/t/eigb36ixy4stmi64".into()),
                short_id: Some("eigb36ixy4stmi64".into()),
                record_rkey: Some("3mxc3gtyjlowr".into()),
            },
        )
        .unwrap();
        for t in [
            id.as_str(),
            "https://impro.social/profile/jackma.veles.7778777.online/post/3mxc3gudjowwr",
            "https://bsky.app/profile/did:plc:bot/post/3mxc3gudjowwr/",
            "<https://bsky.app/profile/did:plc:bot/post/3mxc3gudjowwr?ref=x>",
            "at://did:plc:bot/app.bsky.feed.post/3mxc3gudjowwr",
            "3mxc3gudjowwr",
            "https://7778777.online/t/eigb36ixy4stmi64",
            "eigb36ixy4stmi64",
            "https://7778777.online/tcl/?html=bafyhtml&md=bafymd&did=did:plc:bot",
            "https://impro.social/profile/someone.example/post/3reqrkeyaaaa",
        ] {
            assert_eq!(s.find(t).map(|e| e.id), Some(id.clone()), "found by {t}");
        }
        for t in [
            "",
            "3nothingthere",
            "https://impro.social/profile/jackma.veles.7778777.online",
            "at://did:plc:bot/online.7778777.tcl.output/3mxc3gudjowwr",
            "https://7778777.online/t/",
        ] {
            assert!(s.find(t).is_none(), "{t:?} names nothing");
        }
    }

    /// An inline post and a withheld one of the same text are two entries:
    /// merging them would put an inline post under a question, and a
    /// withheld one out of reach of it.
    #[test]
    fn the_same_output_of_two_kinds_is_two_entries() {
        let (_d, s) = tmp();
        let w = s.insert(entry("same text", now())).unwrap();
        let mut i = entry("same text", now());
        i.kind = PostKind::Inline;
        let i = s.insert(i).unwrap();
        assert_ne!(w, i);
        assert_eq!(s.get(&w).unwrap().kind, PostKind::Withheld);
        assert_eq!(s.get(&i).unwrap().kind, PostKind::Inline);
    }

    #[test]
    fn a_post_link_names_the_post_in_the_owners_client() {
        assert_eq!(
            post_link("at://did:plc:bot/app.bsky.feed.post/3mxc3gudjowwr").as_deref(),
            Some("https://impro.social/profile/did:plc:bot/post/3mxc3gudjowwr")
        );
        assert_eq!(
            post_link("at://did:plc:bot/online.7778777.tcl.output/x"),
            None
        );
        assert_eq!(post_link("https://example.com"), None);
    }

    #[test]
    fn remove_returns_and_drops_the_entry() {
        let (_d, s) = tmp();
        let id = s.insert(entry("out", now())).unwrap();
        let e = s.remove(&id).unwrap();
        assert_eq!(e.id, id);
        assert!(s.get(&id).is_none());
        assert!(s.remove(&id).is_err(), "a second removal says so");
    }

    #[test]
    fn ids_are_unique_across_inserts() {
        let (_d, s) = tmp();
        let mut ids = std::collections::HashSet::new();
        for i in 0..8 {
            let id = s.insert(entry(&format!("out {i}"), now())).unwrap();
            assert!(ids.insert(id), "unique");
        }
    }
}
