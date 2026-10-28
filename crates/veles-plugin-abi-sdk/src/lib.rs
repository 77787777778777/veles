//! Write a veles native plugin in safe Rust (ABI 4; PLAN R8 / M8).
//!
//! A native plugin is a `cdylib` the gateway dlopens. This crate turns
//! the raw C contract in `veles-plugin-abi` into closures and a
//! [`Host`] handle, and it makes the one rule that is UB on the raw
//! ABI — "never unwind across the boundary" — impossible to break by
//! accident: every export and every trampoline is wrapped in
//! `catch_unwind`, so a panicking handler answers an error (and files a
//! note) instead of aborting the gateway.
//!
//! ```ignore
//! use veles_plugin_abi_sdk::{veles_plugin, Host};
//!
//! fn init(host: &Host) -> Result<(), String> {
//!     host.register_tool("hello", "says hello", r#"{"type":"object"}"#, |args| {
//!         Ok(format!("hello: {args}"))
//!     });
//!     Ok(())
//! }
//!
//! veles_plugin!(init);
//! ```
//!
//! `Cargo.toml`: `crate-type = ["cdylib"]`, and this crate is the ONLY
//! dependency that touches veles. Linking veles-core would make the
//! plugin in-tree (same crate graph, same version lock).
//!
//! # What a plugin owns, and for how long
//!
//! Every registration answers a [`Handle`] the HOST owns. On unload the
//! host purges everything the plugin registered — tools, commands,
//! hooks, filters, providers, platforms, timers, sockets — by owner,
//! whether or not the plugin dropped them itself. [`Host::unregister`]
//! drops one early. The closures behind a registration are leaked on
//! purpose (one small allocation per registration): the host may drop
//! the registration without telling the plugin, so nothing here can
//! safely free them. A timer's closure is freed when it fires.
//!
//! # Threads
//!
//! Handlers run on the host's blocking threads, so they may take their
//! time and may call the engine ([`Host::call`]). A plugin that keeps
//! state shares it the usual way (`Mutex`, atomics): a handler may run
//! on any thread, and two handlers may run at once. Socket events for
//! one socket arrive one at a time, in order. An async tool's future is
//! polled on an executor thread: it must not block, and the work it
//! waits for is done elsewhere — a timer, a socket event, a thread the
//! plugin spawned — which wakes it.

#![forbid(unsafe_op_in_unsafe_fn)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::Mutex;

pub use veles_plugin_abi as abi;
use veles_plugin_abi::*;

/// A host-owned resource: a registration, a platform, a timer, a
/// socket. `0` means the host refused (the gateway journal says why).
pub type Handle = VelesHandle;

/// What the host answered when a service call did not succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The handle is not one this plugin owns (or was already dropped).
    NoSuchHandle,
    /// A bad string, malformed JSON — the journal has the detail.
    BadArgument,
    /// The resource exists but could not do it (a closed socket, a
    /// platform this plugin did not register).
    Refused,
    /// A code this SDK does not know (a newer host).
    Other(i32),
}

fn status(code: i32) -> Result<(), Status> {
    match code {
        VELES_OK => Ok(()),
        VELES_ERR_NO_SUCH_HANDLE => Err(Status::NoSuchHandle),
        VELES_ERR_BAD_ARGUMENT => Err(Status::BadArgument),
        VELES_ERR_REFUSED => Err(Status::Refused),
        other => Err(Status::Other(other)),
    }
}

/// A gateway filter's decision about an inbound message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// No opinion; the next filter (or the engine) decides.
    None,
    /// Drop the message.
    Skip,
    /// Let it through as it is.
    Allow,
    /// Replace its text.
    Rewrite(String),
}

/// One event on a socket opened with [`Host::tcp_connect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketEvent {
    Connected,
    Data(Vec<u8>),
    /// The peer closed, or `tcp_close` ran. Nothing follows.
    Closed,
    /// Nothing follows.
    Error(String),
}

/// The future an async tool answers with: `Send`, `'static`, and
/// executor-free — the host polls it.
pub type ToolFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'static>>;

/// The plugin's handle on the gateway: the host's function table plus
/// this plugin's id. `Copy`, so it can be stashed anywhere (a `static
/// OnceLock<Host>` is the usual home).
#[derive(Clone, Copy)]
pub struct Host {
    api: &'static VelesHostApi,
    id: VelesPluginId,
}

// SAFETY: the host table holds only function pointers and a u32, and
// the host guarantees it for the life of the process.
unsafe impl Send for Host {}
unsafe impl Sync for Host {}

/// What every trampoline carries: the closure and a way back to the
/// host, so a caught panic can be reported as a note.
struct Tramp<F: ?Sized> {
    host: Host,
    f: Box<F>,
}

fn leak<F: ?Sized>(host: Host, f: Box<F>) -> *mut c_void {
    Box::into_raw(Box::new(Tramp { host, f })) as *mut c_void
}

