//! `tcl` / `eval` — evaluating a line of Tcl for a caller, without a turn.
//!
//! WHY THIS MODULE EXISTS
//!
//! The implementation used to live in `veles-irc`'s `run_tcl_line`, which
//! made it reachable from exactly one spelling on exactly one surface: the
//! IRC command sigil (`.tcl …`), answered by the adapter before the bus.
//! The OTHER spelling of the same command — a literal `/tcl …`, which some
//! clients pass through untouched — had no tier in `acl::builtin_cap` and
//! no arm in the shared dispatcher, so it fell through to `send_inbound`
//! and reached the MODEL as a literal prompt. USER, 2026-09-11:
//!
//!   *"please make sure that .tcl|/tcl bypass the llm so that they don't
//!   consume a turn for a simple request from a dumb machine"*
//!
//! Half of that was already true and half of it was a billed provider call
//! to compute `expr {1+1}`. Making it true on every surface means the
//! evaluator has to be somewhere every surface can call, which is here —
//! and `sandbox::execute` and `executecode::tclsh_path` were already in
//! this crate, so nothing moved but the seam.
//!
//! THE TWO SEAMS (PLAN-plugins-installer.md R5)
//!
//! A persistent interpreter behind the same command — the slopdrop
//! bridge — is a MODULE's, not core's. It installs itself in the
//! [`TclBackend`] slot; with a configured backend in the slot every
//! surface's `tcl` goes there, and without one the local throwaway
//! sandbox answers, exactly as it did before a bridge existed. The
//! second seam is the PUBLIC surface: what a `/tcl` reply may carry on a
//! public feed is the module's decision too ([`PublicTclReply`]), and
//! with no policy installed the gateway posts no output at all (it
//! fails closed: [`PUBLIC_REPLY_UNAVAILABLE`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

/// Wall-clock cap on one evaluation. The sandbox backend enforces it too;
/// this is the outer bound for the `full` path, which is a plain process.
const CAP_SECS: u64 = 30;

/// How much output a chat surface will carry, LOCAL path. The throwaway
/// sandbox has no pagination, so what is cut here is gone — which is
/// the reason to cut early and say so.
const OUT_CHARS: usize = 1200;

/// Cut to `max` characters — and give the rest back if we can.
///
/// A bare `…` is indistinguishable from a bridge's own pagination,
/// which IS retrievable, so a reader does the reasonable thing, types
/// `tcl more`, and is told there is nothing cached. With publishing
/// unconfigured, say plainly that the remainder is gone rather than
/// implying a command that cannot recover it.
pub fn cut_to(cfg: Option<&crate::config::VelesConfig>, s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    match cfg.and_then(|c| crate::publish::code_block(c, "tcl-output.md", s)) {
        Some(url) => format!("{head} … (cut — the whole thing: {url})"),
        None => {
            format!("{head} … (cut here: too long for the channel, and this remainder is not kept)")
        }
    }
}

/// Trim the blank space AROUND a block of output without eating the
/// indentation INSIDE it.
///
/// `str::trim` is the obvious call here and it is wrong, in a way that
/// is only visible once the output is more than one line. Trimming the
/// whole blob takes the leading spaces off the FIRST line and no
/// other, because the first line is the only one at the string's edge.
/// A drawing therefore arrives with its top row shifted left and every
/// row below it untouched — which looks like corruption rather than
/// like trimming, and `#coven` has a decade of stored procs that draw.
/// USER, 2026-09-12, on `tcl STgoon`:
///
///   *"the first line is unindented and all the others are indented,
///   the output is 'corrupted' for multiline procs"*
///
/// So: drop wholly-blank lines at either end, drop trailing whitespace,
/// and never touch leading whitespace on a line that has content.
pub fn trim_block(s: &str) -> String {
    let mut lines: Vec<&str> = s.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n").trim_end().to_string()
}

/// Is the privileged path being ASKED for, and may this caller have it?
///
/// Two separate questions, and conflating them was a live bug. The
/// bridge originally offered slopdrop's admin path to any caller veles
/// resolved as owner, on every `tcl` line — so an owner typing
/// `tcl expr {6*7}` had it sent as an ADMIN request, and slopdrop's own
/// `privileged_users` hostmask check then refused it. The owner could
/// not run the ordinary command at all.
///
/// slopdrop never worked that way: `tcl` and `tclAdmin` are two
/// commands, and which one you type decides. The role decides whether
/// the privileged spelling is PERMITTED, not whether it is used. So
/// `want_admin` comes from the spelling and `full_interp_allowed` from
/// the principal, and the privileged path needs both.
pub fn admin_requested_and_allowed(
    want_admin: bool,
    principal: Option<&crate::acl::Principal>,
) -> bool {
    want_admin && full_interp_allowed(principal)
}

