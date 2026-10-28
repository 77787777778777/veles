//! Pairing codes (S9, Letta DM-policies parity) — let a friend use a
//! gateway bot WITHOUT handing them the API keys or editing config.
//!
//! USER STORY (from the Letta competitive audit, retired 2026-09-06 —
//! in git history): "Pairing codes are a genuinely
//! good idea for letting a friend use the bot without handing them the
//! keys."
//!
//! Flow:
//!   1. Owner runs `veles pair new [--role trusted] [--ttl-hours N]`
//!      → a code like `K7Q2-M9X4` is printed + persisted in the meta
//!      table (default 24h expiry).
//!   2. The friend messages the bot `/pair K7Q2-M9X4`. The adapter
//!      passes their VERIFIED platform identity (`telegram:<uid>`).
//!   3. On match, the identity receives a RUNTIME ROLE GRANT persisted
//!      in meta (survives restarts); the code is consumed. The friend
//!      is told what they now have.
//!   4. Owner revokes: `/pair revoke <identity>` from an owner chat or
//!      `veles pair revoke <identity>`; `/pair list` / `veles pair
//!      list` shows live grants.
//!
//! Grants ride the SAME resolution chain as config roles: acl.rs's
//! Principal::resolve consults [`runtime_role`] after config roles and
//! before extra_caps. No config file is ever rewritten by the bot.

use crate::acl::Role;
use crate::store::Store;
use std::sync::Arc;

const PENDING_PREFIX: &str = "pair_pending:";
const GRANT_PREFIX: &str = "pair_grant:";
const DEFAULT_TTL_SECS: u64 = 24 * 3600;

/// A live runtime role grant.
///
/// `identity` is either an exact `platform:principal` string or an IRC
/// mask pattern (`nick!user@host`, glob/`re:` per segment). `network`
/// and `channel` scope the grant to one IRC network / channel when set
/// (desktop ACL UI and IRC-tab management fill these; a redeemed
/// pairing code leaves them None = everywhere).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Grant {
    pub identity: String,
    pub role: String,
    /// Unix seconds; 0 = never expires (until revoked).
    pub expires_at: u64,
    pub granted_at: u64,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn store_lock(
    store: &Arc<std::sync::Mutex<Store>>,
) -> anyhow::Result<std::sync::MutexGuard<'_, Store>> {
    store
        .lock()
        .map_err(|_| anyhow::anyhow!(crate::i18n::lookup("err_store_locked", &[])))
}

/// Generate a human-friendly code: XXXX-XXXX from an unambiguous
/// alphabet (no 0/O/1/I/L). Entropy from uuid v4 (already a dep — no
/// rand direct dependency needed).
pub fn generate_code() -> String {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
    let bytes = uuid::Uuid::new_v4().as_bytes().to_vec();
    let mut code = String::new();
    for half in 0..2 {
        if half > 0 {
            code.push('-');
        }
        for i in 0..4 {
            // Fold two bytes per char for a touch more mixing.
            let b = bytes[(half * 4 + i) as usize] ^ bytes[(half * 4 + i + 8) as usize];
            code.push(ALPHABET[(b as usize) % ALPHABET.len()] as char);
        }
    }
    code
}

/// Validate a user-typed code shape early (XXXX-XXXX, good alphabet).
pub fn valid_shape(code: &str) -> bool {
    let c = code.trim().to_uppercase();
    let parts: Vec<&str> = c.split('-').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() == 4
                && p.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() && !"01ILO".contains(ch))
        })
}

/// Owner side: create a pairing code. Returns (code, expires_at unix).
pub fn create(
    store: &Arc<std::sync::Mutex<Store>>,
    role: &str,
    ttl_secs: Option<u64>,
) -> anyhow::Result<(String, u64)> {
    if Role::parse(role).is_none() || role == "owner" {
        anyhow::bail!("invalid role '{role}' (trusted|user|guest; owner is not grantable)");
    }
    let expires = now() + ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
    let g = store_lock(store)?;
    for _ in 0..8 {
        let code = generate_code();
        let key = format!("{PENDING_PREFIX}{code}");
        if g.meta_get(&key).is_some() {
            continue; // astronomically unlikely collision
        }
        let pending = serde_json::json!({
            "role": role,
            "expires_at": expires,
            "created_at": now(),
        })
        .to_string();
        g.meta_set(&key, &pending)?;
        return Ok((code, expires));
    }
    anyhow::bail!("could not allocate a unique code")
}