/// # Safety
/// `ctx` must be a pointer `leak` made for the same `F`, still live.
unsafe fn tramp<'a, F: ?Sized>(ctx: *mut c_void) -> &'a Tramp<F> {
    // SAFETY: the caller's contract.
    unsafe { &*(ctx as *const Tramp<F>) }
}

/// Borrow a host string for the call.
///
/// # Safety
/// `p` is NULL or a NUL-terminated string valid for the call.
unsafe fn borrow(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: the caller's contract.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// Hand a string to the host (freed through `veles_plugin_free_string`).
fn give(s: String) -> *mut c_char {
    CString::new(s.replace('\0', ""))
        .unwrap_or_default()
        .into_raw()
}

fn panic_text(e: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic (no message)".to_string()
    }
}

type ToolF = dyn Fn(&str) -> Result<String, String> + Send + Sync;
type AsyncF = dyn Fn(&str) -> ToolFuture + Send + Sync;
type HookF = dyn Fn(&str) -> Option<String> + Send + Sync;
type CmdF = dyn Fn(&str) -> String + Send + Sync;
type ProvF = dyn Fn(&str) -> String + Send + Sync;
type ServiceF = dyn Fn(&str) -> Result<String, String> + Send + Sync;
type FilterF = dyn Fn(&str, &str, &str) -> Filter + Send + Sync;
type DeliverF = dyn Fn(&str) -> Result<(), String> + Send + Sync;
type TimerF = Mutex<Option<Box<dyn FnOnce() + Send>>>;
type SocketF = dyn Fn(SocketEvent) + Send + Sync;

unsafe extern "C" fn tool_tramp(args: *const c_char, ctx: *mut c_void) -> *mut c_char {
    // SAFETY: ctx is the Tramp<ToolF> register_tool leaked.
    let t = unsafe { tramp::<ToolF>(ctx) };
    // SAFETY: the host's string, valid for the call.
    let args = unsafe { borrow(args) };
    give(match catch_unwind(AssertUnwindSafe(|| (t.f)(&args))) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => format!("error: {e}"),
        Err(e) => t.host.report_panic("a tool call", e),
    })
}

unsafe extern "C" fn async_tool_tramp(args: *const c_char, ctx: *mut c_void) -> *mut VelesFuture {
    // SAFETY: ctx is the Tramp<AsyncF> register_tool_async leaked.
    let t = unsafe { tramp::<AsyncF>(ctx) };
    // SAFETY: the host's string, valid for the call.
    let args = unsafe { borrow(args) };
    let fut: ToolFuture = match catch_unwind(AssertUnwindSafe(|| (t.f)(&args))) {
        Ok(fut) => fut,
        Err(e) => {
            let msg = t.host.report_panic("an async tool call", e);
            Box::pin(async move { Err(msg) })
        }
    };
    future::make(t.host, fut)
}

unsafe extern "C" fn hook_tramp(args: *const c_char, ctx: *mut c_void) -> *mut c_char {
    // SAFETY: ctx is the Tramp<HookF> register_hook leaked.
    let t = unsafe { tramp::<HookF>(ctx) };
    // SAFETY: the host's string, valid for the call.
    let args = unsafe { borrow(args) };
    match catch_unwind(AssertUnwindSafe(|| (t.f)(&args))) {
        Ok(Some(v)) => give(v),
        Ok(None) => std::ptr::null_mut(),
        Err(e) => {
            let _ = t.host.report_panic("a hook handler", e);
            std::ptr::null_mut()
        }
    }
}

unsafe extern "C" fn command_tramp(args: *const c_char, ctx: *mut c_void) -> *mut c_char {
    // SAFETY: ctx is the Tramp<CmdF> register_command leaked.
    let t = unsafe { tramp::<CmdF>(ctx) };
    // SAFETY: the host's string, valid for the call.
    let args = unsafe { borrow(args) };
    give(match catch_unwind(AssertUnwindSafe(|| (t.f)(&args))) {
        Ok(s) => s,
        Err(e) => t.host.report_panic("a command render", e),
    })
}

unsafe extern "C" fn service_tramp(input: *const c_char, ctx: *mut c_void) -> *mut c_char {
    // SAFETY: ctx is the Tramp<ServiceF> register_service leaked.
    let t = unsafe { tramp::<ServiceF>(ctx) };
    // SAFETY: the host's string, valid for the call.
    let input = unsafe { borrow(input) };
    give(match catch_unwind(AssertUnwindSafe(|| (t.f)(&input))) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => format!("error: {e}"),
        Err(e) => t.host.report_panic("a service call", e),
    })
}

