//! Tool registry.
//!
//! Tools are how the model acts. veles keeps hermes' builtin set as the
//! parity target. Registration paths that EXIST: the builtin tables
//! below, `register_global_tool` (adapter-provided tools, e.g. the IRC
//! surface), and MCP tools discovered at boot (`mcp__*`). The old claim
//! of a `veles::register_tool` Tcl bridge was never built — the bridge
//! exposes call_llm/routes/json_get only — and Rust plugins do not
//! exist yet either (see plugins.rs).

use crate::lockutil::{LockOk, RwLockOk};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    async fn call(&self, args: Value) -> String;
    /// The proc implementing this tool inside the plugins interpreter —
    /// `Some` only for Tcl-registered tools. The bridge runs these
    /// DIRECTLY on the interpreter thread: a channel round-trip from
    /// there would deadlock (the tool would post an Eval request to
    /// the very thread blocked waiting for it).
    fn tcl_proc(&self) -> Option<&str> {
        None
    }
    /// A session-bound clone of this tool, for tools registered
    /// GLOBALLY by an adapter rather than per session.
    ///
    /// Per-session tools take a `session_id` in their constructor and
    /// use it to find the caller's principal and sandbox zone. An
    /// adapter's tool is a process-wide singleton
    /// (`register_global_tool`), so it had no way to know who was
    /// calling — which is why the `irc` tool could not check the
    /// caller's capabilities and every action was open to anyone who
    /// could start a turn. Returning `Some` here gets the singleton
    /// replaced with a bound copy when a session registry is built.
    fn for_session(&self, _session_id: &str) -> Option<Arc<dyn Tool>> {
        None
    }
}

/// Where a tool lives: RESIDENT (its schema rides in every prompt) or
/// DEFERRED (discoverable via tool_search, describable via
/// tool_describe, invocable via tool_call — but not paying schema rent
/// every round). Deferring a tool costs a discovery round-trip the
/// first time the model needs it; that is the trade, and
/// `[tools.overrides]` promotes one back in one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Core,
    Deferred,
}

/// How a homed call came out (`ToolRegistry::run_homed`).
enum Homed {
    /// It ran elsewhere, or failed: the tool result.
    Out(String),
    /// Run it here, with a note naming the homes passed over, if any.
    Here(Option<String>),
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// Train-shaped names → registry names. Config overlays the
    /// defaults; dispatch resolves through it and NAMES the alias in
    /// the result.
    aliases: std::sync::RwLock<HashMap<String, String>>,
    /// This session's DEFERRED builtins — same contract as the global
    /// `deferred` registry (MCP tools), but per-session, because
    /// builtins like cronjob/delegate_task carry session state. Shared
    /// with the tool_search/tool_describe/tool_call instances that are
    /// registered alongside it.
    deferred: Arc<std::sync::Mutex<HashMap<String, crate::deferred::DeferredTool>>>,
    /// The session this registry belongs to, so `dispatch` can resolve
    /// the caller's principal and gate on it.
    ///
    /// The ACL check lived only in the agent's tool loop, so any OTHER
    /// caller of `dispatch` reached every tool ungated — `veles::
    /// tool_call` from a plugin most of all, which could run `terminal`
    /// on the host with neither a capability check nor an approval.
    /// Gating here makes the funnel point the boundary, the same
    /// reasoning that put the exec bound in `exec_argv`.
    ///
    /// Empty for a registry with no session (tests, and the owner-side
    /// construction paths): no principal, no gate, which is the
    /// pre-ACL behaviour every owner surface relies on.
    session_id: String,
    /// The store a session registry reads its `/tools home` rules and
    /// its frozen tool list from (remotetool.rs, D1). None: nothing is
    /// ever re-homed — a registry built bare stays on this node.
    store: Option<Arc<std::sync::Mutex<crate::store::Store>>>,
}

/// The bottom of the tool chain (`layers.rs`): a fn item, so it is
/// `Fn(ToolCx<'a>) -> BoxFuture<'a, String>` for every `'a`.
fn tool_base(cx: crate::layers::ToolCx<'_>) -> crate::layers::BoxFuture<'_, String> {
    Box::pin(async move { cx.registry.dispatch_default(&cx.name, cx.args).await })
}

// ── global extra tools (adapter-provided) ────────────────────────────
//
// Adapters that expose model-facing tools (veles-irc's `irc` tool)
// register them here at startup; every registry built afterwards picks
// them up. Kept in core so adapter crates (which depend ON core) can
// contribute without core depending on them.
//
// OWNER-TRACKED (M4/R2): each tool is stored as `Owned<Arc<dyn Tool>>`,
// attributed to the registering thread's owner (`lifecycle::thread_owner`).
// A builtin/adapter tool registers on an untagged core thread → owner
// `None`, never purged. A MODULE's tool registers on the module's tagged
// runtime thread → owner `Some(module)`, so a reload can `purge_owner` it
// without touching core's or another module's tools. The purge entry
// point is what R3's teardown drives.

static EXTRA_TOOLS: std::sync::OnceLock<crate::owned::OwnedRegistry<String, Arc<dyn Tool>>> =
    std::sync::OnceLock::new();

pub(crate) fn extra_tools() -> &'static crate::owned::OwnedRegistry<String, Arc<dyn Tool>> {
    EXTRA_TOOLS.get_or_init(crate::owned::OwnedRegistry::default)
}

/// Register an adapter-provided tool into all future session registries.
/// Sessions created BEFORE registration won't see it (acceptable: the
/// adapter wasn't up anyway); existing registries are unaffected.
///
/// REPLACE-BY-NAME, not append. This used to be a bare `push`, which
/// was invisible while only adapters called it — they register once at
/// startup. Plugins do not: a Tcl reload re-runs `veles::register_tool`
/// and `load_rust_manifests` re-dlopens every `kind = "rust"` plugin,
/// so every reload appended another copy of every plugin tool. Nothing
/// visibly broke, because `builtin_registry` inserts into a HashMap and
/// the last copy wins — the list simply grew forever, and each stale
/// `SoTool` kept its old `Library` mapped for the process lifetime.
///
/// Replacing also matches what `rustplugins::register_plugin` and
/// `commands::register_command` already promise about reloads, so all
/// three plugin registries now say the same thing.
///
/// The owner is read from the calling thread (M4): a module re-registering
/// its own tool after a reload re-attributes the key to the new instance.
pub fn register_global_tool(tool: Arc<dyn Tool>) {
    let name = tool.name().to_string();
    extra_tools().insert(name, tool);
}

/// Remove every tool registered by `owner`. Called by the module host's
/// teardown (R3) during a reload/unload. Returns how many were removed.
/// `None` (core) removes nothing — builtin tools are never purgeable.
pub fn purge_owner_tools(owner: Option<&'static str>) -> usize {
    extra_tools().purge(owner)
}

/// Drop one global tool `owner` registered (an ABI 4 plugin's
/// `unregister`); other owners' same-named tools stay.
pub fn remove_owned_tool(name: &str, owner: Option<&'static str>) -> bool {
    extra_tools().remove_owned(&name.to_string(), owner)
}

/// The registered extras, by name and in order.
///
/// A diagnostic rather than a lookup — `builtin_registry` folds these
/// into a HashMap, so duplicates are invisible from every other angle.
/// That is exactly what hid the reload leak this function exists to
/// pin, so the test that checks it needs to see the raw list.
pub fn global_tool_names() -> Vec<String> {
    extra_tools().keys()
}

/// Every global tool with the owner that registered it (tests and the
/// status surfaces: a `.so`'s tools carry `so:<name>`).
pub fn global_tools_with_owners() -> Vec<(String, Option<&'static str>)> {
    extra_tools()
        .values_with_owners()
        .into_iter()
        .map(|(t, o)| (t.name().to_string(), o))
        .collect()
}

/// What a session registry holds for one registered global tool.
///
/// A core tool is held directly (session-bound when it offers that). A
/// MODULE's tool is held late-bound (M3): the name and schema it had
/// when the session's registry was built, with the implementation looked
/// up on every call. A session then keeps no `Arc` of module code, which
/// would keep the module mapped as long as the session lived, and after
/// a reload its calls reach the new build.
fn session_tool(
    tool: Arc<dyn Tool>,
    owner: Option<&'static str>,
    session_id: &str,
) -> Arc<dyn Tool> {
    if owner.is_none() {
        return tool.for_session(session_id).unwrap_or(tool);
    }
    Arc::new(LateBoundTool {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        schema: tool.schema(),
        session_id: session_id.to_string(),
    })
}

struct LateBoundTool {
    name: String,
    description: String,
    schema: Value,
    session_id: String,
}

#[async_trait]
impl Tool for LateBoundTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    async fn call(&self, args: Value) -> String {
        let Some(tool) = extra_tools().with(&self.name, Arc::clone) else {
            return format!(
                "Error: tool `{}` is not available right now: the module that provides it is not loaded",
                self.name
            );
        };
        let tool = tool.for_session(&self.session_id).unwrap_or(tool);
        tool.call(args).await
    }
}

/// The LAST-published session registry, parked so process-global
/// surfaces without a session of their own (the plugin bridge's
/// `veles::tool_call`) can still reach real tools. Same cell pattern as
/// `hooks::DEFAULT_BUS`: `Agent::new` publishes; last agent wins.
static GLOBAL_REGISTRY: std::sync::OnceLock<std::sync::RwLock<Option<Arc<ToolRegistry>>>> =
    std::sync::OnceLock::new();

fn global_registry_cell() -> &'static std::sync::RwLock<Option<Arc<ToolRegistry>>> {
    GLOBAL_REGISTRY.get_or_init(|| std::sync::RwLock::new(None))
}

pub fn set_global_registry(reg: Arc<ToolRegistry>) {
    if let Ok(mut w) = global_registry_cell().write() {
        *w = Some(reg);
    }
}

pub fn global_registry() -> Option<Arc<ToolRegistry>> {
    global_registry_cell().read().ok()?.clone()
}

// ── per-session registries ───────────────────────────────────────────
//
// The global cell above answers "whose tools can a plugin reach", and
// its own doc comment admits the answer is "the LAST session that
// booted". For a `veles::tool_call` fired from a hook that is the wrong
// question: the call belongs to the session whose hook is running, and
// `dispatch` gates on `registry.session_id`'s principal — so a guest's
// plugin call was judged as whoever booted last (an owner, usually),
// and an owner's as a guest. This map keys the same registries by
// session so the caller can be answered with its OWN.
//
// BOUNDED, unlike the two other per-session maps (`acl::SESSION_
// PRINCIPALS`, `SESSION_ORIGINS` below): those hold a string and a
// principal, this holds an `Arc<ToolRegistry>` — every tool in the tree
// plus their handles. 256 live sessions is far past anything this box
// has seen; the oldest is evicted (insertion order) and its calls fall
// back per `plugin_call_registry`.

/// How many session registries are kept before the oldest is evicted.
pub const SESSION_REGISTRY_CAP: usize = 256;

#[derive(Default)]
struct SessionRegistries {
    /// Insertion order, oldest first — the eviction queue.
    order: Vec<String>,
    map: HashMap<String, Arc<ToolRegistry>>,
}

static SESSION_REGISTRIES: std::sync::OnceLock<std::sync::Mutex<SessionRegistries>> =
    std::sync::OnceLock::new();

fn session_registries() -> &'static std::sync::Mutex<SessionRegistries> {
    SESSION_REGISTRIES.get_or_init(|| std::sync::Mutex::new(SessionRegistries::default()))
}

/// Publish a session's registry so a plugin call FROM that session can
/// be judged as it. Called beside `set_global_registry` in
/// `Agent::new`; republishing a session (a resumed one builds a fresh
/// registry) refreshes the entry and moves it to the back of the
/// eviction queue.
pub fn publish_session_registry(session_id: &str, reg: Arc<ToolRegistry>) {
    if session_id.is_empty() {
        return;
    }
    let mut w = session_registries().lock_ok();
    if let Some(pos) = w.order.iter().position(|s| s == session_id) {
        w.order.remove(pos);
    }
    w.order.push(session_id.to_string());
    w.map.insert(session_id.to_string(), reg);
    while w.order.len() > SESSION_REGISTRY_CAP {
        let oldest = w.order.remove(0);
        w.map.remove(&oldest);
        tracing::debug!(
            "evicted the tool registry for session {oldest} ({} live, cap {})",
            w.order.len(),
            SESSION_REGISTRY_CAP
        );
    }
}

/// The registry a session's calls run through, when one is published.
pub fn session_registry(session_id: &str) -> Option<Arc<ToolRegistry>> {
    if session_id.is_empty() {
        return None;
    }
    session_registries().lock_ok().map.get(session_id).cloned()
}

/// Drop a session's registry (session end, and tests).
pub fn forget_session_registry(session_id: &str) {
    let mut w = session_registries().lock_ok();
    w.map.remove(session_id);
    w.order.retain(|s| s != session_id);
}

// ── the ambient hook session ─────────────────────────────────────────
//
// A plugin calling `veles::tool_call` from a hook handler does not know
// its own session unless the engine tells it. It is told two ways: the
// payload carries `session_id` (`hooks::with_session`), and the engine
// sets this thread-local for the duration of the handler call. Both
// handler flavours run the plugin ON the thread that dispatched the hook
// — `HookBus::fire` hops to a blocking thread for rust handlers and to
// the interpreter thread for Tcl ones, and the callback back into the
// engine (`host_call`, or a native `veles::` command) happens on that
// same thread — so one thread-local covers both.
//
// Best-effort by construction: a handler that spawns its own thread and
// calls back from it sees no ambient, and must pass `session_id`
// explicitly (the third `veles::tool_call` argument). That is why the
// explicit argument exists and why it wins.

