//! Bluesky rich text for `/tcl` (USER 2026-10-10: "special tcl variables
//! and procs to manipulate posts and facets at atproto/bsky level").
//!
//! Two directions, both plain data:
//!
//! - IN — [`post_context`]: the post a mention arrived in, flattened for
//!   Tcl. slopdrop sets it as `$::bsky::post` (`tcl/bsky.tcl` in the
//!   slopdrop repo documents every key): facets become `{start end type
//!   value}` with the record's UTF-8 BYTE offsets, an embed's images become
//!   `{url alt}` pairs on the public CDN.
//! - OUT — [`Spec`]: what the evaluation recorded with `bsky::link`,
//!   `bsky::tag`, `bsky::mention`, `bsky::quote`, `bsky::card`,
//!   `bsky::image` and `bsky::lang`, already validated by the slopdrop
//!   module (who may be mentioned, how many tags, images owner-only) and
//!   carried on the reply's `meta["bsky.spec"]`; with the text art the
//!   module drew as PNGs and the self-labels it decided
//!   (PLAN-slopdrop-art.md P3). [`place_spans`] turns the
//!   recorded spans into facets on the FINAL text of each post of the
//!   thread: the procs returned visible text, so a span is found by its
//!   text, in recorded order, and its byte offsets are computed from the
//!   post being sent — never carried over from the interpreter, where
//!   string operations count characters.

use std::collections::HashMap;

use serde_json::{json, Value};

/// The public CDN URL of a post's image blob.
pub fn cdn_image_url(did: &str, cid: &str) -> String {
    format!("https://cdn.bsky.app/img/feed_fullsize/plain/{did}/{cid}@jpeg")
}

fn s(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or_default().to_string()
}

/// An image's blob CID: `image.ref.$link`, or the legacy `image.cid`.
pub fn image_cid(image: &Value) -> String {
    let blob = image.get("image").unwrap_or(&Value::Null);
    let link = s(blob.get("ref").and_then(|r| r.get("$link")));
    if link.is_empty() {
        s(blob.get("cid"))
    } else {
        link
    }
}

/// The `images` array of an embed: a plain images embed, or the media of
/// a quote-with-media.
pub fn embed_images(embed: &Value) -> &[Value] {
    let t = embed.get("$type").and_then(Value::as_str).unwrap_or("");
    let images = match t {
        "app.bsky.embed.images" => embed.get("images"),
        "app.bsky.embed.recordWithMedia" => embed.get("media").and_then(|m| m.get("images")),
        _ => None,
    };
    images
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The post as Tcl reads it (`$::bsky::post`). Keys with nothing to say
/// are left out, so `dict exists` answers the question a proc asks.
pub fn post_context(
    uri: &str,
    cid: &str,
    did: &str,
    handle: Option<&str>,
    record: &Value,
) -> Value {
    let mut post = serde_json::Map::new();
    post.insert("uri".into(), json!(uri));
    if !cid.is_empty() {
        post.insert("cid".into(), json!(cid));
    }
    post.insert("text".into(), json!(s(record.get("text"))));
    post.insert("did".into(), json!(did));
    if let Some(h) = handle.filter(|h| !h.is_empty()) {
        post.insert("handle".into(), json!(h));
    }
    let created = s(record.get("createdAt"));
    if !created.is_empty() {
        post.insert("created".into(), json!(created));
    }
    if let Some(langs) = record.get("langs").and_then(Value::as_array) {
        let l: Vec<&str> = langs.iter().filter_map(Value::as_str).collect();
        if !l.is_empty() {
            post.insert("langs".into(), json!(l));
        }
    }
    let mut facets = Vec::new();
    for f in record
        .get("facets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let idx = f.get("index").unwrap_or(&Value::Null);
        let (Some(start), Some(end)) = (
            idx.get("byteStart").and_then(Value::as_u64),
            idx.get("byteEnd").and_then(Value::as_u64),
        ) else {
            continue;
        };
        for feat in f
            .get("features")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let t = feat.get("$type").and_then(Value::as_str).unwrap_or("");
            let (kind, value) = match t.rsplit('#').next().unwrap_or("") {
                "mention" => ("mention", s(feat.get("did"))),
                "link" => ("link", s(feat.get("uri"))),
                "tag" => ("tag", s(feat.get("tag"))),
                _ => continue,
            };
            facets.push(json!({"start": start, "end": end, "type": kind, "value": value}));
        }
    }
    if !facets.is_empty() {
        post.insert("facets".into(), Value::Array(facets));
    }
    if let Some(embed) = record.get("embed") {
        let t = embed.get("$type").and_then(Value::as_str).unwrap_or("");
        let mut e = serde_json::Map::new();
        e.insert(
            "type".into(),
            json!(t.strip_prefix("app.bsky.embed.").unwrap_or(t)),
        );
        let images: Vec<Value> = embed_images(embed)
            .iter()
            .map(|im| json!([cdn_image_url(did, &image_cid(im)), s(im.get("alt"))]))
            .collect();
        if !images.is_empty() {
            e.insert("images".into(), Value::Array(images));
        }
        if let Some(ext) = embed.get("external") {
            e.insert("uri".into(), json!(s(ext.get("uri"))));
            e.insert("title".into(), json!(s(ext.get("title"))));
            e.insert("description".into(), json!(s(ext.get("description"))));
        }
        // A quote: `embed.record.uri`, or `embed.record.record.uri` when
        // it carries media too.
        let rec = embed.get("record");
        let quoted = rec
            .and_then(|r| r.get("uri"))
            .or_else(|| rec.and_then(|r| r.get("record")).and_then(|r| r.get("uri")));
        if let Some(q) = quoted.and_then(Value::as_str) {
            e.insert("record".into(), json!(q));
        }
        post.insert("embed".into(), Value::Object(e));
    }
    if let Some(reply) = record.get("reply") {
        post.insert(
            "reply".into(),
            json!({
                "root": s(reply.get("root").and_then(|r| r.get("uri"))),
                "parent": s(reply.get("parent").and_then(|r| r.get("uri"))),
            }),
        );
    }
    Value::Object(post)
}

