//! Named sandboxes, and who reaches which one.
//!
//! `user_envs.rs` owns the MECHANICS of a persistent container —
//! create, exec, copy in and out, background shells, idle-stop, the
//! escape guards. This module owns the one question it deliberately
//! does not answer: *which* container a request belongs in.
//!
//! # Why this exists
//!
//! The first version of user envs keyed the container on the
//! principal's id, one box per person. USER, 2026-09-09: *"One shared
//! sandbox is implicitly created for everyone. Additional sandboxes can
//! be created and added on request. Admin should have access to
//! arbitrary ones. Other users may have one or more assigned. The
//! sandboxes may also be assigned per-channel and per-network and
//! possibly based on other criteria."* — and, on why it must not be
//! IRC-shaped: *"other connectors might also want to use the shared
//! sandbox infrastructure … For example, if I ever feel like this is
//! safe enough to put on bluesky."*
//!
//! So a sandbox has a NAME, the name is what the container key hashes,
//! and everyone routed to one name shares one box and one workspace.
//! That is the point: two people in a channel working on the same
//! thing with the same files, which is what "work interactively" needs.
//!
//! # The implicit ones
//!
//! Nobody has to declare anything. The default for a conversation is
//! decided by the shape of the conversation:
//!
//! - `chan:<platform>/<network>/<chat>` — a group. Everyone talking to
//!   the bot in `#foo` shares `#foo`'s box.
//! - `dm:<subject>` — a one-to-one conversation gets a private box,
//!   because a DM is not a shared workspace.
//! - `shared` — a global box, never implicit, always assignable by
//!   name. It is the "one shared sandbox" for deployments that want
//!   exactly one.
//!
//! Explicit assignments (`[[sandbox.assign]]`, or a runtime bind from
//! any frontend) override the implicit choice, resolved by the SAME
//! specificity rules the ACL uses so the two read alike.
//!
//! # Authority
//!
//! The routing is decided from the SESSION — never from a tool
//! argument for a below-admin caller. `may_use` answers whether a
//! caller may name a box at all: their resolved default plus anything
//! a matching bind names, and for admin and owner, any box that
//! exists. A name outside that set is a framework denial, not a silent
//! fallback to somewhere they can reach.

use crate::acl::{Principal, Role, SenderContext};
use crate::config::{SandboxBind, SandboxDef};
use crate::lockutil::LockOk;
use crate::store::Store;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The global box: never implicit, always nameable.
pub const SHARED: &str = "shared";

const DEF_PREFIX: &str = "sandbox_def:";
const BIND_PREFIX: &str = "sandbox_bind:";

/// A sandbox name is a container name component and a directory name.
/// Keep it boring: letters, digits and the three separators the
/// implicit names use.
pub fn valid_name(name: &str) -> bool {
    // `.` is allowed because hostnames and channel names carry them
    // (`chan:irc/net/#foo.bar`), but `..` never is: the name is
    // hashed before it reaches any path, and a name that LOOKS like
    // traversal should still be refused where it is typed rather than
    // trusted to be harmless three layers down.
    !name.is_empty()
        && name.len() <= 96
        && !name.contains("..")
        && !name.starts_with('/')
        && !name.contains("//")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '/' | '.' | '#'))
}

