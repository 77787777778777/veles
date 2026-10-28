//! The engine: one QuickJS-ng runtime, a context per script, three
//! natives, and the `ScriptHost` impl the seam drives.
//!
//! WHERE JS RUNS. Every entry into the engine (`load`, `invoke`) is
//! spawned onto the module's instance runtime and awaited from wherever
//! the seam called: the interpreter's work stays on the instance's own
//! worker (M3), a teardown's `shutdown_timeout` drops it, and the
//! watchdog's clock has one thread to read. rquickjs serialises access
//! with its own async lock, so the `drive` task (which runs promise
//! jobs and the host-call futures between entries) and an entry never
//! overlap.
//!
//! A LOST WAKEUP. rquickjs's scheduler keeps ONE waker: whoever polled
//! it last (an entry's `async_with` or the drive task). A nested entry
//! — a script's callback awaiting a host call that invokes another
//! script, as a JS-provided service called from JS does — is its own
//! task; when it polls the scheduler last and then ends, the wake meant
//! for the outer call's pending host-call future goes to a finished
//! task, and nothing polls again (the outer callback "did not answer
//! within 60s", 2 runs in 5). So every entry KICKS the drive task when
//! it is done: the drive re-polls, takes the waker back, and runs what
//! was woken.
//!
//! A RUNAWAY SCRIPT. QuickJS asks the interrupt handler every few
//! thousand instructions; the [`Watchdog`] answers "stop" once a script
//! has run for [`JS_SLICE`] without pausing (an `await` that waits on
//! the engine is a pause: the handler is not asked while nothing runs,
//! so a gap in its calls resets the window). The interrupted call
//! answers an error; the engine and the other scripts carry on.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::function::Async;
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Ctx, Function, Module, Promise,
};
use veles_runtime::tokio;
use veles_runtime::veles_core;
use veles_runtime::veles_core::scripthost::{BoxFut, ScriptCx, ScriptHost, ScriptRef};

/// The longest a script may run WITHOUT pausing before it is
/// interrupted (`VELES_JS_SLICE_MS` overrides it: the tests use a
/// short one).
pub const JS_SLICE: Duration = Duration::from_secs(30);
/// A gap this long between two interrupt-handler calls means the
/// script was waiting, not running: the slice starts over.
const IDLE_GAP: Duration = Duration::from_secs(1);

