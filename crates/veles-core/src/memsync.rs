//! Memory replication between pool nodes (Plan B, milestone B1a,
//! 2026-09-20).
//!
//! ## Why this is not git
//!
//! The plan's first sketch said "memory.d is already a git repo — push/
//! pull". That was stale: memory moved OFF git to SQLite
//! `memory_revisions` (see memory.rs's module doc for the measurements
//! that drove it). So replication here is ROW-level: a node bundles the
//! revisions newer than a cursor, a peer applies them in order, and the
//! two stores converge on the same history instead of trading whole
//! files or shelling out to a VCS that was deliberately retired.
//!
//! ## Transport: the existing A2A link, into an INBOX
//!
//! The pool already has an authenticated channel — the A2A server every
//! node runs — so a push rides it rather than a second socket with a
//! second auth story. But it does NOT ride it as conversation. The first
//! version sent the offer as an ordinary `message/send`, which made the
//! peer's A2A server run a full LLM turn over it (a full offer on the
//! live store is ~2.5 MB of content, ~700k tokens), left nothing on the
//! far side that `veles memory apply` could read, and exceeded the
//! server's 2 MiB request limit besides. It was never run end to end.
//!
//! Now the A2A server recognises an offer ([`is_offer_message`]) BEFORE
//! any turn or session exists, and — if the sending peer is named in
//! `[cluster] memsync_peers` — stores it in `<veles_home>/memsync/inbox/`
//! ([`inbox_store`]) and answers with what it stored. Nothing is applied
//! on receipt: `veles memory apply --inbox` is the explicit step.
//!
//! On the wire an offer is base64 inside a fenced block with its sha256
//! ([`encode_message`] / [`decode_message`]): the A2A client redacts
//! outbound text, and redaction inside memory content (every email
//! address becomes `[redacted-email]`) would land a silently ALTERED
//! file on the peer — whose next push would then conflict with itself.
//! Base64 is opaque to the redactor and the checksum catches anything
//! else. Offers are chunked below the request limit ([`chunk_offer`]);
//! chunks may split one path's history, because each chunk carries the
//! per-path base the previous chunk left behind.
//!
//! Paths holding something credential-shaped are NOT pushed
//! ([`crate::a2a::secret_shaped`]) — secrets stay on their node — and the
//! push names them. The dedicated streaming replication socket is still
//! B5; this remains operator-triggered.
//!
//! ## Scoping: node-local paths stay put (B1.5, 2026-09-26)
//!
//! Not all of memory is identity-level shared state: `system/core.md`
//! carried both nodes' note paths, and a statement pushed to a machine
//! where it is false is a superset of the truth. [`LocalPaths`] is the
//! filter — `[cluster] local_memory_globs` plus the always-local
//! `reference/nodes/**` convention — and BOTH ends apply it: the sender
//! withholds ([`build_offer_scoped`]) and the receiver refuses to fold a
//! path that is local on IT ([`apply_offer_scoped`]), because `[cluster]`
//! never replicates and a peer that did not name a path still ships it.
//! Skills remain host-local (`skill_roots()` reads only this machine).
//!
//! ## Conflict detection
//!
//! Two nodes can edit the same path while partitioned. Replication must
//! surface that, never silently last-writer-wins it. Per path, with the
//! receiver's own history H (oldest first) and the offered chain C (the
//! offer's BASE for the path, when it has one, followed by the offered
//! revisions — every sha computed from content):
//!
//!   1. H is empty                      → take everything (fresh path)
//!   2. C's tip is anywhere in H        → UNCHANGED: the receiver already
//!      has that state, so it is equal or AHEAD — never a conflict
//!   3. H's tip is in C (last match)    → FAST-FORWARD past that point
//!   4. otherwise                       → CONFLICT: both moved; keep the
//!      local content, preserve the incoming tip for a hand merge
//!
//! The BASE is what makes incremental offers work. A bundle built with
//! `since > 0` omits the revisions before the cursor — including the one
//! the receiver is sitting on — so without it rule 3 can never match and
//! every incremental push reported a false conflict (the first version
//! did exactly that; its module doc described a base sha the offer never
//! carried). Rule 2 is the other live bug that version had: a receiver
//! that simply moved on after the last sync was reported as a conflict.
//!
//! A base is the SENDER's claimed sha and is advisory in the same sense
//! the per-revision shas are: if a peer hashed differently, the base
//! matches nothing and the path is reported as a conflict — the failure
//! is always the loud one, never a silent overwrite.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::memory::MemoryStore;

/// One revision as it travels in a bundle. `rev_id` is the SENDER's
/// cursor (meaningless on the receiver, whose AUTOINCREMENT differs) —
/// it is reported back so the sender can advance its own "pushed
/// through" high-water mark.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRevision {
    pub rev_id: i64,
    pub path: String,
    pub content: String,
    pub sha256: String,
    pub created_at: String,
}

/// The offer a node sends its peer: who is offering, the cursor the
/// bundle starts after, and the revisions in apply order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncOffer {
    /// The sender's node id (stable; `node.toml`).
    pub from_node: String,
    /// The sender's display name, for humans reading an inbox listing.
    #[serde(default)]
    pub from_name: String,
    /// The sender's high-water mark BEFORE this bundle (the cursor the
    /// receiver's last ack established). First contact is 0 = full sync.
    pub since_rev_id: i64,
    pub revisions: Vec<SyncRevision>,
    /// Per path: the sender's newest sha AT the cursor (`since_rev_id`),
    /// for paths that already existed then. Empty on a full offer
    /// (`since = 0`). Old offers without the field still parse.
    #[serde(default)]
    pub bases: std::collections::BTreeMap<String, String>,
    /// `part` of `parts` when a push was chunked (1-based; 0 of 0 for an
    /// unchunked offer). Display only — chunks apply in arrival order.
    #[serde(default)]
    pub part: u32,
    #[serde(default)]
    pub parts: u32,
}

/// What [`build_offer`] produced, in one displayable line.
pub fn summarize_offer(offer: &SyncOffer) -> String {
    let paths: std::collections::BTreeSet<&str> =
        offer.revisions.iter().map(|r| r.path.as_str()).collect();
    format!(
        "{} revision(s) across {} path(s), since rev {}",
        offer.revisions.len(),
        paths.len(),
        offer.since_rev_id
    )
}

/// Bundle this store's revisions newer than `since_rev_id` into an
/// offer from `from_node` (id) / `from_name` (display). An incremental
/// offer (`since > 0`) also carries each path's BASE — the sender's
/// state at the cursor — so the receiver can fast-forward from it.
pub fn build_offer(
    store: &MemoryStore,
    from_node: &str,
    from_name: &str,
    since_rev_id: i64,
) -> Result<SyncOffer> {
    let rows = store.revisions_since(since_rev_id)?;
    let mut bases = std::collections::BTreeMap::new();
    if since_rev_id > 0 {
        for (_, path, _, _, _) in &rows {
            if bases.contains_key(path) {
                continue;
            }
            if let Some(sha) = store.sha_at(path, since_rev_id) {
                bases.insert(path.clone(), sha);
            }
        }
    }
    let revisions = rows
        .into_iter()
        .map(|(rev_id, path, content, sha256, created_at)| SyncRevision {
            rev_id,
            path,
            content,
            sha256,
            created_at,
        })
        .collect();
    Ok(SyncOffer {
        from_node: from_node.to_string(),
        from_name: from_name.to_string(),
        since_rev_id,
        revisions,
        bases,
        part: 0,
        parts: 0,
    })
}

