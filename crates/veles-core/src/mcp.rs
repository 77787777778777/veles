//! MCP client — Model Context Protocol servers as veles tools.
//!
//! Reads `mcp_servers` from config (native or hermes compat), connects
//! each server (stdio subprocess or streamable-HTTP), discovers its
//! tools via `tools/list`, and exposes them as `mcp__<server>__<tool>`
//! in every session registry — hermes' naming convention, so prompts
//! and skills that reference MCP tools transfer unchanged.
//!
//! Transports:
//!   stdio  — child process, newline-delimited JSON-RPC 2.0
//!   http   — POST JSON-RPC to url w/ headers (streamable HTTP),
//!            accepts application/json or text/event-stream responses
//!
//! Failure policy (slopdrop-grade): a server that fails to connect is
//! logged and skipped — the gateway never dies for MCP.

use crate::tools::Tool;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};
use tokio::time::{timeout, Duration};

/// In-flight JSON-RPC calls, keyed by request id — the reader task
/// resolves each one. Named because the shape appears in both the
/// transport enum and the spawn path.
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// Parsed mcp_servers entry.
#[derive(Debug, Clone, Default)]
pub struct McpServerConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    pub headers: HashMap<String, String>,
    pub transport: Option<String>, // "sse" | "http" (default http for url)
    pub timeout_secs: u64,         // per tool call
    pub connect_timeout_secs: u64,
}

impl McpServerConfig {
    pub fn from_yaml(name: &str, v: &serde_yaml::Value) -> Option<Self> {
        let mut c = McpServerConfig {
            timeout_secs: 120,
            connect_timeout_secs: 60,
            ..Default::default()
        };
        let m = v.as_mapping()?;
        let get = |k: &str| m.get(serde_yaml::Value::String(k.into()));
        // `enabled: false` is hermes' off switch, and veles reads hermes
        // configs through the compat fallback. This parser used to know
        // only "neither command nor url ⇒ skipped", so a hermes entry
        // the operator had DISABLED read as ON here and its `command`
        // spawned anyway — the mechanism behind the 2026-10-02 zai-MCP
        // leak (`npx -y @z_ai/mcp-server@latest`, ten times a day). An
        // explicit `enabled: false` is now honoured: absent means on
        // (hermes' own default, and every veles-native entry), only a
        // literal false turns the server off.
        if get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            tracing::info!("mcp: server '{name}' is disabled (enabled: false) — skipped");
            return None;
        }
        c.command = get("command").and_then(|v| v.as_str()).map(str::to_string);
        c.url = get("url").and_then(|v| v.as_str()).map(str::to_string);
        c.transport = get("transport")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        c.timeout_secs = get("timeout").and_then(|v| v.as_u64()).unwrap_or(120);
        c.connect_timeout_secs = get("connect_timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(60);
        if let Some(args) = get("args").and_then(|v| v.as_sequence()) {
            c.args = args
                .iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect();
        }
        if let Some(env) = get("env").and_then(|v| v.as_mapping()) {
            for (k, val) in env {
                if let (Some(k), Some(val)) = (k.as_str(), val.as_str()) {
                    c.env.insert(k.to_string(), interpolate(val));
                }
            }
        }
        if let Some(h) = get("headers").and_then(|v| v.as_mapping()) {
            for (k, val) in h {
                if let (Some(k), Some(val)) = (k.as_str(), val.as_str()) {
                    c.headers.insert(k.to_string(), interpolate(val));
                }
            }
        }
        if c.command.is_none() && c.url.is_none() {
            tracing::warn!("mcp: server '{name}' has neither command nor url — skipped");
            return None;
        }
        Some(c)
    }
}

