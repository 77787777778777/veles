//! authn — bearer/peer-token authentication for veles' HTTP surfaces.
//!
//! WHY THIS MODULE EXISTS
//!
//! The desktop API's token auth was first written *inside* `a2a.rs`, as
//! `desktop_authenticate` / `desktop_peer_tokens` / `desktop_bearer_token`
//! / `resolve_desktop_bind_host` — four functions that were a byte-for-byte
//! copy of A2A's own, differing only in which env keys they read. That made
//! the A2A module a dependency of the desktop client for no reason: the
//! desktop speaks no A2A, and the IRC pane behind it speaks even less. The
//! only genuinely shared machinery is a constant-time compare and an
//! env-or-dotenv lookup, neither of which has anything to do with
//! agent-to-agent RPC.
//!
//! So the mechanism lives here, parameterised by env key, and both
//! surfaces call it. `a2a.rs` keeps `authenticate` / `resolve_bind_host`
//! as thin wrappers so `a2a_server.rs` is untouched.
//!
//! THE RULE, unchanged from hermes' security.py and A2A's port of it:
//!
//!   - no token configured ⇒ loopback only, identity "ip:<addr>"
//!   - a token configured  ⇒ a valid bearer is required
//!   - a token alone does NOT widen the bind; that needs a host set as
//!     well (the host env var, or the surface's config key — pitfall 75)
//!
//! THE ONE THING THAT IS NOT LIKE THE ORIGINAL: `AuthConfig` is a
//! SNAPSHOT, read once at serve time, not a pair of functions that
//! re-read the environment on every request.
//!
//! Tokens are supported in `$VELES_HOME/.env`, so a live re-read makes
//! the auth decision depend on a file that can be rotated, truncated, or
//! momentarily half-written while the server runs. Every one of those
//! makes `peers.is_empty() && shared.is_empty()` true for a moment — and
//! that branch returns Some(identity) for ANY caller. A server that came
//! up bound to 0.0.0.0 (which requires a token) would then serve
//! `/api/tcl/eval` and `/api/fs/write` to the network unauthenticated
//! until the file came back. Snapshotting is what makes the bind decision
//! and the auth decision agree for the life of the process; it also takes
//! two blocking file reads per request off the async path, which is what
//! the previous shape did on the DEFAULT no-token install where neither
//! env var is set.

/// Constant-time string compare (no subtle dep needed for 32-byte tokens).
/// Shared with process plugins (`veles_types::authn`); re-exported here.
pub use veles_types::authn::ct_eq;

/// Env/dotenv lookup — veles' own sources ONLY, through the one
/// resolver (`secrets::resolve`: the operator's environment, the vault,
/// veles' own env files). The migration fallback to ~/.hermes/.env must
/// NOT apply here: hermes' own gateway runs an A2A server with its own
/// peer tokens; inheriting them would (a) force token auth on a fresh
/// veles install and (b) leak hermes' credentials into veles' identity.
pub fn env_or_dotenv(name: &str) -> String {
    crate::secrets::resolve(name).unwrap_or_default()
}

// The token-auth model (`AuthConfig`, `ct_eq`, `parse_bearer`,
// `peer_tokens_from`) moved to `veles_types::authn` (Phase 3): a process
// plugin authenticates its own consumers with the same code. The RESOLUTION
// of the tokens stays here — a plugin asks `secret.get` and builds an
// `AuthConfig` from the values; only core reads a secrets source.
pub(crate) use veles_types::authn::peer_tokens_from;
pub use veles_types::authn::{parse_bearer, AuthConfig};

/// Read a surface's token configuration from the environment (and
/// `$VELES_HOME/.env`), once, at serve time — the module docstring says
/// why the hermes fallback must not apply. A plugin cannot do this (it has
/// no secrets source); it asks `secret.get` and builds an `AuthConfig`
/// from the values.
pub fn from_env(peer_key: &str, bearer_key: &str) -> AuthConfig {
    AuthConfig::new(
        peer_tokens_from(&env_or_dotenv(peer_key)),
        env_or_dotenv(bearer_key),
    )
}

