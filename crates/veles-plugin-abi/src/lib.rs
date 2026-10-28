//! veles-plugin-abi — the C ABI contract for out-of-tree native plugins
//! (ABI 4, 2026-10-09; PLAN-plugins-installer.md R8 / M8).
//!
//! This crate is deliberately TINY and dependency-free: it is the one
//! thing an out-of-tree plugin links. Linking veles-core would make a
//! plugin in-tree by definition (same crate graph, same allocator
//! assumptions, same version lock); a shared `#[repr(C)]` contract is
//! the standard way to keep the boundary honest. Rust authors should
//! link `veles-plugin-abi-sdk` instead, which wraps every one of these
//! raw shapes in safe code and catches panics at the boundary.
//!
//! # The boundary, in one paragraph
//!
//! A plugin is a `cdylib` that exports three symbols:
//!
//! - `veles_plugin_abi_version() -> u32` — the ABI this plugin was
//!   built against. The host reads it BEFORE anything else and refuses
//!   a plugin whose answer is not [`VELES_PLUGIN_ABI_VERSION`]; a
//!   plugin that does not export it is an ABI 3 (or older) plugin and
//!   is refused with a message that says so. No plugin code runs for a
//!   refusal.
//! - `veles_plugin_init(host, plugin) -> i32` — called once at load.
//!   `host` is the table of host functions (a `'static` on the host
//!   side, so keeping the pointer is sound) and `plugin` is THIS
//!   plugin's id, which every registrar and service takes as its first
//!   argument: that is how a registration made later — from a timer, a
//!   socket event, a handler — is attributed to the plugin rather than
//!   to whichever thread happened to run it. Returns 0 on success;
//!   anything else aborts the load (and the host purges whatever the
//!   plugin registered before failing).
//! - `veles_plugin_free_string(s)` — frees any string the PLUGIN
//!   allocated and returned to the host.
//!
//! # What changed from ABI 3
//!
//! ABI 3 was append-only: a plugin registered hooks, tools, commands, a
//! gateway filter and a provider driver at init, and that was all it
//! could own. ABI 4 is a NEW LAYOUT (reading it through an ABI 3
//! struct is UB, which is why the version is read from its own export
//! before init):
//!
//! - **Every resource is host-owned and handle-addressed.** Each
//!   registrar returns a [`VelesHandle`] (0 = refused; the journal says
//!   why) and [`VelesHostApi::unregister`] drops one. On unload the host
//!   purges EVERYTHING the plugin owns — registrations, platform
//!   routes, timers, sockets — by owner, whether or not the plugin
//!   cleaned up. `veles plugins unload|reload <name>` works on a `.so`.
//! - **Platforms.** [`VelesHostApi::register_platform`] makes the
//!   plugin a chat platform: the host claims the platform's outbound
//!   route and hands every outbound message to the plugin's `deliver`
//!   as JSON; the plugin publishes inbound traffic with
//!   [`VelesHostApi::platform_inbound`].
//! - **Host services.** Timers ([`VelesHostApi::timer_after`]) and TCP
//!   sockets with optional TLS ([`VelesHostApi::tcp_connect`]) run on
//!   the host's runtime and call back on blocking threads, so a plugin
//!   needs no runtime, no thread and no TLS stack of its own. HTTP
//!   stays an op (`host_call("http_get" | "http_post", …)`).
//! - **Async.** A tool may answer with a [`VelesFuture`] instead of a
//!   string: the host polls it as a native future (no blocking thread
//!   is parked), waking through the [`VelesWaker`] the plugin is given.
//!
//! # Calling the engine: `host_call`
//!
//! `VelesHostApi::host_call(op, args_json)` reaches every engine
//! operation the Tcl bridge offers — one table, `bridgeops::OP_NAMES`
//! in veles-core, with a taxonomy test that fails if the two surfaces
//! drift. `args_json` is a JSON ARRAY OF STRINGS (the arguments after
//! the op name, exactly as Tcl would pass them). The answer is a JSON
//! object, always one of:
//!
//! ```text
//!   {"ok":"<result string>"}
//!   {"error":"<message>"}
//! ```
//!
//! so there is no out-of-band failure signal to forget to check. The
//! returned pointer is HOST-allocated: hand it back to
//! `VelesHostApi::host_free_string`, never to `free` and never to your
//! own allocator.
//!
//! # String rules (the part that bites)
//!
//! Two directions, one rule: **whoever allocated it frees it.**
//!
//! - `*const c_char` INTO the plugin: borrowed for the duration of the
//!   call only. Copy it if you keep it.
//! - `*mut c_char` OUT of the plugin (hook verdicts, tool results,
//!   rewrites, command renders, a future's `take`): allocated by the
//!   plugin, freed by the host through your `veles_plugin_free_string`.
//!   NULL means "none".
//! - `*mut c_char` OUT of the host (`host_call` results): allocated by
//!   the host, freed by the host through `host_free_string`. NULL means
//!   the call could not even be attempted.
//! - All strings are UTF-8, NUL-terminated.
//! - Socket data is bytes, not strings: `(ptr, len)`, borrowed for the
//!   call.
//!
//! # Threading
//!
//! Handlers are SYNC and the engine runs them where blocking is
//! allowed: hook handlers, gateway filters, sync tool calls, command
//! renders, provider drivers, platform `deliver`, timer callbacks and
//! socket events all reach a plugin from a blocking thread, so
//! `host_call` is safe inside any of them. Socket events for one socket
//! arrive in order, one at a time. A `VelesFuture`'s `poll` runs on an
//! executor thread: do NOT block or `host_call` inside it — do the
//! blocking work on a timer or socket callback and let `poll` only
//! read the result.
//!
//! A plugin that keeps shared state must make it thread-safe; `ctx`
//! pointers are handed back unchanged on every call.
//!
//! # Panics abort the process
//!
//! A Rust `extern "C"` function that unwinds aborts. There is nothing
//! the host can do about it: `catch_unwind` on the host side cannot see
//! into your frame. Do not panic across this boundary — no `unwrap`,
//! no `expect`, no slice indexing on untrusted input. Return an error
//! string instead; every out-parameter in this file has a shape for
//! saying "no". The `veles-plugin-abi-sdk` crate wraps every export and
//! every callback in `catch_unwind` so this cannot happen by accident.

