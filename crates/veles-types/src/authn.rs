//! Shared token authentication for a veles surface (Phase 3, D2). The
//! constant-time compare and the {token: peer} model are core's
//! `authn`, moved here so a process plugin can authenticate its own
//! loopback consumers without linking core (the atproto fan-out did,
//! until the jetstream-relay replaced it on 2026-10-10). The
//! RESOLUTION of the tokens (env, the vault, .env) stays with the host —
//! a plugin asks `secret.get` and builds an [`AuthConfig`] from the
//! values; it never reads a secrets source itself.

use std::collections::HashMap;

/// Constant-time string compare (no subtle dep needed for 32-byte tokens).
pub fn ct_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in ab.iter().zip(bb.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// `Bearer <token>` → the token. Any other scheme, or no header, is None.
pub fn parse_bearer(auth_header: Option<&str>) -> Option<String> {
    let h = auth_header?;
    let mut parts = h.splitn(2, char::is_whitespace);
    let scheme = parts.next()?.to_lowercase();
    if scheme != "bearer" {
        return None;
    }
    Some(parts.next()?.trim().to_string())
}

/// `"alice:tok1,bob:tok2"` → {token: peer_name}.
pub fn peer_tokens_from(value: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in value.split(',') {
        let pair = pair.trim();
        if let Some((name, token)) = pair.split_once(':') {
            let (name, token) = (name.trim(), token.trim());
            if !name.is_empty() && !token.is_empty() {
                out.insert(token.to_string(), name.to_string());
            }
        }
    }
    out
}

/// One surface's token configuration, resolved once.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    peers: HashMap<String, String>,
    shared: String,
}

impl AuthConfig {
    /// Build directly from already-resolved values: the {token: peer} map
    /// and a single shared bearer token. The host (or a plugin through
    /// `secret.get`) resolves the strings; this type only compares.
    pub fn new(peers: HashMap<String, String>, shared: String) -> Self {
        Self { peers, shared }
    }

    /// No token configured at all — the loopback-only posture.
    pub fn is_open(&self) -> bool {
        self.peers.is_empty() && self.shared.is_empty()
    }

    /// The {token: peer_name} map, for a caller that publishes it under
    /// its own name.
    pub fn peers(&self) -> HashMap<String, String> {
        self.peers.clone()
    }

    /// How many named peers are configured (for the startup log).
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Authenticate an inbound request → identity, or None to reject.
    ///
    /// The client IP is an identity LABEL, never a trust gate: a mobile
    /// client roams, and a loopback restriction is a property of the bind,
    /// imposed by the caller on the open branch.
    pub fn authenticate(&self, auth_header: Option<&str>, client_ip: &str) -> Option<String> {
        let label = |fallback: &str| {
            format!(
                "ip:{}",
                if client_ip.is_empty() {
                    fallback
                } else {
                    client_ip
                }
            )
        };
        if self.is_open() {
            return Some(label("local"));
        }
        let presented = parse_bearer(auth_header)?;
        for (token, name) in &self.peers {
            if ct_eq(&presented, token) {
                return Some(name.clone());
            }
        }
        if !self.shared.is_empty() && ct_eq(&presented, &self.shared) {
            return Some(label("unknown"));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parses_only_bearer() {
        assert_eq!(parse_bearer(Some("Bearer abc")).as_deref(), Some("abc"));
        assert_eq!(parse_bearer(Some("bearer abc")).as_deref(), Some("abc"));
        assert!(parse_bearer(Some("Basic abc")).is_none());
        assert!(parse_bearer(None).is_none());
    }

    #[test]
    fn peers_parse_and_empty_pairs_drop() {
        let m = peer_tokens_from("alice:t1, bob:t2 ,, :no, noname:");
        assert_eq!(m.get("t1").map(String::as_str), Some("alice"));
        assert_eq!(m.get("t2").map(String::as_str), Some("bob"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn an_open_config_authenticates_as_local() {
        let c = AuthConfig::default();
        assert!(c.is_open());
        assert_eq!(c.authenticate(None, "").as_deref(), Some("ip:local"));
    }

    #[test]
    fn a_named_peer_wins_over_the_shared_token() {
        let c = AuthConfig::new(peer_tokens_from("alice:t1"), "shared".into());
        assert_eq!(
            c.authenticate(Some("Bearer t1"), "").as_deref(),
            Some("alice")
        );
        assert_eq!(
            c.authenticate(Some("Bearer shared"), "1.2.3.4").as_deref(),
            Some("ip:1.2.3.4")
        );
        assert!(c.authenticate(Some("Bearer wrong"), "").is_none());
    }

    #[test]
    fn ct_eq_is_length_and_content_exact() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
    }
}