unsafe extern "C" fn provider_tramp(req: *const c_char, ctx: *mut c_void) -> *mut c_char {
    // SAFETY: ctx is the Tramp<ProvF> register_provider leaked.
    let t = unsafe { tramp::<ProvF>(ctx) };
    // SAFETY: the host's string, valid for the call.
    let req = unsafe { borrow(req) };
    give(match catch_unwind(AssertUnwindSafe(|| (t.f)(&req))) {
        Ok(s) => s,
        Err(e) => {
            let msg = t.host.report_panic("a provider driver", e);
            format!("{{\"error\":{},\"status\":500}}", json::quote(&msg))
        }
    })
}

unsafe extern "C" fn filter_tramp(
    platform: *const c_char,
    chat_id: *const c_char,
    text: *const c_char,
    rewrite_out: *mut *mut c_char,
    ctx: *mut c_void,
) -> i32 {
    // SAFETY: ctx is the Tramp<FilterF> register_gateway_filter leaked.
    let t = unsafe { tramp::<FilterF>(ctx) };
    // SAFETY: the host's strings, valid for the call.
    let (p, c, x) = unsafe { (borrow(platform), borrow(chat_id), borrow(text)) };
    match catch_unwind(AssertUnwindSafe(|| (t.f)(&p, &c, &x))) {
        Ok(Filter::None) => FILTER_NONE,
        Ok(Filter::Skip) => FILTER_SKIP,
        Ok(Filter::Allow) => FILTER_ALLOW,
        Ok(Filter::Rewrite(s)) => {
            if !rewrite_out.is_null() {
                // SAFETY: the host's out-parameter.
                unsafe { *rewrite_out = give(s) };
            }
            FILTER_REWRITE
        }
        Err(e) => {
            let _ = t.host.report_panic("a gateway filter", e);
            FILTER_NONE
        }
    }
}

unsafe extern "C" fn deliver_tramp(msg: *const c_char, ctx: *mut c_void) -> i32 {
    // SAFETY: ctx is the Tramp<DeliverF> register_platform leaked.
    let t = unsafe { tramp::<DeliverF>(ctx) };
    // SAFETY: the host's string, valid for the call.
    let msg = unsafe { borrow(msg) };
    match catch_unwind(AssertUnwindSafe(|| (t.f)(&msg))) {
        Ok(Ok(())) => VELES_OK,
        Ok(Err(e)) => {
            let _ = t.host.note("platform delivery failed", &e);
            VELES_ERR_REFUSED
        }
        Err(e) => {
            let _ = t.host.report_panic("a platform delivery", e);
            VELES_ERR_REFUSED
        }
    }
}

unsafe extern "C" fn timer_tramp(ctx: *mut c_void) {
    // SAFETY: ctx is the Tramp<TimerF> timer_after leaked; the host
    // calls a timer exactly once, so it is ours to free afterwards.
    let t = unsafe { Box::from_raw(ctx as *mut Tramp<TimerF>) };
    let f = t.f.lock().ok().and_then(|mut g| g.take());
    if let Some(f) = f {
        if let Err(e) = catch_unwind(AssertUnwindSafe(f)) {
            let _ = t.host.report_panic("a timer", e);
        }
    }
}

unsafe extern "C" fn socket_tramp(kind: i32, data: *const u8, len: usize, ctx: *mut c_void) {
    // SAFETY: ctx is the Tramp<SocketF> tcp_connect leaked.
    let t = unsafe { tramp::<SocketF>(ctx) };
    let bytes = if data.is_null() || len == 0 {
        Vec::new()
    } else {
        // SAFETY: the host's buffer, valid for the call.
        unsafe { std::slice::from_raw_parts(data, len) }.to_vec()
    };
    let ev = match kind {
        SOCK_CONNECTED => SocketEvent::Connected,
        SOCK_DATA => SocketEvent::Data(bytes),
        SOCK_CLOSED => SocketEvent::Closed,
        SOCK_ERROR => SocketEvent::Error(String::from_utf8_lossy(&bytes).into_owned()),
        other => SocketEvent::Error(format!("unknown socket event kind {other}")),
    };
    if let Err(e) = catch_unwind(AssertUnwindSafe(|| (t.f)(ev))) {
        let _ = t.host.report_panic("a socket event", e);
    }
}

impl Host {
    /// This plugin's id with the host.
    pub fn id(&self) -> VelesPluginId {
        self.id
    }