/// May this caller have the FULL `tclsh` rather than the safe interp?
///
/// The rule, unchanged from the IRC arm it came from: the full
/// interpreter is the configured OWNER's, and in dormant mode (no `[acl]`
/// section at all) it is NOBODY's — because dormant resolves every
/// stranger as owner, and "whoever spoke first gets exec" is the eggdrop
/// mistake. `None` is a local owner surface (REPL, TUI, desktop, CLI): the
/// human at that keyboard already owns the machine, which is the same
/// answer `user_envs::zone_for` gives them.
///
/// This mirrors slopdrop's own `tcl` / `tclAdmin` split, which is the
/// split not to flatten.
pub fn full_interp_allowed(principal: Option<&crate::acl::Principal>) -> bool {
    match principal {
        None => true,
        Some(p) => {
            let configured = crate::config::load()
                .map(|c| c.acl)
                .unwrap_or_default()
                .is_configured();
            configured && p.has(crate::acl::Cap::All)
        }
    }
}

/// The display identity for a caller: the platform's own handle/nick, NOT
/// the veles ACL subject (`user:wrath`, `atproto:did:…`). A proc that
/// prints `$who` (the goon family, `[or $who [nick]]`) should show the
/// handle the reader knows, not an internal key. Falls back to the subject
/// when the platform gives no display name (a REPL, a cron, a bare mask).
pub fn display_caller(sender: Option<&crate::acl::SenderContext>, subject: &str) -> String {
    let Some(s) = sender else {
        return subject.to_string();
    };
    if let Some(h) = s.handle.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        return h.to_string();
    }
    let mask = s.mask.as_deref().unwrap_or_default();
    let nick = mask.split('!').next().unwrap_or(mask).trim();
    if nick.is_empty() {
        subject.to_string()
    } else {
        nick.to_string()
    }
}

// ── the backend slot ──────────────────────────────────────────────────

/// What one evaluation answered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TclAnswer {
    /// The rendered answer.
    pub text: String,
    /// The content hash of the stored proc the line named, when the
    /// backend knew one: what the public-surface policy classifies by.
    pub invoked_hash: Option<String>,
    /// What a public feed's reply should carry beyond its text, as the
    /// code recorded it (Bluesky: slopdrop's `bsky::link`, `bsky::tag`,
    /// … — a JSON array), for the public-reply policy to validate. `None`
    /// everywhere else.
    pub spec: Option<serde_json::Value>,
}

/// A backend that knows only text and a hash answers the old pair.
impl From<(String, Option<String>)> for TclAnswer {
    fn from((text, invoked_hash): (String, Option<String>)) -> Self {
        Self {
            text,
            invoked_hash,
            spec: None,
        }
    }
}

pub type EvalFuture<'a> = Pin<Box<dyn Future<Output = TclAnswer> + Send + 'a>>;

/// A persistent interpreter behind the `tcl` command (the slopdrop
/// module's bridge). One install point, owner-tracked: a module installs
/// it in `start`, and its purge on unload brings the local sandbox back.
pub trait TclBackend: Send + Sync {
    /// Is the backend configured to answer at all? A module that is
    /// loaded but whose bridge is switched off answers false, and the
    /// local sandbox serves.
    fn configured(&self) -> bool;
    /// The same contract as [`eval_line_hashed`]: the rendered answer,
    /// plus the content hash of the stored proc the line named (what the
    /// public-surface policy classifies by), when the backend knows one.
    fn eval<'a>(
        &'a self,
        code: &'a str,
        full: bool,
        caller: &'a str,
        sender: Option<&'a crate::acl::SenderContext>,
    ) -> EvalFuture<'a>;
}

fn backend_slot() -> &'static crate::owned::OwnedSlot<Arc<dyn TclBackend>> {
    static S: OnceLock<crate::owned::OwnedSlot<Arc<dyn TclBackend>>> = OnceLock::new();
    S.get_or_init(crate::owned::OwnedSlot::default)
}