thread_local! {
    static AMBIENT_SESSION: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// The session whose hook handler is running on THIS thread, if any.
pub fn ambient_hook_session() -> Option<String> {
    AMBIENT_SESSION.with(|c| c.borrow().clone())
}

/// Run `f` with `session_id` as the ambient hook session, restoring
/// whatever was there before — including on unwind, so a panicking
/// handler cannot leave a stale identity for the next one on this
/// thread to be judged by.
pub fn with_ambient_hook_session<T>(session_id: Option<&str>, f: impl FnOnce() -> T) -> T {
    struct Guard(Option<String>);
    impl Drop for Guard {
        fn drop(&mut self) {
            AMBIENT_SESSION.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let next = session_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let prior = AMBIENT_SESSION.with(|c| std::mem::replace(&mut *c.borrow_mut(), next));
    let _guard = Guard(prior);
    f()
}

/// Which registry a plugin tool call is judged by, and a description of
/// that identity for the note the owner sees.
///
/// Order: the session the caller NAMED, then the ambient hook session,
/// then the last-published global cell — the pre-existing behaviour, now
/// the fallback of last resort rather than the only answer.
///
/// A named session with no published registry is an ERROR, not a fall
/// through: a plugin that went to the trouble of naming one has told us
/// whose call this is, and answering it from somebody else's registry is
/// exactly the mistake this map exists to prevent. The ambient case
/// splits on whether that session has a principal: an evicted guest
/// session fails closed, while an owner surface (no principal, full
/// access by definition) falls through honestly.
pub fn plugin_call_registry(
    explicit: Option<&str>,
    ambient: Option<&str>,
) -> Result<(Arc<ToolRegistry>, String), String> {
    if let Some(sid) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return match session_registry(sid) {
            Some(reg) => Ok((reg, format!("session `{sid}`, named by the caller"))),
            None => Err(format!(
                "no tool registry is published for session `{sid}` — it never started, or it was \
                 evicted from the per-session map (cap {SESSION_REGISTRY_CAP}). Pass no session id \
                 to use the ambient one."
            )),
        };
    }
    if let Some(sid) = ambient.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(reg) = session_registry(sid) {
            return Ok((reg, format!("session `{sid}`, the hook now running")));
        }
        if crate::acl::session_principal(sid).is_some() {
            return Err(format!(
                "session `{sid}` has no published tool registry (evicted, cap \
                 {SESSION_REGISTRY_CAP}), and it has a principal — refusing to judge its call as \
                 another session's"
            ));
        }
    }
    match global_registry() {
        Some(reg) => Ok((
            reg,
            "the last-published session — no session was named and none is ambient".to_string(),
        )),
        None => Err("no tool registry published yet (no agent session has started)".to_string()),
    }
}

fn default_aliases() -> HashMap<String, String> {
    // Names models were RL-trained to emit, that have a real semantic
    // match in the registry. Deliberately conservative: an alias that
    // guesses wrong teaches the model a lie.
    let mut m = HashMap::new();
    let pairs: &[(&str, &str)] = &[
        ("read", "read_file"),
        ("write", "write_file"),
        ("edit", "patch"),
        ("apply_patch", "patch"),
        ("insert_content", "patch"),
        ("grep", "search_files"),
        ("bash", "terminal"),
        // `shell` deliberately ABSENT. It used to map here → terminal,
        // which shadowed the real ShellTool: the model could never reach
        // `shell` by that name, and terminal's own description pointed it
        // at a `shell` tool that resolved straight back to terminal (the
        // 2026-09 PENDING incident: a model tried `shell {action:kill}`,
        // got `[shell → terminal]` + empty output, concluded there was no
        // shell tool, and pkill'd cargo). A registered tool is never an
        // alias — see `resolve_alias`.
        ("run_command", "terminal"),
        // Claude Code's background-task tools (arguments reshaped in
        // `repair::convert_alias_shape`): read, wait for or stop what
        // terminal{background}/monitor/delegate started.
        ("bashoutput", "shell"),
        ("taskoutput", "shell"),
        ("killshell", "shell"),
        ("killbash", "shell"),
        ("taskstop", "shell"),
        ("code_interpreter", "execute_code"),
        ("execute", "execute_code"),
        ("web_fetch", "web_extract"),
        ("fetch_url", "web_extract"),
        ("websearch", "web_search"),
        ("web_search_preview", "web_search"),
        ("todo_write", "todo"),
        // OpenAI's str_replace_editor family — one tool with a
        // `command` discriminator upstream; every command it supports
        // maps onto `patch` or `read_file` here, and `patch` already
        // speaks old_string/new_string.
        ("str_replace_editor", "patch"),
        ("str_replace_based_edit_tool", "patch"),
        ("str_replace", "patch"),
        ("create_file", "write_file"),
        ("view_file", "read_file"),
        ("view", "read_file"),
        ("open_file", "read_file"),
        ("cat", "read_file"),
        // Search-shaped names. `glob` is a FILE-NAME search upstream,
        // which is what search_files does with a path pattern.
        ("glob", "search_files"),
        ("find_files", "search_files"),
        ("ripgrep", "search_files"),
        ("rg", "search_files"),
        ("search", "search_files"),
        // Shell-shaped.
        ("run_terminal_cmd", "terminal"),
        ("run_shell_command", "terminal"),
        ("exec", "terminal"),
        ("sh", "terminal"),
        // Subagent / planning shapes.
        ("task", "delegate_task"),
        ("dispatch_agent", "delegate_task"),
        ("todoread", "todo"),
        // Plan shapes. `exit_plan_mode` is Claude Code's ACTUAL tool
        // name for "present the plan, ask to leave plan mode", which is
        // exactly what `present_plan` is — a model RL'd on that harness
        // emits it in place of present_plan. In the registries that do
        // NOT carry present_plan (cron, A2A) the alias resolves to a
        // name that is not registered, and dispatch reports the
        // MODEL's own name as unknown — identical to today's outcome,
        // no lie.
        ("exit_plan_mode", "present_plan"),
        // The read-back, met by the names a model reaches for when it
        // wants "what was the plan" without re-reading the transcript.
        // Deliberately no single-word `plan` row: `plan` is also the
        // mode word, and an alias that stole it would teach a model
        // that asking and setting are the same act. Case folding and
        // the underscore fold (`resolve_alias`) cover `ShowPlan`,
        // `GetPlan` and friends from these rows alone.
        ("get_plan", "show_plan"),
        ("current_plan", "show_plan"),
        ("read_plan", "show_plan"),
        ("view_plan", "show_plan"),
        ("plan_status", "show_plan"),
        // Everything above is also reached case-insensitively, so
        // `Read`, `Bash`, `Edit`, `TodoWrite`, `WebFetch` resolve
        // without needing their own rows. See `resolve_alias`.
    ];
    for (k, v) in pairs {
        m.insert((*k).to_string(), (*v).to_string());
    }
    m
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            aliases: std::sync::RwLock::new(default_aliases()),
            deferred: Arc::new(std::sync::Mutex::new(HashMap::new())),
            session_id: String::new(),
            store: None,
        }
    }

    /// Bind this registry to a session, so `dispatch` gates on that
    /// session's principal.
    pub fn for_session(mut self, session_id: &str) -> Self {
        self.session_id = session_id.to_string();
        self
    }

    /// Let this registry re-home tools (D1): it reads the session's
    /// `/tools home` rules and frozen tool list from `store`.
    pub fn with_store(mut self, store: Arc<std::sync::Mutex<crate::store::Store>>) -> Self {
        self.store = Some(store);
        self
    }

    /// Where a call to `tool` runs, when not here (remotetool.rs, D1):
    /// the session's `/tools home` rules, then `[tools.home]`, then —
    /// for a tool this registry does not have — the node that built the
    /// session's frozen tool list (a session that came from another node
    /// keeps that node's tools, C1, so the call goes back to it rather
    /// than failing as unknown).
    fn home_of(&self, tool: &str) -> Option<Vec<String>> {
        // Someone else's web call (egress.rs): on a node that proxies it,
        // here, since its home is the owner's and the owner's address; on
        // one that refuses it, the node its `[web] others_via` names.
        match crate::egress::route(tool) {
            crate::egress::Route::Home => {}
            crate::egress::Route::Here => return None,
            crate::egress::Route::Via(node) => return Some(vec![node]),
        }
        let store = self.store.as_ref()?;
        let cfg = crate::config::live();
        let st = store.lock_ok();
        if let Some(nodes) =
            crate::remotetool::homes_for(&st, &cfg.tools.home, &self.session_id, tool)
        {
            return Some(nodes);
        }
        // No rule: a tool whose program this node lacks goes to a task
        // peer that advertises one (PLAN-phone-userland.md T2). The
        // identity is read only for a tool NEEDS names.
        if crate::remotetool::needs_of(&cfg, tool).is_some() {
            let me = crate::remotetool::my_id();
            if let Some(nodes) =
                crate::remotetool::auto_home_for(&st, &cfg, me.as_deref(), &self.session_id, tool)
            {
                return Some(nodes);
            }
        }
        if self.session_id.is_empty()
            || self.get(tool).is_some()
            || crate::remotetool::STAYS_HOME.contains(&tool)
        {
            return None;
        }
        let built_on = st
            .session_prompt_get(&self.session_id)
            .ok()
            .flatten()?
            .node?;
        (!crate::remotetool::is_here(&built_on)).then(|| vec![built_on])
    }

    /// `[cluster] sandbox_home` for this call (see
    /// [`crate::remotetool::sandbox_home_for`]). The zone is decided as
    /// the tool would decide it, from the session's principal and the
    /// call's own `host`/`sandbox` arguments.
    ///
    /// Unset, a node with no container runtime of its own derives one:
    /// the least loaded task peer that advertises a runtime
    /// (`remotetool::auto_sandbox_home`, PLAN-phone-userland.md T2).
    fn sandbox_home_of(&self, tool: &str, args: &Value) -> Option<Result<String, String>> {
        let store = self.store.as_ref()?;
        if !crate::remotetool::SANDBOX_TOOLS.contains(&tool) {
            return None;
        }
        let cfg = crate::config::live();
        let configured = cfg
            .cluster
            .sandbox_home
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_string);
        let sandboxed = matches!(
            crate::user_envs::zone_for(
                Some(&self.session_id),
                crate::user_envs::ZoneArg::from_args(args),
            ),
            crate::user_envs::Zone::Sandbox { .. }
        );
        let home = match configured {
            Some(h) => h,
            None if sandboxed => crate::remotetool::auto_sandbox_home(
                &store.lock_ok(),
                &cfg,
                crate::remotetool::my_id().as_deref(),
                &self.session_id,
                tool,
            )?,
            None => return None,
        };
        crate::remotetool::sandbox_home_for(Some(&home), tool, args, sandboxed)
    }

    /// Run a call on its homes, in order. A home that cannot be reached
    /// at all ([`crate::remotetool::Answer::Unreachable`]) passes the call
    /// to the next one, and the answer says which were passed over; any
    /// other answer — a refusal included — is the answer. `Here`: run it
    /// on this node, with the note to put in front of its output.
    async fn run_homed(&self, nodes: &[String], tool: &str, args: Value) -> Homed {
        let mut passed: Vec<String> = Vec::new();
        let note = |passed: &[String]| {
            (!passed.is_empty()).then(|| format!("[passed over: {}]", passed.join("; ")))
        };
        for node in nodes {
            if crate::remotetool::is_here(node) {
                return Homed::Here(note(&passed));
            }
            match self.run_homed_on(node, tool, args.clone()).await {
                None => return Homed::Here(note(&passed)),
                Some(crate::remotetool::Answer::Unreachable(why)) if nodes.len() > 1 => {
                    passed.push(why);
                }
                Some(answer) => {
                    let text = answer.text(tool);
                    return Homed::Out(match note(&passed) {
                        Some(n) => format!("{n}\n{text}"),
                        None => text,
                    });
                }
            }
        }
        Homed::Out(format!(
            "error: {tool}: none of its homes could be reached — {}",
            passed.join("; ")
        ))
    }

    /// One home of [`run_homed`]. `None`: it resolved to this node after
    /// all — run it here.
    async fn run_homed_on(
        &self,
        node: &str,
        tool: &str,
        args: Value,
    ) -> Option<crate::remotetool::Answer> {
        use crate::remotetool::{ask_for_ticket, call_on, Answer};
        let store = self.store.as_ref()?;
        let who = self.principal();
        let first = call_on(
            store,
            node,
            &self.session_id,
            tool,
            args.clone(),
            who.as_deref(),
            None,
        )
        .await;
        // D3: a call the node runs only with a ticket is put to whoever
        // is attached to this session; a ticket in the answer makes the
        // same call again, carrying it.
        let answer = match first {
            Answer::Here => return None,
            Answer::NeedsTicket {
                node: name,
                why,
                request,
            } if !request.is_empty() => {
                match ask_for_ticket(&self.session_id, tool, &name, &why, &request).await {
                    Some(ticket) => {
                        call_on(
                            store,
                            node,
                            &self.session_id,
                            tool,
                            args,
                            who.as_deref(),
                            Some(ticket),
                        )
                        .await
                    }
                    None => Answer::NeedsTicket {
                        node: name,
                        why,
                        request,
                    },
                }
            }
            a => a,
        };
        match answer {
            Answer::Here => None,
            a => Some(a),
        }
    }

    /// The caller this registry acts for, when there is one. `None` on
    /// an owner surface — full access, as before the ACL existed.
    fn principal(&self) -> Option<std::sync::Arc<crate::acl::Principal>> {
        if self.session_id.is_empty() {
            return None;
        }
        crate::acl::session_principal(&self.session_id)
    }

    /// Overlay config `tool_aliases` on the default table (config wins),
    /// EXCEPT a row whose key names a tool this registry actually has:
    /// that would shadow the real tool (the `shell`→`terminal` bug), and
    /// `resolve_alias` would ignore it anyway — so it is dropped here
    /// rather than left as a lie in the table. The dropped rows are
    /// logged, because an operator who set one deserves to hear that it
    /// did nothing rather than wonder why.
    pub fn set_aliases(&self, extra: std::collections::HashMap<String, String>) {
        let mut a = self.aliases.write_ok();
        for (k, v) in extra {
            if self.tools.contains_key(&k) || self.deferred.lock_ok().contains_key(&k) {
                tracing::warn!(
                    "tool alias `{k}` → `{v}` dropped: `{k}` is a registered tool, not an alias"
                );
                continue;
            }
            a.entry(k).and_modify(|e| *e = v.clone()).or_insert(v);
        }
    }

    /// Every alias key currently in the table — for the shadowing rule's
    /// test. Sorted so an assertion over it is deterministic.
    pub fn alias_keys(&self) -> Vec<String> {
        let mut v: Vec<String> = self.aliases.read_ok().keys().cloned().collect();
        v.sort();
        v
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Register under the tiering decision: `[tools] tier` sets the
    /// posture ("core" = only core tools resident; "full" = everything
    /// resident, today's behavior), `[tools.overrides]` moves ONE tool
    /// against it. Deferred tools stay fully reachable — tool_search
    /// finds them, tool_call runs them — they just stop paying schema
    /// rent in every prompt.
    pub fn register_tiered(&mut self, tool: Arc<dyn Tool>, home: Tier) {
        let name = tool.name().to_string();
        let resident = match crate::config::load() {
            Ok(cfg) => {
                if let Some(o) = cfg.tools.overrides.get(&name) {
                    o.trim() == "core"
                } else {
                    cfg.tools.tier == "full" || home == Tier::Core
                }
            }
            // No config: the home tier decides (defaults to "core").
            Err(_) => home == Tier::Core,
        };
        if resident {
            self.tools.insert(name, tool);
        } else {
            let dt = crate::deferred::DeferredTool {
                name: name.clone(),
                description: tool.description().to_string(),
                schema: tool.schema(),
                handler: tool,
            };
            self.deferred.lock_ok().insert(name, dt);
        }
    }

    /// The session's deferred map — shared with the tool_search /
    /// tool_describe / tool_call instances registered alongside it.
    pub fn deferred_map(
        &self,
    ) -> Arc<std::sync::Mutex<HashMap<String, crate::deferred::DeferredTool>>> {
        self.deferred.clone()
    }

    /// Every deferred tool name in this session's registry.
    pub fn deferred_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.deferred.lock_ok().keys().cloned().collect();
        v.sort();
        v
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tools.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn openai_schemas(&self) -> Vec<Value> {
        // SORTED BY NAME. Each ToolRegistry gets a fresh HashMap with a
        // fresh RandomState, so iterating .values() serialized the array
        // in a different order per session — and the tool array sits in
        // the cacheable prefix, so no two sessions could ever share a
        // cache entry. `names()` already sorted; this did not.
        let mut names: Vec<&String> = self.tools.keys().collect();
        names.sort();
        names
            .iter()
            .map(|n| {
                let t = &self.tools[*n];
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.schema(),
                    }
                })
            })
            .collect()
    }

    /// The registry name behind `name` — train-shaped aliases resolve,
    /// everything else is itself. ONE hop, same as dispatch. This is the
    /// name every GATE must classify on (ACL caps, plan mode, approvals,
    /// the sandbox auto-gate, the edit ledger): classifying on the alias
    /// lets a `bash` call slip past a gate that only knows `terminal`.
    /// Resolve a called name to a registry target.
    ///
    /// The REGISTRY comes first: a name that is a real tool (resident or
    /// deferred, exact or by case) is never an alias. Only then does the
    /// alias table get a look — exact match, then a CASE-INSENSITIVE
    /// retry. The alias table is written in snake_case because that is
    /// what veles' own tools are called, but the harnesses these models
    /// were trained on name their tools `Read`, `Write`, `Edit`, `Bash`,
    /// `Grep`, `Task`, `TodoWrite`. Those missed the table entirely on a
    /// capital letter and came back "unknown tool" — a model RL'd hard
    /// enough to emit `Read` will emit it again next round, so the cost
    /// was not one bad call but a loop.
    ///
    /// Case is the only thing folded. Nothing else about the name is
    /// guessed: an alias that guesses wrong teaches the model a lie,
    /// which is worse than a refusal that teaches it the truth.
    pub fn resolve_alias(&self, name: &str) -> String {
        // A REGISTERED TOOL IS NEVER AN ALIAS — checked first, both
        // tables. The alias row used to win over the registry, so when
        // `shell` was both a tool and an alias for `terminal`, the tool
        // was unreachable BY ITS OWN NAME and calls came back as
        // `[shell → terminal]` with terminal's "command required" for
        // `{action:"kill"}` — the PENDING incident where the model gave
        // up on `shell` entirely and pkill'd cargo. The hard-rule test
        // (`no_alias_may_shadow_a_registered_tool`) makes this property
        // structural rather than a matter of keeping the table clean.
        if self.tools.contains_key(name) || self.deferred.lock_ok().contains_key(name) {
            return name.to_string();
        }
        let lower = name.to_ascii_lowercase();
        if self.tools.contains_key(&lower) || self.deferred.lock_ok().contains_key(&lower) {
            return lower;
        }
        let aliases = self.aliases.read_ok();
        // An alias row resolves only when its TARGET answers in THIS
        // registry (resident, session-deferred or globally deferred).
        // Without the check, `task` in a registry without delegate_task
        // resolved to `delegate_task`, which then failed as "unknown
        // tool delegate_task" — a self-referential dead end that also
        // poisoned `suggest` (the alias key was a distance-0 candidate
        // and echoed the typed name). Returning the typed name instead
        // falls through to the honest unknown-tool refusal with a
        // suggestion that exists.
        let answers = |t: &str| {
            self.tools.contains_key(t)
                || self.deferred.lock_ok().contains_key(t)
                || crate::deferred::global_handler(t).is_some()
        };
        if let Some(t) = aliases.get(name) {
            if answers(t) {
                return t.clone();
            }
        }
        if let Some(t) = aliases.get(&lower) {
            if answers(t) {
                return t.clone();
            }
        }
        // Finally, fold the word separator too: `WebFetch` lowercases to
        // `webfetch`, which still misses the `web_fetch` row, and
        // `TodoWrite` misses `todo_write` the same way. Upstream
        // harnesses spell in PascalCase and veles spells in snake_case,
        // so the underscore is a spelling difference, not a meaning
        // one.
        //
        // Matches are collected and only accepted when there is exactly
        // ONE: if two different tools squash to the same string, that
        // is genuinely ambiguous and guessing would teach the model a
        // lie. Sorted first so the check cannot depend on HashMap
        // order.
        let squash = |s: &str| s.to_ascii_lowercase().replace('_', "");
        let want = squash(name);
        let mut hits: Vec<String> = aliases
            .iter()
            .filter(|(k, _)| squash(k) == want)
            .map(|(_, v)| v.clone())
            .collect();
        hits.extend(
            self.tools
                .keys()
                .filter(|k| squash(k) == want)
                .map(|k| k.to_string()),
        );
        hits.extend(
            self.deferred
                .lock_ok()
                .keys()
                .filter(|k| squash(k) == want)
                .map(|k| k.to_string()),
        );
        // Same existence rule as the exact rows above: a squashed
        // alias hit whose target does not answer here is a dead end,
        // not a resolution.
        hits.retain(|t| answers(t));
        hits.sort();
        hits.dedup();
        if hits.len() == 1 {
            return hits.remove(0);
        }
        name.to_string()
    }

    /// The schema a tool answers under a given name — aliases resolve so
    /// the repair ladder and refusal texts see the TARGET's contract.
    /// Deferred tools (session tier + global MCP) resolve too: a gate
    /// or repair ladder asking about a deferred tool must get its real
    /// contract, not "unknown".
    /// A tool by RESOLVED name: resident first, then this session's
    /// deferred, then the global (MCP) registry — the same lookup order
    /// `dispatch` uses, exposed for the plugin bridge's direct paths.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(t) = self.tools.get(name) {
            return Some(t.clone());
        }
        if let Some(dt) = self.deferred.lock_ok().get(name) {
            return Some(dt.handler.clone());
        }
        crate::deferred::global_handler(name)
    }

    pub fn schema_for(&self, name: &str) -> Option<Value> {
        let target = self.resolve_alias(name);
        if let Some(t) = self.tools.get(&target) {
            return Some(t.schema());
        }
        if let Some(dt) = self.deferred.lock_ok().get(&target) {
            return Some(dt.schema.clone());
        }
        crate::deferred::global_schema(&target)
    }

    /// The session this registry acts for ("" when it has none).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// ── ACL, at the funnel point ──
    ///
    /// The agent's tool loop gates before it gets here, so for a
    /// model-driven call this is a second, identical answer. It is
    /// here for every OTHER caller — `veles::tool_call` from a
    /// plugin, and whatever is added next — which reached the whole
    /// registry with no capability check at all.
    ///
    /// `gate_denial` is the same function the loop calls, so the two
    /// cannot drift, and it judges the resolved TARGET rather than
    /// the alias the caller typed.
    ///
    /// Public because `dispatch` is not the only path that runs a tool:
    /// the bridge runs a Tcl-proc-backed tool directly on the
    /// interpreter thread (`bridge::ffi_tool_call`), where dispatching
    /// asynchronously would wedge that very thread. That path used to
    /// skip this check entirely — the one caller with no gate was the
    /// one an untrusted plugin reaches without going through the model.
    /// Taking `target` (already alias-resolved) keeps both paths judging
    /// the same name.
    pub fn gate(&self, target: &str) -> Option<String> {
        let who = self.principal()?;
        // `[tool_caps]` is what opens a plugin or adapter tool up,
        // and it is read from the live file so an operator does not
        // restart the gateway to classify one.
        let cfg = crate::config::live();
        crate::acl::gate_denial(target, &cfg, &who)
    }

    pub async fn dispatch(&self, name: &str, args: Value) -> String {
        // The `tool.dispatch` override point (PLAN-extension-api.md E3),
        // when a plugin overrides it: a session's call goes through the
        // point, whose default (core's) hands it back here.
        if !self.session_id.is_empty() && crate::services::overridden(crate::points::TOOL) {
            return self.dispatch_through_point(name, args).await;
        }
        self.dispatch_layered(name, args).await
    }

    /// `tool.dispatch` asked about one call. Core's default answer sends
    /// the call (as the layers above passed it on) back to this registry
    /// through the call's slot, and this loop runs it with the layered,
    /// gated dispatch below while the point's answer is awaited.
    async fn dispatch_through_point(&self, name: &str, args: Value) -> String {
        let mut slot = crate::points::open_dispatch();
        let input = serde_json::json!({
            "session_id": self.session_id,
            "name": name,
            "args": args,
            "call_id": slot.id,
        });
        let ask = crate::services::ask(crate::points::TOOL, input, Some(self.session_id.clone()));
        tokio::pin!(ask);
        let answer = loop {
            tokio::select! {
                answer = &mut ask => break answer,
                Some((n, a, reply)) = slot.requests.recv() => {
                    let out = self.dispatch_layered(&n, a).await;
                    let _ = reply.send(out);
                }
            }
        };
        match answer {
            Ok(v) => match v.get("result") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => format!("error: `{}` answered no result", crate::points::TOOL),
            },
            Err(e) => format!("error: `{}` failed: {e}", crate::points::TOOL),
        }
    }

    /// Dispatch under any module layers (R7): what `tool.dispatch`'s
    /// default runs.
    async fn dispatch_layered(&self, name: &str, args: Value) -> String {
        // The tool-dispatch slot (R7, `layers.rs`): a module's layer
        // sees every call before core's gated dispatch and may answer
        // it, rewrite it, or pass it down. Nothing installed is the
        // direct call.
        let layers = crate::layers::tool_layers();
        if layers.is_empty() {
            return self.dispatch_default(name, args).await;
        }
        let cx = crate::layers::ToolCx {
            registry: self,
            name: name.to_string(),
            args,
        };
        let base: &crate::layers::ToolBase<'_> = &tool_base;
        crate::layers::run_tool_layers(&layers, cx, base).await
    }

    /// Core's own dispatch: the bottom of the tool chain.
    pub async fn dispatch_default(&self, name: &str, args: Value) -> String {
        // The tool that runs, looking through a `tool_call` envelope (the
        // browser tools are deferred, so that is how a model calls them).
        let target = self.resolve_alias(name);
        let envelope = target == "tool_call";
        let inner = if envelope {
            self.resolve_alias(args["name"].as_str().unwrap_or_default())
        } else {
            target
        };
        // The URL a web_extract asked for, for the escalation below.
        let fetched_url = (inner == "web_extract")
            .then(|| {
                let a = if envelope { &args["arguments"] } else { &args };
                a["url"].as_str().map(str::to_string)
            })
            .flatten();
        let out = self.dispatch_plain(name, args).await;
        // Part E: a page behind a bot challenge is put to the people on
        // THIS session — the node where the session lives, whichever node
        // ran the browser — and the call answers with the page once they
        // have solved it. Only a browser tool's result can start it, and
        // only by its first line (`challenge::marked`).
        let out = if inner.starts_with("browser_")
            && inner != "browser_challenge"
            && !self.session_id.is_empty()
            && crate::challenge::marked(&out)
        {
            crate::challenge::relay(self, &self.session_id, out, crate::challenge::Hands::Agent)
                .await
        } else if let Some(url) =
            fetched_url.filter(|_| !self.session_id.is_empty() && crate::challenge::marked(&out))
        {
            self.escalate_fetch(&url, out).await
        } else {
            out
        };
        // A file the call downloaded (a PDF), on whichever node it ran:
        // put where this session's commands run, its line turned into
        // the path (downloads.rs).
        crate::downloads::deliver(&self.session_id, out).await
    }

    /// web_extract met a challenge (PLAN-challenge-relay.md step 1): the
    /// fetch cannot solve it, so the same URL is opened in a browser and
    /// the challenge put to a person, as a browser tool's would be. The
    /// owner's sessions use the agent's own browser. Anyone else's use the
    /// GUEST browser: its own agent-browser session, reached only through
    /// `guestproxy`, so the page they chose never shares the owner's
    /// cookies and cannot reach the node's LAN.
    async fn escalate_fetch(&self, url: &str, refused: String) -> String {
        // Read here, not from the scope: the escalation runs after the
        // call's own scope has ended.
        let theirs = !crate::ask::owner_session(&self.session_id) || self.egress_chosen();
        let (opened, hands) = if !theirs {
            (
                self.dispatch_plain("browser_navigate", serde_json::json!({ "url": url }))
                    .await,
                crate::challenge::Hands::Agent,
            )
        } else {
            (
                self.dispatch_internal(
                    crate::browsertools::GUEST_OP,
                    serde_json::json!({ "action": "open", "url": url }),
                )
                .await,
                crate::challenge::Hands::Guest,
            )
        };
        if crate::challenge::marked(&opened) {
            return crate::challenge::relay(self, &self.session_id, opened, hands).await;
        }
        if crate::challenge::is_error(&opened) {
            return format!("{refused}\n[the browser could not open it either: {opened}]");
        }
        // The browser got through on its own (a check that a real browser
        // passes): the page, said as such. The agent's browser answered
        // with a snapshot, which a PDF's viewer leaves empty: it is read
        // again as a page, and a PDF downloaded. The guest's answer
        // already is the page.
        let page = match hands {
            crate::challenge::Hands::Agent => hands.page(self).await,
            crate::challenge::Hands::Guest => opened,
        };
        format!("[web_extract met a challenge; the browser got through without one]\n{page}")
    }

    /// An internal operation (`browsertools::is_internal`): in no tool
    /// registry, so no model can name it, but gated and homed like a tool.
    /// It runs where its home says — the guest browser with the
    /// `browser_*` tools — or here.
    pub(crate) async fn dispatch_internal(&self, name: &str, args: Value) -> String {
        crate::egress::scope(
            &self.session_id,
            self.egress_chosen(),
            self.dispatch_internal_unscoped(name, args),
        )
        .await
    }

    async fn dispatch_internal_unscoped(&self, name: &str, args: Value) -> String {
        if let Some(denial) = self.gate(name) {
            return denial;
        }
        if let Some(nodes) = self.home_of(name) {
            if let Homed::Out(out) = self.run_homed(&nodes, name, args.clone()).await {
                return out;
            }
        }
        crate::browsertools::internal(name, args).await
    }

    /// `dispatch` without the challenge relay — what the relay itself
    /// calls, so it cannot start another one. The call runs as this
    /// session's, for the web tools to see whose it is (egress.rs).
    pub(crate) async fn dispatch_plain(&self, name: &str, args: Value) -> String {
        crate::egress::scope(
            &self.session_id,
            self.egress_chosen(),
            self.dispatch_unscoped(name, args),
        )
        .await
    }

    /// Whether this session chose to send its web calls the way other
    /// people's go (`/egress tor`, egress.rs).
    fn egress_chosen(&self) -> bool {
        self.store
            .as_ref()
            .is_some_and(|s| crate::egress::chosen(&s.lock_ok(), &self.session_id))
    }

    async fn dispatch_unscoped(&self, name: &str, args: Value) -> String {
        // Train-shaped aliases resolve to the registry target, and the
        // answer NAMES the alias so a transcript stays readable. ACL and
        // approval classify on the TARGET, never the alias. A declared
        // SHAPE conversion runs first: it may redirect the target (the
        // editor `command` discriminator picks read_file/write_file/
        // patch) or refuse by name — and its args are already in the
        // target's vocabulary, so the gates below judge the tool that
        // actually runs.
        match super::repair::convert_alias_shape(name, &args.to_string()) {
            super::repair::ShapeOutcome::Refuse(why) => return why,
            super::repair::ShapeOutcome::Run {
                target,
                args: converted,
                notes,
            } => {
                // The redirected target is the tool that RUNS, so it is
                // the one gated: this branch used to call it straight,
                // and `str_replace_editor {command: create}` wrote a file
                // for a principal without FileWrite (a guest) through any
                // caller that is not the agent loop.
                if let Some(denial) = self.gate(&target) {
                    return denial;
                }
                if let Some(t) = self.tools.get(&target).cloned() {
                    let mut out = t.call(converted).await;
                    if target != name {
                        out = format!("[{name} → {target}]\n{out}");
                    }
                    if !notes.is_empty() {
                        out = format!("{}\n{out}", notes.join("; "));
                    }
                    return out;
                }
                // Target absent (a registry without the redirected tool):
                // fall through to the normal ladder — its "unknown tool"
                // names the RESOLVED name and teaches the vocabulary.
            }
            super::repair::ShapeOutcome::None => {}
        }
        let target = self.resolve_alias(name);
        if target != name {
            super::repair::metrics_alias_hit();
        }
        // The envelope is opened first — web and browser tools are
        // deferred, so a model reaches them through `tool_call` — and the
        // call inside is the one gated, as the agent loop gates it
        // (agent.rs, `gate_target`). Judging `tool_call` itself asked
        // for its own cap, `mcp`, which only admin+ holds: every user
        // was refused web_search, a cap their role has (IRC, 2026-10-02).
        let envelope = target == "tool_call";
        let inner = if envelope {
            self.resolve_alias(args["name"].as_str().unwrap_or_default())
        } else {
            target.clone()
        };
        // An internal operation is not a tool, whatever reaches this with
        // its name: only `dispatch_internal` runs one. Without this a model
        // that typed `browser_guest` passed the gate (it takes `web`) and
        // the `browser_*` home carried it to the browser's node.
        if inner.eq_ignore_ascii_case(crate::browsertools::GUEST_OP) {
            return format!("error: unknown tool '{inner}'");
        }
        if let Some(denial) = self.gate(&inner) {
            return denial;
        }
        // D1: a tool homed on another node runs there — the call inside
        // the envelope is the one that is homed.
        // Homes passed over on the way to running the call here.
        let mut passed_note: Option<String> = None;
        if !inner.is_empty() {
            let inner_args = || {
                if envelope {
                    args.get("arguments")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}))
                } else {
                    args.clone()
                }
            };
            // An explicit home first; then, on a node without a sandbox,
            // its `[cluster] sandbox_home` for a call whose zone is one.
            let homes = match self.home_of(&inner) {
                Some(nodes) => Some(nodes),
                None => match self.sandbox_home_of(&inner, &inner_args()) {
                    Some(Ok(node)) => Some(vec![node]),
                    Some(Err(why)) => return why,
                    None => None,
                },
            };
            if let Some(nodes) = homes {
                match self.run_homed(&nodes, &inner, inner_args()).await {
                    Homed::Out(out) => return out,
                    Homed::Here(n) => passed_note = n,
                }
            }
        }
        if let Some(t) = self.tools.get(&target) {
            let mut out = t.call(args).await;
            if target != name {
                out = format!("[{name} → {target}]\n{out}");
            }
            if let Some(n) = passed_note {
                out = format!("{n}\n[{inner} ran here]\n{out}");
            }
            return out;
        }
        // Deferred — session tier first, then the global (MCP) registry.
        // A deferred tool is NOT an unknown tool: it stays fully
        // invocable, the model just found it through tool_search.
        let handler = {
            self.deferred
                .lock_ok()
                .get(&target)
                .map(|dt| dt.handler.clone())
        }
        .or_else(|| crate::deferred::global_handler(&target));
        if let Some(h) = handler {
            let mut out = h.call(args).await;
            if target != name {
                out = format!("[{name} → {target}]\n{out}");
            }
            // The web and browser tools are deferred: a call that came
            // back here from its homes lands in THIS branch.
            if let Some(n) = passed_note {
                out = format!("{n}\n[{inner} ran here]\n{out}");
            }
            return out;
        }
        let suggestion = self.suggest(name);
        match suggestion {
            Some(s) => format!(
                "error: unknown tool `{name}` — did you mean `{s}`? (tools are discoverable via tool_search)"
            ),
            None => format!("error: unknown tool `{name}`"),
        }
    }

    /// Nearest registry tool (or alias key) within a small edit
    /// distance — one round to self-correct instead of three. Deferred
    /// tools count too: a typo of a deferred name should still land on
    /// the deferred tool, not "unknown".
    ///
    /// A suggestion NEVER echoes the typed name: `task` typed in a
    /// registry without delegate_task used to suggest `task` (the
    /// alias key itself, distance 0) — "did you mean `task`?" is not a
    /// self-correction, it is the error restated. Alias keys are also
    /// only suggested when their target actually answers in THIS
    /// registry: suggesting an alias to a tool that is not here sends
    /// the model into the same wall it just hit, one hop deeper.
    pub fn suggest(&self, name: &str) -> Option<String> {
        let name_l = name.to_lowercase();
        let mut best: Option<(usize, String)> = None;
        let mut consider = |candidate: &str| {
            if candidate.eq_ignore_ascii_case(&name_l) {
                return; // never echo the typed name
            }
            let d = super::repair::edit_distance(&name_l, &candidate.to_lowercase());
            if d <= 2 && d < best.as_ref().map(|b| b.0).unwrap_or(usize::MAX) {
                best = Some((d, candidate.to_string()));
            }
        };
        for k in self.deferred.lock_ok().keys() {
            consider(k);
        }
        for k in self.tools.keys() {
            consider(k);
        }
        for (k, v) in self.aliases.read_ok().iter() {
            // Only when the target answers here (resident or deferred):
            // an alias whose target is absent would re-route the model
            // into the same refusal it just got.
            if self.tools.contains_key(v) || self.deferred.lock_ok().contains_key(v) {
                consider(k);
            }
        }
        best.map(|(_, s)| s)
    }
}