/// `${VAR}` interpolation: process env → VELES_HOME/.env → ~/.hermes/.env
/// (compat: hermes configs reference keys like ZAI_CODING_PLAN_API_KEY).
pub fn interpolate(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find('}') {
            let var = &after[..end];
            out.push_str(&lookup_env(var).unwrap_or_default());
            rest = &after[end + 1..];
        } else {
            out.push_str("${");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn lookup_env(var: &str) -> Option<String> {
    // The resolver first (environment, vault, veles' env files), so an
    // MCP server's `${TOKEN}` can come from the vault.
    if let Some(v) = crate::secrets::resolve(var) {
        return Some(v);
    }
    for dir in [crate::config::veles_home(), dirs_home().join(".hermes")] {
        if let Some(v) = dotenv_lookup(var, &dir) {
            return Some(v);
        }
    }
    None
}

fn dirs_home() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

fn dotenv_lookup(var: &str, dir: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(".env")).ok()?;
    for line in raw.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == var {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

// ── transport handle ───────────────────────────────────────────────────

// `tokio::process::Child` is several hundred bytes on Windows (its
// handle wrapper and the pipes), so clippy sees a large variant there
// and not on Linux. One transport handle per server: boxing it would buy
// nothing.
#[allow(clippy::large_enum_variant)]
enum Transport {
    Stdio {
        tx: Mutex<tokio::process::ChildStdin>,
        pending: PendingMap,
        next_id: Mutex<u64>,
        _child: tokio::process::Child, // keep the process alive
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
        client: reqwest::Client,
        session_id: Mutex<Option<String>>,
    },
}

pub struct McpConnection {
    server: String,
    transport: Transport,
    timeout_secs: u64,
}

impl McpConnection {
    /// JSON-RPC request over either transport.
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        match &self.transport {
            Transport::Stdio {
                tx,
                pending,
                next_id,
                ..
            } => {
                let id = {
                    let mut n = next_id.lock().await;
                    *n += 1;
                    *n
                };
                let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
                let mut line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
                line.push('\n');
                let (otx, orx) = oneshot::channel();
                pending.lock().await.insert(id, otx);
                {
                    let mut w = tx.lock().await;
                    w.write_all(line.as_bytes())
                        .await
                        .map_err(|e| format!("write: {e}"))?;
                    w.flush().await.map_err(|e| format!("flush: {e}"))?;
                }
                let res = timeout(Duration::from_secs(self.timeout_secs), orx)
                    .await
                    .map_err(|_| format!("timeout after {}s", self.timeout_secs))?
                    .map_err(|_| "response channel closed".to_string())?;
                res
            }
            Transport::Http {
                url,
                headers,
                client,
                session_id,
            } => {
                let msg = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
                let mut req = client.post(url).json(&msg);
                req = req.header(
                    reqwest::header::ACCEPT,
                    "application/json, text/event-stream",
                );
                for (k, v) in headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                if let Some(sid) = session_id.lock().await.as_ref() {
                    req = req.header("mcp-session-id", sid.as_str());
                }
                let resp = req.send().await.map_err(|e| format!("http: {e}"))?;
                // Capture session id from initialize responses.
                if let Some(sid) = resp.headers().get("mcp-session-id") {
                    if let Ok(s) = sid.to_str() {
                        *session_id.lock().await = Some(s.to_string());
                    }
                }
                let ctype = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().unwrap_or("").split(';').next())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let body = resp.text().await.map_err(|e| format!("body: {e}"))?;
                tracing::debug!(
                    "mcp http {}: method={method} ctype={ctype} body={}",
                    self.server,
                    &body.chars().take(300).collect::<String>()
                );
                let payload = if ctype == "text/event-stream" {
                    // SSE: take the first data: line carrying a JSON object.
                    body.lines()
                        .filter_map(|l| l.strip_prefix("data:"))
                        .find_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
                        .ok_or("no JSON payload in SSE stream")?
                } else {
                    serde_json::from_str(&body).map_err(|e| format!("json: {e}: {body}"))?
                };
                if let Some(err) = payload.get("error") {
                    return Err(format!("rpc error: {err}"));
                }
                Ok(payload.get("result").cloned().unwrap_or(Value::Null))
            }
        }
    }

    /// Notification (no response expected) — initialize handshake.
    async fn notify(&self, method: &str, params: Value) {
        if let Transport::Stdio { tx, .. } = &self.transport {
            let msg = json!({"jsonrpc":"2.0","method":method,"params":params});
            if let Ok(mut line) = serde_json::to_string(&msg) {
                line.push('\n');
                let mut w = tx.lock().await;
                let _ = w.write_all(line.as_bytes()).await;
                let _ = w.flush().await;
            }
        }
        // HTTP transport: initialized notification is optional; servers
        // that need it accept a POST — send it best-effort.
        if let Transport::Http {
            url,
            headers,
            client,
            session_id,
        } = &self.transport
        {
            let msg = json!({"jsonrpc":"2.0","method":method,"params":params});
            let mut req = client.post(url).json(&msg);
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(sid) = session_id.lock().await.as_ref() {
                req = req.header("mcp-session-id", sid.as_str());
            }
            let _ = req.send().await;
        }
    }
}

// ── connection establishment ───────────────────────────────────────────

pub async fn connect(server: &str, cfg: &McpServerConfig) -> Result<Arc<McpConnection>, String> {
    let conn = if let Some(cmd) = &cfg.command {
        connect_stdio(server, cfg, cmd).await?
    } else if let Some(url) = &cfg.url {
        connect_http(server, cfg, url).await?
    } else {
        return Err("neither command nor url".into());
    };
    Ok(Arc::new(conn))
}

async fn connect_stdio(
    server: &str,
    cfg: &McpServerConfig,
    cmd: &str,
) -> Result<McpConnection, String> {
    // A PATH lookup with the platform's spellings (`npx.cmd` on
    // Windows); a bare name reaches the OS unchanged where PATH has
    // nothing.
    let mut c = tokio::process::Command::new(crate::shell::resolve_program(cmd));
    c.args(&cfg.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    for (k, v) in &cfg.env {
        c.env(k, v);
    }
    let mut child = c.spawn().map_err(|e| format!("spawn {cmd}: {e}"))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;

    // Shared pending-response map: the reader task resolves by id.
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let reader_pending = pending.clone();
    crate::rt::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(id) = v.get("id").and_then(Value::as_u64) {
                let result = if let Some(err) = v.get("error") {
                    Err(format!("rpc error: {err}"))
                } else {
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                };
                if let Some(tx) = reader_pending.lock().await.remove(&id) {
                    let _ = tx.send(result);
                }
            }
        }
    });

    let conn = McpConnection {
        server: server.to_string(),
        transport: Transport::Stdio {
            tx: Mutex::new(stdin),
            pending,
            next_id: Mutex::new(0),
            _child: child,
        },
        timeout_secs: cfg.timeout_secs,
    };
    handshake(&conn).await?;
    Ok(conn)
}