    /// The raw host table, for anything this SDK does not wrap.
    pub fn api(&self) -> &'static VelesHostApi {
        self.api
    }

    /// Report a caught panic as a plugin note — the only channel a
    /// trampoline has — and keep going.
    fn report_panic(&self, what: &str, e: Box<dyn std::any::Any + Send>) -> String {
        let msg = panic_text(e);
        let _ = self.note(&format!("plugin panic in {what}"), &msg);
        format!("error: plugin panicked in {what}: {msg}")
    }

    // ── the engine ───────────────────────────────────────────────────

    /// Call an engine op (`bridgeops::OP_NAMES`: `call_llm`,
    /// `config_get`, `http_get`, `http_post`, `note`, `send_outbound`,
    /// `tool_call`, …) with its string arguments. `Ok(result)` or
    /// `Err(message)`, decoded from the host's `{"ok":…}` /
    /// `{"error":…}` answer.
    pub fn call(&self, op: &str, args: &[&str]) -> Result<String, String> {
        let mut arr = String::from("[");
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                arr.push(',');
            }
            arr.push_str(&json::quote(a));
        }
        arr.push(']');
        let c_op = CString::new(op.replace('\0', "")).map_err(|_| "NUL in op".to_string())?;
        let c_args = CString::new(arr).map_err(|_| "NUL in args".to_string())?;
        // SAFETY: both strings live for the call; the answer is host-
        // allocated and handed back to host_free_string below.
        let out = unsafe { (self.api.host_call)(c_op.as_ptr(), c_args.as_ptr()) };
        if out.is_null() {
            return Err("host_call returned NULL".into());
        }
        // SAFETY: a NUL-terminated string the host allocated.
        let s = unsafe { CStr::from_ptr(out) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: the host's own allocation, freed exactly once.
        unsafe { (self.api.host_free_string)(out) };
        if let Some(ok) = json::field(&s, "ok") {
            Ok(ok)
        } else if let Some(err) = json::field(&s, "error") {
            Err(err)
        } else {
            Err(format!(
                "host_call answered something that is not {{ok}}/{{error}}: {s}"
            ))
        }
    }

    /// File a plugin note (`veles::note`): a summary and a detail.
    pub fn note(&self, summary: &str, detail: &str) -> Result<(), String> {
        self.call("note", &[summary, detail]).map(|_| ())
    }

    // ── registrars ───────────────────────────────────────────────────

    /// Register a model-callable tool. `schema_json` is the JSON Schema
    /// of its arguments; `f` gets the arguments as a JSON string and
    /// answers the tool result (`Err` becomes an error string the model
    /// sees). Runs on a blocking thread.
    pub fn register_tool<F>(&self, name: &str, description: &str, schema_json: &str, f: F) -> Handle
    where
        F: Fn(&str) -> Result<String, String> + Send + Sync + 'static,
    {
        let (Ok(c_name), Ok(c_desc), Ok(c_schema)) = (
            CString::new(name),
            CString::new(description),
            CString::new(schema_json),
        ) else {
            return 0;
        };
        let spec = VelesToolSpec {
            name: c_name.as_ptr(),
            description: c_desc.as_ptr(),
            schema_json: c_schema.as_ptr(),
            call: Some(tool_tramp),
            call_async: None,
            ctx: leak(*self, Box::new(f) as Box<ToolF>),
        };
        // SAFETY: the spec and its strings live for the call.
        unsafe { (self.api.register_tool)(self.id, &spec) }
    }

    /// Register an async tool: `f` answers a [`ToolFuture`] the host
    /// polls (no blocking thread is parked while it waits). The future
    /// must not block; it waits on things that wake it — a timer, a
    /// socket event, a thread of the plugin's own.
    pub fn register_tool_async<F>(
        &self,
        name: &str,
        description: &str,
        schema_json: &str,
        f: F,
    ) -> Handle
    where
        F: Fn(&str) -> ToolFuture + Send + Sync + 'static,
    {
        let (Ok(c_name), Ok(c_desc), Ok(c_schema)) = (
            CString::new(name),
            CString::new(description),
            CString::new(schema_json),
        ) else {
            return 0;
        };
        let spec = VelesToolSpec {
            name: c_name.as_ptr(),
            description: c_desc.as_ptr(),
            schema_json: c_schema.as_ptr(),
            call: None,
            call_async: Some(async_tool_tramp),
            ctx: leak(*self, Box::new(f) as Box<AsyncF>),
        };
        // SAFETY: the spec and its strings live for the call.
        unsafe { (self.api.register_tool)(self.id, &spec) }
    }

    /// Register a lifecycle hook handler (`post_llm_call`,
    /// `pre_tool_call`, …; `veles plugins hooks` lists them). `f` gets
    /// the hook's args JSON and answers a verdict, or `None` for no
    /// opinion; the first non-empty verdict wins the hook.
    pub fn register_hook<F>(&self, hook: &str, f: F) -> Handle
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        let Ok(c_hook) = CString::new(hook) else {
            return 0;
        };
        // SAFETY: the name lives for the call; the trampoline matches ctx.
        unsafe {
            (self.api.register_hook)(
                self.id,
                c_hook.as_ptr(),
                hook_tramp,
                leak(*self, Box::new(f) as Box<HookF>),
            )
        }
    }

    /// Register a slash command: `f` gets the text after the command
    /// name and answers the prompt text for the turn.
    pub fn register_command<F>(&self, name: &str, description: &str, f: F) -> Handle
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        let (Ok(c_name), Ok(c_desc)) = (CString::new(name), CString::new(description)) else {
            return 0;
        };
        let spec = VelesCommandSpec {
            name: c_name.as_ptr(),
            description: c_desc.as_ptr(),
            render: command_tramp,
            ctx: leak(*self, Box::new(f) as Box<CmdF>),
        };
        // SAFETY: the spec and its strings live for the call.
        unsafe { (self.api.register_command)(self.id, &spec) }
    }

    /// Provide a service other plugins (or core) call by name; it is also
    /// how a `.so` overrides a point (PLAN-extension-api.md D2). `f` gets
    /// `{"params", "caller", "session_id", "next"}` as JSON and answers
    /// JSON (or text); an `Err` (or a panic) is the caller's error.
    /// Inside `f`, `self.call("service_next", &[next, params_json])` is
    /// what the layer below would answer. `cap` is who may call: "" =
    /// core and in-process modules only, "any", or an ACL capability.
    /// Answers 0 on a host too old for `register_json` (its
    /// `struct_size` says so) or when the host refuses the declaration.
    pub fn register_service<F>(&self, name: &str, description: &str, cap: &str, f: F) -> Handle
    where
        F: Fn(&str) -> Result<String, String> + Send + Sync + 'static,
    {
        if !self.has_register_json() {
            return 0;
        }
        let decl = format!(
            "{{\"description\":{},\"cap\":{}}}",
            json::quote(description),
            json::quote(cap)
        );
        let (Ok(c_kind), Ok(c_name), Ok(c_decl)) = (
            CString::new(JSON_KIND_SERVICE),
            CString::new(name),
            CString::new(decl),
        ) else {
            return 0;
        };
        // SAFETY: the strings live for the call; the ctx is leaked for
        // the registration's life, as every registrar's is.
        unsafe {
            (self.api.register_json)(
                self.id,
                c_kind.as_ptr(),
                c_name.as_ptr(),
                c_decl.as_ptr(),
                service_tramp,
                leak(*self, Box::new(f) as Box<ServiceF>),
            )
        }
    }

    /// Does the host's table reach `register_json`? A field added after
    /// ABI 4.0 exists only on a host whose `struct_size` covers it.
    fn has_register_json(&self) -> bool {
        let end =
            std::mem::offset_of!(VelesHostApi, register_json) + std::mem::size_of::<VelesJsonFn>();
        self.api.struct_size >= end
    }

    /// Register a provider driver — a wire. `f` gets the OpenAI-shaped
    /// request body as JSON and answers an OpenAI-shaped response
    /// object (or `{"error":"…","status":N}`). To stream, call
    /// `host.call("provider_emit", &[chunk_json])` before answering.
    pub fn register_provider<F>(&self, name: &str, f: F) -> Handle
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        let Ok(c_name) = CString::new(name) else {
            return 0;
        };
        let spec = VelesProviderSpec {
            name: c_name.as_ptr(),
            chat: provider_tramp,
            ctx: leak(*self, Box::new(f) as Box<ProvF>),
        };
        // SAFETY: the spec and its string live for the call.
        unsafe { (self.api.register_provider)(self.id, &spec) }
    }

    /// Register a gateway filter: `f(platform, chat_id, text)` decides
    /// before the message is routed.
    pub fn register_gateway_filter<F>(&self, f: F) -> Handle
    where
        F: Fn(&str, &str, &str) -> Filter + Send + Sync + 'static,
    {
        // SAFETY: the trampoline matches ctx.
        unsafe {
            (self.api.register_gateway_filter)(
                self.id,
                filter_tramp,
                leak(*self, Box::new(f) as Box<FilterF>),
            )
        }
    }

    /// Become a chat platform named `key`. The host claims the
    /// platform's outbound route and hands every outbound message to
    /// `deliver` as JSON (`chat_id`, `text`, `reply_to`, `kind`, …);
    /// inbound traffic goes up through [`Host::platform_inbound`].
    /// `public_feed` marks a public feed for the engine's public-reply
    /// policy.
    pub fn register_platform<F>(&self, key: &str, public_feed: bool, deliver: F) -> Handle
    where
        F: Fn(&str) -> Result<(), String> + Send + Sync + 'static,
    {
        let Ok(c_key) = CString::new(key) else {
            return 0;
        };
        let spec = VelesPlatformSpec {
            key: c_key.as_ptr(),
            public_feed: u8::from(public_feed),
            deliver: deliver_tramp,
            ctx: leak(*self, Box::new(deliver) as Box<DeliverF>),
        };
        // SAFETY: the spec and its string live for the call.
        unsafe { (self.api.register_platform)(self.id, &spec) }
    }

    /// Drop one registration, platform, timer or socket this plugin
    /// owns. A platform's route is released; a timer never fires; a
    /// socket is closed (its `Closed` event is still delivered).
    pub fn unregister(&self, handle: Handle) -> Result<(), Status> {
        // SAFETY: a plain call.
        status(unsafe { (self.api.unregister)(self.id, handle) })
    }

    // ── platform traffic ─────────────────────────────────────────────

    /// Publish one inbound message for a platform this plugin
    /// registered. `inbound_json` is a `veles_types::bus::InboundMessage`:
    /// `platform`, `chat_id`, `sender`, `text`, and optionally `route`,
    /// `msg_id`, `sender_id`, `context`.
    pub fn platform_inbound(&self, inbound_json: &str) -> Result<(), Status> {
        let Ok(c) = CString::new(inbound_json) else {
            return Err(Status::BadArgument);
        };
        // SAFETY: the string lives for the call.
        status(unsafe { (self.api.platform_inbound)(self.id, c.as_ptr()) })
    }

    // ── host services ────────────────────────────────────────────────

    /// Run `f` once, `ms` milliseconds from now, on a blocking thread.
    pub fn timer_after<F>(&self, ms: u64, f: F) -> Handle
    where
        F: FnOnce() + Send + 'static,
    {
        let cell: Box<TimerF> = Box::new(Mutex::new(Some(Box::new(f) as Box<dyn FnOnce() + Send>)));
        // SAFETY: the trampoline matches ctx.
        unsafe { (self.api.timer_after)(self.id, ms, timer_tramp, leak(*self, cell)) }
    }

    /// Cancel a timer that has not fired yet.
    pub fn timer_cancel(&self, handle: Handle) -> Result<(), Status> {
        // SAFETY: a plain call.
        status(unsafe { (self.api.timer_cancel)(self.id, handle) })
    }

    /// Open a TCP connection (`tls` wraps it in TLS with the host's
    /// trust roots, verifying `host` as the server name). Returns at
    /// once; `on_event` gets `Connected` or `Error` first, then `Data`
    /// as bytes arrive, then `Closed` or `Error` once, after which
    /// nothing follows. Events for one socket arrive one at a time, in
    /// order.
    pub fn tcp_connect<F>(&self, host: &str, port: u16, tls: bool, on_event: F) -> Handle
    where
        F: Fn(SocketEvent) + Send + Sync + 'static,
    {
        let Ok(c_host) = CString::new(host) else {
            return 0;
        };
        // SAFETY: the string lives for the call; the trampoline matches ctx.
        unsafe {
            (self.api.tcp_connect)(
                self.id,
                c_host.as_ptr(),
                port,
                u8::from(tls),
                socket_tramp,
                leak(*self, Box::new(on_event) as Box<SocketF>),
            )
        }
    }

    /// Queue bytes on a socket.
    pub fn tcp_send(&self, handle: Handle, data: &[u8]) -> Result<(), Status> {
        // SAFETY: the slice lives for the call.
        status(unsafe { (self.api.tcp_send)(self.id, handle, data.as_ptr(), data.len()) })
    }

    /// Close a socket; `Closed` is delivered, then nothing.
    pub fn tcp_close(&self, handle: Handle) -> Result<(), Status> {
        // SAFETY: a plain call.
        status(unsafe { (self.api.tcp_close)(self.id, handle) })
    }
}

