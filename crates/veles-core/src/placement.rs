//! Work placement (B4): which node runs a delegate child.
//!
//! The plan's rule (PLAN-compute-pool.md, "Placement"): sessions are
//! WHOLE-SESSION STICKY — a session lives where it was created
//! (`sessions.node`, B1.5, stamps it) — and the one scheduling decision
//! left is per-CHILD: `delegate_task backend = <node> | gpu | any`.
//!
//! Three spellings, one resolver:
//!
//! - `<id-or-name-or-prefix>` — THAT node, or an honest error. Never a
//!   silent fallback: asking for a node is a placement constraint
//!   (egress IP, GPU, filesystem locality), and quietly running the
//!   work somewhere else is the failure the constraint existed to
//!   prevent.
//! - `gpu` — any live node advertising a GPU, least-loaded first.
//!   "Any", not "the": with one GPU node today the two coincide, but
//!   the spelling describes the CONSTRAINT, not a machine.
//! - `any` — least-loaded across the pool INCLUDING this node. A pool
//!   of one is the degenerate but legal case: the local node wins by
//!   being the only candidate. Nodes whose load is unknown are ranked
//!   LAST but eligible (unknown is not idle, and not busy either); a
//!   STALE heartbeat makes a node ineligible — placing work on a node
//!   we have not heard from is not placement, it is hope.
//!
//! B4 deliberately does not add a scheduling layer for ordinary turns:
//! a turn runs where its session lives, and a session lives where it
//! was created. Sticky placement was decided 2026-09-20; what B4 adds
//! is the delegate knob the plan named, on the sticky foundation.

use crate::store::{NodeMatch, NodeRow};
use anyhow::Result;

/// What a `backend = …` argument asked for, parsed off the tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSpec {
    /// `local` — explicitly this node (distinct from None, which never
    /// mentioned placement at all).
    Local,
    /// A node id, name, or unique prefix — resolved against the
    /// registry at call time, never cached.
    Node(String),
    /// Any live GPU node, least-loaded first.
    Gpu,
    /// Least-loaded live node including this one.
    Any,
}

impl BackendSpec {
    /// Parse the tool argument. Empty string = not a placement ask
    /// (treated as absent by the caller); unknown words are ERRORS,
    /// not defaults — a typo'd `vps2` must not quietly mean "any".
    pub fn parse(s: &str) -> Result<Option<BackendSpec>> {
        let t = s.trim();
        if t.is_empty() {
            return Ok(None);
        }
        Ok(Some(match t.to_ascii_lowercase().as_str() {
            "local" => BackendSpec::Local,
            "gpu" => BackendSpec::Gpu,
            "any" => BackendSpec::Any,
            _ => BackendSpec::Node(t.to_string()),
        }))
    }
}

/// One resolved placement decision — the node that will run the child,
/// with enough context for the tool to say WHAT it did.
#[derive(Debug, Clone)]
pub struct Placement {
    pub node: NodeRow,
    /// Why this node: echoed in the tool result so the caller (model
    /// and, through it, the operator) can audit the decision.
    pub reason: String,
}

/// A node's GPU name, parsed out of the registry's raw JSON column.
fn gpu_name(row: &NodeRow) -> Option<String> {
    let raw = row.gpu.as_deref()?;
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    v.get("name")?.as_str().map(|s| s.to_string())
}

/// Resolve a spec against the live registry.
///
/// `stale_after` and `me` come from the caller (config + node.toml)
/// rather than being re-read here: this function is pure over its
/// inputs so tests can pin every branch without env games.
///
/// `routable` says whether a node can be DIALED from here (this node
/// always can; a remote one needs an `[a2a_agents]` entry). `gpu` and
/// `any` choose only among routable nodes: they are preferences, and
/// picking a node the work cannot reach turned a free pool into an
/// error. A NAMED node is a constraint, so it is resolved regardless
/// and an unreachable one stays an honest error at dial time.
pub fn resolve(
    store: &crate::store::Store,
    spec: &BackendSpec,
    me: Option<&str>,
    stale_after: i64,
    routable: &dyn Fn(&NodeRow) -> bool,
) -> Result<Placement> {
    resolve_needing(store, spec, me, stale_after, routable, &[])
}

