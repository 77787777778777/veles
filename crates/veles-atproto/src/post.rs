//! Assembling an `app.bsky.feed.post` record: text, byte-offset facets
//! (mentions + links), reply strong-refs, and image embeds — plus the
//! 300-grapheme split into a reply chain.
//!
//! The two correctness hazards this module owns:
//! 1. Facet `byteStart`/`byteEnd` are BYTE offsets into the UTF-8 text,
//!    not char indices. A miscount corrupts the post.
//! 2. A post is capped at 300 graphemes; a long reply must split into a
//!    self-replying thread, each link a valid reply to the previous.

use anyhow::Result;

/// Bluesky's per-post grapheme ceiling.
pub const MAX_POST_GRAPHEMES: usize = 300;

/// A mention the model's text produced, resolved to a DID.
#[derive(Debug, Clone)]
pub struct Mention {
    pub handle: String,
    pub did: String,
}

/// Build the facets array for a text: a `app.bsky.richtext.facet#mention`
/// for each resolved mention, a `#link` for each URL. Byte offsets are
/// computed from the UTF-8 text itself (never from a char count).
///
/// A handle that failed to resolve is simply absent — it renders as
/// plain text in the post, never a dead link. This runs on the final
/// text; offsets are found by searching the text for each token's bytes.
pub fn build_facets(text: &str, mentions: &[Mention], urls: &[String]) -> serde_json::Value {
    let bytes = text.as_bytes();
    let mut facets: Vec<serde_json::Value> = Vec::new();

    for m in mentions {
        let token = format!("@{}", m.handle);
        if let Some(start) = find_subslice(bytes, token.as_bytes()) {
            facets.push(serde_json::json!({
                "index": { "byteStart": start, "byteEnd": start + token.len() },
                "features": [{ "$type": "app.bsky.richtext.facet#mention", "did": m.did }],
            }));
        }
    }
    for u in urls {
        if let Some(start) = find_subslice(bytes, u.as_bytes()) {
            facets.push(serde_json::json!({
                "index": { "byteStart": start, "byteEnd": start + u.len() },
                "features": [{ "$type": "app.bsky.richtext.facet#link", "uri": u }],
            }));
        }
    }
    if facets.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Array(facets)
    }
}

/// The reply block: strong refs to the thread root and the immediate
/// parent. `root`/`parent` are `(uri, cid)`.
pub fn build_reply(root: (String, String), parent: (String, String)) -> serde_json::Value {
    serde_json::json!({
        "root": { "uri": root.0, "cid": root.1 },
        "parent": { "uri": parent.0, "cid": parent.1 },
    })
}

/// An images embed from already-uploaded blob objects + their alt texts.
pub fn build_image_embed(images: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({ "$type": "app.bsky.embed.images", "images": images })
}

/// One `app.bsky.embed.images#image`: the uploaded blob, its alt text, and
/// its `aspectRatio` when known.
pub fn image_entry(
    blob: serde_json::Value,
    alt: &str,
    ratio: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut e = serde_json::json!({ "alt": alt, "image": blob });
    if let Some(r) = ratio {
        e["aspectRatio"] = r;
    }
    e
}

/// An image's `aspectRatio` (`app.bsky.embed.defs#aspectRatio`), read from
/// its header. Without it the app lays the picture out at a guessed shape
/// and crops it; a tall drawing lost its bottom half that way. `None` for
/// bytes no decoder here knows.
pub fn aspect_ratio(bytes: &[u8]) -> Option<serde_json::Value> {
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    ratio_of(w, h)
}

/// `{width, height}` for a size, `None` for a zero side.
pub fn ratio_of(w: u32, h: u32) -> Option<serde_json::Value> {
    (w > 0 && h > 0).then(|| serde_json::json!({ "width": w, "height": h }))
}

/// Self-label a post (`com.atproto.label.defs#selfLabels`): the content
/// warning readers opt into. Nothing for no labels.
pub fn self_label(rec: &mut serde_json::Value, labels: &[String]) {
    if labels.is_empty() {
        return;
    }
    let values: Vec<serde_json::Value> = labels
        .iter()
        .map(|v| serde_json::json!({ "val": v }))
        .collect();
    rec["labels"] = serde_json::json!({
        "$type": "com.atproto.label.defs#selfLabels",
        "values": values,
    });
}