fn js_slice() -> Duration {
    std::env::var("VELES_JS_SLICE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(JS_SLICE)
}
/// What a script's heap may grow to.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// How long `load` waits for the registrations a script's top level
/// started (`await`ed or not) to reach the engine.
const SETTLE: Duration = Duration::from_secs(20);

pub struct JsEngine {
    /// Itself: the seam hands out `&dyn ScriptHost`, and an entry needs
    /// an owned handle to spawn its work onto the instance runtime.
    me: std::sync::Weak<JsEngine>,
    rt: AsyncRuntime,
    handle: tokio::runtime::Handle,
    scripts: Mutex<BTreeMap<String, Loaded>>,
    watchdog: Arc<Watchdog>,
    started: Instant,
    /// Wakes the drive task after an entry (see the module doc).
    kick: Arc<tokio::sync::Notify>,
}

#[derive(Clone)]
struct Loaded {
    ctx: AsyncContext,
}

/// The interrupt handler's state: see the module doc.
struct Watchdog {
    epoch: Instant,
    slice_ms: u64,
    window_start_ms: AtomicU64,
    last_tick_ms: AtomicU64,
    tripped: AtomicBool,
}

impl Watchdog {
    fn new(slice: Duration) -> Arc<Self> {
        Arc::new(Watchdog {
            epoch: Instant::now(),
            slice_ms: slice.as_millis() as u64,
            window_start_ms: AtomicU64::new(0),
            last_tick_ms: AtomicU64::new(0),
            tripped: AtomicBool::new(false),
        })
    }
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }
    /// Called by QuickJS while a script runs. `true` interrupts it.
    fn tick(&self) -> bool {
        let now = self.now_ms();
        let last = self.last_tick_ms.swap(now, Ordering::SeqCst);
        if now.saturating_sub(last) > IDLE_GAP.as_millis() as u64 {
            self.window_start_ms.store(now, Ordering::SeqCst);
            self.tripped.store(false, Ordering::SeqCst);
            return false;
        }
        let start = self.window_start_ms.load(Ordering::SeqCst);
        if now.saturating_sub(start) > self.slice_ms {
            self.tripped.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }
    /// A fresh entry into the engine starts a fresh window.
    fn arm(&self) {
        let now = self.now_ms();
        self.window_start_ms.store(now, Ordering::SeqCst);
        self.last_tick_ms.store(now, Ordering::SeqCst);
        self.tripped.store(false, Ordering::SeqCst);
    }
    fn tripped(&self) -> bool {
        self.tripped.load(Ordering::SeqCst)
    }
}

impl JsEngine {
    /// Build the runtime. `handle` is the instance runtime every entry
    /// is spawned onto.
    pub async fn new(handle: tokio::runtime::Handle) -> Result<Arc<Self>, String> {
        let rt = AsyncRuntime::new().map_err(|e| format!("QuickJS runtime: {e}"))?;
        rt.set_memory_limit(MEMORY_LIMIT).await;
        let watchdog = Watchdog::new(js_slice());
        let wd = Arc::clone(&watchdog);
        rt.set_interrupt_handler(Some(Box::new(move || wd.tick())))
            .await;
        Ok(Arc::new_cyclic(|me| JsEngine {
            me: me.clone(),
            rt,
            handle,
            scripts: Mutex::new(BTreeMap::new()),
            watchdog,
            started: Instant::now(),
            kick: Arc::new(tokio::sync::Notify::new()),
        }))
    }

    /// The future that runs promise jobs and host-call futures while
    /// no entry holds the engine: spawn it on the instance runtime once.
    /// A kick re-polls it, so it holds the scheduler's waker again.
    pub fn drive(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let drive = self.rt.drive();
        let kick = Arc::clone(&self.kick);
        async move {
            tokio::pin!(drive);
            loop {
                tokio::select! {
                    () = &mut drive => break,
                    () = kick.notified() => {}
                }
            }
        }
    }

    /// The scripts loaded right now, by name.
    pub fn loaded(&self) -> Vec<String> {
        self.scripts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Drop every script's context (the module's stop).
    pub async fn unload_all(&self) -> usize {
        let all: Vec<Loaded> = {
            let mut m = self.scripts.lock().unwrap_or_else(|p| p.into_inner());
            let v: Vec<Loaded> = m.values().cloned().collect();
            m.clear();
            v
        };
        let n = all.len();
        drop(all);
        self.rt.run_gc().await;
        n
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// A context with the three natives bound and the prelude evaluated.
    async fn fresh_context(
        self: &Arc<Self>,
        script: &ScriptRef,
        pending: Arc<AtomicUsize>,
    ) -> Result<AsyncContext, String> {
        let ctx = AsyncContext::full(&self.rt)
            .await
            .map_err(|e| format!("QuickJS context: {e}"))?;
        let cx: Arc<ScriptCx> = Arc::clone(&script.cx);
        let owner = script.owner;
        let label = format!("{}:{}", script.kind, script.name);
        ctx.async_with(async move |ctx: Ctx<'_>| -> Result<(), String> {
            let globals = ctx.globals();
            // `veles.call`'s native: the op and its args, a JSON answer
            // `{ok}` or `{err}`; the in-flight count moves synchronously,
            // so a `load` can wait for every call the top level started.
            let call_cx = Arc::clone(&cx);
            let call_pending = Arc::clone(&pending);
            let call = Function::new(
                ctx.clone(),
                Async(move |op: String, args: Vec<String>| {
                    let cx = Arc::clone(&call_cx);
                    let pending = Arc::clone(&call_pending);
                    pending.fetch_add(1, Ordering::SeqCst);
                    async move {
                        let r = cx.call(&op, args).await;
                        pending.fetch_sub(1, Ordering::SeqCst);
                        match r {
                            Ok(v) => serde_json::json!({ "ok": v }).to_string(),
                            Err(e) => serde_json::json!({ "err": e }).to_string(),
                        }
                    }
                }),
            )
            .catch(&ctx)
            .map_err(describe)?;
            globals
                .set("__veles_call", call)
                .catch(&ctx)
                .map_err(describe)?;
            // The log native: a line under the script's owner, through
            // core's callsite (no tracing callsite of this module's own).
            let log_label = label.clone();
            let log = Function::new(ctx.clone(), move |level: String, text: String| {
                let lvl = match level.as_str() {
                    "error" => veles_runtime::tracing::Level::ERROR,
                    "warn" | "warning" => veles_runtime::tracing::Level::WARN,
                    "debug" => veles_runtime::tracing::Level::DEBUG,
                    "trace" => veles_runtime::tracing::Level::TRACE,
                    _ => veles_runtime::tracing::Level::INFO,
                };
                veles_core::lifecycle::with_thread_owner(owner, || {
                    veles_core::module::log(lvl, &format!("{log_label}: {text}"));
                });
            })
            .catch(&ctx)
            .map_err(describe)?;
            globals
                .set("__veles_log", log)
                .catch(&ctx)
                .map_err(describe)?;
            // The timers' native: a promise that resolves after `ms`.
            let sleep = Function::new(
                ctx.clone(),
                Async(move |ms: u64| async move {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }),
            )
            .catch(&ctx)
            .map_err(describe)?;
            globals
                .set("__veles_sleep", sleep)
                .catch(&ctx)
                .map_err(describe)?;
            ctx.eval::<(), _>(crate::PRELUDE)
                .catch(&ctx)
                .map_err(describe)?;
            Ok(())
        })
        .await?;
        Ok(ctx)
    }

    /// Evaluate the script's entry as an ES module in a fresh context,
    /// wait for its top-level registrations to land, and keep it.
    async fn load_script(self: Arc<Self>, script: ScriptRef) -> Result<String, String> {
        let source = std::fs::read_to_string(&script.entry)
            .map_err(|e| format!("{}: {e}", script.entry.display()))?;
        let typescript = crate::transpile::is_typescript(&script.entry);
        let code = crate::transpile::to_js(&script.entry, &source)?;
        let pending = Arc::new(AtomicUsize::new(0));
        let ctx = self.fresh_context(&script, Arc::clone(&pending)).await?;
        let module_name = script.entry.display().to_string();
        self.watchdog.arm();
        let wd = Arc::clone(&self.watchdog);
        let evaluated = ctx
            .async_with(async move |ctx: Ctx<'_>| -> Result<(), String> {
                let promise = Module::evaluate(ctx.clone(), module_name, code)
                    .catch(&ctx)
                    .map_err(describe)?;
                promise
                    .into_future::<()>()
                    .await
                    .catch(&ctx)
                    .map_err(|e| interrupted_or(&wd, describe(e)))
            })
            .await;
        self.kick.notify_one();
        evaluated?;
        // The top level may have called `veles.registerTool(…)` without
        // awaiting it: those calls are promises the drive task settles.
        let t0 = Instant::now();
        while pending.load(Ordering::SeqCst) > 0 {
            if t0.elapsed() > SETTLE {
                return Err(format!(
                    "{} host call(s) still unanswered {}s after the top level finished",
                    pending.load(Ordering::SeqCst),
                    SETTLE.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let old = self
            .scripts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(script.name.clone(), Loaded { ctx });
        drop(old);
        Ok(format!(
            "{} bytes{}",
            source.len(),
            if typescript {
                ", TypeScript stripped"
            } else {
                ""
            }
        ))
    }

    async fn invoke_script(
        self: Arc<Self>,
        name: String,
        callback: u64,
        payload: String,
    ) -> Result<String, String> {
        let loaded = self
            .scripts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&name)
            .cloned();
        let Some(loaded) = loaded else {
            return Err(format!("js:{name}: not loaded"));
        };
        self.watchdog.arm();
        let wd = Arc::clone(&self.watchdog);
        let out = loaded
            .ctx
            .async_with(async move |ctx: Ctx<'_>| -> Result<String, String> {
                let invoke: Function = ctx
                    .globals()
                    .get("__veles_invoke")
                    .catch(&ctx)
                    .map_err(describe)?;
                let promise: Promise = invoke
                    .call((callback, payload))
                    .catch(&ctx)
                    .map_err(describe)?;
                promise
                    .into_future::<String>()
                    .await
                    .catch(&ctx)
                    .map_err(|e| interrupted_or(&wd, describe(e)))
            })
            .await;
        self.kick.notify_one();
        out
    }
}

/// What a caught JS error reads like: the message and, when the engine
/// kept one, the stack.
fn describe(e: CaughtError<'_>) -> String {
    match e {
        CaughtError::Exception(ex) => {
            let msg = ex.message().unwrap_or_else(|| "exception".to_string());
            match ex.stack() {
                Some(stack) if !stack.trim().is_empty() => format!("{msg}\n{}", stack.trim_end()),
                _ => msg,
            }
        }
        CaughtError::Value(v) => format!("thrown: {v:?}"),
        CaughtError::Error(e) => e.to_string(),
    }
}

fn interrupted_or(wd: &Watchdog, otherwise: String) -> String {
    if wd.tripped() {
        format!(
            "interrupted: the script ran for more than {} ms without pausing",
            wd.slice_ms
        )
    } else {
        otherwise
    }
}

impl ScriptHost for JsEngine {
    fn kind(&self) -> &'static str {
        crate::KIND
    }

    fn load(&self, script: ScriptRef) -> BoxFut<'_, Result<String, String>> {
        Box::pin(async move {
            let me = self.self_arc()?;
            self.handle
                .spawn(me.load_script(script))
                .await
                .map_err(|e| format!("the js host is reloading ({e})"))?
        })
    }

    fn unload(&self, name: &str) -> BoxFut<'_, Result<String, String>> {
        let name = name.to_string();
        Box::pin(async move {
            let gone = self
                .scripts
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&name);
            match gone {
                Some(l) => {
                    drop(l);
                    Ok("context dropped".into())
                }
                None => Ok("no context".into()),
            }
        })
    }

    fn invoke(
        &self,
        name: &str,
        callback: u64,
        payload: String,
    ) -> BoxFut<'_, Result<String, String>> {
        let name = name.to_string();
        Box::pin(async move {
            let me = self.self_arc()?;
            self.handle
                .spawn(me.invoke_script(name, callback, payload))
                .await
                .map_err(|e| format!("the js host is reloading ({e})"))?
        })
    }
}