use std::ffi::{c_char, c_void};

/// The ABI this crate describes. A plugin exports it from
/// `veles_plugin_abi_version`; the host refuses anything else.
///
/// # Version history
///
/// **1** (2026-09-03) — `register_hook`, `register_tool`,
/// `register_gateway_filter`. Registration only: a plugin could be
/// called BY veles and could never call veles.
///
/// **2** (2026-09-04) — `host_call` and `host_free_string` open the
/// door the other way, and `register_command` joins the registrars.
///
/// **3** (2026-09-04) — `register_provider`. A plugin can supply a
/// WIRE, not just handlers on veles' own wires.
///
/// **4** (2026-10-09) — a new layout, not an extension of 3: the
/// version is its own export (read before init), every registration
/// returns a handle the host owns and purges on unload, a plugin id
/// attributes late registrations, `register_platform` +
/// `platform_inbound` make a plugin a chat platform, timers and TCP/TLS
/// sockets are host services, a tool may answer with a future. ABI 3
/// plugins are REFUSED (the USER's decision: 4 replaces 3) with a
/// message that names the rebuild.
pub const VELES_PLUGIN_ABI_VERSION: u32 = 4;

/// Exported symbol names (byte strings for `libloading::Library::get`).
pub const SYMBOL_ABI_VERSION: &[u8] = b"veles_plugin_abi_version\0";
pub const SYMBOL_INIT: &[u8] = b"veles_plugin_init\0";
pub const SYMBOL_FREE: &[u8] = b"veles_plugin_free_string\0";

