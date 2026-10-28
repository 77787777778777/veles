// An adapter/plugin/types crate: its spawn sites are its OWN work on its
// OWN runtime (a connection task, a test driver), not a module calling
// into core. The M3 hop (clippy.toml) is veles-core/veles-frontends
// business; when THIS crate becomes a module (R4/R6) its spawn sites are
// reviewed and the allow is dropped.
#![allow(clippy::disallowed_methods)]
//! Write a veles process plugin.
//!
//! A process plugin is an executable the gateway spawns and supervises
//! (PLAN-plugins-installer.md). Implement [`Plugin`], describe it with a
//! [`Manifest`], and hand both to [`run`] from `main`:
//!
//! ```no_run
//! use veles_plugin_sdk::*;
//!
//! struct Hello;
//!
//! #[async_trait]
//! impl Plugin for Hello {
//!     async fn start(&self, host: Host, _init: InitParams) -> Result<(), String> {
//!         host.note("hello started", "").ok();
//!         Ok(())
//!     }
//! }
//!
//! fn main() -> std::process::ExitCode {
//!     let manifest = Manifest {
//!         name: "hello".into(),
//!         version: env!("CARGO_PKG_VERSION").into(),
//!         protocol: PROTOCOL,
//!         ..Default::default()
//!     };
//!     run(manifest, Hello)
//! }
//! ```
//!
//! `run` answers `--describe` (prints the manifest, exits), then speaks
//! the protocol on stdin/stdout until the gateway says `shutdown` or
//! goes away. **Never write to stdout yourself**: it is the protocol
//! stream. Logs go to stderr through `tracing`, and the gateway files
//! them under the plugin's name.

use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

pub use async_trait::async_trait;
pub use veles_types::ask;
pub use veles_types::bus::{InboundMessage, MessageKind, OutboundMessage, DEFAULT_ROUTE};
pub use veles_types::jsonrpc::{Peer, RpcError};
pub use veles_types::plugin::{
    self as proto, method, Dirs, InitParams, Instances, Manifest, PlatformDecl, Receipt,
    SecretDecl, ServiceDecl, UseDecl, PROTOCOL, SERVICE_CAP_ANY,
};
pub use veles_types::{Knob, SenderContext};

use veles_types::jsonrpc::{self, CloseReason, Handler, Options};

/// How long [`Plugin::shutdown`] may take before the process exits
/// anyway. The gateway kills a slow plugin itself when it asks for the
/// shutdown, but when the gateway is gone (SIGKILL, OOM) nobody is left
/// to: a plugin whose shutdown hangs would outlive it — for IRC, a
/// second bot on the network next to the restarted gateway's.
pub const SHUTDOWN_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// What a plugin does. Only [`Plugin::start`] is required.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// The gateway's first call. Start your work on tasks of your own and
    /// return: the gateway waits for this answer, with a timeout. An
    /// `Err` ends the process, and the gateway reports the message.
    async fn start(&self, host: Host, init: InitParams) -> Result<(), String>;

    /// Deliver one outbound message to the platform. Called one at a
    /// time per instance, in order.
    async fn deliver(&self, msg: OutboundMessage) -> Result<Receipt, String> {
        let _ = msg;
        Err("this plugin delivers no messages".into())
    }

    /// Any other request from the gateway (a service, a test hook).
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let _ = params;
        Err(RpcError::method_not_found(method))
    }

    /// Any notification from the gateway.
    async fn notification(&self, method: &str, params: Value) {
        let _ = (method, params);
    }

    /// The gateway asked this process to stop (a reload, a stop, the
    /// gateway shutting down), or went away. Close connections cleanly
    /// here; the process exits when this returns.
    async fn shutdown(&self, reason: &str) {
        let _ = reason;
    }
}

/// The gateway, as the plugin sees it.
#[derive(Clone)]
pub struct Host {
    peer: Peer,
    instance: String,
}

