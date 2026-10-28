//! Config replication (B2): every node owns an append-only log of its
//! config writes, offers travel peer-to-peer over the A2A link exactly
//! like memory offers, and every node MERGES the union of all logs.
//!
//! There is no master. The merge is deterministic — last write wins on
//! `(timestamp, node, seq)` — so two nodes that imported the same rows
//! converge on the same file without talking to each other, and a node
//! that was offline catches up by importing what it missed. The only
//! thing a "leader" (`[cluster] arbiter`, B3a) will ever arbitrate is
//! singleton leases, never config: any node appends, every node
//! converges.
//!
//! ## What replicates and what does not
//!
//! The scope taxonomy from PLAN-compute-pool.md, as code:
//!
//! - **Map sections** (`model`, `agent`, `acl`, …) replicate per key:
//!   `model.model` is one key, `model.api_mode` another. `acl` is one
//!   of them because its scopes are surfaces and channels, which mean
//!   the same thing on every node.
//! - **Element arrays** (`custom_providers`, `command_aliases`)
//!   replicate per element: `custom_providers.bailian` is one key, so
//!   two nodes can each add a provider and the merge keeps both.
//! - **Blob sections** (`fallback_providers`, `failover`)
//!   replicate wholesale — a fallback CHAIN is ordered and makes no
//!   sense element-merged, so the last writer's whole section wins.
//! - **Local sections** (`node`, `cluster`, `tui`, and any section not
//!   named) NEVER replicate. `[node]` describes the machine reading it;
//!   `[cluster]` names the peers this node dials (replicating it would
//!   loop a node's seeds onto its peer). `[platforms]` replicates since
//!   2026-09-28: the lease fence decides where an adapter runs.
//!
//! Node-specific *values* of shared keys are the fourth case, and they
//! are not replication at all: `[node.overrides]` shadows a replicated
//! key locally (see `NodeSection::overrides`) and never enters the
//! log — the shared file carries the shared truth, the overlay carries
//! "but on THIS node".
//!
//! ## The echo guard
//!
//! Applying a peer's offer writes merged values into config.toml. A
//! later scan must not log those as local writes — that would bounce
//! the same value back at the peer forever. The `config_scan` table is
//! the guard: it holds the replicated-key snapshot of the file as the
//! scanner last saw it. Scans diff file-against-snapshot (not
//! file-against-nothing), and apply updates the snapshot to what it
//! wrote, so imported values scan as "unchanged".
//!
//! ## Transport
//!
//! Same shape as memsync: a marker line, the payload base64'd with a
//! sha256 inside a fence, delivered as an A2A message the peer's
//! server answers WITHOUT a session or a turn, and stored in an inbox.
//! Offers are cursor diffs and stay small; no chunking. The trust gate
//! is the same list too — `[cluster] memsync_peers`, one list of
//! "peers allowed to deposit sync data in my inboxes".
//!
//! With `[cluster] configsync_auto` (the default) it all happens on its
//! own: the gateway scans its own file when it changes, pushes the new
//! rows to each peer from a per-peer cursor, and a receiver applies an
//! offer as it arrives — except the rows for [`HELD_SECTIONS`], which
//! wait in the inbox for the owner (`/configsync apply`, or `veles
//! configsync apply --inbox`). Without it, every step is the operator's.
//!
//! ## Conflicts
//!
//! Two nodes writing the SAME key within
//! [`CONFLICT_WINDOW_SECONDS`] with different values is flagged, not
//! silently resolved: the report names the key and both writes, and
//! the apply exits 2 while still writing the LWW winner (the state is
//! deterministic; the banner is a demand for a human decision). A
//! node editing only its own keys — the common case — never sees one.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Writes from different nodes closer together than this (seconds)
/// with different values are a CONFLICT, not a clean LWW win. Generous
/// on purpose: clocks on two machines that are not NTP-disciplined
/// drift by more than honest edit spacing.
pub const CONFLICT_WINDOW_SECONDS: i64 = 120;

// ─────────────────────────────────────────────────────────────────
// What replicates
// ─────────────────────────────────────────────────────────────────

/// How one top-level config section replicates (B2 scope taxonomy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionClass {
    /// Per key: every leaf under the section is an independent key.
    Map,
    /// Per element: `section.<name-field>` addresses one array element.
    ElementArray(&'static str),
    /// Wholesale: the whole section is one key, last writer wins.
    Blob,
    /// Never replicated — node-local by scope.
    Local,
}

/// The scope table. Sections not named here default to [`Local`] —
/// fail-closed: an unknown section the scanner has not been taught to
/// classify does not travel.
pub fn section_class(top: &str) -> SectionClass {
    match top {
        // Map sections: identity config, shared truth.
        "model" | "vision" | "imagegen" | "videogen" | "slopdrop" | "triggers" | "tokenplan"
        | "notify" | "web" | "asr" | "agent" | "mechanisms" | "memory" | "cache" | "tools"
        | "approvals" | "sandbox" | "checkpoints" | "plugins" | "skills" | "hooks" => {
            SectionClass::Map
        }
        // Element-keyed arrays.
        "custom_providers" => SectionClass::ElementArray("name"),
        "command_aliases" => SectionClass::ElementArray("name"),
        // Ordered chains — element-merging a fallback chain is
        // meaningless, so the section is one blob. The rules' TOML key
        // is `failover` (the struct field is `failover_rules`); matching
        // the field's name left the rules node-local by accident.
        "fallback_providers" | "failover" => SectionClass::Blob,
        // Aliases are names for routes: the same name must mean the same
        // route on every node, or `/defaults model fast` resolves
        // differently per node (2026-10-04 defaults review).
        "model_aliases" => SectionClass::Map,
        // Node-local, never replicated. `node` describes the machine
        // reading it; `cluster` names the peers THIS node dials (a
        // replicated cluster section would loop one node's seeds onto
        // its peer); `tui` is the local surface's own face.
        "node" | "cluster" | "tui" => SectionClass::Local,
        // `platforms` UNLOCKED 2026-09-28 (USER decision): it was held
        // local "until B3 placement" (B2 era) because the laptop's IRC
        // networks landing on the VPS would double a connection nothing
        // fenced. B3a's lease fence and B4's placement landed, so the
        // deferral's reason is gone — a replicated platform row arms on
        // whichever node the arbiter grants the lease, exactly one.
        // Which node actually SERVES an adapter is placement, answered
        // by the fence at boot, not by where the config lives.
        "platforms" => SectionClass::Map,
        // `acl` replicates per key. Its scopes are SURFACES — platform,
        // network, channel, and identities that carry their platform
        // (`irc:friend`) — and a surface means the same thing on every
        // node: "trusted in #veles on libera" applies wherever #veles is
        // served. Scoping permissions per NODE was tried in b8cc067 and
        // removed (USER 2026-09-26: "Scoping user permissions per node
        // is nonsense, should be scoped per surface/channel").
        "acl" => SectionClass::Map,
        _ => SectionClass::Local,
    }
}

/// True when `key` (dotted, as the scanner produces) is a replicated
/// key. Local sections and their whole subtrees answer false.
pub fn is_replicated_key(key: &str) -> bool {
    match split_key(key) {
        Ok(segs) => {
            segs.first()
                .is_some_and(|top| !matches!(section_class(top), SectionClass::Local))
                && !NODE_KEYS
                    .iter()
                    .any(|k| segs.iter().map(String::as_str).eq(k.split('.')))
        }
        // Not a key path at all: nothing this node could write — refuse.
        Err(_) => false,
    }
}

/// Keys in a shared section that describe the MACHINE, so they never
/// replicate: a path on this node's disk, or a service it runs itself.
/// Found by the first laptop↔VPS preview (2026-10-04): the VPS's
/// `media_public_dir` is `/opt/media-public`, the laptop's a path in its
/// home; the VPS searches brave and ddg with no searxng to wake, the
/// laptop runs its own. Replicated, either side's value is wrong on the
/// other. Where other people's web traffic leaves is a property of the
/// node too: the VPS sends it over Tor and the laptop refuses it.
pub const NODE_KEYS: &[&str] = &[
    "platforms.irc.media_public_dir",
    // Where this node's A2A server binds (2026-10-20, moved out of
    // `.env`'s A2A_HOST): the VPS 0.0.0.0, a laptop or phone its own
    // WireGuard address. Another node's value is an address this one
    // does not have.
    "platforms.a2a.extra.host",
    // Same shape for the desktop API: each node binds its own address
    // (WireGuard on a phone/laptop, 0.0.0.0 on the VPS behind a token).
    "platforms.desktop.extra.host",
    "web.backends",
    "web.searxng_url",
    "web.searxng_wake_cmd",
    "web.others",
    "web.others_proxy",
    "web.others_searxng_url",
    "web.others_via",
    // What fence THIS machine has and what it is made of: the backend
    // (native on a Windows node, podman on the hub), its image, kernel,
    // extra podman args and host mounts. Policy under `[sandbox]`
    // (memory, network, capabilities, zones) stays shared. Found
    // 2026-10-14: winbox's setup wrote `backend = "native"` and the
    // row reached the hub's and the laptop's inboxes as a security
    // change for the owner — approving it would have swapped their
    // podman fence for the native one.
    "sandbox.backend",
    "sandbox.image",
    "sandbox.kernel_path",
    "sandbox.extra_args",
    "sandbox.mounts",
];

// ─────────────────────────────────────────────────────────────────
// Key paths and values — toml_edit's grammar, never a hand-rolled one
// ─────────────────────────────────────────────────────────────────
//
// A replicated key is a dotted TOML key path and its value is TOML
// value text, both produced and read by toml_edit itself. The first
// version hand-rolled both (a quote-toggling key splitter, an inline
// table printer, newline-joined arrays re-parsed through a `v = …`
// wrapper), and each of those was a bug: `[[acl.entries]]` could never
// be applied, a quoted segment (`"irc:friend"`) was addressed as a key
// literally named `"irc:friend"`, and values doubled their spacing.

/// The segments of a dotted replicated key, by TOML's own key grammar
/// (`acl.extra_caps."irc:someone"` → `acl`, `extra_caps`, `irc:someone`).
pub fn split_key(key: &str) -> Result<Vec<String>> {
    Ok(toml_edit::Key::parse(key)
        .with_context(|| format!("`{key}` is not a TOML key path"))?
        .into_iter()
        .map(|k| k.get().to_string())
        .collect())
}

/// Segments back to a dotted key, each quoted exactly when TOML needs.
fn join_key(segs: &[&str]) -> String {
    segs.iter()
        .map(|s| toml_edit::Key::new(*s).display_repr().into_owned())
        .collect::<Vec<_>>()
        .join(".")
}

/// Strip everything about a value that is LAYOUT, not content: decor
/// (comments, spacing), inline formatting, and each scalar's written
/// representation (`'lit'` vs `"lit"`, `0x10` vs `16`). Two files that
/// say the same thing then scan to the same text — a comment edit is
/// not a config write, and a peer's value never differs from ours by
/// whitespace alone.
fn normalize(v: &mut toml_edit::Value) {
    use toml_edit::Value;
    match v {
        Value::String(f) => *v = Value::from(f.value().as_str()),
        Value::Integer(f) => *v = Value::from(*f.value()),
        Value::Float(f) => *v = Value::from(*f.value()),
        Value::Boolean(f) => *v = Value::from(*f.value()),
        Value::Datetime(f) => *v = Value::from(*f.value()),
        Value::Array(a) => {
            a.iter_mut().for_each(normalize);
            a.fmt();
        }
        Value::InlineTable(t) => {
            t.iter_mut().for_each(|(_, e)| normalize(e));
            t.fmt();
        }
    }
    v.decor_mut().clear();
}

/// An item as ONE canonical value text: a table becomes an inline
/// table, an array of tables an array of inline tables (toml_edit's own
/// `into_value`), normalized. `None` for an item that holds no value.
fn canonical(item: &toml_edit::Item) -> Option<String> {
    let mut v = item.clone().into_value().ok()?;
    normalize(&mut v);
    Some(v.to_string())
}

/// Parse a canonical value text back (toml_edit's `Value: FromStr`).
fn parse_value(text: &str) -> Result<toml_edit::Value> {
    text.parse::<toml_edit::Value>()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("value does not parse: {text}"))
}

// ─────────────────────────────────────────────────────────────────
// Scanner: config.toml text → replicated-key snapshot
// ─────────────────────────────────────────────────────────────────