/// The host's id for one loaded plugin, handed to `veles_plugin_init`
/// and passed back as the first argument of every registrar and
/// service. Never 0.
pub type VelesPluginId = u64;

/// A host-owned resource: a registration, a platform, a timer, a
/// socket. 0 means the call was refused (the journal says why).
pub type VelesHandle = u64;

/// Status codes the host's services answer with.
pub const VELES_OK: i32 = 0;
/// The handle is not one this plugin owns (or was already dropped).
pub const VELES_ERR_NO_SUCH_HANDLE: i32 = 1;
/// A NULL pointer, a bad string, malformed JSON — the journal has the
/// detail.
pub const VELES_ERR_BAD_ARGUMENT: i32 = 2;
/// The resource exists but could not do it (a closed socket, a
/// platform the plugin did not register).
pub const VELES_ERR_REFUSED: i32 = 3;

/// `veles_plugin_abi_version`.
pub type VelesAbiVersionFn = unsafe extern "C" fn() -> u32;
/// `veles_plugin_init`.
pub type VelesInitFn =
    unsafe extern "C" fn(host: *const VelesHostApi, plugin: VelesPluginId) -> i32;
/// `veles_plugin_free_string`.
pub type VelesFreeFn = unsafe extern "C" fn(s: *mut c_char);

/// Hook handler: args JSON in, optional verdict out (NULL = no opinion;
/// a non-empty string wins the hook, hermes transform semantics).
pub type VelesHookFn =
    unsafe extern "C" fn(args_json: *const c_char, ctx: *mut c_void) -> *mut c_char;

/// Sync tool call: args JSON in, result string out (never NULL — an
/// error is a string that says so).
pub type VelesToolCallFn =
    unsafe extern "C" fn(args_json: *const c_char, ctx: *mut c_void) -> *mut c_char;

/// Async tool call: args JSON in, a plugin-allocated [`VelesFuture`]
/// out (NULL = the call could not start; the host answers an error).
/// The host owns the future from here: it polls it, and calls its
/// `drop` exactly once when it is done with it — ready, or cancelled.
pub type VelesToolCallAsyncFn =
    unsafe extern "C" fn(args_json: *const c_char, ctx: *mut c_void) -> *mut VelesFuture;

/// Gateway filter decision codes.
pub const FILTER_NONE: i32 = 0;
pub const FILTER_SKIP: i32 = 1;
pub const FILTER_ALLOW: i32 = 2;
pub const FILTER_REWRITE: i32 = 3;

/// Gateway filter: (platform, chat_id, text) in, decision out. For
/// `FILTER_REWRITE`, set `*rewrite_out` to a plugin-allocated string.
pub type VelesFilterFn = unsafe extern "C" fn(
    platform: *const c_char,
    chat_id: *const c_char,
    text: *const c_char,
    rewrite_out: *mut *mut c_char,
    ctx: *mut c_void,
) -> i32;

/// Slash-command render: the raw text after the command name in,
/// the prompt text for the turn out (never NULL — an error is a string
/// that says so).
pub type VelesCommandRenderFn =
    unsafe extern "C" fn(args: *const c_char, ctx: *mut c_void) -> *mut c_char;

/// A provider driver's turn: the OpenAI-shaped request body in, an
/// OpenAI-shaped response object out (never NULL — a failure is
/// `{"error":"...","status":N}`).
///
/// To STREAM, call `host_call("provider_emit", "[<chunk json>]")` as
/// many times as you like before returning.
pub type VelesProviderChatFn =
    unsafe extern "C" fn(request_json: *const c_char, ctx: *mut c_void) -> *mut c_char;

/// A platform's outbound delivery: one outbound message as JSON
/// (`veles_types::bus::OutboundMessage`: `chat_id`, `text`, `reply_to`,
/// `kind`, …) in; [`VELES_OK`] when it was sent, anything else is
/// logged as a delivery failure. Runs on a blocking thread, one message
/// at a time per platform, in order.
pub type VelesDeliverFn =
    unsafe extern "C" fn(outbound_json: *const c_char, ctx: *mut c_void) -> i32;

