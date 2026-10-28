//! The withheld ledger as a pending-decision source for the sync loop
//! (PLAN-plugins-installer.md R5; core's `syncask::PendingSource`). A
//! public `/tcl` reply the output check (or the bad list) withheld is
//! asked about on the owner's leg: approve (repost inline, allowlist),
//! delete (take it down, block), keep it withheld, or keep it and stop
//! asking. The decision id is `tcl:<entry id>`, the spelling the
//! linked version used, so the book of asked decisions carries over.

use veles_core::ask::{AskChoice, AskKind};
use veles_core::syncask::{PendingSource, SourceOutcome, SourceQuestion};

use crate::withheld::{PostKind, WithheldStore};

pub const SOURCE: &str = "tcl";

/// At most this many of one requester's withheld replies are asked about
/// per hour (PLAN-slopdrop-art.md): the rest wait, still listed and still
/// deletable, so a stranger who posts drawing after drawing cannot flood
/// the owner's phone.
pub const ASKS_PER_HOUR: usize = 5;
const HOUR: u64 = 3600;

/// The withheld entries to ask about now: open ones, at most
/// [`ASKS_PER_HOUR`] per requester among those withheld in the last hour
/// (the oldest first; older ones are always asked).
fn askable(entries: Vec<crate::withheld::WithheldEntry>, now: u64) -> Vec<String> {
    let mut open: Vec<_> = entries
        .into_iter()
        .filter(|e| !e.no_ask && e.kind == PostKind::Withheld)
        .collect();
    open.sort_by_key(|e| e.at);
    let mut recent: std::collections::HashMap<String, usize> = Default::default();
    let mut out = Vec::new();
    for e in open {
        if now.saturating_sub(e.at) < HOUR {
            let n = recent.entry(e.requester.clone()).or_default();
            if *n >= ASKS_PER_HOUR {
                continue;
            }
            *n += 1;
        }
        out.push(e.id);
    }
    out
}

pub struct Source;

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

