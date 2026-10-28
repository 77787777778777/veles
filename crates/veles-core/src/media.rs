//! Shared MEDIA: marker parsing — the hermes convention for attaching
//! files to agent replies (`MEDIA:/absolute/path`, one per line).
//!
//! Single source of truth so every frontend parses identically:
//! telegram (send_media_items), desktop (inline image embed), TUI
//! (path notes), irc (text note). Telegram keeps a local copy for now
//! (crate boundary); consolidate if it drifts.

/// Split `MEDIA:` markers out of a reply body.
/// Returns (paths, body-without-marker-lines). Markers look like
/// `MEDIA:/absolute/path` or `MEDIA:/path with spaces.jpg`, one per
/// line; the marker line is consumed entirely.
pub fn extract_media(text: &str) -> (Vec<String>, String) {
    let mut paths = Vec::new();
    let mut kept = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("MEDIA:") {
            let path = rest.trim();
            if !path.is_empty() {
                paths.push(path.to_string());
            }
        } else {
            kept.push(line);
        }
    }
    (paths, kept.join("\n"))
}

/// Image-ish extension (renderable inline where the surface supports it).
pub fn is_image(path: &str) -> bool {
    let ext = path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
}

/// Replace MEDIA: lines with human-readable notes for text-only
/// surfaces (TUI, IRC). Missing files are flagged inline.
pub fn render_media_notes(text: &str) -> String {
    let (paths, rest) = extract_media(text);
    if paths.is_empty() {
        return text.to_string();
    }
    let mut out = rest;
    for p in paths {
        let exists = std::path::Path::new(&p).is_file();
        let note = if exists {
            format!("📎 media: {p}")
        } else {
            format!("📎 media (not found): {p}")
        };
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&note);
        out.push('\n');
    }
    out
}

// ── attachments: the same convention, in the other direction ─────────
//
// USER: "Is image/file support implemented at all? I swear qwen and
// other slopshit models always reassured me this has long been done,
// but image preview just opens my browser, no option to even attach or
// see it in chat or otherwise."
//
// Half of it was real. MODEL → USER worked: a reply carrying
// `MEDIA:/path` rendered inline. USER → MODEL did not exist at all —
// `/image` printed "image noted (use vision_analyze tool in your next
// prompt)" and attached nothing, while `/help` claimed it attached, and
// `image_url` appeared nowhere in providers.rs. The model never saw a
// pixel.
//
// The fix reuses the marker rather than inventing a second mechanism: a
// user message carries `MEDIA:/path` lines exactly as a reply does. So
// persistence is free (the marker is in the stored content), the
// desktop already renders it inline in the transcript, and the only new
// step is resolving those paths into API parts on the way out.

/// A file the user attached, resolved and ready to send.
#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    /// What the user named — a path or URL. Kept for messages and the
    /// transcript; never sent to the provider.
    pub source: String,
    pub mime: String,
    /// `data:<mime>;base64,…`, the shape `image_url` wants.
    pub data_url: String,
}

/// The largest file we will inline, measured BEFORE base64 (which adds
/// a third). Past this the provider will almost certainly reject the
/// request anyway, and a multi-megabyte blob replayed through a retry
/// loop is how a typo turns into a rate limit.
pub const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;

/// The largest non-image file we will inline as text.
pub const MAX_TEXT_ATTACHMENT_BYTES: usize = 256 * 1024;

/// Resolve one `MEDIA:` target into something a provider can read.
///
/// Images become a data URL for an `image_url` part. Anything else that
/// is readable text is inlined into the message body instead, because a
/// text file in an `image_url` is not "unsupported", it is nonsense.
#[derive(Debug, Clone)]
pub enum Resolved {
    Image(Attachment),
    Text { source: String, text: String },
    Skipped { source: String, why: String },
}

/// Turn-scoped attachment resolutions.
///
/// `attach_user_media` used to re-read and re-inline every attached file
/// from disk on EVERY ROUND of a turn — a 200 KB attachment read ten
/// times in a ten-round turn, and a file edited mid-turn silently
/// invalidating the prefix mid-flight. One cache per turn fixes both:
/// resolutions are keyed by (path, mtime), so an UNCHANGED file is read
/// once, and an edited one is picked up at the NEXT turn — the
/// documented freshness contract, at turn granularity instead of round.
#[derive(Debug, Default)]
pub struct MediaCache {
    entries: std::collections::HashMap<String, (std::time::SystemTime, Resolved)>,
    /// The session whose workspace holds sandbox-relative attachments.
    pub session_id: Option<String>,
    /// The cap applied to inlined text attachments (0 = none) — the
    /// same `[agent] tool_output_cap` tool results get.
    pub text_cap: usize,
}

