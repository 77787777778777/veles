//! Plugin services: one plugin using another's features (USER 2026-10-10).
//!
//! Before this, every feature one plugin offered another was its own
//! core slot: the IRC adapter reached slopdrop's trigger engine through
//! `triggers::Forwarder`, a trait only an in-process module could fill,
//! and reached the atproto plugin's AppView reads through the process
//! host's `call_service`, which only Rust inside the gateway could call.
//! A plugin author had no way to offer a feature to someone else's
//! plugin, and no way to see who depended on whom.
//!
//! So there is one registry. A **service** is a dotted name
//! (`appview.query`, `triggers.forward`), JSON params and a JSON answer,
//! and the contract is the provider's to document (the shared ones are
//! typed in `veles_types::plugin`). Every tier can provide one and every
//! tier can call one:
//!
//! | tier            | provides                                   | calls |
//! |-----------------|--------------------------------------------|-------|
//! | core / modules  | [`register`] in `start`                    | [`call`] |
//! | process plugins | its manifest `services`, answered by its `request` handler while it runs | `service.call` (gateway request) |
//! | Tcl scripts     | `veles::register_service`                  | `veles::service_call` |
//! | JS scripts      | `veles.registerService`                    | `veles.serviceCall` |
//!
//! Entries are owner-tracked like tools and hooks: a registration carries
//! its registering thread's owner, a module's or script's unload purges
//! exactly its services, and when two providers register one name the
//! LATER one answers until it goes, then the earlier one is in effect
//! again (USER: "latest wins, earlier returns").
//!
//! **Who may call** is the PROVIDER's decision, per service, in
//! [`ServiceDecl::cap`] (USER 2026-10-10):
//! - unset — internal: core and in-process modules only. They are
//!   compiled into the gateway and share its trust; a process plugin or
//!   a script is refused. The bot's write services (`post.create`,
//!   `repo.delete_record`, `hosted.delete`) and `triggers.forward` are
//!   internal: third-party code must not post as the bot.
//! - `"any"` — every caller (`appview.query`, a read).
//! - an ACL capability (`web`, `shell`, …) — a process plugin's or a
//!   script's call is judged as the session it runs for, like
//!   `veles::tool_call`: that session's principal must hold the
//!   capability. No session, no call.
//!
//! **Dependencies** are declared, not inferred: a plugin lists what it
//! provides (`services`) and what it uses (`uses`, required or optional)
//! in its manifest (scripts: `provides_services` / `uses_services` /
//! `optional_services` in plugin.toml). [`declare`] records them,
//! [`report`] joins them with the live registry — who provides each
//! service, who uses it, what is missing — and a failed call names the
//! plugin that declares the service when none provides it here.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

pub use veles_types::plugin::{ServiceDecl, UseDecl, SERVICE_CAP_ANY};

use crate::acl::Cap;
use crate::owned::OwnedRegistry;

/// A provider's answer, on whatever runtime the caller awaits it.
pub type ServiceFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'static>>;

/// A provider's handler: the params and who is calling.
pub type Handler = Arc<dyn Fn(Value, CallCx) -> ServiceFuture + Send + Sync>;

/// How long any call may take before the caller gets an error instead of
/// a hang. A caller that needs an answer sooner wraps its own timeout
/// (the Bluesky preview waits 8 s).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Per-service deadlines, over the global [`CALL_TIMEOUT`]. Most services
/// answer in well under two minutes, so the global ceiling is right for
/// them; a few wait on something genuinely long (a vision describe waits
/// on a model that may think for minutes) and opt into a longer one with
/// [`set_deadline`] at registration. Keyed by service name and consulted
/// in [`run_layer`], so the override is per call site and never widens a
/// service that did not ask for it.
fn deadlines() -> &'static Mutex<BTreeMap<String, Duration>> {
    static D: OnceLock<Mutex<BTreeMap<String, Duration>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Give one service its own call deadline, over the global
/// [`CALL_TIMEOUT`]. Called at registration by a service whose work is
/// legitimately long. [`Duration::ZERO`] clears it (back to the global).
pub fn set_deadline(name: &str, d: Duration) {
    let mut m = deadlines().lock().unwrap_or_else(|p| p.into_inner());
    if d.is_zero() {
        m.remove(name);
    } else {
        m.insert(name.to_string(), d);
    }
}

/// The deadline `name` opted into, if any.
pub fn deadline_for(name: &str) -> Option<Duration> {
    deadlines().lock().ok()?.get(name).copied()
}

/// Who is calling, as the provider sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallCx {
    /// `core`, `module:irc:synirc`, `process:atproto`, `tcl:<name>`,
    /// `js:<name>`.
    pub caller: String,
    /// The session the call is made for, when there is one.
    pub session_id: Option<String>,
    /// The `next` token of this invocation (PLAN-extension-api.md D2):
    /// [`call_next`] with it runs the layer BELOW this provider, while
    /// this call runs. Opaque and issued per call, so it can neither be
    /// guessed nor kept.
    pub next: Option<String>,
}

impl CallCx {
    /// What the layer below this provider answers to `params`.
    pub async fn next(&self, params: Value) -> Result<Value, String> {
        match &self.next {
            Some(token) => call_next(token, params).await,
            None => Err("this call carries no `next`".into()),
        }
    }
}

/// Who may call a service ([`ServiceDecl::cap`], parsed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Core and in-process modules only.
    Internal,
    /// Every caller.
    Any,
    /// A plugin's call is judged as its session's principal.
    Cap(Cap),
}

impl Access {
    /// Parse a declaration's `cap`. An unknown name is an error at
    /// registration, never a silently open (or silently closed) service.
    pub fn of(cap: Option<&str>) -> Result<Access, String> {
        match cap.map(str::trim) {
            None | Some("") => Ok(Access::Internal),
            Some(c) if c.eq_ignore_ascii_case(SERVICE_CAP_ANY) => Ok(Access::Any),
            Some(c) => Cap::from_str_lossy(c).map(Access::Cap).ok_or_else(|| {
                format!(
                    "cap `{c}` is neither `{SERVICE_CAP_ANY}` nor an ACL capability \
                     (leave it unset for an internal service)"
                )
            }),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Access::Internal => "internal".into(),
            Access::Any => SERVICE_CAP_ANY.into(),
            Access::Cap(c) => format!("cap {}", c.as_str()),
        }
    }
}

#[derive(Clone)]
struct Entry {
    decl: ServiceDecl,
    access: Access,
    provider: String,
    handler: Handler,
    /// The Tcl proc behind it, when a Tcl plugin provides it: a Tcl
    /// caller runs the proc on its own thread (see [`tcl_proc`]).
    tcl_proc: Option<String>,
}