/// A timer's callback. Runs once, on a blocking thread.
pub type VelesTimerFn = unsafe extern "C" fn(ctx: *mut c_void);

/// Socket event kinds for [`VelesSocketEventFn`].
pub const SOCK_CONNECTED: i32 = 0;
/// `data`/`len` carry the bytes read.
pub const SOCK_DATA: i32 = 1;
/// The peer closed (or `tcp_close` ran); no events follow.
pub const SOCK_CLOSED: i32 = 2;
/// `data`/`len` carry a UTF-8 message; no events follow.
pub const SOCK_ERROR: i32 = 3;

/// A socket's event callback: `(kind, data, len, ctx)`. `data` is
/// borrowed for the call (NULL with `len` 0 for `SOCK_CONNECTED` and
/// `SOCK_CLOSED`). Events for one socket arrive in order, one at a
/// time, on blocking threads.
pub type VelesSocketEventFn =
    unsafe extern "C" fn(kind: i32, data: *const u8, len: usize, ctx: *mut c_void);

/// A JSON callback (`register_json`): `input_json` in, borrowed for the
/// call; a plugin-allocated JSON (or text) answer out, freed with the
/// plugin's `veles_plugin_free_string`. NULL, or text starting `error:`,
/// is an error. Called on blocking threads.
pub type VelesJsonFn =
    unsafe extern "C" fn(input_json: *const c_char, ctx: *mut c_void) -> *mut c_char;

/// `register_json`'s `kind` for a service other plugins (or core) call
/// by name, or an override point's provider (PLAN-extension-api.md D2).
/// The callback's input is `{"params", "caller", "session_id", "next"}`;
/// `host_call("service_next", ["<next>", "<params json>"])` is what the
/// layer below would answer. `decl_json` is `{"description", "cap"}`.
pub const JSON_KIND_SERVICE: &str = "service";

/// The host's waker, handed to every `poll` and BORROWED for that call.
/// It is a reference-counted object with the shape of Rust's
/// `RawWakerVTable`: `clone(data)` answers a new reference (a new
/// `data` to use with the same functions), `wake(data)` asks the host
/// to poll again (the reference stays yours), `drop(data)` releases a
/// reference. A plugin that answers "pending" keeps a CLONE and wakes
/// it from wherever progress happens — a timer, a socket event, a
/// thread of its own — then drops it. A clone stays valid after the
/// future itself was dropped (waking it is then a no-op), so a late
/// wake can never dangle. Waking more than once is harmless.
#[repr(C)]
pub struct VelesWaker {
    pub clone: unsafe extern "C" fn(data: *mut c_void) -> *mut c_void,
    pub wake: unsafe extern "C" fn(data: *mut c_void),
    pub drop: unsafe extern "C" fn(data: *mut c_void),
    pub data: *mut c_void,
}

/// A plugin-allocated future the host drives. `poll` answers 0 while
/// pending and 1 once ready (the waker handed to it is how the plugin
/// asks to be polled again); `take` hands over the result string after
/// a ready poll (never NULL — an error is a string that says so; freed
/// through `veles_plugin_free_string`); `drop` frees `ctx` and the
/// struct itself (the host calls it exactly once, ready or cancelled,
/// and never touches the future after). `poll` runs on an executor
/// thread: never block or `host_call` inside it.
#[repr(C)]
pub struct VelesFuture {
    pub poll: unsafe extern "C" fn(ctx: *mut c_void, waker: *const VelesWaker) -> i32,
    pub take: unsafe extern "C" fn(ctx: *mut c_void) -> *mut c_char,
    pub drop: unsafe extern "C" fn(ctx: *mut c_void),
    pub ctx: *mut c_void,
}