impl MediaCache {
    pub fn new(text_cap: usize) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            session_id: None,
            text_cap,
        }
    }
}

pub async fn resolve(src: &str) -> Resolved {
    if src.starts_with("http://") || src.starts_with("https://") {
        return resolve_url(src).await;
    }
    let path = std::path::Path::new(src);
    if !path.is_file() {
        return Resolved::Skipped {
            source: src.to_string(),
            why: "no such file".into(),
        };
    }
    match std::fs::read(path) {
        Ok(bytes) => resolve_bytes(src, bytes),
        Err(_) => Resolved::Skipped {
            source: src.to_string(),
            why: "could not be read".into(),
        },
    }
}

fn resolve_bytes(src: &str, bytes: Vec<u8>) -> Resolved {
    let image = is_image(src);
    let cap = if image {
        MAX_ATTACHMENT_BYTES
    } else {
        MAX_TEXT_ATTACHMENT_BYTES
    };
    if bytes.len() > cap {
        return Resolved::Skipped {
            source: src.to_string(),
            why: format!(
                "{} is over the {} limit",
                human_bytes(bytes.len()),
                human_bytes(cap)
            ),
        };
    }
    if image {
        let mime = mime_for(src);
        return Resolved::Image(Attachment {
            source: src.to_string(),
            mime: mime.to_string(),
            data_url: format!("data:{mime};base64,{}", base64_encode(&bytes)),
        });
    }
    match String::from_utf8(bytes) {
        Ok(text) => Resolved::Text {
            source: src.to_string(),
            text,
        },
        // A binary that is not an image has no representation in a chat
        // message. Saying so beats sending the model 200 KB of mojibake.
        Err(_) => Resolved::Skipped {
            source: src.to_string(),
            why: "binary, and not an image".into(),
        },
    }
}

async fn resolve_url(src: &str) -> Resolved {
    // Guarded: a `MEDIA:http://…` marker is a fetch the SENDER chose,
    // and the bytes come back inlined into the model's context. The
    // path half of this marker is judged by `deliverable`; the URL half
    // is judged here.
    let resp = match crate::urlguard::get(src, std::time::Duration::from_secs(30)).await {
        Ok(r) => r,
        Err(e) => {
            return Resolved::Skipped {
                source: src.to_string(),
                why: e,
            }
        }
    };
    if !resp.status().is_success() {
        return Resolved::Skipped {
            source: src.to_string(),
            why: format!("HTTP {}", resp.status()),
        };
    }
    let mime = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/png")
        .split(';')
        .next()
        .unwrap_or("image/png")
        .trim()
        .to_string();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return Resolved::Skipped {
                source: src.to_string(),
                why: e.to_string(),
            }
        }
    };
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Resolved::Skipped {
            source: src.to_string(),
            why: format!(
                "{} is over the {} limit",
                human_bytes(bytes.len()),
                human_bytes(MAX_ATTACHMENT_BYTES)
            ),
        };
    }
    if !mime.starts_with("image/") {
        return match String::from_utf8(bytes.to_vec()) {
            Ok(text) => Resolved::Text {
                source: src.to_string(),
                text,
            },
            Err(_) => Resolved::Skipped {
                source: src.to_string(),
                why: format!("{mime} is neither an image nor text"),
            },
        };
    }
    Resolved::Image(Attachment {
        source: src.to_string(),
        mime: mime.clone(),
        data_url: format!("data:{mime};base64,{}", base64_encode(&bytes)),
    })
}

/// Resolve one `MEDIA:` target, consulting the turn's cache first.
///
/// Keyed by (path, mtime): an unchanged file is read once per turn, an
/// edited one misses the cache and is re-resolved. URLs are never
/// cached (they are rare and their freshness is the caller's problem).
async fn resolve_cached(src: &str, cache: &mut MediaCache) -> Resolved {
    if src.starts_with("http://") || src.starts_with("https://") {
        return resolve(src).await;
    }
    if let Some(session) = cache.session_id.as_deref() {
        if let Some(result) = crate::user_envs::read_session_bytes(session, src).await {
            return match result {
                Ok(bytes) => resolve_bytes(src, bytes),
                Err(e) => Resolved::Skipped {
                    source: src.to_string(),
                    why: e.to_string(),
                },
            };
        }
    }
    let mtime = std::fs::metadata(src).and_then(|m| m.modified()).ok();
    if let Some(m) = mtime {
        if let Some((cached_m, res)) = cache.entries.get(src) {
            if *cached_m == m {
                return res.clone();
            }
        }
    }
    let res = resolve(src).await;
    if let Some(m) = mtime {
        cache.entries.insert(src.to_string(), (m, res.clone()));
    }
    res
}