/// Bind safety: a token alone does NOT widen the bind; remote exposure
/// needs a token AND a host set explicitly.
///
/// The host is `host_key` from the process environment, then from
/// `.env` (both through `env_or_dotenv`), then `config_fallback` — the
/// surface's config-file value, e.g. `platforms.a2a.extra.host` — and
/// loopback when none is set. The same order `A2A_PORT` has against
/// `platforms.a2a.extra.port`. A host from config is still only a host:
/// without a token the bind stays on loopback.
pub fn resolve_bind_host(
    host_key: &str,
    cfg: &AuthConfig,
    config_fallback: Option<&str>,
) -> String {
    let mut host = env_or_dotenv(host_key);
    if host.is_empty() {
        host = config_fallback.unwrap_or("").trim().to_string();
    }
    if host.is_empty() {
        return "127.0.0.1".to_string();
    }
    if cfg.is_open() {
        tracing::warn!(
            "bind host {host} set ({host_key} or config) but no token configured — refusing to widen bind, staying on 127.0.0.1"
        );
        return "127.0.0.1".to_string();
    }
    host
}

// ── the desktop surface's token ──────────────────────────────────────
//
// Named here because four parties have to agree on the spelling: the
// server that authenticates with it (`desktop_api::serve`), the gateway
// that mints it, the launcher that hands it to the desktop as
// `VELES_TOKEN`, and `veles tcl`, which is a client of the same port.

/// The shared bearer token the desktop API authenticates with.
pub const DESKTOP_BEARER_KEY: &str = "DESKTOP_BEARER_TOKEN";
/// Named peers (`name:token,…`), for a second machine's desktop.
pub const DESKTOP_PEER_KEY: &str = "DESKTOP_PEER_TOKENS";
/// Set to keep the historical posture deliberately: open, loopback only,
/// no token minted. See `desktop_open_opted_out`.
pub const DESKTOP_OPEN_KEY: &str = "VELES_DESKTOP_OPEN";

/// The desktop API's shared bearer token, from the environment or
/// `<config_dir>/.env`. Empty when the API is open.
///
/// This is what a launcher hands the desktop and what a CLI client sends;
/// reading it through the same helper the server resolves means a client
/// cannot disagree with the gateway about which file the token lives in.
pub fn desktop_bearer_token() -> String {
    env_or_dotenv(DESKTOP_BEARER_KEY)
}

/// True when the operator has deliberately asked for the open posture.
///
/// An escape hatch, not a configuration surface: minting a token is the
/// default and this is the one way to decline it. Read from the
/// ENVIRONMENT only — a `.env` line would be indistinguishable from the
/// token the same file holds, and "no auth" should not be a setting one
/// inherits by copying a dotfile.
pub fn desktop_open_opted_out() -> bool {
    match std::env::var(DESKTOP_OPEN_KEY) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => false,
    }
}

/// A fresh bearer token: 64 hex characters from two v4 UUIDs.
///
/// `uuid` is the crate's existing source of randomness (`pairing.rs`
/// mints pairing codes with it), so this adds no dependency — and 244
/// bits is past the point where the shape of the token matters, as long
/// as it is not guessable and not a word anybody would type.
fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Mint a bearer token and persist it under `key` in
/// `<config_dir>/.env`. Returns the path written, for the log line that
/// has to tell the operator where their new credential went.
///
/// A FILE, not the process environment: the token has to outlive the
/// process that minted it. A gateway that kept it in memory would hand
/// every restart a different token and lock out the desktop that
/// remembered the last one — which teaches an operator to switch the
/// protection off, the worst available outcome.
///
/// MERGE, never clobber. `.env` is shared config: it is the file
/// `env_or_dotenv` reads every key from, the one provider keys live in,
/// and the one `scripts/remote-demo.sh` writes. An existing line for
/// `key` is replaced — including an EMPTY one, which `env_or_dotenv`
/// reads as absent and which would otherwise leave two lines behind, the
/// first of them winning.
///
/// 0600, and atomic (a temp file in the same directory, then rename) —
/// the shape `veles-desktop/lib/instances.tcl` uses for its saved-gateway
/// registry, for the same two reasons: a half-written secret file is one
/// that does not parse, and a world-readable one is a token for every
/// local user.
pub fn issue_dotenv_token(key: &str) -> anyhow::Result<std::path::PathBuf> {
    write_dotenv(key, &new_token())
}

/// The value `<config_dir>/.env` gives `key` (the file only, not the
/// process environment), when it names one.
pub fn dotenv_value(key: &str) -> Option<String> {
    let path = crate::config::config_dir().join(".env");
    let text = std::fs::read_to_string(path).ok()?;
    let prefix = format!("{key}=");
    text.lines()
        .filter_map(|l| {
            let l = l.trim_start();
            l.strip_prefix("export ")
                .unwrap_or(l)
                .strip_prefix(&prefix)
                .map(str::to_string)
        })
        .next_back()
        .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
}