/// Scan TOML text for replicated keys: `(key, canonical value text)`.
/// Local sections are skipped by classification; the secret guard
/// happens later, at offer build ([`build_offer`]) — a secret in the
/// log would still be a secret on disk.
pub fn scan_config_toml(raw: &str) -> Result<BTreeMap<String, String>> {
    let doc: toml_edit::DocumentMut = raw.parse().context("config.toml does not parse")?;
    let mut out = BTreeMap::new();
    for (k, item) in doc.iter() {
        match section_class(k) {
            SectionClass::Local => {}
            SectionClass::Blob => {
                if let Some(text) = canonical(item) {
                    out.insert(join_key(&[k]), text);
                }
            }
            SectionClass::ElementArray(name_field) => {
                scan_element_array(k, name_field, item, &mut out);
            }
            SectionClass::Map => walk_map_item(&mut vec![k], item, &mut out),
        }
    }
    Ok(out)
}

/// Walk a Map-section item. Tables recurse; everything else is a leaf —
/// a value (arrays and inline tables included) or an array of tables
/// (`[[acl.entries]]`, one ordered value at its path).
fn walk_map_item<'a>(
    path: &mut Vec<&'a str>,
    item: &'a toml_edit::Item,
    out: &mut BTreeMap<String, String>,
) {
    match item {
        toml_edit::Item::Table(t) => {
            for (k, sub) in t.iter() {
                path.push(k);
                walk_map_item(path, sub, out);
                path.pop();
            }
        }
        other => {
            if NODE_KEYS
                .iter()
                .any(|k| path.iter().copied().eq(k.split('.')))
            {
                return;
            }
            if let Some(text) = canonical(other) {
                out.insert(join_key(path), text);
            }
        }
    }
}

/// Scan an element-keyed array: each element becomes
/// `section.<name>`. An element WITHOUT the name field is not scanned:
/// it cannot be addressed on another node (an index is not an identity
/// once two nodes each add one).
fn scan_element_array(
    section: &str,
    name_field: &str,
    item: &toml_edit::Item,
    out: &mut BTreeMap<String, String>,
) {
    let elems: Vec<toml_edit::Item> = match item {
        toml_edit::Item::ArrayOfTables(a) => a
            .iter()
            .map(|t| toml_edit::Item::Table(t.clone()))
            .collect(),
        toml_edit::Item::Value(toml_edit::Value::Array(arr)) => arr
            .iter()
            .map(|v| toml_edit::Item::Value(v.clone()))
            .collect(),
        _ => Vec::new(),
    };
    for e in &elems {
        let name = e.get(name_field).and_then(|v| v.as_str());
        if let (Some(name), Some(text)) = (name, canonical(e)) {
            out.insert(join_key(&[section, name]), text);
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Log rows
// ─────────────────────────────────────────────────────────────────

/// One config write, as it lives in `config_log` and travels in an
/// offer. `value == ""` is a TOMBSTONE: the key was removed, and a
/// tombstone must outrank silence or deleted keys resurrect from an
/// old replica.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigLogRow {
    /// The node that made the write (its stable `node.toml` id).
    pub node: String,
    /// That node's own sequence number for the write — per-node
    /// monotonic. `(node, node_seq)` is the logical identity: an
    /// offer imported twice inserts once.
    pub node_seq: i64,
    /// RFC 3339 UTC, fixed-width (`to_rfc3339_opts(Nanos, true)`) so
    /// lexical and parsed order agree.
    pub at: String,
    /// Dotted key (`model.model`, `custom_providers.bailian`).
    pub key: String,
    /// TOML value text; "" = tombstone.
    pub value: String,
}

/// Parse an `at` stamp for comparison. Unparseable stamps sort as the
/// epoch — old, not winning.
fn at_ts(at: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::DateTime::from_timestamp(0, 0).unwrap())
}

/// Now, in the fixed-width stamp every writer uses.
pub fn now_stamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

// ─────────────────────────────────────────────────────────────────
// Scan-and-log: detect local writes, append them to the log
// ─────────────────────────────────────────────────────────────────

/// What a scan found and did.
#[derive(Debug, Default)]
pub struct ScanReport {
    /// Keys logged as new writes (added or changed).
    pub logged: Vec<String>,
    /// Keys logged as tombstones (present in the snapshot, gone from
    /// the file).
    pub tombstoned: Vec<String>,
}

impl ScanReport {
    pub fn summarize(&self) -> String {
        if self.logged.is_empty() && self.tombstoned.is_empty() {
            return "no config changes to log".to_string();
        }
        let mut parts = Vec::new();
        if !self.logged.is_empty() {
            parts.push(format!("logged {} write(s):", self.logged.len()));
            parts.extend(self.logged.iter().map(|k| format!("  {k}")));
        }
        if !self.tombstoned.is_empty() {
            parts.push(format!("logged {} tombstone(s):", self.tombstoned.len()));
            parts.extend(self.tombstoned.iter().map(|k| format!("  {k}")));
        }
        parts.join("\n")
    }
}

/// Diff the file against the `config_scan` snapshot and append what
/// changed to `config_log` as this node's writes. Every write path —
/// the operator's editor included — lands here, because detection is
/// on the FILE, not on a setter.
pub fn scan_and_log(store: &crate::store::Store, node: &str, raw: &str) -> Result<ScanReport> {
    scan_and_log_at(store, node, raw, &now_stamp())
}

/// [`scan_and_log`] with every row stamped `at` — the time the edit
/// was MADE, where that is known ([`scan_file`] passes the file's
/// mtime). The stamp is what LWW compares, so stamping an edit with the
/// moment it happened to be NOTICED would let a stale local file beat a
/// peer's newer write just by being scanned late.
pub fn scan_and_log_at(
    store: &crate::store::Store,
    node: &str,
    raw: &str,
    at: &str,
) -> Result<ScanReport> {
    let (rows, report) = scan_rows(store, node, raw, at)?;
    // The snapshot is what the scanner last saw — written AFTER the
    // rows exist so a crash between the two leaves a diff that
    // re-detects, not one that is silently lost.
    for r in &rows {
        store.config_log_append(r)?;
    }
    store.config_scan_replace(&scan_config_toml(raw)?)?;
    Ok(report)
}

/// The rows a scan of `raw` would log, and its report — read-only, so
/// a preview can weigh unlogged local edits the way an apply does.
fn scan_rows(
    store: &crate::store::Store,
    node: &str,
    raw: &str,
    at: &str,
) -> Result<(Vec<ConfigLogRow>, ScanReport)> {
    let current = scan_config_toml(raw)?;
    let prev: BTreeMap<String, String> = store.config_scan_get()?.into_iter().collect();
    let mut report = ScanReport::default();
    let mut node_seq = store.config_log_max_node_seq(node)?;
    let mut rows = Vec::new();
    for (k, v) in &current {
        match prev.get(k) {
            Some(p) if p == v => {}
            _ => {
                node_seq += 1;
                rows.push(ConfigLogRow {
                    node: node.to_string(),
                    node_seq,
                    at: at.to_string(),
                    key: k.clone(),
                    value: v.clone(),
                });
                report.logged.push(k.clone());
            }
        }
    }
    // A key the snapshot holds from before it stopped replicating
    // (NODE_KEYS) is not a deletion.
    for k in prev.keys().filter(|k| is_replicated_key(k)) {
        if !current.contains_key(k) {
            node_seq += 1;
            rows.push(ConfigLogRow {
                node: node.to_string(),
                node_seq,
                at: at.to_string(),
                key: k.clone(),
                value: String::new(),
            });
            report.tombstoned.push(k.clone());
        }
    }
    Ok((rows, report))
}

/// Read config.toml for the sync paths: its text and the stamp its
/// edits carry (the file's mtime, never later than now). `None` = the
/// file does not exist. Any OTHER read failure is an error — the old
/// `unwrap_or_default()` turned an unreadable file into an empty one.
fn read_config_file(path: &Path) -> Result<Option<(String, String)>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let now = chrono::Utc::now();
    let at = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(chrono::DateTime::<chrono::Utc>::from)
        .map(|t| t.min(now))
        .unwrap_or(now);
    Ok(Some((
        raw,
        at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    )))
}

/// Scan config.toml at `path` and log this node's changes — what
/// `veles configsync scan` runs, and the first step of every apply.
///
/// A MISSING file with a non-empty snapshot is refused: scanned as
/// empty, it logged a tombstone for every replicated key, and a push
/// then deleted all of them on every peer. (A missing file with no
/// snapshot is a node that never had a config — nothing to log.)
pub fn scan_file(store: &crate::store::Store, node: &str, path: &Path) -> Result<ScanReport> {
    match read_config_file(path)? {
        Some((raw, at)) => scan_and_log_at(store, node, &raw, &at),
        None if store.config_scan_get()?.is_empty() => Ok(ScanReport::default()),
        None => anyhow::bail!(
            "{} does not exist — refusing to log every replicated key as DELETED (a push \
             would delete them on every peer). Restore the file; to delete keys, remove \
             them from it and scan.",
            path.display()
        ),
    }
}

// ─────────────────────────────────────────────────────────────────
// Merge: the union of every node's rows → one deterministic state
// ─────────────────────────────────────────────────────────────────

/// One side of a conflict, for the banner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictingWrite {
    pub node: String,
    pub at: String,
    pub value: String,
}

/// The merged state of every log row the node has imported.
#[derive(Debug, Default)]
pub struct MergedState {
    /// Winners per key. Absent = deleted (or never set).
    pub values: BTreeMap<String, String>,
    /// Keys whose WINNING row is a tombstone — deleted on purpose by
    /// some node. The only keys an apply may remove from the file: a key
    /// absent from the log was never deleted by anyone (see
    /// [`effective_config_toml`]).
    pub deleted: std::collections::BTreeSet<String>,
    /// Keys two nodes wrote differently inside the window. The LWW
    /// winner is still written; the banner demands a human decision.
    pub conflicts: Vec<(String, ConflictingWrite, ConflictingWrite)>,
}

/// Fold all rows into one state. Pure: same rows in, same state out,
/// whatever order they arrived in. Last write wins on
/// `(at, node, node_seq)`; writes from different nodes within
/// [`CONFLICT_WINDOW_SECONDS`] with different values are flagged.
pub fn merge_rows(rows: &[ConfigLogRow]) -> MergedState {
    // Ascending winner order: later (at, node, seq) wins.
    let mut sorted: Vec<&ConfigLogRow> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        (at_ts(&a.at), &a.node, a.node_seq).cmp(&(at_ts(&b.at), &b.node, b.node_seq))
    });
    let mut state = MergedState::default();
    // Latest write per key.
    for r in &sorted {
        if r.value.is_empty() {
            state.values.remove(&r.key);
            state.deleted.insert(r.key.clone());
        } else {
            state.values.insert(r.key.clone(), r.value.clone());
            state.deleted.remove(&r.key);
        }
    }
    // Conflicts: for each key, the winner vs the strongest DIFFERENT
    // write from another node inside the window.
    let mut keys: Vec<&String> = rows.iter().map(|r| &r.key).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let mut writes: Vec<&ConfigLogRow> =
            sorted.iter().filter(|r| r.key == *key).copied().collect();
        if writes.len() < 2 {
            continue;
        }
        writes.sort_by(|a, b| {
            (at_ts(&b.at), &b.node, b.node_seq).cmp(&(at_ts(&a.at), &a.node, a.node_seq))
        });
        let winner = writes[0];
        let winner_ts = at_ts(&winner.at);
        for other in &writes[1..] {
            if other.node == winner.node {
                continue;
            }
            if other.value == winner.value {
                continue;
            }
            let dt = (winner_ts - at_ts(&other.at)).num_seconds().abs();
            if dt <= CONFLICT_WINDOW_SECONDS {
                state.conflicts.push((
                    key.clone(),
                    ConflictingWrite {
                        node: winner.node.clone(),
                        at: winner.at.clone(),
                        value: winner.value.clone(),
                    },
                    ConflictingWrite {
                        node: other.node.clone(),
                        at: other.at.clone(),
                        value: other.value.clone(),
                    },
                ));
                break;
            }
        }
    }
    state
}

// ─────────────────────────────────────────────────────────────────
// Apply: merged state → config.toml, grafted in place
// ─────────────────────────────────────────────────────────────────

/// A value as the item a file would hold it as when it is NEW here: an
/// inline table as a `[table]`, an array of inline tables as `[[…]]`
/// tables — the expanded layout the operator writes by hand.
fn new_item(v: toml_edit::Value) -> toml_edit::Item {
    match v {
        toml_edit::Value::InlineTable(it) => toml_edit::Item::Table(expand(it)),
        toml_edit::Value::Array(arr)
            if !arr.is_empty() && arr.iter().all(toml_edit::Value::is_inline_table) =>
        {
            let mut aot = toml_edit::ArrayOfTables::new();
            for e in arr {
                if let toml_edit::Value::InlineTable(it) = e {
                    aot.push(expand(it));
                }
            }
            toml_edit::Item::ArrayOfTables(aot)
        }
        mut other => {
            other.decor_mut().clear();
            toml_edit::Item::Value(other)
        }
    }
}