/// Friend side: redeem a code for a VERIFIED platform identity.
/// Returns the granted role. The code is consumed on success.
pub fn redeem(
    store: &Arc<std::sync::Mutex<Store>>,
    code: &str,
    identity: &str,
) -> anyhow::Result<String> {
    let code = code.trim().to_uppercase();
    if !valid_shape(&code) {
        anyhow::bail!("malformed code");
    }
    let key = format!("{PENDING_PREFIX}{code}");
    let role = {
        let g = store_lock(store)?;
        let Some(raw) = g.meta_get(&key) else {
            anyhow::bail!("unknown or already-used code");
        };
        let v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("corrupt pending entry: {e}"))?;
        let expires = v["expires_at"].as_u64().unwrap_or(0);
        if expires != 0 && now() > expires {
            g.meta_delete(&key)?;
            anyhow::bail!("code expired");
        }
        v["role"].as_str().unwrap_or("user").to_string()
    };
    // Consume the code with its own guard scope — persist_grant below
    // takes the store lock, and a writer holding two locks is a
    // deadlock, not a transaction.
    store_lock(store)?.meta_delete(&key)?;
    let grant = persist_grant(store, identity, &role, None, None, None)?;
    Ok(grant.role)
}

/// ACL hook: live runtime role for an identity, if any.
pub fn runtime_role(store: &Arc<std::sync::Mutex<Store>>, identity: &str) -> Option<Role> {
    let g = store_lock(store).ok()?;
    let raw = g.meta_get(&format!("{GRANT_PREFIX}{}", identity.to_lowercase()))?;
    let grant: Grant = serde_json::from_str(&raw).ok()?;
    if grant.expires_at != 0 && now() > grant.expires_at {
        return None;
    }
    Role::parse(&grant.role)
}

/// Persist a grant record. The identity is stored as given; the KEY is
/// the lowercase form, matching the runtime_role lookup. Redeeming a
/// code and direct grants share this — two writers, one format.
fn persist_grant(
    store: &Arc<std::sync::Mutex<Store>>,
    identity: &str,
    role: &str,
    ttl_secs: Option<u64>,
    network: Option<String>,
    channel: Option<String>,
) -> anyhow::Result<Grant> {
    let grant = Grant {
        identity: identity.to_string(),
        role: role.to_string(),
        expires_at: match ttl_secs {
            Some(t) => now() + t,
            None => 0, // persists until revoked
        },
        granted_at: now(),
        network,
        channel,
    };
    let g = store_lock(store)?;
    g.meta_set(
        &format!("{GRANT_PREFIX}{}", identity.to_lowercase()),
        &serde_json::to_string(&grant)?,
    )?;
    Ok(grant)
}

/// Direct grant: owner names an identity and a role, no pairing code.
/// The management path for `veles pair`-style tooling, the desktop ACL
/// UI and the IRC tab's roster menu. Owner is not grantable; banned IS
/// (a runtime ban is the management path's emergency brake — a
/// mask-shaped ban in particular is how you throw a spoofable
/// troublemaker out without touching config).
pub fn grant(
    store: &Arc<std::sync::Mutex<Store>>,
    identity: &str,
    role: &str,
    ttl_secs: Option<u64>,
) -> anyhow::Result<Grant> {
    grant_scoped(store, identity, role, ttl_secs, None, None)
}

/// Direct grant with a network/channel scope and mask-shaped
/// identities. A mask identity that does not parse is refused here
/// rather than stored as a rule that can never match.
pub fn grant_scoped(
    store: &Arc<std::sync::Mutex<Store>>,
    identity: &str,
    role: &str,
    ttl_secs: Option<u64>,
    network: Option<String>,
    channel: Option<String>,
) -> anyhow::Result<Grant> {
    let identity = identity.trim().to_string();
    if identity.is_empty() {
        anyhow::bail!("identity must not be empty");
    }
    if identity.len() > 128 {
        anyhow::bail!("identity too long");
    }
    if crate::ircmask::looks_like_mask(&identity) {
        crate::ircmask::MaskPattern::parse(&identity)
            .map_err(|e| anyhow::anyhow!("invalid mask pattern: {e}"))?;
    }
    if Role::parse(role).is_none() || role == "owner" {
        anyhow::bail!("invalid role '{role}' (banned|trusted|user|guest; owner is not grantable)");
    }
    persist_grant(store, &identity, role, ttl_secs, network, channel)
}