// ── builtin tools ─────────────────────────────────────────────────────

/// Inspect and stop background shells started by `terminal{background:true}`.
///
/// One tool with an `action` enum rather than three tools, matching
/// `cronjob` — three schemas for three verbs is schema rent nobody wants
/// to pay for a rarely-used surface.
pub struct ShellTool {
    /// The CALLING session. Shell ids live in one process-wide space,
    /// so `output` and `kill` are scoped to what this session may
    /// reach — otherwise one gateway user reads or stops another's
    /// background work by guessing a small integer.
    pub session_id: String,
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Background shells (terminal{background:true}): list; output (captured so far); wait (block until it finishes or timeout, then its output — use instead of sleeping); kill. They report [BACKGROUND SHELL COMPLETE] by themselves."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "output", "wait", "kill"]},
                "id": {"type": "integer", "description": "Task id"},
                "timeout": {"type": "integer", "description": "wait: seconds (default 30, max 600)"}
            },
            "required": ["action"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let action = args["action"].as_str().unwrap_or_default();
        let id = args["id"].as_u64().map(|v| v as u32);
        match action {
            "list" => {
                serde_json::json!({"shells": crate::shells::list_for(&self.session_id)}).to_string()
            }
            "output" | "wait" => {
                let Some(id) = id else {
                    return format!("error: 'id' is required for {action}");
                };
                let block = action == "wait" || args["block"].as_bool().unwrap_or(false);
                if block {
                    return self.wait_for(id, args["timeout"].as_u64()).await;
                }
                match crate::shells::detail_for(id, &self.session_id) {
                    Some(mut v) => {
                        if v["status"] == "running" {
                            v["note"] = serde_json::json!(format!(
                                "still running — it reports [BACKGROUND SHELL COMPLETE] by \
                                 itself, so there is no need to poll. {}",
                                wait_advice(&self.session_id, Some(id))
                            ));
                        }
                        v.to_string()
                    }
                    None => "error: no such shell (shell{action:list} shows the live ones)".into(),
                }
            }
            "kill" => match id {
                Some(i) if crate::shells::kill_for(i, &self.session_id) => {
                    serde_json::json!({"status": "killing", "id": i}).to_string()
                }
                Some(i) => format!("error: shell {i} is unknown or already finished"),
                None => "error: 'id' is required for kill".into(),
            },
            other => format!("error: unknown action '{other}' (list | output | wait | kill)"),
        }
    }
}

/// How often a blocking `shell wait` looks at the shell.
const SHELL_WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(200);
/// Ceiling on one blocking wait.
const SHELL_WAIT_MAX_SECS: u64 = 600;

impl ShellTool {
    /// Block until shell `id` finishes or `timeout` passes — Claude Code's
    /// TaskOutput(block) shape. Returns the moment the shell is done, which
    /// is what a foreground `sleep N; tail log` was approximating badly.
    /// A finished shell's queued [BACKGROUND SHELL COMPLETE] is dropped:
    /// this result already carries it.
    async fn wait_for(&self, id: u32, timeout: Option<u64>) -> String {
        let secs = timeout.unwrap_or(30).clamp(1, SHELL_WAIT_MAX_SECS);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let Some(mut v) = crate::shells::detail_for(id, &self.session_id) else {
                return "error: no such shell (shell{action:list} shows the live ones)".into();
            };
            if v["status"] != "running" {
                // `tasks::finish` flips the status a moment before the
                // completion is queued; give it that moment, then drop it.
                for _ in 0..20 {
                    if crate::delegate::discard_shell_completion(&self.session_id, id) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                return v.to_string();
            }
            if std::time::Instant::now() >= deadline {
                v["note"] = serde_json::json!(format!(
                    "still running after {secs}s; it reports [BACKGROUND SHELL COMPLETE] when it \
                     finishes. {}",
                    wait_advice(&self.session_id, Some(id))
                ));
                return v.to_string();
            }
            tokio::time::sleep(SHELL_WAIT_POLL).await;
        }
    }
}

/// Wait for a condition, then wake the conversation.
pub struct MonitorTool {
    pub session_id: String,
}

#[async_trait]
impl Tool for MonitorTool {
    fn name(&self) -> &str {
        "monitor"
    }
    fn description(&self) -> &str {
        "Wake this session when a condition NOTHING ELSE reports holds (not for background shells or subagents: they report themselves). Exactly one of: shell (polled until it exits 0), task (id finishes), file (appears or changes). Arrives as [MONITOR FIRED] at the next step of a running turn, or starts a turn if the session is idle."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "list", "cancel"], "description": "Default: create"},
                "shell": {"type": "string", "description": "Poll this command until it exits 0"},
                "task": {"type": "integer", "description": "Wait for this task id to finish"},
                "file": {"type": "string", "description": "Wait for this path to appear, or to change if it already exists"},
                "timeout": {"type": "integer", "description": "Give up after this many seconds (default 600, max 3600)"},
                "id": {"type": "integer", "description": "Monitor id, for cancel"}
            }
        })
    }
    async fn call(&self, args: Value) -> String {
        match args["action"].as_str().unwrap_or("create") {
            "list" => serde_json::json!({"monitors": crate::monitor::list()}).to_string(),
            "cancel" => match args["id"].as_u64() {
                Some(i) if crate::monitor::cancel(i as u32) => {
                    serde_json::json!({"status": "cancelled", "id": i}).to_string()
                }
                Some(i) => format!("error: monitor {i} is unknown or already finished"),
                None => "error: 'id' is required for cancel".into(),
            },
            "create" => match crate::monitor::Predicate::from_args(&args) {
                Err(e) => format!("error: {e}"),
                Ok(p) => {
                    let desc = p.describe();
                    let id = crate::monitor::start(p, args["timeout"].as_u64(), &self.session_id);
                    serde_json::json!({
                        "status": "watching",
                        "task_id": id,
                        "waiting_for": desc,
                        "note": "reports as [MONITOR FIRED] — at the next step of a running turn, or by waking this session if it is idle"
                    })
                    .to_string()
                }
            },
            other => format!("error: unknown action '{other}' (create | list | cancel)"),
        }
    }
}

/// What a model should do while background work runs, told truthfully
/// for THIS session (`session_wake::wakes_when_idle`). The store showed
/// agents starting a background shell, adding a monitor on the same
/// log, then running a foreground `sleep` — because nothing said that
/// ending the turn is how to wait, or that it is safe. `id` names the
/// shell when there is one.
fn wait_advice(session_id: &str, id: Option<u32>) -> String {
    let wait = match id {
        Some(i) => format!("shell{{action:\"wait\",id:{i}}}"),
        None => "shell{action:\"wait\",id:N}".to_string(),
    };
    if crate::session_wake::wakes_when_idle(session_id) {
        format!(
            "Keep working on something else, or — if nothing is left — END YOUR TURN with one \
             line on what you are waiting for: the completion starts your next turn by itself. \
             Need the result inside this turn? {wait} returns the moment it finishes."
        )
    } else {
        format!(
            "This session does NOT wake on its own (a subagent, the TUI, a one-shot run): when \
             you need the result, {wait} returns the moment it finishes."
        )
    }
}

/// The refusal for a long foreground sleep (`[agent.guards]
/// foreground_sleep_max_secs`), or `None` when the command may run.
fn foreground_sleep_refusal(session_id: &str, cmd: &str) -> Option<String> {
    let max = crate::config::live().agent.guards.foreground_sleep_max_secs;
    if max == 0 {
        return None;
    }
    let secs = crate::guards::longest_foreground_sleep(cmd)?;
    if secs < max as f64 {
        return None;
    }
    let shown = if secs.is_finite() {
        format!("{secs}s")
    } else {
        "forever".to_string()
    };
    Some(format!(
        "error: not run — this command sleeps {shown} in the foreground ([agent.guards] \
         foreground_sleep_max_secs = {max}). A foreground sleep holds the whole turn, and \
         anything that finishes meanwhile waits behind it. Background shells, monitors and \
         subagents report by themselves ([BACKGROUND … COMPLETE] / [MONITOR FIRED]). {} \
         To wait for a condition, use `monitor`, or a bounded poll such as \
         `until <check>; do sleep 2; done`; sleeps under {max}s still run.",
        wait_advice(session_id, None)
    ))
}

/// Upper bound on a foreground command. The default is 60s; before this
/// existed there was NO ceiling, so `timeout: 100000` was a valid call
/// that wedged a tool round for a day. `execute_code` has always
/// clamped; this did not.
const TERMINAL_MAX_TIMEOUT: u64 = 3_600;

pub struct TerminalTool {
    /// Whose conversation a `background: true` command reports back to.
    pub session_id: String,
}