/// An inline table as a table (sub-tables too). A table holding only
/// sub-tables is implicit, so no bare `[custom_providers.models]`
/// header appears above `[custom_providers.models."glm-5.2"]`.
fn expand(it: toml_edit::InlineTable) -> toml_edit::Table {
    let mut t = toml_edit::Table::new();
    for (k, v) in it {
        t.insert(&k, new_item(v));
    }
    if !t.is_empty()
        && t.iter()
            .all(|(_, i)| i.is_table() || i.is_array_of_tables())
    {
        t.set_implicit(true);
    }
    t
}

/// Write `incoming` into `slot`, changing only what differs and keeping
/// the layout that is already there.
///
/// A value replacing a value keeps its decor (the comment after it); a
/// table is grafted key by key, an array of tables element by element,
/// so every comment, header and position on what did NOT change stays
/// put. (The comment block ABOVE a key lives in the key's own decor,
/// which is why a changed key is never removed and re-inserted — the
/// first version did exactly that and deleted the operator's notes.)
/// An item whose content already equals `incoming` is not touched.
fn graft(slot: &mut toml_edit::Item, incoming: toml_edit::Value) {
    let mut want = incoming.clone();
    normalize(&mut want);
    if canonical(slot).as_deref() == Some(want.to_string().as_str()) {
        return;
    }
    match slot {
        toml_edit::Item::Value(old) => {
            let decor = old.decor().clone();
            *old = incoming;
            *old.decor_mut() = decor;
        }
        toml_edit::Item::Table(t) if incoming.is_inline_table() => {
            if let toml_edit::Value::InlineTable(it) = incoming {
                graft_table(t, it);
            }
        }
        toml_edit::Item::ArrayOfTables(aot)
            if matches!(&incoming, toml_edit::Value::Array(a)
                if a.iter().all(toml_edit::Value::is_inline_table)) =>
        {
            if let toml_edit::Value::Array(arr) = incoming {
                graft_array_of_tables(aot, arr);
            }
        }
        _ => *slot = new_item(incoming),
    }
}

/// Graft an inline table's content onto a table: keys it lacks go, keys
/// it has are grafted (unchanged ones untouched), new keys are added.
fn graft_table(t: &mut toml_edit::Table, incoming: toml_edit::InlineTable) {
    let keep: std::collections::HashSet<String> =
        incoming.iter().map(|(k, _)| k.to_string()).collect();
    let stale: Vec<String> = t
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| !keep.contains(k))
        .collect();
    for k in stale {
        t.remove(&k);
    }
    for (k, v) in incoming {
        match t.get_mut(&k) {
            Some(slot) => graft(slot, v),
            None => {
                t.insert(&k, new_item(v));
            }
        }
    }
}

/// Graft element by element: the array's order is its meaning
/// (`fallback_providers` is a chain), so element i stays element i.
fn graft_array_of_tables(aot: &mut toml_edit::ArrayOfTables, incoming: toml_edit::Array) {
    let elems: Vec<toml_edit::InlineTable> = incoming
        .into_iter()
        .filter_map(|v| match v {
            toml_edit::Value::InlineTable(it) => Some(it),
            _ => None,
        })
        .collect();
    while aot.len() > elems.len() {
        aot.remove(aot.len() - 1);
    }
    for (i, it) in elems.into_iter().enumerate() {
        match aot.get_mut(i) {
            Some(t) => graft_table(t, it),
            None => aot.push(expand(it)),
        }
    }
}

/// The table at a path, created (implicit) when missing. An inline
/// table on the path is expanded in place; a scalar where a table is
/// needed gives way — the merged state is the truth, the file follows.
fn table_at_mut<'a>(
    root: &'a mut toml_edit::Table,
    segs: &[String],
) -> Option<&'a mut toml_edit::Table> {
    let mut cur = root;
    for seg in segs {
        let slot = cur.entry(seg).or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            toml_edit::Item::Table(t)
        });
        if let toml_edit::Item::Value(v) = slot {
            let expanded = match v.clone() {
                toml_edit::Value::InlineTable(it) => expand(it),
                _ => toml_edit::Table::new(),
            };
            *slot = toml_edit::Item::Table(expanded);
        }
        cur = slot.as_table_mut()?;
    }
    Some(cur)
}

/// The table at a path, if every segment exists and is a table.
fn table_at_mut_opt<'a>(
    root: &'a mut toml_edit::Table,
    segs: &[String],
) -> Option<&'a mut toml_edit::Table> {
    let mut cur = root;
    for seg in segs {
        cur = cur.get_mut(seg)?.as_table_mut()?;
    }
    Some(cur)
}

/// Remove a key (or element) and prune the empty tables above it.
fn remove_path(doc: &mut toml_edit::DocumentMut, key: &str) -> Result<()> {
    let segs = split_key(key)?;
    let Some(top) = segs.first() else {
        return Ok(());
    };
    let root = doc.as_table_mut();
    match section_class(top) {
        SectionClass::ElementArray(name_field) if segs.len() == 2 => {
            remove_array_element(root, top, name_field, &segs[1]);
        }
        _ => {
            let (parents, leaf) = segs.split_at(segs.len() - 1);
            if let Some(t) = table_at_mut_opt(root, parents) {
                t.remove(&leaf[0]);
            }
            prune_empty(doc, &segs);
        }
    }
    Ok(())
}

/// Is `v` an inline-table element whose name field is `name`?
fn inline_element_named(v: &toml_edit::Value, name_field: &str, name: &str) -> bool {
    v.as_inline_table()
        .and_then(|t| t.get(name_field))
        .and_then(|n| n.as_str())
        == Some(name)
}

fn remove_array_element(root: &mut toml_edit::Table, section: &str, name_field: &str, name: &str) {
    match root.get_mut(section) {
        Some(toml_edit::Item::ArrayOfTables(a)) => {
            let hit = a
                .iter()
                .position(|t| t.get(name_field).and_then(|n| n.as_str()) == Some(name));
            if let Some(i) = hit {
                a.remove(i);
            }
        }
        Some(toml_edit::Item::Value(toml_edit::Value::Array(arr))) => {
            let hit = arr
                .iter()
                .position(|v| inline_element_named(v, name_field, name));
            if let Some(i) = hit {
                arr.remove(i);
            }
        }
        // Absent IS the removed state: the tombstone is a no-op.
        _ => {}
    }
}

/// Drop tables that became empty, bottom-up.
fn prune_empty(doc: &mut toml_edit::DocumentMut, segs: &[String]) {
    for depth in (1..segs.len()).rev() {
        let (parents, leaf) = segs.split_at(depth);
        if let Some(t) = table_at_mut_opt(doc.as_table_mut(), parents) {
            let name = &leaf[0];
            if matches!(t.get(name), Some(toml_edit::Item::Table(sub)) if sub.is_empty()) {
                t.remove(name);
            }
        }
    }
}

/// Set one merged key into the document. Element keys address an
/// array element by name; every other key (a map leaf, or a blob's
/// whole section) is grafted at its path.
fn set_merged_key(doc: &mut toml_edit::DocumentMut, key: &str, value: &str) -> Result<()> {
    let segs = split_key(key)?;
    let incoming =
        parse_value(value).with_context(|| format!("value for `{key}` does not parse"))?;
    let Some(top) = segs.first() else {
        return Ok(());
    };
    let root = doc.as_table_mut();
    match section_class(top) {
        SectionClass::ElementArray(name_field) if segs.len() == 2 => {
            set_array_element(root, top, name_field, &segs[1], incoming)
        }
        _ => {
            let (parents, leaf) = segs.split_at(segs.len() - 1);
            let t = table_at_mut(root, parents)
                .with_context(|| format!("`{key}`: the path is blocked"))?;
            match t.get_mut(&leaf[0]) {
                Some(slot) => graft(slot, incoming),
                None => {
                    t.insert(&leaf[0], new_item(incoming));
                }
            }
            Ok(())
        }
    }
}

/// Set one element of an element array, found by name and grafted IN
/// PLACE — the array's order is the operator's, and resolution is
/// first-match in it. A new element is appended.
fn set_array_element(
    root: &mut toml_edit::Table,
    section: &str,
    name_field: &str,
    name: &str,
    incoming: toml_edit::Value,
) -> Result<()> {
    let toml_edit::Value::InlineTable(it) = incoming else {
        anyhow::bail!("element `{section}.{name}` is not a table");
    };
    match root.get_mut(section) {
        Some(toml_edit::Item::ArrayOfTables(a)) => {
            let hit = a
                .iter()
                .position(|t| t.get(name_field).and_then(|n| n.as_str()) == Some(name));
            match hit.and_then(|i| a.get_mut(i)) {
                Some(t) => graft_table(t, it),
                None => a.push(expand(it)),
            }
        }
        Some(toml_edit::Item::Value(toml_edit::Value::Array(arr))) => {
            let hit = arr
                .iter()
                .position(|v| inline_element_named(v, name_field, name));
            match hit.and_then(|i| arr.get_mut(i)) {
                Some(slot) => {
                    let decor = slot.decor().clone();
                    *slot = toml_edit::Value::InlineTable(it);
                    *slot.decor_mut() = decor;
                }
                None => arr.push(toml_edit::Value::InlineTable(it)),
            }
        }
        _ => {
            let mut a = toml_edit::ArrayOfTables::new();
            a.push(expand(it));
            root.insert(section, toml_edit::Item::ArrayOfTables(a));
        }
    }
    Ok(())
}

/// Compute the file text for a merged state over the current file.
/// Removals first (tombstones + keys the merge deleted), then sets —
/// a key removed and re-set in the same state must end up set.
///
/// Only what DIFFERS is touched. A key whose merged value already is the
/// file's value is left alone — re-setting every key re-ordered element
/// arrays (remove + push: `custom_providers` came back sorted by name,
/// and resolution is first-match in that order), flattened sub-tables
/// and moved blob sections to the end, on an apply that changed nothing.
/// And only a key whose winning row is a TOMBSTONE is removed: a key
/// absent from the log was never deleted by anyone — it is a local
/// value nobody logged, and removing it was silent data loss.
pub fn effective_config_toml(raw: &str, merged: &MergedState) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = raw.parse().context("config.toml does not parse")?;
    let file_scan = scan_config_toml(raw)?;
    for key in file_scan.keys() {
        if merged.deleted.contains(key) {
            remove_path(&mut doc, key)?;
        }
    }
    for (key, value) in &merged.values {
        // Rows logged before a key stopped replicating stay in the log;
        // they do not write the file.
        if !is_replicated_key(key) {
            continue;
        }
        if file_scan.get(key) != Some(value) {
            set_merged_key(&mut doc, key, value)?;
        }
    }
    Ok(doc.to_string())
}

// ─────────────────────────────────────────────────────────────────
// Offers
// ─────────────────────────────────────────────────────────────────

/// A config sync offer: this node's log rows past a cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSyncOffer {
    pub from_node: String,
    #[serde(default)]
    pub from_name: String,
    /// The local-rowid cursor the offer starts AFTER. Display +
    /// operator bookkeeping; import is idempotent regardless.
    pub since_seq: i64,
    pub rows: Vec<ConfigLogRow>,
}

/// What building an offer withheld (secret-shaped values) and what it
/// carries, in one displayable line.
pub fn summarize_offer(offer: &ConfigSyncOffer) -> String {
    let keys: std::collections::BTreeSet<&str> =
        offer.rows.iter().map(|r| r.key.as_str()).collect();
    format!(
        "{} row(s) across {} key(s), since seq {}",
        offer.rows.len(),
        keys.len(),
        offer.since_seq
    )
}

/// Build an offer of THIS node's rows past `since_seq` (local rowid).
/// Rows whose value is credential-shaped are withheld and named —
/// defense in depth on top of the section allowlist, because an
/// operator can put a key under `[web]` and nobody should have to
/// remember that.
pub fn build_offer(
    store: &crate::store::Store,
    from_node: &str,
    from_name: &str,
    since_seq: i64,
) -> Result<(ConfigSyncOffer, Vec<String>)> {
    let rows = store.config_log_rows_for(from_node, since_seq)?;
    let mut withheld = Vec::new();
    let mut kept = Vec::new();
    for r in rows {
        if !is_replicated_key(&r.key) {
            continue;
        }
        if !r.value.is_empty() && crate::a2a::secret_shaped(&r.value) {
            withheld.push(r.key.clone());
            continue;
        }
        kept.push(r);
    }
    let offer = ConfigSyncOffer {
        from_node: from_node.to_string(),
        from_name: from_name.to_string(),
        since_seq,
        rows: kept,
    };
    Ok((offer, withheld))
}

// ─────────────────────────────────────────────────────────────────
// Apply an offer
// ─────────────────────────────────────────────────────────────────