/// A tool registration, valid for the duration of the `register_tool`
/// call: the host copies the strings. `name`/`description`/`schema_json`
/// are borrowed; `call`/`call_async` + `ctx` are kept. Exactly one of
/// `call` (sync, runs on a blocking thread) and `call_async` (answers a
/// [`VelesFuture`]) must be set; both set → `call_async` is used.
#[repr(C)]
pub struct VelesToolSpec {
    pub name: *const c_char,
    pub description: *const c_char,
    /// JSON Schema for the tool's args, as a JSON string.
    pub schema_json: *const c_char,
    pub call: Option<VelesToolCallFn>,
    pub call_async: Option<VelesToolCallAsyncFn>,
    pub ctx: *mut c_void,
}

/// A slash-command registration, valid for the duration of the
/// `register_command` call: the host copies the strings.
#[repr(C)]
pub struct VelesCommandSpec {
    pub name: *const c_char,
    pub description: *const c_char,
    pub render: VelesCommandRenderFn,
    pub ctx: *mut c_void,
}

/// A provider-driver registration, valid for the duration of the
/// `register_provider` call: the host copies the name. A
/// `[[custom_providers]]` entry reaches the driver with
/// `api_mode = "plugin"` and `driver = "<name>"`.
#[repr(C)]
pub struct VelesProviderSpec {
    pub name: *const c_char,
    pub chat: VelesProviderChatFn,
    pub ctx: *mut c_void,
}

/// A platform registration, valid for the duration of the
/// `register_platform` call: the host copies `key`. `key` is the
/// platform name inbound and outbound messages carry (`"soecho"`);
/// `public_feed` (0/1) says whether the platform is a public feed for
/// the engine's public-reply policy. `deliver` + `ctx` are kept.
#[repr(C)]
pub struct VelesPlatformSpec {
    pub key: *const c_char,
    pub public_feed: u8,
    pub deliver: VelesDeliverFn,
    pub ctx: *mut c_void,
}

/// The host's side of the contract, handed to `veles_plugin_init`.
/// Every function pointer is valid for the life of the process. The
/// struct is a NEW layout in ABI 4; `abi_version` is here only so a
/// plugin can double-check what it already proved by exporting its own
/// version.
#[repr(C)]
pub struct VelesHostApi {
    pub abi_version: u32,
    /// `size_of::<VelesHostApi>()` as the HOST that filled it was built:
    /// a field that starts at or past it does not exist on that host.
    /// After the first release the table only grows at its end, so a
    /// plugin checks this before using a field added after it was built
    /// (`register_json` and later).
    pub struct_size: usize,

    // ── registrars: each answers a handle the host owns ──────────────
    /// Register a hook handler. `hook_name` is borrowed for the call;
    /// unknown hook names are refused (0) with a warning (the taxonomy
    /// in hooks::KNOWN_HOOKS is enforced by the host).
    pub register_hook: unsafe extern "C" fn(
        plugin: VelesPluginId,
        hook_name: *const c_char,
        handler: VelesHookFn,
        ctx: *mut c_void,
    ) -> VelesHandle,
    /// Register a tool; the spec's strings are copied during the call.
    pub register_tool:
        unsafe extern "C" fn(plugin: VelesPluginId, spec: *const VelesToolSpec) -> VelesHandle,
    /// Register a slash command; the spec's strings are copied during
    /// the call. A plugin command wins over a same-named `.md` file.
    pub register_command:
        unsafe extern "C" fn(plugin: VelesPluginId, spec: *const VelesCommandSpec) -> VelesHandle,
    /// Register a provider driver — a WIRE. See `VelesProviderSpec`.
    pub register_provider:
        unsafe extern "C" fn(plugin: VelesPluginId, spec: *const VelesProviderSpec) -> VelesHandle,
    /// Register a gateway filter. A plugin may register several; they
    /// are asked in registration order until one decides.
    pub register_gateway_filter: unsafe extern "C" fn(
        plugin: VelesPluginId,
        filter: VelesFilterFn,
        ctx: *mut c_void,
    ) -> VelesHandle,
    /// Register a chat platform. See `VelesPlatformSpec`.
    pub register_platform:
        unsafe extern "C" fn(plugin: VelesPluginId, spec: *const VelesPlatformSpec) -> VelesHandle,
    /// Drop one registration, platform, timer or socket this plugin
    /// owns. A platform's route is released; a timer never fires; a
    /// socket is closed (its `SOCK_CLOSED` is still delivered).
    pub unregister: unsafe extern "C" fn(plugin: VelesPluginId, handle: VelesHandle) -> i32,