impl Host {
    /// This process's instance name (`""` for a single-instance plugin).
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// A message from the platform for the agent. The gateway sets the
    /// route to this instance.
    pub fn inbound(&self, msg: &InboundMessage) -> Result<(), RpcError> {
        let v = serde_json::to_value(msg).map_err(|e| RpcError::internal(e.to_string()))?;
        self.peer.notify(method::INBOUND, v)
    }

    /// Park a plugin-action note on the transcript.
    pub fn note(&self, summary: &str, detail: &str) -> Result<(), RpcError> {
        self.peer
            .notify(method::NOTE, json!({"summary": summary, "detail": detail}))
    }

    /// A secret the manifest declares. `Ok(None)`: declared but unset.
    pub async fn secret(&self, name: &str) -> Result<Option<String>, RpcError> {
        let v = self
            .peer
            .call(method::SECRET, json!({"name": name}))
            .await?;
        let v: proto::SecretValue =
            serde_json::from_value(v).map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(v.value)
    }

    /// Who this sender is, according to the gateway's access model: the
    /// gateway's resolved principal (`role`, `caps`, …), as JSON.
    pub async fn resolve(
        &self,
        sender_id: Option<&str>,
        ctx: &SenderContext,
    ) -> Result<Value, RpcError> {
        self.peer
            .call(
                method::RESOLVE,
                json!({"sender_id": sender_id, "sender_ctx": ctx}),
            )
            .await
    }

    /// Any gateway method by name.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.peer.call(method, params).await
    }

    /// Call another plugin's (or core's) service by name, through the
    /// gateway's registry: whoever provides it answers. Declare it in
    /// the manifest's `uses`. Refused when nothing provides it here, or
    /// when its provider keeps it internal (core and modules only) or
    /// gates it on a capability the session's principal lacks.
    pub async fn call_service(
        &self,
        service: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, RpcError> {
        let p = proto::ServiceCallParams {
            service: service.to_string(),
            params,
            session_id: session_id.map(str::to_string),
        };
        self.peer
            .call(
                method::SERVICE_CALL,
                serde_json::to_value(p).map_err(|e| RpcError::internal(e.to_string()))?,
            )
            .await
    }

    /// What the layer below this plugin's own answer would be, while it
    /// answers an override point (or any service it provides over
    /// another provider): `token` is the `next` field of the request
    /// envelope (`{input, next, caller, session_id}`), valid while that
    /// request runs (PLAN-extension-api.md D2).
    pub async fn service_next(&self, token: &str, params: Value) -> Result<Value, RpcError> {
        let p = proto::ServiceNextParams {
            token: token.to_string(),
            params,
        };
        self.peer
            .call(
                method::SERVICE_NEXT,
                serde_json::to_value(p).map_err(|e| RpcError::internal(e.to_string()))?,
            )
            .await
    }

    /// A scoped, TTL'd runtime grant for an identity on one of this
    /// plugin's own platforms (the atproto door: a mutual gets User).
    /// The gateway writes the ACL; refused for an identity outside the
    /// plugin's platforms, and `owner` is never grantable.
    pub async fn grant_scoped(
        &self,
        identity: &str,
        role: &str,
        ttl_secs: Option<u64>,
    ) -> Result<proto::GrantResult, RpcError> {
        let v = self
            .peer
            .call(
                method::GRANT,
                json!({"identity": identity, "role": role, "ttl_secs": ttl_secs}),
            )
            .await?;
        serde_json::from_value(v).map_err(|e| RpcError::internal(e.to_string()))
    }

    /// Register a public thread's participant roster, so the slopdrop
    /// bridge's `[names]`/`[name]` resolve on this surface. Display only.
    pub fn register_thread_room(&self, thread: &str, handles: &[String]) -> Result<(), RpcError> {
        self.peer
            .notify(method::ROOM, json!({"thread": thread, "handles": handles}))
    }

    /// A dotted config.toml path the manifest declares in
    /// `config_sections`; anything else is refused.
    pub async fn config_get(&self, path: &str) -> Result<Value, RpcError> {
        let v = self
            .peer
            .call(method::CONFIG_GET, json!({"path": path}))
            .await?;
        let v: proto::ConfigGetValue =
            serde_json::from_value(v).map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(v.value)
    }

    /// Report this instance's delivery ids for a ledger entry after the
    /// fact (`meta_keys::LEDGER_ENTRY` named it on the outbound message);
    /// the gateway relays them to whoever keeps that ledger.
    pub fn record_posts(&self, entry_id: &str, ids: &[String]) -> Result<(), RpcError> {
        self.peer.notify(
            method::RECORD_POSTS,
            json!({"entry_id": entry_id, "ids": ids}),
        )
    }
}