/// What applying an offer did.
#[derive(Debug, Default)]
pub struct ApplyReport {
    /// Local edits the apply logged FIRST (edits made since the last
    /// scan), so the merge weighs them instead of overwriting them.
    pub local_logged: Vec<String>,
    /// Rows newly imported (the rest were already present — import is
    /// idempotent on `(node, node_seq)`).
    pub imported: usize,
    /// Rows already in the log.
    pub skipped: usize,
    /// Rows for keys THIS node classifies as local (a newer schema or a
    /// misconfigured peer) — kept out of the file.
    pub local_keys: usize,
    /// Rows naming a node other than the offer's sender. An offer only
    /// ever carries its sender's own rows, so these are refused: a peer
    /// must not write history in another node's name (or this one's).
    pub forged: usize,
    /// Merged keys whose file content changed.
    pub changed: Vec<String>,
    pub conflicts: Vec<(String, ConflictingWrite, ConflictingWrite)>,
}

impl ApplyReport {
    pub fn summarize(&self) -> String {
        let mut out = Vec::new();
        if !self.local_logged.is_empty() {
            out.push(format!(
                "logged {} local edit(s) first (made since the last scan):",
                self.local_logged.len()
            ));
            out.extend(self.local_logged.iter().map(|k| format!("  {k}")));
        }
        out.push(format!(
            "imported {} row(s) ({} already present)",
            self.imported, self.skipped
        ));
        if self.local_keys > 0 {
            out.push(format!(
                "kept out {} row(s) for keys that are node-local here",
                self.local_keys
            ));
        }
        if self.forged > 0 {
            out.push(format!(
                "REFUSED {} row(s) naming a node other than the sender",
                self.forged
            ));
        }
        if self.changed.is_empty() {
            out.push("file already at the merged state".to_string());
        } else {
            out.push(format!(
                "{} key(s) changed in config.toml:",
                self.changed.len()
            ));
            out.extend(self.changed.iter().map(|k| format!("  {k}")));
        }
        if !self.conflicts.is_empty() {
            out.push(format!("{} CONFLICT(S):", self.conflicts.len()));
            for (k, w, l) in &self.conflicts {
                out.push(format!(
                    "  {k}: {a} ({an}) vs {b} ({bn}) — winner written, decide by hand",
                    a = truncate(&w.value),
                    an = w.node,
                    b = truncate(&l.value),
                    bn = l.node,
                ));
            }
        }
        out.join("\n")
    }

    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// The first 40 CHARACTERS: a byte cut panics inside a multi-byte
/// character, and config values here are written in 47 locales.
fn truncate(s: &str) -> String {
    const N: usize = 40;
    match s.char_indices().nth(N) {
        None => s.to_string(),
        Some((cut, _)) => format!("{}…", &s[..cut]),
    }
}

/// The config file path offers apply to.
pub fn config_path() -> std::path::PathBuf {
    crate::config::config_dir().join("config.toml")
}

/// Log this node's own unscanned edits, import an offer's rows, merge
/// EVERYTHING (local history included), write the merged state into
/// config.toml, and update the scan snapshot so the imported values do
/// not bounce back as local writes on the next scan (the echo guard).
///
/// The local scan comes FIRST. An edit made since the last scan was in
/// no log row, so it was not in the merge, and the old apply removed
/// every file key the merge lacked and then overwrote the snapshot —
/// the edit was gone and nothing could ever notice. Scanned first (at
/// the file's mtime), it is weighed like any other write.
///
/// Nothing is imported or written unless the merged file still LOADS
/// as a config (the guard `configedit::edit` has). The write keeps the
/// file's mode — config.toml holds provider keys, and tmp+rename made a
/// fresh 0644 inode of a 0600 file — and so does its backup.
/// Exit-worthy conflicts are in the report, not an error.
pub fn apply_offer(
    store: &crate::store::Store,
    local_node: &str,
    offer: &ConfigSyncOffer,
) -> Result<ApplyReport> {
    apply_or_preview(store, local_node, offer, true)
}

/// What [`import_recovered`] put in the log.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveredImport {
    /// Rows new to this log.
    pub imported: usize,
    /// Rows it already held (import is idempotent on `(node, node_seq)`).
    pub present: usize,
    /// Rows for keys this node keeps local — never imported.
    pub local: usize,
    /// Of the imported, the rows THIS node wrote before its home was lost.
    pub mine: usize,
    /// How many nodes' rows the import carried.
    pub nodes: usize,
}

/// A node coming back from a lost home (`veles recover`,
/// PLAN-recover.md R4) takes the WHOLE log its peer holds into its own:
/// the peer's rows, every other node's, and its own history — the one
/// case where a row in this node's name arrives from elsewhere. Two
/// reasons it cannot wait for the pushes:
///
/// - each peer pushes only its OWN rows, past a cursor that says this
///   node had them all, so nothing it held before the loss comes back;
/// - this node's own `node_seq` restarts at 1 in an empty log, and every
///   peer already holds `(this node, 1..n)`: its next writes would be
///   dropped everywhere as duplicates. Its history, imported first, is
///   what makes the next scan number from n + 1.
///
/// So this runs BEFORE any scan (an apply scans first), and the caller
/// then merges and writes the file with an [`apply_offer`] of nothing.
/// Keys this node classifies as local are left out, as an apply does.
pub fn import_recovered(
    store: &crate::store::Store,
    local_node: &str,
    rows: &[ConfigLogRow],
) -> Result<RecoveredImport> {
    let mut report = RecoveredImport::default();
    let mut nodes = std::collections::BTreeSet::new();
    for r in rows {
        if r.node.trim().is_empty() || r.node_seq <= 0 {
            continue;
        }
        if !is_replicated_key(&r.key) {
            report.local += 1;
            continue;
        }
        nodes.insert(r.node.clone());
        if store.config_log_append(r)? {
            report.imported += 1;
            if r.node == local_node {
                report.mine += 1;
            }
        } else {
            report.present += 1;
        }
    }
    report.nodes = nodes.len();
    Ok(report)
}

/// What [`apply_offer`] would do, doing none of it: no row imported,
/// no local edit logged, the file and the scan snapshot untouched.
/// `veles configsync apply --dry-run`.
pub fn preview_offer(
    store: &crate::store::Store,
    local_node: &str,
    offer: &ConfigSyncOffer,
) -> Result<ApplyReport> {
    apply_or_preview(store, local_node, offer, false)
}

fn apply_or_preview(
    store: &crate::store::Store,
    local_node: &str,
    offer: &ConfigSyncOffer,
    write: bool,
) -> Result<ApplyReport> {
    let path = config_path();
    let mut report = ApplyReport::default();
    let mut unlogged = Vec::new();
    if write {
        let local = scan_file(store, local_node, &path)?;
        report.local_logged = local.logged.into_iter().chain(local.tombstoned).collect();
    } else if let Some((raw, at)) = read_config_file(&path)? {
        let (rows, local) = scan_rows(store, local_node, &raw, &at)?;
        report.local_logged = local.logged.into_iter().chain(local.tombstoned).collect();
        unlogged = rows;
    }

    let mut all = store.config_log_rows()?;
    all.extend(unlogged);
    let mut incoming = Vec::new();
    for r in &offer.rows {
        if r.node != offer.from_node {
            report.forged += 1;
            continue;
        }
        if !is_replicated_key(&r.key) {
            // A peer logged a key we classify as local: newer schema
            // or a misconfigured peer. Keep it OUT — the file we
            // write is read by THIS node's classification.
            report.local_keys += 1;
            continue;
        }
        incoming.push(r);
    }
    // Merge in memory first: rows are only imported once the result is
    // known to load. A row already present is the same row (import is
    // idempotent on (node, node_seq)), so a duplicate changes nothing.
    all.extend(incoming.iter().map(|r| (*r).clone()));
    let merged = merge_rows(&all);
    report.conflicts = merged.conflicts.clone();

    let raw = read_config_file(&path)?
        .map(|(raw, _)| raw)
        .unwrap_or_default();
    let text = effective_config_toml(&raw, &merged)?;
    if text != raw {
        if let Err(e) = toml::from_str::<crate::config::VelesConfig>(&text) {
            anyhow::bail!(
                "applying this offer would make config.toml unreadable ({e}) — nothing \
                 imported, nothing written"
            );
        }
    }
    for r in incoming {
        let new = if write {
            store.config_log_append(r)?
        } else {
            !store
                .config_log_rows()?
                .iter()
                .any(|x| x.node == r.node && x.node_seq == r.node_seq)
        };
        match new {
            true => report.imported += 1,
            false => report.skipped += 1,
        }
    }
    if text != raw {
        if write {
            write_config_keeping_mode(&path, &raw, &text)?;
        }
        let after = scan_config_toml(&text)?;
        let before = scan_config_toml(&raw)?;
        for (k, v) in &after {
            if before.get(k) != Some(v) {
                report.changed.push(k.clone());
            }
        }
        for k in before.keys() {
            if !after.contains_key(k) {
                report.changed.push(k.clone());
            }
        }
    }
    // Echo guard: the snapshot now reflects what the file says for
    // replicated keys, whoever wrote it.
    if write {
        store.config_scan_replace(&scan_config_toml(&text)?)?;
    }
    Ok(report)
}

