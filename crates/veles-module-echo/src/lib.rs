// The spike's module does on purpose what a module must not do in
// production: a raw tokio::spawn, to prove the host's teardown drops it.
#![allow(clippy::disallowed_methods)]
//! veles-module-echo — the R1 spike module (PLAN-plugins-installer.md R1).
//!
//! A dylib on `libveles_runtime.so`, declared with `veles_core::export!`.
//! Besides the export it has one probe per spike check, each a
//! `#[no_mangle]` Rust-ABI function the harness (`veles module-spike`)
//! looks up after dlopen. The point of every probe is that the work runs
//! HERE, in module code: the harness only calls and observes.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use veles_runtime::anyhow;
use veles_runtime::tokio;
use veles_runtime::tracing;
use veles_runtime::veles_core;

/// Which build of the module this is. The live-rebuild check edits this
/// line, rebuilds, and reloads into the running process.
const SPIKE_VERSION: &str = "echo-1";

fn manifest() -> veles_runtime::veles_types::plugin::Manifest {
    veles_runtime::veles_types::plugin::Manifest {
        name: "echo-module".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: 1,
        description:
            "R1 spike module: shared statics, contained runtime, unload, reload, refusal, panics."
                .into(),
        instances: veles_runtime::veles_types::plugin::Instances::Single,
        // A lease when the test asks for one (`module-host-test
        // standby-quit`), so the instance can stand by for it.
        lease: std::env::var_os("VELES_ECHO_LEASE").map(|_| "singleton:echo-module".into()),
        platforms: Vec::new(),
        config_sections: Vec::new(),
        knobs: Vec::new(),
        secrets: Vec::new(),
        services: Vec::new(),
        uses: Vec::new(),
        hooks: Vec::new(),
        enabled_env: None,
        required_env: None,
        points: Vec::new(),
    }
}

#[derive(Default)]
pub struct EchoModule;

impl veles_core::rustplugins::VelesPlugin for EchoModule {
    fn name(&self) -> &str {
        "echo-module"
    }
    fn version(&self) -> &str {
        SPIKE_VERSION
    }