/// The async bridge: a plugin future wrapped as a [`VelesFuture`] the
/// host polls, and the host's waker wrapped as a std `Waker`.
mod future {
    use super::*;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    /// `vf` FIRST, so the pointer to the wrapper is the pointer to the
    /// `VelesFuture` the host holds; `ctx` points at the wrapper, so
    /// `drop(ctx)` frees the lot.
    #[repr(C)]
    struct Wrapper {
        vf: VelesFuture,
        host: Host,
        fut: Option<ToolFuture>,
        result: Option<String>,
    }

    pub(crate) fn make(host: Host, fut: ToolFuture) -> *mut VelesFuture {
        let w = Box::new(Wrapper {
            vf: VelesFuture {
                poll,
                take,
                drop: drop_future,
                ctx: std::ptr::null_mut(),
            },
            host,
            fut: Some(fut),
            result: None,
        });
        let raw = Box::into_raw(w);
        // SAFETY: freshly allocated, ours until the host's drop.
        unsafe { (*raw).vf.ctx = raw as *mut c_void };
        raw as *mut VelesFuture
    }

    /// The host's waker as a std `Waker`: the vtable maps 1:1 onto the
    /// `VelesWaker` functions, and the data pointer is a reference the
    /// host counts.
    struct Fns {
        clone: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
        wake: unsafe extern "C" fn(*mut c_void),
        drop: unsafe extern "C" fn(*mut c_void),
    }