#[async_trait]
impl Tool for TerminalTool {
    fn name(&self) -> &str {
        "terminal"
    }
    fn description(&self) -> &str {
        // The shell is named on the card (PLAN-windows.md §6): a model
        // that reads "PowerShell" here stops writing `bash -c` habits.
        static DESC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        DESC.get_or_init(|| {
            format!(
                "Execute a shell command and return stdout+stderr. The shell is {}. Chain `cd` with `&&`: a failed cd does not stop the next line. background=true for anything long (builds, tests, servers): returns a task id at once and [BACKGROUND SHELL COMPLETE] arrives when it finishes — don't poll, sleep or monitor it; keep working or end your turn. Long foreground sleeps are refused. If the user detaches YOUR running command (/detach), the call returns {{status: detached, task_id}} and it keeps running in the background.",
                crate::platform::facts().shell
            )
        })
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to run"},
                "timeout": {"type": "integer", "description": format!("Max seconds (default 60, max {TERMINAL_MAX_TIMEOUT}); ignored when background=true")},
                "background": {"type": "boolean", "description": "Run detached and return a task id instead of waiting. Use for servers, builds and anything over a minute."},
                "host": {"type": "boolean", "description": "Run on the HOST instead of a sandbox. Admin/owner only and ignored below that; omit it and the call is sandboxed."}, "sandbox": {"type": "string", "description": "Name of a sandbox to run in instead of this session's — one you may reach."}
            },
            "required": ["command"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let cmd = args["command"].as_str().unwrap_or_default().to_string();
        if !args["background"].as_bool().unwrap_or(false) {
            if let Some(refusal) = foreground_sleep_refusal(&self.session_id, &cmd) {
                return refusal;
            }
        }
        // ── user envs: below-admin principals run EVERYWHERE in their
        // own container; Admin/Owner leave it per call with host:true.
        // The zone comes from the SESSION principal — the model's args
        // cannot move a sub-admin onto the host (their `host:true`
        // is simply ignored, and stripped before dispatch).
        let zone = crate::user_envs::zone_for(
            Some(&self.session_id),
            crate::user_envs::ZoneArg::from_args(&args),
        );
        if let crate::user_envs::Zone::Sandbox { key, name } = zone {
            return self.call_in_env(&key, &name, &cmd, &args).await;
        }
        if args["background"].as_bool().unwrap_or(false) {
            if cmd.trim().is_empty() {
                return "error: 'command' is required".into();
            }
            let id = crate::shells::start(
                &cmd,
                &self.session_id,
                crate::user_envs::ZoneArg::from_args(&args),
            );
            return serde_json::json!({
                "status": "started",
                "task_id": id,
                "note": format!(
                    "running in the background; you are notified with [BACKGROUND SHELL COMPLETE] \
                     when it finishes — don't poll it, sleep, or add a monitor for it. {} \
                     shell{{action:output,id}} peeks, shell{{action:kill,id}} stops it.",
                    wait_advice(&self.session_id, Some(id))
                )
            })
            .to_string();
        }
        let timeout = args["timeout"]
            .as_u64()
            .unwrap_or(60)
            .clamp(1, TERMINAL_MAX_TIMEOUT);
        // S4 secrets: resolve $NAME references and inject as env vars
        // (never spliced into the command line — ps-safe), then scrub
        // every known value from the output before the model sees it.
        // `child_env` decides both halves by policy and principal: a
        // non-owner session gets nothing injected and F3's strip; an
        // owner's command loses the engine-only credentials (a bot
        // token) from its inherited environment and gets the `$NAME`s
        // their policy allows.
        let child = crate::secrets::child_env(&self.session_id, &cmd);
        let refs = child.inject.clone();
        let withheld_note = child.note();
        let scrubbable = crate::secrets::scrubbable();
        let mut command = tokio::process::Command::new(crate::shell::shell_binary());
        if !child.strip.is_empty() {
            for k in &child.strip {
                command.env_remove(k);
            }
            tracing::info!(
                "secrets: stripped {} inherited env var(s) from a terminal",
                child.strip.len()
            );
        }
        // Without kill_on_drop a timed-out command is merely ABANDONED:
        // dropping the Child detaches it, so `bash -c` and its whole
        // process tree keep running with nobody holding a handle.
        command
            .args(crate::shell::shell_args(&cmd))
            .kill_on_drop(true);
        // spawn() + wait_with_output() captures NOTHING unless the pipes
        // are requested explicitly — unlike Command::output(), which
        // auto-pipes stdout/stderr. The Stop-mid-command refactor
        // (process_group + cancel watch) switched output() → spawn() and
        // dropped that implicit piping, so every terminal result came
        // back EMPTY: the command ran, its output leaked to the gateway's
        // own stdout, and the model and the desktop both saw "". Pipe
        // them back so wait_with_output() actually collects them.
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        // stdin must NOT be inherited: the gateway's own stdin belongs
        // to whatever surface is driving it (the TUI's raw-mode tty
        // especially — a child reading stdin steals every keystroke
        // and an interactive child hangs waiting for a tty it will
        // never get). Background shells have nulled it all along;
        // `terminal` was the asymmetry.
        command.stdin(std::process::Stdio::null());
        // The child leads its OWN process group, so a cancel or a timeout
        // can kill the WHOLE tree. Killing `bash -c` alone orphans
        // whatever it started — that is the `sleep 300` that kept running
        // after Stop, and the timeout message below used to claim "the
        // command was killed" while only the shell died.
        crate::platform::own_group(&mut command);
        if !refs.is_empty() {
            for (k, v) in &refs {
                command.env(k, v);
            }
            tracing::info!(
                "secrets: injected {} value(s) into terminal env",
                refs.len()
            );
        }
        // ── The fg→bg handover shape (USER taxonomy, 2026-09-17:
        // "there should also be a way for the user to move a running
        // foreground shell into the background").
        //
        // The child, its pipe pumps and the wait loop all live on the
        // process-wide `bg` runtime, and the tool just awaits one
        // outcome over a oneshot. That placement is load-bearing:
        //
        //  - A tokio child is bound to the runtime that SPAWNED it —
        //    its reaping rides that runtime's SIGCHLD driver. The REPL
        //    and TUI build a runtime PER TURN, so a child spawned on
        //    the tool's task could never outlive the turn; "detach"
        //    would orphan or kill the very process it promises to keep.
        //    Spawned on `bg`, it outlives any surface — the same reason
        //    shells::start lives there.
        //  - Detach is then trivial AND lossless: the pumps have fed
        //    the shell ring (scrubbed, capped) from the first line, so
        //    an adopted shell's `output` shows the FULL history, and
        //    the wait loop simply keeps going under the registry's
        //    cancel token instead of the turn's timeout/cancel.
        //
        // The INLINE contract is untouched: pumps also collect raw
        // stdout/stderr into buffers (only until detached), and the
        // Completed outcome rebuilds exactly today's string — stdout,
        // "\n[stderr]\n" + stderr, "\n[exit code: N]" for non-zero —
        // scrubbed whole at the end.
        let ring: crate::shells::Ring =
            Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let exit: crate::shells::ExitSlot = Arc::new(std::sync::Mutex::new(None));
        let stdout_buf: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let stderr_buf: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        // Set when the command is adopted as a shell: from then on the
        // pumps feed only the ring (the tool has its answer; the
        // buffers must not grow unbounded for a process nobody inline-
        // waits on anymore).
        let detached = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let (tx, rx) = tokio::sync::oneshot::channel::<FgOutcome>();
        let sess = self.session_id.clone();
        let cmd_bg = cmd.clone();
        let scrub_map = Arc::new(scrubbable.clone());
        // Arm the fg marker BEFORE the spawn: /detach consults it, and
        // the bg task might reach its own arm only after the user
        // already typed /detach. mark_fg_command(false) on every exit
        // path (including the rx-failure one below) clears it again.
        crate::agent::mark_fg_command(&self.session_id, true);
        crate::bg::spawn(async move {
            let mut child = match command.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(FgOutcome::SpawnError(format!("error: {e}")));
                    return;
                }
            };
            let pid = child.id().unwrap_or(0);
            // The tree a cancel or a timeout ends: the process group the
            // child leads (unix) or the job it was just put into
            // (Windows). Adopted right after the spawn; `None` leaves
            // only the child itself to kill_on_drop.
            let tree = Arc::new(crate::platform::adopt(pid));
            // Pumps run on THIS runtime (plain tokio::spawn inside a
            // bg task attaches to bg — see bg.rs). One pump per pipe:
            // ring feed (scrubbed, capped) always, raw-line buffer only
            // while still foreground.
            let mut pumps =
                {
                    let mut v = Vec::new();
                    for (src, buf) in [
                        (
                            child.stdout.take().map(|s| {
                                Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>
                            }),
                            stdout_buf.clone(),
                        ),
                        (
                            child.stderr.take().map(|s| {
                                Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>
                            }),
                            stderr_buf.clone(),
                        ),
                    ] {
                        let Some(src) = src else { continue };
                        let ring = ring.clone();
                        let detached = detached.clone();
                        let scrub_map = scrub_map.clone();
                        v.push(crate::rt::spawn(async move {
                            use tokio::io::AsyncBufReadExt;
                            let mut rd = tokio::io::BufReader::new(src);
                            let mut chunk: Vec<u8> = Vec::new();
                            // read_until on BYTES, then lossy per line:
                            // `lines()` stops at the first invalid UTF-8
                            // byte, and the old wait_with_output contract
                            // kept everything (from_utf8_lossy). A command
                            // that emits one binary blob must not lose the
                            // text around it.
                            loop {
                                chunk.clear();
                                match rd.read_until(b'\n', &mut chunk).await {
                                    Ok(0) => break, // EOF
                                    Ok(_) => {}
                                    Err(_) => break,
                                }
                                let mut t = chunk.as_slice();
                                if t.last() == Some(&b'\n') {
                                    t = &t[..t.len() - 1];
                                }
                                if t.last() == Some(&b'\r') {
                                    t = &t[..t.len() - 1];
                                }
                                let l = String::from_utf8_lossy(t).into_owned();
                                // Ring lines are scrubbed per line, exactly
                                // as `shells::run` pumps them (its
                                // output(id) may be read by any surface at
                                // any time).
                                crate::shells::push_capped(
                                    &ring,
                                    crate::secrets::scrub(&l, &scrub_map),
                                );
                                if !detached.load(std::sync::atomic::Ordering::Relaxed) {
                                    buf.lock_ok().push(l);
                                }
                            }
                        }));
                    }
                    v
                };

            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
            // Phase 1: the foreground wait. Races the command against
            // the timeout, the turn cancel and the detach flag — the
            // same poll-and-translate shape `shells::run` uses.
            let phase1 = loop {
                if crate::agent::turn_cancelled(&sess) {
                    kill_tree(&tree, pid);
                    let _ = child.wait().await;
                    break FgOutcome::Cancelled;
                }
                if tokio::time::Instant::now() >= deadline {
                    // kill_on_drop already dropped the shell when the
                    // wait was cancelled by the timeout; the GROUP needs
                    // the explicit kill, or the timeout message is the
                    // lie it used to be.
                    kill_tree(&tree, pid);
                    let _ = child.wait().await;
                    break FgOutcome::Timeout;
                }
                if crate::agent::detach_requested(&sess) {
                    // Register the ALREADY-LIVE ring as a shell row and
                    // answer the tool now; phase 2 below keeps the
                    // process running under the registry's cancel token.
                    let (id, cancel) =
                        crate::shells::adopt_live(&cmd_bg, &sess, ring.clone(), exit.clone());
                    detached.store(true, std::sync::atomic::Ordering::Relaxed);
                    // The inline buffers are dead weight now — free
                    // them; the pumps stop appending the instant the
                    // flag is set.
                    stdout_buf.lock_ok().clear();
                    stderr_buf.lock_ok().clear();
                    break FgOutcome::Detached { id, cancel };
                }
                match child.try_wait() {
                    Ok(Some(st)) => {
                        break FgOutcome::Completed {
                            stdout: stdout_buf.lock_ok().join("\n"),
                            stderr: stderr_buf.lock_ok().join("\n"),
                            code: st.code(),
                            left_running: None,
                        }
                    }
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                    Err(e) => break FgOutcome::SpawnError(format!("error: {e}")),
                }
            };
            // The pumps must reach EOF before the buffers are final —
            // try_wait can see the exit while the last lines are still
            // in flight. (For Detached they keep running; the ring is
            // their only sink now and phase 2 joins them at the end.)
            // Bounded: see PIPE_GRACE. A leftover holding the pipes gets
            // them drained into the capped ring only.
            if !matches!(phase1, FgOutcome::Detached { .. }) {
                let drained = join_pumps_within_grace(&mut pumps).await;
                if !drained {
                    detached.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                let _ = tx.send(match phase1 {
                    FgOutcome::Completed { code, .. } => {
                        // Re-read the buffers AFTER the pumps joined —
                        // the values moved into the outcome above were
                        // snapshotted too early.
                        FgOutcome::Completed {
                            stdout: stdout_buf.lock_ok().join("\n"),
                            stderr: stderr_buf.lock_ok().join("\n"),
                            code,
                            left_running: (!drained).then_some(pid),
                        }
                    }
                    other => other,
                });
                return;
            }
            // Phase 2: the adopted shell's life. Same contract as a
            // born-background shell — no timeout (the user asked for it
            // to keep running), killed only by its registry cancel
            // token (/shells kill, /tasks cancel, the desktop button),
            // and the shared completion plumbing at the end.
            let (id, cancel) = match phase1 {
                FgOutcome::Detached { id, cancel } => (id, cancel),
                _ => unreachable!("phase 2 entered only for Detached"),
            };
            // The tool must learn its task id even though phase 2 owns
            // this task from here on — send now, keep waiting after.
            let _ = tx.send(FgOutcome::Detached {
                id,
                cancel: cancel.clone(),
            });
            let code = loop {
                if cancel.is_cancelled() {
                    kill_tree(&tree, pid);
                    let _ = child.wait().await;
                    crate::shells::push_capped(&ring, "[cancelled]".to_string());
                    break -1;
                }
                match child.try_wait() {
                    Ok(Some(st)) => break st.code().unwrap_or(-1),
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                    Err(e) => {
                        crate::shells::push_capped(&ring, format!("error: {e}"));
                        break -1;
                    }
                }
            };
            *exit.lock_ok() = Some(code);
            let _ = join_pumps_within_grace(&mut pumps).await;
            tracing::info!("detached shell {id} (was foreground terminal) exited {code}");
            crate::shells::adopted_finish(id, &cmd_bg, &sess, &ring, &exit, code == 0).await;
        });

        // The tool side: mark the fg wait (so /detach knows there is
        // something to move) and await the one outcome.
        crate::agent::mark_fg_command(&self.session_id, true);
        let outcome = match rx.await {
            Ok(o) => o,
            // The bg task panicked between spawn and send: the child
            // had kill_on_drop, so nothing is orphaned.
            Err(_) => {
                crate::agent::mark_fg_command(&self.session_id, false);
                return "error: the command waiter died unexpectedly (the process group was killed)".into();
            }
        };
        crate::agent::mark_fg_command(&self.session_id, false);
        match outcome {
            FgOutcome::Completed {
                stdout,
                stderr,
                code,
                left_running,
            } => {
                let mut s = stdout;
                if !stderr.is_empty() {
                    s.push_str("\n[stderr]\n");
                    s.push_str(&stderr);
                }
                // Report the exit code unless it's a clean 0 — hermes
                // parity, and the verification ledger's evidence signal
                // relies on it (a failing command is NOT proof).
                if code != Some(0) {
                    let c = code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into());
                    s.push_str(&format!("\n[exit code: {c}]"));
                }
                if let Some(pgid) = left_running {
                    s.push_str(&format!(
                        "\n[note: the shell exited but something it started still holds its output \
                         (a `cmd &`?) — left running; what it prints from now on is not shown. \
                         `kill -- -{pgid}` stops what is left in its process group. To run a server \
                         or anything you want to follow, use background=true.]"
                    ));
                }
                if let Some(n) = &withheld_note {
                    s.push('\n');
                    s.push_str(n);
                }
                crate::secrets::scrub(&s, &scrubbable)
            }
            FgOutcome::SpawnError(e) => e,
            FgOutcome::Cancelled => {
                "error: cancelled by the user mid-command (the command and its process group were killed)".into()
            }
            FgOutcome::Timeout => format!(
                "error: timeout after {timeout}s (the command was killed). For anything this long, call terminal with background=true instead."
            ),
            FgOutcome::Detached { id, .. } => serde_json::json!({
                "status": "detached",
                "task_id": id,
                "note": "the command was moved to the background and keeps running as a shell; its full output history is captured. shell{action:output,id} to read it, shell{action:kill,id} to stop it, and [BACKGROUND SHELL COMPLETE] arrives when it finishes."
            })
            .to_string(),
        }
    }
}

/// How a foreground `terminal` wait ended. Sent once over the oneshot
/// from the bg-runtime waiter to the tool. `Detached` carries the
/// registry id AND its cancel token (phase 2 of the waiter keeps
/// running under that token; the tool needs only the id).
enum FgOutcome {
    Completed {
        stdout: String,
        stderr: String,
        code: Option<i32>,
        /// The process group whose leftover still held the pipes when
        /// the grace ran out (see [`PIPE_GRACE`]).
        left_running: Option<u32>,
    },
    SpawnError(String),
    Cancelled,
    Timeout,
    Detached {
        id: u32,
        cancel: crate::providers::CancelToken,
    },
}

/// How long a finished command's pipes get to reach EOF. EOF needs
/// EVERY holder closed, and a process the command left running
/// (`server &`, a daemon, `cd d && srv & PID=$!` — which backgrounds the
/// whole `&&` list, so `kill $PID` kills the subshell and orphans the
/// server) holds them for its whole life. Joined unbounded, that hung a
/// turn for good (2026-09-28, session d544b5): the shell had exited, so
/// neither the timeout nor Stop — both checked only while it runs —
/// could reach the wait. The call answers when the shell exits, after
/// this grace for the last lines in flight.
const PIPE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Join the pumps, but for no longer than [`PIPE_GRACE`]. `false` =
/// something still holds the pipes. The pumps are not aborted: they keep
/// draining (a full pipe would block the leftover, a closed one would
/// SIGPIPE it), and the caller stops their inline buffering.
async fn join_pumps_within_grace(pumps: &mut [tokio::task::JoinHandle<()>]) -> bool {
    tokio::time::timeout(PIPE_GRACE, async {
        for p in pumps.iter_mut() {
            let _ = p.await;
        }
    })
    .await
    .is_ok()
}

impl TerminalTool {
    /// Sandbox-zone terminal: exec through the env container. The
    /// output contract mirrors the host path (stdout, [stderr],
    /// [exit code], the timeout message) plus the `[env: sandbox …]`
    /// tag so the model sees where the call ran. No secrets are
    /// injected — the container carries none by construction, and
    /// `podman exec` forwards no gateway env (F3 at the exec layer).
    async fn call_in_env(&self, key: &str, name: &str, cmd: &str, args: &Value) -> String {
        if cmd.trim().is_empty() {
            return "error: 'command' is required".into();
        }
        if args["background"].as_bool().unwrap_or(false) {
            // The call's own zone argument, resolved again the same way:
            // `Own` here sent a shell asked for in a named box to the
            // caller's own one. (Only a sandbox reaches here, so there is
            // no `host` for it to honour.)
            let id = crate::shells::start(
                cmd,
                &self.session_id,
                crate::user_envs::ZoneArg::from_args(args),
            );
            return crate::user_envs::annotate(
                name,
                serde_json::json!({
                    "status": "started",
                    "task_id": id,
                    "note": format!(
                        "running in the background inside the sandbox; you are notified with \
                         [BACKGROUND SHELL COMPLETE] when it finishes — don't poll it, sleep, or \
                         add a monitor for it. {} shell{{action:output,id}} peeks, \
                         shell{{action:kill,id}} stops it.",
                        wait_advice(&self.session_id, Some(id))
                    )
                })
                .to_string(),
            );
        }
        let timeout = args["timeout"]
            .as_u64()
            .unwrap_or(60)
            .clamp(1, TERMINAL_MAX_TIMEOUT);
        match crate::user_envs::exec_shell(key, cmd, timeout).await {
            Ok(out) if out.timed_out => crate::user_envs::annotate(
                name,
                format!(
                    "error: timeout after {timeout}s (the command was killed inside the \
sandbox). For anything this long, call terminal with background=true instead."
                ),
            ),
            Ok(out) => {
                let mut s = out.stdout;
                if !out.stderr.is_empty() {
                    s.push_str("\n[stderr]\n");
                    s.push_str(&out.stderr);
                }
                if out.exit_code != 0 {
                    s.push_str(&format!("\n[exit code: {}]", out.exit_code));
                }
                crate::user_envs::annotate(
                    name,
                    crate::secrets::scrub(&s, &crate::secrets::scrubbable()),
                )
            }
            Err(e) => crate::user_envs::annotate(name, format!("error: user env: {e:#}")),
        }
    }
}

/// End the child's whole tree: its process group on unix (the child
/// leads its own), the job it sits in on Windows. Without it a cancel
/// kills `bash -c` alone and orphans whatever it started — that is the
/// `sleep 300` that kept running after Stop. `pid` is the fallback when
/// no tree could be adopted.
fn kill_tree(tree: &Arc<Option<crate::platform::Tree>>, pid: u32) {
    match tree.as_ref() {
        Some(t) => t.kill(),
        None => crate::platform::kill(pid),
    }
}

// ── session → origin ─────────────────────────────────────────────────
//
// `origin` is the `platform:chat` a gateway session was created for.
// Cron jobs already use it to deliver back where they came from; an
// adapter tool needs it for a different reason — to answer "is this
// caller's own channel the one they are asking me to act on". A
// process-global map, same shape as `acl::SESSION_PRINCIPALS`, filled
// where the value already arrives.

static SESSION_ORIGINS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::OnceLock::new();

fn session_origins() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    SESSION_ORIGINS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub fn set_session_origin(session_id: &str, origin: &str) {
    session_origins()
        .lock_ok()
        .insert(session_id.to_string(), origin.to_string());
}

/// The `platform:chat` this session was created for, when it came from
/// a gateway platform at all.
pub fn session_origin(session_id: &str) -> Option<String> {
    session_origins().lock_ok().get(session_id).cloned()
}

/// Populate the registry with the builtin tool set for INTERACTIVE
/// sessions (CLI, TUI, gateway chats). `origin` is 'platform:chat_id' for
/// gateway sessions — the cronjob tool uses it as the default delivery
/// target for jobs created there, and the adapter tools use it to tell
/// the caller's own buffer from every other one.
pub fn builtin_registry(
    store: Arc<std::sync::Mutex<crate::store::Store>>,
    origin: Option<String>,
    session_id: &str,
) -> ToolRegistry {
    if let Some(o) = origin.as_deref() {
        set_session_origin(session_id, o);
    }
    let mut r = base_registry(store.clone(), session_id);
    // Kept before CronjobTool consumes `store`.
    let plan_store = store.clone();
    // cronjob/a2a stay DEFERRED: heavy schemas, rare calls —
    // discoverable via tool_search, invocable via tool_call, and still
    // carrying their session state because they live in the session's
    // own deferred map, not the global one.
    r.register_tiered(
        Arc::new(crate::cronjob::CronjobTool { store, origin }),
        Tier::Deferred,
    );
    // delegate_task is CORE. It was deferred with cronjob/a2a as
    // "heavy schema, rare calls" — but the rarity WAS the defect, not
    // the justification: over the whole store (2026-08-03..09-17) it was
    // called 8 times and went dark after 2026-08-15, while hermes
    // subagents got regular use. A delegation tool the model never sees
    // in its schema array is one it never emits — the skill_manage
    // lesson, same commit shape (`veles toolstats` shows the counts).
    // The schema is one object with five properties; the rent is small
    // and parallel work is a daily-use capability.
    // …but NOT inside a child. The recursion guard lives in the tool's
    // own `call` (VELES_CHILD_DEPTH ≥ 1 → refuse), which is the right
    // place for it to be UNBYPASSABLE — and it left the tool advertised
    // in every child's schema array anyway, so a subagent could read
    // "spawn subagents" in its own toolset, emit a call, and be told
    // no. Found by running `/tools` inside a real depth-1 child during
    // the 2026-09-18 audit. The guard stays where it is; this only
    // stops the child being offered a door it is not allowed through.
    if crate::delegate::child_depth() == 0 {
        r.register_tiered(
            Arc::new(crate::delegate::DelegateTaskTool {
                session_id: session_id.to_string(),
                hooks: None,
            }),
            Tier::Core,
        );
    }
    r.register_tiered(Arc::new(crate::a2a_tool::A2ATool), Tier::Deferred);
    // `batch` sits beside the tool_search/tool_call trio in spirit: it
    // is the resident handle on N calls at once. The TOOL registers
    // here for its schema only — the turn loop expands the envelope
    // (every gate per item; see batch.rs), and the tool's own `call`
    // refuses, so no other dispatch path (plugin bridge, direct
    // dispatch) can run an ungated batch.
    r.register_tiered(Arc::new(crate::batch::BatchTool), Tier::Core);
    // `clarify` only where a human can plausibly answer. Cron jobs and
    // A2A peer sessions use base_registry and never get it — same
    // reasoning as the cronjob recursion guard below. CORE tier: it is
    // how the model asks, and it is small.
    r.register_tiered(
        Arc::new(crate::clarify::ClarifyTool {
            session_id: session_id.to_string(),
            timeout_secs: 300,
        }),
        Tier::Core,
    );
    // `secret_request` beside `clarify`: it asks a human (privately)
    // and is pointless where nobody can answer. DEFERRED, not core: its
    // schema is ~860 B against a core budget with 350 B of headroom
    // (`the_core_tier_is_under_the_declared_budget`), and the owner's
    // prompt names it in one line instead (`secrets::prompt_names`),
    // which is how the model learns never to ask for a key in chat.
    r.register_tiered(
        Arc::new(crate::secret_tools::SecretRequestTool {
            session_id: session_id.to_string(),
            timeout_secs: 600,
        }),
        Tier::Deferred,
    );
    // `config`: read and change veles' own settings for the owner
    // (crate::config_tool). Every change is put to a human first, so it
    // lives here with the asking tools; DEFERRED, the core tier is full.
    r.register_tiered(
        Arc::new(crate::config_tool::ConfigTool {
            session_id: session_id.to_string(),
            timeout_secs: 300,
        }),
        Tier::Deferred,
    );
    // `present_plan` sits beside `clarify` for the same reason: both put
    // something in front of a human and wait, and both are pointless
    // where nobody can answer. It is a no-op outside plan mode, so
    // registering it unconditionally costs a schema entry and nothing
    // else. base_registry (cron jobs, A2A peers) does NOT get it —
    // there is no human there to approve a plan.
    r.register_tiered(
        Arc::new(crate::plan::PresentPlanTool {
            store: plan_store,
            session_id: session_id.to_string(),
            timeout_secs: 1800,
        }),
        Tier::Core,
    );
    // Adapter-provided tools (e.g. the irc tool), registered globally
    // at adapter startup. Deferred tier: adapters opt INTO the prompt
    // with `[tools.overrides] irc = "core"`.
    // A session-bound copy when the tool offers one (`for_session`),
    // so an adapter's process-wide singleton can still see who is
    // calling it.
    // A module's tool is held late-bound (`session_tool`).
    for (t, owner) in extra_tools().values_with_owners() {
        r.register_tiered(session_tool(t, owner, session_id), Tier::Deferred);
    }
    r
}

