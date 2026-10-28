//! What a `/tcl` reply may carry on a PUBLIC feed: core's
//! [`PublicTclReply`] slot, filled by this module (PLAN-plugins-installer.md
//! R5). This was the gateway's own arm; core now has no opinion, and
//! without this policy a public `/tcl` fails closed.
//!
//! The reply goes through the local OUTPUT check (`check`): a
//! deterministic denylist/art/link gate plus the sidecar scorer, with
//! the bad list (unsafe|borderline|blocked procs) held link-only
//! regardless of output. NO third-party endpoint; when the scorer is
//! off the reply fails CLOSED to link-only fast. EVERY public reply is
//! recorded in the ledger, so the owner can delete any of them — by its
//! link — from any surface that reaches this node. A withheld one is
//! also asked about (the sync loop's `tcl` source): approve (repost
//! inline + allowlist) or delete (post/link/blobs down, blocked). The
//! entry id rides on the outbound message's `meta`, so the delivery
//! observer records THIS post's ids on THIS entry.

use std::collections::BTreeMap;

use veles_core::tcleval::{PublicReplyRequest, PublicReplyShaped, PublicTclReply, ShapeFuture};
use veles_types::bus::meta_keys;

use crate::settings::settings;
use crate::withheld::{PostKind, WithheldEntry, WithheldStore};

pub struct Policy;