/// Replace config.toml with `text`, atomically, keeping its mode: a
/// backup of `raw` beside it (same mode) and a tmp file renamed over
/// it. tmp+rename makes a NEW inode, born at the umask — 0644 for a
/// file that holds provider keys — so the source's mode is mirrored
/// onto both; a config written from nothing is made owner-only.
fn write_config_keeping_mode(path: &Path, raw: &str, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existed = path.exists();
    if !raw.is_empty() {
        let bak = path.with_extension("toml.bak-configsync");
        std::fs::write(&bak, raw).with_context(|| format!("backup {}", bak.display()))?;
        crate::configedit::mirror_mode(path, &bak);
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    if existed {
        crate::configedit::mirror_mode(path, &tmp);
    } else {
        crate::configedit::restrict_to_owner(&tmp);
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────
// Transport (the memsync shape: marker, fence, inbox, apply-by-hand)
// ─────────────────────────────────────────────────────────────────

pub const CONFIG_MARKER: &str = "[veles config sync offer]";
const FENCE_B64: &str = "```veles-configsync-b64";

/// Does this text carry a config sync offer?
pub fn is_config_offer(text: &str) -> bool {
    text.trim_start().starts_with(CONFIG_MARKER) && text.contains(FENCE_B64)
}

/// Encode an offer as an A2A message: marker line, human summary, the
/// payload base64'd with its sha256 inside a fence.
pub fn encode_message(offer: &ConfigSyncOffer) -> Result<String> {
    use base64::Engine as _;
    let json = serde_json::to_string(offer)?;
    let sha = crate::memory::MemoryStore::content_sha(&json);
    let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
    Ok(format!(
        "{CONFIG_MARKER} {}\n\
         Stored in the receiver's configsync inbox; applied only by `veles configsync apply --inbox`.\n\
         {FENCE_B64}\nsha256:{sha}\n{b64}\n```",
        summarize_offer(offer)
    ))
}

/// The offer inside a message (or file): the base64 fence, verified
/// against its checksum, else the whole text as JSON (a file written
/// by `veles configsync offer`).
pub fn decode_message(raw: &str) -> Result<ConfigSyncOffer> {
    use base64::Engine as _;
    if let Some(start) = raw.find(FENCE_B64) {
        let body = &raw[start + FENCE_B64.len()..];
        let body = &body[..body.find("```").unwrap_or(body.len())];
        let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
        let sha = lines
            .next()
            .and_then(|l| l.strip_prefix("sha256:"))
            .context("config offer: missing sha256 line")?
            .to_string();
        let b64: String = lines.collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .context("config offer: payload is not base64")?;
        let json = String::from_utf8(bytes).context("config offer: payload is not UTF-8")?;
        if crate::memory::MemoryStore::content_sha(&json) != sha {
            anyhow::bail!("config offer: checksum mismatch — the payload changed in transit");
        }
        return serde_json::from_str(&json).context("config offer: payload is not an offer");
    }
    let v: serde_json::Value =
        serde_json::from_str(raw.trim()).context("config offer: not JSON")?;
    let v = v.get("offer").cloned().unwrap_or(v);
    serde_json::from_value(v).context("config offer: JSON is not an offer")
}

/// Where received offers wait: `<veles_home>/configsync/inbox/`.
pub fn inbox_dir() -> std::path::PathBuf {
    crate::config::veles_home().join("configsync").join("inbox")
}

/// Cap on pending inbox entries, same reasoning as memsync's.
const INBOX_MAX: usize = 200;

/// One stored offer, with who delivered it and when.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEntry {
    pub received_at: String,
    /// The A2A identity that delivered it (authenticated by the server).
    pub peer: String,
    pub task_id: String,
    pub offer: ConfigSyncOffer,
}

/// Store an offer in `dir` — arrival-ordered names, atomic write,
/// refused past the cap.
pub fn inbox_store(
    dir: &Path,
    peer: &str,
    task_id: &str,
    offer: ConfigSyncOffer,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    if inbox_pending(dir)?.len() >= INBOX_MAX {
        anyhow::bail!("configsync inbox full — apply with `veles configsync apply --inbox` first");
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let safe: String = task_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(40)
        .collect();
    let path = dir.join(format!("{nanos:020}-{safe}.json"));
    let entry = InboxEntry {
        received_at: chrono::Utc::now().to_rfc3339(),
        peer: peer.to_string(),
        task_id: task_id.to_string(),
        offer,
    };
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&entry)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
    Ok(path)
}

/// Pending offers in `dir`, oldest first. A missing inbox is empty.
pub fn inbox_pending(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<_> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .collect();
    out.sort();
    Ok(out)
}

/// Read an inbox entry (or any offer file) back.
pub fn inbox_read(path: &Path) -> Result<InboxEntry> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if let Ok(e) = serde_json::from_str::<InboxEntry>(&raw) {
        return Ok(e);
    }
    Ok(InboxEntry {
        received_at: String::new(),
        peer: String::new(),
        task_id: String::new(),
        offer: decode_message(&raw)?,
    })
}

/// Push this node's config rows past `since_seq` to a peer's inbox
/// over the A2A link. Returns what the peer said; a refusal is an
/// error. Re-pushing is safe (import is idempotent).
pub async fn push_to_peer(
    store: &crate::store::Store,
    from_node: &str,
    from_name: &str,
    peer_url: &str,
    bearer: Option<&str>,
    since_seq: i64,
) -> Result<String> {
    let (offer, withheld) = build_offer(store, from_node, from_name, since_seq)?;
    let mut out = Vec::new();
    for k in &withheld {
        out.push(format!(
            "withheld {k}: it contains a credential-shaped string, and secrets stay on their node"
        ));
    }
    if offer.rows.is_empty() {
        out.push(format!("nothing to push since seq {since_seq}"));
        return Ok(out.join("\n"));
    }
    let reply = send_offer(peer_url, bearer, &offer).await?;
    out.push(format!("{}: {}", summarize_offer(&offer), reply));
    Ok(out.join("\n"))
}

/// Deliver one offer to a peer; its reply (stored, applied, held) on
/// success, an error when it refused or could not be reached.
pub async fn send_offer(
    peer_url: &str,
    bearer: Option<&str>,
    offer: &ConfigSyncOffer,
) -> Result<String> {
    let text = encode_message(offer)?;
    let client = crate::a2a::A2AClient::shared();
    let ctx = crate::a2a::new_context_id();
    let result = client
        .call(peer_url, bearer, &text, &ctx)
        .await
        .with_context(|| format!("push to {peer_url}"))?;
    let state = result["status"]["state"].as_str().unwrap_or("?");
    let reply = crate::a2a::summarize_result(&result);
    if state != crate::a2a::STATE_COMPLETED {
        anyhow::bail!("{peer_url} refused the offer:\n{reply}");
    }
    Ok(reply)
}

/// The store-meta key holding how far this node's rows have reached
/// `peer` (a local `config_log` rowid).
pub fn cursor_key(peer: &str) -> String {
    format!("configsync_cursor:{peer}")
}

/// Sections whose replicated changes wait for the owner even with
/// `configsync_auto` on (USER 2026-10-04: "All but security"): who may
/// do what, what runs without asking, and what code is loaded.
pub const HELD_SECTIONS: &[&str] = &[
    "acl",
    "approvals",
    "sandbox",
    "hooks",
    "plugins",
    "mcp_servers",
];

/// Does a change to `key` wait for the owner?
pub fn is_held_key(key: &str) -> bool {
    split_key(key)
        .ok()
        .and_then(|segs| segs.first().cloned())
        .is_some_and(|top| HELD_SECTIONS.contains(&top.as_str()))
}

/// An offer split into what applies on receipt and what waits.
pub fn split_held(offer: &ConfigSyncOffer) -> (ConfigSyncOffer, ConfigSyncOffer) {
    // A row for a key that is node-local HERE is never the owner's to
    // decide: the import keeps it out whatever they say (2026-10-14: a
    // peer's `sandbox.backend` sat in two inboxes as a security change
    // to approve). It goes down the immediate path, where it is kept
    // out and counted.
    let (held, now): (Vec<ConfigLogRow>, Vec<ConfigLogRow>) = offer
        .rows
        .iter()
        .cloned()
        .partition(|r| is_held_key(&r.key) && is_replicated_key(&r.key));
    let part = |rows| ConfigSyncOffer {
        from_node: offer.from_node.clone(),
        from_name: offer.from_name.clone(),
        since_seq: offer.since_seq,
        rows,
    };
    (part(now), part(held))
}

/// One inbox offer's fate under `configsync_auto`.
#[derive(Debug)]
pub struct AutoApplied {
    pub file: String,
    pub from: String,
    pub report: ApplyReport,
    /// Keys still waiting for the owner (a held section).
    pub held: Vec<String>,
}

/// `configsync_auto`, the receiving half: apply every inbox offer now,
/// oldest first, except the rows for a held section, which stay in the
/// inbox (the offer is rewritten to hold only them). An offer whose
/// apply reports a CONFLICT stays whole — the winner is written, the
/// decision is a person's, as with `veles configsync apply --inbox`.
pub fn apply_inbox_auto(store: &crate::store::Store, local_node: &str) -> Result<Vec<AutoApplied>> {
    static DRAIN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _drain = DRAIN.lock().unwrap_or_else(|p| p.into_inner());
    let dir = inbox_dir();
    let done = dir.join("applied");
    let mut out = Vec::new();
    for p in inbox_pending(&dir)? {
        let mut entry = inbox_read(&p)?;
        let (now, mut held) = split_held(&entry.offer);
        let report = if now.rows.is_empty() {
            ApplyReport::default()
        } else {
            apply_offer(store, local_node, &now)?
        };
        // A held row that would change nothing here (the same value on
        // both nodes) lands quietly: the hold is for a CHANGE to a
        // security section, and the first sync between two nodes
        // carries every such key they already agree on.
        if !held.rows.is_empty() && report.is_clean() {
            let p = preview_offer(store, local_node, &held)?;
            if p.changed.is_empty() && p.is_clean() {
                apply_offer(store, local_node, &held)?;
                held.rows.clear();
            }
        }
        let file = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let held_keys: Vec<String> = held.rows.iter().map(|r| r.key.clone()).collect();
        if report.is_clean() {
            if held.rows.is_empty() {
                std::fs::create_dir_all(&done)?;
                match std::fs::rename(&p, done.join(&file)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("archive {}", p.display())),
                }
            } else if held.rows.len() != entry.offer.rows.len() {
                entry.offer = held;
                let tmp = p.with_extension("json.tmp");
                std::fs::write(&tmp, serde_json::to_vec_pretty(&entry)?)
                    .with_context(|| format!("write {}", tmp.display()))?;
                std::fs::rename(&tmp, &p).with_context(|| format!("rename {}", p.display()))?;
            }
        }
        out.push(AutoApplied {
            file,
            from: if entry.offer.from_name.is_empty() {
                entry.offer.from_node.clone()
            } else {
                entry.offer.from_name.clone()
            },
            report,
            held: held_keys,
        });
    }
    Ok(out)
}

/// One inbox offer as the owner decided it.
pub struct InboxApplied {
    pub from: String,
    pub report: ApplyReport,
    /// False: applied, but the file could not be moved to `applied/`.
    pub archived: bool,
}

/// Apply one inbox offer WHOLE — rows for a held section and a
/// conflict's winners included — and archive it in `applied/`.
/// `/configsync apply` runs this per offer, and so does the owner's
/// "apply" to a sync question (`syncask`).
pub fn apply_inbox_file(
    store: &crate::store::Store,
    local_node: &str,
    path: &Path,
) -> Result<InboxApplied> {
    let entry = inbox_read(path)?;
    let report = apply_offer(store, local_node, &entry.offer)?;
    Ok(InboxApplied {
        from: if entry.offer.from_name.is_empty() {
            entry.offer.from_node.clone()
        } else {
            entry.offer.from_name.clone()
        },
        report,
        archived: inbox_set_aside(path, "applied").is_ok(),
    })
}

/// Set one inbox offer aside unapplied (`dropped/`): a held change that
/// is wrong HERE — a peer's [sandbox] for its own runtime, say — would
/// otherwise wait, and be asked about, for ever.
pub fn drop_inbox_file(path: &Path) -> Result<()> {
    inbox_set_aside(path, "dropped")
}

fn inbox_set_aside(path: &Path, sub: &str) -> Result<()> {
    let dir = path
        .parent()
        .context("an inbox file has a directory")?
        .join(sub);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::rename(path, dir.join(path.file_name().unwrap_or_default()))
        .with_context(|| format!("move {} to {sub}/", path.display()))
}