    /// One `(functions, data)` pair the std vtable can carry: the raw
    /// waker's data pointer names this box.
    struct Ref {
        fns: &'static Fns,
        data: *mut c_void,
    }

    unsafe fn vt_clone(p: *const ()) -> RawWaker {
        // SAFETY: p is a Box<Ref> made below.
        let r = unsafe { &*(p as *const Ref) };
        // SAFETY: the host's clone function on a live reference.
        let data = unsafe { (r.fns.clone)(r.data) };
        let b = Box::new(Ref { fns: r.fns, data });
        RawWaker::new(Box::into_raw(b) as *const (), &VTABLE)
    }
    unsafe fn vt_wake(p: *const ()) {
        // SAFETY: p is a Box<Ref> made by vt_clone or the first wrap.
        unsafe {
            vt_wake_by_ref(p);
            vt_drop(p);
        }
    }
    unsafe fn vt_wake_by_ref(p: *const ()) {
        // SAFETY: p is a live Box<Ref>.
        let r = unsafe { &*(p as *const Ref) };
        // SAFETY: the host's wake function on a live reference.
        unsafe { (r.fns.wake)(r.data) };
    }
    unsafe fn vt_drop(p: *const ()) {
        // SAFETY: p is a live Box<Ref>, freed exactly once.
        let r = unsafe { Box::from_raw(p as *mut Ref) };
        // SAFETY: the host's drop function on a live reference.
        unsafe { (r.fns.drop)(r.data) };
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(vt_clone, vt_wake, vt_wake_by_ref, vt_drop);

    /// Interned function triples: the host hands the same three
    /// functions every time, but the std vtable needs a `'static` to
    /// point at, so each distinct triple is leaked once.
    fn intern(w: &VelesWaker) -> &'static Fns {
        static SEEN: Mutex<Vec<&'static Fns>> = Mutex::new(Vec::new());
        let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(f) = seen.iter().find(|f| {
            f.clone as usize == w.clone as usize
                && f.wake as usize == w.wake as usize
                && f.drop as usize == w.drop as usize
        }) {
            return f;
        }
        let f: &'static Fns = Box::leak(Box::new(Fns {
            clone: w.clone,
            wake: w.wake,
            drop: w.drop,
        }));
        seen.push(f);
        f
    }

    unsafe extern "C" fn poll(ctx: *mut c_void, waker: *const VelesWaker) -> i32 {
        // SAFETY: ctx is the Wrapper make() built, live until drop.
        let w = unsafe { &mut *(ctx as *mut Wrapper) };
        if w.result.is_some() {
            return 1;
        }
        let Some(fut) = w.fut.as_mut() else {
            w.result = Some("error: the future was polled after it ended".into());
            return 1;
        };
        let std_waker = if waker.is_null() {
            Waker::noop().clone()
        } else {
            // SAFETY: the host's waker, borrowed for this call; we take
            // our own reference through its clone.
            let vw = unsafe { &*waker };
            let fns = intern(vw);
            // SAFETY: the host's clone function on a live reference.
            let data = unsafe { (vw.clone)(vw.data) };
            let b = Box::new(Ref { fns, data });
            // SAFETY: the vtable's functions match the Box<Ref> shape.
            unsafe { Waker::from_raw(RawWaker::new(Box::into_raw(b) as *const (), &VTABLE)) }
        };
        let mut cx = Context::from_waker(&std_waker);
        match catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(&mut cx))) {
            Ok(Poll::Pending) => 0,
            Ok(Poll::Ready(Ok(s))) => {
                w.result = Some(s);
                w.fut = None;
                1
            }
            Ok(Poll::Ready(Err(e))) => {
                w.result = Some(format!("error: {e}"));
                w.fut = None;
                1
            }
            Err(e) => {
                w.result = Some(w.host.report_panic("an async tool's future", e));
                w.fut = None;
                1
            }
        }
    }

    unsafe extern "C" fn take(ctx: *mut c_void) -> *mut c_char {
        // SAFETY: ctx is the live Wrapper.
        let w = unsafe { &mut *(ctx as *mut Wrapper) };
        give(
            w.result
                .take()
                .unwrap_or_else(|| "error: the future was taken before it was ready".into()),
        )
    }

    unsafe extern "C" fn drop_future(ctx: *mut c_void) {
        // SAFETY: ctx is the Wrapper make() built; the host drops it
        // exactly once and never touches it after.
        let w = unsafe { Box::from_raw(ctx as *mut Wrapper) };
        let host = w.host;
        if let Err(e) = catch_unwind(AssertUnwindSafe(move || drop(w))) {
            let _ = host.report_panic("dropping an async tool's future", e);
        }
    }
}