/// The programs of `needs` that `node` does not advertise
/// (`NodeRow::caps_tools`); all of them for a node that advertises
/// nothing, since an unknown is not a yes.
fn missing<'a>(node: &NodeRow, needs: &'a [String]) -> Vec<&'a str> {
    let has = node.caps_tools().unwrap_or_default();
    needs
        .iter()
        .map(String::as_str)
        .filter(|n| !has.iter().any(|h| h == n))
        .collect()
}

/// [`resolve`] with `needs` (PLAN-phone-userland.md T2, `delegate_task
/// needs`): programs the node must ALL advertise. `gpu` and `any` choose
/// only among nodes that do; a named node (or `local`) that lacks one is
/// an error naming what it lacks, never a quiet move elsewhere.
pub fn resolve_needing(
    store: &crate::store::Store,
    spec: &BackendSpec,
    me: Option<&str>,
    stale_after: i64,
    routable: &dyn Fn(&NodeRow) -> bool,
    needs: &[String],
) -> Result<Placement> {
    let lacks = |what: &str, node: &NodeRow| -> Result<()> {
        let gone = missing(node, needs);
        if gone.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
            "{what} ({}) does not have {} — needs = [{}]; `veles nodes` lists what each node has",
            node.name,
            gone.join(", "),
            needs.join(", ")
        )
    };
    match spec {
        BackendSpec::Local => match me {
            Some(id) => match store.node_get(id)? {
                Some(node) => {
                    lacks("backend=local: this node", &node)?;
                    Ok(Placement {
                        reason: "explicitly local".into(),
                        node,
                    })
                }
                None => anyhow::bail!(
                    "backend=local: this node ({id}) is not in the registry — \
                     run `veles nodes probe` or start a gateway first"
                ),
            },
            None => anyhow::bail!(
                "backend=local: no node identity on this machine \
                 (no node.toml) — placement needs to know who 'local' is"
            ),
        },
        BackendSpec::Node(want) => {
            // Names are what operators actually type (`backend = "vps"`
            // beats `backend = "9f3a…"`); ids and unique id prefixes
            // remain the unambiguous spelling. Exact name match first,
            // then the id resolution — a name that happens to equal
            // another node's id prefix resolves the ID (ids are the
            // identity; names are display).
            let full = match store.node_resolve(want)? {
                NodeMatch::One(row) => *row,
                NodeMatch::None => {
                    let by_name = store.nodes_list()?.into_iter().find(|n| n.name == *want);
                    match by_name {
                        Some(row) => row,
                        None => anyhow::bail!(
                            "backend node '{want}': no such node — /nodes lists the registry"
                        ),
                    }
                }
                NodeMatch::Ambiguous(n) => {
                    anyhow::bail!("backend node '{want}': ambiguous prefix ({n} nodes match)")
                }
            };
            if crate::node::is_stale(&full.heartbeat_at, stale_after) {
                anyhow::bail!(
                    "backend node '{want}' ({}) is STALE — no heartbeat within {stale_after}s; \
                     refusing to place work on a node we have not heard from",
                    full.name
                );
            }
            lacks(&format!("backend node '{want}'"), &full)?;
            Ok(Placement {
                reason: format!("named: {want}"),
                node: full,
            })
        }
        BackendSpec::Gpu => {
            let live: Vec<NodeRow> = store
                .nodes_list()?
                .into_iter()
                .filter(|n| !crate::node::is_stale(&n.heartbeat_at, stale_after))
                .collect();
            let gpu_live: Vec<NodeRow> = live
                .iter()
                .filter(|n| gpu_name(n).is_some())
                .filter(|n| missing(n, needs).is_empty())
                .cloned()
                .collect();
            if gpu_live.is_empty() {
                let total = store.nodes_list()?.len();
                if !needs.is_empty() {
                    anyhow::bail!(
                        "backend=gpu: no live GPU node has all of needs = [{}] \
                         ({total} node(s) in the registry; stale ones excluded)",
                        needs.join(", ")
                    );
                }
                anyhow::bail!(
                    "backend=gpu: no live node advertises a GPU \
                     ({total} node(s) in the registry; stale ones excluded)"
                );
            }
            let mut gpus: Vec<NodeRow> = gpu_live.iter().filter(|n| routable(n)).cloned().collect();
            if gpus.is_empty() {
                anyhow::bail!(
                    "backend=gpu: {} live GPU node(s), none reachable from here — \
                     a remote node needs an [a2a_agents] entry with its url and token",
                    gpu_live.len()
                );
            }
            gpus.sort_by(load_order);
            let node = gpus.into_iter().next().expect("nonempty");
            Ok(Placement {
                reason: format!(
                    "gpu: {} ({})",
                    gpu_name(&node).unwrap_or_default(),
                    node.name
                ),
                node,
            })
        }
        BackendSpec::Any => {
            let all = store.nodes_list()?;
            if all.is_empty() {
                anyhow::bail!("backend=any: the registry is empty — nothing to place on");
            }
            let live: Vec<NodeRow> = all
                .into_iter()
                .filter(|n| !crate::node::is_stale(&n.heartbeat_at, stale_after))
                .collect();
            if live.is_empty() {
                anyhow::bail!(
                    "backend=any: every registry node is stale \
                     (no heartbeat within {stale_after}s)"
                );
            }
            let n_live = live.len();
            let live: Vec<NodeRow> = live
                .into_iter()
                .filter(|n| missing(n, needs).is_empty())
                .collect();
            if live.is_empty() {
                anyhow::bail!(
                    "backend=any: none of {n_live} live node(s) has all of needs = [{}] — \
                     `veles nodes` lists what each node has",
                    needs.join(", ")
                );
            }
            let mut live: Vec<NodeRow> = live.into_iter().filter(|n| routable(n)).collect();
            if live.is_empty() {
                anyhow::bail!(
                    "backend=any: {n_live} live node(s), none reachable from here — \
                     a remote node needs an [a2a_agents] entry with its url and token"
                );
            }
            // Least-loaded first; unknown load ranks last but stays
            // eligible — see the module doc for why unknown ≠ idle.
            live.sort_by(load_order);
            let node = live.into_iter().next().expect("nonempty");
            let load = node
                .load1
                .map(|l| l.to_string())
                .unwrap_or_else(|| "unknown".into());
            Ok(Placement {
                reason: format!("least-loaded: load {load}"),
                node,
            })
        }
    }
}

