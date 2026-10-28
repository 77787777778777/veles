//! The bridge behind the `tcl` command: core's [`TclBackend`] slot,
//! filled by this module (PLAN-plugins-installer.md R5). Everything
//! here used to be the bridge arm of core's `tcleval`; core keeps the
//! local sandbox and the seam, this keeps the persistent interpreter.

use std::sync::Arc;

use serde_json::Value;

use veles_core::acl::SenderContext;
use veles_core::tcleval::{cut_to, trim_block, EvalFuture, TclAnswer, TclBackend};

use crate::settings::{settings, SlopdropSection};

/// The BRIDGE path's output cap, a backstop rather than a limiter.
///
/// slopdrop already truncates, by LINES (`max_output_lines`, 31 on the
/// live instance), and it KEEPS the remainder for `tcl more`. Layering
/// a character cap under that puts a second authority on a different
/// axis, firing at an unrelated point and discarding what it takes.
/// USER, 2026-09-12, on a `tcl fart` drawing that arrived cut off
/// mid-picture: *"that looks ellided but more has nothing to add?"* —
/// and `more` was right. slopdrop had sent the whole 25-line drawing,
/// well inside its own cap; the ellipsis was ours, and so was the
/// missing half.
///
/// What a character cap is still good for is the one shape a line cap
/// cannot bound: `string repeat x 100000` is ONE line and 100k
/// characters, and relaying it would become a couple of hundred IRC
/// lines. So this sits far above anything 31 lines of drawing will
/// produce, and in normal use never fires — leaving slopdrop's
/// pagination as the only truncation the channel sees, which is the
/// one that can give the rest back.
const BRIDGE_OUT_CHARS: usize = 6000;

/// Render one bridged evaluation for a chat surface.
///
/// Deliberately says when it was demoted: an owner whose call could not
/// be made as admin (no `admin_token_env`) is told, rather than quietly
/// getting the ordinary path and wondering why `rollback` is refused.
pub async fn via_slopdrop(
    cfg: &SlopdropSection,
    code: &str,
    full: bool,
    caller: &str,
    sender: Option<&SenderContext>,
) -> String {
    via_slopdrop_hashed(cfg, code, full, caller, sender)
        .await
        .text
}