/// Does one stored grant describe this caller? Mask identities match
/// the observed wire mask; exact identities compare case-insensitively.
/// Scopes (network/channel), when present, must equal the observation.
fn grant_matches(g: &Grant, id: Option<&str>, ctx: &crate::acl::SenderContext) -> bool {
    use crate::ircmask::{looks_like_mask, MaskPattern};
    let ident_ok = if looks_like_mask(&g.identity) {
        match (ctx.mask.as_deref(), MaskPattern::parse(&g.identity)) {
            (Some(mask), Ok(p)) => p.matches(mask),
            _ => false,
        }
    } else {
        id.is_some_and(|id| id.trim().eq_ignore_ascii_case(g.identity.trim()))
    };
    if !ident_ok {
        return false;
    }
    if let Some(net) = &g.network {
        match ctx.network.as_deref() {
            Some(n) if n.eq_ignore_ascii_case(net) => {}
            _ => return false,
        }
    }
    if let Some(chan) = &g.channel {
        match ctx.channel.as_deref() {
            Some(c) if c.eq_ignore_ascii_case(chan) => {}
            _ => return false,
        }
    }
    true
}

/// Every live grant that describes this caller — exact identity or
/// matching mask, scope respected, expired entries dropped. Resolution
/// walks this list; the ban pass and the role pass both read it, which
/// is why it returns all matches rather than a first.
pub fn grants_matching(
    store: &Arc<std::sync::Mutex<Store>>,
    id: Option<&str>,
    ctx: &crate::acl::SenderContext,
) -> Vec<Grant> {
    list_grants(store)
        .into_iter()
        .filter(|g| g.expires_at == 0 || now() <= g.expires_at)
        .filter(|g| grant_matches(g, id, ctx))
        .collect()
}

// ── runtime users (cross-network identities) ─────────────────────────
//
// Named users group identities — platform ids and IRC masks — under one
// role that holds across networks. Config can declare them ([[acl.users]]);
// these runtime records let the desktop ACL UI and IRC management create
// them without a config rewrite, exactly like grants.

const USER_PREFIX: &str = "acl_user:";

/// A runtime user record — the same shape the config declares, so
/// resolution can walk config users and runtime users as one list.
pub type RuntimeUser = crate::acl::AclUser;

/// Create or REPLACE a runtime user. The name keys the record; role must
/// parse (owner is allowed here — unlike grants, a user record is a
/// grouping device the owner curates, and a cross-network owner mask is
/// the point).
pub fn user_save(
    store: &Arc<std::sync::Mutex<Store>>,
    name: &str,
    role: &str,
    identities: &[String],
) -> anyhow::Result<RuntimeUser> {
    let name = name.trim().to_string();
    if name.is_empty() {
        anyhow::bail!("user name must not be empty");
    }
    if name.len() > 64 {
        anyhow::bail!("user name too long");
    }
    if Role::parse(role).is_none() {
        anyhow::bail!("invalid role '{role}'");
    }
    for id in identities {
        let id = id.trim();
        if id.is_empty() || id.len() > 128 {
            anyhow::bail!("identity '{id}' is empty or too long");
        }
        if crate::ircmask::looks_like_mask(id) {
            crate::ircmask::MaskPattern::parse(id)
                .map_err(|e| anyhow::anyhow!("invalid mask pattern '{id}': {e}"))?;
        }
    }
    let user = RuntimeUser {
        name: name.clone(),
        role: role.to_string(),
        identities: identities.iter().map(|s| s.trim().to_string()).collect(),
        ..Default::default()
    };
    let g = store_lock(store)?;
    g.meta_set(
        &format!("{USER_PREFIX}{}", name.to_lowercase()),
        &serde_json::to_string(&user)?,
    )?;
    Ok(user)
}

/// Remove a runtime user. True when something was removed.
pub fn user_delete(store: &Arc<std::sync::Mutex<Store>>, name: &str) -> bool {
    let Ok(g) = store.lock() else { return false };
    let key = format!("{USER_PREFIX}{}", name.trim().to_lowercase());
    if g.meta_get(&key).is_none() {
        return false;
    }
    g.meta_delete(&key).unwrap_or(false)
}