/// The B1.5 path scoping: which memory paths are one node's own facts
/// and must never travel. Two sources, both local-only:
///
/// 1. `[cluster] local_memory_globs` — operator-named globs against the
///    memory path (`system/core.md` today, for the file that
///    carries both nodes' facts).
/// 2. The `reference/nodes/` subtree — ALWAYS local, whoever's node it
///    names, because that is the convention for node-scoped memory; a
///    glob list that forgot it would ship a peer-dossier to the peer.
///
/// Both ends apply it: the sender withholds, and the receiver refuses
/// to fold a path that is local HERE — `[cluster]` never replicates, so
/// a peer that did not name a path still ships it.
///
/// A bad glob is an ERROR that names it, never a silent non-match (and
/// never a panic in library code on the push path).
pub struct LocalPaths {
    set: globset::GlobSet,
}

impl LocalPaths {
    pub fn new(globs: &[String]) -> Result<Self> {
        let mut b = globset::GlobSet::builder();
        for g in globs {
            let glob = globset::GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .with_context(|| {
                    format!("[cluster] local_memory_globs: '{g}' is not a valid glob")
                })?;
            b.add(glob);
        }
        // The convention subtree, unconditional.
        b.add(
            globset::GlobBuilder::new("reference/nodes/**")
                .literal_separator(true)
                .build()
                .expect("reference/nodes/** is a valid glob"),
        );
        Ok(LocalPaths {
            set: b.build().context("[cluster] local_memory_globs")?,
        })
    }

    fn is_local(&self, path: &str) -> bool {
        self.set.is_match(path)
    }

    /// Drop local paths (and their bases) from an offer IN PLACE; return
    /// what was withheld, named, so the push can say so.
    fn withhold(&self, offer: &mut SyncOffer) -> Vec<String> {
        let bad: std::collections::BTreeSet<String> = offer
            .revisions
            .iter()
            .filter(|r| self.is_local(&r.path))
            .map(|r| r.path.clone())
            .collect();
        offer.revisions.retain(|r| !bad.contains(&r.path));
        offer.bases.retain(|p, _| !bad.contains(p));
        bad.into_iter().collect()
    }
}

/// Bundle this store's replicable revisions into an offer (B1.5): the
/// unscoped `build_offer` filtered through the node's local paths.
/// Returns the offer and the withheld path names, so the caller can
/// tell the operator what stayed put.
pub fn build_offer_scoped(
    store: &MemoryStore,
    from_node: &str,
    from_name: &str,
    since_rev_id: i64,
    local_globs: &[String],
) -> Result<(SyncOffer, Vec<String>)> {
    let filter = LocalPaths::new(local_globs)?;
    let mut offer = build_offer(store, from_node, from_name, since_rev_id)?;
    let withheld = filter.withhold(&mut offer);
    Ok((offer, withheld))
}

/// The per-path outcome of applying a bundle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApplyReport {
    /// Paths where a new revision landed.
    pub applied: Vec<String>,
    /// Paths already up to date (content hash matched newest).
    pub unchanged: Vec<String>,
    /// Paths where BOTH sides moved since the last agreement — the
    /// receiver's newest sha differs from the sender's base AND from
    /// the incoming content. Not applied; the operator must pick.
    pub conflicts: Vec<SyncConflict>,
    /// Paths NOT folded because they are node-local on THIS node
    /// (`[cluster] local_memory_globs`, or the `reference/nodes/`
    /// subtree). Not a conflict: the offer is fine, the path is ours.
    #[serde(default)]
    pub local: Vec<String>,
}

impl ApplyReport {
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }

    pub fn summarize(&self) -> String {
        let mut s = format!(
            "applied {}, unchanged {}, conflicts {}",
            self.applied.len(),
            self.unchanged.len(),
            self.conflicts.len()
        );
        for p in &self.local {
            s.push_str(&format!("\n  kept {p}: node-local here, not folded"));
        }
        for c in &self.conflicts {
            s.push_str(&format!(
                "\n  CONFLICT {} — local moved too; pick a winner",
                c.path
            ));
        }
        s
    }
}

/// A path both sides edited since they last agreed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConflict {
    pub path: String,
    /// The receiver's current newest content (kept).
    pub local_content: String,
    /// The sender's incoming content (rejected, but preserved here so
    /// the operator can merge by hand).
    pub incoming_content: String,
}

/// Apply an incoming offer to this store, path by path, under the four
/// rules in the module doc. A divergence is REPORTED, never
/// last-writer-wins'd away: applying the incoming tip over a diverged
/// local tip would discard the local edit with no record — the
/// git-merge mistake this store exists to not repeat.
///
/// B1.5 never-fold guard: an offer may only land in a store that
/// carries the veles ownership marker (or that Store::open would adopt
/// — the memory-first fresh state.db). Folding one into a FOREIGN file
/// (hermes' DB, another program's) would make this node a second writer
/// of rows it does not own. What the marker CANNOT tell apart is a
/// veles replica of a PEER's state.db — it is a byte copy and carries
/// the marker too; that guard is B5's, where replicas get shipped
/// (PENDING).
///
/// No node-local filter here beyond the always-local `reference/nodes/`
/// subtree — callers holding the config use [`apply_offer_scoped`].
pub fn apply_offer(store: &MemoryStore, offer: &SyncOffer) -> Result<ApplyReport> {
    apply_offer_scoped(store, offer, &[])
}

/// [`apply_offer`], refusing to fold any path that is node-local on THIS
/// node (`local_globs` = this node's `[cluster] local_memory_globs`,
/// plus `reference/nodes/**` always). The sender filters by ITS globs,
/// but `[cluster]` never replicates: a peer that never named
/// `system/core.md` ships it, and folding it here would overwrite this
/// node's own facts with the peer's. Skipped paths are named in
/// [`ApplyReport::local`].
pub fn apply_offer_scoped(
    store: &MemoryStore,
    offer: &SyncOffer,
    local_globs: &[String],
) -> Result<ApplyReport> {
    ensure_primary(store)?;
    let filter = LocalPaths::new(local_globs)?;
    let mut report = ApplyReport::default();
    // Group by path, preserving the bundle's oldest-first order.
    let mut by_path: std::collections::BTreeMap<&str, Vec<&SyncRevision>> =
        std::collections::BTreeMap::new();
    let mut local = std::collections::BTreeSet::new();
    for r in &offer.revisions {
        if filter.is_local(&r.path) {
            local.insert(r.path.clone());
            continue;
        }
        by_path.entry(r.path.as_str()).or_default().push(r);
    }
    report.local = local.into_iter().collect();

    let note = if offer.from_name.is_empty() {
        format!("from {}", offer.from_node)
    } else {
        format!("from {} ({})", offer.from_name, offer.from_node)
    };
    for (path, revs) in by_path {
        // The path rule first: a peer's path is refused before anything
        // about it is read or compared.
        MemoryStore::check_rel(path)?;
        let local = store.path_shas(path)?;

        // The chain as the receiver computes it: the base (if the offer
        // is incremental) and then every offered revision, hashed HERE
        // from content — the sender's per-revision shas are advisory.
        let base = offer.bases.get(path).cloned();
        let offered: Vec<String> = revs.iter().map(|r| memsync_sha(&r.content)).collect();
        let incoming_tip = offered.last().cloned();

        let start = if local.is_empty() {
            // Rule 1: nothing here yet.
            0
        } else if incoming_tip.as_ref().is_some_and(|t| local.contains(t)) {
            // Rule 2: we already hold that state — equal, or ahead.
            report.unchanged.push(path.to_string());
            continue;
        } else {
            let tip = local.last().expect("non-empty");
            // Rule 3, LAST occurrence: a history that returned to an
            // earlier content (A → B → A) must fast-forward from its
            // latest A, or a re-apply would replay B and A again.
            match offered.iter().rposition(|s| s == tip) {
                Some(idx) => idx + 1,
                None if base.as_deref() == Some(tip.as_str()) => 0,
                None => {
                    report.conflicts.push(SyncConflict {
                        path: path.to_string(),
                        local_content: store.latest_content(path).unwrap_or_default(),
                        incoming_content: revs
                            .last()
                            .map(|r| r.content.clone())
                            .unwrap_or_default(),
                    });
                    continue;
                }
            }
        };

        let mut landed = false;
        for r in &revs[start..] {
            if store.record_from_peer(path, &r.content, &note)? {
                landed = true;
            }
        }
        if landed {
            report.applied.push(path.to_string());
        } else {
            report.unchanged.push(path.to_string());
        }
    }
    Ok(report)
}