/// Install `backend` for the calling thread's owner (M4: a module's
/// install is purged with it).
pub fn set_backend(backend: Arc<dyn TclBackend>) {
    backend_slot().set(backend);
}

/// Uninstall what the CALLING owner installed.
pub fn clear_backend() {
    backend_slot().clear();
}

/// Remove `owner`'s backend (R3 teardown). `None` (core) never matches.
pub fn purge_owner_backend(owner: Option<&'static str>) -> bool {
    backend_slot().purge(owner)
}

fn backend() -> Option<Arc<dyn TclBackend>> {
    backend_slot().with(Arc::clone)
}

/// Is a persistent interpreter installed AND configured? The callers
/// that decide whether a line is theirs before evaluating (the IRC
/// adapter's bare-`tcl` arm) ask this; it is cheap (no config load).
pub fn backend_configured() -> bool {
    backend().is_some_and(|b| b.configured())
}

// ── the public-surface policy slot ────────────────────────────────────

/// What the gateway hands the policy for one public `/tcl`.
pub struct PublicReplyRequest<'a> {
    pub cfg: &'a crate::config::VelesConfig,
    pub msg: &'a crate::bus::InboundMessage,
    /// The line as typed (after the command word).
    pub code: &'a str,
    /// The evaluation's rendered output.
    pub output: &'a str,
    /// The stored proc the line named, when the backend knew one.
    pub invoked_hash: Option<&'a str>,
    /// What the code recorded for the reply beyond its text
    /// ([`TclAnswer::spec`]), unvalidated: the policy decides what of it
    /// the post may carry.
    pub spec: Option<&'a serde_json::Value>,
}

/// What the policy decided the post carries, and the `meta` the
/// platform's plugin reads (a hosted copy to publish, the ledger entry
/// the post belongs to).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PublicReplyShaped {
    pub text: String,
    pub meta: std::collections::BTreeMap<String, String>,
}

pub type ShapeFuture<'a> = Pin<Box<dyn Future<Output = PublicReplyShaped> + Send + 'a>>;

/// The decision what a `/tcl` reply may carry on a PUBLIC feed. Core has
/// no opinion of its own: without an installed policy the gateway posts
/// [`PUBLIC_REPLY_UNAVAILABLE`] and never the output (fail closed).
pub trait PublicTclReply: Send + Sync {
    fn shape<'a>(&'a self, req: PublicReplyRequest<'a>) -> ShapeFuture<'a>;
}

/// What a public feed gets for `/tcl` with no policy loaded.
pub const PUBLIC_REPLY_UNAVAILABLE: &str =
    "/tcl is not available on this feed right now (the slopdrop module is not loaded)";

fn public_reply_slot() -> &'static crate::owned::OwnedSlot<Arc<dyn PublicTclReply>> {
    static S: OnceLock<crate::owned::OwnedSlot<Arc<dyn PublicTclReply>>> = OnceLock::new();
    S.get_or_init(crate::owned::OwnedSlot::default)
}

/// Install the public-surface policy for the calling thread's owner.
pub fn set_public_reply(policy: Arc<dyn PublicTclReply>) {
    public_reply_slot().set(policy);
}

/// Uninstall what the CALLING owner installed.
pub fn clear_public_reply() {
    public_reply_slot().clear();
}

/// Remove `owner`'s policy (R3 teardown). `None` (core) never matches.
pub fn purge_owner_public_reply(owner: Option<&'static str>) -> bool {
    public_reply_slot().purge(owner)
}

/// The policy in force, if any.
pub fn public_reply() -> Option<Arc<dyn PublicTclReply>> {
    public_reply_slot().with(Arc::clone)
}

// ── the evaluator ─────────────────────────────────────────────────────

/// Evaluate one line of Tcl and render the answer for a chat surface.
///
/// `full` picks the interpreter: the safe one (`sandbox::execute` — no
/// exec/file/socket, bracket pre-check, recursion cap, scratch workspace)
/// or a real `tclsh`. Both normalize to (stdout, stderr, exit code, timed
/// out) so the rendering below is written once.
pub async fn eval_line(
    code: &str,
    full: bool,
    caller: &str,
    sender: Option<&crate::acl::SenderContext>,
) -> String {
    eval_line_hashed(code, full, caller, sender).await.text
}