// ── the reply spec ──────────────────────────────────────────────────

/// What a recorded span means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanKind {
    Link(String),
    Tag(String),
    /// A handle, resolved to a DID at send time.
    Mention(String),
}

/// One text span the reply should carry as a facet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub kind: SpanKind,
}

/// A link card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub uri: String,
    pub title: String,
    pub description: String,
}

/// A picture the slopdrop module drew (text art), ready to upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Art {
    pub png: Vec<u8>,
    pub alt: String,
    pub width: u32,
    pub height: u32,
}

/// The self-label values a post may carry: the content warnings Bluesky's
/// app shows. Anything else in a spec is dropped.
pub const SELF_LABELS: [&str; 4] = ["porn", "sexual", "nudity", "graphic-media"];

/// Bluesky's limit on one image blob.
pub const MAX_IMAGE_BYTES: usize = 1_000_000;

/// The reply spec, as the slopdrop module validated it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    pub spans: Vec<Span>,
    pub quote: Option<String>,
    pub card: Option<Card>,
    /// `(url, alt)`; an empty alt is written by the vision model.
    pub images: Vec<(String, String)>,
    pub langs: Vec<String>,
    /// Drawn pictures, in order: they come before `images`.
    pub art: Vec<Art>,
    /// Self-labels for every post of the reply.
    pub labels: Vec<String>,
}

impl Spec {
    /// Parse `meta["bsky.spec"]`: a JSON array of flat string objects.
    /// Tolerant by design — an unknown or malformed record is skipped, a
    /// bad document is an empty spec — because the post must go out
    /// either way.
    pub fn parse(json_text: &str) -> Spec {
        let mut spec = Spec::default();
        let Ok(Value::Array(records)) = serde_json::from_str::<Value>(json_text) else {
            return spec;
        };
        for r in &records {
            let f = |k: &str| s(r.get(k));
            match f("kind").as_str() {
                "link" if !f("text").is_empty() && !f("uri").is_empty() => spec.spans.push(Span {
                    text: f("text"),
                    kind: SpanKind::Link(f("uri")),
                }),
                "tag" if !f("text").is_empty() && !f("tag").is_empty() => spec.spans.push(Span {
                    text: f("text"),
                    kind: SpanKind::Tag(f("tag")),
                }),
                "mention" if !f("text").is_empty() && !f("handle").is_empty() => {
                    spec.spans.push(Span {
                        text: f("text"),
                        kind: SpanKind::Mention(f("handle")),
                    })
                }
                "quote" if spec.quote.is_none() && !f("uri").is_empty() => {
                    spec.quote = Some(f("uri"))
                }
                "card" if spec.card.is_none() && !f("uri").is_empty() => {
                    spec.card = Some(Card {
                        uri: f("uri"),
                        title: f("title"),
                        description: f("description"),
                    })
                }
                "image" if !f("uri").is_empty() => spec.images.push((f("uri"), f("alt"))),
                "lang" if !f("code").is_empty() => spec.langs.push(f("code")),
                "art" if spec.art.len() < 4 => {
                    use base64::Engine as _;
                    let png = base64::engine::general_purpose::STANDARD
                        .decode(f("png"))
                        .unwrap_or_default();
                    // A PNG, and one Bluesky takes as it is: the module
                    // drew it under the limit and it is never re-encoded.
                    if png.starts_with(b"\x89PNG\r\n\x1a\n") && png.len() <= MAX_IMAGE_BYTES {
                        spec.art.push(Art {
                            png,
                            alt: f("alt"),
                            width: f("w").parse().unwrap_or(0),
                            height: f("h").parse().unwrap_or(0),
                        });
                    }
                }
                "label" => {
                    let v = f("val");
                    if SELF_LABELS.contains(&v.as_str()) && !spec.labels.contains(&v) {
                        spec.labels.push(v);
                    }
                }
                _ => {}
            }
        }
        spec
    }