/// Turn every user message's `MEDIA:` markers into real attachments,
/// in place, just before the request goes out.
///
/// Done HERE rather than when the message is stored, so that reopening
/// a session re-sends the images with it and an edited file is picked
/// up (at TURN granularity — see `MediaCache`). The marker lines are
/// stripped from the text: the model gets the picture, not a path it
/// cannot open.
///
/// `cache` is owned by the turn: every round of one turn passes the
/// SAME cache, so an attachment is read from disk once, not once per
/// round.
pub async fn attach_user_media(
    messages: &mut [crate::providers::ChatMessage],
    cache: &mut MediaCache,
) {
    for m in messages.iter_mut() {
        if m.role != "user" || !m.content.contains("MEDIA:") {
            continue;
        }
        let (paths, rest) = extract_media(&m.content);
        if paths.is_empty() {
            continue;
        }
        let mut content = rest;
        for p in paths {
            match resolve_cached(&p, cache).await {
                Resolved::Image(a) => {
                    // The main model cannot see images — veles' defaults
                    // (deepseek-v4-flash, glm-5.3) reject an `image_url`
                    // part with a 400, which is exactly why "the model
                    // can't use its vision tools" looked broken. The
                    // picture rides a NOTE pointing at the vision_analyze
                    // tool, whose [vision] route CAN see it. Deliberately
                    // NOT an image_url part.
                    content.push_str(&format!(
                        "\n\n[image attached: {}] — to see it, call the vision_analyze tool \
                         with image_url=\"{}\" and your question about the image\n",
                        a.source, a.source
                    ));
                }
                Resolved::Text { source, text } => {
                    // Inlined rather than attached: an `image_url` part
                    // holding a .rs file is not a supported attachment,
                    // it is a malformed request. Same head+tail cap tool
                    // results get — a 200 KB attachment inlined verbatim
                    // every turn is the same tail bloat, just typed in
                    // by the user instead of a command.
                    let text = crate::agent::cap_tool_output(&text, cache.text_cap).unwrap_or(text);
                    content.push_str(&format!(
                        "\n\n--- attached file: {source} ---\n{text}\n--- end {source} ---\n"
                    ));
                }
                Resolved::Skipped { source, why } => {
                    // Said out loud IN the message. A silently dropped
                    // attachment is the failure mode that made the user
                    // ask whether any of this was implemented.
                    content.push_str(&format!("\n\n[attachment not sent: {source} — {why}]"));
                }
            }
        }
        m.content = content;
    }
}

/// The MIME type for a path, by extension.
pub fn mime_for(path: &str) -> &'static str {
    let ext = path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
}

pub fn base64_encode(data: &[u8]) -> String {
    use base64::{engine::general_purpose, Engine};
    general_purpose::STANDARD.encode(data)
}

fn human_bytes(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{} KB", n / 1024)
    } else {
        format!("{n} B")
    }
}

/// Append a `MEDIA:` marker to a message body — the one place that
/// decides what an attached file looks like in stored content.
pub fn with_attachments(body: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return body.to_string();
    }
    let mut out = body.trim_end().to_string();
    for p in paths {
        if p.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("MEDIA:");
        out.push_str(p.trim());
    }
    out
}

// ── who may have WHICH file cross the boundary ────────────────────────
//
// USER: "In the current state can non-admins ask the bot to generate
// images? That seems like it's potentially a huge gap."
//
// The generator was never the gap: `generate_image` is `media-gen`,
// admin-and-owner, and IRC cannot carry a file at all. The gap was the
// CONVENTION around it. `MEDIA:` is a path, and a path is a capability —
// outbound it told telegram to upload any file the gateway could read,
// inbound it told `resolve` to read any file on the host, and neither side
// asked who was talking. The system prompt teaches every principal the
// marker (`prompt.rs`), so the only thing standing between a `user` role and
// `MEDIA:/home/user/.config/veles/.env` was the model's discretion. The F1
// read jail covers `read_file`/`search_files` and never sees a marker.
//
// So the marker gets the same question the file tools already answer, in the
// one place that knows the principal: the gateway seam, both directions.
// Below owner, a path is deliverable if it is in the engine's media root —
// where every legitimate producer already writes — or inside the sender's own
// sandbox workspace, which is theirs and nobody else's (the key is derived
// from the principal, so a marker cannot name a neighbour's `/work`).
// Everything else is withheld: the channel gets a note without the path, the
// log gets the path and the principal, and the file is never opened, so a
// withheld marker is not an existence oracle either.

/// The note a below-owner sender sees in place of a withheld file. No path:
/// the channel is public and the path is host trivia.
pub const WITHHELD_NOTE: &str = "📎 media withheld (outside the media directory)";