async fn connect_http(
    server: &str,
    cfg: &McpServerConfig,
    url: &str,
) -> Result<McpConnection, String> {
    let conn = McpConnection {
        server: server.to_string(),
        transport: Transport::Http {
            url: url.to_string(),
            headers: cfg.headers.clone(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(cfg.timeout_secs))
                .build()
                .map_err(|e| e.to_string())?,
            session_id: Mutex::new(None),
        },
        timeout_secs: cfg.timeout_secs,
    };
    handshake(&conn).await?;
    Ok(conn)
}

async fn handshake(conn: &McpConnection) -> Result<(), String> {
    let init = conn
        .rpc(
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "veles", "version": env!("CARGO_PKG_VERSION")}
            }),
        )
        .await?;
    let server_name = init
        .get("serverInfo")
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    tracing::info!("mcp: {} connected to server '{server_name}'", conn.server);
    conn.notify("notifications/initialized", json!({})).await;
    Ok(())
}

/// Discover tools → one registered Tool per MCP tool.
pub async fn discover_tools(
    server: &str,
    conn: Arc<McpConnection>,
) -> Result<Vec<Arc<dyn Tool>>, String> {
    let res = conn.rpc("tools/list", json!({})).await?;
    let tools = res
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    for t in tools {
        let Some(name) = t.get("name").and_then(Value::as_str) else {
            continue;
        };
        out.push(Arc::new(McpTool {
            full_name: format!(
                "mcp__{}__{}",
                server.replace('-', "_"),
                name.replace('-', "_")
            ),
            server_tool: name.to_string(),
            description: t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("(mcp tool)")
                .to_string(),
            schema: t
                .get("inputSchema")
                .cloned()
                .unwrap_or(json!({"type":"object","properties":{}})),
            conn: conn.clone(),
        }));
    }
    Ok(out)
}

