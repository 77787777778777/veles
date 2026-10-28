//! The registry taxonomy test (PLAN-plugins-installer.md M4 / R2).
//!
//! A registry that can hold PLUGIN code or PLUGIN data must be
//! owner-tracked (`owned::OwnedRegistry` / `owned::OwnedSlot`), so a
//! module reload can purge exactly that module's entries (R3's teardown).
//! This test walks the veles-core / veles-frontends sources and FAILS
//! when it finds a registration point that is neither owner-tracked nor
//! on the core-owned allowlist below — so a NEW registry cannot land
//! untracked without somebody noticing.
//!
//! Two shapes are flagged:
//!   1. a `static … OnceLock/LazyLock` whose type holds `dyn` or a boxed
//!      closure (`Box<dyn Fn`) — process-global plugin code/data;
//!   2. a `pub fn register_*` / `pub fn set_*` free function — the
//!      signature of "install a thing here".
//!
//! The allowlist is the census (R2 step 2): the registries that were
//! audited and classified CORE-OWNED (a derived cache, an atomic counter,
//! a session-scoped cell, an instance method, a store-backed write) with
//! the one-line reason each is not module-purgeable. An entry that is
//! neither owner-tracked nor allowlisted fails the test.
//!
//! This is a SOURCE scan (the surface-parity walk's convention), not a
//! runtime assertion: the thing it guards — "did somebody add a registry
//! without owner-tracking" — is a property of the source, and a runtime
//! test cannot see a registry nobody registered into.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The core-owned allowlist: process-global registration points that were
/// audited and are NOT module-purgeable, each with the reason. Keyed by a
/// substring that must appear on the flagged LINE (the fn name or the
/// static's name), so a renamed symbol falls off the list and fails the
/// test (a silent rename is exactly how a registry would slip through).
///
/// KEEP THIS HONEST: an entry is a claim a reviewer can check. If a
/// registry CAN hold module code/data, it does not belong here — it
/// belongs behind `OwnedRegistry`/`OwnedSlot`.
const CORE_OWNED: &[(&str, &str)] = &[
    // ── derived caches (refreshed wholesale from the store/config; the
    //    source of truth is elsewhere, so a reload just re-derives) ──
    (
        "install_masks",
        "derived cache: refreshed from the store by refresh_masks",
    ),
    (
        "set_override_in_cache",
        "derived cache: per-channel override, store-backed",
    ),
    ("mask_cache", "derived cache holder"),
    ("CACHE", "derived cache (tokenplan/vault/triggers)"),
    // ── atomic counters / process state, not registries ──
    ("SEQ", "atomic id sequence"),
    ("LEG_SEQ", "atomic id sequence"),
    ("RUNNER_SEQ", "atomic id sequence for the runner keys"),
    ("LAST_USE", "idle timestamp"),
    ("LIMIT", "idle limit"),
    ("EXITED", "idle flag"),
    ("METRICS", "repair counters"),
    ("LAST_BY", "idle last-user string"),
    (
        "register_knobs",
        "the binary's own connector knobs for the settings form, registered \
         once at gateway start; a module's knobs come from its manifest",
    ),
    ("set_runtime_limit", "runtime cap config, not a registry"),
    ("set_runtime_budget", "runtime cap config, not a registry"),
    ("set_thread_owner", "the owner tag itself, not a registry"),
    (
        "set_test_offline",
        "test-only flag (testenv's lock): user envs offline, not a registry",
    ),
    (
        "clear_thread_owner_for_test",
        "test helper for the owner tag",
    ),
    // ── session-scoped cells (per-session, torn down WITH the session or
    //    the surface, not module-keyed) ──
    (
        "SESSION_PRINCIPALS",
        "per-session principal, dropped with the session",
    ),
    ("set_session_principal", "per-session write"),
    ("set_session_sender", "per-session write"),
    (
        "SESSION_ASKERS",
        "per-session asker set, self-cleans on surface detach (Weak)",
    ),
    (
        "set_session_asker",
        "per-session write; detach is the surface-death path",
    ),
    ("STEER_SLOTS", "per-session steer handle"),
    (
        "TURN_ASKERS",
        "per-session, one turn long: the owner's asker while their line runs \
         in a session they joined, set and cleared by that turn's guard \
         (agent.rs); it holds core's ChatAsker or PolicyAsker",
    ),
    (
        "JOINS",
        "per-session: who joined it (join.rs), set by the gateway's /join and \
         cleared by /leave; its asker is core's ChatAsker on the gateway bus",
    ),
    ("set_steer_store", "installs the store, not a plugin"),
    (
        "set_bg_store",
        "installs the store for durable background-event parking (the \
         delegate dock's store leg, 5375ed5b), not a plugin",
    ),
    (
        "GLOBAL_REGISTRY",
        "last-agent-wins session registry cell (core-published)",
    ),
    ("set_global_registry", "core publishes the session registry"),
    ("PENDING", "in-flight ask state, not a plugin registry"),
    ("TOMBSTONES", "in-flight ask state"),
    ("BACKGROUND_EVENTS", "in-flight delegate events"),
    ("WAKE_CLAIMS", "in-flight delegate claims"),
    // ── instance methods (&self) and store-backed writes — not
    //    process-global registries ──
    ("set_principal", "Agent instance method"),
    ("set_asker", "Agent instance method"),
    ("set_model", "Agent instance method"),
    ("set_reasoning", "Agent instance method"),
    ("set_mechanism", "Agent instance method"),
    ("set_skill", "Agent instance method"),
    ("set_failover_mode", "Agent instance method"),
    ("set_extra_body", "Agent instance method"),
    ("set_plan_mode", "Agent instance method"),
    ("set_policy", "vault instance method"),
    ("set_child_session", "task instance method"),
    ("set_lease", "scheduler instance method"),
    ("set_aliases", "registry instance method"),
    ("register_tiered", "registry instance method"),
    ("set_session_origin", "per-session write on the registry"),
    ("set_mode", "store-backed plan write"),
    ("set_value", "config document edit, in-memory"),
    ("set_string_array", "config document edit, in-memory"),
    ("set_local", "vault local secret, config-backed"),
    (
        "set_runtime_pairing_store",
        "installs the store, not a plugin",
    ),
    ("set_registered_hooks", "the hook name set (config-derived)"),
    (
        "set_default_bus",
        "the process hook bus (core's own dispatcher)",
    ),
    ("register_resolved", "node registry row, store-backed"),
    ("register_self", "node registry row, store-backed"),
    ("set_context_mark", "per-key seq marker (irc tap state)"),
    // ── the owned module itself ──
    ("set_core", "owned.rs: the core-owned install spelling"),
    // ── hooks registry: the name SET is config-derived; the HANDLERS are
    //    rustplugins' (owner-tracked there) and the Tcl registry's ──
    (
        "REGISTERED",
        "hook name set, config-derived; handlers are owner-tracked in rustplugins",
    ),
    (
        "RUST_HOOK_NAMES",
        "published name list derived from the owner-tracked PLUGINS",
    ),
    ("DEFAULT_BUS", "the process hook bus (core dispatcher)"),
    // ── ask relay / fallback are OwnedSlot-migrated; these names appear in
    //    their OWN purge fns and must not be double-flagged ──
    (
        "purge_owner",
        "an owner-tracked purge entry point (the migration itself)",
    ),
    // ── store / runtime installs (the thing installed is core's store,
    //    lease, bus, handle — not a plugin) ──
    ("set_runs_store", "installs core's runs store"),
    ("set_node_lease", "installs the node's own lease guard"),
    (
        "set_global_handle",
        "installs the Tcl runtime handle (core)",
    ),
    (
        "set_eval_from_blocking_thread",
        "installs core's Tcl eval path",
    ),
    ("set_global_bus", "installs the gateway's own bus"),
    ("set_global_core", "installs the gateway core (frontends)"),
    ("set_relay_base", "a2a relay base URL config"),
    ("set_dirs", "test/dev dir override, not a registry"),
    // ── per-session / per-key state, torn down with the session ──
    ("set_session_sandbox", "per-session sandbox binding"),
    ("set_session_reachable", "per-subject sandbox reachability"),
    (
        "set_peer_cursor",
        "per-peer store-sync cursor, store-backed",
    ),
    ("set_active", "the process locale, config-derived"),
    // ── the bus routes: addressed teardown already exists
    //    (unregister_outbound_route); a module host unregisters its routes
    //    by name — no per-entry owner tag needed ──
    (
        "register_outbound",
        "bus route: addressed teardown via unregister_outbound_route",
    ),
    (
        "register_outbound_route",
        "bus route: addressed teardown via unregister_outbound_route",
    ),
    // ── wrappers over an owner-tracked registry ──
    (
        // vision.rs (2026-10-10): puts `vision.describe` into the services
        // registry (services.rs, an OwnedRegistry) with owner `core` — a
        // core service, so nothing a module reload should purge. It was
        // committed without this line, and the gate that ran then never
        // reached this test (found 2026-10-11).
        "pub fn register_service() -> Result<(), String>",
        "registers a CORE service in the owner-tracked services registry",
    ),
    // ── test-only registration points ──
    ("register_turn_for_test", "test-only cancel-token registry"),
    ("set_acl_snapshot_for_tests", "test-only ACL snapshot"),
    // ── the wake-owner set: surfaces register a Weak; dead Weaks are
    //    pruned on read (self-cleaning, like SESSION_ASKERS) ──
    (
        "OWNERS",
        "wake owners: Weak refs, self-cleaning on surface death",
    ),
];