/// The note a below-owner sender's own withheld ATTACHMENT becomes, in the
/// text the model reads.
pub const WITHHELD_ATTACHMENT: &str =
    "[attachment withheld: only files in the engine's media directory can be attached]";

/// The roots a below-owner principal's markers may name: the media root and
/// their own sandbox workspace (the host side of their container's `/work`).
fn allowed_roots(who: &crate::acl::Principal) -> Vec<std::path::PathBuf> {
    vec![
        crate::config::media_dir(),
        crate::user_envs::workdir_for(&crate::user_envs::key_for(&who.subject)),
    ]
}

/// Collapse `.`/`..` lexically. `starts_with` is a component-wise prefix
/// test, so a path carrying `..` would otherwise be compared to the root
/// BEFORE the climb it describes — `<media>/../secret.env` "starts with"
/// `<media>` as a string of components and walks straight out.
fn normalized(path: &str) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for comp in std::path::Path::new(path).components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// One candidate spelling of a marker path against the allowed roots.
/// Canonicalized when it exists, so a symlink parked in an allowed directory
/// cannot hop the fence; normalized first regardless, so a path that does
/// NOT exist cannot smuggle a `..` past the prefix test.
fn under_any(path: &str, roots: &[std::path::PathBuf]) -> bool {
    // `plain_path`: Windows canonicalizes to a verbatim `\\?\` path and
    // a plain one never starts_with it; both sides go through it.
    let cand = normalized(path);
    let resolved = crate::platform::plain_path(std::fs::canonicalize(&cand).unwrap_or(cand));
    roots.iter().any(|r| {
        let rn = normalized(&r.to_string_lossy());
        let root = crate::platform::plain_path(std::fs::canonicalize(&rn).unwrap_or(rn));
        resolved.starts_with(root)
    })
}

/// May `who` have the file at `path` delivered to them, or attached by them?
/// `None` means yes; `Some(why)` is the log-side reason (never the channel
/// side — see [`WITHHELD_NOTE`]).
///
/// No principal at all is an OWNER surface (TUI, REPL, desktop, CLI): those
/// never traverse the gateway and keep the whole filesystem, exactly as the
/// F1 read jail leaves them alone. `Cap::All` is the same answer with a name
/// on it. Admin is deliberately NOT: an admin is a trusted operator of the
/// bot, not of the host, and the media root is a one-command detour.
pub fn deliverable(path: &str, who: Option<&crate::acl::Principal>) -> Option<&'static str> {
    let who = who?;
    if who.has(crate::acl::Cap::All) {
        return None;
    }
    let roots = allowed_roots(who);
    // Relative markers are refused rather than resolved: whose cwd they
    // would land in is the gateway's, which is nobody's answer to "may I
    // have this file".
    if std::path::Path::new(path).is_absolute() && under_any(path, &roots) {
        return None;
    }
    // The container view of their own workspace: `/work/x.png` and `~/x.png`
    // are what a sandboxed principal's tools report, so a marker quoting one
    // is a marker about their own file. Translated to the host mount and
    // re-tested, which normalization makes safe against `/work/../…`.
    let work = crate::user_envs::workdir_for(&crate::user_envs::key_for(&who.subject));
    if let Some(rest) = path
        .strip_prefix("/work/")
        .or_else(|| path.strip_prefix("~/"))
    {
        let host = work.join(rest.trim_start_matches('/'));
        if under_any(&host.to_string_lossy(), &roots) {
            return None;
        }
    }
    Some("outside the media directory and the sender's own workspace")
}

/// Rewrite a message body so only deliverable markers survive it.
///
/// Returns the new body and the paths that were withheld — the caller logs
/// those with the principal attached, because "a model tried to leave the
/// media root" is the interesting half and the channel is the wrong place
/// for it. Bodies with no markers come back byte-identical.
fn enforce(text: &str, who: Option<&crate::acl::Principal>, note: &str) -> (String, Vec<String>) {
    let mut withheld = Vec::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        match trimmed.strip_prefix("MEDIA:") {
            Some(rest) if !rest.trim().is_empty() => {
                let path = rest.trim();
                if deliverable(path, who).is_none() {
                    out.push(line.to_string());
                } else {
                    withheld.push(path.to_string());
                    out.push(note.to_string());
                }
            }
            _ => out.push(line.to_string()),
        }
    }
    (out.join("\n"), withheld)
}

/// The OUTBOUND half: what a reply may carry to this principal. Called at
/// the gateway's reply seam, so every connector inherits the answer and
/// none of them has to know a role exists.
pub fn enforce_outbound(text: &str, who: Option<&crate::acl::Principal>) -> (String, Vec<String>) {
    enforce(text, who, WITHHELD_NOTE)
}