/// Hash incoming content exactly as the store does, so the ancestor
/// matching and `record`'s dedupe agree on what "the same revision" is.
fn memsync_sha(content: &str) -> String {
    MemoryStore::content_sha(content)
}

/// The B1.5 never-fold check: does this state.db carry the veles marker
/// (or is it a fresh/memory-only file Store::open would ADOPT — the
/// "first command on a new node was `veles memory log`" case)? A
/// foreign file — another program's database — must not receive
/// offers. A veles REPLICA of a peer's store carries the marker too and
/// is NOT caught here; that check belongs with replica shipping (B5). The verdict
/// mirrors Store::open's adoption rule; see
/// [`MemoryStore::is_sync_fold_target`].
fn ensure_primary(store: &MemoryStore) -> Result<()> {
    if store.is_sync_fold_target() {
        Ok(())
    } else {
        anyhow::bail!(
            "refusing to apply an offer into {} — it carries no veles \
             ownership marker, so it is another program's database, not this \
             node's store. Point veles at its own home and apply there.",
            store.db_file().display()
        )
    }
}

// ── transport: chunks, wire encoding, the inbox ─────────────────────

/// The first line of an offer message.
pub const OFFER_MARKER: &str = "[veles memory sync offer]";
const FENCE_B64: &str = "```veles-memsync-b64";
const FENCE_JSON: &str = "```veles-memsync";

/// Serialized-offer budget per A2A message. The server's JSON extractor
/// refuses bodies over 2 MiB (axum's default body limit), and the
/// base64 payload grows by a third before JSON-RPC wraps it — so a chunk
/// stays well under.
pub const CHUNK_BYTES: usize = 768 * 1024;

/// How many offers may wait in one inbox before the server refuses more.
pub const INBOX_MAX: usize = 200;

/// Does this inbound A2A text carry a sync offer (as opposed to a
/// message for the agent)? Checked by the A2A server before any turn.
pub fn is_offer_message(text: &str) -> bool {
    text.trim_start().starts_with(OFFER_MARKER) && text.contains(FENCE_B64)
}

/// The message text for one offer: marker line, human summary, then the
/// base64 payload and its sha256 inside a fence.
pub fn encode_message(offer: &SyncOffer) -> Result<String> {
    encode_message_as(offer, true)
}

/// [`encode_message`], compressed (`enc:z`) or in the plain layout a
/// receiver from before compression reads (see
/// `storesync::call_offer`).
pub fn encode_message_as(offer: &SyncOffer, compress: bool) -> Result<String> {
    let json = serde_json::to_string(offer)?;
    let sha = MemoryStore::content_sha(&json);
    let part = if offer.parts > 1 {
        format!(" (part {}/{})", offer.part, offer.parts)
    } else {
        String::new()
    };
    Ok(format!(
        "{OFFER_MARKER} {}{part}\n\
         Stored in the receiver's memsync inbox; applied only by `veles memory apply --inbox`.\n\
         {FENCE_B64}\nsha256:{sha}\n{}\n```",
        summarize_offer(offer),
        crate::storesync::fence_payload(&json, compress)
    ))
}

/// The offer inside a message (or file): the base64 fence, verified
/// against its checksum, else a plain ```veles-memsync JSON fence, else
/// the whole text as JSON (a file written by `veles memory offer`).
pub fn decode_message(raw: &str) -> Result<SyncOffer> {
    use base64::Engine as _;
    if let Some(start) = raw.find(FENCE_B64) {
        let body = &raw[start + FENCE_B64.len()..];
        let body = &body[..body.find("```").unwrap_or(body.len())];
        let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
        let sha = lines
            .next()
            .and_then(|l| l.strip_prefix("sha256:"))
            .context("sync offer: missing sha256 line")?
            .to_string();
        // Optional `enc:z` marker (see storesync::decode_message): the
        // base64 decodes to a DEFLATE stream over the offer JSON; absent
        // = the older base64-over-plain-JSON layout a mixed pool sends.
        let first = lines.next().unwrap_or("");
        let compressed = first == "enc:z";
        let b64: String = if compressed {
            lines.collect()
        } else {
            std::iter::once(first).chain(lines).collect()
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .context("sync offer: payload is not base64")?;
        let bytes = if compressed {
            crate::storesync::inflate(&bytes).context("sync offer")?
        } else {
            bytes
        };
        let json = String::from_utf8(bytes).context("sync offer: payload is not UTF-8")?;
        if MemoryStore::content_sha(&json) != sha {
            anyhow::bail!("sync offer: checksum mismatch — the payload changed in transit");
        }
        return serde_json::from_str(&json).context("sync offer: payload is not an offer");
    }
    let json = match raw.find(FENCE_JSON) {
        Some(start) => {
            let after = &raw[start + FENCE_JSON.len()..];
            after[..after.find("```").unwrap_or(after.len())].trim()
        }
        None => raw.trim(),
    };
    // An inbox entry wraps the offer with who delivered it; accept both.
    let v: serde_json::Value = serde_json::from_str(json).context("sync offer: not JSON")?;
    let v = v.get("offer").cloned().unwrap_or(v);
    serde_json::from_value(v).context("sync offer: JSON is not an offer")
}

/// Split an offer into chunks whose serialized size stays under
/// `max_bytes`, preserving revision order. A chunk may split one path's
/// history: each chunk carries, per path it touches, the base the path
/// had after everything before the chunk (the original offer's base for
/// the first chunk), so the receiver fast-forwards chunk by chunk. A
/// single revision larger than the budget cannot travel and is refused
/// by name.
pub fn chunk_offer(offer: &SyncOffer, max_bytes: usize) -> Result<Vec<SyncOffer>> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut running: BTreeMap<String, String> = offer.bases.clone();
    let mut chunks: Vec<(Vec<SyncRevision>, BTreeMap<String, String>)> = Vec::new();
    let mut cur: Vec<SyncRevision> = Vec::new();
    let mut cur_start = running.clone();
    let mut cur_bytes = 0usize;
    let finish =
        |cur: &mut Vec<SyncRevision>,
         start: &BTreeMap<String, String>,
         running: &mut BTreeMap<String, String>,
         chunks: &mut Vec<(Vec<SyncRevision>, BTreeMap<String, String>)>| {
            let paths: BTreeSet<&str> = cur.iter().map(|r| r.path.as_str()).collect();
            let bases = start
                .iter()
                .filter(|(p, _)| paths.contains(p.as_str()))
                .map(|(p, s)| (p.clone(), s.clone()))
                .collect();
            for r in cur.iter() {
                running.insert(r.path.clone(), MemoryStore::content_sha(&r.content));
            }
            chunks.push((std::mem::take(cur), bases));
        };
    for r in &offer.revisions {
        let size = serde_json::to_string(r)?.len();
        if size > max_bytes {
            anyhow::bail!(
                "{} rev {} is {} bytes — larger than one A2A message can carry ({} bytes); \
                 move it with `veles memory offer` and a file copy instead",
                r.path,
                r.rev_id,
                size,
                max_bytes
            );
        }
        if cur_bytes + size > max_bytes && !cur.is_empty() {
            finish(&mut cur, &cur_start, &mut running, &mut chunks);
            cur_start = running.clone();
            cur_bytes = 0;
        }
        cur_bytes += size;
        cur.push(r.clone());
    }
    if !cur.is_empty() || chunks.is_empty() {
        finish(&mut cur, &cur_start, &mut running, &mut chunks);
    }
    let parts = chunks.len() as u32;
    Ok(chunks
        .into_iter()
        .enumerate()
        .map(|(i, (revisions, bases))| SyncOffer {
            from_node: offer.from_node.clone(),
            from_name: offer.from_name.clone(),
            since_rev_id: offer.since_rev_id,
            revisions,
            bases,
            part: if parts > 1 { i as u32 + 1 } else { 0 },
            parts: if parts > 1 { parts } else { 0 },
        })
        .collect())
}