/// Keep every `MEDIA:` line of a public reply as TEXT. `MEDIA:/path` is
/// the attachment marker the platform plugins consume
/// (`veles_types::text::extract_media`: any line that is `MEDIA:` after
/// trimming whitespace, form feeds included), and the atproto plugin
/// uploads whatever image file such a line names from its own disk. A
/// turn's markers are vetted by the gateway's media gate; a `/tcl` output
/// is somebody's code and never went through it. One space after the word
/// (`MEDIA :`) keeps the line readable and no longer a marker.
///
/// A line starts after a newline, a carriage return or a FORM FEED: the
/// policy turns `bsky::newpost`'s form feeds into newlines for the check,
/// and an elided head is cut from that text, so `x\fMEDIA:/a.png` would
/// become a marker line there.
pub(crate) fn neutralise_media(s: &str) -> std::borrow::Cow<'_, str> {
    const MARK: &str = "MEDIA:";
    if !s.contains(MARK) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    // Only whitespace since the last line break (or the start).
    let mut at_start = true;
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if at_start && rest.starts_with(MARK) {
            out.push_str("MEDIA :");
            rest = &rest[MARK.len()..];
            at_start = false;
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
        if matches!(c, '\n' | '\r' | '\u{c}') {
            at_start = true;
        } else if !c.is_whitespace() {
            at_start = false;
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Did the configured OWNER ask? A configured `[acl]`, a sender id the
/// platform verified, and every capability. Dormant mode (no `[acl]`)
/// resolves every stranger as owner, so it never counts here — the same
/// rule as the full interpreter (`tcleval::full_interp_allowed`).
pub fn asked_by_owner(
    cfg: &veles_core::config::VelesConfig,
    msg: &veles_core::bus::InboundMessage,
) -> bool {
    let Some(id) = msg.sender_id.as_deref().filter(|s| !s.trim().is_empty()) else {
        return false;
    };
    cfg.acl.is_configured()
        && veles_core::acl::Principal::resolve_ctx(Some(id), &msg.sender_ctx, &cfg.acl)
            .has(veles_core::acl::Cap::All)
}

impl PublicTclReply for Policy {
    fn shape<'a>(&'a self, req: PublicReplyRequest<'a>) -> ShapeFuture<'a> {
        let owner = asked_by_owner(req.cfg, req.msg);
        Box::pin(self.shape_with(req, owner))
    }
}

impl Policy {
    /// [`PublicTclReply::shape`], told whether the configured owner asked.
    /// A trigger's answer passes `false` whoever's event fired it: the
    /// event's author did not write the proc that answered.
    ///
    /// A reply is TEXT and PICTURES (`art`): the output itself when it is
    /// a drawing, and each `bsky::art` the line recorded. Each half has
    /// its own check — the text the output check
    /// (`check::decide_public_with`), the pictures [`decide_pictures`] —
    /// and the reply goes out inline only when both let it: a withheld
    /// half withholds the whole reply, a blocked half blocks it.
    pub async fn shape_with(&self, req: PublicReplyRequest<'_>, owner: bool) -> PublicReplyShaped {
        let cfg = settings();
        // veles' envelope (the raw output and its notes), apart from what
        // the code recorded.
        let (raw, code_spec) = crate::art::take_raw(req.spec);
        let plan = crate::art::plan(raw.as_ref(), code_spec.as_ref());
        // A drawing goes as a picture, and the post's text is then veles'
        // notes alone.
        let text_src = match raw.as_ref().filter(|_| plan.output_is_art) {
            Some(r) => r.notes.as_str(),
            None => req.output,
        };
        // A `magick_scheme` call returns its drawing's handle: the
        // drawing is the answer, and the handle is no text.
        let text_src = crate::art::without_handles(text_src, &plan.handles);
        // Somebody's code wrote this output: a line the platform plugin
        // would read as an attachment marker is kept as text, never as a
        // marker (see [`neutralise_media`]). Every use of the output below
        // reads THIS text.
        let output = neutralise_media(&text_src);
        let output = output.as_ref();
        // `bsky::newpost` marks a post boundary with a form feed, and
        // the output check withholds any control character as art.
        // So the check reads the text with each mark as a newline,
        // and the marks come back only on a reply that goes out
        // inline and whole — the very text the check passed.
        let clean = output.replace('\u{c}', "\n");
        // The whole decision: the owner's lists, the bad list through
        // what the line reaches, the output check on the exact posted
        // text. No proc index (bridge down) fails closed there.
        let idx = crate::safety::proc_index(&cfg).await;
        // The mentions the spec may carry — the caller and the
        // thread's own participants — pass the people gate; nothing
        // else does.
        let participants = veles_core::rooms::thread_room(&req.msg.chat_id);
        let quotable = crate::bsky::quotable(req.msg.sender_ctx.post.as_ref());
        let caller_did = req
            .msg
            .sender_id
            .as_deref()
            .and_then(|id| id.strip_prefix("atproto:"));
        let allowed = crate::bsky::Allowed {
            caller: req.msg.sender_ctx.handle.as_deref(),
            participants: &participants,
            images: true,
            owner,
            caller_did,
            quotable: &quotable,
        };
        let mentions = code_spec
            .as_ref()
            .map(|raw| crate::bsky::allowed_mentions(raw, &allowed))
            .unwrap_or_default();
        // The text half. A reply that is only its pictures or its quote
        // has no text to check.
        let text_decision =
            if clean.trim().is_empty() && (!plan.sources.is_empty() || code_spec.is_some()) {
                None
            } else {
                let decision = crate::check::decide_public_with(
                    &cfg,
                    req.code,
                    &clean,
                    req.invoked_hash,
                    idx.as_deref(),
                    &mentions,
                )
                .await;
                veles_core::module::log(tracing::Level::INFO, &decision.log);
                Some(decision.shaped)
            };
        // The picture half.
        let single_call = req
            .invoked_hash
            .filter(|h| !h.is_empty() && !crate::safety::composes(req.code))
            .map(str::to_string);
        let thread_images = crate::magick::thread_images(req.msg.sender_ctx.post.as_ref());
        let pictures = if plan.sources.is_empty() && plan.macros.is_empty() {
            None
        } else {
            let public = PublicChecks {
                cfg: &cfg,
                code: req.code,
                idx: idx.as_deref(),
                mentions: &mentions,
                images: &thread_images,
            };
            let p = decide_pictures(req.cfg, &plan, single_call.as_deref(), owner, &public).await;
            veles_core::module::log(tracing::Level::INFO, &p.log);
            Some(p)
        };
        // What the hosted copy and the ledger keep: the whole result, the
        // drawings as text.
        let hosted = if plan.sources.is_empty() && plan.macros.is_empty() {
            output.to_string()
        } else {
            let macros: Vec<String> = plan.macros.iter().map(|m| m.describe()).collect();
            let mut parts: Vec<&str> = Vec::new();
            if !plan.output_is_art && !clean.trim().is_empty() {
                parts.push(output);
            }
            parts.extend(plan.sources.iter().map(|s| s.text.as_str()));
            parts.extend(macros.iter().map(String::as_str));
            if plan.output_is_art && !clean.trim().is_empty() {
                parts.push(output);
            }
            parts.join("\n\n")
        };
        let s = combine(
            req.code,
            &hosted,
            text_decision,
            pictures.as_ref(),
            single_call,
        );
        // Long text as a picture (P6): a Bluesky reply that goes out
        // inline cut to its head (the rest behind the link) also carries
        // the whole text drawn, when the text the picture shows passes
        // the word checks. Otherwise the reply is the head and the link,
        // as before.
        let long_text = match raw.as_ref() {
            Some(r)
                if s.kind == PostKind::Inline
                    && !plan.output_is_art
                    && crate::safety::public_text(&clean).elided =>
            {
                long_text_picture(&cfg, &r.output, &mentions).await
            }
            _ => None,
        };
        // A withheld long text keeps its whole text drawn, unchecked (the
        // owner's question shows it): an approve reposts the head and
        // the link with the picture, as an inline one goes out.
        let withheld_text_picture = match raw.as_ref() {
            Some(r)
                if s.kind == PostKind::Withheld
                    && pictures.is_none()
                    && !plan.output_is_art
                    && crate::safety::public_text(&clean).elided =>
            {
                let owned = r.output.clone();
                let drawn = veles_core::rt::spawn_blocking(move || crate::art::draw_text(&owned))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r);
                match drawn {
                    Ok(p) => Some(p),
                    Err(e) => {
                        veles_core::module::log(
                            tracing::Level::WARN,
                            &format!("tcl: withheld long text not drawn: {e}"),
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        // A reply with pictures keeps what an approve reposts: the
        // drawings, their labels, the rest of the code's spec (validated
        // for this requester), and the text beside them.
        let mut labels = plan.labels.clone();
        let (picture_refs, kept_spec, post_text) = match &pictures {
            Some(p) => {
                for l in &p.labels {
                    if !labels.contains(l) {
                        labels.push(l.clone());
                    }
                }
                let refs = p
                    .drawn
                    .iter()
                    .map(|(pic, alt)| pic.reference(alt))
                    .collect::<Vec<_>>();
                let spec = code_spec
                    .as_ref()
                    .and_then(|raw| crate::bsky::validate_spec(raw, &allowed).0)
                    .map(|v| v.to_string());
                let text = if plan.output_is_art {
                    output.trim().to_string()
                } else {
                    crate::safety::public_text(&clean).body
                };
                (refs, spec, Some(text))
            }
            // No post text: the approve reposts the head and the link
            // (`publish::repost_text`).
            None => match &withheld_text_picture {
                Some(p) => (vec![p.reference(&p.alt)], None, None),
                None => (Vec::new(), None, None),
            },
        };
        let entry = WithheldEntry {
            kind: s.kind,
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            requester: match req.msg.sender_id.as_deref() {
                Some(id) if id != req.msg.sender => format!("{} ({id})", req.msg.sender),
                _ => req.msg.sender.clone(),
            },
            chat_id: req.msg.chat_id.clone(),
            request_msg_id: req.msg.msg_id.clone().unwrap_or_default(),
            code: req.code.to_string(),
            output: hosted.clone(),
            proc_name: s
                .proc_hash
                .as_deref()
                .and_then(|h| idx.as_ref().and_then(|i| i.name_for(h).map(str::to_string))),
            proc_hash: s.proc_hash.clone(),
            reason: s.reason.clone(),
            pictures: picture_refs,
            labels: if pictures.is_some() {
                labels.clone()
            } else {
                Vec::new()
            },
            spec: kept_spec,
            post_text,
            ..Default::default()
        };
        let mut meta = BTreeMap::new();
        match WithheldStore::load().insert(entry) {
            Ok(id) => {
                if s.kind == PostKind::Withheld {
                    // Nothing to enqueue: the entry IS the decision —
                    // the sync loop reads the ledger on its next tick
                    // and asks the owner.
                    veles_core::module::log(
                        tracing::Level::INFO,
                        &format!(
                            "tcl: withheld {id} ({}); syncask will ask the owner",
                            s.reason
                        ),
                    );
                }
                meta.insert(meta_keys::LEDGER_ENTRY.to_string(), id);
            }
            Err(e) => veles_core::module::log(
                tracing::Level::WARN,
                &format!("tcl: recording the public reply: {e}"),
            ),
        }
        if let Some((md, html)) = &s.artifacts {
            meta.insert(meta_keys::HOSTED_MD.to_string(), md.clone());
            meta.insert(meta_keys::HOSTED_HTML.to_string(), html.clone());
        }
        let mut text = s.text;
        // The rich parts (`bsky::link`, `tag`, `mention`, the pictures,
        // …) ride only with the output itself: a withheld or blocked post
        // is a note and a link, and nothing the code recorded belongs on
        // it.
        if s.kind == PostKind::Inline {
            // An inline post is the checked body plus its formatting
            // link: put the marks back into exactly that body (never
            // into an elided head, which is not the text checked
            // whole).
            if output.contains('\u{c}') {
                let checked = crate::safety::public_text(&clean);
                if !checked.elided {
                    if let Some(rest) = text.strip_prefix(checked.body.as_str()) {
                        text = format!("{}{rest}", crate::safety::public_text(output).body);
                    }
                }
            }
            let mut records: Vec<serde_json::Value> = Vec::new();
            if let Some(raw) = code_spec.as_ref() {
                // Images: already filtered by privilege where the
                // evaluation ran (`eval::via_slopdrop_hashed`).
                let (spec, mut notes) = crate::bsky::validate_spec(raw, &allowed);
                let spec = match spec {
                    Some(s) if !owner => {
                        check_quote(
                            s,
                            req.msg.sender_ctx.post.as_ref(),
                            &cfg,
                            &mentions,
                            &mut notes,
                        )
                        .await
                    }
                    other => other,
                };
                let spec = spec.and_then(|s| cap_quotes(s, owner, &req.msg.sender, &mut notes));
                if let Some(serde_json::Value::Array(r)) = spec {
                    records = r;
                }
                notes.extend(plan.notes.iter().cloned());
                for n in notes {
                    text.push('\n');
                    text.push_str(&n);
                }
            }
            if let Some(PictureDecision {
                outcome:
                    Pictures::Post {
                        records: pics,
                        notes,
                    },
                ..
            }) = pictures
            {
                let images = records
                    .iter()
                    .filter(|r| r.get("kind").and_then(serde_json::Value::as_str) == Some("image"))
                    .count();
                let room = crate::art::MAX_PICTURES.saturating_sub(images);
                if pics.len() > room {
                    text.push_str(&format!(
                        "\n(a post holds four images: {} drawing(s) left out)",
                        pics.len() - room
                    ));
                }
                records.extend(pics.into_iter().take(room));
                for n in notes {
                    text.push('\n');
                    text.push_str(&n);
                }
            }
            if let Some(page) = long_text {
                let images = records
                    .iter()
                    .filter(|r| {
                        matches!(
                            r.get("kind").and_then(serde_json::Value::as_str),
                            Some("image" | "art")
                        )
                    })
                    .count();
                if images < crate::art::MAX_PICTURES {
                    records.push(page);
                }
            }
            records.extend(
                labels
                    .into_iter()
                    .map(|v| serde_json::json!({"kind": "label", "val": v})),
            );
            if !records.is_empty() {
                meta.insert(
                    meta_keys::BSKY_SPEC.to_string(),
                    serde_json::Value::Array(records).to_string(),
                );
            }
        }
        PublicReplyShaped { text, meta }
    }
}

/// One image macro drawn (P6): its pictures fetched from the thread, the
/// ops or the program run off the async workers, the PNG kept like a
/// drawing's, with the words written on it for the word checks. Its alt
/// text is the code's, else the image check's description.
async fn draw_macro(
    m: &crate::magick::Macro,
    images: &[String],
) -> Result<crate::art::Picture, String> {
    let sources = crate::magick::sources(m, images).await;
    let m = m.clone();
    veles_core::rt::spawn_blocking(move || {
        let (png, width, height, words) = crate::magick::draw(&m, &sources)?;
        crate::art::keep(
            crate::art::Drawn {
                bytes: png,
                width,
                height,
                truncated: false,
            },
            &m.text(),
            m.alt.trim(),
            words.clone(),
            words,
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

/// A long text drawn as one picture (P6), as its `art` record, when the
/// text the picture shows passes the word checks (links and contact
/// details, the denylist, the scorer in pieces): the same checks a text
/// post gets, over the whole text instead of the head. `None` keeps the
/// reply to the head and the link; why is logged.
async fn long_text_picture(
    cfg: &crate::settings::SlopdropSection,
    text: &str,
    mentions: &[String],
) -> Option<serde_json::Value> {
    let owned = text.to_string();
    let drawn = veles_core::rt::spawn_blocking(move || crate::art::draw_text(&owned)).await;
    let pic = match drawn.map_err(|e| e.to_string()).and_then(|r| r) {
        Ok(p) => p,
        Err(e) => {
            veles_core::module::log(
                tracing::Level::WARN,
                &format!("tcl: long text not drawn: {e}"),
            );
            return None;
        }
    };
    let checked = crate::check::words_detailed(
        &cfg.output_classifier,
        &pic.visible,
        cfg.output_classifier_tox.unwrap_or(0.30),
        cfg.output_classifier_id.unwrap_or(0.25),
        mentions,
    )
    .await;
    if let Some(why) = checked.outcome.reason() {
        veles_core::module::log(
            tracing::Level::INFO,
            &format!("tcl: long text kept to its head ({why})"),
        );
        return None;
    }
    veles_core::module::log(
        tracing::Level::INFO,
        &format!(
            "tcl: long text drawn {} ({}x{}{})",
            &pic.sha[..12],
            pic.width,
            pic.height,
            if pic.truncated { ", cut" } else { "" }
        ),
    );
    Some(crate::art::record(&pic, &pic.alt))
}

/// What the picture half of a reply decided.
enum Pictures {
    /// Attach these (`art::record`s); `notes` say what could not be drawn.
    Post {
        records: Vec<serde_json::Value>,
        notes: Vec<String>,
    },
    Withheld(String),
    Blocked,
}

struct PictureDecision {
    outcome: Pictures,
    /// What let the pictures out (the ledger's reason).
    reason: String,
    /// The line logged about the decision: hashes and categories, never
    /// the drawing.
    log: String,
    /// Every drawing drawn, with its alt text: the ledger keeps them (the
    /// owner's question shows them, an approve reposts them) and a
    /// withheld reply's hosted copy shows them.
    drawn: Vec<(crate::art::Picture, String)>,
    /// What the image check saw, as self-labels.
    labels: Vec<String>,
}

/// What judging anyone's pictures needs beyond the pictures
/// (PLAN-slopdrop-art.md P5).
struct PublicChecks<'a> {
    cfg: &'a crate::settings::SlopdropSection,
    /// The line as typed.
    code: &'a str,
    /// The bridge's proc index; `None` (the bridge did not answer) fails
    /// closed.
    idx: Option<&'a crate::safety::ProcIndex>,
    /// The `@handle`s the reply may carry.
    mentions: &'a [String],
    /// The pictures the thread shows: all an image macro may fetch.
    images: &'a [String],
}

/// Anyone else's pictures before the image check (P5); why they are
/// withheld, or `None`. The first that decides:
///  1. the bad list, through what the line reaches: tracked procs through
///     their bodies, the base library one hop, stamps by their `say`
///     names (`ProcIndex::bad_reach`);
///  2. the typed line through the folded denylist (a slur spelled into
///     the code is withheld even when the drawing never shows it);
///  3. the words the pictures show through the word checks: links,
///     contact details, the denylist, the scorer
///     (`check::words_detailed`). The text beside them is the text half's
///     own check, and a withheld text withholds its pictures (`combine`).
async fn public_gate(c: &PublicChecks<'_>, pics: &[crate::art::Picture]) -> Option<String> {
    let Some(idx) = c.idx else {
        return Some("the proc index is unavailable".to_string());
    };
    if let Some(n) = idx.bad_reach(c.code, &crate::safety::verdicts()) {
        return Some(format!("reaches bad-list proc {n}"));
    }
    if crate::check::denylist_hit(c.code) {
        return Some("the line names a denylisted word".to_string());
    }
    let words: Vec<&str> = pics.iter().map(|p| p.visible.as_str()).collect();
    let checked = crate::check::words_detailed(
        &c.cfg.output_classifier,
        &words.join("\n"),
        c.cfg.output_classifier_tox.unwrap_or(0.30),
        c.cfg.output_classifier_id.unwrap_or(0.25),
        c.mentions,
    )
    .await;
    checked
        .outcome
        .reason()
        .map(|r| format!("the words in the picture did not pass the check ({r})"))
}

/// Anyone but the owner quotes a post of the thread only when its text
/// passes the word checks (P5): the bot's account re-posts what it
/// quotes. A quoted post whose text is not in the thread the request
/// came with (the caller's own, from elsewhere) is theirs to quote. The
/// quote is dropped with a note; nothing else of the reply changes.
async fn check_quote(
    spec: serde_json::Value,
    ctx: Option<&serde_json::Value>,
    cfg: &crate::settings::SlopdropSection,
    mentions: &[String],
    notes: &mut Vec<String>,
) -> Option<serde_json::Value> {
    let serde_json::Value::Array(recs) = spec else {
        return Some(spec);
    };
    let mut kept = Vec::with_capacity(recs.len());
    for r in recs {
        if r.get("kind").and_then(serde_json::Value::as_str) == Some("quote") {
            let uri = r
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if let Some(text) = crate::bsky::thread_text(ctx, uri) {
                let checked = crate::check::words_detailed(
                    &cfg.output_classifier,
                    &text,
                    cfg.output_classifier_tox.unwrap_or(0.30),
                    cfg.output_classifier_id.unwrap_or(0.25),
                    mentions,
                )
                .await;
                if !checked.outcome.pass() {
                    notes.push(
                        "(the quoted post did not pass the content check, so it is not quoted)"
                            .to_string(),
                    );
                    continue;
                }
            }
        }
        kept.push(r);
    }
    (!kept.is_empty()).then_some(serde_json::Value::Array(kept))
}

/// The picture half of a reply. The first rule that decides wins:
///  1. a drawing the owner deleted before (its text's hash is on the
///     blocked list), or the one stored proc the line called is Blocked
///     → blocked, nothing drawn;
///  2. anyone but the owner, unless the owner approved these exact
///     drawings before: what the line reaches on the bad list, the line
///     itself through the denylist, and the words the pictures show with
///     the text beside them through the word checks ([`public_gate`]) →
///     withheld and asked, drawn but not shown to the image check;
///  3. each drawing is shown to the image check (`art::classify`); a
///     check that fails, or that sees what holds the reply
///     (`art::OWNER_HOLD` for the owner, `art::PUBLIC_WITHHOLD` for
///     anyone else) → withheld and asked, unless the owner approved
///     these exact drawings;
///  4. otherwise they go out, self-labelled with what the check saw and
///     what the code asked for.
async fn decide_pictures(
    core: &veles_core::config::VelesConfig,
    plan: &crate::art::Plan,
    single_call: Option<&str>,
    owner: bool,
    public: &PublicChecks<'_>,
) -> PictureDecision {
    use crate::safety;
    let mut notes = Vec::new();
    // Image macros are anyone's (USER, 2026-10-12): a stranger's is
    // drawn from the thread's pictures only, and its words and picture
    // pass the checks below like their drawings.
    let macros: &[crate::magick::Macro] = &plan.macros;
    let hashes: Vec<String> = plan
        .sources
        .iter()
        .map(|s| crate::check::text_hash(&s.text))
        .chain(macros.iter().map(|m| crate::check::text_hash(&m.text())))
        .collect();
    if hashes.iter().any(|h| safety::blocked(h))
        || single_call.and_then(safety::verdict_for) == Some(safety::Verdict::Blocked)
    {
        return PictureDecision {
            outcome: Pictures::Blocked,
            reason: "the owner deleted this output before".to_string(),
            log: "tcl: picture check not run (blocked)".to_string(),
            drawn: Vec::new(),
            labels: Vec::new(),
        };
    }
    let approved = hashes.iter().all(|h| safety::allowlisted(h));
    let sources = plan.sources.clone();
    let drawn = veles_core::rt::spawn_blocking(move || {
        sources.iter().map(crate::art::draw).collect::<Vec<_>>()
    })
    .await;
    let drawn = match drawn {
        Ok(d) => d,
        Err(e) => {
            let why = format!("drawing the pictures failed: {e}");
            return PictureDecision {
                outcome: Pictures::Withheld(why.clone()),
                reason: why.clone(),
                log: format!("tcl: {why}"),
                drawn: Vec::new(),
                labels: Vec::new(),
            };
        }
    };
    let mut pics = Vec::new();
    for d in drawn {
        match d {
            Ok(p) => pics.push(p),
            Err(e) => notes.push(format!("(a drawing was left out: {e})")),
        }
    }
    for m in macros {
        match draw_macro(m, public.images).await {
            Ok(p) => pics.push(p),
            Err(e) => notes.push(format!("(an image macro was left out: {e})")),
        }
    }
    if !owner && !approved {
        if let Some(why) = public_gate(public, &pics).await {
            let drawn = pics
                .into_iter()
                .map(|p| {
                    let alt = crate::art::alt_for(&p, crate::art::cached(&p.sha).as_ref());
                    (p, alt)
                })
                .collect();
            return PictureDecision {
                outcome: Pictures::Withheld(why.clone()),
                log: format!("tcl: picture check not run (public: {why})"),
                reason: why,
                drawn,
                labels: Vec::new(),
            };
        }
    }
    let mut labels: Vec<String> = Vec::new();
    let mut holds: Vec<String> = Vec::new();
    let mut failed: Option<String> = None;
    let mut seen = Vec::with_capacity(pics.len());
    let mut logged = Vec::new();
    for p in &pics {
        let short = &p.sha[..12.min(p.sha.len())];
        match crate::art::classify(core, p).await {
            Ok(s) => {
                for l in s.labels() {
                    if !labels.iter().any(|x| x == l) {
                        labels.push(l.to_string());
                    }
                }
                for h in s.holds(owner) {
                    if !holds.iter().any(|x| x == h) {
                        holds.push(h.to_string());
                    }
                }
                logged.push(format!("{short} [{}]", s.categories.join(",")));
                seen.push(Some(s));
            }
            Err(e) => {
                logged.push(format!("{short} failed: {e}"));
                if failed.is_none() {
                    failed = Some(e);
                }
                seen.push(None);
            }
        }
    }
    let log = format!(
        "tcl: picture check ({}{}): {}",
        if owner { "owner" } else { "public" },
        if approved { ", approved before" } else { "" },
        if logged.is_empty() {
            "nothing drawn".to_string()
        } else {
            logged.join("; ")
        }
    );
    let held = if approved {
        None
    } else if let Some(e) = failed {
        Some(e)
    } else if !holds.is_empty() {
        let why: Vec<&str> = seen
            .iter()
            .flatten()
            .map(|s| s.reason.as_str())
            .filter(|r| !r.is_empty())
            .collect();
        Some(format!(
            "the image check saw {} ({})",
            holds.join(", "),
            why.join(" ")
        ))
    } else {
        None
    };
    let drawn: Vec<(crate::art::Picture, String)> = pics
        .into_iter()
        .zip(&seen)
        .map(|(p, s)| {
            let alt = crate::art::alt_for(&p, s.as_ref());
            (p, alt)
        })
        .collect();
    if let Some(why) = held {
        return PictureDecision {
            outcome: Pictures::Withheld(why.clone()),
            reason: why,
            log,
            drawn,
            labels,
        };
    }
    let records = drawn
        .iter()
        .map(|(p, alt)| crate::art::record(p, alt))
        .collect();
    let mut reason = if approved {
        "the owner approved these drawings".to_string()
    } else {
        "the pictures passed the image check".to_string()
    };
    if !labels.is_empty() {
        reason.push_str(&format!(" (labelled {})", labels.join(", ")));
    }
    PictureDecision {
        outcome: Pictures::Post { records, notes },
        reason,
        log,
        drawn,
        labels,
    }
}

/// One reply from its two halves. No pictures: the text's decision as it
/// stands. Pictures: blocked or withheld pictures decide the reply (a
/// blocked text still blocks it); pictures that go out go with an inline
/// text, and a text that does not go out withholds them too. A withheld
/// reply's hosted copy shows the drawings, as text and as pictures.
fn combine(
    code: &str,
    hosted: &str,
    text: Option<crate::safety::ShapedPublic>,
    pictures: Option<&PictureDecision>,
    single_call: Option<String>,
) -> crate::safety::ShapedPublic {
    use crate::safety::{self, ShapedPublic};
    let only_pictures = |reason: String, proc_hash: Option<String>| ShapedPublic {
        text: String::new(),
        artifacts: None,
        kind: PostKind::Inline,
        reason,
        proc_hash,
    };
    let Some(p) = pictures else {
        return text.unwrap_or_else(|| {
            only_pictures(
                "nothing to check but what the code recorded".to_string(),
                single_call,
            )
        });
    };
    let pngs: Vec<Vec<u8>> = p.drawn.iter().map(|(pic, _)| pic.png.clone()).collect();
    match (&p.outcome, text) {
        (_, Some(t)) if t.kind == PostKind::Blocked => t,
        (Pictures::Blocked, _) => safety::blocked_note(single_call),
        (Pictures::Withheld(why), _) => {
            safety::withheld_with_link(code, hosted, &pngs, why.clone(), single_call)
        }
        (Pictures::Post { .. }, Some(t)) if t.kind == PostKind::Withheld => {
            safety::withheld_with_link(code, hosted, &pngs, t.reason, t.proc_hash)
        }
        (Pictures::Post { .. }, Some(t)) => ShapedPublic {
            reason: format!("{}; {}", t.reason, p.reason),
            ..t
        },
        (Pictures::Post { .. }, None) => only_pictures(p.reason.clone(), single_call),
    }
}

/// Anyone but the owner quotes within [`crate::quotecap`]'s limits: an
/// over-the-limit quote is dropped from the spec (the reply says why),
/// and one that goes out is counted. `None` when nothing is left.
fn cap_quotes(
    spec: serde_json::Value,
    owner: bool,
    requester: &str,
    notes: &mut Vec<String>,
) -> Option<serde_json::Value> {
    if owner {
        return Some(spec);
    }
    let serde_json::Value::Array(recs) = spec else {
        return Some(spec);
    };
    let mut kept = Vec::with_capacity(recs.len());
    for r in recs {
        if r.get("kind").and_then(serde_json::Value::as_str) == Some("quote") {
            let uri = r
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if let Some(why) = crate::quotecap::refusal(uri) {
                notes.push(why);
                continue;
            }
            if let Err(e) = crate::quotecap::record(uri, requester) {
                // Not counted means not limited: drop it rather than let
                // an unwritable state dir lift the cap.
                veles_core::module::log(
                    tracing::Level::WARN,
                    &format!("tcl: counting a quote failed ({e}); the quote is dropped"),
                );
                continue;
            }
        }
        kept.push(r);
    }
    (!kept.is_empty()).then_some(serde_json::Value::Array(kept))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::withheld::output_hash;
    use serde_json::json;

    fn msg() -> veles_core::bus::InboundMessage {
        veles_core::bus::InboundMessage {
            platform: "atproto".into(),
            route: None,
            chat_id: "at://did:plc:op/app.bsky.feed.post/root".into(),
            msg_id: Some("at://did:plc:op/app.bsky.feed.post/req#cid".into()),
            dedup_id: None,
            sender: "op.example".into(),
            sender_id: Some("atproto:did:plc:op".into()),
            sender_ctx: Default::default(),
            text: "/tcl expr 1".into(),
            context: None,
        }
    }

    /// Every public `/tcl` reply is recorded in the ledger and named on its
    /// outbound meta (so the delivery observer files the post's ids under
    /// it): a withheld one, which is asked about, and an inline one, which
    /// is not but which the owner can still delete by its link.
    #[tokio::test]
    async fn every_public_tcl_reply_is_recorded_for_a_delete() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        crate::settings::pin_for_test(crate::settings::SlopdropSection::default());
        let cfg = veles_core::config::VelesConfig::default();
        let m = msg();
        let shaped = Policy
            .shape(PublicReplyRequest {
                cfg: &cfg,
                msg: &m,
                code: "expr 1",
                output: "1",
                invoked_hash: None,
                spec: None,
            })
            .await;
        let id = shaped
            .meta
            .get(meta_keys::LEDGER_ENTRY)
            .cloned()
            .expect("the reply names its entry");
        let e = WithheldStore::load().get(&id).expect("recorded");
        assert_eq!(
            e.kind,
            PostKind::Withheld,
            "no scorer here, so withheld: {}",
            e.reason
        );
        assert_eq!(e.code, "expr 1");
        assert_eq!(e.requester, "op.example (atproto:did:plc:op)");
        assert_eq!(
            e.request_msg_id,
            "at://did:plc:op/app.bsky.feed.post/req#cid"
        );
        assert!(
            !shaped.text.contains('1')
                || shaped.text.contains("withheld")
                || shaped.text.contains("http"),
            "the output itself is not posted raw: {}",
            shaped.text
        );
        // The owner approved this exact text: the next one goes inline —
        // and is recorded too, as its own entry.
        crate::safety::allowlist_add(&output_hash("1")).unwrap();
        let second = Policy
            .shape(PublicReplyRequest {
                cfg: &cfg,
                msg: &m,
                code: "expr 1",
                output: "1",
                invoked_hash: None,
                spec: None,
            })
            .await;
        let id2 = second
            .meta
            .get(meta_keys::LEDGER_ENTRY)
            .cloned()
            .expect("an inline reply names its entry too");
        assert_ne!(id2, id);
        let e2 = WithheldStore::load().get(&id2).expect("recorded");
        assert_eq!(e2.kind, PostKind::Inline);
        assert_eq!(e2.reason, "the owner approved this text");
        assert!(second.text.starts_with('1'), "inline: {}", second.text);
    }

    /// A Bluesky reply that goes out inline carries what its code recorded
    /// (validated: a stranger's mention is dropped) and keeps the post
    /// boundaries `bsky::newpost` marked; a withheld one carries neither.
    #[tokio::test]
    async fn an_inline_reply_carries_its_validated_spec_and_its_post_breaks() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        crate::settings::pin_for_test(crate::settings::SlopdropSection::default());
        let cfg = veles_core::config::VelesConfig::default();
        let m = msg();
        let spec = serde_json::json!([
            {"kind": "link", "text": "docs", "uri": "https://d.test"},
            {"kind": "mention", "text": "@stranger.test", "handle": "stranger.test"},
        ]);
        async fn shape(
            cfg: &veles_core::config::VelesConfig,
            m: &veles_core::bus::InboundMessage,
            spec: &serde_json::Value,
            out: &str,
        ) -> veles_core::tcleval::PublicReplyShaped {
            Policy
                .shape(PublicReplyRequest {
                    cfg,
                    msg: m,
                    code: "bsky_demo",
                    output: out,
                    invoked_hash: None,
                    spec: Some(spec),
                })
                .await
        }
        // No scorer here: withheld, and nothing the code recorded rides.
        let withheld = shape(&cfg, &m, &spec, "see docs\u{c}more").await;
        assert!(!withheld.meta.contains_key(meta_keys::BSKY_SPEC));
        assert!(!withheld.text.contains('\u{c}'));
        // The owner approved the text (as the check reads it: the mark
        // as a newline): inline, with the spec and the mark.
        crate::safety::allowlist_add(&output_hash("see docs\nmore")).unwrap();
        let inline = shape(&cfg, &m, &spec, "see docs\u{c}more").await;
        assert!(
            inline
                .text
                .starts_with("see docs\u{c}more\n(with formatting:"),
            "the post break survives, the formatting link after it: {:?}",
            inline.text
        );
        let carried: serde_json::Value = serde_json::from_str(
            inline
                .meta
                .get(meta_keys::BSKY_SPEC)
                .expect("the spec rides"),
        )
        .unwrap();
        assert_eq!(
            carried.as_array().unwrap().len(),
            1,
            "the stranger is dropped: {carried}"
        );
        assert_eq!(carried[0]["uri"], "https://d.test");
    }

    /// A loopback stand-in for the slopdrop bridge (an empty proc list)
    /// and the scorer (every head at zero), for as many requests as come.
    fn stub_bridge_and_scorer() -> u16 {
        stub_bridge_with("[]".to_string())
    }

    /// [`stub_bridge_and_scorer`], whose `/api/procs` lists `procs`.
    fn stub_bridge_with(procs: String) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut heads = serde_json::Map::new();
        for h in crate::check::TOX_HEADS
            .iter()
            .chain(crate::check::ID_HEADS.iter())
        {
            heads.insert(h.to_string(), serde_json::json!(0.0));
        }
        let scored = serde_json::json!({"model": "m", "scores": heads.clone(), "tokens": 5,
            "truncated": false, "ms": 1})
        .to_string();
        // `blorptox` scores as toxic: the scorer's leg of a check that
        // reads words.
        let cut = serde_json::json!({"model": "m", "scores": heads.clone(), "tokens": 512,
            "truncated": true, "ms": 1})
        .to_string();
        heads.insert("toxicity".to_string(), serde_json::json!(0.99));
        let hot = serde_json::json!({"model": "m", "scores": heads, "tokens": 5,
            "truncated": false, "ms": 1})
        .to_string();
        std::thread::spawn(move || {
            for s in listener.incoming() {
                let Ok(mut s) = s else { continue };
                let mut buf = vec![0u8; 65536];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                // Like the sidecar, a text past what it reads whole comes
                // back `truncated` (its 512 tokens; here 1500 bytes).
                let too_long = head
                    .to_ascii_lowercase()
                    .split("content-length:")
                    .nth(1)
                    .and_then(|r| r.trim_start().split(|c: char| !c.is_ascii_digit()).next())
                    .and_then(|n| n.parse::<usize>().ok())
                    .is_some_and(|n| n > 1500);
                let body = if head.starts_with("POST /score") && too_long {
                    cut.clone()
                } else if head.starts_with("POST /score") && head.contains("blorptox") {
                    hot.clone()
                } else if head.starts_with("POST /score") {
                    scored.clone()
                } else if head.starts_with("GET /api/procs?base=1") {
                    // Listed only when asked with the base library, as
                    // slopdrop does.
                    procs.clone()
                } else {
                    "[]".to_string()
                };
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        port
    }

    /// A public reply never carries a line the atproto plugin reads as an
    /// attachment. `MEDIA:/path` is the hermes marker
    /// (`veles_types::text::extract_media`), and the plugin uploads any
    /// image file it can read from such a line. A `/tcl` output is
    /// somebody's code: `MEDIA:/srv/…/x.png` passes every gate of the
    /// output check (ASCII, mostly alphanumeric, the `:/` defeats the
    /// bare-domain test) and the scorer, so before this it went out
    /// inline and the plugin attached a file off its own disk. The line
    /// survives as text, one space in, so nothing reads it as a marker.
    #[tokio::test]
    async fn a_media_line_in_a_public_reply_is_never_an_attachment() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let port = stub_bridge_and_scorer();
        crate::settings::pin_for_test(crate::settings::SlopdropSection {
            url: Some(format!("http://127.0.0.1:{port}")),
            output_classifier: vec![format!("http://127.0.0.1:{port}")],
            ..Default::default()
        });
        let cfg = veles_core::config::VelesConfig::default();
        let m = msg();
        // The last one is long enough to be elided: its head is cut from
        // the text the check read, where the form feed is a newline.
        let long = format!(
            "look\u{c}MEDIA:/srv/media/generated/a.png\n{}",
            "words go here ".repeat(60)
        );
        for out in [
            "MEDIA:/srv/media/generated/a.png",
            "look\n  MEDIA:/srv/media/generated/a.png",
            "look\u{c}MEDIA:/srv/media/generated/a.png",
            long.as_str(),
        ] {
            let shaped = Policy
                .shape(PublicReplyRequest {
                    cfg: &cfg,
                    msg: &m,
                    code: "return $x",
                    output: out,
                    invoked_hash: None,
                    spec: None,
                })
                .await;
            let (paths, _) = veles_types::text::extract_media(&shaped.text);
            assert!(
                paths.is_empty(),
                "{out:?} must not attach anything: {:?}",
                shaped.text
            );
            let id = shaped.meta.get(meta_keys::LEDGER_ENTRY).cloned().unwrap();
            let e = WithheldStore::load().get(&id).unwrap();
            assert_eq!(
                e.kind,
                PostKind::Inline,
                "the stub scorer passes it, so it goes out inline and the marker is \
                 what this test is about: {}",
                e.reason
            );
            assert!(
                shaped.text.contains("MEDIA :/srv/media/generated/a.png"),
                "the line is kept as text: {:?}",
                shaped.text
            );
        }
    }

    /// A quote rides an inline reply only within the conversation unless
    /// the configured owner asked, and anyone else's quotes of one post
    /// are counted (the second is dropped, and the reply says why). The
    /// owner is the CONFIGURED one: with no `[acl]` every sender resolves
    /// as owner, which must not lift the limits.
    #[tokio::test]
    async fn a_quote_rides_inside_the_thread_and_is_counted_unless_the_owner_asked() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let port = stub_bridge_and_scorer();
        crate::settings::pin_for_test(crate::settings::SlopdropSection {
            url: Some(format!("http://127.0.0.1:{port}")),
            output_classifier: vec![format!("http://127.0.0.1:{port}")],
            ..Default::default()
        });
        let parent = "at://did:plc:par/app.bsky.feed.post/p1";
        let stranger = "at://did:plc:far/app.bsky.feed.post/s1";
        let mut m = msg();
        m.sender_id = Some("atproto:did:plc:caller".into());
        m.sender_ctx.post = Some(serde_json::json!({
            "post": {"uri": "at://did:plc:caller/app.bsky.feed.post/ask",
                     "reply": {"parent": parent, "root": parent}},
            "thread": [{"uri": parent}],
        }));
        let quote = |uri: &str| serde_json::json!([{"kind": "quote", "uri": uri}]);
        async fn shape(
            cfg: &veles_core::config::VelesConfig,
            m: &veles_core::bus::InboundMessage,
            spec: &serde_json::Value,
        ) -> veles_core::tcleval::PublicReplyShaped {
            Policy
                .shape(PublicReplyRequest {
                    cfg,
                    msg: m,
                    code: "bsky_quote_demo",
                    output: "quoted",
                    invoked_hash: None,
                    spec: Some(spec),
                })
                .await
        }
        let carried = |s: &veles_core::tcleval::PublicReplyShaped| {
            s.meta
                .get(meta_keys::BSKY_SPEC)
                .map(|j| j.contains("\"quote\""))
                .unwrap_or(false)
        };
        // Dormant ACL: everyone resolves as owner, and that is no owner.
        let dormant = veles_core::config::VelesConfig::default();
        let s = shape(&dormant, &m, &quote(stranger)).await;
        assert!(!carried(&s), "a stranger's post: {:?}", s.text);
        assert!(
            s.text.contains("only a post of this thread"),
            "{:?}",
            s.text
        );
        let s = shape(&dormant, &m, &quote(parent)).await;
        assert!(carried(&s), "the parent: {:?}", s.text);
        let s = shape(&dormant, &m, &quote(parent)).await;
        assert!(!carried(&s), "the same post twice: {:?}", s.text);
        assert!(s.text.contains("quoted already today"), "{:?}", s.text);
        // The configured owner quotes anything, uncounted.
        let mut owned = veles_core::config::VelesConfig::default();
        owned.acl.owner = vec!["atproto:did:plc:caller".into()];
        assert!(asked_by_owner(&owned, &m));
        assert!(!asked_by_owner(&dormant, &m));
        for _ in 0..2 {
            let s = shape(&owned, &m, &quote(stranger)).await;
            assert!(carried(&s), "the owner: {:?}", s.text);
        }
    }

    /// `bsky::mention` of the caller goes out inline: the people gate lets
    /// exactly the handles the spec may carry through. The same reply
    /// naming anyone else is withheld, and carries nothing it recorded.
    #[tokio::test]
    async fn a_reply_mentioning_its_caller_goes_out_inline() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let port = stub_bridge_and_scorer();
        crate::settings::pin_for_test(crate::settings::SlopdropSection {
            url: Some(format!("http://127.0.0.1:{port}")),
            output_classifier: vec![format!("http://127.0.0.1:{port}")],
            ..Default::default()
        });
        let cfg = veles_core::config::VelesConfig::default();
        let mut m = msg();
        m.sender_ctx.handle = Some("op.example".into());
        let spec = serde_json::json!([
            {"kind": "mention", "text": "@op.example", "handle": "op.example"},
        ]);
        let shape = |out: &'static str| {
            let (cfg, m, spec) = (&cfg, &m, &spec);
            async move {
                Policy
                    .shape(PublicReplyRequest {
                        cfg,
                        msg: m,
                        code: "bsky_mention_demo",
                        output: out,
                        invoked_hash: None,
                        spec: Some(spec),
                    })
                    .await
            }
        };
        let ok = shape("thanks @op.example").await;
        assert!(
            ok.text.starts_with("thanks @op.example"),
            "inline: {:?}",
            ok.text
        );
        let carried = ok
            .meta
            .get(meta_keys::BSKY_SPEC)
            .expect("the mention rides");
        assert!(carried.contains("op.example"), "{carried}");
        let no = shape("thanks @op.example and @stranger.example").await;
        assert!(
            !no.meta.contains_key(meta_keys::BSKY_SPEC),
            "withheld, nothing rides: {:?}",
            no.text
        );
        assert!(!no.text.contains("@stranger.example"), "{:?}", no.text);
    }

    /// A loopback vision model on the `[vision]` route: every chat
    /// completion is answered with what `answer` holds at the time, and
    /// counted. It reads each request whole (a PNG rides in it) before it
    /// answers.
    fn stub_vision() -> (
        u16,
        std::sync::Arc<std::sync::Mutex<String>>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let answer = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (a, c) = (answer.clone(), calls.clone());
        std::thread::spawn(move || {
            for s in listener.incoming() {
                let Ok(mut s) = s else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 65536];
                let body_at = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(i + 4);
                    }
                };
                let Some(body_at) = body_at else { continue };
                let head = String::from_utf8_lossy(&buf[..body_at]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while buf.len() < body_at + len {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let content = a.lock().unwrap().clone();
                let body =
                    serde_json::json!({"choices": [{"message": {"content": content}}]}).to_string();
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (port, answer, calls)
    }

    /// The scratch home's config.toml with a `[vision]` route to `port`
    /// (the vision call reads its route from disk), loaded, with `owner`
    /// as the configured owner when given.
    fn vision_config(port: u16, owner: Option<&str>) -> veles_core::config::VelesConfig {
        std::fs::write(
            veles_core::config::config_dir().join("config.toml"),
            format!(
                "[vision]\nprovider = \"custom:stubvision\"\nmodel = \"stub-vl\"\n\n\
                 [[custom_providers]]\nname = \"stubvision\"\n\
                 base_url = \"http://127.0.0.1:{port}/v1\"\nkey_env = \"stubkey\"\n"
            ),
        )
        .unwrap();
        let mut cfg = veles_core::config::load().unwrap();
        if let Some(o) = owner {
            cfg.acl.owner = vec![o.to_string()];
        }
        cfg
    }

    const COW: &str =
        " ,__,           moo\n (oo)____    /     \n (__)    )\\        \n    ||--|| *       ";

    fn carried(s: &veles_core::tcleval::PublicReplyShaped) -> Vec<serde_json::Value> {
        s.meta
            .get(meta_keys::BSKY_SPEC)
            .map(|j| {
                serde_json::from_str::<serde_json::Value>(j)
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .clone()
            })
            .unwrap_or_default()
    }

    fn of_kind<'a>(recs: &'a [serde_json::Value], k: &str) -> Vec<&'a serde_json::Value> {
        recs.iter().filter(|r| r["kind"] == k).collect()
    }

    fn entry_of(s: &veles_core::tcleval::PublicReplyShaped) -> WithheldEntry {
        let id = s.meta.get(meta_keys::LEDGER_ENTRY).expect("recorded");
        WithheldStore::load().get(id).expect("in the ledger")
    }

    /// The owner's drawing goes out as a picture: drawn, shown to the
    /// image check once (the verdict is kept by the PNG's hash), alt text
    /// from the check, self-labelled with what it saw and what the code
    /// asked for — and held for the owner only on what the hold set names,
    /// on a check that fails, never on an approved drawing, always on one
    /// the owner deleted before. Anyone else's drawing is asked about
    /// first, unchecked.
    #[tokio::test]
    async fn the_owners_drawing_goes_out_as_a_checked_and_labelled_picture() {
        use crate::art::envelope;
        let (_home, _env) = veles_core::testenv::scratch_home();
        let port = stub_bridge_and_scorer();
        crate::settings::pin_for_test(crate::settings::SlopdropSection {
            url: Some(format!("http://127.0.0.1:{port}")),
            output_classifier: vec![format!("http://127.0.0.1:{port}")],
            ..Default::default()
        });
        let (vport, answer, calls) = stub_vision();
        let owned = vision_config(vport, Some("atproto:did:plc:op"));
        let m = msg();
        assert!(asked_by_owner(&owned, &m));
        let say = |a: &str| *answer.lock().unwrap() = a.to_string();
        let n = || calls.load(std::sync::atomic::Ordering::SeqCst);
        async fn shape(
            cfg: &veles_core::config::VelesConfig,
            m: &veles_core::bus::InboundMessage,
            output: &str,
            spec: serde_json::Value,
        ) -> veles_core::tcleval::PublicReplyShaped {
            Policy
                .shape(PublicReplyRequest {
                    cfg,
                    msg: m,
                    code: "cowsay moo",
                    output,
                    invoked_hash: None,
                    spec: Some(&spec),
                })
                .await
        }

        // A clean drawing: a picture and no text.
        say(r#"{"categories": [], "reason": "", "alt": "a cow says moo"}"#);
        let s = shape(&owned, &m, COW, json!([envelope(COW, "", false)])).await;
        let recs = carried(&s);
        let art = of_kind(&recs, "art");
        assert_eq!(art.len(), 1, "{recs:?}");
        assert_eq!(art[0]["alt"], "a cow says moo");
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD
            .decode(art[0]["png"].as_str().unwrap())
            .unwrap();
        assert!(png.starts_with(b"\x89PNG"), "a PNG");
        assert!(of_kind(&recs, "label").is_empty());
        assert_eq!(s.text.trim(), "", "the drawing is the post: {:?}", s.text);
        assert!(!s.meta.contains_key(meta_keys::HOSTED_MD), "no hosted copy");
        let e = entry_of(&s);
        assert_eq!(e.kind, PostKind::Inline, "{}", e.reason);
        assert_eq!(e.output, COW, "the ledger keeps the drawing");
        assert_eq!(n(), 1);
        let again = shape(&owned, &m, COW, json!([envelope(COW, "", false)])).await;
        assert_eq!(of_kind(&carried(&again), "art").len(), 1);
        assert_eq!(n(), 1, "the same drawing is checked once");

        // What the check saw labels it, beside what the code asked for;
        // veles' notes are the text.
        let nude = format!("{COW}\n  nude");
        say(r#"{"categories": ["nudity"], "reason": "a nude cow", "alt": ""}"#);
        let s = shape(
            &owned,
            &m,
            "ignored",
            json!([{"kind": "label", "val": "graphic-media"},
                   envelope(&nude, "(state: proc cow changed)", false)]),
        )
        .await;
        let recs = carried(&s);
        let labels: Vec<&str> = of_kind(&recs, "label")
            .iter()
            .map(|r| r["val"].as_str().unwrap())
            .collect();
        assert_eq!(
            labels,
            ["graphic-media", "nudity"],
            "{:?} / {}",
            s.text,
            entry_of(&s).reason
        );
        assert_eq!(
            of_kind(&recs, "art")[0]["alt"],
            "Text art: moo oo nude",
            "no alt from the check: the words it shows"
        );
        assert!(s.text.contains("(state: proc cow changed)"), "{:?}", s.text);
        assert!(!s.text.contains("ignored"), "{:?}", s.text);

        // The hold set holds the owner's picture: a note and the hosted
        // copy, nothing the code recorded.
        let hateful = format!("{COW}\n  hate");
        say(r#"{"categories": ["hate_symbol"], "reason": "a hate symbol", "alt": ""}"#);
        let s = shape(&owned, &m, "x", json!([envelope(&hateful, "", false)])).await;
        assert!(carried(&s).is_empty(), "nothing rides: {:?}", s.meta);
        assert!(s.text.contains("withheld"), "{:?}", s.text);
        assert!(s.meta.contains_key(meta_keys::HOSTED_MD));
        let e = entry_of(&s);
        assert_eq!(e.kind, PostKind::Withheld);
        assert!(e.reason.contains("hate_symbol"), "{}", e.reason);
        // The ledger keeps the drawing for the question and the approve,
        // and the hosted copy shows it as the picture.
        assert_eq!(e.pictures.len(), 1, "{:?}", e.pictures);
        assert!(crate::art::kept_png(&e.pictures[0].sha).is_some());
        assert_eq!(e.pictures[0].text_hash, output_hash(&hateful));
        assert_eq!(e.post_text.as_deref(), Some(""));
        let html = s.meta.get(meta_keys::HOSTED_HTML).unwrap();
        assert!(html.contains("<img") && html.contains("data:image/png;base64,iVBOR"));
        // …until the owner approves this exact drawing.
        crate::safety::allowlist_add(&output_hash(&hateful)).unwrap();
        let s = shape(&owned, &m, "x", json!([envelope(&hateful, "", false)])).await;
        assert_eq!(
            of_kind(&carried(&s), "art").len(),
            1,
            "approved: {:?}",
            s.text
        );
        assert!(entry_of(&s).reason.contains("approved"));

        // A check that does not answer in the verdict's shape holds it.
        let odd = format!("{COW}\n  odd");
        say("looks fine to me");
        let s = shape(&owned, &m, "x", json!([envelope(&odd, "", false)])).await;
        assert!(carried(&s).is_empty());
        assert_eq!(entry_of(&s).kind, PostKind::Withheld);

        // A drawing the owner deleted before is blocked: a note, no copy.
        let gone = format!("{COW}\n  gone");
        crate::safety::block_add(&output_hash(&gone)).unwrap();
        let before = n();
        let s = shape(&owned, &m, "x", json!([envelope(&gone, "", false)])).await;
        assert_eq!(entry_of(&s).kind, PostKind::Blocked);
        assert!(!s.meta.contains_key(meta_keys::HOSTED_MD));
        assert_eq!(n(), before, "not checked");

        // Text beside a recorded drawing: the text is checked as text and
        // goes out with the picture.
        say(r#"{"categories": [], "reason": "", "alt": "a cow"}"#);
        let s = shape(
            &owned,
            &m,
            "look at this",
            json!([{"kind": "art", "text": COW, "alt": "my cow"},
                   envelope("look at this", "", false)]),
        )
        .await;
        assert!(s.text.starts_with("look at this"), "{:?}", s.text);
        let recs = carried(&s);
        assert_eq!(
            of_kind(&recs, "art")[0]["alt"],
            "my cow",
            "the code's alt wins"
        );
        // …and a text that may not go out withholds the picture too,
        // whose drawing is in the hosted copy beside the text.
        let s = shape(
            &owned,
            &m,
            "see http://x.test",
            json!([{"kind": "art", "text": COW},
                   envelope("see http://x.test", "", false)]),
        )
        .await;
        assert!(carried(&s).is_empty());
        assert_eq!(entry_of(&s).kind, PostKind::Withheld);
        let md = s.meta.get(meta_keys::HOSTED_MD).expect("a hosted copy");
        assert!(
            md.contains("see http://x.test") && md.contains("(oo)____"),
            "the text and the drawing: {md}"
        );

        // A reply that is only a quote has no text to check.
        let s = shape(
            &owned,
            &m,
            "",
            json!([{"kind": "quote", "uri": "at://did:plc:far/app.bsky.feed.post/1"},
                   envelope("", "", false)]),
        )
        .await;
        assert_eq!(s.text, "", "{:?}", s.text);
        assert_eq!(of_kind(&carried(&s), "quote").len(), 1);
        assert_eq!(entry_of(&s).kind, PostKind::Inline);

        // Anyone else's drawing: its own test,
        // `anyones_drawing_passes_the_public_checks_then_the_image_check`.
    }

    /// Anyone else's drawing (P5) goes out after the public checks and the
    /// image check with the stricter set. It is withheld, unshown to the
    /// image check, when the line reaches a bad-list stamp by its `say`
    /// name, when the line names a denylisted word, or when the drawing
    /// shows a link; and by the image check on what only the public set
    /// holds. A drawing the owner approved goes out whatever the check
    /// says. A quote of a thread post whose text fails the word checks is
    /// dropped with a note.
    #[tokio::test]
    async fn anyones_drawing_passes_the_public_checks_then_the_image_check() {
        use crate::art::envelope;
        let (_home, _env) = veles_core::testenv::scratch_home();
        let procs = json!([
            {"name": "STbad", "args": "", "body": "return drawing", "hash": "hstbad",
             "origin": "base"},
        ])
        .to_string();
        let port = stub_bridge_with(procs);
        crate::settings::pin_for_test(crate::settings::SlopdropSection {
            url: Some(format!("http://127.0.0.1:{port}")),
            output_classifier: vec![format!("http://127.0.0.1:{port}")],
            ..Default::default()
        });
        crate::safety::forget_index();
        crate::safety::record("hstbad", crate::safety::Verdict::Unsafe).unwrap();
        std::fs::create_dir_all(crate::data::dir()).unwrap();
        std::fs::write(crate::data::dir().join("denylist.txt"), "zorblax\n").unwrap();
        let (vport, answer, calls) = stub_vision();
        let stranger = vision_config(vport, None);
        let mut m = msg();
        let par = "at://did:plc:par/app.bsky.feed.post/p1";
        let spam = "at://did:plc:par/app.bsky.feed.post/p2";
        m.sender_ctx.post = Some(json!({
            "post": {"uri": "at://did:plc:op/app.bsky.feed.post/ask", "text": "/tcl",
                     "reply": {"parent": par, "root": par}},
            "thread": [{"uri": par, "text": "a fine post"},
                       {"uri": spam, "text": "buy now at http://spam.test"}],
        }));
        assert!(!asked_by_owner(&stranger, &m));
        let say = |a: &str| *answer.lock().unwrap() = a.to_string();
        let n = || calls.load(std::sync::atomic::Ordering::SeqCst);
        async fn shape(
            cfg: &veles_core::config::VelesConfig,
            m: &veles_core::bus::InboundMessage,
            code: &str,
            spec: serde_json::Value,
        ) -> veles_core::tcleval::PublicReplyShaped {
            Policy
                .shape(PublicReplyRequest {
                    cfg,
                    msg: m,
                    code,
                    output: "x",
                    invoked_hash: None,
                    spec: Some(&spec),
                })
                .await
        }
        let withheld = |s: &veles_core::tcleval::PublicReplyShaped, why: &str| {
            assert!(carried(s).is_empty(), "nothing rides: {:?}", s.meta);
            let e = entry_of(s);
            assert_eq!(e.kind, PostKind::Withheld, "{why}: {}", e.reason);
            assert!(e.reason.contains(why), "{} ∌ {why}", e.reason);
        };

        // A clean drawing goes out, checked.
        say(r#"{"categories": [], "reason": "", "alt": "a cow"}"#);
        let s = shape(
            &stranger,
            &m,
            "cowsay moo",
            json!([envelope(COW, "", false)]),
        )
        .await;
        assert_eq!(of_kind(&carried(&s), "art").len(), 1, "{:?}", s.text);
        assert_eq!(n(), 1);

        // What only the public set holds withholds anyone's picture.
        let mocking = format!("{COW}\n  bob");
        say(r#"{"categories": ["harassment"], "reason": "mocks Bob", "alt": ""}"#);
        let s = shape(&stranger, &m, "x", json!([envelope(&mocking, "", false)])).await;
        withheld(&s, "harassment");
        assert_eq!(n(), 2);

        // The public checks decide before the image check is asked.
        let other = |tag: &str| format!("{COW}\n  {tag}");
        let s = shape(
            &stranger,
            &m,
            "badsay hi",
            json!([envelope(&other("stamp"), "", false)]),
        )
        .await;
        withheld(&s, "reaches bad-list proc STbad");
        let s = shape(
            &stranger,
            &m,
            "cowsay zorblax",
            json!([envelope(&other("word"), "", false)]),
        )
        .await;
        withheld(&s, "denylisted");
        let linked = other("visit http://x.test");
        let s = shape(&stranger, &m, "x", json!([envelope(&linked, "", false)])).await;
        withheld(&s, "link or contact");
        let s = shape(
            &stranger,
            &m,
            "x",
            json!([envelope(&other("zorblax"), "", false)]),
        )
        .await;
        withheld(&s, "(denylist)");
        let s = shape(
            &stranger,
            &m,
            "x",
            json!([envelope(&other("blorptox"), "", false)]),
        )
        .await;
        withheld(&s, "toxicity head");
        assert_eq!(n(), 2, "none of these was shown to the image check");

        // The owner approved this exact drawing: it goes out.
        crate::safety::allowlist_add(&output_hash(&mocking)).unwrap();
        let s = shape(&stranger, &m, "x", json!([envelope(&mocking, "", false)])).await;
        assert_eq!(of_kind(&carried(&s), "art").len(), 1, "{:?}", s.text);

        // A quote of a thread post: its text is checked like words.
        let s = shape(
            &stranger,
            &m,
            "bsky_quote [bsky_get_parent]",
            json!([{"kind": "quote", "uri": par}, envelope("", "", false)]),
        )
        .await;
        assert_eq!(of_kind(&carried(&s), "quote").len(), 1, "{:?}", s.text);
        let s = shape(
            &stranger,
            &m,
            "bsky_quote x",
            json!([{"kind": "quote", "uri": spam}, envelope("", "", false)]),
        )
        .await;
        assert!(of_kind(&carried(&s), "quote").is_empty());
        assert!(
            s.text.contains("did not pass the content check"),
            "{:?}",
            s.text
        );

        // A bridge that does not answer leaves no proc index: anyone's
        // drawing is withheld, unshown.
        crate::settings::pin_for_test(crate::settings::SlopdropSection {
            url: Some("http://127.0.0.1:0".to_string()),
            output_classifier: vec![format!("http://127.0.0.1:{port}")],
            ..Default::default()
        });
        crate::safety::forget_index();
        // A drawing the image check has not seen: a call would count.
        say(r#"{"categories": [], "reason": "", "alt": "a cow"}"#);
        let fresh = other("fresh");
        let s = shape(
            &stranger,
            &m,
            "cowsay fresh",
            json!([envelope(&fresh, "", false)]),
        )
        .await;
        withheld(&s, "proc index is unavailable");
        assert_eq!(n(), 2, "not shown to the image check");
    }

    /// A long text (P6): a Bluesky reply over the inline cap goes out as
    /// its head and the link, plus the whole text drawn as one picture
    /// whose alt text is the text, when the text the picture shows passes
    /// the word checks. The scorer reads it in pieces (it refuses a text
    /// it cannot read whole). A tail the checks fail keeps the reply to
    /// the head and the link; a short text is never drawn.
    #[tokio::test]
    async fn a_long_text_goes_out_with_the_whole_text_drawn() {
        use crate::art::envelope;
        let (_home, _env) = veles_core::testenv::scratch_home();
        let port = stub_bridge_and_scorer();
        crate::settings::pin_for_test(crate::settings::SlopdropSection {
            url: Some(format!("http://127.0.0.1:{port}")),
            output_classifier: vec![format!("http://127.0.0.1:{port}")],
            ..Default::default()
        });
        crate::safety::forget_index();
        let cfg = veles_core::config::VelesConfig::default();
        let m = msg();
        let lines = |tail: &str| {
            let mut t: String = (0..70)
                .map(|i| format!("line {i:02}: the quick brown fox jumps over the dog\n"))
                .collect();
            t.push_str(tail);
            t
        };
        async fn shape(
            cfg: &veles_core::config::VelesConfig,
            m: &veles_core::bus::InboundMessage,
            out: &str,
        ) -> veles_core::tcleval::PublicReplyShaped {
            let spec = json!([envelope(out, "", false)]);
            Policy
                .shape(PublicReplyRequest {
                    cfg,
                    msg: m,
                    code: "lines",
                    output: out,
                    invoked_hash: None,
                    spec: Some(&spec),
                })
                .await
        }

        let long = lines("the end");
        assert!(long.len() > 3000, "past what the scorer reads whole");
        let s = shape(&cfg, &m, &long).await;
        assert_eq!(entry_of(&s).kind, PostKind::Inline, "{}", s.text);
        assert!(
            s.text.contains("full output:"),
            "the head and the link: {}",
            s.text
        );
        let recs = carried(&s);
        let art = of_kind(&recs, "art");
        assert_eq!(art.len(), 1, "{recs:?}");
        let alt = art[0]["alt"].as_str().unwrap();
        assert!(alt.starts_with("line 00: the quick"), "{alt}");
        // Alt text holds 2000 characters; the picture holds the rest (the
        // tails below are past both the head and the alt, and decide).
        assert!(alt.chars().count() <= crate::art::ALT_MAX_CHARS);
        assert!(
            alt.ends_with("(cut; the full text is at the link)"),
            "{alt}"
        );
        assert!(art[0]["png"].as_str().is_some_and(|b| !b.is_empty()));

        for (tail, why) in [
            ("blorptox", "the scorer"),
            ("see http://x.test", "a link"),
            ("zorblax", "the denylist"),
        ] {
            std::fs::create_dir_all(crate::data::dir()).unwrap();
            std::fs::write(crate::data::dir().join("denylist.txt"), "zorblax\n").unwrap();
            let s = shape(&cfg, &m, &lines(tail)).await;
            assert_eq!(entry_of(&s).kind, PostKind::Inline, "{why}: {}", s.text);
            assert!(s.text.contains("full output:"), "{why}: {}", s.text);
            assert!(of_kind(&carried(&s), "art").is_empty(), "{why}: not drawn");
        }

        let s = shape(&cfg, &m, "a short answer").await;
        assert!(
            of_kind(&carried(&s), "art").is_empty(),
            "a short text stays text"
        );

        // One withheld for its head keeps the whole text drawn for the
        // owner's question and the approve, and no post text of its own
        // (the approve reposts the head and the link with it).
        let s = shape(&cfg, &m, &format!("blorptox {}", lines("the end"))).await;
        let e = entry_of(&s);
        assert_eq!(e.kind, PostKind::Withheld, "{}", e.reason);
        assert_eq!(e.pictures.len(), 1, "the whole text drawn");
        assert!(e.post_text.is_none(), "{:?}", e.post_text);
        assert!(carried(&s).is_empty(), "nothing rides on the note");
    }

    /// A loopback stand-in for the CDN: serves one PNG at `/cat.png` and
    /// counts the requests.
    fn stub_picture(png: Vec<u8>) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for s in listener.incoming() {
                let Ok(mut s) = s else { continue };
                let mut buf = vec![0u8; 4096];
                let _ = s.read(&mut buf);
                h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        png.len()
                    )
                    .as_bytes(),
                );
                let _ = s.write_all(&png);
            }
        });
        (port, hits)
    }

    /// An image macro (P6): it is fetched from the thread, its ops run,
    /// shown to the image check and attached as a PNG; the owner's is
    /// held with its picture when the check sees what holds the owner's.
    /// A picture the thread does not show is never fetched. Anyone
    /// else's macro (USER, 2026-10-12) passes the word checks on what it
    /// writes, then the image check's stricter set.
    #[tokio::test]
    async fn an_image_macro_is_drawn_checked_and_attached() {
        use crate::art::envelope;
        let (_home, _env) = veles_core::testenv::scratch_home();
        let port = stub_bridge_and_scorer();
        crate::settings::pin_for_test(crate::settings::SlopdropSection {
            url: Some(format!("http://127.0.0.1:{port}")),
            output_classifier: vec![format!("http://127.0.0.1:{port}")],
            ..Default::default()
        });
        crate::safety::forget_index();
        let mut src = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            300,
            200,
            image::Rgba([200, 30, 30, 255]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut src), image::ImageFormat::Png)
        .unwrap();
        let (pport, hits) = stub_picture(src);
        let url = format!("http://127.0.0.1:{pport}/cat.png");
        let (vport, answer, calls) = stub_vision();
        let owned = vision_config(vport, Some("atproto:did:plc:op"));
        let mut m = msg();
        let par = "at://did:plc:par/app.bsky.feed.post/p1";
        m.sender_ctx.post = Some(json!({
            "post": {"uri": "at://did:plc:op/app.bsky.feed.post/ask", "text": "/tcl",
                     "reply": {"parent": par, "root": par}},
            "thread": [{"uri": par, "text": "look", "embed": {"images": [[url, "a red card"]]}}],
        }));
        assert!(asked_by_owner(&owned, &m));
        let say = |a: &str| *answer.lock().unwrap() = a.to_string();
        let n = || calls.load(std::sync::atomic::Ordering::SeqCst);
        let fetched = || hits.load(std::sync::atomic::Ordering::SeqCst);
        async fn shape(
            cfg: &veles_core::config::VelesConfig,
            m: &veles_core::bus::InboundMessage,
            spec: serde_json::Value,
        ) -> veles_core::tcleval::PublicReplyShaped {
            Policy
                .shape(PublicReplyRequest {
                    cfg,
                    msg: m,
                    code: "bsky_caption [bsky_get_parent] TOP BOTTOM",
                    output: "",
                    invoked_hash: None,
                    spec: Some(&spec),
                })
                .await
        }
        let macro_of = |src: &str, ops: &str| {
            json!([{"kind": "magick", "src": src, "ops": ops, "alt": ""},
                   envelope("", "", false)])
        };

        say(r#"{"categories": [], "reason": "", "alt": "a red card that says TOP and BOTTOM"}"#);
        let s = shape(&owned, &m, macro_of(&url, "caption\u{1f}TOP\u{1f}BOTTOM")).await;
        assert_eq!(entry_of(&s).kind, PostKind::Inline, "{}", s.text);
        let recs = carried(&s);
        let art = of_kind(&recs, "art");
        assert_eq!(art.len(), 1, "{recs:?} {}", s.text);
        assert_eq!(art[0]["w"], "300");
        assert_eq!(art[0]["h"], "200");
        assert_eq!(art[0]["alt"], "a red card that says TOP and BOTTOM");
        assert_eq!((fetched(), n()), (1, 1), "fetched once, checked once");

        // A picture the thread does not show is never fetched.
        let other = format!("http://127.0.0.1:{pport}/other.png");
        let s = shape(&owned, &m, macro_of(&other, "flip")).await;
        assert!(of_kind(&carried(&s), "art").is_empty());
        assert!(s.text.contains("not one the thread shows"), "{}", s.text);
        assert_eq!(fetched(), 1);

        // What holds the owner's art holds the macro, with its picture.
        say(r#"{"categories": ["hate_symbol"], "reason": "a symbol", "alt": ""}"#);
        let s = shape(&owned, &m, macro_of(&url, "flop")).await;
        assert!(carried(&s).is_empty());
        let e = entry_of(&s);
        assert_eq!(e.kind, PostKind::Withheld, "{}", e.reason);
        assert_eq!(e.pictures.len(), 1, "kept for the question");
        assert_eq!(fetched(), 2);

        // Anyone else's macro is drawn too (USER, 2026-10-12). The words
        // written on it pass the word checks first, before the image
        // check sees it…
        let stranger = vision_config(vport, None);
        assert!(!asked_by_owner(&stranger, &m));
        let checked = n();
        let s = shape(&stranger, &m, macro_of(&url, "caption\u{1f}blorptox")).await;
        let e = entry_of(&s);
        assert_eq!(e.kind, PostKind::Withheld, "{}", e.reason);
        assert!(e.reason.contains("words in the picture"), "{}", e.reason);
        assert_eq!(n(), checked, "not shown to the image check");
        // …then the image check's stricter set: what the owner's picture
        // posts with, a stranger's is withheld for.
        say(r#"{"categories": ["harassment"], "reason": "mocks someone", "alt": ""}"#);
        let s = shape(&owned, &m, macro_of(&url, "grayscale")).await;
        assert_eq!(entry_of(&s).kind, PostKind::Inline, "{}", s.text);
        let s = shape(&stranger, &m, macro_of(&url, "grayscale")).await;
        assert_eq!(entry_of(&s).kind, PostKind::Withheld, "{}", s.text);
        // A stranger's clean macro goes out, labelled with what the check
        // saw.
        say(r#"{"categories": ["nudity"], "reason": "", "alt": "a pale card"}"#);
        let s = shape(&stranger, &m, macro_of(&url, "invert")).await;
        assert_eq!(entry_of(&s).kind, PostKind::Inline, "{}", s.text);
        let recs = carried(&s);
        assert_eq!(of_kind(&recs, "art").len(), 1, "{recs:?}");
        assert!(
            of_kind(&recs, "label").iter().any(|l| l["val"] == "nudity"),
            "{recs:?}"
        );
    }

    /// The base library's `magick_scheme` (wrap-up round): each call is a
    /// recorded program whose handle the next one names; only the program
    /// no other names is drawn, its words pass the checks, and the handle
    /// the code printed is no post text.
    #[tokio::test]
    async fn a_magick_scheme_drawing_goes_out_without_its_handle() {
        use crate::art::envelope;
        let (_home, _env) = veles_core::testenv::scratch_home();
        let port = stub_bridge_and_scorer();
        crate::settings::pin_for_test(crate::settings::SlopdropSection {
            url: Some(format!("http://127.0.0.1:{port}")),
            output_classifier: vec![format!("http://127.0.0.1:{port}")],
            ..Default::default()
        });
        crate::safety::forget_index();
        let mut src = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            300,
            200,
            image::Rgba([200, 30, 30, 255]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut src), image::ImageFormat::Png)
        .unwrap();
        let (pport, hits) = stub_picture(src);
        let url = format!("http://127.0.0.1:{pport}/cat.png");
        let (vport, answer, _calls) = stub_vision();
        let stranger = vision_config(vport, None);
        let mut m = msg();
        let par = "at://did:plc:par/app.bsky.feed.post/p1";
        m.sender_ctx.post = Some(json!({
            "post": {"uri": "at://did:plc:op/app.bsky.feed.post/ask", "text": "/tcl",
                     "reply": {"parent": par, "root": par}},
            "thread": [{"uri": par, "text": "look", "embed": {"images": [[url, "a red card"]]}}],
        }));
        *answer.lock().unwrap() =
            r#"{"categories": [], "reason": "", "alt": "a mirrored card"}"#.to_string();
        let shape = |spec: serde_json::Value| {
            let (stranger, m) = (&stranger, &m);
            async move {
                Policy
                    .shape(PublicReplyRequest {
                        cfg: stranger,
                        msg: m,
                        code: "mirror_left [bsky_image]",
                        output: "magick:2",
                        invoked_hash: None,
                        spec: Some(&spec),
                    })
                    .await
            }
        };
        let programs = |words: &str| {
            json!([
                {"kind": "magick", "scheme": format!("(flop (image {url}))"), "id": "1", "alt": ""},
                {"kind": "magick", "id": "2", "alt": "",
                 "scheme": format!("(annotate (image \"magick:1\") 16 5 5 white \"{words}\")")},
                envelope("magick:2", "", false),
            ])
        };
        let s = shape(programs("hello")).await;
        let e = entry_of(&s);
        assert_eq!(e.kind, PostKind::Inline, "{} {}", e.reason, s.text);
        assert!(
            !s.text.contains("magick:"),
            "the handle is no text: {:?}",
            s.text
        );
        let recs = carried(&s);
        assert_eq!(
            of_kind(&recs, "art").len(),
            1,
            "only the last program: {recs:?}"
        );
        assert_eq!(of_kind(&recs, "art")[0]["alt"], "a mirrored card");
        assert!(e.output.contains("magick_scheme (annotate"), "{}", e.output);
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "fetched once"
        );
        // The words it writes are checked like any drawing's.
        let s = shape(programs("blorptox")).await;
        let e = entry_of(&s);
        assert_eq!(e.kind, PostKind::Withheld, "{}", e.reason);
        assert!(e.reason.contains("words in the picture"), "{}", e.reason);
        // A program that fails is a note, and nothing is attached.
        let s = shape(json!([
            {"kind": "magick", "scheme": "(image https://elsewhere.test/x.png)", "id": "1", "alt": ""},
            envelope("magick:1", "", false),
        ]))
        .await;
        assert!(of_kind(&carried(&s), "art").is_empty());
        assert!(
            s.text.contains("not a picture the thread shows"),
            "{}",
            s.text
        );
    }
}