/// Normalize for comparison: names are case-insensitive, like the
/// channels and networks they are built from.
pub fn normalize(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

/// The implicit sandbox for a conversation.
///
/// Built from the connector's own words — platform, network, chat —
/// so a future connector needs no code here, only a `SenderContext`
/// filled in like IRC's and Telegram's.
pub fn implicit_name(principal: &Principal, ctx: &SenderContext) -> String {
    let platform = ctx.platform.as_deref().unwrap_or("local");
    let chat = ctx.chat_id.as_deref().or(ctx.channel.as_deref());
    match (ctx.direct, chat) {
        // A one-to-one conversation is not a shared workspace.
        (true, _) | (_, None) => normalize(&format!("dm:{}", principal.subject)),
        (false, Some(chat)) => match ctx.network.as_deref() {
            Some(net) => normalize(&format!("chan:{platform}/{net}/{chat}")),
            None => normalize(&format!("chan:{platform}/{chat}")),
        },
    }
}

// ── declarations ─────────────────────────────────────────────────────

/// Every declared sandbox: config first, then runtime definitions.
/// A runtime definition with the same name REPLACES the config one —
/// the live edit is the more recent statement.
pub fn defs(store: Option<&Arc<Mutex<Store>>>) -> Vec<SandboxDef> {
    let mut out: Vec<SandboxDef> = crate::config::live().sandbox.envs.clone();
    for d in runtime_defs(store) {
        out.retain(|e| normalize(&e.name) != normalize(&d.name));
        out.push(d);
    }
    out.sort_by_key(|d| normalize(&d.name));
    out
}

fn runtime_defs(store: Option<&Arc<Mutex<Store>>>) -> Vec<SandboxDef> {
    let Some(store) = store else {
        return Vec::new();
    };
    let Ok(g) = store.lock() else {
        return Vec::new();
    };
    g.meta_scan_prefix(DEF_PREFIX)
        .into_iter()
        .filter_map(|k| g.meta_get(&k))
        .filter_map(|v| serde_json::from_str::<SandboxDef>(&v).ok())
        .collect()
}

/// Declare (or redeclare) a sandbox at runtime — the frontends' write
/// path. Live: nothing is cached, so the next routing decision sees it.
pub fn define(
    store: &Arc<Mutex<Store>>,
    mut def: SandboxDef,
    by: &str,
) -> anyhow::Result<SandboxDef> {
    def.name = normalize(&def.name);
    if !valid_name(&def.name) {
        anyhow::bail!(
            "invalid sandbox name {:?} (letters, digits, - _ : / . #)",
            def.name
        );
    }
    def.created_by = Some(by.to_string());
    def.created_at = Some(now());
    let g = store
        .lock()
        .map_err(|_| anyhow::anyhow!("store lock poisoned"))?;
    g.meta_set(
        &format!("{DEF_PREFIX}{}", def.name),
        &serde_json::to_string(&def)?,
    )?;
    Ok(def)
}

/// Forget a runtime declaration. The CONTAINER is not touched — that is
/// `user_envs::purge`, and it is a separate decision because a
/// workspace holds someone's files.
pub fn undefine(store: &Arc<Mutex<Store>>, name: &str) -> bool {
    let name = normalize(name);
    match store.lock() {
        Ok(g) => g
            .meta_delete(&format!("{DEF_PREFIX}{name}"))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Every assignment: config first, then runtime binds.
pub fn binds(store: Option<&Arc<Mutex<Store>>>) -> Vec<SandboxBind> {
    let mut out: Vec<SandboxBind> = crate::config::live().sandbox.assign.clone();
    out.extend(runtime_binds(store).into_iter().map(|(_, b)| b));
    out
}

fn runtime_binds(store: Option<&Arc<Mutex<Store>>>) -> Vec<(String, SandboxBind)> {
    let Some(store) = store else {
        return Vec::new();
    };
    let Ok(g) = store.lock() else {
        return Vec::new();
    };
    g.meta_scan_prefix(BIND_PREFIX)
        .into_iter()
        .filter_map(|k| {
            let v = g.meta_get(&k)?;
            let b = serde_json::from_str::<SandboxBind>(&v).ok()?;
            Some((k.trim_start_matches(BIND_PREFIX).to_string(), b))
        })
        .collect()
}

/// The key a runtime bind is stored under — its selectors, so
/// re-binding the same selectors replaces rather than accumulates.
pub fn bind_key(b: &SandboxBind) -> String {
    let part = |o: &Option<String>| o.as_deref().unwrap_or("*").to_ascii_lowercase();
    format!(
        "{}|{}|{}|{}|{}",
        part(&b.identity),
        part(&b.network),
        part(&b.channel),
        part(&b.platform),
        part(&b.role)
    )
}

/// Assign at runtime. The sandbox need not exist yet: an implicit box
/// is created on first use, and naming one in a bind is how you say
/// "make this the box for these people".
pub fn bind(store: &Arc<Mutex<Store>>, mut b: SandboxBind) -> anyhow::Result<SandboxBind> {
    b.sandbox = normalize(&b.sandbox);
    if !valid_name(&b.sandbox) {
        anyhow::bail!("invalid sandbox name {:?}", b.sandbox);
    }
    if let Some(id) = &b.identity {
        if crate::ircmask::looks_like_mask(id) {
            crate::ircmask::MaskPattern::parse(id)
                .map_err(|e| anyhow::anyhow!("invalid mask pattern {id:?}: {e}"))?;
        }
    }
    if let Some(r) = &b.role {
        if Role::parse(r).is_none() {
            anyhow::bail!("unknown role {r:?}");
        }
    }
    if b.identity.is_none()
        && b.network.is_none()
        && b.channel.is_none()
        && b.platform.is_none()
        && b.role.is_none()
    {
        anyhow::bail!("a bind with no selectors would capture everyone — name at least one");
    }
    let g = store
        .lock()
        .map_err(|_| anyhow::anyhow!("store lock poisoned"))?;
    g.meta_set(
        &format!("{BIND_PREFIX}{}", bind_key(&b)),
        &serde_json::to_string(&b)?,
    )?;
    Ok(b)
}

/// Drop a runtime bind by the same selectors it was made with.
pub fn unbind(store: &Arc<Mutex<Store>>, b: &SandboxBind) -> bool {
    match store.lock() {
        Ok(g) => g
            .meta_delete(&format!("{BIND_PREFIX}{}", bind_key(b)))
            .unwrap_or(false),
        Err(_) => false,
    }
}

// ── resolution ───────────────────────────────────────────────────────

/// How specifically a bind names this caller — the same shape as the
/// ACL's precedence key, and for the same reason: the narrowest rule
/// should win regardless of where it was written.
///
/// A thin wrapper now: the matching and the ranking live in
/// `crate::selector`, because `[[model.binds]]` asks the identical
/// question and sixty lines of precedence rules maintained twice is the
/// shape this tree keeps finding at the bottom of its own bugs.
///
/// Moving it also fixed a silent no-op here: the old comparison was
/// `Role::parse(r) != Some(principal.role)`, so `role = "<=trusted"`
/// parsed as nothing and matched NOBODY — a rule sitting in a config
/// file doing exactly that, with no warning.
fn bind_specificity(
    b: &SandboxBind,
    principal: &Principal,
    ctx: &SenderContext,
) -> Option<crate::selector::Specificity> {
    crate::selector::specificity(&bind_selector(b), principal, ctx)
}

fn bind_selector(b: &SandboxBind) -> crate::selector::Selector<'_> {
    crate::selector::Selector {
        identity: b.identity.as_deref(),
        network: b.network.as_deref(),
        channel: b.channel.as_deref(),
        platform: b.platform.as_deref(),
        role: b.role.as_deref(),
    }
}

/// Every sandbox this caller may name: their resolved default, plus
/// whatever any matching bind names. Admin and owner may name any box
/// that exists — see `may_use`.
pub fn reachable(
    principal: &Principal,
    ctx: &SenderContext,
    store: Option<&Arc<Mutex<Store>>>,
) -> Vec<String> {
    let mut out = vec![resolve_for(principal, ctx, store)];
    for b in binds(store) {
        if bind_specificity(&b, principal, ctx).is_some() {
            let n = normalize(&b.sandbox);
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

/// The sandbox this caller lands in when nothing says otherwise: the
/// most specific matching bind, else the implicit box for this
/// conversation.
pub fn resolve_for(
    principal: &Principal,
    ctx: &SenderContext,
    store: Option<&Arc<Mutex<Store>>>,
) -> String {
    // `binds()` lists CONFIG binds before runtime ones and
    // `most_specific` keeps the first on a tie, so an equally specific
    // runtime bind does not silently displace a declared one.
    match crate::selector::most_specific(binds(store), principal, ctx, bind_selector) {
        Some(b) => normalize(&b.sandbox),
        None => implicit_name(principal, ctx),
    }
}

/// May this caller run in the box called `name`, judged from what the
/// gateway already published for their session?
///
/// The zone decision happens deep inside a tool call, where the
/// conversation that produced the reachable set is long gone — so the
/// set is looked up by subject rather than recomputed. Admin and owner
/// reach everything.
pub fn may_use_session(principal: &Principal, name: &str) -> bool {
    if principal.role >= Role::Admin {
        return true;
    }
    let want = normalize(name);
    valid_name(&want) && session_reachable(&principal.subject).contains(&want)
}

/// May this caller run in the box called `name`?
///
/// Admin and owner may name any box that EXISTS (declared, or already
/// created — an admin looking into someone's workspace is the point of
/// `.agent sandboxes`). Below admin: their own reachable set only.
pub fn may_use(
    principal: &Principal,
    ctx: &SenderContext,
    store: Option<&Arc<Mutex<Store>>>,
    name: &str,
) -> bool {
    let name = normalize(name);
    if !valid_name(&name) {
        return false;
    }
    if principal.role >= Role::Admin {
        return true;
    }
    reachable(principal, ctx, store).contains(&name)
}

// ── the session's box ────────────────────────────────────────────────
//
// The routing decision needs the CONVERSATION (platform, network,
// channel) and the tool layer only has a session id. So the gateway —
// which has both — publishes the answer here when it builds or
// re-resolves a session, exactly like `acl::set_session_principal`.
// `/sandbox use <name>` writes here too, which is what makes a session
// override a session override.

static SESSION_SANDBOX: std::sync::OnceLock<Mutex<HashMap<String, String>>> =
    std::sync::OnceLock::new();

fn session_sandboxes() -> &'static Mutex<HashMap<String, String>> {
    SESSION_SANDBOX.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn set_session_sandbox(session_id: &str, name: &str) {
    session_sandboxes()
        .lock_ok()
        .insert(session_id.to_string(), normalize(name));
}

pub fn session_sandbox(session_id: &str) -> Option<String> {
    session_sandboxes().lock_ok().get(session_id).cloned()
}

/// The set a SUBJECT may name, published alongside the session's box.
///
/// Keyed on the subject rather than the session because the tool layer
/// asks this question from inside a call, where it holds the principal
/// and not the conversation — and because one human's reachable set is
/// the same in all their sessions.
static SUBJECT_REACHABLE: std::sync::OnceLock<Mutex<HashMap<String, Vec<String>>>> =
    std::sync::OnceLock::new();

fn subject_reachable() -> &'static Mutex<HashMap<String, Vec<String>>> {
    SUBJECT_REACHABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn set_session_reachable(subject: &str, names: Vec<String>) {
    subject_reachable()
        .lock_ok()
        .insert(subject.to_string(), names);
}

pub fn session_reachable(subject: &str) -> Vec<String> {
    subject_reachable()
        .lock_ok()
        .get(subject)
        .cloned()
        .unwrap_or_default()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(platform: &str, network: &str, chat: &str, direct: bool) -> SenderContext {
        SenderContext {
            mask: Some("nick!ident@host".into()),
            network: (!network.is_empty()).then(|| network.to_string()),
            channel: (!direct && !chat.is_empty()).then(|| chat.to_string()),
            platform: Some(platform.to_string()),
            chat_id: (!chat.is_empty()).then(|| chat.to_string()),
            direct,
            handle: None,
            post: None,
        }
    }

    fn store() -> Arc<Mutex<Store>> {
        let dir = crate::testenv::kept_dir();
        Arc::new(Mutex::new(
            Store::open(&dir.join("state.db")).expect("store"),
        ))
    }

    /// The default is decided by the SHAPE of the conversation: a group
    /// shares one box, a one-to-one gets its own. USER, 2026-09-09:
    /// "One shared sandbox is implicitly created for everyone", scoped
    /// per channel.
    #[test]
    fn the_implicit_box_is_per_channel_and_private_in_a_dm() {
        let p = Principal::known("irc:wrath", Role::User);
        assert_eq!(
            implicit_name(&p, &ctx("irc", "libera", "#Veles", false)),
            "chan:irc/libera/#veles"
        );
        // Everyone in that channel lands on the SAME name — that is
        // what makes it shared.
        let q = Principal::known("irc:someone-else", Role::Guest);
        assert_eq!(
            implicit_name(&q, &ctx("irc", "libera", "#veles", false)),
            implicit_name(&p, &ctx("irc", "libera", "#veles", false))
        );
        // Another channel is another box…
        assert_ne!(
            implicit_name(&p, &ctx("irc", "libera", "#other", false)),
            implicit_name(&p, &ctx("irc", "libera", "#veles", false))
        );
        // …and so is the same channel name on another network, or
        // another connector entirely (the bluesky case).
        assert_ne!(
            implicit_name(&p, &ctx("irc", "oftc", "#veles", false)),
            implicit_name(&p, &ctx("irc", "libera", "#veles", false))
        );
        assert_eq!(
            implicit_name(&p, &ctx("telegram", "", "-100123", false)),
            "chan:telegram/-100123"
        );
        // A DM is not a shared workspace.
        assert_eq!(
            implicit_name(&p, &ctx("irc", "libera", "wrath", true)),
            "dm:irc:wrath"
        );
    }

    /// A bind overrides the implicit choice, and the MOST SPECIFIC bind
    /// wins regardless of declaration order — the same rule the ACL
    /// follows, so the two read alike.
    #[test]
    fn the_most_specific_bind_wins() {
        let s = store();
        let p = Principal::known("irc:wrath", Role::User);
        let c = ctx("irc", "libera", "#veles", false);
        // Nothing bound: the implicit box.
        assert_eq!(resolve_for(&p, &c, Some(&s)), "chan:irc/libera/#veles");
        // A broad platform bind…
        bind(
            &s,
            SandboxBind {
                platform: Some("irc".into()),
                sandbox: "everyone".into(),
                ..Default::default()
            },
        )
        .expect("platform bind");
        assert_eq!(resolve_for(&p, &c, Some(&s)), "everyone");
        // …beaten by a channel bind…
        bind(
            &s,
            SandboxBind {
                channel: Some("#veles".into()),
                sandbox: "the-channel-box".into(),
                ..Default::default()
            },
        )
        .expect("channel bind");
        assert_eq!(resolve_for(&p, &c, Some(&s)), "the-channel-box");
        // …beaten by an exact identity, however it was ordered.
        bind(
            &s,
            SandboxBind {
                identity: Some("irc:wrath".into()),
                sandbox: "wraths-own".into(),
                ..Default::default()
            },
        )
        .expect("identity bind");
        assert_eq!(resolve_for(&p, &c, Some(&s)), "wraths-own");
        // A mask bind reaches a caller with no account at all, and a
        // pinned host outranks a pinned nick.
        let anon = Principal {
            id: None,
            subject: "irc:libera/nick!ident@host".into(),
            role: Role::Guest,
            caps: Role::Guest.default_caps(),
            rule_context_limit: None,
            mask: Some("nick!ident@host".into()),
            via: None,
            ticket_role: None,
        };
        bind(
            &s,
            SandboxBind {
                identity: Some("nick!*@*".into()),
                sandbox: "by-nick".into(),
                ..Default::default()
            },
        )
        .expect("nick bind");
        assert_eq!(resolve_for(&anon, &c, Some(&s)), "by-nick");
        bind(
            &s,
            SandboxBind {
                identity: Some("*!*@host".into()),
                sandbox: "by-host".into(),
                ..Default::default()
            },
        )
        .expect("host bind");
        assert_eq!(
            resolve_for(&anon, &c, Some(&s)),
            "by-host",
            "host beats nick, as in the ACL"
        );
    }

    /// A bind can select on ROLE — the one selector an ACL entry cannot
    /// have (an entry grants a role, so it cannot also match on one).
    /// "Guests go in the quarantine box" is one line.
    #[test]
    fn a_bind_can_select_on_role() {
        let s = store();
        let c = ctx("irc", "libera", "#veles", false);
        bind(
            &s,
            SandboxBind {
                role: Some("guest".into()),
                sandbox: "quarantine".into(),
                ..Default::default()
            },
        )
        .expect("role bind");
        assert_eq!(
            resolve_for(&Principal::known("irc:g", Role::Guest), &c, Some(&s)),
            "quarantine"
        );
        assert_eq!(
            resolve_for(&Principal::known("irc:u", Role::User), &c, Some(&s)),
            "chan:irc/libera/#veles",
            "a user is not a guest"
        );
    }

    /// The reachable set is what a below-admin caller may NAME. Admin
    /// and owner reach anything.
    #[test]
    fn reachability_is_the_default_plus_what_binds_name() {
        let s = store();
        let c = ctx("irc", "libera", "#veles", false);
        let user = Principal::known("irc:wrath", Role::User);
        assert_eq!(
            reachable(&user, &c, Some(&s)),
            vec!["chan:irc/libera/#veles"]
        );
        assert!(may_use(&user, &c, Some(&s), "chan:irc/libera/#veles"));
        assert!(
            !may_use(&user, &c, Some(&s), "someone-elses"),
            "a name nobody gave them is not theirs to use"
        );
        bind(
            &s,
            SandboxBind {
                identity: Some("irc:wrath".into()),
                sandbox: SHARED.into(),
                ..Default::default()
            },
        )
        .expect("bind");
        let set = reachable(&user, &c, Some(&s));
        assert!(set.contains(&SHARED.to_string()));
        assert!(may_use(&user, &c, Some(&s), SHARED));
        // Admin reaches arbitrary boxes by name (USER: "Admin should
        // have access to arbitrary ones").
        let admin = Principal::known("irc:boss", Role::Admin);
        assert!(may_use(&admin, &c, Some(&s), "any-box-at-all"));
        // …but not a name that could not be a container.
        assert!(!may_use(&admin, &c, Some(&s), "bad name!"));
    }

    #[test]
    fn a_bind_with_no_selectors_is_refused() {
        let s = store();
        let err = bind(
            &s,
            SandboxBind {
                sandbox: "everything".into(),
                ..Default::default()
            },
        )
        .expect_err("a selectorless bind captures everyone");
        assert!(err.to_string().contains("name at least one"), "{err}");
    }

    #[test]
    fn definitions_round_trip_and_replace() {
        let s = store();
        define(
            &s,
            SandboxDef {
                name: "Build-Box".into(),
                note: Some("for the CI experiments".into()),
                ..Default::default()
            },
            "irc:owner",
        )
        .expect("define");
        let all = defs(Some(&s));
        let d = all
            .iter()
            .find(|d| d.name == "build-box")
            .expect("normalized to lowercase");
        assert_eq!(d.created_by.as_deref(), Some("irc:owner"));
        assert!(d.created_at.is_some());
        // Redefining replaces rather than duplicating.
        define(
            &s,
            SandboxDef {
                name: "build-box".into(),
                note: Some("second".into()),
                ..Default::default()
            },
            "irc:owner",
        )
        .expect("redefine");
        let all = defs(Some(&s));
        assert_eq!(all.iter().filter(|d| d.name == "build-box").count(), 1);
        assert!(undefine(&s, "BUILD-BOX"));
        assert!(!defs(Some(&s)).iter().any(|d| d.name == "build-box"));
        // A name that could not be a container is refused.
        assert!(define(
            &s,
            SandboxDef {
                name: "../escape".into(),
                ..Default::default()
            },
            "irc:owner"
        )
        .is_err());
    }

    #[test]
    fn names_are_bounded_and_boring() {
        assert!(valid_name("chan:irc/libera/#veles"));
        assert!(valid_name(SHARED));
        assert!(!valid_name(""));
        assert!(!valid_name("has space"));
        assert!(!valid_name("../up"));
        assert!(!valid_name(&"x".repeat(200)));
    }
}