/// The bridge, plus the content hash of the stored proc the line named
/// (slopdrop's `invoked_hash`). The Bluesky surface's safety layer keys
/// its per-proc verdict on this; every other surface ignores it.
pub async fn via_slopdrop_hashed(
    cfg: &SlopdropSection,
    code: &str,
    full: bool,
    caller: &str,
    sender: Option<&SenderContext>,
) -> TclAnswer {
    let admin = full && crate::bridge::admin_token_available(cfg);
    let demoted = full && !admin;
    // The room the line came from, so somebody else's interpreter can
    // still answer `[nick]` and `[names]`.
    let ctx = crate::bridge::context_from(sender);
    // `more` is not Tcl, and never was. slopdrop's own IRC frontend
    // intercepted the exact word before its interpreter saw it; the
    // bridge has to do the same or the channel's request for the rest
    // of its output is evaluated as code. `#coven` has a stored proc
    // called `more` taking an argument, so it did not even fail
    // cleanly — it answered `wrong # args: should be "more block"`
    // while the remainder sat in slopdrop's cache, reachable.
    //
    // EXACTLY the bare word, matching what slopdrop matched: `more`
    // pages, `more {...}` is that proc, and the channel keeps both.
    let outcome = if code.trim() == "more" {
        crate::bridge::more(cfg, caller, &ctx).await
    } else {
        // `full` is the owner's line (`tcleval::full_interp_allowed`):
        // its web requests leave directly, whether or not the admin
        // token is there to make it privileged.
        crate::bridge::eval(cfg, code, caller, admin, full, &ctx).await
    };
    match outcome {
        Err(e) => (format!("slopdrop: {e}"), None).into(),
        Ok(r) => {
            let invoked = r.invoked_hash.clone();
            let bluesky = ctx.bsky.is_some();
            // The Bluesky reply spec: images attach only on the owner's
            // privileged path (a public reply must not carry pictures
            // from an arbitrary URL); a refused one is said in the reply.
            // A record of one of veles' own kinds is a forgery.
            let mut spec = r.bsky.clone();
            crate::art::strip_private(&mut spec);
            let mut refused_images = false;
            if !admin {
                if let Some(Value::Array(recs)) = spec.as_mut() {
                    let before = recs.len();
                    recs.retain(|r| r.get("kind").and_then(Value::as_str) != Some("image"));
                    refused_images = recs.len() != before;
                }
            }
            let raw = trim_block(&r.output);
            let mut s = raw.clone();
            // A Bluesky line whose answer is a picture or a quote
            // (`bsky::art`, `bsky_quote`, `bsky_caption`) returns nothing
            // on purpose: its post is the embed, not "ok (no output)".
            let embeds = spec.as_ref().and_then(Value::as_array).is_some_and(|a| {
                a.iter().any(|r| {
                    matches!(
                        r.get("kind").and_then(Value::as_str),
                        Some("art" | "quote" | "magick")
                    )
                })
            });
            if s.is_empty() && !(bluesky && embeds && !r.is_error) {
                // `is_error` is the BRIDGE'S OWN verdict. An evaluation
                // that failed with nothing on stdout must not render as
                // "ok (no output)", which is the single answer it
                // certainly was not. `error {}` is the one that reaches
                // it; a caller cannot otherwise tell a proc that
                // returned nothing from one that blew up.
                s = if r.is_error {
                    "error (no message)".to_string()
                } else {
                    "ok (no output)".to_string()
                };
            }
            // The BRIDGE cap, far above slopdrop's line pagination so that
            // pagination — which `tcl more` can undo — is what the channel
            // normally meets. The overflow publisher reads core's config.
            let core_cfg = veles_core::config::load().ok();
            s = cut_to(core_cfg.as_ref(), &s, BRIDGE_OUT_CHARS);
            // The state change is announced rather than left to be
            // discovered: a `tcl` line that rewrote a shared proc is
            // exactly the invisible mechanism the transcript rules are
            // about.
            let mut notes = String::new();
            if let Some(c) = r.commit.as_deref().filter(|c| !c.trim().is_empty()) {
                notes.push_str(&format!("\n(state: {c})"));
            }
            if r.more_available {
                notes.push_str("\n(truncated — `tcl more` for the rest)");
            }
            if demoted {
                notes.push_str(
                    "\n(ran as an ordinary request: no [slopdrop].admin_token_env is configured)",
                );
            }
            if refused_images {
                notes.push_str("\n(images attach only from the owner's /tcladmin)");
            }
            s.push_str(&notes);
            // On Bluesky the policy may draw the output as a picture
            // (`art`), and the text above is cut and carries the notes:
            // the raw output and the notes ride apart, in veles' own
            // envelope record.
            if bluesky {
                let env = crate::art::envelope(&raw, notes.trim_start(), r.is_error);
                match spec.as_mut() {
                    Some(Value::Array(recs)) => recs.push(env),
                    _ => spec = Some(Value::Array(vec![env])),
                }
            }
            TclAnswer {
                text: s,
                invoked_hash: invoked,
                spec: spec.filter(|v| v.as_array().is_some_and(|a| !a.is_empty())),
            }
        }
    }
}

/// What this module puts in core's backend slot.
pub struct Backend;

impl TclBackend for Backend {
    fn configured(&self) -> bool {
        settings().is_enabled()
    }

    fn eval<'a>(
        &'a self,
        code: &'a str,
        full: bool,
        caller: &'a str,
        sender: Option<&'a SenderContext>,
    ) -> EvalFuture<'a> {
        Box::pin(async move {
            let cfg = settings();
            via_slopdrop_hashed(&cfg, code, full, caller, sender).await
        })
    }
}