/// Registry for contexts that must NOT manage cron jobs — currently cron
/// JOB sessions themselves (recursion guard by construction: a scheduled
/// job gets no `cronjob` tool, so it cannot schedule more jobs; hermes
/// only asks nicely in the prompt).
pub fn base_registry(
    store: Arc<std::sync::Mutex<crate::store::Store>>,
    session_id: &str,
) -> ToolRegistry {
    let mut r = ToolRegistry::new()
        .for_session(session_id)
        .with_store(store.clone());
    // ── CORE tier: resident in every prompt ──
    // The tools a working session reaches for every few rounds. Small
    // schemas, constant use — this is the set the cacheable prefix
    // carries.
    r.register_tiered(
        Arc::new(TerminalTool {
            session_id: session_id.to_string(),
        }),
        Tier::Core,
    );
    // CORE, not deferred. ShellTool and MonitorTool sat in the deferred
    // tier on a "only a session that has backgrounded something needs
    // these" theory — but that is exactly the reasoning that put
    // `skill_manage` there and took skill writes to ZERO (see its
    // comment above). The store agrees: over 2026-08-03..09-17 `shell`
    // and `monitor` were called 0 times, while `terminal` background
    // mode ran constantly — a tool the model cannot see is a tool it
    // does not reach for, and "poll or stop it with the `shell` tool" in
    // terminal's description pointed at a name that resolved nowhere.
    // Both schemas are small (one action enum each).
    r.register_tiered(
        Arc::new(ShellTool {
            session_id: session_id.to_string(),
        }),
        Tier::Core,
    );
    r.register_tiered(
        Arc::new(MonitorTool {
            session_id: session_id.to_string(),
        }),
        Tier::Core,
    );
    // Secrets without a shell: `api_request` sends a key to its own
    // hosts only. Deferred for the same budget reason as
    // `secret_request`; the owner's prompt names it whenever secrets
    // exist. Cron jobs get it too: a scheduled usage check is exactly
    // the use.
    r.register_tiered(
        Arc::new(crate::secret_tools::ApiRequestTool {
            session_id: session_id.to_string(),
        }),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::secret_tools::SecretListTool {
            session_id: session_id.to_string(),
        }),
        Tier::Deferred,
    );
    // File tools (hermes parity — 2026-08-10 tools milestone). The
    // mutating two carry a checkpoint context (M13): every overwrite and
    // patch leaves an undo point behind. All four carry the SESSION so
    // the user-env zone routing can resolve the principal at call time
    // (below-admin ⇒ the caller's container; see user_envs.rs).
    let ckpt = crate::checkpoints::CheckpointCtx {
        store: store.clone(),
        session_id: session_id.to_string(),
    };
    r.register_tiered(
        Arc::new(crate::filetools::ReadFileTool {
            session: Some(session_id.to_string()),
        }),
        Tier::Core,
    );
    r.register_tiered(
        Arc::new(crate::filetools::WriteFileTool {
            ckpt: Some(ckpt.clone()),
            session: Some(session_id.to_string()),
        }),
        Tier::Core,
    );
    r.register_tiered(
        Arc::new(crate::filetools::PatchTool {
            ckpt: Some(ckpt),
            session: Some(session_id.to_string()),
        }),
        Tier::Core,
    );
    r.register_tiered(
        Arc::new(crate::filetools::SearchFilesTool {
            session: Some(session_id.to_string()),
        }),
        Tier::Core,
    );
    r.register_tiered(
        Arc::new(crate::memory::MemoryTool {
            session_id: session_id.to_string(),
        }),
        Tier::Core,
    );
    // CORE, with no properties: the answer to "who is talking" must
    // be one call away, exactly when a stranger in a channel claims to
    // be the operator (the 2026-10-17 #flashsupport incident: /usage
    // knew it was the owner, the model had no way to ask, and argued).
    r.register_tiered(
        Arc::new(crate::whoami::WhoAmITool {
            session_id: session_id.to_string(),
        }),
        Tier::Core,
    );
    // The skill tools carry the session so they see the SAME library the
    // prompt index was built from. Without it an agent could still
    // `skill_view` something the user had switched off for this session,
    // which is not a switch. skills_list/skill_view are CORE — they are
    // how the model reaches everything the prompt no longer lists.
    r.register_tiered(
        Arc::new(crate::skills::SkillsListTool {
            session_id: session_id.to_string(),
            store: store.clone(),
        }),
        Tier::Core,
    );
    r.register_tiered(
        Arc::new(crate::skills::SkillViewTool {
            session_id: session_id.to_string(),
            store: store.clone(),
        }),
        Tier::Core,
    );
    // CORE, not deferred. It was demoted here in cb5e592 ("token
    // efficiency"), which strips a tool from the schema array sent to
    // the provider (see `openai_schemas`) — so writing a skill started
    // costing a `tool_search` + `tool_call` round-trip first. The model
    // stopped paying it: `skill_manage` calls in the store run 10/2/2 on
    // Aug 29-31 and then ZERO every day after that commit. USER: "I never
    // see them writing or reading skills or memory". A tool the model
    // will not reach for is not a saving.
    r.register_tiered(Arc::new(crate::skills::SkillManageTool), Tier::Core);
    // The todo list is per session, so the registry needs to know which
    // one it belongs to (M12 — persisted, unlike hermes' in-memory list).
    r.register_tiered(
        Arc::new(crate::todo::TodoTool {
            store: store.clone(),
            session_id: session_id.to_string(),
        }),
        Tier::Core,
    );
    // The plan read-back, beside the todo list it shares a pane with.
    // CORE for the same reason `todo` is: the moment it matters — after
    // a compaction, or in a session that started mid-plan — is a moment
    // a deferred tool costs a discovery round-trip the model may not
    // spend, and the alternative we watched a model take was shelling
    // out to sqlite (PENDING 2026-09-18). It lives in base_registry,
    // NOT beside `present_plan` in builtin_registry: reading a plan
    // needs no human, so cron and A2A sessions — the ones that wake a
    // session whose plan they must know — get it too.
    r.register_tiered(
        Arc::new(crate::plan::ShowPlanTool {
            store: store.clone(),
            session_id: session_id.to_string(),
        }),
        Tier::Core,
    );
    r.register_tiered(
        Arc::new(crate::session_search::SessionSearchTool {
            store: store.clone(),
        }),
        Tier::Core,
    );
    // `sessions`: list the live sessions on this machine and mail one
    // (crate::mail). DEFERRED — the core tier has no headroom, and the
    // model reaches for another session rarely and on purpose. Here in
    // the base registry so a cron session can mail too; an A2A peer
    // session is below owner, and the tool is owner-only unclassified.
    r.register_tiered(
        Arc::new(crate::sessions_tool::SessionsTool {
            store: store.clone(),
            session_id: session_id.to_string(),
        }),
        Tier::Deferred,
    );
    // `device`: what a connected phone or desktop window can do
    // (crate::devices). DEFERRED for the same reason as `sessions`.
    r.register_tiered(Arc::new(crate::devices::DeviceTool), Tier::Deferred);
    // `app`: the person's apps (crate::apps) — make one, run it on a
    // window, read its errors, look at it. DEFERRED like `device`, which
    // it runs through.
    r.register_tiered(Arc::new(crate::apps::AppTool), Tier::Deferred);
    // Deferred tool discovery (hermes tool_search parity): MCP tools,
    // tiered builtins — everything not resident — is discoverable via
    // tool_search, describable via tool_describe, invocable via
    // tool_call. These three ARE the core tier's handle on the rest,
    // so they are always resident. They hold the session's deferred
    // map (shared with this registry) plus the global MCP registry.
    let session_deferred = r.deferred_map();
    r.register(Arc::new(crate::deferred::ToolSearchTool {
        session: session_deferred.clone(),
    }));
    r.register(Arc::new(crate::deferred::ToolDescribeTool {
        session: session_deferred.clone(),
    }));
    r.register(Arc::new(crate::deferred::ToolCallTool {
        session: session_deferred,
    }));
    // ── DEFERRED tier: discoverable, not resident ──
    // Heavy schemas, occasional calls. Each costs a discovery
    // round-trip the first time a session needs it — that is the
    // trade, and `[tools.overrides]` promotes any of them back.
    r.register_tiered(Arc::new(crate::webtools::WebSearchTool), Tier::Deferred);
    r.register_tiered(Arc::new(crate::webtools::WebExtractTool), Tier::Deferred);
    r.register_tiered(
        Arc::new(crate::executecode::ExecuteCodeTool {
            session_id: session_id.to_string(),
        }),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::vision::VisionAnalyzeTool {
            session_id: session_id.to_string(),
        }),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::asr::AsrTool {
            session_id: session_id.to_string(),
        }),
        Tier::Deferred,
    );
    // Desktop pane tools — drive the veles-desktop right-pane shell
    // (open_preview / focus_pane, hermes parity).
    r.register_tiered(
        Arc::new(crate::desktop_tools::OpenPreviewTool),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::desktop_tools::FocusPaneTool),
        Tier::Deferred,
    );
    // The live interpreters. `app` evals are gated on the desktop having
    // armed live patching, so registering this everywhere does not hand
    // every session the running window.
    r.register_tiered(Arc::new(crate::desktop_tools::TclEvalTool), Tier::Deferred);
    r.register_tiered(
        Arc::new(crate::export::SessionExportTool {
            store: store.clone(),
        }),
        Tier::Deferred,
    );
    // Browser tools (#26): 8 individual tools for fine-grained control.
    r.register_tiered(Arc::new(crate::browsertools::BrowserTool), Tier::Deferred);
    r.register_tiered(
        Arc::new(crate::browsertools::BrowserSnapshotTool),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::browsertools::BrowserClickTool),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::browsertools::BrowserTypeTool),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::browsertools::BrowserScrollTool),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::browsertools::BrowserPressTool),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::browsertools::BrowserBackTool),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::browsertools::BrowserConsoleTool),
        Tier::Deferred,
    );
    // Part E: the relay's hands on a challenge page. Deferred like the
    // rest, and homed with them by `browser_*`.
    r.register_tiered(
        Arc::new(crate::browsertools::BrowserChallengeTool),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::computeruse::ComputerUseTool {
            session_id: session_id.to_string(),
        }),
        Tier::Deferred,
    );
    r.register_tiered(
        Arc::new(crate::tts::TtsTool {
            session_id: session_id.to_string(),
        }),
        Tier::Deferred,
    );
    r.register_tiered(Arc::new(crate::imagegen::ImageGenTool), Tier::Deferred);
    r.register_tiered(Arc::new(crate::imagegen::VideoGenTool), Tier::Deferred);
    // Train-shaped aliases: the config table overlays the defaults.
    if let Ok(cfg) = crate::config::load() {
        r.set_aliases(cfg.tool_aliases);
    }
    r
}

#[cfg(test)]
mod tests {

