//! Live route probes for `backend='any'`/`'gpu'` (owner ask 306787.b).
//!
//! `any` and `gpu` choose among the nodes this one can REACH. That used
//! to mean "has an `[a2a_agents]` entry", which says nothing about
//! whether the url answers or the token is still the one the peer
//! accepts: a phone that stopped five minutes after its last use
//! (`--exit-when-idle`) keeps a fresh-looking heartbeat for up to
//! `stale_after_secs`, and a rotated token is refused only when the
//! child is already on the wire. Either way the placement picked a node
//! the work could not reach, and the call failed while another node sat
//! free.
//!
//! A probe is one authenticated JSON-RPC `tasks/get` for a task id that
//! cannot exist: every veles peer (0.1.6 included, no new wire) answers
//! it with HTTP 200 and "task not found" once the bearer and the
//! trusted-peer list let us in, 401/403 when they do not, and nothing
//! when the node is down. Results are cached per (url, token) —
//! [`OK_TTL`] for a pass, [`FAIL_TTL`] for a failure so a node that
//! comes back is retried soon — and a successful gossip exchange, which
//! crosses the same gates, counts as a pass. Sync traffic is not "use"
//! for an idle phone (`idle.rs`), so a probe never keeps one awake.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::lockutil::LockOk;

/// How long a passed probe stands.
pub const OK_TTL: Duration = Duration::from_secs(300);
/// How long a failed probe stands before the node is tried again.
pub const FAIL_TTL: Duration = Duration::from_secs(60);
/// The whole probe's budget (connect included).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// What one probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// The peer let us in (its answer to the probe, whatever it was).
    Ok,
    /// It answered and refused us: the token, the trusted list, or its
    /// rate limit (a child sent now would be refused the same way).
    Refused(String),
    /// No answer within [`PROBE_TIMEOUT`].
    Unreachable(String),
}

impl Probe {
    pub fn is_ok(&self) -> bool {
        matches!(self, Probe::Ok)
    }

    fn ttl(&self) -> Duration {
        if self.is_ok() {
            OK_TTL
        } else {
            FAIL_TTL
        }
    }
}

struct Entry {
    probe: Probe,
    at: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, Entry>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, Entry>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The cache key: the url as dialed and a digest of the token (a rotated
/// token is a different route; the token itself is never kept here).
fn key(url: &str, bearer: Option<&str>) -> String {
    let digest = crate::memory::MemoryStore::content_sha(bearer.unwrap_or(""));
    format!(
        "{}#{}",
        url.trim_end_matches('/'),
        &digest[..digest.len().min(16)]
    )
}

/// Record a probe result for a route, as of `at`.
pub fn record_at(url: &str, bearer: Option<&str>, probe: Probe, at: Instant) {
    cache()
        .lock_ok()
        .insert(key(url, bearer), Entry { probe, at });
}

/// Record a probe result for a route, as of now.
pub fn record(url: &str, bearer: Option<&str>, probe: Probe) {
    record_at(url, bearer, probe, Instant::now());
}

/// The standing result for a route and its age, or `None` when there
/// is none or it has expired.
pub fn cached(url: &str, bearer: Option<&str>) -> Option<(Probe, Duration)> {
    let now = Instant::now();
    let m = cache().lock_ok();
    let e = m.get(&key(url, bearer))?;
    let age = now.saturating_duration_since(e.at);
    (age < e.probe.ttl()).then(|| (e.probe.clone(), age))
}

/// One client for every probe in the process: building one loads the TLS
/// root store, which costs more than the probe itself. No idle pool —
/// probes are minutes apart, and a pooled connection is tied to the
/// runtime that opened it.
fn client() -> Option<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(PROBE_TIMEOUT)
                .connect_timeout(PROBE_TIMEOUT)
                .pool_max_idle_per_host(0)
                .build()
                .ok()
        })
        .clone()
}

/// Probe one route now (not cached; see [`refresh`]).
pub async fn probe(url: &str, bearer: Option<&str>) -> Probe {
    let Some(client) = client() else {
        return Probe::Unreachable("no HTTP client could be built".into());
    };
    let rpc = serde_json::json!({
        "jsonrpc": "2.0",
        "id": uuid::Uuid::new_v4().to_string(),
        "method": "tasks/get",
        "params": {"id": format!("veles-route-probe-{}", uuid::Uuid::new_v4())},
    });
    let mut req = client
        .post(url.trim_end_matches('/'))
        .header("Content-Type", "application/json")
        .json(&rpc);
    if let Some(tok) = bearer {
        req = req.header("Authorization", format!("Bearer {tok}"));
    }
    match req.send().await {
        Ok(r) => match r.status().as_u16() {
            200 => Probe::Ok,
            401 => Probe::Refused("the peer refused this node's token (HTTP 401)".into()),
            403 => Probe::Refused("this node is not in the peer's trusted list (HTTP 403)".into()),
            // After the token check, before the trusted list: a child sent
            // now would get the same answer, so not a route for a minute.
            429 => Probe::Refused("the peer is rate-limiting this node (HTTP 429)".into()),
            s => Probe::Refused(format!("the peer answered HTTP {s}")),
        },
        Err(e) if e.is_timeout() => {
            Probe::Unreachable(format!("no answer within {}s", PROBE_TIMEOUT.as_secs()))
        }
        Err(e) if e.is_connect() => Probe::Unreachable("connection refused or unreachable".into()),
        Err(_) => Probe::Unreachable("the request failed".into()),
    }
}