/// `eval_line`, plus the content hash of the stored proc the line named
/// (a bridge's `invoked_hash`). Only a public surface's safety layer
/// reads the second element; the local sandbox path always yields `None`
/// (there is no persistent state behind it to classify).
pub async fn eval_line_hashed(
    code: &str,
    full: bool,
    caller: &str,
    sender: Option<&crate::acl::SenderContext>,
) -> TclAnswer {
    if code.trim().is_empty() {
        return ("usage: tcl <code>".to_string(), None).into();
    }
    // A persistent interpreter, when a module installed one and it is
    // configured: the same command, a different back end. `full` is the
    // same decision either way — it picks the local `tclsh` here and
    // asks for the bridge's admin path there.
    if let Some(b) = backend() {
        if b.configured() {
            return b.eval(code, full, caller, sender).await;
        }
    }
    // The overflow publisher on the local path reads the config.
    let cfg = crate::config::load().ok();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!("veles_tcl_{}_{}.tcl", std::process::id(), stamp));
    if std::fs::write(&tmp, code).is_err() {
        return ("error: temp write".to_string(), None).into();
    }
    let result: Result<(String, String, i32, bool), String> = if full {
        let mut cmd = tokio::process::Command::new(crate::executecode::tclsh_path());
        cmd.arg(&tmp)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        match tokio::time::timeout(std::time::Duration::from_secs(CAP_SECS), cmd.output()).await {
            Ok(Ok(o)) => Ok((
                String::from_utf8_lossy(&o.stdout).to_string(),
                String::from_utf8_lossy(&o.stderr).to_string(),
                o.status.code().unwrap_or(-1),
                false,
            )),
            Ok(Err(e)) => Err(format!("error: {e}")),
            Err(_) => Ok((String::new(), String::new(), -1, true)),
        }
    } else {
        match crate::sandbox::execute(&tmp, CAP_SECS).await {
            Ok(o) => Ok((o.stdout, o.stderr, o.exit_code, o.timed_out)),
            Err(e) => Err(format!("error: sandbox: {e}")),
        }
    };
    let _ = std::fs::remove_file(&tmp);
    // The local path has no persistent state, so there is no proc hash to
    // classify by: `None`, always. Only a bridge reports one.
    let rendered = match result {
        Err(e) => e,
        Ok((_, _, _, true)) => format!("timeout after {CAP_SECS}s"),
        Ok((stdout, stderr, code, _)) => {
            let mut s = trim_block(&stdout);
            if code != 0 {
                let err = stderr.trim().to_string();
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&format!(
                    "[exit {code}]{}",
                    if err.is_empty() {
                        String::new()
                    } else {
                        format!(" {err}")
                    }
                ));
            }
            s = cut_to(cfg.as_ref(), &s, OUT_CHARS);
            if s.is_empty() {
                "ok (no output)".to_string()
            } else {
                s
            }
        }
    };
    (rendered, None).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dormant mode is the case that matters: `resolve_ctx` hands every
    /// stranger `Role::Owner` when there is no `[acl]` section, so a naive
    /// `has(Cap::All)` would give the full `tclsh` to whoever spoke first.
    /// That is the eggdrop mistake, and this is the test that says so.
    #[test]
    fn the_full_interpreter_is_the_owners_and_nobodys_while_the_acl_is_dormant() {
        let (_home, _env) = crate::testenv::scratch_home();
        let owner_shaped = crate::acl::Principal {
            id: None,
            subject: "anon:#chan".into(),
            role: crate::acl::Role::Owner,
            caps: crate::acl::Role::Owner.default_caps(),
            rule_context_limit: None,
            mask: None,
            via: None,
            ticket_role: None,
        };
        // No config file in the scratch home ⇒ the ACL is dormant.
        assert!(
            !full_interp_allowed(Some(&owner_shaped)),
            "a dormant ACL resolves strangers as owner — nobody gets exec"
        );
        // A local surface carries no principal at all: that IS the human
        // at the keyboard, and fencing them out of their own machine is
        // not a posture.
        assert!(full_interp_allowed(None));
    }

    /// The privileged path is OFFERED only to a caller veles calls owner
    /// (USER, 2026-09-11, over "always call in as admin"): the decision
    /// is `full_interp_allowed`, the SAME function the local path uses to
    /// choose between the safe interp and a real `tclsh`, so a bridge
    /// behind the backend slot cannot come to a different conclusion
    /// about who a caller is.
    #[tokio::test]
    async fn the_privileged_path_is_offered_only_to_a_caller_veles_calls_owner() {
        let (home, _env) = crate::testenv::scratch_home();
        // A CONFIGURED acl, so dormant mode does not call everyone owner.
        std::fs::write(
            home.path().join("config.toml"),
            "[acl]\nowner = [\"irc:theboss\"]\ndefault_role = \"user\"\n",
        )
        .unwrap();
        crate::acl::invalidate_live_config();
        let acl = crate::acl::live_config();
        assert!(acl.is_configured());

        let mk = |role: crate::acl::Role, id: &str| crate::acl::Principal {
            id: Some(id.into()),
            subject: format!("user:{id}"),
            role,
            caps: role.default_caps(),
            rule_context_limit: None,
            mask: None,
            via: None,
            ticket_role: None,
        };
        assert!(full_interp_allowed(Some(&mk(
            crate::acl::Role::Owner,
            "irc:theboss"
        ))));
        for role in [
            crate::acl::Role::Trusted,
            crate::acl::Role::User,
            crate::acl::Role::Guest,
        ] {
            assert!(
                !full_interp_allowed(Some(&mk(role, "irc:someone"))),
                "{} must not reach the privileged path",
                role.as_str()
            );
        }
        assert!(admin_requested_and_allowed(true, None));
        assert!(
            !admin_requested_and_allowed(false, None),
            "the spelling decides whether it is used"
        );
    }

    #[tokio::test]
    async fn the_safe_interp_computes_and_refuses_the_dangerous_families() {
        let _g = slot_lock().await;
        let (_home, _env) = crate::testenv::scratch_home();
        let out = eval_line("puts [expr {40 + 2}]", false, "test", None).await;
        assert!(out.contains("42"), "pure compute arrives: {out}");
        let danger = eval_line("puts [exec /bin/echo pwned]", false, "test", None).await;
        assert!(
            !danger.contains("pwned"),
            "the safe interp has no exec: {danger}"
        );
        assert_eq!(
            eval_line("   ", false, "test", None).await,
            "usage: tcl <code>"
        );
    }

    #[test]
    fn display_caller_is_the_handle_not_the_subject() {
        // atproto: `handle` carries it — never `mask` (ACL-matched).
        let sender = crate::acl::SenderContext {
            mask: None,
            network: Some("atproto".to_string()),
            channel: None,
            platform: Some("atproto".to_string()),
            chat_id: None,
            direct: false,
            handle: Some("7778777.online".to_string()),
            post: None,
        };
        assert_eq!(
            super::display_caller(Some(&sender), "user:wrath"),
            "7778777.online",
            "the handle, not the subject"
        );
        // IRC: nick from nick!ident@host.
        let sender = crate::acl::SenderContext {
            mask: Some("wrath!~w@example.org".to_string()),
            network: Some("synirc".to_string()),
            channel: Some("#coven".to_string()),
            platform: Some("irc".to_string()),
            chat_id: None,
            direct: false,
            handle: None,
            post: None,
        };
        assert_eq!(super::display_caller(Some(&sender), "user:wrath"), "wrath");
        // No sender (a REPL, a cron): the subject is all there is.
        assert_eq!(super::display_caller(None, "user:wrath"), "user:wrath");
        // A bare-mask-less sender: same.
        let sender = crate::acl::SenderContext::default();
        assert_eq!(
            super::display_caller(Some(&sender), "user:wrath"),
            "user:wrath"
        );
    }

    /// The backend tests share one process-global slot (an async lock:
    /// it is held across the evaluations' awaits).
    async fn slot_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static L: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        L.lock().await
    }

    struct Stub {
        on: bool,
    }
    impl TclBackend for Stub {
        fn configured(&self) -> bool {
            self.on
        }
        fn eval<'a>(
            &'a self,
            code: &'a str,
            full: bool,
            caller: &'a str,
            _sender: Option<&'a crate::acl::SenderContext>,
        ) -> EvalFuture<'a> {
            Box::pin(async move {
                TclAnswer::from((
                    format!("bridge:{caller}:{full}:{code}"),
                    Some("deadbeef".into()),
                ))
            })
        }
    }

    /// A configured backend takes every line, with the caller and the
    /// privilege decision passed through and its proc hash reported; an
    /// unconfigured one (a module loaded with its bridge switched off)
    /// leaves the local sandbox in charge; and clearing it does too.
    #[tokio::test]
    async fn a_configured_backend_takes_the_line_and_an_unconfigured_one_does_not() {
        let _g = slot_lock().await;
        let (_home, _env) = crate::testenv::scratch_home();
        set_backend(Arc::new(Stub { on: true }));
        assert!(backend_configured());
        let TclAnswer {
            text: out,
            invoked_hash: hash,
            ..
        } = eval_line_hashed("expr 1", true, "user:x", None).await;
        assert_eq!(out, "bridge:user:x:true:expr 1");
        assert_eq!(hash.as_deref(), Some("deadbeef"));

        set_backend(Arc::new(Stub { on: false }));
        assert!(!backend_configured());
        let TclAnswer {
            text: out,
            invoked_hash: hash,
            ..
        } = eval_line_hashed("puts [expr {1 + 1}]", false, "user:x", None).await;
        assert!(out.contains('2'), "the local sandbox answered: {out}");
        assert_eq!(hash, None, "the local path classifies nothing");

        clear_backend();
        assert!(!backend_configured());
        assert!(
            eval_line("puts hi", false, "t", None).await.contains("hi"),
            "no backend: the sandbox"
        );
    }

    /// The public-surface policy: absent, core says so and names nothing
    /// of the output; present, it is the policy's word.
    struct Allow;
    impl PublicTclReply for Allow {
        fn shape<'a>(&'a self, req: PublicReplyRequest<'a>) -> ShapeFuture<'a> {
            Box::pin(async move {
                let mut meta = std::collections::BTreeMap::new();
                meta.insert("ledger.entry".to_string(), "e1".to_string());
                PublicReplyShaped {
                    text: format!("shaped:{}", req.output),
                    meta,
                }
            })
        }
    }

    #[tokio::test]
    async fn the_public_policy_slot_is_empty_until_a_module_fills_it() {
        let _g = slot_lock().await;
        assert!(public_reply().is_none());
        set_public_reply(Arc::new(Allow));
        let cfg = crate::config::VelesConfig::default();
        let msg = crate::bus::InboundMessage {
            platform: "atproto".into(),
            route: None,
            chat_id: "at://x".into(),
            msg_id: None,
            dedup_id: None,
            sender: "a".into(),
            sender_id: None,
            sender_ctx: Default::default(),
            text: "/tcl expr 1".into(),
            context: None,
        };
        let shaped = public_reply()
            .expect("installed")
            .shape(PublicReplyRequest {
                cfg: &cfg,
                msg: &msg,
                code: "expr 1",
                output: "1",
                invoked_hash: None,
                spec: None,
            })
            .await;
        assert_eq!(shaped.text, "shaped:1");
        assert_eq!(
            shaped.meta.get("ledger.entry").map(String::as_str),
            Some("e1")
        );
        clear_public_reply();
        assert!(public_reply().is_none());
    }
}

