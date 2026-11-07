//! Read-only compute inventory for placement decisions.
//!
//! The existing `delegate_task.backend` selector is only useful when the
//! agent can discover actual node names, capabilities and current load.
//! This exposes the registry, never a node's secrets or credentials.
//!
//! 306787.b: it reports what `backend='any'` ranks by — cores, load per
//! core, disk IO pressure, the delegate children counted on each node
//! (their own reports plus this node's placements they have not
//! reported yet) — and whether a live auth probe let this node in, so
//! the model can see where a split job would go before splitting it.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use crate::lockutil::LockOk;
use crate::placement::Workload;
use crate::store::Store;
use crate::tools::Tool;

pub struct NodesTool {
    pub store: Arc<Mutex<Store>>,
}

#[async_trait]
impl Tool for NodesTool {
    fn name(&self) -> &str {
        "nodes"
    }

    fn description(&self) -> &str {
        "List compute-pool nodes before dividing work: each node's name, id, freshness, \
         cores, load per core, disk IO pressure, delegate children counted there, \
         capabilities, and whether this node passed a live auth probe to it, with the \
         order backend='any' would pick for mixed, cpu and io work. Splitting is your \
         decision: a job with independent parts of minutes each (builds, tests, \
         encodes, scans) goes out as one delegate_task 'tasks' batch with \
         backend='any' and workload 'cpu' or 'io'; short or same-file work stays \
         here. Read-only: not a deployment or service-restart tool."
    }

    fn schema(&self) -> Value {
        json!({"type":"object","properties":{}})
    }