/// The INBOUND half: what this principal may attach. Called where connector
/// text becomes the turn text, before anything resolves it — a withheld
/// marker is never opened, so it answers no question about the host either.
pub fn enforce_inbound(text: &str, who: Option<&crate::acl::Principal>) -> (String, Vec<String>) {
    enforce(text, who, WITHHELD_ATTACHMENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_single() {
        let (paths, rest) = extract_media("chart:\nMEDIA:/tmp/a.png\ndone");
        assert_eq!(paths, vec!["/tmp/a.png"]);
        assert_eq!(rest, "chart:\ndone");
    }

    #[test]
    fn extract_multiple_with_spaces() {
        let (paths, _) = extract_media("MEDIA:/tmp/a b.png\nMEDIA:/x.jpg");
        assert_eq!(paths, vec!["/tmp/a b.png", "/x.jpg"]);
    }

    #[test]
    fn extract_none() {
        let (paths, rest) = extract_media("plain\ntext");
        assert!(paths.is_empty());
        assert_eq!(rest, "plain\ntext");
    }

    #[test]
    fn kind_detection() {
        assert!(is_image("/x.PNG"));
        assert!(is_image("/x.jpeg"));
        assert!(!is_image("/x.mp3"));
        assert!(!is_image("/x/noext"));
    }

    #[test]
    fn notes_for_missing_file() {
        let out = render_media_notes("see:\nMEDIA:/nonexistent/z.png");
        assert!(out.contains("(not found)"));
        assert!(out.contains("see:"));
    }

    // ── attachments, user → model ────────────────────────────────────
    //
    // USER: "Is image/file support implemented at all? … no option to
    // even attach or see it in chat." Model → user worked; user → model
    // did not exist, and `/help` said it did. These pin the direction
    // that was missing, end to end: marker in, image part out.

    /// A 1x1 PNG. Small enough to inline, real enough that anything
    /// which sniffs the bytes still sees an image.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn user_msg(content: &str) -> crate::providers::ChatMessage {
        crate::providers::ChatMessage::user(content)
    }

    #[test]
    fn with_attachments_writes_markers_the_extractor_reads_back() {
        let body = with_attachments("look at this", &["/tmp/a.png".into(), "/tmp/b.png".into()]);
        let (paths, rest) = extract_media(&body);
        assert_eq!(paths, vec!["/tmp/a.png", "/tmp/b.png"]);
        assert_eq!(rest.trim(), "look at this");
    }

    #[test]
    fn with_attachments_on_an_empty_body_is_just_the_markers() {
        let body = with_attachments("", &["/tmp/a.png".into()]);
        assert_eq!(body, "MEDIA:/tmp/a.png");
    }

    #[tokio::test]
    async fn an_attached_image_becomes_a_vision_note_not_a_parts_array() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(&png, TINY_PNG).unwrap();

        let mut msgs = vec![user_msg(&with_attachments(
            "what is this",
            &[png.display().to_string()],
        ))];
        attach_user_media(&mut msgs, &mut MediaCache::new(0)).await;

        // The default models are text-only, so the image runs through the
        // vision_analyze note — NOT an image_url part that would 400.
        assert!(
            msgs[0].attachments.is_empty(),
            "a text model cannot take an image part: {:#?}",
            msgs[0].attachments
        );
        assert!(
            msgs[0].content.contains("image attached")
                && msgs[0].content.contains("vision_analyze"),
            "the model has to be pointed at the vision tool: {}",
            msgs[0].content
        );
        // The MODEL must not see the path as data it can read, nor a raw
        // "MEDIA:" marker to hallucinate from — it gets the vision_analyze
        // pointer with the path as an argument.
        assert!(
            !msgs[0].content.contains("MEDIA:"),
            "marker survived: {}",
            msgs[0].content
        );
        assert!(msgs[0].content.starts_with("what is this"));
    }

    #[tokio::test]
    async fn a_sandbox_attachment_is_read_in_its_box_not_on_the_host() {
        let (_home, _env) = crate::testenv::scratch_home();
        crate::acl::set_session_principal(
            "media-box",
            crate::acl::Principal::known("irc:media-box", crate::acl::Role::User),
        );
        let mut cache = MediaCache::new(0);
        cache.session_id = Some("media-box".into());
        let mut msgs = vec![user_msg(&with_attachments("read", &["/work/a.txt".into()]))];
        attach_user_media(&mut msgs, &mut cache).await;
        assert!(
            msgs[0]
                .content
                .contains("user envs are offline under tests"),
            "the read must reach the guest backend, not the host: {}",
            msgs[0].content
        );
        assert!(
            !msgs[0].content.contains("no such file"),
            "a remote /work must not fall back to the host: {}",
            msgs[0].content
        );
        crate::acl::clear_session_principal("media-box");
    }

    #[tokio::test]
    async fn a_missing_attachment_is_said_out_loud_not_dropped() {
        let mut msgs = vec![user_msg(&with_attachments(
            "see attached",
            &["/definitely/not/here.png".into()],
        ))];
        attach_user_media(&mut msgs, &mut MediaCache::new(0)).await;

        assert!(msgs[0].attachments.is_empty());
        assert!(
            msgs[0].content.contains("attachment not sent")
                && msgs[0].content.contains("no such file"),
            "a silently dropped attachment is the failure that started this: {}",
            msgs[0].content
        );
    }

    #[tokio::test]
    async fn a_text_file_is_inlined_rather_than_sent_as_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("main.rs");
        std::fs::write(&src, "fn main() { println!(\"hi\") }").unwrap();

        let mut msgs = vec![user_msg(&with_attachments(
            "review this",
            &[src.display().to_string()],
        ))];
        attach_user_media(&mut msgs, &mut MediaCache::new(0)).await;

        assert!(
            msgs[0].attachments.is_empty(),
            "a .rs file in an image_url part is a malformed request, not an attachment"
        );
        assert!(msgs[0].content.contains("fn main()"), "{}", msgs[0].content);
        assert!(
            msgs[0].content.contains("attached file"),
            "{}",
            msgs[0].content
        );
    }

    #[tokio::test]
    async fn an_oversized_image_is_refused_with_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("huge.png");
        std::fs::write(&big, vec![0u8; MAX_ATTACHMENT_BYTES + 1]).unwrap();

        let mut msgs = vec![user_msg(&with_attachments(
            "big",
            &[big.display().to_string()],
        ))];
        attach_user_media(&mut msgs, &mut MediaCache::new(0)).await;

        assert!(msgs[0].attachments.is_empty());
        assert!(
            msgs[0].content.contains("over the"),
            "the message has to say why: {}",
            msgs[0].content
        );
    }

    /// The turn's cache makes an attachment cost ONE read, not one per
    /// round: with an UNCHANGED mtime the resolution is reused even if
    /// the bytes on disk moved under it (the freshness contract is the
    /// turn boundary, not the round).
    #[tokio::test]
    async fn an_attachment_is_read_once_per_turn_not_once_per_round() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("data.txt");
        std::fs::write(&src, "round-one content").unwrap();
        let mtime = std::fs::metadata(&src).unwrap().modified().unwrap();

        let mut cache = MediaCache::new(0);
        let mut msgs = vec![user_msg(&with_attachments(
            "read this",
            &[src.display().to_string()],
        ))];
        attach_user_media(&mut msgs, &mut cache).await;
        assert!(msgs[0].content.contains("round-one content"));

        // Later round, SAME turn: the content changed on disk, but the
        // mtime is put back where it was, so the cache hit stands.
        std::fs::write(&src, "round-two content").unwrap();
        // write(true): Windows refuses set_modified on a read-only handle.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let mut msgs2 = vec![user_msg(&with_attachments(
            "read this",
            &[src.display().to_string()],
        ))];
        attach_user_media(&mut msgs2, &mut cache).await;
        assert!(
            msgs2[0].content.contains("round-one content"),
            "an unchanged mtime must reuse the resolution, not re-read: {}",
            msgs2[0].content
        );
    }

    /// The documented freshness contract survives the cache: a file
    /// EDITED (mtime moved) is re-read — both by the next turn's fresh
    /// cache and by the old one, which keys on mtime.
    #[tokio::test]
    async fn a_file_edited_between_turns_is_re_read() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("data.txt");
        std::fs::write(&src, "old content").unwrap();

        let mut cache = MediaCache::new(0);
        let mut msgs = vec![user_msg(&with_attachments(
            "read this",
            &[src.display().to_string()],
        ))];
        attach_user_media(&mut msgs, &mut cache).await;
        assert!(msgs[0].content.contains("old content"));

        // Edit and bump the mtime forward explicitly (coarse clocks).
        std::fs::write(&src, "new content").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2))
            .unwrap();

        // A NEW turn brings a fresh cache and sees the edit…
        let mut cache2 = MediaCache::new(0);
        let mut msgs2 = vec![user_msg(&with_attachments(
            "read this",
            &[src.display().to_string()],
        ))];
        attach_user_media(&mut msgs2, &mut cache2).await;
        assert!(
            msgs2[0].content.contains("new content"),
            "{}",
            msgs2[0].content
        );
        // …and even the OLD turn's cache misses once the mtime moved.
        let mut msgs3 = vec![user_msg(&with_attachments(
            "read this",
            &[src.display().to_string()],
        ))];
        attach_user_media(&mut msgs3, &mut cache).await;
        assert!(
            msgs3[0].content.contains("new content"),
            "a moved mtime must invalidate the cached resolution: {}",
            msgs3[0].content
        );
    }

    /// Only a USER message carries attachments. A `MEDIA:` marker in an
    /// assistant reply is the OTHER direction — it is what the desktop
    /// renders inline — and resolving it here would send the model its
    /// own picture back.
    #[tokio::test]
    async fn an_assistant_media_marker_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("out.png");
        std::fs::write(&png, TINY_PNG).unwrap();
        let body = with_attachments("here you go", &[png.display().to_string()]);

        let mut msgs = vec![crate::providers::ChatMessage::assistant(&body, None)];
        attach_user_media(&mut msgs, &mut MediaCache::new(0)).await;

        assert!(msgs[0].attachments.is_empty());
        assert!(msgs[0].content.contains("MEDIA:"), "{}", msgs[0].content);
    }
}