fn registry() -> &'static OwnedRegistry<String, Entry> {
    static R: OnceLock<OwnedRegistry<String, Entry>> = OnceLock::new();
    R.get_or_init(OwnedRegistry::default)
}

/// A service name: dotted lowercase segments (`group.verb`), at most 64
/// characters. The dot is required, so every name says whose area it is.
pub fn valid_name(s: &str) -> bool {
    s.len() <= 64
        && s.contains('.')
        && s.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        })
}

fn this_owner() -> String {
    crate::lifecycle::thread_owner()
        .unwrap_or("core")
        .to_string()
}

/// Provide `decl.name` with `handler`, under the calling thread's owner
/// (a module's `start`, a script's load). A later registration of the
/// same name by another owner answers instead until it is purged.
pub fn register(decl: ServiceDecl, handler: Handler) -> Result<(), String> {
    insert(decl, handler, None)
}

/// [`register`] for a Tcl plugin's proc: `handler` serves every other
/// caller (it posts to the interpreter thread); a Tcl caller runs the
/// proc itself, on that thread.
pub fn register_tcl(decl: ServiceDecl, procname: &str, handler: Handler) -> Result<(), String> {
    insert(decl, handler, Some(procname.to_string()))
}

fn insert(decl: ServiceDecl, handler: Handler, tcl_proc: Option<String>) -> Result<(), String> {
    if !valid_name(&decl.name) {
        return Err(format!(
            "service name `{}` must be dotted lowercase segments (`group.verb`), at most 64",
            decl.name
        ));
    }
    let access = Access::of(decl.cap.as_deref()).map_err(|e| format!("{}: {e}", decl.name))?;
    // Core's points exist before any registration lands, so a plugin
    // overriding one goes on top of core's default, never under it.
    ensure_core_points();
    let provider = this_owner();
    let covered = registry()
        .owner_of(&decl.name)
        .map(|o| o.unwrap_or("core").to_string())
        .filter(|o| o != &provider);
    let name = decl.name.clone();
    registry().insert(
        name.clone(),
        Entry {
            decl,
            access,
            provider: provider.clone(),
            handler,
            tcl_proc,
        },
    );
    match covered {
        Some(prev) => {
            tracing::info!(
                "services: {name} provided by {provider} ({}), over {prev}",
                access.describe()
            );
            // An override of a decision core asks is announced: nothing
            // a plugin changes about how veles decides is silent.
            if is_point(&name) {
                crate::notes::record_plugin_note(
                    &format!("`{name}` is now answered by {provider}, over {prev}"),
                    "an override point: unloading the plugin puts the answer below back",
                );
            }
        }
        None => tracing::debug!(
            "services: {name} provided by {provider} ({})",
            access.describe()
        ),
    }
    Ok(())
}

/// [`register`] under `owner` whatever the calling thread: the process
/// host registers a plugin's services as `process:<label>`.
pub fn register_for(
    owner: &'static str,
    decl: ServiceDecl,
    handler: Handler,
) -> Result<(), String> {
    crate::lifecycle::with_thread_owner(owner, || register(decl, handler))
}

/// Withdraw the calling owner's registration of `name`.
pub fn withdraw(name: &str) -> bool {
    registry().remove_owned(&name.to_string(), crate::lifecycle::thread_owner())
}

/// Withdraw `owner`'s registration of `name` (an ABI 4 `unregister`).
pub fn withdraw_owned(name: &str, owner: Option<&'static str>) -> bool {
    registry().remove_owned(&name.to_string(), owner)
}

/// Remove everything `owner` provides (a module's or script's unload, a
/// process plugin's exit). What it covered answers again.
pub fn purge_owner(owner: Option<&'static str>) -> usize {
    // Who answers each point before and after: a point whose answer
    // changes is announced, as its override was.
    let names: Vec<String> = points().keys();
    let before: Vec<Option<String>> = names.iter().map(|n| provider(n)).collect();
    let n = registry().purge(owner);
    points().purge(owner);
    for (name, was) in names.iter().zip(before) {
        let now = provider(name);
        if was.is_some() && was != now {
            let gone = owner.unwrap_or("core");
            crate::notes::record_plugin_note(
                &format!(
                    "`{name}` is answered by {} again",
                    now.as_deref()
                        .unwrap_or("nothing (its declarer is gone too)")
                ),
                &format!("{gone} was unloaded"),
            );
        }
    }
    n
}

/// Does anything here provide `name`?
pub fn available(name: &str) -> bool {
    registry().with(&name.to_string(), |_| ()).is_some()
}

/// Who answers `name` here.
pub fn provider(name: &str) -> Option<String> {
    registry().with(&name.to_string(), |e| e.provider.clone())
}

/// Every provider of `name` here, oldest first: the last answers, the
/// others are covered until it goes.
pub fn providers(name: &str) -> Vec<String> {
    registry()
        .owners_of(&name.to_string())
        .into_iter()
        .map(|o| o.unwrap_or("core").to_string())
        .collect()
}

fn lookup(name: &str) -> Result<Entry, String> {
    registry()
        .with(&name.to_string(), Clone::clone)
        .ok_or_else(|| missing(name))
}

/// Why a call found nothing: and who would answer it, if a plugin
/// declares it (installed elsewhere, not running here, …).
fn missing(name: &str) -> String {
    let declared: Vec<String> = declarations()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .filter(|(_, d)| d.provides.iter().any(|s| s.name == name))
        .map(|(plugin, _)| plugin.clone())
        .collect();
    if declared.is_empty() {
        format!("no plugin provides the service `{name}`")
    } else {
        format!(
            "no running plugin provides the service `{name}` on this node \
             (declared by {}: not running here, or not started yet)",
            declared.join(", ")
        )
    }
}

/// Run `name` from the top of its stack for `cx`'s caller and session.
async fn run(entry: Entry, params: Value, cx: CallCx) -> Result<Value, String> {
    let name = entry.decl.name.clone();
    let top = stack(&name).len().saturating_sub(1);
    run_layer(name, top, params, cx.caller, cx.session_id).await
}

// ── override points and `next` (PLAN-extension-api.md D2) ────────────

pub use veles_types::plugin::{OnError, PointDecl};

fn points() -> &'static OwnedRegistry<String, PointDecl> {
    static P: OnceLock<OwnedRegistry<String, PointDecl>> = OnceLock::new();
    P.get_or_init(OwnedRegistry::default)
}

