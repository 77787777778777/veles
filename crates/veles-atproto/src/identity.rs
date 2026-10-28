//! Identity → role: map a DID to its ACL tier via the operator's graph.
//!
//! The DID itself is certain (it's in the signed commit). The *tier* is
//! the question: is this DID the operator, a mutual of the operator, or a
//! stranger? That is an AppView graph lookup, cached, and it FAILS CLOSED
//! — toward guest, never toward user — on any error.
//!
//! The resolved role is only a *hint* fed to the ACL's identity rules:
//! the operator pins `atproto:<operator_did>` to owner in config, mutuals
//! map to user, everyone else falls to `default_role = guest`. The ACL
//! owns enforcement; this module only answers "who is this DID to the
//! operator".

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::pds::PdsClient;

/// Who a DID is, relative to the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The operator's own DID.
    Operator,
    /// A DID in `platforms.atproto.co_operators`: admitted like the
    /// operator, but it is NOT the operator — its `[acl]` rule says what
    /// it may do, and it opts in to what a mutual opts in to.
    CoOperator,
    /// Follows the operator AND the operator follows back (a mutual).
    Mutual,
    /// Follows the operator but not followed back.
    Follower,
    /// The operator follows them but they don't follow back.
    Following,
    /// No graph relationship.
    Stranger,
}

/// The tier hint a Relation maps to, as an ACL role string.
impl Relation {
    pub fn role_hint(&self) -> &'static str {
        match self {
            Relation::Operator => "owner",
            // Whatever its rule grants is the ACL's to say; the hint is
            // the standing floor.
            Relation::CoOperator => "user",
            Relation::Mutual => "user",
            // A one-way follower is NOT a mutual: they get the guest
            // floor, not user. USER's brief: "mutuals get User".
            Relation::Follower | Relation::Following | Relation::Stranger => "guest",
        }
    }
}

/// A cached relation entry.
#[derive(Clone)]
struct Cached {
    rel: Relation,
    at: Instant,
}

/// The resolver. One per adapter; answers are cached for `ttl`. An
/// AppView error resolves to the least-privilege answer for THIS event
/// and is not cached, so a blip never locks a mutual out for the TTL.
pub struct IdentityResolver {
    client: PdsClient,
    operator_did: String,
    /// `platforms.atproto.co_operators`. Never asked about anyone else's
    /// relation: mutuals are the OPERATOR's.
    co_operators: Vec<String>,
    ttl: Duration,
    /// did → cached relation.
    cache: Mutex<HashMap<String, Cached>>,
    /// did → (handle, when looked up).
    handles: Mutex<HashMap<String, (String, Instant)>>,
}

impl IdentityResolver {
    pub fn new(client: PdsClient, operator_did: String, ttl: Duration) -> Self {
        Self {
            client,
            operator_did,
            co_operators: Vec::new(),
            ttl,
            cache: Mutex::new(HashMap::new()),
            handles: Mutex::new(HashMap::new()),
        }
    }

    /// Admit these DIDs as co-operators (`platforms.atproto.co_operators`).
    pub fn with_co_operators(mut self, dids: Vec<String>) -> Self {
        self.co_operators = dids;
        self
    }

    /// The handle a DID goes by, for DISPLAY (slopdrop's `[nick]`, the
    /// ledger's requester, the session's sender) — never for an ACL
    /// decision, which keys on the DID. A Jetstream event carries only the
    /// DID; since the bot moved to Jetstream every Bluesky caller showed as
    /// a bare DID. One AppView `getProfile`, cached for the TTL; `None`
    /// when the AppView does not answer or reports the handle invalid (the
    /// caller shows the DID, as before).
    pub async fn handle(&self, did: &str) -> Option<String> {
        if let Some((h, at)) = self.handles.lock().unwrap().get(did) {
            if at.elapsed() < self.ttl {
                return Some(h.clone());
            }
        }
        let h = match self.client.get_profile(did).await {
            Ok(v) => handle_from(&v, did)?,
            Err(e) => {
                tracing::info!("atproto: profile lookup for {did} failed: {e:#}");
                return None;
            }
        };
        self.handles
            .lock()
            .unwrap()
            .insert(did.to_string(), (h.clone(), Instant::now()));
        Some(h)
    }

    /// Resolve a DID to its relation, using the cache when fresh.
    pub async fn relation(&self, did: &str) -> Relation {
        if did.is_empty() {
            return Relation::Stranger;
        }
        if did == self.operator_did {
            return Relation::Operator;
        }
        if self.co_operators.iter().any(|d| d == did) {
            return Relation::CoOperator;
        }
        if let Some(c) = self.cache.lock().unwrap().get(did) {
            if c.at.elapsed() < self.ttl {
                return c.rel;
            }
        }
        let Some(rel) = self.resolve_live(did).await else {
            return Relation::Stranger;
        };
        self.cache.lock().unwrap().insert(
            did.to_string(),
            Cached {
                rel,
                at: Instant::now(),
            },
        );
        rel
    }

    /// The tier hint for a DID.
    pub async fn role_hint(&self, did: &str) -> &'static str {
        self.relation(did).await.role_hint()
    }

    /// The live lookup: ONE `app.bsky.graph.getRelationships` call,
    /// which answers both directions for any graph size. It replaced
    /// paging the operator's followers 100 at a time and giving up after
    /// ten pages — the operator has 1,806 followers, so every mutual past
    /// the first thousand read as "Following" and was refused, and each
    /// new stranger cost eleven AppView calls. `None` = the AppView did
    /// not answer (the caller fails closed and does not cache it).
    async fn resolve_live(&self, did: &str) -> Option<Relation> {
        if self.operator_did.is_empty() {
            return Some(Relation::Stranger);
        }
        match self.client.get_relationship(&self.operator_did, did).await {
            Ok(v) => Some(relation_from(&v, did)),
            Err(e) => {
                tracing::warn!("atproto: relationship lookup for {did} failed: {e:#}");
                None
            }
        }
    }
}