// ── the delivery boundary ─────────────────────────────────────────────
//
// Every test below runs in a scratch VELES_HOME, so `media_dir()` and the
// sandbox roots are the temp tree's, not the real box's.

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use crate::acl::{Principal, Role};
    use crate::testenv::scratch_home;

    fn user() -> Principal {
        Principal::known("irc:alice", Role::User)
    }

    fn media_file(home: &std::path::Path, name: &str) -> String {
        let dir = home.join("media");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, b"png-ish").unwrap();
        p.display().to_string()
    }

    /// No principal is an owner surface: the TUI, the REPL, the desktop and
    /// the CLI never traverse the gateway, and jailing them would only teach
    /// the owner that their own bot disagrees with them. `Cap::All` is the
    /// same answer with a name attached.
    #[test]
    fn owner_surfaces_keep_every_path() {
        let (_d, _g) = scratch_home();
        assert!(deliverable("/etc/passwd", None).is_none());
        assert!(deliverable("/etc/passwd", Some(&Principal::owner())).is_none());
    }

    /// The gap the audit found: a `user` role naming a host file, with the
    /// model as the only thing deciding whether to write the marker.
    #[test]
    fn a_below_owner_principal_cannot_name_a_host_file() {
        let (_d, _g) = scratch_home();
        let who = user();
        assert!(deliverable("/etc/passwd", Some(&who)).is_some());
        assert!(deliverable("/etc/hostname", Some(&who)).is_some());
    }

    /// Admin is NOT owner: an admin operates the bot, not the host, and the
    /// media root is a one-command detour.
    #[test]
    fn admin_is_jailed_too() {
        let (_d, _g) = scratch_home();
        let admin = Principal::known("irc:bob", Role::Admin);
        assert!(deliverable("/etc/passwd", Some(&admin)).is_some());
    }

    #[test]
    fn the_media_root_is_deliverable_for_everyone() {
        let (d, _g) = scratch_home();
        let f = media_file(d.path(), "chart.png");
        let who = user();
        assert!(deliverable(&f, Some(&who)).is_none());
        // The rule is about LOCATION, not existence: a producer that names a
        // file it has not finished writing is a "not found" for `resolve` to
        // report, not a boundary violation.
        let pending = d
            .path()
            .join("media")
            .join("generated")
            .join("x.png")
            .display()
            .to_string();
        assert!(deliverable(&pending, Some(&who)).is_none());
    }

    /// A relative marker resolves against the GATEWAY's cwd, which is nobody's
    /// answer to "may I have this file" — so it is refused, not resolved.
    #[test]
    fn a_relative_marker_is_refused() {
        let (_d, _g) = scratch_home();
        let who = user();
        assert!(deliverable("media/chart.png", Some(&who)).is_some());
        assert!(deliverable("./chart.png", Some(&who)).is_some());
        assert!(deliverable("chart.png", Some(&who)).is_some());
    }

    /// `..` is resolved before the prefix test, not after — otherwise
    /// `<media>/../../.config/veles/.env` walks straight out.
    #[test]
    fn dotdot_does_not_escape_the_root() {
        let (d, _g) = scratch_home();
        std::fs::create_dir_all(d.path().join("media")).unwrap();
        std::fs::write(d.path().join("secret.env"), b"token").unwrap();
        let who = user();
        let media = d.path().join("media").display().to_string();
        let escape = format!("{media}/../secret.env");
        assert!(deliverable(&escape, Some(&who)).is_some());
        // The harder half: canonicalize cannot help when the target does not
        // exist, so the lexical normalization is the only thing standing
        // between `<media>/../nothing-yet` and a prefix test that honours the
        // `..` as a component of the path rather than as a climb out of it.
        let missing = format!("{media}/../not-written-yet.env");
        assert!(
            deliverable(&missing, Some(&who)).is_some(),
            "a climb that names nothing yet is still a climb"
        );
    }

    /// A symlink parked in the allowed directory is the same escape with one
    /// extra hop; canonicalizing the candidate is what closes it.
    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_media_root_does_not_hop_the_fence() {
        let (d, _g) = scratch_home();
        std::fs::create_dir_all(d.path().join("media")).unwrap();
        std::fs::write("/dev/shm/veles-boundary-target", b"secret").unwrap();
        let link = d.path().join("media").join("innocent.png");
        std::os::unix::fs::symlink("/dev/shm/veles-boundary-target", &link).unwrap();
        let who = user();
        assert!(
            deliverable(&link.display().to_string(), Some(&who)).is_some(),
            "the link is inside the media root; what it POINTS at is not"
        );
        std::fs::remove_file("/dev/shm/veles-boundary-target").ok();
    }

    /// A sandboxed principal's own workspace is theirs: both the host mount
    /// and the `/work/…` spelling their tools report. A neighbour's is not,
    /// which is the whole reason the key is derived from the principal
    /// rather than from the path.
    #[test]
    fn a_principal_can_deliver_its_own_workspace_and_not_a_neighbours() {
        let (_d, _g) = scratch_home();
        let who = user();
        let mine = crate::user_envs::workdir_for(&crate::user_envs::key_for(&who.subject));
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(mine.join("chart.png"), b"png").unwrap();

        assert!(deliverable(&mine.join("chart.png").display().to_string(), Some(&who)).is_none());
        assert!(deliverable("/work/chart.png", Some(&who)).is_none());
        assert!(deliverable("~/chart.png", Some(&who)).is_none());

        let theirs = crate::user_envs::workdir_for(&crate::user_envs::key_for("irc:mallory"));
        std::fs::create_dir_all(&theirs).unwrap();
        std::fs::write(theirs.join("chart.png"), b"png").unwrap();
        assert!(deliverable(&theirs.join("chart.png").display().to_string(), Some(&who)).is_some());
        assert!(
            deliverable("/work/../etc/passwd", Some(&who)).is_some(),
            "the container spelling is translated and THEN tested"
        );
    }

    #[test]
    fn enforce_outbound_swaps_a_withheld_marker_for_a_pathless_note() {
        let (d, _g) = scratch_home();
        let ok = media_file(d.path(), "chart.png");
        let who = user();
        let body = format!("here you go\nMEDIA:{ok}\nMEDIA:/etc/passwd\nbye");

        let (text, withheld) = enforce_outbound(&body, Some(&who));

        assert_eq!(withheld, vec!["/etc/passwd".to_string()]);
        assert!(text.contains(&format!("MEDIA:{ok}")), "{text}");
        assert!(text.contains(WITHHELD_NOTE), "{text}");
        assert!(
            !text.contains("/etc/passwd"),
            "the channel gets no path: {text}"
        );
        assert!(
            text.contains("here you go") && text.contains("bye"),
            "{text}"
        );
    }

    #[test]
    fn enforce_inbound_uses_the_attachment_wording() {
        let (_d, _g) = scratch_home();
        let who = user();
        let (text, withheld) = enforce_inbound("look at this\nMEDIA:/etc/shadow", Some(&who));
        assert_eq!(withheld, vec!["/etc/shadow".to_string()]);
        assert!(text.contains(WITHHELD_ATTACHMENT), "{text}");
        assert!(!text.contains("MEDIA:"), "{text}");
    }

    /// A body with nothing to withhold comes back byte-identical — the seam
    /// runs on every reply, so it must not be the thing that reformats them.
    #[test]
    fn clean_bodies_are_untouched() {
        let (_d, _g) = scratch_home();
        let who = user();
        for body in [
            "plain reply",
            "MEDIA:\nreal text",
            "no markers here\n\n— sig",
        ] {
            let (text, withheld) = enforce_outbound(body, Some(&who));
            assert_eq!(text, body);
            assert!(withheld.is_empty());
        }
    }

    #[test]
    fn an_owner_principal_keeps_every_marker() {
        let (_d, _g) = scratch_home();
        let body = "MEDIA:/etc/passwd\nMEDIA:/tmp/x.png";
        let (text, withheld) = enforce_outbound(body, Some(&Principal::owner()));
        assert_eq!(text, body);
        assert!(withheld.is_empty());
        let (text, withheld) = enforce_outbound(body, None);
        assert_eq!(text, body);
        assert!(withheld.is_empty());
    }
}