/// Declare an override point and register its default answer at the
/// BOTTOM of `decl.name`'s service stack, under the calling thread's
/// owner (core's is never purged; a module's goes with the module, the
/// point with it). A plugin that registered the name before the
/// declaration stays on top: load order never decides who answers.
pub fn declare_point(decl: PointDecl, default: Handler) -> Result<(), String> {
    if !valid_name(&decl.name) {
        return Err(format!(
            "point name `{}` must be dotted lowercase segments (`group.verb`), at most 64",
            decl.name
        ));
    }
    let name = decl.name.clone();
    let entry = Entry {
        decl: ServiceDecl {
            name: name.clone(),
            description: decl.summary.clone(),
            cap: None,
        },
        access: Access::Internal,
        provider: this_owner(),
        handler: default,
        tcl_proc: None,
    };
    points().insert(name.clone(), decl);
    registry().insert_bottom(name, entry);
    Ok(())
}

/// [`declare_point`] as CORE's, whatever the calling thread: never
/// purged. Core's points are declared lazily ([`ensure_core_points`]),
/// and the first caller can be a module's thread.
pub fn declare_core_point(decl: PointDecl, default: Handler) -> Result<(), String> {
    if !valid_name(&decl.name) {
        return Err(format!(
            "point name `{}` must be dotted lowercase segments (`group.verb`), at most 64",
            decl.name
        ));
    }
    let name = decl.name.clone();
    let entry = Entry {
        decl: ServiceDecl {
            name: name.clone(),
            description: decl.summary.clone(),
            cap: None,
        },
        access: Access::Internal,
        provider: "core".into(),
        handler: default,
        tcl_proc: None,
    };
    points().insert_core(name.clone(), decl);
    registry().insert_core_bottom(name, entry);
    Ok(())
}

/// The points core declares, as published in the extension API spec.
pub fn core_point_decls() -> Vec<PointDecl> {
    vec![
        crate::failover::point_decl(),
        crate::points::prompt_decl(),
        crate::points::tool_decl(),
        crate::points::inbound_decl(),
    ]
}

/// Declare core's points (each once per process). Run before anything
/// asks whether a point is overridden and before any registration lands,
/// so an override registered at any moment sits above core's default.
pub fn ensure_core_points() {
    crate::failover::ensure_point();
    crate::points::ensure_points();
}

/// The point declared as `name`, if one is.
pub fn point(name: &str) -> Option<PointDecl> {
    points().with(&name.to_string(), Clone::clone)
}

pub fn is_point(name: &str) -> bool {
    points().with(&name.to_string(), |_| ()).is_some()
}

/// Does anything answer `name` above its declarer's default? A seam asks
/// this first and, when it is false, runs its own code directly: no JSON,
/// no stack, the cost of a lock and a length check.
pub fn overridden(name: &str) -> bool {
    ensure_core_points();
    let key = name.to_string();
    // A point a module declares is declared once per instance (one per
    // IRC network): those defaults are the point's own, not overrides.
    // An override is a provider that is not one of its declarers.
    let declarers = points().owners_of(&key);
    !declarers.is_empty()
        && registry()
            .owners_of(&key)
            .iter()
            .any(|o| !declarers.contains(o))
}

/// Every declared point (the listing; the spec reads core's from code).
pub fn declared_points() -> Vec<PointDecl> {
    points().values_cloned()
}

/// Ask point `name`: its stack from the top, each failed override
/// falling through to the layer below when the point says so. The call
/// is core's (or the declaring module's), for `session_id`.
pub async fn ask(name: &str, input: Value, session_id: Option<String>) -> Result<Value, String> {
    if !is_point(name) {
        return Err(format!("`{name}` is not a declared override point"));
    }
    let top = stack(name).len().saturating_sub(1);
    run_layer(name.to_string(), top, input, this_owner(), session_id).await
}

/// [`ask`] from synchronous code (the prompt builder): on a scoped
/// thread of its own, driving a small runtime of its own. Blocking the
/// caller's thread on an async call where that thread drives a runtime
/// panics or deadlocks, and the prompt builder is called from inside
/// async code; a fresh thread has no runtime to block. Bounded by the
/// point's deadline per layer, like any ask.
pub fn ask_blocking(name: &str, input: Value, session_id: Option<String>) -> Result<Value, String> {
    let name = name.to_string();
    std::thread::scope(|s| {
        s.spawn(move || blocking_rt().block_on(ask(&name, input, session_id)))
            .join()
            .unwrap_or_else(|_| Err("the point's call panicked".to_string()))
    })
}

fn blocking_rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("veles-point-ask")
            .enable_all()
            .build()
            .expect("the point-ask runtime")
    })
}

/// One `next` token's context: whose layer it was issued to, and the call
/// it belongs to.
#[derive(Clone)]
struct NextSlot {
    name: String,
    provider: String,
    caller: String,
    session_id: Option<String>,
}

fn next_slots() -> &'static Mutex<std::collections::HashMap<String, NextSlot>> {
    static S: OnceLock<Mutex<std::collections::HashMap<String, NextSlot>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// A `next` token, valid while this guard lives (one handler call).
pub struct NextGuard(String);