/// Keys waiting in the inbox, by offer: what `/configsync` lists and
/// `/api/status` counts.
pub fn inbox_waiting() -> Result<Vec<(String, Vec<String>)>> {
    let mut out = Vec::new();
    for p in inbox_pending(&inbox_dir())? {
        let entry = inbox_read(&p)?;
        let from = if entry.offer.from_name.is_empty() {
            entry.offer.from_node.clone()
        } else {
            entry.offer.from_name.clone()
        };
        out.push((
            from,
            entry.offer.rows.iter().map(|r| r.key.clone()).collect(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (tempfile::TempDir, crate::store::Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("state.db")).unwrap();
        (dir, store)
    }

    fn row(node: &str, seq: i64, at: &str, key: &str, value: &str) -> ConfigLogRow {
        ConfigLogRow {
            node: node.into(),
            node_seq: seq,
            at: at.into(),
            key: key.into(),
            value: value.into(),
        }
    }

    /// A recovering node takes the whole log back — its own rows among
    /// them — BEFORE anything scans. Its next write then numbers after its
    /// own history, instead of taking `(me, 1)`, which every peer already
    /// holds and would drop as a duplicate (PLAN-recover.md R4).
    #[test]
    fn a_recovered_log_numbers_this_nodes_next_write_after_its_history() {
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        let t = "2026-10-01T00:00:00.000000000Z";
        let rows = vec![
            row("me", 1, t, "model.default", "\"mine-before\""),
            row("me", 2, t, "agent.tts_voice", "\"nova\""),
            row("me", 3, t, "node.name", "\"laptop\""),
            row("peer", 1, t, "model.provider", "\"stub\""),
        ];
        let imp = import_recovered(&store, "me", &rows).unwrap();
        assert_eq!(
            imp,
            RecoveredImport {
                imported: 3,
                present: 0,
                local: 1,
                mine: 2,
                nodes: 2
            }
        );
        let again = import_recovered(&store, "me", &rows).unwrap();
        assert_eq!((again.imported, again.present), (0, 3), "idempotent");

        // The file as a join leaves it, then the merge of everything.
        let path = config_path();
        std::fs::write(&path, "[platforms.a2a]\nenabled = true\n").unwrap();
        let rep = apply_offer(
            &store,
            "me",
            &ConfigSyncOffer {
                from_node: "me".into(),
                from_name: "laptop".into(),
                since_seq: 0,
                rows: Vec::new(),
            },
        )
        .unwrap();
        assert!(rep.is_clean(), "{rep:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("mine-before") && text.contains("nova") && text.contains("stub"),
            "{text}"
        );
        assert!(
            !text.contains("laptop"),
            "a local key never comes back: {text}"
        );
        let mine: Vec<ConfigLogRow> = store
            .config_log_rows()
            .unwrap()
            .into_iter()
            .filter(|r| r.node == "me")
            .collect();
        let enabled = mine
            .iter()
            .find(|r| r.key == "platforms.a2a.enabled")
            .expect("the join's write is logged");
        assert_eq!(
            enabled.node_seq, 3,
            "after the history, not over it: {mine:?}"
        );
    }

    /// A dry run reports what an apply would change and changes nothing:
    /// not the file, not the log, not the scan snapshot.
    #[test]
    fn a_preview_writes_nothing() {
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        let path = crate::config::config_dir().join("config.toml");
        std::fs::write(&path, "[model]\ndefault = \"mine\"\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
            .unwrap();
        let offer = ConfigSyncOffer {
            from_node: "peer".into(),
            from_name: "laptop".into(),
            since_seq: 0,
            rows: vec![row("peer", 1, &now_stamp(), "model.default", "\"theirs\"")],
        };
        let before_rows = store.config_log_rows().unwrap().len();
        let report = preview_offer(&store, "me", &offer).unwrap();
        assert!(
            report.changed.contains(&"model.default".to_string()),
            "{report:?}"
        );
        assert_eq!(report.imported, 1);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[model]\ndefault = \"mine\"\n",
            "the file is untouched"
        );
        assert_eq!(
            store.config_log_rows().unwrap().len(),
            before_rows,
            "nothing logged"
        );
        assert!(
            store.config_scan_get().unwrap().is_empty(),
            "no snapshot written"
        );
    }

    /// `configsync_auto` on receipt: the model key lands, the acl key
    /// waits in the inbox — the offer rewritten to hold only it.
    #[test]
    fn auto_apply_lands_the_rest_and_holds_security() {
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        let path = crate::config::config_dir().join("config.toml");
        std::fs::write(&path, "[model]\ndefault = \"mine\"\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
            .unwrap();
        let at = now_stamp();
        let offer = ConfigSyncOffer {
            from_node: "peer".into(),
            from_name: "laptop".into(),
            since_seq: 0,
            rows: vec![
                row("peer", 1, &at, "model.default", "\"theirs\""),
                row("peer", 2, &at, "acl.default_role", "\"user\""),
            ],
        };
        inbox_store(&inbox_dir(), "laptop", "t1", offer).unwrap();
        let done = apply_inbox_auto(&store, "me").unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].held, vec!["acl.default_role".to_string()]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"theirs\""),
            "the model key applied:\n{text}"
        );
        assert!(
            !text.contains("default_role"),
            "the acl key did not:\n{text}"
        );
        let waiting = inbox_waiting().unwrap();
        assert_eq!(
            waiting,
            vec![("laptop".to_string(), vec!["acl.default_role".to_string()])],
            "only the held row waits"
        );
        // A second pass (another offer arriving) leaves it waiting.
        apply_inbox_auto(&store, "me").unwrap();
        assert_eq!(inbox_waiting().unwrap().len(), 1);
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("default_role"));
    }

    /// A peer's row for a key that is node-local HERE — its fence's
    /// backend, in a held section — is never the owner's to approve: it
    /// is kept out at once and counted, and only the real security
    /// change waits (2026-10-14: winbox's `native` sat in the hub's
    /// and the laptop's inboxes as a change to approve).
    #[test]
    fn a_node_local_row_in_a_held_section_is_kept_out_not_held() {
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        let path = crate::config::config_dir().join("config.toml");
        std::fs::write(&path, "[sandbox]\nbackend = \"podman\"\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
            .unwrap();
        let at = now_stamp();
        let offer = ConfigSyncOffer {
            from_node: "peer".into(),
            from_name: "winbox".into(),
            since_seq: 0,
            rows: vec![
                row("peer", 1, &at, "sandbox.backend", "\"native\""),
                row("peer", 2, &at, "acl.default_role", "\"user\""),
            ],
        };
        let (now, held) = split_held(&offer);
        assert_eq!(
            now.rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["sandbox.backend"]
        );
        assert_eq!(
            held.rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["acl.default_role"]
        );
        inbox_store(&inbox_dir(), "winbox", "t1", offer).unwrap();
        let done = apply_inbox_auto(&store, "me").unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].report.local_keys, 1, "{:?}", done[0].report);
        assert_eq!(done[0].held, vec!["acl.default_role".to_string()]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"podman\""),
            "this node's fence stays:\n{text}"
        );
        assert!(!text.contains("native"), "{text}");
        assert_eq!(
            inbox_waiting().unwrap(),
            vec![("winbox".to_string(), vec!["acl.default_role".to_string()])],
            "only the real security change waits"
        );
    }

    /// The first sync between two nodes carries every security key they
    /// already agree on; only a CHANGE waits for the owner.
    #[test]
    fn a_held_row_that_changes_nothing_lands_quietly() {
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        let path = crate::config::config_dir().join("config.toml");
        std::fs::write(&path, "[acl]\nlockdown = false\n").unwrap();
        let offer = ConfigSyncOffer {
            from_node: "peer".into(),
            from_name: "laptop".into(),
            since_seq: 0,
            rows: vec![row("peer", 1, &now_stamp(), "acl.lockdown", "false")],
        };
        inbox_store(&inbox_dir(), "laptop", "t1", offer).unwrap();
        let done = apply_inbox_auto(&store, "me").unwrap();
        assert!(done[0].held.is_empty(), "{:?}", done[0].held);
        assert!(inbox_waiting().unwrap().is_empty());
    }

    /// A key that describes the machine stays on it, inside a section
    /// that replicates (the 2026-10-04 laptop↔VPS preview).
    #[test]
    fn a_machine_key_in_a_shared_section_never_replicates() {
        for k in NODE_KEYS {
            assert!(!is_replicated_key(k), "{k}");
        }
        assert!(is_replicated_key("platforms.irc.server"));
        assert!(is_replicated_key("web.searxng_timeout_secs"));
        let scanned = scan_config_toml(
            "[web]\nbackends = [\"brave\"]\nsearxng_timeout_secs = 5\n\n\
             [platforms.irc]\nmedia_public_dir = \"/opt/x\"\nserver = \"irc.example\"\n",
        )
        .unwrap();
        assert!(!scanned.contains_key("web.backends"), "{scanned:?}");
        assert!(
            !scanned.contains_key("platforms.irc.media_public_dir"),
            "{scanned:?}"
        );
        assert!(scanned.contains_key("platforms.irc.server"), "{scanned:?}");
        // The fence a machine has is its own; the policy it runs under
        // is shared (2026-10-14: winbox's `native` row reached the
        // hub's inbox as a change for the owner to approve).
        let scanned = scan_config_toml(
            "[sandbox]\nbackend = \"native\"\nimage = \"localhost/veles-sandbox\"\n\
             extra_args = [\"--userns=keep-id\"]\nmemory_mb = 256\nnetwork = false\n\n\
             [[sandbox.mounts]]\nhost = \"/srv/x\"\nguest = \"/x\"\n",
        )
        .unwrap();
        for k in ["sandbox.backend", "sandbox.image", "sandbox.extra_args"] {
            assert!(!scanned.contains_key(k), "{k} travelled: {scanned:?}");
        }
        assert!(
            !scanned.keys().any(|k| k.starts_with("sandbox.mounts")),
            "{scanned:?}"
        );
        assert!(scanned.contains_key("sandbox.memory_mb"), "{scanned:?}");
        assert!(scanned.contains_key("sandbox.network"), "{scanned:?}");
    }

    /// Where A2A listens is the machine's: the VPS binds 0.0.0.0, the
    /// laptop and the phone their WireGuard addresses. Pool join writes it
    /// to config.toml (`platforms.a2a.extra.host`, beside the port) and
    /// `platforms` replicates, so without the NODE_KEYS row the next local
    /// write logged the laptop's address and shipped it to the VPS — which
    /// would then try to bind an address it does not have. Nor may a
    /// peer's row for it land here.
    #[test]
    fn the_a2a_bind_host_stays_on_its_node() {
        assert!(!is_replicated_key("platforms.a2a.extra.host"));
        assert!(is_replicated_key("platforms.a2a.enabled"));
        let scanned = scan_config_toml(
            "[platforms.a2a]\nenabled = true\n\n[platforms.a2a.extra]\nhost = \"10.0.30.3\"\n",
        )
        .unwrap();
        assert!(
            !scanned.contains_key("platforms.a2a.extra.host"),
            "the bind host would be logged: {scanned:?}"
        );
        assert!(scanned.contains_key("platforms.a2a.enabled"), "{scanned:?}");

        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        let path = crate::config::config_dir().join("config.toml");
        std::fs::write(&path, "[platforms.a2a.extra]\nhost = \"10.0.30.3\"\n").unwrap();
        let offer = ConfigSyncOffer {
            from_node: "peer".into(),
            from_name: "vps".into(),
            since_seq: 0,
            rows: vec![row(
                "peer",
                1,
                &now_stamp(),
                "platforms.a2a.extra.host",
                "\"0.0.0.0\"",
            )],
        };
        inbox_store(&inbox_dir(), "vps", "t1", offer).unwrap();
        let done = apply_inbox_auto(&store, "me").unwrap();
        assert_eq!(done[0].report.local_keys, 1, "{:?}", done[0].report);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("10.0.30.3"),
            "this node's host stays:\n{text}"
        );
        assert!(!text.contains("0.0.0.0"), "{text}");
    }

    /// USER 2026-10-04: "All but security" — a change to who may do what,
    /// what runs unasked, or what code loads waits for the owner.
    #[test]
    fn security_sections_are_held_and_the_rest_is_not() {
        for k in [
            "acl.default_role",
            "approvals.mode",
            "sandbox.network",
            "hooks.on_turn",
            "plugins.enabled",
            "mcp_servers.x",
        ] {
            assert!(is_held_key(k), "{k} must wait for the owner");
        }
        for k in [
            "model.default",
            "agent.reasoning_effort",
            "custom_providers.ali",
        ] {
            assert!(!is_held_key(k), "{k} applies on receipt");
        }
        let offer = ConfigSyncOffer {
            from_node: "n".into(),
            from_name: "laptop".into(),
            since_seq: 0,
            rows: vec![
                row("n", 1, "2099-01-01T00:00:00Z", "model.default", "\"m\""),
                row(
                    "n",
                    2,
                    "2099-01-01T00:00:00Z",
                    "acl.default_role",
                    "\"user\"",
                ),
            ],
        };
        let (now, held) = split_held(&offer);
        assert_eq!(now.rows.len(), 1);
        assert_eq!(held.rows[0].key, "acl.default_role");
    }

    /// The scope table is load-bearing: `node`, `cluster` and `tui`
    /// must NEVER replicate (they describe the machine reading them,
    /// or its own face). `platforms` DOES travel per key since
    /// 2026-09-28 (B3a's fence answers placement). `acl` replicates
    /// per key: its scopes are surfaces and channels, the same on any
    /// node.
    #[test]
    fn local_sections_never_replicate() {
        for top in ["node", "cluster", "tui"] {
            assert!(
                matches!(section_class(top), SectionClass::Local),
                "`{top}` must classify Local"
            );
            assert!(
                !is_replicated_key(&format!("{top}.anything")),
                "`{top}.anything` must not be a replicated key"
            );
        }
        // The keys that verify tickets (PLAN-pool-round.md D2): which
        // devices may authorize calls here is this node's own decision.
        // A replicated signer list would let one compromised node add a
        // key to every node's list — the blast radius tickets exist to
        // bound.
        assert!(!is_replicated_key("cluster.ticket_signers"));
        assert!(!is_replicated_key("cluster.ticket_signers.phone"));
        // And the shared sections do.
        assert!(is_replicated_key("model.model"));
        assert!(is_replicated_key("custom_providers.bailian"));
        assert!(is_replicated_key("fallback_providers"));
        // ACL travels per key: surfaces and channels, not nodes.
        assert!(is_replicated_key("acl.entries"));
        // `platforms` UNLOCKED 2026-09-28: B3a's fence exists, so a
        // replicated platform row arms on exactly the node the arbiter
        // leases to. It travels per key now.
        assert!(is_replicated_key("platforms.telegram.enabled"));
        // Unknown sections fail CLOSED (Local), not open.
        assert!(matches!(
            section_class("brand_new_section"),
            SectionClass::Local
        ));
        // The failover rules are `[[failover]]` in TOML, and travel as
        // one chain; model aliases travel per name (2026-10-04).
        assert!(matches!(section_class("failover"), SectionClass::Blob));
        assert!(is_replicated_key("model_aliases.fast"));
    }

    /// The scanner reads a real-shaped config and produces dotted keys
    /// with stable value text — map leaves, element-array elements,
    /// blob sections — and skips local sections entirely.
    #[test]
    fn scan_reads_shapes_and_skips_local() {
        let raw = r#"
# a comment that must survive
[model]
model = "deepseek-v4-flash-0731"
api_mode = "chat_completions"

[node]
static_ip = "192.168.1.50"     # must NOT appear
name = "laptop"

[cluster]
seeds = ["10.0.20.1"]          # must NOT appear

[[custom_providers]]
name = "bailian"
base_url = "https://example.invalid/v1"

[[command_aliases]]
name = "team"
provider = "alibaba"
model = "deepseek-v4-flash"

[fallback_providers]
"#;
        let snap = scan_config_toml(raw).unwrap();
        assert_eq!(
            snap.get("model.model").unwrap(),
            "\"deepseek-v4-flash-0731\""
        );
        assert_eq!(snap.get("model.api_mode").unwrap(), "\"chat_completions\"");
        assert!(snap.contains_key("custom_providers.bailian"));
        assert!(snap["custom_providers.bailian"].contains("bailian"));
        assert!(snap.contains_key("command_aliases.team"));
        // Local sections produce NO keys at any depth.
        for k in snap.keys() {
            assert!(!k.starts_with("node."), "leaked local key: {k}");
            assert!(!k.starts_with("cluster."), "leaked local key: {k}");
        }
    }

    /// scan_and_log detects file changes as THIS node's writes, with
    /// per-node monotonic seqs; a second scan of an unchanged file
    /// logs nothing (the snapshot is the baseline).
    #[test]
    fn scan_and_log_detects_and_numbers_writes() {
        let (_d, store) = scratch();
        let v1 = "[model]\nmodel = \"a\"\n";
        let r1 = scan_and_log(&store, "nodeA", v1).unwrap();
        assert_eq!(r1.logged, vec!["model.model"]);
        let v2 = "[model]\nmodel = \"b\"\n";
        let r2 = scan_and_log(&store, "nodeA", v2).unwrap();
        assert_eq!(r2.logged, vec!["model.model"]);
        let r3 = scan_and_log(&store, "nodeA", v2).unwrap();
        assert!(r3.logged.is_empty() && r3.tombstoned.is_empty());
        let rows = store.config_log_rows().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].node_seq, 1);
        assert_eq!(rows[1].node_seq, 2);
        assert_eq!(rows[1].value, "\"b\"");
    }

    /// A key removed from the file becomes a TOMBSTONE — not silence.
    /// Without tombstones a deleted key resurrects from an old
    /// replica (the merge would see only the old write).
    #[test]
    fn deletions_become_tombstones() {
        let (_d, store) = scratch();
        scan_and_log(&store, "nodeA", "[model]\nmodel = \"a\"\n").unwrap();
        let r = scan_and_log(&store, "nodeA", "[model]\napi_mode = \"x\"\n").unwrap();
        assert_eq!(r.tombstoned, vec!["model.model"]);
        let merged = merge_rows(&store.config_log_rows().unwrap());
        assert!(!merged.values.contains_key("model.model"));
        assert_eq!(
            merged.values.get("model.api_mode"),
            Some(&"\"x\"".to_string())
        );
    }

    /// LWW with the deterministic tiebreak: same timestamp, different
    /// nodes → the lexically later node id wins, on BOTH nodes, with
    /// no communication. That is what makes the merge leaderless.
    #[test]
    fn merge_is_deterministic_lww() {
        let at = "2026-10-02T12:00:00.000000000Z";
        let rows = vec![
            row("nodeA", 1, at, "model.model", "\"from-a\""),
            row("nodeB", 1, at, "model.model", "\"from-b\""),
        ];
        // Same input, two different arrival orders: same winner.
        let m1 = merge_rows(&rows);
        let mut shuffled = rows.clone();
        shuffled.reverse();
        let m2 = merge_rows(&shuffled);
        assert_eq!(m1.values.get("model.model"), m2.values.get("model.model"));
        // The tiebreak: (at, node) — nodeB > nodeA lexically.
        assert_eq!(
            m1.values.get("model.model"),
            Some(&"\"from-b\"".to_string())
        );
        // Same timestamp + different values from different nodes IS a
        // conflict (inside the window, different nodes, different values).
        assert_eq!(m1.conflicts.len(), 1);
    }

    /// Writes far apart in time are NOT conflicts even when different
    /// nodes wrote them — ordinary LWW. The window exists to catch
    /// concurrent edits, not staggered ones.
    #[test]
    fn staggered_writes_are_not_conflicts() {
        let rows = vec![
            row(
                "nodeA",
                1,
                "2026-10-02T12:00:00.000000000Z",
                "model.model",
                "\"from-a\"",
            ),
            row(
                "nodeB",
                1,
                "2026-10-02T18:00:00.000000000Z",
                "model.model",
                "\"from-b\"",
            ),
        ];
        let m = merge_rows(&rows);
        assert!(m.conflicts.is_empty());
        assert_eq!(m.values.get("model.model"), Some(&"\"from-b\"".to_string()));
    }

    /// Import is idempotent on (node, node_seq): the same offer twice
    /// inserts once — re-pushes and partial-push retries are safe.
    #[test]
    fn import_is_idempotent() {
        let (_d, store) = scratch();
        let r = row(
            "nodeB",
            7,
            "2026-10-02T12:00:00.000000000Z",
            "model.model",
            "\"x\"",
        );
        assert!(store.config_log_append(&r).unwrap());
        assert!(
            !store.config_log_append(&r).unwrap(),
            "second insert must be a no-op"
        );
        assert_eq!(store.config_log_rows().unwrap().len(), 1);
    }

    /// apply_offer converges two nodes and the echo guard holds: the
    /// importing node's next scan does NOT log the imported value as
    /// its own write. Without the guard, apply→scan→push would bounce
    /// the same value between peers forever.
    #[test]
    fn apply_converges_and_the_echo_guard_holds() {
        let (d_a, store_a) = scratch();
        let (_d_b, store_b) = scratch();
        // Node A has a config with one replicated key.
        std::fs::write(
            d_a.path().join("config-a.toml"),
            "[model]\nmodel = \"a-model\"\n",
        )
        .unwrap();
        scan_and_log(&store_a, "nodeA", "[model]\nmodel = \"a-model\"\n").unwrap();
        let (offer, withheld) = build_offer(&store_a, "nodeA", "A", 0).unwrap();
        assert!(withheld.is_empty());
        assert_eq!(offer.rows.len(), 1);

        // Node B applies it — but apply reads config.toml at the real
        // config path, so point it at a scratch home.
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        std::fs::write(
            crate::config::config_dir().join("config.toml"),
            "[model]\nmodel = \"b-model\"\n",
        )
        .unwrap();
        // B's value predates A's write (the file was last edited two
        // hours ago): apply stamps an unscanned edit with its mtime, and
        // LWW then hands the key to A. A file edited AFTER A's write
        // would keep B's value — or, inside the window, banner.
        std::fs::File::options()
            .write(true)
            .open(crate::config::config_dir().join("config.toml"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
            .unwrap();
        let report = apply_offer(&store_b, "nodeB", &offer).unwrap();
        assert!(report.is_clean(), "no conflict: A wrote later than nobody");
        let after =
            std::fs::read_to_string(crate::config::config_dir().join("config.toml")).unwrap();
        assert!(
            after.contains("\"a-model\""),
            "file must carry the merged winner:\n{after}"
        );

        // The echo guard: B's scan of its own (just-written) file logs
        // NOTHING — the snapshot already reflects the imported value.
        let r = scan_and_log(&store_b, "nodeB", &after).unwrap();
        assert!(
            r.logged.is_empty() && r.tombstoned.is_empty(),
            "echo guard failed: {} logged, {} tombstoned",
            r.logged.len(),
            r.tombstoned.len()
        );
    }

    /// The merge never writes a local section into the file even when
    /// a peer's offer carries one (a misconfigured or newer-schema
    /// peer): apply SKIPS rows whose key classifies Local here.
    #[test]
    fn apply_refuses_local_keys_from_peers() {
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        std::fs::write(
            crate::config::config_dir().join("config.toml"),
            "[model]\nmodel = \"local\"\n",
        )
        .unwrap();
        // The peer's writes must be LATER than this node's own scan of
        // the file, which is stamped now: a fixed date in the near future
        // (2026-10-02, when this was written a week earlier) became the
        // past at noon that day, and the local value won.
        let offer = ConfigSyncOffer {
            from_node: "nodeX".into(),
            from_name: "X".into(),
            since_seq: 0,
            rows: vec![
                row(
                    "nodeX",
                    1,
                    "2099-01-01T00:00:00.000000001Z",
                    "model.model",
                    "\"peer\"",
                ),
                row(
                    "nodeX",
                    2,
                    "2099-01-01T00:00:00.000002Z",
                    "node.static_ip",
                    "\"1.2.3.4\"",
                ),
                row(
                    "nodeX",
                    3,
                    "2099-01-01T00:00:00.000003Z",
                    "cluster.seeds",
                    "[\"x\"]",
                ),
            ],
        };
        let report = apply_offer(&store, "nodeB", &offer).unwrap();
        assert_eq!(report.imported, 1, "only the replicated key imports");
        assert_eq!(
            report.local_keys, 2,
            "node.* and cluster.* rows are kept out"
        );
        let after =
            std::fs::read_to_string(crate::config::config_dir().join("config.toml")).unwrap();
        assert!(after.contains("\"peer\""));
        assert!(!after.contains("static_ip"), "node must not land:\n{after}");
        assert!(!after.contains("seeds"), "cluster must not land:\n{after}");
    }

    /// A credential-shaped value never travels: build_offer withholds
    /// it and names the key.
    #[test]
    fn secrets_never_travel() {
        let (_d, store) = scratch();
        scan_and_log(
            &store,
            "nodeA",
            "[web]\nsearch_url = \"https://x.test/?k=sk-1234567890abcdef\"\n",
        )
        .unwrap();
        let (offer, withheld) = build_offer(&store, "nodeA", "A", 0).unwrap();
        assert_eq!(withheld.len(), 1, "the key must be named");
        assert!(offer.rows.is_empty(), "no row may carry the secret");
    }

    /// Round-trip through the wire format: encode → decode returns the
    /// same offer; a tampered payload fails the checksum.
    #[test]
    fn wire_round_trip_and_tamper_detection() {
        let offer = ConfigSyncOffer {
            from_node: "nodeA".into(),
            from_name: "A".into(),
            since_seq: 0,
            rows: vec![row(
                "nodeA",
                1,
                "2026-10-02T12:00:00.000000000Z",
                "model.model",
                "\"m\"",
            )],
        };
        let text = encode_message(&offer).unwrap();
        assert!(is_config_offer(&text));
        let back = decode_message(&text).unwrap();
        assert_eq!(back.rows.len(), 1);
        assert_eq!(back.rows[0].key, "model.model");
        // Tamper: flip a byte in the base64 body — the checksum must
        // catch it.
        let mut tampered = text.clone();
        let pos = tampered.find('m').unwrap();
        tampered.replace_range(pos..pos + 1, "w");
        assert!(
            decode_message(&tampered).is_err(),
            "checksum must catch tampering"
        );
    }

    /// effective_config_toml preserves comments and untouched local
    /// sections — the operator's file stays THEIR file.
    #[test]
    fn file_edits_preserve_comments_and_local_sections() {
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        let raw = "# top comment\n[node]\nname = \"laptop\" # inline\n\n[model]\nmodel = \"old\"\n";
        let mut merged = MergedState::default();
        merged.values.insert("model.model".into(), "\"new\"".into());
        let out = effective_config_toml(raw, &merged).unwrap();
        assert!(out.contains("# top comment"), "comments survive:\n{out}");
        assert!(out.contains("name = \"laptop\" # inline"));
        assert!(out.contains("\"new\""));
        assert!(!out.contains("\"old\""));
    }

    /// Blob sections replace wholesale: node B's later write of
    /// `fallback_providers` replaces node A's whole chain, because an
    /// ordered chain cannot be element-merged.
    #[test]
    fn blob_sections_replace_wholesale() {
        let rows = vec![
            row(
                "nodeA",
                1,
                "2026-10-02T12:00:00.000000000Z",
                "fallback_providers",
                "{ provider = \"a\", model = \"ma\" }\n{ provider = \"b\" }",
            ),
            row(
                "nodeB",
                1,
                "2026-10-02T12:40:00.000000000Z",
                "fallback_providers",
                "{ provider = \"z\" }",
            ),
        ];
        let m = merge_rows(&rows);
        assert_eq!(
            m.values.get("fallback_providers"),
            Some(&"{ provider = \"z\" }".to_string())
        );
        assert!(m.conflicts.is_empty(), "40 minutes apart is not concurrent");
    }

    /// Element arrays merge per element: two nodes adding DIFFERENT
    /// custom_providers keep both — the union, not a clobber.
    #[test]
    fn element_arrays_merge_per_element() {
        let rows = vec![
            row(
                "nodeA",
                1,
                "2026-10-02T12:00:00.000000000Z",
                "custom_providers.alpha",
                "{ name = \"alpha\", base_url = \"https://a\" }",
            ),
            row(
                "nodeB",
                1,
                "2026-10-02T12:00:30.000000000Z",
                "custom_providers.beta",
                "{ name = \"beta\", base_url = \"https://b\" }",
            ),
        ];
        let m = merge_rows(&rows);
        assert!(m.values.contains_key("custom_providers.alpha"));
        assert!(m.values.contains_key("custom_providers.beta"));
        assert!(m.conflicts.is_empty(), "different keys are never conflicts");
    }

    // ── review 2026-09-26: what b8cc067's apply did to a real file ──

    /// A stamp `hours` from now, in the fixed-width form every writer uses.
    fn stamp_in(hours: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::hours(hours))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    }

    /// Write the scratch home's config.toml, 0600 like the live one.
    fn write_config(text: &str) -> std::path::PathBuf {
        let path = crate::config::config_dir().join("config.toml");
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        crate::platform::write_private(&path, text.as_bytes()).unwrap();
        path
    }

    fn peer_offer(rows: Vec<ConfigLogRow>) -> ConfigSyncOffer {
        ConfigSyncOffer {
            from_node: "peer".into(),
            from_name: "vps".into(),
            since_seq: 0,
            rows,
        }
    }

    /// An edit the operator made AFTER the last scan is not in the log,
    /// so it was not in the merge — and apply removed every file key the
    /// merge lacked, then overwrote the snapshot so nothing could notice.
    /// The edit must survive (and, being the newest write, win).
    #[test]
    fn apply_keeps_an_edit_made_since_the_last_scan() {
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        let base = "[model]\nmodel = \"b\"\n";
        write_config(base);
        scan_and_log(&store, "self", base).unwrap();
        // The operator's editor, no scan since.
        let path = write_config(&format!("{base}\n[vision]\nmodel = \"local-v\"\n"));
        let offer = peer_offer(vec![row("peer", 1, &stamp_in(-2), "agent.max_turns", "40")]);
        apply_offer(&store, "self", &offer).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("local-v"),
            "the unscanned edit survives the apply:\n{after}"
        );
        assert!(
            after.contains("max_turns = 40"),
            "and the peer's key lands:\n{after}"
        );
    }

    /// The other half: an unscanned CHANGE to a key the log already
    /// holds. Without the scan-first, the merge knows only the old
    /// logged value and the peer's (newer than that) — the peer's lands
    /// over the operator's newer edit. Scanned first, at the file's
    /// mtime, the edit is the newest write and stays.
    #[test]
    fn apply_weighs_an_unscanned_change_to_a_logged_key() {
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        // Logged long ago...
        store
            .config_log_append(&row("self", 1, &stamp_in(-48), "model.model", "\"b\""))
            .unwrap();
        let mut snap = BTreeMap::new();
        snap.insert("model.model".to_string(), "\"b\"".to_string());
        store.config_scan_replace(&snap).unwrap();
        // ...then edited just now, not scanned.
        let path = write_config("[model]\nmodel = \"c\"\n");
        let offer = peer_offer(vec![row("peer", 1, &stamp_in(-2), "model.model", "\"a\"")]);
        apply_offer(&store, "self", &offer).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("\"c\""),
            "the newest write (the unscanned edit) wins:\n{after}"
        );
    }

    /// A key NOBODY ever logged was never deleted by anyone: only a key
    /// whose winning row is a tombstone may leave the file.
    #[test]
    fn a_key_nobody_logged_is_never_removed() {
        let raw = "[model]\nmodel = \"x\"\n\n[vision]\nmodel = \"never-logged\"\n";
        let mut merged = MergedState::default();
        merged.values.insert("model.model".into(), "\"x\"".into());
        let out = effective_config_toml(raw, &merged).unwrap();
        assert!(out.contains("never-logged"), "nobody deleted it:\n{out}");
    }

    /// config.toml holds provider keys and is 0600 on the live box. The
    /// apply wrote a fresh inode via tmp+rename — born at the umask,
    /// 0644 — and its `.bak-configsync` copy likewise: two
    /// world-readable copies of every key.
    #[cfg(unix)]
    #[test]
    fn apply_keeps_the_config_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        let path = write_config("[model]\nmodel = \"old\"\n");
        // The local value predates the peer's write, so the peer's lands.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let offer = peer_offer(vec![row(
            "peer",
            1,
            &stamp_in(-1),
            "model.model",
            "\"new\"",
        )]);
        apply_offer(&store, "self", &offer).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"new\""));
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "config.toml stays owner-only");
        let bak = path.with_extension("toml.bak-configsync");
        assert_eq!(mode(&bak), 0o600, "and so does its backup");
    }

    /// The live config's shape: providers in a deliberate (non-sorted)
    /// order with `[custom_providers.models."…"]` sub-tables. Every
    /// apply re-set EVERY merged key — element arrays by remove+push,
    /// which sorted the providers by name (resolution is first-match in
    /// that order) and flattened the sub-tables into one inline table.
    /// A no-op apply must leave the file byte-identical, and a real
    /// change to one provider must keep the order and the layout.
    #[test]
    fn apply_touches_only_what_changed() {
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        let raw = "# providers, in resolution order\n\
[model]\nmodel = \"glm-5.2\"\n\n\
[[fallback_providers]]\nprovider = \"zai\"\nmodel = \"glm-5.2\"\n\n\
[[fallback_providers]]\nprovider = \"bailian\"\n\n\
[[custom_providers]]\nname = \"zai\"\nbase_url = \"https://z.example/v1\"\n\
# the models zai serves\n\
[custom_providers.models.\"glm-5.2\"]\ncontext_length = 200000\n\n\
[custom_providers.models.\"glm-5.3-flash\"]\ncontext_length = 128000\n\n\
[[custom_providers]]\nname = \"bailian\"\nbase_url = \"https://b.example/v1\"\n\
[custom_providers.models.\"qwen3.8-max\"]\ncontext_length = 262144\n";
        let path = write_config(raw);
        scan_and_log(&store, "self", raw).unwrap();
        apply_offer(&store, "self", &peer_offer(vec![])).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            raw,
            "a no-op apply leaves the file byte-identical"
        );

        // The peer changes zai's base_url, later than anything here.
        let mut zai = scan_config_toml(raw).unwrap()["custom_providers.zai"].clone();
        zai = zai.replace("https://z.example/v1", "https://z2.example/v1");
        apply_offer(
            &store,
            "self",
            &peer_offer(vec![row(
                "peer",
                1,
                &stamp_in(1),
                "custom_providers.zai",
                &zai,
            )]),
        )
        .unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("https://z2.example/v1"),
            "the change landed:\n{after}"
        );
        let (z, b) = (
            after.find("name = \"zai\"").unwrap(),
            after.find("name = \"bailian\"").unwrap(),
        );
        assert!(z < b, "provider order is resolution order — kept:\n{after}");
        assert!(
            after.contains("[custom_providers.models.\"glm-5.2\"]")
                && after.contains("[custom_providers.models.\"qwen3.8-max\"]"),
            "sub-tables keep their headers:\n{after}"
        );
    }

    /// `[[acl.entries]]` is an array of tables INSIDE a map section. It
    /// scanned as newline-joined inline tables, and apply parsed that
    /// text back as ONE value — so a peer's change to any ACL entry
    /// could never be applied ("value for `acl.entries` does not
    /// parse"; b8cc067's binary fails exactly so on the live config).
    /// It must land as `[[acl.entries]]` tables again, and still load.
    #[test]
    fn a_changed_array_of_tables_in_a_map_section_applies() {
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        let raw = "[acl]\nowner = [\"irc:me\"]\n\n\
[[acl.entries]]\nidentity = \"a!*@*\"\nrole = \"user\"\n\n\
[[acl.entries]]\nidentity = \"b!*@*\"\nrole = \"guest\"\n";
        let path = write_config(raw);
        scan_and_log(&store, "self", raw).unwrap();
        let changed = raw.replace("role = \"guest\"", "role = \"trusted\"");
        let value = scan_config_toml(&changed).unwrap()["acl.entries"].clone();
        apply_offer(
            &store,
            "self",
            &peer_offer(vec![row("peer", 1, &stamp_in(1), "acl.entries", &value)]),
        )
        .unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("[[acl.entries]]"),
            "tables, as written:\n{after}"
        );
        assert!(after.contains("\"trusted\""), "the change landed:\n{after}");
        let cfg: crate::config::VelesConfig = toml::from_str(&after).expect("still loads");
        assert_eq!(cfg.acl.entries.len(), 2);
    }

    /// A changed key keeps the comments around it. toml_edit holds the
    /// lines above a key in the KEY's decor, so re-inserting the key
    /// (as the text round-trip did) dropped them — on a copy of the live
    /// config, changing `acl.default_role` deleted the operator's
    /// ten-line note on why strangers are guests.
    #[test]
    fn a_changed_key_keeps_its_comments() {
        let raw = "[acl]\n# why strangers are guests\n# (second line)\n\
                   default_role = \"guest\" # was user\n";
        let mut merged = MergedState::default();
        merged
            .values
            .insert("acl.default_role".into(), "\"trusted\"".into());
        let out = effective_config_toml(raw, &merged).unwrap();
        assert_eq!(
            out,
            "[acl]\n# why strangers are guests\n# (second line)\n\
             default_role = \"trusted\" # was user\n"
        );
    }

    /// A changed ELEMENT keeps its layout too: the comment inside it and
    /// its `[custom_providers.models."…"]` headers — only the value that
    /// changed moves.
    #[test]
    fn a_changed_element_keeps_its_layout() {
        let raw = "[[custom_providers]]\nname = \"zai\"\n# the endpoint\n\
                   base_url = \"https://z.example/v1\"\n\n\
                   [custom_providers.models.\"glm-5.2\"]\ncontext_length = 200000\n";
        let v = scan_config_toml(raw).unwrap()["custom_providers.zai"]
            .replace("https://z.example/v1", "https://z2.example/v1");
        let mut merged = MergedState::default();
        merged.values.insert("custom_providers.zai".into(), v);
        let out = effective_config_toml(raw, &merged).unwrap();
        assert_eq!(
            out,
            raw.replace("https://z.example/v1", "https://z2.example/v1")
        );
    }

    /// A key path with a QUOTED segment (`acl.extra_caps."irc:friend"`)
    /// is addressed by its NAME. The writer handed toml_edit the quoted
    /// spelling as a raw key, so it looked up — and created — a key
    /// literally named `"irc:friend"`, quotes included.
    #[test]
    fn a_quoted_key_segment_is_addressed_not_mangled() {
        let raw = "[acl.extra_caps]\n\"irc:friend\" = [\"shell\"]\n";
        let key = "acl.extra_caps.\"irc:friend\"";
        assert!(scan_config_toml(raw).unwrap().contains_key(key));
        let mut merged = MergedState::default();
        merged.values.insert(key.into(), "[\"web\"]".into());
        let out = effective_config_toml(raw, &merged).unwrap();
        assert_eq!(out, "[acl.extra_caps]\n\"irc:friend\" = [\"web\"]\n");
        let mut gone = MergedState::default();
        gone.deleted.insert(key.into());
        let out = effective_config_toml(raw, &gone).unwrap();
        assert!(
            !out.contains("irc:friend"),
            "the tombstone removes it:\n{out}"
        );
    }

    /// The conflict banner cut values at byte 40, which panics inside a
    /// multi-byte character — and this user's config is in 47 locales.
    #[test]
    fn the_conflict_banner_cuts_on_a_char_boundary() {
        let long = format!("\"{}\"", "ž".repeat(40));
        let report = ApplyReport {
            conflicts: vec![(
                "agent.system_prompt".into(),
                ConflictingWrite {
                    node: "a".into(),
                    at: "t".into(),
                    value: long.clone(),
                },
                ConflictingWrite {
                    node: "b".into(),
                    at: "t".into(),
                    value: long,
                },
            )],
            ..Default::default()
        };
        assert!(report.summarize().contains("CONFLICT"));
    }

    /// A row names the node that wrote it, and an offer carries only its
    /// sender's own rows (`config_log_rows_for(from_node)`). A row
    /// claiming ANY other node — this one included — is refused, or a
    /// peer could write history in another node's name.
    #[test]
    fn rows_claiming_another_node_are_refused() {
        let (_d, store) = scratch();
        let (_cfg, _data, _g) = crate::testenv::scratch_xdg();
        write_config("[model]\nmodel = \"mine\"\n");
        let offer = peer_offer(vec![row(
            "self",
            99,
            &stamp_in(1),
            "model.model",
            "\"forged\"",
        )]);
        apply_offer(&store, "self", &offer).unwrap();
        assert!(
            store
                .config_log_rows()
                .unwrap()
                .iter()
                .all(|r| r.node_seq != 99),
            "the forged row is not in the log"
        );
    }

    /// `platforms` replicates per key since 2026-09-28 (USER decision,
    /// with the B5 round): the B2-era hold ("local until B3 placement")
    /// existed because a replicated platform row would double a
    /// connection nothing fenced — and B3a's lease fence + B4's
    /// placement landed since. A replicated platform row arms on
    /// exactly the node the arbiter leases to; which node serves an
    /// adapter is placement, answered by the fence at boot.
    #[test]
    fn platforms_replicate_now_that_the_fence_exists() {
        assert!(is_replicated_key("platforms.irc.enabled"));
        assert!(is_replicated_key("platforms.telegram.home_channel.chat_id"));
        let scanned = scan_config_toml("[platforms.irc]\nenabled = true\n").unwrap();
        assert_eq!(
            scanned.len(),
            1,
            "the platform row is logged for replication: {scanned:?}"
        );
        // The machine-local sections still never travel.
        assert!(!is_replicated_key("node.name"));
        assert!(!is_replicated_key("cluster.seeds"));
    }
}