/// Drop every path that holds something credential-shaped in ANY of its
/// offered revisions, returning the dropped paths. Whole paths, because
/// a path's chain must travel intact or not at all.
pub fn withhold_secret_paths(offer: &mut SyncOffer) -> Vec<String> {
    let bad: std::collections::BTreeSet<String> = offer
        .revisions
        .iter()
        .filter(|r| crate::a2a::secret_shaped(&r.content))
        .map(|r| r.path.clone())
        .collect();
    offer.revisions.retain(|r| !bad.contains(&r.path));
    offer.bases.retain(|p, _| !bad.contains(p));
    bad.into_iter().collect()
}

/// Where received offers wait: `<veles_home>/memsync/inbox/`.
pub fn inbox_dir() -> std::path::PathBuf {
    crate::config::veles_home().join("memsync").join("inbox")
}

/// One stored offer, with who delivered it and when.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEntry {
    pub received_at: String,
    /// The A2A identity that delivered it (authenticated by the server).
    pub peer: String,
    pub task_id: String,
    pub offer: SyncOffer,
}

/// Store an offer in `dir`. Named by arrival time (zero-padded nanos,
/// so a directory listing IS arrival order and chunks apply in the
/// order they were sent), written atomically, and refused past
/// [`INBOX_MAX`] pending entries.
pub fn inbox_store(
    dir: &std::path::Path,
    peer: &str,
    task_id: &str,
    offer: SyncOffer,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let pending = inbox_pending(dir)?.len();
    if pending >= INBOX_MAX {
        anyhow::bail!(
            "memsync inbox full ({pending} offers waiting) — apply them with \
             `veles memory apply --inbox` first"
        );
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
pub fn inbox_pending(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
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

/// The files in `dir/applied/` (each an offer that landed cleanly).
/// Nothing else ever reads them — they exist so a re-apply can report
/// what moved, and they grow forever otherwise.
pub fn inbox_applied(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    let done = dir.join("applied");
    inbox_pending(&done)
}

/// Drop applied-offer files older than `max_age_secs`. Only files under
/// `applied/` are ever removed — a pending offer (or a held conflict in
/// the inbox itself) is still owed a decision and is never touched.
/// Returns how many files were removed. Nothing throws: an unreadable
/// applied dir reads as nothing to prune (it was never the live path).
pub fn prune_applied(dir: &std::path::Path, max_age_secs: i64) -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let Ok(applied) = inbox_applied(dir) else {
        return 0;
    };
    let mut removed = 0;
    for p in applied {
        let age = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| now - d.as_secs() as i64);
        // No mtime = unprovable age = keep (fail closed, like the
        // snapshot rule: pruning what cannot be aged is a blind delete).
        if age.is_some_and(|a| a > max_age_secs) && std::fs::remove_file(&p).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The cluster's memory status: how many offers are waiting on a
/// decision (pending, held conflicts included — they stay in the inbox),
/// and how many cleanly-applied offers the archive holds. A held
/// conflict is only ever REAL at apply time (`ApplyReport.conflicts`);
/// nothing persists the verdict, so a status read says "this many are
/// awaiting or held — run `veles memory apply --inbox` to see which
/// are conflicts". That is the plan's "conflict counts" verb, honestly:
/// the count exists where the fact exists.
pub fn status_counts() -> (usize, usize) {
    let dir = inbox_dir();
    let pending = inbox_pending(&dir).unwrap_or_default().len();
    let applied = inbox_applied(&dir).map(|v| v.len()).unwrap_or(0);
    (pending, applied)
}

/// Read an inbox entry (or any offer file) back.
pub fn inbox_read(path: &std::path::Path) -> Result<InboxEntry> {
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

/// Build an offer from the local store and push it to a peer's inbox
/// over the A2A link, chunked, one message per chunk. Paths holding
/// secrets are withheld and named. Returns a report of what the peer
/// said it stored; a refusal from the peer is an error that says which
/// chunks had already been stored (re-pushing is safe — apply is
/// idempotent).
pub async fn push_to_peer(
    store: &MemoryStore,
    from_node: &str,
    from_name: &str,
    peer_url: &str,
    bearer: Option<&str>,
    since_rev_id: i64,
) -> Result<String> {
    // A config that does not load is an error here, not an empty glob
    // list: defaulting would ship exactly the node-local paths the
    // operator configured to keep.
    let cfg = crate::config::load()?;
    push_to_peer_scoped(
        store,
        from_node,
        from_name,
        peer_url,
        bearer,
        since_rev_id,
        &cfg.cluster.local_memory_globs,
    )
    .await
}

/// Apply every offer waiting in the inbox, oldest first. A clean one
/// moves to `inbox/applied/`; one with a conflict stays where it is — it
/// is still owed a decision, and the next apply reports it again. Each
/// entry's file name and report, in order. `veles memory apply --inbox`
/// and the gateway (`[cluster] memsync_auto`) both run this.
pub fn apply_inbox(
    store: &MemoryStore,
    local_globs: &[String],
) -> Result<Vec<(String, ApplyReport)>> {
    apply_inbox_dir(store, local_globs, &inbox_dir())
}

/// [`apply_inbox`] against an explicit directory. One in-flight drain
/// per process, shared by the receive path and the periodic sync loop,
/// so an owner-question scan cannot race a second writer of this inbox.
pub fn apply_inbox_dir(
    store: &MemoryStore,
    local_globs: &[String],
    dir: &std::path::Path,
) -> Result<Vec<(String, ApplyReport)>> {
    static DRAIN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _drain = DRAIN.lock().unwrap_or_else(|p| p.into_inner());
    let done = dir.join("applied");
    let mut out = Vec::new();
    for p in inbox_pending(dir)? {
        let name = p.file_name().unwrap_or_default().to_owned();
        let entry = inbox_read(&p)?;
        let report = apply_offer_scoped(store, &entry.offer, local_globs)?;
        if report.is_clean() {
            std::fs::create_dir_all(&done)?;
            // A CLI/older drain may have archived this offer while the
            // apply was in flight. Its effect is already accounted for;
            // a missing source is not a new failure to retry forever.
            match std::fs::rename(&p, done.join(&name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("archive {}", p.display())),
            }
        }
        out.push((name.to_string_lossy().into_owned(), report));
    }
    Ok(out)
}

/// Where this node's memory push to `peer` got to (store meta): the
/// newest local revision the peer has acknowledged.
pub fn cursor_key(peer: &str) -> String {
    format!("memsync_cursor:{peer}")
}

/// The scoped push (B1.5): withholds node-local paths per
/// `[cluster] local_memory_globs` plus the `reference/nodes/` subtree,
/// on top of the secret-shaped withholding.
pub async fn push_to_peer_scoped(
    store: &MemoryStore,
    from_node: &str,
    from_name: &str,
    peer_url: &str,
    bearer: Option<&str>,
    since_rev_id: i64,
    local_globs: &[String],
) -> Result<String> {
    let (mut offer, mut withheld) =
        build_offer_scoped(store, from_node, from_name, since_rev_id, local_globs)?;
    let secret_withheld = withhold_secret_paths(&mut offer);
    let mut out = Vec::new();
    for p in &withheld {
        out.push(format!(
            "withheld {p}: node-local memory (local_memory_globs / reference/nodes/) stays on its node"
        ));
    }
    withheld.extend(secret_withheld.iter().cloned());
    for p in &secret_withheld {
        out.push(format!(
            "withheld {p}: it contains a credential-shaped string, and secrets stay on their node"
        ));
    }
    if offer.revisions.is_empty() {
        out.push(format!("nothing to push since rev {since_rev_id}"));
        return Ok(out.join("\n"));
    }
    let chunks = chunk_offer(&offer, CHUNK_BYTES)?;
    let client = crate::a2a::A2AClient::shared();
    let ctx = crate::a2a::new_context_id();
    let n = chunks.len();
    for (i, c) in chunks.iter().enumerate() {
        let result = crate::storesync::call_offer(&client, peer_url, bearer, &ctx, |z| {
            encode_message_as(c, z)
        })
        .await
        .with_context(|| {
            format!(
                "push chunk {}/{n} to {peer_url} ({} stored before it)",
                i + 1,
                i
            )
        })?;
        let state = result["status"]["state"].as_str().unwrap_or("?");
        let reply = crate::a2a::summarize_result(&result);
        if state != crate::a2a::STATE_COMPLETED {
            anyhow::bail!(
                "{peer_url} refused chunk {}/{n} ({i} stored before it):\n{reply}",
                i + 1
            );
        }
        out.push(format!("chunk {}/{n}: {}", i + 1, summarize_offer(c)));
    }
    out.push(format!(
        "stored in {peer_url}'s inbox — apply there with `veles memory apply --inbox`"
    ));
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_store() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::at(dir.path().to_path_buf());
        (dir, store)
    }

    fn offer(store: &MemoryStore, since: i64) -> SyncOffer {
        build_offer(store, "node-a", "a", since).unwrap()
    }

    /// A pending offer and a held conflict are never pruned — only
    /// `applied/` files with a PROVEN age past the window go. Unprovable
    /// mtime = kept (the fail-closed snapshot rule: pruning what cannot
    /// be aged is a blind delete).
    #[test]
    fn the_applied_prune_touches_only_old_applied_files() {
        let dir = tempfile::tempdir().unwrap();
        // A pending offer in the inbox proper: must always survive,
        // whether it is clean or a held conflict (both live there).
        let (_d, store) = scratch_store();
        store
            .write_file("reference/pending.md", "owed a decision")
            .unwrap();
        inbox_store(
            dir.path(),
            "peer-a",
            "t1",
            build_offer(&store, "node-a", "a", 0).unwrap(),
        )
        .unwrap();
        // An applied file with a backdated mtime: prunable.
        let applied_dir = dir.path().join("applied");
        std::fs::create_dir_all(&applied_dir).unwrap();
        let old = applied_dir.join("old.json");
        std::fs::write(&old, "{}").unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600);
        // Backdate via a file-times update: FileTimes sets the modified
        // stamp the prune reads. The chained builder IS the value — it
        // must be consumed by set_times, whose Result is checked.
        let times = std::fs::FileTimes::new().set_modified(past);
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_times(times).expect("backdating the applied file");

        let removed = prune_applied(dir.path(), 7 * 24 * 3600);
        assert_eq!(removed, 1, "the old applied file went");
        assert!(!old.exists(), "the file itself, not just the count");
        assert_eq!(
            inbox_pending(dir.path()).unwrap().len(),
            1,
            "the pending offer in the inbox proper survived"
        );
    }

    /// The inbox applies oldest first: a clean offer moves to
    /// `applied/`, and one whose path both sides changed stays — the
    /// local content is kept and the entry is still owed a decision.
    #[test]
    fn the_inbox_applies_clean_offers_and_holds_a_conflict() {
        let (_home, _env) = crate::testenv::scratch_home();
        let (_d1, a) = scratch_store();
        let (_d2, b) = scratch_store();
        let (_d3, me) = scratch_store();
        a.write_file("reference/a.md", "from a").unwrap();
        me.write_file("system/core.md", "mine").unwrap();
        b.write_file("system/core.md", "theirs").unwrap();
        let dir = inbox_dir();
        inbox_store(
            &dir,
            "peer-a",
            "t1",
            build_offer(&a, "node-a", "a", 0).unwrap(),
        )
        .unwrap();
        inbox_store(
            &dir,
            "peer-b",
            "t2",
            build_offer(&b, "node-b", "b", 0).unwrap(),
        )
        .unwrap();
        let reports = apply_inbox(&me, &[]).unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports.iter().filter(|(_, r)| r.is_clean()).count(), 1);
        assert_eq!(me.latest_content("reference/a.md").unwrap(), "from a");
        assert_eq!(
            me.latest_content("system/core.md").unwrap(),
            "mine",
            "local kept"
        );
        let pending = inbox_pending(&dir).unwrap();
        assert_eq!(pending.len(), 1, "the conflict stays in the inbox");
        assert_eq!(inbox_read(&pending[0]).unwrap().peer, "peer-b");
        // Applying again reports it again, and moves nothing.
        assert_eq!(apply_inbox(&me, &[]).unwrap().len(), 1);
    }

    /// Build an offer out of one store and apply it to another that has
    /// never seen the paths: every revision lands, in order, and the
    /// receiver converges on the sender's content. This is the steady
    /// state of replication — a node that was off catching up.
    #[test]
    fn apply_offer_fast_forwards_a_fresh_receiver() {
        let (_d1, sender) = scratch_store();
        let (_d2, receiver) = scratch_store();
        sender.write_file("system/core.md", "alpha").unwrap();
        sender.write_file("system/core.md", "alpha beta").unwrap();
        sender.write_file("reference/x.md", "note").unwrap();

        let report = apply_offer(&receiver, &offer(&sender, 0)).unwrap();
        assert!(report.is_clean(), "no conflicts against an empty receiver");
        assert_eq!(report.applied.len(), 2);
        assert_eq!(
            receiver.latest_content("system/core.md").unwrap(),
            "alpha beta"
        );
    }

    /// A second push from the same cursor must be a no-op: the receiver
    /// already has the content, dedup-by-hash reports `unchanged`, and
    /// nothing double-writes. Incremental replication depends on this —
    /// a re-push of an applied bundle must not stack duplicate rows.
    #[test]
    fn reapplying_an_applied_bundle_is_unchanged_not_duplicated() {
        let (_d1, sender) = scratch_store();
        let (_d2, receiver) = scratch_store();
        sender.write_file("system/user.md", "u1").unwrap();
        let o = offer(&sender, 0);

        let first = apply_offer(&receiver, &o).unwrap();
        assert_eq!(first.applied.len(), 1);
        let second = apply_offer(&receiver, &o).unwrap();
        assert_eq!(second.applied.len(), 0);
        assert_eq!(second.unchanged.len(), 1);
    }

    /// THE property the whole design exists for: both nodes edit the
    /// same path while partitioned, and the sync must SURFACE it, not
    /// last-writer-wins it away. The receiver keeps its local content,
    /// the incoming content is preserved in the conflict report for a
    /// hand merge, and nothing is applied for that path.
    #[test]
    fn diverged_edits_conflict_and_keep_local_content() {
        let (_d1, sender) = scratch_store();
        let (_d2, receiver) = scratch_store();
        sender.write_file("system/core.md", "base").unwrap();
        apply_offer(&receiver, &offer(&sender, 0)).unwrap();

        // Partition: BOTH move independently.
        sender.write_file("system/core.md", "base +sender").unwrap();
        receiver
            .write_file("system/core.md", "base +receiver")
            .unwrap();

        let report = apply_offer(&receiver, &offer(&sender, 0)).unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.conflicts.len(), 1);
        let c = &report.conflicts[0];
        assert_eq!(c.path, "system/core.md");
        assert_eq!(c.local_content, "base +receiver");
        assert_eq!(c.incoming_content, "base +sender");
        assert_eq!(
            receiver.latest_content("system/core.md").unwrap(),
            "base +receiver"
        );
    }

    /// When only the SENDER moved (the always-on node accumulated edits
    /// while the laptop slept), the receiver fast-forwards cleanly.
    #[test]
    fn sender_only_moved_fast_forwards_without_conflict() {
        let (_d1, sender) = scratch_store();
        let (_d2, receiver) = scratch_store();
        sender.write_file("system/core.md", "v1").unwrap();
        apply_offer(&receiver, &offer(&sender, 0)).unwrap();

        sender.write_file("system/core.md", "v2").unwrap();
        sender.write_file("system/core.md", "v3").unwrap();

        let report = apply_offer(&receiver, &offer(&sender, 0)).unwrap();
        assert!(report.is_clean(), "one-sided movement never conflicts");
        assert_eq!(report.applied.len(), 1);
        assert_eq!(receiver.latest_content("system/core.md").unwrap(), "v3");
    }

    /// An applied revision must SURVIVE the next read. The first version
    /// recorded it and never mirrored it to disk, so the next read's
    /// hand-edit absorber saw the stale file, took it for an edit, and
    /// recorded the old content back on top — reproduced live, where
    /// even `veles memory log` undid the apply. There is no mirror and
    /// no absorber now (2026-09-26), but the fast-forward is the common
    /// case, so the pin stays.
    #[test]
    fn applied_revision_survives_the_next_read() {
        let (_d1, sender) = scratch_store();
        let (_d2, receiver) = scratch_store();
        // The receiver already has v1, on disk and in the table.
        sender.write_file("system/core.md", "v1").unwrap();
        receiver.write_file("system/core.md", "v1").unwrap();
        sender.write_file("system/core.md", "v2").unwrap();

        let report = apply_offer(&receiver, &offer(&sender, 0)).unwrap();
        assert_eq!(report.applied, vec!["system/core.md".to_string()]);

        // A read, exactly as the agent or the CLI would do it.
        assert_eq!(receiver.read_file("system/core.md").unwrap(), "v2");
        assert_eq!(receiver.latest_content("system/core.md").unwrap(), "v2");
        assert_eq!(receiver.read(crate::memory::StoreKind::Memory), "v2");
    }

    /// A deletion is a revision (empty content), so it replicates like
    /// one: the receiver's copy disappears, and its history keeps it.
    #[test]
    fn a_deletion_replicates() {
        let (_d1, sender) = scratch_store();
        let (_d2, receiver) = scratch_store();
        sender.write_file("reference/x.md", "shared note").unwrap();
        apply_offer(&receiver, &offer(&sender, 0)).unwrap();
        assert_eq!(receiver.read_file("reference/x.md").unwrap(), "shared note");
        let cursor = sender.latest_rev_id().unwrap();

        sender.delete_file("reference/x.md").unwrap();
        let report = apply_offer(&receiver, &offer(&sender, cursor)).unwrap();
        assert_eq!(
            report.applied,
            vec!["reference/x.md".to_string()],
            "{report:?}"
        );
        assert!(receiver.read_file("reference/x.md").is_err());
        assert!(!receiver
            .list()
            .unwrap()
            .iter()
            .any(|f| f.path == "reference/x.md"));
        assert_eq!(receiver.path_shas("reference/x.md").unwrap().len(), 2);
    }

    /// An INCREMENTAL offer (`since > 0`) omits the revision the
    /// receiver is sitting on. Without the per-path base it could never
    /// find a fast-forward point, and every incremental push was a false
    /// conflict (reproduced live). The base makes it a fast-forward.
    #[test]
    fn incremental_offer_fast_forwards() {
        let (_d1, sender) = scratch_store();
        let (_d2, receiver) = scratch_store();
        sender.write_file("system/core.md", "v1").unwrap();
        apply_offer(&receiver, &offer(&sender, 0)).unwrap();
        let cursor = sender.latest_rev_id().unwrap();

        sender.write_file("system/core.md", "v2").unwrap();
        let inc = offer(&sender, cursor);
        assert_eq!(inc.revisions.len(), 1, "only the new revision travels");
        assert!(inc.bases.contains_key("system/core.md"), "and its base");

        let report = apply_offer(&receiver, &inc).unwrap();
        assert!(report.is_clean(), "an incremental push is not a conflict");
        assert_eq!(receiver.latest_content("system/core.md").unwrap(), "v2");
    }

    /// A receiver that moved on ALONE since the last sync is AHEAD, not
    /// in conflict: it already holds the sender's state. The first
    /// version reported a conflict here (reproduced live), which with two
    /// nodes pushing both ways is the everyday case.
    #[test]
    fn receiver_only_moved_is_not_a_conflict() {
        let (_d1, sender) = scratch_store();
        let (_d2, receiver) = scratch_store();
        sender.write_file("system/core.md", "v1").unwrap();
        apply_offer(&receiver, &offer(&sender, 0)).unwrap();
        receiver
            .write_file("system/core.md", "v2 (receiver)")
            .unwrap();

        let report = apply_offer(&receiver, &offer(&sender, 0)).unwrap();
        assert!(report.is_clean(), "the receiver is ahead: {report:?}");
        assert_eq!(report.unchanged, vec!["system/core.md".to_string()]);
        assert_eq!(
            receiver.latest_content("system/core.md").unwrap(),
            "v2 (receiver)"
        );
    }

    /// A history that RETURNS to an earlier state (A → B → A) and then
    /// moves on (→ C) must fast-forward from the receiver's LATEST A.
    /// Matching the tip at its FIRST occurrence replayed B and A before
    /// C, stacking three revisions where one was new.
    #[test]
    fn revert_shaped_history_fast_forwards_from_the_last_match() {
        let (_d1, sender) = scratch_store();
        let (_d2, receiver) = scratch_store();
        for v in ["A", "B", "A"] {
            sender.write_file("system/core.md", v).unwrap();
        }
        apply_offer(&receiver, &offer(&sender, 0)).unwrap();
        let before = receiver.path_shas("system/core.md").unwrap().len();

        sender.write_file("system/core.md", "C").unwrap();
        let report = apply_offer(&receiver, &offer(&sender, 0)).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(
            receiver.path_shas("system/core.md").unwrap().len(),
            before + 1,
            "exactly one new revision (C), not a replay of B and A"
        );
        assert_eq!(receiver.latest_content("system/core.md").unwrap(), "C");
    }

    /// The A2A client runs `redact_outbound` over every message it
    /// sends. A plain-JSON offer came out with every email address in
    /// memory replaced by `[redacted-email]` — an altered file on the
    /// peer, whose next push then conflicts with itself. The encoded
    /// offer must come through the redactor byte-identical.
    #[test]
    fn an_offer_survives_outbound_redaction() {
        let (_d, sender) = scratch_store();
        let text = "owner: someone@example.org — prefers terse replies";
        sender.write_file("system/user.md", text).unwrap();
        let msg = encode_message(&offer(&sender, 0)).unwrap();
        let back = decode_message(&crate::a2a::redact_outbound(&msg)).unwrap();
        assert_eq!(back.revisions[0].content, text);
    }

    /// A pool running mixed versions keeps syncing: the new wire form
    /// (`enc:z` + base64 over DEFLATE) and the old (base64 over plain
    /// JSON) both decode to the same offer.
    #[test]
    fn wire_accepts_both_compressed_and_plain_payloads() {
        let (_d, sender) = scratch_store();
        sender
            .write_file("system/user.md", "compress compress compress")
            .unwrap();
        let off = offer(&sender, 0);
        let json = serde_json::to_string(&off).unwrap();
        let sha = MemoryStore::content_sha(&json);

        let msg = encode_message(&off).unwrap();
        assert!(msg.contains("enc:z"));
        assert_eq!(
            decode_message(&msg).unwrap().revisions[0].content,
            "compress compress compress"
        );

        use base64::Engine as _;
        let plain_b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        let plain = format!("{OFFER_MARKER} x\n{FENCE_B64}\nsha256:{sha}\n{plain_b64}\n```");
        assert_eq!(
            decode_message(&plain).unwrap().revisions[0].content,
            "compress compress compress",
            "a plain offer from an older node still decodes"
        );
    }

    /// Anything else that changes the payload in transit is caught by
    /// the checksum, not applied.
    #[test]
    fn a_payload_changed_in_transit_is_refused() {
        let (_d, sender) = scratch_store();
        sender
            .write_file("system/user.md", "some memory worth keeping")
            .unwrap();
        let msg = encode_message(&offer(&sender, 0)).unwrap();
        let b64_line = msg
            .lines()
            .find(|l| l.len() > 20 && !l.contains(' ') && !l.starts_with("sha256:"))
            .unwrap()
            .to_string();
        let mut chars: Vec<char> = b64_line.chars().collect();
        chars[10] = if chars[10] == 'A' { 'B' } else { 'A' };
        let tampered = msg.replace(&b64_line, &chars.into_iter().collect::<String>());
        let err = decode_message(&tampered).unwrap_err().to_string();
        assert!(err.contains("checksum") || err.contains("offer"), "{err}");
    }

    /// A chunked push may split ONE path's history across chunks. Each
    /// chunk carries the base the previous chunk left, so applying them
    /// in order fast-forwards cleanly — including onto a receiver that
    /// already had the first revision.
    #[test]
    fn chunks_split_one_path_and_still_fast_forward() {
        let (_d1, sender) = scratch_store();
        let (_d2, receiver) = scratch_store();
        let pad = "x".repeat(400);
        for i in 0..6 {
            sender
                .write_file("system/core.md", &format!("v{i} {pad}"))
                .unwrap();
        }
        receiver
            .write_file("system/core.md", &format!("v0 {pad}"))
            .unwrap();
        let chunks = chunk_offer(&offer(&sender, 0), 1000).unwrap();
        assert!(chunks.len() >= 3, "small budget → several chunks");
        assert_eq!(chunks[0].parts as usize, chunks.len());
        for c in &chunks {
            let r = apply_offer(&receiver, c).unwrap();
            assert!(r.is_clean(), "chunk {}/{}: {r:?}", c.part, c.parts);
        }
        assert_eq!(
            receiver.latest_content("system/core.md").unwrap(),
            format!("v5 {pad}")
        );
    }

    /// A path holding a credential-shaped string does not travel — the
    /// whole path, since a chain must move intact — and it is named.
    #[test]
    fn secret_paths_are_withheld_by_name() {
        let (_d, sender) = scratch_store();
        sender
            .write_file("system/core.md", "ordinary fact")
            .unwrap();
        sender
            .write_file("reference/keys.md", "old: plain\n")
            .unwrap();
        sender
            .write_file("reference/keys.md", "key sk-ant-abcdefghijklmnopqrstuvwx")
            .unwrap();
        let mut o = offer(&sender, 0);
        let withheld = withhold_secret_paths(&mut o);
        assert_eq!(withheld, vec!["reference/keys.md".to_string()]);
        assert!(o.revisions.iter().all(|r| r.path == "system/core.md"));
    }

    /// B1.5: node-local memory stays on its node. The `reference/nodes/`
    /// subtree is ALWAYS local — even with an EMPTY glob list, because
    /// that subtree IS the node-scoped convention (a peer dossier
    /// shipped to the peer is the canonical mistake) — and an operator
    /// glob (here `system/core.md`, the file that carries both nodes'
    /// facts) is withheld and named too. Shared memory still travels.
    #[test]
    fn node_local_paths_stay_on_their_node() {
        let (_d, sender) = scratch_store();
        sender
            .write_file(
                "system/core.md",
                "laptop: ~/.hermes/notes/x; vps: /opt/data",
            )
            .unwrap();
        sender
            .write_file(
                "reference/nodes/vps/peer-dossier.md",
                "demon runs in a container",
            )
            .unwrap();
        sender
            .write_file("reference/projects/pool.md", "shared identity fact")
            .unwrap();
        let (o, withheld) =
            build_offer_scoped(&sender, "n1", "laptop", 0, &["system/core.md".to_string()])
                .unwrap();
        assert_eq!(
            withheld,
            vec![
                "reference/nodes/vps/peer-dossier.md".to_string(),
                "system/core.md".to_string(),
            ],
            "both the convention subtree and the operator glob are named"
        );
        assert_eq!(o.revisions.len(), 1, "only the shared path travels");
        assert_eq!(o.revisions[0].path, "reference/projects/pool.md");
        // The base of a withheld path must not leak either.
        assert!(
            o.bases.is_empty(),
            "withheld paths carry no base: {:?}",
            o.bases
        );
    }

    /// B1.5, the empty-list half: `local_memory_globs` unset must still
    /// withhold `reference/nodes/**`. The convention is not optional.
    #[test]
    fn nodes_subtree_is_local_even_with_no_globs() {
        let (_d, sender) = scratch_store();
        sender
            .write_file("reference/nodes/self/layout.md", "sudo via wheel")
            .unwrap();
        sender.write_file("system/core.md", "shared fact").unwrap();
        let (o, withheld) = build_offer_scoped(&sender, "n1", "laptop", 0, &[]).unwrap();
        assert_eq!(withheld, vec!["reference/nodes/self/layout.md".to_string()]);
        assert!(o.revisions.iter().all(|r| r.path == "system/core.md"));
    }

    #[test]
    fn a_receiver_keeps_its_own_node_local_paths() {
        // `local_memory_globs` lives in `[cluster]`, which never
        // replicates: a peer that did not name `system/core.md` ships
        // it, and the receiver that DID must not fold it over its own
        // node-local facts. Same for the always-local subtree.
        let (_d, sender) = scratch_store();
        sender.write_file("system/core.md", "laptop facts").unwrap();
        sender
            .write_file("reference/nodes/vps/dossier.md", "the laptop's view of vps")
            .unwrap();
        sender
            .write_file("reference/projects/pool.md", "shared")
            .unwrap();
        let (_r, receiver) = scratch_store();
        receiver.write_file("system/core.md", "vps facts").unwrap();
        let o = build_offer(&sender, "node-a", "laptop", 0).unwrap();
        let report = apply_offer_scoped(&receiver, &o, &["system/core.md".to_string()]).unwrap();
        assert_eq!(
            receiver.latest_content("system/core.md").as_deref(),
            Some("vps facts"),
            "the receiver's node-local file is untouched"
        );
        assert!(
            receiver
                .latest_content("reference/nodes/vps/dossier.md")
                .is_none(),
            "a peer's node-scoped memory never lands here"
        );
        assert_eq!(
            receiver
                .latest_content("reference/projects/pool.md")
                .as_deref(),
            Some("shared"),
            "shared memory still lands"
        );
        assert_eq!(
            report.local,
            vec![
                "reference/nodes/vps/dossier.md".to_string(),
                "system/core.md".to_string()
            ],
            "and the skips are named"
        );
    }

    /// A typo in `local_memory_globs` is an operator error to report,
    /// not a panic in library code on the push path.
    #[test]
    fn a_bad_local_glob_is_an_error_not_a_panic() {
        let (_d, sender) = scratch_store();
        sender.write_file("system/core.md", "x").unwrap();
        let r = std::panic::catch_unwind(|| {
            build_offer_scoped(&sender, "n1", "laptop", 0, &["system/[core".to_string()])
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let err = r
            .expect("must not panic")
            .expect_err("a bad glob is refused");
        assert!(
            err.contains("system/[core"),
            "the refusal names the glob: {err}"
        );
    }

    /// B1.5 never-fold guard: an offer must not be foldable into a
    /// foreign state.db. A veles-owned store passes; a file with
    /// another program's `messages` table and no veles marker fails;
    /// a memory-only fresh file passes (the new-node first-act case).
    #[test]
    fn an_offer_never_folds_into_a_foreign_store() {
        // Veles-owned: a real Store::open has run, marker written.
        let dir = tempfile::tempdir().unwrap();
        let owned = crate::store::Store::open(&dir.path().join("state.db")).unwrap();
        drop(owned);
        let receiver = MemoryStore::at(dir.path().to_path_buf());
        receiver.write_file("system/core.md", "v").unwrap();
        assert!(
            receiver.is_sync_fold_target(),
            "a veles-owned state.db is a fold target"
        );

        // Foreign: hermes-shaped — a messages table, no veles marker.
        let fdir = tempfile::tempdir().unwrap();
        let fpath = fdir.path().join("state.db");
        let fc = rusqlite::Connection::open(&fpath).unwrap();
        fc.execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, content TEXT);
             INSERT INTO messages (content) VALUES ('hermes row');",
        )
        .unwrap();
        drop(fc);
        let foreign = MemoryStore::at(fdir.path().to_path_buf());
        let mut o = offer(&receiver, 0);
        o.revisions.clear(); // content irrelevant; the guard fires first
        let err = apply_offer(&foreign, &o).unwrap_err().to_string();
        assert!(
            err.contains("refusing to apply"),
            "the refusal is loud and named: {err}"
        );

        // Memory-only fresh file: adoptable, like Store::open adopts.
        let mdir = tempfile::tempdir().unwrap();
        let fresh = MemoryStore::at(mdir.path().to_path_buf());
        fresh.write_file("system/core.md", "v").unwrap();
        assert!(
            fresh.is_sync_fold_target(),
            "a memory-first state.db is a fold target (new-node first act)"
        );
    }

    /// An inbox lists in arrival order and refuses past its cap.
    #[test]
    fn the_inbox_keeps_arrival_order() {
        let (_d, sender) = scratch_store();
        sender.write_file("system/core.md", "v").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let a = inbox_store(dir.path(), "vps", "t-1", offer(&sender, 0)).unwrap();
        let b = inbox_store(dir.path(), "vps", "t-2", offer(&sender, 0)).unwrap();
        assert_eq!(inbox_pending(dir.path()).unwrap(), vec![a, b.clone()]);
        assert_eq!(inbox_read(&b).unwrap().task_id, "t-2");
    }

    /// A peer's path is refused before anything is written — the offer
    /// arrives from another machine, and `veles memory export` turns
    /// paths back into files.
    #[test]
    fn peer_path_traversal_is_refused() {
        let (dir, receiver) = scratch_store();
        for bad in ["../../evil.md", "system/../../evil.md", ".hidden.md", ""] {
            let o = SyncOffer {
                from_node: "x".into(),
                from_name: String::new(),
                since_rev_id: 0,
                revisions: vec![SyncRevision {
                    rev_id: 1,
                    path: bad.into(),
                    content: "pwned".into(),
                    sha256: String::new(),
                    created_at: String::new(),
                }],
                bases: Default::default(),
                part: 0,
                parts: 0,
            };
            assert!(apply_offer(&receiver, &o).is_err(), "must refuse {bad:?}");
        }
        assert!(!dir.path().join("evil.md").exists());
        assert!(!dir.path().parent().unwrap().join("evil.md").exists());
    }
}