    /// The conformance suite's hook (R8): a verdict on `echo-magic`,
    /// no opinion otherwise — the shape the ABI 4 sample has too.
    fn hooks(&self) -> Vec<(String, veles_core::rustplugins::RustHookFn)> {
        vec![(
            "post_llm_call".to_string(),
            Arc::new(|args: &str| {
                if args.contains("echo-magic") {
                    Some(r#"{"decision":"saw-it-from-echo"}"#.to_string())
                } else {
                    None
                }
            }),
        )]
    }

    /// The conformance suite's tool: answers with its marker. (No
    /// panicking tool here: a module's tool runs on the CALLER's task,
    /// so a panic in it is the caller's — the M5 policy contains panics
    /// on the module's own threads, not this path.)
    fn tools(&self) -> Vec<Arc<dyn veles_core::tools::Tool>> {
        vec![Arc::new(EchoTool) as Arc<dyn veles_core::tools::Tool>]
    }

    /// R3: the module host's start. Registers a command under the
    /// instance's owner (the thread tag), parks a stray `tokio::spawn`
    /// the teardown must drop, and returns — the tasks live on the
    /// instance runtime, which is what `shutdown_timeout` consumes.
    fn start(
        &self,
        cx: veles_core::rustplugins::ModuleContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            // A layer (R7), installed from the instance thread like the
            // command below: the host test reads `layers::installed()`
            // for this owner and expects it gone after a stop.
            struct EchoPromptLayer;
            impl veles_core::layers::PromptLayer for EchoPromptLayer {
                fn build(
                    &self,
                    cx: &veles_core::layers::PromptCx<'_>,
                    next: veles_core::layers::NextPrompt<'_>,
                ) -> String {
                    format!("{}\n\n[echo-module {SPIKE_VERSION}]", next.build(cx))
                }
            }
            veles_core::layers::install_prompt(std::sync::Arc::new(EchoPromptLayer));
            // A command, registered from an instance thread so its owner
            // is the instance (M4: the tag is read from the thread).
            veles_core::commands::register_command(veles_core::commands::PluginCommand {
                name: "echo-module".into(),
                description: "the R3 echo module".into(),
                body: veles_core::commands::CommandBody::Native(std::sync::Arc::new(|_args| {
                    format!("echo-module {SPIKE_VERSION}")
                })),
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            // An override of the conformance suite's point
            // (PLAN-extension-api.md D2): what the layer below answers,
            // with "module" after it. Registered from the instance thread,
            // so the teardown's purge takes it and core answers again.
            veles_core::services::register(
                veles_core::services::ServiceDecl {
                    name: "conformance.point".into(),
                    description: "the conformance suite's point, wrapped".into(),
                    cap: None,
                },
                veles_core::services::handler(|params, cx| async move {
                    let mut v = cx.next(params).await?;
                    match v.get_mut("via").and_then(serde_json::Value::as_array_mut) {
                        Some(via) => via.push(serde_json::json!("module")),
                        None => return Err(format!("no `via` list in {v}")),
                    }
                    Ok(v)
                }),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            // A stray task the teardown must drop (the spike's check 3
            // shape, productionised): it parks forever holding a guard
            // whose Drop flips a flag the tests read.
            cx.handle.spawn(async move {
                struct Guard;
                impl Drop for Guard {
                    fn drop(&mut self) {
                        STRAY_DROPPED.store(true, Ordering::SeqCst);
                    }
                }
                let _g = Guard;
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
            });
            // A task that never yields, when the test asks for one
            // (`module-host-test spin-keeps-mapped`): no shutdown can stop
            // it, so its thread outlives the teardown still running this
            // module's code, and the host must keep the library mapped.
            if std::env::var_os("VELES_ECHO_SPIN").is_some() {
                cx.handle.spawn(async {
                    loop {
                        std::hint::spin_loop();
                    }
                });
            }
            // A blocking thread that never ends but leaves the worker
            // free, like lang-lisp's engine joiner (`module-host-test
            // quit-cut-short`): stop returns, and the teardown waits out
            // the runtime's shutdown. It wakes every 20 ms into this
            // module's code, as ECL's engine thread did when its library
            // was unmapped under it.
            if std::env::var_os("VELES_ECHO_BLOCKING").is_some() {
                drop(cx.handle.spawn_blocking(|| loop {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }));
            }
            STRAY_DROPPED.store(false, Ordering::SeqCst);
            STOPPED.lock().unwrap_or_else(|p| p.into_inner()).clear();
            Ok(())
        })
    }

    /// R3: stop is a courtesy with a deadline. Record that it fired and
    /// with what reason; the teardown does not depend on it.
    fn stop(
        &self,
        reason: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let reason = reason.to_string();
        Box::pin(async move {
            *STOPPED.lock().unwrap_or_else(|p| p.into_inner()) = reason;
        })
    }
}

/// Whether teardown dropped the stray task `start` parked.
struct EchoTool;

#[async_trait::async_trait]
impl veles_core::tools::Tool for EchoTool {
    fn name(&self) -> &str {
        "echo_tool"
    }
    fn description(&self) -> &str {
        "Echo the args back with a marker (the module tier's conformance tool)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}})
    }
    async fn call(&self, args: serde_json::Value) -> String {
        format!("echo_tool: {args}")
    }
}

pub static STRAY_DROPPED: AtomicBool = AtomicBool::new(false);
/// The last stop's reason (empty until the first stop).
pub static STOPPED: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

veles_core::export!(EchoModule, manifest());

/// Check 2: look up a command the HOST registered (one registry in the
/// process, or this finds nothing) and log through `tracing` (the host's
/// subscriber must receive it).
#[no_mangle]
pub fn veles_spike_statics(marker: &str) -> bool {
    let found = veles_core::commands::plugin_command(marker).is_some();
    tracing::info!(target: "veles_module_echo", marker, found, "statics probe");
    found
}

/// Check 2 without a module callsite: the same line through core's
/// logging function, so no `tracing` callsite static lives in this file's
/// mapping.
#[no_mangle]
pub fn veles_spike_log_via_core(marker: &str) {
    veles_core::module::log(tracing::Level::INFO, &format!("statics probe {marker}"));
}

/// Check 3. The host spawns this future on the instance runtime. In
/// module code: a loopback TCP round trip, and a raw `tokio::spawn` that
/// parks forever holding a guard. The guard sets `dropped` when the
/// runtime drops the task, which only the host's teardown can do.
/// Returns the thread the probe ran on and the thread the stray task
/// started on.
/// What `veles_spike_contained` returns: the probe's thread name and the
/// stray task's, or what failed.
pub type Contained = Pin<Box<dyn Future<Output = Result<(String, String), String>> + Send>>;

#[no_mangle]
pub fn veles_spike_contained(dropped: Arc<AtomicBool>) -> Contained {
    Box::pin(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let here = thread_name();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
            s.write_all(&buf).await.map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        });
        let mut c = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| e.to_string())?;
        c.write_all(b"ping").await.map_err(|e| e.to_string())?;
        let mut back = [0u8; 4];
        c.read_exact(&mut back).await.map_err(|e| e.to_string())?;
        server.await.map_err(|e| e.to_string())??;
        if &back != b"ping" {
            return Err(format!("echoed {back:?}"));
        }

        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let guard = Guard(dropped);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _guard = guard;
            let _ = tx.send(thread_name());
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
        let stray = rx.await.map_err(|e| e.to_string())?;
        Ok((here, stray))
    })
}

/// Check 5: give the calling thread a value in a module-owned
/// `thread_local!` whose type has a Drop impl (module code that runs at
/// thread exit).
#[no_mangle]
pub fn veles_spike_touch_tls() {
    struct Noisy;
    impl Drop for Noisy {
        fn drop(&mut self) {
            NOISY_DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }
    thread_local! {
        static PIN: std::cell::RefCell<Option<Noisy>> = const { std::cell::RefCell::new(None) };
    }
    PIN.with(|p| *p.borrow_mut() = Some(Noisy));
}

static NOISY_DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Checks 6 and 7: which build answers.
#[no_mangle]
pub fn veles_spike_version() -> String {
    SPIKE_VERSION.to_string()
}

/// Check 8: a panic located in module code.
#[no_mangle]
pub fn veles_spike_panic() {
    panic!("echo-module: a deliberate module panic (check 8)");
}

fn thread_name() -> String {
    std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_string()
}
