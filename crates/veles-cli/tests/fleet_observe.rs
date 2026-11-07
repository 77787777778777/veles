//! The command center's per-node observation between two REAL gateways
//! (owner ask 287976.1, docs/command-center.md "Observing nodes"): two
//! `veles gateway run` processes on their own homes and loopback ports,
//! each the other's `[a2a_agents]` peer, gossip and memsync peer, with
//! real bearer tokens. Node A's desktop API reads node B's log tail and
//! metrics over the A2A link, compares the two for drift, gossips on
//! demand, and forgets a retired node fleet-wide — and B refuses the
//! same request from a peer outside its `[cluster] memsync_peers`.
//!
//! Hermetic: scratch homes under the per-run TMPDIR, loopback ports, a
//! dead provider URL (no turn is ever run).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A value B holds as a declared secret, and logs at boot (as a home
/// chat id): it must reach A as `NAME=<REDACTED>`.
const SECRET: &str = "fleet-e2e-secret-4b1d9c";
/// A credential SHAPE nobody registered, also logged by B at boot.
const SHAPED: &str = "sk-ant-e2eFleetObserveShapeOnly0123456789";

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[allow(clippy::too_many_arguments)]
fn node_home(
    root: &Path,
    name: &str,
    peer: &str,
    peer_port: u16,
    my_token: &str,
    peer_tokens: &str,
    extra: &str,
) -> PathBuf {
    let d = root.join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("config.toml"),
        format!(
            "[model]\nprovider = \"stub\"\ndefault = \"stub-model\"\n\n\
             [[custom_providers]]\nname = \"stub\"\nbase_url = \"http://127.0.0.1:9/v1\"\n\
             api_mode = \"chat_completions\"\napi_key = \"none\"\nmodel = \"stub-model\"\n\n\
             [node]\nname = \"{name}\"\n\n\
             [platforms.a2a]\nenabled = true\n\n{extra}\n\
             [a2a_agents.{peer}]\nurl = \"http://127.0.0.1:{peer_port}\"\n\
             auth = {{ type = \"bearer\", token = \"{my_token}\" }}\n\n\
             [cluster]\ngossip_interval_secs = 5\nstale_after_secs = 300\n\
             memsync_peers = [\"{peer}\"]\ngossip_peers = [\"{peer}\"]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        d.join(".env"),
        format!("A2A_PEER_TOKENS={peer_tokens}\nA2A_BEARER_TOKEN={my_token}\n"),
    )
    .unwrap();
    d
}

fn veles(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_veles"));
    c.env("VELES_HOME", home)
        .env("HOME", home)
        .env_remove("VELES_SESSION_PRINCIPAL")
        // An operator's ambient A2A settings must not shadow this home's
        // own `.env` (session_mail_net.rs says why).
        .env_remove("A2A_HOST")
        .env_remove("A2A_PEER_TOKENS")
        .env_remove("A2A_BEARER_TOKEN")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null());
    c
}

fn gateway(home: &Path, a2a_port: u16, desk_port: u16) -> Proc {
    Proc(
        veles(home)
            .args(["gateway", "run"])
            .env("A2A_PORT", a2a_port.to_string())
            .env("VELES_DESKTOP_PORT", desk_port.to_string())
            .env("VELES_DESKTOP_OPEN", "1")
            .stdout(std::fs::File::create(home.join("gw.log")).unwrap())
            .stderr(std::fs::File::create(home.join("gw.err")).unwrap())
            .spawn()
            .expect("spawn a gateway"),
    )
}

fn http(port: u16, method: &str, path: &str, body: &str, auth: Option<&str>) -> (u16, String) {
    let Ok(mut s) = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(5),
    ) else {
        return (0, String::new());
    };
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let auth = auth
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         {auth}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(req.as_bytes());
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (
        status,
        out.split("\r\n\r\n").nth(1).unwrap_or("").to_string(),
    )
}

fn json(port: u16, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    let (code, text) = http(port, method, path, body, None);
    (code, serde_json::from_str(&text).unwrap_or_default())
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

fn tail(p: &Path) -> String {
    let s = std::fs::read_to_string(p).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(30)..].join("\n")
}