struct McpTool {
    full_name: String,
    server_tool: String,
    description: String,
    schema: Value,
    conn: Arc<McpConnection>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    async fn call(&self, args: Value) -> String {
        // All MCP I/O executes on the host runtime; this side only awaits
        // the oneshot reply (runtime-agnostic). See the MCP host section.
        let conn = self.conn.clone();
        let server_tool = self.server_tool.clone();
        let res = mcp_host()
            .run(async move {
                conn.rpc(
                    "tools/call",
                    json!({"name": server_tool, "arguments": args}),
                )
                .await
            })
            .await;
        match res {
            Some(Ok(res)) => {
                // content: [{type:"text", text:"..."}, ...]
                let mut out = String::new();
                if let Some(items) = res.get("content").and_then(Value::as_array) {
                    for item in items {
                        if item.get("type").and_then(Value::as_str) == Some("text") {
                            if let Some(txt) = item.get("text").and_then(Value::as_str) {
                                out.push_str(txt);
                            }
                        }
                    }
                }
                if out.is_empty() {
                    out = res.to_string();
                }
                out
            }
            Some(Err(e)) => format!("error: mcp call failed: {e}"),
            None => "error: mcp host is shutting down".to_string(),
        }
    }
}

/// Boot-time: connect all configured servers, return their tools.
/// Never fails the gateway — per-server errors are logged and skipped.
pub async fn connect_all(cfg: &crate::config::VelesConfig) -> Vec<Arc<dyn Tool>> {
    let mut all: Vec<Arc<dyn Tool>> = Vec::new();
    for (name, v) in &cfg.mcp_servers {
        let Some(sc) = McpServerConfig::from_yaml(name, v) else {
            continue;
        };
        match timeout(
            Duration::from_secs(sc.connect_timeout_secs),
            connect(name, &sc),
        )
        .await
        {
            Ok(Ok(conn)) => match discover_tools(name, conn).await {
                Ok(tools) => {
                    tracing::info!(
                        "mcp: server '{name}' registered {} tool(s): {}",
                        tools.len(),
                        tools
                            .iter()
                            .map(|t| t.name().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    all.extend(tools);
                }
                Err(e) => tracing::warn!("mcp: '{name}' tools/list failed: {e}"),
            },
            Ok(Err(e)) => tracing::warn!("mcp: '{name}' connect failed: {e}"),
            Err(_) => tracing::warn!("mcp: '{name}' connect timed out"),
        }
    }
    all
}

// ── MCP host thread (slopdrop-pattern: dedicated I/O thread + channels) ─
//
// Tokio I/O objects (stdio children, reader tasks) belong to the runtime
// that spawned them. The gateway runs MCP on its long-lived multi-thread
// runtime, but chat/TUI/one-shot frontends used short-lived or foreign
// runtimes — every spawned MCP object died or stalled the moment control
// returned, so tool calls hung until timeout.
//
// The fix: ONE process-lifetime host thread that owns ALL MCP I/O.
//   * owns a `new_current_thread().enable_all()` runtime
//   * parks on a tokio mpsc command channel — parked, not blocked, so
//     spawned tasks (per-connection readers, in-flight calls) progress
//     concurrently while the host waits
//   * work arrives as boxed futures and is `tokio::spawn`ed there, so
//     every MCP connection AND every tool call executes under the same
//     reactor — no cross-runtime polling, ever
//   * replies cross back on oneshots (runtime-agnostic awaiters)
//   * handles are `Arc`-counted; the last drop sends Shutdown and joins
//
// Same shape as slopdrop's TclThreadHandle: command channel + oneshot
// replies + explicit lifecycle. The registry keeps tool handles (each
// holding a host clone) alive for the process lifetime, and process exit
// reaps the children regardless (they are kill_on_drop).

use futures::future::BoxFuture;

enum HostCommand {
    /// Run a boxed future on the host runtime. The future itself is
    /// responsible for delivering its result (oneshot, channel, ...).
    Run(BoxFuture<'static, ()>),
    Shutdown,
}

struct McpHostInner {
    command_tx: tokio::sync::mpsc::UnboundedSender<HostCommand>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl McpHostInner {
    fn spawn() -> Self {
        let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel::<HostCommand>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<tokio::runtime::Handle>();
        let thread = std::thread::Builder::new()
            .name("veles-mcp-host".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("mcp host: runtime build");
                let _ = ready_tx.send(rt.handle().clone());
                drop(ready_tx);
                rt.block_on(async move {
                    while let Some(cmd) = command_rx.recv().await {
                        match cmd {
                            HostCommand::Run(fut) => {
                                crate::rt::spawn(fut);
                            }
                            HostCommand::Shutdown => break,
                        }
                    }
                });
                // Dropping `rt` here stops the driver; kill_on_drop
                // children terminate their subprocesses.
            })
            .expect("mcp host: thread spawn");
        // Startup barrier: block until the host thread's runtime is up.
        // The handle itself is discarded — the thread owns the runtime —
        // but the recv must stay, or callers can send commands into a
        // channel nobody is draining yet.
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("mcp host: thread failed to start");
        Self {
            command_tx,
            thread: Some(thread),
        }
    }
}

impl Drop for McpHostInner {
    fn drop(&mut self) {
        let _ = self.command_tx.send(HostCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A handle to the process-lifetime MCP host thread. Cloning distributes
/// ownership; when all clones drop, the host shuts down and joins.
#[derive(Clone)]
pub struct McpHostHandle {
    inner: std::sync::Arc<McpHostInner>,
}

impl McpHostHandle {
    /// Execute an async closure on the host runtime, await its result from
    /// any async context. Safe to call from any runtime (or a block_on).
    pub async fn run<T, F>(&self, f: F) -> Option<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<T>();
        let fut: BoxFuture<'static, ()> = Box::pin(async move {
            let out = f.await;
            let _ = tx.send(out);
        });
        match self.inner.command_tx.send(HostCommand::Run(fut)) {
            Ok(()) => rx.await.ok(),
            Err(_) => None, // host shutting down
        }
    }

    /// Synchronous entry for non-async callers (boot paths, tests).
    /// Safe from ANY context — including inside a running tokio runtime:
    /// the host thread drives the future, and this side parks on a plain
    /// std channel. No second runtime is built, so this can never hit
    /// "Cannot start a runtime from within a runtime".
    pub fn block_on_call<T, F>(&self, f: F) -> Option<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel::<T>();
        let fut: BoxFuture<'static, ()> = Box::pin(async move {
            let out = f.await;
            let _ = tx.send(out);
        });
        match self.inner.command_tx.send(HostCommand::Run(fut)) {
            Ok(()) => rx.recv().ok(),
            Err(_) => None, // host shutting down
        }
    }
}