    // ── platform traffic ─────────────────────────────────────────────
    /// Publish one inbound message as JSON
    /// (`veles_types::bus::InboundMessage`: `platform`, `chat_id`,
    /// `sender`, `text`, optional `route`/`msg_id`/`sender_id`/
    /// `context`). `platform` must be one THIS plugin registered;
    /// anything else is `VELES_ERR_REFUSED`.
    pub platform_inbound:
        unsafe extern "C" fn(plugin: VelesPluginId, inbound_json: *const c_char) -> i32,

    // ── host services ────────────────────────────────────────────────
    /// Call `cb(ctx)` once, `ms` milliseconds from now, on a blocking
    /// thread. The handle is spent once it fires.
    pub timer_after: unsafe extern "C" fn(
        plugin: VelesPluginId,
        ms: u64,
        cb: VelesTimerFn,
        ctx: *mut c_void,
    ) -> VelesHandle,
    /// Cancel a timer that has not fired. `VELES_ERR_NO_SUCH_HANDLE`
    /// once it has.
    pub timer_cancel: unsafe extern "C" fn(plugin: VelesPluginId, handle: VelesHandle) -> i32,
    /// Open a TCP connection (`tls` = 1 wraps it in TLS with the host's
    /// trust roots, verifying `host` as the server name). Returns at
    /// once; `SOCK_CONNECTED` or `SOCK_ERROR` follows on the callback.
    pub tcp_connect: unsafe extern "C" fn(
        plugin: VelesPluginId,
        host: *const c_char,
        port: u16,
        tls: u8,
        on_event: VelesSocketEventFn,
        ctx: *mut c_void,
    ) -> VelesHandle,
    /// Queue bytes for sending; `data` is borrowed for the call.
    pub tcp_send: unsafe extern "C" fn(
        plugin: VelesPluginId,
        handle: VelesHandle,
        data: *const u8,
        len: usize,
    ) -> i32,
    /// Close a socket; `SOCK_CLOSED` is delivered, then nothing.
    pub tcp_close: unsafe extern "C" fn(plugin: VelesPluginId, handle: VelesHandle) -> i32,

    // ── the engine ───────────────────────────────────────────────────
    /// Call an engine operation. See this module's `host_call` section:
    /// `args_json` is a JSON array of strings, the answer is
    /// `{"ok":…}` or `{"error":…}`, and the returned pointer belongs to
    /// the HOST — free it with `host_free_string`.
    ///
    /// Call it from a handler, a timer or a socket event — those run on
    /// threads where blocking is allowed. From `veles_plugin_init` it is
    /// not dangerous, just too early to be useful (no tool registry, no
    /// gateway, no session yet). From a future's `poll` it is refused.
    pub host_call: unsafe extern "C" fn(op: *const c_char, args_json: *const c_char) -> *mut c_char,
    /// Free a string `host_call` returned. NULL is a no-op.
    pub host_free_string: unsafe extern "C" fn(s: *mut c_char),

    // ── generic registrars (added in place of a new field per kind) ──
    /// Register a JSON callback of `kind` under `name` (see
    /// [`JSON_KIND_SERVICE`]); `decl_json` describes it. Answers a
    /// handle (`unregister` drops it), or 0 for a kind this host does not
    /// know or a refused declaration (the journal says why).
    pub register_json: unsafe extern "C" fn(
        plugin: VelesPluginId,
        kind: *const c_char,
        name: *const c_char,
        decl_json: *const c_char,
        cb: VelesJsonFn,
        ctx: *mut c_void,
    ) -> VelesHandle,
}