/// Probe every route in `targets` that has no standing result, at once,
/// and record what each found.
pub async fn refresh(targets: Vec<(String, Option<String>)>) {
    let mut seen = std::collections::HashSet::new();
    let due: Vec<(String, Option<String>)> = targets
        .into_iter()
        .filter(|(u, b)| seen.insert(key(u, b.as_deref())))
        .filter(|(u, b)| cached(u, b.as_deref()).is_none())
        .collect();
    let runs = due.into_iter().map(|(url, bearer)| async move {
        let p = probe(&url, bearer.as_deref()).await;
        record(&url, bearer.as_deref(), p);
    });
    futures::future::join_all(runs).await;
}

/// Refresh the routes of every live peer in the registry that has a
/// dialing entry — the candidates `any`/`gpu` will choose among.
pub async fn refresh_pool() {
    let targets = crate::rt::spawn_blocking(|| {
        let store =
            crate::store::Store::open(&crate::config::veles_home().join("state.db")).ok()?;
        let stale = crate::config::load()
            .map(|c| c.cluster.stale_after_secs)
            .unwrap_or(600) as i64;
        let me = crate::node::NodeIdentity::load(&crate::config::config_dir())
            .ok()
            .flatten()
            .map(|i| i.id);
        let rows = store.nodes_list().ok()?;
        Some(
            rows.iter()
                .filter(|n| me.as_deref() != Some(n.id.as_str()))
                .filter(|n| !crate::node::is_stale(&n.heartbeat_at, stale))
                .filter_map(crate::delegate::remote_target_for)
                .collect::<Vec<_>>(),
        )
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    refresh(targets).await;
}

/// How this node can reach a registry node, for `routable` and the
/// `nodes` tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// It is this node.
    Local,
    /// No `[a2a_agents]` entry names it.
    NoEntry,
    /// An entry, and no standing probe result.
    Unprobed,
    /// An entry, and its last probe.
    Probed(Probe, Duration),
}

impl Route {
    /// Can `any`/`gpu` place work there?
    pub fn routable(&self) -> bool {
        matches!(self, Route::Local | Route::Probed(Probe::Ok, _))
    }

    /// One line for the `nodes` tool (no url, no token).
    pub fn describe(&self) -> String {
        match self {
            Route::Local => "this node".into(),
            Route::NoEntry => "no [a2a_agents] entry with its url and token".into(),
            Route::Unprobed => "has an [a2a_agents] entry; not probed yet".into(),
            Route::Probed(Probe::Ok, age) => {
                format!("auth probe passed {}s ago", age.as_secs())
            }
            Route::Probed(Probe::Refused(why), age) => {
                format!("auth probe refused {}s ago: {why}", age.as_secs())
            }
            Route::Probed(Probe::Unreachable(why), age) => {
                format!("auth probe got no answer {}s ago: {why}", age.as_secs())
            }
        }
    }
}

/// The route to `n` from this node (`me`).
pub fn route(n: &crate::store::NodeRow, me: Option<&str>) -> Route {
    if me == Some(n.id.as_str()) {
        return Route::Local;
    }
    match crate::delegate::remote_target_for(n) {
        None => Route::NoEntry,
        Some((url, bearer)) => match cached(&url, bearer.as_deref()) {
            Some((p, age)) => Route::Probed(p, age),
            None => Route::Unprobed,
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A loopback peer that answers like a veles A2A server: `tasks/get`
    /// with HTTP 200 + task-not-found for `good` (or any bearer when
    /// `good` is None), HTTP 429 for the bearer `rate-limited`, HTTP 401
    /// otherwise; `message/send` with a
    /// JSON-RPC error naming `name`, so a delegate verdict says which
    /// fake node the child was sent to. Returns its url.
    pub(crate) async fn fake_peer(name: &str, good: Option<&str>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/a2a", listener.local_addr().unwrap());
        let name = name.to_string();
        let good = good.map(str::to_string);
        crate::rt::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (name, good) = (name.clone(), good.clone());
                crate::rt::spawn(async move {
                    let mut raw = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let mut need: Option<usize> = None;
                    loop {
                        let n = match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        raw.extend_from_slice(&chunk[..n]);
                        if need.is_none() {
                            if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&raw[..end + 4]).to_lowercase();
                                let len = head
                                    .split("content-length:")
                                    .nth(1)
                                    .and_then(|t| t.split(['\r', '\n']).next())
                                    .and_then(|t| t.trim().parse::<usize>().ok())
                                    .unwrap_or(0);
                                need = Some(end + 4 + len);
                            }
                        }
                        if matches!(need, Some(t) if raw.len() >= t) {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&raw).to_string();
                    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                    let bearer = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .starts_with("authorization:")
                                .then(|| l.split_once("Bearer ").map(|(_, t)| t.trim().to_string()))
                                .flatten()
                        })
                        .unwrap_or_default();
                    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
                    let id = v["id"].clone();
                    let let_in = good.as_deref().is_none_or(|g| g == bearer);
                    let (status, reply) = if bearer == "rate-limited" {
                        (
                            "429 Too Many Requests",
                            serde_json::json!({"jsonrpc": "2.0", "id": id,
                                "error": {"code": -32029, "message": "rate limit exceeded"}}),
                        )
                    } else if !let_in {
                        (
                            "401 Unauthorized",
                            serde_json::json!({"jsonrpc": "2.0", "id": id,
                                "error": {"code": -32001, "message": "unauthorized"}}),
                        )
                    } else if v["method"] == "tasks/get" {
                        (
                            "200 OK",
                            serde_json::json!({"jsonrpc": "2.0", "id": id,
                                "error": {"code": -32001, "message": "task not found"}}),
                        )
                    } else {
                        (
                            "200 OK",
                            serde_json::json!({"jsonrpc": "2.0", "id": id,
                                "error": {"code": -32000, "message": format!("fake peer {name}")}}),
                        )
                    };
                    let body = reply.to_string();
                    let resp = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        url
    }

    /// A port nothing listens on (bound, then released).
    pub(crate) async fn dead_url() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        format!("http://{addr}/a2a")
    }

    /// The probe tells the outcomes apart over a real socket: let in,
    /// refused (wrong token, or rate-limited: a child sent now would be
    /// refused too), no answer.
    #[tokio::test]
    async fn a_probe_tells_let_in_from_refused_from_down() {
        let url = fake_peer("p", Some("right")).await;
        assert_eq!(probe(&url, Some("right")).await, Probe::Ok);
        assert!(matches!(
            probe(&url, Some("wrong")).await,
            Probe::Refused(why) if why.contains("401")
        ));
        assert!(matches!(
            probe(&url, Some("rate-limited")).await,
            Probe::Refused(why) if why.contains("429")
        ));
        assert!(matches!(
            probe(&dead_url().await, Some("right")).await,
            Probe::Unreachable(_)
        ));
    }

    /// Results stand for their TTL, a failure for less than a pass, and
    /// a rotated token is a different route (no standing result).
    #[tokio::test]
    async fn results_are_cached_per_route_and_expire() {
        let url = fake_peer("c", Some("tok-1")).await;
        refresh(vec![(url.clone(), Some("tok-1".into()))]).await;
        assert!(matches!(cached(&url, Some("tok-1")), Some((Probe::Ok, _))));
        assert!(cached(&url, Some("tok-2")).is_none(), "keyed by token");
        let past = |d: Duration| Instant::now().checked_sub(d).unwrap();
        record_at(
            &url,
            Some("tok-1"),
            Probe::Ok,
            past(OK_TTL - Duration::from_secs(5)),
        );
        assert!(
            cached(&url, Some("tok-1")).is_some(),
            "a pass stands for OK_TTL"
        );
        record_at(
            &url,
            Some("tok-1"),
            Probe::Ok,
            past(OK_TTL + Duration::from_secs(1)),
        );
        assert!(cached(&url, Some("tok-1")).is_none(), "and then expires");
        let fail = Probe::Unreachable("down".into());
        record_at(
            &url,
            Some("tok-1"),
            fail,
            past(FAIL_TTL + Duration::from_secs(1)),
        );
        assert!(
            cached(&url, Some("tok-1")).is_none(),
            "a failure is retried sooner"
        );
        // refresh re-probes an expired route.
        refresh(vec![(url.clone(), Some("tok-1".into()))]).await;
        assert!(matches!(cached(&url, Some("tok-1")), Some((Probe::Ok, _))));
        let r = Route::Probed(Probe::Refused("x".into()), Duration::from_secs(3));
        assert!(!r.routable());
        assert!(Route::Local.routable());
        assert!(!Route::Unprobed.routable(), "unprobed is not reachable");
    }
}