    /// The handles whose DIDs the mentions need.
    pub fn mention_handles(&self) -> Vec<String> {
        self.spans
            .iter()
            .filter_map(|s| match &s.kind {
                SpanKind::Mention(h) => Some(h.clone()),
                _ => None,
            })
            .collect()
    }
}

/// The posts of a reply: split at each `bsky::newpost` mark (a form feed
/// in the text), then each part at the post limit.
pub fn split_parts(body: &str) -> Vec<String> {
    body.split('\u{c}')
        .filter(|p| !p.trim().is_empty())
        .flat_map(crate::post::split_thread)
        .filter(|p| !p.is_empty())
        .collect()
}

/// The facets one post of the reply carries from the spec. Spans are
/// taken in recorded order, each searched from the end of the last one
/// found in this post, so a repeated text lands on the right occurrence;
/// a span not in this post stays for the next (`placed` carries which
/// are used across the thread). A mention whose handle did not resolve
/// gets no facet: it stays plain text, never a dead link.
pub fn place_spans(
    chunk: &str,
    spec: &Spec,
    placed: &mut [bool],
    dids: &HashMap<String, String>,
) -> Vec<Value> {
    let mut out = Vec::new();
    let mut from = 0usize;
    for (i, span) in spec.spans.iter().enumerate() {
        if placed.get(i).copied().unwrap_or(true) {
            continue;
        }
        let Some(rel) = chunk.get(from..).and_then(|rest| rest.find(&span.text)) else {
            continue;
        };
        let start = from + rel;
        let end = start + span.text.len();
        let feature = match &span.kind {
            SpanKind::Link(uri) => json!({"$type": "app.bsky.richtext.facet#link", "uri": uri}),
            SpanKind::Tag(tag) => json!({"$type": "app.bsky.richtext.facet#tag", "tag": tag}),
            SpanKind::Mention(h) => match dids.get(&h.to_ascii_lowercase()) {
                Some(did) => json!({"$type": "app.bsky.richtext.facet#mention", "did": did}),
                None => {
                    placed[i] = true;
                    from = end;
                    continue;
                }
            },
        };
        out.push(json!({"index": {"byteStart": start, "byteEnd": end}, "features": [feature]}));
        placed[i] = true;
        from = end;
    }
    out
}

/// The spec's facets plus the automatic link facets that do not overlap
/// them (a spec link over a bare URL wins; two facets on one byte range
/// are invalid).
pub fn merge_facets(mut placed: Vec<Value>, auto: Value) -> Value {
    let range = |f: &Value| {
        let i = f.get("index").unwrap_or(&Value::Null);
        (
            i.get("byteStart").and_then(Value::as_u64).unwrap_or(0),
            i.get("byteEnd").and_then(Value::as_u64).unwrap_or(0),
        )
    };
    let taken: Vec<(u64, u64)> = placed.iter().map(range).collect();
    for f in auto.as_array().into_iter().flatten() {
        let (a, b) = range(f);
        if taken.iter().all(|&(x, y)| b <= x || a >= y) {
            placed.push(f.clone());
        }
    }
    if placed.is_empty() {
        Value::Null
    } else {
        Value::Array(placed)
    }
}