/// All runtime users, sorted by name.
pub fn list_users(store: &Arc<std::sync::Mutex<Store>>) -> Vec<RuntimeUser> {
    let mut out = Vec::new();
    let Ok(g) = store.lock() else { return out };
    for key in g.meta_scan_prefix(USER_PREFIX) {
        if let Some(raw) = g.meta_get(&key) {
            if let Ok(u) = serde_json::from_str::<RuntimeUser>(&raw) {
                out.push(u);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// All live grants (owner listing), sorted by identity.
pub fn list_grants(store: &Arc<std::sync::Mutex<Store>>) -> Vec<Grant> {
    let mut out = Vec::new();
    let Ok(g) = store.lock() else { return out };
    for key in g.meta_scan_prefix(GRANT_PREFIX) {
        if let Some(raw) = g.meta_get(&key) {
            if let Ok(grant) = serde_json::from_str::<Grant>(&raw) {
                out.push(grant);
            }
        }
    }
    out.sort_by(|a, b| a.identity.cmp(&b.identity));
    out
}

/// Revoke one identity's grant. True when something was removed.
///
/// Not `.is_ok()`: `DELETE` of a key that was never there is not an
/// error, so that spelling reported a revocation for an identity with no
/// grant — and `/revoke` printed "revoked" for a typo.
pub fn revoke(store: &Arc<std::sync::Mutex<Store>>, identity: &str) -> bool {
    let Ok(g) = store.lock() else { return false };
    g.meta_delete(&format!("{GRANT_PREFIX}{}", identity.to_lowercase()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_store() -> Arc<std::sync::Mutex<Store>> {
        let dir = crate::testenv::kept_dir();
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(std::sync::Mutex::new(Store::open(&dir.join("db")).unwrap()))
    }

    #[test]
    fn code_shape_is_unambiguous() {
        for _ in 0..50 {
            let c = generate_code();
            assert!(valid_shape(&c), "{c}");
            assert_eq!(c.len(), 9, "XXXX-XXXX"); // 4 + dash + 4
            assert!(!c.contains('0') && !c.contains('O'));
            assert!(!c.contains('1') && !c.contains('I') && !c.contains('L'));
        }
        assert!(!valid_shape("ABCD")); // no dash
        assert!(!valid_shape("AB1D-EFGH")); // 1 excluded
    }

    #[test]
    fn create_redeem_revoke_roundtrip() {
        let store = scratch_store();
        let (code, exp) = create(&store, "trusted", Some(600)).unwrap();
        assert!(exp > now());
        // Unknown code rejected.
        assert!(redeem(&store, "ZZZZ-ZZZZ", "telegram:42").is_err());
        // Redeem grants the role.
        assert_eq!(redeem(&store, &code, "telegram:42").unwrap(), "trusted");
        let grants = list_grants(&store);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].identity, "telegram:42");
        assert_eq!(grants[0].role, "trusted");
        // Code consumed — second redeem fails.
        assert!(redeem(&store, &code, "telegram:43").is_err());
        // Revoke clears it.
        assert!(revoke(&store, "telegram:42"));
        assert!(list_grants(&store).is_empty());
    }

    #[test]
    fn direct_grant_persists_lists_and_revokes() {
        let store = scratch_store();
        // Invalid targets are refused: owner is not grantable, unknown
        // roles do not parse, empty identities cannot be keyed.
        assert!(grant(&store, "irc:friend", "owner", None).is_err());
        assert!(grant(&store, "irc:friend", "emperor", None).is_err());
        assert!(grant(&store, "   ", "user", None).is_err());
        // A valid one sticks and resolves through the ACL hook.
        let g = grant(&store, "irc:friend", "trusted", None).unwrap();
        assert_eq!(g.role, "trusted");
        assert_eq!(runtime_role(&store, "irc:friend"), Some(Role::Trusted));
        assert_eq!(list_grants(&store).len(), 1);
        // Regranting REPLACES rather than stacking.
        let _ = grant(&store, "irc:friend", "user", None).unwrap();
        let grants = list_grants(&store);
        assert_eq!(grants.len(), 1, "one identity, one grant");
        assert_eq!(grants[0].role, "user");
        // Revoke clears it back to config-only resolution.
        assert!(revoke(&store, "irc:friend"));
        assert!(list_grants(&store).is_empty());
        assert_eq!(runtime_role(&store, "irc:friend"), None);
    }

    #[test]
    fn owner_role_not_grantable() {
        let store = scratch_store();
        assert!(create(&store, "owner", None).is_err());
    }

    #[test]
    fn expired_code_refused_and_cleaned() {
        let store = scratch_store();
        // ttl 0 seconds → already expired at redemption time.
        let (code, _) = create(&store, "user", Some(0)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(redeem(&store, &code, "telegram:9").is_err());
        // The pending entry was cleaned up.
        let g = store_lock(&store).unwrap();
        assert!(g.meta_get(&format!("{PENDING_PREFIX}{code}")).is_none());
    }

    // ── IRC masks, scopes, bans, users (2026-08-30) ──────────────────

    fn irc_ctx(mask: &str, network: &str, channel: &str) -> crate::acl::SenderContext {
        crate::acl::SenderContext {
            mask: Some(mask.into()),
            network: Some(network.into()),
            channel: if channel.is_empty() {
                None
            } else {
                Some(channel.into())
            },
            platform: Some("irc".into()),
            chat_id: if channel.is_empty() {
                None
            } else {
                Some(channel.into())
            },
            direct: channel.is_empty(),
            handle: None,
            post: None,
        }
    }

    #[test]
    fn grant_scoped_validates_masks_and_bans() {
        let store = scratch_store();
        // A mask identity that does not parse is refused up front.
        assert!(grant_scoped(&store, "re:[bad!*@*", "user", None, None, None).is_err());
        // banned IS grantable (the management path's emergency brake)…
        assert!(grant_scoped(&store, "troll!*@trollfarm.net", "banned", None, None, None).is_ok());
        // …owner is not.
        assert!(grant_scoped(&store, "irc:x", "owner", None, None, None).is_err());
        // A scoped exact-identity grant stores its scope.
        let g = grant_scoped(
            &store,
            "irc:scoped",
            "trusted",
            None,
            Some("libera".into()),
            Some("#veles".into()),
        )
        .unwrap();
        assert_eq!(g.network.as_deref(), Some("libera"));
        assert_eq!(g.channel.as_deref(), Some("#veles"));
    }

    #[test]
    fn grants_matching_respects_mask_scope_and_expiry() {
        let store = scratch_store();
        grant_scoped(
            &store,
            "nyx!~n@*.example.com",
            "trusted",
            None,
            Some("libera".into()),
            None,
        )
        .unwrap();
        grant(&store, "irc:exact", "user", None).unwrap();

        // Mask matches, network matches → one grant.
        let m = grants_matching(
            &store,
            None,
            &irc_ctx("nyx!~n@box.example.com", "libera", ""),
        );
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].role, "trusted");
        // Right mask, wrong network → none.
        let m = grants_matching(&store, None, &irc_ctx("nyx!~n@box.example.com", "oftc", ""));
        assert!(m.is_empty());
        // Exact identity, case-insensitive, with a mask context that
        // must not leak into the comparison.
        let m = grants_matching(
            &store,
            Some("IRC:EXACT"),
            &irc_ctx("someone!else@elsewhere", "libera", ""),
        );
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].role, "user");
        // A different mask does not match the pattern.
        let m = grants_matching(
            &store,
            None,
            &irc_ctx("eve!~e@box.example.com", "libera", ""),
        );
        assert!(m.is_empty());
    }

    #[test]
    fn users_save_list_delete_roundtrip() {
        let store = scratch_store();
        assert!(user_save(&store, "   ", "user", &[]).is_err(), "empty name");
        assert!(
            user_save(&store, "pal", "emperor", &[]).is_err(),
            "bad role"
        );
        assert!(
            user_save(&store, "pal", "user", &["re:[bad!*@*".into()]).is_err(),
            "bad mask identity"
        );

        let u = user_save(
            &store,
            "pal",
            "trusted",
            &["irc:pal".into(), "pal!~p@*.pal.net".into()],
        )
        .unwrap();
        assert_eq!(u.identities.len(), 2);
        let all = list_users(&store);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "pal");

        // Saving the same name REPLACES the record.
        user_save(&store, "Pal", "user", &["irc:pal".into()]).unwrap();
        let all = list_users(&store);
        assert_eq!(all.len(), 1, "one user per name");
        assert_eq!(all[0].role, "user");
        assert_eq!(all[0].identities.len(), 1);

        assert!(user_delete(&store, "pal"));
        assert!(list_users(&store).is_empty());
        assert!(!user_delete(&store, "pal"), "second delete finds nothing");
    }
}