/// Run `plugin` as a process plugin. Call from `main`; returns the
/// process's exit code.
pub fn run<P: Plugin>(manifest: Manifest, plugin: P) -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--describe") {
        match serde_json::to_string_pretty(&manifest) {
            Ok(s) => {
                println!("{s}");
                return std::process::ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("{}: cannot describe itself: {e}", manifest.name);
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    if args.iter().any(|a| a == "--version") {
        println!("{} {}", manifest.name, manifest.version);
        return std::process::ExitCode::SUCCESS;
    }
    if let Some(a) = args.first() {
        eprintln!(
            "{}: a veles process plugin, started by the gateway (unknown argument {a}; \
             --describe prints what it provides)",
            manifest.name
        );
        return std::process::ExitCode::from(2);
    }
    init_logging();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{}: no runtime: {e}", manifest.name);
            return std::process::ExitCode::FAILURE;
        }
    };
    let code = rt.block_on(serve(
        manifest,
        Arc::new(plugin),
        tokio::io::stdin(),
        tokio::io::stdout(),
    ));
    // Tasks the plugin left running (a reconnect loop) must not keep the
    // process alive once the conversation with the gateway is over.
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    code
}

/// Logs to stderr, plain: the gateway adds the time and the plugin's
/// name when it files each line.
fn init_logging() {
    let filter = log_filter(std::env::var("RUST_LOG").ok());
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .with_env_filter(filter)
        .try_init();
}

/// The filter `RUST_LOG` asks for, with an EMPTY variable read as unset
/// ("info"), not as "no directives" (which tracing takes as OFF: no line
/// at all). The gateway passes `RUST_LOG` through to a plugin process as
/// it finds it, and nixpkgs' buildRustPackage exports it empty, so in
/// the flake's sandbox the echo plugin printed nothing and the host's
/// log test timed out on "echo room a up" (2026-10-13). `off` stays off.
fn log_filter(rust_log: Option<String>) -> tracing_subscriber::EnvFilter {
    match rust_log {
        Some(v) if !v.trim().is_empty() => tracing_subscriber::EnvFilter::try_new(v)
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        _ => tracing_subscriber::EnvFilter::new("info"),
    }
}

/// The protocol loop over any byte stream: what [`run`] does on
/// stdin/stdout, and what a test drives over an in-memory pipe.
pub async fn serve<P, R, W>(
    manifest: Manifest,
    plugin: Arc<P>,
    reader: R,
    writer: W,
) -> std::process::ExitCode
where
    P: Plugin,
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let sdk = Arc::new(Sdk {
        manifest,
        plugin,
        host: OnceLock::new(),
        stopped: tokio::sync::Notify::new(),
        stopping: std::sync::atomic::AtomicBool::new(false),
    });
    let (_peer, done) = jsonrpc::connect(reader, writer, sdk.clone(), Options::default());
    tokio::select! {
        reason = done => {
            // The gateway went away without a shutdown: still let the
            // plugin close its connections.
            let reason = reason.unwrap_or(CloseReason::Eof);
            if !sdk.stopping.swap(true, std::sync::atomic::Ordering::AcqRel) {
                let why = format!("the gateway went away ({reason})");
                if tokio::time::timeout(SHUTDOWN_LIMIT, sdk.plugin.shutdown(&why))
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        "shutdown did not finish within {}s; exiting anyway",
                        SHUTDOWN_LIMIT.as_secs()
                    );
                    return std::process::ExitCode::FAILURE;
                }
            }
        }
        _ = sdk.stopped.notified() => {
            // Give the writer a moment to send the shutdown answer.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    std::process::ExitCode::SUCCESS
}