#[cfg(test)]
mod trim_block_tests {
    use super::trim_block;

    /// The `tcl STgoon` shape: every line indented, and `str::trim`
    /// unindents exactly the first one.
    #[test]
    fn a_drawings_first_row_keeps_its_indentation() {
        let art = " __________\n (--[ .]-[ .]\n (_______O__)";
        assert_eq!(trim_block(art), art, "no line may be re-indented");
        // The bug, stated so the fix is not mistaken for a no-op.
        assert_ne!(
            art.trim(),
            art,
            "str::trim IS destructive here — that is why trim_block exists"
        );
    }

    #[test]
    fn blank_lines_around_the_block_still_go() {
        assert_eq!(
            trim_block("\n\n  keep me\n   and me\n\n  \n"),
            "  keep me\n   and me"
        );
    }

    #[test]
    fn trailing_whitespace_goes_and_leading_stays() {
        assert_eq!(trim_block("   42   "), "   42");
    }

    #[test]
    fn a_carriage_return_does_not_survive_into_a_line() {
        assert_eq!(trim_block("  a\r\n  b\r\n"), "  a\n  b");
    }

    #[test]
    fn nothing_at_all_stays_nothing() {
        assert_eq!(trim_block("\n  \n"), "");
        assert_eq!(trim_block(""), "");
    }
}