static MCP_HOST: std::sync::OnceLock<McpHostHandle> = std::sync::OnceLock::new();

/// The shared MCP host for this process (lazily spawned on first use).
pub fn mcp_host() -> McpHostHandle {
    MCP_HOST
        .get_or_init(|| McpHostHandle {
            inner: std::sync::Arc::new(McpHostInner::spawn()),
        })
        .clone()
}

static MCP_BOOTED: std::sync::Once = std::sync::Once::new();

/// Connect all configured MCP servers ON the host runtime and register
/// their tools into the deferred registry. Sync, safe from any context
/// (including inside a running tokio runtime), never fails the caller —
/// per-server failures are logged and skipped. Idempotent: the gateway
/// boot path AND DesktopServer::new both call this; the Once guard keeps
/// the second call from reconnecting every server (each reconnect would
/// spawn a duplicate stdio child).
pub fn boot_mcp_deferred(cfg: &crate::config::VelesConfig) {
    if cfg.mcp_servers.is_empty() {
        return;
    }
    MCP_BOOTED.call_once(|| {
        tracing::info!(
            "mcp: connecting {} configured server(s)",
            cfg.mcp_servers.len()
        );
        let host = mcp_host();
        let cfg = cfg.clone();
        let tools = host
            .block_on_call(async move { connect_all(&cfg).await })
            .unwrap_or_default();
        for t in tools {
            let name = t.name().to_string();
            let desc = t.description().to_string();
            let schema = t.schema();
            crate::deferred::register_deferred(crate::deferred::DeferredTool {
                name,
                description: desc,
                schema,
                handler: t,
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_direct() {
        let _g = crate::testenv::lock();
        std::env::set_var("VELES_MCP_TEST_VAR", "hello");
        assert_eq!(interpolate("Bearer ${VELES_MCP_TEST_VAR}"), "Bearer hello");
        assert_eq!(interpolate("no vars here"), "no vars here");
        assert_eq!(interpolate("${MISSING_VAR_XYZ}"), "");
    }

    #[test]
    fn config_parse_stdio_and_http() {
        let v: serde_yaml::Value = serde_yaml::from_str(
            r#"
command: npx
args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
timeout: 90
"#,
        )
        .unwrap();
        let c = McpServerConfig::from_yaml("fs", &v).unwrap();
        assert_eq!(c.command.as_deref(), Some("npx"));
        assert_eq!(c.args.len(), 3);
        assert_eq!(c.timeout_secs, 90);

        let v2: serde_yaml::Value = serde_yaml::from_str(
            r#"
url: https://example.com/mcp
headers:
  Authorization: Bearer tok
"#,
        )
        .unwrap();
        let c2 = McpServerConfig::from_yaml("remote", &v2).unwrap();
        assert!(c2.url.is_some());
        assert_eq!(c2.headers.get("Authorization").unwrap(), "Bearer tok");

        // Neither command nor url → rejected.
        let v3: serde_yaml::Value = serde_yaml::from_str("timeout: 5").unwrap();
        assert!(McpServerConfig::from_yaml("bad", &v3).is_none());
    }

    /// The 2026-10-02 zai-MCP leak, pinned at the parser. A hermes
    /// entry the operator DISABLED (`enabled: false`) must not connect:
    /// veles reads hermes configs through the compat fallback, and this
    /// parser used to know only "neither command nor url ⇒ skipped", so
    /// a disabled `command: npx` entry spawned anyway. Absent `enabled`
    /// stays ON (hermes' default, and every veles-native entry); only a
    /// literal `false` turns it off; `true` and junk stay on.
    #[test]
    fn a_disabled_entry_never_connects() {
        let _g = crate::testenv::lock();
        let off: serde_yaml::Value = serde_yaml::from_str(
            "enabled: false\ncommand: npx\nargs: [\"-y\", \"@z_ai/mcp-server@latest\"]",
        )
        .unwrap();
        assert!(
            McpServerConfig::from_yaml("zai-vision", &off).is_none(),
            "enabled: false must refuse to build a server that would spawn npx"
        );

        // A disabled REMOTE entry is refused too (the url branch).
        let off_url: serde_yaml::Value =
            serde_yaml::from_str("enabled: false\nurl: https://api.z.ai/mcp").unwrap();
        assert!(McpServerConfig::from_yaml("zai-web", &off_url).is_none());

        // Absent `enabled` stays ON — the common case must not regress.
        let on: serde_yaml::Value =
            serde_yaml::from_str("command: npx\nargs: [\"-y\", \"srv\"]").unwrap();
        assert!(McpServerConfig::from_yaml("plain", &on).is_some());

        // Explicit true, and a non-bool value, stay ON (only a literal
        // false is an off switch).
        let on_true: serde_yaml::Value =
            serde_yaml::from_str("enabled: true\ncommand: npx\nargs: [\"x\"]").unwrap();
        assert!(McpServerConfig::from_yaml("on", &on_true).is_some());
        let on_junk: serde_yaml::Value =
            serde_yaml::from_str("enabled: \"nope\"\ncommand: npx\nargs: [\"x\"]").unwrap();
        assert!(McpServerConfig::from_yaml("junk", &on_junk).is_some());
    }
}