struct Sdk<P: Plugin> {
    manifest: Manifest,
    plugin: Arc<P>,
    host: OnceLock<Host>,
    stopped: tokio::sync::Notify,
    stopping: std::sync::atomic::AtomicBool,
}

fn params<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, RpcError> {
    serde_json::from_value(v).map_err(|e| RpcError::invalid_params(e.to_string()))
}

#[async_trait]
impl<P: Plugin> Handler for Sdk<P> {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            method::INITIALIZE => {
                let init: InitParams = params(p)?;
                if init.protocol != PROTOCOL {
                    return Err(RpcError::refused(format!(
                        "the gateway speaks plugin protocol {}, {} speaks {PROTOCOL}",
                        init.protocol, self.manifest.name
                    )));
                }
                let host = Host {
                    peer: peer.clone(),
                    instance: init.instance.clone(),
                };
                if self.host.set(host.clone()).is_err() {
                    return Err(RpcError::refused("already initialized"));
                }
                self.plugin
                    .start(host, init)
                    .await
                    .map_err(RpcError::internal)?;
                serde_json::to_value(&self.manifest).map_err(|e| RpcError::internal(e.to_string()))
            }
            method::DELIVER => {
                if self.host.get().is_none() {
                    return Err(RpcError::refused("not initialized"));
                }
                let msg: OutboundMessage = params(p)?;
                let r = self.plugin.deliver(msg).await.map_err(RpcError::internal)?;
                serde_json::to_value(r).map_err(|e| RpcError::internal(e.to_string()))
            }
            method::SHUTDOWN => {
                let sp: proto::ShutdownParams = params(p).unwrap_or_default();
                if !self
                    .stopping
                    .swap(true, std::sync::atomic::Ordering::AcqRel)
                {
                    let _ = tokio::time::timeout(SHUTDOWN_LIMIT, self.plugin.shutdown(&sp.reason))
                        .await;
                }
                self.stopped.notify_one();
                Ok(Value::Null)
            }
            other => {
                if self.host.get().is_none() {
                    return Err(RpcError::refused("not initialized"));
                }
                self.plugin.request(other, p).await
            }
        }
    }

    async fn notification(&self, _peer: &Peer, method: &str, p: Value) {
        self.plugin.notification(method, p).await;
    }
}

#[cfg(test)]
mod tests {
    /// An empty `RUST_LOG` is unset, not off: the plugin's lines reach
    /// its stderr (and the host's per-instance log) as with no variable.
    #[test]
    fn an_empty_rust_log_is_unset_not_off() {
        use tracing::level_filters::LevelFilter;
        let hint = |v: Option<&str>| super::log_filter(v.map(str::to_string)).max_level_hint();
        assert_eq!(hint(None), Some(LevelFilter::INFO));
        assert_eq!(hint(Some("")), Some(LevelFilter::INFO));
        assert_eq!(hint(Some("  ")), Some(LevelFilter::INFO));
        assert_eq!(hint(Some("off")), Some(LevelFilter::OFF));
        assert_eq!(hint(Some("debug")), Some(LevelFilter::DEBUG));
    }

    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Rec {
        started: Mutex<Option<InitParams>>,
        stopped: Mutex<Option<String>>,
    }

    #[async_trait]
    impl Plugin for Rec {
        async fn start(&self, _host: Host, init: InitParams) -> Result<(), String> {
            *self.started.lock().unwrap() = Some(init);
            Ok(())
        }
        async fn shutdown(&self, reason: &str) {
            *self.stopped.lock().unwrap() = Some(reason.into());
        }
    }