/// The comparator behind least-loaded: lower `load1` first, unknown
/// after every known value, ties broken by `sessions_served` (spread
/// work) and then by id (determinism — two equal candidates must not
/// flip between calls, or repeated placements would oscillate).
pub(crate) fn load_order(a: &NodeRow, b: &NodeRow) -> std::cmp::Ordering {
    use std::cmp::Ordering as O;
    let ka = a.load1.unwrap_or(i64::MAX);
    let kb = b.load1.unwrap_or(i64::MAX);
    ka.cmp(&kb)
        .then_with(|| a.sessions_served.cmp(&b.sessions_served))
        .then_with(|| a.id.cmp(&b.id))
        .then(O::Equal)
}

/// Does this placement land on THIS node? The caller runs the child
/// locally when it does (sticky: local execution is the degenerate
/// case of remote, not a separate code path with its own bugs).
pub fn is_local(p: &Placement, me: Option<&str>) -> bool {
    me == Some(p.node.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> crate::store::Store {
        let dir = tempfile::tempdir().unwrap();
        crate::store::Store::open(&dir.path().join("state.db")).unwrap()
    }

    fn reg(store: &crate::store::Store, id: &str, name: &str, gpu: Option<&str>) {
        store
            .node_register(id, name, "h", "0", "always-on", gpu, None, None, None)
            .unwrap();
    }

    /// The parse: keywords, free-form names, empty = absent, and the
    /// case-insensitivity of the keywords only (node NAMES keep their
    /// case — they are identifiers, not flags).
    #[test]
    fn spec_parse_keywords_names_and_case() {
        assert_eq!(BackendSpec::parse("").unwrap(), None);
        assert_eq!(BackendSpec::parse("  ").unwrap(), None);
        assert_eq!(
            BackendSpec::parse("local").unwrap(),
            Some(BackendSpec::Local)
        );
        assert_eq!(BackendSpec::parse("GPU").unwrap(), Some(BackendSpec::Gpu));
        assert_eq!(BackendSpec::parse("Any").unwrap(), Some(BackendSpec::Any));
        assert_eq!(
            BackendSpec::parse("vps").unwrap(),
            Some(BackendSpec::Node("vps".into()))
        );
        // Keywords are ASCII-case-insensitive; node NAMES keep their
        // case. A node named exactly "local"/"gpu"/"any" is therefore
        // unreachable through this argument — the registry id or a
        // prefix still reaches it, and nobody has ever named a machine
        // that way.
        assert_eq!(
            BackendSpec::parse("LOCAL").unwrap(),
            Some(BackendSpec::Local)
        );
    }

    /// The named-node resolution: by id, by name, by prefix, and the
    /// honest errors for none/ambiguous/stale.
    #[test]
    fn named_node_resolves_and_refuses() {
        let store = scratch();
        reg(&store, "aaaa-1", "vps", None);
        reg(&store, "aabb-2", "laptop", None);
        // by id
        let p = resolve(
            &store,
            &BackendSpec::Node("aaaa-1".into()),
            None,
            600,
            &|_| true,
        )
        .unwrap();
        assert_eq!(p.node.id, "aaaa-1");
        // by name
        let p = resolve(
            &store,
            &BackendSpec::Node("laptop".into()),
            None,
            600,
            &|_| true,
        )
        .unwrap();
        assert_eq!(p.node.id, "aabb-2");
        // ambiguous prefix
        let err =
            resolve(&store, &BackendSpec::Node("a".into()), None, 600, &|_| true).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err:#}");
        // unknown name
        let err = resolve(
            &store,
            &BackendSpec::Node("nope".into()),
            None,
            600,
            &|_| true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no such node"), "{err:#}");
        // named but STALE: refused too — naming a node does not make a
        // dead one a placement target ("the work will run there" is
        // the promise; a node we have not heard from cannot keep it).
        store
            .conn()
            .execute(
                "UPDATE nodes SET heartbeat_at = '2001-01-01 00:00:00' WHERE id = 'aaaa-1'",
                [],
            )
            .unwrap();
        let err = resolve(&store, &BackendSpec::Node("vps".into()), None, 600, &|_| {
            true
        })
        .unwrap_err();
        assert!(err.to_string().contains("STALE"), "{err:#}");
    }

    /// `gpu` picks the GPU node, and says WHICH card; no live GPU node
    /// is an honest error, not a fallback to CPU.
    #[test]
    fn gpu_picks_a_gpu_node_or_refuses() {
        let store = scratch();
        reg(&store, "g1", "laptop", Some(r#"{"name":"RTX 3080"}"#));
        reg(&store, "c1", "vps", None);
        let p = resolve(&store, &BackendSpec::Gpu, None, 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "g1");
        assert!(p.reason.contains("RTX 3080"), "{}", p.reason);

        let bare = scratch();
        reg(&bare, "c1", "vps", None);
        let err = resolve(&bare, &BackendSpec::Gpu, None, 600, &|_| true).unwrap_err();
        assert!(err.to_string().contains("no live node advertises a GPU"));
    }

    /// `any` is least-loaded INCLUDING this node, unknown ranks last,
    /// and a stale node is never placed on.
    #[test]
    fn any_is_least_loaded_with_unknown_last_and_stale_excluded() {
        let store = scratch();
        reg(&store, "busy", "a", None);
        reg(&store, "idle", "b", None);
        reg(&store, "quiet", "c", None);
        store.node_heartbeat("busy", Some(80)).unwrap();
        store.node_heartbeat("idle", Some(10)).unwrap();
        // quiet: no load row at all (unknown)
        let p = resolve(&store, &BackendSpec::Any, None, 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "idle");
        assert!(p.reason.contains("least-loaded"));

        // unknown ranks after every known value — even a known 100
        // beats an unknown (both `busy` and `idle` sit at 100; the tie
        // breaks to `busy` by id).
        store.node_heartbeat("idle", Some(100)).unwrap();
        store.node_heartbeat("busy", Some(100)).unwrap();
        let p = resolve(&store, &BackendSpec::Any, None, 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "busy", "unknown load ranks last, not first");

        // stale = ineligible. Make everyone stale except `idle`.
        let old = "2001-01-01 00:00:00";
        store
            .conn()
            .execute(
                "UPDATE nodes SET heartbeat_at = ?1 WHERE id != 'idle'",
                rusqlite::params![old],
            )
            .unwrap();
        let p = resolve(&store, &BackendSpec::Any, None, 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "idle");
    }

    /// A single-node registry is a legal `any` (the degenerate pool),
    /// and `local` needs an identity plus a registry row.
    #[test]
    fn single_node_any_and_local() {
        let store = scratch();
        reg(&store, "me-1", "solo", None);
        let p = resolve(&store, &BackendSpec::Any, Some("me-1"), 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "me-1");
        assert!(is_local(&p, Some("me-1")));

        let err = resolve(&store, &BackendSpec::Local, None, 600, &|_| true).unwrap_err();
        assert!(err.to_string().contains("no node identity"));

        let err = resolve(&store, &BackendSpec::Local, Some("ghost"), 600, &|_| true).unwrap_err();
        assert!(err.to_string().contains("not in the registry"));
    }

    /// `any` and `gpu` are preferences: they choose among nodes the
    /// work can REACH. B4 ranked every live node, so an idle peer with
    /// no dialing route won `any` and the call failed while this node
    /// sat free. A named node is a constraint and still resolves.
    #[test]
    fn any_and_gpu_choose_only_reachable_nodes() {
        let store = scratch();
        reg(&store, "me-1", "laptop", None);
        reg(&store, "far-2", "vps", Some(r#"{"name":"A100"}"#));
        store.node_heartbeat("me-1", Some(90)).unwrap();
        store.node_heartbeat("far-2", Some(5)).unwrap();
        let only_me = |n: &NodeRow| n.id == "me-1";
        // The idle peer is unreachable: `any` lands here, not there.
        let p = resolve(&store, &BackendSpec::Any, Some("me-1"), 600, &only_me).unwrap();
        assert_eq!(p.node.id, "me-1");
        // The only GPU is unreachable: an honest error that says so.
        let err = resolve(&store, &BackendSpec::Gpu, Some("me-1"), 600, &only_me).unwrap_err();
        assert!(err.to_string().contains("none reachable"), "{err:#}");
        // Named: still resolved; the dial reports the missing route.
        let p = resolve(
            &store,
            &BackendSpec::Node("vps".into()),
            Some("me-1"),
            600,
            &only_me,
        )
        .unwrap();
        assert_eq!(p.node.id, "far-2");
    }

    /// The comparator's determinism: equal load + equal served breaks
    /// by id, so repeated resolutions cannot oscillate.
    #[test]
    fn load_order_is_deterministic_on_ties() {
        let store = scratch();
        reg(&store, "zz", "x", None);
        reg(&store, "aa", "y", None);
        store.node_heartbeat("zz", Some(42)).unwrap();
        store.node_heartbeat("aa", Some(42)).unwrap();
        let p = resolve(&store, &BackendSpec::Any, None, 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "aa", "id breaks the tie, deterministically");
    }

    fn reg_with_tools(
        store: &crate::store::Store,
        id: &str,
        name: &str,
        gpu: Option<&str>,
        tools: &[&str],
        load: u8,
    ) {
        let caps = serde_json::json!({ "os": "linux", "tools": tools }).to_string();
        store
            .node_register(
                id,
                name,
                "h",
                "0",
                "always-on",
                gpu,
                None,
                None,
                Some(&caps),
            )
            .unwrap();
        store.node_heartbeat(id, Some(load)).unwrap();
    }

    fn needs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// `any` with needs skips the least loaded node when it lacks one
    /// of them, and keeps every node when needs is empty.
    #[test]
    fn any_with_needs_skips_a_node_that_lacks_one() {
        let store = scratch();
        reg_with_tools(&store, "phone-1", "phone", None, &["bash"], 0);
        reg_with_tools(
            &store,
            "lap-1",
            "laptop",
            None,
            &["bash", "rg", "podman"],
            50,
        );
        let all = |_: &NodeRow| true;
        let p =
            resolve_needing(&store, &BackendSpec::Any, None, 600, &all, &needs(&["rg"])).unwrap();
        assert_eq!(p.node.id, "lap-1");
        let p = resolve_needing(&store, &BackendSpec::Any, None, 600, &all, &[]).unwrap();
        assert_eq!(p.node.id, "phone-1", "no needs: least loaded wins");
        let err = resolve_needing(
            &store,
            &BackendSpec::Any,
            None,
            600,
            &all,
            &needs(&["agent-browser"]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("agent-browser"), "{err}");
    }

    /// `gpu` with needs: only GPU nodes that have them all.
    #[test]
    fn gpu_with_needs_filters_by_tools() {
        let store = scratch();
        let card = Some(r#"{"name":"RTX"}"#);
        reg_with_tools(&store, "g1", "rig", card, &["bash"], 0);
        reg_with_tools(&store, "g2", "laptop", card, &["bash", "python3"], 70);
        let all = |_: &NodeRow| true;
        let p = resolve_needing(
            &store,
            &BackendSpec::Gpu,
            None,
            600,
            &all,
            &needs(&["python3"]),
        )
        .unwrap();
        assert_eq!(p.node.id, "g2");
        let err = resolve_needing(&store, &BackendSpec::Gpu, None, 600, &all, &needs(&["jq"]))
            .unwrap_err();
        assert!(err.to_string().contains("jq"), "{err}");
    }

    /// A named node lacking a needed program is an error naming it —
    /// never a quiet move to a node that has it. One that advertises
    /// nothing (an older build) cannot confirm and is refused too.
    #[test]
    fn a_named_node_lacking_a_need_is_an_error_naming_it() {
        let store = scratch();
        reg_with_tools(&store, "phone-1", "phone", None, &["bash"], 0);
        reg_with_tools(&store, "lap-1", "laptop", None, &["bash", "rg"], 50);
        reg(&store, "old-1", "old", None);
        let all = |_: &NodeRow| true;
        let named = |n: &str| BackendSpec::Node(n.into());
        let err = resolve_needing(
            &store,
            &named("phone"),
            None,
            600,
            &all,
            &needs(&["bash", "rg"]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("phone") && err.contains("does not have rg"),
            "{err}"
        );
        assert!(!err.contains("have bash"), "only what is missing: {err}");
        let p =
            resolve_needing(&store, &named("laptop"), None, 600, &all, &needs(&["rg"])).unwrap();
        assert_eq!(p.node.id, "lap-1");
        assert!(resolve_needing(&store, &named("old"), None, 600, &all, &needs(&["rg"])).is_err());
        assert!(resolve_needing(&store, &named("old"), None, 600, &all, &[]).is_ok());
        let err = resolve_needing(
            &store,
            &BackendSpec::Local,
            Some("phone-1"),
            600,
            &all,
            &needs(&["rg"]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("rg"), "{err}");
    }
}