/// Split a long reply into ≤300-grapheme chunks that thread as a reply
/// chain. Splits on a whitespace boundary at or before the limit; a token
/// longer than the limit is hard-cut. Grapheme count is approximated by
/// char count (a refinement, if needed, is the `unicode-segmentation`
/// crate — char count is the conservative upper bound for CJK/ASCII).
///
/// Returns one string per post in the chain, in order.
pub fn split_thread(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.chars().count() <= MAX_POST_GRAPHEMES {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut rest = text;
    while rest.chars().count() > MAX_POST_GRAPHEMES {
        // Take the first MAX chars, then back off to the last whitespace.
        let byte_limit = char_to_byte_idx(rest, MAX_POST_GRAPHEMES);
        let window = &rest[..byte_limit];
        let cut = window
            .rfind(char::is_whitespace)
            .filter(|&i| i > 0)
            .unwrap_or(byte_limit);
        out.push(rest[..cut].trim_end().to_string());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

/// The byte index of the `n`-th char boundary (n must be ≤ char count).
fn char_to_byte_idx(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

/// First index of `needle` in `hay`, byte-wise.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Assemble one post record ready for `create_post`.
pub fn assemble_post(
    text: &str,
    langs: &[String],
    facets: serde_json::Value,
    reply: Option<serde_json::Value>,
    embed: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut rec = serde_json::json!({
        "$type": "app.bsky.feed.post",
        "text": text,
        "createdAt": now_iso(),
    });
    if !langs.is_empty() {
        rec["langs"] = serde_json::json!(langs);
    }
    if !facets.is_null() {
        rec["facets"] = facets;
    }
    if let Some(r) = reply {
        rec["reply"] = r;
    }
    if let Some(e) = embed {
        rec["embed"] = e;
    }
    rec
}

/// RFC 3339 UTC with a `Z`, the atproto-preferred timestamp.
fn now_iso() -> String {
    // chrono is already in the tree via the workspace; keep it simple.
    let now = std::time::SystemTime::now();
    let d = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // civil-from-days, no external dep for a timestamp we only format once.
    format_epoch(d.as_secs())
}

fn format_epoch(secs: u64) -> String {
    format_epoch_pub(secs)
}

/// The public spelling, for the notification lane's seen mark.
pub fn format_epoch_pub(secs: u64) -> String {
    // Days → y/m/d (Howard Hinnant's algorithm), secs-of-day → h:m:s.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let yr = if mo <= 2 { y + 1 } else { y };
    format!("{yr:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Resolve reply strong-refs for a parent at-uri, via the PDS: a reply's
/// root is the parent's own root if it has one, else the parent itself.
pub async fn reply_refs_for(
    client: &super::pds::PdsClient,
    parent_uri: &str,
) -> Result<((String, String), (String, String))> {
    let (repo, collection, rkey) = parse_at_uri(parent_uri)?;
    // The reply target almost always lives on a repo we do NOT host (a
    // stranger's or the operator's post on bsky.social). Our PDS's
    // getRecord only serves hosted repos, so resolve through the AppView;
    // fall back to the local repo only if the AppView can't (a record on
    // our own PDS that the AppView hasn't indexed yet, e.g. the bot's own
    // just-created post in a thread continuation).
    let parent = match client.get_record_appview(&repo, &collection, &rkey).await {
        Ok(p) => p,
        Err(e) => {
            tracing::info!(
                "atproto: appview getRecord for {parent_uri} failed ({e:#}); trying local repo"
            );
            client.get_record(&repo, &collection, &rkey).await?
        }
    };
    let p_uri = parent
        .get("uri")
        .and_then(|u| u.as_str())
        .unwrap_or(parent_uri)
        .to_string();
    let p_cid = parent
        .get("cid")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    // If the parent is itself a reply, reuse its root; else parent = root.
    let root = parent
        .get("value")
        .and_then(|v| v.get("reply"))
        .and_then(|r| r.get("root"))
        .and_then(|root| {
            Some((
                root.get("uri")?.as_str()?.to_string(),
                root.get("cid")?.as_str()?.to_string(),
            ))
        })
        .unwrap_or((p_uri.clone(), p_cid.clone()));
    Ok((root, (p_uri, p_cid)))
}

/// Split `at://<repo>/<collection>/<rkey>`.
pub fn parse_at_uri(uri: &str) -> Result<(String, String, String)> {
    let rest = uri
        .strip_prefix("at://")
        .ok_or_else(|| anyhow::anyhow!("not an at-uri: {uri}"))?;
    let mut it = rest.split('/');
    let repo = it.next().unwrap_or_default().to_string();
    let collection = it.next().unwrap_or_default().to_string();
    let rkey = it.next().unwrap_or_default().to_string();
    if repo.is_empty() || collection.is_empty() {
        anyhow::bail!("malformed at-uri: {uri}");
    }
    Ok((repo, collection, rkey))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn facets_use_byte_offsets_not_char_indices() {
        // "✨ " is 4 bytes + 1 byte space before the token; an emoji before
        // a mention is the classic off-by-N. The mention starts at byte 4.
        let text = "✨ hi @bot.x";
        let mentions = vec![Mention {
            handle: "bot.x".into(),
            did: "did:plc:b".into(),
        }];
        let f = build_facets(text, &mentions, &[]);
        let arr = f.as_array().unwrap();
        let idx = &arr[0]["index"];
        let start = idx["byteStart"].as_u64().unwrap() as usize;
        let end = idx["byteEnd"].as_u64().unwrap() as usize;
        assert_eq!(&text.as_bytes()[start..end], b"@bot.x");
    }

    #[test]
    fn a_link_gets_a_link_facet_at_its_bytes() {
        let text = "see https://example.com now";
        let f = build_facets(text, &[], &["https://example.com".into()]);
        let idx = &f.as_array().unwrap()[0]["index"];
        let (s, e) = (
            idx["byteStart"].as_u64().unwrap() as usize,
            idx["byteEnd"].as_u64().unwrap() as usize,
        );
        assert_eq!(&text.as_bytes()[s..e], b"https://example.com");
        assert_eq!(
            f.as_array().unwrap()[0]["features"][0]["$type"],
            "app.bsky.richtext.facet#link"
        );
    }

    #[test]
    fn no_facets_is_null_not_an_empty_array() {
        assert!(build_facets("plain text", &[], &[]).is_null());
    }

    #[test]
    fn short_text_is_one_post() {
        assert_eq!(split_thread("hello").len(), 1);
    }

    #[test]
    fn long_text_splits_on_whitespace_and_stays_under_the_cap() {
        let long = "word ".repeat(200); // 1000 chars, spaces everywhere
        let parts = split_thread(&long);
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(
                p.chars().count() <= MAX_POST_GRAPHEMES,
                "chunk over cap: {}",
                p.chars().count()
            );
        }
        // Nothing lost: rejoined word count equals the original's.
        let rejoined_words: usize = parts.iter().map(|p| p.split_whitespace().count()).sum();
        assert_eq!(rejoined_words, 200);
    }

    #[test]
    fn a_token_longer_than_the_cap_is_hard_cut() {
        let monster = "x".repeat(900);
        let parts = split_thread(&monster);
        assert_eq!(parts.len(), 3);
        assert!(parts
            .iter()
            .all(|p| p.chars().count() <= MAX_POST_GRAPHEMES));
    }

    #[test]
    fn parse_at_uri_splits_three_parts() {
        let (r, c, k) = parse_at_uri("at://did:plc:a/app.bsky.feed.post/3k4").unwrap();
        assert_eq!(
            (r.as_str(), c.as_str(), k.as_str()),
            ("did:plc:a", "app.bsky.feed.post", "3k4")
        );
        assert!(parse_at_uri("not a uri").is_err());
    }

    /// Every image says its shape, read from its own header, and a
    /// labelled reply says its labels in the selfLabels shape.
    #[test]
    fn an_image_says_its_shape_and_a_post_its_self_labels() {
        let mut png = Vec::new();
        image::RgbImage::new(30, 12)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(aspect_ratio(&png), Some(json!({"width": 30, "height": 12})));
        assert_eq!(aspect_ratio(b"not an image"), None);
        assert_eq!(ratio_of(0, 5), None);
        let e = image_entry(json!({"$type": "blob"}), "a", aspect_ratio(&png));
        assert_eq!(e["aspectRatio"]["width"], 30);
        let mut rec = assemble_post("", &[], serde_json::Value::Null, None, None);
        self_label(&mut rec, &[]);
        assert!(rec.get("labels").is_none());
        self_label(
            &mut rec,
            &["nudity".to_string(), "graphic-media".to_string()],
        );
        assert_eq!(
            rec["labels"],
            json!({"$type": "com.atproto.label.defs#selfLabels",
                   "values": [{"val": "nudity"}, {"val": "graphic-media"}]})
        );
    }

    #[test]
    fn assemble_post_carries_reply_and_embed_and_langs() {
        let reply = build_reply(
            ("at://r".into(), "cr".into()),
            ("at://p".into(), "cp".into()),
        );
        let embed = build_image_embed(vec![image_entry(json!({"$type":"blob"}), "alt", None)]);
        let rec = assemble_post(
            "hi",
            &["en".to_string()],
            serde_json::Value::Null,
            Some(reply),
            Some(embed),
        );
        assert_eq!(rec["$type"], "app.bsky.feed.post");
        assert_eq!(rec["reply"]["root"]["uri"], "at://r");
        assert_eq!(rec["reply"]["parent"]["uri"], "at://p");
        assert_eq!(rec["embed"]["$type"], "app.bsky.embed.images");
        assert_eq!(rec["langs"][0], "en");
        assert!(
            rec.get("facets").is_none(),
            "null facets must not be written"
        );
    }

    #[test]
    fn epoch_formats_a_known_timestamp() {
        // 2023-08-07T05:31:12Z = 1691386272.
        assert_eq!(format_epoch(1691386272), "2023-08-07T05:31:12Z");
        // The unix epoch itself.
        assert_eq!(format_epoch(0), "1970-01-01T00:00:00Z");
    }
}