impl Drop for NextGuard {
    fn drop(&mut self) {
        next_slots()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

fn issue_next(
    name: &str,
    provider: &str,
    caller: &str,
    session_id: Option<String>,
) -> (String, NextGuard) {
    let token = uuid::Uuid::new_v4().simple().to_string();
    next_slots()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(
            token.clone(),
            NextSlot {
                name: name.to_string(),
                provider: provider.to_string(),
                caller: caller.to_string(),
                session_id,
            },
        );
    (token.clone(), NextGuard(token))
}

/// A token for the provider on top of `name`, for a caller that runs that
/// provider itself (the Tcl fast path, where the provider is a proc on
/// the calling thread).
pub fn next_for_top(
    name: &str,
    caller: &str,
    session_id: Option<String>,
) -> Option<(String, NextGuard)> {
    let provider = provider(name)?;
    Some(issue_next(name, &provider, caller, session_id))
}

/// The entries of `name`, bottom first.
fn stack(name: &str) -> Vec<Entry> {
    registry()
        .stack(&name.to_string())
        .into_iter()
        .map(|(_, e)| e)
        .collect()
}

/// The layer below the one `token` was issued to, as the stack stands now:
/// its index, and the provider entry. `Err` names why there is none.
fn below(token: &str) -> Result<(NextSlot, usize, Entry), String> {
    let slot = next_slots()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(token)
        .cloned()
        .ok_or_else(|| {
            "`next` with an unknown or spent token: a token is valid only while the call \
             that carried it runs"
                .to_string()
        })?;
    let stack = stack(&slot.name);
    let at = stack
        .iter()
        .position(|e| e.provider == slot.provider)
        .ok_or_else(|| {
            format!(
                "{} no longer provides `{}`: nothing to call `next` from",
                slot.provider, slot.name
            )
        })?;
    if at == 0 {
        return Err(format!(
            "nothing is below {} on `{}`: it is the bottom layer",
            slot.provider, slot.name
        ));
    }
    let entry = stack[at - 1].clone();
    Ok((slot, at - 1, entry))
}

/// The Tcl proc behind the layer below `token`'s, when a Tcl plugin
/// provides it (the Tcl fast path of `veles::service_next`).
pub fn tcl_proc_below(token: &str) -> Option<String> {
    below(token).ok().and_then(|(_, _, e)| e.tcl_proc)
}

/// What the layer below `token`'s provider answers to `params`: `next`,
/// for a provider wrapping what it covers. The layer runs for the same
/// caller and session as the call `token` came with, with a token of its
/// own; a failing override below falls through as [`ask`]'s do.
pub async fn call_next(token: &str, params: Value) -> Result<Value, String> {
    let (slot, at, _) = below(token)?;
    run_layer(slot.name, at, params, slot.caller, slot.session_id).await
}

/// A token and payload for the layer below `token`'s, for a caller that
/// runs that layer itself (the Tcl fast path): `(payload, guard)`.
pub fn next_payload_below(token: &str, params: &Value) -> Result<(Value, NextGuard), String> {
    let (slot, _, entry) = below(token)?;
    let (tok, guard) = issue_next(
        &slot.name,
        &entry.provider,
        &slot.caller,
        slot.session_id.clone(),
    );
    let cx = CallCx {
        caller: slot.caller,
        session_id: slot.session_id,
        next: Some(tok),
    };
    Ok((script_payload(params, &cx), guard))
}

/// Run layer `idx` (0 = bottom) of `name`'s stack. A point's answer is
/// held to its output schema, and a failed override falls through to the
/// layer below when the point says `fall_through`.
fn run_layer(
    name: String,
    idx: usize,
    params: Value,
    caller: String,
    session_id: Option<String>,
) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>> {
    Box::pin(async move {
        let stack = stack(&name);
        let Some(entry) = stack.get(idx).cloned() else {
            return Err(missing(&name));
        };
        let point = point(&name);
        // The point's deadline is the extension-API contract and wins;
        // else the service's own opt-in (`set_deadline`); else the global
        // ceiling. A long-running service asks for its longer budget at
        // registration rather than widening every other service.
        let limit = point
            .as_ref()
            .map(|p| Duration::from_millis(p.deadline_ms.max(1)))
            .or_else(|| deadline_for(&name))
            .unwrap_or(CALL_TIMEOUT);
        let (token, guard) = issue_next(&name, &entry.provider, &caller, session_id.clone());
        let cx = CallCx {
            caller: caller.clone(),
            session_id: session_id.clone(),
            next: Some(token),
        };
        let answer = match tokio::time::timeout(limit, (entry.handler)(params.clone(), cx)).await {
            Ok(r) => r,
            Err(_) => Err(format!(
                "{name} ({}) did not answer within {:.1}s",
                entry.provider,
                limit.as_secs_f64()
            )),
        };
        drop(guard);
        let Some(point) = point else {
            return answer;
        };
        let answer = answer.and_then(|v| {
            check_output(&point.output, &v).map(|()| v).map_err(|e| {
                format!(
                    "{name} ({}) answered outside the point's schema: {e}",
                    entry.provider
                )
            })
        });
        match answer {
            Err(e) if point.on_error == OnError::FallThrough && idx > 0 => {
                let next_down = stack[idx - 1].provider.clone();
                tracing::warn!("{e}; {next_down} answers instead");
                crate::notes::record_plugin_note(
                    &format!(
                        "the override of `{name}` by {} failed; {next_down} answered",
                        entry.provider
                    ),
                    &e,
                );
                run_layer(name, idx - 1, params, caller, session_id).await
            }
            other => other,
        }
    })
}

/// The part of JSON Schema a point's answer is held to: `type: object`,
/// `required`, and each property's `enum`. The rest of a schema documents
/// the contract; it is not enforced.
fn check_output(schema: &Value, v: &Value) -> Result<(), String> {
    if schema.get("type").and_then(Value::as_str) == Some("object") && !v.is_object() {
        return Err(format!("expected an object, got {v}"));
    }
    for k in schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if v.get(k).is_none() {
            return Err(format!("no `{k}`"));
        }
    }
    if let Some(props) = schema["properties"].as_object() {
        for (k, p) in props {
            if let (Some(allowed), Some(got)) = (p["enum"].as_array(), v.get(k)) {
                if !allowed.contains(got) {
                    return Err(format!(
                        "`{k}` is {got}, not one of {}",
                        Value::Array(allowed.clone())
                    ));
                }
            }
        }
    }
    Ok(())
}

/// One point in the overrides view: who answers, top first.
#[derive(Debug, Clone, PartialEq)]
pub struct PointReport {
    pub name: String,
    pub summary: String,
    /// Providers, top (answering) first; the last is the default.
    pub stack: Vec<String>,
}

/// Every declared point and its stack (`/plugins overrides`).
pub fn points_report() -> Vec<PointReport> {
    ensure_core_points();
    let mut out: Vec<PointReport> = declared_points()
        .into_iter()
        .map(|p| {
            let mut stack = providers(&p.name);
            stack.reverse();
            PointReport {
                name: p.name,
                summary: p.summary,
                stack,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The overrides view as text.
pub fn overrides_listing() -> String {
    let rows = points_report();
    if rows.is_empty() {
        return "no override points declared".into();
    }
    let mut out = String::new();
    for r in rows {
        let state = match r.stack.as_slice() {
            [] => "nothing answers".to_string(),
            [only] => format!("← {only} (the default; no override)"),
            [top, rest @ ..] => format!("← {top}, over {}", rest.join(", ")),
        };
        out.push_str(&format!("{} {state}\n", r.name));
        if !r.summary.is_empty() {
            out.push_str(&format!("    {}\n", r.summary));
        }
    }
    out
}

/// The overrides view as JSON (`GET /api/plugins/overrides`).
pub fn overrides_json() -> Value {
    Value::Array(
        points_report()
            .into_iter()
            .map(|r| serde_json::json!({ "name": r.name, "summary": r.summary, "stack": r.stack }))
            .collect(),
    )
}

/// Call `name` from core or an in-process module: the same trust as
/// core, so no gate. The caller is the calling thread's owner.
pub async fn call(name: &str, params: Value) -> Result<Value, String> {
    let entry = lookup(name)?;
    let cx = CallCx {
        caller: this_owner(),
        session_id: crate::tools::ambient_hook_session(),
        next: None,
    };
    run(entry, params, cx).await
}

/// The Tcl proc behind `name`, when a Tcl plugin provides it. The Tcl
/// interpreter is ONE thread: a Tcl plugin calling a service another Tcl
/// plugin provides is already on that thread, and the handler's hop to
/// it would wait for itself forever. So the Tcl caller asks this, passes
/// [`admit_plugin_call`], and runs the proc in place (the same fast path
/// `veles::tool_call` takes for a Tcl tool).
pub fn tcl_proc(name: &str) -> Option<String> {
    registry()
        .with(&name.to_string(), |e| e.tcl_proc.clone())
        .flatten()
}

/// The gate and the announcement of a plugin-tier call, for a caller
/// that runs the provider itself ([`tcl_proc`]): `Ok` with the session
/// the call is judged for.
pub fn admit_plugin_call(
    caller: &str,
    name: &str,
    session_id: Option<String>,
) -> Result<Option<String>, String> {
    let entry = lookup(name)?;
    admit(&entry, caller, session_id)
}

fn admit(
    entry: &Entry,
    caller: &str,
    session_id: Option<String>,
) -> Result<Option<String>, String> {
    let name = &entry.decl.name;
    let session_id = session_id
        .filter(|s| !s.trim().is_empty())
        .or_else(crate::tools::ambient_hook_session);
    gate(entry, caller, session_id.as_deref())?;
    crate::notes::record_plugin_note(
        &format!("{caller} called the service `{name}`"),
        &format!(
            "provided by {} ({}){}",
            entry.provider,
            entry.access.describe(),
            session_id
                .as_deref()
                .map(|s| format!(", for session `{s}`"))
                .unwrap_or_default()
        ),
    );
    Ok(session_id)
}

/// Call `name` for a process plugin or a script (`caller` names it):
/// the provider's [`Access`] decides. Every such call is announced, as
/// `tool_call` is: nothing a plugin does is silent.
pub async fn call_from_plugin(
    caller: &str,
    name: &str,
    params: Value,
    session_id: Option<String>,
) -> Result<Value, String> {
    let entry = lookup(name)?;
    let session_id = admit(&entry, caller, session_id)?;
    let cx = CallCx {
        caller: caller.to_string(),
        session_id,
        next: None,
    };
    run(entry, params, cx).await
}

/// Payload a script's service callback receives: the params, and who is
/// calling for which session.
pub fn script_payload(params: &Value, cx: &CallCx) -> Value {
    serde_json::json!({
        "params": params,
        "caller": cx.caller,
        "session_id": cx.session_id,
        "next": cx.next,
    })
}

/// May a plugin-tier caller use this entry?
fn gate(entry: &Entry, caller: &str, session_id: Option<&str>) -> Result<(), String> {
    let name = &entry.decl.name;
    match entry.access {
        Access::Any => Ok(()),
        Access::Internal => Err(format!(
            "`{name}` is an internal service of {}: only core and in-process modules may call \
             it, not {caller}",
            entry.provider
        )),
        Access::Cap(cap) => {
            let Some(sid) = session_id else {
                return Err(format!(
                    "`{name}` needs the `{}` capability of the session it is called for, and \
                     {caller} named none (pass a session id, or call it from a hook)",
                    cap.as_str()
                ));
            };
            match crate::acl::session_principal(sid) {
                Some(p) if p.has(cap) => Ok(()),
                Some(p) => Err(format!(
                    "`{name}` needs the `{}` capability; session `{sid}` acts for {} ({}), \
                     who lacks it",
                    cap.as_str(),
                    p.subject,
                    p.role.as_str()
                )),
                None => Err(format!(
                    "`{name}` needs the `{}` capability, and session `{sid}` has no principal \
                     to judge (unknown, or an owner surface with none recorded)",
                    cap.as_str()
                )),
            }
        }
    }
}

// ── dependencies ────────────────────────────────────────────────────

/// What one plugin declares: the services it provides and uses.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Declared {
    pub provides: Vec<ServiceDecl>,
    pub uses: Vec<UseDecl>,
}

fn declarations() -> &'static Mutex<BTreeMap<String, Declared>> {
    static D: OnceLock<Mutex<BTreeMap<String, Declared>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Record what `plugin` declares (a host adopting its manifest or
/// plugin.toml). Kept while the plugin is installed, running or not:
/// "atproto provides appview.query, but it runs on another node" is the
/// answer a dependency view exists to give.
pub fn declare(plugin: &str, provides: Vec<ServiceDecl>, uses: Vec<UseDecl>) {
    let mut d = declarations().lock().unwrap_or_else(|p| p.into_inner());
    if provides.is_empty() && uses.is_empty() {
        d.remove(plugin);
    } else {
        d.insert(plugin.to_string(), Declared { provides, uses });
    }
}

/// One service in the dependency view.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceReport {
    pub name: String,
    pub description: String,
    /// Who answers here now, and with what access.
    pub provider: Option<String>,
    pub access: Option<String>,
    /// Providers it covers (they answer again when it goes).
    pub covered: Vec<String>,
    /// Plugins that declare providing it.
    pub declared_by: Vec<String>,
    /// `(plugin, optional)` for every plugin that declares using it.
    pub used_by: Vec<(String, bool)>,
}

impl ServiceReport {
    /// A required use with no provider here.
    pub fn missing_required(&self) -> bool {
        self.provider.is_none() && self.used_by.iter().any(|(_, optional)| !optional)
    }
}

/// Every service anything provides, declares or uses, by name.
pub fn report() -> Vec<ServiceReport> {
    let mut out: BTreeMap<String, ServiceReport> = BTreeMap::new();
    let blank = |name: &str| ServiceReport {
        name: name.to_string(),
        description: String::new(),
        provider: None,
        access: None,
        covered: Vec::new(),
        declared_by: Vec::new(),
        used_by: Vec::new(),
    };
    // Copy the rows out first, and ask who else provides each only once
    // the registry's read guard is gone. Asking from inside `for_each`
    // took the same RwLock twice: a writer queued between the two reads
    // (a script loading, a module unloading) and both waited forever,
    // and so did every later service call (the gate's veles-core tests
    // hung 25 minutes, 2026-10-10).
    let mut shown = Vec::new();
    registry().for_each(|name, e| {
        shown.push((
            name.clone(),
            e.value.provider.clone(),
            e.value.access.describe(),
            e.value.decl.description.clone(),
        ));
    });
    for (name, provider, access, description) in shown {
        let mut covered = providers(&name);
        covered.pop();
        let r = out.entry(name.clone()).or_insert_with(|| blank(&name));
        r.provider = Some(provider);
        r.access = Some(access);
        r.description = description;
        r.covered = covered;
    }
    let decls = declarations().lock().unwrap_or_else(|p| p.into_inner());
    for (plugin, d) in decls.iter() {
        for s in &d.provides {
            let r = out.entry(s.name.clone()).or_insert_with(|| blank(&s.name));
            r.declared_by.push(plugin.clone());
            if r.description.is_empty() {
                r.description = s.description.clone();
            }
        }
        for u in &d.uses {
            let r = out.entry(u.name.clone()).or_insert_with(|| blank(&u.name));
            r.used_by.push((plugin.clone(), u.optional));
        }
    }
    out.into_values().collect()
}

/// The dependency view as text (`/plugins services`).
pub fn listing() -> String {
    let rows = report();
    if rows.is_empty() {
        return "no services: no plugin provides or uses one".into();
    }
    let mut out = String::new();
    for r in &rows {
        let state = match (&r.provider, r.missing_required()) {
            (Some(p), _) if !r.covered.is_empty() => format!(
                "← {p} ({}), over {}",
                r.access.as_deref().unwrap_or("?"),
                r.covered.join(", ")
            ),
            (Some(p), _) => format!("← {p} ({})", r.access.as_deref().unwrap_or("?")),
            (None, true) => "MISSING (a required use has no provider here)".into(),
            (None, false) => "not provided here".into(),
        };
        out.push_str(&format!("{} {state}\n", r.name));
        if !r.description.is_empty() {
            out.push_str(&format!("    {}\n", r.description));
        }
        if !r.declared_by.is_empty() {
            out.push_str(&format!("    declared by: {}\n", r.declared_by.join(", ")));
        }
        if !r.used_by.is_empty() {
            let users: Vec<String> = r
                .used_by
                .iter()
                .map(|(p, optional)| {
                    if *optional {
                        format!("{p} (optional)")
                    } else {
                        p.clone()
                    }
                })
                .collect();
            out.push_str(&format!("    used by: {}\n", users.join(", ")));
        }
    }
    out
}

/// The dependency view as JSON (the `services` bridge op, the API).
pub fn report_json() -> Value {
    Value::Array(
        report()
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "description": r.description,
                    "provider": r.provider,
                    "access": r.access,
                    "covered": r.covered,
                    "declared_by": r.declared_by,
                    "used_by": r.used_by.iter().map(|(p, o)| serde_json::json!({
                        "plugin": p, "optional": o
                    })).collect::<Vec<_>>(),
                    "missing_required": r.missing_required(),
                })
            })
            .collect(),
    )
}