/// Read one DID's relation to the operator out of a getRelationships
/// answer. `following` = the operator follows them; `followedBy` = they
/// follow the operator. A block either way, a not-found actor, or no row
/// at all is a stranger.
pub(crate) fn relation_from(v: &serde_json::Value, did: &str) -> Relation {
    let Some(rel) = v
        .get("relationships")
        .and_then(|r| r.as_array())
        .and_then(|a| {
            a.iter()
                .find(|r| r.get("did").and_then(|d| d.as_str()) == Some(did))
        })
    else {
        return Relation::Stranger;
    };
    let has = |k: &str| {
        rel.get(k)
            .and_then(|u| u.as_str())
            .is_some_and(|u| !u.is_empty())
    };
    if rel.get("notFound").and_then(|b| b.as_bool()) == Some(true)
        || has("blocking")
        || has("blockedBy")
    {
        return Relation::Stranger;
    }
    match (has("followedBy"), has("following")) {
        (true, true) => Relation::Mutual,
        (true, false) => Relation::Follower,
        (false, true) => Relation::Following,
        (false, false) => Relation::Stranger,
    }
}

/// The handle out of a getProfile answer, when it is THIS DID's and the
/// AppView vouches for it (`handle.invalid` is what it shows for a handle
/// whose DNS/well-known no longer points back at the DID).
pub(crate) fn handle_from(v: &serde_json::Value, did: &str) -> Option<String> {
    if v.get("did").and_then(|d| d.as_str()) != Some(did) {
        return None;
    }
    v.get("handle")
        .and_then(|h| h.as_str())
        .filter(|h| !h.is_empty() && *h != "handle.invalid")
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_answer_yields_only_a_vouched_handle_of_that_did() {
        let p = |did: &str, h: &str| serde_json::json!({"did": did, "handle": h});
        assert_eq!(
            handle_from(&p("did:plc:x", "alice.example"), "did:plc:x").as_deref(),
            Some("alice.example")
        );
        assert_eq!(
            handle_from(&p("did:plc:x", "handle.invalid"), "did:plc:x"),
            None
        );
        assert_eq!(
            handle_from(&p("did:plc:y", "mallory.example"), "did:plc:x"),
            None
        );
        assert_eq!(
            handle_from(&serde_json::json!({"error": "x"}), "did:plc:x"),
            None
        );
    }

    fn rels(row: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"actor":"did:plc:op","relationships":[row]})
    }

    #[test]
    fn a_relationship_answer_reads_both_directions() {
        let d = "did:plc:x";
        let fol = "at://did:plc:op/app.bsky.graph.follow/1";
        let by = "at://did:plc:x/app.bsky.graph.follow/2";
        let r = |row| relation_from(&rels(row), d);
        assert_eq!(
            r(serde_json::json!({"did":d,"following":fol,"followedBy":by})),
            Relation::Mutual
        );
        assert_eq!(
            r(serde_json::json!({"did":d,"followedBy":by})),
            Relation::Follower
        );
        assert_eq!(
            r(serde_json::json!({"did":d,"following":fol})),
            Relation::Following
        );
        assert_eq!(r(serde_json::json!({"did":d})), Relation::Stranger);
        // A block either way, or a missing actor, is never a mutual.
        assert_eq!(
            r(serde_json::json!({"did":d,"following":fol,"followedBy":by,"blockedBy":by})),
            Relation::Stranger
        );
        assert_eq!(
            r(
                serde_json::json!({"did":d,"notFound":true,"$type":"app.bsky.graph.defs#notFoundActor"})
            ),
            Relation::Stranger
        );
        // An answer about someone else says nothing about this DID.
        assert_eq!(
            relation_from(
                &rels(serde_json::json!({"did":"did:plc:other","following":fol,"followedBy":by})),
                d
            ),
            Relation::Stranger
        );
    }

    /// A co-operator is answered from config, before any lookup — and
    /// with no AppView answering, everyone else still fails closed.
    #[tokio::test]
    async fn a_co_operator_resolves_without_the_network() {
        let r = IdentityResolver::new(
            crate::pds::PdsClient::offline(),
            "did:plc:op".into(),
            Duration::from_secs(60),
        )
        .with_co_operators(vec!["did:plc:tester".into()]);
        assert_eq!(r.relation("did:plc:op").await, Relation::Operator);
        assert_eq!(r.relation("did:plc:tester").await, Relation::CoOperator);
        assert_eq!(r.relation("did:plc:someone").await, Relation::Stranger);
        // The hint for a co-operator is the standing floor; its rule in
        // [acl] says the rest.
        assert_eq!(Relation::CoOperator.role_hint(), "user");
    }

    #[test]
    fn role_hints_map_only_operator_and_mutual_up() {
        assert_eq!(Relation::Operator.role_hint(), "owner");
        assert_eq!(Relation::Mutual.role_hint(), "user");
        // A one-way follower is NOT a mutual — guest floor.
        assert_eq!(Relation::Follower.role_hint(), "guest");
        assert_eq!(Relation::Following.role_hint(), "guest");
        assert_eq!(Relation::Stranger.role_hint(), "guest");
    }
}