/// Just enough JSON for the host's answers: quoting a string, and
/// reading one top-level string field. (The SDK is dependency-free on
/// purpose: a plugin brings its own JSON crate for its own data.)
pub mod json {
    /// `s` as a JSON string literal, quotes included.
    pub fn quote(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for ch in s.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    /// The string value of top-level `"key"` in a JSON object, or
    /// `None` when it is absent or not a string. Keys inside nested
    /// objects and arrays do not count.
    pub fn field(doc: &str, key: &str) -> Option<String> {
        let chars: Vec<char> = doc.chars().collect();
        let mut depth = 0i32;
        let mut expect_key = false;
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '{' => {
                    depth += 1;
                    expect_key = depth == 1;
                    i += 1;
                }
                '[' => {
                    depth += 1;
                    expect_key = false;
                    i += 1;
                }
                '}' | ']' => {
                    depth -= 1;
                    expect_key = false;
                    i += 1;
                }
                ',' => {
                    expect_key = depth == 1;
                    i += 1;
                }
                '"' => {
                    let (s, after) = read_string(&chars, i)?;
                    i = after;
                    if depth == 1 && expect_key {
                        let mut j = i;
                        while j < chars.len() && chars[j].is_whitespace() {
                            j += 1;
                        }
                        if j < chars.len() && chars[j] == ':' {
                            j += 1;
                            while j < chars.len() && chars[j].is_whitespace() {
                                j += 1;
                            }
                            if s == key {
                                return if j < chars.len() && chars[j] == '"' {
                                    read_string(&chars, j).map(|(v, _)| v)
                                } else {
                                    None
                                };
                            }
                            i = j;
                        }
                    }
                    expect_key = false;
                }
                _ => i += 1,
            }
        }
        None
    }

    /// Read the JSON string starting at `chars[at] == '"'`; answers the
    /// unescaped text and the index after the closing quote.
    fn read_string(chars: &[char], at: usize) -> Option<(String, usize)> {
        if chars.get(at) != Some(&'"') {
            return None;
        }
        let mut out = String::new();
        let mut i = at + 1;
        while i < chars.len() {
            match chars[i] {
                '"' => return Some((out, i + 1)),
                '\\' => {
                    i += 1;
                    match *chars.get(i)? {
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'u' => {
                            let hex: String = chars.get(i + 1..i + 5)?.iter().collect();
                            let code = u32::from_str_radix(&hex, 16).ok()?;
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                            i += 4;
                        }
                        other => out.push(other),
                    }
                    i += 1;
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        None
    }
}

/// The three exports, generated for the plugin. `$init` is a
/// `fn(&Host) -> Result<(), String>`: register things, keep the
/// [`Host`] somewhere, answer `Ok`. An `Err` or a panic fails the load
/// (the host purges what was registered) and files a note.
#[macro_export]
macro_rules! veles_plugin {
    ($init:expr) => {
        #[no_mangle]
        pub extern "C" fn veles_plugin_abi_version() -> u32 {
            $crate::abi::VELES_PLUGIN_ABI_VERSION
        }

        /// # Safety
        /// Called by the veles host with its own table and this
        /// plugin's id.
        #[no_mangle]
        pub unsafe extern "C" fn veles_plugin_init(
            host: *const $crate::abi::VelesHostApi,
            plugin: $crate::abi::VelesPluginId,
        ) -> i32 {
            unsafe { $crate::__init(host, plugin, $init) }
        }

        /// # Safety
        /// `s` is NULL or a string this plugin returned to the host.
        #[no_mangle]
        pub unsafe extern "C" fn veles_plugin_free_string(s: *mut ::std::ffi::c_char) {
            unsafe { $crate::__free_string(s) }
        }
    };
}

/// Behind `veles_plugin!`: the init export's body.
///
/// # Safety
/// `host` is NULL or the host's table, valid for the process.
#[doc(hidden)]
pub unsafe fn __init(
    host: *const VelesHostApi,
    plugin: VelesPluginId,
    f: fn(&Host) -> Result<(), String>,
) -> i32 {
    if host.is_null() {
        return 1;
    }
    // SAFETY: the caller's contract — a 'static on the host side.
    let api: &'static VelesHostApi = unsafe { &*host };
    if api.abi_version != VELES_PLUGIN_ABI_VERSION {
        return 2;
    }
    let h = Host { api, id: plugin };
    match catch_unwind(AssertUnwindSafe(|| f(&h))) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            let _ = h.note("plugin init failed", &e);
            3
        }
        Err(e) => {
            let _ = h.report_panic("init", e);
            4
        }
    }
}

/// Behind `veles_plugin!`: frees a string `give` handed out.
///
/// # Safety
/// `s` is NULL or a pointer `CString::into_raw` produced in this crate.
#[doc(hidden)]
pub unsafe fn __free_string(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: the caller's contract.
        drop(unsafe { CString::from_raw(s) });
    }
}

#[cfg(test)]
mod tests {
    use super::json;

    #[test]
    fn json_quote_and_field_round_trip() {
        let s = "a \"quoted\"\nline\twith \\ and é";
        let doc = format!("{{\"ok\":{}}}", json::quote(s));
        assert_eq!(json::field(&doc, "ok").as_deref(), Some(s));
        assert_eq!(json::field(&doc, "error"), None);
        assert_eq!(
            json::field(r#"{"error":"bad é thing"}"#, "error").as_deref(),
            Some("bad é thing")
        );
        // A nested "ok" is not the top-level one.
        assert_eq!(
            json::field(r#"{"error":"x","inner":{"ok":"no"}}"#, "ok"),
            None
        );
    }
}