impl PendingSource for Source {
    /// Withheld public `/tcl` replies awaiting approve/delete. The store
    /// is read live; an entry the owner already decided is gone from it
    /// (or, approved, is inline now), and one they marked "keep + stop
    /// asking" (`no_ask`) is left but never asked. Inline replies and
    /// blocked notes are recorded only so they can be deleted: nothing
    /// to ask.
    fn scan(&self) -> Vec<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        askable(WithheldStore::load().list(), now)
    }

    fn question(&self, id: &str, _node_name: &str) -> SourceQuestion {
        // The withheld entry carries the requester, the reason, the proc
        // and the first 600 chars of the output, plus the link once the
        // adapter has filled it in.
        let mut image = None;
        let (prompt, detail) = match WithheldStore::load().get(id) {
            Some(e) => {
                // A held drawing is judged by looking at it: the first
                // picture rides the question (Telegram sends it as a
                // photo above it), and the detail says what else it has.
                let mut pictures = String::new();
                if let Some(first) = e.pictures.first() {
                    use base64::Engine as _;
                    image = crate::art::kept_png(&first.sha)
                        .map(|b| base64::engine::general_purpose::STANDARD.encode(b));
                    pictures = format!(
                        "\n{} picture(s){}; alt: {}",
                        e.pictures.len(),
                        if e.labels.is_empty() {
                            String::new()
                        } else {
                            format!(", labelled {}", e.labels.join(", "))
                        },
                        cut(&first.alt, 200)
                    );
                }
                let proc = e.proc_name.as_deref().unwrap_or("(ad-hoc)");
                let link = e
                    .link
                    .clone()
                    .unwrap_or_else(|| "(no hosted copy)".to_string());
                let post = e
                    .post_uris
                    .first()
                    .and_then(|u| crate::withheld::post_link(u))
                    .map(|l| format!("\npost: {l}"))
                    .unwrap_or_default();
                (
                    format!(
                        "a public /tcl reply from {} was withheld ({}) — approve it \
                         (repost inline), delete it (take it down), keep it withheld \
                         (asks again in a day), or keep it and stop asking?",
                        e.requester, e.reason
                    ),
                    format!(
                        "proc {proc}{pictures}\n{}\n{link}{post}",
                        cut(&e.output, 600)
                    ),
                )
            }
            None => (
                format!("withheld /tcl reply {id} is gone from the store — keep it closed?"),
                String::new(),
            ),
        };
        SourceQuestion {
            kind: AskKind::Approval,
            prompt,
            detail,
            choices: vec![
                AskChoice::new("approve", "repost it inline"),
                AskChoice::new("delete", "take it down"),
                AskChoice::new("keep", "keep it withheld (asks again in a day)"),
                AskChoice::new("keepsilent", "keep it withheld and stop asking"),
            ],
            image,
        }
    }

    fn decide(&self, id: &str, pick: &str, _free_text: &str) -> anyhow::Result<SourceOutcome> {
        match pick {
            "approve" | "delete" => {
                let action = if pick == "approve" {
                    crate::publish::WithheldAction::Approve
                } else {
                    crate::publish::WithheldAction::Delete
                };
                // `decide` is sync but the publisher is async; block on it
                // here (the sync loop runs decisions on a blocking thread,
                // so a repost that takes seconds does not stall the
                // gateway loop).
                let outcome = match tokio::runtime::Handle::try_current() {
                    Ok(h) => h.block_on(crate::publish::act(id, action)),
                    Err(_) => Err("no async runtime to reach the publisher".to_string()),
                };
                match outcome {
                    Ok(report) => Ok(SourceOutcome::Decided(format!(
                        "/tcl {id} {pick}: {report}"
                    ))),
                    Err(e) => anyhow::bail!("tcl withheld {pick} {id}: {e}"),
                }
            }
            "keepsilent" => {
                // Keep the entry (still listed, still deletable) but never
                // re-ask: the `no_ask` flag takes it out of `scan`, so a
                // decided outcome here is never re-derived into an ask.
                WithheldStore::load()
                    .set_no_ask(id, true)
                    .map_err(|e| anyhow::anyhow!("keepsilent {id}: {e}"))?;
                Ok(SourceOutcome::Decided(format!(
                    "/tcl withheld {id} kept withheld; not asking again"
                )))
            }
            // "keep", a plain yes/no, anything else: kept, asked again.
            _ => Ok(SourceOutcome::Later(format!(
                "/tcl withheld {id} kept withheld"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::withheld::WithheldEntry;

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// A held drawing is asked about with the picture itself (Telegram
    /// sends it above the question), and the detail says how many there
    /// are, their labels and the first one's alt text.
    #[test]
    fn a_held_drawing_is_asked_with_its_picture() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let pic = crate::art::draw(&crate::art::Source {
            text: " ,__,  moo\n (oo)____\n (__)    )\\".into(),
            alt: String::new(),
            theme: veles_textart::Theme::Dark,
        })
        .unwrap();
        WithheldStore::load()
            .insert(WithheldEntry {
                id: "c0ffee00".into(),
                at: now(),
                requester: "someone".into(),
                output: "the cow".into(),
                reason: "the image check saw slur".into(),
                pictures: vec![pic.reference("a cow says moo")],
                labels: vec!["nudity".into()],
                ..Default::default()
            })
            .unwrap();
        let q = Source.question("c0ffee00", "vps");
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD
            .decode(q.image.expect("the picture rides the question"))
            .unwrap();
        assert_eq!(png, pic.png);
        assert!(
            q.detail
                .contains("1 picture(s), labelled nudity; alt: a cow says moo"),
            "{}",
            q.detail
        );
        let plain = WithheldEntry {
            id: "c0ffee01".into(),
            at: now(),
            output: "text".into(),
            ..Default::default()
        };
        WithheldStore::load().insert(plain).unwrap();
        assert!(Source.question("c0ffee01", "vps").image.is_none());
    }

    /// One requester is asked about at most five withheld replies an hour;
    /// the rest wait (oldest first), and an older one is always asked.
    #[test]
    fn one_requester_is_asked_at_most_five_times_an_hour() {
        let t = 1_800_000_000;
        let e = |id: &str, who: &str, at: u64| WithheldEntry {
            id: id.into(),
            requester: who.into(),
            at,
            ..Default::default()
        };
        let mut entries = vec![e("old", "a", t - 2 * HOUR)];
        for i in 0..7 {
            entries.push(e(&format!("a{i}"), "a", t - 600 + i));
        }
        entries.push(e("b0", "b", t - 10));
        let mut done = e("a9", "a", t - 1);
        done.kind = PostKind::Inline;
        entries.push(done);
        let asked = askable(entries, t);
        assert_eq!(asked, ["old", "a0", "a1", "a2", "a3", "a4", "b0"]);
    }

    /// "keep + stop asking" sets the entry's no_ask flag, keeps it in the
    /// store (still listed, still deletable), and removes it from `scan`,
    /// so it is never asked about again (it expires silently at 7 days).
    #[test]
    fn keepsilent_keeps_the_entry_but_never_asks_again() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let ws = WithheldStore::load();
        ws.insert(WithheldEntry {
            id: "deadbeef".into(),
            at: now(),
            requester: "atproto:did:plc:x".into(),
            chat_id: "at://x".into(),
            request_msg_id: "at://x/post/1".into(),
            code: "draw".into(),
            output: "┌───┐".into(),
            reason: "art or non-ascii".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(Source.scan().contains(&"deadbeef".to_string()));
        let q = Source.question("deadbeef", "laptop");
        assert!(
            q.prompt.contains("was withheld (art or non-ascii)"),
            "{}",
            q.prompt
        );
        assert_eq!(q.choices.len(), 4);
        let out = Source.decide("deadbeef", "keepsilent", "").unwrap();
        assert!(matches!(out, SourceOutcome::Decided(_)));
        let kept = ws.get("deadbeef").expect("the entry stays");
        assert!(kept.no_ask, "flagged no_ask");
        assert!(!Source.scan().contains(&"deadbeef".to_string()));
        // "keep" and a plain yes are "later".
        ws.set_no_ask("deadbeef", false).unwrap();
        assert!(matches!(
            Source.decide("deadbeef", "keep", "").unwrap(),
            SourceOutcome::Later(_)
        ));
        assert!(matches!(
            Source
                .decide("deadbeef", veles_core::ask::CHOICE_ONCE, "")
                .unwrap(),
            SourceOutcome::Later(_)
        ));
    }

    /// Inline replies and blocked notes are recorded so the owner can
    /// delete them, never to be asked about: only a withheld one asks.
    #[test]
    fn only_a_withheld_reply_is_asked_about() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let ws = WithheldStore::load();
        for (id, kind) in [
            ("aaaa0001", PostKind::Withheld),
            ("aaaa0002", PostKind::Inline),
            ("aaaa0003", PostKind::Blocked),
        ] {
            ws.insert(WithheldEntry {
                id: id.into(),
                kind,
                at: now(),
                output: format!("output of {id}"),
                ..Default::default()
            })
            .unwrap();
        }
        assert_eq!(Source.scan(), vec!["aaaa0001".to_string()]);
    }
}