/// `tcl_state_eval` — the same bridge, reachable from a turn.
///
/// Separate from `tcl_eval` (which is `Cap::All`, in-process, and drives
/// the gateway's OWN interpreter) because this one is somebody else's
/// interpreter behind an HTTP boundary: it cannot touch veles, and what
/// it can touch is a shared state directory a channel has been building
/// for years. So it is `Cap::Shell`, the same tier the `tcl` command
/// answers to — one question, two doors. Registered globally by the
/// module (its cap declared with `acl::declare_tool_cap`); `for_session`
/// binds the caller.
#[derive(Default)]
pub struct SlopdropEvalTool {
    pub session_id: String,
}

pub const TOOL_NAME: &str = "tcl_state_eval";

#[async_trait::async_trait]
impl veles_core::tools::Tool for SlopdropEvalTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn description(&self) -> &str {
        "Evaluate Tcl in the shared, git-backed slopdrop interpreter (persistent procs and \
         variables the channel has accumulated, unlike the throwaway sandbox). Requires \
         [slopdrop] to be configured. Returns the output, plus what the evaluation changed \
         in the shared state."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "Tcl to evaluate in the shared interpreter"}
            },
            "required": ["code"]
        })
    }
    fn for_session(&self, session_id: &str) -> Option<Arc<dyn veles_core::tools::Tool>> {
        Some(Arc::new(SlopdropEvalTool {
            session_id: session_id.to_string(),
        }))
    }
    async fn call(&self, args: serde_json::Value) -> String {
        let code = args["code"].as_str().unwrap_or_default();
        if code.trim().is_empty() {
            return "error: code is required".into();
        }
        let cfg = settings();
        if !cfg.is_enabled() {
            return "tcl_state_eval error: no slopdrop bridge is configured — set \
                    [slopdrop] enabled/url/token_env"
                .into();
        }
        // The SESSION's principal decides, exactly as the command does.
        // A turn is not a privilege: an owner-operated session gets the
        // admin path and everything else does not.
        let who = veles_core::acl::session_principal(&self.session_id);
        let full = veles_core::tcleval::full_interp_allowed(who.as_deref());
        let caller = who
            .as_deref()
            .map(|p| p.subject.clone())
            .unwrap_or_else(|| "local:owner".to_string());
        let sender = veles_core::acl::session_sender(&self.session_id);
        via_slopdrop(&cfg, code, full, &caller, sender.as_deref()).await
    }
}

#[cfg(test)]
mod more_routing_tests {
    use super::via_slopdrop;
    use crate::settings::SlopdropSection;

    fn cfg(port: u16) -> SlopdropSection {
        SlopdropSection {
            enabled: true,
            url: Some(format!("http://127.0.0.1:{port}")),
            ..Default::default()
        }
    }