/// A handler from an async closure: `handler(|params, cx| async move { … })`.
pub fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(Value, CallCx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, String>> + Send + 'static,
{
    Arc::new(move |params, cx| Box::pin(f(params, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decl(name: &str, cap: Option<&str>) -> ServiceDecl {
        ServiceDecl {
            name: name.into(),
            description: format!("{name} for tests"),
            cap: cap.map(str::to_string),
        }
    }

    fn echo(tag: &'static str) -> Handler {
        handler(move |params, cx| async move {
            Ok(json!({"by": tag, "params": params, "caller": cx.caller}))
        })
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn names_are_dotted_lowercase() {
        for ok in [
            "appview.query",
            "triggers.forward",
            "repo.delete_record",
            "a.b-c.d1",
        ] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "nodot",
            "Appview.query",
            "a..b",
            ".a",
            "a.",
            "a b.c",
            &"a.".repeat(40),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(register(decl("nodot", None), echo("x")).is_err());
        assert!(register(decl("t.badcap", Some("nonsense")), echo("x")).is_err());
    }

    #[test]
    fn access_parses_from_the_declaration() {
        assert_eq!(Access::of(None), Ok(Access::Internal));
        assert_eq!(Access::of(Some(" ")), Ok(Access::Internal));
        assert_eq!(Access::of(Some("ANY")), Ok(Access::Any));
        assert_eq!(Access::of(Some("web")), Ok(Access::Cap(Cap::Web)));
        assert!(Access::of(Some("everyone")).is_err());
    }

    /// The latest provider answers; when it goes, the earlier one does.
    #[test]
    fn the_latest_provider_wins_and_the_earlier_returns() {
        let rt = rt();
        let a = crate::lifecycle::owner_tag("module:svc-test-a");
        let b = crate::lifecycle::owner_tag("module:svc-test-b");
        register_for(a, decl("svctest.latest", None), echo("a")).unwrap();
        register_for(b, decl("svctest.latest", None), echo("b")).unwrap();
        assert_eq!(provider("svctest.latest").as_deref(), Some(b));
        assert_eq!(
            providers("svctest.latest"),
            vec![a.to_string(), b.to_string()]
        );
        let row = report()
            .into_iter()
            .find(|r| r.name == "svctest.latest")
            .unwrap();
        assert_eq!(row.covered, vec![a.to_string()]);
        assert!(listing().contains(&format!("over {a}")));
        let v = rt.block_on(call("svctest.latest", json!(1))).unwrap();
        assert_eq!(v["by"], "b");
        assert_eq!(purge_owner(Some(b)), 1);
        let v = rt.block_on(call("svctest.latest", json!(2))).unwrap();
        assert_eq!(v["by"], "a", "the earlier provider answers again");
        purge_owner(Some(a));
        assert!(!available("svctest.latest"));
        let err = rt.block_on(call("svctest.latest", json!(3))).unwrap_err();
        assert!(err.contains("no plugin provides"), "{err}");
    }

    /// A service may opt into its own deadline, over the global
    /// [`CALL_TIMEOUT`], and a call that overruns names the budget that
    /// actually expired. This is the outer half of the vision fix: a
    /// describe waits on a model that may think for minutes, but the
    /// global ceiling must stay where it is for every other service.
    #[test]
    fn a_service_can_opt_into_its_own_deadline() {
        let rt = rt();
        let owner = crate::lifecycle::owner_tag("module:svc-test-deadline");
        register_for(
            owner,
            decl("svctest.slow", None),
            handler(|_p, _cx| async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                Ok(json!("late"))
            }),
        )
        .unwrap();

        // Under the global ceiling it would time out; with a per-service
        // deadline over the sleep it answers. The error text carries the
        // budget that expired, so a log says which ceiling fired.
        set_deadline("svctest.slow", Duration::from_millis(50));
        assert_eq!(
            deadline_for("svctest.slow"),
            Some(Duration::from_millis(50))
        );
        let err = rt.block_on(call("svctest.slow", json!({}))).unwrap_err();
        assert!(err.contains("did not answer within 0.1s"), "{err}");

        set_deadline("svctest.slow", Duration::from_secs(30));
        assert_eq!(
            rt.block_on(call("svctest.slow", json!({}))),
            Ok(json!("late"))
        );

        // Clearing the opt-in is back to the global ceiling, and another
        // service's deadline is untouched by any of this.
        set_deadline("svctest.slow", Duration::ZERO);
        assert_eq!(deadline_for("svctest.slow"), None);
        assert_eq!(deadline_for("svctest.latest"), None);
        purge_owner(Some(owner));
    }

    /// Internal services refuse plugin-tier callers; `any` lets them in;
    /// in-process callers are never gated.
    #[test]
    fn the_provider_decides_who_may_call() {
        let rt = rt();
        let owner = crate::lifecycle::owner_tag("module:svc-test-gate");
        register_for(owner, decl("svctest.internal", None), echo("i")).unwrap();
        register_for(owner, decl("svctest.open", Some("any")), echo("o")).unwrap();
        register_for(owner, decl("svctest.web", Some("web")), echo("w")).unwrap();

        let err = rt
            .block_on(call_from_plugin(
                "js:other",
                "svctest.internal",
                json!({}),
                None,
            ))
            .unwrap_err();
        assert!(err.contains("internal service"), "{err}");
        assert!(rt.block_on(call("svctest.internal", json!({}))).is_ok());

        let v = rt
            .block_on(call_from_plugin(
                "js:other",
                "svctest.open",
                json!({"x": 1}),
                None,
            ))
            .unwrap();
        assert_eq!(v["caller"], "js:other");
        assert_eq!(v["params"]["x"], 1);

        // A cap-gated service needs a session whose principal holds it.
        let err = rt
            .block_on(call_from_plugin(
                "tcl:other",
                "svctest.web",
                json!({}),
                None,
            ))
            .unwrap_err();
        assert!(err.contains("named none"), "{err}");
        let sid = "svc-test-session-guest";
        crate::acl::set_session_principal(sid, crate::acl::Principal::guest());
        let err = rt
            .block_on(call_from_plugin(
                "tcl:other",
                "svctest.web",
                json!({}),
                Some(sid.into()),
            ))
            .unwrap_err();
        assert!(err.contains("lacks it"), "{err}");
        let sid = "svc-test-session-owner";
        crate::acl::set_session_principal(sid, crate::acl::Principal::owner());
        assert!(rt
            .block_on(call_from_plugin(
                "tcl:other",
                "svctest.web",
                json!({}),
                Some(sid.into())
            ))
            .is_ok());
        purge_owner(Some(owner));
    }

    /// A missing service names the plugin that declares it.
    #[test]
    fn declarations_feed_the_dependency_view() {
        let rt = rt();
        declare(
            "svc-test-provider",
            vec![decl("svctest.declared", Some("any"))],
            Vec::new(),
        );
        declare(
            "svc-test-user",
            Vec::new(),
            vec![
                UseDecl {
                    name: "svctest.declared".into(),
                    optional: false,
                    description: "needs it".into(),
                },
                UseDecl {
                    name: "svctest.nobody".into(),
                    optional: true,
                    description: String::new(),
                },
            ],
        );
        let err = rt
            .block_on(call("svctest.declared", json!({})))
            .unwrap_err();
        assert!(err.contains("declared by svc-test-provider"), "{err}");
        let rows = report();
        let r = rows.iter().find(|r| r.name == "svctest.declared").unwrap();
        assert!(r.missing_required(), "{r:?}");
        assert_eq!(r.declared_by, vec!["svc-test-provider".to_string()]);
        let n = rows.iter().find(|r| r.name == "svctest.nobody").unwrap();
        assert!(!n.missing_required(), "an optional use is not missing");
        let text = listing();
        assert!(text.contains("svctest.declared MISSING"), "{text}");
        assert!(text.contains("svc-test-user (optional)"), "{text}");

        let owner = crate::lifecycle::owner_tag("module:svc-test-provider");
        register_for(owner, decl("svctest.declared", Some("any")), echo("p")).unwrap();
        let rows = report();
        let r = rows.iter().find(|r| r.name == "svctest.declared").unwrap();
        assert_eq!(r.provider.as_deref(), Some("module:svc-test-provider"));
        assert!(!r.missing_required());
        purge_owner(Some(owner));
        declare("svc-test-provider", Vec::new(), Vec::new());
        declare("svc-test-user", Vec::new(), Vec::new());
    }

    /// The dependency view while plugins come and go: `report` used to
    /// take the registry's read lock twice, so a register or purge queued
    /// between the two deadlocked it and every service call after it.
    #[test]
    fn the_report_does_not_deadlock_against_registrations() {
        let owner = crate::lifecycle::owner_tag("module:svc-test-churn");
        register_for(owner, decl("svctest.churn", None), echo("c")).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let churn = crate::lifecycle::owner_tag("module:svc-test-churn2");
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    register_for(churn, decl("svctest.churn", None), echo("d")).unwrap();
                    purge_owner(Some(churn));
                }
            })
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(1500);
            let mut n = 0u64;
            while std::time::Instant::now() < until {
                let _ = report();
                n += 1;
            }
            let _ = done_tx.send(n);
        });
        let reports = done_rx.recv_timeout(std::time::Duration::from_secs(20));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let reports = reports.expect("report() deadlocked against register/purge");
        writer.join().unwrap();
        purge_owner(Some(owner));
        assert!(reports > 0);
    }
    // ── override points (PLAN-extension-api.md D2) ───────────────────

    fn point_decl(name: &str, on_error: OnError) -> PointDecl {
        PointDecl {
            name: name.into(),
            summary: format!("{name} for tests"),
            input: json!({"type": "object"}),
            output: json!({
                "type": "object",
                "required": ["via"],
                "properties": {"via": {"type": "array"}}
            }),
            on_error,
            deadline_ms: 5_000,
        }
    }

    fn core_default() -> Handler {
        handler(|_p, _cx| async { Ok(json!({"via": ["core"]})) })
    }

    /// A layer that asks `next` and appends its tag.
    fn wrapper(tag: &'static str) -> Handler {
        handler(move |params, cx| async move {
            let mut v = cx.next(params).await?;
            v["via"].as_array_mut().ok_or("no via")?.push(json!(tag));
            Ok(v)
        })
    }

    /// Overrides stack on the default, each `next` reaching the layer
    /// below; purging one puts the layer below it back in its place; the
    /// seam's fast-path flag follows.
    #[test]
    fn an_override_wraps_next_and_a_purge_restores_the_layer_below() {
        let rt = rt();
        declare_point(point_decl("pt.wrap", OnError::FallThrough), core_default()).unwrap();
        assert!(
            !overridden("pt.wrap"),
            "only the default: the seam's fast path"
        );
        let a = crate::lifecycle::owner_tag("module:pt-wrap-a");
        let b = crate::lifecycle::owner_tag("module:pt-wrap-b");
        register_for(a, decl("pt.wrap", None), wrapper("a")).unwrap();
        register_for(b, decl("pt.wrap", None), wrapper("b")).unwrap();
        assert!(overridden("pt.wrap"));
        let v = rt.block_on(ask("pt.wrap", json!({}), None)).unwrap();
        assert_eq!(v["via"], json!(["core", "a", "b"]));
        let row = points_report()
            .into_iter()
            .find(|r| r.name == "pt.wrap")
            .unwrap();
        assert_eq!(
            row.stack,
            vec![b.to_string(), a.to_string(), "core".to_string()]
        );
        assert!(overrides_listing().contains(&format!("pt.wrap ← {b}, over {a}, core")));
        purge_owner(Some(a));
        let v = rt.block_on(ask("pt.wrap", json!({}), None)).unwrap();
        assert_eq!(v["via"], json!(["core", "b"]), "b's next reaches core now");
        purge_owner(Some(b));
        let v = rt.block_on(ask("pt.wrap", json!({}), None)).unwrap();
        assert_eq!(v["via"], json!(["core"]));
        assert!(!overridden("pt.wrap"));
        let err = rt.block_on(ask("pt.nosuch", json!({}), None)).unwrap_err();
        assert!(err.contains("not a declared override point"), "{err}");
    }

    /// An override that errors or answers outside the point's schema is
    /// skipped when the point says `fall_through`; with `fail` the caller
    /// gets the error.
    #[test]
    fn a_failing_override_falls_through_or_fails_as_the_point_says() {
        let rt = rt();
        let x = crate::lifecycle::owner_tag("module:pt-fall-x");
        declare_point(point_decl("pt.fall", OnError::FallThrough), core_default()).unwrap();
        register_for(
            x,
            decl("pt.fall", None),
            handler(|_p, _cx| async { Err("broken".to_string()) }),
        )
        .unwrap();
        let v = rt.block_on(ask("pt.fall", json!({}), None)).unwrap();
        assert_eq!(v["via"], json!(["core"]), "the error fell through");
        register_for(
            x,
            decl("pt.fall", None),
            handler(|_p, _cx| async { Ok(json!({"nope": 1})) }),
        )
        .unwrap();
        let v = rt.block_on(ask("pt.fall", json!({}), None)).unwrap();
        assert_eq!(
            v["via"],
            json!(["core"]),
            "the off-schema answer fell through"
        );
        register_for(
            x,
            decl("pt.fall", None),
            handler(|_p, _cx| async { Ok(json!({"via": "not a list"})) }),
        )
        .unwrap();
        let v = rt.block_on(ask("pt.fall", json!({}), None)).unwrap();
        assert_eq!(
            v["via"],
            json!("not a list"),
            "only type/required/enum are held"
        );
        purge_owner(Some(x));

        declare_point(point_decl("pt.fail", OnError::Fail), core_default()).unwrap();
        register_for(
            x,
            decl("pt.fail", None),
            handler(|_p, _cx| async { Err("broken".to_string()) }),
        )
        .unwrap();
        let err = rt.block_on(ask("pt.fail", json!({}), None)).unwrap_err();
        assert!(err.contains("broken"), "{err}");
        purge_owner(Some(x));
    }

    /// A point's enum is held: an answer naming another value is refused.
    #[test]
    fn a_points_enum_is_held() {
        let schema = json!({
            "type": "object",
            "required": ["action"],
            "properties": {"action": {"enum": ["stop", "advance"]}}
        });
        assert_eq!(check_output(&schema, &json!({"action": "stop"})), Ok(()));
        assert!(check_output(&schema, &json!({"action": "explode"})).is_err());
        assert!(check_output(&schema, &json!({})).is_err());
        assert!(check_output(&schema, &json!("stop")).is_err());
    }

    /// A token is issued per call and dies with it: a made-up one and a
    /// kept one are refused, and the bottom layer has nothing below it.
    #[test]
    fn a_next_token_is_per_call_and_can_be_neither_forged_nor_kept() {
        let rt = rt();
        let err = rt
            .block_on(call_next("not-a-token", json!({})))
            .unwrap_err();
        assert!(err.contains("unknown or spent"), "{err}");

        declare_point(point_decl("pt.token", OnError::Fail), core_default()).unwrap();
        let kept: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let k = kept.clone();
        let t = crate::lifecycle::owner_tag("module:pt-token");
        register_for(
            t,
            decl("pt.token", None),
            handler(move |params, cx| {
                let k = k.clone();
                async move {
                    *k.lock().unwrap() = cx.next.clone();
                    cx.next(params).await
                }
            }),
        )
        .unwrap();
        rt.block_on(ask("pt.token", json!({}), None)).unwrap();
        let token = kept
            .lock()
            .unwrap()
            .clone()
            .expect("the call carried a token");
        let err = rt.block_on(call_next(&token, json!({}))).unwrap_err();
        assert!(
            err.contains("unknown or spent"),
            "a kept token must be dead: {err}"
        );
        purge_owner(Some(t));

        declare_point(
            point_decl("pt.bottom", OnError::Fail),
            handler(|params, cx| async move { cx.next(params).await }),
        )
        .unwrap();
        let err = rt.block_on(ask("pt.bottom", json!({}), None)).unwrap_err();
        assert!(err.contains("bottom layer"), "{err}");
    }

    /// A default declared after an override goes under it: load order
    /// never decides who answers.
    #[test]
    fn a_default_declared_late_still_sits_below_the_override() {
        let rt = rt();
        let e = crate::lifecycle::owner_tag("module:pt-early");
        register_for(e, decl("pt.late", None), wrapper("early")).unwrap();
        declare_point(point_decl("pt.late", OnError::Fail), core_default()).unwrap();
        let v = rt.block_on(ask("pt.late", json!({}), None)).unwrap();
        assert_eq!(v["via"], json!(["core", "early"]));
        purge_owner(Some(e));
    }
    /// A point a module declares once per instance (one per IRC network)
    /// has several defaults; none of them is an override, so the seams'
    /// fast path holds until a plugin that is not a declarer provides it.
    #[test]
    fn a_point_declared_by_several_instances_is_not_overridden_by_them() {
        let a = crate::lifecycle::owner_tag("module:pt-multi:a");
        let b = crate::lifecycle::owner_tag("module:pt-multi:b");
        for o in [a, b] {
            crate::lifecycle::with_thread_owner(o, || {
                declare_point(point_decl("pt.multi", OnError::FallThrough), core_default())
            })
            .unwrap();
        }
        assert!(!overridden("pt.multi"), "two declarers, no override");
        let x = crate::lifecycle::owner_tag("module:pt-multi-override");
        register_for(x, decl("pt.multi", None), wrapper("x")).unwrap();
        assert!(overridden("pt.multi"));
        purge_owner(Some(x));
        assert!(!overridden("pt.multi"));
        purge_owner(Some(a));
        purge_owner(Some(b));
        assert!(
            !is_point("pt.multi"),
            "the declarers took the point with them"
        );
    }
}