/// Write `key=value` into `<config_dir>/.env`, by the rule above: merged,
/// atomic, 0600. Pool pairing writes its peer tokens through this.
pub fn write_dotenv(key: &str, value: &str) -> anyhow::Result<std::path::PathBuf> {
    write_dotenv_in(&crate::config::config_dir(), key, value)
}

/// [`write_dotenv`] for another installation's config directory: the root
/// installer handoff writes a system service's `.env`, never its own.
pub fn write_dotenv_in(
    dir: &std::path::Path,
    key: &str,
    value: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let token = value.to_string();
    let path = dir.join(".env");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let prefix = format!("{key}=");
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.trim_start().starts_with(&prefix))
        .map(str::to_string)
        .collect();
    lines.push(format!("{prefix}{token}"));
    let body = format!("{}\n", lines.join("\n"));

    let tmp = path.with_file_name(format!(".env.minting-{}", std::process::id()));
    std::fs::write(&tmp, body.as_bytes())?;
    set_secret_perms(&tmp)?;
    std::fs::rename(&tmp, &path)?;
    // rename carries the source's mode, so this is belt-and-braces for a
    // `.env` that already existed with looser permissions: whatever else
    // is in it, it is credentials.
    set_secret_perms(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn set_secret_perms(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_secret_perms(_path: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(peer_key: &str, bearer_key: &str) -> AuthConfig {
        from_env(peer_key, bearer_key)
    }

    #[test]
    fn peer_tokens_parse_and_drop_junk() {
        let _g = crate::testenv::lock();
        std::env::remove_var("T_PEERS");
        std::env::remove_var("T_BEARER");
        assert!(cfg("T_PEERS", "T_BEARER").is_open());

        std::env::set_var(
            "T_PEERS",
            "alice:tok1, bob:tok2,   , :novalue, charlie:tok3,",
        );
        std::env::set_var("T_BEARER", "shared-tok");
        let c = cfg("T_PEERS", "T_BEARER");
        assert_eq!(
            c.peer_count(),
            3,
            "empty entries and colon-less pairs dropped"
        );
        assert_eq!(
            c.authenticate(Some("Bearer tok2"), "10.0.0.2"),
            Some("bob".into())
        );
        assert_eq!(
            c.authenticate(Some("bearer shared-tok"), "10.0.0.2"),
            Some("ip:10.0.0.2".into()),
            "scheme is case-insensitive; the shared token yields an ip label"
        );
        std::env::remove_var("T_PEERS");
        std::env::remove_var("T_BEARER");
    }

    #[test]
    fn wrong_and_missing_token_reject() {
        let _g = crate::testenv::lock();
        std::env::remove_var("T_PEERS");
        std::env::set_var("T_BEARER", "right");
        let c = cfg("T_PEERS", "T_BEARER");
        assert_eq!(c.authenticate(None, "10.0.0.2"), None);
        assert_eq!(c.authenticate(Some("Bearer wrong"), "10.0.0.2"), None);
        assert_eq!(c.authenticate(Some("Basic right"), "10.0.0.2"), None);
        std::env::remove_var("T_BEARER");
    }

    /// The whole point of the snapshot.
    ///
    /// Tokens are supported in `$VELES_HOME/.env`. The shape this replaced
    /// re-read that file on EVERY request, so truncating or rotating it
    /// while the server ran flipped `is_open()` to true and every route
    /// answered unauthenticated — on a bind that had been widened to the
    /// network precisely BECAUSE a token existed. A snapshot cannot do
    /// that: once the config says "token mode", it says so for the life of
    /// the process.
    ///
    /// Mutation check: make `authenticate` call `AuthConfig::from_env`
    /// instead of reading `self`, and this goes red.
    #[test]
    fn a_configured_auth_never_falls_back_to_open() {
        let _g = crate::testenv::lock();
        std::env::remove_var("T_PEERS");
        std::env::set_var("T_BEARER", "right");
        let c = cfg("T_PEERS", "T_BEARER");
        assert!(!c.is_open());

        // The config source disappears out from under the running server.
        std::env::remove_var("T_BEARER");
        assert!(
            !c.is_open(),
            "the snapshot must not re-read the environment"
        );
        assert_eq!(
            c.authenticate(None, "203.0.113.7"),
            None,
            "a server that started in token mode must never serve unauthenticated"
        );
        assert_eq!(
            c.authenticate(Some("Bearer right"), "203.0.113.7"),
            Some("ip:203.0.113.7".into()),
            "…and the token it started with must keep working"
        );
    }

    /// A roaming client changes address constantly; the same token must
    /// authorise from any of them.
    #[test]
    fn identity_is_ip_agnostic_for_roaming() {
        let _g = crate::testenv::lock();
        std::env::remove_var("T_PEERS");
        std::env::set_var("T_BEARER", "roam-token");
        let c = cfg("T_PEERS", "T_BEARER");
        let hdr = Some("Bearer roam-token");
        assert!(c.authenticate(hdr, "10.0.0.2").is_some());
        assert!(c.authenticate(hdr, "203.0.113.7").is_some());
        assert!(c.authenticate(hdr, "2001:db8::1").is_some());
        std::env::remove_var("T_BEARER");
    }

    #[test]
    fn bind_stays_loopback_without_a_token() {
        let _g = crate::testenv::lock();
        std::env::remove_var("T_PEERS");
        std::env::remove_var("T_BEARER");
        std::env::remove_var("T_HOST");
        let open = cfg("T_PEERS", "T_BEARER");
        assert_eq!(resolve_bind_host("T_HOST", &open, None), "127.0.0.1");

        std::env::set_var("T_HOST", "0.0.0.0");
        assert_eq!(
            resolve_bind_host("T_HOST", &open, None),
            "127.0.0.1",
            "a host without a token must NOT widen the bind"
        );

        std::env::set_var("T_BEARER", "tok");
        let closed = cfg("T_PEERS", "T_BEARER");
        assert_eq!(resolve_bind_host("T_HOST", &closed, None), "0.0.0.0");
        std::env::remove_var("T_HOST");
        std::env::remove_var("T_BEARER");
    }

    /// The host key goes through `env_or_dotenv` like every other key, so
    /// an operator who keeps deployment settings in `$VELES_HOME/.env`
    /// gets the same behaviour as one who exports them. The desktop's
    /// previous `std::env::var` read silently ignored the file while
    /// honouring it for the token in the same file.
    #[test]
    fn host_key_is_read_from_dotenv_too() {
        let _g = crate::testenv::lock();
        let dir = tempfile::tempdir().expect("tmpdir");
        let prev = std::env::var("VELES_HOME").ok();
        std::env::set_var("VELES_HOME", dir.path());
        std::env::remove_var("T_HOST");
        std::env::remove_var("T_PEERS");
        std::env::remove_var("T_BEARER");
        std::fs::write(
            dir.path().join(".env"),
            "T_BEARER=from-file\nT_HOST=0.0.0.0\n",
        )
        .expect("write .env");

        let c = cfg("T_PEERS", "T_BEARER");
        assert!(!c.is_open(), "the token comes from the file");
        assert_eq!(resolve_bind_host("T_HOST", &c, None), "0.0.0.0");

        match prev {
            Some(v) => std::env::set_var("VELES_HOME", v),
            None => std::env::remove_var("VELES_HOME"),
        }
    }

    /// The host has three homes, ranked like `A2A_PORT`'s: the process
    /// environment, then `$VELES_HOME/.env`, then the node-local config
    /// key the caller hands in (`platforms.a2a.extra.host`), then
    /// loopback. Pool join writes the config key, so an export or a
    /// `.env` line still overrides what it wrote.
    #[test]
    fn bind_host_ranks_env_then_dotenv_then_config_then_loopback() {
        let (home, _env) = crate::testenv::scratch_home();
        std::env::remove_var("T_HOST");
        std::env::remove_var("T_PEERS");
        std::env::set_var("T_BEARER", "tok");
        let closed = cfg("T_PEERS", "T_BEARER");
        assert!(!closed.is_open());

        assert_eq!(resolve_bind_host("T_HOST", &closed, None), "127.0.0.1");
        assert_eq!(
            resolve_bind_host("T_HOST", &closed, Some("10.0.30.3")),
            "10.0.30.3",
            "the config key decides when nothing overrides it"
        );
        assert_eq!(
            resolve_bind_host("T_HOST", &closed, Some("  ")),
            "127.0.0.1",
            "a blank config value is no host"
        );

        std::fs::write(home.path().join(".env"), "T_HOST=10.0.0.9\n").expect("write .env");
        assert_eq!(
            resolve_bind_host("T_HOST", &closed, Some("10.0.30.3")),
            "10.0.0.9",
            ".env beats config"
        );

        std::env::set_var("T_HOST", "0.0.0.0");
        assert_eq!(
            resolve_bind_host("T_HOST", &closed, Some("10.0.30.3")),
            "0.0.0.0",
            "the process environment beats .env"
        );
        std::env::remove_var("T_HOST");
        std::env::remove_var("T_BEARER");
    }

    /// The bind-safety rule covers the new home too: a host that comes
    /// from config.toml, with no token configured, stays on loopback.
    #[test]
    fn a_config_host_without_a_token_stays_loopback() {
        let (_home, _env) = crate::testenv::scratch_home();
        std::env::remove_var("T_HOST");
        std::env::remove_var("T_PEERS");
        std::env::remove_var("T_BEARER");
        let open = cfg("T_PEERS", "T_BEARER");
        assert!(open.is_open());
        assert_eq!(
            resolve_bind_host("T_HOST", &open, Some("0.0.0.0")),
            "127.0.0.1",
            "a config host without a token must NOT widen the bind"
        );
    }

    // ── minting the desktop's token ──────────────────────────────────

    /// A minted token lands in CONFIG's `.env` (not data, not the process
    /// environment), merged with what was already there, at 0600 — and the
    /// surface stops being open, which is the only reason to mint one.
    #[test]
    fn issuing_a_token_merges_into_the_config_dotenv_at_0600() {
        let (_cfg_root, _data_root, _g) = crate::testenv::scratch_xdg();
        std::env::remove_var(DESKTOP_BEARER_KEY);
        std::env::remove_var(DESKTOP_PEER_KEY);
        let dir = crate::config::config_dir();
        std::fs::create_dir_all(&dir).expect("config dir");
        let path = dir.join(".env");
        // A provider key that must survive, and an EMPTY token line — the
        // shape `env_or_dotenv` reads as absent, so minting has to replace
        // it rather than append a second line the first one shadows.
        std::fs::write(
            &path,
            "SOME_PROVIDER_KEY=sk-live-abc\nDESKTOP_BEARER_TOKEN=\n",
        )
        .expect("write .env");
        // Looser than a secret file should be, to prove the mint tightens
        // what it finds as well as what it creates.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        assert!(
            cfg(DESKTOP_PEER_KEY, DESKTOP_BEARER_KEY).is_open(),
            "an empty token line is absent, so the surface starts open"
        );

        let written = issue_dotenv_token(DESKTOP_BEARER_KEY).expect("mint");
        assert_eq!(written, path, "the log line must name the real path");

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("SOME_PROVIDER_KEY=sk-live-abc"),
            "the merge dropped a neighbouring key: {body}"
        );
        assert_eq!(
            body.matches("DESKTOP_BEARER_TOKEN=").count(),
            1,
            "the empty line must be REPLACED, not duplicated: {body}"
        );
        let token = desktop_bearer_token();
        assert_eq!(token.len(), 64, "two v4 UUIDs, hex: {token:?}");
        assert!(
            token.chars().all(|c| c.is_ascii_hexdigit()),
            "a token that needs quoting is a token that breaks a shell: {token:?}"
        );
        assert!(
            !cfg(DESKTOP_PEER_KEY, DESKTOP_BEARER_KEY).is_open(),
            "…and the surface is no longer open"
        );
        assert!(
            cfg(DESKTOP_PEER_KEY, DESKTOP_BEARER_KEY)
                .authenticate(Some(&format!("Bearer {token}")), "127.0.0.1")
                .is_some(),
            "the minted token must authenticate against a freshly resolved config"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the .env must be owner-only: {mode:o}");
        }
    }

    /// The escape hatch is an environment switch and nothing else: a
    /// `.env` line would be inherited by copying a dotfile, and "serve the
    /// mutating API with no credential" is not a setting that should
    /// travel.
    #[test]
    fn the_open_opt_out_is_an_environment_switch() {
        let _g = crate::testenv::lock();
        std::env::remove_var(DESKTOP_OPEN_KEY);
        assert!(!desktop_open_opted_out(), "unset means mint a token");
        for no in ["", "0", "false", "no", "off", "FALSE", " Off "] {
            std::env::set_var(DESKTOP_OPEN_KEY, no);
            assert!(!desktop_open_opted_out(), "{no:?} is not an opt-out");
        }
        for yes in ["1", "true", "yes", "on", "TRUE"] {
            std::env::set_var(DESKTOP_OPEN_KEY, yes);
            assert!(desktop_open_opted_out(), "{yes:?} must opt out");
        }
        std::env::remove_var(DESKTOP_OPEN_KEY);
    }
}