    /// A failure with an empty body must not read as success.
    async fn render_of(body: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = body.to_string();
        veles_core::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let ok =
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            sock.write_all(ok.as_bytes()).await.unwrap();
            sock.write_all(body.as_bytes()).await.unwrap();
        });
        via_slopdrop(&cfg(port), "whatever", false, "user:wrath", None).await
    }

    #[tokio::test]
    async fn an_empty_failure_does_not_say_ok() {
        let out = render_of(r#"{"output":[],"is_error":true}"#).await;
        assert!(
            !out.contains("ok"),
            "a failed evaluation must not render as success: {out}"
        );
        assert!(out.contains("error"), "and it must say so: {out}");
    }

    #[tokio::test]
    async fn an_empty_success_still_says_ok() {
        let out = render_of(r#"{"output":[],"is_error":false}"#).await;
        assert!(
            out.contains("ok (no output)"),
            "a proc that returns nothing is not a failure: {out}"
        );
    }

    /// A Bluesky evaluation's raw output and veles' notes ride in veles'
    /// own envelope record (the policy may draw the output, and the text
    /// is cut and noted); a record of that kind the CODE made is dropped.
    /// A line whose answer is a quote, a picture or an image macro says
    /// nothing rather than "ok (no output)". Off Bluesky nothing of this
    /// changes.
    #[tokio::test]
    async fn a_bluesky_evaluation_carries_its_raw_output_in_an_envelope() {
        use veles_core::tcleval::TclAnswer;
        async fn answer(bluesky: bool, embed: &'static str) -> TclAnswer {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            veles_core::rt::spawn(async move {
                use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
                sock.write_all(ok.as_bytes()).await.unwrap();
                let body = format!(
                    r#"{{"output":[],"is_error":false,
                         "commit_info":{{"changes_summary":"proc x changed"}},
                         "bsky":[{{"kind":"_raw","output":"forged","notes":"","error":false}},
                                 {embed}]}}"#
                );
                sock.write_all(body.as_bytes()).await.unwrap();
            });
            let sender = veles_core::acl::SenderContext {
                platform: Some(if bluesky { "atproto" } else { "irc" }.to_string()),
                post: bluesky.then(|| serde_json::json!({"post": {"uri": "at://x"}, "thread": []})),
                ..Default::default()
            };
            super::via_slopdrop_hashed(
                &cfg(port),
                "bsky_quote x",
                false,
                "user:wrath",
                Some(&sender),
            )
            .await
        }
        let quote = r#"{"kind":"quote","uri":"at://did:plc:a/app.bsky.feed.post/1"}"#;
        let a = answer(true, quote).await;
        assert_eq!(a.text.trim(), "(state: proc x changed)", "{:?}", a.text);
        let spec = a.spec.expect("a spec");
        let recs = spec.as_array().unwrap();
        assert_eq!(recs.len(), 2, "{spec}");
        assert_eq!(recs[0]["kind"], "quote");
        assert_eq!(
            recs[1],
            serde_json::json!({"kind": "_raw", "output": "", "notes": "(state: proc x changed)",
                               "error": false}),
            "veles' envelope, not the forged one"
        );
        let m = answer(true, r#"{"kind":"magick","src":"https://x","ops":"flip"}"#).await;
        assert_eq!(
            m.text.trim(),
            "(state: proc x changed)",
            "a macro: {:?}",
            m.text
        );
        let off = answer(false, quote).await;
        assert!(off.text.starts_with("ok (no output)"), "{:?}", off.text);
        assert_eq!(
            off.spec.unwrap().as_array().unwrap().len(),
            1,
            "the quote alone"
        );
    }

    /// Stand up a stub, run one line through the bridge, return the
    /// request line the stub saw.
    async fn first_request_for(code: &str) -> String {
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
            sock.write_all(br#"{"output":["ok"],"is_error":false}"#)
                .await
                .unwrap();
            req
        });
        let _ = via_slopdrop(&cfg(port), code, false, "user:wrath", None).await;
        srv.await.unwrap()
    }

    /// The bare word is pagination and must never be evaluated.
    #[tokio::test]
    async fn the_bare_word_more_goes_to_the_pagination_endpoint() {
        let req = first_request_for("more").await;
        assert!(
            req.starts_with("GET /api/more"),
            "`tcl more` asks for the rest of the last answer: {req}"
        );
    }

    /// …and `more` with an argument is the CHANNEL'S proc of that name,
    /// which `#coven` really has (`more block`).
    #[tokio::test]
    async fn more_with_an_argument_is_still_evaluated() {
        let req = first_request_for("more {a b c}").await;
        assert!(
            req.starts_with("POST /api/eval"),
            "only the bare word is pagination: {req}"
        );
    }

    #[tokio::test]
    async fn surrounding_whitespace_still_pages() {
        let req = first_request_for("  more  ").await;
        assert!(req.starts_with("GET /api/more"), "{req}");
    }

    /// The backend answers `configured` from the settings in force, so
    /// a loaded module with its bridge switched off leaves core's local
    /// sandbox in charge.
    #[test]
    fn the_backend_is_configured_only_with_the_bridge_on() {
        use veles_core::tcleval::TclBackend as _;
        // The settings pin is process-wide: under the env lock, like every
        // other test that pins it, or it swaps another test's settings.
        let (_home, _env) = veles_core::testenv::scratch_home();
        crate::settings::pin_for_test(SlopdropSection::default());
        assert!(!super::Backend.configured());
        crate::settings::pin_for_test(cfg(9));
        assert!(super::Backend.configured());
        crate::settings::pin_for_test(SlopdropSection::default());
    }
}