    struct Quiet;
    #[async_trait]
    impl Handler for Quiet {
        async fn request(&self, _: &Peer, m: &str, _: Value) -> Result<Value, RpcError> {
            Err(RpcError::method_not_found(m))
        }
        async fn notification(&self, _: &Peer, _: &str, _: Value) {}
    }

    fn manifest() -> Manifest {
        Manifest {
            name: "rec".into(),
            version: "0".into(),
            protocol: PROTOCOL,
            ..Default::default()
        }
    }

    /// The gateway end of an in-memory pipe, and the plugin served on
    /// the other end.
    fn wired(plugin: Arc<Rec>) -> (Peer, tokio::task::JoinHandle<std::process::ExitCode>) {
        let (g, p) = tokio::io::duplex(1 << 16);
        let (gr, gw) = tokio::io::split(g);
        let (pr, pw) = tokio::io::split(p);
        let (gateway, _) = jsonrpc::connect(gr, gw, Arc::new(Quiet), Options::default());
        let served = tokio::spawn(serve(manifest(), plugin, pr, pw));
        (gateway, served)
    }

    fn init(protocol: u32) -> Value {
        serde_json::to_value(InitParams {
            protocol,
            plugin: "rec".into(),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initialize_starts_the_plugin_and_answers_the_manifest() {
        let rec = Arc::new(Rec::default());
        let (gw, _served) = wired(rec.clone());
        let v = gw.call(method::INITIALIZE, init(PROTOCOL)).await.unwrap();
        assert_eq!(serde_json::from_value::<Manifest>(v).unwrap(), manifest());
        assert!(rec.started.lock().unwrap().is_some());
    }

    /// A gateway that speaks another protocol is refused by name, and the
    /// plugin never starts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn another_protocol_is_refused_before_start() {
        let rec = Arc::new(Rec::default());
        let (gw, _served) = wired(rec.clone());
        let e = gw
            .call(method::INITIALIZE, init(PROTOCOL + 1))
            .await
            .unwrap_err();
        assert_eq!(e.code, RpcError::REFUSED);
        assert!(e.message.contains("protocol"), "{e}");
        assert!(rec.started.lock().unwrap().is_none());
    }

    /// Nothing is delivered to a plugin that has not been initialized.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delivery_before_initialize_is_refused() {
        let (gw, _served) = wired(Arc::new(Rec::default()));
        let msg = OutboundMessage::plain("c", "t", MessageKind::Reply);
        let e = gw
            .call(method::DELIVER, serde_json::to_value(msg).unwrap())
            .await
            .unwrap_err();
        assert_eq!(e.code, RpcError::REFUSED);
    }

    /// `shutdown` runs the plugin's own shutdown and ends the loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_ends_the_loop() {
        let rec = Arc::new(Rec::default());
        let (gw, served) = wired(rec.clone());
        gw.call(method::INITIALIZE, init(PROTOCOL)).await.unwrap();
        let _ = gw.call(method::SHUTDOWN, json!({"reason": "reload"})).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), served)
            .await
            .expect("the loop did not end")
            .unwrap();
        assert_eq!(rec.stopped.lock().unwrap().as_deref(), Some("reload"));
    }

    /// The gateway vanishing (EOF) also runs the plugin's shutdown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_gateway_going_away_runs_shutdown() {
        let rec = Arc::new(Rec::default());
        let (g, p) = tokio::io::duplex(1 << 16);
        let (pr, pw) = tokio::io::split(p);
        let served = tokio::spawn(serve(manifest(), rec.clone(), pr, pw));
        drop(g);
        tokio::time::timeout(std::time::Duration::from_secs(5), served)
            .await
            .expect("the loop did not end")
            .unwrap();
        let why = rec.stopped.lock().unwrap().clone().unwrap_or_default();
        assert!(why.contains("went away"), "{why}");
    }
}