/// A quote target as `(repo, rkey)`: an `at://…/app.bsky.feed.post/…`
/// uri, or a `https://bsky.app/profile/<handle-or-did>/post/<rkey>` link.
pub fn quote_target(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("at://") {
        let mut it = rest.split('/');
        let (repo, coll, rkey) = (it.next()?, it.next()?, it.next()?);
        return (coll == "app.bsky.feed.post" && !repo.is_empty() && !rkey.is_empty())
            .then(|| (repo.to_string(), rkey.to_string()));
    }
    let rest = s
        .strip_prefix("https://bsky.app/profile/")
        .or_else(|| s.strip_prefix("http://bsky.app/profile/"))?;
    let mut it = rest.split('/');
    let (repo, post, rkey) = (it.next()?, it.next()?, it.next()?);
    let rkey = rkey.split(['?', '#']).next().unwrap_or("");
    (post == "post" && !repo.is_empty() && !rkey.is_empty())
        .then(|| (repo.to_string(), rkey.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_post_flattens_for_tcl_with_byte_offsets_and_cdn_images() {
        let rec = json!({
            "$type": "app.bsky.feed.post",
            "text": "hi @a.test 🙂 #tcl",
            "createdAt": "2026-10-10T20:00:00Z",
            "langs": ["en"],
            "facets": [
                {"index": {"byteStart": 3, "byteEnd": 10},
                 "features": [{"$type": "app.bsky.richtext.facet#mention", "did": "did:plc:a"}]},
                {"index": {"byteStart": 16, "byteEnd": 20},
                 "features": [{"$type": "app.bsky.richtext.facet#tag", "tag": "tcl"}]},
            ],
            "embed": {"$type": "app.bsky.embed.images",
                      "images": [{"alt": "", "image": {"ref": {"$link": "bafyimg"}, "mimeType": "image/jpeg"}}]},
            "reply": {"root": {"uri": "at://r", "cid": "c1"}, "parent": {"uri": "at://p", "cid": "c2"}},
        });
        let p = post_context(
            "at://x/app.bsky.feed.post/1",
            "bafy",
            "did:plc:x",
            Some("x.test"),
            &rec,
        );
        assert_eq!(p["text"], "hi @a.test 🙂 #tcl");
        assert_eq!(p["handle"], "x.test");
        assert_eq!(
            p["facets"][0],
            json!({"start": 3, "end": 10, "type": "mention", "value": "did:plc:a"})
        );
        assert_eq!(p["facets"][1]["type"], "tag");
        assert_eq!(p["embed"]["type"], "images");
        assert_eq!(
            p["embed"]["images"][0],
            json!([
                "https://cdn.bsky.app/img/feed_fullsize/plain/did:plc:x/bafyimg@jpeg",
                ""
            ])
        );
        assert_eq!(p["reply"], json!({"root": "at://r", "parent": "at://p"}));
        assert_eq!(p["langs"], json!(["en"]));
    }

    #[test]
    fn a_spec_parses_tolerantly_and_keeps_one_quote_and_one_card() {
        let spec = Spec::parse(
            r##"[{"kind":"link","text":"docs","uri":"https://d.test"},
                {"kind":"tag","text":"#tcl","tag":"tcl"},
                {"kind":"mention","text":"@b.test","handle":"b.test"},
                {"kind":"quote","uri":"at://q/app.bsky.feed.post/1"},
                {"kind":"quote","uri":"at://second/app.bsky.feed.post/2"},
                {"kind":"card","uri":"https://c.test","title":"","description":""},
                {"kind":"image","uri":"https://i.test/a.png","alt":""},
                {"kind":"lang","code":"de"},
                {"kind":"nonsense"}, 7, {"kind":"link","text":"","uri":"x"}]"##,
        );
        assert_eq!(spec.spans.len(), 3);
        assert_eq!(spec.quote.as_deref(), Some("at://q/app.bsky.feed.post/1"));
        assert!(spec.card.is_some());
        assert_eq!(spec.images.len(), 1);
        assert_eq!(spec.langs, ["de"]);
        assert_eq!(Spec::parse("not json"), Spec::default());
    }

    /// A drawn picture rides as base64 PNG bytes with its size; only a
    /// PNG under Bluesky's limit is kept, at most four, and only the
    /// self-label values.
    #[test]
    fn a_spec_carries_drawn_pictures_and_self_labels() {
        use base64::Engine as _;
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let png = b"\x89PNG\r\n\x1a\nrest".to_vec();
        let big = [png.clone(), vec![0; MAX_IMAGE_BYTES]].concat();
        let mut recs = vec![
            json!({"kind": "art", "png": b64(&png), "alt": "a cow", "w": "800", "h": "400"}),
            json!({"kind": "art", "png": b64(b"GIF89a"), "alt": "not a png"}),
            json!({"kind": "art", "png": "%%%", "alt": "not base64"}),
            json!({"kind": "art", "png": b64(&big), "alt": "too big"}),
            json!({"kind": "label", "val": "nudity"}),
            json!({"kind": "label", "val": "nudity"}),
            json!({"kind": "label", "val": "!takedown"}),
        ];
        for _ in 0..4 {
            recs.push(json!({"kind": "art", "png": b64(&png), "alt": "more"}));
        }
        let spec = Spec::parse(&Value::Array(recs).to_string());
        assert_eq!(spec.art.len(), 4, "four pictures, the bad ones dropped");
        assert!(
            spec.art[1..].iter().all(|a| a.alt == "more"),
            "{:?}",
            spec.art
        );
        assert_eq!(
            spec.art[0],
            Art {
                png,
                alt: "a cow".into(),
                width: 800,
                height: 400
            }
        );
        assert_eq!(spec.labels, ["nudity"]);
    }

    #[test]
    fn spans_land_on_the_right_bytes_across_posts_and_in_order() {
        let spec = Spec::parse(
            r##"[{"kind":"link","text":"here","uri":"https://one.test"},
                {"kind":"link","text":"here","uri":"https://two.test"},
                {"kind":"mention","text":"@b.test","handle":"b.test"},
                {"kind":"mention","text":"@ghost.test","handle":"ghost.test"},
                {"kind":"tag","text":"#tcl","tag":"tcl"}]"##,
        );
        let dids: HashMap<String, String> =
            [("b.test".to_string(), "did:plc:b".to_string())].into();
        let mut placed = vec![false; spec.spans.len()];
        // Post one: an emoji before the spans, so char and byte counts differ.
        let one = "🙂 first here, then here";
        let f1 = place_spans(one, &spec, &mut placed, &dids);
        assert_eq!(f1.len(), 2, "{f1:?}");
        let (s0, e0) = (
            f1[0]["index"]["byteStart"].as_u64().unwrap() as usize,
            f1[0]["index"]["byteEnd"].as_u64().unwrap() as usize,
        );
        assert_eq!(&one[s0..e0], "here");
        assert_eq!(f1[0]["features"][0]["uri"], "https://one.test");
        assert_eq!(
            f1[1]["features"][0]["uri"], "https://two.test",
            "the second occurrence"
        );
        assert!(
            f1[1]["index"]["byteStart"].as_u64().unwrap()
                > f1[0]["index"]["byteStart"].as_u64().unwrap()
        );
        // Post two: the rest.
        let two = "ask @b.test or @ghost.test #tcl";
        let f2 = place_spans(two, &spec, &mut placed, &dids);
        assert_eq!(
            f2.len(),
            2,
            "the unresolved mention stays plain text: {f2:?}"
        );
        assert_eq!(f2[0]["features"][0]["did"], "did:plc:b");
        assert_eq!(f2[1]["features"][0]["tag"], "tcl");
        assert!(placed.iter().all(|p| *p));
    }

    #[test]
    fn a_spec_link_over_a_bare_url_wins_and_the_rest_stay() {
        let text = "see https://a.test and https://b.test";
        let spec =
            Spec::parse(r##"[{"kind":"link","text":"https://a.test","uri":"https://a.test/x"}]"##);
        let mut placed = vec![false];
        let mine = place_spans(text, &spec, &mut placed, &HashMap::new());
        let auto = crate::post::build_facets(
            text,
            &[],
            &["https://a.test".into(), "https://b.test".into()],
        );
        let merged = merge_facets(mine, auto);
        let arr = merged.as_array().unwrap();
        assert_eq!(arr.len(), 2, "{arr:?}");
        assert_eq!(arr[0]["features"][0]["uri"], "https://a.test/x");
        assert_eq!(arr[1]["features"][0]["uri"], "https://b.test");
    }

    #[test]
    fn newpost_marks_split_the_reply() {
        assert_eq!(
            split_parts("one\u{c}two\u{c}\u{c}three"),
            ["one", "two", "three"]
        );
        assert_eq!(split_parts("plain"), ["plain"]);
    }

    #[test]
    fn quote_targets_read_from_both_spellings() {
        assert_eq!(
            quote_target("at://did:plc:a/app.bsky.feed.post/3k"),
            Some(("did:plc:a".into(), "3k".into()))
        );
        assert_eq!(
            quote_target("https://bsky.app/profile/a.test/post/3k?x=1"),
            Some(("a.test".into(), "3k".into()))
        );
        assert_eq!(quote_target("at://did:plc:a/app.bsky.feed.like/3k"), None);
        assert_eq!(quote_target("https://example.com/post/3k"), None);
    }
}