    /// The T2 hook (PLAN-phone-userland.md): with no rule, a call to a
    /// tool whose program this node does not advertise goes to a task
    /// peer that does; a tool it has, and a sandbox-zone call on a node
    /// with no task peer runtime, stay here. What each node has is its
    /// own registry row.
    #[test]
    fn home_of_routes_a_lacking_tool_to_a_capable_peer() {
        let (home, _guard) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join("config.toml"),
            "[cluster]\ntask_peers = [\"laptop\"]\n\
             [a2a_agents.laptop]\nurl = \"http://laptop:9900\"\n",
        )
        .expect("config");
        let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())
            .expect("identity");
        let store = crate::store::Store::open(&home.path().join("state.db")).expect("store");
        let caps = |tools: &[&str]| serde_json::json!({ "os": "android", "tools": tools });
        store
            .node_register(
                &me.id,
                "phone",
                "h",
                "v",
                "ephemeral",
                None,
                None,
                None,
                Some(&caps(&["bash"]).to_string()),
            )
            .unwrap();
        store
            .node_register(
                "laptop-1",
                "laptop",
                "h",
                "v",
                "ephemeral",
                None,
                Some("http://laptop:9900"),
                None,
                Some(&caps(&["bash", "rg"]).to_string()),
            )
            .unwrap();
        let reg = ToolRegistry::new()
            .for_session("t2-hook")
            .with_store(Arc::new(std::sync::Mutex::new(store)));
        assert_eq!(
            reg.home_of("search_files"),
            Some(vec!["laptop-1".to_string()])
        );
        assert_eq!(reg.home_of("terminal"), None, "the phone has bash");
        // The laptop advertises no runtime, so no sandbox home appears.
        assert_eq!(
            reg.sandbox_home_of("terminal", &serde_json::json!({"command": "true"})),
            None
        );
    }

    /// The command the F3 tests run: print the canary variable, or GONE
    /// when the child did not inherit it — in the shell this node's
    /// terminal tool actually runs (pwsh on Windows, bash/sh elsewhere).
    fn canary_or_gone() -> &'static str {
        if cfg!(windows) {
            "if ($env:VELES_F3_CANARY_SASL) { $env:VELES_F3_CANARY_SASL } else { 'GONE' }"
        } else {
            concat!("printenv ", "VELES_F3_CANARY_SASL", " || echo GONE")
        }
    }
    /// The tool-dispatch slot (R7): a layer sees every `dispatch`
    /// before core's and may answer it or wrap the answer; a purge of
    /// its owner leaves core's dispatch alone again. The layers act on
    /// probe names only: the slot is process-global and other tests
    /// dispatch concurrently.
    #[tokio::test]
    async fn a_dispatch_layer_answers_or_wraps_and_a_purge_restores_core() {
        use crate::layers::{BoxFuture, NextTool, ToolCx, ToolLayer};
        struct Probe;
        impl ToolLayer for Probe {
            fn call<'a>(&'a self, cx: ToolCx<'a>, next: NextTool<'a>) -> BoxFuture<'a, String> {
                Box::pin(async move {
                    match cx.name.as_str() {
                        "layer_probe_answer" => "answered by the layer".to_string(),
                        "layer_probe_wrap" => format!("wrapped[{}]", next.call(cx).await),
                        _ => next.call(cx).await,
                    }
                })
            }
        }
        let reg = ToolRegistry::default();
        let unknown = reg
            .dispatch("layer_probe_wrap", serde_json::json!({}))
            .await;
        assert!(!unknown.starts_with("wrapped["), "no layer yet: {unknown}");

        let prev = crate::lifecycle::thread_owner();
        crate::lifecycle::set_thread_owner("module:tool-probe");
        crate::layers::install_tool(std::sync::Arc::new(Probe));
        crate::lifecycle::clear_thread_owner_for_test(prev);

        assert_eq!(
            reg.dispatch("layer_probe_answer", serde_json::json!({}))
                .await,
            "answered by the layer",
            "a replacing layer never reaches core's dispatch"
        );
        let wrapped = reg
            .dispatch("layer_probe_wrap", serde_json::json!({}))
            .await;
        assert_eq!(
            wrapped,
            format!("wrapped[{unknown}]"),
            "a wrapping layer gets core's answer"
        );

        assert_eq!(crate::layers::purge_owner(Some("module:tool-probe")), 1);
        assert_eq!(
            reg.dispatch("layer_probe_wrap", serde_json::json!({}))
                .await,
            unknown
        );
    }

    use super::*;

    #[tokio::test]
    async fn a_train_shaped_alias_dispatches_to_the_target_and_says_so() {
        let mut r = ToolRegistry::new();
        struct Echo;
        #[async_trait::async_trait]
        impl Tool for Echo {
            fn name(&self) -> &str {
                "patch"
            }
            fn description(&self) -> &str {
                "x"
            }
            fn schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(&self, args: Value) -> String {
                format!("called with {args}")
            }
        }
        r.register(Arc::new(Echo));
        // `edit` is the NAME-ONLY alias for patch; `apply_patch` is a
        // declared shape refusal now (its freeform format has no honest
        // conversion), covered by its own test in repair's suites.
        let out = r.dispatch("edit", serde_json::json!({"a": 1})).await;
        assert!(
            out.starts_with("[edit → patch]"),
            "the result names the alias: {out}"
        );
        assert!(out.contains("called with"), "{out}");
    }

    #[tokio::test]
    async fn unknown_tools_suggest_the_nearest_real_one() {
        let mut r = ToolRegistry::new();
        struct Echo;
        #[async_trait::async_trait]
        impl Tool for Echo {
            fn name(&self) -> &str {
                "terminal"
            }
            fn description(&self) -> &str {
                "x"
            }
            fn schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(&self, _args: Value) -> String {
                "ok".into()
            }
        }
        r.register(Arc::new(Echo));
        let out = r.dispatch("termnial", serde_json::json!({})).await;
        assert!(
            out.contains("did you mean `terminal`"),
            "near-misses self-correct: {out}"
        );
        assert!(
            r.dispatch("nonexistent", serde_json::json!({}))
                .await
                .contains("unknown tool"),
            "far names get the plain error"
        );
    }

    /// The `task` incident, closed. An alias whose target is absent
    /// from the registry in use must (a) NOT resolve — the typed name
    /// falls through to the honest unknown-tool refusal — and (b) never
    /// be SUGGESTED as its own correction ("unknown tool `task` — did
    /// you mean `task`?"), because the alias key was a distance-0
    /// candidate in the same pool.
    #[tokio::test]
    async fn an_alias_without_its_target_neither_resolves_nor_suggests_itself() {
        // A registry with NO delegate_task: an empty one plus the
        // default alias table (`task → delegate_task` is in there).
        let r = ToolRegistry::new();
        assert_eq!(
            r.resolve_alias("task"),
            "task",
            "the alias must not resolve when delegate_task is absent"
        );
        let out = r.dispatch("task", serde_json::json!({"prompt": "x"})).await;
        assert!(
            !out.contains("did you mean `task`"),
            "a suggestion never echoes the typed name: {out}"
        );
        // And in a registry that HAS the target, it resolves normally.
        let mut full = ToolRegistry::new();
        struct Delegate;
        #[async_trait::async_trait]
        impl Tool for Delegate {
            fn name(&self) -> &str {
                "delegate_task"
            }
            fn description(&self) -> &str {
                "x"
            }
            fn schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(&self, args: Value) -> String {
                format!("ran with {args}")
            }
        }
        full.register(Arc::new(Delegate));
        assert_eq!(full.resolve_alias("task"), "delegate_task");
        let out = full
            .dispatch("task", serde_json::json!({"goal": "fix it"}))
            .await;
        assert!(out.contains("[task → delegate_task]"), "{out}");
        assert!(out.contains("fix it"), "{out}");
    }

    /// META-TEST: every alias row declares whether it is name-only or
    /// carries a shape conversion. A row in NEITHER list means someone
    /// added an alias without deciding what happens to its arguments —
    /// exactly how `str_replace_editor` shipped with a comment that
    /// claimed every command maps onto patch while no code mapped
    /// anything.
    #[test]
    fn every_alias_row_declares_name_only_or_shape_converted() {
        for key in default_aliases().keys() {
            let declared = crate::repair::SHAPE_CONVERTED_ALIASES
                .iter()
                .chain(crate::repair::NAME_ONLY_ALIASES.iter());
            assert!(
                declared.clone().any(|k| k == key),
                "alias `{key}` is in neither SHAPE_CONVERTED_ALIASES nor \
                 NAME_ONLY_ALIASES — declare which kind it is"
            );
        }
        // And the two lists do not overlap: a row cannot be both.
        for k in crate::repair::SHAPE_CONVERTED_ALIASES {
            assert!(
                !crate::repair::NAME_ONLY_ALIASES.contains(k),
                "alias `{k}` is declared twice"
            );
        }
        // And nothing in either list is stale (row since removed).
        for k in crate::repair::SHAPE_CONVERTED_ALIASES
            .iter()
            .chain(crate::repair::NAME_ONLY_ALIASES.iter())
        {
            assert!(
                default_aliases().contains_key(*k),
                "alias `{k}` is declared but no longer exists"
            );
        }
    }

    /// The editor family's `command` discriminator actually redirects:
    /// view→read_file, create→write_file, str_replace→patch, each with
    /// the vocabulary keys picked up under every upstream spelling, and
    /// the transcript NAMES the redirection. insert and unknown
    /// commands are teaching refusals.
    #[tokio::test]
    async fn the_editor_family_converts_by_discriminator() {
        let (home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().expect("store dir");
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        let r = builtin_registry(store, None, "editor-shape");

        let f = home.path().join("editor.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let p = f.to_str().unwrap();

        // view → read_file, file_path picked up.
        let out = r
            .dispatch(
                "str_replace_editor",
                serde_json::json!({"command": "view", "file_path": p}),
            )
            .await;
        assert!(
            out.contains("[str_replace_editor command=view → read_file]"),
            "{out}"
        );
        assert!(out.contains("one"), "the file actually read: {out}");

        // create → write_file, file_text picked up.
        let f2 = home.path().join("made.txt");
        let out = r
            .dispatch(
                "str_replace_editor",
                serde_json::json!({"command": "create", "path": f2.to_str().unwrap(), "file_text": "created"}),
            )
            .await;
        assert!(out.contains("write_file"), "{out}");
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), "created");

        // str_replace → patch, old_str/new_str picked up.
        let out = r
            .dispatch(
                "str_replace_editor",
                serde_json::json!({"command": "str_replace", "path": p, "old_str": "two", "new_str": "TWO"}),
            )
            .await;
        assert!(out.contains("patch"), "{out}");
        assert!(std::fs::read_to_string(&f).unwrap().contains("TWO"));

        // insert: a teaching refusal, not a garbled conversion.
        let out = r
            .dispatch(
                "str_replace_editor",
                serde_json::json!({"command": "insert", "path": p, "insert_text": "x"}),
            )
            .await;
        assert!(
            out.contains("old_string"),
            "the refusal teaches patch's shape: {out}"
        );
        assert!(!out.contains("called with"), "{out}");

        // unknown command: refusal naming the real tools.
        let out = r
            .dispatch(
                "str_replace_editor",
                serde_json::json!({"command": "undo", "path": p}),
            )
            .await;
        assert!(out.contains("view/create/str_replace/insert"), "{out}");
    }

    /// `glob` is a file-NAME pattern: content mode compiles it as a
    /// ripgrep regex (`*.rs` → regex parse error) and files mode
    /// matches basenames, so the conversion forces target=files and
    /// strips a leading `**/`.
    #[test]
    fn glob_converts_to_a_files_mode_search_with_prefix_stripped() {
        match crate::repair::convert_alias_shape(
            "glob",
            r#"{"pattern": "**/*.rs", "path": "/tmp"}"#,
        ) {
            crate::repair::ShapeOutcome::Run { target, args, .. } => {
                assert_eq!(target, "search_files");
                assert_eq!(args["target"], "files");
                assert_eq!(args["pattern"], "*.rs", "the **/ prefix is basename noise");
                assert_eq!(args["path"], "/tmp");
            }
            other => panic!("expected a Run, got {other:?}"),
        }
    }

    /// The python pair refuses by name with the reason — execute_code
    /// here is Tcl, and `print(1)` dying as "invalid command name"
    /// teaches nothing about why.
    #[test]
    fn the_python_pair_refuses_by_name_with_the_reason() {
        for name in ["code_interpreter", "execute"] {
            match crate::repair::convert_alias_shape(name, r#"{"code": "print(1)"}"#) {
                crate::repair::ShapeOutcome::Refuse(why) => {
                    assert!(why.contains("Tcl"), "says what IS here: {why}");
                    assert!(why.contains("execute_code"), "names the real tool: {why}");
                }
                other => panic!("{name}: expected a Refuse, got {other:?}"),
            }
        }
    }

    /// apply_patch / insert_content: declared refusals that teach what
    /// patch CAN express (context-anchored inserts), not a garbled
    /// conversion or a bare unknown-tool error.
    #[test]
    fn the_patch_freeform_pair_refuses_teaching_patchs_shape() {
        for name in ["apply_patch", "insert_content"] {
            match crate::repair::convert_alias_shape(name, r#"{"path": "/x"}"#) {
                crate::repair::ShapeOutcome::Refuse(why) => {
                    assert!(why.contains("old_string"), "teaches the anchor: {why}");
                    assert!(why.contains("new_string"), "teaches the replacement: {why}");
                }
                other => panic!("{name}: expected a Refuse, got {other:?}"),
            }
        }
    }

    /// Claude's `timeout` is milliseconds; veles' is seconds. Above
    /// 3600 the value is read as ms and divided — 600000 becomes 600s
    /// with a note, instead of silently clamping to the 3600 max.
    #[test]
    fn millisecond_timeouts_are_converted_with_a_note() {
        let mut args = serde_json::json!({"command": "sleep 10", "timeout": 600000});
        let mut notes = Vec::new();
        crate::repair::convert_timeout_units(&mut args, &mut notes);
        assert_eq!(args["timeout"], serde_json::json!(600));
        assert!(!notes.is_empty(), "the conversion is announced");
        // A seconds-scale value passes through untouched.
        let mut args = serde_json::json!({"command": "x", "timeout": 120});
        let mut notes = Vec::new();
        crate::repair::convert_timeout_units(&mut args, &mut notes);
        assert_eq!(args["timeout"], serde_json::json!(120));
        assert!(notes.is_empty(), "no note for an honest seconds count");
    }

    /// The rename note tells the truth per pair: `questions → question`
    /// says the extras are held back, `prompt → goal` says it is the
    /// task to run — and an ordinary transport rename still says "same
    /// field here".
    #[test]
    fn rename_notes_are_per_pair_honest() {
        let mut args = serde_json::json!({"file_path": "/x"});
        let mut notes = Vec::new();
        crate::repair::rename_param_aliases(
            &mut args,
            &serde_json::json!({"properties": {"path": {}}}),
            &mut notes,
        );
        assert!(
            notes.iter().any(|n| n.contains("same field here")),
            "the ordinary pair keeps its note: {notes:?}"
        );
    }

    /// `is_background` is pure transport (Claude Code's Bash spelling)
    /// and rides the schema-guarded rename — terminal's schema takes
    /// `background`, so the flag survives instead of being silently
    /// dropped as an unknown field.
    #[test]
    fn the_background_flag_survives_as_a_param_rename() {
        let mut args = serde_json::json!({"command": "make", "is_background": true});
        let mut notes = Vec::new();
        crate::repair::rename_param_aliases(
            &mut args,
            &serde_json::json!({"properties": {"command": {}, "background": {}}}),
            &mut notes,
        );
        assert_eq!(args["background"], serde_json::json!(true), "{args}");
        assert!(args.get("is_background").is_none(), "{args}");
        assert!(
            notes.iter().any(|n| n.contains("is_background")),
            "{notes:?}"
        );
    }

    // ── tool tiering (Step 2.1) ──────────────────────────────────────

    /// The interactive registry under the DEFAULT config (tier = core):
    /// scratch VELES_HOME, so no config.toml, so the code defaults
    /// decide.
    fn tiered_registry() -> ToolRegistry {
        let dir = crate::testenv::kept_dir();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.join("state.db")).expect("store"),
        ));
        crate::tools::builtin_registry(store, None, "tier-test")
    }

    /// EVERY tool a session can reach must be ACL-classified. An
    /// unclassified name is not "no gate" in any useful sense — it is
    /// an open door: the cap gate in `agent.rs` does nothing when
    /// `tool_cap` returns None, which is how `monitor` (a host shell
    /// under another name) and `shell` stayed reachable by any
    /// principal who could start a turn. A new tool now fails this
    /// test instead of shipping ungated.
    #[test]
    fn every_registered_tool_is_acl_classified() {
        let (_home, _env) = crate::testenv::scratch_home();
        let r = tiered_registry();
        let mut all = r.names();
        all.extend(r.deferred_names());
        // Tools registered by plugins or adapters at runtime are not
        // veles' to classify here (another test in this binary leaves
        // some behind — the process-global registry is shared); they
        // are covered by the fail-closed gate below instead.
        let extras = crate::tools::global_tool_names();
        let unclassified: Vec<String> = all
            .into_iter()
            .filter(|n| !extras.contains(n))
            .filter(|n| crate::acl::tool_cap(n).is_none())
            .collect();
        assert!(
            unclassified.is_empty(),
            "these tools reach a session with NO capability gate: {unclassified:?} — add an arm to acl::tool_cap"
        );
    }

    /// …and a tool NOBODY classified is owner-only, not open. This is
    /// The gate is at the FUNNEL POINT, not only in the agent's tool
    /// loop.
    ///
    /// The ACL check lived in `run_turn`'s tool loop alone, so every
    /// other caller of `dispatch` reached the whole registry ungated —
    /// `veles::tool_call` from a plugin most of all, which could run
    /// `terminal` on the host with neither a capability check nor an
    /// approval. Asserted through `dispatch` directly, because that is
    /// the door the other callers come through.
    #[tokio::test]
    async fn dispatch_gates_on_the_sessions_principal_not_only_the_turn_loop() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("s.db")).unwrap(),
        ));
        let sid = "dispatch-gate-session";
        let reg = crate::tools::builtin_registry(store, None, sid);

        // A guest: Chat + FileRead, deliberately no Shell.
        crate::acl::set_session_principal(
            sid,
            crate::acl::Principal::known("irc:stranger", crate::acl::Role::Guest),
        );
        let out = reg
            .dispatch("terminal", serde_json::json!({"command": "id"}))
            .await;
        assert!(
            out.contains("[acl]") || out.to_lowercase().contains("denied"),
            "a guest reached `terminal` straight through the registry: {out}"
        );
        // The ALIAS must not be a way around it — classification is on
        // the resolved target, which is the whole reason the alias
        // table is safe to have.
        let out = reg
            .dispatch("bash", serde_json::json!({"command": "id"}))
            .await;
        assert!(
            out.contains("[acl]") || out.to_lowercase().contains("denied"),
            "the `bash` alias walked past the gate: {out}"
        );
        // Something the guest MAY do still runs.
        let out = reg
            .dispatch("read_file", serde_json::json!({"path": "/nonexistent-xyz"}))
            .await;
        assert!(
            !out.contains("[acl]"),
            "a capability the guest HAS must not be gated: {out}"
        );

        // No principal (an owner surface) — no gate, which is the
        // pre-ACL behaviour every local surface relies on.
        crate::acl::set_session_principal(
            sid,
            crate::acl::Principal::known("irc:boss", crate::acl::Role::Owner),
        );
        let out = reg
            .dispatch(
                "terminal",
                serde_json::json!({"command": "echo gated-or-not"}),
            )
            .await;
        assert!(!out.contains("[acl]"), "an owner is not gated: {out}");
    }

    /// A SHAPE alias is gated on the tool it runs. `str_replace_editor`
    /// with `command: create` is converted to `write_file` and run — and
    /// that branch called the tool straight, so a guest (no FileWrite)
    /// wrote a file through any caller that is not the agent loop.
    #[tokio::test]
    async fn a_shape_alias_is_gated_on_the_tool_it_runs() {
        let (home, _env) = crate::testenv::scratch_home();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&home.path().join("s.db")).unwrap(),
        ));
        let sid = "shape-gate-session";
        let reg = crate::tools::builtin_registry(store, None, sid);
        crate::acl::set_session_principal(
            sid,
            crate::acl::Principal::known("irc:stranger", crate::acl::Role::Guest),
        );
        let target = home.path().join("planted.txt");
        let out = reg
            .dispatch(
                "str_replace_editor",
                serde_json::json!({
                    "command": "create",
                    "path": target.display().to_string(),
                    "file_text": "planted",
                }),
            )
            .await;
        assert!(
            out.contains("[acl]") || out.to_lowercase().contains("denied"),
            "a guest wrote through a shape alias: {out}"
        );
        // The refusal names the tool that would have RUN, not the alias
        // the model typed: classification is on the converted target.
        assert!(
            out.contains("write_file"),
            "the denial must name what the alias converts to: {out}"
        );
        assert!(!target.exists());
    }

    /// D1: a tool homed on another node is sent there — and one the
    /// session's rules keep `local` is not. The node here is unknown to
    /// the registry, so the call fails NAMING the node instead of
    /// running quietly on this one.
    #[tokio::test]
    async fn a_homed_tool_goes_to_its_node_and_never_quietly_runs_here() {
        let (home, _env) = crate::testenv::scratch_home();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&home.path().join("s.db")).unwrap(),
        ));
        let sid = "homed-session";
        let reg = crate::tools::builtin_registry(store.clone(), None, sid);
        let f = home.path().join("here.txt");
        std::fs::write(&f, "on this node").unwrap();
        let read = serde_json::json!({"path": f.display().to_string()});
        store
            .lock_ok()
            .tool_home_set(sid, "read_*", "elsewhere-node")
            .unwrap();
        let out = reg.dispatch("read_file", read.clone()).await;
        assert!(out.contains("elsewhere-node"), "{out}");
        assert!(!out.contains("on this node"), "ran here instead: {out}");
        // Through the tool_call envelope too (deferred tools arrive so).
        let out = reg
            .dispatch(
                "tool_call",
                serde_json::json!({"name": "read_file", "arguments": read.clone()}),
            )
            .await;
        assert!(out.contains("elsewhere-node"), "{out}");
        store
            .lock_ok()
            .tool_home_set(sid, "read_*", "local")
            .unwrap();
        let out = reg.dispatch("read_file", read).await;
        assert!(out.contains("on this node"), "{out}");
    }

    /// the other half: `tool_cap` cannot enumerate what a plugin
    /// registers, so the gate has to fail closed on the name it has
    /// never heard of.
    #[test]
    fn an_unclassified_tool_is_owner_only_not_ungated() {
        let (_home, _env) = crate::testenv::scratch_home();
        let mut cfg = crate::config::VelesConfig::default();
        assert_eq!(
            crate::acl::tool_gate("some_plugin_tool", &cfg),
            crate::acl::ToolGate::OwnerOnly
        );
        // The owner can classify it, and only for names veles does not
        // ship — a builtin's cap is not config-editable.
        cfg.tool_caps
            .insert("some_plugin_tool".into(), "web".into());
        cfg.tool_caps.insert("terminal".into(), "chat".into());
        assert_eq!(
            crate::acl::tool_gate("some_plugin_tool", &cfg),
            crate::acl::ToolGate::Cap(crate::acl::Cap::Web)
        );
        assert_eq!(
            crate::acl::tool_gate("terminal", &cfg),
            crate::acl::ToolGate::Cap(crate::acl::Cap::Shell),
            "config must not be able to lower a builtin's capability"
        );
        // An adapter can declare that its tool gates itself per action.
        crate::acl::declare_self_gated("irc");
        assert_eq!(
            crate::acl::tool_gate("irc", &cfg),
            crate::acl::ToolGate::SelfGated
        );
    }

    // ── whose call is it ─────────────────────────────────────────────

    /// The guest browser is an internal operation, not a tool: only the
    /// challenge relay and the web_extract escalation run it
    /// (`dispatch_internal`). A model that names it — directly, through
    /// `tool_call`, or in any casing — is told there is no such tool,
    /// even with the `web` capability it is gated on, and nothing is
    /// homed to the browser's node. Before the refusal in
    /// `dispatch_plain`, the gate passed it and the `browser_*` home
    /// would have carried it there.
    #[tokio::test]
    async fn a_model_cannot_call_the_guest_browser() {
        use crate::acl::{set_session_principal, Principal, Role};
        let (home, _guard) = crate::testenv::scratch_home();
        // Homed elsewhere, as the pool homes `browser_*` on the laptop:
        // without the refusal a call would travel there, not be unknown.
        std::fs::write(
            home.path().join("config.toml"),
            "[tools.home]\n\"browser_*\" = \"node-elsewhere\"\n",
        )
        .expect("config");
        let dir = crate::testenv::kept_dir();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.join("state.db")).expect("store"),
        ));
        let sid = "guest-op-user";
        let reg = crate::tools::builtin_registry(store, None, sid);
        set_session_principal(sid, Principal::known("irc:a", Role::User));
        let open = serde_json::json!({"action": "open", "url": "https://example.com/"});
        for (name, args) in [
            ("browser_guest", open.clone()),
            ("Browser_Guest", open.clone()),
            (
                "tool_call",
                serde_json::json!({"name": "browser_guest", "arguments": open}),
            ),
        ] {
            let out = reg.dispatch(name, args).await;
            assert!(out.contains("unknown tool"), "{name}: {out}");
        }
        assert!(reg.get("browser_guest").is_none(), "in no registry");
        crate::acl::clear_session_principal(sid);
    }

    /// And the gate it does pass is `web`'s: a guest, who has no `web`,
    /// is refused before anything is homed.
    #[test]
    fn the_guest_browser_takes_what_web_extract_takes() {
        use crate::acl::{Cap, Role};
        assert_eq!(crate::acl::tool_cap("browser_guest"), Some(Cap::Web));
        assert!(Role::User.default_caps().contains(&Cap::Web));
        assert!(!Role::Guest.default_caps().contains(&Cap::Web));
    }

    /// `tool_call` is judged on the tool INSIDE it, at the dispatcher's
    /// gate as at the agent loop's. Judging the envelope asked for its
    /// own cap, `mcp` (admin+), so a user was refused web_search — a cap
    /// their role has (IRC, 2026-10-02) — and with `tier = "core"` that
    /// was every deferred tool for everyone below admin.
    #[tokio::test]
    async fn tool_call_is_judged_on_the_tool_inside_it() {
        use crate::acl::{set_session_principal, Principal, Role};
        let (_home, _guard) = crate::testenv::scratch_home();
        let dir = crate::testenv::kept_dir();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.join("state.db")).expect("store"),
        ));
        let (user, guest) = ("envelope-user", "envelope-guest");
        let user_reg = crate::tools::builtin_registry(store.clone(), None, user);
        let guest_reg = crate::tools::builtin_registry(store, None, guest);
        set_session_principal(user, Principal::known("irc:a", Role::User));
        set_session_principal(guest, Principal::known("irc:stranger", Role::Guest));

        let web = serde_json::json!({"name": "web_search", "arguments": {}});
        let out = user_reg.dispatch("tool_call", web.clone()).await;
        assert!(
            out.contains("query is required"),
            "a user's web_search must run: {out}"
        );
        let out = guest_reg.dispatch("tool_call", web).await;
        assert!(
            out.contains("access denied") && out.contains("`web_search`") && out.contains("`web`"),
            "a guest has no `web`, and is told so by the tool's name: {out}"
        );
        // What the role does not hold stays shut through the envelope.
        for (tool, why) in [
            ("tcl_eval", "owner-only"),
            ("mcp__files__read", "`mcp`"),
            ("generate_video", "`video-gen`"),
        ] {
            let out = user_reg
                .dispatch(
                    "tool_call",
                    serde_json::json!({"name": tool, "arguments": {}}),
                )
                .await;
            assert!(
                out.contains("access denied") && out.contains(why),
                "{tool} through tool_call: {out}"
            );
        }
    }

    /// The identity a plugin's `veles::tool_call` is judged by.
    ///
    /// `set_global_registry` is last-writer-wins, so a call made from a
    /// GUEST's hook used to be gated on whichever session booted last —
    /// an owner, usually. Both directions were wrong: the guest borrowed
    /// the owner's capabilities, and an owner's call could be denied
    /// something a guest may do. Resolution is now the named session,
    /// then the ambient hook session, then the global cell; this pins all
    /// three and the two ways it must fail.
    #[test]
    fn a_plugin_call_is_judged_as_its_own_session_not_the_last_one_published() {
        // Both maps this touches are process-global, and so is the lock
        // every other registry-publishing test in this binary holds
        // (`testenv::scratch_home`). Take it, or a parallel test's
        // publish lands between ours and the assertion below — the same
        // interleaving that made `hooks::registry_gate` necessary.
        let (_home, _guard) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("s.db")).unwrap(),
        ));
        let (guest, owner) = ("identity-guest", "identity-owner");
        let guest_reg = Arc::new(crate::tools::builtin_registry(store.clone(), None, guest));
        let owner_reg = Arc::new(crate::tools::builtin_registry(store.clone(), None, owner));
        publish_session_registry(guest, guest_reg);
        publish_session_registry(owner, owner_reg.clone());
        // The owner booted LAST — the condition that used to decide
        // every plugin call in the process.
        set_global_registry(owner_reg);
        crate::acl::set_session_principal(
            guest,
            crate::acl::Principal::known("irc:stranger", crate::acl::Role::Guest),
        );
        // No principal for `owner`: an owner surface, full access.

        // 1. A named session is judged as itself.
        let (reg, judged) = plugin_call_registry(Some(guest), None).expect("guest registry");
        assert_eq!(reg.session_id(), guest);
        assert!(judged.contains(guest), "the note must name it: {judged}");
        assert!(
            reg.gate("terminal").is_some(),
            "a guest reached `terminal` through the plugin path"
        );

        // 2. The ambient session — what a hook handler gets for free —
        //    resolves to the same registry, and says why.
        let (reg, judged) = plugin_call_registry(None, Some(guest)).expect("ambient guest");
        assert_eq!(reg.session_id(), guest);
        assert!(judged.contains("hook now running"), "{judged}");

        // 3. An explicit session beats the ambient one: a handler that
        //    hops threads can still name its caller.
        let (reg, _) = plugin_call_registry(Some(owner), Some(guest)).expect("explicit wins");
        assert_eq!(reg.session_id(), owner);
        assert!(
            reg.gate("terminal").is_none(),
            "the owner must not be refused what a guest may do"
        );

        // 4. Neither: the global cell, exactly as before — named as the
        //    fallback it now is.
        let (reg, judged) = plugin_call_registry(None, None).expect("global fallback");
        assert_eq!(reg.session_id(), owner);
        assert!(judged.contains("last-published"), "{judged}");

        // 5. A NAMED session with nothing published is an error, not a
        //    fall-through: the caller told us whose call this is, and
        //    answering from somebody else's registry is the bug.
        let err = match plugin_call_registry(Some("identity-never-booted"), None) {
            Ok((reg, _)) => panic!(
                "a NAMED session with no published registry must not be answered from `{}`",
                reg.session_id()
            ),
            Err(e) => e,
        };
        assert!(err.contains("identity-never-booted"), "{err}");

        // 6. An ambient session whose registry is gone but whose
        //    PRINCIPAL is known fails closed — `SESSION_PRINCIPALS` is
        //    unbounded, so eviction is the only way to get here, and
        //    guessing is not an option.
        forget_session_registry(guest);
        let err = match plugin_call_registry(None, Some(guest)) {
            Ok((reg, _)) => panic!(
                "an evicted session WITH a principal must fail closed, not be judged as `{}`",
                reg.session_id()
            ),
            Err(e) => e,
        };
        assert!(err.contains(guest) && err.contains("refusing"), "{err}");

        // 7. …while an owner surface with no registry falls through
        //    honestly: no principal, full access, nothing to protect.
        forget_session_registry(owner);
        let (reg, _) = plugin_call_registry(None, Some(owner)).expect("owner falls through");
        assert_eq!(reg.session_id(), owner);
    }

    /// The per-session map is BOUNDED, unlike the two string-valued ones
    /// it sits beside: an `Arc<ToolRegistry>` holds every tool in the
    /// tree plus their handles, so an unbounded map of them is a leak
    /// with a session counter on it. Oldest goes first.
    #[test]
    fn the_session_registry_map_evicts_its_oldest_entry_at_the_cap() {
        let _guard = crate::testenv::lock();
        let first = "evict-oldest";
        publish_session_registry(first, Arc::new(ToolRegistry::new().for_session(first)));
        for i in 0..SESSION_REGISTRY_CAP {
            let sid = format!("evict-{i}");
            publish_session_registry(&sid, Arc::new(ToolRegistry::new().for_session(&sid)));
        }
        assert!(
            session_registry(first).is_none(),
            "the oldest entry must be evicted once the cap is passed"
        );
        let newest = format!("evict-{}", SESSION_REGISTRY_CAP - 1);
        assert!(
            session_registry(&newest).is_some(),
            "the newest entry must survive"
        );
        // Republishing a session (a resumed one builds a fresh registry)
        // puts it back at the END of the queue, so a live session is not
        // the next one out: the eviction this triggers takes the oldest
        // survivor (`evict-0`), not the entry just restored.
        publish_session_registry(first, Arc::new(ToolRegistry::new().for_session(first)));
        assert!(session_registry(first).is_some(), "republishing restores");
        assert!(
            session_registry("evict-0").is_none(),
            "the republish must evict the oldest survivor"
        );
        assert!(
            session_registry(&newest).is_some(),
            "…and leave the newest alone"
        );
        forget_session_registry(first);
        for i in 0..SESSION_REGISTRY_CAP {
            forget_session_registry(&format!("evict-{i}"));
        }
    }

    /// The ambient identity is scoped to one handler call — nested calls
    /// restore what they found, an empty session is no session, and a
    /// handler that PANICS does not leave its session behind for the next
    /// caller on this thread to be judged by.
    #[test]
    fn the_ambient_session_is_scoped_to_the_handler_call() {
        assert_eq!(ambient_hook_session(), None);
        with_ambient_hook_session(Some("outer"), || {
            assert_eq!(ambient_hook_session().as_deref(), Some("outer"));
            with_ambient_hook_session(Some("inner"), || {
                assert_eq!(ambient_hook_session().as_deref(), Some("inner"));
            });
            assert_eq!(
                ambient_hook_session().as_deref(),
                Some("outer"),
                "the inner scope must restore the outer one"
            );
        });
        assert_eq!(ambient_hook_session(), None, "and the outer one must clear");
        // "" means "no session", never a session named "".
        with_ambient_hook_session(Some(""), || assert_eq!(ambient_hook_session(), None));
        // The panic path. Noisy on purpose: the message below is the
        // handler's, not a failure.
        let _ = std::panic::catch_unwind(|| {
            with_ambient_hook_session(Some("panicking"), || panic!("handler blew up"));
        });
        assert_eq!(
            ambient_hook_session(),
            None,
            "a panicking handler must not leave its identity behind"
        );
    }

    /// The core tier is a BUDGET, not a vibe: this fails when someone
    /// adds a fat tool to core, which is the moment the cost should be
    /// argued, not discovered in a token bill.
    #[test]
    fn the_core_tier_is_under_the_declared_budget() {
        let (_home, _env) = crate::testenv::scratch_home();
        let r = tiered_registry();
        let schemas = r.openai_schemas();
        let bytes = serde_json::to_string(&schemas).unwrap().len();
        // 18 KB ≈ 4.5k tokens of resident schema. This was 16 KB until
        // the awareness fix promoted delegate_task, shell and monitor
        // from deferred to core — the store showed all three dark
        // (delegate_task: 8 calls ever, nothing since 2026-08-15; shell
        // and monitor: 0) while terminal ran 25k times, i.e. deferred ==
        // unused. The three schemas cost ~2.9 KB together (delegate_task
        // 1500 B, monitor 1001 B, shell 418 B — `veles tools` reports
        // each), taking the array to ~16.2 KB. That is real rent, argued
        // and accepted: parallel/background work is a daily-use
        // capability and a resident schema is the only one a model
        // reliably emits. The next fat promotion has to make the same
        // case or stay deferred.
        //
        // 18.5 KB since 2026-09-19, and the number that matters is the
        // one this test printed on the way: the array was at 17,828
        // against an 18,000 ceiling, i.e. 172 bytes of headroom, which
        // is a tripwire rather than a budget. `skill_view` then needed
        // `offset`/`limit` — two integers, ~177 bytes at their most
        // frugal, already stripped of every word `cap_warning` says
        // better at the point of use. The trade argued and accepted:
        // 177 bytes of resident schema buys back the ~26,000 chars of
        // SKILL.md that the tool-output cap was silently eliding from
        // the MIDDLE of every long skill load (USER 2026-09-19, of
        // rust-agent-runtime: "is the middle of it just invisible
        // forever now"). Raised to 18,500 rather than to 18,010,
        // because a ceiling you land 5 bytes under teaches the next
        // person to shave a description instead of making a case.
        //
        // 19,000 since 2026-09-26, when two branches that were each
        // under 18,500 on their own met in one merge. master's memory
        // tool had gained delete_file/move_file/list and `to`; the
        // clarify-parity branch gave clarify a `questions` array, which
        // costs ~509 B. Together they came to 18,833. Cutting the
        // duplicated wording (the hint was in the tool description AND
        // the property description) saved 182 B. What remains, ~327 B,
        // is the array's own structure, and that is the capability:
        // claude-code's AskUserQuestion asks up to 4 questions in one
        // dialog. The alternative is one clarify call per question,
        // and each call is a full model round-trip that re-sends the
        // whole context, which costs far more than 327 resident bytes.
        // Landed at 18,651.
        //
        // 19,700 since 2026-10-17, for `whoami` (503 B). Same case as
        // delegate_task/shell/monitor above, and sharper: the moment it
        // matters is a stranger in a channel claiming to be the owner
        // (the #flashsupport incident — /usage knew, the model had no
        // way to ask, and argued with its operator). A DEFERRED
        // authority check is one the model will not spend a discovery
        // round-trip on mid-argument. The schema is a no-property object
        // plus a description stripped to its trigger words.
        assert!(
            bytes < 19_700,
            "core schema array grew to {bytes} bytes — a core tool must stay cheap"
        );
        // Sanity: the discovery trio is always resident, whatever the
        // tier — they are core's handle on everything else.
        let names = r.names();
        for t in ["tool_search", "tool_describe", "tool_call", "terminal"] {
            assert!(
                names.iter().any(|n| n == t),
                "{t} must be resident: {names:?}"
            );
        }
    }

    /// A deferred tool is NOT a removed tool: tool_call reaches it,
    /// runs it, and the answer is the tool's own — never "unknown".
    #[tokio::test]
    async fn a_deferred_tool_is_still_reachable_via_tool_call() {
        let (_home, _env) = crate::testenv::scratch_home();
        let r = tiered_registry();
        // web_search is deferred tier; empty args make it answer
        // "query is required" — proof of life with no network call and no
        // side effects.
        //
        // Deliberately NOT skill_manage, which this used to assert was
        // deferred. That turned a tiering choice into a pinned invariant,
        // so the test went green on exactly the regression that took skill
        // writes to zero. A test naming ANY deferred tool proves the
        // property; naming one whose tier is a live product decision
        // defends the decision instead of the property.
        assert!(
            r.deferred_names().iter().any(|n| n == "web_search"),
            "the fixture needs at least one deferred tool to reach"
        );
        let out = r
            .dispatch(
                "tool_call",
                serde_json::json!({"name": "web_search", "arguments": {}}),
            )
            .await;
        assert!(
            !out.contains("Unknown deferred tool") && !out.contains("unknown tool"),
            "a deferred tool must stay invocable: {out}"
        );
        assert!(out.contains("query is required"), "it really ran: {out}");
    }

    /// `tool_search` matches word by word: "web search" — the example in
    /// its own schema — found nothing against `web_search`, and the
    /// model told IRC it had no web tool (2026-10-02).
    #[tokio::test]
    async fn tool_search_finds_a_tool_by_the_words_of_its_name() {
        let (_home, _env) = crate::testenv::scratch_home();
        let r = tiered_registry();
        let out = r
            .dispatch("tool_search", serde_json::json!({"query": "web search"}))
            .await;
        let first = out.lines().nth(1).unwrap_or_default();
        assert!(
            first.starts_with("  web_search — "),
            "the tool both words name comes first: {out}"
        );
        assert!(out.contains("  web_extract — "), "one word is a hit: {out}");
        let none = r
            .dispatch("tool_search", serde_json::json!({"query": "zzqx of"}))
            .await;
        assert!(none.starts_with("No deferred tools matched"), "{none}");
    }

    /// The one-line rollback is real: `[tools] tier = "full"` restores
    /// EXACTLY the historical everything-resident set.
    #[test]
    fn tier_full_restores_the_previous_set() {
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join("config.toml"),
            "[tools]\ntier = \"full\"\n",
        )
        .unwrap();
        let r = tiered_registry();
        assert!(r.deferred_names().is_empty(), "tier=full defers nothing");
        let mut names = r.names();
        names.sort();
        // The historical 40-tool interactive set (minus any adapter
        // extras, which register only when an adapter is up).
        let historical = [
            "a2a",
            "browser_back",
            "browser_click",
            "browser_console",
            "browser_navigate",
            "browser_press",
            "browser_scroll",
            "browser_snapshot",
            "browser_type",
            "clarify",
            "computer_use",
            "cronjob",
            "delegate_task",
            "execute_code",
            "focus_pane",
            "generate_image",
            "generate_video",
            "memory",
            "open_preview",
            "patch",
            "present_plan",
            "read_file",
            "search_files",
            "session_export",
            "session_search",
            "show_plan",
            "skill_manage",
            "skill_view",
            "skills_list",
            "tcl_eval",
            "terminal",
            "text_to_speech",
            "todo",
            "tool_call",
            "tool_describe",
            "tool_search",
            "vision_analyze",
            "web_extract",
            "web_search",
            "write_file",
        ];
        for t in historical {
            assert!(
                names.iter().any(|n| n == t),
                "tier=full must keep {t} resident"
            );
        }
    }

    /// Two registries holding the SAME tools registered in OPPOSITE
    /// order must serialize byte-identical schema arrays: the array is
    /// part of the cacheable prefix, and per-registry HashMap iteration
    /// order used to make every session ship a different byte-prefix.
    #[test]
    fn openai_schemas_is_deterministic_across_registries() {
        struct Named {
            name: &'static str,
        }
        #[async_trait::async_trait]
        impl Tool for Named {
            fn name(&self) -> &str {
                self.name
            }
            fn description(&self) -> &str {
                "d"
            }
            fn schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(&self, _args: Value) -> String {
                "ok".into()
            }
        }
        let mut a = ToolRegistry::new();
        let mut b = ToolRegistry::new();
        for n in ["terminal", "read_file", "memory", "clarify"] {
            a.register(Arc::new(Named { name: n }));
        }
        for n in ["clarify", "memory", "read_file", "terminal"] {
            b.register(Arc::new(Named { name: n }));
        }
        assert_eq!(
            serde_json::to_string(&a.openai_schemas()).unwrap(),
            serde_json::to_string(&b.openai_schemas()).unwrap(),
            "registration order and HashMap state must not leak into the wire prefix"
        );
    }

    #[test]
    fn schema_for_resolves_through_aliases() {
        let mut r = ToolRegistry::new();
        struct Echo;
        #[async_trait::async_trait]
        impl Tool for Echo {
            fn name(&self) -> &str {
                "terminal"
            }
            fn description(&self) -> &str {
                "x"
            }
            fn schema(&self) -> Value {
                serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}})
            }
            async fn call(&self, _args: Value) -> String {
                "ok".into()
            }
        }
        r.register(Arc::new(Echo));
        assert!(r.schema_for("bash").is_some(), "bash → terminal");
        assert!(r.schema_for("terminal").is_some());
        assert!(r.schema_for("missing").is_none());
    }

    // ── train-shaped tool names (case + alias table) ────────────────

    /// The names other harnesses trained models to emit must land on a
    /// real tool. Capitalisation is the whole reason this exists: the
    /// alias table is snake_case, the harnesses these models were RL'd
    /// on call their tools `Read`, `Bash`, `Edit`, `TodoWrite` — and a
    /// capital letter was enough to make every one of them "unknown
    /// tool". A model trained hard enough to emit `Read` emits it again
    /// next round, so that was a loop, not a single bad call.
    #[test]
    fn train_shaped_names_resolve_to_real_tools() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().expect("store dir");
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        let r = builtin_registry(store, None, "alias-test");

        let cases: &[(&str, &str)] = &[
            // Claude Code shapes, exactly as emitted.
            ("Read", "read_file"),
            ("Write", "write_file"),
            ("Edit", "patch"),
            ("Bash", "terminal"),
            ("Grep", "search_files"),
            ("Glob", "search_files"),
            ("WebFetch", "web_extract"),
            ("TodoWrite", "todo"),
            // OpenAI / Codex shapes.
            ("str_replace_editor", "patch"),
            ("str_replace_based_edit_tool", "patch"),
            ("create_file", "write_file"),
            // Plan shapes: Claude Code's real approval-exit name, and
            // the read-back under the names a model reaches for when it
            // wants "what was the plan". PascalCase variants prove the
            // fold, not extra rows.
            ("exit_plan_mode", "present_plan"),
            ("ExitPlanMode", "present_plan"),
            ("get_plan", "show_plan"),
            ("GetPlan", "show_plan"),
            ("ShowPlan", "show_plan"),
            ("current_plan", "show_plan"),
            ("plan_status", "show_plan"),
            // A real tool reached by casing alone.
            ("Terminal", "terminal"),
            ("READ_FILE", "read_file"),
        ];
        for (emitted, want) in cases {
            assert_eq!(
                &r.resolve_alias(emitted),
                want,
                "`{emitted}` must resolve to `{want}`"
            );
        }
    }

    /// An exact registry name always wins over any folding, and a name
    /// that means nothing is still left alone — it must reach the
    /// "unknown tool, did you mean" path rather than being bent onto
    /// something that merely looks similar.
    #[test]
    fn resolution_never_invents_a_target() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().expect("store dir");
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        let r = builtin_registry(store, None, "alias-test");

        assert_eq!(r.resolve_alias("terminal"), "terminal");
        assert_eq!(r.resolve_alias("read_file"), "read_file");
        for nonsense in ["frobnicate", "Wibble", "definitely_not_a_tool"] {
            assert_eq!(
                r.resolve_alias(nonsense),
                nonsense,
                "`{nonsense}` must stay unresolved, not be guessed at"
            );
        }
    }

    /// THE shell-shadowing regression, pinned. `shell` is a real tool
    /// (inspect/stop background shells) and was ALSO an alias row for
    /// `terminal`. The alias won, so `shell` never reached ShellTool:
    /// `{action:"kill",id:N}` came back as `[shell → terminal]` plus
    /// terminal's "command required", and the model — reading that as
    /// "there is no shell tool here" — went and pkill'd cargo. A
    /// registered tool must ALWAYS win over the alias table, by exact
    /// name and by case.
    #[test]
    fn shell_resolves_to_the_shell_tool_not_terminal() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().expect("store dir");
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        let r = builtin_registry(store, None, "alias-test");
        for emitted in ["shell", "Shell", "SHELL"] {
            assert_eq!(
                r.resolve_alias(emitted),
                "shell",
                "`{emitted}` must reach the shell tool, not the terminal alias"
            );
        }
        // The old alias name still works for the thing it always meant:
        // `bash` is not a registered tool, so it stays an alias.
        assert_eq!(r.resolve_alias("bash"), "terminal");
    }

    /// The hard rule that makes the shell fix structural instead of
    /// "keep the table clean and hope": NO alias key may name a tool
    /// that is actually registered (resident or deferred). Such a row is
    /// dead at best (resolve_alias prefers the registry) and a lie at
    /// worst (a future re-order re-shadows the tool). Fail the build on
    /// one rather than discovering it as a model that cannot reach a
    /// tool by its own name.
    #[test]
    fn no_alias_may_shadow_a_registered_tool() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().expect("store dir");
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        let r = builtin_registry(store.clone(), None, "alias-shadow-test");
        let mut registered: Vec<String> = r.names();
        registered.extend(r.deferred_names());
        let shadows: Vec<String> = r
            .alias_keys()
            .into_iter()
            .filter(|k| registered.iter().any(|t| t == k))
            .collect();
        assert!(
            shadows.is_empty(),
            "these alias keys name a registered tool and would shadow it: {shadows:?}"
        );
        // And a config-supplied alias is held to the same rule when it
        // is overlaid: set_aliases must drop a row that would shadow.
        let r2 = builtin_registry(store.clone(), None, "alias-shadow-test");
        r2.set_aliases(std::collections::HashMap::from([(
            "terminal".to_string(),
            "read_file".to_string(),
        )]));
        assert_eq!(
            r2.resolve_alias("terminal"),
            "terminal",
            "a config alias must not shadow the real `terminal` either"
        );
    }

    /// `suggest` must never echo the name it was given. It answers "did
    /// you mean X?" — a suggestion equal to the misspelling the model
    /// already emitted teaches nothing and reads as a bug. With `shell`
    /// now a real tool (not an alias), the edit-distance walk over alias
    /// keys could surface `shell` for an input of `shell`; guard it.
    #[test]
    fn suggest_never_returns_its_own_input() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().expect("store dir");
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        let r = builtin_registry(store, None, "suggest-test");
        for misspell in ["shel", "termnial", "delegat_task", "shelll"] {
            if let Some(s) = r.suggest(misspell) {
                assert_ne!(
                    s, misspell,
                    "`{misspell}` was suggested itself — a no-op correction"
                );
            }
        }
    }

    /// The awareness fix, pinned: delegate_task, shell and monitor are
    /// RESIDENT (core tier) in a default interactive session, so they
    /// are in the schema array the provider sees — the whole point. They
    /// were deferred on a "rare calls" theory, but the store showed the
    /// rarity WAS the defect (delegate_task: 8 calls ever, dark since
    /// 2026-08-15; shell and monitor: 0). This is the same promotion
    /// that took skill_manage off the deferred tier after its writes
    /// went to zero. `[tools.overrides]` can still demote any of them.
    #[test]
    fn delegation_and_background_tools_are_resident_by_default() {
        let (_home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().expect("store dir");
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        let r = builtin_registry(store, None, "tier-awareness-test");
        let resident = r.names();
        let deferred = r.deferred_names();
        for tool in ["delegate_task", "shell", "monitor"] {
            assert!(
                resident.iter().any(|n| n == tool),
                "`{tool}` must be core-resident so the model sees it: resident={resident:?}"
            );
            assert!(
                !deferred.iter().any(|n| n == tool),
                "`{tool}` must not be on the deferred tier"
            );
        }
        // The genuinely-heavy, genuinely-rare ones stay deferred.
        for tool in ["cronjob", "a2a"] {
            assert!(
                deferred.iter().any(|n| n == tool),
                "`{tool}` should stay deferred: deferred={deferred:?}"
            );
        }
    }

    /// End to end through dispatch: a Claude-shaped call — wrong tool
    /// NAME and wrong parameter NAME together — must actually run and
    /// must say what it did, so the model can conform next round
    /// instead of repeating the same call.
    #[tokio::test]
    async fn a_claude_shaped_call_runs_and_says_what_it_rewrote() {
        let (home, _env) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().expect("store dir");
        let store = Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        let r = builtin_registry(store, None, "claude-shape");

        let target = home.path().join("hello.txt");
        std::fs::write(&target, "hello world\n").unwrap();

        // `Read` is Claude Code's name; `file_path` is its parameter.
        // Neither is what veles calls them.
        assert_eq!(r.resolve_alias("Read"), "read_file");
        let schema = r
            .schema_for("Read")
            .expect("aliases resolve for schema too");
        let mut args = serde_json::json!({ "file_path": target.to_str().unwrap() });
        let mut notes = Vec::new();
        crate::repair::rename_param_aliases(&mut args, &schema, &mut notes);
        assert_eq!(notes.len(), 1, "the rename is reported: {notes:?}");

        let out = r.dispatch("Read", args).await;
        assert!(out.contains("hello world"), "the call actually ran: {out}");
        assert!(
            out.contains("[Read → read_file]"),
            "and the transcript names the alias rather than hiding it: {out}"
        );
    }

    /// STOP MEANS STOP, mid-command. Cancellation was only ever checked
    /// BETWEEN rounds, so a running tool call owned the turn for its whole
    /// timeout and the Stop button looked broken (USER, testing a
    /// `sleep 300` loop: "it won't stop even when I press stop.."). The
    /// terminal tool now watches the same LIVE_TURNS flag the cancel
    /// endpoint flips, kills the WHOLE process group, and returns within
    /// one poll — and the group kill is asserted, not assumed: a
    /// backgrounded child that would outlive an abandoned wait must not
    /// run to completion.
    #[tokio::test]
    async fn terminal_call_is_interrupted_mid_command_by_a_turn_cancel() {
        let sid = "terminal-cancel-mid-call";
        let tok = crate::providers::CancelToken::new();
        crate::agent::register_turn_for_test(sid, tok.clone());

        // The orphan canary: a backgrounded subshell that touches a file
        // after 3s. kill_on_drop alone kills the shell and ORPHANS this —
        // only a process-group kill stops it, so "the file never appears"
        // is the difference between abandoning the wait and killing the
        // tree.
        let canary =
            std::env::temp_dir().join(format!("veles-cancel-canary-{}", std::process::id()));
        let _ = std::fs::remove_file(&canary);
        #[cfg(unix)]
        let cmd = format!("(sleep 3; touch {}) & wait", canary.display());
        // The shell is PowerShell here (platform::default_shell), and the
        // POSIX line above is a parse error to it: it exited in 0.3 s and
        // this test passed only while a cold Windows PowerShell took
        // longer than the 0.7 s below to say so (measured on winbox,
        // 2026-10-14). The same shape in its words: a detached child that
        // writes the canary after 3 s (a parent-only kill leaves it
        // running — measured), under a parent that waits.
        #[cfg(windows)]
        let cmd = format!(
            "Start-Process -WindowStyle Hidden {} -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep 3; New-Item -ItemType File -Path ''{}'''; Start-Sleep 30",
            crate::shell::shell_binary(),
            canary.display()
        );

        let tool = TerminalTool {
            session_id: sid.to_string(),
        };
        let call = crate::rt::spawn(async move {
            tool.call(serde_json::json!({"command": cmd, "timeout": 60}))
                .await
        });
        // Let the child get properly started, then cancel exactly the way
        // POST /api/sessions/:id/cancel does.
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        assert!(
            !call.is_finished(),
            "a 3s wait must still be running at 0.7s"
        );
        tok.cancel();

        let started = std::time::Instant::now();
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), call)
            .await
            .expect("the call must return promptly after cancel, not ride out the command")
            .expect("no panic");
        let elapsed = started.elapsed();
        assert!(out.contains("cancelled"), "says what happened: {out}");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "returns within ~one poll of the cancel, took {elapsed:?}"
        );

        // Wait past the canary's 3s fuse: if the group was merely
        // abandoned rather than killed, the file appears now.
        tokio::time::sleep(std::time::Duration::from_millis(3200)).await;
        assert!(
            !canary.exists(),
            "the process group must be KILLED, not orphaned — the backgrounded child ran to completion"
        );
        let _ = std::fs::remove_file(&canary);
        crate::agent::clear_turn_for_test(sid);
    }

    /// F3, tool-level — post-user-envs there are TWO shell paths:
    ///
    /// - below-admin (User here): the call routes into the caller's
    ///   env container. The env never forwards gateway env, so the
    ///   strip holds by construction (pinned as exactly ONE `-e` pair
    ///   in `user_envs::tests::exec_command_carries_no_caller_env`);
    ///   under `--lib` the env is offline, so the pin here is the
    ///   ROUTING marker plus canary-absence.
    /// - Admin/Owner (host zone): the F3 strip is real and pinned —
    ///   the canary is absent for admin (no Cap::All) and present for
    ///   the owner, whose children keep inherit-everything.
    #[tokio::test]
    async fn terminal_strips_inherited_secrets_for_non_owners() {
        let (_home, _env) = crate::testenv::scratch_home();
        const VAR: &str = "VELES_F3_CANARY_SASL";
        const VAL: &str = "f3-leak-canary-value";
        let prev = std::env::var(VAR).ok();
        std::env::set_var(VAR, VAL);

        // Below-admin: sandbox zone, no env can cross.
        let user_sid = "f3-terminal-user";
        crate::acl::set_session_principal(
            user_sid,
            crate::acl::Principal::known("irc:tester", crate::acl::Role::User),
        );
        let out = TerminalTool {
            session_id: user_sid.to_string(),
        }
        .call(serde_json::json!({
            "command": canary_or_gone()
        }))
        .await;
        assert!(
            out.contains("[env: sandbox"),
            "a below-admin terminal routes into the env: {out}"
        );
        assert!(
            !out.contains(VAL),
            "the env route must not carry the host env: {out}"
        );

        // Admin over a connector: the SANDBOX by default now (USER,
        // 2026-09-09: "Admins/owners should have an option to act
        // outside of the sandbox, but use the sandbox by default"), so
        // the host path needs an explicit `host: true` — and the
        // strip still removes the canary when they take it.
        let admin_sid = "f3-terminal-admin";
        crate::acl::set_session_principal(
            admin_sid,
            crate::acl::Principal::known("irc:boss", crate::acl::Role::Admin),
        );
        let out = TerminalTool {
            session_id: admin_sid.to_string(),
        }
        .call(serde_json::json!({
            "command": canary_or_gone()
        }))
        .await;
        assert!(
            out.contains("[env: sandbox"),
            "an admin defaults INTO a sandbox: {out}"
        );
        let out = TerminalTool {
            session_id: admin_sid.to_string(),
        }
        .call(serde_json::json!({
            "command": canary_or_gone(),
            "host": true
        }))
        .await;
        assert!(
            !out.contains("[env: sandbox"),
            "…and opts out per call: {out}"
        );
        assert!(
            !out.contains(VAL) && out.contains("GONE"),
            "the admin's host shell does not inherit secret-bearing env: {out}"
        );

        // The OWNER surface — no principal on the session at all, the
        // human at the keyboard — keeps the host and keeps its
        // environment. A session with an owner PRINCIPAL is a connector
        // owner and follows the admin rule above.
        let owner_sid = "f3-terminal-owner-surface";
        let out = TerminalTool {
            session_id: owner_sid.to_string(),
        }
        .call(serde_json::json!({
            "command": canary_or_gone()
        }))
        .await;
        assert!(
            out.contains(VAL),
            "the owner's children still inherit everything: {out}"
        );

        match prev {
            Some(v) => std::env::set_var(VAR, v),
            None => std::env::remove_var(VAR),
        }
    }

    /// The success path the cancel test above short-circuits past: a
    /// permitted command's stdout AND stderr must come BACK in the result
    /// string. `call()` returns at the cancel arm before the
    /// output-capture arm, so the cancel test stays green even when
    /// capture is broken — which is exactly how the output()→spawn()
    /// refactor (Stop-mid-command) shipped a terminal tool that returned
    /// "" for every command: spawn() inherits stdio, only output()
    /// auto-pipes, and wait_with_output() then collected nothing. Assert
    /// the bytes actually come back.
    #[tokio::test]
    async fn terminal_call_returns_captured_stdout_and_stderr() {
        let tool = TerminalTool {
            session_id: "terminal-capture".to_string(),
        };
        // No turn registered for this session, so the cancel watch never
        // resolves and the command's own completion wins the select.
        let out = tool
            .call(serde_json::json!({
                "command": if cfg!(windows) {
                    "Write-Output OUT_MARKER; [Console]::Error.WriteLine('ERR_MARKER'); exit 3"
                } else {
                    "echo OUT_MARKER; echo ERR_MARKER >&2; exit 3"
                }
            }))
            .await;
        assert!(out.contains("OUT_MARKER"), "stdout is captured: {out:?}");
        assert!(
            out.contains("[stderr]") && out.contains("ERR_MARKER"),
            "stderr is captured and labelled: {out:?}"
        );
        assert!(
            out.contains("[exit code: 3]"),
            "a non-zero exit is reported: {out:?}"
        );
    }

    /// A command that leaves something running answers when its SHELL
    /// exits. The pipes reach EOF only when every holder closes them, and
    /// the unbounded join waited on the leftover — past the timeout and
    /// deaf to Stop, both checked only while the shell runs. That hung a
    /// live session for good (2026-09-28, d544b5: `cd … && tclsh mock.tcl
    /// & MOCK=$!` backgrounds the whole `&&` list, so `kill $MOCK` killed
    /// the subshell and orphaned the mock on the tool's stdout).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_that_leaves_a_process_running_still_answers() {
        let tool = TerminalTool {
            session_id: "terminal-leftover".to_string(),
        };
        let call = tool.call(serde_json::json!({
            "command": "sleep 30 & echo LEFTOVER_MARKER",
            "timeout": 60
        }));
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), call)
            .await
            .expect("the call answers when the shell exits, not when its leftover does");
        assert!(out.contains("LEFTOVER_MARKER"), "{out}");
        assert!(out.contains("left running"), "says what it left: {out}");
        // The note names the group to kill: use it, which also tidies up.
        let pgid: i32 = out
            .split("kill -- -")
            .nth(1)
            .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse().ok())
            .expect("the note names the process group");
        assert_eq!(unsafe { libc::kill(-pgid, libc::SIGKILL) }, 0, "{out}");
    }

    /// A wake owner that owns exactly one session — the gateway or
    /// desktop server, reduced to the one question the tools ask.
    struct OwnsOne(&'static str);
    impl crate::session_wake::WakeOwner for OwnsOne {
        fn own_sid(&self, session_id: &str) -> bool {
            session_id == self.0
        }
        fn admit_and_run(
            &self,
            _event: crate::session_wake::WakeEvent,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
    }

    /// The advice a model gets while background work runs is TRUE for
    /// its session: end the turn where a wake owner will start the next
    /// one, wait with `shell wait` where nothing will (a subagent's
    /// one-shot process exits with its turn).
    #[test]
    fn wait_advice_follows_whether_the_session_wakes_on_its_own() {
        let owner: Arc<dyn crate::session_wake::WakeOwner> = Arc::new(OwnsOne("wakes-sid"));
        crate::session_wake::register_owner(&owner);
        let woken = wait_advice("wakes-sid", Some(7));
        assert!(woken.contains("END YOUR TURN"), "{woken}");
        let alone = wait_advice("a-subagent-sid", Some(7));
        assert!(alone.contains("does NOT wake"), "{alone}");
        assert!(alone.contains("shell{action:\"wait\",id:7}"), "{alone}");
        drop(owner);
        assert!(
            !crate::session_wake::wakes_when_idle("wakes-sid"),
            "an owner that went away no longer counts"
        );
    }

    /// A long FOREGROUND sleep is refused with what to do instead; a
    /// short one, a poll loop and a background sleep still run.
    #[tokio::test]
    async fn a_long_foreground_sleep_is_refused_and_short_ones_run() {
        let (_home, _env) = crate::testenv::scratch_home();
        let tool = TerminalTool {
            session_id: "sleep-guard".into(),
        };
        let out = tool
            .call(serde_json::json!({"command": "sleep 240; tail -c 200 /nonexistent"}))
            .await;
        assert!(out.starts_with("error: not run"), "{out}");
        assert!(out.contains("sleeps 240s"), "{out}");
        assert!(
            out.contains("shell{action:\"wait\""),
            "names the alternative: {out}"
        );
        let out = tool
            .call(serde_json::json!({"command": "sleep 0.1; echo SHORT_OK"}))
            .await;
        assert!(out.contains("SHORT_OK"), "a short sleep runs: {out}");
        let out = tool
            .call(serde_json::json!({"command": "sleep 60", "background": true}))
            .await;
        assert!(
            out.contains("\"started\""),
            "a background sleep runs: {out}"
        );
        let id = serde_json::from_str::<Value>(&out).unwrap()["task_id"]
            .as_u64()
            .unwrap() as u32;
        assert!(crate::shells::kill(id));
    }

    /// A background shell asked for in a NAMED sandbox runs in that one,
    /// as the foreground command does — not in the caller's own box. It
    /// went to the caller's own (measured on the phone 2026-10-20: a
    /// `sandbox: "box-a"` background shell wrote into `dm_local_owner`'s
    /// /work while its answer said `[env: sandbox box-a]`).
    #[tokio::test]
    async fn a_background_shell_runs_in_the_sandbox_it_names() {
        let (_home, _env) = crate::testenv::scratch_home();
        let tool = TerminalTool {
            session_id: "bg-named-box".into(),
        };
        let out = tool
            .call(serde_json::json!({
                "command": "true",
                "background": true,
                "sandbox": "box-a",
            }))
            .await;
        assert!(out.contains("[env: sandbox box-a]"), "{out}");
        let body = out.split_once('\n').map(|(_, b)| b).unwrap_or(&out);
        let id = serde_json::from_str::<Value>(body).unwrap()["task_id"]
            .as_u64()
            .unwrap() as u32;
        let d = crate::shells::detail(id).expect("the shell is registered");
        assert_eq!(d["sandbox"], "box-a", "{d}");
        // No name: the caller's own box, as before.
        let out = tool
            .call(serde_json::json!({"command": "true", "background": true, "host": false}))
            .await;
        let body = out.split_once('\n').map(|(_, b)| b).unwrap_or(&out);
        let id = serde_json::from_str::<Value>(body).unwrap()["task_id"]
            .as_u64()
            .unwrap() as u32;
        let own = crate::shells::detail(id).unwrap()["sandbox"].clone();
        assert!(own.as_str().is_some_and(|n| n.starts_with("dm:")), "{own}");
    }

    /// `shell wait` returns the moment the shell finishes — what a
    /// foreground `sleep N; tail log` approximated badly — and drops the
    /// queued [BACKGROUND SHELL COMPLETE], which would only repeat it.
    /// A wait that times out says the shell is still running.
    #[tokio::test]
    async fn shell_wait_returns_when_the_shell_finishes() {
        let sid = "shell-wait-e2e";
        let id = crate::shells::start(
            "sleep 0.5; echo WAITED_OUTPUT",
            sid,
            crate::user_envs::ZoneArg::Unset,
        );
        let tool = ShellTool {
            session_id: sid.into(),
        };
        let started = std::time::Instant::now();
        let out = tool
            .call(serde_json::json!({"action": "wait", "id": id, "timeout": 20}))
            .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "returned when the shell ended, not at the timeout"
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "done", "{v}");
        assert!(
            v["output"].as_str().unwrap().contains("WAITED_OUTPUT"),
            "{v}"
        );
        assert!(
            !crate::delegate::has_background_events(sid),
            "the completion this result already carries is not queued again"
        );

        let id = crate::shells::start("sleep 30", sid, crate::user_envs::ZoneArg::Unset);
        let out = tool
            .call(serde_json::json!({"action": "wait", "id": id, "timeout": 1}))
            .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "running", "{v}");
        assert!(v["note"].as_str().unwrap().contains("still running"), "{v}");
        assert!(crate::shells::kill(id));
    }

    /// Stage 4, the whole fg→bg move in one test: a long-running
    /// foreground command, a `/detach`-shaped request mid-call, and
    /// the assertions that make it a MOVE and not a restart or a
    /// kill:
    ///
    ///  - the tool call RETURNS promptly with `{status: detached,
    ///    task_id}` instead of riding out the command;
    ///  - the registry knows the shell (same id space as
    ///    `background=true`);
    ///  - the process is STILL ALIVE after the call returned (detach
    ///    must not kill — that is /cancel's job);
    ///  - the ring carries the pre-detach output (lossless: the pumps
    ///    fed it from the first line);
    ///  - killing through the registry actually stops it.
    #[tokio::test]
    async fn terminal_foreground_call_detaches_into_a_live_background_shell() {
        let (home, _env) = crate::testenv::scratch_home();
        // This test NEEDS a long foreground command to detach, which is
        // exactly what the foreground-sleep guard refuses: off here.
        std::fs::write(
            home.path().join("config.toml"),
            "[agent.guards]\nforeground_sleep_max_secs = 0\n",
        )
        .unwrap();
        let sid = "terminal-detach-e2e";
        let tok = crate::providers::CancelToken::new();
        crate::agent::register_turn_for_test(sid, tok.clone());

        let tool = TerminalTool {
            session_id: sid.to_string(),
        };
        let call = crate::rt::spawn(async move {
            tool.call(serde_json::json!({
                "command": "echo PRE_DETACH_MARKER; sleep 60",
                "timeout": 55,
            }))
            .await
        });

        // Wait until the bg waiter has actually spawned the child and
        // armed the fg marker — request_detach refuses before then
        // (correctly: there is nothing to move yet).
        let mut armed = false;
        for _ in 0..200 {
            if crate::agent::fg_command_active(sid) {
                armed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(armed, "the fg marker must arm while the command waits");
        assert!(
            crate::agent::request_detach(sid),
            "request_detach must succeed with a live turn inside a fg command"
        );

        let out = tokio::time::timeout(std::time::Duration::from_secs(10), call)
            .await
            .expect("a detached call must return promptly, not ride out `sleep 60`")
            .expect("no panic");
        let v: serde_json::Value =
            serde_json::from_str(&out).expect("the detach answer is JSON, not prose");
        assert_eq!(v["status"], "detached", "says what happened: {out}");
        let id = v["task_id"].as_u64().expect("names the shell") as u32;
        assert!(
            crate::tasks::get(id).is_some(),
            "the shell is in the SAME registry a background=true call uses"
        );

        // Give the pumps a moment, then the lossless-history assertion:
        // output printed BEFORE the detach is in the ring.
        for _ in 0..100 {
            if crate::shells::output(id)
                .unwrap_or_default()
                .contains("PRE_DETACH_MARKER")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let text = crate::shells::output(id).unwrap_or_default();
        assert!(
            text.contains("PRE_DETACH_MARKER"),
            "pre-detach output survives the move (the ring was fed from line one): {text:?}"
        );
        assert!(
            crate::tasks::get(id).map(|t| t.status) == Some(crate::tasks::TaskStatus::Running),
            "the process is STILL RUNNING after the tool call returned — detach is a move, not a kill"
        );

        // And the registry kill path stops it, like any shell.
        assert!(
            crate::shells::kill(id),
            "the owner-facing kill works on an adopted shell"
        );
        for _ in 0..100 {
            if crate::tasks::get(id).map(|t| t.status) != Some(crate::tasks::TaskStatus::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            crate::tasks::get(id).map(|t| t.status),
            Some(crate::tasks::TaskStatus::Cancelled),
            "an adopted shell killed through the registry reports Cancelled, like a born one"
        );

        // The fg marker and detach flag are both clear: a stale flag
        // must not ambush the next terminal call in this session.
        assert!(
            !crate::agent::fg_command_active(sid),
            "the fg marker disarms when the call returns"
        );
        assert!(
            !crate::agent::request_detach(sid),
            "no phantom detach is armed afterwards"
        );
        crate::agent::clear_turn_for_test(sid);
    }

    /// `request_detach` refuses HONESTLY in the two no-op cases, and
    /// the refusal is the whole safety story: without the fg-marker
    /// precondition, a `/detach` typed between tool rounds would arm a
    /// flag the NEXT unrelated terminal call silently consumes.
    #[test]
    fn detach_refuses_when_nothing_detachable_is_running() {
        // No turn at all.
        assert!(
            !crate::agent::request_detach("detach-refuses-no-turn"),
            "no turn → nothing to detach"
        );
        // A live turn, but not inside a foreground terminal command.
        let sid = "detach-refuses-live-turn";
        let tok = crate::providers::CancelToken::new();
        crate::agent::register_turn_for_test(sid, tok);
        assert!(
            !crate::agent::request_detach(sid),
            "a turn outside a fg command → nothing to detach"
        );
        assert!(
            !crate::agent::detach_requested(sid),
            "and no flag was armed behind the refusal"
        );
        crate::agent::clear_turn_for_test(sid);
    }

    /// The recursion guard has two halves and they must agree.
    ///
    /// `call` refuses at depth ≥ 1, which is the half that cannot be
    /// bypassed. But the tool was still REGISTERED in a child's
    /// registry, so a subagent read "Spawn subagents…" in its own
    /// schema array, emitted a call, and was told no — a wasted round
    /// trip and a contradiction inside one prompt. Found by running
    /// `/tools` inside a real depth-1 child during the 2026-09-18
    /// audit, not by reading.
    #[test]
    fn a_child_is_not_offered_the_tool_its_own_guard_will_refuse() {
        let _g = crate::testenv::lock();
        let dir = tempfile::tempdir().expect("scratch");
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.path().join("state.db")).expect("store"),
        ));
        let reachable = |r: &ToolRegistry| {
            let mut v = r.names();
            v.extend(r.deferred_names());
            v
        };

        std::env::remove_var("VELES_CHILD_DEPTH");
        let parent = builtin_registry(store.clone(), None, "s");
        assert!(
            reachable(&parent).iter().any(|n| n == "delegate_task"),
            "the operator's own surface still has it"
        );

        std::env::set_var("VELES_CHILD_DEPTH", "1");
        let child = builtin_registry(store.clone(), None, "s");
        std::env::remove_var("VELES_CHILD_DEPTH");
        assert!(
            !reachable(&child).iter().any(|n| n == "delegate_task"),
            "a child must not be offered a door its own guard closes: {:?}",
            reachable(&child)
        );
        // …and the guard itself is untouched: it is what makes this
        // unbypassable for any OTHER dispatch path (the plugin bridge,
        // tool_call, a stale registry).
        std::env::set_var("VELES_CHILD_DEPTH", "1");
        let t = crate::delegate::DelegateTaskTool {
            session_id: "s".into(),
            hooks: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(crate::tools::Tool::call(
            &t,
            serde_json::json!({"goal": "x"}),
        ));
        std::env::remove_var("VELES_CHILD_DEPTH");
        assert!(
            out.contains("VELES_CHILD_DEPTH"),
            "the call-time refusal stands on its own: {out}"
        );
    }

    /// M3 / plan R2: a session's handle on a MODULE's tool follows the
    /// registry. Once the module is purged the call is refused (no
    /// unloaded code runs), and once it registers again the same handle
    /// reaches the new build. A core tool is held as itself.
    #[tokio::test]
    async fn a_late_bound_module_tool_follows_a_reload() {
        struct V(&'static str);
        #[async_trait::async_trait]
        impl Tool for V {
            fn name(&self) -> &str {
                "lb-reload-probe"
            }
            fn description(&self) -> &str {
                "probe"
            }
            fn schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(&self, _: Value) -> String {
                self.0.to_string()
            }
        }
        let owner = "module:lb-probe";
        let as_module = |v: &'static str| {
            let prev = crate::lifecycle::thread_owner();
            crate::lifecycle::set_thread_owner(owner);
            register_global_tool(Arc::new(V(v)));
            crate::lifecycle::clear_thread_owner_for_test(prev);
        };
        as_module("v1");
        let (tool, who) = extra_tools()
            .values_with_owners()
            .into_iter()
            .find(|(t, _)| t.name() == "lb-reload-probe")
            .expect("registered");
        assert_eq!(who, Some(owner));
        let handle = session_tool(tool, who, "sess-lb");
        let args = serde_json::json!({});
        assert_eq!(handle.call(args.clone()).await, "v1");

        assert_eq!(purge_owner_tools(Some(owner)), 1);
        let gone = handle.call(args.clone()).await;
        assert!(gone.contains("not available"), "{gone}");

        as_module("v2");
        assert_eq!(handle.call(args.clone()).await, "v2");
        purge_owner_tools(Some(owner));

        let core: Arc<dyn Tool> = Arc::new(V("core"));
        let held = session_tool(core.clone(), None, "sess-lb");
        assert!(Arc::ptr_eq(&core, &held), "a core tool is held directly");
    }
}