fn self_id(desk: u16) -> Option<String> {
    let (_, v) = json(desk, "GET", "/api/fleet", "");
    v["nodes"]
        .as_array()?
        .iter()
        .find(|n| n["self"] == true)
        .and_then(|n| n["id"].as_str().map(str::to_string))
}

fn lists(desk: u16, id: &str) -> bool {
    let (_, v) = json(desk, "GET", "/api/fleet", "");
    v["nodes"]
        .as_array()
        .is_some_and(|ns| ns.iter().any(|n| n["id"] == id))
}

/// A retired node both registries list, with a heartbeat from long ago.
fn seed_ghost(home: &Path) {
    let store = veles_core::store::Store::open(&home.join("state.db")).unwrap();
    let row: veles_core::store::GossipNodeRow = serde_json::from_value(serde_json::json!({
        "id": "ghost-node-0001", "name": "ghost", "host": "gone", "version": "0.1.6",
        "uptime_class": "ephemeral",
        "registered_at": "2026-10-01 00:00:00",
        "heartbeat_at": "2026-10-02 00:00:00",
    }))
    .unwrap();
    store.node_merge_from_gossip(&row).unwrap();
}

#[test]
fn a_node_reads_its_peers_log_and_metrics_compares_and_forgets_fleet_wide() {
    let root = std::env::temp_dir().join(format!(
        "veles-fleet-observe-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (a2a_a, a2a_b) = (free_port(), free_port());
    let (desk_a, desk_b) = (free_port(), free_port());
    let home_a = node_home(&root, "a", "b", a2a_b, "tok-a", "b:tok-b", "");
    // B logs both values at boot ("home chat: <platform>:<chat>"), knows
    // the secret as a declared one, and has a third token whose peer is
    // NOT a cluster peer.
    let home_b = node_home(
        &root,
        "b",
        "a",
        a2a_a,
        "tok-b",
        "a:tok-a,c:tok-c",
        &format!(
            "[platforms.e2esecret.home_channel]\nchat_id = \"{SECRET}\"\n\
             [platforms.e2eshape.home_channel]\nchat_id = \"{SHAPED}\"\n"
        ),
    );
    std::fs::write(
        home_b.join("secrets.env"),
        format!("FLEET_E2E_SECRET={SECRET}\n"),
    )
    .unwrap();
    seed_ghost(&home_a);
    seed_ghost(&home_b);
    let _ga = gateway(&home_a, a2a_a, desk_a);
    let _gb = gateway(&home_b, a2a_b, desk_b);
    let logs = || {
        format!(
            "--- a\n{}\n--- b\n{}",
            tail(&home_a.join("gw.log")),
            tail(&home_b.join("gw.log"))
        )
    };
    for port in [desk_a, desk_b] {
        wait_until("a gateway's health route", 90, || {
            http(port, "GET", "/api/health", "", None).0 == 200
        });
    }
    for port in [a2a_a, a2a_b] {
        wait_until("an A2A server", 60, || {
            http(port, "GET", "/.well-known/agent-card.json", "", None).0 == 200
        });
    }
    let mut b_id = String::new();
    wait_until("B's own registry row", 60, || {
        b_id = self_id(desk_b).unwrap_or_default();
        !b_id.is_empty()
    });
    wait_until("A to learn B by gossip", 60, || lists(desk_a, &b_id));

    // Metrics: B's build, machine and work, through A.
    let (code, m) = json(
        desk_a,
        "POST",
        "/api/fleet/metrics",
        &format!(r#"{{"node":"{b_id}"}}"#),
    );
    assert_eq!(code, 200, "{m}\n{}", logs());
    assert_eq!(m["node"]["id"], b_id.as_str(), "{m}");
    assert_eq!(m["via"], "b", "dialed through A's [a2a_agents] entry: {m}");
    assert_eq!(m["version"], env!("CARGO_PKG_VERSION"));
    assert!(m["revision"].is_string(), "{m}");
    assert!(m["cpu"]["cores"].as_u64().unwrap_or(0) >= 1, "{m}");
    assert!(m["in_flight"]["turns_total"].is_number(), "{m}");

    // Logs: B's ring, redacted ON B — the declared secret by name, the
    // unregistered credential shape by its shape.
    let (code, l) = json(
        desk_a,
        "POST",
        "/api/fleet/logs",
        &format!(r#"{{"node":"{b_id}","lines":500}}"#),
    );
    assert_eq!(code, 200, "{l}\n{}", logs());
    let text = l["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["msg"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.is_empty(), "{l}");
    assert!(!text.contains(SECRET), "a declared secret left B:\n{text}");
    assert!(
        !text.contains("e2eFleetObserveShapeOnly"),
        "a credential shape left B:\n{text}"
    );
    assert!(
        text.contains("FLEET_E2E_SECRET=<REDACTED>"),
        "B's boot line was in its ring, redacted by name:\n{text}"
    );
    assert!(l["lines"].as_array().unwrap().len() <= 500);

    // Drift: one compared node, the same build.
    let (code, d) = json(desk_a, "POST", "/api/fleet/drift", "{}");
    assert_eq!(code, 200, "{d}");
    let row = d["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == b_id.as_str())
        .unwrap_or_else(|| panic!("B in the drift report: {d}"))
        .clone();
    assert_eq!(row["asked"], true, "{row}");
    assert!(row["error"].is_null(), "{row}\n{}", logs());
    assert_eq!(row["version_differs"], false, "{row}");
    assert_eq!(row["revision_differs"], false, "{row}");
    assert_eq!(row["syncs_config"], true);
    assert!(row["config"]["keys"]["differ"].is_array(), "{row}");

    // B answers only its cluster peers: peer "c" authenticates but is not
    // in B's memsync_peers; no bearer at all is not even a peer.
    let ask = r#"{"jsonrpc":"2.0","id":1,"method":"veles/fleet","params":{"op":"logs"}}"#;
    let (code, body) = http(a2a_b, "POST", "/", ask, Some("tok-c"));
    assert_eq!(code, 200, "{body}");
    assert!(
        body.contains("-32052") && body.contains("memsync_peers"),
        "{body}"
    );
    assert!(!body.contains(SECRET));
    let (code, _) = http(a2a_b, "POST", "/", ask, None);
    assert_eq!(code, 401);

    // Gossip now, from A.
    let (code, g) = json(desk_a, "POST", "/api/fleet/gossip", "{}");
    assert_eq!(code, 200, "{g}");
    assert_eq!(g["results"][0]["peer"], "b");
    assert_eq!(g["results"][0]["ok"], true, "{g}\n{}", logs());

    // Fleet-wide forget: A forgets the ghost, one exchange takes it off
    // B, and B's next gossip back does not return it to A.
    assert!(lists(desk_a, "ghost-node-0001") && lists(desk_b, "ghost-node-0001"));
    let (code, f) = json(
        desk_a,
        "POST",
        "/api/fleet/forget",
        r#"{"node":"ghost-node-0001","confirm":true}"#,
    );
    assert_eq!(code, 200, "{f}");
    assert_eq!(f["fleet_wide"], true);
    let (code, g) = json(desk_a, "POST", "/api/fleet/gossip", "{}");
    assert_eq!(code, 200, "{g}");
    wait_until("B to drop the forgotten node", 30, || {
        !lists(desk_b, "ghost-node-0001")
    });
    // B gossips to A on its own cadence (5 s): two rounds later A still
    // does not list it, and both say it is forgotten.
    std::thread::sleep(Duration::from_secs(11));
    assert!(!lists(desk_a, "ghost-node-0001"), "{}", logs());
    for desk in [desk_a, desk_b] {
        let (_, v) = json(desk, "GET", "/api/fleet", "");
        assert!(
            v["forgotten"]
                .as_array()
                .is_some_and(|t| t.iter().any(|t| t["id"] == "ghost-node-0001")),
            "{}",
            v["forgotten"]
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