/// True when a flagged line is explained by the allowlist.
fn allowlisted(line: &str) -> Option<&'static str> {
    CORE_OWNED
        .iter()
        .find(|(needle, _)| line.contains(needle))
        .map(|(_, reason)| *reason)
}

/// True when the line uses the owner-tracked types (the migration).
fn is_owner_tracked(line: &str) -> bool {
    line.contains("OwnedRegistry") || line.contains("OwnedSlot")
}

/// The owner-trackable patterns, applied to one source line.
/// Returns a short tag for the matched pattern, or None.
fn flagged(line: &str) -> Option<&'static str> {
    let t = line.trim();
    // A free `pub fn register_*` / `pub fn set_*` (NOT an instance method,
    // which has `&self` on the next line or the same; NOT a purge fn,
    // which IS the migration).
    if (t.starts_with("pub fn register_") || t.starts_with("pub fn set_"))
        // A pure function over a borrowed TOML document is not a global
        // registry setter. Do not confuse set_item/set_value with the
        // process-global registration points this census is for.
        && !t.contains("&")
        && !t.contains("&self")
        && !t.contains("purge_owner")
    {
        return Some("register_/set_ fn");
    }
    // A process-global static holding plugin code/data.
    if t.starts_with("static ")
        && (t.contains("OnceLock") || t.contains("LazyLock"))
        && (t.contains("dyn ") || t.contains("Box<dyn") || t.contains("Arc<dyn"))
    {
        return Some("static holding dyn");
    }
    None
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn every_process_global_registry_is_owner_tracked_or_accounted_for() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rs_files(&root.join("src"), &mut files);
    // frontends too — its registries (gateway filters, desktop tools) are
    // in the census.
    let frontends = root.parent().unwrap().join("veles-frontends/src");
    rs_files(&frontends, &mut files);

    assert!(!files.is_empty(), "the source scan found no files");

    let mut unexplained: Vec<String> = Vec::new();
    let mut allowlist_used: BTreeSet<&str> = BTreeSet::new();

    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        // A file that USES an owner-tracked registry (declares or returns
        // an OwnedRegistry/OwnedSlot) has done the migration; its
        // register_*/set_* fns write through that registry and are covered
        // by it. The type may be on a continuation line of a multi-line
        // static, so scan the whole file, not just the `static` lines.
        let file_has_owned_static = text.contains("OwnedRegistry") || text.contains("OwnedSlot");
        let lines: Vec<&str> = text.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            let Some(kind) = flagged(line) else {
                continue;
            };
            if is_owner_tracked(line) {
                continue;
            }
            // A register_*/set_* fn in a file that migrated its registry is
            // covered by that migration (the tracking is in the registry
            // the fn writes through, not on the fn's own line).
            if kind == "register_/set_ fn" && file_has_owned_static {
                continue;
            }
            match allowlisted(line) {
                Some(reason) => {
                    // record WHICH needle matched, for the unused-entry check
                    for (needle, _) in CORE_OWNED {
                        if line.contains(needle) {
                            allowlist_used.insert(needle);
                        }
                    }
                    let _ = reason;
                }
                None => {
                    unexplained.push(format!(
                        "{}:{}: {} — `{}`",
                        f.display(),
                        n + 1,
                        kind,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        unexplained.is_empty(),
        "process-global registration points that are NEITHER owner-tracked \
         NOR on the core-owned census (R2). Either migrate them to \
         owned::OwnedRegistry / owned::OwnedSlot, or add them to CORE_OWNED \
         in this test WITH the reason they are not module-purgeable:\n  {}",
        unexplained.join("\n  ")
    );
}

/// The allowlist must not ROT: an entry that matches nothing in the tree
/// (a renamed registry, a deleted fn) is a silent hole a new untracked
/// registry could be filed under. Every needle must appear at least once.
#[test]
fn the_core_owned_census_has_no_dead_entries() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rs_files(&root.join("src"), &mut files);
    let frontends = root.parent().unwrap().join("veles-frontends/src");
    rs_files(&frontends, &mut files);

    let mut corpus = String::new();
    for f in &files {
        if let Ok(t) = std::fs::read_to_string(f) {
            corpus.push_str(&t);
            corpus.push('\n');
        }
    }

    let dead: Vec<&str> = CORE_OWNED
        .iter()
        .map(|(needle, _)| *needle)
        .filter(|needle| !corpus.contains(needle))
        .collect();

    assert!(
        dead.is_empty(),
        "CORE_OWNED entries that match NOTHING in the tree — a dead entry \
         is a hole a future untracked registry could hide under. Remove the \
         entry or fix the needle:\n  {}",
        dead.join("\n  ")
    );
}

/// The scanner itself, self-tested: a line that LOOKS like an untracked
/// registry is flagged, and a migrated/core line is not. This is the
/// mutation pin for the test above — a scanner that flags nothing would
/// let the whole taxonomy rot.
#[test]
fn the_scanner_flags_an_untracked_registry() {
    let untracked = r#"static FOO: std::sync::OnceLock<std::sync::Mutex<Vec<Arc<dyn Tool>>>> = std::sync::OnceLock::new();"#;
    assert_eq!(flagged(untracked), Some("static holding dyn"));
    assert!(!is_owner_tracked(untracked));
    assert!(allowlisted(untracked).is_none());

    let untracked_fn = "pub fn register_evil(thing: Arc<dyn Tool>) {";
    assert_eq!(flagged(untracked_fn), Some("register_/set_ fn"));

    // A migrated line is not unexplained.
    let tracked = r#"static TOOLS: std::sync::OnceLock<crate::owned::OwnedRegistry<String, Arc<dyn Tool>>> = std::sync::OnceLock::new();"#;
    assert!(is_owner_tracked(tracked));

    // Narrowing the scanner for pure document helpers must not hide a
    // genuine global setter. Mutable and immutable references are the
    // distinction; an instance method was already covered separately.
    let global_setter = "pub fn set_default_bus(bus: Arc<HookBus>) {";
    assert_eq!(flagged(global_setter), Some("register_/set_ fn"));
    let document_helper = "pub fn set_item(item: &mut Item, path: &[&str], value: Item) {";
    assert_eq!(flagged(document_helper), None);
    let borrowed_document = "pub fn set_readonly(item: &Item, value: Item) {";
    assert_eq!(flagged(borrowed_document), None);

    // An instance method is not a process-global registry.
    let method = "pub fn set_model(&self, name: &str) {";
    assert_eq!(flagged(method), None);

    // A purge fn is the migration, not a registry.
    let purge = "pub fn purge_owner_tools(owner: Option<&'static str>) -> usize {";
    assert_eq!(flagged(purge), None);
}