    async fn call(&self, _args: Value) -> String {
        // Probe expired routes first, so `routable` is a live answer.
        crate::routeprobe::refresh_pool().await;
        let cfg = crate::config::load().unwrap_or_default();
        let me = crate::node::NodeIdentity::load(&crate::config::config_dir())
            .ok()
            .flatten()
            .map(|n| n.id);
        let store = self.store.lock_ok();
        let rows = match store.nodes_list() {
            Ok(rows) => rows,
            Err(e) => return format!("error reading compute inventory: {e}"),
        };
        let stale_after = cfg.cluster.stale_after_secs as i64;
        let _ = crate::placement::prune(store.conn(), &crate::placement::pid_lives);
        // The order `any` would pick, per workload, among the live nodes
        // it could reach (the same filter delegate_task applies).
        let eligible: Vec<_> = rows
            .iter()
            .filter(|n| !crate::node::is_stale(&n.heartbeat_at, stale_after))
            .filter(|n| crate::routeprobe::route(n, me.as_deref()).routable())
            .cloned()
            .collect();
        let order = |w: Workload| -> Vec<String> {
            crate::placement::rank(store.conn(), me.as_deref(), eligible.clone(), w)
                .into_iter()
                .map(|(n, _)| n.name)
                .collect()
        };
        let mut out = Vec::with_capacity(rows.len());
        for n in &rows {
            let stale = crate::node::is_stale(&n.heartbeat_at, stale_after);
            let this_node = me.as_deref() == Some(n.id.as_str());
            // Only whether a route exists and how its probe went is
            // reported, never the url or token.
            let route = crate::routeprobe::route(n, me.as_deref());
            let routable = route.routable();
            let placement = if stale {
                "unavailable (stale heartbeat)"
            } else if !routable {
                "not reachable from here (see route)"
            } else {
                "eligible for backend='any'/'gpu' (subject to needs)"
            };
            let l = crate::placement::node_load(store.conn(), me.as_deref(), n);
            out.push(json!({
                "name": n.name,
                "id": n.id,
                "this_node": this_node,
                "stale": stale,
                "routable": routable,
                "route": route.describe(),
                "cores": l.cores,
                "load_per_core_percent": l.load_percent,
                "load_observed_at": n.load_at,
                "io_pressure_percent": match (l.io_some, l.io_full) {
                    (None, None) => Value::String("unknown".into()),
                    (some, full) => json!({"some_avg10": some, "full_avg10": full}),
                },
                "io_high": l.io_high(),
                "children_running": l.running,
                "children_queued": l.queued,
                "placed_from_here_unreported": l.unreported_from_here,
                "children_counted": l.children(),
                "projected_load_per_core_percent": l.projected_x100().map(|p| p / 100),
                "capabilities": n.caps_tools().unwrap_or_default(),
                "gpu": n.gpu.as_deref().and_then(|g| serde_json::from_str::<Value>(g).ok()),
                "placement": placement,
            }));
        }
        serde_json::to_string_pretty(&json!({
            "nodes": out,
            "any_order": {
                "mixed": order(Workload::General),
                "cpu": order(Workload::Cpu),
                "io": order(Workload::Io),
            },
            "default_backend": cfg.cluster.delegate_backend,
            "note": format!(
                "backend='any' ranks the reachable live nodes by load per core plus one \
                 core's worth (100/cores points) per delegate child counted there — the ones \
                 running or queued by the node's own report, plus ones this node placed that \
                 it has not reported — plus its IO pressure (PSI some avg10; workload='cpu' \
                 halves it, workload='io' doubles it and avoids nodes at or over {}%). \
                 Unknown load ranks last; unknown cores count as {}; unknown IO adds nothing \
                 but ranks after measured low IO for io work. A named node never silently \
                 falls back. Splitting a job into parts is your decision: only independent \
                 parts of minutes each are worth a child.",
                crate::placement::IO_HIGH,
                crate::placement::DEFAULT_CORES,
            )
        }))
        .unwrap_or_else(|e| format!("error encoding compute inventory: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inventory_is_read_only_and_explains_placement() {
        let temp = tempfile::tempdir().unwrap();
        let tool = NodesTool {
            store: Arc::new(Mutex::new(
                Store::open(&temp.path().join("state.db")).unwrap(),
            )),
        };
        assert_eq!(tool.name(), "nodes");
        assert!(tool.description().contains("load per core"));
        assert!(tool.description().contains("IO pressure"));
        assert!(tool.description().contains("Splitting is your decision"));
        assert_eq!(tool.schema()["type"], "object");
        assert!(tool.schema()["properties"].as_object().unwrap().is_empty());
    }

    /// SI-8 and 306787.b: what `call` actually reports. A stale node is
    /// unavailable; a fresh one whose auth probe passes is routable and
    /// eligible, with its tools, GPU, cores, load per core, IO pressure
    /// and the children counted there; a fresh one without an entry, or
    /// whose probe was refused, is NOT called eligible; a child placed
    /// from here and not yet in the node's report raises the projection;
    /// and nothing secret (the peer's url, port or token) appears.
    #[tokio::test]
    async fn the_inventory_reports_staleness_routes_load_and_no_secrets() {
        let (home, _env) = crate::testenv::scratch_home();
        let good = crate::routeprobe::tests::fake_peer("gpubox", Some("tok-SHOULD-NOT-LEAK")).await;
        let refused = crate::routeprobe::tests::fake_peer("rotated", Some("the-new-one")).await;
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                "[a2a_agents.gpubox]\nurl = \"{good}\"\n\
                 [a2a_agents.gpubox.auth]\ntype = \"bearer\"\ntoken = \"tok-SHOULD-NOT-LEAK\"\n\
                 [a2a_agents.rotated]\nurl = \"{refused}\"\n\
                 [a2a_agents.rotated.auth]\ntype = \"bearer\"\ntoken = \"an-old-token\"\n"
            ),
        )
        .unwrap();
        let db = crate::config::veles_home().join("state.db");
        let store = Store::open(&db).unwrap();
        let caps = json!({"os": "linux", "tools": ["ffmpeg", "cargo"]}).to_string();
        store
            .node_register(
                "n-gpu",
                "gpubox",
                "h",
                "0",
                "always-on",
                Some(r#"{"name":"RTX 4090"}"#),
                None,
                None,
                Some(&caps),
            )
            .unwrap();
        store
            .node_heartbeat_sample(
                "n-gpu",
                &crate::store::HeartbeatSample {
                    load1: Some(30),
                    cores: Some(8),
                    io: Some(crate::node::IoPressure {
                        some: Some(25),
                        full: Some(4),
                    }),
                    work: Some(crate::placement::WorkReport {
                        running: 2,
                        queued: 1,
                        from: [("someone-else".to_string(), 2)].into(),
                    }),
                },
            )
            .unwrap();
        for (id, name) in [
            ("n-old", "oldbox"),
            ("n-noroute", "lonely"),
            ("n-rot", "rotated"),
        ] {
            store
                .node_register(id, name, "h", "0", "always-on", None, None, None, None)
                .unwrap();
        }
        store.node_heartbeat("n-noroute", Some(5)).unwrap();
        store.node_heartbeat("n-rot", Some(1)).unwrap();
        store
            .conn()
            .execute(
                "UPDATE nodes SET heartbeat_at = '2001-01-01 00:00:00' WHERE id = 'n-old'",
                [],
            )
            .unwrap();
        let tool = NodesTool {
            store: Arc::new(Mutex::new(store)),
        };
        let held = crate::placement::claim_in(&db, "n-gpu", "");
        let raw = tool.call(json!({})).await;
        drop(held);
        let v: Value = serde_json::from_str(&raw).unwrap_or_else(|_| panic!("{raw}"));
        let node = |id: &str| {
            v["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["id"] == id)
                .cloned()
                .unwrap_or_else(|| panic!("{id} in {raw}"))
        };
        let gpu = node("n-gpu");
        assert_eq!(gpu["name"], "gpubox");
        assert_eq!(gpu["stale"], false);
        assert_eq!(gpu["routable"], true, "{gpu}");
        assert!(gpu["route"]
            .as_str()
            .unwrap()
            .starts_with("auth probe passed"));
        assert_eq!(gpu["cores"], 8);
        assert_eq!(gpu["load_per_core_percent"], 30);
        assert_eq!(gpu["io_pressure_percent"]["some_avg10"], 25);
        assert_eq!(gpu["io_pressure_percent"]["full_avg10"], 4);
        assert_eq!(gpu["io_high"], true);
        assert_eq!(gpu["children_running"], 2);
        assert_eq!(gpu["children_queued"], 1);
        assert_eq!(gpu["placed_from_here_unreported"], 1);
        assert_eq!(gpu["children_counted"], 4);
        // 30 + 4 children × 100/8 = 80.
        assert_eq!(gpu["projected_load_per_core_percent"], 80);
        assert_eq!(gpu["capabilities"], json!(["ffmpeg", "cargo"]));
        assert_eq!(gpu["gpu"]["name"], "RTX 4090");
        assert!(gpu["placement"].as_str().unwrap().starts_with("eligible"));
        assert_eq!(v["any_order"]["mixed"], json!(["gpubox"]));
        let old = node("n-old");
        assert_eq!(old["stale"], true);
        assert!(old["placement"]
            .as_str()
            .unwrap()
            .starts_with("unavailable"));
        let lonely = node("n-noroute");
        assert_eq!(lonely["routable"], false);
        assert_eq!(lonely["io_pressure_percent"], "unknown");
        assert_eq!(
            lonely["cores"],
            Value::Null,
            "an older node: unknown, not 0"
        );
        assert!(
            lonely["placement"]
                .as_str()
                .unwrap()
                .starts_with("not reachable"),
            "a node delegate cannot dial is not called eligible: {lonely}"
        );
        let rot = node("n-rot");
        assert_eq!(rot["routable"], false, "a refused token is no route: {rot}");
        assert!(rot["route"].as_str().unwrap().contains("401"), "{rot}");
        let port = good.rsplit(':').next().unwrap().trim_end_matches("/a2a");
        for secret in ["tok-SHOULD-NOT-LEAK", "an-old-token", "/a2a", port] {
            assert!(!raw.contains(secret), "{secret} leaked: {raw}");
        }
    }
}