impl JsEngine {
    /// The engine is only ever held in an `Arc` (see `new`); an entry
    /// recovers it from the weak self-reference.
    fn self_arc(&self) -> Result<Arc<Self>, String> {
        self.me
            .upgrade()
            .ok_or_else(|| "the js host is gone".to_string())
    }
}

#[cfg(test)]
mod tests {
    //! In-process: the engine as the `js` host of the real seam, scripts
    //! under a scratch plugins root, the registrations read back through
    //! core. (The module form is proven by `veles module-host-test
    //! conformance-js`.)
    use super::*;
    use veles_core::plugins::{LoadedPlugin, PluginManifest};
    use veles_core::scripthost;

    /// The seam has ONE host per kind, process-wide, and these tests
    /// share a process: run them one at a time, each with its own
    /// engine as the `js` host for its duration.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn plugin(root: &std::path::Path, name: &str, entry: &str, body: &str) -> LoadedPlugin {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(entry), body).unwrap();
        LoadedPlugin {
            manifest: PluginManifest {
                name: name.into(),
                kind: "js".into(),
                provides_hooks: Vec::new(),
                entry: entry.into(),
                description: String::new(),
                api: None,
            },
            dir: d,
        }
    }

    async fn engine(owner: &'static str) -> Arc<JsEngine> {
        let e = JsEngine::new(tokio::runtime::Handle::current())
            .await
            .unwrap();
        veles_core::rt::spawn(e.drive());
        let host: Arc<dyn ScriptHost> = e.clone();
        veles_core::lifecycle::with_thread_owner(owner, || scripthost::register_host(host));
        e
    }

    /// Dispatch a global tool the way a session would.
    async fn call_tool(name: &str, args: serde_json::Value) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            veles_core::store::Store::open(&tmp.path().join("state.db")).unwrap(),
        ));
        let reg = veles_core::tools::builtin_registry(store, None, "lang-js-test");
        reg.dispatch(name, args).await
    }

    fn owned_by(owner: &str) -> Vec<String> {
        veles_core::tools::global_tools_with_owners()
            .into_iter()
            .filter(|(_, o)| *o == Some(owner))
            .map(|(n, _)| n)
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_typescript_plugin_registers_and_answers_through_the_seam() {
        let _serial = SERIAL.lock().await;
        let root = tempfile::tempdir().unwrap();
        let e = engine("module:lang-js-e1").await;
        let p = plugin(
            root.path(),
            "e1-demo",
            "plugin.ts",
            r#"
interface Args { text?: string }
enum Tag { Tool = "e1-tool:" }
// Not awaited on purpose: load must still wait for it.
veles.registerTool(
  { name: "e1_echo", description: "echo", schema: { type: "object", properties: { text: { type: "string" } } } },
  async (args: Args, sessionId: string) => `${Tag.Tool} ${args.text ?? ""} for ${sessionId}`,
);
await veles.registerCommand("e1cmd", "a command", (text: string) => `e1cmd rendered ${text}`);
veles.hook("post_llm_call", (args: { content?: string }) =>
  args.content === "e1-magic" ? { decision: "from-e1" } : undefined,
);
console.log("e1 loaded");
"#,
        );
        let line = scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &p)
            .await
            .unwrap();
        assert!(
            line.contains("js:e1-demo: loaded") && line.contains("TypeScript stripped"),
            "{line}"
        );
        assert_eq!(owned_by("js:e1-demo"), vec!["e1_echo".to_string()]);
        let out = call_tool("e1_echo", serde_json::json!({"text": "hi"})).await;
        assert!(out.starts_with("e1-tool: hi for "), "{out}");
        let cmd = veles_core::commands::plugin_command("e1cmd").unwrap();
        let rendered = veles_core::commands::render_plugin_async(&cmd, "now")
            .await
            .unwrap();
        assert_eq!(rendered, "e1cmd rendered now");
        let verdict = veles_core::rt::spawn_blocking(|| {
            veles_core::rustplugins::run_hooks("post_llm_call", r#"{"content":"e1-magic"}"#)
        })
        .await
        .unwrap();
        assert_eq!(verdict.as_deref(), Some(r#"{"decision":"from-e1"}"#));
        let none = veles_core::rt::spawn_blocking(|| {
            veles_core::rustplugins::run_hooks("post_llm_call", r#"{"content":"e1-plain"}"#)
        })
        .await
        .unwrap();
        assert_eq!(none, None);
        // A reload: one tool, the new context answers.
        scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &p)
            .await
            .unwrap();
        assert_eq!(owned_by("js:e1-demo").len(), 1);
        assert_eq!(e.loaded(), vec!["e1-demo".to_string()]);
        scripthost::unload_script("e1-demo").await.unwrap();
        assert!(owned_by("js:e1-demo").is_empty());
        assert!(e.loaded().is_empty());
        scripthost::purge_owner_hosts(Some("module:lang-js-e1"));
    }

    /// One script provides services, another calls them through the
    /// registry: the provider's gate applies, the caller is named, Rust
    /// in-process reaches the script's handler too, and the provider's
    /// unload withdraws its services.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scripts_provide_and_call_services_through_the_registry() {
        let _serial = SERIAL.lock().await;
        let root = tempfile::tempdir().unwrap();
        let e = engine("module:lang-js-e9").await;
        let prov = plugin(
            root.path(),
            "e9-prov",
            "plugin.ts",
            r#"
await veles.registerService(
  { name: "jstest.add", description: "adds", cap: "any" },
  async (p: { a: number; b: number }, who: { caller: string }) => ({ sum: p.a + p.b, caller: who.caller }),
);
await veles.registerService({ name: "jstest.internal" }, () => "secret");
"#,
        );
        scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &prov)
            .await
            .unwrap();
        let v = veles_core::services::call("jstest.add", serde_json::json!({"a": 2, "b": 3}))
            .await
            .unwrap();
        assert_eq!(v["sum"], 5, "{v}");
        assert_eq!(
            veles_core::services::provider("jstest.add").as_deref(),
            Some("js:e9-prov")
        );

        let user = plugin(
            root.path(),
            "e9-user",
            "plugin.js",
            r#"
await veles.registerCommand("e9call", "calls services", async () => {
  const r = await veles.serviceCall("jstest.add", { a: 1, b: 1 });
  let refused = "";
  try { await veles.serviceCall("jstest.internal", {}); } catch (err) { refused = String(err); }
  const listed = (await veles.services()).map((s) => s.name).filter((n) => n.startsWith("jstest."));
  return JSON.stringify({ r, refused, listed });
});
"#,
        );
        scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &user)
            .await
            .unwrap();
        let cmd = veles_core::commands::plugin_command("e9call").unwrap();
        let out = veles_core::commands::render_plugin_async(&cmd, "")
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("{out}"));
        assert_eq!(v["r"]["sum"], 2, "{out}");
        assert_eq!(v["r"]["caller"], "js:e9-user", "{out}");
        assert!(
            v["refused"]
                .as_str()
                .unwrap_or_default()
                .contains("internal service"),
            "{out}"
        );
        assert_eq!(
            v["listed"],
            serde_json::json!(["jstest.add", "jstest.internal"]),
            "{out}"
        );

        scripthost::unload_script("e9-prov").await.unwrap();
        assert!(!veles_core::services::available("jstest.add"));
        scripthost::unload_script("e9-user").await.unwrap();
        scripthost::purge_owner_hosts(Some("module:lang-js-e9"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scripts_error_is_the_load_error_and_contexts_do_not_share_globals() {
        let _serial = SERIAL.lock().await;
        let root = tempfile::tempdir().unwrap();
        let e = engine("module:lang-js-e2").await;
        let bad = plugin(
            root.path(),
            "e2-bad",
            "plugin.js",
            "globalThis.shared = 1;\nthrow new Error('e2 boom');\n",
        );
        let err = scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &bad)
            .await
            .unwrap_err();
        assert!(err.contains("e2 boom"), "{err}");
        assert!(e.loaded().is_empty());
        let other = plugin(
            root.path(),
            "e2-other",
            "plugin.js",
            "if (globalThis.shared !== undefined) throw new Error('globals leaked');\nawait veles.registerCommand('e2ok', 'ok', () => 'ok');\n",
        );
        scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &other)
            .await
            .unwrap();
        assert!(veles_core::commands::plugin_command("e2ok").is_some());
        let ts_bad = plugin(root.path(), "e2-ts", "plugin.ts", "let x: number = ;\n");
        let err = scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &ts_bad)
            .await
            .unwrap_err();
        assert!(err.contains("plugin.ts:"), "{err}");
        scripthost::unload_script("e2-other").await.unwrap();
        scripthost::purge_owner_hosts(Some("module:lang-js-e2"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timers_fire_and_a_runaway_callback_is_interrupted() {
        let _serial = SERIAL.lock().await;
        std::env::set_var("VELES_JS_SLICE_MS", "400");
        let root = tempfile::tempdir().unwrap();
        let e = engine("module:lang-js-e3").await;
        std::env::remove_var("VELES_JS_SLICE_MS");
        let p = plugin(
            root.path(),
            "e3-timers",
            "plugin.js",
            r#"
await veles.registerTool({ name: "e3_wait", description: "waits" }, async () => {
  const t0 = Date.now();
  await new Promise((r) => setTimeout(r, 30));
  return `waited ${Date.now() - t0 >= 25 ? "ok" : "too little"}`;
});
await veles.registerTool({ name: "e3_spin", description: "never pauses" }, () => { for (;;) {} });
await veles.registerTool({ name: "e3_after", description: "still answers" }, () => "alive");
"#,
        );
        scripthost::load_script(Arc::clone(&e) as Arc<dyn ScriptHost>, &p)
            .await
            .unwrap();
        assert_eq!(
            call_tool("e3_wait", serde_json::json!({})).await,
            "waited ok"
        );
        let t0 = Instant::now();
        let out = call_tool("e3_spin", serde_json::json!({})).await;
        assert!(out.contains("interrupted"), "{out}");
        assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
        assert_eq!(call_tool("e3_after", serde_json::json!({})).await, "alive");
        scripthost::unload_script("e3-timers").await.unwrap();
        scripthost::purge_owner_hosts(Some("module:lang-js-e3"));
    }
}
